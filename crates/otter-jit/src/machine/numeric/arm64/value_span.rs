//! Boxed-value span arguments from precise Machine safepoint homes.
//!
//! # Contents
//! - Shared argument emission for calls and literal allocation.
//! - [`emit_value_span_words`] — packets with non-root words, such as a
//!   published formals context.
//!
//! # Invariants
//! - Roots are already saved and published when this helper runs.
//! - Packet words contain copies, never independent roots; the runtime copies
//!   the whole packet before allocating or reentering JavaScript.
//! - Raw storage belongs to the one allocator-derived Machine frame layout.
//! - An empty span needs no raw slot and uses a null pointer with zero count.

use super::*;

pub(super) fn emit_value_span_arguments(
    ops: &mut dynasmrt::aarch64::Assembler,
    sequence: &InstructionSequence,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
    arguments: impl ExactSizeIterator<Item = super::super::super::MachineValue>,
) -> Result<(), Unsupported> {
    let arguments = arguments.collect::<Vec<_>>();
    emit_value_span_words(ops, sequence, frame, arguments.len(), |ops, index, register| {
        emit_load_safepoint_root(ops, frame, site, arguments[index], register, 0)
    })
}

/// Fill a `count`-word packet through `load(ops, index, register)`, which
/// loads word `index` into `register`, and leave its address in `x1` and its
/// length in `w2`.
pub(super) fn emit_value_span_words<Load>(
    ops: &mut dynasmrt::aarch64::Assembler,
    sequence: &InstructionSequence,
    frame: MachineFrameLayout,
    count: usize,
    mut load: Load,
) -> Result<(), Unsupported>
where
    Load: FnMut(&mut dynasmrt::aarch64::Assembler, usize, u8) -> Result<(), Unsupported>,
{
    let count =
        u16::try_from(count).map_err(|_| Unsupported::OperandShape("scalar value-span length"))?;
    if count == 0 {
        dynasm!(ops ; .arch aarch64 ; mov x1, xzr ; mov w2, wzr);
        return Ok(());
    }
    let packet = super::super::value_packet_frame(sequence)?;
    let end = packet
        .raw_start
        .checked_add(count)
        .ok_or(Unsupported::OperandShape("scalar value-span extent"))?;
    if count > packet.raw_words || end > frame.raw_slots() {
        return Err(Unsupported::OperandShape("scalar value-span capacity"));
    }
    for index in 0..count {
        load(ops, usize::from(index), 16)?;
        let offset = raw_offset(frame, packet.raw_start + index)?;
        emit_frame_str_x(ops, 16, offset);
    }
    let offset = raw_offset(frame, packet.raw_start)?;
    emit_sp_address_x9(ops, offset);
    dynasm!(ops ; .arch aarch64 ; mov x1, x9 ; movz w2, u32::from(count));
    Ok(())
}
