//! Dense instruction feedback shared by the interpreter and baseline JIT.
//!
//! The interpreter records observed operand/value representations at numeric
//! bytecode sites while a hot function is still warming up. Cells live in the
//! owning [`crate::CodeBlock`] at the canonical instruction index, so recording
//! and compilation never hash `(function_id, pc)` pairs or copy feedback into a
//! parallel interpreter-owned map.
//!
//! # Contents
//! - [`ArithFeedback`] — decoded arithmetic representation bits.
//! - [`InstructionFeedback`] — one dense atomic cell per CodeBlock instruction.
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
//! - Atomic ordering is relaxed: feedback is advisory and monotonic. A compiler
//!   may observe an older, narrower state; emitted guards and deoptimization
//!   preserve correctness when later executions disagree.
//!
//! # See also
//! - [`crate::jit::JitInstructionMetadata`] — compile-time snapshot metadata.

use std::sync::atomic::{AtomicU8, Ordering};

use crate::Value;
use crate::jit::JitElementLoadKind;

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
const ARITH_WIDEN_FLOAT: u8 = 1 << 7;

const ELEMENT_UNSEEN: u8 = 0;
const ELEMENT_FLOAT64: u8 = 1;
const ELEMENT_INT32: u8 = 2;
const ELEMENT_GENERIC: u8 = 3;

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

/// Dense feedback owned by one canonical CodeBlock instruction.
#[derive(Debug, Default)]
pub struct InstructionFeedback {
    arith: AtomicU8,
    element_load: AtomicU8,
}

impl Clone for InstructionFeedback {
    fn clone(&self) -> Self {
        Self {
            arith: AtomicU8::new(self.arith.load(Ordering::Relaxed)),
            element_load: AtomicU8::new(self.element_load.load(Ordering::Relaxed)),
        }
    }
}

impl InstructionFeedback {
    /// Fold one observed arithmetic operand pair into this instruction cell.
    #[inline]
    pub fn record_arith(&self, lhs: Value, rhs: Value) {
        let bits = ArithFeedback::classify(lhs) | ArithFeedback::classify(rhs);
        self.arith.fetch_or(bits, Ordering::Relaxed);
    }

    /// Mark an arithmetic site for float widening after its first overflow bail.
    /// Returns `true` exactly once for the cell.
    pub fn widen_arith_to_float(&self) -> bool {
        self.arith.fetch_or(ARITH_WIDEN_FLOAT, Ordering::Relaxed) & ARITH_WIDEN_FLOAT == 0
    }

    /// Arithmetic bits consumed by a compile snapshot.
    #[must_use]
    pub fn arith_bits(&self) -> u8 {
        let bits = self.arith.load(Ordering::Relaxed);
        if bits & ARITH_WIDEN_FLOAT != 0 {
            ARITH_INT32 | ARITH_FLOAT64
        } else {
            bits & !ARITH_WIDEN_FLOAT
        }
    }

    /// Record the receiver family observed at one `LoadElement` instruction.
    /// `None` preserves an unseen cell for an ordinary non-typed receiver;
    /// mixed or unsupported typed-array kinds become permanently generic.
    pub fn record_element_load(&self, observed: Option<JitElementLoadKind>) {
        let Some(observed) = observed else {
            let current = self.element_load.load(Ordering::Relaxed);
            if current != ELEMENT_UNSEEN {
                self.element_load.store(ELEMENT_GENERIC, Ordering::Relaxed);
            }
            return;
        };
        let observed = match observed {
            JitElementLoadKind::Any => ELEMENT_GENERIC,
            JitElementLoadKind::Float64 => ELEMENT_FLOAT64,
            JitElementLoadKind::Int32 => ELEMENT_INT32,
        };
        let mut current = self.element_load.load(Ordering::Relaxed);
        loop {
            let next = match current {
                ELEMENT_UNSEEN => observed,
                value if value == observed => value,
                _ => ELEMENT_GENERIC,
            };
            if next == current {
                return;
            }
            match self.element_load.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    /// Element-load specialization consumed by a compile snapshot.
    #[must_use]
    pub fn element_load_kind(&self) -> JitElementLoadKind {
        match self.element_load.load(Ordering::Relaxed) {
            ELEMENT_FLOAT64 => JitElementLoadKind::Float64,
            ELEMENT_INT32 => JitElementLoadKind::Int32,
            _ => JitElementLoadKind::Any,
        }
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

    #[test]
    fn dense_cell_widens_once_and_keeps_element_demotion_sticky() {
        let cell = InstructionFeedback::default();
        cell.record_arith(Value::number_i32(1), Value::number_i32(2));
        assert_eq!(cell.arith_bits(), ARITH_INT32);
        assert!(cell.widen_arith_to_float());
        assert!(!cell.widen_arith_to_float());
        assert_eq!(cell.arith_bits(), ARITH_INT32 | ARITH_FLOAT64);

        cell.record_element_load(Some(JitElementLoadKind::Float64));
        assert_eq!(cell.element_load_kind(), JitElementLoadKind::Float64);
        cell.record_element_load(Some(JitElementLoadKind::Int32));
        assert_eq!(cell.element_load_kind(), JitElementLoadKind::Any);
        cell.record_element_load(Some(JitElementLoadKind::Float64));
        assert_eq!(cell.element_load_kind(), JitElementLoadKind::Any);

        let ordinary_then_typed = InstructionFeedback::default();
        ordinary_then_typed.record_element_load(None);
        ordinary_then_typed.record_element_load(Some(JitElementLoadKind::Int32));
        assert_eq!(
            ordinary_then_typed.element_load_kind(),
            JitElementLoadKind::Int32
        );
        ordinary_then_typed.record_element_load(None);
        assert_eq!(
            ordinary_then_typed.element_load_kind(),
            JitElementLoadKind::Any
        );
    }
}
