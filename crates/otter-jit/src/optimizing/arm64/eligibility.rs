//! Optimizing-tier eligibility analysis.
//!
//! # Contents
//! - Which inline bodies and method splices the backend can lower.
//! - Per-instruction operand, representation and constant checks.
//! - OSR entry sites and element-transition safepoint sites.
//!
//! # Invariants
//! - Nothing here emits machine code. A function that answers "can this be
//!   lowered" lives here; one that lowers it lives in [`super`] or
//!   [`super::emit_support`].
//! - Every rejection is an [`Unsupported`], so an unlowerable body keeps the
//!   template tier rather than failing the compile.

use super::*;
use crate::ir::licm::natural_loop_blocks;

pub(super) fn splice_lowerable(callee: &otter_vm::JitInlineCallee) -> bool {
    callee.instructions.iter().all(|instruction| {
        matches!(
            instruction.op(callee.code_block.as_ref()),
            Op::LoadInt32
                | Op::LoadNumber
                | Op::LoadUndefined
                | Op::LoadNull
                | Op::LoadTrue
                | Op::LoadFalse
                | Op::LoadLocal
                | Op::StoreLocal
                | Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Div
                | Op::Rem
                | Op::Neg
                | Op::Increment
                | Op::AddImm
                | Op::SubImm
                | Op::BitwiseAndImm
                | Op::LessThanImm
                | Op::EqualImm
                | Op::NotEqualImm
                | Op::LogicalNot
                | Op::BitwiseAnd
                | Op::BitwiseOr
                | Op::BitwiseXor
                | Op::Shl
                | Op::Shr
                | Op::Equal
                | Op::NotEqual
                | Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Jump
                | Op::JumpIfTrue
                | Op::JumpIfFalse
                | Op::Return
                | Op::ReturnValue
                | Op::ReturnUndefined
        )
    })
}

/// `true` when a method body can execute wholly inside the optimizing unit.
///
/// Property reads must have one VM-baked sealed data slot. Calls, stores,
/// allocations, and other effectful operations stay on the ordinary generated
/// call path so a side exit never repeats a committed effect.
pub(super) fn splice_lowerable_method(method: &otter_vm::JitInlineMethod) -> bool {
    method.instructions.iter().all(|instruction| {
        let op = instruction.op(method.code_block.as_ref());
        if op == Op::LoadProperty && !method.prop_offsets.contains_key(&instruction.byte_pc) {
            return false;
        }
        matches!(
            op,
            Op::LoadInt32
                | Op::LoadNumber
                | Op::LoadUndefined
                | Op::LoadNull
                | Op::LoadTrue
                | Op::LoadFalse
                | Op::LoadThis
                | Op::LoadLocal
                | Op::StoreLocal
                | Op::LoadProperty
                | Op::ToPrimitive
                | Op::ToNumeric
                | Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Div
                | Op::Rem
                | Op::Neg
                | Op::Increment
                | Op::AddImm
                | Op::SubImm
                | Op::BitwiseAndImm
                | Op::LessThanImm
                | Op::EqualImm
                | Op::NotEqualImm
                | Op::LogicalNot
                | Op::BitwiseAnd
                | Op::BitwiseOr
                | Op::BitwiseXor
                | Op::Shl
                | Op::Shr
                | Op::Equal
                | Op::NotEqual
                | Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Jump
                | Op::JumpIfTrue
                | Op::JumpIfFalse
                | Op::Return
                | Op::ReturnValue
                | Op::ReturnUndefined
        )
    })
}

/// Direct value-slab read proven by an enclosing method identity guard.
pub(super) fn inline_method_property<'a>(
    tree: &'a InlineTree,
    instruction: &SsaInstr,
) -> Option<(&'a InlineFrame, u32, u32)> {
    if instruction.inline == InlineId::ROOT || instruction.op != SsaOp::Bytecode(Op::LoadProperty) {
        return None;
    }
    let frame = tree.frames.get(instruction.inline.0 as usize)?;
    let method = frame.method.as_ref()?;
    let byte_pc = frame.instructions.get(instruction.pc as usize)?.byte_pc;
    let value_byte = *method.prop_offsets.get(&byte_pc)?;
    let guard = match &frame.call_site.as_ref()?.kind {
        InlineCallKind::Method { guard, .. } => guard,
        InlineCallKind::Plain { .. } => return None,
    };
    let expected_shape = method
        .prop_shapes
        .get(&byte_pc)
        .copied()
        .unwrap_or(guard.recv_shape);
    Some((frame, value_byte, expected_shape))
}

/// Receiver-own property load that can consume the method guard's already
/// validated receiver body instead of probing the same cell and shape twice.
///
/// Only the leading `LoadLocal`/`LoadThis` scaffolding may separate the call
/// guard from this load. Those operations cannot allocate, throw, or mutate the
/// heap, so the raw body pointer remains valid in the reserved native-stack
/// slot until the fused load consumes it.
pub(super) fn fused_inline_method_property<'a>(
    tree: &'a InlineTree,
    ssa: &SsaFunction,
    instruction: &SsaInstr,
) -> Option<(&'a InlineFrame, &'a otter_vm::jit::JitMethodGuard, u32)> {
    let (frame, value_byte, expected_shape) = inline_method_property(tree, instruction)?;
    let call_site = frame.call_site.as_ref()?;
    let InlineCallKind::Method { guard, .. } = &call_site.kind else {
        return None;
    };
    let synthetic_this = ssa.frames.get(instruction.inline.0 as usize)?.this_value?;
    let loaded_this = *instruction.inputs.first()?;
    let loads_synthetic_this = matches!(
        &ssa.values.get(loaded_this.0 as usize)?.def,
        ValueDef::Op {
            inline,
            op: SsaOp::Bytecode(Op::LoadThis),
            inputs,
            ..
        } if *inline == instruction.inline && inputs.as_ref() == [synthetic_this]
    );
    if expected_shape != guard.recv_shape || !loads_synthetic_this {
        return None;
    }
    let property_pc = usize::try_from(instruction.pc).ok()?;
    if frame.instructions.iter().take(property_pc).any(|metadata| {
        !matches!(
            metadata.op(frame.code_block.as_ref()),
            Op::LoadLocal | Op::LoadThis
        )
    }) {
        return None;
    }
    Some((frame, guard, value_byte))
}

pub(super) fn fused_method_property_for_frame(
    tree: &InlineTree,
    ssa: &SsaFunction,
    inline: InlineId,
) -> bool {
    ssa.blocks
        .iter()
        .flat_map(|block| &block.instrs)
        .any(|instruction| {
            instruction.inline == inline
                && fused_inline_method_property(tree, ssa, instruction).is_some()
        })
}

pub(super) fn guard_cache_safe_instruction(
    tree: &InlineTree,
    cfg: &ControlFlowGraph,
    block: BlockId,
    instruction: &SsaInstr,
) -> bool {
    let Some(op) = instruction.op.bytecode() else {
        // A primitive guard or field read touches no cache cell.
        return true;
    };
    match op {
        Op::LoadInt32
        | Op::LoadNumber
        | Op::LoadUndefined
        | Op::LoadNull
        | Op::LoadTrue
        | Op::LoadFalse
        | Op::LoadLocal
        | Op::StoreLocal
        | Op::LoadThis
        | Op::ToPrimitive
        | Op::ToNumeric
        | Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Div
        | Op::Rem
        | Op::Neg
        | Op::Increment
        | Op::AddImm
        | Op::SubImm
        | Op::BitwiseAndImm
        | Op::LessThanImm
        | Op::EqualImm
        | Op::NotEqualImm
        | Op::LogicalNot
        | Op::BitwiseAnd
        | Op::BitwiseOr
        | Op::BitwiseXor
        | Op::Shl
        | Op::Shr
        | Op::Equal
        | Op::NotEqual
        | Op::LessThan
        | Op::LessEq
        | Op::GreaterThan
        | Op::GreaterEq
        | Op::Jump
        | Op::JumpIfTrue
        | Op::JumpIfFalse => true,
        Op::LoadProperty => inline_method_property(tree, instruction).is_some(),
        Op::CallMethodValue => is_spliced_call(cfg, block, instruction),
        Op::Return | Op::ReturnValue | Op::ReturnUndefined => matches!(
            cfg.blocks[block.0 as usize].terminator,
            Terminator::InlineReturn { .. }
        ),
        _ => false,
    }
}

pub(super) fn cached_method_guard_site(
    tree: &InlineTree,
    cfg: &ControlFlowGraph,
    ssa: &SsaFunction,
    back_edges: &BTreeMap<(BlockId, BlockId), (DeoptExitId, u32)>,
) -> Option<(InlineId, u32)> {
    let mut candidates = Vec::new();
    for block in &cfg.blocks {
        for instruction in &ssa.blocks[block.id.0 as usize].instrs {
            if instruction.op != SsaOp::Bytecode(Op::CallMethodValue)
                || !is_spliced_call(cfg, block.id, instruction)
            {
                continue;
            }
            let Terminator::InlineCall { callee_entry, .. } = block.terminator else {
                continue;
            };
            if !fused_method_property_for_frame(
                tree,
                ssa,
                cfg.blocks[callee_entry.0 as usize].inline,
            ) {
                continue;
            }
            candidates.push((block.id, instruction));
        }
    }
    let [(call_block, call)] = candidates.as_slice() else {
        return None;
    };
    let receiver = ssa.copy_origin(*call.inputs.first()?)?;
    for &(latch, header) in back_edges.keys() {
        let loop_blocks = natural_loop_blocks(cfg, latch, header);
        if !loop_blocks.contains(call_block)
            || loop_blocks.contains(&ssa.values.get(receiver.0 as usize)?.def_block)
        {
            continue;
        }
        if loop_blocks.iter().all(|block| {
            ssa.blocks[block.0 as usize]
                .instrs
                .iter()
                .all(|instruction| guard_cache_safe_instruction(tree, cfg, *block, instruction))
        }) {
            return Some((call.inline, call.pc));
        }
    }
    None
}

/// Canonical instruction index of `byte_pc` within `function_id`'s body.
pub(super) fn logical_pc(
    tree: &InlineTree,
    function_id: u32,
    byte_pc: u32,
) -> Result<u32, Unsupported> {
    let frame = tree
        .frames
        .iter()
        .find(|frame| frame.function_id == function_id)
        .ok_or(Unsupported::OperandShape("optimizing chain frame body"))?;
    frame
        .instructions
        .iter()
        .position(|instruction| instruction.byte_pc == byte_pc)
        .map(|position| position as u32)
        .ok_or(Unsupported::OperandShape("optimizing chain frame byte PC"))
}

/// `true` when this instruction is the call a spliced frame replaces.
pub(super) fn is_spliced_call(
    cfg: &ControlFlowGraph,
    block: BlockId,
    instruction: &SsaInstr,
) -> bool {
    let block = &cfg.blocks[block.0 as usize];
    matches!(block.terminator, Terminator::InlineCall { .. })
        && block.instr_pcs.last() == Some(&instruction.pc)
}

/// Arithmetic feedback for one instruction of its own frame — a spliced
/// callee's instruction must never read the root body's cell.
pub(super) fn frame_feedback(
    tree: &InlineTree,
    instruction: &SsaInstr,
) -> otter_vm::jit_feedback::ArithFeedback {
    tree.frames[instruction.inline.0 as usize]
        .instructions
        .get(instruction.pc as usize)
        .map_or_else(
            otter_vm::jit_feedback::ArithFeedback::default,
            otter_vm::JitInstructionMetadata::arith_feedback,
        )
}

pub(super) fn check_eligibility(
    view: &JitCompileSnapshot,
    tree: &InlineTree,
    cfg: &ControlFlowGraph,
    dom: &DominatorTree,
    ssa: &SsaFunction,
    hoisted_loop_headers: &BTreeSet<BlockId>,
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
            // A back edge is a same-frame edge to a leader at or before this
            // block's. A splice edge leaves the frame — a callee entering at PC
            // 0 from a call block at PC 0 is not a loop — and PCs of different
            // frames are not comparable at all.
            let target = &cfg.blocks[successor.0 as usize];
            if target.inline == block.inline && target.start_pc <= block.start_pc {
                if !dom.dominates(successor, block.id) {
                    return Err(Unsupported::OperandShape(
                        "optimizing subset rejects irreducible back-edges",
                    ));
                }
                let header = &cfg.blocks[successor.0 as usize];
                back_edges.insert(
                    (block.id, successor),
                    (
                        DeoptLowering::exit_at(frame_states, header.inline, header.start_pc)
                            .ok_or(Unsupported::OperandShape(
                                "optimizing back edge header has no frame state",
                            ))?,
                        header.start_pc,
                    ),
                );
            }
        }
        match block.terminator {
            Terminator::FallThrough | Terminator::Jump if block.normal_succs.len() == 1 => {}
            Terminator::Branch { .. } if !block.normal_succs.is_empty() => {}
            Terminator::Return if block.normal_succs.is_empty() => {}
            // A spliced call reaches its callee's entry; the callee's returns
            // reach the call's continuation. The graph already verified both as
            // single-successor frame-crossing edges.
            Terminator::InlineCall { .. } | Terminator::InlineReturn { .. }
                if block.normal_succs.len() == 1 => {}
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
    let mut allowed_conversions = BTreeSet::<(InlineId, u32, usize)>::new();
    let mut element_transition_instructions = Vec::new();
    let mut insufficient_feedback = BTreeSet::new();
    for block in dom.reverse_postorder().iter().copied() {
        for (instruction_index, instruction) in
            ssa.blocks[block.0 as usize].instrs.iter().enumerate()
        {
            let Some(op) = instruction.op.bytecode() else {
                // A primitive neither calls, throws, nor allocates, so it owes
                // no materialized frame. Its receiver, its holder and anything
                // it produces are ordinary boxed values; a numeric operand is
                // boxed at the site, which is the conversion representation
                // selection already planned.
                if instruction
                    .result
                    .is_some_and(|result| reprs.representation(result) != Representation::Tagged)
                {
                    return Err(instruction_unsupported(instruction));
                }
                check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                continue;
            };
            // A feedback-driven op whose cell never recorded an execution is
            // unreachable-by-feedback: lower it as an unconditional deopt
            // instead of refusing the whole function for a cold path.
            if matches!(
                op,
                Op::Add
                    | Op::Sub
                    | Op::Mul
                    | Op::Div
                    | Op::Rem
                    | Op::Neg
                    | Op::Increment
                    | Op::AddImm
                    | Op::SubImm
                    | Op::BitwiseAndImm
                    | Op::LessThanImm
                    | Op::EqualImm
                    | Op::NotEqualImm
                    | Op::LessThan
                    | Op::LessEq
                    | Op::GreaterThan
                    | Op::GreaterEq
                    | Op::Equal
                    | Op::NotEqual
                    | Op::BitwiseAnd
                    | Op::BitwiseOr
                    | Op::BitwiseXor
                    | Op::Shl
                    | Op::Shr
            ) && frame_feedback(tree, instruction).is_unseen()
            {
                insufficient_feedback.insert((instruction.inline, instruction.pc));
                // Whatever conversions representation selection planned at
                // this site are moot — the op is never emitted — but they must
                // not fail the unit-wide conversion sweep.
                for conversion in reprs.conversions() {
                    if conversion.inline == instruction.inline && conversion.at_pc == instruction.pc
                    {
                        allowed_conversions.insert((
                            conversion.inline,
                            conversion.at_pc,
                            conversion.operand_index,
                        ));
                    }
                }
                continue;
            }
            match op {
                Op::LoadInt32 => check_constant_result(instruction, reprs)?,
                Op::LoadNumber => check_number_constant_result(view, instruction, reprs)?,
                Op::LoadUndefined => check_tagged_constant_result(instruction, reprs)?,
                Op::LoadNull => check_tagged_constant_result(instruction, reprs)?,
                Op::LoadThis if instruction.inline != InlineId::ROOT => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("inlined LoadThis result"))?;
                    if instruction.inputs.len() != 1
                        || !instruction.input_registers.is_empty()
                        || reprs.representation(result) != Representation::Tagged
                        || reprs.representation(instruction.inputs[0]) != Representation::Tagged
                    {
                        return Err(Unsupported::Opcode(op));
                    }
                }
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
                        return Err(Unsupported::Opcode(op));
                    }
                }
                Op::ToPrimitive | Op::ToNumeric => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("numeric coercion result"))?;
                    if instruction.inputs.len() != 1
                        || instruction.input_registers.len() != 1
                        || instruction.result_register.is_none()
                        || reprs.representation(result)
                            != reprs.representation(instruction.inputs[0])
                    {
                        return Err(Unsupported::Opcode(op));
                    }
                    if reprs.representation(instruction.inputs[0]) == Representation::Tagged {
                        check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
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
                        return Err(Unsupported::Opcode(op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    element_transition_instructions.push((
                        instruction.pc,
                        block,
                        instruction_index,
                    ));
                }
                Op::StoreElement => {
                    if instruction.result.is_some()
                        || instruction.result_register.is_some()
                        || instruction.inputs.len() != 3
                        || instruction.input_registers.len() != 3
                        || reprs.representation(instruction.inputs[0]) != Representation::Tagged
                        || reprs.representation(instruction.inputs[1]) == Representation::Float64
                    {
                        return Err(Unsupported::Opcode(op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    element_transition_instructions.push((
                        instruction.pc,
                        block,
                        instruction_index,
                    ));
                }
                Op::LoadProperty if inline_method_property(tree, instruction).is_some() => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("inlined property-load result"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || instruction.inputs.len() != 1
                        || instruction.input_registers.len() != 1
                        || instruction.result_register.is_none()
                        || reprs.representation(instruction.inputs[0]) != Representation::Tagged
                    {
                        return Err(Unsupported::Opcode(op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
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
                        return Err(Unsupported::Opcode(op));
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
                        return Err(Unsupported::Opcode(op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    element_transition_instructions.push((
                        instruction.pc,
                        block,
                        instruction_index,
                    ));
                }
                Op::LoadGlobalOrThrow => {
                    // `WRITE_CONST`: the name is a constant immediate and there are
                    // no register inputs; the global value is tagged. The reentrant
                    // stub can allocate/throw, so it is a precise-rooted transition.
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("global-load result"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || !instruction.inputs.is_empty()
                        || !instruction.input_registers.is_empty()
                        || instruction.result_register.is_none()
                    {
                        return Err(Unsupported::Opcode(op));
                    }
                    element_transition_instructions.push((
                        instruction.pc,
                        block,
                        instruction_index,
                    ));
                }
                Op::LooseEqual | Op::LooseNotEqual => {
                    // `WRITE_READ_READ`: two tagged register operands compared
                    // under §7.2.14 abstract equality, which may run `ToPrimitive`
                    // (user `valueOf`/`toString`), allocate, and throw — a precise
                    // reentrant transition. The boolean result is tagged.
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("loose-eq result"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || instruction.inputs.len() != 2
                        || instruction.input_registers.len() != 2
                        || instruction.result_register.is_none()
                        || reprs.representation(instruction.inputs[0]) != Representation::Tagged
                        || reprs.representation(instruction.inputs[1]) != Representation::Tagged
                    {
                        return Err(Unsupported::Opcode(op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    element_transition_instructions.push((
                        instruction.pc,
                        block,
                        instruction_index,
                    ));
                }
                Op::CallMethodValue if !is_spliced_call(cfg, block, instruction) => {
                    // `dst, receiver, name-const, argc-const, arg-regs...`. The
                    // receiver plus up to `MAX_METHOD_ARGS` tagged arguments are
                    // register inputs; resolution runs the full method walk and
                    // may reenter arbitrary (possibly compiled) callee code, so it
                    // is a precise reentrant transition. An exotic-receiver report
                    // side-exits with a full deopt to this pc.
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("method-call result"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || instruction.result_register.is_none()
                        || instruction.input_registers.is_empty()
                        || instruction.input_registers.len() > 1 + MAX_METHOD_ARGS
                        || instruction.inputs.len() != instruction.input_registers.len()
                        || reprs.representation(instruction.inputs[0]) != Representation::Tagged
                    {
                        return Err(Unsupported::Opcode(op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    element_transition_instructions.push((
                        instruction.pc,
                        block,
                        instruction_index,
                    ));
                }
                Op::CallMethodValue if is_spliced_call(cfg, block, instruction) => {
                    if instruction.result.is_some()
                        || instruction.result_register.is_some()
                        || instruction.inputs.is_empty()
                        || instruction.inputs.len() != instruction.input_registers.len()
                        || reprs.representation(instruction.inputs[0]) != Representation::Tagged
                    {
                        return Err(Unsupported::Opcode(op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                }
                Op::New => {
                    // `dst, callee, argc-const, arg-regs...`. Construction may
                    // execute arbitrary JS, allocate, or throw, so every tagged
                    // operand is materialized in the precise frame window.
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("construct result"))?;
                    let argc = view.instructions[instruction.pc as usize]
                        .const_index(view.code_block.as_ref(), 2)
                        .ok_or(Unsupported::OperandShape("construct argument count"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || instruction.result_register.is_none()
                        || instruction.input_registers.is_empty()
                        || instruction.input_registers.len() > 1 + MAX_METHOD_ARGS
                        || instruction.inputs.len() != instruction.input_registers.len()
                        || argc as usize != instruction.input_registers.len() - 1
                        || instruction
                            .inputs
                            .iter()
                            .any(|&input| reprs.representation(input) != Representation::Tagged)
                    {
                        return Err(Unsupported::Opcode(op));
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
                        return Err(Unsupported::Opcode(op));
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
                Op::Increment => {
                    // `dst = src + delta` over int32 (the common loop-counter form);
                    // float increments stay ineligible and complete elsewhere.
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("increment result"))?;
                    if reprs.representation(result) != Representation::Int32
                        || instruction.inputs.len() != 1
                    {
                        return Err(Unsupported::Opcode(op));
                    }
                    check_numeric_inputs(
                        instruction,
                        ssa,
                        reprs,
                        Representation::Int32,
                        &mut guarded_uses,
                        &mut allowed_conversions,
                    )?;
                }
                // Immediate-right int32 operators: one register input (the left
                // operand); the right operand is the baked constant. Same int32
                // discipline as `Op::Increment` / the register bitwise family.
                Op::AddImm | Op::SubImm | Op::BitwiseAndImm => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("immediate int32 result"))?;
                    if reprs.representation(result) != Representation::Int32
                        || instruction.inputs.len() != 1
                    {
                        return Err(Unsupported::Opcode(op));
                    }
                    check_numeric_inputs(
                        instruction,
                        ssa,
                        reprs,
                        Representation::Int32,
                        &mut guarded_uses,
                        &mut allowed_conversions,
                    )?;
                }
                // Immediate-right comparisons: one register input, boolean
                // result. Restricted to int32 operand feedback (the counting-loop
                // shape); any other feedback declines to the template tier.
                Op::LessThanImm | Op::EqualImm | Op::NotEqualImm => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("immediate comparison result"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || instruction.inputs.len() != 1
                        || !frame_feedback(tree, instruction).is_int32_only()
                    {
                        return Err(Unsupported::OperandShape("immediate comparison shape"));
                    }
                    check_numeric_inputs(
                        instruction,
                        ssa,
                        reprs,
                        Representation::Int32,
                        &mut guarded_uses,
                        &mut allowed_conversions,
                    )?;
                }
                Op::Neg => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("negate result"))?;
                    let result_repr = reprs.representation(result);
                    if !matches!(result_repr, Representation::Int32 | Representation::Float64)
                        || instruction.inputs.len() != 1
                    {
                        return Err(Unsupported::Opcode(op));
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
                Op::LogicalNot => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("logical-not result"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || instruction.inputs.len() != 1
                        || instruction.input_registers.len() != 1
                    {
                        return Err(Unsupported::Opcode(op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                }
                Op::Div | Op::Rem => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("float arithmetic result"))?;
                    if reprs.representation(result) != Representation::Float64
                        || instruction.inputs.len() != 2
                    {
                        return Err(Unsupported::Opcode(op));
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
                Op::BitwiseAnd | Op::BitwiseOr | Op::BitwiseXor | Op::Shl | Op::Shr => {
                    // Bitwise and shift results are int32 in JS. Int32-only
                    // feedback computes directly; mixed numeric feedback takes
                    // float64 operands through the exact JS ToInt32 conversion
                    // (fjcvtzs) and returns the int32 result as an exact double.
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("bitwise result"))?;
                    if instruction.inputs.len() != 2 {
                        return Err(Unsupported::Opcode(op));
                    }
                    let required = match reprs.representation(result) {
                        Representation::Int32 => Representation::Int32,
                        Representation::Float64 => Representation::Float64,
                        Representation::Tagged => {
                            return Err(Unsupported::Opcode(op));
                        }
                    };
                    check_numeric_inputs(
                        instruction,
                        ssa,
                        reprs,
                        required,
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
                    if reprs.representation(result) != Representation::Tagged {
                        return Err(Unsupported::OperandShape("comparison result repr"));
                    }
                    if instruction.inputs.len() != 2 {
                        return Err(Unsupported::OperandShape("comparison input count"));
                    }
                    let feedback = frame_feedback(tree, instruction);
                    if feedback.is_int32_only() {
                        check_numeric_inputs(
                            instruction,
                            ssa,
                            reprs,
                            Representation::Int32,
                            &mut guarded_uses,
                            &mut allowed_conversions,
                        )?;
                    } else if feedback.is_numeric_only() {
                        check_numeric_inputs(
                            instruction,
                            ssa,
                            reprs,
                            Representation::Float64,
                            &mut guarded_uses,
                            &mut allowed_conversions,
                        )?;
                    } else if matches!(op, Op::Equal | Op::NotEqual) {
                        // Mixed operands: strict equality is total over tagged
                        // values, so it lowers inline whatever the feedback.
                        check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    } else {
                        return Err(Unsupported::OperandShape("comparison feedback kind"));
                    }
                }
                // A plain call uses only a VM-baked monomorphic native target.
                // Every tagged operand is materialized in the precise frame
                // window first; absence or invalidation of that target takes
                // the exact pre-effect deopt exit.
                Op::Call if !is_spliced_call(cfg, block, instruction) => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("call result"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || instruction.result_register.is_none()
                        || instruction.input_registers.is_empty()
                        || instruction.inputs.len() != instruction.input_registers.len()
                    {
                        return Err(Unsupported::Opcode(op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                    let frame = &tree.frames[instruction.inline.0 as usize];
                    let byte_pc = frame
                        .instructions
                        .get(instruction.pc as usize)
                        .map(|metadata| metadata.byte_pc)
                        .ok_or(Unsupported::OperandShape("optimizing call byte PC"))?;
                    if instruction.inline != InlineId::ROOT
                        || !view.static_native_calls.contains_key(&byte_pc)
                    {
                        element_transition_instructions.push((
                            instruction.pc,
                            block,
                            instruction_index,
                        ));
                    }
                }
                // A spliced call is not lowered as a call: control enters the
                // callee's body. Its operands stay inputs so the emitter can
                // guard the callee's identity, and the continuation's merge —
                // not the call — defines its result.
                Op::Call if is_spliced_call(cfg, block, instruction) => {
                    if instruction.result.is_some()
                        || instruction.result_register.is_some()
                        || instruction.inputs.is_empty()
                        || instruction.inputs.len() != instruction.input_registers.len()
                        || reprs.representation(instruction.inputs[0]) != Representation::Tagged
                    {
                        return Err(Unsupported::Opcode(op));
                    }
                }
                // A spliced return hands its value to the continuation's merge
                // through the edge at the merge's representation, so it needs
                // no boxing of its own and never leaves the unit.
                Op::Return | Op::ReturnValue
                    if matches!(
                        cfg.blocks[block.0 as usize].terminator,
                        Terminator::InlineReturn { .. }
                    ) =>
                {
                    if instruction.result.is_some() || instruction.inputs.len() != 1 {
                        return Err(Unsupported::OperandShape("optimizing return shape"));
                    }
                    // Representation selection describes a bytecode return as
                    // leaving the native unit and therefore records numeric
                    // boxing. A spliced return stays inside the unit: its CFG
                    // edge performs the callee-result phi conversion directly.
                    for conversion in reprs.conversions().iter().filter(|conversion| {
                        conversion.inline == instruction.inline
                            && conversion.at_pc == instruction.pc
                    }) {
                        if conversion.operand_index != 0
                            || conversion.value != instruction.inputs[0]
                            || conversion.may_deopt
                            || !matches!(
                                conversion.kind,
                                ConversionKind::BoxInt32 | ConversionKind::BoxFloat64
                            )
                        {
                            return Err(Unsupported::OperandShape(
                                "optimizing inlined return conversion",
                            ));
                        }
                        allowed_conversions.insert((
                            conversion.inline,
                            conversion.at_pc,
                            conversion.operand_index,
                        ));
                    }
                }
                Op::ReturnUndefined
                    if matches!(
                        cfg.blocks[block.0 as usize].terminator,
                        Terminator::InlineReturn { .. }
                    ) =>
                {
                    if instruction.result.is_some() || !instruction.inputs.is_empty() {
                        return Err(Unsupported::OperandShape(
                            "optimizing return-undefined shape",
                        ));
                    }
                }
                // A captured-binding write is a leaf with a write barrier; the
                // checked form throws on a TDZ write. The value materializes
                // into its window slot for the stub.
                Op::StoreUpvalue | Op::StoreUpvalueChecked => {
                    if instruction.result.is_some()
                        || instruction.inputs.len() != 1
                        || instruction.input_registers.len() != 1
                    {
                        return Err(Unsupported::Opcode(op));
                    }
                    check_tagged_inputs(instruction, reprs, &mut allowed_conversions)?;
                }
                // A captured-binding read is a leaf: it reaches the upvalue
                // cell through pointers, never allocates or runs JS, and only
                // a TDZ read throws. No safepoint or materialization is owed;
                // the destination reloads from the window after the call.
                Op::LoadUpvalue => {
                    let result = instruction
                        .result
                        .ok_or(Unsupported::OperandShape("upvalue-load result"))?;
                    if reprs.representation(result) != Representation::Tagged
                        || instruction.result_register.is_none()
                        || !instruction.inputs.is_empty()
                    {
                        return Err(Unsupported::Opcode(op));
                    }
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
                    // A provably-boolean condition compares directly; any other
                    // tagged value reduces through the total `ToBoolean` sequence
                    // (numbers/bool/null/undefined inline, heap cells via the leaf
                    // probe) at emit time.
                    if reprs.representation(instruction.inputs[0]) != Representation::Tagged {
                        return Err(Unsupported::Opcode(op));
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
                        conversion.inline == instruction.inline
                            && conversion.at_pc == instruction.pc
                            && conversion.operand_index == 0
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
                    allowed_conversions.insert((instruction.inline, instruction.pc, 0));
                }
                Op::ReturnUndefined => {
                    if instruction.result.is_some() || !instruction.inputs.is_empty() {
                        return Err(Unsupported::OperandShape(
                            "optimizing return-undefined shape",
                        ));
                    }
                }
                other => return Err(Unsupported::Opcode(other)),
            }
        }
    }

    if reprs.conversions().iter().any(|conversion| {
        !allowed_conversions.contains(&(
            conversion.inline,
            conversion.at_pc,
            conversion.operand_index,
        ))
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
        guarded_numeric_uses.push(GuardedUse { use_pc });
    }
    guarded_numeric_uses.sort_by_key(|guarded| guarded.use_pc);
    guarded_numeric_uses.dedup_by_key(|guarded| guarded.use_pc);
    // A reentrant stub addresses its operands as indices into the *caller's*
    // interpreter register window. A spliced callee has no window of its own
    // until its frame is reified at a deopt exit, so those indices would name
    // the caller's registers and corrupt them. Splicing is therefore confined
    // to bodies the tier lowers entirely into machine registers.
    for &(_, block, _) in &element_transition_instructions {
        if cfg.blocks[block.0 as usize].inline != InlineId::ROOT {
            return Err(Unsupported::OperandShape(
                "optimizing subset rejects a spliced frame that needs a register window",
            ));
        }
    }
    element_transition_instructions.sort_unstable_by_key(|&(pc, _, _)| pc);
    element_transition_instructions.dedup_by_key(|instruction| instruction.0);
    let element_transitions = build_element_transition_sites(
        ssa,
        liveness,
        reprs,
        frame_states,
        element_transition_instructions,
    )?;
    let osr_entries =
        build_osr_entry_sites(cfg, ssa, liveness, frame_states, hoisted_loop_headers)?;
    let cached_method_guard = cached_method_guard_site(tree, cfg, ssa, &back_edges);
    Ok(Eligibility {
        guarded_uses: guarded_numeric_uses,
        back_edges,
        osr_entries,
        element_transitions,
        insufficient_feedback,
        cached_method_guard,
    })
}

pub(super) fn build_osr_entry_sites(
    cfg: &ControlFlowGraph,
    ssa: &SsaFunction,
    liveness: &Liveness,
    frame_states: &FrameStateTable,
    hoisted_loop_headers: &BTreeSet<BlockId>,
) -> Result<BTreeMap<BlockId, OsrEntrySite>, Unsupported> {
    let mut sites = BTreeMap::new();
    // Only the root frame's headers are OSR targets: the interpreter requests
    // OSR by the root function's PC, and a PC inside a spliced callee is not in
    // that namespace. A hot loop inside a spliced body runs compiled from the
    // unit's entry instead.
    for block in cfg
        .blocks
        .iter()
        .filter(|block| block.is_loop_header && block.inline == InlineId::ROOT)
        // Entering a hoisted loop here would skip the pre-header that computes
        // what its body reads.
        .filter(|block| !hoisted_loop_headers.contains(&block.id))
    {
        let frame_state =
            frame_states
                .at(InlineId::ROOT, block.start_pc)
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

pub(super) fn build_element_transition_sites(
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
        // Eligibility confines reentrant transitions to the root frame — a
        // spliced callee has no interpreter window for the stub to address.
        let frame_state = frame_states
            .at(InlineId::ROOT, pc)
            .ok_or(Unsupported::OperandShape(
                "optimizing element-transition abstract frame state",
            ))?;
        let live_after = liveness
            .live_after_instruction(ssa, block, instruction_index)
            .ok_or(Unsupported::OperandShape(
                "optimizing element-transition live-out boundary",
            ))?;
        let result = instruction.result;
        let mut tagged_live_across = Vec::new();
        for value in live_after {
            if Some(value) == result || reprs.representation(value) != Representation::Tagged {
                continue;
            }
            // Values the deopt table rematerializes as literals own no machine
            // home: an `Uninitialized` register and a structurally dead phi are
            // written by neither the prologue, a block prelude, nor an edge
            // move. Materializing one would publish uninitialized native-stack
            // bits into an interpreter register slot the collector traces, and
            // reloading one would overwrite a home nothing ever established.
            if rematerialized_deopt_slot(ssa, reprs, Some(value)).is_some() {
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
        for (&value, &register) in instruction.inputs.iter().zip(&instruction.input_registers) {
            if reprs.representation(value) == Representation::Tagged {
                root_registers.insert(register);
            }
        }
        if instruction.op == SsaOp::Bytecode(Op::StoreProperty)
            && root_registers.contains(
                &instruction
                    .result_register
                    .expect("eligibility checked property-store scratch"),
            )
        {
            return Err(Unsupported::OperandShape(
                "optimizing store-transition scratch aliases a tagged root",
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

pub(super) fn frame_register_for_value(
    frame_state: &AbstractFrameState,
    value: ValueId,
) -> Option<u16> {
    frame_state
        .registers
        .iter()
        .position(|candidate| *candidate == Some(value))
        .and_then(|register| u16::try_from(register).ok())
}

/// The silent-fallback reason for an instruction this backend cannot lower.
pub(super) fn instruction_unsupported(instruction: &SsaInstr) -> Unsupported {
    instruction.op.bytecode().map_or(
        Unsupported::OperandShape("optimizing primitive node shape"),
        Unsupported::Opcode,
    )
}

pub(super) fn check_tagged_inputs(
    instruction: &SsaInstr,
    reprs: &ReprMap,
    allowed_conversions: &mut BTreeSet<(InlineId, u32, usize)>,
) -> Result<(), Unsupported> {
    for (operand_index, &input) in instruction.inputs.iter().enumerate() {
        let expected_kind = match reprs.representation(input) {
            Representation::Tagged => continue,
            Representation::Int32 => ConversionKind::BoxInt32,
            Representation::Float64 => ConversionKind::BoxFloat64,
        };
        let conversion = reprs.conversions().iter().find(|conversion| {
            conversion.inline == instruction.inline
                && conversion.at_pc == instruction.pc
                && conversion.operand_index == operand_index
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
        allowed_conversions.insert((instruction.inline, instruction.pc, operand_index));
    }
    Ok(())
}

pub(super) fn check_numeric_inputs(
    instruction: &SsaInstr,
    ssa: &SsaFunction,
    reprs: &ReprMap,
    required: Representation,
    guarded_uses: &mut BTreeMap<(u32, ValueId), Option<u32>>,
    allowed_conversions: &mut BTreeSet<(InlineId, u32, usize)>,
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
            _ => return Err(instruction_unsupported(instruction)),
        };
        let conversion = reprs.conversions().iter().find(|conversion| {
            conversion.inline == instruction.inline
                && conversion.at_pc == instruction.pc
                && conversion.operand_index == operand_index
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
            return Err(instruction_unsupported(instruction));
        }
        if may_deopt {
            // A checked tagged->numeric conversion guards the value and can
            // deoptimize, so the value must be reconstructible: a parameter
            // reconstructs from the incoming argument, and every bytecode
            // operation result reconstructs from the destination register the
            // operation writes. A phi, an inline result, an exception input, or
            // an `Uninitialized` register owns no such home and stays out.
            //
            // Enumerating producer opcodes here instead used to work only
            // because a coercion opcode always sat between the producer and the
            // arithmetic that consumed it. The operators now run their own
            // ToPrimitive / ToNumeric, so operands arrive straight from a local,
            // a call, or a property load.
            let parameter_index = match ssa.values[input.0 as usize].def {
                ValueDef::Param { index, .. } => Some(index),
                ValueDef::Op { .. } => None,
                _ => return Err(instruction_unsupported(instruction)),
            };
            guarded_uses.insert((instruction.pc, input), parameter_index);
        }
        allowed_conversions.insert((instruction.inline, instruction.pc, operand_index));
    }
    Ok(())
}

pub(super) fn is_boolean_value(ssa: &SsaFunction, value: ValueId) -> bool {
    matches!(
        ssa.values[value.0 as usize].def,
        ValueDef::Op {
            op: SsaOp::Bytecode(
                Op::LoadTrue
                    | Op::LoadFalse
                    | Op::LessThan
                    | Op::LessEq
                    | Op::GreaterThan
                    | Op::GreaterEq
                    | Op::Equal
                    | Op::NotEqual,
            ),
            ..
        }
    )
}

/// Whether `instructions[index]` can leave its numeric comparison in flags for
/// the immediately following `JumpIf*`.
///
/// The branch materializes the proven boolean separately on both CFG edges, so
/// later exact-PC deopt states still see the bytecode destination even when it
/// is otherwise only frame-state live. Cold-feedback deopts cannot participate:
/// they resume at the comparison/branch boundary before an edge value exists.
pub(super) fn fused_numeric_compare_at(
    tree: &InlineTree,
    instructions: &[SsaInstr],
    index: usize,
    insufficient_feedback: &BTreeSet<(InlineId, u32)>,
) -> bool {
    let Some(comparison) = instructions.get(index) else {
        return false;
    };
    let Some(branch) = instructions.get(index.saturating_add(1)) else {
        return false;
    };
    if !matches!(
        comparison.op,
        SsaOp::Bytecode(
            Op::LessThan | Op::LessEq | Op::GreaterThan | Op::GreaterEq | Op::Equal | Op::NotEqual
        )
    ) || !matches!(branch.op, SsaOp::Bytecode(Op::JumpIfTrue | Op::JumpIfFalse))
        || comparison.inline != branch.inline
        || insufficient_feedback.contains(&(comparison.inline, comparison.pc))
        || insufficient_feedback.contains(&(branch.inline, branch.pc))
    {
        return false;
    }
    let Some(result) = comparison.result else {
        return false;
    };
    if branch.inputs.as_ref() != [result] {
        return false;
    }
    let feedback = frame_feedback(tree, comparison);
    feedback.is_int32_only() || feedback.is_numeric_only()
}

/// Reduce the tagged `Value` in `x9` to `VALUE_TRUE` / `VALUE_FALSE` in `x9`
/// per §7.1.2 `ToBoolean`. Int32 and boxed doubles (including `±0`/NaN),
/// booleans, `null`, and `undefined` decide inline; every heap cell resolves
/// through the total leaf `ToBoolean` probe, whose only miss is an isolate-less
/// null heap (never on a live VM) and side-exits at `bail`. Clobbers
/// `x14`/`x15` and the leaf-call argument registers.
pub(super) fn emit_truthiness_reduce(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    to_boolean_entry: ResolvedRuntimeEntry,
    bail: DynamicLabel,
) {
    let int_case = ops.new_dynamic_label();
    let double_case = ops.new_dynamic_label();
    let truthy = ops.new_dynamic_label();
    let falsy = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; and x14, x9, x15
        ; cmp x14, x15
        ; b.eq =>int_case                       // all tag bits → int32
        ; cbnz x14, =>double_case               // some tag bits → boxed double
        ; cmp x9, VALUE_TRUE as u32
        ; b.eq =>truthy
        ; cmp x9, VALUE_FALSE as u32
        ; b.eq =>falsy
        ; cmp x9, VALUE_NULL as u32
        ; b.eq =>falsy
        ; cmp x9, VALUE_UNDEFINED as u32
        ; b.eq =>falsy
        ; ldr x0, [x20, THREAD_OFFSET]
        ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
        ; mov x1, x9
        ; movz x2, #0
    );
    emit_runtime_entry(ops, relocations, 16, to_boolean_entry);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; and x1, x1, #0xff
        ; cbnz x1, =>bail
        ; mov x9, x0                            // boolean Value from the probe
        ; b =>done
        ; =>int_case
        ; cbz w9, =>falsy
        ; b =>truthy
        ; =>double_case
        ; movz x14, DOUBLE_OFFSET_HI16, lsl #48
        ; sub x14, x9, x14                      // raw f64 bit pattern
        ; cbz x14, =>falsy                      // +0.0
        ; movz x15, #0x8000, lsl #48
        ; cmp x14, x15
        ; b.eq =>falsy                          // -0.0
        ; movz x15, CANONICAL_NAN_HI16, lsl #48
        ; cmp x14, x15
        ; b.eq =>falsy                          // canonical NaN
        ; =>truthy
        ; movz x9, VALUE_TRUE as u32
        ; b =>done
        ; =>falsy
        ; movz x9, VALUE_FALSE as u32
        ; =>done
    );
}

pub(super) fn check_constant_result(
    instruction: &SsaInstr,
    reprs: &ReprMap,
) -> Result<(), Unsupported> {
    let result = instruction
        .result
        .ok_or(Unsupported::OperandShape("optimizing constant result"))?;
    if !instruction.inputs.is_empty() || reprs.representation(result) != Representation::Int32 {
        return Err(instruction_unsupported(instruction));
    }
    Ok(())
}

pub(super) fn check_number_constant_result(
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
        return Err(instruction_unsupported(instruction));
    }
    Ok(())
}

pub(super) fn check_boolean_result(
    instruction: &SsaInstr,
    reprs: &ReprMap,
) -> Result<(), Unsupported> {
    let result = instruction
        .result
        .ok_or(Unsupported::OperandShape("optimizing boolean result"))?;
    if !instruction.inputs.is_empty() || reprs.representation(result) != Representation::Tagged {
        return Err(instruction_unsupported(instruction));
    }
    Ok(())
}

pub(super) fn check_tagged_constant_result(
    instruction: &SsaInstr,
    reprs: &ReprMap,
) -> Result<(), Unsupported> {
    let result = instruction.result.ok_or(Unsupported::OperandShape(
        "optimizing tagged constant result",
    ))?;
    if !instruction.inputs.is_empty() || reprs.representation(result) != Representation::Tagged {
        return Err(instruction_unsupported(instruction));
    }
    Ok(())
}

pub(super) fn load_number(view: &JitCompileSnapshot, pc: u32) -> Result<f64, Unsupported> {
    view.instructions
        .get(pc as usize)
        .and_then(|instruction| instruction.load_number)
        .ok_or(Unsupported::OperandShape("optimizing LoadNumber metadata"))
}

pub(super) fn is_exact_i32(number: f64) -> bool {
    number.is_finite()
        && !(number == 0.0 && number.is_sign_negative())
        && number >= f64::from(i32::MIN)
        && number <= f64::from(i32::MAX)
        && number == f64::from(number as i32)
}

/// Name the exit an instruction deoptimizes through.
pub(super) fn deopt_exit_at(
    frame_states: &FrameStateTable,
    instruction: &SsaInstr,
) -> Result<DeoptExitId, Unsupported> {
    DeoptLowering::exit_at(frame_states, instruction.inline, instruction.pc).ok_or(
        Unsupported::OperandShape("optimizing deopt exit has no frame state"),
    )
}

pub(super) fn optimizing_direct_call_target_tier(
    target: &otter_vm::JitDirectCallee,
) -> otter_vm::JitDebugTier {
    match target.plan.tier {
        otter_vm::native_abi::NativeFrameKind::Baseline => otter_vm::JitDebugTier::Template,
        otter_vm::native_abi::NativeFrameKind::Optimizing => otter_vm::JitDebugTier::Optimizing,
        otter_vm::native_abi::NativeFrameKind::Interpreter => {
            unreachable!("interpreter has no entry-capable code generation")
        }
    }
}

pub(super) fn optimizing_direct_call_event(
    call_kind: otter_vm::JitDirectCallKind,
    instruction_pc: u32,
    byte_pc: u32,
    target: &otter_vm::JitDirectCallee,
    target_index: u32,
    target_count: u32,
    outcome: otter_vm::JitDirectCallLoweringOutcome,
) -> otter_vm::JitCompilerDiagnostic {
    otter_vm::JitCompilerDiagnostic::DirectCallLowered {
        call_kind,
        instruction_pc,
        byte_pc,
        callee_function_id: target.plan.function_id,
        target_index,
        target_count,
        outcome,
    }
}
