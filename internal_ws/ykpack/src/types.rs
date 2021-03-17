//! Types for the Yorick intermediate language.

use bitflags::bitflags;
use fxhash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::{
    convert::TryFrom,
    default::Default,
    fmt::{self, Display},
};

// FIXME these should probably all be tuple structs, as type aliases offer little type safety.
pub type SirOffset = usize;
pub type DefIndex = u32;
pub type BasicBlockIndex = u32;
pub type StatementIndex = usize;
pub type LocalIndex = u32;
pub type FieldIndex = u32;
pub type ArrayIndex = u32;
pub type OffT = i32;

/// Uniquely identifies a compilation unit.
#[derive(Debug, Hash, Eq, PartialEq, Copy, Clone, Serialize, Deserialize)]
#[repr(C)]
pub struct CguHash(pub u64);

impl Display for CguHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x}", self.0)
    }
}

/// The index of a SIR type.
#[derive(Debug, Hash, Eq, PartialEq, Copy, Clone, Serialize, Deserialize)]
#[repr(C)]
pub struct TyIndex(pub u32);

impl Display for TyIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x}", self.0)
    }
}

/// Uniquely identifies a SIR type.
#[derive(Debug, Hash, Eq, PartialEq, Copy, Clone, Serialize, Deserialize)]
#[repr(C)]
pub struct TypeId {
    /// Identifies which compilation unit the type comes from.
    pub cgu: CguHash,
    /// The type index within the compilation unit.
    pub idx: TyIndex,
}

impl Display for TypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}, {})", self.cgu, self.idx)
    }
}

/// The type of a local variable.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone, Hash)]
pub struct Ty {
    pub size: usize,
    pub align: usize,
    pub kind: TyKind,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone, Hash)]
pub enum TyKind {
    /// Signed integers.
    SignedInt(SignedIntTy),
    /// Unsigned integers.
    UnsignedInt(UnsignedIntTy),
    /// A structure type.
    Struct(StructTy),
    /// A tuple type.
    Tuple(TupleTy),
    /// An array type.
    /// FIXME size_align can be computed from elem_ty and len, but requires some refactoring.
    Array { elem_ty: TypeId, len: usize },
    /// A slice type.
    Slice(TypeId),
    /// A reference to something.
    Ref(TypeId),
    /// A Boolean.
    Bool,
    /// A char.
    Char,
    /// Anything that we've not yet defined a lowering for.
    Unimplemented(String),
}

impl Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            TyKind::SignedInt(si) => write!(f, "{}", si),
            TyKind::UnsignedInt(ui) => write!(f, "{}", ui),
            TyKind::Struct(sty) => write!(f, "{}", sty),
            TyKind::Tuple(tty) => write!(f, "{}", tty),
            TyKind::Array { elem_ty, len, .. } => write!(f, "[{}; {}]", elem_ty, len),
            TyKind::Slice(sty) => write!(f, "&[{:?}]", sty),
            TyKind::Ref(rty) => write!(f, "&{:?}", rty),
            TyKind::Bool => write!(f, "bool"),
            TyKind::Char => write!(f, "char"),
            TyKind::Unimplemented(m) => write!(f, "Unimplemented: {}", m),
        }
    }
}

impl Ty {
    pub fn size(&self) -> u64 {
        u64::try_from(self.size).unwrap()
    }

    pub fn align(&self) -> u64 {
        u64::try_from(self.align).unwrap()
    }

    pub fn is_signed_int(&self) -> bool {
        matches!(self.kind, TyKind::SignedInt(..))
    }

    pub fn is_int(&self) -> bool {
        matches!(self.kind, TyKind::SignedInt(..)) || matches!(self.kind, TyKind::UnsignedInt(..))
    }

    pub fn is_unit(&self) -> bool {
        if let TyKind::Tuple(tty) = &self.kind {
            tty.is_unit()
        } else {
            false
        }
    }

    /// Unwraps a tuple if `self` is `Self::Tuple` or panics otherwise.
    pub fn unwrap_tuple(&self) -> &TupleTy {
        if let TyKind::Tuple(tty) = &self.kind {
            &tty
        } else {
            panic!("tried to unwrap a non-tuple");
        }
    }
}

/// Describes the various signed integer types.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone, Hash)]
pub enum SignedIntTy {
    Isize,
    I8,
    I16,
    I32,
    I64,
    I128,
}

impl Display for SignedIntTy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Isize => "isize",
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::I128 => "i128",
        };
        write!(f, "{}", s)
    }
}

/// Describes the various unsigned integer types.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone, Hash)]
pub enum UnsignedIntTy {
    Usize,
    U8,
    U16,
    U32,
    U64,
    U128,
}

impl Display for UnsignedIntTy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Usize => "usize",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::U128 => "u128",
        };
        write!(f, "{}", s)
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone, Hash)]
pub struct Fields {
    /// Field offsets.
    pub offsets: Vec<OffT>,
    /// The type of each field.
    pub tys: Vec<TypeId>,
}

impl Display for Fields {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "offsets: [{}], tys: [{}]",
            self.offsets
                .iter()
                .map(|o| o.to_string())
                .collect::<Vec<String>>()
                .join(", "),
            self.tys
                .iter()
                .map(|t| format!("{}", t))
                .collect::<Vec<String>>()
                .join(", ")
        )
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone, Hash)]
pub struct TupleTy {
    /// The fields of the tuple.
    pub fields: Fields,
}

impl TupleTy {
    pub fn is_unit(&self) -> bool {
        self.fields.offsets.is_empty()
    }
}

impl Display for TupleTy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TupleTy {{ {} }}", self.fields)
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone, Hash)]
pub struct StructTy {
    /// The fields of the struct.
    pub fields: Fields,
}

impl Display for StructTy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StructTy {{ {} }}", self.fields)
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone, Copy, Hash, Ord, PartialOrd)]
#[repr(C)]
pub struct Local(pub LocalIndex);

impl Display for Local {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "${}", self.0)
    }
}

bitflags! {
    /// Bits in the `flags` bitfield in `Body`.
    #[derive(Serialize, Deserialize)]
    pub struct BodyFlags: u8 {
        /// This function is annotated #[do_not_trace].
        const DO_NOT_TRACE = 0b00000001;
        /// This function is annotated #[interp_step].
        const INTERP_STEP = 0b00000010;
        /// This function is yktrace::trace_debug.
        const TRACE_DEBUG = 0b00000100;
    }
}

/// The definition of a local variable, including its type.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
#[repr(C)]
pub struct LocalDecl {
    pub ty: TypeId,
    /// If true this local variable is at some point referenced, and thus should be allocated on
    /// the stack and never in a register.
    pub referenced: bool,
}

impl LocalDecl {
    pub fn new(ty: TypeId, referenced: bool) -> Self {
        Self { ty, referenced }
    }
}

impl Display for LocalDecl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.ty)
    }
}

/// A tracing IR pack.
/// Each Body maps to exactly one MIR Body.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub struct Body {
    pub symbol_name: String,
    pub blocks: Vec<BasicBlock>,
    pub flags: BodyFlags,
    pub local_decls: Vec<LocalDecl>,
    pub num_args: usize,
    pub layout: (usize, usize),
    pub offsets: Vec<usize>,
}

impl Display for Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "symbol: {}", self.symbol_name)?;
        writeln!(f, "  flags: {:?}", self.flags)?;
        writeln!(f, "  num_args: {}", self.num_args)?;

        writeln!(f, "  local_decls:")?;
        for (di, d) in self.local_decls.iter().enumerate() {
            writeln!(f, "    {}: {}", di, d)?;
        }

        let mut block_strs = Vec::new();
        for (i, b) in self.blocks.iter().enumerate() {
            block_strs.push(format!("    bb{}:\n{}", i, b));
        }

        writeln!(f, "  blocks:")?;
        writeln!(f, "{}", block_strs.join("\n"))?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub struct BasicBlock {
    pub stmts: Vec<Statement>,
    pub term: Terminator,
}

impl BasicBlock {
    pub fn new(stmts: Vec<Statement>, term: Terminator) -> Self {
        Self { stmts, term }
    }
}

impl Display for BasicBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for s in self.stmts.iter() {
            writeln!(f, "        {}", s)?;
        }
        write!(f, "        {}", self.term)
    }
}

/// Represents a pointer to be dereferenced at runtime.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub struct Ptr {
    pub local: Local,
    pub off: OffT,
}

impl Display for Ptr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}+{}", self.local, self.off)
    }
}

/// An IR place. This is used in SIR and TIR to describe the (abstract) address of a piece of data.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub enum IRPlace {
    /// The IRPlace describes a value as a Local+offset pair.
    Val { local: Local, off: OffT, ty: TypeId },
    /// An indirect place, i.e. an IRPlace whose value is a pointer to another IRPlace. ykrustc uses
    /// these for deref and (dynamic) index projections (which cannot be resolved statically and
    /// thus depend on a runtime pointer).
    Indirect {
        /// The location of the pointer to be dereferenced at runtime.
        ptr: Ptr,
        /// The offset to apply to the above pointer.
        off: OffT,
        /// The type of the resulting place.
        ty: TypeId,
    },
    /// The IRPlace describes a constant.
    Const { val: Constant, ty: TypeId },
    /// A construct which we have no lowering for yet.
    Unimplemented(String),
}

impl Display for IRPlace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Val {
                local,
                off,
                ty: _ty,
            } => {
                if *off != 0 {
                    write!(f, "{}+{}", local, off)
                } else {
                    write!(f, "{}", local)
                }
            }
            Self::Indirect { ptr, off, ty: _ty } => {
                if *off != 0 {
                    write!(f, "*({})+{}", ptr, off)
                } else {
                    write!(f, "*{}", ptr)
                }
            }
            Self::Const { val, ty: _ty } => write!(f, "{}", val),
            Self::Unimplemented(c) => write!(f, "{}", c),
        }
    }
}

impl IRPlace {
    /// Returns the local used (if any) in the place.
    pub fn local(&self) -> Option<Local> {
        match self {
            Self::Val { local, .. }
            | Self::Indirect {
                ptr: Ptr { local, .. },
                ..
            } => Some(*local),
            Self::Const { .. } | Self::Unimplemented(_) => None,
        }
    }

    /// Returns the type of the place.
    pub fn ty(&self) -> TypeId {
        match self {
            Self::Val { ty, .. } | Self::Indirect { ty, .. } | Self::Const { ty, .. } => *ty,
            Self::Unimplemented(_) => unreachable!(),
        }
    }

    /// Converts a direct place into an indirect one, forcing a dereference when read from or
    /// stored to.
    pub fn to_indirect(&self, new_ty: TypeId) -> IRPlace {
        match self {
            Self::Val { local, off, ty: _ } => {
                let ptr = Ptr {
                    local: *local,
                    off: *off,
                };
                IRPlace::Indirect {
                    ptr,
                    off: 0,
                    ty: new_ty,
                }
            }
            Self::Const { .. } => unreachable!(),
            Self::Indirect { .. } => unreachable!(),
            Self::Unimplemented(_) => self.clone(),
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub enum Statement {
    /// Do nothing.
    Nop,
    /// Stores the content addressed by the right hand side into the left hand side.
    Store(IRPlace, IRPlace),
    /// Binary operations. FIXME dst should be a local?
    BinaryOp {
        dst: IRPlace,
        op: BinOp,
        opnd1: IRPlace,
        opnd2: IRPlace,
        checked: bool,
    },
    /// Makes a reference to another IRPlace.
    MkRef(IRPlace, IRPlace),
    /// Computes a pointer address at runtime.
    DynOffs {
        /// Where to store the result.
        dst: IRPlace,
        /// The base address. `idx` * `scale` are added to this at runtime to give the result.
        base: IRPlace,
        /// The index to multiply with `scale`.
        idx: IRPlace,
        /// The scaling factor for `idx`.
        scale: u32,
    },
    /// Marks a local variable live.
    StorageLive(Local),
    /// Marks a local variable dead.
    StorageDead(Local),
    /// A (non-inlined) call from a TIR trace to a binary symbol using the system ABI. This does
    /// not appear in SIR. Not to be confused with Terminator::Call in SIR.
    Call(CallOperand, Vec<IRPlace>, Option<IRPlace>),
    /// Cast a value into another. Since the cast type and the destination type are the same, we
    /// only need the latter.
    Cast(IRPlace, IRPlace),
    /// Marks the spot where we want the trace to loop.
    LoopStart,
    /// Jumps to the position marked by `LoopStart`.
    LoopEnd,
    /// A debug marker. This does not appear in SIR.
    Debug(String),
    /// Any unimplemented lowering maps to this variant.
    /// The string inside is the stringified MIR statement.
    Unimplemented(String),
}

impl Display for Statement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Statement::Nop => write!(f, "nop"),
            Statement::MkRef(l, r) => write!(f, "{} = &({})", l, r),
            Statement::DynOffs {
                dst,
                base,
                idx,
                scale,
                ..
            } => write!(f, "{} = dynoffs({}, {}, {})", dst, base, idx, scale),
            Statement::Store(l, r) => write!(f, "{} = {}", l, r),
            Statement::BinaryOp {
                dst,
                op,
                opnd1,
                opnd2,
                checked,
            } => {
                let c = if *checked { " (checked)" } else { "" };
                write!(f, "{} = {} {} {}{}", dst, opnd1, op, opnd2, c)
            }
            Statement::StorageLive(local) => write!(f, "live({})", local),
            Statement::StorageDead(local) => write!(f, "dead({})", local),
            Statement::Call(op, args, dst) => {
                let args_s = args
                    .iter()
                    .map(|a| format!("{}", a))
                    .collect::<Vec<String>>()
                    .join(", ");
                let dst_s = if let Some(dst) = dst {
                    format!("{}", dst)
                } else {
                    String::from("none")
                };
                write!(f, "{} = call({}, [{}])", dst_s, op, args_s)
            }
            Statement::Cast(d, s) => write!(f, "Cast({}, {})", d, s),
            Statement::LoopStart => write!(f, "LoopStart"),
            Statement::LoopEnd => write!(f, "LoopEnd"),
            Statement::Debug(s) => write!(f, "// {}", s),
            Statement::Unimplemented(mir_stmt) => write!(f, "unimplemented_stmt: {}", mir_stmt),
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub enum Constant {
    Int(ConstantInt),
    Bool(bool),
    Tuple(TypeId), // FIXME assumed to be unit for now. Needs a value in here.
    Unimplemented(String),
}

impl Constant {
    pub fn i64_cast(&self) -> i64 {
        match self {
            Self::Int(ci) => ci.i64_cast(),
            Self::Bool(b) => *b as i64,
            Self::Tuple(..) => unreachable!(),
            Self::Unimplemented(_) => unreachable!(),
        }
    }
}

impl Display for Constant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Constant::Int(i) => write!(f, "{}", i),
            Constant::Bool(b) => write!(f, "{}", b),
            Constant::Tuple(..) => write!(f, "()"), // FIXME assumed unit.
            Constant::Unimplemented(s) => write!(f, "unimplemented constant: {:?}", s),
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub enum ConstantInt {
    UnsignedInt(UnsignedInt),
    SignedInt(SignedInt),
}

impl From<bool> for ConstantInt {
    fn from(b: bool) -> Self {
        if b {
            ConstantInt::UnsignedInt(UnsignedInt::Usize(1))
        } else {
            ConstantInt::UnsignedInt(UnsignedInt::Usize(0))
        }
    }
}

impl ConstantInt {
    /// Returns an i64 value suitable for loading into a register.
    /// If the constant is signed, then it will be sign-extended.
    pub fn i64_cast(&self) -> i64 {
        match self {
            ConstantInt::UnsignedInt(ui) => match ui {
                UnsignedInt::U8(i) => *i as i64,
                UnsignedInt::U16(i) => *i as i64,
                UnsignedInt::U32(i) => *i as i64,
                UnsignedInt::U64(i) => *i as i64,
                #[cfg(target_pointer_width = "64")]
                UnsignedInt::Usize(i) => *i as i64,
                UnsignedInt::U128(_) => panic!("i64_cast: u128 to isize"),
            },
            ConstantInt::SignedInt(si) => match si {
                SignedInt::I8(i) => *i as i64,
                SignedInt::I16(i) => *i as i64,
                SignedInt::I32(i) => *i as i64,
                SignedInt::I64(i) => *i as i64,
                #[cfg(target_pointer_width = "64")]
                SignedInt::Isize(i) => *i as i64,
                SignedInt::I128(_) => panic!("i64_cast: i128 to isize"),
            },
        }
    }
}

/// Generate a method that constructs a ConstantInt variant from bits in u128 form.
macro_rules! const_int_from_bits {
    ($fn_name: ident, $rs_t: ident, $yk_t: ident, $yk_variant: ident) => {
        pub fn $fn_name(bits: u128) -> Self {
            ConstantInt::$yk_t($yk_t::$yk_variant(bits as $rs_t))
        }
    };
}

impl ConstantInt {
    const_int_from_bits!(u8_from_bits, u8, UnsignedInt, U8);
    const_int_from_bits!(u16_from_bits, u16, UnsignedInt, U16);
    const_int_from_bits!(u32_from_bits, u32, UnsignedInt, U32);
    const_int_from_bits!(u64_from_bits, u64, UnsignedInt, U64);
    const_int_from_bits!(u128_from_bits, u128, UnsignedInt, U128);
    const_int_from_bits!(usize_from_bits, usize, UnsignedInt, Usize);

    const_int_from_bits!(i8_from_bits, i8, SignedInt, I8);
    const_int_from_bits!(i16_from_bits, i16, SignedInt, I16);
    const_int_from_bits!(i32_from_bits, i32, SignedInt, I32);
    const_int_from_bits!(i64_from_bits, i64, SignedInt, I64);
    const_int_from_bits!(i128_from_bits, i128, SignedInt, I128);
    const_int_from_bits!(isize_from_bits, isize, SignedInt, Isize);
}

impl Display for ConstantInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConstantInt::UnsignedInt(u) => write!(f, "{}", u),
            ConstantInt::SignedInt(s) => write!(f, "{}", s),
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub enum UnsignedInt {
    Usize(usize),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    U128(u128),
}

impl Display for UnsignedInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usize(v) => write!(f, "{}usize", v),
            Self::U8(v) => write!(f, "{}u8", v),
            Self::U16(v) => write!(f, "{}u16", v),
            Self::U32(v) => write!(f, "{}u32", v),
            Self::U64(v) => write!(f, "{}u64", v),
            Self::U128(v) => write!(f, "{}u128", v),
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub enum SignedInt {
    Isize(isize),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    I128(i128),
}

impl Display for SignedInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Isize(v) => write!(f, "{}isize", v),
            Self::I8(v) => write!(f, "{}i8", v),
            Self::I16(v) => write!(f, "{}i16", v),
            Self::I32(v) => write!(f, "{}i32", v),
            Self::I64(v) => write!(f, "{}i64", v),
            Self::I128(v) => write!(f, "{}i128", v),
        }
    }
}

/// A call target.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub enum CallOperand {
    /// A call to a binary symbol by name.
    Fn(String),
    /// An unknown or unhandled callable.
    Unknown, // FIXME -- Find out what else. Closures jump to mind.
}

impl CallOperand {
    pub fn symbol(&self) -> Option<&str> {
        if let Self::Fn(sym) = self {
            Some(sym)
        } else {
            None
        }
    }
}

impl Display for CallOperand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallOperand::Fn(sym_name) => write!(f, "{}", sym_name),
            CallOperand::Unknown => write!(f, "<unknown>"),
        }
    }
}

/// A basic block terminator.
/// Note that we assume an the abort strategy, so there are no unwind or cleanup edges present.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub enum Terminator {
    Goto(BasicBlockIndex),
    SwitchInt {
        discr: IRPlace,
        values: Vec<u128>,
        target_bbs: Vec<BasicBlockIndex>,
        otherwise_bb: BasicBlockIndex,
    },
    Return,
    Unreachable,
    Drop {
        location: IRPlace,
        target_bb: BasicBlockIndex,
    },
    Call {
        operand: CallOperand,
        args: Vec<IRPlace>,
        /// The return value and basic block to continue at, if the call converges.
        destination: Option<(IRPlace, BasicBlockIndex)>,
    },
    /// A call to yktrace::trace_debug. This is converted into a Statement::Debug at TIR
    /// compilation time.
    TraceDebugCall {
        msg: String,
        destination: BasicBlockIndex,
    },
    /// The value in `cond` must equal to `expected` to advance to `target_bb`.
    Assert {
        cond: IRPlace,
        expected: bool,
        target_bb: BasicBlockIndex,
    },
    Unimplemented(String), // FIXME will eventually disappear.
}

impl Display for Terminator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Terminator::Goto(bb) => write!(f, "goto bb{}", bb),
            Terminator::SwitchInt {
                discr,
                values,
                target_bbs,
                otherwise_bb,
            } => write!(
                f,
                "switch_int {}, [{}], [{}], {}",
                discr,
                values
                    .iter()
                    .map(|b| format!("{}", b))
                    .collect::<Vec<String>>()
                    .join(", "),
                target_bbs
                    .iter()
                    .map(|b| format!("{}", b))
                    .collect::<Vec<String>>()
                    .join(", "),
                otherwise_bb
            ),
            Terminator::Return => write!(f, "return"),
            Terminator::Unreachable => write!(f, "unreachable"),
            Terminator::Drop {
                location,
                target_bb,
            } => write!(f, "drop {}, bb{}", target_bb, location,),
            Terminator::Call {
                operand,
                args,
                destination,
            } => {
                let ret_bb = if let Some((ret_val, bb)) = destination {
                    write!(f, "{} = ", ret_val)?;
                    format!(" -> bb{}", bb)
                } else {
                    String::from("")
                };
                let args_str = args
                    .iter()
                    .map(|a| format!("{}", a))
                    .collect::<Vec<String>>()
                    .join(", ");
                write!(f, "call {}({}){}", operand, args_str, ret_bb)
            }
            Terminator::TraceDebugCall { msg, destination } => {
                write!(f, "// {} ->  {}", msg, destination)
            }
            Terminator::Assert {
                cond,
                target_bb,
                expected,
            } => write!(f, "assert {}, {}, bb{}", cond, target_bb, expected),
            Terminator::Unimplemented(s) => write!(f, "unimplemented: {}", s),
        }
    }
}

/// Binary operations.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone, Copy)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitXor,
    BitAnd,
    BitOr,
    Shl,
    Shr,
    Eq,
    Lt,
    Le,
    Ne,
    Ge,
    Gt,
    Offset,
}

impl Display for BinOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::BitXor => "^",
            BinOp::BitAnd => "&",
            BinOp::BitOr => "|",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
            BinOp::Eq => "==",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Ne => "!=",
            BinOp::Ge => ">=",
            BinOp::Gt => ">",
            BinOp::Offset => "off",
        };
        write!(f, "{}", s)
    }
}

/// This serves as a table of contents for the section, and is required to allow lazy loading of
/// only selected parts of SIR (rather than loading the whole lot in, which is very slow).
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub struct SirHeader {
    /// Codegen unit hash.
    pub cgu_hash: CguHash,
    /// Maps type indices to their offsets. The offsets are relative to the end of the end of the
    /// SIR header.
    pub types: Vec<SirOffset>,
    /// Maps a symbol name to the offset of the corresponding SIR body. The offsets are relative to
    /// the end of the SIR header.
    pub bodies: FxHashMap<String, SirOffset>,
}

impl SirHeader {
    pub fn new(cgu_hash: CguHash) -> Self {
        Self {
            cgu_hash,
            types: Default::default(),
            bodies: Default::default(),
        }
    }
}

/// The top-level pack type.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub enum Pack {
    Header(SirHeader),
    Body(Body),
    Type(Ty),
}

impl Display for Pack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Pack::Header(_) => write!(f, "<sir-header>"),
            Pack::Body(sir) => write!(f, "{}", sir),
            Pack::Type(t) => write!(f, "{}", t),
        }
    }
}

/// A SIR mapping label.
#[derive(Debug, Hash, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct SirLabel {
    pub off: SirOffset,
    pub symbol_name: String,
    pub bb: BasicBlockIndex,
}

#[cfg(test)]
mod tests {
    use super::{ConstantInt, SignedInt, UnsignedInt};

    #[test]
    fn const_u8_from_bits() {
        let v = 233;
        let cst = ConstantInt::u8_from_bits(v as u128);
        assert_eq!(cst, ConstantInt::UnsignedInt(UnsignedInt::U8(v)));
    }

    #[test]
    fn const_i32_from_bits() {
        let v = -42i32;
        let cst = ConstantInt::i32_from_bits(v as u128);
        assert_eq!(cst, ConstantInt::SignedInt(SignedInt::I32(v)));
    }

    #[test]
    fn const_u64_from_bits() {
        let v = std::u64::MAX;
        let cst = ConstantInt::u64_from_bits(v as u128);
        assert_eq!(cst, ConstantInt::UnsignedInt(UnsignedInt::U64(v)));
    }

    #[test]
    fn const_i128_from_bits() {
        let v = -100001i128;
        let cst = ConstantInt::i128_from_bits(v as u128);
        assert_eq!(cst, ConstantInt::SignedInt(SignedInt::I128(v)));
    }

    #[test]
    fn const_u128_from_bits() {
        let v = u128::MAX;
        let cst = ConstantInt::u128_from_bits(v as u128);
        assert_eq!(cst, ConstantInt::UnsignedInt(UnsignedInt::U128(v)));
    }
}
