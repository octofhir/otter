//! The one native frame a generated call builds and retires, shared by the
//! baseline and graph tiers.
//!
//! # Contents
//! - [`SpillArea`] — tier-owned slots reserved below the register window
//!   and the safepoint that roots their tagged part.
//! - [`emit_call_entry`] — the JavaScript call ABI entry: frame record and
//!   register window built in the callee prologue, entry accounting,
//!   receiver binding and publication.
//! - [`emit_publish_lazy_window`] — the window of a record whose entry left
//!   it unpublished, published by the exit that rebuilds the frame.
//! - [`emit_tier_prologue`] — the entry over an already published
//!   interpreter frame (function-entry tier transfer and loop OSR).
//! - [`emit_epilogue`] / [`emit_exits`] — constructor completion,
//!   unpublication, and side exits that resume the interpreter in place.
//! - [`emit_tail_admission`], [`emit_tail_span_check`],
//!   [`emit_tail_transfer`] and [`emit_tail_return`] — a proper tail call
//!   (§15.10.3): the callee takes the place of this called record, in place
//!   when its actuals fit the record's span and through the caller
//!   otherwise.
//!
//! # Invariants
//! - Body registers: `x19` register window, `x20` context, `x21` published
//!   frame, `x29` this native frame.
//! - `[x29 + 40]` holds the frame to publish on return: the caller of a
//!   called record, or the published interpreter frame itself under a tier
//!   entry. Bit 0 marks a record that owes constructor completion.
//! - A call entry builds its record directly below `x29`, then the saved
//!   callee-saved pairs, the window and the spill area (lowest, at `sp`), and
//!   publishes the record after every field and register is initialized.
//!   The record's return address is the link register saved at `[x29 + 8]`. Nothing allocates or reenters
//!   before publication. A lazy-window entry publishes a record without a
//!   window and points `x19` at its actual span when every formal is present.
//!   Underarity initializes and publishes the reserved window before any
//!   allocating helper; an exit that rebuilds an unpublished interpreter
//!   frame also publishes the reserved window.
//! - Only a baseline generation counts its entries toward promotion.
//! - The native stack limit alone bounds call depth; it rejects entry before
//!   the reservation, accounting, receiver preparation or frame publication.
//!   Records carry no depth: the runtime derives it on first use.
//! - With a spill area, the record names the area's safepoint and its base
//!   for the body's whole extent; a tier entry saves the interpreter
//!   record's previous words in the area's top 16 bytes and every exit of a
//!   tier frame restores them. Tagged slots are zeroed before publication.
//! - A side exit of a called record continues in the interpreter on the same
//!   record and window and returns its completion; a tier-entered frame
//!   returns the exit to its interpreter.
//! - A called record's actuals start at `[x29 + 48]`, in the span its caller
//!   pushed: actual-count words rounded up to an even count. Only the callee
//!   window contains missing formals. A tail callee's span starts at the same address, so the caller
//!   releases exactly the span it pushed when the callee returns to it.
//!
//! # See also
//! - [`crate::arm64::activation`] — receiver and object tests used here.
//! - [`crate::call_linkage`] — the call contract.

use dynasmrt::{
    AssemblyOffset, DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm,
};
use otter_vm::{JitCompileSnapshot, native_abi as abi};

pub(crate) use crate::arm64::activation::EntryShape;
use crate::entry::TransitionTable;
use crate::frame::{ActivationExits, CallEntryCold, SpillArea};
use crate::template::arm64::values::{emit_load_runtime_stub, emit_load_u64};
use crate::{
    arm64::activation::{emit_lexical_this, emit_object_receiver_test, emit_object_test},
    artifact::relocation::RelocationCapture,
    entry::{
        CODE_ENTRY_GENERATED_ENTRIES_OFFSET, CODE_ENTRY_TIERING_ENABLED_OFFSET,
        CODE_ENTRY_TIERING_WORK_TARGET_OFFSET, GENERATED_FEEDBACK_CLEAN_OFFSET,
        NATIVE_FRAME_OFFSET, NATIVE_FRAME_REGISTER_BASE_OFFSET, NATIVE_FRAME_SELF_OFFSET,
        NATIVE_FRAME_THIS_OFFSET, NATIVE_STACK_LIMIT_OFFSET, THREAD_OFFSET, VALUE_HOLE,
        VALUE_UNDEFINED, VM_THREAD_INTERRUPT_CELL_OFFSET,
    },
};

/// `[x29 + RETURN_FRAME]`: the frame published on return.
const RETURN_FRAME: u32 = 40;
/// Bytes of the record slot directly below `x29`. A call entry builds its
/// record there, so a directly entered record's return address is
/// `[record + RECORD_SLOT + 8]`; a tier entry leaves the slot unused so the
/// callee-saved pairs share one location across both entries.
const RECORD_SLOT: u32 = abi::FRAME_RECORD_BYTES as u32;
const _: () =
    assert!(abi::FRAME_RECORD_SLOT == abi::FRAME_RECORD_BYTES || cfg!(target_arch = "x86_64"));
/// `call_site` and `machine_roots` of the record, as two words: the
/// depth/call-site word at 80 and the roots word at 88.
const DEPTH_CALL_SITE: u32 = abi::NATIVE_FRAME_DEPTH_OFFSET;
const MACHINE_ROOTS: u32 = abi::NATIVE_FRAME_MACHINE_ROOTS_OFFSET;

/// Zero the exception scratch at `sp` before the frame is published.
fn emit_zero_scratch(ops: &mut Assembler, spill: SpillArea) {
    let Some(slot) = spill.scratch_slot else {
        return;
    };
    let offset = slot * 8;
    if offset <= 32760 {
        dynasm!(ops ; .arch aarch64 ; str xzr, [sp, offset]);
    } else {
        emit_load_u64(ops, 16, u64::from(offset));
        dynasm!(ops ; .arch aarch64 ; str xzr, [sp, x16]);
    }
}

/// Bytes the saved callee-saved pairs occupy just below the record slot.
fn saved_bytes(pairs: u8) -> u32 {
    16 * pairs.count_ones()
}

/// `(first register, x29-relative offset)` of each saved pair, the lowest
/// pair highest, below the record slot.
fn saved_slots(pairs: u8) -> impl Iterator<Item = (u8, u32)> {
    (0..4u8)
        .filter(move |pair| pairs & (1 << pair) != 0)
        .enumerate()
        .map(|(index, pair)| (22 + 2 * pair, RECORD_SLOT + 16 * (index as u32 + 1)))
}

/// Save the frame registers and establish `x29`.
fn emit_save(ops: &mut Assembler) {
    dynasm!(ops
        ; .arch aarch64
        ; stp x29, x30, [sp, #-48]!
        ; stp x19, x20, [sp, #16]
        ; str x21, [sp, #32]
        ; mov x29, sp
    );
}

/// Store the callee-saved pairs a Graph body allocates, once `sp` covers
/// their slots.
fn emit_store_saved(ops: &mut Assembler, pairs: u8) {
    for (first, offset) in saved_slots(pairs) {
        let offset = -(offset as i32);
        if first == 28 {
            dynasm!(ops ; .arch aarch64 ; stur x28, [x29, offset]);
        } else {
            dynasm!(ops ; .arch aarch64 ; stp X(first), X(first + 1), [x29, offset]);
        }
    }
}

/// `x19` = the register window of a call-entry record: below the record
/// slot and the saved pairs. Clobbers x16.
fn emit_window_base(ops: &mut Assembler, register_count: u16, pairs: u8) {
    let below =
        RECORD_SLOT + saved_bytes(pairs) + (u32::from(register_count) * 8).next_multiple_of(16);
    emit_load_u64(ops, 16, u64::from(below));
    dynasm!(ops ; .arch aarch64 ; sub x19, x29, x16);
}

/// Reload the callee-saved pairs [`emit_save`] stored.
fn emit_restore_saved(ops: &mut Assembler, pairs: u8) {
    for (first, offset) in saved_slots(pairs) {
        let offset = -(offset as i32);
        if first == 28 {
            dynasm!(ops ; .arch aarch64 ; ldur x28, [x29, offset]);
        } else {
            dynasm!(ops ; .arch aarch64 ; ldp X(first), X(first + 1), [x29, offset]);
        }
    }
}

/// The entry over the published interpreter frame: its window and record
/// become the body's, and the return publishes that frame again.
pub(crate) fn emit_tier_prologue(ops: &mut Assembler, spill: SpillArea) {
    emit_save(ops);
    dynasm!(ops
        ; .arch aarch64
        ; mov x20, x0
        ; ldr x21, [x20, NATIVE_FRAME_OFFSET]
        ; ldr x19, [x21, NATIVE_FRAME_REGISTER_BASE_OFFSET]
        ; str x21, [x29, RETURN_FRAME]
    );
    // The interpreter already checked the stack for its own frame; the
    // area is bounded by the same headroom the call entry checks.
    let below = RECORD_SLOT + saved_bytes(spill.saved_pairs) + spill.bytes;
    if below <= 4095 {
        dynasm!(ops ; .arch aarch64 ; sub sp, sp, below);
    } else {
        emit_load_u64(ops, 16, u64::from(below));
        dynasm!(ops ; .arch aarch64 ; sub sp, sp, x16);
    }
    emit_store_saved(ops, spill.saved_pairs);
    if spill.bytes == 0 {
        return;
    }
    emit_zero_scratch(ops, spill);
    emit_load_u64(ops, 17, u64::from(spill.safepoint));
    let roots = -((RECORD_SLOT + saved_bytes(spill.saved_pairs)) as i32);
    dynasm!(ops
        ; .arch aarch64
        ; ldr x16, [x21, DEPTH_CALL_SITE]
        ; stur x16, [x29, roots - 16]
        ; ldr x16, [x21, MACHINE_ROOTS]
        ; stur x16, [x29, roots - 8]
        ; str w17, [x21, DEPTH_CALL_SITE + 4]
        ; mov x16, sp
        ; str x16, [x21, MACHINE_ROOTS]
    );
}

/// Restore an interpreter record's root words before a tier frame leaves.
/// `x9` holds the return frame; clobbers x16.
fn emit_restore_tier_roots(ops: &mut Assembler, spill: SpillArea) {
    if spill.bytes == 0 {
        return;
    }
    let called = ops.new_dynamic_label();
    let roots = -((RECORD_SLOT + saved_bytes(spill.saved_pairs)) as i32);
    dynasm!(ops
        ; .arch aarch64
        ; cmp x9, x21
        ; b.ne =>called
        ; ldur x16, [x29, roots - 16]
        ; str x16, [x21, DEPTH_CALL_SITE]
        ; ldur x16, [x29, roots - 8]
        ; str x16, [x21, MACHINE_ROOTS]
        ; =>called
    );
}

/// Bytes the call entry reserves below `x29`: the record slot, the saved
/// pairs, the register window and the spill area.
fn reservation(shape: EntryShape, spill: SpillArea) -> u32 {
    RECORD_SLOT
        + saved_bytes(spill.saved_pairs)
        + (u32::from(shape.register_count) * 8).next_multiple_of(16)
        + spill.bytes
}

/// Emit the call-ABI entry. It falls through into the body; its cold
/// continuations, named by `cold`, are emitted by [`emit_call_entry_cold`]
/// just before it.
pub(crate) fn emit_call_entry(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    shape: EntryShape,
    spill: SpillArea,
    cold: CallEntryCold,
) -> AssemblyOffset {
    let start = ops.offset();
    let CallEntryCold {
        overflow,
        break_even,
        promote,
        construct,
        prepare,
        underarity,
    } = cold;
    let bytes = reservation(shape, spill);
    emit_save(ops);
    dynasm!(ops ; .arch aarch64 ; mov x20, x0);
    emit_load_u64(ops, 9, u64::from(bytes));
    dynasm!(ops
        ; .arch aarch64
        ; mov x10, sp
        ; sub x9, x10, x9
        ; ldr x11, [x20, NATIVE_STACK_LIMIT_OFFSET]
        ; cmp x9, x11
        ; b.lo =>overflow
    );
    if bytes >= 4096 {
        // Touch each page before moving SP past a guard page.
        let probe = ops.new_dynamic_label();
        let probed = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; =>probe
            ; sub x10, x10, 1, lsl 12
            ; cmp x10, x9
            ; b.ls =>probed
            ; str xzr, [x10]
            ; b =>probe
            ; =>probed
        );
    }
    dynasm!(ops ; .arch aarch64 ; mov sp, x9 ; sub x21, x29, RECORD_SLOT);
    emit_store_saved(ops, spill.saved_pairs);
    emit_zero_scratch(ops, spill);
    if !shape.lazy_window {
        // The lazy window, below the record and the pairs, is published only
        // by an exit; the body's x19 is set once the record is built.
        if spill.bytes == 0 {
            dynasm!(ops ; .arch aarch64 ; mov x19, x9);
        } else {
            emit_load_u64(ops, 19, u64::from(spill.bytes));
            dynasm!(ops ; .arch aarch64 ; add x19, x9, x19);
        }
    }
    // Only the trampoline tags the aligned generation word. A direct entry's
    // return address is the link register saved at `[x29 + 8]`, read back
    // through the record's anchor; a staged entry consumes the original
    // caller anchor of the request before any generation-cell dereference
    // or frame publication.
    let direct_origin = ops.new_dynamic_label();
    let origin_ready = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; tbz x8, 0, =>direct_origin
        ; and x8, x8, !(abi::CODE_ENTRY_STAGED_REQUEST_MASK as u64)
        ; ldr x16, [x20, crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_CALLER_RETURN_PC_OFFSET]
        ; cmp x16, 0
        ; csinc x16, x16, xzr, ne
        ; stp xzr, xzr, [x20, (crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_CALLER_OFFSET) as i32]
        ; b =>origin_ready
        ; =>direct_origin
        ; mov x16, xzr
        ; =>origin_ready
        ; str x16, [x21, abi::NATIVE_FRAME_CALLER_RETURN_PC_OFFSET]
    );
    if let Some((check, back)) = break_even {
        // Entry diagnostics and canonical source-work admission for promotion.
        dynasm!(ops
            ; .arch aarch64
            ; mov x6, xzr
            ; ldr x9, [x8, CODE_ENTRY_GENERATED_ENTRIES_OFFSET]
            ; add x9, x9, 1
            ; str x9, [x8, CODE_ENTRY_GENERATED_ENTRIES_OFFSET]
            ; str xzr, [x20, GENERATED_FEEDBACK_CLEAN_OFFSET]
        );
        crate::template::arm64::values::emit_load_symbol_u64(
            ops,
            relocations,
            9,
            view.code_block.source_work().native_address() as u64,
            crate::artifact::relocation::RelocationTarget::SourceWorkCell {
                function_id: shape.function_id,
            },
        );
        // A Template activation charges its body's opcode count once on entry,
        // the way an interrupt budget charges a return; loops charge at polls.
        emit_load_u64(ops, 11, crate::frame::entry_work(view));
        dynasm!(ops
            ; .arch aarch64
            ; ldr x10, [x9]
            ; adds x10, x10, x11
            ; csinv x10, x10, xzr, cc
            ; str x10, [x9]
            ; ldr x11, [x8, CODE_ENTRY_TIERING_WORK_TARGET_OFFSET]
            ; cmp x10, x11
            ; b.hs =>check
            ; =>back
        );
    }
    if shape.sloppy_receiver() {
        emit_object_receiver_test(ops, view);
    }
    if shape.lexical_this {
        emit_lexical_this(ops, view);
    }
    if shape.derived {
        emit_load_u64(ops, 2, VALUE_HOLE);
    }
    dynasm!(ops ; .arch aarch64 ; ldr x10, [x20, NATIVE_FRAME_OFFSET]);
    // A derived constructor is entered only by `[[Construct]]` and always
    // owes constructor completion.
    if shape.derived {
        dynasm!(ops ; .arch aarch64 ; orr x13, x10, 1 ; str x13, [x29, RETURN_FRAME]);
    } else {
        dynasm!(ops ; .arch aarch64 ; str x10, [x29, RETURN_FRAME]);
    }
    let window_slots = if shape.lazy_window {
        0
    } else {
        shape.register_count
    };
    emit_load_u64(ops, 13, u64::from(shape.function_id));
    emit_load_u64(ops, 14, shape.header_word(window_slots));
    dynasm!(ops ; .arch aarch64 ; stp x13, x14, [x21]);
    if shape.lazy_window {
        dynasm!(ops
            ; .arch aarch64
            ; stp xzr, xzr, [x21, (NATIVE_FRAME_REGISTER_BASE_OFFSET) as i32]
        );
    } else {
        emit_load_u64(ops, 14, u64::from(shape.register_count));
        dynasm!(ops
            ; .arch aarch64
            ; stp x19, x14, [x21, (NATIVE_FRAME_REGISTER_BASE_OFFSET) as i32]
        );
    }
    dynasm!(ops
        ; .arch aarch64
        ; stp x2, x3, [x21, (NATIVE_FRAME_THIS_OFFSET) as i32]
        ; stp x1, x4, [x21, (NATIVE_FRAME_SELF_OFFSET) as i32]
    );
    emit_construct_request(ops);
    dynasm!(ops
        ; .arch aarch64
        ; add x15, x29, 48
        ; stp x15, x10, [x21, (abi::NATIVE_FRAME_ACTUALS_OFFSET) as i32]
    );
    // The depth half stays zero: the runtime derives it on first use.
    if spill.bytes == 0 {
        dynasm!(ops
            ; .arch aarch64
            ; orr x16, xzr, 0xffff_ffff_0000_0000
            ; stp x16, xzr, [x21, (abi::NATIVE_FRAME_DEPTH_OFFSET) as i32]
        );
    } else if spill.safepoint == abi::NO_SAFEPOINT {
        dynasm!(ops
            ; .arch aarch64
            ; orr x16, xzr, 0xffff_ffff_0000_0000
            ; mov x17, sp
            ; stp x16, x17, [x21, (abi::NATIVE_FRAME_DEPTH_OFFSET) as i32]
        );
    } else {
        emit_load_u64(ops, 16, u64::from(spill.safepoint) << 32);
        dynasm!(ops
            ; .arch aarch64
            ; mov x17, sp
            ; stp x16, x17, [x21, (abi::NATIVE_FRAME_DEPTH_OFFSET) as i32]
        );
    }
    dynasm!(ops
        ; .arch aarch64
        ; movn w16, 0
        ; str x16, [x21, abi::NATIVE_FRAME_CONTINUATION_OFFSET]
    );
    if shape.lazy_window {
        // The body reads its formals where the caller pushed them.
        dynasm!(ops ; .arch aarch64 ; add x19, x29, 48);
    } else {
        emit_fill_window(ops, shape);
    }
    dynasm!(ops ; .arch aarch64 ; str x21, [x20, NATIVE_FRAME_OFFSET]);
    if let Some((cold, back)) = underarity {
        emit_load_u64(ops, 9, u64::from(shape.param_count));
        dynasm!(ops ; .arch aarch64 ; cmp w4, w9 ; b.lo =>cold ; =>back);
    }
    if let Some((cold, _)) = construct {
        dynasm!(ops ; .arch aarch64 ; cmp x3, VALUE_UNDEFINED as u32 ; b.ne =>cold);
    }
    if let Some((cold, back)) = prepare {
        dynasm!(ops ; .arch aarch64 ; cbnz x5, =>cold ; =>back);
    }
    if let Some((_, back)) = construct {
        dynasm!(ops ; .arch aarch64 ; =>back);
    }
    if let Some((promote, promoted)) = promote {
        dynasm!(ops ; .arch aarch64 ; cbnz x6, =>promote ; =>promoted);
    }
    start
}

/// Publish the register window of a record whose entry left it
/// unpublished: the slots reserved just below the record, all `undefined`,
/// become the record's window in `x19`. A record with a window keeps it.
/// Runs where no live value is in a register; clobbers `x16` and `x17`.
pub(crate) fn emit_publish_lazy_window(ops: &mut Assembler, register_count: u16, pairs: u8) {
    let published = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; ldr x16, [x21, NATIVE_FRAME_REGISTER_BASE_OFFSET]
        ; cbnz x16, =>published
    );
    emit_window_base(ops, register_count, pairs);
    emit_load_u64(ops, 16, VALUE_UNDEFINED);
    for index in 0..u32::from(register_count) {
        let offset = index * 8;
        if offset <= 32760 {
            dynasm!(ops ; .arch aarch64 ; str x16, [x19, offset]);
        } else {
            emit_load_u64(ops, 17, u64::from(offset));
            dynasm!(ops ; .arch aarch64 ; str x16, [x19, x17]);
        }
    }
    emit_load_u64(ops, 16, u64::from(register_count));
    dynasm!(ops
        ; .arch aarch64
        ; stp x19, x16, [x21, (NATIVE_FRAME_REGISTER_BASE_OFFSET) as i32]
        ; strh w16, [x21, abi::NATIVE_FRAME_REGISTER_COUNT_OFFSET]
        ; =>published
    );
}

/// Move the pending construction request into the frame record.
///
/// Every consumer of the request (this prologue and the call trampoline)
/// clears it, so outside a `[[Construct]]` it is already clear and an
/// ordinary call (`new.target` undefined) only initializes its own record.
/// Clobbers `x15`–`x17`.
fn emit_construct_request(ops: &mut Assembler) {
    const _: () = assert!(
        abi::NATIVE_FRAME_CONSTRUCT_RECEIVER_OFFSET
            == abi::NATIVE_FRAME_CONSTRUCT_LAYOUT_OFFSET + 8
    );
    let ordinary = ops.new_dynamic_label();
    let recorded = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, VALUE_UNDEFINED as u32
        ; cmp x3, x15
        ; b.eq =>ordinary
        ; ldr w16, [x20, crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_CONSTRUCT_LAYOUT_OFFSET]
        ; ldr x17, [x20, crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_CONSTRUCT_RECEIVER_OFFSET]
        ; stp x16, x17, [x21, (abi::NATIVE_FRAME_CONSTRUCT_LAYOUT_OFFSET) as i32]
        ; ldr x16, [x20, crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_SUPER_ORIGIN_OFFSET]
        ; str x16, [x21, abi::NATIVE_FRAME_SUPER_ORIGIN_OFFSET]
        ; str xzr, [x20, crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_SUPER_ORIGIN_OFFSET]
        ; str wzr, [x20, crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_CONSTRUCT_LAYOUT_OFFSET]
        ; str x15, [x20, crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_CONSTRUCT_RECEIVER_OFFSET]
        ; b =>recorded
        ; =>ordinary
        ; stp xzr, x15, [x21, (abi::NATIVE_FRAME_CONSTRUCT_LAYOUT_OFFSET) as i32]
        ; str xzr, [x21, abi::NATIVE_FRAME_SUPER_ORIGIN_OFFSET]
        ; =>recorded
    );
}

/// Seed the complete window from only the supplied actuals. Missing formals
/// and non-parameter registers receive `undefined`. Clobbers x9–x12.
fn emit_fill_window(ops: &mut Assembler, shape: EntryShape) {
    let params = u32::from(shape.param_count.min(shape.register_count));
    let registers = u32::from(shape.register_count);
    let missing = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    if params != 0 {
        emit_load_u64(ops, 9, u64::from(params));
        dynasm!(ops ; .arch aarch64 ; cmp w4, w9 ; b.lo =>missing);
    }
    let mut index = 0;
    while index + 1 < params && 48 + index * 8 + 8 <= 504 {
        let offset = index * 8;
        dynasm!(ops
            ; .arch aarch64
            ; ldp x9, x10, [x29, (48 + offset) as i32]
            ; stp x9, x10, [x19, (offset) as i32]
        );
        index += 2;
    }
    while index < params {
        emit_load_u64(ops, 11, u64::from(48 + index * 8));
        emit_load_u64(ops, 12, u64::from(index * 8));
        dynasm!(ops ; .arch aarch64 ; ldr x9, [x29, x11] ; str x9, [x19, x12]);
        index += 1;
    }
    if index != registers {
        emit_load_u64(ops, 9, VALUE_UNDEFINED);
    }
    while index + 1 < registers && index < 62 {
        dynasm!(ops ; .arch aarch64 ; stp x9, x9, [x19, (index * 8) as i32]);
        index += 2;
    }
    if index < registers {
        let fill = ops.new_dynamic_label();
        emit_load_u64(ops, 11, u64::from(index));
        emit_load_u64(ops, 12, u64::from(registers));
        dynasm!(ops
            ; .arch aarch64
            ; =>fill
            ; str x9, [x19, x11, lsl #3]
            ; add x11, x11, 1
            ; cmp x11, x12
            ; b.lo =>fill
        );
    }
    if params != 0 {
        dynasm!(ops
            ; .arch aarch64
            ; b =>done
            ; =>missing
        );
        emit_fill_underarity_window(ops, registers);
        dynasm!(ops ; .arch aarch64 ; =>done);
    }
}

/// Initialize an underarity window: `argc < params <= registers`, so at least
/// one undefined word follows the copied prefix. No actual-span slack is read.
/// Clobbers only x9/x11/x12, preserving call metadata and preparation flags.
fn emit_fill_underarity_window(ops: &mut Assembler, registers: u32) {
    let copy = ops.new_dynamic_label();
    let fill = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; mov x11, xzr
        ; add x12, x29, 48
        ; cbz w4, =>fill
        ; =>copy
        ; ldr x9, [x12, x11, lsl #3]
        ; str x9, [x19, x11, lsl #3]
        ; add x11, x11, 1
        ; cmp w11, w4
        ; b.lo =>copy
        ; =>fill
    );
    emit_load_u64(ops, 9, VALUE_UNDEFINED);
    emit_load_u64(ops, 12, u64::from(registers));
    let fill_word = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; =>fill_word
        ; str x9, [x19, x11, lsl #3]
        ; add x11, x11, 1
        ; cmp x11, x12
        ; b.lo =>fill_word
    );
}

/// Emit the cold continuations of the call entry, ahead of the entry itself
/// so its conditional branches stay in reach.
pub(crate) fn emit_call_entry_cold(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    view: &JitCompileSnapshot,
    exits: ActivationExits,
    shape: EntryShape,
    saved_pairs: u8,
    cold: CallEntryCold,
) {
    if let Some((missing, ready)) = cold.underarity {
        dynasm!(ops ; .arch aarch64 ; =>missing);
        emit_window_base(ops, shape.register_count, saved_pairs);
        emit_fill_underarity_window(ops, u32::from(shape.register_count));
        emit_load_u64(ops, 16, u64::from(shape.register_count));
        // Publish every initialized register before receiver conversion or
        // constructor allocation can collect. Adequate-actual Graph entries
        // retain the zero-window fast path and read the rooted actual span.
        dynasm!(ops
            ; .arch aarch64
            ; stp x19, x16, [x21, NATIVE_FRAME_REGISTER_BASE_OFFSET as i32]
            ; strh w16, [x21, abi::NATIVE_FRAME_REGISTER_COUNT_OFFSET]
            ; b =>ready
        );
    }
    // Promotion is requested only while this generation may still tier up.
    if let Some((check, back)) = cold.break_even {
        dynasm!(ops
            ; .arch aarch64
            ; =>check
            ; ldr w10, [x8, CODE_ENTRY_TIERING_ENABLED_OFFSET]
            ; cbz w10, =>back
            ; movz x6, 1
            ; b =>back
        );
    }
    // [[Construct]] of a base constructor: owe constructor completion and
    // create the receiver unless the caller allocated it. Receiver
    // conversion does not apply to the created receiver.
    if let Some((construct, constructed)) = cold.construct {
        let created = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; =>construct
            ; ldrb w9, [x21, crate::entry::NATIVE_FRAME_FLAGS_OFFSET]
            ; orr w9, w9, u32::from(abi::NativeFrameFlags::CONSTRUCT)
            ; strb w9, [x21, crate::entry::NATIVE_FRAME_FLAGS_OFFSET]
            ; ldr x9, [x29, RETURN_FRAME]
            ; orr x9, x9, 1
            ; str x9, [x29, RETURN_FRAME]
            ; cmp x2, VALUE_UNDEFINED as u32
            ; b.ne =>created
            // x6 is the entered generation's promotion decision, not a C
            // callee-saved register. Keep it across both leaves and the cold
            // preparation, without changing the canonical frame geometry.
            ; sub sp, sp, #16
            ; str x6, [sp]
        );
        let canonical = ops.new_dynamic_label();
        let complete = ops.new_dynamic_label();
        crate::arm64::allocation::emit_dynamic_construct_receiver(
            ops,
            relocations,
            transitions,
            view,
            complete,
            canonical,
        );
        dynasm!(ops ; .arch aarch64 ; =>canonical ; mov x0, x20);
        emit_stub(
            ops,
            relocations,
            transitions,
            abi::STUB_JIT_PREPARE_ACTIVATION,
        );
        // The epilogue follows the body: `b` reaches it at any size.
        dynasm!(ops
            ; .arch aarch64
            ; =>complete
            ; ldr x6, [sp]
            ; add sp, sp, #16
            ; cbz x1, =>created
            ; b =>exits.construct
            ; =>created
            ; b =>constructed
        );
    }
    if let Some((prepare, prepared)) = cold.prepare {
        dynasm!(ops ; .arch aarch64 ; =>prepare ; mov x0, x20);
        emit_stub(
            ops,
            relocations,
            transitions,
            abi::STUB_JIT_PREPARE_ACTIVATION,
        );
        dynasm!(ops
            ; .arch aarch64
            ; cbz x1, =>prepared
            ; b =>exits.construct
        );
    }
    // Promotion compiles against the published record; this activation
    // keeps its generation and later entries take the new one.
    if let Some((promote, promoted)) = cold.promote {
        dynasm!(ops ; .arch aarch64 ; =>promote ; mov x0, x20);
        emit_stub(ops, relocations, transitions, abi::STUB_JIT_PROMOTE_ENTERED);
        dynasm!(ops ; .arch aarch64 ; b =>promoted);
    }
    // Nothing is published: return the overflow. The body never ran, so
    // the callee-saved pairs still hold their caller's values.
    dynasm!(ops ; .arch aarch64 ; =>cold.overflow ; mov x0, x20);
    emit_stub(ops, relocations, transitions, abi::STUB_JIT_CALL_OVERFLOW);
    emit_restore(ops, 0);
}

fn emit_stub(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    stub: abi::RuntimeStubDescriptor,
) {
    emit_load_runtime_stub(ops, relocations, 16, transitions.entry(stub), stub);
    dynasm!(ops ; .arch aarch64 ; blr x16);
}

/// Release this native frame and return `x0`/`x1`.
fn emit_restore(ops: &mut Assembler, pairs: u8) {
    emit_restore_saved(ops, pairs);
    dynasm!(ops
        ; .arch aarch64
        ; mov sp, x29
        ; ldr x21, [sp, #32]
        ; ldp x19, x20, [sp, #16]
        ; ldp x29, x30, [sp], #48
        ; ret
    );
}

/// Return `x0`/`x1`: constructor completion when the record owes it, then
/// publish the return frame and release this native frame.
pub(crate) fn emit_epilogue(ops: &mut Assembler, exits: ActivationExits, spill: SpillArea) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr x9, [x29, RETURN_FRAME]
        ; tbnz x9, 0, =>exits.construct
    );
    emit_restore_tier_roots(ops, spill);
    dynasm!(ops ; .arch aarch64 ; str x9, [x20, NATIVE_FRAME_OFFSET]);
    emit_restore(ops, spill.saved_pairs);
}

/// Emit the shared constructor completion and side-exit continuation.
pub(crate) fn emit_exits(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    view: &JitCompileSnapshot,
    derived: bool,
    exits: ActivationExits,
    spill: SpillArea,
) {
    // §10.2.2 steps 10–12; an abrupt completion passes through.
    let done = ops.new_dynamic_label();
    let primitive = ops.new_dynamic_label();
    let no_ticket = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; =>exits.construct ; cbnz x1, =>done);
    emit_object_test(ops, view, 0, done, primitive);
    dynasm!(ops ; .arch aarch64 ; =>primitive);
    if derived {
        dynasm!(ops ; .arch aarch64 ; mov x1, x0 ; mov x0, x20);
        emit_stub(
            ops,
            relocations,
            transitions,
            abi::STUB_JIT_DERIVED_CONSTRUCT_RESULT,
        );
    } else {
        dynasm!(ops ; .arch aarch64 ; ldr x0, [x21, NATIVE_FRAME_THIS_OFFSET]);
    }
    dynasm!(ops
        ; .arch aarch64
        ; =>done
        ; ldr w9, [x21, abi::NATIVE_FRAME_CONSTRUCT_LAYOUT_OFFSET]
        ; ldr x10, [x21, abi::NATIVE_FRAME_SUPER_ORIGIN_OFFSET]
        ; orr x9, x9, x10
        ; cbz x9, =>no_ticket
        ; sub sp, sp, 16
        ; stp x0, x1, [sp]
        ; mov x1, sp
        ; mov x0, x20
    );
    emit_stub(
        ops,
        relocations,
        transitions,
        abi::STUB_JIT_CONSTRUCTOR_TERMINAL,
    );
    dynasm!(ops
        ; .arch aarch64
        ; add sp, sp, 16
        ; =>no_ticket
        ; ldr x9, [x29, RETURN_FRAME]
        ; and x9, x9, -2i64 as u64
        ; str x9, [x20, NATIVE_FRAME_OFFSET]
    );
    emit_restore(ops, spill.saved_pairs);
    // A called record continues in the interpreter on its own window; a
    // tier-entered frame hands the exit back to its interpreter.
    let tier = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; =>exits.side_exit
        ; ldr x9, [x29, RETURN_FRAME]
        ; and x9, x9, -2i64 as u64
        ; cmp x9, x21
        ; b.eq =>tier
    );
    if spill.bytes != 0 {
        // The interpreter continues on this record: it roots its window only.
        dynasm!(ops
            ; .arch aarch64
            ; movn w16, 0
            ; str w16, [x21, DEPTH_CALL_SITE + 4]
            ; str xzr, [x21, MACHINE_ROOTS]
        );
    }
    dynasm!(ops ; .arch aarch64 ; mov x1, x0 ; mov x0, x20);
    emit_stub(ops, relocations, transitions, abi::STUB_JIT_DEOPT_CALL);
    emit_epilogue(ops, exits, spill);
    dynasm!(ops ; .arch aarch64 ; =>tier);
    emit_restore_tier_roots(ops, spill);
    dynasm!(ops
        ; .arch aarch64
        ; movz x1, abi::NativeResultStatus::SideExit as u32
        ; str x9, [x20, NATIVE_FRAME_OFFSET]
    );
    emit_restore(ops, spill.saved_pairs);
}

/// Branch to `leave` unless this body may hand its record to a tail callee,
/// and to `ordinary` when the record owes constructor completion.
///
/// Only a called record is replaced: a tier-entered frame belongs to its
/// interpreter, which performs the replacement itself, and so does a
/// pending interrupt, since an unbounded tail-call chain has no back-edge
/// to poll at. A constructing record calls ordinarily and completes on the
/// return that follows. Clobbers `x9`, `x10` and `x17`.
pub(crate) fn emit_tail_admission(
    ops: &mut Assembler,
    leave: DynamicLabel,
    ordinary: DynamicLabel,
) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr x9, [x29, RETURN_FRAME]
        ; cmp x9, x21
        ; b.eq =>leave
        ; tbnz x9, 0, =>ordinary
        ; ldr x17, [x20, THREAD_OFFSET]
        ; ldr x10, [x17, VM_THREAD_INTERRUPT_CELL_OFFSET]
        ; ldrb w10, [x10]
        ; cbnz w10, =>leave
    );
}

/// Branch to `outgrown` unless a tail callee's span of `words` (an even
/// count) fits this record's aligned actual span. Clobbers `x9` and `x10`.
pub(crate) fn emit_tail_span_check(ops: &mut Assembler, words: u32, outgrown: DynamicLabel) {
    if words == 0 {
        return;
    }
    emit_load_u64(ops, 10, u64::from(words - 1));
    dynasm!(ops
        ; .arch aarch64
        ; ldr w9, [x21, abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET]
        ; cmp w9, w10
        ; b.lo =>outgrown
    );
}

/// Retire this called record and return `Continue`: the context holds the
/// tail call it staged, and the caller enters that request in its place.
/// [`emit_tail_admission`] passed, so the record owes no constructor
/// completion.
pub(crate) fn emit_tail_return(ops: &mut Assembler, saved_pairs: u8) {
    dynasm!(ops
        ; .arch aarch64
        ; movz x0, VALUE_UNDEFINED as u32
        ; movz x1, abi::NativeResultStatus::Continue as u32
        ; ldr x9, [x29, RETURN_FRAME]
        ; str x9, [x20, NATIVE_FRAME_OFFSET]
    );
    emit_restore(ops, saved_pairs);
}

/// Retire this called record and enter the callee in `x13` in its place.
///
/// The callee's `count` actuals occupy the `bytes` just pushed at `sp`, and
/// [`emit_tail_admission`] and [`emit_tail_span_check`] passed. The record
/// is unpublished, the saved registers and the return address are
/// restored, the span moves up to `[x29 + 48]` where this record's own
/// actuals began, and control branches to `target` with the call ABI
/// registers set; the callee returns straight to this record's caller.
/// Nothing allocates between the unpublication and the callee's entry.
pub(crate) fn emit_tail_transfer(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    bytes: u32,
    count: u32,
    target: crate::arm64::js_call::CallTarget,
    saved_pairs: u8,
) {
    emit_load_u64(ops, 4, u64::from(count));
    emit_restore_saved(ops, saved_pairs);
    dynasm!(ops
        ; .arch aarch64
        ; mov x0, x20
        ; mov x1, x13
        ; movz x2, VALUE_UNDEFINED as u32
        ; movz x3, VALUE_UNDEFINED as u32
        ; ldr x9, [x29, RETURN_FRAME]
        ; str x9, [x20, NATIVE_FRAME_OFFSET]
    );
    // A record the trampoline entered returns into the trampoline: its
    // caller anchor travels to the callee through the request.
    let direct = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; ldr x10, [x21, abi::NATIVE_FRAME_CALLER_RETURN_PC_OFFSET]
        ; cbz x10, =>direct
        ; stp x9, x10, [x20, (crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_CALLER_OFFSET) as i32]
        ; =>direct
        ; add x10, x29, 48
        ; ldr x21, [x29, #32]
        ; ldp x19, x20, [x29, #16]
        ; ldp x29, x30, [x29]
    );
    // The destination lies above the pushed span: copying from the top
    // down reads every word before a store can reach it.
    if bytes <= 512 {
        let mut offset = bytes;
        while offset > 0 {
            offset -= 16;
            dynasm!(ops
                ; .arch aarch64
                ; ldp x11, x12, [sp, offset as i32]
                ; stp x11, x12, [x10, offset as i32]
            );
        }
    } else {
        let copy = ops.new_dynamic_label();
        emit_load_u64(ops, 14, u64::from(bytes));
        dynasm!(ops
            ; .arch aarch64
            ; =>copy
            ; sub x14, x14, 16
            ; add x15, sp, x14
            ; ldp x11, x12, [x15]
            ; add x16, x10, x14
            ; stp x11, x12, [x16]
            ; cbnz x14, =>copy
        );
    }
    dynasm!(ops ; .arch aarch64 ; mov sp, x10);
    crate::arm64::js_call::emit_tail_branch(ops, relocations, transitions, target);
}
