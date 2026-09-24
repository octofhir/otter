//! System V x86-64 emission for allocated scalar Machine IR.
//!
//! # Contents
//! - [`emit`] — the target encoder boundary shared by the optimizing pipeline.
//! - Allocation-driven frames, spill moves, scalar operations, control flow,
//!   cooperative polling, and exact deoptimization exits.
//! - Typed derived-constructor `this` binding and exact class-super loads.
//! - Shared pure receiver proofs for intrinsic-prototype CacheIR loads.
//!
//! # Invariants
//! - This encoder consumes the same verified Machine sequence, allocation,
//!   safepoints, and deopt table as the AArch64 encoder.
//! - `r15` retains the JIT context; `r11` and `xmm15` are reserved scratch.
//! - Every generated call observes the System V 16-byte stack alignment.

#![allow(clippy::useless_conversion)]

use std::collections::BTreeMap;

#[path = "x86_64/direct_call.rs"]
mod direct_call;
#[path = "x86_64/inline_calls.rs"]
mod inline_calls;
#[path = "x86_64/loose_equality.rs"]
mod loose_equality;
#[path = "x86_64/megamorphic_property.rs"]
mod megamorphic_property;

pub(crate) use direct_call::{emit_generated_receiver_allocation, emit_increment_runtime_counter};

use dynasmrt::{AssemblyOffset, DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_bytecode::opcode_schema::{
    BindingRead, BindingSemantics, BindingWrite, BindingWriteCheck,
};
use otter_vm::{
    JitCompileSnapshot, UPVALUE_CELL_TYPE_TAG, Value,
    closure::JS_CLOSURE_BODY_TYPE_TAG,
    deopt::DeoptRuntime,
    native_abi::{
        ExitAction, ExitReason, NativeResultDomain, NativeResultStatus, RuntimeStubDescriptor,
        RuntimeStubResultAbi, RuntimeStubSignature, STUB_JIT_BACKEDGE_POLL,
        STUB_JIT_DEOPT_WRITEBACK, STUB_JIT_FINISH_ERROR, STUB_NUMBER_POW_F64_LEAF,
        STUB_NUMBER_REM_F64_LEAF, STUB_NUMBER_TO_INT32_F64_LEAF, SideExit,
    },
};

use super::super::{
    AllocatedLocation, AllocatedSequence, AllocationEdit, AllocationPoint, CallDescriptor,
    CallTarget, DeoptId, ExceptionalEdge, InstructionSequence, MachineBindingTarget,
    MachineCallGuard, MachineFrameLayout, MachineInstructionId, MachineOpcode, MachineOsrInput,
    MachineOsrType, MachineRepresentation, MachineSafepointSite, MachineSafepointTable,
};
use crate::{
    CompiledCode, Unsupported,
    artifact::{
        DirectCallArgumentModeArtifact, DirectCallArtifact, DirectCallKindArtifact,
        DirectCallThisModeArtifact, DirectCallTierArtifact,
        relocation::{PropertySourceAccess, RelocationCapture, RelocationTarget},
    },
    entry::{
        ACTIVATION_BASE_OFFSET, ACTIVATION_LIMIT_OFFSET, ACTIVATION_TOP_PTR_OFFSET,
        ALLOC_CTX_SAFEPOINT_ID_OFFSET, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET,
        ALLOC_CTX_SPILL_SLOTS_OFFSET, ALLOC_CTX_STACK_SIZE, ALLOC_CTX_THREAD_OFFSET,
        CANONICAL_NAN_HI16, CODE_ENTRY_CODE_OBJECT_ID_OFFSET, CODE_ENTRY_GENERATED_DEOPTS_OFFSET,
        CODE_ENTRY_GENERATED_STACK_FRAME_BYTES_OFFSET, CODE_ENTRY_GENERATED_THROWS_OFFSET,
        CODE_ENTRY_NATIVE_FRAME_HEADER_OFFSET, DOUBLE_OFFSET_HI16,
        FUNCTION_ENTRY_GENERATION_CELL_OFFSET, GC_PAGE_SIZE, GENERATED_FEEDBACK_CLEAN_OFFSET,
        GLOBAL_THIS_OFFSET_PTR_OFFSET, MACHINE_ROOT_RECORD_BASE_OFFSET,
        MACHINE_ROOT_RECORD_CODE_OBJECT_ID_OFFSET, MACHINE_ROOT_RECORD_COUNT_OFFSET,
        MACHINE_ROOT_RECORD_PREVIOUS_OFFSET, MACHINE_ROOT_RECORD_SAFEPOINT_ID_OFFSET,
        MACHINE_ROOT_RECORD_SIZE, MACHINE_ROOTS_PTR_OFFSET, NATIVE_FRAME_FLAGS_OFFSET,
        NATIVE_FRAME_NEW_TARGET_OFFSET, NATIVE_FRAME_OFFSET, NATIVE_FRAME_PC_OFFSET,
        NATIVE_FRAME_REGISTER_BASE_OFFSET, NATIVE_FRAME_REGISTER_COUNT_OFFSET,
        NATIVE_FRAME_SELF_OFFSET, NATIVE_FRAME_STACK_SIZE, NATIVE_FRAME_THIS_OFFSET,
        NATIVE_FRAME_UPVALUE_BASE_OFFSET, NATIVE_FRAME_UPVALUE_COUNT_OFFSET,
        NATIVE_STACK_LIMIT_OFFSET, NEW_FROM_SPACE_KIND, NUMBER_TAG_HI16, OBJECT_BODY_TYPE_TAG,
        PAGE_ALLOCATED_BYTES_OFFSET, PAGE_BUMP_CURSOR_OFFSET, PAGE_SPACE_OFFSET,
        PropertySourceCell, RECEIVER_ALLOC_ATTEMPTS_OFFSET, RECEIVER_ALLOC_GENERATED_OFFSET,
        RECEIVER_ALLOC_GUARD_MISSES_OFFSET, RECEIVER_ALLOC_MAX_HEAP_BYTES_OFFSET,
        RECEIVER_ALLOC_PAGE_OFFSET, RECEIVER_ALLOC_SPACE_MISSES_OFFSET,
        RECEIVER_ALLOC_TRACKED_BYTES_OFFSET, RECEIVER_ALLOC_TYPE_BYTES_OFFSET,
        RECEIVER_ALLOC_TYPE_COUNT_OFFSET, RECEIVER_ALLOC_TYPE_LIVE_BYTES_OFFSET,
        RUNTIME_STATS_OFFSET, THREAD_OFFSET, TransitionTable, VALUE_UNDEFINED,
        VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET, VM_THREAD_CODE_OBJECT_ID_OFFSET,
        VM_THREAD_CURRENT_FRAME_OFFSET, VM_THREAD_GC_HEAP_OFFSET,
        VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET, VM_THREAD_INTERRUPT_CELL_OFFSET,
        VM_THREAD_MARKING_FLAG_CELL_OFFSET,
    },
};

const NUMBER_TAG: u64 = (NUMBER_TAG_HI16 as u64) << 48;
const DOUBLE_OFFSET: u64 = (DOUBLE_OFFSET_HI16 as u64) << 48;
const CANONICAL_NAN: u64 = (CANONICAL_NAN_HI16 as u64) << 48;
const NOT_CELL_MASK: u64 = otter_vm::value::tag::NOT_CELL_MASK;
const DEOPT_BANK_BYTES: i32 = 16 * 8;
const DEOPT_DUMP_BYTES: i32 = DEOPT_BANK_BYTES * 2;

pub(super) struct Emission {
    pub(super) code: CompiledCode,
    pub(super) generated_stack_frame_bytes: u32,
    pub(super) relocations: RelocationCapture,
    pub(super) osr_entries: BTreeMap<u32, usize>,
    pub(super) osr_regions: Vec<(u32, usize, usize)>,
    pub(super) structural_regions: Vec<(&'static str, Option<u32>, usize, usize)>,
}

#[derive(Clone, Copy)]
struct SavedFrame {
    rbx: bool,
    highest: Option<u8>,
}

struct OsrSite {
    instruction: MachineInstructionId,
    logical_pc: u32,
    inputs: Vec<MachineOsrInput>,
    continuation: DynamicLabel,
}

type OsrEmission = (BTreeMap<u32, usize>, Vec<(u32, usize, usize)>);

impl SavedFrame {
    fn new(allocation: &AllocatedSequence) -> Self {
        Self {
            rbx: allocation
                .used_registers()
                .any(|r| r.is_integer() && r.encoding() == 3),
            highest: allocation
                .used_registers()
                .filter(|r| r.is_integer() && (12..=14).contains(&r.encoding()))
                .map(|r| r.encoding())
                .max(),
        }
    }

    fn actual_bytes(self) -> u32 {
        16 + u32::from(self.rbx) * 8 + self.highest.map_or(0, |r| u32::from(r - 11) * 8)
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit(
    view: &JitCompileSnapshot,
    sequence: &InstructionSequence,
    allocation: &AllocatedSequence,
    frame: MachineFrameLayout,
    deopt_runtime: &DeoptRuntime,
    safepoints: &MachineSafepointTable,
    transitions: &TransitionTable,
    poll_entry: u64,
    deopt_writeback_entry: u64,
    _deopt_stack_call_entry: u64,
    _resolve_direct_entry: u64,
    _try_prepare_construct_entry: u64,
    _prepare_construct_entry: u64,
    _derived_construct_result_entry: u64,
    _copy_spread_arguments_entry: u64,
    _initialize_upvalues_entry: u64,
    string_concat_entry: u64,
    _array_construct_entry: u64,
    number_rem_entry: u64,
    number_pow_entry: u64,
    number_to_int32_entry: u64,
    strict_eq_entry: u64,
    to_boolean_entry: u64,
    _call_method_value_entry: u64,
    _call_with_this_value_entry: u64,
    _construct_value_entry: u64,
    load_ic_cells: &mut [PropertySourceCell],
    store_ic_cells: &mut [PropertySourceCell],
    vm_register_count: u16,
    capture_artifacts: bool,
) -> Result<Emission, Unsupported> {
    validate_locations(sequence, allocation)?;
    let saved = SavedFrame::new(allocation);
    if saved.actual_bytes() > frame.fixed_bytes() {
        return Err(Unsupported::OperandShape("x86-64 scalar frame layout"));
    }
    let mut ops = Assembler::new()
        .map_err(|_| Unsupported::Backend(crate::BackendFailure::AssemblerAllocation))?;
    let mut relocations = RelocationCapture::new(capture_artifacts);
    let mut structural_regions = Vec::new();
    let mut next_load_ic = 0usize;
    let mut next_store_ic = 0usize;
    let bail = ops.new_dynamic_label();
    let finish_error = ops.new_dynamic_label();
    let fatal = ops.new_dynamic_label();
    let throw_value = ops.new_dynamic_label();
    let shared_deopt = ops.new_dynamic_label();
    let deopts = deopt_runtime
        .exits
        .iter()
        .map(|_| ops.new_dynamic_label())
        .collect::<Vec<_>>();
    let blocks = sequence
        .blocks()
        .iter()
        .map(|_| ops.new_dynamic_label())
        .collect::<Vec<_>>();
    let mut osr_sites = Vec::new();
    for (index, instruction) in sequence.instructions().iter().enumerate() {
        let MachineOpcode::OsrEntry {
            logical_pc,
            ref inputs,
        } = instruction.opcode
        else {
            continue;
        };
        osr_sites.push(OsrSite {
            instruction: MachineInstructionId(index as u32),
            logical_pc,
            inputs: inputs.clone(),
            continuation: ops.new_dynamic_label(),
        });
    }
    let has_poll = sequence
        .instructions()
        .iter()
        .any(|i| i.opcode == MachineOpcode::BackedgePoll);

    prologue(&mut ops, frame, saved);
    if has_poll {
        dynasm!(ops ; .arch x64 ; mov ebp, crate::GENERATED_POLL_BATCH as i32);
    }
    for (index, instruction) in sequence.instructions().iter().enumerate() {
        let id = MachineInstructionId(index as u32);
        let block_index = block_for(sequence, id)?;
        if sequence.blocks()[block_index].first == id {
            let label = blocks[block_index];
            dynasm!(ops ; .arch x64 ; =>label);
        }
        edits(
            &mut ops,
            allocation.edits(),
            AllocationPoint::Before(id),
            frame,
        )?;
        let terminal = instruction.control != super::super::ControlFlow::None;
        if terminal {
            edits(
                &mut ops,
                allocation.edits(),
                AllocationPoint::After(id),
                frame,
            )?;
        }
        let loc = allocation
            .instruction_locations(id)
            .ok_or(Unsupported::OperandShape("scalar allocation coverage"))?;
        match instruction.opcode {
            MachineOpcode::OsrEntry { .. } => {
                let continuation = osr_sites
                    .iter()
                    .find(|site| site.instruction == id)
                    .ok_or(Unsupported::OperandShape("x86-64 OSR continuation"))?
                    .continuation;
                dynasm!(ops ; .arch x64 ; =>continuation);
            }
            MachineOpcode::LoopPreheader => {}
            MachineOpcode::EntryValue(parameter) => {
                let dst = ireg(loc[0])?;
                let offset = i32::from(parameter) * 8;
                dynasm!(ops
                    ; .arch x64
                    ; mov r11, [r15 + NATIVE_FRAME_OFFSET as i32]
                    ; mov r11, [r11 + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32]
                    ; mov Rq(dst), [r11 + offset]
                );
            }
            MachineOpcode::EntryThis => {
                let dst = ireg(loc[0])?;
                dynasm!(ops
                    ; .arch x64
                    ; mov r11, [r15 + NATIVE_FRAME_OFFSET as i32]
                    ; mov Rq(dst), [r11 + NATIVE_FRAME_THIS_OFFSET as i32]
                );
            }
            MachineOpcode::TryBindDerivedThis { byte_pc } => {
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                let flags = otter_vm::native_abi::NativeFrameFlags::STACK_REGISTERS
                    | otter_vm::native_abi::NativeFrameFlags::DERIVED_CONSTRUCTOR;
                load_integer(&mut ops, frame, loc[0], 6)?;
                dynasm!(ops
                    ; .arch x64
                    ; mov r11, [r15 + NATIVE_FRAME_OFFSET as i32]
                    ; movzx r10d, BYTE [r11 + NATIVE_FRAME_FLAGS_OFFSET as i32]
                    ; and r10d, flags as i32
                    ; cmp r10d, flags as i32
                    ; jne =>miss
                    ; mov r10, [r11 + NATIVE_FRAME_THIS_OFFSET as i32]
                );
                load64(&mut ops, 8, Value::hole().to_bits());
                dynasm!(ops
                    ; .arch x64
                    ; cmp r10, r8
                    ; jne =>miss
                    ; mov [r11 + NATIVE_FRAME_THIS_OFFSET as i32], rsi
                    ; mov eax, 1
                    ; jmp =>done
                    ; =>miss
                    ; xor eax, eax
                    ; =>done
                );
                store_integer(&mut ops, frame, loc[1], 0)?;
                structural_regions.push((
                    "machineDerivedThisBindFast",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::StringConstantCellLoad { byte_pc, target } => {
                let start = ops.offset().0;
                symbolic(
                    &mut ops,
                    &mut relocations,
                    11,
                    target.cell_addr as u64,
                    RelocationTarget::StringConstantCell {
                        function_id: view.code_block.id,
                        byte_pc,
                    },
                );
                dynasm!(ops ; .arch x64 ; mov r11, [r11]);
                store_integer(&mut ops, frame, loc[0], 11)?;
                structural_regions.push((
                    "machineStringConstantLoad",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::BindingGuard {
                byte_pc,
                semantics,
                target,
            } => {
                let start = ops.offset().0;
                binding_guard(
                    &mut ops,
                    &mut relocations,
                    view,
                    frame,
                    semantics,
                    target,
                    byte_pc,
                    loc,
                )?;
                structural_regions.push((
                    "machineBindingGuard",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::BindingHit {
                byte_pc,
                semantics,
                target,
            } => {
                let start = ops.offset().0;
                binding_hit(&mut ops, frame, semantics, target, loc)?;
                structural_regions.push((
                    "machineBindingHit",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::BindingWriteBarrier => {
                if loc.len() != 2 {
                    return Err(Unsupported::OperandShape("x86-64 binding write barrier"));
                }
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[1], 2)?;
                load64(&mut ops, 11, NOT_CELL_MASK);
                dynasm!(ops ; .arch x64 ; test rdx, r11 ; jnz =>done);
                load_integer(&mut ops, frame, loc[0], 6)?;
                dynasm!(ops
                    ; .arch x64
                    ; mov rdi, [r15 + THREAD_OFFSET as i32]
                    ; mov rdi, [rdi + VM_THREAD_GC_HEAP_OFFSET as i32]
                );
                runtime(
                    &mut ops,
                    &mut relocations,
                    otter_vm::runtime_stubs::WRITE_BARRIER_MUTATING.entry_addr() as u64,
                    otter_vm::native_abi::STUB_WRITE_BARRIER,
                );
                dynasm!(ops ; .arch x64 ; call r11 ; =>done);
            }
            MachineOpcode::BindingJoin { byte_pc, .. } => {
                let offset = ops.offset().0;
                structural_regions.push(("machineBindingJoin", Some(byte_pc), offset, offset));
            }
            MachineOpcode::GuardCallTarget {
                guard: MachineCallGuard::Method(ref guard),
            } => {
                let start = ops.offset().0;
                let miss = deopt(instruction.deopt_id(), &deopts)?;
                load_integer(&mut ops, frame, loc[0], 9)?;
                emit_inline_method_guard(&mut ops, &mut relocations, view, guard, false, miss)?;
                store_integer(&mut ops, frame, loc[1], 9)?;
                structural_regions.push(("machineCallTargetGuard", None, start, ops.offset().0));
            }
            MachineOpcode::GuardCallTarget {
                guard:
                    MachineCallGuard::Plain {
                        function_id,
                        this_mode,
                    },
            } => {
                let start = ops.offset().0;
                let miss = deopt(instruction.deopt_id(), &deopts)?;
                load_integer(&mut ops, frame, loc[0], 9)?;
                emit_inline_identity(&mut ops, view, function_id, miss);
                emit_inline_this(&mut ops, &mut relocations, view, this_mode, miss)?;
                store_integer(&mut ops, frame, loc[1], 9)?;
                structural_regions.push(("machineCallTargetGuard", None, start, ops.offset().0));
            }
            MachineOpcode::GuardCallTarget {
                guard: MachineCallGuard::Construct { function_id },
            } => {
                let start = ops.offset().0;
                let miss = deopt(instruction.deopt_id(), &deopts)?;
                let callable = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[0], 9)?;
                load64(&mut ops, 11, NOT_CELL_MASK);
                dynasm!(ops
                    ; .arch x64
                    ; mov r10, r9
                    ; and r10, r11
                    ; test r10, r10
                    ; jnz =>callable
                    ; test r9, r9
                    ; jz =>callable
                    ; cmp BYTE [r9], view.class_constructor_layout.type_tag as i8
                    ; jne =>callable
                    ; mov r9, [r9 + view.class_constructor_layout.callable_byte as i32]
                    ; =>callable
                );
                emit_inline_identity(&mut ops, view, function_id, miss);
                store_integer(&mut ops, frame, loc[1], 9)?;
                structural_regions.push(("machineCallTargetGuard", None, start, ops.offset().0));
            }
            MachineOpcode::ResolveCallThis { this_mode } => {
                let impossible = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[0], 9)?;
                emit_inline_this(&mut ops, &mut relocations, view, this_mode, impossible)?;
                store_integer(&mut ops, frame, loc[1], 0)?;
                dynasm!(ops ; .arch x64 ; jmp =>done ; =>impossible ; jmp =>fatal ; =>done);
            }
            MachineOpcode::AllocateObject { plan, byte_pc } => {
                let start = ops.offset().0;
                load_integer(&mut ops, frame, loc[0], 2)?;
                direct_call::emit_receiver_candidate_probe(&mut ops, &mut relocations, view, plan);
                store_integer(&mut ops, frame, loc[1], 0)?;
                store_integer(&mut ops, frame, loc[2], 1)?;
                structural_regions.push((
                    "machineAllocateObject",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::AllocationHit => {
                load_integer(&mut ops, frame, loc[0], 9)?;
                load64(&mut ops, 10, VALUE_UNDEFINED);
                dynasm!(ops ; .arch x64 ; cmp r9, r10);
                bool_from_flags(&mut ops, ireg(loc[1])?, Cond::Ne);
            }
            MachineOpcode::PublishObject { byte_pc } => {
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                let hit = ireg(loc[3])?;
                dynasm!(ops ; .arch x64 ; test Rd(hit), Rd(hit) ; jz =>miss);
                load_integer(&mut ops, frame, loc[0], 2)?;
                load_integer(&mut ops, frame, loc[1], 0)?;
                load_integer(&mut ops, frame, loc[2], 1)?;
                direct_call::emit_receiver_publication_effect(&mut ops, view);
                dynasm!(ops ; .arch x64 ; jmp =>done ; =>miss);
                load64(&mut ops, 0, VALUE_UNDEFINED);
                dynasm!(ops ; .arch x64 ; =>done);
                store_integer(&mut ops, frame, loc[4], 0)?;
                structural_regions.push((
                    "machinePublishObject",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::BaseConstructResult => {
                load_integer(&mut ops, frame, loc[1], 9)?;
                load_integer(&mut ops, frame, loc[0], 0)?;
                select_base_construct_value(&mut ops, &mut relocations, view, 9);
                store_integer(&mut ops, frame, loc[2], 0)?;
            }
            MachineOpcode::TaggedConstant(bits) => load64(&mut ops, ireg(loc[0])?, bits),
            MachineOpcode::IntegerConstant(value) => {
                load64(&mut ops, ireg(loc[0])?, value as u32 as u64);
            }
            MachineOpcode::FloatConstant(bits) => {
                load64(&mut ops, 11, bits);
                let dst = freg(loc[0])?;
                dynasm!(ops ; .arch x64 ; movq Rx(dst), r11);
            }
            MachineOpcode::DecodeNumber => {
                let exit = maybe_deopt(instruction.deopt_id(), &deopts)?.unwrap_or(bail);
                decode_number(&mut ops, ireg(loc[0])?, freg(loc[1])?, exit);
            }
            MachineOpcode::DecodeInt32 => {
                let src = ireg(loc[0])?;
                if src != ireg(loc[1])? {
                    return Err(Unsupported::OperandShape("x86-64 Int32 decode reuse"));
                }
                let exit = maybe_deopt(instruction.deopt_id(), &deopts)?.unwrap_or(bail);
                guard_int32(&mut ops, src, exit);
            }
            MachineOpcode::Int32ToFloat64 => {
                let (src, dst) = (ireg(loc[0])?, freg(loc[1])?);
                dynasm!(ops ; .arch x64 ; cvtsi2sd Rx(dst), Rd(src));
            }
            MachineOpcode::Uint32ToFloat64 => {
                let (src, dst) = (ireg(loc[0])?, freg(loc[1])?);
                dynasm!(ops ; .arch x64 ; mov r11d, Rd(src) ; cvtsi2sd Rx(dst), r11);
            }
            MachineOpcode::BooleanToInt32 => {
                let (src, dst) = (ireg(loc[0])?, ireg(loc[1])?);
                dynasm!(ops ; .arch x64 ; mov Rd(dst), Rd(src));
            }
            MachineOpcode::Float64ToInt32 => {
                if freg(loc[0])? != 0 || ireg(loc[1])? != 0 {
                    return Err(Unsupported::OperandShape("x86-64 ToInt32 leaf ABI"));
                }
                runtime(
                    &mut ops,
                    &mut relocations,
                    number_to_int32_entry,
                    STUB_NUMBER_TO_INT32_F64_LEAF,
                );
                dynasm!(ops ; .arch x64 ; call r11);
            }
            MachineOpcode::FloatAdd
            | MachineOpcode::FloatSub
            | MachineOpcode::FloatMul
            | MachineOpcode::FloatDiv => {
                let (left, right, dst) = (freg(loc[0])?, freg(loc[1])?, freg(loc[2])?);
                dynasm!(ops ; .arch x64 ; movsd xmm15, Rx(left));
                match instruction.opcode {
                    MachineOpcode::FloatAdd => dynasm!(ops ; .arch x64 ; addsd xmm15, Rx(right)),
                    MachineOpcode::FloatSub => dynasm!(ops ; .arch x64 ; subsd xmm15, Rx(right)),
                    MachineOpcode::FloatMul => dynasm!(ops ; .arch x64 ; mulsd xmm15, Rx(right)),
                    MachineOpcode::FloatDiv => dynasm!(ops ; .arch x64 ; divsd xmm15, Rx(right)),
                    _ => unreachable!(),
                }
                dynasm!(ops ; .arch x64 ; movsd Rx(dst), xmm15);
            }
            MachineOpcode::FloatRem | MachineOpcode::FloatPow => {
                if freg(loc[0])? != 0 || freg(loc[1])? != 1 || freg(loc[2])? != 0 {
                    return Err(Unsupported::OperandShape("x86-64 FP leaf ABI"));
                }
                let (entry, stub) = if instruction.opcode == MachineOpcode::FloatRem {
                    (number_rem_entry, STUB_NUMBER_REM_F64_LEAF)
                } else {
                    (number_pow_entry, STUB_NUMBER_POW_F64_LEAF)
                };
                runtime(&mut ops, &mut relocations, entry, stub);
                dynasm!(ops ; .arch x64 ; call r11);
            }
            MachineOpcode::FloatNeg => {
                let (src, dst) = (freg(loc[0])?, freg(loc[1])?);
                load64(&mut ops, 11, 1_u64 << 63);
                dynasm!(ops ; .arch x64 ; movq xmm15, r11 ; xorpd xmm15, Rx(src) ; movsd Rx(dst), xmm15);
            }
            MachineOpcode::IntegerAdd | MachineOpcode::IntegerSub => {
                let (left, right, dst) = (ireg(loc[0])?, ireg(loc[1])?, ireg(loc[2])?);
                let exit = deopt(instruction.deopt_id(), &deopts)?;
                dynasm!(ops ; .arch x64 ; mov r11d, Rd(left));
                if instruction.opcode == MachineOpcode::IntegerAdd {
                    dynasm!(ops ; .arch x64 ; add r11d, Rd(right));
                } else {
                    dynasm!(ops ; .arch x64 ; sub r11d, Rd(right));
                }
                dynasm!(ops ; .arch x64 ; jo =>exit ; mov Rd(dst), r11d);
            }
            MachineOpcode::IntegerAddImmediate(imm) | MachineOpcode::IntegerSubImmediate(imm) => {
                let (src, dst) = (ireg(loc[0])?, ireg(loc[1])?);
                let exit = deopt(instruction.deopt_id(), &deopts)?;
                dynasm!(ops ; .arch x64 ; mov r11d, Rd(src));
                if matches!(instruction.opcode, MachineOpcode::IntegerAddImmediate(_)) {
                    dynasm!(ops ; .arch x64 ; add r11d, imm);
                } else {
                    dynasm!(ops ; .arch x64 ; sub r11d, imm);
                }
                dynasm!(ops ; .arch x64 ; jo =>exit ; mov Rd(dst), r11d);
            }
            MachineOpcode::IntegerMul => emit_mul(&mut ops, loc, instruction, &deopts)?,
            MachineOpcode::IntegerNeg => emit_neg(&mut ops, loc, instruction, &deopts)?,
            MachineOpcode::IntegerAnd
            | MachineOpcode::IntegerOr
            | MachineOpcode::IntegerXor
            | MachineOpcode::IntegerShiftLeft
            | MachineOpcode::IntegerShiftRight
            | MachineOpcode::IntegerShiftRightLogical => {
                integer_binary(&mut ops, &instruction.opcode, loc)?
            }
            MachineOpcode::IntegerNot => {
                let (src, dst) = (ireg(loc[0])?, ireg(loc[1])?);
                dynasm!(ops ; .arch x64 ; mov r11d, Rd(src) ; not r11d ; mov Rd(dst), r11d);
            }
            MachineOpcode::IntegerAndImmediate(imm) => {
                let (src, dst) = (ireg(loc[0])?, ireg(loc[1])?);
                dynasm!(ops ; .arch x64 ; mov r11d, Rd(src) ; and r11d, imm ; mov Rd(dst), r11d);
            }
            MachineOpcode::IntegerLessThanImmediate(imm)
            | MachineOpcode::IntegerEqualImmediate(imm)
            | MachineOpcode::IntegerNotEqualImmediate(imm) => {
                let (src, dst) = (ireg(loc[0])?, ireg(loc[1])?);
                dynasm!(ops ; .arch x64 ; cmp Rd(src), imm);
                let cc = match instruction.opcode {
                    MachineOpcode::IntegerLessThanImmediate(_) => Cond::Lt,
                    MachineOpcode::IntegerEqualImmediate(_) => Cond::Eq,
                    _ => Cond::Ne,
                };
                bool_from_flags(&mut ops, dst, cc);
            }
            MachineOpcode::IntegerEqual
            | MachineOpcode::IntegerNotEqual
            | MachineOpcode::IntegerLessThan
            | MachineOpcode::IntegerLessEqual
            | MachineOpcode::IntegerGreaterThan
            | MachineOpcode::IntegerGreaterEqual => {
                let (left, right, dst) = (ireg(loc[0])?, ireg(loc[1])?, ireg(loc[2])?);
                dynasm!(ops ; .arch x64 ; cmp Rd(left), Rd(right));
                let cc = match instruction.opcode {
                    MachineOpcode::IntegerEqual => Cond::Eq,
                    MachineOpcode::IntegerNotEqual => Cond::Ne,
                    MachineOpcode::IntegerLessThan => Cond::Lt,
                    MachineOpcode::IntegerLessEqual => Cond::Le,
                    MachineOpcode::IntegerGreaterThan => Cond::Gt,
                    _ => Cond::Ge,
                };
                bool_from_flags(&mut ops, dst, cc);
            }
            MachineOpcode::FloatEqual
            | MachineOpcode::FloatNotEqual
            | MachineOpcode::FloatLessThan
            | MachineOpcode::FloatLessEqual
            | MachineOpcode::FloatGreaterThan
            | MachineOpcode::FloatGreaterEqual => {
                float_compare(
                    &mut ops,
                    freg(loc[0])?,
                    freg(loc[1])?,
                    ireg(loc[2])?,
                    &instruction.opcode,
                );
            }
            MachineOpcode::IntegerToBoolean => {
                let (src, dst) = (ireg(loc[0])?, ireg(loc[1])?);
                dynasm!(ops ; .arch x64 ; test Rd(src), Rd(src));
                bool_from_flags(&mut ops, dst, Cond::Ne);
            }
            MachineOpcode::FloatToBoolean => float_to_bool(&mut ops, freg(loc[0])?, ireg(loc[1])?),
            MachineOpcode::TruthinessProbe => truthiness_probe(
                &mut ops,
                &mut relocations,
                view,
                ireg(loc[0])?,
                ireg(loc[1])?,
                ireg(loc[2])?,
            ),
            MachineOpcode::LooseEqualityProbe { byte_pc, equal } => {
                let start = ops.offset().0;
                loose_equality::emit(
                    &mut ops,
                    &mut relocations,
                    view,
                    [ireg(loc[0])?, ireg(loc[1])?],
                    [ireg(loc[2])?, ireg(loc[3])?],
                    equal,
                );
                structural_regions.push((
                    "machineLooseEqualityProbe",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::TaggedNullishEqual { byte_pc, equal } => {
                let source = ireg(loc[0])?;
                let destination = ireg(loc[1])?;
                let exit = deopt(instruction.deopt_id(), &deopts)?;
                let nullish = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                let not_native_function = ops.new_dynamic_label();
                let start = ops.offset().0;

                load64(&mut ops, 11, Value::null().to_bits());
                dynasm!(ops ; .arch x64 ; cmp Rq(source), r11 ; je =>nullish);
                load64(&mut ops, 11, Value::undefined().to_bits());
                dynasm!(ops ; .arch x64 ; cmp Rq(source), r11 ; je =>nullish);
                load64(&mut ops, 11, NOT_CELL_MASK);
                dynasm!(ops
                    ; .arch x64
                    ; test Rq(source), r11
                    ; jnz =>not_native_function
                );
                if view.cage_base == 0 {
                    dynasm!(ops ; .arch x64 ; jmp =>exit);
                } else {
                    dynasm!(ops ; .arch x64 ; mov r10d, Rd(source));
                    symbolic(
                        &mut ops,
                        &mut relocations,
                        11,
                        view.cage_base as u64,
                        RelocationTarget::GcCageBase,
                    );
                    dynasm!(ops
                        ; .arch x64
                        ; add r10, r11
                        ; cmp BYTE [r10], view.collection_layout.native_function_type_tag as i8
                        ; je =>exit
                    );
                }
                dynasm!(ops ; .arch x64 ; =>not_native_function);
                if equal {
                    dynasm!(ops ; .arch x64 ; xor Rd(destination), Rd(destination));
                } else {
                    dynasm!(ops ; .arch x64 ; mov Rd(destination), 1);
                }
                dynasm!(ops ; .arch x64 ; jmp =>done ; =>nullish);
                if equal {
                    dynasm!(ops ; .arch x64 ; mov Rd(destination), 1);
                } else {
                    dynasm!(ops ; .arch x64 ; xor Rd(destination), Rd(destination));
                }
                dynasm!(ops ; .arch x64 ; =>done);
                structural_regions.push((
                    "machineTaggedNullishEqual",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::BooleanNot => {
                let (src, dst) = (ireg(loc[0])?, ireg(loc[1])?);
                dynasm!(ops ; .arch x64 ; mov Rd(dst), Rd(src) ; xor Rd(dst), 1);
            }
            MachineOpcode::BooleanConstant(value) => {
                store_const(&mut ops, frame, loc[0], u64::from(value))?
            }
            MachineOpcode::BooleanOr => {
                let (left, right, dst) = (ireg(loc[0])?, ireg(loc[1])?, ireg(loc[2])?);
                dynasm!(ops ; .arch x64 ; mov r11d, Rd(left) ; or r11d, Rd(right) ; mov Rd(dst), r11d);
            }
            MachineOpcode::TaggedSelect => tagged_select(&mut ops, loc)?,
            MachineOpcode::BoxNumber => box_number(&mut ops, freg(loc[0])?, ireg(loc[1])?),
            MachineOpcode::BoxInt32 => box_int32(&mut ops, ireg(loc[0])?, ireg(loc[1])?),
            MachineOpcode::BoxUint32 => box_uint32(&mut ops, ireg(loc[0])?, ireg(loc[1])?),
            MachineOpcode::BoxBoolean => box_bool(&mut ops, ireg(loc[0])?, ireg(loc[1])?),
            MachineOpcode::GuardCondition => {
                let src = ireg(loc[0])?;
                let miss = deopt(instruction.deopt_id(), &deopts)?;
                dynasm!(ops ; .arch x64 ; test Rq(src), Rq(src) ; jz =>miss);
            }
            MachineOpcode::PropertySource {
                function_id,
                logical_pc,
                store,
                ..
            } => {
                let (cell, ordinal, access) =
                    if store {
                        let ordinal = u32::try_from(next_store_ic).map_err(|_| {
                            Unsupported::OperandShape("x86-64 property store source ordinal")
                        })?;
                        let cell = store_ic_cells.get_mut(next_store_ic).ok_or(
                            Unsupported::OperandShape("x86-64 property store source cell"),
                        )?;
                        next_store_ic += 1;
                        (cell, ordinal, PropertySourceAccess::Store)
                    } else {
                        let ordinal = u32::try_from(next_load_ic).map_err(|_| {
                            Unsupported::OperandShape("x86-64 property load source ordinal")
                        })?;
                        let cell = load_ic_cells.get_mut(next_load_ic).ok_or(
                            Unsupported::OperandShape("x86-64 property load source cell"),
                        )?;
                        next_load_ic += 1;
                        (cell, ordinal, PropertySourceAccess::Load)
                    };
                cell.set_source(function_id, logical_pc);
                let address = std::ptr::from_mut::<PropertySourceCell>(cell) as u64;
                symbolic(
                    &mut ops,
                    &mut relocations,
                    11,
                    address,
                    RelocationTarget::PropertySourceCell { access, ordinal },
                );
                store_integer(&mut ops, frame, loc[0], 11)?;
            }
            MachineOpcode::PropertyMegamorphicLoad { byte_pc, atom } => {
                let start = ops.offset().0;
                megamorphic_property::emit(&mut ops, &mut relocations, view, frame, loc, atom)?;
                structural_regions.push((
                    "machineMegamorphicPropertyLoad",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::CacheIrGuardShape { byte_pc, shape } => {
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[1], 10)?;
                dynasm!(ops ; .arch x64 ; test r10d, r10d ; jz =>miss);
                object_header(&mut ops, &mut relocations, view, frame, loc[0], miss)?;
                shape_state_guard(&mut ops, view, miss);
                dynasm!(ops
                    ; .arch x64
                    ; mov r10d, [r11 + view.object_shape_byte as i32]
                    ; test r10d, r10d
                    ; jz =>miss
                    ; cmp r10d, shape as i32
                    ; jne =>miss
                    ; mov r10d, 1
                    ; jmp =>done
                    ; =>miss
                    ; xor r10d, r10d
                    ; =>done
                );
                store_integer(&mut ops, frame, loc[2], 10)?;
                structural_regions.push((
                    "machineCacheIrGuardShape",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::CacheIrGuardDictionaryLayout { byte_pc, layout } => {
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[1], 10)?;
                dynasm!(ops ; .arch x64 ; test r10d, r10d ; jz =>miss);
                object_header(&mut ops, &mut relocations, view, frame, loc[0], miss)?;
                load64(&mut ops, 10, layout);
                dynasm!(ops
                    ; .arch x64
                    ; cmp DWORD [r11 + view.object_shape_byte as i32], 0
                    ; jne =>miss
                    ; cmp [r11 + view.object_dictionary_shape_id_byte as i32], r10
                    ; jne =>miss
                    ; mov r10d, 1
                    ; jmp =>done
                    ; =>miss
                    ; xor r10d, r10d
                    ; =>done
                );
                store_integer(&mut ops, frame, loc[2], 10)?;
                structural_regions.push((
                    "machineCacheIrGuardDictionaryLayout",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::CacheIrGuardOrdinaryState { byte_pc }
            | MachineOpcode::CacheIrGuardAtomSlot { byte_pc, .. } => {
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[1], 10)?;
                dynasm!(ops ; .arch x64 ; test r10d, r10d ; jz =>miss);
                object_header(&mut ops, &mut relocations, view, frame, loc[0], miss)?;
                ordinary_lookup_state_guard(&mut ops, view, miss);
                dynasm!(ops
                    ; .arch x64
                    ; mov r10d, 1
                    ; jmp =>done
                    ; =>miss
                    ; xor r10d, r10d
                    ; =>done
                );
                store_integer(&mut ops, frame, loc[2], 10)?;
                structural_regions.push((
                    if matches!(
                        instruction.opcode,
                        MachineOpcode::CacheIrGuardOrdinaryState { .. }
                    ) {
                        "machineCacheIrGuardOrdinaryState"
                    } else {
                        "machineCacheIrGuardAtomSlot"
                    },
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::CacheIrLoadIntrinsicPrototype { byte_pc, target } => {
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[0], 11)?;
                load_integer(&mut ops, frame, loc[1], 10)?;
                dynasm!(ops ; .arch x64 ; test r10d, r10d ; jz =>miss);
                crate::template::x86_64::intrinsic_prototype::emit(
                    &mut ops,
                    &mut relocations,
                    view,
                    target,
                    byte_pc,
                    11,
                    miss,
                );
                dynasm!(ops
                    ; .arch x64
                    ; mov r9d, 1
                    ; jmp =>done
                    ; =>miss
                );
                load64(&mut ops, 8, VALUE_UNDEFINED);
                dynasm!(ops ; .arch x64 ; xor r9d, r9d ; =>done);
                store_integer(&mut ops, frame, loc[2], 8)?;
                store_integer(&mut ops, frame, loc[3], 9)?;
                structural_regions.push((
                    "machineCacheIrLoadIntrinsicPrototype",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::CacheIrLoadPrototype { byte_pc } => {
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[1], 10)?;
                load64(&mut ops, 8, VALUE_UNDEFINED);
                dynasm!(ops ; .arch x64 ; xor r9d, r9d);
                dynasm!(ops ; .arch x64 ; test r10d, r10d ; jz =>miss);
                object_header(&mut ops, &mut relocations, view, frame, loc[0], miss)?;
                dynasm!(ops
                    ; .arch x64
                    ; mov r8d, [r11 + view.jit_proto_byte as i32]
                    ; test r8d, r8d
                    ; jz =>miss
                    ; mov r9d, 1
                    ; =>miss
                    ; =>done
                );
                store_integer(&mut ops, frame, loc[2], 8)?;
                store_integer(&mut ops, frame, loc[3], 9)?;
                structural_regions.push((
                    "machineCacheIrLoadPrototype",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::CacheIrGuardPrototypeNull { byte_pc } => {
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[1], 10)?;
                dynasm!(ops ; .arch x64 ; test r10d, r10d ; jz =>miss);
                object_header(&mut ops, &mut relocations, view, frame, loc[0], miss)?;
                dynasm!(ops
                    ; .arch x64
                    ; cmp DWORD [r11 + view.jit_proto_byte as i32], 0
                    ; jne =>miss
                    ; mov r10d, 1
                    ; jmp =>done
                    ; =>miss
                    ; xor r10d, r10d
                    ; =>done
                );
                store_integer(&mut ops, frame, loc[2], 10)?;
                structural_regions.push((
                    "machineCacheIrGuardPrototypeNull",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::CacheIrLoadField {
                byte_pc,
                value_byte,
            } => {
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[1], 10)?;
                dynasm!(ops ; .arch x64 ; test r10d, r10d ; jz =>miss);
                object_header(&mut ops, &mut relocations, view, frame, loc[0], miss)?;
                slab_base(&mut ops, view, miss);
                dynasm!(ops
                    ; .arch x64
                    ; mov r8, [r8 + value_byte as i32]
                    ; mov r9d, 1
                    ; jmp =>done
                    ; =>miss
                );
                load64(&mut ops, 8, VALUE_UNDEFINED);
                dynasm!(ops ; .arch x64 ; xor r9d, r9d ; =>done);
                store_integer(&mut ops, frame, loc[2], 8)?;
                store_integer(&mut ops, frame, loc[3], 9)?;
                structural_regions.push((
                    "machineCacheIrLoadField",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::PropertyMegamorphicStore { byte_pc, atom } => {
                let start = ops.offset().0;
                megamorphic_property::emit_store(
                    &mut ops,
                    &mut relocations,
                    view,
                    frame,
                    loc,
                    atom,
                )?;
                structural_regions.push((
                    "machineMegamorphicPropertyStore",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::CacheIrStoreField {
                byte_pc,
                value_byte,
            } => {
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[2], 10)?;
                dynasm!(ops ; .arch x64 ; test r10d, r10d ; jz =>miss);
                object_header(&mut ops, &mut relocations, view, frame, loc[0], miss)?;
                slab_base(&mut ops, view, miss);
                load_integer(&mut ops, frame, loc[1], 10)?;
                dynasm!(ops
                    ; .arch x64
                    ; mov [r8 + value_byte as i32], r10
                    ; mov r9d, 1
                    ; jmp =>done
                    ; =>miss
                    ; xor r11d, r11d
                    ; xor r9d, r9d
                    ; =>done
                );
                store_integer(&mut ops, frame, loc[3], 11)?;
                store_integer(&mut ops, frame, loc[4], 9)?;
                structural_regions.push((
                    "machineCacheIrStoreField",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::CacheIrGuardExtensible {
                byte_pc,
                value_byte,
            } => {
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let inline = ops.new_dynamic_label();
                let storage_fits = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[1], 10)?;
                dynasm!(ops ; .arch x64 ; test r10d, r10d ; jz =>miss);
                object_header(&mut ops, &mut relocations, view, frame, loc[0], miss)?;
                ordinary_lookup_state_guard(&mut ops, view, miss);
                dynasm!(ops
                    ; .arch x64
                    ; mov r10d, value_byte as i32
                    ; test r10d, 7
                    ; jnz =>miss
                    ; mov r9d, [r11 + view.object_slab_handle_byte as i32]
                    ; test r9d, r9d
                    ; jz =>inline
                );
                symbolic(
                    &mut ops,
                    &mut relocations,
                    8,
                    view.cage_base as u64,
                    RelocationTarget::GcCageBase,
                );
                dynasm!(ops
                    ; .arch x64
                    ; add r8, r9
                    ; movzx r9d, WORD [r8 + view.object_slab_capacity_byte as i32]
                    ; shr r10d, 3
                    ; cmp r10d, r9d
                    ; jae =>miss
                    ; jmp =>storage_fits
                    ; =>inline
                    ; shr r10d, 3
                    ; cmp r10d, view.object_inline_slot_cap as i32
                    ; jae =>miss
                    ; =>storage_fits
                    ; cmp BYTE [r11 + view.object_extensible_byte as i32], 0
                    ; je =>miss
                    ; movzx r9d, WORD [r11 + view.object_slab_len_byte as i32]
                    ; cmp r9d, r10d
                    ; jne =>miss
                    ; mov r10d, 1
                    ; jmp =>done
                    ; =>miss
                    ; xor r10d, r10d
                    ; =>done
                );
                store_integer(&mut ops, frame, loc[2], 10)?;
                structural_regions.push((
                    "machineCacheIrGuardExtensible",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::CacheIrPublishShape {
                byte_pc,
                shape,
                new_len,
                initialize_inline,
            } => {
                let start = ops.offset().0;
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[1], 10)?;
                dynasm!(ops ; .arch x64 ; test r10d, r10d ; jz =>done);
                load_integer(&mut ops, frame, loc[0], 11)?;
                if initialize_inline {
                    let ready = ops.new_dynamic_label();
                    dynasm!(ops
                        ; .arch x64
                        ; cmp DWORD [r11 + view.object_slab_handle_byte as i32], 0
                        ; jne =>ready
                        ; lea r10, [r11 + view.object_inline_values_byte as i32]
                        ; mov [r11 + view.object_values_ptr_byte as i32], r10
                        ; =>ready
                    );
                }
                dynasm!(ops
                    ; .arch x64
                    ; mov WORD [r11 + view.object_slab_len_byte as i32], new_len as i16
                    ; mov DWORD [r11 + view.object_shape_byte as i32], shape as i32
                    ; =>done
                );
                structural_regions.push((
                    "machineCacheIrPublishShape",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::CacheIrWriteBarrier {
                byte_pc,
                value_is_non_cell,
            } => {
                let start = ops.offset().0;
                if !value_is_non_cell {
                    let done = ops.new_dynamic_label();
                    load_integer(&mut ops, frame, loc[2], 11)?;
                    dynasm!(ops ; .arch x64 ; test r11d, r11d ; jz =>done);
                    load_integer(&mut ops, frame, loc[1], 2)?;
                    load64(&mut ops, 11, NOT_CELL_MASK);
                    dynasm!(ops ; .arch x64 ; test rdx, r11 ; jnz =>done);
                    load_integer(&mut ops, frame, loc[0], 6)?;
                    dynasm!(ops
                        ; .arch x64
                        ; mov rdi, [r15 + THREAD_OFFSET as i32]
                        ; mov rdi, [rdi + VM_THREAD_GC_HEAP_OFFSET as i32]
                    );
                    runtime(
                        &mut ops,
                        &mut relocations,
                        otter_vm::runtime_stubs::WRITE_BARRIER_MUTATING.entry_addr() as u64,
                        otter_vm::native_abi::STUB_WRITE_BARRIER,
                    );
                    dynasm!(ops ; .arch x64 ; call r11 ; =>done);
                }
                structural_regions.push((
                    "machineCacheIrWriteBarrier",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::ExoticLength { byte_pc } => {
                let start = ops.offset().0;
                let try_string = ops.new_dynamic_label();
                let have = ops.new_dynamic_label();
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[0], 10)?;
                load64(&mut ops, 11, NOT_CELL_MASK);
                dynasm!(ops ; .arch x64 ; test r10, r11 ; jnz =>miss ; mov r10d, r10d);
                symbolic(
                    &mut ops,
                    &mut relocations,
                    11,
                    view.cage_base as u64,
                    RelocationTarget::GcCageBase,
                );
                dynasm!(ops
                    ; .arch x64
                    ; add r10, r11
                    ; movzx eax, BYTE [r10]
                    ; cmp eax, i32::from(view.array_layout.type_tag)
                    ; jne =>try_string
                    ; mov r11, [r10 + view.array_layout.length_byte as i32]
                    ; cmp r11, i32::MAX
                    ; ja =>miss
                    ; jmp =>have
                    ; =>try_string
                    ; cmp eax, i32::from(view.string_layout.string_type_tag)
                    ; jne =>miss
                    ; mov r11d, [r10 + view.string_layout.string_len_byte as i32]
                    ; cmp r11, i32::MAX
                    ; ja =>miss
                    ; =>have
                );
                dynasm!(ops ; .arch x64 ; mov r11d, r11d);
                load64(&mut ops, 10, NUMBER_TAG);
                dynasm!(ops ; .arch x64 ; or r11, r10 ; mov r10d, 1 ; jmp =>done ; =>miss);
                load64(&mut ops, 11, VALUE_UNDEFINED);
                dynasm!(ops ; .arch x64 ; xor r10d, r10d ; =>done);
                store_integer(&mut ops, frame, loc[2], 10)?;
                store_integer(&mut ops, frame, loc[1], 11)?;
                structural_regions.push((
                    "machineExoticLength",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::CacheIrJoin { store: true, .. } => {
                load_integer(&mut ops, frame, loc[0], 11)?;
                store_integer(&mut ops, frame, loc[1], 11)?;
            }
            MachineOpcode::CacheIrJoin { store: false, .. } => {
                // Preserve the condition in the non-allocatable move scratch
                // before loading the value: the value scratch may itself own
                // the condition input, so the opposite order corrupts the
                // parallel SSA copy.
                load_integer(&mut ops, frame, loc[1], 11)?;
                load_integer(&mut ops, frame, loc[0], 10)?;
                store_integer(&mut ops, frame, loc[2], 10)?;
                store_integer(&mut ops, frame, loc[3], 11)?;
            }
            MachineOpcode::ElementView { byte_pc } => {
                let access = element_access(view, byte_pc)?;
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                element_view(
                    &mut ops,
                    &mut relocations,
                    view,
                    frame,
                    loc[0],
                    access,
                    miss,
                )?;
                store_integer(&mut ops, frame, loc[1], 8)?;
                store_integer(&mut ops, frame, loc[2], 9)?;
                dynasm!(ops
                    ; .arch x64
                    ; mov r10d, 1
                    ; jmp =>done
                    ; =>miss
                    ; xor r10d, r10d
                    ; =>done
                );
                store_integer(&mut ops, frame, loc[3], 10)?;
                structural_regions.push((
                    "machineElementView",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::ElementAddress { byte_pc } => {
                let access = element_access(view, byte_pc)?;
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[3], 10)?;
                dynasm!(ops ; .arch x64 ; test r10d, r10d ; jz =>miss);
                load_integer(&mut ops, frame, loc[0], 8)?;
                load_integer(&mut ops, frame, loc[1], 9)?;
                load_integer(&mut ops, frame, loc[2], 11)?;
                dynasm!(ops
                    ; .arch x64
                    ; mov r10, r11
                    ; shr r10, 48
                    ; cmp r10w, NUMBER_TAG_HI16 as i16
                    ; jne =>miss
                );
                match access.length_width {
                    otter_vm::jit::JitGuardWidth::Byte | otter_vm::jit::JitGuardWidth::Word32 => {
                        dynasm!(ops ; .arch x64 ; cmp r11d, r9d ; jae =>miss);
                    }
                    otter_vm::jit::JitGuardWidth::Word64 => {
                        dynasm!(ops ; .arch x64 ; mov r11d, r11d ; cmp r11, r9 ; jae =>miss);
                    }
                }
                let shift = access.element.stride_shift() as i8;
                dynasm!(ops
                    ; .arch x64
                    ; mov r11d, r11d
                    ; shl r11, shift
                    ; add r8, r11
                );
                store_integer(&mut ops, frame, loc[4], 8)?;
                dynasm!(ops
                    ; .arch x64
                    ; mov r10d, 1
                    ; jmp =>done
                    ; =>miss
                    ; xor r10d, r10d
                    ; =>done
                );
                store_integer(&mut ops, frame, loc[5], 10)?;
                structural_regions.push((
                    "machineElementAddress",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::ElementValueLoad { byte_pc } => {
                let access = element_access(view, byte_pc)?;
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[1], 9)?;
                dynasm!(ops ; .arch x64 ; test r9d, r9d ; jz =>miss);
                load_integer(&mut ops, frame, loc[0], 8)?;
                match access.element {
                    otter_vm::jit::JitElementRepr::Boxed => {
                        dynasm!(ops ; .arch x64 ; mov r10, [r8]);
                        load64(&mut ops, 11, Value::hole().to_bits());
                        dynasm!(ops ; .arch x64 ; cmp r10, r11 ; je =>miss);
                    }
                    otter_vm::jit::JitElementRepr::Int32 => {
                        dynasm!(ops ; .arch x64 ; mov r10d, [r8]);
                        load64(&mut ops, 11, NUMBER_TAG);
                        dynasm!(ops ; .arch x64 ; or r10, r11);
                    }
                    otter_vm::jit::JitElementRepr::Float64 => {
                        dynasm!(ops ; .arch x64 ; movsd xmm14, [r8]);
                        box_number(&mut ops, 14, 10);
                    }
                }
                dynasm!(ops ; .arch x64 ; mov r9d, 1 ; jmp =>done ; =>miss);
                load64(&mut ops, 10, VALUE_UNDEFINED);
                dynasm!(ops ; .arch x64 ; xor r9d, r9d ; =>done);
                store_integer(&mut ops, frame, loc[2], 10)?;
                store_integer(&mut ops, frame, loc[3], 9)?;
                structural_regions.push((
                    "machineElementValueLoad",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::ElementValueGuard { byte_pc } => {
                let access = element_access(view, byte_pc)?;
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[2], 10)?;
                dynasm!(ops ; .arch x64 ; test r10d, r10d ; jz =>miss);
                if access.element == otter_vm::jit::JitElementRepr::Boxed {
                    load_integer(&mut ops, frame, loc[0], 8)?;
                    dynasm!(ops ; .arch x64 ; mov r10, [r8]);
                    load64(&mut ops, 11, Value::hole().to_bits());
                    dynasm!(ops ; .arch x64 ; cmp r10, r11 ; je =>miss);
                }
                if access.element != otter_vm::jit::JitElementRepr::Float64 {
                    load_integer(&mut ops, frame, loc[1], 10)?;
                    match access.element {
                        otter_vm::jit::JitElementRepr::Boxed => {
                            load64(&mut ops, 11, NOT_CELL_MASK);
                            dynasm!(ops ; .arch x64 ; test r10, r11 ; jz =>miss);
                        }
                        otter_vm::jit::JitElementRepr::Int32 => {
                            dynasm!(ops ; .arch x64 ; mov r11, r10 ; shr r11, 48 ; cmp r11w, NUMBER_TAG_HI16 as i16 ; jne =>miss);
                        }
                        otter_vm::jit::JitElementRepr::Float64 => unreachable!(),
                    }
                }
                dynasm!(ops
                    ; .arch x64
                    ; mov r10d, 1
                    ; jmp =>done
                    ; =>miss
                    ; xor r10d, r10d
                    ; =>done
                );
                store_integer(&mut ops, frame, loc[3], 10)?;
                structural_regions.push((
                    "machineElementValueGuard",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::ElementValueStore { byte_pc } => {
                let access = element_access(view, byte_pc)?;
                let start = ops.offset().0;
                load_integer(&mut ops, frame, loc[0], 11)?;
                match access.element {
                    otter_vm::jit::JitElementRepr::Boxed => {
                        load_integer(&mut ops, frame, loc[1], 10)?;
                        dynasm!(ops ; .arch x64 ; mov [r11], r10);
                    }
                    otter_vm::jit::JitElementRepr::Int32 => {
                        load_integer(&mut ops, frame, loc[1], 10)?;
                        dynasm!(ops ; .arch x64 ; mov [r11], r10d);
                    }
                    otter_vm::jit::JitElementRepr::Float64 => {
                        load_float(&mut ops, frame, loc[1], 14)?;
                        dynasm!(ops ; .arch x64 ; movsd [r11], xmm14);
                    }
                }
                structural_regions.push((
                    "machineElementValueStore",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::BackedgePoll => {
                let exit = deopt(instruction.deopt_id(), &deopts)?;
                poll(
                    &mut ops,
                    &mut relocations,
                    poll_entry,
                    exit,
                    finish_error,
                    fatal,
                );
            }
            MachineOpcode::Return => {
                let src = ireg(loc[0])?;
                dynasm!(ops ; .arch x64 ; mov rax, Rq(src) ; xor edx, edx);
                epilogue(&mut ops, frame, saved);
            }
            MachineOpcode::Jump => {
                let successor = sequence.blocks()[block_index]
                    .successors
                    .first()
                    .ok_or(Unsupported::OperandShape("x86-64 jump successor"))?;
                let target = blocks[successor.0 as usize];
                dynasm!(ops ; .arch x64 ; jmp =>target);
            }
            MachineOpcode::BranchIf(when_true) => {
                let [taken, fallthrough] = sequence.blocks()[block_index].successors.as_slice()
                else {
                    return Err(Unsupported::OperandShape("x86-64 branch successors"));
                };
                let (taken, fallthrough, condition) = (
                    blocks[taken.0 as usize],
                    blocks[fallthrough.0 as usize],
                    ireg(loc[0])?,
                );
                dynasm!(ops ; .arch x64 ; test Rd(condition), Rd(condition));
                if when_true {
                    dynasm!(ops ; .arch x64 ; jnz =>taken);
                } else {
                    dynasm!(ops ; .arch x64 ; jz =>taken);
                }
                dynasm!(ops ; .arch x64 ; jmp =>fallthrough);
            }
            MachineOpcode::BranchNativeStatus => {
                let [success, throw, fatal_target] =
                    sequence.blocks()[block_index].successors.as_slice()
                else {
                    return Err(Unsupported::OperandShape("x86-64 native-status successors"));
                };
                let (success, throw, fatal_target) = (
                    blocks[success.0 as usize],
                    blocks[throw.0 as usize],
                    blocks[fatal_target.0 as usize],
                );
                load_integer(&mut ops, frame, loc[0], 11)?;
                dynasm!(ops
                    ; .arch x64
                    ; test r11, r11
                    ; jz =>success
                    ; cmp r11d, NativeResultStatus::Throw as i32
                    ; je =>throw
                    ; jmp =>fatal_target
                );
            }
            MachineOpcode::Throw => {
                load_integer(&mut ops, frame, loc[0], 0)?;
                dynasm!(ops ; .arch x64 ; mov edx, NativeResultStatus::Throw as i32);
                epilogue(&mut ops, frame, saved);
            }
            MachineOpcode::NativeLeafIdentity {
                builtin_native_ref,
                byte_pc,
            } => {
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[1], 9)?;
                dynasm!(ops ; .arch x64 ; test r9d, r9d ; jz =>miss);
                load_integer(&mut ops, frame, loc[0], 10)?;
                super::super::native_leaf::x86_64::emit_guard(
                    &mut ops,
                    view,
                    builtin_native_ref,
                    miss,
                );
                dynasm!(ops
                    ; .arch x64
                    ; mov r9d, 1
                    ; jmp =>done
                    ; =>miss
                    ; xor r9d, r9d
                    ; =>done
                );
                store_integer(&mut ops, frame, loc[2], 9)?;
                structural_regions.push((
                    "machineNativeLeafIdentity",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::NativeInt32Math { stub, byte_pc } => {
                let argument_count = otter_vm::jit_static_native::jit_leaf_builtin(stub)
                    .ok_or(Unsupported::OperandShape("x86-64 Int32 math declaration"))?
                    .argument_count as usize;
                if loc.len() != argument_count + 3 {
                    return Err(Unsupported::OperandShape("x86-64 Int32 math operands"));
                }
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[argument_count], 9)?;
                dynasm!(ops ; .arch x64 ; test r9d, r9d ; jz =>miss);
                super::super::native_leaf::x86_64::emit_int32(&mut ops, stub, miss)?;
                dynasm!(ops
                    ; .arch x64
                    ; mov r9d, 1
                    ; jmp =>done
                    ; =>miss
                    ; xor eax, eax
                    ; xor r9d, r9d
                    ; =>done
                );
                store_integer(&mut ops, frame, loc[argument_count + 2], 9)?;
                structural_regions.push((
                    "machineMethodIntrinsic",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::NativeLeafProbe { stub, byte_pc } => {
                let words = loc
                    .len()
                    .checked_sub(3)
                    .filter(|words| (1..=2).contains(words))
                    .ok_or(Unsupported::OperandShape(
                        "x86-64 native leaf probe operands",
                    ))?;
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[words], 9)?;
                dynasm!(ops ; .arch x64 ; test r9d, r9d ; jz =>miss);
                super::super::native_leaf::x86_64::emit_tagged_call(
                    &mut ops,
                    &mut relocations,
                    stub,
                    words as u8,
                    miss,
                )?;
                dynasm!(ops ; .arch x64 ; mov r9d, 1 ; jmp =>done ; =>miss);
                load64(&mut ops, 0, VALUE_UNDEFINED);
                dynasm!(ops ; .arch x64 ; xor r9d, r9d ; =>done);
                store_integer(&mut ops, frame, loc[words + 2], 9)?;
                structural_regions.push((
                    "machineNativeLeafProbe",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::Call(descriptor_index) => {
                let descriptor = sequence
                    .call_descriptors()
                    .get(descriptor_index as usize)
                    .ok_or(Unsupported::OperandShape("x86-64 call descriptor"))?;
                if let CallTarget::NativeLeaf { target, byte_pc } = descriptor.target {
                    let exit = deopt(instruction.deopt_id(), &deopts)?;
                    let start = ops.offset().0;
                    super::super::native_leaf::x86_64::emit_guard(
                        &mut ops,
                        view,
                        target.builtin_native_ref,
                        exit,
                    );
                    let int32 = descriptor.results == [MachineRepresentation::Int32];
                    if int32 {
                        super::super::native_leaf::x86_64::emit_int32(
                            &mut ops,
                            target.leaf_stub_id,
                            exit,
                        )?;
                    } else {
                        super::super::native_leaf::x86_64::emit_tagged_call(
                            &mut ops,
                            &mut relocations,
                            target.leaf_stub_id,
                            target.argument_count,
                            exit,
                        )?;
                    }
                    structural_regions.push((
                        if int32 {
                            "machineNativeInt32MathIntrinsic"
                        } else {
                            "machineNativeLeafCall"
                        },
                        Some(byte_pc),
                        start,
                        ops.offset().0,
                    ));
                    if !terminal {
                        edits(
                            &mut ops,
                            allocation.edits(),
                            AllocationPoint::After(id),
                            frame,
                        )?;
                    }
                    continue;
                }
                if let CallTarget::ColdCallExit { byte_pc, .. } = descriptor.target {
                    let exit = deopt(instruction.deopt_id(), &deopts)?;
                    let start = ops.offset().0;
                    dynasm!(ops ; .arch x64 ; jmp =>exit);
                    structural_regions.push((
                        "machineColdCallExit",
                        Some(byte_pc),
                        start,
                        ops.offset().0,
                    ));
                    continue;
                }
                if let CallTarget::CommittedRuntime {
                    target,
                    logical_pc,
                    byte_pc,
                    semantic_arity,
                } = descriptor.target
                {
                    let start = ops.offset().0;
                    if descriptor.results == [MachineRepresentation::Tagged] {
                        committed_value_call(
                            &mut ops,
                            &mut relocations,
                            transitions,
                            sequence,
                            frame,
                            instruction,
                            id,
                            descriptor,
                            loc,
                            allocation.edits(),
                            safepoints,
                            &blocks,
                            throw_value,
                            fatal,
                        )?;
                    } else {
                        committed_pair_call(
                            view,
                            &mut ops,
                            &mut relocations,
                            transitions,
                            sequence,
                            frame,
                            instruction,
                            id,
                            loc,
                            safepoints,
                            target,
                            logical_pc,
                            semantic_arity,
                        )?;
                    }
                    structural_regions.push((
                        if target == otter_vm::native_abi::STUB_JIT_BINDING_VALUE {
                            "machineBindingCold"
                        } else if target == otter_vm::native_abi::STUB_JIT_LOAD_PROPERTY {
                            "machinePropertyLoadCold"
                        } else if target == otter_vm::native_abi::STUB_JIT_STORE_PROPERTY {
                            "machinePropertyStoreCold"
                        } else if target == otter_vm::native_abi::STUB_JIT_LOAD_ELEMENT {
                            "machineElementLoadCold"
                        } else if target == otter_vm::native_abi::STUB_JIT_STORE_ELEMENT {
                            "machineElementStoreCold"
                        } else if super::super::derived_this::cold_byte_pc(sequence, block_index)
                            == Some(byte_pc)
                        {
                            "machineDerivedThisBindCold"
                        } else {
                            "machineCommittedValueEffect"
                        },
                        Some(byte_pc),
                        start,
                        ops.offset().0,
                    ));
                    if target == otter_vm::native_abi::STUB_JIT_LOAD_ELEMENT
                        || target == otter_vm::native_abi::STUB_JIT_STORE_ELEMENT
                    {
                        structural_regions.push((
                            "machineCommittedValueEffect",
                            Some(byte_pc),
                            start,
                            ops.offset().0,
                        ));
                    }
                    if !terminal {
                        edits(
                            &mut ops,
                            allocation.edits(),
                            AllocationPoint::After(id),
                            frame,
                        )?;
                    }
                    continue;
                }
                if let CallTarget::Direct {
                    kind,
                    argument_mode,
                    candidates,
                    caller_function_id,
                    logical_pc,
                    byte_pc,
                } = &descriptor.target
                {
                    let site = safepoints
                        .site(id)
                        .filter(|site| instruction.safepoint == Some(site.id))
                        .ok_or(Unsupported::OperandShape("x86-64 direct call safepoint"))?;
                    let exit = deopt(instruction.deopt_id(), &deopts)?;
                    let direct_throw = ops.new_dynamic_label();
                    let direct_done = ops.new_dynamic_label();
                    let publish_inline = *kind == super::super::DirectCallKind::Construct
                        && candidates.len() == 1
                        && !instruction.inline_frames.is_empty();
                    let inline_throw = ops.new_dynamic_label();
                    let inline_done = ops.new_dynamic_label();
                    let inline_finish_error = ops.new_dynamic_label();
                    let inline_fatal = ops.new_dynamic_label();
                    let inline_publication_fail = ops.new_dynamic_label();
                    let packet = super::value_packet_frame(sequence)?;
                    let inline_start = packet.raw_start.checked_add(packet.raw_words).ok_or(
                        Unsupported::OperandShape("x86-64 inline native frame start"),
                    )?;
                    if publish_inline {
                        save_roots(&mut ops, frame, site)?;
                        publish_roots(&mut ops, frame, site)?;
                        inline_calls::enter(
                            &mut ops,
                            view,
                            frame,
                            instruction,
                            site,
                            inline_start,
                            inline_publication_fail,
                        )?;
                    }
                    let start = ops.offset().0;
                    let mut direct_regions = direct_call::DirectCallRegions::default();
                    direct_call::emit(
                        &mut ops,
                        &mut relocations,
                        view,
                        transitions,
                        sequence,
                        frame,
                        instruction,
                        descriptor,
                        loc,
                        site,
                        *kind,
                        *argument_mode,
                        candidates,
                        *caller_function_id,
                        *logical_pc,
                        *byte_pc,
                        publish_inline,
                        exit,
                        if publish_inline {
                            inline_finish_error
                        } else {
                            finish_error
                        },
                        if publish_inline { inline_fatal } else { fatal },
                        if publish_inline {
                            inline_throw
                        } else {
                            direct_throw
                        },
                        if publish_inline {
                            inline_done
                        } else {
                            direct_done
                        },
                        &mut direct_regions,
                    )?;
                    if publish_inline {
                        dynasm!(ops ; .arch x64 ; =>inline_throw);
                        inline_calls::leave(&mut ops, frame, instruction, inline_start)?;
                        dynasm!(ops ; .arch x64 ; jmp =>direct_throw ; =>inline_done);
                        inline_calls::leave(&mut ops, frame, instruction, inline_start)?;
                        dynasm!(ops ; .arch x64 ; jmp =>direct_done ; =>inline_finish_error);
                        inline_calls::leave(&mut ops, frame, instruction, inline_start)?;
                        dynasm!(ops ; .arch x64 ; jmp =>finish_error ; =>inline_fatal);
                        inline_calls::leave(&mut ops, frame, instruction, inline_start)?;
                        dynasm!(ops ; .arch x64 ; jmp =>fatal ; =>inline_publication_fail);
                        clear_roots(&mut ops);
                        reload_roots(&mut ops, frame, site)?;
                        dynasm!(ops ; .arch x64 ; jmp =>exit);
                    }
                    dynasm!(ops ; .arch x64 ; =>direct_throw);
                    match descriptor.exceptional {
                        super::super::ExceptionalEdge::Propagate => {
                            dynasm!(ops ; .arch x64 ; mov edx, NativeResultStatus::Throw as i32);
                            epilogue(&mut ops, frame, saved);
                        }
                        super::super::ExceptionalEdge::LandingPad(target) => {
                            let result_index = descriptor.arguments.len();
                            let result = *loc
                                .get(result_index)
                                .ok_or(Unsupported::OperandShape("x86-64 direct-call result"))?;
                            store_integer(&mut ops, frame, result, 0)?;
                            edits(
                                &mut ops,
                                allocation.edits(),
                                AllocationPoint::After(id),
                                frame,
                            )?;
                            let target = blocks.get(target.0 as usize).copied().ok_or(
                                Unsupported::OperandShape("x86-64 direct-call landing pad"),
                            )?;
                            dynasm!(ops ; .arch x64 ; jmp =>target);
                        }
                        super::super::ExceptionalEdge::None => {
                            return Err(Unsupported::OperandShape(
                                "x86-64 direct-call exceptional edge",
                            ));
                        }
                    }
                    dynasm!(ops ; .arch x64 ; =>direct_done);
                    structural_regions.push((
                        if *kind == super::super::DirectCallKind::Forward {
                            "machineForwardCall"
                        } else {
                            "machineDirectCall"
                        },
                        Some(*byte_pc),
                        start,
                        ops.offset().0,
                    ));
                    if *kind == super::super::DirectCallKind::Construct && candidates.is_empty() {
                        structural_regions.push((
                            "machineGenericConstruct",
                            Some(*byte_pc),
                            start,
                            ops.offset().0,
                        ));
                    }
                    for (guard_start, guard_end) in direct_regions.method_guards {
                        structural_regions.push((
                            "machineDirectMethodGuard",
                            Some(*byte_pc),
                            guard_start,
                            guard_end,
                        ));
                    }
                    for (candidate_start, candidate_end) in direct_regions.method_candidates {
                        structural_regions.push((
                            "machineDirectMethodCandidate",
                            Some(*byte_pc),
                            candidate_start,
                            candidate_end,
                        ));
                    }
                    for (generic_start, generic_end) in direct_regions.generic_methods {
                        structural_regions.push((
                            "machineGenericMethodCall",
                            Some(*byte_pc),
                            generic_start,
                            generic_end,
                        ));
                    }
                    for (region, ranges) in [
                        (
                            "directConstructPrepareFast",
                            direct_regions.construct_prepare_fast,
                        ),
                        (
                            "directConstructPrepareObservable",
                            direct_regions.construct_prepare_observable,
                        ),
                        (
                            "directConstructReceiverAllocFast",
                            direct_regions.construct_receiver_alloc_fast,
                        ),
                        (
                            "directConstructReceiverAllocCold",
                            direct_regions.construct_receiver_alloc_cold,
                        ),
                        (
                            "directConstructResultFast",
                            direct_regions.construct_result_fast,
                        ),
                        (
                            "directConstructResultThrow",
                            direct_regions.construct_result_throw,
                        ),
                    ] {
                        for (region_start, region_end) in ranges {
                            structural_regions.push((
                                region,
                                Some(*byte_pc),
                                region_start,
                                region_end,
                            ));
                        }
                    }
                    if !terminal {
                        edits(
                            &mut ops,
                            allocation.edits(),
                            AllocationPoint::After(id),
                            frame,
                        )?;
                    }
                    continue;
                }
                if let CallTarget::LiteralAllocation {
                    target,
                    logical_pc,
                    byte_pc,
                } = descriptor.target
                {
                    let start = ops.offset().0;
                    literal_allocation_call(
                        &mut ops,
                        &mut relocations,
                        transitions,
                        sequence,
                        frame,
                        instruction,
                        id,
                        loc,
                        safepoints,
                        target,
                        logical_pc,
                        fatal,
                    )?;
                    structural_regions.push((
                        "machineLiteralAllocation",
                        Some(byte_pc),
                        start,
                        ops.offset().0,
                    ));
                    if !terminal {
                        edits(
                            &mut ops,
                            allocation.edits(),
                            AllocationPoint::After(id),
                            frame,
                        )?;
                    }
                    continue;
                }
                let CallTarget::RuntimeStub(target) = descriptor.target else {
                    return Err(Unsupported::OperandShape("x86-64 scalar call target"));
                };
                if target == otter_vm::native_abi::STUB_JIT_CLASS_SUPER_CONSTRUCTOR {
                    if loc.len() < 2 {
                        return Err(Unsupported::OperandShape("x86-64 class-super load call"));
                    }
                    let exit = deopt(instruction.deopt_id(), &deopts)?;
                    let start = ops.offset().0;
                    load_integer(&mut ops, frame, loc[0], 6)?;
                    load64(&mut ops, 10, NOT_CELL_MASK);
                    dynasm!(ops
                        ; .arch x64
                        ; test rsi, r10
                        ; jnz =>exit
                        ; mov r11d, esi
                    );
                    symbolic(
                        &mut ops,
                        &mut relocations,
                        10,
                        view.cage_base as u64,
                        RelocationTarget::GcCageBase,
                    );
                    dynasm!(ops
                        ; .arch x64
                        ; add r11, r10
                        ; cmp BYTE [r11], view.class_constructor_layout.type_tag as i8
                        ; jne =>exit
                        ; mov rax, [r11 + view.class_constructor_layout.super_constructor_byte as i32]
                    );
                    load64(&mut ops, 10, Value::hole().to_bits());
                    dynasm!(ops ; .arch x64 ; cmp rax, r10 ; je =>exit);
                    store_integer(&mut ops, frame, loc[1], 0)?;
                    structural_regions.push(("machineClassSuperLoad", None, start, ops.offset().0));
                    if !terminal {
                        edits(
                            &mut ops,
                            allocation.edits(),
                            AllocationPoint::After(id),
                            frame,
                        )?;
                    }
                    continue;
                }
                if target == otter_vm::native_abi::STUB_JIT_ACKNOWLEDGE_CAUGHT_THROW {
                    if !descriptor.arguments.is_empty()
                        || !descriptor.results.is_empty()
                        || descriptor.exceptional != super::super::ExceptionalEdge::None
                        || instruction.safepoint.is_some()
                        || !loc.is_empty()
                    {
                        return Err(Unsupported::OperandShape(
                            "x86-64 caught-throw acknowledgement",
                        ));
                    }
                    dynasm!(ops ; .arch x64 ; mov rdi, r15);
                    runtime(
                        &mut ops,
                        &mut relocations,
                        transitions.entry(target),
                        target,
                    );
                    dynasm!(ops ; .arch x64 ; call r11 ; test rax, rax ; jne =>fatal);
                    if !terminal {
                        edits(
                            &mut ops,
                            allocation.edits(),
                            AllocationPoint::After(id),
                            frame,
                        )?;
                    }
                    continue;
                }
                if target == otter_vm::native_abi::STUB_STRING_CONCAT_ALLOC {
                    if loc.len() < 4
                        || ireg(loc[0])? != 2
                        || ireg(loc[1])? != 1
                        || ireg(loc[2])? != 8
                        || ireg(loc[3])? != 0
                    {
                        return Err(Unsupported::OperandShape("x86-64 string concat ABI"));
                    }
                    let site = safepoints
                        .site(id)
                        .filter(|site| instruction.safepoint == Some(site.id))
                        .ok_or(Unsupported::OperandShape("x86-64 allocating safepoint"))?;
                    let exit = deopt(instruction.deopt_id(), &deopts)?;
                    allocating_call(
                        &mut ops,
                        &mut relocations,
                        frame,
                        site,
                        string_concat_entry,
                        target,
                        exit,
                    )?;
                    if !terminal {
                        edits(
                            &mut ops,
                            allocation.edits(),
                            AllocationPoint::After(id),
                            frame,
                        )?;
                    }
                    continue;
                }
                let entry = if target == otter_vm::native_abi::STUB_STRICT_EQ_LEAF {
                    strict_eq_entry
                } else if target == otter_vm::native_abi::STUB_TO_BOOLEAN_LEAF {
                    to_boolean_entry
                } else {
                    return Err(Unsupported::OperandShape("x86-64 scalar runtime call"));
                };
                if loc.len() < 3 || ireg(loc[0])? != 6 || ireg(loc[1])? != 2 || ireg(loc[2])? != 0 {
                    return Err(Unsupported::OperandShape("x86-64 scalar leaf ABI"));
                }
                let exit = deopt(instruction.deopt_id(), &deopts)?;
                dynasm!(ops
                    ; .arch x64
                    ; mov rdi, [r15 + THREAD_OFFSET as i32]
                    ; mov rdi, [rdi + VM_THREAD_GC_HEAP_OFFSET as i32]
                );
                runtime(&mut ops, &mut relocations, entry, target);
                dynasm!(ops ; .arch x64 ; call r11 ; test rdx, rdx ; jne =>exit);
                load64(&mut ops, 11, Value::boolean(true).to_bits());
                dynasm!(ops ; .arch x64 ; cmp rax, r11);
                bool_from_flags(&mut ops, 0, Cond::Eq);
            }
            MachineOpcode::Fatal => dynasm!(ops ; .arch x64 ; jmp =>fatal),
        }
        if !terminal {
            edits(
                &mut ops,
                allocation.edits(),
                AllocationPoint::After(id),
                frame,
            )?;
        }
    }

    dynasm!(ops
        ; .arch x64
        ; =>throw_value
        ; mov edx, NativeResultStatus::Throw as i32
    );
    epilogue(&mut ops, frame, saved);
    emit_cold_exits(
        &mut ops,
        &mut relocations,
        frame,
        saved,
        transitions,
        bail,
        finish_error,
        fatal,
        vm_register_count,
    );
    emit_deopts(
        &mut ops,
        &mut relocations,
        frame,
        saved,
        deopt_runtime,
        deopt_writeback_entry,
        &deopts,
        shared_deopt,
    )?;
    let (osr_entries, osr_regions) = emit_osr_entries(
        &mut ops, sequence, allocation, frame, saved, has_poll, &osr_sites,
    )?;
    let buffer = ops
        .finalize()
        .map_err(|_| Unsupported::Backend(crate::BackendFailure::Finalization))?;
    Ok(Emission {
        code: CompiledCode::new(buffer, AssemblyOffset(0)),
        generated_stack_frame_bytes: frame.frame_bytes()
            + if deopts.is_empty() {
                0
            } else {
                DEOPT_DUMP_BYTES as u32
            }
            .max(
                if sequence.call_descriptors().iter().any(|descriptor| {
                    matches!(
                        descriptor.target,
                        CallTarget::CommittedRuntime { .. } | CallTarget::LiteralAllocation { .. }
                    )
                }) {
                    MACHINE_ROOT_RECORD_SIZE
                } else {
                    0
                },
            ),
        relocations,
        osr_entries,
        osr_regions,
        structural_regions,
    })
}

fn validate_locations(
    sequence: &InstructionSequence,
    allocation: &AllocatedSequence,
) -> Result<(), Unsupported> {
    for index in 0..sequence.instructions().len() {
        for location in allocation
            .instruction_locations(MachineInstructionId(index as u32))
            .ok_or(Unsupported::OperandShape("scalar allocation coverage"))?
        {
            match location {
                AllocatedLocation::Register(r) if r.is_integer() && r.encoding() > 15 => {
                    return Err(Unsupported::OperandShape("x86-64 Machine IR GPR"));
                }
                AllocatedLocation::Register(r) if r.is_float() && r.encoding() > 14 => {
                    return Err(Unsupported::OperandShape("x86-64 Machine IR FP register"));
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn block_for(
    sequence: &InstructionSequence,
    id: MachineInstructionId,
) -> Result<usize, Unsupported> {
    sequence
        .blocks()
        .iter()
        .position(|block| block.first.0 <= id.0 && id.0 < block.end.0)
        .ok_or(Unsupported::OperandShape("scalar instruction block"))
}

fn deopt(id: Option<DeoptId>, labels: &[DynamicLabel]) -> Result<DynamicLabel, Unsupported> {
    id.and_then(|id| labels.get(id.0 as usize).copied())
        .ok_or(Unsupported::OperandShape("scalar deopt label"))
}

fn maybe_deopt(
    id: Option<DeoptId>,
    labels: &[DynamicLabel],
) -> Result<Option<DynamicLabel>, Unsupported> {
    id.map(|id| {
        labels
            .get(id.0 as usize)
            .copied()
            .ok_or(Unsupported::OperandShape("scalar deopt label"))
    })
    .transpose()
}

fn prologue(ops: &mut Assembler, frame: MachineFrameLayout, saved: SavedFrame) {
    dynasm!(ops ; .arch x64 ; push rbp ; push r15);
    if saved.rbx {
        dynasm!(ops ; .arch x64 ; push rbx);
    }
    if let Some(highest) = saved.highest {
        for register in 12..=highest {
            dynasm!(ops ; .arch x64 ; push Rq(register));
        }
    }
    let padding = frame.fixed_bytes() - saved.actual_bytes();
    if padding != 0 {
        dynasm!(ops ; .arch x64 ; sub rsp, padding as i32);
    }
    if frame.spill_area_bytes() != 0 {
        dynasm!(ops ; .arch x64 ; sub rsp, frame.spill_area_bytes() as i32);
    }
    dynasm!(ops ; .arch x64 ; mov r15, rdi);
}

fn epilogue(ops: &mut Assembler, frame: MachineFrameLayout, saved: SavedFrame) {
    if frame.spill_area_bytes() != 0 {
        dynasm!(ops ; .arch x64 ; add rsp, frame.spill_area_bytes() as i32);
    }
    let padding = frame.fixed_bytes() - saved.actual_bytes();
    if padding != 0 {
        dynasm!(ops ; .arch x64 ; add rsp, padding as i32);
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

fn edits(
    ops: &mut Assembler,
    edits: &[AllocationEdit],
    point: AllocationPoint,
    frame: MachineFrameLayout,
) -> Result<(), Unsupported> {
    for edit in edits.iter().filter(|edit| edit.point == point) {
        if edit.from == edit.to {
            continue;
        }
        match (edit.from, edit.to) {
            (AllocatedLocation::Register(from), AllocatedLocation::Register(to))
                if from.is_integer() && to.is_integer() =>
            {
                dynasm!(ops ; .arch x64 ; mov Rq(to.encoding()), Rq(from.encoding()))
            }
            (AllocatedLocation::Register(from), AllocatedLocation::Register(to))
                if from.is_float() && to.is_float() =>
            {
                dynasm!(ops ; .arch x64 ; movsd Rx(to.encoding()), Rx(from.encoding()))
            }
            (AllocatedLocation::Register(from), AllocatedLocation::Stack(slot)) => {
                let offset = spill(frame, slot)?;
                if from.is_integer() {
                    dynasm!(ops ; .arch x64 ; mov [rsp + offset], Rq(from.encoding()));
                } else if from.is_float() {
                    dynasm!(ops ; .arch x64 ; movsd [rsp + offset], Rx(from.encoding()));
                } else {
                    return Err(Unsupported::OperandShape("x86-64 spill class"));
                }
            }
            (AllocatedLocation::Stack(slot), AllocatedLocation::Register(to)) => {
                let offset = spill(frame, slot)?;
                if to.is_integer() {
                    dynasm!(ops ; .arch x64 ; mov Rq(to.encoding()), [rsp + offset]);
                } else if to.is_float() {
                    dynasm!(ops ; .arch x64 ; movsd Rx(to.encoding()), [rsp + offset]);
                } else {
                    return Err(Unsupported::OperandShape("x86-64 reload class"));
                }
            }
            (AllocatedLocation::Stack(from), AllocatedLocation::Stack(to)) => {
                let (from, to) = (spill(frame, from)?, spill(frame, to)?);
                dynasm!(ops ; .arch x64 ; mov r11, [rsp + from] ; mov [rsp + to], r11);
            }
            _ => return Err(Unsupported::OperandShape("x86-64 allocation edit class")),
        }
    }
    Ok(())
}

fn emit_mul(
    ops: &mut Assembler,
    loc: &[AllocatedLocation],
    instruction: &super::super::MachineInstruction,
    labels: &[DynamicLabel],
) -> Result<(), Unsupported> {
    let (left, right, dst) = (ireg(loc[0])?, ireg(loc[1])?, ireg(loc[2])?);
    let overflow = deopt(instruction.exit_id(ExitReason::Int32Overflow), labels)?;
    let negative_zero = deopt(instruction.exit_id(ExitReason::NegativeZero), labels)?;
    let done = ops.new_dynamic_label();
    let left_nonnegative = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; mov r11d, Rd(left)
        ; imul r11d, Rd(right)
        ; jo =>overflow
        ; test r11d, r11d
        ; jne =>done
        ; test Rd(left), Rd(left)
        ; jns =>left_nonnegative
        ; test Rd(right), Rd(right)
        ; jns =>negative_zero
        ; jmp =>done
        ; =>left_nonnegative
        ; test Rd(right), Rd(right)
        ; js =>negative_zero
        ; =>done
        ; mov Rd(dst), r11d
    );
    Ok(())
}

fn emit_neg(
    ops: &mut Assembler,
    loc: &[AllocatedLocation],
    instruction: &super::super::MachineInstruction,
    labels: &[DynamicLabel],
) -> Result<(), Unsupported> {
    let (src, dst) = (ireg(loc[0])?, ireg(loc[1])?);
    let overflow = deopt(instruction.exit_id(ExitReason::Int32Overflow), labels)?;
    let negative_zero = deopt(instruction.exit_id(ExitReason::NegativeZero), labels)?;
    dynasm!(ops
        ; .arch x64
        ; mov r11d, Rd(src)
        ; test r11d, r11d
        ; je =>negative_zero
        ; neg r11d
        ; jo =>overflow
        ; mov Rd(dst), r11d
    );
    Ok(())
}

fn integer_binary(
    ops: &mut Assembler,
    opcode: &MachineOpcode,
    loc: &[AllocatedLocation],
) -> Result<(), Unsupported> {
    let (left, right, dst) = (ireg(loc[0])?, ireg(loc[1])?, ireg(loc[2])?);
    dynasm!(ops ; .arch x64 ; mov r11d, Rd(left));
    match opcode {
        MachineOpcode::IntegerAnd => dynasm!(ops ; .arch x64 ; and r11d, Rd(right)),
        MachineOpcode::IntegerOr => dynasm!(ops ; .arch x64 ; or r11d, Rd(right)),
        MachineOpcode::IntegerXor => dynasm!(ops ; .arch x64 ; xor r11d, Rd(right)),
        MachineOpcode::IntegerShiftLeft => {
            dynasm!(ops ; .arch x64 ; push rcx ; mov ecx, Rd(right) ; shl r11d, cl ; pop rcx)
        }
        MachineOpcode::IntegerShiftRight => {
            dynasm!(ops ; .arch x64 ; push rcx ; mov ecx, Rd(right) ; sar r11d, cl ; pop rcx)
        }
        MachineOpcode::IntegerShiftRightLogical => {
            dynasm!(ops ; .arch x64 ; push rcx ; mov ecx, Rd(right) ; shr r11d, cl ; pop rcx)
        }
        _ => unreachable!(),
    }
    dynasm!(ops ; .arch x64 ; mov Rd(dst), r11d);
    Ok(())
}

#[derive(Clone, Copy)]
enum Cond {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

fn bool_from_flags(ops: &mut Assembler, dst: u8, condition: Cond) {
    let yes = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    match condition {
        Cond::Eq => dynasm!(ops ; .arch x64 ; je =>yes),
        Cond::Ne => dynasm!(ops ; .arch x64 ; jne =>yes),
        Cond::Lt => dynasm!(ops ; .arch x64 ; jl =>yes),
        Cond::Le => dynasm!(ops ; .arch x64 ; jle =>yes),
        Cond::Gt => dynasm!(ops ; .arch x64 ; jg =>yes),
        Cond::Ge => dynasm!(ops ; .arch x64 ; jge =>yes),
    }
    dynasm!(ops ; .arch x64 ; xor Rd(dst), Rd(dst) ; jmp =>done ; =>yes ; mov Rd(dst), 1 ; =>done);
}

fn float_compare(ops: &mut Assembler, left: u8, right: u8, dst: u8, opcode: &MachineOpcode) {
    let yes = ops.new_dynamic_label();
    let no = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; ucomisd Rx(left), Rx(right));
    match opcode {
        MachineOpcode::FloatEqual => dynasm!(ops ; .arch x64 ; jp =>no ; je =>yes),
        MachineOpcode::FloatNotEqual => dynasm!(ops ; .arch x64 ; jp =>yes ; jne =>yes),
        MachineOpcode::FloatLessThan => dynasm!(ops ; .arch x64 ; jp =>no ; jb =>yes),
        MachineOpcode::FloatLessEqual => dynasm!(ops ; .arch x64 ; jp =>no ; jbe =>yes),
        MachineOpcode::FloatGreaterThan => dynasm!(ops ; .arch x64 ; ja =>yes),
        MachineOpcode::FloatGreaterEqual => dynasm!(ops ; .arch x64 ; jae =>yes),
        _ => unreachable!(),
    }
    dynasm!(ops ; .arch x64 ; =>no ; xor Rd(dst), Rd(dst) ; jmp =>done ; =>yes ; mov Rd(dst), 1 ; =>done);
}

fn float_to_bool(ops: &mut Assembler, src: u8, dst: u8) {
    let no = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; movq r11, Rx(src)
        ; shl r11, 1
        ; jz =>no
        ; ucomisd Rx(src), Rx(src)
        ; jp =>no
        ; mov Rd(dst), 1
        ; jmp =>done
        ; =>no
        ; xor Rd(dst), Rd(dst)
        ; =>done
    );
}

fn truthiness_probe(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    source: u8,
    result: u8,
    hit: u8,
) {
    let int = ops.new_dynamic_label();
    let double = ops.new_dynamic_label();
    let truthy = ops.new_dynamic_label();
    let falsy = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; mov r10, Rq(source));
    load64(ops, 11, NUMBER_TAG);
    dynasm!(ops
        ; .arch x64
        ; mov rax, r10
        ; and rax, r11
        ; cmp rax, r11
        ; je =>int
        ; test rax, rax
        ; jne =>double
    );
    for (value, label) in [
        (Value::boolean(true).to_bits(), truthy),
        (Value::boolean(false).to_bits(), falsy),
        (Value::null().to_bits(), falsy),
        (Value::undefined().to_bits(), falsy),
    ] {
        load64(ops, 11, value);
        dynasm!(ops ; .arch x64 ; cmp r10, r11 ; je =>label);
    }
    load64(ops, 11, otter_vm::value::tag::NOT_CELL_MASK);
    dynasm!(ops ; .arch x64 ; mov rax, r10 ; and rax, r11 ; jne =>miss ; test r10, r10 ; jz =>miss);
    if view.cage_base == 0 {
        dynasm!(ops ; .arch x64 ; jmp =>miss);
    } else {
        symbolic(
            ops,
            relocations,
            11,
            view.cage_base as u64,
            RelocationTarget::GcCageBase,
        );
        dynasm!(ops ; .arch x64 ; mov eax, r10d ; add r11, rax ; movzx eax, BYTE [r11]);
        for tag in view
            .primitive_cell_type_tags
            .into_iter()
            .chain([view.collection_layout.native_function_type_tag])
        {
            dynasm!(ops ; .arch x64 ; cmp eax, i32::from(tag) ; je =>miss);
        }
        dynasm!(ops ; .arch x64 ; jmp =>truthy);
    }
    dynasm!(ops ; .arch x64 ; =>int ; test r10d, r10d ; jz =>falsy ; jmp =>truthy ; =>double);
    load64(ops, 11, DOUBLE_OFFSET);
    dynasm!(ops ; .arch x64 ; sub r10, r11 ; test r10, r10 ; jz =>falsy);
    load64(ops, 11, (-0.0_f64).to_bits());
    dynasm!(ops ; .arch x64 ; cmp r10, r11 ; je =>falsy);
    load64(ops, 11, otter_vm::value::tag::CANONICAL_NAN);
    dynasm!(ops
        ; .arch x64
        ; cmp r10, r11
        ; je =>falsy
        ; =>truthy
        ; mov Rd(result), 1
        ; mov Rd(hit), 1
        ; jmp =>done
        ; =>falsy
        ; xor Rd(result), Rd(result)
        ; mov Rd(hit), 1
        ; jmp =>done
        ; =>miss
        ; xor Rd(result), Rd(result)
        ; xor Rd(hit), Rd(hit)
        ; =>done
    );
}

/// Select the ECMAScript base-constructor result in `rax`.
///
/// Object results win; primitive results fall back to the already-published
/// receiver. Function-id immediates and every non-primitive GC body are
/// Objects, while strings, symbols, and bigints are primitive cells.
fn select_base_construct_value(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    receiver: u8,
) {
    let cell = ops.new_dynamic_label();
    let object = ops.new_dynamic_label();
    let primitive = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    load64(ops, 11, NOT_CELL_MASK);
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
        ; mov r10d, eax
    );
    symbolic(
        ops,
        relocations,
        11,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops ; .arch x64 ; add r10, r11 ; movzx r10d, BYTE [r10]);
    for tag in view.primitive_cell_type_tags {
        dynasm!(ops ; .arch x64 ; cmp r10d, tag as i32 ; je =>primitive);
    }
    dynasm!(ops
        ; .arch x64
        ; =>object
        ; jmp =>done
        ; =>primitive
        ; mov rax, Rq(receiver)
        ; =>done
    );
}

fn tagged_select(ops: &mut Assembler, loc: &[AllocatedLocation]) -> Result<(), Unsupported> {
    let (condition, yes, no, dst) = (ireg(loc[0])?, ireg(loc[1])?, ireg(loc[2])?, ireg(loc[3])?);
    dynasm!(ops
        ; .arch x64
        ; mov r11, Rq(no)
        ; test Rd(condition), Rd(condition)
        ; cmovne r11, Rq(yes)
        ; mov Rq(dst), r11
    );
    Ok(())
}

fn decode_number(ops: &mut Assembler, src: u8, dst: u8, bail: DynamicLabel) {
    let double = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; mov r11, Rq(src)
        ; shr r11, 48
        ; cmp r11w, NUMBER_TAG_HI16 as i16
        ; jne =>double
        ; cvtsi2sd Rx(dst), Rd(src)
        ; jmp =>done
        ; =>double
        ; test r11w, NUMBER_TAG_HI16 as i16
        ; jz =>bail
    );
    load64(ops, 11, DOUBLE_OFFSET);
    dynasm!(ops
        ; .arch x64
        ; movq Rx(dst), Rq(src)
        ; movq xmm15, r11
        ; psubq Rx(dst), xmm15
        ; =>done
    );
}

fn guard_int32(ops: &mut Assembler, src: u8, bail: DynamicLabel) {
    dynasm!(ops
        ; .arch x64
        ; mov r11, Rq(src)
        ; shr r11, 48
        ; cmp r11w, NUMBER_TAG_HI16 as i16
        ; jne =>bail
    );
}

fn box_int32(ops: &mut Assembler, src: u8, dst: u8) {
    dynasm!(ops ; .arch x64 ; mov Rd(dst), Rd(src));
    load64(ops, 11, NUMBER_TAG);
    dynasm!(ops ; .arch x64 ; or Rq(dst), r11);
}

fn box_number(ops: &mut Assembler, src: u8, dst: u8) {
    let double = ops.new_dynamic_label();
    let tag = ops.new_dynamic_label();
    let ready = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; cvttsd2si r11d, Rx(src)
        ; cvtsi2sd xmm15, r11d
        ; ucomisd Rx(src), xmm15
        ; jp =>double
        ; jne =>double
        ; test r11d, r11d
        ; jne =>tag
        ; movq r11, Rx(src)
        ; test r11, r11
        ; js =>double
        ; =>tag
        ; mov Rd(dst), r11d
    );
    load64(ops, 11, NUMBER_TAG);
    dynasm!(ops ; .arch x64 ; or Rq(dst), r11 ; jmp =>done ; =>double);
    dynasm!(ops ; .arch x64 ; movq Rq(dst), Rx(src) ; ucomisd Rx(src), Rx(src) ; jnp =>ready);
    load64(ops, dst, CANONICAL_NAN);
    dynasm!(ops ; .arch x64 ; =>ready);
    load64(ops, 11, DOUBLE_OFFSET);
    dynasm!(ops ; .arch x64 ; add Rq(dst), r11 ; =>done);
}

fn box_uint32(ops: &mut Assembler, src: u8, dst: u8) {
    let double = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; test Rd(src), Rd(src) ; js =>double);
    box_int32(ops, src, dst);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>double ; mov r11d, Rd(src) ; cvtsi2sd xmm15, r11 ; movq Rq(dst), xmm15);
    load64(ops, 11, DOUBLE_OFFSET);
    dynasm!(ops ; .arch x64 ; add Rq(dst), r11 ; =>done);
}

fn box_bool(ops: &mut Assembler, src: u8, dst: u8) {
    let is_false = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; test Rd(src), Rd(src) ; jz =>is_false);
    load64(ops, dst, Value::boolean(true).to_bits());
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>is_false);
    load64(ops, dst, Value::boolean(false).to_bits());
    dynasm!(ops ; .arch x64 ; =>done);
}

fn poll(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    entry: u64,
    bailout: DynamicLabel,
    finish_error: DynamicLabel,
    fatal: DynamicLabel,
) {
    let batched = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    let interrupted = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; sub ebp, 1
        ; jne =>batched
        ; mov ebp, crate::GENERATED_POLL_BATCH as i32
        ; push r10
        ; mov r10, [r15 + THREAD_OFFSET as i32]
        ; mov r11, [r10 + VM_THREAD_INTERRUPT_CELL_OFFSET as i32]
        ; cmp BYTE [r11], 0
        ; jne =>interrupted
        ; mov r11, [r10 + VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET as i32]
        ; sub QWORD [r11], crate::GENERATED_POLL_BATCH as i32
        ; jg >fuel_ready
        ; pop r10
        ; mov rdi, r15
    );
    runtime(ops, relocations, entry, STUB_JIT_BACKEDGE_POLL);
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; cmp eax, NativeResultStatus::Success as i32
        ; je =>done
        ; cmp eax, NativeResultStatus::Yield as i32
        ; je =>done
        ; cmp eax, NativeResultStatus::Throw as i32
        ; je =>finish_error
        ; jmp =>fatal
        ; fuel_ready:
        ; pop r10
        ; =>done
        ; =>batched
        ; jmp >poll_complete
        ; =>interrupted
        ; pop r10
        ; jmp =>bailout
        ; poll_complete:
    );
}

fn binding_guard(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    frame: MachineFrameLayout,
    semantics: BindingSemantics,
    target: MachineBindingTarget,
    byte_pc: u32,
    locations: &[AllocatedLocation],
) -> Result<(), Unsupported> {
    if locations.len() != 3 + semantics.value_operands().into_iter().flatten().count() {
        return Err(Unsupported::OperandShape("x86-64 binding guard"));
    }
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    if matches!(
        semantics,
        BindingSemantics::Write(BindingWrite::GlobalChecked { .. })
    ) {
        load_integer(ops, frame, locations[4], 8)?;
        load64(ops, 11, Value::boolean(true).to_bits());
        dynasm!(ops ; .arch x64 ; cmp r8, r11 ; jne =>miss);
    }
    match target {
        MachineBindingTarget::Cold => dynasm!(ops ; .arch x64 ; jmp =>miss),
        MachineBindingTarget::GlobalThis => {
            dynasm!(ops
                ; .arch x64
                ; mov r9, [r15 + GLOBAL_THIS_OFFSET_PTR_OFFSET as i32]
                ; test r9, r9
                ; jz =>miss
                ; mov r10d, [r9]
                ; test r10d, r10d
                ; jz =>miss
            );
            symbolic(
                ops,
                relocations,
                11,
                view.cage_base as u64,
                RelocationTarget::GcCageBase,
            );
            dynasm!(ops ; .arch x64 ; add r10, r11);
        }
        MachineBindingTarget::Upvalue { index } => {
            let index = i32::try_from(index)
                .map_err(|_| Unsupported::OperandShape("x86-64 upvalue index"))?;
            let spine_offset = index
                .checked_mul(4)
                .ok_or(Unsupported::OperandShape("x86-64 upvalue spine offset"))?;
            dynasm!(ops
                ; .arch x64
                ; mov r11, [r15 + NATIVE_FRAME_OFFSET as i32]
                ; cmp DWORD [r11 + NATIVE_FRAME_UPVALUE_COUNT_OFFSET as i32], index
                ; jbe =>miss
                ; mov r10, [r11 + NATIVE_FRAME_UPVALUE_BASE_OFFSET as i32]
                ; test r10, r10
                ; jz =>miss
                ; mov r10d, [r10 + spine_offset]
                ; test r10d, r10d
                ; jz =>miss
            );
            symbolic(
                ops,
                relocations,
                9,
                view.cage_base as u64,
                RelocationTarget::GcCageBase,
            );
            dynasm!(ops
                ; .arch x64
                ; add r10, r9
                ; cmp BYTE [r10], UPVALUE_CELL_TYPE_TAG as i8
                ; jne =>miss
                ; lea r9, [r10 + view.upvalue_value_byte as i32]
            );
        }
        MachineBindingTarget::Global(otter_vm::jit::BindingHitProof::GlobalLexical {
            cell_offset,
            writable,
        }) => {
            if matches!(semantics, BindingSemantics::Write(_)) && !writable {
                return Err(Unsupported::OperandShape("x86-64 writable lexical binding"));
            }
            let cell_addr = view.cage_base.checked_add(cell_offset as usize).ok_or(
                Unsupported::OperandShape("x86-64 global lexical binding cell"),
            )?;
            symbolic(
                ops,
                relocations,
                10,
                cell_addr as u64,
                RelocationTarget::GlobalLexicalCell {
                    function_id: view.code_block.id,
                    byte_pc,
                },
            );
            dynasm!(ops ; .arch x64 ; lea r9, [r10 + view.upvalue_value_byte as i32]);
        }
        MachineBindingTarget::Global(otter_vm::jit::BindingHitProof::GlobalObject {
            shape,
            dictionary,
            value_byte,
            global_lexical_epoch,
            writable,
        }) => {
            if matches!(semantics, BindingSemantics::Write(_)) && !writable {
                return Err(Unsupported::OperandShape("x86-64 writable object binding"));
            }
            dynasm!(ops
                ; .arch x64
                ; mov r11, [r15 + THREAD_OFFSET as i32]
                ; mov r11, [r11 + VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET as i32]
                ; test r11, r11
                ; jz =>miss
                ; mov r8, [r11]
            );
            load64(ops, 11, global_lexical_epoch);
            dynasm!(ops
                ; .arch x64
                ; cmp r8, r11
                ; jne =>miss
                ; mov r11, [r15 + GLOBAL_THIS_OFFSET_PTR_OFFSET as i32]
                ; test r11, r11
                ; jz =>miss
                ; mov r10d, [r11]
            );
            symbolic(
                ops,
                relocations,
                11,
                view.cage_base as u64,
                RelocationTarget::GcCageBase,
            );
            dynasm!(ops ; .arch x64 ; add r11, r10);
            if dictionary {
                dynasm!(ops
                    ; .arch x64
                    ; cmp DWORD [r11 + view.object_shape_byte as i32], 0
                    ; jne =>miss
                );
                load64(ops, 8, shape);
                dynasm!(ops
                    ; .arch x64
                    ; cmp [r11 + view.object_dictionary_shape_id_byte as i32], r8
                    ; jne =>miss
                );
            } else {
                dynasm!(ops
                    ; .arch x64
                    ; cmp DWORD [r11 + view.object_shape_byte as i32], shape as i32
                    ; jne =>miss
                );
            }
            slab_base(ops, view, miss);
            dynasm!(ops
                ; .arch x64
                ; mov r10, r11
                ; lea r9, [r8 + value_byte as i32]
            );
        }
    }
    if binding_requires_live_cell(semantics) {
        dynasm!(ops ; .arch x64 ; mov r11, [r9]);
        load64(ops, 8, Value::hole().to_bits());
        dynasm!(ops ; .arch x64 ; cmp r11, r8 ; je =>miss);
    }
    store_integer(ops, frame, locations[1], 10)?;
    store_integer(ops, frame, locations[2], 9)?;
    dynasm!(ops ; .arch x64 ; mov r8d, 1);
    store_integer(ops, frame, locations[0], 8)?;
    dynasm!(ops
        ; .arch x64
        ; jmp =>done
        ; =>miss
        ; xor r8d, r8d
        ; xor r9d, r9d
        ; xor r10d, r10d
    );
    store_integer(ops, frame, locations[0], 8)?;
    store_integer(ops, frame, locations[1], 10)?;
    store_integer(ops, frame, locations[2], 9)?;
    dynasm!(ops ; .arch x64 ; =>done);
    Ok(())
}

fn binding_hit(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    semantics: BindingSemantics,
    target: MachineBindingTarget,
    locations: &[AllocatedLocation],
) -> Result<(), Unsupported> {
    if matches!(target, MachineBindingTarget::Cold) {
        return Err(Unsupported::OperandShape("x86-64 binding hit target"));
    }
    match semantics {
        BindingSemantics::Read(read) if locations.len() == 3 => {
            match read {
                BindingRead::GlobalThis { .. } => {
                    load_integer(ops, frame, locations[0], 11)?;
                }
                BindingRead::Exists { .. } => {
                    load64(ops, 11, Value::boolean(true).to_bits());
                }
                BindingRead::Global { .. } | BindingRead::Upvalue { .. } => {
                    load_integer(ops, frame, locations[1], 11)?;
                    dynasm!(ops ; .arch x64 ; mov r11, [r11]);
                }
                BindingRead::Dynamic { .. }
                | BindingRead::ShadowedUpvalue { .. }
                | BindingRead::EvalBindingSeq { .. } => {
                    return Err(Unsupported::OperandShape("x86-64 dynamic binding hit"));
                }
            }
            store_integer(ops, frame, locations[2], 11)?;
            Ok(())
        }
        BindingSemantics::Write(_) if locations.len() == 3 => {
            load_integer(ops, frame, locations[1], 11)?;
            load_integer(ops, frame, locations[2], 10)?;
            dynasm!(ops ; .arch x64 ; mov [r11], r10);
            Ok(())
        }
        _ => Err(Unsupported::OperandShape("x86-64 binding hit")),
    }
}

fn binding_requires_live_cell(semantics: BindingSemantics) -> bool {
    matches!(
        semantics,
        BindingSemantics::Read(BindingRead::Global { .. } | BindingRead::Upvalue { .. })
            | BindingSemantics::Write(
                BindingWrite::Global { .. } | BindingWrite::GlobalChecked { .. }
            )
            | BindingSemantics::Write(BindingWrite::Upvalue {
                check: BindingWriteCheck::Checked,
                ..
            })
    )
}

#[allow(clippy::too_many_arguments)]
fn literal_allocation_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    sequence: &InstructionSequence,
    frame: MachineFrameLayout,
    instruction: &super::super::MachineInstruction,
    id: MachineInstructionId,
    locations: &[AllocatedLocation],
    safepoints: &MachineSafepointTable,
    target: RuntimeStubDescriptor,
    logical_pc: u32,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let descriptor_index = match instruction.opcode {
        MachineOpcode::Call(index) => index as usize,
        _ => unreachable!("literal allocation helper only receives calls"),
    };
    let descriptor = &sequence.call_descriptors()[descriptor_index];
    if target.signature != RuntimeStubSignature::ReentrantValueSpan
        || target.result_abi != RuntimeStubResultAbi::NativePair
        || target.result_domain != NativeResultDomain::Committed
        || descriptor.results != [MachineRepresentation::Tagged]
        || descriptor.exceptional != super::super::ExceptionalEdge::None
        || !instruction.exits.is_empty()
    {
        return Err(Unsupported::OperandShape(
            "x86-64 literal allocation contract",
        ));
    }
    let arguments = instruction
        .operands
        .iter()
        .filter(|operand| operand.purpose == super::super::OperandPurpose::Input)
        .map(|operand| operand.value)
        .collect::<Vec<_>>();
    if arguments.len() != descriptor.arguments.len() {
        return Err(Unsupported::OperandShape("x86-64 literal allocation arity"));
    }
    let result_index = instruction
        .operands
        .iter()
        .position(|operand| operand.purpose == super::super::OperandPurpose::Output)
        .ok_or(Unsupported::OperandShape(
            "x86-64 literal allocation result",
        ))?;
    let result_location = *locations
        .get(result_index)
        .ok_or(Unsupported::OperandShape(
            "x86-64 literal allocation result allocation",
        ))?;
    let site = safepoints
        .site(id)
        .filter(|site| instruction.safepoint == Some(site.id))
        .ok_or(Unsupported::OperandShape(
            "x86-64 literal allocation safepoint",
        ))?;

    save_roots(ops, frame, site)?;
    publish_roots(ops, frame, site)?;
    let count = u16::try_from(arguments.len())
        .map_err(|_| Unsupported::OperandShape("x86-64 value-span length"))?;
    if count == 0 {
        dynasm!(ops ; .arch x64 ; xor esi, esi ; xor edx, edx);
    } else {
        let packet = super::value_packet_frame(sequence)?;
        let end = packet
            .raw_start
            .checked_add(count)
            .ok_or(Unsupported::OperandShape("x86-64 value-span extent"))?;
        if count > packet.raw_words || end > frame.raw_slots() {
            return Err(Unsupported::OperandShape("x86-64 value-span capacity"));
        }
        for (index, value) in arguments.iter().copied().enumerate() {
            load_saved_root(ops, frame, site, value, 10)?;
            let offset = frame
                .raw_offset(packet.raw_start + index as u16)
                .map_err(|_| Unsupported::OperandShape("x86-64 value-span slot"))?
                .checked_add(MACHINE_ROOT_RECORD_SIZE)
                .and_then(|offset| i32::try_from(offset).ok())
                .ok_or(Unsupported::OperandShape("x86-64 value-span slot"))?;
            dynasm!(ops ; .arch x64 ; mov [rsp + offset], r10);
        }
        let offset = frame
            .raw_offset(packet.raw_start)
            .map_err(|_| Unsupported::OperandShape("x86-64 value-span base"))?
            .checked_add(MACHINE_ROOT_RECORD_SIZE)
            .and_then(|offset| i32::try_from(offset).ok())
            .ok_or(Unsupported::OperandShape("x86-64 value-span base"))?;
        dynasm!(ops ; .arch x64 ; lea rsi, [rsp + offset] ; mov edx, count as i32);
    }
    dynasm!(ops
        ; .arch x64
        ; mov r11, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov DWORD [r11 + NATIVE_FRAME_PC_OFFSET as i32], logical_pc as i32
        ; mov rdi, r15
    );
    runtime(ops, relocations, transitions.entry(target), target);
    dynasm!(ops ; .arch x64 ; call r11 ; mov r8, rax ; mov r9, rdx);
    clear_roots(ops);
    reload_roots(ops, frame, site)?;
    dynasm!(ops ; .arch x64 ; test r9, r9 ; jne =>fatal);
    store_integer(ops, frame, result_location, 8)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn committed_value_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    _sequence: &InstructionSequence,
    frame: MachineFrameLayout,
    instruction: &super::super::MachineInstruction,
    id: MachineInstructionId,
    descriptor: &CallDescriptor,
    locations: &[AllocatedLocation],
    allocation_edits: &[AllocationEdit],
    safepoints: &MachineSafepointTable,
    block_labels: &[DynamicLabel],
    throw_value: DynamicLabel,
    fatal_exit: DynamicLabel,
) -> Result<(), Unsupported> {
    let CallTarget::CommittedRuntime {
        target,
        logical_pc,
        semantic_arity,
        ..
    } = descriptor.target
    else {
        return Err(Unsupported::OperandShape("x86-64 committed runtime target"));
    };
    let semantic_arity = usize::from(semantic_arity);
    if semantic_arity > 2
        || target.signature != RuntimeStubSignature::CommittedValue2
        || target.result_abi != RuntimeStubResultAbi::NativePair
        || target.result_domain != NativeResultDomain::Committed
        || descriptor.arguments.len() != semantic_arity
        || descriptor.results != [MachineRepresentation::Tagged]
        || !instruction.exits.is_empty()
    {
        return Err(Unsupported::OperandShape(
            "x86-64 committed runtime contract",
        ));
    }
    if descriptor.exceptional == ExceptionalEdge::None {
        return Err(Unsupported::OperandShape(
            "x86-64 committed runtime exceptional edge",
        ));
    }
    let arguments = instruction
        .operands
        .iter()
        .filter(|operand| operand.purpose == super::super::OperandPurpose::Input)
        .map(|operand| operand.value)
        .collect::<Vec<_>>();
    if arguments.len() != semantic_arity {
        return Err(Unsupported::OperandShape(
            "x86-64 committed runtime semantic arity",
        ));
    }
    let result_operand = instruction
        .operands
        .iter()
        .position(|operand| operand.purpose == super::super::OperandPurpose::Output)
        .ok_or(Unsupported::OperandShape("x86-64 committed runtime result"))?;
    let result_location = *locations
        .get(result_operand)
        .ok_or(Unsupported::OperandShape(
            "x86-64 committed runtime result allocation",
        ))?;
    let site = safepoints
        .site(id)
        .filter(|site| instruction.safepoint == Some(site.id))
        .ok_or(Unsupported::OperandShape(
            "x86-64 committed runtime safepoint",
        ))?;

    save_roots(ops, frame, site)?;
    publish_roots(ops, frame, site)?;
    load64(ops, 6, VALUE_UNDEFINED);
    load64(ops, 2, VALUE_UNDEFINED);
    for (index, value) in arguments.iter().copied().enumerate() {
        load_saved_root(ops, frame, site, value, [6_u8, 2][index])?;
    }
    dynasm!(ops
        ; .arch x64
        ; mov r11, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov DWORD [r11 + NATIVE_FRAME_PC_OFFSET as i32], logical_pc as i32
        ; mov rdi, r15
    );
    runtime(ops, relocations, transitions.entry(target), target);
    let completed = ops.new_dynamic_label();
    let js_throw = ops.new_dynamic_label();
    let fatal = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; call r11
        // r11 is outside the allocator bank and survives root cleanup.
        ; mov r11, rax
        ; test rdx, rdx
        ; jz =>completed
        ; cmp edx, NativeResultStatus::Throw as i32
        ; je =>js_throw
        ; jmp =>fatal
        ; =>js_throw
    );
    clear_roots_preserving_r11(ops);
    reload_roots_preserving_r11(ops, frame, site)?;
    match descriptor.exceptional {
        ExceptionalEdge::LandingPad(target) => {
            store_integer(ops, frame, result_location, 11)?;
            edits(ops, allocation_edits, AllocationPoint::After(id), frame)?;
            let target =
                block_labels
                    .get(target.0 as usize)
                    .copied()
                    .ok_or(Unsupported::OperandShape(
                        "x86-64 committed runtime landing pad",
                    ))?;
            dynasm!(ops ; .arch x64 ; jmp =>target);
        }
        ExceptionalEdge::Propagate => {
            dynasm!(ops ; .arch x64 ; mov rax, r11 ; jmp =>throw_value);
        }
        ExceptionalEdge::None => unreachable!("validated above"),
    }
    dynasm!(ops ; .arch x64 ; =>fatal);
    clear_roots_preserving_r11(ops);
    reload_roots_preserving_r11(ops, frame, site)?;
    dynasm!(ops ; .arch x64 ; jmp =>fatal_exit ; =>completed);
    clear_roots_preserving_r11(ops);
    reload_roots_preserving_r11(ops, frame, site)?;
    store_integer(ops, frame, result_location, 11)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn committed_pair_call(
    view: &JitCompileSnapshot,
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    sequence: &InstructionSequence,
    frame: MachineFrameLayout,
    instruction: &super::super::MachineInstruction,
    id: MachineInstructionId,
    locations: &[AllocatedLocation],
    safepoints: &MachineSafepointTable,
    target: RuntimeStubDescriptor,
    logical_pc: u32,
    semantic_arity: u8,
) -> Result<(), Unsupported> {
    let logical_pc = if let Some(parent) = instruction.inline_frames.first() {
        super::frame_state::suspended_call_pc(view, parent.function_id, parent.byte_pc)
            .ok_or(Unsupported::OperandShape(
                "x86-64 inline caller publication PC",
            ))?
            .0
    } else {
        logical_pc
    };
    let semantic_arity = usize::from(semantic_arity);
    let named_store = target == otter_vm::native_abi::STUB_JIT_STORE_PROPERTY;
    let named_property = target == otter_vm::native_abi::STUB_JIT_LOAD_PROPERTY || named_store;
    let element_store = target == otter_vm::native_abi::STUB_JIT_STORE_ELEMENT;
    let element = target == otter_vm::native_abi::STUB_JIT_LOAD_ELEMENT || element_store;
    let value2 = target.signature == RuntimeStubSignature::CommittedValue2;
    if (!named_property && !element && !value2)
        || semantic_arity > if named_store || element_store { 3 } else { 2 }
        || target.result_abi != RuntimeStubResultAbi::NativePair
    {
        return Err(Unsupported::OperandShape(
            "x86-64 committed pair runtime contract",
        ));
    }
    let descriptor_index = match instruction.opcode {
        MachineOpcode::Call(index) => index as usize,
        _ => unreachable!("committed call helper only receives calls"),
    };
    let descriptor = &sequence.call_descriptors()[descriptor_index];
    if descriptor.arguments.len() != semantic_arity || descriptor.results.len() != 2 {
        return Err(Unsupported::OperandShape("x86-64 committed pair arity"));
    }
    let outputs = instruction
        .operands
        .iter()
        .enumerate()
        .filter(|(_, operand)| operand.purpose == super::super::OperandPurpose::Output)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let [payload_index, status_index] = outputs.as_slice() else {
        return Err(Unsupported::OperandShape("x86-64 committed pair outputs"));
    };
    let site = safepoints
        .site(id)
        .filter(|site| instruction.safepoint == Some(site.id))
        .ok_or(Unsupported::OperandShape("x86-64 committed pair safepoint"))?;

    save_roots(ops, frame, site)?;
    publish_roots(ops, frame, site)?;
    load64(ops, 6, VALUE_UNDEFINED);
    load64(ops, 2, VALUE_UNDEFINED);
    for index in 0..semantic_arity {
        let destination = [6_u8, 2, 1][index];
        if named_property && index + 1 == semantic_arity {
            load_integer_with_bias(
                ops,
                frame,
                locations[index],
                destination,
                MACHINE_ROOT_RECORD_SIZE,
            )?;
        } else {
            let value = instruction.operands[index].value;
            load_saved_root(ops, frame, site, value, destination)?;
        }
    }
    dynasm!(ops
        ; .arch x64
        ; mov r11, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov DWORD [r11 + NATIVE_FRAME_PC_OFFSET as i32], logical_pc as i32
        ; mov rdi, r15
    );
    runtime(ops, relocations, transitions.entry(target), target);
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; mov r8, rax
        ; mov r9, rdx
    );
    clear_roots(ops);
    reload_roots(ops, frame, site)?;
    store_integer(ops, frame, locations[*payload_index], 8)?;
    store_integer(ops, frame, locations[*status_index], 9)?;
    Ok(())
}

fn publish_roots(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
) -> Result<(), Unsupported> {
    dynasm!(ops ; .arch x64 ; sub rsp, MACHINE_ROOT_RECORD_SIZE as i32);
    if site.roots.is_empty() {
        dynasm!(ops ; .arch x64 ; xor r10d, r10d);
    } else {
        let offset = root_offset(frame, 0)?
            .checked_add(MACHINE_ROOT_RECORD_SIZE)
            .and_then(|offset| i32::try_from(offset).ok())
            .ok_or(Unsupported::OperandShape("x86-64 machine root base"))?;
        dynasm!(ops ; .arch x64 ; lea r10, [rsp + offset]);
    }
    dynasm!(ops
        ; .arch x64
        ; mov r11, [r15 + MACHINE_ROOTS_PTR_OFFSET as i32]
        ; mov rax, [r11]
        ; mov [rsp + MACHINE_ROOT_RECORD_PREVIOUS_OFFSET as i32], rax
        ; mov [rsp + MACHINE_ROOT_RECORD_BASE_OFFSET as i32], r10
        ; mov DWORD [rsp + MACHINE_ROOT_RECORD_COUNT_OFFSET as i32], site.roots.len() as i32
        ; mov DWORD [rsp + MACHINE_ROOT_RECORD_SAFEPOINT_ID_OFFSET as i32], site.id.0 as i32
        ; mov rax, [r15 + THREAD_OFFSET as i32]
        ; mov rax, [rax + VM_THREAD_CODE_OBJECT_ID_OFFSET as i32]
        ; mov [rsp + MACHINE_ROOT_RECORD_CODE_OBJECT_ID_OFFSET as i32], rax
        ; mov [r11], rsp
    );
    Ok(())
}

fn clear_roots(ops: &mut Assembler) {
    dynasm!(ops
        ; .arch x64
        ; mov r11, [r15 + MACHINE_ROOTS_PTR_OFFSET as i32]
        ; mov rax, [rsp + MACHINE_ROOT_RECORD_PREVIOUS_OFFSET as i32]
        ; mov [r11], rax
        ; add rsp, MACHINE_ROOT_RECORD_SIZE as i32
    );
}

fn clear_roots_preserving_r11(ops: &mut Assembler) {
    dynasm!(ops
        ; .arch x64
        ; mov r10, [r15 + MACHINE_ROOTS_PTR_OFFSET as i32]
        ; mov rax, [rsp + MACHINE_ROOT_RECORD_PREVIOUS_OFFSET as i32]
        ; mov [r10], rax
        ; add rsp, MACHINE_ROOT_RECORD_SIZE as i32
    );
}

fn load_saved_root(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
    value: super::super::MachineValue,
    destination: u8,
) -> Result<(), Unsupported> {
    let root =
        site.roots
            .iter()
            .find(|root| root.value == value)
            .ok_or(Unsupported::OperandShape(
                "x86-64 canonical safepoint argument root",
            ))?;
    let offset = root_offset(frame, root.save_slot)?
        .checked_add(MACHINE_ROOT_RECORD_SIZE)
        .and_then(|offset| i32::try_from(offset).ok())
        .ok_or(Unsupported::OperandShape(
            "x86-64 safepoint argument root offset",
        ))?;
    dynasm!(ops ; .arch x64 ; mov Rq(destination), [rsp + offset]);
    Ok(())
}

fn allocating_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
    entry: u64,
    target: RuntimeStubDescriptor,
    exit: DynamicLabel,
) -> Result<(), Unsupported> {
    save_roots(ops, frame, site)?;
    dynasm!(ops
        ; .arch x64
        ; sub rsp, ALLOC_CTX_STACK_SIZE as i32
        ; mov r11, [r15 + THREAD_OFFSET as i32]
        ; mov [rsp + ALLOC_CTX_THREAD_OFFSET as i32], r11
        ; mov DWORD [rsp + ALLOC_CTX_SAFEPOINT_ID_OFFSET as i32], site.id.0 as i32
    );
    if frame.root_slots() == 0 {
        dynasm!(ops
            ; .arch x64
            ; mov QWORD [rsp + ALLOC_CTX_SPILL_SLOTS_OFFSET as i32], 0
            ; mov WORD [rsp + ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET as i32], 0
        );
    } else {
        let root_base = ALLOC_CTX_STACK_SIZE
            .checked_add(root_offset(frame, 0)?)
            .and_then(|offset| i32::try_from(offset).ok())
            .ok_or(Unsupported::OperandShape("x86-64 root-save base"))?;
        dynasm!(ops
            ; .arch x64
            ; lea r11, [rsp + root_base]
            ; mov [rsp + ALLOC_CTX_SPILL_SLOTS_OFFSET as i32], r11
            ; mov WORD [rsp + ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET as i32], frame.root_slots() as i16
        );
    }
    dynasm!(ops ; .arch x64 ; mov rdi, rsp ; mov esi, site.id.0 as i32);
    runtime(ops, relocations, entry, target);
    dynasm!(ops ; .arch x64 ; call r11 ; add rsp, ALLOC_CTX_STACK_SIZE as i32);
    reload_roots(ops, frame, site)?;
    dynasm!(ops ; .arch x64 ; test rdx, rdx ; jne =>exit);
    Ok(())
}

fn save_roots(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
) -> Result<(), Unsupported> {
    for root in &site.roots {
        let destination = i32::try_from(root_offset(frame, root.save_slot)?)
            .map_err(|_| Unsupported::OperandShape("x86-64 root offset"))?;
        match root.source {
            AllocatedLocation::Register(register) if register.is_integer() => {
                dynasm!(ops ; .arch x64 ; mov [rsp + destination], Rq(register.encoding()));
            }
            AllocatedLocation::Stack(slot) => {
                let source = spill(frame, slot)?;
                dynasm!(ops ; .arch x64 ; mov r11, [rsp + source] ; mov [rsp + destination], r11);
            }
            _ => return Err(Unsupported::OperandShape("x86-64 tagged root source")),
        }
    }
    Ok(())
}

fn reload_roots(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
) -> Result<(), Unsupported> {
    for root in &site.roots {
        let source = i32::try_from(root_offset(frame, root.save_slot)?)
            .map_err(|_| Unsupported::OperandShape("x86-64 root offset"))?;
        match root.source {
            AllocatedLocation::Register(register) if register.is_integer() => {
                dynasm!(ops ; .arch x64 ; mov Rq(register.encoding()), [rsp + source]);
            }
            AllocatedLocation::Stack(slot) => {
                let destination = spill(frame, slot)?;
                dynasm!(ops ; .arch x64 ; mov r11, [rsp + source] ; mov [rsp + destination], r11);
            }
            _ => return Err(Unsupported::OperandShape("x86-64 tagged root reload")),
        }
    }
    Ok(())
}

fn reload_roots_preserving_r11(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
) -> Result<(), Unsupported> {
    for root in &site.roots {
        let source = i32::try_from(root_offset(frame, root.save_slot)?)
            .map_err(|_| Unsupported::OperandShape("x86-64 root offset"))?;
        match root.source {
            AllocatedLocation::Register(register) if register.is_integer() => {
                dynasm!(ops ; .arch x64 ; mov Rq(register.encoding()), [rsp + source]);
            }
            AllocatedLocation::Stack(slot) => {
                let destination = spill(frame, slot)?;
                dynasm!(ops ; .arch x64 ; mov rax, [rsp + source] ; mov [rsp + destination], rax);
            }
            _ => return Err(Unsupported::OperandShape("x86-64 tagged root reload")),
        }
    }
    Ok(())
}

fn root_offset(frame: MachineFrameLayout, slot: u16) -> Result<u32, Unsupported> {
    frame
        .root_offset(slot)
        .map_err(|_| Unsupported::OperandShape("x86-64 root-save offset"))
}

#[allow(clippy::too_many_arguments)]
fn emit_cold_exits(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    frame: MachineFrameLayout,
    saved: SavedFrame,
    transitions: &TransitionTable,
    bail: DynamicLabel,
    finish_error: DynamicLabel,
    fatal: DynamicLabel,
    register_count: u16,
) {
    dynasm!(ops
        ; .arch x64
        ; =>bail
        ; mov r11, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov DWORD [r11 + NATIVE_FRAME_PC_OFFSET as i32], 0
    );
    materialize_vm_window(ops, register_count);
    load64(
        ops,
        0,
        SideExit::new(0, ExitReason::TypeMismatch, ExitAction::Recompile).to_bits(),
    );
    dynasm!(ops ; .arch x64 ; mov edx, NativeResultStatus::SideExit as i32);
    epilogue(ops, frame, saved);

    let pair_exit = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; =>finish_error ; mov rdi, r15);
    runtime(
        ops,
        relocations,
        transitions.entry(STUB_JIT_FINISH_ERROR),
        STUB_JIT_FINISH_ERROR,
    );
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; cmp edx, NativeResultStatus::SideExit as i32
        ; je =>pair_exit
        ; cmp edx, NativeResultStatus::Throw as i32
        ; je =>pair_exit
        ; cmp edx, NativeResultStatus::Fatal as i32
        ; je =>pair_exit
        ; =>fatal
    );
    load64(ops, 0, VALUE_UNDEFINED);
    dynasm!(ops ; .arch x64 ; mov edx, NativeResultStatus::Fatal as i32 ; =>pair_exit);
    epilogue(ops, frame, saved);
}

fn materialize_vm_window(ops: &mut Assembler, register_count: u16) {
    let done = ops.new_dynamic_label();
    let loop_label = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; movzx r11d, WORD [r10 + NATIVE_FRAME_REGISTER_COUNT_OFFSET as i32]
        ; cmp r11d, register_count as i32
        ; je =>done
        ; mov rax, [r10 + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32]
        ; lea rax, [rax + r11 * 8]
        ; mov ecx, register_count as i32
        ; sub ecx, r11d
    );
    load64(ops, 11, VALUE_UNDEFINED);
    dynasm!(ops
        ; .arch x64
        ; =>loop_label
        ; mov [rax], r11
        ; add rax, 8
        ; sub ecx, 1
        ; jne =>loop_label
        ; mov WORD [r10 + NATIVE_FRAME_REGISTER_COUNT_OFFSET as i32], register_count as i16
        ; =>done
    );
}

#[allow(clippy::too_many_arguments)]
fn emit_deopts(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    frame: MachineFrameLayout,
    saved: SavedFrame,
    runtime_data: &DeoptRuntime,
    entry: u64,
    labels: &[DynamicLabel],
    shared: DynamicLabel,
) -> Result<(), Unsupported> {
    if labels.is_empty() {
        return Ok(());
    }
    for (index, &label) in labels.iter().enumerate() {
        let index = i32::try_from(index)
            .ok()
            .filter(|index| *index <= i32::from(u16::MAX))
            .ok_or(Unsupported::OperandShape("x86-64 deopt count"))?;
        dynasm!(ops ; .arch x64 ; =>label ; mov r11d, index ; jmp =>shared);
    }
    dynasm!(ops ; .arch x64 ; =>shared ; sub rsp, DEOPT_DUMP_BYTES);
    for register in 0_u8..16 {
        let offset = i32::from(register) * 8;
        dynasm!(ops ; .arch x64 ; mov [rsp + offset], Rq(register));
    }
    for register in 0_u8..16 {
        let offset = DEOPT_BANK_BYTES + i32::from(register) * 8;
        dynasm!(ops ; .arch x64 ; movsd [rsp + offset], Rx(register));
    }
    dynasm!(ops ; .arch x64 ; mov rdi, r15 ; mov esi, r11d);
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
        ; mov r9, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov r9, [r9 + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32]
    );
    runtime(ops, relocations, entry, STUB_JIT_DEOPT_WRITEBACK);
    dynasm!(ops ; .arch x64 ; call r11 ; add rsp, DEOPT_DUMP_BYTES);
    epilogue(ops, frame, saved);
    Ok(())
}

fn emit_osr_entries(
    ops: &mut Assembler,
    sequence: &InstructionSequence,
    allocation: &AllocatedSequence,
    frame: MachineFrameLayout,
    saved: SavedFrame,
    has_poll: bool,
    sites: &[OsrSite],
) -> Result<OsrEmission, Unsupported> {
    let mut entries = BTreeMap::new();
    let mut regions = Vec::with_capacity(sites.len());
    for site in sites {
        let start = ops.offset().0;
        let reject = ops.new_dynamic_label();
        prologue(ops, frame, saved);
        if has_poll {
            dynasm!(ops ; .arch x64 ; mov ebp, crate::GENERATED_POLL_BATCH as i32);
        }
        let locations = allocation
            .instruction_locations(site.instruction)
            .ok_or(Unsupported::OperandShape("x86-64 OSR allocation"))?;
        let instruction = &sequence.instructions()[site.instruction.0 as usize];
        if site.inputs.len() != locations.len() || locations.len() != instruction.operands.len() {
            return Err(Unsupported::OperandShape("x86-64 OSR arity"));
        }
        for ((input, &location), operand) in
            site.inputs.iter().zip(locations).zip(&instruction.operands)
        {
            osr_value(
                ops,
                frame,
                *input,
                sequence.representations()[operand.value.0 as usize],
                location,
                reject,
            )?;
        }
        let continuation = site.continuation;
        dynasm!(ops ; .arch x64 ; jmp =>continuation ; =>reject);
        dynasm!(ops
            ; .arch x64
            ; mov r11, [r15 + NATIVE_FRAME_OFFSET as i32]
            ; mov DWORD [r11 + NATIVE_FRAME_PC_OFFSET as i32], site.logical_pc as i32
        );
        load64(
            ops,
            0,
            SideExit::new(
                site.logical_pc,
                ExitReason::TypeMismatch,
                ExitAction::Recompile,
            )
            .to_bits(),
        );
        dynasm!(ops ; .arch x64 ; mov edx, NativeResultStatus::SideExit as i32);
        epilogue(ops, frame, saved);
        let end = ops.offset().0;
        if entries.insert(site.logical_pc, start).is_some() {
            return Err(Unsupported::OperandShape("duplicate x86-64 OSR logical PC"));
        }
        regions.push((site.logical_pc, start, end));
    }
    Ok((entries, regions))
}

fn osr_value(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    input: MachineOsrInput,
    representation: MachineRepresentation,
    location: AllocatedLocation,
    reject: DynamicLabel,
) -> Result<(), Unsupported> {
    let expected = match input.value_type {
        MachineOsrType::Tagged => MachineRepresentation::Tagged,
        MachineOsrType::Int32 => MachineRepresentation::Int32,
        MachineOsrType::Uint32 => MachineRepresentation::Uint32,
        MachineOsrType::Float64 => MachineRepresentation::Float64,
        MachineOsrType::Boolean => MachineRepresentation::Boolean,
    };
    if representation != expected {
        return Err(Unsupported::OperandShape("x86-64 OSR representation"));
    }
    load_osr_source(ops, input.frame_register);
    match input.value_type {
        MachineOsrType::Tagged => store_osr_integer(ops, frame, location, 11),
        MachineOsrType::Int32 => {
            guard_int32(ops, 11, reject);
            load_osr_source(ops, input.frame_register);
            store_osr_integer(ops, frame, location, 11)
        }
        MachineOsrType::Float64 => {
            decode_osr_number(ops, reject);
            store_osr_float(ops, frame, location, 15)
        }
        MachineOsrType::Uint32 => {
            decode_osr_number(ops, reject);
            let bad = ops.new_dynamic_label();
            let ready = ops.new_dynamic_label();
            dynasm!(ops
                ; .arch x64
                ; sub rsp, 8
                ; movsd [rsp], xmm15
                ; xorpd xmm15, xmm15
                ; ucomisd xmm15, [rsp]
                ; ja =>bad
                ; jp =>bad
            );
            load64(ops, 11, f64::from(u32::MAX).to_bits());
            dynasm!(ops
                ; .arch x64
                ; movq xmm15, r11
                ; ucomisd xmm15, [rsp]
                ; jb =>bad
                ; movsd xmm15, [rsp]
                ; cvttsd2si r11, xmm15
                ; cvtsi2sd xmm15, r11
                ; ucomisd xmm15, [rsp]
                ; jne =>bad
                ; add rsp, 8
                ; jmp =>ready
                ; =>bad
                ; add rsp, 8
                ; jmp =>reject
                ; =>ready
            );
            store_osr_integer(ops, frame, location, 11)
        }
        MachineOsrType::Boolean => {
            let is_true = ops.new_dynamic_label();
            let ready = ops.new_dynamic_label();
            let bad = ops.new_dynamic_label();
            dynasm!(ops ; .arch x64 ; push r10);
            load64(ops, 10, Value::boolean(true).to_bits());
            dynasm!(ops ; .arch x64 ; cmp r11, r10 ; je =>is_true);
            load64(ops, 10, Value::boolean(false).to_bits());
            dynasm!(ops
                ; .arch x64
                ; cmp r11, r10
                ; jne =>bad
                ; xor r11d, r11d
                ; jmp =>ready
                ; =>is_true
                ; mov r11d, 1
                ; =>ready
                ; pop r10
                ; jmp >store
                ; =>bad
                ; pop r10
                ; jmp =>reject
                ; store:
            );
            store_osr_integer(ops, frame, location, 11)
        }
    }
}

fn decode_osr_number(ops: &mut Assembler, reject: DynamicLabel) {
    let double = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    let bad = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; push r10
        ; mov r10, r11
        ; shr r11, 48
        ; cmp r11w, NUMBER_TAG_HI16 as i16
        ; jne =>double
        ; cvtsi2sd xmm15, r10d
        ; jmp =>done
        ; =>double
        ; test r11w, NUMBER_TAG_HI16 as i16
        ; jz =>bad
    );
    load64(ops, 11, DOUBLE_OFFSET);
    dynasm!(ops
        ; .arch x64
        ; sub r10, r11
        ; movq xmm15, r10
        ; =>done
        ; pop r10
        ; jmp >ready
        ; =>bad
        ; pop r10
        ; jmp =>reject
        ; ready:
    );
}

fn load_osr_source(ops: &mut Assembler, register: u16) {
    let offset = i32::from(register) * 8;
    dynasm!(ops
        ; .arch x64
        ; mov r11, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov r11, [r11 + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32]
        ; mov r11, [r11 + offset]
    );
}

fn store_osr_integer(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    location: AllocatedLocation,
    source: u8,
) -> Result<(), Unsupported> {
    match location {
        AllocatedLocation::Register(register) if register.is_integer() => {
            dynasm!(ops ; .arch x64 ; mov Rq(register.encoding()), Rq(source));
        }
        AllocatedLocation::Stack(slot) => {
            let offset = spill(frame, slot)?;
            dynasm!(ops ; .arch x64 ; mov [rsp + offset], Rq(source));
        }
        _ => return Err(Unsupported::OperandShape("x86-64 OSR integer location")),
    }
    Ok(())
}

fn store_osr_float(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    location: AllocatedLocation,
    source: u8,
) -> Result<(), Unsupported> {
    match location {
        AllocatedLocation::Register(register) if register.is_float() => {
            dynasm!(ops ; .arch x64 ; movsd Rx(register.encoding()), Rx(source));
        }
        AllocatedLocation::Stack(slot) => {
            let offset = spill(frame, slot)?;
            dynasm!(ops ; .arch x64 ; movsd [rsp + offset], Rx(source));
        }
        _ => return Err(Unsupported::OperandShape("x86-64 OSR float location")),
    }
    Ok(())
}

fn store_const(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    location: AllocatedLocation,
    value: u64,
) -> Result<(), Unsupported> {
    match location {
        AllocatedLocation::Register(r) if r.is_integer() => load64(ops, r.encoding(), value),
        AllocatedLocation::Stack(slot) => {
            load64(ops, 11, value);
            let offset = spill(frame, slot)?;
            dynasm!(ops ; .arch x64 ; mov [rsp + offset], r11);
        }
        _ => return Err(Unsupported::OperandShape("x86-64 constant output")),
    }
    Ok(())
}

fn emit_inline_identity(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    function_id: u32,
    miss: DynamicLabel,
) {
    let guarded = ops.new_dynamic_label();
    load64(ops, 10, otter_vm::value::tag::box_function_id(function_id));
    dynasm!(ops
        ; .arch x64
        ; cmp r9, r10
        ; je =>guarded
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
        ; cmp r8d, JS_CLOSURE_BODY_TYPE_TAG as i32
        ; jne =>miss
    );
    if view.closure_call_layout.runtime_setup_flags != 0 {
        dynasm!(ops
            ; .arch x64
            ; mov r8d, [r9 + view.closure_call_layout.flags_byte as i32]
        );
        load64(
            ops,
            11,
            u64::from(view.closure_call_layout.runtime_setup_flags),
        );
        dynasm!(ops ; .arch x64 ; test r8d, r11d ; jnz =>miss);
    }
    dynasm!(ops
        ; .arch x64
        ; cmp DWORD [r9 + view.closure_call_layout.eval_env_byte as i32], 0
        ; jne =>miss
        ; cmp DWORD [r9 + view.closure_call_layout.function_id_byte as i32], function_id as i32
        ; jne =>miss
        ; =>guarded
    );
}

fn emit_inline_method_guard(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    guard: &otter_vm::jit::JitMethodGuard,
    allow_eval_env: bool,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    if view.cage_base == 0 {
        return Err(Unsupported::OperandShape("x86-64 method guard cage base"));
    }
    load64(ops, 10, NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; test r9, r10
        ; jnz =>miss
        ; mov r11d, r9d
    );
    symbolic(
        ops,
        relocations,
        0,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch x64
        ; add r11, rax
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
            ; add r10, rax
            ; mov r11, r10
            ; cmp BYTE [r11], OBJECT_BODY_TYPE_TAG as i8
            ; jne =>miss
            ; cmp DWORD [r11 + view.object_shape_byte as i32], shape as i32
            ; jne =>miss
        );
    }
    slab_base(ops, view, miss);
    dynasm!(ops ; .arch x64 ; mov r9, [r8 + guard.method_value_byte as i32]);

    let guarded = ops.new_dynamic_label();
    load64(
        ops,
        10,
        otter_vm::value::tag::box_function_id(guard.method_fid),
    );
    dynasm!(ops
        ; .arch x64
        ; cmp r9, r10
        ; je =>guarded
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
        ; cmp BYTE [r9], JS_CLOSURE_BODY_TYPE_TAG as i8
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
    );
    if !allow_eval_env {
        dynasm!(ops
            ; .arch x64
            ; cmp DWORD [r9 + view.closure_call_layout.eval_env_byte as i32], 0
            ; jne =>miss
        );
    }
    dynasm!(ops
        ; .arch x64
        ; cmp DWORD [r9 + view.closure_call_layout.function_id_byte as i32], guard.method_fid as i32
        ; jne =>miss
        ; =>guarded
    );
    Ok(())
}

fn emit_inline_this(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    this_mode: otter_vm::JitDirectCallThisMode,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    let unbound = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    load64(ops, 10, NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; mov r11, r9
        ; and r11, r10
        ; test r11, r11
        ; jnz =>unbound
        ; mov r11d, [r9 + view.closure_call_layout.flags_byte as i32]
    );
    load64(ops, 10, u64::from(view.closure_call_layout.bound_this_flag));
    dynasm!(ops ; .arch x64 ; test r11d, r10d ; jz =>unbound);
    match this_mode {
        otter_vm::JitDirectCallThisMode::StrictOrLexical => {
            dynasm!(ops
                ; .arch x64
                ; mov rax, [r9 + view.closure_call_layout.bound_this_byte as i32]
                ; jmp =>done
            );
        }
        otter_vm::JitDirectCallThisMode::SloppyGlobal => {
            dynasm!(ops ; .arch x64 ; jmp =>miss);
        }
        _ => return Err(Unsupported::OperandShape("x86-64 inline this mode")),
    }
    dynasm!(ops ; .arch x64 ; =>unbound);
    match this_mode {
        otter_vm::JitDirectCallThisMode::StrictOrLexical => {
            load64(ops, 0, VALUE_UNDEFINED);
        }
        otter_vm::JitDirectCallThisMode::SloppyGlobal => {
            dynasm!(ops
                ; .arch x64
                ; mov r10, [r15 + GLOBAL_THIS_OFFSET_PTR_OFFSET as i32]
                ; mov eax, [r10]
            );
            symbolic(
                ops,
                relocations,
                10,
                view.cage_base as u64,
                RelocationTarget::GcCageBase,
            );
            dynasm!(ops ; .arch x64 ; add rax, r10);
        }
        _ => unreachable!(),
    }
    dynasm!(ops ; .arch x64 ; =>done);
    Ok(())
}

fn load_integer(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    location: AllocatedLocation,
    destination: u8,
) -> Result<(), Unsupported> {
    load_integer_with_bias(ops, frame, location, destination, 0)
}

fn load_integer_with_bias(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    location: AllocatedLocation,
    destination: u8,
    stack_bias: u32,
) -> Result<(), Unsupported> {
    match location {
        AllocatedLocation::Register(register) if register.is_integer() => {
            if register.encoding() != destination {
                dynasm!(ops ; .arch x64 ; mov Rq(destination), Rq(register.encoding()));
            }
        }
        AllocatedLocation::Stack(slot) => {
            let offset =
                spill(frame, slot)?
                    .checked_add(i32::try_from(stack_bias).map_err(|_| {
                        Unsupported::OperandShape("x86-64 integer source stack bias")
                    })?)
                    .ok_or(Unsupported::OperandShape(
                        "x86-64 integer source stack offset",
                    ))?;
            dynasm!(ops ; .arch x64 ; mov Rq(destination), [rsp + offset]);
        }
        _ => return Err(Unsupported::OperandShape("x86-64 integer source")),
    }
    Ok(())
}

fn store_integer(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    location: AllocatedLocation,
    source: u8,
) -> Result<(), Unsupported> {
    match location {
        AllocatedLocation::Register(register) if register.is_integer() => {
            if register.encoding() != source {
                dynasm!(ops ; .arch x64 ; mov Rq(register.encoding()), Rq(source));
            }
        }
        AllocatedLocation::Stack(slot) => {
            let offset = spill(frame, slot)?;
            dynasm!(ops ; .arch x64 ; mov [rsp + offset], Rq(source));
        }
        _ => return Err(Unsupported::OperandShape("x86-64 integer destination")),
    }
    Ok(())
}

fn load_float(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    location: AllocatedLocation,
    destination: u8,
) -> Result<(), Unsupported> {
    match location {
        AllocatedLocation::Register(register) if register.is_float() => {
            if register.encoding() != destination {
                dynasm!(ops ; .arch x64 ; movsd Rx(destination), Rx(register.encoding()));
            }
        }
        AllocatedLocation::Stack(slot) => {
            let offset = spill(frame, slot)?;
            dynasm!(ops ; .arch x64 ; movsd Rx(destination), [rsp + offset]);
        }
        _ => return Err(Unsupported::OperandShape("x86-64 floating source")),
    }
    Ok(())
}

fn object_header(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    frame: MachineFrameLayout,
    receiver: AllocatedLocation,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    load_integer(ops, frame, receiver, 11)?;
    load64(ops, 10, NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; test r11, r10
        ; jnz =>miss
        ; mov r11d, r11d
    );
    symbolic(
        ops,
        relocations,
        10,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch x64
        ; add r11, r10
        ; cmp BYTE [r11], OBJECT_BODY_TYPE_TAG as i8
        ; jne =>miss
    );
    Ok(())
}

fn shape_state_guard(ops: &mut Assembler, view: &JitCompileSnapshot, miss: DynamicLabel) {
    dynasm!(ops
        ; .arch x64
        ; movzx r10d, BYTE [r11 + view.object_shape_cache_mode_byte as i32]
    );
    if view.object_shape_cache_fast == 0 {
        dynasm!(ops ; .arch x64 ; test r10d, r10d ; jnz =>miss);
    } else {
        dynasm!(ops
            ; .arch x64
            ; cmp r10d, view.object_shape_cache_fast as i32
            ; jne =>miss
        );
    }
    dynasm!(ops
        ; .arch x64
        ; cmp BYTE [r11 + view.object_chain_link_opaque_byte as i32], 0
        ; jne =>miss
    );
}

fn ordinary_lookup_state_guard(ops: &mut Assembler, view: &JitCompileSnapshot, miss: DynamicLabel) {
    shape_state_guard(ops, view, miss);
    dynasm!(ops
        ; .arch x64
        ; cmp BYTE [r11 + view.object_slot_attrs_overridden_byte as i32], 0
        ; jne =>miss
    );
}

fn slab_base(ops: &mut Assembler, view: &JitCompileSnapshot, miss: DynamicLabel) {
    let spilled = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; mov r10d, [r11 + view.object_slab_handle_byte as i32]
        ; test r10d, r10d
        ; jnz =>spilled
        ; lea r8, [r11 + view.object_inline_values_byte as i32]
        ; jmp =>done
        ; =>spilled
        ; mov r8, [r11 + view.object_values_ptr_byte as i32]
        ; test r8, r8
        ; jz =>miss
        ; =>done
    );
}

fn element_access(
    view: &JitCompileSnapshot,
    byte_pc: u32,
) -> Result<otter_vm::jit::JitElementAccess, Unsupported> {
    (view.cage_base != 0)
        .then(|| view.element_accesses.get(&byte_pc).copied())
        .flatten()
        .filter(|access| access.type_tag != 0)
        .ok_or(Unsupported::OperandShape("x86-64 element access"))
}

#[allow(clippy::too_many_arguments)]
fn element_view(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    frame: MachineFrameLayout,
    receiver: AllocatedLocation,
    access: otter_vm::jit::JitElementAccess,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    load_integer(ops, frame, receiver, 11)?;
    load64(ops, 10, NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; test r11, r10
        ; jnz =>miss
        ; mov r11d, r11d
    );
    symbolic(
        ops,
        relocations,
        10,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch x64
        ; add r11, r10
        ; cmp BYTE [r11], access.type_tag as i8
        ; jne =>miss
    );
    for guard in access.guards.into_iter().flatten() {
        element_body_guard(ops, guard, miss);
    }
    match access.length_width {
        otter_vm::jit::JitGuardWidth::Byte => {
            dynasm!(ops ; .arch x64 ; movzx r9d, BYTE [r11 + access.length_byte as i32]);
        }
        otter_vm::jit::JitGuardWidth::Word32 => {
            dynasm!(ops ; .arch x64 ; mov r9d, [r11 + access.length_byte as i32]);
        }
        otter_vm::jit::JitGuardWidth::Word64 => {
            dynasm!(ops ; .arch x64 ; mov r9, [r11 + access.length_byte as i32]);
        }
    }
    match access.base {
        otter_vm::jit::JitElementBase::None => {
            return Err(Unsupported::OperandShape("x86-64 element base"));
        }
        otter_vm::jit::JitElementBase::InBody { byte } => {
            dynasm!(ops ; .arch x64 ; mov r8, [r11 + byte as i32]);
        }
        otter_vm::jit::JitElementBase::ThroughLocalBuffer {
            storage_tag_byte,
            local_tag,
            handle_byte,
            detached_byte,
            data_ptr_byte,
            byte_len_byte,
            view_offset_byte,
        } => {
            dynasm!(ops
                ; .arch x64
                ; cmp DWORD [r11 + storage_tag_byte as i32], local_tag as i32
                ; jne =>miss
                ; mov eax, [r11 + handle_byte as i32]
                ; test eax, eax
                ; jz =>miss
            );
            symbolic(
                ops,
                relocations,
                8,
                view.cage_base as u64,
                RelocationTarget::GcCageBase,
            );
            dynasm!(ops
                ; .arch x64
                ; add rax, r8
                ; cmp BYTE [rax + detached_byte as i32], 0
                ; jne =>miss
                ; mov rdx, [r11 + view_offset_byte as i32]
                ; mov r10, r9
                ; shl r10, access.element.stride_shift() as i8
                ; jo =>miss
                ; add r10, rdx
                ; jc =>miss
                ; cmp r10, [rax + byte_len_byte as i32]
                ; ja =>miss
                ; mov r8, [rax + data_ptr_byte as i32]
                ; test r8, r8
                ; jz =>miss
                ; add r8, rdx
            );
        }
    }
    Ok(())
}

fn element_body_guard(ops: &mut Assembler, guard: otter_vm::jit::JitBodyGuard, miss: DynamicLabel) {
    match guard.width {
        otter_vm::jit::JitGuardWidth::Byte => {
            dynasm!(ops ; .arch x64 ; movzx eax, BYTE [r11 + guard.byte as i32]);
        }
        otter_vm::jit::JitGuardWidth::Word32 => {
            dynasm!(ops ; .arch x64 ; mov eax, [r11 + guard.byte as i32]);
        }
        otter_vm::jit::JitGuardWidth::Word64 => {
            dynasm!(ops ; .arch x64 ; mov rax, [r11 + guard.byte as i32]);
        }
    }
    load64(ops, 10, u64::from(guard.expect));
    dynasm!(ops ; .arch x64 ; cmp rax, r10 ; jne =>miss);
}

fn spill(frame: MachineFrameLayout, slot: u32) -> Result<i32, Unsupported> {
    frame
        .spill_offset(slot)
        .ok()
        .and_then(|v| i32::try_from(v).ok())
        .ok_or(Unsupported::OperandShape("x86-64 spill offset"))
}

fn ireg(location: AllocatedLocation) -> Result<u8, Unsupported> {
    match location {
        AllocatedLocation::Register(r) if r.is_integer() => Ok(r.encoding()),
        _ => Err(Unsupported::OperandShape("x86-64 integer register")),
    }
}

fn freg(location: AllocatedLocation) -> Result<u8, Unsupported> {
    match location {
        AllocatedLocation::Register(r) if r.is_float() => Ok(r.encoding()),
        _ => Err(Unsupported::OperandShape("x86-64 floating register")),
    }
}

fn load64(ops: &mut Assembler, register: u8, value: u64) {
    dynasm!(ops ; .arch x64 ; mov Rq(register), QWORD value as i64);
}

fn symbolic(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    register: u8,
    value: u64,
    target: RelocationTarget,
) {
    let start = ops.offset().0;
    load64(ops, register, value);
    relocations.record_x86_imm64(start, ops.offset().0, register, target);
}

fn runtime(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    value: u64,
    target: RuntimeStubDescriptor,
) {
    symbolic(
        ops,
        relocations,
        11,
        value,
        RelocationTarget::runtime_stub(target),
    );
}
