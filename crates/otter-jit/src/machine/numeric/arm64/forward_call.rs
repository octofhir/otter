//! Argument forwarding staged for the common call trampoline.
//!
//! # Contents
//! - [`emit_stage`] — pre-effect source admission, then the staged request of
//!   a `CallForwardArguments` site from its canonical operand homes.
//!
//! # Invariants
//! - The packet is `[method, callee, receiver, register bindings…, formals
//!   context]`; the formals context is read from the frame register it was
//!   published to, every other word from its moving-root home.
//! - A source the staging entry cannot complete exits before any effect.
//!
//! # See also
//! - [`super::call`] — trampoline entry and completion routing.

use super::*;

pub(super) fn emit_stage(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    call: &call::CallSite<'_>,
) -> Result<(), Unsupported> {
    emit_cold_source_admission(ops, relocations, call)?;
    let words = call.result_index;
    let context_register = call.view.code_block.forwarded_formals_context();
    emit_value_span_words(
        ops,
        call.sequence,
        call.frame,
        words,
        |ops, index, register| match context_register {
            Some(frame_register) if index + 1 == words => {
                let offset = u32::from(frame_register) * 8;
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr X(register), [x19, NATIVE_FRAME_OFFSET]
                    ; ldr X(register), [X(register), NATIVE_FRAME_REGISTER_BASE_OFFSET]
                    ; ldr X(register), [X(register), offset]
                );
                Ok(())
            }
            _ => call.load_operand(ops, index, register, 0),
        },
    )?;
    call.emit_stub_call(
        ops,
        relocations,
        otter_vm::native_abi::STUB_JIT_STAGE_FORWARD,
    );
    call.emit_staging_status(ops)
}

/// Only an admitted source stages; any other exits before call effects.
fn emit_cold_source_admission(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    call: &call::CallSite<'_>,
) -> Result<(), Unsupported> {
    call.load_operand(ops, 0, 1, 0)?;
    call.emit_stub_call(
        ops,
        relocations,
        otter_vm::native_abi::STUB_JIT_FORWARD_SOURCE_READY,
    );
    let admitted = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; cbnz x0, =>admitted);
    emit_reload_safepoint_roots(ops, call.frame, call.site)?;
    dynasm!(ops ; .arch aarch64 ; b =>call.deopt ; =>admitted);
    Ok(())
}
