//! API to the optimised internals of the JIT runtime.
//!
//! For more information, see this section in the documentation:
//! https://softdevteam.github.io/ykdocs/tech/yk_structure.html

#[cfg(feature = "testing")]
mod test_api;

use std::ffi::{c_void, CString};
use std::os::raw::c_char;
use ykcompile::CompiledTrace;
use yktrace::{
    sir::{SirTrace, SIR},
    tir::TirTrace,
    ThreadTracer,
};

/// The different ways by which we can collect a trace.
#[derive(Clone, Copy)]
#[repr(u8)]
#[allow(dead_code)]
enum TracingKind {
    /// Software tracing via ykrustc.
    SoftwareTracing = 0,
    /// Hardware tracing via ykrustc + hwtracer.
    HardwareTracing = 1,
}

/// Starts tracing using the specified kind, returning a ThreadTracer through which to stop
/// tracing.
#[no_mangle]
unsafe extern "C" fn __ykshim_start_tracing(tracing_kind: u8) -> *mut ThreadTracer {
    let tracing_kind = match tracing_kind {
        0 => yktrace::TracingKind::SoftwareTracing,
        1 => yktrace::TracingKind::HardwareTracing,
        _ => return std::ptr::null_mut(),
    };
    let tracer = yktrace::start_tracing(tracing_kind);
    Box::into_raw(Box::new(tracer))
}

/// Stops tracing, consuming the ThreadTracer and returning an opaque pointer to a SirTrace. If an
/// error occurs then the returned pointer will be NULL and `error_msg` will contain details of the
/// error.
#[no_mangle]
unsafe extern "C" fn __ykshim_stop_tracing(
    tracer: *mut ThreadTracer,
    error_msg: *mut *mut c_char,
) -> *mut SirTrace {
    let tracer = Box::from_raw(tracer);
    let sir_trace = tracer.stop_tracing();
    match sir_trace {
        Ok(sir_trace) => Box::into_raw(Box::new(sir_trace)),
        Err(err) => {
            *error_msg = CString::new(err.to_string()).unwrap().into_raw();
            std::ptr::null_mut()
        }
    }
}

/// Compiles a SIR trace into an opaque pointer to a native code trace. If an error occurs, the
/// returned pointer will be null, and `error_msg` will contain details of the error.
#[no_mangle]
unsafe extern "C" fn __ykshim_compile_trace(
    sir_trace: *mut SirTrace,
    error_msg: *mut *mut c_char,
) -> *mut CompiledTrace {
    let sir_trace = Box::from_raw(sir_trace);
    let tt = match TirTrace::new(&*SIR, &*sir_trace) {
        Ok(tt) => tt,
        Err(err) => {
            *error_msg = CString::new(err.to_string()).unwrap().into_raw();
            return std::ptr::null_mut();
        }
    };
    let compiled_trace = ykcompile::compile_trace(tt);
    Box::into_raw(Box::new(compiled_trace))
}

/// Gets a callable function pointer from a compiled trace.
#[no_mangle]
unsafe extern "C" fn __ykshim_compiled_trace_get_ptr(
    compiled_trace: *const CompiledTrace,
) -> *const c_void {
    let compiled_trace = &*(compiled_trace as *mut CompiledTrace);
    compiled_trace.ptr() as *const c_void
}

/// Drop a compiled trace.
#[no_mangle]
unsafe extern "C" fn __ykshim_compiled_trace_drop(compiled_trace: *mut CompiledTrace) {
    Box::from_raw(compiled_trace);
}

/// Drop a SIR trace.
#[no_mangle]
unsafe extern "C" fn __ykshim_sirtrace_drop(trace: *mut SirTrace) {
    Box::from_raw(trace);
}

/// Drop a TIR trace.
#[no_mangle]
unsafe fn __ykshim_tirtrace_drop(tir_trace: *mut TirTrace) {
    Box::from_raw(tir_trace);
}

/// Start an initialised StopgapInterpreter.
#[no_mangle]
unsafe extern "C" fn __ykshim_si_interpret(si: *mut yksg::StopgapInterpreter) -> bool {
    let si = &mut *si;
    si.interpret()
}

#[no_mangle]
unsafe extern "C" fn __ykshim_sirinterpreter_drop(interp: *mut yksg::StopgapInterpreter) {
    Box::from_raw(interp);
}
