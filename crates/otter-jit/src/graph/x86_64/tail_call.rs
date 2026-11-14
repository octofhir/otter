//! Graph tail calls through the shared physical activation transfer.
//!
//! # Contents
//! - Actual-only span reuse and cold staging when the outgoing span grows.
//!
//! # Invariants
//! - Admission preserves operands by touching only reserved scratch registers.
//! - Tail transfers inherit the caller's source/return anchor without a CALL
//!   or a new return association; constructors keep ordinary completion.
//! - Tier entries and interrupts resume the original instruction. Growing
//!   spans use the common rooted staging boundary once before retirement.
//!
//! # See also
//! - `crate::x86_64::frame` owns physical retirement and private JS transfers.

use super::*;
use crate::x86_64::{frame, js_call::CallTarget};

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
        frame::emit_tail_admission(&mut self.ops, resume, ordinary);
        let inputs = self.loc(node).inputs.clone();
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
                6,
                generic,
            );
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
                abi::NativeFrameKind::Optimizing,
            );
        }
        dynasm!(self.ops ; .arch x64 ; =>generic);
        frame::emit_tail_transfer(
            &mut self.ops,
            &mut self.relocations,
            self.transitions,
            bytes,
            arguments.len() as u32,
            CallTarget::Generic,
            abi::NativeFrameKind::Optimizing,
        );
        self.sp_delta -= bytes;
        self.defer(move |codegen| {
            dynasm!(codegen.ops ; .arch x64 ; =>outgrown);
            codegen.emit_committed_span_call(node, abi::STUB_JIT_STAGE_TAIL_CALL, &inputs, 0)?;
            frame::emit_tail_return(&mut codegen.ops, abi::NativeFrameKind::Optimizing);
            Ok(())
        });
        dynasm!(self.ops ; .arch x64 ; =>ordinary);
        Ok(())
    }
}
