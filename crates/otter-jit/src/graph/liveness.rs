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

use smallvec::SmallVec;

use super::ir::{BlockId, Graph, Kind, NodeId, Repr, StateValues};

/// A node or value outside the sets.
const NONE: u32 = u32::MAX;

/// SSA values live after each node.
#[derive(Debug, Default)]
pub(crate) struct Liveness {
    /// Dense bit index of block values and rematerializable constants, by
    /// node id; [`NONE`] for a node that defines no tracked value.
    index: Vec<u32>,
    /// Row of each placed node's live-after set in `sets`, by node id;
    /// [`NONE`] for a node outside the layout's bodies and controls.
    rows: Vec<u32>,
    /// Words of one set.
    words: usize,
    /// The live-after sets, `words` per row.
    sets: Vec<u64>,
}

impl Liveness {
    #[cfg(test)]
    pub(crate) fn compute(graph: &Graph, layout: &[BlockId]) -> Self {
        Self::compute_with(graph, layout, &graph.all_state_values())
    }

    /// [`Self::compute`] over frame-state values already flattened.
    pub(crate) fn compute_with(graph: &Graph, layout: &[BlockId], states: &StateValues) -> Self {
        let mut index = vec![NONE; graph.nodes.len()];
        let mut tracked = 0u32;
        for (node, data) in graph.nodes.iter().enumerate() {
            if data.kind.is_constant() {
                index[node] = tracked;
                tracked += 1;
            }
        }
        for &block in layout {
            let data = graph.block(block);
            for &node in data.phis.iter().chain(&data.body) {
                let kind = &graph.node(node).kind;
                if graph.node(node).repr != Repr::None
                    && !kind.is_constant()
                    && index[node.0 as usize] == NONE
                {
                    index[node.0 as usize] = tracked;
                    tracked += 1;
                }
            }
        }
        let words = (tracked as usize).div_ceil(64);
        let bit = |set: &mut [u64], value: NodeId| {
            let slot = index[value.0 as usize];
            if slot != NONE {
                set[slot as usize / 64] |= 1 << (slot % 64);
            }
        };
        let clear = |set: &mut [u64], value: NodeId| {
            let slot = index[value.0 as usize];
            if slot != NONE {
                set[slot as usize / 64] &= !(1 << (slot % 64));
            }
        };
        // Everything `node` reads: its inputs and its frame states' values.
        let read = |set: &mut [u64], node: NodeId| {
            let data = graph.node(node);
            for &input in &data.inputs {
                bit(set, input);
            }
            for &state in data.eager.iter().chain(data.lazy.iter()) {
                for &value in states.of(state) {
                    if value != node {
                        bit(set, value);
                    }
                }
            }
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
        let row_of = |block: BlockId| block.0 as usize * words;
        // Upward-exposed reads and definitions of each block; phis define
        // at the block's start and read nothing in it.
        let blocks = graph.blocks.len();
        let mut generated = vec![0u64; blocks * words];
        let mut killed = vec![0u64; blocks * words];
        for &block in layout {
            let data = graph.block(block);
            let row = row_of(block);
            let live = &mut generated[row..row + words];
            let kill = &mut killed[row..row + words];
            for &node in data.control.iter().rev().chain(data.body.iter().rev()) {
                clear(live, node);
                bit(kill, node);
                read(live, node);
            }
            for &phi in &data.phis {
                clear(live, phi);
                bit(kill, phi);
            }
        }
        let live_out_of = |block: BlockId, live_in: &[u64], out: &mut [u64]| {
            out.fill(0);
            for successor in successors(block) {
                let row = row_of(successor);
                for (word, &incoming) in out.iter_mut().zip(&live_in[row..row + words]) {
                    *word |= incoming;
                }
                let target = graph.block(successor);
                if let Some(position) = target.predecessors.iter().position(|&p| p == block) {
                    for &phi in &target.phis {
                        if let Some(&input) = graph.node(phi).inputs.get(position) {
                            bit(out, input);
                        }
                    }
                }
            }
        };
        let mut live_in = vec![0u64; blocks * words];
        let mut out = vec![0u64; words];
        loop {
            let mut changed = false;
            for &block in layout.iter().rev() {
                live_out_of(block, &live_in, &mut out);
                let row = row_of(block);
                for (offset, &outgoing) in out.iter().enumerate() {
                    let next = generated[row + offset] | (outgoing & !killed[row + offset]);
                    if next != live_in[row + offset] {
                        live_in[row + offset] = next;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        // The live-after set at every node, regardless of representation.
        let mut rows = vec![NONE; graph.nodes.len()];
        let placed: usize = layout
            .iter()
            .map(|&block| {
                let data = graph.block(block);
                data.body.len() + usize::from(data.control.is_some())
            })
            .sum();
        let mut sets = vec![0u64; placed * words];
        let mut next_row = 0u32;
        for &block in layout {
            let data = graph.block(block);
            live_out_of(block, &live_in, &mut out);
            for &node in data.control.iter().rev().chain(data.body.iter().rev()) {
                let start = next_row as usize * words;
                sets[start..start + words].copy_from_slice(&out);
                rows[node.0 as usize] = next_row;
                next_row += 1;
                clear(&mut out, node);
                read(&mut out, node);
            }
        }
        Self {
            index,
            rows,
            words,
            sets,
        }
    }

    /// Whether `value` is live after `node` on some successor path.
    pub(crate) fn is_live_after(&self, node: NodeId, value: NodeId) -> bool {
        let (Some(&slot), Some(&row)) = (
            self.index.get(value.0 as usize),
            self.rows.get(node.0 as usize),
        ) else {
            return false;
        };
        if slot == NONE || row == NONE {
            return false;
        }
        self.sets[row as usize * self.words + slot as usize / 64] & (1 << (slot % 64)) != 0
    }
}
