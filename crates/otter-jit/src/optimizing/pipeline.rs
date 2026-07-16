//! Backend-neutral optimizing analysis orchestration.
//!
//! This module owns the ordered transformation from an immutable VM snapshot to
//! a verified [`OptimizedUnit`]. Machine backends provide only their register
//! budget and inline-body acceptance policy; graph construction, SSA, liveness,
//! representation selection, allocation, deopt legalization, and concrete
//! frame lowering stay identical across backends.
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
//! - Deopt legalization happens after recording raw linear-scan pressure and
//!   before rebuilding phi edge moves and lowering the concrete deopt table.
//! - Analysis is pure over the compile snapshot; no executable memory or
//!   runtime transition address is observed here.
//!
//! # See also
//! - [`super::unit::OptimizedUnit`] — the owned verified output.
//! - [`crate::optimizing::arm64`] — the first machine-code consumer.

use std::collections::BTreeMap;

use otter_vm::{JitCompileSnapshot, JitInlineCallee};

use super::unit::OptimizedUnit;
use crate::{
    Unsupported,
    ir::{
        cfg::{CfgError, ControlFlowGraph},
        deopt_lower::{DeoptLowering, DeoptLoweringError},
        dom::{DomError, DominatorTree},
        frame_state::{FrameStateError, FrameStateTable},
        inline::{InlineError, InlineTree},
        liveness::{Liveness, LivenessError},
        regalloc::{Allocation, Location, RegClass, RegallocError, RegisterBudget},
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

    /// Analyze and verify one immutable snapshot for a machine backend.
    pub(crate) fn analyze(
        self,
        view: &JitCompileSnapshot,
        accept_inline: impl Fn(&JitInlineCallee) -> bool,
    ) -> Result<OptimizedUnit, OptimizationError> {
        let tree = InlineTree::build_where(view, accept_inline);
        tree.verify()
            .map_err(OptimizationError::InlineTreeVerification)?;

        let cfg =
            ControlFlowGraph::build_inlined(&tree).map_err(OptimizationError::CfgConstruction)?;
        cfg.verify().map_err(OptimizationError::CfgVerification)?;

        let dom = DominatorTree::compute(&cfg);
        dom.verify(&cfg)
            .map_err(OptimizationError::DominanceVerification)?;

        let ssa =
            SsaFunction::build_inlined(&tree, &cfg).map_err(OptimizationError::SsaConstruction)?;
        ssa.verify(&cfg, &dom)
            .map_err(OptimizationError::SsaVerification)?;

        let liveness = Liveness::compute(&ssa, &cfg);
        liveness
            .verify(&ssa, &cfg, &dom)
            .map_err(OptimizationError::LivenessVerification)?;

        let reprs = ReprMap::compute(&tree, &ssa);
        reprs
            .verify(&tree, &ssa)
            .map_err(OptimizationError::RepresentationVerification)?;

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
        let frame_states = FrameStateTable::build_inlined(&call_sites, &ssa, &cfg)
            .map_err(OptimizationError::FrameStateConstruction)?;
        frame_states
            .verify(&ssa, &cfg, &dom)
            .map_err(OptimizationError::FrameStateVerification)?;

        let linear_scan_spill_slot_count = total_spill_slots(&allocation)?;
        let mut allocation = legalize_deopt_locations(&allocation, &frame_states)?;
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
            linear_scan_spill_slot_count,
            spill_slot_count,
        })
    }
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
fn legalize_deopt_locations(
    allocation: &Allocation,
    frame_states: &FrameStateTable,
) -> Result<Allocation, OptimizationError> {
    let mut legalized = allocation.clone();
    for state in frame_states.states() {
        let mut owners = BTreeMap::<Location, ValueId>::new();
        for value in state.registers.iter().flatten().copied() {
            let location = legalized.location(value);
            if owners.get(&location).is_some_and(|owner| *owner != value) {
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
    }
    Ok(legalized)
}
