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

/// Rust resolver behind a machine-visible [`CodeRegistryView`].
pub type SafepointResolverFn = unsafe extern "C" fn(
    context: u64,
    code_object_id: u64,
    safepoint_id: SafepointId,
) -> *const SafepointRecord;

/// Fixed code-registry lookup surface published on [`super::VmThread`].
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeRegistryView {
    /// Opaque resolver-owned context address.
    pub context: u64,
    /// Address of a [`SafepointResolverFn`].
    pub resolve_safepoint: u64,
}

impl CodeRegistryView {
    /// Resolve one code-object-local safepoint record.
    ///
    /// # Safety
    /// The resolver and context must remain live for the active native call.
    #[must_use]
    pub unsafe fn resolve(
        self,
        code_object_id: u64,
        safepoint_id: SafepointId,
    ) -> Option<*const SafepointRecord> {
        if self.resolve_safepoint == 0 {
            return None;
        }
        // SAFETY: guaranteed by the registry publisher.
        let resolver: SafepointResolverFn = unsafe {
            std::mem::transmute::<usize, SafepointResolverFn>(self.resolve_safepoint as usize)
        };
        let record = unsafe { resolver(self.context, code_object_id, safepoint_id) };
        (!record.is_null()).then_some(record)
    }
}

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
    /// Register, frame-slot, or spill-slot index.
    pub index: u16,
}

impl TaggedLocation {
    /// Tagged interpreter-visible frame slot.
    #[must_use]
    pub const fn frame_slot(index: u16) -> Self {
        Self {
            kind: TaggedLocationKind::FrameSlot,
            index,
        }
    }

    /// Tagged machine register.
    #[must_use]
    pub const fn machine_register(index: u16) -> Self {
        Self {
            kind: TaggedLocationKind::MachineRegister,
            index,
        }
    }

    /// Tagged native spill slot.
    #[must_use]
    pub const fn spill_slot(index: u16) -> Self {
        Self {
            kind: TaggedLocationKind::SpillSlot,
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

    /// Expand one compact frame bitmap into the collector-facing root record.
    ///
    /// `bitmap_words` is the owning code object's complete immutable bitmap
    /// table. `None` rejects a descriptor whose range or width does not cover
    /// exactly `slot_count` frame slots.
    #[must_use]
    pub fn from_frame_map(
        frame_map: FrameMap,
        frame_state: FrameStateId,
        bitmap_words: &[u64],
    ) -> Option<Self> {
        let required_words = usize::from(frame_map.slot_count).div_ceil(u64::BITS as usize);
        if usize::from(frame_map.bitmap_word_count) != required_words {
            return None;
        }
        let start = usize::try_from(frame_map.bitmap_offset).ok()?;
        let end = start.checked_add(required_words)?;
        let words = bitmap_words.get(start..end)?;
        let tagged_locations = (0..frame_map.slot_count)
            .filter(|slot| {
                let slot = usize::from(*slot);
                words[slot / u64::BITS as usize] & (1_u64 << (slot % u64::BITS as usize)) != 0
            })
            .map(TaggedLocation::frame_slot)
            .collect();
        Some(Self {
            id: frame_map.id,
            frame_state,
            tagged_locations,
        })
    }

    /// Whether this map can reconstruct interpreter-visible state.
    #[must_use]
    pub fn has_deopt_state(&self) -> bool {
        self.frame_state != NO_FRAME_STATE
    }
}

const _: [(); 4] = [(); std::mem::size_of::<TaggedLocation>()];
const _: [(); 16] = [(); std::mem::size_of::<CodeRegistryView>()];
const _: [(); 8] = [(); std::mem::align_of::<CodeRegistryView>()];
const _: [(); 12] = [(); std::mem::size_of::<FrameMap>()];
const _: [(); 12] = [(); std::mem::size_of::<SpillMap>()];
const _: [(); 28] = [(); std::mem::size_of::<SafepointEntry>()];
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

    #[test]
    fn precise_frame_map_expands_only_set_slots() {
        let map = FrameMap {
            id: 7,
            bitmap_offset: 1,
            bitmap_word_count: 2,
            slot_count: 65,
        };
        let record = SafepointRecord::from_frame_map(map, NO_FRAME_STATE, &[u64::MAX, 0b101, 1])
            .expect("valid precise frame map");
        assert_eq!(
            record.tagged_locations,
            vec![
                TaggedLocation::frame_slot(0),
                TaggedLocation::frame_slot(2),
                TaggedLocation::frame_slot(64),
            ]
        );
    }
}
