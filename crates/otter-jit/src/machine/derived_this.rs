//! Derived-this probe CFG validation and source attribution.
//!
//! # Contents
//! - Cold region identity and matching committed-call validation.
//!
//! # Invariants
//! - Successful probes bypass the canonical binding call.
//! - The cold sibling owns the original source PC and exception boundary.
//!
//! # See also
//! - `committed_probe` — shared CFG expansion before register allocation.

use super::{
    CallTarget, MachineBlock, MachineInstructionId, MachineOpcode, MachineOperand, MachineValue,
};

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
