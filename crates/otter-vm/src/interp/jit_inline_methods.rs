//! Descriptor-checked receiver fields for Template method inlining.
//!
//! # Contents
//! - `bake_method_inline_fields` proves Template field access for the one
//!   prepared body owned by a canonical method target.
//! - Own receiver fields and independently guarded non-receiver fields.
//!
//! # Invariants
//! - A field name alone never authorizes a direct property-word access.
//! - Receiver loads require an own data descriptor; stores also require writable.
//! - Accessors keep their committed invocation, including getter/setter effects.
//! - Every retained bank location belongs to its exact guarded storage shape.
//! - This pass neither prepares another body nor duplicates its identity guard.
//!
//! # See also
//! - `super` owns source snapshots, method guards and compile roots.
//! - `crate::object::shape_body` owns immutable ordinary descriptors.
//! - `otter_jit::template::inline_leaf` validates scratch and receiver use.

use super::{Baker, ExecutionContext, Op, PolyMethodTarget, jit};

impl Baker<'_> {
    /// Prove the prepared body's property access for this exact guarded
    /// receiver. Mono and poly targets use the same descriptor checks; refusal
    /// removes only Template's field proof, leaving the target's call plan and
    /// Graph body intact.
    pub(super) fn bake_method_inline_fields(
        &mut self,
        context: &ExecutionContext,
        target: &PolyMethodTarget,
        method_view: &jit::JitCompileSnapshot,
    ) -> Option<jit::JitMethodInlineFields> {
        let method_context = context.for_function(target.method_fid).ok()?;
        let context = &*method_context;
        let method = context.exec_function(target.method_fid)?;
        if method.is_generator
            || method.is_async
            || method.is_async_generator
            || method.requires_argument_frame()
            || method.has_rest
            || method.contains_direct_eval
            || method.is_derived_constructor
            || method.makes_function
        {
            return None;
        }
        // Resolve every body `LoadProperty`/`StoreProperty` to a logical own
        // field; bail out if a property is absent or an accessor. The shape
        // fixes its bank and relative index. A receiver property resolves against
        // the identity-guarded receiver shape (no per-op guard); a non-receiver
        // property falls back to its own monomorphic site feedback and records
        // the shape the inliner must guard. Loads carry the name at operand 2,
        // stores at operand 1.
        let mut prop_fields: rustc_hash::FxHashMap<u32, crate::object::FieldLocation> =
            rustc_hash::FxHashMap::default();
        let mut prop_shapes: rustc_hash::FxHashMap<u32, u32> = rustc_hash::FxHashMap::default();
        for instr in &method_view.instructions {
            let name_operand = match instr.op(&method_view.code_block) {
                Op::LoadProperty => 2,
                Op::StoreProperty => 1,
                _ => continue,
            };
            let otter_bytecode::Operand::ConstIndex(name_idx) =
                instr.operand(&method_view.code_block, name_operand)?
            else {
                return None;
            };
            let key = context.property_atom(name_idx)?;
            let recv_shape = self.shape_for_id(target.recv_shape)?;
            if let Some(slot) = self.env.shape_offset_of(recv_shape, key.name()) {
                let (flags, is_accessor) =
                    crate::object::shape_body::shape_slot_attrs(&self.env.heap, recv_shape, slot)?;
                if is_accessor
                    || (instr.op(&method_view.code_block) == Op::StoreProperty && !flags.writable())
                {
                    return None;
                }
                prop_fields.insert(
                    instr.byte_pc,
                    crate::object::field_location(recv_shape, slot),
                );
                continue;
            }
            // Not a receiver property: use the op's own monomorphic own-data site
            // feedback (shape offset, slot byte). Anything else — polymorphic,
            // prototype, accessor, or unobserved — is not inlinable.
            let (shape_off, field) =
                self.monomorphic_own_property_feedback(context, &method_view.code_block, instr)?;
            prop_fields.insert(instr.byte_pc, field);
            prop_shapes.insert(instr.byte_pc, shape_off);
        }
        Some(jit::JitMethodInlineFields {
            prop_fields,
            prop_shapes,
        })
    }
}
