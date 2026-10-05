//! Spread call and construct emission through the call trampoline.
//!
//! # Contents
//! - `CallSpread`, `NewSpread` and `SuperConstructSpread` stage the dense
//!   spread array's elements as the request's actual span, then enter the
//!   common call trampoline.
//!
//! # Invariants
//! - Staging reads the compiler-created array without allocating; the
//!   trampoline copies the staged span before any other work.
//! - Every completion commits once: success stores the destination, a throw
//!   or a parked error reaches the frame's exception routing.
//!
//! # See also
//! - [`super::calls::emit_trampoline_call`] — the shared request and entry.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::native_abi as abi;

use super::{
    calls::{self, CallActuals, CallCallee, CallNewTarget},
    values::{emit_load_reg, emit_load_runtime_stub},
};
use crate::{
    artifact::{CodeMapCapture, relocation::RelocationCapture},
    entry::Unsupported,
};

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_spread_call_op(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    return_sites: &mut crate::return_sites::ReturnSiteRecorder<'_>,
    view: &otter_vm::JitCompileSnapshot,
    code_map: Option<&mut CodeMapCapture>,
    opcode: u8,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    logical_pc: u32,
    byte_pc: u32,
    threw: DynamicLabel,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let lane = |packed: u64, index: usize| ((packed >> (index * 16)) & 0xffff) as u16;
    let (dst, callee, receiver, array, new_target) =
        if opcode == otter_bytecode::Op::CallSpread as u8 {
            (
                lane(arg0, 0),
                lane(arg0, 1),
                Some(lane(arg0, 2)),
                lane(arg0, 3),
                CallNewTarget::None,
            )
        } else if opcode == otter_bytecode::Op::NewSpread as u8 {
            (
                arg0 as u16,
                arg1 as u16,
                None,
                arg2 as u16,
                CallNewTarget::Callee,
            )
        } else if opcode == otter_bytecode::Op::SuperConstructSpread as u8 {
            (
                arg0 as u16,
                arg1 as u16,
                None,
                arg2 as u16,
                CallNewTarget::Super,
            )
        } else {
            return Err(Unsupported::OperandShape("spread call opcode"));
        };
    emit_load_reg(ops, 1, array)?;
    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
    super::emit_cold_call_source(ops, return_sites.logical_pc, return_sites.safepoint_id);
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        transitions.entry(abi::STUB_JIT_STAGE_SPREAD),
        abi::STUB_JIT_STAGE_SPREAD,
    );
    let staged = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbz x1, =>staged
        ; cmp x1, abi::NativeResultStatus::Throw as u32
        ; b.eq =>throw_value
        ; b =>threw
        ; =>staged
    );
    calls::emit_trampoline_call(
        ops,
        relocations,
        transitions,
        return_sites,
        view,
        code_map,
        view.code_block.id,
        logical_pc,
        byte_pc,
        CallCallee::Register(callee),
        receiver,
        new_target,
        CallActuals::Staged,
        None,
        dst,
        throw_value,
        threw,
        fatal,
    )
}
