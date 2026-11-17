//! Layout positions and reads shared by CFG liveness and register allocation.
//!
//! # Contents
//! - [`UsePositions`] — contiguous block ranges and sorted uses by SSA value.
//!
//! # Invariants
//! - Phis define at the start of the block, and their operands are read at
//!   the predecessor's terminator. Eager and lazy states read their values
//!   at the owning node, excluding that node's own result.
//! - Only real reads enter the table. The allocator accounts for extra
//!   linear live range on loop backedges separately.
//!
//! # See also
//! - [`super::Liveness`] — combines local reads with outgoing edge sets.

use std::ops::Range;

use super::super::ir::{BlockId, Graph, NodeId, StateValues};
use super::node_reads;

/// Position of a node outside the layout, including rematerialized constants.
pub(in super::super) const NO_POSITION: u32 = u32::MAX;

/// Numbered definitions and their uses in layout order.
#[derive(Debug)]
pub(in super::super) struct UsePositions {
    nodes: Vec<u32>,
    blocks: Vec<Range<u32>>,
    by_value: Vec<Range<u32>>,
    uses: Vec<u32>,
}

impl UsePositions {
    pub(super) fn collect(graph: &Graph, layout: &[BlockId], states: &StateValues) -> Self {
        let mut nodes = vec![NO_POSITION; graph.nodes.len()];
        let mut blocks = vec![NO_POSITION..NO_POSITION; graph.blocks.len()];
        let mut next = 1u32;
        for &block in layout {
            let data = graph.block(block);
            let start = next;
            for &phi in &data.phis {
                nodes[phi.0 as usize] = next;
            }
            next += 1;
            for &node in data.body.iter().chain(data.control.iter()) {
                nodes[node.0 as usize] = next;
                next += 1;
            }
            blocks[block.0 as usize] = start..next;
        }

        let mut uses: Vec<(u32, u32)> = Vec::new();
        for &block in layout {
            let data = graph.block(block);
            for (index, &predecessor) in data.predecessors.iter().enumerate() {
                let Some(control) = graph.block(predecessor).control else {
                    continue;
                };
                let at = nodes[control.0 as usize];
                for &phi in &data.phis {
                    if let Some(&input) = graph.node(phi).inputs.get(index) {
                        uses.push((input.0, at));
                    }
                }
            }
            for &node in data.body.iter().chain(data.control.iter()) {
                let at = nodes[node.0 as usize];
                uses.extend(node_reads(graph, node, states).map(|value| (value.0, at)));
            }
        }
        // Group the positions by value in one flat array, each group sorted
        // and distinct. Uses arrive in layout order but for phi inputs, read
        // at a predecessor's end, so a counting placement keeps each group
        // nearly sorted and only an out-of-order group is sorted.
        let mut starts = vec![0u32; graph.nodes.len() + 1];
        for &(value, _) in &uses {
            starts[value as usize + 1] += 1;
        }
        for index in 1..starts.len() {
            starts[index] += starts[index - 1];
        }
        let mut grouped = vec![0u32; uses.len()];
        let mut fill = starts.clone();
        for &(value, at) in &uses {
            grouped[fill[value as usize] as usize] = at;
            fill[value as usize] += 1;
        }
        let mut grouped_uses = Vec::with_capacity(uses.len());
        let mut by_value = vec![0..0; graph.nodes.len()];
        for (value, range) in by_value.iter_mut().enumerate() {
            let group = &mut grouped[starts[value] as usize..starts[value + 1] as usize];
            if group.is_empty() {
                continue;
            }
            if !group.is_sorted() {
                group.sort_unstable();
            }
            let first = grouped_uses.len();
            for &at in group.iter() {
                if grouped_uses.len() == first || grouped_uses.last() != Some(&at) {
                    grouped_uses.push(at);
                }
            }
            *range = first as u32..grouped_uses.len() as u32;
        }

        Self {
            nodes,
            blocks,
            by_value,
            uses: grouped_uses,
        }
    }

    pub(in super::super) fn node(&self, node: NodeId) -> u32 {
        self.nodes[node.0 as usize]
    }

    pub(in super::super) fn block(&self, block: BlockId) -> &Range<u32> {
        &self.blocks[block.0 as usize]
    }

    /// Sorted, distinct read positions of a value.
    pub(in super::super) fn of(&self, value: NodeId) -> &[u32] {
        let range = &self.by_value[value.0 as usize];
        &self.uses[range.start as usize..range.end as usize]
    }
}
