//! Intrinsic apply forwarding over an activation's live actual arguments.
//!
//! # Contents
//! - A proved intrinsic `apply` and elided arguments object copy the exact
//!   activation actual span and current mapped formals before entering the
//!   common private JavaScript entry.
//! - Overridden apply, exposed arguments, and stack admission misses stage
//!   one complete request through the existing committed runtime boundary.
//!
//! # Invariants
//! - `r15` owns the context, `r14` the published frame, and `r13` its traced
//!   register window. Parameter contexts are reloaded from that window.
//! - The already evaluated method's cell type and bootstrap native identity
//!   are proved before the stack changes; no apply property lookup is repeated.
//! - Copying and mapped refresh allocate nothing. No context-derived address
//!   survives the operation or a collecting call. The callee owns its frame,
//!   receiver conversion, actual tracing, and missing-formal initialization.
//! - Stack admission refusal precedes writes and RSP movement. Every call
//!   completion releases its actual span before status routing, including throw.
//! - Cold staging publishes its exact PC and safepoint.
//! - Every generated entry records its physical CALL return before cleanup.
//!
//! # See also
//! - [`crate::x86_64::js_call`] — sole outgoing actual-span and call ABI owner.
//! - [`super::calls::emit_completion`] — once-only completion routing.
//! - `crate::template::arm64::forward_call` — the peer target encoder.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::{JitCompileSnapshot, native_abi as abi};

use super::{calls, emit_load_reg, native_leaf};
use crate::{
    artifact::relocation::RelocationCapture,
    call_linkage::ForwardedBinding,
    entry::{TransitionTable, Unsupported},
    x86_64::{
        js_call::{
            CallTarget, emit_enter_staged, emit_forwarded_call, emit_pop_forwarded,
            emit_push_forwarded,
        },
        values::emit_load_runtime_stub,
    },
};

#[cfg(test)]
#[path = "forward_call_tests.rs"]
mod tests;

/// Emit one forward from `[destination, resolved apply, callee, receiver]`.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_forward_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    return_sites: &mut crate::return_sites::ReturnSiteRecorder<'_>,
    view: &JitCompileSnapshot,
    [dst, method, callee, receiver]: [u16; 4],
    throw_value: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let staged = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    if let Some(apply) = view.forward_apply_native_ref {
        emit_load_reg(ops, 10, method);
        native_leaf::emit_guard(ops, view, apply, staged);
        dynasm!(ops ; .arch x64
            ; cmp DWORD [r14 + abi::NATIVE_FRAME_ARGUMENTS_OBJECT_OFFSET as i32], 0
            ; jne =>staged
        );
        let bindings = view
            .code_block
            .forwarded_argument_bindings()
            .map(|(index, storage)| {
                Ok((
                    index,
                    match storage {
                        otter_bytecode::ArgumentBindingStorage::Register { reg } => {
                            ForwardedBinding::Load {
                                base: 13,
                                offset: u32::from(reg) * 8,
                            }
                        }
                        otter_bytecode::ArgumentBindingStorage::Context { reg, slot } => {
                            let slot_byte = view
                                .context_layout
                                .slots_byte
                                .checked_add(u32::from(slot) * 8)
                                .filter(|&offset| i32::try_from(offset).is_ok())
                                .ok_or(Unsupported::OperandShape(
                                    "forward context slot displacement",
                                ))?;
                            ForwardedBinding::ContextSlot {
                                base: 13,
                                offset: u32::from(reg) * 8,
                                slot_byte,
                            }
                        }
                    },
                ))
            })
            .collect::<Result<Vec<_>, Unsupported>>()?;
        // R8/R9 are scratch until copy completes; shared code reserves R10/R11.
        emit_push_forwarded(ops, 14, [8, 9], staged, &bindings);
        emit_load_reg(ops, 6, callee);
        emit_load_reg(ops, 2, receiver);
        let generic = ops.new_dynamic_label();
        let returned = ops.new_dynamic_label();
        if matches!(
            view.native_calls
                .get(&view.instructions[return_sites.logical_pc as usize].byte_pc),
            Some(otter_vm::JitNativeCall::Native)
        ) {
            crate::x86_64::js_call::emit_native_kind_guard(ops, 6, generic);
            return_sites.record(emit_forwarded_call(
                ops,
                relocations,
                table,
                15,
                CallTarget::Native,
            ))?;
            dynasm!(ops ; .arch x64 ; jmp =>returned);
        }
        dynasm!(ops ; .arch x64 ; =>generic);
        return_sites.record(emit_forwarded_call(
            ops,
            relocations,
            table,
            15,
            CallTarget::Generic,
        ))?;
        dynasm!(ops ; .arch x64 ; =>returned);
        emit_pop_forwarded(ops, 14);
        calls::emit_completion(
            ops,
            relocations,
            table,
            return_sites,
            dst,
            throw_value,
            threw,
            fatal,
        )?;
        dynasm!(ops ; .arch x64 ; jmp =>done);
    }
    dynasm!(ops ; .arch x64 ; =>staged);
    let mut words = vec![method, callee, receiver];
    words.extend(
        view.code_block
            .forwarded_argument_bindings()
            .filter_map(|(_, storage)| match storage {
                otter_bytecode::ArgumentBindingStorage::Register { reg } => Some(reg),
                otter_bytecode::ArgumentBindingStorage::Context { .. } => None,
            }),
    );
    words.extend(view.code_block.forwarded_formals_context());
    let bytes = crate::call_linkage::pushed_argument_bytes(words.len())?;
    dynasm!(ops ; .arch x64 ; sub rsp, bytes as i32);
    for (index, &word) in words.iter().enumerate() {
        emit_load_reg(ops, 0, word);
        dynasm!(ops ; .arch x64 ; mov [rsp + (index * 8) as i32], rax);
    }
    let ready = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; mov rdi, r15
        ; mov rsi, rsp
        ; mov edx, words.len() as i32
    );
    super::emit_cold_call_source(ops, return_sites.logical_pc, return_sites.safepoint_id);
    emit_load_runtime_stub(
        ops,
        relocations,
        table.entry(abi::STUB_JIT_STAGE_FORWARD),
        abi::STUB_JIT_STAGE_FORWARD,
    );
    crate::x86_64::call_abi::emit_runtime_call(ops, abi::STUB_JIT_STAGE_FORWARD);
    dynasm!(ops
        ; .arch x64
        ; add rsp, bytes as i32
        ; test rdx, rdx
        ; jz =>ready
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>throw_value
        ; jmp =>threw
        ; =>ready
    );
    return_sites.record(emit_enter_staged(ops, relocations, table, 15))?;
    calls::emit_completion(
        ops,
        relocations,
        table,
        return_sites,
        dst,
        throw_value,
        threw,
        fatal,
    )?;
    dynasm!(ops ; .arch x64 ; =>done);
    Ok(())
}
