//! Scoped uncaught reporting at the active Runtime job boundary.
//!
//! # Contents
//! - The existing process/domain uncaught handler algorithm.
//! - Rooted restoration of an unhandled exception after collecting handlers.
//!
//! # Invariants
//! The caller installs the exact job realm, async context and optional source.
//! Handler calls use the canonical native call boundary and resolve bytecode
//! callees by their own FunctionID. No source context is invented. The original
//! thrown value is a scoped handle throughout all property access and calls;
//! an unhandled result restores its collector-updated identity. Fatal/control
//! and escaping OOM remain typed errors for the caller's checkpoint policy.
//!
//! # See also
//! - `otter_vm::Interpreter::drain_microtasks` owns current checkpoint stopping.
//! - `crate::checkpoint` guards direct script-to-checkpoint admission.

use otter_vm::{NativeCtx, NativeError};

pub(crate) fn dispatch(ctx: &mut NativeCtx<'_>) -> Result<bool, NativeError> {
    ctx.scope(|mut scope| {
        let Some(thrown) = scope.take_pending_uncaught_throw() else {
            return Ok(false);
        };
        let origin_name = if scope.take_uncaught_from_promise_rejection() {
            "unhandledRejection"
        } else {
            "uncaughtException"
        };
        let result = (|| -> Result<bool, NativeError> {
            // A domain claims the error first: that is the whole point
            // of `domain.run`, and it must win over the process-wide
            // handlers below.
            if let Some(domain) = scope.global("__otterDomainModule") {
                let handler = scope.get(domain, "_handleUncaught")?;
                if scope.is_callable(handler) {
                    let handled = scope.call(handler, domain, &[thrown])?;
                    if scope.boolean_value(handled).unwrap_or(false) {
                        return Ok(true);
                    }
                }
            }

            let Some(process) = scope.global("process") else {
                return Ok(false);
            };

            // A program may replace `process._fatalException`; a
            // non-function replacement is Node's "internal fatal
            // exception handler failure" (exit code 6).
            let fatal = scope.get(process, "_fatalException")?;
            if !scope.is_undefined(fatal) && !scope.is_callable(fatal) {
                return Err(otter_vm::NativeError::Coded {
                    kind: otter_vm::ErrorKind::TypeError,
                    code: "ERR_FATAL_HANDLER_INVALID",
                    message: "process._fatalException is not a function".to_string(),
                });
            }
            if scope.is_undefined(fatal) {
                // `process` is a null-prototype object, so the probe
                // borrows `Object.prototype.hasOwnProperty`.
                let has_own = {
                    let object_ctor = scope.global("Object");
                    let probe = match object_ctor {
                        Some(object_ctor) => {
                            let prototype = scope.get(object_ctor, "prototype")?;
                            Some(scope.get(prototype, "hasOwnProperty")?)
                        }
                        None => None,
                    };
                    match probe {
                        Some(probe) if scope.is_callable(probe) => {
                            let key = scope.string("_fatalException")?;
                            let owned = scope.call(probe, process, &[key])?;
                            scope.boolean_value(owned).unwrap_or(false)
                        }
                        _ => false,
                    }
                };
                if has_own {
                    return Err(otter_vm::NativeError::Coded {
                        kind: otter_vm::ErrorKind::TypeError,
                        code: "ERR_FATAL_HANDLER_INVALID",
                        message: "process._fatalException is not a function".to_string(),
                    });
                }
            }

            // `uncaughtExceptionMonitor` observes every uncaught
            // exception before any handler — including the crash
            // path — and cannot mark it handled.
            {
                let emit = scope.get(process, "emit")?;
                if scope.is_callable(emit) {
                    let event = scope.string("uncaughtExceptionMonitor")?;
                    let origin = scope.string(origin_name)?;
                    scope.call(emit, process, &[event, thrown, origin])?;
                }
            }

            let capture = scope.get(process, crate::process_control::CAPTURE_SLOT)?;
            if scope.is_callable(capture) {
                scope.call(capture, process, &[thrown])?;
                return Ok(true);
            }

            let count = scope.get(process, "listenerCount")?;
            if !scope.is_callable(count) {
                return Ok(false);
            }
            let event = scope.string("uncaughtException")?;
            let listeners = scope.call(count, process, &[event])?;
            if scope.number_value(listeners).unwrap_or(0.0) < 1.0 {
                return Ok(false);
            }
            let emit = scope.get(process, "emit")?;
            if !scope.is_callable(emit) {
                return Ok(false);
            }
            let event = scope.string("uncaughtException")?;
            let origin = scope.string(origin_name)?;
            scope.call(emit, process, &[event, thrown, origin])?;
            Ok(true)
        })();
        if matches!(result, Ok(false)) {
            scope.set_pending_uncaught_throw(thrown);
        }
        result
    })
}

#[cfg(test)]
mod tests;
