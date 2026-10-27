//! Construct an interpreter from an isolate snapshot instead of
//! running the bootstrap.
//!
//! # Contents
//! Restore heap pages, interpreter roots, code directories and keyed state.
//!
//! The restore is the mirror of `snapshot::IsolateSnapshot`'s three
//! parts: put the old-space pages back (relocated), write the fixed
//! root walk's handles into a shell interpreter, and replay the keyed
//! side state — atoms, global lexicals, registered symbols, dynamic-native
//! closures, regexp and array records by body handle. What
//! is neither pages nor roots nor keyed state is a cache, and caches
//! start empty: shape identities, transitions and provisional descendants
//! rebuild from the restored bodies, while source-to-state associations
//! refill on demand. Realm intrinsics, iterator prototypes, and function-kind
//! prototypes are installed from captured, relocated roots without
//! allocating against the restored heap.
//!
//! # Invariants
//!
//! - A snapshot comes from this process, or from the startup image this
//!   executable carries: static native entries then move by the image
//!   slide, and the image names no dynamic closure. In process, admitted
//!   bytecode is shared by `Arc` and dynamic closures are Arc-clones at
//!   their captured host-ref indices.
//! - Weak collections, `WeakRef`s and `FinalizationRegistry`s rejoin the
//!   collector's weak registries before anything can collect.
//! - Host payloads are severed; the host's run services give the declared
//!   bootstrap host objects fresh ones.
//! - Supplied account admission precedes page restoration and shape-floor publication.
//! - Each restored code directory owns fresh feedback and atom tables.
//!   IC shapes and validity cells never refer to the donor's heap.
//! - The fixed root walk must consume exactly the captured sequence;
//!   a length mismatch is a layout drift between capture and restore
//!   builds and panics rather than mis-rooting.
//! - Array own-element eligibility is refreshed after sidecar descriptors are
//!   reconstructed, after the earlier page relocation has refreshed slab bases.
//!
//! # See also
//!
//! - `crate::snapshot` — the capture half.
//! - `otter-gc/src/heap_image.rs` — the unsafe low-level page operation whose
//!   capture provenance this safe wrapper establishes.

use crate::snapshot::IsolateSnapshot;
use crate::{Interpreter, object, symbol::SymbolRegistry};
use otter_gc::raw::RawGc;

fn relocate_object_roots<const N: usize>(
    relocation: &otter_gc::Relocation,
    captured_roots: &[RawGc; N],
) -> Result<[Option<object::JsObject>; N], otter_gc::ImageError> {
    let mut restored = [None; N];
    for (slot, captured) in restored.iter_mut().zip(captured_roots.iter().copied()) {
        if captured.is_null() {
            continue;
        }
        let relocated = relocation
            .relocate_raw(captured)
            .ok_or(otter_gc::ImageError::DanglingSlot { offset: captured.0 })?;
        // SAFETY: the opaque snapshot arrays are populated only from
        // `JsObject` fields, and relocation preserves each body type.
        *slot = Some(unsafe { object::JsObject::from_offset(relocated.0) });
    }
    Ok(restored)
}

impl Interpreter {
    /// Build an interpreter from `snapshot` without running the
    /// bootstrap.
    ///
    /// # Errors
    /// Propagates page, heap and resource admission errors. The supplied account
    /// owns every fresh restored execution table and directory allocation.
    pub fn from_isolate_snapshot(
        snapshot: &IsolateSnapshot,
        account: &otter_resource::ResourceAccount,
    ) -> Result<Self, otter_gc::ImageError> {
        Self::from_isolate_snapshot_capped(snapshot, 0, account)
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
        account: &otter_resource::ResourceAccount,
    ) -> Result<Self, otter_gc::ImageError> {
        // Admit code before touching heap pages or the process-wide shape floor.
        // No externally visible owner is published if any fresh table refuses.
        let mut names = crate::property_atom::NameInterner::default();
        for (expected, name) in snapshot.atom_names.iter().enumerate() {
            let minted = names.intern(name);
            debug_assert_eq!(
                minted.raw() as usize,
                expected,
                "atom replay must re-mint identical ids"
            );
        }
        names.restore_symbol_atoms(snapshot.symbol_atoms);
        let code_space = snapshot.code_space.restore(&mut names, account)?;
        let mut gc_heap = otter_gc::GcHeap::with_max_heap_bytes(max_heap_bytes)?;
        gc_heap.set_external_bytes_account(account)?;
        object::register_gc_traceables(&mut gc_heap);
        // SAFETY: `IsolateSnapshot` has no public constructor or mutable raw
        // fields; it is produced by this VM's capture, or decoded under
        // `IsolateSnapshot::decode`'s contract from bytes this executable
        // wrote.
        // The trace/sever registrations above are the same registrations used
        // by the source interpreter.
        let relocation = unsafe { gc_heap.restore_old_space(&snapshot.image) }?;
        // The collector reaches weak bodies through its registries, never
        // through the pages; each restored one joins them as its allocation
        // did, before anything can collect.
        crate::weak_refs::register_restored_weak_bodies(&mut gc_heap);
        let iterator_prototype_roots =
            relocate_object_roots(&relocation, &snapshot.iterator_prototype_roots)?;
        let function_kind_prototypes =
            crate::function_kind::FunctionKindPrototypes::from_snapshot_roots(
                relocate_object_roots(&relocation, &snapshot.function_kind_prototype_roots)?,
            );
        // No shell construction above mints object shapes. Publish the donor's
        // id floor only after page admission succeeds so a failed restore has
        // no process-global side effect, and before any later code can mint a
        // shape that would collide with restored bodies.
        object::bump_next_shape_id_to(snapshot.next_shape_id);
        // Static entry points rebuild at identical indices; a startup image
        // read in another process already moved them by the image slide.
        gc_heap.restore_external_refs(snapshot.external_ref_addrs.iter().copied());

        // Keyed side state uses the admitted directory's atom replay.
        let shape_runtime = object::ShapeRuntime::restored_shell(&mut gc_heap, names);

        let mut interp = Self {
            local_time_zone: crate::date::LocalTimeZone::default(),
            template_objects: rustc_hash::FxHashMap::default(),
            literal_cells: rustc_hash::FxHashMap::default(),
            array_boilerplates: rustc_hash::FxHashMap::default(),
            number_strings: crate::string::number_strings::NumberStrings::empty(),
            pending_error_detail: std::cell::RefCell::new(None),
            handle_arena: crate::handles::HandleArena::new(),
            host_atoms: crate::HostAtomInterner::new(),
            object_layout_cache: crate::object_layout_cache::ObjectLayoutCache::default(),
            json_stringify_capacity_hint: 0,
            external_memory_adjustment: None,
            array_index_accessor_protector: false,
            array_buffer_detach_protector: false,
            preparing_stack_trace: false,
            array_index_accessor_protector_epoch: 0,
            interrupt: {
                let interrupt = crate::InterruptFlag::new();
                interrupt.arm_fuel(Self::JIT_BACKEDGE_POLL_BATCH);
                interrupt
            },
            atomics_wait_agent: crate::atomics_wait::WaitAgent::new(),
            jit_backedge_fuel_window: Self::JIT_BACKEDGE_POLL_BATCH,
            gc_heap,
            code_space,
            code_eviction_high_water_bytes: Self::DEFAULT_CODE_EVICTION_HIGH_WATER_BYTES,
            code_eviction_trigger_bytes: Self::DEFAULT_CODE_EVICTION_HIGH_WATER_BYTES,
            code_eviction_stats: crate::CodeEvictionStats::default(),
            property_cache: crate::property_cache::PropertyActionCache::default(),
            realm_context: None,
            shape_runtime,
            simple_constructor_init_cache: rustc_hash::FxHashMap::default(),
            simple_constructor_absence: rustc_hash::FxHashMap::default(),
            simple_constructor_shape_cache: rustc_hash::FxHashMap::default(),
            object_literal_layouts: rustc_hash::FxHashMap::default(),
            function_constructor_layouts: rustc_hash::FxHashMap::default(),
            constructor_families: Default::default(),
            arguments_shape_cache: rustc_hash::FxHashMap::default(),
            max_stack_depth: crate::DEFAULT_MAX_STACK_DEPTH,
            sync_reentry_depth: 0,
            allow_blocking_atomics_wait: false,
            microtasks: crate::MicrotaskQueue::new(),
            module_environments: std::collections::HashMap::new(),
            host_module_env_cache: std::collections::HashMap::new(),
            module_init_closures: std::collections::HashMap::new(),
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
            method_feedback: crate::interp::MethodFeedbackDirectory::default(),
            jit_hook: None,
            jit_debug: crate::jit_debug::JitDebugState::default(),
            jit_artifacts: crate::jit_artifact::JitArtifactState::default(),
            optimizing_tier_policy: crate::tier_policy::TierPolicy::default(),
            jit_entry_bail_counts: rustc_hash::FxHashMap::default(),
            jit_osr_disabled: rustc_hash::FxHashSet::default(),
            jit_code: rustc_hash::FxHashMap::default(),
            jit_template_osr_fids: rustc_hash::FxHashSet::default(),
            jit_template_compiling: rustc_hash::FxHashSet::default(),
            jit_optimized_code: rustc_hash::FxHashMap::default(),
            jit_optimized_code_cache: None,
            own_slot_cache: crate::object::OwnSlotCache::new(),
            jit_optimized_exit_profiles: std::collections::BTreeMap::new(),
            jit_parameter_widening: rustc_hash::FxHashMap::default(),
            jit_optimized_declined_epoch: rustc_hash::FxHashMap::default(),
            jit_code_cache: None,
            jit_entry_osr_only: rustc_hash::FxHashSet::default(),
            jit_runtime_stats: crate::JitRuntimeStats::default(),
            jit_code_registry: crate::jit_registry::JitCodeRegistry::new_boxed(),
            caller_function_cache: std::cell::Cell::new(None),
            jit_compile_roots: std::cell::RefCell::default(),
            jit_compile_queue: crate::jit_compile_queue::OptimizingCompileQueue::default(),
            jit_instanceof_cells: std::cell::RefCell::default(),
            runtime_turn_depth: 0,
            jit_generated_feedback_pending: false,
            jit_next_code_object_id: 1,
            jit_context: None,
            native_stack_floor: None,
            jit_detached_frame: 0,
            work_budget: crate::WorkBudget::default(),
            work_budget_stats: crate::WorkBudgetStats::default(),
            work_budget_telemetry: crate::WorkBudgetTelemetry::default(),
            work_budget_depth: 0,
            work_budget_slice_started_at: None,
            work_budget_clock_due: 0,
            work_budget_elapsed_nanos: 0,
            work_budget_heap_start: None,
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
            lazy_compile_hook: None,
            lazy_bodies_compiled: 0,
            lazy_body_store: crate::lazy_body_cache::LazyBodyStore::default(),
            owner_cache: std::cell::RefCell::new(crate::execution_context::OwnerCache::new()),
            pending_uncaught_throw: None,
            completed_activation_result: None,
            uncaught_from_promise_rejection: false,
            async_context: crate::Value::undefined(),
            iteration_anchors: Vec::new(),
            pending_throw_provenance: None,
            resource_account: account.clone(),
            function_user_props: std::collections::HashMap::new(),
            function_prototype_overrides: std::collections::HashMap::new(),
            function_prototype_slots: std::collections::HashMap::new(),
            function_non_extensible: std::collections::HashSet::new(),
            function_deleted_metadata: std::collections::HashSet::new(),
            iterator_prototype_overrides: None,
            iterator_user_props: None,
            persistent_roots: crate::persistent_roots::PersistentRoots::new(),
            pending_atomic_waits: Vec::new(),
            intl_fallback_symbol: None,
            typeof_strings: [None; 8],
            console_sink: crate::console::default_console_sink(),
            timer_scheduler: None,
            host_completion_sink: None,
            promise_rejection_hook: None,
            timer_callbacks: crate::timers::TimerCallbacks::new(),
            dynamic_import_loader: None,
            dynamic_import_registry: crate::dynamic_import::DynamicImportRegistry::new(),
            array_iterator_prototype: crate::gc_trace::RootCell::new(iterator_prototype_roots[0]),
            map_iterator_prototype: crate::gc_trace::RootCell::new(iterator_prototype_roots[1]),
            set_iterator_prototype: crate::gc_trace::RootCell::new(iterator_prototype_roots[2]),
            string_iterator_prototype: crate::gc_trace::RootCell::new(iterator_prototype_roots[3]),
            regexp_string_iterator_prototype: crate::gc_trace::RootCell::new(
                iterator_prototype_roots[4],
            ),
            iterator_helper_prototype: crate::gc_trace::RootCell::new(iterator_prototype_roots[5]),
            wrap_for_valid_iterator_prototype: crate::gc_trace::RootCell::new(
                iterator_prototype_roots[6],
            ),
            default_realm_iterator_prototypes: std::array::from_fn(|index| {
                crate::gc_trace::RootCell::new(iterator_prototype_roots[7 + index])
            }),
            function_kind_prototypes,
            cold_frames: crate::cold_frame::ColdFramePool::new(),
            realm_intrinsics: crate::realm_intrinsics::RealmIntrinsics::default(),
            regexp_legacy: crate::regexp_legacy::RegExpLegacyState::default(),
            regex_compile_cache: crate::regexp::RegexCompileCache::default(),
            regex_scratch: std::cell::Cell::default(),
            regexp_offsets: Vec::new(),
            tracer: None,
            cpu_profiler: None,
            host_services: crate::host_services::HostServices::default(),
        };

        interp.jit_code_registry.set_account(account.clone());
        interp.code_space.visit_function_headers(|header| {
            interp.jit_code_registry.link_function_header(header, 0)
        });
        // Host events run their callbacks against the realm's host-script
        // context, which a restored isolate never ran a script to set.
        interp.realm_context = snapshot.realm_context_base.and_then(|base| {
            match interp.code_space.resolve_chunk(base) {
                crate::code_space::ChunkResolution::Live {
                    function_base,
                    payload,
                } => Some(crate::ExecutionContext::from_chunk_payload(
                    payload,
                    function_base,
                    std::sync::Arc::clone(&interp.code_space),
                )),
                _ => None,
            }
        });

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

        // Keyed state: global lexicals, registered symbols and dynamic-native
        // closures.
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
        for (key, symbol) in &snapshot.registered_symbols {
            let relocated = relocation
                .relocate_raw(*symbol)
                .expect("registered symbol points into the image");
            // SAFETY: the captured handle named a `SymbolBody`; the
            // relocated offset names its restored copy.
            let handle = unsafe { crate::symbol::SymbolHandle::from_offset(relocated.0) };
            interp
                .symbol_registry
                .restore_entry(key, crate::JsSymbol::from_handle(&interp.gc_heap, handle));
        }
        for (index, payload) in &snapshot.dynamic_natives {
            crate::native_function::install_dynamic_native(&mut interp.gc_heap, *index, payload);
        }
        // A startup image written by another process of this executable
        // recorded static entry points where its program image loaded.
        if snapshot.image_slide != 0 {
            crate::native_function::slide_static_entries(&interp.gc_heap, snapshot.image_slide);
        }

        // Rebuild the one payload the page image cannot carry: every
        // regexp's compiled matcher and pattern text, from the
        // snapshot's own records. Host-data boxes and array sidecars
        // were already severed inside the page restore, before the
        // relocation walk could touch a foreign vtable. Each record names
        // its body by the handle it was captured at.
        {
            let heap = &mut interp.gc_heap;
            for (body, pattern_utf16, source) in &snapshot.regexp_payloads {
                let body = relocation
                    .relocate_raw(*body)
                    .expect("a captured regexp body is in the image");
                // SAFETY: the record was taken from a live `JsRegExpBody`
                // at this captured offset; relocation names its copy.
                let body: otter_gc::Gc<crate::regexp::JsRegExpBody> =
                    unsafe { otter_gc::Gc::from_offset(body.0) };
                heap.with_payload(body, |body| {
                    body.rebuild_after_restore(pattern_utf16, source)
                });
            }
            for (sidecar, record) in &snapshot.array_indexed_records {
                let sidecar = relocation
                    .relocate_raw(*sidecar)
                    .expect("a captured array sidecar is in the image");
                // SAFETY: the record was taken from a live `ArrayExoticSlots`
                // at this captured offset; relocation names its copy.
                let sidecar: otter_gc::Gc<crate::array::ArrayExoticSlots> =
                    unsafe { otter_gc::Gc::from_offset(sidecar.0) };
                heap.with_payload(sidecar, |sidecar| sidecar.restore_indexed_snapshot(record));
            }
            heap.for_each_live_payload::<crate::array::ArrayBody, _>(|_space, body| {
                body.refresh_dense_own_guard();
            });
        }

        // The admitted pages contain the canonical lineage graph. Rebuild
        // ids, transitions, provisional descendants and canonical key cells
        // together before any mutator can allocate against this heap.
        object::shape_interner::ShapeInterner::rebuild_restored(&mut interp.gc_heap);
        interp.constructor_families =
            crate::constructor_layout::ConstructorFamilies::restored(&interp.gc_heap);

        interp.gc_heap.set_tenure_all(false);
        Ok(interp)
    }
}

#[cfg(test)]
mod tests {
    use super::Interpreter;

    #[test]
    fn an_encoded_image_restores_and_runs() {
        let mut source = Interpreter::new().expect("fixture interpreter bootstrap");
        // A registration lives in the registry, beside the page image.
        source
            .symbol_for_key("otter.image.registered")
            .expect("register a symbol");
        source.collect_minor_tracing_runtime_roots();
        source.force_gc().expect("promote the registered symbol");
        let snapshot = source.capture_isolate_snapshot().expect("capture");
        let natives = source.startup_native_names(&snapshot);
        let mut out = otter_gc::image_codec::ImageWriter::new();
        snapshot.encode(&mut out, &natives);
        // A leaked copy stands in for the executable's own static section.
        let bytes: &'static [u8] = Box::leak(out.into_bytes().into_boxed_slice());
        let account = otter_resource::ResourceAccount::default();
        let mut input = otter_gc::image_codec::ImageReader::new(bytes);
        // SAFETY: the bytes were encoded by this executable just above.
        let decoded = unsafe {
            crate::snapshot::IsolateSnapshot::decode(&mut input, &mut |_| None, &account)
        }
        .expect("decode");
        assert!(input.is_empty());
        assert_eq!(decoded.atom_names, snapshot.atom_names);
        let restored = Interpreter::from_isolate_snapshot(&decoded, &account).expect("restore");
        assert!(restored.array_iterator_prototype.get().is_some());
        let registered = restored.symbol_registry().snapshot_entries();
        assert_eq!(registered.len(), 1);
        assert_eq!(&*registered[0].0, "otter.image.registered");
        assert_eq!(
            restored
                .symbol_registry()
                .key_for(crate::JsSymbol::from_handle(
                    &restored.gc_heap,
                    // SAFETY: the registry holds a restored `SymbolBody` handle.
                    unsafe { crate::symbol::SymbolHandle::from_offset(registered[0].1.0) },
                )),
            Some("otter.image.registered".to_owned())
        );
    }

    #[test]
    fn exact_image_cap_restore_does_not_allocate_or_panic() {
        let source = Interpreter::new().expect("fixture interpreter bootstrap");
        let snapshot = source
            .capture_isolate_snapshot()
            .expect("fresh isolate must be capturable");
        let exact_cap = snapshot.image.live_bytes();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Interpreter::from_isolate_snapshot_capped(
                &snapshot,
                exact_cap,
                &otter_resource::ResourceAccount::default(),
            )
        }));
        let restored = result
            .expect("restore at the exact image cap must not panic")
            .expect("the image itself fits the exact cap");

        assert!(restored.array_iterator_prototype.get().is_some());
        assert!(
            restored
                .function_kind_prototypes
                .snapshot_roots()
                .iter()
                .all(|root| !root.is_null())
        );
    }
}
