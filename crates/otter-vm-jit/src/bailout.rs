//! JIT bailout mechanism.
//!
//! When JIT-compiled code encounters an operation it cannot handle (e.g.,
//! unsupported type at runtime, overflow), it "bails out" by returning a
//! special sentinel value. The caller detects this and re-executes the
//! function in the interpreter.
//!
//! # Bailout flow
//!
//! ```text
//! JIT code:
//!   operation check
//!     ├─ handled → fast path result
//!     └─ not handled → return BAILOUT_SENTINEL
//!
//! Caller (jit_runtime / interpreter):
//!   result = call_jit_function(...)
//!   if result == BAILOUT_SENTINEL:
//!     increment bailout_count
//!     if bailout_count >= DEOPT_THRESHOLD:
//!       mark function as deoptimized (never JIT again)
//!     re-execute in interpreter
//! ```

/// Sentinel value returned by JIT code to signal a bailout.
///
/// Uses `0x7FFC_0000_0000_0000` which is in the NaN space but unused by
/// any JS type tag in the NaN-boxing scheme:
///
/// - `0x7FF8_0000` = quiet NaN prefix (undefined, null, true, false, int32)
/// - `0x7FFA_0000` = canonical NaN
/// - `0x7FFC_0000` = **bailout sentinel** (unused by any JS type)
pub const BAILOUT_SENTINEL: i64 = 0x7FFC_0000_0000_0000_u64 as i64;

/// Number of bailouts before a function is deoptimized (JIT code invalidated).
///
/// After this many bailouts, the function is permanently returned to the
/// interpreter and will never be re-queued for JIT compilation.
pub const DEOPT_THRESHOLD: u32 = 10;

/// Reason for a bailout from JIT code.
///
/// Values are persisted in JIT runtime telemetry and therefore must remain
/// stable once released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub enum BailoutReason {
    /// Bailout happened but no specific reason was recorded.
    Unknown = 0,
    /// A runtime helper returned `BAILOUT_SENTINEL`.
    HelperReturnedSentinel = 1,
    /// A speculative/type fast path failed and entered the slow bailout branch.
    TypeGuardFailure = 2,
}

impl BailoutReason {
    /// Integer code persisted in JIT runtime telemetry.
    #[inline]
    pub const fn code(self) -> i64 {
        self as i64
    }

    /// Decode a telemetry code into a reason category.
    #[inline]
    pub const fn from_code(code: i64) -> Self {
        match code {
            1 => Self::HelperReturnedSentinel,
            2 => Self::TypeGuardFailure,
            _ => Self::Unknown,
        }
    }
}

impl Default for BailoutReason {
    fn default() -> Self {
        Self::Unknown
    }
}

/// Check whether a JIT return value is the bailout sentinel.
#[inline]
pub fn is_bailout(value: i64) -> bool {
    value == BAILOUT_SENTINEL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_does_not_collide_with_common_values() {
        // Not zero (f64 +0.0)
        assert_ne!(BAILOUT_SENTINEL, 0);
        // Not small ints that the simple translator produces
        assert_ne!(BAILOUT_SENTINEL, 1);
        assert_ne!(BAILOUT_SENTINEL, -1);
        assert_ne!(BAILOUT_SENTINEL, 42);
        // It's in NaN space
        let high_bits = (BAILOUT_SENTINEL as u64) >> 48;
        assert_eq!(high_bits, 0x7FFC);
    }

    #[test]
    fn is_bailout_detects_sentinel() {
        assert!(is_bailout(BAILOUT_SENTINEL));
        assert!(!is_bailout(0));
        assert!(!is_bailout(42));
        assert!(!is_bailout(-1));
    }
}
