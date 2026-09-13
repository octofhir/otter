//! Shared resource bound for plain and method inline snapshot trees.
//!
//! # Contents
//! - Ancestry rejection, maximum nesting, and a per-root body budget.
//!
//! # Invariants
//! - Every candidate consumes the same budget before preparing its descendants.
//! - Rejected or failed candidates never replenish work already performed.
//! - Function recursion cannot build cyclic snapshots.

pub(super) struct InlineSnapshotBudget {
    ancestry: Vec<u32>,
    remaining: usize,
}

impl InlineSnapshotBudget {
    pub(super) fn new(root: u32) -> Self {
        Self {
            ancestry: vec![root],
            remaining: 64,
        }
    }

    pub(super) fn enter(&mut self, function_id: u32) -> bool {
        if self.remaining == 0 || self.ancestry.len() > 3 || self.ancestry.contains(&function_id) {
            return false;
        }
        self.remaining -= 1;
        self.ancestry.push(function_id);
        true
    }

    pub(super) fn leave(&mut self) {
        self.ancestry.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_work_is_bounded_across_siblings_and_recursion() {
        let mut budget = InlineSnapshotBudget::new(1);
        assert!(!budget.enter(1));
        assert!(budget.enter(2));
        assert!(!budget.enter(1));
        assert!(!budget.enter(2));
        assert!(budget.enter(3));
        assert!(budget.enter(4));
        assert!(!budget.enter(5));
        budget.leave();
        budget.leave();
        budget.leave();
        for _ in 0..61 {
            assert!(budget.enter(2));
            budget.leave();
        }
        assert!(!budget.enter(2));
    }
}
