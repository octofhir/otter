//! Layout ownership, scalar element representation and proof reuse tests.
//!
//! # Contents
//! - Repeated accesses at distinct source PCs share only equivalent layouts.
//! - Both target allocators accept scalar index and payload contracts.
//!
//! # Invariants
//! - Diagnostics never supply semantic identity to value numbering.
//! - Signedness and storage width remain part of every reusable proof.

use super::*;
use otter_bytecode::{Op, Operand};
use otter_vm::{JitElementAccess, JitElementBase, JitElementRepr, jit::JitTestInstruction};

fn view(first: JitElementRepr, second: JitElementRepr) -> JitCompileSnapshot {
    let mut view = JitCompileSnapshot::without_feedback(
        193,
        2,
        4,
        vec![
            JitTestInstruction::new(
                Op::LoadElement,
                0,
                0,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            JitTestInstruction::new(
                Op::LoadElement,
                1,
                8,
                vec![
                    Operand::Register(3),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            JitTestInstruction::new(
                Op::Add,
                2,
                16,
                vec![
                    Operand::Register(2),
                    Operand::Register(2),
                    Operand::Register(3),
                ],
            ),
            JitTestInstruction::new(Op::ReturnValue, 3, 24, vec![Operand::Register(2)]),
        ],
    );
    view.cage_base = 0x1000;
    for (pc, element) in [(0, first), (8, second)] {
        view.element_accesses.insert(
            pc,
            JitElementAccess {
                type_tag: 1,
                base: JitElementBase::InBody { byte: 8 },
                element,
                ..Default::default()
            },
        );
    }
    view
}

#[test]
fn layout_equivalence_replaces_source_pc_identity() {
    for (second, expected) in [(JitElementRepr::Int32, 1), (JitElementRepr::Uint32, 2)] {
        let hir = NumericFunction::build(&view(JitElementRepr::Int32, second)).unwrap();
        for target in [TargetSpec::aarch64(), TargetSpec::x86_64()] {
            let sequence =
                select_with_loop_entries(&target, &hir, &hir.plan_loop_entries(), None).unwrap();
            let (sequence, _) = sequence.optimize(&target).unwrap();
            assert_eq!(
                sequence
                    .instructions()
                    .iter()
                    .filter(|instruction| matches!(
                        instruction.opcode,
                        MachineOpcode::ElementView { .. }
                    ))
                    .count(),
                expected
            );
            sequence
                .allocate(&target)
                .unwrap_or_else(|error| panic!("{:?}: {error:?}", target.architecture()));
        }
    }
}

#[test]
fn element_payload_signedness_survives_selection_and_allocation() {
    for (element, expected) in [
        (JitElementRepr::Int8, MachineRepresentation::Int32),
        (JitElementRepr::Uint8, MachineRepresentation::Int32),
        (JitElementRepr::Int16, MachineRepresentation::Int32),
        (JitElementRepr::Uint16, MachineRepresentation::Int32),
        (JitElementRepr::Int32, MachineRepresentation::Int32),
        (JitElementRepr::Uint32, MachineRepresentation::Uint32),
        (JitElementRepr::Float32, MachineRepresentation::Float64),
    ] {
        let hir = NumericFunction::build(&view(element, element)).unwrap();
        for target in [TargetSpec::aarch64(), TargetSpec::x86_64()] {
            let sequence =
                select_with_loop_entries(&target, &hir, &hir.plan_loop_entries(), None).unwrap();
            for instruction in sequence.instructions() {
                if let MachineOpcode::ElementValueLoad { access, .. } = instruction.opcode {
                    assert_eq!(access.element, element);
                    assert_eq!(
                        sequence.representations()[instruction.operands[2].value.0 as usize],
                        expected
                    );
                }
            }
            sequence
                .allocate(&target)
                .unwrap_or_else(|error| panic!("{:?}: {error:?}", target.architecture()));
        }
    }
}
