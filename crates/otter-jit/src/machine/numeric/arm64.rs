//! AArch64 emission for allocated scalar Machine IR.
//!
//! # Contents
//! - [`emit`] — emits one scalar function from allocator locations.
//! - Exact JavaScript Number decode, scalar coercion leaves, canonical boxing,
//!   and shared cold exits.
//! - regalloc2 edit emission between selected instructions.
//!
//! # Invariants
//! - Callee-saved `x19` retains the entry context; allocated `x20..x28` and
//!   `d8..d15` have one exact allocation-driven save/restore set.
//! - `x15..x18`, `d16..d31` are emitter/platform scratch excluded from
//!   allocation; cold deopt dumps use the target's complete register-id map.
//! - Spill storage and offsets come only from [`MachineFrameLayout`].
//! - Allocating calls copy late-use tagged roots into the layout's native save
//!   area before constructing the VM allocation packet. Direct base constructs
//!   probe non-reentrant receiver allocation before an observable fallback;
//!   reentrant direct calls link the same homes through the VM-owned root chain.
//!   Both reload every collector-rewritten value before success, throw, or
//!   exact deoptimization.
//! - A failed entry Number guard writes logical PC zero. Mid-function tagged
//!   numeric guards use allocator-driven exact deopt state at their owning
//!   bytecode operation, always before externally visible effects.
//! - Checked integer overflow uses the allocator-driven VM [`DeoptRuntime`];
//!   the emitter owns no parallel reconstruction recipe.
//! - Settled dense and typed element accesses keep receiver, index, and store
//!   value in allocator-owned late locations while one shared guard program
//!   proves the VM-baked layout. A guard miss deoptimizes at the original
//!   operation before effects; the generated hit cannot allocate or reenter.
//! - Settled ordinary named properties consume the VM's complete monomorphic
//!   or polymorphic shape/slot chain directly. Loads remain tagged; stores
//!   guard the whole chain before one commit. Tagged values use the `x19`
//!   Machine context for the post-commit generational barrier, while values
//!   proven non-cell before boxing omit that barrier entirely. Dense-array and
//!   primitive-string `.length` reads use the shared exotic layout guard before
//!   that chain.
//! - Captured-binding reads validate the current native frame's cell count,
//!   spine, cell type, and TDZ state, then load directly without reentry. Any
//!   miss deoptimizes at the original read.
//! - Backedge polls run before allocator edge edits and preserve every value
//!   live into the loop header across the leaf runtime call. An `x29` local
//!   countdown amortizes shared interrupt and fuel-cell traffic over the same
//!   bounded batch used by the general optimizing tier.
//! - OSR trampolines decode only live loop-header inputs into the exact
//!   late-use locations selected by regalloc2; rejection never mutates VM slots.
//! - Successful results use the VM's canonical tagged representation.
//! - Pure scalar leaves exchange unboxed scalars in fixed ABI operands;
//!   regalloc2 owns every argument/result move and no frame shuttle exists.
//! - Plain, guarded-method, and fixed-arity base/derived/super JavaScript calls
//!   reuse the shared generated linkage emitter; the allocator supplies only
//!   location-aware loads, stores, root reloads, and landing-pad result homes.

// dynasm's dynamic-register expansion calls `.into()` on the required u8
// register encoding. Clippy sees the macro expansion as an identity conversion.
#![allow(clippy::useless_conversion)]

use dynasmrt::{AssemblyOffset, DynamicLabel, DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::{
    JitCompileSnapshot, NativeFrameFlags, UPVALUE_CELL_TYPE_TAG, Value,
    deopt::DeoptRuntime,
    native_abi::{
        RuntimeStubDescriptor, STUB_ARRAY_CONSTRUCT_ALLOC, STUB_JIT_BACKEDGE_POLL,
        STUB_JIT_BIND_DERIVED_THIS, STUB_JIT_CLASS_SUPER_CONSTRUCTOR, STUB_JIT_DEOPT_WRITEBACK,
        STUB_NUMBER_POW_F64_LEAF, STUB_NUMBER_REM_F64_LEAF, STUB_NUMBER_TO_INT32_F64_LEAF,
        STUB_STRICT_EQ_LEAF, STUB_STRING_CONCAT_ALLOC, STUB_TO_BOOLEAN_LEAF,
    },
};

use super::super::{
    AllocatedLocation, AllocatedSequence, AllocationEdit, AllocationPoint, CallTarget, DeoptId,
    DirectCallArgumentMode, DirectCallKind, InstructionSequence, MachineFrameLayout,
    MachineInstructionId, MachineOpcode, MachineOsrInput, MachineOsrType, MachineRepresentation,
    MachineSafepointSite, MachineSafepointTable,
};
use crate::{
    CompiledCode, Unsupported,
    arm64::{
        DirectCallArguments, DirectCallForm, DirectCallSite, GENERATED_POLL_BATCH,
        emit_direct_call_with_access, emit_method_guard_from_tagged_register,
    },
    artifact::relocation::{RelocationCapture, RelocationTarget},
    entry::{
        ALLOC_CTX_SAFEPOINT_ID_OFFSET, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET,
        ALLOC_CTX_SPILL_SLOTS_OFFSET, ALLOC_CTX_STACK_SIZE, ALLOC_CTX_THREAD_OFFSET,
        CANONICAL_NAN_HI16, DOUBLE_OFFSET_HI16, MACHINE_ROOT_RECORD_BASE_OFFSET,
        MACHINE_ROOT_RECORD_CODE_OBJECT_ID_OFFSET, MACHINE_ROOT_RECORD_COUNT_OFFSET,
        MACHINE_ROOT_RECORD_PREVIOUS_OFFSET, MACHINE_ROOT_RECORD_SAFEPOINT_ID_OFFSET,
        MACHINE_ROOT_RECORD_SIZE, MACHINE_ROOTS_PTR_OFFSET, NATIVE_FRAME_FLAGS_OFFSET,
        NATIVE_FRAME_OFFSET, NATIVE_FRAME_PC_OFFSET, NATIVE_FRAME_REGISTER_BASE_OFFSET,
        NATIVE_FRAME_REGISTER_COUNT_OFFSET, NATIVE_FRAME_THIS_OFFSET,
        NATIVE_FRAME_UPVALUE_BASE_OFFSET, NATIVE_FRAME_UPVALUE_COUNT_OFFSET, NUMBER_TAG_HI16,
        OBJECT_BODY_TYPE_TAG, STATUS_BAILED, STATUS_RETURNED, STATUS_THREW, THREAD_OFFSET,
        VALUE_HOLE, VALUE_UNDEFINED, VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET,
        VM_THREAD_CODE_OBJECT_ID_OFFSET, VM_THREAD_GC_HEAP_OFFSET, VM_THREAD_INTERRUPT_CELL_OFFSET,
    },
    template::arm64::ic_probe::{
        DenseIndexForm, element_access_for, emit_element_address, emit_element_read,
        emit_element_write, emit_exotic_length_fast, emit_settled_property_load,
        emit_settled_property_store_guard,
    },
    template::arm64::values::{
        CellTest, emit_cell_test, emit_slab_base, emit_write_barrier_with_context,
    },
};
use std::collections::BTreeMap;

// The deopt namespace preserves physical encodings. It includes one unused
// x29 slot so the following FP bank remains naturally 16-byte aligned.
pub(super) const GPR_BUDGET: u16 = 30;
pub(super) const FP_BUDGET: u16 = 16;
const BASE_FIXED_FRAME_BYTES: u32 = 32;
const DEOPT_DUMP_BYTES: u32 = (GPR_BUDGET as u32 + FP_BUDGET as u32) * 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SavedFrame {
    gpr_count: u8,
    fp_count: u8,
}

impl SavedFrame {
    fn from_allocation(allocation: &AllocatedSequence) -> Self {
        let mut gpr_count = 0;
        let mut fp_count = 0;
        for register in allocation
            .used_registers()
            .filter(|register| register.is_integer() && (20..=28).contains(&register.encoding()))
        {
            gpr_count = gpr_count.max(register.encoding() - 19);
        }
        for register in allocation
            .used_registers()
            .filter(|register| register.is_float() && (8..=15).contains(&register.encoding()))
        {
            fp_count = fp_count.max(register.encoding() - 7);
        }
        Self {
            gpr_count,
            fp_count,
        }
    }

    fn fixed_bytes(self) -> u32 {
        BASE_FIXED_FRAME_BYTES
            + u32::from(self.gpr_count.saturating_sub(1))
                .saturating_mul(8)
                .next_multiple_of(16)
            + u32::from(self.fp_count)
                .saturating_mul(8)
                .next_multiple_of(16)
    }
}

pub(super) struct Emission {
    pub(super) code: CompiledCode,
    pub(super) generated_stack_frame_bytes: u32,
    pub(super) relocations: RelocationCapture,
    pub(super) osr_entries: BTreeMap<u32, usize>,
    pub(super) osr_regions: Vec<(u32, usize, usize)>,
    pub(super) constructor_field_regions: Vec<(u32, usize, usize)>,
    pub(super) structural_regions: Vec<(&'static str, Option<u32>, usize, usize)>,
}

struct OsrSite {
    instruction: MachineInstructionId,
    logical_pc: u32,
    inputs: Vec<MachineOsrInput>,
    continuation: DynamicLabel,
}

fn instruction_deopt_label(
    deopt: Option<DeoptId>,
    labels: &[DynamicLabel],
) -> Result<DynamicLabel, Unsupported> {
    deopt
        .and_then(|deopt| labels.get(deopt.0 as usize).copied())
        .ok_or(Unsupported::OperandShape(
            "numeric checked operation deopt label",
        ))
}

fn emit_backedge_poll(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    poll_entry: u64,
    bailout: DynamicLabel,
    threw: DynamicLabel,
) {
    let batched = ops.new_dynamic_label();
    let slow = ops.new_dynamic_label();
    let cont = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; subs w29, w29, #1
        ; b.ne =>batched
        ; movz w29, GENERATED_POLL_BATCH
        ; ldr x17, [x19, THREAD_OFFSET]
        ; ldr x16, [x17, VM_THREAD_INTERRUPT_CELL_OFFSET]
        ; ldrb w16, [x16]
        ; cbnz w16, =>bailout
        ; ldr x16, [x17, VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET]
        ; ldr x17, [x16]
        ; subs x17, x17, GENERATED_POLL_BATCH
        ; str x17, [x16]
        ; b.gt =>cont
        ; =>slow
        ; mov x0, x19
    );
    emit_load_symbolic_u64(
        ops,
        relocations,
        16,
        poll_entry,
        RelocationTarget::runtime_stub(STUB_JIT_BACKEDGE_POLL),
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbnz x0, =>threw
        ; =>cont
        ; =>batched
    );
}

fn emit_float_leaf_binary(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    entry: u64,
    descriptor: RuntimeStubDescriptor,
) {
    emit_load_symbolic_u64(
        ops,
        relocations,
        16,
        entry,
        RelocationTarget::runtime_stub(descriptor),
    );
    dynasm!(ops ; .arch aarch64 ; blr x16);
}

fn emit_float_to_int32_leaf(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    entry: u64,
) {
    emit_load_symbolic_u64(
        ops,
        relocations,
        16,
        entry,
        RelocationTarget::runtime_stub(STUB_NUMBER_TO_INT32_F64_LEAF),
    );
    dynasm!(ops ; .arch aarch64 ; blr x16);
}

pub(super) fn frame_layout(
    allocation: &AllocatedSequence,
    root_slots: u16,
) -> Result<MachineFrameLayout, Unsupported> {
    MachineFrameLayout::new(
        allocation,
        root_slots,
        SavedFrame::from_allocation(allocation).fixed_bytes(),
        16,
    )
    .map_err(|_| Unsupported::OperandShape("scalar Machine IR frame layout"))
}

pub(super) fn emit(
    view: &JitCompileSnapshot,
    sequence: &InstructionSequence,
    allocation: &AllocatedSequence,
    frame: MachineFrameLayout,
    deopt_runtime: &DeoptRuntime,
    safepoints: &MachineSafepointTable,
    poll_entry: u64,
    deopt_writeback_entry: u64,
    deopt_stack_call_entry: u64,
    resolve_direct_entry: u64,
    try_prepare_construct_entry: u64,
    prepare_construct_entry: u64,
    derived_construct_result_entry: u64,
    copy_spread_arguments_entry: u64,
    initialize_upvalues_entry: u64,
    bind_derived_this_entry: u64,
    string_concat_entry: u64,
    array_construct_entry: u64,
    number_rem_entry: u64,
    number_pow_entry: u64,
    number_to_int32_entry: u64,
    strict_eq_entry: u64,
    to_boolean_entry: u64,
    vm_register_count: u16,
    capture_artifacts: bool,
) -> Result<Emission, Unsupported> {
    reject_unimplemented_locations(sequence, allocation)?;
    let saved = SavedFrame::from_allocation(allocation);
    if frame.fixed_bytes() != saved.fixed_bytes() {
        return Err(Unsupported::OperandShape(
            "scalar Machine IR saved frame layout",
        ));
    }
    let mut ops = dynasmrt::aarch64::Assembler::new()
        .map_err(|_| Unsupported::Backend(crate::BackendFailure::AssemblerAllocation))?;
    let bail = ops.new_dynamic_label();
    let threw = ops.new_dynamic_label();
    let shared_deopt = ops.new_dynamic_label();
    let deopt_labels = deopt_runtime
        .exits
        .iter()
        .map(|_| ops.new_dynamic_label())
        .collect::<Vec<_>>();
    let mut relocations = RelocationCapture::new(capture_artifacts);
    let mut constructor_field_regions = Vec::new();
    let mut structural_regions = Vec::new();
    let block_labels = sequence
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
        if inputs.len() != instruction.operands.len() {
            return Err(Unsupported::OperandShape("scalar OSR input arity"));
        }
        osr_sites.push(OsrSite {
            instruction: MachineInstructionId(index as u32),
            logical_pc,
            inputs: inputs.clone(),
            continuation: ops.new_dynamic_label(),
        });
    }
    let has_backedge_poll = sequence
        .instructions()
        .iter()
        .any(|instruction| instruction.opcode == MachineOpcode::BackedgePoll);

    emit_prologue(&mut ops, frame, saved);
    if has_backedge_poll {
        dynasm!(ops ; .arch aarch64 ; movz w29, GENERATED_POLL_BATCH);
    }

    for (index, instruction) in sequence.instructions().iter().enumerate() {
        let id = MachineInstructionId(index as u32);
        let block_index = block_for_instruction(sequence, id)?;
        if sequence.blocks()[block_index].first == id {
            let label = block_labels[block_index];
            dynasm!(ops ; .arch aarch64 ; =>label);
        }
        emit_edits(
            &mut ops,
            allocation.edits(),
            AllocationPoint::Before(id),
            frame,
        )?;
        let is_terminator = instruction.control != super::super::ControlFlow::None;
        if is_terminator {
            emit_edits(
                &mut ops,
                allocation.edits(),
                AllocationPoint::After(id),
                frame,
            )?;
        }
        let locations = allocation
            .instruction_locations(id)
            .ok_or(Unsupported::OperandShape("scalar Machine IR locations"))?;
        match instruction.opcode {
            MachineOpcode::OsrEntry { .. } => {
                let site = osr_sites
                    .iter()
                    .find(|site| site.instruction == id)
                    .ok_or(Unsupported::OperandShape("scalar OSR continuation"))?;
                let continuation = site.continuation;
                dynasm!(ops ; .arch aarch64 ; =>continuation);
            }
            MachineOpcode::EntryValue(parameter) => {
                let destination = integer_register(locations[0])?;
                let offset = u32::from(parameter)
                    .checked_mul(8)
                    .ok_or(Unsupported::OperandShape("numeric parameter offset"))?;
                dynasm!(ops ; .arch aarch64 ; ldr X(destination), [x17, offset]);
            }
            MachineOpcode::EntryThis => {
                let destination = integer_register(locations[0])?;
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr x16, [x19, NATIVE_FRAME_OFFSET]
                    ; ldr X(destination), [x16, NATIVE_FRAME_THIS_OFFSET]
                );
            }
            MachineOpcode::TaggedConstant(bits) => {
                emit_load_u64(&mut ops, integer_register(locations[0])?, bits);
            }
            MachineOpcode::DecodeNumber => {
                let miss = if instruction.deopt.is_some() {
                    instruction_deopt_label(instruction.deopt, &deopt_labels)?
                } else {
                    bail
                };
                emit_decode_number(
                    &mut ops,
                    integer_register(locations[0])?,
                    float_register(locations[1])?,
                    miss,
                );
            }
            MachineOpcode::DecodeInt32 => {
                let source = integer_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                if source != destination {
                    return Err(Unsupported::OperandShape(
                        "numeric Int32 decode reuse allocation",
                    ));
                }
                let miss = if instruction.deopt.is_some() {
                    instruction_deopt_label(instruction.deopt, &deopt_labels)?
                } else {
                    bail
                };
                emit_decode_int32(&mut ops, source, miss);
            }
            MachineOpcode::FloatConstant(bits) => {
                emit_load_u64(&mut ops, 16, bits);
                let destination = float_register(locations[0])?;
                dynasm!(ops ; .arch aarch64 ; fmov D(destination), x16);
            }
            MachineOpcode::FloatAdd
            | MachineOpcode::FloatSub
            | MachineOpcode::FloatMul
            | MachineOpcode::FloatDiv => {
                let left = float_register(locations[0])?;
                let right = float_register(locations[1])?;
                let destination = float_register(locations[2])?;
                match instruction.opcode {
                    MachineOpcode::FloatAdd => {
                        dynasm!(ops ; .arch aarch64 ; fadd D(destination), D(left), D(right));
                    }
                    MachineOpcode::FloatSub => {
                        dynasm!(ops ; .arch aarch64 ; fsub D(destination), D(left), D(right));
                    }
                    MachineOpcode::FloatMul => {
                        dynasm!(ops ; .arch aarch64 ; fmul D(destination), D(left), D(right));
                    }
                    MachineOpcode::FloatDiv => {
                        dynasm!(ops ; .arch aarch64 ; fdiv D(destination), D(left), D(right));
                    }
                    _ => unreachable!("matched floating binary operation"),
                }
            }
            MachineOpcode::FloatRem | MachineOpcode::FloatPow => {
                if float_register(locations[0])? != 0
                    || float_register(locations[1])? != 1
                    || float_register(locations[2])? != 0
                {
                    return Err(Unsupported::OperandShape("numeric FP leaf ABI"));
                }
                let (entry, descriptor) = if instruction.opcode == MachineOpcode::FloatRem {
                    (number_rem_entry, STUB_NUMBER_REM_F64_LEAF)
                } else {
                    (number_pow_entry, STUB_NUMBER_POW_F64_LEAF)
                };
                emit_float_leaf_binary(&mut ops, &mut relocations, entry, descriptor);
            }
            MachineOpcode::Float64ToInt32 => {
                if float_register(locations[0])? != 0 || integer_register(locations[1])? != 0 {
                    return Err(Unsupported::OperandShape("numeric ToInt32 leaf ABI"));
                }
                emit_float_to_int32_leaf(&mut ops, &mut relocations, number_to_int32_entry);
            }
            MachineOpcode::FloatNeg => {
                let source = float_register(locations[0])?;
                let destination = float_register(locations[1])?;
                dynasm!(ops ; .arch aarch64 ; fneg D(destination), D(source));
            }
            MachineOpcode::FloatEqual
            | MachineOpcode::FloatNotEqual
            | MachineOpcode::FloatLessThan
            | MachineOpcode::FloatLessEqual
            | MachineOpcode::FloatGreaterThan
            | MachineOpcode::FloatGreaterEqual => {
                let left = float_register(locations[0])?;
                let right = float_register(locations[1])?;
                let destination = integer_register(locations[2])?;
                dynasm!(ops ; .arch aarch64 ; fcmp D(left), D(right));
                match instruction.opcode {
                    MachineOpcode::FloatEqual => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), eq);
                    }
                    MachineOpcode::FloatNotEqual => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), ne);
                    }
                    MachineOpcode::FloatLessThan => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), mi);
                    }
                    MachineOpcode::FloatLessEqual => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), ls);
                    }
                    MachineOpcode::FloatGreaterThan => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), gt);
                    }
                    MachineOpcode::FloatGreaterEqual => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), ge);
                    }
                    _ => unreachable!("matched Float64 comparison"),
                }
            }
            MachineOpcode::IntegerConstant(value) => {
                emit_load_u64(
                    &mut ops,
                    integer_register(locations[0])?,
                    value as u32 as u64,
                );
            }
            MachineOpcode::Int32ToFloat64 => {
                let source = integer_register(locations[0])?;
                let destination = float_register(locations[1])?;
                dynasm!(ops ; .arch aarch64 ; scvtf D(destination), W(source));
            }
            MachineOpcode::Uint32ToFloat64 => {
                let source = integer_register(locations[0])?;
                let destination = float_register(locations[1])?;
                dynasm!(ops ; .arch aarch64 ; ucvtf D(destination), W(source));
            }
            MachineOpcode::BooleanToInt32 => {
                let source = integer_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                dynasm!(ops ; .arch aarch64 ; mov W(destination), W(source));
            }
            MachineOpcode::IntegerAdd | MachineOpcode::IntegerSub => {
                let left = integer_register(locations[0])?;
                let right = integer_register(locations[1])?;
                let destination = integer_register(locations[2])?;
                let exit = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                if instruction.opcode == MachineOpcode::IntegerAdd {
                    dynasm!(ops
                        ; .arch aarch64
                        ; adds W(destination), W(left), W(right)
                        ; b.vs =>exit
                    );
                } else {
                    dynasm!(ops
                        ; .arch aarch64
                        ; subs W(destination), W(left), W(right)
                        ; b.vs =>exit
                    );
                }
            }
            MachineOpcode::IntegerMul => {
                let left = integer_register(locations[0])?;
                let right = integer_register(locations[1])?;
                let destination = integer_register(locations[2])?;
                let exit = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                let nonzero = ops.new_dynamic_label();
                dynasm!(ops
                    ; .arch aarch64
                    ; smull x16, W(left), W(right)
                    ; sxtw x17, w16
                    ; cmp x16, x17
                    ; b.ne =>exit
                    ; cbnz w16, =>nonzero
                    ; eor w17, W(left), W(right)
                    ; tbnz w17, #31, =>exit
                    ; =>nonzero
                    ; mov W(destination), w16
                );
            }
            MachineOpcode::IntegerNeg => {
                let source = integer_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                let exit = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                dynasm!(ops
                    ; .arch aarch64
                    ; negs w16, W(source)
                    ; b.vs =>exit
                    ; cbz W(source), =>exit
                    ; mov W(destination), w16
                );
            }
            MachineOpcode::IntegerAddImmediate(immediate)
            | MachineOpcode::IntegerSubImmediate(immediate) => {
                let source = integer_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                let exit = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                emit_load_u64(&mut ops, 16, immediate as u32 as u64);
                if matches!(instruction.opcode, MachineOpcode::IntegerAddImmediate(_)) {
                    dynasm!(ops
                        ; .arch aarch64
                        ; adds W(destination), W(source), w16
                        ; b.vs =>exit
                    );
                } else {
                    dynasm!(ops
                        ; .arch aarch64
                        ; subs W(destination), W(source), w16
                        ; b.vs =>exit
                    );
                }
            }
            MachineOpcode::IntegerAnd
            | MachineOpcode::IntegerOr
            | MachineOpcode::IntegerXor
            | MachineOpcode::IntegerShiftLeft
            | MachineOpcode::IntegerShiftRight => {
                let left = integer_register(locations[0])?;
                let right = integer_register(locations[1])?;
                let destination = integer_register(locations[2])?;
                match instruction.opcode {
                    MachineOpcode::IntegerAnd => {
                        dynasm!(ops ; .arch aarch64 ; and W(destination), W(left), W(right));
                    }
                    MachineOpcode::IntegerOr => {
                        dynasm!(ops ; .arch aarch64 ; orr W(destination), W(left), W(right));
                    }
                    MachineOpcode::IntegerXor => {
                        dynasm!(ops ; .arch aarch64 ; eor W(destination), W(left), W(right));
                    }
                    // AArch64 variable 32-bit shifts use only the low five count bits,
                    // exactly matching JavaScript's shift-count normalization.
                    MachineOpcode::IntegerShiftLeft => {
                        dynasm!(ops ; .arch aarch64 ; lsl W(destination), W(left), W(right));
                    }
                    MachineOpcode::IntegerShiftRight => {
                        dynasm!(ops ; .arch aarch64 ; asr W(destination), W(left), W(right));
                    }
                    _ => unreachable!("matched binary integer operation"),
                }
            }
            MachineOpcode::IntegerNot => {
                let source = integer_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                dynasm!(ops ; .arch aarch64 ; mvn W(destination), W(source));
            }
            MachineOpcode::IntegerShiftRightLogical => {
                let left = integer_register(locations[0])?;
                let right = integer_register(locations[1])?;
                let destination = integer_register(locations[2])?;
                dynasm!(ops ; .arch aarch64 ; lsr W(destination), W(left), W(right));
            }
            MachineOpcode::IntegerAndImmediate(immediate) => {
                let source = integer_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                emit_load_u64(&mut ops, 16, immediate as u32 as u64);
                dynasm!(ops ; .arch aarch64 ; and W(destination), W(source), w16);
            }
            MachineOpcode::IntegerLessThanImmediate(immediate)
            | MachineOpcode::IntegerEqualImmediate(immediate)
            | MachineOpcode::IntegerNotEqualImmediate(immediate) => {
                let source = integer_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                emit_load_u64(&mut ops, 16, immediate as u32 as u64);
                dynasm!(ops ; .arch aarch64 ; cmp W(source), w16);
                match instruction.opcode {
                    MachineOpcode::IntegerLessThanImmediate(_) => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), lt);
                    }
                    MachineOpcode::IntegerEqualImmediate(_) => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), eq);
                    }
                    MachineOpcode::IntegerNotEqualImmediate(_) => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), ne);
                    }
                    _ => unreachable!("matched immediate integer comparison"),
                }
            }
            MachineOpcode::IntegerEqual
            | MachineOpcode::IntegerNotEqual
            | MachineOpcode::IntegerLessThan
            | MachineOpcode::IntegerLessEqual
            | MachineOpcode::IntegerGreaterThan
            | MachineOpcode::IntegerGreaterEqual => {
                let left = integer_register(locations[0])?;
                let right = integer_register(locations[1])?;
                let destination = integer_register(locations[2])?;
                dynasm!(ops ; .arch aarch64 ; cmp W(left), W(right));
                match instruction.opcode {
                    MachineOpcode::IntegerEqual => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), eq);
                    }
                    MachineOpcode::IntegerNotEqual => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), ne);
                    }
                    MachineOpcode::IntegerLessThan => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), lt);
                    }
                    MachineOpcode::IntegerLessEqual => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), le);
                    }
                    MachineOpcode::IntegerGreaterThan => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), gt);
                    }
                    MachineOpcode::IntegerGreaterEqual => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), ge);
                    }
                    _ => unreachable!("matched int32 comparison"),
                }
            }
            MachineOpcode::IntegerToBoolean => {
                let source = integer_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                dynasm!(ops
                    ; .arch aarch64
                    ; tst W(source), W(source)
                    ; cset W(destination), ne
                );
            }
            MachineOpcode::FloatToBoolean => {
                let source = float_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                dynasm!(ops
                    ; .arch aarch64
                    ; fmov d31, xzr
                    ; fcmp D(source), d31
                    ; cset W(destination), ne
                    ; cset w16, vc
                    ; and W(destination), W(destination), w16
                );
            }
            MachineOpcode::BooleanNot => {
                let source = integer_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                dynasm!(ops
                    ; .arch aarch64
                    ; mov w16, #1
                    ; eor W(destination), W(source), w16
                );
            }
            MachineOpcode::BackedgePoll => {
                let exit = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                emit_backedge_poll(&mut ops, &mut relocations, poll_entry, exit, threw);
            }
            MachineOpcode::BoxNumber => emit_box_number(
                &mut ops,
                float_register(locations[0])?,
                integer_register(locations[1])?,
            ),
            MachineOpcode::BoxInt32 => {
                let source = integer_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                dynasm!(ops
                    ; .arch aarch64
                    ; mov W(destination), W(source)
                    ; movz x16, NUMBER_TAG_HI16, lsl #48
                    ; orr X(destination), X(destination), x16
                );
            }
            MachineOpcode::BoxUint32 => emit_box_uint32(
                &mut ops,
                integer_register(locations[0])?,
                integer_register(locations[1])?,
            ),
            MachineOpcode::BoxBoolean => emit_box_boolean(
                &mut ops,
                integer_register(locations[0])?,
                integer_register(locations[1])?,
            ),
            MachineOpcode::LoadUpvalue { index, byte_pc } => {
                let index = u32::try_from(index)
                    .ok()
                    .filter(|index| *index <= 4095)
                    .ok_or(Unsupported::OperandShape("scalar upvalue load index"))?;
                if view.cage_base == 0 {
                    return Err(Unsupported::OperandShape("scalar upvalue load cage"));
                }
                let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                let start = ops.offset().0;
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr x10, [x19, NATIVE_FRAME_OFFSET]
                    ; ldr w11, [x10, NATIVE_FRAME_UPVALUE_COUNT_OFFSET]
                    ; cmp w11, index
                    ; b.ls =>deopt
                    ; ldr x9, [x10, NATIVE_FRAME_UPVALUE_BASE_OFFSET]
                    ; cbz x9, =>deopt
                    ; ldr w9, [x9, index * 4]
                    ; cbz w9, =>deopt
                );
                emit_load_symbolic_u64(
                    &mut ops,
                    &mut relocations,
                    13,
                    view.cage_base as u64,
                    RelocationTarget::GcCageBase,
                );
                dynasm!(ops
                    ; .arch aarch64
                    ; add x13, x13, x9
                    ; ldrb w10, [x13]
                    ; cmp w10, UPVALUE_CELL_TYPE_TAG as u32
                    ; b.ne =>deopt
                    ; ldr x9, [x13, view.upvalue_value_byte]
                );
                emit_load_u64(&mut ops, 11, VALUE_HOLE);
                dynasm!(ops
                    ; .arch aarch64
                    ; cmp x9, x11
                    ; b.eq =>deopt
                );
                emit_store_allocated_tagged(&mut ops, frame, locations[0], 9, 0)?;
                structural_regions.push((
                    "machineUpvalueLoad",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::PropertyLoad {
                byte_pc,
                exotic_length,
            } => {
                let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                let start = ops.offset().0;
                let done = ops.new_dynamic_label();

                // Dense-array and primitive-string `.length` values do not
                // live in an ordinary own-property slab, so no settled shape
                // chain can describe them. Try that shared exotic program
                // first. A non-exotic receiver then reloads the same late
                // allocator location for the ordinary settled proof below.
                if view.cage_base != 0 && exotic_length {
                    let have_length = ops.new_dynamic_label();
                    let not_length = ops.new_dynamic_label();
                    emit_load_allocated_tagged(&mut ops, frame, locations[0], 9, 0)?;
                    emit_exotic_length_fast(
                        &mut ops,
                        &mut relocations,
                        view,
                        have_length,
                        not_length,
                    );
                    dynasm!(ops ; .arch aarch64 ; =>have_length);
                    emit_store_allocated_tagged(&mut ops, frame, locations[1], 9, 0)?;
                    dynasm!(ops
                        ; .arch aarch64
                        ; b =>done
                        ; =>not_length
                    );
                }

                if let Some(chain) = (view.cage_base != 0)
                    .then(|| view.property_loads.get(&byte_pc))
                    .flatten()
                    .filter(|chain| !chain.is_empty())
                {
                    emit_settled_property_load(
                        &mut ops,
                        &mut relocations,
                        view,
                        chain,
                        |ops, target| {
                            emit_load_allocated_tagged(ops, frame, locations[0], target, 0)
                        },
                        deopt,
                    )?;
                    emit_store_allocated_tagged(&mut ops, frame, locations[1], 9, 0)?;
                } else {
                    // Missing settled metadata is an ordinary speculation
                    // miss, not a reason to discard the whole scalar body.
                    dynasm!(ops ; .arch aarch64 ; b =>deopt);
                }
                dynasm!(ops ; .arch aarch64 ; =>done);
                structural_regions.push((
                    "machinePropertyLoad",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::PropertyStore {
                byte_pc,
                value_is_non_cell,
            } => {
                let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                let start = ops.offset().0;
                if let Some(chain) = (view.cage_base != 0)
                    .then(|| view.property_stores.get(&byte_pc))
                    .flatten()
                    .filter(|chain| !chain.is_empty())
                {
                    emit_settled_property_store_guard(
                        &mut ops,
                        &mut relocations,
                        view,
                        chain,
                        |ops, target| {
                            emit_load_allocated_tagged(ops, frame, locations[0], target, 0)
                        },
                        deopt,
                    )?;

                    // Selection has already boxed the value and keeps it in a
                    // late allocator location. Nothing after this load may
                    // deopt: every observable guard has completed.
                    emit_load_allocated_tagged(&mut ops, frame, locations[1], 9, 0)?;
                    if value_is_non_cell {
                        // Scalar typing survived boxing in the opcode. No bit
                        // pattern produced by these box operations is a cell,
                        // so the commit cannot create a traced heap edge.
                        dynasm!(ops ; .arch aarch64 ; str x9, [x13, x17]);
                    } else {
                        let primitive = ops.new_dynamic_label();
                        let committed = ops.new_dynamic_label();
                        emit_cell_test(&mut ops, 9, 11, CellTest::IsNotCell, primitive);
                        dynasm!(ops ; .arch aarch64 ; str x9, [x13, x17]);
                        emit_write_barrier_with_context(
                            &mut ops,
                            &mut relocations,
                            view,
                            12,
                            9,
                            19,
                        );
                        dynasm!(ops
                            ; .arch aarch64
                            ; b =>committed
                            ; =>primitive
                            ; str x9, [x13, x17]
                            ; =>committed
                        );
                    }
                } else {
                    // No receiver/value load and no effect precedes this exact
                    // fallback; the interpreter re-executes StoreProperty.
                    dynasm!(ops ; .arch aarch64 ; b =>deopt);
                }
                structural_regions.push((
                    "machinePropertyStore",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::ElementLoad(byte_pc) => {
                let access = element_access_for(view, byte_pc)
                    .copied()
                    .ok_or(Unsupported::OperandShape("scalar element load access"))?;
                let index_value = instruction
                    .operands
                    .get(1)
                    .ok_or(Unsupported::OperandShape("scalar element load index"))?
                    .value;
                let index_form = match sequence
                    .representations()
                    .get(index_value.0 as usize)
                    .copied()
                {
                    Some(MachineRepresentation::Tagged) => DenseIndexForm::Tagged,
                    Some(MachineRepresentation::Int32 | MachineRepresentation::Uint32) => {
                        DenseIndexForm::Int32
                    }
                    _ => {
                        return Err(Unsupported::OperandShape(
                            "scalar element load index representation",
                        ));
                    }
                };
                let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                let start = ops.offset().0;
                emit_element_address(
                    &mut ops,
                    &mut relocations,
                    view,
                    &access,
                    |ops, target| emit_load_allocated_tagged(ops, frame, locations[0], target, 0),
                    |ops, target| emit_load_allocated_integer(ops, frame, locations[1], target, 0),
                    index_form,
                    deopt,
                )?;
                emit_element_read(&mut ops, access.element, deopt);
                emit_store_allocated_tagged(&mut ops, frame, locations[2], 9, 0)?;
                structural_regions.push((
                    "machineElementLoad",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::ElementStore(byte_pc) => {
                let access = element_access_for(view, byte_pc)
                    .copied()
                    .ok_or(Unsupported::OperandShape("scalar element store access"))?;
                let index_value = instruction
                    .operands
                    .get(1)
                    .ok_or(Unsupported::OperandShape("scalar element store index"))?
                    .value;
                let index_form = match sequence
                    .representations()
                    .get(index_value.0 as usize)
                    .copied()
                {
                    Some(MachineRepresentation::Tagged) => DenseIndexForm::Tagged,
                    Some(MachineRepresentation::Int32 | MachineRepresentation::Uint32) => {
                        DenseIndexForm::Int32
                    }
                    _ => {
                        return Err(Unsupported::OperandShape(
                            "scalar element store index representation",
                        ));
                    }
                };
                let value = instruction
                    .operands
                    .get(2)
                    .ok_or(Unsupported::OperandShape("scalar element store value"))?;
                let value_representation = sequence
                    .representations()
                    .get(value.value.0 as usize)
                    .copied()
                    .ok_or(Unsupported::OperandShape(
                        "scalar element store value representation",
                    ))?;
                let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                let start = ops.offset().0;
                emit_element_address(
                    &mut ops,
                    &mut relocations,
                    view,
                    &access,
                    |ops, target| emit_load_allocated_tagged(ops, frame, locations[0], target, 0),
                    |ops, target| emit_load_allocated_integer(ops, frame, locations[1], target, 0),
                    index_form,
                    deopt,
                )?;
                // A boxed hole is an absent property, so both reads and writes
                // must prove that the indexed slot already exists before the
                // generated store can commit its first effect.
                emit_element_read(&mut ops, access.element, deopt);
                if value_representation != MachineRepresentation::Tagged {
                    return Err(Unsupported::OperandShape(
                        "scalar element store tagged value",
                    ));
                }
                emit_load_allocated_tagged(&mut ops, frame, locations[2], 9, 0)?;
                emit_element_write(&mut ops, access.element, deopt);
                structural_regions.push((
                    "machineElementStore",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::ConstructorFieldStore(byte_pc) => {
                let transition = view
                    .constructor_field_transitions
                    .get(&byte_pc)
                    .ok_or(Unsupported::OperandShape("constructor field transition"))?;
                let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                let start = ops.offset().0;
                // x9-x16 are emitter scratch registers, and regalloc may also
                // place the field value in x9. Preserve the early-use value
                // in the non-allocatable x17 before the receiver guards
                // overwrite any allocator-owned scratch home.
                emit_load_allocated_tagged(&mut ops, frame, locations[1], 17, 0)?;
                emit_load_allocated_tagged(&mut ops, frame, locations[0], 9, 0)?;
                dynasm!(ops
                    ; .arch aarch64
                    ; movz x11, NUMBER_TAG_HI16, lsl #48
                    ; orr x11, x11, #0x2
                    ; tst x9, x11
                    ; b.ne =>deopt
                    ; mov w11, w9
                );
                emit_load_symbolic_u64(
                    &mut ops,
                    &mut relocations,
                    12,
                    view.cage_base as u64,
                    RelocationTarget::GcCageBase,
                );
                dynasm!(ops
                    ; .arch aarch64
                    ; add x13, x12, x11
                    ; ldrb w16, [x13]
                    ; cmp w16, OBJECT_BODY_TYPE_TAG
                    ; b.ne =>deopt
                    ; ldr w16, [x13, view.object_shape_byte]
                );
                emit_load_u64(&mut ops, 15, u64::from(transition.from_shape));
                dynasm!(ops
                    ; .arch aarch64
                    ; cmp w16, w15
                    ; b.ne =>deopt
                    // An intervening call in a non-simple constructor may
                    // freeze `this` after the transition plan was baked.
                    // Adding an own field to a non-extensible receiver would
                    // bypass the canonical StoreProperty semantics, so prove
                    // the live flag before any shape/length mutation.
                    ; ldrb w16, [x13, view.object_extensible_byte]
                    ; cbz w16, =>deopt
                    ; ldrh w16, [x13, view.object_slab_len_byte]
                    ; cmp w16, transition.slot as u32
                    ; b.ne =>deopt
                );
                for &prototype_shape in &transition.prototype_shapes {
                    dynasm!(ops
                        ; .arch aarch64
                        ; ldr w11, [x13, view.jit_proto_byte]
                        ; cbz w11, =>deopt
                        ; add x13, x12, x11
                        ; ldrb w16, [x13]
                        ; cmp w16, OBJECT_BODY_TYPE_TAG
                        ; b.ne =>deopt
                        ; ldr w16, [x13, view.object_shape_byte]
                    );
                    emit_load_u64(&mut ops, 15, u64::from(prototype_shape));
                    dynasm!(ops ; .arch aarch64 ; cmp w16, w15 ; b.ne =>deopt);
                }
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr w11, [x13, view.jit_proto_byte]
                    ; cbnz w11, =>deopt
                );

                // Recompute the receiver after walking the prototype chain;
                // every observable guard precedes the first mutation.
                emit_load_allocated_tagged(&mut ops, frame, locations[0], 9, 0)?;
                dynasm!(ops ; .arch aarch64 ; mov w11, w9 ; add x13, x12, x11);
                emit_load_u64(&mut ops, 14, u64::from(transition.to_shape));
                dynasm!(ops
                    ; .arch aarch64
                    ; str w14, [x13, view.object_shape_byte]
                    ; mov w15, transition.slot as u32 + 1
                    ; strh w15, [x13, view.object_slab_len_byte]
                    ; mov x12, x13
                );
                if transition.slot == 0 {
                    let values_ready = ops.new_dynamic_label();
                    dynasm!(ops
                        ; .arch aarch64
                        // Receiver preparation can reserve an out-of-line
                        // slab for a multi-field transition program before
                        // the first StoreProperty executes. Preserve that
                        // stable slab pointer; only a genuinely inline object
                        // needs its cached values pointer initialized here.
                        ; ldr w16, [x13, view.object_slab_handle_byte]
                        ; cbnz w16, =>values_ready
                        ; add x16, x13, view.object_inline_values_byte
                        ; str x16, [x13, view.object_values_ptr_byte]
                        ; =>values_ready
                    );
                }
                dynasm!(ops ; .arch aarch64 ; mov x10, x17);
                dynasm!(ops ; .arch aarch64 ; mov x13, x12);
                emit_slab_base(&mut ops, view, 13, 14);
                let slot_byte = u32::from(transition.slot) * 8;
                dynasm!(ops
                    ; .arch aarch64
                    ; str x10, [x13, slot_byte]
                    ; sub sp, sp, #16
                    ; stp x12, x10, [sp]
                );
                // The barrier implementation owns x14-x16 as scratch, so the
                // raw shape handle must live in a disjoint argument register
                // if marking makes this call take its slow sibling.
                emit_load_u64(&mut ops, 3, u64::from(transition.to_shape));
                emit_write_barrier_with_context(&mut ops, &mut relocations, view, 12, 3, 19);
                dynasm!(ops ; .arch aarch64 ; ldp x12, x10, [sp] ; add sp, sp, #16);
                let value_barrier_done = ops.new_dynamic_label();
                emit_cell_test(&mut ops, 10, 14, CellTest::IsNotCell, value_barrier_done);
                emit_write_barrier_with_context(&mut ops, &mut relocations, view, 12, 10, 19);
                dynasm!(ops ; .arch aarch64 ; =>value_barrier_done);
                constructor_field_regions.push((byte_pc, start, ops.offset().0));
            }
            MachineOpcode::Return => {
                let source = integer_register(locations[0])?;
                dynasm!(ops
                    ; .arch aarch64
                    ; mov x0, X(source)
                    ; movz x1, STATUS_RETURNED as u32
                );
                emit_epilogue(&mut ops, frame, saved);
            }
            MachineOpcode::Jump => {
                let block = &sequence.blocks()[block_index];
                let successor = block
                    .successors
                    .iter()
                    .copied()
                    .find(|&successor| !is_exceptional_successor(sequence, block_index, successor))
                    .ok_or(Unsupported::OperandShape("numeric jump successors"))?;
                let target = block_labels[successor.0 as usize];
                dynasm!(ops ; .arch aarch64 ; b =>target);
            }
            MachineOpcode::BranchIf(when_true) => {
                let condition = integer_register(locations[0])?;
                let block = &sequence.blocks()[block_index];
                let [taken, fallthrough] = block.successors.as_slice() else {
                    return Err(Unsupported::OperandShape("numeric branch successors"));
                };
                let taken = block_labels[taken.0 as usize];
                let fallthrough = block_labels[fallthrough.0 as usize];
                if when_true {
                    dynasm!(ops ; .arch aarch64 ; cbnz W(condition), =>taken);
                } else {
                    dynasm!(ops ; .arch aarch64 ; cbz W(condition), =>taken);
                }
                dynasm!(ops ; .arch aarch64 ; b =>fallthrough);
            }
            MachineOpcode::Call(descriptor_index) => {
                let descriptor = sequence
                    .call_descriptors()
                    .get(descriptor_index as usize)
                    .ok_or(Unsupported::OperandShape("scalar call descriptor"))?;
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
                        .ok_or(Unsupported::OperandShape("scalar direct call safepoint"))?;
                    let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                    let direct_done = ops.new_dynamic_label();
                    let direct_threw = ops.new_dynamic_label();
                    let direct_bail = ops.new_dynamic_label();
                    let final_method_guard_miss = ops.new_dynamic_label();
                    emit_save_safepoint_roots(&mut ops, frame, site)?;
                    emit_publish_machine_roots(&mut ops, frame, site)?;
                    let result_index = descriptor.arguments.len();
                    let arguments = (1..result_index)
                        .map(|index| {
                            u16::try_from(index).map_err(|_| {
                                Unsupported::OperandShape("scalar direct call argument count")
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let arguments = match argument_mode {
                        DirectCallArgumentMode::Fixed => DirectCallArguments::Fixed(&arguments),
                        DirectCallArgumentMode::Spread => DirectCallArguments::Spread(1),
                    };
                    for (candidate_index, candidate) in candidates.iter().enumerate() {
                        let candidate_start = ops.offset().0;
                        let next_method_candidate = (*kind == DirectCallKind::Method
                            && candidate_index + 1 != candidates.len())
                        .then(|| ops.new_dynamic_label());
                        let method_guard_miss =
                            next_method_candidate.unwrap_or(final_method_guard_miss);
                        let form = match kind {
                            DirectCallKind::Plain => DirectCallForm::Plain { callable: 0 },
                            DirectCallKind::Method => {
                                let guard = candidate.guard.as_ref().ok_or(
                                    Unsupported::OperandShape("scalar method candidate guard"),
                                )?;
                                let receiver = *locations
                                    .first()
                                    .ok_or(Unsupported::OperandShape("scalar method receiver"))?;
                                let guard_start = ops.offset().0;
                                emit_load_allocated_tagged(
                                    &mut ops,
                                    frame,
                                    receiver,
                                    9,
                                    MACHINE_ROOT_RECORD_SIZE,
                                )?;
                                emit_method_guard_from_tagged_register(
                                    &mut ops,
                                    &mut relocations,
                                    view,
                                    guard,
                                    9,
                                    17,
                                    None,
                                    method_guard_miss,
                                )?;
                                structural_regions.push((
                                    "machineDirectMethodGuard",
                                    Some(*byte_pc),
                                    guard_start,
                                    ops.offset().0,
                                ));
                                DirectCallForm::Method {
                                    callable: 17,
                                    receiver: 0,
                                }
                            }
                            DirectCallKind::Construct => DirectCallForm::Construct {
                                callable: 0,
                                receiver: u16::try_from(result_index + 1).map_err(|_| {
                                    Unsupported::OperandShape("scalar construct receiver root")
                                })?,
                            },
                            DirectCallKind::DerivedConstruct => {
                                DirectCallForm::DerivedConstruct { callable: 0 }
                            }
                            DirectCallKind::SuperConstruct => DirectCallForm::SuperConstruct {
                                callable: 0,
                                receiver: u16::try_from(result_index + 1).map_err(|_| {
                                    Unsupported::OperandShape("scalar super receiver root")
                                })?,
                            },
                            DirectCallKind::DerivedSuperConstruct => {
                                DirectCallForm::DerivedSuperConstruct { callable: 0 }
                            }
                        };
                        emit_direct_call_with_access(
                            &mut ops,
                            &mut relocations,
                            view,
                            DirectCallSite {
                                target: &candidate.callee,
                                target_index: candidate.target_index,
                                target_count: candidate.target_count,
                                caller_function_id: *caller_function_id,
                                logical_pc: *logical_pc,
                                byte_pc: *byte_pc,
                                dst: u16::try_from(result_index).map_err(|_| {
                                    Unsupported::OperandShape("scalar direct call result")
                                })?,
                                form,
                                arguments,
                            },
                            deopt_stack_call_entry,
                            resolve_direct_entry,
                            try_prepare_construct_entry,
                            prepare_construct_entry,
                            derived_construct_result_entry,
                            copy_spread_arguments_entry,
                            initialize_upvalues_entry,
                            None,
                            direct_bail,
                            direct_threw,
                            direct_done,
                            19,
                            |ops, source, target, sp_bias| {
                                let operand = instruction.operands.get(usize::from(source)).ok_or(
                                    Unsupported::OperandShape("scalar direct call source"),
                                )?;
                                // A moving safepoint rewrites the canonical save
                                // slot named by the late TaggedRoot metadata. Read
                                // that slot directly after receiver preparation;
                                // its allocator early/late homes may differ and
                                // either register may have been clobbered meanwhile.
                                if let Some(root) =
                                    site.roots.iter().find(|root| root.value == operand.value)
                                {
                                    let offset = root_offset(frame, root.save_slot)?
                                        .checked_add(sp_bias)
                                        .and_then(|offset| {
                                            offset.checked_add(MACHINE_ROOT_RECORD_SIZE)
                                        })
                                        .ok_or(Unsupported::OperandShape(
                                            "scalar direct call canonical root offset",
                                        ))?;
                                    emit_sp_ldr_x(ops, target, offset);
                                    return Ok(());
                                }
                                let location = locations[usize::from(source)];
                                emit_load_allocated_tagged(
                                    ops,
                                    frame,
                                    location,
                                    target,
                                    sp_bias.checked_add(MACHINE_ROOT_RECORD_SIZE).ok_or(
                                        Unsupported::OperandShape("scalar direct call stack bias"),
                                    )?,
                                )
                            },
                            |ops, destination, source, sp_bias| {
                                let location = *locations.get(usize::from(destination)).ok_or(
                                    Unsupported::OperandShape("scalar direct call destination"),
                                )?;
                                emit_store_allocated_tagged(ops, frame, location, source, sp_bias)
                            },
                            |ops| {
                                // x17 is outside regalloc2's allocatable bank and
                                // survives the activation-root descriptor cleanup.
                                dynasm!(ops ; .arch aarch64 ; mov x17, x0);
                                emit_clear_machine_roots(ops);
                                emit_reload_safepoint_roots(ops, frame, site)?;
                                dynasm!(ops ; .arch aarch64 ; mov x0, x17);
                                Ok(())
                            },
                            |ops, source, sp_bias| {
                                let receiver = instruction
                                    .operands
                                    .get(result_index + 1)
                                    .ok_or(Unsupported::OperandShape(
                                        "scalar construct receiver operand",
                                    ))?
                                    .value;
                                let root = site
                                    .roots
                                    .iter()
                                    .find(|root| root.value == receiver)
                                    .ok_or(Unsupported::OperandShape(
                                        "scalar construct receiver save home",
                                    ))?;
                                let offset = root_offset(frame, root.save_slot)?
                                    .checked_add(sp_bias)
                                    .ok_or(Unsupported::OperandShape(
                                        "scalar construct linkage root offset",
                                    ))?
                                    .checked_add(MACHINE_ROOT_RECORD_SIZE)
                                    .ok_or(Unsupported::OperandShape(
                                        "scalar construct receiver root offset",
                                    ))?;
                                dynasm!(ops ; .arch aarch64 ; str X(source), [sp, offset]);
                                Ok(())
                            },
                            |ops, sp_bias| {
                                emit_reload_safepoint_roots_with_bias(
                                    ops,
                                    frame,
                                    site,
                                    sp_bias.checked_add(MACHINE_ROOT_RECORD_SIZE).ok_or(
                                        Unsupported::OperandShape(
                                            "scalar construct root reload bias",
                                        ),
                                    )?,
                                )
                            },
                        )?;
                        if *kind == DirectCallKind::Method {
                            structural_regions.push((
                                "machineDirectMethodCandidate",
                                Some(*byte_pc),
                                candidate_start,
                                ops.offset().0,
                            ));
                        }
                        if let Some(next_method_candidate) = next_method_candidate {
                            dynasm!(ops ; .arch aarch64 ; =>next_method_candidate);
                        }
                    }
                    if *kind == DirectCallKind::Method {
                        dynasm!(ops ; .arch aarch64 ; =>final_method_guard_miss);
                        emit_clear_machine_roots(&mut ops);
                        emit_reload_safepoint_roots(&mut ops, frame, site)?;
                        dynasm!(ops ; .arch aarch64 ; b =>deopt);
                    }
                    dynasm!(ops
                        ; .arch aarch64
                        ; =>direct_bail
                        ; b =>deopt
                        ; =>direct_threw
                    );
                    match descriptor.exceptional {
                        super::super::ExceptionalEdge::LandingPad(target) => {
                            emit_store_allocated_tagged(
                                &mut ops,
                                frame,
                                locations[result_index],
                                0,
                                0,
                            )?;
                            let target = block_labels[target.0 as usize];
                            dynasm!(ops ; .arch aarch64 ; b =>target);
                        }
                        super::super::ExceptionalEdge::Propagate => {
                            dynasm!(ops ; .arch aarch64 ; b =>threw);
                        }
                        super::super::ExceptionalEdge::None => {
                            return Err(Unsupported::OperandShape(
                                "scalar direct call exceptional edge",
                            ));
                        }
                    }
                    dynasm!(ops ; .arch aarch64 ; =>direct_done);
                } else {
                    if let CallTarget::ColdCallExit { byte_pc, .. } = &descriptor.target {
                        let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                        let start = ops.offset().0;
                        dynasm!(ops ; .arch aarch64 ; b =>deopt);
                        structural_regions.push((
                            "machineColdCallExit",
                            Some(*byte_pc),
                            start,
                            ops.offset().0,
                        ));
                        continue;
                    }
                    if matches!(
                        descriptor.target,
                        CallTarget::RuntimeStub(target)
                            if target == STUB_JIT_CLASS_SUPER_CONSTRUCTOR
                    ) {
                        if locations.len() < 2 {
                            return Err(Unsupported::OperandShape("scalar class-super load call"));
                        }
                        let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                        let start = ops.offset().0;
                        emit_load_allocated_tagged(&mut ops, frame, locations[0], 1, 0)?;
                        dynasm!(ops
                            ; .arch aarch64
                            ; movz x11, NUMBER_TAG_HI16, lsl #48
                            ; orr x11, x11, #0x2
                            ; tst x1, x11
                            ; b.ne =>deopt
                            ; mov w11, w1
                        );
                        emit_load_symbolic_u64(
                            &mut ops,
                            &mut relocations,
                            12,
                            view.cage_base as u64,
                            RelocationTarget::GcCageBase,
                        );
                        dynasm!(ops
                            ; .arch aarch64
                            ; add x13, x12, x11
                            ; ldrb w14, [x13]
                            ; cmp w14, view.class_constructor_layout.type_tag as u32
                            ; b.ne =>deopt
                            ; ldr x0, [x13, view.class_constructor_layout.super_constructor_byte]
                        );
                        emit_load_u64(&mut ops, 16, Value::hole().to_bits());
                        dynasm!(ops ; .arch aarch64 ; cmp x0, x16 ; b.eq =>deopt);
                        emit_store_allocated_tagged(&mut ops, frame, locations[1], 0, 0)?;
                        structural_regions.push((
                            "machineClassSuperLoad",
                            None,
                            start,
                            ops.offset().0,
                        ));
                        if !is_terminator {
                            emit_edits(
                                &mut ops,
                                allocation.edits(),
                                AllocationPoint::After(id),
                                frame,
                            )?;
                        }
                        continue;
                    }
                    if matches!(
                        descriptor.target,
                        CallTarget::RuntimeStub(target) if target == STUB_JIT_BIND_DERIVED_THIS
                    ) {
                        if locations.len() < 2 {
                            return Err(Unsupported::OperandShape("scalar derived-this bind call"));
                        }
                        let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                        let fast_start = ops.offset().0;
                        let bind_cold = ops.new_dynamic_label();
                        let bind_returned = ops.new_dynamic_label();
                        let bind_done = ops.new_dynamic_label();
                        emit_load_allocated_tagged(&mut ops, frame, locations[0], 1, 0)?;
                        let required_flags = u32::from(
                            NativeFrameFlags::STACK_REGISTERS
                                | NativeFrameFlags::DERIVED_CONSTRUCTOR,
                        );
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x16, [x19, NATIVE_FRAME_OFFSET]
                            ; ldrb w14, [x16, NATIVE_FRAME_FLAGS_OFFSET]
                            ; and w14, w14, required_flags
                            ; cmp w14, required_flags
                            ; b.ne =>bind_cold
                            ; ldr x14, [x16, NATIVE_FRAME_THIS_OFFSET]
                        );
                        emit_load_u64(&mut ops, 15, Value::hole().to_bits());
                        dynasm!(ops
                            ; .arch aarch64
                            ; cmp x14, x15
                            ; b.ne =>bind_cold
                            ; str x1, [x16, NATIVE_FRAME_THIS_OFFSET]
                            ; mov x0, x1
                        );
                        emit_store_allocated_tagged(&mut ops, frame, locations[1], 0, 0)?;
                        dynasm!(ops ; .arch aarch64 ; b =>bind_done);
                        structural_regions.push((
                            "machineDerivedThisBindFast",
                            None,
                            fast_start,
                            ops.offset().0,
                        ));

                        dynasm!(ops ; .arch aarch64 ; =>bind_cold ; mov x0, x19);
                        let cold_start = ops.offset().0;
                        emit_load_symbolic_u64(
                            &mut ops,
                            &mut relocations,
                            16,
                            bind_derived_this_entry,
                            RelocationTarget::runtime_stub(STUB_JIT_BIND_DERIVED_THIS),
                        );
                        dynasm!(ops
                            ; .arch aarch64
                            ; blr x16
                            ; and x5, x1, #0xff
                        );
                        dynasm!(ops
                            ; .arch aarch64
                            ; cbz x5, =>bind_returned
                            ; cmp x5, STATUS_THREW as u32
                            ; b.ne =>deopt
                        );
                        match descriptor.exceptional {
                            super::super::ExceptionalEdge::LandingPad(target) => {
                                emit_store_allocated_tagged(&mut ops, frame, locations[1], 0, 0)?;
                                let target = block_labels[target.0 as usize];
                                dynasm!(ops ; .arch aarch64 ; b =>target);
                            }
                            super::super::ExceptionalEdge::Propagate => {
                                dynasm!(ops ; .arch aarch64 ; b =>threw);
                            }
                            super::super::ExceptionalEdge::None => {
                                return Err(Unsupported::OperandShape(
                                    "scalar derived-this bind exceptional edge",
                                ));
                            }
                        }
                        dynasm!(ops ; .arch aarch64 ; =>bind_returned);
                        emit_store_allocated_tagged(&mut ops, frame, locations[1], 0, 0)?;
                        structural_regions.push((
                            "machineDerivedThisBindCold",
                            None,
                            cold_start,
                            ops.offset().0,
                        ));
                        dynasm!(ops ; .arch aarch64 ; =>bind_done);
                        if !is_terminator {
                            emit_edits(
                                &mut ops,
                                allocation.edits(),
                                AllocationPoint::After(id),
                                frame,
                            )?;
                        }
                        continue;
                    }
                    let (target, entry, result_index, allocating) = match &descriptor.target {
                        CallTarget::RuntimeStub(target) if *target == STUB_TO_BOOLEAN_LEAF => {
                            if locations.len() < 3
                                || integer_register(locations[0])? != 1
                                || integer_register(locations[1])? != 2
                            {
                                return Err(Unsupported::OperandShape("scalar ToBoolean call"));
                            }
                            (*target, to_boolean_entry, 2, false)
                        }
                        CallTarget::RuntimeStub(target) if *target == STUB_STRICT_EQ_LEAF => {
                            if locations.len() < 3
                                || integer_register(locations[0])? != 1
                                || integer_register(locations[1])? != 2
                            {
                                return Err(Unsupported::OperandShape(
                                    "scalar strict equality call",
                                ));
                            }
                            (*target, strict_eq_entry, 2, false)
                        }
                        CallTarget::RuntimeStub(target) if *target == STUB_STRING_CONCAT_ALLOC => {
                            if locations.len() < 4
                                || integer_register(locations[0])? != 2
                                || integer_register(locations[1])? != 3
                                || integer_register(locations[2])? != 4
                            {
                                return Err(Unsupported::OperandShape("scalar string concat call"));
                            }
                            (*target, string_concat_entry, 3, true)
                        }
                        CallTarget::RuntimeStub(target)
                            if *target == STUB_ARRAY_CONSTRUCT_ALLOC =>
                        {
                            if locations.len() < 4
                                || integer_register(locations[0])? != 2
                                || integer_register(locations[1])? != 3
                                || integer_register(locations[2])? != 4
                            {
                                return Err(Unsupported::OperandShape(
                                    "scalar ArrayConstruct call",
                                ));
                            }
                            (*target, array_construct_entry, 3, true)
                        }
                        CallTarget::RuntimeStub(_) => {
                            return Err(Unsupported::OperandShape("scalar runtime call target"));
                        }
                        CallTarget::Direct { .. } => unreachable!("handled direct call above"),
                        CallTarget::ColdCallExit { .. } => {
                            unreachable!("handled cold call exit above")
                        }
                    };
                    if integer_register(locations[result_index])? != 0 {
                        return Err(Unsupported::OperandShape("scalar runtime call result"));
                    }
                    let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                    let array_construct_region = if target == STUB_ARRAY_CONSTRUCT_ALLOC {
                        let byte_pc = instruction
                            .deopt
                            .and_then(|deopt| {
                                deopt_runtime
                                    .table
                                    .lookup(otter_vm::deopt::DeoptExitId(deopt.0))
                            })
                            .map(|state| state.innermost().byte_pc)
                            .ok_or(Unsupported::OperandShape(
                                "scalar ArrayConstruct deopt state",
                            ))?;
                        Some((byte_pc, ops.offset().0))
                    } else {
                        None
                    };
                    if allocating {
                        let site = safepoints
                            .site(id)
                            .filter(|site| instruction.safepoint == Some(site.id))
                            .ok_or(Unsupported::OperandShape(
                                "scalar allocating call safepoint",
                            ))?;
                        emit_allocating_call(
                            &mut ops,
                            &mut relocations,
                            frame,
                            site,
                            entry,
                            target,
                            deopt,
                        )?;
                    } else {
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x0, [x19, THREAD_OFFSET]
                            ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
                        );
                        emit_load_symbolic_u64(
                            &mut ops,
                            &mut relocations,
                            16,
                            entry,
                            RelocationTarget::runtime_stub(target),
                        );
                        dynasm!(ops
                            ; .arch aarch64
                            ; blr x16
                            ; and x1, x1, #0xff
                            ; cbnz x1, =>deopt
                        );
                        emit_load_u64(&mut ops, 16, Value::boolean(true).to_bits());
                        dynasm!(ops
                            ; .arch aarch64
                            ; cmp x0, x16
                            ; cset w0, eq
                        );
                    }
                    if let Some((byte_pc, start)) = array_construct_region {
                        structural_regions.push((
                            "machineArrayConstruct",
                            Some(byte_pc),
                            start,
                            ops.offset().0,
                        ));
                    }
                }
            }
        }
        if !is_terminator {
            emit_edits(
                &mut ops,
                allocation.edits(),
                AllocationPoint::After(id),
                frame,
            )?;
        }
    }

    dynasm!(ops ; .arch aarch64 ; =>bail);
    emit_materialize_vm_window(&mut ops, vm_register_count);
    dynasm!(ops
        ; .arch aarch64
        ; ldr x17, [x19, NATIVE_FRAME_OFFSET]
        ; str wzr, [x17, NATIVE_FRAME_PC_OFFSET]
        ; mov x0, xzr
        ; movz x1, STATUS_BAILED as u32
    );
    emit_epilogue(&mut ops, frame, saved);

    dynasm!(ops
        ; .arch aarch64
        ; =>threw
        ; mov x0, xzr
        ; movz x1, STATUS_THREW as u32
    );
    emit_epilogue(&mut ops, frame, saved);

    if !deopt_labels.is_empty() {
        for (index, &label) in deopt_labels.iter().enumerate() {
            let index = u32::try_from(index)
                .ok()
                .filter(|&index| index <= u32::from(u16::MAX))
                .ok_or(Unsupported::OperandShape("scalar deopt exit count"))?;
            dynasm!(ops
                ; .arch aarch64
                ; =>label
                ; movz w17, index
                ; b =>shared_deopt
            );
        }

        // Dump layout consumed by `jit_deopt_writeback_stub`: physical x0..x29
        // followed by d0..d15 at ascending addresses. x15..x18 and x29 are
        // namespace holes; x19 retains the context across the call.
        dynasm!(ops
            ; .arch aarch64
            ; =>shared_deopt
            ; stp d14, d15, [sp, #-16]!
            ; stp d12, d13, [sp, #-16]!
            ; stp d10, d11, [sp, #-16]!
            ; stp d8, d9, [sp, #-16]!
            ; stp d6, d7, [sp, #-16]!
            ; stp d4, d5, [sp, #-16]!
            ; stp d2, d3, [sp, #-16]!
            ; stp d0, d1, [sp, #-16]!
            ; stp x28, xzr, [sp, #-16]!
            ; stp x26, x27, [sp, #-16]!
            ; stp x24, x25, [sp, #-16]!
            ; stp x22, x23, [sp, #-16]!
            ; stp x20, x21, [sp, #-16]!
            ; stp x18, x19, [sp, #-16]!
            ; stp x16, x17, [sp, #-16]!
            ; stp x14, x15, [sp, #-16]!
            ; stp x12, x13, [sp, #-16]!
            ; stp x10, x11, [sp, #-16]!
            ; stp x8, x9, [sp, #-16]!
            ; stp x6, x7, [sp, #-16]!
            ; stp x4, x5, [sp, #-16]!
            ; stp x2, x3, [sp, #-16]!
            ; stp x0, x1, [sp, #-16]!
            ; mov x0, x19
            ; mov w1, w17
        );
        emit_load_symbolic_u64(
            &mut ops,
            &mut relocations,
            2,
            std::ptr::from_ref::<DeoptRuntime>(deopt_runtime) as u64,
            RelocationTarget::DeoptRuntimeData,
        );
        dynasm!(ops
            ; .arch aarch64
            ; mov x3, sp
            ; add x4, sp, DEOPT_DUMP_BYTES
            ; ldr x5, [x19, NATIVE_FRAME_OFFSET]
            ; ldr x5, [x5, NATIVE_FRAME_REGISTER_BASE_OFFSET]
        );
        emit_load_symbolic_u64(
            &mut ops,
            &mut relocations,
            16,
            deopt_writeback_entry,
            RelocationTarget::runtime_stub(STUB_JIT_DEOPT_WRITEBACK),
        );
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; add sp, sp, DEOPT_DUMP_BYTES
        );
        emit_epilogue(&mut ops, frame, saved);
    }

    let mut osr_entries = BTreeMap::new();
    let mut osr_regions = Vec::with_capacity(osr_sites.len());
    for site in &osr_sites {
        let offset = ops.offset().0;
        let representation_bail = ops.new_dynamic_label();
        emit_prologue(&mut ops, frame, saved);
        if has_backedge_poll {
            dynasm!(ops ; .arch aarch64 ; movz w29, GENERATED_POLL_BATCH);
        }
        let locations = allocation
            .instruction_locations(site.instruction)
            .ok_or(Unsupported::OperandShape("scalar OSR allocation coverage"))?;
        for ((input, &location), operand) in site
            .inputs
            .iter()
            .zip(locations)
            .zip(&sequence.instructions()[site.instruction.0 as usize].operands)
        {
            emit_osr_materialization(
                &mut ops,
                frame,
                *input,
                sequence.representations()[operand.value.0 as usize],
                location,
                representation_bail,
            )?;
        }
        let continuation = site.continuation;
        dynasm!(ops
            ; .arch aarch64
            ; b =>continuation
            ; =>representation_bail
        );
        emit_load_u64(&mut ops, 16, u64::from(site.logical_pc));
        dynasm!(ops
            ; .arch aarch64
            ; ldr x17, [x19, NATIVE_FRAME_OFFSET]
            ; str w16, [x17, NATIVE_FRAME_PC_OFFSET]
            ; mov x0, xzr
            ; movz x1, STATUS_BAILED as u32
        );
        emit_epilogue(&mut ops, frame, saved);
        let end = ops.offset().0;
        if osr_entries.insert(site.logical_pc, offset).is_some() {
            return Err(Unsupported::OperandShape("duplicate scalar OSR logical PC"));
        }
        osr_regions.push((site.logical_pc, offset, end));
    }

    let buffer = ops
        .finalize()
        .map_err(|_| Unsupported::Backend(crate::BackendFailure::Finalization))?;
    let cold_dump_bytes = if deopt_labels.is_empty() {
        0
    } else {
        DEOPT_DUMP_BYTES
    };
    let generated_stack_frame_bytes =
        frame
            .frame_bytes()
            .checked_add(cold_dump_bytes)
            .ok_or(Unsupported::OperandShape(
                "numeric generated stack frame bytes",
            ))?;
    Ok(Emission {
        code: CompiledCode::new(buffer, AssemblyOffset(0)),
        generated_stack_frame_bytes,
        relocations,
        osr_entries,
        osr_regions,
        constructor_field_regions,
        structural_regions,
    })
}

fn is_exceptional_successor(
    sequence: &InstructionSequence,
    block_index: usize,
    successor: super::super::MachineBlock,
) -> bool {
    let block = &sequence.blocks()[block_index];
    (block.first.0..block.end.0).any(|instruction_index| {
        let instruction = &sequence.instructions()[instruction_index as usize];
        let MachineOpcode::Call(descriptor_index) = instruction.opcode else {
            return false;
        };
        sequence
            .call_descriptors()
            .get(descriptor_index as usize)
            .is_some_and(|descriptor| {
                descriptor.exceptional == super::super::ExceptionalEdge::LandingPad(successor)
            })
    })
}

fn emit_allocating_call(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
    entry: u64,
    target: RuntimeStubDescriptor,
    deopt: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_save_safepoint_roots(ops, frame, site)?;
    dynasm!(ops
        ; .arch aarch64
        ; sub sp, sp, ALLOC_CTX_STACK_SIZE
        ; ldr x9, [x19, THREAD_OFFSET]
        ; str x9, [sp, ALLOC_CTX_THREAD_OFFSET]
        ; movz w9, site.id.0
        ; str w9, [sp, ALLOC_CTX_SAFEPOINT_ID_OFFSET]
    );
    if frame.root_slots() == 0 {
        dynasm!(ops
            ; .arch aarch64
            ; str xzr, [sp, ALLOC_CTX_SPILL_SLOTS_OFFSET]
            ; strh wzr, [sp, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET]
        );
    } else {
        let root_base = ALLOC_CTX_STACK_SIZE
            .checked_add(root_offset(frame, 0)?)
            .ok_or(Unsupported::OperandShape("scalar root-save base"))?;
        emit_sp_address_x9(ops, root_base);
        dynasm!(ops
            ; .arch aarch64
            ; str x9, [sp, ALLOC_CTX_SPILL_SLOTS_OFFSET]
            ; movz w9, frame.root_slots() as u32
            ; strh w9, [sp, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET]
        );
    }
    dynasm!(ops
        ; .arch aarch64
        ; mov x0, sp
        ; movz w1, site.id.0
    );
    emit_load_symbolic_u64(
        ops,
        relocations,
        16,
        entry,
        RelocationTarget::runtime_stub(target),
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; mov x6, x0
        ; and x5, x1, #0xff
        ; add sp, sp, ALLOC_CTX_STACK_SIZE
    );
    emit_reload_safepoint_roots(ops, frame, site)?;
    dynasm!(ops
        ; .arch aarch64
        ; mov x0, x6
        ; cbnz x5, =>deopt
    );
    Ok(())
}

fn emit_save_safepoint_roots(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
) -> Result<(), Unsupported> {
    for root in &site.roots {
        let destination = root_offset(frame, root.save_slot)?;
        match root.source {
            AllocatedLocation::Register(register) if register.is_integer() => {
                dynasm!(ops ; .arch aarch64 ; str X(register.encoding()), [sp, destination]);
            }
            AllocatedLocation::Stack(slot) => {
                let source = spill_offset(frame, slot)?;
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr x16, [sp, source]
                    ; str x16, [sp, destination]
                );
            }
            AllocatedLocation::Register(_) => {
                return Err(Unsupported::OperandShape("scalar tagged root source"));
            }
        }
    }
    Ok(())
}

fn emit_reload_safepoint_roots(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
) -> Result<(), Unsupported> {
    emit_reload_safepoint_roots_with_bias(ops, frame, site, 0)
}

fn emit_reload_safepoint_roots_with_bias(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
    sp_bias: u32,
) -> Result<(), Unsupported> {
    for root in &site.roots {
        let source = root_offset(frame, root.save_slot)?
            .checked_add(sp_bias)
            .ok_or(Unsupported::OperandShape("scalar root reload stack bias"))?;
        match root.source {
            AllocatedLocation::Register(register) if register.is_integer() => {
                dynasm!(ops ; .arch aarch64 ; ldr X(register.encoding()), [sp, source]);
            }
            AllocatedLocation::Stack(slot) => {
                let destination = spill_offset(frame, slot)?
                    .checked_add(sp_bias)
                    .ok_or(Unsupported::OperandShape("scalar spill reload stack bias"))?;
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr x16, [sp, source]
                    ; str x16, [sp, destination]
                );
            }
            AllocatedLocation::Register(_) => {
                return Err(Unsupported::OperandShape("scalar tagged root reload"));
            }
        }
    }
    Ok(())
}

fn emit_publish_machine_roots(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
) -> Result<(), Unsupported> {
    dynasm!(ops ; .arch aarch64 ; sub sp, sp, MACHINE_ROOT_RECORD_SIZE);
    if site.roots.is_empty() {
        dynasm!(ops ; .arch aarch64 ; mov x13, xzr);
    } else {
        let offset = root_offset(frame, 0)?
            .checked_add(MACHINE_ROOT_RECORD_SIZE)
            .ok_or(Unsupported::OperandShape("scalar machine root base"))?;
        if offset <= 4095 {
            dynasm!(ops ; .arch aarch64 ; add x13, sp, offset);
        } else {
            emit_load_u64(ops, 13, u64::from(offset));
            dynasm!(ops ; .arch aarch64 ; add x13, sp, x13);
        }
    }
    dynasm!(ops
        ; .arch aarch64
        ; ldr x9, [x19, MACHINE_ROOTS_PTR_OFFSET]
        ; ldr x10, [x9]
        ; str x10, [sp, MACHINE_ROOT_RECORD_PREVIOUS_OFFSET]
        ; str x13, [sp, MACHINE_ROOT_RECORD_BASE_OFFSET]
        ; movz w11, site.roots.len() as u32
        // One word initializes both root_count and the reserved zero field.
        ; str w11, [sp, MACHINE_ROOT_RECORD_COUNT_OFFSET]
        ; movz w11, site.id.0
        ; str w11, [sp, MACHINE_ROOT_RECORD_SAFEPOINT_ID_OFFSET]
        ; ldr x11, [x19, THREAD_OFFSET]
        ; ldr x11, [x11, VM_THREAD_CODE_OBJECT_ID_OFFSET]
        ; str x11, [sp, MACHINE_ROOT_RECORD_CODE_OBJECT_ID_OFFSET]
        ; mov x12, sp
        ; str x12, [x9]
    );
    Ok(())
}

fn emit_clear_machine_roots(ops: &mut dynasmrt::aarch64::Assembler) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr x9, [x19, MACHINE_ROOTS_PTR_OFFSET]
        ; ldr x10, [sp, MACHINE_ROOT_RECORD_PREVIOUS_OFFSET]
        ; str x10, [x9]
        ; add sp, sp, MACHINE_ROOT_RECORD_SIZE
    );
}

fn emit_load_allocated_tagged(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    location: AllocatedLocation,
    target: u8,
    sp_bias: u32,
) -> Result<(), Unsupported> {
    emit_load_allocated_integer(ops, frame, location, target, sp_bias)
}

fn emit_load_allocated_integer(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    location: AllocatedLocation,
    target: u8,
    sp_bias: u32,
) -> Result<(), Unsupported> {
    match location {
        AllocatedLocation::Register(register) if register.is_integer() => {
            dynasm!(ops ; .arch aarch64 ; mov X(target), X(register.encoding()));
        }
        AllocatedLocation::Stack(slot) => {
            let offset = spill_offset(frame, slot)?
                .checked_add(sp_bias)
                .ok_or(Unsupported::OperandShape("scalar direct call spill offset"))?;
            if offset <= 32_760 {
                dynasm!(ops ; .arch aarch64 ; ldr X(target), [sp, offset]);
            } else {
                emit_load_u64(ops, 16, u64::from(offset));
                dynasm!(ops ; .arch aarch64 ; ldr X(target), [sp, x16]);
            }
        }
        AllocatedLocation::Register(_) => {
            return Err(Unsupported::OperandShape("scalar allocated integer source"));
        }
    }
    Ok(())
}

fn emit_store_allocated_tagged(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    location: AllocatedLocation,
    source: u8,
    sp_bias: u32,
) -> Result<(), Unsupported> {
    match location {
        AllocatedLocation::Register(register) if register.is_integer() => {
            dynasm!(ops ; .arch aarch64 ; mov X(register.encoding()), X(source));
        }
        AllocatedLocation::Stack(slot) => {
            let offset = spill_offset(frame, slot)?
                .checked_add(sp_bias)
                .ok_or(Unsupported::OperandShape("scalar direct call spill offset"))?;
            if offset <= 32_760 {
                dynasm!(ops ; .arch aarch64 ; str X(source), [sp, offset]);
            } else {
                emit_load_u64(ops, 16, u64::from(offset));
                dynasm!(ops ; .arch aarch64 ; str X(source), [sp, x16]);
            }
        }
        AllocatedLocation::Register(_) => {
            return Err(Unsupported::OperandShape(
                "scalar direct call tagged destination",
            ));
        }
    }
    Ok(())
}

/// Expand a parameter-prefix frame before shared VM machinery can observe it.
///
/// This is cold, allocation-free, and publishes the full count only after all
/// newly visible slots contain canonical tagged `undefined` values.
fn emit_materialize_vm_window(ops: &mut dynasmrt::aarch64::Assembler, register_count: u16) {
    let done = ops.new_dynamic_label();
    let loop_label = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; ldr x17, [x19, NATIVE_FRAME_OFFSET]
        ; ldrh w16, [x17, NATIVE_FRAME_REGISTER_COUNT_OFFSET]
        ; movz w15, register_count as u32
        ; cmp w16, w15
        ; b.eq =>done
        ; ldr x14, [x17, NATIVE_FRAME_REGISTER_BASE_OFFSET]
        ; add x14, x14, x16, lsl #3
        ; sub w15, w15, w16
    );
    emit_load_u64(ops, 16, VALUE_UNDEFINED);
    dynasm!(ops
        ; .arch aarch64
        ; =>loop_label
        ; str x16, [x14], #8
        ; subs w15, w15, #1
        ; b.ne =>loop_label
        ; movz w16, register_count as u32
        ; strh w16, [x17, NATIVE_FRAME_REGISTER_COUNT_OFFSET]
        ; =>done
    );
}

fn block_for_instruction(
    sequence: &InstructionSequence,
    instruction: MachineInstructionId,
) -> Result<usize, Unsupported> {
    sequence
        .blocks()
        .iter()
        .position(|block| block.first.0 <= instruction.0 && instruction.0 < block.end.0)
        .ok_or(Unsupported::OperandShape("numeric instruction block"))
}

fn reject_unimplemented_locations(
    sequence: &InstructionSequence,
    allocation: &AllocatedSequence,
) -> Result<(), Unsupported> {
    for instruction_index in 0..sequence.instructions().len() {
        let locations = allocation
            .instruction_locations(MachineInstructionId(instruction_index as u32))
            .ok_or(Unsupported::OperandShape("numeric allocation coverage"))?;
        for &location in locations {
            match location {
                AllocatedLocation::Register(register)
                    if register.is_integer()
                        && !((register.encoding() <= 14)
                            || (20..=28).contains(&register.encoding())) =>
                {
                    return Err(Unsupported::OperandShape("scalar Machine IR GPR"));
                }
                AllocatedLocation::Register(register)
                    if register.is_float() && register.encoding() > 15 =>
                {
                    return Err(Unsupported::OperandShape("scalar Machine IR FP register"));
                }
                AllocatedLocation::Register(_) | AllocatedLocation::Stack(_) => {}
            }
        }
    }
    Ok(())
}

fn emit_edits(
    ops: &mut dynasmrt::aarch64::Assembler,
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
                dynasm!(ops ; .arch aarch64 ; mov X(to.encoding()), X(from.encoding()));
            }
            (AllocatedLocation::Register(from), AllocatedLocation::Register(to))
                if from.is_float() && to.is_float() =>
            {
                dynasm!(ops ; .arch aarch64 ; fmov D(to.encoding()), D(from.encoding()));
            }
            (AllocatedLocation::Register(from), AllocatedLocation::Stack(slot)) => {
                let offset = spill_offset(frame, slot)?;
                if from.is_integer() {
                    dynasm!(ops ; .arch aarch64 ; str X(from.encoding()), [sp, offset]);
                } else if from.is_float() {
                    dynasm!(ops ; .arch aarch64 ; str D(from.encoding()), [sp, offset]);
                } else {
                    return Err(Unsupported::OperandShape("numeric spill register class"));
                }
            }
            (AllocatedLocation::Stack(slot), AllocatedLocation::Register(to)) => {
                let offset = spill_offset(frame, slot)?;
                if to.is_integer() {
                    dynasm!(ops ; .arch aarch64 ; ldr X(to.encoding()), [sp, offset]);
                } else if to.is_float() {
                    dynasm!(ops ; .arch aarch64 ; ldr D(to.encoding()), [sp, offset]);
                } else {
                    return Err(Unsupported::OperandShape("numeric reload register class"));
                }
            }
            (AllocatedLocation::Stack(from), AllocatedLocation::Stack(to)) => {
                let from = spill_offset(frame, from)?;
                let to = spill_offset(frame, to)?;
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr x16, [sp, from]
                    ; str x16, [sp, to]
                );
            }
            (AllocatedLocation::Register(_), AllocatedLocation::Register(_)) => {
                return Err(Unsupported::OperandShape("numeric edit register class"));
            }
        }
    }
    Ok(())
}

fn emit_decode_number(
    ops: &mut dynasmrt::aarch64::Assembler,
    source: u8,
    destination: u8,
    bail: dynasmrt::DynamicLabel,
) {
    let non_int = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x16, NUMBER_TAG_HI16, lsl #48
        ; and x17, X(source), x16
        ; cmp x17, x16
        ; b.ne =>non_int
        ; scvtf D(destination), W(source)
        ; b =>done
        ; =>non_int
        ; tst X(source), x16
        ; b.eq =>bail
        ; movz x16, DOUBLE_OFFSET_HI16, lsl #48
        ; sub x17, X(source), x16
        ; fmov D(destination), x17
        ; =>done
    );
}

fn emit_decode_int32(
    ops: &mut dynasmrt::aarch64::Assembler,
    source: u8,
    bail: dynasmrt::DynamicLabel,
) {
    dynasm!(ops
        ; .arch aarch64
        ; movz x16, NUMBER_TAG_HI16, lsl #48
        ; and x17, X(source), x16
        ; cmp x17, x16
        ; b.ne =>bail
    );
}

fn emit_box_number(ops: &mut dynasmrt::aarch64::Assembler, source: u8, destination: u8) {
    let encode_double = ops.new_dynamic_label();
    let tag_integer = ops.new_dynamic_label();
    let double_ready = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; fcvtzs w16, D(source)
        ; scvtf d31, w16
        ; fcmp D(source), d31
        ; b.ne =>encode_double
        ; fmov x17, D(source)
        ; cbnz w16, =>tag_integer
        ; tbnz x17, #63, =>encode_double
        ; =>tag_integer
        ; mov w17, w16
        ; movz x16, NUMBER_TAG_HI16, lsl #48
        ; orr X(destination), x17, x16
        ; b =>done
        ; =>encode_double
        ; fmov X(destination), D(source)
        ; fcmp D(source), D(source)
        ; b.vc =>double_ready
        ; movz X(destination), CANONICAL_NAN_HI16, lsl #48
        ; =>double_ready
        ; movz x16, DOUBLE_OFFSET_HI16, lsl #48
        ; add X(destination), X(destination), x16
        ; =>done
    );
}

fn emit_box_uint32(ops: &mut dynasmrt::aarch64::Assembler, source: u8, destination: u8) {
    let encode_double = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; tbnz W(source), #31, =>encode_double
        ; mov W(destination), W(source)
        ; movz x16, NUMBER_TAG_HI16, lsl #48
        ; orr X(destination), X(destination), x16
        ; b =>done
        ; =>encode_double
        ; ucvtf d31, W(source)
        ; fmov X(destination), d31
        ; movz x16, DOUBLE_OFFSET_HI16, lsl #48
        ; add X(destination), X(destination), x16
        ; =>done
    );
}

fn emit_box_boolean(ops: &mut dynasmrt::aarch64::Assembler, source: u8, destination: u8) {
    let is_false = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; cbz W(source), =>is_false);
    emit_load_u64(ops, destination, Value::boolean(true).to_bits());
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>is_false);
    emit_load_u64(ops, destination, Value::boolean(false).to_bits());
    dynasm!(ops ; .arch aarch64 ; =>done);
}

fn emit_prologue(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    saved: SavedFrame,
) {
    dynasm!(ops
        ; .arch aarch64
        ; stp x29, x30, [sp, #-16]!
    );
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
    emit_reserve_spill_area(ops, frame.spill_area_bytes());
    dynasm!(ops
        ; .arch aarch64
        ; mov x19, x0
        ; ldr x17, [x19, NATIVE_FRAME_OFFSET]
        ; ldr x17, [x17, NATIVE_FRAME_REGISTER_BASE_OFFSET]
    );
}

fn emit_load_osr_source(ops: &mut dynasmrt::aarch64::Assembler, frame_register: u16) {
    let offset = u32::from(frame_register) * 8;
    dynasm!(ops
        ; .arch aarch64
        ; ldr x17, [x19, NATIVE_FRAME_OFFSET]
        ; ldr x17, [x17, NATIVE_FRAME_REGISTER_BASE_OFFSET]
        ; ldr x16, [x17, offset]
    );
}

fn emit_decode_osr_number(ops: &mut dynasmrt::aarch64::Assembler, bail: DynamicLabel) {
    let non_int = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; fmov d30, x16
        ; movz x16, NUMBER_TAG_HI16, lsl #48
        ; fmov x17, d30
        ; and x17, x17, x16
        ; cmp x17, x16
        ; b.ne =>non_int
        ; fmov x17, d30
        ; scvtf d31, w17
        ; b =>done
        ; =>non_int
        ; fmov x17, d30
        ; tst x17, x16
        ; b.eq =>bail
        ; movz x16, DOUBLE_OFFSET_HI16, lsl #48
        ; sub x17, x17, x16
        ; fmov d31, x17
        ; =>done
    );
}

fn emit_store_osr_integer(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    location: AllocatedLocation,
) -> Result<(), Unsupported> {
    match location {
        AllocatedLocation::Register(register) if register.is_integer() => {
            let destination = register.encoding();
            dynasm!(ops ; .arch aarch64 ; mov W(destination), w16);
        }
        AllocatedLocation::Stack(slot) => {
            let offset = spill_offset(frame, slot)?;
            dynasm!(ops ; .arch aarch64 ; str x16, [sp, offset]);
        }
        AllocatedLocation::Register(_) => {
            return Err(Unsupported::OperandShape("scalar OSR integer location"));
        }
    }
    Ok(())
}

fn emit_store_osr_tagged(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    location: AllocatedLocation,
) -> Result<(), Unsupported> {
    match location {
        AllocatedLocation::Register(register) if register.is_integer() => {
            let destination = register.encoding();
            dynasm!(ops ; .arch aarch64 ; mov X(destination), x16);
        }
        AllocatedLocation::Stack(slot) => {
            let offset = spill_offset(frame, slot)?;
            dynasm!(ops ; .arch aarch64 ; str x16, [sp, offset]);
        }
        AllocatedLocation::Register(_) => {
            return Err(Unsupported::OperandShape("scalar OSR tagged location"));
        }
    }
    Ok(())
}

fn emit_store_osr_float(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    location: AllocatedLocation,
) -> Result<(), Unsupported> {
    match location {
        AllocatedLocation::Register(register) if register.is_float() => {
            let destination = register.encoding();
            dynasm!(ops ; .arch aarch64 ; fmov D(destination), d31);
        }
        AllocatedLocation::Stack(slot) => {
            let offset = spill_offset(frame, slot)?;
            dynasm!(ops ; .arch aarch64 ; str d31, [sp, offset]);
        }
        AllocatedLocation::Register(_) => {
            return Err(Unsupported::OperandShape("scalar OSR float location"));
        }
    }
    Ok(())
}

fn emit_osr_materialization(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    input: MachineOsrInput,
    representation: MachineRepresentation,
    location: AllocatedLocation,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    let expected = match input.value_type {
        MachineOsrType::Tagged => MachineRepresentation::Tagged,
        MachineOsrType::Int32 => MachineRepresentation::Int32,
        MachineOsrType::Uint32 => MachineRepresentation::Uint32,
        MachineOsrType::Float64 => MachineRepresentation::Float64,
        MachineOsrType::Boolean => MachineRepresentation::Boolean,
    };
    if representation != expected {
        return Err(Unsupported::OperandShape("scalar OSR representation"));
    }

    emit_load_osr_source(ops, input.frame_register);
    match input.value_type {
        MachineOsrType::Tagged => emit_store_osr_tagged(ops, frame, location),
        MachineOsrType::Int32 => {
            dynasm!(ops
                ; .arch aarch64
                ; movz x17, NUMBER_TAG_HI16, lsl #48
                ; and x16, x16, x17
                ; cmp x16, x17
                ; b.ne =>bail
            );
            emit_load_osr_source(ops, input.frame_register);
            dynasm!(ops ; .arch aarch64 ; mov w16, w16);
            emit_store_osr_integer(ops, frame, location)
        }
        MachineOsrType::Uint32 => {
            emit_decode_osr_number(ops, bail);
            dynasm!(ops
                ; .arch aarch64
                ; fcvtzu w16, d31
                ; ucvtf d30, w16
                ; fcmp d31, d30
                ; b.ne =>bail
            );
            emit_store_osr_integer(ops, frame, location)
        }
        MachineOsrType::Float64 => {
            emit_decode_osr_number(ops, bail);
            emit_store_osr_float(ops, frame, location)
        }
        MachineOsrType::Boolean => {
            let is_true = ops.new_dynamic_label();
            let ready = ops.new_dynamic_label();
            emit_load_u64(ops, 17, Value::boolean(true).to_bits());
            dynasm!(ops
                ; .arch aarch64
                ; cmp x16, x17
                ; b.eq =>is_true
            );
            emit_load_u64(ops, 17, Value::boolean(false).to_bits());
            dynasm!(ops
                ; .arch aarch64
                ; cmp x16, x17
                ; b.ne =>bail
                ; mov w16, wzr
                ; b =>ready
                ; =>is_true
                ; mov w16, #1
                ; =>ready
            );
            emit_store_osr_integer(ops, frame, location)
        }
    }
}

fn emit_epilogue(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    saved: SavedFrame,
) {
    emit_release_spill_area(ops, frame.spill_area_bytes());
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
    dynasm!(ops
        ; .arch aarch64
        ; ldp x29, x30, [sp], #16
        ; ret
    );
}

fn emit_reserve_spill_area(ops: &mut dynasmrt::aarch64::Assembler, bytes: u32) {
    if bytes == 0 {
        return;
    }
    if bytes <= 4095 {
        dynasm!(ops ; .arch aarch64 ; sub sp, sp, bytes);
    } else {
        emit_load_u64(ops, 16, u64::from(bytes));
        dynasm!(ops ; .arch aarch64 ; sub sp, sp, x16);
    }
}

fn emit_release_spill_area(ops: &mut dynasmrt::aarch64::Assembler, bytes: u32) {
    if bytes == 0 {
        return;
    }
    if bytes <= 4095 {
        dynasm!(ops ; .arch aarch64 ; add sp, sp, bytes);
    } else {
        emit_load_u64(ops, 16, u64::from(bytes));
        dynasm!(ops ; .arch aarch64 ; add sp, sp, x16);
    }
}

fn spill_offset(frame: MachineFrameLayout, slot: u32) -> Result<u32, Unsupported> {
    frame
        .spill_offset(slot)
        .map_err(|_| Unsupported::OperandShape("scalar Machine IR spill offset"))
}

fn root_offset(frame: MachineFrameLayout, slot: u16) -> Result<u32, Unsupported> {
    frame
        .root_offset(slot)
        .map_err(|_| Unsupported::OperandShape("scalar Machine IR root-save offset"))
}

fn emit_sp_address_x9(ops: &mut dynasmrt::aarch64::Assembler, offset: u32) {
    if offset <= 4095 {
        dynasm!(ops ; .arch aarch64 ; add x9, sp, offset);
    } else {
        emit_load_u64(ops, 9, u64::from(offset));
        dynasm!(ops ; .arch aarch64 ; add x9, sp, x9);
    }
}

fn emit_sp_ldr_x(ops: &mut dynasmrt::aarch64::Assembler, register: u8, offset: u32) {
    if offset <= 32_760 && offset.is_multiple_of(8) {
        dynasm!(ops ; .arch aarch64 ; ldr X(register), [sp, offset]);
    } else {
        emit_sp_address_x9(ops, offset);
        dynasm!(ops ; .arch aarch64 ; ldr X(register), [x9]);
    }
}

fn emit_load_u64(ops: &mut dynasmrt::aarch64::Assembler, register: u8, value: u64) {
    dynasm!(ops ; .arch aarch64 ; movz X(register), (value & 0xffff) as u32);
    if (value >> 16) & 0xffff != 0 {
        dynasm!(ops ; .arch aarch64 ; movk X(register), ((value >> 16) & 0xffff) as u32, lsl #16);
    }
    if (value >> 32) & 0xffff != 0 {
        dynasm!(ops ; .arch aarch64 ; movk X(register), ((value >> 32) & 0xffff) as u32, lsl #32);
    }
    if (value >> 48) & 0xffff != 0 {
        dynasm!(ops ; .arch aarch64 ; movk X(register), ((value >> 48) & 0xffff) as u32, lsl #48);
    }
}

fn emit_load_symbolic_u64(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    register: u8,
    value: u64,
    target: RelocationTarget,
) {
    let start = ops.offset().0;
    emit_load_u64(ops, register, value);
    relocations.record_mov_wide(start, ops.offset().0, register, target);
}

fn integer_register(location: AllocatedLocation) -> Result<u8, Unsupported> {
    match location {
        AllocatedLocation::Register(register) if register.is_integer() => Ok(register.encoding()),
        AllocatedLocation::Register(_) | AllocatedLocation::Stack(_) => {
            Err(Unsupported::OperandShape("numeric integer register"))
        }
    }
}

fn float_register(location: AllocatedLocation) -> Result<u8, Unsupported> {
    match location {
        AllocatedLocation::Register(register) if register.is_float() => Ok(register.encoding()),
        AllocatedLocation::Register(_) | AllocatedLocation::Stack(_) => {
            Err(Unsupported::OperandShape("numeric floating register"))
        }
    }
}
