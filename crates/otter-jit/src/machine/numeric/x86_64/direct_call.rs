//! System V x86-64 linkage for generated calls and constructors.
//!
//! # Contents
//! - Exact callable and installed-generation guards.
//! - Caller-owned `NativeFrame`, register window, and capture spine setup.
//! - Exact actual-argument windows for callees that consume `arguments`.
//! - Runtime-selected intrinsic-apply forwarding through generated entries,
//!   with one committed value-span cold sibling.
//! - Ordinary call receiver binding plus base/super receiver preparation,
//!   derived-frame initialization, result validation, and cleanup.
//! - Fixed and compiler-collected spread argument materialization.
//!
//! # Invariants
//! - The caller's Machine roots remain published across every allocating or
//!   reentrant transition and are reloaded before control leaves the call.
//! - The callee frame is fully initialized before publication; a started call
//!   is completed or stack-deoptimized and is never replayed.
//! - Stable entry cells and runtime stubs are captured through the shared
//!   relocation schema used by the AArch64 encoder.
//! - Forwarding source admission is pre-effect; a native miss reaches one
//!   committed cold sibling, and a started generated call is never replayed.
//!
//! # See also
//! - `crate::arm64::direct_call` — peer target implementation.
//! - `otter_vm::native_abi::NativeFrame` — published frame contract.

use super::*;
#[path = "direct_call/receiver_allocation.rs"]
mod receiver_allocation;
use crate::machine::{
    CallDescriptor, DirectCallArgumentMode, DirectCallCandidate, DirectCallKind, ExceptionalEdge,
    MachineInstruction, OperandPurpose,
};
pub(crate) use receiver_allocation::{
    emit_generated_receiver_allocation, emit_increment_runtime_counter,
};
pub(super) use receiver_allocation::{
    emit_receiver_candidate_probe, emit_receiver_publication_effect,
};

const MAX_DIRECT_CALL_FRAME_BYTES: u32 = 4_080;
const INCOMING_ARGUMENTS_HEADER_WORD: u32 =
    (otter_vm::native_abi::NativeFrameFlags::INCOMING_ARGUMENTS as u32)
        << (8
            * (std::mem::offset_of!(otter_vm::native_abi::VmFrameHeader, flags)
                - std::mem::offset_of!(otter_vm::native_abi::VmFrameHeader, register_count)));

#[derive(Debug, Clone, Copy)]
struct StackLayout {
    register_base: u32,
    incoming_base: u32,
    incoming_count: u32,
    upvalue_base: u32,
    result_word: u32,
    status_word: u32,
    caller_frame: u32,
    caller_code_object_id: u32,
    target_cell: u32,
    frame_bytes: u32,
}

#[derive(Debug, Default)]
pub(super) struct DirectCallRegions {
    pub(super) method_guards: Vec<(usize, usize)>,
    pub(super) method_candidates: Vec<(usize, usize)>,
    pub(super) generic_methods: Vec<(usize, usize)>,
    pub(super) construct_prepare_fast: Vec<(usize, usize)>,
    pub(super) construct_prepare_observable: Vec<(usize, usize)>,
    pub(super) construct_receiver_alloc_fast: Vec<(usize, usize)>,
    pub(super) construct_receiver_alloc_cold: Vec<(usize, usize)>,
    pub(super) construct_result_fast: Vec<(usize, usize)>,
    pub(super) construct_result_throw: Vec<(usize, usize)>,
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
            register_base,
            incoming_base,
            incoming_count,
            upvalue_base,
            result_word: control,
            status_word: control + 8,
            caller_frame: control + 16,
            caller_code_object_id: control + 24,
            target_cell: control + 32,
            frame_bytes,
        })
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    transitions: &TransitionTable,
    _sequence: &InstructionSequence,
    frame: MachineFrameLayout,
    instruction: &MachineInstruction,
    descriptor: &CallDescriptor,
    locations: &[AllocatedLocation],
    site: &MachineSafepointSite,
    kind: DirectCallKind,
    argument_mode: DirectCallArgumentMode,
    candidates: &[DirectCallCandidate],
    caller_function_id: u32,
    logical_pc: u32,
    byte_pc: u32,
    roots_published: bool,
    deopt: DynamicLabel,
    finish_error: DynamicLabel,
    fatal: DynamicLabel,
    throw_value: DynamicLabel,
    done: DynamicLabel,
    regions: &mut DirectCallRegions,
) -> Result<(), Unsupported> {
    if kind == DirectCallKind::Forward {
        if argument_mode != DirectCallArgumentMode::Fixed
            || descriptor.exceptional == ExceptionalEdge::None
            || descriptor.arguments.len() < 3
            || descriptor.results != [MachineRepresentation::Tagged]
        {
            return Err(Unsupported::OperandShape(
                "x86-64 generic forward-call form",
            ));
        }
        let result_index = descriptor.arguments.len();
        let result = *locations
            .get(result_index)
            .ok_or(Unsupported::OperandShape("x86-64 forward-call result"))?;
        let canonical = ops.new_dynamic_label();
        save_roots(ops, frame, site)?;
        publish_roots(ops, frame, site)?;
        dynasm!(ops
            ; .arch x64
            ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
            ; mov DWORD [r10 + NATIVE_FRAME_PC_OFFSET as i32], logical_pc as i32
        );
        crate::x86_64::emit_runtime_forward(
            ops,
            relocations,
            view,
            transitions,
            [
                u16::try_from(result_index)
                    .map_err(|_| Unsupported::OperandShape("x86-64 forward result index"))?,
                0,
                1,
                2,
            ],
            logical_pc,
            byte_pc,
            None,
            canonical,
            finish_error,
            throw_value,
            fatal,
            done,
            |ops, source, target, bias| {
                let value = instruction
                    .operands
                    .get(usize::from(source))
                    .ok_or(Unsupported::OperandShape("x86-64 forward source operand"))?
                    .value;
                let root = site
                    .roots
                    .iter()
                    .find(|root| root.value == value)
                    .ok_or(Unsupported::OperandShape("x86-64 forward source root"))?;
                let offset = bias
                    .checked_add(MACHINE_ROOT_RECORD_SIZE)
                    .and_then(|offset| offset.checked_add(root_offset(frame, root.save_slot).ok()?))
                    .and_then(|offset| i32::try_from(offset).ok())
                    .ok_or(Unsupported::OperandShape("x86-64 forward source offset"))?;
                dynasm!(ops ; .arch x64 ; mov Rq(target), [rsp + offset]);
                Ok(())
            },
            |ops, destination, source, _| {
                let location = *locations
                    .get(usize::from(destination))
                    .ok_or(Unsupported::OperandShape("x86-64 forward result location"))?;
                store_integer(ops, frame, location, source)
            },
            |ops| {
                clear_roots_preserving_r11(ops);
                reload_roots_preserving_r11(ops, frame, site)
            },
            |ops, register, base| {
                let index = view
                    .code_block
                    .forwarded_argument_bindings()
                    .filter_map(|(_, storage)| match storage {
                        otter_bytecode::ArgumentBindingStorage::Register { reg } => Some(reg),
                        _ => None,
                    })
                    .position(|reg| reg == register)
                    .ok_or(Unsupported::OperandShape("x86-64 forward binding operand"))?
                    + 3;
                let value = instruction
                    .operands
                    .get(index)
                    .ok_or(Unsupported::OperandShape("x86-64 forward binding input"))?
                    .value;
                let root = site
                    .roots
                    .iter()
                    .find(|root| root.value == value)
                    .ok_or(Unsupported::OperandShape("x86-64 forward binding root"))?;
                let offset = MACHINE_ROOT_RECORD_SIZE
                    .checked_add(root_offset(frame, root.save_slot)?)
                    .and_then(|offset| i32::try_from(offset).ok())
                    .ok_or(Unsupported::OperandShape("x86-64 forward binding offset"))?;
                dynasm!(ops ; .arch x64 ; mov r11, [Rq(base) + offset]);
                Ok(())
            },
        )?;
        dynasm!(ops ; .arch x64 ; =>canonical);
        save_roots(ops, frame, site)?;
        publish_roots(ops, frame, site)?;
        return emit_generic_value_call(
            ops,
            relocations,
            transitions,
            frame,
            instruction,
            descriptor,
            result,
            site,
            kind,
            logical_pc,
            Some(deopt),
            throw_value,
            fatal,
            done,
        );
    }
    if matches!(
        kind,
        DirectCallKind::Plain | DirectCallKind::CallWithThis | DirectCallKind::Method
    ) {
        if descriptor.exceptional == ExceptionalEdge::None
            || (argument_mode == DirectCallArgumentMode::Spread && kind != DirectCallKind::Plain)
        {
            return Err(Unsupported::OperandShape("x86-64 generic direct-call form"));
        }
        let result_index = descriptor.arguments.len();
        if result_index == 0
            || instruction.operands.len() <= result_index
            || descriptor.results != [MachineRepresentation::Tagged]
        {
            return Err(Unsupported::OperandShape(
                "x86-64 generic direct-call operands",
            ));
        }
        if kind == DirectCallKind::Method && !candidates.is_empty() {
            let final_miss = ops.new_dynamic_label();
            for (index, candidate) in candidates.iter().enumerate() {
                let next = if index + 1 == candidates.len() {
                    final_miss
                } else {
                    ops.new_dynamic_label()
                };
                let start = ops.offset().0;
                emit_generated_value_call(
                    ops,
                    relocations,
                    view,
                    transitions,
                    frame,
                    instruction,
                    descriptor,
                    locations,
                    site,
                    kind,
                    argument_mode,
                    candidate,
                    caller_function_id,
                    logical_pc,
                    byte_pc,
                    index != 0,
                    Some(next),
                    finish_error,
                    fatal,
                    throw_value,
                    done,
                    regions,
                )?;
                regions.method_candidates.push((start, ops.offset().0));
                if index + 1 != candidates.len() {
                    dynasm!(ops ; .arch x64 ; =>next);
                }
            }
            dynasm!(ops ; .arch x64 ; =>final_miss);
            let start = ops.offset().0;
            emit_generic_value_call(
                ops,
                relocations,
                transitions,
                frame,
                instruction,
                descriptor,
                locations[result_index],
                site,
                kind,
                logical_pc,
                None,
                throw_value,
                fatal,
                done,
            )?;
            regions.generic_methods.push((start, ops.offset().0));
            return Ok(());
        }
        if candidates.len() == 1 {
            return emit_generated_value_call(
                ops,
                relocations,
                view,
                transitions,
                frame,
                instruction,
                descriptor,
                locations,
                site,
                kind,
                argument_mode,
                &candidates[0],
                caller_function_id,
                logical_pc,
                byte_pc,
                false,
                None,
                finish_error,
                fatal,
                throw_value,
                done,
                regions,
            );
        }
        if argument_mode == DirectCallArgumentMode::Spread {
            return Err(Unsupported::OperandShape(
                "x86-64 spread call without generated target",
            ));
        }
        save_roots(ops, frame, site)?;
        publish_roots(ops, frame, site)?;
        let start = ops.offset().0;
        emit_generic_value_call(
            ops,
            relocations,
            transitions,
            frame,
            instruction,
            descriptor,
            locations[result_index],
            site,
            kind,
            logical_pc,
            None,
            throw_value,
            fatal,
            done,
        )?;
        if kind == DirectCallKind::Method {
            regions.generic_methods.push((start, ops.offset().0));
        }
        return Ok(());
    }
    if !matches!(
        kind,
        DirectCallKind::Construct
            | DirectCallKind::DerivedConstruct
            | DirectCallKind::SuperConstruct
    ) || argument_mode != DirectCallArgumentMode::Fixed
        || descriptor.exceptional == ExceptionalEdge::None
    {
        return Err(Unsupported::OperandShape(
            "x86-64 generated direct-call form",
        ));
    }
    let result_index = descriptor.arguments.len();
    if result_index < 1 || descriptor.results != [MachineRepresentation::Tagged] {
        return Err(Unsupported::OperandShape("x86-64 direct-call operands"));
    }
    let [candidate] = candidates else {
        if kind != DirectCallKind::Construct {
            return Err(Unsupported::OperandShape(
                "x86-64 generated constructor target",
            ));
        }
        save_roots(ops, frame, site)?;
        publish_roots(ops, frame, site)?;
        return emit_generic_construct(
            ops,
            relocations,
            transitions,
            frame,
            instruction,
            descriptor,
            locations[result_index],
            site,
            logical_pc,
            throw_value,
            fatal,
            done,
        );
    };
    let target = &candidate.callee;
    let derived = kind == DirectCallKind::DerivedConstruct;
    let super_construct = kind == DirectCallKind::SuperConstruct;
    if target.plan.is_derived_constructor != derived {
        return Err(Unsupported::OperandShape("x86-64 direct constructor kind"));
    }
    let layout = StackLayout::for_target(target, result_index - 1)
        .ok_or(Unsupported::OperandShape("x86-64 direct-call frame"))?;
    let receiver_root = if derived {
        None
    } else {
        let receiver_index = instruction
            .operands
            .iter()
            .position(|operand| operand.purpose == OperandPurpose::RuntimeRoot)
            .ok_or(Unsupported::OperandShape("x86-64 construct receiver root"))?;
        let receiver_value = instruction.operands[receiver_index].value;
        Some(
            site.roots
                .iter()
                .find(|root| root.value == receiver_value)
                .ok_or(Unsupported::OperandShape(
                    "x86-64 construct receiver save home",
                ))?,
        )
    };
    let artifact = direct_call_artifact(candidate, layout, kind, DirectCallArgumentMode::Fixed)?;

    if !roots_published {
        save_roots(ops, frame, site)?;
        publish_roots(ops, frame, site)?;
    }
    dynasm!(ops
        ; .arch x64
        ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov DWORD [r10 + NATIVE_FRAME_PC_OFFSET as i32], logical_pc as i32
    );

    let guard_fail = ops.new_dynamic_label();
    let generation_ready = ops.new_dynamic_label();
    let unpublished_fail = ops.new_dynamic_label();
    let generic_construct = ops.new_dynamic_label();
    let prepare_pending = ops.new_dynamic_label();
    let prepare_throw = ops.new_dynamic_label();
    let prepare_fatal = ops.new_dynamic_label();
    let prepare_ready = ops.new_dynamic_label();
    let started_side_exit = ops.new_dynamic_label();
    let started_throw = ops.new_dynamic_label();
    let started_fatal = ops.new_dynamic_label();
    let result_ready = ops.new_dynamic_label();

    // Prove recursion publication capacity and callable identity before any
    // receiver allocation or other observable constructor work.
    dynasm!(ops
        ; .arch x64
        ; mov r10, [r15 + ACTIVATION_TOP_PTR_OFFSET as i32]
        ; mov r11, [r10]
        ; cmp r11, [r15 + ACTIVATION_LIMIT_OFFSET as i32]
        ; jae =>guard_fail
    );
    load_saved_root(ops, frame, site, instruction.operands[0].value, 9)?;
    emit_callable_guard(ops, view, target, guard_fail);

    dynasm!(ops ; .arch x64 ; sub rsp, layout.frame_bytes as i32);
    symbolic(
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
    runtime(
        ops,
        relocations,
        transitions.entry(otter_vm::native_abi::STUB_JIT_RESOLVE_DIRECT_ENTRY),
        otter_vm::native_abi::STUB_JIT_RESOLVE_DIRECT_ENTRY,
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

    if !derived {
        // Try the no-safepoint nursery allocator first. Its misses are
        // pre-effect, so rooted canonical preparation is the exact cold
        // sibling. Super calls inherit the enclosing new.target.
        let fast_prepare_start = ops.offset().0;
        load_root_with_linkage(ops, frame, site, instruction.operands[0].value, layout, 6)?;
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
        let observable_prepare = ops.new_dynamic_label();
        if let Some(allocation) = target.receiver_allocation {
            let allocation_guard_miss = ops.new_dynamic_label();
            let allocation_space_miss = ops.new_dynamic_label();
            let cold_prepare = ops.new_dynamic_label();
            let allocation_fast_start = ops.offset().0;
            emit_generated_receiver_allocation(
                ops,
                relocations,
                view,
                allocation,
                allocation_guard_miss,
                allocation_space_miss,
                prepare_ready,
            );
            regions
                .construct_receiver_alloc_fast
                .push((allocation_fast_start, ops.offset().0));
            let allocation_cold_start = ops.offset().0;
            dynasm!(ops ; .arch x64 ; =>allocation_guard_miss);
            receiver_allocation::emit_increment_runtime_counter(
                ops,
                RECEIVER_ALLOC_GUARD_MISSES_OFFSET,
            );
            dynasm!(ops ; .arch x64 ; jmp =>cold_prepare ; =>allocation_space_miss);
            receiver_allocation::emit_increment_runtime_counter(
                ops,
                RECEIVER_ALLOC_SPACE_MISSES_OFFSET,
            );
            dynasm!(ops ; .arch x64 ; =>cold_prepare);
            load_root_with_linkage(ops, frame, site, instruction.operands[0].value, layout, 6)?;
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
                ; mov r8d, 1
            );
            regions
                .construct_receiver_alloc_cold
                .push((allocation_cold_start, ops.offset().0));
        } else {
            dynasm!(ops ; .arch x64 ; xor r8d, r8d);
        }
        runtime(
            ops,
            relocations,
            transitions.entry(otter_vm::native_abi::STUB_JIT_TRY_PREPARE_BASE_CONSTRUCT),
            otter_vm::native_abi::STUB_JIT_TRY_PREPARE_BASE_CONSTRUCT,
        );
        dynasm!(ops
            ; .arch x64
            ; call r11
            ; test rdx, rdx
            ; je =>prepare_ready
            ; cmp edx, NativeResultStatus::SideExit as i32
            ; je =>observable_prepare
            ; cmp edx, NativeResultStatus::Throw as i32
            ; je =>prepare_pending
            ; jmp =>prepare_fatal
            ; =>observable_prepare
        );
        regions
            .construct_prepare_fast
            .push((fast_prepare_start, ops.offset().0));
        let observable_prepare_start = ops.offset().0;
        load_root_with_linkage(ops, frame, site, instruction.operands[0].value, layout, 6)?;
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
        runtime(
            ops,
            relocations,
            transitions.entry(otter_vm::native_abi::STUB_JIT_PREPARE_BASE_CONSTRUCT),
            otter_vm::native_abi::STUB_JIT_PREPARE_BASE_CONSTRUCT,
        );
        dynasm!(ops
            ; .arch x64
            ; call r11
            ; test rdx, rdx
            ; je =>prepare_ready
            ; cmp edx, NativeResultStatus::Throw as i32
            ; je =>prepare_throw
            ; jmp =>prepare_fatal
            ; =>prepare_ready
        );
        regions
            .construct_prepare_observable
            .push((observable_prepare_start, ops.offset().0));

        let receiver_root = receiver_root.expect("base construct receiver root");
        store_root_with_linkage(ops, frame, receiver_root.save_slot, layout, 0)?;
        dynasm!(ops ; .arch x64 ; mov [rsp + NATIVE_FRAME_THIS_OFFSET as i32], rax);
    }
    load_root_with_linkage(ops, frame, site, instruction.operands[0].value, layout, 9)?;
    dynasm!(ops ; .arch x64 ; mov r14, r9);
    unwrap_constructor(ops, view, target, 9);
    let direct_constructor = ops.new_dynamic_label();
    let constructor_state_ready = ops.new_dynamic_label();
    load64(
        ops,
        10,
        otter_vm::value::tag::box_function_id(target.plan.function_id),
    );
    dynasm!(ops
        ; .arch x64
        ; cmp r9, r10
        ; je =>direct_constructor
        ; mov r10, [r9 + view.closure_call_layout.upvalue_base_byte as i32]
        ; mov r11d, [r9 + view.closure_call_layout.upvalue_count_byte as i32]
        ; mov r8d, [r9 + view.closure_call_layout.eval_env_byte as i32]
        ; jmp =>constructor_state_ready
        ; =>direct_constructor
        ; xor r10d, r10d
        ; xor r11d, r11d
        ; xor r8d, r8d
        ; =>constructor_state_ready
        ; mov [rsp + NATIVE_FRAME_SELF_OFFSET as i32], r9
        ; mov [rsp + NATIVE_FRAME_UPVALUE_BASE_OFFSET as i32], r10
        ; mov [rsp + NATIVE_FRAME_UPVALUE_COUNT_OFFSET as i32], r11d
        ; mov [rsp + otter_vm::native_abi::NATIVE_FRAME_EVAL_ENV_OFFSET as i32], r8d
    );
    if super_construct {
        dynasm!(ops
            ; .arch x64
            ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
            ; mov r14, [r10 + NATIVE_FRAME_NEW_TARGET_OFFSET as i32]
        );
    }
    dynasm!(ops ; .arch x64 ; mov [rsp + NATIVE_FRAME_NEW_TARGET_OFFSET as i32], r14);
    if derived {
        load64(ops, 10, otter_vm::Value::hole().to_bits());
        dynasm!(ops
            ; .arch x64
            ; mov [rsp + NATIVE_FRAME_THIS_OFFSET as i32], r10
            ; or BYTE [rsp + crate::entry::NATIVE_FRAME_FLAGS_OFFSET as i32], otter_vm::native_abi::NativeFrameFlags::DERIVED_CONSTRUCTOR as i8
        );
    }
    if target.plan.own_upvalue_count != 0 {
        dynasm!(ops
            ; .arch x64
            ; lea r10, [rsp + layout.upvalue_base as i32]
            ; mov [rsp + NATIVE_FRAME_UPVALUE_BASE_OFFSET as i32], r10
            ; mov DWORD [rsp + NATIVE_FRAME_UPVALUE_COUNT_OFFSET as i32], 0
        );
    }

    copy_arguments(ops, frame, site, instruction, result_index, target, layout)?;
    if target.plan.own_upvalue_count != 0 {
        dynasm!(ops
            ; .arch x64
            ; mov rdi, r15
            ; mov rsi, rsp
            ; mov edx, target.plan.own_upvalue_count as i32
            ; mov ecx, target.plan.inherited_upvalue_count as i32
        );
        runtime(
            ops,
            relocations,
            transitions.entry(otter_vm::native_abi::STUB_JIT_INITIALIZE_UPVALUES),
            otter_vm::native_abi::STUB_JIT_INITIALIZE_UPVALUES,
        );
        dynasm!(ops
            ; .arch x64
            ; call r11
            ; test rdx, rdx
            ; je >captures_ready
            ; cmp edx, NativeResultStatus::SideExit as i32
            ; je =>unpublished_fail
            ; cmp edx, NativeResultStatus::Throw as i32
            ; je =>prepare_pending
            ; jmp =>prepare_fatal
            ; captures_ready:
        );
    }

    // Publish the initialized stack-owned callee and enter its selected
    // generation. The outer activation keeps that generation alive.
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
        ; cmp edx, NativeResultStatus::SideExit as i32
        ; je =>started_side_exit
        ; cmp edx, NativeResultStatus::Success as i32
        ; je >normal_return
        ; cmp edx, NativeResultStatus::Throw as i32
        ; je =>started_throw
        ; cmp edx, NativeResultStatus::Fatal as i32
        ; je =>started_fatal
        ; jmp =>started_fatal
        ; normal_return:
    );
    let result_fast_start = ops.offset().0;
    if derived {
        let object = ops.new_dynamic_label();
        let primitive = ops.new_dynamic_label();
        let cold = ops.new_dynamic_label();
        emit_construct_object_branch(ops, view, 0, object, primitive);
        dynasm!(ops ; .arch x64 ; =>primitive);
        load64(ops, 10, VALUE_UNDEFINED);
        dynasm!(ops
            ; .arch x64
            ; cmp rax, r10
            ; jne =>cold
            ; mov rdx, [rsp + NATIVE_FRAME_THIS_OFFSET as i32]
        );
        load64(ops, 10, otter_vm::Value::hole().to_bits());
        dynasm!(ops
            ; .arch x64
            ; cmp rdx, r10
            ; je =>cold
            ; mov rax, rdx
            ; xor edx, edx
            ; jmp =>result_ready
            ; =>object
            ; xor edx, edx
            ; jmp =>result_ready
            ; =>cold
        );
        regions
            .construct_result_fast
            .push((result_fast_start, ops.offset().0));
        let result_throw_start = ops.offset().0;
        dynasm!(ops
            ; .arch x64
            ; mov rsi, rax
            ; mov rdx, [rsp + NATIVE_FRAME_THIS_OFFSET as i32]
            ; mov rdi, r15
        );
        runtime(
            ops,
            relocations,
            transitions.entry(otter_vm::native_abi::STUB_JIT_DERIVED_CONSTRUCT_RESULT),
            otter_vm::native_abi::STUB_JIT_DERIVED_CONSTRUCT_RESULT,
        );
        dynasm!(ops
            ; .arch x64
            ; call r11
            ; test rdx, rdx
            ; je =>result_ready
            ; cmp edx, NativeResultStatus::Throw as i32
            ; je =>started_throw
            ; jmp =>started_fatal
        );
        regions
            .construct_result_throw
            .push((result_throw_start, ops.offset().0));
    } else {
        select_base_construct_result(ops, view, result_ready);
        regions
            .construct_result_fast
            .push((result_fast_start, ops.offset().0));
    }

    dynasm!(ops
        ; .arch x64
        ; =>started_throw
        ; mov r12, [rsp + layout.target_cell as i32]
        ; add QWORD [r12 + CODE_ENTRY_GENERATED_THROWS_OFFSET as i32], 1
        ; jmp =>result_ready
        ; =>started_fatal
        ; mov edx, NativeResultStatus::Fatal as i32
        ; jmp =>result_ready
        ; =>started_side_exit
        ; mov r12, [rsp + layout.target_cell as i32]
        ; add QWORD [r12 + CODE_ENTRY_GENERATED_DEOPTS_OFFSET as i32], 1
        ; mov rsi, rsp
        ; mov rdi, r15
        ; mov edx, caller_function_id as i32
        ; mov ecx, logical_pc as i32
        ; mov r8, [r12 + CODE_ENTRY_CODE_OBJECT_ID_OFFSET as i32]
        ; mov r9, [rsp + layout.caller_code_object_id as i32]
        ; sub rsp, 16
        ; mov QWORD [rsp], 2
        ; mov [rsp + 8], rax
    );
    runtime(
        ops,
        relocations,
        transitions.variadic_entry(otter_vm::native_abi::STUB_JIT_DEOPT_STACK_CALL),
        otter_vm::native_abi::STUB_JIT_DEOPT_STACK_CALL,
    );
    dynasm!(ops ; .arch x64 ; call r11 ; add rsp, 16 ; =>result_ready);

    // Restore the caller publication before making the callee frame private.
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
    );
    finish_published_pair(
        ops,
        frame,
        site,
        layout,
        locations[result_index],
        throw_value,
        fatal,
        done,
    )?;

    dynasm!(ops ; .arch x64 ; =>guard_fail);
    if derived || super_construct {
        clear_roots(ops);
        reload_roots(ops, frame, site)?;
        dynasm!(ops ; .arch x64 ; jmp =>deopt ; =>unpublished_fail ; add rsp, layout.frame_bytes as i32);
        clear_roots(ops);
        reload_roots(ops, frame, site)?;
        dynasm!(ops ; .arch x64 ; jmp =>deopt ; =>generic_construct);
    } else {
        dynasm!(ops
            ; .arch x64
            ; jmp =>generic_construct
            ; =>unpublished_fail
            ; add rsp, layout.frame_bytes as i32
            ; =>generic_construct
        );
    }
    emit_generic_construct(
        ops,
        relocations,
        transitions,
        frame,
        instruction,
        descriptor,
        locations[result_index],
        site,
        logical_pc,
        throw_value,
        fatal,
        done,
    )?;
    dynasm!(ops ; .arch x64 ; =>prepare_pending);
    release_unpublished(ops, frame, site, layout)?;
    dynasm!(ops ; .arch x64 ; jmp =>finish_error ; =>prepare_throw);
    park_unpublished_payload(ops, frame, site, layout)?;
    dynasm!(ops ; .arch x64 ; jmp =>throw_value ; =>prepare_fatal);
    release_unpublished(ops, frame, site, layout)?;
    dynasm!(ops ; .arch x64 ; jmp =>fatal);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_generic_construct(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    frame: MachineFrameLayout,
    instruction: &MachineInstruction,
    descriptor: &CallDescriptor,
    result: AllocatedLocation,
    site: &MachineSafepointSite,
    logical_pc: u32,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
    done: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_generic_value_call(
        ops,
        relocations,
        transitions,
        frame,
        instruction,
        descriptor,
        result,
        site,
        DirectCallKind::Construct,
        logical_pc,
        None,
        throw_value,
        fatal,
        done,
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_generated_value_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    transitions: &TransitionTable,
    frame: MachineFrameLayout,
    instruction: &MachineInstruction,
    descriptor: &CallDescriptor,
    locations: &[AllocatedLocation],
    site: &MachineSafepointSite,
    kind: DirectCallKind,
    argument_mode: DirectCallArgumentMode,
    candidate: &DirectCallCandidate,
    caller_function_id: u32,
    logical_pc: u32,
    byte_pc: u32,
    roots_published: bool,
    guard_miss: Option<DynamicLabel>,
    finish_error: DynamicLabel,
    fatal: DynamicLabel,
    throw_value: DynamicLabel,
    done: DynamicLabel,
    regions: &mut DirectCallRegions,
) -> Result<(), Unsupported> {
    let target = &candidate.callee;
    let result_index = descriptor.arguments.len();
    let result = *locations
        .get(result_index)
        .ok_or(Unsupported::OperandShape("x86-64 direct-call result"))?;
    let argument_start = if kind == DirectCallKind::CallWithThis {
        2
    } else {
        1
    };
    if result_index < argument_start || instruction.operands.len() <= result_index {
        return Err(Unsupported::OperandShape(
            "x86-64 generated value-call operands",
        ));
    }
    if argument_mode == DirectCallArgumentMode::Spread && target.plan.needs_incoming_arguments {
        return Err(Unsupported::OperandShape(
            "x86-64 spread call publishing incoming arguments",
        ));
    }
    let argument_count = if argument_mode == DirectCallArgumentMode::Fixed {
        result_index - argument_start
    } else {
        0
    };
    let layout = StackLayout::for_target(target, argument_count)
        .ok_or(Unsupported::OperandShape("x86-64 direct-call frame"))?;
    let artifact = direct_call_artifact(candidate, layout, kind, argument_mode)?;

    if !roots_published {
        save_roots(ops, frame, site)?;
        publish_roots(ops, frame, site)?;
    }

    let guard_fail = ops.new_dynamic_label();
    let generation_ready = ops.new_dynamic_label();
    let unpublished_fail = ops.new_dynamic_label();
    let started_side_exit = ops.new_dynamic_label();
    let started_throw = ops.new_dynamic_label();
    let started_fatal = ops.new_dynamic_label();
    let result_ready = ops.new_dynamic_label();
    let captures_pending = ops.new_dynamic_label();
    let captures_fatal = ops.new_dynamic_label();
    let deopt_call_kind = if kind == DirectCallKind::Method { 1 } else { 0 };

    dynasm!(ops
        ; .arch x64
        ; mov r10, [r15 + ACTIVATION_TOP_PTR_OFFSET as i32]
        ; mov r11, [r10]
        ; cmp r11, [r15 + ACTIVATION_LIMIT_OFFSET as i32]
        ; jae =>guard_fail
    );
    load_saved_root(ops, frame, site, instruction.operands[0].value, 9)?;
    if kind != DirectCallKind::Method {
        emit_callable_guard(ops, view, target, guard_fail);
    }

    match kind {
        DirectCallKind::Method => {
            load_saved_root(ops, frame, site, instruction.operands[0].value, 12)?;
        }
        DirectCallKind::Plain | DirectCallKind::CallWithThis => match target.plan.this_mode {
            otter_vm::JitDirectCallThisMode::StrictOrLexical => {
                if kind == DirectCallKind::CallWithThis {
                    load_saved_root(ops, frame, site, instruction.operands[1].value, 12)?;
                } else {
                    load64(ops, 12, VALUE_UNDEFINED);
                }
            }
            otter_vm::JitDirectCallThisMode::SloppyGlobal => {
                if kind == DirectCallKind::CallWithThis {
                    load_saved_root(ops, frame, site, instruction.operands[1].value, 12)?;
                    load64(ops, 10, Value::null().to_bits());
                    dynasm!(ops ; .arch x64 ; cmp r12, r10 ; je >global_this);
                    load64(ops, 10, VALUE_UNDEFINED);
                    dynasm!(ops ; .arch x64 ; cmp r12, r10 ; jne =>guard_fail ; global_this:);
                }
                dynasm!(ops
                    ; .arch x64
                    ; mov r10, [r15 + GLOBAL_THIS_OFFSET_PTR_OFFSET as i32]
                    ; test r10, r10
                    ; jz =>guard_fail
                    ; mov r12d, [r10]
                    ; test r12d, r12d
                    ; jz =>guard_fail
                );
                symbolic(
                    ops,
                    relocations,
                    11,
                    view.cage_base as u64,
                    RelocationTarget::GcCageBase,
                );
                dynasm!(ops ; .arch x64 ; add r12, r11);
            }
            _ => {
                return Err(Unsupported::OperandShape(
                    "x86-64 generated value-call receiver mode",
                ));
            }
        },
        _ => unreachable!("value-call helper receives ordinary calls"),
    }

    dynasm!(ops
        ; .arch x64
        ; sub rsp, layout.frame_bytes as i32
        ; mov [rsp + layout.result_word as i32], r12
    );
    symbolic(
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
    runtime(
        ops,
        relocations,
        transitions.entry(otter_vm::native_abi::STUB_JIT_RESOLVE_DIRECT_ENTRY),
        otter_vm::native_abi::STUB_JIT_RESOLVE_DIRECT_ENTRY,
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
    load_root_with_linkage(ops, frame, site, instruction.operands[0].value, layout, 9)?;
    if kind == DirectCallKind::Method {
        let guard = candidate.guard.as_ref().ok_or(Unsupported::OperandShape(
            "x86-64 generated method candidate guard",
        ))?;
        let guard_start = ops.offset().0;
        super::emit_inline_method_guard(ops, relocations, view, guard, true, unpublished_fail)?;
        regions.method_guards.push((guard_start, ops.offset().0));
    }
    dynasm!(ops
        ; .arch x64
        ; mov r12, [rsp + layout.result_word as i32]
        ; mov [rsp + NATIVE_FRAME_SELF_OFFSET as i32], r9
        ; mov [rsp + NATIVE_FRAME_THIS_OFFSET as i32], r12
    );
    load64(ops, 11, VALUE_UNDEFINED);
    dynasm!(ops
        ; .arch x64
        ; mov [rsp + NATIVE_FRAME_NEW_TARGET_OFFSET as i32], r11
        ; mov DWORD [rsp + otter_vm::native_abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET as i32], argument_count as i32
    );
    let direct_callable = ops.new_dynamic_label();
    let inherited_ready = ops.new_dynamic_label();
    load64(
        ops,
        10,
        otter_vm::value::tag::box_function_id(target.plan.function_id),
    );
    dynasm!(ops
        ; .arch x64
        ; cmp r9, r10
        ; je =>direct_callable
        ; mov r10, [r9 + view.closure_call_layout.upvalue_base_byte as i32]
        ; mov [rsp + NATIVE_FRAME_UPVALUE_BASE_OFFSET as i32], r10
        ; mov r10d, [r9 + view.closure_call_layout.upvalue_count_byte as i32]
        ; mov [rsp + NATIVE_FRAME_UPVALUE_COUNT_OFFSET as i32], r10d
        ; mov r10d, [r9 + view.closure_call_layout.eval_env_byte as i32]
        ; mov [rsp + otter_vm::native_abi::NATIVE_FRAME_EVAL_ENV_OFFSET as i32], r10d
        ; jmp =>inherited_ready
        ; =>direct_callable
        ; mov QWORD [rsp + NATIVE_FRAME_UPVALUE_BASE_OFFSET as i32], 0
        ; mov DWORD [rsp + NATIVE_FRAME_UPVALUE_COUNT_OFFSET as i32], 0
        ; mov DWORD [rsp + otter_vm::native_abi::NATIVE_FRAME_EVAL_ENV_OFFSET as i32], 0
        ; =>inherited_ready
    );
    if argument_mode == DirectCallArgumentMode::Fixed {
        copy_value_arguments(
            ops,
            frame,
            site,
            instruction,
            argument_start,
            result_index,
            target,
            layout,
        )?;
    } else {
        let spread = instruction
            .operands
            .get(1)
            .ok_or(Unsupported::OperandShape("x86-64 spread call operand"))?
            .value;
        load_root_with_linkage(ops, frame, site, spread, layout, 6)?;
        dynasm!(ops
            ; .arch x64
            ; mov rdi, r15
            ; mov rdx, rsp
            ; mov ecx, target.plan.param_count as i32
        );
        runtime(
            ops,
            relocations,
            transitions.entry(otter_vm::native_abi::STUB_JIT_COPY_SPREAD_ARGUMENTS),
            otter_vm::native_abi::STUB_JIT_COPY_SPREAD_ARGUMENTS,
        );
        dynasm!(ops ; .arch x64 ; call r11 ; test rax, rax ; jnz =>unpublished_fail);
    }
    if target.plan.own_upvalue_count != 0 {
        dynasm!(ops
            ; .arch x64
            ; lea r10, [rsp + layout.upvalue_base as i32]
            ; mov [rsp + NATIVE_FRAME_UPVALUE_BASE_OFFSET as i32], r10
            ; mov DWORD [rsp + NATIVE_FRAME_UPVALUE_COUNT_OFFSET as i32], 0
            ; mov rdi, r15
            ; mov rsi, rsp
            ; mov edx, target.plan.own_upvalue_count as i32
            ; mov ecx, target.plan.inherited_upvalue_count as i32
        );
        runtime(
            ops,
            relocations,
            transitions.entry(otter_vm::native_abi::STUB_JIT_INITIALIZE_UPVALUES),
            otter_vm::native_abi::STUB_JIT_INITIALIZE_UPVALUES,
        );
        dynasm!(ops
            ; .arch x64
            ; call r11
            ; test rdx, rdx
            ; je >captures_ready
            ; cmp edx, NativeResultStatus::SideExit as i32
            ; je =>unpublished_fail
            ; cmp edx, NativeResultStatus::Throw as i32
            ; je =>captures_pending
            ; jmp =>captures_fatal
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
        ; cmp edx, NativeResultStatus::SideExit as i32
        ; je =>started_side_exit
        ; cmp edx, NativeResultStatus::Success as i32
        ; je =>result_ready
        ; cmp edx, NativeResultStatus::Throw as i32
        ; je =>started_throw
        ; jmp =>started_fatal
        ; =>started_throw
        ; mov r12, [rsp + layout.target_cell as i32]
        ; add QWORD [r12 + CODE_ENTRY_GENERATED_THROWS_OFFSET as i32], 1
        ; jmp =>result_ready
        ; =>started_fatal
        ; mov edx, NativeResultStatus::Fatal as i32
        ; jmp =>result_ready
        ; =>started_side_exit
        ; mov r12, [rsp + layout.target_cell as i32]
        ; add QWORD [r12 + CODE_ENTRY_GENERATED_DEOPTS_OFFSET as i32], 1
        ; mov rsi, rsp
        ; mov rdi, r15
        ; mov edx, caller_function_id as i32
        ; mov ecx, logical_pc as i32
        ; mov r8, [r12 + CODE_ENTRY_CODE_OBJECT_ID_OFFSET as i32]
        ; mov r9, [rsp + layout.caller_code_object_id as i32]
        ; sub rsp, 16
        ; mov QWORD [rsp], deopt_call_kind
        ; mov [rsp + 8], rax
    );
    runtime(
        ops,
        relocations,
        transitions.variadic_entry(otter_vm::native_abi::STUB_JIT_DEOPT_STACK_CALL),
        otter_vm::native_abi::STUB_JIT_DEOPT_STACK_CALL,
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
    );
    finish_published_pair(ops, frame, site, layout, result, throw_value, fatal, done)?;

    dynasm!(ops ; .arch x64 ; =>unpublished_fail ; add rsp, layout.frame_bytes as i32 ; =>guard_fail);
    if let Some(guard_miss) = guard_miss {
        dynasm!(ops ; .arch x64 ; jmp =>guard_miss);
    } else {
        let start = ops.offset().0;
        emit_generic_value_call(
            ops,
            relocations,
            transitions,
            frame,
            instruction,
            descriptor,
            result,
            site,
            kind,
            logical_pc,
            None,
            throw_value,
            fatal,
            done,
        )?;
        if kind == DirectCallKind::Method {
            regions.generic_methods.push((start, ops.offset().0));
        }
    }
    dynasm!(ops ; .arch x64 ; =>captures_pending);
    release_unpublished(ops, frame, site, layout)?;
    dynasm!(ops ; .arch x64 ; jmp =>finish_error ; =>captures_fatal);
    release_unpublished(ops, frame, site, layout)?;
    dynasm!(ops ; .arch x64 ; jmp =>fatal);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_generic_value_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    frame: MachineFrameLayout,
    instruction: &MachineInstruction,
    descriptor: &CallDescriptor,
    result: AllocatedLocation,
    site: &MachineSafepointSite,
    kind: DirectCallKind,
    logical_pc: u32,
    pre_effect_bail: Option<DynamicLabel>,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
    done: DynamicLabel,
) -> Result<(), Unsupported> {
    let source_count = descriptor.arguments.len();
    let inserts_receiver = kind == DirectCallKind::Plain;
    let packet_count = source_count + usize::from(inserts_receiver);
    if source_count == 0 || packet_count > 510 || instruction.operands.len() < source_count {
        return Err(Unsupported::OperandShape(
            "x86-64 generic direct-call value span",
        ));
    }
    let packet_bytes = u32::try_from(packet_count)
        .ok()
        .and_then(|count| count.checked_mul(8))
        .and_then(|bytes| bytes.checked_add(15))
        .map(|bytes| bytes & !15)
        .ok_or(Unsupported::OperandShape(
            "x86-64 generic direct-call packet",
        ))?;
    if kind == DirectCallKind::Forward {
        let bail = pre_effect_bail.ok_or(Unsupported::OperandShape(
            "x86-64 forward-call source admission",
        ))?;
        load_saved_root(ops, frame, site, instruction.operands[0].value, 6)?;
        dynasm!(ops ; .arch x64 ; mov rdi, r15);
        let source_ready = otter_vm::native_abi::STUB_JIT_FORWARD_SOURCE_READY;
        runtime(
            ops,
            relocations,
            transitions.entry(source_ready),
            source_ready,
        );
        let admitted = ops.new_dynamic_label();
        dynasm!(ops ; .arch x64 ; call r11 ; test rax, rax ; jnz =>admitted);
        clear_roots(ops);
        reload_roots(ops, frame, site)?;
        dynasm!(ops ; .arch x64 ; jmp =>bail ; =>admitted);
    }
    dynasm!(ops ; .arch x64 ; sub rsp, packet_bytes as i32);
    for index in 0..source_count {
        let value = instruction.operands[index].value;
        let root = site
            .roots
            .iter()
            .find(|root| root.value == value)
            .ok_or(Unsupported::OperandShape("x86-64 generic direct-call root"))?;
        let source = packet_bytes
            .checked_add(MACHINE_ROOT_RECORD_SIZE)
            .and_then(|offset| offset.checked_add(root_offset(frame, root.save_slot).ok()?))
            .and_then(|offset| i32::try_from(offset).ok())
            .ok_or(Unsupported::OperandShape(
                "x86-64 generic direct-call root offset",
            ))?;
        let packet_index = if inserts_receiver && index != 0 {
            index + 1
        } else {
            index
        };
        let destination = i32::try_from(packet_index * 8)
            .map_err(|_| Unsupported::OperandShape("x86-64 direct-call packet offset"))?;
        dynasm!(ops
            ; .arch x64
            ; mov r11, [rsp + source]
            ; mov [rsp + destination], r11
        );
    }
    if inserts_receiver {
        load64(ops, 11, VALUE_UNDEFINED);
        dynasm!(ops ; .arch x64 ; mov [rsp + 8], r11);
    }
    dynasm!(ops
        ; .arch x64
        ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov DWORD [r10 + NATIVE_FRAME_PC_OFFSET as i32], logical_pc as i32
        ; mov rdi, r15
        ; lea rsi, [rsp]
        ; mov edx, packet_count as i32
    );
    let target = match kind {
        DirectCallKind::Method => otter_vm::native_abi::STUB_JIT_CALL_METHOD_VALUE,
        DirectCallKind::Plain | DirectCallKind::CallWithThis => {
            otter_vm::native_abi::STUB_JIT_CALL_WITH_THIS_VALUE
        }
        DirectCallKind::Construct => otter_vm::native_abi::STUB_JIT_CONSTRUCT_VALUE,
        DirectCallKind::Forward => otter_vm::native_abi::STUB_JIT_CALL_FORWARD_ARGUMENTS,
        DirectCallKind::DerivedConstruct
        | DirectCallKind::SuperConstruct
        | DirectCallKind::DerivedSuperConstruct => {
            return Err(Unsupported::OperandShape("x86-64 generic direct-call kind"));
        }
    };
    runtime(ops, relocations, transitions.entry(target), target);
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; add rsp, packet_bytes as i32
        ; mov [rsp + MACHINE_ROOT_RECORD_BASE_OFFSET as i32], rax
        ; mov [rsp + MACHINE_ROOT_RECORD_CODE_OBJECT_ID_OFFSET as i32], rdx
    );
    clear_roots(ops);
    reload_roots(ops, frame, site)?;
    let parked_payload = i32::try_from(MACHINE_ROOT_RECORD_BASE_OFFSET).unwrap()
        - i32::try_from(MACHINE_ROOT_RECORD_SIZE).unwrap();
    let parked_status = i32::try_from(MACHINE_ROOT_RECORD_CODE_OBJECT_ID_OFFSET).unwrap()
        - i32::try_from(MACHINE_ROOT_RECORD_SIZE).unwrap();
    dynasm!(ops
        ; .arch x64
        ; mov rax, [rsp + parked_payload]
        ; mov rdx, [rsp + parked_status]
        ; test rdx, rdx
        ; je >generic_returned
        ; cmp edx, NativeResultStatus::Throw as i32
        ; je =>throw_value
        ; jmp =>fatal
        ; generic_returned:
    );
    store_integer(ops, frame, result, 0)?;
    dynasm!(ops ; .arch x64 ; jmp =>done);
    Ok(())
}

fn direct_call_artifact(
    candidate: &DirectCallCandidate,
    layout: StackLayout,
    kind: DirectCallKind,
    argument_mode: DirectCallArgumentMode,
) -> Result<DirectCallArtifact, Unsupported> {
    let target = &candidate.callee.plan;
    let callee_native_frame_bytes = target
        .generated_stack_frame_bytes
        .ok_or(Unsupported::OperandShape("x86-64 direct target frame"))?;
    Ok(DirectCallArtifact {
        call_kind: match kind {
            DirectCallKind::Plain | DirectCallKind::CallWithThis | DirectCallKind::Forward => {
                DirectCallKindArtifact::Plain
            }
            DirectCallKind::Method => DirectCallKindArtifact::Method,
            DirectCallKind::Construct => DirectCallKindArtifact::Construct,
            DirectCallKind::DerivedConstruct => DirectCallKindArtifact::DerivedConstruct,
            DirectCallKind::SuperConstruct => DirectCallKindArtifact::SuperConstruct,
            DirectCallKind::DerivedSuperConstruct => DirectCallKindArtifact::DerivedSuperConstruct,
        },
        argument_mode: match argument_mode {
            DirectCallArgumentMode::Fixed => DirectCallArgumentModeArtifact::Fixed,
            DirectCallArgumentMode::Spread => DirectCallArgumentModeArtifact::Spread,
        },
        target_function_id: target.function_id,
        target_index: candidate.target_index,
        target_count: candidate.target_count,
        target_code_object_id: target.code_object_id,
        target_tier: match target.tier {
            otter_vm::native_abi::NativeFrameKind::Baseline => DirectCallTierArtifact::Template,
            otter_vm::native_abi::NativeFrameKind::Optimizing => DirectCallTierArtifact::Optimizing,
            otter_vm::native_abi::NativeFrameKind::Interpreter => {
                return Err(Unsupported::OperandShape("x86-64 direct target tier"));
            }
        },
        this_mode: match kind {
            DirectCallKind::Method => DirectCallThisModeArtifact::MethodReceiver,
            DirectCallKind::Construct | DirectCallKind::SuperConstruct => {
                DirectCallThisModeArtifact::ConstructReceiver
            }
            DirectCallKind::DerivedConstruct | DirectCallKind::DerivedSuperConstruct => {
                DirectCallThisModeArtifact::DerivedConstructor
            }
            DirectCallKind::Plain | DirectCallKind::CallWithThis => match target.this_mode {
                otter_vm::JitDirectCallThisMode::StrictOrLexical => {
                    DirectCallThisModeArtifact::StrictOrLexical
                }
                otter_vm::JitDirectCallThisMode::SloppyGlobal => {
                    DirectCallThisModeArtifact::SloppyGlobal
                }
                _ => {
                    return Err(Unsupported::OperandShape(
                        "x86-64 direct-call receiver artifact",
                    ));
                }
            },
            DirectCallKind::Forward => DirectCallThisModeArtifact::StrictOrLexical,
        },
        callee_native_frame_bytes,
        linkage_bytes: Some(layout.frame_bytes),
        reserved_stack_bytes: Some(
            layout
                .frame_bytes
                .checked_add(callee_native_frame_bytes)
                .ok_or(Unsupported::OperandShape("x86-64 direct stack reservation"))?,
        ),
        callee_register_count: target.register_count,
        own_upvalue_count: target.own_upvalue_count,
        inherited_upvalue_count: target.inherited_upvalue_count,
    })
}

fn emit_callable_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    target: &otter_vm::JitDirectCallee,
    miss: DynamicLabel,
) {
    let retry = ops.new_dynamic_label();
    let direct = ops.new_dynamic_label();
    load64(
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
    load64(ops, 11, NOT_CELL_MASK);
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
    load64(
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
    load64(
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
        ; mov DWORD [rsp + otter_vm::native_abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET as i32], layout.incoming_count as i32
    );
    load64(ops, 11, VALUE_UNDEFINED);
    for index in 0..u32::from(target.plan.register_count) {
        let offset = layout.register_base + index * 8;
        dynasm!(ops ; .arch x64 ; mov [rsp + offset as i32], r11);
    }
}

#[allow(clippy::too_many_arguments)]
fn copy_arguments(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
    instruction: &MachineInstruction,
    result_index: usize,
    target: &otter_vm::JitDirectCallee,
    layout: StackLayout,
) -> Result<(), Unsupported> {
    let count = result_index - 1;
    for argument in 0..count {
        let operand = &instruction.operands[argument + 1];
        load_root_with_linkage(ops, frame, site, operand.value, layout, 11)?;
        if argument < usize::from(target.plan.param_count) {
            let offset = layout.register_base + argument as u32 * 8;
            dynasm!(ops ; .arch x64 ; mov [rsp + offset as i32], r11);
        }
        if argument < layout.incoming_count as usize {
            let offset = layout.incoming_base + argument as u32 * 8;
            dynasm!(ops ; .arch x64 ; mov [rsp + offset as i32], r11);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn copy_value_arguments(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
    instruction: &MachineInstruction,
    argument_start: usize,
    result_index: usize,
    target: &otter_vm::JitDirectCallee,
    layout: StackLayout,
) -> Result<(), Unsupported> {
    let count = result_index.saturating_sub(argument_start);
    for argument in 0..count {
        let operand = &instruction.operands[argument_start + argument];
        load_root_with_linkage(ops, frame, site, operand.value, layout, 11)?;
        if argument < usize::from(target.plan.param_count) {
            let offset = layout.register_base + argument as u32 * 8;
            dynasm!(ops ; .arch x64 ; mov [rsp + offset as i32], r11);
        }
        if argument < layout.incoming_count as usize {
            let offset = layout.incoming_base + argument as u32 * 8;
            dynasm!(ops ; .arch x64 ; mov [rsp + offset as i32], r11);
        }
    }
    Ok(())
}

fn load_root_with_linkage(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
    value: crate::machine::MachineValue,
    layout: StackLayout,
    destination: u8,
) -> Result<(), Unsupported> {
    let root = site
        .roots
        .iter()
        .find(|root| root.value == value)
        .ok_or(Unsupported::OperandShape("x86-64 direct-call root"))?;
    let offset = layout
        .frame_bytes
        .checked_add(MACHINE_ROOT_RECORD_SIZE)
        .and_then(|offset| offset.checked_add(root_offset(frame, root.save_slot).ok()?))
        .and_then(|offset| i32::try_from(offset).ok())
        .ok_or(Unsupported::OperandShape("x86-64 direct-call root offset"))?;
    dynasm!(ops ; .arch x64 ; mov Rq(destination), [rsp + offset]);
    Ok(())
}

fn store_root_with_linkage(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    save_slot: u16,
    layout: StackLayout,
    source: u8,
) -> Result<(), Unsupported> {
    let offset = layout
        .frame_bytes
        .checked_add(MACHINE_ROOT_RECORD_SIZE)
        .and_then(|offset| offset.checked_add(root_offset(frame, save_slot).ok()?))
        .and_then(|offset| i32::try_from(offset).ok())
        .ok_or(Unsupported::OperandShape(
            "x86-64 direct-call receiver root offset",
        ))?;
    dynasm!(ops ; .arch x64 ; mov [rsp + offset], Rq(source));
    Ok(())
}

fn select_base_construct_result(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    ready: DynamicLabel,
) {
    let object = ops.new_dynamic_label();
    let primitive = ops.new_dynamic_label();
    emit_construct_object_branch(ops, view, 0, object, primitive);
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

fn emit_construct_object_branch(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    value: u8,
    object: DynamicLabel,
    primitive: DynamicLabel,
) {
    let cell = ops.new_dynamic_label();
    load64(ops, 11, NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; mov r10, Rq(value)
        ; and r10, r11
        ; test r10, r10
        ; jz =>cell
        ; mov r10, Rq(value)
        ; shr r10, 48
        ; test r10, r10
        ; jnz =>primitive
        ; mov r10d, Rd(value)
        ; and r10d, 0xffff
        ; cmp r10d, otter_vm::value::tag::FUNCTION_ID_TAG as i32
        ; je =>object
        ; jmp =>primitive
        ; =>cell
        ; movzx r10d, BYTE [Rq(value)]
    );
    for tag in view.primitive_cell_type_tags {
        dynasm!(ops ; .arch x64 ; cmp r10d, tag as i32 ; je =>primitive);
    }
    dynasm!(ops ; .arch x64 ; jmp =>object);
}

#[allow(clippy::too_many_arguments)]
fn finish_published_pair(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
    layout: StackLayout,
    result: AllocatedLocation,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
    done: DynamicLabel,
) -> Result<(), Unsupported> {
    let record_base = layout.frame_bytes + MACHINE_ROOT_RECORD_BASE_OFFSET;
    let record_status = layout.frame_bytes + MACHINE_ROOT_RECORD_CODE_OBJECT_ID_OFFSET;
    dynasm!(ops
        ; .arch x64
        ; mov rax, [rsp + layout.result_word as i32]
        ; mov rdx, [rsp + layout.status_word as i32]
        ; mov [rsp + record_base as i32], rax
        ; mov [rsp + record_status as i32], rdx
        ; add rsp, layout.frame_bytes as i32
    );
    clear_reload_parked_pair(ops, frame, site)?;
    let returned = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; test rdx, rdx
        ; je =>returned
        ; cmp edx, NativeResultStatus::Throw as i32
        ; je =>throw_value
        ; jmp =>fatal
        ; =>returned
    );
    store_integer(ops, frame, result, 0)?;
    dynasm!(ops ; .arch x64 ; jmp =>done);
    Ok(())
}

fn clear_reload_parked_pair(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
) -> Result<(), Unsupported> {
    dynasm!(ops
        ; .arch x64
        ; mov r11, [r15 + MACHINE_ROOTS_PTR_OFFSET as i32]
        ; mov rax, [rsp + MACHINE_ROOT_RECORD_PREVIOUS_OFFSET as i32]
        ; mov [r11], rax
        ; add rsp, MACHINE_ROOT_RECORD_SIZE as i32
    );
    reload_roots(ops, frame, site)?;
    let payload = i32::try_from(MACHINE_ROOT_RECORD_BASE_OFFSET).unwrap()
        - i32::try_from(MACHINE_ROOT_RECORD_SIZE).unwrap();
    let status = i32::try_from(MACHINE_ROOT_RECORD_CODE_OBJECT_ID_OFFSET).unwrap()
        - i32::try_from(MACHINE_ROOT_RECORD_SIZE).unwrap();
    dynasm!(ops ; .arch x64 ; mov rax, [rsp + payload] ; mov rdx, [rsp + status]);
    Ok(())
}

fn release_unpublished(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
    layout: StackLayout,
) -> Result<(), Unsupported> {
    dynasm!(ops ; .arch x64 ; add rsp, layout.frame_bytes as i32);
    clear_roots(ops);
    reload_roots(ops, frame, site)
}

fn park_unpublished_payload(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
    layout: StackLayout,
) -> Result<(), Unsupported> {
    let record_base = layout.frame_bytes + MACHINE_ROOT_RECORD_BASE_OFFSET;
    let record_status = layout.frame_bytes + MACHINE_ROOT_RECORD_CODE_OBJECT_ID_OFFSET;
    dynasm!(ops
        ; .arch x64
        ; mov [rsp + record_base as i32], rax
        ; mov [rsp + record_status as i32], rdx
        ; add rsp, layout.frame_bytes as i32
    );
    clear_reload_parked_pair(ops, frame, site)
}
