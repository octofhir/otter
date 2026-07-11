//! `Interpreter` construction and introspection accessors.
//!
//! # Contents
//! - `new` / `with_string_heap_cap` bootstrap the heap, realm surfaces,
//!   shape runtime, caches, and JIT state.
//! - Introspection accessors expose property-IC and JIT counters.
//!
//! # Invariants
//! - Every partially built GC graph is owned by an RAII root scope until its
//!   fields move into the completed interpreter.
//! - Root providers are dropped before their stack slots move into the
//!   interpreter, and no GC allocation occurs during that move.
//! - A half-built interpreter is never observable outside this module.
#![allow(unused_imports)]
use crate::rooting::RootScopeExt;
use crate::*;

unsafe fn trace_bootstrap_well_known_symbols(
    slot: *mut (),
    visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc),
) {
    // SAFETY: the matching scope in `with_string_heap_cap` is nested inside
    // the table's lifetime and is dropped before the table moves into the
    // completed interpreter.
    let table = unsafe { &*slot.cast::<WellKnownSymbols>() };
    for symbol in table.entries() {
        symbol.trace_value_slots(visitor);
    }
}

unsafe fn trace_bootstrap_error_classes(
    slot: *mut (),
    visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc),
) {
    // SAFETY: same construction-scope contract as the well-known table above.
    let registry = unsafe { &*slot.cast::<ErrorClassRegistry>() };
    registry.trace_gc_roots(visitor);
}

impl Interpreter {
    /// Construct a fresh interpreter with its own interrupt flag,
    /// a no-cap string heap, the default stack-depth limit, and a
    /// fresh GC heap.
    #[must_use]
    pub fn new() -> Self {
        Self::with_string_heap_cap(0)
    }

    /// Construct an interpreter with a string heap cap (`0` =
    /// unlimited). The same cap is honoured by the interpreter's
    /// GC heap.
    #[must_use]
    pub fn with_string_heap_cap(cap_bytes: u64) -> Self {
        let startup_timer = StartupPhaseTimer::from_env();
        let mut gc_heap = otter_gc::GcHeap::with_max_heap_bytes(cap_bytes)
            .expect("GcHeap construction never fails on the default cage");
        object::register_gc_traceables(&mut gc_heap);
        startup_timer.mark("vm_gc_heap");
        let mut well_known_symbols = WellKnownSymbols::new(&mut gc_heap)
            .expect("well-known symbol descriptions + bodies fit within any positive cap");
        let mut well_known_scope = otter_gc::RootScope::new(&mut gc_heap);
        // SAFETY: the table precedes the scope and the scope is explicitly
        // dropped before the table moves into `Interpreter`.
        unsafe {
            well_known_scope.add_erased(
                (&mut well_known_symbols as *mut WellKnownSymbols).cast::<()>(),
                trace_bootstrap_well_known_symbols,
            );
        }
        startup_timer.mark("vm_well_known_symbols");
        let mut error_classes = ErrorClassRegistry::new(&mut gc_heap)
            .expect("error class prototypes fit within any positive cap");
        let mut error_scope = otter_gc::RootScope::new(&mut gc_heap);
        // SAFETY: the registry precedes the scope and remains stationary until
        // the scope is dropped immediately before the struct move.
        unsafe {
            error_scope.add_erased(
                (&mut error_classes as *mut ErrorClassRegistry).cast::<()>(),
                trace_bootstrap_error_classes,
            );
        }
        startup_timer.mark("vm_error_classes");
        let mut global_this = bootstrap::build_global_this(&mut gc_heap, &well_known_symbols)
            .expect("global_this fits within any positive cap");
        let mut global_scope = otter_gc::RootScope::new(&mut gc_heap);
        // SAFETY: the global handle precedes the scope and stays stationary
        // until the explicit drop below.
        unsafe { crate::rooting::RootScopeExt::add_object(&mut global_scope, &mut global_this) };
        startup_timer.mark("vm_global_this");
        // §20.4.2 — install well-known symbols on the realm's
        // `Symbol` constructor + `Symbol.prototype[@@toPrimitive]`.
        // Bootstrap allocates the ctor + prototype objects; this
        // hook attaches the per-realm singleton symbols once
        // `WellKnownSymbols` exists.
        crate::intrinsics::symbol::install_symbol_well_knowns_post_bootstrap(
            &mut gc_heap,
            global_this,
            &well_known_symbols,
        )
        .expect("Symbol well-known properties fit within any positive cap");
        // §20.2.3.6 — install `Function.prototype[@@hasInstance]`.
        // Bootstrap can't see `WellKnownSymbols`, so we wire the
        // realm-local @@hasInstance after both Function.prototype
        // and the symbol table exist.
        let function_prototype_handle = if let Some(function_proto) =
            resolve_ctor_prototype(&mut gc_heap, global_this, "Function")
        {
            let has_instance = well_known_symbols.get(symbol::WellKnown::HasInstance);
            let global_root = Value::object(global_this);
            function_prototype::install_symbol_has_instance(
                &mut gc_heap,
                function_proto,
                has_instance,
                &[&global_root],
            )
            .expect("Function.prototype[@@hasInstance] fits within any positive cap");
            // The installer can allocate and relocate the young prototype.
            // Resolve it again through the rooted global graph instead of
            // returning the pre-allocation copy.
            resolve_ctor_prototype(&mut gc_heap, global_this, "Function")
        } else {
            None
        };
        // §20.5.6 — finalize the native error class hierarchy now
        // that `%Function.prototype%` and `%Object.prototype%` are
        // installed: link constructor and prototype `[[Prototype]]`
        // chains and surface every error constructor on `globalThis`
        // as a writable, non-enumerable, configurable data property.
        if let Some(function_prototype) = function_prototype_handle
            && let Some(object_prototype) =
                resolve_ctor_prototype(&mut gc_heap, global_this, "Object")
        {
            error_classes.finalize_after_bootstrap(
                &mut gc_heap,
                function_prototype,
                object_prototype,
                global_this,
            );
        }
        let shape_runtime = object::ShapeRuntime::new(&mut gc_heap)
            .expect("shape root fits within any positive cap");
        startup_timer.mark("vm_shape_runtime");
        // No GC allocation occurs between these drops and the struct move.
        // Dropping in reverse registration order keeps the frame-root stack
        // strictly LIFO and prevents providers from pointing at moved-from
        // stack values after the fields enter `Interpreter`.
        drop(global_scope);
        drop(error_scope);
        drop(well_known_scope);
        let mut interp = Self {
            template_objects: rustc_hash::FxHashMap::default(),
            string_constant_cache: rustc_hash::FxHashMap::default(),
            small_int_string_cache: vec![None; Self::SMALL_INT_STRING_CACHE as usize]
                .into_boxed_slice(),
            bigint_constant_cache: rustc_hash::FxHashMap::default(),
            lean_callback_roots: Vec::new(),
            pending_error_detail: std::cell::RefCell::new(None),
            json_root_stack: Vec::new(),
            handle_arena: crate::handles::HandleArena::new(),
            json_stringify_capacity_hint: 0,
            array_index_accessor_protector: false,
            interrupt: InterruptFlag::new(),
            jit_backedge_fuel: Self::JIT_BACKEDGE_POLL_BATCH,
            current_function_id: 0,
            current_instruction_pc: 0,
            gc_heap,
            code_space: std::sync::Arc::new(code_space::CodeSpace::default()),
            realm_context: None,
            shape_runtime,
            simple_constructor_init_cache: rustc_hash::FxHashMap::default(),
            simple_constructor_shape_cache: rustc_hash::FxHashMap::default(),
            max_stack_depth: DEFAULT_MAX_STACK_DEPTH,
            sync_reentry_depth: 0,
            allow_blocking_atomics_wait: false,
            microtasks: MicrotaskQueue::new(),
            module_environments: std::collections::HashMap::new(),
            host_module_env_cache: std::collections::HashMap::new(),
            module_init_upvalues: std::collections::HashMap::new(),
            global_lexicals: rustc_hash::FxHashMap::default(),
            global_lexical_load_ic: rustc_hash::FxHashMap::default(),
            module_hoisted: std::collections::HashSet::new(),
            module_evaluation_depth: 0,
            module_resolution_cache: std::collections::HashMap::new(),
            module_records: std::collections::HashMap::new(),
            next_module_async_order: 0,
            deferred_namespaces: std::collections::HashMap::new(),
            module_namespaces: std::collections::HashMap::new(),
            module_resolved_exports: std::collections::HashMap::new(),
            load_property_ics: Vec::new(),
            store_property_ics: Vec::new(),
            method_call_ics: Vec::new(),
            jit_collection_method_ics: Vec::new(),
            has_property_ics: Vec::new(),
            property_ic_stats: property_ic::PropertyIcStats::default(),
            jit_hook: None,
            jit_call_counts: rustc_hash::FxHashMap::default(),
            jit_call_site_feedback: rustc_hash::FxHashMap::default(),
            jit_method_site_feedback: rustc_hash::FxHashMap::default(),
            jit_arith_feedback: rustc_hash::FxHashMap::default(),
            jit_element_load_kind: rustc_hash::FxHashMap::default(),
            jit_arith_widen_float: rustc_hash::FxHashSet::default(),
            jit_entry_bail_counts: rustc_hash::FxHashMap::default(),
            jit_entry_reopt_counts: rustc_hash::FxHashMap::default(),
            jit_osr_disabled: rustc_hash::FxHashSet::default(),
            jit_osr_counts: rustc_hash::FxHashMap::default(),
            jit_osr_threshold: std::env::var("OTTER_JIT_OSR_THRESHOLD")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|&t| t > 0)
                .unwrap_or(Self::JIT_OSR_THRESHOLD),
            jit_code: rustc_hash::FxHashMap::default(),
            jit_osr_code: rustc_hash::FxHashMap::default(),
            jit_code_cache: None,
            jit_entry_osr_only: rustc_hash::FxHashSet::default(),
            jit_direct_code_anchors: Vec::new(),
            jit_direct_method_cache: Vec::new(),
            jit_direct_method_inline_slots: Vec::new(),
            jit_runtime_stats: JitRuntimeStats::default(),
            holt_pool: Vec::new(),
            register_stack: register_stack::RegisterStack::new(),
            jit_native_activations: vec![
                jit::JitNativeActivation::EMPTY;
                DEFAULT_MAX_STACK_DEPTH as usize
            ],
            jit_native_activation_top: 0,
            runtime_budget: RuntimeBudget::default(),
            runtime_budget_stats: RuntimeBudgetStats::default(),
            runtime_budget_depth: 0,
            runtime_budget_turn_started_at: None,
            runtime_budget_heap_start: None,
            well_known_symbols,
            symbol_registry: SymbolRegistry::new(),
            error_classes,
            global_this,
            extra_realms: Vec::new(),
            active_realm_is_extra: false,
            eval_hook: None,
            pending_generator_throw: None,
            pending_uncaught_throw: None,
            iteration_anchors: Vec::new(),
            rejection_tracker: crate::promise_rejection::RejectionTracker::default(),
            pending_uncaught_frames: None,
            module_sources: source_registry::SourceRegistry::default(),
            active_frame_stack: std::ptr::null(),
            function_user_props: std::collections::HashMap::new(),
            function_prototype_overrides: std::collections::HashMap::new(),
            function_non_extensible: std::collections::HashSet::new(),
            function_deleted_metadata: std::collections::HashSet::new(),
            non_gc_exotic_prototype_overrides: std::collections::HashMap::new(),
            non_gc_exotic_user_props: std::collections::HashMap::new(),
            persistent_roots: persistent_roots::PersistentRoots::new(),
            console_sink: console::default_console_sink(),
            timer_scheduler: None,
            host_completion_sink: None,
            lazy_global_groups: Vec::new(),
            timer_callbacks: timers::TimerCallbacks::new(),
            dynamic_import_loader: None,
            dynamic_import_registry: dynamic_import::DynamicImportRegistry::new(),
            array_iterator_prototype: crate::gc_trace::RootCell::new(None),
            map_iterator_prototype: crate::gc_trace::RootCell::new(None),
            set_iterator_prototype: crate::gc_trace::RootCell::new(None),
            string_iterator_prototype: crate::gc_trace::RootCell::new(None),
            regexp_string_iterator_prototype: crate::gc_trace::RootCell::new(None),
            iterator_helper_prototype: crate::gc_trace::RootCell::new(None),
            wrap_for_valid_iterator_prototype: crate::gc_trace::RootCell::new(None),
            default_realm_iterator_prototypes: std::array::from_fn(|_| {
                crate::gc_trace::RootCell::new(None)
            }),
            function_kind_prototypes: function_kind::FunctionKindPrototypes::default(),
            cold_frames: cold_frame::ColdFramePool::new(),
            realm_intrinsics: realm_intrinsics::RealmIntrinsics::default(),
            regex_compile_cache: regexp::RegexCompileCache::default(),
            tracer: None,
            cpu_profiler: None,
        };
        // Cache typed handles for the well-known constructors and
        // prototypes. Subsequent runtime lookups read the slots and
        // skip the global → ctor → prototype string walk.
        interp
            .realm_intrinsics
            .populate(&mut interp.gc_heap, global_this);
        let extra_roots = otter_gc::ExtraRoots::new(&interp);
        let extra_roots_guard = interp.gc_heap.register_extra_roots(extra_roots);
        let mut iterator_roots = [Value::undefined(); 7];
        let mut iterator_scope = otter_gc::RootScope::new(&mut interp.gc_heap);
        // SAFETY: the fixed array stays in this stack frame until every
        // post-bootstrap allocation completes and the final handles are
        // published into the interpreter's stable root cells below.
        unsafe {
            for value in &mut iterator_roots {
                iterator_scope.add_value(value);
            }
        }
        // §22.1.5 / §23.1.5 / §24.1.5 / §24.2.5 — build the per-kind
        // iterator prototypes once `%Iterator.prototype%` is wired
        // into the global. The bootstrap helper owns the install
        // logic; this site only caches the resulting handles so
        // `intrinsic_prototype_object_for` (iterator family) can
        // route without a global lookup per access.
        if let Ok(iter_proto_value) = interp.constructor_prototype_value("Iterator")
            && let Some(iter_proto) = iter_proto_value.as_object()
        {
            let shape_root = interp.shape_runtime.root();
            let protos =
                crate::intrinsics::iterator::build_builtin_iterator_prototypes_post_bootstrap(
                    &mut interp.gc_heap,
                    shape_root,
                    iter_proto,
                    &interp.well_known_symbols,
                )
                .expect("per-kind iterator prototypes fit within any positive cap");
            let built = [
                Value::object(protos.array),
                Value::object(protos.map),
                Value::object(protos.set),
                Value::object(protos.string),
                Value::object(protos.regexp_string),
                Value::object(protos.helper),
                Value::object(protos.wrap_for_valid_iterator),
            ];
            for (slot, value) in iterator_roots.iter_mut().zip(built) {
                *slot = value;
            }
        }
        interp.install_function_kind_prototypes_post_bootstrap();
        drop(iterator_scope);
        let completed: [Option<JsObject>; 7] = iterator_roots.map(|value| value.as_object());
        for (slot, value) in [
            &interp.array_iterator_prototype,
            &interp.map_iterator_prototype,
            &interp.set_iterator_prototype,
            &interp.string_iterator_prototype,
            &interp.regexp_string_iterator_prototype,
            &interp.iterator_helper_prototype,
            &interp.wrap_for_valid_iterator_prototype,
        ]
        .into_iter()
        .zip(completed)
        {
            slot.set(value);
        }
        // Never-swapped default-realm copies — bare (override-less) iterators
        // resolve against these while an extra realm is active.
        for (slot, value) in interp
            .default_realm_iterator_prototypes
            .iter()
            .zip(completed)
        {
            slot.set(value);
        }
        drop(extra_roots_guard);
        interp
    }

    fn swap_active_realm_state(&mut self, state: &mut RealmState) {
        std::mem::swap(&mut self.global_this, &mut state.global_this);
        std::mem::swap(&mut self.error_classes, &mut state.error_classes);
        std::mem::swap(&mut self.realm_intrinsics, &mut state.realm_intrinsics);
        let swap_root = |cell: &crate::gc_trace::RootCell<Option<JsObject>>,
                         slot: &mut Option<JsObject>| {
            let current = cell.get();
            cell.set(*slot);
            *slot = current;
        };
        swap_root(
            &self.array_iterator_prototype,
            &mut state.array_iterator_prototype,
        );
        swap_root(
            &self.map_iterator_prototype,
            &mut state.map_iterator_prototype,
        );
        swap_root(
            &self.set_iterator_prototype,
            &mut state.set_iterator_prototype,
        );
        swap_root(
            &self.string_iterator_prototype,
            &mut state.string_iterator_prototype,
        );
        swap_root(
            &self.regexp_string_iterator_prototype,
            &mut state.regexp_string_iterator_prototype,
        );
        swap_root(
            &self.iterator_helper_prototype,
            &mut state.iterator_helper_prototype,
        );
        swap_root(
            &self.wrap_for_valid_iterator_prototype,
            &mut state.wrap_for_valid_iterator_prototype,
        );
    }

    fn build_realm_state(&mut self) -> Result<RealmState, VmError> {
        let error_classes = ErrorClassRegistry::new(&mut self.gc_heap).map_err(crate::oom_to_vm)?;
        let global_this = bootstrap::build_global_this(&mut self.gc_heap, &self.well_known_symbols)
            .map_err(|err| {
                self.err_type((format!("createRealm bootstrap failed: {err}")).into())
            })?;
        crate::intrinsics::symbol::install_symbol_well_knowns_post_bootstrap(
            &mut self.gc_heap,
            global_this,
            &self.well_known_symbols,
        )
        .map_err(|err| {
            self.err_type((format!("createRealm Symbol bootstrap failed: {err}")).into())
        })?;
        let function_prototype = resolve_ctor_prototype(&mut self.gc_heap, global_this, "Function");
        if let Some(function_prototype) = function_prototype {
            let has_instance = self
                .well_known_symbols
                .get(crate::symbol::WellKnown::HasInstance);
            let global_root = Value::object(global_this);
            function_prototype::install_symbol_has_instance(
                &mut self.gc_heap,
                function_prototype,
                has_instance,
                &[&global_root],
            )
            .map_err(|err| {
                self.err_type((format!("createRealm Function bootstrap failed: {err}")).into())
            })?;
            if let Some(object_prototype) =
                resolve_ctor_prototype(&mut self.gc_heap, global_this, "Object")
            {
                error_classes.finalize_after_bootstrap(
                    &mut self.gc_heap,
                    function_prototype,
                    object_prototype,
                    global_this,
                );
            }
        }
        let mut realm_intrinsics = realm_intrinsics::RealmIntrinsics::default();
        realm_intrinsics.populate(&mut self.gc_heap, global_this);
        let mut state = RealmState {
            global_this,
            error_classes,
            realm_intrinsics,
            array_iterator_prototype: None,
            map_iterator_prototype: None,
            set_iterator_prototype: None,
            string_iterator_prototype: None,
            regexp_string_iterator_prototype: None,
            iterator_helper_prototype: None,
            wrap_for_valid_iterator_prototype: None,
        };
        if let Some(iter_proto) = object::get(global_this, &self.gc_heap, "Iterator")
            .and_then(|ctor| {
                if let Some(obj) = ctor.as_object() {
                    object::get(obj, &self.gc_heap, "prototype")
                } else if let Some(native) = ctor.as_native_function() {
                    native
                        .own_property_descriptor(&mut self.gc_heap, "prototype")
                        .ok()
                        .flatten()
                        .map(|desc| match desc.kind {
                            object::DescriptorKind::Data { value } => value,
                            object::DescriptorKind::Accessor { .. } => Value::undefined(),
                        })
                } else {
                    None
                }
            })
            .and_then(|value| value.as_object())
        {
            let shape_root = self.shape_runtime.root();
            let protos =
                crate::intrinsics::iterator::build_builtin_iterator_prototypes_post_bootstrap(
                    &mut self.gc_heap,
                    shape_root,
                    iter_proto,
                    &self.well_known_symbols,
                )
                .map_err(|err| {
                    self.err_type((format!("createRealm Iterator bootstrap failed: {err}")).into())
                })?;
            state.array_iterator_prototype = Some(protos.array);
            state.map_iterator_prototype = Some(protos.map);
            state.set_iterator_prototype = Some(protos.set);
            state.string_iterator_prototype = Some(protos.string);
            state.regexp_string_iterator_prototype = Some(protos.regexp_string);
            state.iterator_helper_prototype = Some(protos.helper);
            state.wrap_for_valid_iterator_prototype = Some(protos.wrap_for_valid_iterator);
        }
        self.tag_array_realm_natives(global_this);
        self.tag_iterator_realm_natives(global_this);
        Ok(state)
    }

    fn tag_native_value_realm(&mut self, value: Value, global: JsObject) {
        if let Some(native) = value.as_native_function() {
            native.set_realm_global(&mut self.gc_heap, Some(global));
        } else if let Some(class) = value.as_class_constructor() {
            self.tag_native_value_realm(class.ctor(&self.gc_heap), global);
        } else if let Some(obj) = value.as_object()
            && let Some(native) =
                object::call_native(obj, &self.gc_heap).and_then(|v| v.as_native_function())
        {
            native.set_realm_global(&mut self.gc_heap, Some(global));
        }
    }

    fn native_data_property(&mut self, native: crate::NativeFunction, name: &str) -> Option<Value> {
        native
            .own_property_descriptor(&mut self.gc_heap, name)
            .ok()
            .flatten()
            .and_then(|desc| match desc.kind {
                object::DescriptorKind::Data { value } => Some(value),
                object::DescriptorKind::Accessor { .. } => None,
            })
    }

    pub(crate) fn iterator_prototype_override_for_state(
        &self,
        state: &IteratorState,
    ) -> Option<Value> {
        state
            .builtin_origin()
            .and_then(|origin| self.active_realm_iterator_prototype_for(origin))
            .map(Value::object)
    }

    pub(crate) fn register_iterator_prototype_override(
        &mut self,
        handle: IteratorHandle,
        prototype: Option<Value>,
    ) {
        // Stamp the override only for iterators minted while a
        // non-default realm is active, mirroring the array policy: a
        // bare iterator then uniquely means "default realm", and
        // resolution routes it through the never-swapped default-realm
        // prototype copies. Same-realm iterators (the overwhelming
        // majority: every for-of / spread / apply drain mints one) stay
        // bare, because unconditionally inserting them into
        // `non_gc_exotic_prototype_overrides` grew a GC-traced map by one
        // entry per iterator ever created: entries never die (the key is a
        // reusable header address), every scavenge re-traced the whole map,
        // and v8-v7 raytrace degraded ~200x.
        if !self.active_realm_is_extra {
            return;
        }
        self.set_non_gc_exotic_prototype_override(&Value::iterator(handle), prototype);
    }

    pub(crate) fn current_array_prototype_override(&self) -> Option<Value> {
        self.realm_intrinsics.array_prototype.map(Value::object)
    }

    pub(crate) fn register_array_prototype_override(&mut self, array: crate::array::JsArray) {
        // Stamp the per-instance `[[Prototype]]` only for arrays minted while
        // a non-default realm is active. A default-realm array resolves
        // through the active realm's %Array.prototype% anyway, and stamping
        // it would materialize the exotic sidecar that permanently
        // disqualifies the array from every dense fast path (method ICs,
        // builtin dispatch) — one boxed side-table per array, traced on every
        // scavenge.
        if !self.active_realm_is_extra {
            return;
        }
        let prototype = self.current_array_prototype_override();
        crate::array::set_prototype_override(array, &mut self.gc_heap, prototype);
    }

    fn tag_iterator_realm_natives(&mut self, global: JsObject) {
        let Some(iterator_value) = object::get(global, &self.gc_heap, "Iterator") else {
            return;
        };
        self.tag_native_value_realm(iterator_value, global);
        let prototype = if let Some(native) = iterator_value.as_native_function() {
            self.native_data_property(native, "prototype")
        } else if let Some(class) = iterator_value.as_class_constructor() {
            Some(Value::object(class.prototype(&self.gc_heap)))
        } else if let Some(obj) = iterator_value.as_object() {
            object::get(obj, &self.gc_heap, "prototype")
        } else {
            None
        };
        if let Some(native) = iterator_value.as_native_function()
            && let Some(from) = self.native_data_property(native, "from")
        {
            self.tag_native_value_realm(from, global);
        } else if let Some(class) = iterator_value.as_class_constructor() {
            let statics = class.statics(&self.gc_heap);
            if let Some(from) = object::get(statics, &self.gc_heap, "from") {
                self.tag_native_value_realm(from, global);
            }
        } else if let Some(obj) = iterator_value.as_object()
            && let Some(from) = object::get(obj, &self.gc_heap, "from")
        {
            self.tag_native_value_realm(from, global);
        }
        let Some(prototype) = prototype.and_then(|value| value.as_object()) else {
            return;
        };
        for name in [
            "next", "return", "throw", "map", "filter", "take", "drop", "flatMap", "toArray",
            "forEach", "reduce", "some", "every", "find",
        ] {
            if let Some(value) = object::get(prototype, &self.gc_heap, name) {
                self.tag_native_value_realm(value, global);
            }
        }
    }

    fn tag_array_realm_natives(&mut self, global: JsObject) {
        let Some(array_value) = object::get(global, &self.gc_heap, "Array") else {
            return;
        };
        self.tag_native_value_realm(array_value, global);
        let prototype = if let Some(native) = array_value.as_native_function() {
            for name in ["isArray", "of", "from", "fromAsync"] {
                if let Some(value) = self.native_data_property(native, name) {
                    self.tag_native_value_realm(value, global);
                }
            }
            self.native_data_property(native, "prototype")
        } else if let Some(obj) = array_value.as_object() {
            for name in ["isArray", "of", "from", "fromAsync"] {
                if let Some(value) = object::get(obj, &self.gc_heap, name) {
                    self.tag_native_value_realm(value, global);
                }
            }
            object::get(obj, &self.gc_heap, "prototype")
        } else {
            None
        };
        let Some(prototype) = prototype.and_then(|value| value.as_object()) else {
            return;
        };
        for name in [
            "push",
            "pop",
            "shift",
            "unshift",
            "slice",
            "concat",
            "join",
            "includes",
            "indexOf",
            "lastIndexOf",
            "at",
            "reverse",
            "fill",
            "flat",
            "splice",
            "sort",
            "toString",
            "copyWithin",
            "toReversed",
            "toSpliced",
            "toSorted",
            "with",
            "toLocaleString",
            "keys",
            "values",
            "entries",
            "forEach",
            "map",
            "filter",
            "some",
            "every",
            "find",
            "findIndex",
            "findLast",
            "findLastIndex",
            "reduce",
            "reduceRight",
            "flatMap",
        ] {
            if let Some(value) = object::get(prototype, &self.gc_heap, name) {
                self.tag_native_value_realm(value, global);
            }
        }
    }

    /// Create an additional realm global object inside this interpreter.
    pub fn create_host_realm_global(&mut self) -> Result<JsObject, VmError> {
        let state = self.build_realm_state()?;
        let global = state.global_this;
        self.extra_realms.push(state);
        Ok(global)
    }

    /// Run `body` with the realm identified by `global` as the active realm.
    pub fn with_host_realm_global<R>(
        &mut self,
        global: JsObject,
        body: impl FnOnce(&mut Self) -> Result<R, VmError>,
    ) -> Result<R, VmError> {
        if self.global_this == global {
            return body(self);
        }
        let Some(index) = self
            .extra_realms
            .iter()
            .position(|realm| realm.global_this == global)
        else {
            return Err(self.err_type(("unknown host realm global".to_string()).into()));
        };
        let mut realm = self.extra_realms.remove(index);
        self.swap_active_realm_state(&mut realm);
        let was_extra = self.active_realm_is_extra;
        self.active_realm_is_extra = true;
        let realm_roots_guard = self
            .gc_heap
            .register_extra_roots(otter_gc::ExtraRoots::new(&realm));
        let result = body(self);
        drop(realm_roots_guard);
        self.swap_active_realm_state(&mut realm);
        self.active_realm_is_extra = was_extra;
        self.extra_realms.insert(index, realm);
        result
    }

    /// Execute a host script in the realm identified by `global`.
    pub fn run_host_script_in_realm_global(
        &mut self,
        global: JsObject,
        source: &Value,
    ) -> Result<Value, VmError> {
        self.with_host_realm_global(global, |interp| interp.run_host_script(source))
    }

    /// Look up `%<Kind>IteratorPrototype%` by origin.
    #[must_use]
    pub(crate) fn builtin_iterator_prototype_for(
        &self,
        origin: BuiltinIteratorOrigin,
    ) -> Option<JsObject> {
        // A bare (override-less) builtin iterator always belongs to the
        // DEFAULT realm — extra-realm iterators get a stored override at
        // creation. While an extra realm is active its own prototype set
        // lives in the swapped fields below, so bare iterators must
        // resolve through the never-swapped default-realm copies or a
        // default-realm iterator observed from `$262.createRealm()` code
        // would claim the foreign realm's prototypes.
        if self.active_realm_is_extra {
            return self.default_realm_iterator_prototypes[Self::iterator_origin_index(origin)]
                .get();
        }
        self.active_realm_iterator_prototype_for(origin)
    }

    /// The ACTIVE realm's per-kind iterator prototype (the swapped
    /// fields) — creation-side accessor: a fresh iterator adopts the
    /// realm it is minted in.
    pub(crate) fn active_realm_iterator_prototype_for(
        &self,
        origin: BuiltinIteratorOrigin,
    ) -> Option<JsObject> {
        match origin {
            BuiltinIteratorOrigin::Array => self.array_iterator_prototype.get(),
            BuiltinIteratorOrigin::Map => self.map_iterator_prototype.get(),
            BuiltinIteratorOrigin::Set => self.set_iterator_prototype.get(),
            BuiltinIteratorOrigin::String => self.string_iterator_prototype.get(),
            BuiltinIteratorOrigin::RegExpString => self.regexp_string_iterator_prototype.get(),
            BuiltinIteratorOrigin::Helper => self.iterator_helper_prototype.get(),
            BuiltinIteratorOrigin::WrapForValidIterator => {
                self.wrap_for_valid_iterator_prototype.get()
            }
        }
    }

    /// Index into [`Self::default_realm_iterator_prototypes`].
    fn iterator_origin_index(origin: BuiltinIteratorOrigin) -> usize {
        match origin {
            BuiltinIteratorOrigin::Array => 0,
            BuiltinIteratorOrigin::Map => 1,
            BuiltinIteratorOrigin::Set => 2,
            BuiltinIteratorOrigin::String => 3,
            BuiltinIteratorOrigin::RegExpString => 4,
            BuiltinIteratorOrigin::Helper => 5,
            BuiltinIteratorOrigin::WrapForValidIterator => 6,
        }
    }

    #[cfg(test)]
    pub(crate) fn load_property_ic_count(&self) -> usize {
        self.load_property_ics
            .iter()
            .filter(|entry| entry.is_polymorphic())
            .count()
    }

    #[cfg(test)]
    pub(crate) fn store_property_ic_count(&self) -> usize {
        self.store_property_ics
            .iter()
            .filter(|entry| entry.is_polymorphic())
            .count()
    }

    /// Return aggregate property inline-cache counters.
    #[must_use]
    pub fn property_ic_stats(&self) -> property_ic::PropertyIcStats {
        self.property_ic_stats
    }

    /// Install or remove the runtime-owned JIT compiler hook.
    ///
    /// `None` keeps interpreter-only behavior. A hook returning
    /// [`JitCompileStatus::Unavailable`] or [`JitCompileStatus::Unsupported`]
    /// must also leave execution on the interpreter fallback path.
    pub fn set_jit_compiler(&mut self, hook: Option<std::sync::Arc<dyn jit::JitCompilerHook>>) {
        self.jit_hook = hook;
    }

    /// `true` when a JIT compiler hook has been installed.
    #[must_use]
    pub fn jit_compiler_installed(&self) -> bool {
        self.jit_hook.is_some()
    }

    /// Return aggregate baseline-JIT runtime counters.
    #[must_use]
    pub fn jit_runtime_stats(&self) -> JitRuntimeStats {
        self.jit_runtime_stats
    }

    /// Return the current VM-published collection method IC mirror summary.
    #[must_use]
    pub fn jit_collection_method_ic_stats(&self) -> JitCollectionMethodIcStats {
        let mut stats = JitCollectionMethodIcStats {
            slots: self.jit_collection_method_ics.len() as u64,
            ..JitCollectionMethodIcStats::default()
        };
        for slot in &self.jit_collection_method_ics {
            if slot.state == jit::JIT_COLLECTION_METHOD_IC_EMPTY {
                stats.empty_slots = stats.empty_slots.saturating_add(1);
                continue;
            }
            if slot.is_collection() {
                stats.collection_slots = stats.collection_slots.saturating_add(1);
                if slot.leaf_stub_id != jit::JIT_COLLECTION_METHOD_IC_NO_STUB {
                    stats.leaf_stub_slots = stats.leaf_stub_slots.saturating_add(1);
                }
                if slot.alloc_stub_id != jit::JIT_COLLECTION_METHOD_IC_NO_STUB {
                    stats.alloc_stub_slots = stats.alloc_stub_slots.saturating_add(1);
                }
            }
        }
        stats
    }

    /// Call-count at which a function body is offered to the JIT. Low enough
    /// that genuinely hot functions tier up early, high enough that one-shot
    /// calls never pay compile latency.
    pub(crate) const JIT_TIER_UP_THRESHOLD: u32 = 50;

    /// Entry-bail count at which an installed body is evicted and recompiled
    /// against current feedback (see [`Self::note_jit_entry_bail`]). Low enough
    /// that a body bailing on every call stops wasting entries quickly, high
    /// enough that a handful of cold-path bails (a rare branch hitting an
    /// unsupported region) never evicts a body that is fine on its hot path.
    pub(crate) const JIT_ENTRY_BAIL_REOPT_THRESHOLD: u32 = 8;

    /// Recompile budget per function for entry-bail eviction. A body still
    /// bail-looping after this many fresh-feedback recompiles is stuck on
    /// something feedback cannot express; it is pinned to the interpreter
    /// rather than thrashing the compiler.
    pub(crate) const JIT_MAX_ENTRY_BAIL_REOPTS: u32 = 4;

    /// Number of compiled back-edges the fuel counter allows between cooperative
    /// budget checkpoints. Large enough to amortize the VM re-entry across a hot
    /// loop, small enough that a runtime budget is enforced within a bounded
    /// number of iterations. The interrupt flag is polled inline every back-edge,
    /// so cancellation latency is unaffected by this batch size.
    pub(crate) const JIT_BACKEDGE_POLL_BATCH: u64 = 4096;

    /// Back-edge count at which a hot loop tiers up via OSR. Higher than the
    /// call-count threshold: a loop iterating this many times amortizes the
    /// compile cost many times over, while short loops never pay it.
    const JIT_OSR_THRESHOLD: u32 = 1000;
}
