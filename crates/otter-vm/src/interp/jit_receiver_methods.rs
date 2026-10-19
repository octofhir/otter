//! Methods a constructor's in-place receiver inherits before it owns the key.
//!
//! # Contents
//! - [`ConstructReceiver`] names the receiver a `new` site allocates in place
//!   by its initial hidden class.
//! - `Interpreter::bake_receiver_methods` gives a constructor body spliced at
//!   such a site a direct method for each named read or method call whose key
//!   that receiver inherits as a closure, where the site has none of its own.
//!
//! # Invariants
//! - Each method carries the ordinary method guard: the receiver's hidden
//!   class, the chain proof of its prototype and the holder slot. A write to
//!   any link of the chain retires the proof.
//! - A site's own method feedback is never replaced or extended.
//! - Generated code reads such a method only for a receiver proved to have
//!   the initial hidden class; any other receiver takes the site's ordinary
//!   path.
//! - Every lookup completes before the first allocation of the bake, so no
//!   heap handle is held across one.
//!
//! # See also
//! - `crate::cache_ir::resolve_inherited_atom_data_slot` for the lookup.
//! - `super::Interpreter::bake_one_direct_method` for the guard, plan and body.

use super::InlineSnapshotBudget;
use crate::object::ShapeId;
use crate::object::shape_body::{self, ShapePrototype};
use crate::{
    ExecutionContext, Interpreter, MethodLookupProof, Op, PolyMethodTarget, jit, jit_debug,
};

/// The receiver a `new` site allocates in place, by the stable id of its
/// initial hidden class.
#[derive(Debug, Clone, Copy)]
pub(super) struct ConstructReceiver {
    pub(super) shape: ShapeId,
}

impl Interpreter {
    /// Give `view`, a constructor body spliced where `receiver` is its
    /// `this`, a direct method for every named read and method call whose key
    /// the receiver inherits as a closure and which has no method feedback.
    pub(super) fn bake_receiver_methods(
        &mut self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        receiver: ConstructReceiver,
        tier: jit_debug::JitDebugTier,
        budget: &mut InlineSnapshotBudget,
    ) {
        let function_id = view.code_block.id;
        let sites: Vec<(u32, u32)> = view
            .instructions
            .iter()
            .filter(|instr| {
                matches!(
                    instr.op(&view.code_block),
                    Op::LoadProperty | Op::CallMethodValue
                ) && !view.direct_methods.contains_key(&instr.byte_pc)
            })
            .filter_map(|instr| Some((instr.byte_pc, instr.const_index(&view.code_block, 2)?)))
            .collect();
        let targets: Vec<(u32, PolyMethodTarget)> = sites
            .into_iter()
            .filter_map(|(byte_pc, name)| {
                self.receiver_method_target(context, function_id, name, receiver)
                    .map(|target| (byte_pc, target))
            })
            .collect();
        for (byte_pc, target) in targets {
            if let Ok(method) = self.bake_one_direct_method(
                context,
                &target,
                0,
                1,
                tier,
                budget,
                None,
                view.call_frequencies.get(&byte_pc).copied(),
            ) {
                view.direct_methods.insert(byte_pc, vec![method]);
            }
        }
    }

    /// The method a receiver whose hidden class is `receiver.shape` reads
    /// for constant `name` of `function_id`: an inherited data slot holding
    /// a closure, under the chain proof of the class's prototype.
    fn receiver_method_target(
        &self,
        context: &ExecutionContext,
        function_id: u32,
        name: u32,
        receiver: ConstructReceiver,
    ) -> Option<PolyMethodTarget> {
        let key = context.property_atom_for_function(function_id, name)?;
        let shape = self.shape_runtime.handle_for_id(receiver.shape)?;
        if self.shape_slot_of_atom(shape, key.atom().id()).is_some() {
            return None;
        }
        let ShapePrototype::Object(prototype) = shape_body::prototype_of(shape) else {
            return None;
        };
        let resolved =
            crate::cache_ir::resolve_inherited_atom_data_slot(prototype, &self.gc_heap, key)?;
        if resolved.hit.shape.is_null() || resolved.holder_root.is_null() {
            return None;
        }
        let method_fid = resolved.value.as_closure(&self.gc_heap)?.function_id();
        self.shape_runtime
            .register_shape(&self.gc_heap, resolved.holder_root);
        Some(PolyMethodTarget {
            method_fid,
            recv_shape: receiver.shape,
            prototype: MethodLookupProof {
                validity: resolved.validity,
                holder_root: resolved.holder_root_id,
            },
            method_field: crate::object::field_location(
                resolved.hit.shape,
                u32::from(resolved.hit.slot),
            ),
            hits: 1,
        })
    }
}
