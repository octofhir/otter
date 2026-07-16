//! `Interpreter` construction and introspection accessors.
//!
//! # Contents
//! - `new` / `with_string_heap_cap` bootstrap the heap, realm surfaces,
//!   shape runtime, caches, and JIT state.
//! - Host-realm construction publishes a provisional traced `RealmState`
//!   before any later allocation and finalizes JS-visible error globals through
//!   the handle arena.
//! - Introspection accessors expose property-IC, JIT, protector, and shape
//!   epoch counters.
//!
//! # Invariants
//! - Every partially built GC graph is owned either by an RAII bootstrap root
//!   scope or by the interpreter's ordinary traced realm graph.
//! - Root providers are dropped before their stack slots move into the
//!   interpreter, and no GC allocation occurs during that move.
//! - A half-built interpreter is never observable outside this module.
//! - Protector and shape epochs start at zero and are isolate-local plain data,
//!   not GC roots.
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

    /// Current array-index accessor protector epoch.
    ///
    /// This starts at zero and advances exactly once when the one-shot
    /// protector latch first observes an array-index accessor definition.
    #[must_use]
    pub fn array_index_accessor_protector_epoch(&self) -> u64 {
        self.array_index_accessor_protector_epoch
    }

    /// Current ordinary-object prototype shape epoch.
    ///
    /// Slice 11.8 advances this only for an actual prototype change in the
    /// ordinary-`JsObject` branch of the proxy-aware `[[SetPrototypeOf]]`
    /// funnel. Other exotic or low-level shape/prototype mutations are not
    /// covered by this epoch yet.
    #[must_use]
    pub fn shape_epoch(&self) -> u64 {
        self.shape_epoch
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
        // VM-owned phase of the isolate runtime-stub table. JIT-owned slots
        // stay vacant until explicit compiler-hook installation.
        let jit_runtime_stub_entries = crate::runtime_stubs::vm_runtime_stub_entries();
        let jit_runtime_stub_machine_entries =
            crate::runtime_stubs::machine_stub_entries(&jit_runtime_stub_entries);
        let jit_runtime_stub_table = crate::native_abi::RuntimeStubTable::new(
            jit_runtime_stub_machine_entries.as_ptr() as u64,
            crate::native_abi::RUNTIME_STUB_DESCRIPTORS.as_ptr() as u64,
            jit_runtime_stub_machine_entries.len() as u32,
        );
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
            external_memory_adjustment: None,
            array_index_accessor_protector: false,
            array_index_accessor_protector_epoch: 0,
            interrupt: InterruptFlag::new(),
            jit_backedge_fuel: Self::JIT_BACKEDGE_POLL_BATCH,
            gc_heap,
            code_space: std::sync::Arc::new(code_space::CodeSpace::default()),
            realm_context: None,
            shape_runtime,
            shape_epoch: 0,
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
            feedback_directory: crate::interp::FeedbackDirectory::default(),
            jit_hook: None,
            jit_call_counts: rustc_hash::FxHashMap::default(),
            optimizing_tier_policy: tier_policy::TierPolicy::default(),
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
            jit_optimized_code: rustc_hash::FxHashMap::default(),
            jit_optimized_code_cache: None,
            jit_optimized_declined_epoch: rustc_hash::FxHashMap::default(),
            jit_osr_code: rustc_hash::FxHashMap::default(),
            jit_code_cache: None,
            jit_entry_osr_only: rustc_hash::FxHashSet::default(),
            jit_direct_method_cache: Vec::new(),
            jit_runtime_stats: JitRuntimeStats::default(),
            jit_code_registry: crate::jit_registry::JitCodeRegistry::new_boxed(),
            jit_next_code_object_id: 1,
            jit_runtime_stub_entries,
            jit_runtime_stub_machine_entries,
            jit_runtime_stub_table,
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
            active_realm_id: 0,
            next_realm_id: 1,
            function_realm_ids: rustc_hash::FxHashMap::default(),
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

    fn build_realm_state(&mut self, id: u32) -> Result<usize, VmError> {
        let error_classes = ErrorClassRegistry::new(&mut self.gc_heap).map_err(crate::oom_to_vm)?;
        let state_index = self.extra_realms.len();
        self.extra_realms.push(RealmState {
            id,
            // Temporary duplicate of the active global. Moving-GC can now
            // trace `error_classes` through the normal Interpreter root walk
            // while the real realm global is built below; no raw root slots or
            // contributor-facing heap API are needed.
            global_this: self.global_this,
            error_classes,
            realm_intrinsics: realm_intrinsics::RealmIntrinsics::default(),
            array_iterator_prototype: None,
            map_iterator_prototype: None,
            set_iterator_prototype: None,
            string_iterator_prototype: None,
            regexp_string_iterator_prototype: None,
            iterator_helper_prototype: None,
            wrap_for_valid_iterator_prototype: None,
        });
        let result = self.initialize_realm_state(state_index);
        if result.is_err() {
            self.extra_realms.remove(state_index);
        }
        result.map(|()| state_index)
    }

    fn initialize_realm_state(&mut self, state_index: usize) -> Result<(), VmError> {
        let global_this = bootstrap::build_global_this(&mut self.gc_heap, &self.well_known_symbols)
            .map_err(|err| {
                self.err_type((format!("createRealm bootstrap failed: {err}")).into())
            })?;
        self.extra_realms[state_index].global_this = global_this;
        crate::intrinsics::symbol::install_symbol_well_knowns_post_bootstrap(
            &mut self.gc_heap,
            self.extra_realms[state_index].global_this,
            &self.well_known_symbols,
        )
        .map_err(|err| {
            self.err_type((format!("createRealm Symbol bootstrap failed: {err}")).into())
        })?;
        let global_this = self.extra_realms[state_index].global_this;
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
            // The installer above allocates. Re-read both objects from the
            // rooted realm graph instead of using pre-safepoint raw handles.
            let global_this = self.extra_realms[state_index].global_this;
            let function_prototype =
                resolve_ctor_prototype(&mut self.gc_heap, global_this, "Function")
                    .ok_or(VmError::InvalidOperand)?;
            if let Some(object_prototype) =
                resolve_ctor_prototype(&mut self.gc_heap, global_this, "Object")
            {
                self.finalize_extra_realm_error_classes(
                    state_index,
                    function_prototype,
                    object_prototype,
                )?;
            }
        }
        let mut realm_intrinsics = realm_intrinsics::RealmIntrinsics::default();
        let global_this = self.extra_realms[state_index].global_this;
        realm_intrinsics.populate(&mut self.gc_heap, global_this);
        self.extra_realms[state_index].realm_intrinsics = realm_intrinsics;
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
            let state = &mut self.extra_realms[state_index];
            state.array_iterator_prototype = Some(protos.array);
            state.map_iterator_prototype = Some(protos.map);
            state.set_iterator_prototype = Some(protos.set);
            state.string_iterator_prototype = Some(protos.string);
            state.regexp_string_iterator_prototype = Some(protos.regexp_string);
            state.iterator_helper_prototype = Some(protos.helper);
            state.wrap_for_valid_iterator_prototype = Some(protos.wrap_for_valid_iterator);
        }
        let global_this = self.extra_realms[state_index].global_this;
        self.tag_array_realm_natives(global_this);
        self.tag_iterator_realm_natives(global_this);
        Ok(())
    }

    /// Finish a host realm's native error hierarchy through the interpreter's
    /// high-level handle arena. The realm registry itself already lives in
    /// `extra_realms` and is traced by the normal runtime root walk; every
    /// JS-visible global write resolves its object/value handles after the
    /// preceding allocation.
    fn finalize_extra_realm_error_classes(
        &mut self,
        state_index: usize,
        function_prototype: JsObject,
        object_prototype: JsObject,
    ) -> Result<(), VmError> {
        let registry = &self.extra_realms[state_index].error_classes;
        let error_constructor = registry.constructor(ErrorKind::Error);
        let error_prototype = registry.prototype(ErrorKind::Error);
        object::set_prototype(
            error_constructor,
            &mut self.gc_heap,
            Some(function_prototype),
        );
        object::set_prototype(error_prototype, &mut self.gc_heap, Some(object_prototype));
        for kind in [
            ErrorKind::TypeError,
            ErrorKind::RangeError,
            ErrorKind::SyntaxError,
            ErrorKind::ReferenceError,
            ErrorKind::URIError,
            ErrorKind::EvalError,
            ErrorKind::AggregateError,
        ] {
            let constructor = self.extra_realms[state_index]
                .error_classes
                .constructor(kind);
            let error_constructor = self.extra_realms[state_index]
                .error_classes
                .constructor(ErrorKind::Error);
            object::set_prototype(constructor, &mut self.gc_heap, Some(error_constructor));
        }
        for (name, kind) in [
            ("Error", ErrorKind::Error),
            ("TypeError", ErrorKind::TypeError),
            ("RangeError", ErrorKind::RangeError),
            ("SyntaxError", ErrorKind::SyntaxError),
            ("ReferenceError", ErrorKind::ReferenceError),
            ("URIError", ErrorKind::URIError),
            ("EvalError", ErrorKind::EvalError),
            ("AggregateError", ErrorKind::AggregateError),
        ] {
            self.with_handle_scope(|interp, scope| {
                let global = interp.scoped_value(
                    scope,
                    Value::object(interp.extra_realms[state_index].global_this),
                );
                let constructor = interp.scoped_value(
                    scope,
                    Value::object(
                        interp.extra_realms[state_index]
                            .error_classes
                            .constructor(kind),
                    ),
                );
                interp.scoped_define_data(
                    scope,
                    global,
                    name,
                    constructor,
                    object::PropertyFlags::new(true, false, true),
                )
            })?;
        }
        Ok(())
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
        let id = self.next_realm_id;
        self.next_realm_id = self
            .next_realm_id
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        let state_index = self.build_realm_state(id)?;
        Ok(self.extra_realms[state_index].global_this)
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
        let previous_realm_id = self.active_realm_id;
        self.active_realm_id = realm.id;
        let was_extra = self.active_realm_is_extra;
        self.active_realm_is_extra = true;
        let realm_roots_guard = self
            .gc_heap
            .register_extra_roots(otter_gc::ExtraRoots::new(&realm));
        let result = body(self);
        drop(realm_roots_guard);
        self.swap_active_realm_state(&mut realm);
        self.active_realm_id = previous_realm_id;
        self.active_realm_is_extra = was_extra;
        self.extra_realms.insert(index, realm);
        result
    }

    /// Run `body` with a stable realm identity active. Bytecode function
    /// metadata uses scalar ids, while this boundary reuses the existing
    /// traced [`RealmState`] swap instead of retaining raw GC handles.
    pub(crate) fn with_host_realm_id<R>(
        &mut self,
        realm_id: u32,
        body: impl FnOnce(&mut Self) -> Result<R, VmError>,
    ) -> Result<R, VmError> {
        if realm_id == self.active_realm_id {
            return body(self);
        }
        let global = self
            .extra_realms
            .iter()
            .find(|realm| realm.id == realm_id)
            .map(|realm| realm.global_this)
            .ok_or_else(|| self.err_type(("unknown host realm id".to_string()).into()))?;
        self.with_host_realm_global(global, body)
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
        self.feedback_directory
            .polymorphic_property_count(property_ic::PropertyIcKind::Load)
    }

    #[cfg(test)]
    pub(crate) fn store_property_ic_count(&self) -> usize {
        self.feedback_directory
            .polymorphic_property_count(property_ic::PropertyIcKind::Store)
    }

    /// Return aggregate property inline-cache counters.
    #[must_use]
    pub fn property_ic_stats(&self) -> property_ic::PropertyIcStats {
        self.feedback_directory.property_stats()
    }

    /// Override the back-edge count at which a hot loop tiers up via OSR.
    ///
    /// Structured counterpart of the `OTTER_JIT_OSR_THRESHOLD` conformance
    /// knob for embedders and differential harnesses that must force loop
    /// tier-up without touching the process environment. Zero is rejected;
    /// the current threshold is kept.
    pub fn set_jit_osr_threshold(&mut self, threshold: u32) {
        if threshold > 0 {
            self.jit_osr_threshold = threshold;
        }
    }

    /// Install or remove the runtime-owned JIT compiler hook.
    ///
    /// `None` keeps interpreter-only behavior. A hook returning
    /// [`JitCompileStatus::Unavailable`] or [`JitCompileStatus::Unsupported`]
    /// must also leave execution on the interpreter fallback path.
    ///
    /// Installation is the second phase of the isolate runtime-stub table:
    /// VM-owned entries are rebuilt, then every JIT-owned transition binding is
    /// validated against the descriptor inventory (dense id, matching signature
    /// family, nonzero entry, vacant slot) and installed. With a hook present
    /// no slot may stay vacant; a bad binding fails here, never at a call.
    pub fn set_jit_compiler(&mut self, hook: Option<std::sync::Arc<dyn jit::JitCompilerHook>>) {
        let mut entries = crate::runtime_stubs::vm_runtime_stub_entries();
        if let Some(compiler) = &hook {
            for binding in compiler.runtime_stub_bindings() {
                let index = binding
                    .id
                    .checked_sub(1)
                    .map(|index| index as usize)
                    .expect("JIT runtime-stub id is 1-based");
                let descriptor = crate::native_abi::RUNTIME_STUB_DESCRIPTORS
                    .get(index)
                    .expect("JIT runtime-stub id names a VM descriptor");
                assert_eq!(descriptor.id, binding.id);
                assert_eq!(
                    descriptor.signature, binding.signature,
                    "JIT runtime-stub binding {} declares the descriptor signature family",
                    binding.id
                );
                assert_ne!(binding.entry_addr, 0);
                assert!(
                    matches!(
                        entries[index],
                        crate::runtime_stubs::RuntimeStubEntry::Vacant
                    ),
                    "JIT runtime-stub binding {} may only fill a JIT-owned slot",
                    binding.id
                );
                entries[index] = crate::runtime_stubs::RuntimeStubEntry::JitOwned {
                    signature: binding.signature,
                    entry_addr: binding.entry_addr,
                };
            }
            for (index, entry) in entries.iter().enumerate() {
                assert!(
                    !matches!(entry, crate::runtime_stubs::RuntimeStubEntry::Vacant),
                    "runtime stub {} left vacant after JIT installation",
                    index + 1
                );
            }
        }
        self.jit_runtime_stub_entries = entries;
        self.jit_runtime_stub_machine_entries =
            crate::runtime_stubs::machine_stub_entries(&self.jit_runtime_stub_entries);
        self.jit_runtime_stub_table = crate::native_abi::RuntimeStubTable::new(
            self.jit_runtime_stub_machine_entries.as_ptr() as u64,
            crate::native_abi::RUNTIME_STUB_DESCRIPTORS.as_ptr() as u64,
            self.jit_runtime_stub_machine_entries.len() as u32,
        );
        self.jit_hook = hook;
    }

    /// Address of the published C-layout header over the isolate-owned
    /// machine entry column. The header and both columns live as long as the
    /// interpreter and are replaced only by [`Self::set_jit_compiler`].
    #[must_use]
    pub fn jit_runtime_stub_table_addr(&self) -> u64 {
        std::ptr::from_ref(&self.jit_runtime_stub_table) as u64
    }

    /// Address of the published isolate code-registry view. The boxed registry
    /// cell is address-stable for the interpreter's lifetime, so the view
    /// survives interpreter moves and resolves safepoints for any installed
    /// code object.
    #[must_use]
    pub fn jit_code_registry_view_addr(&self) -> u64 {
        self.jit_code_registry.view_addr()
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

    /// Return the current collection method IC summary.
    #[must_use]
    pub fn jit_collection_method_ic_stats(&self) -> JitCollectionMethodIcStats {
        self.feedback_directory.collection_method_stats()
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
