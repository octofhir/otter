//! Graph tail calls through the shared physical activation transfer.
//!
//! # Contents
//! - Actual-only span reuse and cold staging when the outgoing span grows.
//!
//! # Invariants
//! - Admission touches only reserved scratch registers before operands move.
//! - A transfer retires this called frame and inherits its caller's return site;
//!   it creates no suspended caller or synthetic return association.
//! - Tier entries and interrupts resume the original tail instruction. A
//!   constructor retains its ordinary call and constructor completion.
//! - The growing-span helper roots its copied inputs before collection. It
//!   stages once, then the common trampoline enters the replacement activation.
//!
//! # See also
//! - `crate::arm64::frame` owns retirement and private JS tail transfer.

use super::*;
use crate::arm64::{frame, js_call::CallTarget};

impl Codegen<'_> {
    pub(super) fn emit_tail_call(
        &mut self,
        node: NodeId,
        plan: crate::call_linkage::CallPlan,
    ) -> Result<(), Unsupported> {
        let ordinary = self.ops.new_dynamic_label();
        let outgrown = self.ops.new_dynamic_label();
        let resume = self.typed_exit(
            node,
            abi::ExitReason::RuntimeTransition,
            abi::ExitAction::Resume,
        );
        let resume = self.cond_target(resume);
        frame::emit_tail_admission(&mut self.ops, resume, ordinary);
        let inputs = self.allocation.node(node).inputs.clone();
        let arguments = &inputs[1..];
        let bytes = crate::call_linkage::pushed_argument_bytes(arguments.len())?;
        frame::emit_tail_span_check(&mut self.ops, bytes / 8, outgrown);
        self.emit_push_actuals(arguments)?;
        let generic = self.ops.new_dynamic_label();
        if let crate::call_linkage::CallPlan::Bytecode(plan) = plan {
            self.emit_function_identity(
                plan.function_id,
                plan.callee_cell,
                self.graph.node(node).pc,
                1,
                generic,
            );
            dynasm!(self.ops ; .arch aarch64 ; mov x13, x1);
            frame::emit_tail_transfer(
                &mut self.ops,
                &mut self.relocations,
                self.transitions,
                bytes,
                arguments.len() as u32,
                CallTarget::Known {
                    entry_cell: plan.entry_cell,
                    function_id: plan.function_id,
                    receiver_ready: false,
                },
                self.spill.saved_pairs,
            );
        }
        dynasm!(self.ops ; .arch aarch64 ; =>generic ; mov x13, x1);
        frame::emit_tail_transfer(
            &mut self.ops,
            &mut self.relocations,
            self.transitions,
            bytes,
            arguments.len() as u32,
            CallTarget::Generic,
            self.spill.saved_pairs,
        );
        // Both branches reset SP while retiring the frame. The cold branch
        // and constructor continuation still begin at this node's original SP.
        self.sp_delta -= bytes;
        self.defer(move |codegen| {
            dynasm!(codegen.ops ; .arch aarch64 ; =>outgrown);
            codegen.emit_committed_span_call(node, abi::STUB_JIT_STAGE_TAIL_CALL, &inputs, 0)?;
            frame::emit_tail_return(&mut codegen.ops, codegen.spill.saved_pairs);
            Ok(())
        });
        dynasm!(self.ops ; .arch aarch64 ; =>ordinary);
        Ok(())
    }
}
