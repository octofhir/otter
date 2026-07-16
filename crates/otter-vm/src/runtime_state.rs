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
        for realm in interp.extra_realms_for_trace() {
            realm.trace_roots(visitor);
        }
        // 2) Module environments.
        for env in interp.module_environments_for_trace() {
            env.trace_gc_roots(visitor);
        }
        // 2a) Host-installed builtin module namespaces — cached across
        // program runs, so not covered by the per-run registry above.
        for env in interp.host_module_envs_for_trace() {
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
        // Primitive string constants materialized from bytecode constant pools.
        // Immutable strings can be reused across executions, but cached GC
        // handles must move with the heap.
        for value in interp.string_constants_for_trace() {
            value.trace_value_slots(visitor);
        }
        // Cached small-integer decimal strings (`SmallStrings`-style). Immutable
        // shared handles that must move with the heap.
        for value in interp.small_int_strings_for_trace() {
            value.trace_value_slots(visitor);
        }
        // Immutable BigInt constants use the same bytecode-literal cache shape
        // as strings. The cached primitive handle must move with the heap.
        for value in interp.bigint_constants_for_trace() {
            value.trace_value_slots(visitor);
        }
        // Prepared native-loop callbacks cache resolved closure metadata between
        // repeated invocations; every cached slot must move with the heap.
        for root in interp.lean_callback_roots_for_trace() {
            root.trace_slots(visitor);
        }
        // 2b-quater) Native serializer scratch roots (`JSON.stringify`).
        for value in interp.json_root_stack_for_trace() {
            value.trace_value_slots(visitor);
        }
        // 2b-quinquies-pre) Scope-handle arena — native value-building roots.
        // Every collection that can run while a native call holds handles must
        // trace these, or a parked handle goes stale across a move. The
        // snapshot path (`collect_runtime_roots`) also reaches here.
        interp.handle_arena_trace(visitor);
        // 2b-quinquies) Host-resource persistent roots.
        interp.persistent_roots_for_trace().trace_gc_roots(visitor);
        // 2c) Global declarative-record cells (§9.1.1.4 script
        // top-level lexical bindings).
        for slot in interp.global_lexicals_for_trace() {
            let p = slot as *const crate::UpvalueCell as *mut otter_gc::raw::RawGc;
            visitor(p);
        }
        for slot in interp.global_lexical_load_ic_for_trace() {
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
        for value in interp.function_prototype_overrides_for_trace() {
            value.trace_value_slots(visitor);
        }
        interp.trace_iterator_prototypes(visitor);
        interp.trace_function_kind_roots(visitor);
        // 6b) Prototype overrides for non-GC exotic payloads.
        for value in interp.non_gc_exotic_prototype_overrides_for_trace() {
            value.trace_value_slots(visitor);
        }
        for obj in interp.non_gc_exotic_user_props_for_trace() {
            obj.trace_gc_roots(visitor);
        }
        // 7) GC-managed hidden-class root/key/transition side tables.
        if include_shape_runtime {
            interp.shape_runtime_for_trace().trace_roots(visitor);
        }
        for shape in interp.simple_constructor_shapes_for_trace() {
            let p = shape as *const crate::object::ShapeHandle as *mut otter_gc::raw::RawGc;
            visitor(p);
        }
        // 7b) Store-property ICs can retain cached GC shape transitions.
        for ic in interp.store_property_ics_for_trace() {
            ic.trace_roots(visitor);
        }
        // 8) Pending throw side-channels retain arbitrary JS values. They are
        //    roots even while no frame or job queue references the thrown
        //    value, and a moving collection must rewrite their slots in place.
        if let Some(value) = interp.pending_generator_throw_for_trace() {
            value.trace_value_slots(visitor);
        }
        if let Some(value) = interp.pending_uncaught_throw_for_trace() {
            value.trace_value_slots(visitor);
        }
        // 8b) Iteration-anchor stack — handles for in-flight
        //     iterator drains live here so a GC triggered inside a
        //     user `next` body cannot reclaim them. See
        //     [`Interpreter::push_iteration_anchor`].
        for value in interp.iteration_anchors_for_trace() {
            value.trace_value_slots(visitor);
        }
        // 8c) Promise-rejection tracker: rejected promises awaiting the
        //     unhandled-rejection checkpoint are roots until reported.
        interp.rejection_tracker_for_trace().trace(visitor);
        // 9) Active call frames are NOT enumerated here. The
        //    frame stack lives on the call stack of
        //    `Interpreter::run_inner` (`ActivationStack`),
        //    not on the [`Interpreter`] struct itself, so an
        //    out-of-band GC triggered through `RuntimeState`
        //    has no frame stack to walk. When the interpreter
        //    starts triggering GCs from inside alloc paths
        //    (task 76+), it will pass an additional
        //    `external_visit` closure to
        //    `GcHeap::collect_full` that walks the live
        //    `&mut ActivationStack` directly. This
        //    `RuntimeState` walker stays as-is.
        // 9b) The flat JIT register stack DOES live on the `Interpreter`
        //    struct, so its in-flight callee windows are traced here.
        interp.trace_reg_stack(visitor);
        // 9c) Native JIT contexts keep SELF/`this` in ABI scalar fields on the
        // native stack. Their VM-owned descriptors expose those precise slots.
        interp.trace_native_jit_activations(visitor);
    }
}
