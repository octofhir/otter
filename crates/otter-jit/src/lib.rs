//! Otter JIT for OtterJS.
//!
//! # Architecture
//!
//! ```text
//! Tier 1 candidate: bytecode -> template baseline -> asm
//! Tier 2 today:     bytecode -> MIR -> [optimize] -> CLIF -> machine code
//! ```
//!
//! The refactor keeps the existing MIR/CLIF pipeline alive while a new
//! template-baseline Tier 1 path is introduced incrementally.
//!
//! Both tiers share one ABI, one JitContext, one deopt model.

pub mod abi;
pub mod arch;
pub mod baseline;
pub mod cache_ir;
pub mod code_cache;
pub mod compile_queue;
pub mod code_memory;
pub mod codegen;
pub mod config;
pub mod context;
pub mod deopt;
pub mod helpers;
pub mod ic;
pub mod mir;
pub mod osr;
pub mod osr_compile;
pub mod profile_cache;
pub mod snapshot;
pub mod watchpoint;
pub mod pipeline;
mod runtime_helpers;
pub mod telemetry;

pub use deopt::{BAILOUT_SENTINEL, BailoutReason};

/// Compilation tier for a function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Fast compile, no MIR optimization passes.
    Baseline,
    /// Full optimization: guard elimination, inlining, LICM, etc.
    Optimized,
}

/// Result of JIT compilation.
#[derive(Debug)]
pub enum CompileResult {
    /// Compilation succeeded, code is in the code cache.
    Compiled {
        tier: Tier,
        compile_time_ns: u64,
        code_size_bytes: usize,
    },
    /// Function is not eligible for JIT compilation.
    NotEligible,
    /// Compilation failed (internal error, not a correctness issue).
    Error(JitError),
}

/// Result of executing JIT-compiled code.
#[derive(Debug)]
pub enum ExecuteResult {
    /// Execution completed, return value is NaN-boxed u64.
    Ok(u64),
    /// Bailout: resume interpreter at the given bytecode PC.
    Bailout {
        reason: BailoutReason,
        bytecode_pc: u32,
    },
    /// No compiled code available for this function.
    NotCompiled,
    /// Recompilation needed (IC state changed since last compile).
    NeedsRecompilation,
}

/// JIT compilation or execution error.
#[derive(Debug, thiserror::Error)]
pub enum JitError {
    #[error("cranelift error: {0}")]
    Cranelift(String),
    #[error("MIR verification failed: {0}")]
    MirVerification(String),
    #[error("unsupported bytecode instruction: {0}")]
    UnsupportedInstruction(String),
    #[error("code cache full")]
    CodeCacheFull,
    #[error("internal error: {0}")]
    Internal(String),
}

/// Release all thread-local JIT state (code cache, telemetry, helper symbols).
///
/// Must be called when an `OtterRuntime` is dropped so that compiled code and
/// accumulated metrics do not leak across runtime instances.
pub fn cleanup_thread_locals() {
    code_cache::clear();
    telemetry::reset();
    pipeline::clear_helper_symbols();
}
