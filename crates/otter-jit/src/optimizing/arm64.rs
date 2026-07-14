//! Arm64 emission for the single-block int32 optimizing subset.
//!
//! # Contents
//! - Whole-pipeline construction and eligibility validation.
//! - Parameter tag guards, unboxed arithmetic, spills, boxing, and deopt exits.
//!
//! # Invariants
//! - Linear-scan registers `0..4` map to `x19..x22`; `x8..x12` are reserved
//!   caller-saved scratch registers. Spill slot `n` is `[sp, #n*8]` after the
//!   aligned spill frame is reserved.
//! - Every used tagged parameter is checked for the `0xfffe` int32 tag before
//!   its low 32 bits enter an allocated location.
//! - `Add`/`Sub` use the arm64 signed-overflow flag and `Mul` compares its
//!   signed 64-bit product with the sign-extended low word. Overflow deopts at
//!   the arithmetic instruction's exact byte PC; it never silently wraps.
//! - The emitted function is a standalone leaf and never re-enters the VM.
//!
//! # See also
//! - [`super`] — public code object and leaf ABI.
//! - [`crate::ir`] — source analyses and allocation contracts.

// dynasm's dynamic-register forms inject an internal conversion that Clippy
// sees as redundant when our register selector is already a `u8`.
#![allow(clippy::useless_conversion)]

use std::collections::{BTreeMap, BTreeSet};

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_bytecode::{Op, Operand};
use otter_vm::JitCompileSnapshot;
use otter_vm::deopt::{DeoptLocation, DeoptRepr, DeoptTable, FrameState};

use super::{OptimizedCode, OptimizedMetadata};
use crate::{
    CompiledCode,
    entry::{STATUS_DEOPT, STATUS_RETURNED, Unsupported},
    ir::{
        cfg::{ControlFlowGraph, Terminator},
        deopt_lower::DeoptLowering,
        dom::DominatorTree,
        frame_state::FrameStateTable,
        liveness::Liveness,
        regalloc::{Allocation, Location},
        repr::{ConversionKind, ReprMap, Representation},
        ssa::{SsaFunction, SsaInstr, ValueDef, ValueId},
    },
};

const ALLOCATABLE_REGISTER_COUNT: u8 = 4;
const VALUE_REGISTERS: [u8; ALLOCATABLE_REGISTER_COUNT as usize] = [19, 20, 21, 22];
const STACK_SLOT_BYTES: u32 = 8;
const MAX_SPILL_FRAME_BYTES: u32 = 4080;
const MAX_PARAMETER_OFFSET: u32 = 32_760;

#[derive(Debug, Clone, Copy)]
struct GuardedParam {
    value: ValueId,
    index: u32,
    first_use_pc: u32,
    deopt_byte_pc: u32,
}

#[derive(Debug)]
struct Eligibility {
    guarded_params: Vec<GuardedParam>,
}

pub(super) fn compile(
    view: &JitCompileSnapshot,
    code_object_id: u64,
) -> Result<OptimizedCode, Unsupported> {
    let cfg = ControlFlowGraph::build(view)
        .map_err(|_| Unsupported::OperandShape("optimizing CFG construction"))?;
    cfg.verify()
        .map_err(|_| Unsupported::OperandShape("optimizing CFG verification"))?;
    let dom = DominatorTree::compute(&cfg);
    dom.verify(&cfg)
        .map_err(|_| Unsupported::OperandShape("optimizing dominance verification"))?;
    let ssa = SsaFunction::build(view, &cfg)
        .map_err(|_| Unsupported::OperandShape("optimizing SSA construction"))?;
    ssa.verify(&cfg, &dom)
        .map_err(|_| Unsupported::OperandShape("optimizing SSA verification"))?;
    let liveness = Liveness::compute(&ssa, &cfg);
    liveness
        .verify(&ssa, &cfg, &dom)
        .map_err(|_| Unsupported::OperandShape("optimizing liveness verification"))?;
    let allocation = Allocation::compute(&ssa, &cfg, &liveness, ALLOCATABLE_REGISTER_COUNT)
        .map_err(|_| Unsupported::OperandShape("optimizing register allocation"))?;
    allocation
        .verify(&ssa, &cfg, &liveness)
        .map_err(|_| Unsupported::OperandShape("optimizing allocation verification"))?;
    let reprs = ReprMap::compute(view, &ssa);
    reprs
        .verify(view, &ssa)
        .map_err(|_| Unsupported::OperandShape("optimizing representation verification"))?;
    let frame_states = FrameStateTable::build(&ssa, &cfg)
        .map_err(|_| Unsupported::OperandShape("optimizing frame-state construction"))?;
    frame_states
        .verify(&ssa, &cfg, &dom)
        .map_err(|_| Unsupported::OperandShape("optimizing frame-state verification"))?;
    let linear_scan_spill_slot_count = allocation.spill_slot_count;
    let allocation = legalize_deopt_locations(&allocation, &frame_states)?;
    let deopt = DeoptLowering::build(view, &ssa, &frame_states, &allocation, &reprs)
        .map_err(|_| Unsupported::OperandShape("optimizing deopt lowering"))?;

    let eligibility = check_eligibility(view, &cfg, &ssa, &reprs, &allocation)?;
    let code = emit(view, &ssa, &allocation, &eligibility, deopt.table())?;
    Ok(OptimizedCode::new(
        code,
        deopt.table().clone(),
        OptimizedMetadata {
            code_object_id,
            function_id: view.code_block.id,
            param_count: view.code_block.param_count,
            register_count: view.code_block.register_count,
            machine_register_count: allocation.register_count,
            linear_scan_spill_slot_count,
            spill_slot_count: allocation.spill_slot_count,
        },
    ))
}

/// Linear scan may reuse a register after its live interval ends while an
/// abstract frame state still names the stale interpreter-register value.
/// Give only those colliding values fresh spill homes before concrete deopt
/// lowering. This preserves every non-conflicting register assignment and the
/// emitter materializes supported definitions directly into the legalized
/// location, so machine code and the returned deopt table agree.
fn legalize_deopt_locations(
    allocation: &Allocation,
    frame_states: &FrameStateTable,
) -> Result<Allocation, Unsupported> {
    let mut legalized = allocation.clone();
    let mut next_spill = legalized.spill_slot_count;
    for state in frame_states.states() {
        let mut owners = BTreeMap::<Location, ValueId>::new();
        for value in state.registers.iter().flatten().copied() {
            let location = legalized.location(value);
            if owners.get(&location).is_some_and(|owner| *owner != value) {
                legalized.locations[value.0 as usize] = Location::Spill(next_spill);
                next_spill = next_spill
                    .checked_add(1)
                    .ok_or(Unsupported::OperandShape("optimizing deopt spill overflow"))?;
                owners.insert(legalized.location(value), value);
            } else {
                owners.insert(location, value);
            }
        }
    }
    legalized.spill_slot_count = next_spill;
    Ok(legalized)
}

fn check_eligibility(
    view: &JitCompileSnapshot,
    cfg: &ControlFlowGraph,
    ssa: &SsaFunction,
    reprs: &ReprMap,
    allocation: &Allocation,
) -> Result<Eligibility, Unsupported> {
    if cfg.blocks.len() != 1 || cfg.entry.0 != 0 {
        return Err(Unsupported::OperandShape(
            "optimizing subset requires one basic block",
        ));
    }
    let block = &cfg.blocks[0];
    if block.terminator != Terminator::Return
        || !block.normal_succs.is_empty()
        || !block.exception_succs.is_empty()
    {
        return Err(Unsupported::OperandShape(
            "optimizing subset requires a plain return terminator",
        ));
    }
    if ssa
        .values
        .iter()
        .any(|value| matches!(value.def, ValueDef::Phi { .. }))
    {
        return Err(Unsupported::OperandShape(
            "optimizing subset does not support phis",
        ));
    }

    let spill_bytes = allocation
        .spill_slot_count
        .checked_mul(STACK_SLOT_BYTES)
        .and_then(|bytes| bytes.checked_add(15))
        .map(|bytes| bytes & !15)
        .ok_or(Unsupported::OperandShape("optimizing spill frame overflow"))?;
    if spill_bytes > MAX_SPILL_FRAME_BYTES {
        return Err(Unsupported::OperandShape(
            "optimizing spill frame exceeds arm64 immediates",
        ));
    }

    let mut used_params = BTreeMap::<ValueId, (u32, u32)>::new();
    let mut allowed_conversions = BTreeSet::new();
    for instruction in &ssa.blocks[0].instrs {
        match instruction.op {
            Op::LoadInt32 => check_constant_result(instruction, reprs)?,
            Op::LoadNumber => {
                check_constant_result(instruction, reprs)?;
                exact_load_number(view, instruction.pc)?;
            }
            Op::Add | Op::Sub | Op::Mul => {
                let result = instruction
                    .result
                    .ok_or(Unsupported::OperandShape("arithmetic result"))?;
                if reprs.representation(result) != Representation::Int32
                    || instruction.inputs.len() != 2
                {
                    return Err(Unsupported::Opcode(instruction.op));
                }
                for (operand_index, &input) in instruction.inputs.iter().enumerate() {
                    match reprs.representation(input) {
                        Representation::Int32 => {}
                        Representation::Tagged => {
                            let ValueDef::Param { index, .. } = ssa.values[input.0 as usize].def
                            else {
                                return Err(Unsupported::Opcode(instruction.op));
                            };
                            let conversion = reprs.conversions().iter().find(|conversion| {
                                conversion.at_pc == instruction.pc
                                    && conversion.operand_index == operand_index
                            });
                            if !matches!(
                                conversion,
                                Some(conversion)
                                    if conversion.value == input
                                        && conversion.kind
                                            == ConversionKind::CheckedTaggedToInt32
                                        && conversion.may_deopt
                            ) {
                                return Err(Unsupported::Opcode(instruction.op));
                            }
                            used_params
                                .entry(input)
                                .and_modify(|entry| {
                                    if instruction.pc < entry.1 {
                                        *entry = (index, instruction.pc);
                                    }
                                })
                                .or_insert((index, instruction.pc));
                            allowed_conversions.insert((instruction.pc, operand_index));
                        }
                        Representation::Float64 => {
                            return Err(Unsupported::Opcode(instruction.op));
                        }
                    }
                }
            }
            Op::Return | Op::ReturnValue => {
                if instruction.result.is_some() || instruction.inputs.len() != 1 {
                    return Err(Unsupported::OperandShape("optimizing return shape"));
                }
                let returned = instruction.inputs[0];
                if reprs.representation(returned) != Representation::Int32 {
                    return Err(Unsupported::Opcode(instruction.op));
                }
                let conversion = reprs.conversions().iter().find(|conversion| {
                    conversion.at_pc == instruction.pc && conversion.operand_index == 0
                });
                if !matches!(
                    conversion,
                    Some(conversion)
                        if conversion.value == returned
                            && conversion.kind == ConversionKind::BoxInt32
                            && !conversion.may_deopt
                ) {
                    return Err(Unsupported::OperandShape(
                        "optimizing return requires int32 boxing",
                    ));
                }
                allowed_conversions.insert((instruction.pc, 0));
            }
            other => return Err(Unsupported::Opcode(other)),
        }
    }

    if reprs.conversions().iter().any(|conversion| {
        !allowed_conversions.contains(&(conversion.at_pc, conversion.operand_index))
    }) {
        return Err(Unsupported::OperandShape(
            "optimizing subset has an unsupported conversion",
        ));
    }

    let mut guarded_params = Vec::with_capacity(used_params.len());
    for (value, (index, first_use_pc)) in used_params {
        let offset = index
            .checked_mul(STACK_SLOT_BYTES)
            .ok_or(Unsupported::OperandShape("optimizing parameter offset"))?;
        if offset > MAX_PARAMETER_OFFSET {
            return Err(Unsupported::OperandShape(
                "optimizing parameter exceeds arm64 load range",
            ));
        }
        guarded_params.push(GuardedParam {
            value,
            index,
            first_use_pc,
            deopt_byte_pc: byte_pc(view, first_use_pc)?,
        });
    }
    guarded_params.sort_by_key(|param| param.index);
    Ok(Eligibility { guarded_params })
}

fn check_constant_result(instruction: &SsaInstr, reprs: &ReprMap) -> Result<(), Unsupported> {
    let result = instruction
        .result
        .ok_or(Unsupported::OperandShape("optimizing constant result"))?;
    if !instruction.inputs.is_empty() || reprs.representation(result) != Representation::Int32 {
        return Err(Unsupported::Opcode(instruction.op));
    }
    Ok(())
}

fn exact_load_number(view: &JitCompileSnapshot, pc: u32) -> Result<i32, Unsupported> {
    let number = view
        .instructions
        .get(pc as usize)
        .and_then(|instruction| instruction.load_number)
        .ok_or(Unsupported::OperandShape("optimizing LoadNumber metadata"))?;
    if !number.is_finite()
        || (number == 0.0 && number.is_sign_negative())
        || number < f64::from(i32::MIN)
        || number > f64::from(i32::MAX)
        || number != f64::from(number as i32)
    {
        return Err(Unsupported::Opcode(Op::LoadNumber));
    }
    Ok(number as i32)
}

fn byte_pc(view: &JitCompileSnapshot, pc: u32) -> Result<u32, Unsupported> {
    view.instructions
        .get(pc as usize)
        .map(|instruction| instruction.byte_pc)
        .ok_or(Unsupported::OperandShape("optimizing instruction byte PC"))
}

fn emit(
    view: &JitCompileSnapshot,
    ssa: &SsaFunction,
    allocation: &Allocation,
    eligibility: &Eligibility,
    deopt_table: &DeoptTable,
) -> Result<CompiledCode, Unsupported> {
    let spill_frame_bytes = aligned_spill_bytes(allocation.spill_slot_count)?;
    let mut ops = Assembler::new().expect("arm64 optimizing assembler allocation");
    let mut deopt_exits = Vec::<(DynamicLabel, u32)>::new();
    let entry = ops.offset();
    emit_prologue(&mut ops, spill_frame_bytes);

    dynasm!(ops ; .arch aarch64 ; mov x8, x0 ; mov x13, x1);
    for value in &ssa.values {
        match value.def {
            ValueDef::Param { index, .. } => {
                emit_load_parameter(&mut ops, index, 9);
                emit_store_tagged_location(&mut ops, allocation.location(value.id), 9)?;
            }
            ValueDef::Uninitialized { .. } => {
                emit_load_u32(&mut ops, 9, otter_vm::Value::undefined().to_bits() as u32);
                emit_store_tagged_location(&mut ops, allocation.location(value.id), 9)?;
            }
            ValueDef::ExceptionInput { .. } | ValueDef::Phi { .. } | ValueDef::Op { .. } => {}
        }
    }

    for instruction in &ssa.blocks[0].instrs {
        for param in eligibility
            .guarded_params
            .iter()
            .filter(|param| param.first_use_pc == instruction.pc)
        {
            let deopt = ops.new_dynamic_label();
            emit_load_tagged_location(&mut ops, allocation.location(param.value), 9)?;
            dynasm!(ops
                ; .arch aarch64
                ; lsr x10, x9, #48
                ; movz x11, #0xfffe
                ; cmp x10, x11
                ; b.ne =>deopt
            );
            deopt_exits.push((deopt, param.deopt_byte_pc));
        }
        match instruction.op {
            Op::LoadInt32 => {
                let value = load_int32(view, instruction.pc)?;
                emit_load_i32(&mut ops, 9, value);
                emit_store_location(
                    &mut ops,
                    allocation.location(instruction.result.expect("eligibility checked result")),
                    9,
                )?;
            }
            Op::LoadNumber => {
                let value = exact_load_number(view, instruction.pc)?;
                emit_load_i32(&mut ops, 9, value);
                emit_store_location(
                    &mut ops,
                    allocation.location(instruction.result.expect("eligibility checked result")),
                    9,
                )?;
            }
            Op::Add | Op::Sub | Op::Mul => {
                emit_load_location(&mut ops, allocation.location(instruction.inputs[0]), 9)?;
                emit_load_location(&mut ops, allocation.location(instruction.inputs[1]), 10)?;
                let deopt = ops.new_dynamic_label();
                match instruction.op {
                    Op::Add => dynasm!(ops
                        ; .arch aarch64
                        ; adds w11, w9, w10
                        ; b.vs =>deopt
                    ),
                    Op::Sub => dynasm!(ops
                        ; .arch aarch64
                        ; subs w11, w9, w10
                        ; b.vs =>deopt
                    ),
                    Op::Mul => dynasm!(ops
                        ; .arch aarch64
                        ; smull x11, w9, w10
                        ; sxtw x12, w11
                        ; cmp x11, x12
                        ; b.ne =>deopt
                    ),
                    _ => unreachable!(),
                }
                emit_store_location(
                    &mut ops,
                    allocation.location(instruction.result.expect("eligibility checked result")),
                    11,
                )?;
                deopt_exits.push((deopt, byte_pc(view, instruction.pc)?));
            }
            Op::Return | Op::ReturnValue => {
                emit_load_location(&mut ops, allocation.location(instruction.inputs[0]), 9)?;
                dynasm!(ops
                    ; .arch aarch64
                    ; movz x10, #0xfffe, lsl #48
                    ; orr x0, x10, x9
                    ; movz x1, STATUS_RETURNED as u32
                );
                emit_epilogue(&mut ops, spill_frame_bytes);
            }
            _ => unreachable!("eligibility rejected unsupported opcode"),
        }
    }

    for (label, deopt_byte_pc) in deopt_exits {
        dynasm!(ops ; .arch aarch64 ; =>label);
        let frame_state = deopt_table
            .lookup(deopt_byte_pc)
            .ok_or(Unsupported::OperandShape(
                "optimizing deopt exit missing frame state",
            ))?;
        emit_deopt_writeback(&mut ops, frame_state)?;
        emit_load_u32(&mut ops, 0, deopt_byte_pc);
        dynasm!(ops ; .arch aarch64 ; movz x1, STATUS_DEOPT as u32);
        emit_epilogue(&mut ops, spill_frame_bytes);
    }

    let buffer = ops
        .finalize()
        .expect("arm64 optimizing assembler finalization");
    Ok(CompiledCode::new(buffer, entry))
}

fn emit_load_parameter(ops: &mut Assembler, index: u32, scratch: u8) {
    let offset = index * STACK_SLOT_BYTES;
    if offset <= MAX_PARAMETER_OFFSET {
        dynasm!(ops ; .arch aarch64 ; ldr X(scratch), [x8, offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; ldr X(scratch), [x8, x12]);
    }
}

fn emit_deopt_writeback(ops: &mut Assembler, frame_state: &FrameState) -> Result<(), Unsupported> {
    for (register, slot) in frame_state.slots.iter().enumerate() {
        match slot.location {
            DeoptLocation::Register(register) => {
                let physical = VALUE_REGISTERS.get(register as usize).copied().ok_or(
                    Unsupported::OperandShape("optimizing deopt register mapping"),
                )?;
                match slot.repr {
                    DeoptRepr::Int32 => dynasm!(ops ; .arch aarch64 ; mov w9, W(physical)),
                    DeoptRepr::Tagged | DeoptRepr::Float64 => {
                        dynasm!(ops ; .arch aarch64 ; mov x9, X(physical));
                    }
                }
            }
            DeoptLocation::StackSlot(offset) => {
                let offset = u32::try_from(offset).map_err(|_| {
                    Unsupported::OperandShape("optimizing negative deopt spill offset")
                })?;
                match slot.repr {
                    DeoptRepr::Int32 => dynasm!(ops ; .arch aarch64 ; ldr w9, [sp, offset]),
                    DeoptRepr::Tagged | DeoptRepr::Float64 => {
                        dynasm!(ops ; .arch aarch64 ; ldr x9, [sp, offset]);
                    }
                }
            }
            DeoptLocation::Constant(_) => {
                emit_load_u32(ops, 9, otter_vm::Value::undefined().to_bits() as u32);
            }
        }
        match slot.repr {
            DeoptRepr::Tagged => {}
            DeoptRepr::Int32 => dynasm!(ops
                ; .arch aarch64
                ; movz x10, #0xfffe, lsl #48
                ; orr x9, x10, x9
            ),
            DeoptRepr::Float64 => emit_box_float64(ops),
        }
        emit_store_frame_register(ops, register as u32, 9)?;
    }
    Ok(())
}

fn emit_box_float64(ops: &mut Assembler) {
    let not_nan = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; ubfx x10, x9, #52, #11
        ; cmp x10, #0x7ff
        ; b.ne =>not_nan
        ; lsl x10, x9, #12
        ; cbz x10, =>not_nan
        ; movz x9, #0x7ff8, lsl #48
        ; =>not_nan
        ; movz x10, #2, lsl #48
        ; add x9, x9, x10
    );
}

fn emit_store_frame_register(
    ops: &mut Assembler,
    register: u32,
    scratch: u8,
) -> Result<(), Unsupported> {
    let offset = register
        .checked_mul(STACK_SLOT_BYTES)
        .ok_or(Unsupported::OperandShape(
            "optimizing frame register offset",
        ))?;
    if offset <= MAX_PARAMETER_OFFSET {
        dynasm!(ops ; .arch aarch64 ; str X(scratch), [x13, offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; str X(scratch), [x13, x12]);
    }
    Ok(())
}

fn load_int32(view: &JitCompileSnapshot, pc: u32) -> Result<i32, Unsupported> {
    match view
        .instructions
        .get(pc as usize)
        .and_then(|instruction| instruction.operand(&view.code_block, 1))
    {
        Some(Operand::Imm32(value)) => Ok(value),
        _ => Err(Unsupported::OperandShape("optimizing LoadInt32 operands")),
    }
}

fn aligned_spill_bytes(spill_slot_count: u32) -> Result<u32, Unsupported> {
    let bytes = spill_slot_count
        .checked_mul(STACK_SLOT_BYTES)
        .and_then(|bytes| bytes.checked_add(15))
        .map(|bytes| bytes & !15)
        .ok_or(Unsupported::OperandShape("optimizing spill frame overflow"))?;
    if bytes > MAX_SPILL_FRAME_BYTES {
        return Err(Unsupported::OperandShape(
            "optimizing spill frame exceeds arm64 immediates",
        ));
    }
    Ok(bytes)
}

fn spill_offset(slot: u32) -> Result<u32, Unsupported> {
    slot.checked_mul(STACK_SLOT_BYTES)
        .ok_or(Unsupported::OperandShape("optimizing spill offset"))
}

fn emit_load_location(
    ops: &mut Assembler,
    location: Location,
    scratch: u8,
) -> Result<(), Unsupported> {
    match location {
        Location::Register(register) => {
            let physical = VALUE_REGISTERS
                .get(register as usize)
                .copied()
                .ok_or(Unsupported::OperandShape("optimizing register mapping"))?;
            dynasm!(ops ; .arch aarch64 ; mov W(scratch), W(physical));
        }
        Location::Spill(slot) => {
            let offset = spill_offset(slot)?;
            dynasm!(ops ; .arch aarch64 ; ldr W(scratch), [sp, offset]);
        }
    }
    Ok(())
}

fn emit_store_location(
    ops: &mut Assembler,
    location: Location,
    scratch: u8,
) -> Result<(), Unsupported> {
    match location {
        Location::Register(register) => {
            let physical = VALUE_REGISTERS
                .get(register as usize)
                .copied()
                .ok_or(Unsupported::OperandShape("optimizing register mapping"))?;
            dynasm!(ops ; .arch aarch64 ; mov W(physical), W(scratch));
        }
        Location::Spill(slot) => {
            let offset = spill_offset(slot)?;
            dynasm!(ops ; .arch aarch64 ; str W(scratch), [sp, offset]);
        }
    }
    Ok(())
}

fn emit_load_tagged_location(
    ops: &mut Assembler,
    location: Location,
    scratch: u8,
) -> Result<(), Unsupported> {
    match location {
        Location::Register(register) => {
            let physical = VALUE_REGISTERS
                .get(register as usize)
                .copied()
                .ok_or(Unsupported::OperandShape("optimizing register mapping"))?;
            dynasm!(ops ; .arch aarch64 ; mov X(scratch), X(physical));
        }
        Location::Spill(slot) => {
            let offset = spill_offset(slot)?;
            dynasm!(ops ; .arch aarch64 ; ldr X(scratch), [sp, offset]);
        }
    }
    Ok(())
}

fn emit_store_tagged_location(
    ops: &mut Assembler,
    location: Location,
    scratch: u8,
) -> Result<(), Unsupported> {
    match location {
        Location::Register(register) => {
            let physical = VALUE_REGISTERS
                .get(register as usize)
                .copied()
                .ok_or(Unsupported::OperandShape("optimizing register mapping"))?;
            dynasm!(ops ; .arch aarch64 ; mov X(physical), X(scratch));
        }
        Location::Spill(slot) => {
            let offset = spill_offset(slot)?;
            dynasm!(ops ; .arch aarch64 ; str X(scratch), [sp, offset]);
        }
    }
    Ok(())
}

fn emit_load_i32(ops: &mut Assembler, register: u8, value: i32) {
    emit_load_u32(ops, register, value as u32);
}

fn emit_load_u32(ops: &mut Assembler, register: u8, value: u32) {
    let low = value & 0xffff;
    let high = value >> 16;
    dynasm!(ops
        ; .arch aarch64
        ; movz W(register), low
        ; movk W(register), high, lsl #16
    );
}

fn emit_prologue(ops: &mut Assembler, spill_frame_bytes: u32) {
    dynasm!(ops
        ; .arch aarch64
        ; stp x19, x20, [sp, #-32]!
        ; stp x21, x22, [sp, #16]
    );
    if spill_frame_bytes != 0 {
        dynasm!(ops ; .arch aarch64 ; sub sp, sp, spill_frame_bytes);
    }
}

fn emit_epilogue(ops: &mut Assembler, spill_frame_bytes: u32) {
    if spill_frame_bytes != 0 {
        dynasm!(ops ; .arch aarch64 ; add sp, sp, spill_frame_bytes);
    }
    dynasm!(ops
        ; .arch aarch64
        ; ldp x21, x22, [sp, #16]
        ; ldp x19, x20, [sp], #32
        ; ret
    );
}

#[cfg(test)]
mod tests {
    use otter_vm::{
        jit::JitTestInstruction,
        jit_feedback::{ARITH_INT32, ArithFeedback},
    };

    use super::*;
    use crate::optimizing::{
        OPTIMIZED_STATUS_DEOPT, OPTIMIZED_STATUS_RETURNED, OptimizedLeafEntry, OptimizedLeafRet,
    };

    const STRIDE: u32 = 8;

    fn box_i32(value: i32) -> u64 {
        (0xfffe_u64 << 48) | u64::from(value as u32)
    }

    fn unbox_i32(value: u64) -> i32 {
        value as u32 as i32
    }

    fn view(
        param_count: u16,
        register_count: u16,
        instructions: Vec<(Op, Vec<Operand>)>,
    ) -> JitCompileSnapshot {
        let mut view = JitCompileSnapshot::without_feedback(
            41,
            param_count,
            register_count,
            instructions
                .into_iter()
                .enumerate()
                .map(|(pc, (op, operands))| {
                    JitTestInstruction::new(op, pc as u32, pc as u32 * STRIDE + 3, operands)
                })
                .collect(),
        );
        for pc in 0..view.instructions.len() {
            if matches!(
                view.instructions[pc].op(&view.code_block),
                Op::Add | Op::Sub | Op::Mul
            ) {
                view.seed_arith_feedback_for_test(pc as u32, ArithFeedback::from_bits(ARITH_INT32));
            }
        }
        view
    }

    fn execute(code: &OptimizedCode, args: &[u64]) -> OptimizedLeafRet {
        execute_with_frame(code, args).0
    }

    fn execute_with_frame(code: &OptimizedCode, args: &[u64]) -> (OptimizedLeafRet, Vec<u64>) {
        // SAFETY: the compiler emitted `OptimizedLeafEntry`, `code` owns the
        // mapping through the call, and `args` covers every used parameter.
        let entry: OptimizedLeafEntry =
            unsafe { std::mem::transmute(code.compiled_code().entry_ptr()) };
        let mut frame =
            vec![otter_vm::Value::undefined().to_bits(); code.metadata().register_count as usize];
        let result = entry(args.as_ptr(), frame.as_mut_ptr());
        (result, frame)
    }

    #[test]
    fn executes_add() {
        let view = view(
            2,
            3,
            vec![
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let code = compile(&view, 1).expect("add is eligible");
        let result = execute(&code, &[box_i32(17), box_i32(-5)]);
        assert_eq!(result.status, OPTIMIZED_STATUS_RETURNED);
        assert_eq!(unbox_i32(result.value), 12);
    }

    #[test]
    fn executes_three_operation_expression() {
        let view = view(
            3,
            7,
            vec![
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(7)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(4),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Mul,
                    vec![
                        Operand::Register(5),
                        Operand::Register(4),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::Sub,
                    vec![
                        Operand::Register(6),
                        Operand::Register(5),
                        Operand::Register(3),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(6)]),
            ],
        );
        let code = compile(&view, 2).expect("three-op expression is eligible");
        let result = execute(&code, &[box_i32(6), box_i32(4), box_i32(3)]);
        assert_eq!(result.status, OPTIMIZED_STATUS_RETURNED);
        assert_eq!(unbox_i32(result.value), 23);
    }

    #[test]
    fn executes_forced_spills() {
        let mut instructions = vec![
            (Op::LoadInt32, vec![Operand::Register(4), Operand::Imm32(1)]),
            (Op::LoadInt32, vec![Operand::Register(5), Operand::Imm32(2)]),
            (Op::LoadInt32, vec![Operand::Register(6), Operand::Imm32(3)]),
            (Op::LoadInt32, vec![Operand::Register(7), Operand::Imm32(4)]),
        ];
        for (dst, left, right) in [
            (8, 0, 1),
            (9, 2, 3),
            (10, 4, 5),
            (11, 6, 7),
            (12, 8, 9),
            (13, 10, 11),
            (14, 12, 13),
        ] {
            instructions.push((
                Op::Add,
                vec![
                    Operand::Register(dst),
                    Operand::Register(left),
                    Operand::Register(right),
                ],
            ));
        }
        instructions.push((Op::ReturnValue, vec![Operand::Register(14)]));
        let view = view(4, 15, instructions);
        let code = compile(&view, 3).expect("spill expression is eligible");
        assert!(code.metadata().linear_scan_spill_slot_count > 0);
        assert!(code.metadata().spill_slot_count > 0);
        let result = execute(&code, &[box_i32(10), box_i32(20), box_i32(30), box_i32(40)]);
        assert_eq!(result.status, OPTIMIZED_STATUS_RETURNED);
        assert_eq!(unbox_i32(result.value), 110);
    }

    #[test]
    fn parameter_guard_deopts_at_first_use_byte_pc() {
        let view = view(
            2,
            3,
            vec![
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let code = compile(&view, 4).expect("guarded add is eligible");
        let (result, frame) = execute_with_frame(&code, &[0, box_i32(9)]);
        assert_eq!(result.status, OPTIMIZED_STATUS_DEOPT);
        assert_eq!(result.value, 3);
        assert_eq!(
            frame,
            vec![0, box_i32(9), otter_vm::Value::undefined().to_bits()]
        );
    }

    #[test]
    fn int32_overflow_deopts_at_arithmetic_byte_pc() {
        let view = view(
            2,
            3,
            vec![
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let code = compile(&view, 5).expect("overflow-checked add is eligible");
        let (result, frame) = execute_with_frame(&code, &[box_i32(i32::MAX), box_i32(1)]);
        assert_eq!(result.status, OPTIMIZED_STATUS_DEOPT);
        assert_eq!(result.value, 3);
        assert_eq!(
            frame,
            vec![
                box_i32(i32::MAX),
                box_i32(1),
                otter_vm::Value::undefined().to_bits()
            ]
        );
    }

    #[test]
    fn later_parameter_guard_materializes_prior_intermediates() {
        let view = view(
            2,
            5,
            vec![
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(7)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(4),
                        Operand::Register(3),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(4)]),
            ],
        );
        let code = compile(&view, 6).expect("later guard leaf is eligible");
        let undefined = otter_vm::Value::undefined().to_bits();
        let (result, frame) = execute_with_frame(&code, &[box_i32(5), undefined]);
        assert_eq!(result.status, OPTIMIZED_STATUS_DEOPT);
        assert_eq!(result.value, 19);
        assert_eq!(
            frame,
            vec![box_i32(5), undefined, box_i32(7), box_i32(12), undefined]
        );
    }

    #[test]
    fn refuses_property_operation() {
        let view = view(
            1,
            2,
            vec![
                (
                    Op::LoadProperty,
                    vec![
                        Operand::Register(1),
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        assert!(compile(&view, 6).is_err());
    }
}
