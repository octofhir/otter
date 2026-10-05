//! Shared resource bound for owned plain and method inline snapshot trees.
//!
//! # Contents
//! - Maximum nesting and a per-root preparation budget.
//!
//! # Invariants
//! - Every candidate consumes the same budget before preparing its descendants.
//! - Rejected or failed candidates never replenish work already performed.
//! - Each body is a fresh owned snapshot; depth bounds recursive preparation.
//! - Graph lowering independently charges its existing depth and byte budgets.
//!
//! # See also
//! - `jit_compile::bake_inline_body` owns each descendant's source and feedback.

pub(super) struct InlineSnapshotBudget {
    depth: u8,
    remaining: usize,
}

impl InlineSnapshotBudget {
    pub(super) fn new() -> Self {
        Self {
            depth: 0,
            remaining: 64,
        }
    }

    pub(super) fn enter(&mut self) -> bool {
        if self.remaining == 0 || self.depth >= 3 {
            return false;
        }
        self.remaining -= 1;
        self.depth += 1;
        true
    }

    pub(super) fn leave(&mut self) {
        self.depth = self
            .depth
            .checked_sub(1)
            .expect("inline preparation leaves an entered body");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_work_is_bounded_across_siblings_and_nesting() {
        let mut budget = InlineSnapshotBudget::new();
        assert!(budget.enter());
        assert!(budget.enter());
        assert!(budget.enter());
        assert!(!budget.enter());
        budget.leave();
        budget.leave();
        budget.leave();
        for _ in 0..61 {
            assert!(budget.enter());
            budget.leave();
        }
        assert!(!budget.enter());
    }
}
