//! Argument forwarding: `callee.apply(this, arguments)` over an elided
//! `arguments` object.
//!
//! # Contents
//! - `CallForwardArguments` whose method is `%Function.prototype.apply%` and
//!   whose activation has no arguments object copies the activation's
//!   actual arguments, mapped formals read from the window, into a span and
//!   calls the callee through the generic entry.
//! - Any other method, a materialized arguments object, or a span past the
//!   stack limit stages the complete request from the resolved `apply`,
//!   callee, receiver and current argument bindings, then enters the common
//!   call trampoline.
//!
//! # Invariants
//! - The span is filled before the call and nothing allocates between its
//!   reservation and the call.
//! - The staging entry resolves the intrinsic `apply` to the activation's
//!   actual arguments (mapped formals refreshed from the packet) and any other
//!   method to a call with the arguments object; it never calls.
//! - The request is complete before the trampoline is entered; nothing
//!   allocates in between.
//!
//! # See also
//! - [`super::calls::emit_call_completion`] — the shared completion routing.

use crate::call_linkage::ForwardedBinding;
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{JitCompileSnapshot, native_abi as abi};

use super::{
    calls,
    value_packet::{PacketWord, emit_value_packet_transition},
    values::{emit_load_reg, emit_load_u64},
};
use crate::{
    arm64::js_call::{CallTarget, emit_call, emit_pop_forwarded, emit_push_forwarded},
    artifact::{CodeMapCapture, CodeRegion, relocation::RelocationCapture},
    entry::{TransitionTable, Unsupported, reg_offset},
};

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_forward_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    return_sites: &mut crate::return_sites::ReturnSiteRecorder<'_>,
    view: &JitCompileSnapshot,
    code_map: Option<&mut CodeMapCapture>,
    logical_pc: u32,
    [dst, method, callee, receiver]: [u16; 4],
    threw: DynamicLabel,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let byte_pc = view
        .instructions
        .get(logical_pc as usize)
        .ok_or(Unsupported::OperandShape("forward call instruction PC"))?
        .byte_pc;
    let start = ops.offset().0;
    let staged = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    if let Some(apply) = view.forward_apply_native_ref {
        // The method is `%Function.prototype.apply%`, and no arguments
        // object stands for the activation's actuals.
        emit_load_reg(ops, 9, method)?;
        emit_load_u64(ops, 16, otter_vm::value::tag::NOT_CELL_MASK);
        dynasm!(ops
            ; .arch aarch64
            ; tst x9, x16
            ; b.ne =>staged
            ; cbz x9, =>staged
            ; ldrb w16, [x9]
            ; cmp w16, u32::from(view.collection_layout.native_function_type_tag)
            ; b.ne =>staged
            ; ldr w16, [x9, view.native_call_layout.identity_byte]
        );
        emit_load_u64(ops, 17, u64::from(apply));
        dynasm!(ops
            ; .arch aarch64
            ; cmp w16, w17
            ; b.ne =>staged
            ; ldr w16, [x21, abi::NATIVE_FRAME_ARGUMENTS_OBJECT_OFFSET]
            ; cbnz w16, =>staged
        );
        let slots_byte = view.context_layout.slots_byte;
        let bindings = view
            .code_block
            .forwarded_argument_bindings()
            .map(|(index, storage)| {
                Ok((
                    index,
                    match storage {
                        otter_bytecode::ArgumentBindingStorage::Register { reg } => {
                            ForwardedBinding::Load {
                                base: 19,
                                offset: reg_offset(reg)?,
                            }
                        }
                        otter_bytecode::ArgumentBindingStorage::Context { reg, slot } => {
                            ForwardedBinding::ContextSlot {
                                base: 19,
                                offset: reg_offset(reg)?,
                                slot_byte: slots_byte + u32::from(slot) * 8,
                            }
                        }
                    },
                ))
            })
            .collect::<Result<Vec<_>, Unsupported>>()?;
        emit_push_forwarded(ops, 21, [10, 11], staged, &bindings);
        emit_load_reg(ops, 12, callee)?;
        emit_load_reg(ops, 13, receiver)?;
        let generic = ops.new_dynamic_label();
        let returned = ops.new_dynamic_label();
        if matches!(
            view.native_calls.get(&byte_pc),
            Some(otter_vm::JitNativeCall::Native)
        ) {
            crate::arm64::js_call::emit_native_kind_guard(ops, 12, generic);
            return_sites.record(emit_call(
                ops,
                relocations,
                table,
                20,
                12,
                Some(13),
                None,
                None,
                CallTarget::Native,
            ))?;
            dynasm!(ops ; .arch aarch64 ; b =>returned);
        }
        dynasm!(ops ; .arch aarch64 ; =>generic);
        return_sites.record(emit_call(
            ops,
            relocations,
            table,
            20,
            12,
            Some(13),
            None,
            None,
            CallTarget::Generic,
        ))?;
        dynasm!(ops ; .arch aarch64 ; =>returned);
        emit_pop_forwarded(ops, 21);
        calls::emit_call_completion(
            ops,
            relocations,
            table,
            return_sites,
            dst,
            throw_value,
            threw,
            fatal,
        )?;
        dynasm!(ops ; .arch aarch64 ; b =>done);
    }
    dynasm!(ops ; .arch aarch64 ; =>staged);
    let mut words = vec![
        PacketWord::Register(method),
        PacketWord::Register(callee),
        PacketWord::Register(receiver),
    ];
    words.extend(
        view.code_block
            .forwarded_argument_bindings()
            .filter_map(|(_, storage)| match storage {
                otter_bytecode::ArgumentBindingStorage::Register { reg } => {
                    Some(PacketWord::Register(reg))
                }
                otter_bytecode::ArgumentBindingStorage::Context { .. } => None,
            }),
    );
    words.extend(
        view.code_block
            .forwarded_formals_context()
            .map(PacketWord::Register),
    );
    super::emit_cold_call_source(ops, return_sites.logical_pc, return_sites.safepoint_id);
    emit_value_packet_transition(
        ops,
        relocations,
        table,
        abi::STUB_JIT_STAGE_FORWARD,
        &words,
        None,
        throw_value,
        fatal,
    )?;
    return_sites.record(crate::arm64::js_call::emit_enter_staged(
        ops,
        relocations,
        table,
        20,
    ))?;
    calls::emit_call_completion(
        ops,
        relocations,
        table,
        return_sites,
        dst,
        throw_value,
        threw,
        fatal,
    )?;
    dynasm!(ops ; .arch aarch64 ; =>done);
    if let Some(code_map) = code_map {
        code_map.record(CodeRegion::call_structural(
            "callTrampoline",
            start,
            ops.offset().0,
            view.code_block.id,
            logical_pc,
            byte_pc,
            None,
        ));
    }
    Ok(())
}
