//! Straight-line block merging over Machine SSA.
//!
//! # Contents
//! - [`optimize`] joins every block into its layout predecessor when that
//!   predecessor ends in an unconditional `Jump` to it and nothing else
//!   reaches it, the maximal-basic-block shape V8's scheduler produces.
//!   Selection emits one block per guarded HIR node, so without this pass a
//!   run of straight-line accesses is a chain of single-edge blocks: every
//!   link costs a branch, and block-local reuse (binding guards, element
//!   views) stops at each one.
//!
//! # Invariants
//! - A merged pair is adjacent in layout, the predecessor's only successor is
//!   the merged block, the merged block's only predecessor is that
//!   predecessor, and the merged block has no parameters.
//! - Landing pads, loop preheaders, and OSR/entry blocks are never merged into
//!   a predecessor; they keep their identity for exception, LICM and entry
//!   metadata.
//! - Only the removed `Jump` instructions disappear; every other instruction
//!   keeps its order. Block ids are renumbered densely and every block
//!   reference (edges, entry, call landing pads) is remapped.
//!
//! # See also
//! - `super::gvn` reuses block-local proofs across the merged span.
//! - `super::dce` follows the same instruction-removal repair.

use rustc_hash::FxHashSet;

use super::{
    ExceptionalEdge, InstructionSequence, MachineBlock, MachineBlockData, MachineInstructionId,
    MachineOpcode, TargetSpec, VerificationError,
};

pub(super) fn optimize(
    mut sequence: InstructionSequence,
    target: &TargetSpec,
) -> Result<(InstructionSequence, u32), VerificationError> {
    let landing_pads = sequence
        .call_descriptors
        .iter()
        .filter_map(|descriptor| match descriptor.exceptional {
            ExceptionalEdge::LandingPad(block) => Some(block.0 as usize),
            ExceptionalEdge::None | ExceptionalEdge::Propagate => None,
        })
        .collect::<FxHashSet<_>>();
    let blocks = &sequence.blocks;
    let instructions = &sequence.instructions;
    // `joins[b]`: block `b` continues block `b - 1`.
    let mut joins = vec![false; blocks.len()];
    for index in 1..blocks.len() {
        let (previous, block) = (&blocks[index - 1], &blocks[index]);
        let terminator = &instructions[previous.end.0 as usize - 1];
        let marker = instructions[block.first.0 as usize..block.end.0 as usize]
            .iter()
            .any(|instruction| {
                matches!(
                    instruction.opcode,
                    MachineOpcode::LoopPreheader
                        | MachineOpcode::OsrEntry { .. }
                        | MachineOpcode::EntryValue(_)
                        | MachineOpcode::EntryThis
                )
            });
        joins[index] = terminator.opcode == MachineOpcode::Jump
            && terminator.operands.is_empty()
            && terminator.exits.is_empty()
            && terminator.safepoint.is_none()
            && terminator.frame_state.is_none()
            && previous.successors == [MachineBlock(index as u32)]
            && block.predecessors == [MachineBlock(index as u32 - 1)]
            && block.parameters.is_empty()
            && index != sequence.entry.0 as usize
            && !landing_pads.contains(&index)
            && !marker;
    }
    let merged = joins.iter().filter(|join| **join).count() as u32;
    if merged == 0 {
        return Ok((sequence, 0));
    }

    let mut new_id = Vec::with_capacity(blocks.len());
    let mut next = 0u32;
    for &join in &joins {
        if !join && !new_id.is_empty() {
            next += 1;
        }
        new_id.push(next);
    }
    let remap = |block: MachineBlock| MachineBlock(new_id[block.0 as usize]);

    let old_blocks = std::mem::take(&mut sequence.blocks);
    let old_instructions = std::mem::take(&mut sequence.instructions);
    let mut new_blocks: Vec<MachineBlockData> = Vec::with_capacity(old_blocks.len());
    let mut kept = Vec::with_capacity(old_instructions.len());
    for (index, block) in old_blocks.iter().enumerate() {
        let continues = index + 1 < old_blocks.len() && joins[index + 1];
        let (first, end) = (block.first.0 as usize, block.end.0 as usize);
        // A block that flows into its merged successor drops its `Jump`.
        let body_end = if continues { end - 1 } else { end };
        let start = MachineInstructionId(kept.len() as u32);
        kept.extend(old_instructions[first..body_end].iter().cloned());
        let finish = MachineInstructionId(kept.len() as u32);
        if joins[index] {
            let owner = new_blocks
                .last_mut()
                .expect("a joined block has a predecessor");
            owner.end = finish;
            owner.successors = block.successors.iter().copied().map(remap).collect();
            owner.successor_arguments = block.successor_arguments.clone();
        } else {
            new_blocks.push(MachineBlockData {
                first: start,
                end: finish,
                predecessors: block.predecessors.iter().copied().map(remap).collect(),
                successors: block.successors.iter().copied().map(remap).collect(),
                parameters: block.parameters.clone(),
                successor_arguments: block.successor_arguments.clone(),
            });
        }
    }
    for descriptor in &mut sequence.call_descriptors {
        if let ExceptionalEdge::LandingPad(block) = &mut descriptor.exceptional {
            *block = remap(*block);
        }
    }
    sequence.entry = remap(sequence.entry);
    sequence.blocks = new_blocks;
    sequence.instructions = kept;
    super::renumber_safepoints(&mut sequence.instructions);
    sequence.complete_gc_root_liveness();
    sequence.verify(target)?;
    Ok((sequence, merged))
}
