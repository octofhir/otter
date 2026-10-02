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
//! - Indexed layouts belong to Machine operations; scalar loads/stores preserve
//!   signedness and Float32 rounding without reconstructing JS Values on hits.
//! - Every generated call observes the System V 16-byte stack alignment.
//! - Context words load and store at 32-bit displacements from the tagged
//!   owning value; SELF's closure context tests the cell and closure tag in
//!   `r11` first. Context allocation uses the shared `AllocValue3` call with
//!   rooted safepoint homes, and a checked context-slot guard walks the chain
//!   in `r10` before its hole test.

#![allow(clippy::useless_conversion)]

use std::collections::BTreeSet;

#[path = "x86_64/activation.rs"]
mod activation;
#[path = "x86_64/call.rs"]
mod call;
#[path = "x86_64/instanceof.rs"]
mod instanceof;
#[path = "x86_64/loose_equality.rs"]
mod loose_equality;
#[path = "x86_64/megamorphic_property.rs"]
mod megamorphic_property;
#[path = "x86_64/number_probe.rs"]
mod number_probe;
#[path = "x86_64/receiver_allocation.rs"]
mod receiver_allocation;


use dynasmrt::{AssemblyOffset, DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_bytecode::opcode_schema::{BindingRead, BindingSemantics, BindingWrite};
use otter_vm::{
    JitCompileSnapshot, Value,
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
    CallTarget, ContextField, DeoptId, ExceptionalEdge, InstructionSequence, MachineBindingTarget,
    MachineCallGuard, MachineFrameLayout, MachineInstructionId, MachineOpcode, MachineOsrType,
    MachineRepresentation, MachineSafepointSite, MachineSafepointTable,
};
use crate::{
    CompiledCode, Unsupported,
    artifact::relocation::{PropertySourceAccess, RelocationCapture, RelocationTarget},
    entry::{
        ALLOC_CTX_SAFEPOINT_ID_OFFSET, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET,
        ALLOC_CTX_SPILL_SLOTS_OFFSET, ALLOC_CTX_STACK_SIZE, ALLOC_CTX_THREAD_OFFSET,
        ALLOC_WINDOW_LAB_OFFSET, CANONICAL_NAN_HI16, DOUBLE_OFFSET_HI16,
        GLOBAL_THIS_OFFSET_PTR_OFFSET, LAB_LIMIT_OFFSET, LAB_TOP_OFFSET,
        NATIVE_FRAME_CALL_SITE_OFFSET, NATIVE_FRAME_FLAGS_OFFSET,
        NATIVE_FRAME_MACHINE_ROOTS_OFFSET, NATIVE_FRAME_NEW_TARGET_OFFSET, NATIVE_FRAME_OFFSET,
        NATIVE_FRAME_PC_OFFSET, NATIVE_FRAME_REGISTER_BASE_OFFSET, NATIVE_FRAME_SELF_OFFSET,
        NATIVE_FRAME_THIS_OFFSET, NATIVE_STACK_LIMIT_OFFSET, NUMBER_TAG_HI16, OBJECT_BODY_TYPE_TAG,
        PropertySourceCell, RECEIVER_ALLOC_ATTEMPTS_OFFSET, RECEIVER_ALLOC_GENERATED_OFFSET,
        RECEIVER_ALLOC_GUARD_MISSES_OFFSET, RECEIVER_ALLOC_SPACE_MISSES_OFFSET,
        RUNTIME_STATS_OFFSET, THREAD_OFFSET, TransitionTable, VALUE_UNDEFINED,
        VM_THREAD_ARRAY_INDEX_PROTECTOR_CELL_OFFSET, VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET,
        VM_THREAD_GC_HEAP_OFFSET, VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET,
        VM_THREAD_INTERRUPT_CELL_OFFSET,
    },
};

const NUMBER_TAG: u64 = (NUMBER_TAG_HI16 as u64) << 48;
const DOUBLE_OFFSET: u64 = (DOUBLE_OFFSET_HI16 as u64) << 48;
const CANONICAL_NAN: u64 = (CANONICAL_NAN_HI16 as u64) << 48;
const NOT_CELL_MASK: u64 = otter_vm::value::tag::NOT_CELL_MASK;
const DEOPT_BANK_BYTES: i32 = 16 * 8;
const DEOPT_DUMP_BYTES: i32 = DEOPT_BANK_BYTES * 2;

pub(super) struct Emission {
    /// The finalized body; its entry is the tier entry over a published
    /// interpreter frame.
    pub(super) code: CompiledCode,
    /// Offset of the JavaScript call ABI entry.
    pub(super) call_entry: usize,
    pub(super) relocations: RelocationCapture,
    pub(super) osr_headers: BTreeSet<u32>,
    pub(super) osr_regions: Vec<(u32, usize, usize)>,
    pub(super) structural_regions: Vec<(&'static str, Option<u32>, usize, usize)>,
}

#[derive(Clone, Copy)]
struct SavedFrame {
    rbx: bool,
    highest: Option<u8>,
}

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
    code_object_id: u64,
    sequence: &InstructionSequence,
    allocation: &AllocatedSequence,
    frame: MachineFrameLayout,
    deopt_runtime: &DeoptRuntime,
    safepoints: &MachineSafepointTable,
    transitions: &TransitionTable,
    poll_entry: u64,
    deopt_writeback_entry: u64,
    string_concat_entry: u64,
    _array_construct_entry: u64,
    number_rem_entry: u64,
    number_pow_entry: u64,
    number_to_int32_entry: u64,
    strict_eq_entry: u64,
    to_boolean_entry: u64,
    load_ic_cells: &mut [PropertySourceCell],
    store_ic_cells: &mut [PropertySourceCell],
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
    // OSR frame reads that miss their header representation leave through
    // one side exit per read, emitted after the body.
    let mut osr_bails = Vec::<(DynamicLabel, u32)>::new();
    let mut osr_headers = BTreeSet::new();
    let mut osr_regions = Vec::new();
    let has_poll = sequence
        .instructions()
        .iter()
        .any(|i| i.opcode == MachineOpcode::BackedgePoll);

    let shape = crate::x86_64::activation::EntryShape::of(
        view,
        code_object_id,
        otter_vm::native_abi::NativeFrameKind::Optimizing,
        !safepoints.records().is_empty(),
    )?;
    let exits = activation::ExitLabels {
        plain: ops.new_dynamic_label(),
        construct: shape.completes_construct().then(|| ops.new_dynamic_label()),
    };
    // The tier entry continues a published interpreter frame; the call entry
    // builds this function's record and falls through into the body.
    let body = ops.new_dynamic_label();
    let tier_entry = activation::emit_tier_entry(&mut ops, frame, saved, body);
    let (call_entry, call_entry_cold) =
        activation::emit_call_entry(&mut ops, view, frame, saved, shape);
    structural_regions.push(("machineTierEntry", None, tier_entry.0, call_entry.0));
    structural_regions.push(("machineCallEntry", None, call_entry.0, ops.offset().0));
    dynasm!(ops ; .arch x64 ; =>body);
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
            MachineOpcode::OsrDispatch { ref logical_pcs } => {
                let block = &sequence.blocks()[block_index];
                let osr = ops.new_dynamic_label();
                let ordinary = blocks[block.successors[0].0 as usize];
                let bit = otter_vm::native_abi::NativeFrameFlags::OSR_ENTRY;
                dynasm!(ops
                    ; .arch x64
                    ; mov r11, [r15 + NATIVE_FRAME_OFFSET as i32]
                    ; test BYTE [r11 + crate::entry::NATIVE_FRAME_FLAGS_OFFSET as i32], bit as i8
                    ; jnz =>osr
                    ; jmp =>ordinary
                    ; =>osr
                    // The dispatch consumes the entry bit: the OSR block leaves
                    // an ordinary optimizing frame behind it.
                    ; and BYTE [r11 + crate::entry::NATIVE_FRAME_FLAGS_OFFSET as i32], !bit as i8
                    ; mov r11d, DWORD [r11 + NATIVE_FRAME_PC_OFFSET as i32]
                );
                for (&logical_pc, successor) in logical_pcs.iter().zip(&block.successors[1..]) {
                    let target = blocks[successor.0 as usize];
                    let pc = i32::try_from(logical_pc)
                        .map_err(|_| Unsupported::OperandShape("x86-64 OSR header PC"))?;
                    dynasm!(ops ; .arch x64 ; cmp r11d, pc ; je =>target);
                    if !osr_headers.insert(logical_pc) {
                        return Err(Unsupported::OperandShape("duplicate x86-64 OSR header"));
                    }
                }
                // No other header can carry the entry bit.
                dynasm!(ops ; .arch x64 ; jmp =>fatal);
                continue;
            }
            MachineOpcode::OsrValue {
                logical_pc,
                frame_register,
                value_type,
            } => {
                let start = ops.offset().0;
                let bail = ops.new_dynamic_label();
                osr_value(&mut ops, frame, frame_register, value_type, loc[0], bail)?;
                osr_bails.push((bail, logical_pc));
                osr_regions.push((logical_pc, start, ops.offset().0));
            }
            MachineOpcode::LoopPreheader => {}
            MachineOpcode::EntryValue(parameter) => {
                // The record's actual pointer names the padded span of a
                // call, or the window of a tier-entered interpreter frame.
                let dst = ireg(loc[0])?;
                let offset = i32::from(parameter) * 8;
                let actuals = frame.record_offset() as i32
                    + otter_vm::native_abi::NATIVE_FRAME_ACTUALS_OFFSET as i32;
                dynasm!(ops
                    ; .arch x64
                    ; mov r11, [rsp + actuals]
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
            MachineOpcode::EntryCallee => {
                let dst = ireg(loc[0])?;
                dynasm!(ops
                    ; .arch x64
                    ; mov r11, [r15 + NATIVE_FRAME_OFFSET as i32]
                    ; mov Rq(dst), [r11 + NATIVE_FRAME_SELF_OFFSET as i32]
                );
            }
            MachineOpcode::ContextLoad { field } => {
                // The early input may share the late output's register:
                // every path reads the input before its one output write.
                let src = ireg(loc[0])?;
                let dst = ireg(loc[1])?;
                match field {
                    ContextField::ClosureContext => {
                        let not_closure = ops.new_dynamic_label();
                        let done = ops.new_dynamic_label();
                        let context = context_word(view.closure_call_layout.context_byte)?;
                        load64(&mut ops, 11, NOT_CELL_MASK);
                        dynasm!(ops
                            ; .arch x64
                            ; test Rq(src), r11
                            ; jnz =>not_closure
                            ; cmp BYTE [Rq(src)], JS_CLOSURE_BODY_TYPE_TAG as i8
                            ; jne =>not_closure
                            ; mov Rq(dst), [Rq(src) + context]
                            ; jmp =>done
                            ; =>not_closure
                        );
                        load64(&mut ops, dst, VALUE_UNDEFINED);
                        dynasm!(ops ; .arch x64 ; =>done);
                    }
                    ContextField::Parent => {
                        let parent = context_word(view.context_layout.parent_byte)?;
                        dynasm!(ops ; .arch x64 ; mov Rq(dst), [Rq(src) + parent]);
                    }
                    ContextField::Slot(slot) => {
                        let offset = context_slot_offset(view, slot)?;
                        dynasm!(ops ; .arch x64 ; mov Rq(dst), [Rq(src) + offset]);
                    }
                }
            }
            MachineOpcode::ContextStore { slot } => {
                let context = ireg(loc[0])?;
                let value = ireg(loc[1])?;
                let offset = context_slot_offset(view, slot)?;
                dynasm!(ops ; .arch x64 ; mov [Rq(context) + offset], Rq(value));
            }
            MachineOpcode::TaggedIsNotHole => {
                let src = ireg(loc[0])?;
                let dst = ireg(loc[1])?;
                load64(&mut ops, 11, Value::hole().to_bits());
                dynasm!(ops ; .arch x64 ; cmp Rq(src), r11);
                bool_from_flags(&mut ops, dst, Cond::Ne);
            }
            MachineOpcode::TryBindDerivedThis { byte_pc } => {
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                let flags = otter_vm::native_abi::NativeFrameFlags::DERIVED_CONSTRUCTOR;
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
            // A binding owner is a raw cell address and a context owner is
            // the context value; both are the header address.
            MachineOpcode::BindingWriteBarrier | MachineOpcode::ContextWriteBarrier => {
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
                guard: MachineCallGuard::Method { ref guard },
            } => {
                let start = ops.offset().0;
                let miss = deopt(instruction.deopt_id(), &deopts)?;
                load_integer(&mut ops, frame, loc[0], 9)?;
                emit_inline_method_guard(&mut ops, &mut relocations, view, guard, miss)?;
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
                guard:
                    MachineCallGuard::Explicit {
                        function_id,
                        this_mode,
                    },
            } => {
                let start = ops.offset().0;
                let miss = deopt(instruction.deopt_id(), &deopts)?;
                // Either input may live in r9 or the identity proof's r8, so
                // both are read before the proof; rax is not one of its
                // scratch registers.
                load_integer(&mut ops, frame, loc[1], 11)?;
                load_integer(&mut ops, frame, loc[0], 9)?;
                dynasm!(ops ; .arch x64 ; mov rax, r11);
                emit_inline_identity(&mut ops, view, function_id, miss);
                emit_inline_explicit_this(&mut ops, &mut relocations, view, this_mode, miss)?;
                store_integer(&mut ops, frame, loc[2], 0)?;
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
                receiver_allocation::emit_receiver_candidate_probe(&mut ops, &mut relocations, view, plan);
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
                receiver_allocation::emit_receiver_publication_effect(&mut ops, view);
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
                // `cvttsd2si` truncates exactly below 2^63 in magnitude and
                // the low 32 bits are then the modular ToInt32 result. The
                // integer-indefinite answer (i64::MIN) marks NaN, Infinity or
                // an out-of-range magnitude; only those call the leaf, which
                // the instruction's caller-saved clobbers already permit.
                let (src, dst) = (freg(loc[0])?, ireg(loc[1])?);
                dynasm!(ops
                    ; .arch x64
                    ; cvttsd2si Rq(dst), Rx(src)
                    ; cmp Rq(dst), 1
                    ; jno >fast
                    ; movsd xmm0, Rx(src)
                );
                runtime(
                    &mut ops,
                    &mut relocations,
                    number_to_int32_entry,
                    STUB_NUMBER_TO_INT32_F64_LEAF,
                );
                dynasm!(ops
                    ; .arch x64
                    ; call r11
                    ; mov Rd(dst), eax
                    ; jmp >done
                    ; fast:
                    ; mov Rd(dst), Rd(dst)
                    ; done:
                );
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
            MachineOpcode::IntegerRem => emit_rem(&mut ops, loc, instruction, &deopts)?,
            MachineOpcode::IntegerNeg => emit_neg(&mut ops, loc, instruction, &deopts)?,
            MachineOpcode::IntegerAnd
            | MachineOpcode::IntegerOr
            | MachineOpcode::IntegerXor
            | MachineOpcode::IntegerAddWrapping
            | MachineOpcode::IntegerSubWrapping
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
            MachineOpcode::InstanceofProbe { byte_pc } => {
                let start = ops.offset().0;
                instanceof::emit(
                    &mut ops,
                    &mut relocations,
                    view,
                    [ireg(loc[0])?, ireg(loc[1])?],
                    [ireg(loc[2])?, ireg(loc[3])?],
                );
                structural_regions.push((
                    "machineInstanceofProbe",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::ClosureAllocationProbe {
                byte_pc,
                with_context,
            } => {
                let start = ops.offset().0;
                let input = usize::from(with_context);
                if with_context {
                    let context = ireg(loc[0])?;
                    dynasm!(ops ; .arch x64 ; mov rdx, Rq(context));
                } else {
                    load64(&mut ops, 2, VALUE_UNDEFINED);
                }
                let result = ireg(loc[input])?;
                let hit = ireg(loc[input + 1])?;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                if let Some(&plan) = view.closure_allocations.get(&byte_pc) {
                    dynasm!(ops ; .arch x64 ; mov rcx, [r15 + NATIVE_FRAME_OFFSET as i32]);
                    crate::x86_64::allocation::emit_closure(&mut ops, view, plan, 1, miss);
                    dynasm!(ops ; .arch x64 ; mov Rq(result), rax ; mov Rd(hit), 1 ; jmp =>done);
                } else {
                    dynasm!(ops ; .arch x64 ; jmp =>miss);
                }
                dynasm!(ops ; .arch x64 ; =>miss);
                load64(&mut ops, result, VALUE_UNDEFINED);
                dynasm!(ops ; .arch x64 ; xor Rd(hit), Rd(hit) ; =>done);
                structural_regions.push((
                    "machineClosureAllocationProbe",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::ArgumentsReadProbe { byte_pc, element } => {
                let start = ops.offset().0;
                let key = if element { Some(ireg(loc[0])?) } else { None };
                let result = ireg(loc[usize::from(element)])?;
                let hit = ireg(loc[usize::from(element) + 1])?;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                dynasm!(ops ; .arch x64 ; mov r11, [r15 + NATIVE_FRAME_OFFSET as i32]);
                crate::x86_64::arguments::emit(&mut ops, key, miss);
                dynasm!(ops ; .arch x64 ; mov Rq(result), r8 ; mov Rd(hit), 1 ; jmp =>done ; =>miss);
                load64(&mut ops, result, VALUE_UNDEFINED);
                dynasm!(ops ; .arch x64 ; xor Rd(hit), Rd(hit) ; =>done);
                structural_regions.push((
                    "machineArgumentsReadProbe",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::BinaryNumberProbe { byte_pc, operator } => {
                let start = ops.offset().0;
                number_probe::emit(
                    &mut ops,
                    operator,
                    [ireg(loc[0])?, ireg(loc[1])?],
                    [ireg(loc[2])?, ireg(loc[3])?],
                );
                structural_regions.push((
                    "machineBinaryNumberProbe",
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
            MachineOpcode::PropertyPolymorphicLoad { byte_pc, ref cases } => {
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let hit = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                object_header(&mut ops, &mut relocations, view, frame, loc[0], miss)?;
                shape_state_guard(&mut ops, view, miss);
                dynasm!(ops
                    ; .arch x64
                    ; mov r9d, [r11 + view.object_shape_byte as i32]
                );
                let labels = cases
                    .iter()
                    .map(|_| ops.new_dynamic_label())
                    .collect::<Vec<_>>();
                for (case, &label) in cases.iter().zip(&labels) {
                    dynasm!(ops ; .arch x64 ; cmp r9d, case.shape as i32 ; je =>label);
                }
                dynasm!(ops ; .arch x64 ; jmp =>miss);
                for (case, &label) in cases.iter().zip(&labels) {
                    dynasm!(ops ; .arch x64 ; =>label);
                    if case.ordinary {
                        object_flags_guard(
                            &mut ops,
                            view,
                            otter_vm::jit::JIT_OBJECT_FLAG_SLOT_ATTRS_OVERRIDDEN,
                            miss,
                        );
                    }
                    slab_base(&mut ops, &mut relocations, view);
                    dynasm!(ops
                        ; .arch x64
                        ; mov r8, [r8 + case.value_byte as i32]
                        ; jmp =>hit
                    );
                }
                dynasm!(ops ; .arch x64 ; =>hit ; mov r9d, 1 ; jmp =>done ; =>miss);
                load64(&mut ops, 8, VALUE_UNDEFINED);
                dynasm!(ops ; .arch x64 ; xor r9d, r9d ; =>done);
                store_integer(&mut ops, frame, loc[1], 8)?;
                store_integer(&mut ops, frame, loc[2], 9)?;
                structural_regions.push((
                    "machinePolymorphicPropertyLoad",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
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
                symbolic(
                    &mut ops,
                    &mut relocations,
                    8,
                    view.cage_base as u64,
                    RelocationTarget::GcCageBase,
                );
                dynasm!(ops
                    ; .arch x64
                    ; mov r9d, [r11 + view.object_shape_byte as i32]
                    ; test BYTE [r8 + r9 + view.shape_kind_byte as i32], otter_vm::jit::JIT_SHAPE_KIND_DICTIONARY as i8
                    ; jz =>miss
                    ; mov r9d, [r11 + view.object_exotic_handle_byte as i32]
                    ; test r9d, r9d
                    ; jz =>miss
                    ; add r9, r8
                    ; cmp [r9 + view.exotic_dictionary_layout_byte as i32], r10d
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
                if matches!(
                    instruction.opcode,
                    MachineOpcode::CacheIrGuardAtomSlot { writable: true, .. }
                ) {
                    dynasm!(ops ; .arch x64 ; test BYTE [r11 + view.object_flags_byte as i32], otter_vm::jit::JIT_OBJECT_FLAG_USED_AS_PROTOTYPE as i8 ; jnz =>miss);
                }
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
            MachineOpcode::CacheIrGuardArrayIndexProtector { byte_pc } => {
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[0], 10)?;
                dynasm!(ops
                    ; .arch x64
                    ; test r10d, r10d
                    ; jz =>miss
                    ; mov r11, [r15 + THREAD_OFFSET as i32]
                    ; mov r11, [r11 + VM_THREAD_ARRAY_INDEX_PROTECTOR_CELL_OFFSET as i32]
                    ; test r11, r11
                    ; jz =>miss
                    ; cmp BYTE [r11], 0
                    ; jne =>miss
                    ; mov r10d, 1
                    ; jmp =>done
                    ; =>miss
                    ; xor r10d, r10d
                    ; =>done
                );
                store_integer(&mut ops, frame, loc[1], 10)?;
                structural_regions.push((
                    "machineCacheIrGuardArrayIndexProtector",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::CacheIrLoadPrototypeHolder { byte_pc, root } => {
                let start = ops.offset().0;
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[0], 9)?;
                dynasm!(ops ; .arch x64 ; xor r10d, r10d ; test r9d, r9d ; jz =>done);
                symbolic(
                    &mut ops,
                    &mut relocations,
                    11,
                    view.cage_base as u64,
                    RelocationTarget::GcCageBase,
                );
                load64(&mut ops, 10, u64::from(root));
                dynasm!(ops ; .arch x64 ; mov r10d, [r11 + r10 + view.shape_prototype_byte as i32] ; =>done);
                store_integer(&mut ops, frame, loc[1], 10)?;
                store_integer(&mut ops, frame, loc[2], 9)?;
                structural_regions.push((
                    "machineCacheIrLoadPrototypeHolder",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::CacheIrGuardPrototypeValidity { byte_pc, validity } => {
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[0], 10)?;
                dynasm!(ops ; .arch x64 ; test r10d, r10d ; jz =>miss);
                symbolic(
                    &mut ops,
                    &mut relocations,
                    11,
                    validity.address as u64,
                    RelocationTarget::PrototypeValidityCell {
                        identity: validity.identity,
                    },
                );
                dynasm!(ops ; .arch x64 ; cmp DWORD [r11], 0 ; je =>miss
                    ; mov r10d, 1 ; jmp =>done ; =>miss ; xor r10d, r10d ; =>done);
                store_integer(&mut ops, frame, loc[1], 10)?;
                structural_regions.push((
                    "machineCacheIrGuardPrototypeValidity",
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
                symbolic(
                    &mut ops,
                    &mut relocations,
                    8,
                    view.cage_base as u64,
                    RelocationTarget::GcCageBase,
                );
                crate::template::x86_64::emit_x64_load_prototype(&mut ops, view, 10, 11, 8);
                dynasm!(ops
                    ; .arch x64
                    ; test r10d, r10d
                    ; jnz =>miss
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
                slab_base(&mut ops, &mut relocations, view);
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
            MachineOpcode::PropertyShapeProof {
                byte_pc,
                shape,
                ordinary,
            } => {
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                object_header(&mut ops, &mut relocations, view, frame, loc[0], miss)?;
                if ordinary {
                    ordinary_lookup_state_guard(&mut ops, view, miss);
                } else {
                    shape_state_guard(&mut ops, view, miss);
                }
                dynasm!(ops
                    ; .arch x64
                    ; mov r10d, [r11 + view.object_shape_byte as i32]
                    ; cmp r10d, shape as i32
                    ; jne =>miss
                    ; mov r10d, 1
                    ; jmp =>done
                    ; =>miss
                    ; xor r10d, r10d
                    ; =>done
                );
                store_integer(&mut ops, frame, loc[1], 10)?;
                structural_regions.push((
                    "machinePropertyShapeProof",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::PropertySlotLoad {
                byte_pc,
                value_byte,
            } => {
                // The dominating shape proof established an ordinary object
                // owning this slot, so only the moving-safe decode remains.
                let start = ops.offset().0;
                let spilled = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[0], 11)?;
                dynasm!(ops ; .arch x64 ; mov r11d, r11d);
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
                    ; mov r10d, [r11 + view.object_slab_handle_byte as i32]
                    ; test r10d, r10d
                    ; jnz =>spilled
                    ; lea r8, [r11 + view.object_inline_values_byte as i32]
                    ; jmp =>done
                    ; =>spilled
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
                    ; add r8, r10
                    ; add r8, view.object_slab_words_byte as i32
                    ; =>done
                    ; mov r8, [r8 + value_byte as i32]
                );
                store_integer(&mut ops, frame, loc[1], 8)?;
                structural_regions.push((
                    "machinePropertySlotLoad",
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
            MachineOpcode::PropertyStoreDispatch { byte_pc, ref cases } => {
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let hit = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                object_header(&mut ops, &mut relocations, view, frame, loc[0], miss)?;
                ordinary_lookup_state_guard(&mut ops, view, miss);
                // rsi keeps the receiver header; r11 names the object a
                // shared helper inspects.
                dynasm!(ops
                    ; .arch x64
                    ; mov r9d, [r11 + view.object_shape_byte as i32]
                    ; mov rsi, r11
                );
                dynasm!(ops ; .arch x64 ; test BYTE [rsi + view.object_flags_byte as i32], otter_vm::jit::JIT_OBJECT_FLAG_USED_AS_PROTOTYPE as i8 ; jnz =>miss);
                let labels = cases
                    .iter()
                    .map(|_| ops.new_dynamic_label())
                    .collect::<Vec<_>>();
                for (case, &label) in cases.iter().zip(&labels) {
                    dynasm!(ops ; .arch x64 ; cmp r9d, case.shape as i32 ; je =>label);
                }
                dynasm!(ops ; .arch x64 ; jmp =>miss);
                for (case, &label) in cases.iter().zip(&labels) {
                    dynasm!(ops ; .arch x64 ; =>label);
                    if let Some(transition) = &case.transition {
                        if let Some(validity) = transition.prototype_validity {
                            symbolic(
                                &mut ops,
                                &mut relocations,
                                10,
                                validity.address as u64,
                                RelocationTarget::PrototypeValidityCell {
                                    identity: validity.identity,
                                },
                            );
                            dynasm!(ops ; .arch x64 ; cmp DWORD [r10], 0 ; je =>miss);
                        }
                        let slot = case.value_byte / 8;
                        let inline_storage = ops.new_dynamic_label();
                        let fits = ops.new_dynamic_label();
                        symbolic(
                            &mut ops,
                            &mut relocations,
                            8,
                            view.cage_base as u64,
                            RelocationTarget::GcCageBase,
                        );
                        dynasm!(ops
                            ; .arch x64
                            ; mov r11, rsi
                            ; mov r9d, [r11 + view.object_slab_handle_byte as i32]
                            ; test r9d, r9d
                            ; jz =>inline_storage
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
                            ; cmp r9d, slot as i32
                            ; jbe =>miss
                            ; jmp =>fits
                            ; =>inline_storage
                        );
                        inline_capacity_guard(&mut ops, view, slot, miss);
                        dynasm!(ops
                            ; .arch x64
                            ; =>fits
                            ; test BYTE [r11 + view.object_flags_byte as i32], otter_vm::jit::JIT_OBJECT_FLAG_EXTENSIBLE as i8
                            ; jz =>miss
                        );
                        // The matched receiver shape has exactly `slot`
                        // slots, so the store is the exact append.
                    }
                    slab_base(&mut ops, &mut relocations, view);
                    load_integer(&mut ops, frame, loc[1], 10)?;
                    dynasm!(ops ; .arch x64 ; mov [r8 + case.value_byte as i32], r10);
                    if let Some(transition) = &case.transition {
                        dynasm!(ops
                            ; .arch x64
                            ; mov DWORD [r11 + view.object_shape_byte as i32], transition.child_shape as i32
                            ; mov r9d, transition.child_shape as i32
                        );
                    } else {
                        load64(&mut ops, 9, VALUE_UNDEFINED);
                    }
                    dynasm!(ops ; .arch x64 ; jmp =>hit);
                }
                dynasm!(ops ; .arch x64 ; =>hit ; mov r10d, 1 ; jmp =>done ; =>miss ; xor r11d, r11d);
                load64(&mut ops, 9, VALUE_UNDEFINED);
                dynasm!(ops ; .arch x64 ; xor r10d, r10d ; =>done);
                // r11 (owner) is outside the allocation file; write the child
                // and hit outputs in an order that never overwrites an unread
                // source.
                store_outputs_ordered(&mut ops, frame, [(loc[3], 9), (loc[4], 10)])?;
                store_integer(&mut ops, frame, loc[2], 11)?;
                structural_regions.push((
                    "machinePropertyStoreDispatch",
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
                slab_base(&mut ops, &mut relocations, view);
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
                dynasm!(ops ; .arch x64 ; test BYTE [r11 + view.object_flags_byte as i32], otter_vm::jit::JIT_OBJECT_FLAG_USED_AS_PROTOTYPE as i8 ; jnz =>miss);

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
                    ; movzx r9d, BYTE [r11 + view.object_inline_capacity_byte as i32]
                    ; cmp r10d, r9d
                    ; jae =>miss
                    ; =>storage_fits
                    ; test BYTE [r11 + view.object_flags_byte as i32], otter_vm::jit::JIT_OBJECT_FLAG_EXTENSIBLE as i8
                    ; jz =>miss
                    // The program's receiver shape guard fixes the slot
                    // count at the appended index.
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
            MachineOpcode::CacheIrPublishShape { byte_pc, shape } => {
                let start = ops.offset().0;
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[1], 10)?;
                dynasm!(ops ; .arch x64 ; test r10d, r10d ; jz =>done);
                load_integer(&mut ops, frame, loc[0], 11)?;
                dynasm!(ops
                    ; .arch x64
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
            MachineOpcode::ElementView { byte_pc, access } => {
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
            MachineOpcode::ElementProof { byte_pc, access } => {
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
                store_integer(&mut ops, frame, loc[1], 9)?;
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
                    "machineElementProof",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::ElementAddress { byte_pc, access } => {
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[3], 10)?;
                dynasm!(ops ; .arch x64 ; test r10d, r10d ; jz =>miss);
                load_integer(&mut ops, frame, loc[0], 8)?;
                load_integer(&mut ops, frame, loc[1], 9)?;
                let index_representation =
                    sequence.representations()[instruction.operands[2].value.0 as usize];
                if index_representation == MachineRepresentation::Float64 {
                    let source = freg(loc[2])?;
                    dynasm!(ops ; .arch x64
                        ; cvttsd2si r11, Rx(source)
                        ; mov r11d, r11d
                        ; cvtsi2sd xmm15, r11
                        ; ucomisd xmm15, Rx(source)
                        ; jp =>miss
                        ; jne =>miss
                    );
                } else {
                    load_integer(&mut ops, frame, loc[2], 11)?;
                    if index_representation == MachineRepresentation::Tagged {
                        dynasm!(ops ; .arch x64 ; mov r10, r11 ; shr r10, 48 ; cmp r10w, NUMBER_TAG_HI16 as i16 ; jne =>miss);
                    }
                    if matches!(
                        index_representation,
                        MachineRepresentation::Tagged | MachineRepresentation::Int32
                    ) {
                        dynasm!(ops ; .arch x64 ; test r11d, r11d ; js =>miss);
                    }
                }
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
            MachineOpcode::ElementValueLoad { byte_pc, access } => {
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                load_integer(&mut ops, frame, loc[1], 9)?;
                dynasm!(ops ; .arch x64 ; test r9d, r9d ; jz =>miss);
                load_integer(&mut ops, frame, loc[0], 8)?;
                let representation =
                    sequence.representations()[instruction.operands[2].value.0 as usize];
                element_value_read(&mut ops, access, representation, loc[2], miss)?;
                dynasm!(ops ; .arch x64 ; mov r9d, 1 ; jmp =>done ; =>miss);
                load64(&mut ops, 10, VALUE_UNDEFINED);
                dynasm!(ops ; .arch x64 ; xor r9d, r9d ; =>done);
                if representation != MachineRepresentation::Float64 {
                    store_integer(&mut ops, frame, loc[2], 10)?;
                }
                store_integer(&mut ops, frame, loc[3], 9)?;
                structural_regions.push((
                    "machineElementValueLoad",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::ElementCheckedLoad { byte_pc, access }
            | MachineOpcode::ElementCheckedAddress { byte_pc, access } => {
                let start = ops.offset().0;
                let load = matches!(instruction.opcode, MachineOpcode::ElementCheckedLoad { .. });
                let exit = deopt(instruction.deopt_id(), &deopts)?;
                load_integer(&mut ops, frame, loc[3], 10)?;
                dynasm!(ops ; .arch x64 ; test r10d, r10d ; jz =>exit);
                load_integer(&mut ops, frame, loc[0], 8)?;
                // A dense access reads its receiver's live element base only
                // once the proof holds; the base never outlives this operation.
                if let otter_vm::JitElementBase::InBody { byte } = access.base {
                    dynasm!(ops ; .arch x64 ; mov r8, [r8 + byte as i32]);
                }
                load_integer(&mut ops, frame, loc[1], 9)?;
                // The index enters r11 extended to 64 bits so one unsigned
                // compare against the length also rejects a negative int32.
                match sequence.representations()[instruction.operands[2].value.0 as usize] {
                    MachineRepresentation::Float64 => {
                        let source = freg(loc[2])?;
                        dynasm!(ops ; .arch x64
                            ; cvttsd2si r11, Rx(source)
                            ; mov r11d, r11d
                            ; cvtsi2sd xmm15, r11
                            ; ucomisd xmm15, Rx(source)
                            ; jp =>exit
                            ; jne =>exit
                        );
                    }
                    MachineRepresentation::Tagged => {
                        load_integer(&mut ops, frame, loc[2], 11)?;
                        dynasm!(ops ; .arch x64
                            ; mov r10, r11
                            ; shr r10, 48
                            ; cmp r10w, NUMBER_TAG_HI16 as i16
                            ; jne =>exit
                            ; movsxd r11, r11d
                        );
                    }
                    MachineRepresentation::Int32 => {
                        load_integer(&mut ops, frame, loc[2], 11)?;
                        dynasm!(ops ; .arch x64 ; movsxd r11, r11d);
                    }
                    MachineRepresentation::Uint32 => {
                        load_integer(&mut ops, frame, loc[2], 11)?;
                        dynasm!(ops ; .arch x64 ; mov r11d, r11d);
                    }
                    _ => return Err(Unsupported::OperandShape("element index representation")),
                }
                if matches!(
                    access.length_width,
                    otter_vm::jit::JitGuardWidth::Byte | otter_vm::jit::JitGuardWidth::Word32
                ) {
                    dynasm!(ops ; .arch x64 ; mov r9d, r9d);
                }
                let shift = access.element.stride_shift() as i8;
                dynasm!(ops
                    ; .arch x64
                    ; cmp r11, r9
                    ; jae =>exit
                    ; shl r11, shift
                    ; add r8, r11
                );
                if load {
                    let representation =
                        sequence.representations()[instruction.operands[4].value.0 as usize];
                    element_value_read(&mut ops, access, representation, loc[4], exit)?;
                    if representation != MachineRepresentation::Float64 {
                        store_integer(&mut ops, frame, loc[4], 10)?;
                    }
                } else {
                    // A boxed store needs a present slot; scalar operands
                    // already satisfy the verified storage contract, so only
                    // a tagged value owes a representation guard.
                    if access.element == otter_vm::jit::JitElementRepr::Boxed {
                        dynasm!(ops ; .arch x64 ; mov r10, [r8]);
                        load64(&mut ops, 11, Value::hole().to_bits());
                        dynasm!(ops ; .arch x64 ; cmp r10, r11 ; je =>exit);
                    }
                    if sequence.representations()[instruction.operands[4].value.0 as usize]
                        == MachineRepresentation::Tagged
                    {
                        load_integer(&mut ops, frame, loc[4], 10)?;
                        element_store_value_guard(&mut ops, access, exit);
                    }
                    store_integer(&mut ops, frame, loc[5], 8)?;
                }
                structural_regions.push((
                    if load {
                        "machineElementCheckedLoad"
                    } else {
                        "machineElementCheckedAddress"
                    },
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::ElementValueGuard { byte_pc, access } => {
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
                // Scalar operands already satisfy the verified storage contract.
                // Only tagged values need a representation guard here.
                if sequence.representations()[instruction.operands[1].value.0 as usize]
                    == MachineRepresentation::Tagged
                {
                    load_integer(&mut ops, frame, loc[1], 10)?;
                    element_store_value_guard(&mut ops, access, miss);
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
            MachineOpcode::ElementValueStore { byte_pc, access } => {
                let start = ops.offset().0;
                load_integer(&mut ops, frame, loc[0], 11)?;
                if element_value_is_float(sequence, instruction, &access)? {
                    load_float(&mut ops, frame, loc[1], 14)?;
                    if access.element == otter_vm::JitElementRepr::Float32 {
                        dynasm!(ops ; .arch x64 ; cvtsd2ss xmm14, xmm14 ; movss [r11], xmm14);
                    } else {
                        dynasm!(ops ; .arch x64 ; movsd [r11], xmm14);
                    }
                } else {
                    load_integer(&mut ops, frame, loc[1], 10)?;
                    match access.element {
                        otter_vm::jit::JitElementRepr::Boxed => {
                            dynasm!(ops ; .arch x64 ; mov [r11], r10);
                        }
                        otter_vm::jit::JitElementRepr::Int8
                        | otter_vm::jit::JitElementRepr::Uint8 => {
                            dynasm!(ops ; .arch x64 ; mov [r11], r10b);
                        }
                        otter_vm::jit::JitElementRepr::Uint8Clamped => dynasm!(ops
                            ; .arch x64
                            ; xor r9d, r9d
                            ; cmp r10d, 0
                            ; cmovl r10d, r9d
                            ; mov r9d, 255
                            ; cmp r10d, r9d
                            ; cmovg r10d, r9d
                            ; mov [r11], r10b
                        ),
                        otter_vm::jit::JitElementRepr::Int16
                        | otter_vm::jit::JitElementRepr::Uint16 => {
                            dynasm!(ops ; .arch x64 ; mov [r11], r10w);
                        }
                        otter_vm::jit::JitElementRepr::Int32
                        | otter_vm::jit::JitElementRepr::Uint32 => {
                            dynasm!(ops ; .arch x64 ; mov [r11], r10d);
                        }
                        otter_vm::jit::JitElementRepr::Float32 => {
                            // The guard proved a number; decoding cannot miss.
                            let unreachable = ops.new_dynamic_label();
                            dynasm!(ops ; .arch x64 ; mov r8, r11);
                            decode_number(&mut ops, 10, 14, unreachable);
                            dynasm!(ops
                                ; .arch x64
                                ; =>unreachable
                                ; cvtsd2ss xmm14, xmm14
                                ; movss [r8], xmm14
                            );
                        }
                        otter_vm::jit::JitElementRepr::Float64 => {
                            // The guard proved a number; decoding cannot miss.
                            let unreachable = ops.new_dynamic_label();
                            dynasm!(ops ; .arch x64 ; mov r8, r11);
                            decode_number(&mut ops, 10, 14, unreachable);
                            dynasm!(ops
                                ; .arch x64
                                ; =>unreachable
                                ; movsd [r8], xmm14
                            );
                        }
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
                activation::emit_return(&mut ops, frame, saved, exits);
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
                activation::emit_plain_return(&mut ops, frame, saved);
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
                    .filter(|&words| super::super::native_leaf::supports_leaf_probe(stub, words))
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
                            allocation,
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
                    byte_pc,
                    ..
                } = &descriptor.target
                {
                    let site = safepoints
                        .site(id)
                        .filter(|site| instruction.safepoint == Some(site.id))
                        .ok_or(Unsupported::OperandShape("x86-64 direct call safepoint"))?;
                    let direct_threw = ops.new_dynamic_label();
                    let direct_done = ops.new_dynamic_label();
                    if *kind == super::super::DirectCallKind::Forward {
                        publish_forwarded_formals_context(
                            &mut ops,
                            view,
                            frame,
                            loc,
                            descriptor.arguments.len(),
                        )?;
                    }
                    save_roots(&mut ops, frame, site)?;
                    stamp_call_site(&mut ops, site);
                    let start = ops.offset().0;
                    call::emit(
                        &mut ops,
                        &mut relocations,
                        &call::CallSite {
                            view,
                            transitions,
                            sequence,
                            instruction,
                            frame,
                            site,
                            locations: loc,
                            result_index: descriptor.arguments.len(),
                            call_pc: safepoints.records()[site.id.0 as usize].call_pc,
                            deopt: deopt(instruction.deopt_id(), &deopts)?,
                            threw: direct_threw,
                            fatal,
                            done: direct_done,
                        },
                        *kind,
                        *argument_mode,
                        candidates,
                    )?;
                    structural_regions.push((
                        "machineCallTrampoline",
                        Some(*byte_pc),
                        start,
                        ops.offset().0,
                    ));
                    dynasm!(ops ; .arch x64 ; =>direct_threw);
                    match descriptor.exceptional {
                        super::super::ExceptionalEdge::Propagate => {
                            dynasm!(ops ; .arch x64 ; mov edx, NativeResultStatus::Throw as i32);
                            activation::emit_plain_return(&mut ops, frame, saved);
                        }
                        super::super::ExceptionalEdge::LandingPad(target) => {
                            let result_index = descriptor.arguments.len();
                            let result = *loc
                                .get(result_index)
                                .ok_or(Unsupported::OperandShape("x86-64 direct-call result"))?;
                            store_integer(&mut ops, frame, result, 0)?;
                            // The normal path's moves after the call and around
                            // the block terminator run on the exceptional edge too.
                            edit_run(&mut ops, allocation.exceptional_edge_edits(id), frame)?;
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
                if let CallTarget::ContextAllocation { target, scope } = descriptor.target {
                    if loc.len() < 4
                        || ireg(loc[0])? != 2
                        || ireg(loc[1])? != 1
                        || ireg(loc[2])? != 8
                        || ireg(loc[3])? != 0
                    {
                        return Err(Unsupported::OperandShape("x86-64 context allocation ABI"));
                    }
                    let entry = otter_vm::runtime_stubs::alloc_value_stub_by_id(target.id)
                        .and_then(|stub| stub.entry_addr())
                        .ok_or(Unsupported::OperandShape("x86-64 context allocation entry"))?;
                    let site = safepoints
                        .site(id)
                        .filter(|site| instruction.safepoint == Some(site.id))
                        .ok_or(Unsupported::OperandShape("x86-64 allocating safepoint"))?;
                    let exit = deopt(instruction.deopt_id(), &deopts)?;
                    // The carve leaves `rdx`, `rcx` and `r8` intact for the
                    // allocating call its miss falls into.
                    let slow = ops.new_dynamic_label();
                    let done = ops.new_dynamic_label();
                    let carved = match scope {
                        Some(key) => view.context_allocations.get(&key).map(|plan| {
                            crate::x86_64::allocation::emit_create_context(
                                &mut ops, view, plan, slow,
                            );
                        }),
                        None => {
                            crate::x86_64::allocation::emit_copy_context(&mut ops, view, slow);
                            Some(())
                        }
                    };
                    if carved.is_some() {
                        dynasm!(ops ; .arch x64 ; jmp =>done ; =>slow);
                    }
                    allocating_call(
                        &mut ops,
                        &mut relocations,
                        frame,
                        site,
                        entry as u64,
                        target,
                        exit,
                    )?;
                    dynasm!(ops ; .arch x64 ; =>done);
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
                } else if target == otter_vm::native_abi::STUB_TYPEOF_TEST_LEAF {
                    otter_vm::runtime_stubs::TYPEOF_TEST_LEAF.entry_addr() as u64
                } else {
                    return Err(Unsupported::OperandShape("x86-64 scalar runtime call"));
                };
                if loc.len() < 3 || ireg(loc[0])? != 6 || ireg(loc[1])? != 2 || ireg(loc[2])? != 0 {
                    return Err(Unsupported::OperandShape("x86-64 scalar leaf ABI"));
                }
                let exit = deopt(instruction.deopt_id(), &deopts)?;
                let strict_equal_done =
                    (target == otter_vm::native_abi::STUB_STRICT_EQ_LEAF).then(|| {
                        let done = ops.new_dynamic_label();
                        strict_equal_fast_path(&mut ops, done);
                        done
                    });
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
                if let Some(done) = strict_equal_done {
                    dynasm!(ops ; .arch x64 ; =>done);
                }
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
        ; =>exits.plain
    );
    activation::emit_plain_return(&mut ops, frame, saved);
    activation::emit_bail(
        &mut ops,
        &mut relocations,
        transitions,
        frame,
        saved,
        shape,
        exits,
        bail,
    );
    // A parked error finishes at this frame's boundary.
    let pair_side_exit = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; =>finish_error ; mov rdi, r15);
    runtime(
        &mut ops,
        &mut relocations,
        transitions.entry(STUB_JIT_FINISH_ERROR),
        STUB_JIT_FINISH_ERROR,
    );
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; cmp edx, NativeResultStatus::SideExit as i32
        ; je =>pair_side_exit
        ; cmp edx, NativeResultStatus::Throw as i32
        ; je =>exits.plain
        ; cmp edx, NativeResultStatus::Fatal as i32
        ; je =>exits.plain
        ; =>fatal
    );
    load64(&mut ops, 0, VALUE_UNDEFINED);
    dynasm!(ops ; .arch x64 ; mov edx, NativeResultStatus::Fatal as i32 ; jmp =>exits.plain);
    activation::emit_pair_side_exit(
        &mut ops,
        &mut relocations,
        transitions,
        frame,
        saved,
        exits,
        pair_side_exit,
    );
    activation::emit_construct_completion(
        &mut ops,
        &mut relocations,
        transitions,
        view,
        frame,
        shape,
        exits,
    );
    activation::emit_call_entry_cold(
        &mut ops,
        &mut relocations,
        transitions,
        frame,
        saved,
        exits,
        call_entry_cold,
    );
    if !deopts.is_empty() {
        for (index, &label) in deopts.iter().enumerate() {
            let index = i32::try_from(index)
                .ok()
                .filter(|index| *index <= i32::from(u16::MAX))
                .ok_or(Unsupported::OperandShape("x86-64 deopt count"))?;
            dynasm!(ops ; .arch x64 ; =>label ; mov r11d, index ; jmp =>shared_deopt);
        }
        activation::emit_deopt(
            &mut ops,
            &mut relocations,
            transitions,
            frame,
            saved,
            shape,
            exits,
            deopt_runtime,
            deopt_writeback_entry,
            shared_deopt,
        );
    }
    // OSR exits leave a tier-entered frame only.
    for (bail, logical_pc) in osr_bails {
        dynasm!(ops
            ; .arch x64
            ; =>bail
            ; mov r11, [r15 + NATIVE_FRAME_OFFSET as i32]
            ; mov DWORD [r11 + NATIVE_FRAME_PC_OFFSET as i32], logical_pc as i32
        );
        load64(
            &mut ops,
            0,
            SideExit::new(logical_pc, ExitReason::TypeMismatch, ExitAction::Recompile).to_bits(),
        );
        dynasm!(ops ; .arch x64 ; mov edx, NativeResultStatus::SideExit as i32);
        activation::emit_plain_return(&mut ops, frame, saved);
    }
    let buffer = crate::entry::finalize_assembler(ops)?;
    Ok(Emission {
        code: CompiledCode::new(buffer, tier_entry),
        call_entry: call_entry.0,
        relocations,
        osr_headers,
        osr_regions,
        structural_regions,
    })
}

/// Store the forwarding function's parameter-scope context, the forward
/// call's trailing argument, into its frame register. Native argument
/// copies and arguments-object materialization read the context-held mapped
/// formals through that register, which Machine code does not otherwise
/// keep current. The register window is traced.
fn publish_forwarded_formals_context(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    frame: MachineFrameLayout,
    locations: &[AllocatedLocation],
    argument_count: usize,
) -> Result<(), Unsupported> {
    let Some(frame_register) = view.code_block.forwarded_formals_context() else {
        return Ok(());
    };
    let context = *argument_count
        .checked_sub(1)
        .and_then(|index| locations.get(index))
        .ok_or(Unsupported::OperandShape("x86-64 forwarded formals context"))?;
    // r11 is the only integer scratch; borrow rax around the store.
    dynasm!(ops ; .arch x64 ; push rax);
    load_integer_with_bias(ops, frame, context, 11, 8)?;
    dynasm!(ops
        ; .arch x64
        ; mov rax, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov rax, [rax + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32]
        ; mov [rax + i32::from(frame_register) * 8], r11
        ; pop rax
    );
    Ok(())
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
    // Verified blocks own contiguous, ascending instruction ranges.
    let blocks = sequence.blocks();
    let index = blocks.partition_point(|block| block.end.0 <= id.0);
    blocks
        .get(index)
        .filter(|block| block.first.0 <= id.0 && id.0 < block.end.0)
        .map(|_| index)
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



fn edits(
    ops: &mut Assembler,
    edits: &[AllocationEdit],
    point: AllocationPoint,
    frame: MachineFrameLayout,
) -> Result<(), Unsupported> {
    edit_run(ops, super::super::regalloc::edits_at(edits, point), frame)
}

/// Emit one contiguous run of allocation edits in program order.
fn edit_run(
    ops: &mut Assembler,
    run: &[AllocationEdit],
    frame: MachineFrameLayout,
) -> Result<(), Unsupported> {
    for edit in run {
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

/// V8's Int32ModulusWithOverflow through `div`: the divisor's magnitude in
/// r11, the dividend's magnitude in eax, the remainder in edx (both declared
/// clobbers). The result takes the dividend's sign; a zero divisor and a zero
/// result of a negative dividend exit.
fn emit_rem(
    ops: &mut Assembler,
    loc: &[AllocatedLocation],
    instruction: &super::super::MachineInstruction,
    labels: &[DynamicLabel],
) -> Result<(), Unsupported> {
    let (left, right, dst) = (ireg(loc[0])?, ireg(loc[1])?, ireg(loc[2])?);
    let overflow = deopt(instruction.exit_id(ExitReason::Int32Overflow), labels)?;
    let negative_zero = deopt(instruction.exit_id(ExitReason::NegativeZero), labels)?;
    let divisor_ready = ops.new_dynamic_label();
    let non_negative = ops.new_dynamic_label();
    let general = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; mov r11d, Rd(right)
        ; mov eax, Rd(left)
        ; test r11d, r11d
        ; jg =>divisor_ready
        ; neg r11d
        ; je =>overflow
        ; =>divisor_ready
        ; test eax, eax
        ; jns =>non_negative
        ; neg eax
        ; xor edx, edx
        ; div r11d
        ; test edx, edx
        ; je =>negative_zero
        ; neg edx
        ; mov Rd(dst), edx
        ; jmp =>done
        ; =>non_negative
        ; lea edx, [r11 - 1]
        ; test edx, r11d
        ; jne =>general
        ; and eax, edx
        ; mov Rd(dst), eax
        ; jmp =>done
        ; =>general
        ; xor edx, edx
        ; div r11d
        ; mov Rd(dst), edx
        ; =>done
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
        MachineOpcode::IntegerAddWrapping => dynasm!(ops ; .arch x64 ; add r11d, Rd(right)),
        MachineOpcode::IntegerSubWrapping => dynasm!(ops ; .arch x64 ; sub r11d, Rd(right)),
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

/// Decide `rsi === rdx` inline, leaving the Boolean in `eax` and jumping to
/// `done`, whenever the answer needs no heap body: identical bits (true
/// unless NaN), two numbers (compared as doubles, so `1 === 1.0` and
/// `+0 === -0`), a number or an immediate against anything else, and two
/// cells of different kinds or of a kind compared by identity. Only two
/// strings or two BigInts fall through to the strict-equality leaf call that
/// follows. Uses the call's clobbers: rax, rcx, r11, xmm0 and xmm1.
fn strict_equal_fast_path(ops: &mut Assembler, done: DynamicLabel) {
    use otter_vm::value::tag;
    let differ = ops.new_dynamic_label();
    let numbers = ops.new_dynamic_label();
    let not_equal = ops.new_dynamic_label();
    let slow = ops.new_dynamic_label();
    let left_double = ops.new_dynamic_label();
    let right_decoded = ops.new_dynamic_label();
    let right_double = ops.new_dynamic_label();
    let compare = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; cmp rsi, rdx ; jne =>differ);
    // Identical bits: every value equals itself except NaN, whose one
    // canonical encoding this is.
    load64(ops, 11, tag::box_double(tag::CANONICAL_NAN));
    dynasm!(ops ; .arch x64 ; cmp rsi, r11);
    bool_from_flags(ops, 0, Cond::Ne);
    dynasm!(ops
        ; .arch x64
        ; jmp =>done
        ; =>differ
        ; mov r11, rsi
        ; or r11, rdx
    );
    load64(ops, 1, tag::NUMBER_TAG);
    dynasm!(ops
        ; .arch x64
        ; test r11, rcx
        ; jnz =>numbers
        // Neither is a number. An immediate (bit 1) differs from everything
        // with other bits.
        ; test r11, tag::OTHER_TAG as i32
        ; jnz =>not_equal
        ; movzx ecx, BYTE [rsi]
        ; movzx r11d, BYTE [rdx]
        ; cmp ecx, r11d
        ; jne =>not_equal
        ; cmp ecx, i32::from(otter_vm::string::JS_STRING_BODY_TYPE_TAG)
        ; je =>slow
        ; cmp ecx, i32::from(otter_vm::bigint::BIG_INT_BODY_TYPE_TAG)
        ; je =>slow
        ; =>not_equal
        ; xor eax, eax
        ; jmp =>done
        ; =>numbers
        // At least one is a number; both must be.
        ; test rsi, rcx
        ; jz =>not_equal
        ; test rdx, rcx
        ; jz =>not_equal
        ; mov r11, rsi
        ; and r11, rcx
        ; cmp r11, rcx
        ; jne =>left_double
        ; cvtsi2sd xmm0, esi
        ; jmp =>right_decoded
        ; =>left_double
    );
    load64(ops, 11, tag::DOUBLE_ENCODE_OFFSET);
    dynasm!(ops
        ; .arch x64
        ; mov rax, rsi
        ; sub rax, r11
        ; movq xmm0, rax
        ; =>right_decoded
        ; mov r11, rdx
        ; and r11, rcx
        ; cmp r11, rcx
        ; jne =>right_double
        ; cvtsi2sd xmm1, edx
        ; jmp =>compare
        ; =>right_double
    );
    load64(ops, 11, tag::DOUBLE_ENCODE_OFFSET);
    dynasm!(ops
        ; .arch x64
        ; mov rax, rdx
        ; sub rax, r11
        ; movq xmm1, rax
        ; =>compare
        // Equal only when ordered (PF clear) and ZF set.
        ; xor eax, eax
        ; ucomisd xmm0, xmm1
        ; setnp al
        ; sete cl
        ; and al, cl
        ; jmp =>done
        ; =>slow
    );
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

/// Whether an element store's value operand is the raw double selected for a
/// Float32/Float64 store. Such a value is never guarded; a raw double for
/// any other representation is a selection contract violation.
fn element_value_is_float(
    sequence: &InstructionSequence,
    instruction: &super::super::MachineInstruction,
    access: &otter_vm::JitElementAccess,
) -> Result<bool, Unsupported> {
    let value = instruction
        .operands
        .get(1)
        .ok_or(Unsupported::OperandShape("x86-64 element store value"))?;
    let is_float =
        sequence.representations()[value.value.0 as usize] == MachineRepresentation::Float64;
    if is_float
        && !matches!(
            access.element,
            otter_vm::JitElementRepr::Float32 | otter_vm::JitElementRepr::Float64
        )
    {
        return Err(Unsupported::OperandShape("x86-64 raw double element store"));
    }
    Ok(is_float)
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
        MachineBindingTarget::ContextSlot { depth, slot } => {
            // The context is the last boxed input: after the stored value
            // for a write, alone for a read.
            let context = *locations
                .last()
                .ok_or(Unsupported::OperandShape("x86-64 context binding input"))?;
            load_integer(ops, frame, context, 10)?;
            let parent = context_word(view.context_layout.parent_byte)?;
            for _ in 0..depth {
                dynasm!(ops ; .arch x64 ; mov r10, [r10 + parent]);
            }
            let offset = context_slot_offset(view, slot)?;
            dynasm!(ops ; .arch x64 ; lea r9, [r10 + offset] ; mov r11, [r9]);
            load64(ops, 8, Value::hole().to_bits());
            dynasm!(ops ; .arch x64 ; cmp r11, r8 ; je =>miss);
        }
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
            dynasm!(ops ; .arch x64 ; lea r9, [r10 + view.global_lexical_value_byte as i32]);
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
            if matches!(semantics, BindingSemantics::Write(_)) {
                dynasm!(ops ; .arch x64 ; test BYTE [r11 + view.object_flags_byte as i32], otter_vm::jit::JIT_OBJECT_FLAG_USED_AS_PROTOTYPE as i8 ; jnz =>miss);
            }
            if dictionary {
                symbolic(
                    ops,
                    relocations,
                    9,
                    view.cage_base as u64,
                    RelocationTarget::GcCageBase,
                );
                dynasm!(ops
                    ; .arch x64
                    ; mov r10d, [r11 + view.object_shape_byte as i32]
                    ; test BYTE [r9 + r10 + view.shape_kind_byte as i32], otter_vm::jit::JIT_SHAPE_KIND_DICTIONARY as i8
                    ; jz =>miss
                    ; mov r10d, [r11 + view.object_exotic_handle_byte as i32]
                    ; test r10d, r10d
                    ; jz =>miss
                );
                load64(ops, 8, shape);
                dynasm!(ops
                    ; .arch x64
                    ; add r10, r9
                    ; cmp [r10 + view.exotic_dictionary_layout_byte as i32], r8d
                    ; jne =>miss
                );
            } else {
                dynasm!(ops
                    ; .arch x64
                    ; cmp DWORD [r11 + view.object_shape_byte as i32], shape as i32
                    ; jne =>miss
                );
            }
            slab_base(ops, relocations, view);
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
                BindingRead::Global { .. } | BindingRead::ContextSlot { .. } => {
                    load_integer(ops, frame, locations[1], 11)?;
                    dynasm!(ops ; .arch x64 ; mov r11, [r11]);
                }
                BindingRead::LookupSlot { .. }
                | BindingRead::LookupGlobal { .. }
                | BindingRead::ResolveRef { .. } => {
                    return Err(Unsupported::OperandShape("x86-64 context binding hit"));
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

/// Displacement of one aligned context or closure word.
fn context_word(offset: u32) -> Result<i32, Unsupported> {
    offset
        .is_multiple_of(8)
        .then(|| i32::try_from(offset).ok())
        .flatten()
        .ok_or(Unsupported::OperandShape("x86-64 context word offset"))
}

/// Displacement of context slot `slot` from the context's header address.
fn context_slot_offset(view: &JitCompileSnapshot, slot: u16) -> Result<i32, Unsupported> {
    view.context_layout
        .slots_byte
        .checked_add(u32::from(slot) * 8)
        .ok_or(Unsupported::OperandShape("x86-64 context slot offset"))
        .and_then(context_word)
}

fn binding_requires_live_cell(semantics: BindingSemantics) -> bool {
    matches!(
        semantics,
        BindingSemantics::Read(BindingRead::Global { .. })
            | BindingSemantics::Write(
                BindingWrite::Global { .. } | BindingWrite::GlobalChecked { .. }
            )
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
    stamp_call_site(ops, site);
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
                .map_err(|_| Unsupported::OperandShape("x86-64 value-span slot"))
                .and_then(|offset| {
                    i32::try_from(offset)
                        .map_err(|_| Unsupported::OperandShape("x86-64 value-span slot"))
                })?;
            dynasm!(ops ; .arch x64 ; mov [rsp + offset], r10);
        }
        let offset = frame
            .raw_offset(packet.raw_start)
            .map_err(|_| Unsupported::OperandShape("x86-64 value-span base"))
            .and_then(|offset| {
                i32::try_from(offset)
                    .map_err(|_| Unsupported::OperandShape("x86-64 value-span base"))
            })?;
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
    allocation: &AllocatedSequence,
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
    stamp_call_site(ops, site);
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
    reload_roots_preserving_r11(ops, frame, site)?;
    match descriptor.exceptional {
        ExceptionalEdge::LandingPad(target) => {
            store_integer(ops, frame, result_location, 11)?;
            // The normal path's moves after the call and around the block
            // terminator run on the exceptional edge too.
            edit_run(ops, allocation.exceptional_edge_edits(id), frame)?;
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
    reload_roots_preserving_r11(ops, frame, site)?;
    dynasm!(ops ; .arch x64 ; jmp =>fatal_exit ; =>completed);
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
    stamp_call_site(ops, site);
    load64(ops, 6, VALUE_UNDEFINED);
    load64(ops, 2, VALUE_UNDEFINED);
    for index in 0..semantic_arity {
        let destination = [6_u8, 2, 1][index];
        if named_property && index + 1 == semantic_arity {
            load_integer_with_bias(ops, frame, locations[index], destination, 0)?;
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
    reload_roots(ops, frame, site)?;
    store_integer(ops, frame, locations[*payload_index], 8)?;
    store_integer(ops, frame, locations[*status_index], 9)?;
    Ok(())
}

/// Name the safepoint of the call about to be made in the executing frame's
/// record; a collection during the call resolves it to the saved root homes
/// at the frame's `machine_roots` base.
fn stamp_call_site(ops: &mut Assembler, site: &MachineSafepointSite) {
    dynasm!(ops
        ; .arch x64
        ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov DWORD [r10 + NATIVE_FRAME_CALL_SITE_OFFSET as i32], site.id.0 as i32
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
    let offset = i32::try_from(root_offset(frame, root.save_slot)?)
        .ok()
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



/// Read interpreter frame register `frame_register` into `location` as
/// `value_type`, jumping to `reject` when the tagged value lies outside it.
fn osr_value(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    frame_register: u16,
    value_type: MachineOsrType,
    location: AllocatedLocation,
    reject: DynamicLabel,
) -> Result<(), Unsupported> {
    load_osr_source(ops, frame_register);
    match value_type {
        MachineOsrType::Tagged => store_osr_integer(ops, frame, location, 11),
        MachineOsrType::Int32 => {
            guard_int32(ops, 11, reject);
            load_osr_source(ops, frame_register);
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
    dynasm!(ops ; .arch x64
        ; test BYTE [r11 + view.object_flags_byte as i32], (otter_vm::jit::JIT_OBJECT_FLAG_SLOT_ATTRS_OVERRIDDEN | otter_vm::jit::JIT_OBJECT_FLAG_CHAIN_LINK_OPAQUE | otter_vm::jit::JIT_OBJECT_FLAG_DICTIONARY_COMPATIBLE) as i8
        ; jnz =>miss);
    if let Some(validity) = guard.prototype_validity {
        symbolic(
            ops,
            relocations,
            10,
            validity.address as u64,
            RelocationTarget::PrototypeValidityCell {
                identity: validity.identity,
            },
        );
        dynasm!(ops ; .arch x64 ; cmp DWORD [r10], 0 ; je =>miss);
        load64(ops, 10, u64::from(guard.holder_root));
        dynasm!(ops ; .arch x64 ; mov r11d, [rax + r10 + view.shape_prototype_byte as i32]
            ; add r11, rax);
    }
    slab_base(ops, relocations, view);
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

/// `this` of a spliced reduced `f.call(receiver, ...)` (§10.2.1.2
/// OrdinaryCallBindThis): r9 holds the proven callable and rax the receiver,
/// which receives the binding. A bound-`this` closure keeps its own; a sloppy
/// callee binds an Object receiver, the global object for a nullish one, and
/// misses on a primitive or a bound sloppy closure.
fn emit_inline_explicit_this(
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
    if this_mode == otter_vm::JitDirectCallThisMode::SloppyGlobal {
        let global_this = ops.new_dynamic_label();
        load64(ops, 10, VALUE_UNDEFINED);
        dynasm!(ops ; .arch x64 ; cmp rax, r10 ; je =>global_this);
        load64(ops, 10, Value::null().to_bits());
        dynasm!(ops ; .arch x64 ; cmp rax, r10 ; je =>global_this);
        crate::x86_64::activation::emit_object_test(ops, view, 0, done, miss);
        dynasm!(ops
            ; .arch x64
            ; =>global_this
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
    dynasm!(ops ; .arch x64 ; =>done);
    Ok(())
}

/// Prove the tagged value in `r10` is directly storable in `access`'s
/// element representation, or branch to `miss`. Clobbers `r11`.
fn element_store_value_guard(
    ops: &mut Assembler,
    access: otter_vm::JitElementAccess,
    miss: DynamicLabel,
) {
    match access.element {
        otter_vm::jit::JitElementRepr::Boxed => {
            load64(ops, 11, NOT_CELL_MASK);
            dynasm!(ops ; .arch x64 ; test r10, r11 ; jz =>miss);
        }
        element if element.stores_int32() => {
            dynasm!(ops ; .arch x64 ; mov r11, r10 ; shr r11, 48 ; cmp r11w, NUMBER_TAG_HI16 as i16 ; jne =>miss);
        }
        _ => {
            // Float32 and Float64 store any number: a double rounds or
            // copies, an int32 converts exactly.
            dynasm!(ops ; .arch x64 ; mov r11, r10 ; shr r11, 48 ; test r11w, NUMBER_TAG_HI16 as i16 ; jz =>miss);
        }
    }
}

/// Read the element at `[r8]` in `representation`: a tagged or integer value
/// into `r10`, a double into the float `destination`. A hole in a boxed
/// element branches to `miss`. Clobbers `r10`, `r11` and `xmm14`.
fn element_value_read(
    ops: &mut Assembler,
    access: otter_vm::JitElementAccess,
    representation: MachineRepresentation,
    destination: AllocatedLocation,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    if representation == MachineRepresentation::Tagged {
        match access.element {
            otter_vm::jit::JitElementRepr::Boxed => {
                dynasm!(ops ; .arch x64 ; mov r10, [r8]);
                load64(ops, 11, Value::hole().to_bits());
                dynasm!(ops ; .arch x64 ; cmp r10, r11 ; je =>miss);
            }
            otter_vm::jit::JitElementRepr::Int32 => {
                dynasm!(ops ; .arch x64 ; mov r10d, [r8]);
                load64(ops, 11, NUMBER_TAG);
                dynasm!(ops ; .arch x64 ; or r10, r11);
            }
            // Narrow loads extend into the 32-bit register, whose write
            // clears the upper half: exactly the int32 box payload.
            otter_vm::jit::JitElementRepr::Int8 => {
                dynasm!(ops ; .arch x64 ; movsx r10d, BYTE [r8]);
                load64(ops, 11, NUMBER_TAG);
                dynasm!(ops ; .arch x64 ; or r10, r11);
            }
            otter_vm::jit::JitElementRepr::Uint8 | otter_vm::jit::JitElementRepr::Uint8Clamped => {
                dynasm!(ops ; .arch x64 ; movzx r10d, BYTE [r8]);
                load64(ops, 11, NUMBER_TAG);
                dynasm!(ops ; .arch x64 ; or r10, r11);
            }
            otter_vm::jit::JitElementRepr::Int16 => {
                dynasm!(ops ; .arch x64 ; movsx r10d, WORD [r8]);
                load64(ops, 11, NUMBER_TAG);
                dynasm!(ops ; .arch x64 ; or r10, r11);
            }
            otter_vm::jit::JitElementRepr::Uint16 => {
                dynasm!(ops ; .arch x64 ; movzx r10d, WORD [r8]);
                load64(ops, 11, NUMBER_TAG);
                dynasm!(ops ; .arch x64 ; or r10, r11);
            }
            otter_vm::jit::JitElementRepr::Uint32 => {
                // The zero-extended 64-bit integer converts exactly.
                dynasm!(ops ; .arch x64 ; mov r10d, [r8] ; cvtsi2sd xmm14, r10);
                box_number(ops, 14, 10);
            }
            otter_vm::jit::JitElementRepr::Float32 => {
                dynasm!(ops ; .arch x64 ; cvtss2sd xmm14, DWORD [r8]);
                box_number(ops, 14, 10);
            }
            otter_vm::jit::JitElementRepr::Float64 => {
                dynasm!(ops ; .arch x64 ; movsd xmm14, [r8]);
                box_number(ops, 14, 10);
            }
        }
    } else if representation == MachineRepresentation::Float64 {
        let destination = freg(destination)?;
        if access.element == otter_vm::JitElementRepr::Float32 {
            dynasm!(ops ; .arch x64 ; cvtss2sd Rx(destination), DWORD [r8]);
        } else {
            dynasm!(ops ; .arch x64 ; movsd Rx(destination), [r8]);
        }
    } else {
        use otter_vm::JitElementRepr as E;
        match access.element {
            E::Int8 => dynasm!(ops ; .arch x64 ; movsx r10d, BYTE [r8]),
            E::Uint8 | E::Uint8Clamped => {
                dynasm!(ops ; .arch x64 ; movzx r10d, BYTE [r8])
            }
            E::Int16 => dynasm!(ops ; .arch x64 ; movsx r10d, WORD [r8]),
            E::Uint16 => dynasm!(ops ; .arch x64 ; movzx r10d, WORD [r8]),
            E::Int32 | E::Uint32 => dynasm!(ops ; .arch x64 ; mov r10d, [r8]),
            _ => {
                return Err(Unsupported::OperandShape(
                    "scalar element load representation",
                ));
            }
        }
    }
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

/// Store two integer results whose sources are allocatable registers.
///
/// A destination may be the other result's source: that result is written
/// first, and a two-register cycle swaps in place.
fn store_outputs_ordered(
    ops: &mut Assembler,
    frame: MachineFrameLayout,
    outputs: [(AllocatedLocation, u8); 2],
) -> Result<(), Unsupported> {
    let [(first_location, first), (second_location, second)] = outputs;
    let writes = |location: AllocatedLocation, source: u8| {
        matches!(location, AllocatedLocation::Register(register)
            if register.is_integer() && register.encoding() == source)
    };
    if writes(first_location, second) && writes(second_location, first) {
        dynasm!(ops ; .arch x64 ; xchg Rq(first), Rq(second));
        return Ok(());
    }
    if writes(first_location, second) {
        store_integer(ops, frame, second_location, second)?;
        store_integer(ops, frame, first_location, first)
    } else {
        store_integer(ops, frame, first_location, first)?;
        store_integer(ops, frame, second_location, second)
    }
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
    object_flags_guard(ops, view, otter_vm::jit::JIT_OBJECT_SHAPE_STATE_MASK, miss);
}

fn ordinary_lookup_state_guard(ops: &mut Assembler, view: &JitCompileSnapshot, miss: DynamicLabel) {
    object_flags_guard(
        ops,
        view,
        otter_vm::jit::JIT_OBJECT_ORDINARY_LOOKUP_MASK,
        miss,
    );
}

/// Branch to `miss` unless in-object slot `slot` exists in the object whose
/// header is in `r11` (its `u8` in-object capacity is above `slot`).
fn inline_capacity_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    slot: u32,
    miss: DynamicLabel,
) {
    if slot > u32::from(u8::MAX) {
        dynasm!(ops ; .arch x64 ; jmp =>miss);
        return;
    }
    dynasm!(ops
        ; .arch x64
        ; cmp BYTE [r11 + view.object_inline_capacity_byte as i32], slot as u8 as i8
        ; jbe =>miss
    );
}

/// Branch to `miss` when any bit of `mask` is set in the flag byte of the
/// object whose header is in `r11`.
fn object_flags_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    mask: u8,
    miss: DynamicLabel,
) {
    dynasm!(ops
        ; .arch x64
        ; test BYTE [r11 + view.object_flags_byte as i32], mask as i8
        ; jnz =>miss
    );
}

/// Slot base of the object whose header is in `r11`, into `r8` (`r10` is
/// clobbered): its in-object slots while the slab handle is null, otherwise
/// the slab's words — cage base plus handle plus the fixed word offset.
fn slab_base(ops: &mut Assembler, relocations: &mut RelocationCapture, view: &JitCompileSnapshot) {
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
        ; add r8, r10
        ; add r8, view.object_slab_words_byte as i32
        ; =>done
    );
}

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
            cached_data_byte: _,
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
            );
            // Byte extent of the view: length << stride, rejecting a length
            // whose extent overflows 64 bits (`shl` sets OF only for 1-bit
            // shifts, so the discarded high bits are tested explicitly).
            let shift = access.element.stride_shift() as i8;
            if shift != 0 {
                dynasm!(ops
                    ; .arch x64
                    ; mov r10, r9
                    ; shr r10, 64 - shift
                    ; jnz =>miss
                );
            }
            dynasm!(ops
                ; .arch x64
                ; mov r10, r9
                ; shl r10, shift
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
