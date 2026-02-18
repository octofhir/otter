//! GC root tracking for active timer callbacks.
//!
//! Timer callbacks capture JS `Value`s (containing `GcRef<Closure>`) inside
//! opaque `Box<dyn FnOnce()>` / `Arc<dyn Fn()>` closures stored in the event loop.
//! Those closures are invisible to `collect_gc_roots`, which can allow the GC
//! to collect a `Closure` while the timer still holds a raw pointer to it —
//! causing a use-after-free when the timer fires.
//!
//! `TimerCallbackRoots` keeps the callback `Value`s alive as explicitly tracked
//! GC roots for the duration of each active timer. Entries are removed when a
//! timer fires (`setTimeout`, `setImmediate`) or is cancelled
//! (`clearTimeout`, `clearInterval`, `clearImmediate`).

use std::collections::HashMap;
use std::sync::Arc;

use otter_vm_core::value::Value;
use parking_lot::Mutex;

struct TimerEntry {
    callback: Value,
    extra_args: Vec<Value>,
}

/// Tracks active timer callback `Value`s as GC roots.
///
/// Owned by `EventLoop` and registered with `VmContext` via
/// `register_external_root_set` so that `collect_gc_roots` traces all
/// live timer callbacks each GC cycle.
pub struct TimerCallbackRoots {
    entries: Mutex<HashMap<u64, TimerEntry>>,
}

// SAFETY: `TimerCallbackRoots` is accessed only from the single VM thread.
// `parking_lot::Mutex` provides the interior-mutability needed for shared access
// between timer Op handlers (which insert/remove) and the GC tracer (which reads).
unsafe impl Send for TimerCallbackRoots {}
unsafe impl Sync for TimerCallbackRoots {}

impl TimerCallbackRoots {
    /// Create a new (empty) root set.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(HashMap::new()),
        })
    }

    /// Register a timer callback and its extra arguments as GC roots under `id`.
    ///
    /// Call this immediately after the timer is created (before any JS executes),
    /// using the `u64` numeric timer ID returned by the event loop.
    pub fn register(&self, id: u64, callback: Value, extra_args: Vec<Value>) {
        self.entries.lock().insert(
            id,
            TimerEntry {
                callback,
                extra_args,
            },
        );
    }

    /// Remove `id` from GC roots.
    ///
    /// Call when the timer fires (for once-only timers) or is cancelled.
    pub fn remove(&self, id: u64) {
        self.entries.lock().remove(&id);
    }
}

impl otter_vm_core::context::ExternalRootSet for TimerCallbackRoots {
    fn trace_roots(&self, tracer: &mut dyn FnMut(*const otter_vm_core::gc::GcHeader)) {
        let entries = self.entries.lock();
        for entry in entries.values() {
            entry.callback.trace(tracer);
            for arg in &entry.extra_args {
                arg.trace(tracer);
            }
        }
    }
}
