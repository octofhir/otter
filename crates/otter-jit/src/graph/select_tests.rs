//! Diamonds into selects over hand-built graphs.
//!
//! # Contents
//! - A diamond whose arms only pass values, or read a field of an object
//!   the test does not read, becomes selects in the testing block.
//! - A field the test's own operand reaches stays below its edge, and so
//!   does an arm that may deopt.
//!
//! # Invariants
//! The graphs are built in the pass's input form: placed blocks, explicit
//! predecessors and controls, values defined before use.

use super::*;
use crate::graph::ir::Condition;
use otter_vm::object::FieldLocation;

fn place(graph: &mut Graph, block: BlockId, kind: Kind, inputs: &[NodeId], repr: Repr) -> NodeId {
    let node = graph.add_node(kind, inputs, repr);
    graph.node_mut(node).block = Some(block);
    graph.block_mut(block).body.push(node);
    node
}

fn control(graph: &mut Graph, block: BlockId, kind: Kind, inputs: &[NodeId]) {
    let node = graph.add_node(kind, inputs, Repr::None);
    graph.node_mut(node).block = Some(block);
    graph.block_mut(block).control = Some(node);
}

fn field(atom: u32) -> Kind {
    Kind::LoadOwnField {
        field: FieldLocation::inline(atom),
        atom,
    }
}

/// b0 tests `test` on `operands` into b1 / b2, which jump to b3, whose phi
/// merges `left` (from b1) and `right` (from b2) and which returns it.
/// `arm` places each arm's nodes, given the test's operands, and answers
/// the value it hands over.
fn diamond(
    test: BranchKind,
    operands: impl Fn(&mut Graph, BlockId) -> Vec<NodeId>,
    arm: impl Fn(&mut Graph, BlockId, bool, &[NodeId]) -> NodeId,
) -> (Graph, Vec<BlockId>, NodeId) {
    let mut graph = Graph::default();
    let b: Vec<BlockId> = (0..4).map(|_| graph.new_block()).collect();
    let inputs = operands(&mut graph, b[0]);
    control(
        &mut graph,
        b[0],
        Kind::Branch {
            kind: test,
            if_true: b[1],
            if_false: b[2],
        },
        &inputs,
    );
    let left = arm(&mut graph, b[1], true, &inputs);
    let right = arm(&mut graph, b[2], false, &inputs);
    control(&mut graph, b[1], Kind::Jump(b[3]), &[]);
    control(&mut graph, b[2], Kind::Jump(b[3]), &[]);
    let phi = graph.add_node(Kind::Phi, &[left, right], Repr::Tagged);
    graph.node_mut(phi).block = Some(b[3]);
    graph.block_mut(b[3]).phis.push(phi);
    control(&mut graph, b[3], Kind::Return, &[phi]);
    graph.block_mut(b[1]).predecessors = vec![b[0]];
    graph.block_mut(b[2]).predecessors = vec![b[0]];
    graph.block_mut(b[3]).predecessors = vec![b[1], b[2]];
    (graph, b, phi)
}

#[test]
fn a_diamond_that_picks_fields_of_another_object_becomes_a_select() {
    let (mut graph, b, phi) = diamond(
        BranchKind::Int32(Condition::Equal),
        |graph, entry| {
            let direction = place(graph, entry, Kind::InitialRegister(2), &[], Repr::Int32);
            let forward = graph.constant(Kind::ConstInt32(1));
            vec![direction, forward]
        },
        |graph, arm, taken, _| {
            let receiver = graph.constant(Kind::ConstTagged(0x10));
            place(
                graph,
                arm,
                field(if taken { 1 } else { 2 }),
                &[receiver],
                Repr::Tagged,
            )
        },
    );
    let mut layout = b.clone();
    assert_eq!(convert(&mut graph, &mut layout), 1);
    assert_eq!(layout, [b[0], b[3]]);
    let select = graph.node(phi);
    assert_eq!(
        select.kind,
        Kind::Select(BranchKind::Int32(Condition::Equal))
    );
    assert_eq!(select.block, Some(b[0]));
    // The condition's operands, then the true arm's value and the false one.
    let body = &graph.block(b[0]).body;
    assert_eq!(
        body.len(),
        4,
        "the test's operand, both loads and the select"
    );
    assert_eq!(select.inputs[2], body[1]);
    assert_eq!(select.inputs[3], body[2]);
    assert_eq!(*body.last().unwrap(), phi);
    let jump = graph.block(b[0]).control.unwrap();
    assert_eq!(graph.node(jump).kind, Kind::Jump(b[3]));
    assert_eq!(graph.block(b[3]).predecessors, [b[0]]);
    assert!(graph.block(b[3]).phis.is_empty());
}

#[test]
fn a_field_the_test_reads_stays_below_its_edge() {
    // `x === null ? 0 : x.f`: the test may be what proved `x` an object.
    let (mut graph, b, phi) = diamond(
        BranchKind::TaggedEqual,
        |graph, entry| {
            let x = place(graph, entry, Kind::InitialRegister(1), &[], Repr::Tagged);
            let null = graph.constant(Kind::ConstTagged(otter_vm::value::tag::VALUE_NULL));
            vec![x, null]
        },
        |graph, arm, taken, operands| {
            if taken {
                graph.constant(Kind::ConstInt32(0))
            } else {
                place(graph, arm, field(1), &[operands[0]], Repr::Tagged)
            }
        },
    );
    let mut layout = b.clone();
    assert_eq!(convert(&mut graph, &mut layout), 0);
    assert_eq!(layout, b);
    assert_eq!(graph.node(phi).kind, Kind::Phi);
}

#[test]
fn an_arm_that_may_deopt_stays_below_its_edge() {
    let (mut graph, b, phi) = diamond(
        BranchKind::Int32(Condition::Less),
        |graph, entry| {
            let i = place(graph, entry, Kind::InitialRegister(1), &[], Repr::Int32);
            let n = place(graph, entry, Kind::InitialRegister(2), &[], Repr::Int32);
            vec![i, n]
        },
        |graph, arm, taken, _| {
            let value = graph.constant(Kind::ConstTagged(0x20));
            if taken {
                place(
                    graph,
                    arm,
                    Kind::CheckedNumberToInt32,
                    &[value],
                    Repr::Int32,
                );
            }
            value
        },
    );
    let mut layout = b.clone();
    assert_eq!(convert(&mut graph, &mut layout), 0);
    assert_eq!(layout, b);
    assert_eq!(graph.node(phi).kind, Kind::Phi);
}
