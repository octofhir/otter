//! Otter JIT for OtterJS.
//!
//! Single-tier template-baseline compiler: Ignition bytecode is walked
//! once and lowered into an x21-pinned aarch64 stencil (interpreter
//! fallback on non-aarch64 hosts). Earlier iterations shipped a MIR →
//! CLIF Tier 2 path and a v1 baseline; both have been retired.
//!
//! # Architecture
//!
//! ```text
//! Tier 1 (only tier today): bytecode → template analyzer → arch emitter → asm
//! ```
//!
//! Every path shares one ABI ([`context::JitContext`]), one bailout
//! sentinel ([`BAILOUT_SENTINEL`]), and one deopt model ([`BailoutReason`]).

pub mod arch;
pub mod baseline;
pub mod code_cache;
pub mod code_memory;
pub mod config;
pub mod context;
pub mod deopt;
pub mod pipeline;
pub mod telemetry;
pub mod tier_up_hook;

pub use deopt::{BAILOUT_SENTINEL, BailoutReason};

/// Compilation tier for a function.
///
/// Only `Baseline` is produced today; `Optimized` is reserved for a
/// future tier 2 pipeline and currently unreachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Template baseline (`bytecode -> asm` stencil).
    Baseline,
    /// Reserved for a future optimizing tier.
    Optimized,
}

/// Result of JIT compilation.
///
/// Returned by the compile pipeline so callers can distinguish a
/// successful install, a deliberate "not our subset" skip, and a real
/// internal error.
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
///
/// Produced by the tier-up hook; the interpreter treats `Bailout` as a
/// side-exit (resume bytecode at `bytecode_pc`) and `NotCompiled` as a
/// cache miss.
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
    /// Bytecode instruction or construct the baseline does not support.
    #[error("unsupported bytecode instruction: {0}")]
    UnsupportedInstruction(String),
    /// Code cache capacity exhausted.
    #[error("code cache full")]
    CodeCacheFull,
    /// Host architecture has no code emitter (non-aarch64 today).
    #[error("unsupported host architecture: {0}")]
    UnsupportedHostArch(&'static str),
    /// Unexpected internal error (not a correctness violation).
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
