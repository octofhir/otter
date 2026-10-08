//! Shared resource bound for owned plain and method inline snapshot trees.
//!
//! # Contents
//! - Maximum nesting, deeper for small bodies, and a per-root preparation
//!   budget.
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

    /// Admit one body, `small` when its bytecode is at most
    /// [`crate::jit::JIT_SMALL_INLINE_BYTECODE_BYTES`] long.
    pub(super) fn enter(&mut self, small: bool) -> bool {
        let depth = if small {
            crate::jit::JIT_SMALL_INLINE_DEPTH
        } else {
            crate::jit::JIT_INLINE_DEPTH
        };
        if self.remaining == 0 || self.depth >= depth {
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
        assert!(budget.enter(false));
        assert!(budget.enter(false));
        assert!(budget.enter(false));
        assert!(!budget.enter(false));
        // Small bodies nest deeper.
        for _ in 3..8 {
            assert!(budget.enter(true));
        }
        assert!(!budget.enter(true));
        for _ in 0..8 {
            budget.leave();
        }
        for _ in 0..56 {
            assert!(budget.enter(false));
            budget.leave();
        }
        assert!(!budget.enter(true));
    }
}
