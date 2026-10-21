//! Constant folding over hand-built graphs.
//!
//! # Contents
//! - A compare of constants is its boolean, and the branch on it a jump that
//!   drops the dead arm and the merge phi input it brought.
//! - A loop whose only back edge sits in a dead arm stops being a loop.
//! - A branch below one edge of a dominating branch on the same condition
//!   takes that edge; one below a merge of both edges stays.
//!
//! # Invariants
//! The graphs are built in the pass's input form: placed blocks, explicit
//! predecessors and controls, values defined before use.

use super::*;
use crate::graph::ir::Repr;

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

#[test]
fn a_constant_compare_branch_jumps_and_drops_its_dead_arm() {
    let mut graph = Graph::default();
    let b: Vec<BlockId> = (0..4).map(|_| graph.new_block()).collect();
    let zero = graph.constant(Kind::ConstInt32(0));
    let compare = place(
        &mut graph,
        b[0],
        Kind::Int32Compare(Condition::NotEqual),
        &[zero, zero],
        Repr::Tagged,
    );
    control(
        &mut graph,
        b[0],
        Kind::Branch {
            kind: BranchKind::Truthy,
            if_true: b[1],
            if_false: b[2],
        },
        &[compare],
    );
    let one = graph.constant(Kind::ConstInt32(1));
    let two = graph.constant(Kind::ConstInt32(2));
    control(&mut graph, b[1], Kind::Jump(b[3]), &[]);
    control(&mut graph, b[2], Kind::Jump(b[3]), &[]);
    let phi = graph.add_node(Kind::Phi, &[one, two], Repr::Int32);
    graph.node_mut(phi).block = Some(b[3]);
    graph.block_mut(b[3]).phis.push(phi);
    control(&mut graph, b[3], Kind::Return, &[phi]);
    graph.block_mut(b[1]).predecessors = vec![b[0]];
    graph.block_mut(b[2]).predecessors = vec![b[0]];
    graph.block_mut(b[3]).predecessors = vec![b[1], b[2]];
    let mut layout = b.clone();
    let mut headers = Vec::new();
    assert_eq!(fold(&mut graph, &mut layout, &mut headers), 2);
    assert_eq!(layout, [b[0], b[2], b[3]], "the true arm is dead");
    let jump = graph.block(b[0]).control.unwrap();
    assert_eq!(graph.node(jump).kind, Kind::Jump(b[2]));
    assert_eq!(graph.block(b[3]).predecessors, [b[2]]);
    // The merge's phi had one input left and went; its user reads it.
    assert!(graph.block(b[3]).phis.is_empty());
    let ret = graph.block(b[3]).control.unwrap();
    assert_eq!(graph.node(ret).inputs.as_slice(), [two]);
}

#[test]
fn a_loop_whose_back_edge_is_dead_is_no_loop() {
    // b0 -> b1 (header) -> b2 (latch: JumpLoop b1) | b3 (exit) on `1 == 2`.
    let mut graph = Graph::default();
    let b: Vec<BlockId> = (0..4).map(|_| graph.new_block()).collect();
    let one = graph.constant(Kind::ConstInt32(1));
    let two = graph.constant(Kind::ConstInt32(2));
    control(&mut graph, b[0], Kind::Jump(b[1]), &[]);
    control(
        &mut graph,
        b[1],
        Kind::Branch {
            kind: BranchKind::Int32(Condition::Equal),
            if_true: b[2],
            if_false: b[3],
        },
        &[one, two],
    );
    control(&mut graph, b[2], Kind::JumpLoop(b[1]), &[]);
    control(&mut graph, b[3], Kind::Return, &[one]);
    graph.block_mut(b[1]).predecessors = vec![b[0], b[2]];
    graph.block_mut(b[1]).is_loop = true;
    graph.block_mut(b[2]).predecessors = vec![b[1]];
    graph.block_mut(b[3]).predecessors = vec![b[1]];
    let state = graph.add_frame_state(crate::graph::ir::FrameState {
        pc: 0,
        byte_pc: 0,
        function_id: 0,
        register_count: 0,
        registers: Vec::new(),
        caller: None,
    });
    let mut headers = vec![LoopHeader {
        block: b[1],
        state,
        phis: Vec::new(),
        speculate: true,
        hoist: true,
        exit_reasons: Default::default(),
    }];
    let mut layout = b.clone();
    assert_eq!(fold(&mut graph, &mut layout, &mut headers), 1);
    assert_eq!(layout, [b[0], b[1], b[3]]);
    assert!(headers.is_empty());
    assert!(!graph.block(b[1]).is_loop);
    assert_eq!(graph.block(b[1]).predecessors, [b[0]]);
}

#[test]
fn a_branch_below_one_edge_of_the_same_test_takes_that_edge() {
    // b0 tests `x == 1`; b1 (its true edge) and b2 (its false edge) test it
    // again into b3 / b4, which merge in b5, which tests it once more.
    let mut graph = Graph::default();
    let b: Vec<BlockId> = (0..8).map(|_| graph.new_block()).collect();
    let x = place(&mut graph, b[0], Kind::InitialRegister(1), &[], Repr::Int32);
    let one = graph.constant(Kind::ConstInt32(1));
    let test = |if_true, if_false| Kind::Branch {
        kind: BranchKind::Int32(Condition::Equal),
        if_true,
        if_false,
    };
    control(&mut graph, b[0], test(b[1], b[2]), &[x, one]);
    control(&mut graph, b[1], test(b[3], b[4]), &[x, one]);
    control(&mut graph, b[2], test(b[3], b[4]), &[x, one]);
    control(&mut graph, b[3], Kind::Jump(b[5]), &[]);
    control(&mut graph, b[4], Kind::Jump(b[5]), &[]);
    control(&mut graph, b[5], test(b[6], b[7]), &[x, one]);
    control(&mut graph, b[6], Kind::Return, &[x]);
    control(&mut graph, b[7], Kind::Return, &[one]);
    graph.block_mut(b[1]).predecessors = vec![b[0]];
    graph.block_mut(b[2]).predecessors = vec![b[0]];
    graph.block_mut(b[3]).predecessors = vec![b[1], b[2]];
    graph.block_mut(b[4]).predecessors = vec![b[1], b[2]];
    graph.block_mut(b[5]).predecessors = vec![b[3], b[4]];
    graph.block_mut(b[6]).predecessors = vec![b[5]];
    graph.block_mut(b[7]).predecessors = vec![b[5]];
    let mut layout = b.clone();
    let mut headers = Vec::new();
    assert_eq!(fold(&mut graph, &mut layout, &mut headers), 2);
    let jump = |graph: &Graph, block: BlockId| {
        graph.node(graph.block(block).control.unwrap()).kind.clone()
    };
    assert_eq!(jump(&graph, b[1]), Kind::Jump(b[3]));
    assert_eq!(jump(&graph, b[2]), Kind::Jump(b[4]));
    assert_eq!(graph.block(b[3]).predecessors, [b[1]]);
    assert_eq!(graph.block(b[4]).predecessors, [b[2]]);
    // Both edges of the first test reach the merge: its test stays.
    assert!(matches!(jump(&graph, b[5]), Kind::Branch { .. }));
    assert_eq!(layout, b);
}
