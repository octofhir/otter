//! GC root enumeration surface for interpreter-owned state.
//!
//! # Contents
//! `*_for_trace` iterators over module envs/namespaces, global lexicals,
//! function expando props, iterator prototypes, plus iteration anchors
//! and module-root slot stacks.
//!
//! # Invariants
//! Every collection reachable from JS values owned by the interpreter
//! must be enumerated here or in [`crate::ActivationStack::trace_roots`];
//! missing a source is a use-after-free under GC stress.
#![allow(unused_imports)]
use crate::*;

impl Interpreter {
    /// Iterator over every `module_env` object in the per-run
    /// module-environment registry. Used by the GC root
    /// walker (task 75) — values are `JsObject`s holding
    /// live module bindings.
    pub fn module_environments_for_trace(&self) -> impl Iterator<Item = &JsObject> {
        self.module_environments.values()
    }

    /// Iterator over cached host-installed builtin module namespaces
    /// (`otter:*` etc.). These outlive `reset_module_state`, so they must be
    /// enumerated as GC roots independently of the per-run module registry.
    pub fn host_module_envs_for_trace(&self) -> impl Iterator<Item = &JsObject> {
        self.host_module_env_cache.values()
    }

    /// Borrow additional host-created realms for GC root tracing.
    pub(crate) fn extra_realms_for_trace(&self) -> impl Iterator<Item = &RealmState> {
        self.extra_realms.iter()
    }

    /// Borrow the persistent module-init upvalue spines for GC root
    /// tracing. The cells back module-scope bindings shared between
    /// the link-phase and evaluation-phase init invocations.
    pub(crate) fn module_init_upvalues_for_trace(
        &self,
    ) -> impl Iterator<Item = &Box<[crate::UpvalueCell]>> {
        self.module_init_upvalues.values()
    }

    /// Global declarative-record cells for the GC root walk.
    pub(crate) fn global_lexicals_for_trace(&self) -> impl Iterator<Item = &crate::UpvalueCell> {
        self.global_lexicals.values().map(|(cell, _)| cell)
    }

    /// Cached global-load cells for the GC root walk. These alias cells already
    /// held by [`Self::global_lexicals`], but a moving collector rewrites each
    /// root slot in place, so the cache copies must be visited too.
    pub(crate) fn global_lexical_load_ic_for_trace(
        &self,
    ) -> impl Iterator<Item = &crate::UpvalueCell> {
        self.global_lexical_load_ic.values()
    }

    /// Borrow cached eager + deferred module namespace exotic objects
    /// for GC root tracing. They are reachable from JS via `import * as
    /// ns`, so they must survive collection even when no live register
    /// currently holds them.
    pub fn module_namespaces_for_trace(&self) -> impl Iterator<Item = &JsObject> {
        self.module_namespaces
            .values()
            .chain(self.deferred_namespaces.values())
    }

    /// Borrow cached module-evaluation thrown values for GC root tracing.
    pub fn module_errors_for_trace(&self) -> impl Iterator<Item = &Value> {
        self.module_records
            .values()
            .filter_map(|record| record.evaluation_error.as_ref())
    }

    /// Borrow per-module evaluation gate promises for GC root tracing.
    pub(crate) fn module_async_init_promises_for_trace(
        &self,
    ) -> impl Iterator<Item = &crate::promise::JsPromiseHandle> {
        self.module_records
            .values()
            .filter_map(|record| record.evaluation_promise.as_ref())
    }

    /// Borrow the well-known symbol singleton table. Used by
    /// the GC root walker (task 75).
    #[must_use]
    pub fn well_known_symbols_for_trace(&self) -> &WellKnownSymbols {
        &self.well_known_symbols
    }

    /// Borrow the error-class registry. Used by the GC root
    /// walker (task 75); embedder-facing reads should prefer
    /// [`Self::error_classes_clone`].
    #[must_use]
    pub fn error_classes_for_trace(&self) -> &ErrorClassRegistry {
        &self.error_classes
    }

    /// Borrow the symbol registry. Used by the GC root walker
    /// (task 75); see also [`Self::symbol_registry`] which is
    /// the older spelling kept for back-compat.
    #[must_use]
    pub fn symbol_registry_for_trace(&self) -> &SymbolRegistry {
        &self.symbol_registry
    }

    /// Iterator over every per-function user-property bag.
    /// Used by the GC root walker (task 75) — each value is a
    /// `JsObject` carrying user-side `f.foo = bar` writes.
    pub fn function_user_props_for_trace(&self) -> impl Iterator<Item = &JsObject> {
        self.function_user_props.values()
    }

    /// Iterator over ordinary-function `[[Prototype]]` override
    /// values. Used by the GC root walker because subclassed
    /// dynamic functions can retain user-created prototype objects.
    pub fn function_prototype_overrides_for_trace(&self) -> impl Iterator<Item = &Value> {
        self.function_prototype_overrides.values()
    }

    pub(crate) fn set_function_prototype_override(&mut self, value: &Value, proto: Option<Value>) {
        let function_id = value.as_function().or_else(|| {
            value
                .as_closure(&self.gc_heap)
                .map(|closure| closure.cached_function_id)
        });
        let Some(function_id) = function_id else {
            return;
        };
        if let Some(proto) = proto {
            self.function_prototype_overrides.insert(function_id, proto);
        } else {
            self.function_prototype_overrides.remove(&function_id);
        }
    }

    /// Trace cached per-kind iterator prototypes, including the never-swapped
    /// default-realm copies, through legal interior-mutable root cells.
    pub fn trace_iterator_prototypes(&self, visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)) {
        let trace = |cell: &crate::gc_trace::RootCell<Option<JsObject>>,
                     visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            // SAFETY: `RootCell` owns a stable slot inside `Interpreter`; root
            // tracing is STW and fallback paths may retain this pointer until
            // the collection starts immediately after the walk.
            if let Some(object) = unsafe { (&mut *cell.as_mut_ptr()).as_mut() } {
                visitor((object as *mut JsObject).cast::<otter_gc::raw::RawGc>());
            }
        };
        for cell in [
            &self.array_iterator_prototype,
            &self.map_iterator_prototype,
            &self.set_iterator_prototype,
            &self.string_iterator_prototype,
            &self.regexp_string_iterator_prototype,
            &self.iterator_helper_prototype,
            &self.wrap_for_valid_iterator_prototype,
        ] {
            trace(cell, visitor);
        }
        for cell in &self.default_realm_iterator_prototypes {
            trace(cell, visitor);
        }
    }

    /// Iterator over non-GC exotic prototype override values.
    /// Used by the GC root walker because the side table can retain
    /// subclass prototype objects for `ArrayBuffer`, `DataView`, and
    /// `TypedArray` instances.
    pub fn non_gc_exotic_prototype_overrides_for_trace(&self) -> impl Iterator<Item = &Value> {
        self.non_gc_exotic_prototype_overrides.values()
    }

    /// Iterator over non-GC exotic own-property bags.
    pub fn non_gc_exotic_user_props_for_trace(&self) -> impl Iterator<Item = &JsObject> {
        self.non_gc_exotic_user_props.values()
    }

    /// Borrow the GC-managed shape side tables for root tracing.
    #[must_use]
    pub(crate) fn shape_runtime_for_trace(&self) -> &object::ShapeRuntime {
        &self.shape_runtime
    }

    /// Borrow cached simple-constructor final shapes for root tracing.
    pub(crate) fn simple_constructor_shapes_for_trace(
        &self,
    ) -> impl Iterator<Item = &object::ShapeHandle> {
        self.simple_constructor_shape_cache.values()
    }

    /// Borrow store-property ICs for root tracing of cached GC shape handles.
    pub(crate) fn store_property_ics_for_trace(
        &self,
    ) -> &[property_ic::PropertyIcEntry<cache_ir::CacheStub>] {
        self.feedback_directory.store_ics_for_trace()
    }
}

impl Interpreter {
    /// Borrow the pending-generator-throw side-channel slot.
    /// The GC root walker traces and rewrites the contained value.
    #[must_use]
    pub fn pending_generator_throw_for_trace(&self) -> Option<&Value> {
        self.pending_generator_throw.as_ref()
    }

    /// Borrow the pending uncaught throw side-channel slot for GC
    /// root tracing.
    #[must_use]
    pub fn pending_uncaught_throw_for_trace(&self) -> Option<&Value> {
        self.pending_uncaught_throw.as_ref()
    }

    /// Borrow the iteration-anchor stack for GC root tracing.
    #[must_use]
    pub(crate) fn iteration_anchors_for_trace(&self) -> &[Value] {
        &self.iteration_anchors
    }

    /// Borrow the promise-rejection tracker for GC root tracing. Its two
    /// handle lists retain rejected promises until the checkpoint reports them.
    #[must_use]
    pub(crate) fn rejection_tracker_for_trace(
        &self,
    ) -> &crate::promise_rejection::RejectionTracker {
        &self.rejection_tracker
    }

    /// Push a value onto the iteration-anchor stack. Returns the
    /// new stack depth so the matching pop can sanity-check.
    pub(crate) fn push_iteration_anchor(&mut self, value: Value) -> usize {
        self.iteration_anchors.push(value);
        self.iteration_anchors.len()
    }

    /// Pop entries back down to the depth captured at push time.
    pub(crate) fn pop_iteration_anchors_to(&mut self, depth: usize) {
        self.iteration_anchors.truncate(depth);
    }

    /// Overwrite an existing iteration-anchor slot. Used by loops that
    /// carry a *mutating* rooted value (an accumulator, the current
    /// element) across a reentrant callback: the slot is refreshed
    /// before each callback so a moving scavenge rewrites the live
    /// value, and read back afterwards via [`Self::iteration_anchor`].
    pub(crate) fn set_iteration_anchor(&mut self, index: usize, value: Value) {
        self.iteration_anchors[index] = value;
    }

    /// Read an iteration-anchor slot back after a reentrant callback —
    /// a moving scavenge rewrites the slot in place, so this returns the
    /// relocated handle.
    #[must_use]
    pub(crate) fn iteration_anchor(&self, index: usize) -> Value {
        self.iteration_anchors[index]
    }

    /// Root a value for the duration of an out-of-crate builder (e.g. the
    /// runtime's module `ModuleScope`). Backed by the iteration-anchor stack,
    /// which the GC traces and rewrites in place, so the rooted value survives a
    /// moving scavenge triggered by a later allocation. Returns the new depth;
    /// pass it (minus one) to read the value back via [`Self::module_root`].
    ///
    /// Because the GC moves, a `Value` copy held across an allocation is stale —
    /// re-read it with [`Self::module_root`] after any allocation, and balance
    /// every push with [`Self::pop_module_roots_to`].
    pub fn push_module_root(&mut self, value: Value) -> usize {
        self.push_iteration_anchor(value)
    }

    /// Current module-root stack depth. Capture before a build and pass to
    /// [`Self::pop_module_roots_to`] to release everything pushed since.
    #[must_use]
    pub fn module_root_depth(&self) -> usize {
        self.iteration_anchors.len()
    }

    /// Pop module roots back to a depth previously returned by
    /// [`Self::push_module_root`] / [`Self::module_root_depth`]. Must be called
    /// to balance the pushes.
    pub fn pop_module_roots_to(&mut self, depth: usize) {
        self.pop_iteration_anchors_to(depth);
    }

    /// Read a module-root slot back after an allocation/reentry — the moving GC
    /// rewrites the slot in place, so this returns the relocated handle.
    #[must_use]
    pub fn module_root(&self, index: usize) -> Value {
        self.iteration_anchor(index)
    }

    /// Overwrite a module-root slot (for a value mutated across allocations).
    pub fn set_module_root(&mut self, index: usize, value: Value) {
        self.set_iteration_anchor(index, value);
    }

    /// Consume the pending uncaught-throw payload, if any. Embedder
    /// callers that catch a `VmError::Uncaught` at a sync entry
    /// point use this to recover the original thrown
    /// [`Value`] (an `Error` instance, a string, etc.) instead of
    /// the lossy `Display` rendering carried by the `VmError`.
    pub fn take_pending_uncaught_throw(&mut self) -> Option<Value> {
        self.pending_uncaught_throw.take()
    }

    /// Stash a [`Value`] on the pending-uncaught-throw side channel
    /// so the surrounding microtask drain / sync entry point can
    /// surface the original [[Value]] verbatim after the native
    /// returns [`NativeError::Thrown`]. The pairing with
    /// `NativeError::Thrown` (which carries only a display rendering)
    /// preserves identity per §27.2.1.3.2 step 1.f.iii for natives
    /// that need to re-throw a JS value verbatim — such as the
    /// `thrower` function CreateCatchFinally(C, onFinally) installs.
    pub(crate) fn set_pending_uncaught_throw(&mut self, value: Value) {
        self.pending_uncaught_throw = Some(value);
    }
}
