//! Generated nursery allocation shared by the AArch64 tiers.
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
//! - A carve reads the linear allocation buffer through the entry context's
//!   allocation window. A disabled window names an empty buffer whose bump
//!   always misses, so the heap turns generated allocation off by emptying
//!   it (marking, stress, tenuring and heap caps all do).
//! - Every word of a cell is written before the bump cursor is published,
//!   and nothing between the bump probe and the publication can collect.
//! - A miss branches before any effect, with the caller's input registers
//!   and every register outside the documented clobber set intact.
//! - A fresh cell is young and unmarked, so its initializing stores need no
//!   write barrier.
//!
//! # See also
//! - `otter_vm::context` — the context body and its initialization contract.
//! - `otter_gc::heap` — the Rust-side buffer bump these carves mirror.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::JitCompileSnapshot;
use otter_vm::jit::{
    JIT_CLOSURE_CELL_BYTES, JIT_CONTEXT_HAS_EXTENSION_BIT, JIT_INLINE_CONTEXT_MAX_WORDS,
    JIT_TYPE_STATS_ALLOC_BYTES_OFFSET, JIT_TYPE_STATS_ALLOC_COUNT_OFFSET,
    JIT_TYPE_STATS_LIVE_BYTES_OFFSET, JIT_TYPE_STATS_ROW_BYTES, JIT_YOUNG_CLOSURE_HEADER_WORD,
    JIT_YOUNG_CONTEXT_HEADER_WORD, JitClosureAllocationPlan, JitContextAllocationPlan,
};

use crate::entry::{
    ALLOC_WINDOW_LAB_OFFSET, ALLOC_WINDOW_TYPE_STATS_OFFSET, LAB_LIMIT_OFFSET, LAB_TOP_OFFSET,
    NATIVE_FRAME_NEW_TARGET_OFFSET, NATIVE_FRAME_THIS_OFFSET, VALUE_UNDEFINED,
};
use crate::template::arm64::values::{CellTest, emit_cell_test, emit_load_u64};

/// Account one generated allocation of `X(size)` bytes against the
/// statistics row of `type_tag`. Clobbers `X(base)` and `X(scratch)`.
pub(crate) fn emit_count_allocation(
    ops: &mut Assembler,
    context_register: u8,
    type_tag: u8,
    size: u8,
    base: u8,
    scratch: u8,
) {
    let row = u32::from(type_tag) * JIT_TYPE_STATS_ROW_BYTES;
    let live = row + JIT_TYPE_STATS_LIVE_BYTES_OFFSET;
    let count = row + JIT_TYPE_STATS_ALLOC_COUNT_OFFSET;
    let bytes = row + JIT_TYPE_STATS_ALLOC_BYTES_OFFSET;
    dynasm!(ops
        ; .arch aarch64
        ; ldr X(base), [X(context_register), ALLOC_WINDOW_TYPE_STATS_OFFSET]
        ; ldr X(scratch), [X(base), live]
        ; add X(scratch), X(scratch), X(size)
        ; str X(scratch), [X(base), live]
        ; ldr X(scratch), [X(base), count]
        ; add XSP(scratch), XSP(scratch), 1
        ; str X(scratch), [X(base), count]
        ; ldr X(scratch), [X(base), bytes]
        ; add X(scratch), X(scratch), X(size)
        ; str X(scratch), [X(base), bytes]
    );
}

/// Bump probe for a cell of `X(size)` bytes: `x13` = the buffer, `x16` =
/// the cell, `x12` = the new cursor. Branches to `slow` when the cell does
/// not fit. Clobbers `x14`.
fn emit_bump_probe(ops: &mut Assembler, context_register: u8, size: u8, slow: DynamicLabel) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr x13, [X(context_register), ALLOC_WINDOW_LAB_OFFSET]
        ; ldr x16, [x13, LAB_TOP_OFFSET]
        ; ldr x14, [x13, LAB_LIMIT_OFFSET]
        ; add x12, x16, X(size)
        ; cmp x12, x14
        ; b.hi =>slow
    );
}

/// Publish the cell in `x16` by storing the cursor `x12` into buffer `x13`
/// and account `X(size)` bytes to `type_tag`. The cell stays in `x16`.
fn emit_publish(ops: &mut Assembler, context_register: u8, type_tag: u8, size: u8) {
    dynasm!(ops ; .arch aarch64 ; str x12, [x13, LAB_TOP_OFFSET]);
    emit_count_allocation(ops, context_register, type_tag, size, 13, 14);
}

/// Carve `CreateContext` for `plan` under the parent value in `X(parent)`
/// and return it in `x0`; `slow` is taken before any effect. Clobbers `x0`,
/// `x9`, `x12`–`x17`. `parent` must lie outside the clobber set.
pub(crate) fn emit_create_context(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    plan: &JitContextAllocationPlan,
    context_register: u8,
    parent: u8,
    slow: DynamicLabel,
) {
    let layout = view.context_layout;
    emit_load_u64(ops, 17, u64::from(plan.cell_bytes));
    emit_bump_probe(ops, context_register, 17, slow);
    emit_load_u64(ops, 14, plan.header_word);
    dynasm!(ops ; .arch aarch64 ; str x14, [x16]);
    emit_load_u64(ops, 14, plan.body_word);
    dynasm!(ops
        ; .arch aarch64
        ; str x14, [x16, layout.scope_function_id_byte]
        ; str X(parent), [x16, layout.parent_byte]
    );
    // Trailing words take at most two distinct values (`hole` and
    // `undefined`); each lives in one register for the whole fill.
    let mut cached: [(u8, Option<u64>); 2] = [(14, None), (15, None)];
    for (index, &word) in plan.initial_words.iter().enumerate() {
        let register =
            if let Some(&(register, _)) = cached.iter().find(|(_, value)| *value == Some(word)) {
                register
            } else if let Some(entry) = cached.iter_mut().find(|(_, value)| value.is_none()) {
                entry.1 = Some(word);
                emit_load_u64(ops, entry.0, word);
                entry.0
            } else {
                emit_load_u64(ops, 9, word);
                9
            };
        let offset = layout.slots_byte + 8 * index as u32;
        dynasm!(ops ; .arch aarch64 ; str X(register), [x16, offset]);
    }
    emit_publish(ops, context_register, layout.type_tag, 17);
    dynasm!(ops ; .arch aarch64 ; mov x0, x16);
}

/// Carve `CopyContext` from the context in `X(source)` and return the copy
/// in `x0`; `slow` is taken before any effect for a scope carrying an
/// eval-extension word or more than [`JIT_INLINE_CONTEXT_MAX_WORDS`] slots.
/// Clobbers `x0`, `x9`–`x17`. `source` must lie outside the clobber set.
pub(crate) fn emit_copy_context(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    context_register: u8,
    source: u8,
    slow: DynamicLabel,
) {
    let layout = view.context_layout;
    let copy = ops.new_dynamic_label();
    let copied = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; ldrh w14, [X(source), layout.scope_index_byte]
        ; tst w14, 1u32 << JIT_CONTEXT_HAS_EXTENSION_BIT ; b.ne =>slow
        ; ldrh w15, [X(source), layout.slot_count_byte]
        ; cmp w15, JIT_INLINE_CONTEXT_MAX_WORDS as u32
        ; b.hi =>slow
        ; lsl w17, w15, 3
        ; add w17, w17, layout.slots_byte
    );
    emit_bump_probe(ops, context_register, 17, slow);
    emit_load_u64(ops, 14, JIT_YOUNG_CONTEXT_HEADER_WORD);
    dynasm!(ops
        ; .arch aarch64
        ; orr x14, x14, x17, lsl 32
        ; str x14, [x16]
        // Scope identity, slot count and parent carry over verbatim.
        ; ldr x14, [X(source), layout.scope_function_id_byte]
        ; str x14, [x16, layout.scope_function_id_byte]
        ; ldr x14, [X(source), layout.parent_byte]
        ; str x14, [x16, layout.parent_byte]
        ; cbz w15, =>copied
        ; add x9, XSP(source), layout.slots_byte
        ; add x10, x16, layout.slots_byte
        ; mov x11, xzr
        ; =>copy
        ; ldr x14, [x9, x11, lsl 3]
        ; str x14, [x10, x11, lsl 3]
        ; add x11, x11, 1
        ; cmp x11, x15
        ; b.lo =>copy
        ; =>copied
    );
    emit_publish(ops, context_register, layout.type_tag, 17);
    dynasm!(ops ; .arch aarch64 ; mov x0, x16);
}

/// Carve the closure of one `MakeClosure` / `MakeFunction` site over the
/// context value in `x15` (`undefined` for `MakeFunction`) and return it in
/// `x16`. An arrow's lexical `this` and `new.target` come from the native
/// frame in `X(frame)`. `slow` is taken before any effect when the context
/// operand is neither `undefined` nor a context. Clobbers `x9`, `x12`–`x14`,
/// `x16`, `x17`; `x15` and `X(frame)` survive.
pub(crate) fn emit_closure(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    plan: JitClosureAllocationPlan,
    context_register: u8,
    frame: u8,
    slow: DynamicLabel,
) {
    let layout = view.closure_call_layout;
    let context_ready = ops.new_dynamic_label();
    emit_load_u64(ops, 14, VALUE_UNDEFINED);
    dynasm!(ops ; .arch aarch64 ; cmp x15, x14 ; b.eq =>context_ready);
    emit_cell_test(ops, 15, 14, CellTest::IsNotCell, slow);
    dynasm!(ops
        ; .arch aarch64
        ; ldrb w14, [x15]
        ; cmp w14, u32::from(view.context_layout.type_tag)
        ; b.ne =>slow
        ; =>context_ready
    );
    // An arrow over a defined `new.target` also carries that word.
    let sized = ops.new_dynamic_label();
    let base_bytes = JIT_CLOSURE_CELL_BYTES + if plan.arrow { 8 } else { 0 };
    emit_load_u64(ops, 17, u64::from(base_bytes));
    if plan.arrow {
        emit_load_u64(ops, 14, VALUE_UNDEFINED);
        dynasm!(ops
            ; .arch aarch64
            ; ldr x9, [X(frame), NATIVE_FRAME_NEW_TARGET_OFFSET]
            ; cmp x9, x14
            ; b.eq =>sized
            ; add x17, x17, 8
            ; =>sized
        );
    }
    emit_bump_probe(ops, context_register, 17, slow);
    emit_load_u64(ops, 14, JIT_YOUNG_CLOSURE_HEADER_WORD);
    dynasm!(ops
        ; .arch aarch64
        ; orr x14, x14, x17, lsl 32
        ; str x14, [x16]
    );
    emit_load_u64(ops, 14, plan.call_word);
    if plan.arrow {
        let call_word_ready = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; cmp x17, JIT_CLOSURE_CELL_BYTES + 8
            ; b.eq =>call_word_ready
            ; str x9, [x16, layout.bound_new_target_byte]
        );
        emit_load_u64(ops, 9, JitClosureAllocationPlan::bound_new_target_word());
        dynasm!(ops
            ; .arch aarch64
            ; orr x14, x14, x9
            ; =>call_word_ready
            ; ldr x9, [X(frame), NATIVE_FRAME_THIS_OFFSET]
            ; str x9, [x16, layout.bound_this_byte]
        );
    }
    dynasm!(ops
        ; .arch aarch64
        ; str x14, [x16, layout.function_id_byte]
        ; str x15, [x16, layout.context_byte]
        // The rare handle and the last-instance observation start null.
        ; str xzr, [x16, layout.rare_byte]
    );
    emit_publish(
        ops,
        context_register,
        otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG,
        17,
    );
}
