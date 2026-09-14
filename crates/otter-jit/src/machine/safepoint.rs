//! Post-allocation Machine IR safepoint root lowering.
//!
//! # Contents
//! - [`MachineSafepointTable`] — dense call sites and VM root records.
//! - [`MachineSafepointSite`] — exact allocator sources and native save homes.
//! - [`lower_safepoints`] — converts late-use root metadata after regalloc2.
//!
//! # Invariants
//! - Every moving tagged root is copied from its exact allocator location into
//!   one rewriteable native save slot before an allocating call. Inline
//!   descendants reference those exact save slots in the VM-owned record.
//! - VM [`otter_vm::native_abi::SafepointRecord`] entries name those same save
//!   slots; emitters own no parallel root recipe.
//! - Machine-register roots never reach the collector directly. Generated code
//!   reloads every save home after the call, including values rewritten by GC.
//!
//! # See also
//! - [`super::AllocatedMetadata`] for post-allocation root locations.
//! - [`super::MachineFrameLayout`] for save-area offsets.

use std::fmt::Write as _;

use otter_vm::native_abi::{NO_FRAME_STATE, SafepointRecord, TaggedLocation};

use super::{
    AllocatedLocation, AllocatedSequence, InstructionSequence, MachineInstructionId, MachineValue,
    OperandPurpose, SafepointId,
};

/// One tagged value saved around an allocating Machine IR call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineSafepointRoot {
    /// Virtual value represented by this root.
    pub value: MachineValue,
    /// Exact regalloc2 location immediately before and after the call.
    pub source: AllocatedLocation,
    /// Dense index in the frame's native root-save area.
    pub save_slot: u16,
}

/// One allocating instruction and its precise roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineSafepointSite {
    /// Selected instruction that owns the call.
    pub instruction: MachineInstructionId,
    /// Dense code-object-local safepoint id.
    pub id: SafepointId,
    /// Roots saved and reloaded around the call.
    pub roots: Box<[MachineSafepointRoot]>,
}

/// Complete post-allocation safepoint contract for one code object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineSafepointTable {
    sites: Box<[MachineSafepointSite]>,
    records: Box<[SafepointRecord]>,
    root_slot_count: u16,
}

impl MachineSafepointTable {
    /// Safepoint site for one selected instruction.
    #[must_use]
    pub fn site(&self, instruction: MachineInstructionId) -> Option<&MachineSafepointSite> {
        self.sites
            .binary_search_by_key(&instruction, |site| site.instruction)
            .ok()
            .map(|index| &self.sites[index])
    }

    /// VM-owned collector records in dense safepoint-id order.
    #[must_use]
    pub fn records(&self) -> &[SafepointRecord] {
        &self.records
    }

    pub(crate) fn records_mut(&mut self) -> &mut [SafepointRecord] {
        &mut self.records
    }

    /// Maximum simultaneous native root-save slots required by this function.
    #[must_use]
    pub const fn root_slot_count(&self) -> u16 {
        self.root_slot_count
    }

    /// Whether this code object contains no GC safepoints.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.sites.is_empty()
    }

    /// Deterministic artifact identity for allocator root homes.
    #[must_use]
    pub fn normalized(&self) -> String {
        let mut output = format!("safepoints root-slots={}\n", self.root_slot_count);
        for site in &self.sites {
            writeln!(
                output,
                "sp{} i{} roots={:?}",
                site.id.0, site.instruction.0, site.roots
            )
            .expect("writing to String cannot fail");
        }
        output
    }
}

/// Invalid post-allocation safepoint metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineSafepointError {
    /// Safepoint ids must be dense in instruction order for registry lookup.
    NonDenseId {
        /// Dense id required at this table position.
        expected: u32,
        /// Id attached to the instruction.
        actual: u32,
    },
    /// Root metadata named a non-tagged/raw-cell root not supported by the
    /// current VM spill-slot record.
    UnsupportedRoot(MachineInstructionId, MachineValue),
    /// A root metadata row did not carry the instruction's safepoint id.
    RootSafepointMismatch(MachineInstructionId, MachineValue),
    /// One instruction named the same virtual root more than once.
    DuplicateRoot(MachineInstructionId, MachineValue),
    /// A site needs more root-save homes than the ABI can index.
    RootSlotOverflow(MachineInstructionId),
}

impl std::fmt::Display for MachineSafepointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Machine IR safepoint lowering failed: {self:?}")
    }
}

impl std::error::Error for MachineSafepointError {}

/// Lower allocator metadata into emitter save/reload sites and VM records.
pub fn lower_safepoints(
    sequence: &InstructionSequence,
    allocation: &AllocatedSequence,
) -> Result<MachineSafepointTable, MachineSafepointError> {
    let mut sites = Vec::new();
    let mut records = Vec::new();
    let mut root_slot_count = 0_u16;
    for (instruction_index, instruction) in sequence.instructions().iter().enumerate() {
        let Some(id) = instruction.safepoint else {
            continue;
        };
        let instruction_id = MachineInstructionId(instruction_index as u32);
        let expected = records.len() as u32;
        if id.0 != expected {
            return Err(MachineSafepointError::NonDenseId {
                expected,
                actual: id.0,
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut roots = Vec::new();
        for metadata in allocation
            .metadata()
            .iter()
            .filter(|metadata| metadata.instruction == instruction_id)
        {
            match metadata.purpose {
                OperandPurpose::TaggedRoot | OperandPurpose::RuntimeRoot => {}
                OperandPurpose::CellRoot => {
                    return Err(MachineSafepointError::UnsupportedRoot(
                        instruction_id,
                        metadata.value,
                    ));
                }
                OperandPurpose::FrameState | OperandPurpose::Input | OperandPurpose::Output => {
                    continue;
                }
            }
            if metadata.safepoint != Some(id) {
                return Err(MachineSafepointError::RootSafepointMismatch(
                    instruction_id,
                    metadata.value,
                ));
            }
            if !seen.insert(metadata.value) {
                return Err(MachineSafepointError::DuplicateRoot(
                    instruction_id,
                    metadata.value,
                ));
            }
            let save_slot = u16::try_from(roots.len())
                .map_err(|_| MachineSafepointError::RootSlotOverflow(instruction_id))?;
            roots.push(MachineSafepointRoot {
                value: metadata.value,
                source: metadata.location,
                save_slot,
            });
        }
        let site_root_count = u16::try_from(roots.len())
            .map_err(|_| MachineSafepointError::RootSlotOverflow(instruction_id))?;
        root_slot_count = root_slot_count.max(site_root_count);
        let frame_state = instruction.frame_state.unwrap_or(NO_FRAME_STATE);
        records.push(SafepointRecord {
            inline_frames: Box::default(),
            inline_frames_published: false,
            id: id.0,
            frame_state,
            tagged_locations: (0..site_root_count)
                .map(TaggedLocation::spill_slot)
                .collect(),
        });
        sites.push(MachineSafepointSite {
            instruction: instruction_id,
            id,
            roots: roots.into_boxed_slice(),
        });
    }
    Ok(MachineSafepointTable {
        sites: sites.into_boxed_slice(),
        records: records.into_boxed_slice(),
        root_slot_count,
    })
}
