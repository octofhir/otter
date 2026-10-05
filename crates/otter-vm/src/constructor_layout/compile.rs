//! Exact observed-family selection for generated receiver plans.
//!
//! # Contents
//! - Read-only finalized-head selection on actual closure/class GC owners.
//! - Borrowed caller ownership and lazy scalar observation at the exact source.
//!
//! # Invariants
//! No function-id alias or process-address key selects a family. The weak
//! identity index resolves it without a heap walk and cannot collect; only an
//! attached closure/class family with a finalized, bakeable root is returned. Scalar feedback is not a root or code-ID
//! census entry. Exact caller/inline attribution precedes the sole feedback
//! owner's terminal guard; only then may its noalloc observation read a layout.
//! Bound/proxy/native new.targets keep semantic family sampling
//! but use canonical receiver preparation with the current generated encoders.
//!
//! # See also
//! - `owner` publishes the exact actual-constructor head before allocation.
//! - `crate::interp::jit_compile` bakes immutable shapes from this selection.

use super::ConstructorLayout;
use crate::{ExecutionContext, Interpreter, VmError};

impl Interpreter {
    pub(crate) fn finalized_constructor_layout_for_id(
        &self,
        family_id: u64,
        base: u32,
    ) -> Option<(ConstructorLayout, u32, bool)> {
        if family_id == 0 {
            return None;
        }
        let layout = self.constructor_families.get(family_id)?;
        let (finalized, detached, layout_base, (owner, owner_fid)) =
            self.gc_heap.read_payload(layout, |body| {
                (
                    body.finalized(),
                    body.detached(),
                    body.base_function_id(),
                    body.owner(),
                )
            });
        if !finalized || detached || layout_base != base {
            return None;
        }
        match owner {
            super::ConstructorFamilyOwner::Class => Some((layout, owner_fid, true)),
            super::ConstructorFamilyOwner::Closure => Some((layout, owner_fid, false)),
            super::ConstructorFamilyOwner::Other => None,
        }
    }

    /// Called after rooted receiver preparation selected/finalized its exact
    /// family; `caller` is the suspended source frame, not the entered callee.
    /// The caller's safepoint recipe resolves inline and root source alike.
    pub(crate) fn record_prepared_construct_family(
        &self,
        context: Option<&ExecutionContext>,
        caller: &crate::native_abi::Frame,
        layout: ConstructorLayout,
    ) -> Result<(), VmError> {
        if caller.header.kind == crate::native_abi::NativeFrameKind::Host {
            return Ok(());
        }
        // The actual activation may have no source or an unrelated admitted
        // chunk. Borrow its context only inside this isolate's current space;
        // otherwise resolve the real caller owner through the canonical lookup.
        // No callee context or synthetic source participates in attribution.
        let owned;
        let context = match context {
            Some(context) if std::sync::Arc::ptr_eq(context.space(), &self.code_space) => context,
            _ => {
                owned = self.function_context(None, caller.header.function_id)?;
                &owned
            }
        };
        let (fid, pc) = crate::runtime_activation::semantic_source::frame_semantic_source(
            self, context, caller,
        )?;
        let owner = context
            .for_function(fid)
            .map_err(|_| VmError::InvalidOperand)?;
        let function = owner.exec_function(fid).ok_or(VmError::InvalidOperand)?;
        let instruction = function
            .instr_at_index(pc as usize)
            .ok_or(VmError::InvalidOperand)?;
        if !matches!(
            function.op(instruction),
            otter_bytecode::Op::New
                | otter_bytecode::Op::NewSpread
                | otter_bytecode::Op::SuperConstruct
                | otter_bytecode::Op::SuperConstructSpread
        ) {
            // A host/Reflect.construct body has no source allocation-plan slot.
            // Its actual GC-owned new.target still owns the family normally.
            return Ok(());
        }
        function.record_construct_family(pc as usize, || {
            if layout.is_null() {
                0
            } else {
                self.gc_heap.read_payload(
                    layout,
                    |body| if body.finalized() { body.family_id } else { 0 },
                )
            }
        });
        Ok(())
    }
}
