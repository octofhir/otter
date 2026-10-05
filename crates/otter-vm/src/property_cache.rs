//! Isolate-wide shared property actions under one `(shape, atom)` key.
//!
//! # Contents
//! - [`PropertyActionCache`] owns one fixed set-associative scalar table.
//! - Independent load and store facts share each key and retained proof owners.
//! - [`jit`] describes the same entries to all generated property probes.
//!
//! # Invariants
//! Exact immutable shape identity fixes own descriptors and the first prototype.
//! A key can simultaneously load an inherited slot and append an own slot;
//! those actions have separate slots and nonreviving validity proofs. Generated
//! probes do not allocate or reenter. Target shapes are traced in their actual
//! entry words; holder references are weak and cleared by their own mark state
//! before reuse. A runtime allocating replay releases all proof borrows and
//! rereads the canonical child word after GC. Every miss precedes effects and
//! enters the source operation once.
//!
//! # See also
//! - `crate::object::shape_transition` owns the one add-property replay kernel.
//! - `crate::cache_ir` owns bounded per-site programs over the same semantics.

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use crate::cache_ir;
use crate::object::prototype_validity::PrototypeValidity;
use crate::object::{self, AtomOwnPropertyHit, JsObject, ShapeId};
use crate::property_atom::{AtomId, AtomizedPropertyKey};

pub(crate) mod jit;

const SETS: usize = 512;
const WAYS: usize = 4;
const HASH_SHAPE_MULTIPLIER: u64 = 0x9E37_79B9_7F4A_7C15;
const HASH_ATOM_MULTIPLIER: u64 = 0xC2B2_AE3D_27D4_EB4F;
const HASH_SHIFT: u8 = 32;

/// Selected live shared-table load action. Unknown and Unresolvable are misses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PropertyLoadAction {
    /// No load resolution is recorded under this key.
    Unknown = 0,
    /// A full walk found no cacheable data slot; retain the semantic ladder.
    Unresolvable = 1,
    /// Load a current own data slot.
    OwnData = 2,
    /// Load a current holder data slot under a complete chain proof.
    InheritedData = 3,
}

/// Selected live shared-table store action, independently authorized from loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PropertyStoreAction {
    /// No store authorization is recorded under this key.
    Unknown = 0,
    /// Overwrite a current own writable data slot.
    OwnWritable = 1,
    /// Append an own slot and publish the recorded child shape.
    AddOwn = 2,
}

/// The one physical scalar row read by generated and runtime probes.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct PropertyActionEntry {
    receiver_shape_id: ShapeId,
    atom: AtomId,
    load_action: PropertyLoadAction,
    store_action: PropertyStoreAction,
    load_slot: u16,
    store_slot: u16,
    load_writable: bool,
    reserved: u8,
    holder_shape: object::ShapeHandle,
    holder_root: object::ShapeHandle,
    target_shape: object::ShapeHandle,
    holder_shape_id: ShapeId,
    target_shape_id: ShapeId,
    load_validity: usize,
    store_validity: usize,
}

impl PropertyActionEntry {
    const EMPTY: Self = Self {
        receiver_shape_id: ShapeId::UNASSIGNED,
        atom: AtomId::NONE,
        load_action: PropertyLoadAction::Unknown,
        store_action: PropertyStoreAction::Unknown,
        load_slot: 0,
        store_slot: 0,
        load_writable: false,
        reserved: 0,
        holder_shape: object::ShapeHandle::null(),
        holder_root: object::ShapeHandle::null(),
        target_shape: object::ShapeHandle::null(),
        holder_shape_id: ShapeId::UNASSIGNED,
        target_shape_id: ShapeId::UNASSIGNED,
        load_validity: 0,
        store_validity: 0,
    };

    fn hit(self) -> AtomOwnPropertyHit {
        AtomOwnPropertyHit {
            shape_id: self.holder_shape_id,
            shape: self.holder_shape,
            atom_id: self.atom,
            slot: self.load_slot,
            is_data: true,
        }
    }

    fn clear_load(&mut self) {
        self.load_action = PropertyLoadAction::Unknown;
        self.load_slot = 0;
        self.load_writable = false;
        self.holder_shape = object::ShapeHandle::null();
        self.holder_root = object::ShapeHandle::null();
        self.holder_shape_id = ShapeId::UNASSIGNED;
        self.load_validity = 0;
    }
}

/// Retention metadata only; the key, slots and child live solely in the scalar row.
#[derive(Debug, Default)]
struct ActionOwners {
    load: Option<Arc<PrototypeValidity>>,
    store: Option<object::StorePropertyTransitionKind>,
}

/// The selected runtime load fact, including an existing semantic resolution.
#[derive(Debug)]
pub(crate) enum PropertyProbe {
    /// A live data slot authorized under the key and retained chain proof.
    Resolved(cache_ir::ResolvedDataSlot),
    /// A guarded walk found no cacheable data slot.
    Unresolvable,
    /// No matching valid fact exists.
    Unknown,
}

/// One fixed key table for independent property read and write actions.
#[derive(Debug)]
pub(crate) struct PropertyActionCache {
    entries: Box<[Cell<PropertyActionEntry>]>,
    owners: Box<[RefCell<ActionOwners>]>,
}

impl Default for PropertyActionCache {
    fn default() -> Self {
        Self {
            entries: (0..SETS * WAYS)
                .map(|_| Cell::new(PropertyActionEntry::EMPTY))
                .collect(),
            owners: (0..SETS * WAYS)
                .map(|_| RefCell::new(ActionOwners::default()))
                .collect(),
        }
    }
}

impl PropertyActionCache {
    fn set_start(shape: ShapeId, atom: AtomId) -> usize {
        let mixed = shape.raw().wrapping_mul(HASH_SHAPE_MULTIPLIER)
            ^ u64::from(atom.raw()).wrapping_mul(HASH_ATOM_MULTIPLIER);
        (((mixed >> HASH_SHIFT) as usize) & (SETS - 1)) * WAYS
    }

    fn find(&self, shape: ShapeId, atom: AtomId) -> Option<usize> {
        let start = Self::set_start(shape, atom);
        (start..start + WAYS).find(|&index| {
            let entry = self.entries[index].get();
            entry.receiver_shape_id == shape && entry.atom == atom
        })
    }

    /// Reuse an exact key, or move one complete row plus its proof owners.
    /// Publication cannot interleave with generated probes or collection.
    fn record_way(&self, shape: ShapeId, atom: AtomId) -> usize {
        if let Some(index) = self.find(shape, atom) {
            return index;
        }
        let start = Self::set_start(shape, atom);
        let vacate = (start..start + WAYS)
            .find(|&index| self.entries[index].get().receiver_shape_id == ShapeId::UNASSIGNED)
            .unwrap_or(start + WAYS - 1);
        self.entries[vacate].set(PropertyActionEntry::EMPTY);
        self.owners[vacate].replace(ActionOwners::default());
        for index in (start + 1..=vacate).rev() {
            let owners = self.owners[index - 1].replace(ActionOwners::default());
            self.owners[index].replace(owners);
            self.entries[index].set(self.entries[index - 1].get());
        }
        self.entries[start].set(PropertyActionEntry {
            receiver_shape_id: shape,
            atom,
            ..PropertyActionEntry::EMPTY
        });
        start
    }

    fn entry_for(
        &self,
        obj: JsObject,
        heap: &otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<(usize, PropertyActionEntry)> {
        if key.atom().id() == AtomId::NONE || !object::supports_fast_property_ic(obj, heap) {
            return None;
        }
        let index = self.find(object::shape_id(obj, heap), key.atom().id())?;
        Some((index, self.entries[index].get()))
    }

    pub(crate) fn probe_load(
        &self,
        obj: JsObject,
        heap: &otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
    ) -> PropertyProbe {
        let Some((index, entry)) = self.entry_for(obj, heap, key) else {
            return PropertyProbe::Unknown;
        };
        let validity = self.owners[index].borrow().load.clone();
        if entry.load_action == PropertyLoadAction::Unresolvable {
            return if validity.as_ref().is_none_or(|cell| cell.is_valid()) {
                PropertyProbe::Unresolvable
            } else {
                PropertyProbe::Unknown
            };
        }
        let hit = entry.hit();
        let (hops, value) = match entry.load_action {
            PropertyLoadAction::OwnData => {
                (0, object::load_own_data_slot_atom(obj, heap, key, hit))
            }
            PropertyLoadAction::InheritedData
                if validity.as_ref().is_some_and(|cell| cell.is_valid())
                    && !heap.read_payload(obj, |body| body.chain_link_opaque()) =>
            {
                let value = match object::shape_body::prototype_of(entry.holder_root) {
                    object::shape_body::ShapePrototype::Object(holder)
                        if object::shape(holder, heap) == entry.holder_shape
                            && object::shape_id(holder, heap) == entry.holder_shape_id =>
                    {
                        Some(object::load_proven_data_slot(holder, heap, entry.load_slot))
                    }
                    _ => None,
                };
                (1, value)
            }
            _ => return PropertyProbe::Unknown,
        };
        let Some(value) = value else {
            return PropertyProbe::Unknown;
        };
        let holder_root_id = if entry.holder_root.is_null() {
            ShapeId::UNASSIGNED
        } else {
            heap.read_payload(entry.holder_root, object::shape_body::ShapeBody::id)
        };
        PropertyProbe::Resolved(cache_ir::ResolvedDataSlot {
            hops,
            hit,
            value,
            is_writable: entry.load_writable,
            validity,
            holder_root: entry.holder_root,
            holder_root_id,
        })
    }

    /// Publish one load fact without discarding an independent append action.
    pub(crate) fn record_load(
        &self,
        obj: JsObject,
        heap: &otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
        resolved: Option<&cache_ir::ResolvedDataSlot>,
    ) {
        if key.atom().id() == AtomId::NONE || !object::supports_fast_property_ic(obj, heap) {
            return;
        }
        let validity = if let Some(resolved) = resolved {
            resolved.validity.clone()
        } else if let Some(first) = object::prototype(obj, heap) {
            let Some(cell) = object::prototype_validity::chain_validity(first, heap) else {
                return;
            };
            Some(cell)
        } else if object::prototype_value(obj, heap).is_none() {
            None
        } else {
            return;
        };
        let index = self.record_way(object::shape_id(obj, heap), key.atom().id());
        let mut entry = self.entries[index].get();
        entry.clear_load();
        entry.load_validity = validity.as_ref().map_or(0, |cell| cell.address());
        if let Some(resolved) = resolved {
            entry.load_action = if resolved.hops == 0 {
                PropertyLoadAction::OwnData
            } else {
                PropertyLoadAction::InheritedData
            };
            entry.load_slot = resolved.hit.slot;
            entry.load_writable = resolved.is_writable;
            entry.holder_shape = resolved.hit.shape;
            entry.holder_shape_id = resolved.hit.shape_id;
            entry.holder_root = resolved.holder_root;
            if resolved.hops == 0 && resolved.is_writable {
                entry.store_action = PropertyStoreAction::OwnWritable;
                entry.store_slot = resolved.hit.slot;
                entry.target_shape = object::ShapeHandle::null();
                entry.target_shape_id = ShapeId::UNASSIGNED;
                entry.store_validity = 0;
                self.owners[index].borrow_mut().store = None;
            }
        } else {
            entry.load_action = PropertyLoadAction::Unresolvable;
        }
        self.owners[index].borrow_mut().load = validity;
        self.entries[index].set(entry);
    }

    /// Publish the existing own-data hit of a committed writable store program.
    /// The program's constructor already proved the data descriptor writable;
    /// immutable shape identity makes its load and overwrite facts compatible.
    pub(crate) fn record_own_store(&self, hit: AtomOwnPropertyHit) {
        if hit.atom_id == AtomId::NONE {
            return;
        }
        let index = self.record_way(hit.shape_id, hit.atom_id);
        self.owners[index].replace(ActionOwners::default());
        self.entries[index].set(PropertyActionEntry {
            receiver_shape_id: hit.shape_id,
            atom: hit.atom_id,
            load_action: PropertyLoadAction::OwnData,
            store_action: PropertyStoreAction::OwnWritable,
            load_slot: hit.slot,
            store_slot: hit.slot,
            load_writable: true,
            holder_shape: hit.shape,
            holder_shape_id: hit.shape_id,
            ..PropertyActionEntry::EMPTY
        });
    }

    /// Merge a committed add sample into its original key, retaining any load.
    pub(crate) fn record_store(&self, transition: object::StorePropertyTransition) {
        if transition.atom_id == AtomId::NONE {
            return;
        }
        let index = self.record_way(transition.from_shape_id, transition.atom_id);
        let mut entry = self.entries[index].get();
        entry.store_action = PropertyStoreAction::AddOwn;
        entry.store_slot = transition.slot;
        entry.target_shape = transition.to_shape.get();
        entry.target_shape_id = transition.to_shape_id;
        entry.store_validity = match &transition.kind {
            object::StorePropertyTransitionKind::OwnAdd => 0,
            object::StorePropertyTransitionKind::PrototypeChainMissing { validity }
            | object::StorePropertyTransitionKind::DirectPrototypeWritableData { validity } => {
                validity.address()
            }
        };
        self.owners[index].borrow_mut().store = Some(transition.kind);
        self.entries[index].set(entry);
    }

    /// Replay only the selected store action, preserving a matched refusal.
    pub(crate) fn replay_store(
        &self,
        obj: JsObject,
        heap: &mut otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
        value: &crate::Value,
    ) -> Result<Option<PropertyStoreAction>, otter_gc::OutOfMemory> {
        if object::state(obj, heap).is_prototype() {
            return Ok(None);
        }
        let Some((index, entry)) = self.entry_for(obj, heap, key) else {
            return Ok(None);
        };
        match entry.store_action {
            PropertyStoreAction::OwnWritable => Ok(object::store_own_data_slot_atom(
                obj,
                heap,
                key,
                AtomOwnPropertyHit {
                    shape_id: entry.receiver_shape_id,
                    shape: object::shape(obj, heap),
                    atom_id: entry.atom,
                    slot: entry.store_slot,
                    is_data: true,
                },
                value,
            )
            .map(|()| PropertyStoreAction::OwnWritable)),
            PropertyStoreAction::AddOwn => {
                // No proof borrow crosses the allocating replay/weak sweep.
                let Some(kind) = self.owners[index].borrow().store.clone() else {
                    return Ok(None);
                };
                object::replay_store_property_transition(
                    obj,
                    heap,
                    key,
                    entry.receiver_shape_id,
                    entry.atom,
                    entry.target_shape_id,
                    || self.entries[index].get().target_shape,
                    &kind,
                    entry.store_slot,
                    value,
                )
                .map(|result| result.map(|()| PropertyStoreAction::AddOwn))
            }
            PropertyStoreAction::Unknown => Ok(None),
        }
    }

    /// The full collection's weak pass: clear every load fact whose holder
    /// shape or holder root the marking left unreached, before the sweep frees
    /// those cells, without retiring a separately retained store.
    ///
    /// Liveness is read per reference from the mark bits, not from the shape
    /// id registry: heap-only producers (prototype-state and other state
    /// variants) create holder shapes that no id table names, and their death
    /// must still clear the weak word before its cell can be reused.
    pub(crate) fn sweep_dead_holders(&self, heap: &otter_gc::GcHeap) {
        let dead = |shape: object::ShapeHandle| !shape.is_null() && !heap.is_marked(shape.raw());
        for (index, way) in self.entries.iter().enumerate() {
            let mut entry = way.get();
            if dead(entry.holder_shape) || dead(entry.holder_root) {
                entry.clear_load();
                if entry.store_action == PropertyStoreAction::Unknown {
                    entry = PropertyActionEntry::EMPTY;
                }
                way.set(entry);
                self.owners[index].borrow_mut().load = None;
            }
        }
    }

    /// Trace each append child at its actual scalar word, with no mirrored row.
    pub(crate) fn trace_roots(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        for way in &self.entries {
            if !way.get().target_shape.is_null() {
                // SAFETY: the initialized Cell owns this interior word for the
                // entire fixed table lifetime. ShapeHandle and RawGc have the
                // same compressed-word representation. No borrow survives GC.
                let slot = unsafe { std::ptr::addr_of_mut!((*way.as_ptr()).target_shape) }
                    .cast::<otter_gc::raw::RawGc>();
                visitor(slot);
            }
        }
    }
}

impl crate::Interpreter {
    /// Resolve a live named data slot using the one key/action cache.
    #[must_use]
    pub(crate) fn resolve_property_data_slot(
        &self,
        obj: JsObject,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<cache_ir::ResolvedDataSlot> {
        if key.atom().id() == AtomId::NONE {
            return cache_ir::resolve_atom_data_slot(obj, &self.gc_heap, key);
        }
        match self.property_cache.probe_load(obj, &self.gc_heap, key) {
            PropertyProbe::Resolved(resolved) => return Some(resolved),
            PropertyProbe::Unresolvable => return None,
            PropertyProbe::Unknown => {}
        }
        let resolved = cache_ir::resolve_atom_data_slot(obj, &self.gc_heap, key);
        if let Some(resolved) = &resolved {
            self.shape_runtime
                .register_shape(&self.gc_heap, object::shape(obj, &self.gc_heap));
            if !resolved.holder_root.is_null() {
                self.shape_runtime
                    .register_shape(&self.gc_heap, resolved.holder_root);
            }
        }
        self.property_cache
            .record_load(obj, &self.gc_heap, key, resolved.as_ref());
        resolved
    }
}

#[cfg(test)]
mod tests;
