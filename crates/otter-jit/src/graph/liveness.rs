//! Control-flow liveness of SSA values at block boundaries.
//!
//! # Contents
//! - [`Liveness`] — backward dataflow over blocks, retaining one live-out
//!   set per block and answering exact queries using local read positions.
//! - [`positions`] — the layout and read table also used by the allocator.
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
//! - [`super::regalloc`] — derives exact liveness at a node to preserve
//!   registers and identify written roots at collecting boundaries.

use smallvec::SmallVec;

use super::ir::{BlockId, Graph, Kind, NodeId, Repr, StateValues};
use positions::UsePositions;

mod positions;
pub(super) use positions::NO_POSITION;

/// A block or value outside the sets.
const NONE: u32 = u32::MAX;

/// Instruction inputs and reconstruction reads at the instruction. A lazy
/// state can reference the instruction's own result without reading it here.
fn node_reads<'g>(
    graph: &'g Graph,
    node: NodeId,
    states: &'g StateValues,
) -> impl Iterator<Item = NodeId> + 'g {
    let data = graph.node(node);
    data.inputs.iter().copied().chain(
        data.eager
            .iter()
            .chain(data.lazy.iter())
            .flat_map(move |&state| states.of(state).iter().copied())
            .filter(move |&value| value != node),
    )
}

/// Block live-out sets and read positions for exact liveness queries.
#[derive(Debug)]
pub(crate) struct Liveness {
    pub(super) positions: UsePositions,
    /// Dense bit index of block values and rematerializable constants, by
    /// node id; [`NONE`] for a node that defines no tracked value.
    index: Vec<u32>,
    /// Dense row index by block id; [`NONE`] outside the layout.
    block_rows: Vec<u32>,
    /// Words of one set.
    words: usize,
    /// One live-out set per block in layout order.
    live_out: Vec<u64>,
}

impl Liveness {
    /// Solve liveness using the already flattened frame-state values.
    pub(crate) fn compute(graph: &Graph, layout: &[BlockId], states: &StateValues) -> Self {
        let positions = UsePositions::collect(graph, layout, states);
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
        let mut block_rows = vec![NONE; graph.blocks.len()];
        for (row, &block) in layout.iter().enumerate() {
            block_rows[block.0 as usize] = row as u32;
        }
        let row_of = |block: BlockId| block_rows[block.0 as usize];
        // Upward-exposed reads and definitions of each block; phis define
        // at the block's start and read nothing in it.
        let blocks = layout.len();
        let mut generated = vec![0u64; blocks * words];
        let mut killed = vec![0u64; blocks * words];
        for &block in layout {
            let data = graph.block(block);
            let row = row_of(block) as usize * words;
            let live = &mut generated[row..row + words];
            let kill = &mut killed[row..row + words];
            for &node in data.control.iter().rev().chain(data.body.iter().rev()) {
                clear(live, node);
                bit(kill, node);
                for value in node_reads(graph, node, states) {
                    bit(live, value);
                }
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
                if row != NONE {
                    let start = row as usize * words;
                    for (word, &incoming) in out.iter_mut().zip(&live_in[start..start + words]) {
                        *word |= incoming;
                    }
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
                let row = row_of(block) as usize * words;
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
        // The fixed point no longer needs the locally generated sets.
        // Reuse their storage for the outgoing edge sets.
        let mut live_out = generated;
        for &block in layout {
            let row = row_of(block) as usize * words;
            live_out_of(block, &live_in, &mut live_out[row..row + words]);
        }
        Self {
            positions,
            index,
            block_rows,
            words,
            live_out,
        }
    }

    /// Whether `value` is read after `node` before its next definition, via
    /// a local read or an outgoing edge of `block`.
    pub(crate) fn is_live_after(&self, block: BlockId, node: NodeId, value: NodeId) -> bool {
        let slot = self.index[value.0 as usize];
        if slot == NONE {
            return false;
        }
        let at = self.positions.node(node);
        let end = self.positions.block(block).end;
        debug_assert!(self.positions.block(block).contains(&at));
        let definition = self.positions.node(value);
        let read_end = if at < definition && definition < end {
            // The later definition kills the value from this point onward.
            // Any reads by the defining instruction precede its write.
            definition + 1
        } else {
            let row = self.block_rows[block.0 as usize] as usize;
            let word = row * self.words + slot as usize / 64;
            if self.live_out[word] & (1 << (slot % 64)) != 0 {
                return true;
            }
            end
        };
        if at + 1 == read_end {
            return false;
        }
        let uses = self.positions.of(value);
        match uses.last() {
            Some(&last) if last < read_end => last > at,
            Some(_) => uses
                .get(uses.partition_point(|&read| read <= at))
                .is_some_and(|&read| read < read_end),
            None => false,
        }
    }
}
