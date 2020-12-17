//! Conceptually this module takes an ordered collection of SIR block locations and converts it
//! into a Tracing IR (TIR) Trace using the SIR found in the `.yk_sir` section of the currently
//! running executable.

use super::SirTrace;
use crate::{
    errors::InvalidTraceError,
    sir::{self, Sir},
    INTERP_STEP_ARG
};
use std::{
    collections::{HashMap, HashSet},
    convert::TryFrom,
    fmt::{self, Display, Write}
};
pub use ykpack::{
    BinOp, BodyFlags, CallOperand, Constant, ConstantInt, IPlace, Local, LocalDecl, LocalIndex,
    Ptr, SignedInt, Statement, Terminator, UnsignedInt
};

/// A TIR trace is conceptually a straight-line path through the SIR with guarded speculation.
#[derive(Debug)]
pub struct TirTrace<'a, 'm> {
    ops: Vec<TirOp>,
    /// Maps each local variable to its declaration, including type.
    pub local_decls: HashMap<Local, LocalDecl>,
    pub addr_map: HashMap<String, u64>,
    sir: &'a Sir<'m>
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
        let mut ignore: Option<String> = None;
        // Maps symbol names to their virtual addresses.
        let mut addr_map: HashMap<String, u64> = HashMap::new();

        // A stack to keep track of where to store return values of inlined calls. When we
        // encounter `$x = Call(...)` we push `$x` to the stack so that later, when we encounter
        // the corresponding Return, we can find the correct place to store the return value (by
        // popping from the stack).
        let mut return_iplaces: Vec<IPlace> = Vec::new();

        // As we compile, we are going to check the define-use (DU) chain of our local
        // variables. No local should be used without first being defined. If that happens it's
        // likely that the user used a variable from outside the scope of the trace without
        // introducing it via `trace_locals()`.
        let mut defined_locals: HashSet<Local> = HashSet::new();
        let mut def_sites: HashMap<Local, usize> = HashMap::new();
        let mut last_use_sites = HashMap::new();

        // Ensure the argument to the `interp_step` function is defined by the first statement.
        // The arg is always at local index 1.
        defined_locals.insert(INTERP_STEP_ARG);
        def_sites.insert(INTERP_STEP_ARG, 0);

        let mut update_defined_locals = |op: &TirOp, op_idx: usize| {
            // Locals reported by `maybe_defined_locals()` are only defined if they are not already
            // defined.
            let newly_defined = op
                .maybe_defined_locals()
                .iter()
                .filter_map(|l| {
                    if !defined_locals.contains(l) {
                        Some(*l)
                    } else {
                        None
                    }
                })
                .collect::<Vec<Local>>();
            defined_locals.extend(&newly_defined);
            for d in newly_defined {
                def_sites.insert(d, op_idx);
            }

            for lcl in op.used_locals() {
                // The trace inputs local is regarded as being live for the whole trace.
                if lcl == INTERP_STEP_ARG {
                    continue;
                }
                if !defined_locals.contains(&lcl) {
                    panic!("undefined local: {} in {}", lcl, op);
                }
                last_use_sites.insert(lcl, op_idx);
            }
        };

        let mut in_interp_step = false;
        while let Some(loc) = itr.next() {
            let body = match sir.body(&loc.symbol_name) {
                Some(b) => b,
                None => {
                    return Err(InvalidTraceError::no_sir(&loc.symbol_name));
                }
            };

            // Ignore yktrace::trace_debug.
            // We don't use the 'ignore' machinery below, as that would require the TraceDebugCall
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

            // If a function was annotated with `do_not_trace`, skip all instructions within it as
            // well. FIXME: recursion.
            if let Some(sym) = &ignore {
                if sym == &loc.symbol_name
                    && body.blocks[user_bb_idx_usize].term == Terminator::Return
                {
                    ignore = None;
                }
                continue;
            }

            // If we are not in the `interp_step` function, then ignore statements.
            if in_interp_step {
                // When converting the SIR trace into a TIR trace we alpha-rename the `Local`s from
                // inlined functions by adding an offset to each. This offset is derived from the
                // number of assigned variables in the functions outer context. For example, if a
                // function `bar` is inlined into a function `foo`, and `foo` used 5 variables, then
                // all variables in `bar` are offset by 5.
                for stmt in body.blocks[user_bb_idx_usize].stmts.iter() {
                    let op = match stmt {
                        Statement::MkRef(dest, src) => Statement::MkRef(
                            rnm.rename_iplace(dest, &body),
                            rnm.rename_iplace(src, &body)
                        ),
                        Statement::DynOffs {
                            dest,
                            base,
                            idx,
                            scale
                        } => Statement::DynOffs {
                            dest: rnm.rename_iplace(dest, &body),
                            base: rnm.rename_iplace(base, &body),
                            idx: rnm.rename_iplace(idx, &body),
                            scale: *scale
                        },
                        Statement::Store(dest, src) => Statement::Store(
                            rnm.rename_iplace(dest, &body),
                            rnm.rename_iplace(src, &body)
                        ),
                        Statement::BinaryOp {
                            dest,
                            op,
                            opnd1,
                            opnd2,
                            checked
                        } => Statement::BinaryOp {
                            dest: rnm.rename_iplace(dest, &body),
                            op: *op,
                            opnd1: rnm.rename_iplace(opnd1, &body),
                            opnd2: rnm.rename_iplace(opnd2, &body),
                            checked: *checked
                        },
                        Statement::Nop => stmt.clone(),
                        Statement::Unimplemented(_) | Statement::Debug(_) => stmt.clone(),
                        Statement::Cast(dest, src) => Statement::Cast(
                            rnm.rename_iplace(dest, &body),
                            rnm.rename_iplace(src, &body)
                        ),
                        // The following statements are specific to TIR and cannot appear in SIR.
                        Statement::Call(..) | Statement::StorageDead(_) => unreachable!()
                    };

                    // In TIR, stores to local number zero are always to the return value of the
                    // #[interp_step] function. We know this is unit so we can ignore it.
                    if let Statement::Store(
                        IPlace::Val {
                            local: sir::RETURN_LOCAL,
                            ..
                        },
                        _
                    ) = op
                    {
                        debug_assert!(sir.ty(&rnm.local_decls[&sir::RETURN_LOCAL].ty).is_unit());
                        continue;
                    }

                    let op = TirOp::Statement(op);

                    update_defined_locals(&op, ops.len());
                    ops.push(op);
                }
            }

            if let Terminator::Call {
                operand: op,
                args: _,
                destination: _
            } = &body.blocks[user_bb_idx_usize].term
            {
                if let Some(callee_sym) = op.symbol() {
                    if let Some(callee_body) = sir.body(callee_sym) {
                        if callee_body.flags.contains(BodyFlags::INTERP_STEP) {
                            if in_interp_step {
                                panic!("recursion into interp_step detected");
                            }
                            in_interp_step = true;
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
                    destination: dest
                } => {
                    // Rename the return value.
                    //
                    // FIXME It seems that calls always have a destination despite the field being
                    // `Option`. If this is not always the case, we may want add the `Local` offset
                    // (`var_len`) to this statement so we can assign the arguments to the correct
                    // `Local`s during trace compilation.
                    let ret_val = dest
                        .as_ref()
                        .map(|(ret_val, _)| rnm.rename_iplace(&ret_val, &body))
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
                                ignore = Some(callee_sym.to_string());
                                term_stmts.push(Statement::Call(op.clone(), newargs, Some(ret_val)))
                            } else {
                                // Push the IPlace that the corresponding Return terminator should
                                // assign the result of the call to.
                                return_iplaces.push(ret_val.clone());

                                // Inform VarRenamer about this function's offset, which is equal to the
                                // number of variables assigned in the outer body.
                                rnm.enter(callbody.local_decls.len());

                                // Copy args in.
                                for (arg_idx, arg) in newargs.iter().enumerate() {
                                    let dest_local = rnm.rename_local(
                                        &Local(u32::try_from(arg_idx).unwrap() + 1),
                                        &body
                                    );
                                    let dest_ip = IPlace::Val {
                                        local: dest_local,
                                        off: 0,
                                        ty: arg.ty()
                                    };
                                    term_stmts.push(Statement::Store(dest_ip, arg.clone()));
                                }
                            }
                        } else {
                            // We have a symbol name but no SIR. Without SIR the callee can't
                            // appear inlined in the trace, so we should emit a native call to the
                            // symbol instead.
                            term_stmts.push(Statement::Call(op.clone(), newargs, Some(ret_val)))
                        }
                    } else {
                        todo!("Unknown callee encountered");
                    }
                }
                Terminator::Return => {
                    if body.flags.contains(BodyFlags::INTERP_STEP) {
                        debug_assert!(in_interp_step);
                        in_interp_step = false;
                        continue;
                    }
                    // After leaving an inlined function call we need to clean up any renaming
                    // mappings we have added manually, because we don't get `StorageDead`
                    // statements for call arguments. Which mappings we need to remove depends on
                    // the number of arguments the function call had, which we keep track of in
                    // `cur_call_args`.
                    let dest_ip = return_iplaces.pop().unwrap();
                    let src_ip = rnm.rename_iplace(
                        &IPlace::Val {
                            local: sir::RETURN_LOCAL,
                            off: 0,
                            ty: dest_ip.ty()
                        },
                        &body
                    );
                    rnm.leave();

                    // Copy out the return value into the caller.
                    term_stmts.push(Statement::Store(dest_ip, src_ip));
                }
                _ => ()
            }

            for stmt in term_stmts {
                let op = TirOp::Statement(stmt);
                update_defined_locals(&op, ops.len());
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
                    otherwise_bb
                } => {
                    // Peek at the next block in the trace to see which outgoing edge was taken and
                    // infer which value we must guard upon. We are working on the assumption that
                    // a trace can't end on a SwitchInt. i.e. that another block follows.
                    let next_blk = itr.peek().expect("no block to peek at").bb_idx;
                    let edge_idx = target_bbs.iter().position(|e| *e == next_blk);
                    match edge_idx {
                        Some(idx) => Some(Guard {
                            val: rnm.rename_iplace(discr, &body),
                            kind: GuardKind::Integer(values[idx].val()),
                            block: GuardBlock {
                                symbol_name: loc.symbol_name,
                                bb_idx: loc.bb_idx
                            },
                            live_locals: Vec::new()
                        }),
                        None => {
                            debug_assert!(next_blk == otherwise_bb);
                            Some(Guard {
                                val: rnm.rename_iplace(discr, &body),
                                kind: GuardKind::OtherInteger(
                                    values.iter().map(|v| v.val()).collect()
                                ),
                                block: GuardBlock {
                                    symbol_name: loc.symbol_name,
                                    bb_idx: loc.bb_idx
                                },
                                live_locals: Vec::new()
                            })
                        }
                    }
                }
                Terminator::Assert {
                    ref cond,
                    ref expected,
                    ..
                } => Some(Guard {
                    val: cond.clone(),
                    kind: GuardKind::Boolean(*expected),
                    block: GuardBlock {
                        symbol_name: loc.symbol_name,
                        bb_idx: loc.bb_idx
                    },
                    live_locals: Vec::new()
                }),
                Terminator::TraceDebugCall { ref msg, .. } => {
                    // No guard, but we do add a debug statement.
                    ops.push(TirOp::Statement(Statement::Debug(msg.to_owned())));
                    None
                }
            };

            if let Some(g) = guard {
                let op = TirOp::Guard(g);
                update_defined_locals(&op, ops.len());
                ops.push(op);
            }
        }

        let mut local_decls = rnm.done();

        // Insert `StorageDead` statements after the last use of each local variable. We process
        // the locals in reverse order of death site, so that inserting a statement cannot skew
        // the indices for subsequent insertions.
        let mut deads = last_use_sites.iter().collect::<Vec<(&Local, &usize)>>();
        deads.sort_by(|a, b| b.1.cmp(a.1));
        for (local, idx) in deads {
            if def_sites[local] == *idx && !ops[*idx].may_have_side_effects() {
                // If a defined local is never used, and the statement that defines it isn't
                // side-effecting, then we can remove the statement and local's decl entirely.
                //
                // FIXME This is not perfect. Consider `x.0 = 0; x.1 = 1` and then x is not
                // used after. The first operation will be seen to define `x`, the second will
                // be seen as a use of `x`, and thus neither of these statements will be
                // removed.
                ops.remove(*idx);
                let prev = local_decls.remove(&local);
                debug_assert!(prev.is_some());
            } else {
                let ds = TirOp::Statement(ykpack::Statement::StorageDead(*local));
                if *idx == ops.len() {
                    ops.push(ds);
                } else {
                    ops.insert(*idx + 1, ds);
                }

                // Scan the live range of `local`, adding the variable to the live list of any
                // guards we find.
                for op_idx in def_sites[local]..*idx {
                    if let TirOp::Guard(g) = &mut ops[op_idx] {
                        g.live_locals.push(*local);
                    }
                }
            }
        }

        Ok(Self {
            ops,
            local_decls,
            addr_map,
            sir
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
    local_decls: HashMap<Local, LocalDecl>
}

impl VarRenamer {
    fn new() -> Self {
        VarRenamer {
            stack: vec![0],
            offset: 0,
            acc: None,
            local_decls: HashMap::new()
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
            *v += num_locals as u32;
        }
    }

    fn leave(&mut self) {
        // When we leave an inlined function call, we pop the previous offset from the stack,
        // reverting the offset to what it was before the function was entered.
        self.stack.pop();
        if let Some(v) = self.stack.last() {
            self.offset = *v;
        } else {
            panic!("Unbalanced enter/leave statements!")
        }
    }

    fn rename_iplace(&mut self, ip: &IPlace, body: &ykpack::Body) -> IPlace {
        match ip {
            IPlace::Val { local, off, ty } => IPlace::Val {
                local: self.rename_local(local, body),
                off: *off,
                ty: *ty
            },
            IPlace::Indirect { ptr, off, ty } => IPlace::Indirect {
                ptr: Ptr {
                    local: self.rename_local(&ptr.local, body),
                    off: ptr.off
                },
                off: *off,
                ty: *ty
            },
            IPlace::Const { .. } => ip.clone(),
            IPlace::Unimplemented(..) => ip.clone()
        }
    }

    fn rename_args(&mut self, args: &[IPlace], body: &ykpack::Body) -> Vec<IPlace> {
        args.iter()
            .map(|op| self.rename_iplace(&op, body))
            .collect()
    }

    fn rename_local(&mut self, local: &Local, body: &ykpack::Body) -> Local {
        let renamed = Local(local.0 + self.offset);
        self.local_decls.insert(
            renamed,
            body.local_decls[usize::try_from(local.0).unwrap()].clone()
        );
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
            writeln!(
                f,
                "  {}: ({}, {}) => {}",
                l,
                dcl.ty.0,
                dcl.ty.1,
                self.sir.ty(&dcl.ty)
            )?;
        }

        writeln!(f, "ops:")?;
        for op in &self.ops {
            writeln!(f, "  {}", op)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct GuardBlock {
    pub symbol_name: &'static str,
    pub bb_idx: ykpack::BasicBlockIndex
}

impl Display for GuardBlock {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "<{}, {}>", self.symbol_name, self.bb_idx)
    }
}

/// A guard states the assumptions from its position in a trace onward.
#[derive(Debug)]
pub struct Guard {
    /// The value to be checked if the guard is to pass.
    pub val: IPlace,
    /// The requirement upon `val` for the guard to pass.
    pub kind: GuardKind,
    /// The block whose terminator was the basis for this guard. This is here so that, in the event
    /// that the guard fails, we know where to start the blackhole interpreter.
    pub block: GuardBlock,
    /// The TIR locals that are live at the time of the guard (in addition to the trace I/O
    /// variable, $1, which is assumed to be live throughout the entirety of the trace). This is
    /// needed so that we can initialise the blackhole interpreter with the correct state.
    pub live_locals: Vec<Local>
}

impl Guard {
    fn maybe_defined_locals(&self) -> Vec<Local> {
        Vec::new()
    }

    fn used_locals(&self) -> Vec<Local> {
        let mut ret = Vec::new();
        match &self.val {
            IPlace::Val { local, .. } => ret.push(*local),
            IPlace::Indirect { ptr, .. } => ret.push(ptr.local),
            _ => {}
        }
        ret
    }
}

impl fmt::Display for Guard {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut live = String::from("");
        write!(
            live,
            "{}",
            self.live_locals
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<String>>()
                .join(", ")
        )?;
        write!(
            f,
            "guard({}, {}, {}, [{}])",
            self.val, self.kind, self.block, live
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
    Boolean(bool)
}

impl fmt::Display for GuardKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Integer(u128v) => write!(f, "integer({})", u128v),
            Self::OtherInteger(u128vs) => write!(f, "other_integer({:?})", u128vs),
            Self::Boolean(expect) => write!(f, "bool({})", expect)
        }
    }
}

/// A TIR operation. A collection of these makes a TIR trace.
#[derive(Debug)]
pub enum TirOp {
    Statement(Statement),
    Guard(Guard)
}

impl fmt::Display for TirOp {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            TirOp::Statement(st) => write!(f, "{}", st),
            TirOp::Guard(gd) => write!(f, "{}", gd)
        }
    }
}

impl TirOp {
    /// Returns true if the operation may affect locals besides those appearing in the operation.
    fn may_have_side_effects(&self) -> bool {
        if let TirOp::Statement(s) = self {
            s.may_have_side_effects()
        } else {
            false
        }
    }

    fn maybe_defined_locals(&self) -> Vec<Local> {
        match &self {
            TirOp::Statement(stmt) => stmt.maybe_defined_locals(),
            TirOp::Guard(guard) => guard.maybe_defined_locals()
        }
    }

    fn used_locals(&self) -> Vec<Local> {
        match &self {
            TirOp::Statement(stmt) => stmt.used_locals(),
            TirOp::Guard(guard) => guard.used_locals()
        }
    }
}

#[cfg(test)]
pub mod test_helpers {}

#[cfg(test)]
pub mod tests {
    use super::TirTrace;
    use crate::{
        sir::{self, SIR},
        start_tracing, trace_debug, TracingKind
    };
    use fm::FMBuilder;
    use regex::Regex;
    use test::black_box;

    /// Fuzzy matches the textual TIR for the trace `tt` with the pattern `ptn`.
    fn assert_tir(ptn: &str, tt: &TirTrace) {
        let ptn_re = Regex::new(r"%.+?\b").unwrap(); // Names are words prefixed with `%`.
        let text_re = Regex::new(r"\$?.+?\b").unwrap(); // Any word optionally prefixed with `$`.
        let matcher = FMBuilder::new(ptn)
            .unwrap()
            .name_matcher(Some((ptn_re, text_re)))
            .distinct_name_matching(true)
            .build()
            .unwrap();

        let res = matcher.matches(&format!("{}", tt));
        if let Err(e) = res {
            panic!("{}", e);
        }
    }

    #[test]
    fn nonempty_tir_trace() {
        #[inline(never)]
        #[interp_step]
        fn work(io: &mut IO) {
            let mut res = 0;
            while res < io.1 {
                res += io.0;
            }
            io.2 = res
        }

        struct IO(usize, usize, usize);
        let mut io = IO(3, 13, 0);
        let tracer = start_tracing(TracingKind::default());
        black_box(work(&mut io));
        let sir_trace = tracer.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &sir_trace).unwrap();
        assert_eq!(io.2, 15);
        assert!(tir_trace.len() > 0);
    }

    struct DebugTirIO(usize, usize);

    #[inline(never)]
    #[interp_step]
    fn debug_tir_work(io: &mut DebugTirIO) {
        match io.0 {
            0 => {
                trace_debug("Add 10");
                io.1 += 10;
            }
            1 => {
                trace_debug("Minus 2");
                io.1 -= 2;
            }
            2 => {
                trace_debug("Multiply 2");
                io.1 *= 2;
            }
            _ => unreachable!()
        }
    }

    #[test]
    fn trace_debug_tir() {
        let mut io = DebugTirIO(0, 0);
        let tracer = start_tracing(TracingKind::default());
        black_box(debug_tir_work(&mut io)); // +10
        black_box(debug_tir_work(&mut io)); // +10
        io.0 = 2;
        black_box(debug_tir_work(&mut io)); // *2
        io.0 = 1;
        black_box(debug_tir_work(&mut io)); // -2
        let sir_trace = tracer.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &sir_trace).unwrap();
        assert_eq!(io.1, 38);
        assert_tir(
            "...\n\
            ops:\n\
              ...
              // Add 10
              ...
              ... + 10usize (checked)
              ...
              // Add 10
              ...
              ... + 10usize (checked)
              ...
              // Multiply 2
              ...
              ... * 2usize (checked)
              ...
              // Minus 2
              ...
              ... - 2usize (checked)
              ...",
            &tir_trace
        );
    }

    #[test]
    fn no_zero_locals() {
        let mut io = DebugTirIO(0, 0);
        let tracer = start_tracing(TracingKind::default());
        black_box(debug_tir_work(&mut io));
        io.0 = 1;
        black_box(debug_tir_work(&mut io));
        io.0 = 2;
        let sir_trace = tracer.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &sir_trace).unwrap();
        for idx in 0..tir_trace.len() {
            let op = unsafe { tir_trace.op(idx) };
            assert!(!op.used_locals().contains(&sir::RETURN_LOCAL));
        }
    }
}
