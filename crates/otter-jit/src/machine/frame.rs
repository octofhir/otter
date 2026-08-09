//! Post-register-allocation native frame layout.
//!
//! # Contents
//! - [`MachineFrameLayout`] — fixed frame bytes, aligned spill area, and exact
//!   stack-slot offsets shared by emitters and metadata.
//! - [`FrameLayoutError`] — checked arithmetic and invalid-alignment failures.
//!
//! # Invariants
//! - Every spill slot is one eight-byte machine word.
//! - The total native reservation includes target-owned fixed bytes and is
//!   aligned to the target ABI requirement.
//! - Spill offsets are relative to the post-prologue stack pointer and never
//!   overlap target-owned fixed frame state.

use super::AllocatedSequence;

const SPILL_SLOT_BYTES: u32 = 8;

/// Failure to construct or query one machine frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameLayoutError {
    /// Alignment must be a non-zero power of two.
    InvalidAlignment,
    /// Frame byte arithmetic exceeded the metadata width.
    FrameSizeOverflow,
    /// Requested spill slot is outside the allocator-owned range.
    InvalidSpillSlot(u32),
}

impl std::fmt::Display for FrameLayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Machine IR frame layout failed: {self:?}")
    }
}

impl std::error::Error for FrameLayoutError {}

/// Exact native-frame reservation finalized after register allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineFrameLayout {
    fixed_bytes: u32,
    spill_slots: u32,
    spill_area_bytes: u32,
    frame_bytes: u32,
}

impl MachineFrameLayout {
    /// Build an aligned frame around one allocator result.
    pub fn new(
        allocation: &AllocatedSequence,
        fixed_bytes: u32,
        stack_alignment: u32,
    ) -> Result<Self, FrameLayoutError> {
        if !stack_alignment.is_power_of_two() {
            return Err(FrameLayoutError::InvalidAlignment);
        }
        let raw_spill_bytes = allocation
            .spill_slots()
            .checked_mul(SPILL_SLOT_BYTES)
            .ok_or(FrameLayoutError::FrameSizeOverflow)?;
        let unaligned_total = fixed_bytes
            .checked_add(raw_spill_bytes)
            .ok_or(FrameLayoutError::FrameSizeOverflow)?;
        let frame_bytes = align_up(unaligned_total, stack_alignment)?;
        let spill_area_bytes = frame_bytes
            .checked_sub(fixed_bytes)
            .ok_or(FrameLayoutError::FrameSizeOverflow)?;
        Ok(Self {
            fixed_bytes,
            spill_slots: allocation.spill_slots(),
            spill_area_bytes,
            frame_bytes,
        })
    }

    /// Target-owned bytes saved before the spill area is reserved.
    #[must_use]
    pub const fn fixed_bytes(self) -> u32 {
        self.fixed_bytes
    }

    /// Aligned bytes reserved below the fixed frame state.
    #[must_use]
    pub const fn spill_area_bytes(self) -> u32 {
        self.spill_area_bytes
    }

    /// Complete native stack reservation published with the code object.
    #[must_use]
    pub const fn frame_bytes(self) -> u32 {
        self.frame_bytes
    }

    /// Byte offset of one spill slot from the post-prologue stack pointer.
    pub fn spill_offset(self, slot: u32) -> Result<u32, FrameLayoutError> {
        if slot >= self.spill_slots {
            return Err(FrameLayoutError::InvalidSpillSlot(slot));
        }
        slot.checked_mul(SPILL_SLOT_BYTES)
            .ok_or(FrameLayoutError::FrameSizeOverflow)
    }
}

fn align_up(value: u32, alignment: u32) -> Result<u32, FrameLayoutError> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
        .ok_or(FrameLayoutError::FrameSizeOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{
        ControlFlow, InstructionSequence, MachineBlock, MachineBlockData, MachineInstruction,
        MachineInstructionId, MachineOpcode, MachineOperand, MachineRepresentation, MachineValue,
        TargetRegisterFile,
    };

    #[test]
    fn frame_reservation_aligns_allocator_spills_without_overlap() {
        let values = (0..32).map(MachineValue).collect::<Vec<_>>();
        let mut instructions = values
            .iter()
            .copied()
            .map(|value| {
                MachineInstruction::plain(
                    MachineOpcode::FloatConstant(0),
                    vec![MachineOperand::register_output(value)],
                )
            })
            .collect::<Vec<_>>();
        let mut representations = vec![MachineRepresentation::Float64; values.len()];
        let mut result = values[0];
        for &right in &values[1..] {
            let output = MachineValue(representations.len() as u32);
            representations.push(MachineRepresentation::Float64);
            instructions.push(MachineInstruction::plain(
                MachineOpcode::FloatAdd,
                vec![
                    MachineOperand::register_input(result),
                    MachineOperand::register_input(right),
                    MachineOperand::register_output(output),
                ],
            ));
            result = output;
        }
        let mut ret = MachineInstruction::plain(
            MachineOpcode::Return,
            vec![MachineOperand::register_input(result)],
        );
        ret.control = ControlFlow::Return;
        instructions.push(ret);
        let end = MachineInstructionId(instructions.len() as u32);
        let sequence = InstructionSequence::new(
            MachineBlock(0),
            representations,
            Vec::new(),
            vec![MachineBlockData {
                first: MachineInstructionId(0),
                end,
                predecessors: Vec::new(),
                successors: Vec::new(),
                parameters: Vec::new(),
                successor_arguments: Vec::new(),
            }],
            instructions,
        )
        .expect("valid pressure sequence");
        let allocation = sequence
            .allocate(&TargetRegisterFile::aarch64_scalar_function())
            .expect("pressure sequence allocates");
        assert!(allocation.spill_slots() > 0);

        let layout = MachineFrameLayout::new(&allocation, 16, 16).expect("valid frame");
        assert_eq!(layout.frame_bytes() % 16, 0);
        assert!(layout.spill_area_bytes() >= allocation.spill_slots() * 8);
        assert_eq!(layout.spill_offset(0), Ok(0));
        assert_eq!(
            layout.spill_offset(allocation.spill_slots() - 1),
            Ok((allocation.spill_slots() - 1) * 8)
        );
        assert!(matches!(
            layout.spill_offset(allocation.spill_slots()),
            Err(FrameLayoutError::InvalidSpillSlot(_))
        ));
    }
}
