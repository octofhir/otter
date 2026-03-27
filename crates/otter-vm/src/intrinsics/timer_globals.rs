//! Timer and microtask globals — setTimeout, setInterval, clearTimeout,
//! clearInterval, queueMicrotask.
//!
//! These are installed on the global object during intrinsic bootstrap.
//! They delegate to the [`EventLoopHost`] via handles stored on the runtime.
//!
//! Note: The actual timer scheduling happens through the event loop host,
//! not directly in these functions. These natives parse arguments, validate
//! inputs, and record the timer in the runtime's pending timer table. The
//! event loop driver fires callbacks and drains microtasks.

use std::time::Duration;

use crate::descriptors::{
    NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor, VmNativeCallError,
};
use crate::interpreter::RuntimeState;
use crate::microtask::MicrotaskJob;
use crate::object::ObjectHandle;
use crate::value::RegisterValue;

/// Returns the binding descriptors for all timer/microtask globals.
pub(super) fn timer_global_bindings() -> Vec<NativeBindingDescriptor> {
    vec![
        NativeBindingDescriptor::new(
            NativeBindingTarget::Global,
            NativeFunctionDescriptor::method("setTimeout", 1, set_timeout),
        ),
        NativeBindingDescriptor::new(
            NativeBindingTarget::Global,
            NativeFunctionDescriptor::method("setInterval", 1, set_interval),
        ),
        NativeBindingDescriptor::new(
            NativeBindingTarget::Global,
            NativeFunctionDescriptor::method("clearTimeout", 1, clear_timeout),
        ),
        NativeBindingDescriptor::new(
            NativeBindingTarget::Global,
            NativeFunctionDescriptor::method("clearInterval", 1, clear_interval),
        ),
        NativeBindingDescriptor::new(
            NativeBindingTarget::Global,
            NativeFunctionDescriptor::method("queueMicrotask", 1, queue_microtask),
        ),
    ]
}

/// `setTimeout(callback, delay?)` — HTML5 §8.6
///
/// Schedules `callback` to run after `delay` milliseconds (default 0).
/// Returns a numeric timer ID for `clearTimeout`.
fn set_timeout(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let callback = args
        .first()
        .copied()
        .and_then(|v| v.as_object_handle())
        .map(ObjectHandle)
        .ok_or_else(|| {
            VmNativeCallError::Internal("setTimeout requires a function argument".into())
        })?;

    let delay_ms = args
        .get(1)
        .copied()
        .and_then(|v| v.as_number())
        .unwrap_or(0.0)
        .max(0.0) as u64;

    let id = runtime.schedule_timeout(callback, Duration::from_millis(delay_ms));
    Ok(RegisterValue::from_i32(id.0 as i32))
}

/// `setInterval(callback, interval?)` — HTML5 §8.6
fn set_interval(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let callback = args
        .first()
        .copied()
        .and_then(|v| v.as_object_handle())
        .map(ObjectHandle)
        .ok_or_else(|| {
            VmNativeCallError::Internal("setInterval requires a function argument".into())
        })?;

    let interval_ms = args
        .get(1)
        .copied()
        .and_then(|v| v.as_number())
        .unwrap_or(0.0)
        .max(0.0) as u64;

    let id = runtime.schedule_interval(callback, Duration::from_millis(interval_ms));
    Ok(RegisterValue::from_i32(id.0 as i32))
}

/// `clearTimeout(id)` — HTML5 §8.6
fn clear_timeout(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if let Some(id) = args.first().copied().and_then(|v| v.as_i32()) {
        runtime.clear_timer(crate::event_loop_host::TimerId(id as u32));
    }
    Ok(RegisterValue::undefined())
}

/// `clearInterval(id)` — same as clearTimeout per spec
fn clear_interval(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    clear_timeout(_this, args, runtime)
}

/// `queueMicrotask(callback)` — WHATWG §8.7
fn queue_microtask(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let callback = args
        .first()
        .copied()
        .and_then(|v| v.as_object_handle())
        .map(ObjectHandle)
        .ok_or_else(|| {
            VmNativeCallError::Internal("queueMicrotask requires a function argument".into())
        })?;

    runtime.microtasks_mut().enqueue_microtask(MicrotaskJob {
        callback,
        this_value: RegisterValue::undefined(),
        args: vec![],
    });

    Ok(RegisterValue::undefined())
}
