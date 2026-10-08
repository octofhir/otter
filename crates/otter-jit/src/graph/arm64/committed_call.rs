//! One canonical collecting call boundary over SSA values and scalar arguments.
//!
//! # Contents
//! - Bounded argument recipes with explicit address relocation ownership.
//! - Save/expire/publish/call/status/restore for committed runtime entries.
//!
//! # Invariants
//! Every Value argument is staged before ABI registers are overwritten. Live
//! SSA values are saved to canonical homes before any collecting transition.
//! The unproduced result is written only after preserved registers return.
//!
//! # See also
//! `allocation` uses null span/count scalars without an interpreter window.

use super::{
    Codegen, CommittedArgument, Location, NodeId, RelocationTarget, abi, emit_load_symbol_u64,
};
use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::native_abi::NativeResultStatus;

impl Codegen<'_> {
    pub(super) fn emit_committed_call(
        &mut self,
        node: NodeId,
        stub: abi::RuntimeStubDescriptor,
        arguments: &[CommittedArgument],
        destination: Option<u8>,
    ) -> Result<(), crate::Unsupported> {
        self.emit_committed_call_inner(node, stub, arguments, destination, 0)
    }

    pub(super) fn emit_committed_span_call(
        &mut self,
        node: NodeId,
        stub: abi::RuntimeStubDescriptor,
        values: &[Location],
        destination: u8,
    ) -> Result<(), crate::Unsupported> {
        let bytes = self.emit_push_actuals(values)?;
        self.emit_committed_call_inner(
            node,
            stub,
            &[
                CommittedArgument::StackAddress(0),
                CommittedArgument::Scalar(values.len() as u64),
            ],
            Some(destination),
            bytes,
        )
    }

    fn emit_committed_call_inner(
        &mut self,
        node: NodeId,
        stub: abi::RuntimeStubDescriptor,
        arguments: &[CommittedArgument],
        destination: Option<u8>,
        span_bytes: u32,
    ) -> Result<(), crate::Unsupported> {
        assert!(arguments.len() <= 3, "bounded committed ABI arguments");
        assert!(
            arguments
                .iter()
                .filter(|arg| matches!(arg, CommittedArgument::Value(_)))
                .count()
                <= 2
        );
        let live = self.allocation.node(node).live_homes.clone();
        for &(location, home) in &live {
            if !matches!(home, Location::Constant(_)) {
                self.emit_move(location, home);
            }
        }
        self.stamp_node_safepoint(node)?;
        self.load_word32(16, self.graph.outer_pc(node));
        dynasm!(self.ops ; .arch aarch64 ; str w16, [x21, crate::entry::NATIVE_FRAME_PC_OFFSET]);
        let mut staged = 0u8;
        for argument in arguments {
            if let CommittedArgument::Value(value) = *argument {
                dynasm!(self.ops ; .arch aarch64 ; mov X(16 + staged), X(value));
                staged += 1;
            }
        }
        dynasm!(self.ops ; .arch aarch64 ; mov x0, x20);
        staged = 0;
        for (index, argument) in arguments.iter().enumerate() {
            let register = 1 + index as u8;
            match argument {
                CommittedArgument::Value(_) => {
                    dynasm!(self.ops ; .arch aarch64 ; mov X(register), X(16 + staged));
                    staged += 1;
                }
                CommittedArgument::Scalar(bits) => self.load_immediate(register, *bits),
                CommittedArgument::Address(bits, target) => emit_load_symbol_u64(
                    &mut self.ops,
                    &mut self.relocations,
                    register,
                    *bits,
                    target.clone(),
                ),
                CommittedArgument::StackAddress(offset) => {
                    dynasm!(self.ops ; .arch aarch64 ; add XSP(register), sp, *offset);
                }
            }
        }
        emit_load_symbol_u64(
            &mut self.ops,
            &mut self.relocations,
            16,
            self.transitions.entry(stub),
            RelocationTarget::runtime_stub(stub),
        );
        let (_, committed_throw) = self.throw_targets(node);
        let committed_throw = self.cond_target(committed_throw);
        let fatal = self.cond_target(self.fatal);
        let error = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        dynasm!(self.ops ; .arch aarch64 ; blr x16);
        // The stub copies the boxed span before it may collect. No stale
        // packet is read afterward; restore canonical SP before status routing.
        self.emit_pop_actuals(span_bytes);
        dynasm!(self.ops ; .arch aarch64 ; cbnz x1, =>error);
        dynasm!(self.ops ; .arch aarch64 ; str x0, [sp, #-16]!);
        self.sp_delta += 16;
        for &(location, home) in &live {
            self.emit_move(home, location);
        }
        match destination {
            Some(destination) => dynasm!(self.ops ; .arch aarch64 ; ldr X(destination), [sp], #16),
            None => dynasm!(self.ops ; .arch aarch64 ; add sp, sp, #16),
        }
        self.sp_delta -= 16;
        dynasm!(self.ops
            ; .arch aarch64
            ; b =>done
            ; =>error
            ; cmp x1, NativeResultStatus::Throw as u32
            ; b.ne =>fatal
            ; b =>committed_throw
            ; =>done
        );
        Ok(())
    }
}
