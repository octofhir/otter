//! Central GC root walker.
//!
//! [`RuntimeState`] is a thin view over an [`Interpreter`] that
//! exposes a single entry point — [`RuntimeState::trace_roots`]
//! — for [`otter_gc::GcHeap::collect_full`] to consume during a
//! full GC pause.
//!
//! Phase 1 is the scaffolding pass: every type that will move
//! to a `Gc<…>` slot in tasks 76–83 already implements
//! [`crate::gc_trace::GcTrace`] (with empty bodies today).
//! [`Self::trace_roots`] enumerates the right set of roots
//! against those stubs. As each migration task lands, the
//! stub bodies fill in and this walker starts visiting real
//! slots — **without any change to the wiring**.
//!
//! # Contents
//!
//! - [`RuntimeState`] — borrowed view over an
//!   [`crate::Interpreter`].
//! - [`RuntimeState::trace_roots`] — root-set enumeration.
//!
//! # Invariants
//!
//! - The walker must not allocate against the GC heap.
//! - The walker runs under the same STW pause that the GC
//!   relies on.
//! - The walker visits every root listed in §4.2 of the GC
//!   architecture plan: globals, intrinsics, module envs,
//!   active call frames, parked async / generator frames,
//!   microtask queue, dynamic-import host (deferred), symbol
//!   registry, error-class registry, function-user-prop bag.
//!
//! # See also
//!
//! - GC architecture plan §4.2 (root sources), §4.3
//!   (pseudocode).
//! - Task 75 — root enumeration.

use crate::Interpreter;
use crate::gc_trace::{GcRootVisitor, GcTrace};

/// Borrowed view over an [`Interpreter`] used by the GC root
/// walker. Holds no state of its own; the lifetime is tied to
/// the borrow handed to [`Self::new`].
pub struct RuntimeState<'a> {
    interp: &'a Interpreter,
}

impl<'a> RuntimeState<'a> {
    /// Build a walker view over `interp`. Construction is free —
    /// `RuntimeState` only holds a reference.
    #[must_use]
    pub fn new(interp: &'a Interpreter) -> Self {
        Self { interp }
    }

    /// Walk every strong root the GC must see, yielding slot
    /// pointers via `visitor`.
    ///
    /// Per §4.2, roots are:
    /// - shared `globalThis` object;
    /// - intrinsics (Phase 1: not yet a distinct root —
    ///   tracked as a stub for future migrations);
    /// - module-environment registry;
    /// - active call frames (locals + register window +
    ///   accumulator + `this` + bytecode-module reference);
    /// - parked async / generator frames in promise reactions
    ///   (Phase 1: covered by the per-frame stub);
    /// - microtask queue;
    /// - dynamic-import host (filed as a runtime-side TODO,
    ///   `module_loader::DYNAMIC_IMPORT_HOST` does not yet
    ///   exist);
    /// - symbol registry + well-known symbols;
    /// - error-class registry;
    /// - function-user-prop bag;
    /// - pending generator throw side-channel.
    pub fn trace_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        let interp = self.interp;
        // 1) Shared globalThis.
        interp.global_this().trace_gc_roots(visitor);
        // 2) Module environments.
        for env in interp.module_environments_for_trace() {
            env.trace_gc_roots(visitor);
        }
        // 3) Microtask queue.
        interp.microtasks().trace_gc_roots(visitor);
        // 4) Symbol registry + well-known table.
        interp.symbol_registry_for_trace().trace_gc_roots(visitor);
        interp
            .well_known_symbols_for_trace()
            .trace_gc_roots(visitor);
        // 5) Error-class registry.
        interp.error_classes_for_trace().trace_gc_roots(visitor);
        // 6) Function user-property bag.
        for obj in interp.function_user_props_for_trace() {
            obj.trace_gc_roots(visitor);
        }
        // 7) Pending generator throw side-channel — see
        //    `Interpreter::pending_generator_throw`. Phase 1
        //    holds it as a `Value` (Rc-shared); the trace body
        //    in `Value::trace_gc_roots` lands with task 76
        //    alongside the first real `Gc<…>`-bearing variant.
        if interp.pending_generator_throw_for_trace().is_some() {
            // No-op stub.
        }
        // 8) Active call frames are NOT enumerated here. The
        //    frame stack lives on the call stack of
        //    `Interpreter::run_inner` (`SmallVec<[Frame; 8]>`),
        //    not on the [`Interpreter`] struct itself, so an
        //    out-of-band GC triggered through `RuntimeState`
        //    has no frame stack to walk. When the interpreter
        //    starts triggering GCs from inside alloc paths
        //    (task 76+), it will pass an additional
        //    `external_visit` closure to
        //    `GcHeap::collect_full` that walks the live
        //    `&mut SmallVec<[Frame; 8]>` directly. This
        //    `RuntimeState` walker stays as-is.
    }
}
