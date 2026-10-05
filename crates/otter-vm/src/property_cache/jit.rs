//! Native layout of the isolate's one shared property-action table.
//!
//! # Contents
//! - [`JitPropertyActionCache`] describes its stable base, key and fact offsets.
//! - Compile-time assertions pin the one physical entry on supported targets.
//!
//! # Invariants
//! Installed code belongs to the same isolate as this fixed table. A key owns
//! independent load/store facts, never a moving receiver, value or slab address.
//! Native probes do not allocate or reenter; canonical publication retains proof
//! owners before exposing complete entries. Target words are the actual GC roots,
//! while holder words are weak and cleared before reuse. No mirrored table exists.
//!
//! # See also
//! - `super` owns publication, collection and the runtime consumers.
//! - `crate::jit::JitCompileSnapshot` carries the immutable layout DTO.

use std::mem::{offset_of, size_of};

use super::{
    HASH_ATOM_MULTIPLIER, HASH_SHAPE_MULTIPLIER, HASH_SHIFT, PropertyActionCache,
    PropertyActionEntry, SETS, WAYS,
};

/// Borrow-free layout of the current isolate's shared key/action table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitPropertyActionCache {
    /// Process-local base of the one fixed scalar table.
    pub table_addr: usize,
    /// Byte stride of one key entry.
    pub entry_bytes: u32,
    /// Power-of-two set count minus one.
    pub set_mask: u32,
    /// Consecutive entries in one set.
    pub ways: u32,
    /// Exact receiver u64 shape identity offset.
    pub receiver_shape_id_byte: u32,
    /// Exact isolate u32 property atom offset.
    pub atom_byte: u32,
    /// Canonical PropertyLoadAction discriminant offset.
    pub load_action_byte: u32,
    /// Canonical PropertyStoreAction discriminant offset.
    pub store_action_byte: u32,
    /// Logical u16 load slot, independent of the store slot.
    pub load_slot_byte: u32,
    /// Logical u16 own-overwrite or append slot.
    pub store_slot_byte: u32,
    /// Recorded load descriptor writability, independent of store authorization.
    pub load_writable_byte: u32,
    /// Weak compressed holder shape offset.
    pub holder_shape_byte: u32,
    /// Weak compressed holder instance-root shape offset.
    pub holder_root_byte: u32,
    /// Exact holder u64 shape identity offset.
    pub holder_shape_id_byte: u32,
    /// Traced compressed append child offset, zero for runtime-only recipes.
    pub target_shape_byte: u32,
    /// Exact child u64 shape identity offset.
    pub target_shape_id_byte: u32,
    /// Retained complete load-chain validity-word address offset.
    pub load_validity_byte: u32,
    /// Retained append-chain validity-word address offset; zero means null chain.
    pub store_validity_byte: u32,
    /// Header-inclusive immutable identity offset within a shape cell.
    pub shape_id_byte: u32,
    /// Receiver shape hash multiplier.
    pub hash_shape_multiplier: u64,
    /// Property atom hash multiplier.
    pub hash_atom_multiplier: u64,
    /// Shift after XORing both wrapping products.
    pub hash_shift: u8,
}

impl PropertyActionCache {
    pub(crate) fn jit_layout(&self) -> JitPropertyActionCache {
        JitPropertyActionCache {
            table_addr: self.entries[0].as_ptr() as usize,
            entry_bytes: size_of::<PropertyActionEntry>() as u32,
            set_mask: (SETS - 1) as u32,
            ways: WAYS as u32,
            receiver_shape_id_byte: offset_of!(PropertyActionEntry, receiver_shape_id) as u32,
            atom_byte: offset_of!(PropertyActionEntry, atom) as u32,
            load_action_byte: offset_of!(PropertyActionEntry, load_action) as u32,
            store_action_byte: offset_of!(PropertyActionEntry, store_action) as u32,
            load_slot_byte: offset_of!(PropertyActionEntry, load_slot) as u32,
            store_slot_byte: offset_of!(PropertyActionEntry, store_slot) as u32,
            load_writable_byte: offset_of!(PropertyActionEntry, load_writable) as u32,
            holder_shape_byte: offset_of!(PropertyActionEntry, holder_shape) as u32,
            holder_root_byte: offset_of!(PropertyActionEntry, holder_root) as u32,
            holder_shape_id_byte: offset_of!(PropertyActionEntry, holder_shape_id) as u32,
            target_shape_byte: offset_of!(PropertyActionEntry, target_shape) as u32,
            target_shape_id_byte: offset_of!(PropertyActionEntry, target_shape_id) as u32,
            load_validity_byte: offset_of!(PropertyActionEntry, load_validity) as u32,
            store_validity_byte: offset_of!(PropertyActionEntry, store_validity) as u32,
            shape_id_byte: (otter_gc::header::HEADER_SIZE + crate::object::SHAPE_BODY_ID_OFFSET)
                as u32,
            hash_shape_multiplier: HASH_SHAPE_MULTIPLIER,
            hash_atom_multiplier: HASH_ATOM_MULTIPLIER,
            hash_shift: HASH_SHIFT,
        }
    }
}

const _: () = {
    assert!(SETS.is_power_of_two());
    assert!(size_of::<PropertyActionEntry>() == 64);
    assert!(std::mem::align_of::<PropertyActionEntry>() == 8);
    assert!(size_of::<std::cell::Cell<PropertyActionEntry>>() == size_of::<PropertyActionEntry>());
    assert!(offset_of!(PropertyActionEntry, receiver_shape_id) == 0);
    assert!(offset_of!(PropertyActionEntry, atom) == 8);
    assert!(offset_of!(PropertyActionEntry, load_action) == 12);
    assert!(offset_of!(PropertyActionEntry, store_action) == 13);
    assert!(offset_of!(PropertyActionEntry, load_slot) == 14);
    assert!(offset_of!(PropertyActionEntry, store_slot) == 16);
    assert!(offset_of!(PropertyActionEntry, load_writable) == 18);
    assert!(offset_of!(PropertyActionEntry, reserved) == 19);
    assert!(offset_of!(PropertyActionEntry, holder_shape) == 20);
    assert!(offset_of!(PropertyActionEntry, holder_root) == 24);
    assert!(offset_of!(PropertyActionEntry, target_shape) == 28);
    assert!(offset_of!(PropertyActionEntry, holder_shape_id) == 32);
    assert!(offset_of!(PropertyActionEntry, target_shape_id) == 40);
    assert!(offset_of!(PropertyActionEntry, load_validity) == 48);
    assert!(offset_of!(PropertyActionEntry, store_validity) == 56);
    assert!(size_of::<crate::object::ShapeHandle>() == size_of::<otter_gc::raw::RawGc>());
};
