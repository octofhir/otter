//! `Interpreter` construction and introspection accessors.
//!
//! # Contents
//! `new`/`with_string_heap_cap` (heap, shape runtime, IC tables, JIT
//! hooks all start empty), property-IC counters, and JIT stat getters.
//!
//! # Invariants
//! Construction never allocates on the GC heap; intrinsics install later
//! via bootstrap so a half-built interpreter is never observable.
#![allow(unused_imports)]
use crate::*;

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
        let well_known_symbols = WellKnownSymbols::new(&mut gc_heap)
            .expect("well-known symbol descriptions + bodies fit within any positive cap");
        startup_timer.mark("vm_well_known_symbols");
        let error_classes = ErrorClassRegistry::new(&mut gc_heap)
            .expect("error class prototypes fit within any positive cap");
        startup_timer.mark("vm_error_classes");
        let global_this = bootstrap::build_global_this(&mut gc_heap, &well_known_symbols)
            .expect("global_this fits within any positive cap");
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
            Some(function_proto)
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
        let mut interp = Self {
            template_objects: rustc_hash::FxHashMap::default(),
            string_constant_cache: rustc_hash::FxHashMap::default(),
            small_int_string_cache: vec![None; Self::SMALL_INT_STRING_CACHE as usize]
                .into_boxed_slice(),
            bigint_constant_cache: rustc_hash::FxHashMap::default(),
            lean_callback_roots: Vec::new(),
            pending_error_detail: std::cell::RefCell::new(None),
            json_root_stack: Vec::new(),
            json_stringify_capacity_hint: 0,
            array_index_accessor_protector: false,
            interrupt: InterruptFlag::new(),
            jit_backedge_fuel: Self::JIT_BACKEDGE_POLL_BATCH,
            current_byte_len: 1,
            current_function_id: 0,
            current_byte_pc: 0,
            gc_heap,
            code_space: std::sync::Arc::new(code_space::CodeSpace::default()),
            shape_runtime,
            simple_constructor_init_cache: rustc_hash::FxHashMap::default(),
            simple_constructor_shape_cache: rustc_hash::FxHashMap::default(),
            max_stack_depth: DEFAULT_MAX_STACK_DEPTH,
            sync_reentry_depth: 0,
            allow_blocking_atomics_wait: false,
            microtasks: MicrotaskQueue::new(),
            module_environments: std::collections::HashMap::new(),
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
            reg_pool: Vec::new(),
            holt_pool: Vec::new(),
            reg_stack: Vec::new(),
            reg_top: 0,
            runtime_budget: RuntimeBudget::default(),
            runtime_budget_stats: RuntimeBudgetStats::default(),
            runtime_budget_depth: 0,
            runtime_budget_turn_started_at: None,
            runtime_budget_heap_start: None,
            well_known_symbols,
            symbol_registry: SymbolRegistry::new(),
            error_classes,
            global_this,
            eval_hook: None,
            pending_generator_throw: None,
            pending_uncaught_throw: None,
            iteration_anchors: Vec::new(),
            pending_uncaught_frames: None,
            module_sources: source_registry::SourceRegistry::default(),
            active_frame_stack: std::ptr::null(),
            function_user_props: std::collections::HashMap::new(),
            function_prototype_overrides: std::collections::HashMap::new(),
            function_non_extensible: std::collections::HashSet::new(),
            function_deleted_metadata: std::collections::HashSet::new(),
            non_gc_exotic_prototype_overrides: std::collections::HashMap::new(),
            non_gc_exotic_user_props: std::collections::HashMap::new(),
            console_sink: console::default_console_sink(),
            timer_scheduler: None,
            timer_callbacks: timers::TimerCallbacks::new(),
            dynamic_import_loader: None,
            dynamic_import_registry: dynamic_import::DynamicImportRegistry::new(),
            array_iterator_prototype: None,
            map_iterator_prototype: None,
            set_iterator_prototype: None,
            string_iterator_prototype: None,
            regexp_string_iterator_prototype: None,
            iterator_helper_prototype: None,
            wrap_for_valid_iterator_prototype: None,
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
        let extra_root_depth = interp.gc_heap.push_extra_roots(extra_roots);
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
            interp.array_iterator_prototype = Some(protos.array);
            interp.map_iterator_prototype = Some(protos.map);
            interp.set_iterator_prototype = Some(protos.set);
            interp.string_iterator_prototype = Some(protos.string);
            interp.regexp_string_iterator_prototype = Some(protos.regexp_string);
            interp.iterator_helper_prototype = Some(protos.helper);
            interp.wrap_for_valid_iterator_prototype = Some(protos.wrap_for_valid_iterator);
        }
        interp.install_function_kind_prototypes_post_bootstrap();
        interp.gc_heap.pop_extra_roots_to(extra_root_depth - 1);
        interp
    }

    /// Look up `%<Kind>IteratorPrototype%` by origin.
    #[must_use]
    pub(crate) fn builtin_iterator_prototype_for(
        &self,
        origin: BuiltinIteratorOrigin,
    ) -> Option<JsObject> {
        match origin {
            BuiltinIteratorOrigin::Array => self.array_iterator_prototype,
            BuiltinIteratorOrigin::Map => self.map_iterator_prototype,
            BuiltinIteratorOrigin::Set => self.set_iterator_prototype,
            BuiltinIteratorOrigin::String => self.string_iterator_prototype,
            BuiltinIteratorOrigin::RegExpString => self.regexp_string_iterator_prototype,
            BuiltinIteratorOrigin::Helper => self.iterator_helper_prototype,
            BuiltinIteratorOrigin::WrapForValidIterator => self.wrap_for_valid_iterator_prototype,
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
