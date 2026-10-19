//! Physical receiver proofs at source operations across effects and replacements.
//!
//! # Contents
//! - Object births remove only the first shape guard's receiver checks.
//! - Array births and successful length reads prove an immutable body kind.
//! - Replacing an SSA receiver loses its predecessor's kind proof.
//!
//! # Invariants
//! - Synthetic shape addresses are graph geometry, never executable pointers.
//! - Every length node retains its original eager source state.
//!
//! # See also
//! - `super::load_graph` inspects the production builder.
//! - `otter-runtime/tests/jit_empty_allocations.rs` covers real native allocation.

use super::*;

fn snapshot(ops: Vec<(Op, Vec<Operand>)>) -> JitCompileSnapshot {
    let kinds: Vec<_> = ops.iter().map(|(op, _)| *op).collect();
    let mut view = JitCompileSnapshot::without_feedback(
        90,
        2,
        6,
        ops.into_iter()
            .enumerate()
            .map(|(pc, (op, operands))| {
                JitTestInstruction::new(op, pc as u32, pc as u32 * 4, operands)
            })
            .collect(),
    );
    for (instruction, op) in view.instructions.iter_mut().zip(kinds) {
        if op == Op::LoadProperty {
            instruction.load_array_length = true;
            instruction.length_reads_arrays_only = true;
            instruction.property_attempted = true;
        }
        if op == Op::Call {
            instruction.call_attempted = true;
        }
    }
    view
}

fn length(destination: u16, receiver: u16) -> (Op, Vec<Operand>) {
    (
        Op::LoadProperty,
        vec![
            Operand::Register(destination),
            Operand::Register(receiver),
            Operand::ConstIndex(0),
        ],
    )
}

fn callback() -> (Op, Vec<Operand>) {
    (
        Op::Call,
        vec![
            Operand::Register(5),
            Operand::Register(1),
            Operand::ConstIndex(0),
        ],
    )
}

#[test]
fn array_births_and_successful_reads_prove_kind_across_a_callback() {
    for birth in [
        None,
        Some((
            Op::NewArray,
            vec![Operand::Register(0), Operand::ConstIndex(0)],
        )),
        Some((
            Op::NewArray,
            vec![
                Operand::Register(0),
                Operand::ConstIndex(1),
                Operand::Register(1),
            ],
        )),
        Some((Op::NewObject, vec![Operand::Register(0)])),
    ] {
        let array_birth = birth.as_ref().is_some_and(|(op, _)| *op == Op::NewArray);
        let mut ops: Vec<_> = birth.into_iter().collect();
        ops.extend([
            length(2, 0),
            callback(),
            length(3, 0),
            (Op::ReturnValue, vec![Operand::Register(3)]),
        ]);
        let view = snapshot(ops);
        let built = load_graph(&view);
        let loads: Vec<_> = built
            .graph
            .nodes
            .iter()
            .filter_map(|node| {
                let Kind::LoadArrayLength { array_proved } = node.kind else {
                    return None;
                };
                let state = built.graph.frame_state(node.eager.unwrap());
                assert_eq!(state.function_id, 90);
                assert_eq!(state.pc, node.pc);
                Some(array_proved)
            })
            .collect();
        assert_eq!(loads, [array_birth, true]);
        assert!(
            built
                .graph
                .nodes
                .iter()
                .any(|node| node.kind.properties().call)
        );
    }
}

#[test]
fn replacing_a_receiver_keeps_its_new_length_check() {
    let view = snapshot(vec![
        length(2, 0),
        callback(),
        (Op::LoadLocal, vec![Operand::Register(0), Operand::Imm32(5)]),
        length(3, 0),
        (Op::ReturnValue, vec![Operand::Register(3)]),
    ]);
    let built = load_graph(&view);
    let proofs: Vec<_> = built
        .graph
        .nodes
        .iter()
        .filter_map(|node| match node.kind {
            Kind::LoadArrayLength { array_proved } => Some(array_proved),
            _ => None,
        })
        .collect();
    assert_eq!(proofs, [false, false]);
}

#[test]
fn object_births_remove_receiver_checks_but_keep_live_shape_checks() {
    for (op, operands) in [
        (Op::NewObject, vec![Operand::Register(0)]),
        (
            Op::NewObjectLiteral,
            vec![
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        ),
        (Op::LoadGlobalThis, vec![Operand::Register(0)]),
        (
            Op::NewArray,
            vec![Operand::Register(0), Operand::ConstIndex(0)],
        ),
    ] {
        let mut view = snapshot(vec![
            (op, operands),
            callback(),
            length(2, 0),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        view.cage_base = 1;
        view.instructions[2].load_array_length = false;
        view.instructions[2].length_reads_arrays_only = false;
        view.property_programs
            .insert(8, vec![load_program(128, FieldLocation::inline(0), false)]);
        let built = load_graph(&view);
        let guards: Vec<_> = built
            .graph
            .nodes
            .iter()
            .filter_map(|node| match &node.kind {
                Kind::CheckShapes {
                    cell_proved,
                    shapes,
                    ..
                } => {
                    assert_eq!(shapes.as_slice(), [128]);
                    Some(*cell_proved)
                }
                _ => None,
            })
            .collect();
        // Every birth proves a heap cell; an array's map word then fails the
        // shape compare like any other cell's.
        assert_eq!(guards, [true]);
    }
}

#[test]
fn born_array_length_needs_no_receiver_feedback() {
    let mut view = snapshot(vec![
        (
            Op::NewArray,
            vec![Operand::Register(0), Operand::ConstIndex(0)],
        ),
        length(2, 0),
        (Op::ReturnValue, vec![Operand::Register(2)]),
    ]);
    view.instructions[1].property_attempted = false;
    view.instructions[1].length_reads_arrays_only = false;
    // Feedback from a different receiver cannot override the proven Array body.
    view.property_programs
        .insert(4, vec![load_program(128, FieldLocation::inline(0), false)]);
    let built = load_graph(&view);
    assert!(
        built
            .graph
            .nodes
            .iter()
            .any(|node| node.kind == Kind::LoadArrayLength { array_proved: true })
    );
    assert!(!built.graph.nodes.iter().any(|node| matches!(
        node.kind,
        Kind::CheckShapes { .. } | Kind::LoadOwnField { .. }
    )));
}
