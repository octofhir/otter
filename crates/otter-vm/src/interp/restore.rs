//! Construct an interpreter from an isolate snapshot instead of
//! running the bootstrap.
//!
//! The restore is the mirror of `snapshot::IsolateSnapshot`'s three
//! parts: put the old-space pages back (relocated), write the fixed
//! root walk's handles into a shell interpreter, and replay the keyed
//! side state — atoms, global lexicals, dynamic-native closures. What
//! is neither pages nor roots nor keyed state is a cache, and caches
//! start empty: the shape runtime's side tables refill on use (only
//! its id registry is re-populated from the restored shape bodies),
//! and the realm's typed intrinsic slots are re-primed from the
//! restored global graph by the same `prime_realm_caches` the ordinary
//! constructor runs.
//!
//! # Invariants
//!
//! - In-process only for now: static native bodies carry entry
//!   addresses verbatim (valid within the process), the code space is
//!   shared by `Arc`, and dynamic closures are Arc-clones at their
//!   captured host-ref indices. The cross-build path adds external-ref
//!   resolution and the re-install-by-name list on top of this.
//! - The fixed root walk must consume exactly the captured sequence;
//!   a length mismatch is a layout drift between capture and restore
//!   builds and panics rather than mis-rooting.
//!
//! # See also
//!
//! - `crate::snapshot` — the capture half.
//! - `scratchpad/PLAN_BOOTSTRAP_SNAPSHOT.md` — S3/S4.

use crate::snapshot::IsolateSnapshot;
use crate::{Interpreter, object, symbol::SymbolRegistry};
use otter_gc::raw::RawGc;

impl Interpreter {
    /// Build an interpreter from `snapshot` without running the
    /// bootstrap.
    ///
    /// # Errors
    /// Propagates [`otter_gc::ImageError`] from the page restore.
    pub fn from_isolate_snapshot(snapshot: &IsolateSnapshot) -> Result<Self, otter_gc::ImageError> {
        Self::from_isolate_snapshot_capped(snapshot, 0)
    }

    /// [`Self::from_isolate_snapshot`] with a heap cap (`0` =
    /// unlimited), for hosts that bound per-isolate memory — the
    /// test262 runner caps every test at 512 MiB.
    ///
    /// # Errors
    /// Propagates [`otter_gc::ImageError`] from the page restore.
    pub fn from_isolate_snapshot_capped(
        snapshot: &IsolateSnapshot,
        max_heap_bytes: u64,
    ) -> Result<Self, otter_gc::ImageError> {
        let mut gc_heap = otter_gc::GcHeap::with_max_heap_bytes(max_heap_bytes)
            .expect("GcHeap construction never fails on the default cage");
        object::register_gc_traceables(&mut gc_heap);
        // Before anything can mint a shape id in this process: freshly
        // minted ids must never collide with the ids the image's shapes
        // and dictionary objects already carry.
        object::bump_next_shape_id_to(snapshot.next_shape_id);
        let relocation = gc_heap.restore_old_space(&snapshot.image)?;
        // Same indices, current addresses: every captured entry slides
        // by however far the loader moved the binary image (zero for an
        // in-process restore).
        gc_heap.restore_external_refs(
            snapshot
                .external_ref_addrs
                .iter()
                .map(|&addr| relocation.slide_image_pointer(addr)),
        );

        // Keyed side state that other shells depend on.
        let names = std::sync::Arc::new(crate::property_atom::NameInterner::default());
        for (expected, name) in snapshot.atom_names.iter().enumerate() {
            let minted = names.intern(name);
            debug_assert_eq!(
                minted.raw() as usize,
                expected,
                "atom replay must re-mint identical ids"
            );
        }
        let shape_runtime = object::ShapeRuntime::restored_shell(std::sync::Arc::clone(&names));

        let mut interp = Self {
            local_time_zone: crate::date::LocalTimeZone::default(),
            template_objects: rustc_hash::FxHashMap::default(),
            string_constant_cells: rustc_hash::FxHashMap::default(),
            small_int_string_cache: vec![None; Self::SMALL_INT_STRING_CACHE as usize]
                .into_boxed_slice(),
            bigint_constant_cache: rustc_hash::FxHashMap::default(),
            lean_callback_roots: Vec::new(),
            pending_error_detail: std::cell::RefCell::new(None),
            handle_arena: crate::handles::HandleArena::new(),
            host_atoms: crate::HostAtomInterner::new(),
            object_layout_cache: crate::object_layout_cache::ObjectLayoutCache::default(),
            json_stringify_capacity_hint: 0,
            external_memory_adjustment: None,
            array_index_accessor_protector: false,
            array_index_accessor_protector_epoch: 0,
            interrupt: crate::InterruptFlag::new(),
            jit_backedge_fuel: Self::JIT_BACKEDGE_POLL_BATCH,
            gc_heap,
            code_space: std::sync::Arc::clone(&snapshot.code_space),
            names,
            property_cache: crate::property_cache::PropertyLookupCache::default(),
            realm_context: None,
            shape_runtime,
            shape_epoch: 0,
            simple_constructor_init_cache: rustc_hash::FxHashMap::default(),
            simple_constructor_shape_cache: rustc_hash::FxHashMap::default(),
            constructor_field_transition_cache: rustc_hash::FxHashMap::default(),
            constructor_field_capacity_cache: rustc_hash::FxHashMap::default(),
            constructor_prototype_shape_cache: rustc_hash::FxHashMap::default(),
            arguments_shape_cache: rustc_hash::FxHashMap::default(),
            max_stack_depth: crate::DEFAULT_MAX_STACK_DEPTH,
            sync_reentry_depth: 0,
            jit_materialized_generated_call_depth: 0,
            allow_blocking_atomics_wait: false,
            microtasks: crate::MicrotaskQueue::new(),
            module_environments: std::collections::HashMap::new(),
            host_module_env_cache: std::collections::HashMap::new(),
            module_init_upvalues: std::collections::HashMap::new(),
            global_lexicals: rustc_hash::FxHashMap::default(),
            global_lexical_epoch: 0,
            global_lexical_load_ic: rustc_hash::FxHashMap::default(),
            global_object_load_ic: rustc_hash::FxHashMap::default(),
            module_hoisted: std::collections::HashSet::new(),
            module_evaluation_depth: 0,
            module_resolution_cache: std::collections::HashMap::new(),
            module_records: std::collections::HashMap::new(),
            next_module_async_order: 0,
            deferred_namespaces: std::collections::HashMap::new(),
            module_namespaces: std::collections::HashMap::new(),
            module_resolved_exports: std::collections::HashMap::new(),
            rejection_tracker: crate::promise_rejection::RejectionTracker::default(),
            feedback_directory: crate::interp::FeedbackDirectory::default(),
            jit_hook: None,
            jit_debug: crate::jit_debug::JitDebugState::default(),
            jit_artifacts: crate::jit_artifact::JitArtifactState::default(),
            jit_call_counts: rustc_hash::FxHashMap::default(),
            optimizing_tier_policy: crate::tier_policy::TierPolicy::default(),
            jit_entry_bail_counts: rustc_hash::FxHashMap::default(),
            jit_entry_reopt_counts: rustc_hash::FxHashMap::default(),
            jit_feedback_refresh_attempted: rustc_hash::FxHashSet::default(),
            jit_pending_direct_targets: rustc_hash::FxHashMap::default(),
            jit_osr_disabled: rustc_hash::FxHashSet::default(),
            jit_osr_counts: rustc_hash::FxHashMap::default(),
            jit_osr_threshold: Self::JIT_OSR_THRESHOLD,
            jit_code: rustc_hash::FxHashMap::default(),
            jit_optimized_code: rustc_hash::FxHashMap::default(),
            jit_optimized_code_cache: None,
            jit_optimized_bail_pcs: std::collections::BTreeMap::new(),
            jit_optimized_bail_counts: rustc_hash::FxHashMap::default(),
            jit_optimized_reopt_counts: rustc_hash::FxHashMap::default(),
            jit_optimized_declined_epoch: rustc_hash::FxHashMap::default(),
            jit_osr_code: rustc_hash::FxHashMap::default(),
            jit_code_cache: None,
            jit_entry_osr_only: rustc_hash::FxHashSet::default(),
            jit_runtime_stats: crate::JitRuntimeStats::default(),
            jit_code_registry: crate::jit_registry::JitCodeRegistry::new_boxed(),
            jit_generated_feedback_pending: false,
            jit_next_code_object_id: 1,
            register_stack: crate::register_stack::RegisterStack::new(),
            jit_native_activations: vec![
                crate::jit::JitNativeActivation::EMPTY;
                crate::DEFAULT_MAX_STACK_DEPTH as usize
            ],
            jit_native_activation_top: 0,
            jit_machine_roots: 0,
            runtime_budget: crate::RuntimeBudget::default(),
            runtime_budget_stats: crate::RuntimeBudgetStats::default(),
            runtime_budget_depth: 0,
            runtime_budget_turn_started_at: None,
            runtime_budget_heap_start: None,
            well_known_symbols: crate::symbol::WellKnownSymbols::restored_shell(),
            symbol_registry: SymbolRegistry::new(),
            error_classes: crate::error_classes::ErrorClassRegistry::restored_shell(),
            global_this: object::JsObject::null(),
            extra_realms: Vec::new(),
            active_realm_id: 0,
            next_realm_id: 1,
            function_realm_ids: rustc_hash::FxHashMap::default(),
            active_realm_is_extra: false,
            eval_hook: None,
            pending_generator_throw: None,
            pending_uncaught_throw: None,
            uncaught_from_promise_rejection: false,
            async_context: crate::Value::undefined(),
            iteration_anchors: Vec::new(),
            pending_uncaught_frames: None,
            module_sources: crate::source_registry::SourceRegistry::default(),
            function_user_props: std::collections::HashMap::new(),
            function_prototype_overrides: std::collections::HashMap::new(),
            function_non_extensible: std::collections::HashSet::new(),
            function_deleted_metadata: std::collections::HashSet::new(),
            non_gc_exotic_prototype_overrides: std::collections::HashMap::new(),
            non_gc_exotic_user_props: std::collections::HashMap::new(),
            iterator_prototype_overrides: None,
            iterator_user_props: None,
            persistent_roots: crate::persistent_roots::PersistentRoots::new(),
            console_sink: crate::console::default_console_sink(),
            timer_scheduler: None,
            host_completion_sink: None,
            promise_rejection_hook: None,
            timer_callbacks: crate::timers::TimerCallbacks::new(),
            dynamic_import_loader: None,
            dynamic_import_registry: crate::dynamic_import::DynamicImportRegistry::new(),
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
            function_kind_prototypes: crate::function_kind::FunctionKindPrototypes::default(),
            cold_frames: crate::cold_frame::ColdFramePool::new(),
            realm_intrinsics: crate::realm_intrinsics::RealmIntrinsics::default(),
            regex_compile_cache: crate::regexp::RegexCompileCache::default(),
            tracer: None,
            cpu_profiler: None,
        };

        // Write the captured fixed roots back through the same walk that
        // collected them, relocated to the restored pages.
        let mut cursor = 0usize;
        let roots = &snapshot.fixed_roots;
        interp.visit_snapshot_roots(&mut |slot| {
            let captured = roots
                .get(cursor)
                .copied()
                .expect("restore walk longer than capture");
            cursor += 1;
            let restored = if captured.is_null() {
                RawGc::NULL
            } else {
                relocation
                    .relocate_raw(captured)
                    .expect("captured root points into the image")
            };
            // SAFETY: the walk yields live slots of `interp`'s fields.
            unsafe { *slot = restored };
        });
        assert_eq!(
            cursor,
            roots.len(),
            "restore walk shorter than capture — root layout drifted"
        );
        interp
            .well_known_symbols
            .refresh_from_bodies(&interp.gc_heap);

        // Keyed state: global lexicals and dynamic-native closures.
        for (name, cell, is_const) in &snapshot.global_lexicals {
            let relocated = relocation
                .relocate_raw(*cell)
                .expect("lexical cell points into the image");
            // SAFETY: the captured cell named an `UpvalueCellBody`; the
            // relocated offset names its restored copy.
            let cell = unsafe { crate::UpvalueCell::from_offset(relocated.0) };
            interp
                .global_lexicals
                .insert(name.clone(), (cell, *is_const));
        }
        for (index, payload) in &snapshot.dynamic_natives {
            crate::native_function::install_dynamic_native(&mut interp.gc_heap, *index, payload);
        }

        // Rebuild the one payload the page image cannot carry: every
        // regexp's compiled matcher and pattern text, from the
        // snapshot's own records. Host-data boxes and array sidecars
        // were already severed inside the page restore, before the
        // relocation walk could touch a foreign vtable. The live walk
        // here visits bodies in the same page order the capture walk
        // did, so the records zip positionally.
        {
            let heap = &interp.gc_heap;
            let mut regexps: Vec<*mut crate::regexp::JsRegExpBody> = Vec::new();
            heap.for_each_live_payload::<crate::regexp::JsRegExpBody, _>(|_space, body| {
                regexps.push(body as *const _ as *mut _);
            });
            assert_eq!(
                regexps.len(),
                snapshot.regexp_payloads.len(),
                "regexp record count must match the restored bodies"
            );
            let mut array_sidecars: Vec<*mut crate::array::ArrayExoticSlots> = Vec::new();
            heap.for_each_live_payload::<crate::array::ArrayExoticSlots, _>(|_space, body| {
                array_sidecars.push(body as *const _ as *mut _);
            });
            assert_eq!(
                array_sidecars.len(),
                snapshot.array_sidecar_flags.len(),
                "array-sidecar record count must match the restored bodies"
            );
            // SAFETY: single mutator, no allocation between collect and
            // write; each pointer names a live payload of its type.
            unsafe {
                for (body, (pattern_utf16, source)) in
                    regexps.into_iter().zip(&snapshot.regexp_payloads)
                {
                    (*body).rebuild_after_restore(pattern_utf16, source);
                }
                for (body, records) in array_sidecars
                    .into_iter()
                    .zip(&snapshot.array_sidecar_flags)
                {
                    (*body).restore_property_flags(records);
                }
            }
        }

        // Native entry points are binary-image addresses; slide each one
        // by the loader's image move. In-process the slide is zero and
        // the pass rewrites nothing.
        let slide = relocation.image_pointer_slide();
        if slide != 0 {
            let mut natives: Vec<*mut crate::native_function::NativeFunctionBody> = Vec::new();
            interp
                .gc_heap
                .for_each_live_payload::<crate::native_function::NativeFunctionBody, _>(
                    |_space, body| {
                        natives.push(body as *const _ as *mut _);
                    },
                );
            // SAFETY: single mutator, no allocation between collect and
            // write; each pointer names a live payload of its type.
            unsafe {
                for body in natives {
                    (*body).slide_entry_points(slide);
                }
            }
        }

        // Re-register every restored shape body under its id; the other
        // shape-runtime tables are lookup caches that refill on use.
        let mut restored_shapes: Vec<(crate::object::ShapeId, object::ShapeHandle)> = Vec::new();
        let cage_base = otter_gc::cage_base() as usize;
        interp
            .gc_heap
            .for_each_live_payload::<object::ShapeBody, _>(|_space, body| {
                let header = (body as *const object::ShapeBody as usize)
                    - std::mem::size_of::<otter_gc::GcHeader>();
                let offset = (header - cage_base) as u32;
                // SAFETY: the payload walk visited a live `ShapeBody`
                // cell at this cage offset.
                let handle: object::ShapeHandle = unsafe { otter_gc::Gc::from_offset(offset) };
                restored_shapes.push((body.id(), handle));
            });
        for (id, handle) in restored_shapes {
            interp.shape_runtime.register_restored_shape(id, handle);
        }

        // Prime the realm's typed caches from the restored global graph.
        interp.prime_realm_caches();
        interp.gc_heap.set_tenure_all(false);
        Ok(interp)
    }
}
