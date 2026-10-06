//! Top-level execution drivers: `run`, microtask drain, dispatch entry.
//!
//! # Contents
//! `run`/`run_inner`, pinned and reclaimable module linking, between-turn code
//! reclamation, GC-heap accessors and `force_gc`, microtask drain (with
//! per-task origin contexts) and capability settlement, and the
//! activation-root / `dispatch_loop` shells.
//!
//! # Invariants
//! Drains that run outside `run`'s rooted scope must push the
//! interpreter's extra roots before touching the heap.
//! Each ordinary microtask publishes one activation stack for native or
//! bytecode execution, and nested dispatch releases back to that task's floor.
//! Async reaction result promises remain in traced anchors across parking and
//! downstream settlement.
//! Linked bytecode chunks retain the scalar identity of their creation realm;
//! no moving-GC handle is stored in function metadata.
//! Dynamic-code reclamation runs only with no active native/materialized
//! activation, completes a full collection, retires generated code, and only
//! then tombstones a payload proven free of ids and retained contexts.
//! Successful payload eviction purges function-, chunk- and constant-keyed
//! caches across active and parked realms. Constructor pairs retire when either
//! function is evicted; shared shape/argument-count caches retain their owners.
#![allow(unused_imports)]
use super::call_dispatch::DispatchOutcome;
use crate::activation_stack::ThrowSite;
use crate::*;

impl Interpreter {
    /// Default retained-byte high-water mark for reclaimable dynamic code.
    pub const DEFAULT_CODE_EVICTION_HIGH_WATER_BYTES: u64 = 16 * 1024 * 1024;

    /// Current code-chunk reclamation counters.
    #[must_use]
    pub fn code_eviction_stats(&self) -> CodeEvictionStats {
        let mut stats = self.code_eviction_stats;
        stats.retained_bytes = self.code_space.evictable_retained_bytes();
        stats.peak_retained_bytes = stats.peak_retained_bytes.max(stats.retained_bytes);
        stats
    }

    /// Configure the byte high-water mark checked at between-turn entry.
    pub fn set_code_eviction_high_water_bytes(&mut self, bytes: u64) {
        self.code_eviction_high_water_bytes = bytes;
    }

    /// Run a full-GC liveness census and reclaim dead dynamic chunks until the
    /// configured retained-byte target is met.
    pub fn reclaim_dynamic_code(&mut self) -> Result<(), otter_gc::OutOfMemory> {
        self.reclaim_dynamic_code_to(self.code_eviction_high_water_bytes)
    }

    fn reclaim_dynamic_code_to(&mut self, target_bytes: u64) -> Result<(), otter_gc::OutOfMemory> {
        let mut retained = self.code_space.evictable_retained_bytes();
        self.code_eviction_stats.retained_bytes = retained;
        self.code_eviction_stats.peak_retained_bytes =
            self.code_eviction_stats.peak_retained_bytes.max(retained);
        if retained <= target_bytes {
            return Ok(());
        }
        debug_assert_eq!(self.sync_reentry_depth, 0);
        debug_assert!(!self.jit_has_native_frames());

        self.code_eviction_stats.census_passes =
            self.code_eviction_stats.census_passes.saturating_add(1);
        self.force_gc_with_trigger(otter_gc::GcPauseTrigger::CodeReclamation)?;
        let candidates = self.code_space.eviction_candidates();
        let liveness = crate::code_liveness::census_candidate_ids(self, candidates.clone());
        let mut retired_jit = false;
        for candidate in candidates {
            if retained <= target_bytes {
                break;
            }
            if liveness.is_live(candidate) {
                self.code_eviction_stats.live_id_skips =
                    self.code_eviction_stats.live_id_skips.saturating_add(1);
                continue;
            }
            if !retired_jit {
                self.retire_jit_for_code_eviction();
                retired_jit = true;
            }
            if let crate::code_space::ChunkEvictionResult::Evicted { retained_bytes } =
                self.code_space.evict_candidate(candidate)
            {
                self.purge_chunk_side_tables(candidate);
                retained = retained.saturating_sub(retained_bytes);
                self.code_eviction_stats.evicted_chunks =
                    self.code_eviction_stats.evicted_chunks.saturating_add(1);
                self.code_eviction_stats.evicted_bytes = self
                    .code_eviction_stats
                    .evicted_bytes
                    .saturating_add(retained_bytes);
            }
        }
        self.code_eviction_stats.retained_bytes = self.code_space.evictable_retained_bytes();
        Ok(())
    }

    fn retire_jit_for_code_eviction(&mut self) {
        let invalidated = self.jit_code_registry.invalidate_all().len() as u64;
        self.jit_runtime_stats.caller_invalidations = self
            .jit_runtime_stats
            .caller_invalidations
            .saturating_add(invalidated);
        self.jit_code.clear();
        self.jit_optimized_code.clear();
        self.jit_code_cache = None;
        self.jit_optimized_code_cache = None;
        self.jit_template_osr_fids.clear();
        self.jit_entry_osr_only.clear();
        self.jit_template_compiling.clear();
        self.retire_unreferenced_jit_code();
    }

    fn purge_chunk_side_tables(&mut self, candidate: crate::code_space::ChunkEvictionCandidate) {
        let start = candidate.function_base;
        let end = candidate.function_end();
        let outside = |function_id: &u32| *function_id < start || *function_id >= end;
        self.template_objects
            .retain(|(function_base, _), _| *function_base != start);
        self.string_constant_cells
            .retain(|(identity, _), _| *identity != candidate.module_identity);
        self.bigint_constant_cache
            .retain(|(identity, _), _| *identity != candidate.module_identity);
        self.simple_constructor_init_cache
            .retain(|id, _| outside(id));
        self.simple_constructor_absence.retain(|id, _| outside(id));
        self.simple_constructor_shape_cache
            .retain(|(id, _), _| outside(id));
        self.object_literal_layouts.retain(|(id, _), _| outside(id));
        self.prune_constructor_layouts_for_evicted_range(start, end);
        self.global_lexical_load_ic.retain(|(id, _), _| outside(id));
        self.global_object_load_ic.retain(|(id, _), _| outside(id));
        for realm in &mut self.extra_realms {
            realm
                .template_objects
                .retain(|(function_base, _), _| *function_base != start);
            realm
                .global_lexical_load_ic
                .retain(|(id, _), _| outside(id));
            realm.global_object_load_ic.retain(|(id, _), _| outside(id));
        }
        self.function_realm_ids.retain(|id, _| outside(id));
        self.function_user_props.retain(|id, _| outside(id));
        self.function_prototype_overrides
            .retain(|id, _| outside(id));
        self.function_prototype_slots.retain(|id, _| outside(id));
        self.function_non_extensible.retain(outside);
        self.function_deleted_metadata.retain(|(id, _)| outside(id));
        self.jit_optimized_exit_profiles
            .retain(|(id, _, _), _| outside(id));
        self.jit_parameter_widening.retain(|id, _| outside(id));
        self.jit_entry_bail_counts.retain(|id, _| outside(id));
        self.jit_osr_disabled.retain(|(id, _)| outside(id));
        self.jit_optimized_declined_epoch
            .retain(|id, _| outside(id));
        self.optimizing_tier_policy.evict_function_range(start, end);
        self.method_feedback.evict_site_range(
            candidate.property_ic_site_base,
            candidate.property_ic_site_end,
        );
    }

    /// Borrow the per-isolate GC heap (read-only).
    #[must_use]
    pub fn gc_heap(&self) -> &otter_gc::GcHeap {
        &self.gc_heap
    }

    /// Mutable borrow of the per-isolate GC heap.
    #[must_use]
    pub fn gc_heap_mut(&mut self) -> &mut otter_gc::GcHeap {
        &mut self.gc_heap
    }

    /// Snapshot aggregate collector counters without exposing mutable heap
    /// authority to diagnostics and benchmark clients.
    #[must_use]
    pub fn gc_stats_snapshot(&mut self) -> otter_gc::GcStats {
        self.gc_heap.gc_stats().clone()
    }

    /// `pub(crate)` alias used by [`crate::runtime_cx::RuntimeTurn`]
    /// to forward the heap borrow without rebinding through a
    /// public method. Tracks the explicit-context migration in
    /// runtime-turn boundary.
    #[must_use]
    pub(crate) fn gc_heap_for_cx(&self) -> &otter_gc::GcHeap {
        &self.gc_heap
    }

    /// `pub(crate)` mutable alias — see [`Self::gc_heap_for_cx`].
    #[must_use]
    pub(crate) fn gc_heap_for_cx_mut(&mut self) -> &mut otter_gc::GcHeap {
        &mut self.gc_heap
    }

    /// Force a full GC cycle. Runtime-owned roots are supplied through the
    /// heap's [`otter_gc::ExtraRoots`] callback so explicit GC and
    /// allocation-triggered GC use the same root walk.
    ///
    /// **Debug / test only** — production embedders let the GC
    /// trigger itself.
    pub fn force_gc(&mut self) -> Result<(), otter_gc::OutOfMemory> {
        self.force_gc_with_trigger(otter_gc::GcPauseTrigger::Explicit)
    }

    /// One rooted full-collection owner for explicit and code-reclamation entry.
    fn force_gc_with_trigger(
        &mut self,
        trigger: otter_gc::GcPauseTrigger,
    ) -> Result<(), otter_gc::OutOfMemory> {
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let _extra_roots_guard = self.gc_heap.register_extra_roots(extra_roots);
        let mut noop = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        self.gc_heap.collect_full_observed(&mut noop, trigger)?;
        self.enqueue_finalization_cleanup();
        Ok(())
    }

    /// Turn finalization cells emptied by any collection since the last
    /// checkpoint into `FinalizationCallback` microtasks.
    ///
    /// Runs at job checkpoints and after explicit collections. The removed
    /// cells' callbacks and held values move straight into the traced
    /// microtask queue with no allocation in between.
    pub(crate) fn enqueue_finalization_cleanup(&mut self) {
        for job in crate::weak_refs::take_finalization_jobs(&mut self.gc_heap) {
            let mut args = SmallVec::new();
            args.push(job.held_value);
            self.microtasks.enqueue(Microtask {
                callee: job.cleanup_callback,
                this_value: Value::undefined(),
                args,
                context: job.context,
                realm_id: job.realm_id,
                result_capability: None,
                kind: MicrotaskKind::FinalizationCallback,
                async_context: job.async_context,
            });
        }
    }

    /// Link a freshly compiled module into this interpreter's code
    /// space. Rebases the module's function ids onto the global id
    /// space so function values created by this chunk stay callable
    /// after they escape to frames executing other chunks (the
    /// `eval` / `new Function` / dynamic-import escape paths), and
    /// resolves the chunk's string constants to this isolate's global
    /// property-name atoms.
    ///
    /// # Errors
    /// Returns [`crate::BytecodeLinkError`] before publishing any code when the
    /// module is malformed or the interpreter-wide id space is exhausted.
    pub fn link_module(
        &mut self,
        module: otter_bytecode::BytecodeModule,
        sources: crate::source_registry::SourceRegistry,
    ) -> Result<ExecutionContext, crate::BytecodeLinkError> {
        let function_count = u32::try_from(module.functions.len()).map_err(|_| {
            crate::BytecodeLinkError::FunctionIdCapacity {
                base: 0,
                function_count: module.functions.len(),
            }
        })?;
        let context = self
            .code_space
            .link_module(module, sources, &self.resource_account)?;
        self.finish_linked_module(context, function_count)
    }

    /// Link an eval or on-demand module chunk that may be reclaimed after a
    /// between-turn liveness census proves its ids and payload unreachable.
    pub fn link_evictable_module(
        &mut self,
        module: otter_bytecode::BytecodeModule,
        sources: crate::source_registry::SourceRegistry,
    ) -> Result<ExecutionContext, crate::BytecodeLinkError> {
        let function_count = u32::try_from(module.functions.len()).map_err(|_| {
            crate::BytecodeLinkError::FunctionIdCapacity {
                base: 0,
                function_count: module.functions.len(),
            }
        })?;
        let context =
            self.code_space
                .link_evictable_module(module, sources, &self.resource_account)?;
        let context = self.finish_linked_module(context, function_count)?;
        let retained = self.code_space.evictable_retained_bytes();
        self.code_eviction_stats.retained_bytes = retained;
        self.code_eviction_stats.peak_retained_bytes =
            self.code_eviction_stats.peak_retained_bytes.max(retained);
        Ok(context)
    }

    /// [`Self::link_evictable_module`] for a module whose verification the
    /// host already holds: its wordcode is not inspected again.
    pub fn link_evictable_verified_module(
        &mut self,
        module: otter_bytecode::VerifiedBytecodeModule,
        sources: crate::source_registry::SourceRegistry,
    ) -> Result<ExecutionContext, crate::BytecodeLinkError> {
        let function_count = u32::try_from(module.module().functions.len()).map_err(|_| {
            crate::BytecodeLinkError::FunctionIdCapacity {
                base: 0,
                function_count: module.module().functions.len(),
            }
        })?;
        let context = self.code_space.link_evictable_verified_module(
            module,
            sources,
            &self.resource_account,
        )?;
        let context = self.finish_linked_module(context, function_count)?;
        let retained = self.code_space.evictable_retained_bytes();
        self.code_eviction_stats.retained_bytes = retained;
        self.code_eviction_stats.peak_retained_bytes =
            self.code_eviction_stats.peak_retained_bytes.max(retained);
        Ok(context)
    }

    /// Link a decoded or cached module while retaining its mandatory
    /// verification proof through code-space rebasing and executable building.
    ///
    /// # Errors
    /// Returns [`crate::BytecodeLinkError`] before publishing any code when the
    /// interpreter-wide id space is exhausted or the retained proof cannot be
    /// rebased.
    pub fn link_verified_module(
        &mut self,
        module: otter_bytecode::VerifiedBytecodeModule,
        sources: crate::source_registry::SourceRegistry,
    ) -> Result<ExecutionContext, crate::BytecodeLinkError> {
        let function_count = u32::try_from(module.module().functions.len()).map_err(|_| {
            crate::BytecodeLinkError::FunctionIdCapacity {
                base: 0,
                function_count: module.module().functions.len(),
            }
        })?;
        let context =
            self.code_space
                .link_verified_module(module, sources, &self.resource_account)?;
        self.finish_linked_module(context, function_count)
    }

    fn finish_linked_module(
        &mut self,
        context: ExecutionContext,
        function_count: u32,
    ) -> Result<ExecutionContext, crate::BytecodeLinkError> {
        context.resolve_atoms(&self.names);
        for function in context.linked_functions() {
            self.jit_code_registry
                .link_function(function, self.active_realm_id);
        }
        if self.active_realm_id != 0 {
            let base = context.function_base();
            let end = base.checked_add(function_count).ok_or(
                crate::BytecodeLinkError::FunctionIdCapacity {
                    base,
                    function_count: function_count as usize,
                },
            )?;
            for function_id in base..end {
                self.function_realm_ids
                    .insert(function_id, self.active_realm_id);
            }
        }
        Ok(context)
    }

    /// Execute `<main>` of `module` and return its completion value.
    ///
    /// # Errors
    /// Returns [`RunError`] (a `VmError` plus a stack-frame
    /// snapshot) on bytecode malformation, type mismatch, OOM,
    /// interrupt, or stack overflow.
    pub fn run(&mut self, context: &ExecutionContext) -> Result<Value, RunError> {
        // Adopt the entry chunk's code space so chunks linked during
        // this run (eval / new Function bodies) land in the same
        // function-id space as the running script. No-op for contexts
        // produced by `link_module`.
        //
        // The adopted chunks' property-name atoms were minted by whatever
        // interner linked them, which is not this isolate's. Re-resolve the
        // whole space so every atom id this run compares — against shape
        // nodes, IC guards, transition records — comes from `self.names`.
        if !std::sync::Arc::ptr_eq(&self.code_space, context.space()) {
            self.code_space = std::sync::Arc::clone(context.space());
            self.code_space.resolve_atoms(&self.names);
            for function in context.linked_functions() {
                let realm_id = self.function_realm_id(function.id);
                self.jit_code_registry.link_function(function, realm_id);
            }
        }
        // Retain the explicit realm host-script context. Queued jobs never
        // consult this field: they carry source admission or resolve their own
        // bytecode FunctionID through CodeSpace.
        self.realm_context = Some(context.clone());
        if let Err(error) = self.reclaim_dynamic_code() {
            return Err(RunError {
                error: crate::oom_to_vm(error),
                frames: Vec::new(),
                detail: self.take_error_detail(),
            });
        }
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let _extra_roots_guard = self.gc_heap.register_extra_roots(extra_roots);
        self.pending_uncaught_throw = None;
        self.clear_throw_provenance();
        self.ensure_method_feedback_context(context);
        match self.run_inner(context) {
            Ok(v) => Ok(v),
            Err((error, frames)) => Err(RunError {
                error,
                frames,
                detail: self.take_error_detail(),
            }),
        }
    }

    /// Drain record-owned jobs until quiescent. An escaping failure stops this
    /// checkpoint; remaining jobs stay traced for a deliberate later turn.
    pub fn drain_microtasks(
        &mut self,
        mut report: impl FnMut(&mut NativeCtx<'_>, &RunError) -> Result<bool, NativeError>,
    ) -> Result<(), RunError> {
        if self.microtasks.begin_drain().is_none() {
            return Ok(());
        }
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let _extra_roots_guard = self.gc_heap.register_extra_roots(extra_roots);
        self.begin_work_budget_turn();
        let result = self.drain_microtasks_inner(&mut report);
        self.microtasks.end_drain();
        self.finish_work_budget_turn();
        result
    }

    pub(crate) fn drain_microtasks_inner(
        &mut self,
        report: &mut impl FnMut(&mut NativeCtx<'_>, &RunError) -> Result<bool, NativeError>,
    ) -> Result<(), RunError> {
        loop {
            self.enqueue_finalization_cleanup();
            self.drain_microtask_generations_inner(report)?;
            self.run_all_promise_rejection_checkpoints(report)?;
            if !self.microtasks.has_any_pending() {
                return Ok(());
            }
        }
    }

    fn drain_microtask_generations_inner(
        &mut self,
        report: &mut impl FnMut(&mut NativeCtx<'_>, &RunError) -> Result<bool, NativeError>,
    ) -> Result<(), RunError> {
        self.record_runtime_microtask_drain_started();
        loop {
            if !self.microtasks.has_in_flight() {
                if !self.microtasks.has_pending_sync() {
                    return Ok(());
                }
                self.microtasks.next_generation();
            }
            // Waiting jobs stay in the queue's traced owner through every
            // budget checkpoint and every predecessor's collecting call.
            while self.microtasks.has_in_flight() {
                if let Err(error) = self.enforce_work_budget_checkpoint() {
                    return Err(RunError {
                        error,
                        frames: Vec::new(),
                        detail: self.take_error_detail(),
                    });
                }
                let task = self
                    .microtasks
                    .next_in_flight()
                    .expect("checked queued job");
                if !self.job_realm_is_live(task.realm_id) {
                    // Dropping the existing record releases its parked state
                    // and source lease. Disposed jobs do not settle or reenter.
                    continue;
                }
                self.record_runtime_microtask_executed();
                let realm_id = task.realm_id;
                let outcome = self.with_handle_scope(|vm, scope| {
                    let ambient = vm.async_context();
                    let ambient = vm.scoped_value(scope, ambient);
                    vm.set_async_context(task.async_context);
                    // Capture RunError and its source diagnostics inside the
                    // owning realm; the realm boundary itself carries VmError.
                    let outcome = vm.with_host_realm_id(realm_id, |vm| {
                        let context = task.context.clone();
                        let outcome = vm.invoke_microtask(context.as_ref(), task);
                        Ok(match outcome {
                            Ok(()) => Ok(()),
                            Err(error) => {
                                vm.report_microtask_failure(context.as_ref(), error, report)
                            }
                        })
                    });
                    vm.set_async_context(vm.escape_scoped(ambient));
                    match outcome {
                        Ok(outcome) => outcome,
                        Err(error) => Err(RunError {
                            error,
                            frames: Vec::new(),
                            detail: vm.take_error_detail(),
                        }),
                    }
                });
                if let Err(error) = outcome {
                    return Err(error);
                }
            }
            if !self.microtasks.has_any_pending() {
                return Ok(());
            }
        }
    }

    /// Offer one catchable failure before restoring its job realm/async
    /// context. Fatal/control and escaping allocation failure never invoke a
    /// reporter. The original exception is a scoped root throughout reporter
    /// allocation; owned diagnostics return unchanged on an unhandled throw or
    /// catchable reporter failure. An actual fatal reporter failure replaces
    /// it with its own cause and provenance.
    pub(crate) fn report_microtask_failure(
        &mut self,
        context: Option<&ExecutionContext>,
        error: RunError,
        report: &mut impl FnMut(&mut NativeCtx<'_>, &RunError) -> Result<bool, NativeError>,
    ) -> Result<(), RunError> {
        if error.error.is_fatal() || matches!(error.error, VmError::OutOfMemory { .. }) {
            return Err(error);
        }
        self.with_handle_scope(|vm, scope| {
            let thrown = vm
                .take_pending_uncaught_throw()
                .map(|value| vm.scoped_value(scope, value));
            let from_rejection = vm.take_uncaught_from_promise_rejection();
            if let Some(thrown) = thrown {
                vm.set_pending_uncaught_throw(vm.escape_scoped(thrown));
            }
            vm.uncaught_from_promise_rejection = from_rejection;
            let outcome =
                NativeCtx::with_host_context(vm, NativeCallInfo::default_call(), context, |ctx| {
                    report(ctx, &error)
                });
            match outcome {
                Ok(true) => {
                    let _ = vm.take_pending_uncaught_throw();
                    let _ = vm.take_error_detail();
                    vm.clear_throw_provenance();
                    vm.uncaught_from_promise_rejection = false;
                    Ok(())
                }
                Err(native) => {
                    if native.is_fatal() || matches!(native, NativeError::OutOfMemory { .. }) {
                        // A fatal reporter does not throw the original job
                        // exception. Its owned imported failure installs its
                        // own diagnostics at the canonical projection.
                        let _ = vm.take_pending_uncaught_throw();
                        vm.uncaught_from_promise_rejection = false;
                    }
                    let cause = crate::native_to_vm_error(vm, native);
                    if cause.is_fatal() || matches!(cause, VmError::OutOfMemory { .. }) {
                        return Err(RunError {
                            error: cause,
                            frames: vm.take_uncaught_frames(),
                            detail: vm.take_error_detail(),
                        });
                    }
                    // Catchable reporting failure is subordinate to the
                    // original job failure; it never resumes this drain.
                    let _ = vm.take_pending_uncaught_throw();
                    if let Some(thrown) = thrown {
                        vm.set_pending_uncaught_throw(vm.escape_scoped(thrown));
                    }
                    *vm.pending_error_detail.borrow_mut() = error.detail.clone();
                    vm.set_uncaught_frames(error.frames.clone());
                    vm.uncaught_from_promise_rejection = from_rejection;
                    Err(error)
                }
                Ok(false) => {
                    let _ = vm.take_pending_uncaught_throw();
                    if let Some(thrown) = thrown {
                        vm.set_pending_uncaught_throw(vm.escape_scoped(thrown));
                    }
                    *vm.pending_error_detail.borrow_mut() = error.detail.clone();
                    vm.set_uncaught_frames(error.frames.clone());
                    vm.uncaught_from_promise_rejection = from_rejection;
                    Err(error)
                }
            }
        })
    }

    /// Invoke one microtask top-level. Builds a fresh frame stack
    /// containing just the task's callee; runs `dispatch_loop`
    /// until it returns. Errors include the snapshot of frames
    /// the task accumulated when it failed.
    pub(crate) fn invoke_microtask(
        &mut self,
        context: Option<&ExecutionContext>,
        task: Microtask,
    ) -> Result<(), RunError> {
        // Reaction-mode rejection forwarding (§27.2.1.3.2) reads the
        // abrupt completion's [[Value]] from `pending_uncaught_throw`
        // after `dispatch_loop` returns. Clear any stale payload
        // carried over from a prior microtask so we cannot read a
        // foreign reaction's value into this one.
        self.pending_uncaught_throw = None;
        self.clear_throw_provenance();
        self.uncaught_from_promise_rejection = false;
        let _ = self.take_error_detail();
        // Async-resume tasks bypass callee resolution entirely:
        // the parked frame replaces a fresh callee invocation,
        // so route them to `run_async_resume` directly.
        if let MicrotaskKind::AsyncResume {
            frame,
            cold,
            await_dst,
            fulfilled,
        } = task.kind
        {
            let value = task.args.into_iter().next().unwrap_or(Value::undefined());
            let context = self
                .function_context(context, frame.header.function_id)
                .map_err(RunError::bare)?;
            return self.run_async_resume(&context, frame, cold, await_dst, fulfilled, value);
        }
        if let MicrotaskKind::AsyncGenResume {
            frame,
            cold,
            await_dst,
            fulfilled,
            owner,
        } = task.kind
        {
            let value = task.args.into_iter().next().unwrap_or(Value::undefined());
            let context = self
                .function_context(context, frame.header.function_id)
                .map_err(RunError::bare)?;
            return self
                .run_async_gen_resume(&context, frame, cold, await_dst, fulfilled, value, owner);
        }
        // Resolve callee → function_id + SELF. Mirrors the
        // unwrap loop inside `invoke`, but for a top-level call
        // (no caller frame to write back into).
        let mut result_capability = task.result_capability.clone();
        let mut current = task.callee;
        let mut effective_this = task.this_value;
        let mut effective_args: SmallVec<[Value; 8]> = task.args.into_iter().collect();
        // The task left the (traced) microtask queue; from here until
        // the callee's own roots take over, these locals are the only
        // owners of the callee/this/argument values. Everything below
        // allocates before frame roots exist — the `this` box,
        // bound-function unwrapping — and a moving
        // scavenge in any of those would otherwise launder the
        // argument values into foreign heap words. Register a live
        // root over the locals for the whole invocation.
        let locals_root = MicrotaskLocalsRoot {
            current: &raw const current,
            this_value: &raw const effective_this,
            args: &raw const effective_args,
            result_capability: &raw const result_capability,
        };
        let _locals_guard = self
            .gc_heap
            .register_extra_roots(otter_gc::ExtraRoots::new(&locals_root));
        let mut stack = ActivationStack::new();
        let floor = stack.floor();
        self.with_runtime_turn(&mut stack, |turn| {
            let (interp, stack) = turn.into_parts();
            let result = interp.invoke_microtask_rooted(
                context,
                &mut result_capability,
                &mut current,
                &mut effective_this,
                &mut effective_args,
                stack,
            );
            interp.release_frames_above(stack, floor);
            result
        })
    }

    /// Body of [`Self::invoke_microtask`] running under the
    /// locals-root registration (see there). The job's callable enters the
    /// classifying trampoline like any other call; an async handler completes
    /// with its result promise, which the downstream capability adopts.
    fn invoke_microtask_rooted(
        &mut self,
        context: Option<&ExecutionContext>,
        result_capability: &mut Option<crate::microtask::MicrotaskCapability>,
        current: &mut Value,
        effective_this: &mut Value,
        effective_args: &mut SmallVec<[Value; 8]>,
        stack: &mut ActivationStack,
    ) -> Result<(), RunError> {
        if !stack.is_runtime_rooted_by(self) {
            return Err(RunError::bare(VmError::InvalidOperand));
        }
        let callee = *current;
        let this_value = *effective_this;
        let args = std::mem::take(effective_args);
        match self.run_callable_sync_rooted(stack, context, &callee, this_value, args) {
            Ok(value) => self.settle_microtask_capability(
                context,
                stack,
                result_capability.take(),
                Ok(value),
            ),
            Err(error) => {
                if result_capability.is_some()
                    && !error.is_fatal()
                    && !matches!(error, VmError::OutOfMemory { .. })
                {
                    // §27.2.1.3.2 step 1.f.iii: the rejection carries the
                    // original thrown value when one was preserved.
                    let reason = self
                        .vm_error_to_throwable_with_stack_roots(context, stack, &error)
                        .map_err(|error| RunError {
                            error,
                            frames: self.take_uncaught_frames_or_snapshot(context),
                            detail: self.take_error_detail(),
                        })?;
                    self.settle_microtask_capability(
                        context,
                        stack,
                        result_capability.take(),
                        Err(reason),
                    )
                } else {
                    let frames = self.take_uncaught_frames_or_snapshot(context);
                    Err(RunError {
                        error,
                        frames,
                        detail: self.take_error_detail(),
                    })
                }
            }
        }
    }

    /// Resolve / reject the downstream promise that a reaction
    /// job belongs to. No-op when `cap` is `None` (plain
    /// `queueMicrotask` callbacks).
    pub(crate) fn settle_microtask_capability(
        &mut self,
        context: Option<&ExecutionContext>,
        stack: &mut ActivationStack,
        cap: Option<microtask::MicrotaskCapability>,
        outcome: Result<Value, Value>,
    ) -> Result<(), RunError> {
        let Some(cap) = cap else {
            return Ok(());
        };
        let (callee, args): (Value, SmallVec<[Value; 8]>) = match outcome {
            Ok(v) => (cap.resolve, smallvec::smallvec![v]),
            Err(reason) => (cap.reject, smallvec::smallvec![reason]),
        };
        // §27.2.2.1 PromiseReactionJob step 2 — the capability's resolve /
        // reject runs INSIDE this job, so the downstream settles before any
        // job that was already queued behind this one. A deferred call here
        // adds an observable tick and reorders `Promise.race` winners.
        self.run_callable_sync_rooted(stack, context, &callee, Value::undefined(), args)
            .map(|_| ())
            .map_err(|error| RunError {
                error,
                frames: self.take_uncaught_frames_or_snapshot(context),
                detail: self.take_error_detail(),
            })
    }

    /// Internal driver. Pulls the snapshot capture out of the
    /// dispatch loop so the hot path remains allocation-free; the
    /// snapshot is built only when a `VmError` actually escapes.
    pub(crate) fn run_inner(
        &mut self,
        context: &ExecutionContext,
    ) -> Result<Value, (VmError, Vec<StackFrameSnapshot>)> {
        let main = context.exec_main();
        let mut stack: ActivationStack = ActivationStack::new();
        // The script `<main>` closes over no context.
        let self_value = crate::closure::alloc_closure(
            &mut self.gc_heap,
            main.id,
            Value::undefined(),
            None,
            None,
        )
        .map(Value::closure)
        .map_err(|oom| (VmError::from(oom), Vec::new()))?;
        let entry_this = if main.is_module {
            Value::undefined()
        } else {
            Value::object(self.global_this)
        };
        let entry = crate::PreparedCall::for_code_block(main, None, self_value, entry_this);
        let entry_is_async = main.is_async;
        stack.push(entry);
        // §16.2.1.7 ModuleDeclarationInstantiation step 5 — when the
        // entry function carries top-level await, wire up an async
        // result promise so `Op::Await` can park / resume normally.
        // The dispatch loop's exit returns the result promise's
        // resolved value once microtasks drain.
        let entry_promise = if entry_is_async {
            let result = promise_dispatch::PromiseBuilder::with_context(Some(context.clone()))
                .pending_stack_rooted(self, &stack, &[], &[])
                .map_err(|oom| (VmError::from(oom), Vec::new()))?;
            let frame = stack.pending_mut().expect("entry inputs were just queued");
            self.prepared_set_async_state(
                frame,
                AsyncFrameState {
                    result_promise: result,
                },
            );
            Some(result)
        } else {
            None
        };

        // Park the entry promise on the scratch root stack: once the
        // async entry frame settles and is popped, nothing else roots
        // the handle, so the microtask drain's allocations would leave
        // a bare local pointing at the promise body's vacated slot.
        let entry_promise_root = entry_promise.map(|p| self.json_root_push(Value::promise(p)));

        let dispatch_result = self.dispatch_loop(context, &mut stack);
        match dispatch_result {
            Ok(value) => {
                if let Some(root_idx) = entry_promise_root {
                    // Drain microtasks until the entry promise
                    // settles. The settled value (or rejection)
                    // becomes the program's completion value.
                    if let Err(err) = self.drain_microtasks(|_, _| Ok(false)) {
                        self.json_root_pop_to(root_idx);
                        // The drain already owns the exact completed detail.
                        // run_inner returns the existing scalar/frames tuple;
                        // run takes this sole pending owner into its RunError.
                        *self.pending_error_detail.borrow_mut() = err.detail;
                        return Err((err.error, err.frames));
                    }
                    let promise = self
                        .json_root_get(root_idx)
                        .as_promise()
                        .expect("entry promise stays a promise across the drain");
                    let state = promise.state(&self.gc_heap);
                    self.json_root_pop_to(root_idx);
                    match state {
                        crate::promise::PromiseState::Fulfilled(v) => return Ok(v),
                        crate::promise::PromiseState::Rejected(reason) => {
                            return Err((
                                self.err_uncaught((self.render_thrown(&reason)).into()),
                                Vec::new(),
                            ));
                        }
                        crate::promise::PromiseState::Pending => return Ok(Value::undefined()),
                    }
                }
                Ok(value)
            }
            Err(err) => {
                if let Some(root_idx) = entry_promise_root {
                    self.json_root_pop_to(root_idx);
                }
                let frames = self.take_uncaught_frames_or_snapshot(Some(context));
                Err((err, frames))
            }
        }
    }

    /// Open a rooted runtime turn and drive the complete activation stack.
    pub(crate) fn dispatch_loop(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
    ) -> Result<Value, VmError> {
        self.dispatch_loop_above(context, stack, ActivationFloor::ROOT)
    }

    /// Drive only the activation region above `floor`.
    ///
    /// This is the shared-stack re-entry boundary: caller frames below the
    /// floor remain visible to GC and diagnostics, but terminal completion,
    /// suspension, and uncaught throws cannot consume or execute them.
    pub(crate) fn dispatch_loop_above(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        floor: ActivationFloor,
    ) -> Result<Value, VmError> {
        if floor.depth() > stack.len() {
            return Err(VmError::InvalidOperand);
        }
        // The turn roots queued inputs before assembly creates their native
        // extent, then retains the physical caller chain until completion.
        self.with_runtime_turn(stack, |turn| {
            let (interp, stack) = turn.into_parts();
            interp.execute_prepared_call(Some(context), stack)
        })
    }

    /// Drive a nested region of the stack already owned by a runtime turn.
    ///
    /// This is deliberately separate from [`Self::dispatch_loop_above`]: it
    /// cannot register a second [`otter_gc::RawFrameRoots`] for the same stack.
    pub(crate) fn dispatch_loop_above_rooted(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        floor: ActivationFloor,
    ) -> Result<Value, VmError> {
        if floor.depth() > stack.len() || !stack.is_runtime_rooted_by(self) {
            return Err(VmError::InvalidOperand);
        }
        self.execute_prepared_call(Some(context), stack)
    }

    /// Drive the dispatch loop, converting convertible `VmError`
    /// variants (TypeMismatch, NotCallable, TemporalDeadZone,
    /// OutOfMemory, etc.)
    /// into typed `Error` instances that flow through `unwind_throw`
    /// — so user code can `try { … } catch (e) { e instanceof
    /// TypeError }` and observe the same shape it would in any
    /// spec-conforming engine. Structural and runtime-control failures
    /// propagate unchanged. An explicit completed terminal outcome bypasses
    /// this source-error conversion. A failure while creating the exception value
    /// propagates its actual typed cause instead of suppressing the failure.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-error-objects>
    /// - <https://tc39.es/ecma262/#sec-native-error-types-used-in-this-standard>
    ///
    /// `resume_error` fails the resumption itself; `resume_site` says where
    /// the activation's PC stands for it.
    pub(super) fn dispatch_current_activation(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        floor: ActivationFloor,
        mut resume_error: Option<VmError>,
        resume_site: ThrowSite,
    ) -> Result<DispatchOutcome, VmError> {
        debug_assert!(stack.is_runtime_rooted_by(self));
        self.ensure_method_feedback_context(context);
        (|| -> Result<DispatchOutcome, VmError> {
            loop {
                let (step, site) = match resume_error.take() {
                    Some(error) => (Err(error), resume_site),
                    None => (
                        self.dispatch_loop_inner(context, stack, floor),
                        ThrowSite::Instruction,
                    ),
                };
                match step {
                    // This operation's native failure was projected at its
                    // live source/root boundary. Final failure must not enter
                    // the ordinary source VM error materializer again.
                    Ok(DispatchOutcome::Fatal(error)) => break Err(error),
                    Ok(value) => break Ok(value),
                    Err(err) => {
                        if matches!(err, VmError::Uncaught)
                            && !stack.is_at_floor(floor)
                            && let Some(thrown) = self.pending_uncaught_throw.take()
                        {
                            let unwind =
                                self.unwind_throw_above(context, stack, floor, thrown, site);
                            if unwind.is_err() {
                                // No handler in THIS dispatch stack —
                                // restore the original thrown value so
                                // an outer dispatch loop (across a
                                // native boundary) can still unwind
                                // with identity intact instead of the
                                // rendered string.
                                self.pending_uncaught_throw = Some(thrown);
                            }
                            unwind?;
                            if stack.is_at_floor(floor) {
                                // An async activation that absorbed the throw
                                // completes with its rejected result promise.
                                let result = self
                                    .completed_activation_result
                                    .take()
                                    .unwrap_or_else(Value::undefined);
                                break Ok(DispatchOutcome::Returned(result));
                            }
                            continue;
                        }
                        match self.vm_error_to_throwable_with_stack_roots(
                            Some(context),
                            stack,
                            &err,
                        ) {
                            Ok(thrown) => {
                                let uncaught = if matches!(
                                    err,
                                    VmError::OutOfMemory { .. } | VmError::JsonError
                                ) {
                                    Some(err)
                                } else {
                                    None
                                };
                                self.unwind_throw_with_uncaught_above(
                                    context, stack, floor, thrown, uncaught, site,
                                )?;
                                if stack.is_at_floor(floor) {
                                    let result = self
                                        .completed_activation_result
                                        .take()
                                        .unwrap_or_else(Value::undefined);
                                    break Ok(DispatchOutcome::Returned(result));
                                }
                                continue;
                            }
                            Err(error) => break Err(error),
                        }
                    }
                }
            }
        })()
    }
}

/// Live root over [`Interpreter::invoke_microtask`]'s
/// callee/this/argument locals: the values leave the traced microtask
/// queue before the staged request owns them. Raw pointers because the
/// locals are moved out while registered; the registration is popped
/// before the locals drop.
struct MicrotaskLocalsRoot {
    current: *const Value,
    this_value: *const Value,
    args: *const SmallVec<[Value; 8]>,
    result_capability: *const Option<crate::microtask::MicrotaskCapability>,
}

impl otter_gc::ExtraRootSource for MicrotaskLocalsRoot {
    fn visit_extra_roots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        // SAFETY: `invoke_microtask` pops this registration before the
        // pointed-at locals go out of scope, so the reads always see
        // the live locals.
        unsafe {
            (*self.current).trace_value_slots(visitor);
            (*self.this_value).trace_value_slots(visitor);
            for value in (*self.args).iter() {
                value.trace_value_slots(visitor);
            }
            if let Some(capability) = &*self.result_capability {
                capability.resolve.trace_value_slots(visitor);
                capability.reject.trace_value_slots(visitor);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Interpreter;
    use crate::jit::{JitExitProfile, JitParameterWidening};
    use crate::native_abi::{ExitAction, ExitReason};
    use crate::test_support::minimal_bytecode_module;
    use crate::{Value, activation_stack::ActivationStack};
    use otter_bytecode::{Constant, FunctionCodeBuilder, Op, Operand};

    #[test]
    fn chunk_eviction_purges_all_function_side_tables_only_in_its_function_range() {
        fn module(name: &str) -> otter_bytecode::BytecodeModule {
            let mut module = minimal_bytecode_module(name);
            module.constants = ["evictionLexical", "evictionObject"]
                .into_iter()
                .map(|name| Constant::String {
                    utf16: name.encode_utf16().collect(),
                })
                .collect();
            // An unreachable method site provides a real verified directory
            // address without invoking JavaScript or retaining a callable.
            let mut code = FunctionCodeBuilder::new();
            code.push(Op::ReturnUndefined, &[]);
            code.push(
                Op::CallMethodValue,
                &[
                    Operand::Register(0),
                    Operand::Register(0),
                    Operand::ConstIndex(1),
                    Operand::ConstIndex(0),
                ],
            );
            module.functions[0].code = code.finish();
            module
        }
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        vm.create_host_realm().unwrap();
        vm.set_code_eviction_high_water_bytes(0);
        let before = vm
            .link_module(
                module("pinned-before.js"),
                crate::source_registry::SourceRegistry::default(),
            )
            .unwrap();
        let mut dynamic_module = module("dynamic.js");
        let mut second = dynamic_module.functions[0].clone();
        second.id = 1;
        second.name = "second".to_owned();
        dynamic_module.functions.push(second);
        let dynamic = vm
            .link_evictable_module(
                dynamic_module,
                crate::source_registry::SourceRegistry::default(),
            )
            .unwrap();
        let after = vm
            .link_module(
                module("pinned-after.js"),
                crate::source_registry::SourceRegistry::default(),
            )
            .unwrap();
        let before_fid = before.function_base();
        let first_evicted = dynamic.function_base();
        let second_evicted = first_evicted + 1;
        let after_fid = after.function_base();
        assert_eq!(after_fid, second_evicted + 1);

        vm.declare_global_lex_value(&before, before_fid, 0, false)
            .unwrap();
        vm.init_global_lex_value(&before, before_fid, 0, Value::boolean(true))
            .unwrap();
        let mut stack = ActivationStack::new();
        vm.define_global_var_value(&before, &mut stack, before_fid, 1, Value::boolean(false))
            .unwrap();
        let mut global = vm.global_this;
        vm.migrate_slow_to_fast(&mut global);
        vm.global_this = global;
        vm.load_global_or_throw_value(&mut stack, &before, before_fid, 0)
            .unwrap();
        vm.load_global_or_throw_value(&mut stack, &before, before_fid, 1)
            .unwrap();
        let lexical = vm.global_lexical_load_ic[&(before_fid, 0)];
        let object_load = vm.global_object_load_ic[&(before_fid, 1)];
        let layout = vm.object_layout(&["field"]).unwrap();
        let shape_id = layout.shape_id();
        let shape = vm.shape_runtime.handle_for_id(shape_id).unwrap();
        let mut prototype = vm.realm_intrinsics.object_prototype().unwrap();
        vm.migrate_slow_to_fast(&mut prototype);
        let validity =
            crate::object::prototype_validity::chain_validity(prototype, vm.gc_heap()).unwrap();
        // Argument counts and shape identities are shared cache domains, even
        // when their numeric value happens to lie in the evicted fid range.
        vm.arguments_shape_cache
            .insert((first_evicted, false, shape_id), shape);

        let mut method_sites = Vec::new();
        for context in [&before, &dynamic, &after] {
            vm.template_objects
                .insert((context.function_base(), 0), Value::boolean(true));
            vm.extra_realms[0]
                .template_objects
                .insert((context.function_base(), 0), Value::boolean(false));
            vm.string_constant_cells
                .insert(context.constant_cache_key(0), Box::new(Value::undefined()));
            vm.bigint_constant_cache
                .insert(context.constant_cache_key(1), Value::undefined());
            vm.ensure_method_feedback_context(context);
            for (site, address) in context.feedback_slot_addresses() {
                assert!(address.is_method());
                vm.saturate_method_site_feedback(site);
                assert!(vm.method_feedback.install_method_ic(
                    site,
                    crate::method_ops::MethodCallIc::Collection(
                        crate::method_ops::CollectionMethodCallIc {
                            proto_shape: shape_id,
                            proto_slot: 0,
                            op: crate::method_ops::CollectionFastOp::MapGet,
                            leaf_stub_id: Some(crate::native_abi::STUB_COLLECTION_MAP_GET_LEAF.id),
                            mutating_stub_id: None,
                            alloc_stub_id: None,
                        },
                    ),
                ));
                method_sites.push((context.function_base(), site));
            }
        }
        let evicted_constants = [dynamic.constant_cache_key(0), dynamic.constant_cache_key(1)];

        for fid in [before_fid, first_evicted, second_evicted, after_fid] {
            // Default-realm linking omits the redundant zero entry. Seed its
            // equivalent explicit mapping so this range cleanup is exercised.
            vm.function_realm_ids.insert(fid, vm.active_realm_id);
            vm.jit_entry_bail_counts.insert(fid, fid + 12);
            vm.jit_osr_disabled.insert((fid, 8));
            vm.jit_optimized_declined_epoch.insert(fid, Some(fid + 14));
            vm.optimizing_tier_policy.begin_retraining(fid);
            for tier in [
                crate::tier_policy::CostedTier::Template,
                crate::tier_policy::CostedTier::Optimizing,
            ] {
                vm.optimizing_tier_policy.record_compile_attempt(fid, tier);
            }
            vm.simple_constructor_init_cache.insert(fid, None);
            vm.simple_constructor_absence.insert(fid, validity.clone());
            vm.simple_constructor_shape_cache
                .insert((fid, shape_id), shape);
            vm.object_literal_layouts.insert((fid, 0), layout);
            vm.global_lexical_load_ic.insert((fid, 0), lexical);
            vm.global_object_load_ic.insert((fid, 1), object_load);
            vm.extra_realms[0]
                .global_lexical_load_ic
                .insert((fid, 0), lexical);
            vm.extra_realms[0]
                .global_object_load_ic
                .insert((fid, 1), object_load);
            vm.function_user_props.insert(fid, vm.global_this);
            vm.function_prototype_overrides
                .insert(fid, Value::undefined());
            vm.function_prototype_slots
                .insert(fid, (Value::undefined(), false));
            vm.function_non_extensible.insert(fid);
            vm.function_deleted_metadata.insert((fid, "name"));
            for (pc, reason) in [(0, ExitReason::ShapeGuard), (7, ExitReason::IdentityGuard)] {
                vm.jit_optimized_exit_profiles.insert(
                    (fid, pc, reason),
                    JitExitProfile {
                        action: ExitAction::Recompile,
                        count: fid + pc + 1,
                        feedback_population: Some(fid + 2),
                    },
                );
            }
            vm.jit_parameter_widening.insert(
                fid,
                vec![JitParameterWidening::Number, JitParameterWidening::Tagged].into(),
            );
        }
        let pairs = [
            (before_fid, after_fid),
            (after_fid, before_fid),
            (before_fid, first_evicted),
            (before_fid, second_evicted),
            (first_evicted, after_fid),
            (second_evicted, after_fid),
        ];
        for (base, target) in pairs {
            vm.constructor_layout_for_receiver(
                base,
                Value::function(target),
                Value::object(prototype),
                |_, _| 0,
            )
            .expect("actual new.target family");
        }
        let profiles_before = vm.jit_optimized_exit_profiles.clone();
        let widening_before = vm.jit_parameter_widening.clone();

        vm.reclaim_dynamic_code().unwrap();
        assert_eq!(vm.code_eviction_stats().evicted_chunks, 0);
        assert_eq!(vm.jit_optimized_exit_profiles, profiles_before);
        assert_eq!(vm.jit_parameter_widening, widening_before);
        assert_eq!(vm.object_literal_layouts.len(), 4);
        assert_eq!(
            vm.function_constructor_layouts.len(),
            4,
            "actual immediate targets own heads, and same-target base families form one list"
        );
        assert_eq!(vm.extra_realms[0].global_lexical_load_ic.len(), 4);

        drop(dynamic);
        vm.reclaim_dynamic_code().unwrap();
        assert_eq!(vm.code_eviction_stats().evicted_chunks, 1);
        assert_eq!(vm.jit_optimized_exit_profiles.len(), 4);
        assert_eq!(vm.jit_parameter_widening.len(), 2);
        assert_eq!(vm.template_objects.len(), 2);
        assert_eq!(vm.extra_realms[0].template_objects.len(), 2);
        assert_eq!(vm.string_constant_cells.len(), 2);
        assert_eq!(vm.bigint_constant_cache.len(), 2);
        assert!(!vm.string_constant_cells.contains_key(&evicted_constants[0]));
        assert!(!vm.bigint_constant_cache.contains_key(&evicted_constants[1]));
        macro_rules! outside_keys {
            ($map:expr, $expected:expr) => {
                assert_eq!(
                    $map.keys()
                        .copied()
                        .collect::<std::collections::HashSet<_>>(),
                    $expected
                        .into_iter()
                        .collect::<std::collections::HashSet<_>>()
                );
            };
        }
        let surviving = [before_fid, after_fid];
        outside_keys!(vm.jit_entry_bail_counts, surviving);
        outside_keys!(vm.jit_optimized_declined_epoch, surviving);
        outside_keys!(vm.simple_constructor_init_cache, surviving);
        outside_keys!(vm.simple_constructor_absence, surviving);
        outside_keys!(
            vm.simple_constructor_shape_cache,
            surviving.map(|fid| (fid, shape_id))
        );
        outside_keys!(vm.object_literal_layouts, surviving.map(|fid| (fid, 0)));
        outside_keys!(vm.global_lexical_load_ic, surviving.map(|fid| (fid, 0)));
        outside_keys!(vm.global_object_load_ic, surviving.map(|fid| (fid, 1)));
        outside_keys!(
            vm.extra_realms[0].global_lexical_load_ic,
            surviving.map(|fid| (fid, 0))
        );
        outside_keys!(
            vm.extra_realms[0].global_object_load_ic,
            surviving.map(|fid| (fid, 1))
        );
        outside_keys!(vm.function_realm_ids, surviving);
        outside_keys!(vm.function_user_props, surviving);
        outside_keys!(vm.function_prototype_overrides, surviving);
        outside_keys!(vm.function_prototype_slots, surviving);
        outside_keys!(vm.function_constructor_layouts, surviving);
        assert_eq!(
            vm.function_non_extensible
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>(),
            surviving.into()
        );
        assert_eq!(
            vm.function_deleted_metadata
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>(),
            surviving.map(|fid| (fid, "name")).into()
        );
        assert_eq!(
            vm.jit_osr_disabled
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>(),
            surviving.map(|fid| (fid, 8)).into()
        );
        assert!(
            vm.arguments_shape_cache
                .contains_key(&(first_evicted, false, shape_id))
        );
        for (base, site) in method_sites {
            if base == first_evicted {
                assert!(vm.method_target_feedback(site).is_none());
                assert!(vm.method_feedback.method_ic(site).is_none());
            } else {
                assert!(vm.method_target_feedback_saturated(site));
                assert!(vm.method_feedback.method_ic(site).is_some());
            }
        }
        for fid in [first_evicted, second_evicted] {
            assert!(!vm.jit_parameter_widening.contains_key(&fid));
            assert!(
                !vm.jit_optimized_exit_profiles
                    .keys()
                    .any(|(source_fid, _, _)| *source_fid == fid)
            );
            assert!(
                vm.optimizing_tier_policy
                    .retraining_generation(fid)
                    .is_none()
            );
            for tier in [
                crate::tier_policy::CostedTier::Template,
                crate::tier_policy::CostedTier::Optimizing,
            ] {
                assert_eq!(vm.optimizing_tier_policy.compile_attempts(fid, tier), 0);
            }
        }
        for fid in [before_fid, after_fid] {
            assert_eq!(vm.jit_parameter_widening[&fid], widening_before[&fid]);
            assert_eq!(vm.jit_entry_bail_counts[&fid], fid + 12);
            assert_eq!(vm.jit_optimized_declined_epoch[&fid], Some(fid + 14));
            assert!(std::sync::Arc::ptr_eq(
                &vm.simple_constructor_absence[&fid],
                &validity
            ));
            assert_eq!(vm.object_literal_layouts[&(fid, 0)], layout);
            assert_eq!(
                crate::read_upvalue(vm.gc_heap(), vm.global_lexical_load_ic[&(fid, 0)]),
                Value::boolean(true)
            );
            assert_eq!(vm.function_user_props[&fid], vm.global_this);
            assert_eq!(
                vm.function_prototype_slots[&fid],
                (Value::undefined(), false)
            );
            assert!(
                vm.optimizing_tier_policy
                    .retraining_generation(fid)
                    .is_some()
            );
            for tier in [
                crate::tier_policy::CostedTier::Template,
                crate::tier_policy::CostedTier::Optimizing,
            ] {
                assert_eq!(vm.optimizing_tier_policy.compile_attempts(fid, tier), 1);
            }
            for (pc, reason) in [(0, ExitReason::ShapeGuard), (7, ExitReason::IdentityGuard)] {
                let key = (fid, pc, reason);
                assert_eq!(vm.jit_optimized_exit_profiles[&key], profiles_before[&key]);
            }
        }
        for pair in &pairs[..2] {
            let head = vm.function_constructor_layouts[&pair.1];
            assert_eq!(
                vm.gc_heap.read_payload(
                    head,
                    crate::constructor_layout::ConstructorLayoutBody::base_function_id
                ),
                pair.0,
                "evicted base keys were pruned without retaining their code"
            );
            assert_eq!(
                vm.gc_heap.read_payload(
                    head,
                    crate::constructor_layout::ConstructorLayoutBody::samples_remaining
                ),
                7
            );
        }
    }
}
