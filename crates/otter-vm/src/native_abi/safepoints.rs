//! Code-object-owned safepoint and frame-map contracts.
//!
//! # Contents
//! - [`FrameMap`] and [`SpillMap`] index immutable code-object side tables.
//! - [`SafepointEntry`] maps a native return PC to logical state and a stub id.
//! - [`SafepointRecord`] is the VM-owned expanded root map used during the
//!   current baseline migration.
//!
//! # Invariants
//! - Machine code publishes only `(code_object_id, safepoint_id)`; it never
//!   supplies a raw metadata-table pointer.
//! - Every live tagged value is named exactly once by a frame or spill map.
//! - Machine-register roots are saved to mapped spill slots before an
//!   allocating or reentrant call.
//!
//! # See also
//! - [`super::frame`] for the published activation.
//! - [`super::metadata::CodeObjectMetadata`] for table ownership.

use super::{FrameStateId, NO_FRAME_STATE, RuntimeStubId, SafepointId};

/// Storage class for one tagged value location at a safepoint.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaggedLocationKind {
    /// Interpreter-visible register-window slot.
    FrameSlot = 0,
    /// Machine register in platform ABI numbering.
    MachineRegister = 1,
    /// Native spill slot relative to the spill-area base.
    SpillSlot = 2,
}

/// One tagged `Value` location live at a safepoint.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaggedLocation {
    /// Storage class.
    pub kind: TaggedLocationKind,
    /// Reserved; zero in layout version 2.
    pub reserved: u8,
    /// Register, frame-slot, or spill-slot index.
    pub index: u16,
}

impl TaggedLocation {
    /// Tagged interpreter-visible frame slot.
    #[must_use]
    pub const fn frame_slot(index: u16) -> Self {
        Self {
            kind: TaggedLocationKind::FrameSlot,
            reserved: 0,
            index,
        }
    }

    /// Tagged machine register.
    #[must_use]
    pub const fn machine_register(index: u16) -> Self {
        Self {
            kind: TaggedLocationKind::MachineRegister,
            reserved: 0,
            index,
        }
    }

    /// Tagged native spill slot.
    #[must_use]
    pub const fn spill_slot(index: u16) -> Self {
        Self {
            kind: TaggedLocationKind::SpillSlot,
            reserved: 0,
            index,
        }
    }
}

/// Compact immutable frame-root map descriptor.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMap {
    /// Dense map id local to the owning code object.
    pub id: u32,
    /// First bitmap word in the code object's frame-map table.
    pub bitmap_offset: u32,
    /// Number of bitmap words.
    pub bitmap_word_count: u16,
    /// Number of initialized frame slots covered by the map.
    pub slot_count: u16,
}

/// Compact immutable native spill-root map descriptor.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpillMap {
    /// Dense map id local to the owning code object.
    pub id: u32,
    /// First spill offset in the code object's location table.
    pub location_offset: u32,
    /// Number of tagged spill locations.
    pub location_count: u16,
    /// Reserved; zero in layout version 2.
    pub reserved: u16,
}

/// Machine-code return-PC safepoint entry.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafepointEntry {
    /// Dense id local to the owning code object.
    pub id: SafepointId,
    /// Native return offset within the owning code object.
    pub native_return_offset: u32,
    /// Canonical instruction-index PC published before the call.
    pub logical_pc: u32,
    /// [`FrameMap::id`].
    pub frame_map_id: u32,
    /// [`SpillMap::id`].
    pub spill_map_id: u32,
    /// Deopt/side-exit frame state or [`NO_FRAME_STATE`].
    pub frame_state_id: FrameStateId,
    /// Runtime stub invoked at this return PC.
    pub stub_id: RuntimeStubId,
    /// Reserved; zero in layout version 2.
    pub reserved: u32,
}

/// VM-owned expanded root map used by the current collector integration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafepointRecord {
    /// Stable safepoint id.
    pub id: SafepointId,
    /// Frame-state snapshot or [`NO_FRAME_STATE`].
    pub frame_state: FrameStateId,
    /// Tagged values visible to the moving collector.
    pub tagged_locations: Vec<TaggedLocation>,
}

impl SafepointRecord {
    /// Build a full interpreter-visible register-window root set.
    #[must_use]
    pub fn frame_slot_window(
        id: SafepointId,
        frame_state: FrameStateId,
        register_count: u16,
    ) -> Self {
        Self {
            id,
            frame_state,
            tagged_locations: (0..register_count)
                .map(TaggedLocation::frame_slot)
                .collect(),
        }
    }

    /// Whether this map can reconstruct interpreter-visible state.
    #[must_use]
    pub fn has_deopt_state(&self) -> bool {
        self.frame_state != NO_FRAME_STATE
    }
}

const _: [(); 4] = [(); std::mem::size_of::<TaggedLocation>()];
const _: [(); 12] = [(); std::mem::size_of::<FrameMap>()];
const _: [(); 12] = [(); std::mem::size_of::<SpillMap>()];
const _: [(); 32] = [(); std::mem::size_of::<SafepointEntry>()];
const _: [(); 4] = [(); std::mem::align_of::<SafepointEntry>()];
const _: [(); 8] = [(); std::mem::offset_of!(SafepointEntry, logical_pc)];
const _: [(); 24] = [(); std::mem::offset_of!(SafepointEntry, stub_id)];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_slot_window_covers_every_register() {
        let record = SafepointRecord::frame_slot_window(3, NO_FRAME_STATE, 4);
        assert_eq!(record.tagged_locations.len(), 4);
        assert_eq!(record.tagged_locations[3], TaggedLocation::frame_slot(3));
        assert!(!record.has_deopt_state());
    }
}
