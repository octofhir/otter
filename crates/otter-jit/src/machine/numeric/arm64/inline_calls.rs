//! Physical inline-parent publication around cold generated construct calls.
//!
//! # Contents
//! - Bounded native headers and register windows in the owning Machine frame.
//! - Exact source PCs, closure spines and moving values from safepoint homes.
//! - Publication and release through the existing native activation cursor.
//!
//! # Invariants
//! - No VM operation, allocation or collection occurs during publication.
//! - The same NativeFrame ABI and generated construct linkage remain authoritative.
//! - Raw words become roots only while their canonical native frames are published.
//! - Every call completion removes the physical parents before restoring SSA homes.
//!
//! # See also
//! - `crate::arm64::direct_call` — the shared generated callee linkage.
//! - `super::super::inline_reentry` — code-owned source and root recipes.

use super::*;
use crate::entry::{
    ACTIVATION_BASE_OFFSET, ACTIVATION_LIMIT_OFFSET, ACTIVATION_TOP_PTR_OFFSET,
    VM_THREAD_CURRENT_FRAME_OFFSET,
};
use crate::machine::MachineInstruction;
use otter_vm::native_abi::{NativeFrame, NativeFrameFlags, NativeFrameKind};

const HEADER_WORDS: usize = std::mem::size_of::<NativeFrame>() / 8;

pub(in crate::machine::numeric) fn frame_words(
    sequence: &InstructionSequence,
) -> Result<u16, Unsupported> {
    let max = sequence.instructions().iter().filter(|instruction| {
        matches!(instruction.opcode, MachineOpcode::Call(index)
            if matches!(sequence.call_descriptors()[index as usize].target, CallTarget::Direct { .. }))
    }).map(|instruction| {
        if instruction.inline_frames.is_empty() { 0 } else {
            1 + instruction.inline_frames.iter().skip(1).map(|frame| HEADER_WORDS + frame.slots.len()).sum::<usize>()
        }
    }).max().unwrap_or(0);
    u16::try_from(max).map_err(|_| Unsupported::OperandShape("inline native frame capacity"))
}

fn offset(frame: MachineFrameLayout, word: u16) -> Result<u32, Unsupported> {
    frame
        .raw_offset(word)
        .map_err(|_| Unsupported::OperandShape("inline native frame word"))?
        .checked_add(MACHINE_ROOT_RECORD_SIZE)
        .ok_or(Unsupported::OperandShape("inline native frame offset"))
}

fn load_slot(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
    value: Option<MachineValue>,
    target: u8,
) -> Result<(), Unsupported> {
    if let Some(value) = value {
        let root = site
            .roots
            .iter()
            .find(|root| root.value == value)
            .ok_or(Unsupported::OperandShape("inline native frame root"))?;
        let address = root_offset(frame, root.save_slot)?
            .checked_add(MACHINE_ROOT_RECORD_SIZE)
            .ok_or(Unsupported::OperandShape("inline native root offset"))?;
        emit_sp_ldr_x(ops, target, address);
    } else {
        emit_load_u64(ops, target, VALUE_UNDEFINED);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn enter(
    ops: &mut dynasmrt::aarch64::Assembler,
    view: &JitCompileSnapshot,
    frame: MachineFrameLayout,
    instruction: &MachineInstruction,
    site: &MachineSafepointSite,
    first: u16,
    fail: dynasmrt::DynamicLabel,
) -> Result<(), Unsupported> {
    if instruction.inline_frames.is_empty() {
        return Ok(());
    }
    let count = instruction.inline_frames.len() - 1;
    let parent = &instruction.inline_frames[0];
    let parent_pc =
        super::super::frame_state::suspended_call_pc(view, parent.function_id, parent.byte_pc)
            .ok_or(Unsupported::OperandShape("inline native root source"))?
            .0;
    dynasm!(ops ; .arch aarch64
        ; ldr x9, [x19, ACTIVATION_TOP_PTR_OFFSET]
        ; ldr x10, [x9]
        ; add x10, x10, count as u32
        ; ldr x11, [x19, ACTIVATION_LIMIT_OFFSET]
        ; cmp x10, x11
        ; b.hi =>fail
        ; ldr x12, [x19, NATIVE_FRAME_OFFSET]);
    emit_sp_str_x(ops, 12, offset(frame, first)?);
    emit_load_u64(ops, 15, u64::from(parent_pc));
    dynasm!(ops ; .arch aarch64 ; str w15, [x12, NATIVE_FRAME_PC_OFFSET]);
    let mut word = usize::from(first) + 1;
    for (index, recipe) in instruction.inline_frames.iter().enumerate().skip(1) {
        let entry = recipe
            .entry
            .as_ref()
            .ok_or(Unsupported::OperandShape("inline native entry"))?;
        let pc = if index + 1 < instruction.inline_frames.len() {
            super::super::frame_state::suspended_call_pc(view, recipe.function_id, recipe.byte_pc)
                .map(|pc| pc.0)
        } else {
            super::super::frame_state::resume_pc(view, recipe.function_id, recipe.byte_pc)
        }
        .ok_or(Unsupported::OperandShape("inline native source PC"))?;
        let start = offset(
            frame,
            u16::try_from(word).map_err(|_| Unsupported::OperandShape("inline native start"))?,
        )?;
        for slot in 0..HEADER_WORDS {
            emit_sp_str_x(ops, 31, start + slot as u32 * 8);
        }
        emit_sp_address_x9(ops, start);
        dynasm!(ops ; .arch aarch64 ; mov x13, x9);
        emit_load_u64(
            ops,
            14,
            u64::from(recipe.function_id) | (u64::from(pc) << 32),
        );
        dynasm!(ops ; .arch aarch64 ; str x14, [x13]);
        let header = recipe.slots.len() as u32
            | ((NativeFrameKind::Optimizing as u32) << 16)
            | (u32::from(NativeFrameFlags::STACK_REGISTERS) << 24);
        emit_load_u64(ops, 14, u64::from(header));
        dynasm!(ops ; .arch aarch64 ; str w14, [x13, NATIVE_FRAME_REGISTER_COUNT_OFFSET]);
        emit_sp_address_x9(ops, start + std::mem::size_of::<NativeFrame>() as u32);
        dynasm!(ops ; .arch aarch64 ; str x9, [x13, NATIVE_FRAME_REGISTER_BASE_OFFSET]);
        for (value, member) in [
            (
                entry.this,
                std::mem::offset_of!(NativeFrame, this_value_bits),
            ),
            (
                entry.new_target,
                std::mem::offset_of!(NativeFrame, new_target_bits),
            ),
            (
                entry.closure,
                std::mem::offset_of!(NativeFrame, self_value_bits),
            ),
        ] {
            load_slot(ops, frame, site, value, 14)?;
            dynasm!(ops ; .arch aarch64 ; str x14, [x13, member as u32]);
        }
        // The closure's immutable old-space spine remains stable while SELF is rooted.
        let no_spine = ops.new_dynamic_label();
        crate::template::arm64::values::emit_cell_test(
            ops,
            14,
            15,
            crate::template::arm64::values::CellTest::IsNotCell,
            no_spine,
        );
        dynasm!(ops ; .arch aarch64
            ; ldr x10, [x14, view.closure_call_layout.upvalue_base_byte]
            ; ldr w11, [x14, view.closure_call_layout.upvalue_count_byte]
            ; str x10, [x13, NATIVE_FRAME_UPVALUE_BASE_OFFSET]
            ; str w11, [x13, NATIVE_FRAME_UPVALUE_COUNT_OFFSET]
            ; =>no_spine);
        for (index, value) in recipe.slots.iter().enumerate() {
            load_slot(ops, frame, site, *value, 14)?;
            emit_sp_str_x(ops, 14, start + (HEADER_WORDS + index) as u32 * 8);
        }
        dynasm!(ops ; .arch aarch64
            ; ldr x9, [x19, ACTIVATION_TOP_PTR_OFFSET]
            ; ldr x10, [x9]
            ; ldr x11, [x19, ACTIVATION_BASE_OFFSET]
            ; str x13, [x11, x10, lsl #3]
            ; add x10, x10, #1
            ; str x10, [x9]);
        word += HEADER_WORDS + recipe.slots.len();
    }
    dynasm!(ops ; .arch aarch64
        ; str x13, [x19, NATIVE_FRAME_OFFSET]
        ; ldr x9, [x19, THREAD_OFFSET]
        ; str x13, [x9, VM_THREAD_CURRENT_FRAME_OFFSET]);
    Ok(())
}

pub(super) fn leave(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    instruction: &MachineInstruction,
    first: u16,
) -> Result<(), Unsupported> {
    if instruction.inline_frames.is_empty() {
        return Ok(());
    }
    let count = instruction.inline_frames.len() - 1;
    emit_sp_ldr_x(ops, 12, offset(frame, first)?);
    dynasm!(ops ; .arch aarch64
        ; str x12, [x19, NATIVE_FRAME_OFFSET]
        ; ldr x9, [x19, THREAD_OFFSET]
        ; str x12, [x9, VM_THREAD_CURRENT_FRAME_OFFSET]
        ; ldr x9, [x19, ACTIVATION_TOP_PTR_OFFSET]
        ; ldr x10, [x9]
        ; sub x10, x10, count as u32
        ; str x10, [x9]
        ; ldr x11, [x19, ACTIVATION_BASE_OFFSET]
        ; add x11, x11, x10, lsl #3);
    for index in 0..count {
        dynasm!(ops ; .arch aarch64 ; str xzr, [x11, index as u32 * 8]);
    }
    Ok(())
}
