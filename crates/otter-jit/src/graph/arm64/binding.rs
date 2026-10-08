//! Native Graph global binding reads with exact eager source recovery.
//!
//! # Contents
//! - One source-body proof lookup and shared physical read.
//! - The source realm's global object behind the same realm guard.
//! - The existing WrongShape eager exit for TDZ or revoked physical facts.
//!
//! # Invariants
//! The builder admitted the original read opcode and this body's proof. The
//! producer owns realm eligibility; an inlined source never substitutes an
//! ambient context. Only the result, two allocated GPs and x16/x17 change.
//! Recovery precedes the original read and reconstructs every inline frame;
//! no property lookup, getter, declaration or argument is replayed.
//!
//! # See also
//! - `crate::arm64::binding` owns the one native physical probe.
//! - `super::super::builder` owns semantic admission and frame states.

use super::*;

impl Codegen<'_> {
    pub(super) fn emit_global_binding(
        &mut self,
        node: NodeId,
        byte_pc: u32,
        destination: u8,
        temps: [u8; 2],
    ) -> Result<(), Unsupported> {
        let miss = self.eager_exit(node, DeoptReason::WrongShape);
        let miss = self.cond_target(miss);
        let view = metadata::source_view(self.view, self.inline_views, self.graph, node);
        let proof = view
            .binding_hit_proofs
            .get(&byte_pc)
            .copied()
            .ok_or(Unsupported::OperandShape("Graph source binding proof"))?;
        crate::arm64::binding::emit_global_read(
            &mut self.ops,
            &mut self.relocations,
            view,
            proof,
            byte_pc,
            destination,
            temps,
            miss,
        )?;
        Ok(())
    }

    /// The source realm's global object, read through the active realm's
    /// rooted compressed handle once the realm is proved active.
    pub(super) fn emit_global_this(&mut self, node: NodeId, destination: u8) {
        let miss = self.eager_exit(node, DeoptReason::WrongValue);
        let miss = self.cond_target(miss);
        let view = metadata::source_view(self.view, self.inline_views, self.graph, node);
        crate::arm64::binding::emit_global_realm_guard(&mut self.ops, view, miss);
        dynasm!(self.ops ; .arch aarch64
            ; ldr x16, [x20, crate::entry::GLOBAL_THIS_OFFSET_PTR_OFFSET]
            ; ldr w16, [x16]
        );
        emit_load_symbol_u64(
            &mut self.ops,
            &mut self.relocations,
            destination,
            view.cage_base as u64,
            RelocationTarget::GcCageBase,
        );
        dynasm!(self.ops ; .arch aarch64 ; add X(destination), X(destination), x16);
    }
}
