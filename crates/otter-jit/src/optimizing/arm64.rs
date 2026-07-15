//! Arm64 emission for the reducible numeric and element-access optimizing subset.
//!
//! # Contents
//! - Whole-pipeline construction and eligibility validation.
//! - Reducible loop checks, numeric comparisons, and per-edge phi copies.
//! - Loop-header OSR trampolines materializing allocated state from the
//!   interpreter register window.
//! - Cooperative back-edge polling with loop-header bail writeback.
//! - Precise live-tagged GC safepoints around element load/store transitions.
//! - Guarded numeric fast paths for source-lowered coercion scaffolding.
//! - Tagged-number guards, mixed-representation arithmetic, spills, boxing,
//!   and bail exits backed by exact deopt frame states.
//!
//! # Invariants
//! - `x20` retains the sole `JitCtx` argument and `x19` retains `ctx.regs`.
//!   GPR linear-scan registers `0..4` map to `x21..x24`, disjoint from both
//!   fixed ABI registers; FP registers `0..8` map to the AAPCS64 callee-saved
//!   `d8..d15`. `x8..x15` and `d16..d17` are caller-saved scratch registers.
//!   GPR spill slots precede FP spill slots in one aligned stack frame.
//! - Every tagged numeric input, including an element-load result, is checked
//!   with the VM's frozen number-tag mask before entering an unboxed operation.
//!   `ToPrimitive` / `ToNumeric` accept only those checked number encodings;
//!   other values bail before any user-observable coercion can run.
//! - `Add`/`Sub` use the arm64 signed-overflow flag and `Mul` compares its
//!   signed 64-bit product with the sign-extended low word. Overflow bails at
//!   the arithmetic instruction's exact logical PC; it never silently wraps.
//! - Every CFG edge targets a block label in reverse postorder. Sequentialized
//!   phi moves execute only on their owning edge before its final jump.
//!   Structurally dead compiler-scratch phis are initialized at block entry
//!   instead of receiving cross-representation edge copies.
//! - Float64 arithmetic never bails for overflow or division by zero. NaNs
//!   remain unordered in comparisons and are canonicalized whenever boxed.
//! - Every backwards bytecode edge targets a dominating loop header. Its phi moves
//!   execute before the interrupt/fuel poll so a poll exit reconstructs the
//!   loop-header frame, and no optimized loop can bypass cooperative polling.
//! - Every OSR trampoline loads exactly the live loop-header frame-state
//!   values, unboxes them into their allocated locations, and only then
//!   branches to the header body. A representation mismatch bails with the
//!   untouched interpreter window.
//! - Conditional inputs are optimizer-produced tagged booleans and branch by
//!   exact comparison with the VM's `true` immediate.
//! - Every element transition boxes its operands plus tagged SSA values live
//!   across the call into their interpreter-window slots. Its precise frame
//!   bitmap names only tagged materialized slots; moving-GC reloads restore
//!   those values and a load result while numeric machine locations remain
//!   untouched. Store scratch slots are non-roots that the runtime may clobber.
//!   Poll slow paths still bail so the interpreter owns interrupt/budget handling.
//!
//! # See also
//! - [`super`] — public optimizing code object.
//! - [`crate::entry`] — shared reentrant entry ABI and activation publication.
//! - [`crate::ir`] — source analyses and allocation contracts.

// dynasm's dynamic-register forms inject an internal conversion that Clippy
// sees as redundant when our register selector is already a `u8`.
#![allow(clippy::useless_conversion)]

use std::collections::{BTreeMap, BTreeSet};

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_bytecode::{Op, Operand};
use otter_vm::JitCompileSnapshot;
use otter_vm::deopt::{DeoptLocation, DeoptRepr, DeoptTable, FrameState};
use otter_vm::native_abi::{
    FrameMap, NO_FRAME_STATE, STUB_JIT_LOAD_ELEMENT, STUB_JIT_LOAD_PROP_WINDOW,
    STUB_JIT_STORE_ELEMENT, STUB_JIT_STORE_PROP_WINDOW, SafepointId, SafepointRecord,
};

use super::{OptimizedCode, OptimizedMetadata};
use crate::{
    CompiledCode,
    entry::{
        CANONICAL_NAN_HI16, DOUBLE_OFFSET_HI16, NATIVE_FRAME_OFFSET, NATIVE_FRAME_PC_OFFSET,
        NUMBER_TAG_HI16, STATUS_BAILED, STATUS_RETURNED, STATUS_THREW, THIS_VALUE_OFFSET,
        THREAD_OFFSET,
        TransitionTable, Unsupported, VALUE_FALSE, VALUE_FALSE_LOW, VALUE_TRUE,
        VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET, VM_THREAD_INTERRUPT_CELL_OFFSET,
    },
    ir::{
        cfg::{BlockId, ControlFlowGraph, Terminator},
        deopt_lower::DeoptLowering,
        dom::DominatorTree,
        frame_state::{AbstractFrameState, FrameStateTable},
        liveness::Liveness,
        regalloc::{
            Allocation, EdgeMoves, Location, Move, RegClass, RegisterBudget, has_non_dead_use,
            is_dead_phi,
        },
        repr::{ConversionKind, ReprMap, Representation},
        ssa::{SsaFunction, SsaInstr, ValueDef, ValueId},
    },
};

const ALLOCATABLE_REGISTER_COUNT: u8 = 4;
const REGISTER_BUDGET: RegisterBudget = RegisterBudget {
    gpr: ALLOCATABLE_REGISTER_COUNT,
    fp: 8,
};
const VALUE_REGISTERS: [u8; ALLOCATABLE_REGISTER_COUNT as usize] = [21, 22, 23, 24];
const FP_REGISTERS: [u8; 8] = [8, 9, 10, 11, 12, 13, 14, 15];
const FP_SCRATCH: u8 = 16;
const FP_SCRATCH_2: u8 = 17;
const STACK_SLOT_BYTES: u32 = 8;
const MAX_SPILL_FRAME_BYTES: u32 = 4080;
const MAX_PARAMETER_OFFSET: u32 = 32_760;

#[derive(Debug, Clone, Copy)]
struct GuardedUse {
    use_pc: u32,
    deopt_byte_pc: u32,
}

#[derive(Debug)]
struct Eligibility {
    guarded_uses: Vec<GuardedUse>,
    /// `(deopt-table byte PC, native-frame logical resume PC)` per back-edge.
    back_edges: BTreeMap<(BlockId, BlockId), (u32, u32)>,
    /// Verified loop-header entry state keyed by target block.
    osr_entries: BTreeMap<BlockId, OsrEntrySite>,
    /// Precise transition protocol per element load/store logical PC.
    element_transitions: ElementTransitionSafepoints,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OsrLiveValue {
    value: ValueId,
    register: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OsrEntrySite {
    logical_pc: u32,
    live_values: Box<[OsrLiveValue]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TaggedLiveAcross {
    value: ValueId,
    register: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ElementTransitionSite {
    safepoint_id: SafepointId,
    frame_map: FrameMap,
    tagged_live_across: Box<[TaggedLiveAcross]>,
}

#[derive(Debug)]
struct ElementTransitionSafepoints {
    sites: BTreeMap<u32, ElementTransitionSite>,
    /// Concatenated immutable frame-map bitmap words owned by the code object.
    bitmap_words: Box<[u64]>,
}

struct EligibilityAnalyses<'a> {
    liveness: &'a Liveness,
    reprs: &'a ReprMap,
    allocation: &'a Allocation,
    frame_states: &'a FrameStateTable,
}

struct EmissionPlan<'a> {
    reprs: &'a ReprMap,
    allocation: &'a Allocation,
    eligibility: &'a Eligibility,
    deopt_table: &'a DeoptTable,
    load_element_entry: u64,
    store_element_entry: u64,
    load_property_entry: u64,
    store_property_entry: u64,
    /// Owning function id, baked into property transitions so the stub resolves
    /// the name constant against this function's constant pool.
    function_id: u64,
}

struct OptimizedEmission {
    code: CompiledCode,
    osr_entries: BTreeMap<u32, usize>,
}

#[cfg(test)]
fn compile(view: &JitCompileSnapshot, code_object_id: u64) -> Result<OptimizedCode, Unsupported> {
    let transitions = TransitionTable::resolve();
    compile_with_transitions(view, code_object_id, &transitions)
}

pub(super) fn compile_with_transitions(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &TransitionTable,
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
    let reprs = ReprMap::compute(view, &ssa);
    reprs
        .verify(view, &ssa)
        .map_err(|_| Unsupported::OperandShape("optimizing representation verification"))?;
    let allocation = Allocation::compute(&ssa, &cfg, &liveness, &reprs, REGISTER_BUDGET)
        .map_err(|_| Unsupported::OperandShape("optimizing register allocation"))?;
    allocation
        .verify(&ssa, &cfg, &liveness, &reprs)
        .map_err(|_| Unsupported::OperandShape("optimizing allocation verification"))?;
    let frame_states = FrameStateTable::build(&ssa, &cfg)
        .map_err(|_| Unsupported::OperandShape("optimizing frame-state construction"))?;
    frame_states
        .verify(&ssa, &cfg, &dom)
        .map_err(|_| Unsupported::OperandShape("optimizing frame-state verification"))?;
    let linear_scan_spill_slot_count = total_spill_slots(&allocation)?;
    let mut allocation = legalize_deopt_locations(&allocation, &frame_states)?;
    allocation
        .rebuild_edge_moves(&ssa, &cfg, &reprs)
        .map_err(|_| Unsupported::OperandShape("optimizing legalized phi moves"))?;
    let deopt = DeoptLowering::build(view, &ssa, &frame_states, &allocation, &reprs)
        .map_err(|_| Unsupported::OperandShape("optimizing deopt lowering"))?;

    let eligibility = check_eligibility(
        view,
        &cfg,
        &dom,
        &ssa,
        EligibilityAnalyses {
            liveness: &liveness,
            reprs: &reprs,
            allocation: &allocation,
            frame_states: &frame_states,
        },
    )?;
    let emission = emit(
        view,
        &cfg,
        dom.reverse_postorder(),
        &ssa,
        EmissionPlan {
            reprs: &reprs,
            allocation: &allocation,
            eligibility: &eligibility,
            deopt_table: deopt.table(),
            load_element_entry: transitions.variadic_entry(STUB_JIT_LOAD_ELEMENT),
            store_element_entry: transitions.variadic_entry(STUB_JIT_STORE_ELEMENT),
            load_property_entry: transitions.variadic_entry(STUB_JIT_LOAD_PROP_WINDOW),
            store_property_entry: transitions.variadic_entry(STUB_JIT_STORE_PROP_WINDOW),
            function_id: u64::from(view.code_block.id),
        },
    )?;
    let frame_maps = eligibility
        .element_transitions
        .sites
        .values()
        .map(|site| site.frame_map)
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let safepoint_records = frame_maps
        .iter()
        .copied()
        .map(|frame_map| {
            SafepointRecord::from_frame_map(
                frame_map,
                NO_FRAME_STATE,
                &eligibility.element_transitions.bitmap_words,
            )
            .ok_or(Unsupported::OperandShape(
                "optimizing precise frame-map expansion",
            ))
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_boxed_slice();
    Ok(OptimizedCode::new(
        emission.code,
        deopt.table().clone(),
        safepoint_records,
        frame_maps,
        eligibility.element_transitions.bitmap_words,
        emission.osr_entries,
        OptimizedMetadata {
            code_object_id,
            function_id: view.code_block.id,
            param_count: view.code_block.param_count,
            register_count: view.code_block.register_count,
            machine_register_count: allocation
                .register_budget
                .gpr
                .checked_add(allocation.register_budget.fp)
                .ok_or(Unsupported::OperandShape(
                    "optimizing machine register count overflow",
                ))?,
            linear_scan_spill_slot_count,
            spill_slot_count: total_spill_slots(&allocation)?,
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
    for state in frame_states.states() {
        let mut owners = BTreeMap::<Location, ValueId>::new();
        for value in state.registers.iter().flatten().copied() {
            let location = legalized.location(value);
            if owners.get(&location).is_some_and(|owner| *owner != value) {
                let class = location.class();
                let next_spill = match class {
                    RegClass::Gpr => &mut legalized.spill_slot_counts.gpr,
                    RegClass::Fp => &mut legalized.spill_slot_counts.fp,
                };
                let slot = *next_spill;
                *next_spill = next_spill
                    .checked_add(1)
                    .ok_or(Unsupported::OperandShape("optimizing deopt spill overflow"))?;
                legalized.locations[value.0 as usize] = Location::Spill(class, slot);
                owners.insert(legalized.location(value), value);
            } else {
                owners.insert(location, value);
            }
        }
    }
    Ok(legalized)
}

fn check_eligibility(
    view: &JitCompileSnapshot,
    cfg: &ControlFlowGraph,
    dom: &DominatorTree,
    ssa: &SsaFunction,
    analyses: EligibilityAnalyses<'_>,
) -> Result<Eligibility, Unsupported> {
    let EligibilityAnalyses {
        liveness,
        reprs,
        allocation,
        frame_states,
    } = analyses;
    if cfg.entry.0 != 0 || dom.reverse_postorder().len() != cfg.blocks.len() {
        return Err(Unsupported::OperandShape(
            "optimizing subset requires one reachable entry graph",
        ));
    }
    let mut back_edges = BTreeMap::new();
    for block in &cfg.blocks {
        if !block.exception_succs.is_empty() {
            return Err(Unsupported::OperandShape(
                "optimizing subset rejects exception edges",
            ));
        }
        for successor in block.normal_succs.iter().copied() {
            if cfg.blocks[successor.0 as usize].start_pc <= block.start_pc {
                if !dom.dominates(successor, block.id) {
                    return Err(Unsupported::OperandShape(
                        "optimizing subset rejects irreducible back-edges",
                    ));
                }
                back_edges.insert(
                    (block.id, successor),
                    (
                        byte_pc(view, cfg.blocks[successor.0 as usize].start_pc)?,
                        cfg.blocks[successor.0 as usize].start_pc,
                    ),
                );
            }
        }
        match block.terminator {
            Terminator::FallThrough | Terminator::Jump if block.normal_succs.len() == 1 => {}
            Terminator::Branch { .. } if !block.normal_succs.is_empty() => {}
            Terminator::Return if block.normal_succs.is_empty() => {}
            _ => {
                return Err(Unsupported::OperandShape(
                    "optimizing subset has an unsupported terminator",
                ));
            }
        }
    }
    let spill_bytes = total_spill_slots(allocation)?
        .checked_mul(STACK_SLOT_BYTES)
        .and_then(|bytes| bytes.checked_add(15))
        .map(|bytes| bytes & !15)
        .ok_or(Unsupported::OperandShape("optimizing spill frame overflow"))?;
    if spill_bytes > MAX_SPILL_FRAME_BYTES {
        return Err(Unsupported::OperandShape(
            "optimizing spill frame exceeds arm64 immediates",
        ));
    }

    let mut guarded_uses = BTreeMap::<(u32, ValueId), Option<u32>>::new();
    let mut allowed_conversions = BTreeSet::new();
    let mut element_transition_instructions = Vec::new();
    for block in dom.reverse_postorder().iter().copied() {
        for (instruction_index, instruction) in
            ssa.blocks[block.0 as usize].instrs.iter().enumerate()
        {
            match instruction.op {
                Op::LoadInt32 => check_constant_result(instruction, reprs)?,
                Op::LoadNumber => check_number_constant_result(view, instruction, reprs)?,
                Op::LoadUndefined => check_tagged_constant_result(instruction, reprs)?,
                Op::LoadThis => check_tagged_constant_result(instruction, reprs)?,
                Op::LoadTrue | Op::LoadFalse => check_boolean_result(instruction, reprs)?,
                Op::LoadLocal | Op::StoreLocal => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("local move result"))?;
                    if instruction.inputs.len() != 1
                        || instruction.input_registers.len() != 1
                        || instruction.result_register.is_none()
                        || reprs.representation(result)
                            != reprs.representation(instruction.inputs[0])
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                }
                Op::ToPrimitive | Op::ToNumeric => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("numeric coercion result"))?;
                    if instruction.inputs.len() != 1
                        || instruction.input_registers.len() != 1
                        || instruction.result_register.is_none()
                        || reprs.representation(result) != Representation::Tagged
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    if reprs.representation(instruction.inputs[0]) == Representation::Tagged {
                        guarded_uses.insert((instruction.pc, instruction.inputs[0]), None);
                    }
                }
                Op::LoadElement => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("element-load result"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || instruction.inputs.len() != 2
                        || instruction.input_registers.len() != 2
                        || instruction.result_register.is_none()
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    element_transition_instructions.push((
                        instruction.pc,
                        block,
                        instruction_index,
                    ));
                }
                Op::StoreElement => {
                    let scratch = instruction
                        .result_register
                        .ok_or(Unsupported::OperandShape("element-store scratch"))?;
                    if instruction.result.is_none()
                        || instruction.inputs.len() != 3
                        || instruction.input_registers.len() != 3
                        || reprs.representation(instruction.inputs[0]) != Representation::Tagged
                        || reprs.representation(instruction.inputs[1]) != Representation::Int32
                        || instruction.input_registers.contains(&scratch)
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    element_transition_instructions.push((
                        instruction.pc,
                        block,
                        instruction_index,
                    ));
                }
                Op::LoadProperty => {
                    // `WRITE_READ_CONST`: the object is the sole register input,
                    // the name is a constant operand (an immediate, not a window
                    // slot), and the loaded value is tagged.
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("property-load result"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || instruction.inputs.len() != 1
                        || instruction.input_registers.len() != 1
                        || instruction.result_register.is_none()
                        || reprs.representation(instruction.inputs[0]) != Representation::Tagged
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    element_transition_instructions.push((
                        instruction.pc,
                        block,
                        instruction_index,
                    ));
                }
                Op::StoreProperty => {
                    // `READ_CONST_READ_WRITE`: object + value are register inputs,
                    // the name is a constant immediate, and the scratch WRITE slot
                    // is a spare interpreter-window register the stub may clobber.
                    let scratch = instruction
                        .result_register
                        .ok_or(Unsupported::OperandShape("property-store scratch"))?;
                    if instruction.result.is_none()
                        || instruction.inputs.len() != 2
                        || instruction.input_registers.len() != 2
                        || reprs.representation(instruction.inputs[0]) != Representation::Tagged
                        || instruction.input_registers.contains(&scratch)
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    element_transition_instructions.push((
                        instruction.pc,
                        block,
                        instruction_index,
                    ));
                }
                Op::Add | Op::Sub | Op::Mul => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("arithmetic result"))?;
                    let result_repr = reprs.representation(result);
                    if !matches!(result_repr, Representation::Int32 | Representation::Float64)
                        || instruction.inputs.len() != 2
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_numeric_inputs(
                        instruction,
                        ssa,
                        reprs,
                        result_repr,
                        &mut guarded_uses,
                        &mut allowed_conversions,
                    )?;
                }
                Op::Div => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("division result"))?;
                    if reprs.representation(result) != Representation::Float64
                        || instruction.inputs.len() != 2
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    check_numeric_inputs(
                        instruction,
                        ssa,
                        reprs,
                        Representation::Float64,
                        &mut guarded_uses,
                        &mut allowed_conversions,
                    )?;
                }
                Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Equal
                | Op::NotEqual => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("comparison result"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || instruction.inputs.len() != 2
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                    let input_repr = if view.feedback_at(instruction.pc).is_int32_only() {
                        Representation::Int32
                    } else if view.feedback_at(instruction.pc).is_numeric_only() {
                        Representation::Float64
                    } else {
                        return Err(Unsupported::Opcode(instruction.op));
                    };
                    check_numeric_inputs(
                        instruction,
                        ssa,
                        reprs,
                        input_repr,
                        &mut guarded_uses,
                        &mut allowed_conversions,
                    )?;
                }
                Op::Jump => {
                    if instruction.result.is_some() || !instruction.inputs.is_empty() {
                        return Err(Unsupported::OperandShape("optimizing jump shape"));
                    }
                }
                Op::JumpIfTrue | Op::JumpIfFalse => {
                    if instruction.result.is_some() || instruction.inputs.len() != 1 {
                        return Err(Unsupported::OperandShape("optimizing branch shape"));
                    }
                    let condition = instruction.inputs[0];
                    if reprs.representation(condition) != Representation::Tagged
                        || !is_boolean_value(ssa, condition)
                    {
                        return Err(Unsupported::Opcode(instruction.op));
                    }
                }
                Op::Return | Op::ReturnValue => {
                    if instruction.result.is_some() || instruction.inputs.len() != 1 {
                        return Err(Unsupported::OperandShape("optimizing return shape"));
                    }
                    let returned = instruction.inputs[0];
                    let returned_repr = reprs.representation(returned);
                    let expected_conversion = match returned_repr {
                        Representation::Int32 => ConversionKind::BoxInt32,
                        Representation::Float64 => ConversionKind::BoxFloat64,
                        Representation::Tagged => continue,
                    };
                    let conversion = reprs.conversions().iter().find(|conversion| {
                        conversion.at_pc == instruction.pc && conversion.operand_index == 0
                    });
                    if !matches!(
                        conversion,
                        Some(conversion)
                            if conversion.value == returned
                                && conversion.kind == expected_conversion
                                && !conversion.may_deopt
                    ) {
                        return Err(Unsupported::OperandShape(
                            "optimizing return requires numeric boxing",
                        ));
                    }
                    allowed_conversions.insert((instruction.pc, 0));
                }
                other => return Err(Unsupported::Opcode(other)),
            }
        }
    }

    if reprs.conversions().iter().any(|conversion| {
        !allowed_conversions.contains(&(conversion.at_pc, conversion.operand_index))
    }) {
        return Err(Unsupported::OperandShape(
            "optimizing subset has an unsupported conversion",
        ));
    }

    let mut guarded_numeric_uses = Vec::with_capacity(guarded_uses.len());
    for ((use_pc, _value), parameter_index) in guarded_uses {
        if let Some(index) = parameter_index {
            let offset = index
                .checked_mul(STACK_SLOT_BYTES)
                .ok_or(Unsupported::OperandShape("optimizing parameter offset"))?;
            if offset > MAX_PARAMETER_OFFSET {
                return Err(Unsupported::OperandShape(
                    "optimizing parameter exceeds arm64 load range",
                ));
            }
        }
        guarded_numeric_uses.push(GuardedUse {
            use_pc,
            deopt_byte_pc: byte_pc(view, use_pc)?,
        });
    }
    guarded_numeric_uses.sort_by_key(|guarded| guarded.use_pc);
    guarded_numeric_uses.dedup_by_key(|guarded| guarded.use_pc);
    element_transition_instructions.sort_unstable_by_key(|&(pc, _, _)| pc);
    element_transition_instructions.dedup_by_key(|instruction| instruction.0);
    let element_transitions = build_element_transition_sites(
        ssa,
        liveness,
        reprs,
        frame_states,
        element_transition_instructions,
    )?;
    let osr_entries = build_osr_entry_sites(cfg, ssa, liveness, frame_states)?;
    Ok(Eligibility {
        guarded_uses: guarded_numeric_uses,
        back_edges,
        osr_entries,
        element_transitions,
    })
}

fn build_osr_entry_sites(
    cfg: &ControlFlowGraph,
    ssa: &SsaFunction,
    liveness: &Liveness,
    frame_states: &FrameStateTable,
) -> Result<BTreeMap<BlockId, OsrEntrySite>, Unsupported> {
    let mut sites = BTreeMap::new();
    for block in cfg.blocks.iter().filter(|block| block.is_loop_header) {
        let frame_state = frame_states
            .at(block.start_pc)
            .ok_or(Unsupported::OperandShape(
                "optimizing OSR header frame state",
            ))?;
        let live_in = liveness.live_in(block.id);
        let mut register_by_value = BTreeMap::<ValueId, u16>::new();
        for (register, value) in frame_state.registers.iter().copied().enumerate() {
            let Some(value) = value.filter(|value| live_in.contains(value)) else {
                continue;
            };
            let register = u16::try_from(register)
                .map_err(|_| Unsupported::OperandShape("optimizing OSR register overflow"))?;
            register_by_value.entry(value).or_insert(register);
        }
        if live_in
            .iter()
            .any(|value| has_non_dead_use(ssa, *value) && !register_by_value.contains_key(value))
        {
            return Err(Unsupported::OperandShape(
                "optimizing OSR live value is absent from header frame state",
            ));
        }
        let live_values = register_by_value
            .into_iter()
            .map(|(value, register)| OsrLiveValue { value, register })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        sites.insert(
            block.id,
            OsrEntrySite {
                logical_pc: block.start_pc,
                live_values,
            },
        );
    }
    Ok(sites)
}

fn build_element_transition_sites(
    ssa: &SsaFunction,
    liveness: &Liveness,
    reprs: &ReprMap,
    frame_states: &FrameStateTable,
    instructions: Vec<(u32, BlockId, usize)>,
) -> Result<ElementTransitionSafepoints, Unsupported> {
    let bitmap_word_count = usize::from(ssa.register_count).div_ceil(u64::BITS as usize);
    let bitmap_word_count_u16 = u16::try_from(bitmap_word_count)
        .map_err(|_| Unsupported::OperandShape("optimizing frame-map word count overflow"))?;
    let mut bitmap_words = Vec::<u64>::new();
    let mut sites = BTreeMap::new();

    for (id, (pc, block, instruction_index)) in instructions.into_iter().enumerate() {
        let safepoint_id = u32::try_from(id)
            .map_err(|_| Unsupported::OperandShape("optimizing safepoint id overflow"))?;
        let instruction = ssa.blocks[block.0 as usize]
            .instrs
            .get(instruction_index)
            .ok_or(Unsupported::OperandShape(
                "optimizing element-transition instruction boundary",
            ))?;
        let frame_state = frame_states.at(pc).ok_or(Unsupported::OperandShape(
            "optimizing element-transition abstract frame state",
        ))?;
        let live_after = liveness
            .live_after_instruction(ssa, block, instruction_index)
            .ok_or(Unsupported::OperandShape(
                "optimizing element-transition live-out boundary",
            ))?;
        let result = instruction.result.ok_or(Unsupported::OperandShape(
            "optimizing element-transition result",
        ))?;
        if instruction.op == Op::StoreElement
            && live_after.contains(&result)
            && has_non_dead_use(ssa, result)
        {
            return Err(Unsupported::OperandShape(
                "optimizing element-store scratch is live after the transition",
            ));
        }
        let mut tagged_live_across = Vec::new();
        for value in live_after {
            if value == result || reprs.representation(value) != Representation::Tagged {
                continue;
            }
            let register =
                frame_register_for_value(frame_state, value).ok_or(Unsupported::OperandShape(
                    "optimizing tagged live-out value is absent from frame state",
                ))?;
            tagged_live_across.push(TaggedLiveAcross { value, register });
        }
        tagged_live_across.sort_unstable_by_key(|live| (live.register, live.value));
        tagged_live_across.dedup();

        let mut root_registers = tagged_live_across
            .iter()
            .map(|live| live.register)
            .collect::<BTreeSet<_>>();
        root_registers.insert(instruction.input_registers[0]);
        if instruction.op == Op::LoadElement
            && reprs.representation(instruction.inputs[1]) == Representation::Tagged
        {
            root_registers.insert(instruction.input_registers[1]);
        }
        if instruction.op == Op::StoreElement
            && reprs.representation(instruction.inputs[2]) == Representation::Tagged
        {
            root_registers.insert(instruction.input_registers[2]);
        }
        if instruction.op == Op::StoreElement
            && root_registers.contains(
                &instruction
                    .result_register
                    .expect("eligibility checked element-store scratch"),
            )
        {
            return Err(Unsupported::OperandShape(
                "optimizing element-store scratch aliases a tagged root",
            ));
        }

        let bitmap_offset = u32::try_from(bitmap_words.len())
            .map_err(|_| Unsupported::OperandShape("optimizing frame-map bitmap overflow"))?;
        let mut site_words = vec![0_u64; bitmap_word_count];
        for register in root_registers {
            let register = usize::from(register);
            site_words[register / u64::BITS as usize] |= 1_u64 << (register % u64::BITS as usize);
        }
        bitmap_words.extend(site_words);
        let frame_map = FrameMap {
            id: safepoint_id,
            bitmap_offset,
            bitmap_word_count: bitmap_word_count_u16,
            slot_count: ssa.register_count,
        };
        sites.insert(
            pc,
            ElementTransitionSite {
                safepoint_id,
                frame_map,
                tagged_live_across: tagged_live_across.into_boxed_slice(),
            },
        );
    }

    Ok(ElementTransitionSafepoints {
        sites,
        bitmap_words: bitmap_words.into_boxed_slice(),
    })
}

fn frame_register_for_value(frame_state: &AbstractFrameState, value: ValueId) -> Option<u16> {
    frame_state
        .registers
        .iter()
        .position(|candidate| *candidate == Some(value))
        .and_then(|register| u16::try_from(register).ok())
}

fn check_tagged_inputs(
    instruction: &SsaInstr,
    reprs: &ReprMap,
    allowed_conversions: &mut BTreeSet<(u32, usize)>,
) -> Result<(), Unsupported> {
    for (operand_index, &input) in instruction.inputs.iter().enumerate() {
        let expected_kind = match reprs.representation(input) {
            Representation::Tagged => continue,
            Representation::Int32 => ConversionKind::BoxInt32,
            Representation::Float64 => ConversionKind::BoxFloat64,
        };
        let conversion = reprs.conversions().iter().find(|conversion| {
            conversion.at_pc == instruction.pc && conversion.operand_index == operand_index
        });
        if !matches!(
            conversion,
            Some(conversion)
                if conversion.value == input
                    && conversion.kind == expected_kind
                    && !conversion.may_deopt
        ) {
            return Err(Unsupported::OperandShape(
                "optimizing element transition requires tagged operands",
            ));
        }
        allowed_conversions.insert((instruction.pc, operand_index));
    }
    Ok(())
}

fn check_numeric_inputs(
    instruction: &SsaInstr,
    ssa: &SsaFunction,
    reprs: &ReprMap,
    required: Representation,
    guarded_uses: &mut BTreeMap<(u32, ValueId), Option<u32>>,
    allowed_conversions: &mut BTreeSet<(u32, usize)>,
) -> Result<(), Unsupported> {
    for (operand_index, &input) in instruction.inputs.iter().enumerate() {
        let actual = reprs.representation(input);
        if actual == required {
            continue;
        }
        let expected_kind = match (actual, required) {
            (Representation::Int32, Representation::Float64) => ConversionKind::Int32ToFloat64,
            (Representation::Tagged, Representation::Int32) => ConversionKind::CheckedTaggedToInt32,
            (Representation::Tagged, Representation::Float64) => {
                ConversionKind::CheckedTaggedToFloat64
            }
            _ => return Err(Unsupported::Opcode(instruction.op)),
        };
        let conversion = reprs.conversions().iter().find(|conversion| {
            conversion.at_pc == instruction.pc && conversion.operand_index == operand_index
        });
        let may_deopt = matches!(
            expected_kind,
            ConversionKind::CheckedTaggedToInt32 | ConversionKind::CheckedTaggedToFloat64
        );
        if !matches!(
            conversion,
            Some(conversion)
                if conversion.value == input
                    && conversion.kind == expected_kind
                    && conversion.may_deopt == may_deopt
        ) {
            return Err(Unsupported::Opcode(instruction.op));
        }
        if may_deopt {
            let parameter_index = match ssa.values[input.0 as usize].def {
                ValueDef::Param { index, .. } => Some(index),
                ValueDef::Op {
                    op: Op::LoadElement | Op::ToPrimitive | Op::ToNumeric,
                    ..
                } => None,
                _ => return Err(Unsupported::Opcode(instruction.op)),
            };
            guarded_uses.insert((instruction.pc, input), parameter_index);
        }
        allowed_conversions.insert((instruction.pc, operand_index));
    }
    Ok(())
}

fn is_boolean_value(ssa: &SsaFunction, value: ValueId) -> bool {
    matches!(
        ssa.values[value.0 as usize].def,
        ValueDef::Op {
            op: Op::LoadTrue
                | Op::LoadFalse
                | Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Equal
                | Op::NotEqual,
            ..
        }
    )
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

fn check_number_constant_result(
    view: &JitCompileSnapshot,
    instruction: &SsaInstr,
    reprs: &ReprMap,
) -> Result<(), Unsupported> {
    let result = instruction.result.ok_or(Unsupported::OperandShape(
        "optimizing number constant result",
    ))?;
    let number = load_number(view, instruction.pc)?;
    let expected = if is_exact_i32(number) {
        Representation::Int32
    } else {
        Representation::Float64
    };
    if !instruction.inputs.is_empty() || reprs.representation(result) != expected {
        return Err(Unsupported::Opcode(instruction.op));
    }
    Ok(())
}

fn check_boolean_result(instruction: &SsaInstr, reprs: &ReprMap) -> Result<(), Unsupported> {
    let result = instruction
        .result
        .ok_or(Unsupported::OperandShape("optimizing boolean result"))?;
    if !instruction.inputs.is_empty() || reprs.representation(result) != Representation::Tagged {
        return Err(Unsupported::Opcode(instruction.op));
    }
    Ok(())
}

fn check_tagged_constant_result(
    instruction: &SsaInstr,
    reprs: &ReprMap,
) -> Result<(), Unsupported> {
    let result = instruction.result.ok_or(Unsupported::OperandShape(
        "optimizing tagged constant result",
    ))?;
    if !instruction.inputs.is_empty() || reprs.representation(result) != Representation::Tagged {
        return Err(Unsupported::Opcode(instruction.op));
    }
    Ok(())
}

fn load_number(view: &JitCompileSnapshot, pc: u32) -> Result<f64, Unsupported> {
    view.instructions
        .get(pc as usize)
        .and_then(|instruction| instruction.load_number)
        .ok_or(Unsupported::OperandShape("optimizing LoadNumber metadata"))
}

fn is_exact_i32(number: f64) -> bool {
    number.is_finite()
        && !(number == 0.0 && number.is_sign_negative())
        && number >= f64::from(i32::MIN)
        && number <= f64::from(i32::MAX)
        && number == f64::from(number as i32)
}

fn byte_pc(view: &JitCompileSnapshot, pc: u32) -> Result<u32, Unsupported> {
    view.instructions
        .get(pc as usize)
        .map(|instruction| instruction.byte_pc)
        .ok_or(Unsupported::OperandShape("optimizing instruction byte PC"))
}

fn emit(
    view: &JitCompileSnapshot,
    cfg: &ControlFlowGraph,
    rpo: &[BlockId],
    ssa: &SsaFunction,
    plan: EmissionPlan<'_>,
) -> Result<OptimizedEmission, Unsupported> {
    let EmissionPlan {
        reprs,
        allocation,
        eligibility,
        deopt_table,
        load_element_entry,
        store_element_entry,
        load_property_entry,
        store_property_entry,
        function_id,
    } = plan;
    let spill_frame_bytes = aligned_spill_bytes(total_spill_slots(allocation)?)?;
    let mut ops = Assembler::new().expect("arm64 optimizing assembler allocation");
    let mut deopt_exits = Vec::<(DynamicLabel, u32, u32)>::new();
    let threw = ops.new_dynamic_label();
    let block_labels: Vec<_> = (0..cfg.blocks.len())
        .map(|_| ops.new_dynamic_label())
        .collect();
    let entry = ops.offset();
    emit_prologue(&mut ops, spill_frame_bytes);

    dynasm!(ops
        ; .arch aarch64
        ; mov x20, x0
        ; ldr x19, [x20]
    );
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

    for block_id in rpo.iter().copied() {
        let block = &cfg.blocks[block_id.0 as usize];
        let label = block_labels[block_id.0 as usize];
        dynasm!(ops ; .arch aarch64 ; =>label);
        emit_initialize_dead_phis(
            &mut ops,
            ssa,
            reprs,
            allocation,
            &ssa.blocks[block_id.0 as usize].phis,
        )?;
        for instruction in &ssa.blocks[block_id.0 as usize].instrs {
            let guard_deopt = eligibility
                .guarded_uses
                .iter()
                .find(|param| param.use_pc == instruction.pc)
                .map(|param| {
                    let label = ops.new_dynamic_label();
                    deopt_exits.push((label, param.deopt_byte_pc, instruction.pc));
                    label
                });
            match instruction.op {
                Op::LoadInt32 => {
                    let value = load_int32(view, instruction.pc)?;
                    emit_load_i32(&mut ops, 9, value);
                    emit_store_location(
                        &mut ops,
                        allocation
                            .location(instruction.result.expect("eligibility checked result")),
                        9,
                    )?;
                }
                Op::LoadNumber => {
                    let result = instruction.result.expect("eligibility checked result");
                    let value = load_number(view, instruction.pc)?;
                    match reprs.representation(result) {
                        Representation::Int32 => {
                            emit_load_i32(&mut ops, 9, value as i32);
                            emit_store_location(&mut ops, allocation.location(result), 9)?;
                        }
                        Representation::Float64 => {
                            emit_load_u64(&mut ops, 9, value.to_bits());
                            dynasm!(ops ; .arch aarch64 ; fmov D(FP_SCRATCH), x9);
                            emit_store_fp_location(
                                &mut ops,
                                allocation,
                                allocation.location(result),
                                FP_SCRATCH,
                            )?;
                        }
                        Representation::Tagged => unreachable!("eligibility checked number"),
                    }
                }
                Op::LoadTrue | Op::LoadFalse => {
                    let value = if instruction.op == Op::LoadTrue {
                        VALUE_TRUE
                    } else {
                        VALUE_FALSE
                    };
                    emit_load_u32(&mut ops, 9, value as u32);
                    emit_store_tagged_location(
                        &mut ops,
                        allocation
                            .location(instruction.result.expect("eligibility checked result")),
                        9,
                    )?;
                }
                Op::LoadUndefined => {
                    emit_load_u32(&mut ops, 9, otter_vm::Value::undefined().to_bits() as u32);
                    emit_store_tagged_location(
                        &mut ops,
                        allocation.location(
                            instruction
                                .result
                                .expect("eligibility checked undefined result"),
                        ),
                        9,
                    )?;
                }
                Op::LoadThis => {
                    // `this` is a tagged value published in the JitCtx.
                    dynasm!(ops ; .arch aarch64 ; ldr x9, [x20, THIS_VALUE_OFFSET]);
                    emit_store_tagged_location(
                        &mut ops,
                        allocation.location(
                            instruction
                                .result
                                .expect("eligibility checked LoadThis result"),
                        ),
                        9,
                    )?;
                }
                Op::LoadLocal | Op::StoreLocal => {
                    emit_move(
                        &mut ops,
                        allocation,
                        Move {
                            src: allocation.location(instruction.inputs[0]),
                            dst: allocation.location(
                                instruction.result.expect("eligibility checked local move"),
                            ),
                        },
                    )?;
                }
                Op::ToPrimitive | Op::ToNumeric => {
                    emit_tagged_numeric_coercion(
                        &mut ops,
                        reprs,
                        allocation,
                        instruction,
                        guard_deopt,
                    )?;
                }
                Op::LoadElement => {
                    let dst = instruction
                        .result_register
                        .expect("eligibility checked element-load destination");
                    let receiver = instruction.input_registers[0];
                    let index = instruction.input_registers[1];
                    let site = eligibility
                        .element_transitions
                        .sites
                        .get(&instruction.pc)
                        .ok_or(Unsupported::OperandShape(
                            "optimizing element load missing site",
                        ))?;
                    debug_assert_eq!(site.safepoint_id, site.frame_map.id);
                    emit_materialize_element_transition(
                        &mut ops,
                        reprs,
                        allocation,
                        instruction,
                        site,
                    )?;
                    emit_load_u32(&mut ops, 9, instruction.pc);
                    dynasm!(ops
                        ; .arch aarch64
                        ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
                        ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
                        ; mov x0, x20
                        ; movz x1, dst as u32
                        ; movz x2, receiver as u32
                        ; movz x3, index as u32
                    );
                    emit_load_u64(&mut ops, 16, load_element_entry);
                    let succeeded = ops.new_dynamic_label();
                    dynasm!(ops
                        ; .arch aarch64
                        ; blr x16
                        ; cbz x0, =>succeeded
                        ; b =>threw
                        ; =>succeeded
                    );
                    emit_reload_element_transition(
                        &mut ops,
                        allocation,
                        site,
                        Some((
                            dst,
                            allocation.location(
                                instruction
                                    .result
                                    .expect("eligibility checked element-load result"),
                            ),
                        )),
                    )?;
                }
                Op::StoreElement => {
                    let receiver = instruction.input_registers[0];
                    let index = instruction.input_registers[1];
                    let value = instruction.input_registers[2];
                    // The bytecode compiler owns this spare interpreter-window
                    // register. The store stub may clobber it, so it is never a
                    // precise root and eligibility requires it not to alias one.
                    let scratch = instruction
                        .result_register
                        .expect("eligibility checked element-store scratch");
                    let site = eligibility
                        .element_transitions
                        .sites
                        .get(&instruction.pc)
                        .ok_or(Unsupported::OperandShape(
                            "optimizing element store missing site",
                        ))?;
                    debug_assert_eq!(site.safepoint_id, site.frame_map.id);
                    emit_materialize_element_transition(
                        &mut ops,
                        reprs,
                        allocation,
                        instruction,
                        site,
                    )?;
                    emit_load_u32(&mut ops, 9, instruction.pc);
                    dynasm!(ops
                        ; .arch aarch64
                        ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
                        ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
                        ; mov x0, x20
                        ; movz x1, receiver as u32
                        ; movz x2, index as u32
                        ; movz x3, value as u32
                        ; movz x4, scratch as u32
                    );
                    emit_load_u64(&mut ops, 16, store_element_entry);
                    let succeeded = ops.new_dynamic_label();
                    dynasm!(ops
                        ; .arch aarch64
                        ; blr x16
                        ; cbz x0, =>succeeded
                        ; b =>threw
                        ; =>succeeded
                    );
                    emit_reload_element_transition(&mut ops, allocation, site, None)?;
                }
                Op::LoadProperty => {
                    let dst = instruction
                        .result_register
                        .expect("eligibility checked property-load destination");
                    let object = instruction.input_registers[0];
                    let name = view.instructions[instruction.pc as usize]
                        .const_index(view.code_block.as_ref(), 2)
                        .ok_or(Unsupported::OperandShape("property-load name constant"))?;
                    let site = eligibility
                        .element_transitions
                        .sites
                        .get(&instruction.pc)
                        .ok_or(Unsupported::OperandShape(
                            "optimizing property load missing site",
                        ))?;
                    debug_assert_eq!(site.safepoint_id, site.frame_map.id);
                    emit_materialize_element_transition(
                        &mut ops,
                        reprs,
                        allocation,
                        instruction,
                        site,
                    )?;
                    emit_load_u32(&mut ops, 9, instruction.pc);
                    dynasm!(ops
                        ; .arch aarch64
                        ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
                        ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
                        ; mov x0, x20
                        ; movz x1, dst as u32
                        ; movz x2, object as u32
                    );
                    // The optimizing tier keeps no per-site property IC: a site
                    // index past the IC table (`u64::MAX`) and a null cell make the
                    // stub run the full `[[Get]]` resolution without touching a
                    // cache. `function_id` names the constant pool for the name.
                    emit_load_u64(&mut ops, 3, u64::from(name));
                    emit_load_u64(&mut ops, 4, u64::MAX);
                    emit_load_u64(&mut ops, 5, 0);
                    emit_load_u64(&mut ops, 6, function_id);
                    emit_load_u64(&mut ops, 16, load_property_entry);
                    let succeeded = ops.new_dynamic_label();
                    dynasm!(ops
                        ; .arch aarch64
                        ; blr x16
                        ; cbz x0, =>succeeded
                        ; b =>threw
                        ; =>succeeded
                    );
                    emit_reload_element_transition(
                        &mut ops,
                        allocation,
                        site,
                        Some((
                            dst,
                            allocation.location(
                                instruction
                                    .result
                                    .expect("eligibility checked property-load result"),
                            ),
                        )),
                    )?;
                }
                Op::StoreProperty => {
                    let object = instruction.input_registers[0];
                    let value = instruction.input_registers[1];
                    let name = view.instructions[instruction.pc as usize]
                        .const_index(view.code_block.as_ref(), 1)
                        .ok_or(Unsupported::OperandShape("property-store name constant"))?;
                    let site = eligibility
                        .element_transitions
                        .sites
                        .get(&instruction.pc)
                        .ok_or(Unsupported::OperandShape(
                            "optimizing property store missing site",
                        ))?;
                    debug_assert_eq!(site.safepoint_id, site.frame_map.id);
                    emit_materialize_element_transition(
                        &mut ops,
                        reprs,
                        allocation,
                        instruction,
                        site,
                    )?;
                    emit_load_u32(&mut ops, 9, instruction.pc);
                    dynasm!(ops
                        ; .arch aarch64
                        ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
                        ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
                        ; mov x0, x20
                        ; movz x1, object as u32
                    );
                    emit_load_u64(&mut ops, 2, u64::from(name));
                    dynasm!(ops ; .arch aarch64 ; movz x3, value as u32);
                    emit_load_u64(&mut ops, 4, u64::MAX);
                    emit_load_u64(&mut ops, 5, 0);
                    emit_load_u64(&mut ops, 6, function_id);
                    emit_load_u64(&mut ops, 16, store_property_entry);
                    let succeeded = ops.new_dynamic_label();
                    dynasm!(ops
                        ; .arch aarch64
                        ; blr x16
                        ; cbz x0, =>succeeded
                        ; b =>threw
                        ; =>succeeded
                    );
                    emit_reload_element_transition(&mut ops, allocation, site, None)?;
                }
                Op::Add | Op::Sub | Op::Mul | Op::Div => {
                    let result = instruction.result.expect("eligibility checked result");
                    match reprs.representation(result) {
                        Representation::Int32 => {
                            emit_load_int_operand(
                                &mut ops,
                                reprs,
                                allocation,
                                instruction,
                                0,
                                9,
                                guard_deopt,
                            )?;
                            emit_load_int_operand(
                                &mut ops,
                                reprs,
                                allocation,
                                instruction,
                                1,
                                10,
                                guard_deopt,
                            )?;
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
                                _ => unreachable!("int32 division is ineligible"),
                            }
                            emit_store_location(&mut ops, allocation.location(result), 11)?;
                            deopt_exits.push((
                                deopt,
                                byte_pc(view, instruction.pc)?,
                                instruction.pc,
                            ));
                        }
                        Representation::Float64 => {
                            emit_load_float_operand(
                                &mut ops,
                                reprs,
                                allocation,
                                instruction,
                                0,
                                FP_SCRATCH,
                                guard_deopt,
                            )?;
                            emit_load_float_operand(
                                &mut ops,
                                reprs,
                                allocation,
                                instruction,
                                1,
                                FP_SCRATCH_2,
                                guard_deopt,
                            )?;
                            match instruction.op {
                                Op::Add => dynasm!(ops
                                    ; .arch aarch64
                                    ; fadd D(FP_SCRATCH), D(FP_SCRATCH), D(FP_SCRATCH_2)
                                ),
                                Op::Sub => dynasm!(ops
                                    ; .arch aarch64
                                    ; fsub D(FP_SCRATCH), D(FP_SCRATCH), D(FP_SCRATCH_2)
                                ),
                                Op::Mul => dynasm!(ops
                                    ; .arch aarch64
                                    ; fmul D(FP_SCRATCH), D(FP_SCRATCH), D(FP_SCRATCH_2)
                                ),
                                Op::Div => dynasm!(ops
                                    ; .arch aarch64
                                    ; fdiv D(FP_SCRATCH), D(FP_SCRATCH), D(FP_SCRATCH_2)
                                ),
                                _ => unreachable!(),
                            }
                            emit_store_fp_location(
                                &mut ops,
                                allocation,
                                allocation.location(result),
                                FP_SCRATCH,
                            )?;
                        }
                        Representation::Tagged => unreachable!("eligibility checked arithmetic"),
                    }
                }
                Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Equal
                | Op::NotEqual => {
                    if view.feedback_at(instruction.pc).is_int32_only() {
                        emit_load_int_operand(
                            &mut ops,
                            reprs,
                            allocation,
                            instruction,
                            0,
                            9,
                            guard_deopt,
                        )?;
                        emit_load_int_operand(
                            &mut ops,
                            reprs,
                            allocation,
                            instruction,
                            1,
                            10,
                            guard_deopt,
                        )?;
                        emit_int_comparison(&mut ops, instruction.op);
                    } else {
                        emit_load_float_operand(
                            &mut ops,
                            reprs,
                            allocation,
                            instruction,
                            0,
                            FP_SCRATCH,
                            guard_deopt,
                        )?;
                        emit_load_float_operand(
                            &mut ops,
                            reprs,
                            allocation,
                            instruction,
                            1,
                            FP_SCRATCH_2,
                            guard_deopt,
                        )?;
                        emit_float_comparison(&mut ops, instruction.op);
                    }
                    emit_store_tagged_location(
                        &mut ops,
                        allocation
                            .location(instruction.result.expect("eligibility checked result")),
                        11,
                    )?;
                }
                Op::Jump => {
                    let target = block.normal_succs[0];
                    emit_cfg_edge(
                        &mut ops,
                        allocation,
                        eligibility,
                        &mut deopt_exits,
                        &block_labels,
                        block_id,
                        target,
                    )?;
                }
                Op::JumpIfTrue | Op::JumpIfFalse => {
                    let Terminator::Branch { taken, fallthrough } = block.terminator else {
                        unreachable!("eligibility checked branch terminator");
                    };
                    emit_load_tagged_location(
                        &mut ops,
                        allocation.location(instruction.inputs[0]),
                        9,
                    )?;
                    emit_load_u32(&mut ops, 10, VALUE_TRUE as u32);
                    let taken_edge = ops.new_dynamic_label();
                    if instruction.op == Op::JumpIfTrue {
                        dynasm!(ops ; .arch aarch64 ; cmp x9, x10 ; b.eq =>taken_edge);
                    } else {
                        dynasm!(ops ; .arch aarch64 ; cmp x9, x10 ; b.ne =>taken_edge);
                    }

                    emit_cfg_edge(
                        &mut ops,
                        allocation,
                        eligibility,
                        &mut deopt_exits,
                        &block_labels,
                        block_id,
                        fallthrough,
                    )?;
                    dynasm!(ops ; .arch aarch64 ; =>taken_edge);

                    emit_cfg_edge(
                        &mut ops,
                        allocation,
                        eligibility,
                        &mut deopt_exits,
                        &block_labels,
                        block_id,
                        taken,
                    )?;
                }
                Op::Return | Op::ReturnValue => {
                    let returned = instruction.inputs[0];
                    match reprs.representation(returned) {
                        Representation::Int32 => {
                            emit_load_location(&mut ops, allocation.location(returned), 9)?;
                            emit_box_int32(&mut ops, 9, 10);
                        }
                        Representation::Float64 => {
                            emit_load_fp_location(
                                &mut ops,
                                allocation,
                                allocation.location(returned),
                                FP_SCRATCH,
                            )?;
                            emit_box_double(&mut ops, FP_SCRATCH, 9);
                        }
                        Representation::Tagged => {
                            emit_load_tagged_location(&mut ops, allocation.location(returned), 9)?;
                        }
                    }
                    dynasm!(ops
                        ; .arch aarch64
                        ; mov x0, x9
                        ; movz x1, STATUS_RETURNED as u32
                    );
                    emit_epilogue(&mut ops, spill_frame_bytes);
                }
                _ => unreachable!("eligibility rejected unsupported opcode"),
            }
        }

        if block.terminator == Terminator::FallThrough {
            let target = block.normal_succs[0];
            emit_cfg_edge(
                &mut ops,
                allocation,
                eligibility,
                &mut deopt_exits,
                &block_labels,
                block_id,
                target,
            )?;
        }
    }

    dynasm!(ops
        ; .arch aarch64
        ; =>threw
        ; mov x0, xzr
        ; movz x1, STATUS_THREW as u32
    );
    emit_epilogue(&mut ops, spill_frame_bytes);

    for (label, deopt_byte_pc, resume_pc) in deopt_exits {
        dynasm!(ops ; .arch aarch64 ; =>label);
        let frame_state = deopt_table
            .lookup(deopt_byte_pc)
            .ok_or(Unsupported::OperandShape(
                "optimizing deopt exit missing frame state",
            ))?;
        emit_deopt_writeback(&mut ops, allocation, frame_state)?;
        emit_load_u32(&mut ops, 9, resume_pc);
        dynasm!(ops
            ; .arch aarch64
            ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
            ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
            ; mov x0, xzr
            ; movz x1, STATUS_BAILED as u32
        );
        emit_epilogue(&mut ops, spill_frame_bytes);
    }

    let mut osr_entries = BTreeMap::new();
    for (&block, site) in &eligibility.osr_entries {
        let target = block_labels[block.0 as usize];
        let offset = ops.offset().0;
        let representation_bail = ops.new_dynamic_label();
        emit_prologue(&mut ops, spill_frame_bytes);
        dynasm!(ops
            ; .arch aarch64
            ; mov x20, x0
            ; ldr x19, [x20]
        );
        emit_osr_materialization(&mut ops, reprs, allocation, site, representation_bail)?;
        dynasm!(ops ; .arch aarch64 ; b =>target ; =>representation_bail);
        emit_load_u32(&mut ops, 9, site.logical_pc);
        dynasm!(ops
            ; .arch aarch64
            ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
            ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
            ; mov x0, xzr
            ; movz x1, STATUS_BAILED as u32
        );
        emit_epilogue(&mut ops, spill_frame_bytes);
        osr_entries.insert(site.logical_pc, offset);
    }

    let buffer = ops
        .finalize()
        .expect("arm64 optimizing assembler finalization");
    Ok(OptimizedEmission {
        code: CompiledCode::new(buffer, entry),
        osr_entries,
    })
}

/// Emit one normal edge. Loop-carried phi destinations are populated before
/// the poll because the poll's deopt state is the target header's entry state.
fn emit_cfg_edge(
    ops: &mut Assembler,
    allocation: &Allocation,
    eligibility: &Eligibility,
    deopt_exits: &mut Vec<(DynamicLabel, u32, u32)>,
    block_labels: &[DynamicLabel],
    predecessor: BlockId,
    target: BlockId,
) -> Result<(), Unsupported> {
    emit_edge_moves(
        ops,
        allocation,
        edge_moves(allocation, predecessor, target)?,
    )?;
    if let Some(&(header_byte_pc, header_pc)) = eligibility.back_edges.get(&(predecessor, target)) {
        let deopt = ops.new_dynamic_label();
        emit_backedge_poll(ops, deopt);
        deopt_exits.push((deopt, header_byte_pc, header_pc));
    }
    let target_label = block_labels[target.0 as usize];
    dynasm!(ops ; .arch aarch64 ; b =>target_label);
    Ok(())
}

/// Poll every optimized back-edge without re-entering the VM.
///
/// The policy mirrors the baseline read/decrement pattern: the interrupt byte
/// is read first, then the shared fuel is decremented and stored (clamped at
/// zero so repeated entries cannot underflow it). An interrupt or non-positive
/// fuel value bails at the loop header. The interpreter then
/// resumes with the shared fuel still exhausted and performs its ordinary
/// interrupt/budget checkpoint on its next back-edge. Thus optimized code
/// bails at least as often as the interpreter/baseline poll cadence, never
/// less often, while keeping all refill and error policy inside the VM.
fn emit_backedge_poll(ops: &mut Assembler, deopt: DynamicLabel) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr x9, [x20, THREAD_OFFSET]
        ; ldr x9, [x9, VM_THREAD_INTERRUPT_CELL_OFFSET]
        ; ldrb w10, [x9]
        ; cbnz w10, =>deopt
        ; ldr x9, [x20, THREAD_OFFSET]
        ; ldr x9, [x9, VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET]
        ; ldr x10, [x9]
        ; subs x10, x10, #1
        ; csel x10, x10, xzr, gt
        ; str x10, [x9]
        ; b.le =>deopt
    );
}

fn emit_int_comparison(ops: &mut Assembler, op: Op) {
    dynasm!(ops ; .arch aarch64 ; cmp w9, w10);
    match op {
        Op::LessThan => dynasm!(ops ; .arch aarch64 ; cset w11, lt),
        Op::LessEq => dynasm!(ops ; .arch aarch64 ; cset w11, le),
        Op::GreaterThan => dynasm!(ops ; .arch aarch64 ; cset w11, gt),
        Op::GreaterEq => dynasm!(ops ; .arch aarch64 ; cset w11, ge),
        Op::Equal => dynasm!(ops ; .arch aarch64 ; cset w11, eq),
        Op::NotEqual => dynasm!(ops ; .arch aarch64 ; cset w11, ne),
        _ => unreachable!("eligibility checked comparison"),
    }
    dynasm!(ops
        ; .arch aarch64
        ; movz w12, VALUE_FALSE_LOW
        ; add w11, w11, w12
    );
}

/// Materialize a tagged boolean for an IEEE-754 comparison. AArch64's
/// unordered `fcmp` flags make `mi`/`ls` the relational conditions that stay
/// false for NaN; `eq` is likewise false, while `ne` is true as JavaScript
/// requires for numeric inequality.
fn emit_float_comparison(ops: &mut Assembler, op: Op) {
    dynasm!(ops ; .arch aarch64 ; fcmp D(FP_SCRATCH), D(FP_SCRATCH_2));
    match op {
        Op::LessThan => dynasm!(ops ; .arch aarch64 ; cset w11, mi),
        Op::LessEq => dynasm!(ops ; .arch aarch64 ; cset w11, ls),
        Op::GreaterThan => dynasm!(ops ; .arch aarch64 ; cset w11, gt),
        Op::GreaterEq => dynasm!(ops ; .arch aarch64 ; cset w11, ge),
        Op::Equal => dynasm!(ops ; .arch aarch64 ; cset w11, eq),
        Op::NotEqual => dynasm!(ops ; .arch aarch64 ; cset w11, ne),
        _ => unreachable!("eligibility checked comparison"),
    }
    dynasm!(ops
        ; .arch aarch64
        ; movz w12, VALUE_FALSE_LOW
        ; add w11, w11, w12
    );
}

fn edge_moves(
    allocation: &Allocation,
    predecessor: BlockId,
    block: BlockId,
) -> Result<&EdgeMoves, Unsupported> {
    allocation
        .edge_moves
        .iter()
        .find(|edge| edge.predecessor == predecessor && edge.block == block)
        .ok_or(Unsupported::OperandShape(
            "optimizing edge is missing phi moves",
        ))
}

fn emit_edge_moves(
    ops: &mut Assembler,
    allocation: &Allocation,
    edge: &EdgeMoves,
) -> Result<(), Unsupported> {
    for &movement in &edge.moves {
        emit_move(ops, allocation, movement)?;
    }
    Ok(())
}

fn emit_move(
    ops: &mut Assembler,
    allocation: &Allocation,
    movement: Move,
) -> Result<(), Unsupported> {
    match (movement.src, movement.dst) {
        (Location::Register(RegClass::Gpr, src), Location::Register(RegClass::Gpr, dst)) => {
            let src = gpr_move_register(src)?;
            let dst = gpr_move_register(dst)?;
            dynasm!(ops ; .arch aarch64 ; mov X(dst), X(src));
        }
        (Location::Register(RegClass::Gpr, src), Location::Spill(RegClass::Gpr, dst)) => {
            let src = gpr_move_register(src)?;
            let offset = spill_offset(dst)?;
            dynasm!(ops ; .arch aarch64 ; str X(src), [sp, offset]);
        }
        (Location::Spill(RegClass::Gpr, src), Location::Register(RegClass::Gpr, dst)) => {
            let dst = gpr_move_register(dst)?;
            let offset = spill_offset(src)?;
            dynasm!(ops ; .arch aarch64 ; ldr X(dst), [sp, offset]);
        }
        (Location::Spill(RegClass::Gpr, src), Location::Spill(RegClass::Gpr, dst)) => {
            let src_offset = spill_offset(src)?;
            let dst_offset = spill_offset(dst)?;
            dynasm!(ops
                ; .arch aarch64
                ; ldr x9, [sp, src_offset]
                ; str x9, [sp, dst_offset]
            );
        }
        (Location::Register(RegClass::Fp, src), Location::Register(RegClass::Fp, dst)) => {
            let src = fp_move_register(src)?;
            let dst = fp_move_register(dst)?;
            dynasm!(ops ; .arch aarch64 ; fmov D(dst), D(src));
        }
        (Location::Register(RegClass::Fp, src), Location::Spill(RegClass::Fp, dst)) => {
            let src = fp_move_register(src)?;
            let offset = fp_spill_offset(allocation, dst)?;
            dynasm!(ops ; .arch aarch64 ; str D(src), [sp, offset]);
        }
        (Location::Spill(RegClass::Fp, src), Location::Register(RegClass::Fp, dst)) => {
            let dst = fp_move_register(dst)?;
            let offset = fp_spill_offset(allocation, src)?;
            dynasm!(ops ; .arch aarch64 ; ldr D(dst), [sp, offset]);
        }
        (Location::Spill(RegClass::Fp, src), Location::Spill(RegClass::Fp, dst)) => {
            let src_offset = fp_spill_offset(allocation, src)?;
            let dst_offset = fp_spill_offset(allocation, dst)?;
            dynasm!(ops
                ; .arch aarch64
                ; ldr D(FP_SCRATCH), [sp, src_offset]
                ; str D(FP_SCRATCH), [sp, dst_offset]
            );
        }
        _ => return Err(Unsupported::OperandShape("optimizing cross-class phi move")),
    }
    Ok(())
}

/// Give pruned, structurally dead phis representation-valid homes. They emit
/// no predecessor copies, but exact deopt writeback may still name their
/// compiler-scratch register slots before later bytecode overwrites them.
fn emit_initialize_dead_phis(
    ops: &mut Assembler,
    ssa: &SsaFunction,
    reprs: &ReprMap,
    allocation: &Allocation,
    phis: &[ValueId],
) -> Result<(), Unsupported> {
    for &phi in phis {
        if !is_dead_phi(ssa, phi) {
            continue;
        }
        match reprs.representation(phi) {
            Representation::Tagged => {
                emit_load_u32(ops, 9, otter_vm::Value::undefined().to_bits() as u32);
                emit_store_tagged_location(ops, allocation.location(phi), 9)?;
            }
            Representation::Int32 => {
                emit_load_u32(ops, 9, 0);
                emit_store_location(ops, allocation.location(phi), 9)?;
            }
            Representation::Float64 => {
                emit_load_u64(ops, 9, 0.0_f64.to_bits());
                dynasm!(ops ; .arch aarch64 ; fmov D(FP_SCRATCH), x9);
                emit_store_fp_location(ops, allocation, allocation.location(phi), FP_SCRATCH)?;
            }
        }
    }
    Ok(())
}

fn gpr_move_register(register: u8) -> Result<u8, Unsupported> {
    if register == ALLOCATABLE_REGISTER_COUNT {
        Ok(12)
    } else {
        VALUE_REGISTERS
            .get(register as usize)
            .copied()
            .ok_or(Unsupported::OperandShape(
                "optimizing phi move register mapping",
            ))
    }
}

fn fp_move_register(register: u8) -> Result<u8, Unsupported> {
    if register == REGISTER_BUDGET.fp {
        Ok(FP_SCRATCH)
    } else {
        FP_REGISTERS
            .get(register as usize)
            .copied()
            .ok_or(Unsupported::OperandShape(
                "optimizing FP phi move register mapping",
            ))
    }
}

fn emit_load_parameter(ops: &mut Assembler, index: u32, scratch: u8) {
    let offset = index * STACK_SLOT_BYTES;
    if offset <= MAX_PARAMETER_OFFSET {
        dynasm!(ops ; .arch aarch64 ; ldr X(scratch), [x19, offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; ldr X(scratch), [x19, x12]);
    }
}

/// Inverse of [`emit_deopt_writeback`] for a loop-header live set.
///
/// The interpreter window is still the canonical rooted state on entry. Each
/// live frame-state value is loaded from its bytecode register, checked and
/// unboxed according to the representation analysis, then written to the same
/// allocated location that deopt later reads. Until every value has been
/// materialized, a representation failure returns through `bail` without
/// writing any machine location back over the window.
fn emit_osr_materialization(
    ops: &mut Assembler,
    reprs: &ReprMap,
    allocation: &Allocation,
    site: &OsrEntrySite,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    for live in &site.live_values {
        emit_load_frame_register(ops, u32::from(live.register), 9)?;
        match reprs.representation(live.value) {
            Representation::Tagged => {
                emit_store_tagged_location(ops, allocation.location(live.value), 9)?;
            }
            Representation::Int32 => {
                emit_guard_int32(ops, 9, bail);
                emit_store_location(ops, allocation.location(live.value), 9)?;
            }
            Representation::Float64 => {
                emit_num_to_double(ops, 9, FP_SCRATCH, bail);
                emit_store_fp_location(
                    ops,
                    allocation,
                    allocation.location(live.value),
                    FP_SCRATCH,
                )?;
            }
        }
    }
    Ok(())
}

fn emit_deopt_writeback(
    ops: &mut Assembler,
    allocation: &Allocation,
    frame_state: &FrameState,
) -> Result<(), Unsupported> {
    for (register, slot) in frame_state.slots.iter().enumerate() {
        match slot.location {
            DeoptLocation::Register(machine_register) => match slot.repr {
                DeoptRepr::Int32 | DeoptRepr::Tagged => {
                    let physical = VALUE_REGISTERS
                        .get(machine_register as usize)
                        .copied()
                        .ok_or(Unsupported::OperandShape("optimizing deopt GPR mapping"))?;
                    if slot.repr == DeoptRepr::Int32 {
                        dynasm!(ops ; .arch aarch64 ; mov w9, W(physical));
                    } else {
                        dynasm!(ops ; .arch aarch64 ; mov x9, X(physical));
                    }
                }
                DeoptRepr::Float64 => {
                    let fp_index = machine_register
                        .checked_sub(u16::from(allocation.register_budget.gpr))
                        .ok_or(Unsupported::OperandShape(
                            "optimizing deopt FP register class",
                        ))?;
                    let physical = FP_REGISTERS
                        .get(fp_index as usize)
                        .copied()
                        .ok_or(Unsupported::OperandShape("optimizing deopt FP mapping"))?;
                    dynasm!(ops ; .arch aarch64 ; fmov D(FP_SCRATCH), D(physical));
                }
            },
            DeoptLocation::StackSlot(offset) => {
                let offset = u32::try_from(offset).map_err(|_| {
                    Unsupported::OperandShape("optimizing negative deopt spill offset")
                })?;
                match slot.repr {
                    DeoptRepr::Int32 => dynasm!(ops ; .arch aarch64 ; ldr w9, [sp, offset]),
                    DeoptRepr::Tagged => dynasm!(ops ; .arch aarch64 ; ldr x9, [sp, offset]),
                    DeoptRepr::Float64 => {
                        dynasm!(ops ; .arch aarch64 ; ldr D(FP_SCRATCH), [sp, offset]);
                    }
                }
            }
            DeoptLocation::Constant(_) if slot.repr == DeoptRepr::Float64 => {
                return Err(Unsupported::OperandShape(
                    "optimizing float64 deopt constant",
                ));
            }
            DeoptLocation::Constant(_) => {
                emit_load_u32(ops, 9, otter_vm::Value::undefined().to_bits() as u32);
            }
        }
        match slot.repr {
            DeoptRepr::Tagged => {}
            DeoptRepr::Int32 => dynasm!(ops
                ; .arch aarch64
                ; movz x10, NUMBER_TAG_HI16, lsl #48
                ; orr x9, x10, x9
            ),
            DeoptRepr::Float64 => emit_box_double(ops, FP_SCRATCH, 9),
        }
        emit_store_frame_register(ops, register as u32, 9)?;
    }
    Ok(())
}

/// Materialize only the transition operands and tagged values live across the
/// call. Optimizing spills are private and unscanned, so live tagged spills
/// take the same interpreter-window rooting path as tagged machine registers.
fn emit_materialize_element_transition(
    ops: &mut Assembler,
    reprs: &ReprMap,
    allocation: &Allocation,
    instruction: &SsaInstr,
    site: &ElementTransitionSite,
) -> Result<(), Unsupported> {
    let mut materialized_registers = BTreeSet::new();
    for (&value, &register) in instruction.inputs.iter().zip(&instruction.input_registers) {
        emit_materialize_frame_value(ops, reprs, allocation, value, register)?;
        materialized_registers.insert(register);
    }
    for live in &site.tagged_live_across {
        if !materialized_registers.insert(live.register) {
            continue;
        }
        emit_load_tagged_location(ops, allocation.location(live.value), 9)?;
        emit_store_frame_register(ops, u32::from(live.register), 9)?;
    }
    Ok(())
}

fn emit_materialize_frame_value(
    ops: &mut Assembler,
    reprs: &ReprMap,
    allocation: &Allocation,
    value: ValueId,
    register: u16,
) -> Result<(), Unsupported> {
    match reprs.representation(value) {
        Representation::Tagged => {
            emit_load_tagged_location(ops, allocation.location(value), 9)?;
        }
        Representation::Int32 => {
            emit_load_location(ops, allocation.location(value), 9)?;
            emit_box_int32(ops, 9, 10);
        }
        Representation::Float64 => {
            emit_load_fp_location(ops, allocation, allocation.location(value), FP_SCRATCH)?;
            emit_box_double(ops, FP_SCRATCH, 9);
        }
    }
    emit_store_frame_register(ops, u32::from(register), 9)
}

/// Reload every tagged value live across moving GC, then optionally load an
/// element-load result last. Numeric homes and indices stay untouched. Store
/// scratch slots are deliberately ignored because the runtime may clobber them.
fn emit_reload_element_transition(
    ops: &mut Assembler,
    allocation: &Allocation,
    site: &ElementTransitionSite,
    load_result: Option<(u16, Location)>,
) -> Result<(), Unsupported> {
    let mut reloaded = BTreeSet::new();
    for live in &site.tagged_live_across {
        if !reloaded.insert(live.value) {
            continue;
        }
        emit_load_frame_register(ops, u32::from(live.register), 9)?;
        emit_store_tagged_location(ops, allocation.location(live.value), 9)?;
    }
    if let Some((result_register, dst_location)) = load_result {
        emit_load_frame_register(ops, u32::from(result_register), 9)?;
        emit_store_tagged_location(ops, dst_location, 9)?;
    }
    Ok(())
}

fn emit_load_frame_register(
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
        dynasm!(ops ; .arch aarch64 ; ldr X(scratch), [x19, offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; ldr X(scratch), [x19, x12]);
    }
    Ok(())
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
        dynasm!(ops ; .arch aarch64 ; str X(scratch), [x19, offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; str X(scratch), [x19, x12]);
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

fn total_spill_slots(allocation: &Allocation) -> Result<u32, Unsupported> {
    allocation
        .spill_slot_counts
        .gpr
        .checked_add(allocation.spill_slot_counts.fp)
        .ok_or(Unsupported::OperandShape(
            "optimizing total spill slot overflow",
        ))
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

fn fp_spill_offset(allocation: &Allocation, slot: u32) -> Result<u32, Unsupported> {
    let unified = allocation
        .spill_slot_counts
        .gpr
        .checked_add(slot)
        .ok_or(Unsupported::OperandShape("optimizing FP spill offset"))?;
    spill_offset(unified)
}

fn conversion_kind_at(
    reprs: &ReprMap,
    instruction: &SsaInstr,
    operand_index: usize,
) -> Option<ConversionKind> {
    reprs
        .conversions()
        .iter()
        .find(|conversion| {
            conversion.at_pc == instruction.pc && conversion.operand_index == operand_index
        })
        .map(|conversion| conversion.kind)
}

/// Fast-path source-lowered numeric coercions without invoking user code.
/// Tagged non-numbers bail at the coercion's exact bytecode PC so the
/// interpreter performs the observable `ToPrimitive` / `ToNumeric` semantics.
fn emit_tagged_numeric_coercion(
    ops: &mut Assembler,
    reprs: &ReprMap,
    allocation: &Allocation,
    instruction: &SsaInstr,
    deopt: Option<DynamicLabel>,
) -> Result<(), Unsupported> {
    let input = instruction.inputs[0];
    match reprs.representation(input) {
        Representation::Tagged => {
            let deopt = deopt.ok_or(Unsupported::OperandShape(
                "optimizing numeric coercion missing deopt exit",
            ))?;
            emit_load_tagged_location(ops, allocation.location(input), 9)?;
            emit_guard_number(ops, 9, deopt);
        }
        Representation::Int32 => {
            emit_load_location(ops, allocation.location(input), 9)?;
            emit_box_int32(ops, 9, 10);
        }
        Representation::Float64 => {
            emit_load_fp_location(ops, allocation, allocation.location(input), FP_SCRATCH)?;
            emit_box_double(ops, FP_SCRATCH, 9);
        }
    }
    emit_store_tagged_location(
        ops,
        allocation.location(
            instruction
                .result
                .expect("eligibility checked numeric coercion result"),
        ),
        9,
    )
}

fn emit_load_int_operand(
    ops: &mut Assembler,
    reprs: &ReprMap,
    allocation: &Allocation,
    instruction: &SsaInstr,
    operand_index: usize,
    scratch: u8,
    deopt: Option<DynamicLabel>,
) -> Result<(), Unsupported> {
    let input = instruction.inputs[operand_index];
    match reprs.representation(input) {
        Representation::Int32 => emit_load_location(ops, allocation.location(input), scratch),
        Representation::Tagged
            if conversion_kind_at(reprs, instruction, operand_index)
                == Some(ConversionKind::CheckedTaggedToInt32) =>
        {
            let deopt = deopt.ok_or(Unsupported::OperandShape(
                "optimizing int32 guard missing deopt exit",
            ))?;
            emit_load_tagged_location(ops, allocation.location(input), scratch)?;
            emit_guard_int32(ops, scratch, deopt);
            Ok(())
        }
        Representation::Float64 | Representation::Tagged => Err(Unsupported::OperandShape(
            "optimizing int32 operand conversion",
        )),
    }
}

fn emit_load_float_operand(
    ops: &mut Assembler,
    reprs: &ReprMap,
    allocation: &Allocation,
    instruction: &SsaInstr,
    operand_index: usize,
    scratch: u8,
    deopt: Option<DynamicLabel>,
) -> Result<(), Unsupported> {
    let input = instruction.inputs[operand_index];
    match reprs.representation(input) {
        Representation::Float64 => {
            emit_load_fp_location(ops, allocation, allocation.location(input), scratch)
        }
        Representation::Int32
            if conversion_kind_at(reprs, instruction, operand_index)
                == Some(ConversionKind::Int32ToFloat64) =>
        {
            emit_load_location(ops, allocation.location(input), 9)?;
            dynasm!(ops ; .arch aarch64 ; scvtf D(scratch), w9);
            Ok(())
        }
        Representation::Tagged
            if conversion_kind_at(reprs, instruction, operand_index)
                == Some(ConversionKind::CheckedTaggedToFloat64) =>
        {
            let deopt = deopt.ok_or(Unsupported::OperandShape(
                "optimizing float64 guard missing deopt exit",
            ))?;
            emit_load_tagged_location(ops, allocation.location(input), 9)?;
            emit_num_to_double(ops, 9, scratch, deopt);
            Ok(())
        }
        Representation::Int32 | Representation::Tagged => Err(Unsupported::OperandShape(
            "optimizing float64 operand conversion",
        )),
    }
}

/// Exact frozen number-tag test used by the template tier.
fn emit_guard_int32(ops: &mut Assembler, register: u8, deopt: DynamicLabel) {
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; and x14, X(register), x15
        ; cmp x14, x15
        ; b.ne =>deopt
    );
}

/// Accept either frozen tagged-number encoding while rejecting all other
/// primitives and heap references before a source-level coercion can run.
fn emit_guard_number(ops: &mut Assembler, register: u8, deopt: DynamicLabel) {
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; and x14, X(register), x15
        ; cmp x14, x15
        ; b.eq =>done
        ; tst X(register), x15
        ; b.eq =>deopt
        ; =>done
    );
}

/// Decode an engine-tagged number exactly as the template tier does: all
/// number-tag bits select int32, some select a double, and none is non-number.
fn emit_num_to_double(ops: &mut Assembler, source: u8, destination: u8, deopt: DynamicLabel) {
    let non_int = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; and x14, X(source), x15
        ; cmp x14, x15
        ; b.ne =>non_int
        ; scvtf D(destination), W(source)
        ; b =>done
        ; =>non_int
        ; tst X(source), x15
        ; b.eq =>deopt
        ; movz x14, DOUBLE_OFFSET_HI16, lsl #48
        ; sub x14, X(source), x14
        ; fmov D(destination), x14
        ; =>done
    );
}

fn emit_load_fp_location(
    ops: &mut Assembler,
    allocation: &Allocation,
    location: Location,
    scratch: u8,
) -> Result<(), Unsupported> {
    match location {
        Location::Register(RegClass::Fp, register) => {
            let physical = FP_REGISTERS
                .get(register as usize)
                .copied()
                .ok_or(Unsupported::OperandShape("optimizing FP register mapping"))?;
            dynasm!(ops ; .arch aarch64 ; fmov D(scratch), D(physical));
        }
        Location::Spill(RegClass::Fp, slot) => {
            let offset = fp_spill_offset(allocation, slot)?;
            dynasm!(ops ; .arch aarch64 ; ldr D(scratch), [sp, offset]);
        }
        Location::Register(RegClass::Gpr, _) | Location::Spill(RegClass::Gpr, _) => {
            return Err(Unsupported::OperandShape("optimizing non-FP location"));
        }
    }
    Ok(())
}

fn emit_store_fp_location(
    ops: &mut Assembler,
    allocation: &Allocation,
    location: Location,
    scratch: u8,
) -> Result<(), Unsupported> {
    match location {
        Location::Register(RegClass::Fp, register) => {
            let physical = FP_REGISTERS
                .get(register as usize)
                .copied()
                .ok_or(Unsupported::OperandShape("optimizing FP register mapping"))?;
            dynasm!(ops ; .arch aarch64 ; fmov D(physical), D(scratch));
        }
        Location::Spill(RegClass::Fp, slot) => {
            let offset = fp_spill_offset(allocation, slot)?;
            dynasm!(ops ; .arch aarch64 ; str D(scratch), [sp, offset]);
        }
        Location::Register(RegClass::Gpr, _) | Location::Spill(RegClass::Gpr, _) => {
            return Err(Unsupported::OperandShape("optimizing non-FP location"));
        }
    }
    Ok(())
}

fn emit_load_location(
    ops: &mut Assembler,
    location: Location,
    scratch: u8,
) -> Result<(), Unsupported> {
    match location {
        Location::Register(RegClass::Gpr, register) => {
            let physical = VALUE_REGISTERS
                .get(register as usize)
                .copied()
                .ok_or(Unsupported::OperandShape("optimizing register mapping"))?;
            dynasm!(ops ; .arch aarch64 ; mov W(scratch), W(physical));
        }
        Location::Spill(RegClass::Gpr, slot) => {
            let offset = spill_offset(slot)?;
            dynasm!(ops ; .arch aarch64 ; ldr W(scratch), [sp, offset]);
        }
        Location::Register(RegClass::Fp, _) | Location::Spill(RegClass::Fp, _) => {
            return Err(Unsupported::OperandShape("optimizing FP location"));
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
        Location::Register(RegClass::Gpr, register) => {
            let physical = VALUE_REGISTERS
                .get(register as usize)
                .copied()
                .ok_or(Unsupported::OperandShape("optimizing register mapping"))?;
            dynasm!(ops ; .arch aarch64 ; mov W(physical), W(scratch));
        }
        Location::Spill(RegClass::Gpr, slot) => {
            let offset = spill_offset(slot)?;
            dynasm!(ops ; .arch aarch64 ; str W(scratch), [sp, offset]);
        }
        Location::Register(RegClass::Fp, _) | Location::Spill(RegClass::Fp, _) => {
            return Err(Unsupported::OperandShape("optimizing FP location"));
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
        Location::Register(RegClass::Gpr, register) => {
            let physical = VALUE_REGISTERS
                .get(register as usize)
                .copied()
                .ok_or(Unsupported::OperandShape("optimizing register mapping"))?;
            dynasm!(ops ; .arch aarch64 ; mov X(scratch), X(physical));
        }
        Location::Spill(RegClass::Gpr, slot) => {
            let offset = spill_offset(slot)?;
            dynasm!(ops ; .arch aarch64 ; ldr X(scratch), [sp, offset]);
        }
        Location::Register(RegClass::Fp, _) | Location::Spill(RegClass::Fp, _) => {
            return Err(Unsupported::OperandShape("optimizing FP location"));
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
        Location::Register(RegClass::Gpr, register) => {
            let physical = VALUE_REGISTERS
                .get(register as usize)
                .copied()
                .ok_or(Unsupported::OperandShape("optimizing register mapping"))?;
            dynasm!(ops ; .arch aarch64 ; mov X(physical), X(scratch));
        }
        Location::Spill(RegClass::Gpr, slot) => {
            let offset = spill_offset(slot)?;
            dynasm!(ops ; .arch aarch64 ; str X(scratch), [sp, offset]);
        }
        Location::Register(RegClass::Fp, _) | Location::Spill(RegClass::Fp, _) => {
            return Err(Unsupported::OperandShape("optimizing FP location"));
        }
    }
    Ok(())
}

fn emit_box_int32(ops: &mut Assembler, value: u8, scratch: u8) {
    dynasm!(ops
        ; .arch aarch64
        ; movz X(scratch), NUMBER_TAG_HI16, lsl #48
        ; orr X(value), X(value), X(scratch)
    );
}

/// Canonicalize NaN and add the VM's frozen JSC-style encode offset, matching
/// `template::arm64::values::emit_box_double` instruction for instruction.
fn emit_box_double(ops: &mut Assembler, source: u8, destination: u8) {
    let ready = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; fmov X(destination), D(source)
        ; fcmp D(source), D(source)
        ; b.vc =>ready
        ; movz X(destination), CANONICAL_NAN_HI16, lsl #48
        ; =>ready
        ; movz x14, DOUBLE_OFFSET_HI16, lsl #48
        ; add X(destination), X(destination), x14
    );
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

fn emit_load_u64(ops: &mut Assembler, register: u8, value: u64) {
    dynasm!(ops ; .arch aarch64 ; movz X(register), (value & 0xffff) as u32);
    if (value >> 16) & 0xffff != 0 {
        dynasm!(ops
            ; .arch aarch64
            ; movk X(register), ((value >> 16) & 0xffff) as u32, lsl #16
        );
    }
    if (value >> 32) & 0xffff != 0 {
        dynasm!(ops
            ; .arch aarch64
            ; movk X(register), ((value >> 32) & 0xffff) as u32, lsl #32
        );
    }
    if (value >> 48) & 0xffff != 0 {
        dynasm!(ops
            ; .arch aarch64
            ; movk X(register), ((value >> 48) & 0xffff) as u32, lsl #48
        );
    }
}

fn emit_prologue(ops: &mut Assembler, spill_frame_bytes: u32) {
    dynasm!(ops
        ; .arch aarch64
        ; stp x29, x30, [sp, #-16]!
        ; stp x19, x20, [sp, #-48]!
        ; stp x21, x22, [sp, #16]
        ; stp x23, x24, [sp, #32]
        ; stp d8, d9, [sp, #-64]!
        ; stp d10, d11, [sp, #16]
        ; stp d12, d13, [sp, #32]
        ; stp d14, d15, [sp, #48]
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
        ; ldp d14, d15, [sp, #48]
        ; ldp d12, d13, [sp, #32]
        ; ldp d10, d11, [sp, #16]
        ; ldp d8, d9, [sp], #64
        ; ldp x23, x24, [sp, #32]
        ; ldp x21, x22, [sp, #16]
        ; ldp x19, x20, [sp], #48
        ; ldp x29, x30, [sp], #16
        ; ret
    );
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use otter_vm::{
        JitFunctionCode,
        jit::JitTestInstruction,
        jit_feedback::{ARITH_FLOAT64, ARITH_INT32, ArithFeedback},
        native_abi::{NativeFrame, NativeFrameFlags, NativeFrameKind, VmFrameHeader, VmThread},
    };

    use super::*;
    use crate::entry::{JitCtx, JitEntry, JitRet, STATUS_BAILED, STATUS_RETURNED, STATUS_THREW};

    const STRIDE: u32 = 8;

    fn box_i32(value: i32) -> u64 {
        (0xfffe_u64 << 48) | u64::from(value as u32)
    }

    fn unbox_i32(value: u64) -> i32 {
        value as u32 as i32
    }

    fn box_f64(value: f64) -> u64 {
        otter_vm::Value::number_f64(value).to_bits()
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
                Op::Add
                    | Op::Sub
                    | Op::Mul
                    | Op::LessThan
                    | Op::LessEq
                    | Op::GreaterThan
                    | Op::GreaterEq
                    | Op::Equal
                    | Op::NotEqual
            ) {
                view.seed_arith_feedback_for_test(pc as u32, ArithFeedback::from_bits(ARITH_INT32));
            }
        }
        view
    }

    fn float_view(
        param_count: u16,
        register_count: u16,
        instructions: Vec<(Op, Vec<Operand>)>,
        float_feedback_pcs: &[u32],
        numbers: &[(u32, f64)],
    ) -> JitCompileSnapshot {
        let mut view = view(param_count, register_count, instructions);
        for &pc in float_feedback_pcs {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_FLOAT64));
        }
        for &(pc, number) in numbers {
            view.instructions[pc as usize].load_number = Some(number);
        }
        view
    }

    fn execute(code: &OptimizedCode, args: &[u64]) -> JitRet {
        execute_with_frame(code, args).0
    }

    fn execute_with_frame(code: &OptimizedCode, args: &[u64]) -> (JitRet, Vec<u64>, u32) {
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        execute_with_poll_cells(code, args, std::ptr::addr_of!(interrupt), &mut fuel)
    }

    fn execute_with_poll_cells(
        code: &OptimizedCode,
        args: &[u64],
        interrupt: *const u8,
        fuel: &mut u64,
    ) -> (JitRet, Vec<u64>, u32) {
        // SAFETY: the compiler emitted `JitEntry`, `code` owns the mapping
        // through the call, and the context plus poll cells remain valid until
        // the entry returns.
        let entry = unsafe { code.compiled_code().entry_ptr() };
        let mut frame =
            vec![otter_vm::Value::undefined().to_bits(); code.metadata().register_count as usize];
        frame[..args.len()].copy_from_slice(args);
        execute_at(code, entry, frame, interrupt, fuel)
    }

    fn execute_osr_with_frame(
        code: &OptimizedCode,
        logical_pc: u32,
        frame: Vec<u64>,
    ) -> (JitRet, Vec<u64>, u32) {
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        // SAFETY: the code object owns the recorded trampoline while this call
        // executes.
        let entry = unsafe {
            code.osr_entry_ptr_for_test(logical_pc)
                .expect("optimized OSR entry")
        };
        execute_at(code, entry, frame, std::ptr::addr_of!(interrupt), &mut fuel)
    }

    fn execute_at(
        code: &OptimizedCode,
        entry: *const u8,
        mut frame: Vec<u64>,
        interrupt: *const u8,
        fuel: &mut u64,
    ) -> (JitRet, Vec<u64>, u32) {
        assert_eq!(frame.len(), code.metadata().register_count as usize);
        // SAFETY: `entry` is a main entry or OSR trampoline in `code`, whose
        // executable mapping outlives this call.
        let entry: JitEntry = unsafe { std::mem::transmute(entry) };
        let metadata = code.metadata();
        let mut native_frame = NativeFrame {
            header: VmFrameHeader {
                function_id: metadata.function_id,
                code_block_id: metadata.function_id,
                pc: 0,
                register_count: metadata.register_count,
                kind: NativeFrameKind::Baseline,
                flags: NativeFrameFlags::empty(),
            },
            previous_frame: 0,
            register_base: frame.as_mut_ptr() as u64,
            argument_base: 0,
            feedback_base: 0,
            code_object_id: metadata.code_object_id,
            this_value_bits: otter_vm::Value::undefined().to_bits(),
            new_target_bits: otter_vm::Value::undefined().to_bits(),
            return_register: u32::MAX,
            cold_state_index: u32::MAX,
            argument_count: 0,
            reserved0: 0,
            feedback_id: 0,
        };
        let mut thread = VmThread::empty();
        thread.current_frame = std::ptr::addr_of_mut!(native_frame) as u64;
        thread.interrupt_cell = interrupt as u64;
        thread.backedge_fuel_cell = std::ptr::from_mut(fuel) as u64;
        let mut error = None;
        let mut ctx = JitCtx {
            regs: frame.as_mut_ptr(),
            self_closure: otter_vm::Value::undefined().to_bits(),
            this_value: otter_vm::Value::undefined().to_bits(),
            thread: std::ptr::addr_of_mut!(thread),
            native_frame: std::ptr::addr_of_mut!(native_frame),
            frame_index: 0,
            upvalues_ptr: 0,
            error: &mut error,
            direct_entry_addr: 0,
            direct_regs: std::ptr::null_mut(),
            direct_self_closure: 0,
            direct_this_value: 0,
            direct_frame_index: 0,
            direct_upvalues_ptr: 0,
            direct_frame_ids: 0,
            direct_frame_meta: 0,
            direct_code_object_id: 0,
            reg_stack_base: std::ptr::null_mut(),
            reg_top_ptr: std::ptr::null_mut(),
        };
        let result = entry(&mut ctx);
        (result, frame, native_frame.header.pc)
    }

    fn summation_view() -> JitCompileSnapshot {
        view(
            1,
            5,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(4),
                        Operand::Register(1),
                        Operand::Register(0),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(3), Operand::Register(4)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(2),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(1),
                        Operand::Register(1),
                        Operand::Register(3),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(-5)]),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        )
    }

    extern "C" fn relocating_element_load(
        ctx: *mut JitCtx,
        dst: u64,
        receiver: u64,
        _index: u64,
    ) -> u64 {
        // SAFETY: the execution fixture supplies a live three-or-more-slot
        // register window for the duration of this transition call.
        let regs = unsafe { (*ctx).regs };
        unsafe {
            *regs.add(receiver as usize) = box_i32(5);
            *regs.add(dst as usize) = box_i32(37);
        }
        0
    }

    extern "C" fn relocating_precise_element_load(
        ctx: *mut JitCtx,
        dst: u64,
        receiver: u64,
        index: u64,
    ) -> u64 {
        // SAFETY: this fixture compiles a twelve-slot frame and keeps it live
        // for the transition. Tagged slots model moving-GC rewrites; numeric
        // slots are deliberately poisoned to prove they are never reloaded.
        let regs = unsafe { (*ctx).regs };
        unsafe {
            assert_eq!(*regs.add(receiver as usize), box_i32(99));
            assert_eq!(*regs.add(index as usize), box_i32(0));
            assert_eq!(*regs.add(1), box_i32(20));
            assert_eq!(*regs.add(2), box_i32(30));
            assert_eq!(*regs.add(4), otter_vm::Value::undefined().to_bits());
            assert_eq!(*regs.add(5), otter_vm::Value::undefined().to_bits());
            *regs.add(0) = box_i32(5);
            *regs.add(1) = box_i32(7);
            *regs.add(2) = box_i32(11);
            *regs.add(3) = box_i32(1_000);
            *regs.add(4) = box_i32(1_000);
            *regs.add(5) = box_f64(1_000.0);
            *regs.add(dst as usize) = box_i32(37);
        }
        0
    }

    extern "C" fn throwing_element_load(
        ctx: *mut JitCtx,
        _dst: u64,
        _receiver: u64,
        _index: u64,
    ) -> u64 {
        // SAFETY: the execution fixture owns the live error slot for the
        // complete entry call, matching the production transition contract.
        unsafe {
            *(*ctx).error = Some(otter_vm::VmError::InvalidOperand);
        }
        1
    }

    extern "C" fn relocating_precise_element_store(
        ctx: *mut JitCtx,
        receiver: u64,
        index: u64,
        value: u64,
        scratch: u64,
    ) -> u64 {
        // SAFETY: this fixture owns a seven-slot interpreter window for the
        // transition. Slots 0..=2 model moving-GC rewrites; the numeric index
        // and non-root scratch are poisoned to prove optimized code ignores
        // their window contents after the call.
        let regs = unsafe { (*ctx).regs };
        unsafe {
            assert_eq!(*regs.add(receiver as usize), box_i32(99));
            assert_eq!(*regs.add(index as usize), box_i32(0));
            assert_eq!(*regs.add(value as usize), box_i32(20));
            assert_eq!(
                *regs.add(scratch as usize),
                otter_vm::Value::undefined().to_bits()
            );
            *regs.add(0) = box_i32(5);
            *regs.add(1) = box_i32(7);
            *regs.add(2) = box_i32(11);
            *regs.add(index as usize) = box_i32(1_000);
            *regs.add(scratch as usize) = box_i32(2_000);
        }
        0
    }

    extern "C" fn throwing_element_store(
        ctx: *mut JitCtx,
        _receiver: u64,
        _index: u64,
        _value: u64,
        _scratch: u64,
    ) -> u64 {
        // SAFETY: the execution fixture owns the live error slot for the
        // complete entry call, matching the production transition contract.
        unsafe {
            *(*ctx).error = Some(otter_vm::VmError::InvalidOperand);
        }
        1
    }

    fn element_load_transitions(entry: usize) -> TransitionTable {
        let mut transitions = TransitionTable::resolve();
        transitions.replace_variadic_entry_for_test(STUB_JIT_LOAD_ELEMENT, entry);
        transitions
    }

    fn element_store_transitions(entry: usize) -> TransitionTable {
        let mut transitions = TransitionTable::resolve();
        transitions.replace_variadic_entry_for_test(STUB_JIT_STORE_ELEMENT, entry);
        transitions
    }

    #[test]
    fn executes_element_load_and_reloads_relocated_tagged_values() {
        let view = view(
            2,
            4,
            vec![
                (
                    Op::LoadElement,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(3),
                        Operand::Register(2),
                        Operand::Register(0),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
        );
        let transitions = element_load_transitions(relocating_element_load as *const () as usize);
        let code = compile_with_transitions(&view, 109, &transitions)
            .expect("element load is optimizing-eligible");
        let result = execute(&code, &[box_i32(99), box_i32(0)]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(result.value), 42);

        assert_eq!(code.safepoint_count(), 1);
        assert_eq!(JitFunctionCode::metadata(&code).safepoint_count, 1);
        let record = code.safepoint_record(0).expect("element-load safepoint");
        assert_eq!(
            record.tagged_locations,
            vec![
                otter_vm::native_abi::TaggedLocation::frame_slot(0),
                otter_vm::native_abi::TaggedLocation::frame_slot(1),
            ]
        );
        let frame_map = code.frame_map(0).expect("precise element-load frame map");
        assert_eq!(frame_map.slot_count, 4);
        assert_eq!(frame_map.bitmap_word_count, 1);
        assert_eq!(code.frame_map_bitmap_words(), &[0b11]);
    }

    #[test]
    fn precise_element_load_reloads_tagged_spills_but_not_numeric_values() {
        let view = float_view(
            3,
            12,
            vec![
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(4), Operand::Imm32(4)]),
                (
                    Op::LoadNumber,
                    vec![Operand::Register(5), Operand::ConstIndex(0)],
                ),
                (
                    Op::LoadElement,
                    vec![
                        Operand::Register(6),
                        Operand::Register(0),
                        Operand::Register(3),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(7),
                        Operand::Register(6),
                        Operand::Register(0),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(8),
                        Operand::Register(7),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(9),
                        Operand::Register(8),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(10),
                        Operand::Register(9),
                        Operand::Register(4),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(11),
                        Operand::Register(10),
                        Operand::Register(5),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(11)]),
            ],
            &[8],
            &[(2, 2.5)],
        );
        let transitions =
            element_load_transitions(relocating_precise_element_load as *const () as usize);
        let code = compile_with_transitions(&view, 111, &transitions)
            .expect("precise element load is optimizing-eligible");
        assert!(
            code.metadata().linear_scan_spill_slot_count > 0,
            "tagged live-across fixture must exercise optimizing spills"
        );

        let result = execute(&code, &[box_i32(99), box_i32(20), box_i32(30)]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, box_f64(66.5));

        let record = code.safepoint_record(0).expect("precise safepoint");
        assert_eq!(
            record.tagged_locations,
            vec![
                otter_vm::native_abi::TaggedLocation::frame_slot(0),
                otter_vm::native_abi::TaggedLocation::frame_slot(1),
                otter_vm::native_abi::TaggedLocation::frame_slot(2),
            ]
        );
        assert_eq!(code.frame_map_bitmap_words(), &[0b111]);
    }

    #[test]
    fn element_load_nonzero_status_uses_shared_throw_exit() {
        let view = view(
            2,
            3,
            vec![
                (
                    Op::LoadElement,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let transitions = element_load_transitions(throwing_element_load as *const () as usize);
        let code = compile_with_transitions(&view, 110, &transitions)
            .expect("throwing element load is optimizing-eligible");
        let result = execute(&code, &[box_i32(99), box_i32(0)]);
        assert_eq!(result.status, STATUS_THREW);
        assert_eq!(result.value, 0);
    }

    #[test]
    fn element_store_reloads_tagged_roots_and_ignores_scratch() {
        let view = view(
            3,
            7,
            vec![
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(0)]),
                (
                    Op::StoreElement,
                    vec![
                        Operand::Register(0),
                        Operand::Register(3),
                        Operand::Register(1),
                        Operand::Register(4),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(5),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(6),
                        Operand::Register(5),
                        Operand::Register(2),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(6)]),
            ],
        );
        let transitions =
            element_store_transitions(relocating_precise_element_store as *const () as usize);
        let code = compile_with_transitions(&view, 112, &transitions)
            .expect("precise element store is optimizing-eligible");
        let result = execute(&code, &[box_i32(99), box_i32(20), box_i32(30)]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(result.value), 23);

        let record = code.safepoint_record(0).expect("element-store safepoint");
        assert_eq!(
            record.tagged_locations,
            vec![
                otter_vm::native_abi::TaggedLocation::frame_slot(0),
                otter_vm::native_abi::TaggedLocation::frame_slot(1),
                otter_vm::native_abi::TaggedLocation::frame_slot(2),
            ]
        );
        assert_eq!(code.frame_map_bitmap_words(), &[0b111]);
    }

    #[test]
    fn element_store_nonzero_status_uses_shared_throw_exit() {
        let view = view(
            1,
            4,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(7)]),
                (
                    Op::StoreElement,
                    vec![
                        Operand::Register(0),
                        Operand::Register(1),
                        Operand::Register(2),
                        Operand::Register(3),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let transitions = element_store_transitions(throwing_element_store as *const () as usize);
        let code = compile_with_transitions(&view, 113, &transitions)
            .expect("throwing element store is optimizing-eligible");
        let result = execute(&code, &[box_i32(99)]);
        assert_eq!(result.status, STATUS_THREW);
        assert_eq!(result.value, 0);
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
        assert_eq!(result.status, STATUS_RETURNED);
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
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(result.value), 23);
    }

    #[test]
    fn executes_float_add_mul_chain() {
        let view = float_view(
            3,
            5,
            vec![
                (
                    Op::Add,
                    vec![
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Mul,
                    vec![
                        Operand::Register(4),
                        Operand::Register(3),
                        Operand::Register(2),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(4)]),
            ],
            &[0, 1],
            &[],
        );
        let code = compile(&view, 101).expect("float add/mul chain is eligible");
        let result = execute(&code, &[box_f64(1.5), box_f64(2.0), box_f64(4.0)]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, box_f64(14.0));
    }

    #[test]
    fn executes_float_division_with_int32_widening() {
        let view = float_view(
            0,
            3,
            vec![
                (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(7)]),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(2)]),
                (
                    Op::Div,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
            &[2],
            &[],
        );
        let code = compile(&view, 102).expect("float division is eligible");
        let result = execute(&code, &[]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, box_f64(3.5));
    }

    #[test]
    fn executes_mixed_tagged_int_and_double_division() {
        let view = float_view(
            2,
            3,
            vec![
                (
                    Op::Div,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
            &[0],
            &[],
        );
        let code = compile(&view, 103).expect("mixed division is eligible");
        let result = execute(&code, &[box_i32(7), box_f64(2.0)]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, box_f64(3.5));
        assert_eq!(
            execute(&code, &[box_f64(1.0), box_f64(0.0)]).value,
            box_f64(f64::INFINITY)
        );
        assert_eq!(
            execute(&code, &[box_f64(-1.0), box_f64(f64::INFINITY)]).value,
            box_f64(-0.0)
        );
        assert_eq!(
            execute(&code, &[box_f64(0.0), box_f64(0.0)]).value,
            box_f64(f64::NAN)
        );
    }

    #[test]
    fn executes_float_compare_branch() {
        let view = float_view(
            2,
            4,
            vec![
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(2)],
                ),
                (
                    Op::LoadNumber,
                    vec![Operand::Register(3), Operand::ConstIndex(0)],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
                (
                    Op::LoadNumber,
                    vec![Operand::Register(3), Operand::ConstIndex(1)],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
            &[0],
            &[(2, 11.5), (4, 22.5)],
        );
        let code = compile(&view, 104).expect("float comparison branch is eligible");
        assert_eq!(
            execute(&code, &[box_f64(1.5), box_f64(2.5)]).value,
            box_f64(11.5)
        );
        assert_eq!(
            execute(&code, &[box_f64(3.5), box_f64(2.5)]).value,
            box_f64(22.5)
        );
    }

    #[test]
    fn executes_float_accumulation_loop_with_fp_phi_moves() {
        let view = float_view(
            1,
            7,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (
                    Op::LoadNumber,
                    vec![Operand::Register(2), Operand::ConstIndex(0)],
                ),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
                (
                    Op::LoadNumber,
                    vec![Operand::Register(4), Operand::ConstIndex(1)],
                ),
                (
                    Op::LoadNumber,
                    vec![Operand::Register(6), Operand::ConstIndex(2)],
                ),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(5),
                        Operand::Register(1),
                        Operand::Register(0),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(4), Operand::Register(5)],
                ),
                (
                    Op::Mul,
                    vec![
                        Operand::Register(6),
                        Operand::Register(1),
                        Operand::Register(4),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(2),
                        Operand::Register(6),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(1),
                        Operand::Register(1),
                        Operand::Register(3),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(-6)]),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
            &[7, 8],
            &[(1, -0.0), (3, 1.5), (4, -0.0)],
        );
        let code = compile(&view, 105).expect("float accumulation loop is eligible");
        let result = execute(&code, &[box_i32(5)]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, box_f64(15.0));
    }

    #[test]
    fn float_nan_relational_and_equality_compares_are_false() {
        let view = float_view(
            0,
            4,
            vec![
                (
                    Op::LoadNumber,
                    vec![Operand::Register(0), Operand::ConstIndex(0)],
                ),
                (
                    Op::LoadNumber,
                    vec![Operand::Register(1), Operand::ConstIndex(1)],
                ),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfTrue,
                    vec![Operand::Imm32(2), Operand::Register(2)],
                ),
                (
                    Op::Equal,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(2)],
                ),
                (
                    Op::LoadNumber,
                    vec![Operand::Register(3), Operand::ConstIndex(2)],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
                (
                    Op::LoadNumber,
                    vec![Operand::Register(3), Operand::ConstIndex(3)],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
            &[2, 4],
            &[(0, f64::NAN), (1, 1.0), (6, 11.5), (8, 22.5)],
        );
        let code = compile(&view, 106).expect("NaN comparison is eligible");
        let result = execute(&code, &[]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, box_f64(22.5));
    }

    #[test]
    fn non_number_float_guard_deopts_and_boxes_prior_fp_value() {
        let view = float_view(
            2,
            4,
            vec![
                (
                    Op::LoadNumber,
                    vec![Operand::Register(2), Operand::ConstIndex(0)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::Div,
                    vec![
                        Operand::Register(3),
                        Operand::Register(2),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
            &[1, 2],
            &[(0, 1.5)],
        );
        let code = compile(&view, 107).expect("float deopt fixture is eligible");
        let undefined = otter_vm::Value::undefined().to_bits();
        let (result, frame, resume_pc) = execute_with_frame(&code, &[box_f64(2.0), undefined]);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(result.value, 0);
        assert_eq!(resume_pc, 2);
        assert_eq!(frame[0], box_f64(2.0));
        assert_eq!(frame[1], undefined);
        assert_eq!(frame[2], box_f64(3.5));
    }

    #[test]
    fn executes_if_else_with_distinct_values() {
        let view = view(
            2,
            4,
            vec![
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(2)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(3), Operand::Imm32(11)],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(3), Operand::Imm32(22)],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
        );
        let code = compile(&view, 7).expect("if/else is eligible");

        let taken = execute(&code, &[box_i32(3), box_i32(8)]);
        assert_eq!(taken.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(taken.value), 11);

        let fallthrough = execute(&code, &[box_i32(9), box_i32(4)]);
        assert_eq!(fallthrough.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(fallthrough.value), 22);
    }

    #[test]
    fn executes_max_diamond_phi_in_both_orders() {
        let view = view(
            2,
            5,
            vec![
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
                (
                    Op::GreaterThan,
                    vec![
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(3)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(4),
                        Operand::Register(0),
                        Operand::Register(2),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(1)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(4),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(4)]),
            ],
        );
        let cfg = ControlFlowGraph::build(&view).expect("diamond CFG");
        let ssa = SsaFunction::build(&view, &cfg).expect("diamond SSA");
        let phi_block =
            ssa.blocks
                .iter()
                .find(|block| {
                    block.phis.iter().any(|value| {
                        matches!(ssa.values[value.0 as usize].def, ValueDef::Phi { .. })
                    })
                })
                .expect("diamond must contain a join phi")
                .id;
        let liveness = Liveness::compute(&ssa, &cfg);
        let reprs = ReprMap::compute(&view, &ssa);
        let allocation = Allocation::compute(&ssa, &cfg, &liveness, &reprs, REGISTER_BUDGET)
            .expect("diamond allocation");
        let incoming: Vec<_> = allocation
            .edge_moves
            .iter()
            .filter(|edge| edge.block == phi_block)
            .collect();
        assert_eq!(incoming.len(), 2);
        assert!(
            incoming.iter().any(|edge| !edge.moves.is_empty()),
            "fixture must execute a concrete phi edge move"
        );
        let code = compile(&view, 8).expect("max diamond is eligible");

        let left = execute(&code, &[box_i32(19), box_i32(7)]);
        assert_eq!(left.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(left.value), 19);

        let right = execute(&code, &[box_i32(-4), box_i32(12)]);
        assert_eq!(right.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(right.value), 12);
    }

    #[test]
    fn executes_nested_if() {
        let view = view(
            3,
            6,
            vec![
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(6), Operand::Register(3)],
                ),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(4),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(4)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(5), Operand::Imm32(11)],
                ),
                (Op::ReturnValue, vec![Operand::Register(5)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(5), Operand::Imm32(22)],
                ),
                (Op::ReturnValue, vec![Operand::Register(5)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(5), Operand::Imm32(33)],
                ),
                (Op::ReturnValue, vec![Operand::Register(5)]),
            ],
        );
        let code = compile(&view, 9).expect("nested if is eligible");

        assert_eq!(
            unbox_i32(execute(&code, &[box_i32(1), box_i32(2), box_i32(3)]).value),
            11
        );
        assert_eq!(
            unbox_i32(execute(&code, &[box_i32(1), box_i32(4), box_i32(3)]).value),
            22
        );
        assert_eq!(
            unbox_i32(execute(&code, &[box_i32(5), box_i32(2), box_i32(3)]).value),
            33
        );
    }

    #[test]
    fn non_entry_block_overflow_deopts() {
        let view = view(
            1,
            5,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (Op::LoadTrue, vec![Operand::Register(2)]),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(3), Operand::Register(2)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(3), Operand::Imm32(i32::MAX)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(4),
                        Operand::Register(3),
                        Operand::Register(0),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(4)]),
                (Op::LoadInt32, vec![Operand::Register(4), Operand::Imm32(7)]),
                (Op::ReturnValue, vec![Operand::Register(4)]),
            ],
        );
        let code = compile(&view, 10).expect("branch-local overflow is eligible");
        let (result, frame, resume_pc) = execute_with_frame(&code, &[box_i32(1)]);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(result.value, 0);
        assert_eq!(resume_pc, 4);
        assert_eq!(frame[0], box_i32(1));
        assert_eq!(frame[1], box_i32(0));
        assert_eq!(frame[2], VALUE_TRUE);
        assert_eq!(frame[3], box_i32(i32::MAX));
    }

    #[test]
    fn executes_strict_int32_equality_branch() {
        let view = view(
            2,
            4,
            vec![
                (
                    Op::Equal,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(2)],
                ),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
                (Op::ReturnValue, vec![Operand::Register(3)]),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(0)]),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
        );
        let code = compile(&view, 11).expect("strict equality branch is eligible");
        assert_eq!(
            unbox_i32(execute(&code, &[box_i32(6), box_i32(6)]).value),
            1
        );
        assert_eq!(
            unbox_i32(execute(&code, &[box_i32(6), box_i32(7)]).value),
            0
        );
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
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(result.value), 110);
    }

    #[test]
    fn executes_forced_fp_spills() {
        let mut instructions = Vec::new();
        let mut numbers = Vec::new();
        for register in 0_u16..9 {
            instructions.push((
                Op::LoadNumber,
                vec![
                    Operand::Register(register),
                    Operand::ConstIndex(register as u32),
                ],
            ));
            numbers.push((u32::from(register), f64::from(register) + 0.5));
        }
        let mut float_pcs = Vec::new();
        let mut accumulator = 0_u16;
        for right in 1_u16..9 {
            let destination = 8 + right;
            let pc = instructions.len() as u32;
            float_pcs.push(pc);
            instructions.push((
                Op::Add,
                vec![
                    Operand::Register(destination),
                    Operand::Register(accumulator),
                    Operand::Register(right),
                ],
            ));
            accumulator = destination;
        }
        instructions.push((Op::ReturnValue, vec![Operand::Register(accumulator)]));

        let view = float_view(0, 17, instructions, &float_pcs, &numbers);
        let code = compile(&view, 108).expect("FP spill expression is eligible");
        assert!(code.metadata().linear_scan_spill_slot_count > 0);
        assert!(code.metadata().spill_slot_count > 0);
        let result = execute(&code, &[]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, box_f64(40.5));
    }

    #[test]
    fn parameter_guard_bails_at_first_use_logical_pc() {
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
        let (result, frame, resume_pc) = execute_with_frame(&code, &[0, box_i32(9)]);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(result.value, 0);
        assert_eq!(resume_pc, 0);
        assert_eq!(
            frame,
            vec![0, box_i32(9), otter_vm::Value::undefined().to_bits()]
        );
    }

    #[test]
    fn int32_overflow_bails_at_arithmetic_logical_pc() {
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
        let (result, frame, resume_pc) =
            execute_with_frame(&code, &[box_i32(i32::MAX), box_i32(1)]);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(result.value, 0);
        assert_eq!(resume_pc, 0);
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
        let (result, frame, resume_pc) = execute_with_frame(&code, &[box_i32(5), undefined]);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(result.value, 0);
        assert_eq!(resume_pc, 2);
        assert_eq!(
            frame,
            vec![box_i32(5), undefined, box_i32(7), box_i32(12), undefined]
        );
    }

    #[test]
    fn refuses_unsupported_operation() {
        let view = view(
            1,
            2,
            vec![
                (
                    Op::TypeOf,
                    vec![Operand::Register(1), Operand::Register(0)],
                ),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        assert!(compile(&view, 6).is_err());
    }

    #[test]
    fn executes_summation_loop_with_header_phis() {
        let view = summation_view();
        let cfg = ControlFlowGraph::build(&view).expect("summation CFG");
        let header = cfg
            .blocks
            .iter()
            .find(|block| block.is_loop_header)
            .expect("summation loop header")
            .id;
        let ssa = SsaFunction::build(&view, &cfg).expect("summation SSA");
        assert!(ssa.blocks[header.0 as usize].phis.len() >= 2);
        let liveness = Liveness::compute(&ssa, &cfg);
        let reprs = ReprMap::compute(&view, &ssa);
        let allocation = Allocation::compute(&ssa, &cfg, &liveness, &reprs, REGISTER_BUDGET)
            .expect("summation allocation");
        assert!(
            allocation
                .edge_moves
                .iter()
                .any(|edge| edge.block == header && !edge.moves.is_empty()),
            "fixture must require concrete loop-header phi moves"
        );

        let code = compile(&view, 12).expect("summation loop is eligible");
        for (n, expected) in [(0, 0), (1, 0), (5, 10), (10, 45), (100, 4_950)] {
            let result = execute(&code, &[box_i32(n)]);
            assert_eq!(result.status, STATUS_RETURNED);
            assert_eq!(unbox_i32(result.value), expected, "n={n}");
        }
    }

    #[test]
    fn osr_materializes_live_header_phis_from_interpreter_window() {
        let code = compile(&summation_view(), 120).expect("summation loop is eligible");
        let frame = vec![box_i32(10), box_i32(4), box_i32(6), box_i32(1), VALUE_TRUE];
        let (result, _frame, _resume_pc) = execute_osr_with_frame(&code, 3, frame);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(result.value), 45);
    }

    #[test]
    fn osr_representation_mismatch_bails_with_window_untouched() {
        let code = compile(&summation_view(), 121).expect("summation loop is eligible");
        let frame = vec![
            box_i32(10),
            box_f64(4.5),
            box_i32(6),
            box_i32(1),
            VALUE_TRUE,
        ];
        let (result, after, resume_pc) = execute_osr_with_frame(&code, 3, frame.clone());
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(resume_pc, 3);
        assert_eq!(after, frame);
    }

    #[test]
    fn loop_overflow_deopts_with_reconstructed_mid_loop_frame() {
        let view = view(
            1,
            5,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(2), Operand::Imm32(i32::MAX - 2)],
                ),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(4),
                        Operand::Register(1),
                        Operand::Register(0),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(3), Operand::Register(4)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(2),
                        Operand::Register(3),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(1),
                        Operand::Register(1),
                        Operand::Register(3),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(-5)]),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let code = compile(&view, 13).expect("overflowing loop is eligible");
        let (result, frame, resume_pc) = execute_with_frame(&code, &[box_i32(5)]);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(result.value, 0);
        assert_eq!(resume_pc, 5);
        assert_eq!(frame[0], box_i32(5));
        assert_eq!(frame[1], box_i32(2));
        assert_eq!(frame[2], box_i32(i32::MAX));
        assert_eq!(frame[3], box_i32(1));
        assert_eq!(frame[4], VALUE_TRUE);
    }

    #[test]
    fn overflow_after_osr_reconstructs_current_loop_frame() {
        let view = view(
            1,
            5,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(2), Operand::Imm32(i32::MAX - 2)],
                ),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(4),
                        Operand::Register(1),
                        Operand::Register(0),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(3), Operand::Register(4)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(2),
                        Operand::Register(3),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(1),
                        Operand::Register(1),
                        Operand::Register(3),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(-5)]),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let code = compile(&view, 122).expect("overflowing loop is eligible");
        let frame = vec![
            box_i32(5),
            box_i32(1),
            box_i32(i32::MAX - 1),
            box_i32(1),
            VALUE_TRUE,
        ];
        let (result, frame, resume_pc) = execute_osr_with_frame(&code, 3, frame);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(resume_pc, 5);
        assert_eq!(frame[0], box_i32(5));
        assert_eq!(frame[1], box_i32(2));
        assert_eq!(frame[2], box_i32(i32::MAX));
        assert_eq!(frame[3], box_i32(1));
        assert_eq!(frame[4], VALUE_TRUE);
    }

    #[test]
    fn executes_nested_loops() {
        let view = view(
            1,
            4,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(3),
                        Operand::Register(1),
                        Operand::Register(0),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(9), Operand::Register(3)],
                ),
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(3),
                        Operand::Register(2),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(3), Operand::Register(3)],
                ),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(2),
                        Operand::Register(3),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(-5)]),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(1),
                        Operand::Register(1),
                        Operand::Register(3),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(-11)]),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        let code = compile(&view, 14).expect("nested loops are eligible");
        for n in [0, 1, 5, 20] {
            let result = execute(&code, &[box_i32(n)]);
            assert_eq!(result.status, STATUS_RETURNED);
            assert_eq!(unbox_i32(result.value), n, "n={n}");
        }
    }

    #[test]
    fn backedge_interrupt_deopts_near_infinite_loop_at_header() {
        let view = view(
            0,
            2,
            vec![
                (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(0),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(-2)]),
            ],
        );
        let code = compile(&view, 15).expect("reducible infinite loop is eligible");
        let interrupt = Arc::new(AtomicBool::new(false));
        let setter = Arc::clone(&interrupt);
        let interrupter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            setter.store(true, Ordering::Release);
        });
        let mut fuel = i64::MAX as u64;
        let (result, frame, resume_pc) =
            execute_with_poll_cells(&code, &[], Arc::as_ptr(&interrupt).cast::<u8>(), &mut fuel);
        interrupter.join().expect("interrupt setter");

        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(result.value, 0);
        assert_eq!(resume_pc, 2);
        assert_eq!(frame, vec![box_i32(0), box_i32(0)]);
    }

    #[test]
    fn exhausted_backedge_fuel_deopts_at_header_after_phi_moves() {
        let view = summation_view();
        let code = compile(&view, 16).expect("summation loop is eligible");
        let interrupt = 0_u8;
        let mut fuel = 1_u64;
        let (result, frame, resume_pc) = execute_with_poll_cells(
            &code,
            &[box_i32(5)],
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );

        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(result.value, 0);
        assert_eq!(resume_pc, 3);
        assert_eq!(fuel, 0);
        assert_eq!(frame[1], box_i32(1));
        assert_eq!(frame[2], box_i32(0));
    }

    #[test]
    fn refuses_irreducible_loop() {
        let view = view(
            0,
            1,
            vec![
                (Op::LoadTrue, vec![Operand::Register(0)]),
                (
                    Op::JumpIfTrue,
                    vec![Operand::Imm32(1), Operand::Register(0)],
                ),
                (Op::Jump, vec![Operand::Imm32(0)]),
                (Op::Jump, vec![Operand::Imm32(-2)]),
            ],
        );
        assert!(compile(&view, 17).is_err());
    }
}
