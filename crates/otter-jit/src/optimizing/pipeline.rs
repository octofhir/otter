//! Backend-neutral optimizing analysis orchestration.
//!
//! This module owns the ordered transformation from an immutable VM snapshot to
//! a verified [`OptimizedUnit`]. Machine backends provide only their register
//! budget and inline-body acceptance policy; graph construction, SSA, liveness,
//! representation selection, identity-copy coalescing, allocation, deopt
//! legalization, and concrete frame lowering stay identical across backends.
//!
//! # Contents
//! - [`OptimizationPipeline`] — configuration and whole-unit analysis driver.
//! - [`OptimizationError`] — typed stage failures converted to the existing
//!   silent [`Unsupported`] fallback at the backend boundary.
//! - [`total_spill_slots`] — checked final spill-area sizing shared with emitters.
//!
//! # Invariants
//! - Stages run in dependency order and every verifier runs before its output is
//!   consumed by a later stage.
//! - Bytecode nodes are lowered into the primitive guard vocabulary between SSA
//!   construction and SSA verification, so every later stage sees one graph.
//! - Deopt legalization happens after recording raw linear-scan pressure and
//!   before rebuilding phi edge moves and lowering the concrete deopt table.
//!   Values in one exact-bit copy web remain one owner during legalization;
//!   unread rematerializable heads never acquire a concrete home.
//! - Analysis is pure over the compile snapshot; no executable memory or
//!   runtime transition address is observed here.
//!
//! # See also
//! - [`super::unit::OptimizedUnit`] — the owned verified output.
//! - [`crate::optimizing::arm64`] — the first machine-code consumer.

use std::collections::BTreeMap;

use otter_vm::JitCompileSnapshot;

use super::unit::OptimizedUnit;
use crate::{
    Unsupported,
    ir::{
        cfg::{CfgError, ControlFlowGraph},
        deopt_lower::{DeoptLowering, DeoptLoweringError, rematerialized_deopt_slot},
        dom::{DomError, DominatorTree},
        frame_state::{FrameStateError, FrameStateTable},
        inline::{InlineError, InlineTree},
        licm::hoist_loop_invariant_accesses,
        liveness::{Liveness, LivenessError},
        lower::lower_settled_property_accesses,
        regalloc::{Allocation, AllocationPlan, Location, RegClass, RegallocError, RegisterBudget},
        repr::{ReprError, ReprMap},
        ssa::{SsaError, SsaFunction, ValueId},
    },
};

/// Typed failure at one deterministic optimizing analysis stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OptimizationError {
    InlineTreeVerification(InlineError),
    CfgConstruction(CfgError),
    CfgVerification(CfgError),
    FrameEntryIsLoopHeader { inline: u32 },
    DominanceVerification(DomError),
    SsaConstruction(SsaError),
    SsaVerification(SsaError),
    LivenessVerification(LivenessError),
    RepresentationVerification(ReprError),
    RegisterAllocation(RegallocError),
    AllocationVerification(RegallocError),
    FrameStateConstruction(FrameStateError),
    FrameStateVerification(FrameStateError),
    TotalSpillSlotOverflow,
    DeoptSpillOverflow,
    LegalizedPhiMoves(RegallocError),
    DeoptLowering(DeoptLoweringError),
}

impl OptimizationError {
    /// Preserve the VM's established silent interpreter-fallback contract.
    pub(crate) fn into_unsupported(self) -> Unsupported {
        let reason = match self {
            Self::InlineTreeVerification(_) => "optimizing inline-tree verification",
            Self::CfgConstruction(_) => "optimizing CFG construction",
            Self::CfgVerification(_) => "optimizing CFG verification",
            Self::FrameEntryIsLoopHeader { .. } => "optimizing frame entry is a loop header",
            Self::DominanceVerification(_) => "optimizing dominance verification",
            Self::SsaConstruction(_) => "optimizing SSA construction",
            Self::SsaVerification(_) => "optimizing SSA verification",
            Self::LivenessVerification(_) => "optimizing liveness verification",
            Self::RepresentationVerification(_) => "optimizing representation verification",
            Self::RegisterAllocation(_) => "optimizing register allocation",
            Self::AllocationVerification(_) => "optimizing allocation verification",
            Self::FrameStateConstruction(_) => "optimizing frame-state construction",
            Self::FrameStateVerification(_) => "optimizing frame-state verification",
            Self::TotalSpillSlotOverflow => "optimizing total spill slot overflow",
            Self::DeoptSpillOverflow => "optimizing deopt spill overflow",
            Self::LegalizedPhiMoves(_) => "optimizing legalized phi moves",
            Self::DeoptLowering(_) => "optimizing deopt lowering",
        };
        Unsupported::OperandShape(reason)
    }
}

impl std::fmt::Display for OptimizationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "optimizing analysis failed: {self:?}")
    }
}

impl std::error::Error for OptimizationError {}

/// Configuration and deterministic driver for backend-neutral optimization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OptimizationPipeline {
    register_budget: RegisterBudget,
}

impl OptimizationPipeline {
    /// Configure the backend-visible register files used by linear scan.
    pub(crate) const fn new(register_budget: RegisterBudget) -> Self {
        Self { register_budget }
    }

    /// Analyze and verify one already-decided inline tree.
    ///
    /// Backends use this entry point to test a budget-admitted frame against
    /// the complete splicer and analysis contract before retaining it.
    pub(crate) fn analyze_tree(
        self,
        view: &JitCompileSnapshot,
        tree: InlineTree,
    ) -> Result<OptimizedUnit, OptimizationError> {
        tree.verify()
            .map_err(OptimizationError::InlineTreeVerification)?;

        let cfg =
            ControlFlowGraph::build_inlined(&tree).map_err(OptimizationError::CfgConstruction)?;
        cfg.verify().map_err(OptimizationError::CfgVerification)?;
        reject_looping_frame_entry(&cfg)?;

        let dom = DominatorTree::compute(&cfg);
        dom.verify(&cfg)
            .map_err(OptimizationError::DominanceVerification)?;

        let mut ssa =
            SsaFunction::build_inlined(&tree, &cfg).map_err(OptimizationError::SsaConstruction)?;
        // Lowering a settled write needs the stored value's representation,
        // which this pass does not change: it selects by producer and every
        // producer here keeps its node. The map is recomputed below over the
        // renumbered graph.
        lower_settled_property_accesses(&mut ssa, &cfg, view, &tree);
        // A settled access on a receiver its loop cannot change belongs before
        // the loop, not in it.
        let hoisted_loops =
            hoist_loop_invariant_accesses(&mut ssa, &cfg, &dom, &view.optimized_bail_pcs);

        let reprs = ReprMap::compute(&tree, &ssa);
        reprs
            .verify(&tree, &ssa)
            .map_err(OptimizationError::RepresentationVerification)?;
        ssa.verify(&cfg, &dom, &reprs)
            .map_err(OptimizationError::SsaVerification)?;

        let liveness = Liveness::compute(&ssa, &cfg);
        liveness
            .verify(&ssa, &cfg, &dom)
            .map_err(OptimizationError::LivenessVerification)?;

        let allocation = Allocation::compute(&ssa, &cfg, &liveness, &reprs, self.register_budget)
            .map_err(OptimizationError::RegisterAllocation)?;
        allocation
            .verify(&ssa, &cfg, &liveness, &reprs)
            .map_err(OptimizationError::AllocationVerification)?;

        let call_sites: Vec<_> = tree
            .frames
            .iter()
            .map(|frame| frame.call_site.clone())
            .collect();
        let frame_states = FrameStateTable::build_inlined(&call_sites, &ssa, &cfg, &reprs)
            .map_err(OptimizationError::FrameStateConstruction)?;
        frame_states
            .verify(&ssa, &cfg, &dom)
            .map_err(OptimizationError::FrameStateVerification)?;

        let linear_scan_spill_slot_count = total_spill_slots(&allocation)?;
        let merges = crate::ir::regalloc::MergeLiveness::compute(&ssa);
        let mut allocation =
            legalize_deopt_locations(&allocation, &frame_states, &ssa, &reprs, &merges)?;
        allocation
            .rebuild_edge_moves(&ssa, &cfg, &reprs)
            .map_err(OptimizationError::LegalizedPhiMoves)?;
        let spill_slot_count = total_spill_slots(&allocation)?;

        let deopt = DeoptLowering::build(view, &tree, &ssa, &frame_states, &allocation, &reprs)
            .map_err(OptimizationError::DeoptLowering)?;

        Ok(OptimizedUnit {
            tree,
            cfg,
            dom,
            ssa,
            liveness,
            reprs,
            allocation,
            frame_states,
            deopt,
            hoisted_loops,
            linear_scan_spill_slot_count,
            spill_slot_count,
        })
    }
}

/// Refuse a frame whose entry block is its own loop header.
///
/// A frame entry carries the seed definitions of its registers — parameters for
/// the root frame, `Uninitialized` for the rest — and those seeds are the
/// block's head values. Phi placement only considers a block a join when it has
/// two normal predecessors, and a frame entry reached by its own back edge has
/// exactly one: the latch. The entry edge is implicit, so no phi is placed, the
/// seeds win, and the value the latch carries is dropped. A loop written so
/// that its header is the first instruction (`while (node !== null) { … node =
/// node.next }`) would then re-read the parameter every iteration and never
/// terminate.
///
/// Modeling that implicit edge belongs in the graph, not in a repair here.
/// Until it is, the frame belongs to the template tier.
fn reject_looping_frame_entry(cfg: &ControlFlowGraph) -> Result<(), OptimizationError> {
    for (inline, &entry) in cfg.frame_entries.iter().enumerate() {
        let frame = cfg.blocks[entry.0 as usize].inline;
        let looping = cfg.blocks[entry.0 as usize].preds.iter().any(|&pred| {
            cfg.blocks[pred.0 as usize].inline == frame
                && cfg.blocks[pred.0 as usize].normal_succs.contains(&entry)
        });
        if looping {
            return Err(OptimizationError::FrameEntryIsLoopHeader {
                inline: inline as u32,
            });
        }
    }
    Ok(())
}

/// Checked total number of final GPR and FP spill slots.
pub(crate) fn total_spill_slots(allocation: &Allocation) -> Result<u32, OptimizationError> {
    allocation
        .spill_slot_counts
        .gpr
        .checked_add(allocation.spill_slot_counts.fp)
        .ok_or(OptimizationError::TotalSpillSlotOverflow)
}

/// Move only deopt-colliding values into fresh spill homes.
///
/// Linear scan may reuse a register after its live interval ends while an
/// abstract frame state still names the old interpreter-register value. The
/// final allocation gives only those colliding values fresh homes and leaves
/// every non-conflicting assignment intact.
///
/// The colliding set of one exit is its whole reified chain, not one frame: an
/// exit inside a spliced callee rebuilds that callee and every caller above it
/// from a single register dump, so a caller's value and a callee's value that
/// share a machine home restore each other's bits.
fn legalize_deopt_locations(
    allocation: &Allocation,
    frame_states: &FrameStateTable,
    ssa: &SsaFunction,
    reprs: &ReprMap,
    merges: &crate::ir::regalloc::MergeLiveness,
) -> Result<Allocation, OptimizationError> {
    let mut legalized = allocation.clone();
    let plan = AllocationPlan::compute(ssa, reprs, merges);
    for state in frame_states.states() {
        let mut owners = BTreeMap::<Location, ValueId>::new();
        let mut frame = Some(state);
        while let Some(current) = frame {
            for value in current.registers.iter().flatten().copied() {
                if rematerialized_deopt_slot(ssa, reprs, merges, Some(value)).is_some() {
                    continue;
                }
                let location = legalized.location(value);
                if owners
                    .get(&location)
                    .is_some_and(|owner| !plan.same_value(*owner, value))
                {
                    let class = location.class();
                    let next_spill = match class {
                        RegClass::Gpr => &mut legalized.spill_slot_counts.gpr,
                        RegClass::Fp => &mut legalized.spill_slot_counts.fp,
                    };
                    let slot = *next_spill;
                    *next_spill = next_spill
                        .checked_add(1)
                        .ok_or(OptimizationError::DeoptSpillOverflow)?;
                    legalized.locations[value.0 as usize] = Location::Spill(class, slot);
                    owners.insert(legalized.location(value), value);
                } else {
                    owners.insert(location, value);
                }
            }
            frame = current.caller.map(|index| &frame_states.states()[index]);
        }
    }
    Ok(legalized)
}
