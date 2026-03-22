//! Otter JIT — Cranelift-based compilation pipeline for OtterJS.
//!
//! # Architecture
//!
//! ```text
//! bytecode -> MIR -> [optimize] -> CLIF -> machine code
//! ```
//!
//! - **Tier 1**: Bytecode -> MIR -> CLIF (OptLevel::None) for fast compile
//! - **Tier 2**: Bytecode -> MIR -> [passes] -> CLIF (OptLevel::Speed) for peak perf
//!
//! Both tiers share one ABI, one JitContext, one deopt model.

pub mod abi;
pub mod code_cache;
pub mod code_memory;
pub mod codegen;
pub mod config;
pub mod context;
pub mod feedback;
pub mod helpers;
pub mod mir;
pub mod pipeline;
pub mod telemetry;

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

/// Why JIT code bailed out to the interpreter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BailoutReason {
    /// Type guard failed (e.g., expected Int32 but got Float64).
    TypeGuardFailed = 0,
    /// Shape guard failed (object's hidden class changed).
    ShapeGuardFailed = 1,
    /// Prototype epoch changed (prototype chain was mutated).
    ProtoEpochMismatch = 2,
    /// Int32 arithmetic overflow.
    Overflow = 3,
    /// Array bounds check failed.
    BoundsCheckFailed = 4,
    /// Array is not dense (sparse or has holes).
    ArrayNotDense = 5,
    /// Call target changed (monomorphic call miss).
    CallTargetMismatch = 6,
    /// Unsupported operation encountered in JIT code.
    Unsupported = 7,
    /// Interrupt flag set (timeout, GC request).
    Interrupted = 8,
    /// Tier-up: function should be recompiled at a higher tier.
    TierUp = 9,
    /// Exception thrown (deopt to interpreter for unwinding).
    Exception = 10,
    /// Debugger breakpoint.
    Breakpoint = 11,
}

/// Sentinel value returned by JIT code to signal a bailout.
/// Must not collide with any valid NaN-boxed value.
pub const BAILOUT_SENTINEL: u64 = 0xDEAD_BA11_0000_0000;

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
