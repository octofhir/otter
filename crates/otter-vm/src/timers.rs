//! Per-isolate timer state owned by [`crate::Interpreter`].
//!
//! ECMA-262 has no notion of timers; the spec scaffolding lives in
//! HTML §8.1.5.5.4 (`setTimeout`) and §8.1.5.5.7 (`setInterval`).
//! What ECMA-262 DOES specify is the microtask drain ordering
//! relative to "tasks" (§9.4 Jobs and Job Queues): every host task
//! must drain pending microtasks before another task starts.
//!
//! # Architecture
//!
//! Timers are an *isolate-local* construct because the callback is
//! a JS [`Value`] (closure / native) bound to a specific
//! `BytecodeModule`. Scheduling itself, however, is *host-side*:
//! the runtime layer owns the Tokio worker that fires the inbox
//! [`crate::microtask`]-equivalent message after the delay.
//!
//! This module only owns the bridge:
//!
//! - [`TimerScheduler`] — trait the runtime layer implements to
//!   talk to its event loop without otter-vm depending on Tokio.
//! - [`TimerCallbacks`] — per-interpreter table mapping the
//!   runtime-issued [`u64`] token to the JS callback + extra
//!   arguments + interval (for `setInterval`).
//!
//! # Invariants
//!
//! - Token allocation is the runtime layer's responsibility. The
//!   VM only stores callbacks under tokens it has been handed.
//! - Token reuse is impossible: tokens are u64-monotonic on the
//!   runtime side. The VM treats them as opaque keys.
//! - Cancellation deletes the entry from [`TimerCallbacks`] so a
//!   late `TimerFired` (lost the cancel race) becomes a no-op
//!   rather than running a stale callback.
//!
//! # See also
//!
//! - [HTML setTimeout](https://html.spec.whatwg.org/multipage/timers-and-user-prompts.html#dom-settimeout)
//! - [Microtask queue](crate::microtask)

use std::collections::HashMap;
use std::sync::Arc;

use otter_gc::raw::RawGc;
use smallvec::SmallVec;

use crate::native_function::{NativeCall, NativeError, NativeFastFn};
use crate::number::{self, NumberValue};
use crate::object::JsObject;
use crate::runtime_cx::NativeCtx;
use crate::{Attr, JsSurfaceError, ObjectBuilder, Value};

/// Host-side scheduler the runtime layer plugs in. Lives behind
/// an [`Arc<dyn TimerScheduler>`] on [`crate::Interpreter`].
///
/// Implementations must be `Send + Sync` because the VM stores
/// the handle on isolate-local state, but the underlying scheduler
/// usually owns a Tokio runtime that crosses thread boundaries.
pub trait TimerScheduler: Send + Sync {
    /// Schedule a fresh one-shot or repeating timer. Returns the
    /// stable token the VM uses to identify the entry; the VM
    /// stores its callback under this key. The implementation MUST
    /// post a runtime inbox message (e.g.
    /// `RuntimeMessage::TimerFired { token }`) when the delay
    /// elapses so the isolate runner can re-enter the VM and run
    /// the callback. Repeating timers re-arm themselves on the
    /// host side until [`Self::cancel`] removes the token.
    fn schedule(&self, delay_ms: u64, repeat_ms: Option<u64>) -> u64;

    /// Cancel a pending timer. Returns `true` when the token was
    /// known to the host and the schedule was suppressed before
    /// firing. A late cancel (callback already invoked) returns
    /// `false`; the VM additionally drops the entry from
    /// [`TimerCallbacks`] so the late fire is a no-op.
    fn cancel(&self, token: u64) -> bool;
}

/// Cloneable handle the VM uses to talk to the host scheduler.
pub type TimerSchedulerHandle = Arc<dyn TimerScheduler>;

/// Stored callback for one outstanding `setTimeout` / `setInterval`.
#[derive(Debug, Clone)]
pub struct TimerEntry {
    /// JS callable to invoke when the delay elapses.
    pub callback: Value,
    /// Extra positional arguments forwarded to the callback per
    /// HTML §8.1.5.5.4 (`setTimeout(handler, delay, ...arguments)`).
    pub extra_args: SmallVec<[Value; 4]>,
    /// `Some(ms)` for `setInterval`; `None` for `setTimeout`.
    /// Re-arming is the host's job — the VM only inspects this
    /// field to keep the entry alive after firing instead of
    /// removing it.
    pub repeat_ms: Option<u64>,
}

impl TimerEntry {
    /// Trace every GC-bearing slot held by this entry.
    pub(crate) fn trace_gc_slots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        self.callback.trace_value_slots(visitor);
        for arg in &self.extra_args {
            arg.trace_value_slots(visitor);
        }
    }
}

/// Per-interpreter map keyed by host-issued token.
#[derive(Debug, Default)]
pub struct TimerCallbacks {
    entries: HashMap<u64, TimerEntry>,
}

impl TimerCallbacks {
    /// Empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a freshly scheduled timer.
    pub fn insert(&mut self, token: u64, entry: TimerEntry) {
        self.entries.insert(token, entry);
    }

    /// Remove a timer entry, e.g. on `clearTimeout` or after a
    /// one-shot fires. Repeating timers stay in the table until
    /// the host cancels them.
    pub fn remove(&mut self, token: u64) -> Option<TimerEntry> {
        self.entries.remove(&token)
    }

    /// Borrow an entry by token without removing it. Used by the
    /// fire path so a repeating callback's `repeat_ms` can be
    /// observed before deciding whether to keep the entry.
    #[must_use]
    pub fn get(&self, token: u64) -> Option<&TimerEntry> {
        self.entries.get(&token)
    }

    /// Number of registered timers — diagnostic only.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when no entries are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Trace every entry's GC-bearing slots. Called from
    /// [`crate::runtime_state::RuntimeState::trace_roots`] so
    /// callbacks survive across collections that occur between
    /// scheduling and firing.
    pub(crate) fn trace_gc_slots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        for entry in self.entries.values() {
            entry.trace_gc_slots(visitor);
        }
    }
}

// -- Globals: setTimeout / setInterval / clearTimeout / clearInterval ----

const TIMER_NATIVES: &[(&str, u8, NativeFastFn)] = &[
    ("setTimeout", 1, set_timeout_native),
    ("setInterval", 1, set_interval_native),
    ("clearTimeout", 1, clear_timeout_native),
    ("clearInterval", 1, clear_interval_native),
];

/// Install the `setTimeout` / `setInterval` / `clearTimeout` /
/// `clearInterval` natives on the global object.
///
/// HTML §8.1.5.5.4 requires these to live as plain global
/// functions. Otter follows that exactly — they are not bound to
/// a `window`-style namespace because Otter has no document. The
/// scheduler implementation is provided by the runtime layer
/// (see [`crate::Interpreter::set_timer_scheduler`]); a script
/// running without a scheduler installed receives a TypeError
/// when it calls one of the natives.
pub(crate) fn install_timer_globals(
    global_this: JsObject,
    heap: &mut otter_gc::GcHeap,
) -> Result<(), JsSurfaceError> {
    let mut builder = ObjectBuilder::from_object(heap, global_this);
    for (name, length, call) in TIMER_NATIVES {
        builder.method(
            name,
            *length,
            NativeCall::Static(*call),
            Attr::builtin_function(),
        )?;
    }
    Ok(())
}

fn coerce_delay_ms(value: Option<&Value>) -> u64 {
    let n = match value {
        Some(Value::Number(num)) => num.as_f64(),
        Some(Value::Undefined) | None => 0.0,
        Some(other) => number::parse::to_number_value(other),
    };
    if n.is_nan() || n <= 0.0 {
        0
    } else {
        let clamped = n.min(u64::MAX as f64);
        clamped as u64
    }
}

fn ensure_callable(value: &Value, native: &'static str) -> Result<(), NativeError> {
    if crate::is_callable_value(value) {
        Ok(())
    } else {
        Err(NativeError::TypeError {
            name: native,
            reason: "callback is not a function".to_string(),
        })
    }
}

fn schedule_timer_common(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    repeat: bool,
    native_name: &'static str,
) -> Result<Value, NativeError> {
    let callback = args.first().cloned().unwrap_or(Value::Undefined);
    ensure_callable(&callback, native_name)?;
    let delay_ms = coerce_delay_ms(args.get(1));
    let extra: SmallVec<[Value; 4]> = args.iter().skip(2).cloned().collect();
    let interp = ctx.interp_mut();
    let scheduler = interp.timer_scheduler().ok_or(NativeError::TypeError {
        name: native_name,
        reason: "host runtime did not install a timer scheduler".to_string(),
    })?;
    let token = scheduler.schedule(delay_ms, repeat.then_some(delay_ms));
    interp.timer_callbacks_mut().insert(
        token,
        TimerEntry {
            callback,
            extra_args: extra,
            repeat_ms: repeat.then_some(delay_ms),
        },
    );
    Ok(Value::Number(NumberValue::from_f64(token as f64)))
}

fn cancel_timer_common(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    _native_name: &'static str,
) -> Result<Value, NativeError> {
    let token = match args.first() {
        Some(Value::Number(n)) => {
            let raw = n.as_f64();
            if raw.is_finite() && raw >= 0.0 {
                raw as u64
            } else {
                return Ok(Value::Undefined);
            }
        }
        _ => return Ok(Value::Undefined),
    };
    let interp = ctx.interp_mut();
    interp.timer_callbacks_mut().remove(token);
    if let Some(scheduler) = interp.timer_scheduler() {
        let _ = scheduler.cancel(token);
    }
    Ok(Value::Undefined)
}

fn set_timeout_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    schedule_timer_common(ctx, args, false, "setTimeout")
}

fn set_interval_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    schedule_timer_common(ctx, args, true, "setInterval")
}

fn clear_timeout_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    cancel_timer_common(ctx, args, "clearTimeout")
}

fn clear_interval_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    cancel_timer_common(ctx, args, "clearInterval")
}
