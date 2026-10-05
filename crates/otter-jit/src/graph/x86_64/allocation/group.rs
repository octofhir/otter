//! Fixed-shell group emission over canonical Graph recovery.
//!
//! # Contents
//! - One native group fit and one canonical noncharging admission Probe.
//! - Original source ids retain tagged initialized projections.
//!
//! # Invariants
//! Ensure success creates no result; every false/error restores homes then
//! eagerly resumes the first source member. Retrying the fit rereads LAB/policy
//! after GC. No result register is overwritten until complete publication.
//!
//! # See also
//! - `super::lexical` owns the shared Probe packet and full-word status decoder.

use super::*;

impl Codegen<'_> {
    pub(in crate::graph::x86_64) fn emit_allocation_group(
        &mut self,
        node: NodeId,
        index: u32,
    ) -> Result<(), crate::Unsupported> {
        let plan = &self.graph.allocation_groups[index as usize];
        let members = plan
            .members
            .iter()
            .map(|m| (m.byte, m.layout))
            .collect::<Vec<_>>();
        let bytes = plan.bytes;
        let realm = plan.realm;
        let a = self.allocation.node(node);
        let destination = Self::gp(a.result.expect("group result"));
        let regs = LabRegisters {
            buffer: a.gp_temps[0],
            candidate: a.gp_temps[1],
            end: a.gp_temps[2],
            scratch: a.gp_temps[3],
            size: 11,
        };
        let refill = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        crate::x86_64::allocation::emit_fixed_group(
            &mut self.ops,
            15,
            realm,
            &members,
            bytes,
            regs,
            refill,
        );
        dynasm!(self.ops ; .arch x64 ; mov Rq(destination),Rq(regs.candidate) ; jmp =>done ; =>refill);
        self.emit_probe_allocation(
            node,
            abi::STUB_ALLOC_GROUP_ENSURE,
            [
                AllocationValue::Constant(otter_vm::Value::number_i32(bytes as i32).to_bits()),
                AllocationValue::Constant(crate::entry::VALUE_UNDEFINED),
                AllocationValue::Constant(crate::entry::VALUE_UNDEFINED),
            ],
            None,
        )?;
        let miss = self.typed_exit(
            node,
            abi::ExitReason::AllocationMiss,
            abi::ExitAction::Resume,
        );
        crate::x86_64::allocation::emit_fixed_group(
            &mut self.ops,
            15,
            realm,
            &members,
            bytes,
            regs,
            miss,
        );
        dynasm!(self.ops ; .arch x64 ; mov Rq(destination),Rq(regs.candidate) ; =>done);
        Ok(())
    }
    pub(in crate::graph::x86_64) fn emit_allocation_projection(&mut self, node: NodeId, byte: u32) {
        let a = self.allocation.node(node);
        let source = Self::gp(a.inputs[0]);
        let destination = Self::gp(a.result.expect("tagged projection"));
        dynasm!(self.ops ; .arch x64 ; lea Rq(destination),[Rq(source)+byte as i32]);
    }
}
