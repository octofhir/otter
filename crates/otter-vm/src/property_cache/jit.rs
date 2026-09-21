//! Generated reads of the isolate's existing property lookup table.
//!
//! # Contents
//! - [`JitPropertyLookupCache`] declares scalar address, hash and field offsets.
//! - [`PropertyLookupCache::jit_layout`] snapshots the one live table's layout.
//!
//! # Invariants
//! - The fixed boxed table is allocated with its interpreter and never replaced
//!   or resized. Installed code belongs to that same isolate; a saved compile
//!   DTO does not authorize entry after the isolate has been destroyed.
//! - VM publication replaces one complete `Cell<Entry>` on the owning mutator
//!   thread. A generated probe neither allocates nor reenters, so it cannot race
//!   an entry update and needs no atomic publication protocol or epoch.
//! - Entries contain scalar keys and pinned, immortal shape handles, never a
//!   moving receiver, prototype, JavaScript value or borrowed slab address.
//! - Generated positive hits validate the live receiver/holder ordinary state,
//!   exact key and holder shape, descriptor kind and storage bounds. Loads may
//!   use own/direct-prototype data slots. Stores additionally require an own
//!   writable data slot before their single effect.
//!
//! # See also
//! - `super` owns the only table and every producer/runtime consumer.
//! - `crate::object::shape_body` owns pinned hidden-class layout nodes.
//! - `crate::jit::JitCompileSnapshot` owns immutable compiler inputs.

use std::mem::{offset_of, size_of};

use super::{
    CAPACITY, Entry, HASH_ATOM_MULTIPLIER, HASH_SHAPE_MULTIPLIER, HASH_SHIFT, PropertyLookupCache,
};
use crate::object::AtomOwnPropertyHit;

/// Owned native-layout description of one isolate's shared property table.
///
/// Address fields are process-local relocation inputs, never serialized as
/// portable semantic identities. Every offset is relative to an entry except
/// `shape_id_byte`, which includes the collector header of a shape cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitPropertyLookupCache {
    /// Address of the first entry in the existing fixed boxed table.
    pub table_addr: usize,
    /// Byte stride between table entries.
    pub entry_bytes: u32,
    /// Power-of-two table index mask.
    pub index_mask: u32,
    /// Offset of the receiver's semantic `u64` shape identity.
    pub receiver_shape_id_byte: u32,
    /// Offset of the key's isolate-global `u32` atom identity.
    pub atom_byte: u32,
    /// Offset of `u8` depth: zero means own, one means direct prototype.
    /// Every other value is a generated miss.
    pub hops_byte: u32,
    /// Offset of the holder's pinned compressed `u32` shape handle.
    pub holder_shape_byte: u32,
    /// Offset of the data property's `u16` slot index.
    pub slot_byte: u32,
    /// Offset of the cached one-byte data-kind Boolean.
    pub is_data_byte: u32,
    /// Offset of the cached one-byte writable-descriptor Boolean.
    pub is_writable_byte: u32,
    /// Header-inclusive offset of the shape cell's immutable `u64` identity.
    pub shape_id_byte: u32,
    /// Multiplier of the receiver shape in the shared index calculation.
    pub hash_shape_multiplier: u64,
    /// Multiplier of the property atom in the shared index calculation.
    pub hash_atom_multiplier: u64,
    /// Right shift after XORing the two wrapping products.
    pub hash_shift: u8,
}

impl PropertyLookupCache {
    /// Describe the same entries used by runtime property lookup.
    pub(crate) fn jit_layout(&self) -> JitPropertyLookupCache {
        let hit = offset_of!(Entry, hit);
        JitPropertyLookupCache {
            table_addr: self.ways[0].as_ptr() as usize,
            entry_bytes: size_of::<Entry>() as u32,
            index_mask: (CAPACITY - 1) as u32,
            receiver_shape_id_byte: offset_of!(Entry, receiver_shape) as u32,
            atom_byte: offset_of!(Entry, atom) as u32,
            hops_byte: offset_of!(Entry, hops) as u32,
            holder_shape_byte: (hit + offset_of!(AtomOwnPropertyHit, shape)) as u32,
            slot_byte: (hit + offset_of!(AtomOwnPropertyHit, slot)) as u32,
            is_data_byte: (hit + offset_of!(AtomOwnPropertyHit, is_data)) as u32,
            is_writable_byte: offset_of!(Entry, is_writable) as u32,
            shape_id_byte: (otter_gc::header::HEADER_SIZE + crate::object::SHAPE_BODY_ID_OFFSET)
                as u32,
            hash_shape_multiplier: HASH_SHAPE_MULTIPLIER,
            hash_atom_multiplier: HASH_ATOM_MULTIPLIER,
            hash_shift: HASH_SHIFT,
        }
    }
}

const _: () = assert!(CAPACITY.is_power_of_two());
const _: () = assert!(size_of::<std::cell::Cell<Entry>>() == size_of::<Entry>());
