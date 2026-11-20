//! JIT compile sessions: preparation, baking hand-off, queueing and
//! installation.
//!
//! # Contents
//! - [`diagnostics`] records tier outcomes, generation snapshots and profiler symbols.
//! - `compile_jit_function` bakes and compiles a template generation;
//!   `compile_optimized_jit_function` prepares the facts and queues a bake on
//!   the isolate's compiler threads, which bake and compile it;
//!   `install_finished_optimized_compiles` installs their results at loop
//!   polls and tier decisions. A bake a collection made yield re-arms its
//!   function.
//!
//! # Invariants
//! Baking reads only (see `jit_bake`): every effect it relies on happened in
//! preparation, at a site's first execution or IC miss, or runs as an install
//! task. An install task that finds its fact changed declines the
//! installation, which re-arms tiering; a context slot written since baking
//! becomes volatile, as if its watch had fired.
//! Compile requests use the function's defining context, even when a later
//! script triggered invalidation or reentry. Compiled code is published only
//! after the registry accepts its metadata and exact isolate-epoch dependency
//! snapshot. Both tiers retain their roots and defining source chunks until
//! terminal compiler acknowledgement; cancelled, replaced-source or retraining
//! results cannot be published. The shared acknowledgement owner re-arms
//! deferred source reclamation after releasing those leases, without collecting
//! inside acknowledgement. Caller compilation never recursively compiles its
//! generated-call targets.
//!
//! # See also
//! - [`super::jit_compile_facts`] prepares and validates the facts a bake uses.
//! - `jit_bake` — what a compilation reads and how it names it.
//! - [`crate::jit::JitCompileSnapshot`] owns the native compile input.
//! - [`crate::jit_roots`] owns compile-session and generation lifetimes.
//! - [`crate::jit_compile_queue`] owns the compiler threads.
#![allow(unused_imports)]
use crate::*;

mod diagnostics;

/// Counted calls past which a hot loop's function is optimized for its entry
/// before any of its loops is entered through OSR.
const OSR_ENTRY_TIER_MIN_ENTRIES: u64 = 16;

/// Internal result of one Template compiler invocation.
///
/// Only [`TemplateCompileOutcome::Unsupported`] is stable enough to enter the
/// function-wide canonical cache. Allocation, backend availability, hook,
/// prewarm, and registry-admission failures are
/// [`TemplateCompileOutcome::Deferred`] so a later profitable policy decision
/// can retry without caching a performance failure as unsupported semantics.
#[derive(Debug, Clone)]
pub(super) enum TemplateCompileOutcome {
    Installed(std::sync::Arc<dyn jit::JitFunctionCode>),
    Unsupported,
    Deferred,
}

impl Interpreter {
    /// Publish one Template compile result into the single entry/OSR cache.
    ///
    /// A permanent unsupported verdict disables every OSR header. Deferred
    /// failures retain no code/cache verdict; entry and OSR each wait until the
    /// cost model again predicts positive payoff before trying again.
    pub(super) fn retain_template_compile_outcome(
        &mut self,
        fid: u32,
        outcome: TemplateCompileOutcome,
    ) -> TemplateCompileOutcome {
        match &outcome {
            TemplateCompileOutcome::Installed(code) => {
                self.jit_code.insert(fid, Some(code.clone()));
                if code.osr_only() {
                    self.jit_entry_osr_only.insert(fid);
                } else {
                    self.jit_entry_osr_only.remove(&fid);
                }
            }
            TemplateCompileOutcome::Unsupported => {
                self.jit_code.insert(fid, None);
                self.jit_template_osr_fids.remove(&fid);
                self.jit_entry_osr_only.remove(&fid);
                self.jit_osr_disabled.insert((fid, u32::MAX));
            }
            TemplateCompileOutcome::Deferred => {}
        }
        self.jit_code_cache = None;
        outcome
    }
    /// Prepare `fid`'s optimizing compilation from its latest feedback and
    /// hand it to the compiler, to replace the function's current native
    /// generation.
    ///
    /// Compilation leaves the current baseline generation installed. With
    /// compiler threads the request returns `None` at once and the isolate
    /// installs the result later
    /// ([`Self::install_finished_optimized_compiles`]); without them the body
    /// compiles here and is returned installed. A successful entry-capable
    /// optimizer is published through the function's stable entry cell;
    /// generated callers observe it on their next entry without
    /// recompilation. A declined optimizer leaves the baseline target
    /// untouched.
    ///
    /// Unsupported functions return `None`; a retraining function stays in the
    /// interpreter before any compile preparation or allocation begins, and a
    /// function with a compilation in flight is not prepared again.
    pub(crate) fn compile_optimized_jit_function(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        osr_pc: Option<u32>,
    ) -> Option<std::sync::Arc<dyn jit::JitFunctionCode>> {
        if self.jit_retraining_blocks(fid) || self.jit_compile_queue.is_pending(fid) {
            return None;
        }
        let available_code_bytes = self.jit_code_registry.available_code_bytes();
        if !self
            .jit_tier_work_decision(
                context,
                fid,
                crate::tier_policy::CostedTier::Optimizing,
                available_code_bytes,
            )?
            .should_compile()
        {
            return None;
        }
        self.compile_optimized_jit_function_session(context, fid, osr_pc)
    }

    /// [`Self::compile_optimized_jit_function`] once the policy admitted it.
    fn compile_optimized_jit_function_session(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        osr_pc: Option<u32>,
    ) -> Option<std::sync::Arc<dyn jit::JitFunctionCode>> {
        let hook = self.jit_hook.as_ref()?.clone();
        if !hook.optimizing_tier_enabled() {
            return None;
        }
        let owner = context.for_function(fid).ok()?;
        self.prepare_compile_facts(&owner, fid)?;
        let function = owner.code_block_arc(fid)?;
        // Everything the bake reads it reads on a compiler thread; the
        // isolate captures only what is not shared with it.
        let mut env = super::jit_bake_env::BakeEnv::capture(self);
        env.entry_bail_count = self.jit_entry_bail_counts.get(&fid).copied().unwrap_or(0);
        let products = std::sync::Arc::new(crate::jit_compile_queue::BakeProducts::holding(
            env.held_roots(),
        ));
        let debug = self.jit_debug.request();
        let artifact_identity = debug
            .artifacts_enabled()
            .then(|| crate::JitArtifactIdentity {
                function_name: owner
                    .function(fid)
                    .map(|function| function.name.clone())
                    .unwrap_or_else(|| "<unknown>".to_string()),
                module: function.module_url().to_string(),
            });
        // Compilations may be in flight together, so the identity is reserved
        // before the compiler stamps it into the code.
        let code_object_id = self.jit_next_code_object_id;
        self.jit_next_code_object_id += 1;
        let queued_ns = if debug.events_enabled() {
            self.jit_debug.monotonic_ns()
        } else {
            0
        };
        let perf_name = debug.perf_map_enabled().then(|| {
            owner.function(fid).map_or_else(
                || "<unknown>".to_string(),
                |function| format!("{}@{}", function.name, function.span.0),
            )
        });
        let cancellation = crate::JitCompileCancellation::default();
        let pending = crate::jit_compile_queue::PendingCompile {
            function_id: fid,
            osr_pc,
            tier: jit_debug::JitDebugTier::Optimizing,
            function,
            products: std::sync::Arc::clone(&products),
            cancellation: cancellation.clone(),
            retry_code_reclamation: false,
            feedback_epoch: self.code_space.feedback_epoch(fid),
            queued_ns,
            perf_name,
        };
        let job = super::jit_bake::BakeJob {
            #[cfg(test)]
            checkpoint: None,
            #[cfg(test)]
            before_hook: None,
            #[cfg(test)]
            publish_ready: None,
            #[cfg(test)]
            before_shape_census: None,
            env,
            context: (*owner).clone(),
            function_id: fid,
            osr_pc,
            tier: jit_debug::JitDebugTier::Optimizing,
            dependency_flag: self.jit_code_registry.dependency_flag(),
            debug,
            artifact_identity,
            code_object_id,
            cancellation,
            products,
        };
        let (pending, finished) = self.jit_compile_queue.submit(
            hook,
            crate::jit_compile_queue::CompileInput::Bake(Box::new(job)),
            code_object_id,
            pending,
        )?;
        self.finish_optimized_compile(pending, finished)
    }

    /// Retire the generations whose subscribed facts fired, and re-arm their
    /// functions' optimizing work window: they compile again from the
    /// current facts instead of staying in baseline code.
    pub(super) fn retire_fired_jit_code(&mut self) {
        let fired = self.jit_code_registry.invalidate_fired();
        if fired.is_empty() {
            return;
        }
        self.discard_invalidated_jit_state(&fired);
        for &fid in &fired {
            if let Ok(context) = self.function_context(None, fid)
                && let Some(function) = context.exec_function(fid)
            {
                let decision = self.optimizing_tier_policy.decide(
                    function,
                    crate::tier_policy::CostedTier::Optimizing,
                    self.jit_code_registry.available_code_bytes(),
                );
                self.jit_code_registry
                    .defer_generated_tiering(fid, decision.work_target);
            }
        }
    }

    /// Retire the generations whose subscribed facts fired, then install
    /// every optimized body the compiler threads finished since the last
    /// collection.
    #[inline]
    pub(crate) fn install_finished_optimized_compiles(&mut self) {
        self.retire_fired_jit_code();
        if self.jit_compile_queue.has_finished() {
            for (pending, finished) in self.jit_compile_queue.take_finished() {
                let retry_code_reclamation = pending.retry_code_reclamation;
                match pending.tier {
                    jit_debug::JitDebugTier::Template => {
                        let _ = self.finish_template_compile(pending, finished);
                    }
                    _ => {
                        let _ = self.finish_optimized_compile(pending, finished);
                    }
                }
                if retry_code_reclamation {
                    // Both tiers release their source leases before re-arming
                    // the next top-level pressure check. ACK itself never
                    // collects or recursively evicts source.
                    self.code_eviction_trigger_bytes = self.code_eviction_high_water_bytes;
                }
            }
        }
    }

    /// Install one finished optimizing compilation and record its outcome:
    /// the installed body, a decline that waits for new feedback, or nothing
    /// when only code memory refused it.
    ///
    /// A compilation whose function was invalidated or entered retraining
    /// after its snapshot is dropped, and the function's generated callers may
    /// request a fresh one.
    fn finish_optimized_compile(
        &mut self,
        pending: crate::jit_compile_queue::PendingCompile,
        mut finished: crate::jit_compile_queue::FinishedCompile,
    ) -> Option<std::sync::Arc<dyn jit::JitFunctionCode>> {
        let crate::jit_compile_queue::PendingCompile {
            function_id: fid,
            osr_pc,
            function,
            products,
            cancellation,
            feedback_epoch,
            queued_ns,
            perf_name,
            ..
        } = pending;
        let crate::jit_compile_queue::BakeOutput {
            roots,
            install,
            events,
            sources,
        } = products.take();
        self.publish_bake_events(events);
        let roots = roots.into_boxed_slice();
        // No source leases and no cancellation: the bake yielded to a
        // collection (or declined) before it could finish.
        let yielded = sources.is_none() && !cancellation.is_cancelled();
        let spliced = match &finished.status {
            Ok(jit::JitCompileStatus::Compiled { code, .. }) => code.spliced_functions(),
            _ => &[],
        };
        // A bake that yielded or declined left no source leases, and no code.
        let root_current = sources
            .as_ref()
            .is_some_and(|sources| sources.is_current(&self.code_space, &function, &[]));
        let sources_current = root_current
            && (spliced.is_empty()
                || sources.as_ref().is_some_and(|sources| {
                    sources.is_current(&self.code_space, &function, spliced)
                }));
        if !root_current {
            cancellation.cancel();
        }
        if cancellation.is_cancelled() {
            finished.status = Ok(jit::JitCompileStatus::Cancelled);
        }
        if finished.pre_hook_exit.is_none() {
            self.jit_runtime_stats.compile_attempts =
                self.jit_runtime_stats.compile_attempts.saturating_add(1);
            // Actual compiler work belongs to a still-current root even when
            // a returned artifact cannot publish one of its spliced sources.
            // Dropping that attempt would leave its unchanged source-work
            // budget immediately ready to compile the same snapshot again.
            if root_current {
                self.optimizing_tier_policy
                    .record_compile_attempt(fid, crate::tier_policy::CostedTier::Optimizing);
            }
        }
        let nanos =
            |duration: std::time::Duration| duration.as_nanos().min(u128::from(u64::MAX)) as u64;
        let queue_delay_ns = nanos(finished.queue_delay);
        let target = osr_pc.map_or(jit_debug::JitDebugTarget::Entry, |pc| {
            jit_debug::JitDebugTarget::Osr { pc }
        });
        let code_object_id = finished.code_object_id;
        self.record_jit_compile_finished(
            fid,
            jit_debug::JitDebugTier::Optimizing,
            target,
            code_object_id,
            finished.pre_hook_exit,
            finished
                .pre_hook_exit
                .is_none()
                .then(|| queued_ns.saturating_add(queue_delay_ns)),
            queue_delay_ns,
            nanos(finished.duration),
            &finished.status,
        );
        let rearm = |vm: &mut Self| {
            let decision = vm.optimizing_tier_policy.decide(
                &function,
                crate::tier_policy::CostedTier::Optimizing,
                vm.jit_code_registry.available_code_bytes(),
            );
            vm.jit_code_registry
                .defer_generated_tiering(fid, decision.work_target);
        };
        if self.jit_retraining_blocks(fid) {
            return None;
        }
        if matches!(finished.status, Ok(jit::JitCompileStatus::Cancelled)) {
            // A bake a collection made yield left nothing; nothing cancelled
            // it, so its function compiles again from the current facts.
            if yielded {
                rearm(self);
            }
            return None;
        }
        let declined = match finished.status {
            Ok(jit::JitCompileStatus::Compiled { .. }) if !sources_current => {
                // Every actual splice must have arrived in the prepared source
                // closure. With a current root and retained chunk leases this
                // is an invalid artifact, not a transient cancellation. Keep
                // truthful compile diagnostics and cache the ordinary decline
                // for this feedback rather than repeatedly compiling it.
                self.record_jit_install_declined(
                    fid,
                    code_object_id,
                    jit_debug::JitDebugTier::Optimizing,
                    crate::jit_registry::JitInstallError::InvalidCode,
                );
                true
            }
            Ok(jit::JitCompileStatus::Compiled {
                code,
                artifact,
                required_cells,
                ..
            }) => {
                if let Some(artifact) = artifact {
                    self.record_jit_artifact(*artifact);
                }
                if !self.run_install_tasks(install) {
                    // A fact the code reads as a constant changed since
                    // baking: stale like a changed dependency.
                    self.record_jit_install_declined(
                        fid,
                        code_object_id,
                        jit_debug::JitDebugTier::Optimizing,
                        crate::jit_registry::JitInstallError::DependencyChanged,
                    );
                    self.jit_optimized_code_cache = None;
                    rearm(self);
                    return None;
                }
                let roots = crate::jit_roots::CompilationRoot::retain_required_cells(
                    roots,
                    &required_cells,
                );
                code.prepare_for_execution();
                // Generated callers take no per-call executable lease; the
                // published frame chain names every executing generation.
                self.retire_unreferenced_jit_code();
                match self.jit_code_registry.install_compiled(
                    code_object_id,
                    code.clone(),
                    &function,
                    None,
                    roots,
                ) {
                    Ok(()) => {
                        self.jit_runtime_stats.code_generations =
                            self.jit_runtime_stats.code_generations.saturating_add(1);
                        if let Some(name) = &perf_name {
                            self.record_jit_perf_map(name, code.as_ref(), "optimizing");
                        }
                        self.jit_optimized_code.insert(fid, Some(code.clone()));
                        self.jit_optimized_declined_epoch.remove(&fid);
                        self.jit_optimized_code_cache = self
                            .jit_code_registry
                            .is_current_for_entry(code.as_ref())
                            .then(|| (fid, code.clone()));
                        return Some(code);
                    }
                    Err(crate::jit_registry::JitInstallError::DependencyChanged) => {
                        // Stale like an invalidated compile: the next tier-up
                        // compiles from the current facts.
                        self.record_jit_install_declined(
                            fid,
                            code_object_id,
                            jit_debug::JitDebugTier::Optimizing,
                            crate::jit_registry::JitInstallError::DependencyChanged,
                        );
                        self.jit_optimized_code_cache = None;
                        rearm(self);
                        return None;
                    }
                    Err(error) => {
                        if let crate::jit_registry::JitInstallError::ResourceBudget {
                            required_bytes,
                        } = error
                        {
                            self.optimizing_tier_policy.record_resource_refusal(
                                &function,
                                crate::tier_policy::CostedTier::Optimizing,
                                required_bytes,
                            );
                        }
                        self.record_jit_install_declined(
                            fid,
                            code_object_id,
                            jit_debug::JitDebugTier::Optimizing,
                            error,
                        );
                        error == crate::jit_registry::JitInstallError::InvalidCode
                    }
                }
            }
            _ => true,
        };
        self.jit_optimized_code_cache = None;
        // A declined entry compile is final for this feedback; a declined
        // loop compile waits for the feedback epoch to advance.
        if declined {
            match osr_pc {
                None => {
                    self.jit_optimized_code.insert(fid, None);
                }
                Some(_) => {
                    self.jit_optimized_declined_epoch
                        .insert(fid, feedback_epoch);
                }
            }
        }
        None
    }

    /// Resolve or compile one whole-body optimizer object for loop OSR.
    ///
    /// Entry and OSR share the function's exact source-work owner, so a
    /// single-call loop can fund admission without a second back-edge ledger.
    /// Successful code is shared with later function entries. An actual hook
    /// decline waits for a new feedback epoch; pre-hook deferral and resource
    /// refusal leave OSR eligible when work or physical headroom permits it.
    ///
    /// A function already entered [`OSR_ENTRY_TIER_MIN_ENTRIES`] times is
    /// compiled for its entry first, and this activation stays below the
    /// optimized tier.
    ///
    /// Optimized code enters at most the one header it was compiled for. When
    /// the current code cannot enter `osr_pc`, this header's back-edge crossed
    /// its tier threshold while the function kept running below the optimized
    /// tier, so the function is recompiled for it. The replacement serves
    /// function entries as well.
    pub(crate) fn resolve_optimized_osr_code(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        osr_pc: u32,
    ) -> Option<std::sync::Arc<dyn jit::JitFunctionCode>> {
        self.install_finished_optimized_compiles();
        if self.jit_retraining_blocks(fid)
            || !self
                .jit_hook
                .as_ref()
                .is_some_and(|hook| hook.optimizing_tier_enabled())
        {
            return None;
        }
        if let Some(Some(code)) = self.jit_optimized_code.get(&fid) {
            if !self.jit_code_registry.is_current_for_entry(code.as_ref()) {
                return None;
            }
            if code.enters_optimized_osr_header(osr_pc) {
                return Some(code.clone());
            }
        } else if !self.jit_optimized_code.contains_key(&fid)
            && self.jit_code_registry.counted_entries(fid) >= OSR_ENTRY_TIER_MIN_ENTRIES
        {
            // A function that keeps being called is optimized for its entry;
            // a loop is entered through OSR only while one activation stays
            // in it. This activation finishes below the optimized tier unless
            // it funds this header's threshold again.
            let _ = self.resolve_optimized_code_for_fid(context, fid);
            return None;
        }
        // A declined compile is retried at a back-edge only when the feedback
        // epoch has advanced since the last failed attempt: on unchanged
        // feedback the optimizer declines again, and would recompile on every
        // hot iteration. A compilation in flight serves this header when it
        // finishes.
        if self.jit_compile_queue.is_pending(fid)
            || self.jit_optimized_declined_epoch.get(&fid)
                == Some(&self.code_space.feedback_epoch(fid))
        {
            return None;
        }
        self.compile_optimized_jit_function(context, fid, Some(osr_pc))
    }

    /// Build a compile request for `fid` and run the installed hook. Returns the
    /// installed code, or `None` when the hook declines (unsupported subset or
    /// executable memory unavailable) — either way execution stays correct on
    /// the interpreter.
    pub(super) fn compile_jit_function(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        osr_pc: Option<u32>,
    ) -> TemplateCompileOutcome {
        let available_code_bytes = self.jit_code_registry.available_code_bytes();
        if self.jit_retraining_blocks(fid)
            || !self
                .jit_tier_work_decision(
                    context,
                    fid,
                    crate::tier_policy::CostedTier::Template,
                    available_code_bytes,
                )
                .is_some_and(crate::tier_policy::TierWorkDecision::should_compile)
            || self.jit_compile_queue.is_pending(fid)
            || !self.jit_template_compiling.insert(fid)
        {
            return TemplateCompileOutcome::Deferred;
        }
        let outcome = self.compile_jit_function_session(context, fid, osr_pc);
        if !self.jit_compile_queue.is_pending(fid) {
            self.jit_template_compiling.remove(&fid);
        }
        outcome
    }

    /// [`Self::compile_jit_function`] once the in-flight guard is held:
    /// prepare the facts and queue the bake and compile, or run them here
    /// without compiler threads. A queued compilation answers `Deferred` and
    /// installs at a later poll.
    fn compile_jit_function_session(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        osr_pc: Option<u32>,
    ) -> TemplateCompileOutcome {
        let Some(hook) = self.jit_hook.as_ref().cloned() else {
            return TemplateCompileOutcome::Deferred;
        };
        let Ok(owner) = context.for_function(fid) else {
            return TemplateCompileOutcome::Deferred;
        };
        let context = &*owner;
        if self.prepare_template_facts(context, fid).is_none() {
            return TemplateCompileOutcome::Deferred;
        }
        let Some(function) = owner.code_block_arc(fid) else {
            return TemplateCompileOutcome::Deferred;
        };
        let mut env = super::jit_bake_env::BakeEnv::capture(self);
        env.entry_bail_count = self.jit_entry_bail_counts.get(&fid).copied().unwrap_or(0);
        let products = std::sync::Arc::new(crate::jit_compile_queue::BakeProducts::holding(
            env.held_roots(),
        ));
        let debug = self.jit_debug.request();
        let artifact_identity = debug
            .artifacts_enabled()
            .then(|| crate::JitArtifactIdentity {
                function_name: owner
                    .function(fid)
                    .map(|function| function.name.clone())
                    .unwrap_or_else(|| "<unknown>".to_string()),
                module: function.module_url().to_string(),
            });
        let code_object_id = self.jit_next_code_object_id;
        self.jit_next_code_object_id += 1;
        let queued_ns = if debug.events_enabled() {
            self.jit_debug.monotonic_ns()
        } else {
            0
        };
        let perf_name = debug.perf_map_enabled().then(|| {
            owner.function(fid).map_or_else(
                || "<unknown>".to_string(),
                |function| format!("{}@{}", function.name, function.span.0),
            )
        });
        let cancellation = crate::JitCompileCancellation::default();
        let pending = crate::jit_compile_queue::PendingCompile {
            function_id: fid,
            osr_pc,
            tier: jit_debug::JitDebugTier::Template,
            function,
            products: std::sync::Arc::clone(&products),
            cancellation: cancellation.clone(),
            retry_code_reclamation: false,
            feedback_epoch: self.code_space.feedback_epoch(fid),
            queued_ns,
            perf_name,
        };
        let job = super::jit_bake::BakeJob {
            #[cfg(test)]
            checkpoint: None,
            #[cfg(test)]
            before_hook: None,
            #[cfg(test)]
            publish_ready: None,
            #[cfg(test)]
            before_shape_census: None,
            env,
            context: (*owner).clone(),
            function_id: fid,
            osr_pc,
            tier: jit_debug::JitDebugTier::Template,
            dependency_flag: self.jit_code_registry.dependency_flag(),
            debug,
            artifact_identity,
            code_object_id,
            cancellation,
            products,
        };
        match self.jit_compile_queue.submit(
            hook,
            crate::jit_compile_queue::CompileInput::Bake(Box::new(job)),
            code_object_id,
            pending,
        ) {
            Some((pending, finished)) => self.finish_template_compile(pending, finished),
            None => TemplateCompileOutcome::Deferred,
        }
    }

    /// Install one finished template compilation: register its code and keep
    /// the outcome in the function's template cache, which later entries and
    /// loop polls read.
    fn finish_template_compile(
        &mut self,
        pending: crate::jit_compile_queue::PendingCompile,
        finished: crate::jit_compile_queue::FinishedCompile,
    ) -> TemplateCompileOutcome {
        let crate::jit_compile_queue::PendingCompile {
            function_id: fid,
            osr_pc,
            function,
            products,
            cancellation,
            queued_ns,
            perf_name,
            ..
        } = pending;
        self.jit_template_compiling.remove(&fid);
        let crate::jit_compile_queue::BakeOutput {
            roots,
            install,
            events,
            sources,
        } = products.take();
        self.publish_bake_events(events);
        let target = osr_pc.map_or(jit_debug::JitDebugTarget::Entry, |pc| {
            jit_debug::JitDebugTarget::Osr { pc }
        });
        let nanos =
            |duration: std::time::Duration| duration.as_nanos().min(u128::from(u64::MAX)) as u64;
        let queue_delay_ns = nanos(finished.queue_delay);
        let code_object_id = finished.code_object_id;
        let current = sources
            .as_ref()
            .is_some_and(|sources| sources.is_current(&self.code_space, &function, &[]));
        let cancelled = cancellation.is_cancelled() || !current;
        let status = if cancelled {
            Ok(jit::JitCompileStatus::Cancelled)
        } else {
            finished.status
        };
        // Only compiler work on a still-current body is an attempt: a
        // cancelled or abandoned compilation leaves the function's work
        // budget ready to request it again at once.
        if finished.pre_hook_exit.is_none() && !cancelled {
            self.jit_runtime_stats.compile_attempts =
                self.jit_runtime_stats.compile_attempts.saturating_add(1);
            self.optimizing_tier_policy
                .record_compile_attempt(fid, crate::tier_policy::CostedTier::Template);
        }
        self.record_jit_compile_finished(
            fid,
            jit_debug::JitDebugTier::Template,
            target,
            code_object_id,
            finished.pre_hook_exit,
            finished
                .pre_hook_exit
                .is_none()
                .then(|| queued_ns.saturating_add(queue_delay_ns)),
            queue_delay_ns,
            nanos(finished.duration),
            &status,
        );
        if self.jit_retraining_blocks(fid) {
            return TemplateCompileOutcome::Deferred;
        }
        let outcome = match status {
            Ok(jit::JitCompileStatus::Compiled {
                code,
                artifact,
                required_cells,
                ..
            }) => {
                if let Some(artifact) = artifact {
                    self.record_jit_artifact(*artifact);
                }
                // Sweep before registering: cached/installed users hold an
                // `Arc`; executing generations are named by published frames.
                self.retire_unreferenced_jit_code();
                if !self.run_install_tasks(install) {
                    return TemplateCompileOutcome::Deferred;
                }
                let roots = crate::jit_roots::CompilationRoot::retain_required_cells(
                    roots.into_boxed_slice(),
                    &required_cells,
                );
                let required_bytes = crate::jit_registry::JitCodeRegistry::retained_admission_bytes(
                    code.as_ref(),
                    &roots,
                );
                let optimizing = self
                    .jit_hook
                    .as_ref()
                    .is_some_and(|hook| hook.optimizing_tier_enabled());
                let admission = self.jit_code_registry.install_compiled(
                    code_object_id,
                    code.clone(),
                    &function,
                    if optimizing {
                        self.optimizing_tier_policy
                            .decide(
                                &function,
                                crate::tier_policy::CostedTier::Optimizing,
                                self.jit_code_registry
                                    .available_code_bytes()
                                    .saturating_sub(required_bytes),
                            )
                            .work_target
                    } else {
                        None
                    },
                    roots,
                );
                if let Err(crate::jit_registry::JitInstallError::ResourceBudget {
                    required_bytes,
                }) = admission
                {
                    self.optimizing_tier_policy.record_resource_refusal(
                        &function,
                        crate::tier_policy::CostedTier::Template,
                        required_bytes,
                    );
                }
                if let Err(error) = admission {
                    self.record_jit_install_declined(
                        fid,
                        code_object_id,
                        jit_debug::JitDebugTier::Template,
                        error,
                    );
                }
                if admission.is_ok() {
                    self.jit_runtime_stats.code_generations =
                        self.jit_runtime_stats.code_generations.saturating_add(1);
                    self.optimizing_tier_policy
                        .observe_template_generation(&function);
                    if let Some(name) = &perf_name {
                        self.record_jit_perf_map(name, code.as_ref(), "template");
                    }
                    TemplateCompileOutcome::Installed(code)
                } else {
                    TemplateCompileOutcome::Deferred
                }
            }
            Ok(jit::JitCompileStatus::Unsupported { .. }) => TemplateCompileOutcome::Unsupported,
            Ok(jit::JitCompileStatus::Cancelled | jit::JitCompileStatus::Unavailable) | Err(_) => {
                TemplateCompileOutcome::Deferred
            }
        };
        self.retain_template_compile_outcome(fid, outcome)
    }
}
