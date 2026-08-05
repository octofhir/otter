//! AArch64 emission for allocated numeric Machine IR.
//!
//! # Contents
//! - [`emit`] — emits one call-free numeric function from allocator locations.
//! - Exact JavaScript Number decode, canonical boxing, and bailout sequences.
//! - regalloc2 edit emission between selected instructions.
//!
//! # Invariants
//! - `x15` retains the entry context; `x16`/`x17` and `d31` are emitter-only
//!   scratch registers excluded from allocation.
//! - Spill storage and offsets come only from [`MachineFrameLayout`].
//! - Callee-saved allocations remain excluded until shared save/restore maps
//!   land with general-function lowering.
//! - A failed Number guard writes logical PC zero and returns `BAILED` before
//!   any externally visible effect.
//! - Successful results use the VM's canonical int32/double representation.

// dynasm's dynamic-register expansion calls `.into()` on the required u8
// register encoding. Clippy sees the macro expansion as an identity conversion.
#![allow(clippy::useless_conversion)]

use dynasmrt::{AssemblyOffset, DynasmApi, DynasmLabelApi, dynasm};

use super::super::{
    AllocatedLocation, AllocatedSequence, AllocationEdit, AllocationPoint, InstructionSequence,
    MachineFrameLayout, MachineInstructionId, MachineOpcode,
};
use crate::{
    CompiledCode, Unsupported,
    entry::{
        CANONICAL_NAN_HI16, DOUBLE_OFFSET_HI16, NATIVE_FRAME_OFFSET, NATIVE_FRAME_PC_OFFSET,
        NATIVE_FRAME_REGISTER_BASE_OFFSET, NUMBER_TAG_HI16, STATUS_BAILED, STATUS_RETURNED,
    },
};

pub(super) struct Emission {
    pub(super) code: CompiledCode,
    pub(super) stack_frame_bytes: u32,
}

pub(super) fn emit(
    sequence: &InstructionSequence,
    allocation: &AllocatedSequence,
) -> Result<Emission, Unsupported> {
    reject_unimplemented_locations(sequence, allocation)?;
    let frame = MachineFrameLayout::new(allocation, 16, 16)
        .map_err(|_| Unsupported::OperandShape("numeric Machine IR frame layout"))?;
    let mut ops = dynasmrt::aarch64::Assembler::new()
        .map_err(|_| Unsupported::Backend(crate::BackendFailure::AssemblerAllocation))?;
    let bail = ops.new_dynamic_label();
    let block_labels = sequence
        .blocks()
        .iter()
        .map(|_| ops.new_dynamic_label())
        .collect::<Vec<_>>();

    dynasm!(ops
        ; .arch aarch64
        ; stp x29, x30, [sp, #-16]!
        ; mov x29, sp
    );
    emit_reserve_spill_area(&mut ops, frame.spill_area_bytes());
    dynasm!(ops
        ; .arch aarch64
        ; mov x15, x0
        ; ldr x17, [x15, NATIVE_FRAME_OFFSET]
        ; ldr x17, [x17, NATIVE_FRAME_REGISTER_BASE_OFFSET]
    );

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
            .ok_or(Unsupported::OperandShape("numeric Machine IR locations"))?;
        match instruction.opcode {
            MachineOpcode::EntryValue(parameter) => {
                let destination = integer_register(locations[0])?;
                let offset = u32::from(parameter)
                    .checked_mul(8)
                    .ok_or(Unsupported::OperandShape("numeric parameter offset"))?;
                dynasm!(ops ; .arch aarch64 ; ldr X(destination), [x17, offset]);
            }
            MachineOpcode::DecodeNumber => {
                emit_decode_number(
                    &mut ops,
                    integer_register(locations[0])?,
                    float_register(locations[1])?,
                    bail,
                );
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
            MachineOpcode::FloatNeg => {
                let source = float_register(locations[0])?;
                let destination = float_register(locations[1])?;
                dynasm!(ops ; .arch aarch64 ; fneg D(destination), D(source));
            }
            MachineOpcode::FloatLessThan => {
                let left = float_register(locations[0])?;
                let right = float_register(locations[1])?;
                let destination = integer_register(locations[2])?;
                dynasm!(ops
                    ; .arch aarch64
                    ; fcmp D(left), D(right)
                    ; cset W(destination), lt
                );
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
            MachineOpcode::Return => {
                let source = integer_register(locations[0])?;
                dynasm!(ops
                    ; .arch aarch64
                    ; mov x0, X(source)
                    ; movz x1, STATUS_RETURNED as u32
                );
                emit_epilogue(&mut ops, frame);
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
            MachineOpcode::IntegerAdd
            | MachineOpcode::IntegerAddImmediate(_)
            | MachineOpcode::IntegerAndImmediate(_)
            | MachineOpcode::IntegerLessThanImmediate(_)
            | MachineOpcode::IntegerEqualImmediate(_)
            | MachineOpcode::BackedgePoll
            | MachineOpcode::Call(_) => {
                return Err(Unsupported::OperandShape(
                    "numeric AArch64 Machine IR opcode",
                ));
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

    dynasm!(ops
        ; .arch aarch64
        ; =>bail
        ; ldr x17, [x15, NATIVE_FRAME_OFFSET]
        ; str wzr, [x17, NATIVE_FRAME_PC_OFFSET]
        ; mov x0, xzr
        ; movz x1, STATUS_BAILED as u32
    );
    emit_epilogue(&mut ops, frame);
    let buffer = ops
        .finalize()
        .map_err(|_| Unsupported::Backend(crate::BackendFailure::Finalization))?;
    Ok(Emission {
        code: CompiledCode::new(buffer, AssemblyOffset(0)),
        stack_frame_bytes: frame.frame_bytes(),
    })
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
                    if register.is_integer() && register.encoding() >= 19 =>
                {
                    return Err(Unsupported::OperandShape(
                        "numeric Machine IR callee-saved GPR",
                    ));
                }
                AllocatedLocation::Register(register)
                    if register.is_float() && (8..=15).contains(&register.encoding()) =>
                {
                    return Err(Unsupported::OperandShape(
                        "numeric Machine IR callee-saved FP register",
                    ));
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

fn emit_epilogue(ops: &mut dynasmrt::aarch64::Assembler, frame: MachineFrameLayout) {
    emit_release_spill_area(ops, frame.spill_area_bytes());
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
        .map_err(|_| Unsupported::OperandShape("numeric Machine IR spill offset"))
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
