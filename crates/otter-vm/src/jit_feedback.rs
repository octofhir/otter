//! Runtime value-representation feedback consumed by the optimizing JIT tier.
//!
//! The interpreter records observed operand/value representations at numeric
//! bytecode sites while a hot function is still warming up (before it tiers up
//! to compiled code). The optimizing tier reads these cells to pick a node
//! representation — `Int32`, `Float64`, or fully generic `Tagged` — and to
//! insert the matching speculation guard. Once a function is compiled the
//! interpreter arms no longer run for it, so steady-state execution records
//! nothing: the cost is bounded to the warm-up window.
//!
//! # Contents
//! - [`ArithFeedback`] — an OR-accumulated representation bitset for one
//!   numeric-specialized bytecode site, keyed in the interpreter by
//!   `(function_id, byte_pc)`.
//!
//! # Invariants
//! - **Monotonic.** Bits are only ever set, never cleared. A site that has ever
//!   observed a non-numeric operand can therefore never be mis-speculated as
//!   numeric: the optimizing tier's "numeric only" test fails permanently once
//!   a string / bigint / object operand is seen.
//! - **Advisory.** A site that was never recorded reads as empty
//!   ([`ArithFeedback::is_empty`]); the optimizing tier treats that as unknown
//!   and lowers it generically. Dropping or losing feedback is always sound —
//!   only less fast.
//! - Recording happens only while a JIT hook is installed; interpreter-only
//!   execution never touches these cells.
//!
//! # See also
//! - [`crate::jit::JitInstrView::arith_feedback`] — the baked per-instruction
//!   copy the optimizing tier consumes at compile time.

use crate::Value;

/// At least one operand was an `int32` fast-path number.
pub const ARITH_INT32: u8 = 1 << 0;
/// At least one operand was a non-int32 (double) number, including
/// NaN / ±Infinity.
pub const ARITH_FLOAT64: u8 = 1 << 1;
/// At least one operand was a string (the `+` concat path, or a relational
/// string comparison).
pub const ARITH_STRING: u8 = 1 << 2;
/// At least one operand was a BigInt.
pub const ARITH_BIGINT: u8 = 1 << 3;
/// At least one operand was none of the above: boolean, null, undefined,
/// symbol, or object (requiring a full `ToPrimitive` / `ToNumeric`).
pub const ARITH_OTHER: u8 = 1 << 4;

/// Non-numeric observation bits. A site with any of these set can never be
/// speculated as a pure numeric operation.
const NON_NUMERIC: u8 = ARITH_STRING | ARITH_BIGINT | ARITH_OTHER;

/// OR-accumulated representation feedback for one numeric-specialized bytecode
/// site.
///
/// The interpreter folds both operands of every observed execution into the
/// same cell, so the bitset summarises *every representation ever seen at the
/// site*, across executions and across the two operand positions. The
/// optimizing tier reads it to decide whether the site is safe to lower as a
/// speculative `Int32` or `Float64` operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ArithFeedback(u8);

impl ArithFeedback {
    /// Construct a feedback cell directly from raw observation bits. Used when
    /// baking the interpreter cell into the borrow-free compile snapshot.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    /// Raw observation bits, for the baked compile snapshot.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// `true` when no operand representation has ever been recorded.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Fold both operands of one observed execution into the cell.
    pub fn record(&mut self, lhs: Value, rhs: Value) {
        self.0 |= Self::classify(lhs) | Self::classify(rhs);
    }

    /// Representation bit for one operand value.
    fn classify(value: Value) -> u8 {
        if value.is_int32() {
            ARITH_INT32
        } else if value.is_number() {
            ARITH_FLOAT64
        } else if value.is_string() {
            ARITH_STRING
        } else if value.is_big_int() {
            ARITH_BIGINT
        } else {
            ARITH_OTHER
        }
    }

    /// `true` when every operand ever seen was a number (int32 or double) and
    /// the site was observed at least once — the precondition for speculating a
    /// `Float64` lowering with an "is number" guard.
    #[must_use]
    pub const fn is_numeric_only(self) -> bool {
        self.0 != 0 && (self.0 & NON_NUMERIC) == 0
    }

    /// `true` when every operand ever seen was an `int32` — the precondition for
    /// speculating an unboxed `Int32` lowering with an "is int32" guard.
    #[must_use]
    pub const fn is_int32_only(self) -> bool {
        self.0 == ARITH_INT32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_neither_numeric_nor_int32() {
        let fb = ArithFeedback::default();
        assert!(fb.is_empty());
        assert!(!fb.is_numeric_only());
        assert!(!fb.is_int32_only());
    }

    #[test]
    fn pure_int32_site_is_int32_and_numeric() {
        let mut fb = ArithFeedback::default();
        fb.record(Value::number_i32(3), Value::number_i32(4));
        fb.record(Value::number_i32(-1), Value::number_i32(0));
        assert!(fb.is_int32_only());
        assert!(fb.is_numeric_only());
    }

    #[test]
    fn mixed_int_and_double_is_numeric_not_int32() {
        let mut fb = ArithFeedback::default();
        fb.record(Value::number_i32(3), Value::number_f64(2.5));
        assert!(!fb.is_int32_only());
        assert!(fb.is_numeric_only());
    }

    #[test]
    fn any_string_operand_poisons_numeric() {
        let mut fb = ArithFeedback::default();
        fb.record(Value::number_i32(3), Value::number_i32(4));
        fb.record(Value::number_f64(1.0), Value::undefined());
        assert!(!fb.is_numeric_only());
        assert!(!fb.is_int32_only());
        assert_eq!(fb.bits() & ARITH_OTHER, ARITH_OTHER);
    }
}
