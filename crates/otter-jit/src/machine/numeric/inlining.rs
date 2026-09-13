//! Bounded callee CFG splicing into the single numeric HIR.
//!
//! # Contents
//! - Plain numeric-body admission from each candidate's own compile snapshot.
//! - Argument substitution, return joins and complete deopt activation chains.
//!
//! # Invariants
//! - Only self-contained scalar bodies are admitted. Property, binding,
//!   allocation and JavaScript-call operations retain ordinary call linkage.
//! - Identity and parameter guards precede callee effects; body exits rebuild
//!   the caller after its call and the callee at its exact source instruction.
//! - Caller CFG order and backedge identities survive insertion. Callee loops
//!   and protected caller sites require further CFG admission and stay calls.
//! - A rejected candidate cannot partially mutate the caller graph.
//!
//! # See also
//! - `frame_state` — shared frame recipes and complete late-use liveness.

use super::hir::*;
use otter_bytecode::Operand;
use otter_vm::{
    JitCompileSnapshot,
    deopt::{DeoptFrame, DeoptFrameEntry},
};

const MAX_INLINE_NODES: usize = 64;
const MAX_ADDED_NODES: usize = 256;

/// Each scalar node costs one unit; parameter boxing/decoding costs at most
/// two, and callable/this guarding one. Diagnostics are published only after
/// the enclosing Machine pipeline succeeds.
pub(super) fn splice(
    function: &mut NumericFunction,
    view: &JitCompileSnapshot,
    capture_events: bool,
) -> Vec<otter_vm::JitCompilerDiagnostic> {
    let original_nodes = function.nodes.len();
    let mut diagnostics = Vec::new();
    for index in 0..original_nodes {
        let NumericNode::DirectCall {
            byte_pc,
            logical_pc,
            target,
            exceptional_edge,
            ..
        } = function.nodes[index]
        else {
            continue;
        };
        let Some(candidate) = view.inline_callees.get(&byte_pc) else {
            continue;
        };
        let mut cost = 0;
        let outcome = (|| -> Result<(), String> {
            let target = function
                .direct_call_targets
                .get(target as usize)
                .ok_or("missing direct-call target")?;
            if exceptional_edge.is_some() {
                return Err("protected call site".into());
            }
            if target.kind != NumericDirectCallKind::Plain || target.candidates.len() != 1 {
                return Err("requires one plain-call target".into());
            }
            if candidate.function_id() == view.code_block.id {
                return Err("recursive callee".into());
            }
            if candidate.function_id() != target.candidates[0].callee.plan.function_id {
                return Err("callee snapshot disagrees with target".into());
            }
            let body = NumericFunction::build(&candidate.body)
                .map_err(|reason| format!("callee HIR: {reason:?}"))?;
            cost = body.nodes.len() + 2 * usize::from(body.parameter_count) + 1;
            if body.nodes.len() > MAX_INLINE_NODES || body.blocks.len() > 8 {
                return Err("scalar body budget".into());
            }
            if function.nodes.len() - original_nodes + cost > MAX_ADDED_NODES {
                return Err("caller growth budget".into());
            }
            if let Some(node) = body.nodes.iter().copied().find(|&node| {
                !matches!(node, NumericNode::Parameter { .. } | NumericNode::This)
                    && map_scalar(node, &|v| v).is_none()
            }) {
                return Err(format!("non-scalar callee operation: {node:?}"));
            }
            if body.blocks.iter().enumerate().any(|(i, b)| {
                b.successors.iter().any(|&s| s <= i)
                    || matches!(b.terminator, NumericTerminator::Throw(_))
            }) {
                return Err("callee requires loop or throw CFG".into());
            }
            let this_mode = target.candidates[0].callee.plan.this_mode;
            let mut proposed = function.clone();
            splice_one(&mut proposed, view, NumericValue(index), &body, this_mode)
                .ok_or("scalar splice frame or argument contract")?;
            *function = proposed;
            Ok(())
        })();
        if capture_events {
            diagnostics.push(otter_vm::JitCompilerDiagnostic::InlineLowered {
                parent_function_id: view.code_block.id,
                instruction_pc: logical_pc,
                byte_pc,
                callee_function_id: candidate.function_id(),
                depth: 1,
                cost: cost as u32,
                outcome: match outcome {
                    Ok(()) => otter_vm::JitInlineLoweringOutcome::Inlined,
                    Err(reason) => otter_vm::JitInlineLoweringOutcome::Rejected { reason },
                },
            });
        }
    }
    diagnostics
}

fn push(hir: &mut NumericFunction, node: NumericNode) -> NumericValue {
    let value = NumericValue(hir.nodes.len());
    hir.nodes.push(node);
    value
}

fn boxed(
    hir: &mut NumericFunction,
    value: NumericValue,
    prefix: &mut Vec<NumericValue>,
) -> NumericValue {
    if hir.nodes[value.0].value_type() == NumericType::Tagged {
        return value;
    }
    let result = push(hir, NumericNode::BoxTagged(value));
    prefix.push(result);
    result
}

fn splice_one(
    hir: &mut NumericFunction,
    view: &JitCompileSnapshot,
    call: NumericValue,
    body: &NumericFunction,
    this_mode: otter_vm::JitDirectCallThisMode,
) -> Option<()> {
    let NumericNode::DirectCall {
        source,
        arguments: NumericDirectCallArguments::Fixed { start, count },
        logical_pc,
        byte_pc,
        ..
    } = hir.nodes[call.0]
    else {
        return None;
    };
    if count != u32::from(body.parameter_count) {
        return None;
    }
    let arguments = hir
        .operand_values
        .get(start as usize..(start as usize).checked_add(count as usize)?)?
        .to_vec();
    let instruction = view.instructions.get(logical_pc as usize)?;
    if instruction.byte_pc != byte_pc {
        return None;
    }
    let Operand::Register(destination) = instruction.operand(view.code_block.as_ref(), 0)? else {
        return None;
    };
    let after_pc = view.instructions.get(logical_pc as usize + 1)?.byte_pc;
    let state_index = hir
        .frame_states
        .iter()
        .position(|state| state.point == NumericFramePoint::Node(call))?;
    let call_state = hir.frame_states[state_index].clone();
    let block_index = hir
        .blocks
        .iter()
        .position(|block| block.nodes.contains(&call))?;
    let position = hir.blocks[block_index]
        .nodes
        .iter()
        .position(|&node| node == call)?;
    let old_block = hir.blocks[block_index].clone();
    let mut prefix = old_block.nodes[..position].to_vec();
    let callable = boxed(hir, source, &mut prefix);
    let guard = push(
        hir,
        NumericNode::InlineCallGuard {
            source: callable,
            function_id: body.function_id,
            this_mode,
        },
    );
    prefix.push(guard);
    let mut guard_state = call_state.clone();
    guard_state.point = NumericFramePoint::Node(guard);
    let mut parents = call_state.frames.to_vec();
    let parent = parents.last_mut()?;
    parent.byte_pc = after_pc;
    *parent.slots.get_mut(usize::from(destination))? = NumericFrameSlot::Undefined;
    let entry = DeoptFrameEntry {
        return_register: destination,
        this: NumericFrameSlot::Value(guard),
        closure: NumericFrameSlot::Value(callable),
    };
    let mut raw_arguments = Vec::new();
    for argument in arguments {
        raw_arguments.push(boxed(hir, argument, &mut prefix));
    }
    let mut entry_slots = vec![NumericFrameSlot::Undefined; usize::from(body.register_count)];
    for (slot, &argument) in entry_slots.iter_mut().zip(&raw_arguments) {
        *slot = NumericFrameSlot::Value(argument);
    }
    let mut entry_frames = parents.clone();
    entry_frames.push(DeoptFrame {
        function_id: body.function_id,
        byte_pc: view
            .inline_callees
            .get(&byte_pc)?
            .body
            .instructions
            .first()?
            .byte_pc,
        entry: Some(entry),
        slots: entry_slots.into(),
    });
    let mut extra_states = vec![guard_state];
    let mut mapping = vec![NumericValue(usize::MAX); body.nodes.len()];
    for (index, &node) in body.nodes.iter().enumerate() {
        match node {
            NumericNode::Parameter {
                register,
                value_type,
            } => {
                let argument = *raw_arguments.get(usize::from(register))?;
                mapping[index] = match value_type {
                    NumericType::Tagged => argument,
                    NumericType::Int32 | NumericType::Number => {
                        let decoded = push(
                            hir,
                            if value_type == NumericType::Int32 {
                                NumericNode::TaggedToInt32(argument)
                            } else {
                                NumericNode::TaggedToNumber(argument)
                            },
                        );
                        prefix.push(decoded);
                        extra_states.push(NumericFrameState {
                            point: NumericFramePoint::Node(decoded),
                            frames: entry_frames.clone().into(),
                        });
                        decoded
                    }
                    _ => return None,
                };
            }
            NumericNode::This => mapping[index] = guard,
            _ => {
                mapping[index] = push(
                    hir,
                    NumericNode::TaggedConstant(otter_vm::Value::undefined().to_bits()),
                )
            }
        }
    }
    let map = |value: NumericValue| mapping[value.0];
    for (index, &node) in body.nodes.iter().enumerate() {
        if !matches!(node, NumericNode::Parameter { .. } | NumericNode::This) {
            hir.nodes[mapping[index].0] = map_scalar(node, &map)?;
        }
    }
    for state in &body.frame_states {
        let NumericFramePoint::Node(node) = state.point else {
            return None;
        };
        if matches!(
            body.nodes[node.0],
            NumericNode::Parameter { .. } | NumericNode::This
        ) {
            continue;
        }
        let mut frames = parents.clone();
        for original in state.frames.iter() {
            let mut frame = original.clone();
            frame.entry = Some(entry);
            for slot in frame.slots.iter_mut() {
                if let NumericFrameSlot::Value(value) = slot {
                    *value = map(*value);
                }
            }
            frames.push(frame);
        }
        extra_states.push(NumericFrameState {
            point: NumericFramePoint::Node(map(node)),
            frames: frames.into(),
        });
    }
    let added_blocks = body.blocks.len() + 1;
    let join = block_index + added_blocks;
    let remap = |block: usize| {
        if block > block_index {
            block + added_blocks
        } else {
            block
        }
    };
    let outgoing = |block: usize| {
        if block == block_index {
            join
        } else {
            remap(block)
        }
    };
    let mut blocks = Vec::with_capacity(hir.blocks.len() + added_blocks);
    for (index, old) in hir.blocks.iter().enumerate() {
        if index != block_index {
            let mut block = old.clone();
            block.successors.iter_mut().for_each(|s| *s = remap(*s));
            blocks.push(block);
            continue;
        }
        let mut before = old_block.clone();
        before.nodes = prefix.clone();
        before.terminator = NumericTerminator::Jump;
        before.successors = vec![block_index + 1];
        before.successor_arguments = vec![vec![]];
        blocks.push(before);
        for original in &body.blocks {
            let mut block = original.clone();
            block.osr_entry_allowed = false;
            block.parameters = block.parameters.iter().copied().map(map).collect();
            block.nodes = original
                .nodes
                .iter()
                .copied()
                .filter(|n| {
                    !matches!(
                        body.nodes[n.0],
                        NumericNode::Parameter { .. } | NumericNode::This
                    )
                })
                .map(map)
                .collect();
            block.successor_arguments = original
                .successor_arguments
                .iter()
                .map(|edge| edge.iter().copied().map(map).collect())
                .collect();
            block.successors = original
                .successors
                .iter()
                .map(|s| block_index + 1 + s)
                .collect();
            block.terminator = match original.terminator {
                NumericTerminator::Return(value) => {
                    block.successors = vec![join];
                    block.successor_arguments = vec![vec![map(value)]];
                    NumericTerminator::Jump
                }
                NumericTerminator::Branch {
                    condition,
                    when_true,
                } => NumericTerminator::Branch {
                    condition: map(condition),
                    when_true,
                },
                NumericTerminator::Jump => NumericTerminator::Jump,
                NumericTerminator::Throw(_) => return None,
            };
            blocks.push(block);
        }
        let mut after = old_block.clone();
        after.osr_entry_allowed = false;
        after.parameters = vec![call];
        after.parameter_registers = vec![destination];
        after.nodes = old_block.nodes[position + 1..].to_vec();
        after.successors.iter_mut().for_each(|s| *s = remap(*s));
        blocks.push(after);
    }
    for block in &mut blocks {
        block.predecessors.clear();
    }
    for predecessor in 0..blocks.len() {
        for successor in blocks[predecessor].successors.clone() {
            if !blocks[successor].predecessors.contains(&predecessor) {
                blocks[successor].predecessors.push(predecessor);
            }
        }
    }
    hir.frame_states.remove(state_index);
    for state in &mut hir.frame_states {
        if let NumericFramePoint::Backedge { predecessor, .. } = &mut state.point {
            *predecessor = outgoing(*predecessor);
        }
    }
    hir.frame_states.extend(extra_states);
    hir.nodes[call.0] = NumericNode::BlockParameter(NumericType::Tagged);
    hir.blocks = blocks;
    hir.arithmetic_op_count += body.arithmetic_op_count;
    Some(())
}

fn map_scalar(
    node: NumericNode,
    map: &impl Fn(NumericValue) -> NumericValue,
) -> Option<NumericNode> {
    use NumericNode::*;
    Some(match node {
        BlockParameter(_) | TaggedConstant(_) | IntegerConstant(_) | BooleanConstant(_)
        | Constant(_) => node,
        TaggedToNumber(value) => TaggedToNumber(map(value)),
        TaggedToInt32(value) => TaggedToInt32(map(value)),
        WidenInt32(value) => WidenInt32(map(value)),
        WidenUint32(value) => WidenUint32(map(value)),
        FloatToInt32(value) => FloatToInt32(map(value)),
        BooleanToInt32(value) => BooleanToInt32(map(value)),
        IntegerNeg(value) => IntegerNeg(map(value)),
        IntegerNot(value) => IntegerNot(map(value)),
        Neg(value) => Neg(map(value)),
        IntegerToBoolean(value) => IntegerToBoolean(map(value)),
        FloatToBoolean(value) => FloatToBoolean(map(value)),
        BooleanNot(value) => BooleanNot(map(value)),
        BoxTagged(value) => BoxTagged(map(value)),
        IntegerAdd(left, right) => IntegerAdd(map(left), map(right)),
        IntegerSub(left, right) => IntegerSub(map(left), map(right)),
        IntegerMul(left, right) => IntegerMul(map(left), map(right)),
        IntegerAnd(left, right) => IntegerAnd(map(left), map(right)),
        IntegerOr(left, right) => IntegerOr(map(left), map(right)),
        IntegerXor(left, right) => IntegerXor(map(left), map(right)),
        IntegerShiftLeft(left, right) => IntegerShiftLeft(map(left), map(right)),
        IntegerShiftRight(left, right) => IntegerShiftRight(map(left), map(right)),
        IntegerShiftRightLogical(left, right) => IntegerShiftRightLogical(map(left), map(right)),
        IntegerEqual(left, right) => IntegerEqual(map(left), map(right)),
        IntegerNotEqual(left, right) => IntegerNotEqual(map(left), map(right)),
        IntegerLessThan(left, right) => IntegerLessThan(map(left), map(right)),
        IntegerLessEqual(left, right) => IntegerLessEqual(map(left), map(right)),
        IntegerGreaterThan(left, right) => IntegerGreaterThan(map(left), map(right)),
        IntegerGreaterEqual(left, right) => IntegerGreaterEqual(map(left), map(right)),
        Add(left, right) => Add(map(left), map(right)),
        Sub(left, right) => Sub(map(left), map(right)),
        Mul(left, right) => Mul(map(left), map(right)),
        Div(left, right) => Div(map(left), map(right)),
        Rem(left, right) => Rem(map(left), map(right)),
        Pow(left, right) => Pow(map(left), map(right)),
        LessThan(left, right) => LessThan(map(left), map(right)),
        Equal(left, right) => Equal(map(left), map(right)),
        NotEqual(left, right) => NotEqual(map(left), map(right)),
        LessEqual(left, right) => LessEqual(map(left), map(right)),
        GreaterThan(left, right) => GreaterThan(map(left), map(right)),
        GreaterEqual(left, right) => GreaterEqual(map(left), map(right)),
        IntegerAddImmediate(value, immediate) => IntegerAddImmediate(map(value), immediate),
        IntegerSubImmediate(value, immediate) => IntegerSubImmediate(map(value), immediate),
        IntegerAndImmediate(value, immediate) => IntegerAndImmediate(map(value), immediate),
        IntegerLessThanImmediate(value, immediate) => {
            IntegerLessThanImmediate(map(value), immediate)
        }
        IntegerEqualImmediate(value, immediate) => IntegerEqualImmediate(map(value), immediate),
        IntegerNotEqualImmediate(value, immediate) => {
            IntegerNotEqualImmediate(map(value), immediate)
        }
        _ => return None,
    })
}
