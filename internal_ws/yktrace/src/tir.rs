//! Conceptually this module takes an ordered collection of SIR block locations and converts it
//! into a Tracing IR (TIR) Trace using the SIR found in the `.yk_sir` section of the currently
//! running executable.

use super::SirTrace;
use crate::{
    errors::InvalidTraceError,
    sir::{self, Sir, INTERP_STEP_ARG},
};
use std::{
    collections::{HashMap, HashSet},
    convert::TryFrom,
    fmt::{self, Display, Write},
};
pub use ykpack::{
    BinOp, BodyFlags, CallOperand, Constant, ConstantInt, IRPlace, Local, LocalDecl, LocalIndex,
    Ptr, SignedInt, Statement, Terminator, UnsignedInt,
};

/// A TIR trace is conceptually a straight-line path through the SIR with guarded speculation.
#[derive(Debug)]
pub struct TirTrace<'a, 'm> {
    ops: Vec<TirOp>,
    /// Maps each local variable to its declaration, including type.
    pub local_decls: HashMap<Local, LocalDecl>,
    pub addr_map: HashMap<String, u64>,
    sir: &'a Sir<'m>,
}

impl<'a, 'm> TirTrace<'a, 'm> {
    /// Create a TirTrace from a SirTrace, trimming remnants of the code which starts/stops the
    /// tracer. Returns a TIR trace and the bounds the SIR trace was trimmed to, or Err if a symbol
    /// is encountered for which no SIR is available.
    pub fn new<'s>(sir: &'a Sir<'m>, trace: &'s SirTrace) -> Result<Self, InvalidTraceError> {
        let mut ops = Vec::new();
        let mut itr = trace.iter().peekable();
        let mut rnm = VarRenamer::new();
        // Symbol name of the function currently being ignored during tracing.
        let mut dnt_func: Option<String> = None;
        // Number matching calls and returns of the current `do_not_trace` function.
        let mut dnt_count: usize = 0;
        // Maps symbol names to their virtual addresses.
        let mut addr_map: HashMap<String, u64> = HashMap::new();

        // A stack to keep track of where to store return values of inlined calls. When we
        // encounter `$x = Call(...)` we push `$x` to the stack so that later, when we encounter
        // the corresponding Return, we can find the correct place to store the return value (by
        // popping from the stack).
        let mut return_irplaces: Vec<IRPlace> = Vec::new();

        let mut live_locals: Vec<HashSet<Local>> = Vec::new();
        let mut guard_blocks: Vec<GuardBlock> = Vec::new();

        let mut loop_trace = false;
        let mut in_interp_step = false;
        let mut entered_call = false;
        while let Some(loc) = itr.next() {
            let body = match sir.body(&loc.symbol_name) {
                Some(b) => b,
                None => {
                    return Err(InvalidTraceError::no_sir(&loc.symbol_name));
                }
            };

            // Ignore yktrace::trace_debug.
            // We don't use the 'do_not_trace' machinery below, as that would require the TraceDebugCall
            // terminator to contain the symbol name, which would be wasteful.
            if body.flags.contains(BodyFlags::TRACE_DEBUG) {
                continue;
            }

            // Initialise VarRenamer's accumulator (and thus also set the first offset) to the
            // traces most outer number of locals.
            rnm.init_acc(body.local_decls.len());

            // When adding statements to the trace, we clone them (rather than referencing the
            // statements in the SIR) so that we have the freedom to mutate them later.
            let user_bb_idx_usize = usize::try_from(loc.bb_idx).unwrap();

            // When we see the first block of a SirFunc, store its virtual address so we can turn
            // this function into a `Call` if the user decides not to trace it.
            let addr = &loc.addr;
            if user_bb_idx_usize == 0 {
                addr_map.insert(loc.symbol_name.to_string(), addr.unwrap());
            }

            // If a function was annotated with `do_not_trace`, we need to skip all instructions
            // within it. However, since the tracer will have inlined function calls, the only way
            // to detect that the `do_not_trace` function has finished, is by observing its return
            // terminator. Since the `do_not_trace` function can be recursive we count how many
            // times it is called and returned from and only stop ignoring things until we return
            // from the outer-most `do_not_trace` function.
            if let Some(sym) = &dnt_func {
                match &body.blocks[user_bb_idx_usize].term {
                    Terminator::Return => {
                        if sym == loc.symbol_name {
                            dnt_count -= 1;
                            if dnt_count == 0 {
                                dnt_func = None;
                            }
                        }
                    }
                    Terminator::Call { operand, .. } => {
                        if let Some(csym) = operand.symbol() {
                            if csym == sym {
                                dnt_count += 1;
                            }
                        }
                    }
                    _ => {}
                };
                continue;
            }

            // If we are not in the `interp_step` function, then ignore statements.
            if in_interp_step {
                if entered_call {
                    entered_call = false;
                    // For each new function we enter during the trace, create a new guard block.
                    // The list of guard blocks is later added to the guard, enabling us to
                    // recreate the stack frames for the stopgap interpreter.
                    guard_blocks.push(GuardBlock {
                        symbol_name: loc.symbol_name,
                        bb_idx: loc.bb_idx,
                    });
                } else {
                    // Update the basic block index of the current guard block, if there is one
                    // (i.e. when we see a terminator that isn't a call, e.g. Goto, we update the
                    // current guard block instead of creating a new one).
                    if let Some(gblock) = guard_blocks.last_mut() {
                        gblock.bb_idx = loc.bb_idx;
                    }
                }

                // When converting the SIR trace into a TIR trace we alpha-rename the `Local`s from
                // inlined functions by adding an offset to each. This offset is derived from the
                // number of assigned variables in the functions outer context. For example, if a
                // function `bar` is inlined into a function `foo`, and `foo` used 5 variables, then
                // all variables in `bar` are offset by 5.
                for stmt in body.blocks[user_bb_idx_usize].stmts.iter() {
                    let op = match stmt {
                        Statement::MkRef(dst, src) => Statement::MkRef(
                            rnm.rename_irplace(dst, &body),
                            rnm.rename_irplace(src, &body),
                        ),
                        Statement::DynOffs {
                            dst,
                            base,
                            idx,
                            scale,
                        } => Statement::DynOffs {
                            dst: rnm.rename_irplace(dst, &body),
                            base: rnm.rename_irplace(base, &body),
                            idx: rnm.rename_irplace(idx, &body),
                            scale: *scale,
                        },
                        Statement::Store(dst, src) => {
                            if matches!(dst, IRPlace::Val { local, .. } if local == &sir::RETURN_LOCAL)
                            {
                                if body.flags.contains(BodyFlags::INTERP_STEP) {
                                    match src {
                                        IRPlace::Const { val: Constant::Bool(b), ..} => {
                                            // If the `interp_step` function returns false, we
                                            // enabled trace looping which loops the trace
                                            // indefinitely until a guard fails.
                                            if *b == false {
                                                loop_trace = true;
                                            }
                                        }
                                        _ => {
                                            panic!("Return value of `interp_step` function must be a constant bool.")
                                        }
                                    }
                                }
                            }
                            Statement::Store(
                                rnm.rename_irplace(dst, &body),
                                rnm.rename_irplace(src, &body),
                            )
                        }
                        Statement::BinaryOp {
                            dst,
                            op,
                            opnd1,
                            opnd2,
                            checked,
                        } => Statement::BinaryOp {
                            dst: rnm.rename_irplace(dst, &body),
                            op: *op,
                            opnd1: rnm.rename_irplace(opnd1, &body),
                            opnd2: rnm.rename_irplace(opnd2, &body),
                            checked: *checked,
                        },
                        Statement::Nop => stmt.clone(),
                        Statement::Unimplemented(_) | Statement::Debug(_) => stmt.clone(),
                        Statement::Cast(dst, src) => Statement::Cast(
                            rnm.rename_irplace(dst, &body),
                            rnm.rename_irplace(src, &body),
                        ),
                        Statement::StorageLive(local) => {
                            let l = rnm.rename_local(local, &body);
                            live_locals.last_mut().unwrap().insert(l);
                            Statement::StorageLive(l)
                        }
                        Statement::StorageDead(local) => {
                            let l = rnm.rename_local(local, &body);
                            live_locals.last_mut().unwrap().remove(&l);
                            Statement::StorageDead(l)
                        }
                        // The following statements are specific to TIR and cannot appear in SIR.
                        Statement::Call(..) | Statement::LoopStart | Statement::LoopEnd => {
                            unreachable!()
                        }
                    };

                    // In TIR, stores to local number zero are always to the return value of the
                    // #[interp_step] function. We know this is unit so we can ignore it.
                    if let Statement::Store(
                        IRPlace::Val {
                            local: sir::RETURN_LOCAL,
                            ..
                        },
                        _,
                    ) = op
                    {
                        debug_assert!(sir.ty(&rnm.local_decls[&sir::RETURN_LOCAL].ty).is_unit());
                        continue;
                    }

                    let op = TirOp::Statement(op);
                    ops.push(op);
                }
            }

            if let Terminator::Call {
                operand: op,
                args: _,
                destination: _,
            } = &body.blocks[user_bb_idx_usize].term
            {
                if let Some(callee_sym) = op.symbol() {
                    if let Some(callee_body) = sir.body(callee_sym) {
                        if callee_body.flags.contains(BodyFlags::INTERP_STEP) {
                            if in_interp_step {
                                panic!("recursion into interp_step detected");
                            }

                            // FIXME This means we add this call terminator to the statements, even
                            // though the rest of this block was skipped.
                            in_interp_step = true;
                            entered_call = true;
                            live_locals.push(HashSet::new());
                            live_locals.last_mut().unwrap().insert(INTERP_STEP_ARG);
                            continue;
                        }
                    }
                }
            }

            if !in_interp_step {
                continue;
            }

            // Each SIR terminator becomes zero or more TIR statements.
            let mut term_stmts = Vec::new();
            match &body.blocks[user_bb_idx_usize].term {
                Terminator::Call {
                    operand: op,
                    args,
                    destination: dst,
                } => {
                    // Rename the return value.
                    //
                    // FIXME It seems that calls always have a destination despite the field being
                    // `Option`. If this is not always the case, we may want to add the `Local`
                    // offset (`var_len`) to this statement so we can assign the arguments to the
                    // correct `Local`s during trace compilation.
                    let ret_val = dst
                        .as_ref()
                        .map(|(ret_val, _)| rnm.rename_irplace(&ret_val, &body))
                        .unwrap();

                    if let Some(callee_sym) = op.symbol() {
                        // We know the symbol name of the callee at least.
                        // Rename all `Local`s within the arguments.
                        let newargs = rnm.rename_args(args, &body);
                        if let Some(callbody) = sir.body(callee_sym) {
                            // We have SIR for the callee, so it will appear inlined in the trace.

                            // If the function has been annotated with do_not_trace, turn it into a
                            // call.
                            if callbody.flags.contains(BodyFlags::DO_NOT_TRACE) {
                                dnt_func = Some(callee_sym.to_string());
                                dnt_count = 1;
                                term_stmts.push(Statement::Call(op.clone(), newargs, Some(ret_val)))
                            } else {
                                entered_call = true;
                                // Push the IRPlace that the corresponding Return terminator should
                                // assign the result of the call to.
                                return_irplaces.push(ret_val.clone());

                                // Inform VarRenamer about this function's offset, which is equal to the
                                // number of variables assigned in the outer body.
                                rnm.enter(callbody.local_decls.len());

                                // Copy args in.
                                live_locals.push(HashSet::new());
                                for (arg_idx, arg) in newargs.iter().enumerate() {
                                    let dst_local = rnm.rename_local(
                                        &Local(u32::try_from(arg_idx).unwrap() + 1),
                                        &body,
                                    );
                                    live_locals.last_mut().unwrap().insert(dst_local);
                                    let dst_ip = IRPlace::Val {
                                        local: dst_local,
                                        off: 0,
                                        ty: arg.ty(),
                                    };
                                    term_stmts.push(Statement::Store(dst_ip, arg.clone()));
                                }
                            }
                        } else {
                            // We have a symbol name but no SIR. Without SIR the callee can't
                            // appear inlined in the trace, so we should emit a native call to the
                            // symbol instead.
                            term_stmts.push(Statement::Call(op.clone(), newargs, Some(ret_val)))
                        }
                    } else {
                        todo!();
                    }
                }
                Terminator::Return => {
                    guard_blocks.pop();
                    live_locals.pop();

                    if body.flags.contains(BodyFlags::INTERP_STEP) {
                        debug_assert!(in_interp_step);
                        in_interp_step = false;
                        entered_call = false;
                        continue;
                    }
                    // After leaving an inlined function call we need to clean up any renaming
                    // mappings we have added manually, because we don't get `StorageDead`
                    // statements for call arguments. Which mappings we need to remove depends on
                    // the number of arguments the function call had, which we keep track of in
                    // `cur_call_args`.
                    let dst_ip = return_irplaces.pop().unwrap();
                    let src_ip = rnm.rename_irplace(
                        &IRPlace::Val {
                            local: sir::RETURN_LOCAL,
                            off: 0,
                            ty: dst_ip.ty(),
                        },
                        &body,
                    );
                    rnm.leave();

                    // Copy out the return value into the caller.
                    term_stmts.push(Statement::Store(dst_ip, src_ip));
                }
                _ => (),
            }

            for stmt in term_stmts {
                let op = TirOp::Statement(stmt);
                ops.push(op);
            }

            // Convert the block terminator to a guard if necessary.
            let guard = match body.blocks[user_bb_idx_usize].term {
                Terminator::Goto(_)
                | Terminator::Return
                | Terminator::Drop { .. }
                | Terminator::Call { .. }
                | Terminator::Unimplemented(_) => None,
                Terminator::Unreachable => panic!("Traced unreachable code"),
                Terminator::SwitchInt {
                    ref discr,
                    ref values,
                    ref target_bbs,
                    otherwise_bb,
                } => {
                    // Peek at the next block in the trace to see which outgoing edge was taken and
                    // infer which value we must guard upon. We are working on the assumption that
                    // a trace can't end on a SwitchInt. i.e. that another block follows.
                    let next_blk = itr.peek().unwrap().bb_idx;
                    let edge_idx = target_bbs.iter().position(|e| *e == next_blk);
                    match edge_idx {
                        Some(idx) => Some(Guard {
                            val: rnm.rename_irplace(discr, &body),
                            kind: GuardKind::Integer(values[idx]),
                            blocks: Vec::new(),
                            live_locals: Vec::new(),
                        }),
                        None => {
                            debug_assert!(next_blk == otherwise_bb);
                            Some(Guard {
                                val: rnm.rename_irplace(discr, &body),
                                kind: GuardKind::OtherInteger(values.to_vec()),
                                blocks: Vec::new(),
                                live_locals: Vec::new(),
                            })
                        }
                    }
                }
                Terminator::Assert {
                    ref cond,
                    ref expected,
                    ..
                } => Some(Guard {
                    val: rnm.rename_irplace(cond, &body),
                    kind: GuardKind::Boolean(*expected),
                    blocks: Vec::new(),
                    live_locals: Vec::new(),
                }),
                Terminator::TraceDebugCall { ref msg, .. } => {
                    // No guard, but we do add a debug statement.
                    ops.push(TirOp::Statement(Statement::Debug(msg.to_owned())));
                    None
                }
            };

            if let Some(mut g) = guard {
                for gb in &guard_blocks {
                    g.blocks.push(gb.clone());
                }
                for ll in &live_locals {
                    let mut v = Vec::new();
                    for local in ll {
                        let sirlocal = rnm.sir_map.get(local).unwrap();
                        v.push(LiveLocal {
                            tir: *local,
                            sir: *sirlocal,
                        });
                    }
                    g.live_locals.push(v);
                }
                let op = TirOp::Guard(g);
                ops.push(op);
            }
        }

        let local_decls = rnm.done();

        if loop_trace {
            ops.insert(0, TirOp::Statement(Statement::LoopStart));
            ops.push(TirOp::Statement(Statement::LoopEnd));
        }

        Ok(Self {
            ops,
            local_decls,
            addr_map,
            sir,
        })
    }

    /// Return the TIR operation at index `idx` in the trace.
    ///
    /// # Safety
    ///
    /// Undefined behaviour will result if the index is out of bounds.
    pub unsafe fn op(&self, idx: usize) -> &TirOp {
        debug_assert!(idx < self.ops.len(), "bogus trace index");
        &self.ops.get_unchecked(idx)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Return the length of the trace measure in operations.
    pub fn len(&self) -> usize {
        self.ops.len()
    }
}

struct VarRenamer {
    /// Stores the offset before entering an inlined call, so that the correct offset can be
    /// restored again after leaving that call.
    stack: Vec<u32>,
    /// Current offset used to rename variables.
    offset: u32,
    /// Accumulator keeping track of total number of variables used. Needed to use different
    /// offsets for consecutive inlined function calls.
    acc: Option<u32>,
    /// Maps a renamed local to its local declaration.
    local_decls: HashMap<Local, LocalDecl>,
    pub sir_map: HashMap<Local, Local>,
}

impl VarRenamer {
    fn new() -> Self {
        VarRenamer {
            stack: vec![0],
            offset: 0,
            acc: None,
            local_decls: HashMap::new(),
            sir_map: HashMap::new(),
        }
    }

    /// Finalises the renamer, returning the local decls.
    fn done(self) -> HashMap<Local, LocalDecl> {
        self.local_decls
    }

    fn init_acc(&mut self, num_locals: usize) {
        if self.acc.is_none() {
            self.acc.replace(num_locals as u32);
        }
    }

    fn enter(&mut self, num_locals: usize) {
        // When entering an inlined function call set the offset to the current accumulator. Then
        // increment the accumulator by the number of locals in the current function. Also add the
        // offset to the stack, so we can restore it once we leave the inlined function call again.
        self.offset = self.acc.unwrap();
        self.stack.push(self.offset);
        if let Some(v) = self.acc.as_mut() {
            *v += u32::try_from(num_locals).unwrap();
        }
    }

    fn leave(&mut self) {
        // When we leave an inlined function call, we pop the previous offset from the stack,
        // reverting the offset to what it was before the function was entered.
        self.stack.pop();
        debug_assert!(!self.stack.is_empty());
        self.offset = *self.stack.last().unwrap();
    }

    fn rename_irplace(&mut self, ip: &IRPlace, body: &ykpack::Body) -> IRPlace {
        match ip {
            IRPlace::Val { local, off, ty } => IRPlace::Val {
                local: self.rename_local(local, body),
                off: *off,
                ty: *ty,
            },
            IRPlace::Indirect { ptr, off, ty } => IRPlace::Indirect {
                ptr: Ptr {
                    local: self.rename_local(&ptr.local, body),
                    off: ptr.off,
                },
                off: *off,
                ty: *ty,
            },
            IRPlace::Const { .. } => ip.clone(),
            IRPlace::Unimplemented(..) => ip.clone(),
        }
    }

    fn rename_args(&mut self, args: &[IRPlace], body: &ykpack::Body) -> Vec<IRPlace> {
        args.iter()
            .map(|op| self.rename_irplace(&op, body))
            .collect()
    }

    fn rename_local(&mut self, local: &Local, body: &ykpack::Body) -> Local {
        let renamed = Local(local.0 + self.offset);
        self.local_decls.insert(
            renamed,
            body.local_decls[usize::try_from(local.0).unwrap()].clone(),
        );
        self.sir_map.insert(renamed, *local);
        renamed
    }
}

impl Display for TirTrace<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "local_decls:")?;
        let mut sort_decls = self
            .local_decls
            .iter()
            .collect::<Vec<(&Local, &LocalDecl)>>();
        sort_decls.sort_by(|l, r| l.0.partial_cmp(r.0).unwrap());
        for (l, dcl) in sort_decls {
            writeln!(f, "  {}: {} => {}", l, dcl.ty, self.sir.ty(&dcl.ty))?;
        }

        writeln!(f, "ops:")?;
        for op in &self.ops {
            writeln!(f, "  {}", op)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct GuardBlock {
    pub symbol_name: &'static str,
    pub bb_idx: ykpack::BasicBlockIndex,
}

impl Display for GuardBlock {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "<{}, {}>", self.symbol_name, self.bb_idx)
    }
}

/// A mapping from a TIR local to its equivalent in SIR.
#[derive(Debug)]
pub struct LiveLocal {
    pub tir: Local,
    pub sir: Local,
}

/// A guard states the assumptions from its position in a trace onward.
#[derive(Debug)]
pub struct Guard {
    /// The value to be checked if the guard is to pass.
    pub val: IRPlace,
    /// The requirement upon `val` for the guard to pass.
    pub kind: GuardKind,
    /// The blocks active at the point of a terminator. Because a trace inlines functions, a guard
    /// can fail at a point when, normally, we'd be `blocks.len()` stack frames deep in execution.
    /// In order to start the `StopgapInterpreter`, we need to recreate each of those stack frames.
    pub blocks: Vec<GuardBlock>,
    /// The TIR locals (and their SIR equivalent) that are live at the time of the guard. This is
    /// needed so that we can initialise the stopgap interpreter with the correct state.
    pub live_locals: Vec<Vec<LiveLocal>>,
}

impl fmt::Display for Guard {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut live = String::from("");
        for ll in &self.live_locals {
            write!(
                live,
                "[{}]",
                ll.iter()
                    .map(|l| l.tir.to_string())
                    .collect::<Vec<String>>()
                    .join(", ")
            )?;
        }
        write!(
            f,
            "guard({}, {}, {:?}, [{}])",
            self.val, self.kind, self.blocks, live
        )
    }
}

/// A guard states the assumptions from its position in a trace onward.
#[derive(Debug)]
pub enum GuardKind {
    /// The value must be equal to an integer constant.
    Integer(u128),
    /// The value must not be a member of the specified collection of integers. This is necessary
    /// due to the "otherwise" semantics of the `SwitchInt` terminator in SIR.
    OtherInteger(Vec<u128>),
    /// The value must equal a Boolean constant.
    Boolean(bool),
}

impl fmt::Display for GuardKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Integer(u128v) => write!(f, "integer({})", u128v),
            Self::OtherInteger(u128vs) => write!(f, "other_integer({:?})", u128vs),
            Self::Boolean(expect) => write!(f, "bool({})", expect),
        }
    }
}

/// A TIR operation. A collection of these makes a TIR trace.
#[derive(Debug)]
pub enum TirOp {
    Statement(Statement),
    Guard(Guard),
}

impl fmt::Display for TirOp {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            TirOp::Statement(st) => write!(f, "{}", st),
            TirOp::Guard(gd) => write!(f, "{}", gd),
        }
    }
}
