//! Per-frame transient state: feedback vector + property inline-cache vector.
//!
//! Lives alongside an `Activation` but is only touched by the interpreter.
//! The `FeedbackVector` accumulates runtime type/shape/branch observations
//! that the JIT consumes for speculative compilation.

use crate::bytecode::ProgramCounter;
use crate::feedback::{
    ArithmeticFeedback, ComparisonFeedback, FeedbackKind, FeedbackSlotId, FeedbackVector,
};
use crate::module::Function;
use crate::object::{ObjectShapeId, PropertyInlineCache};

#[derive(Debug, Clone)]
#[allow(dead_code)] // New feedback methods wired incrementally in dispatch.
pub(super) struct FrameRuntimeState {
    /// Legacy per-instruction property IC (used by existing JIT path).
    pub(super) property_feedback: Box<[Option<PropertyInlineCache>]>,
    /// Full feedback vector with monotonic lattices for all slot kinds.
    pub(super) feedback_vector: FeedbackVector,
}

impl FrameRuntimeState {
    pub(super) fn new(function: &Function) -> Self {
        Self {
            property_feedback: vec![None; function.feedback().len()].into_boxed_slice(),
            feedback_vector: FeedbackVector::from_layout(function.feedback()),
        }
    }

    // ---- Legacy property IC (unchanged) ----

    pub(super) fn property_cache(
        &self,
        function: &Function,
        pc: ProgramCounter,
    ) -> Option<PropertyInlineCache> {
        let index = Self::property_feedback_index(function, pc)?;
        self.property_feedback[index]
    }

    pub(super) fn update_property_cache(
        &mut self,
        function: &Function,
        pc: ProgramCounter,
        cache: PropertyInlineCache,
    ) {
        let Some(index) = Self::property_feedback_index(function, pc) else {
            return;
        };
        self.property_feedback[index] = Some(cache);
    }

    pub(super) fn property_feedback_index(
        function: &Function,
        pc: ProgramCounter,
    ) -> Option<usize> {
        let slot = FeedbackSlotId(u16::try_from(pc).ok()?);
        let layout = function.feedback().get(slot)?;
        (layout.kind() == FeedbackKind::Property).then_some(usize::from(slot.0))
    }

    // ---- New feedback recording (monotonic lattices) ----

    /// Record arithmetic feedback for the instruction at `pc`.
    /// Only records if the slot at this PC is an Arithmetic kind.
    #[allow(dead_code)]
    pub(super) fn record_arithmetic(
        &mut self,
        function: &Function,
        pc: ProgramCounter,
        observed: ArithmeticFeedback,
    ) {
        if let Some(slot) = Self::feedback_slot_of_kind(function, pc, FeedbackKind::Arithmetic) {
            self.feedback_vector.record_arithmetic(slot, observed);
        }
    }

    /// Record comparison feedback.
    #[allow(dead_code)]
    pub(super) fn record_comparison(
        &mut self,
        function: &Function,
        pc: ProgramCounter,
        observed: ComparisonFeedback,
    ) {
        if let Some(slot) = Self::feedback_slot_of_kind(function, pc, FeedbackKind::Comparison) {
            self.feedback_vector.record_comparison(slot, observed);
        }
    }

    /// Record branch taken/not-taken feedback.
    #[allow(dead_code)]
    pub(super) fn record_branch(&mut self, function: &Function, pc: ProgramCounter, taken: bool) {
        if let Some(slot) = Self::feedback_slot_of_kind(function, pc, FeedbackKind::Branch) {
            self.feedback_vector.record_branch(slot, taken);
        }
    }

    /// Record property access feedback (shape + slot).
    #[allow(dead_code)]
    pub(super) fn record_property(
        &mut self,
        function: &Function,
        pc: ProgramCounter,
        shape_id: ObjectShapeId,
        slot_index: u16,
    ) {
        if let Some(slot) = Self::feedback_slot_of_kind(function, pc, FeedbackKind::Property) {
            self.feedback_vector
                .record_property(slot, shape_id, slot_index);
        }
    }

    /// Record call target feedback.
    #[allow(dead_code)]
    pub(super) fn record_call(&mut self, function: &Function, pc: ProgramCounter, target: u32) {
        if let Some(slot) = Self::feedback_slot_of_kind(function, pc, FeedbackKind::Call) {
            self.feedback_vector.record_call(slot, target);
        }
    }

    /// Get the feedback slot ID for a given PC if it matches the expected kind.
    #[allow(dead_code)]
    fn feedback_slot_of_kind(
        function: &Function,
        pc: ProgramCounter,
        expected_kind: FeedbackKind,
    ) -> Option<FeedbackSlotId> {
        let slot = FeedbackSlotId(u16::try_from(pc).ok()?);
        let layout = function.feedback().get(slot)?;
        (layout.kind() == expected_kind).then_some(slot)
    }

    /// Get a reference to the full feedback vector (for JIT consumption).
    #[allow(dead_code)]
    pub(super) fn feedback(&self) -> &FeedbackVector {
        &self.feedback_vector
    }
}
