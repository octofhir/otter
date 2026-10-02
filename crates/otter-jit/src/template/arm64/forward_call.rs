//! Argument forwarding through the call trampoline.
//!
//! # Contents
//! - `CallForwardArguments` stages its complete request from the resolved
//!   `apply`, callee, receiver and current argument bindings, then enters the
//!   common call trampoline.
//!
//! # Invariants
//! - The staging entry resolves the intrinsic `apply` to the activation's
//!   actual arguments (mapped formals refreshed from the packet) and any other
//!   method to a call with the arguments object; it never calls.
//! - The request is complete before the trampoline is entered; nothing
//!   allocates in between.
//!
//! # See also
//! - [`super::calls::emit_call_completion`] — the shared completion routing.

use dynasmrt::{DynamicLabel, DynasmApi, aarch64::Assembler};
use otter_vm::{JitCompileSnapshot, native_abi as abi};

use super::{
    calls,
    value_packet::{PacketWord, emit_value_packet_transition},
};
use crate::{
    artifact::{CodeMapCapture, CodeRegion, relocation::RelocationCapture},
    entry::{TransitionTable, Unsupported},
};

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_forward_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
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
    crate::arm64::js_call::emit_enter_staged(ops, relocations, table, 20);
    calls::emit_call_completion(ops, dst, throw_value, threw)?;
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
