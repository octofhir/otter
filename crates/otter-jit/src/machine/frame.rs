//! Post-register-allocation native frame layout.
//!
//! # Contents
//! - [`MachineFrameLayout`] — fixed frame bytes, aligned spill area, and exact
//!   stack-slot offsets shared by emitters and metadata.
//! - [`FrameLayoutError`] — checked arithmetic and invalid-alignment failures.
//!
//! # Invariants
//! - Every spill, root, and raw-cache slot is one eight-byte machine word.
//! - Allocator spills precede the reusable tagged-root save area, which in
//!   turn precedes untraced raw-cache words.
//! - Raw-cache words never enter safepoint or deoptimization root metadata.
//! - The total native reservation includes target-owned fixed bytes and is
//!   aligned to the target ABI requirement.
//! - Spill offsets are relative to the post-prologue stack pointer and never
//!   overlap target-owned fixed frame state.
//!
//! # See also
//! - [`crate::machine::TargetSpec`] — the sole owner of target frame inputs.
//! - [`crate::machine::regalloc`] — the allocation result framed here.

use super::AllocatedSequence;

const SPILL_SLOT_BYTES: u32 = 8;

/// Failure to construct or query one machine frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameLayoutError {
    /// Allocation and frame specifications name different targets.
    TargetMismatch,
    /// Alignment must be a non-zero power of two.
    InvalidAlignment,
    /// Frame byte arithmetic exceeded the metadata width.
    FrameSizeOverflow,
    /// Requested spill slot is outside the allocator-owned range.
    InvalidSpillSlot(u32),
    /// Requested raw-cache slot is outside the target-owned range.
    InvalidRawSlot(u16),
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
    root_slots: u16,
    root_area_offset: u32,
    raw_slots: u16,
    raw_area_offset: u32,
    spill_area_bytes: u32,
    frame_bytes: u32,
}

impl MachineFrameLayout {
    /// Build an aligned frame around one allocator result.
    pub fn new(
        allocation: &AllocatedSequence,
        root_slots: u16,
        fixed_bytes: u32,
        stack_alignment: u32,
    ) -> Result<Self, FrameLayoutError> {
        Self::new_with_raw_slots(allocation, root_slots, 0, fixed_bytes, stack_alignment)
    }

    /// Build an aligned frame with an additional untraced raw-cache area.
    pub fn new_with_raw_slots(
        allocation: &AllocatedSequence,
        root_slots: u16,
        raw_slots: u16,
        fixed_bytes: u32,
        stack_alignment: u32,
    ) -> Result<Self, FrameLayoutError> {
        if !stack_alignment.is_power_of_two() {
            return Err(FrameLayoutError::InvalidAlignment);
        }
        let allocator_spill_bytes = allocation
            .spill_slots()
            .checked_mul(SPILL_SLOT_BYTES)
            .ok_or(FrameLayoutError::FrameSizeOverflow)?;
        let root_area_offset = allocator_spill_bytes;
        let root_bytes = u32::from(root_slots)
            .checked_mul(SPILL_SLOT_BYTES)
            .ok_or(FrameLayoutError::FrameSizeOverflow)?;
        let raw_area_offset = root_area_offset
            .checked_add(root_bytes)
            .ok_or(FrameLayoutError::FrameSizeOverflow)?;
        let raw_bytes = u32::from(raw_slots)
            .checked_mul(SPILL_SLOT_BYTES)
            .ok_or(FrameLayoutError::FrameSizeOverflow)?;
        let raw_spill_bytes = allocator_spill_bytes
            .checked_add(root_bytes)
            .and_then(|bytes| bytes.checked_add(raw_bytes))
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
            root_slots,
            root_area_offset,
            raw_slots,
            raw_area_offset,
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

    /// Number of reusable tagged-root save slots.
    #[must_use]
    pub const fn root_slots(self) -> u16 {
        self.root_slots
    }

    /// Number of untraced target-owned raw-cache words.
    #[must_use]
    pub const fn raw_slots(self) -> u16 {
        self.raw_slots
    }

    /// Byte offset of one spill slot from the post-prologue stack pointer.
    pub fn spill_offset(self, slot: u32) -> Result<u32, FrameLayoutError> {
        if slot >= self.spill_slots {
            return Err(FrameLayoutError::InvalidSpillSlot(slot));
        }
        slot.checked_mul(SPILL_SLOT_BYTES)
            .ok_or(FrameLayoutError::FrameSizeOverflow)
    }

    /// Byte offset of one tagged-root save home from the post-prologue SP.
    pub fn root_offset(self, slot: u16) -> Result<u32, FrameLayoutError> {
        if slot >= self.root_slots {
            return Err(FrameLayoutError::InvalidSpillSlot(u32::from(slot)));
        }
        u32::from(slot)
            .checked_mul(SPILL_SLOT_BYTES)
            .and_then(|offset| self.root_area_offset.checked_add(offset))
            .ok_or(FrameLayoutError::FrameSizeOverflow)
    }

    /// Byte offset of one untraced raw-cache word from the post-prologue SP.
    pub fn raw_offset(self, slot: u16) -> Result<u32, FrameLayoutError> {
        if slot >= self.raw_slots {
            return Err(FrameLayoutError::InvalidRawSlot(slot));
        }
        u32::from(slot)
            .checked_mul(SPILL_SLOT_BYTES)
            .and_then(|offset| self.raw_area_offset.checked_add(offset))
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
        TargetSpec,
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
            &TargetSpec::aarch64(),
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
            .allocate(&TargetSpec::aarch64())
            .expect("pressure sequence allocates");
        assert!(allocation.spill_slots() > 0);

        let layout = MachineFrameLayout::new(&allocation, 3, 16, 16).expect("valid frame");
        assert_eq!(layout.frame_bytes() % 16, 0);
        assert!(layout.spill_area_bytes() >= allocation.spill_slots() * 8);
        assert_eq!(layout.spill_offset(0), Ok(0));
        assert_eq!(layout.root_slots(), 3);
        assert_eq!(
            layout.root_offset(0),
            Ok(allocation.spill_slots() * SPILL_SLOT_BYTES)
        );
        assert_eq!(
            layout.spill_offset(allocation.spill_slots() - 1),
            Ok((allocation.spill_slots() - 1) * 8)
        );
        assert!(matches!(
            layout.spill_offset(allocation.spill_slots()),
            Err(FrameLayoutError::InvalidSpillSlot(_))
        ));

        let raw = MachineFrameLayout::new_with_raw_slots(&allocation, 3, 4, 16, 16)
            .expect("valid raw-cache frame");
        assert_eq!(raw.raw_slots(), 4);
        assert_eq!(raw.raw_offset(0), Ok(raw.root_offset(2).unwrap() + 8));
        assert_eq!(raw.raw_offset(3), Ok(raw.raw_offset(0).unwrap() + 24));
        assert_eq!(raw.raw_offset(4), Err(FrameLayoutError::InvalidRawSlot(4)));
        assert!(raw.frame_bytes() >= layout.frame_bytes() + 32);
    }
}
