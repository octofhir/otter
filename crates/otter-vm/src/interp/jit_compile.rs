//! JIT compile sessions: preparation, baking hand-off, queueing and
//! installation.
//!
//! # Contents
//! - `jit_code_residency` and `jit_code_generation_snapshot` — opt-in
//!   whole-isolate executable ownership and generation snapshots.
//! - `prepare_compile_facts` / `prepare_template_facts` — the mutator work a
//!   compilation needs before it bakes: canonical literals, intrinsic
//!   prototypes and their programs, chain and layout proofs, closure-context
//!   classification and context watches.
//! - `compile_jit_function` bakes and compiles a template generation;
//!   `compile_optimized_jit_function` bakes an optimizing snapshot and hands it
//!   to the isolate's compiler threads; `install_finished_optimized_compiles`
//!   installs their results at loop polls and tier decisions.
//! - `run_install_tasks` — the watches and registrations a baked snapshot
//!   asked for, re-verified before its code is published.
//! - Call/method target profiling and reoptimization eviction.
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
//! snapshot. An optimizing snapshot owns its roots and defining source chunks
//! until terminal compiler acknowledgement; cancelled, replaced-source or
//! retraining results cannot be published. Caller compilation never
//! recursively compiles its generated-call targets.
//!
//! # See also
//! - `jit_bake` — what a compilation reads and how it names it.
//! - [`crate::jit::JitCompileSnapshot`] owns the native compile input.
//! - [`crate::jit_roots`] owns compile-session and generation lifetimes.
//! - [`crate::jit_compile_queue`] owns the compiler threads.
#![allow(unused_imports)]
use crate::*;

use super::jit_bake::Baker;

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

use super::jit_bake::literal_matches;

impl Interpreter {
    /// Snapshot installed, invalid, and retired-tombstone JIT generations.
    ///
    /// This explicit diagnostics call walks cold registry metadata and the
    /// published frame chain; it performs no work during ordinary compilation
    /// or execution. A generation's `active_count` adds the published native
    /// frames executing it to its explicit entry leases.
    #[must_use]
    pub fn jit_code_generation_snapshot(&self) -> Vec<jit::JitCodeGenerationSnapshot> {
        let mut generations = self.jit_code_registry.generation_snapshot();
        for frame in self.jit_native_frames() {
            // SAFETY: every published record stays live while it is linked.
            let code_object_id = u64::from(unsafe { (*frame).code_object_id });
            if code_object_id == 0 {
                continue;
            }
            if let Some(generation) = generations
                .iter_mut()
                .find(|generation| generation.code_object_id == code_object_id)
            {
                generation.active_count = generation.active_count.saturating_add(1);
            }
        }
        generations
    }

    /// Snapshot all executable code objects currently retained by this isolate.
    ///
    /// This walks cold JIT ownership/cache tables only when explicitly called;
    /// ordinary dispatch, compilation, and stats collection do no residency
    /// accounting.
    #[must_use]
    pub fn jit_code_residency(&self) -> jit::JitCodeResidency {
        let mut seen = rustc_hash::FxHashSet::default();
        let mut code_bytes = 0u64;
        let mut record = |code: &std::sync::Arc<dyn jit::JitFunctionCode>| {
            let identity = std::sync::Arc::as_ptr(code) as *const () as usize;
            if seen.insert(identity) {
                code_bytes =
                    code_bytes.saturating_add(u64::try_from(code.code_len()).unwrap_or(u64::MAX));
            }
        };

        for code in self.jit_code.values().flatten() {
            record(code);
        }
        for code in self.jit_optimized_code.values().flatten() {
            record(code);
        }
        if let Some((_, code)) = &self.jit_code_cache {
            record(code);
        }
        if let Some((_, code)) = &self.jit_optimized_code_cache {
            record(code);
        }
        jit::JitCodeResidency {
            installed_optimized_bodies: self.jit_optimized_code.values().flatten().count() as u64,
            installed_entry_bodies: self
                .jit_code
                .values()
                .flatten()
                .filter(|code| !code.osr_only())
                .count() as u64,
            installed_osr_bodies: self
                .jit_template_osr_fids
                .iter()
                .filter(|fid| matches!(self.jit_code.get(*fid), Some(Some(_))))
                .count() as u64,
            unique_code_objects: seen.len() as u64,
            code_bytes,
        }
    }

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

    fn record_jit_compile_prepared(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        tier: jit_debug::JitDebugTier,
        target: jit_debug::JitDebugTarget,
        view: &jit::JitCompileSnapshot,
    ) {
        if !self.reserve_jit_debug_event() {
            return;
        }
        let function_name = context
            .function(fid)
            .map(|function| function.name.clone())
            .unwrap_or_else(|| "<unknown>".to_string());
        let method_feedback_sites = view
            .instructions
            .iter()
            .filter(|instr| {
                instr
                    .property_ic_site(&view.code_block)
                    .is_some_and(|site| self.method_target_feedback(site).is_some())
            })
            .count();
        let call_feedback_sites = view
            .instructions
            .iter()
            .filter(|instr| {
                view.code_block
                    .call_distribution_at(instr.instruction_pc(&view.code_block) as usize)
                    .is_some()
            })
            .count();
        let global_load_sites = view
            .instructions
            .iter()
            .filter(|instruction| instruction.op(&view.code_block) == Op::LoadGlobalOrThrow)
            .count();
        let source_work = view.code_block.source_work().total();
        let exit_count = u64::from(self.jit_entry_bail_counts.get(&fid).copied().unwrap_or(0))
            .saturating_add(
                self.exit_profiles_of(fid)
                    .map(|(_, profile)| u64::from(profile.count))
                    .sum::<u64>(),
            );
        let event = jit_debug::JitDebugEvent::CompilePrepared {
            function_id: fid,
            function_name,
            tier,
            target,
            register_count: u32::from(view.code_block.register_count),
            parameter_count: u32::from(view.code_block.param_count),
            bytecode_instruction_count: u64::try_from(view.instructions.len()).unwrap_or(u64::MAX),
            source_work,
            exit_count,
            call_feedback_sites: u32::try_from(call_feedback_sites).unwrap_or(u32::MAX),
            method_feedback_sites: u32::try_from(method_feedback_sites).unwrap_or(u32::MAX),
            global_load_sites: u32::try_from(global_load_sites).unwrap_or(u32::MAX),
            global_lexical_loads: u32::try_from(view.global_lexical_loads.len())
                .unwrap_or(u32::MAX),
            literal_cells: u32::try_from(view.literal_cells.len()).unwrap_or(u32::MAX),
            global_object_loads: u32::try_from(view.global_object_loads.len()).unwrap_or(u32::MAX),
            direct_callees: u32::try_from(view.direct_callees.len()).unwrap_or(u32::MAX),
            direct_constructs: u32::try_from(view.direct_constructs.len()).unwrap_or(u32::MAX),
            direct_method_sites: u32::try_from(
                view.method_targets
                    .values()
                    .filter(|targets| targets.iter().any(|target| target.callee.is_some()))
                    .count(),
            )
            .unwrap_or(u32::MAX),
            direct_method_targets: u32::try_from(
                view.method_targets
                    .values()
                    .flatten()
                    .filter(|target| target.callee.is_some())
                    .count(),
            )
            .unwrap_or(u32::MAX),
            native_calls: u32::try_from(view.native_calls.len()).unwrap_or(u32::MAX),
            inline_callees: u32::try_from(view.inline_callees.len()).unwrap_or(u32::MAX),
            inline_methods: u32::try_from(
                view.method_targets
                    .values()
                    .filter(|targets| {
                        targets
                            .iter()
                            .filter(|target| target.inline_fields.is_some())
                            .count()
                            == 1
                    })
                    .count(),
            )
            .unwrap_or(u32::MAX),
        };
        self.push_reserved_jit_debug_event(event);
        self.record_property_cache_ir_sites(fid, tier, view);
    }

    /// Report every immutable CacheIR program bank in `view`.
    ///
    /// Emitted once per baked snapshot, immediately after the prepare event, so
    /// a report reads as: this function, this tier, these sites lower to
    /// first-class guards/effects and these are their entry shapes/fields.
    fn record_property_cache_ir_sites(
        &mut self,
        fid: u32,
        tier: jit_debug::JitDebugTier,
        view: &jit::JitCompileSnapshot,
    ) {
        for (&byte_pc, programs) in &view.property_programs {
            if !self.reserve_jit_debug_event() {
                return;
            }
            let access = if programs.iter().any(|program| {
                program.ops.iter().any(|op| {
                    matches!(
                        op,
                        jit::JitCacheIrOp::StoreField { .. }
                            | jit::JitCacheIrOp::StoreBodySlot { .. }
                    )
                })
            }) {
                jit_debug::JitDebugPropertyAccess::Store
            } else {
                jit_debug::JitDebugPropertyAccess::Load
            };
            self.push_reserved_jit_debug_event(jit_debug::JitDebugEvent::PropertyCacheIrSite {
                function_id: fid,
                tier,
                byte_pc,
                access,
                programs: programs
                    .iter()
                    .filter_map(|program| {
                        let shape = program.ops.iter().find_map(|op| match op {
                            jit::JitCacheIrOp::GuardShape { shape, .. } => Some(*shape),
                            _ => None,
                        })?;
                        let field = program.ops.iter().find_map(|op| match op {
                            jit::JitCacheIrOp::LoadField { field, .. }
                            | jit::JitCacheIrOp::StoreField { field, .. } => Some(*field),
                            _ => None,
                        })?;
                        Some(jit_debug::JitDebugPropertyProgram { shape, field })
                    })
                    .collect(),
            });
        }
    }

    fn record_jit_install_declined(
        &mut self,
        fid: u32,
        code_object_id: u64,
        tier: jit_debug::JitDebugTier,
        error: crate::jit_registry::JitInstallError,
    ) {
        if !self.reserve_jit_debug_event() {
            return;
        }
        let reason = match error {
            crate::jit_registry::JitInstallError::InvalidCode => {
                jit_debug::JitInstallDeclineReason::InvalidCode
            }
            crate::jit_registry::JitInstallError::DependencyChanged => {
                jit_debug::JitInstallDeclineReason::DependencyChanged
            }
            crate::jit_registry::JitInstallError::ResourceBudget { required_bytes } => {
                jit_debug::JitInstallDeclineReason::ResourceBudget {
                    required_bytes,
                    available_bytes: self.jit_code_registry.available_code_bytes(),
                }
            }
        };
        self.push_reserved_jit_debug_event(jit_debug::JitDebugEvent::InstallDeclined {
            function_id: fid,
            code_object_id,
            tier,
            reason,
        });
    }

    fn record_jit_compile_finished(
        &mut self,
        fid: u32,
        tier: jit_debug::JitDebugTier,
        target: jit_debug::JitDebugTarget,
        code_object_id: u64,
        compile_started_ns: Option<u64>,
        queue_delay_ns: u64,
        compile_duration_ns: u64,
        status: &Result<jit::JitCompileStatus, jit::JitCompileError>,
    ) {
        if !self.reserve_jit_debug_event() {
            return;
        }
        let (outcome, ir_node_count) = match status {
            Ok(jit::JitCompileStatus::Cancelled) => {
                (jit_debug::JitDebugCompileOutcome::Cancelled, 0)
            }
            Ok(jit::JitCompileStatus::Compiled {
                code,
                ir_node_count,
                ..
            }) => (
                jit_debug::JitDebugCompileOutcome::Compiled {
                    code_object_id,
                    code_bytes: u64::try_from(code.code_len()).unwrap_or(u64::MAX),
                },
                *ir_node_count,
            ),
            Ok(jit::JitCompileStatus::Unavailable) => {
                (jit_debug::JitDebugCompileOutcome::Unavailable, 0)
            }
            Ok(jit::JitCompileStatus::Unsupported { reason }) => (
                jit_debug::JitDebugCompileOutcome::Unsupported {
                    reason: reason.clone(),
                },
                0,
            ),
            Err(error) => (
                jit_debug::JitDebugCompileOutcome::Error {
                    message: error.message.clone(),
                },
                0,
            ),
        };
        self.push_reserved_jit_debug_event(jit_debug::JitDebugEvent::CompileFinished {
            function_id: fid,
            tier,
            target,
            compile_started_ns,
            queue_delay_ns,
            compile_duration_ns,
            ir_node_count,
            outcome,
        });
        let Ok(jit::JitCompileStatus::Compiled { diagnostics, .. }) = status else {
            return;
        };
        for diagnostic in diagnostics.iter().cloned() {
            if !self.reserve_jit_debug_event() {
                break;
            }
            let event = match diagnostic {
                jit_debug::JitCompilerDiagnostic::InlineLowered {
                    parent_function_id,
                    instruction_pc,
                    byte_pc,
                    callee_function_id,
                    depth,
                    cost,
                    outcome,
                } => jit_debug::JitDebugEvent::InlineLowered {
                    function_id: fid,
                    code_object_id,
                    parent_function_id,
                    instruction_pc,
                    byte_pc,
                    tier,
                    callee_function_id,
                    depth,
                    cost,
                    outcome,
                },
                jit_debug::JitCompilerDiagnostic::DirectCallLowered {
                    call_kind,
                    instruction_pc,
                    byte_pc,
                    callee_function_id,
                    target_index,
                    target_count,
                    outcome,
                } => jit_debug::JitDebugEvent::DirectCallLowered {
                    call_kind,
                    caller_function_id: fid,
                    caller_code_object_id: code_object_id,
                    instruction_pc,
                    byte_pc,
                    tier,
                    callee_function_id,
                    target_index,
                    target_count,
                    outcome,
                },
                jit_debug::JitCompilerDiagnostic::StaticNativeCallLowered {
                    instruction_pc,
                    byte_pc,
                    target,
                    outcome,
                } => jit_debug::JitDebugEvent::StaticNativeCallLowered {
                    caller_function_id: fid,
                    caller_code_object_id: code_object_id,
                    instruction_pc,
                    byte_pc,
                    tier,
                    target,
                    outcome,
                },
            };
            self.push_reserved_jit_debug_event(event);
        }
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
        let context = &*owner;
        self.prepare_compile_facts(context, fid)?;
        let dependency_flag = self.jit_code_registry.dependency_flag();
        let env = super::jit_bake_env::BakeEnv::capture(self);
        let mut baker = Baker::new(&env);
        let snapshot = baker.bake_optimized(context, fid, dependency_flag);
        let (roots, install, events) = baker.finish();
        self.publish_bake_events(events);
        let snapshot = snapshot?;
        let sources = crate::jit_compile_lifetime::CompilationSources::retain(context, &snapshot)?;
        let target = osr_pc.map_or(jit_debug::JitDebugTarget::Entry, |pc| {
            jit_debug::JitDebugTarget::Osr { pc }
        });
        self.record_jit_compile_prepared(
            context,
            fid,
            jit_debug::JitDebugTier::Optimizing,
            target,
            &snapshot,
        );
        let artifact_identity =
            self.jit_debug
                .request()
                .artifacts_enabled()
                .then(|| crate::JitArtifactIdentity {
                    function_name: context
                        .function(fid)
                        .map(|function| function.name.clone())
                        .unwrap_or_else(|| "<unknown>".to_string()),
                    module: snapshot.code_block.module_url().to_string(),
                });
        let function = snapshot.code_block.clone();
        // Compilations may be in flight together, so the identity is reserved
        // before the compiler stamps it into the code.
        let code_object_id = self.jit_next_code_object_id;
        self.jit_next_code_object_id += 1;
        let queued_ns = if self.jit_debug.request().events_enabled() {
            self.jit_debug.monotonic_ns()
        } else {
            0
        };
        let perf_name = self.jit_debug.request().perf_map_enabled().then(|| {
            context
                .function(fid)
                .map_or_else(|| "<unknown>".to_string(), |function| function.name.clone())
        });
        let cancellation = crate::JitCompileCancellation::default();
        let pending = crate::jit_compile_queue::PendingCompile {
            function_id: fid,
            osr_pc,
            function,
            roots,
            install,
            sources,
            cancellation: cancellation.clone(),
            retry_code_reclamation: false,
            feedback_epoch: self.code_space.feedback_epoch(fid),
            queued_ns,
            perf_name,
        };
        let request = jit::JitCompileRequest {
            cancellation,
            snapshot,
            debug: self.jit_debug.request(),
            artifact_identity,
            osr_pc,
            code_object_id,
        };
        let (pending, finished) = self.jit_compile_queue.submit(hook, request, pending)?;
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
                let _ = self.finish_optimized_compile(pending, finished);
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
            roots,
            install,
            sources,
            cancellation,
            retry_code_reclamation,
            feedback_epoch,
            queued_ns,
            perf_name,
        } = pending;
        if retry_code_reclamation {
            // Release the leases below before the next top-level pressure
            // check. ACK itself never collects or recursively evicts source.
            self.code_eviction_trigger_bytes = self.code_eviction_high_water_bytes;
        }
        let spliced = match &finished.status {
            Ok(jit::JitCompileStatus::Compiled { code, .. }) => code.spliced_functions(),
            _ => &[],
        };
        let root_current = sources.is_current(&self.code_space, &function, &[]);
        let sources_current = root_current
            && (spliced.is_empty() || sources.is_current(&self.code_space, &function, spliced));
        if !root_current {
            cancellation.cancel();
        }
        if cancellation.is_cancelled() {
            finished.status = Ok(jit::JitCompileStatus::Cancelled);
        }
        if finished.entered_hook {
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
            finished
                .entered_hook
                .then(|| queued_ns.saturating_add(queue_delay_ns)),
            queue_delay_ns,
            nanos(finished.duration),
            &finished.status,
        );
        if self.jit_retraining_blocks(fid)
            || matches!(finished.status, Ok(jit::JitCompileStatus::Cancelled))
        {
            return None;
        }
        let rearm = |vm: &mut Self| {
            let decision = vm.optimizing_tier_policy.decide(
                &function,
                crate::tier_policy::CostedTier::Optimizing,
                vm.jit_code_registry.available_code_bytes(),
            );
            vm.jit_code_registry
                .defer_generated_tiering(fid, decision.work_target);
        };
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
            || !self.jit_template_compiling.insert(fid)
        {
            return TemplateCompileOutcome::Deferred;
        }
        let outcome = self.compile_jit_function_unchecked(context, fid, osr_pc);
        self.jit_template_compiling.remove(&fid);
        outcome
    }

    /// Compile after the per-function in-flight guard has been acquired.
    fn compile_jit_function_unchecked(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        osr_pc: Option<u32>,
    ) -> TemplateCompileOutcome {
        self.compile_jit_function_session(context, fid, osr_pc)
    }

    /// [`Self::compile_jit_function_unchecked`] once the guard is held.
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
        let env = super::jit_bake_env::BakeEnv::capture(self);
        let mut baker = Baker::new(&env);
        let view = baker.bake_template(context, fid);
        let (roots, install, events) = baker.finish();
        self.publish_bake_events(events);
        let Some(view) = view else {
            return TemplateCompileOutcome::Deferred;
        };
        let target = osr_pc.map_or(jit_debug::JitDebugTarget::Entry, |pc| {
            jit_debug::JitDebugTarget::Osr { pc }
        });
        self.record_jit_compile_prepared(
            context,
            fid,
            jit_debug::JitDebugTier::Template,
            target,
            &view,
        );
        let function = view.code_block.clone();
        let code_object_id = self.jit_next_code_object_id;
        let artifact_identity =
            self.jit_debug
                .request()
                .artifacts_enabled()
                .then(|| crate::JitArtifactIdentity {
                    function_name: context
                        .function(fid)
                        .map(|function| function.name.clone())
                        .unwrap_or_else(|| "<unknown>".to_string()),
                    module: view.code_block.module_url().to_string(),
                });
        let capture_timing = self.jit_debug.request().events_enabled();
        let queued_ns = if capture_timing {
            self.jit_debug.monotonic_ns()
        } else {
            0
        };
        let compile_started_ns = if capture_timing {
            self.jit_debug.monotonic_ns()
        } else {
            0
        };
        let compile_started = capture_timing.then(std::time::Instant::now);
        self.jit_runtime_stats.compile_attempts =
            self.jit_runtime_stats.compile_attempts.saturating_add(1);
        self.optimizing_tier_policy
            .record_compile_attempt(fid, crate::tier_policy::CostedTier::Template);
        let cancellation = crate::JitCompileCancellation::default();
        let mut status = hook.compile_function(jit::JitCompileRequest {
            cancellation: cancellation.clone(),
            snapshot: view,
            debug: self.jit_debug.request(),
            artifact_identity,
            osr_pc,
            code_object_id,
        });
        if cancellation.is_cancelled() {
            status = Ok(jit::JitCompileStatus::Cancelled);
        }
        let compile_duration_ns = compile_started.map_or(0, |started| {
            started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
        });
        self.record_jit_compile_finished(
            fid,
            jit_debug::JitDebugTier::Template,
            target,
            code_object_id,
            Some(compile_started_ns),
            compile_started_ns.saturating_sub(queued_ns),
            compile_duration_ns,
            &status,
        );
        match status {
            Ok(jit::JitCompileStatus::Compiled {
                code,
                artifact,
                required_cells,
                ..
            }) => {
                if let Some(artifact) = artifact {
                    self.record_jit_artifact(*artifact);
                }
                self.jit_next_code_object_id += 1;
                // Sweep before registering: cached/installed users hold an
                // `Arc`; executing generations are named by published frames.
                self.retire_unreferenced_jit_code();
                if !self.run_install_tasks(install) {
                    return TemplateCompileOutcome::Deferred;
                }
                let roots = crate::jit_roots::CompilationRoot::retain_required_cells(
                    roots,
                    &required_cells,
                );
                let required_bytes = crate::jit_registry::JitCodeRegistry::retained_admission_bytes(
                    code.as_ref(),
                    &roots,
                );
                let admission = self.jit_code_registry.install_compiled(
                    code_object_id,
                    code.clone(),
                    &function,
                    if hook.optimizing_tier_enabled() {
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
                let installed = admission.is_ok();
                if installed {
                    self.jit_runtime_stats.code_generations =
                        self.jit_runtime_stats.code_generations.saturating_add(1);
                    self.optimizing_tier_policy
                        .observe_template_generation(&function);
                    if self.jit_debug.request().perf_map_enabled() {
                        let name = context.function(fid).map_or_else(
                            || "<unknown>".to_string(),
                            |function| function.name.clone(),
                        );
                        self.record_jit_perf_map(&name, code.as_ref(), "template");
                    }
                }
                if installed {
                    TemplateCompileOutcome::Installed(code)
                } else {
                    TemplateCompileOutcome::Deferred
                }
            }
            Ok(jit::JitCompileStatus::Unsupported { .. }) => TemplateCompileOutcome::Unsupported,
            Ok(jit::JitCompileStatus::Cancelled | jit::JitCompileStatus::Unavailable) | Err(_) => {
                TemplateCompileOutcome::Deferred
            }
        }
    }

    /// Name one installed code object of function `name` in the perf map.
    fn record_jit_perf_map(&mut self, name: &str, code: &dyn jit::JitFunctionCode, tier: &str) {
        let Some(address) = code.native_code_address() else {
            return;
        };
        let id = code.metadata().id;
        self.jit_debug.record_perf_map(
            address,
            code.code_len(),
            &format!("JS:{name} [{tier} c{id}]"),
        );
        for (address, len, routine) in code.routine_symbols() {
            self.jit_debug.record_perf_map_once(address, len, routine);
        }
    }

    /// Empty every generated `instanceof` cell before `closure` changes its
    /// `prototype`, own properties or `[[Prototype]]`, when a cell ever
    /// cached it: no site may answer from a pair the change invalidates.
    pub(crate) fn retire_instanceof_proofs_for(&self, closure: crate::closure::JsClosure) {
        if !closure.instanceof_cached(&self.gc_heap) {
            return;
        }
        self.jit_instanceof_cells.borrow_mut().retain(|cell| {
            let Some(cell) = cell.upgrade() else {
                return false;
            };
            cell.clear();
            true
        });
    }

    /// Whether a method-call site's feedback has already saturated to
    /// `Megamorphic`. Once it has, further [`Self::note_method_target`]
    /// observations are no-ops, so a caller can skip the receiver/prototype
    /// shape walk that only exists to build the `MethodSite` argument — the hot
    /// path for a megamorphic site (e.g. one `arr[i].run()` over many classes).
    pub(crate) fn method_site_feedback_saturated(&self, site: usize) -> bool {
        self.method_target_feedback_saturated(site)
    }

    /// Record the live `Mono`/`Poly` overlay for one `Op::CallMethodValue` site.
    ///
    /// Returns `true` when the bounded target population changes. The caller
    /// uses that transition to invalidate an immutable optimizing guard chain;
    /// repeated hits merely update frequency and return `false`.
    pub(crate) fn note_method_target(
        &mut self,
        feedback_site: usize,
        method_fid: u32,
        site: MethodSite,
    ) -> bool {
        self.record_method_target_feedback(feedback_site, method_fid, site)
    }

    /// Classify the method a `CallMethodValue` site is about to invoke against
    /// the declared leaf-callable builtin table.
    ///
    /// Runs only on the feedback-capture path, which is already gated on an
    /// unsaturated site, and reads the method through the non-observable own /
    /// prototype data lookup. `None` whenever the callee is not a declared
    /// entry or the site's argument count is not the one that entry implements
    /// — the same declaration gate the backend applies, checked once here so a
    /// mismatched site never records.
    pub(crate) fn method_slot_native_leaf(
        &self,
        context: &ExecutionContext,
        caller_fid: u32,
        name_idx: u32,
        argc: usize,
        recv: Value,
    ) -> Option<crate::native_abi::RuntimeStubId> {
        let name = context.property_atom_for_function(caller_fid, name_idx)?;
        let method = crate::object::get(recv.as_object()?, &self.gc_heap, name.name())?;
        let declaration = crate::jit_static_native::jit_static_call_target(
            method.as_native_function()?,
            &self.gc_heap,
        )?;
        // A shaped method site passes only its arguments, so an entry that
        // reads `this` as an operand word is never recorded here.
        (argc == usize::from(declaration.argument_count) && !declaration.this_operand)
            .then_some(declaration.leaf_stub_id)
    }

    pub(crate) fn method_site_for_receiver(
        &mut self,
        context: &ExecutionContext,
        caller_fid: u32,
        name_idx: u32,
        recv: &mut Value,
    ) -> Option<MethodSite> {
        let name = context.property_atom_for_function(caller_fid, name_idx)?;
        let mut receiver = recv.as_object()?;
        // A receiver without a hidden class cannot be named by any guard, so
        // put it and its prototype chain on the shaped path first. The
        // migration allocates and may relocate the receiver, so the caller's
        // value is refreshed from the rooted handle before anything reads it.
        self.migrate_slow_to_fast(&mut receiver);
        *recv = Value::object(receiver);
        let recv = receiver;
        let recv_shape_handle = crate::object::keyed_shape(recv, &self.gc_heap);
        if recv_shape_handle.is_null() {
            return None;
        }
        let recv_shape = crate::object::shape_id(recv, &self.gc_heap);
        let resolved = crate::cache_ir::resolve_atom_data_slot(recv, &self.gc_heap, name)?;
        // A wide dictionary holder can keep ordinary data semantics without
        // having a keyed layout that native method linkage can guard. The
        // receiver's shape does not describe that holder's storage bank.
        if resolved.hit.shape.is_null() {
            return None;
        }
        self.shape_runtime
            .register_shape(&self.gc_heap, recv_shape_handle);
        if !resolved.holder_root.is_null() {
            self.shape_runtime
                .register_shape(&self.gc_heap, resolved.holder_root);
        }
        Some(MethodSite {
            recv_shape,
            prototype: crate::MethodLookupProof {
                validity: resolved.validity,
                holder_root: resolved.holder_root_id,
            },
            method_field: crate::object::field_location(
                resolved.hit.shape,
                u32::from(resolved.hit.slot),
            ),
        })
    }

    /// Make the facts a compilation of `fid` reads exist: canonical
    /// literals, the realm's Array chain and iteration proofs, collection
    /// prototypes in their final hidden classes, the intrinsic programs of
    /// its named sites, the global object's layout proof, the classification
    /// of its closures' contexts and the watch of its own closure context.
    /// Each step is cached and idempotent; baking reads what they made.
    pub(super) fn prepare_compile_facts(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
    ) -> Option<()> {
        if self.jit_retraining_blocks(fid) {
            return None;
        }
        let literals = self.prewarm_literal_cells(context, fid);
        self.prewarm_array_prototype_chain();
        // Prototypes first: the intrinsic programs' layout stamp names their
        // final shapes.
        self.prepare_intrinsic_prototypes();
        let owner = context.for_function(fid).ok()?;
        let function = owner.exec_function(fid)?;
        let mut iterates = false;
        for instruction in function.code.iter() {
            match function.op(instruction) {
                Op::LoadProperty | Op::HasNamedProperty | Op::CallMethodValue => {
                    if let Some(key) = function
                        .const_index(instruction, 2)
                        .and_then(|name| owner.property_atom_for_function(fid, name))
                    {
                        self.prepare_intrinsic_property_programs(key);
                    }
                }
                Op::MakeClosure | Op::MakeFunction => {
                    if let Some(function_id) = function
                        .const_index(instruction, 1)
                        .and_then(|index| owner.function_id_constant(index))
                    {
                        self.prepare_closure_context(&owner, function_id);
                    }
                }
                Op::GetIterator | Op::IteratorClose | Op::IteratorCloseThrow => iterates = true,
                _ => {}
            }
        }
        if iterates {
            let _ = self.array_iteration_cells();
        }
        let _ = crate::object::dictionary_layout_validity(self.global_this, &self.gc_heap);
        if let Some((_, closure_context)) = self.jit_code_registry.closure_contexts.single(fid)
            && let Some(closure_context) = closure_context.as_context()
        {
            let mut current = Some(closure_context);
            while let Some(context) = current {
                let _ = self
                    .jit_code_registry
                    .ensure_context_watch(&mut self.gc_heap, context);
                current = crate::context::walk(&self.gc_heap, context, 1);
            }
        }
        literals
    }

    /// [`Self::prepare_compile_facts`] for a template compilation: `fid`, and
    /// the method targets whose bodies may lend it field proofs.
    pub(super) fn prepare_template_facts(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
    ) -> Option<()> {
        self.prepare_compile_facts(context, fid)?;
        let owner = context.for_function(fid).ok()?;
        let function = owner.exec_function(fid)?;
        let targets: Vec<u32> = function
            .code
            .iter()
            .filter(|instruction| function.op(*instruction) == Op::CallMethodValue)
            .filter_map(|instruction| instruction.property_ic_site())
            .filter_map(|site| self.method_target_feedback(site))
            .flat_map(|feedback| match feedback {
                crate::MethodCallFeedback::Mono { method_fid, .. } => vec![method_fid],
                crate::MethodCallFeedback::Poly(targets) => {
                    targets.iter().map(|target| target.method_fid).collect()
                }
                _ => Vec::new(),
            })
            .collect();
        for target in targets {
            let _ = self.prepare_compile_facts(context, target);
        }
        Some(())
    }

    /// Publish the debug events one baking pass recorded.
    fn publish_bake_events(&mut self, events: Vec<jit_debug::JitDebugEvent>) {
        for event in events {
            self.record_jit_debug_event(|| event);
        }
    }

    /// Run a compilation's install tasks before its code is published; `false`
    /// when a fact its code reads changed since baking, which declines it.
    pub(super) fn run_install_tasks(&mut self, install: super::jit_bake::InstallTasks) -> bool {
        let super::jit_bake::InstallTasks {
            instanceof_cells,
            global_slot_watches,
            context_slot_watches,
        } = install;
        for watch in &global_slot_watches {
            if crate::object::dictionary_layout(self.global_this, &self.gc_heap)
                != Some(watch.layout)
                || !crate::object::watch_dictionary_slot(
                    self.global_this,
                    &mut self.gc_heap,
                    watch.slot,
                )
            {
                return false;
            }
        }
        for watch in &context_slot_watches {
            let Some(context) = watch.context.held().as_context() else {
                return false;
            };
            let expected = watch
                .value_cell
                .as_ref()
                .map_or(watch.bits, |cell| cell.held().to_bits());
            let Some(current) = crate::context::read_slot(&self.gc_heap, context, watch.slot)
            else {
                return false;
            };
            let watched =
                self.jit_code_registry
                    .watch_context_slot(&mut self.gc_heap, context, watch.slot);
            if current.to_bits() != expected {
                // Written after baking read it: the slot is volatile as if
                // its watch had fired, so no later compile folds it.
                if watched.is_some() {
                    crate::code_dependencies::context_slot_written(
                        crate::context::watch(&self.gc_heap, context),
                        watch.slot,
                    );
                }
                return false;
            }
            if watched != Some(watch.watch) {
                return false;
            }
        }
        if !instanceof_cells.is_empty() {
            let mut cells = self.jit_instanceof_cells.borrow_mut();
            for cell in instanceof_cells {
                if cells.len().is_power_of_two() {
                    cells.retain(|cell| cell.strong_count() != 0);
                }
                cells.push(std::sync::Arc::downgrade(&cell));
            }
        }
        true
    }

    /// Canonicalize every string literal needed by `fid` before snapshotting.
    ///
    /// Existing canonical cells and functions without literals are
    /// allocation-free and need no active frame-root provider. The first cold
    /// literal may allocate and therefore requires the current activation stack
    /// to be registered with the collector. Allocation failure only declines
    /// this optional compile; it does not publish a pending JavaScript throw or
    /// retain an error for later execution.
    /// Give the default realm's %Array.prototype% chain the prototype role
    /// whose writes retire proofs, once, so a bake reads that chain's validity
    /// cell for an Array method call without allocating. Like the literal
    /// prewarm, it runs before a body's snapshot and may collect.
    pub(super) fn prewarm_array_prototype_chain(&mut self) {
        if self.active_realm_id != 0 || !self.gc_heap.has_frame_root_providers() {
            return;
        }
        let Some(prototype) = self.default_array_prototype() else {
            return;
        };
        if crate::object::prototype_validity::chain_validity(prototype, &self.gc_heap).is_none() {
            let _ = self.prove_chain(Self::default_array_prototype);
        }
    }

    pub(super) fn prewarm_literal_cells(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
    ) -> Option<()> {
        if self.jit_retraining_blocks(fid) {
            return None;
        }
        let owner = context.for_function(fid).ok()?;
        let function = owner.exec_function(fid)?;
        let mut instruction_index = 0usize;
        while let Some(instruction) = function.instr_at_index(instruction_index) {
            instruction_index = instruction_index.checked_add(1)?;
            let op = function.op(instruction);
            if !matches!(op, Op::LoadString | Op::LoadBigInt) {
                continue;
            }
            let constant = function.const_index(instruction, 1)?;
            let key = owner.constant_cache_key(constant);
            if let Some(cell) = self.literal_cells().get(&key) {
                if !literal_matches(op, cell) {
                    return None;
                }
                continue;
            }
            if !self.gc_heap.has_frame_root_providers() {
                return None;
            }
            let value = if op == Op::LoadString {
                self.load_string_constant_value(&owner, constant)
            } else {
                self.load_bigint_constant_value(&owner, constant)
            };
            debug_assert!(value.as_ref().is_ok_and(|value| literal_matches(op, value)));
            value.ok()?;
        }
        Some(())
    }

    /// The optimizing exit evidence recorded for sites of `fid`, by PC and
    /// reason: one range of the profile map, never a walk of every function's.
    pub(super) fn exit_profiles_of(
        &self,
        fid: u32,
    ) -> impl Iterator<Item = (&(u32, u32, native_abi::ExitReason), &jit::JitExitProfile)> {
        self.jit_optimized_exit_profiles()
            .range((fid, 0, native_abi::ExitReason::TypeMismatch)..)
            .take_while(move |((profile_fid, _, _), _)| *profile_fid == fid)
    }
}
