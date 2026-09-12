//! Boxed-value span arguments from precise Machine safepoint homes.
//!
//! # Contents
//! - Shared argument emission for calls and literal allocation.
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
    let count = u16::try_from(arguments.len())
        .map_err(|_| Unsupported::OperandShape("scalar value-span length"))?;
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
    for (index, value) in arguments.enumerate() {
        emit_load_safepoint_root(ops, frame, site, value, 16, MACHINE_ROOT_RECORD_SIZE)?;
        let offset = raw_offset(frame, packet.raw_start + index as u16)?
            .checked_add(MACHINE_ROOT_RECORD_SIZE)
            .ok_or(Unsupported::OperandShape("scalar value-span slot"))?;
        emit_sp_str_x(ops, 16, offset);
    }
    let offset = raw_offset(frame, packet.raw_start)?
        .checked_add(MACHINE_ROOT_RECORD_SIZE)
        .ok_or(Unsupported::OperandShape("scalar value-span base"))?;
    emit_sp_address_x9(ops, offset);
    dynasm!(ops ; .arch aarch64 ; mov x1, x9 ; movz w2, u32::from(count));
    Ok(())
}
