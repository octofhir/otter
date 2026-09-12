//! Temporary boxed-value spans for shared compiled runtime boundaries.
//!
//! # Contents
//! - Frame-register and constant packet sources and one result-pair emitter.
//!
//! # Invariants
//! - The published native frame roots every source; the runtime copies the
//!   packet before collection or JavaScript reentry.
//! - Stack storage is aligned and balanced before every success/error branch.
//! - Empty literals pass an empty span. Bytecode's u8 operand-count bound
//!   keeps every dense literal packet within the shared 4080-byte limit.
//! - Success commits the destination once; a pure exception uses the common
//!   throw router and cannot replay the source operation.
//!
//! # See also
//! - `crate::entry::runtime_ops::literals` and `otter_vm::native_abi`.

use super::values::{emit_load_reg, emit_load_runtime_stub, emit_load_u64, emit_store_reg};
use crate::{Unsupported, artifact::relocation::RelocationCapture, entry::TransitionTable};
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::Value;
use otter_vm::native_abi as abi;

/// One word of a boxed-value packet handed to a reentrant value transition.
#[derive(Debug, Clone, Copy)]
pub(super) enum PacketWord {
    /// A live interpreter-compatible register of the compiled frame.
    Register(u16),
    /// The `undefined` constant.
    Undefined,
}

/// Build one contiguous boxed-value packet on the machine stack, complete it
/// through `descriptor`, and commit the returned value to `dst`.
///
/// The packet lives below `sp` only for the duration of the transition; the
/// VM copies it before any allocation, so no packet word is a collector root.
/// Success falls through with `dst` written, a JavaScript exception reaches
/// `throw_value`, and a structural failure reaches `fatal`. The transition
/// never requests a side exit or a replay.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_value_packet_transition(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    descriptor: abi::RuntimeStubDescriptor,
    words: &[PacketWord],
    dst: u16,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let packet_words = u32::try_from(words.len())
        .ok()
        .ok_or(Unsupported::OperandShape(
            "template value-packet word count",
        ))?;
    let packet_bytes = packet_words
        .checked_mul(8)
        .and_then(|bytes| bytes.checked_add(15))
        .map(|bytes| bytes & !15)
        .filter(|bytes| *bytes <= 4_080)
        .ok_or(Unsupported::OperandShape("template value-packet frame"))?;
    dynasm!(ops ; .arch aarch64 ; sub sp, sp, packet_bytes);
    for (index, word) in words.iter().enumerate() {
        match *word {
            PacketWord::Register(register) => emit_load_reg(ops, 9, register)?,
            PacketWord::Undefined => emit_load_u64(ops, 9, Value::undefined().to_bits()),
        }
        let offset = u32::try_from(index)
            .ok()
            .and_then(|word| word.checked_mul(8))
            .ok_or(Unsupported::OperandShape("template value-packet offset"))?;
        dynasm!(ops ; .arch aarch64 ; str x9, [sp, offset]);
    }
    dynasm!(ops ; .arch aarch64 ; mov x0, x20 ; mov x1, sp ; movz w2, packet_words);
    emit_load_runtime_stub(ops, relocations, 16, table.entry(descriptor), descriptor);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; add sp, sp, packet_bytes
        ; cbz x1, >completed
        ; cmp x1, abi::NativeResultStatus::Throw as u32
        ; b.eq =>throw_value
        ; b =>fatal
        ; completed:
    );
    emit_store_reg(ops, 0, dst)
}
