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
        self.force_gc()?;
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
        self.jit_code_registry.retire_unreferenced();
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
        self.simple_constructor_shape_cache
            .retain(|(id, _), _| outside(id));
        self.constructor_field_transition_cache
            .retain(|id, _| outside(id));
        self.constructor_field_capacity_cache
            .retain(|(id, _), _| outside(id));
        self.constructor_instance_profiles
            .retain(|(id, _), _| outside(id));
        self.constructor_prototype_validity_cache
            .retain(|(id, _, _), _| outside(id));
        self.global_lexical_load_ic.retain(|(id, _), _| outside(id));
        self.global_object_load_ic.retain(|(id, _), _| outside(id));
        self.function_realm_ids.retain(|id, _| outside(id));
        self.function_user_props.retain(|id, _| outside(id));
        self.function_prototype_overrides
            .retain(|id, _| outside(id));
        self.function_prototype_slots.retain(|id, _| outside(id));
        self.function_non_extensible.retain(outside);
        self.function_deleted_metadata.retain(|(id, _)| outside(id));
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
        self.flush_constructor_observations(&self.gc_heap);
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
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let _extra_roots_guard = self.gc_heap.register_extra_roots(extra_roots);
        let mut noop = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        self.gc_heap.collect_full(&mut noop)?;
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
            let async_context = self.async_context();
            self.microtasks.enqueue(Microtask {
                callee: job.cleanup_callback,
                this_value: Value::undefined(),
                args,
                context: job.context,
                result_capability: None,
                kind: MicrotaskKind::FinalizationCallback,
                async_context,
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
    ) -> Result<ExecutionContext, crate::BytecodeLinkError> {
        let function_count = u32::try_from(module.functions.len()).map_err(|_| {
            crate::BytecodeLinkError::FunctionIdCapacity {
                base: 0,
                function_count: module.functions.len(),
            }
        })?;
        let context = self
            .code_space
            .link_module(module, self.module_sources.account())?;
        self.finish_linked_module(context, function_count)
    }

    /// Link an eval or on-demand module chunk that may be reclaimed after a
    /// between-turn liveness census proves its ids and payload unreachable.
    pub fn link_evictable_module(
        &mut self,
        module: otter_bytecode::BytecodeModule,
    ) -> Result<ExecutionContext, crate::BytecodeLinkError> {
        let function_count = u32::try_from(module.functions.len()).map_err(|_| {
            crate::BytecodeLinkError::FunctionIdCapacity {
                base: 0,
                function_count: module.functions.len(),
            }
        })?;
        let context = self
            .code_space
            .link_evictable_module(module, self.module_sources.account())?;
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
    ) -> Result<ExecutionContext, crate::BytecodeLinkError> {
        let function_count = u32::try_from(module.module().functions.len()).map_err(|_| {
            crate::BytecodeLinkError::FunctionIdCapacity {
                base: 0,
                function_count: module.module().functions.len(),
            }
        })?;
        let context = self
            .code_space
            .link_evictable_verified_module(module, self.module_sources.account())?;
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
    ) -> Result<ExecutionContext, crate::BytecodeLinkError> {
        let function_count = u32::try_from(module.module().functions.len()).map_err(|_| {
            crate::BytecodeLinkError::FunctionIdCapacity {
                base: 0,
                function_count: module.module().functions.len(),
            }
        })?;
        let context = self
            .code_space
            .link_verified_module(module, self.module_sources.account())?;
        self.finish_linked_module(context, function_count)
    }

    fn finish_linked_module(
        &mut self,
        context: ExecutionContext,
        function_count: u32,
    ) -> Result<ExecutionContext, crate::BytecodeLinkError> {
        context.resolve_atoms(&self.names);
        for function_id in context.function_base()..context.function_base() + function_count {
            let function = context
                .exec_function(function_id)
                .expect("verified linked function");
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
            for function_id in context.function_base()..context.function_end() {
                let function = context
                    .exec_function(function_id)
                    .expect("verified adopted function");
                let realm_id = self
                    .function_realm_ids
                    .get(&function_id)
                    .copied()
                    .unwrap_or(0);
                self.jit_code_registry.link_function(function, realm_id);
            }
        }
        // Remember the realm's dispatch context as the universal microtask
        // fallback. It shares the code space adopted above, so it resolves
        // function ids for any closure in the realm — a later drain of a
        // context-less job (async-resume continuation, host-settled reaction)
        // never strands for want of one.
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
        self.pending_uncaught_frames = None;
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

    /// Drain the microtask queue until empty.
    ///
    /// Each task is executed by invoking its callee with `this`
    /// and `args` set up at enqueue time. Tasks pushed during the
    /// drain go on the **next** generation, mirroring V8 / JSC.
    ///
    /// Foundation exception policy: the **first** error wins.
    /// The remaining queue is left in place so a follow-up
    /// `drain_microtasks` after the embedder recovers picks up
    /// where this drain stopped. Once the `Promise` constructor
    /// lands (task 34), this flips to spec semantics ("rejected
    /// promise, continue draining").
    pub fn drain_microtasks(&mut self, context: &ExecutionContext) -> Result<(), RunError> {
        self.drain_microtasks_with_default(Some(context.clone()))
    }

    /// Drain queued microtasks using each task's origin context,
    /// falling back to the caller-supplied context for jobs created
    /// inside the same VM turn. Host-settlement paths pass `None`
    /// so missing task origin is reported as an engine error.
    pub fn drain_microtasks_with_default(
        &mut self,
        default_context: Option<ExecutionContext>,
    ) -> Result<(), RunError> {
        // The drain runs outside `Interpreter::run`'s rooted scope
        // (the runtime layer drains after `run` returns), so register
        // the interpreter's runtime roots here. Without this, a
        // scavenge triggered by any allocation in a microtask body —
        // including async-resume parked frames and queued reaction
        // values — would miss every root enumerated by
        // [`crate::runtime_state::RuntimeState`] (shape side tables,
        // the microtask queue itself, globalThis, module envs) and
        // free or move objects still reachable through them.
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let _extra_roots_guard = self.gc_heap.register_extra_roots(extra_roots);
        self.begin_work_budget_turn();
        let result = self.drain_microtasks_with_default_inner(default_context);
        self.finish_work_budget_turn();
        result
    }

    pub(crate) fn drain_microtasks_with_default_inner(
        &mut self,
        default_context: Option<ExecutionContext>,
    ) -> Result<(), RunError> {
        // Alternate draining the queue empty with the HTML unhandled-rejection
        // checkpoint. The checkpoint's reporter/handler can enqueue follow-up
        // microtasks (and those can reject further promises), so loop until both
        // the queue and the tracker are quiescent.
        loop {
            self.enqueue_finalization_cleanup();
            self.drain_microtask_generations_inner(default_context.clone())?;
            if !self.promise_rejections_need_checkpoint() {
                return Ok(());
            }
            let context = default_context
                .clone()
                .or_else(|| self.realm_context.clone());
            let Some(context) = context else {
                // No realm context established yet — the JS reporter cannot run.
                // Drop the tracked rejections rather than strand them.
                self.clear_promise_rejection_tracking();
                return Ok(());
            };
            self.run_promise_rejection_checkpoint(&context)?;
            if !self.microtasks.has_any_pending() {
                return Ok(());
            }
        }
    }

    fn drain_microtask_generations_inner(
        &mut self,
        default_context: Option<ExecutionContext>,
    ) -> Result<(), RunError> {
        self.record_runtime_microtask_drain_started();
        loop {
            let Some(batch_len) = self.microtasks.begin_drain() else {
                return Ok(());
            };
            if batch_len == 0 {
                self.microtasks.end_drain();
                return Ok(());
            }
            // Tasks stay queue-owned (`next_in_flight`) rather than
            // being moved into a driver-local batch, so the ones
            // waiting behind the executing task remain visible to
            // the GC root walk — parked async frames in the queue
            // hold raw register slots a scavenge must rewrite.
            while let Some(task) = self.microtasks.next_in_flight() {
                self.record_runtime_microtask_executed();
                if let Err(error) = self.enforce_work_budget_checkpoint() {
                    self.microtasks.end_drain();
                    return Err(RunError {
                        error,
                        frames: Vec::new(),
                        detail: self.take_error_detail(),
                    });
                }
                // Context resolution is uniform for every drain entry point:
                // the job's own origin context, else the caller's hint, else
                // the realm fallback captured in `run`. Only a drain before any
                // top-level run (no realm context yet) can fail here, which is
                // an engine invariant violation, not a stranded async chain.
                let context = task
                    .context
                    .clone()
                    .or_else(|| default_context.clone())
                    .or_else(|| self.realm_context.clone());
                let Some(context) = context else {
                    self.microtasks.end_drain();
                    return Err(RunError {
                        error: VmError::InvalidOperand,
                        frames: Vec::new(),
                        detail: self.take_error_detail(),
                    });
                };
                // The task runs in the async context it was queued under, and
                // the drain restores the ambient one afterwards: two tasks
                // queued from different stores must not see each other's.
                let ambient = self.async_context();
                self.set_async_context(task.async_context);
                let outcome = self.invoke_microtask(&context, task);
                if let Err(err) = outcome {
                    // The failing task's context stays installed: the host
                    // decides what to do with the error and needs to see the
                    // context it was raised in. Restoring is the host's job
                    // from here.
                    self.microtasks.end_drain();
                    return Err(err);
                }
                self.set_async_context(ambient);
            }
            self.microtasks.end_drain();
            // Loop continues: any tasks pushed during this
            // generation get picked up by the next `begin_drain`.
            if !self.microtasks.has_any_pending() {
                return Ok(());
            }
        }
    }

    /// Invoke one microtask top-level. Builds a fresh frame stack
    /// containing just the task's callee; runs `dispatch_loop`
    /// until it returns. Errors include the snapshot of frames
    /// the task accumulated when it failed.
    pub(crate) fn invoke_microtask(
        &mut self,
        context: &ExecutionContext,
        task: Microtask,
    ) -> Result<(), RunError> {
        // Reaction-mode rejection forwarding (§27.2.1.3.2) reads the
        // abrupt completion's [[Value]] from `pending_uncaught_throw`
        // after `dispatch_loop` returns. Clear any stale payload
        // carried over from a prior microtask so we cannot read a
        // foreign reaction's value into this one.
        self.pending_uncaught_throw = None;
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
            return self.run_async_resume(context, frame, cold, await_dst, fulfilled, value);
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
            return self
                .run_async_gen_resume(context, frame, cold, await_dst, fulfilled, value, owner);
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
        context: &ExecutionContext,
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
                if result_capability.is_some() && !error.is_termination() {
                    // §27.2.1.3.2 step 1.f.iii: the rejection carries the
                    // original thrown value when one was preserved.
                    let reason = self.pending_uncaught_throw.take().unwrap_or_else(|| {
                        crate::promise_dispatch::rejection_value_for(self, &error)
                    });
                    self.settle_microtask_capability(
                        context,
                        stack,
                        result_capability.take(),
                        Err(reason),
                    )
                } else {
                    let frames = self.snapshot_active_frames(context, usize::MAX);
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
        context: &ExecutionContext,
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
                frames: Vec::new(),
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
            let result = promise_dispatch::PromiseBuilder::with_context(context.clone())
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
                    if let Err(err) = self.drain_microtasks_with_default(Some(context.clone())) {
                        self.json_root_pop_to(root_idx);
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
                let frames = self
                    .pending_uncaught_frames
                    .take()
                    .unwrap_or_else(|| self.snapshot_active_frames(context, usize::MAX));
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
            interp.execute_prepared_call(context, stack)
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
        self.execute_prepared_call(context, stack)
    }

    /// Drive the dispatch loop, converting convertible `VmError`
    /// variants (TypeMismatch, NotCallable, TemporalDeadZone,
    /// OutOfMemory, etc.)
    /// into typed `Error` instances that flow through `unwind_throw`
    /// — so user code can `try { … } catch (e) { e instanceof
    /// TypeError }` and observe the same shape it would in any
    /// spec-conforming engine. Variants that aren't user-recoverable
    /// (StackOverflow, Interrupted, Uncaught, MissingReturn,
    /// InvalidOperand) propagate as-is.
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
                    Ok(value) => break Ok(value),
                    Err(err) => {
                        if matches!(err, VmError::Uncaught)
                            && !stack.is_at_floor(floor)
                            && let Some(thrown) = self.pending_uncaught_throw.take()
                        {
                            if self.pending_uncaught_frames.is_none() {
                                self.pending_uncaught_frames =
                                    Some(self.snapshot_active_frames(context, usize::MAX));
                            }
                            let unwind =
                                self.unwind_throw_above(context, stack, floor, thrown, site);
                            if unwind.is_ok() {
                                self.pending_uncaught_frames = None;
                            } else {
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
                        if let Some(thrown) =
                            self.vm_error_to_throwable_with_stack_roots(Some(context), stack, &err)
                        {
                            let uncaught = if matches!(
                                err,
                                VmError::OutOfMemory { .. } | VmError::JsonError
                            ) {
                                Some(err)
                            } else {
                                None
                            };
                            if self.pending_uncaught_frames.is_none() {
                                self.pending_uncaught_frames =
                                    Some(self.snapshot_active_frames(context, usize::MAX));
                            }
                            let unwind = self.unwind_throw_with_uncaught_above(
                                context, stack, floor, thrown, uncaught, site,
                            );
                            if unwind.is_ok() {
                                self.pending_uncaught_frames = None;
                            }
                            unwind?;
                            if stack.is_at_floor(floor) {
                                let result = self
                                    .completed_activation_result
                                    .take()
                                    .unwrap_or_else(Value::undefined);
                                break Ok(DispatchOutcome::Returned(result));
                            }
                            continue;
                        }
                        break Err(err);
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
