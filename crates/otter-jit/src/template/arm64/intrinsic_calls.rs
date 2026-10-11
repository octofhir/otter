//! Identity-proven intrinsic calls over evaluated Template operands.
//!
//! # Contents
//! - Native primitive results and shared empty-literal LAB allocation.
//! - Source-stamped committed allocation misses and length probes.
//!
//! # Invariants
//! - Callable identity precedes every effect; misses retain the original call.
//! - Fits cannot collect and initialize the complete cell before publication.
//! - Collecting helpers publish the existing call source and window record.
//! - Probe misses precede effects; committed completion never replays a call.
//!
//! # See also
//! - [`crate::intrinsic_calls`] owns semantic admission for both tiers.
//! - [`crate::arm64::allocation`] owns the shared LAB recipe.

use super::values::{emit_load_reg, emit_load_symbol_u64, emit_load_u64, emit_store_reg};
use crate::allocation::{EmptyLiteralLayout, LabRegisters};
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::entry::{TransitionTable, Unsupported, VALUE_FALSE};
use crate::intrinsic_calls::IntrinsicCallForm;
use crate::template::{
    TemplateOp, operation::AllocPacket, operation::OperationExits, plan::TemplateInstr,
};
use dynasmrt::{DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{JitCompileSnapshot, native_abi as abi};

pub(super) fn emit_intrinsic_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    view: &JitCompileSnapshot,
    instruction: &TemplateInstr,
    packet: AllocPacket,
    exits: OperationExits,
) -> Result<(), Unsupported> {
    let TemplateOp::IntrinsicCall {
        dst,
        callee,
        argument,
        proof,
        form,
    } = instruction.op
    else {
        unreachable!("intrinsic call operation");
    };
    emit_load_reg(ops, 9, callee)?;
    emit_load_symbol_u64(
        ops,
        relocations,
        16,
        proof.identity_cell,
        RelocationTarget::CalleeIdentityCell {
            function_id: view.code_block.id,
            call_pc: instruction.pc,
        },
    );
    let identity_miss = exits.identity_guard_exit;
    dynasm!(ops ; .arch aarch64 ; ldr x16, [x16] ; cmp x9, x16 ; b.ne =>identity_miss);
    match form {
        IntrinsicCallForm::Boolean => {
            if let Some(argument) = argument {
                emit_load_reg(ops, 9, argument)?;
                super::emit_truthiness_bool(ops, relocations, exits.type_mismatch_exit);
            } else {
                emit_load_u64(ops, 9, VALUE_FALSE);
            }
            emit_store_reg(ops, 9, dst)?;
        }
        IntrinsicCallForm::NumberZero => {
            emit_load_u64(ops, 9, otter_vm::Value::number_i32(0).to_bits());
            emit_store_reg(ops, 9, dst)?;
        }
        IntrinsicCallForm::ArrayLength => {
            super::emit_cold_call_source(ops, instruction.pc, packet.safepoint);
            super::transitions::emit_array_length_alloc_call(
                ops,
                relocations,
                dst,
                argument.expect("length operation has an argument"),
                packet,
                exits.allocation_miss_exit,
            )?;
        }
        IntrinsicCallForm::ArrayEmpty | IntrinsicCallForm::ObjectEmpty => {
            let plans = &view.literal_allocations;
            let (layout, descriptor) = if form == IntrinsicCallForm::ArrayEmpty {
                (
                    Some(EmptyLiteralLayout::Array(plans.array)),
                    abi::STUB_JIT_NEW_ARRAY,
                )
            } else {
                (
                    plans.object.map(EmptyLiteralLayout::Object),
                    abi::STUB_JIT_NEW_OBJECT,
                )
            };
            let slow = ops.new_dynamic_label();
            let done = ops.new_dynamic_label();
            let regs = LabRegisters {
                buffer: 9,
                candidate: 10,
                end: 11,
                scratch: 12,
                size: 17,
            };
            if let Some(layout) = layout {
                crate::arm64::allocation::emit_empty_literal(
                    ops,
                    20,
                    proof.realm_id,
                    layout,
                    regs,
                    slow,
                );
                emit_store_reg(ops, regs.candidate, dst)?;
                dynasm!(ops ; .arch aarch64 ; b =>done);
            }
            dynasm!(ops ; .arch aarch64 ; =>slow);
            super::emit_cold_call_source(ops, instruction.pc, packet.safepoint);
            super::value_packet::emit_value_packet_transition(
                ops,
                relocations,
                transitions,
                descriptor,
                &[],
                Some(dst),
                exits.committed_throw,
                exits.fatal,
            )?;
            dynasm!(ops ; .arch aarch64 ; =>done);
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "../intrinsic_calls_tests.rs"]
mod tests;
