//! Inline constructor receivers over the shared nursery buffer.
//!
//! # Contents
//! - Live family/prototype proof of the new.target, then one LAB fit.
//! - The same proof alone for a receiver escape analysis removed.
//! - A collecting buffer refill and one retried fit on a space miss.
//!
//! # Invariants
//! Every failed proof precedes any effect and resumes the construct. The
//! proof is not repeated after the refill: the collector changes no family,
//! prototype or shape, and the fit reads only the plan and the context.
//! The cell is completely initialized before the buffer bump publishes it.
//! Refill and retry misses carry the receiver operation's exact exit reason.
//!
//! # See also
//! - `crate::x86_64::allocation::receiver` owns the proof and fit encoders.
//! - `super::group` owns the same refill boundary for literal shells.

use super::*;
use crate::graph::ir::DeoptReason;

impl Codegen<'_> {
    /// The proofs of `plan` with no allocation: a scalar-replaced receiver
    /// still needs its family and prototype.
    pub(in crate::graph::x86_64) fn emit_check_receiver_plan(
        &mut self,
        node: NodeId,
        plan: otter_vm::jit::JitReceiverAllocationPlan,
    ) {
        let view = self.view_of(node);
        let guard_miss = self.eager_exit(node, DeoptReason::WrongValue);
        crate::x86_64::allocation::emit_receiver_guards(
            &mut self.ops,
            &mut self.relocations,
            view,
            plan,
            guard_miss,
        );
    }

    pub(in crate::graph::x86_64) fn emit_new_receiver(
        &mut self,
        node: NodeId,
        plan: otter_vm::jit::JitReceiverAllocationPlan,
    ) -> Result<(), crate::Unsupported> {
        let view = self.view_of(node);
        let guard_miss = self.eager_exit(node, DeoptReason::WrongValue);
        crate::x86_64::allocation::emit_receiver_guards(
            &mut self.ops,
            &mut self.relocations,
            view,
            plan,
            guard_miss,
        );
        let refill = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        crate::x86_64::allocation::emit_receiver_fit(&mut self.ops, view, plan, 15, refill);
        crate::x86_64::allocation::emit_receiver_bump(&mut self.ops);
        dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>refill);
        let bytes = view
            .field_layout
            .cell_bytes(usize::from(plan.inline_capacity)) as i32;
        self.emit_probe_allocation(
            node,
            abi::STUB_ALLOC_GROUP_ENSURE,
            [
                AllocationValue::Constant(otter_vm::Value::number_i32(bytes).to_bits()),
                AllocationValue::Constant(crate::entry::VALUE_UNDEFINED),
                AllocationValue::Constant(crate::entry::VALUE_UNDEFINED),
            ],
            None,
        )?;
        let miss = self.typed_exit(
            node,
            crate::graph::frame::allocation_exit_reason(&self.graph.node(node).kind),
            abi::ExitAction::Resume,
        );
        crate::x86_64::allocation::emit_receiver_fit(&mut self.ops, view, plan, 15, miss);
        crate::x86_64::allocation::emit_receiver_bump(&mut self.ops);
        dynasm!(self.ops ; .arch x64 ; =>done);
        Ok(())
    }
}
