//! Native x86-64 Graph reads of canonical source-owned global bindings.
//!
//! # Contents
//! - Eligible lexical/object reads through the one shared binding emitter.
//! - Pre-effect WrongShape recovery at the actual owning source instruction.
//! - The source realm's global object behind the same realm guard.
//!
//! # Invariants
//! - `view_of(node)` selects an inlined body's own FID, byte PC and proof.
//! - Exactly two allocated GPs plus reserved r10/r11 implement the read.
//! - No call, safepoint, interior-pointer SSA value or source replay is added.
//! - The canonical eager recipe owns the complete inline chain and live homes;
//!   TDZ or changed lookup semantics resume before this guarded instruction.
//!
//! # See also
//! - `crate::x86_64::binding` owns the shared physical guards and banks.
//! - `super::exits` owns eager exit publication and recovery materialization.

use super::*;

impl Codegen<'_> {
    pub(super) fn emit_global_binding(
        &mut self,
        node: NodeId,
        byte_pc: u32,
    ) -> Result<(), Unsupported> {
        let allocation = self.loc(node).clone();
        let source = self.view_of(node);
        let proof = source
            .binding_hit_proofs
            .get(&byte_pc)
            .copied()
            .ok_or(Unsupported::OperandShape("Graph global binding proof"))?;
        let destination = Self::gp(allocation.result.expect("global binding result"));
        let temps = [allocation.gp_temps[0], allocation.gp_temps[1]];
        let miss = self.eager_exit(node, DeoptReason::WrongShape);
        crate::x86_64::binding::emit_global_read(
            &mut self.ops,
            &mut self.relocations,
            source,
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
    pub(super) fn emit_global_this(&mut self, node: NodeId) {
        let destination = Self::gp(self.loc(node).result.expect("global object result"));
        let source = self.view_of(node);
        let miss = self.eager_exit(node, DeoptReason::WrongValue);
        crate::x86_64::binding::emit_global_realm_guard(&mut self.ops, source, miss);
        dynasm!(self.ops ; .arch x64
            ; mov r10, [r15 + crate::entry::GLOBAL_THIS_OFFSET_PTR_OFFSET as i32]
            ; mov Rd(destination), [r10]
        );
        crate::x86_64::values::emit_load_symbol_u64(
            &mut self.ops,
            &mut self.relocations,
            11,
            source.cage_base as u64,
            RelocationTarget::GcCageBase,
        );
        dynasm!(self.ops ; .arch x64 ; add Rq(destination), r11);
    }
}
