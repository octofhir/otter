//! JIT compile requests and cold profile-feedback baking.
//!
//! # Contents
//! - `jit_code_residency` and `jit_code_generation_snapshot` — opt-in
//!   whole-isolate executable ownership and generation snapshots.
//! - `compile_jit_function` and cold feedback baking into the instruction view
//!   (property/object-literal/global-lexical/inline-callee tables).
//! - Call plans snapshot permanent function entry cells independently of
//!   callee tier.
//! - Call/method target profiling and reoptimization eviction.
//! - Plain, method and optimizing base-constructor snapshots share one bounded
//!   owned tree, including bounded recursive bodies. Template construction
//!   keeps generated call linkage.
//! - Cross-script recompilation resolves the defining code owner before baking.
//!
//! # Invariants
//! Baked pointers (shape ids, global cells, prototype slots) must only
//! reference permanent or non-moving allocations; anything movable goes
//! through a runtime stub instead.
//! Compile requests use the function's defining context, even when a later
//! script triggered invalidation or reentry. Global proofs and body splices
//! require the linked source realm to be active; foreign calls retain their
//! source-aware committed operations instead of baking the caller's cells.
//! Compiled code is published only after the registry accepts its metadata and
//! exact isolate-epoch dependency snapshot.
//! Caller compilation never recursively compiles its generated-call targets.
//! Property shape preparation precedes nested body and method snapshots, so
//! metadata migration cannot immediately stale a freshly captured method guard.
//! The sole ordinary shape baker refuses dictionary and opaque identities. A
//! provisional constructor lineage is an immutable layout like any other and
//! is baked for guards; only receiver allocation plans refuse a provisional
//! root. A refusal drops that specialized operation or call plan while
//! Generic semantics and runtime IC training remain available.
#![allow(unused_imports)]
use crate::*;

#[path = "inline_snapshot_budget.rs"]
mod inline_snapshot_budget;
use inline_snapshot_budget::InlineSnapshotBudget;

/// Counted calls past which a hot loop's function is optimized for its entry
/// before any of its loops is entered through OSR.
const OSR_ENTRY_TIER_MIN_ENTRIES: u64 = 16;

#[cfg(test)]
#[path = "jit_compile_binding_tests.rs"]
mod binding_tests;

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

/// Whether a literal cell holds the primitive `op` loads.
fn literal_matches(op: Op, value: &Value) -> bool {
    if op == Op::LoadString {
        value.is_string()
    } else {
        value.is_big_int()
    }
}

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
                self.jit_optimized_exit_profiles
                    .iter()
                    .filter(|((profile_fid, _, _), _)| *profile_fid == fid)
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
            direct_method_sites: u32::try_from(view.direct_methods.len()).unwrap_or(u32::MAX),
            direct_method_targets: u32::try_from(
                view.direct_methods.values().map(Vec::len).sum::<usize>(),
            )
            .unwrap_or(u32::MAX),
            native_calls: u32::try_from(view.native_calls.len()).unwrap_or(u32::MAX),
            inline_callees: u32::try_from(view.inline_callees.len()).unwrap_or(u32::MAX),
            inline_methods: u32::try_from(view.inline_methods.len()).unwrap_or(u32::MAX),
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
                program
                    .ops
                    .iter()
                    .any(|op| matches!(op, jit::JitCacheIrOp::StoreField { .. }))
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
        compile_started_ns: u64,
        queue_delay_ns: u64,
        compile_duration_ns: u64,
        status: &Result<jit::JitCompileStatus, jit::JitCompileError>,
    ) {
        if !self.reserve_jit_debug_event() {
            return;
        }
        let (outcome, ir_node_count) = match status {
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

    fn record_jit_inline_candidate(
        &mut self,
        caller_function_id: u32,
        instruction_pc: u32,
        tier: jit_debug::JitDebugTier,
        callee_function_id: Option<u32>,
        bake_rejection: Option<jit_debug::JitInlineRejectionReason>,
    ) {
        self.record_jit_debug_event(|| jit_debug::JitDebugEvent::InlineCandidate {
            caller_function_id,
            instruction_pc,
            tier,
            callee_function_id,
            bake_rejection,
        });
    }

    fn record_jit_direct_call_plan(
        &mut self,
        call_kind: jit::JitDirectCallKind,
        caller_function_id: u32,
        instruction_pc: u32,
        tier: jit_debug::JitDebugTier,
        callee_function_id: u32,
        target_index: u32,
        target_count: u32,
        outcome: jit_debug::JitDirectCallPlanOutcome,
    ) {
        self.record_jit_debug_event(|| jit_debug::JitDebugEvent::DirectCallPlan {
            call_kind,
            caller_function_id,
            instruction_pc,
            tier,
            callee_function_id,
            target_index,
            target_count,
            outcome,
        });
    }

    fn record_jit_static_native_call_plan(
        &mut self,
        caller_function_id: u32,
        instruction_pc: u32,
        tier: jit_debug::JitDebugTier,
        target: &'static str,
    ) {
        self.record_jit_debug_event(|| jit_debug::JitDebugEvent::StaticNativeCallPlan {
            caller_function_id,
            instruction_pc,
            tier,
            target,
        });
    }

    /// Replace `fid`'s current native generation with one optimizing-tier body
    /// compiled from the latest feedback snapshot.
    ///
    /// Compilation leaves the current baseline generation installed. A
    /// successful entry-capable optimizer is published through the function's
    /// stable entry cell; generated callers observe it on their next entry
    /// without recompilation. A declined optimizer leaves the baseline target
    /// untouched.
    ///
    /// Unsupported functions return `None`; a retraining function stays in the
    /// interpreter before any compile preparation or allocation begins.
    pub(crate) fn compile_optimized_jit_function(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        osr_pc: Option<u32>,
    ) -> Option<std::sync::Arc<dyn jit::JitFunctionCode>> {
        if self.jit_retraining_blocks(fid) {
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
        let roots_start = self.jit_compile_roots.borrow().len();
        let code = self.compile_optimized_jit_function_session(context, fid, osr_pc, roots_start);
        self.jit_compile_roots.borrow_mut().truncate(roots_start);
        code
    }

    /// [`Self::compile_optimized_jit_function`] inside one compile-shape
    /// session starting at `roots_start`.
    fn compile_optimized_jit_function_session(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        osr_pc: Option<u32>,
        roots_start: usize,
    ) -> Option<std::sync::Arc<dyn jit::JitFunctionCode>> {
        let hook = self.jit_hook.as_ref()?.clone();
        if !hook.optimizing_tier_enabled() {
            return None;
        }
        let owner = context.for_function(fid).ok()?;
        let context = &*owner;
        self.prewarm_literal_cells(context, fid)?;
        let mut snapshot = context.jit_compile_snapshot(fid)?;
        self.bake_literal_allocations(&mut snapshot, context, fid)?;
        // The optimizing tier consumes the same baked compile inputs as the
        // template tier: without the cage base and body offsets no inline access
        // can be emitted at all, and without monomorphic call-site candidates
        // there is nothing to inline.
        Self::bake_typed_array_layout(&mut snapshot);
        Self::bake_string_layout(&mut snapshot);
        self.bake_literal_cells(&mut snapshot, context, fid)?;
        self.bake_global_lexical_loads(&mut snapshot, context, fid);
        self.bake_binding_hit_proofs(&mut snapshot, context, fid);
        self.bake_property_cache_ir(&mut snapshot, context);
        self.bake_inline_callees(
            &mut snapshot,
            context,
            fid,
            jit_debug::JitDebugTier::Optimizing,
        );
        self.bake_guarded_method_calls(&mut snapshot);
        self.bake_instanceof_cells(&mut snapshot);
        self.bake_forward_apply(&mut snapshot);
        self.bake_element_accesses(&mut snapshot);
        Self::bake_context_allocations(&mut snapshot);
        Self::bake_closure_allocations(&mut snapshot, context);
        self.bake_optimized_exit_profile(&mut snapshot, fid);
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
        let code_object_id = self.jit_next_code_object_id;
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
            .record_compile_attempt(fid, crate::tier_policy::CostedTier::Optimizing);
        let status = hook.compile_optimized_function(jit::JitCompileRequest {
            snapshot,
            debug: self.jit_debug.request(),
            artifact_identity,
            osr_pc,
            code_object_id,
        });
        let compile_duration_ns = compile_started.map_or(0, |started| {
            started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
        });
        self.record_jit_compile_finished(
            fid,
            jit_debug::JitDebugTier::Optimizing,
            target,
            code_object_id,
            compile_started_ns,
            compile_started_ns.saturating_sub(queued_ns),
            compile_duration_ns,
            &status,
        );
        match status {
            Ok(jit::JitCompileStatus::Compiled { code, artifact, .. }) => {
                if let Some(artifact) = artifact {
                    self.record_jit_artifact(*artifact);
                }
                self.jit_next_code_object_id += 1;
                // Generated callers take no per-call executable lease; the
                // published frame chain names every executing generation.
                self.retire_unreferenced_jit_code();
                let roots = self.finalized_compile_roots(roots_start);
                let admission = self.jit_code_registry.install_compiled(
                    code_object_id,
                    code.clone(),
                    &function,
                    None,
                    roots,
                );
                if let Err(crate::jit_registry::JitInstallError::ResourceBudget {
                    required_bytes,
                }) = admission
                {
                    self.optimizing_tier_policy.record_resource_refusal(
                        &function,
                        crate::tier_policy::CostedTier::Optimizing,
                        required_bytes,
                    );
                }
                if let Err(error) = admission {
                    self.record_jit_install_declined(
                        fid,
                        code_object_id,
                        jit_debug::JitDebugTier::Optimizing,
                        error,
                    );
                }
                let installed = admission.is_ok();
                if installed {
                    self.jit_runtime_stats.code_generations =
                        self.jit_runtime_stats.code_generations.saturating_add(1);
                    self.record_jit_perf_map(context, fid, code.as_ref(), "optimizing");
                }
                installed.then_some(code)
            }
            _ => None,
        }
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
            // V8 optimizes a function that keeps being called for its entry
            // and enters a loop through OSR only while one activation stays
            // in it. This activation finishes below the optimized tier unless
            // it funds this header's threshold again.
            let _ = self.resolve_optimized_code_for_fid(context, fid);
            return None;
        }
        // A declined compile is retried at a back-edge only when the feedback
        // epoch has advanced since the last failed attempt: on unchanged
        // feedback the optimizer declines again, and would recompile on every
        // hot iteration.
        let epoch = self.code_space.feedback_epoch(fid);
        if self.jit_optimized_declined_epoch.get(&fid) == Some(&epoch) {
            return None;
        }
        let prior_attempts = self
            .optimizing_tier_policy
            .compile_attempts(fid, crate::tier_policy::CostedTier::Optimizing);
        let compiled = self.compile_optimized_jit_function(context, fid, Some(osr_pc));
        let attempted = self
            .optimizing_tier_policy
            .compile_attempts(fid, crate::tier_policy::CostedTier::Optimizing)
            > prior_attempts;
        // Actual compiler attempts increase the sole owner's required work;
        // subsequent dispatch/Template paths must fund replacement admission.
        match compiled {
            Some(compiled) => {
                self.jit_optimized_code.insert(fid, Some(compiled.clone()));
                self.jit_optimized_declined_epoch.remove(&fid);
                self.jit_optimized_code_cache = Some((fid, compiled.clone()));
                Some(compiled)
            }
            None => {
                if let Some(function) = context.exec_function(fid) {
                    let blocked = self
                        .optimizing_tier_policy
                        .decide(
                            function,
                            crate::tier_policy::CostedTier::Optimizing,
                            self.jit_code_registry.available_code_bytes(),
                        )
                        .resource_blocked();
                    if attempted && !blocked {
                        self.jit_optimized_declined_epoch.insert(fid, epoch);
                    }
                }
                None
            }
        }
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
        let roots_start = self.jit_compile_roots.borrow().len();
        let outcome = self.compile_jit_function_session(context, fid, osr_pc, roots_start);
        self.jit_compile_roots.borrow_mut().truncate(roots_start);
        outcome
    }

    /// [`Self::compile_jit_function_unchecked`] inside one
    /// compile-shape session starting at `roots_start`.
    fn compile_jit_function_session(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        osr_pc: Option<u32>,
        roots_start: usize,
    ) -> TemplateCompileOutcome {
        let Some(hook) = self.jit_hook.as_ref().cloned() else {
            return TemplateCompileOutcome::Deferred;
        };
        let Ok(owner) = context.for_function(fid) else {
            return TemplateCompileOutcome::Deferred;
        };
        let context = &*owner;
        if self.prewarm_literal_cells(context, fid).is_none() {
            return TemplateCompileOutcome::Deferred;
        }
        let Some(mut view) = context.jit_compile_snapshot(fid) else {
            return TemplateCompileOutcome::Deferred;
        };
        // Realm guards compare the active realm with the function's own.
        view.literal_allocations.realm_id = self.function_realm_id(fid);
        Self::bake_typed_array_layout(&mut view);
        Self::bake_string_layout(&mut view);
        if self.bake_literal_cells(&mut view, context, fid).is_none() {
            return TemplateCompileOutcome::Deferred;
        }
        self.bake_global_lexical_loads(&mut view, context, fid);
        self.bake_binding_hit_proofs(&mut view, context, fid);
        self.bake_property_cache_ir(&mut view, context);
        self.bake_inline_callees(&mut view, context, fid, jit_debug::JitDebugTier::Template);
        self.bake_guarded_method_calls(&mut view);
        self.bake_forward_apply(&mut view);
        self.bake_element_accesses(&mut view);
        Self::bake_context_allocations(&mut view);
        Self::bake_closure_allocations(&mut view, context);
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
        let status = hook.compile_function(jit::JitCompileRequest {
            snapshot: view,
            debug: self.jit_debug.request(),
            artifact_identity,
            osr_pc,
            code_object_id,
        });
        let compile_duration_ns = compile_started.map_or(0, |started| {
            started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
        });
        self.record_jit_compile_finished(
            fid,
            jit_debug::JitDebugTier::Template,
            target,
            code_object_id,
            compile_started_ns,
            compile_started_ns.saturating_sub(queued_ns),
            compile_duration_ns,
            &status,
        );
        match status {
            Ok(jit::JitCompileStatus::Compiled { code, artifact, .. }) => {
                if let Some(artifact) = artifact {
                    self.record_jit_artifact(*artifact);
                }
                self.jit_next_code_object_id += 1;
                // Sweep before registering: cached/installed users hold an
                // `Arc`; executing generations are named by published frames.
                self.retire_unreferenced_jit_code();
                let roots = self.finalized_compile_roots(roots_start);
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
                    self.record_jit_perf_map(context, fid, code.as_ref(), "template");
                }
                if installed {
                    TemplateCompileOutcome::Installed(code)
                } else {
                    TemplateCompileOutcome::Deferred
                }
            }
            Ok(jit::JitCompileStatus::Unsupported { .. }) => TemplateCompileOutcome::Unsupported,
            Ok(jit::JitCompileStatus::Unavailable) | Err(_) => TemplateCompileOutcome::Deferred,
        }
    }

    /// Name one installed code object in the perf map, when requested.
    fn record_jit_perf_map(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        code: &dyn jit::JitFunctionCode,
        tier: &str,
    ) {
        if !self.jit_debug.request().perf_map_enabled() {
            return;
        }
        let Some(address) = code.native_code_address() else {
            return;
        };
        let name = context
            .function(fid)
            .map_or_else(|| "<unknown>".to_string(), |function| function.name.clone());
        let id = code.metadata().id;
        self.jit_debug.record_perf_map(
            address,
            code.code_len(),
            &format!("JS:{name} [{tier} c{id}]"),
        );
    }

    /// Name `shape` in the code being compiled and return the compressed
    /// handle generated code compares or publishes. Hidden classes are
    /// collectable, so the compilation keeps the shape alive and the
    /// installed code object takes it over ([`Self::finalized_compile_roots`]).
    /// Unsupported state declines only this native proof, never publishes a
    /// fabricated zero identity or permanently poisons dynamic IC feedback.
    pub(crate) fn bake_shape(&self, shape: crate::object::ShapeHandle) -> Option<u32> {
        if shape.is_null() {
            return None;
        }
        let state = crate::object::shape_body::state_of(shape);
        if state.is_dictionary() || state.is_opaque() {
            return None;
        }
        self.jit_compile_roots
            .borrow_mut()
            .push(crate::jit_roots::CompilationRoot::Shape(shape));
        Some(shape.offset())
    }

    /// A fresh callee identity cell for one call site of the generation being
    /// compiled; the installed code object retains it.
    pub(crate) fn bake_callee_identity_cell(&self) -> u64 {
        let cell = std::sync::Arc::new(crate::jit_roots::CalleeIdentityCell::new());
        let address = cell.address();
        self.jit_compile_roots
            .borrow_mut()
            .push(crate::jit_roots::CompilationRoot::CalleeIdentity(cell));
        address
    }

    /// A fresh `instanceof` cell for one generated site, retained by the
    /// compilation and registered for [`Self::retire_instanceof_proofs_for`].
    pub(crate) fn bake_instanceof_cell(&self) -> u64 {
        let cell = std::sync::Arc::new(crate::jit_roots::InstanceofCell::new());
        let address = cell.address();
        {
            let mut cells = self.jit_instanceof_cells.borrow_mut();
            if cells.len().is_power_of_two() {
                cells.retain(|cell| cell.strong_count() != 0);
            }
            cells.push(std::sync::Arc::downgrade(&cell));
        }
        self.jit_compile_roots
            .borrow_mut()
            .push(crate::jit_roots::CompilationRoot::Instanceof(cell));
        address
    }

    /// Bake one `instanceof` cell for every `instanceof` site of `view`.
    fn bake_instanceof_cells(&self, view: &mut jit::JitCompileSnapshot) {
        let sites: Vec<u32> = view
            .instructions
            .iter()
            .filter(|instruction| instruction.op(&view.code_block) == Op::Instanceof)
            .map(|instruction| instruction.byte_pc)
            .collect();
        for byte_pc in sites {
            view.instanceof_cells
                .insert(byte_pc, self.bake_instanceof_cell());
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

    /// Retain a valid chain proof for the generation being compiled.
    pub(crate) fn bake_prototype_validity(
        &self,
        cell: &std::sync::Arc<crate::object::prototype_validity::PrototypeValidity>,
    ) -> Option<jit::JitPrototypeValidity> {
        if !cell.is_valid() {
            return None;
        }
        self.jit_compile_roots
            .borrow_mut()
            .push(crate::jit_roots::CompilationRoot::Prototype(cell.clone()));
        Some(jit::JitPrototypeValidity {
            address: cell.address(),
            identity: cell.identity(),
        })
    }

    /// [`Self::bake_shape`] for a shape feedback names by id; `None` when
    /// that shape has been collected since or cannot authorize a native proof.
    pub(crate) fn bake_shape_id(&self, id: crate::object::ShapeId) -> Option<u32> {
        self.shape_runtime
            .handle_for_id(id)
            .and_then(|shape| self.bake_shape(shape))
    }

    /// The live layout of a method holder as a generated guard names it: its
    /// baked hidden class, else its dictionary slot-layout epoch.
    pub(crate) fn jit_method_holder(
        &self,
        holder: crate::object::JsObject,
    ) -> Option<jit::JitMethodHolder> {
        let shape = crate::object::keyed_shape(holder, &self.gc_heap);
        if !shape.is_null() {
            return self.bake_shape(shape).map(jit::JitMethodHolder::Shape);
        }
        let state = crate::object::state(holder, &self.gc_heap);
        if !state.is_dictionary() {
            return None;
        }
        if state.is_opaque()
            && (Some(holder) != self.realm_intrinsics.string_prototype()
                || state.bits() & crate::object::ShapeState::OPAQUE_LOOKUP_MASK
                    != crate::object::ShapeState::STRING_WRAPPER_MASK)
        {
            return None;
        }
        // The only opaque dictionary holder admitted here is the pinned
        // source-realm String prototype. Its callers prove primitive receiver
        // type plus nonvirtual names (fixed builtin names or explicit index/
        // length exclusion), then own Data and watched layout/live callable.
        crate::object::dictionary_layout(holder, &self.gc_heap)
            .map(|layout| jit::JitMethodHolder::Dictionary(u64::from(layout)))
    }

    /// Retain all assumptions baked since this compile session began.
    fn finalized_compile_roots(
        &self,
        roots_start: usize,
    ) -> Box<[crate::jit_roots::CompilationRoot]> {
        let mut roots = self.jit_compile_roots.borrow()[roots_start..].to_vec();
        roots.sort_unstable_by_key(crate::jit_roots::CompilationRoot::key);
        roots.dedup_by_key(|root| root.key());
        roots.into_boxed_slice()
    }

    /// Prepare exact source-realm literal shapes and scalar dense geometry.
    fn bake_literal_allocations(
        &mut self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        fid: u32,
    ) -> Option<()> {
        let realm_id = self.function_realm_id(fid);
        view.literal_allocations.realm_id = realm_id;
        view.literal_allocations.group_allowed = self.gc_heap.machine_allocation_allowed();
        for (pc, instruction) in view.instructions.iter().enumerate() {
            match instruction.op(&view.code_block) {
                Op::NewObject => {
                    let plan = self
                        .with_host_realm_id(realm_id, |vm| {
                            let prototype = vm.object_prototype_object_opt();
                            let root = vm.object_root(
                                prototype,
                                crate::object::DEFAULT_INLINE_CAPACITY,
                                crate::object::ShapeState::ORDINARY,
                            )?;
                            Ok(vm
                                .bake_shape(root)
                                .map(jit::JitEmptyObjectAllocationPlan::new))
                        })
                        .ok()?;
                    view.literal_allocations.object = plan;
                }
                Op::NewObjectLiteral => {
                    let count = instruction.const_index(&view.code_block, 1)? as usize;
                    let first_key = instruction.const_index(&view.code_block, 2)?;
                    let plan = self
                        .with_host_realm_id(realm_id, |vm| {
                            let layout =
                                vm.object_literal_layout(context, fid, first_key, count)?;
                            let shape = vm
                                .shape_runtime
                                .handle_for_id(layout.shape_id())
                                .ok_or(crate::VmError::TypeMismatch)?;
                            let Some(_) = vm.bake_shape(shape) else {
                                return Ok(None);
                            };
                            Ok(jit::JitObjectLiteralAllocationPlan::new(shape, count))
                        })
                        .ok()?;
                    if let Some(plan) = plan {
                        view.literal_allocations
                            .objects
                            .insert(u32::try_from(pc).ok()?, plan);
                    }
                }
                Op::NewArray => {
                    let count = instruction.const_index(&view.code_block, 1)? as usize;
                    if let Some(plan) = jit::JitArrayLiteralAllocationPlan::new(count) {
                        view.literal_allocations
                            .arrays
                            .insert(u32::try_from(pc).ok()?, plan);
                    }
                }
                _ => {}
            }
        }
        Some(())
    }

    /// Plan inline context allocation for every scope of this function and
    /// of every body inlined into it.
    pub(crate) fn bake_context_allocations(view: &mut jit::JitCompileSnapshot) {
        fn collect(
            view: &jit::JitCompileSnapshot,
            plans: &mut rustc_hash::FxHashMap<(u32, u32), jit::JitContextAllocationPlan>,
            depth: u32,
        ) {
            let code_block = &view.code_block;
            for scope in 0..code_block.scopes.len() {
                let Ok(scope) = u32::try_from(scope) else {
                    break;
                };
                if let Some(plan) = jit::context_allocation_plan(code_block, scope) {
                    plans.insert((code_block.id, scope), plan);
                }
            }
            // Inline nesting is bounded by the inliner; the cap only guards
            // a malformed snapshot graph.
            if depth >= 16 {
                return;
            }
            for callee in view.inline_callees.values() {
                collect(&callee.body, plans, depth + 1);
            }
            for method in view.inline_methods.values() {
                collect(&method.body, plans, depth + 1);
            }
        }
        let mut plans = rustc_hash::FxHashMap::default();
        collect(view, &mut plans, 0);
        view.context_allocations = plans;
    }

    /// Plan inline closure allocation for every `MakeClosure` and
    /// `MakeFunction` site of this function. `context` owns the function.
    pub(crate) fn bake_closure_allocations(
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
    ) {
        let mut plans = rustc_hash::FxHashMap::default();
        let Ok(owner) = context.for_function(view.code_block.id) else {
            view.closure_allocations = plans;
            return;
        };
        for instruction in &view.instructions {
            let op = instruction.op(&view.code_block);
            if !matches!(op, Op::MakeClosure | Op::MakeFunction) {
                continue;
            }
            let Some(function_id) = instruction
                .const_index(&view.code_block, 1)
                .and_then(|index| owner.function_id_constant(index))
            else {
                continue;
            };
            // A generator or async function starts from its kind's
            // prototype, not the ordinary `%Function.prototype%` lookup; its
            // closures take the allocating runtime entry.
            let plain = owner.for_function(function_id).ok().is_some_and(|owner| {
                owner.function(function_id).is_some_and(|function| {
                    !function.is_generator && !function.is_async && !function.is_async_generator
                })
            });
            if !plain {
                continue;
            }
            let arrow = op == Op::MakeClosure && owner.function_is_arrow(function_id);
            let flags = crate::closure::CLOSURE_FLAGS_ORDINARY_LOOKUP
                | if arrow {
                    crate::closure::CLOSURE_CALL_FLAG_BOUND_THIS
                } else {
                    0
                };
            plans.insert(
                instruction.byte_pc,
                jit::JitClosureAllocationPlan {
                    call_word: u64::from(function_id) | (u64::from(flags) << 32),
                    arrow,
                },
            );
        }
        view.closure_allocations = plans;
    }

    /// Bake fixed Array-body fields used by native guards. Element backing
    /// stores remain behind runtime stubs and are deliberately absent here.
    pub(crate) fn bake_typed_array_layout(view: &mut jit::JitCompileSnapshot) {
        let header = otter_gc::header::HEADER_SIZE as u32;
        view.array_layout = jit::JitArrayLayout {
            type_tag: crate::array::ARRAY_BODY_TYPE_TAG,
            length_byte: header + crate::array::ARRAY_BODY_LENGTH_OFFSET as u32,
        };
        view.cage_base = otter_gc::cage_base() as usize;
    }

    /// Snapshot complete CodeBlock-owned CacheIR programs for native lowering.
    pub(crate) fn bake_property_cache_ir(
        &mut self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
    ) {
        view.property_action_cache = Some(self.property_cache.jit_layout());
        let sites: Vec<_> = view
            .instructions
            .iter()
            .filter(|instr| {
                matches!(
                    instr.op(&view.code_block),
                    Op::LoadProperty
                        | Op::HasNamedProperty
                        | Op::StoreProperty
                        | Op::CallMethodValue
                )
            })
            .map(|instr| {
                (
                    instr.byte_pc,
                    instr.instruction_pc(&view.code_block),
                    instr.op(&view.code_block),
                    match instr.op(&view.code_block) {
                        Op::LoadProperty | Op::HasNamedProperty | Op::CallMethodValue => {
                            instr.const_index(&view.code_block, 2)
                        }
                        Op::StoreProperty => instr.const_index(&view.code_block, 1),
                        _ => None,
                    },
                )
            })
            .collect();
        for (byte_pc, instruction_pc, op, name_index) in sites {
            // A method call's lookup shares the action cache.
            let kind = if op == Op::StoreProperty {
                crate::property_ic::PropertyIcKind::Store
            } else {
                crate::property_ic::PropertyIcKind::Load
            };
            let Some(key) = name_index.and_then(|name_index| {
                context.property_atom_for_function(view.code_block.id, name_index)
            }) else {
                continue;
            };
            let slot = view
                .code_block
                .property_feedback_at(instruction_pc as usize, kind);
            let shared = slot.is_some_and(|slot| slot.is_megamorphic());
            let atom = key.atom().id().raw();
            if let Some(slot) = slot {
                slot.native()
                    .bind_site(view.code_block.id, instruction_pc, atom);
            }
            view.property_accesses.insert(
                byte_pc,
                jit::JitPropertyAccess {
                    atom,
                    shared,
                    ic_slot: slot.map_or(0, |slot| slot.native().address()),
                },
            );
            let Some(slot) = slot else {
                continue;
            };
            if shared {
                continue;
            }
            let mut programs = slot
                .jit_programs(
                    atom,
                    |shape| self.bake_shape(shape),
                    |cell| self.bake_prototype_validity(cell),
                )
                .unwrap_or_default();
            if op != Op::StoreProperty {
                programs.extend(self.jit_intrinsic_property_programs(key));
            }
            if !programs.is_empty() {
                view.property_programs.insert(byte_pc, programs);
            }
        }
    }

    /// Describe how each `LoadElement` / `StoreElement` site addresses its
    /// receiver's elements, and an `IteratorNext` site the array its fast
    /// record steps.
    ///
    /// The family comes from what the site observed, so a typed view and a
    /// dense array are the same program over different declared offsets. A
    /// store additionally guards that the value already has the view's
    /// representation, because coercing it could run user code.
    pub(crate) fn bake_element_accesses(&mut self, view: &mut jit::JitCompileSnapshot) {
        self.bake_array_iteration(view);
        let sites: Vec<_> = view
            .instructions
            .iter()
            .filter_map(|instr| {
                let op = instr.op(&view.code_block);
                matches!(
                    op,
                    Op::LoadElement | Op::StoreElement | Op::StoreElementStrict | Op::IteratorNext
                )
                .then_some((instr.byte_pc, instr.instruction_pc(&view.code_block)))
            })
            .collect();
        for (byte_pc, pc) in sites {
            let family = view
                .code_block
                .feedback_at(pc as usize)
                .map_or(jit::JitElementFamily::Unseen, |cell| cell.element_family());
            let access = match family {
                jit::JitElementFamily::Typed(kind) => {
                    let Some(element) = jit::JitElementRepr::for_typed_kind(kind) else {
                        continue;
                    };
                    Self::typed_element_access(kind, element)
                }
                jit::JitElementFamily::DenseTagged => Self::dense_element_access(
                    jit::JitElementRepr::Boxed,
                    crate::array::DENSE_ELEMENT_KIND_TAGGED,
                ),
                jit::JitElementFamily::DenseFloat64 => Self::dense_element_access(
                    jit::JitElementRepr::Float64,
                    crate::array::DENSE_ELEMENT_KIND_PACKED_DOUBLE,
                ),
                jit::JitElementFamily::DenseHoleyFloat64 => Self::holey_double_element_access(),
                jit::JitElementFamily::Unseen => {
                    view.unseen_element_sites.insert(byte_pc);
                    continue;
                }
                jit::JitElementFamily::Generic => continue,
            };
            view.element_accesses.insert(byte_pc, access);
        }
    }

    /// The realm's Array iteration proof cells, for a function that opens or
    /// closes a synchronous iterator record while the proof holds.
    fn bake_array_iteration(&mut self, view: &mut jit::JitCompileSnapshot) {
        let iterates = view.instructions.iter().any(|instr| {
            matches!(
                instr.op(&view.code_block),
                Op::GetIterator | Op::IteratorClose | Op::IteratorCloseThrow
            )
        });
        if !iterates {
            return;
        }
        let Some(cells) = self.array_iteration_cells() else {
            return;
        };
        let (Some(iterable), Some(iterator)) = (
            self.bake_prototype_validity(&cells.iterable),
            self.bake_prototype_validity(&cells.iterator),
        ) else {
            return;
        };
        view.array_iteration = Some(jit::JitArrayIteration {
            iterable,
            iterator,
            close: cells.close,
            realm: self.active_realm_id,
            array_type_tag: crate::array::ARRAY_BODY_TYPE_TAG,
            array_exotic_byte: (otter_gc::header::HEADER_SIZE
                + std::mem::offset_of!(crate::array::ArrayBody, exotic))
                as u32,
        });
    }

    /// Describe one exact present-own array representation behind the body's
    /// element cache. Prototype-only sidecars are legal; descriptor/accessor
    /// baggage and a numeric-to-tagged transition leave before access.
    fn dense_element_access(
        element: jit::JitElementRepr,
        dense_kind: u32,
    ) -> jit::JitElementAccess {
        if element == jit::JitElementRepr::Float64
            && dense_kind == crate::array::DENSE_ELEMENT_KIND_PACKED_DOUBLE
        {
            return jit::JitElementAccess::packed_double_array();
        }
        let header = otter_gc::header::HEADER_SIZE as u32;
        jit::JitElementAccess {
            type_tag: crate::array::ARRAY_BODY_TYPE_TAG,
            guards: [
                Some(jit::JitBodyGuard::clear(
                    header + crate::array::ARRAY_BODY_DENSE_OWN_GUARD_OFFSET as u32,
                    jit::JitGuardWidth::Byte,
                )),
                Some(jit::JitBodyGuard {
                    byte: header + crate::array::ARRAY_BODY_DENSE_KIND_OFFSET as u32,
                    width: jit::JitGuardWidth::Byte,
                    expect: dense_kind,
                }),
            ],
            length_byte: header + crate::array::ARRAY_BODY_DENSE_LEN_OFFSET as u32,
            length_width: jit::JitGuardWidth::Word32,
            base: jit::JitElementBase::InBody {
                byte: header + crate::array::ARRAY_BODY_ELEMENTS_PTR_OFFSET as u32,
            },
            element,
            holes: None,
        }
    }

    /// Numeric dense storage of either kind, read through the hole bitmap.
    /// The exotic sidecar is guarded as for every dense family; the storage
    /// kind is proved by the bitmap descriptor, which admits both numeric
    /// kinds.
    fn holey_double_element_access() -> jit::JitElementAccess {
        let header = otter_gc::header::HEADER_SIZE as u32;
        jit::JitElementAccess {
            guards: [
                Some(jit::JitBodyGuard::clear(
                    header + std::mem::offset_of!(crate::array::ArrayBody, exotic) as u32,
                    jit::JitGuardWidth::Word32,
                )),
                None,
            ],
            holes: Some(jit::JitHoleBitmap {
                capacity_byte: crate::array::elements::CAPACITY_FROM_DATA_BYTE,
                kind_byte: header + crate::array::ARRAY_BODY_DENSE_KIND_OFFSET as u32,
                packed_kind: crate::array::DENSE_ELEMENT_KIND_PACKED_DOUBLE as u8,
                holey_kind: crate::array::DENSE_ELEMENT_KIND_HOLEY_DOUBLE as u8,
            }),
            ..Self::dense_element_access(
                jit::JitElementRepr::Float64,
                crate::array::DENSE_ELEMENT_KIND_HOLEY_DOUBLE,
            )
        }
    }

    /// A typed view's elements are raw scalars in its backing buffer. The
    /// view's own cached length is a construction-time field that a detach
    /// leaves untouched, so the buffer's detached flag is guarded on the way
    /// through rather than inferred from the bounds check; a length-tracking
    /// view over a resizable buffer has a stale cached length and leaves the
    /// fast path outright.
    fn typed_element_access(
        kind: crate::binary::TypedArrayKind,
        element: jit::JitElementRepr,
    ) -> jit::JitElementAccess {
        use crate::binary::array_buffer as buffer;
        use crate::binary::typed_array as view;
        let header = otter_gc::header::HEADER_SIZE as u32;
        let buffer_byte = header + view::TYPED_ARRAY_BODY_BUFFER_OFFSET as u32;
        jit::JitElementAccess {
            type_tag: view::TYPED_ARRAY_BODY_TYPE_TAG,
            guards: [
                Some(jit::JitBodyGuard {
                    byte: header + view::TYPED_ARRAY_BODY_KIND_OFFSET as u32,
                    width: jit::JitGuardWidth::Word32,
                    expect: kind as u32,
                }),
                Some(jit::JitBodyGuard::clear(
                    header + view::TYPED_ARRAY_BODY_LENGTH_TRACKING_OFFSET as u32,
                    jit::JitGuardWidth::Byte,
                )),
            ],
            length_byte: header + view::TYPED_ARRAY_BODY_LENGTH_OFFSET as u32,
            length_width: jit::JitGuardWidth::Word64,
            base: jit::JitElementBase::ThroughLocalBuffer {
                storage_tag_byte: buffer_byte + buffer::BUFFER_STORAGE_DISCRIMINANT_OFFSET as u32,
                local_tag: buffer::BUFFER_STORAGE_LOCAL_TAG,
                handle_byte: buffer_byte + buffer::BUFFER_STORAGE_HANDLE_OFFSET as u32,
                detached_byte: header + buffer::LOCAL_ARRAY_BUFFER_BODY_DETACHED_OFFSET as u32,
                data_ptr_byte: header + buffer::LOCAL_ARRAY_BUFFER_BODY_DATA_OFFSET as u32,
                byte_len_byte: header + buffer::LOCAL_ARRAY_BUFFER_BODY_BYTE_LEN_OFFSET as u32,
                view_offset_byte: header + view::TYPED_ARRAY_BODY_BYTE_OFFSET_OFFSET as u32,
                cached_data_byte: header + view::TYPED_ARRAY_BODY_DATA_OFFSET as u32,
            },
            element,
            holes: None,
        }
    }

    /// Bake the static heap-layout offsets for inline primitive string fast
    /// paths. String bodies are GC cells addressed through the same cage base as
    /// object/array bodies, so this only enables when the compile snapshot has a
    /// cage base.
    pub(crate) fn bake_string_layout(view: &mut jit::JitCompileSnapshot) {
        view.string_layout = jit::JitStringLayout::default();
        view.cage_base = otter_gc::cage_base() as usize;
    }

    /// Read a monomorphic own-data case directly from the shared interpreter PIC.
    ///
    /// A site whose cache is still empty falls back to the compiler's
    /// TypeScript class annotation, which resolves against the live instance
    /// shape of the annotated class. Any recorded observation supersedes it.
    fn monomorphic_own_property_feedback(
        &self,
        context: &ExecutionContext,
        code_block: &CodeBlock,
        instruction: &jit::JitInstructionMetadata,
    ) -> Option<(u32, crate::object::FieldLocation)> {
        let op = instruction.op(code_block);
        let kind = match op {
            Op::LoadProperty => crate::property_ic::PropertyIcKind::Load,
            Op::StoreProperty => crate::property_ic::PropertyIcKind::Store,
            _ => return None,
        };
        let instruction_pc = instruction.instruction_pc(code_block) as usize;
        match code_block
            .property_feedback_at(instruction_pc, kind)?
            .state()
        {
            crate::feedback::PropertyFeedbackState::MonomorphicOwnData { shape, slot } => {
                let baked = self.bake_shape(shape)?;
                Some((baked, crate::object::field_location(shape, u32::from(slot))))
            }
            crate::feedback::PropertyFeedbackState::Empty => {
                let slot = self.class_annotation_property_slot(context, code_block, instruction)?;
                Some(slot)
            }
            _ => None,
        }
    }

    /// Own-data slot a class-annotated property site is expected to hit.
    ///
    /// The annotation names a locally declared class; the interpreter's
    /// simple-constructor cache holds the instance shape that class builds, so
    /// the property resolves to a concrete shape and slot without the site
    /// having run. Unsound by construction — the caller must only use this
    /// where the site already guards the shape it was given.
    fn class_annotation_property_slot(
        &self,
        context: &ExecutionContext,
        code_block: &CodeBlock,
        instruction: &jit::JitInstructionMetadata,
    ) -> Option<(u32, crate::object::FieldLocation)> {
        let name_operand = match instruction.op(code_block) {
            Op::LoadProperty => 2,
            Op::StoreProperty => 1,
            _ => return None,
        };
        let class_fid = code_block.class_hint(instruction.instruction_pc(code_block) as usize)?;
        // One prototype lineage per class in practice; several leave the
        // site to its own feedback.
        let mut shapes = self
            .simple_constructor_shape_cache
            .iter()
            .filter(|((id, _), _)| *id == class_fid)
            .map(|(_, shape)| *shape);
        let shape = shapes.next()?;
        if shapes.next().is_some() {
            return None;
        }
        let otter_bytecode::Operand::ConstIndex(name_idx) =
            instruction.operand(code_block, name_operand)?
        else {
            return None;
        };
        let key = context.property_atom(name_idx)?;
        let slot = crate::object::shape_offset_of_str(&self.gc_heap, shape, key.name())?;
        Some((
            self.bake_shape(shape)?,
            crate::object::field_location(shape, slot),
        ))
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

    /// Bake every `Op::CallMethodValue` site whose callee is a declared native
    /// entry into the compile snapshot.
    ///
    /// The receiver layout comes from the same feedback a property site
    /// records, so generated code guards it with the shared way walk and
    /// prototype hop rather than a second description of the same access. An
    /// entry that may collect reserves its safepoint here, at the site that
    /// will publish it.
    /// Proof for an `f.call(...)` site (V8's `ReduceFunctionPrototypeCall`)
    /// whose feedback recorded the function `%Function.prototype.call%` ran.
    /// A method call named `call` proves its lookup through the closure
    /// property program while the intrinsic still occupies the slot; an
    /// explicit-receiver call proves only its loaded callee. Generated code
    /// re-proves the intrinsic's identity before calling the function.
    fn function_prototype_call_proof(
        &mut self,
        view: &jit::JitCompileSnapshot,
        context: &ExecutionContext,
        op: Op,
        instruction_pc: u32,
    ) -> Option<jit::JitFunctionPrototypeCall> {
        let intrinsic = crate::native_function::VmIntrinsicFunction::FunctionPrototypeCall;
        let call_native_ref = intrinsic.native_ref(&self.gc_heap)?;
        let lookup = match op {
            Op::CallWithThis => None,
            Op::CallMethodValue => {
                let name_index = view
                    .instructions
                    .get(instruction_pc as usize)?
                    .const_index(&view.code_block, 2)?;
                let key = context.property_atom_for_function(view.code_block.id, name_index)?;
                let holds_intrinsic = self
                    .realm_intrinsics
                    .function_prototype()
                    .and_then(|prototype| object::get(prototype, &self.gc_heap, "call"))
                    .and_then(|value| value.as_native_function())
                    .is_some_and(|native| native.is_vm_intrinsic(&self.gc_heap, intrinsic));
                if key.name() != "call" || !holds_intrinsic {
                    return None;
                }
                let program = self.closure_property_program(key)?;
                let [
                    jit::JitCacheIrOp::LoadIntrinsicPrototype { target, .. },
                    jit::JitCacheIrOp::GuardShape { shape, .. },
                    _,
                    jit::JitCacheIrOp::LoadField { field, .. },
                ] = *program.ops
                else {
                    return None;
                };
                Some(jit::JitFunctionCallLookup {
                    receiver: target,
                    holder_shape: shape,
                    call_field: field,
                })
            }
            _ => return None,
        };
        Some(jit::JitFunctionPrototypeCall {
            lookup,
            call_native_ref,
        })
    }

    pub(crate) fn bake_guarded_method_calls(&mut self, view: &mut jit::JitCompileSnapshot) {
        let sites: Vec<_> = view
            .instructions
            .iter()
            .filter(|instr| instr.op(&view.code_block) == Op::CallMethodValue)
            .map(|instr| {
                (
                    instr.byte_pc,
                    instr.property_ic_site(&view.code_block),
                    instr.method_hint,
                )
            })
            .collect();
        for (byte_pc, site, method_hint) in sites {
            let alloc_safepoint_id = view.safepoints.len() as native_abi::SafepointId;
            // An exotic receiver — a collection body, a dense array or a
            // primitive — reaches its builtin through a pinned realm prototype
            // rather than a shape, so its feedback names the call directly.
            if let Some(call) = site
                .and_then(|site| self.jit_collection_method_call(site, alloc_safepoint_id))
                .or_else(|| {
                    site.and_then(|site| self.jit_array_method_call(site, alloc_safepoint_id))
                })
                .or_else(|| self.jit_primitive_method_call(method_hint))
            {
                if reserve_guarded_entry_safepoint(view, &call) {
                    view.guarded_method_calls.insert(byte_pc, call);
                }
                continue;
            }
            let Some(site) = site else {
                continue;
            };
            let Some(MethodCallFeedback::MonoNativeLeaf {
                stub_id,
                method_field,
                recv_shape,
                prototype,
            }) = self.method_target_feedback(site)
            else {
                continue;
            };
            // Feedback names shapes by id; a shape collected since then leaves
            // the site on the runtime path.
            let Some(recv_shape_handle) = self.shape_runtime.handle_for_id(recv_shape) else {
                continue;
            };
            let holder = match &prototype.validity {
                None => jit::JitMethodHolder::Receiver,
                Some(cell) => {
                    let Some(validity) = self.bake_prototype_validity(cell) else {
                        continue;
                    };
                    let Some(root) = self.bake_shape_id(prototype.holder_root) else {
                        continue;
                    };
                    jit::JitMethodHolder::Prototype { validity, root }
                }
            };
            let Some(declaration) = crate::jit_static_native::jit_leaf_builtin(stub_id)
                .filter(|declaration| !declaration.this_operand)
            else {
                continue;
            };
            // A builtin this isolate never installed has no external-ref
            // index, so no live receiver could carry its identity.
            let Some(builtin_native_ref) =
                crate::jit_static_native::jit_static_call_ref(stub_id, &self.gc_heap)
            else {
                continue;
            };
            let Some(shape) = self.bake_shape(recv_shape_handle) else {
                continue;
            };
            view.guarded_method_calls.insert(
                byte_pc,
                jit::JitGuardedMethodCall {
                    receiver: jit::JitGuardedReceiver::Shape { shape },
                    holder,
                    method_field,
                    builtin_native_ref,
                    entry_stub_id: stub_id,
                    safepoint_id: native_abi::NO_SAFEPOINT,
                    argument_count: declaration.argument_count,
                },
            );
        }
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

    /// Resolve the stable entry cell for one compiler-native call.
    ///
    /// The registry publishes an optimizing generation only when it advertises
    /// safe stack-owned cold deoptimization; otherwise the current baseline
    /// remains selected. The returned function-cell address survives every
    /// later generation replacement.
    pub(crate) fn current_direct_callee_plan(
        &self,
        function: &CodeBlock,
    ) -> Option<jit::JitDirectCallPlan> {
        self.jit_code_registry.direct_call_plan(function)
    }

    /// Bake direct reads for global-declarative bindings and guarded own-data
    /// slots already owned by the isolate's global object record.
    ///
    /// Global lexical cells are old-space, non-moving GC objects rooted for the
    /// lifetime of the binding. Their identity cannot be replaced by later
    /// declarations, while their contained `Value` remains mutable. Generated
    /// code may therefore read the live cell directly; a TDZ hole still enters
    /// the canonical `LoadGlobalOrThrow` stub to construct the named error.
    /// Object-record reads additionally guard the live declarative-record epoch
    /// and the global object's hidden class or dictionary slot layout, so later
    /// eval/script lexicals, deletions and redefinitions miss before reading
    /// the baked slot; appending unrelated globals keeps the proof.
    fn bake_global_lexical_loads(
        &mut self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        fid: u32,
    ) {
        // The linked FID owns its realm. A foreign call may reach compilation
        // while the caller's realm is active; those cells cannot prove this
        // source's bindings. Keep the source-aware committed operation instead.
        if self.foreign_function_realm(fid).is_some() {
            return;
        }
        for instruction in &view.instructions {
            if instruction.op(&view.code_block) != Op::LoadGlobalOrThrow {
                continue;
            }
            let Some(name_index) = instruction.const_index(&view.code_block, 1) else {
                continue;
            };
            let Some(name) = context.string_constant_str_for_function(fid, name_index) else {
                continue;
            };
            if let Some(&(cell, _)) = self.global_lexicals.get(name) {
                view.global_lexical_loads.insert(
                    instruction.byte_pc,
                    jit::JitGlobalLexicalLoad {
                        cell_offset: cell.offset(),
                    },
                );
                continue;
            }
            let (Some(hit), crate::object::PropertyLookup::Data { .. }) =
                crate::object::lookup_own_slot(self.global_this, &self.gc_heap, name)
            else {
                continue;
            };
            let state = crate::object::state(self.global_this, &self.gc_heap);
            if state.is_opaque() {
                continue;
            }
            let shape = crate::object::keyed_shape(self.global_this, &self.gc_heap);
            let (shape, dictionary) = if shape.is_null() {
                // A dictionary global object is proven by its slot layout, so
                // globals the program adds later do not retire the proof. The
                // generated read trusts this slot's kind; watching it makes a
                // redefinition advance the layout the proof guards.
                let Some(layout) =
                    crate::object::dictionary_layout(self.global_this, &self.gc_heap)
                else {
                    continue;
                };
                if !crate::object::watch_dictionary_slot(
                    self.global_this,
                    &mut self.gc_heap,
                    hit.slot,
                ) {
                    continue;
                }
                (u64::from(layout), true)
            } else {
                let Some(shape) = self.bake_shape(shape) else {
                    continue;
                };
                (u64::from(shape), false)
            };
            view.global_object_loads.insert(
                instruction.byte_pc,
                jit::JitGlobalObjectLoad {
                    shape,
                    dictionary,
                    field: crate::object::field_location_at(
                        self.global_this,
                        &self.gc_heap,
                        u32::from(hit.slot),
                    ),
                    global_lexical_epoch: self.global_lexical_epoch,
                },
            );
        }
    }

    /// Bake direct hit proofs for global-declarative bindings and guarded
    /// own-data slots already owned by the isolate's global object record.
    ///
    /// Global lexical cells are old-space, non-moving GC objects rooted for the
    /// lifetime of the binding. Their identity cannot be replaced by later
    /// declarations, while their contained `Value` remains mutable. Generated
    /// code may therefore read the live cell directly; a TDZ hole enters the
    /// schema-decoded committed binding boundary to construct the named error.
    /// Object-record reads additionally guard the live declarative-record epoch
    /// and the global object's hidden class or dictionary slot layout, so later
    /// eval/script lexicals, deletions and redefinitions miss before reading
    /// the baked slot; appending unrelated dictionary globals keeps the proof.
    /// Ordinary hits prove one eligible finalized immutable shape. Descriptor,
    /// extensibility and lookup-state changes install another identity. Writes
    /// refuse prototype-role shapes at admission; dictionary hits also prove
    /// their dynamic state and watched slot layout before any effect.
    fn bake_binding_hit_proofs(
        &mut self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        fid: u32,
    ) {
        // The linked FID owns its realm. A foreign call may reach compilation
        // while the caller's realm is active; those cells cannot prove this
        // source's bindings. Keep the source-aware committed operation instead.
        if self.foreign_function_realm(fid).is_some() {
            return;
        }
        for instruction in &view.instructions {
            let op = instruction.op(&view.code_block);
            let Some(binding) = otter_bytecode::opcode_schema::opcode_schema(op).binding else {
                continue;
            };
            let (name_operand, writing) = match binding {
                otter_bytecode::opcode_schema::BindingSemantics::Read(
                    otter_bytecode::opcode_schema::BindingRead::Global { name, .. }
                    | otter_bytecode::opcode_schema::BindingRead::Exists { name, .. },
                ) => (name, false),
                otter_bytecode::opcode_schema::BindingSemantics::Write(
                    otter_bytecode::opcode_schema::BindingWrite::Global { name, .. }
                    | otter_bytecode::opcode_schema::BindingWrite::GlobalChecked { name, .. },
                ) => (name, true),
                _ => continue,
            };
            let Some(name_index) =
                instruction.const_index(&view.code_block, usize::from(name_operand))
            else {
                continue;
            };
            let Some(name) = context.string_constant_str_for_function(fid, name_index) else {
                continue;
            };
            if let Some(&(cell, is_const)) = self.global_lexicals.get(name) {
                view.binding_hit_proofs.insert(
                    instruction.byte_pc,
                    jit::BindingHitProof::GlobalLexical {
                        cell_offset: cell.offset(),
                        writable: !is_const,
                    },
                );
                continue;
            }
            let (Some(hit), crate::object::PropertyLookup::Data { flags, .. }) =
                crate::object::lookup_own_slot(self.global_this, &self.gc_heap, name)
            else {
                continue;
            };
            let state = crate::object::state(self.global_this, &self.gc_heap);
            if state.is_opaque() || (writing && state.is_prototype()) {
                continue;
            }
            let shape = crate::object::keyed_shape(self.global_this, &self.gc_heap);
            let (shape, dictionary) = if shape.is_null() {
                // A dictionary global object is proven by its slot layout, so
                // globals the program adds later do not retire the proof. The
                // generated read trusts this slot's kind; watching it makes a
                // redefinition advance the layout the proof guards.
                let Some(layout) =
                    crate::object::dictionary_layout(self.global_this, &self.gc_heap)
                else {
                    continue;
                };
                if !crate::object::watch_dictionary_slot(
                    self.global_this,
                    &mut self.gc_heap,
                    hit.slot,
                ) {
                    continue;
                }
                (u64::from(layout), true)
            } else {
                let Some(shape) = self.bake_shape(shape) else {
                    continue;
                };
                (u64::from(shape), false)
            };
            view.binding_hit_proofs.insert(
                instruction.byte_pc,
                jit::BindingHitProof::GlobalObject {
                    shape,
                    dictionary,
                    field: crate::object::field_location_at(
                        self.global_this,
                        &self.gc_heap,
                        u32::from(hit.slot),
                    ),
                    global_lexical_epoch: self.global_lexical_epoch,
                    writable: flags.writable(),
                },
            );
        }
    }

    /// Canonicalize every string literal needed by `fid` before snapshotting.
    ///
    /// Existing canonical cells and functions without literals are
    /// allocation-free and need no active frame-root provider. The first cold
    /// literal may allocate and therefore requires the current activation stack
    /// to be registered with the collector. Allocation failure only declines
    /// this optional compile; it does not publish a pending JavaScript throw or
    /// retain an error for later execution.
    fn prewarm_literal_cells(&mut self, context: &ExecutionContext, fid: u32) -> Option<()> {
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
            if let Some(cell) = self.literal_cells.get(&key) {
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

    /// Publish direct reads of already-canonical string and BigInt literals.
    ///
    /// The isolate owns boxed `Value` cells: hash-table growth may move a box
    /// but never its allocation, and root tracing rewrites the cell after a
    /// moving collection. Every published site is therefore a leaf relocation
    /// load; cold materialization was completed before the snapshot existed.
    fn bake_literal_cells(
        &self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        fid: u32,
    ) -> Option<()> {
        let owner = context.for_function(fid).ok()?;
        for instruction in &view.instructions {
            let op = instruction.op(&view.code_block);
            if !matches!(op, Op::LoadString | Op::LoadBigInt) {
                continue;
            }
            let constant = instruction.const_index(&view.code_block, 1)?;
            let key = owner.constant_cache_key(constant);
            let cell = self.literal_cells.get(&key)?;
            if !literal_matches(op, cell) {
                return None;
            }
            view.literal_cells.insert(
                instruction.byte_pc,
                jit::JitLiteralCell {
                    cell_addr: std::ptr::from_ref::<Value>(cell.as_ref()) as usize,
                },
            );
        }
        Some(())
    }

    /// Bake one spliced body's own compile inputs.
    ///
    /// A body compiled inside another function resolves its constants, global
    /// cells, hidden classes and call plans against *itself*, exactly as it
    /// would as an outermost function. Only a source in the active realm may
    /// prepare a splice: another realm's body cannot inherit the physical
    /// caller's global cells. Nested candidates share the root budget and use
    /// this body's own feedback; the compiler decides which bodies to splice.
    fn bake_inline_body(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        tier: jit_debug::JitDebugTier,
        budget: &mut InlineSnapshotBudget,
    ) -> Option<std::sync::Arc<jit::JitCompileSnapshot>> {
        // An inline body shares the physical caller's active-global cells.
        // Source identity alone does not make a foreign realm's globals active.
        // Refuse the optional splice before preparing any ambient proofs.
        let small = context.exec_function(fid).is_some_and(|function| {
            function.bytecode_byte_len() <= jit::JIT_SMALL_INLINE_BYTECODE_BYTES
        });
        if self.jit_retraining_blocks(fid)
            || self.foreign_function_realm(fid).is_some()
            || !budget.enter(small)
        {
            return None;
        }
        let result = (|| {
            self.prewarm_literal_cells(context, fid)?;
            let mut body = context.jit_compile_snapshot(fid)?;
            self.bake_literal_allocations(&mut body, context, fid)?;
            Self::bake_typed_array_layout(&mut body);
            Self::bake_string_layout(&mut body);
            self.bake_literal_cells(&mut body, context, fid)?;
            self.bake_global_lexical_loads(&mut body, context, fid);
            self.bake_binding_hit_proofs(&mut body, context, fid);
            self.bake_property_cache_ir(&mut body, context);
            self.bake_call_site_plans(&mut body, context, fid, tier, budget);
            self.bake_guarded_method_calls(&mut body);
            self.bake_instanceof_cells(&mut body);
            self.bake_forward_apply(&mut body);
            self.bake_element_accesses(&mut body);
            Self::bake_context_allocations(&mut body);
            Self::bake_closure_allocations(&mut body, context);
            self.bake_optimized_exit_profile(&mut body, fid);
            Some(std::sync::Arc::new(body))
        })();
        budget.leave();
        result
    }

    /// Name `%Function.prototype.apply%` for a body that forwards its
    /// arguments, so generated code can prove a forwarding site's method.
    fn bake_forward_apply(&self, view: &mut jit::JitCompileSnapshot) {
        let forwards = view
            .instructions
            .iter()
            .any(|instruction| instruction.op(&view.code_block) == Op::CallForwardArguments);
        if forwards {
            view.forward_apply_native_ref =
                crate::native_function::VmIntrinsicFunction::FunctionPrototypeApply
                    .native_ref(&self.gc_heap);
        }
    }

    /// Bake the typed reasons at which earlier optimized generations of `fid`
    /// exited, and widen the baked feedback of those instructions.
    ///
    /// The exit profile is evidence the operand observations cannot carry: an
    /// int32 result that overflowed, or a value a guard refused. Folding it
    /// into the per-instruction feedback means a rebuilt generation — or a
    /// caller splicing this body inline — does not emit the speculation that
    /// already exited.
    fn bake_optimized_exit_profile(&self, snapshot: &mut jit::JitCompileSnapshot, fid: u32) {
        snapshot.parameter_widening = self
            .jit_parameter_widening
            .get(&fid)
            .cloned()
            .unwrap_or_default();
        let code_block = &snapshot.code_block;
        snapshot.optimized_exit_reasons = self
            .jit_optimized_exit_profiles
            .iter()
            // An insufficient-feedback exit only collected feedback, and a
            // runtime transition resumed a call no speculation guarded; the
            // site speculates again from what it has observed since. So does
            // a property site whose inline cache learned another receiver
            // program after its shape guard failed: the receiver that left
            // is now part of the feedback.
            .filter_map(|(&(profile_fid, pc, reason), profile)| {
                let relearned = reason == native_abi::ExitReason::ShapeGuard
                    && profile.feedback_population.is_some_and(|population| {
                        code_block
                            .property_site_population(pc as usize)
                            .is_some_and(|current| current != population)
                    });
                (profile_fid == fid
                    && !relearned
                    && !matches!(
                        reason,
                        native_abi::ExitReason::InsufficientFeedback
                            | native_abi::ExitReason::RuntimeTransition
                    ))
                .then_some((pc, reason))
            })
            .fold(
                std::collections::BTreeMap::new(),
                |mut exits, (pc, reason)| {
                    exits.entry(pc).or_default().insert(reason);
                    exits
                },
            );
        for &pc in snapshot.optimized_exit_reasons.keys() {
            if let Some(instruction) = snapshot.instructions.get_mut(pc as usize) {
                instruction.note_optimized_exit();
            }
        }
        // A site that an earlier generation left to collect feedback has
        // executed since. An element site that still records no family meets
        // receivers no generated access describes, and is no longer unseen.
        for &(profile_fid, pc, reason) in self.jit_optimized_exit_profiles.keys() {
            if profile_fid == fid && reason == native_abi::ExitReason::InsufficientFeedback {
                snapshot.feedback_exits.insert(pc);
                if let Some(instruction) = snapshot.instructions.get(pc as usize) {
                    snapshot.unseen_element_sites.remove(&instruction.byte_pc);
                }
            }
        }
    }

    /// Bake compiler-native direct-call plans and inline-candidate bodies for
    /// `fid`'s call sites.
    ///
    /// Fixed/spread ordinary-call candidates remain monomorphic. Forwarded
    /// arguments admit the bounded ordinary target population; method calls may
    /// contain a bounded, most-frequent-first polymorphic chain. Every generated
    /// target is a synchronous bytecode function with one current non-OSR
    /// installed entry; it reaches its context through its SELF closure.
    /// Generated linkage binds
    /// both strict/lexical and unbound sloppy-global `this`; an explicitly bound
    /// sloppy closure misses before entry. The emitter applies the final
    /// pure-leaf / size / arity test to the separate monomorphic inline tables.
    pub(crate) fn bake_inline_callees(
        &mut self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        fid: u32,
        tier: jit_debug::JitDebugTier,
    ) {
        self.bake_call_site_plans(view, context, fid, tier, &mut InlineSnapshotBudget::new());
    }

    /// Call-site plan baking shared by an outermost body and a spliced one.
    ///
    /// Every body owns its nested candidate tables. One depth/work
    /// budget bounds the whole tree independently of generated-entry tiering.
    fn bake_call_site_plans(
        &mut self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        fid: u32,
        tier: jit_debug::JitDebugTier,
        budget: &mut InlineSnapshotBudget,
    ) {
        let call_sites: Vec<_> = view
            .instructions
            .iter()
            .filter_map(|instr| {
                let instruction_pc = instr.instruction_pc(&view.code_block);
                let state = view
                    .code_block
                    .call_distribution_at(instruction_pc as usize)?;
                let op = instr.op(&view.code_block);
                Some((instruction_pc, instr.byte_pc, op, state))
            })
            .collect();
        for (instruction_pc, call_byte_pc, op, state) in call_sites {
            let is_construct = matches!(
                op,
                Op::New | Op::NewSpread | Op::SuperConstruct | Op::SuperConstructSpread
            );
            let unresolved_call_kind = match op {
                Op::New | Op::NewSpread => jit::JitDirectCallKind::Construct,
                Op::SuperConstruct | Op::SuperConstructSpread => {
                    jit::JitDirectCallKind::SuperConstruct
                }
                _ => jit::JitDirectCallKind::Plain,
            };
            let targets: Vec<_> = match state {
                feedback::CallSiteDistribution::Mono(target) => vec![target],
                feedback::CallSiteDistribution::Poly(targets) if op == Op::CallForwardArguments => {
                    self.record_jit_inline_candidate(
                        fid,
                        instruction_pc,
                        tier,
                        None,
                        Some(jit_debug::JitInlineRejectionReason::Polymorphic),
                    );
                    targets.iter().copied().collect()
                }
                state => {
                    let reason = match state {
                        feedback::CallSiteDistribution::Poly(_) => {
                            jit_debug::JitInlineRejectionReason::Polymorphic
                        }
                        feedback::CallSiteDistribution::Megamorphic => {
                            jit_debug::JitInlineRejectionReason::Megamorphic
                        }
                        feedback::CallSiteDistribution::Mono(_) => unreachable!(),
                    };
                    self.record_jit_inline_candidate(fid, instruction_pc, tier, None, Some(reason));
                    continue;
                }
            };
            let target_count = targets.len() as u32;
            for (target_index, target) in targets.into_iter().enumerate() {
                let target_index = target_index as u32;
                let function_prototype_call = matches!(
                    target.target,
                    feedback::OrdinaryCallTarget::FunctionPrototypeCall(_)
                );
                let callee_fid = match target.target {
                    feedback::OrdinaryCallTarget::Native => {
                        view.native_calls
                            .insert(call_byte_pc, jit::JitNativeCall::Native);
                        continue;
                    }
                    feedback::OrdinaryCallTarget::Bytecode(callee_fid)
                    | feedback::OrdinaryCallTarget::FunctionPrototypeCall(callee_fid) => callee_fid,
                    feedback::OrdinaryCallTarget::StaticNative(stub_id) => {
                        let name = crate::native_abi::runtime_stub_name(stub_id);
                        let declaration = crate::jit_static_native::jit_leaf_builtin(stub_id)
                            .expect("native leaf call feedback names a declared entry");
                        // Feedback recorded a call to this builtin, so the isolate
                        // installed it and its external-ref index exists.
                        let builtin_native_ref =
                            crate::jit_static_native::jit_static_call_ref(stub_id, &self.gc_heap)
                                .expect(
                                    "a builtin the site already called is interned in this isolate",
                                );
                        view.native_calls.insert(
                            call_byte_pc,
                            jit::JitNativeCall::Leaf(jit::JitStaticNativeCall {
                                builtin_native_ref,
                                leaf_stub_id: stub_id,
                                argument_count: declaration.argument_count,
                            }),
                        );
                        self.record_jit_inline_candidate(
                            fid,
                            instruction_pc,
                            tier,
                            None,
                            Some(jit_debug::JitInlineRejectionReason::StaticNative {
                                target: name,
                            }),
                        );
                        self.record_jit_static_native_call_plan(fid, instruction_pc, tier, name);
                        continue;
                    }
                };
                let Ok(callee_context) = context.for_function(callee_fid) else {
                    self.record_jit_inline_candidate(
                        fid,
                        instruction_pc,
                        tier,
                        Some(callee_fid),
                        Some(jit_debug::JitInlineRejectionReason::MissingCallee),
                    );
                    self.record_jit_direct_call_plan(
                        unresolved_call_kind,
                        fid,
                        instruction_pc,
                        tier,
                        callee_fid,
                        target_index,
                        target_count,
                        jit_debug::JitDirectCallPlanOutcome::Rejected {
                            reason: jit_debug::JitDirectCallRejectionReason::MissingCallee,
                        },
                    );
                    continue;
                };
                let Some(callee) = callee_context.exec_function(callee_fid) else {
                    self.record_jit_inline_candidate(
                        fid,
                        instruction_pc,
                        tier,
                        Some(callee_fid),
                        Some(jit_debug::JitInlineRejectionReason::MissingCallee),
                    );
                    self.record_jit_direct_call_plan(
                        unresolved_call_kind,
                        fid,
                        instruction_pc,
                        tier,
                        callee_fid,
                        target_index,
                        target_count,
                        jit_debug::JitDirectCallPlanOutcome::Rejected {
                            reason: jit_debug::JitDirectCallRejectionReason::MissingCallee,
                        },
                    );
                    continue;
                };
                let direct_ineligible = !callee.admits_generated_call(unresolved_call_kind);
                // An `arguments` body reads the actual-argument window its
                // generated caller publishes, so it still takes direct linkage;
                // spliced into the caller it would have no window to read.
                let inline_ineligible = direct_ineligible || callee.requires_argument_frame();
                if inline_ineligible {
                    self.record_jit_inline_candidate(
                        fid,
                        instruction_pc,
                        tier,
                        Some(callee_fid),
                        Some(jit_debug::JitInlineRejectionReason::Ineligible {
                            generator: callee.is_generator,
                            async_function: callee.is_async,
                            async_generator: callee.is_async_generator,
                            needs_arguments: callee.needs_arguments,
                            has_rest: callee.has_rest,
                            contains_direct_eval: callee.contains_direct_eval,
                            derived_constructor: callee.is_derived_constructor,
                            makes_function: callee.makes_function,
                        }),
                    );
                }
                if direct_ineligible {
                    self.record_jit_direct_call_plan(
                        unresolved_call_kind,
                        fid,
                        instruction_pc,
                        tier,
                        callee_fid,
                        target_index,
                        target_count,
                        jit_debug::JitDirectCallPlanOutcome::Rejected {
                            reason: jit_debug::JitDirectCallRejectionReason::IneligibleFunction,
                        },
                    );
                    continue;
                }
                let direct_call_outcome = if let Some(mut plan) =
                    self.current_direct_callee_plan(callee)
                {
                    debug_assert_eq!(plan.function_id, callee_fid);
                    plan.callee_cell = self.bake_callee_identity_cell();
                    let receiver_allocation = if is_construct && !callee.is_derived_constructor {
                        view.code_block
                            .construct_family_at(instruction_pc as usize)
                            .and_then(|family| {
                                self.bake_receiver_allocation_plan(callee_fid, family)
                            })
                    } else {
                        None
                    };
                    let callee = jit::JitDirectCallee {
                        plan,
                        receiver_allocation,
                    };
                    if is_construct {
                        view.direct_constructs.insert(call_byte_pc, callee);
                    } else if function_prototype_call {
                        if let Some(proof) =
                            self.function_prototype_call_proof(view, context, op, instruction_pc)
                        {
                            view.function_prototype_calls.insert(
                                call_byte_pc,
                                jit::JitFunctionPrototypeCallSite { proof, callee },
                            );
                        }
                    } else {
                        view.direct_callees
                            .entry(call_byte_pc)
                            .or_default()
                            .push(callee);
                    }
                    jit_debug::JitDirectCallPlanOutcome::Available {
                        code_object_id: plan.code_object_id,
                        target_tier: match plan.tier {
                            native_abi::NativeFrameKind::Baseline => {
                                jit_debug::JitDebugTier::Template
                            }
                            native_abi::NativeFrameKind::Optimizing => {
                                jit_debug::JitDebugTier::Optimizing
                            }
                            native_abi::NativeFrameKind::Interpreter
                            | native_abi::NativeFrameKind::Host => {
                                jit_debug::JitDebugTier::Interpreter
                            }
                        },
                        this_mode: if is_construct && callee.plan.is_derived_constructor {
                            jit::JitDirectCallThisMode::DerivedConstructor
                        } else if is_construct {
                            jit::JitDirectCallThisMode::ConstructReceiver
                        } else {
                            plan.this_mode
                        },
                    }
                } else {
                    jit_debug::JitDirectCallPlanOutcome::Rejected {
                        reason: jit_debug::JitDirectCallRejectionReason::NoEntryGeneration,
                    }
                };
                let call_kind = match (op, callee.is_derived_constructor) {
                    (Op::New | Op::NewSpread, false) => jit::JitDirectCallKind::Construct,
                    (Op::New | Op::NewSpread, true) => jit::JitDirectCallKind::DerivedConstruct,
                    (Op::SuperConstruct | Op::SuperConstructSpread, false) => {
                        jit::JitDirectCallKind::SuperConstruct
                    }
                    (Op::SuperConstruct | Op::SuperConstructSpread, true) => {
                        jit::JitDirectCallKind::DerivedSuperConstruct
                    }
                    _ => jit::JitDirectCallKind::Plain,
                };
                self.record_jit_direct_call_plan(
                    call_kind,
                    fid,
                    instruction_pc,
                    tier,
                    callee_fid,
                    target_index,
                    target_count,
                    direct_call_outcome,
                );
                if (is_construct
                    && (tier != jit_debug::JitDebugTier::Optimizing
                        || op != Op::New
                        || callee.is_derived_constructor
                        || callee.is_arrow))
                    || op == Op::CallSpread
                {
                    continue;
                }
                // Only the optimizing tier splices a reduced `f.call` target.
                let tier_splices =
                    !function_prototype_call || tier == jit_debug::JitDebugTier::Optimizing;
                if inline_ineligible
                    || target_count != 1
                    || !tier_splices
                    || !callee.admits_graph_inlining()
                {
                    continue;
                }
                let Some(body) = self.bake_inline_body(&callee_context, callee_fid, tier, budget)
                else {
                    self.record_jit_inline_candidate(
                        fid,
                        instruction_pc,
                        tier,
                        Some(callee_fid),
                        Some(jit_debug::JitInlineRejectionReason::MissingSnapshot),
                    );
                    continue;
                };
                self.record_jit_inline_candidate(fid, instruction_pc, tier, Some(callee_fid), None);
                view.inline_callees
                    .insert(call_byte_pc, jit::JitInlineCallee { body });
            }
        }

        // Method-call sites: snapshot monomorphic and polymorphic feedback for
        // `fid` first so the per-target `shape_offset_of` (which needs
        // `&mut self`) does not alias the feedback map borrow. Each snapshot is a
        // list of candidate targets — one for `Mono`, up to
        // `MAX_POLY_METHOD_TARGETS` (most-frequent first) for `Poly`.
        // `Megamorphic` sites are skipped and side-exit before method lookup.
        struct PolySnapshot {
            instruction_pc: u32,
            call_byte_pc: u32,
            targets: SmallVec<[PolyMethodTarget; MAX_POLY_METHOD_TARGETS]>,
        }
        let method_sites: Vec<PolySnapshot> =
            view.instructions
                .iter()
                .filter_map(|instr| {
                    let site = instr.property_ic_site(&view.code_block)?;
                    let state = self.method_target_feedback(site)?;
                    match state {
                        MethodCallFeedback::Mono {
                            method_fid,
                            recv_shape,
                            prototype,
                            method_field,
                        } => {
                            let mut targets: SmallVec<[PolyMethodTarget; MAX_POLY_METHOD_TARGETS]> =
                                SmallVec::new();
                            targets.push(PolyMethodTarget {
                                method_fid,
                                recv_shape,
                                prototype,
                                method_field,
                                hits: 1,
                            });
                            Some(PolySnapshot {
                                instruction_pc: instr.instruction_pc(&view.code_block),
                                call_byte_pc: instr.byte_pc,
                                targets,
                            })
                        }
                        MethodCallFeedback::Poly(observed) => {
                            let mut targets = (*observed).clone();
                            // Most-frequent target first: the common receiver shape
                            // then hits the shortest guard chain.
                            targets.sort_by_key(|t| std::cmp::Reverse(t.hits));
                            Some(PolySnapshot {
                                instruction_pc: instr.instruction_pc(&view.code_block),
                                call_byte_pc: instr.byte_pc,
                                targets,
                            })
                        }
                        // Native leaf sites are baked by their own pass: they
                        // carry an entry id rather than a callee body, so there is
                        // no inline chain to build here.
                        MethodCallFeedback::MonoNativeLeaf { .. }
                        | MethodCallFeedback::Megamorphic => None,
                    }
                })
                .collect();
        for snap in method_sites {
            let mut direct_methods = Vec::with_capacity(snap.targets.len());
            let target_count = u32::try_from(snap.targets.len()).unwrap_or(u32::MAX);
            for (target_index, target) in snap.targets.iter().enumerate() {
                let (callee_function_id, outcome) = match self.bake_one_direct_method(
                    context,
                    target,
                    u32::try_from(target_index).unwrap_or(u32::MAX),
                    target_count,
                    tier,
                    budget,
                ) {
                    Ok(method) => {
                        let plan = method.callee.plan;
                        direct_methods.push(method);
                        (
                            plan.function_id,
                            jit_debug::JitDirectCallPlanOutcome::Available {
                                code_object_id: plan.code_object_id,
                                target_tier: match plan.tier {
                                    native_abi::NativeFrameKind::Baseline => {
                                        jit_debug::JitDebugTier::Template
                                    }
                                    native_abi::NativeFrameKind::Optimizing => {
                                        jit_debug::JitDebugTier::Optimizing
                                    }
                                    native_abi::NativeFrameKind::Interpreter
                                    | native_abi::NativeFrameKind::Host => {
                                        jit_debug::JitDebugTier::Interpreter
                                    }
                                },
                                this_mode: jit::JitDirectCallThisMode::MethodReceiver,
                            },
                        )
                    }
                    Err(reason) => (
                        target.method_fid,
                        jit_debug::JitDirectCallPlanOutcome::Rejected { reason },
                    ),
                };
                self.record_jit_direct_call_plan(
                    jit::JitDirectCallKind::Method,
                    fid,
                    snap.instruction_pc,
                    tier,
                    callee_function_id,
                    u32::try_from(target_index).unwrap_or(u32::MAX),
                    target_count,
                    outcome,
                );
            }
            if !direct_methods.is_empty() {
                view.direct_methods
                    .insert(snap.call_byte_pc, direct_methods);
            }
            let mut baked: Vec<jit::JitInlineMethod> = Vec::new();
            for target in &snap.targets {
                // A body already baked for the direct call serves the leaf
                // inliner too.
                let body = view
                    .direct_methods
                    .get(&snap.call_byte_pc)
                    .and_then(|methods| {
                        methods
                            .iter()
                            .find(|method| method.guard.method_fid == target.method_fid)
                    })
                    .and_then(|method| method.body.clone());
                if let Some(method) =
                    self.bake_one_inline_method(context, target, body, tier, budget)
                {
                    baked.push(method);
                }
            }
            match baked.len() {
                0 => {}
                // A single inlinable target remains useful even when the site
                // observed several shapes: other shapes miss its guard and
                // side-exit before method lookup.
                1 => {
                    view.inline_methods
                        .insert(snap.call_byte_pc, baked.pop().unwrap());
                }
                // Two or more: emit the guarded inline chain.
                _ => {
                    view.inline_poly_methods.insert(snap.call_byte_pc, baked);
                }
            }
        }
    }

    /// Materialize one exact receiver/prototype/method-slot identity guard.
    ///
    /// Feedback keeps stable shape ids; generated code consumes compressed
    /// shape-handle offsets. Resolving every hop here keeps heap/runtime layout
    /// knowledge on the VM side.
    fn bake_method_guard(&self, target: &PolyMethodTarget) -> Option<jit::JitMethodGuard> {
        let recv_shape = self.bake_shape_id(target.recv_shape)?;
        let (prototype_validity, holder_root) = match &target.prototype.validity {
            Some(cell) => (
                Some(self.bake_prototype_validity(cell)?),
                self.bake_shape_id(target.prototype.holder_root)?,
            ),
            None => (None, 0),
        };
        Some(jit::JitMethodGuard {
            method_fid: target.method_fid,
            recv_shape,
            prototype_validity,
            holder_root,
            method_field: target.method_field,
        })
    }

    /// Bake one compiler-generated method call independently of leaf inlining.
    ///
    /// Each bounded mono/poly feedback target reaches this helper independently.
    /// The target must be an ordinary synchronous function with one
    /// entry-capable native generation. Recursive entry resolves through the
    /// same stable generation cell as every other generated call. The callee
    /// reaches its context through the exact SELF closure.
    fn bake_one_direct_method(
        &mut self,
        context: &ExecutionContext,
        target: &PolyMethodTarget,
        target_index: u32,
        target_count: u32,
        tier: jit_debug::JitDebugTier,
        budget: &mut InlineSnapshotBudget,
    ) -> Result<jit::JitDirectMethod, jit_debug::JitDirectCallRejectionReason> {
        let method_context = context
            .for_function(target.method_fid)
            .map_err(|_| jit_debug::JitDirectCallRejectionReason::MissingCallee)?;
        let method = method_context
            .exec_function(target.method_fid)
            .ok_or(jit_debug::JitDirectCallRejectionReason::MissingCallee)?;
        if !method.admits_generated_call(jit::JitDirectCallKind::Method) {
            return Err(jit_debug::JitDirectCallRejectionReason::IneligibleFunction);
        }
        let guard = self
            .bake_method_guard(target)
            .ok_or(jit_debug::JitDirectCallRejectionReason::MethodGuardUnavailable)?;
        let mut plan = self
            .current_direct_callee_plan(method)
            .ok_or(jit_debug::JitDirectCallRejectionReason::NoEntryGeneration)?;
        debug_assert_eq!(plan.function_id, target.method_fid);
        plan.callee_cell = self.bake_callee_identity_cell();
        // An optimizing caller may build the method in place of the call;
        // one that reads its actual arguments needs a frame of its own.
        let body = (tier == jit_debug::JitDebugTier::Optimizing
            && !method.requires_argument_frame()
            && method.admits_graph_inlining())
        .then(|| self.bake_inline_body(&method_context, target.method_fid, tier, budget))
        .flatten();
        Ok(jit::JitDirectMethod {
            target_index,
            target_count,
            guard,
            callee: jit::JitDirectCallee {
                plan,
                receiver_allocation: None,
            },
            body,
        })
    }

    /// Bake the finalized actual new.target family observed at this source
    /// site. Unknown/provisional/changed/unsupported owners have no fit plan.
    fn bake_receiver_allocation_plan(
        &self,
        base_function_id: u32,
        family_id: u64,
    ) -> Option<jit::JitReceiverAllocationPlan> {
        let (layout, new_target_function_id, new_target_is_class) =
            self.finalized_constructor_layout_for_id(family_id, base_function_id)?;
        let (root_handle, prototype) = self
            .gc_heap
            .read_payload(layout, |body| (body.root(), body.prototype()));
        // Generated code proves only the family identity; the family fixes an
        // ordinary prototype object, never an exotic one.
        if crate::object::shape_body::state_of(root_handle).is_provisional()
            || prototype.as_object().is_none()
        {
            return None;
        }
        let root_id = crate::object::shape_body::id_of(root_handle);
        let inline_capacity = crate::object::shape_body::inline_capacity_of(root_handle);
        if inline_capacity > crate::object::MAX_INLINE_CAPACITY {
            return None;
        }
        let prototype_root = self.bake_shape(root_handle)?;
        let cell = self
            .gc_heap
            .read_payload(layout, |body| body.preparation_validity())?;
        let prototype_validity = self.bake_prototype_validity(&cell)?;
        let (receiver_shape, initial_field_count) = match (
            self.simple_constructor_init_cache
                .get(&base_function_id)
                .and_then(Option::as_ref),
            self.simple_constructor_shape_cache
                .get(&(base_function_id, root_id))
                .copied(),
        ) {
            (Some(init), Some(shape))
                if init.fields.len() <= inline_capacity
                    && !crate::object::shape_body::state_of(shape).is_provisional()
                    && crate::object::shape_body::inline_capacity_of(shape) == inline_capacity =>
            {
                debug_assert_eq!(
                    crate::object::shape_property_count(shape, &self.gc_heap) as usize,
                    init.fields.len(),
                    "source initial fields and final shape diverged"
                );
                (
                    self.bake_shape(shape)?,
                    u8::try_from(init.fields.len()).ok()?,
                )
            }
            _ => (0, 0),
        };
        Some(jit::JitReceiverAllocationPlan {
            new_target_function_id,
            new_target_is_class,
            family_id,
            receiver_shape,
            initial_field_count,
            inline_capacity: u8::try_from(inline_capacity).ok()?,
            prototype_validity: Some(prototype_validity),
            prototype_root,
        })
    }

    /// Bake one inline-method candidate body for a `(method, receiver shape)`
    /// target, resolving its sealed property loads/stores to banked field
    /// locations against the receiver shape. Returns `None` when the method shape
    /// is ineligible (generator/async/derived-constructor/etc.), its view is
    /// missing, or any body property fails to resolve to a sealed receiver slot.
    /// Shared by the monomorphic and polymorphic method-inline bake paths.
    fn bake_one_inline_method(
        &mut self,
        context: &ExecutionContext,
        target: &PolyMethodTarget,
        body: Option<std::sync::Arc<jit::JitCompileSnapshot>>,
        tier: jit_debug::JitDebugTier,
        budget: &mut InlineSnapshotBudget,
    ) -> Option<jit::JitInlineMethod> {
        let method_context = context.for_function(target.method_fid).ok()?;
        let context = &*method_context;
        let method = context.exec_function(target.method_fid)?;
        if method.is_generator
            || method.is_async
            || method.is_async_generator
            || method.requires_argument_frame()
            || method.has_rest
            || method.contains_direct_eval
            || method.is_derived_constructor
            || method.makes_function
        {
            return None;
        }
        let method_view = match body {
            Some(body) => body,
            None => self.bake_inline_body(context, target.method_fid, tier, budget)?,
        };
        // Resolve every body `LoadProperty`/`StoreProperty` to a logical own
        // field; bail out if a property is absent or an accessor. The shape
        // fixes its bank and relative index. A receiver property resolves against
        // the identity-guarded receiver shape (no per-op guard); a non-receiver
        // property falls back to its own monomorphic site feedback and records
        // the shape the inliner must guard. Loads carry the name at operand 2,
        // stores at operand 1.
        let mut prop_fields: rustc_hash::FxHashMap<u32, crate::object::FieldLocation> =
            rustc_hash::FxHashMap::default();
        let mut prop_shapes: rustc_hash::FxHashMap<u32, u32> = rustc_hash::FxHashMap::default();
        for instr in &method_view.instructions {
            let name_operand = match instr.op(&method_view.code_block) {
                Op::LoadProperty => 2,
                Op::StoreProperty => 1,
                _ => continue,
            };
            let otter_bytecode::Operand::ConstIndex(name_idx) =
                instr.operand(&method_view.code_block, name_operand)?
            else {
                return None;
            };
            let key = context.property_atom(name_idx)?;
            let recv_shape = self.shape_runtime.handle_for_id(target.recv_shape)?;
            if let Some(slot) = self.shape_offset_of(recv_shape, key.name()) {
                prop_fields.insert(
                    instr.byte_pc,
                    crate::object::field_location(recv_shape, slot),
                );
                continue;
            }
            // Not a receiver property: use the op's own monomorphic own-data site
            // feedback (shape offset, slot byte). Anything else — polymorphic,
            // prototype, accessor, or unobserved — is not inlinable.
            let (shape_off, field) =
                self.monomorphic_own_property_feedback(context, &method_view.code_block, instr)?;
            prop_fields.insert(instr.byte_pc, field);
            prop_shapes.insert(instr.byte_pc, shape_off);
        }
        let guard = self.bake_method_guard(target)?;
        Some(jit::JitInlineMethod {
            body: method_view,
            guard,
            prop_fields,
            prop_shapes,
        })
    }
}

/// Admit a guarded method call only when its declared entry has machine-callable
/// code, reserving the frame-slot root window an allocating entry needs.
///
/// The family the entry id resolves in is the whole decision: a leaf or
/// mutating-leaf entry cannot collect and publishes nothing, while an
/// allocating one must name a precise map covering the caller's full register
/// window before generated code may call it.
fn reserve_guarded_entry_safepoint(
    view: &mut jit::JitCompileSnapshot,
    call: &jit::JitGuardedMethodCall,
) -> bool {
    let stub_id = call.entry_stub_id;
    if call.safepoint_id == native_abi::NO_SAFEPOINT {
        return crate::runtime_stubs::leaf_no_alloc_stub2_by_id(stub_id)
            .is_some_and(crate::runtime_stubs::LeafNoAllocStub2::is_valid)
            || crate::runtime_stubs::mutating_leaf_stub2_by_id(stub_id)
                .is_some_and(crate::runtime_stubs::MutatingLeafStub2::is_valid)
            || crate::runtime_stubs::mutating_leaf_stub3_by_id(stub_id)
                .is_some_and(crate::runtime_stubs::MutatingLeafStub3::is_valid);
    }
    if !crate::runtime_stubs::alloc_value_stub_by_id(stub_id)
        .is_some_and(|stub| stub.is_valid_for_safepoint(call.safepoint_id) && stub.has_entry())
    {
        return false;
    }
    view.safepoints.insert(
        call.safepoint_id,
        native_abi::SafepointRecord::window(call.safepoint_id, native_abi::NO_FRAME_STATE),
    );
    true
}

#[cfg(test)]
mod tests {
    use otter_bytecode::{
        BytecodeModule, Constant, Function, Instruction, Op, Operand, SourceKind,
    };

    use crate::{ActivationStack, Interpreter, Value};

    #[test]
    fn method_feedback_declines_a_wide_dictionary_holder_without_losing_its_data() {
        let mut module = crate::test_support::minimal_bytecode_module("wide-method-holder.js");
        module.constants = vec![Constant::String {
            utf16: "method".encode_utf16().collect(),
        }];
        let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
        let context = interpreter
            .link_module(module, crate::source_registry::SourceRegistry::default())
            .expect("caller context");
        interpreter.with_handle_scope(|interpreter, scope| {
            let prototype = interpreter.scoped_object_bare(scope).expect("prototype");
            for index in 0..crate::object::MAX_FAST_PROPERTIES {
                let mut object = interpreter.escape_scoped(prototype).as_object().unwrap();
                interpreter
                    .create_data_property(
                        &mut object,
                        &format!("p{index}"),
                        Value::number_i32(index as i32),
                    )
                    .expect("prototype field");
            }
            let mut object = interpreter.escape_scoped(prototype).as_object().unwrap();
            interpreter
                .create_data_property(&mut object, "method", Value::number_i32(317))
                .unwrap();
            let receiver = interpreter
                .scoped_object_with_proto(scope, prototype)
                .expect("receiver");
            let mut value = interpreter.escape_scoped(receiver);
            let mut object = value.as_object().expect("object receiver");
            interpreter.migrate_slow_to_fast(&mut object);
            value = Value::object(object);
            let property = context.property_atom_for_function(0, 0).unwrap();
            let prototype = interpreter.escape_scoped(prototype).as_object().unwrap();
            assert_eq!(
                crate::object::prototype(object, interpreter.gc_heap()),
                Some(prototype),
                "migration retains the exact inherited holder"
            );
            assert!(!crate::object::keyed_shape(object, interpreter.gc_heap()).is_null());
            assert!(crate::object::is_dictionary(
                prototype,
                interpreter.gc_heap()
            ));
            assert!(crate::object::keyed_shape(prototype, interpreter.gc_heap()).is_null());
            assert_eq!(
                crate::object::with_properties(prototype, interpreter.gc_heap(), |properties| {
                    properties.keys().count()
                }),
                crate::object::MAX_FAST_PROPERTIES as usize + 1,
                "the real holder exceeds ordinary migration capacity"
            );
            let descriptor =
                crate::object::get_own_descriptor(prototype, interpreter.gc_heap(), "method")
                    .expect("wide holder retains its actual own data descriptor");
            assert!(descriptor.writable() && descriptor.enumerable() && descriptor.configurable());
            assert!(
                matches!(descriptor.kind, crate::object::DescriptorKind::Data { value }
                if value == Value::number_i32(317))
            );
            assert!(
                crate::cache_ir::resolve_atom_data_slot(object, interpreter.gc_heap(), property,)
                    .is_none(),
                "the central runtime CacheIR owner refuses dictionary holder proofs"
            );
            assert!(
                interpreter
                    .method_site_for_receiver(&context, 0, 0, &mut value)
                    .is_none()
            );
            assert_eq!(
                crate::object::get(value.as_object().unwrap(), interpreter.gc_heap(), "method"),
                Some(Value::number_i32(317))
            );
        });
    }

    #[test]
    fn retraining_blocks_prewarm_and_inline_body_baking_before_preparation() {
        let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
        let context = interpreter
            .link_module(
                crate::test_support::minimal_bytecode_module("retraining-bake.js"),
                crate::source_registry::SourceRegistry::default(),
            )
            .unwrap();
        interpreter.optimizing_tier_policy.begin_retraining(0);
        assert!(interpreter.prewarm_literal_cells(&context, 0).is_none());
        let mut budget = super::InlineSnapshotBudget::new();
        assert!(
            interpreter
                .bake_inline_body(
                    &context,
                    0,
                    crate::jit_debug::JitDebugTier::Optimizing,
                    &mut budget
                )
                .is_none()
        );
        assert!(interpreter.literal_cells.is_empty());
    }

    #[test]
    fn repeated_inline_site_history_suppresses_the_source_snapshot_guard() {
        let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
        let context = interpreter
            .link_module(
                crate::test_support::minimal_bytecode_module("inline-source-history.js"),
                crate::source_registry::SourceRegistry::default(),
            )
            .unwrap();
        let exit = crate::native_abi::SideExit::new(
            37,
            crate::native_abi::ExitReason::IdentityGuard,
            crate::native_abi::ExitAction::Recompile,
        );
        interpreter.note_jit_optimized_bail_at(&context, 99, 99, exit, (0, 0), false);
        interpreter.note_jit_optimized_bail_at(&context, 100, 100, exit, (0, 0), false);
        let mut source = context.jit_compile_snapshot(0).unwrap();
        interpreter.bake_optimized_exit_profile(&mut source, 0);
        assert!(source.optimized_exit_reasons[&0].contains(&exit.reason()));
        let mut outer = context.jit_compile_snapshot(0).unwrap();
        interpreter.bake_optimized_exit_profile(&mut outer, 99);
        assert!(
            outer.optimized_exit_reasons.is_empty(),
            "the diagnostic outer resume PC is not the guarded source operation"
        );
    }

    #[test]
    fn fresh_interpreter_has_no_executable_code_residency() {
        let interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
        assert_eq!(interpreter.jit_code_residency().code_bytes, 0);
        assert_eq!(interpreter.jit_code_residency().unique_code_objects, 0);
    }

    #[test]
    fn prewarm_publishes_one_shared_stable_cell_for_all_literal_sites() {
        let module = BytecodeModule {
            module: "lazy-string-cell.js".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::JavaScript,
            functions: vec![Function {
                id: 0,
                name: "literal".to_string(),
                locals: 2,
                code: vec![
                    Instruction {
                        pc: 0,
                        op: Op::LoadString,
                        operands: vec![Operand::Register(0), Operand::ConstIndex(0)],
                    },
                    Instruction {
                        pc: 1,
                        op: Op::LoadString,
                        operands: vec![Operand::Register(1), Operand::ConstIndex(0)],
                    },
                    Instruction {
                        pc: 2,
                        op: Op::ReturnValue,
                        operands: vec![Operand::Register(1)],
                    },
                ]
                .into(),
                ..Function::default()
            }],
            constants: vec![Constant::String {
                utf16: "cold literal".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
            function_source: None,
        };
        let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
        let context = interpreter
            .link_module(module, crate::source_registry::SourceRegistry::default())
            .expect("valid bytecode fixture");
        assert!(
            interpreter.prewarm_literal_cells(&context, 0).is_none(),
            "a cold allocation without published frame roots must decline"
        );
        assert_eq!(interpreter.literal_cells.len(), 0);

        let mut stack = ActivationStack::new();
        interpreter.with_runtime_turn(&mut stack, |turn| {
            let (interpreter, _) = turn.into_parts();
            interpreter
                .prewarm_literal_cells(&context, 0)
                .expect("rooted prewarm");
        });
        assert!(
            interpreter.prewarm_literal_cells(&context, 0).is_some(),
            "an already-prepared function needs no active root provider"
        );

        let mut snapshot = context.jit_compile_snapshot(0).expect("snapshot");
        interpreter
            .bake_literal_cells(&mut snapshot, &context, 0)
            .expect("prepared bake");
        assert_eq!(snapshot.literal_cells.len(), 2);
        let cells = snapshot.literal_cells.values().copied().collect::<Vec<_>>();
        let first = cells[0];
        let second = cells[1];
        assert_eq!(first.cell_addr, second.cell_addr);
        assert!(unsafe { *(first.cell_addr as *const Value) }.is_string());

        interpreter.force_gc().expect("move the prepared literal");
        assert!(unsafe { *(first.cell_addr as *const Value) }.is_string());
        assert!(
            snapshot
                .literal_cells
                .values()
                .all(|cell| cell.cell_addr == first.cell_addr)
        );

        // No production path inserts a cell before successful allocation. If
        // corrupt state nevertheless exposes a hole, preparation and baking
        // both decline instead of publishing that state to generated code.
        **interpreter
            .literal_cells
            .get_mut(&context.constant_cache_key(0))
            .expect("prepared cell") = Value::hole();
        assert!(interpreter.prewarm_literal_cells(&context, 0).is_none());
        let mut invalid = context
            .jit_compile_snapshot(0)
            .expect("invalid snapshot input");
        assert!(
            interpreter
                .bake_literal_cells(&mut invalid, &context, 0)
                .is_none()
        );
        assert!(invalid.literal_cells.is_empty());
    }

    #[test]
    fn caller_snapshot_survives_forced_gc_then_nested_target_prewarm() {
        let literal_function = |id, constant, name: &str| Function {
            id,
            name: name.to_string(),
            locals: 1,
            code: vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadString,
                    operands: vec![Operand::Register(0), Operand::ConstIndex(constant)],
                },
                Instruction {
                    pc: 1,
                    op: Op::ReturnValue,
                    operands: vec![Operand::Register(0)],
                },
            ]
            .into(),
            ..Function::default()
        };
        let module = BytecodeModule {
            module: "nested-prewarm-gc.js".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::JavaScript,
            functions: vec![
                literal_function(0, 0, "caller"),
                literal_function(1, 1, "nestedTarget"),
            ],
            constants: vec![
                Constant::String {
                    utf16: "caller literal".encode_utf16().collect(),
                },
                Constant::String {
                    utf16: "nested target literal".encode_utf16().collect(),
                },
            ],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
            function_source: None,
        };
        let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
        let context = interpreter
            .link_module(module, crate::source_registry::SourceRegistry::default())
            .expect("valid bytecode fixture");
        let mut stack = ActivationStack::new();
        let (caller, nested) = interpreter.with_runtime_turn(&mut stack, |turn| {
            let (interpreter, _) = turn.into_parts();
            interpreter
                .prewarm_literal_cells(&context, 0)
                .expect("caller prewarm");
            let mut caller = context.jit_compile_snapshot(0).expect("caller snapshot");
            interpreter
                .bake_literal_cells(&mut caller, &context, 0)
                .expect("caller bake");
            let caller_cell = caller.literal_cells[&0].cell_addr;

            // Direct-target baking owns this ordering: the caller snapshot is
            // already live when preparing a nested body may move the heap.
            // Snapshot state is GC-stable and its literal cell is traced and
            // rewritten in place.
            interpreter.collect_minor_tracing_runtime_roots();
            assert!(unsafe { *(caller_cell as *const Value) }.is_string());

            interpreter
                .prewarm_literal_cells(&context, 1)
                .expect("nested target prewarm after moving GC");
            let mut nested = context.jit_compile_snapshot(1).expect("nested snapshot");
            interpreter
                .bake_literal_cells(&mut nested, &context, 1)
                .expect("nested target bake");
            assert_eq!(caller.literal_cells[&0].cell_addr, caller_cell);
            assert!(unsafe { *(caller_cell as *const Value) }.is_string());
            (caller, nested)
        });

        assert_eq!(caller.literal_cells.len(), 1);
        assert_eq!(nested.literal_cells.len(), 1);
        let result = interpreter
            .run(&context)
            .expect("caller executes after snapshot/GC/nested prewarm");
        assert!(result.is_string());
    }

    #[test]
    fn finalized_shape_baker_refuses_unsupported_state_without_retaining_a_proof() {
        use crate::object::{LookupFact, ShapeState, shape_body};
        let mut interpreter = Interpreter::new().expect("shape baker runtime");
        let context = interpreter
            .link_module(
                crate::test_support::minimal_bytecode_module(
                    "shape-baker-nonordinary-prototype.js",
                ),
                crate::source_registry::SourceRegistry::default(),
            )
            .expect("actual linked function prototype");
        let prototype_function = Value::function(context.function_base());
        let mut stack = ActivationStack::new();
        interpreter.with_runtime_turn(&mut stack, |turn| {
            let (vm, _) = turn.into_parts();
            let ordinary = vm
                .object_root(None, 4, ShapeState::ORDINARY)
                .expect("ordinary root");
            let ordinary_id = shape_body::id_of(ordinary);
            assert_eq!(vm.bake_shape(ordinary), Some(ordinary.offset()));
            assert_eq!(vm.bake_shape_id(ordinary_id), Some(ordinary.offset()));
            // Reads can prove finalized non-extensible/prototype ordinary
            // state; write emitters retain their independent role guard.
            for state in [
                ShapeState::ORDINARY.with_extensible(false),
                ShapeState::ORDINARY.with_prototype_role(true),
            ] {
                let shape = vm
                    .object_root(None, 4, state)
                    .expect("eligible ordinary state root");
                assert_eq!(vm.bake_shape(shape), Some(shape.offset()));
                assert_eq!(
                    vm.bake_shape_id(shape_body::id_of(shape)),
                    Some(shape.offset())
                );
            }
            // A provisional constructor lineage is an immutable layout every
            // guard may name; only an allocation plan refuses its root.
            let provisional = vm
                .object_root(None, 4, ShapeState::ORDINARY.with_provisional(true))
                .expect("provisional state root");
            assert_eq!(vm.bake_shape(provisional), Some(provisional.offset()));
            assert!(crate::jit::JitObjectLiteralAllocationPlan::new(provisional, 0).is_none());
            let retained = vm.jit_compile_roots.borrow().len();
            assert_eq!(vm.bake_shape(crate::object::ShapeHandle::null()), None);
            assert_eq!(vm.jit_compile_roots.borrow().len(), retained);
            for state in [
                ShapeState::ORDINARY.with_lookup(LookupFact::StringWrapper, true),
                ShapeState::ORDINARY.with_lookup(LookupFact::MappedArguments, true),
                ShapeState::ORDINARY.with_lookup(LookupFact::HostLookup, true),
                ShapeState::ORDINARY.with_lookup(LookupFact::NonOrdinaryPrototype, true),
            ] {
                // The sole root owner derives this fact from the actual
                // prototype. A null prototype cannot fabricate that bit.
                let shape = if state.bits() & ShapeState::OPAQUE_PROTOTYPE_MASK != 0 {
                    vm.value_root(prototype_function, 4, ShapeState::ORDINARY)
                } else {
                    vm.object_root(None, 4, state)
                }
                .expect("actual immutable state root");
                if state.bits() & ShapeState::OPAQUE_PROTOTYPE_MASK != 0 {
                    assert!(matches!(
                        vm.gc_heap.read_payload(shape, shape_body::ShapeBody::prototype),
                        shape_body::ShapePrototype::Value(value) if value == prototype_function
                    ));
                }
                assert_eq!(shape_body::state_of(shape), state);
                let before = vm.jit_compile_roots.borrow().len();
                assert_eq!(vm.bake_shape(shape), None);
                assert_eq!(vm.bake_shape_id(shape_body::id_of(shape)), None);
                assert_eq!(vm.jit_compile_roots.borrow().len(), before);
                assert!(crate::jit::JitObjectLiteralAllocationPlan::new(shape, 0).is_none());
            }
            let dictionary = shape_body::dictionary_of(ordinary);
            assert!(shape_body::state_of(dictionary).is_dictionary());
            let before = vm.jit_compile_roots.borrow().len();
            assert_eq!(vm.bake_shape(dictionary), None);
            assert_eq!(vm.jit_compile_roots.borrow().len(), before);
            assert!(crate::jit::JitObjectLiteralAllocationPlan::new(dictionary, 0).is_none());

            // Pinned primitive String holders are an explicit dictionary
            // proof domain with nonvirtual names, never an ordinary shape.
            for hint in [
                crate::jit::JitMethodHint::StringCharCodeAt,
                crate::jit::JitMethodHint::StringIndexOf,
            ] {
                let call = vm
                    .jit_primitive_method_call(hint)
                    .expect("actual pinned String leaf proof");
                assert!(matches!(
                    call.holder,
                    crate::jit::JitMethodHolder::Dictionary(_)
                ));
                assert_eq!(call.safepoint_id, crate::native_abi::NO_SAFEPOINT);
            }
        });
    }
}

#[cfg(test)]
#[path = "jit_compile_group_tests.rs"]
mod group_policy_tests;
