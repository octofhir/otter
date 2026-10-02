//! Template activations on x86-64: the one native frame a call builds and
//! retires.
//!
//! # Contents
//! - [`emit_call_entry`] / [`emit_call_entry_cold`] — the JavaScript call
//!   ABI entry: record and register window built in the callee prologue,
//!   entry accounting, receiver binding and publication.
//! - [`emit_tier_prologue`] — the entry over an already published
//!   interpreter frame (function-entry tier transfer and loop OSR).
//! - [`emit_epilogue`] / [`emit_exits`] — constructor completion,
//!   unpublication, and side exits that resume the interpreter in place.
//!
//! # Invariants
//! - Body registers: `r15` context, `r14` published frame, `r13` register
//!   window, `rbp` this native frame.
//! - `[rbp - 40]` holds the frame to publish on return: the caller of a
//!   called record, or the published interpreter frame itself under a tier
//!   entry. Bit 0 marks a record that owes constructor completion.
//! - A call entry reserves the window and the record below the saved
//!   registers and publishes the record after every field and register is
//!   initialized. Nothing allocates or reenters before publication.
//!
//! # See also
//! - [`crate::x86_64::activation`] — pieces shared with the optimizing tier.
//! - [`crate::call_linkage`] — the call contract.

use dynasmrt::{AssemblyOffset, DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::{JitCompileSnapshot, native_abi as abi};

pub(super) use crate::x86_64::activation::EntryShape;
use crate::{
    artifact::relocation::RelocationCapture,
    entry::{
        CODE_ENTRY_GENERATED_ENTRIES_OFFSET, CODE_ENTRY_TIERING_BREAK_EVEN_OFFSET,
        CODE_ENTRY_TIERING_ENABLED_OFFSET, GENERATED_FEEDBACK_CLEAN_OFFSET, NATIVE_FRAME_OFFSET,
        NATIVE_FRAME_REGISTER_BASE_OFFSET, NATIVE_FRAME_SELF_OFFSET, NATIVE_FRAME_THIS_OFFSET,
        NATIVE_STACK_LIMIT_OFFSET, TransitionTable, VALUE_UNDEFINED,
    },
    x86_64::activation::{emit_lexical_this, emit_object_receiver_test, emit_object_test},
};

/// `[rbp + RETURN_FRAME]`: the frame published on return.
const RETURN_FRAME: i32 = -40;
/// Bytes of the record reserved below the saved registers.
const RECORD_BYTES: u32 = std::mem::size_of::<abi::Frame>() as u32;

/// Labels shared by the exits of one Template body.
#[derive(Debug, Clone, Copy)]
pub(super) struct ActivationExits {
    /// Constructor completion of `rax`/`rdx`, then the return.
    pub(super) construct: DynamicLabel,
    /// Side exit with the encoded exit in `rax`.
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

fn emit_save(ops: &mut Assembler) {
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
}

fn emit_restore(ops: &mut Assembler) {
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
    super::emit_load_runtime_stub(ops, relocations, transitions.entry(stub), stub);
    dynasm!(ops ; .arch x64 ; call r11);
}

/// The entry over the published interpreter frame: its window and record
/// become the body's, and the return publishes that frame again.
pub(super) fn emit_tier_prologue(ops: &mut Assembler) {
    emit_save(ops);
    dynasm!(ops
        ; .arch x64
        ; mov r15, rdi
        ; mov r14, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov r13, [r14 + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32]
        ; mov [rbp + RETURN_FRAME], r14
    );
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
    let window = u32::from(shape.register_count) * 8;
    let bytes = (window + RECORD_BYTES).next_multiple_of(16) as i32;
    emit_save(ops);
    dynasm!(ops
        ; .arch x64
        ; mov r15, rdi
        ; lea rax, [rsp - bytes]
        ; cmp rax, [r15 + NATIVE_STACK_LIMIT_OFFSET as i32]
        ; jb =>overflow
    );
    if bytes >= 4096 {
        // Touch each page before moving the stack pointer past a guard page.
        let probe = ops.new_dynamic_label();
        let probed = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch x64
            ; mov r10, rsp
            ; =>probe
            ; sub r10, 4096
            ; cmp r10, rax
            ; jbe =>probed
            ; mov QWORD [r10], 0
            ; jmp =>probe
            ; =>probed
        );
    }
    dynasm!(ops
        ; .arch x64
        ; mov rsp, rax
        ; mov r13, rsp
        ; lea r14, [rsp + window as i32]
        // Entry accounting; past break-even the record asks for promotion.
        ; xor r12d, r12d
        ; mov rax, [r9 + CODE_ENTRY_GENERATED_ENTRIES_OFFSET as i32]
        ; inc rax
        ; mov [r9 + CODE_ENTRY_GENERATED_ENTRIES_OFFSET as i32], rax
        ; mov QWORD [r15 + GENERATED_FEEDBACK_CLEAN_OFFSET as i32], 0
        ; cmp rax, [r9 + CODE_ENTRY_TIERING_BREAK_EVEN_OFFSET as i32]
        ; jae =>break_even.0
        ; =>break_even.1
    );
    if shape.sloppy_receiver() {
        emit_object_receiver_test(ops, view);
    }
    if shape.lexical_this {
        emit_lexical_this(ops, view);
    }
    if shape.derived {
        dynasm!(ops ; .arch x64 ; mov edx, otter_vm::value::tag::VALUE_HOLE as i32);
    }
    let have_depth = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov eax, 1
        ; test r10, r10
        ; jz =>have_depth
        ; mov eax, [r10 + abi::NATIVE_FRAME_DEPTH_OFFSET as i32]
        ; inc eax
        ; =>have_depth
    );
    // A derived constructor is entered only by `[[Construct]]` and always
    // owes constructor completion.
    if shape.derived {
        dynasm!(ops ; .arch x64 ; lea r11, [r10 + 1] ; mov [rbp + RETURN_FRAME], r11);
    } else {
        dynasm!(ops ; .arch x64 ; mov [rbp + RETURN_FRAME], r10);
    }
    dynasm!(ops
        ; .arch x64
        ; mov r11d, shape.function_id as i32
        ; mov [r14], r11
        ; mov r11, QWORD shape.header_word(shape.register_count) as i64
        ; mov [r14 + 8], r11
        ; mov [r14 + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32], r13
        ; mov QWORD [r14 + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32 + 8], i32::from(shape.register_count)
        ; mov [r14 + NATIVE_FRAME_THIS_OFFSET as i32], rdx
        ; mov [r14 + NATIVE_FRAME_THIS_OFFSET as i32 + 8], rcx
        ; mov [r14 + NATIVE_FRAME_SELF_OFFSET as i32], rsi
        ; mov [r14 + NATIVE_FRAME_SELF_OFFSET as i32 + 8], r8
        ; lea r11, [rbp + 16]
        ; mov [r14 + abi::NATIVE_FRAME_ACTUALS_OFFSET as i32], r11
        ; mov [r14 + abi::NATIVE_FRAME_CALLER_OFFSET as i32], r10
        ; mov [r14 + abi::NATIVE_FRAME_DEPTH_OFFSET as i32], eax
        ; mov DWORD [r14 + abi::NATIVE_FRAME_DEPTH_OFFSET as i32 + 4], -1
        ; mov QWORD [r14 + abi::NATIVE_FRAME_MACHINE_ROOTS_OFFSET as i32], 0
        ; mov DWORD [r14 + abi::NATIVE_FRAME_CONTINUATION_OFFSET as i32], -1
        ; mov DWORD [r14 + abi::NATIVE_FRAME_CONTINUATION_OFFSET as i32 + 4], 0
    );
    // Formals from the padded span, `undefined` above them.
    let params = i32::from(shape.param_count.min(shape.register_count));
    for index in 0..i32::from(shape.register_count) {
        if index < params {
            dynasm!(ops
                ; .arch x64
                ; mov r11, [rbp + 16 + index * 8]
                ; mov [r13 + index * 8], r11
            );
        } else {
            dynasm!(ops ; .arch x64 ; mov QWORD [r13 + index * 8], VALUE_UNDEFINED as i32);
        }
    }
    dynasm!(ops ; .arch x64 ; mov [r15 + NATIVE_FRAME_OFFSET as i32], r14);
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
    let promote = (ops.new_dynamic_label(), ops.new_dynamic_label());
    dynasm!(ops ; .arch x64 ; test r12, r12 ; jnz =>promote.0 ; =>promote.1);
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
        ; .arch x64
        ; =>check
        ; cmp DWORD [r9 + CODE_ENTRY_TIERING_ENABLED_OFFSET as i32], 0
        ; je =>back
        ; mov r12d, 1
        ; jmp =>back
    );
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
            ; mov rdi, r15
        );
        emit_stub(
            ops,
            relocations,
            transitions,
            abi::STUB_JIT_PREPARE_ACTIVATION,
        );
        dynasm!(ops ; .arch x64 ; test rdx, rdx ; jnz =>exits.construct ; jmp =>constructed);
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
    let (promote, promoted) = cold.promote;
    dynasm!(ops ; .arch x64 ; =>promote ; mov rdi, r15);
    emit_stub(ops, relocations, transitions, abi::STUB_JIT_PROMOTE_ENTERED);
    dynasm!(ops ; .arch x64 ; jmp =>promoted);
    // Nothing is published: return the overflow.
    dynasm!(ops ; .arch x64 ; =>cold.overflow ; mov rdi, r15);
    emit_stub(ops, relocations, transitions, abi::STUB_JIT_CALL_OVERFLOW);
    emit_restore(ops);
}

/// Return `rax`/`rdx`: constructor completion when the record owes it, then
/// publish the return frame and release this native frame.
pub(super) fn emit_epilogue(ops: &mut Assembler, exits: ActivationExits) {
    dynasm!(ops
        ; .arch x64
        ; mov r11, [rbp + RETURN_FRAME]
        ; test r11b, 1
        ; jnz =>exits.construct
        ; mov [r15 + NATIVE_FRAME_OFFSET as i32], r11
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
    dynasm!(ops ; .arch x64 ; =>exits.construct ; test rdx, rdx ; jnz =>done);
    emit_object_test(ops, view, 0, done, primitive);
    dynasm!(ops ; .arch x64 ; =>primitive);
    if derived {
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
        ; mov r11, [rbp + RETURN_FRAME]
        ; and r11, -2
        ; mov [r15 + NATIVE_FRAME_OFFSET as i32], r11
    );
    emit_restore(ops);
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
        ; mov rsi, rax
        ; mov rdi, r15
    );
    emit_stub(ops, relocations, transitions, abi::STUB_JIT_DEOPT_CALL);
    emit_epilogue(ops, exits);
    dynasm!(ops
        ; .arch x64
        ; =>tier
        ; mov edx, abi::NativeResultStatus::SideExit as i32
        ; mov [r15 + NATIVE_FRAME_OFFSET as i32], r11
    );
    emit_restore(ops);
}
