//! Machine activations: the one native frame a call builds and retires.
//!
//! # Contents
//! - [`EntryShape`] — the static call semantics of the compiled function.
//! - [`emit_call_entry`] — the JavaScript call ABI entry: frame record,
//!   receiver binding and publication in the callee prologue.
//! - [`emit_tier_entry`] — the entry that continues an already published
//!   interpreter frame (function-entry tier transfer and OSR).
//! - [`emit_return`] / [`emit_plain_return`] — constructor completion,
//!   unpublication and return.
//! - [`emit_construct_completion`] — §10.2.2 steps 10–12 for a constructor.
//! - [`emit_resume_interpreter`], [`emit_reserve_window`] and
//!   [`emit_branch_if_tier_frame`] — side-exit continuation of a called
//!   record: its register window, then the interpreter on the same record.
//!
//! # Invariants
//! - Call ABI: `x0` context, `x1` callee (SELF), `x2` receiver as given (for
//!   a base `[[Construct]]` an allocated receiver or `undefined`), `x3`
//!   `new.target` (`undefined` exactly for `[[Call]]`), `x4` the actual count,
//!   the actual span at the entry `sp`, padded with `undefined` to the formal
//!   count. The completion returns in `x0`/`x1` with Success, Throw or Fatal.
//! - The activation record lives at [`MachineFrameLayout::record_offset`] in
//!   this frame. The call entry publishes it after every field is written. A
//!   tier entry runs on the interpreter's published frame and writes only a
//!   shadow record whose caller is that frame and whose flags are clear, so
//!   the common return restores the same frame and leaves constructor
//!   completion to the interpreter's owner.
//! - The published frame is this body's record exactly when it was called;
//!   exits test that identity while `sp` is still the spill base.
//! - Nothing allocates or reenters before publication. Receiver preparation
//!   and base-receiver creation run on the published record.
//! - A record without a register window gets one below the spill area when
//!   it side-exits; the interpreter then continues on the same record, and
//!   `sp` returns to the spill base before the record is unpublished.
//!
//! # See also
//! - `otter_vm::native_abi::call_trampoline` — generic classification.
//! - [`super::call`] — the caller side of the same ABI.

use super::*;
use otter_vm::native_abi::{
    NativeFrameFlags, STUB_JIT_CALL_OVERFLOW, STUB_JIT_DEOPT_CALL,
    STUB_JIT_DERIVED_CONSTRUCT_RESULT, STUB_JIT_PREPARE_ACTIVATION,
};

const CALLER: u32 = otter_vm::native_abi::NATIVE_FRAME_CALLER_OFFSET;
const ACTUALS: u32 = otter_vm::native_abi::NATIVE_FRAME_ACTUALS_OFFSET;
const DEPTH: u32 = otter_vm::native_abi::NATIVE_FRAME_DEPTH_OFFSET;
const CONTINUATION: u32 = otter_vm::native_abi::NATIVE_FRAME_CONTINUATION_OFFSET;
const FLAGS: u32 = crate::entry::NATIVE_FRAME_FLAGS_OFFSET;
const REGISTER_COUNT: u32 = otter_vm::native_abi::NATIVE_FRAME_REGISTER_COUNT_OFFSET;
const REGISTER_LEN: u32 = otter_vm::native_abi::NATIVE_FRAME_REGISTER_EXTENT_OFFSET;

pub(super) use crate::arm64::activation::EntryShape;
use crate::arm64::activation::{emit_lexical_this, emit_object_receiver_test, emit_object_test};

/// Labels shared by the returns of one Machine body.
#[derive(Debug, Clone, Copy)]
pub(super) struct ExitLabels {
    /// Returns `x0`/`x1` without constructor completion: abrupt completions,
    /// tier-entry returns and a completed construct. `sp` is the spill base.
    pub(super) plain: DynamicLabel,
    /// Constructor completion of `x0`/`x1`; present for constructors.
    pub(super) construct: Option<DynamicLabel>,
}

pub(super) fn emit_save(ops: &mut dynasmrt::aarch64::Assembler, saved: SavedFrame) {
    dynasm!(ops ; .arch aarch64 ; stp x29, x30, [sp, #-16]!);
    if saved.gpr_count == 0 {
        dynasm!(ops ; .arch aarch64 ; str x19, [sp, #-16]!);
    } else {
        dynasm!(ops ; .arch aarch64 ; stp x19, x20, [sp, #-16]!);
    }
    for pair in 0..(saved.gpr_count.saturating_sub(1) / 2) {
        let first = 21 + pair * 2;
        dynasm!(ops ; .arch aarch64 ; stp X(first), X(first + 1), [sp, #-16]!);
    }
    if !saved.gpr_count.saturating_sub(1).is_multiple_of(2) {
        let last = 19 + saved.gpr_count;
        dynasm!(ops ; .arch aarch64 ; str X(last), [sp, #-16]!);
    }
    for pair in 0..(saved.fp_count / 2) {
        let first = 8 + pair * 2;
        dynasm!(ops ; .arch aarch64 ; stp D(first), D(first + 1), [sp, #-16]!);
    }
    if !saved.fp_count.is_multiple_of(2) {
        let last = 7 + saved.fp_count;
        dynasm!(ops ; .arch aarch64 ; str D(last), [sp, #-16]!);
    }
}

fn emit_restore(ops: &mut dynasmrt::aarch64::Assembler, saved: SavedFrame) {
    if !saved.fp_count.is_multiple_of(2) {
        let last = 7 + saved.fp_count;
        dynasm!(ops ; .arch aarch64 ; ldr D(last), [sp], #16);
    }
    for pair in (0..(saved.fp_count / 2)).rev() {
        let first = 8 + pair * 2;
        dynasm!(ops ; .arch aarch64 ; ldp D(first), D(first + 1), [sp], #16);
    }
    if !saved.gpr_count.saturating_sub(1).is_multiple_of(2) {
        let last = 19 + saved.gpr_count;
        dynasm!(ops ; .arch aarch64 ; ldr X(last), [sp], #16);
    }
    for pair in (0..(saved.gpr_count.saturating_sub(1) / 2)).rev() {
        let first = 21 + pair * 2;
        dynasm!(ops ; .arch aarch64 ; ldp X(first), X(first + 1), [sp], #16);
    }
    if saved.gpr_count == 0 {
        dynasm!(ops ; .arch aarch64 ; ldr x19, [sp], #16);
    } else {
        dynasm!(ops ; .arch aarch64 ; ldp x19, x20, [sp], #16);
    }
    dynasm!(ops ; .arch aarch64 ; ldp x29, x30, [sp], #16 ; ret);
}

/// `X(register) = sp + offset`, at any offset.
fn emit_sp_address(ops: &mut dynasmrt::aarch64::Assembler, register: u8, offset: u32) {
    if offset <= 4095 {
        dynasm!(ops ; .arch aarch64 ; add XSP(register), sp, offset);
    } else {
        emit_load_u64(ops, register, u64::from(offset));
        dynasm!(ops ; .arch aarch64 ; add XSP(register), sp, X(register));
    }
}

fn emit_stub(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    stub: RuntimeStubDescriptor,
) {
    emit_load_symbolic_u64(
        ops,
        relocations,
        16,
        transitions.entry(stub),
        RelocationTarget::runtime_stub(stub),
    );
    dynasm!(ops ; .arch aarch64 ; blr x16);
}

/// Branch to `target` when the flags say "not equal", at any distance.
fn emit_branch_ne(ops: &mut dynasmrt::aarch64::Assembler, target: DynamicLabel, far: bool) {
    if far {
        let skip = ops.new_dynamic_label();
        dynasm!(ops ; .arch aarch64 ; b.eq =>skip ; b =>target ; =>skip);
    } else {
        dynasm!(ops ; .arch aarch64 ; b.ne =>target);
    }
}

/// Cold continuations of one call entry, emitted after the body.
#[derive(Debug, Clone, Copy)]
pub(super) struct CallEntryCold {
    overflow: DynamicLabel,
    construct: Option<(DynamicLabel, DynamicLabel)>,
    prepare: Option<(DynamicLabel, DynamicLabel)>,
}

/// Emit the call-ABI entry. It falls through into the body with the formal
/// base in `x17`; its cold continuations are emitted by
/// [`emit_call_entry_cold`]. Returns the entry offset.
pub(super) fn emit_call_entry(
    ops: &mut dynasmrt::aarch64::Assembler,
    view: &JitCompileSnapshot,
    frame: MachineFrameLayout,
    saved: SavedFrame,
    shape: EntryShape,
) -> (AssemblyOffset, CallEntryCold) {
    let start = ops.offset();
    let overflow = ops.new_dynamic_label();
    let record = frame.record_offset();
    emit_save(ops, saved);
    emit_reserve_spill_area(ops, frame.spill_area_bytes());
    dynasm!(ops
        ; .arch aarch64
        ; mov x19, x0
        ; ldr x9, [x19, crate::entry::NATIVE_STACK_LIMIT_OFFSET]
        ; mov x10, sp
        ; cmp x10, x9
        ; b.lo =>overflow
    );
    // Receiver binding. A conversion the entry cannot decide inline runs
    // on the published record, signalled by `x5`.
    let base_constructor = shape.base_constructor();
    let sloppy_receiver = shape.sloppy_receiver();
    if sloppy_receiver {
        emit_object_receiver_test(ops, view);
    }
    if shape.lexical_this {
        emit_lexical_this(ops, view);
    }
    if shape.derived {
        emit_load_u64(ops, 2, VALUE_HOLE);
    }
    // Depth from the caller's record; the outermost generated callee is one.
    let have_depth = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; ldr x10, [x19, NATIVE_FRAME_OFFSET]
        ; movz w11, 1
        ; cbz x10, =>have_depth
        ; ldr w11, [x10, DEPTH]
        ; add w11, w11, 1
        ; =>have_depth
    );
    emit_sp_address(ops, 12, record);
    emit_load_u64(ops, 13, u64::from(shape.function_id));
    emit_load_u64(ops, 14, shape.header_word(frame.window_slots()));
    dynasm!(ops ; .arch aarch64 ; stp x13, x14, [x12]);
    if frame.window_slots() == 0 {
        dynasm!(ops ; .arch aarch64 ; stp xzr, xzr, [x12, (NATIVE_FRAME_REGISTER_BASE_OFFSET) as i32]);
    } else {
        emit_sp_address(ops, 13, frame.window_offset());
        emit_load_u64(ops, 14, u64::from(frame.window_slots()));
        dynasm!(ops ; .arch aarch64 ; stp x13, x14, [x12, (NATIVE_FRAME_REGISTER_BASE_OFFSET) as i32]);
    }
    emit_sp_address(ops, 15, frame.frame_bytes());
    dynasm!(ops
        ; .arch aarch64
        ; stp x2, x3, [x12, (NATIVE_FRAME_THIS_OFFSET) as i32]
        ; stp x1, x4, [x12, (NATIVE_FRAME_SELF_OFFSET) as i32]
        ; stp x15, x10, [x12, (ACTUALS) as i32]
        ; orr x16, x11, 0xffff_ffff_0000_0000
    );
    match frame.root_offset(0) {
        Ok(roots) => {
            emit_sp_address(ops, 17, roots);
            dynasm!(ops ; .arch aarch64 ; stp x16, x17, [x12, (DEPTH) as i32]);
        }
        Err(_) => dynasm!(ops ; .arch aarch64 ; stp x16, xzr, [x12, (DEPTH) as i32]),
    }
    dynasm!(ops
        ; .arch aarch64
        ; movn w16, 0
        ; str x16, [x12, CONTINUATION]
    );
    if frame.window_slots() != 0 {
        emit_fill_window(ops, 13, 15, shape.param_count, frame.window_slots());
    }
    dynasm!(ops
        ; .arch aarch64
        ; str x12, [x19, NATIVE_FRAME_OFFSET]
        ; mov x17, x15
    );
    let construct = base_constructor.then(|| {
        let (cold, back) = (ops.new_dynamic_label(), ops.new_dynamic_label());
        dynasm!(ops ; .arch aarch64 ; cmp x3, VALUE_UNDEFINED as u32 ; b.ne =>cold);
        (cold, back)
    });
    let prepare = sloppy_receiver.then(|| {
        let (cold, back) = (ops.new_dynamic_label(), ops.new_dynamic_label());
        dynasm!(ops ; .arch aarch64 ; cbnz x5, =>cold ; =>back);
        (cold, back)
    });
    if let Some((_, back)) = construct {
        dynasm!(ops ; .arch aarch64 ; =>back);
    }
    (
        start,
        CallEntryCold {
            overflow,
            construct,
            prepare,
        },
    )
}

/// Emit the cold continuations of one call entry.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_call_entry_cold(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    frame: MachineFrameLayout,
    saved: SavedFrame,
    exits: ExitLabels,
    cold: CallEntryCold,
) {
    // [[Construct]] of a base constructor: mark the record and create the
    // receiver unless the caller allocated it. Receiver conversion does not
    // apply to the created receiver.
    if let Some((construct, constructed)) = cold.construct {
        dynasm!(ops ; .arch aarch64 ; =>construct ; ldr x12, [x19, NATIVE_FRAME_OFFSET]);
        dynasm!(ops
            ; .arch aarch64
            ; ldrb w9, [x12, FLAGS]
            ; orr w9, w9, u32::from(NativeFrameFlags::CONSTRUCT)
            ; strb w9, [x12, FLAGS]
            ; cmp x2, VALUE_UNDEFINED as u32
            ; b.ne =>constructed
            ; mov x0, x19
        );
        emit_stub(ops, relocations, transitions, STUB_JIT_PREPARE_ACTIVATION);
        dynasm!(ops ; .arch aarch64 ; cbnz x1, =>exits.plain);
        emit_sp_address(ops, 17, frame.frame_bytes());
        dynasm!(ops ; .arch aarch64 ; b =>constructed);
    }
    if let Some((prepare, prepared)) = cold.prepare {
        dynasm!(ops ; .arch aarch64 ; =>prepare ; mov x0, x19);
        emit_stub(ops, relocations, transitions, STUB_JIT_PREPARE_ACTIVATION);
        dynasm!(ops ; .arch aarch64 ; cbnz x1, =>exits.plain);
        emit_sp_address(ops, 17, frame.frame_bytes());
        dynasm!(ops ; .arch aarch64 ; b =>prepared);
    }
    // Nothing is published: release the frame and return the overflow.
    dynasm!(ops ; .arch aarch64 ; =>cold.overflow ; mov x0, x19);
    emit_stub(ops, relocations, transitions, STUB_JIT_CALL_OVERFLOW);
    emit_release_spill_area(ops, frame.spill_area_bytes());
    emit_restore(ops, saved);
}

/// Fill the window `X(window)` with the formals of the padded span
/// `X(span)` and `undefined` above them. Clobbers x9 and x16.
fn emit_fill_window(
    ops: &mut dynasmrt::aarch64::Assembler,
    window: u8,
    span: u8,
    param_count: u16,
    slots: u16,
) {
    let params = u32::from(param_count.min(slots));
    for index in 0..params {
        emit_load_u64(ops, 16, u64::from(index * 8));
        dynasm!(ops
            ; .arch aarch64
            ; ldr x9, [X(span), x16]
            ; str x9, [X(window), x16]
        );
    }
    emit_load_u64(ops, 9, VALUE_UNDEFINED);
    for index in params..u32::from(slots) {
        emit_load_u64(ops, 16, u64::from(index * 8));
        dynasm!(ops ; .arch aarch64 ; str x9, [X(window), x16]);
    }
}

/// Emit the entry over an already published interpreter frame, branching to
/// `body` with that frame's register window in `x17`. Returns its offset.
pub(super) fn emit_tier_entry(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    saved: SavedFrame,
    body: DynamicLabel,
) -> AssemblyOffset {
    let start = ops.offset();
    emit_save(ops, saved);
    emit_reserve_spill_area(ops, frame.spill_area_bytes());
    dynasm!(ops
        ; .arch aarch64
        ; mov x19, x0
        ; ldr x16, [x19, NATIVE_FRAME_OFFSET]
    );
    emit_sp_address(ops, 12, frame.record_offset());
    dynasm!(ops
        ; .arch aarch64
        ; str x16, [x12, CALLER]
        ; strb wzr, [x12, FLAGS]
    );
    if let Ok(roots) = frame.root_offset(0) {
        emit_sp_address(ops, 9, roots);
        dynasm!(ops ; .arch aarch64 ; str x9, [x16, NATIVE_FRAME_MACHINE_ROOTS_OFFSET]);
    }
    dynasm!(ops
        ; .arch aarch64
        ; ldr x17, [x16, NATIVE_FRAME_REGISTER_BASE_OFFSET]
        ; b =>body
    );
    start
}

/// Return `x0`/`x1` from the spill base: constructor completion when the
/// record asks for it, then unpublish the record and release the frame.
pub(super) fn emit_return(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    saved: SavedFrame,
    exits: ExitLabels,
    far: bool,
) {
    if let Some(construct) = exits.construct {
        let flags = frame.record_offset() + FLAGS;
        if flags <= 4095 {
            dynasm!(ops ; .arch aarch64 ; ldrb w9, [sp, flags]);
        } else {
            emit_sp_address(ops, 9, flags);
            dynasm!(ops ; .arch aarch64 ; ldrb w9, [x9]);
        }
        dynasm!(ops ; .arch aarch64 ; tst w9, u32::from(NativeFrameFlags::CONSTRUCT));
        emit_branch_ne(ops, construct, far);
    }
    emit_plain_return(ops, frame, saved);
}

/// Unpublish and return `x0`/`x1` as they are, from the spill base.
pub(super) fn emit_plain_return(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    saved: SavedFrame,
) {
    emit_frame_ldr_x(ops, 9, frame.record_offset() + CALLER);
    dynasm!(ops ; .arch aarch64 ; str x9, [x19, NATIVE_FRAME_OFFSET]);
    emit_release_spill_area(ops, frame.spill_area_bytes());
    emit_restore(ops, saved);
}

/// Constructor completion of `x0`/`x1` (§10.2.2 steps 10–12), then the
/// plain return. An abrupt completion passes through.
pub(super) fn emit_construct_completion(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    view: &JitCompileSnapshot,
    frame: MachineFrameLayout,
    shape: EntryShape,
    exits: ExitLabels,
) {
    let Some(construct) = exits.construct else {
        return;
    };
    let primitive = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; =>construct ; cbnz x1, =>exits.plain);
    emit_object_test(ops, view, 0, exits.plain, primitive);
    dynasm!(ops ; .arch aarch64 ; =>primitive);
    if shape.derived {
        dynasm!(ops ; .arch aarch64 ; mov x1, x0 ; mov x0, x19);
        emit_stub(ops, relocations, transitions, STUB_JIT_DERIVED_CONSTRUCT_RESULT);
    } else {
        emit_frame_ldr_x(ops, 0, frame.record_offset() + NATIVE_FRAME_THIS_OFFSET);
    }
    dynasm!(ops ; .arch aarch64 ; b =>exits.plain);
}

/// Branch to `tier` unless the published frame is this body's own record.
/// Valid only while `sp` is the spill base; clobbers x16 and x30.
pub(super) fn emit_branch_if_tier_frame(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    tier: DynamicLabel,
) {
    dynasm!(ops ; .arch aarch64 ; ldr x16, [x19, NATIVE_FRAME_OFFSET]);
    emit_sp_address(ops, 30, frame.record_offset());
    dynasm!(ops ; .arch aarch64 ; cmp x16, x30 ; b.ne =>tier);
}

/// Give the published called record a register window below `sp` when it
/// has none, leaving the reserved bytes in `x15` (zero otherwise). Every
/// slot reads `undefined` until the interpreter state is written into it. A
/// full stack returns the overflow from the record. Clobbers only x15, x16
/// and x30.
pub(super) fn emit_reserve_window(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    frame: MachineFrameLayout,
    saved: SavedFrame,
    register_count: u16,
) {
    let ready = ops.new_dynamic_label();
    let fits = ops.new_dynamic_label();
    let fill = ops.new_dynamic_label();
    let bytes = (u32::from(register_count) * 8).next_multiple_of(16);
    dynasm!(ops
        ; .arch aarch64
        ; mov x15, xzr
        ; ldr x16, [x19, NATIVE_FRAME_OFFSET]
        ; ldr x30, [x16, NATIVE_FRAME_REGISTER_BASE_OFFSET]
        ; cbnz x30, =>ready
    );
    if bytes == 0 {
        dynasm!(ops ; .arch aarch64 ; =>ready);
        return;
    }
    emit_load_u64(ops, 15, u64::from(bytes));
    dynasm!(ops
        ; .arch aarch64
        ; mov x30, sp
        ; sub x30, x30, x15
        ; ldr x16, [x19, crate::entry::NATIVE_STACK_LIMIT_OFFSET]
        ; cmp x30, x16
        ; b.hs =>fits
        ; mov x0, x19
    );
    // No room to continue in the interpreter: the record returns the
    // overflow instead.
    emit_stub(ops, relocations, transitions, STUB_JIT_CALL_OVERFLOW);
    emit_plain_return(ops, frame, saved);
    dynasm!(ops
        ; .arch aarch64
        ; =>fits
        ; mov sp, x30
    );
    // Every slot reads `undefined` before the record counts it.
    emit_load_u64(ops, 30, VALUE_UNDEFINED);
    emit_load_u64(ops, 16, u64::from(register_count));
    dynasm!(ops
        ; .arch aarch64
        ; =>fill
        ; subs x16, x16, 1
        ; str x30, [sp, x16, lsl #3]
        ; b.ne =>fill
        ; mov x30, sp
        ; ldr x16, [x19, NATIVE_FRAME_OFFSET]
        ; str x30, [x16, NATIVE_FRAME_REGISTER_BASE_OFFSET]
    );
    emit_load_u64(ops, 30, u64::from(register_count));
    dynasm!(ops
        ; .arch aarch64
        ; str w30, [x16, REGISTER_LEN]
        ; strh w30, [x16, REGISTER_COUNT]
        ; =>ready
    );
}

/// Seed the published record's window with its formals from the padded
/// actual span: the interpreter state of a pc-zero exit. Clobbers x9–x12
/// and x16.
pub(super) fn emit_entry_window_state(ops: &mut dynasmrt::aarch64::Assembler, shape: EntryShape) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr x9, [x19, NATIVE_FRAME_OFFSET]
        ; ldr x10, [x9, ACTUALS]
        ; ldr x11, [x9, NATIVE_FRAME_REGISTER_BASE_OFFSET]
    );
    for index in 0..u32::from(shape.param_count.min(shape.register_count)) {
        emit_load_u64(ops, 12, u64::from(index * 8));
        dynasm!(ops ; .arch aarch64 ; ldr x16, [x10, x12] ; str x16, [x11, x12]);
    }
}

/// Continue the called record in the interpreter after a side exit whose
/// encoded exit is in `x0`, then return its completion through constructor
/// completion. `sp` may sit below the spill base on a reserved window.
pub(super) fn emit_resume_interpreter(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    frame: MachineFrameLayout,
    saved: SavedFrame,
    exits: ExitLabels,
) {
    dynasm!(ops ; .arch aarch64 ; mov x1, x0 ; mov x0, x19);
    emit_stub(ops, relocations, transitions, STUB_JIT_DEOPT_CALL);
    emit_return_from_record(ops, frame, saved, exits);
}

/// Return `x0`/`x1` with `sp` recovered from the published called record,
/// releasing a window reserved below the spill base.
pub(super) fn emit_return_from_record(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    saved: SavedFrame,
    exits: ExitLabels,
) {
    let record = frame.record_offset();
    dynasm!(ops ; .arch aarch64 ; ldr x9, [x19, NATIVE_FRAME_OFFSET]);
    if record <= 4095 {
        dynasm!(ops ; .arch aarch64 ; sub x9, x9, record);
    } else {
        emit_load_u64(ops, 16, u64::from(record));
        dynasm!(ops ; .arch aarch64 ; sub x9, x9, x16);
    }
    dynasm!(ops ; .arch aarch64 ; mov sp, x9);
    emit_return(ops, frame, saved, exits, true);
}
