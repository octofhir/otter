//! Redundancy elimination over diamonds and loops.
//!
//! # Contents
//! - A receiver's shape read again after a diamond whose arms store only
//!   fields is the first read; a shape-changing store in one arm keeps it.
//! - A loop that writes a field keeps the field's reload in its body; one
//!   that writes another field does not.
//! - Frame states and phis read the surviving node.
//! - A site-relative read of one inlined body never stands for another's.
//!
//! # Invariants
//! The graphs are built by hand in the pass's input form: placed blocks,
//! explicit predecessors and controls, and values defined before use.

use super::*;
use crate::graph::ir::{BranchKind, FrameState};

struct Builder {
    graph: Graph,
    blocks: Vec<BlockId>,
}

impl Builder {
    fn new(count: usize) -> Self {
        let mut graph = Graph::default();
        let blocks = (0..count).map(|_| graph.new_block()).collect();
        Self { graph, blocks }
    }

    fn node(&mut self, block: usize, kind: Kind, inputs: &[NodeId], repr: Repr) -> NodeId {
        let node = self.graph.add_node(kind, inputs, repr);
        let block = self.blocks[block];
        self.graph.node_mut(node).block = Some(block);
        self.graph.block_mut(block).body.push(node);
        node
    }

    fn control(&mut self, block: usize, kind: Kind, inputs: &[NodeId]) {
        let node = self.graph.add_node(kind, inputs, Repr::None);
        let block = self.blocks[block];
        self.graph.node_mut(node).block = Some(block);
        self.graph.block_mut(block).control = Some(node);
    }

    fn predecessors(&mut self, block: usize, predecessors: &[usize]) {
        let list = predecessors.iter().map(|&p| self.blocks[p]).collect();
        let block = self.blocks[block];
        self.graph.block_mut(block).predecessors = list;
    }

    fn jump(&mut self, from: usize, to: usize) {
        let target = self.blocks[to];
        self.control(from, Kind::Jump(target), &[]);
    }

    fn branch(&mut self, from: usize, condition: NodeId, yes: usize, no: usize) {
        let (if_true, if_false) = (self.blocks[yes], self.blocks[no]);
        self.control(
            from,
            Kind::Branch {
                kind: BranchKind::WordEqual(0),
                if_true,
                if_false,
            },
            &[condition],
        );
    }

    fn body(&self, block: usize) -> &[NodeId] {
        &self.graph.block(self.blocks[block]).body
    }
}

fn field_store(atom: u32) -> Kind {
    Kind::StoreOwnField {
        field: otter_vm::object::FieldLocation::inline(0),
        atom,
        owners: Default::default(),
    }
}

fn field_load(atom: u32) -> Kind {
    Kind::LoadOwnField {
        field: otter_vm::object::FieldLocation::inline(0),
        atom,
    }
}

/// b0: receiver, shape → b1 | b2 (each stores `field_kind`) → b3: shape.
fn diamond(arm_store: Kind) -> (Builder, NodeId, NodeId, NodeId) {
    let mut b = Builder::new(4);
    let receiver = b.node(0, Kind::InitialRegister(0), &[], Repr::Tagged);
    let shape = b.node(0, Kind::LoadReceiverShape, &[receiver], Repr::Word);
    b.branch(0, shape, 1, 2);
    let value = b.node(1, Kind::InitialRegister(1), &[], Repr::Tagged);
    b.node(1, field_store(7), &[receiver, value], Repr::None);
    b.jump(1, 3);
    b.node(2, arm_store, &[receiver, value], Repr::None);
    b.jump(2, 3);
    let again = b.node(3, Kind::LoadReceiverShape, &[receiver], Repr::Word);
    b.control(3, Kind::Return, &[again]);
    b.predecessors(1, &[0]);
    b.predecessors(2, &[0]);
    b.predecessors(3, &[1, 2]);
    (b, shape, again, receiver)
}

#[test]
fn a_shape_read_after_field_only_arms_is_the_dominating_read() {
    let (mut b, shape, again, _) = diamond(field_store(8));
    let state = b.graph.add_frame_state(FrameState {
        pc: 0,
        byte_pc: 0,
        function_id: 0,
        register_count: 1,
        registers: vec![(0, again)],
        caller: None,
    });
    let layout = b.blocks.clone();
    assert_eq!(eliminate(&mut b.graph, &layout), 1);
    assert!(b.body(3).is_empty(), "the reload left its block");
    let control = b.graph.block(b.blocks[3]).control.unwrap();
    assert_eq!(b.graph.node(control).inputs.as_slice(), [shape]);
    assert_eq!(b.graph.frame_state(state).registers, vec![(0, shape)]);
}

#[test]
fn a_shape_changing_arm_keeps_the_reload() {
    let store = Kind::StoreNamedProperty {
        byte_pc: 0,
        owners: Default::default(),
        receiver_shape: None,
        fresh: false,
    };
    let (mut b, _, again, _) = diamond(store);
    let layout = b.blocks.clone();
    assert_eq!(eliminate(&mut b.graph, &layout), 0);
    assert_eq!(b.body(3), [again]);
}

/// b0: receiver, load → b1 loop header: load, store in body → back edge.
fn field_loop(stored_atom: u32) -> (Builder, NodeId, NodeId) {
    let mut b = Builder::new(3);
    let receiver = b.node(0, Kind::InitialRegister(0), &[], Repr::Tagged);
    let first = b.node(0, field_load(7), &[receiver], Repr::Tagged);
    b.jump(0, 1);
    let inner = b.node(1, field_load(7), &[receiver], Repr::Tagged);
    b.node(1, field_store(stored_atom), &[receiver, inner], Repr::None);
    b.branch(1, first, 1, 2);
    b.control(2, Kind::Return, &[first]);
    b.predecessors(1, &[0, 1]);
    b.predecessors(2, &[1]);
    let header = b.blocks[1];
    b.graph.block_mut(header).is_loop = true;
    let control = b.graph.block(header).control.unwrap();
    b.graph.node_mut(control).kind = Kind::Branch {
        kind: BranchKind::WordEqual(0),
        if_true: header,
        if_false: b.blocks[2],
    };
    (b, first, inner)
}

#[test]
fn a_loop_that_writes_the_field_keeps_its_reload() {
    let (mut b, _, inner) = field_loop(7);
    let layout = b.blocks.clone();
    assert_eq!(eliminate(&mut b.graph, &layout), 0);
    assert_eq!(b.body(1)[0], inner);
}

#[test]
fn a_loop_that_writes_another_field_reads_the_entry_load() {
    let (mut b, first, inner) = field_loop(8);
    let layout = b.blocks.clone();
    assert_eq!(eliminate(&mut b.graph, &layout), 1);
    assert!(!b.body(1).contains(&inner));
    let store = b.body(1)[0];
    assert_eq!(b.graph.node(store).inputs[1], first);
}

#[test]
fn a_site_of_one_inlined_body_never_stands_for_another_bodys() {
    // Two inlined methods each read the global at their own byte pc 5.
    let mut b = Builder::new(1);
    let first = b.node(0, Kind::LoadGlobalBinding(5), &[], Repr::Tagged);
    let second = b.node(0, Kind::LoadGlobalBinding(5), &[], Repr::Tagged);
    let same_body = b.node(0, Kind::LoadGlobalBinding(5), &[], Repr::Tagged);
    b.graph.node_mut(first).origin = 1;
    b.graph.node_mut(second).origin = 2;
    b.graph.node_mut(same_body).origin = 1;
    b.control(0, Kind::Return, &[second]);
    let layout = b.blocks.clone();
    assert_eq!(eliminate(&mut b.graph, &layout), 1);
    assert_eq!(b.body(0), [first, second]);
}
