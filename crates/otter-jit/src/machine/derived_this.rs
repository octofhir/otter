//! Explicit fast/cold control flow for derived-constructor this binding.
//!
//! # Contents
//! - [`expand`] splits committed BindThisValue calls before register allocation.
//!
//! # Invariants
//! - The generated probe commits only an unbound stack-owned derived frame.
//! - The original committed call, safepoint and exception edge remain the cold
//!   sibling. Its exception payload reaches the original landing pad directly.
//! - Success values join through SSA parameters; fast completion cannot make
//!   a cold-only exception value live on the generated path.
//! - Block ids remain stable and instruction ranges are rebuilt contiguously.
//!   Safepoint ids follow the rebuilt instruction order before root lowering.
//! - Both success edges are split before their shared SSA join, including the
//!   cold call's normal edge when it also owns a catch successor.

use super::{
    CallDescriptor, CallTarget, ControlFlow, ExceptionalEdge, MachineBlock, MachineBlockData,
    MachineInstruction, MachineInstructionId, MachineOpcode, MachineOperand, MachineRepresentation,
    MachineValue, OperandPurpose, PhysicalRegister, VerificationError,
};

pub(super) fn expand(
    representations: &mut Vec<MachineRepresentation>,
    descriptors: &[CallDescriptor],
    sites: &std::collections::BTreeSet<u32>,
    blocks: &mut Vec<MachineBlockData>,
    instructions: &mut Vec<MachineInstruction>,
) -> Result<(), VerificationError> {
    if sites.is_empty() {
        return Ok(());
    }
    let mut bodies = blocks
        .iter()
        .map(|block| instructions[block.first.0 as usize..block.end.0 as usize].to_vec())
        .collect::<Vec<_>>();
    let original_count = blocks.len();
    for original in 0..original_count {
        let mut current = original;
        while let Some((position, descriptor)) = bodies[current].iter().enumerate().find_map(|(position, instruction)| {
                let MachineOpcode::Call(index) = instruction.opcode else { return None; };
                let descriptor = descriptors.get(index as usize)?;
                matches!(descriptor.target, CallTarget::CommittedRuntime { target, byte_pc, .. }
                    if target == otter_vm::native_abi::STUB_JIT_SCALAR_VALUE && sites.contains(&byte_pc))
                    .then_some(())?;
                Some((position, descriptor))
            }) {
            let CallTarget::CommittedRuntime { byte_pc, .. } = descriptor.target else {
                unreachable!()
            };
            let mut suffix = bodies[current].split_off(position);
            let mut call = suffix.remove(0);
            let input = call
                .operands
                .iter()
                .find(|operand| operand.purpose == OperandPurpose::Input)
                .ok_or(VerificationError::InvalidEntry)?
                .value;
            let output = call
                .operands
                .iter_mut()
                .find(|operand| operand.purpose == OperandPurpose::Output)
                .ok_or(VerificationError::InvalidEntry)?;
            let result = output.value;
            let cold_result = value(representations, MachineRepresentation::Tagged);
            output.value = cold_result;
            let condition = value(representations, MachineRepresentation::Boolean);
            let undefined = value(representations, MachineRepresentation::Tagged);
            let cold = MachineBlock(blocks.len() as u32);
            let fast = MachineBlock(cold.0 + 1);
            let success = MachineBlock(cold.0 + 2);
            let join = MachineBlock(cold.0 + 3);
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
            bodies[current].push(MachineInstruction::plain(
                MachineOpcode::TaggedConstant(otter_vm::Value::undefined().to_bits()),
                vec![MachineOperand::register_output(undefined)],
            ));
            bodies[current].push(MachineInstruction::plain(
                MachineOpcode::TryBindDerivedThis { byte_pc },
                vec![
                    MachineOperand::fixed_register_input(input, PhysicalRegister::integer(1)),
                    MachineOperand::fixed_register_output(condition, PhysicalRegister::integer(0)),
                ],
            ));
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
            for argument in [undefined, cold_result] {
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
            bodies.push(vec![call, jump.clone()]);
            bodies.push(vec![jump.clone()]);
            bodies.push(vec![jump]);
            bodies.push(suffix);
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

/// Recover the cold region's source identity from its explicit predecessor.
pub(super) fn cold_byte_pc(sequence: &super::InstructionSequence, block: usize) -> Option<u32> {
    let [predecessor] = sequence.blocks().get(block)?.predecessors.as_slice() else {
        return None;
    };
    let predecessor = sequence.blocks().get(predecessor.0 as usize)?;
    if predecessor.successors.get(1) != Some(&MachineBlock(block as u32)) {
        return None;
    }
    let probe = sequence
        .instructions()
        .get(predecessor.end.0.checked_sub(2)? as usize)?;
    match probe.opcode {
        MachineOpcode::TryBindDerivedThis { byte_pc } => Some(byte_pc),
        _ => None,
    }
}

/// Require the successful probe to bypass the one committed cold call.
pub(super) fn probe_cfg_is_valid(
    sequence: &super::InstructionSequence,
    block: usize,
    id: MachineInstructionId,
    byte_pc: u32,
    condition: MachineValue,
) -> bool {
    let Some(block) = sequence.blocks().get(block) else {
        return false;
    };
    if block.end.0 != id.0 + 2 || block.successors.len() != 2 {
        return false;
    }
    let Some(branch) = sequence.instructions().get(id.0 as usize + 1) else {
        return false;
    };
    if branch.opcode != MachineOpcode::BranchIf(true)
        || branch.operands != [MachineOperand::register_input(condition)]
    {
        return false;
    }
    let Some(cold) = sequence.blocks().get(block.successors[1].0 as usize) else {
        return false;
    };
    let Some(call) = sequence.instructions().get(cold.first.0 as usize) else {
        return false;
    };
    let MachineOpcode::Call(descriptor) = call.opcode else {
        return false;
    };
    sequence
        .call_descriptors()
        .get(descriptor as usize)
        .is_some_and(|descriptor| {
            matches!(
                descriptor.target,
                CallTarget::CommittedRuntime { target, byte_pc: pc, semantic_arity: 1, .. }
                    if target == otter_vm::native_abi::STUB_JIT_SCALAR_VALUE && pc == byte_pc
            )
        })
}
