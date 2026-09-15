//! Explicit indexed-element guards, effects, committed cold call and CFG.
//!
//! # Contents
//! - Receiver-view, bounds/address, slot and value guards as separate SSA nodes.
//! - Direct no-fail element loads/stores and committed runtime completion.
//! - Success, Throw, Fatal and ordinary join blocks.
//!
//! # Invariants
//! - No raw element address crosses a call, allocation, or reentrant edge.
//! - Every miss-capable proof dominates the generated store.
//! - Only the cold call owns a safepoint; its status is explicit control flow.
//! - A committed cold operation is never deoptimized and replayed.

use super::*;

#[derive(Clone, Copy)]
pub(super) struct Blocks {
    pub hit: MachineBlock,
    pub cold: MachineBlock,
    pub success: MachineBlock,
    pub throw: Option<MachineBlock>,
    pub fatal: MachineBlock,
    pub join: MachineBlock,
}

#[derive(Clone, Copy)]
pub(super) struct Values {
    pub base: MachineValue,
    pub length: MachineValue,
    pub view_hit: MachineValue,
    pub address: MachineValue,
    pub address_hit: MachineValue,
    pub fast_payload: MachineValue,
    pub hit: MachineValue,
    pub cold_payload: MachineValue,
    pub status: MachineValue,
}

impl Values {
    pub fn new(representations: &mut Vec<MachineRepresentation>) -> Self {
        Self {
            base: push_value(representations, MachineRepresentation::Int64),
            length: push_value(representations, MachineRepresentation::Int64),
            view_hit: push_value(representations, MachineRepresentation::Boolean),
            address: push_value(representations, MachineRepresentation::Int64),
            address_hit: push_value(representations, MachineRepresentation::Boolean),
            fast_payload: push_value(representations, MachineRepresentation::Tagged),
            hit: push_value(representations, MachineRepresentation::Boolean),
            cold_payload: push_value(representations, MachineRepresentation::Tagged),
            status: push_value(representations, MachineRepresentation::NativeStatus),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Inputs {
    pub receiver: MachineValue,
    pub index: MachineValue,
    pub stored_fast: Option<MachineValue>,
    pub stored_tagged: Option<MachineValue>,
}

pub(super) fn site(hir: &NumericFunction, block: usize) -> Option<hir::NumericValue> {
    let value = *hir.blocks.get(block)?.nodes.last()?;
    matches!(
        hir.nodes[value.0],
        NumericNode::ElementLoad { .. } | NumericNode::ElementStore { .. }
    )
    .then_some(value)
}

pub(super) fn exceptional(hir: &NumericFunction, block: usize) -> Option<usize> {
    match hir.nodes[site(hir, block)?.0] {
        NumericNode::ElementLoad {
            exceptional_edge, ..
        }
        | NumericNode::ElementStore {
            exceptional_edge, ..
        } => exceptional_edge.map(usize::from),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn select_probe(
    target_spec: &TargetSpec,
    byte_pc: u32,
    access: Option<NumericElementAccess>,
    inputs: Inputs,
    values: Values,
    _representations: &mut Vec<MachineRepresentation>,
    instructions: &mut Vec<MachineInstruction>,
) -> Result<(), super::super::VerificationError> {
    let Some(_access) = access else {
        instructions.push(MachineInstruction::plain(
            MachineOpcode::IntegerConstant(0),
            vec![MachineOperand::register_output(values.address)],
        ));
        instructions.push(MachineInstruction::plain(
            MachineOpcode::TaggedConstant(otter_vm::Value::undefined().to_bits()),
            vec![MachineOperand::register_output(values.fast_payload)],
        ));
        instructions.push(MachineInstruction::plain(
            MachineOpcode::BooleanConstant(false),
            vec![MachineOperand::register_output(values.hit)],
        ));
        return Ok(());
    };
    let mut view = MachineInstruction::plain(
        MachineOpcode::ElementView { byte_pc },
        vec![
            MachineOperand::location_input(inputs.receiver),
            MachineOperand::register_output(values.base),
            MachineOperand::register_output(values.length),
            MachineOperand::register_output(values.view_hit),
        ],
    );
    view.clobbers = element_clobbers(target_spec);
    instructions.push(view);
    let mut address = MachineInstruction::plain(
        MachineOpcode::ElementAddress { byte_pc },
        vec![
            MachineOperand::location_input(values.base),
            MachineOperand::location_input(values.length),
            MachineOperand::location_input(inputs.index),
            MachineOperand::register_input(values.view_hit),
            MachineOperand::register_output(values.address),
            MachineOperand::register_output(values.address_hit),
        ],
    );
    address.clobbers = element_clobbers(target_spec);
    instructions.push(address);
    let mut terminal = if let Some(stored) = inputs.stored_fast {
        MachineInstruction::plain(
            MachineOpcode::ElementValueGuard { byte_pc },
            vec![
                MachineOperand::location_input(values.address),
                MachineOperand::location_input(stored),
                MachineOperand::register_input(values.address_hit),
                MachineOperand::register_output(values.hit),
            ],
        )
    } else {
        MachineInstruction::plain(
            MachineOpcode::ElementValueLoad { byte_pc },
            vec![
                MachineOperand::location_input(values.address),
                MachineOperand::register_input(values.address_hit),
                MachineOperand::register_output(values.fast_payload),
                MachineOperand::register_output(values.hit),
            ],
        )
    };
    terminal.clobbers = element_clobbers(target_spec);
    instructions.push(terminal);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn select_block(
    target_spec: &TargetSpec,
    selected: SelectedBlock,
    hir: &NumericFunction,
    cfg: &SelectionCfg,
    block_index: usize,
    values: Values,
    inputs: Inputs,
    machine_values: &[MachineValue],
    representations: &mut Vec<MachineRepresentation>,
    call_descriptors: &mut Vec<CallDescriptor>,
    next_safepoint: &mut u32,
    instructions: &mut Vec<MachineInstruction>,
) -> Result<MachineBlockData, super::super::VerificationError> {
    let first = MachineInstructionId(instructions.len() as u32);
    let node = site(hir, block_index).ok_or(super::super::VerificationError::InvalidBlock(
        cfg.originals[block_index],
    ))?;
    let blocks = cfg.elements[&block_index];
    let store = inputs.stored_fast.is_some();
    let mut result = MachineBlockData {
        first,
        end: first,
        predecessors: vec![],
        successors: vec![],
        parameters: vec![],
        successor_arguments: vec![],
    };
    match selected {
        SelectedBlock::ElementHit(_) => {
            if let Some(stored) = inputs.stored_fast {
                let (byte_pc, access) = match hir.nodes[node.0] {
                    NumericNode::ElementStore {
                        byte_pc, access, ..
                    } => (byte_pc, access),
                    _ => unreachable!(),
                };
                if access.is_some() {
                    let mut effect = MachineInstruction::plain(
                        MachineOpcode::ElementValueStore { byte_pc },
                        vec![
                            MachineOperand::location_input(values.address),
                            MachineOperand::location_input(stored),
                        ],
                    );
                    effect.clobbers = element_clobbers(target_spec);
                    instructions.push(effect);
                }
            }
            jump(instructions);
            result.predecessors = vec![cfg.originals[block_index]];
            result.successors = vec![blocks.join];
            result.successor_arguments = vec![if store {
                vec![]
            } else {
                vec![values.fast_payload]
            }];
        }
        SelectedBlock::ElementCold(_) => {
            let (logical_pc, byte_pc) = hir
                .frame_states
                .iter()
                .find(|state| state.point == NumericFramePoint::Node(node))
                .and_then(|state| state.frames.last())
                .map(|frame| (hir.blocks[block_index].logical_pc, frame.byte_pc))
                .ok_or(super::super::VerificationError::OpcodeSignatureMismatch(
                    first,
                ))?;
            let mut descriptor = binding_value_descriptor(
                target_spec,
                logical_pc,
                byte_pc,
                if store { 3 } else { 2 },
            );
            if let CallTarget::CommittedRuntime { target, .. } = &mut descriptor.target {
                *target = if store {
                    otter_vm::native_abi::STUB_JIT_STORE_ELEMENT
                } else {
                    otter_vm::native_abi::STUB_JIT_LOAD_ELEMENT
                };
            }
            descriptor.arguments = vec![MachineRepresentation::Tagged; if store { 3 } else { 2 }];
            let descriptor_index = intern_call_descriptor(call_descriptors, descriptor);
            let mut operands = vec![
                MachineOperand::location_input(inputs.receiver),
                MachineOperand::location_input(inputs.index),
            ];
            if let Some(value) = inputs.stored_tagged {
                operands.push(MachineOperand::location_input(value));
            }
            operands.extend([
                MachineOperand::register_output(values.cold_payload),
                MachineOperand::register_output(values.status),
            ]);
            let mut call =
                MachineInstruction::plain(MachineOpcode::Call(descriptor_index as u32), operands);
            call.clobbers = call_descriptors[descriptor_index].clobbers.clone();
            call.safepoint = Some(SafepointId(*next_safepoint));
            *next_safepoint += 1;
            let state_index = hir
                .frame_states
                .iter()
                .position(|state| state.point == NumericFramePoint::Node(node))
                .ok_or(super::super::VerificationError::OpcodeSignatureMismatch(
                    first,
                ))?;
            inline_reentry::select_frames(
                hir,
                state_index,
                machine_values,
                representations,
                instructions,
                &mut call,
            )?;
            attach_frame_state_tagged_roots(hir, machine_values, state_index, &mut call);
            instructions.push(call);
            let mut branch = MachineInstruction::plain(
                MachineOpcode::BranchNativeStatus,
                vec![MachineOperand::register_input(values.status)],
            );
            branch.clobbers = target_spec
                .clobbers(TargetClobberSet::StatusScratch)
                .to_vec();
            branch.control = ControlFlow::Branch;
            instructions.push(branch);
            result.predecessors = vec![cfg.originals[block_index]];
            let throw = exceptional(hir, block_index)
                .map(|edge| cfg.split_edges[&(block_index, edge)])
                .or(blocks.throw)
                .ok_or(super::super::VerificationError::OpcodeSignatureMismatch(
                    first,
                ))?;
            result.successors = vec![blocks.success, throw, blocks.fatal];
            result.successor_arguments = vec![vec![], vec![], vec![]];
        }
        SelectedBlock::ElementSuccess(_) => {
            jump(instructions);
            result.predecessors = vec![blocks.cold];
            result.successors = vec![blocks.join];
            result.successor_arguments = vec![if store {
                vec![]
            } else {
                vec![values.cold_payload]
            }];
        }
        SelectedBlock::ElementThrow(_) | SelectedBlock::ElementFatal(_) => {
            let throwing = matches!(selected, SelectedBlock::ElementThrow(_));
            let mut terminator = MachineInstruction::plain(
                if throwing {
                    MachineOpcode::Throw
                } else {
                    MachineOpcode::Fatal
                },
                if throwing {
                    vec![MachineOperand::register_input(values.cold_payload)]
                } else {
                    vec![]
                },
            );
            terminator.control = ControlFlow::Return;
            instructions.push(terminator);
            result.predecessors = vec![blocks.cold];
        }
        SelectedBlock::ElementJoin(_) => {
            if hir.blocks[block_index].terminator != NumericTerminator::Jump {
                return Err(super::super::VerificationError::OpcodeSignatureMismatch(
                    first,
                ));
            }
            jump(instructions);
            result.predecessors = vec![blocks.hit, blocks.success];
            if !store {
                result.parameters = vec![machine_value(machine_values, node)];
            }
            for (edge, &successor) in hir.blocks[block_index].successors.iter().enumerate() {
                if exceptional(hir, block_index) == Some(edge) {
                    continue;
                }
                result.successors.push(
                    cfg.split_edges
                        .get(&(block_index, edge))
                        .copied()
                        .unwrap_or(cfg.originals[successor]),
                );
                result.successor_arguments.push(
                    hir.blocks[block_index].successor_arguments[edge]
                        .iter()
                        .map(|&value| machine_value(machine_values, value))
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
