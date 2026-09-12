//! Snapshot-aware semantic classification for scalar Machine lowering.
//!
//! # Contents
//! - [`InstructionSemantics`] — the effective effects and optional committed
//!   boxed-value operation selected for one immutable bytecode snapshot.
//! - [`classify_snapshot`] — one authoritative classification pass shared by
//!   CFG construction, liveness, and HIR lowering.
//!
//! # Invariants
//! - Bytecode-schema effects are the conservative default. Snapshot proofs may
//!   only replace them with the effects of the exact path Machine will lower.
//!   Packed-double access proofs remain conservative inside an exception
//!   region because their generated path can exact-deopt before the access.
//! - A committed operation always retains the complete allocating, throwing,
//!   reentrant GC boundary even when one member is usually a leaf operation.
//! - The committed family is typed here once; CFG splitting and lowering may
//!   not grow independent opcode mirrors.
//!
//! # See also
//! - `otter_bytecode::opcode_schema::OpcodeEffects`
//! - `crate::machine::CallTarget::CommittedRuntime`
//! - `super::hir::NumericNode::CommittedValue`

use otter_bytecode::{
    Op,
    opcode_schema::{ControlFlow, OpcodeEffects, opcode_schema},
};
use otter_vm::{
    JitCompileSnapshot, JitElementBase,
    native_abi::{ObjectProtocolValueOp, ScalarValueOp},
};

const EFFECTS_NONE: OpcodeEffects = OpcodeEffects {
    may_throw: false,
    may_allocate: false,
    may_trigger_gc: false,
    may_reenter_javascript: false,
    safepoint_required: false,
};

const EFFECTS_ALLOCATING: OpcodeEffects = OpcodeEffects {
    may_throw: false,
    may_allocate: true,
    may_trigger_gc: true,
    may_reenter_javascript: false,
    safepoint_required: true,
};

const EFFECTS_COMMITTED_RUNTIME: OpcodeEffects = OpcodeEffects {
    may_throw: true,
    may_allocate: true,
    may_trigger_gc: true,
    may_reenter_javascript: true,
    safepoint_required: true,
};

/// Typed semantic family completed by the fixed boxed-value runtime boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CommittedValueOperation {
    ObjectProtocol(ObjectProtocolValueOp),
    Scalar(ScalarValueOp),
}

/// Effective semantics of one instruction in an immutable compile snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct InstructionSemantics {
    pub(super) effects: OpcodeEffects,
    pub(super) committed_value: Option<CommittedValueOperation>,
}

impl InstructionSemantics {
    /// Whether this instruction owns an implicit catch edge after lowering.
    pub(super) fn has_implicit_exception_side_exit(self, op: Op) -> bool {
        self.effects.may_throw
            && matches!(
                opcode_schema(op).control_flow,
                ControlFlow::Fallthrough | ControlFlow::Call
            )
    }

    /// Whether this is the complete committed reentrant GC boundary.
    pub(super) fn is_committed_runtime(self) -> bool {
        self.committed_value.is_some() && self.effects == EFFECTS_COMMITTED_RUNTIME
    }
}

fn committed_value_operation(op: Op, derived_constructor: bool) -> Option<CommittedValueOperation> {
    Some(match op {
        Op::Instanceof => {
            CommittedValueOperation::ObjectProtocol(ObjectProtocolValueOp::Instanceof)
        }
        Op::HasProperty => {
            CommittedValueOperation::ObjectProtocol(ObjectProtocolValueOp::HasProperty)
        }
        Op::GetPrototype if !derived_constructor => {
            CommittedValueOperation::ObjectProtocol(ObjectProtocolValueOp::GetPrototype)
        }
        Op::SetPrototype => {
            CommittedValueOperation::ObjectProtocol(ObjectProtocolValueOp::SetPrototype)
        }
        Op::LooseEqual => {
            CommittedValueOperation::ObjectProtocol(ObjectProtocolValueOp::LooseEqual)
        }
        Op::LooseNotEqual => {
            CommittedValueOperation::ObjectProtocol(ObjectProtocolValueOp::LooseNotEqual)
        }
        Op::ToObject => CommittedValueOperation::Scalar(ScalarValueOp::ToObject),
        Op::ToPropertyKey => CommittedValueOperation::Scalar(ScalarValueOp::ToPropertyKey),
        Op::TypeOf => CommittedValueOperation::Scalar(ScalarValueOp::TypeOf),
        Op::LoadNewTarget => CommittedValueOperation::Scalar(ScalarValueOp::LoadNewTarget),
        Op::SameValue => CommittedValueOperation::Scalar(ScalarValueOp::SameValue),
        Op::BindThisValue => CommittedValueOperation::Scalar(ScalarValueOp::BindThisValue),
        _ => return None,
    })
}

fn has_exact_packed_double_access(
    view: &JitCompileSnapshot,
    logical_pc: usize,
    byte_pc: u32,
) -> bool {
    let Ok(logical_pc) = u32::try_from(logical_pc) else {
        return false;
    };
    view.code_block
        .control_flow()
        .enclosing_exception_region(logical_pc)
        .is_none()
        && view.cage_base != 0
        && view.element_accesses.get(&byte_pc).is_some_and(|access| {
            access.type_tag != 0
                && !matches!(access.base, JitElementBase::None)
                && packed_double_element_access_is_exact(access)
        })
}

/// Whether immutable element metadata proves the exact packed-double family.
pub(super) fn packed_double_element_access_is_exact(access: &otter_vm::JitElementAccess) -> bool {
    access.is_packed_double_array()
}

fn classify_instruction(
    view: &JitCompileSnapshot,
    logical_pc: usize,
) -> Option<InstructionSemantics> {
    let instruction = view.instructions.get(logical_pc)?;
    let code = view.code_block.as_ref();
    let op = instruction.op(code);
    // Loose equality stays a guarded numeric or nullish comparison while its
    // feedback is numeric or empty; only a site that has seen coercive
    // operands takes the committed canonical comparison.
    let committed_value = committed_value_operation(op, view.derived_constructor).filter(|_| {
        !matches!(op, Op::LooseEqual | Op::LooseNotEqual) || {
            let feedback = instruction.arith_feedback();
            !feedback.is_numeric_only() && !feedback.is_empty()
        }
    });
    let effects = if committed_value.is_some() || opcode_schema(op).binding.is_some() {
        EFFECTS_COMMITTED_RUNTIME
    } else {
        match op {
            Op::GetPrototype if view.derived_constructor => EFFECTS_NONE,
            Op::LoadString
                if view
                    .string_constant_cells
                    .contains_key(&instruction.byte_pc) =>
            {
                EFFECTS_NONE
            }
            Op::StoreProperty
                if view
                    .constructor_field_transitions
                    .contains_key(&instruction.byte_pc) =>
            {
                EFFECTS_NONE
            }
            Op::LoadElement | Op::StoreElement
                if has_exact_packed_double_access(view, logical_pc, instruction.byte_pc) =>
            {
                EFFECTS_NONE
            }
            Op::ArrayConstruct | Op::NewObject | Op::NewArray => EFFECTS_ALLOCATING,
            Op::Add
                if instruction
                    .arith_feedback()
                    .is_primitive_string_concat_only() =>
            {
                EFFECTS_ALLOCATING
            }
            Op::ToPrimitive
            | Op::ToNumeric
            | Op::ToNumber
            | Op::ToBoolean
            | Op::LogicalNot
            | Op::Neg
            | Op::Increment
            | Op::BitwiseNot
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Rem
            | Op::Pow
            | Op::BitwiseAnd
            | Op::BitwiseOr
            | Op::BitwiseXor
            | Op::Shl
            | Op::Shr
            | Op::Ushr
            | Op::Equal
            | Op::NotEqual
            | Op::LooseEqual
            | Op::LooseNotEqual
            | Op::LessThan
            | Op::LessEq
            | Op::GreaterThan
            | Op::GreaterEq
            | Op::AddImm
            | Op::SubImm
            | Op::BitwiseAndImm
            | Op::LessThanImm
            | Op::EqualImm
            | Op::NotEqualImm => EFFECTS_NONE,
            _ => opcode_schema(op).effects,
        }
    };
    Some(InstructionSemantics {
        effects,
        committed_value,
    })
}

/// Classify every instruction once for all Machine admission consumers.
pub(super) fn classify_snapshot(view: &JitCompileSnapshot) -> Option<Vec<InstructionSemantics>> {
    (0..view.instructions.len())
        .map(|logical_pc| classify_instruction(view, logical_pc))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm::jit::JitTestInstruction;

    fn one_instruction(op: Op, operands: Vec<otter_bytecode::Operand>) -> JitCompileSnapshot {
        JitCompileSnapshot::without_feedback(
            41,
            2,
            3,
            vec![JitTestInstruction::new(op, 0, 0, operands)],
        )
    }

    #[test]
    fn committed_family_keeps_one_complete_effect_contract() {
        let cases = [
            (Op::Instanceof, 3),
            (Op::HasProperty, 3),
            (Op::GetPrototype, 2),
            (Op::SetPrototype, 2),
            (Op::ToObject, 2),
            (Op::ToPropertyKey, 2),
            (Op::TypeOf, 2),
            (Op::LoadNewTarget, 1),
            (Op::SameValue, 3),
            (Op::BindThisValue, 1),
        ];
        for (op, operand_count) in cases {
            let operands = (0..operand_count)
                .map(|index| otter_bytecode::Operand::Register(index as u16))
                .collect::<Vec<_>>();
            let view = one_instruction(op, operands);
            let semantics = classify_snapshot(&view).expect("classified snapshot")[0];
            assert!(semantics.committed_value.is_some(), "{op:?}");
            assert!(semantics.is_committed_runtime(), "{op:?}");
            assert!(semantics.has_implicit_exception_side_exit(op), "{op:?}");
        }
    }

    #[test]
    fn derived_get_prototype_retains_the_proved_leaf_path() {
        let mut view = one_instruction(
            Op::GetPrototype,
            vec![
                otter_bytecode::Operand::Register(2),
                otter_bytecode::Operand::Register(0),
            ],
        );
        view.derived_constructor = true;
        let semantics = classify_snapshot(&view).expect("classified snapshot")[0];
        assert_eq!(semantics.committed_value, None);
        assert_eq!(semantics.effects, EFFECTS_NONE);
        assert!(!semantics.has_implicit_exception_side_exit(Op::GetPrototype));
    }

    #[test]
    fn prepared_load_string_is_a_pure_snapshot_selected_leaf() {
        let mut view = one_instruction(
            Op::LoadString,
            vec![
                otter_bytecode::Operand::Register(0),
                otter_bytecode::Operand::ConstIndex(0),
            ],
        );
        view.string_constant_cells.insert(
            0,
            otter_vm::jit::JitStringConstantCell { cell_addr: 0x1234 },
        );
        let semantics = classify_snapshot(&view).expect("classified snapshot")[0];
        assert_eq!(semantics.committed_value, None);
        assert_eq!(semantics.effects, EFFECTS_NONE);
        assert!(!semantics.has_implicit_exception_side_exit(Op::LoadString));
    }

    #[test]
    fn template_fast_scalar_queries_are_not_replaced_by_unconditional_rust_calls() {
        for op in [Op::IsArray, Op::ArrayLength, Op::LoadLength] {
            let view = one_instruction(
                op,
                vec![
                    otter_bytecode::Operand::Register(2),
                    otter_bytecode::Operand::Register(0),
                ],
            );
            let semantics = classify_snapshot(&view).expect("classified snapshot")[0];
            assert_eq!(semantics.committed_value, None, "{op:?}");
            assert!(semantics.has_implicit_exception_side_exit(op), "{op:?}");
        }
    }
}
