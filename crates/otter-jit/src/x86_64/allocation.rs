//! Generated nursery allocation shared by the x86-64 tiers.
//!
//! # Contents
//! - [`emit_count_allocation`]: the per-type statistics row update the Rust
//!   allocator performs for every cell.
//! - [`emit_create_context`]: `CreateContext` carved from a
//!   [`otter_vm::jit::JitContextAllocationPlan`].
//! - [`emit_copy_context`]: `CopyContext` carved from the live source cell.
//! - [`emit_closure`]: `MakeClosure` / `MakeFunction` carved from a
//!   [`otter_vm::jit::JitClosureAllocationPlan`].
//!
//! # Invariants
//! - `r15` holds the entry context. A carve reads the linear allocation
//!   buffer through its allocation window; a disabled window names an empty
//!   buffer whose bump always misses, so the heap turns generated allocation
//!   off by emptying it (marking, stress, tenuring and heap caps all do).
//! - Every word of a cell is written before the bump cursor is published,
//!   and nothing between the bump probe and the publication can collect.
//! - A miss branches before any effect, with `rcx`, `rdx`, `r8` and every
//!   register outside the documented clobber set intact.
//! - A fresh cell is young and unmarked, so its initializing stores need no
//!   write barrier.
//!
//! # See also
//! - `crate::arm64::allocation` — the peer AArch64 carves.
//! - `otter_vm::context` — the context body and its initialization contract.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::JitCompileSnapshot;
use otter_vm::jit::{
    JIT_CONTEXT_HAS_EXTENSION_BIT, JIT_INLINE_CONTEXT_MAX_WORDS, JIT_TYPE_STATS_ALLOC_BYTES_OFFSET,
    JIT_TYPE_STATS_ALLOC_COUNT_OFFSET, JIT_TYPE_STATS_LIVE_BYTES_OFFSET, JIT_TYPE_STATS_ROW_BYTES,
    JIT_CLOSURE_CELL_BYTES, JIT_YOUNG_CLOSURE_HEADER_WORD, JIT_YOUNG_CONTEXT_HEADER_WORD,
    JitClosureAllocationPlan, JitContextAllocationPlan,
};

use crate::entry::{
    ALLOC_WINDOW_LAB_OFFSET, ALLOC_WINDOW_TYPE_STATS_OFFSET, LAB_LIMIT_OFFSET, LAB_TOP_OFFSET,
    NATIVE_FRAME_NEW_TARGET_OFFSET, NATIVE_FRAME_THIS_OFFSET, VALUE_UNDEFINED,
};

/// Account one generated allocation of `Rq(size)` bytes against the
/// statistics row of `type_tag`. Clobbers `Rq(base)`.
pub(crate) fn emit_count_allocation(ops: &mut Assembler, type_tag: u8, size: u8, base: u8) {
    let row = u32::from(type_tag) * JIT_TYPE_STATS_ROW_BYTES;
    let live = (row + JIT_TYPE_STATS_LIVE_BYTES_OFFSET) as i32;
    let count = (row + JIT_TYPE_STATS_ALLOC_COUNT_OFFSET) as i32;
    let bytes = (row + JIT_TYPE_STATS_ALLOC_BYTES_OFFSET) as i32;
    dynasm!(ops
        ; .arch x64
        ; mov Rq(base), [r15 + ALLOC_WINDOW_TYPE_STATS_OFFSET as i32]
        ; add [Rq(base) + live], Rq(size)
        ; add QWORD [Rq(base) + count], 1
        ; add [Rq(base) + bytes], Rq(size)
    );
}

/// Bump probe for a cell of `r9` bytes: `r11` = the buffer, `rax` = the
/// cell, `r10` = the new cursor. Jumps to `slow` when the cell does not fit.
fn emit_bump_probe(ops: &mut Assembler, slow: DynamicLabel) {
    dynasm!(ops
        ; .arch x64
        ; mov r11, [r15 + ALLOC_WINDOW_LAB_OFFSET as i32]
        ; mov rax, [r11 + LAB_TOP_OFFSET as i32]
        ; lea r10, [rax + r9]
        ; cmp r10, [r11 + LAB_LIMIT_OFFSET as i32]
        ; ja =>slow
    );
}

/// Publish the cell in `rax` by storing the cursor `r10` into buffer `r11`
/// and account `r9` bytes to contexts.
fn emit_publish_context(ops: &mut Assembler, view: &JitCompileSnapshot) {
    dynasm!(ops ; .arch x64 ; mov [r11 + LAB_TOP_OFFSET as i32], r10);
    emit_count_allocation(ops, view.context_layout.type_tag, 9, 11);
}

/// Carve `CreateContext` for `plan` under the parent value in `rdx` and
/// return it in `rax`; `slow` is taken before any effect. Clobbers `rax`,
/// `rsi`, `rdi`, `r9`–`r11`.
pub(crate) fn emit_create_context(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    plan: &JitContextAllocationPlan,
    slow: DynamicLabel,
) {
    let layout = view.context_layout;
    dynasm!(ops ; .arch x64 ; mov r9d, plan.cell_bytes as i32);
    emit_bump_probe(ops, slow);
    dynasm!(ops
        ; .arch x64
        ; mov rdi, QWORD plan.header_word as i64
        ; mov [rax], rdi
        ; mov rdi, QWORD plan.body_word as i64
        ; mov [rax + layout.scope_function_id_byte as i32], rdi
        ; mov [rax + layout.parent_byte as i32], rdx
    );
    // Trailing words take at most two distinct values (`hole` and
    // `undefined`); each lives in one register for the whole fill.
    let mut cached: [(u8, Option<u64>); 2] = [(6, None), (7, None)];
    for (index, &word) in plan.initial_words.iter().enumerate() {
        let register =
            if let Some(&(register, _)) = cached.iter().find(|(_, value)| *value == Some(word)) {
                register
            } else if let Some(entry) = cached.iter_mut().find(|(_, value)| value.is_none()) {
                entry.1 = Some(word);
                dynasm!(ops ; .arch x64 ; mov Rq(entry.0), QWORD word as i64);
                entry.0
            } else {
                dynasm!(ops ; .arch x64 ; mov rdi, QWORD word as i64);
                7
            };
        let offset = (layout.slots_byte + 8 * index as u32) as i32;
        dynasm!(ops ; .arch x64 ; mov [rax + offset], Rq(register));
    }
    emit_publish_context(ops, view);
}

/// Carve `CopyContext` from the context in `rdx` and return the copy in
/// `rax`; `slow` is taken before any effect for a scope carrying an
/// eval-extension word or more than [`JIT_INLINE_CONTEXT_MAX_WORDS`] slots.
/// Clobbers `rax`, `rsi`, `rdi`, `r9`–`r11`.
pub(crate) fn emit_copy_context(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    slow: DynamicLabel,
) {
    let layout = view.context_layout;
    let copy = ops.new_dynamic_label();
    let copied = ops.new_dynamic_label();
    // Slot `i` lives at `slots_byte + 8 * i`; the loop walks `rsi` from the
    // slot count down to one.
    let last_slot = layout.slots_byte as i32 - 8;
    dynasm!(ops
        ; .arch x64
        ; test WORD [rdx + layout.scope_index_byte as i32], (1u16 << JIT_CONTEXT_HAS_EXTENSION_BIT) as i16
        ; jnz =>slow
        ; movzx esi, WORD [rdx + layout.slot_count_byte as i32]
        ; cmp esi, JIT_INLINE_CONTEXT_MAX_WORDS as i32
        ; ja =>slow
        ; lea r9d, [rsi * 8 + layout.slots_byte as i32]
    );
    emit_bump_probe(ops, slow);
    dynasm!(ops
        ; .arch x64
        ; mov rdi, r9
        ; shl rdi, 32
        ; or rdi, JIT_YOUNG_CONTEXT_HEADER_WORD as i32
        ; mov [rax], rdi
        // Scope identity, slot count and parent carry over verbatim.
        ; mov rdi, [rdx + layout.scope_function_id_byte as i32]
        ; mov [rax + layout.scope_function_id_byte as i32], rdi
        ; mov rdi, [rdx + layout.parent_byte as i32]
        ; mov [rax + layout.parent_byte as i32], rdi
        ; test esi, esi
        ; jz =>copied
        ; =>copy
        ; mov rdi, [rdx + rsi * 8 + last_slot]
        ; mov [rax + rsi * 8 + last_slot], rdi
        ; sub esi, 1
        ; jnz =>copy
        ; =>copied
    );
    emit_publish_context(ops, view);
}

/// Carve the closure of one `MakeClosure` / `MakeFunction` site over the
/// context value in `rdx` (`undefined` for `MakeFunction`) and return it in
/// `rax`. An arrow's lexical `this` and `new.target` come from the native
/// frame in `Rq(frame)`. `slow` is taken before any effect when the context
/// operand is neither `undefined` nor a context. Clobbers `rax`, `rsi`,
/// `rdi`, `r9`–`r11`; `rdx` and `Rq(frame)` survive.
pub(crate) fn emit_closure(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    plan: JitClosureAllocationPlan,
    frame: u8,
    slow: DynamicLabel,
) {
    let layout = view.closure_call_layout;
    let context_ready = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; mov rdi, QWORD VALUE_UNDEFINED as i64
        ; cmp rdx, rdi
        ; je =>context_ready
        ; mov rdi, QWORD otter_vm::value::tag::NOT_CELL_MASK as i64
        ; test rdx, rdi
        ; jnz =>slow
        ; cmp BYTE [rdx], view.context_layout.type_tag as i8
        ; jne =>slow
        ; =>context_ready
    );
    let base_bytes = JIT_CLOSURE_CELL_BYTES + if plan.arrow { 8 } else { 0 };
    dynasm!(ops ; .arch x64 ; mov r9d, base_bytes as i32);
    if plan.arrow {
        // An arrow over a defined `new.target` also carries that word.
        let sized = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch x64
            ; mov rsi, [Rq(frame) + NATIVE_FRAME_NEW_TARGET_OFFSET as i32]
            ; mov rdi, QWORD VALUE_UNDEFINED as i64
            ; cmp rsi, rdi
            ; je =>sized
            ; add r9d, 8
            ; =>sized
        );
    }
    emit_bump_probe(ops, slow);
    dynasm!(ops
        ; .arch x64
        ; mov rdi, r9
        ; shl rdi, 32
        ; or rdi, JIT_YOUNG_CLOSURE_HEADER_WORD as i32
        ; mov [rax], rdi
        ; mov rdi, QWORD plan.call_word as i64
    );
    if plan.arrow {
        let call_word_ready = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch x64
            ; cmp r9d, (JIT_CLOSURE_CELL_BYTES + 8) as i32
            ; je =>call_word_ready
            ; mov [rax + layout.bound_new_target_byte as i32], rsi
            ; mov rsi, QWORD JitClosureAllocationPlan::bound_new_target_word() as i64
            ; or rdi, rsi
            ; =>call_word_ready
            ; mov rsi, [Rq(frame) + NATIVE_FRAME_THIS_OFFSET as i32]
            ; mov [rax + layout.bound_this_byte as i32], rsi
        );
    }
    dynasm!(ops
        ; .arch x64
        ; mov [rax + layout.function_id_byte as i32], rdi
        ; mov [rax + layout.context_byte as i32], rdx
        // The rare handle and the last-instance observation start null.
        ; mov QWORD [rax + layout.rare_byte as i32], 0
        ; mov [r11 + LAB_TOP_OFFSET as i32], r10
    );
    emit_count_allocation(ops, otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG, 9, 11);
}
