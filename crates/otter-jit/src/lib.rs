//! Baseline JIT for the Otter VM.
//!
//! A Sparkplug-style template macro-assembler that lowers Otter register
//! bytecode to native machine code with **no IR, no register allocation, and no
//! deopt**. It deletes the interpreter dispatch envelope for hot functions while
//! reusing the interpreter's own frame array and explicit safepoint records for
//! moving-GC rooting. Backend chosen by the
//! `JIT_DESIGN.md` §3.2 prototype gate (dynasm-rs template assembler).
//!
//! # Contents
//! - [`CompiledCode`] — a finalized, owned block of W^X executable machine code
//!   plus its entry offset. The foundational output type every compile produces.
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
//! - **One production compiler.** The template baseline is the only native
//!   compiler; the pre-refactor experimental optimizer and its parallel IR were
//!   deleted rather than carried across the CodeBlock migration.
//! - **JIT is runtime-optional.** When executable memory cannot be obtained
//!   (missing macOS `allow-jit` entitlement, locked sandbox, etc.) the engine
//!   falls back to the interpreter; the JIT never hard-fails execution.
//!
//! # See also
//! - `JIT_DESIGN.md` — full design, phasing, and the §3.2 backend decision.
//! - `otter-gc` — the moving collector, `FrameRoots`, and the W^X/rooting
//!   contract this tier must honor.

mod baseline;
mod code;

pub use baseline::{BaselineCode, Unsupported, compile};
pub use code::CompiledCode;

/// Baseline JIT compiler implementation wired into `otter-vm` through the
/// VM-owned [`otter_vm::JitCompilerHook`] trait.
///
/// Step 1 installs the dependency-inverted contract and compile-input DTOs.
/// Real bytecode lowering lands in the following Phase 1 step, so this hook
/// currently reports `Unsupported` and leaves execution on the interpreter
/// fallback path.
#[derive(Debug, Default)]
pub struct BaselineJitCompiler;

impl BaselineJitCompiler {
    /// Construct a baseline JIT compiler hook.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl otter_vm::JitCompilerHook for BaselineJitCompiler {
    fn runtime_stub_bindings(&self) -> Vec<otter_vm::JitRuntimeStubBinding> {
        baseline::runtime_stub_bindings()
    }

    fn compile_function(
        &self,
        request: otter_vm::JitCompileRequest,
    ) -> Result<otter_vm::JitCompileStatus, otter_vm::JitCompileError> {
        let fid = request.snapshot.code_block.id;
        // The baseline tier serves OSR requests too: it builds a loop-header
        // OSR trampoline per back-edge target, so a hot loop the optimizing tier
        // declined (an unsupported opcode or not-yet-int32 feedback in its
        // region) still tiers up to a native loop body instead of interpreting.
        match baseline::compile(&request.snapshot) {
            Ok(code) => Ok(otter_vm::JitCompileStatus::Compiled {
                code: std::sync::Arc::new(code),
            }),
            Err(reason) => Ok(otter_vm::JitCompileStatus::Unsupported {
                reason: format!("function {fid} not in baseline subset: {reason:?}"),
            }),
        }
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
