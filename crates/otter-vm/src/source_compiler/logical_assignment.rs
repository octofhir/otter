//! Helpers for lowering logical compound assignment operators
//! `||=`, `&&=`, and `??=` (ES2021, spec §13.15.2
//! <https://tc39.es/ecma262/#sec-assignment-operators-runtime-semantics-evaluation>).
//!
//! These differ from arithmetic compound `+=` / `*=` in three ways:
//!
//! 1. They **short-circuit**: if the LHS passes the operator's test
//!    (truthy for `||=`, falsy for `&&=`, non-nullish for `??=`), the
//!    RHS is not evaluated and the write never happens. The
//!    assignment expression's value is the original LHS.
//! 2. They do **not** lower onto a binary operator: there is no
//!    "logical-or" numeric op — the result is either the LHS (on
//!    short-circuit) or the RHS (on fall-through).
//! 3. As a consequence, the store opcode must be guarded by the
//!    short-circuit jump. Arithmetic compound path always stores
//!    unconditionally.
//!
//! The lowering pattern shared by every assignment target is:
//!
//! ```text
//!   <load LHS into acc>
//!   <short-circuit jump to end_label>
//!   <lower RHS into acc>
//!   <store acc → target>
//! end_label:
//! ```
//!
//! When control falls through `emit_short_circuit_jump`, the LHS
//! check failed and the write must proceed. When control jumps to
//! `end_label`, the accumulator still holds the original LHS and no
//! write is performed — which is exactly the assignment expression's
//! result value in that case.

use oxc_ast::ast::AssignmentOperator;

use super::SourceLoweringError;
use crate::bytecode::{BytecodeBuilder, Label, Opcode};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LogicalAssignmentKind {
    /// `||=` — write when LHS is falsy.
    Or,
    /// `&&=` — write when LHS is truthy.
    And,
    /// `??=` — write when LHS is null or undefined.
    Coalesce,
}

pub(super) fn classify(op: AssignmentOperator) -> Option<LogicalAssignmentKind> {
    match op {
        AssignmentOperator::LogicalOr => Some(LogicalAssignmentKind::Or),
        AssignmentOperator::LogicalAnd => Some(LogicalAssignmentKind::And),
        AssignmentOperator::LogicalNullish => Some(LogicalAssignmentKind::Coalesce),
        _ => None,
    }
}

/// Emits the short-circuit jump for a logical compound assignment.
/// Pre: accumulator holds the original LHS value. Post: control
/// either jumps to `end_label` (short-circuit — caller must leave
/// acc untouched for the assignment-expression's result) or falls
/// through (caller proceeds to lower RHS + store).
pub(super) fn emit_short_circuit_jump(
    builder: &mut BytecodeBuilder,
    kind: LogicalAssignmentKind,
    end_label: Label,
) -> Result<(), SourceLoweringError> {
    match kind {
        LogicalAssignmentKind::Or => {
            // `||=`: skip assign when acc is truthy.
            builder
                .emit_jump_to(Opcode::JumpIfToBooleanTrue, end_label)
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode JumpIfToBooleanTrue (||=): {err:?}"
                    ))
                })?;
        }
        LogicalAssignmentKind::And => {
            // `&&=`: skip assign when acc is falsy.
            builder
                .emit_jump_to(Opcode::JumpIfToBooleanFalse, end_label)
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode JumpIfToBooleanFalse (&&=): {err:?}"
                    ))
                })?;
        }
        LogicalAssignmentKind::Coalesce => {
            // `??=`: skip assign only when acc is neither null nor
            // undefined. Mirrors `lower_logical_expression` Coalesce,
            // but with the "fall through to RHS" arm gated on a
            // dedicated label so the caller's store stays inside the
            // fall-through path.
            let check_undefined = builder.new_label();
            let do_assign = builder.new_label();
            builder
                .emit_jump_to(Opcode::JumpIfNotNull, check_undefined)
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode JumpIfNotNull (??=): {err:?}"))
                })?;
            // acc == null: fall through to the assign path.
            builder
                .emit_jump_to(Opcode::Jump, do_assign)
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Jump (??= null → assign): {err:?}"
                    ))
                })?;
            builder.bind_label(check_undefined).map_err(|err| {
                SourceLoweringError::Internal(format!("bind ??= check_undefined: {err:?}"))
            })?;
            // Not null — check undefined. If not undefined either,
            // short-circuit to end_label with acc = original LHS.
            builder
                .emit_jump_to(Opcode::JumpIfNotUndefined, end_label)
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode JumpIfNotUndefined (??=): {err:?}"
                    ))
                })?;
            builder.bind_label(do_assign).map_err(|err| {
                SourceLoweringError::Internal(format!("bind ??= do_assign: {err:?}"))
            })?;
        }
    }
    Ok(())
}
