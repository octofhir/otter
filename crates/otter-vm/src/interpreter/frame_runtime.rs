//! Per-frame transient state: property inline-cache vector sized to the
//! function's feedback table. Lives alongside an `Activation` but is only
//! touched by the interpreter.

use crate::bytecode::ProgramCounter;
use crate::feedback::{FeedbackKind, FeedbackSlotId};
use crate::module::Function;
use crate::object::PropertyInlineCache;

#[derive(Debug, Clone, PartialEq)]
pub(super) struct FrameRuntimeState {
    pub(super) property_feedback: Box<[Option<PropertyInlineCache>]>,
}

impl FrameRuntimeState {
    pub(super) fn new(function: &Function) -> Self {
        Self {
            property_feedback: vec![None; function.feedback().len()].into_boxed_slice(),
        }
    }

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
}
