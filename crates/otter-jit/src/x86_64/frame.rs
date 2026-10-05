//! The one x86-64 native frame built and retired by every generated tier.
//!
//! # Contents
//! - [`emit_call_entry`] / [`emit_call_entry_cold`] — the JavaScript call
//!   ABI entry: record and register window built in the callee prologue,
//!   entry accounting, receiver binding and publication.
//! - [`emit_tier_prologue`] — the entry over an already published
//!   interpreter frame (function-entry tier transfer and loop OSR).
//! - [`emit_publish_lazy_window`] — initialized register-window publication
//!   for an actual-only entry's interpreter continuation.
//! - [`emit_epilogue`] / [`emit_exits`] — constructor completion,
//!   unpublication, and side exits that resume the interpreter in place.
//! - [`emit_tail_admission`], [`emit_tail_span_check`],
//!   [`emit_tail_transfer`] and [`emit_tail_return`] — a proper tail call
//!   (§15.10.3): the callee takes the place of this called record, in place
//!   when its actuals fit the record's span and through the caller
//!   otherwise.
//!
//! # Invariants
//! - Body registers: `r15` context, `r14` published frame, `r13` register
//!   window, `rbp` this native frame.
//! - `[rbp - 40]` holds the frame to publish on return: the caller of a
//!   called record, or the published interpreter frame itself under a tier
//!   entry. Bit 0 marks a record that owes constructor completion.
//! - A call entry reserves spills, window and record, then publishes only
//!   initialized roots. Adequate actual-only entries leave the window absent;
//!   underarity initializes and publishes it before any collecting helper.
//! - Depth carry and the configured JS depth limit reject entry before the
//!   reservation, accounting, receiver preparation or frame publication.
//! - Spill roots are zeroed before publication. Tier exits restore both
//!   previous interpreter root words from the area's top 16 bytes.
//! - Graph alone saves `rbx` in the unused fixed slot at `[rbp - 48]`;
//!   baseline uses its existing instruction footprint and save geometry.
//! - The C ABI owner adapts external tier entries and runtime calls; private
//!   JavaScript entries always use the shared internal call ABI.
//! - A called record's actuals start at `[rbp + 16]`, above the return
//!   address, in the span its caller pushed: the actual count rounded up to
//!   an even word count. Missing formals are initialized only in this
//!   callee's window. A tail callee's span starts
//!   at the same address, so the caller releases exactly the span it pushed
//!   when the callee returns to it.
//!
//! # See also
//! - [`super::activation`] — receiver binding and constructor completion.
//! - [`super::call_abi`] — platform C entries and runtime calls.
//! - [`crate::call_linkage`] — the call contract.

use dynasmrt::{AssemblyOffset, DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::{JitCompileSnapshot, native_abi as abi};

use crate::{
    artifact::relocation::RelocationCapture,
    call_linkage::EntryShape,
    entry::{
        CODE_ENTRY_GENERATED_ENTRIES_OFFSET, CODE_ENTRY_TIERING_ENABLED_OFFSET,
        CODE_ENTRY_TIERING_WORK_TARGET_OFFSET, GENERATED_FEEDBACK_CLEAN_OFFSET,
        NATIVE_FRAME_OFFSET, NATIVE_FRAME_REGISTER_BASE_OFFSET, NATIVE_FRAME_SELF_OFFSET,
        NATIVE_FRAME_THIS_OFFSET, NATIVE_STACK_LIMIT_OFFSET, THREAD_OFFSET, TransitionTable,
        VALUE_UNDEFINED, VM_THREAD_INTERRUPT_CELL_OFFSET,
    },
    frame::{ActivationExits, CallEntryCold, SpillArea},
    x86_64::{
        activation::{emit_lexical_this, emit_object_receiver_test, emit_object_test},
        values::{emit_load_runtime_stub, emit_load_symbol_u64},
    },
};

/// `[rbp + RETURN_FRAME]`: the frame published on return.
const RETURN_FRAME: i32 = -40;
/// Bytes of the record reserved below the saved registers.
const RECORD_BYTES: u32 = std::mem::size_of::<abi::Frame>() as u32;

fn emit_save(ops: &mut Assembler, kind: abi::NativeFrameKind) {
    dynasm!(ops
        ; .arch x64
        ; push rbp
        ; mov rbp, rsp
        ; push r12
        ; push r13
        ; push r14
        ; push r15
        ; sub rsp, 16
    );
    if kind == abi::NativeFrameKind::Optimizing {
        dynasm!(ops ; .arch x64 ; mov [rbp - 48], rbx);
    }
}

fn emit_restore(ops: &mut Assembler, kind: abi::NativeFrameKind) {
    if kind == abi::NativeFrameKind::Optimizing {
        dynasm!(ops ; .arch x64 ; mov rbx, [rbp - 48]);
    }
    dynasm!(ops
        ; .arch x64
        ; lea rsp, [rbp - 32]
        ; pop r15
        ; pop r14
        ; pop r13
        ; pop r12
        ; pop rbp
        ; ret
    );
}

fn emit_stub(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    stub: abi::RuntimeStubDescriptor,
) {
    emit_load_runtime_stub(ops, relocations, transitions.entry(stub), stub);
    super::call_abi::emit_runtime_call(ops, stub);
}

/// The entry over the published interpreter frame: its window and record
/// become the body's, and the return publishes that frame again.
pub(crate) fn emit_tier_prologue(
    ops: &mut Assembler,
    kind: abi::NativeFrameKind,
    spill: SpillArea,
) {
    super::call_abi::emit_c_entry(ops);
    emit_save(ops, kind);
    dynasm!(ops
        ; .arch x64
        ; mov r15, rdi
        ; mov r14, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov r13, [r14 + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32]
        ; mov [rbp + RETURN_FRAME], r14
    );
    if spill.bytes == 0 {
        return;
    }
    dynasm!(ops ; .arch x64 ; sub rsp, spill.bytes as i32);
    emit_zero_scratch(ops, spill);
    dynasm!(ops
        ; .arch x64
        ; mov r10, [r14 + abi::NATIVE_FRAME_DEPTH_OFFSET as i32]
        ; mov [rbp - 64], r10
        ; mov r10, [r14 + abi::NATIVE_FRAME_MACHINE_ROOTS_OFFSET as i32]
        ; mov [rbp - 56], r10
        ; mov DWORD [r14 + abi::NATIVE_FRAME_DEPTH_OFFSET as i32 + 4], spill.safepoint as i32
        ; mov [r14 + abi::NATIVE_FRAME_MACHINE_ROOTS_OFFSET as i32], rsp
    );
}

/// Zero the exception scratch before the frame is published.
fn emit_zero_scratch(ops: &mut Assembler, spill: SpillArea) {
    if let Some(slot) = spill.scratch_slot {
        dynasm!(ops ; .arch x64 ; mov QWORD [rsp + (slot * 8) as i32], 0);
    }
}

/// Restore both interpreter root words when r11 names this tier's record.
fn emit_restore_tier_roots(ops: &mut Assembler, spill: SpillArea) {
    if spill.bytes == 0 {
        return;
    }
    let called = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; cmp r11, r14
        ; jne =>called
        ; mov r10, [rbp - 64]
        ; mov [r14 + abi::NATIVE_FRAME_DEPTH_OFFSET as i32], r10
        ; mov r10, [rbp - 56]
        ; mov [r14 + abi::NATIVE_FRAME_MACHINE_ROOTS_OFFSET as i32], r10
        ; =>called
    );
}

/// Initialize the reserved window from actuals, filling all missing slots.
fn emit_fill_window(ops: &mut Assembler, shape: EntryShape) {
    let params = i32::from(shape.param_count.min(shape.register_count));
    let missing = ops.new_dynamic_label();
    let filled = ops.new_dynamic_label();
    if params != 0 {
        dynasm!(ops ; .arch x64 ; cmp r8d, params ; jb =>missing);
    }
    let registers = i32::from(shape.register_count);
    // A shared zero-extended immediate is smaller once two suffix slots need
    // initialization. The underarity path below retains its own copy scratch.
    let shared_undefined = registers - params > 1;
    for index in 0..registers {
        if index < params {
            dynasm!(ops
                ; .arch x64
                ; mov r11, [rbp + 16 + index * 8]
                ; mov [r13 + index * 8], r11
            );
        } else if shared_undefined {
            if index == params {
                dynasm!(ops ; .arch x64 ; mov r11d, VALUE_UNDEFINED as i32);
            }
            dynasm!(ops ; .arch x64 ; mov [r13 + index * 8], r11);
        } else {
            dynasm!(ops ; .arch x64 ; mov QWORD [r13 + index * 8], VALUE_UNDEFINED as i32);
        }
    }
    if params != 0 {
        let copy = ops.new_dynamic_label();
        let fill = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch x64
            ; jmp =>filled
            ; =>missing
            ; xor r10d, r10d
            ; test r8d, r8d
            ; jz =>fill
            ; =>copy
            ; mov r11, [rbp + r10 * 8 + 16]
            ; mov [r13 + r10 * 8], r11
            ; inc r10d
            ; cmp r10d, r8d
            ; jb =>copy
            ; =>fill
            ; mov QWORD [r13 + r10 * 8], VALUE_UNDEFINED as i32
            ; inc r10d
            ; cmp r10d, i32::from(shape.register_count)
            ; jb =>fill
            ; =>filled
        );
    }
}

/// Emit the internal JavaScript entry with initialized roots and exact actuals.
pub(crate) fn emit_call_entry(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    shape: EntryShape,
    spill: SpillArea,
    cold: CallEntryCold,
) -> AssemblyOffset {
    let start = ops.offset();
    let window = u32::from(shape.register_count) * 8;
    let bytes = ((window + RECORD_BYTES).next_multiple_of(16) + spill.bytes) as i32;
    emit_save(ops, shape.kind);
    let depth_ready = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; mov r15, rdi
        ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov eax, 1
        ; test r10, r10
        ; jz =>depth_ready
        ; mov eax, [r10 + abi::NATIVE_FRAME_DEPTH_OFFSET as i32]
        ; add eax, 1
        ; jc =>cold.overflow
        ; =>depth_ready
        ; cmp rax, [r15 + std::mem::offset_of!(abi::JitCtx, generated_depth_limit) as i32]
        ; ja =>cold.overflow
    );
    dynasm!(ops
        ; .arch x64
        ; lea r11, [rsp - bytes]
        ; cmp r11, [r15 + NATIVE_STACK_LIMIT_OFFSET as i32]
        ; jb =>cold.overflow
    );
    if bytes >= 4096 {
        let probe = ops.new_dynamic_label();
        let probed = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch x64
            ; mov r10, rsp
            ; =>probe
            ; sub r10, 4096
            ; cmp r10, r11
            ; jbe =>probed
            ; mov QWORD [r10], 0
            ; jmp =>probe
            ; =>probed
        );
    }
    dynasm!(ops
        ; .arch x64
        ; mov rsp, r11
    );
    emit_zero_scratch(ops, spill);
    if spill.bytes == 0 {
        dynasm!(ops ; .arch x64 ; mov r13, rsp);
    } else {
        dynasm!(ops ; .arch x64 ; lea r13, [rsp + spill.bytes as i32]);
    }
    dynasm!(ops ; .arch x64 ; lea r14, [r13 + window as i32]);
    // RAX still holds the depth checked before reservation. Publish it only
    // into this private, unpublished record before accounting or receiver
    // classification can use RAX. No helper or collection precedes the final
    // initialized-frame publication below.
    dynasm!(ops ; .arch x64
        ; mov [r14 + abi::NATIVE_FRAME_DEPTH_OFFSET as i32], eax
        ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov [r14 + abi::NATIVE_FRAME_CALLER_OFFSET as i32], r10
    );
    if shape.derived {
        dynasm!(ops ; .arch x64 ; lea r11, [r10 + 1] ; mov [rbp + RETURN_FRAME], r11);
    } else {
        dynasm!(ops ; .arch x64 ; mov [rbp + RETURN_FRAME], r10);
    }
    // The one origin bit belongs to the existing aligned generation input.
    // Mask it before dereference; a staged target transfers the original JS
    // return rather than the trampoline's own internal continuation.
    let direct_origin = ops.new_dynamic_label();
    let origin_ready = ops.new_dynamic_label();
    let no_native_caller = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; test r9b, abi::CODE_ENTRY_STAGED_REQUEST_MASK as i8
        ; jz =>direct_origin
        ; and r9, !(abi::CODE_ENTRY_STAGED_REQUEST_MASK as i32)
        ; mov r11, [r15 + (crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_CALLER_RETURN_PC_OFFSET) as i32]
        ; jmp =>origin_ready
        ; =>direct_origin
        ; xor r11d, r11d
        ; test r10, r10
        ; jz =>no_native_caller
        ; cmp DWORD [r10 + abi::NATIVE_FRAME_CODE_OBJECT_ID_OFFSET as i32], 0
        ; je =>no_native_caller
        ; mov r11, [rbp + 8]
        ; =>no_native_caller
        ; =>origin_ready
        ; mov [r14 + abi::NATIVE_FRAME_CALLER_RETURN_PC_OFFSET as i32], r11
        ; xor r11d, r11d
        ; mov [r15 + (crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_CALLER_OFFSET) as i32], r11
        ; mov [r15 + (crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_CALLER_RETURN_PC_OFFSET) as i32], r11
    );
    if let Some((check, back)) = cold.break_even {
        dynasm!(ops
            ; .arch x64
            ; xor r12d, r12d
            ; mov rax, [r9 + CODE_ENTRY_GENERATED_ENTRIES_OFFSET as i32]
            ; inc rax
            ; mov [r9 + CODE_ENTRY_GENERATED_ENTRIES_OFFSET as i32], rax
            ; mov QWORD [r15 + GENERATED_FEEDBACK_CLEAN_OFFSET as i32], 0
        );
        emit_load_symbol_u64(
            ops,
            relocations,
            10,
            view.code_block.source_work().native_address() as u64,
            crate::artifact::relocation::RelocationTarget::SourceWorkCell {
                function_id: shape.function_id,
            },
        );
        // A Template activation charges its body's opcode count once on entry,
        // the way an interrupt budget charges a return; loops charge at polls.
        let work = i32::try_from(crate::frame::entry_work(view)).unwrap_or(i32::MAX);
        let charged = ops.new_dynamic_label();
        dynasm!(ops ; .arch x64
            ; mov rax, [r10]
            ; add rax, work
            ; jnc =>charged
            ; mov rax, -1
            ; =>charged
            ; mov [r10], rax
            ; cmp rax, [r9 + CODE_ENTRY_TIERING_WORK_TARGET_OFFSET as i32]
            ; jae =>check
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
        dynasm!(ops ; .arch x64 ; mov edx, otter_vm::value::tag::VALUE_HOLE as i32);
    }
    let window_slots = if shape.lazy_window {
        0
    } else {
        shape.register_count
    };
    dynasm!(ops
        ; .arch x64
        ; mov r11d, shape.function_id as i32
        ; mov [r14], r11
        ; mov r11, QWORD shape.header_word(window_slots) as i64
        ; mov [r14 + 8], r11
    );
    if shape.lazy_window {
        dynasm!(ops
            ; .arch x64
            ; mov QWORD [r14 + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32], 0
            ; mov QWORD [r14 + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32 + 8], 0
        );
    } else {
        dynasm!(ops
            ; .arch x64
            ; mov [r14 + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32], r13
            ; mov QWORD [r14 + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32 + 8], i32::from(shape.register_count)
        );
    }
    // Every request consumer clears the construction fields, so an ordinary
    // call (`new.target` undefined) only initializes its own record.
    let ordinary = ops.new_dynamic_label();
    let recorded = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; cmp rcx, VALUE_UNDEFINED as i32
        ; je =>ordinary
        ; mov r11d, [r15 + (crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_CONSTRUCT_LAYOUT_OFFSET) as i32]
        ; mov [r14 + abi::NATIVE_FRAME_CONSTRUCT_LAYOUT_OFFSET as i32], r11
        ; mov r11, [r15 + (crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_CONSTRUCT_RECEIVER_OFFSET) as i32]
        ; mov [r14 + abi::NATIVE_FRAME_CONSTRUCT_RECEIVER_OFFSET as i32], r11
        ; mov r11, [r15 + (crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_SUPER_ORIGIN_OFFSET) as i32]
        ; mov [r14 + abi::NATIVE_FRAME_SUPER_ORIGIN_OFFSET as i32], r11
        ; mov QWORD [r15 + (crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_SUPER_ORIGIN_OFFSET) as i32], 0
        ; mov QWORD [r15 + (crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_CONSTRUCT_LAYOUT_OFFSET) as i32], 0
        ; mov QWORD [r15 + (crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_CONSTRUCT_RECEIVER_OFFSET) as i32], VALUE_UNDEFINED as i32
        ; jmp =>recorded
        ; =>ordinary
        ; mov QWORD [r14 + abi::NATIVE_FRAME_CONSTRUCT_LAYOUT_OFFSET as i32], 0
        ; mov QWORD [r14 + abi::NATIVE_FRAME_CONSTRUCT_RECEIVER_OFFSET as i32], VALUE_UNDEFINED as i32
        ; mov QWORD [r14 + abi::NATIVE_FRAME_SUPER_ORIGIN_OFFSET as i32], 0
        ; =>recorded
    );
    dynasm!(ops
        ; .arch x64
        ; mov [r14 + NATIVE_FRAME_THIS_OFFSET as i32], rdx
        ; mov [r14 + NATIVE_FRAME_THIS_OFFSET as i32 + 8], rcx
        ; mov [r14 + NATIVE_FRAME_SELF_OFFSET as i32], rsi
        ; mov [r14 + NATIVE_FRAME_SELF_OFFSET as i32 + 8], r8
        ; lea r11, [rbp + 16]
        ; mov [r14 + abi::NATIVE_FRAME_ACTUALS_OFFSET as i32], r11
        ; mov DWORD [r14 + abi::NATIVE_FRAME_DEPTH_OFFSET as i32 + 4], spill.safepoint as i32
    );
    if spill.bytes == 0 {
        dynasm!(ops ; .arch x64 ; mov QWORD [r14 + abi::NATIVE_FRAME_MACHINE_ROOTS_OFFSET as i32], 0);
    } else {
        dynasm!(ops ; .arch x64 ; mov [r14 + abi::NATIVE_FRAME_MACHINE_ROOTS_OFFSET as i32], rsp);
    }
    dynasm!(ops
        ; .arch x64
        ; mov DWORD [r14 + abi::NATIVE_FRAME_CONTINUATION_OFFSET as i32], -1
        ; mov DWORD [r14 + abi::NATIVE_FRAME_CONTINUATION_OFFSET as i32 + 4], 0
    );
    if shape.lazy_window {
        dynasm!(ops ; .arch x64 ; lea r13, [rbp + 16]);
    } else {
        emit_fill_window(ops, shape);
    }
    dynasm!(ops ; .arch x64 ; mov [r15 + NATIVE_FRAME_OFFSET as i32], r14);
    if let Some((underarity, back)) = cold.underarity {
        dynasm!(ops ; .arch x64 ; cmp r8d, i32::from(shape.param_count) ; jb =>underarity ; =>back);
    }
    if let Some((construct, _)) = cold.construct {
        dynasm!(ops ; .arch x64 ; cmp rcx, VALUE_UNDEFINED as i32 ; jne =>construct);
    }
    if let Some((prepare, back)) = cold.prepare {
        dynasm!(ops ; .arch x64 ; test r9, r9 ; jnz =>prepare ; =>back);
    }
    if let Some((_, back)) = cold.construct {
        dynasm!(ops ; .arch x64 ; =>back);
    }
    if let Some((promote, back)) = cold.promote {
        dynasm!(ops ; .arch x64 ; test r12, r12 ; jnz =>promote ; =>back);
    }
    start
}

/// Publish initialized reserved registers when an exit needs an interpreter window.
/// Clobbers only the frame's pinned window register and r10/r11.
pub(crate) fn emit_publish_lazy_window(ops: &mut Assembler, register_count: u16) {
    let published = ops.new_dynamic_label();
    let window = i32::from(register_count) * 8;
    dynasm!(ops
        ; .arch x64
        ; cmp QWORD [r14 + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32], 0
        ; jne =>published
        ; lea r13, [r14 - window]
    );
    for index in 0..i32::from(register_count) {
        dynasm!(ops ; .arch x64 ; mov QWORD [r13 + index * 8], VALUE_UNDEFINED as i32);
    }
    emit_window_publication(ops, register_count);
    dynasm!(ops ; .arch x64 ; =>published);
}

fn emit_window_publication(ops: &mut Assembler, register_count: u16) {
    dynasm!(ops
        ; .arch x64
        ; mov WORD [r14 + 8], register_count as i16
        ; mov [r14 + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32], r13
        ; mov QWORD [r14 + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32 + 8], i32::from(register_count)
    );
}

/// Emit the cold continuations of the call entry.
pub(crate) fn emit_call_entry_cold(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    view: &JitCompileSnapshot,
    shape: EntryShape,
    exits: ActivationExits,
    cold: CallEntryCold,
) {
    // Promotion is requested only while this generation may still tier up.
    if let Some((check, back)) = cold.break_even {
        dynasm!(ops
            ; .arch x64
            ; =>check
            ; cmp DWORD [r9 + CODE_ENTRY_TIERING_ENABLED_OFFSET as i32], 0
            ; je =>back
            ; mov r12d, 1
            ; jmp =>back
        );
    }
    if let Some((underarity, back)) = cold.underarity {
        let window = i32::from(shape.register_count) * 8;
        dynasm!(ops ; .arch x64 ; =>underarity ; lea r13, [r14 - window]);
        emit_fill_window(ops, shape);
        emit_window_publication(ops, shape.register_count);
        dynasm!(ops ; .arch x64 ; jmp =>back);
    }
    // [[Construct]] of a base constructor: owe constructor completion and
    // create the receiver unless the caller allocated it. Receiver
    // conversion does not apply to the created receiver.
    if let Some((construct, constructed)) = cold.construct {
        dynasm!(ops
            ; .arch x64
            ; =>construct
            ; or BYTE [r14 + crate::entry::NATIVE_FRAME_FLAGS_OFFSET as i32], abi::NativeFrameFlags::CONSTRUCT as i8
            ; or QWORD [rbp + RETURN_FRAME], 1
            ; cmp rdx, VALUE_UNDEFINED as i32
            ; jne =>constructed
        );
        let canonical = ops.new_dynamic_label();
        let complete = ops.new_dynamic_label();
        crate::x86_64::allocation::emit_dynamic_construct_receiver(
            ops,
            relocations,
            transitions,
            view,
            complete,
            canonical,
        );
        dynasm!(ops ; .arch x64 ; =>canonical ; mov rdi, r15);
        emit_stub(
            ops,
            relocations,
            transitions,
            abi::STUB_JIT_PREPARE_ACTIVATION,
        );
        dynasm!(ops ; .arch x64 ; =>complete ; test rdx, rdx ; jnz =>exits.construct ; jmp =>constructed);
    }
    if let Some((prepare, prepared)) = cold.prepare {
        dynasm!(ops ; .arch x64 ; =>prepare ; mov rdi, r15);
        emit_stub(
            ops,
            relocations,
            transitions,
            abi::STUB_JIT_PREPARE_ACTIVATION,
        );
        dynasm!(ops ; .arch x64 ; test rdx, rdx ; jnz =>exits.construct ; jmp =>prepared);
    }
    // Promotion compiles against the published record; this activation
    // keeps its generation and later entries take the new one.
    if let Some((promote, promoted)) = cold.promote {
        dynasm!(ops ; .arch x64 ; =>promote ; mov rdi, r15);
        emit_stub(ops, relocations, transitions, abi::STUB_JIT_PROMOTE_ENTERED);
        dynasm!(ops ; .arch x64 ; jmp =>promoted);
    }
    // Nothing is published: return the overflow.
    dynasm!(ops ; .arch x64 ; =>cold.overflow ; mov rdi, r15);
    emit_stub(ops, relocations, transitions, abi::STUB_JIT_CALL_OVERFLOW);
    emit_restore(ops, shape.kind);
}

/// Return `rax`/`rdx`: constructor completion when the record owes it, then
/// publish the return frame and release this native frame.
pub(crate) fn emit_epilogue(
    ops: &mut Assembler,
    exits: ActivationExits,
    kind: abi::NativeFrameKind,
    spill: SpillArea,
) {
    emit_epilogue_to(ops, exits, kind, spill, None);
}

/// Reuse a physical restore sequence when this caller owns its final label.
/// Only the final register/stack restore is shared: constructor selection and
/// interpreter-root restoration retain their original per-path ordering.
fn emit_epilogue_to(
    ops: &mut Assembler,
    exits: ActivationExits,
    kind: abi::NativeFrameKind,
    spill: SpillArea,
    restore: Option<DynamicLabel>,
) {
    dynasm!(ops
        ; .arch x64
        ; mov r11, [rbp + RETURN_FRAME]
        ; test r11b, 1
        ; jnz =>exits.construct
    );
    emit_restore_tier_roots(ops, spill);
    dynasm!(ops ; .arch x64 ; mov [r15 + NATIVE_FRAME_OFFSET as i32], r11);
    if let Some(restore) = restore {
        dynasm!(ops ; .arch x64 ; jmp =>restore);
    } else {
        emit_restore(ops, kind);
    }
}

/// Emit the shared constructor completion and side-exit continuation.
pub(crate) fn emit_exits(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    view: &JitCompileSnapshot,
    shape: EntryShape,
    exits: ActivationExits,
    spill: SpillArea,
) {
    // §10.2.2 steps 10–12; an abrupt completion passes through.
    let restore = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    let primitive = ops.new_dynamic_label();
    let no_ticket = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; =>exits.construct ; test rdx, rdx ; jnz =>done);
    emit_object_test(ops, view, 0, done, primitive);
    dynasm!(ops ; .arch x64 ; =>primitive);
    if shape.derived {
        dynasm!(ops ; .arch x64 ; mov rsi, rax ; mov rdi, r15);
        emit_stub(
            ops,
            relocations,
            transitions,
            abi::STUB_JIT_DERIVED_CONSTRUCT_RESULT,
        );
    } else {
        dynasm!(ops ; .arch x64 ; mov rax, [r14 + NATIVE_FRAME_THIS_OFFSET as i32]);
    }
    dynasm!(ops
        ; .arch x64
        ; =>done
        ; mov r11d, [r14 + abi::NATIVE_FRAME_CONSTRUCT_LAYOUT_OFFSET as i32]
        ; or r11, [r14 + abi::NATIVE_FRAME_SUPER_ORIGIN_OFFSET as i32]
        ; jz =>no_ticket
        ; sub rsp, 16
        ; mov [rsp], rax
        ; mov [rsp + 8], rdx
        ; mov rsi, rsp
        ; mov rdi, r15
    );
    emit_stub(
        ops,
        relocations,
        transitions,
        abi::STUB_JIT_CONSTRUCTOR_TERMINAL,
    );
    dynasm!(ops
        ; .arch x64
        ; add rsp, 16
        ; =>no_ticket
        ; mov r11, [rbp + RETURN_FRAME]
        ; and r11, -2
        ; mov [r15 + NATIVE_FRAME_OFFSET as i32], r11
    );
    dynasm!(ops ; .arch x64 ; jmp =>restore);
    // A called record continues in the interpreter on its own window; a
    // tier-entered frame hands the exit back to its interpreter.
    let tier = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; =>exits.side_exit
        ; mov r11, [rbp + RETURN_FRAME]
        ; and r11, -2
        ; cmp r11, r14
        ; je =>tier
    );
    if shape.lazy_window {
        emit_publish_lazy_window(ops, shape.register_count);
    }
    if spill.bytes != 0 {
        dynasm!(ops
            ; .arch x64
            ; mov DWORD [r14 + abi::NATIVE_FRAME_DEPTH_OFFSET as i32 + 4], -1
            ; mov QWORD [r14 + abi::NATIVE_FRAME_MACHINE_ROOTS_OFFSET as i32], 0
        );
    }
    dynasm!(ops ; .arch x64 ; mov rsi, rax ; mov rdi, r15);
    emit_stub(ops, relocations, transitions, abi::STUB_JIT_DEOPT_CALL);
    emit_epilogue_to(ops, exits, shape.kind, spill, Some(restore));
    dynasm!(ops
        ; .arch x64
        ; =>tier
    );
    emit_restore_tier_roots(ops, spill);
    dynasm!(ops
        ; .arch x64
        ; mov edx, abi::NativeResultStatus::SideExit as i32
        ; mov [r15 + NATIVE_FRAME_OFFSET as i32], r11
    );
    dynasm!(ops ; .arch x64 ; =>restore);
    emit_restore(ops, shape.kind);
}

/// Branch to `leave` unless this body may hand its record to a tail callee,
/// and to `ordinary` when the record owes constructor completion.
///
/// Only a called record is replaced: a tier-entered frame belongs to its
/// interpreter, which performs the replacement itself, and so does a
/// pending interrupt, since an unbounded tail-call chain has no back-edge
/// to poll at. A constructing record calls ordinarily and completes on the
/// return that follows. Clobbers `rax` and `r11`.
pub(crate) fn emit_tail_admission(
    ops: &mut Assembler,
    leave: DynamicLabel,
    ordinary: DynamicLabel,
) {
    dynasm!(ops
        ; .arch x64
        ; mov r11, [rbp + RETURN_FRAME]
        ; cmp r11, r14
        ; je =>leave
        ; test r11b, 1
        ; jnz =>ordinary
        ; mov rax, [r15 + THREAD_OFFSET as i32]
        ; mov rax, [rax + VM_THREAD_INTERRUPT_CELL_OFFSET as i32]
        ; cmp BYTE [rax], 0
        ; jne =>leave
    );
}

/// Branch to `outgrown` unless a tail callee's span of `words` (an even
/// count) fits this record's aligned actual span. Clobbers `rax`.
pub(crate) fn emit_tail_span_check(ops: &mut Assembler, words: u32, outgrown: DynamicLabel) {
    if words == 0 {
        return;
    }
    dynasm!(ops
        ; .arch x64
        ; mov eax, [r14 + abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET as i32]
        ; cmp eax, (words - 1) as i32
        ; jb =>outgrown
    );
}

/// Retire this called record and enter the callee in `rsi` in its place.
///
/// The callee's `count` actuals occupy the `bytes` just pushed at `rsp`, and
/// [`emit_tail_admission`] and [`emit_tail_span_check`] passed. The record
/// is unpublished, the span moves up to `[rbp + 16]` where this record's own
/// actuals began, the saved registers are restored with `rsp` on the return
/// address this record was called with, and control jumps to `target` with
/// the call ABI registers set; the callee returns straight to this record's
/// caller. Nothing allocates between the unpublication and the callee's
/// entry.
pub(crate) fn emit_tail_transfer(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    bytes: u32,
    count: u32,
    target: crate::x86_64::js_call::CallTarget,
    kind: abi::NativeFrameKind,
) {
    if kind == abi::NativeFrameKind::Optimizing {
        dynasm!(ops ; .arch x64 ; mov rbx, [rbp - 48]);
    }
    dynasm!(ops
        ; .arch x64
        ; mov rdi, r15
        ; mov edx, VALUE_UNDEFINED as i32
        ; mov ecx, VALUE_UNDEFINED as i32
        ; mov r8d, count as i32
        ; mov r11, [rbp + RETURN_FRAME]
        ; mov [r15 + NATIVE_FRAME_OFFSET as i32], r11
    );
    // The destination lies above the pushed span: copying from the top
    // down reads every word before a store can reach it.
    if bytes <= 512 {
        let mut offset = bytes as i32;
        while offset > 0 {
            offset -= 8;
            dynasm!(ops
                ; .arch x64
                ; mov rax, [rsp + offset]
                ; mov [rbp + 16 + offset], rax
            );
        }
    } else {
        let copy = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch x64
            ; mov r10d, bytes as i32
            ; =>copy
            ; sub r10, 8
            ; mov rax, [rsp + r10]
            ; mov [rbp + r10 + 16], rax
            ; jnz =>copy
        );
    }
    dynasm!(ops
        ; .arch x64
        ; mov r12, [rbp - 8]
        ; mov r13, [rbp - 16]
        ; mov r14, [rbp - 24]
        ; mov r15, [rbp - 32]
        ; lea rsp, [rbp + 8]
        ; mov rbp, [rbp]
    );
    crate::x86_64::js_call::emit_tail_branch(ops, relocations, transitions, target);
}

/// Retire this called record and return `Continue`: the context holds the
/// tail call it staged, and the caller enters that request in its place.
/// [`emit_tail_admission`] passed, so the record owes no constructor
/// completion.
pub(crate) fn emit_tail_return(ops: &mut Assembler, kind: abi::NativeFrameKind) {
    dynasm!(ops
        ; .arch x64
        ; mov eax, VALUE_UNDEFINED as i32
        ; mov edx, abi::NativeResultStatus::Continue as i32
        ; mov r11, [rbp + RETURN_FRAME]
        ; mov [r15 + NATIVE_FRAME_OFFSET as i32], r11
    );
    emit_restore(ops, kind);
}

#[cfg(test)]
#[path = "frame_tests.rs"]
mod tests;
