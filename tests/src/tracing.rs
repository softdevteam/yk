//! Low-level tracing tests.

// FIXME hard-coded hardware testing. Use default mode, but requires more shimming.

use std::{hint::black_box, thread};
use ykshim_client::{start_tracing, TracingKind};

// Some work to trace.
#[interp_step]
fn work(io: &mut InterpCtx) -> bool {
    let mut res = 0;
    for i in 0..(io.0) {
        if i % 2 == 0 {
            res += 5;
        } else {
            res += 10 / i;
        }
    }
    println!("{}", res); // prevents the above from being optimised out.
    true
}

struct InterpCtx(usize);

/// Test that basic tracing works.
#[test]
fn trace() {
    #[cfg(tracermode = "hw")]
    let th = start_tracing(TracingKind::HardwareTracing);
    #[cfg(tracermode = "sw")]
    let th = start_tracing(TracingKind::SoftwareTracing);
    black_box(work(&mut InterpCtx(10)));
    let trace = th.stop_tracing().unwrap();
    assert!(trace.len() > 0);
}

/// Test that tracing twice sequentially in the same thread works.
#[test]
fn trace_twice() {
    #[cfg(tracermode = "hw")]
    let kind = TracingKind::HardwareTracing;
    #[cfg(tracermode = "sw")]
    let kind = TracingKind::SoftwareTracing;

    let th1 = start_tracing(kind);
    black_box(work(&mut InterpCtx(10)));
    let trace1 = th1.stop_tracing().unwrap();

    let th2 = start_tracing(kind);
    black_box(work(&mut InterpCtx(20)));
    let trace2 = th2.stop_tracing().unwrap();

    assert!(trace1.len() < trace2.len());
}

/// Test that tracing in different threads works.
#[test]
pub(crate) fn trace_concurrent() {
    #[cfg(tracermode = "hw")]
    let kind = TracingKind::HardwareTracing;
    #[cfg(tracermode = "sw")]
    let kind = TracingKind::SoftwareTracing;

    let thr = thread::spawn(move || {
        let th1 = start_tracing(kind);
        black_box(work(&mut InterpCtx(10)));
        th1.stop_tracing().unwrap().len()
    });

    let th2 = start_tracing(kind);
    black_box(work(&mut InterpCtx(20)));
    let len2 = th2.stop_tracing().unwrap().len();

    let len1 = thr.join().unwrap();
    assert!(len1 < len2);
}
