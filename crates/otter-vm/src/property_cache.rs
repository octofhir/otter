//! Isolate-wide `(receiver shape, property atom)` property cache.
//!
//! One direct-mapped table shares resolved own/direct-prototype data slots
//! across property sites. Runtime loads consult it when a per-site program
//! misses; generated megamorphic accesses probe the same entries before
//! entering their committed cold sibling. Every hit validates live state and
//! reads or writes the current slot rather than caching a JavaScript value.
//!
//! # Contents
//! - [`PropertyLookupCache`] — the direct-mapped table.
//! - [`StoreTransitionCache`] — the add-a-property counterpart: shared
//!   `(receiver shape, prototype shape, atom)` hidden-class transitions for
//!   megamorphic stores.
//! - [`jit`] — owned scalar table layouts for generated probes.
//!
//! # Invariants
//! - A shape's own-key set never changes, so an entry keyed by receiver shape
//!   never needs invalidating: it either still matches the receiver's shape or
//!   it does not.
//! - A one-hop entry re-reads the live prototype and revalidates the holder's
//!   shape on every hit, exactly like [`crate::cache_ir::CacheStub`]'s
//!   direct-prototype program. Receiver-shape identity proves the receiver has
//!   no own slot shadowing it.
//! - Positive resolutions require fast-IC-compatible receivers. Entries are
//!   keyed only by nonnull receiver shapes, never dictionary-mode identities.
//! - A positive entry's atom and data-slot metadata come from one resolution
//!   of that exact key. The receiver shape proves own-key presence/absence;
//!   generated hits also require a nonnull cached holder shape fixing the
//!   slot's key and descriptor kind, with no live descriptor overrides.
//! - The table is derived data: dropping any entry is always sound.
//! - A shared transition carries the complete replay guard of a per-site
//!   add-transition stub (receiver shape, extensibility, and every recorded
//!   prototype shape), so a hit commits exactly the store `[[Set]]` would.
//! - Boxed tables never resize or move during their interpreter's lifetime.
//!   The compact JIT transition entry and its rooted replay record use the
//!   same index and are replaced together by the owning VM thread.
//!
//! # See also
//! - [`crate::cache_ir`]
//! - [`crate::property_ic`]

use std::cell::Cell;

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
    /// Receiver hidden class this answer was resolved under.
    /// [`ShapeId::UNASSIGNED`] marks an empty way.
    receiver_shape: ShapeId,
    /// Property name this answer is for.
    atom: AtomId,
    /// `0` when the receiver owns the slot, `1` when its direct prototype
    /// does, [`Entry::UNRESOLVABLE`] when no data slot describes this pair.
    hops: u8,
    /// Whether the resolved data descriptor was writable.
    is_writable: bool,
    /// The holder's own-slot hit, revalidated on every read.
    hit: AtomOwnPropertyHit,
}

impl Entry {
    /// Recorded when the walk found no cacheable data slot — an accessor, a
    /// deeper holder, or nothing at all. Purely a hint: acting on it only ever
    /// means "take the full ladder", which is always correct, so a prototype
    /// that later grows the property costs a slow read, never a wrong one.
    const UNRESOLVABLE: u8 = u8::MAX;

    const EMPTY: Self = Self {
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
}

impl Default for PropertyLookupCache {
    fn default() -> Self {
        Self {
            ways: (0..CAPACITY).map(|_| Cell::new(Entry::EMPTY)).collect(),
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
        if object::shape(obj, heap).is_null() {
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
        let holder = match entry.hops {
            0 => obj,
            // A negative answer depends on the prototype as well as the
            // receiver's class, and only the receiver's class is in the key.
            // Pinning the prototype's own class makes the answer expire the
            // moment that prototype grows the property.
            Entry::UNRESOLVABLE => {
                return if entry.hit.shape_id == proto_shape_id(obj, heap) {
                    PropertyProbe::Unresolvable
                } else {
                    PropertyProbe::Unknown
                };
            }
            _ => match object::prototype(obj, heap) {
                Some(proto) if object::supports_fast_property_ic(proto, heap) => proto,
                _ => return PropertyProbe::Unknown,
            },
        };
        match object::load_own_data_slot_atom(holder, heap, key, entry.hit) {
            Some(value) => PropertyProbe::Resolved(cache_ir::ResolvedDataSlot {
                hops: entry.hops,
                hit: entry.hit,
                value,
                is_writable: entry.is_writable,
            }),
            None => PropertyProbe::Unknown,
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
        if object::shape(obj, heap).is_null() {
            return;
        }
        let receiver_shape = object::shape_id(obj, heap);
        let atom = key.atom().id();
        self.ways[Self::index(receiver_shape, atom)].set(Entry {
            receiver_shape,
            atom,
            hops: resolved.map_or(Entry::UNRESOLVABLE, |resolved| resolved.hops),
            is_writable: resolved.is_some_and(|resolved| resolved.is_writable),
            hit: resolved.map_or(
                AtomOwnPropertyHit {
                    shape_id: proto_shape_id(obj, heap),
                    ..AtomOwnPropertyHit::PLACEHOLDER
                },
                |resolved| resolved.hit,
            ),
        });
    }
}

/// Direct-mapped `(receiver shape, prototype shape, atom)` → add-property
/// transition table.
///
/// A store site that has gone megamorphic stops installing its own
/// transitions; without this table every property-adding store at such a site
/// would repeat the complete `[[Set]]` walk. Sites share the entries, exactly
/// as V8's megamorphic stub cache shares transition handlers. A hidden class
/// here does not fix the prototype, unlike a V8 map, so the direct
/// prototype's class joins the key: sibling subclasses sharing one receiver
/// layout would otherwise evict each other's prototype-chain guards.
#[derive(Debug)]
pub(crate) struct StoreTransitionCache {
    ways: Box<[Option<(ShapeId, object::StorePropertyTransition)>]>,
    jit_ways: Box<[Cell<StoreTransitionJitEntry>]>,
}

/// Compact scalar entry read by generated code. Zero `target_shape` means that the
/// runtime record has no allocation-free generated handler.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct StoreTransitionJitEntry {
    receiver_shape: u64,
    prototype_shape: u64,
    atom: u32,
    target_shape: u32,
    slot: u16,
    chain_len: u8,
    _padding: u8,
    chain: [u64; 8],
}

impl StoreTransitionJitEntry {
    const EMPTY: Self = Self {
        receiver_shape: 0,
        prototype_shape: 0,
        atom: 0,
        target_shape: 0,
        slot: 0,
        chain_len: 0,
        _padding: 0,
        chain: [0; 8],
    };

    fn from_transition(
        prototype_shape: ShapeId,
        transition: &object::StorePropertyTransition,
    ) -> Self {
        let mut entry = Self {
            receiver_shape: transition.from_shape_id.raw(),
            prototype_shape: prototype_shape.raw(),
            atom: transition.atom_id.raw(),
            target_shape: transition.to_shape.get().offset(),
            slot: transition.slot,
            ..Self::EMPTY
        };
        match &transition.kind {
            object::StorePropertyTransitionKind::OwnAdd => {}
            object::StorePropertyTransitionKind::PrototypeChainMissing { chain } => {
                entry.chain_len = chain.len() as u8;
                for (dst, shape) in entry.chain.iter_mut().zip(chain) {
                    *dst = shape.raw();
                }
            }
            object::StorePropertyTransitionKind::DirectPrototypeWritableData { .. } => {
                entry.target_shape = 0;
            }
        }
        entry
    }
}

impl Default for StoreTransitionCache {
    fn default() -> Self {
        Self {
            ways: (0..CAPACITY).map(|_| None).collect(),
            jit_ways: (0..CAPACITY)
                .map(|_| Cell::new(StoreTransitionJitEntry::EMPTY))
                .collect(),
        }
    }
}

impl StoreTransitionCache {
    fn index(receiver_shape: ShapeId, prototype_shape: ShapeId, atom: AtomId) -> usize {
        let mixed = receiver_shape.raw().wrapping_mul(HASH_SHAPE_MULTIPLIER)
            ^ prototype_shape
                .raw()
                .wrapping_mul(HASH_ATOM_MULTIPLIER)
                .rotate_left(17)
            ^ u64::from(atom.raw()).wrapping_mul(HASH_ATOM_MULTIPLIER);
        ((mixed >> HASH_SHIFT) as usize) & (CAPACITY - 1)
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
        if object::shape(obj, heap).is_null() {
            return Ok(None);
        }
        let receiver_shape = object::shape_id(obj, heap);
        let prototype_shape = proto_shape_id(obj, heap);
        let atom = key.atom().id();
        match &self.ways[Self::index(receiver_shape, prototype_shape, atom)] {
            Some((recorded_prototype, transition))
                if transition.from_shape_id == receiver_shape
                    && transition.atom_id == atom
                    && *recorded_prototype == prototype_shape =>
            {
                object::replay_store_property_transition(obj, heap, key, transition, value)
            }
            _ => Ok(None),
        }
    }

    /// Record one transition captured on `obj` before the store, displacing
    /// whatever shared its index.
    pub(crate) fn record(
        &mut self,
        prototype_shape: ShapeId,
        transition: object::StorePropertyTransition,
    ) {
        let index = Self::index(
            transition.from_shape_id,
            prototype_shape,
            transition.atom_id,
        );
        let jit = StoreTransitionJitEntry::from_transition(prototype_shape, &transition);
        self.ways[index] = Some((prototype_shape, transition));
        self.jit_ways[index].set(jit);
    }

    /// Visit the target shapes the recorded transitions keep alive.
    pub(crate) fn trace_roots(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        for (_, transition) in self.ways.iter().flatten() {
            transition.trace_roots(visitor);
        }
    }
}

/// The class of `obj`'s prototype, or [`ShapeId::UNASSIGNED`] when it has none.
#[must_use]
pub(crate) fn prototype_shape_id(obj: JsObject, heap: &otter_gc::GcHeap) -> ShapeId {
    proto_shape_id(obj, heap)
}

/// The class of `obj`'s prototype, or [`ShapeId::UNASSIGNED`] when it has none.
#[must_use]
fn proto_shape_id(obj: JsObject, heap: &otter_gc::GcHeap) -> ShapeId {
    object::prototype(obj, heap).map_or(ShapeId::UNASSIGNED, |proto| object::shape_id(proto, heap))
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
