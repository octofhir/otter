//! JIT compile requests and cold profile-feedback baking.
//!
//! # Contents
//! - `jit_code_residency` and `jit_code_generation_snapshot` — opt-in
//!   whole-isolate executable ownership and generation snapshots.
//! - `compile_jit_function` and cold feedback baking into the instruction view
//!   (property/object-literal/global-lexical/inline-callee tables).
//! - Bounded call-graph tiering for hot observed callees that need an entry
//!   generation before the caller's stable direct-link snapshot is sealed.
//! - Call/method target profiling and reoptimization eviction.
//! - Plain and method candidate snapshots share one bounded ancestry tree.
//! - Cross-script recompilation resolves the defining code owner before baking.
//!
//! # Invariants
//! Baked pointers (shape ids, global cells, prototype slots) must only
//! reference permanent or non-moving allocations; anything movable goes
//! through a runtime stub instead.
//! Compile requests use the function's defining context, even when a later
//! script triggered invalidation or reentry.
//! Compiled code is published only after the registry accepts its metadata and
//! exact isolate-epoch dependency snapshot.
//! Eager direct-target compilation consumes an explicit depth budget, so a
//! closure-backed target can bring in one nested hot edge without recursively
//! compiling an unbounded observed call graph.
#![allow(unused_imports)]
use crate::*;

#[path = "inline_snapshot_budget.rs"]
mod inline_snapshot_budget;
use inline_snapshot_budget::InlineSnapshotBudget;

const EAGER_DIRECT_TARGET_DEPTH: u8 = 2;

/// Internal result of one Template compiler invocation.
///
/// Only [`TemplateCompileOutcome::Unsupported`] is stable enough to enter the
/// function-wide canonical cache. Allocation, backend availability, hook,
/// prewarm, and registry-admission failures are
/// [`TemplateCompileOutcome::Deferred`] so execution can retry after a bounded
/// cooling interval.
#[derive(Debug, Clone)]
pub(super) enum TemplateCompileOutcome {
    Installed(std::sync::Arc<dyn jit::JitFunctionCode>),
    Unsupported,
    Deferred,
}

impl Interpreter {
    /// Snapshot installed, invalid, and retired-tombstone JIT generations.
    ///
    /// This explicit diagnostics call walks cold registry metadata and performs
    /// no work during ordinary compilation or execution.
    #[must_use]
    pub fn jit_code_generation_snapshot(&self) -> Vec<jit::JitCodeGenerationSnapshot> {
        self.jit_code_registry.generation_snapshot()
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
    /// failures retain no code/cache verdict and only arm the entry retry
    /// countdown; each OSR header naturally waits for its next hotness
    /// threshold before trying again.
    pub(super) fn retain_template_compile_outcome(
        &mut self,
        fid: u32,
        outcome: TemplateCompileOutcome,
    ) -> TemplateCompileOutcome {
        match &outcome {
            TemplateCompileOutcome::Installed(code) => {
                self.jit_code.insert(fid, Some(code.clone()));
                self.jit_template_entry_retry_remaining.remove(&fid);
                if code.osr_only() {
                    self.jit_entry_osr_only.insert(fid);
                } else {
                    self.jit_entry_osr_only.remove(&fid);
                }
            }
            TemplateCompileOutcome::Unsupported => {
                self.jit_code.insert(fid, None);
                self.jit_template_entry_retry_remaining.remove(&fid);
                self.jit_template_osr_fids.remove(&fid);
                self.jit_entry_osr_only.remove(&fid);
                self.jit_osr_disabled.insert((fid, u32::MAX));
                self.jit_osr_counts
                    .retain(|(counted_fid, _), _| *counted_fid != fid);
            }
            TemplateCompileOutcome::Deferred => {
                self.jit_template_entry_retry_remaining
                    .insert(fid, Self::JIT_TEMPLATE_DEFERRED_RETRY_ENTRIES);
            }
        }
        self.jit_code_cache = None;
        outcome
    }

    /// Consume one ordinary entry from a transient-compile cooling interval.
    /// Returns `true` exactly when compilation may be attempted now.
    pub(super) fn template_entry_retry_ready(&mut self, fid: u32) -> bool {
        let Some(remaining) = self.jit_template_entry_retry_remaining.get_mut(&fid) else {
            return true;
        };
        if *remaining > 1 {
            *remaining -= 1;
            return false;
        }
        self.jit_template_entry_retry_remaining.remove(&fid);
        true
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
        let event = jit_debug::JitDebugEvent::CompilePrepared {
            function_id: fid,
            function_name,
            tier,
            target,
            register_count: u32::from(view.code_block.register_count),
            parameter_count: u32::from(view.code_block.param_count),
            call_feedback_sites: u32::try_from(call_feedback_sites).unwrap_or(u32::MAX),
            method_feedback_sites: u32::try_from(method_feedback_sites).unwrap_or(u32::MAX),
            global_load_sites: u32::try_from(global_load_sites).unwrap_or(u32::MAX),
            global_lexical_loads: u32::try_from(view.global_lexical_loads.len())
                .unwrap_or(u32::MAX),
            string_constant_cells: u32::try_from(view.string_constant_cells.len())
                .unwrap_or(u32::MAX),
            global_object_loads: u32::try_from(view.global_object_loads.len()).unwrap_or(u32::MAX),
            direct_callees: u32::try_from(view.direct_callees.len()).unwrap_or(u32::MAX),
            direct_constructs: u32::try_from(view.direct_constructs.len()).unwrap_or(u32::MAX),
            direct_method_sites: u32::try_from(view.direct_methods.len()).unwrap_or(u32::MAX),
            direct_method_targets: u32::try_from(
                view.direct_methods.values().map(Vec::len).sum::<usize>(),
            )
            .unwrap_or(u32::MAX),
            static_native_calls: u32::try_from(view.static_native_calls.len()).unwrap_or(u32::MAX),
            inline_callees: u32::try_from(view.inline_callees.len()).unwrap_or(u32::MAX),
            inline_methods: u32::try_from(view.inline_methods.len()).unwrap_or(u32::MAX),
        };
        self.push_reserved_jit_debug_event(event);
        self.record_settled_property_sites(fid, tier, view);
    }

    /// Report the guard chain every settled property site in `view` carries.
    ///
    /// Emitted once per baked snapshot, immediately after the prepare event, so
    /// a report reads as: this function, this tier, these sites lower without
    /// their cache cells and these are the ways each one must distinguish.
    fn record_settled_property_sites(
        &mut self,
        fid: u32,
        tier: jit_debug::JitDebugTier,
        view: &jit::JitCompileSnapshot,
    ) {
        let sites = view
            .property_loads
            .iter()
            .map(|entry| (jit_debug::JitDebugPropertyAccess::Load, entry))
            .chain(
                view.property_stores
                    .iter()
                    .map(|entry| (jit_debug::JitDebugPropertyAccess::Store, entry)),
            );
        for (access, (&byte_pc, chain)) in sites {
            if !self.reserve_jit_debug_event() {
                return;
            }
            self.push_reserved_jit_debug_event(jit_debug::JitDebugEvent::PropertySiteSettled {
                function_id: fid,
                tier,
                byte_pc,
                access,
                ways: chain
                    .iter()
                    .map(|way| jit_debug::JitDebugPropertyWay {
                        shape: way.receiver_shape,
                        value_byte: way.value_byte,
                    })
                    .collect(),
            });
        }
    }

    fn record_jit_compile_finished(
        &mut self,
        fid: u32,
        tier: jit_debug::JitDebugTier,
        target: jit_debug::JitDebugTarget,
        code_object_id: u64,
        status: &Result<jit::JitCompileStatus, jit::JitCompileError>,
    ) {
        if !self.reserve_jit_debug_event() {
            return;
        }
        let outcome = match status {
            Ok(jit::JitCompileStatus::Compiled { code, .. }) => {
                jit_debug::JitDebugCompileOutcome::Compiled {
                    code_object_id,
                    code_bytes: u64::try_from(code.code_len()).unwrap_or(u64::MAX),
                }
            }
            Ok(jit::JitCompileStatus::Unavailable) => {
                jit_debug::JitDebugCompileOutcome::Unavailable
            }
            Ok(jit::JitCompileStatus::Unsupported { reason }) => {
                jit_debug::JitDebugCompileOutcome::Unsupported {
                    reason: reason.clone(),
                }
            }
            Err(error) => jit_debug::JitDebugCompileOutcome::Error {
                message: error.message.clone(),
            },
        };
        self.push_reserved_jit_debug_event(jit_debug::JitDebugEvent::CompileFinished {
            function_id: fid,
            tier,
            target,
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
    /// Unsupported functions return `None`; hotness survives invalidation, so
    /// the normal resolver can immediately rebuild a baseline body.
    pub(crate) fn compile_optimized_jit_function(
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
        self.prewarm_string_constant_cells(context, fid)?;
        let mut snapshot = context.jit_compile_snapshot(fid)?;
        self.publish_property_feedback_for_view(&snapshot);
        // The optimizing tier consumes the same baked compile inputs as the
        // template tier: without the cage base and body offsets no inline access
        // can be emitted at all, and without monomorphic call-site candidates
        // there is nothing to inline.
        Self::bake_typed_array_layout(&mut snapshot);
        Self::bake_string_layout(&mut snapshot);
        self.bake_string_constant_cells(&mut snapshot, context, fid)?;
        self.bake_global_lexical_loads(&mut snapshot, context, fid);
        self.bake_binding_hit_proofs(&mut snapshot, context, fid);
        self.bake_inline_callees(
            &mut snapshot,
            context,
            fid,
            jit_debug::JitDebugTier::Optimizing,
            EAGER_DIRECT_TARGET_DEPTH,
        );
        self.bake_guarded_method_calls(&mut snapshot);
        self.bake_element_accesses(&mut snapshot);
        self.bake_property_loads(&mut snapshot);
        self.bake_constructor_field_transitions(&mut snapshot);
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
        let status = hook.compile_optimized_function(jit::JitCompileRequest {
            snapshot,
            debug: self.jit_debug.request(),
            artifact_identity,
            osr_pc,
            code_object_id,
        });
        self.record_jit_compile_finished(
            fid,
            jit_debug::JitDebugTier::Optimizing,
            target,
            code_object_id,
            &status,
        );
        match status {
            Ok(jit::JitCompileStatus::Compiled { code, artifact, .. }) => {
                if let Some(artifact) = artifact {
                    self.record_jit_artifact(*artifact);
                }
                self.jit_next_code_object_id += 1;
                // Generated callers do not take a per-call executable lease.
                // Any published native activation pins the current retirement
                // epoch, including across reentrant compilation/invalidation.
                if self.jit_native_activation_top == 0 {
                    self.jit_code_registry.retire_unreferenced();
                }
                let installed = self.jit_code_registry.install_compiled(
                    code_object_id,
                    code.clone(),
                    &function,
                );
                if installed {
                    self.jit_runtime_stats.code_generations =
                        self.jit_runtime_stats.code_generations.saturating_add(1);
                }
                installed.then_some(code)
            }
            _ => None,
        }
    }

    /// Resolve or compile one whole-body optimizer object for loop OSR.
    ///
    /// Back-edge hotness is independent of function-entry promotion: a
    /// single-call loop can therefore compile here before the entry policy is
    /// hot. Successful code is shared with later function entries. A failed
    /// early feedback snapshot is not cached as a permanent entry-tier miss;
    /// the current header falls back to template OSR and future entry feedback
    /// may still make the function optimizable.
    pub(crate) fn resolve_optimized_osr_code(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        osr_pc: u32,
    ) -> Option<std::sync::Arc<dyn jit::JitFunctionCode>> {
        if !self
            .jit_hook
            .as_ref()
            .is_some_and(|hook| hook.optimizing_tier_enabled())
        {
            return None;
        }
        if let Some(Some(code)) = self.jit_optimized_code.get(&fid) {
            return self
                .jit_code_registry
                .is_current_for_entry(code.as_ref())
                .then(|| code.clone());
        }
        // A declined body is retried at a back-edge only when its feedback epoch
        // has advanced since the last failed attempt: the optimizer's whole-body
        // subset check is structural (unsupported opcode) except for
        // feedback-driven representation, so re-running it while the feedback is
        // unchanged always fails again and would recompile on every hot iteration.
        let epoch = self.code_space.feedback_epoch(fid);
        if matches!(self.jit_optimized_code.get(&fid), Some(None))
            && self.jit_optimized_declined_epoch.get(&fid) == Some(&epoch)
        {
            return None;
        }
        match self.compile_optimized_jit_function(context, fid, Some(osr_pc)) {
            Some(compiled) => {
                self.jit_optimized_code.insert(fid, Some(compiled.clone()));
                self.jit_optimized_declined_epoch.remove(&fid);
                self.jit_optimized_code_cache = Some((fid, compiled.clone()));
                Some(compiled)
            }
            None => {
                self.jit_optimized_code.insert(fid, None);
                self.jit_optimized_declined_epoch.insert(fid, epoch);
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
        self.compile_jit_function_with_direct_targets(
            context,
            fid,
            osr_pc,
            EAGER_DIRECT_TARGET_DEPTH,
        )
    }

    /// Compile one template body, optionally materializing a bounded depth of
    /// hot direct-target entry generations before the caller snapshot is sealed.
    fn compile_jit_function_with_direct_targets(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        osr_pc: Option<u32>,
        eager_direct_target_depth: u8,
    ) -> TemplateCompileOutcome {
        if !self.jit_template_compiling.insert(fid) {
            return TemplateCompileOutcome::Deferred;
        }
        let outcome = self.compile_jit_function_with_direct_targets_unchecked(
            context,
            fid,
            osr_pc,
            eager_direct_target_depth,
        );
        self.jit_template_compiling.remove(&fid);
        outcome
    }

    /// Compile after the per-function in-flight guard has been acquired.
    fn compile_jit_function_with_direct_targets_unchecked(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        osr_pc: Option<u32>,
        eager_direct_target_depth: u8,
    ) -> TemplateCompileOutcome {
        let Some(hook) = self.jit_hook.as_ref().cloned() else {
            return TemplateCompileOutcome::Deferred;
        };
        let Ok(owner) = context.for_function(fid) else {
            return TemplateCompileOutcome::Deferred;
        };
        let context = &*owner;
        if self.prewarm_string_constant_cells(context, fid).is_none() {
            return TemplateCompileOutcome::Deferred;
        }
        let Some(mut view) = context.jit_compile_snapshot(fid) else {
            return TemplateCompileOutcome::Deferred;
        };
        self.publish_property_feedback_for_view(&view);
        Self::bake_typed_array_layout(&mut view);
        Self::bake_string_layout(&mut view);
        if self
            .bake_string_constant_cells(&mut view, context, fid)
            .is_none()
        {
            return TemplateCompileOutcome::Deferred;
        }
        self.bake_global_lexical_loads(&mut view, context, fid);
        self.bake_binding_hit_proofs(&mut view, context, fid);
        self.bake_inline_callees(
            &mut view,
            context,
            fid,
            jit_debug::JitDebugTier::Template,
            eager_direct_target_depth,
        );
        self.bake_guarded_method_calls(&mut view);
        self.bake_element_accesses(&mut view);
        self.bake_property_loads(&mut view);
        self.bake_constructor_field_transitions(&mut view);
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
        let status = hook.compile_function(jit::JitCompileRequest {
            snapshot: view,
            debug: self.jit_debug.request(),
            artifact_identity,
            osr_pc,
            code_object_id,
        });
        self.record_jit_compile_finished(
            fid,
            jit_debug::JitDebugTier::Template,
            target,
            code_object_id,
            &status,
        );
        match status {
            Ok(jit::JitCompileStatus::Compiled { code, artifact, .. }) => {
                if let Some(artifact) = artifact {
                    self.record_jit_artifact(*artifact);
                }
                self.jit_next_code_object_id += 1;
                // Sweep before registering: cached/installed users hold an
                // `Arc`; executing generated generations are protected by the
                // isolate's published native-activation epoch.
                if self.jit_native_activation_top == 0 {
                    self.jit_code_registry.retire_unreferenced();
                }
                let installed = self.jit_code_registry.install_compiled(
                    code_object_id,
                    code.clone(),
                    &function,
                );
                if installed {
                    self.jit_runtime_stats.code_generations =
                        self.jit_runtime_stats.code_generations.saturating_add(1);
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

    /// Describe every property load and store site whose receiver shape has
    /// settled.
    ///
    /// The site's own-data feedback is the declaration: a shape handle and a
    /// slot. Generated code then compares against that shape as an immediate
    /// and reads a fixed slab offset, so the settled site never loads its
    /// cache cell or walks the cell's ways. A receiver that stops matching
    /// misses to the same window transition, which re-patches the cell.
    pub(crate) fn bake_property_loads(&mut self, view: &mut jit::JitCompileSnapshot) {
        const SLOT_BYTES: u32 = std::mem::size_of::<crate::Value>() as u32;
        let sites: Vec<_> = view
            .instructions
            .iter()
            .filter(|instr| {
                matches!(
                    instr.op(&view.code_block),
                    Op::LoadProperty | Op::StoreProperty
                )
            })
            .map(|instr| {
                (
                    instr.byte_pc,
                    instr.op(&view.code_block),
                    instr.property_ic_site(&view.code_block),
                )
            })
            .collect();
        for (byte_pc, op, site) in sites {
            let Some(site) = site else { continue };
            let kind = if op == Op::LoadProperty {
                crate::property_ic::PropertyIcKind::Load
            } else {
                crate::property_ic::PropertyIcKind::Store
            };
            let Some(slots) = self.feedback_directory.settled_property_slots(site, kind) else {
                continue;
            };
            let slot_count = slots.len();
            // Every installed program must lower, or the chain would silently
            // drop a shape the site really sees and send it to the transition.
            let chain: Vec<_> = slots
                .into_iter()
                .filter_map(|(shape_id, shape_offset, slot)| {
                    let live = self.shape_runtime.handle_for_id(shape_id)?;
                    (live.offset() == shape_offset).then_some(jit::JitInlinePropertyLoad {
                        receiver_shape: shape_offset,
                        value_byte: u32::from(slot) * SLOT_BYTES,
                    })
                })
                .collect();
            if chain.len() != slot_count || chain.is_empty() {
                continue;
            }
            if op == Op::LoadProperty {
                view.property_loads.insert(byte_pc, chain);
            } else {
                view.property_stores.insert(byte_pc, chain);
            }
        }
        self.bake_prototype_loads(view);
    }

    /// Publish exact constructor field transitions learned during rooted
    /// receiver preparation. The byte-PC key keeps the descriptor attached to
    /// the original `StoreProperty`; generated guard failure deoptimizes before
    /// that operation and never replays a committed store.
    fn bake_constructor_field_transitions(&self, view: &mut jit::JitCompileSnapshot) {
        if let Some(transitions) = self
            .constructor_field_transition_cache
            .get(&view.code_block.id)
        {
            view.constructor_field_transitions = transitions
                .iter()
                .filter_map(|(&byte_pc, transition)| {
                    let from_shape = self
                        .shape_runtime
                        .handle_for_id(transition.from_shape)?
                        .offset();
                    let to_shape = self
                        .shape_runtime
                        .handle_for_id(transition.to_shape)?
                        .offset();
                    let prototype_shapes = transition
                        .prototype_shapes
                        .iter()
                        .map(|&shape| {
                            self.shape_runtime
                                .handle_for_id(shape)
                                .map(|shape| shape.offset())
                        })
                        .collect::<Option<Vec<_>>>()?;
                    Some((
                        byte_pc,
                        jit::JitConstructorFieldTransition {
                            from_shape,
                            to_shape,
                            prototype_shapes,
                            slot: transition.slot,
                        },
                    ))
                })
                .collect();
        }
    }

    /// Describe every load site whose programs all reach their slot through the
    /// receiver's prototype.
    ///
    /// The receiver's shape decides which prototype the hop lands on and the
    /// holder's shape decides where the slot is, so both are compile-time
    /// constants and the site needs no cache cell. A shape that has since moved
    /// on drops its way, and a site that loses every way keeps its cell.
    fn bake_prototype_loads(&mut self, view: &mut jit::JitCompileSnapshot) {
        const SLOT_BYTES: u32 = std::mem::size_of::<crate::Value>() as u32;
        let sites: Vec<_> = view
            .instructions
            .iter()
            .filter(|instr| instr.op(&view.code_block) == Op::LoadProperty)
            .filter(|instr| !instr.load_array_length)
            .map(|instr| (instr.byte_pc, instr.property_ic_site(&view.code_block)))
            .collect();
        for (byte_pc, site) in sites {
            let Some(site) = site else { continue };
            if view.property_loads.contains_key(&byte_pc) {
                continue;
            }
            let Some(slots) = self.feedback_directory.settled_prototype_slots(site) else {
                continue;
            };
            let chain: Vec<_> = slots
                .into_iter()
                .filter_map(|(receiver_id, holder_id, holder_offset, slot)| {
                    let receiver = self.shape_runtime.handle_for_id(receiver_id)?;
                    let holder = self.shape_runtime.handle_for_id(holder_id)?;
                    (holder.offset() == holder_offset && receiver.offset() != 0).then_some(
                        jit::JitInlinePropertyHop {
                            receiver_shape: receiver.offset(),
                            holder_shape: holder_offset,
                            value_byte: u32::from(slot) * SLOT_BYTES,
                        },
                    )
                })
                .collect();
            if !chain.is_empty() {
                view.property_prototype_loads.insert(byte_pc, chain);
            }
        }
    }

    /// Describe how each `LoadElement` / `StoreElement` site addresses its
    /// receiver's elements.
    ///
    /// The family comes from what the site observed, so a typed view and a
    /// dense array are the same program over different declared offsets. A
    /// store additionally guards that the value already has the view's
    /// representation, because coercing it could run user code.
    pub(crate) fn bake_element_accesses(&mut self, view: &mut jit::JitCompileSnapshot) {
        let sites: Vec<_> = view
            .instructions
            .iter()
            .filter_map(|instr| {
                let op = instr.op(&view.code_block);
                matches!(op, Op::LoadElement | Op::StoreElement)
                    .then_some((instr.byte_pc, instr.instruction_pc(&view.code_block)))
            })
            .collect();
        for (byte_pc, pc) in sites {
            let family = view
                .code_block
                .feedback_at(pc as usize)
                .map_or(jit::JitElementFamily::Unseen, |cell| cell.element_family());
            let access = match family {
                jit::JitElementFamily::TypedInt32 => Self::typed_element_access(
                    crate::binary::TypedArrayKind::Int32,
                    jit::JitElementRepr::Int32,
                ),
                jit::JitElementFamily::TypedFloat64 => Self::typed_element_access(
                    crate::binary::TypedArrayKind::Float64,
                    jit::JitElementRepr::Float64,
                ),
                jit::JitElementFamily::DenseTagged => Self::dense_element_access(
                    jit::JitElementRepr::Boxed,
                    crate::array::DENSE_ELEMENT_KIND_TAGGED,
                ),
                jit::JitElementFamily::DenseFloat64 => Self::dense_element_access(
                    jit::JitElementRepr::Float64,
                    crate::array::DENSE_ELEMENT_KIND_PACKED_DOUBLE,
                ),
                jit::JitElementFamily::Unseen | jit::JitElementFamily::Generic => continue,
            };
            view.element_accesses.insert(byte_pc, access);
        }
    }

    /// Describe one exact ordinary-array dense representation behind the
    /// body's element cache. Both the exotic sidecar and the physical element
    /// kind are guarded: a custom prototype/descriptor or a monotonic
    /// numeric-to-tagged transition must leave generated code before access.
    fn dense_element_access(
        element: jit::JitElementRepr,
        dense_kind: u32,
    ) -> jit::JitElementAccess {
        if element == jit::JitElementRepr::Float64 {
            return jit::JitElementAccess::packed_double_array();
        }
        let header = otter_gc::header::HEADER_SIZE as u32;
        jit::JitElementAccess {
            type_tag: crate::array::ARRAY_BODY_TYPE_TAG,
            guards: [
                // The sidecar is a 4-byte GC handle: read exactly it.
                Some(jit::JitBodyGuard::clear(
                    header + std::mem::offset_of!(crate::array::ArrayBody, exotic) as u32,
                    jit::JitGuardWidth::Word32,
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
            },
            element,
        }
    }

    /// Bake the static heap-layout offsets for inline primitive string fast
    /// paths. String bodies are GC cells addressed through the same cage base as
    /// object/array bodies, so this only enables when the compile snapshot has a
    /// cage base.
    pub(crate) fn bake_string_layout(view: &mut jit::JitCompileSnapshot) {
        let header = otter_gc::header::HEADER_SIZE as u32;
        let repr = header + std::mem::offset_of!(crate::string::JsStringBody, repr) as u32;
        view.string_layout = jit::JitStringLayout {
            string_type_tag: crate::string::JS_STRING_BODY_TYPE_TAG,
            string_len_byte: header + std::mem::offset_of!(crate::string::JsStringBody, len) as u32,
            string_repr_byte: repr,
            string_repr_payload_byte: repr
                + crate::string::gc_body::STRING_REPR_PAYLOAD_BYTE as u32,
            string_body_size: header + std::mem::size_of::<crate::string::JsStringBody>() as u32,
            inline_flat_tag: crate::string::gc_body::STRING_REPR_INLINE_FLAT,
            seq_flat_tag: crate::string::gc_body::STRING_REPR_SEQ_FLAT,
            inline_latin1_tag: crate::string::gc_body::STRING_REPR_INLINE_LATIN1,
            seq_latin1_tag: crate::string::gc_body::STRING_REPR_SEQ_LATIN1,
        };
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
        site: usize,
    ) -> Option<(u32, u32)> {
        const SLOT_BYTES: u32 = std::mem::size_of::<crate::Value>() as u32;
        let op = instruction.op(code_block);
        let kind = match op {
            Op::LoadProperty => crate::property_ic::PropertyIcKind::Load,
            Op::StoreProperty => crate::property_ic::PropertyIcKind::Store,
            _ => return None,
        };
        self.publish_property_feedback(site, kind);
        match self.property_feedback_state(site, kind)? {
            crate::feedback::PropertyFeedbackState::MonomorphicOwnData { shape_id, slot } => {
                let shape = self.shape_runtime.handle_for_id(shape_id)?;
                Some((shape.offset(), u32::from(slot) * SLOT_BYTES))
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
    ) -> Option<(u32, u32)> {
        const SLOT_BYTES: u32 = std::mem::size_of::<crate::Value>() as u32;
        let name_operand = match instruction.op(code_block) {
            Op::LoadProperty => 2,
            Op::StoreProperty => 1,
            _ => return None,
        };
        let class_fid = code_block.class_hint(instruction.instruction_pc(code_block) as usize)?;
        let shape = *self.simple_constructor_shape_cache.get(&class_fid)?;
        let otter_bytecode::Operand::ConstIndex(name_idx) =
            instruction.operand(code_block, name_operand)?
        else {
            return None;
        };
        let key = context.property_atom(name_idx)?;
        let slot = crate::object::shape_offset_of_str(&self.gc_heap, shape, key.name())?;
        Some((shape.offset(), slot * SLOT_BYTES))
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
        (argc == usize::from(declaration.argument_count)).then_some(declaration.leaf_stub_id)
    }

    /// Bake every `Op::CallMethodValue` site whose callee is a declared native
    /// entry into the compile snapshot.
    ///
    /// The receiver layout comes from the same feedback a property site
    /// records, so generated code guards it with the shared way walk and
    /// prototype hop rather than a second description of the same access. An
    /// entry that may collect reserves its safepoint here, at the site that
    /// will publish it.
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
                method_value_byte,
                recv_shape_offset,
                holder_shape_offset,
                ..
            }) = self.method_target_feedback(site)
            else {
                continue;
            };
            // An empty shape token never matches a live receiver, so a site
            // that cannot name its guard stays on the runtime path.
            if recv_shape_offset == 0 {
                continue;
            }
            let Some(declaration) = crate::jit_static_native::jit_leaf_builtin(stub_id) else {
                continue;
            };
            // A builtin this isolate never installed has no external-ref
            // index, so no live receiver could carry its identity.
            let Some(builtin_native_ref) =
                crate::jit_static_native::jit_static_call_ref(stub_id, &self.gc_heap)
            else {
                continue;
            };
            view.guarded_method_calls.insert(
                byte_pc,
                jit::JitGuardedMethodCall {
                    receiver: jit::JitGuardedReceiver::Shape {
                        shape: recv_shape_offset,
                    },
                    holder_shape: holder_shape_offset,
                    method_value_byte,
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
        let recv_shape_handle = crate::object::shape(recv, &self.gc_heap);
        if recv_shape_handle.is_null() {
            return None;
        }
        let recv_shape = crate::object::shape_id(recv, &self.gc_heap);
        let slot_byte = |slot: u32| slot * std::mem::size_of::<crate::Value>() as u32;
        if let Some(slot) = self.shape_offset_of(recv_shape_handle, name.name()) {
            return Some(MethodSite {
                recv_shape,
                proto_chain: crate::MethodProtoChain::own(),
                method_value_byte: slot_byte(slot),
                recv_shape_offset: recv_shape_handle.offset(),
                holder_shape_offset: 0,
            });
        }
        // Walk the prototype chain, recording each hopped object's shape; the
        // baked guard checks exactly this chain (flat-prototype chase + shape
        // compare per hop) before trusting the holder's slot offset.
        let mut proto_chain = crate::MethodProtoChain::own();
        let mut cur = recv;
        loop {
            cur = crate::object::prototype(cur, &self.gc_heap)?;
            let shape = crate::object::shape(cur, &self.gc_heap);
            if shape.is_null() || !proto_chain.push(crate::object::shape_id(cur, &self.gc_heap)) {
                return None;
            }
            if let Some(slot) = self.shape_offset_of(shape, name.name()) {
                return Some(MethodSite {
                    recv_shape,
                    proto_chain,
                    method_value_byte: slot_byte(slot),
                    recv_shape_offset: recv_shape_handle.offset(),
                    holder_shape_offset: shape.offset(),
                });
            }
        }
    }

    /// Drop any compiled body for `fid` (and re-arm its OSR headers) so the next
    /// tier-up recompiles it. Called when call/method feedback for one of its
    /// sites first matures: a function whose hot loop calls out is often compiled
    /// by an *earlier* loop in the same body, before the callee feedback exists,
    /// so its inline sites baked nothing. Recompiling once the feedback is warm
    /// lets those sites inline. The currently-running body, if any, stays alive
    /// through its `Arc` until the frame returns.
    pub(crate) fn evict_compiled_for_reopt(&mut self, fid: u32) {
        self.invalidate_jit_function(fid);
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

    /// Ensure one hot feedback target has an entry-capable baseline generation.
    ///
    /// The caller compile may run before a loop-heavy callee reaches the
    /// function-entry threshold: that callee can already own optimizing OSR
    /// code while every call boundary still returns to Rust. Compile at most
    /// one target while the caller's explicit depth budget remains, then seal
    /// the caller against its stable function cell. The decremented budget
    /// bounds recursive planning even for cyclic call graphs while allowing a
    /// hot closure-backed caller to bring its already-observed nested target
    /// into the same sealed generation.
    fn ensure_direct_callee_plan(
        &mut self,
        context: &ExecutionContext,
        function: &CodeBlock,
        eager_depth: u8,
    ) -> Option<jit::JitDirectCallPlan> {
        if let Some(plan) = self.current_direct_callee_plan(function) {
            return Some(plan);
        }
        if eager_depth == 0 || self.jit_code.contains_key(&function.id) {
            return None;
        }
        self.jit_runtime_stats.compile_attempts =
            self.jit_runtime_stats.compile_attempts.saturating_add(1);
        let outcome = self.compile_jit_function_with_direct_targets(
            context,
            function.id,
            None,
            eager_depth - 1,
        );
        self.retain_template_compile_outcome(function.id, outcome);
        self.current_direct_callee_plan(function)
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
    /// and global-object shape, so later eval/script lexicals and structural
    /// mutations miss before reading the baked slot.
    fn bake_global_lexical_loads(
        &self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        fid: u32,
    ) {
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
            let shape = crate::object::shape(self.global_this, &self.gc_heap);
            let (shape, dictionary) = if shape.is_null() {
                (hit.shape_id.raw(), true)
            } else {
                (u64::from(shape.offset()), false)
            };
            const SLOT_BYTES: u32 = std::mem::size_of::<crate::Value>() as u32;
            view.global_object_loads.insert(
                instruction.byte_pc,
                jit::JitGlobalObjectLoad {
                    shape,
                    dictionary,
                    value_byte: u32::from(hit.slot) * SLOT_BYTES,
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
    /// and global-object shape, so later eval/script lexicals and structural
    /// mutations miss before reading the baked slot.
    fn bake_binding_hit_proofs(
        &self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        fid: u32,
    ) {
        for instruction in &view.instructions {
            let op = instruction.op(&view.code_block);
            let Some(binding) = otter_bytecode::opcode_schema::opcode_schema(op).binding else {
                continue;
            };
            let name_operand = match binding {
                otter_bytecode::opcode_schema::BindingSemantics::Read(
                    otter_bytecode::opcode_schema::BindingRead::Global { name, .. }
                    | otter_bytecode::opcode_schema::BindingRead::Exists { name, .. },
                ) => name,
                otter_bytecode::opcode_schema::BindingSemantics::Write(
                    otter_bytecode::opcode_schema::BindingWrite::Global { name, .. }
                    | otter_bytecode::opcode_schema::BindingWrite::GlobalChecked { name, .. },
                ) => name,
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
            let shape = crate::object::shape(self.global_this, &self.gc_heap);
            let (shape, dictionary) = if shape.is_null() {
                (hit.shape_id.raw(), true)
            } else {
                (u64::from(shape.offset()), false)
            };
            const SLOT_BYTES: u32 = std::mem::size_of::<crate::Value>() as u32;
            view.binding_hit_proofs.insert(
                instruction.byte_pc,
                jit::BindingHitProof::GlobalObject {
                    shape,
                    dictionary,
                    value_byte: u32::from(hit.slot) * SLOT_BYTES,
                    global_lexical_epoch: self.global_lexical_epoch,
                    writable: flags.writable(),
                },
            );
        }
    }

    /// Canonicalize every string literal needed by `fid` before snapshotting.
    ///
    /// Existing canonical cells and functions without string literals are
    /// allocation-free and need no active frame-root provider. The first cold
    /// literal may allocate and therefore requires the current activation stack
    /// to be registered with the collector. Allocation failure only declines
    /// this optional compile; it does not publish a pending JavaScript throw or
    /// retain an error for later execution.
    fn prewarm_string_constant_cells(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
    ) -> Option<()> {
        let owner = context.for_function(fid).ok()?;
        let function = owner.exec_function(fid)?;
        let mut instruction_index = 0usize;
        while let Some(instruction) = function.instr_at_index(instruction_index) {
            instruction_index = instruction_index.checked_add(1)?;
            if function.op(instruction) != Op::LoadString {
                continue;
            }
            let constant = function.const_index(instruction, 1)?;
            let key = owner.constant_cache_key(constant);
            if let Some(cell) = self.string_constant_cells.get(&key) {
                if !cell.is_string() {
                    return None;
                }
                continue;
            }
            if !self.gc_heap.has_frame_root_providers() {
                return None;
            }
            let value = self.load_string_constant_value(&owner, constant).ok()?;
            debug_assert!(value.is_string(), "prewarmed literal must be a string");
        }
        Some(())
    }

    /// Publish direct reads of already-canonical primitive-string literals.
    ///
    /// The isolate owns boxed `Value` cells: hash-table growth may move a box
    /// but never its allocation, and root tracing rewrites the cell after a
    /// moving collection. Every published site is therefore a leaf relocation
    /// load; cold materialization was completed before the snapshot existed.
    fn bake_string_constant_cells(
        &self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        fid: u32,
    ) -> Option<()> {
        let owner = context.for_function(fid).ok()?;
        for instruction in &view.instructions {
            if instruction.op(&view.code_block) != Op::LoadString {
                continue;
            }
            let constant = instruction.const_index(&view.code_block, 1)?;
            let key = owner.constant_cache_key(constant);
            let cell = self.string_constant_cells.get(&key)?;
            if !cell.is_string() {
                return None;
            }
            view.string_constant_cells.insert(
                instruction.byte_pc,
                jit::JitStringConstantCell {
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
    /// would as an outermost function: a spliced instruction that read the
    /// caller's tables would find nothing to fold against and could only lower
    /// as an exit. Nested candidates share the root preparation budget and use
    /// this body's own feedback; the compiler decides which bodies to splice.
    fn bake_inline_body(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        tier: jit_debug::JitDebugTier,
        budget: &mut InlineSnapshotBudget,
    ) -> Option<std::sync::Arc<jit::JitCompileSnapshot>> {
        if !budget.enter(fid) {
            return None;
        }
        let result = (|| {
            self.prewarm_string_constant_cells(context, fid)?;
            let mut body = context.jit_compile_snapshot(fid)?;
            self.publish_property_feedback_for_view(&body);
            Self::bake_typed_array_layout(&mut body);
            Self::bake_string_layout(&mut body);
            self.bake_string_constant_cells(&mut body, context, fid)?;
            self.bake_global_lexical_loads(&mut body, context, fid);
            self.bake_call_site_plans(&mut body, context, fid, tier, 0, budget);
            self.bake_guarded_method_calls(&mut body);
            self.bake_element_accesses(&mut body);
            self.bake_property_loads(&mut body);
            self.bake_optimized_exit_profile(&mut body, fid);
            Some(std::sync::Arc::new(body))
        })();
        budget.leave();
        result
    }

    /// Bake the logical PCs at which earlier optimized generations of `fid`
    /// exited, and widen the baked feedback of those instructions.
    ///
    /// The exit profile is evidence the operand observations cannot carry: an
    /// int32 result that overflowed, or a value a guard refused. Folding it
    /// into the per-instruction feedback means a rebuilt generation — or a
    /// caller splicing this body inline — does not emit the speculation that
    /// already exited.
    fn bake_optimized_exit_profile(&self, snapshot: &mut jit::JitCompileSnapshot, fid: u32) {
        snapshot.optimized_bail_pcs = self
            .jit_optimized_bail_pcs
            .get(&fid)
            .cloned()
            .unwrap_or_default();
        for pc in snapshot.optimized_bail_pcs.clone() {
            if let Some(instruction) = snapshot.instructions.get_mut(pc as usize) {
                instruction.note_optimized_exit();
            }
        }
    }

    /// Bake compiler-native direct-call plans and inline-candidate bodies for
    /// `fid`'s call sites.
    ///
    /// Fixed/spread ordinary-call candidates remain monomorphic. Forwarded
    /// arguments admit the bounded ordinary target population; method calls may
    /// contain a bounded, most-frequent-first polymorphic chain. Every generated
    /// target is a synchronous bytecode function with an exact bounded upvalue
    /// spine and one current non-OSR installed entry. Generated linkage binds
    /// both strict/lexical and unbound sloppy-global `this`; an explicitly bound
    /// sloppy closure misses before entry. The emitter applies the final
    /// pure-leaf / size / arity test to the separate monomorphic inline tables.
    pub(crate) fn bake_inline_callees(
        &mut self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        fid: u32,
        tier: jit_debug::JitDebugTier,
        eager_direct_target_depth: u8,
    ) {
        self.bake_call_site_plans(
            view, context, fid, tier, eager_direct_target_depth,
            &mut InlineSnapshotBudget::new(fid),
        );
    }

    /// Call-site plan baking shared by an outermost body and a spliced one.
    ///
    /// Every body owns its nested candidate tables. One ancestry/depth/work
    /// budget bounds the whole tree independently of generated-entry tiering.
    fn bake_call_site_plans(
        &mut self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        fid: u32,
        tier: jit_debug::JitDebugTier,
        eager_direct_target_depth: u8,
        budget: &mut InlineSnapshotBudget,
    ) {
        let mut pending_direct_targets = rustc_hash::FxHashSet::default();
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
                let callee_fid = match target.target {
                    feedback::OrdinaryCallTarget::Bytecode(callee_fid) => callee_fid,
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
                        view.static_native_calls.insert(
                            call_byte_pc,
                            jit::JitStaticNativeCall {
                                builtin_native_ref,
                                leaf_stub_id: stub_id,
                                argument_count: declaration.argument_count,
                            },
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
                let Some(callee) = context.exec_function(callee_fid) else {
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
                let inline_ineligible = direct_ineligible || callee.needs_arguments;
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
                let direct_call_outcome = if let Some(plan) =
                    self.ensure_direct_callee_plan(context, callee, eager_direct_target_depth)
                {
                    debug_assert_eq!(plan.function_id, callee_fid);
                    let receiver_allocation = if is_construct && !callee.is_derived_constructor {
                        let new_target_function_id = match op {
                            Op::New | Op::NewSpread => callee_fid,
                            Op::SuperConstruct | Op::SuperConstructSpread => fid,
                            _ => callee_fid,
                        };
                        self.bake_receiver_allocation_plan(callee_fid, new_target_function_id)
                    } else {
                        None
                    };
                    let callee = jit::JitDirectCallee {
                        plan,
                        receiver_allocation,
                    };
                    if is_construct {
                        view.direct_constructs.insert(call_byte_pc, callee);
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
                            native_abi::NativeFrameKind::Interpreter => {
                                unreachable!("interpreter has no entry-capable code generation")
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
                    pending_direct_targets.insert(callee_fid);
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
                if is_construct || op == Op::CallSpread {
                    continue;
                }
                if inline_ineligible || target_count != 1 {
                    continue;
                }
                let Some(body) = self.bake_inline_body(context, callee_fid, tier, budget) else {
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
                            proto_chain,
                            method_value_byte,
                        } => {
                            let mut targets: SmallVec<[PolyMethodTarget; MAX_POLY_METHOD_TARGETS]> =
                                SmallVec::new();
                            targets.push(PolyMethodTarget {
                                method_fid,
                                recv_shape,
                                proto_chain,
                                method_value_byte,
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
                    eager_direct_target_depth,
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
                                    native_abi::NativeFrameKind::Interpreter => {
                                        unreachable!(
                                            "interpreter has no entry-capable code generation"
                                        )
                                    }
                                },
                                this_mode: jit::JitDirectCallThisMode::MethodReceiver,
                            },
                        )
                    }
                    Err(reason) => {
                        if matches!(
                            reason,
                            jit_debug::JitDirectCallRejectionReason::NoEntryGeneration
                        ) {
                            pending_direct_targets.insert(target.method_fid);
                        }
                        (
                            target.method_fid,
                            jit_debug::JitDirectCallPlanOutcome::Rejected { reason },
                        )
                    }
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
                if let Some(method) = self.bake_one_inline_method(context, target, tier, budget) {
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
        if tier == jit_debug::JitDebugTier::Template
            && !self.jit_feedback_refresh_attempted.contains(&fid)
        {
            if pending_direct_targets.is_empty() {
                self.jit_pending_direct_targets.remove(&fid);
            } else {
                self.jit_pending_direct_targets
                    .insert(fid, pending_direct_targets);
            }
        }
    }

    /// Materialize one exact receiver/prototype/method-slot identity guard.
    ///
    /// Feedback keeps stable shape ids; generated code consumes compressed
    /// shape-handle offsets. Resolving every hop here keeps heap/runtime layout
    /// knowledge on the VM side.
    fn bake_method_guard(&self, target: &PolyMethodTarget) -> Option<jit::JitMethodGuard> {
        let recv_shape = self.shape_runtime.handle_for_id(target.recv_shape)?;
        let proto_chain = target
            .proto_chain
            .as_slice()
            .iter()
            .map(|shape_id| {
                self.shape_runtime
                    .handle_for_id(*shape_id)
                    .map(|shape| shape.offset())
            })
            .collect::<Option<Vec<_>>>()?;
        Some(jit::JitMethodGuard {
            method_fid: target.method_fid,
            recv_shape: recv_shape.offset(),
            proto_chain,
            method_value_byte: target.method_value_byte,
        })
    }

    /// Bake one compiler-generated method call independently of leaf inlining.
    ///
    /// Each bounded mono/poly feedback target reaches this helper independently.
    /// The target must be an ordinary synchronous function with exact fresh and
    /// inherited upvalue counts and one entry-capable native generation.
    /// Recursive entry resolves through the same stable generation cell as
    /// every other generated call. Inherited closure captures are consumed
    /// directly.
    fn bake_one_direct_method(
        &mut self,
        context: &ExecutionContext,
        target: &PolyMethodTarget,
        target_index: u32,
        target_count: u32,
        eager_direct_target_depth: u8,
    ) -> Result<jit::JitDirectMethod, jit_debug::JitDirectCallRejectionReason> {
        let method = context
            .exec_function(target.method_fid)
            .ok_or(jit_debug::JitDirectCallRejectionReason::MissingCallee)?;
        if !method.admits_generated_call(jit::JitDirectCallKind::Method) {
            return Err(jit_debug::JitDirectCallRejectionReason::IneligibleFunction);
        }
        let guard = self
            .bake_method_guard(target)
            .ok_or(jit_debug::JitDirectCallRejectionReason::MethodGuardUnavailable)?;
        let plan = self
            .ensure_direct_callee_plan(context, method, eager_direct_target_depth)
            .ok_or(jit_debug::JitDirectCallRejectionReason::NoEntryGeneration)?;
        debug_assert_eq!(plan.function_id, target.method_fid);
        Ok(jit::JitDirectMethod {
            target_index,
            target_count,
            guard,
            callee: jit::JitDirectCallee {
                plan,
                receiver_allocation: None,
            },
        })
    }

    /// Bake the allocation program for one construct edge. Class wrappers use
    /// the template-wide capacity proof; ordinary closures additionally guard
    /// their current weak sample and per-instance capacity before allocation.
    fn bake_receiver_allocation_plan(
        &self,
        base_function_id: u32,
        new_target_function_id: u32,
    ) -> Option<jit::JitReceiverAllocationPlan> {
        let key = (base_function_id, new_target_function_id);
        // A receiver whose learned instance size outgrows the inline words
        // needs the runtime preparation's reserved slab; an inline allocation
        // would hand the body storage it immediately has to grow.
        let class_allocation =
            !self
                .constructor_instance_profiles
                .get(&key)
                .is_some_and(|profile| {
                    profile.learned_field_count(&self.gc_heap) > crate::object::INLINE_SLOT_CAP
                });
        let capacity = self
            .constructor_field_capacity_cache
            .get(&key)
            .copied()
            .unwrap_or(0);
        if capacity == 0 {
            return Some(jit::JitReceiverAllocationPlan {
                new_target_function_id,
                class_allocation,
                receiver_shape: self.shape_root().offset(),
                initial_field_count: 0,
                reserved_capacity: 0,
                prototype_shape_count: 0,
                prototype_shapes: [0; jit::JIT_RECEIVER_PROTOTYPE_GUARD_CAP],
            });
        };
        if capacity > crate::object::INLINE_SLOT_CAP {
            return None;
        }
        let prototype_ids = self.constructor_prototype_shape_cache.get(&key)?;
        if prototype_ids.is_empty() || prototype_ids.len() > jit::JIT_RECEIVER_PROTOTYPE_GUARD_CAP {
            return None;
        }
        let mut prototype_shapes = [0; jit::JIT_RECEIVER_PROTOTYPE_GUARD_CAP];
        for (destination, shape_id) in prototype_shapes.iter_mut().zip(prototype_ids) {
            *destination = self.shape_runtime.handle_for_id(*shape_id)?.offset();
        }
        let (receiver_shape, initial_field_count) = match (
            self.simple_constructor_init_cache
                .get(&base_function_id)
                .and_then(Option::as_ref),
            self.simple_constructor_shape_cache.get(&base_function_id),
        ) {
            (Some(init), Some(shape)) if init.fields.len() <= capacity => {
                (shape.offset(), u8::try_from(init.fields.len()).ok()?)
            }
            _ => (self.shape_root().offset(), 0),
        };
        Some(jit::JitReceiverAllocationPlan {
            new_target_function_id,
            class_allocation,
            receiver_shape,
            initial_field_count,
            reserved_capacity: u8::try_from(capacity).ok()?,
            prototype_shape_count: u8::try_from(prototype_ids.len()).ok()?,
            prototype_shapes,
        })
    }

    /// Bake one inline-method candidate body for a `(method, receiver shape)`
    /// target, resolving its sealed property loads/stores to value-slab byte
    /// offsets against the receiver shape. Returns `None` when the method shape
    /// is ineligible (generator/async/derived-constructor/etc.), its view is
    /// missing, or any body property fails to resolve to a sealed receiver slot.
    /// Shared by the monomorphic and polymorphic method-inline bake paths.
    fn bake_one_inline_method(
        &mut self,
        context: &ExecutionContext,
        target: &PolyMethodTarget,
        tier: jit_debug::JitDebugTier,
        budget: &mut InlineSnapshotBudget,
    ) -> Option<jit::JitInlineMethod> {
        let method = context.exec_function(target.method_fid)?;
        if method.is_generator
            || method.is_async
            || method.is_async_generator
            || method.needs_arguments
            || method.has_rest
            || method.contains_direct_eval
            || method.is_derived_constructor
            || method.makes_function
        {
            return None;
        }
        let method_view = self.bake_inline_body(context, target.method_fid, tier, budget)?;
        // Resolve every body `LoadProperty`/`StoreProperty` to a sealed value
        // byte offset; bail out if any property is absent, an accessor, or spills
        // past the inline value capacity. A receiver property resolves against
        // the identity-guarded receiver shape (no per-op guard); a non-receiver
        // property falls back to its own monomorphic site feedback and records
        // the shape the inliner must guard. Loads carry the name at operand 2,
        // stores at operand 1.
        let mut prop_offsets: rustc_hash::FxHashMap<u32, u32> = rustc_hash::FxHashMap::default();
        let mut prop_shapes: rustc_hash::FxHashMap<u32, u32> = rustc_hash::FxHashMap::default();
        const SLOT_BYTES: u32 = std::mem::size_of::<crate::Value>() as u32;
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
                prop_offsets.insert(instr.byte_pc, slot * SLOT_BYTES);
                continue;
            }
            // Not a receiver property: use the op's own monomorphic own-data site
            // feedback (shape offset, slot byte). Anything else — polymorphic,
            // prototype, accessor, or unobserved — is not inlinable.
            let site = instr.property_ic_site(&method_view.code_block)?;
            let (shape_off, value_byte) = self.monomorphic_own_property_feedback(
                context,
                &method_view.code_block,
                instr,
                site,
            )?;
            prop_offsets.insert(instr.byte_pc, value_byte);
            prop_shapes.insert(instr.byte_pc, shape_off);
        }
        let guard = self.bake_method_guard(target)?;
        Some(jit::JitInlineMethod {
            body: method_view,
            guard,
            prop_offsets,
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
        native_abi::SafepointRecord::frame_slot_window(
            call.safepoint_id,
            native_abi::NO_FRAME_STATE,
            view.code_block.register_count,
        ),
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
    fn fresh_interpreter_has_no_executable_code_residency() {
        let interpreter = Interpreter::new();
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
        let mut interpreter = Interpreter::new();
        let context = interpreter
            .link_module(module)
            .expect("valid bytecode fixture");
        assert!(
            interpreter
                .prewarm_string_constant_cells(&context, 0)
                .is_none(),
            "a cold allocation without published frame roots must decline"
        );
        assert_eq!(interpreter.string_constant_cells.len(), 0);

        let mut stack = ActivationStack::new();
        interpreter.with_runtime_turn(&mut stack, |turn| {
            let (interpreter, _) = turn.into_parts();
            interpreter
                .prewarm_string_constant_cells(&context, 0)
                .expect("rooted prewarm");
        });
        assert!(
            interpreter
                .prewarm_string_constant_cells(&context, 0)
                .is_some(),
            "an already-prepared function needs no active root provider"
        );

        let mut snapshot = context.jit_compile_snapshot(0).expect("snapshot");
        interpreter
            .bake_string_constant_cells(&mut snapshot, &context, 0)
            .expect("prepared bake");
        assert_eq!(snapshot.string_constant_cells.len(), 2);
        let cells = snapshot
            .string_constant_cells
            .values()
            .copied()
            .collect::<Vec<_>>();
        let first = cells[0];
        let second = cells[1];
        assert_eq!(first.cell_addr, second.cell_addr);
        assert!(unsafe { *(first.cell_addr as *const Value) }.is_string());

        interpreter.force_gc().expect("move the prepared literal");
        assert!(unsafe { *(first.cell_addr as *const Value) }.is_string());
        assert!(
            snapshot
                .string_constant_cells
                .values()
                .all(|cell| cell.cell_addr == first.cell_addr)
        );

        // No production path inserts a cell before successful allocation. If
        // corrupt state nevertheless exposes a hole, preparation and baking
        // both decline instead of publishing that state to generated code.
        **interpreter
            .string_constant_cells
            .get_mut(&context.constant_cache_key(0))
            .expect("prepared cell") = Value::hole();
        assert!(
            interpreter
                .prewarm_string_constant_cells(&context, 0)
                .is_none()
        );
        let mut invalid = context
            .jit_compile_snapshot(0)
            .expect("invalid snapshot input");
        assert!(
            interpreter
                .bake_string_constant_cells(&mut invalid, &context, 0)
                .is_none()
        );
        assert!(invalid.string_constant_cells.is_empty());
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
        let mut interpreter = Interpreter::new();
        let context = interpreter
            .link_module(module)
            .expect("valid bytecode fixture");
        let mut stack = ActivationStack::new();
        let (caller, nested) = interpreter.with_runtime_turn(&mut stack, |turn| {
            let (interpreter, _) = turn.into_parts();
            interpreter
                .prewarm_string_constant_cells(&context, 0)
                .expect("caller prewarm");
            let mut caller = context.jit_compile_snapshot(0).expect("caller snapshot");
            interpreter
                .bake_string_constant_cells(&mut caller, &context, 0)
                .expect("caller bake");
            let caller_cell = caller.string_constant_cells[&0].cell_addr;

            // Direct-target baking owns this ordering: the caller snapshot is
            // already live when preparing a nested body may move the heap.
            // Snapshot state is GC-stable and its literal cell is traced and
            // rewritten in place.
            interpreter.collect_minor_tracing_runtime_roots();
            assert!(unsafe { *(caller_cell as *const Value) }.is_string());

            interpreter
                .prewarm_string_constant_cells(&context, 1)
                .expect("nested target prewarm after moving GC");
            let mut nested = context.jit_compile_snapshot(1).expect("nested snapshot");
            interpreter
                .bake_string_constant_cells(&mut nested, &context, 1)
                .expect("nested target bake");
            assert_eq!(caller.string_constant_cells[&0].cell_addr, caller_cell);
            assert!(unsafe { *(caller_cell as *const Value) }.is_string());
            (caller, nested)
        });

        assert_eq!(caller.string_constant_cells.len(), 1);
        assert_eq!(nested.string_constant_cells.len(), 1);
        let result = interpreter
            .run(&context)
            .expect("caller executes after snapshot/GC/nested prewarm");
        assert!(result.is_string());
    }
}
