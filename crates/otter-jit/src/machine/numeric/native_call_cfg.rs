//! Native identity probes, generated hits and committed method/resolved-call
//! siblings.
//!
//! # Contents
//! - Selection eligibility from existing method and static-native snapshots.
//! - Ordinary CacheIR receiver/holder/slot proofs, pinned intrinsic-prototype
//!   proofs for exotic receivers, and native identity probes.
//! - [`HitKind`]: a pure Int32 Math hit or a bounded no-allocation leaf hit,
//!   joined with the canonical boxed completion.
//!
//! # Invariants
//! - Every guard, arithmetic and leaf miss precedes effects and enters one cold
//!   call, which performs the complete JavaScript operation exactly once.
//! - Shaped receiver and holder state exclude descriptor overrides and exotic
//!   lookup before a shape-derived method slot is read; symbol sidecars remain
//!   eligible. An exotic receiver is proven by type tag and latch, and its
//!   pinned prototype by fast mode and shape; the builtin identity guard then
//!   rejects any slot that no longer holds the declared data function.
//! - Resolved calls consume their loaded callee after argument evaluation; a
//!   cold miss never repeats the property lookup that produced it.
//! - Only the cold sibling publishes roots, a frame state or a safepoint and
//!   enters the VM. A leaf hit calls a declared entry that cannot allocate,
//!   collect, throw or reenter JavaScript, so its operands need no roots.
//! - The cold call retains its source function and pre-call frame state.
//!
//! # See also
//! - `super::property_cfg` — the corresponding named-property CFG.
//! - `crate::machine::native_leaf` — authoritative Int32 Math declarations.

use super::*;

#[derive(Clone, Copy)]
pub(super) struct Blocks {
    pub hit: MachineBlock,
    pub cold: MachineBlock,
    pub join: MachineBlock,
}

#[derive(Clone, Copy)]
pub(super) struct Values {
    pub result: MachineValue,
    pub hit: MachineValue,
    pub payload: MachineValue,
    pub cold_payload: MachineValue,
}

/// Generated completion selected for one native call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HitKind {
    /// Proven Int32 `Math.abs` / `max` / `min` replacement.
    Int32Math,
    /// Declared no-allocation leaf entry returning a boxed value or a miss.
    Leaf,
}

impl Values {
    pub fn new(representations: &mut Vec<MachineRepresentation>, kind: HitKind) -> Self {
        Self {
            result: push_value(
                representations,
                match kind {
                    HitKind::Int32Math => MachineRepresentation::Int32,
                    HitKind::Leaf => MachineRepresentation::Tagged,
                },
            ),
            hit: push_value(representations, MachineRepresentation::Boolean),
            payload: push_value(representations, MachineRepresentation::Tagged),
            cold_payload: push_value(representations, MachineRepresentation::Tagged),
        }
    }
}

/// Generated hit for a guarded method site, or `None` when it stays generic.
///
/// A shaped receiver with an Int32 Math declaration and Int32 operands keeps
/// its pure hit. Every other declared no-allocation leaf is called directly;
/// an exotic receiver is the entry's first operand, so at most one argument
/// fits its two words. A shaped site never passes its receiver, so it admits
/// no declaration that reads `this`.
pub(super) fn method_hit(
    view: &JitCompileSnapshot,
    call: otter_vm::JitGuardedMethodCall,
    argument_count: usize,
    int32_operands: bool,
) -> Option<HitKind> {
    use otter_vm::JitMethodHolder;
    let receiver_word = match (call.receiver, call.holder) {
        (
            otter_vm::JitGuardedReceiver::Shape { shape },
            JitMethodHolder::Receiver | JitMethodHolder::Shape(_),
        ) if shape != 0 => 0,
        (
            otter_vm::JitGuardedReceiver::Exotic(target),
            JitMethodHolder::Shape(_) | JitMethodHolder::Dictionary(_),
        ) if target.is_generated_receiver() => 1,
        _ => return None,
    };
    if matches!(
        call.holder,
        JitMethodHolder::Shape(0) | JitMethodHolder::Dictionary(0)
    ) {
        return None;
    }
    if call.safepoint_id != otter_vm::native_abi::NO_SAFEPOINT
        || !call.method_value_byte.is_multiple_of(8)
        || view.cage_base == 0
        || view.native_ref_byte == 0
        || argument_count != usize::from(call.argument_count)
    {
        return None;
    }
    if receiver_word == 0
        && otter_vm::jit_static_native::jit_leaf_builtin(call.entry_stub_id)
            .is_some_and(|declaration| declaration.this_operand)
    {
        return None;
    }
    if receiver_word == 0
        && int32_operands
        && super::super::native_leaf::supports_int32(call.entry_stub_id)
        && super::super::native_leaf::supports_site(
            view,
            otter_vm::JitStaticNativeCall {
                leaf_stub_id: call.entry_stub_id,
                builtin_native_ref: call.builtin_native_ref,
                argument_count: call.argument_count,
            },
            argument_count,
        )
    {
        return Some(HitKind::Int32Math);
    }
    super::super::native_leaf::supports_leaf_probe(
        call.entry_stub_id,
        receiver_word + argument_count,
    )
    .then_some(HitKind::Leaf)
}

/// Generated hit already admitted for the native call ending `block`.
pub(super) fn hit_kind(hir: &NumericFunction, block: usize) -> HitKind {
    match site(hir, block).map(|node| hir.nodes[node.0]) {
        Some(NumericNode::NativeCall { hit, .. }) => hit,
        _ => HitKind::Int32Math,
    }
}

/// Generated hit for an explicit-receiver call whose callee was already
/// loaded, or `None` when it stays generic.
///
/// The site owns both its callee and its `this`, so any declared leaf
/// qualifies: Int32 Math with Int32 operands keeps its pure hit, and every
/// other declaration calls its entry with `this` as the first word when it
/// reads one. The entry proves its own receiver and operands.
pub(super) fn resolved_hit(
    view: &JitCompileSnapshot,
    call: otter_vm::JitStaticNativeCall,
    argument_count: usize,
    int32_operands: bool,
) -> Option<HitKind> {
    let declaration = otter_vm::jit_static_native::jit_leaf_builtin(call.leaf_stub_id)?;
    if view.cage_base == 0
        || view.native_ref_byte == 0
        || argument_count != usize::from(declaration.argument_count)
        || call.argument_count != declaration.argument_count
    {
        return None;
    }
    if int32_operands
        && super::super::native_leaf::supports_int32(call.leaf_stub_id)
        && super::super::native_leaf::supports_site(view, call, argument_count)
    {
        return Some(HitKind::Int32Math);
    }
    super::super::native_leaf::supports_leaf_probe(call.leaf_stub_id, declaration.operand_words())
        .then_some(HitKind::Leaf)
}

pub(super) fn site(hir: &NumericFunction, block: usize) -> Option<NumericValue> {
    let value = *hir.blocks.get(block)?.nodes.last()?;
    matches!(hir.nodes[value.0], NumericNode::NativeCall { .. }).then_some(value)
}

fn boolean(representations: &mut Vec<MachineRepresentation>) -> MachineValue {
    push_value(representations, MachineRepresentation::Boolean)
}

fn tagged(representations: &mut Vec<MachineRepresentation>) -> MachineValue {
    push_value(representations, MachineRepresentation::Tagged)
}

fn push_probe(
    target_spec: &TargetSpec,
    instructions: &mut Vec<MachineInstruction>,
    opcode: MachineOpcode,
    operands: Vec<MachineOperand>,
) {
    let mut instruction = MachineInstruction::plain(opcode, operands);
    instruction.clobbers = target_spec
        .clobbers(TargetClobberSet::PropertyLoad)
        .to_vec();
    instructions.push(instruction);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn select_probe(
    target_spec: &TargetSpec,
    hir: &NumericFunction,
    node: NumericValue,
    receiver: MachineValue,
    outputs: Values,
    values: &[MachineValue],
    representations: &mut Vec<MachineRepresentation>,
    instructions: &mut Vec<MachineInstruction>,
) -> Result<(), super::super::VerificationError> {
    let NumericNode::NativeCall {
        target,
        hit,
        argument_start,
        byte_pc,
        ..
    } = hir.nodes[node.0]
    else {
        return Err(super::super::VerificationError::InvalidValue(
            machine_value(values, node),
        ));
    };
    let call = target.declaration();
    let (callee, active) = match target {
        NumericNativeCallTarget::Method(method) => select_method_lookup(
            target_spec,
            receiver,
            method,
            byte_pc,
            representations,
            instructions,
        )?,
        NumericNativeCallTarget::Resolved { callee, .. } => {
            let callee = tagged_call_argument(hir, values, representations, instructions, callee);
            let active = boolean(representations);
            instructions.push(MachineInstruction::plain(
                MachineOpcode::BooleanConstant(true),
                vec![MachineOperand::register_output(active)],
            ));
            (callee, active)
        }
    };
    let identity_hit = boolean(representations);
    push_probe(
        target_spec,
        instructions,
        MachineOpcode::NativeLeafIdentity {
            byte_pc,
            builtin_native_ref: call.builtin_native_ref,
        },
        vec![
            MachineOperand::location_input(callee),
            MachineOperand::location_input(active),
            MachineOperand::register_output(identity_hit),
        ],
    );
    let start = argument_start as usize;
    let args = hir
        .operand_values
        .get(start..start + usize::from(call.argument_count))
        .ok_or(super::super::VerificationError::InvalidValue(
            machine_value(values, node),
        ))?;
    if hit == HitKind::Leaf {
        // An exotic receiver, or the `this` of a declaration that reads one,
        // is the entry's first operand word.
        let receiver_word = match target {
            NumericNativeCallTarget::Method(method) => {
                matches!(method.receiver, otter_vm::JitGuardedReceiver::Exotic(_))
            }
            NumericNativeCallTarget::Resolved { call, .. } => {
                otter_vm::jit_static_native::jit_leaf_builtin(call.leaf_stub_id)
                    .is_some_and(|declaration| declaration.this_operand)
            }
        }
        .then_some(receiver);
        let words = receiver_word
            .into_iter()
            .chain(args.iter().map(|&argument| {
                tagged_call_argument(hir, values, representations, instructions, argument)
            }))
            .collect::<Vec<_>>();
        let mut operands = words
            .into_iter()
            .enumerate()
            .map(|(index, word)| {
                MachineOperand::fixed_register_input(
                    word,
                    target_spec
                        .integer_argument(index + 1)
                        .expect("bounded native leaf operand"),
                )
            })
            .collect::<Vec<_>>();
        operands.extend([
            MachineOperand::location_input(identity_hit),
            MachineOperand::fixed_register_output(outputs.result, target_spec.integer_result()),
            MachineOperand::register_output(outputs.hit),
        ]);
        let mut leaf = MachineInstruction::plain(
            MachineOpcode::NativeLeafProbe {
                stub: call.leaf_stub_id,
                byte_pc,
            },
            operands,
        );
        leaf.clobbers = super::super::native_leaf::leaf_probe_clobbers(target_spec);
        instructions.push(leaf);
        return Ok(());
    }
    let mut operands = args
        .iter()
        .enumerate()
        .map(|(index, &argument)| {
            MachineOperand::fixed_register_input(
                machine_value(values, argument),
                target_spec
                    .integer_argument(index + 1)
                    .expect("bounded native Math argument"),
            )
        })
        .collect::<Vec<_>>();
    operands.extend([
        MachineOperand::location_input(identity_hit),
        MachineOperand::fixed_register_output(outputs.result, target_spec.integer_result()),
        MachineOperand::register_output(outputs.hit),
    ]);
    let mut math = MachineInstruction::plain(
        MachineOpcode::NativeInt32Math {
            stub: call.leaf_stub_id,
            byte_pc,
        },
        operands,
    );
    math.clobbers = super::super::native_leaf::method_math_clobbers(target_spec);
    instructions.push(math);
    Ok(())
}

fn select_method_lookup(
    target_spec: &TargetSpec,
    receiver: MachineValue,
    call: otter_vm::JitGuardedMethodCall,
    byte_pc: u32,
    representations: &mut Vec<MachineRepresentation>,
    instructions: &mut Vec<MachineInstruction>,
) -> Result<(MachineValue, MachineValue), super::super::VerificationError> {
    let active = boolean(representations);
    instructions.push(MachineInstruction::plain(
        MachineOpcode::BooleanConstant(true),
        vec![MachineOperand::register_output(active)],
    ));
    let (holder, active) = match call.receiver {
        otter_vm::JitGuardedReceiver::Shape { shape } => {
            let hit = guard_shaped(
                target_spec,
                receiver,
                active,
                shape,
                byte_pc,
                representations,
                instructions,
            );
            if call.holder == otter_vm::JitMethodHolder::Receiver {
                (receiver, hit)
            } else {
                let holder = tagged(representations);
                let prototype_hit = boolean(representations);
                push_probe(
                    target_spec,
                    instructions,
                    MachineOpcode::CacheIrLoadPrototype { byte_pc },
                    vec![
                        MachineOperand::location_input(receiver),
                        MachineOperand::register_input(hit),
                        MachineOperand::register_output(holder),
                        MachineOperand::register_output(prototype_hit),
                    ],
                );
                (holder, prototype_hit)
            }
        }
        otter_vm::JitGuardedReceiver::Exotic(target) => {
            let holder = tagged(representations);
            let receiver_hit = boolean(representations);
            push_probe(
                target_spec,
                instructions,
                MachineOpcode::CacheIrLoadIntrinsicPrototype { byte_pc, target },
                vec![
                    MachineOperand::location_input(receiver),
                    MachineOperand::register_input(active),
                    MachineOperand::register_output(holder),
                    MachineOperand::register_output(receiver_hit),
                ],
            );
            (holder, receiver_hit)
        }
    };
    let hit = match call.holder {
        otter_vm::JitMethodHolder::Receiver => active,
        otter_vm::JitMethodHolder::Shape(shape) => guard_shaped(
            target_spec,
            holder,
            active,
            shape,
            byte_pc,
            representations,
            instructions,
        ),
        otter_vm::JitMethodHolder::Dictionary(layout) => {
            let hit = boolean(representations);
            push_probe(
                target_spec,
                instructions,
                MachineOpcode::CacheIrGuardDictionaryLayout { byte_pc, layout },
                vec![
                    MachineOperand::location_input(holder),
                    MachineOperand::register_input(active),
                    MachineOperand::register_output(hit),
                ],
            );
            hit
        }
    };
    Ok(load_method(
        target_spec,
        call,
        byte_pc,
        holder,
        hit,
        representations,
        instructions,
    ))
}

/// Prove a fast object's hidden class and that its shape authorizes named
/// lookup without descriptor overrides or opaque lookup state.
fn guard_shaped(
    target_spec: &TargetSpec,
    object: MachineValue,
    active: MachineValue,
    shape: u32,
    byte_pc: u32,
    representations: &mut Vec<MachineRepresentation>,
    instructions: &mut Vec<MachineInstruction>,
) -> MachineValue {
    let shape_hit = boolean(representations);
    push_probe(
        target_spec,
        instructions,
        MachineOpcode::CacheIrGuardShape { byte_pc, shape },
        vec![
            MachineOperand::location_input(object),
            MachineOperand::register_input(active),
            MachineOperand::register_output(shape_hit),
        ],
    );
    let state_hit = boolean(representations);
    push_probe(
        target_spec,
        instructions,
        MachineOpcode::CacheIrGuardOrdinaryState { byte_pc },
        vec![
            MachineOperand::location_input(object),
            MachineOperand::register_input(shape_hit),
            MachineOperand::register_output(state_hit),
        ],
    );
    state_hit
}

/// Read the method slot from an already-proven holder.
fn load_method(
    target_spec: &TargetSpec,
    call: otter_vm::JitGuardedMethodCall,
    byte_pc: u32,
    holder: MachineValue,
    hit: MachineValue,
    representations: &mut Vec<MachineRepresentation>,
    instructions: &mut Vec<MachineInstruction>,
) -> (MachineValue, MachineValue) {
    let callee = tagged(representations);
    let field_hit = boolean(representations);
    push_probe(
        target_spec,
        instructions,
        MachineOpcode::CacheIrLoadField {
            byte_pc,
            value_byte: call.method_value_byte,
        },
        vec![
            MachineOperand::location_input(holder),
            MachineOperand::register_input(hit),
            MachineOperand::register_output(callee),
            MachineOperand::register_output(field_hit),
        ],
    );
    (callee, field_hit)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn select_block(
    target_spec: &TargetSpec,
    selected: SelectedBlock,
    hir: &NumericFunction,
    cfg: &SelectionCfg,
    block_index: usize,
    outputs: Values,
    receiver: MachineValue,
    values: &[MachineValue],
    representations: &mut Vec<MachineRepresentation>,
    call_descriptors: &mut Vec<CallDescriptor>,
    next_safepoint: &mut u32,
    instructions: &mut Vec<MachineInstruction>,
    exits: Box<[MachineExit]>,
) -> Result<MachineBlockData, super::super::VerificationError> {
    let first = MachineInstructionId(instructions.len() as u32);
    let node = site(hir, block_index).ok_or(super::super::VerificationError::InvalidBlock(
        cfg.originals[block_index],
    ))?;
    let NumericNode::NativeCall {
        target,
        hit,
        argument_start,
        logical_pc,
        byte_pc,
        ..
    } = hir.nodes[node.0]
    else {
        unreachable!()
    };
    let call = target.declaration();
    let selected_blocks = cfg.native_calls[&block_index];
    let mut result = MachineBlockData {
        first,
        end: first,
        predecessors: vec![],
        successors: vec![],
        successor_arguments: vec![],
        parameters: vec![],
    };
    match selected {
        SelectedBlock::NativeCallHit(_) => {
            let payload = match hit {
                HitKind::Int32Math => {
                    instructions.push(MachineInstruction::plain(
                        MachineOpcode::BoxInt32,
                        vec![
                            MachineOperand::register_input(outputs.result),
                            MachineOperand::register_output(outputs.payload),
                        ],
                    ));
                    outputs.payload
                }
                HitKind::Leaf => outputs.result,
            };
            jump(instructions);
            result.predecessors = vec![cfg.originals[block_index]];
            result.successors = vec![selected_blocks.join];
            result.successor_arguments = vec![vec![payload]];
        }
        SelectedBlock::NativeCallCold(_) => {
            let state_index = hir
                .frame_states
                .iter()
                .position(|state| state.point == NumericFramePoint::Node(node))
                .ok_or(super::super::VerificationError::OpcodeSignatureMismatch(
                    first,
                ))?;
            let source_function = hir.frame_states[state_index]
                .frames
                .last()
                .filter(|frame| frame.byte_pc == byte_pc)
                .ok_or(super::super::VerificationError::OpcodeSignatureMismatch(
                    first,
                ))?
                .function_id;
            let (kind, receiver_words) = match target {
                NumericNativeCallTarget::Method(_) => (NumericDirectCallKind::Method, 0),
                NumericNativeCallTarget::Resolved { .. } => {
                    (NumericDirectCallKind::CallWithThis, 1)
                }
            };
            let descriptor = direct_call_descriptor(
                target_spec,
                &NumericDirectCallTarget {
                    kind,
                    candidates: vec![],
                },
                source_function,
                logical_pc,
                byte_pc,
                DirectCallArgumentMode::Fixed,
                usize::from(call.argument_count) + receiver_words,
                None,
            )
            .ok_or(super::super::VerificationError::OpcodeSignatureMismatch(
                first,
            ))?;
            let descriptor_index = intern_call_descriptor(call_descriptors, descriptor);
            let start = argument_start as usize;
            let args = hir
                .operand_values
                .get(start..start + usize::from(call.argument_count))
                .ok_or(super::super::VerificationError::OpcodeSignatureMismatch(
                    first,
                ))?;
            let mut operands = Vec::with_capacity(args.len() + receiver_words + 2);
            if let NumericNativeCallTarget::Resolved { callee, .. } = target {
                let callee =
                    tagged_call_argument(hir, values, representations, instructions, callee);
                operands.push(MachineOperand::location_input(callee));
            }
            operands.push(MachineOperand::location_input(receiver));
            for &argument in args {
                let argument =
                    tagged_call_argument(hir, values, representations, instructions, argument);
                operands.push(MachineOperand::location_input(argument));
            }
            operands.push(MachineOperand::register_output(outputs.cold_payload));
            let mut cold =
                MachineInstruction::plain(MachineOpcode::Call(descriptor_index as u32), operands);
            cold.clobbers = call_descriptors[descriptor_index].clobbers.clone();
            cold.safepoint = Some(SafepointId(*next_safepoint));
            *next_safepoint += 1;
            attach_frame_state(hir, values, state_index, exits, &mut cold);
            inline_reentry::select_frames(
                hir,
                state_index,
                values,
                representations,
                instructions,
                &mut cold,
            )?;
            instructions.push(cold);
            jump(instructions);
            result.predecessors = vec![cfg.originals[block_index]];
            result.successors = vec![selected_blocks.join];
            result.successor_arguments = vec![vec![outputs.cold_payload]];
        }
        SelectedBlock::NativeCallJoin(_) => {
            if hir.blocks[block_index].terminator != NumericTerminator::Jump {
                return Err(super::super::VerificationError::OpcodeSignatureMismatch(
                    first,
                ));
            }
            jump(instructions);
            result.predecessors = vec![selected_blocks.hit, selected_blocks.cold];
            result.parameters = vec![machine_value(values, node)];
            for (edge, &successor) in hir.blocks[block_index].successors.iter().enumerate() {
                result.successors.push(
                    cfg.split_edges
                        .get(&(block_index, edge))
                        .copied()
                        .unwrap_or(cfg.originals[successor]),
                );
                result.successor_arguments.push(
                    hir.blocks[block_index].successor_arguments[edge]
                        .iter()
                        .map(|&value| machine_value(values, value))
                        .collect(),
                );
            }
        }
        _ => {
            return Err(super::super::VerificationError::OpcodeSignatureMismatch(
                first,
            ));
        }
    }
    result.end = MachineInstructionId(instructions.len() as u32);
    Ok(result)
}

fn jump(instructions: &mut Vec<MachineInstruction>) {
    let mut jump = MachineInstruction::plain(MachineOpcode::Jump, vec![]);
    jump.control = ControlFlow::Branch;
    instructions.push(jump);
}
