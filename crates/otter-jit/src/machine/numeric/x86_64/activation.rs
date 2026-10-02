//! Machine activations on x86-64: the one native frame a call builds and
//! retires.
//!
//! # Contents
//! - [`emit_call_entry`] / [`emit_call_entry_cold`] — the JavaScript call
//!   ABI entry: frame record, receiver binding and publication.
//! - [`emit_tier_entry`] — the entry continuing an already published
//!   interpreter frame (function-entry tier transfer and OSR).
//! - [`emit_return`] / [`emit_plain_return`] / [`emit_construct_completion`]
//!   — constructor completion, unpublication and return.
//! - [`emit_bail`] / [`emit_deopt`] / [`emit_pair_side_exit`] — side exits:
//!   a called record gets its register window and continues in the
//!   interpreter in place; a tier-entered frame hands the exit back.
//!
//! # Invariants
//! - Same contract as the AArch64 Machine activation. The record lives at
//!   [`MachineFrameLayout::record_offset`]; a tier entry writes only a shadow
//!   whose caller is the published frame, whose flags are clear and whose
//!   actual pointer names the interpreter's register window, so `EntryValue`
//!   reads formals through the record in both modes.
//! - `r15` holds the context and `r11` is the only scratch the body lends to
//!   exits that run before the deopt dump.
//!
//! # See also
//! - [`crate::x86_64::activation`] — pieces shared with the template tier.
//! - [`super::call`] — the caller side of the same ABI.

use super::*;
use crate::x86_64::activation::{
    EntryShape, emit_lexical_this, emit_object_receiver_test, emit_object_test,
};
use otter_vm::native_abi::{
    NativeFrameFlags, STUB_JIT_CALL_OVERFLOW, STUB_JIT_DEOPT_CALL,
    STUB_JIT_DERIVED_CONSTRUCT_RESULT, STUB_JIT_PREPARE_ACTIVATION,
};

const CALLER: i32 = otter_vm::native_abi::NATIVE_FRAME_CALLER_OFFSET as i32;
const ACTUALS: i32 = otter_vm::native_abi::NATIVE_FRAME_ACTUALS_OFFSET as i32;
const DEPTH: i32 = otter_vm::native_abi::NATIVE_FRAME_DEPTH_OFFSET as i32;
const CONTINUATION: i32 = otter_vm::native_abi::NATIVE_FRAME_CONTINUATION_OFFSET as i32;
const FLAGS: i32 = NATIVE_FRAME_FLAGS_OFFSET as i32;
const REGISTERS: i32 = NATIVE_FRAME_REGISTER_BASE_OFFSET as i32;
const REGISTER_LEN: i32 = otter_vm::native_abi::NATIVE_FRAME_REGISTER_EXTENT_OFFSET as i32;
const REGISTER_COUNT: i32 = otter_vm::native_abi::NATIVE_FRAME_REGISTER_COUNT_OFFSET as i32;

/// Labels shared by the returns of one Machine body.
#[derive(Debug, Clone, Copy)]
pub(super) struct ExitLabels {
    /// Returns `rax`/`rdx` without constructor completion; `rsp` is the
    /// spill base.
    pub(super) plain: DynamicLabel,
    /// Constructor completion of `rax`/`rdx`; present for constructors.
    pub(super) construct: Option<DynamicLabel>,
}

/// Cold continuations of one call entry.
#[derive(Debug, Clone, Copy)]
pub(super) struct CallEntryCold {
    overflow: DynamicLabel,
    construct: Option<(DynamicLabel, DynamicLabel)>,
    prepare: Option<(DynamicLabel, DynamicLabel)>,
}

fn record(frame: MachineFrameLayout) -> i32 {
    frame.record_offset() as i32
}

fn emit_save(ops: &mut Assembler, frame: MachineFrameLayout, saved: SavedFrame) {
    dynasm!(ops ; .arch x64 ; push rbp ; push r15);
    if saved.rbx {
        dynasm!(ops ; .arch x64 ; push rbx);
    }
    if let Some(highest) = saved.highest {
        for register in 12..=highest {
            dynasm!(ops ; .arch x64 ; push Rq(register));
        }
    }
    let reserved = frame.fixed_bytes() - saved.actual_bytes() + frame.spill_area_bytes();
    if reserved != 0 {
        dynasm!(ops ; .arch x64 ; sub rsp, reserved as i32);
    }
}

fn emit_restore(ops: &mut Assembler, frame: MachineFrameLayout, saved: SavedFrame) {
    let reserved = frame.fixed_bytes() - saved.actual_bytes() + frame.spill_area_bytes();
    if reserved != 0 {
        dynasm!(ops ; .arch x64 ; add rsp, reserved as i32);
    }
    if let Some(highest) = saved.highest {
        for register in (12..=highest).rev() {
            dynasm!(ops ; .arch x64 ; pop Rq(register));
        }
    }
    if saved.rbx {
        dynasm!(ops ; .arch x64 ; pop rbx);
    }
    dynasm!(ops ; .arch x64 ; pop r15 ; pop rbp ; ret);
}

fn emit_stub(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    stub: RuntimeStubDescriptor,
) {
    runtime(ops, relocations, transitions.entry(stub), stub);
    dynasm!(ops ; .arch x64 ; call r11);
}

/// Emit the call-ABI entry. It falls through into the body; its cold
/// continuations are emitted by [`emit_call_entry_cold`]. Returns the entry
/// offset.
pub(super) fn emit_call_entry(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    frame: MachineFrameLayout,
    saved: SavedFrame,
    shape: EntryShape,
) -> (AssemblyOffset, CallEntryCold) {
    let start = ops.offset();
    let overflow = ops.new_dynamic_label();
    let rec = record(frame);
    emit_save(ops, frame, saved);
    dynasm!(ops
        ; .arch x64
        ; mov r15, rdi
        ; cmp rsp, [r15 + NATIVE_STACK_LIMIT_OFFSET as i32]
        ; jb =>overflow
    );
    // Receiver binding. A conversion the entry cannot decide inline runs
    // on the published record, signalled by `r9`.
    if shape.sloppy_receiver() {
        emit_object_receiver_test(ops, view);
    }
    if shape.lexical_this {
        emit_lexical_this(ops, view);
    }
    if shape.derived {
        dynasm!(ops ; .arch x64 ; mov edx, otter_vm::value::tag::VALUE_HOLE as i32);
    }
    // Depth from the caller's record; the outermost generated callee is one.
    let have_depth = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov eax, 1
        ; test r10, r10
        ; jz =>have_depth
        ; mov eax, [r10 + DEPTH]
        ; inc eax
        ; =>have_depth
        ; lea r11, [rsp + rec]
        ; mov edi, shape.function_id as i32
        ; mov [r11], rdi
        ; mov rdi, QWORD shape.header_word(frame.window_slots()) as i64
        ; mov [r11 + 8], rdi
    );
    if frame.window_slots() == 0 {
        dynasm!(ops
            ; .arch x64
            ; mov QWORD [r11 + REGISTERS], 0
            ; mov QWORD [r11 + REGISTERS + 8], 0
        );
    } else {
        dynasm!(ops
            ; .arch x64
            ; lea rdi, [rsp + frame.window_offset() as i32]
            ; mov [r11 + REGISTERS], rdi
            ; mov QWORD [r11 + REGISTERS + 8], i32::from(frame.window_slots())
        );
    }
    dynasm!(ops
        ; .arch x64
        ; mov [r11 + NATIVE_FRAME_THIS_OFFSET as i32], rdx
        ; mov [r11 + NATIVE_FRAME_NEW_TARGET_OFFSET as i32], rcx
        ; mov [r11 + NATIVE_FRAME_SELF_OFFSET as i32], rsi
        ; mov [r11 + NATIVE_FRAME_SELF_OFFSET as i32 + 8], r8
        ; lea rdi, [rsp + frame.frame_bytes() as i32 + 8]
        ; mov [r11 + ACTUALS], rdi
        ; mov [r11 + CALLER], r10
        ; mov [r11 + DEPTH], eax
        ; mov DWORD [r11 + DEPTH + 4], -1
    );
    match frame.root_offset(0) {
        Ok(roots) => dynasm!(ops
            ; .arch x64
            ; lea rax, [rsp + roots as i32]
            ; mov [r11 + NATIVE_FRAME_MACHINE_ROOTS_OFFSET as i32], rax
        ),
        Err(_) => {
            dynasm!(ops ; .arch x64 ; mov QWORD [r11 + NATIVE_FRAME_MACHINE_ROOTS_OFFSET as i32], 0)
        }
    }
    dynasm!(ops
        ; .arch x64
        ; mov DWORD [r11 + CONTINUATION], -1
        ; mov DWORD [r11 + CONTINUATION + 4], 0
    );
    if frame.window_slots() != 0 {
        // Formals from the padded span, `undefined` above them.
        let window = frame.window_offset() as i32;
        let params = i32::from(shape.param_count.min(frame.window_slots()));
        for index in 0..i32::from(frame.window_slots()) {
            if index < params {
                dynasm!(ops
                    ; .arch x64
                    ; mov rax, [rdi + index * 8]
                    ; mov [rsp + window + index * 8], rax
                );
            } else {
                dynasm!(ops ; .arch x64 ; mov QWORD [rsp + window + index * 8], VALUE_UNDEFINED as i32);
            }
        }
    }
    dynasm!(ops ; .arch x64 ; mov [r15 + NATIVE_FRAME_OFFSET as i32], r11);
    let construct = shape.base_constructor().then(|| {
        let (cold, back) = (ops.new_dynamic_label(), ops.new_dynamic_label());
        dynasm!(ops ; .arch x64 ; cmp rcx, VALUE_UNDEFINED as i32 ; jne =>cold);
        (cold, back)
    });
    let prepare = shape.sloppy_receiver().then(|| {
        let (cold, back) = (ops.new_dynamic_label(), ops.new_dynamic_label());
        dynasm!(ops ; .arch x64 ; test r9, r9 ; jnz =>cold ; =>back);
        (cold, back)
    });
    if let Some((_, back)) = construct {
        dynasm!(ops ; .arch x64 ; =>back);
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
pub(super) fn emit_call_entry_cold(
    ops: &mut Assembler,
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
        dynasm!(ops
            ; .arch x64
            ; =>construct
            ; mov r11, [r15 + NATIVE_FRAME_OFFSET as i32]
            ; or BYTE [r11 + FLAGS], NativeFrameFlags::CONSTRUCT as i8
            ; cmp rdx, VALUE_UNDEFINED as i32
            ; jne =>constructed
            ; mov rdi, r15
        );
        emit_stub(ops, relocations, transitions, STUB_JIT_PREPARE_ACTIVATION);
        dynasm!(ops ; .arch x64 ; test rdx, rdx ; jnz =>exits.plain ; jmp =>constructed);
    }
    if let Some((prepare, prepared)) = cold.prepare {
        dynasm!(ops ; .arch x64 ; =>prepare ; mov rdi, r15);
        emit_stub(ops, relocations, transitions, STUB_JIT_PREPARE_ACTIVATION);
        dynasm!(ops ; .arch x64 ; test rdx, rdx ; jnz =>exits.plain ; jmp =>prepared);
    }
    // Nothing is published: release the frame and return the overflow.
    dynasm!(ops ; .arch x64 ; =>cold.overflow ; mov rdi, r15);
    emit_stub(ops, relocations, transitions, STUB_JIT_CALL_OVERFLOW);
    emit_restore(ops, frame, saved);
}

/// Emit the entry over an already published interpreter frame. Returns its
/// offset; the caller continues with the body.
pub(super) fn emit_tier_entry(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    saved: SavedFrame,
    body: DynamicLabel,
) -> AssemblyOffset {
    let start = ops.offset();
    let rec = record(frame);
    emit_save(ops, frame, saved);
    dynasm!(ops
        ; .arch x64
        ; mov r15, rdi
        ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; lea r11, [rsp + rec]
        ; mov [r11 + CALLER], r10
        ; mov BYTE [r11 + FLAGS], 0
        ; mov rax, [r10 + REGISTERS]
        ; mov [r11 + ACTUALS], rax
    );
    if let Ok(roots) = frame.root_offset(0) {
        dynasm!(ops
            ; .arch x64
            ; lea rax, [rsp + roots as i32]
            ; mov [r10 + NATIVE_FRAME_MACHINE_ROOTS_OFFSET as i32], rax
        );
    }
    dynasm!(ops ; .arch x64 ; jmp =>body);
    start
}

/// Return `rax`/`rdx` from the spill base: constructor completion when the
/// record asks for it, then unpublish the record and release the frame.
pub(super) fn emit_return(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    saved: SavedFrame,
    exits: ExitLabels,
) {
    if let Some(construct) = exits.construct {
        dynasm!(ops
            ; .arch x64
            ; test BYTE [rsp + record(frame) + FLAGS], NativeFrameFlags::CONSTRUCT as i8
            ; jnz =>construct
        );
    }
    emit_plain_return(ops, frame, saved);
}

/// Unpublish and return `rax`/`rdx` as they are, from the spill base.
pub(super) fn emit_plain_return(ops: &mut Assembler, frame: MachineFrameLayout, saved: SavedFrame) {
    dynasm!(ops
        ; .arch x64
        ; mov r11, [rsp + record(frame) + CALLER]
        ; mov [r15 + NATIVE_FRAME_OFFSET as i32], r11
    );
    emit_restore(ops, frame, saved);
}

/// Constructor completion of `rax`/`rdx` (§10.2.2 steps 10–12), then the
/// plain return. An abrupt completion passes through.
pub(super) fn emit_construct_completion(
    ops: &mut Assembler,
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
    dynasm!(ops ; .arch x64 ; =>construct ; test rdx, rdx ; jnz =>exits.plain);
    emit_object_test(ops, view, 0, exits.plain, primitive);
    dynasm!(ops ; .arch x64 ; =>primitive);
    if shape.derived {
        dynasm!(ops ; .arch x64 ; mov rsi, rax ; mov rdi, r15);
        emit_stub(
            ops,
            relocations,
            transitions,
            STUB_JIT_DERIVED_CONSTRUCT_RESULT,
        );
    } else {
        dynasm!(ops ; .arch x64 ; mov rax, [rsp + record(frame) + NATIVE_FRAME_THIS_OFFSET as i32]);
    }
    dynasm!(ops ; .arch x64 ; jmp =>exits.plain);
}

/// Give the published called record `r10` a register window below `rsp`
/// when it has none: every slot `undefined`, counted. A full stack returns
/// the overflow from the record with `rsp` at `reset`. Clobbers rax and rcx.
fn emit_reserve_window(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    frame: MachineFrameLayout,
    saved: SavedFrame,
    register_count: u16,
    reset: i32,
) {
    let ready = ops.new_dynamic_label();
    let fits = ops.new_dynamic_label();
    let fill = ops.new_dynamic_label();
    let bytes = (u32::from(register_count) * 8).next_multiple_of(16) as i32;
    dynasm!(ops
        ; .arch x64
        ; cmp QWORD [r10 + REGISTERS], 0
        ; jne =>ready
    );
    if bytes == 0 {
        dynasm!(ops ; .arch x64 ; =>ready);
        return;
    }
    dynasm!(ops
        ; .arch x64
        ; lea rax, [rsp - bytes]
        ; cmp rax, [r15 + NATIVE_STACK_LIMIT_OFFSET as i32]
        ; jae =>fits
        ; add rsp, reset
        ; mov rdi, r15
    );
    emit_stub(ops, relocations, transitions, STUB_JIT_CALL_OVERFLOW);
    emit_plain_return(ops, frame, saved);
    dynasm!(ops
        ; .arch x64
        ; =>fits
        ; mov rsp, rax
        ; mov ecx, i32::from(register_count)
        ; =>fill
        ; mov QWORD [rsp + rcx * 8 - 8], VALUE_UNDEFINED as i32
        ; dec ecx
        ; jnz =>fill
        ; mov [r10 + REGISTERS], rsp
        ; mov DWORD [r10 + REGISTER_LEN], i32::from(register_count)
        ; mov WORD [r10 + REGISTER_COUNT], register_count as i16
        ; =>ready
    );
}

/// Continue the called record in the interpreter after a side exit whose
/// encoded exit is in `rax`, then return its completion through constructor
/// completion with `rsp` recovered from the record.
fn emit_resume_interpreter(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    frame: MachineFrameLayout,
    saved: SavedFrame,
    exits: ExitLabels,
) {
    dynasm!(ops ; .arch x64 ; mov rsi, rax ; mov rdi, r15);
    emit_stub(ops, relocations, transitions, STUB_JIT_DEOPT_CALL);
    emit_return_from_record(ops, frame, saved, exits);
}

/// Return `rax`/`rdx` with `rsp` recovered from the published called record.
fn emit_return_from_record(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    saved: SavedFrame,
    exits: ExitLabels,
) {
    dynasm!(ops
        ; .arch x64
        ; mov r11, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; lea rsp, [r11 - record(frame)]
    );
    emit_return(ops, frame, saved, exits);
}

/// The exit of a failed entry guard: the interpreter resumes at PC zero
/// with the formals.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_bail(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    frame: MachineFrameLayout,
    saved: SavedFrame,
    shape: EntryShape,
    exits: ExitLabels,
    bail: DynamicLabel,
) {
    let tier = ops.new_dynamic_label();
    let bits = SideExit::new(0, ExitReason::TypeMismatch, ExitAction::Recompile).to_bits();
    dynasm!(ops
        ; .arch x64
        ; =>bail
        ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov DWORD [r10 + NATIVE_FRAME_PC_OFFSET as i32], 0
        ; lea r11, [rsp + record(frame)]
        ; cmp r10, r11
        ; jne =>tier
    );
    emit_reserve_window(
        ops,
        relocations,
        transitions,
        frame,
        saved,
        shape.register_count,
        0,
    );
    dynasm!(ops
        ; .arch x64
        ; mov rax, [r10 + ACTUALS]
        ; mov rcx, [r10 + REGISTERS]
    );
    for index in 0..i32::from(shape.param_count.min(shape.register_count)) {
        dynasm!(ops
            ; .arch x64
            ; mov r11, [rax + index * 8]
            ; mov [rcx + index * 8], r11
        );
    }
    load64(ops, 0, bits);
    emit_resume_interpreter(ops, relocations, transitions, frame, saved, exits);
    dynasm!(ops ; .arch x64 ; =>tier);
    load64(ops, 0, bits);
    dynasm!(ops ; .arch x64 ; mov edx, NativeResultStatus::SideExit as i32);
    emit_plain_return(ops, frame, saved);
}

/// A finished error that selected a local handler: a tier-entered frame
/// continues in its interpreter; a called record resumes in place.
pub(super) fn emit_pair_side_exit(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    frame: MachineFrameLayout,
    saved: SavedFrame,
    exits: ExitLabels,
    side_exit: DynamicLabel,
) {
    let tier = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; =>side_exit
        ; lea r11, [rsp + record(frame)]
        ; cmp r11, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; jne =>tier
    );
    emit_resume_interpreter(ops, relocations, transitions, frame, saved, exits);
    dynasm!(ops ; .arch x64 ; =>tier);
    emit_plain_return(ops, frame, saved);
}

/// The shared deopt exit with the exit index in `r11d`: dump every
/// register, write the interpreter state back into the frame's window and
/// continue.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_deopt(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    frame: MachineFrameLayout,
    saved: SavedFrame,
    shape: EntryShape,
    exits: ExitLabels,
    runtime_data: &DeoptRuntime,
    entry: u64,
    shared: DynamicLabel,
) {
    dynasm!(ops ; .arch x64 ; =>shared ; sub rsp, DEOPT_DUMP_BYTES);
    for register in 0_u8..16 {
        let offset = i32::from(register) * 8;
        dynasm!(ops ; .arch x64 ; mov [rsp + offset], Rq(register));
    }
    for register in 0_u8..16 {
        let offset = DEOPT_BANK_BYTES + i32::from(register) * 8;
        dynasm!(ops ; .arch x64 ; movsd [rsp + offset], Rx(register));
    }
    let tier = ops.new_dynamic_label();
    let completed = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; lea rax, [rsp + DEOPT_DUMP_BYTES + record(frame)]
        ; cmp r10, rax
        ; jne =>tier
    );
    // A called record without a window gets one below the dump.
    emit_reserve_window(
        ops,
        relocations,
        transitions,
        frame,
        saved,
        shape.register_count,
        DEOPT_DUMP_BYTES,
    );
    dynasm!(ops
        ; .arch x64
        ; mov rdi, r15
        ; lea rcx, [r10 - record(frame) - DEOPT_DUMP_BYTES]
        ; mov esi, [rcx + 11 * 8]
        ; lea r8, [r10 - record(frame)]
        ; mov r9, [r10 + REGISTERS]
    );
    symbolic(
        ops,
        relocations,
        2,
        std::ptr::from_ref::<DeoptRuntime>(runtime_data) as u64,
        RelocationTarget::DeoptRuntimeData,
    );
    runtime(ops, relocations, entry, STUB_JIT_DEOPT_WRITEBACK);
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; cmp edx, NativeResultStatus::SideExit as i32
        ; jne =>completed
    );
    emit_resume_interpreter(ops, relocations, transitions, frame, saved, exits);
    dynasm!(ops ; .arch x64 ; =>completed);
    emit_return_from_record(ops, frame, saved, exits);
    // A tier-entered frame writes back into its interpreter window.
    dynasm!(ops ; .arch x64 ; =>tier ; mov rdi, r15 ; mov esi, r11d);
    symbolic(
        ops,
        relocations,
        2,
        std::ptr::from_ref::<DeoptRuntime>(runtime_data) as u64,
        RelocationTarget::DeoptRuntimeData,
    );
    dynasm!(ops
        ; .arch x64
        ; lea rcx, [rsp]
        ; lea r8, [rsp + DEOPT_DUMP_BYTES]
        ; mov r9, [r10 + REGISTERS]
    );
    runtime(ops, relocations, entry, STUB_JIT_DEOPT_WRITEBACK);
    dynasm!(ops ; .arch x64 ; call r11 ; add rsp, DEOPT_DUMP_BYTES);
    emit_plain_return(ops, frame, saved);
}
