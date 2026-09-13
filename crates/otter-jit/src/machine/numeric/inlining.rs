//! Bounded callee CFG splicing into the single numeric HIR.
//!
//! # Contents
//! - Plain/method and base-constructor admission from each candidate's own snapshot.
//! - Argument substitution, return joins and complete deopt activation chains.
//!
//! # Invariants
//! - Scalar, global-read and named-property bodies, including bounded fully spliced helper
//!   chains, are admitted. Base construction probes the shared nursery allocator;
//!   misses retain full construct linkage in an explicit sibling. Enclosing helpers
//!   retain that sibling with source-owned arguments and native parent publication. Other allocations
//!   and residual JavaScript calls retain ordinary call linkage. Named-property cold
//!   calls publish exact inline frames without replaying completed effects.
//! - Identity guards precede allocation. Constructor parameter guards run after
//!   successful allocation and reconstruct that same receiver and new.target.
//!   Body exits rebuild the caller after its call and the exact callee operation.
//!   Object returns replace the receiver; primitive returns substitute it.
//! - Caller CFG order and backedge identities survive insertion. Callee loops
//!   and protected caller sites require further CFG admission and stay calls.
//! - A rejected candidate cannot partially mutate the caller graph or publish
//!   successful descendant diagnostics. Descendant entry operands and method
//!   target indices are remapped into the caller alongside ordinary SSA values.
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
    splice_tree(
        function,
        view,
        capture_events,
        &mut vec![view.code_block.id],
    )
}

fn splice_tree(
    function: &mut NumericFunction,
    view: &JitCompileSnapshot,
    capture_events: bool,
    ancestry: &mut Vec<u32>,
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
        let Some(call_target) = function.direct_call_targets.get(target as usize) else {
            continue;
        };
        let candidate = match call_target.kind {
            NumericDirectCallKind::Plain | NumericDirectCallKind::Construct => {
                view.inline_callees.get(&byte_pc).map(|c| &*c.body)
            }
            NumericDirectCallKind::Method => view.inline_methods.get(&byte_pc).map(|c| &*c.body),
            _ => None,
        };
        let Some(candidate) = candidate else {
            continue;
        };
        let mut cost = 0;
        let mut nested_diagnostics = Vec::new();
        let outcome = (|| -> Result<(), String> {
            let target = function
                .direct_call_targets
                .get(target as usize)
                .ok_or("missing direct-call target")?;
            if exceptional_edge.is_some() {
                return Err("protected call site".into());
            }
            if target.candidates.len() != 1 {
                return Err("requires one call target".into());
            }
            if ancestry.contains(&candidate.code_block.id) || ancestry.len() > 3 {
                return Err("inline ancestry/depth budget".into());
            }
            if candidate.code_block.id != target.candidates[0].callee.plan.function_id {
                return Err("callee snapshot disagrees with target".into());
            }
            let mut body = NumericFunction::build(candidate)
                .map_err(|reason| format!("callee HIR: {reason:?}"))?;
            if body.nodes.len() > MAX_INLINE_NODES || body.blocks.len() > 8 {
                return Err("source body budget".into());
            }
            ancestry.push(candidate.code_block.id);
            nested_diagnostics = splice_tree(&mut body, candidate, capture_events, ancestry);
            ancestry.pop();
            cost = body.nodes.len() + 2 * usize::from(body.parameter_count) + 1;
            if target.kind == NumericDirectCallKind::Construct {
                cost += 3 + 2 * body
                    .blocks
                    .iter()
                    .filter(|block| matches!(block.terminator, NumericTerminator::Return(_)))
                    .count();
            }
            if function.nodes.len() - original_nodes + cost > MAX_ADDED_NODES {
                return Err("caller growth budget".into());
            }
            if let Some(node) = body.nodes.iter().copied().find(|&node| {
                !matches!(node, NumericNode::Parameter { .. } | NumericNode::This)
                    && !(target.kind == NumericDirectCallKind::Construct && is_new_target(node))
                    && map_body_node(node, &|v| v).is_none()
            }) {
                return Err(format!("unsupported callee operation: {node:?}"));
            }
            if body.nodes.iter().any(|node| match node {
                NumericNode::DirectCall { target, .. } => body
                    .direct_call_targets
                    .get(*target as usize)
                    .is_none_or(|target| {
                        target.kind != NumericDirectCallKind::Construct
                            || target.candidates.len() != 1
                    }),
                _ => false,
            }) {
                return Err("residual call requires inline activation publication".into());
            }
            if body.blocks.iter().enumerate().any(|(i, b)| {
                b.successors.iter().any(|&s| s <= i)
                    || matches!(b.terminator, NumericTerminator::Throw(_))
            }) {
                return Err("callee requires loop or throw CFG".into());
            }
            let plan = &target.candidates[0].callee.plan;
            if plan.own_upvalue_count != 0 || plan.needs_incoming_arguments {
                return Err("callee requires activation entry setup".into());
            }
            if target.kind == NumericDirectCallKind::Method
                && target.candidates[0].guard.as_ref()
                    != view.inline_methods.get(&byte_pc).map(|m| &m.guard)
            {
                return Err("method snapshot disagrees with guard".into());
            }
            if target.kind == NumericDirectCallKind::Construct {
                let allocation = target.candidates[0]
                    .callee
                    .receiver_allocation
                    .ok_or("constructor has no nursery allocation program")?;
                if allocation.new_target_function_id != candidate.code_block.id {
                    return Err("constructor allocation disagrees with target".into());
                }
                if plan.is_derived_constructor {
                    return Err("constructor requires derived entry".into());
                }
            }
            let this_mode = plan.this_mode;
            let mut proposed = function.clone();
            splice_one(
                &mut proposed,
                view,
                candidate,
                NumericValue(index),
                &body,
                this_mode,
            )
            .ok_or("scalar splice frame or argument contract")?;
            if proposed.nodes.len() - original_nodes > MAX_ADDED_NODES {
                return Err("caller growth budget".into());
            }
            *function = proposed;
            Ok(())
        })();
        if capture_events {
            if outcome.is_ok() {
                diagnostics.extend(nested_diagnostics);
            }
            diagnostics.push(otter_vm::JitCompilerDiagnostic::InlineLowered {
                parent_function_id: view.code_block.id,
                instruction_pc: logical_pc,
                byte_pc,
                callee_function_id: candidate.code_block.id,
                depth: ancestry.len() as u32,
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
    callee_view: &JitCompileSnapshot,
    call: NumericValue,
    body: &NumericFunction,
    this_mode: otter_vm::JitDirectCallThisMode,
) -> Option<()> {
    let NumericNode::DirectCall {
        source,
        target,
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
    let source = boxed(hir, source, &mut prefix);
    let call_target = hir.direct_call_targets.get(target as usize)?;
    let method = call_target.kind == NumericDirectCallKind::Method;
    let construct = call_target.kind == NumericDirectCallKind::Construct;
    let allocation = if construct {
        Some(call_target.candidates.first()?.callee.receiver_allocation?)
    } else {
        None
    };
    let guard = push(
        hir,
        if method {
            NumericNode::InlineMethodGuard { source, target }
        } else if construct {
            NumericNode::InlineConstructGuard {
                source,
                function_id: body.function_id,
            }
        } else {
            NumericNode::InlineCallGuard {
                source,
                function_id: body.function_id,
                this_mode,
            }
        },
    );
    let (callable, mut this) = if method || construct {
        (guard, source)
    } else {
        (source, guard)
    };
    prefix.push(guard);
    let receiver_hit = allocation.map(|plan| {
        this = push(
            hir,
            NumericNode::ConstructReceiver {
                source,
                plan,
                byte_pc,
            },
        );
        prefix.push(this);
        let hit = push(hir, NumericNode::ConstructReceiverHit(this));
        prefix.push(hit);
        hit
    });
    let mut guard_state = call_state.clone();
    guard_state.point = NumericFramePoint::Node(guard);
    let mut parents = call_state.frames.to_vec();
    let parent = parents.last_mut()?;
    parent.byte_pc = after_pc;
    *parent.slots.get_mut(usize::from(destination))? = NumericFrameSlot::Undefined;
    let entry = DeoptFrameEntry {
        new_target: if construct {
            NumericFrameSlot::Value(source)
        } else {
            NumericFrameSlot::Undefined
        },
        return_register: destination,
        this: NumericFrameSlot::Value(this),
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
        byte_pc: callee_view.instructions.first()?.byte_pc,
        entry: Some(entry),
        slots: entry_slots.into(),
    });
    let mut extra_states = vec![guard_state];
    let cold_call = if construct {
        let node = hir.nodes[call.0];
        let value = push(hir, node);
        let mut state = call_state.clone();
        state.point = NumericFramePoint::Node(value);
        extra_states.push(state);
        Some(value)
    } else {
        None
    };
    let mut entry_decodes = Vec::new();
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
                        if construct {
                            entry_decodes.push(decoded);
                        } else {
                            prefix.push(decoded);
                        }
                        extra_states.push(NumericFrameState {
                            point: NumericFramePoint::Node(decoded),
                            frames: entry_frames.clone().into(),
                        });
                        decoded
                    }
                    _ => return None,
                };
            }
            NumericNode::This => mapping[index] = this,
            node if construct && is_new_target(node) => mapping[index] = source,
            _ => {
                mapping[index] = push(
                    hir,
                    NumericNode::TaggedConstant(otter_vm::Value::undefined().to_bits()),
                )
            }
        }
    }
    let target_base = u16::try_from(hir.direct_call_targets.len()).ok()?;
    u16::try_from(
        hir.direct_call_targets
            .len()
            .checked_add(body.direct_call_targets.len())?,
    )
    .ok()?;
    hir.direct_call_targets
        .extend(body.direct_call_targets.iter().cloned());
    let map = |value: NumericValue| mapping[value.0];
    let operand_base = u32::try_from(hir.operand_values.len()).ok()?;
    u32::try_from(
        hir.operand_values
            .len()
            .checked_add(body.operand_values.len())?,
    )
    .ok()?;
    hir.operand_values
        .extend(body.operand_values.iter().copied().map(map));
    for (index, &node) in body.nodes.iter().enumerate() {
        if !matches!(node, NumericNode::Parameter { .. } | NumericNode::This)
            && !(construct && is_new_target(node))
        {
            let mut mapped = map_body_node(node, &map)?;
            match &mut mapped {
                NumericNode::InlineMethodGuard { target, .. } => {
                    *target = target.checked_add(target_base)?
                }
                NumericNode::DirectCall {
                    target,
                    arguments: NumericDirectCallArguments::Fixed { start, .. },
                    ..
                } => {
                    *target = target.checked_add(target_base)?;
                    *start = start.checked_add(operand_base)?;
                }
                _ => {}
            }
            hir.nodes[mapping[index].0] = mapped;
        }
    }
    for (node, site) in &body.property_sites {
        hir.property_sites.insert(map(*node), site.clone());
    }
    for (node, site) in &body.constructor_field_sites {
        hir.constructor_field_sites.insert(map(*node), site.clone());
    }
    for state in &body.frame_states {
        let NumericFramePoint::Node(node) = state.point else {
            return None;
        };
        if matches!(
            body.nodes[node.0],
            NumericNode::Parameter { .. } | NumericNode::This
        ) || (construct && is_new_target(body.nodes[node.0]))
        {
            continue;
        }
        let mut frames = parents.clone();
        for (index, original) in state.frames.iter().enumerate() {
            let mut frame = original.clone();
            if index == 0 {
                if frame.entry.is_some() {
                    return None;
                }
                frame.entry = Some(entry);
            } else {
                let nested = frame.entry.as_mut()?;
                for slot in [
                    &mut nested.this,
                    &mut nested.closure,
                    &mut nested.new_target,
                ] {
                    if let NumericFrameSlot::Value(value) = slot {
                        *value = map(*value);
                    }
                }
            }
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
    let added_blocks = body.blocks.len() + 1 + usize::from(construct);
    let cold_block = block_index + body.blocks.len() + 1;
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
    let original_blocks = hir.blocks.clone();
    for (index, old) in original_blocks.iter().enumerate() {
        if index != block_index {
            let mut block = old.clone();
            block.successors.iter_mut().for_each(|s| *s = remap(*s));
            blocks.push(block);
            continue;
        }
        let mut before = old_block.clone();
        before.nodes = prefix.clone();
        before.terminator = if let Some(condition) = receiver_hit {
            NumericTerminator::Branch {
                condition,
                when_true: true,
            }
        } else {
            NumericTerminator::Jump
        };
        before.successors = if construct {
            vec![block_index + 1, cold_block]
        } else {
            vec![block_index + 1]
        };
        before.successor_arguments = vec![vec![]; before.successors.len()];
        blocks.push(before);
        for (body_index, original) in body.blocks.iter().enumerate() {
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
                    ) && !(construct && is_new_target(body.nodes[n.0]))
                })
                .map(map)
                .collect();
            if body_index == 0 && construct {
                block.nodes.splice(0..0, entry_decodes.iter().copied());
            }
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
                    let value = if construct {
                        let returned = boxed(hir, map(value), &mut block.nodes);
                        let result = push(
                            hir,
                            NumericNode::BaseConstructResult {
                                result: returned,
                                receiver: this,
                            },
                        );
                        block.nodes.push(result);
                        result
                    } else {
                        map(value)
                    };
                    block.successor_arguments = vec![vec![value]];
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
        if let Some(cold_call) = cold_call {
            blocks.push(NumericBlock {
                logical_pc,
                osr_entry_allowed: false,
                predecessors: vec![],
                successors: vec![join],
                parameters: vec![],
                parameter_registers: vec![],
                successor_arguments: vec![vec![cold_call]],
                nodes: vec![cold_call],
                terminator: NumericTerminator::Jump,
            });
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

fn is_new_target(node: NumericNode) -> bool {
    matches!(
        node,
        NumericNode::CommittedValue {
            operation: super::CommittedValueOperation::Scalar(
                otter_vm::ScalarValueOp::LoadNewTarget
            ),
            inputs: [None, None],
            exceptional_edge: None,
            ..
        }
    )
}

fn map_body_node(
    node: NumericNode,
    map: &impl Fn(NumericValue) -> NumericValue,
) -> Option<NumericNode> {
    use NumericNode::*;
    Some(match node {
        Binding {
            semantics:
                semantics @ otter_bytecode::opcode_schema::BindingSemantics::Read(
                    otter_bytecode::opcode_schema::BindingRead::Global { .. },
                ),
            inputs,
            target,
            logical_pc,
            byte_pc,
            exceptional_edge: None,
        } => Binding {
            semantics,
            inputs: inputs.map(|input| input.map(map)),
            target,
            logical_pc,
            byte_pc,
            exceptional_edge: None,
        },
        InlineConstructGuard {
            source,
            function_id,
        } => InlineConstructGuard {
            source: map(source),
            function_id,
        },
        ConstructReceiver {
            source,
            plan,
            byte_pc,
        } => ConstructReceiver {
            source: map(source),
            plan,
            byte_pc,
        },
        ConstructReceiverHit(value) => ConstructReceiverHit(map(value)),
        BaseConstructResult { result, receiver } => BaseConstructResult {
            result: map(result),
            receiver: map(receiver),
        },
        DirectCall {
            source,
            target,
            arguments: arguments @ NumericDirectCallArguments::Fixed { .. },
            logical_pc,
            byte_pc,
            exceptional_edge: None,
        } => DirectCall {
            source: map(source),
            target,
            arguments,
            logical_pc,
            byte_pc,
            exceptional_edge: None,
        },
        InlineCallGuard {
            source,
            function_id,
            this_mode,
        } => InlineCallGuard {
            source: map(source),
            function_id,
            this_mode,
        },
        InlineMethodGuard { source, target } => InlineMethodGuard {
            source: map(source),
            target,
        },
        PropertyLoad {
            receiver,
            byte_pc,
            exotic_length,
            exceptional_edge: None,
        } => PropertyLoad {
            receiver: map(receiver),
            byte_pc,
            exotic_length,
            exceptional_edge: None,
        },
        BlockParameter(_) | TaggedConstant(_) | IntegerConstant(_) | BooleanConstant(_)
        | Constant(_) => node,
        TaggedToNumber(value) => TaggedToNumber(map(value)),
        TaggedToInt32(value) => TaggedToInt32(map(value)),
        WidenInt32(value) => WidenInt32(map(value)),
        WidenUint32(value) => WidenUint32(map(value)),
        ConstructorFieldStore {
            object,
            value,
            byte_pc,
        } => ConstructorFieldStore {
            object: map(object),
            value: map(value),
            byte_pc,
        },
        PropertyStore {
            receiver,
            value,
            byte_pc,
        } => PropertyStore {
            receiver: map(receiver),
            value: map(value),
            byte_pc,
        },
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
