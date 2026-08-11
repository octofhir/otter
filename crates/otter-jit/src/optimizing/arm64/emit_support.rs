//! Low-level AArch64 emission helpers for the optimizing tier.
//!
//! # Contents
//! - Control-flow edges, comparisons and edge moves.
//! - Spill-slot addressing and register/location loads and stores.
//! - Representation conversions, boxing, constant materialization.
//! - Identity-guarded Int32 Math intrinsic bodies selected by the main
//!   instruction dispatcher.
//! - Activation-local loop cache addressing and complete cold-path clearing.
//! - Minimal frame prologue/epilogue over `x19,x21..x28` plus `d8..d15`.
//! - Published root/spliced register-window addressing across temporary
//!   generated-linkage stack reservations.
//!
//! # Invariants
//! - Nothing here decides *what* to emit; every function lowers one already
//!   chosen operation, so the dispatch in [`super`] owns all opcode policy.
//! - Math intrinsic bodies receive already-proven Int32 operands and execute
//!   only after the caller has emitted the exact builtin identity guard.
//! - Loop cache slots contain untraced raw addresses only while generated code
//!   cannot allocate or re-enter; every cold transition clears the full set.
//! - `x19` is an allocatable value register. Root-window helpers reload the
//!   interpreter base through the fixed `x20` context only at VM boundaries.
//!   Callers name any live scratch explicitly, so address materialization
//!   cannot overwrite the value about to be published through that window.
//! - The fixed context-pair save already preserves `x19`; the variable GPR
//!   save prefix therefore covers only the used `x21..x28` suffix.

use super::*;

/// Activation-local stack slot for one loop-invariant method guard.
pub(super) fn cached_method_guard_slot(
    eligibility: &Eligibility,
    base: u32,
    site: (InlineId, u32),
) -> Option<u32> {
    let index = eligibility
        .cached_method_guards
        .keys()
        .position(|entry| *entry == site)?;
    u32::try_from(index)
        .ok()
        .and_then(|index| index.checked_mul(STACK_SLOT_BYTES))
        .and_then(|offset| base.checked_add(offset))
}

/// Activation-local stack slot for one loop-invariant global-object read.
pub(super) fn cached_global_object_load_slot(
    eligibility: &Eligibility,
    base: u32,
    site: (InlineId, u32),
) -> Option<u32> {
    let index = eligibility
        .cached_global_object_loads
        .iter()
        .position(|entry| *entry == site)?;
    u32::try_from(index)
        .ok()
        .and_then(|index| index.checked_mul(STACK_SLOT_BYTES))
        .and_then(|offset| base.checked_add(offset))
}

/// Clear every raw receiver header and property address before a fresh entry
/// or a cold transition that can allocate, collect, or re-enter JavaScript.
pub(super) fn emit_clear_loop_caches(ops: &mut Assembler, base: u32, count: usize) {
    if count == 0 {
        return;
    }
    dynasm!(ops ; .arch aarch64 ; mov x9, xzr);
    for index in 0..count {
        let offset = base + u32::try_from(index).expect("cache count was frame-checked") * 8;
        emit_sp_str_x(ops, 9, offset);
    }
}

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
        Some(
            ConversionKind::CheckedTaggedToInt32
            | ConversionKind::CheckedTaggedToFloat64
            | ConversionKind::CheckedFloat64ToInt32,
        ) => Err(Unsupported::OperandShape(
            "optimizing checked phi conversion",
        )),
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
    let window = emit_root_window_base(ops, scratch);
    let offset = index * STACK_SLOT_BYTES;
    if offset <= MAX_PARAMETER_OFFSET {
        dynasm!(ops ; .arch aarch64 ; ldr X(scratch), [X(window), offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; ldr X(scratch), [X(window), x12]);
    }
}

/// Reload the root interpreter-window base only at a boundary that needs it.
fn emit_root_window_base(ops: &mut Assembler, protected: u8) -> u8 {
    let window = if protected == WINDOW_SCRATCH {
        HEADER_SCRATCH
    } else {
        WINDOW_SCRATCH
    };
    dynasm!(ops
        ; .arch aarch64
        ; ldr X(window), [x20, NATIVE_FRAME_OFFSET]
        ; ldr X(window), [X(window), NATIVE_FRAME_REGISTER_BASE_OFFSET]
    );
    window
}

/// Reload a loop-header live set from its interpreter-window slots.
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

/// Register holding the base of `inline`'s interpreter window.
///
/// The root frame's window is reloaded through `x20` on demand. A spliced
/// frame's window is a reservation in this generation's own stack frame.
pub(super) fn emit_window_base(
    ops: &mut Assembler,
    windows: &InlineWindows,
    inline: InlineId,
) -> Result<u8, Unsupported> {
    emit_window_base_protecting(ops, windows, inline, u8::MAX)
}

/// [`emit_window_base`] without clobbering one caller-owned scratch register.
pub(super) fn emit_window_base_protecting(
    ops: &mut Assembler,
    windows: &InlineWindows,
    inline: InlineId,
    protected: u8,
) -> Result<u8, Unsupported> {
    if inline == InlineId::ROOT {
        return Ok(emit_root_window_base(ops, protected));
    }
    let offset = windows.get(inline)?.window;
    // `sp` is only addressable through the fixed-register forms, so the
    // window base always materializes in the same scratch register.
    if offset <= 4095 {
        dynasm!(ops ; .arch aarch64 ; add x8, sp, offset);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; add x8, sp, x12);
    }
    Ok(WINDOW_SCRATCH)
}

/// Register-window base while generated linkage has temporarily moved `sp`.
///
/// Root windows are heap/interpreter-owned and reloaded through `x20`. An
/// inlined frame's window belongs to this code object's fixed stack frame, so
/// linkage reports the exact downward stack bias that must be added back.
/// `offset_scratch` must not contain a value the caller is about to store
/// through the returned base; `protected` names that source independently so
/// root-window reload can choose its other address scratch.
pub(super) fn emit_window_base_with_bias(
    ops: &mut Assembler,
    windows: &InlineWindows,
    inline: InlineId,
    sp_bias: u32,
    offset_scratch: u8,
    protected: u8,
) -> Result<u8, Unsupported> {
    if inline == InlineId::ROOT {
        return Ok(emit_root_window_base(ops, protected));
    }
    let offset = windows
        .get(inline)?
        .window
        .checked_add(sp_bias)
        .ok_or(Unsupported::OperandShape("optimizing biased inline window"))?;
    if offset <= 4095 {
        dynasm!(ops ; .arch aarch64 ; add x8, sp, offset);
    } else {
        emit_load_u32(ops, offset_scratch, offset);
        dynasm!(ops ; .arch aarch64 ; add x8, sp, X(offset_scratch));
    }
    Ok(WINDOW_SCRATCH)
}

/// Materialize only the transition operands and tagged values live across the
/// call, each into the window of the frame that names it. Optimizing spills are
/// private and unscanned, so live tagged spills take the same window rooting
/// path as tagged machine registers.
pub(super) fn emit_materialize_element_transition(
    ops: &mut Assembler,
    reprs: &ReprMap,
    allocation: &Allocation,
    windows: &InlineWindows,
    instruction: &SsaInstr,
    site: &ElementTransitionSite,
) -> Result<(), Unsupported> {
    let mut materialized = BTreeSet::new();
    for (&value, &register) in instruction.inputs.iter().zip(&instruction.input_registers) {
        emit_materialize_frame_value(
            ops,
            reprs,
            allocation,
            windows,
            instruction.inline,
            value,
            register,
        )?;
        materialized.insert((instruction.inline, register));
    }
    for live in &site.tagged_live_across {
        if !materialized.insert((live.inline, live.register)) {
            continue;
        }
        emit_load_tagged_location(ops, allocation.location(live.value), 9)?;
        let window = emit_window_base_protecting(ops, windows, live.inline, 9)?;
        emit_store_frame_register_in(ops, window, u32::from(live.register), 9)?;
    }
    Ok(())
}

pub(super) fn emit_materialize_frame_value(
    ops: &mut Assembler,
    reprs: &ReprMap,
    allocation: &Allocation,
    windows: &InlineWindows,
    inline: InlineId,
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
    let window = emit_window_base_protecting(ops, windows, inline, 9)?;
    emit_store_frame_register_in(ops, window, u32::from(register), 9)
}

/// Build the activation record of every spliced frame a transition needs, and
/// seed the window slots the transition itself does not write.
///
/// Publication makes a window a collector root wholesale, so every slot must
/// hold a real `Value`, not just the slots this site materializes. The whole
/// construction belongs on the transition path: an entry that never misses an
/// inline cache never builds a frame at all, and that is the common case.
pub(super) fn emit_build_transition_frames(
    ops: &mut Assembler,
    tree: &InlineTree,
    windows: &InlineWindows,
    instruction: &SsaInstr,
    site: &ElementTransitionSite,
) -> Result<(), Unsupported> {
    if instruction.inline == InlineId::ROOT {
        return Ok(());
    }
    let mut written = site
        .tagged_live_across
        .iter()
        .map(|live| (live.inline, live.register))
        .collect::<BTreeSet<_>>();
    written.extend(
        instruction
            .input_registers
            .iter()
            .map(|&register| (instruction.inline, register)),
    );
    for (inline, _) in transition_chain(tree, instruction)? {
        let layout = windows.get(inline)?;
        let function_id = tree
            .frames
            .get(inline.0 as usize)
            .ok_or(Unsupported::OperandShape("optimizing spliced frame body"))?
            .function_id();
        emit_load_u64(
            ops,
            9,
            u64::from(function_id) | (u64::from(function_id) << 32),
        );
        emit_sp_str_x(ops, 9, layout.record + NATIVE_FRAME_FUNCTION_ID_OFFSET);
        emit_load_u32(
            ops,
            9,
            stack_register_frame_shape_word(layout.register_count),
        );
        emit_sp_str_w(ops, 9, layout.record + NATIVE_FRAME_SHAPE_WORD_OFFSET);
        if layout.window <= 4095 {
            dynasm!(ops ; .arch aarch64 ; add x9, sp, layout.window);
        } else {
            emit_load_u32(ops, 12, layout.window);
            dynasm!(ops ; .arch aarch64 ; add x9, sp, x12);
        }
        emit_sp_str_x(ops, 9, layout.record + NATIVE_FRAME_REGISTER_BASE_OFFSET);
        dynasm!(ops ; .arch aarch64 ; mov x9, xzr);
        emit_sp_str_x(ops, 9, layout.record + NATIVE_FRAME_UPVALUE_BASE_OFFSET);
        emit_sp_str_x(ops, 9, layout.record + NATIVE_FRAME_UPVALUE_COUNT_OFFSET);
        debug_assert_eq!(
            NATIVE_FRAME_ACTIVATION_ID_OFFSET,
            NATIVE_FRAME_UPVALUE_COUNT_OFFSET + 4,
            "the upvalue count and activation id are one aligned pair"
        );
        emit_load_u64(ops, 9, VALUE_UNDEFINED);
        emit_sp_str_x(ops, 9, layout.record + NATIVE_FRAME_THIS_OFFSET);
        emit_sp_str_x(ops, 9, layout.record + NATIVE_FRAME_NEW_TARGET_OFFSET);
        emit_sp_str_x(ops, 9, layout.record + NATIVE_FRAME_SELF_OFFSET);
        for slot in 0..layout.register_count {
            if written.contains(&(inline, slot)) {
                continue;
            }
            emit_sp_str_x(ops, 9, layout.window + u32::from(slot) * STACK_SLOT_BYTES);
        }
    }
    Ok(())
}

/// Every spliced frame a transition at `instruction` is executing or paused
/// inside, outermost first, with the PC each of them resumes at.
fn transition_chain(
    tree: &InlineTree,
    instruction: &SsaInstr,
) -> Result<Vec<(InlineId, u32)>, Unsupported> {
    let mut chain = Vec::new();
    let mut current = instruction.inline;
    let mut resume = instruction.pc;
    while current != InlineId::ROOT {
        chain.push((current, resume));
        let call_site = tree
            .frames
            .get(current.0 as usize)
            .and_then(|frame| frame.call_site.as_ref())
            .ok_or(Unsupported::OperandShape("optimizing spliced frame parent"))?;
        resume = call_site.call_pc;
        current = call_site.parent;
    }
    chain.reverse();
    Ok(chain)
}

/// Publish the exact resume PC of the frame an operation is about to run in,
/// and of every spliced frame it is paused inside.
///
/// A logical PC is canonical only within its own body, so writing a spliced
/// frame's PC into the root's record would name a different instruction.
pub(super) fn emit_publish_transition_pc(
    ops: &mut Assembler,
    tree: &InlineTree,
    windows: &InlineWindows,
    instruction: &SsaInstr,
) -> Result<(), Unsupported> {
    let chain = transition_chain(tree, instruction)?;
    // The root frame is paused at the call that entered the outermost spliced
    // frame; without a chain it is the frame running the operation itself.
    let root_pc = match chain.first() {
        None => instruction.pc,
        Some(&(outermost, _)) => {
            tree.frames
                .get(outermost.0 as usize)
                .and_then(|frame| frame.call_site.as_ref())
                .ok_or(Unsupported::OperandShape("optimizing spliced frame parent"))?
                .call_pc
        }
    };
    emit_load_u32(ops, 9, root_pc);
    dynasm!(ops
        ; .arch aarch64
        ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
        ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
    );
    for (frame, frame_pc) in chain {
        let layout = windows.get(frame)?;
        emit_load_u32(ops, 9, frame_pc);
        emit_sp_str_w(ops, 9, layout.record + NATIVE_FRAME_PC_OFFSET);
    }
    Ok(())
}

pub(super) fn emit_publish_transition_frame(
    ops: &mut Assembler,
    tree: &InlineTree,
    windows: &InlineWindows,
    frame_states: &FrameStateTable,
    deopt_exits: &mut Vec<(DynamicLabel, DeoptExitId, u32)>,
    instruction: &SsaInstr,
) -> Result<usize, Unsupported> {
    emit_publish_transition_pc(ops, tree, windows, instruction)?;
    if instruction.inline == InlineId::ROOT {
        return Ok(0);
    }
    let chain = transition_chain(tree, instruction)?;
    let depth = u32::try_from(chain.len())
        .map_err(|_| Unsupported::OperandShape("optimizing spliced chain depth"))?;
    let overflow = ops.new_dynamic_label();
    deopt_exits.push((
        overflow,
        deopt_exit_at(frame_states, instruction)?,
        instruction.pc,
    ));
    // The activation array is both the publication capacity and the generated
    // recursion bound; exceeding it side-exits before any effect.
    dynasm!(ops
        ; .arch aarch64
        ; ldr x9, [x20, ACTIVATION_TOP_PTR_OFFSET]
        ; ldr x10, [x9]
        ; ldr x11, [x20, ACTIVATION_LIMIT_OFFSET]
        ; add x12, x10, depth
        ; cmp x12, x11
        ; b.hi =>overflow
        ; ldr x11, [x20, ACTIVATION_BASE_OFFSET]
        ; add x11, x11, x10, lsl #3
    );
    for &(frame, _) in &chain {
        let layout = windows.get(frame)?;
        if layout.record <= 4095 {
            dynasm!(ops ; .arch aarch64 ; add x14, sp, layout.record);
        } else {
            emit_load_u32(ops, 13, layout.record);
            dynasm!(ops ; .arch aarch64 ; add x14, sp, x13);
        }
        dynasm!(ops ; .arch aarch64 ; str x14, [x11], #8);
    }
    dynasm!(ops
        ; .arch aarch64
        ; str x12, [x9]
        ; ldr x13, [x20, NATIVE_FRAME_OFFSET]
    );
    emit_sp_str_x(ops, 13, windows.saved_frame);
    dynasm!(ops
        ; .arch aarch64
        ; str x14, [x20, NATIVE_FRAME_OFFSET]
        ; ldr x15, [x20, THREAD_OFFSET]
        ; str x14, [x15, VM_THREAD_CURRENT_FRAME_OFFSET]
    );
    Ok(chain.len())
}

/// Unpublish everything [`emit_publish_transition_frame`] pushed and restore
/// the caller's published activation.
pub(super) fn emit_unpublish_transition_frame(
    ops: &mut Assembler,
    windows: &InlineWindows,
    depth: usize,
) -> Result<(), Unsupported> {
    if depth == 0 {
        return Ok(());
    }
    let depth = u32::try_from(depth)
        .map_err(|_| Unsupported::OperandShape("optimizing spliced chain depth"))?;
    dynasm!(ops
        ; .arch aarch64
        ; ldr x9, [x20, ACTIVATION_TOP_PTR_OFFSET]
        ; ldr x10, [x9]
        ; sub x10, x10, depth
        ; str x10, [x9]
        ; ldr x11, [x20, ACTIVATION_BASE_OFFSET]
        ; add x11, x11, x10, lsl #3
    );
    for _ in 0..depth {
        dynasm!(ops ; .arch aarch64 ; str xzr, [x11], #8);
    }
    emit_sp_ldr_x(ops, 13, windows.saved_frame);
    dynasm!(ops
        ; .arch aarch64
        ; str x13, [x20, NATIVE_FRAME_OFFSET]
        ; ldr x15, [x20, THREAD_OFFSET]
        ; str x13, [x15, VM_THREAD_CURRENT_FRAME_OFFSET]
    );
    Ok(())
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

/// Reload every tagged value live across moving GC from the window that rooted
/// it, then optionally load a result last. Numeric homes and indices stay
/// untouched.
pub(super) fn emit_reload_element_transition(
    ops: &mut Assembler,
    allocation: &Allocation,
    windows: &InlineWindows,
    inline: InlineId,
    site: &ElementTransitionSite,
    load_result: Option<(u16, Location)>,
) -> Result<(), Unsupported> {
    let mut reloaded = BTreeSet::new();
    for live in &site.tagged_live_across {
        if !reloaded.insert(live.value) {
            continue;
        }
        let window = emit_window_base(ops, windows, live.inline)?;
        emit_load_frame_register_in(ops, window, u32::from(live.register), 9)?;
        emit_store_tagged_location(ops, allocation.location(live.value), 9)?;
    }
    if let Some((result_register, dst_location)) = load_result {
        let window = emit_window_base(ops, windows, inline)?;
        emit_load_frame_register_in(ops, window, u32::from(result_register), 9)?;
        emit_store_tagged_location(ops, dst_location, 9)?;
    }
    Ok(())
}

pub(super) fn emit_load_frame_register(
    ops: &mut Assembler,
    register: u32,
    scratch: u8,
) -> Result<(), Unsupported> {
    let window = emit_root_window_base(ops, scratch);
    emit_load_frame_register_in(ops, window, register, scratch)
}

/// Read from an interpreter register window addressed by `window`.
pub(super) fn emit_load_frame_register_in(
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
        dynasm!(ops ; .arch aarch64 ; ldr X(scratch), [X(window), offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; ldr X(scratch), [X(window), x12]);
    }
    Ok(())
}

pub(super) fn emit_store_frame_register(
    ops: &mut Assembler,
    register: u32,
    scratch: u8,
) -> Result<(), Unsupported> {
    let window = emit_root_window_base(ops, scratch);
    emit_store_frame_register_in(ops, window, register, scratch)
}

/// Store into an interpreter register window addressed by `window`.
///
/// The root frame's window is reloaded through `x20`; a spliced frame's is a
/// reservation in this generation's own stack frame, and the callee's
/// registers belong there and not in its caller's.
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

pub(super) fn load_int32(tree: &InlineTree, instruction: &SsaInstr) -> Result<i32, Unsupported> {
    match frame_operand(tree, instruction, 1) {
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

/// Largest unsigned scaled immediate offset for a 32-bit `ldr`/`str`.
pub(super) const MAX_SP_IMM_OFFSET_W: u32 = 4095 * 4;

/// `str W(reg), [sp, offset]` for any spill offset.
pub(super) fn emit_sp_str_w(ops: &mut Assembler, reg: u8, offset: u32) {
    if offset <= MAX_SP_IMM_OFFSET_W {
        dynasm!(ops ; .arch aarch64 ; str W(reg), [sp, offset]);
    } else {
        emit_load_u32(ops, 12, offset);
        dynasm!(ops ; .arch aarch64 ; str W(reg), [sp, x12]);
    }
}

/// `ldr W(reg), [sp, offset]` for any spill offset.
pub(super) fn emit_sp_ldr_w(ops: &mut Assembler, reg: u8, offset: u32) {
    if offset <= MAX_SP_IMM_OFFSET_W {
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
        Representation::Float64
            if conversion_kind_at(reprs, instruction, operand_index)
                == Some(ConversionKind::CheckedFloat64ToInt32) =>
        {
            let deopt = deopt.ok_or(Unsupported::OperandShape(
                "optimizing int32 narrow missing deopt exit",
            ))?;
            emit_load_fp_location(ops, allocation, allocation.location(input), FP_SCRATCH)?;
            emit_checked_float64_to_int32(ops, FP_SCRATCH, scratch, deopt);
            Ok(())
        }
        Representation::Float64 | Representation::Tagged => Err(Unsupported::OperandShape(
            "optimizing int32 operand conversion",
        )),
    }
}

/// Narrow an exact-int32 `Float64` into `scratch`, deoptimizing on a
/// fractional, out-of-range, NaN, or negative-zero value. The round trip
/// through `fcvtzs`/`scvtf` proves exactness — `fcvtzs` saturates and NaN
/// converts to zero, so any inexact input fails the compare — and a zero
/// result with the sign bit set is `-0.0`, which int32 cannot represent.
pub(super) fn emit_checked_float64_to_int32(
    ops: &mut Assembler,
    source_fp: u8,
    scratch: u8,
    deopt: DynamicLabel,
) {
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; fcvtzs W(scratch), D(source_fp)
        ; scvtf D(FP_SCRATCH_2), W(scratch)
        ; fcmp D(source_fp), D(FP_SCRATCH_2)
        ; b.ne =>deopt
        ; cbnz W(scratch), =>done
        ; fmov x14, D(source_fp)
        ; cbnz x14, =>deopt
        ; =>done
    );
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

/// Whether one guarded Math leaf can complete from its unboxed Int32 SSA
/// operands without crossing the Rust leaf ABI.
pub(super) fn guarded_int32_math_intrinsic_is_supported(
    stub_id: otter_vm::native_abi::RuntimeStubId,
    reprs: &ReprMap,
    instruction: &SsaInstr,
) -> bool {
    let arguments = &instruction.inputs[1..];
    if stub_id == otter_vm::native_abi::STUB_MATH_ABS_LEAF.id {
        return arguments.len() == 1 && reprs.representation(arguments[0]) == Representation::Int32;
    }
    if stub_id == otter_vm::native_abi::STUB_MATH_MAX_LEAF.id
        || stub_id == otter_vm::native_abi::STUB_MATH_MIN_LEAF.id
    {
        return arguments.len() == 2
            && arguments
                .iter()
                .all(|value| reprs.representation(*value) == Representation::Int32);
    }
    false
}

/// Emit one identity-guarded Math operation selected by
/// [`guarded_int32_math_intrinsic_is_supported`]. The tagged result is left in
/// `x0`, matching the ordinary declared-leaf ABI.
pub(super) fn emit_guarded_int32_math_intrinsic_body(
    ops: &mut Assembler,
    stub_id: otter_vm::native_abi::RuntimeStubId,
    allocation: &Allocation,
    instruction: &SsaInstr,
) -> Result<(), Unsupported> {
    let arguments = &instruction.inputs[1..];
    if stub_id == otter_vm::native_abi::STUB_MATH_ABS_LEAF.id {
        emit_load_location(ops, allocation.location(arguments[0]), 9)?;
        let minimum = ops.new_dynamic_label();
        let done = ops.new_dynamic_label();
        emit_load_u32(ops, 10, i32::MIN as u32);
        dynasm!(ops
            ; .arch aarch64
            ; cmp w9, w10
            ; b.eq =>minimum
            ; neg w10, w9
            ; cmp w9, wzr
            ; csel w0, w9, w10, ge
        );
        emit_box_int32(ops, 0, 10);
        dynasm!(ops
            ; .arch aarch64
            ; b =>done
            ; =>minimum
        );
        emit_load_u64(ops, 9, 2_147_483_648);
        dynasm!(ops ; .arch aarch64 ; scvtf D(FP_SCRATCH), x9);
        emit_box_double(ops, FP_SCRATCH, 0);
        dynasm!(ops ; .arch aarch64 ; =>done);
        return Ok(());
    }

    emit_load_location(ops, allocation.location(arguments[0]), 9)?;
    emit_load_location(ops, allocation.location(arguments[1]), 10)?;
    dynasm!(ops ; .arch aarch64 ; cmp w9, w10);
    if stub_id == otter_vm::native_abi::STUB_MATH_MAX_LEAF.id {
        dynasm!(ops ; .arch aarch64 ; csel w0, w9, w10, ge);
    } else if stub_id == otter_vm::native_abi::STUB_MATH_MIN_LEAF.id {
        dynasm!(ops ; .arch aarch64 ; csel w0, w9, w10, le);
    } else {
        return Err(Unsupported::OperandShape("guarded int32 Math intrinsic"));
    }
    emit_box_int32(ops, 0, 11);
    Ok(())
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

/// The callee-saved machine state one compiled body actually touches, and
/// therefore the exact save/restore set its prologue and epilogue move.
/// GPR index zero is `x19`, already covered by the fixed context-pair save;
/// later indices are the `x21..x28` prefix recorded here. An unused suffix is
/// never saved. Entry cost is what a generated call pays per invocation, so
/// the frame carries nothing idle.
#[derive(Debug, Clone, Copy)]
pub(super) struct SavedFrame {
    /// Used prefix of the additional allocatable GPR file (`x21..`), 0..=8.
    pub(super) gpr_count: u8,
    /// Used prefix of the allocatable FP file (`d8..`), 0..=8.
    pub(super) fp_count: u8,
    /// Spill-area reservation below the saved registers, 16-aligned.
    pub(super) spill_frame_bytes: u32,
}

impl SavedFrame {
    pub(super) fn from_allocation(allocation: &Allocation, spill_frame_bytes: u32) -> Self {
        let mut gpr_count = 0u8;
        let mut fp_count = 0u8;
        for &location in allocation.locations.iter() {
            if let Location::Register(class, index) = location {
                match class {
                    RegClass::Gpr if index > 0 => gpr_count = gpr_count.max(index),
                    RegClass::Gpr => {}
                    RegClass::Fp => fp_count = fp_count.max(index + 1),
                }
            }
        }
        Self {
            gpr_count: gpr_count.min(8),
            fp_count: fp_count.min(8),
            spill_frame_bytes,
        }
    }

    /// Persistent prologue reservation: frame record, context pair, used
    /// register saves (16-aligned per class), and the spill area.
    pub(super) fn persistent_bytes(&self) -> u32 {
        32 + Self::save_bytes(self.gpr_count)
            + Self::save_bytes(self.fp_count)
            + self.spill_frame_bytes
    }

    fn save_bytes(count: u8) -> u32 {
        (u32::from(count) * 8).next_multiple_of(16)
    }
}

pub(super) fn emit_prologue(ops: &mut Assembler, saved: SavedFrame) {
    dynasm!(ops
        ; .arch aarch64
        ; stp x29, x30, [sp, #-16]!
        ; stp x19, x20, [sp, #-16]!
    );
    for pair in 0..(saved.gpr_count / 2) {
        let first = 21 + pair * 2;
        dynasm!(ops ; .arch aarch64 ; stp X(first), X(first + 1), [sp, #-16]!);
    }
    if !saved.gpr_count.is_multiple_of(2) {
        let last = 21 + saved.gpr_count - 1;
        dynasm!(ops ; .arch aarch64 ; str X(last), [sp, #-16]!);
    }
    for pair in 0..(saved.fp_count / 2) {
        let first = 8 + pair * 2;
        dynasm!(ops ; .arch aarch64 ; stp D(first), D(first + 1), [sp, #-16]!);
    }
    if !saved.fp_count.is_multiple_of(2) {
        let last = 8 + saved.fp_count - 1;
        dynasm!(ops ; .arch aarch64 ; str D(last), [sp, #-16]!);
    }
    let spill_frame_bytes = saved.spill_frame_bytes;
    if spill_frame_bytes != 0 {
        if spill_frame_bytes <= 4095 {
            dynasm!(ops ; .arch aarch64 ; sub sp, sp, spill_frame_bytes);
        } else {
            emit_load_u32(ops, 12, spill_frame_bytes);
            dynasm!(ops ; .arch aarch64 ; sub sp, sp, x12);
        }
    }
}

pub(super) fn emit_epilogue(ops: &mut Assembler, saved: SavedFrame) {
    let spill_frame_bytes = saved.spill_frame_bytes;
    if spill_frame_bytes != 0 {
        if spill_frame_bytes <= 4095 {
            dynasm!(ops ; .arch aarch64 ; add sp, sp, spill_frame_bytes);
        } else {
            emit_load_u32(ops, 12, spill_frame_bytes);
            dynasm!(ops ; .arch aarch64 ; add sp, sp, x12);
        }
    }
    if !saved.fp_count.is_multiple_of(2) {
        let last = 8 + saved.fp_count - 1;
        dynasm!(ops ; .arch aarch64 ; ldr D(last), [sp], #16);
    }
    for pair in (0..(saved.fp_count / 2)).rev() {
        let first = 8 + pair * 2;
        dynasm!(ops ; .arch aarch64 ; ldp D(first), D(first + 1), [sp], #16);
    }
    if !saved.gpr_count.is_multiple_of(2) {
        let last = 21 + saved.gpr_count - 1;
        dynasm!(ops ; .arch aarch64 ; ldr X(last), [sp], #16);
    }
    for pair in (0..(saved.gpr_count / 2)).rev() {
        let first = 21 + pair * 2;
        dynasm!(ops ; .arch aarch64 ; ldp X(first), X(first + 1), [sp], #16);
    }
    dynasm!(ops
        ; .arch aarch64
        ; ldp x19, x20, [sp], #16
        ; ldp x29, x30, [sp], #16
        ; ret
    );
}
