//! Textual TIR matching tests.

use crate::helpers::{add6, assert_tir, neg_assert_tir};
use std::hint::black_box;
use ykrt::trace_debug;
use ykshim_client::{start_tracing, TirTrace, TracingKind};

#[test]
fn nonempty_tir_trace() {
    #[inline(never)]
    #[interp_step]
    fn work(io: &mut InterpCtx) -> bool {
        let mut res = 0;
        while res < io.1 {
            res += io.0;
        }
        io.2 = res;
        true
    }

    struct InterpCtx(usize, usize, usize);
    let mut io = InterpCtx(3, 13, 0);
    // FIXME TracingMode::Default.
    #[cfg(tracermode = "hw")]
    let tracer = start_tracing(TracingKind::HardwareTracing);
    #[cfg(tracermode = "sw")]
    let tracer = start_tracing(TracingKind::SoftwareTracing);
    black_box(work(&mut io));
    let sir_trace = tracer.stop_tracing().unwrap();
    let tir_trace = TirTrace::new(&sir_trace);
    assert_eq!(io.2, 15);
    assert!(tir_trace.len() > 0);
}

struct DebugTirInterpCtx(usize, usize);

#[inline(never)]
#[interp_step]
fn debug_tir_work(io: &mut DebugTirInterpCtx) -> bool {
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
        _ => unreachable!(),
    }
    true
}

#[test]
fn trace_debug_tir() {
    let mut io = DebugTirInterpCtx(0, 0);
    #[cfg(tracermode = "hw")]
    let tracer = start_tracing(TracingKind::HardwareTracing);
    #[cfg(tracermode = "sw")]
    let tracer = start_tracing(TracingKind::SoftwareTracing);
    black_box(debug_tir_work(&mut io)); // +10
    black_box(debug_tir_work(&mut io)); // +10
    io.0 = 2;
    black_box(debug_tir_work(&mut io)); // *2
    io.0 = 1;
    black_box(debug_tir_work(&mut io)); // -2
    let sir_trace = tracer.stop_tracing().unwrap();
    let tir_trace = TirTrace::new(&sir_trace);
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
        &tir_trace,
    );
}

#[test]
fn call_symbol_tir() {
    struct InterpCtx(());
    #[interp_step]
    fn interp_step(_: &mut InterpCtx) -> bool {
        let _ = unsafe { add6(1, 1, 1, 1, 1, 1) };
        true
    }

    #[cfg(tracermode = "hw")]
    let th = start_tracing(TracingKind::HardwareTracing);
    #[cfg(tracermode = "sw")]
    let th = start_tracing(TracingKind::SoftwareTracing);
    interp_step(&mut InterpCtx(()));
    let sir_trace = th.stop_tracing().unwrap();
    let tir_trace = TirTrace::new(&sir_trace);
    assert_tir(
        "...\n\
            ops:\n\
              ...
              %a = call(add6, [1u64, 1u64, 1u64, 1u64, 1u64, 1u64])\n\
              ...
              dead(%a)\n\
              ...",
        &tir_trace,
    );
}

#[test]
fn do_not_trace() {
    struct InterpCtx(u8);

    #[do_not_trace]
    fn dont_trace_this(a: u8) -> u8 {
        let b = 2;
        let c = a + b;
        c
    }

    #[interp_step]
    fn interp_step(io: &mut InterpCtx) -> bool {
        io.0 = dont_trace_this(io.0);
        true
    }

    let mut ctx = InterpCtx(1);
    #[cfg(tracermode = "hw")]
    let th = start_tracing(TracingKind::HardwareTracing);
    #[cfg(tracermode = "sw")]
    let th = start_tracing(TracingKind::SoftwareTracing);
    interp_step(&mut ctx);
    let sir_trace = th.stop_tracing().unwrap();
    let tir_trace = TirTrace::new(&sir_trace);

    assert_tir(
        "
            local_decls:
              ...
            ops:
              ...
              %s1 = call(...
              ...",
        &tir_trace,
    );
}

#[test]
fn loop_trace() {
    struct InterpCtx(u8);

    #[interp_step]
    fn interp_step(io: &mut InterpCtx) -> bool {
        if io.0 < 100 {
            io.0 += 1;
            return false;
        }
        true
    }

    let mut ctx = InterpCtx(1);
    #[cfg(tracermode = "hw")]
    let th = start_tracing(TracingKind::HardwareTracing);
    #[cfg(tracermode = "sw")]
    let th = start_tracing(TracingKind::SoftwareTracing);
    interp_step(&mut ctx);
    let sir_trace = th.stop_tracing().unwrap();
    let tir_trace = TirTrace::new(&sir_trace);

    assert_tir(
        "
            local_decls:
              ...
            ops:
              LoopStart
              ...
              LoopEnd",
        &tir_trace,
    );
}

#[test]
fn dont_loop_trace() {
    struct InterpCtx(u8);

    #[interp_step]
    fn interp_step(io: &mut InterpCtx) -> bool {
        io.0 = 2;
        true
    }

    let mut ctx = InterpCtx(1);
    #[cfg(tracermode = "hw")]
    let th = start_tracing(TracingKind::HardwareTracing);
    #[cfg(tracermode = "sw")]
    let th = start_tracing(TracingKind::SoftwareTracing);
    interp_step(&mut ctx);
    let sir_trace = th.stop_tracing().unwrap();
    let tir_trace = TirTrace::new(&sir_trace);

    neg_assert_tir(
        "
            local_decls:
              ...
            ops:
              LoopStart
              ...
              LoopEnd",
        &tir_trace,
    );
}
