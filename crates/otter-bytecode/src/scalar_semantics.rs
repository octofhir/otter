//! Typed, target-neutral scalar semantics attached to bytecode operations.
//!
//! # Contents
//! - [`NumericBinaryOp`] — the closed string-or-numeric binary family.
//! - [`NumericBinarySemantics`] — result properties that every execution tier
//!   must preserve when selecting an Int32 or Float64 representation.
//! - [`numeric_binary_semantics`] — the sole opcode-to-numeric-operation map.
//!
//! # Invariants
//! - Interpreter, baseline, and optimizing tiers consume the same operation
//!   identity and Int32 result policy; target emitters do not infer JavaScript
//!   semantics from a raw opcode.
//! - Overflow and signed-zero preservation are independent requirements. An
//!   Int32 fast path may publish its result only after satisfying both.
//! - Coercive and BigInt behavior stays on the canonical runtime operation;
//!   this descriptor governs only representation selection after the operand
//!   domain has been proved.
//!
//! # See also
//! - [`crate::opcode_schema`] for operand, effect, and control-flow metadata.

use serde::Serialize;

use crate::Op;

/// A binary operation governed by ApplyStringOrNumericBinaryOperator.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum NumericBinaryOp {
    /// Addition, including the string-concatenation branch.
    Add,
    /// Numeric or BigInt subtraction.
    Sub,
    /// Numeric or BigInt multiplication.
    Mul,
    /// Numeric or BigInt division.
    Div,
    /// Numeric or BigInt remainder.
    Rem,
    /// Numeric or BigInt exponentiation.
    Pow,
}

impl NumericBinaryOp {
    /// Canonical bytecode identity for runtime ABI publication.
    #[must_use]
    pub const fn opcode(self) -> Op {
        match self {
            Self::Add => Op::Add,
            Self::Sub => Op::Sub,
            Self::Mul => Op::Mul,
            Self::Div => Op::Div,
            Self::Rem => Op::Rem,
            Self::Pow => Op::Pow,
        }
    }

    /// Authoritative semantic descriptor for this operation.
    #[must_use]
    pub const fn semantics(self) -> NumericBinarySemantics {
        numeric_binary_semantics(self.opcode()).expect("numeric operation has a descriptor")
    }
}

/// Operand-sign proof required before an Int32 zero may be published.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum NegativeZeroCondition {
    /// A zero product is negative when exactly one operand is negative.
    OppositeOperandSigns,
    /// A zero remainder is negative when the dividend is negative.
    NegativeLeftOperand,
}

/// Representation contract for an Int32 arithmetic candidate.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", tag = "kind", content = "negative_zero")]
pub enum Int32ResultPolicy {
    /// This operation has no Int32 result representation.
    Unavailable,
    /// Promote an overflowing result to Float64.
    PromoteOverflow,
    /// Promote overflow and any zero satisfying the sign condition to Float64.
    PromoteOverflowOrNegativeZero(NegativeZeroCondition),
}

impl Int32ResultPolicy {
    /// Signed-zero condition that must be disproved before publishing Int32 zero.
    #[must_use]
    pub const fn negative_zero_condition(self) -> Option<NegativeZeroCondition> {
        match self {
            Self::PromoteOverflowOrNegativeZero(condition) => Some(condition),
            Self::Unavailable | Self::PromoteOverflow => None,
        }
    }
}

/// Target-neutral semantic result properties for one numeric binary operation.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct NumericBinarySemantics {
    /// Closed operation identity consumed by all tiers.
    pub operation: NumericBinaryOp,
    /// Whether the operation also owns the string-concatenation branch.
    pub may_concatenate_strings: bool,
    /// Whether equal-kind BigInt operands are accepted by the canonical path.
    pub accepts_bigint: bool,
    /// Conditions under which an Int32 candidate must promote to Float64.
    pub int32_result: Int32ResultPolicy,
}

/// Return the one semantic descriptor for a numeric binary bytecode.
#[must_use]
pub const fn numeric_binary_semantics(op: Op) -> Option<NumericBinarySemantics> {
    let (operation, may_concatenate_strings, int32_result) = match op {
        Op::Add => (
            NumericBinaryOp::Add,
            true,
            Int32ResultPolicy::PromoteOverflow,
        ),
        Op::Sub => (
            NumericBinaryOp::Sub,
            false,
            Int32ResultPolicy::PromoteOverflow,
        ),
        Op::Mul => (
            NumericBinaryOp::Mul,
            false,
            Int32ResultPolicy::PromoteOverflowOrNegativeZero(
                NegativeZeroCondition::OppositeOperandSigns,
            ),
        ),
        Op::Div => (NumericBinaryOp::Div, false, Int32ResultPolicy::Unavailable),
        Op::Rem => (
            NumericBinaryOp::Rem,
            false,
            Int32ResultPolicy::PromoteOverflowOrNegativeZero(
                NegativeZeroCondition::NegativeLeftOperand,
            ),
        ),
        Op::Pow => (NumericBinaryOp::Pow, false, Int32ResultPolicy::Unavailable),
        _ => return None,
    };
    Some(NumericBinarySemantics {
        operation,
        may_concatenate_strings,
        accepts_bigint: true,
        int32_result,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_family_has_one_closed_opcode_map() {
        let operations = [
            NumericBinaryOp::Add,
            NumericBinaryOp::Sub,
            NumericBinaryOp::Mul,
            NumericBinaryOp::Div,
            NumericBinaryOp::Rem,
            NumericBinaryOp::Pow,
        ];
        for operation in operations {
            assert_eq!(operation.semantics().operation, operation);
        }
        assert!(numeric_binary_semantics(Op::Neg).is_none());
    }

    #[test]
    fn multiplication_and_remainder_require_signed_zero_proofs() {
        assert_eq!(
            NumericBinaryOp::Mul
                .semantics()
                .int32_result
                .negative_zero_condition(),
            Some(NegativeZeroCondition::OppositeOperandSigns)
        );
        assert_eq!(
            NumericBinaryOp::Rem
                .semantics()
                .int32_result
                .negative_zero_condition(),
            Some(NegativeZeroCondition::NegativeLeftOperand)
        );
        assert_eq!(
            NumericBinaryOp::Sub
                .semantics()
                .int32_result
                .negative_zero_condition(),
            None
        );
    }
}
