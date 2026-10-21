//! Search completion from a proven, complete literal.
//!
//! # Contents
//! - Direct range publication from the existing width-preserving scan.
//! - Exact candidate work accounting for leftmost and sticky searches.
//!
//! # Invariants
//! - Admission proves a nonempty, untruncated, capture-free code-unit literal.
//! - A leftmost occurrence proves both endpoints without another comparison.
//! - A sticky search considers only its position. A prefilter-positive attempt
//!   consumes one step even when the remaining literal fails, as in both tiers.
//! - No matcher stack, undo log or executable mapping is needed.
//!
//! # See also
//! - [`crate::program::RequiredLiteral::for_complete_program`] owns admission.
//! - `Regex::find_on_tier` bypasses completion for differential tests.

use super::*;

impl<U: Unit> Matches<'_, '_, U> {
    pub(super) fn advance_literal(
        &mut self,
        literal: &RequiredLiteral,
    ) -> Option<Result<(), ExecError>> {
        let at = if self.anchored {
            if !self.may_start_at(self.next_start) {
                self.done = true;
                return None;
            }
            self.next_start
        } else {
            let Some(at) = literal.find(self.text, self.next_start) else {
                self.done = true;
                return None;
            };
            at
        };
        self.steps_executed = self.steps_executed.saturating_add(1);
        if self.steps_executed > self.config.step_limit {
            self.done = true;
            return Some(Err(ExecError::StepLimitExceeded));
        }
        let end = at + literal.units().len();
        if self.anchored && self.text.get(..end).and_then(|text| literal.find(text, at)) != Some(at)
        {
            self.done = true;
            return None;
        }
        self.scratch.literal = [at as u64, end as u64];
        self.next_start = end;
        Some(Ok(()))
    }
}
