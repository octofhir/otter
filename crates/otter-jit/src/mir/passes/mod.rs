//! MIR optimization passes.
//!
//! ## Pass pipelines
//!
//! **Tier 1** (baseline): fast, lightweight passes only.
//!   `const_fold -> guard_elim -> repr_elim`
//!
//! **Tier 2** (optimized): full speculative pipeline.
//!   `const_fold -> guard_elim (dominator-based + type-proof) -> repr_elim -> licm`
//!
//! Standalone analysis passes (used by other passes, not in pipeline directly):
//! - `type_analysis`: forward dataflow type inference (AbstractType per value)
//! - `repr_propagation`: representation selection (Int32/Float64/Tagged per value)
//! - `dominators`: dominator tree computation (Cooper-Harvey-Kennedy)
//!
//! ## Implemented but disabled
//! - `dce`: operand tracking incomplete for some MIR ops
//! - `block_layout`: needs BlockId→index remapping in lowering

pub mod alias;
pub mod block_layout;
pub mod const_fold;
pub mod dce;
pub mod dominators;
pub mod escape_analysis;
pub mod guard_elim;
pub mod gvn;
pub mod inline;
pub mod licm;
pub mod repr_elim;
pub mod repr_propagation;
pub mod strength_reduce;
pub mod type_analysis;

use super::graph::MirGraph;

type PassFn = fn(&mut MirGraph);
type PassPipeline<'a> = &'a [(&'a str, PassFn)];

/// Which tier's pass pipeline to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassTier {
    /// Tier 1: lightweight passes only.
    Baseline,
    /// Tier 2: full optimization pipeline (speculative tier).
    Optimized,
}

/// Run the optimization pass pipeline on a MIR graph.
///
/// If `dump_passes` is true, prints the MIR after each pass to stderr.
pub fn run_passes(graph: &mut MirGraph, tier: PassTier, dump_passes: bool) {
    let passes: PassPipeline<'_> = match tier {
        PassTier::Baseline => &[
            ("const_fold", const_fold::run),
            ("guard_elim", guard_elim::run),
            ("repr_elim", repr_elim::run),
            ("dce", dce::run),
            ("block_layout", block_layout::run),
        ],
        PassTier::Optimized => &[
            // Phase 1: constant folding (cheap, reduces graph size).
            ("const_fold", const_fold::run),
            // Phase 2: strength reduction (algebraic simplification).
            ("strength_reduce", strength_reduce::run),
            // Phase 3: guard elimination with dominator tree + type proofs.
            ("guard_elim", guard_elim::run),
            // Phase 4: box/unbox chain elimination.
            ("repr_elim", repr_elim::run),
            // Phase 5: global value numbering (cross-block CSE).
            ("gvn", gvn::run),
            // Phase 6: loop-invariant code motion.
            ("licm", licm::run),
            // Phase 7: escape analysis (identify non-escaping allocations).
            ("escape_analysis", escape_analysis::run),
            // Phase 8: dead code elimination (cleanup after all other passes).
            ("dce", dce::run),
            // Phase 9: block layout (deopt blocks to end, fallthrough ordering).
            ("block_layout", block_layout::run),
        ],
    };

    for &(name, pass_fn) in passes {
        pass_fn(graph);

        if dump_passes {
            eprintln!("[JIT] === MIR after {name} ===");
            eprintln!("{graph}");
        }
    }
}
