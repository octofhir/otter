//! Owned backend-neutral state for one optimizing compilation.
//!
//! An [`OptimizedUnit`] is the immutable result of the analysis pipeline. It
//! keeps every graph, SSA, allocation, frame-state, and deoptimization artifact
//! together so a machine backend consumes one coherent compilation unit rather
//! than rebuilding or loosely pairing individual analyses.
//!
//! # Contents
//! - [`OptimizedUnit`] — the complete verified analysis product handed to a
//!   backend.
//!
//! # Invariants
//! - Every field describes the same [`otter_vm::JitCompileSnapshot`].
//! - `allocation` has already been legalized for every abstract deopt state and
//!   its edge moves have been rebuilt against those final locations.
//! - `linear_scan_spill_slot_count` records allocator pressure before deopt
//!   legalization; `spill_slot_count` is the final emitter reservation.
//!
//! # See also
//! - [`super::pipeline`] — the sole constructor of verified units.
//! - [`crate::ir`] — the individual backend-neutral analysis types.

use crate::ir::{
    cfg::ControlFlowGraph, deopt_lower::DeoptLowering, dom::DominatorTree,
    frame_state::FrameStateTable, inline::InlineTree, liveness::Liveness, regalloc::Allocation,
    repr::ReprMap, ssa::SsaFunction,
};

/// Complete verified analysis state consumed by an optimizing backend.
#[derive(Debug)]
pub(crate) struct OptimizedUnit {
    pub(crate) tree: InlineTree,
    pub(crate) cfg: ControlFlowGraph,
    pub(crate) dom: DominatorTree,
    pub(crate) ssa: SsaFunction,
    pub(crate) liveness: Liveness,
    pub(crate) reprs: ReprMap,
    pub(crate) allocation: Allocation,
    pub(crate) frame_states: FrameStateTable,
    pub(crate) deopt: DeoptLowering,
    pub(crate) linear_scan_spill_slot_count: u32,
    pub(crate) spill_slot_count: u32,
}
