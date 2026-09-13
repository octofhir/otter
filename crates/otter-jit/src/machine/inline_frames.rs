//! Boxed inline-frame operands at committed Machine reentry.
//!
//! # Contents
//! - Verification of the shared frame schema against explicit safepoint roots.
//!
//! # Invariants
//! - Only committed named-property and binding cold calls carry these recipes.
//! - The published caller is represented by source identity, never copied slots.
//! - Every descendant value is tagged and an explicit allocator-visible root.

use super::*;

pub(super) fn verify(sequence: &InstructionSequence) -> Result<(), VerificationError> {
    for (index, instruction) in sequence.instructions.iter().enumerate() {
        let frames = &instruction.inline_frames;
        if frames.is_empty() {
            continue;
        }
        let id = MachineInstructionId(index as u32);
        let invalid = || VerificationError::OpcodeSignatureMismatch(id);
        let MachineOpcode::Call(descriptor) = instruction.opcode else {
            return Err(invalid());
        };
        let descriptor = &sequence.call_descriptors[descriptor as usize];
        if !matches!(descriptor.target, CallTarget::CommittedRuntime { target, .. }
            if target == otter_vm::native_abi::STUB_JIT_LOAD_PROPERTY
                || target == otter_vm::native_abi::STUB_JIT_BINDING_VALUE
                || target == otter_vm::native_abi::STUB_JIT_STORE_PROPERTY)
            || instruction.safepoint.is_none()
            || instruction.deopt.is_some()
            || frames.len() < 2
            || frames[0].entry.is_some()
            || !frames[0].slots.is_empty()
        {
            return Err(invalid());
        }
        let current = frames.last().ok_or_else(invalid)?;
        match descriptor.target {
            CallTarget::CommittedRuntime {
                target, byte_pc, ..
            } if target == otter_vm::native_abi::STUB_JIT_BINDING_VALUE => {
                if current.byte_pc != byte_pc {
                    return Err(invalid());
                }
            }
            _ => {
                let store = matches!(descriptor.target, CallTarget::CommittedRuntime { target, .. } if target == otter_vm::native_abi::STUB_JIT_STORE_PROPERTY);
                let cell = instruction.operands[if store { 2 } else { 1 }].value;
                let property = sequence
                    .instructions
                    .iter()
                    .find_map(|probe| match &probe.opcode {
                        MachineOpcode::PropertyLoad { site, .. }
                        | MachineOpcode::PropertyStore { site, .. }
                            if probe.operands[3].value == cell =>
                        {
                            Some(site)
                        }
                        _ => None,
                    })
                    .ok_or_else(invalid)?;
                if (current.function_id, current.byte_pc)
                    != (property.function_id, property.byte_pc)
                {
                    return Err(invalid());
                }
            }
        }
        for (index, frame) in frames.iter().enumerate().skip(1) {
            let entry = frame.entry.as_ref().ok_or_else(invalid)?;
            if entry.closure.is_none()
                || (index > 1
                    && usize::from(entry.return_register) >= frames[index - 1].slots.len())
            {
                return Err(invalid());
            }
            for value in frame
                .slots
                .iter()
                .chain([&entry.this, &entry.closure, &entry.new_target])
                .flatten()
            {
                if sequence.representations.get(value.0 as usize)
                    != Some(&MachineRepresentation::Tagged)
                    || !instruction
                        .operands
                        .contains(&MachineOperand::tagged_root(*value))
                {
                    return Err(invalid());
                }
            }
        }
    }
    Ok(())
}
