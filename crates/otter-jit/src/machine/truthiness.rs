//! Explicit generated truthiness probes and canonical cold leaf calls.
//!
//! # Contents
//! - [`expand`] splits tagged ToBoolean calls before register allocation.
//!
//! # Invariants
//! - A probe neither calls nor collects; uncertain cells use the original leaf.
//! - Both results reach one SSA join; call clobbers apply only to the cold block.
//! - Original block identities and deopt operands remain intact. Instruction
//!   ranges, predecessor lists and safepoint ids follow the rebuilt CFG.
//!
//! # See also
//! - `numeric::arm64` — allocation-driven probe emission.

use super::{
    CallDescriptor, CallTarget, ControlFlow, MachineBlock, MachineBlockData, MachineInstruction,
    MachineInstructionId, MachineOpcode, MachineOperand, MachineRepresentation, MachineValue,
    OperandPurpose,
};

pub(super) fn expand(
    representations: &mut Vec<MachineRepresentation>,
    descriptors: &[CallDescriptor],
    blocks: &mut Vec<MachineBlockData>,
    instructions: &mut Vec<MachineInstruction>,
) {
    let is_truthiness = |instruction: &MachineInstruction| match instruction.opcode {
        MachineOpcode::Call(index) => matches!(descriptors[index as usize].target,
            CallTarget::RuntimeStub(target) if target == otter_vm::native_abi::STUB_TO_BOOLEAN_LEAF),
        _ => false,
    };
    if !instructions.iter().any(is_truthiness) {
        return;
    }
    let mut bodies: Vec<_> = blocks
        .iter()
        .map(|b| instructions[b.first.0 as usize..b.end.0 as usize].to_vec())
        .collect();
    for original in 0..blocks.len() {
        let mut current = original;
        while let Some(position) = bodies[current].iter().position(is_truthiness) {
            let mut suffix = bodies[current].split_off(position);
            let mut call = suffix.remove(0);
            let source = call.operands[0].value;
            let output = call
                .operands
                .iter_mut()
                .find(|o| o.purpose == OperandPurpose::Output)
                .expect("selected truthiness call result");
            let result = output.value;
            let mut boolean = || {
                let v = MachineValue(representations.len() as u32);
                representations.push(MachineRepresentation::Boolean);
                v
            };
            let cold_result = boolean();
            let fast_result = boolean();
            let hit = boolean();
            output.value = cold_result;
            let cold = MachineBlock(blocks.len() as u32);
            let fast = MachineBlock(cold.0 + 1);
            let join = MachineBlock(cold.0 + 2);
            let mut join_block = blocks[current].clone();
            join_block.parameters = vec![result];
            bodies[current].push(MachineInstruction::plain(
                MachineOpcode::TruthinessProbe,
                vec![
                    MachineOperand::register_input(source),
                    MachineOperand::register_output(fast_result),
                    MachineOperand::register_output(hit),
                ],
            ));
            let mut branch = MachineInstruction::plain(
                MachineOpcode::BranchIf(true),
                vec![MachineOperand::register_input(hit)],
            );
            branch.control = ControlFlow::Branch;
            bodies[current].push(branch);
            blocks[current].successors = vec![fast, cold];
            blocks[current].successor_arguments = vec![Vec::new(), Vec::new()];
            for argument in [cold_result, fast_result] {
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
    let mut next = 0;
    for instruction in instructions {
        if instruction.safepoint.is_some() {
            instruction.safepoint = Some(super::SafepointId(next));
            next += 1;
        }
    }
    for source in 0..blocks.len() {
        for target in blocks[source].successors.clone() {
            blocks[target.0 as usize]
                .predecessors
                .push(MachineBlock(source as u32));
        }
    }
}
