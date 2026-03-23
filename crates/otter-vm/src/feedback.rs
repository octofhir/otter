//! Runtime feedback side-table definitions.

/// Stable identifier of a feedback slot within a function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FeedbackSlotId(pub u16);

/// Kind of feedback a slot is expected to carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FeedbackKind {
    /// Arithmetic or numeric coercion feedback.
    Arithmetic,
    /// Comparison feedback.
    Comparison,
    /// Truthiness/branch feedback.
    Branch,
    /// Property access feedback.
    Property,
    /// Call-target feedback.
    Call,
}

/// Static layout for a single feedback slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FeedbackSlotLayout {
    id: FeedbackSlotId,
    kind: FeedbackKind,
}

impl FeedbackSlotLayout {
    /// Creates a slot layout.
    #[must_use]
    pub const fn new(id: FeedbackSlotId, kind: FeedbackKind) -> Self {
        Self { id, kind }
    }

    /// Returns the slot identifier.
    #[must_use]
    pub const fn id(self) -> FeedbackSlotId {
        self.id
    }

    /// Returns the feedback kind.
    #[must_use]
    pub const fn kind(self) -> FeedbackKind {
        self.kind
    }
}

/// Immutable feedback side-table layout for a function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackTableLayout {
    slots: Box<[FeedbackSlotLayout]>,
}

impl FeedbackTableLayout {
    /// Creates a feedback table layout from owned slot definitions.
    #[must_use]
    pub fn new(slots: Vec<FeedbackSlotLayout>) -> Self {
        Self {
            slots: slots.into_boxed_slice(),
        }
    }

    /// Creates an empty feedback layout.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Returns the number of slots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Returns `true` when there are no slots.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Returns the slot layout for a given slot identifier.
    #[must_use]
    pub fn get(&self, id: FeedbackSlotId) -> Option<FeedbackSlotLayout> {
        self.slots.get(usize::from(id.0)).copied()
    }

    /// Returns all slot layouts.
    #[must_use]
    pub fn slots(&self) -> &[FeedbackSlotLayout] {
        &self.slots
    }
}

impl Default for FeedbackTableLayout {
    fn default() -> Self {
        Self::empty()
    }
}
