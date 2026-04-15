//! Sparse PC → feedback-slot map stored alongside the bytecode stream.
//!
//! In v1 feedback slots were PC-indexed via `FeedbackTableLayout` but
//! never populated by the source compiler. v2 inverts that: every
//! feedback-carrying instruction gets its slot id assigned at compile
//! time, recorded here as a sorted `Vec<(u32, FeedbackSlot)>`, and the
//! JIT's `trust_int32` path reads it back by binary search.

/// Identifier of a feedback slot within this function's feedback vector.
/// Opaque to the bytecode layer — the interpreter and JIT resolve it
/// against `FeedbackVector` / `FeedbackTableLayout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FeedbackSlot(pub u16);

/// A sparse, sorted, binary-searchable map from bytecode PC to
/// [`FeedbackSlot`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeedbackMap {
    entries: Vec<(u32, FeedbackSlot)>,
}

impl FeedbackMap {
    /// Construct from an already-sorted `Vec<(pc, slot)>`. Callers who
    /// emit in PC-increasing order save a sort.
    #[must_use]
    pub fn from_sorted(entries: Vec<(u32, FeedbackSlot)>) -> Self {
        debug_assert!(
            entries.windows(2).all(|w| w[0].0 <= w[1].0),
            "FeedbackMap::from_sorted requires entries sorted by PC"
        );
        Self { entries }
    }

    /// Look up the feedback slot for `pc`. Returns `None` for
    /// instructions that have no associated slot (most do not).
    #[must_use]
    pub fn get(&self, pc: u32) -> Option<FeedbackSlot> {
        self.entries
            .binary_search_by_key(&pc, |(p, _)| *p)
            .ok()
            .map(|idx| self.entries[idx].1)
    }

    /// Number of slots attached to this bytecode.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether any slot is attached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate all `(pc, slot)` entries in ascending PC order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, FeedbackSlot)> + '_ {
        self.entries.iter().copied()
    }
}
