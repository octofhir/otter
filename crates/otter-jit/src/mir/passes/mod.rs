//! MIR optimization passes.
//!
//! ## Pass pipeline
//!
//! Tier 1 (baseline) runs a lightweight pipeline:
//!   `const_fold -> guard_elim -> repr_elim`
//!
//! DCE and block_layout are implemented but disabled until operand tracking
//! and block index remapping are complete.
//!
//! Tier 2 (optimized) will add: type_analysis, repr_propagation, inline, licm, bce.
//!
//! Each pass implements `fn run(graph: &mut MirGraph)` and is idempotent.

pub mod block_layout;
pub mod const_fold;
pub mod dce;
pub mod guard_elim;
pub mod repr_elim;

use super::graph::MirGraph;

/// Which tier's pass pipeline to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassTier {
    /// Tier 1: lightweight passes only.
    Baseline,
    /// Tier 2: full optimization pipeline.
    Optimized,
}

/// Run the optimization pass pipeline on a MIR graph.
///
/// If `dump_passes` is true, prints the MIR after each pass to stderr.
pub fn run_passes(graph: &mut MirGraph, tier: PassTier, dump_passes: bool) {
    let passes: &[(&str, fn(&mut MirGraph))] = match tier {
        PassTier::Baseline => &[
            ("const_fold", const_fold::run),
            ("guard_elim", guard_elim::run),
            ("repr_elim", repr_elim::run),
            // DCE disabled: operand tracking catch-all misses some ValueId refs,
            // causing live instructions to be removed. Enable after full coverage.
            // ("dce", dce::run),
            // block_layout disabled: reorders blocks without updating BlockId->index.
            // ("block_layout", block_layout::run),
        ],
        PassTier::Optimized => &[
            ("const_fold", const_fold::run),
            ("guard_elim", guard_elim::run),
            ("repr_elim", repr_elim::run),
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
