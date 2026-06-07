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
//!   microtask queue, dynamic-import registry, module error
//!   cache, symbol registry, error-class registry,
//!   function-user-prop bag.
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
    /// - dynamic-import registry and module evaluation error cache;
    /// - symbol registry + well-known symbols;
    /// - error-class registry;
    /// - function-user-prop bag;
    /// - pending generator / uncaught throw side-channels.
    pub fn trace_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        self.trace_roots_inner(visitor, true);
    }

    pub(crate) fn trace_roots_without_shape_runtime(&self, visitor: &mut GcRootVisitor<'_>) {
        self.trace_roots_inner(visitor, false);
    }

    fn trace_roots_inner(&self, visitor: &mut GcRootVisitor<'_>, include_shape_runtime: bool) {
        let interp = self.interp;
        // 1) Shared globalThis.
        interp.global_this().trace_gc_roots(visitor);
        // 1b) Cached realm intrinsic prototypes.
        interp.realm_intrinsics().trace_roots(visitor);
        // 2) Module environments.
        for env in interp.module_environments_for_trace() {
            env.trace_gc_roots(visitor);
        }
        // 2b) Persistent module-init upvalue cells (module
        // environment records shared between link and eval phases).
        for spine in interp.module_init_upvalues_for_trace() {
            for slot in spine.iter() {
                let p = slot as *const crate::UpvalueCell as *mut otter_gc::raw::RawGc;
                visitor(p);
            }
        }
        // 2b-ter) Template-object realm cache (§13.2.8.4).
        for value in interp.template_objects_for_trace() {
            value.trace_value_slots(visitor);
        }
        // 2c) Global declarative-record cells (§9.1.1.4 script
        // top-level lexical bindings).
        for slot in interp.global_lexicals_for_trace() {
            let p = slot as *const crate::UpvalueCell as *mut otter_gc::raw::RawGc;
            visitor(p);
        }
        for ns in interp.module_namespaces_for_trace() {
            ns.trace_gc_roots(visitor);
        }
        for value in interp.module_errors_for_trace() {
            value.trace_value_slots(visitor);
        }
        for promise in interp.module_async_init_promises_for_trace() {
            promise.trace_value_slots(visitor);
        }
        // 3) Microtask queue.
        interp.microtasks().trace_gc_roots(visitor);
        // 3b) Timer callbacks waiting on host-side fire.
        interp.timer_callbacks().trace_gc_roots(visitor);
        // 3c) Pending dynamic-import promises waiting on host load.
        interp.dynamic_import_registry().trace_gc_roots(visitor);
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
        for obj in interp.iterator_prototypes_for_trace() {
            obj.trace_gc_roots(visitor);
        }
        interp.trace_function_kind_roots(visitor);
        // 6b) Prototype overrides for non-GC exotic payloads.
        for value in interp.non_gc_exotic_prototype_overrides_for_trace() {
            value.trace_value_slots(visitor);
        }
        // 7) GC-managed hidden-class root/key/transition side tables.
        if include_shape_runtime {
            interp.shape_runtime_for_trace().trace_roots(visitor);
        }
        // 7b) Store-property ICs can retain cached GC shape transitions.
        for ic in interp.store_property_ics_for_trace() {
            ic.trace_roots(visitor);
        }
        // 8) Pending throw side-channels. Phase 1 holds them as
        //    `Value` (Rc-shared); the trace body in
        //    `Value::trace_gc_roots` lands with task 76 alongside
        //    the first real `Gc<…>`-bearing variant.
        if interp.pending_generator_throw_for_trace().is_some() {
            // No-op stub.
        }
        if interp.pending_uncaught_throw_for_trace().is_some() {
            // No-op stub.
        }
        // 8b) Iteration-anchor stack — handles for in-flight
        //     iterator drains live here so a GC triggered inside a
        //     user `next` body cannot reclaim them. See
        //     [`Interpreter::push_iteration_anchor`].
        for value in interp.iteration_anchors_for_trace() {
            value.trace_value_slots(visitor);
        }
        // 9) Active call frames are NOT enumerated here. The
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
