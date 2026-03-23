use std::collections::{BTreeSet, HashMap};

use otter_vm_bytecode::instruction::Instruction;

use crate::mir::graph::BlockId;

pub(super) fn find_block_starts(instructions: &[Instruction]) -> Vec<usize> {
    let mut starts = BTreeSet::new();
    starts.insert(0);
    for (pc, inst) in instructions.iter().enumerate() {
        match inst {
            Instruction::Jump { offset } => {
                let target = (pc as i64 + offset.0 as i64) as usize;
                starts.insert(target);
                starts.insert(pc + 1);
            }
            Instruction::JumpIfTrue { offset, .. }
            | Instruction::JumpIfFalse { offset, .. }
            | Instruction::JumpIfNullish { offset, .. }
            | Instruction::JumpIfNotNullish { offset, .. } => {
                let target = (pc as i64 + offset.0 as i64) as usize;
                starts.insert(target);
                starts.insert(pc + 1);
            }
            Instruction::TryStart { catch_offset } => {
                let target = (pc as i64 + catch_offset.0 as i64) as usize;
                starts.insert(target);
                starts.insert(pc + 1);
            }
            Instruction::Return { .. }
            | Instruction::ReturnUndefined
            | Instruction::Throw { .. } => {
                starts.insert(pc + 1);
            }
            _ => {}
        }
    }
    starts.into_iter().collect()
}

pub(super) fn resolve_target(pc: u32, offset: i32, pc_to_block: &HashMap<u32, BlockId>) -> BlockId {
    let target = (pc as i64 + offset as i64) as u32;
    *pc_to_block
        .get(&target)
        .unwrap_or_else(|| panic!("jump target pc={} not mapped to a block", target))
}
