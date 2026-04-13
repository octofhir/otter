//! Runtime feedback collection for the JIT.
//!
//! Each function has a `FeedbackTableLayout` (immutable metadata) and a
//! `FeedbackVector` (mutable runtime data, one per activation).
//!
//! ## Monotonic Lattices
//!
//! All feedback types use monotonic lattices — values only move "upward"
//! toward more general types. This prevents deoptimization loops.
//!
//! - Arithmetic: `None → Int32 → Number → BigInt → Any`
//! - Comparison: `None → Int32 → Number → String → Any`
//! - Branch: taken/not-taken saturating counters
//! - Property: `Uninitialized → Monomorphic(shape, offset) → Polymorphic → Megamorphic`
//! - Call: `Uninitialized → Monomorphic(target) → Polymorphic → Megamorphic`
//!
//! Inspired by V8's FeedbackVector and SpiderMonkey's CacheIR.

use crate::object::{ObjectShapeId, PropertyInlineCache};

// ============================================================
// Slot ID and Kind (metadata, unchanged from before)
// ============================================================

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
    #[must_use]
    pub const fn new(id: FeedbackSlotId, kind: FeedbackKind) -> Self {
        Self { id, kind }
    }

    #[must_use]
    pub const fn id(self) -> FeedbackSlotId {
        self.id
    }

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
    #[must_use]
    pub fn new(slots: Vec<FeedbackSlotLayout>) -> Self {
        Self {
            slots: slots.into_boxed_slice(),
        }
    }

    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    #[must_use]
    pub fn get(&self, id: FeedbackSlotId) -> Option<FeedbackSlotLayout> {
        self.slots.get(usize::from(id.0)).copied()
    }

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

// ============================================================
// Monotonic Lattices
// ============================================================

/// Arithmetic feedback lattice (monotonic: only moves upward).
///
/// V8 analog: `BinaryOperationFeedback`.
/// `None → Int32 → Number → BigInt → Any`
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(u8)]
pub enum ArithmeticFeedback {
    /// No feedback recorded yet.
    #[default]
    None = 0,
    /// All observed operands were Int32.
    Int32 = 1,
    /// Observed at least one Float64 (but no BigInt).
    Number = 2,
    /// Observed BigInt operands.
    BigInt = 3,
    /// Mixed or unknown types.
    Any = 4,
}

impl ArithmeticFeedback {
    /// Merge new observation (monotonic: can only move up).
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        if other > self { other } else { self }
    }

    /// Record an Int32 observation.
    #[must_use]
    pub fn observe_int32(self) -> Self {
        self.merge(Self::Int32)
    }

    /// Record a Float64 observation.
    #[must_use]
    pub fn observe_number(self) -> Self {
        self.merge(Self::Number)
    }

    /// Record a BigInt observation.
    #[must_use]
    pub fn observe_bigint(self) -> Self {
        self.merge(Self::BigInt)
    }

    /// Record an unknown/mixed type.
    #[must_use]
    pub fn observe_any(self) -> Self {
        Self::Any
    }
}

/// Comparison feedback lattice.
/// `None → Int32 → Number → String → Any`
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(u8)]
pub enum ComparisonFeedback {
    #[default]
    None = 0,
    Int32 = 1,
    Number = 2,
    String = 3,
    Any = 4,
}

impl ComparisonFeedback {
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        if other > self { other } else { self }
    }

    #[must_use]
    pub fn observe_int32(self) -> Self {
        self.merge(Self::Int32)
    }
    #[must_use]
    pub fn observe_number(self) -> Self {
        self.merge(Self::Number)
    }
    #[must_use]
    pub fn observe_string(self) -> Self {
        self.merge(Self::String)
    }
    #[must_use]
    pub fn observe_any(self) -> Self {
        Self::Any
    }
}

/// Branch feedback: taken/not-taken saturating counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct BranchFeedback {
    /// Number of times the branch was taken (saturates at u16::MAX).
    pub taken: u16,
    /// Number of times the branch was not taken.
    pub not_taken: u16,
}

impl BranchFeedback {
    pub fn record_taken(&mut self) {
        self.taken = self.taken.saturating_add(1);
    }

    pub fn record_not_taken(&mut self) {
        self.not_taken = self.not_taken.saturating_add(1);
    }

    /// Bias toward taken (0.0 = always not-taken, 1.0 = always taken).
    #[must_use]
    pub fn taken_ratio(&self) -> f64 {
        let total = u32::from(self.taken) + u32::from(self.not_taken);
        if total == 0 {
            0.5
        } else {
            f64::from(self.taken) / total as f64
        }
    }
}

/// Property access feedback.
/// `Uninitialized → Monomorphic → Polymorphic → Megamorphic`
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum PropertyFeedback {
    #[default]
    Uninitialized,
    /// Single observed shape + slot offset.
    Monomorphic(PropertyInlineCache),
    /// 2-4 observed shapes.
    Polymorphic(Vec<PropertyInlineCache>),
    /// Too many shapes — use generic fallback.
    Megamorphic,
}

/// Maximum number of shapes before transitioning to megamorphic.
const MAX_POLYMORPHIC_SHAPES: usize = 4;

impl PropertyFeedback {
    /// Record an observed property access.
    pub fn observe(&mut self, shape_id: ObjectShapeId, slot_index: u16) {
        let cache = PropertyInlineCache::new(shape_id, slot_index);
        match self {
            Self::Uninitialized => {
                *self = Self::Monomorphic(cache);
            }
            Self::Monomorphic(existing) => {
                if *existing != cache {
                    *self = Self::Polymorphic(vec![*existing, cache]);
                }
            }
            Self::Polymorphic(shapes) => {
                if !shapes.contains(&cache) {
                    if shapes.len() >= MAX_POLYMORPHIC_SHAPES {
                        *self = Self::Megamorphic;
                    } else {
                        shapes.push(cache);
                    }
                }
            }
            Self::Megamorphic => {} // Terminal state.
        }
    }

    /// Whether this is monomorphic with a known shape.
    #[must_use]
    pub fn as_monomorphic(&self) -> Option<PropertyInlineCache> {
        match self {
            Self::Monomorphic(c) => Some(*c),
            _ => None,
        }
    }

    /// Whether we've seen too many shapes.
    #[must_use]
    pub fn is_megamorphic(&self) -> bool {
        matches!(self, Self::Megamorphic)
    }
}

/// Call target feedback.
/// `Uninitialized → Monomorphic → Polymorphic → Megamorphic`
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum CallFeedback {
    #[default]
    Uninitialized,
    /// Single observed call target (function index within module).
    Monomorphic(u32),
    /// 2-4 observed targets.
    Polymorphic(Vec<u32>),
    /// Too many targets.
    Megamorphic,
}

impl CallFeedback {
    /// Record an observed call target.
    pub fn observe(&mut self, target: u32) {
        match self {
            Self::Uninitialized => {
                *self = Self::Monomorphic(target);
            }
            Self::Monomorphic(existing) => {
                if *existing != target {
                    *self = Self::Polymorphic(vec![*existing, target]);
                }
            }
            Self::Polymorphic(targets) => {
                if !targets.contains(&target) {
                    if targets.len() >= MAX_POLYMORPHIC_SHAPES {
                        *self = Self::Megamorphic;
                    } else {
                        targets.push(target);
                    }
                }
            }
            Self::Megamorphic => {}
        }
    }

    #[must_use]
    pub fn as_monomorphic(&self) -> Option<u32> {
        match self {
            Self::Monomorphic(t) => Some(*t),
            _ => None,
        }
    }
}

// ============================================================
// FeedbackVector — mutable runtime data
// ============================================================

/// Runtime feedback data for a single slot.
#[derive(Debug, Clone)]
pub enum FeedbackSlotData {
    Arithmetic(ArithmeticFeedback),
    Comparison(ComparisonFeedback),
    Branch(BranchFeedback),
    Property(PropertyFeedback),
    Call(CallFeedback),
}

impl FeedbackSlotData {
    /// Create default slot data for a given kind.
    #[must_use]
    pub fn for_kind(kind: FeedbackKind) -> Self {
        match kind {
            FeedbackKind::Arithmetic => Self::Arithmetic(ArithmeticFeedback::None),
            FeedbackKind::Comparison => Self::Comparison(ComparisonFeedback::None),
            FeedbackKind::Branch => Self::Branch(BranchFeedback::default()),
            FeedbackKind::Property => Self::Property(PropertyFeedback::Uninitialized),
            FeedbackKind::Call => Self::Call(CallFeedback::Uninitialized),
        }
    }
}

/// Mutable feedback vector for a function — one per activation.
///
/// Sized to match the function's `FeedbackTableLayout`. Each slot
/// accumulates runtime observations as the interpreter executes.
#[derive(Debug, Clone)]
pub struct FeedbackVector {
    slots: Box<[FeedbackSlotData]>,
}

impl FeedbackVector {
    /// Create a feedback vector from a layout.
    #[must_use]
    pub fn from_layout(layout: &FeedbackTableLayout) -> Self {
        let slots: Vec<FeedbackSlotData> = layout
            .slots()
            .iter()
            .map(|s| FeedbackSlotData::for_kind(s.kind()))
            .collect();
        Self {
            slots: slots.into_boxed_slice(),
        }
    }

    /// Create an empty feedback vector.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            slots: Vec::new().into_boxed_slice(),
        }
    }

    /// Get slot data by ID.
    #[must_use]
    pub fn get(&self, id: FeedbackSlotId) -> Option<&FeedbackSlotData> {
        self.slots.get(usize::from(id.0))
    }

    /// Get mutable slot data by ID.
    pub fn get_mut(&mut self, id: FeedbackSlotId) -> Option<&mut FeedbackSlotData> {
        self.slots.get_mut(usize::from(id.0))
    }

    /// Number of slots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether the vector is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// All slots.
    #[must_use]
    pub fn slots(&self) -> &[FeedbackSlotData] {
        &self.slots
    }

    // ---- Convenience recording methods ----

    /// Record arithmetic feedback for a slot.
    pub fn record_arithmetic(&mut self, id: FeedbackSlotId, observed: ArithmeticFeedback) {
        if let Some(FeedbackSlotData::Arithmetic(fb)) = self.get_mut(id) {
            *fb = fb.merge(observed);
        }
    }

    /// Record comparison feedback for a slot.
    pub fn record_comparison(&mut self, id: FeedbackSlotId, observed: ComparisonFeedback) {
        if let Some(FeedbackSlotData::Comparison(fb)) = self.get_mut(id) {
            *fb = fb.merge(observed);
        }
    }

    /// Record a taken/not-taken branch observation.
    pub fn record_branch(&mut self, id: FeedbackSlotId, taken: bool) {
        if let Some(FeedbackSlotData::Branch(fb)) = self.get_mut(id) {
            if taken {
                fb.record_taken();
            } else {
                fb.record_not_taken();
            }
        }
    }

    /// Record a property access observation.
    pub fn record_property(
        &mut self,
        id: FeedbackSlotId,
        shape_id: ObjectShapeId,
        slot_index: u16,
    ) {
        if let Some(FeedbackSlotData::Property(fb)) = self.get_mut(id) {
            fb.observe(shape_id, slot_index);
        }
    }

    /// Record a call target observation.
    pub fn record_call(&mut self, id: FeedbackSlotId, target: u32) {
        if let Some(FeedbackSlotData::Call(fb)) = self.get_mut(id) {
            fb.observe(target);
        }
    }

    /// Get arithmetic feedback for a slot.
    #[must_use]
    pub fn arithmetic(&self, id: FeedbackSlotId) -> Option<ArithmeticFeedback> {
        match self.get(id)? {
            FeedbackSlotData::Arithmetic(fb) => Some(*fb),
            _ => None,
        }
    }

    /// Get property feedback for a slot.
    #[must_use]
    pub fn property(&self, id: FeedbackSlotId) -> Option<&PropertyFeedback> {
        match self.get(id)? {
            FeedbackSlotData::Property(fb) => Some(fb),
            _ => None,
        }
    }

    /// Get call feedback for a slot.
    #[must_use]
    pub fn call(&self, id: FeedbackSlotId) -> Option<&CallFeedback> {
        match self.get(id)? {
            FeedbackSlotData::Call(fb) => Some(fb),
            _ => None,
        }
    }

    /// Get branch feedback for a slot.
    #[must_use]
    pub fn branch(&self, id: FeedbackSlotId) -> Option<BranchFeedback> {
        match self.get(id)? {
            FeedbackSlotData::Branch(fb) => Some(*fb),
            _ => None,
        }
    }
}

impl Default for FeedbackVector {
    fn default() -> Self {
        Self::empty()
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_lattice_monotonic() {
        let mut fb = ArithmeticFeedback::None;
        fb = fb.observe_int32();
        assert_eq!(fb, ArithmeticFeedback::Int32);
        fb = fb.observe_number();
        assert_eq!(fb, ArithmeticFeedback::Number);
        // Can't go back down.
        fb = fb.observe_int32();
        assert_eq!(fb, ArithmeticFeedback::Number);
        fb = fb.observe_bigint();
        assert_eq!(fb, ArithmeticFeedback::BigInt);
    }

    #[test]
    fn comparison_lattice_monotonic() {
        let mut fb = ComparisonFeedback::None;
        fb = fb.observe_int32();
        assert_eq!(fb, ComparisonFeedback::Int32);
        fb = fb.observe_string();
        assert_eq!(fb, ComparisonFeedback::String);
        // Can't go back to Int32.
        fb = fb.observe_int32();
        assert_eq!(fb, ComparisonFeedback::String);
    }

    #[test]
    fn property_feedback_transitions() {
        let mut fb = PropertyFeedback::Uninitialized;
        let shape1 = ObjectShapeId(1);
        let shape2 = ObjectShapeId(2);
        let shape3 = ObjectShapeId(3);
        let shape4 = ObjectShapeId(4);
        let shape5 = ObjectShapeId(5);

        fb.observe(shape1, 0);
        assert!(fb.as_monomorphic().is_some());

        fb.observe(shape2, 1);
        assert!(matches!(fb, PropertyFeedback::Polymorphic(_)));

        fb.observe(shape3, 2);
        fb.observe(shape4, 3);
        assert!(matches!(fb, PropertyFeedback::Polymorphic(_)));

        fb.observe(shape5, 4);
        assert!(fb.is_megamorphic());

        // Terminal — stays megamorphic.
        fb.observe(shape1, 0);
        assert!(fb.is_megamorphic());
    }

    #[test]
    fn branch_feedback_ratio() {
        let mut fb = BranchFeedback::default();
        for _ in 0..3 {
            fb.record_taken();
        }
        for _ in 0..1 {
            fb.record_not_taken();
        }
        assert!((fb.taken_ratio() - 0.75).abs() < 0.01);
    }

    #[test]
    fn feedback_vector_from_layout() {
        let layout = FeedbackTableLayout::new(vec![
            FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Arithmetic),
            FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
            FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Branch),
        ]);
        let mut vec = FeedbackVector::from_layout(&layout);
        assert_eq!(vec.len(), 3);

        vec.record_arithmetic(FeedbackSlotId(0), ArithmeticFeedback::Int32);
        assert_eq!(
            vec.arithmetic(FeedbackSlotId(0)),
            Some(ArithmeticFeedback::Int32)
        );

        vec.record_property(FeedbackSlotId(1), ObjectShapeId(42), 3);
        assert!(
            vec.property(FeedbackSlotId(1))
                .unwrap()
                .as_monomorphic()
                .is_some()
        );

        vec.record_branch(FeedbackSlotId(2), true);
        assert_eq!(vec.branch(FeedbackSlotId(2)).unwrap().taken, 1);
    }
}
