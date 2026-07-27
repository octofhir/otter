//! Low-level AArch64 emission helpers for the optimizing tier.
//!
//! # Contents
//! - Control-flow edges, comparisons and edge moves.
//! - Spill-slot addressing and register/location loads and stores.
//! - Representation conversions, boxing, constant materialization.
//! - Frame prologue and epilogue.
//!
//! # Invariants
//! - Nothing here decides *what* to emit; every function lowers one already
//!   chosen operation, so the dispatch in [`super`] owns all opcode policy.

use super::*;

/// Emit one normal edge. Loop-carried phi destinations are populated before
/// the poll because the poll's deopt state is the target header's entry state.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_cfg_edge(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    allocation: &Allocation,
    eligibility: &Eligibility,
    poll_entry: ResolvedRuntimeEntry,
    threw: DynamicLabel,
    block_labels: &[DynamicLabel],
    predecessor: BlockId,
    target: BlockId,
) -> Result<(), Unsupported> {
    emit_edge_moves(
        ops,
        allocation,
        edge_moves(allocation, predecessor, target)?,
    )?;
    let is_back_edge = eligibility.back_edges.contains_key(&(predecessor, target));
    if is_back_edge {
        emit_backedge_poll(ops, relocations, poll_entry, threw);
    }
    let target_label = block_labels[target.0 as usize];
    dynasm!(ops ; .arch aarch64 ; b =>target_label);
    Ok(())
}

/// Amortized cooperative back-edge poll.
///
/// A native activation-local countdown keeps the common back edge independent
/// of VM-thread memory. Every bounded batch, generated code checks the shared
/// interrupt cell and subtracts the whole batch from shared fuel before using
/// the canonical leaf refill/throw path. This matches the bounded safepoint
/// latency used by unrolled production JIT loops without changing total fuel.
pub(super) fn emit_backedge_poll(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    poll_entry: ResolvedRuntimeEntry,
    threw: DynamicLabel,
) {
    let batched = ops.new_dynamic_label();
    let slow = ops.new_dynamic_label();
    let cont = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; subs w29, w29, #1
        ; b.ne =>batched
        ; movz w29, OPTIMIZED_POLL_BATCH
    );
    dynasm!(ops
        ; .arch aarch64
        ; ldr x17, [x20, THREAD_OFFSET]
        ; ldr x9, [x17, VM_THREAD_INTERRUPT_CELL_OFFSET]
        ; ldrb w9, [x9]
        ; cbnz w9, =>slow
        ; ldr x9, [x17, VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET]
        ; ldr x10, [x9]
        ; subs x10, x10, OPTIMIZED_POLL_BATCH
        ; str x10, [x9]
        ; b.gt =>cont
        ; =>slow
        ; mov x0, x20
    );
    emit_runtime_entry(ops, relocations, 16, poll_entry);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbnz x0, =>threw
        ; =>cont
        ; =>batched
    );
}

pub(super) fn emit_int_compare_flags(ops: &mut Assembler) {
    dynasm!(ops ; .arch aarch64 ; cmp w9, w10);
}

pub(super) fn emit_int_comparison(ops: &mut Assembler, op: Op) {
    emit_int_compare_flags(ops);
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

pub(super) fn emit_int_comparison_branch(
    ops: &mut Assembler,
    op: Op,
    branch_on_true: bool,
    target: DynamicLabel,
) {
    match (op, branch_on_true) {
        (Op::LessThan, true) => dynasm!(ops ; .arch aarch64 ; b.lt =>target),
        (Op::LessThan, false) => dynasm!(ops ; .arch aarch64 ; b.ge =>target),
        (Op::LessEq, true) => dynasm!(ops ; .arch aarch64 ; b.le =>target),
        (Op::LessEq, false) => dynasm!(ops ; .arch aarch64 ; b.gt =>target),
        (Op::GreaterThan, true) => dynasm!(ops ; .arch aarch64 ; b.gt =>target),
        (Op::GreaterThan, false) => dynasm!(ops ; .arch aarch64 ; b.le =>target),
        (Op::GreaterEq, true) => dynasm!(ops ; .arch aarch64 ; b.ge =>target),
        (Op::GreaterEq, false) => dynasm!(ops ; .arch aarch64 ; b.lt =>target),
        (Op::Equal, true) => dynasm!(ops ; .arch aarch64 ; b.eq =>target),
        (Op::Equal, false) => dynasm!(ops ; .arch aarch64 ; b.ne =>target),
        (Op::NotEqual, true) => dynasm!(ops ; .arch aarch64 ; b.ne =>target),
        (Op::NotEqual, false) => dynasm!(ops ; .arch aarch64 ; b.eq =>target),
        _ => unreachable!("eligibility checked comparison branch"),
    }
}

/// Materialize a tagged boolean for an IEEE-754 comparison. AArch64's
/// unordered `fcmp` flags make `mi`/`ls` the relational conditions that stay
/// false for NaN; `eq` is likewise false, while `ne` is true as JavaScript
/// requires for numeric inequality.
pub(super) fn emit_float_compare_flags(ops: &mut Assembler) {
    dynasm!(ops ; .arch aarch64 ; fcmp D(FP_SCRATCH), D(FP_SCRATCH_2));
}

pub(super) fn emit_float_comparison(ops: &mut Assembler, op: Op) {
    emit_float_compare_flags(ops);
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

pub(super) fn emit_float_comparison_branch(
    ops: &mut Assembler,
    op: Op,
    branch_on_true: bool,
    target: DynamicLabel,
) {
    match (op, branch_on_true) {
        (Op::LessThan, true) => dynasm!(ops ; .arch aarch64 ; b.mi =>target),
        (Op::LessThan, false) => dynasm!(ops ; .arch aarch64 ; b.pl =>target),
        (Op::LessEq, true) => dynasm!(ops ; .arch aarch64 ; b.ls =>target),
        (Op::LessEq, false) => dynasm!(ops ; .arch aarch64 ; b.hi =>target),
        (Op::GreaterThan, true) => dynasm!(ops ; .arch aarch64 ; b.gt =>target),
        (Op::GreaterThan, false) => dynasm!(ops ; .arch aarch64 ; b.le =>target),
        (Op::GreaterEq, true) => dynasm!(ops ; .arch aarch64 ; b.ge =>target),
        (Op::GreaterEq, false) => dynasm!(ops ; .arch aarch64 ; b.lt =>target),
        (Op::Equal, true) => dynasm!(ops ; .arch aarch64 ; b.eq =>target),
        (Op::Equal, false) => dynasm!(ops ; .arch aarch64 ; b.ne =>target),
        (Op::NotEqual, true) => dynasm!(ops ; .arch aarch64 ; b.ne =>target),
        (Op::NotEqual, false) => dynasm!(ops ; .arch aarch64 ; b.eq =>target),
        _ => unreachable!("eligibility checked comparison branch"),
    }
}

pub(super) fn emit_store_boolean_constant(
    ops: &mut Assembler,
    location: Location,
    truthy: bool,
) -> Result<(), Unsupported> {
    if truthy {
        dynasm!(ops ; .arch aarch64 ; movz w11, VALUE_TRUE as u32);
    } else {
        dynasm!(ops ; .arch aarch64 ; movz w11, VALUE_FALSE as u32);
    }
    emit_store_tagged_location(ops, location, 11)
}

pub(super) fn edge_moves(
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

pub(super) fn emit_edge_moves(
    ops: &mut Assembler,
    allocation: &Allocation,
    edge: &EdgeMoves,
) -> Result<(), Unsupported> {
    for &movement in &edge.moves {
        emit_move(ops, allocation, movement)?;
    }
    Ok(())
}

pub(super) fn emit_move(
    ops: &mut Assembler,
    allocation: &Allocation,
    movement: Move,
) -> Result<(), Unsupported> {
    match movement.conversion {
        None => emit_raw_move(ops, allocation, movement.src, movement.dst),
        Some(ConversionKind::BoxInt32) => {
            emit_load_move_gpr(ops, movement.src, 9)?;
            emit_box_int32(ops, 9, 10);
            emit_store_move_gpr(ops, movement.dst, 9)
        }
        Some(ConversionKind::Int32ToFloat64) => {
            emit_load_move_gpr(ops, movement.src, 9)?;
            dynasm!(ops ; .arch aarch64 ; scvtf d17, w9);
            emit_store_move_fp(ops, allocation, movement.dst, FP_SCRATCH_2)
        }
        Some(ConversionKind::BoxFloat64) => {
            emit_load_move_fp(ops, allocation, movement.src, FP_SCRATCH_2)?;
            emit_box_double(ops, FP_SCRATCH_2, 9);
            emit_store_move_gpr(ops, movement.dst, 9)
        }
        Some(ConversionKind::CheckedTaggedToInt32 | ConversionKind::CheckedTaggedToFloat64) => Err(
            Unsupported::OperandShape("optimizing checked phi conversion"),
        ),
    }
}

pub(super) fn emit_raw_move(
    ops: &mut Assembler,
    allocation: &Allocation,
    src: Location,
    dst: Location,
) -> Result<(), Unsupported> {
    if src == dst {
        return Ok(());
    }
    match (src, dst) {
        (Location::Register(RegClass::Gpr, src), Location::Register(RegClass::Gpr, dst)) => {
            let src = gpr_move_register(src)?;
            let dst = gpr_move_register(dst)?;
            dynasm!(ops ; .arch aarch64 ; mov X(dst), X(src));
        }
        (Location::Register(RegClass::Gpr, src), Location::Spill(RegClass::Gpr, dst)) => {
            let src = gpr_move_register(src)?;
            let offset = spill_offset(dst)?;
            emit_sp_str_x(ops, src, offset);
        }
        (Location::Spill(RegClass::Gpr, src), Location::Register(RegClass::Gpr, dst)) => {
            let dst = gpr_move_register(dst)?;
            let offset = spill_offset(src)?;
            emit_sp_ldr_x(ops, dst, offset);
        }
        (Location::Spill(RegClass::Gpr, src), Location::Spill(RegClass::Gpr, dst)) => {
            let src_offset = spill_offset(src)?;
            let dst_offset = spill_offset(dst)?;
            emit_sp_ldr_x(ops, 9, src_offset);
            emit_sp_str_x(ops, 9, dst_offset);
        }
        (Location::Register(RegClass::Fp, src), Location::Register(RegClass::Fp, dst)) => {
            let src = fp_move_register(src)?;
            let dst = fp_move_register(dst)?;
            dynasm!(ops ; .arch aarch64 ; fmov D(dst), D(src));
        }
        (Location::Register(RegClass::Fp, src), Location::Spill(RegClass::Fp, dst)) => {
            let src = fp_move_register(src)?;
            let offset = fp_spill_offset(allocation, dst)?;
            emit_sp_str_d(ops, src, offset);
        }
        (Location::Spill(RegClass::Fp, src), Location::Register(RegClass::Fp, dst)) => {
            let dst = fp_move_register(dst)?;
            let offset = fp_spill_offset(allocation, src)?;
            emit_sp_ldr_d(ops, dst, offset);
        }
        (Location::Spill(RegClass::Fp, src), Location::Spill(RegClass::Fp, dst)) => {
            let src_offset = fp_spill_offset(allocation, src)?;
            let dst_offset = fp_spill_offset(allocation, dst)?;
            emit_sp_ldr_d(ops, FP_SCRATCH_2, src_offset);
            emit_sp_str_d(ops, FP_SCRATCH_2, dst_offset);
        }
        _ => return Err(Unsupported::OperandShape("optimizing cross-class phi move")),
    }
    Ok(())
}

pub(super) fn emit_load_move_gpr(
    ops: &mut Assembler,
    location: Location,
    scratch: u8,
) -> Result<(), Unsupported> {
    match location {
        Location::Register(RegClass::Gpr, register) => {
            let physical = gpr_move_register(register)?;
            dynasm!(ops ; .arch aarch64 ; mov X(scratch), X(physical));
        }
        Location::Spill(RegClass::Gpr, slot) => {
            let offset = spill_offset(slot)?;
            emit_sp_ldr_x(ops, scratch, offset);
        }
        Location::Register(RegClass::Fp, _) | Location::Spill(RegClass::Fp, _) => {
            return Err(Unsupported::OperandShape("optimizing non-GPR phi source"));
        }
    }
    Ok(())
}

pub(super) fn emit_store_move_gpr(
    ops: &mut Assembler,
    location: Location,
    scratch: u8,
) -> Result<(), Unsupported> {
    match location {
        Location::Register(RegClass::Gpr, register) => {
            let physical = gpr_move_register(register)?;
            dynasm!(ops ; .arch aarch64 ; mov X(physical), X(scratch));
        }
        Location::Spill(RegClass::Gpr, slot) => {
            let offset = spill_offset(slot)?;
            emit_sp_str_x(ops, scratch, offset);
        }
        Location::Register(RegClass::Fp, _) | Location::Spill(RegClass::Fp, _) => {
            return Err(Unsupported::OperandShape(
                "optimizing non-GPR phi destination",
            ));
        }
    }
    Ok(())
}

pub(super) fn emit_load_move_fp(
    ops: &mut Assembler,
    allocation: &Allocation,
    location: Location,
    scratch: u8,
) -> Result<(), Unsupported> {
    match location {
        Location::Register(RegClass::Fp, register) => {
            let physical = fp_move_register(register)?;
            dynasm!(ops ; .arch aarch64 ; fmov D(scratch), D(physical));
        }
        Location::Spill(RegClass::Fp, slot) => {
            let offset = fp_spill_offset(allocation, slot)?;
            emit_sp_ldr_d(ops, scratch, offset);
        }
        Location::Register(RegClass::Gpr, _) | Location::Spill(RegClass::Gpr, _) => {
            return Err(Unsupported::OperandShape("optimizing non-FP phi source"));
        }
    }
    Ok(())
}

pub(super) fn emit_store_move_fp(
    ops: &mut Assembler,
    allocation: &Allocation,
    location: Location,
    scratch: u8,
) -> Result<(), Unsupported> {
    match location {
        Location::Register(RegClass::Fp, register) => {
            let physical = fp_move_register(register)?;
            dynasm!(ops ; .arch aarch64 ; fmov D(physical), D(scratch));
        }
        Location::Spill(RegClass::Fp, slot) => {
            let offset = fp_spill_offset(allocation, slot)?;
            emit_sp_str_d(ops, scratch, offset);
        }
        Location::Register(RegClass::Gpr, _) | Location::Spill(RegClass::Gpr, _) => {
            return Err(Unsupported::OperandShape(
                "optimizing non-FP phi destination",
            ));
        }
    }
    Ok(())
}

pub(super) fn gpr_move_register(register: u8) -> Result<u8, Unsupported> {
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

pub(super) fn fp_move_register(register: u8) -> Result<u8, Unsupported> {
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

pub(super) fn emit_load_parameter(ops: &mut Assembler, index: u32, scratch: u8) {
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
pub(super) fn emit_osr_materialization(
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

/// Restore one interpreter frame's registers from an exit's slots.
///
/// `window` addresses the frame being rebuilt: `x19` for the compiled function's
/// own frame, or the window a reify handed back for an inlined callee's.
pub(super) fn emit_deopt_writeback(
    ops: &mut Assembler,
    allocation: &Allocation,
    frame: &DeoptFrame,
    window: u8,
) -> Result<(), Unsupported> {
    for (register, slot) in frame.slots.iter().enumerate() {
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
                    DeoptRepr::Int32 => emit_sp_ldr_w(ops, 9, offset),
                    DeoptRepr::Tagged => emit_sp_ldr_x(ops, 9, offset),
                    DeoptRepr::Float64 => {
                        emit_sp_ldr_d(ops, FP_SCRATCH, offset);
                    }
                }
            }
            DeoptLocation::Literal(raw) => {
                emit_load_u64(ops, 9, raw);
                if slot.repr == DeoptRepr::Float64 {
                    dynasm!(ops ; .arch aarch64 ; fmov D(FP_SCRATCH), x9);
                }
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
        emit_store_frame_register_in(ops, window, register as u32, 9)?;
    }
    Ok(())
}

/// Materialize only the transition operands and tagged values live across the
/// call. Optimizing spills are private and unscanned, so live tagged spills
/// take the same interpreter-window rooting path as tagged machine registers.
pub(super) fn emit_materialize_element_transition(
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

pub(super) fn emit_materialize_frame_value(
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

pub(super) fn emit_load_boxed_value(
    ops: &mut Assembler,
    reprs: &ReprMap,
    allocation: &Allocation,
    value: ValueId,
    scratch: u8,
) -> Result<(), Unsupported> {
    match reprs.representation(value) {
        Representation::Tagged => {
            emit_load_tagged_location(ops, allocation.location(value), scratch)
        }
        Representation::Int32 => {
            emit_load_location(ops, allocation.location(value), scratch)?;
            let tag_scratch = if scratch == 10 { 11 } else { 10 };
            emit_box_int32(ops, scratch, tag_scratch);
            Ok(())
        }
        Representation::Float64 => {
            emit_load_fp_location(ops, allocation, allocation.location(value), FP_SCRATCH)?;
            emit_box_double(ops, FP_SCRATCH, scratch);
            Ok(())
        }
    }
}

/// Reload every tagged value live across moving GC, then optionally load an
/// element-load result last. Numeric homes and indices stay untouched.
pub(super) fn emit_reload_element_transition(
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

pub(super) fn emit_load_frame_register(
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

pub(super) fn emit_store_frame_register(
    ops: &mut Assembler,
    register: u32,
    scratch: u8,
) -> Result<(), Unsupported> {
    emit_store_frame_register_in(ops, 19, register, scratch)
}

/// Store into an interpreter register window addressed by `window`.
///
/// The compiled frame's own window is `x19`; a frame reified for an inlined
/// callee lives in the window its reify handed back, and the callee's registers
/// belong there and not in its caller's.
pub(super) fn emit_store_frame_register_in(
    ops: &mut Assembler,
    window: u8,
    register: u32,
    scratch: u8,
) -> Result<(), Unsupported> {
    let offset = register
        .checked_mul(STACK_SLOT_BYTES)
        .ok_or(Unsupported::OperandShape(
            "optimizing frame register offset",
        ))?;
    if offset <= MAX_PARAMETER_OFFSET {
        dynasm!(ops ; .arch aarch64 ; str X(scratch), [X(window), offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; str X(scratch), [X(window), x12]);
    }
    Ok(())
}

pub(super) fn load_int32(view: &JitCompileSnapshot, pc: u32) -> Result<i32, Unsupported> {
    match view
        .instructions
        .get(pc as usize)
        .and_then(|instruction| instruction.operand(&view.code_block, 1))
    {
        Some(Operand::Imm32(value)) => Ok(value),
        _ => Err(Unsupported::OperandShape("optimizing LoadInt32 operands")),
    }
}

pub(super) fn total_spill_slots(allocation: &Allocation) -> Result<u32, Unsupported> {
    analyzed_spill_slot_count(allocation).map_err(OptimizationError::into_unsupported)
}

pub(super) fn aligned_spill_bytes(spill_slot_count: u32) -> Result<u32, Unsupported> {
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

/// Largest unsigned scaled immediate offset for a 64-bit `ldr`/`str`.
pub(super) const MAX_SP_IMM_OFFSET: u32 = 4095 * 8;

/// `ldr X(reg), [sp, offset]` for any spill offset; big frames go through the
/// `x12` scratch the same way window addressing does.
pub(super) fn emit_sp_ldr_x(ops: &mut Assembler, reg: u8, offset: u32) {
    if offset <= MAX_SP_IMM_OFFSET {
        dynasm!(ops ; .arch aarch64 ; ldr X(reg), [sp, offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; ldr X(reg), [sp, x12]);
    }
}

/// `str X(reg), [sp, offset]` for any spill offset.
pub(super) fn emit_sp_str_x(ops: &mut Assembler, reg: u8, offset: u32) {
    if offset <= MAX_SP_IMM_OFFSET {
        dynasm!(ops ; .arch aarch64 ; str X(reg), [sp, offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; str X(reg), [sp, x12]);
    }
}

/// `ldr W(reg), [sp, offset]` for any spill offset.
pub(super) fn emit_sp_ldr_w(ops: &mut Assembler, reg: u8, offset: u32) {
    if offset <= MAX_SP_IMM_OFFSET {
        dynasm!(ops ; .arch aarch64 ; ldr W(reg), [sp, offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; ldr W(reg), [sp, x12]);
    }
}

/// `ldr D(reg), [sp, offset]` for any spill offset.
pub(super) fn emit_sp_ldr_d(ops: &mut Assembler, reg: u8, offset: u32) {
    if offset <= MAX_SP_IMM_OFFSET {
        dynasm!(ops ; .arch aarch64 ; ldr D(reg), [sp, offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; ldr D(reg), [sp, x12]);
    }
}

/// `str D(reg), [sp, offset]` for any spill offset.
pub(super) fn emit_sp_str_d(ops: &mut Assembler, reg: u8, offset: u32) {
    if offset <= MAX_SP_IMM_OFFSET {
        dynasm!(ops ; .arch aarch64 ; str D(reg), [sp, offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; str D(reg), [sp, x12]);
    }
}

pub(super) fn spill_offset(slot: u32) -> Result<u32, Unsupported> {
    slot.checked_mul(STACK_SLOT_BYTES)
        .ok_or(Unsupported::OperandShape("optimizing spill offset"))
}

pub(super) fn fp_spill_offset(allocation: &Allocation, slot: u32) -> Result<u32, Unsupported> {
    let unified = allocation
        .spill_slot_counts
        .gpr
        .checked_add(slot)
        .ok_or(Unsupported::OperandShape("optimizing FP spill offset"))?;
    spill_offset(unified)
}

pub(super) fn conversion_kind_at(
    reprs: &ReprMap,
    instruction: &SsaInstr,
    operand_index: usize,
) -> Option<ConversionKind> {
    reprs
        .conversions()
        .iter()
        .find(|conversion| {
            conversion.inline == instruction.inline
                && conversion.at_pc == instruction.pc
                && conversion.operand_index == operand_index
        })
        .map(|conversion| conversion.kind)
}

/// Fast-path source-lowered numeric coercions without invoking user code.
/// Tagged non-numbers bail at the coercion's exact bytecode PC so the
/// interpreter performs the observable `ToPrimitive` / `ToNumeric` semantics.
pub(super) fn emit_tagged_numeric_coercion(
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
        Representation::Int32 | Representation::Float64 => emit_move(
            ops,
            allocation,
            Move {
                src: allocation.location(input),
                dst: allocation.location(
                    instruction
                        .result
                        .expect("eligibility checked numeric coercion result"),
                ),
                conversion: None,
            },
        ),
    }
}

pub(super) fn emit_load_int_operand(
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

pub(super) fn emit_load_float_operand(
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
pub(super) fn emit_guard_int32(ops: &mut Assembler, register: u8, deopt: DynamicLabel) {
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
pub(super) fn emit_guard_number(ops: &mut Assembler, register: u8, deopt: DynamicLabel) {
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
pub(super) fn emit_num_to_double(
    ops: &mut Assembler,
    source: u8,
    destination: u8,
    deopt: DynamicLabel,
) {
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

pub(super) fn emit_load_fp_location(
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
            emit_sp_ldr_d(ops, scratch, offset);
        }
        Location::Register(RegClass::Gpr, _) | Location::Spill(RegClass::Gpr, _) => {
            return Err(Unsupported::OperandShape("optimizing non-FP location"));
        }
    }
    Ok(())
}

pub(super) fn emit_store_fp_location(
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
            emit_sp_str_d(ops, scratch, offset);
        }
        Location::Register(RegClass::Gpr, _) | Location::Spill(RegClass::Gpr, _) => {
            return Err(Unsupported::OperandShape("optimizing non-FP location"));
        }
    }
    Ok(())
}

pub(super) fn emit_load_location(
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
            emit_sp_ldr_w(ops, scratch, offset);
        }
        Location::Register(RegClass::Fp, _) | Location::Spill(RegClass::Fp, _) => {
            return Err(Unsupported::OperandShape("optimizing FP location"));
        }
    }
    Ok(())
}

pub(super) fn emit_store_location(
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
            // An int32 spill slot is 8 bytes and other emitters read it with
            // 64-bit loads (raw moves, boxing edge moves), so the store must
            // cover the whole slot. A 32-bit store would leave the upper half
            // as stack garbage that a later 64-bit read folds into the boxed
            // value. The `mov` zero-extends in case the caller's register
            // carries tag bits above the payload (OSR reloads pass the boxed
            // form).
            dynasm!(ops ; .arch aarch64 ; mov W(scratch), W(scratch));
            emit_sp_str_x(ops, scratch, offset);
        }
        Location::Register(RegClass::Fp, _) | Location::Spill(RegClass::Fp, _) => {
            return Err(Unsupported::OperandShape("optimizing FP location"));
        }
    }
    Ok(())
}

pub(super) fn emit_load_tagged_location(
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
            emit_sp_ldr_x(ops, scratch, offset);
        }
        Location::Register(RegClass::Fp, _) | Location::Spill(RegClass::Fp, _) => {
            return Err(Unsupported::OperandShape("optimizing FP location"));
        }
    }
    Ok(())
}

pub(super) fn emit_store_tagged_location(
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
            emit_sp_str_x(ops, scratch, offset);
        }
        Location::Register(RegClass::Fp, _) | Location::Spill(RegClass::Fp, _) => {
            return Err(Unsupported::OperandShape("optimizing FP location"));
        }
    }
    Ok(())
}

/// Whether an SSA index value still carries its `Value` tag when the dense
/// element guard reads it. A float64 index is not an element index at all.
pub(super) fn dense_index_form(
    reprs: &ReprMap,
    index: ValueId,
) -> Result<DenseIndexForm, Unsupported> {
    match reprs.representation(index) {
        Representation::Int32 => Ok(DenseIndexForm::Int32),
        Representation::Tagged => Ok(DenseIndexForm::Tagged),
        Representation::Float64 => Err(Unsupported::OperandShape("dense element float64 index")),
    }
}

/// Materialize an SSA index value into `register` in the form
/// [`dense_index_form`] declared for it.
pub(super) fn emit_load_dense_index(
    ops: &mut Assembler,
    reprs: &ReprMap,
    allocation: &Allocation,
    index: ValueId,
    register: u8,
) -> Result<(), Unsupported> {
    match dense_index_form(reprs, index)? {
        DenseIndexForm::Int32 => emit_load_location(ops, allocation.location(index), register),
        DenseIndexForm::Tagged => {
            emit_load_tagged_location(ops, allocation.location(index), register)
        }
    }
}

pub(super) fn emit_box_int32(ops: &mut Assembler, value: u8, scratch: u8) {
    dynasm!(ops
        ; .arch aarch64
        ; movz X(scratch), NUMBER_TAG_HI16, lsl #48
        ; orr X(value), X(value), X(scratch)
    );
}

/// Canonicalize NaN and add the VM's frozen JSC-style encode offset, matching
/// `template::arm64::values::emit_box_double` instruction for instruction.
pub(super) fn emit_box_double(ops: &mut Assembler, source: u8, destination: u8) {
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

pub(super) fn emit_load_i32(ops: &mut Assembler, register: u8, value: i32) {
    emit_load_u32(ops, register, value as u32);
}

pub(super) fn emit_load_u32(ops: &mut Assembler, register: u8, value: u32) {
    let low = value & 0xffff;
    let high = value >> 16;
    dynasm!(ops
        ; .arch aarch64
        ; movz W(register), low
        ; movk W(register), high, lsl #16
    );
}

pub(super) fn emit_load_u64(ops: &mut Assembler, register: u8, value: u64) {
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

pub(super) fn emit_load_symbolic_u64(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    register: u8,
    value: u64,
    target: RelocationTarget,
) {
    let start = ops.offset().0;
    emit_load_u64(ops, register, value);
    relocations.record_mov_wide(start, ops.offset().0, register, target);
}

pub(super) fn emit_runtime_entry(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    register: u8,
    entry: ResolvedRuntimeEntry,
) {
    emit_load_symbolic_u64(
        ops,
        relocations,
        register,
        entry.address,
        RelocationTarget::runtime_stub(entry.descriptor),
    );
}

pub(super) fn emit_prologue(ops: &mut Assembler, spill_frame_bytes: u32) {
    dynasm!(ops
        ; .arch aarch64
        ; stp x29, x30, [sp, #-16]!
        ; stp x19, x20, [sp, #-80]!
        ; stp x21, x22, [sp, #16]
        ; stp x23, x24, [sp, #32]
        ; stp x25, x26, [sp, #48]
        ; stp x27, x28, [sp, #64]
        ; stp d8, d9, [sp, #-64]!
        ; stp d10, d11, [sp, #16]
        ; stp d12, d13, [sp, #32]
        ; stp d14, d15, [sp, #48]
    );
    if spill_frame_bytes != 0 {
        if spill_frame_bytes <= 4095 {
            dynasm!(ops ; .arch aarch64 ; sub sp, sp, spill_frame_bytes);
        } else {
            emit_load_u32(ops, 12, spill_frame_bytes);
            dynasm!(ops ; .arch aarch64 ; sub sp, sp, x12);
        }
    }
}

pub(super) fn emit_epilogue(ops: &mut Assembler, spill_frame_bytes: u32) {
    if spill_frame_bytes != 0 {
        if spill_frame_bytes <= 4095 {
            dynasm!(ops ; .arch aarch64 ; add sp, sp, spill_frame_bytes);
        } else {
            emit_load_u32(ops, 12, spill_frame_bytes);
            dynasm!(ops ; .arch aarch64 ; add sp, sp, x12);
        }
    }
    dynasm!(ops
        ; .arch aarch64
        ; ldp d14, d15, [sp, #48]
        ; ldp d12, d13, [sp, #32]
        ; ldp d10, d11, [sp, #16]
        ; ldp d8, d9, [sp], #64
        ; ldp x27, x28, [sp, #64]
        ; ldp x25, x26, [sp, #48]
        ; ldp x23, x24, [sp, #32]
        ; ldp x21, x22, [sp, #16]
        ; ldp x19, x20, [sp], #80
        ; ldp x29, x30, [sp], #16
        ; ret
    );
}
