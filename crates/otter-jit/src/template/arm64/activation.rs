//! Template activations: the one native frame a call builds and retires.
//!
//! # Contents
//! - [`emit_call_entry`] — the JavaScript call ABI entry: frame record and
//!   register window built in the callee prologue, entry accounting,
//!   receiver binding and publication.
//! - [`emit_tier_prologue`] — the entry over an already published
//!   interpreter frame (function-entry tier transfer and loop OSR).
//! - [`emit_epilogue`] / [`emit_exits`] — constructor completion,
//!   unpublication, and side exits that resume the interpreter in place.
//!
//! # Invariants
//! - Body registers: `x19` register window, `x20` context, `x21` published
//!   frame, `x29` this native frame.
//! - `[x29 + 40]` holds the frame to publish on return: the caller of a
//!   called record, or the published interpreter frame itself under a tier
//!   entry. Bit 0 marks a record that owes constructor completion.
//! - A call entry reserves the window and the record below `x29` and
//!   publishes the record after every field and register is initialized.
//!   Nothing allocates or reenters before publication.
//! - A side exit of a called record continues in the interpreter on the same
//!   record and window and returns its completion; a tier-entered frame
//!   returns the exit to its interpreter.
//!
//! # See also
//! - [`crate::arm64::activation`] — pieces shared with the optimizing tier.
//! - [`crate::call_linkage`] — the call contract.

use dynasmrt::{AssemblyOffset, DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{JitCompileSnapshot, native_abi as abi};

use super::transitions::TransitionTable;
use super::values::{emit_load_runtime_stub, emit_load_u64};
pub(super) use crate::arm64::activation::EntryShape;
use crate::{
    arm64::activation::{emit_lexical_this, emit_object_receiver_test, emit_object_test},
    artifact::relocation::RelocationCapture,
    entry::{
        CODE_ENTRY_GENERATED_ENTRIES_OFFSET, CODE_ENTRY_TIERING_BREAK_EVEN_OFFSET,
        CODE_ENTRY_TIERING_ENABLED_OFFSET, GENERATED_FEEDBACK_CLEAN_OFFSET, NATIVE_FRAME_OFFSET,
        NATIVE_FRAME_REGISTER_BASE_OFFSET, NATIVE_FRAME_SELF_OFFSET, NATIVE_FRAME_THIS_OFFSET,
        NATIVE_STACK_LIMIT_OFFSET, VALUE_HOLE, VALUE_UNDEFINED,
    },
};

/// `[x29 + RETURN_FRAME]`: the frame published on return.
const RETURN_FRAME: u32 = 40;
/// Bytes of the record reserved below the saved registers.
const RECORD_BYTES: u32 = std::mem::size_of::<abi::Frame>() as u32;

/// Labels shared by the exits of one Template body.
#[derive(Debug, Clone, Copy)]
pub(super) struct ActivationExits {
    /// Constructor completion of `x0`/`x1`, then the return.
    pub(super) construct: DynamicLabel,
    /// Side exit with the encoded exit in `x0`.
    pub(super) side_exit: DynamicLabel,
}

/// Cold continuations of the call entry, each a `(cold, back)` pair.
#[derive(Debug, Clone, Copy)]
pub(super) struct CallEntryCold {
    overflow: DynamicLabel,
    break_even: (DynamicLabel, DynamicLabel),
    promote: (DynamicLabel, DynamicLabel),
    construct: Option<(DynamicLabel, DynamicLabel)>,
    prepare: Option<(DynamicLabel, DynamicLabel)>,
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

/// The entry over the published interpreter frame: its window and record
/// become the body's, and the return publishes that frame again.
pub(super) fn emit_tier_prologue(ops: &mut Assembler) {
    emit_save(ops);
    dynasm!(ops
        ; .arch aarch64
        ; mov x20, x0
        ; ldr x21, [x20, NATIVE_FRAME_OFFSET]
        ; ldr x19, [x21, NATIVE_FRAME_REGISTER_BASE_OFFSET]
        ; str x21, [x29, RETURN_FRAME]
    );
}

/// Bytes the call entry reserves below the saved registers: the register
/// window, then the record.
fn reservation(shape: EntryShape) -> u32 {
    (u32::from(shape.register_count) * 8 + RECORD_BYTES).next_multiple_of(16)
}

/// Emit the call-ABI entry. It falls through into the body; its cold
/// continuations are emitted by [`emit_call_entry_cold`].
pub(super) fn emit_call_entry(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    shape: EntryShape,
) -> (AssemblyOffset, CallEntryCold) {
    let start = ops.offset();
    let overflow = ops.new_dynamic_label();
    let break_even = (ops.new_dynamic_label(), ops.new_dynamic_label());
    let bytes = reservation(shape);
    let window = u32::from(shape.register_count) * 8;
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
    dynasm!(ops ; .arch aarch64 ; mov sp, x9 ; mov x19, x9);
    emit_load_u64(ops, 21, u64::from(window));
    dynasm!(ops
        ; .arch aarch64
        ; add x21, x19, x21
        // Entry accounting; past break-even the record asks for promotion.
        ; mov x6, xzr
        ; ldr x9, [x8, CODE_ENTRY_GENERATED_ENTRIES_OFFSET]
        ; add x9, x9, 1
        ; str x9, [x8, CODE_ENTRY_GENERATED_ENTRIES_OFFSET]
        ; str xzr, [x20, GENERATED_FEEDBACK_CLEAN_OFFSET]
        ; ldr x10, [x8, CODE_ENTRY_TIERING_BREAK_EVEN_OFFSET]
        ; cmp x9, x10
        ; b.hs =>break_even.0
        ; =>break_even.1
    );
    if shape.sloppy_receiver() {
        emit_object_receiver_test(ops, view);
    }
    if shape.lexical_this {
        emit_lexical_this(ops, view);
    }
    if shape.derived {
        emit_load_u64(ops, 2, VALUE_HOLE);
    }
    let have_depth = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
        ; movz w11, 1
        ; cbz x10, =>have_depth
        ; ldr w11, [x10, abi::NATIVE_FRAME_DEPTH_OFFSET]
        ; add w11, w11, 1
        ; =>have_depth
    );
    // A derived constructor is entered only by `[[Construct]]` and always
    // owes constructor completion.
    if shape.derived {
        dynasm!(ops ; .arch aarch64 ; orr x13, x10, 1 ; str x13, [x29, RETURN_FRAME]);
    } else {
        dynasm!(ops ; .arch aarch64 ; str x10, [x29, RETURN_FRAME]);
    }
    emit_load_u64(ops, 13, u64::from(shape.function_id));
    emit_load_u64(ops, 14, shape.header_word(shape.register_count));
    dynasm!(ops ; .arch aarch64 ; stp x13, x14, [x21]);
    emit_load_u64(ops, 14, u64::from(shape.register_count));
    dynasm!(ops
        ; .arch aarch64
        ; stp x19, x14, [x21, (NATIVE_FRAME_REGISTER_BASE_OFFSET) as i32]
        ; stp x2, x3, [x21, (NATIVE_FRAME_THIS_OFFSET) as i32]
        ; stp x1, x4, [x21, (NATIVE_FRAME_SELF_OFFSET) as i32]
        ; add x15, x29, 48
        ; stp x15, x10, [x21, (abi::NATIVE_FRAME_ACTUALS_OFFSET) as i32]
        ; orr x16, x11, 0xffff_ffff_0000_0000
        ; stp x16, xzr, [x21, (abi::NATIVE_FRAME_DEPTH_OFFSET) as i32]
        ; movn w16, 0
        ; str x16, [x21, abi::NATIVE_FRAME_CONTINUATION_OFFSET]
    );
    emit_fill_window(ops, shape);
    dynasm!(ops ; .arch aarch64 ; str x21, [x20, NATIVE_FRAME_OFFSET]);
    let construct = shape.base_constructor().then(|| {
        let (cold, back) = (ops.new_dynamic_label(), ops.new_dynamic_label());
        dynasm!(ops ; .arch aarch64 ; cmp x3, VALUE_UNDEFINED as u32 ; b.ne =>cold);
        (cold, back)
    });
    let prepare = shape.sloppy_receiver().then(|| {
        let (cold, back) = (ops.new_dynamic_label(), ops.new_dynamic_label());
        dynasm!(ops ; .arch aarch64 ; cbnz x5, =>cold ; =>back);
        (cold, back)
    });
    if let Some((_, back)) = construct {
        dynasm!(ops ; .arch aarch64 ; =>back);
    }
    let promote = (ops.new_dynamic_label(), ops.new_dynamic_label());
    dynasm!(ops ; .arch aarch64 ; cbnz x6, =>promote.0 ; =>promote.1);
    (
        start,
        CallEntryCold {
            overflow,
            break_even,
            promote,
            construct,
            prepare,
        },
    )
}

/// Seed the window `x19` from the padded span `[x29 + 48]`: formals, then
/// `undefined`. Clobbers x9–x12.
fn emit_fill_window(ops: &mut Assembler, shape: EntryShape) {
    let params = u32::from(shape.param_count.min(shape.register_count));
    let registers = u32::from(shape.register_count);
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
    if index == registers {
        return;
    }
    emit_load_u64(ops, 9, VALUE_UNDEFINED);
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
}

/// Emit the cold continuations of the call entry.
pub(super) fn emit_call_entry_cold(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    exits: ActivationExits,
    cold: CallEntryCold,
) {
    // Promotion is requested only while this generation may still tier up.
    let (check, back) = cold.break_even;
    dynasm!(ops
        ; .arch aarch64
        ; =>check
        ; ldr w10, [x8, CODE_ENTRY_TIERING_ENABLED_OFFSET]
        ; cbz w10, =>back
        ; movz x6, 1
        ; b =>back
    );
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
            ; mov x0, x20
        );
        emit_stub(ops, relocations, transitions, abi::STUB_JIT_PREPARE_ACTIVATION);
        dynasm!(ops ; .arch aarch64 ; cbnz x1, =>exits.construct ; =>created ; b =>constructed);
    }
    if let Some((prepare, prepared)) = cold.prepare {
        dynasm!(ops ; .arch aarch64 ; =>prepare ; mov x0, x20);
        emit_stub(ops, relocations, transitions, abi::STUB_JIT_PREPARE_ACTIVATION);
        dynasm!(ops ; .arch aarch64 ; cbnz x1, =>exits.construct ; b =>prepared);
    }
    // Promotion compiles against the published record; this activation
    // keeps its generation and later entries take the new one.
    let (promote, promoted) = cold.promote;
    dynasm!(ops ; .arch aarch64 ; =>promote ; mov x0, x20);
    emit_stub(ops, relocations, transitions, abi::STUB_JIT_PROMOTE_ENTERED);
    dynasm!(ops ; .arch aarch64 ; b =>promoted);
    // Nothing is published: return the overflow.
    dynasm!(ops ; .arch aarch64 ; =>cold.overflow ; mov x0, x20);
    emit_stub(ops, relocations, transitions, abi::STUB_JIT_CALL_OVERFLOW);
    emit_restore(ops);
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
fn emit_restore(ops: &mut Assembler) {
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
pub(super) fn emit_epilogue(ops: &mut Assembler, exits: ActivationExits) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr x9, [x29, RETURN_FRAME]
        ; tbnz x9, 0, =>exits.construct
        ; str x9, [x20, NATIVE_FRAME_OFFSET]
    );
    emit_restore(ops);
}

/// Emit the shared constructor completion and side-exit continuation.
pub(super) fn emit_exits(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    view: &JitCompileSnapshot,
    derived: bool,
    exits: ActivationExits,
) {
    // §10.2.2 steps 10–12; an abrupt completion passes through.
    let done = ops.new_dynamic_label();
    let primitive = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; =>exits.construct ; cbnz x1, =>done);
    emit_object_test(ops, view, 0, done, primitive);
    dynasm!(ops ; .arch aarch64 ; =>primitive);
    if derived {
        dynasm!(ops ; .arch aarch64 ; mov x1, x0 ; mov x0, x20);
        emit_stub(ops, relocations, transitions, abi::STUB_JIT_DERIVED_CONSTRUCT_RESULT);
    } else {
        dynasm!(ops ; .arch aarch64 ; ldr x0, [x21, NATIVE_FRAME_THIS_OFFSET]);
    }
    dynasm!(ops
        ; .arch aarch64
        ; =>done
        ; ldr x9, [x29, RETURN_FRAME]
        ; and x9, x9, -2i64 as u64
        ; str x9, [x20, NATIVE_FRAME_OFFSET]
    );
    emit_restore(ops);
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
        ; mov x1, x0
        ; mov x0, x20
    );
    emit_stub(ops, relocations, transitions, abi::STUB_JIT_DEOPT_CALL);
    emit_epilogue(ops, exits);
    dynasm!(ops
        ; .arch aarch64
        ; =>tier
        ; movz x1, abi::NativeResultStatus::SideExit as u32
        ; str x9, [x20, NATIVE_FRAME_OFFSET]
    );
    emit_restore(ops);
}
