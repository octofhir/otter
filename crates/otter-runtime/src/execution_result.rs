//! Owned results, diagnostic envelopes and per-run execution counters.
//!
//! # Contents
//! ExecutionResult, ExecutionAttempt and RuntimeExecutionStats.
//!
//! # Invariants
//! Execution results carry owned data across isolate boundaries.
//!
//! # See also
//! - [`crate::Runtime`] owns the runtime session.

use crate::*;

/// Successful run output.
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    /// Completion value rendered at the isolate boundary.
    ///
    /// Public async handles must not expose internal VM values or GC
    /// handles. The local [`Runtime`] therefore renders the completion
    /// before sending it through [`RuntimeHandle`].
    completion: String,
    /// Process-style exit status requested by JS-visible runtime APIs.
    exit_code: u8,
    /// True when the run ended through an explicit `process.exit` /
    /// `reallyExit` unwind rather than by draining to idle. `beforeExit`
    /// must not fire for such runs.
    explicit_exit: bool,
    /// Wall-clock duration.
    pub duration: Duration,
    /// Diagnostic counter snapshot sampled after the run completed.
    stats: Box<RuntimeExecutionStats>,
    /// Owned, opt-in JIT diagnostics for this top-level run.
    jit_debug_report: Option<Box<JitDebugReport>>,
    /// Owned, opt-in successful compile artifacts for this top-level run.
    jit_artifacts: Option<Box<JitArtifactBatch>>,
    /// Owned, opt-in VM stack samples collected during this run.
    cpu_profile: Option<Box<otter_vm::CpuProfile>>,
    /// Owned, opt-in collection pauses observed during this run.
    gc_pauses: Option<Box<GcPauseCapture>>,
}

impl ExecutionResult {
    /// Build from an interpreter completion value.
    #[must_use]
    pub(super) fn from_vm_value(
        completion: otter_vm::Value,
        duration: Duration,
        heap: &otter_gc::GcHeap,
    ) -> Self {
        Self {
            completion: completion.display_string(heap),
            exit_code: 0,
            explicit_exit: false,
            duration,
            stats: Box::default(),
            jit_debug_report: None,
            jit_artifacts: None,
            cpu_profile: None,
            gc_pauses: None,
        }
    }

    /// Build from a host-visible runtime exit request.
    #[must_use]
    pub(super) fn from_exit_code(code: u8, duration: Duration) -> Self {
        Self {
            completion: "undefined".to_string(),
            exit_code: code,
            explicit_exit: true,
            duration,
            stats: Box::default(),
            jit_debug_report: None,
            jit_artifacts: None,
            cpu_profile: None,
            gc_pauses: None,
        }
    }

    pub(super) fn with_stats(mut self, stats: RuntimeExecutionStats) -> Self {
        self.stats = Box::new(stats);
        self
    }

    pub(super) fn with_jit_debug_report(mut self, report: JitDebugReport) -> Self {
        let report = match self.jit_debug_report.take() {
            Some(existing) if report.is_empty() => existing,
            Some(existing) => Box::new((*existing).merged(report)),
            None => Box::new(report),
        };
        self.jit_debug_report = Some(report);
        self
    }

    pub(super) fn with_cpu_profile(mut self, profile: otter_vm::CpuProfile) -> Self {
        match self.cpu_profile.take() {
            Some(mut existing) => {
                existing.samples.extend(profile.samples);
                existing.time_deltas_us.extend(profile.time_deltas_us);
                existing.dropped_samples = existing
                    .dropped_samples
                    .saturating_add(profile.dropped_samples);
                existing.truncated_frames = existing
                    .truncated_frames
                    .saturating_add(profile.truncated_frames);
                self.cpu_profile = Some(existing);
            }
            None => self.cpu_profile = Some(Box::new(profile)),
        }
        self
    }

    pub(super) fn with_gc_pauses(mut self, pauses: GcPauseCapture) -> Self {
        match self.gc_pauses.take() {
            Some(mut existing) => {
                existing.records.extend(pauses.records);
                existing.dropped_records = existing
                    .dropped_records
                    .saturating_add(pauses.dropped_records);
                existing.contains_split_phases |= pauses.contains_split_phases;
                self.gc_pauses = Some(existing);
            }
            None => self.gc_pauses = Some(Box::new(pauses)),
        }
        self
    }

    /// Collection pauses observed during the run, when it asked for them
    /// through [`RuntimeBuilder::gc_trace`].
    #[must_use]
    pub fn gc_pauses(&self) -> Option<&GcPauseCapture> {
        self.gc_pauses.as_deref()
    }

    /// Owned VM stack samples, when the run asked for a CPU profile through
    /// [`RuntimeBuilder::cpu_profile_interval`].
    #[must_use]
    pub fn cpu_profile(&self) -> Option<&otter_vm::CpuProfile> {
        self.cpu_profile.as_deref()
    }

    pub(super) fn with_jit_artifacts(mut self, artifacts: JitArtifactBatch) -> Self {
        let artifacts = match self.jit_artifacts.take() {
            Some(existing) => Box::new((*existing).merged(artifacts)),
            None => Box::new(artifacts),
        };
        self.jit_artifacts = Some(artifacts);
        self
    }

    #[must_use]
    pub(crate) fn with_exit_code(mut self, code: u8) -> Self {
        self.exit_code = code;
        self
    }

    /// See the `explicit_exit` field.
    #[must_use]
    pub(crate) fn explicit_exit(&self) -> bool {
        self.explicit_exit
    }

    /// Render the completion value for CLI preview output.
    #[must_use]
    pub fn completion_string(&self) -> &str {
        &self.completion
    }

    /// Process-style exit status requested by runtime APIs.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        self.exit_code
    }

    /// Diagnostic counter snapshot sampled after the run completed.
    #[must_use]
    pub fn stats(&self) -> RuntimeExecutionStats {
        *self.stats
    }

    /// Structured JIT diagnostics captured for this top-level run.
    ///
    /// Returns `None` when capture was not requested. The report is owned and
    /// remains valid across later allocations and full collections.
    #[must_use]
    pub fn jit_debug_report(&self) -> Option<&JitDebugReport> {
        self.jit_debug_report.as_deref()
    }

    /// Take the owned JIT report without cloning its event payloads.
    #[must_use]
    pub fn take_jit_debug_report(&mut self) -> Option<JitDebugReport> {
        self.jit_debug_report.take().map(|report| *report)
    }

    /// Successful compile artifacts captured for this top-level run.
    #[must_use]
    pub fn jit_artifacts(&self) -> Option<&JitArtifactBatch> {
        self.jit_artifacts.as_deref()
    }

    /// Take the owned artifact batch without cloning code or text payloads.
    #[must_use]
    pub fn take_jit_artifacts(&mut self) -> Option<JitArtifactBatch> {
        self.jit_artifacts.take().map(|artifacts| *artifacts)
    }
}

/// One top-level execution plus diagnostics retained on abrupt failure.
///
/// Ordinary runtime APIs keep returning `Result<ExecutionResult, OtterError>`.
/// Diagnostic-aware callers use this non-error envelope when they must persist
/// partial JIT events and successful compile artifacts even though execution
/// did not produce a result.
#[derive(Debug)]
pub struct ExecutionAttempt {
    result: Result<ExecutionResult, OtterError>,
    failure_jit_debug_report: Option<Box<JitDebugReport>>,
    failure_jit_artifacts: Option<Box<JitArtifactBatch>>,
    /// Final process exit code chosen by an `'exit'` listener when the run
    /// itself failed (an uncaught exception still fires the exit event, and
    /// the listener may replace the code). `None` when no listener ran.
    exit_code_override: Option<u8>,
}

impl ExecutionAttempt {
    pub(super) fn from_result(
        result: Result<ExecutionResult, OtterError>,
        report: Option<JitDebugReport>,
        artifacts: Option<JitArtifactBatch>,
    ) -> Self {
        match result {
            Ok(result) => {
                let result = match report {
                    Some(report) => result.with_jit_debug_report(report),
                    None => result,
                };
                let result = match artifacts {
                    Some(artifacts) => result.with_jit_artifacts(artifacts),
                    None => result,
                };
                Self {
                    result: Ok(result),
                    failure_jit_debug_report: None,
                    failure_jit_artifacts: None,
                    exit_code_override: None,
                }
            }
            Err(error) => Self {
                result: Err(error),
                failure_jit_debug_report: report.map(Box::new),
                failure_jit_artifacts: artifacts.map(Box::new),
                exit_code_override: None,
            },
        }
    }

    /// Borrow the execution result or its original runtime error.
    pub fn result(&self) -> Result<&ExecutionResult, &OtterError> {
        self.result.as_ref()
    }

    /// See the `exit_code_override` field.
    #[must_use]
    pub fn exit_code_override(&self) -> Option<u8> {
        self.exit_code_override
    }

    /// Stamp the exit code an `'exit'` listener chose for a failed run.
    #[must_use]
    pub(crate) fn with_exit_code_override(mut self, code: Option<u8>) -> Self {
        self.exit_code_override = code;
        self
    }

    /// Borrow the complete or partial JIT diagnostics report.
    #[must_use]
    pub fn jit_debug_report(&self) -> Option<&JitDebugReport> {
        match &self.result {
            Ok(result) => result.jit_debug_report(),
            Err(_) => self.failure_jit_debug_report.as_deref(),
        }
    }

    /// Borrow complete or partial successful compile artifacts.
    #[must_use]
    pub fn jit_artifacts(&self) -> Option<&JitArtifactBatch> {
        match &self.result {
            Ok(result) => result.jit_artifacts(),
            Err(_) => self.failure_jit_artifacts.as_deref(),
        }
    }

    /// Discard failure-only diagnostics and recover the established API result.
    pub fn into_result(self) -> Result<ExecutionResult, OtterError> {
        self.result
    }

    /// Split execution from its owned complete or partial JIT event report.
    ///
    /// Successful-result artifacts remain attached to the returned
    /// [`ExecutionResult`]. Use [`Self::into_diagnostic_parts`] when a host
    /// needs to persist both diagnostic channels after abrupt failure.
    pub fn into_parts(self) -> (Result<ExecutionResult, OtterError>, Option<JitDebugReport>) {
        match self.result {
            Ok(mut result) => {
                let report = result.take_jit_debug_report();
                (Ok(result), report)
            }
            Err(error) => (
                Err(error),
                self.failure_jit_debug_report.map(|report| *report),
            ),
        }
    }

    /// Split execution from both owned complete or partial JIT channels.
    pub fn into_diagnostic_parts(
        self,
    ) -> (
        Result<ExecutionResult, OtterError>,
        Option<JitDebugReport>,
        Option<JitArtifactBatch>,
    ) {
        match self.result {
            Ok(mut result) => {
                let report = result.take_jit_debug_report();
                let artifacts = result.take_jit_artifacts();
                (Ok(result), report, artifacts)
            }
            Err(error) => (
                Err(error),
                self.failure_jit_debug_report.map(|report| *report),
                self.failure_jit_artifacts.map(|artifacts| *artifacts),
            ),
        }
    }
}

/// Machine-readable diagnostic counter snapshot sampled at the end of a run.
///
/// These counters reflect the current `Runtime` state at sampling time. A reused
/// runtime reports cumulative counters unless the caller resets the relevant VM
/// budget counters between runs.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeExecutionStats {
    /// `LoadProperty` IC fast-path hits.
    pub property_load_hits: u64,
    /// `LoadProperty` IC misses or absent entries.
    pub property_load_misses: u64,
    /// `LoadProperty` IC installs.
    pub property_load_installs: u64,
    /// `LoadProperty` IC disables / megamorphic transitions.
    pub property_load_disables: u64,
    /// `StoreProperty` IC fast-path hits.
    pub property_store_hits: u64,
    /// `StoreProperty` IC misses or absent entries.
    pub property_store_misses: u64,
    /// `StoreProperty` IC installs.
    pub property_store_installs: u64,
    /// `StoreProperty` IC disables / megamorphic transitions.
    pub property_store_disables: u64,
    /// Unified VM work units executed.
    pub work_units_executed: u64,
    /// RegExp backtrack points included in `work_units_executed`.
    pub regex_backtrack_steps: u64,
    /// GC work included in `work_units_executed`.
    pub gc_work_units: u64,
    /// Cooperative work-slice yields.
    pub forced_yields: u64,
    /// Hard work-budget rejections.
    pub budget_rejections: u64,
    /// Bytecode calls observed by work-budget stats.
    pub bytecode_calls: u64,
    /// Native calls observed by work-budget stats.
    pub native_calls: u64,
    /// Construct calls observed by work-budget stats.
    pub construct_calls: u64,
    /// Max frame-stack depth observed.
    pub max_stack_depth_observed: u32,
    /// Bytes allocated in the largest observed root turn.
    pub max_turn_allocated_bytes: u64,
    /// Longest observed root turn, in nanoseconds.
    pub max_turn_nanos: u64,
    /// Compiled closure-inline validation transitions.
    pub jit_runtime_calls: u64,
    /// Counted Template stack-frame calls into native callee code.
    /// The Graph backend records cold exits but omits entry accounting.
    pub jit_generated_calls: u64,
    /// Compiler-generated calls resumed through cold callee deoptimization.
    pub jit_generated_call_deopts: u64,
    /// Generated callers invalidated because a callee generation changed.
    pub jit_caller_invalidations: u64,
    /// Call-family transitions from generated code into Rust completion paths.
    pub jit_to_rust_call_transitions: u64,
    /// Generated template-tier callee entries.
    pub jit_generated_template_entries: u64,
    /// Generated template-tier callees that returned normally.
    pub jit_generated_template_returns: u64,
    /// Generated template-tier callees that cold-deoptimized.
    pub jit_generated_template_deopts: u64,
    /// Generated optimizing-tier callee entries. The current Graph backend
    /// omits this accounting, so it reports zero.
    pub jit_generated_optimizing_entries: u64,
    /// Generated optimizing-tier returns derived from recorded entries.
    /// The current Graph backend omits entries, so it reports zero.
    pub jit_generated_optimizing_returns: u64,
    /// Generated optimizing-tier callees that cold-deoptimized.
    pub jit_generated_optimizing_deopts: u64,
    /// Materialized interpreter-to-optimizing entry and OSR transfers.
    /// Ordinary calls that select the compiled call ABI bypass this counter.
    pub jit_optimized_entries: u64,
    /// Optimizing-tier entries materialized at a hot loop header.
    pub jit_optimized_osr_entries: u64,
    /// Optimizing-tier deopts resumed on reconstructed interpreter frames.
    pub jit_optimized_deopts: u64,
    /// Function-entry compile attempts across native tiers.
    pub jit_compile_attempts: u64,
    /// Successfully installed native code generations across tiers.
    pub jit_code_generations: u64,
    /// Loop-OSR threshold attempts.
    pub jit_osr_attempts: u64,
    /// JIT property/method/element/global/context runtime stub calls.
    pub jit_runtime_property_stubs: u64,
    /// ABI-classified runtime stub transitions from compiled code.
    pub jit_runtime_stub_transitions: u64,
    /// ABI-classified leaf runtime stubs.
    pub jit_leaf_stub_transitions: u64,
    /// ABI-classified allocating runtime stubs.
    pub jit_alloc_stub_transitions: u64,
    /// ABI-classified re-entrant runtime stubs.
    pub jit_reentrant_stub_transitions: u64,
    /// Executed `AllocValueStub` entries that returned `Ok`.
    pub jit_alloc_value_stub_ok: u64,
    /// Executed `AllocValueStub` entries that returned `Miss`.
    pub jit_alloc_value_stub_miss: u64,
    /// Executed `AllocValueStub` entries that returned `OutOfMemory`.
    pub jit_alloc_value_stub_out_of_memory: u64,
    /// Executed `AllocValueStub` entries that returned another non-`Ok` status.
    pub jit_alloc_value_stub_other: u64,
    /// Pre-effect allocation-plan guard misses.
    pub jit_receiver_alloc_guard_misses: u64,
    /// Nursery-window capacity/state misses.
    pub jit_receiver_alloc_space_misses: u64,
    /// Receivers the runtime's canonical construction created; a construct
    /// a generated receiver plan serves adds none.
    pub jit_receiver_alloc_canonical: u64,
    /// Receiver allocation deoptimizations.
    pub jit_receiver_alloc_deopts: u64,
    /// VM-published collection method IC mirror slots.
    pub jit_collection_method_ic_slots: u64,
    /// Empty collection method IC mirror slots.
    pub jit_collection_method_ic_empty_slots: u64,
    /// Live collection method IC mirror slots.
    pub jit_collection_method_ic_collection_slots: u64,
    /// Live collection method IC slots with leaf/no-allocation stubs.
    pub jit_collection_method_ic_leaf_stub_slots: u64,
    /// Live collection method IC slots with allocating stubs.
    pub jit_collection_method_ic_alloc_stub_slots: u64,
    /// Total GC-cell bytes allocated since heap creation.
    pub gc_alloc_bytes_total: u64,
    /// Live heap objects after the last stats reconciliation.
    pub gc_live_objects: usize,
    /// Live heap bytes after the last stats reconciliation.
    pub gc_live_bytes: usize,
    /// Full GC cycles executed.
    pub gc_cycles: u64,
    /// Most recent full-GC pause in milliseconds.
    pub gc_last_pause_ms: f32,
    /// Cumulative full-GC pause time, in nanoseconds.
    pub gc_full_pause_ns_total: u64,
    /// Minor (young-gen scavenge) cycles executed.
    pub gc_minor_cycles: u64,
    /// Cumulative minor-GC pause time, in nanoseconds.
    pub gc_minor_pause_ns_total: u64,
    /// Cumulative root slots visited across minor GCs, excluding heap tracing.
    pub gc_minor_root_slots_scanned: u64,
    /// Largest number of root slots visited by one minor GC.
    pub gc_minor_root_slots_peak: u64,
    /// Cumulative minor-GC slot updates, including relocation and weak-slot clearing.
    pub gc_minor_slot_updates: u64,
    /// Cumulative remembered-set entries scanned across all minor GCs.
    pub gc_minor_dirty_cards_scanned: u64,
    /// Cumulative old-space headers strided to re-derive edge owners; zero
    /// once the parents are held directly (proves no per-page header walk).
    pub gc_minor_old_headers_walked: u64,
    /// Cumulative remembered parents re-traced across all minor GCs.
    pub gc_minor_objects_retraced: u64,
    /// Cumulative slots visited while tracing remembered parents and the
    /// children of freshly promoted bodies.
    pub gc_minor_slots_scanned: u64,
}
