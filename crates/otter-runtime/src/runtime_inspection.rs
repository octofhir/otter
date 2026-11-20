//! Heap inspection, execution statistics and JIT diagnostics.
//!
//! # Contents
//! Heap and code residency, profiling, execution counters and diagnostic capture.
//!
//! # Invariants
//! Diagnostics observe the owning isolate and use its resource accounting.
//!
//! # See also
//! - [`crate::Runtime`] owns the isolate session.

use crate::*;

impl Runtime {
    /// Force a full garbage collection cycle — mark, ephemeron fixpoint,
    /// WeakRef/FinalizationRegistry processing (cleanup callbacks are
    /// enqueued as microtasks), then sweep. This is the `--expose-gc`
    /// collection, also run before the `'exit'` event so finalization
    /// WeakRefs resolve the way Node's do at shutdown.
    pub fn force_full_gc(&mut self) {
        let _ = self.interp.force_gc();
    }

    /// Configured heap cap in bytes (`0` = disabled).
    ///
    /// Allocations against the [`otter_gc::GcHeap`] and string allocations
    /// against the interpreter's string heap that would overshoot the cap
    /// surface as [`OtterError::OutOfMemory`].
    #[must_use]
    pub fn max_heap_bytes(&self) -> u64 {
        self.config.max_heap_bytes
    }

    /// Per-heap GC counters: live objects / live bytes / per-
    /// `type_tag` rows / last-GC pause / cycle counter.
    ///
    /// Takes `&mut self` because the aggregate `live_objects` /
    /// `live_bytes` fields are derived lazily from the per-tag
    /// rows (the alloc fast path only updates the per-tag
    /// counters, see [`otter_gc::GcHeap::gc_stats`]).
    pub fn heap_stats(&mut self) -> &GcStats {
        self.interp.gc_heap_mut().gc_stats()
    }

    /// Snapshot collector space and off-heap reservation accounting.
    ///
    /// Unlike a heap snapshot, this copies fixed-size counters only: it does
    /// not walk the heap, allocate diagnostic objects, or enable background
    /// instrumentation.
    #[must_use]
    pub fn heap_accounting_stats(&self) -> otter_gc::HeapStats {
        self.interp.gc_heap().stats()
    }

    /// Return the VM's work-budget policy.
    #[must_use]
    pub fn work_budget(&self) -> WorkBudget {
        self.interp.work_budget()
    }

    /// Replace the per-slice work budget of a built runtime.
    ///
    /// [`RuntimeBuilder::work_budget`] is the configured form and is in
    /// force before the first turn; this retunes a live isolate. Observe mode
    /// records crossings in [`WorkBudgetStats`], Reject returns a budget
    /// diagnostic, and Yield resumes the same turn after a host scheduling
    /// point.
    pub fn set_work_budget(&mut self, budget: WorkBudget) {
        self.interp.set_work_budget(budget);
    }

    /// Snapshot VM work-budget/resource counters.
    #[must_use]
    pub fn work_budget_stats(&self) -> WorkBudgetStats {
        self.interp.work_budget_stats()
    }

    /// Return the cell this isolate publishes its budget counters to.
    ///
    /// The cell outlives a borrow of the runtime, so a monitor thread can read
    /// the counters while the isolate keeps running.
    #[must_use]
    pub fn budget_telemetry(&self) -> WorkBudgetTelemetry {
        self.interp.work_budget_telemetry()
    }

    /// Read the isolate's CPU policy, its published counters, and the shared
    /// resource ledger as one consistent picture.
    #[must_use]
    pub fn budget_report(&self) -> WorkBudgetReport {
        WorkBudgetReport {
            limits: self.interp.work_budget(),
            execution: self.interp.work_budget_stats(),
            resources: self.config.resource_account.snapshot(),
        }
    }

    /// Snapshot the compact end-of-run diagnostics used by `OTTER_STATS=1`.
    pub fn execution_stats(&mut self) -> RuntimeExecutionStats {
        let ic = self.interp.property_ic_stats();
        let budget = self.interp.work_budget_stats();
        let jit = self.interp.jit_runtime_stats();
        let collection_method_ics = self.interp.jit_collection_method_ic_stats();
        let gc = self.interp.gc_heap_mut().gc_stats().clone();
        RuntimeExecutionStats {
            property_load_hits: ic.load_hits,
            property_load_misses: ic.load_misses,
            property_load_installs: ic.load_installs,
            property_load_disables: ic.load_disables,
            property_store_hits: ic.store_hits,
            property_store_misses: ic.store_misses,
            property_store_installs: ic.store_installs,
            property_store_disables: ic.store_disables,
            work_units_executed: budget.work_units_executed,
            regex_backtrack_steps: budget.regex_backtrack_steps,
            gc_work_units: budget.gc_work_units,
            forced_yields: budget.forced_yields,
            budget_rejections: budget.budget_rejections,
            bytecode_calls: budget.bytecode_calls,
            native_calls: budget.native_calls,
            construct_calls: budget.construct_calls,
            max_stack_depth_observed: budget.max_stack_depth_observed,
            max_turn_allocated_bytes: budget.max_turn_allocated_bytes,
            max_turn_nanos: budget.max_turn_nanos,
            jit_runtime_calls: jit.runtime_calls,
            jit_generated_calls: jit.generated_calls,
            jit_generated_call_deopts: jit.generated_call_deopts,
            jit_caller_invalidations: jit.caller_invalidations,
            jit_to_rust_call_transitions: jit.jit_to_rust_call_transitions,
            jit_generated_template_entries: jit.generated_template_entries,
            jit_generated_template_returns: jit.generated_template_returns,
            jit_generated_template_deopts: jit.generated_template_deopts,
            jit_generated_optimizing_entries: jit.generated_optimizing_entries,
            jit_generated_optimizing_returns: jit.generated_optimizing_returns,
            jit_generated_optimizing_deopts: jit.generated_optimizing_deopts,
            jit_optimized_entries: jit.optimized_entries,
            jit_optimized_osr_entries: jit.optimized_osr_entries,
            jit_optimized_deopts: jit.optimized_deopts,
            jit_compile_attempts: jit.compile_attempts,
            jit_code_generations: jit.code_generations,
            jit_osr_attempts: jit.osr_attempts,
            jit_runtime_property_stubs: jit.runtime_property_stubs,
            jit_runtime_stub_transitions: jit.runtime_stub_transitions,
            jit_leaf_stub_transitions: jit.leaf_stub_transitions,
            jit_alloc_stub_transitions: jit.alloc_stub_transitions,
            jit_reentrant_stub_transitions: jit.reentrant_stub_transitions,
            jit_alloc_value_stub_ok: jit.alloc_value_stub_ok,
            jit_alloc_value_stub_miss: jit.alloc_value_stub_miss,
            jit_alloc_value_stub_out_of_memory: jit.alloc_value_stub_out_of_memory,
            jit_alloc_value_stub_other: jit.alloc_value_stub_other,
            jit_receiver_alloc_guard_misses: jit.receiver_alloc_guard_misses,
            jit_receiver_alloc_space_misses: jit.receiver_alloc_space_misses,
            jit_receiver_alloc_canonical: jit.receiver_alloc_canonical,
            jit_receiver_alloc_deopts: jit.receiver_alloc_deopts,
            jit_collection_method_ic_slots: collection_method_ics.slots,
            jit_collection_method_ic_empty_slots: collection_method_ics.empty_slots,
            jit_collection_method_ic_collection_slots: collection_method_ics.collection_slots,
            jit_collection_method_ic_leaf_stub_slots: collection_method_ics.leaf_stub_slots,
            jit_collection_method_ic_alloc_stub_slots: collection_method_ics.alloc_stub_slots,
            gc_alloc_bytes_total: gc.alloc_bytes_total,
            gc_live_objects: gc.live_objects,
            gc_live_bytes: gc.live_bytes,
            gc_cycles: gc.gc_cycles,
            gc_last_pause_ms: gc.last_gc_pause_ms,
            gc_full_pause_ns_total: gc.full_pause_ns_total,
            gc_minor_cycles: gc.minor_gc_cycles,
            gc_minor_pause_ns_total: gc.minor_pause_ns_total,
            gc_minor_root_slots_scanned: gc.minor_root_slots_scanned,
            gc_minor_root_slots_peak: gc.minor_root_slots_peak,
            gc_minor_slot_updates: gc.minor_slot_updates,
            gc_minor_dirty_cards_scanned: gc.minor_dirty_cards_scanned,
            gc_minor_old_headers_walked: gc.minor_old_headers_walked,
            gc_minor_objects_retraced: gc.minor_objects_retraced,
            gc_minor_slots_scanned: gc.minor_slots_scanned,
        }
    }

    /// Snapshot executable code retained by this runtime's JIT caches.
    ///
    /// This is an explicit diagnostics/benchmark query and performs no
    /// accounting on ordinary execution paths.
    #[must_use]
    pub fn jit_code_residency(&self) -> otter_vm::JitCodeResidency {
        self.interp.jit_code_residency()
    }

    /// Snapshot every JIT code generation retained by this isolate.
    ///
    /// The owned result includes installed/invalid executable metadata and
    /// retired entry-cell tombstones. Calling it is explicit and does not add
    /// ordinary execution-path accounting.
    #[must_use]
    pub fn jit_code_generation_snapshot(&self) -> Vec<otter_vm::JitCodeGenerationSnapshot> {
        self.interp.jit_code_generation_snapshot()
    }

    pub(super) fn attach_execution_stats(&mut self, result: ExecutionResult) -> ExecutionResult {
        let result = if self.config.execution_stats {
            let stats = self.execution_stats();
            result.with_stats(stats)
        } else {
            result
        };
        self.attach_cpu_profile(result)
    }

    /// Hand the samples taken so far to `result`, leaving the profiler running.
    ///
    /// A run that only ends when its event loop drains keeps sampling long
    /// after entry evaluation returned, so the loop driver attaches again on
    /// the way out.
    pub(crate) fn attach_cpu_profile(&mut self, result: ExecutionResult) -> ExecutionResult {
        let result = match self.interp.drain_cpu_profile() {
            Some(profile) => result.with_cpu_profile(profile),
            None => result,
        };
        match self.interp.gc_heap_mut().drain_gc_pause_capture() {
            Some(pauses) => result.with_gc_pauses(pauses),
            None => result,
        }
    }

    pub(crate) fn attach_jit_debug_report(&mut self, result: ExecutionResult) -> ExecutionResult {
        let result = match self.interp.take_jit_debug_report() {
            Some(report) => result.with_jit_debug_report(report),
            None => result,
        };
        match self.interp.take_jit_artifacts() {
            Some(artifacts) => result.with_jit_artifacts(artifacts),
            None => result,
        }
    }

    pub(crate) fn finish_jit_debug_attempt(
        &mut self,
        result: Result<ExecutionResult, OtterError>,
    ) -> ExecutionAttempt {
        ExecutionAttempt::from_result(
            result,
            self.interp.take_jit_debug_report(),
            self.interp.take_jit_artifacts(),
        )
    }

    /// Drain the current structured JIT diagnostics batch.
    ///
    /// This remains available after an abrupt run that cannot return an
    /// [`ExecutionResult`].
    #[must_use]
    pub fn take_jit_debug_report(&mut self) -> Option<JitDebugReport> {
        self.interp.take_jit_debug_report()
    }

    /// Drain the current successful compile artifact batch.
    ///
    /// This remains available after abrupt execution and is independent from
    /// structured event capture.
    #[must_use]
    pub fn take_jit_artifacts(&mut self) -> Option<JitArtifactBatch> {
        self.interp.take_jit_artifacts()
    }

    /// Reset VM work-budget/resource counters.
    pub fn reset_work_budget_stats(&mut self) {
        self.interp.reset_work_budget_stats();
    }

    /// Snapshot every property inline-cache site. See
    /// [`inspect::IcSiteSnapshot`].
    #[must_use]
    pub fn ic_snapshot(&self) -> Vec<inspect::IcSiteSnapshot> {
        self.interp.ic_snapshot()
    }

    /// Snapshot the active hidden-class transition tree. See
    /// [`inspect::ShapeTransitionSnapshot`].
    #[must_use]
    pub fn shape_transition_snapshot(&self) -> inspect::ShapeTransitionSnapshot {
        self.interp.shape_transition_snapshot()
    }

    /// Install (or clear) the shape-transition observer used by
    /// shape-transition breakpoints. See
    /// [`inspect::ShapeTransitionObserver`].
    pub fn set_shape_transition_observer(
        &mut self,
        observer: Option<Box<dyn inspect::ShapeTransitionObserver>>,
    ) {
        self.interp.set_shape_transition_observer(observer);
    }

    /// Install (or clear) the per-instruction step tracer. See
    /// [`inspect::StepTracer`].
    pub fn set_tracer(&mut self, tracer: Option<Box<dyn inspect::StepTracer>>) {
        self.interp.set_tracer(tracer);
    }

    /// Enable VM stack sampling for CPU-profile artifacts, at most one
    /// sample per `interval` wall-clock microseconds.
    pub fn enable_cpu_profiler(&mut self, interval: u64) {
        self.interp.enable_cpu_profiler(interval);
    }

    /// Disable VM stack sampling without returning collected samples.
    pub fn disable_cpu_profiler(&mut self) {
        self.interp.disable_cpu_profiler();
    }

    /// Take and clear the current VM stack CPU profile.
    #[must_use]
    pub fn take_cpu_profile(&mut self) -> Option<otter_vm::CpuProfile> {
        self.interp.take_cpu_profile()
    }

    /// Type-count summary of every live GC body. See
    /// [`inspect::HeapSnapshotSummary`].
    #[must_use]
    pub fn heap_snapshot_summary(&mut self) -> inspect::HeapSnapshotSummary {
        self.interp.heap_snapshot_summary()
    }

    /// Per-space, per-type-tag census of the live heap. Separates old
    /// space (what a bootstrap snapshot would dump) from the nursery.
    #[must_use]
    pub fn heap_census(&mut self) -> otter_gc::HeapCensus {
        self.interp.heap_census()
    }

    /// Census of every live native callable by dispatch storage — static
    /// function pointers, VM intrinsics, and closure-backed natives retained by
    /// an in-process snapshot.
    #[must_use]
    pub fn native_census(&mut self) -> otter_vm::native_census::NativeCensus {
        self.interp.native_census()
    }

    /// Which live bodies still own GC references outside their own heap
    /// cell. A capture is only sound once this reports none.
    #[must_use]
    pub fn self_containment_audit(&mut self) -> otter_gc::SelfContainmentAudit {
        self.interp.self_containment_audit()
    }

    /// Inventory of the interpreter's GC root sources — which of them a
    /// finished build actually filled, and how much they hold.
    #[must_use]
    pub fn root_census(&self) -> otter_vm::root_census::RootCensus {
        self.interp.root_census()
    }

    /// Whether this runtime restored from an in-process snapshot instead of
    /// bootstrapping.
    #[must_use]
    pub fn restored_from_snapshot(&self) -> bool {
        self.restored_from_snapshot
    }

    /// Capture an in-process runtime image tied to this isolate's complete
    /// runtime configuration and resource account.
    ///
    /// # Errors
    /// Propagates [`otter_gc::ImageError`].
    pub fn capture_isolate_snapshot(&self) -> Result<RuntimeSnapshot, otter_gc::ImageError> {
        Ok(RuntimeSnapshot {
            isolate: self.interp.capture_isolate_snapshot()?,
            donor_config: self.config.clone(),
        })
    }

    /// Build, before the capture, what every launch would build first: the
    /// realm makes the program's `console` on its first read, and a run's
    /// entry reads it, so reading it here puts the console in the image.
    /// The build stays tenured, so the nursery the capture needs empty is.
    ///
    /// # Errors
    /// Returns [`OtterError`] when the console fails to build.
    pub fn prepare_startup_image(&mut self) -> Result<(), OtterError> {
        self.interp.gc_heap_mut().set_tenure_all(true);
        let global = otter_vm::Value::object(*self.interp.global_this());
        let built = otter_vm::NativeCtx::with_host_context(
            &mut self.interp,
            otter_vm::NativeCallInfo::call(global),
            None,
            |ctx| {
                ctx.scope(|mut scope| {
                    let global = scope.global_this();
                    let console = scope.get(global, "console")?;
                    Ok(scope.finish(console))
                })
            },
        );
        self.interp.gc_heap_mut().set_tenure_all(false);
        built.map(|_| ()).map_err(map_native_error)
    }

    /// This built runtime as a startup image: its heap, code and side state
    /// as bytes a later process of this same executable restores.
    ///
    /// # Errors
    /// Propagates [`otter_gc::ImageError`] from the capture; refuses an
    /// executable without a linker build identity, and a heap that calls
    /// natives closing over host state, which no launch could re-create.
    pub fn encode_startup_image(&self) -> Result<Vec<u8>, otter_gc::ImageError> {
        let fingerprint =
            surface_fingerprint(&self.config).ok_or(otter_gc::ImageError::NoBuildIdentity)?;
        let snapshot = self.interp.capture_isolate_snapshot()?;
        let natives = self.interp.startup_native_names(&snapshot);
        if !natives.is_empty() {
            return Err(otter_gc::ImageError::DynamicNatives {
                count: natives.len(),
            });
        }
        let mut out = otter_gc::image_codec::ImageWriter::new();
        out.u64(fingerprint);
        snapshot.encode(&mut out, &natives);
        Ok(out.into_bytes())
    }

    /// This realm's `globalThis` as a GC handle, for callers that need
    /// the object identity rather than a JS value.
    #[must_use]
    pub fn global_this_object(&self) -> otter_vm::object::JsObject {
        *self.interp.global_this()
    }

    /// Write a Chrome DevTools `.heapsnapshot` for the current heap
    /// state. The output is JSON; the DevTools "Memory" panel
    /// accepts it as-is.
    ///
    /// # Errors
    /// Propagates I/O errors from `writer`.
    pub fn write_chrome_heap_snapshot<W: std::io::Write>(
        &mut self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        self.interp.write_chrome_heap_snapshot(writer)
    }

    /// Force a full GC cycle (scavenge + old-gen mark-sweep).
    ///
    /// This explicit embedding hook is primarily for diagnostics and tests;
    /// ordinary collection remains allocation-pressure driven. The runtime's
    /// between-turn code-reclamation policy uses the same complete collection
    /// internally before its liveness census.
    ///
    /// The walker delegates to
    /// [`otter_vm::runtime_state::RuntimeState::trace_roots`]
    /// via [`otter_vm::Interpreter::force_gc`], which
    /// owns the heap and does the split-borrow internally.
    pub fn force_gc(&mut self) -> Result<(), OtterError> {
        self.interp.force_gc().map_err(Into::into)
    }

    /// Classic-script sources this runtime compiled: every
    /// [`Self::run_script`] compiles one, and a
    /// [`Self::run_script_cached`] that its cache served compiles none.
    #[must_use]
    pub fn scripts_compiled(&self) -> u64 {
        self.scripts_compiled.get()
    }

    /// Code chunks ever linked into this runtime: each script, eval body,
    /// module graph and lazily compiled function body is one.
    #[must_use]
    pub fn linked_code_chunks(&self) -> usize {
        self.interp.linked_code_chunks()
    }

    /// Current per-isolate dynamic-code reclamation counters.
    #[must_use]
    pub fn code_eviction_stats(&self) -> CodeEvictionStats {
        self.interp.code_eviction_stats()
    }

    /// Configure the retained-byte high-water mark checked between turns.
    ///
    /// A zero threshold is useful for deterministic embedding tests. Normal
    /// runtimes use [`Interpreter::DEFAULT_CODE_EVICTION_HIGH_WATER_BYTES`].
    pub fn set_code_eviction_high_water_bytes(&mut self, bytes: u64) {
        self.interp.set_code_eviction_high_water_bytes(bytes);
    }

    /// Run the between-turn liveness census and reclaim eligible dynamic code
    /// until the configured retained-byte target is met.
    ///
    /// # Errors
    /// Returns an out-of-memory error if the full collection required for the
    /// census cannot complete.
    pub fn reclaim_dynamic_code(&mut self) -> Result<(), OtterError> {
        self.interp.reclaim_dynamic_code().map_err(Into::into)
    }

    /// Configured stack-depth cap.
    #[must_use]
    pub fn max_stack_depth(&self) -> u32 {
        self.config.max_stack_depth
    }

    /// Borrow the capability set.
    #[must_use]
    pub fn capabilities(&self) -> &CapabilitySet {
        &self.config.capabilities
    }

    /// Check a runtime capability request through the configured hook policy.
    #[must_use]
    pub fn check_capability(
        &self,
        capability: RuntimeCapability,
        request: &CapabilityRequest<'_>,
    ) -> bool {
        hooks::check_capability_with_hooks(
            &self.config.hooks,
            &self.config.capabilities,
            capability,
            request,
        )
    }

    /// Emit a structured diagnostic through the configured runtime hook.
    pub fn emit_diagnostic(&self, diagnostic: &Diagnostic) {
        self.diagnostics.emit(&self.config.hooks, diagnostic);
    }

    /// Map a `(module_url, function_id, pc)` triple back to the
    /// original source byte range using the runtime-owned source
    /// map table.
    ///
    /// Populated incrementally as the runtime compiles modules /
    /// scripts. Returns the predecessor entry's span when the
    /// table has no exact PC match — matches the VM's frame
    /// snapshot policy in
    /// [`otter_vm::snapshot_frames`].
    ///
    /// Returns `None` when the module URL has no compiled
    /// metadata yet, when the function id is unknown for that
    /// module, or when the function's span table is empty.
    #[must_use]
    pub fn resolve_frame_span(
        &self,
        module_url: &str,
        function_id: u32,
        pc: u32,
    ) -> Option<(u32, u32)> {
        self.source_maps
            .resolve_frame_span(module_url, function_id, pc)
    }
}
