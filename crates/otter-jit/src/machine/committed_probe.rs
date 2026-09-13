//! Explicit fast/cold CFG for committed operations with a generated probe.
//!
//! # Contents
//! - [`expand`] splits derived-this and loose-equality calls before allocation.
//!
//! # Invariants
//! - Each probe owns its complete no-call proof and reports whether it finished.
//! - The cold call exposes its value/status pair to an explicit three-way
//!   branch. Register-allocation edge moves run before catch or propagation.
//!   No emitter-hidden exception branch may bypass SSA payload transfers.
//! - Success values join through SSA parameters; fast completion cannot make
//!   a cold-only exception value live on the generated path.
//! - Probe selection is keyed by exact call descriptor, never by source PC alone.
//! - Block ids remain stable and instruction ranges are rebuilt contiguously.
//!   Safepoint ids follow the rebuilt instruction order before root lowering.
//! - Both success edges are split before their shared SSA join, including the
//!   cold call's normal edge when it also owns a catch successor.
//!
//! # See also
//! - `numeric::arm64` — physical pair transport and allocated status branching.
//! - `derived_this` — binding proof validation and source attribution.

use super::{
    CallDescriptor, CallTarget, ControlFlow, ExceptionalEdge, MachineBlock, MachineBlockData,
    MachineInstruction, MachineInstructionId, MachineOpcode, MachineOperand, MachineRepresentation,
    MachineValue, OperandPurpose, PhysicalRegister, VerificationError,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProbeKind {
    DerivedThis,
    LooseEquality { equal: bool },
}

pub(super) fn expand(
    representations: &mut Vec<MachineRepresentation>,
    descriptors: &mut [CallDescriptor],
    sites: &std::collections::BTreeMap<u32, ProbeKind>,
    blocks: &mut Vec<MachineBlockData>,
    instructions: &mut Vec<MachineInstruction>,
) -> Result<(), VerificationError> {
    if sites.is_empty() {
        return Ok(());
    }
    let original_descriptors = descriptors.to_vec();
    let mut bodies = blocks
        .iter()
        .map(|block| instructions[block.first.0 as usize..block.end.0 as usize].to_vec())
        .collect::<Vec<_>>();
    let original_count = blocks.len();
    for original in 0..original_count {
        let mut current = original;
        while let Some((position, descriptor_index, descriptor, kind)) = bodies[current]
            .iter()
            .enumerate()
            .find_map(|(position, instruction)| {
                let MachineOpcode::Call(index) = instruction.opcode else {
                    return None;
                };
                let descriptor = original_descriptors.get(index as usize)?;
                let CallTarget::CommittedRuntime { target, .. } = descriptor.target else {
                    return None;
                };
                let kind = *sites.get(&index)?;
                let expected = match kind {
                    ProbeKind::DerivedThis => otter_vm::native_abi::STUB_JIT_SCALAR_VALUE,
                    ProbeKind::LooseEquality { .. } => {
                        otter_vm::native_abi::STUB_JIT_OBJECT_PROTOCOL_VALUE
                    }
                };
                (target == expected).then_some(())?;
                Some((position, index as usize, descriptor, kind))
            })
        {
            let CallTarget::CommittedRuntime { byte_pc, .. } = descriptor.target else {
                unreachable!()
            };
            let mut suffix = bodies[current].split_off(position);
            let mut call = suffix.remove(0);
            let inputs: Vec<_> = call
                .operands
                .iter()
                .filter(|operand| operand.purpose == OperandPurpose::Input)
                .map(|operand| operand.value)
                .collect();
            if inputs.len()
                != match kind {
                    ProbeKind::DerivedThis => 1,
                    ProbeKind::LooseEquality { .. } => 2,
                }
            {
                return Err(VerificationError::InvalidEntry);
            }
            let output = call
                .operands
                .iter_mut()
                .find(|operand| operand.purpose == OperandPurpose::Output)
                .ok_or(VerificationError::InvalidEntry)?;
            let result = output.value;
            let cold_result = value(representations, MachineRepresentation::Tagged);
            output.value = cold_result;
            let status = value(representations, MachineRepresentation::NativeStatus);
            call.operands.push(MachineOperand::register_output(status));
            descriptors[descriptor_index].results = vec![
                MachineRepresentation::Tagged,
                MachineRepresentation::NativeStatus,
            ];
            descriptors[descriptor_index].exceptional = ExceptionalEdge::None;
            let condition = value(representations, MachineRepresentation::Boolean);
            let fast_result = value(representations, MachineRepresentation::Tagged);
            let cold = MachineBlock(blocks.len() as u32);
            let fast = MachineBlock(cold.0 + 1);
            let success = MachineBlock(cold.0 + 2);
            let join = MachineBlock(cold.0 + 3);
            let fatal = MachineBlock(cold.0 + 4);
            let propagate = MachineBlock(cold.0 + 5);
            let mut join_block = blocks[current].clone();
            join_block.parameters = vec![result];
            let mut cold_successors = vec![success];
            let mut cold_arguments = vec![Vec::new()];
            if let ExceptionalEdge::LandingPad(target) = descriptor.exceptional {
                let edge = join_block
                    .successors
                    .iter()
                    .position(|candidate| *candidate == target)
                    .ok_or(VerificationError::InvalidEntry)?;
                cold_successors.push(join_block.successors.remove(edge));
                let mut arguments = join_block.successor_arguments.remove(edge);
                for argument in &mut arguments {
                    if *argument == result {
                        *argument = cold_result;
                    }
                }
                cold_arguments.push(arguments);
                // Selection gives each committed throw a private landing
                // block. Its acknowledgement/edge moves must use the cold
                // payload, which is defined before this exceptional edge;
                // the normal SSA join is deliberately not on that path.
                for instruction in &mut bodies[target.0 as usize] {
                    for operand in &mut instruction.operands {
                        if operand.value == result {
                            operand.value = cold_result;
                        }
                    }
                }
                for argument in blocks[target.0 as usize]
                    .successor_arguments
                    .iter_mut()
                    .flatten()
                {
                    if *argument == result {
                        *argument = cold_result;
                    }
                }
            }
            if descriptor.exceptional == ExceptionalEdge::Propagate {
                cold_successors.push(propagate);
                cold_arguments.push(Vec::new());
            }
            if cold_successors.len() != 2 {
                return Err(VerificationError::InvalidEntry);
            }
            cold_successors.push(fatal);
            cold_arguments.push(Vec::new());
            match kind {
                ProbeKind::DerivedThis => {
                    bodies[current].push(MachineInstruction::plain(
                        MachineOpcode::TaggedConstant(otter_vm::Value::undefined().to_bits()),
                        vec![MachineOperand::register_output(fast_result)],
                    ));
                    bodies[current].push(MachineInstruction::plain(
                        MachineOpcode::TryBindDerivedThis { byte_pc },
                        vec![
                            MachineOperand::fixed_register_input(
                                inputs[0],
                                PhysicalRegister::integer(1),
                            ),
                            MachineOperand::fixed_register_output(
                                condition,
                                PhysicalRegister::integer(0),
                            ),
                        ],
                    ));
                }
                ProbeKind::LooseEquality { equal } => {
                    bodies[current].push(MachineInstruction::plain(
                        MachineOpcode::LooseEqualityProbe { byte_pc, equal },
                        vec![
                            MachineOperand::register_input(inputs[0]),
                            MachineOperand::register_input(inputs[1]),
                            MachineOperand::register_output(fast_result),
                            MachineOperand::register_output(condition),
                        ],
                    ))
                }
            }
            let mut branch = MachineInstruction::plain(
                MachineOpcode::BranchIf(true),
                vec![MachineOperand::register_input(condition)],
            );
            branch.control = ControlFlow::Branch;
            bodies[current].push(branch);
            blocks[current].successors = vec![fast, cold];
            blocks[current].successor_arguments = vec![Vec::new(), Vec::new()];
            blocks.push(MachineBlockData {
                first: MachineInstructionId(0),
                end: MachineInstructionId(0),
                predecessors: Vec::new(),
                successors: cold_successors,
                parameters: Vec::new(),
                successor_arguments: cold_arguments,
            });
            for argument in [fast_result, cold_result] {
                blocks.push(MachineBlockData {
                    first: MachineInstructionId(0),
                    end: MachineInstructionId(0),
                    predecessors: Vec::new(),
                    successors: vec![join],
                    parameters: Vec::new(),
                    successor_arguments: vec![vec![argument]],
                });
            }
            blocks.push(join_block);
            let mut jump = MachineInstruction::plain(MachineOpcode::Jump, Vec::new());
            jump.control = ControlFlow::Branch;
            let mut status_branch = MachineInstruction::plain(
                MachineOpcode::BranchNativeStatus,
                vec![MachineOperand::register_input(status)],
            );
            status_branch.control = ControlFlow::Branch;
            status_branch.clobbers = vec![PhysicalRegister::integer(16)];
            bodies.push(vec![call, status_branch]);
            bodies.push(vec![jump.clone()]);
            bodies.push(vec![jump]);
            bodies.push(suffix);
            let mut fatal_instruction = MachineInstruction::plain(MachineOpcode::Fatal, Vec::new());
            fatal_instruction.control = ControlFlow::Return;
            blocks.push(terminal_block());
            bodies.push(vec![fatal_instruction]);
            if descriptor.exceptional == ExceptionalEdge::Propagate {
                let mut throw = MachineInstruction::plain(
                    MachineOpcode::Throw,
                    vec![MachineOperand::register_input(cold_result)],
                );
                throw.control = ControlFlow::Return;
                blocks.push(terminal_block());
                bodies.push(vec![throw]);
            }
            current = join.0 as usize;
        }
    }
    instructions.clear();
    for (block, body) in blocks.iter_mut().zip(bodies) {
        block.first = MachineInstructionId(instructions.len() as u32);
        instructions.extend(body);
        block.end = MachineInstructionId(instructions.len() as u32);
        block.predecessors.clear();
    }
    let mut next_safepoint = 0;
    for instruction in instructions {
        if instruction.safepoint.is_some() {
            instruction.safepoint = Some(super::SafepointId(next_safepoint));
            next_safepoint += 1;
        }
    }
    for source in 0..blocks.len() {
        for target in blocks[source].successors.clone() {
            blocks[target.0 as usize]
                .predecessors
                .push(MachineBlock(source as u32));
        }
    }
    Ok(())
}

fn value(
    representations: &mut Vec<MachineRepresentation>,
    representation: MachineRepresentation,
) -> MachineValue {
    let value = MachineValue(representations.len() as u32);
    representations.push(representation);
    value
}

fn terminal_block() -> MachineBlockData {
    MachineBlockData {
        first: MachineInstructionId(0),
        end: MachineInstructionId(0),
        predecessors: Vec::new(),
        successors: Vec::new(),
        parameters: Vec::new(),
        successor_arguments: Vec::new(),
    }
}
