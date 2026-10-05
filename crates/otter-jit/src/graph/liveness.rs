//! Control-flow liveness of SSA values at every node, including values read
//! by eager and lazy deopt frame states.
//!
//! # Contents
//! - [`Liveness`] — backward dataflow over blocks, with phi inputs read on
//!   their incoming edges and frame states read at their owning nodes.
//!
//! # Invariants
//! - A value is live after a point only when some path from the point reads
//!   it before it is defined again. Every value is defined on every path
//!   into a point where it is live, under either entry of the graph (the
//!   function entry or the OSR entry), so its location holds the value
//!   itself and the collector may relocate it.
//! - A phi's input is read at the end of the predecessor it comes from; a
//!   frame state's values are read by its node.
//!
//! # See also
//! - [`super::regalloc`] — uses these sets to preserve only registers whose
//!   values are defined on every path into a collecting slow path.

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use super::ir::{BlockId, Graph, Kind, NodeId, Repr};

/// SSA values live after each node.
#[derive(Debug, Default)]
pub(crate) struct Liveness {
    /// Dense index of block values and rematerializable constants.
    index: FxHashMap<NodeId, u32>,
    /// Per node, the live-after set as a bit set over `index`.
    live_after: FxHashMap<NodeId, Box<[u64]>>,
}

impl Liveness {
    pub(crate) fn compute(graph: &Graph, layout: &[BlockId]) -> Self {
        let mut index = FxHashMap::default();
        for (node, data) in graph.nodes.iter().enumerate() {
            if data.kind.is_constant() {
                let next = index.len() as u32;
                index.insert(NodeId(node as u32), next);
            }
        }
        for &block in layout {
            let data = graph.block(block);
            for &node in data.phis.iter().chain(&data.body) {
                let kind = &graph.node(node).kind;
                if graph.node(node).repr != Repr::None && !kind.is_constant() {
                    let next = index.len() as u32;
                    index.entry(node).or_insert(next);
                }
            }
        }
        let words = index.len().div_ceil(64);
        let bit = |set: &mut [u64], value: NodeId| {
            if let Some(&slot) = index.get(&value) {
                set[slot as usize / 64] |= 1 << (slot % 64);
            }
        };
        let clear = |set: &mut [u64], value: NodeId| {
            if let Some(&slot) = index.get(&value) {
                set[slot as usize / 64] &= !(1 << (slot % 64));
            }
        };
        let reads = |node: NodeId| -> SmallVec<[NodeId; 16]> {
            let data = graph.node(node);
            let mut values: SmallVec<[NodeId; 16]> = data.inputs.iter().copied().collect();
            for state in data.eager.iter().chain(data.lazy.iter()) {
                values.extend(
                    graph
                        .state_values(*state)
                        .into_iter()
                        .filter(|&value| value != node),
                );
            }
            values
        };
        let successors = |block: BlockId| -> SmallVec<[BlockId; 2]> {
            match graph
                .block(block)
                .control
                .map(|control| &graph.node(control).kind)
            {
                Some(Kind::Jump(target) | Kind::JumpLoop(target)) => smallvec::smallvec![*target],
                Some(Kind::Branch {
                    if_true, if_false, ..
                }) => smallvec::smallvec![*if_true, *if_false],
                _ => SmallVec::new(),
            }
        };
        // Upward-exposed reads and definitions of each block; phis define
        // at the block's start and read nothing in it.
        let mut generated: FxHashMap<BlockId, Box<[u64]>> = FxHashMap::default();
        let mut killed: FxHashMap<BlockId, Box<[u64]>> = FxHashMap::default();
        for &block in layout {
            let data = graph.block(block);
            let mut live = vec![0u64; words].into_boxed_slice();
            let mut kill = vec![0u64; words].into_boxed_slice();
            for &node in data.control.iter().rev().chain(data.body.iter().rev()) {
                clear(&mut live, node);
                bit(&mut kill, node);
                for value in reads(node) {
                    bit(&mut live, value);
                }
            }
            for &phi in &data.phis {
                clear(&mut live, phi);
                bit(&mut kill, phi);
            }
            generated.insert(block, live);
            killed.insert(block, kill);
        }
        let live_out_of = |block: BlockId, live_in: &FxHashMap<BlockId, Box<[u64]>>| {
            let mut out = vec![0u64; words].into_boxed_slice();
            for successor in successors(block) {
                if let Some(entry) = live_in.get(&successor) {
                    for (word, &incoming) in out.iter_mut().zip(entry.iter()) {
                        *word |= incoming;
                    }
                }
                let target = graph.block(successor);
                if let Some(position) = target.predecessors.iter().position(|&p| p == block) {
                    for &phi in &target.phis {
                        if let Some(&input) = graph.node(phi).inputs.get(position) {
                            bit(&mut out, input);
                        }
                    }
                }
            }
            out
        };
        let mut live_in: FxHashMap<BlockId, Box<[u64]>> = layout
            .iter()
            .map(|&block| (block, vec![0u64; words].into_boxed_slice()))
            .collect();
        loop {
            let mut changed = false;
            for &block in layout.iter().rev() {
                let out = live_out_of(block, &live_in);
                let kill = &killed[&block];
                let generate = &generated[&block];
                let entry = live_in.get_mut(&block).expect("a block of the layout");
                for (word, ((&outgoing, &kill), &generate)) in entry
                    .iter_mut()
                    .zip(out.iter().zip(kill.iter()).zip(generate.iter()))
                {
                    let next = generate | (outgoing & !kill);
                    if next != *word {
                        *word = next;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        // The live-after set at every node, regardless of representation.
        let mut live_after = FxHashMap::default();
        for &block in layout {
            let data = graph.block(block);
            let mut live = live_out_of(block, &live_in);
            for &node in data.control.iter().rev().chain(data.body.iter().rev()) {
                live_after.insert(node, live.clone());
                clear(&mut live, node);
                for value in reads(node) {
                    bit(&mut live, value);
                }
            }
        }
        Self { index, live_after }
    }

    /// Whether `value` is live after `node` on some successor path.
    pub(crate) fn is_live_after(&self, node: NodeId, value: NodeId) -> bool {
        let (Some(&slot), Some(set)) = (self.index.get(&value), self.live_after.get(&node)) else {
            return false;
        };
        set[slot as usize / 64] & (1 << (slot % 64)) != 0
    }
}
