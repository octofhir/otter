//! Isolate-wide `(receiver shape, property atom)` property cache.
//!
//! One direct-mapped table shares resolved own and inherited data slots across
//! sites. Runtime misses and generated megamorphic accesses use the same
//! entries, reading each slot's live value.
//!
//! # Contents
//! - [`PropertyLookupCache`] stores `(receiver shape, atom)` resolutions.
//! - [`StoreTransitionCache`] stores add-property transitions with the same key.
//! - [`jit`] exposes the owned scalar layouts to generated probes.
//!
//! # Invariants
//! - Receiver shape identity fixes own keys and the first prototype.
//! - Inherited entries retain one validity cell covering the complete chain.
//!   A holder is loaded through its root shape's traced prototype word.
//! - Canonical prototype mutation invalidates cells before effects; generated
//!   stores to watched prototypes enter that mutation boundary.
//! - Table shape references are weak and cleared before shape collection.
//! - Transition entries and their owning replay records share one index and
//!   are replaced together on the mutator. Tables never resize or move.
//! - Every access checks the complete key and ordinary receiver state before
//!   trusting a slot. A miss enters the canonical property operation.
//!
//! # See also
//! - [`crate::cache_ir`]
//! - [`crate::property_ic`]

use crate::object::prototype_validity::PrototypeValidity;
use std::cell::{Cell, RefCell};
use std::sync::Arc;

use crate::cache_ir;
use crate::object::{self, AtomOwnPropertyHit, JsObject, ShapeId};
use crate::property_atom::{AtomId, AtomizedPropertyKey};

pub(crate) mod jit;

/// Entries in the direct-mapped table. Power of two so the index is a mask.
const CAPACITY: usize = 1024;
const HASH_SHAPE_MULTIPLIER: u64 = 0x9E37_79B9_7F4A_7C15;
const HASH_ATOM_MULTIPLIER: u64 = 0xC2B2_AE3D_27D4_EB4F;
const HASH_SHIFT: u8 = 32;

/// One resolved `(receiver shape, atom)` answer.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct Entry {
    validity: usize,
    holder_root: object::ShapeHandle,
    holder_root_id: ShapeId,
    /// Receiver hidden class this answer was resolved under.
    /// [`ShapeId::UNASSIGNED`] marks an empty way.
    receiver_shape: ShapeId,
    /// Property name this answer is for.
    atom: AtomId,
    /// `0` when the receiver owns the slot, `1` when an inherited holder
    /// does, [`Entry::UNRESOLVABLE`] when no data slot describes this pair.
    hops: u8,
    /// Whether the resolved data descriptor was writable.
    is_writable: bool,
    /// Own-slot metadata, protected by receiver shape or the inherited proof.
    hit: AtomOwnPropertyHit,
}

impl Entry {
    /// Recorded when the walk found no cacheable data slot — an accessor or
    /// nothing at all. Purely a hint: acting on it only ever
    /// means "take the full ladder", which is always correct, so a prototype
    /// that later grows the property costs a slow read, never a wrong one.
    const UNRESOLVABLE: u8 = u8::MAX;

    const EMPTY: Self = Self {
        validity: 0,
        holder_root: object::ShapeHandle::null(),
        holder_root_id: ShapeId::UNASSIGNED,
        receiver_shape: ShapeId::UNASSIGNED,
        atom: AtomId::NONE,
        hops: 0,
        is_writable: false,
        hit: AtomOwnPropertyHit::PLACEHOLDER,
    };
}

/// What the shared table knows about one `(receiver shape, atom)` pair.
#[derive(Debug)]
pub(crate) enum PropertyProbe {
    /// The pair resolves to this data slot.
    Resolved(cache_ir::ResolvedDataSlot),
    /// A walk already proved this pair has no cacheable data slot.
    Unresolvable,
    /// Nothing recorded, or the recorded answer's guards no longer hold.
    Unknown,
}

/// Direct-mapped `(shape, atom)` → resolved data slot table.
#[derive(Debug)]
pub(crate) struct PropertyLookupCache {
    ways: Box<[Cell<Entry>]>,
    proofs: Box<[RefCell<Option<Arc<PrototypeValidity>>>]>,
}

impl Default for PropertyLookupCache {
    fn default() -> Self {
        Self {
            ways: (0..CAPACITY).map(|_| Cell::new(Entry::EMPTY)).collect(),
            proofs: (0..CAPACITY).map(|_| RefCell::new(None)).collect(),
        }
    }
}

impl PropertyLookupCache {
    /// Mix the shape id and the atom into a table index. Both are dense
    /// counters, so a plain xor would collide systematically for neighbouring
    /// shapes; multiplying spreads the low bits.
    fn index(receiver_shape: ShapeId, atom: AtomId) -> usize {
        let mixed = receiver_shape.raw().wrapping_mul(HASH_SHAPE_MULTIPLIER)
            ^ u64::from(atom.raw()).wrapping_mul(HASH_ATOM_MULTIPLIER);
        ((mixed >> HASH_SHIFT) as usize) & (CAPACITY - 1)
    }

    /// The entry recorded for this receiver's class and this name, if any.
    #[must_use]
    fn entry_for(
        &self,
        obj: JsObject,
        heap: &otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<Entry> {
        if object::is_dictionary(obj, heap) {
            return None;
        }
        let receiver_shape = object::shape_id(obj, heap);
        let atom = key.atom().id();
        let entry = self.ways[Self::index(receiver_shape, atom)].get();
        (entry.receiver_shape == receiver_shape && entry.atom == atom).then_some(entry)
    }

    /// Everything the table knows about this pair.
    ///
    /// A [`PropertyProbe::Resolved`] answer has the same shape a fresh walk
    /// would return, so a hit serves both the read and any per-site stub the
    /// caller still wants to install.
    #[must_use]
    pub(crate) fn probe(
        &self,
        obj: JsObject,
        heap: &otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
    ) -> PropertyProbe {
        let Some(entry) = self.entry_for(obj, heap, key) else {
            return PropertyProbe::Unknown;
        };
        if !object::supports_fast_property_ic(obj, heap) {
            return PropertyProbe::Unknown;
        }
        let validity = self.proofs[Self::index(entry.receiver_shape, entry.atom)]
            .borrow()
            .clone();
        if entry.hops == Entry::UNRESOLVABLE {
            return if validity.as_ref().is_none_or(|cell| cell.is_valid()) {
                PropertyProbe::Unresolvable
            } else {
                PropertyProbe::Unknown
            };
        }
        let value = if entry.hops == 0 {
            object::load_own_data_slot_atom(obj, heap, key, entry.hit)
        } else if validity.as_ref().is_some_and(|cell| cell.is_valid())
            && !heap.read_payload(obj, |body| body.chain_link_opaque())
        {
            match object::shape_body::prototype_of(entry.holder_root) {
                object::shape_body::ShapePrototype::Object(holder) => {
                    Some(object::load_proven_data_slot(holder, heap, entry.hit.slot))
                }
                _ => None,
            }
        } else {
            None
        };
        match value {
            Some(value) => PropertyProbe::Resolved(cache_ir::ResolvedDataSlot {
                hops: entry.hops,
                hit: entry.hit,
                value,
                is_writable: entry.is_writable,
                validity,
                holder_root: entry.holder_root,
                holder_root_id: entry.holder_root_id,
            }),
            None => PropertyProbe::Unknown,
        }
    }

    /// Forget every entry whose holder hit names a shape the full
    /// collection is about to free. Receiver keys are ids, which are never
    /// reused, but generated megamorphic probes compare the holder's shape
    /// handle, which a later shape could reuse.
    pub(crate) fn forget_shapes(&self, dead: &rustc_hash::FxHashSet<u32>) {
        for (index, way) in self.ways.iter().enumerate() {
            let entry = way.get();
            if dead.contains(&entry.hit.shape.offset())
                || dead.contains(&entry.holder_root.offset())
            {
                way.set(Entry::EMPTY);
                *self.proofs[index].borrow_mut() = None;
            }
        }
    }

    /// Record a resolution so any site asking for this `(shape, atom)` pair
    /// answers without the ladder. Overwrites whatever shared the index —
    /// re-resolving a displaced pair costs one walk, keeping a second way
    /// costs a compare on every probe.
    pub(crate) fn record(
        &self,
        obj: JsObject,
        heap: &otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
        resolved: Option<&cache_ir::ResolvedDataSlot>,
    ) {
        if object::is_dictionary(obj, heap) {
            return;
        }
        let receiver_shape = object::shape_id(obj, heap);
        let atom = key.atom().id();
        let index = Self::index(receiver_shape, atom);
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
        self.ways[index].set(Entry {
            validity: validity.as_ref().map_or(0, |cell| cell.address()),
            holder_root: resolved
                .map_or(object::ShapeHandle::null(), |resolved| resolved.holder_root),
            holder_root_id: resolved
                .map_or(ShapeId::UNASSIGNED, |resolved| resolved.holder_root_id),
            receiver_shape,
            atom,
            hops: resolved.map_or(Entry::UNRESOLVABLE, |resolved| resolved.hops),
            is_writable: resolved.is_some_and(|resolved| resolved.is_writable),
            hit: resolved.map_or(AtomOwnPropertyHit::PLACEHOLDER, |resolved| resolved.hit),
        });
        *self.proofs[index].borrow_mut() = validity;
    }
}

/// Sets in the add-property transition table. Power of two so the index is a
/// mask.
const TRANSITION_SETS: usize = 512;
/// Ways per transition set, most recently recorded first.
const TRANSITION_WAYS: usize = 4;

/// Set-associative `(receiver shape, atom)` → add-property
/// transition table.
///
/// A store site that has gone megamorphic stops installing its own
/// transitions; without this table every property-adding store at such a site
/// would repeat the complete `[[Set]]` walk. Sites share the entries, exactly
/// as V8's megamorphic stub cache shares transition handlers. A hidden class
/// fixes its prototype, so class hierarchies give every subclass its own
/// receiver lineage and one base-constructor store site sees as many keys as
/// there are subclasses; the ways absorb the index collisions a direct-mapped
/// table would thrash on (V8 backs its primary stub cache with a secondary
/// table for the same reason). Each handler retains the shared validity cell
/// proving inherited absence or writable data.
#[derive(Debug)]
pub(crate) struct StoreTransitionCache {
    ways: Box<[Option<object::StorePropertyTransition>]>,
    jit_ways: Box<[Cell<StoreTransitionJitEntry>]>,
}

/// Compact scalar entry read by generated code. Zero `target_shape` means that the
/// runtime record has no allocation-free generated handler.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct StoreTransitionJitEntry {
    receiver_shape: u64,
    validity: usize,
    atom: u32,
    target_shape: u32,
    slot: u16,
}

impl StoreTransitionJitEntry {
    const EMPTY: Self = Self {
        receiver_shape: 0,
        validity: 0,
        atom: 0,
        target_shape: 0,
        slot: 0,
    };

    fn from_transition(transition: &object::StorePropertyTransition) -> Self {
        let validity = match &transition.kind {
            object::StorePropertyTransitionKind::OwnAdd => 0,
            object::StorePropertyTransitionKind::PrototypeChainMissing { validity }
            | object::StorePropertyTransitionKind::DirectPrototypeWritableData { validity } => {
                validity.address()
            }
        };
        Self {
            receiver_shape: transition.from_shape_id.raw(),
            validity,
            atom: transition.atom_id.raw(),
            target_shape: transition.to_shape.get().offset(),
            slot: transition.slot,
        }
    }
}

impl Default for StoreTransitionCache {
    fn default() -> Self {
        let entries = TRANSITION_SETS * TRANSITION_WAYS;
        Self {
            ways: (0..entries).map(|_| None).collect(),
            jit_ways: (0..entries)
                .map(|_| Cell::new(StoreTransitionJitEntry::EMPTY))
                .collect(),
        }
    }
}

impl StoreTransitionCache {
    /// First entry of the set a key hashes to.
    fn set_start(receiver_shape: ShapeId, atom: AtomId) -> usize {
        let mixed = receiver_shape.raw().wrapping_mul(HASH_SHAPE_MULTIPLIER)
            ^ u64::from(atom.raw()).wrapping_mul(HASH_ATOM_MULTIPLIER);
        (((mixed >> HASH_SHIFT) as usize) & (TRANSITION_SETS - 1)) * TRANSITION_WAYS
    }

    /// The entry recording this key, if its set holds one.
    fn find(
        &self,
        receiver_shape: ShapeId,
        atom: AtomId,
    ) -> Option<&object::StorePropertyTransition> {
        let start = Self::set_start(receiver_shape, atom);
        self.ways[start..start + TRANSITION_WAYS]
            .iter()
            .flatten()
            .find(|transition| {
                transition.from_shape_id == receiver_shape && transition.atom_id == atom
            })
    }

    /// Replay the recorded transition for this receiver class, prototype
    /// class and name.
    ///
    /// `Ok(Some(()))` committed the store; `Ok(None)` is an allocation-free
    /// miss (nothing recorded, or a replay guard failed).
    pub(crate) fn replay(
        &self,
        obj: JsObject,
        heap: &mut otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
        value: &crate::Value,
    ) -> Result<Option<()>, otter_gc::OutOfMemory> {
        if object::is_dictionary(obj, heap) {
            return Ok(None);
        }
        let receiver_shape = object::shape_id(obj, heap);
        let atom = key.atom().id();
        match self.find(receiver_shape, atom) {
            Some(transition) => {
                object::replay_store_property_transition(obj, heap, key, transition, value)
            }
            None => Ok(None),
        }
    }

    /// Record one transition captured on `obj` before the store as its set's
    /// most recent way: an entry for the same key is replaced, and a full set
    /// drops its least recently recorded way.
    pub(crate) fn record(&mut self, transition: object::StorePropertyTransition) {
        let start = Self::set_start(transition.from_shape_id, transition.atom_id);
        let same_key = |entry: &Option<object::StorePropertyTransition>| {
            entry.as_ref().is_some_and(|recorded| {
                recorded.from_shape_id == transition.from_shape_id
                    && recorded.atom_id == transition.atom_id
            })
        };
        // The way to vacate: the key's own entry, else the first empty one,
        // else the least recently recorded.
        let set = &self.ways[start..start + TRANSITION_WAYS];
        let vacate = set
            .iter()
            .position(same_key)
            .or_else(|| set.iter().position(Option::is_none))
            .unwrap_or(TRANSITION_WAYS - 1);
        for way in (start + 1..=start + vacate).rev() {
            self.ways[way] = self.ways[way - 1].take();
            self.jit_ways[way].set(self.jit_ways[way - 1].get());
        }
        let jit = StoreTransitionJitEntry::from_transition(&transition);
        self.ways[start] = Some(transition);
        self.jit_ways[start].set(jit);
    }

    /// Visit the target shapes the recorded transitions keep alive.
    pub(crate) fn trace_roots(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        for transition in self.ways.iter().flatten() {
            transition.trace_roots(visitor);
        }
    }
}

impl crate::Interpreter {
    /// Resolve a named data property, answering from the shared
    /// `(shape, atom)` table when it already knows this pair and recording the
    /// answer when it does not.
    ///
    /// Every property opcode's miss path funnels through here, so the walk
    /// happens once per `(receiver class, name)` in the isolate rather than
    /// once per site that has to re-learn it.
    #[must_use]
    pub(crate) fn resolve_property_data_slot(
        &self,
        obj: JsObject,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<cache_ir::ResolvedDataSlot> {
        // Every spelling nothing interned shares the one `NONE` atom, so it
        // can never key a shared cache entry.
        if key.atom().id() == AtomId::NONE {
            return cache_ir::resolve_atom_data_slot(obj, &self.gc_heap, key);
        }
        match self.property_cache.probe(obj, &self.gc_heap, key) {
            PropertyProbe::Resolved(resolved) => return Some(resolved),
            PropertyProbe::Unresolvable => return None,
            PropertyProbe::Unknown => {}
        }
        let resolved = cache_ir::resolve_atom_data_slot(obj, &self.gc_heap, key);
        if let Some(resolved) = &resolved {
            self.shape_runtime
                .register_root(&self.gc_heap, object::shape(obj, &self.gc_heap));
            if !resolved.holder_root.is_null() {
                self.shape_runtime
                    .register_root(&self.gc_heap, resolved.holder_root);
            }
        }
        self.property_cache
            .record(obj, &self.gc_heap, key, resolved.as_ref());
        resolved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_pairs_do_not_share_one_index() {
        let a = ShapeId::for_test(7);
        let b = ShapeId::for_test(8);
        let x = AtomId::from_global(3);
        let y = AtomId::from_global(4);
        let indexes = [
            PropertyLookupCache::index(a, x),
            PropertyLookupCache::index(a, y),
            PropertyLookupCache::index(b, x),
            PropertyLookupCache::index(b, y),
        ];
        let mut sorted = indexes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 4, "neighbouring pairs collide: {indexes:?}");
    }

    #[test]
    fn an_empty_table_answers_nothing() {
        let mut interp = crate::Interpreter::new();
        let obj = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).expect("object");
        let names = crate::property_atom::NameInterner::default();
        let atom = crate::property_atom::PropertyAtom::new(names.intern("x"));
        let key = AtomizedPropertyKey::new(atom, "x");
        let cache = PropertyLookupCache::default();
        assert!(matches!(
            cache.probe(obj, interp.gc_heap(), key),
            PropertyProbe::Unknown
        ));
    }
}
