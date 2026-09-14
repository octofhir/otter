//! Explicit named-property probe, committed cold call and completion CFG.
//!
//! # Contents
//! - Machine values and blocks for a source-owned property operation.
//! - Collector-visible cold operands and explicit native status control.
//!
//! # Invariants
//! - The probe never allocates or reenters JavaScript.
//! - Only the cold call owns a safepoint; success joins through ordinary SSA.
//! - Throw and Fatal are explicit terminators, never emitter-hidden branches.
//! - The untraced cell address refers to code-owned stable memory.
//!
//! # See also
//! - `super::SelectionCfg` assigns block identities before allocation.
//! - `super::super::MachinePropertySite` owns source proofs and IC identity.

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
    pub hit: MachineValue,
    pub payload: MachineValue,
    pub cell: MachineValue,
    pub cold_payload: MachineValue,
    pub status: MachineValue,
}

impl Values {
    pub fn new(representations: &mut Vec<MachineRepresentation>) -> Self {
        Self {
            hit: push_value(representations, MachineRepresentation::Boolean),
            payload: push_value(representations, MachineRepresentation::Tagged),
            cell: push_value(representations, MachineRepresentation::Int64),
            cold_payload: push_value(representations, MachineRepresentation::Tagged),
            status: push_value(representations, MachineRepresentation::NativeStatus),
        }
    }
}

pub(super) fn site(hir: &NumericFunction, block: usize) -> Option<hir::NumericValue> {
    let value = *hir.blocks.get(block)?.nodes.last()?;
    matches!(
        hir.nodes[value.0],
        NumericNode::PropertyLoad { .. } | NumericNode::PropertyStore { .. }
    )
    .then_some(value)
}

pub(super) fn exceptional(hir: &NumericFunction, block: usize) -> Option<usize> {
    match hir.nodes[site(hir, block)?.0] {
        NumericNode::PropertyLoad {
            exceptional_edge, ..
        } => exceptional_edge.map(usize::from),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn select_block(
    target_spec: &TargetSpec,
    selected: SelectedBlock,
    hir: &NumericFunction,
    cfg: &SelectionCfg,
    block_index: usize,
    values: Values,
    inputs: (MachineValue, Option<MachineValue>),
    machine_values: &[MachineValue],
    representations: &mut Vec<MachineRepresentation>,
    call_descriptors: &mut Vec<CallDescriptor>,
    next_safepoint: &mut u32,
    instructions: &mut Vec<MachineInstruction>,
) -> Result<MachineBlockData, super::super::VerificationError> {
    let (receiver, stored) = inputs;
    let first = MachineInstructionId(instructions.len() as u32);
    let node = site(hir, block_index).ok_or(super::super::VerificationError::InvalidBlock(
        cfg.originals[block_index],
    ))?;
    let blocks = cfg.properties[&block_index];
    let mut result = MachineBlockData {
        first,
        end: first,
        predecessors: vec![],
        successors: vec![],
        parameters: vec![],
        successor_arguments: vec![],
    };
    match selected {
        SelectedBlock::PropertyCold(_) => {
            let source = &hir.property_sites[&node];
            let mut descriptor = binding_value_descriptor(
                target_spec,
                source.logical_pc,
                source.byte_pc,
                if stored.is_some() { 3 } else { 2 },
            );
            if let CallTarget::CommittedRuntime { target, .. } = &mut descriptor.target {
                *target = if stored.is_some() {
                    otter_vm::native_abi::STUB_JIT_STORE_PROPERTY
                } else {
                    otter_vm::native_abi::STUB_JIT_LOAD_PROPERTY
                };
            }
            descriptor.arguments = vec![MachineRepresentation::Tagged];
            if stored.is_some() {
                descriptor.arguments.push(MachineRepresentation::Tagged);
            }
            descriptor.arguments.push(MachineRepresentation::Int64);
            let descriptor_index = intern_call_descriptor(call_descriptors, descriptor);
            let mut operands = vec![MachineOperand::location_input(receiver)];
            if let Some(value) = stored {
                operands.push(MachineOperand::location_input(value));
            }
            operands.extend([
                MachineOperand::location_input(values.cell),
                MachineOperand::register_output(values.cold_payload),
                MachineOperand::register_output(values.status),
                MachineOperand::tagged_root(receiver),
            ]);
            if let Some(value) = stored {
                operands.push(MachineOperand::tagged_root(value));
            }
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
            );
            attach_frame_state_tagged_roots(hir, machine_values, state_index, &mut call);
            attach_safepoint_roots(representations, &mut call);
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
        SelectedBlock::PropertyHit(_) => {
            jump(instructions);
            result.predecessors = vec![cfg.originals[block_index]];
            result.successors = vec![blocks.join];
            result.successor_arguments = vec![if stored.is_some() {
                vec![]
            } else {
                vec![values.payload]
            }];
        }
        SelectedBlock::PropertySuccess(_) => {
            jump(instructions);
            result.predecessors = vec![blocks.cold];
            result.successors = vec![blocks.join];
            result.successor_arguments = vec![if stored.is_some() {
                vec![]
            } else {
                vec![values.cold_payload]
            }];
        }
        SelectedBlock::PropertyThrow(_) | SelectedBlock::PropertyFatal(_) => {
            let throwing = matches!(selected, SelectedBlock::PropertyThrow(_));
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
        SelectedBlock::PropertyJoin(_) => {
            if hir.blocks[block_index].terminator != NumericTerminator::Jump {
                return Err(super::super::VerificationError::OpcodeSignatureMismatch(
                    first,
                ));
            }
            jump(instructions);
            result.predecessors = vec![blocks.hit, blocks.success];
            if stored.is_none() {
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
