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
//! [`ExecutionContext`]. Scheduling itself, however, is
//! *host-side*: the runtime layer owns the Tokio worker that fires
//! the inbox [`crate::microtask`]-equivalent message after the
//! delay.
//!
//! This module only owns the bridge:
//!
//! - [`TimerScheduler`] — trait the runtime layer implements to
//!   talk to its event loop without otter-vm depending on Tokio.
//! - [`TimerCallbacks`] — per-interpreter table mapping the
//!   runtime-issued [`u64`] token to the JS callback + origin
//!   context + extra arguments + interval (for `setInterval`).
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
//! - Bulk teardown drains callback ownership before asking the host to cancel
//!   deadlines. A cancellation race can therefore only observe a missing
//!   callback, never revive a process that has already finalized.
//! - Each callback is tagged with its scalar origin realm. Realm disposal
//!   removes its entries and cancels the matching host deadlines.
//!
//! # See also
//!
//! - [HTML setTimeout](https://html.spec.whatwg.org/multipage/timers-and-user-prompts.html#dom-settimeout)
//! - [Microtask queue](crate::microtask)

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use otter_gc::raw::RawGc;
use smallvec::SmallVec;

use crate::execution_context::ExecutionContext;
use crate::native_function::{NativeCall, NativeError, NativeFastFn};
use crate::number;
use crate::object::JsObject;
use crate::runtime_cx::NativeCtx;
use crate::{Attr, JsSurfaceError, ObjectBuilder, Value};

/// Unique runtime-owned credit for one live timer origin.
///
/// The VM acquires this before retaining callback state or asking the host to
/// arm a deadline, then moves the same carrier into [`TimerScheduler::schedule`].
/// Dropping it rolls back the runtime's physical and resource-ledger credits.
pub struct TimerAdmission(Option<Box<dyn Any + Send>>);

impl TimerAdmission {
    /// Wrap an embedder-owned timer admission guard.
    #[must_use]
    pub fn new(token: Box<dyn Any + Send>) -> Self {
        Self(Some(token))
    }

    /// Recover a guard of the expected concrete type in the scheduler that
    /// created it. A foreign carrier is returned intact instead of panicking.
    pub fn try_into_inner<T: Any + Send>(mut self) -> Result<Box<T>, Self> {
        let Some(token) = self.0.take() else {
            return Err(self);
        };
        match token.downcast::<T>() {
            Ok(token) => Ok(token),
            Err(token) => {
                self.0 = Some(token);
                Err(self)
            }
        }
    }
}

impl std::fmt::Debug for TimerAdmission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TimerAdmission")
            .field("live", &self.0.is_some())
            .finish()
    }
}

/// Host-side scheduler the runtime layer plugs in. Lives behind
/// an [`Arc<dyn TimerScheduler>`] on [`crate::Interpreter`].
///
/// Implementations must be `Send + Sync` because the VM stores
/// the handle on isolate-local state, but the underlying scheduler
/// usually owns a Tokio runtime that crosses thread boundaries.
pub trait TimerScheduler: Send + Sync {
    /// Reserve one physically bounded live-timer slot before callback state is
    /// retained or a host deadline is armed.
    fn admit(&self, repeat: bool) -> Result<TimerAdmission, String>;

    /// Schedule a fresh one-shot or repeating timer. Returns the
    /// stable token the VM uses to identify the entry; the VM
    /// stores its callback under this key. The implementation MUST
    /// post a runtime inbox message (e.g.
    /// `RuntimeMessage::TimerFired { token }`) when the delay
    /// elapses so the isolate runner can re-enter the VM and run
    /// the callback. Repeating timers re-arm themselves on the
    /// host side until [`Self::cancel`] removes the token.
    fn schedule(
        &self,
        admission: TimerAdmission,
        delay_ms: u64,
        repeat_ms: Option<u64>,
    ) -> Result<u64, String>;

    /// Cancel a pending timer. Returns `true` when the token was known to the
    /// host and its callback can still be suppressed, including a deadline
    /// that fired concurrently but has not dispatched on the isolate. A late
    /// cancel after ownership moved into dispatch returns `false`; the VM
    /// additionally drops the entry from
    /// [`TimerCallbacks`] so the late fire is a no-op.
    fn cancel(&self, token: u64) -> bool;

    /// Move a pending timer between the ref/unref liveness classes.
    /// An unref'd timer still fires while the loop is alive but no
    /// longer holds the run-until-idle boundary open. Returns `false`
    /// for an unknown or already-fired token.
    fn set_ref(&self, token: u64, refed: bool) -> bool;
}

/// Cloneable handle the VM uses to talk to the host scheduler.
pub type TimerSchedulerHandle = Arc<dyn TimerScheduler>;

/// Stored callback for one outstanding `setTimeout` / `setInterval`.
///
/// The entry owns the scheduling [`ExecutionContext`] so a later
/// timer task can dispatch through the same function table. This
/// is intentionally isolate-local state, not VM state crossing to
/// the host scheduler: the entry stays on the isolate side and
/// only the opaque token leaves the VM.
#[derive(Debug, Clone)]
pub struct TimerEntry {
    /// Scalar realm identity that scheduled the callback.
    pub realm_id: u32,
    /// JS callable to invoke when the delay elapses.
    pub callback: Value,
    /// Extra positional arguments forwarded to the callback per
    /// HTML §8.1.5.5.4 (`setTimeout(handler, delay, ...arguments)`).
    pub extra_args: SmallVec<[Value; 4]>,
    /// Execution context that produced the callback. Timer
    /// callbacks may run after another script has executed, so the
    /// entry owns the context needed for dispatch.
    pub context: ExecutionContext,
    /// `Some(ms)` for `setInterval`; `None` for `setTimeout`.
    /// Re-arming is the host's job — the VM only inspects this
    /// field to keep the entry alive after firing instead of
    /// removing it.
    pub repeat_ms: Option<u64>,
    /// Which API scheduled the entry. `setImmediate` and
    /// `setTimeout(fn, 0)` are indistinguishable by delay alone, and a host
    /// reporting its live resources has to tell them apart.
    pub kind: TimerKind,
    /// Async context captured when the timer was scheduled; the host restores
    /// it around the callback.
    pub async_context: Value,
}

/// The API a pending timer entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerKind {
    /// `setTimeout` or `setInterval`.
    Timeout,
    /// `setImmediate`.
    Immediate,
}

impl TimerEntry {
    /// Trace every GC-bearing slot held by this entry.
    pub(crate) fn trace_gc_slots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        self.callback.trace_value_slots(visitor);
        self.async_context.trace_value_slots(visitor);
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

    /// Drain every registered callback and yield its host token.
    ///
    /// The entry is removed before its token is yielded, so a host fire racing
    /// bulk process teardown observes the callback as missing. The iterator
    /// allocates no intermediate token buffer, and dropping it early still
    /// clears every remaining entry through [`HashMap::drain`].
    pub fn drain_tokens(&mut self) -> impl Iterator<Item = u64> + '_ {
        self.entries.drain().map(|(token, _entry)| token)
    }

    /// The kind of every entry still pending, in token order so a host
    /// reporting them gets a stable answer.
    #[must_use]
    pub fn active_kinds(&self) -> Vec<TimerKind> {
        let mut entries: Vec<(u64, TimerKind)> = self
            .entries
            .iter()
            .map(|(token, entry)| (*token, entry.kind))
            .collect();
        entries.sort_unstable_by_key(|(token, _)| *token);
        entries.into_iter().map(|(_, kind)| kind).collect()
    }

    /// Remove and return host tokens owned by a disposed realm.
    pub fn remove_realm(&mut self, realm_id: u32) -> Vec<u64> {
        let mut tokens: Vec<u64> = self
            .entries
            .iter()
            .filter_map(|(token, entry)| (entry.realm_id == realm_id).then_some(*token))
            .collect();
        tokens.sort_unstable();
        for token in &tokens {
            self.entries.remove(token);
        }
        tokens
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
    ("setImmediate", 1, set_immediate_native),
    ("clearImmediate", 1, clear_immediate_native),
    ("__otterTimerSetRef", 2, timer_set_ref_native),
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
    let mut builder = ObjectBuilder::from_object_with_value_roots(
        heap,
        global_this,
        vec![Value::object(global_this)],
    );
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

/// `BuiltinIntrinsic` adapter for the WHATWG timer globals
/// (`setTimeout`, `clearTimeout`, `setInterval`, `clearInterval`).
///
/// The real scheduling — `tokio::time`, libuv, browser event loop —
/// is supplied by the embedder through [`TimerScheduler`] and
/// installed on the runtime before the first script runs. The
/// intrinsic only attaches the four native function entry points on
/// `globalThis`; their bodies delegate every queue / cancel call to
/// the embedder-supplied scheduler.
pub struct Intrinsic;

impl crate::intrinsic_install::BuiltinIntrinsic for Intrinsic {
    /// `setTimeout` is the conventional name used by the bootstrap
    /// registry to address the whole timer family.
    const NAME: &'static str = "setTimeout";
    const FEATURE: crate::bootstrap::BootstrapFeatures = crate::bootstrap::BootstrapFeatures::CORE;

    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_timer_globals(global, heap)
    }
}

fn coerce_delay_ms(value: Option<&Value>, heap: &otter_gc::GcHeap) -> u64 {
    let n = match value {
        Some(v) if let Some(num) = v.as_number() => num.as_f64(),
        Some(v) if v.is_undefined() => 0.0,
        None => 0.0,
        Some(other) => number::parse::to_number_value(other, heap),
    };
    if n.is_nan() || n <= 0.0 {
        0
    } else {
        let clamped = n.min(u64::MAX as f64);
        clamped as u64
    }
}

fn ensure_callable(
    value: &Value,
    heap: &otter_gc::GcHeap,
    native: &'static str,
) -> Result<(), NativeError> {
    // The heapless check cannot see an ordinary object carrying a native
    // `[[Call]]` slot, and `Function.prototype` is one — Node's own cluster
    // uses it as its no-op timer callback.
    if crate::abstract_ops::is_callable_in_heap(value, heap) {
        Ok(())
    } else {
        Err(NativeError::TypeError {
            name: native,
            reason: "callback is not a function".to_string(),
        })
    }
}

fn schedule_timer_entry(
    ctx: &mut NativeCtx<'_>,
    callback: Value,
    delay_ms: u64,
    repeat_ms: Option<u64>,
    extra_args: &[Value],
    kind: TimerKind,
    native_name: &'static str,
) -> Result<u64, NativeError> {
    ensure_callable(&callback, ctx.heap(), native_name)?;
    let scheduler = ctx
        .interp_mut()
        .timer_scheduler()
        .ok_or_else(|| NativeError::TypeError {
            name: native_name,
            reason: "host runtime did not install a timer scheduler".to_string(),
        })?;
    let admission =
        scheduler
            .admit(repeat_ms.is_some())
            .map_err(|reason| NativeError::RangeError {
                name: native_name,
                reason,
            })?;
    let extra_args: SmallVec<[Value; 4]> = extra_args.iter().cloned().collect();
    let context = ctx
        .execution_context()
        .ok_or_else(|| NativeError::TypeError {
            name: native_name,
            reason: "timer callback is missing its execution context".to_string(),
        })?
        .clone();
    let interp = ctx.interp_mut();
    let async_context = interp.async_context();
    let token = scheduler
        .schedule(admission, delay_ms, repeat_ms)
        .map_err(|reason| NativeError::RangeError {
            name: native_name,
            reason,
        })?;
    interp.record_runtime_host_op_enqueued();
    let realm_id = interp.active_host_realm_id();
    interp.timer_callbacks_mut().insert(
        token,
        TimerEntry {
            realm_id,
            callback,
            extra_args,
            context,
            repeat_ms,
            kind,
            async_context,
        },
    );
    Ok(token)
}

/// Schedule a rooted internal interval without consulting mutable JavaScript
/// globals. This deliberately shares admission, host arming, and callback-table
/// registration with the public timer builtins.
pub(crate) fn schedule_interval_rooted(
    ctx: &mut NativeCtx<'_>,
    callback: Value,
    delay_ms: u64,
) -> Result<u64, NativeError> {
    schedule_timer_entry(
        ctx,
        callback,
        delay_ms,
        Some(delay_ms),
        &[],
        TimerKind::Timeout,
        "NativeScope::schedule_interval",
    )
}

fn schedule_timer_common(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    repeat: bool,
    native_name: &'static str,
) -> Result<Value, NativeError> {
    let callback = args.first().cloned().unwrap_or(Value::undefined());
    let delay_ms = coerce_delay_ms(args.get(1), ctx.heap());
    let token = schedule_timer_entry(
        ctx,
        callback,
        delay_ms,
        repeat.then_some(delay_ms),
        args.get(2..).unwrap_or(&[]),
        TimerKind::Timeout,
        native_name,
    )?;
    Ok(Value::number_f64(token as f64))
}

fn cancel_timer_common(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    _native_name: &'static str,
) -> Result<Value, NativeError> {
    let token = match args.first().and_then(|v| v.as_number()) {
        Some(n) => {
            let raw = n.as_f64();
            if raw.is_finite() && raw >= 0.0 {
                raw as u64
            } else {
                return Ok(Value::undefined());
            }
        }
        None => return Ok(Value::undefined()),
    };
    let interp = ctx.interp_mut();
    let _ = interp.cancel_timer(token);
    Ok(Value::undefined())
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

/// `setImmediate(callback, ...args)` — schedule a zero-delay one-shot timer.
/// Unlike `setTimeout`, the first argument after the callback is already an
/// extra callback argument (there is no delay parameter).
fn set_immediate_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let callback = args.first().cloned().unwrap_or(Value::undefined());
    let token = schedule_timer_entry(
        ctx,
        callback,
        0,
        None,
        args.get(1..).unwrap_or(&[]),
        TimerKind::Immediate,
        "setImmediate",
    )?;
    Ok(Value::number_f64(token as f64))
}

fn clear_immediate_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    cancel_timer_common(ctx, args, "clearImmediate")
}

/// `__otterTimerSetRef(token, refed)` — move a pending timer between the
/// ref/unref liveness classes on the host scheduler. Node's `Timeout`
/// wrapper drives this from `ref()`/`unref()`.
fn timer_set_ref_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let token = match args.first().and_then(|v| v.as_number()) {
        Some(n) => {
            let raw = n.as_f64();
            if raw.is_finite() && raw >= 0.0 {
                raw as u64
            } else {
                return Ok(Value::boolean(false));
            }
        }
        None => return Ok(Value::boolean(false)),
    };
    let refed = args.get(1).is_none_or(|value| value.to_boolean(ctx.heap()));
    let moved = ctx
        .interp_mut()
        .timer_scheduler()
        .is_some_and(|scheduler| scheduler.set_ref(token, refed));
    Ok(Value::boolean(moved))
}
