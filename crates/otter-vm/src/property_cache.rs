//! Isolate-wide `(receiver shape, property atom)` property cache.
//!
//! A per-site inline cache answers a property read only while that site stays
//! monomorphic or narrowly polymorphic. Past that the site goes megamorphic and
//! every execution re-enters the full `[[Get]]` ladder — even though the same
//! handful of `(shape, name)` pairs keep coming back, and even though a
//! *different* site already resolved that exact pair. A dispatch loop over
//! sibling classes (DeltaBlue's constraint list, Richards' task queue) spends
//! its time there.
//!
//! This is the shared answer, direct-mapped on `(ShapeId, AtomId)` the way
//! SpiderMonkey's megamorphic cache is: one probe replaces the ladder, whatever
//! site asks. It caches only what the cache stubs already prove sound — a plain
//! data slot on the receiver or on its direct prototype — so a hit runs the
//! same shape/atom/attribute validation an installed stub runs.
//!
//! # Contents
//! - [`PropertyLookupCache`] — the direct-mapped table.
//!
//! # Invariants
//! - A shape's own-key set never changes, so an entry keyed by receiver shape
//!   never needs invalidating: it either still matches the receiver's shape or
//!   it does not.
//! - A one-hop entry re-reads the live prototype and revalidates the holder's
//!   shape on every hit, exactly like [`crate::cache_ir::CacheStub`]'s
//!   direct-prototype program. Receiver-shape identity proves the receiver has
//!   no own slot shadowing it.
//! - Entries are recorded only for shaped, fast-IC-compatible receivers.
//!   Dictionary-mode identities are never inserted.
//! - The table is derived data: dropping any entry is always sound.
//!
//! # See also
//! - [`crate::cache_ir`]
//! - [`crate::property_ic`]

use std::cell::Cell;

use crate::cache_ir;
use crate::object::{self, AtomOwnPropertyHit, JsObject, ShapeId};
use crate::property_atom::{AtomId, AtomizedPropertyKey};

/// Entries in the direct-mapped table. Power of two so the index is a mask.
const CAPACITY: usize = 1024;

/// One resolved `(receiver shape, atom)` answer.
#[derive(Debug, Clone, Copy)]
struct Entry {
    /// Receiver hidden class this answer was resolved under.
    /// [`ShapeId::UNASSIGNED`] marks an empty way.
    receiver_shape: ShapeId,
    /// Property name this answer is for.
    atom: AtomId,
    /// `0` when the receiver owns the slot, `1` when its direct prototype
    /// does, [`Entry::UNRESOLVABLE`] when no data slot describes this pair.
    hops: u8,
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
        let mixed = receiver_shape.raw().wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ u64::from(atom.raw()).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
        ((mixed >> 32) as usize) & (CAPACITY - 1)
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
