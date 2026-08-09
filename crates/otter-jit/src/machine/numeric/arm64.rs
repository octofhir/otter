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
//! - A failed Number guard writes logical PC zero and returns `BAILED` before
//!   any externally visible effect.
//! - Checked integer overflow uses the allocator-driven VM [`DeoptRuntime`];
//!   the emitter owns no parallel reconstruction recipe.
//! - Backedge polls run before allocator edge edits and preserve every value
//!   live into the loop header across the leaf runtime call.
//! - OSR trampolines decode only live loop-header inputs into the exact
//!   late-use locations selected by regalloc2; rejection never mutates VM slots.
//! - Successful results use the VM's canonical tagged representation.
//! - Pure scalar leaves exchange unboxed scalars in fixed ABI operands;
//!   regalloc2 owns every argument/result move and no frame shuttle exists.

// dynasm's dynamic-register expansion calls `.into()` on the required u8
// register encoding. Clippy sees the macro expansion as an identity conversion.
#![allow(clippy::useless_conversion)]

use dynasmrt::{AssemblyOffset, DynamicLabel, DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::{
    Value,
    deopt::DeoptRuntime,
    native_abi::{
        RuntimeStubDescriptor, STUB_JIT_BACKEDGE_POLL, STUB_JIT_DEOPT_WRITEBACK,
        STUB_NUMBER_POW_F64_LEAF, STUB_NUMBER_REM_F64_LEAF, STUB_NUMBER_TO_INT32_F64_LEAF,
        STUB_STRICT_EQ_LEAF, STUB_TO_BOOLEAN_LEAF,
    },
};

use super::super::{
    AllocatedLocation, AllocatedSequence, AllocationEdit, AllocationPoint, CallTarget, DeoptId,
    InstructionSequence, MachineFrameLayout, MachineInstructionId, MachineOpcode, MachineOsrInput,
    MachineOsrType, MachineRepresentation,
};
use crate::{
    CompiledCode, Unsupported,
    artifact::relocation::{RelocationCapture, RelocationTarget},
    entry::{
        CANONICAL_NAN_HI16, DOUBLE_OFFSET_HI16, NATIVE_FRAME_OFFSET, NATIVE_FRAME_PC_OFFSET,
        NATIVE_FRAME_REGISTER_BASE_OFFSET, NATIVE_FRAME_REGISTER_COUNT_OFFSET,
        NATIVE_FRAME_THIS_OFFSET, NUMBER_TAG_HI16, STATUS_BAILED, STATUS_RETURNED, STATUS_THREW,
        THREAD_OFFSET, VALUE_UNDEFINED, VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET,
        VM_THREAD_GC_HEAP_OFFSET, VM_THREAD_INTERRUPT_CELL_OFFSET,
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
    let slow = ops.new_dynamic_label();
    let cont = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; ldr x17, [x19, THREAD_OFFSET]
        ; ldr x16, [x17, VM_THREAD_INTERRUPT_CELL_OFFSET]
        ; ldrb w16, [x16]
        ; cbnz w16, =>bailout
        ; ldr x16, [x17, VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET]
        ; ldr x17, [x16]
        ; subs x17, x17, #1
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
) -> Result<MachineFrameLayout, Unsupported> {
    MachineFrameLayout::new(
        allocation,
        SavedFrame::from_allocation(allocation).fixed_bytes(),
        16,
    )
    .map_err(|_| Unsupported::OperandShape("scalar Machine IR frame layout"))
}

pub(super) fn emit(
    sequence: &InstructionSequence,
    allocation: &AllocatedSequence,
    frame: MachineFrameLayout,
    deopt_runtime: &DeoptRuntime,
    poll_entry: u64,
    deopt_writeback_entry: u64,
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

    emit_prologue(&mut ops, frame, saved);

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
                emit_decode_number(
                    &mut ops,
                    integer_register(locations[0])?,
                    float_register(locations[1])?,
                    bail,
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
                emit_decode_int32(&mut ops, source, bail);
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
                let [successor] = block.successors.as_slice() else {
                    return Err(Unsupported::OperandShape("numeric jump successors"));
                };
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
                let (target, entry, result_index) = match descriptor.target {
                    CallTarget::RuntimeStub(target) if target == STUB_TO_BOOLEAN_LEAF => {
                        if locations.len() < 3
                            || integer_register(locations[0])? != 1
                            || integer_register(locations[1])? != 2
                        {
                            return Err(Unsupported::OperandShape("scalar ToBoolean call"));
                        }
                        (target, to_boolean_entry, 2)
                    }
                    CallTarget::RuntimeStub(target) if target == STUB_STRICT_EQ_LEAF => {
                        if locations.len() < 3
                            || integer_register(locations[0])? != 1
                            || integer_register(locations[1])? != 2
                        {
                            return Err(Unsupported::OperandShape("scalar strict equality call"));
                        }
                        (target, strict_eq_entry, 2)
                    }
                    CallTarget::RuntimeStub(_) => {
                        return Err(Unsupported::OperandShape("scalar runtime call target"));
                    }
                };
                if integer_register(locations[result_index])? != 0 {
                    return Err(Unsupported::OperandShape("scalar runtime call result"));
                }
                let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
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
    })
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
        ; mov x29, sp
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
        MachineOsrType::Int32 | MachineOsrType::Boolean => MachineRepresentation::Int32,
        MachineOsrType::Uint32 => MachineRepresentation::Uint32,
        MachineOsrType::Float64 => MachineRepresentation::Float64,
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
