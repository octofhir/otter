//! Admission of a deeper entry Graph from call sites reached in compact code.
//!
//! # Contents
//! A transition to zero wakes the normal mutator poll. It submits a replacement
//! for each funded current generation while that generation continues serving
//! callers.
//!
//! # Invariants
//! Only the owning realm can submit. The existing compiler queue, retraining,
//! source leases, install proofs and physical code budget apply to replacement.
//! Suspended frames and their GC and deopt records stay with their generation.
//!
//! # See also
//! [`crate::native_abi::SourceWork`] owns the budget across generations.

use crate::{Interpreter, tier_policy::CostedTier};

impl Interpreter {
    pub(super) fn expand_funded_inline_calls(&mut self) {
        let ready: Vec<_> = self
            .jit_optimized_code
            .iter()
            .filter_map(|(&fid, slot)| {
                let code = slot.as_ref()?;
                let budget = code.inline_call_budget()?;
                (self.jit_code_registry.is_current_for_entry(code.as_ref())
                    && budget.claim_inline_calls())
                .then_some(fid)
            })
            .collect();
        for fid in ready {
            let Ok(owner) = self.function_context(None, fid) else {
                continue;
            };
            if self.jit_compile_queue.is_pending(fid) || self.foreign_function_realm(fid).is_some()
            {
                self.rearm_inline_expansion(fid);
                continue;
            }
            if self.jit_retraining_blocks(fid) {
                continue;
            }
            let headroom = self.jit_code_registry.available_code_bytes();
            let Some(decision) =
                self.jit_tier_work_decision(&owner, fid, CostedTier::Optimizing, headroom)
            else {
                continue;
            };
            if decision.resource_blocked() {
                if let Some(function) = owner.exec_function(fid) {
                    function.source_work().defer_inline_calls_for_pressure();
                }
                continue;
            }
            self.compile_optimized_jit_function_session(&owner, fid, None);
        }
    }

    /// A compiler that yielded to GC or a changing fact left the compact body
    /// current. One more reached site can request a fresh snapshot.
    pub(super) fn rearm_inline_expansion(&self, fid: u32) {
        if let Some(work) = self
            .jit_optimized_code
            .get(&fid)
            .and_then(Option::as_ref)
            .and_then(|code| code.inline_call_budget())
        {
            work.retry_inline_calls();
        }
    }
}
