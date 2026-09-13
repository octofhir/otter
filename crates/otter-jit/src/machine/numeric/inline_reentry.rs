//! Inline activation recipes from cold SSA inputs to code-owned root homes.
//!
//! # Contents
//! - Cold-only boxing and explicit tagged-root operands before allocation.
//! - Post-allocation property-cell recipes using the shared VM frame schema.
//!
//! # Invariants
//! - The caller's register window is never copied; it stays natively published.
//! - Every descendant value comes from its exact allocator root save slot.
//! - Source PCs belong to each frame's function, independently of property facts.
//! - Only suspended parents adjust an after-call PC back to its call site.

use super::*;
use otter_vm::deopt::{DeoptFrame, DeoptFrameEntry};

pub(super) fn select_frames(
    hir: &NumericFunction,
    state_index: usize,
    values: &[MachineValue],
    representations: &mut Vec<MachineRepresentation>,
    instructions: &mut Vec<MachineInstruction>,
    call: &mut MachineInstruction,
) {
    let frames = &hir.frame_states[state_index].frames;
    if frames.len() <= 1 {
        return;
    }
    let mut boxed = BTreeMap::new();
    let mut slot = |source: &hir::NumericFrameSlot| match source {
        hir::NumericFrameSlot::Undefined => None,
        hir::NumericFrameSlot::Value(value) => Some(*boxed.entry(*value).or_insert_with(|| {
            tagged_call_argument(hir, values, representations, instructions, *value)
        })),
    };
    call.inline_frames = frames
        .iter()
        .enumerate()
        .map(|(index, frame)| DeoptFrame {
            function_id: frame.function_id,
            byte_pc: frame.byte_pc,
            entry: frame.entry.as_ref().map(|entry| DeoptFrameEntry {
                return_register: entry.return_register,
                this: slot(&entry.this),
                closure: slot(&entry.closure),
            }),
            slots: if index == 0 {
                Box::default()
            } else {
                frame.slots.iter().map(&mut slot).collect()
            },
        })
        .collect();
    append_unique_tagged_roots(&mut call.operands, boxed.into_values());
}

pub(super) fn prepare_property_cells(
    view: &JitCompileSnapshot,
    sequence: &InstructionSequence,
    safepoints: &super::super::MachineSafepointTable,
    code_object_id: u64,
    cells: &mut [crate::entry::WhiskerIcCell],
) -> Result<(), Unsupported> {
    let probes = sequence
        .instructions()
        .iter()
        .filter(|instruction| matches!(instruction.opcode, MachineOpcode::PropertyLoad { .. }));
    for (probe, cell) in probes.zip(cells) {
        let cell_value = probe.operands[3].value;
        let Some((index, cold)) =
            sequence
                .instructions()
                .iter()
                .enumerate()
                .find(|(_, instruction)| {
                    !instruction.inline_frames.is_empty()
                        && instruction
                            .operands
                            .get(1)
                            .is_some_and(|operand| operand.value == cell_value)
                })
        else {
            continue;
        };
        if cold.inline_frames[0].function_id != view.code_block.id {
            return Err(Unsupported::OperandShape("inline caller source owner"));
        }
        let safepoint = safepoints
            .site(MachineInstructionId(index as u32))
            .ok_or(Unsupported::OperandShape("inline property safepoint"))?;
        let slot = |source: &Option<MachineValue>| -> Result<Option<u16>, Unsupported> {
            source
                .map(|value| {
                    safepoint
                        .roots
                        .iter()
                        .find(|root| root.value == value)
                        .map(|root| root.save_slot)
                        .ok_or(Unsupported::OperandShape("inline activation root home"))
                })
                .transpose()
        };
        let frames = cold
            .inline_frames
            .iter()
            .enumerate()
            .skip(1)
            .map(|(index, frame)| {
                let entry = frame
                    .entry
                    .as_ref()
                    .ok_or(Unsupported::OperandShape("inline activation entry"))?;
                Ok(DeoptFrame {
                    function_id: frame.function_id,
                    byte_pc: if index + 1 < cold.inline_frames.len() {
                        frame_state::suspended_call_pc(view, frame.function_id, frame.byte_pc)
                            .ok_or(Unsupported::OperandShape("inline suspended source PC"))?
                            .1
                    } else {
                        frame.byte_pc
                    },
                    entry: Some(DeoptFrameEntry {
                        return_register: entry.return_register,
                        this: slot(&entry.this)?,
                        closure: slot(&entry.closure)?,
                    }),
                    slots: frame.slots.iter().map(&slot).collect::<Result<_, _>>()?,
                })
            })
            .collect::<Result<Box<[_]>, Unsupported>>()?;
        cell.set_inline_frames(code_object_id, safepoint.id.0, frames);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::property_selection_hir;
    use super::super::*;

    #[test]
    fn inline_property_frames_box_and_root_activation_only_values() {
        use otter_vm::deopt::{DeoptFrame, DeoptFrameEntry};
        let mut hir = property_selection_hir();
        let callable = hir::NumericValue(hir.nodes.len());
        hir.nodes.push(NumericNode::TaggedConstant(
            otter_vm::Value::function(94).to_bits(),
        ));
        let before_load = hir.blocks[0].nodes.len() - 1;
        hir.blocks[0].nodes.insert(before_load, callable);
        let state = &mut hir.frame_states[0];
        let mut child = state.frames[0].clone();
        child.entry = Some(DeoptFrameEntry {
            return_register: 0,
            this: hir::NumericFrameSlot::Value(hir::NumericValue(0)),
            closure: hir::NumericFrameSlot::Value(callable),
        });
        state.frames = Box::new([
            DeoptFrame {
                function_id: 999,
                byte_pc: 100,
                entry: None,
                slots: Box::default(),
            },
            child,
        ]);
        let sequence = select(&hir).expect("inline property selection");
        let (index, call) = sequence
            .instructions()
            .iter()
            .enumerate()
            .find(|(_, instruction)| !instruction.inline_frames.is_empty())
            .unwrap();
        assert_eq!(call.inline_frames.len(), 2);
        let child = &call.inline_frames[1];
        let entry = child.entry.as_ref().unwrap();
        let required: Vec<_> = child
            .slots
            .iter()
            .chain([&entry.this, &entry.closure])
            .flatten()
            .copied()
            .collect();
        assert!(required.len() >= 4);
        for value in &required {
            assert_eq!(
                sequence.representations()[value.0 as usize],
                MachineRepresentation::Tagged
            );
            assert!(call.operands.contains(&MachineOperand::tagged_root(*value)));
        }
        let allocation = sequence
            .allocate(&TargetRegisterFile::aarch64_scalar_function())
            .unwrap();
        let safepoints = lower_safepoints(&sequence, &allocation).unwrap();
        let site = safepoints.site(MachineInstructionId(index as u32)).unwrap();
        assert!(
            required
                .iter()
                .all(|value| site.roots.iter().any(|root| root.value == *value))
        );

        let mut missing_root = sequence.clone();
        missing_root.instructions[index]
            .operands
            .retain(|operand| *operand != MachineOperand::tagged_root(entry.closure.unwrap()));
        assert!(missing_root.verify().is_err());
        let mut foreign_source = sequence.clone();
        foreign_source.instructions[index].inline_frames[1].function_id += 1;
        assert!(foreign_source.verify().is_err());
        let mut missing_closure = sequence.clone();
        missing_closure.instructions[index].inline_frames[1]
            .entry
            .as_mut()
            .unwrap()
            .closure = None;
        assert!(missing_closure.verify().is_err());
        let mut copied_caller = sequence.clone();
        copied_caller.instructions[index].inline_frames[0].slots = Box::new([None]);
        assert!(copied_caller.verify().is_err());
    }
}
