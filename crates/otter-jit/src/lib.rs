//! Native JIT tiers for the Otter VM.
//!
//! The baseline compiler is a Sparkplug-style template macro-assembler that
//! lowers Otter register bytecode directly to native machine code with no
//! register allocation or deopt. Backend-independent optimizing analyses are
//! consumed by the general optimizing emitter, while profitable straight-line
//! Number leaves use Cranelift inside the same optimizing tier. Native
//! execution reuses the interpreter's frame array and explicit safepoint
//! records for moving-GC rooting. Cranelift produces relocation-free bytes
//! only; the existing dynasm-backed [`CompiledCode`] remains the sole W^X
//! executable-memory owner.
//!
//! # Contents
//! - [`CompiledCode`] — a finalized, owned block of W^X executable machine code
//!   plus its entry offset. The foundational output type every compile produces.
//! - [`ir`] — backend-independent analysis structures for optimizing compilers.
//! - [`optimizing`] — the production-wired reducible numeric/element tier with
//!   function and loop-header OSR entries.
//! - Default-off owned artifact sidecars containing tier input, exact code,
//!   code maps, deopt metadata, and safepoints for outer-host persistence.
//!
//! # Invariants
//! - **`unsafe` is contained here.** This crate lifts the workspace
//!   `forbid(unsafe_code)` (like `otter-gc`) because emitting and executing
//!   machine code requires W^X mappings and fn-pointer transmutes. All `unsafe`
//!   stays behind this crate's safe API; `otter-vm` keeps the ban and reaches
//!   the JIT through a runtime-wired trait hook (no dependency cycle).
//! - **Canonical GC roots.** Compiled code keeps live JS values in the reused
//!   interpreter frame array (already a `FrameRoots` provider), publishes an
//!   explicit safepoint record for allocating calls, and reloads derived object
//!   pointers after every safepoint. A value cached only in a machine register
//!   across a safepoint would be a use-after-move bug.
//! - **One runtime stack.** The optimizing tier and template baseline share the
//!   VM-owned hook, registry, frame array, and fallback interpreter; neither is
//!   a parallel engine/runtime stack.
//! - **JIT is runtime-optional.** When executable memory cannot be obtained
//!   (missing macOS `allow-jit` entitlement, locked sandbox, etc.) the engine
//!   falls back to the interpreter; the JIT never hard-fails execution.
//! - **Diagnostics stay cold and owned.** Disabled compilation does not build
//!   maps, format tier input, or clone code. Enabled sidecars contain no GC
//!   handles, executable pointers, runtime borrows, locks, or sinks.
//!
//! # See also
//! - `JIT_DESIGN.md` — full design, phasing, and the §3.2 backend decision.
//! - `otter-gc` — the moving collector, `FrameRoots`, and the W^X/rooting
//!   contract this tier must honor.

#[cfg(target_arch = "aarch64")]
mod arm64;
mod artifact;
mod code;
mod entry;
pub mod ir;
pub mod optimizing;
mod template;

pub use code::CompiledCode;
pub use entry::{BackendFailure, TransitionTable, Unsupported};
pub use optimizing::{OptimizedCode, compile_optimized};
pub use template::{TemplateCode, compile};

/// Native-tier policy selected by the embedding runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitTierPolicy {
    /// Run the optimizing tier before falling back to template compilation.
    ProductionTiered,
    /// Compile only with the template tier.
    TemplateOnly,
}

/// JIT compiler implementation wired into `otter-vm` through the VM-owned
/// [`otter_vm::JitCompilerHook`] trait.
///
/// The constructor requires an explicit [`JitTierPolicy`]. Hosts that want
/// interpreter-only execution do not install the hook.
pub struct OtterJitCompiler {
    policy: JitTierPolicy,
    /// Hook-lifetime resolution of the transition inventory; every compile
    /// bakes entry addresses through this table.
    transitions: TransitionTable,
    /// Immutable host ISA used only by the profitable numeric-leaf backend.
    #[cfg(target_arch = "aarch64")]
    numeric_leaf_backend: Option<optimizing::NumericLeafBackend>,
}

impl Default for OtterJitCompiler {
    fn default() -> Self {
        Self::production_tiered()
    }
}

impl std::fmt::Debug for OtterJitCompiler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OtterJitCompiler")
            .field("policy", &self.policy)
            .finish()
    }
}

impl OtterJitCompiler {
    /// Construct the production tiered compiler.
    #[must_use]
    pub fn production_tiered() -> Self {
        Self::with_policy(JitTierPolicy::ProductionTiered)
    }

    /// Construct a template-only compiler.
    #[must_use]
    pub fn template_only() -> Self {
        Self::with_policy(JitTierPolicy::TemplateOnly)
    }

    fn with_policy(policy: JitTierPolicy) -> Self {
        let transitions = TransitionTable::resolve();
        #[cfg(target_arch = "aarch64")]
        let numeric_leaf_backend = (policy == JitTierPolicy::ProductionTiered)
            .then(optimizing::NumericLeafBackend::for_host)
            .flatten();
        Self {
            policy,
            transitions,
            #[cfg(target_arch = "aarch64")]
            numeric_leaf_backend,
        }
    }
}

impl otter_vm::JitCompilerHook for OtterJitCompiler {
    fn optimizing_tier_enabled(&self) -> bool {
        self.policy == JitTierPolicy::ProductionTiered
    }

    fn optimizing_generated_entry_supported(
        &self,
        snapshot: &otter_vm::JitCompileSnapshot,
        osr_pc: Option<u32>,
    ) -> bool {
        #[cfg(target_arch = "aarch64")]
        {
            self.policy == JitTierPolicy::ProductionTiered
                && self
                    .numeric_leaf_backend
                    .as_ref()
                    .is_some_and(|backend| backend.supports_generated_entry(snapshot, osr_pc))
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            let _ = (snapshot, osr_pc);
            false
        }
    }

    fn runtime_stub_bindings(&self) -> Vec<otter_vm::JitRuntimeStubBinding> {
        entry::runtime_stub_bindings()
    }

    // The baseline tier serves OSR requests too: it builds a loop-header
    // OSR trampoline per back-edge target, so a hot loop with an opcode
    // outside the compiled subset still tiers up to a native loop body
    // instead of interpreting.
    fn compile_function(
        &self,
        request: otter_vm::JitCompileRequest,
    ) -> Result<otter_vm::JitCompileStatus, otter_vm::JitCompileError> {
        let fid = request.snapshot.code_block.id;
        let capture_events = request.debug.events_enabled();
        let artifact_request = request
            .debug
            .artifacts_enabled()
            .then_some(request.artifact_identity)
            .flatten()
            .map(|identity| artifact::ArtifactRequest {
                identity,
                tier: otter_vm::JitDebugTier::Template,
                entry: request
                    .osr_pc
                    .map_or(otter_vm::JitDebugTarget::Entry, |pc| {
                        otter_vm::JitDebugTarget::Osr { pc }
                    }),
            });
        #[cfg(target_arch = "aarch64")]
        let compiled = template::compile_with_artifacts(
            &request.snapshot,
            request.code_object_id,
            &self.transitions,
            artifact_request,
            capture_events,
        );
        #[cfg(not(target_arch = "aarch64"))]
        let compiled = {
            let _ = artifact_request;
            template::compile(&request.snapshot, request.code_object_id, &self.transitions).map(
                |code| artifact::NativeCompileOutput {
                    code,
                    artifact: None,
                    diagnostics: Box::default(),
                },
            )
        };
        match compiled {
            Ok(output) => Ok(otter_vm::JitCompileStatus::Compiled {
                code: std::sync::Arc::new(output.code),
                artifact: output.artifact,
                diagnostics: output.diagnostics,
            }),
            Err(reason) => Ok(otter_vm::JitCompileStatus::Unsupported {
                reason: format!("function {fid} not in template subset: {reason:?}"),
            }),
        }
    }

    fn compile_optimized_function(
        &self,
        request: otter_vm::JitCompileRequest,
    ) -> Result<otter_vm::JitCompileStatus, otter_vm::JitCompileError> {
        if self.policy == JitTierPolicy::TemplateOnly {
            return Ok(otter_vm::JitCompileStatus::Unavailable);
        }
        let fid = request.snapshot.code_block.id;
        let capture_events = request.debug.events_enabled();
        #[cfg(target_arch = "aarch64")]
        if template::has_emit_eligible_inline_method(&request.snapshot) {
            return Ok(otter_vm::JitCompileStatus::Unsupported {
                reason: format!("function {fid} prefers the template method-inline path"),
            });
        }
        let artifact_request = request
            .debug
            .artifacts_enabled()
            .then_some(request.artifact_identity)
            .flatten()
            .map(|identity| artifact::ArtifactRequest {
                identity,
                tier: otter_vm::JitDebugTier::Optimizing,
                entry: request
                    .osr_pc
                    .map_or(otter_vm::JitDebugTarget::Entry, |pc| {
                        otter_vm::JitDebugTarget::Osr { pc }
                    }),
            });
        #[cfg(target_arch = "aarch64")]
        let compiled = optimizing::compile_optimized_with_artifacts(
            &request.snapshot,
            request.code_object_id,
            &self.transitions,
            self.numeric_leaf_backend.as_ref(),
            request.osr_pc,
            artifact_request,
            capture_events,
        );
        #[cfg(not(target_arch = "aarch64"))]
        let compiled = {
            let _ = artifact_request;
            optimizing::compile_optimized_with_transitions(
                &request.snapshot,
                request.code_object_id,
                &self.transitions,
            )
            .map(|code| artifact::NativeCompileOutput {
                code,
                artifact: None,
                diagnostics: Box::default(),
            })
        };
        match compiled {
            Ok(output) => Ok(otter_vm::JitCompileStatus::Compiled {
                code: std::sync::Arc::new(output.code),
                artifact: output.artifact,
                diagnostics: output.diagnostics,
            }),
            Err(reason) => Ok(otter_vm::JitCompileStatus::Unsupported {
                reason: format!("function {fid} not in optimizing subset: {reason:?}"),
            }),
        }
    }
}

#[cfg(test)]
mod tier_policy_tests {
    use otter_vm::JitCompilerHook;

    use super::OtterJitCompiler;

    #[test]
    fn compiler_reports_only_the_selected_tiers() {
        assert!(
            OtterJitCompiler::production_tiered().optimizing_tier_enabled(),
            "production policy must expose optimizing compilation"
        );
        assert!(
            !OtterJitCompiler::template_only().optimizing_tier_enabled(),
            "template-only policy must stop promotion before snapshotting"
        );
    }
}

#[cfg(all(test, target_arch = "aarch64"))]
mod toolchain_tests {
    //! In-workspace proof that the dynasm-rs arm64 toolchain emits and executes
    //! JIT code under this crate's unsafe-lift. These are the §3.2 gate's
    //! toolchain + tagged-codegen checks, running inside the real workspace
    //! build.

    use crate::CompiledCode;
    use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};

    fn assemble<F>(emit: F) -> CompiledCode
    where
        F: FnOnce(&mut dynasmrt::aarch64::Assembler) -> dynasmrt::AssemblyOffset,
    {
        let mut ops = dynasmrt::aarch64::Assembler::new().unwrap();
        let entry = emit(&mut ops);
        CompiledCode::new(ops.finalize().unwrap(), entry)
    }

    #[test]
    fn emits_and_runs_ret_const() {
        let code = assemble(|ops| {
            let entry = ops.offset();
            dynasm!(ops
                ; .arch aarch64
                ; movz w0, 42
                ; ret
            );
            entry
        });
        // SAFETY: emitted `extern "C" fn() -> i32`; `code` outlives the call.
        let f: extern "C" fn() -> i32 = unsafe { std::mem::transmute(code.entry_ptr()) };
        assert_eq!(f(), 42, "arm64 JIT toolchain must execute on this host");
    }

    #[test]
    fn emits_and_runs_tagged_fib() {
        // Tagged fib over the JSC value encoding: an int32 carries NUMBER_TAG
        // (0xfffe in the top 16 bits) with the payload in the low 32. int32
        // guard + checked arith + rebox; self-recursive.
        let code = assemble(|ops| {
            let entry = ops.offset();
            dynasm!(ops
                ; .arch aarch64
                ; ->fibt:
                ; lsr x9, x0, #48
                ; movz x10, #0xfffe
                ; cmp x9, x10
                ; b.ne >slow
                ; cmp w0, #2
                ; b.lt >done
                ; stp x29, x30, [sp, #-48]!
                ; stp x19, x20, [sp, #16]
                ; stp x21, x22, [sp, #32]
                ; movz x21, #0xfffe, lsl #48
                ; mov w19, w0
                ; sub w0, w19, #1
                ; orr x0, x0, x21
                ; bl ->fibt
                ; mov w20, w0
                ; sub w0, w19, #2
                ; orr x0, x0, x21
                ; bl ->fibt
                ; add w0, w0, w20
                ; orr x0, x0, x21
                ; ldp x21, x22, [sp, #32]
                ; ldp x19, x20, [sp, #16]
                ; ldp x29, x30, [sp], #48
                ; ret
                ; done:
                ; ret
                ; slow:
                ; brk #1
            );
            entry
        });
        let box_i32 = |v: i32| -> u64 { (0xfffeu64 << 48) | (v as u32 as u64) };
        let unbox = |v: u64| -> i32 { v as u32 as i32 };
        // SAFETY: emitted `extern "C" fn(u64) -> u64`; `code` outlives the call.
        let f: extern "C" fn(u64) -> u64 = unsafe { std::mem::transmute(code.entry_ptr()) };
        assert_eq!(unbox(f(box_i32(10))), 55, "tagged fib(10) == 55");
        assert_eq!(unbox(f(box_i32(20))), 6765, "tagged fib(20) == 6765");
    }
}
