//! Physical inline-parent publication around x86-64 generated construct calls.
//!
//! # Contents
//! - Bounded native headers and register windows in the owning Machine frame.
//! - Exact source PCs, closure spines and moving values from safepoint homes.
//! - Publication and release through the native activation cursor.
//!
//! # Invariants
//! - Publication performs no VM operation, allocation, or collection.
//! - Every boxed member is read from the canonical safepoint save home.
//! - Completion removes every synthetic activation before returning to Machine SSA.
//!
//! # See also
//! - `crate::machine::numeric::arm64::inline_calls` — peer target emitter.
//! - `crate::machine::numeric::inline_reentry` — code-owned source recipes.

use super::*;
use crate::machine::MachineInstruction;
use otter_vm::native_abi::{NativeFrame, NativeFrameFlags, NativeFrameKind};

const HEADER_WORDS: usize = std::mem::size_of::<NativeFrame>() / 8;

fn offset(frame: MachineFrameLayout, word: u16, stack_bias: u32) -> Result<i32, Unsupported> {
    frame
        .raw_offset(word)
        .map_err(|_| Unsupported::OperandShape("x86-64 inline native frame word"))?
        .checked_add(stack_bias)
        .and_then(|offset| i32::try_from(offset).ok())
        .ok_or(Unsupported::OperandShape(
            "x86-64 inline native frame offset",
        ))
}

fn load_slot(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
    value: Option<super::super::MachineValue>,
    target: u8,
) -> Result<(), Unsupported> {
    if let Some(value) = value {
        let root = site
            .roots
            .iter()
            .find(|root| root.value == value)
            .ok_or(Unsupported::OperandShape("x86-64 inline native frame root"))?;
        let address = root_offset(frame, root.save_slot)?
            .checked_add(MACHINE_ROOT_RECORD_SIZE)
            .and_then(|offset| i32::try_from(offset).ok())
            .ok_or(Unsupported::OperandShape(
                "x86-64 inline native root offset",
            ))?;
        dynasm!(ops ; .arch x64 ; mov Rq(target), [rsp + address]);
    } else {
        load64(ops, target, VALUE_UNDEFINED);
    }
    Ok(())
}

pub(super) fn enter(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    frame: MachineFrameLayout,
    instruction: &MachineInstruction,
    site: &MachineSafepointSite,
    first: u16,
    fail: DynamicLabel,
) -> Result<(), Unsupported> {
    if instruction.inline_frames.is_empty() {
        return Ok(());
    }
    let count = instruction.inline_frames.len() - 1;
    let parent = &instruction.inline_frames[0];
    let parent_pc =
        super::super::frame_state::suspended_call_pc(view, parent.function_id, parent.byte_pc)
            .ok_or(Unsupported::OperandShape(
                "x86-64 inline native root source",
            ))?
            .0;
    dynasm!(ops ; .arch x64
        ; mov r8, [r15 + ACTIVATION_TOP_PTR_OFFSET as i32]
        ; mov r9, [r8]
        ; lea r10, [r9 + count as i32]
        ; cmp r10, [r15 + ACTIVATION_LIMIT_OFFSET as i32]
        ; ja =>fail
        ; mov r12, [r15 + NATIVE_FRAME_OFFSET as i32]
    );
    let parent_slot = offset(frame, first, MACHINE_ROOT_RECORD_SIZE)?;
    dynasm!(ops ; .arch x64
        ; mov [rsp + parent_slot], r12
        ; mov DWORD [r12 + NATIVE_FRAME_PC_OFFSET as i32], parent_pc as i32
    );

    let mut word = usize::from(first) + 1;
    for (index, recipe) in instruction.inline_frames.iter().enumerate().skip(1) {
        let entry = recipe
            .entry
            .as_ref()
            .ok_or(Unsupported::OperandShape("x86-64 inline native entry"))?;
        let pc = if index + 1 < instruction.inline_frames.len() {
            super::super::frame_state::suspended_call_pc(view, recipe.function_id, recipe.byte_pc)
                .map(|pc| pc.0)
        } else {
            super::super::frame_state::resume_pc(view, recipe.function_id, recipe.byte_pc)
        }
        .ok_or(Unsupported::OperandShape("x86-64 inline native source PC"))?;
        let start_word = u16::try_from(word)
            .map_err(|_| Unsupported::OperandShape("x86-64 inline native start"))?;
        let start = offset(frame, start_word, MACHINE_ROOT_RECORD_SIZE)?;
        dynasm!(ops ; .arch x64 ; xor eax, eax);
        for slot in 0..HEADER_WORDS {
            dynasm!(ops ; .arch x64 ; mov [rsp + start + slot as i32 * 8], rax);
        }
        dynasm!(ops ; .arch x64 ; lea r13, [rsp + start]);
        load64(
            ops,
            14,
            u64::from(recipe.function_id) | (u64::from(pc) << 32),
        );
        dynasm!(ops ; .arch x64 ; mov [r13], r14);
        let header = recipe.slots.len() as u32
            | ((NativeFrameKind::Optimizing as u32) << 16)
            | (u32::from(NativeFrameFlags::STACK_REGISTERS) << 24);
        dynasm!(ops
            ; .arch x64
            ; mov DWORD [r13 + NATIVE_FRAME_REGISTER_COUNT_OFFSET as i32], header as i32
            ; lea r14, [r13 + HEADER_WORDS as i32 * 8]
            ; mov [r13 + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32], r14
        );
        for (value, member) in [
            (entry.this, NATIVE_FRAME_THIS_OFFSET),
            (entry.new_target, NATIVE_FRAME_NEW_TARGET_OFFSET),
            (entry.closure, NATIVE_FRAME_SELF_OFFSET),
        ] {
            load_slot(ops, frame, site, value, 14)?;
            dynasm!(ops ; .arch x64 ; mov [r13 + member as i32], r14);
        }

        let no_spine = ops.new_dynamic_label();
        load64(ops, 11, NOT_CELL_MASK);
        dynasm!(ops
            ; .arch x64
            ; test r14, r14
            ; jz =>no_spine
            ; mov r10, r14
            ; and r10, r11
            ; jnz =>no_spine
            ; cmp BYTE [r14], JS_CLOSURE_BODY_TYPE_TAG as i8
            ; jne =>no_spine
            ; mov r10, [r14 + view.closure_call_layout.upvalue_base_byte as i32]
            ; mov r11d, [r14 + view.closure_call_layout.upvalue_count_byte as i32]
            ; mov [r13 + NATIVE_FRAME_UPVALUE_BASE_OFFSET as i32], r10
            ; mov [r13 + NATIVE_FRAME_UPVALUE_COUNT_OFFSET as i32], r11d
            ; =>no_spine
        );
        for (slot, value) in recipe.slots.iter().enumerate() {
            load_slot(ops, frame, site, *value, 14)?;
            let destination = start
                .checked_add(((HEADER_WORDS + slot) * 8) as i32)
                .ok_or(Unsupported::OperandShape(
                    "x86-64 inline native register offset",
                ))?;
            dynasm!(ops ; .arch x64 ; mov [rsp + destination], r14);
        }
        dynasm!(ops
            ; .arch x64
            ; mov r8, [r15 + ACTIVATION_TOP_PTR_OFFSET as i32]
            ; mov r9, [r8]
            ; mov r10, [r15 + ACTIVATION_BASE_OFFSET as i32]
            ; mov [r10 + r9 * 8], r13
            ; add r9, 1
            ; mov [r8], r9
        );
        word += HEADER_WORDS + recipe.slots.len();
    }
    dynasm!(ops
        ; .arch x64
        ; mov [r15 + NATIVE_FRAME_OFFSET as i32], r13
        ; mov r8, [r15 + THREAD_OFFSET as i32]
        ; mov [r8 + VM_THREAD_CURRENT_FRAME_OFFSET as i32], r13
    );
    Ok(())
}

pub(super) fn leave(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    instruction: &MachineInstruction,
    first: u16,
) -> Result<(), Unsupported> {
    if instruction.inline_frames.is_empty() {
        return Ok(());
    }
    let count = instruction.inline_frames.len() - 1;
    let parent_slot = offset(frame, first, 0)?;
    dynasm!(ops
        ; .arch x64
        ; mov r10, [rsp + parent_slot]
        ; mov [r15 + NATIVE_FRAME_OFFSET as i32], r10
        ; mov r8, [r15 + THREAD_OFFSET as i32]
        ; mov [r8 + VM_THREAD_CURRENT_FRAME_OFFSET as i32], r10
        ; mov r8, [r15 + ACTIVATION_TOP_PTR_OFFSET as i32]
        ; mov r9, [r8]
        ; sub r9, count as i32
        ; mov [r8], r9
        ; mov r10, [r15 + ACTIVATION_BASE_OFFSET as i32]
        ; lea r10, [r10 + r9 * 8]
        ; xor r11d, r11d
    );
    for index in 0..count {
        dynasm!(ops ; .arch x64 ; mov [r10 + index as i32 * 8], r11);
    }
    Ok(())
}
