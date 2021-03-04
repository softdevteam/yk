//! Testing the stopgap interpreter via guard failures.

use ykshim_client::{compile_trace, start_tracing, StopgapInterpreter, TracingKind};

#[test]
fn simple() {
    struct InterpCtx(u8, u8);

    fn guard(i: u8) -> u8 {
        if i != 3 {
            9
        } else {
            10
        }
    }

    #[interp_step]
    fn interp_step(io: &mut InterpCtx) -> bool {
        let x = guard(io.0);
        io.1 = x;
        true
    }

    let mut ctx = InterpCtx(std::hint::black_box(|i| i)(0), 0);
    #[cfg(tracermode = "hw")]
    let th = start_tracing(TracingKind::HardwareTracing);
    #[cfg(tracermode = "sw")]
    let th = start_tracing(TracingKind::SoftwareTracing);
    interp_step(&mut ctx);
    let sir_trace = th.stop_tracing().unwrap();
    let ct = compile_trace(sir_trace).unwrap();
    let mut args = InterpCtx(0, 0);
    assert!(unsafe { ct.execute(&mut args).is_null() });
    assert_eq!(args.1, 9);
    // Execute the trace with a context that causes a guard to fail.
    let mut args = InterpCtx(3, 0);
    let ptr = unsafe { ct.execute(&mut args) };
    assert!(!ptr.is_null());
    // Check that running the interpreter gets us the correct result.
    let mut si: StopgapInterpreter = StopgapInterpreter(ptr);
    unsafe { si.interpret() };
    assert_eq!(args.1, 10);
}

#[test]
fn recursion() {
    struct InterpCtx(u8, u8);

    // Test that if a guard fails within a recursive call, we still construct the correct stack
    // frames for the stopgap interpreter.
    fn rec(i: u8, j: u8) -> u8 {
        let mut k = i;
        if i < 1 {
            k = rec(i + 1, j);
            if j == 1 {
                // Produce a guard failure here deep within multiple recursions.
                k = 99;
            }
        }
        return k;
    }

    #[interp_step]
    fn interp_step(io: &mut InterpCtx) -> bool {
        let x = rec(io.0, io.1);
        io.1 = x;
        true
    }

    let mut ctx = InterpCtx(std::hint::black_box(|i| i)(0), 0);
    #[cfg(tracermode = "hw")]
    let th = start_tracing(TracingKind::HardwareTracing);
    #[cfg(tracermode = "sw")]
    let th = start_tracing(TracingKind::SoftwareTracing);
    interp_step(&mut ctx);
    let sir_trace = th.stop_tracing().unwrap();
    let ct = compile_trace(sir_trace).unwrap();
    let mut args = InterpCtx(0, 0);
    assert!(unsafe { ct.execute(&mut args).is_null() });
    assert_eq!(args.1, 1);
    // Execute the trace with a context that causes a guard to fail.
    let mut args = InterpCtx(0, 1);
    let ptr = unsafe { ct.execute(&mut args) };
    assert!(!ptr.is_null());
    // Check that running the interpreter gets us the correct result.
    let mut si: StopgapInterpreter = StopgapInterpreter(ptr);
    unsafe { si.interpret() };
    assert_eq!(args.1, 99);
}

#[test]
fn recursion2() {
    struct InterpCtx(u8, u8);

    // Test that the SIR interpreter can deal with new recursions after a guard failure.
    fn rec(i: u8) -> u8 {
        if i < 5 {
            return rec(i + 1);
        } else {
            return i;
        }
    }

    #[interp_step]
    fn interp_step(io: &mut InterpCtx) -> bool {
        let x = rec(io.0);
        io.1 = x;
        true
    }

    let mut ctx = InterpCtx(7, 0);
    #[cfg(tracermode = "hw")]
    let th = start_tracing(TracingKind::HardwareTracing);
    #[cfg(tracermode = "sw")]
    let th = start_tracing(TracingKind::SoftwareTracing);
    interp_step(&mut ctx);
    let sir_trace = th.stop_tracing().unwrap();
    let ct = compile_trace(sir_trace).unwrap();
    let mut args = InterpCtx(7, 0);
    assert!(unsafe { ct.execute(&mut args).is_null() });
    assert_eq!(args.1, 7);
    // Execute the trace with a context that causes a guard to fail.
    let mut args = InterpCtx(1, 0);
    let ptr = unsafe { ct.execute(&mut args) };
    assert!(!ptr.is_null());
    // Check that running the interpreter gets us the correct result.
    let mut si: StopgapInterpreter = StopgapInterpreter(ptr);
    unsafe { si.interpret() };
    assert_eq!(args.1, 5);
}

#[test]
fn trace_looping() {
    struct InterpCtx(u8);

    #[interp_step]
    fn interp_step(io: &mut InterpCtx) -> bool {
        if io.0 < 100 {
            io.0 += 1;
            return false;
        }
        true
    }

    let mut ctx = InterpCtx(0);
    #[cfg(tracermode = "hw")]
    let th = start_tracing(TracingKind::HardwareTracing);
    #[cfg(tracermode = "sw")]
    let th = start_tracing(TracingKind::SoftwareTracing);
    interp_step(&mut ctx);
    let sir_trace = th.stop_tracing().unwrap();
    let ct = compile_trace(sir_trace).unwrap();
    let mut args = InterpCtx(0);
    // With trace looping, calling the trace once will loop until the `interp_step` function
    // returns true.
    unsafe { ct.execute(&mut args) };
    assert_eq!(args.0, 100);
}

#[test]
fn trace_looping2() {
    struct InterpCtx(u8);

    #[interp_step]
    fn interp_step(io: &mut InterpCtx) -> bool {
        if io.0 < 100 {
            io.0 += 1;
            false
        } else if io.0 < 200 {
            io.0 += 2;
            false
        } else {
            true
        }
    }

    let mut ctx = InterpCtx(0);
    #[cfg(tracermode = "hw")]
    let th = start_tracing(TracingKind::HardwareTracing);
    #[cfg(tracermode = "sw")]
    let th = start_tracing(TracingKind::SoftwareTracing);
    interp_step(&mut ctx);
    let sir_trace = th.stop_tracing().unwrap();
    let ct = compile_trace(sir_trace).unwrap();
    let mut args = InterpCtx(0);
    unsafe { ct.execute(&mut args) };
    assert_eq!(args.0, 100);
}

#[test]
fn trace_looping_nested() {
    struct InterpCtx(u8);

    fn func() -> bool {
        true
    }

    #[interp_step]
    fn interp_step(io: &mut InterpCtx) -> bool {
        if io.0 < 100 {
            io.0 += 1;
            // Other functions returning true must not interfere with the looping.
            func();
            return false;
        }
        true
    }

    let mut ctx = InterpCtx(0);
    #[cfg(tracermode = "hw")]
    let th = start_tracing(TracingKind::HardwareTracing);
    #[cfg(tracermode = "sw")]
    let th = start_tracing(TracingKind::SoftwareTracing);
    interp_step(&mut ctx);
    let sir_trace = th.stop_tracing().unwrap();
    let ct = compile_trace(sir_trace).unwrap();
    let mut args = InterpCtx(0);
    unsafe { ct.execute(&mut args) };
    assert_eq!(args.0, 100);
}
