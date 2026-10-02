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
//! - Every location that holds a copy of a root's bits at the call is saved
//!   and reloaded, not only the one the root operand names. regalloc2 assumes
//!   values are immutable: inside a block its redundant-move eliminator skips
//!   a move whose destination already holds a copy of the source, so a value
//!   can sit in a register and a spill slot at once. A collection that
//!   rewrote only the named copy would leave the other stale for a later
//!   elided move to read. This is Ion's `populateSafepoints` rule (record
//!   every live allocation of a GC value), derived here from the allocator
//!   output because regalloc2 reports one location per operand.
//!
//! # See also
//! - [`super::AllocatedMetadata`] for post-allocation root locations.
//! - [`super::MachineFrameLayout`] for save-area offsets.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use otter_vm::native_abi::{NO_FRAME_STATE, SafepointRecord, TaggedLocation};

use super::regalloc::edits_at;
use super::{
    AllocatedLocation, AllocatedSequence, AllocationPoint, InstructionSequence,
    MachineInstructionId, MachineValue, OperandPurpose, OperandRole, SafepointId,
};

/// One tagged value saved around an allocating Machine IR call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineSafepointRoot {
    /// Virtual value represented by this root.
    pub value: MachineValue,
    /// Exact regalloc2 location immediately before and after the call: the
    /// root operand's own location, or another location that holds a copy of
    /// the same bits at the call. A value's first root is its operand's.
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
    let copies = LocationCopies::new(sequence, allocation);
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
        for metadata in allocation.metadata_for(instruction_id) {
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
        let named = roots.len();
        for index in 0..named {
            let root = roots[index];
            for source in copies.aliases(instruction_id, root.source) {
                if roots.iter().any(|saved| saved.source == source) {
                    continue;
                }
                let save_slot = u16::try_from(roots.len())
                    .map_err(|_| MachineSafepointError::RootSlotOverflow(instruction_id))?;
                roots.push(MachineSafepointRoot {
                    value: root.value,
                    source,
                    save_slot,
                });
            }
        }
        let site_root_count = u16::try_from(roots.len())
            .map_err(|_| MachineSafepointError::RootSlotOverflow(instruction_id))?;
        root_slot_count = root_slot_count.max(site_root_count);
        let frame_state = instruction.frame_state.unwrap_or(NO_FRAME_STATE);
        records.push(SafepointRecord {
            inline_frames: Box::default(),
            call_pc: otter_vm::native_abi::NO_CALL_PC,
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

/// Which allocator locations hold the same bits at each safepoint.
///
/// A forward walk of each block tracks an abstract content per location:
/// an edit copies its source's content to its destination, and a definition
/// or clobber gives its location fresh content. Blocks start from nothing
/// known, matching the scope of regalloc2's redundant-move eliminator, which
/// forgets every copy at a block boundary.
struct LocationCopies {
    /// For each safepoint instruction, the content of every location that
    /// keeps its bits across the call: known after the edits before it, and
    /// neither defined nor clobbered by it.
    at_call: BTreeMap<MachineInstructionId, BTreeMap<AllocatedLocation, u32>>,
}

/// Abstract location contents inside one block.
#[derive(Default)]
struct ContentWalk {
    contents: BTreeMap<AllocatedLocation, u32>,
    next: u32,
}

impl ContentWalk {
    fn fresh(&mut self) -> u32 {
        self.next += 1;
        self.next
    }

    fn overwrite(&mut self, location: AllocatedLocation) {
        let content = self.fresh();
        self.contents.insert(location, content);
    }

    /// Run the edits at `point` in emission order.
    fn apply_edits(&mut self, allocation: &AllocatedSequence, point: AllocationPoint) {
        for edit in edits_at(allocation.edits(), point) {
            let content = match self.contents.get(&edit.from) {
                Some(content) => *content,
                None => {
                    let content = self.fresh();
                    self.contents.insert(edit.from, content);
                    content
                }
            };
            self.contents.insert(edit.to, content);
        }
    }
}

impl LocationCopies {
    fn new(sequence: &InstructionSequence, allocation: &AllocatedSequence) -> Self {
        let mut at_call = BTreeMap::new();
        let mut walk = ContentWalk::default();
        let instructions = sequence.instructions();
        for block in sequence.blocks() {
            walk.contents.clear();
            for index in block.first.0..block.end.0 {
                let id = MachineInstructionId(index);
                let instruction = &instructions[index as usize];
                walk.apply_edits(allocation, AllocationPoint::Before(id));
                let locations = allocation.instruction_locations(id).unwrap_or(&[]);
                let overwritten = instruction
                    .operands
                    .iter()
                    .zip(locations)
                    .filter(|(operand, _)| operand.role == OperandRole::Definition)
                    .map(|(_, location)| *location)
                    .chain(
                        instruction
                            .clobbers
                            .iter()
                            .map(|register| AllocatedLocation::Register(*register)),
                    )
                    .collect::<BTreeSet<_>>();
                if instruction.safepoint.is_some() {
                    let contents = walk
                        .contents
                        .iter()
                        .filter(|(location, _)| !overwritten.contains(location))
                        .map(|(location, content)| (*location, *content))
                        .collect();
                    at_call.insert(id, contents);
                }
                for location in overwritten {
                    walk.overwrite(location);
                }
                walk.apply_edits(allocation, AllocationPoint::After(id));
            }
        }
        Self { at_call }
    }

    /// Locations other than `source` that hold `source`'s bits across the
    /// call at `instruction`.
    fn aliases(
        &self,
        instruction: MachineInstructionId,
        source: AllocatedLocation,
    ) -> Vec<AllocatedLocation> {
        let Some(contents) = self.at_call.get(&instruction) else {
            return Vec::new();
        };
        let Some(content) = contents.get(&source) else {
            return Vec::new();
        };
        contents
            .iter()
            .filter(|(location, other)| **location != source && *other == content)
            .map(|(location, _)| *location)
            .collect()
    }
}
