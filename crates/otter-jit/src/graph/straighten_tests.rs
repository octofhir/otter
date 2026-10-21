//! Straight-line merging over hand-built graphs.
//!
//! # Contents
//! - Branch edge blocks absorb the blocks they alone enter, and the diamond
//!   they form then becomes a select.
//! - A merge, a loop header and a kept entry stay their own blocks, and the
//!   blocks an absorbed control reaches name the merged block in place.
//!
//! # Invariants
//! The graphs are built in the pass's input form: placed blocks, explicit
//! predecessors and controls, values defined before use.

use super::*;
use crate::graph::ir::{BranchKind, Condition, NodeId, Repr};
use otter_vm::object::FieldLocation;

fn place(graph: &mut Graph, block: BlockId, kind: Kind, inputs: &[NodeId], repr: Repr) -> NodeId {
    let node = graph.add_node(kind, inputs, repr);
    graph.node_mut(node).block = Some(block);
    graph.block_mut(block).body.push(node);
    node
}

fn control(graph: &mut Graph, block: BlockId, kind: Kind, inputs: &[NodeId]) -> NodeId {
    let node = graph.add_node(kind, inputs, Repr::None);
    graph.node_mut(node).block = Some(block);
    graph.block_mut(block).control = Some(node);
    node
}

fn field(atom: u32) -> Kind {
    Kind::LoadOwnField {
        field: FieldLocation::inline(atom),
        atom,
    }
}

/// `this.direction == 1 ? this.v1 : this.v2` as the builder leaves it: b0
/// tests a field of `this` into the edge blocks b1 / b2, which jump to the
/// bytecode blocks b3 / b4 that read the other fields and jump to the merge
/// b5.
fn guarded_diamond() -> (Graph, Vec<BlockId>, NodeId) {
    let mut graph = Graph::default();
    let b: Vec<BlockId> = (0..6).map(|_| graph.new_block()).collect();
    let this = place(
        &mut graph,
        b[0],
        Kind::InitialRegister(0),
        &[],
        Repr::Tagged,
    );
    let direction = place(&mut graph, b[0], field(3), &[this], Repr::Int32);
    let forward = graph.constant(Kind::ConstInt32(1));
    control(
        &mut graph,
        b[0],
        Kind::Branch {
            kind: BranchKind::Int32(Condition::Equal),
            if_true: b[1],
            if_false: b[2],
        },
        &[direction, forward],
    );
    control(&mut graph, b[1], Kind::Jump(b[3]), &[]);
    control(&mut graph, b[2], Kind::Jump(b[4]), &[]);
    let v1 = place(&mut graph, b[3], field(1), &[this], Repr::Tagged);
    control(&mut graph, b[3], Kind::Jump(b[5]), &[]);
    let v2 = place(&mut graph, b[4], field(2), &[this], Repr::Tagged);
    control(&mut graph, b[4], Kind::Jump(b[5]), &[]);
    let phi = graph.add_node(Kind::Phi, &[v1, v2], Repr::Tagged);
    graph.node_mut(phi).block = Some(b[5]);
    graph.block_mut(b[5]).phis.push(phi);
    control(&mut graph, b[5], Kind::Return, &[phi]);
    graph.block_mut(b[1]).predecessors = vec![b[0]];
    graph.block_mut(b[2]).predecessors = vec![b[0]];
    graph.block_mut(b[3]).predecessors = vec![b[1]];
    graph.block_mut(b[4]).predecessors = vec![b[2]];
    graph.block_mut(b[5]).predecessors = vec![b[3], b[4]];
    (graph, b, phi)
}

#[test]
fn edge_blocks_absorb_their_arms_and_the_diamond_becomes_a_select() {
    let (mut graph, b, phi) = guarded_diamond();
    let mut layout = b.clone();
    assert_eq!(merge(&mut graph, &mut layout, &[]), 2);
    assert_eq!(layout, [b[0], b[1], b[2], b[5]]);
    assert_eq!(graph.block(b[5]).predecessors, [b[1], b[2]]);
    assert_eq!(graph.block(b[1]).body.len(), 1);
    let jump = graph.block(b[1]).control.unwrap();
    assert_eq!(graph.node(jump).kind, Kind::Jump(b[5]));
    assert_eq!(graph.node(jump).block, Some(b[1]));
    // The test reads a field of `this`, which proves nothing about `this`:
    // both field reads run before it and a select picks one.
    assert_eq!(super::super::select::convert(&mut graph, &mut layout), 1);
    assert_eq!(
        graph.node(phi).kind,
        Kind::Select(BranchKind::Int32(Condition::Equal))
    );
    // The testing block now jumps to the merge, its only way in.
    assert_eq!(merge(&mut graph, &mut layout, &[]), 1);
    assert_eq!(layout, [b[0]]);
    assert_eq!(
        graph.node(graph.block(b[0]).control.unwrap()).kind,
        Kind::Return
    );
}

#[test]
fn merges_loop_headers_and_kept_entries_stay_and_successors_follow() {
    // b0 -> b1 (kept) -> b2 -> b3 (loop header) -> b4, whose back edge
    // returns to b3 and whose exit b5 a second chain b6 -> b7 also enters.
    let mut graph = Graph::default();
    let b: Vec<BlockId> = (0..8).map(|_| graph.new_block()).collect();
    control(&mut graph, b[0], Kind::Jump(b[1]), &[]);
    control(&mut graph, b[1], Kind::Jump(b[2]), &[]);
    control(&mut graph, b[2], Kind::Jump(b[3]), &[]);
    graph.block_mut(b[3]).is_loop = true;
    control(&mut graph, b[3], Kind::Jump(b[4]), &[]);
    let flag = place(&mut graph, b[4], Kind::InitialRegister(1), &[], Repr::Int32);
    let zero = graph.constant(Kind::ConstInt32(0));
    control(
        &mut graph,
        b[4],
        Kind::Branch {
            kind: BranchKind::Int32(Condition::Equal),
            if_true: b[3],
            if_false: b[5],
        },
        &[flag, zero],
    );
    control(&mut graph, b[6], Kind::Jump(b[7]), &[]);
    let late = place(&mut graph, b[7], Kind::InitialRegister(2), &[], Repr::Int32);
    control(&mut graph, b[7], Kind::Jump(b[5]), &[]);
    let phi = graph.add_node(Kind::Phi, &[late, flag], Repr::Int32);
    graph.node_mut(phi).block = Some(b[5]);
    graph.block_mut(b[5]).phis.push(phi);
    control(&mut graph, b[5], Kind::Return, &[phi]);
    graph.block_mut(b[1]).predecessors = vec![b[0]];
    graph.block_mut(b[2]).predecessors = vec![b[1]];
    graph.block_mut(b[3]).predecessors = vec![b[2], b[4]];
    graph.block_mut(b[4]).predecessors = vec![b[3]];
    graph.block_mut(b[5]).predecessors = vec![b[7], b[4]];
    graph.block_mut(b[7]).predecessors = vec![b[6]];
    let mut layout = b.clone();
    // b1 is kept and absorbs b2; b3 heads a loop, so neither it nor its body
    // merges; b5 has two ways in; b6 absorbs b7.
    assert_eq!(merge(&mut graph, &mut layout, &[b[1]]), 2);
    assert_eq!(layout, [b[0], b[1], b[3], b[4], b[5], b[6]]);
    assert_eq!(
        graph.node(graph.block(b[1]).control.unwrap()).kind,
        Kind::Jump(b[3])
    );
    assert_eq!(graph.block(b[3]).predecessors, [b[1], b[4]]);
    // The merge names b6 where it named b7, so its phi keeps its edges.
    assert_eq!(graph.block(b[5]).predecessors, [b[6], b[4]]);
    assert_eq!(graph.node(phi).inputs.as_slice(), [late, flag]);
    assert_eq!(graph.node(late).block, Some(b[6]));
}
