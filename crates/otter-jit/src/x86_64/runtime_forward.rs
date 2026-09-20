//! Runtime-selected forwarding through shared native call completion.
//!
//! # Contents
//! - Leaf admission into current generated-call metadata.
//! - Bounded dynamic frame construction and live argument copying.
//! - Generated entry, deoptimization, exception routing, and exact cleanup.
//!
//! # Invariants
//! - Scratch metadata contains no moving roots. The published caller remains the
//!   authoritative root owner until the private callee frame is complete.
//! - Every register and incoming-argument word is initialized before allocation.
//! - The dynamic allocation-size slot releases the callee and plan scratch on
//!   every pre-entry rejection and every completed generated call.
//! - `restore_roots` preserves `r11`, which carries a pure result or exception.
//!
//! # See also
//! - `crate::x86_64` — shared target boundary.
//! - `crate::arm64::direct_call::runtime_forward` — peer target lifecycle.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::{JitCompileSnapshot, jit::JitDirectCallPlan, native_abi as abi};

use crate::{
    Unsupported,
    artifact::{
        CodeMapCapture, CodeRegion,
        relocation::{RelocationCapture, RelocationTarget},
    },
    entry::{
        ACTIVATION_BASE_OFFSET, ACTIVATION_LIMIT_OFFSET, ACTIVATION_TOP_PTR_OFFSET,
        CODE_ENTRY_CODE_OBJECT_ID_OFFSET, CODE_ENTRY_FLAGS_OFFSET,
        CODE_ENTRY_GENERATED_DEOPTS_OFFSET, CODE_ENTRY_GENERATED_STACK_FRAME_BYTES_OFFSET,
        CODE_ENTRY_GENERATED_THROWS_OFFSET, CODE_ENTRY_NATIVE_FRAME_HEADER_OFFSET,
        FUNCTION_ENTRY_GENERATION_CELL_OFFSET, GENERATED_FEEDBACK_CLEAN_OFFSET,
        GLOBAL_THIS_OFFSET_PTR_OFFSET, NATIVE_FRAME_NEW_TARGET_OFFSET, NATIVE_FRAME_OFFSET,
        NATIVE_FRAME_REGISTER_BASE_OFFSET, NATIVE_FRAME_SELF_OFFSET, NATIVE_FRAME_STACK_SIZE,
        NATIVE_FRAME_THIS_OFFSET, NATIVE_FRAME_UPVALUE_BASE_OFFSET,
        NATIVE_FRAME_UPVALUE_COUNT_OFFSET, NATIVE_STACK_LIMIT_OFFSET, THREAD_OFFSET,
        TransitionTable, VALUE_NULL, VALUE_UNDEFINED, VM_THREAD_CODE_OBJECT_ID_OFFSET,
        VM_THREAD_CURRENT_FRAME_OFFSET,
    },
};

const MAX_DIRECT_CALL_FRAME_BYTES: u32 = 4_080;
const PLAN_BYTES: u32 = (std::mem::size_of::<JitDirectCallPlan>() as u32 + 15) & !15;
const SCRATCH_BYTES: u32 = PLAN_BYTES + 32;
const SCRATCH_CALLEE: u32 = PLAN_BYTES;
const SCRATCH_RECEIVER: u32 = PLAN_BYTES + 8;
const SCRATCH_COUNT: u32 = PLAN_BYTES + 16;

const RESULT_WORD: u32 = NATIVE_FRAME_STACK_SIZE;
const ENTRY_WORD: u32 = NATIVE_FRAME_STACK_SIZE + 8;
const CALLER_FRAME: u32 = NATIVE_FRAME_STACK_SIZE + 16;
const CALLER_CODE_OBJECT_ID: u32 = NATIVE_FRAME_STACK_SIZE + 24;
const TARGET_CELL: u32 = NATIVE_FRAME_STACK_SIZE + 32;
const ALLOCATION_SIZE: u32 = NATIVE_FRAME_STACK_SIZE + 40;
const UPVALUE_BASE: u32 = NATIVE_FRAME_STACK_SIZE + 48;

const PLAN_ENTRY: u32 = std::mem::offset_of!(JitDirectCallPlan, entry_cell) as u32;
const PLAN_THIS: u32 = std::mem::offset_of!(JitDirectCallPlan, this_mode) as u32;
const PLAN_PARAMS: u32 = std::mem::offset_of!(JitDirectCallPlan, param_count) as u32;
const PLAN_REGISTERS: u32 = std::mem::offset_of!(JitDirectCallPlan, register_count) as u32;
const PLAN_OWN: u32 = std::mem::offset_of!(JitDirectCallPlan, own_upvalue_count) as u32;
const PLAN_INHERITED: u32 = std::mem::offset_of!(JitDirectCallPlan, inherited_upvalue_count) as u32;
const PLAN_ACTUALS: u32 = std::mem::offset_of!(JitDirectCallPlan, needs_incoming_arguments) as u32;

const INCOMING_ARGUMENTS_HEADER_WORD: u32 = (abi::NativeFrameFlags::INCOMING_ARGUMENTS as u32)
    << (8
        * (std::mem::offset_of!(abi::VmFrameHeader, flags)
            - std::mem::offset_of!(abi::VmFrameHeader, register_count)));

#[allow(clippy::useless_conversion)] // dynasm dynamic-register operands call `Into<u8>`.
fn load64(ops: &mut Assembler, register: u8, value: u64) {
    dynasm!(ops ; .arch x64 ; mov Rq(register), QWORD value as i64);
}

fn runtime(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    descriptor: abi::RuntimeStubDescriptor,
) {
    let start = ops.offset().0;
    load64(ops, 11, table.entry(descriptor));
    relocations.record_x86_imm64(
        start,
        ops.offset().0,
        11,
        RelocationTarget::runtime_stub(descriptor),
    );
}

fn cage_base(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    register: u8,
) {
    let start = ops.offset().0;
    load64(ops, register, view.cage_base as u64);
    relocations.record_x86_imm64(
        start,
        ops.offset().0,
        register,
        RelocationTarget::GcCageBase,
    );
}

fn release_dynamic(ops: &mut Assembler) {
    dynasm!(ops
        ; .arch x64
        ; mov r10d, [rsp + ALLOCATION_SIZE as i32]
        ; add rsp, r10
    );
}

#[allow(clippy::useless_conversion)] // dynasm dynamic-register operands call `Into<u8>`.
fn emit_object_branch(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    value: u8,
    object: DynamicLabel,
    primitive: DynamicLabel,
) {
    load64(ops, 10, otter_vm::value::tag::NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; mov r11, Rq(value)
        ; and r11, r10
        ; test r11, r11
        ; jnz >non_cell
        ; mov r11d, Rd(value)
    );
    cage_base(ops, relocations, view, 10);
    dynasm!(ops ; .arch x64 ; add r11, r10 ; movzx r11d, BYTE [r11]);
    for tag in view.primitive_cell_type_tags {
        dynasm!(ops ; .arch x64 ; cmp r11d, tag as i32 ; je =>primitive);
    }
    dynasm!(ops
        ; .arch x64
        ; jmp =>object
        ; non_cell:
        ; mov r11, Rq(value)
        ; shr r11, 48
        ; test r11, r11
        ; jnz =>primitive
        ; mov r11d, Rd(value)
        ; and r11d, 0xffff
        ; cmp r11d, otter_vm::value::tag::FUNCTION_ID_TAG as i32
        ; je =>object
        ; jmp =>primitive
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_runtime_forward<Load, Store, Restore, LoadBinding>(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    table: &TransitionTable,
    [dst, method, callee, receiver]: [u16; 4],
    logical_pc: u32,
    byte_pc: u32,
    code_map: Option<&mut CodeMapCapture>,
    bail: DynamicLabel,
    finish_error: DynamicLabel,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
    done: DynamicLabel,
    mut load: Load,
    mut store: Store,
    mut restore_roots: Restore,
    mut load_binding: LoadBinding,
) -> Result<(), Unsupported>
where
    Load: FnMut(&mut Assembler, u16, u8, u32) -> Result<(), Unsupported>,
    Store: FnMut(&mut Assembler, u16, u8, u32) -> Result<(), Unsupported>,
    Restore: FnMut(&mut Assembler) -> Result<(), Unsupported>,
    LoadBinding: FnMut(&mut Assembler, u16, u8) -> Result<(), Unsupported>,
{
    let caller_bail = ops.new_dynamic_label();
    let scratch_miss = ops.new_dynamic_label();
    let uncommitted_rejected = ops.new_dynamic_label();
    let entry_rejected = ops.new_dynamic_label();
    let prepare_error = ops.new_dynamic_label();
    let prepare_fatal = ops.new_dynamic_label();
    let result_ready = ops.new_dynamic_label();
    let started_side_exit = ops.new_dynamic_label();
    let started_throw = ops.new_dynamic_label();
    let started_fatal = ops.new_dynamic_label();
    let start = ops.offset().0;

    dynasm!(ops
        ; .arch x64
        ; mov r10, [r15 + ACTIVATION_TOP_PTR_OFFSET as i32]
        ; mov r11, [r10]
        ; cmp r11, [r15 + ACTIVATION_LIMIT_OFFSET as i32]
        ; jae =>caller_bail
    );
    load(ops, method, 6, 0)?;
    load(ops, callee, 2, 0)?;
    load(ops, receiver, 1, 0)?;
    dynasm!(ops
        ; .arch x64
        ; sub rsp, SCRATCH_BYTES as i32
        ; mov [rsp + SCRATCH_CALLEE as i32], rdx
        ; mov [rsp + SCRATCH_RECEIVER as i32], rcx
        ; mov rdi, r15
        ; lea rcx, [rsp]
    );
    runtime(ops, relocations, table, abi::STUB_JIT_FORWARD_CALL_PLAN);
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; cmp rax, -1
        ; je =>scratch_miss
        ; mov [rsp + SCRATCH_COUNT as i32], rax
        ; movzx r8d, WORD [rsp + PLAN_OWN as i32]
        ; movzx r9d, WORD [rsp + PLAN_INHERITED as i32]
        ; add r8, r9
        ; shl r8, 2
        ; add r8, (UPVALUE_BASE + 7) as i32
        ; and r8, -8
        ; movzx r9d, WORD [rsp + PLAN_REGISTERS as i32]
        ; lea r9, [r8 + r9 * 8]
        ; cmp BYTE [rsp + PLAN_ACTUALS as i32], 0
        ; je >windows_sized
        ; mov r10, [rsp + SCRATCH_COUNT as i32]
        ; lea r9, [r9 + r10 * 8]
        ; windows_sized:
        ; add r9, 15
        ; and r9, -16
        ; lea r10, [r9 + SCRATCH_BYTES as i32]
        ; cmp r10, MAX_DIRECT_CALL_FRAME_BYTES as i32
        ; ja =>scratch_miss
        ; mov r11, rsp
        ; sub rsp, r9
        ; mov [rsp + ALLOCATION_SIZE as i32], r10d
        ; mov [rsp + CALLER_CODE_OBJECT_ID as i32], r11
        ; mov rax, [r11 + SCRATCH_COUNT as i32]
        ; mov [rsp + abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET as i32], eax
        ; lea rax, [rsp + r8]
        ; mov [rsp + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32], rax
        ; mov rsi, [r11 + PLAN_ENTRY as i32]
        ; mov rax, [rsi + FUNCTION_ENTRY_GENERATION_CELL_OFFSET as i32]
        ; test rax, rax
        ; jz =>uncommitted_rejected
        ; mov [rsp + TARGET_CELL as i32], rax
        ; mov ecx, [rax + CODE_ENTRY_GENERATED_STACK_FRAME_BYTES_OFFSET as i32]
        ; test ecx, ecx
        ; jz =>uncommitted_rejected
        ; mov rdx, rsp
        ; sub rdx, rcx
        ; jc =>uncommitted_rejected
        ; cmp rdx, [r15 + NATIVE_STACK_LIMIT_OFFSET as i32]
        ; jb =>uncommitted_rejected
        ; mov rdx, [rax]
        ; test rdx, rdx
        ; jz =>entry_rejected
        ; mov [rsp + ENTRY_WORD as i32], rdx
        ; mov rdx, [rax + CODE_ENTRY_NATIVE_FRAME_HEADER_OFFSET as i32]
        ; mov [rsp], rdx
        ; mov edx, [rax + CODE_ENTRY_NATIVE_FRAME_HEADER_OFFSET as i32 + 8]
        ; cmp BYTE [r11 + PLAN_ACTUALS as i32], 0
        ; je >header_ready
        ; test DWORD [rax + CODE_ENTRY_FLAGS_OFFSET as i32], abi::CODE_ENTRY_PARAMETER_PREFIX as i32
        ; jnz =>uncommitted_rejected
        ; or edx, INCOMING_ARGUMENTS_HEADER_WORD as i32
        ; header_ready:
        ; mov [rsp + 8], edx
        ; mov r9, [r11 + SCRATCH_CALLEE as i32]
        ; mov rcx, [r11 + SCRATCH_RECEIVER as i32]
        ; mov [rsp + NATIVE_FRAME_SELF_OFFSET as i32], r9
    );

    let direct_function = ops.new_dynamic_label();
    let closure_ready = ops.new_dynamic_label();
    load64(ops, 10, otter_vm::value::tag::NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; mov r8, r9
        ; and r8, r10
        ; test r8, r8
        ; jnz =>direct_function
        ; mov r8, [r9 + view.closure_call_layout.upvalue_base_byte as i32]
        ; mov r10d, [r9 + view.closure_call_layout.upvalue_count_byte as i32]
        ; mov edx, [r9 + view.closure_call_layout.eval_env_byte as i32]
        ; mov eax, [r9 + view.closure_call_layout.flags_byte as i32]
    );
    let no_bound_this = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; test eax, view.closure_call_layout.bound_this_flag as i32
        ; jz =>no_bound_this
        ; mov rcx, [r9 + view.closure_call_layout.bound_this_byte as i32]
        ; =>no_bound_this
        ; jmp =>closure_ready
        ; =>direct_function
        ; xor r8d, r8d
        ; xor r10d, r10d
        ; xor edx, edx
        ; =>closure_ready
        ; mov [rsp + NATIVE_FRAME_UPVALUE_BASE_OFFSET as i32], r8
        ; mov [rsp + NATIVE_FRAME_UPVALUE_COUNT_OFFSET as i32], r10d
        ; mov [rsp + abi::NATIVE_FRAME_EVAL_ENV_OFFSET as i32], edx
    );

    let this_ready = ops.new_dynamic_label();
    let global_this = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; mov r11, [rsp + CALLER_CODE_OBJECT_ID as i32]
        ; cmp BYTE [r11 + PLAN_THIS as i32], otter_vm::JitDirectCallThisMode::StrictOrLexical as i8
        ; je =>this_ready
    );
    load64(ops, 10, VALUE_UNDEFINED);
    dynasm!(ops ; .arch x64 ; cmp rcx, r10 ; je =>global_this);
    load64(ops, 10, VALUE_NULL);
    dynasm!(ops ; .arch x64 ; cmp rcx, r10 ; je =>global_this);
    let object_receiver = ops.new_dynamic_label();
    emit_object_branch(
        ops,
        relocations,
        view,
        1,
        object_receiver,
        uncommitted_rejected,
    );
    dynasm!(ops ; .arch x64 ; =>global_this);
    dynasm!(ops
        ; .arch x64
        ; mov r10, [r15 + GLOBAL_THIS_OFFSET_PTR_OFFSET as i32]
        ; test r10, r10
        ; jz =>uncommitted_rejected
        ; mov ecx, [r10]
        ; test ecx, ecx
        ; jz =>uncommitted_rejected
    );
    cage_base(ops, relocations, view, 10);
    dynasm!(ops
        ; .arch x64
        ; add rcx, r10
        ; jmp =>this_ready
        ; =>object_receiver
        ; =>this_ready
        ; mov [rsp + NATIVE_FRAME_THIS_OFFSET as i32], rcx
    );
    load64(ops, 10, VALUE_UNDEFINED);
    dynasm!(ops ; .arch x64 ; mov [rsp + NATIVE_FRAME_NEW_TARGET_OFFSET as i32], r10);

    // Initialize the complete traced register/actual window before allocation.
    dynasm!(ops
        ; .arch x64
        ; mov r11, [rsp + CALLER_CODE_OBJECT_ID as i32]
        ; movzx ecx, WORD [r11 + PLAN_REGISTERS as i32]
        ; cmp BYTE [r11 + PLAN_ACTUALS as i32], 0
        ; je >initialize_words
        ; add ecx, [rsp + abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET as i32]
        ; initialize_words:
        ; mov rdx, [rsp + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32]
        ; test ecx, ecx
        ; jz >initialized
        ; initialize_loop:
        ; mov [rdx], r10
        ; add rdx, 8
        ; sub ecx, 1
        ; jne <initialize_loop
        ; initialized:
        ; movzx edx, WORD [r11 + PLAN_OWN as i32]
        ; test edx, edx
        ; jz >captures_ready
        ; movzx ecx, WORD [r11 + PLAN_INHERITED as i32]
        ; lea rax, [rsp + UPVALUE_BASE as i32]
        ; mov [rsp + NATIVE_FRAME_UPVALUE_BASE_OFFSET as i32], rax
        ; mov DWORD [rsp + NATIVE_FRAME_UPVALUE_COUNT_OFFSET as i32], 0
        ; mov rdi, r15
        ; mov rsi, rsp
    );
    runtime(ops, relocations, table, abi::STUB_JIT_INITIALIZE_UPVALUES);
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; test rdx, rdx
        ; je >captures_ready
        ; cmp edx, abi::NativeResultStatus::SideExit as i32
        ; je =>uncommitted_rejected
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>prepare_error
        ; jmp =>prepare_fatal
        ; captures_ready:
        ; mov r11, [rsp + CALLER_CODE_OBJECT_ID as i32]
        ; movzx edx, WORD [r11 + PLAN_PARAMS as i32]
        ; mov rdi, r15
        ; mov rsi, rsp
    );
    runtime(
        ops,
        relocations,
        table,
        abi::STUB_JIT_COPY_FORWARDED_ARGUMENTS,
    );
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; cmp rax, -1
        ; je =>uncommitted_rejected
        ; mov r8, rax
        ; mov r11, [rsp + CALLER_CODE_OBJECT_ID as i32]
        ; movzx edx, WORD [r11 + PLAN_PARAMS as i32]
        ; movzx ecx, WORD [r11 + PLAN_REGISTERS as i32]
    );

    for (index, storage) in view.code_block.forwarded_argument_bindings() {
        let otter_bytecode::ArgumentBindingStorage::Register { reg } = storage else {
            continue;
        };
        let next = ops.new_dynamic_label();
        let actual = ops.new_dynamic_label();
        dynasm!(ops ; .arch x64 ; cmp r8, index as i32 ; jbe =>next);
        dynasm!(ops
            ; .arch x64
            ; mov eax, [rsp + ALLOCATION_SIZE as i32]
            ; lea r10, [rsp + rax]
        );
        load_binding(ops, reg, 10)?;
        dynasm!(ops
            ; .arch x64
            ; mov rax, [rsp + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32]
            ; cmp edx, index as i32
            ; jbe =>actual
            ; mov [rax + index as i32 * 8], r11
            ; =>actual
            ; cmp DWORD [rsp + abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET as i32], 0
            ; je =>next
            ; lea rax, [rax + rcx * 8]
            ; mov [rax + index as i32 * 8], r11
            ; =>next
        );
    }

    dynasm!(ops
        ; .arch x64
        ; mov r12, [rsp + TARGET_CELL as i32]
        ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov [rsp + CALLER_FRAME as i32], r10
        ; mov r11, [r15 + THREAD_OFFSET as i32]
        ; mov r10, [r11 + VM_THREAD_CODE_OBJECT_ID_OFFSET as i32]
        ; mov [rsp + CALLER_CODE_OBJECT_ID as i32], r10
        ; mov r8, [r15 + ACTIVATION_TOP_PTR_OFFSET as i32]
        ; mov r9, [r8]
        ; mov r10, [r15 + ACTIVATION_BASE_OFFSET as i32]
        ; mov [r10 + r9 * 8], rsp
        ; add r9, 1
        ; mov [r8], r9
        ; mov [r15 + NATIVE_FRAME_OFFSET as i32], rsp
        ; mov r10, [r12 + CODE_ENTRY_CODE_OBJECT_ID_OFFSET as i32]
        ; mov [r11 + VM_THREAD_CURRENT_FRAME_OFFSET as i32], rsp
        ; mov [r11 + VM_THREAD_CODE_OBJECT_ID_OFFSET as i32], r10
    );
    let entry_start = ops.offset().0;
    crate::entry::x86_64_tiering::emit_entry(ops);
    dynasm!(ops
        ; .arch x64
        ; mov QWORD [r15 + GENERATED_FEEDBACK_CLEAN_OFFSET as i32], 0
        ; mov r11, [rsp + ENTRY_WORD as i32]
        ; mov rdi, r15
        ; call r11
    );
    if let Some(map) = code_map {
        map.record(CodeRegion::call_structural(
            "runtimeForwardCallFrameSetup",
            start,
            entry_start,
            view.code_block.id,
            logical_pc,
            byte_pc,
            None,
        ));
        map.record(CodeRegion::call_structural(
            "runtimeForwardCallNativeEntry",
            entry_start,
            ops.offset().0,
            view.code_block.id,
            logical_pc,
            byte_pc,
            None,
        ));
    }

    dynasm!(ops
        ; .arch x64
        ; cmp edx, abi::NativeResultStatus::SideExit as i32
        ; je =>started_side_exit
        ; cmp edx, abi::NativeResultStatus::Success as i32
        ; je =>result_ready
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>started_throw
        ; jmp =>started_fatal
        ; =>started_throw
        ; mov r10, [rsp + TARGET_CELL as i32]
        ; add QWORD [r10 + CODE_ENTRY_GENERATED_THROWS_OFFSET as i32], 1
        ; jmp =>result_ready
        ; =>started_fatal
        ; mov edx, abi::NativeResultStatus::Fatal as i32
        ; jmp =>result_ready
        ; =>started_side_exit
        ; mov r10, [rsp + TARGET_CELL as i32]
        ; add QWORD [r10 + CODE_ENTRY_GENERATED_DEOPTS_OFFSET as i32], 1
        ; mov rsi, rsp
        ; mov rdi, r15
        ; mov edx, view.code_block.id as i32
        ; mov ecx, logical_pc as i32
        ; mov r8, [r10 + CODE_ENTRY_CODE_OBJECT_ID_OFFSET as i32]
        ; mov r9, [rsp + CALLER_CODE_OBJECT_ID as i32]
        ; sub rsp, 16
        ; mov QWORD [rsp], 0
        ; mov [rsp + 8], rax
    );
    runtime(ops, relocations, table, abi::STUB_JIT_DEOPT_STACK_CALL);
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; add rsp, 16
        ; =>result_ready
        ; mov r10, [rsp + CALLER_FRAME as i32]
        ; mov r11, [rsp + CALLER_CODE_OBJECT_ID as i32]
        ; mov [r15 + NATIVE_FRAME_OFFSET as i32], r10
        ; mov r8, [r15 + THREAD_OFFSET as i32]
        ; mov [r8 + VM_THREAD_CURRENT_FRAME_OFFSET as i32], r10
        ; mov [r8 + VM_THREAD_CODE_OBJECT_ID_OFFSET as i32], r11
        ; mov r8, [r15 + ACTIVATION_TOP_PTR_OFFSET as i32]
        ; mov r9, [r8]
        ; sub r9, 1
        ; mov [r8], r9
        ; mov r10, [r15 + ACTIVATION_BASE_OFFSET as i32]
        ; mov QWORD [r10 + r9 * 8], 0
        ; mov [rsp + RESULT_WORD as i32], rax
        ; mov [rsp + ENTRY_WORD as i32], rdx
        ; mov rax, [rsp + RESULT_WORD as i32]
        ; mov rdx, [rsp + ENTRY_WORD as i32]
    );
    release_dynamic(ops);
    let returned = ops.new_dynamic_label();
    let cleanup_throw = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; test rdx, rdx
        ; je =>returned
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>cleanup_throw
    );
    restore_roots(ops)?;
    dynasm!(ops ; .arch x64 ; jmp =>fatal ; =>cleanup_throw ; mov r11, rax);
    restore_roots(ops)?;
    dynasm!(ops ; .arch x64 ; mov rax, r11 ; jmp =>throw_value ; =>returned ; mov r11, rax);
    restore_roots(ops)?;
    store(ops, dst, 11, 0)?;
    dynasm!(ops ; .arch x64 ; jmp =>done);

    dynasm!(ops ; .arch x64 ; =>entry_rejected ; =>uncommitted_rejected);
    release_dynamic(ops);
    dynasm!(ops ; .arch x64 ; jmp =>caller_bail ; =>caller_bail);
    restore_roots(ops)?;
    dynasm!(ops ; .arch x64 ; jmp =>bail);

    dynasm!(ops ; .arch x64 ; =>scratch_miss ; add rsp, SCRATCH_BYTES as i32);
    restore_roots(ops)?;
    dynasm!(ops ; .arch x64 ; jmp =>bail);

    dynasm!(ops ; .arch x64 ; =>prepare_error);
    release_dynamic(ops);
    restore_roots(ops)?;
    dynasm!(ops ; .arch x64 ; jmp =>finish_error ; =>prepare_fatal);
    release_dynamic(ops);
    restore_roots(ops)?;
    dynasm!(ops ; .arch x64 ; jmp =>fatal);
    Ok(())
}
