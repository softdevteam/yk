//! The Yorick TIR trace compiler.

use crate::{
    find_symbol, stack_builder::StackBuilder, CompileError, CompiledTrace, IndirectLoc, Location,
    RegAlloc, RegAndOffset,
};
use dynasmrt::{
    dynasm, x64::Rq::*, DynamicLabel, DynasmApi, DynasmLabelApi, ExecutableBuffer, Register,
};
use lazy_static::lazy_static;
use std::alloc::{alloc, Layout};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::mem;
use std::process::Command;
use ykpack::{IRPlace, LocalDecl, SignedIntTy, Ty, TyKind, UnsignedIntTy};
use yksg::{FrameInfo, StopgapInterpreter};
use yktrace::sir::{INTERP_STEP_ARG, SIR};
use yktrace::tir::{BinOp, CallOperand, Guard, GuardKind, Local, Statement, TirOp, TirTrace};

mod store;

lazy_static! {
    // Registers that are caller-save as per the Sys-V ABI.
    // Note that R11 is also caller-save, but it's our temproary register and we never want to
    // preserve its value across calls.
    static ref CALLER_SAVED_REGS: [u8; 8] = [RAX.code(), RDI.code(), RSI.code(), RDX.code(),
                                            RCX.code(), R8.code(), R9.code(), R10.code()];

    // Registers that are callee-save as per the Sys-V ABI.
    // Note that RBP is also callee-save, but is handled specially.
    static ref CALLEE_SAVED_REGS: [u8; 5] = [RBX.code(), R12.code(), R13.code(),
                                            R14.code(), R15.code()];

    // The register partitioning. These arrays must not overlap.
    static ref TEMP_REG: u8 = R11.code();
    pub static ref REG_POOL: [u8; 11] = [RAX.code(), RCX.code(), RDX.code(), R8.code(), R9.code(),
                                     R10.code(), RBX.code(), R12.code(), R13.code(), R14.code(),
                                     R15.code()];

    // The interpreter context is always allocated to this reserved register.
    // This register should not appear in REG_POOL.
    static ref ICTX_REG: u8 = RDI.code();

    static ref TEMP_LOC: Location = Location::Reg(*TEMP_REG);

    /// The size of a memory address in bytes.
    static ref PTR_SIZE: u64 = u64::try_from(mem::size_of::<usize>()).unwrap();
}

/// The size of a quad-word register (e.g. RAX) in bytes.
const QWORD_REG_SIZE: usize = 8;

/// Number of bytes the stack must be aligned to, immediately before a call via the Sys-V ABI.
const SYSV_CALL_STACK_ALIGN: usize = 16;

/// Generates functions for add/sub-style operations.
/// The first operand must be in a register.
macro_rules! binop_add_sub {
    ($name: ident, $op:expr) => {
        fn $name(&mut self, opnd1_reg: u8, opnd2: &IRPlace) {
            let size = SIR.ty(&opnd2.ty()).size();
            let opnd2_loc = self.irplace_to_location(opnd2);
            match opnd2_loc {
                Location::Reg(r) => match size {
                    1 => {
                        dynasm!(self.asm
                            ; $op Rb(opnd1_reg), Rb(r)
                        );
                    }
                    2 => {
                        dynasm!(self.asm
                            ; $op Rw(opnd1_reg), Rw(r)
                        );
                    }
                    4 => {
                        dynasm!(self.asm
                            ; $op Rd(opnd1_reg), Rd(r)
                        );
                    }
                    8 => {
                        dynasm!(self.asm
                            ; $op Rq(opnd1_reg), Rq(r)
                        );
                    }
                    _ => unreachable!(format!("{}", SIR.ty(&opnd2.ty()))),
                },
                Location::Mem(..) => todo!(),
                Location::Const { val, .. } => {
                    let val = val.i64_cast();
                    match size {
                        1 => {
                            dynasm!(self.asm
                                ; $op Rb(opnd1_reg), val as i8
                            );
                        }
                        2 => {
                            dynasm!(self.asm
                                ; $op Rw(opnd1_reg), val as i16
                            );
                        }
                        4 => {
                            dynasm!(self.asm
                                ; $op Rd(opnd1_reg), val as i32
                            );
                        }
                        8 => {
                            if i32::try_from(val).is_err() {
                                // FIXME Work around x86_64 encoding limitations (no imm64 operands).
                                todo!();
                            } else {
                                dynasm!(self.asm
                                    ; $op Rq(opnd1_reg), val as i32
                                );
                            }
                        }
                        _ => unreachable!(format!("{}", SIR.ty(&opnd2.ty()))),
                    }
                }
                Location::Indirect { .. } => todo!(),
            }
        }
    }
}

/// Generates functions for mul/div-style operations.
/// The first operand must be in a register.
macro_rules! binop_mul_div {
    // The addressing modes and semantics of x86_64 MUL and DIV are as follows:
    //
    //   Encoding       Semantics
    //   --------------------------------------------------------------------
    //   MUL r/m8:      AL * r/m8          AX <- result
    //   MUL r/m16:     AX * r/m16         DX:AX <- result
    //   MUL r/m32:     EAX * r/m32        EDX:EAX <- result
    //   MUL r/m64:     RAX * r/m64        RDX:RAX <- result
    //
    //   DIV r/m8:      AX / r/m8          AL <- quotient,  AH <- remainder
    //   DIV r/m16:     DX:AX / r/m16      AX <- quotient,  DX <- remainder
    //   DIV r/m32:     EDX:EAX * r/m32    EAX <- quotient, EDX <- remainder
    //   DIV r/m64:     RDX:RAX * r/m64    RAX <- quotient, RDX <- remainder
    //
    // A couple of thing to note:
    //   - DIV's first (implicit) operand is twice as large as the second (explicit) operand and
    //     for all but the r/m8 variant, the operand extends into (some subset of) RDX.
    //   - Similarly, the result of both MUL and DIV is twice as large as the second operand.
    //
    // SIR only deals with same-sized operands and doesn't require us to capture the remainder for
    // division, so we don't use this extended operand/result space. This makes MUL and DIV very
    // similar from our point of view, hence this macro is used to generate both implementations.
    //
    // We are however, required to zero the unused extended operand space to get the correct
    // result, and we must be careful to save and restore anything that would be clobbered.
    // Specifically:
    //   - RAX is always clobbered, so we must always save and restore it.
    //   - For all but the r/m8 variations, RDX is clobbered. So if the size of our SIR operands is
    //     >1 byte, then we must also save and restore RDX.
    //   - If our register allocator has put the second SIR operand in a register that will be
    //     clobbered, then we cannot use this register as the r/m operand. In this case we borrow
    //     RCX temporarily.
    ($name: ident, $op:expr) => {
        fn $name(&mut self, opnd1_reg: u8, opnd2: &IRPlace) {
            let size = SIR.ty(&opnd2.ty()).size();
            // For all but r/m8 variants, RDX is clobbered.
            if size > 1 {
                dynasm!(self.asm
                    ; push rdx
                );
            }
            // RAX is always clobbered.
            dynasm!(self.asm
                ; push rax
            );
            // Set up first operand.
            dynasm!(self.asm
                ; mov rax, Rq(opnd1_reg)
            );
            // Set up second operand.
            let src_loc = self.irplace_to_location(opnd2);
            match src_loc {
                Location::Reg(src_r) => {
                    // Handle cases where our input operands clash with a clobbered register.
                    let (r, borrow_rcx) = if src_r == RAX.code() {
                        // RAX has already been clobbered. Take a copy from the stack.
                        let rax_off = i32::try_from(QWORD_REG_SIZE).unwrap();
                        dynasm!(self.asm
                            ; push rcx
                            ; mov rcx, [rsp + rax_off]
                        );
                        (RCX.code(), true)
                    } else if size > 1 && src_r == RDX.code() {
                        // RDX hasn't been clobbered yet, but it will be.
                        dynasm!(self.asm
                            ; push rcx
                            ; mov rcx, rdx
                        );
                        (RCX.code(), true)
                    } else {
                        (src_r, false)
                    };
                    // Set DIV's unused extended operand space to zero.
                    if stringify!($op) == "div" {
                        if size == 1 {
                            dynasm!(self.asm
                                ; and ax, 0x00ff
                            );
                        } else {
                            dynasm!(self.asm
                                ; xor rdx, rdx
                            );
                        }
                    }
                    match size {
                        1 => {
                            dynasm!(self.asm
                                ; $op Rb(r)
                            );
                        }
                        2 => {
                            dynasm!(self.asm
                                ; $op Rw(r)
                            );
                        }
                        4 => {
                            dynasm!(self.asm
                                ; $op Rd(r)
                            );
                        }
                        8 => {
                            dynasm!(self.asm
                                ; $op Rq(r)
                            );
                        }
                        _ => unreachable!(format!("{}", SIR.ty(&opnd2.ty()))),
                    }
                    // If we borrowed RCX, then restore it.
                    if borrow_rcx {
                        dynasm!(self.asm
                            ; pop rcx
                        );
                    }
                },
                Location::Mem(..) => todo!(),
                Location::Const { val, .. } => {
                    // Set DIV's unused extended operand space to zero.
                    if stringify!($op) == "div" {
                        if size == 1 {
                            dynasm!(self.asm
                                ; and ax, 0x00ff
                            );
                        } else {
                            dynasm!(self.asm
                                ; xor rdx, rdx
                            );
                        }
                    }
                    // It's safe to use TEMP_REG here, because opnd2 isn't in a register and if
                    // opnd1_reg was TEMP_REG then we've already moved it into RAX.
                    let val = val.i64_cast();
                    match size {
                        1 => {
                            dynasm!(self.asm
                                ; mov Rb(*TEMP_REG), val as i8
                                ; $op Rb(*TEMP_REG)
                            );
                        }
                        2 => {
                            dynasm!(self.asm
                                ; mov Rw(*TEMP_REG), val as i16
                                ; $op Rw(*TEMP_REG)
                            );
                        }
                        4 => {
                            dynasm!(self.asm
                                ; mov Rd(*TEMP_REG), val as i32
                                ; $op Rd(*TEMP_REG)
                            );
                        }
                        8 => {
                            if i32::try_from(val).is_err() {
                                // FIXME Work around x86_64 encoding limitations (no imm64 operands).
                                todo!();
                            } else {
                                dynasm!(self.asm
                                    ; mov Rq(*TEMP_REG), val as i32
                                    ; $op Rq(*TEMP_REG)
                                );
                            }
                        }
                        _ => unreachable!(format!("{}", SIR.ty(&opnd2.ty()))),
                    }
                }
                Location::Indirect { .. } => todo!(),
            }
            // Put result in place and restore clobbered registers.
            dynasm!(self.asm
                ; mov Rq(opnd1_reg), rax
                ; pop rax
            );
            if size > 1 {
                dynasm!(self.asm
                    ; pop rdx
                );
            }
        }
    }
}

/// Converts a register number into it's string name.
fn local_to_reg_name(loc: &Location) -> &'static str {
    match loc {
        Location::Reg(r) => match r {
            0 => "rax",
            1 => "rcx",
            2 => "rdx",
            3 => "rbx",
            4 => "rsp",
            5 => "rbp",
            6 => "rsi",
            7 => "rdi",
            8 => "r8",
            9 => "r9",
            10 => "r10",
            11 => "r11",
            12 => "r12",
            13 => "r13",
            14 => "r14",
            15 => "r15",
            _ => unimplemented!(),
        },
        _ => "",
    }
}

// Collection of functions required during a guard failure to instantiate and initialise the
// stopgap interpreter.

/// Given a pointer to a vector of `FrameInfo`s, creates and returns a boxed StopgapInterpreter.
/// Consumes `vptr`.
extern "sysv64" fn new_stopgap(vptr: *mut Vec<FrameInfo>) -> *mut StopgapInterpreter {
    let v = unsafe { Box::from_raw(vptr) };
    let si = StopgapInterpreter::from_frames(*v);
    Box::into_raw(Box::new(si))
}

/// Given a size and alignment, `alloc` a block of memory for storing a frame's live variables.
extern "sysv64" fn alloc_live_vars(size: usize, align: usize) -> *mut u8 {
    let layout = Layout::from_size_align(size, align).unwrap();
    unsafe { alloc(layout) }
}

/// Returns a pointer to a boxed empty `Vec<FrameInfo>`, suitable for passing to `push_frames_vec`.
extern "sysv64" fn new_frames_vec() -> *mut Vec<FrameInfo> {
    let v: Vec<FrameInfo> = Vec::new();
    Box::into_raw(Box::new(v))
}

/// Construct and push a new `FrameInfo` to the `Vec<FrameInfo>`. `sym_ptr` must be a pointer to a
/// function symbol name (of length `sym_len`) that is guaranteed not to be deallocated or moved.
/// `locals` is a pointer to a block of memory from `alloc_live_vars`, responsibility for which is
/// effectively moved to this function (i.e. the caller of `push_frames_vec` should no longer
/// read/write/free `mem`). Note that this function converts the raw pointer `vptr` into an `&mut`
/// reference.
extern "sysv64" fn push_frames_vec(
    vptr: *mut Vec<FrameInfo>,
    sym_ptr: *const u8,
    sym_len: usize,
    bbidx: usize,
    locals: *mut u8,
) {
    let fname =
        unsafe { std::str::from_utf8(std::slice::from_raw_parts(sym_ptr, sym_len)).unwrap() };
    let body = SIR.body(fname).unwrap();
    let fi = FrameInfo {
        body,
        bbidx,
        locals,
    };
    let v = unsafe { &mut *vptr };
    v.push(fi);
}

/// Compile a TIR trace.
pub fn compile_trace(tt: TirTrace) -> CompiledTrace {
    CompiledTrace {
        mc: TraceCompiler::compile(tt, false),
    }
}

/// The `TraceCompiler` takes a `SIRTrace` and compiles it to machine code. Returns a `CompiledTrace`.
pub struct TraceCompiler {
    /// The dynasm assembler which will do all of the heavy lifting of the assembly.
    asm: dynasmrt::x64::Assembler,
    /// Stores the content of each register.
    register_content_map: HashMap<u8, RegAlloc>,
    /// Maps trace locals to their location (register, stack).
    variable_location_map: HashMap<Local, Location>,
    /// Local decls of the TIR trace.
    pub local_decls: HashMap<Local, LocalDecl>,
    /// Stack builder for allocating objects on the stack.
    stack_builder: StackBuilder,
    /// Stores the memory addresses of local functions.
    addr_map: HashMap<String, u64>,
}

impl TraceCompiler {
    pub fn new(local_decls: HashMap<Local, LocalDecl>, addr_map: HashMap<String, u64>) -> Self {
        let mut tc = TraceCompiler {
            asm: dynasmrt::x64::Assembler::new().unwrap(),
            register_content_map: REG_POOL.iter().map(|r| (*r, RegAlloc::Free)).collect(),
            variable_location_map: HashMap::new(),
            local_decls,
            stack_builder: StackBuilder::default(),
            addr_map,
        };

        // At the start of the trace, jump to the label that allocates stack space.
        dynasm!(tc.asm
            ; jmp ->reserve
            ; ->crash:
            ; ud2
            ; ->main:
        );
        tc
    }

    fn can_live_in_register(decl: &LocalDecl) -> bool {
        if decl.referenced {
            // We must allocate it on the stack so that we can reference it.
            return false;
        }

        // FIXME: optimisation: small structs and tuples etc. could actually live in a register.
        let ty = &*SIR.ty(&decl.ty);
        match &ty.kind {
            TyKind::UnsignedInt(ui) => !matches!(ui, UnsignedIntTy::U128),
            TyKind::SignedInt(si) => !matches!(si, SignedIntTy::I128),
            TyKind::Array { .. } => false,
            TyKind::Slice(_) => false,
            TyKind::Ref(_) | TyKind::Bool | TyKind::Char => true,
            TyKind::Struct(..) | TyKind::Tuple(..) => false,
            TyKind::Unimplemented(..) => todo!("{}", ty),
        }
    }

    fn irplace_to_location(&mut self, ip: &IRPlace) -> Location {
        match ip {
            IRPlace::Val { local, off, .. } => self.local_to_location(*local).offset(*off),
            IRPlace::Indirect { ptr, off, .. } => self
                .local_to_location(ptr.local)
                .offset(ptr.off)
                .to_indirect()
                .offset(*off),
            IRPlace::Const { val, ty } => Location::Const {
                val: val.clone(),
                ty: *ty,
            },
            e => todo!("{}", e),
        }
    }

    /// Given a local, returns the register allocation for it, or, if there is no allocation yet,
    /// performs one.
    pub fn local_to_location(&mut self, l: Local) -> Location {
        if l == INTERP_STEP_ARG {
            // There is a register set aside for the interpreter context.
            Location::Reg(*ICTX_REG)
        } else if let Some(location) = self.variable_location_map.get(&l) {
            // We already have a location for this local.
            location.clone()
        } else {
            let decl = &self.local_decls[&l];
            if Self::can_live_in_register(&decl) {
                // Find a free register to store this local.
                let loc = if let Some(reg) = self.get_free_register() {
                    self.register_content_map.insert(reg, RegAlloc::Local(l));
                    Location::Reg(reg)
                } else {
                    // All registers are occupied, so we need to spill the local to the stack.
                    self.spill_local_to_stack(&l)
                };
                let ret = loc.clone();
                self.variable_location_map.insert(l, loc);
                ret
            } else {
                let ty = SIR.ty(&decl.ty);
                let loc = self.stack_builder.alloc(ty.size(), ty.align());
                self.variable_location_map.insert(l, loc.clone());
                loc
            }
        }
    }

    /// Returns a free register or `None` if all registers are occupied.
    fn get_free_register(&self) -> Option<u8> {
        self.register_content_map.iter().find_map(|(k, v)| match v {
            RegAlloc::Free => Some(*k),
            _ => None,
        })
    }

    /// Spill a local to the stack and return its location. Note: This does not update the
    /// `variable_location_map`.
    fn spill_local_to_stack(&mut self, local: &Local) -> Location {
        let tyid = self.local_decls[&local].ty;
        let ty = SIR.ty(&tyid);
        self.stack_builder.alloc(ty.size(), ty.align())
    }

    /// Assign a `Location` to a `Local` turning live. If possible, find it a register for it to
    /// live in, or failing that, allocate space on the stack.
    fn local_live(&mut self, local: &Local) {
        debug_assert!(self.variable_location_map.get(local).is_none());
        // Assign a Location to this Local.
        self.local_to_location(*local);
    }

    /// Notifies the register allocator that a local has died and that its storage may be freed.
    pub fn local_dead(&mut self, local: &Local) -> Result<(), CompileError> {
        match self
            .variable_location_map
            .get(local)
            .expect("freeing unallocated register")
        {
            Location::Reg(reg) => {
                // This local is currently stored in a register, so free the register.
                //
                // Note that if we are marking the reserved ICTX_REG free then this actually adds a
                // new register key to the map (as opposed to marking a pre-existing entry free).
                // This is safe since if we are freeing ICTX_REG, then the interpreter context
                // local must not be used for the remainder of the trace.
                self.register_content_map.insert(*reg, RegAlloc::Free);
            }
            Location::Mem { .. } | Location::Indirect { .. } => {}
            Location::Const { .. } => unreachable!(),
        }
        self.variable_location_map.remove(local);
        Ok(())
    }

    /// Copy bytes from one memory location to another.
    fn copy_memory(&mut self, dst: &RegAndOffset, src: &RegAndOffset, size: u64) {
        // We use memmove(3), as it's not clear if MIR (and therefore SIR) could cause copies
        // involving overlapping buffers. See https://github.com/rust-lang/rust/issues/68364.
        let sym = find_symbol("memmove").unwrap();
        self.save_regs(&*CALLER_SAVED_REGS);
        dynasm!(self.asm
            ; push rax
            ; xor rax, rax
        );

        dynasm!(self.asm
            ; lea rdi, [Rq(dst.reg) + dst.off]
        );
        if src.reg == RDI.code() {
            // If the second argument lived in RDI then we've just overwritten it, so we load it
            // back from when we callee-saved it.
            let mut rdi_stackpos = CALLER_SAVED_REGS
                .iter()
                .rev()
                .position(|r| *r == RDI.code())
                .unwrap();
            // We've also pushed RAX in the meantime.
            rdi_stackpos += 1;
            dynasm!(self.asm
                ; mov rsi, [rsp + i32::try_from(rdi_stackpos).unwrap() * i32::try_from(QWORD_REG_SIZE).unwrap()]
            );
        } else {
            dynasm!(self.asm
                ; lea rsi, [Rq(src.reg) + src.off]
            );
        };
        dynasm!(self.asm
            ; mov rdx, size as i32
            ; mov r11, QWORD sym as i64
            ; call r11
            ; pop rax
        );
        self.restore_regs(&*CALLER_SAVED_REGS);
    }

    /// Push the specified registers to the stack in the order they appear in the array.
    fn save_regs(&mut self, regs: &[u8]) {
        for reg in regs.iter() {
            dynasm!(self.asm
                ; push Rq(reg)
            );
        }
    }
    /// Pop the specified registers from the stack in reverse order to how they appear in the
    /// array.
    fn restore_regs(&mut self, regs: &[u8]) {
        for reg in regs.iter().rev() {
            dynasm!(self.asm
                ; pop Rq(reg)
            );
        }
    }

    /// Compile a call to a native symbol using the Sys-V ABI. This is used for occasions where you
    /// don't want to, or cannot, inline the callee (e.g. it's a foreign function).
    ///
    /// For now we do something very simple. There are limitations (FIXME):
    ///
    ///  - We assume there are no more than 6 arguments (spilling is not yet implemented).
    ///
    ///  - We push all of the callee save registers on the stack, and local variable arguments are
    ///    then loaded back from the stack into the correct ABI-specified registers. We can
    ///    optimise this later by only loading an argument from the stack if it cannot be loaded
    ///    from its original register location (because another argument overwrote it already).
    ///
    ///  - We assume the return value fits in rax. 128-bit return values are not yet supported.
    ///
    ///  - We don't support varags calls.
    fn c_call(
        &mut self,
        opnd: &CallOperand,
        args: &[IRPlace],
        dst: &Option<IRPlace>,
    ) -> Result<(), CompileError> {
        let sym = if let CallOperand::Fn(sym) = opnd {
            sym
        } else {
            todo!("unknown call target");
        };

        if args.len() > 6 {
            todo!("call with spilled args");
        }

        // Save Sys-V caller save registers to the stack, but skip the one (if there is one) that
        // will store the return value. It's safe to assume the caller expects this to be
        // clobbered.
        // OPTIMISE: Only save registers in use by the register allocator.
        let mut save_regs = CALLER_SAVED_REGS.iter().cloned().collect::<Vec<u8>>();
        if let Some(d) = dst {
            let dst_loc = self.irplace_to_location(d);
            if let Location::Reg(dst_reg) = dst_loc {
                // If the result of the call is destined for one of the caller-save registers, then
                // there's no point in saving the register.
                save_regs.retain(|r| *r != dst_reg);
            }
        }
        self.save_regs(&*save_regs);

        // Helper function to find the index of a caller-save register previously pushed to the
        // stack. The first register pushed is at the highest stack offset (from the stack
        // pointer), hence reversing the order of `save_regs`. Returns `None` if `reg` was never
        // saved during caller-save.
        let saved_stack_index = |reg: u8| -> Option<i32> {
            save_regs
                .iter()
                .rev()
                .position(|&r| r == reg)
                .map(|i| i32::try_from(i).unwrap())
        };

        // Sys-V ABI dictates the first 6 arguments are passed in these registers.
        // The order is reversed so they pop() in the right order.
        let mut arg_regs = vec![R9, R8, RCX, RDX, RSI, RDI]
            .iter()
            .map(|r| r.code())
            .collect::<Vec<u8>>();

        for arg in args {
            // In which register will this argument be passed?
            // `unwrap()` must succeed, as we checked there are no more than 6 args above.
            let arg_reg = arg_regs.pop().unwrap();

            // Now load the argument into the correct argument register.
            match self.irplace_to_location(arg) {
                Location::Reg(reg) => {
                    if let Some(idx) = saved_stack_index(reg) {
                        // We saved this register to the stack during caller-save. Since there is
                        // overlap between caller-save registers and argument registers, we may
                        // have overwritten the value in the meantime. So we should load the value
                        // back from the stack.
                        dynasm!(self.asm
                            ; mov Rq(arg_reg), [rsp + idx * i32::try_from(QWORD_REG_SIZE).unwrap()]
                        );
                    } else {
                        // We didn't save this register, so it remains intact.
                        dynasm!(self.asm
                            ; mov Rq(arg_reg), Rq(reg)
                        );
                    }
                }
                Location::Mem(ro) => dynasm!(self.asm
                    ; mov Rq(arg_reg), [Rq(ro.reg) + ro.off]
                ),
                Location::Indirect { .. } => todo!(),
                Location::Const { val, .. } => {
                    // FIXME assumes constant fits in a register.
                    dynasm!(self.asm
                        ; mov Rq(arg_reg), QWORD val.i64_cast()
                    );
                }
            }
        }

        let sym_addr = if let Some(addr) = self.addr_map.get(sym) {
            *addr as i64
        } else {
            // This path is required for system calls. hwtracer doesn't trace through the kernel,
            // so system call addresses will never appear in addr_map.
            find_symbol(sym)? as i64
        };
        dynasm!(self.asm
            // In Sys-V ABI, `al` is a hidden argument used to specify the number of vector args
            // for a vararg call. We don't support this right now, so set it to zero.
            ; xor rax, rax
            ; mov Rq(*TEMP_REG), QWORD sym_addr
            ; call Rq(*TEMP_REG)
            // Stash return value. We do this because restore_regs() below may clobber RAX.
            ; mov Rq(*TEMP_REG), rax
        );

        // Restore caller-save registers.
        self.restore_regs(&save_regs);

        if let Some(d) = dst {
            let dst_loc = self.irplace_to_location(d);
            self.store_raw(&dst_loc, &Location::Reg(*TEMP_REG), SIR.ty(&d.ty()).size());
        }

        Ok(())
    }

    /// Load an IRPlace into the given register. Panic if it doesn't fit.
    fn load_reg_irplace(&mut self, reg: u8, src_ip: &IRPlace) -> Location {
        let dst_loc = Location::Reg(reg);
        let src_loc = self.irplace_to_location(src_ip);
        self.store_raw(&dst_loc, &src_loc, SIR.ty(&src_ip.ty()).size());
        dst_loc
    }

    fn c_binop(
        &mut self,
        dst: &IRPlace,
        op: BinOp,
        opnd1: &IRPlace,
        opnd2: &IRPlace,
        checked: bool,
    ) {
        let opnd1_ty = SIR.ty(&opnd1.ty());
        debug_assert!(opnd1_ty == SIR.ty(&opnd2.ty()));

        // For now this whole function assumes we are operating on integers.
        if !opnd1_ty.is_int() {
            todo!("binops for non-integers");
        }

        match op {
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                return self.c_condition(dst, &op, opnd1, opnd2);
            }
            _ => {}
        }

        // We do this in three stages.
        // 1) Copy the first operand into the temp register.
        self.load_reg_irplace(*TEMP_REG, opnd1);

        // 2) Perform arithmetic.
        match op {
            BinOp::Add => self.c_binop_add(*TEMP_REG, opnd2),
            BinOp::Sub => self.c_binop_sub(*TEMP_REG, opnd2),
            BinOp::Mul => {
                if opnd1_ty.is_signed_int() {
                    todo!("signed mul"); // use IMUL
                } else {
                    self.c_binop_mul(*TEMP_REG, opnd2);
                }
            }
            BinOp::Div => {
                if opnd1_ty.is_signed_int() {
                    todo!("signed div"); // use IDIV
                } else {
                    self.c_binop_div(*TEMP_REG, opnd2);
                }
            }
            _ => todo!(),
        }

        // 3) Move the result to where it is supposed to live.
        let dst_loc = self.irplace_to_location(dst);
        let size = opnd1_ty.size();
        if checked {
            // If it is a checked operation, then we have to build a (value, overflow-flag) tuple.
            // Let's do the flag first, so as to read EFLAGS closest to where they are set.
            let dst_ro = dst_loc.unwrap_mem();
            let sir_ty = SIR.ty(&dst.ty());
            let tty = sir_ty.unwrap_tuple();
            let flag_off = tty.fields.offsets[1];

            if opnd1_ty.is_signed_int() {
                dynasm!(self.asm
                    ; jo >overflow
                );
            } else {
                dynasm!(self.asm
                    ; jc >overflow
                );
            }
            dynasm!(self.asm
                ; mov BYTE [Rq(dst_ro.reg) + dst_ro.off + flag_off], 0
                ; jmp >done
                ; overflow:
                ; mov BYTE [Rq(dst_ro.reg) + dst_ro.off + flag_off], 1
                ; done:
            );
        }
        self.store_raw(&dst_loc, &*TEMP_LOC, size);
    }

    binop_add_sub!(c_binop_add, add);
    binop_add_sub!(c_binop_sub, sub);
    binop_mul_div!(c_binop_mul, mul);
    binop_mul_div!(c_binop_div, div);

    fn c_condition(&mut self, dst: &IRPlace, binop: &BinOp, op1: &IRPlace, op2: &IRPlace) {
        let src1 = self.irplace_to_location(op1);
        let ty = SIR.ty(&op1.ty());

        self.load_reg_irplace(*TEMP_REG, op2);

        match &src1 {
            Location::Reg(reg) => match ty.size() {
                1 => {
                    dynasm!(self.asm
                        ; cmp Rb(reg), Rb(*TEMP_REG)
                    );
                }
                2 => {
                    dynasm!(self.asm
                        ; cmp Rw(reg), Rw(*TEMP_REG)
                    );
                }
                4 => {
                    dynasm!(self.asm
                        ; cmp Rd(reg), Rd(*TEMP_REG)
                    );
                }
                8 => {
                    dynasm!(self.asm
                        ; cmp Rq(reg), Rq(*TEMP_REG)
                    );
                }
                _ => unreachable!(),
            },
            Location::Mem(ro) => match ty.size() {
                1 => {
                    dynasm!(self.asm
                        ; cmp BYTE [Rq(ro.reg) + ro.off], Rb(*TEMP_REG)
                    );
                }
                2 => {
                    dynasm!(self.asm
                        ; cmp WORD [Rq(ro.reg) + ro.off], Rw(*TEMP_REG)
                    );
                }
                4 => {
                    dynasm!(self.asm
                        ; cmp DWORD [Rq(ro.reg) + ro.off], Rd(*TEMP_REG)
                    );
                }
                8 => {
                    dynasm!(self.asm
                        ; cmp QWORD [Rq(ro.reg) + ro.off], Rq(*TEMP_REG)
                    );
                }
                _ => unreachable!(),
            },
            _ => todo!(),
        }
        dynasm!(self.asm
         ; mov Rq(*TEMP_REG), 1
        );
        match binop {
            BinOp::Eq => {
                dynasm!(self.asm
                    ; je >skip
                );
            }
            BinOp::Ne => {
                dynasm!(self.asm
                    ; jne >skip
                );
            }
            BinOp::Lt => {
                dynasm!(self.asm
                    ; jl >skip
                );
            }
            BinOp::Le => {
                dynasm!(self.asm
                    ; jle >skip
                );
            }
            BinOp::Gt => {
                dynasm!(self.asm
                    ; jg >skip
                );
            }
            BinOp::Ge => {
                dynasm!(self.asm
                    ; jge >skip
                );
            }
            _ => unreachable!(),
        }
        dynasm!(self.asm
         ; mov Rq(*TEMP_REG), 0
         ; skip:
        );
        let dst_loc = self.irplace_to_location(dst);
        self.store_raw(&dst_loc, &*TEMP_LOC, SIR.ty(&dst.ty()).size());
    }

    fn c_dynoffs(&mut self, dst: &IRPlace, base: &IRPlace, idx: &IRPlace, scale: u32) {
        // FIXME possible optimisation, use LEA if scale fits in a u8.

        // MUL clobbers RDX:RAX, so store/restore those.
        // FIXME only do this if these registers are allocated?
        dynasm!(self.asm
            ; push rax
            ; push rdx
        );

        // 1) Multiply scale by idx, store in RAX.
        self.load_reg_irplace(RAX.code(), idx);
        dynasm!(self.asm
            ; mov Rq(*TEMP_REG), i32::try_from(scale).unwrap()
            ; mul Rq(*TEMP_REG)
            ; jo ->crash
        );

        // 2) Get the address of the thing we want to offset into a register.
        let base_loc = self.irplace_to_location(base);
        match base_loc {
            Location::Reg(..) => todo!(),
            Location::Mem(..) => todo!(),
            Location::Indirect { ptr, off } => match ptr {
                IndirectLoc::Reg(..) => todo!(),
                IndirectLoc::Mem(ind_ro) => {
                    dynasm!(self.asm
                        ; mov Rq(*TEMP_REG), [Rq(ind_ro.reg) + ind_ro.off]
                        ; add Rq(*TEMP_REG), off
                        ; jo ->crash
                    );
                }
            },
            Location::Const { .. } => todo!(),
        }

        // 3) Apply the offset.
        dynasm!(self.asm
            ; add Rq(*TEMP_REG), rax
            ; jo ->crash
        );

        // Restore what we saved earlier.
        dynasm!(self.asm
            ; pop rdx
            ; pop rax
        );

        // 4) Store the resulting pointer into the destination.
        // The IR is constructed such that `dst_loc` will be indirect to ensure that subsequent
        // operations on this locatiion dereference the pointer.
        let dst_loc = self.irplace_to_location(dst);
        self.store_raw(&dst_loc, &*TEMP_LOC, *PTR_SIZE);
    }

    /// Compile a TIR statement.
    fn c_statement(&mut self, stmt: &Statement) -> Result<(), CompileError> {
        match stmt {
            Statement::Store(dst, src) => self.c_istore(dst, src),
            Statement::BinaryOp {
                dst,
                op,
                opnd1,
                opnd2,
                checked,
            } => self.c_binop(dst, *op, opnd1, opnd2, *checked),
            Statement::MkRef(dst, src) => self.c_mkref(dst, src),
            Statement::DynOffs {
                dst,
                base,
                idx,
                scale,
            } => self.c_dynoffs(dst, base, idx, *scale),
            Statement::StorageLive(l) => self.local_live(l),
            Statement::StorageDead(l) => self.local_dead(l)?,
            Statement::Call(target, args, dst) => self.c_call(target, args, dst)?,
            Statement::Cast(dst, src) => self.c_cast(dst, src),
            Statement::LoopStart => {
                dynasm!(self.asm
                    ; ->loop_start:
                );
            }
            Statement::LoopEnd => {
                dynasm!(self.asm
                    ; jmp ->loop_start
                );
            }
            Statement::Nop | Statement::Debug(..) => {}
            Statement::Unimplemented(s) => todo!("{:?}", s),
        }

        Ok(())
    }

    fn c_mkref(&mut self, dst: &IRPlace, src: &IRPlace) {
        let src_loc = self.irplace_to_location(src);
        match src_loc {
            Location::Reg(..) => {
                // This isn't possible as the allocator explicitly puts things which are
                // referenced onto the stack and never in registers.
                unreachable!()
            }
            Location::Mem(ref ro) => {
                debug_assert!(src_loc.uses_reg() != Some(*TEMP_REG));
                dynasm!(self.asm
                    ; lea Rq(*TEMP_REG), [Rq(ro.reg) + ro.off]
                );
            }
            Location::Const { .. } => todo!(),
            Location::Indirect { ref ptr, off } => {
                debug_assert!(src_loc.uses_reg() != Some(*TEMP_REG));
                match ptr {
                    IndirectLoc::Reg(reg) => {
                        dynasm!(self.asm
                            ; lea Rq(*TEMP_REG), [Rq(reg) + off]
                        );
                    }
                    IndirectLoc::Mem(ro) => {
                        dynasm!(self.asm
                            ; lea Rq(*TEMP_REG), [Rq(ro.reg) + ro.off]
                            ; mov Rq(*TEMP_REG), [Rq(*TEMP_REG)]
                            ; add Rq(*TEMP_REG), off
                        );
                    }
                }
            }
        }
        let dst_loc = self.irplace_to_location(dst);
        debug_assert_eq!(SIR.ty(&dst.ty()).size(), *PTR_SIZE);
        self.store_raw(&dst_loc, &*TEMP_LOC, *PTR_SIZE);
    }

    fn c_cast(&mut self, dst: &IRPlace, src: &IRPlace) {
        let src_loc = self.irplace_to_location(src);
        let ty = &*SIR.ty(&src.ty()); // Type of the source.
        let cty = SIR.ty(&dst.ty()); // Type of the cast.
        match ty.kind {
            TyKind::UnsignedInt(_) => self.c_cast_uint(src_loc, &ty, &cty),
            _ => todo!(),
        }
        let dst_loc = self.irplace_to_location(dst);
        self.store_raw(&dst_loc, &*TEMP_LOC, SIR.ty(&dst.ty()).size());
    }

    fn c_cast_uint(&mut self, src: Location, ty: &Ty, cty: &Ty) {
        match src {
            Location::Reg(reg) => {
                match cty.size() {
                    1 => {
                        dynasm!(self.asm
                            ; mov Rb(*TEMP_REG), Rb(reg)
                        );
                    }
                    2 => match ty.size() {
                        1 => {
                            dynasm!(self.asm
                                ; movzx Rw(*TEMP_REG), Rb(reg)
                            );
                        }
                        2 | 4 | 8 => {
                            dynasm!(self.asm
                                ; mov Rw(*TEMP_REG), Rw(reg)
                            );
                        }
                        _ => todo!("{}", ty.size()),
                    },
                    4 => match ty.size() {
                        1 => {
                            dynasm!(self.asm
                                ; movzx Rd(*TEMP_REG), Rb(reg)
                            );
                        }
                        2 => {
                            dynasm!(self.asm
                                ; movzx Rd(*TEMP_REG), Rw(reg)
                            );
                        }
                        4 | 8 => {
                            dynasm!(self.asm
                                ; mov Rd(*TEMP_REG), Rd(reg)
                            );
                        }
                        _ => todo!("{}", ty.size()),
                    },
                    8 => {
                        match ty.size() {
                            1 => {
                                dynasm!(self.asm
                                    ; movzx Rq(*TEMP_REG), Rb(reg)
                                );
                            }
                            2 => {
                                dynasm!(self.asm
                                    ; movzx Rq(*TEMP_REG), Rw(reg)
                                );
                            }
                            4 => {
                                // mov reg32, reg32 in x64 automatically zero-extends to
                                // reg64.
                                dynasm!(self.asm
                                    ; mov Rd(*TEMP_REG), Rd(reg)
                                );
                            }
                            8 => {
                                dynasm!(self.asm
                                    ; mov Rq(*TEMP_REG), Rq(reg)
                                );
                            }
                            _ => todo!("{}", ty.size()),
                        }
                    }
                    _ => todo!("{}", ty.size()),
                }
            }
            Location::Mem(_) => todo!(),
            Location::Indirect { .. } => todo!(),
            Location::Const { .. } => todo!(),
        }
    }

    fn c_istore(&mut self, dst: &IRPlace, src: &IRPlace) {
        self.store(dst, src);
    }

    /// Compile a guard in the trace, emitting code to abort execution in case the guard fails.
    fn c_guard(&mut self, guard: &Guard, dl: DynamicLabel) {
        // FIXME some of the terminators from which we build these guards can have cleanup blocks.
        // Currently we don't run any cleanup, but should we?
        match guard {
            Guard {
                val,
                kind: GuardKind::OtherInteger(v),
                ..
            } => match self.irplace_to_location(val) {
                Location::Reg(reg) => {
                    for c in v {
                        self.cmp_reg_const(reg, *c, SIR.ty(&val.ty()).size());
                        dynasm!(self.asm
                            ; je =>dl
                        );
                    }
                }
                Location::Mem(ro) => {
                    dynasm!(self.asm
                        ; mov Rq(*TEMP_REG), QWORD [Rq(ro.reg) + ro.off]
                    );
                    for c in v {
                        self.cmp_reg_const(*TEMP_REG, *c, SIR.ty(&val.ty()).size());
                        dynasm!(self.asm
                            ; je =>dl
                        );
                    }
                }
                _ => todo!(),
            },
            Guard {
                val,
                kind: GuardKind::Integer(c),
                ..
            } => match self.irplace_to_location(val) {
                Location::Reg(reg) => {
                    self.cmp_reg_const(reg, *c, SIR.ty(&val.ty()).size());
                    dynasm!(self.asm
                        ; jne =>dl
                    );
                }
                Location::Mem(ro) => {
                    dynasm!(self.asm
                        ; mov Rq(*TEMP_REG), QWORD [Rq(ro.reg) + ro.off]
                    );
                    self.cmp_reg_const(*TEMP_REG, *c, SIR.ty(&val.ty()).size());
                    dynasm!(self.asm
                        ; jne =>dl
                    );
                }
                Location::Indirect { ptr, off } => {
                    match ptr {
                        IndirectLoc::Reg(reg) => {
                            dynasm!(self.asm
                                ; mov Rq(*TEMP_REG), QWORD [Rq(reg) + off]
                            );
                        }
                        IndirectLoc::Mem(src_ro) => {
                            dynasm!(self.asm
                                ; mov Rq(*TEMP_REG), QWORD [Rq(src_ro.reg) + src_ro.off]
                                ; mov Rq(*TEMP_REG), QWORD [Rq(*TEMP_REG) + off]
                            );
                        }
                    }
                    self.cmp_reg_const(*TEMP_REG, *c, SIR.ty(&val.ty()).size());
                    dynasm!(self.asm
                        ; jne =>dl
                    );
                }
                _ => todo!(),
            },
            Guard {
                val,
                kind: GuardKind::Boolean(expect),
                ..
            } => match self.irplace_to_location(val) {
                Location::Reg(reg) => {
                    dynasm!(self.asm
                        ; cmp Rb(reg), *expect as i8
                        ; jne =>dl
                    );
                }
                Location::Mem(ro) => {
                    dynasm!(self.asm
                        ; cmp BYTE [Rq(ro.reg) + ro.off], *expect as i8
                        ; jne =>dl
                    );
                }
                _ => todo!(),
            },
        }
    }

    fn cmp_reg_const(&mut self, reg: u8, c: u128, size: u64) {
        match size {
            1 => {
                dynasm!(self.asm
                    ; cmp Rb(reg), i8::try_from(c).unwrap()
                );
            }
            2 => {
                dynasm!(self.asm
                    ; cmp Rw(reg), i16::try_from(c).unwrap()
                );
            }
            4 => {
                dynasm!(self.asm
                    ; cmp Rd(reg), i32::try_from(c).unwrap()
                );
            }
            8 => {
                dynasm!(self.asm
                    ; cmp Rq(reg), i32::try_from(c).unwrap()
                );
            }
            _ => todo!(),
        }
    }

    /// Print information about the state of the compiler and exit.
    fn crash_dump(self, e: Option<CompileError>) -> ! {
        eprintln!("\nThe trace compiler crashed!\n");

        if let Some(e) = e {
            eprintln!("Reason: {}.\n", e);
        } else {
            eprintln!("Reason: unknown");
        }

        // To help us figure out what has gone wrong, we can print the disassembled instruction
        // stream with the help of `rasm2`.
        eprintln!("Executable code buffer:");
        let code = &*self.asm.finalize().unwrap();
        if code.is_empty() {
            eprintln!("  <empty buffer>");
        } else {
            let hex_code = hex::encode(code);
            let res = Command::new("rasm2")
                .arg("-d")
                .arg("-b 64") // x86_64.
                .arg(hex_code.clone())
                .output()
                .unwrap();
            if !res.status.success() {
                eprintln!("  Failed to invoke rasm2. Raw bytes follow...");
                eprintln!("  {}", hex_code);
            } else {
                let asm = String::from_utf8(res.stdout).unwrap();
                for line in asm.lines() {
                    eprintln!("  {}", line);
                }
            }
        }

        // Print the register allocation.
        eprintln!("\nRegister allocation (place -> reg):");
        for (place, location) in &self.variable_location_map {
            eprintln!(
                "  {:2} -> {:?} ({})",
                place,
                location,
                local_to_reg_name(location)
            );
        }
        eprintln!();

        panic!("stopped due to trace compilation error");
    }

    /// Finalise the compiled trace returning an ExecutableBuffer. Consumes self.
    fn finish(
        mut self,
        gl: Vec<(&Guard, HashMap<&Local, Location>, DynamicLabel)>,
    ) -> ExecutableBuffer {
        // Reset the stack/base pointers and return from the trace. We also need to generate the
        // code that reserves stack space for spilled locals here, since we don't know at the
        // beginning of the trace how many locals are going to be spilled.
        // We also need to reserve space for any live local that remains in a register at the point
        // of a guard failure. Finally, we push the CALLEE-SAVED registers onto the stack, so the
        // stack looks as follows:
        // RSP                                         RBP
        // +------------+-------------+----------------+-------+-----+
        // | SAVED REGS | LIVE LOCALS | SPILLED LOCALS | ALIGN | RBP |
        // +------------+-------------+----------------+-------+-----+

        // Reserved stack space for spilled locals during execution.
        let soff = self.stack_builder.size();
        // Reserved memory on the stack to spill live locals to during a guard failure.
        let live_off = REG_POOL.len() * QWORD_REG_SIZE;

        // After allocating stack space for the trace, we pad the stack pointer up to the next
        // 16-byte alignment boundary. Calls can use this fact when catering for alignment
        // requirements of callees (if necessary). Interim pushes and pops in trace code are
        // allowed as long as they are short-lived and correctly restore 16-byte alignment.
        let topalign = SYSV_CALL_STACK_ALIGN
            - (live_off + soff as usize + CALLEE_SAVED_REGS.len() * QWORD_REG_SIZE)
                % SYSV_CALL_STACK_ALIGN;

        dynasm!(self.asm
            ; mov rax, 0 // Signifies that there were no guard failures.
            ; ->cleanup:
        );
        // Exit from the trace, by resetting the stack and previously saved registers.
        self.restore_regs(&*CALLEE_SAVED_REGS);
        dynasm!(self.asm
            ; add rsp, live_off as i32
            ; add rsp, soff as i32
            ; add rsp, topalign as i32
            ; pop rbp
            ; ret
        );
        // Generate guard failure code.
        self.compile_guards(gl, soff);
        // Initialise the compiled trace, by aligning the stack, reserving space for spilling, and
        // saving callee-saved registers.
        dynasm!(self.asm
            ; ->reserve:
            ; push rbp
            ; sub rsp, topalign as i32
            ; mov rbp, rsp
            ; sub rsp, soff as i32
            ; sub rsp, live_off as i32
        );
        self.save_regs(&*CALLEE_SAVED_REGS);
        dynasm!(self.asm
            ; jmp ->main
        );
        // Return executable buffer.
        self.asm.finalize().unwrap()
    }

    fn compile_guards(
        &mut self,
        gl: Vec<(&Guard, HashMap<&Local, Location>, DynamicLabel)>,
        soff: u32,
    ) {
        // Output code for guard failure. This consists of a label for the guard to jump to and
        // various code to get the system in the appropriate state for a StopgapInterpreter to take
        // over execution.
        for (guard, mut live_locations, label) in gl {
            // Symbol names of the functions called while executing the trace. Needed to recreate
            // the stack frames in the StopgapInterpreter.
            let mut sym_labels = Vec::new();
            for block in &guard.blocks {
                let dynlbl = self.asm.new_dynamic_label();
                dynasm!(self.asm
                    ; => dynlbl
                    ; .bytes block.symbol_name.as_bytes()
                );
                sym_labels.push(dynlbl);
            }

            // The beginning of the guard code.
            dynasm!(self.asm
                ; => label
            );

            // Spill all registers holding live values to the stack. This serves two purposes:
            // first, it frees up the registers for us to use for other purposes; second, it means
            // that the stack frame reconstruction only has to deal with stack entries and can
            // ignore registers entirely.
            for (tirlocal, loc) in live_locations.iter_mut() {
                if let Location::Reg(reg) = loc {
                    let off = -i32::try_from(
                        soff + u32::from(*reg) * u32::try_from(QWORD_REG_SIZE).unwrap(),
                    )
                    .unwrap();
                    let newloc = Location::Mem(RegAndOffset {
                        reg: RBP.code(),
                        off,
                    });
                    let ty = self.local_decls[&tirlocal].ty;
                    let size = SIR.ty(&ty).size();
                    self.store_raw(&newloc, &loc, size);
                    *loc = newloc;
                }
            }

            // The trace has a single stack frame. However, since the trace can have inlined an
            // arbitrary number of functions, at any given point in the trace we've effectively
            // squashed multiple stack frames down into one. When a guard fails, we have to
            // reconstruct these stack frames in the format that the StopgapInterpreter needs. This
            // will have the same number of stack frames as the "real" executable would have,
            // though we store them in a different format (the StopgapInterpreter has its own
            // layout of stack frames).
            //
            // First we create the Vec that we'll store the new stack frames into.
            let frame_vec_reg = R12.code();
            dynasm!(self.asm
                ; mov r11, QWORD new_frames_vec as i64
                ; call r11
                ; mov Rq(frame_vec_reg), rax
            );

            // Second we iterate over all the functions that are "active" at the point of the
            // guard: each maps to a new stack frame.
            for (i, block) in guard.blocks.iter().enumerate() {
                let sym = block.symbol_name;
                let bbidx = block.bb_idx;
                let body = SIR.body(sym).unwrap();
                let sym_label = sym_labels[i];

                // Allocate memory for the live variables.
                dynasm!(self.asm
                    ; mov rdi, i32::try_from(body.layout.0).unwrap()
                    ; mov rsi, i32::try_from(body.layout.1).unwrap()
                    ; mov r11, QWORD alloc_live_vars as i64
                    ; call r11
                );
                // Move the live variables into the allocated memory.
                for liveloc in &guard.live_locals[i] {
                    let ty = self.local_decls[&liveloc.tir].ty;
                    let size = SIR.ty(&ty).size();
                    let off = i32::try_from(body.offsets[usize::try_from(liveloc.sir.0).unwrap()])
                        .unwrap();
                    let dst_loc = Location::Mem(RegAndOffset {
                        reg: RAX.code(),
                        off,
                    });
                    let src_loc = &live_locations[&liveloc.tir];
                    self.store_raw(&dst_loc, &src_loc, size);
                }

                // Push the new stack frame to the Vec.
                dynasm!(self.asm
                    ; mov rdi, Rq(frame_vec_reg)
                    ; lea rsi, [=>sym_label]
                    ; mov rdx, sym.len() as i32
                    ; mov rcx, bbidx as i32
                    ; mov r8, rax // allocated memory
                    ; mov r11, QWORD push_frames_vec as i64
                    ; call r11
                );
            }

            // Create and initialise SIR interpreter, then return it.
            //
            // There's no need to save any registers here, since we immediately jump to cleanup
            // afterwards, which exits the trace.
            dynasm!(self.asm
                ; mov rdi, Rq(frame_vec_reg)
                ; xor rax, rax
                ; mov r11, QWORD new_stopgap as i64
                ; call r11
                ; jmp ->cleanup
            );
        }
    }

    fn compile(mut tt: TirTrace, debug: bool) -> dynasmrt::ExecutableBuffer {
        let mut tc: Self = TraceCompiler::new(
            tt.local_decls.clone(),
            tt.addr_map.drain().into_iter().collect(),
        );
        let mut gl = Vec::new();
        for i in 0..tt.len() {
            let res = match unsafe { tt.op(i) } {
                TirOp::Statement(st) => tc.c_statement(st),
                TirOp::Guard(g) => {
                    let dl = tc.asm.new_dynamic_label();
                    tc.c_guard(g, dl);
                    // As the locations of live variables may change throughout the trace, we need
                    // to save them here for each guard, so when a guard fails we know from which
                    // location to retrieve the live variables' values.
                    let mut live_locations = HashMap::new();
                    for v in &g.live_locals {
                        for liveloc in v {
                            let loc = tc.local_to_location(liveloc.tir);
                            live_locations.insert(&liveloc.tir, loc);
                        }
                    }
                    gl.push((g, live_locations, dl));
                    Ok(())
                }
            };

            // FIXME -- Later errors should not be fatal. We should be able to abort trace
            // compilation and carry on.
            match res {
                Ok(_) => (),
                Err(e) => tc.crash_dump(Some(e)),
            }
        }
        let buf = tc.finish(gl);
        if debug {
            // In debug mode the memory section which contains the compiled trace is marked as
            // writeable, which enables gdb/lldb to set breakpoints within the compiled code.
            unsafe {
                let ptr = buf.ptr(dynasmrt::AssemblyOffset(0)) as *mut libc::c_void;
                let len = buf.len();
                let alignment = ptr as usize % libc::sysconf(libc::_SC_PAGESIZE) as usize;
                let ptr = ptr.sub(alignment);
                let len = len + alignment;
                libc::mprotect(ptr, len, libc::PROT_EXEC | libc::PROT_WRITE);
            }
        }
        buf
    }
}
