//! MIR optimization passes.
//!
//! Tier 1 skips all passes. Tier 2 runs them in this order:
//! inline → const_fold → shape_prop → guard_elim → loop_invariant → dead_code

// Passes will be added in Phase 5.
