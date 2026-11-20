//! JIT generation snapshots, compile events and profiler symbols.
//!
//! # Contents
//! - Residency and generation snapshots, tier outcomes and perf-map publication.
//!
//! # Invariants
//! - Detailed snapshots walk the isolate on request. Event capture reserves space before building payloads.
//!
//! # See also
//! - [`super`] owns compile sessions and installation.

use crate::*;

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
            pending_compilations: self.jit_compile_queue.pending_count(),
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

    pub(super) fn record_jit_install_declined(
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

    pub(super) fn record_jit_compile_finished(
        &mut self,
        fid: u32,
        tier: jit_debug::JitDebugTier,
        target: jit_debug::JitDebugTarget,
        code_object_id: u64,
        pre_hook_exit: Option<jit_debug::JitDebugPreHookExit>,
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
            pre_hook_exit,
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

    /// Name one installed code object of function `name` (`name@source-offset`)
    /// in the perf map.
    pub(super) fn record_jit_perf_map(
        &mut self,
        name: &str,
        code: &dyn jit::JitFunctionCode,
        tier: &str,
    ) {
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
    /// Publish the debug events one baking pass recorded.
    pub(super) fn publish_bake_events(&mut self, events: Vec<jit_debug::JitDebugEvent>) {
        for event in events {
            self.record_jit_debug_event(|| event);
        }
    }
}
