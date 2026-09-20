//! System V x86-64 linkage for template-tier generated calls and constructors.
//!
//! # Contents
//! - Exact callable and installed-generation guards.
//! - Stack-owned callee frame and register-window construction.
//! - Exact actual-argument windows for callees that consume `arguments`.
//! - Base/super receiver allocation and constructor result selection.
//! - Fixed and compiler-collected spread argument materialization.
//! - Generated entry, stack deoptimization, and caller restoration.
//!
//! # Invariants
//! - Guard and generation rejection occur before publishing a callee frame and
//!   fall through to the canonical value-call transition exactly once.
//! - The caller remains the active VM frame until the callee frame is complete.
//! - Constructor receiver misses remain pre-effect until the rooted canonical
//!   preparation sibling commits one receiver.
//! - Actual arguments immediately follow the complete register window and are
//!   published through the shared native-frame flag/count contract.
//! - Every started generated call is removed from the activation stack before
//!   its success, throw, or fatal result leaves this linkage.
//!
//! # See also
//! - `crate::arm64::direct_call` — peer target implementation.
//! - `crate::machine::numeric::x86_64::direct_call` — allocator-owned caller
//!   linkage using the same VM frame and generation contracts.

use super::*;
use crate::{
    artifact::{
        DirectCallArgumentModeArtifact, DirectCallArtifact, DirectCallKindArtifact,
        DirectCallThisModeArtifact, DirectCallTierArtifact,
    },
    entry::{
        ACTIVATION_BASE_OFFSET, ACTIVATION_LIMIT_OFFSET, ACTIVATION_TOP_PTR_OFFSET,
        CODE_ENTRY_CODE_OBJECT_ID_OFFSET, CODE_ENTRY_GENERATED_DEOPTS_OFFSET,
        CODE_ENTRY_GENERATED_STACK_FRAME_BYTES_OFFSET, CODE_ENTRY_GENERATED_THROWS_OFFSET,
        CODE_ENTRY_NATIVE_FRAME_HEADER_OFFSET, FUNCTION_ENTRY_GENERATION_CELL_OFFSET,
        GENERATED_FEEDBACK_CLEAN_OFFSET, GLOBAL_THIS_OFFSET_PTR_OFFSET,
        NATIVE_FRAME_NEW_TARGET_OFFSET, NATIVE_FRAME_STACK_SIZE, NATIVE_FRAME_UPVALUE_BASE_OFFSET,
        NATIVE_FRAME_UPVALUE_COUNT_OFFSET, NATIVE_STACK_LIMIT_OFFSET,
        VM_THREAD_CODE_OBJECT_ID_OFFSET, VM_THREAD_CURRENT_FRAME_OFFSET,
    },
};

const MAX_DIRECT_CALL_FRAME_BYTES: u32 = 4_080;
const INCOMING_ARGUMENTS_HEADER_WORD: u32 = (abi::NativeFrameFlags::INCOMING_ARGUMENTS as u32)
    << (8
        * (std::mem::offset_of!(abi::VmFrameHeader, flags)
            - std::mem::offset_of!(abi::VmFrameHeader, register_count)));

#[derive(Debug, Clone, Copy)]
struct StackLayout {
    upvalue_base: u32,
    register_base: u32,
    incoming_base: u32,
    incoming_count: u32,
    result_word: u32,
    status_word: u32,
    caller_frame: u32,
    caller_code_object_id: u32,
    target_cell: u32,
    frame_bytes: u32,
}

impl StackLayout {
    fn for_target(target: &otter_vm::JitDirectCallee, argument_count: usize) -> Option<Self> {
        target
            .plan
            .generated_stack_frame_bytes
            .filter(|bytes| *bytes != 0)?;
        let control = NATIVE_FRAME_STACK_SIZE;
        let upvalue_base = control.checked_add(40)?;
        let upvalue_count = u32::from(target.plan.own_upvalue_count)
            .checked_add(u32::from(target.plan.inherited_upvalue_count))?;
        let register_base = upvalue_base
            .checked_add(upvalue_count.checked_mul(4)?)?
            .checked_add(7)?
            & !7;
        let incoming_count = if target.plan.needs_incoming_arguments {
            u32::try_from(argument_count).ok()?
        } else {
            0
        };
        let incoming_base =
            register_base.checked_add(u32::from(target.plan.register_count).checked_mul(8)?)?;
        let frame_bytes = incoming_base
            .checked_add(incoming_count.checked_mul(8)?)?
            .checked_add(15)?
            & !15;
        (frame_bytes <= MAX_DIRECT_CALL_FRAME_BYTES).then_some(Self {
            upvalue_base,
            register_base,
            incoming_base,
            incoming_count,
            result_word: control,
            status_word: control + 8,
            caller_frame: control + 16,
            caller_code_object_id: control + 24,
            target_cell: control + 32,
            frame_bytes,
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum CallForm<'a> {
    Plain {
        callee: u16,
    },
    CallWithThis {
        callee: u16,
        receiver: u16,
    },
    Method {
        receiver: u16,
        guard: &'a otter_vm::jit::JitMethodGuard,
    },
}

#[derive(Debug, Clone, Copy)]
enum CallArguments<'a> {
    Fixed(&'a [u16]),
    Spread(u16),
}

impl CallArguments<'_> {
    fn fixed_len(self) -> usize {
        match self {
            Self::Fixed(arguments) => arguments.len(),
            Self::Spread(_) => 0,
        }
    }

    fn artifact_mode(self) -> DirectCallArgumentModeArtifact {
        match self {
            Self::Fixed(_) => DirectCallArgumentModeArtifact::Fixed,
            Self::Spread(_) => DirectCallArgumentModeArtifact::Spread,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_plain(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &JitCompileSnapshot,
    dst: u16,
    callee: u16,
    arguments: &[u16],
    logical_pc: u32,
    byte_pc: u32,
    finish_error: DynamicLabel,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
    done: DynamicLabel,
) -> Result<bool, Unsupported> {
    let Some(candidates) = view.direct_callees.get(&byte_pc) else {
        return Ok(false);
    };
    let [target] = candidates.as_slice() else {
        return Ok(false);
    };
    emit_candidate(
        ops,
        relocations,
        transitions,
        view,
        dst,
        CallArguments::Fixed(arguments),
        logical_pc,
        byte_pc,
        target,
        0,
        1,
        CallForm::Plain { callee },
        finish_error,
        throw_value,
        fatal,
        done,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_method(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &JitCompileSnapshot,
    dst: u16,
    receiver: u16,
    arguments: &[u16],
    logical_pc: u32,
    byte_pc: u32,
    finish_error: DynamicLabel,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
    done: DynamicLabel,
) -> Result<bool, Unsupported> {
    let Some(methods) = view.direct_methods.get(&byte_pc) else {
        return Ok(false);
    };
    let mut emitted = false;
    for method in methods {
        emitted |= emit_candidate(
            ops,
            relocations,
            transitions,
            view,
            dst,
            CallArguments::Fixed(arguments),
            logical_pc,
            byte_pc,
            &method.callee,
            method.target_index,
            method.target_count,
            CallForm::Method {
                receiver,
                guard: &method.guard,
            },
            finish_error,
            throw_value,
            fatal,
            done,
        )?;
    }
    Ok(emitted)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_spread_plain(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &JitCompileSnapshot,
    dst: u16,
    callee: u16,
    receiver: u16,
    arguments: u16,
    logical_pc: u32,
    byte_pc: u32,
    finish_error: DynamicLabel,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
    done: DynamicLabel,
) -> Result<bool, Unsupported> {
    let Some(candidates) = view.direct_callees.get(&byte_pc) else {
        return Ok(false);
    };
    let [target] = candidates.as_slice() else {
        return Ok(false);
    };
    emit_candidate(
        ops,
        relocations,
        transitions,
        view,
        dst,
        CallArguments::Spread(arguments),
        logical_pc,
        byte_pc,
        target,
        0,
        1,
        CallForm::CallWithThis { callee, receiver },
        finish_error,
        throw_value,
        fatal,
        done,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_construct(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &JitCompileSnapshot,
    code_map: Option<&mut CodeMapCapture>,
    dst: u16,
    callee: u16,
    arguments: &[u16],
    logical_pc: u32,
    byte_pc: u32,
    super_construct: bool,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
    done: DynamicLabel,
) -> Result<bool, Unsupported> {
    emit_construct_with_arguments(
        ops,
        relocations,
        transitions,
        view,
        code_map,
        dst,
        callee,
        CallArguments::Fixed(arguments),
        logical_pc,
        byte_pc,
        super_construct,
        throw_value,
        fatal,
        done,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_spread_construct(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &JitCompileSnapshot,
    code_map: Option<&mut CodeMapCapture>,
    dst: u16,
    callee: u16,
    arguments: u16,
    logical_pc: u32,
    byte_pc: u32,
    super_construct: bool,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
    done: DynamicLabel,
) -> Result<bool, Unsupported> {
    emit_construct_with_arguments(
        ops,
        relocations,
        transitions,
        view,
        code_map,
        dst,
        callee,
        CallArguments::Spread(arguments),
        logical_pc,
        byte_pc,
        super_construct,
        throw_value,
        fatal,
        done,
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_construct_with_arguments(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &JitCompileSnapshot,
    mut code_map: Option<&mut CodeMapCapture>,
    dst: u16,
    callee: u16,
    arguments: CallArguments<'_>,
    logical_pc: u32,
    byte_pc: u32,
    super_construct: bool,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
    done: DynamicLabel,
) -> Result<bool, Unsupported> {
    let Some(target) = view.direct_constructs.get(&byte_pc) else {
        return Ok(false);
    };
    if target.plan.is_derived_constructor
        || target.plan.own_upvalue_count != 0
        || (matches!(arguments, CallArguments::Spread(_)) && target.plan.needs_incoming_arguments)
    {
        return Ok(false);
    }
    let Some(layout) = StackLayout::for_target(target, arguments.fixed_len()) else {
        return Ok(false);
    };
    let artifact = construct_artifact(target, layout, arguments.artifact_mode(), super_construct)?;
    let guard_fail = ops.new_dynamic_label();
    let generation_ready = ops.new_dynamic_label();
    let unpublished_fail = ops.new_dynamic_label();
    let prepare_throw = ops.new_dynamic_label();
    let prepare_fatal = ops.new_dynamic_label();
    let observable_prepare = ops.new_dynamic_label();
    let receiver_ready = ops.new_dynamic_label();
    let started_side_exit = ops.new_dynamic_label();
    let started_throw = ops.new_dynamic_label();
    let started_fatal = ops.new_dynamic_label();
    let result_ready = ops.new_dynamic_label();
    let fallback = ops.new_dynamic_label();

    dynasm!(ops
        ; .arch x64
        ; mov r10, [r15 + ACTIVATION_TOP_PTR_OFFSET as i32]
        ; mov r11, [r10]
        ; cmp r11, [r15 + ACTIVATION_LIMIT_OFFSET as i32]
        ; jae =>guard_fail
    );
    emit_load_reg(ops, 9, callee);
    emit_construct_callable_guard(ops, view, target, guard_fail);

    dynasm!(ops ; .arch x64 ; sub rsp, layout.frame_bytes as i32);
    emit_load_symbol_u64(
        ops,
        relocations,
        6,
        target.plan.entry_cell,
        RelocationTarget::DirectCallEntryCell {
            byte_pc,
            direct_call: artifact,
        },
    );
    dynasm!(ops
        ; .arch x64
        ; mov r12, [rsi + FUNCTION_ENTRY_GENERATION_CELL_OFFSET as i32]
        ; test r12, r12
        ; jnz =>generation_ready
        ; mov rdi, r15
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.entry(abi::STUB_JIT_RESOLVE_DIRECT_ENTRY),
        abi::STUB_JIT_RESOLVE_DIRECT_ENTRY,
    );
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; mov r12, rax
        ; test r12, r12
        ; jz =>unpublished_fail
        ; =>generation_ready
        ; mov [rsp + layout.target_cell as i32], r12
        ; mov eax, [r12 + CODE_ENTRY_GENERATED_STACK_FRAME_BYTES_OFFSET as i32]
        ; test eax, eax
        ; jz =>unpublished_fail
        ; mov r10, rsp
        ; sub r10, rax
        ; jc =>unpublished_fail
        ; cmp r10, [r15 + NATIVE_STACK_LIMIT_OFFSET as i32]
        ; jb =>unpublished_fail
        ; mov r11, [r12]
        ; test r11, r11
        ; jz =>unpublished_fail
        ; mov [rsp + layout.status_word as i32], r11
    );

    initialize_frame(ops, target, layout);
    let prepare_start = ops.offset().0;
    let fast_prepare_start = prepare_start;
    emit_load_reg(ops, 6, callee);
    if super_construct {
        dynasm!(ops
            ; .arch x64
            ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
            ; mov rdx, [r10 + NATIVE_FRAME_NEW_TARGET_OFFSET as i32]
        );
    } else {
        dynasm!(ops ; .arch x64 ; mov rdx, rsi);
    }
    let mut receiver_cold_start = None;
    if let Some(allocation) = target.receiver_allocation {
        let receiver_allocation_start = ops.offset().0;
        let allocation_guard_miss = ops.new_dynamic_label();
        let allocation_space_miss = ops.new_dynamic_label();
        let cold_prepare = ops.new_dynamic_label();
        crate::machine::numeric::emit_x86_64_generated_receiver_allocation(
            ops,
            relocations,
            view,
            allocation,
            allocation_guard_miss,
            allocation_space_miss,
            receiver_ready,
        );
        dynasm!(ops ; .arch x64 ; =>allocation_guard_miss);
        crate::machine::numeric::emit_x86_64_increment_runtime_counter(
            ops,
            crate::entry::RECEIVER_ALLOC_GUARD_MISSES_OFFSET,
        );
        dynasm!(ops ; .arch x64 ; jmp =>cold_prepare ; =>allocation_space_miss);
        crate::machine::numeric::emit_x86_64_increment_runtime_counter(
            ops,
            crate::entry::RECEIVER_ALLOC_SPACE_MISSES_OFFSET,
        );
        dynasm!(ops ; .arch x64 ; =>cold_prepare);
        if let Some(code_map) = code_map.as_deref_mut() {
            code_map.record(CodeRegion::call_structural(
                "directConstructReceiverAllocFast",
                receiver_allocation_start,
                ops.offset().0,
                view.code_block.id,
                logical_pc,
                byte_pc,
                artifact,
            ));
        }
        receiver_cold_start = Some(ops.offset().0);
        emit_load_reg(ops, 6, callee);
        if super_construct {
            dynasm!(ops
                ; .arch x64
                ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
                ; mov rdx, [r10 + NATIVE_FRAME_NEW_TARGET_OFFSET as i32]
            );
        } else {
            dynasm!(ops ; .arch x64 ; mov rdx, rsi);
        }
    }
    dynasm!(ops
        ; .arch x64
        ; mov rdi, r15
        ; mov ecx, target.plan.function_id as i32
        ; mov r8d, if target.receiver_allocation.is_some() { 1 } else { 0 }
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.entry(abi::STUB_JIT_TRY_PREPARE_BASE_CONSTRUCT),
        abi::STUB_JIT_TRY_PREPARE_BASE_CONSTRUCT,
    );
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; test rdx, rdx
        ; je =>receiver_ready
        ; cmp edx, abi::NativeResultStatus::SideExit as i32
        ; je =>observable_prepare
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>prepare_throw
        ; jmp =>prepare_fatal
        ; =>observable_prepare
    );
    if let (Some(code_map), Some(start)) = (code_map.as_deref_mut(), receiver_cold_start) {
        code_map.record(CodeRegion::call_structural(
            "directConstructReceiverAllocCold",
            start,
            ops.offset().0,
            view.code_block.id,
            logical_pc,
            byte_pc,
            artifact,
        ));
    }
    if let Some(code_map) = code_map.as_deref_mut() {
        code_map.record(CodeRegion::call_structural(
            "directConstructPrepareFast",
            fast_prepare_start,
            ops.offset().0,
            view.code_block.id,
            logical_pc,
            byte_pc,
            artifact,
        ));
    }
    let observable_prepare_start = ops.offset().0;
    emit_load_reg(ops, 6, callee);
    if super_construct {
        dynasm!(ops
            ; .arch x64
            ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
            ; mov rdx, [r10 + NATIVE_FRAME_NEW_TARGET_OFFSET as i32]
        );
    } else {
        dynasm!(ops ; .arch x64 ; mov rdx, rsi);
    }
    dynasm!(ops
        ; .arch x64
        ; mov rdi, r15
        ; mov ecx, target.plan.function_id as i32
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.entry(abi::STUB_JIT_PREPARE_BASE_CONSTRUCT),
        abi::STUB_JIT_PREPARE_BASE_CONSTRUCT,
    );
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; test rdx, rdx
        ; je =>receiver_ready
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>prepare_throw
        ; jmp =>prepare_fatal
        ; =>receiver_ready
        ; mov [rsp + NATIVE_FRAME_THIS_OFFSET as i32], rax
    );
    if let Some(code_map) = code_map.as_deref_mut() {
        code_map.record(CodeRegion::call_structural(
            "directConstructPrepareObservable",
            observable_prepare_start,
            ops.offset().0,
            view.code_block.id,
            logical_pc,
            byte_pc,
            artifact,
        ));
    }

    emit_load_reg(ops, 9, callee);
    if super_construct {
        dynasm!(ops
            ; .arch x64
            ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
            ; mov r14, [r10 + NATIVE_FRAME_NEW_TARGET_OFFSET as i32]
        );
    } else {
        dynasm!(ops ; .arch x64 ; mov r14, r9);
    }
    unwrap_constructor(ops, view, target, 9);
    dynasm!(ops
        ; .arch x64
        ; mov [rsp + NATIVE_FRAME_SELF_OFFSET as i32], r9
        ; mov [rsp + NATIVE_FRAME_NEW_TARGET_OFFSET as i32], r14
        ; mov DWORD [rsp + abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET as i32], arguments.fixed_len() as i32
    );
    initialize_inherited_state(ops, view, target);
    match arguments {
        CallArguments::Fixed(arguments) => {
            for (index, &argument) in arguments.iter().enumerate() {
                emit_load_reg(ops, 11, argument);
                let index = u32::try_from(index)
                    .map_err(|_| Unsupported::OperandShape("x86-64 construct argument index"))?;
                if index < u32::from(target.plan.param_count) {
                    let offset = layout.register_base + index * 8;
                    dynasm!(ops ; .arch x64 ; mov [rsp + offset as i32], r11);
                }
                if index < layout.incoming_count {
                    let offset = layout.incoming_base + index * 8;
                    dynasm!(ops ; .arch x64 ; mov [rsp + offset as i32], r11);
                }
            }
        }
        CallArguments::Spread(arguments) => {
            emit_load_reg(ops, 6, arguments);
            dynasm!(ops
                ; .arch x64
                ; mov rdi, r15
                ; mov rdx, rsp
                ; mov ecx, target.plan.param_count as i32
            );
            emit_load_runtime_stub(
                ops,
                relocations,
                transitions.entry(abi::STUB_JIT_COPY_SPREAD_ARGUMENTS),
                abi::STUB_JIT_COPY_SPREAD_ARGUMENTS,
            );
            dynasm!(ops ; .arch x64 ; call r11 ; test rax, rax ; jnz =>unpublished_fail);
        }
    }
    if let Some(code_map) = code_map {
        code_map.record(CodeRegion::call_structural(
            "directConstructPrepare",
            prepare_start,
            ops.offset().0,
            view.code_block.id,
            logical_pc,
            byte_pc,
            artifact,
        ));
    }

    dynasm!(ops
        ; .arch x64
        ; mov r12, [rsp + layout.target_cell as i32]
        ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov [rsp + layout.caller_frame as i32], r10
        ; mov r11, [r15 + THREAD_OFFSET as i32]
        ; mov rax, [r11 + VM_THREAD_CODE_OBJECT_ID_OFFSET as i32]
        ; mov [rsp + layout.caller_code_object_id as i32], rax
        ; mov r8, [r15 + ACTIVATION_TOP_PTR_OFFSET as i32]
        ; mov r9, [r8]
        ; mov r10, [r15 + ACTIVATION_BASE_OFFSET as i32]
        ; mov [r10 + r9 * 8], rsp
        ; add r9, 1
        ; mov [r8], r9
        ; mov [r15 + NATIVE_FRAME_OFFSET as i32], rsp
        ; mov rax, [r12 + CODE_ENTRY_CODE_OBJECT_ID_OFFSET as i32]
        ; mov [r11 + VM_THREAD_CURRENT_FRAME_OFFSET as i32], rsp
        ; mov [r11 + VM_THREAD_CODE_OBJECT_ID_OFFSET as i32], rax
    );
    crate::entry::x86_64_tiering::emit_entry(ops);
    dynasm!(ops
        ; .arch x64
        ; mov QWORD [r15 + GENERATED_FEEDBACK_CLEAN_OFFSET as i32], 0
        ; mov r11, [rsp + layout.status_word as i32]
        ; mov rdi, r15
        ; call r11
        ; cmp edx, abi::NativeResultStatus::SideExit as i32
        ; je =>started_side_exit
        ; cmp edx, abi::NativeResultStatus::Success as i32
        ; je >normal_return
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>started_throw
        ; jmp =>started_fatal
        ; normal_return:
    );
    select_base_construct_result(ops, view, result_ready);

    dynasm!(ops
        ; .arch x64
        ; =>started_throw
        ; mov r12, [rsp + layout.target_cell as i32]
        ; add QWORD [r12 + CODE_ENTRY_GENERATED_THROWS_OFFSET as i32], 1
        ; jmp =>result_ready
        ; =>started_fatal
        ; mov edx, abi::NativeResultStatus::Fatal as i32
        ; jmp =>result_ready
        ; =>started_side_exit
        ; mov r12, [rsp + layout.target_cell as i32]
        ; add QWORD [r12 + CODE_ENTRY_GENERATED_DEOPTS_OFFSET as i32], 1
        ; mov rsi, rsp
        ; mov rdi, r15
        ; mov edx, view.code_block.id as i32
        ; mov ecx, logical_pc as i32
        ; mov r8, [r12 + CODE_ENTRY_CODE_OBJECT_ID_OFFSET as i32]
        ; mov r9, [rsp + layout.caller_code_object_id as i32]
        ; sub rsp, 16
        ; mov QWORD [rsp], 2
        ; mov [rsp + 8], rax
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.variadic_entry(abi::STUB_JIT_DEOPT_STACK_CALL),
        abi::STUB_JIT_DEOPT_STACK_CALL,
    );
    dynasm!(ops ; .arch x64 ; call r11 ; add rsp, 16 ; =>result_ready);

    dynasm!(ops
        ; .arch x64
        ; mov r10, [rsp + layout.caller_frame as i32]
        ; mov r11, [rsp + layout.caller_code_object_id as i32]
        ; mov [r15 + NATIVE_FRAME_OFFSET as i32], r10
        ; mov r12, [r15 + THREAD_OFFSET as i32]
        ; mov [r12 + VM_THREAD_CURRENT_FRAME_OFFSET as i32], r10
        ; mov [r12 + VM_THREAD_CODE_OBJECT_ID_OFFSET as i32], r11
        ; mov r8, [r15 + ACTIVATION_TOP_PTR_OFFSET as i32]
        ; mov r9, [r8]
        ; sub r9, 1
        ; mov [r8], r9
        ; mov r10, [r15 + ACTIVATION_BASE_OFFSET as i32]
        ; mov QWORD [r10 + r9 * 8], 0
        ; mov [rsp + layout.result_word as i32], rax
        ; mov [rsp + layout.status_word as i32], rdx
        ; mov r14, [rsp + layout.caller_frame as i32]
        ; mov r13, [r14 + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32]
        ; mov rax, [rsp + layout.result_word as i32]
        ; mov rdx, [rsp + layout.status_word as i32]
        ; add rsp, layout.frame_bytes as i32
        ; test rdx, rdx
        ; je >returned
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>throw_value
        ; jmp =>fatal
        ; returned:
    );
    emit_store_reg(ops, 0, dst);
    dynasm!(ops
        ; .arch x64
        ; jmp =>done
        ; =>unpublished_fail
        ; add rsp, layout.frame_bytes as i32
        ; =>guard_fail
        ; jmp =>fallback
        ; =>fallback
        ; jmp >finished
        ; =>prepare_throw
        ; add rsp, layout.frame_bytes as i32
        ; jmp =>throw_value
        ; =>prepare_fatal
        ; add rsp, layout.frame_bytes as i32
        ; jmp =>fatal
        ; finished:
    );
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn emit_candidate(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &JitCompileSnapshot,
    dst: u16,
    arguments: CallArguments<'_>,
    logical_pc: u32,
    byte_pc: u32,
    target: &otter_vm::JitDirectCallee,
    target_index: u32,
    target_count: u32,
    form: CallForm<'_>,
    finish_error: DynamicLabel,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
    done: DynamicLabel,
) -> Result<bool, Unsupported> {
    if (matches!(form, CallForm::Plain { .. } | CallForm::CallWithThis { .. })
        && !matches!(
            target.plan.this_mode,
            otter_vm::JitDirectCallThisMode::StrictOrLexical
                | otter_vm::JitDirectCallThisMode::SloppyGlobal
        ))
    {
        return Ok(false);
    }
    if matches!(arguments, CallArguments::Spread(_)) && target.plan.needs_incoming_arguments {
        return Ok(false);
    }
    let Some(layout) = StackLayout::for_target(target, arguments.fixed_len()) else {
        return Ok(false);
    };
    let artifact = artifact(
        target,
        layout,
        form,
        arguments.artifact_mode(),
        target_index,
        target_count,
    )?;
    let guard_fail = ops.new_dynamic_label();
    let generation_ready = ops.new_dynamic_label();
    let unpublished_fail = ops.new_dynamic_label();
    let started_side_exit = ops.new_dynamic_label();
    let started_throw = ops.new_dynamic_label();
    let started_fatal = ops.new_dynamic_label();
    let result_ready = ops.new_dynamic_label();
    let fallback = ops.new_dynamic_label();
    let finished = ops.new_dynamic_label();
    let prepare_pending = ops.new_dynamic_label();
    let prepare_fatal = ops.new_dynamic_label();

    dynasm!(ops
        ; .arch x64
        ; mov r10, [r15 + ACTIVATION_TOP_PTR_OFFSET as i32]
        ; mov r11, [r10]
        ; cmp r11, [r15 + ACTIVATION_LIMIT_OFFSET as i32]
        ; jae =>guard_fail
    );
    emit_load_and_guard_callable(ops, relocations, view, target, form, guard_fail)?;

    dynasm!(ops ; .arch x64 ; sub rsp, layout.frame_bytes as i32);
    emit_load_symbol_u64(
        ops,
        relocations,
        6,
        target.plan.entry_cell,
        RelocationTarget::DirectCallEntryCell {
            byte_pc,
            direct_call: artifact,
        },
    );
    dynasm!(ops
        ; .arch x64
        ; mov r12, [rsi + FUNCTION_ENTRY_GENERATION_CELL_OFFSET as i32]
        ; test r12, r12
        ; jnz =>generation_ready
        ; mov rdi, r15
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.entry(abi::STUB_JIT_RESOLVE_DIRECT_ENTRY),
        abi::STUB_JIT_RESOLVE_DIRECT_ENTRY,
    );
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; mov r12, rax
        ; test r12, r12
        ; jz =>unpublished_fail
        ; =>generation_ready
        ; mov [rsp + layout.target_cell as i32], r12
        ; mov eax, [r12 + CODE_ENTRY_GENERATED_STACK_FRAME_BYTES_OFFSET as i32]
        ; test eax, eax
        ; jz =>unpublished_fail
        ; mov r10, rsp
        ; sub r10, rax
        ; jc =>unpublished_fail
        ; cmp r10, [r15 + NATIVE_STACK_LIMIT_OFFSET as i32]
        ; jb =>unpublished_fail
        ; mov r11, [r12]
        ; test r11, r11
        ; jz =>unpublished_fail
        ; mov [rsp + layout.status_word as i32], r11
    );

    initialize_frame(ops, target, layout);
    emit_load_and_guard_callable(ops, relocations, view, target, form, unpublished_fail)?;
    dynasm!(ops ; .arch x64 ; mov [rsp + NATIVE_FRAME_SELF_OFFSET as i32], r9);
    initialize_receiver(ops, relocations, view, target, form)?;
    emit_load_u64(ops, 11, VALUE_UNDEFINED);
    dynasm!(ops
        ; .arch x64
        ; mov [rsp + NATIVE_FRAME_NEW_TARGET_OFFSET as i32], r11
        ; mov DWORD [rsp + abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET as i32], arguments.fixed_len() as i32
    );
    initialize_inherited_state(ops, view, target);
    if target.plan.own_upvalue_count != 0 {
        dynasm!(ops
            ; .arch x64
            ; lea r10, [rsp + layout.upvalue_base as i32]
            ; mov [rsp + NATIVE_FRAME_UPVALUE_BASE_OFFSET as i32], r10
            ; mov DWORD [rsp + NATIVE_FRAME_UPVALUE_COUNT_OFFSET as i32], 0
        );
    }
    match arguments {
        CallArguments::Fixed(arguments) => {
            for (index, &argument) in arguments.iter().enumerate() {
                emit_load_reg(ops, 11, argument);
                let index = u32::try_from(index)
                    .map_err(|_| Unsupported::OperandShape("x86-64 call argument index"))?;
                if index < u32::from(target.plan.param_count) {
                    let offset = layout.register_base + index * 8;
                    dynasm!(ops ; .arch x64 ; mov [rsp + offset as i32], r11);
                }
                if index < layout.incoming_count {
                    let offset = layout.incoming_base + index * 8;
                    dynasm!(ops ; .arch x64 ; mov [rsp + offset as i32], r11);
                }
            }
        }
        CallArguments::Spread(arguments) => {
            emit_load_reg(ops, 6, arguments);
            dynasm!(ops
                ; .arch x64
                ; mov rdi, r15
                ; mov rdx, rsp
                ; mov ecx, target.plan.param_count as i32
            );
            emit_load_runtime_stub(
                ops,
                relocations,
                transitions.entry(abi::STUB_JIT_COPY_SPREAD_ARGUMENTS),
                abi::STUB_JIT_COPY_SPREAD_ARGUMENTS,
            );
            dynasm!(ops ; .arch x64 ; call r11 ; test rax, rax ; jnz =>unpublished_fail);
        }
    }
    if target.plan.own_upvalue_count != 0 {
        dynasm!(ops
            ; .arch x64
            ; mov rdi, r15
            ; mov rsi, rsp
            ; mov edx, target.plan.own_upvalue_count as i32
            ; mov ecx, target.plan.inherited_upvalue_count as i32
        );
        emit_load_runtime_stub(
            ops,
            relocations,
            transitions.entry(abi::STUB_JIT_INITIALIZE_UPVALUES),
            abi::STUB_JIT_INITIALIZE_UPVALUES,
        );
        dynasm!(ops
            ; .arch x64
            ; call r11
            ; test rdx, rdx
            ; je >captures_ready
            ; cmp edx, abi::NativeResultStatus::SideExit as i32
            ; je =>unpublished_fail
            ; cmp edx, abi::NativeResultStatus::Throw as i32
            ; je =>prepare_pending
            ; jmp =>prepare_fatal
            ; captures_ready:
        );
    }

    dynasm!(ops
        ; .arch x64
        ; mov r12, [rsp + layout.target_cell as i32]
        ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov [rsp + layout.caller_frame as i32], r10
        ; mov r11, [r15 + THREAD_OFFSET as i32]
        ; mov rax, [r11 + VM_THREAD_CODE_OBJECT_ID_OFFSET as i32]
        ; mov [rsp + layout.caller_code_object_id as i32], rax
        ; mov r8, [r15 + ACTIVATION_TOP_PTR_OFFSET as i32]
        ; mov r9, [r8]
        ; mov r10, [r15 + ACTIVATION_BASE_OFFSET as i32]
        ; mov [r10 + r9 * 8], rsp
        ; add r9, 1
        ; mov [r8], r9
        ; mov [r15 + NATIVE_FRAME_OFFSET as i32], rsp
        ; mov rax, [r12 + CODE_ENTRY_CODE_OBJECT_ID_OFFSET as i32]
        ; mov [r11 + VM_THREAD_CURRENT_FRAME_OFFSET as i32], rsp
        ; mov [r11 + VM_THREAD_CODE_OBJECT_ID_OFFSET as i32], rax
    );
    crate::entry::x86_64_tiering::emit_entry(ops);
    dynasm!(ops
        ; .arch x64
        ; mov QWORD [r15 + GENERATED_FEEDBACK_CLEAN_OFFSET as i32], 0
        ; mov r11, [rsp + layout.status_word as i32]
        ; mov rdi, r15
        ; call r11
        ; cmp edx, abi::NativeResultStatus::SideExit as i32
        ; je =>started_side_exit
        ; cmp edx, abi::NativeResultStatus::Success as i32
        ; je =>result_ready
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>started_throw
        ; jmp =>started_fatal
        ; =>started_throw
        ; mov r12, [rsp + layout.target_cell as i32]
        ; add QWORD [r12 + CODE_ENTRY_GENERATED_THROWS_OFFSET as i32], 1
        ; jmp =>result_ready
        ; =>started_fatal
        ; mov edx, abi::NativeResultStatus::Fatal as i32
        ; jmp =>result_ready
        ; =>started_side_exit
        ; mov r12, [rsp + layout.target_cell as i32]
        ; add QWORD [r12 + CODE_ENTRY_GENERATED_DEOPTS_OFFSET as i32], 1
        ; mov rsi, rsp
        ; mov rdi, r15
        ; mov edx, view.code_block.id as i32
        ; mov ecx, logical_pc as i32
        ; mov r8, [r12 + CODE_ENTRY_CODE_OBJECT_ID_OFFSET as i32]
        ; mov r9, [rsp + layout.caller_code_object_id as i32]
        ; sub rsp, 16
        ; mov QWORD [rsp], if matches!(form, CallForm::Method { .. }) { 1 } else { 0 }
        ; mov [rsp + 8], rax
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.variadic_entry(abi::STUB_JIT_DEOPT_STACK_CALL),
        abi::STUB_JIT_DEOPT_STACK_CALL,
    );
    dynasm!(ops ; .arch x64 ; call r11 ; add rsp, 16 ; =>result_ready);

    dynasm!(ops
        ; .arch x64
        ; mov r10, [rsp + layout.caller_frame as i32]
        ; mov r11, [rsp + layout.caller_code_object_id as i32]
        ; mov [r15 + NATIVE_FRAME_OFFSET as i32], r10
        ; mov r12, [r15 + THREAD_OFFSET as i32]
        ; mov [r12 + VM_THREAD_CURRENT_FRAME_OFFSET as i32], r10
        ; mov [r12 + VM_THREAD_CODE_OBJECT_ID_OFFSET as i32], r11
        ; mov r8, [r15 + ACTIVATION_TOP_PTR_OFFSET as i32]
        ; mov r9, [r8]
        ; sub r9, 1
        ; mov [r8], r9
        ; mov r10, [r15 + ACTIVATION_BASE_OFFSET as i32]
        ; mov QWORD [r10 + r9 * 8], 0
        ; mov [rsp + layout.result_word as i32], rax
        ; mov [rsp + layout.status_word as i32], rdx
        ; mov r14, [rsp + layout.caller_frame as i32]
        ; mov r13, [r14 + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32]
        ; mov rax, [rsp + layout.result_word as i32]
        ; mov rdx, [rsp + layout.status_word as i32]
        ; add rsp, layout.frame_bytes as i32
        ; test rdx, rdx
        ; je >returned
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>throw_value
        ; jmp =>fatal
        ; returned:
    );
    emit_store_reg(ops, 0, dst);
    dynasm!(ops
        ; .arch x64
        ; jmp =>done
        ; =>unpublished_fail
        ; add rsp, layout.frame_bytes as i32
        ; =>guard_fail
        ; jmp =>fallback
        ; =>fallback
        ; jmp =>finished
    );
    dynasm!(ops
        ; .arch x64
        ; =>prepare_pending
        ; add rsp, layout.frame_bytes as i32
        ; jmp =>finish_error
        ; =>prepare_fatal
        ; add rsp, layout.frame_bytes as i32
        ; jmp =>fatal
        ; =>finished
    );
    Ok(true)
}

fn initialize_frame(ops: &mut Assembler, target: &otter_vm::JitDirectCallee, layout: StackLayout) {
    dynasm!(ops
        ; .arch x64
        ; mov r12, [rsp + layout.target_cell as i32]
        ; mov rax, [r12 + CODE_ENTRY_NATIVE_FRAME_HEADER_OFFSET as i32]
        ; mov [rsp], rax
        ; mov eax, [r12 + CODE_ENTRY_NATIVE_FRAME_HEADER_OFFSET as i32 + 8]
    );
    if target.plan.needs_incoming_arguments {
        dynasm!(ops ; .arch x64 ; or eax, INCOMING_ARGUMENTS_HEADER_WORD as i32);
    }
    dynasm!(ops
        ; .arch x64
        ; mov [rsp + 8], eax
        ; lea rax, [rsp + layout.register_base as i32]
        ; mov [rsp + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32], rax
        ; mov DWORD [rsp + abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET as i32], layout.incoming_count as i32
    );
    emit_load_u64(ops, 11, VALUE_UNDEFINED);
    for index in 0..u32::from(target.plan.register_count) {
        let offset = layout.register_base + index * 8;
        dynasm!(ops ; .arch x64 ; mov [rsp + offset as i32], r11);
    }
}

fn initialize_receiver(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    target: &otter_vm::JitDirectCallee,
    form: CallForm<'_>,
) -> Result<(), Unsupported> {
    match form {
        CallForm::Method { receiver, .. } | CallForm::CallWithThis { receiver, .. } => {
            emit_load_reg(ops, 12, receiver);
            dynasm!(ops ; .arch x64 ; mov [rsp + NATIVE_FRAME_THIS_OFFSET as i32], r12);
            return Ok(());
        }
        CallForm::Plain { .. } => {}
    }
    match target.plan.this_mode {
        otter_vm::JitDirectCallThisMode::StrictOrLexical => {
            emit_load_u64(ops, 12, VALUE_UNDEFINED);
        }
        otter_vm::JitDirectCallThisMode::SloppyGlobal => {
            dynasm!(ops
                ; .arch x64
                ; mov r10, [r15 + GLOBAL_THIS_OFFSET_PTR_OFFSET as i32]
                ; test r10, r10
                ; jz >missing
                ; mov r12d, [r10]
                ; test r12d, r12d
                ; jz >missing
            );
            emit_load_symbol_u64(
                ops,
                relocations,
                11,
                view.cage_base as u64,
                RelocationTarget::GcCageBase,
            );
            let ready = ops.new_dynamic_label();
            dynasm!(ops
                ; .arch x64
                ; add r12, r11
                ; jmp =>ready
                ; missing:
            );
            emit_load_u64(ops, 12, VALUE_UNDEFINED);
            dynasm!(ops ; .arch x64 ; =>ready);
        }
        _ => {
            return Err(Unsupported::OperandShape(
                "x86-64 template direct-call receiver",
            ));
        }
    }
    dynasm!(ops ; .arch x64 ; mov [rsp + NATIVE_FRAME_THIS_OFFSET as i32], r12);
    Ok(())
}

fn emit_load_and_guard_callable(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    target: &otter_vm::JitDirectCallee,
    form: CallForm<'_>,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    match form {
        CallForm::Plain { callee } | CallForm::CallWithThis { callee, .. } => {
            emit_load_reg(ops, 9, callee);
            emit_callable_guard(ops, view, target, miss);
        }
        CallForm::Method { receiver, guard } => {
            emit_method_guard(ops, relocations, view, receiver, guard, miss)?;
            emit_callable_guard(ops, view, target, miss);
        }
    }
    Ok(())
}

fn emit_method_guard(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    receiver: u16,
    guard: &otter_vm::jit::JitMethodGuard,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    if view.cage_base == 0 {
        return Err(Unsupported::OperandShape("x86-64 method guard cage base"));
    }
    emit_load_reg(ops, 9, receiver);
    emit_load_u64(ops, 10, NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; test r9, r10
        ; jnz =>miss
        ; mov r11d, r9d
    );
    emit_load_symbol_u64(
        ops,
        relocations,
        12,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch x64
        ; add r11, r12
        ; cmp BYTE [r11], OBJECT_BODY_TYPE_TAG as i8
        ; jne =>miss
        ; cmp DWORD [r11 + view.object_shape_byte as i32], guard.recv_shape as i32
        ; jne =>miss
    );
    for &shape in &guard.proto_chain {
        dynasm!(ops
            ; .arch x64
            ; mov r10d, [r11 + view.jit_proto_byte as i32]
            ; test r10d, r10d
            ; jz =>miss
            ; add r10, r12
            ; mov r11, r10
            ; cmp BYTE [r11], OBJECT_BODY_TYPE_TAG as i8
            ; jne =>miss
            ; cmp DWORD [r11 + view.object_shape_byte as i32], shape as i32
            ; jne =>miss
        );
    }
    emit_template_slab_base(ops, view, 11, 8, miss);
    dynasm!(ops ; .arch x64 ; mov r9, [r8 + guard.method_value_byte as i32]);
    Ok(())
}

fn initialize_inherited_state(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    target: &otter_vm::JitDirectCallee,
) {
    let direct = ops.new_dynamic_label();
    let ready = ops.new_dynamic_label();
    emit_load_u64(
        ops,
        10,
        otter_vm::value::tag::box_function_id(target.plan.function_id),
    );
    dynasm!(ops
        ; .arch x64
        ; cmp r9, r10
        ; je =>direct
        ; mov r10, [r9 + view.closure_call_layout.upvalue_base_byte as i32]
        ; mov [rsp + NATIVE_FRAME_UPVALUE_BASE_OFFSET as i32], r10
        ; mov r10d, [r9 + view.closure_call_layout.upvalue_count_byte as i32]
        ; mov [rsp + NATIVE_FRAME_UPVALUE_COUNT_OFFSET as i32], r10d
        ; mov r10d, [r9 + view.closure_call_layout.eval_env_byte as i32]
        ; mov [rsp + abi::NATIVE_FRAME_EVAL_ENV_OFFSET as i32], r10d
        ; jmp =>ready
        ; =>direct
        ; mov QWORD [rsp + NATIVE_FRAME_UPVALUE_BASE_OFFSET as i32], 0
        ; mov DWORD [rsp + NATIVE_FRAME_UPVALUE_COUNT_OFFSET as i32], 0
        ; mov DWORD [rsp + abi::NATIVE_FRAME_EVAL_ENV_OFFSET as i32], 0
        ; =>ready
    );
}

fn emit_callable_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    target: &otter_vm::JitDirectCallee,
    miss: DynamicLabel,
) {
    let direct = ops.new_dynamic_label();
    emit_load_u64(
        ops,
        10,
        otter_vm::value::tag::box_function_id(target.plan.function_id),
    );
    dynasm!(ops
        ; .arch x64
        ; cmp r9, r10
        ; je =>direct
        ; test r9, r9
        ; jz =>miss
    );
    emit_load_u64(ops, 11, NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; mov r8, r9
        ; and r8, r11
        ; test r8, r8
        ; jnz =>miss
        ; cmp BYTE [r9], otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG as i8
        ; jne =>miss
        ; mov r8d, [r9 + view.closure_call_layout.flags_byte as i32]
    );
    emit_load_u64(
        ops,
        11,
        u64::from(
            view.closure_call_layout.runtime_setup_flags | view.closure_call_layout.bound_this_flag,
        ),
    );
    dynasm!(ops
        ; .arch x64
        ; test r8d, r11d
        ; jnz =>miss
        ; cmp DWORD [r9 + view.closure_call_layout.function_id_byte as i32], target.plan.function_id as i32
        ; jne =>miss
        ; cmp DWORD [r9 + view.closure_call_layout.upvalue_count_byte as i32], target.plan.inherited_upvalue_count as i32
        ; jne =>miss
        ; =>direct
    );
}

fn emit_construct_callable_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    target: &otter_vm::JitDirectCallee,
    miss: DynamicLabel,
) {
    let retry = ops.new_dynamic_label();
    let direct = ops.new_dynamic_label();
    emit_load_u64(
        ops,
        10,
        otter_vm::value::tag::box_function_id(target.plan.function_id),
    );
    dynasm!(ops
        ; .arch x64
        ; =>retry
        ; cmp r9, r10
        ; je =>direct
        ; test r9, r9
        ; jz =>miss
    );
    emit_load_u64(ops, 11, NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; mov r8, r9
        ; and r8, r11
        ; test r8, r8
        ; jnz =>miss
        ; movzx r8d, BYTE [r9]
        ; cmp r8d, view.class_constructor_layout.type_tag as i32
        ; jne >closure
        ; mov r9, [r9 + view.class_constructor_layout.callable_byte as i32]
        ; jmp =>retry
        ; closure:
        ; cmp r8d, otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG as i32
        ; jne =>miss
        ; mov r8d, [r9 + view.closure_call_layout.flags_byte as i32]
    );
    emit_load_u64(
        ops,
        11,
        u64::from(
            view.closure_call_layout.runtime_setup_flags | view.closure_call_layout.bound_this_flag,
        ),
    );
    dynasm!(ops
        ; .arch x64
        ; test r8d, r11d
        ; jnz =>miss
        ; cmp DWORD [r9 + view.closure_call_layout.function_id_byte as i32], target.plan.function_id as i32
        ; jne =>miss
        ; cmp DWORD [r9 + view.closure_call_layout.upvalue_count_byte as i32], target.plan.inherited_upvalue_count as i32
        ; jne =>miss
        ; =>direct
    );
}

fn unwrap_constructor(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    target: &otter_vm::JitDirectCallee,
    register: u8,
) {
    let ready = ops.new_dynamic_label();
    emit_load_u64(
        ops,
        10,
        otter_vm::value::tag::box_function_id(target.plan.function_id),
    );
    dynasm!(ops
        ; .arch x64
        ; cmp Rq(register), r10
        ; je =>ready
        ; movzx r10d, BYTE [Rq(register)]
        ; cmp r10d, view.class_constructor_layout.type_tag as i32
        ; jne =>ready
        ; mov Rq(register), [Rq(register) + view.class_constructor_layout.callable_byte as i32]
        ; =>ready
    );
}

fn select_base_construct_result(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    ready: DynamicLabel,
) {
    let cell = ops.new_dynamic_label();
    let object = ops.new_dynamic_label();
    let primitive = ops.new_dynamic_label();
    emit_load_u64(ops, 11, NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; mov r10, rax
        ; and r10, r11
        ; test r10, r10
        ; jz =>cell
        ; mov r10, rax
        ; shr r10, 48
        ; test r10, r10
        ; jnz =>primitive
        ; mov r10d, eax
        ; and r10d, 0xffff
        ; cmp r10d, otter_vm::value::tag::FUNCTION_ID_TAG as i32
        ; je =>object
        ; jmp =>primitive
        ; =>cell
        ; movzx r10d, BYTE [rax]
    );
    for tag in view.primitive_cell_type_tags {
        dynasm!(ops ; .arch x64 ; cmp r10d, tag as i32 ; je =>primitive);
    }
    dynasm!(ops
        ; .arch x64
        ; =>object
        ; xor edx, edx
        ; jmp =>ready
        ; =>primitive
        ; mov rax, [rsp + NATIVE_FRAME_THIS_OFFSET as i32]
        ; xor edx, edx
        ; jmp =>ready
    );
}

fn artifact(
    target: &otter_vm::JitDirectCallee,
    layout: StackLayout,
    form: CallForm<'_>,
    argument_mode: DirectCallArgumentModeArtifact,
    target_index: u32,
    target_count: u32,
) -> Result<DirectCallArtifact, Unsupported> {
    let callee_native_frame_bytes =
        target
            .plan
            .generated_stack_frame_bytes
            .ok_or(Unsupported::OperandShape(
                "x86-64 template direct target frame",
            ))?;
    Ok(DirectCallArtifact {
        call_kind: if matches!(form, CallForm::Method { .. }) {
            DirectCallKindArtifact::Method
        } else {
            DirectCallKindArtifact::Plain
        },
        argument_mode,
        target_function_id: target.plan.function_id,
        target_index,
        target_count,
        target_code_object_id: target.plan.code_object_id,
        target_tier: match target.plan.tier {
            abi::NativeFrameKind::Baseline => DirectCallTierArtifact::Template,
            abi::NativeFrameKind::Optimizing => DirectCallTierArtifact::Optimizing,
            abi::NativeFrameKind::Interpreter => {
                return Err(Unsupported::OperandShape(
                    "x86-64 template direct target tier",
                ));
            }
        },
        this_mode: if matches!(form, CallForm::Method { .. }) {
            DirectCallThisModeArtifact::MethodReceiver
        } else {
            match target.plan.this_mode {
                otter_vm::JitDirectCallThisMode::StrictOrLexical => {
                    DirectCallThisModeArtifact::StrictOrLexical
                }
                otter_vm::JitDirectCallThisMode::SloppyGlobal => {
                    DirectCallThisModeArtifact::SloppyGlobal
                }
                _ => {
                    return Err(Unsupported::OperandShape(
                        "x86-64 template direct-call artifact",
                    ));
                }
            }
        },
        callee_native_frame_bytes,
        linkage_bytes: Some(layout.frame_bytes),
        reserved_stack_bytes: Some(
            layout
                .frame_bytes
                .checked_add(callee_native_frame_bytes)
                .ok_or(Unsupported::OperandShape(
                    "x86-64 template direct stack reservation",
                ))?,
        ),
        callee_register_count: target.plan.register_count,
        own_upvalue_count: target.plan.own_upvalue_count,
        inherited_upvalue_count: target.plan.inherited_upvalue_count,
    })
}

pub(super) fn forward_artifact(
    target: &otter_vm::JitDirectCallee,
    target_index: u32,
    target_count: u32,
) -> Result<DirectCallArtifact, Unsupported> {
    let plan = &target.plan;
    let callee_native_frame_bytes = plan
        .generated_stack_frame_bytes
        .ok_or(Unsupported::OperandShape("x86-64 forward target frame"))?;
    Ok(DirectCallArtifact {
        call_kind: DirectCallKindArtifact::Plain,
        argument_mode: DirectCallArgumentModeArtifact::Forward,
        target_function_id: plan.function_id,
        target_index,
        target_count,
        target_code_object_id: plan.code_object_id,
        target_tier: match plan.tier {
            otter_vm::native_abi::NativeFrameKind::Baseline => DirectCallTierArtifact::Template,
            otter_vm::native_abi::NativeFrameKind::Optimizing => DirectCallTierArtifact::Optimizing,
            otter_vm::native_abi::NativeFrameKind::Interpreter => {
                return Err(Unsupported::OperandShape("x86-64 forward target tier"));
            }
        },
        this_mode: match plan.this_mode {
            otter_vm::JitDirectCallThisMode::StrictOrLexical => {
                DirectCallThisModeArtifact::StrictOrLexical
            }
            otter_vm::JitDirectCallThisMode::SloppyGlobal => {
                DirectCallThisModeArtifact::SloppyGlobal
            }
            _ => return Err(Unsupported::OperandShape("x86-64 forward receiver mode")),
        },
        callee_native_frame_bytes,
        linkage_bytes: None,
        reserved_stack_bytes: None,
        callee_register_count: plan.register_count,
        own_upvalue_count: plan.own_upvalue_count,
        inherited_upvalue_count: plan.inherited_upvalue_count,
    })
}

fn construct_artifact(
    target: &otter_vm::JitDirectCallee,
    layout: StackLayout,
    argument_mode: DirectCallArgumentModeArtifact,
    super_construct: bool,
) -> Result<DirectCallArtifact, Unsupported> {
    let callee_native_frame_bytes =
        target
            .plan
            .generated_stack_frame_bytes
            .ok_or(Unsupported::OperandShape(
                "x86-64 template direct construct target frame",
            ))?;
    Ok(DirectCallArtifact {
        call_kind: if super_construct {
            DirectCallKindArtifact::SuperConstruct
        } else {
            DirectCallKindArtifact::Construct
        },
        argument_mode,
        target_function_id: target.plan.function_id,
        target_index: 0,
        target_count: 1,
        target_code_object_id: target.plan.code_object_id,
        target_tier: match target.plan.tier {
            abi::NativeFrameKind::Baseline => DirectCallTierArtifact::Template,
            abi::NativeFrameKind::Optimizing => DirectCallTierArtifact::Optimizing,
            abi::NativeFrameKind::Interpreter => {
                return Err(Unsupported::OperandShape(
                    "x86-64 template direct construct target tier",
                ));
            }
        },
        this_mode: DirectCallThisModeArtifact::ConstructReceiver,
        callee_native_frame_bytes,
        linkage_bytes: Some(layout.frame_bytes),
        reserved_stack_bytes: Some(
            layout
                .frame_bytes
                .checked_add(callee_native_frame_bytes)
                .ok_or(Unsupported::OperandShape(
                    "x86-64 template direct construct stack reservation",
                ))?,
        ),
        callee_register_count: target.plan.register_count,
        own_upvalue_count: target.plan.own_upvalue_count,
        inherited_upvalue_count: target.plan.inherited_upvalue_count,
    })
}
