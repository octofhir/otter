//! Bailout reasons shared across JIT compilation and execution paths.

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

impl BailoutReason {
    /// Decodes a raw reason code written into `JitContext`.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::TypeGuardFailed),
            1 => Some(Self::ShapeGuardFailed),
            2 => Some(Self::ProtoEpochMismatch),
            3 => Some(Self::Overflow),
            4 => Some(Self::BoundsCheckFailed),
            5 => Some(Self::ArrayNotDense),
            6 => Some(Self::CallTargetMismatch),
            7 => Some(Self::Unsupported),
            8 => Some(Self::Interrupted),
            9 => Some(Self::TierUp),
            10 => Some(Self::Exception),
            11 => Some(Self::Breakpoint),
            _ => None,
        }
    }
}

/// Sentinel value returned by JIT code to signal a bailout.
/// Must not collide with any valid NaN-boxed value.
pub const BAILOUT_SENTINEL: u64 = 0xDEAD_BA11_0000_0000;
