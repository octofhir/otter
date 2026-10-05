//! Final Graph machine-code output independent of its instruction encoder.
//!
//! # Contents
//! - [`ExitSite`] identifies a native exit and its exact source reconstruction.
//! - [`Emission`] owns executable bytes, relocations, roots and cold sidecars.
//! - Edge assignments and bounded forwarding share one target-neutral planner.
//!
//! # Invariants
//! - One code object owns the mapping and every address-bearing retained cell.
//! - Machine offsets use the same finalized buffer across all sidecars.
//! - Safepoints describe the one initialized tagged region and inline position.
//!
//! # See also
//! - [`super::frame`] for shared frame and recovery geometry.
//! - [`super::compile`] for installation and opt-in artifact capture.

use dynasmrt::DynamicLabel;
use otter_vm::native_abi::{ExitAction, ExitReason, SafepointRecord};

use super::ir::NodeId;
use super::ir::{BlockId, Graph, Kind};
use super::regalloc::{Allocation, Move};
use crate::artifact::relocation::RelocationCapture;

#[derive(Debug, Clone)]
pub(crate) struct ExitSite {
    pub(crate) label: DynamicLabel,
    pub(crate) node: NodeId,
    pub(crate) lazy: bool,
    pub(crate) reason: ExitReason,
    pub(crate) action: ExitAction,
}

pub(crate) struct Emission {
    pub(crate) buffer: dynasmrt::ExecutableBuffer,
    pub(crate) tier_entry: usize,
    pub(crate) call_entry: usize,
    pub(crate) exits: Vec<ExitSite>,
    pub(crate) relocations: RelocationCapture,
    pub(crate) load_ic_cells: Box<[crate::entry::PropertySourceCell]>,
    pub(crate) store_ic_cells: Box<[crate::entry::PropertySourceCell]>,
    pub(crate) node_offsets: Vec<(usize, NodeId)>,
    pub(crate) body_end: usize,
    pub(crate) osr_dispatch_end: Option<usize>,
    pub(crate) site_records: Vec<SafepointRecord>,
    pub(crate) return_sites: Vec<otter_vm::native_abi::SafepointEntry>,
    pub(crate) spliced_functions: std::collections::BTreeSet<u32>,
}

/// Complete edge assignment, including final phi output locations.
pub(crate) fn edge_moves(allocation: &Allocation, from: BlockId, to: BlockId) -> Vec<Move> {
    let Some(edge) = allocation.edges.get(&(from, to)) else {
        return Vec::new();
    };
    let mut moves = edge.moves.clone();
    for &(phi, source) in &edge.phi_inputs {
        let node = allocation.node(phi);
        if !node.skipped
            && let Some(target) = node.result
        {
            moves.push(Move {
                from: source,
                to: target,
            });
        }
    }
    moves
}

/// Bounded empty-block forwarding with no value or phi assignment bypass.
pub(crate) fn forwarded(graph: &Graph, allocation: &Allocation, mut block: BlockId) -> BlockId {
    for _ in 0..8 {
        let data = graph.block(block);
        let Some(Kind::Jump(target)) = data.control.map(|id| &graph.node(id).kind) else {
            break;
        };
        if !data.phis.is_empty()
            || !data.body.is_empty()
            || !edge_moves(allocation, block, *target).is_empty()
        {
            break;
        }
        block = *target;
    }
    block
}
