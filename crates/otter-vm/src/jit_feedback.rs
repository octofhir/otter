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
//! - The isolate's VM thread is the sole writer. Arithmetic/element bits use
//!   relaxed atomics because they are advisory and monotonic; call state uses a
//!   release/acquire publication edge because its function id is a separate
//!   payload. Older feedback remains sound through guards and deoptimization.
//!
//! # See also
//! - [`crate::CodeBlock`] — owner of the live feedback vector.

use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use crate::jit::JitElementLoadKind;
use crate::{CallTargetFeedback, Value};

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
const ELEMENT_MASK: u8 = 0b0000_0011;

const CALL_UNSEEN: u8 = 0;
const CALL_MONO: u8 = 1;
const CALL_POLY: u8 = 2;
const CALL_SHIFT: u8 = 2;
const CALL_MASK: u8 = 0b0000_1100;

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
    states: AtomicU8,
    branch_taken: AtomicU8,
    branch_total: AtomicU8,
    call_target: AtomicU32,
}

impl Clone for InstructionFeedback {
    fn clone(&self) -> Self {
        Self {
            arith: AtomicU8::new(self.arith.load(Ordering::Relaxed)),
            states: AtomicU8::new(self.states.load(Ordering::Acquire)),
            branch_taken: AtomicU8::new(self.branch_taken.load(Ordering::Relaxed)),
            branch_total: AtomicU8::new(self.branch_total.load(Ordering::Relaxed)),
            call_target: AtomicU32::new(self.call_target.load(Ordering::Relaxed)),
        }
    }
}

impl InstructionFeedback {
    /// Record one conditional-branch outcome in this instruction's dense cell.
    pub fn record_branch(&self, taken: bool) {
        let saturating_increment = |value: u8| Some(value.saturating_add(1));
        let _ = self.branch_total.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            saturating_increment,
        );
        if taken {
            let _ = self.branch_taken.fetch_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                saturating_increment,
            );
        }
    }

    /// `(taken, total)` conditional-branch observations.
    #[must_use]
    pub fn branch_counts(&self) -> (u8, u8) {
        (
            self.branch_taken.load(Ordering::Relaxed),
            self.branch_total.load(Ordering::Relaxed),
        )
    }

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
            let current = self.states.load(Ordering::Relaxed) & ELEMENT_MASK;
            if current != ELEMENT_UNSEEN {
                self.states
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |states| {
                        Some((states & !ELEMENT_MASK) | ELEMENT_GENERIC)
                    })
                    .ok();
            }
            return;
        };
        let observed = match observed {
            JitElementLoadKind::Any => ELEMENT_GENERIC,
            JitElementLoadKind::Float64 => ELEMENT_FLOAT64,
            JitElementLoadKind::Int32 => ELEMENT_INT32,
        };
        let mut states = self.states.load(Ordering::Relaxed);
        loop {
            let current = states & ELEMENT_MASK;
            let next_element = match current {
                ELEMENT_UNSEEN => observed,
                value if value == observed => value,
                _ => ELEMENT_GENERIC,
            };
            if next_element == current {
                return;
            }
            let next = (states & !ELEMENT_MASK) | next_element;
            match self.states.compare_exchange_weak(
                states,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => states = actual,
            }
        }
    }

    /// Element-load specialization consumed by a compile snapshot.
    #[must_use]
    pub fn element_load_kind(&self) -> JitElementLoadKind {
        match self.states.load(Ordering::Relaxed) & ELEMENT_MASK {
            ELEMENT_FLOAT64 => JitElementLoadKind::Float64,
            ELEMENT_INT32 => JitElementLoadKind::Int32,
            _ => JitElementLoadKind::Any,
        }
    }

    /// Record one bytecode callee at an ordinary `Call` instruction.
    /// Returns `true` only when the previously unseen cell becomes monomorphic.
    pub(crate) fn record_call_target(&self, callee_fid: u32) -> bool {
        match (self.states.load(Ordering::Acquire) & CALL_MASK) >> CALL_SHIFT {
            CALL_UNSEEN => {
                self.call_target.store(callee_fid, Ordering::Relaxed);
                self.states
                    .fetch_update(Ordering::Release, Ordering::Acquire, |states| {
                        Some((states & !CALL_MASK) | (CALL_MONO << CALL_SHIFT))
                    })
                    .ok();
                true
            }
            CALL_MONO if self.call_target.load(Ordering::Relaxed) != callee_fid => {
                self.states
                    .fetch_update(Ordering::Release, Ordering::Acquire, |states| {
                        Some((states & !CALL_MASK) | (CALL_POLY << CALL_SHIFT))
                    })
                    .ok();
                false
            }
            _ => false,
        }
    }

    /// Monomorphic/polymorphic call target observed at this instruction.
    #[must_use]
    pub(crate) fn call_target(&self) -> Option<CallTargetFeedback> {
        match (self.states.load(Ordering::Acquire) & CALL_MASK) >> CALL_SHIFT {
            CALL_UNSEEN => None,
            CALL_MONO => Some(CallTargetFeedback::Mono(
                self.call_target.load(Ordering::Relaxed),
            )),
            _ => Some(CallTargetFeedback::Poly),
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

    #[test]
    fn dense_cell_layout_stays_compact() {
        assert_eq!(std::mem::size_of::<InstructionFeedback>(), 8);
    }

    #[test]
    fn branch_feedback_counts_taken_and_total_compactly() {
        let cell = InstructionFeedback::default();
        cell.record_branch(true);
        cell.record_branch(false);
        cell.record_branch(true);
        assert_eq!(cell.branch_counts(), (2, 3));
        assert_eq!(cell.clone().branch_counts(), (2, 3));
    }

    #[test]
    fn call_target_tracks_mono_then_poly_without_truncating_ids() {
        let max_id = InstructionFeedback::default();
        assert!(max_id.record_call_target(u32::MAX));
        assert_eq!(
            max_id.call_target(),
            Some(CallTargetFeedback::Mono(u32::MAX))
        );

        let cell = InstructionFeedback::default();
        assert!(cell.record_call_target(7));
        assert!(!cell.record_call_target(7));
        assert_eq!(cell.call_target(), Some(CallTargetFeedback::Mono(7)));
        assert!(!cell.record_call_target(9));
        assert_eq!(cell.call_target(), Some(CallTargetFeedback::Poly));
        assert!(!cell.record_call_target(7));
        assert_eq!(cell.call_target(), Some(CallTargetFeedback::Poly));
    }
}
