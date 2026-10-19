//! Shared resource bound for owned plain and method inline snapshot trees.
//!
//! # Contents
//! - Maximum nesting, deeper for small bodies, a per-root bound on prepared
//!   bodies, and the call frequency of the body being baked.
//!
//! # Invariants
//! - Every candidate consumes the same budget before preparing its descendants.
//! - Rejected or failed candidates never replenish work already performed.
//! - Each body is a fresh owned snapshot; depth bounds recursive preparation.
//! - A candidate's frequency is its site's calls per entry of the body that
//!   holds it, times that body's own frequency; the root runs once. A site
//!   below [`crate::jit::JIT_MIN_INLINING_FREQUENCY`] is never prepared
//!   unless its body is small: no optimizing caller inlines it. A site
//!   without call feedback is unknown and passes.
//! - Graph lowering independently chooses, by frequency, which prepared
//!   bodies it inlines.
//!
//! # See also
//! - `jit_compile::bake_inline_body` owns each descendant's source and feedback.

pub(super) struct InlineSnapshotBudget {
    remaining: usize,
    /// Frequency of every entered body, outermost first, relative to one
    /// run of the root.
    frequencies: smallvec::SmallVec<[f32; 8]>,
}

impl InlineSnapshotBudget {
    pub(super) fn new() -> Self {
        Self {
            remaining: 64,
            frequencies: smallvec::SmallVec::new(),
        }
    }

    /// Admit one body of `instructions` bytecode instructions called at a
    /// site of the current body that ran `site_frequency` times per entry.
    pub(super) fn enter(&mut self, instructions: u32, site_frequency: Option<f32>) -> bool {
        let small = instructions <= crate::jit::JIT_SMALL_INLINE_INSTRUCTIONS;
        let depth = if small {
            crate::jit::JIT_SMALL_INLINE_DEPTH
        } else {
            crate::jit::JIT_INLINE_DEPTH
        };
        let caller = self.frequencies.last().copied().unwrap_or(1.0);
        let frequency = site_frequency.map_or(caller, |site| caller * site);
        if self.frequencies.len() >= usize::from(depth)
            || self.remaining == 0
            || (!small && frequency < crate::jit::JIT_MIN_INLINING_FREQUENCY)
        {
            return false;
        }
        self.remaining -= 1;
        self.frequencies.push(frequency);
        true
    }

    pub(super) fn leave(&mut self) {
        self.frequencies
            .pop()
            .expect("inline preparation leaves an entered body");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_work_is_bounded_across_siblings_and_nesting() {
        let mut budget = InlineSnapshotBudget::new();
        assert!(budget.enter(20, None));
        assert!(budget.enter(20, None));
        assert!(budget.enter(20, None));
        assert!(!budget.enter(20, None));
        // Small bodies nest deeper.
        for _ in 3..8 {
            assert!(budget.enter(4, None));
        }
        assert!(!budget.enter(4, None));
        for _ in 0..8 {
            budget.leave();
        }
        for _ in 0..56 {
            assert!(budget.enter(20, None));
            budget.leave();
        }
        assert!(!budget.enter(4, None));
    }

    #[test]
    fn rarely_reached_sites_are_not_prepared() {
        let mut budget = InlineSnapshotBudget::new();
        assert!(!budget.enter(20, Some(0.05)));
        // A small body costs no more inlined than its call.
        assert!(budget.enter(4, Some(0.05)));
        budget.leave();
        assert!(budget.enter(20, Some(0.5)));
        // A site run once per entry of a body reached half the time.
        assert!(budget.enter(20, Some(1.0)));
        budget.leave();
        assert!(!budget.enter(20, Some(0.15)));
        assert!(budget.enter(20, Some(4.0)));
    }
}
