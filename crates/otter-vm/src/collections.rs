//! `Map`, `Set`, `WeakMap`, `WeakSet` collection value types.
//!
//! `Map` and `Set` preserve insertion order (ECMA-262 §24.1 /
//! §24.2). `WeakMap` and `WeakSet` accept object keys and
//! unregistered symbol keys. Object-keyed entries flow through
//! ephemeron tables: values are marked only when their object key is
//! already reachable through another path.
//!
//! # Contents
//! - [`JsMap`] — heap-shared, tombstone-list associative store.
//! - [`JsSet`] — heap-shared, tombstone-list unique-element store.
//! - [`JsWeakMap`] — GC-managed weak map.
//! - [`JsWeakSet`] — GC-managed weak set.
//! - [`MapKey`] — equality key used by `JsMap`/`JsSet`. Implements
//!   ECMA-262 SameValueZero so `+0` / `-0` collapse and `NaN`
//!   matches itself.
//!
//! # Invariants
//! - `JsMap::set` / `JsSet::add` preserve insertion order; updating
//!   an existing key does not change its position.
//! - A host-frozen [`JsSet`] keeps its insertion snapshot immutable even when
//!   its mutators are invoked through `%Set.prototype%` directly.
//! - Two `JsMap` handles cloned from the same heap object share
//!   storage — both observe subsequent mutations.
//! - A capacity reservation may run moving GC. Map/Set insertion roots mutable
//!   key/value slots and derives `MapKey` plus write-barrier operands only
//!   after that reservation returns.
//! - `JsWeakMap` / `JsWeakSet` reject values that cannot be held weakly with
//!   [`CollectionError::NonObjectKey`].
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-map-objects>
//! - <https://tc39.es/ecma262/#sec-set-objects>
//! - <https://tc39.es/ecma262/#sec-weakmap-objects>
//! - <https://tc39.es/ecma262/#sec-weakset-objects>
//! - <https://tc39.es/ecma262/#sec-samevaluezero>

use crate::Value;
use crate::string::JsString;
use crate::symbol::JsSymbol;
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::{RawGc, SlotVisitor};

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`MapBody`].
pub const MAP_BODY_TYPE_TAG: u8 = 0x13;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`SetBody`].
pub const SET_BODY_TYPE_TAG: u8 = 0x14;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`WeakMapBody`].
pub const WEAK_MAP_BODY_TYPE_TAG: u8 = 0x15;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`WeakSetBody`].
pub const WEAK_SET_BODY_TYPE_TAG: u8 = 0x16;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for a `Map`'s entry table.
pub const MAP_TABLE_BODY_TYPE_TAG: u8 = 0x34;
/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for a `Set`'s entry table.
pub const SET_TABLE_BODY_TYPE_TAG: u8 = 0x35;

pub mod table;
pub(crate) mod weak_table;

/// Equality key for [`JsMap`] / [`JsSet`].
///
/// Implements ECMA-262 SameValueZero (§7.2.12): `+0` and `-0` map
/// to the same key, `NaN` matches itself, strings compare by
/// content, symbols compare by identity, migrated GC objects compare
/// by heap identity, and remaining callable shapes fall back to the
/// originating [`Value`] identity comparison.
///
/// The structural projection in [`MapKey::from_value`] normalises
/// `-0.0 → 0.0` so the equality + hashing paths can stay branch-free
/// on the hot insertion / lookup path. The canonical reference
/// implementation is [`crate::abstract_ops::same_value_zero`]; the
/// two paths agree element-for-element.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-samevaluezero>
/// - [`crate::abstract_ops::same_value_zero`]
#[derive(Debug, Clone)]
pub enum MapKey {
    /// `undefined` — singleton.
    Undefined,
    /// `null` — singleton.
    Null,
    /// `true` / `false`.
    Boolean(bool),
    /// IEEE-754 with SameValueZero collapsing (`+0`/`-0` map to the
    /// same key; `NaN` matches itself).
    Number(f64),
    /// BigInt — compared by exact value.
    BigInt(crate::bigint::BigIntValue),
    /// Strings compare by code-unit content.
    String(JsString),
    /// Symbols compare by handle identity.
    Symbol(JsSymbol),
    /// The original [`Value`] for the object key — kept so iteration
    /// can hand back the live key reference and the moving collector can
    /// rewrite the key slot in place.
    ObjectValue(Value),
}

impl MapKey {
    /// Project a [`Value`] into its [`MapKey`] form.
    ///
    /// # Algorithm
    /// 1. Primitives map to a structural variant (number normalises
    ///    `-0.0 → 0.0`).
    /// 2. Object-shaped values map to [`MapKey::ObjectValue`] so the key is a
    ///    traced slot. This keeps identity stable across young-generation
    ///    relocation.
    pub fn from_value(value: &Value, heap: &otter_gc::GcHeap) -> Self {
        if value.is_undefined() {
            MapKey::Undefined
        } else if value.is_null() {
            MapKey::Null
        } else if let Some(b) = value.as_boolean() {
            MapKey::Boolean(b)
        } else if let Some(n) = value.as_number() {
            let f = n.as_f64();
            // SameValueZero: collapse −0 to +0; preserve NaN bits.
            let normalised = if f == 0.0 { 0.0 } else { f };
            MapKey::Number(normalised)
        } else if let Some(b) = value.as_big_int() {
            MapKey::BigInt(b)
        } else if let Some(s) = value.as_string(heap) {
            MapKey::String(s)
        } else if let Some(s) = value.as_symbol(heap) {
            MapKey::Symbol(s)
        } else {
            // Object-shaped values map to ObjectValue (identity).
            MapKey::ObjectValue(*value)
        }
    }
}

impl MapKey {
    /// SameValueZero comparison for two projected keys. Strings
    /// compare by code-unit content (heap-aware); other variants use
    /// the same structural rules as the original `PartialEq` impl
    /// (retired with Phase B because string equality could not be
    /// expressed heap-free).
    #[must_use]
    pub fn matches(&self, other: &Self, heap: &otter_gc::GcHeap) -> bool {
        match (self, other) {
            (MapKey::Undefined, MapKey::Undefined) => true,
            (MapKey::Null, MapKey::Null) => true,
            (MapKey::Boolean(a), MapKey::Boolean(b)) => a == b,
            (MapKey::Number(a), MapKey::Number(b)) => {
                if a.is_nan() && b.is_nan() {
                    true
                } else {
                    a == b
                }
            }
            (MapKey::BigInt(a), MapKey::BigInt(b)) => a == b,
            (MapKey::String(a), MapKey::String(b)) => {
                if a.cached_hash() != b.cached_hash() || a.len() != b.len() {
                    return false;
                }
                a.equals(*b, heap)
            }
            (MapKey::Symbol(a), MapKey::Symbol(b)) => a.ptr_eq(*b),
            (MapKey::ObjectValue(a), MapKey::ObjectValue(b)) => a == b,
            _ => false,
        }
    }
}

/// Failure modes for collection mutations.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum CollectionError {
    /// Receiver is not the expected collection kind.
    #[error("collection method called on non-{expected} receiver")]
    BadReceiver {
        /// Expected JS-visible name (`"Map"` / `"WeakSet"` / …).
        expected: &'static str,
    },
    /// `WeakMap` / `WeakSet` rejects keys that cannot be held weakly.
    #[error("WeakMap / WeakSet keys must be objects or unregistered symbols")]
    NonObjectKey,
    /// Allocation or accounting failed while growing collection storage.
    #[error("out of memory: requested {requested_bytes} bytes, heap limit {heap_limit_bytes}")]
    OutOfMemory {
        /// Bytes requested.
        requested_bytes: u64,
        /// Heap cap (`0` = unlimited).
        heap_limit_bytes: u64,
    },
}

impl From<otter_gc::OutOfMemory> for CollectionError {
    fn from(err: otter_gc::OutOfMemory) -> Self {
        Self::OutOfMemory {
            requested_bytes: err.requested_bytes(),
            heap_limit_bytes: err.heap_limit_bytes(),
        }
    }
}

/// JS `Map` — ordered associative store.
///
/// Cloning shares storage. Storage is an insertion-ordered raw list
/// with tombstones for deleted entries, matching the spec's
/// `[[MapData]]` list so active iterators and `forEach` observe
/// deletes, clears, and later additions correctly.
pub type JsMap = otter_gc::Gc<MapBody>;

const COLLECTION_JIT_FLAG_PROTO_OVERRIDE: u32 = 1 << 0;
const COLLECTION_JIT_FLAG_EXPANDO: u32 = 1 << 1;

#[derive(Debug, Default, otter_macros::Pelt)]
#[pelt(tag = MAP_BODY_TYPE_TAG)]
#[repr(C)]
/// GC-allocated storage backing every [`JsMap`] handle.
pub struct MapBody {
    /// Machine-readable receiver guard flags for baseline method ICs.
    ///
    /// Bit 0 means the Map has an explicit prototype override. Bit 1 means it
    /// has an ordinary expando bag that may shadow prototype methods. The
    /// canonical fast path requires both bits to be zero and falls back to the
    /// full method path otherwise.
    #[pelt(skip)]
    jit_guard_flags: u32,
    /// Insertion-ordered `[[MapData]]` and its collision chains, in one
    /// GC body so the map owns nothing outside the heap. Null until the
    /// first insertion.
    table: table::TableHandle<MapEntry>,
    prototype_override: Option<Value>,
    /// Lazy ordinary own-property bag. Maps are ordinary extensible
    /// objects, so `m.x = 1` / `Object.defineProperty(m, …)` install here
    /// (the `[[MapData]]` entries are NOT own properties).
    expando: Option<crate::object::JsObject>,
}

impl MapBody {
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        if let Some(value) = &self.prototype_override {
            crate::code_liveness::visit_value(value, visitor);
        }
    }
}

pub(crate) const MAP_BODY_JIT_GUARD_FLAGS_OFFSET: usize =
    std::mem::offset_of!(MapBody, jit_guard_flags);

const _: () = assert!(MAP_BODY_JIT_GUARD_FLAGS_OFFSET.is_multiple_of(4));

#[derive(Debug, Clone)]
#[repr(C)]
pub(crate) struct MapEntry {
    /// Original key value. The compact entry keeps the spec value once rather
    /// than duplicating its potentially large [`MapKey`] projection.
    key: Value,
    /// Current mapped value.
    value: Value,
    /// Stable structural hash for indexed keys. Meaningful only when
    /// [`MAP_ENTRY_INDEXED`] is set.
    hash: u64,
    /// Next entry in the same bucket, or [`table::EMPTY`]. The chain
    /// lives in the entries themselves so the table needs no side index.
    next: u32,
    /// Compact liveness/indexability bits with a stable generated-code layout.
    flags: u32,
}

const MAP_ENTRY_LIVE: u32 = 1 << 0;
const MAP_ENTRY_INDEXED: u32 = 1 << 1;

pub(crate) const MAP_BODY_TABLE_OFFSET: usize = std::mem::offset_of!(MapBody, table);
pub(crate) const MAP_ENTRY_KEY_OFFSET: usize = std::mem::offset_of!(MapEntry, key);
pub(crate) const MAP_ENTRY_VALUE_OFFSET: usize = std::mem::offset_of!(MapEntry, value);
pub(crate) const MAP_ENTRY_NEXT_OFFSET: usize = std::mem::offset_of!(MapEntry, next);
pub(crate) const MAP_ENTRY_FLAGS_OFFSET: usize = std::mem::offset_of!(MapEntry, flags);
pub(crate) const MAP_ENTRY_SIZE: usize = std::mem::size_of::<MapEntry>();
pub(crate) const MAP_ENTRY_LIVE_FLAG: u32 = MAP_ENTRY_LIVE;

const _: () = assert!(MAP_ENTRY_SIZE == 32);
const _: () = assert!(MAP_ENTRY_KEY_OFFSET == 0);
const _: () = assert!(MAP_ENTRY_VALUE_OFFSET == 8);
const _: () = assert!(MAP_ENTRY_NEXT_OFFSET == 24);
const _: () = assert!(MAP_ENTRY_FLAGS_OFFSET == 28);

impl table::TableEntry for MapEntry {
    const TABLE_TYPE_TAG: u8 = MAP_TABLE_BODY_TYPE_TAG;

    fn trace_entry(&mut self, visitor: &mut SlotVisitor<'_>) {
        <Self as crate::pelt::PeltField>::pelt_trace(self, visitor);
    }

    fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        if self.is_live() {
            crate::code_liveness::visit_value(&self.key, visitor);
            crate::code_liveness::visit_value(&self.value, visitor);
        }
    }

    fn entry_hash(&self) -> Option<u64> {
        (self.flags & MAP_ENTRY_INDEXED != 0).then_some(self.hash)
    }

    fn next(&self) -> u32 {
        self.next
    }

    fn set_next(&mut self, next: u32) {
        self.next = next;
    }

    fn vacant() -> Self {
        Self {
            key: Value::hole(),
            value: Value::hole(),
            hash: 0,
            next: table::EMPTY,
            flags: 0,
        }
    }
}

impl crate::pelt::PeltField for MapEntry {
    fn pelt_trace(&mut self, visitor: &mut SlotVisitor<'_>) {
        if self.is_live() {
            self.key.trace_value_slot_mut(visitor);
            self.value.trace_value_slot_mut(visitor);
        }
    }
}

impl MapEntry {
    fn live(key_hash: MapKey, key: Value, value: Value) -> Self {
        let hash = map_key_hash(&key_hash);
        Self {
            key,
            value,
            hash: hash.unwrap_or(0),
            next: table::EMPTY,
            flags: MAP_ENTRY_LIVE | (u32::from(hash.is_some()) * MAP_ENTRY_INDEXED),
        }
    }

    fn is_live(&self) -> bool {
        self.flags & MAP_ENTRY_LIVE != 0
    }

    fn key_matches(&self, key: &MapKey, heap: &otter_gc::GcHeap) -> bool {
        self.is_live() && MapKey::from_value(&self.key, heap).matches(key, heap)
    }

    fn pair(&self) -> Option<(Value, Value)> {
        self.is_live().then_some((self.key, self.value))
    }

    fn clear(&mut self) {
        self.key = Value::hole();
        self.value = Value::hole();
        self.hash = 0;
        self.flags = 0;
    }
}

impl MapKey {
    /// Remember every GC reference this key holds against `parent`.
    ///
    /// Used when entries are copied into a fresh table behind the
    /// mutator's back, where no ordinary store barrier ran.
    fn record_into<T: ?Sized>(&self, heap: &mut otter_gc::GcHeap, parent: otter_gc::Gc<T>) {
        match self {
            Self::Undefined | Self::Null | Self::Boolean(_) | Self::Number(_) => {}
            Self::BigInt(value) => heap.record_write(parent, &Value::big_int(*value)),
            Self::String(value) => heap.record_write(parent, &Value::string(*value)),
            Self::Symbol(value) => heap.record_write(parent, &Value::symbol(*value)),
            Self::ObjectValue(value) => heap.record_write(parent, value),
        }
    }
}

impl MapBody {
    /// The appended entries, tombstones included.
    pub(crate) fn entries(&self) -> &[MapEntry] {
        table::body_of(self.table).map_or(&[], |body| {
            // SAFETY: the handle names a live table payload whose entry
            // prefix outlives this borrow of the map body.
            unsafe { std::slice::from_raw_parts((*body).entries().as_ptr(), (*body).len()) }
        })
    }

    /// The appended entries, mutably.
    pub(crate) fn entries_mut(&mut self) -> &mut [MapEntry] {
        table::body_of(self.table).map_or(&mut [], |body| {
            // SAFETY: as in `entries`; `&mut self` rules out an aliasing
            // read of the same map body.
            unsafe {
                std::slice::from_raw_parts_mut((*body).entries_mut().as_mut_ptr(), (*body).len())
            }
        })
    }

    /// Entries the table can take before it must grow.
    pub(crate) fn entry_capacity(&self) -> usize {
        table::body_of(self.table).map_or(0, |body| {
            // SAFETY: the handle names a live table payload.
            unsafe { (*body).capacity() }
        })
    }

    fn table_mut(&mut self) -> Option<&mut table::OrderedTableBody<MapEntry>> {
        // SAFETY: the handle names a live table payload, and `&mut self`
        // rules out another borrow of it through this map.
        table::body_of(self.table).map(|body| unsafe { &mut *body })
    }
}

impl SetBody {
    /// The appended entries, tombstones included.
    pub(crate) fn entries(&self) -> &[SetEntry] {
        table::body_of(self.table).map_or(&[], |body| {
            // SAFETY: the handle names a live table payload whose entry
            // prefix outlives this borrow of the set body.
            unsafe { std::slice::from_raw_parts((*body).entries().as_ptr(), (*body).len()) }
        })
    }

    /// The appended entries, mutably.
    pub(crate) fn entries_mut(&mut self) -> &mut [SetEntry] {
        table::body_of(self.table).map_or(&mut [], |body| {
            // SAFETY: as in `entries`.
            unsafe {
                std::slice::from_raw_parts_mut((*body).entries_mut().as_mut_ptr(), (*body).len())
            }
        })
    }

    /// Entries the table can take before it must grow.
    pub(crate) fn entry_capacity(&self) -> usize {
        table::body_of(self.table).map_or(0, |body| {
            // SAFETY: the handle names a live table payload.
            unsafe { (*body).capacity() }
        })
    }

    fn table_mut(&mut self) -> Option<&mut table::OrderedTableBody<SetEntry>> {
        // SAFETY: as in `MapBody::table_mut`.
        table::body_of(self.table).map(|body| unsafe { &mut *body })
    }
}

/// Structural hash of an indexable [`MapKey`], or `None` when the key is
/// identity-based (symbol / object) and therefore not GC-stable enough to
/// index — those keys fall back to a linear scan.
///
/// `NaN` collapses to a single canonical hash so all `NaN` keys land in the
/// same bucket (SameValueZero treats them equal); `-0`/`+0` were already
/// collapsed in [`MapKey::from_value`]. Strings use the heap-free content
/// [`JsString::cached_hash`]. The final avalanche is required because ordered
/// tables select a bucket from the low hash bits: adjacent integral doubles
/// differ mainly in their high IEEE-754 bits and otherwise collapse into one
/// small-table bucket. Heap-independent by construction.
fn map_key_hash(key: &MapKey) -> Option<u64> {
    let mut hash = 0;
    match key {
        MapKey::Undefined => hash = fx_hash_word(hash, 0),
        MapKey::Null => hash = fx_hash_word(hash, 1),
        MapKey::Boolean(b) => {
            hash = fx_hash_word(hash, 2);
            hash = fx_hash_word(hash, u64::from(*b));
        }
        MapKey::Number(f) => {
            hash = fx_hash_word(hash, MAP_NUMBER_HASH_TAG);
            let bits = if f.is_nan() {
                f64::NAN.to_bits()
            } else {
                f.to_bits()
            };
            hash = fx_hash_word(hash, bits);
        }
        MapKey::String(s) => {
            hash = fx_hash_word(hash, 4);
            hash = fx_hash_word(hash, u64::from(s.cached_hash()));
        }
        MapKey::BigInt(_) | MapKey::Symbol(_) | MapKey::ObjectValue(_) => return None,
    }
    Some(avalanche_map_hash(hash.rotate_left(26)))
}

pub(crate) const MAP_FX_HASH_MULTIPLIER: u64 = 0xf135_7aea_2e62_a9c5;
pub(crate) const MAP_HASH_AVALANCHE_1: u64 = 0xff51_afd7_ed55_8ccd;
pub(crate) const MAP_HASH_AVALANCHE_2: u64 = 0xc4ce_b9fe_1a85_ec53;
pub(crate) const MAP_NUMBER_HASH_TAG: u64 = 3;

#[inline]
fn fx_hash_word(hash: u64, word: u64) -> u64 {
    hash.wrapping_add(word).wrapping_mul(MAP_FX_HASH_MULTIPLIER)
}

/// Spread every input bit into the low bits consumed by
/// [`table::OrderedTableBody`].
#[inline]
fn avalanche_map_hash(mut hash: u64) -> u64 {
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(MAP_HASH_AVALANCHE_1);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(MAP_HASH_AVALANCHE_2);
    hash ^ (hash >> 33)
}

/// Locate the live entry index for `key` in `body`.
///
/// Indexable keys probe the hash index then verify the real key via
/// [`MapEntry::key_matches`] (so hash collisions stay correct); identity
/// keys fall back to the linear scan. Returns the index into `body.entries`
/// (which is append-plus-tombstone, so indices are stable for the life of
/// the entry).
fn map_find_entry(body: &MapBody, key: &MapKey, heap: &otter_gc::GcHeap) -> Option<usize> {
    let entries = body.entries();
    let Some(hash) = map_key_hash(key) else {
        return entries.iter().position(|e| e.key_matches(key, heap));
    };
    let table = table::body_of(body.table)?;
    // SAFETY: the handle names a live table payload.
    let mut current = unsafe { (*table).bucket_head(hash) };
    while current != table::EMPTY {
        let index = current as usize;
        let entry = entries.get(index)?;
        if entry.key_matches(key, heap) {
            return Some(index);
        }
        current = entry.next;
    }
    None
}

/// Unlink a now-tombstoned entry from its collision chain (no-op for
/// non-indexable keys, which were never chained).
fn map_index_remove(body: &mut MapBody, key: &MapKey, entry_idx: usize) {
    if let Some(hash) = map_key_hash(key)
        && let Some(table) = body.table_mut()
    {
        table.unlink(hash, entry_idx);
    }
}

/// Allocate a fresh empty `Map`.
pub fn alloc_map(heap: &mut otter_gc::GcHeap) -> Result<JsMap, otter_gc::OutOfMemory> {
    heap.alloc_old(MapBody::default())
}

/// Allocate a fresh empty `Map` while exposing caller-owned roots.
pub(crate) fn alloc_map_with_roots(
    heap: &mut otter_gc::GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsMap, otter_gc::OutOfMemory> {
    heap.alloc_with_roots(MapBody::default(), external_visit)
}

pub(crate) fn map_prototype_override(map: JsMap, heap: &otter_gc::GcHeap) -> Option<Value> {
    heap.read_payload(map, |body| body.prototype_override)
}

pub(crate) fn set_map_prototype_override(
    map: JsMap,
    heap: &mut otter_gc::GcHeap,
    proto: Option<Value>,
) {
    let barrier_value = proto;
    heap.with_payload(map, |body| {
        if proto.is_some() {
            body.jit_guard_flags |= COLLECTION_JIT_FLAG_PROTO_OVERRIDE;
        } else {
            body.jit_guard_flags &= !COLLECTION_JIT_FLAG_PROTO_OVERRIDE;
        }
        body.prototype_override = proto;
    });
    if let Some(value) = &barrier_value {
        heap.record_write(map, value);
    }
}

/// Ordinary own-property bag for a Map, if materialized.
pub(crate) fn map_expando(map: JsMap, heap: &otter_gc::GcHeap) -> Option<crate::object::JsObject> {
    heap.read_payload(map, |body| body.expando)
}

/// Install the Map's ordinary own-property bag.
pub(crate) fn map_set_expando(
    map: JsMap,
    heap: &mut otter_gc::GcHeap,
    bag: crate::object::JsObject,
) {
    heap.with_payload(map, |body| {
        body.jit_guard_flags |= COLLECTION_JIT_FLAG_EXPANDO;
        body.expando = Some(bag);
    });
    heap.write_barrier(map, bag);
}

/// Number of entries.
#[must_use]
pub fn map_len(map: JsMap, heap: &otter_gc::GcHeap) -> usize {
    heap.read_payload(map, |body| {
        body.entries()
            .iter()
            .filter(|entry| entry.is_live())
            .count()
    })
}

/// `true` when empty.
#[must_use]
pub fn map_is_empty(map: JsMap, heap: &otter_gc::GcHeap) -> bool {
    map_len(map, heap) == 0
}

/// `Map.prototype.get` — Spec §24.1.3.6.
#[must_use]
pub fn map_get(map: JsMap, heap: &otter_gc::GcHeap, key: &Value) -> Option<Value> {
    let k = MapKey::from_value(key, heap);
    heap.read_payload(map, |body| {
        map_find_entry(body, &k, heap).map(|idx| body.entries()[idx].value)
    })
}

/// `Map.prototype.has` — Spec §24.1.3.7.
#[must_use]
pub fn map_has(map: JsMap, heap: &otter_gc::GcHeap, key: &Value) -> bool {
    let k = MapKey::from_value(key, heap);
    heap.read_payload(map, |body| map_find_entry(body, &k, heap).is_some())
}

/// `Map.prototype.set` — Spec §24.1.3.9. Updates in place
/// without changing insertion order; new keys append.
pub fn map_set(
    mut map: JsMap,
    heap: &mut otter_gc::GcHeap,
    mut key: Value,
    mut value: Value,
) -> Result<(), otter_gc::OutOfMemory> {
    let lookup_key = MapKey::from_value(&key, heap);
    let needs_insert = heap.read_payload(map, |body| {
        map_find_entry(body, &lookup_key, heap).is_none()
    });
    if needs_insert {
        let target_len = heap.read_payload(map, |body| body.entries().len().saturating_add(1));
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            key.trace_value_slot_mut(visitor);
            value.trace_value_slot_mut(visitor);
        };
        reserve_map_for_target_len_with_roots(&mut map, heap, target_len, &mut reserve_roots)?;
    }

    // Reserving external collection storage can run a moving collection. Both
    // values above are real mutable root slots, so rebuild every derived key
    // and barrier operand only after the reserve returns.
    let k = MapKey::from_value(&key, heap);
    let existing_idx = heap.read_payload(map, |body| map_find_entry(body, &k, heap));
    let exists = existing_idx.is_some();
    heap.with_payload(map, |body| match existing_idx {
        Some(idx) => body.entries_mut()[idx].value = value,
        None => {
            if let Some(table) = body.table_mut() {
                table.push(MapEntry::live(k, key, value));
            }
        }
    });
    if !exists {
        record_map_write(heap, map, &key);
    }
    record_map_write(heap, map, &value);
    Ok(())
}

/// `Map.prototype.set` restricted to a key the map already holds.
///
/// Overwriting an existing entry rewrites one slot and runs its write barrier;
/// insertion order is unchanged and nothing allocates, which is what lets a
/// leaf entry perform it with no safepoint and no rooting packet. Returns
/// `false` when the key is absent, leaving the map untouched so the allocating
/// path can append.
#[must_use]
pub fn map_set_existing(
    map: JsMap,
    heap: &mut otter_gc::GcHeap,
    key: &Value,
    value: Value,
) -> bool {
    let k = MapKey::from_value(key, heap);
    let Some(idx) = heap.read_payload(map, |body| map_find_entry(body, &k, heap)) else {
        return false;
    };
    heap.with_payload(map, |body| body.entries_mut()[idx].value = value);
    record_map_write(heap, map, &value);
    true
}

/// `Map.prototype.set` for stack-visible VM construction paths.
pub(crate) fn map_set_with_roots(
    map: &mut JsMap,
    heap: &mut otter_gc::GcHeap,
    mut key: Value,
    mut value: Value,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    let lookup_key = MapKey::from_value(&key, heap);
    let needs_insert = heap.read_payload(*map, |body| {
        map_find_entry(body, &lookup_key, heap).is_none()
    });
    if needs_insert {
        let target_len = heap.read_payload(*map, |body| body.entries().len().saturating_add(1));
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            key.trace_value_slot_mut(visitor);
            value.trace_value_slot_mut(visitor);
        };
        reserve_map_for_target_len_with_roots(map, heap, target_len, &mut reserve_roots)?;
    }

    let k = MapKey::from_value(&key, heap);
    let existing_idx = heap.read_payload(*map, |body| map_find_entry(body, &k, heap));
    let exists = existing_idx.is_some();
    heap.with_payload(*map, |body| match existing_idx {
        Some(idx) => body.entries_mut()[idx].value = value,
        None => {
            if let Some(table) = body.table_mut() {
                table.push(MapEntry::live(k, key, value));
            }
        }
    });
    if !exists {
        record_map_write(heap, *map, &key);
    }
    record_map_write(heap, *map, &value);
    Ok(())
}

/// `Map.prototype.delete` — Spec §24.1.3.3. Returns `true` when
/// the entry existed.
pub fn map_delete(map: JsMap, heap: &mut otter_gc::GcHeap, key: &Value) -> bool {
    let k = MapKey::from_value(key, heap);
    let idx = heap.read_payload(map, |body| map_find_entry(body, &k, heap));
    match idx {
        Some(idx) => {
            heap.with_payload(map, |body| {
                map_index_remove(body, &k, idx);
                body.entries_mut()[idx].clear();
            });
            true
        }
        None => false,
    }
}

/// `Map.prototype.clear` — Spec §24.1.3.2.
pub fn map_clear(map: JsMap, heap: &mut otter_gc::GcHeap) {
    heap.with_payload(map, |body| {
        for entry in body.entries_mut() {
            entry.clear();
        }
        if let Some(table) = body.table_mut() {
            table.clear();
        }
    });
}

/// Snapshot key list (in insertion order).
#[must_use]
pub fn map_keys(map: JsMap, heap: &otter_gc::GcHeap) -> Vec<Value> {
    heap.read_payload(map, |body| {
        body.entries()
            .iter()
            .filter_map(|entry| entry.is_live().then_some(entry.key))
            .collect()
    })
}

/// Snapshot value list (in insertion order).
#[must_use]
pub fn map_values(map: JsMap, heap: &otter_gc::GcHeap) -> Vec<Value> {
    heap.read_payload(map, |body| {
        body.entries()
            .iter()
            .filter_map(|entry| entry.is_live().then_some(entry.value))
            .collect()
    })
}

/// Snapshot entry list.
#[must_use]
pub fn map_entries(map: JsMap, heap: &otter_gc::GcHeap) -> Vec<(Value, Value)> {
    heap.read_payload(map, |body| {
        body.entries().iter().filter_map(MapEntry::pair).collect()
    })
}

/// Raw backing-list length, including deleted tombstone slots.
#[must_use]
pub(crate) fn map_raw_len(map: JsMap, heap: &otter_gc::GcHeap) -> usize {
    heap.read_payload(map, |body| body.entries().len())
}

/// Read the raw entry currently at `index` in insertion order.
#[must_use]
pub(crate) fn map_entry_at(
    map: JsMap,
    heap: &otter_gc::GcHeap,
    index: usize,
) -> Option<(Value, Value)> {
    heap.read_payload(map, |body| {
        body.entries().get(index).and_then(MapEntry::pair)
    })
}

/// Identity comparison.
#[must_use]
pub fn map_ptr_eq(a: JsMap, b: JsMap) -> bool {
    a == b
}

/// JS `Set` — ordered unique-element store.
pub type JsSet = otter_gc::Gc<SetBody>;

#[derive(Debug, Default, otter_macros::Pelt)]
#[pelt(tag = SET_BODY_TYPE_TAG)]
#[repr(C)]
/// GC-allocated storage backing every [`JsSet`] handle.
pub struct SetBody {
    /// Machine-readable receiver guard flags for baseline method ICs. Same bit
    /// layout as [`MapBody::jit_guard_flags`].
    #[pelt(skip)]
    jit_guard_flags: u32,
    /// Host-owned immutable snapshots (for example Node's allowed environment
    /// flags) ignore all Set mutators, including prototype-borrowed calls.
    #[pelt(skip)]
    readonly: bool,
    /// Insertion-ordered `[[SetData]]` and its collision chains, in one
    /// GC body. Deleted entries become tombstones so active iterators and
    /// `forEach` observe later additions before exhaustion. Null until
    /// the first insertion.
    table: table::TableHandle<SetEntry>,
    prototype_override: Option<Value>,
    /// Lazy ordinary own-property bag (see [`MapBody::expando`]).
    expando: Option<crate::object::JsObject>,
}

impl SetBody {
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        if let Some(value) = &self.prototype_override {
            crate::code_liveness::visit_value(value, visitor);
        }
    }
}

pub(crate) const SET_BODY_JIT_GUARD_FLAGS_OFFSET: usize =
    std::mem::offset_of!(SetBody, jit_guard_flags);

const _: () = assert!(SET_BODY_JIT_GUARD_FLAGS_OFFSET == MAP_BODY_JIT_GUARD_FLAGS_OFFSET);

#[derive(Debug, Clone)]
pub(crate) struct SetEntry {
    key_hash: Option<MapKey>,
    value: Option<Value>,
    /// Next entry in the same bucket, or [`table::EMPTY`].
    next: u32,
}

impl table::TableEntry for SetEntry {
    const TABLE_TYPE_TAG: u8 = SET_TABLE_BODY_TYPE_TAG;

    fn trace_entry(&mut self, visitor: &mut SlotVisitor<'_>) {
        <Self as crate::pelt::PeltField>::pelt_trace(self, visitor);
    }

    fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        if let Some(MapKey::ObjectValue(value)) = &self.key_hash {
            crate::code_liveness::visit_value(value, visitor);
        }
        if let Some(value) = &self.value {
            crate::code_liveness::visit_value(value, visitor);
        }
    }

    fn entry_hash(&self) -> Option<u64> {
        map_key_hash(self.key_hash.as_ref()?)
    }

    fn next(&self) -> u32 {
        self.next
    }

    fn set_next(&mut self, next: u32) {
        self.next = next;
    }

    fn vacant() -> Self {
        Self {
            key_hash: None,
            value: None,
            next: table::EMPTY,
        }
    }
}

impl crate::pelt::PeltField for SetEntry {
    fn pelt_trace(&mut self, visitor: &mut SlotVisitor<'_>) {
        if let Some(key_hash) = &mut self.key_hash {
            <MapKey as crate::pelt::PeltField>::pelt_trace(key_hash, visitor);
        }
        if let Some(value) = &mut self.value {
            value.trace_value_slot_mut(visitor);
        }
    }
}

impl SetEntry {
    fn live(key_hash: MapKey, value: Value) -> Self {
        Self {
            key_hash: Some(key_hash),
            value: Some(value),
            next: table::EMPTY,
        }
    }

    fn key_matches(&self, key: &MapKey, heap: &otter_gc::GcHeap) -> bool {
        self.value.is_some()
            && self
                .key_hash
                .as_ref()
                .is_some_and(|stored| stored.matches(key, heap))
    }

    fn clear(&mut self) {
        self.key_hash = None;
        self.value = None;
    }
}

/// Set analogue of [`map_find_entry`] — hash-index probe with linear
/// fallback for identity keys.
fn set_find_entry(body: &SetBody, key: &MapKey, heap: &otter_gc::GcHeap) -> Option<usize> {
    if let Some(hash) = map_key_hash(key) {
        let entries = body.entries();
        let table = table::body_of(body.table)?;
        // SAFETY: the handle names a live table payload.
        let mut current = unsafe { (*table).bucket_head(hash) };
        while current != table::EMPTY {
            let index = current as usize;
            let entry = entries.get(index)?;
            if entry.key_matches(key, heap) {
                return Some(index);
            }
            current = entry.next;
        }
        None
    } else {
        body.entries().iter().position(|e| e.key_matches(key, heap))
    }
}

/// Unlink a now-tombstoned entry from its collision chain.
fn set_index_remove(body: &mut SetBody, key: &MapKey, entry_idx: usize) {
    if let Some(hash) = map_key_hash(key)
        && let Some(table) = body.table_mut()
    {
        table.unlink(hash, entry_idx);
    }
}

/// Allocate a fresh empty `Set`.
pub fn alloc_set(heap: &mut otter_gc::GcHeap) -> Result<JsSet, otter_gc::OutOfMemory> {
    heap.alloc_old(SetBody::default())
}

/// Allocate a fresh empty `Set` while exposing caller-owned roots.
pub(crate) fn alloc_set_with_roots(
    heap: &mut otter_gc::GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsSet, otter_gc::OutOfMemory> {
    heap.alloc_with_roots(SetBody::default(), external_visit)
}

pub(crate) fn set_prototype_override(set: JsSet, heap: &otter_gc::GcHeap) -> Option<Value> {
    heap.read_payload(set, |body| body.prototype_override)
}

/// Ordinary own-property bag for a Set, if materialized.
pub(crate) fn set_expando(set: JsSet, heap: &otter_gc::GcHeap) -> Option<crate::object::JsObject> {
    heap.read_payload(set, |body| body.expando)
}

/// Install the Set's ordinary own-property bag.
pub(crate) fn set_set_expando(
    set: JsSet,
    heap: &mut otter_gc::GcHeap,
    bag: crate::object::JsObject,
) {
    heap.with_payload(set, |body| {
        body.jit_guard_flags |= COLLECTION_JIT_FLAG_EXPANDO;
        body.expando = Some(bag);
    });
    heap.write_barrier(set, bag);
}

pub(crate) fn set_set_prototype_override(
    set: JsSet,
    heap: &mut otter_gc::GcHeap,
    proto: Option<Value>,
) {
    let barrier_value = proto;
    heap.with_payload(set, |body| {
        if proto.is_some() {
            body.jit_guard_flags |= COLLECTION_JIT_FLAG_PROTO_OVERRIDE;
        } else {
            body.jit_guard_flags &= !COLLECTION_JIT_FLAG_PROTO_OVERRIDE;
        }
        body.prototype_override = proto;
    });
    if let Some(value) = &barrier_value {
        heap.record_write(set, value);
    }
}

/// Number of unique entries.
#[must_use]
pub fn set_len(set: JsSet, heap: &otter_gc::GcHeap) -> usize {
    heap.read_payload(set, |body| {
        body.entries()
            .iter()
            .filter(|entry| entry.value.is_some())
            .count()
    })
}

/// `true` when empty.
#[must_use]
pub fn set_is_empty(set: JsSet, heap: &otter_gc::GcHeap) -> bool {
    set_len(set, heap) == 0
}

/// `Set.prototype.has` — Spec §24.2.3.7.
#[must_use]
pub fn set_has(set: JsSet, heap: &otter_gc::GcHeap, value: &Value) -> bool {
    let k = MapKey::from_value(value, heap);
    heap.read_payload(set, |body| set_find_entry(body, &k, heap).is_some())
}

/// `Set.prototype.add` — Spec §24.2.3.1.
pub fn set_add(
    mut set: JsSet,
    heap: &mut otter_gc::GcHeap,
    mut value: Value,
) -> Result<(), otter_gc::OutOfMemory> {
    if set_is_readonly(set, heap) {
        return Ok(());
    }
    let lookup_key = MapKey::from_value(&value, heap);
    let needs_insert = heap.read_payload(set, |body| {
        set_find_entry(body, &lookup_key, heap).is_none()
    });
    if needs_insert {
        let target_len = heap.read_payload(set, |body| body.entries().len().saturating_add(1));
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            value.trace_value_slot_mut(visitor);
        };
        reserve_set_for_target_len_with_roots(&mut set, heap, target_len, &mut reserve_roots)?;
    }

    let k = MapKey::from_value(&value, heap);
    let exists = heap.read_payload(set, |body| set_find_entry(body, &k, heap).is_some());
    if !exists {
        heap.with_payload(set, |body| {
            if let Some(table) = body.table_mut() {
                table.push(SetEntry::live(k, value));
            }
        });
        record_set_write(heap, set, &value);
    }
    Ok(())
}

/// `Set.prototype.add` for stack-visible VM construction paths.
pub(crate) fn set_add_with_roots(
    set: &mut JsSet,
    heap: &mut otter_gc::GcHeap,
    mut value: Value,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    if set_is_readonly(*set, heap) {
        return Ok(());
    }
    let lookup_key = MapKey::from_value(&value, heap);
    let needs_insert = heap.read_payload(*set, |body| {
        set_find_entry(body, &lookup_key, heap).is_none()
    });
    if needs_insert {
        let target_len = heap.read_payload(*set, |body| body.entries().len().saturating_add(1));
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            value.trace_value_slot_mut(visitor);
        };
        reserve_set_for_target_len_with_roots(set, heap, target_len, &mut reserve_roots)?;
    }

    let k = MapKey::from_value(&value, heap);
    let exists = heap.read_payload(*set, |body| set_find_entry(body, &k, heap).is_some());
    if !exists {
        heap.with_payload(*set, |body| {
            if let Some(table) = body.table_mut() {
                table.push(SetEntry::live(k, value));
            }
        });
        record_set_write(heap, *set, &value);
    }
    Ok(())
}

/// `Set.prototype.delete` — Spec §24.2.3.4.
pub fn set_delete(set: JsSet, heap: &mut otter_gc::GcHeap, value: &Value) -> bool {
    if set_is_readonly(set, heap) {
        return false;
    }
    let k = MapKey::from_value(value, heap);
    let idx = heap.read_payload(set, |body| set_find_entry(body, &k, heap));
    match idx {
        Some(idx) => {
            heap.with_payload(set, |body| {
                set_index_remove(body, &k, idx);
                body.entries_mut()[idx].clear();
            });
            true
        }
        None => false,
    }
}

/// `Set.prototype.clear` — Spec §24.2.3.3.
pub fn set_clear(set: JsSet, heap: &mut otter_gc::GcHeap) {
    if set_is_readonly(set, heap) {
        return;
    }
    heap.with_payload(set, |body| {
        for entry in body.entries_mut() {
            entry.clear();
        }
        if let Some(table) = body.table_mut() {
            table.clear();
        }
    });
}

/// Whether host code has sealed this Set's internal entry snapshot.
#[must_use]
pub fn set_is_readonly(set: JsSet, heap: &otter_gc::GcHeap) -> bool {
    heap.read_payload(set, |body| body.readonly)
}

/// Seal a host-owned Set snapshot and freeze its ordinary expando properties.
/// ECMAScript `Object.freeze(new Set())` does not call this: normal Sets remain
/// free to mutate their internal `[[SetData]]` after ordinary property freeze.
pub fn set_make_readonly(set: JsSet, heap: &mut otter_gc::GcHeap) {
    let expando = heap.read_payload(set, |body| body.expando);
    heap.with_payload(set, |body| body.readonly = true);
    if let Some(expando) = expando {
        crate::object::freeze(expando, heap);
    }
}

/// Snapshot value list in insertion order.
#[must_use]
pub fn set_values(set: JsSet, heap: &otter_gc::GcHeap) -> Vec<Value> {
    heap.read_payload(set, |body| {
        body.entries()
            .iter()
            .filter_map(|entry| entry.value)
            .collect()
    })
}

/// Raw backing-list length, including deleted tombstone slots.
#[must_use]
pub(crate) fn set_raw_len(set: JsSet, heap: &otter_gc::GcHeap) -> usize {
    heap.read_payload(set, |body| body.entries().len())
}

/// Read the raw set value currently at `index` in insertion order.
#[must_use]
pub(crate) fn set_value_at(set: JsSet, heap: &otter_gc::GcHeap, index: usize) -> Option<Value> {
    heap.read_payload(set, |body| {
        body.entries().get(index).and_then(|entry| entry.value)
    })
}

/// Identity comparison.
#[must_use]
pub fn set_ptr_eq(a: JsSet, b: JsSet) -> bool {
    a == b
}

/// JS `WeakMap` — weakly-held object / unregistered-symbol-key table.
pub type JsWeakMap = otter_gc::Gc<WeakMapBody>;

#[derive(Debug, Clone, Copy)]
pub(crate) enum WeakCollectionKey {
    Object(RawGc),
    Symbol(JsSymbol),
}

impl WeakCollectionKey {
    fn matches(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Object(a), Self::Object(b)) => a == b,
            (Self::Symbol(a), Self::Symbol(b)) => a.ptr_eq(*b),
            _ => false,
        }
    }

    fn is_live_object_key(&self) -> bool {
        match self {
            Self::Object(raw) => !raw.is_null(),
            Self::Symbol(_) => true,
        }
    }

    /// Identity hash for the weak-collection index.
    ///
    /// Weak keys are compared by identity, so the address is the hash. A moving
    /// collection rewrites those addresses, which is why the index that uses
    /// this is invalidated from the ephemeron walk rather than kept live.
    fn identity_hash(&self) -> u64 {
        use core::hash::{Hash, Hasher};
        let mut hasher = rustc_hash::FxHasher::default();
        match self {
            Self::Object(raw) => {
                0u8.hash(&mut hasher);
                raw.0.hash(&mut hasher);
            }
            Self::Symbol(symbol) => {
                1u8.hash(&mut hasher);
                symbol.identity_addr().hash(&mut hasher);
            }
        }
        hasher.finish()
    }
}

#[derive(Default, otter_macros::Pelt)]
#[pelt(tag = WEAK_MAP_BODY_TYPE_TAG, ephemeron_via = weak_map_ephemeron_walk)]
/// GC-allocated storage backing every [`JsWeakMap`] handle.
///
/// The entries live in a [`weak_table::WeakTableBody`] this body names
/// by handle. The handle is a strong edge — the table cell must live as
/// long as the map — but the table's own strong trace is empty:
/// ephemeron entries are not ordinary edges, and the `ephemeron_via`
/// hook reaches them through the handle so the fixpoint marks a value
/// only after its key is already live.
pub struct WeakMapBody {
    table: weak_table::WeakTableHandle<weak_table::MapKind>,
    prototype_override: Option<Value>,
    /// Lazy ordinary own-property bag (see [`MapBody::expando`]).
    expando: Option<crate::object::JsObject>,
}

impl WeakMapBody {
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        if let Some(value) = &self.prototype_override {
            crate::code_liveness::visit_value(value, visitor);
        }
    }
}

pub(crate) fn weak_map_expando(
    map: JsWeakMap,
    heap: &otter_gc::GcHeap,
) -> Option<crate::object::JsObject> {
    heap.read_payload(map, |body| body.expando)
}

pub(crate) fn weak_map_set_expando(
    map: JsWeakMap,
    heap: &mut otter_gc::GcHeap,
    bag: crate::object::JsObject,
) {
    heap.with_payload(map, |body| {
        body.expando = Some(bag);
    });
    heap.write_barrier(map, bag);
}

pub(crate) fn weak_set_expando(
    set: JsWeakSet,
    heap: &otter_gc::GcHeap,
) -> Option<crate::object::JsObject> {
    heap.read_payload(set, |body| body.expando)
}

pub(crate) fn weak_set_set_expando(
    set: JsWeakSet,
    heap: &mut otter_gc::GcHeap,
    bag: crate::object::JsObject,
) {
    heap.with_payload(set, |body| {
        body.expando = Some(bag);
    });
    heap.write_barrier(set, bag);
}

fn weak_map_ephemeron_walk(
    body: &mut WeakMapBody,
    visitor: &mut otter_gc::trace::EphemeronVisitor<'_>,
) {
    let Some(table) = weak_table::body_of(body.table) else {
        return;
    };
    // SAFETY: the handle names a live old-space table; entry slot
    // addresses are stable for the duration of the walk.
    let table = unsafe { &mut *table };
    // The walk can relocate every key, and the chains hash their
    // addresses.
    table.mark_stale();
    for entry in table.entries_mut() {
        let weak_table::WeakEntry { key, value, .. } = entry;
        if let WeakCollectionKey::Object(raw) = key {
            let key_slot = raw as *mut RawGc;
            let mut visit_value_slots =
                |slot_visitor: &mut SlotVisitor<'_>| value.trace_value_slots(slot_visitor);
            visitor(key_slot, &mut visit_value_slots);
        }
    }
}

/// Allocate a fresh empty `WeakMap`.
pub fn alloc_weak_map(heap: &mut otter_gc::GcHeap) -> Result<JsWeakMap, otter_gc::OutOfMemory> {
    let map = heap.alloc_old(WeakMapBody::default())?;
    heap.register_ephemeron_table(map);
    Ok(map)
}

/// Allocate a fresh empty `WeakMap` while exposing caller-owned roots.
pub(crate) fn alloc_weak_map_with_roots(
    heap: &mut otter_gc::GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsWeakMap, otter_gc::OutOfMemory> {
    let map = heap.alloc_with_roots(WeakMapBody::default(), external_visit)?;
    heap.register_ephemeron_table(map);
    Ok(map)
}

pub(crate) fn weak_map_prototype_override(
    map: JsWeakMap,
    heap: &otter_gc::GcHeap,
) -> Option<Value> {
    heap.read_payload(map, |body| body.prototype_override)
}

pub(crate) fn set_weak_map_prototype_override(
    map: JsWeakMap,
    heap: &mut otter_gc::GcHeap,
    proto: Option<Value>,
) {
    let barrier_value = proto;
    heap.with_payload(map, |body| {
        body.prototype_override = proto;
    });
    if let Some(value) = &barrier_value {
        heap.record_write(map, value);
    }
}

/// Run `f` over the map's table, or return `default` when the map has
/// never grown one.
fn with_weak_map_table<R>(
    heap: &otter_gc::GcHeap,
    map: JsWeakMap,
    default: R,
    f: impl FnOnce(&mut weak_table::WeakTableBody<weak_table::MapKind>) -> R,
) -> R {
    heap.read_payload(map, |body| {
        match weak_table::body_of(body.table) {
            // SAFETY: the handle names a live old-space table no other
            // borrow reaches — table access is funneled through the
            // map's payload borrow.
            Some(table) => f(unsafe { &mut *table }),
            None => default,
        }
    })
}

/// Look a key up without borrowing the heap mutably.
///
/// The table borrow is already shared; a reader — a prototype or
/// own-property lookup on the key's behalf — has no reason to demand
/// exclusive access to the heap.
pub(crate) fn weak_map_get_shared(
    map: JsWeakMap,
    heap: &otter_gc::GcHeap,
    key: &Value,
) -> Option<Value> {
    let key = weak_collection_key(key, heap).ok()?;
    with_weak_map_table(heap, map, None, |table| {
        table
            .position(&key)
            .map(|position| table.entries()[position].value)
    })
}

/// `WeakMap.prototype.get` — Spec §24.3.3.3.
pub fn weak_map_get(
    map: JsWeakMap,
    heap: &mut otter_gc::GcHeap,
    key: &Value,
) -> Result<Option<Value>, CollectionError> {
    let key = weak_collection_key(key, heap)?;
    Ok(with_weak_map_table(heap, map, None, |table| {
        table
            .position(&key)
            .map(|position| table.entries()[position].value)
    }))
}

/// `WeakMap.prototype.has` — Spec §24.3.3.4.
pub fn weak_map_has(
    map: JsWeakMap,
    heap: &mut otter_gc::GcHeap,
    key: &Value,
) -> Result<bool, CollectionError> {
    let key = weak_collection_key(key, heap)?;
    Ok(with_weak_map_table(heap, map, false, |table| {
        table.position(&key).is_some()
    }))
}

/// Number of weak-map entries currently stored.
#[must_use]
pub fn weak_map_len(map: JsWeakMap, heap: &otter_gc::GcHeap) -> usize {
    with_weak_map_table(heap, map, 0, |table| {
        table
            .entries()
            .iter()
            .filter(|entry| entry.key.is_live_object_key())
            .count()
    })
}

/// `WeakMap.prototype.set` — Spec §24.3.3.5.
pub fn weak_map_set(
    mut map: JsWeakMap,
    heap: &mut otter_gc::GcHeap,
    key: Value,
    value: Value,
) -> Result<(), CollectionError> {
    weak_map_set_with_roots(&mut map, heap, key, value, &mut |_| {})
}

/// `WeakMap.prototype.set` for stack/native-visible VM construction paths.
pub(crate) fn weak_map_set_with_roots(
    map: &mut JsWeakMap,
    heap: &mut otter_gc::GcHeap,
    key: Value,
    value: Value,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), CollectionError> {
    let barrier_value = value;
    let key_root = key;
    let value_root = value;
    let key = weak_collection_key(&key, heap)?;
    let exists = with_weak_map_table(heap, *map, false, |table| table.position(&key).is_some());
    if !exists {
        let target_len = with_weak_map_table(heap, *map, 0, |table| table.len()).saturating_add(1);
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            key_root.trace_value_slots(visitor);
            value_root.trace_value_slots(visitor);
        };
        reserve_weak_map_for_target_len_with_roots(map, heap, target_len, &mut reserve_roots)?;
    }
    // Reservation may have run a moving collection, which relocates the key
    // the weak entry is about to record.
    let key = weak_collection_key(&key_root, heap)?;
    with_weak_map_table(heap, *map, (), |table| {
        if let Some(position) = table.position(&key) {
            table.entries_mut()[position].value = value;
        } else {
            table.push(key, value);
        }
    });
    record_weak_map_write(heap, *map, &barrier_value);
    Ok(())
}

/// `WeakMap.prototype.delete` — Spec §24.3.3.2.
pub fn weak_map_delete(
    map: JsWeakMap,
    heap: &mut otter_gc::GcHeap,
    key: &Value,
) -> Result<bool, CollectionError> {
    let key = weak_collection_key(key, heap)?;
    Ok(with_weak_map_table(heap, map, false, |table| {
        if let Some(position) = table.position(&key) {
            table.swap_remove(position);
            true
        } else {
            false
        }
    }))
}

/// JS `WeakSet` — weakly-held object / unregistered-symbol set.
pub type JsWeakSet = otter_gc::Gc<WeakSetBody>;

#[derive(Default, otter_macros::Pelt)]
#[pelt(tag = WEAK_SET_BODY_TYPE_TAG, ephemeron_via = weak_set_ephemeron_walk)]
/// GC-allocated storage backing every [`JsWeakSet`] handle.
///
/// The entries live in a [`weak_table::WeakTableBody`] this body names
/// by handle, exactly as [`WeakMapBody`] does; the value slot of every
/// entry is `undefined`.
pub struct WeakSetBody {
    table: weak_table::WeakTableHandle<weak_table::SetKind>,
    prototype_override: Option<Value>,
    /// Lazy ordinary own-property bag (see [`MapBody::expando`]).
    expando: Option<crate::object::JsObject>,
}

impl WeakSetBody {
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        if let Some(value) = &self.prototype_override {
            crate::code_liveness::visit_value(value, visitor);
        }
    }
}

fn weak_set_ephemeron_walk(
    body: &mut WeakSetBody,
    visitor: &mut otter_gc::trace::EphemeronVisitor<'_>,
) {
    let Some(table) = weak_table::body_of(body.table) else {
        return;
    };
    // SAFETY: as in `weak_map_ephemeron_walk`.
    let table = unsafe { &mut *table };
    // As in [`weak_map_ephemeron_walk`]: relocation invalidates the hashes.
    table.mark_stale();
    for entry in table.entries_mut() {
        if let WeakCollectionKey::Object(raw) = &mut entry.key {
            let key_slot = raw as *mut RawGc;
            let mut visit_value_slots = |_slot_visitor: &mut SlotVisitor<'_>| {};
            visitor(key_slot, &mut visit_value_slots);
        }
    }
}

/// Run `f` over the set's table, or return `default` when the set has
/// never grown one. See [`with_weak_map_table`].
fn with_weak_set_table<R>(
    heap: &otter_gc::GcHeap,
    set: JsWeakSet,
    default: R,
    f: impl FnOnce(&mut weak_table::WeakTableBody<weak_table::SetKind>) -> R,
) -> R {
    heap.read_payload(set, |body| {
        match weak_table::body_of(body.table) {
            // SAFETY: as in `with_weak_map_table`.
            Some(table) => f(unsafe { &mut *table }),
            None => default,
        }
    })
}

/// Allocate a fresh empty `WeakSet`.
pub fn alloc_weak_set(heap: &mut otter_gc::GcHeap) -> Result<JsWeakSet, otter_gc::OutOfMemory> {
    let set = heap.alloc_old(WeakSetBody::default())?;
    heap.register_ephemeron_table(set);
    Ok(set)
}

/// Allocate a fresh empty `WeakSet` while exposing caller-owned roots.
pub(crate) fn alloc_weak_set_with_roots(
    heap: &mut otter_gc::GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsWeakSet, otter_gc::OutOfMemory> {
    let set = heap.alloc_with_roots(WeakSetBody::default(), external_visit)?;
    heap.register_ephemeron_table(set);
    Ok(set)
}

pub(crate) fn weak_set_prototype_override(
    set: JsWeakSet,
    heap: &otter_gc::GcHeap,
) -> Option<Value> {
    heap.read_payload(set, |body| body.prototype_override)
}

pub(crate) fn set_weak_set_prototype_override(
    set: JsWeakSet,
    heap: &mut otter_gc::GcHeap,
    proto: Option<Value>,
) {
    let barrier_value = proto;
    heap.with_payload(set, |body| {
        body.prototype_override = proto;
    });
    if let Some(value) = &barrier_value {
        heap.record_write(set, value);
    }
}

/// `WeakSet.prototype.has` — Spec §24.4.3.4.
pub fn weak_set_has(
    set: JsWeakSet,
    heap: &mut otter_gc::GcHeap,
    value: &Value,
) -> Result<bool, CollectionError> {
    let key = weak_collection_key(value, heap)?;
    Ok(with_weak_set_table(heap, set, false, |table| {
        table.position(&key).is_some()
    }))
}

/// Number of weak-set entries currently stored.
#[must_use]
pub fn weak_set_len(set: JsWeakSet, heap: &otter_gc::GcHeap) -> usize {
    with_weak_set_table(heap, set, 0, |table| {
        table
            .entries()
            .iter()
            .filter(|entry| entry.key.is_live_object_key())
            .count()
    })
}

/// `WeakSet.prototype.add` — Spec §24.4.3.1.
pub fn weak_set_add(
    mut set: JsWeakSet,
    heap: &mut otter_gc::GcHeap,
    value: Value,
) -> Result<(), CollectionError> {
    weak_set_add_with_roots(&mut set, heap, value, &mut |_| {})
}

/// `WeakSet.prototype.add` for stack/native-visible VM construction paths.
pub(crate) fn weak_set_add_with_roots(
    set: &mut JsWeakSet,
    heap: &mut otter_gc::GcHeap,
    value: Value,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), CollectionError> {
    let value_root = value;
    let key = weak_collection_key(&value, heap)?;
    let exists = with_weak_set_table(heap, *set, false, |table| table.position(&key).is_some());
    if !exists {
        let target_len = with_weak_set_table(heap, *set, 0, |table| table.len()).saturating_add(1);
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            value_root.trace_value_slots(visitor);
        };
        reserve_weak_set_for_target_len_with_roots(set, heap, target_len, &mut reserve_roots)?;
    }
    // Reservation may have run a moving collection, which relocates the
    // key the weak entry is about to record.
    let key = weak_collection_key(&value_root, heap)?;
    with_weak_set_table(heap, *set, (), |table| {
        if table.position(&key).is_none() {
            table.push(key, Value::undefined());
        }
    });
    Ok(())
}

/// `WeakSet.prototype.delete` — Spec §24.4.3.3.
pub fn weak_set_delete(
    set: JsWeakSet,
    heap: &mut otter_gc::GcHeap,
    value: &Value,
) -> Result<bool, CollectionError> {
    let key = weak_collection_key(value, heap)?;
    Ok(with_weak_set_table(heap, set, false, |table| {
        if let Some(position) = table.position(&key) {
            table.swap_remove(position);
            true
        } else {
            false
        }
    }))
}

/// Run the WeakMap / WeakSet ephemeron fixpoint after ordinary mark.
pub fn run_ephemeron_fixpoint(heap: &mut otter_gc::GcHeap) {
    loop {
        let mut additions = Vec::new();
        for raw in heap.ephemeron_tables_snapshot() {
            if !heap.is_marked(raw) {
                continue;
            }
            if heap.raw_type_tag(raw) == Some(WEAK_MAP_BODY_TYPE_TAG) {
                let Some(map) = heap.cast_raw_if_type::<WeakMapBody>(raw) else {
                    continue;
                };
                with_weak_map_table(heap, map, (), |table| {
                    for entry in table.entries() {
                        match entry.key {
                            WeakCollectionKey::Object(raw) if heap.is_marked(raw) => {
                                if let Some(value_raw) = entry.value.as_raw_gc() {
                                    additions.push(value_raw);
                                }
                            }
                            WeakCollectionKey::Symbol(_) => {
                                if let Some(value_raw) = entry.value.as_raw_gc() {
                                    additions.push(value_raw);
                                }
                            }
                            _ => {}
                        }
                    }
                });
            }
        }
        if !heap.mark_additional(additions) {
            break;
        }
    }

    // Prune dead-key entries.
    for raw in heap.ephemeron_tables_snapshot() {
        if !heap.is_marked(raw) {
            continue;
        }
        let keep = |key: &WeakCollectionKey| match *key {
            WeakCollectionKey::Object(raw) => !raw.is_null() && heap.is_marked(raw),
            WeakCollectionKey::Symbol(_) => true,
        };
        match heap.raw_type_tag(raw) {
            Some(WEAK_MAP_BODY_TYPE_TAG) => {
                let Some(map) = heap.cast_raw_if_type::<WeakMapBody>(raw) else {
                    continue;
                };
                with_weak_map_table(heap, map, (), |table| {
                    table.retain(|entry| keep(&entry.key));
                });
            }
            Some(WEAK_SET_BODY_TYPE_TAG) => {
                let Some(set) = heap.cast_raw_if_type::<WeakSetBody>(raw) else {
                    continue;
                };
                with_weak_set_table(heap, set, (), |table| {
                    table.retain(|entry| keep(&entry.key));
                });
            }
            _ => {}
        }
    }
}

/// Project a value accepted by `CanBeHeldWeakly` to a weak collection key.
fn weak_collection_key(
    value: &Value,
    heap: &otter_gc::GcHeap,
) -> Result<WeakCollectionKey, CollectionError> {
    // §6.1.7.4 CanBeHeldWeakly — check Symbol first: a Symbol is also
    // GC-backed (as_raw_gc would match it), but a registered
    // (Symbol.for) symbol cannot be held weakly and must be rejected.
    if let Some(symbol) = value.as_symbol(heap) {
        if symbol.is_registered() {
            return Err(CollectionError::NonObjectKey);
        }
        return Ok(WeakCollectionKey::Symbol(symbol));
    }
    // Only genuine Objects can be held weakly. `as_raw_gc` also matches
    // GC-backed primitives (String / BigInt), so gate on the positive
    // object-type predicate first.
    if value.is_object_type()
        && let Some(raw) = value.as_raw_gc()
    {
        return Ok(WeakCollectionKey::Object(raw));
    }
    Err(CollectionError::NonObjectKey)
}

impl crate::pelt::PeltField for MapKey {
    fn pelt_trace(&mut self, visitor: &mut SlotVisitor<'_>) {
        match self {
            // The object key holds a live `Value` slot the collector rewrites.
            MapKey::ObjectValue(value) => value.trace_value_slot_mut(visitor),
            // A string key's body handle moves under a young-gen scavenge, so
            // its slot must be traced too (the equality path reads the body).
            MapKey::String(s) => s.trace_handle_slot(visitor),
            MapKey::Undefined
            | MapKey::Null
            | MapKey::Boolean(_)
            | MapKey::Number(_)
            | MapKey::BigInt(_)
            | MapKey::Symbol(_) => {}
        }
    }
}

/// Remember a write against the table that actually holds the entry.
///
/// A collection's entries live in a separate old-space table, so the map
/// is not the parent of its own keys and values. Remembering only the map
/// would leave the scavenger re-tracing a body whose sole outgoing edge
/// is the table handle — and it stops there, because the table is old.
/// The map is remembered too: the same call sites also write the expando
/// and prototype override, which the map does own.
fn record_map_write<V>(heap: &mut otter_gc::GcHeap, map: JsMap, value: &V)
where
    V: otter_gc::GcStore + ?Sized,
{
    heap.record_write(map, value);
    let table = heap.read_payload(map, |body| body.table);
    if !table.is_null() {
        heap.record_write(table, value);
    }
}

/// Remember a write against the table that holds the set's entries. See
/// [`record_map_write`].
fn record_set_write<V>(heap: &mut otter_gc::GcHeap, set: JsSet, value: &V)
where
    V: otter_gc::GcStore + ?Sized,
{
    heap.record_write(set, value);
    let table = heap.read_payload(set, |body| body.table);
    if !table.is_null() {
        heap.record_write(table, value);
    }
}

/// Make room for `target_len` entries, growing the table if the current
/// one is too small.
///
/// Growth is the collection's one allocation point on the insert path and
/// is deliberately separate from the insert: the insert runs inside a
/// payload borrow with no heap available, and allocating there could move
/// the map being written. So callers reserve first — which roots the map
/// across the allocation — and then mutate.
///
/// Capacity doubles, so a run of inserts pays for growth a logarithmic
/// number of times. The replacement table rebuilds its chains as it takes
/// the old entries in order, which keeps every index a live iterator is
/// holding valid.
fn reserve_map_for_target_len_with_roots(
    map: &mut JsMap,
    heap: &mut otter_gc::GcHeap,
    target_len: usize,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    let capacity = heap.read_payload(*map, |body| body.entry_capacity());
    if target_len <= capacity {
        return Ok(());
    }
    let grown = target_len.max(capacity.saturating_mul(2)).max(4);
    let owner_slot = std::ptr::addr_of_mut!(*map);
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        visitor(owner_slot.cast::<RawGc>());
    };
    let table = table::alloc_table::<MapEntry>(heap, grown, &mut visit)?;
    let owner = *map;
    heap.with_payload(owner, |body| {
        let carried: Vec<MapEntry> = body.entries().to_vec();
        // SAFETY: the handle names the table just allocated, which no
        // other borrow reaches.
        unsafe { (*table::body_of(table).expect("fresh table")).refill_from(&carried) };
        body.table = table;
        true
    });
    // The table handle was installed by a raw payload write, and the
    // entries were copied in behind the mutator's back, so record both
    // edges the barrier would have.
    heap.record_write(owner, &table);
    record_map_table_contents(heap, table);
    Ok(())
}

/// Remember every value a freshly filled map table holds.
///
/// The entries are the table's own children now, not the map's, so a
/// scavenge that only re-traces the map would stop at an old child and
/// leave every young key and value in the table unmarked.
fn record_map_table_contents(heap: &mut otter_gc::GcHeap, table: table::TableHandle<MapEntry>) {
    let Some(body) = table::body_of(table) else {
        return;
    };
    // SAFETY: the handle names a live table payload.
    let entries: Vec<MapEntry> = unsafe { (*body).entries().to_vec() };
    for entry in entries {
        if entry.is_live() {
            heap.record_write(table, &entry.key);
            heap.record_write(table, &entry.value);
        }
    }
}

/// Make room for `target_len` entries. See
/// [`reserve_map_for_target_len_with_roots`].
fn reserve_set_for_target_len_with_roots(
    set: &mut JsSet,
    heap: &mut otter_gc::GcHeap,
    target_len: usize,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    let capacity = heap.read_payload(*set, |body| body.entry_capacity());
    if target_len <= capacity {
        return Ok(());
    }
    let grown = target_len.max(capacity.saturating_mul(2)).max(4);
    let owner_slot = std::ptr::addr_of_mut!(*set);
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        visitor(owner_slot.cast::<RawGc>());
    };
    let table = table::alloc_table::<SetEntry>(heap, grown, &mut visit)?;
    let owner = *set;
    heap.with_payload(owner, |body| {
        let carried: Vec<SetEntry> = body.entries().to_vec();
        // SAFETY: as in the map path.
        unsafe { (*table::body_of(table).expect("fresh table")).refill_from(&carried) };
        body.table = table;
        true
    });
    heap.record_write(owner, &table);
    record_set_table_contents(heap, table);
    Ok(())
}

/// Remember every value a freshly filled set table holds.
fn record_set_table_contents(heap: &mut otter_gc::GcHeap, table: table::TableHandle<SetEntry>) {
    let Some(body) = table::body_of(table) else {
        return;
    };
    // SAFETY: the handle names a live table payload.
    let entries: Vec<SetEntry> = unsafe { (*body).entries().to_vec() };
    for entry in entries {
        if let Some(value) = entry.value {
            heap.record_write(table, &value);
        }
        if let Some(key_hash) = entry.key_hash {
            key_hash.record_into(heap, table);
        }
    }
}

/// Make room for `target_len` entries by allocating a larger table and
/// carrying the entries over. The entries are ephemerons, so no strong
/// barrier pass follows the copy: every scavenge walks every registered
/// collection's entries through the `ephemeron_via` hook regardless of
/// remembered sets.
fn reserve_weak_map_for_target_len_with_roots(
    map: &mut JsWeakMap,
    heap: &mut otter_gc::GcHeap,
    target_len: usize,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    let capacity = with_weak_map_table(heap, *map, 0, |table| table.capacity());
    if target_len <= capacity {
        return Ok(());
    }
    let grown = target_len.max(capacity.saturating_mul(2)).max(4);
    let owner_slot = std::ptr::addr_of_mut!(*map);
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        visitor(owner_slot.cast::<RawGc>());
    };
    let table = weak_table::alloc_weak_table::<weak_table::MapKind>(heap, grown, &mut visit)?;
    let owner = *map;
    let carried: Vec<weak_table::WeakEntry> =
        with_weak_map_table(heap, owner, Vec::new(), |old| old.entries().to_vec());
    heap.with_payload(owner, |body| {
        // SAFETY: the handle names the table just allocated, which no
        // other borrow reaches.
        unsafe { (*weak_table::body_of(table).expect("fresh table")).refill_from(&carried) };
        body.table = table;
        true
    });
    heap.record_write(owner, &table);
    Ok(())
}

/// See [`reserve_weak_map_for_target_len_with_roots`].
fn reserve_weak_set_for_target_len_with_roots(
    set: &mut JsWeakSet,
    heap: &mut otter_gc::GcHeap,
    target_len: usize,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    let capacity = with_weak_set_table(heap, *set, 0, |table| table.capacity());
    if target_len <= capacity {
        return Ok(());
    }
    let grown = target_len.max(capacity.saturating_mul(2)).max(4);
    let owner_slot = std::ptr::addr_of_mut!(*set);
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        visitor(owner_slot.cast::<RawGc>());
    };
    let table = weak_table::alloc_weak_table::<weak_table::SetKind>(heap, grown, &mut visit)?;
    let owner = *set;
    let carried: Vec<weak_table::WeakEntry> =
        with_weak_set_table(heap, owner, Vec::new(), |old| old.entries().to_vec());
    heap.with_payload(owner, |body| {
        // SAFETY: as in the map path.
        unsafe { (*weak_table::body_of(table).expect("fresh table")).refill_from(&carried) };
        body.table = table;
        true
    });
    heap.record_write(owner, &table);
    Ok(())
}

/// Remember a value write against the map and its table.
///
/// The value slot lives in the table's own old-space cell; the map is
/// remembered too because the same call sites also write slots it owns.
/// Keys need no barrier: weak entries are reached through the ephemeron
/// registry on every collection, not through remembered sets.
fn record_weak_map_write<V>(heap: &mut otter_gc::GcHeap, map: JsWeakMap, value: &V)
where
    V: otter_gc::GcStore + ?Sized,
{
    heap.record_write(map, value);
    let table = heap.read_payload(map, |body| body.table);
    if !table.is_null() {
        heap.record_write(table, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::number::NumberValue;

    fn n(i: i32) -> Value {
        Value::number(NumberValue::from_i32(i))
    }

    fn young_object_value(heap: &mut otter_gc::GcHeap) -> Value {
        let mut no_roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        Value::object(crate::object::alloc_object_with_roots(heap, &mut no_roots).unwrap())
    }

    #[test]
    fn small_integral_map_keys_do_not_collapse_into_one_bucket() {
        let occupied = (0..64)
            .map(|value| {
                map_key_hash(&MapKey::Number(f64::from(value))).expect("number keys are indexed")
                    & 63
            })
            .collect::<std::collections::BTreeSet<_>>();

        assert!(
            occupied.len() >= 32,
            "64 adjacent integral keys occupied only {} buckets",
            occupied.len()
        );
    }

    #[test]
    fn map_entries_keep_the_compact_generated_layout() {
        assert_eq!(std::mem::size_of::<MapEntry>(), 32);
        assert_eq!(MAP_ENTRY_KEY_OFFSET, 0);
        assert_eq!(MAP_ENTRY_VALUE_OFFSET, 8);
        assert_eq!(MAP_ENTRY_NEXT_OFFSET, 24);
        assert_eq!(MAP_ENTRY_FLAGS_OFFSET, 28);
    }

    #[test]
    fn map_insertion_order_preserved() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let m = alloc_map(&mut heap).unwrap();
        map_set(m, &mut heap, n(1), Value::boolean(true)).unwrap();
        map_set(m, &mut heap, n(2), Value::boolean(false)).unwrap();
        map_set(m, &mut heap, n(1), Value::boolean(false)).unwrap(); // update
        let keys = map_keys(m, &heap);
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].as_number().unwrap().as_smi(), Some(1));
        assert_eq!(keys[1].as_number().unwrap().as_smi(), Some(2));
        assert_eq!(map_get(m, &heap, &n(1)), Some(Value::boolean(false)));
    }

    #[test]
    fn map_string_keys_compare_by_content() {
        // Two distinct GC allocations of the same code units must
        // collide as Map keys (SameValueZero). Regression for the
        // Phase B handle-identity equality bug.
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let m = alloc_map(&mut heap).unwrap();
        let a = crate::string::JsString::from_str("hello", &mut heap).unwrap();
        let b = crate::string::JsString::from_str("hello", &mut heap).unwrap();
        assert_ne!(a.handle(), b.handle(), "test setup: handles must differ");

        map_set(m, &mut heap, Value::string(a), n(1)).unwrap();
        assert!(map_has(m, &heap, &Value::string(b)));
        assert_eq!(map_get(m, &heap, &Value::string(b)), Some(n(1)));

        // Update should hit the existing slot, not append.
        map_set(m, &mut heap, Value::string(b), n(2)).unwrap();
        assert_eq!(map_len(m, &heap), 1);
        assert_eq!(map_get(m, &heap, &Value::string(a)), Some(n(2)));

        // Mismatched content stays distinct.
        let c = crate::string::JsString::from_str("world", &mut heap).unwrap();
        assert!(!map_has(m, &heap, &Value::string(c)));
    }

    #[test]
    fn set_string_keys_compare_by_content() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let s = alloc_set(&mut heap).unwrap();
        let a = crate::string::JsString::from_str("k", &mut heap).unwrap();
        let b = crate::string::JsString::from_str("k", &mut heap).unwrap();
        set_add(s, &mut heap, Value::string(a)).unwrap();
        set_add(s, &mut heap, Value::string(b)).unwrap();
        assert_eq!(set_len(s, &heap), 1);
        assert!(set_has(s, &heap, &Value::string(b)));
    }

    #[test]
    fn map_samevaluezero_zero_collapse() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let m = alloc_map(&mut heap).unwrap();
        map_set(
            m,
            &mut heap,
            Value::number(NumberValue::from_f64(-0.0)),
            n(7),
        )
        .unwrap();
        let v = map_get(m, &heap, &Value::number(NumberValue::from_f64(0.0)));
        assert_eq!(v, Some(n(7)));
    }

    #[test]
    fn map_samevaluezero_nan_matches() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let m = alloc_map(&mut heap).unwrap();
        map_set(
            m,
            &mut heap,
            Value::number(NumberValue::from_f64(f64::NAN)),
            n(9),
        )
        .unwrap();
        let v = map_get(m, &heap, &Value::number(NumberValue::from_f64(f64::NAN)));
        assert_eq!(v, Some(n(9)));
    }

    #[test]
    fn set_dedupes() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let s = alloc_set(&mut heap).unwrap();
        set_add(s, &mut heap, n(1)).unwrap();
        set_add(s, &mut heap, n(1)).unwrap();
        set_add(s, &mut heap, n(2)).unwrap();
        assert_eq!(set_len(s, &heap), 2);
    }

    #[test]
    fn readonly_set_ignores_every_mutator() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let s = alloc_set(&mut heap).unwrap();
        set_add(s, &mut heap, n(1)).unwrap();
        set_make_readonly(s, &mut heap);
        set_add(s, &mut heap, n(2)).unwrap();
        assert!(!set_delete(s, &mut heap, &n(1)));
        set_clear(s, &mut heap);
        assert!(set_is_readonly(s, &heap));
        assert_eq!(set_values(s, &heap), vec![n(1)]);
    }

    #[test]
    fn map_object_key_survives_minor_relocation() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let mut m = alloc_map(&mut heap).unwrap();
        let mut key = young_object_value(&mut heap);
        let before = key.as_raw_gc().unwrap();

        map_set(m, &mut heap, key, n(42)).unwrap();

        let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visitor(std::ptr::addr_of_mut!(m) as *mut RawGc);
            // Root the live local `key` through a real mutable raw pointer to
            // the local itself. The scavenger rewrites this slot in place to the
            // relocated address; deriving the slot from a shared `&self`
            // (`key.trace_value_slots`) is UB the release optimizer exploits by
            // assuming `key` is unchanged across the collection.
            visitor(std::ptr::addr_of_mut!(key) as *mut RawGc);
        };
        heap.collect_minor_with_roots(&mut roots).expect("minor GC");

        let after = key.as_raw_gc().unwrap();
        assert_ne!(after, before);
        assert!(map_has(m, &heap, &key));
        assert_eq!(map_get(m, &heap, &key), Some(n(42)));
        assert_eq!(map_keys(m, &heap), vec![key]);
    }

    #[test]
    fn set_object_key_survives_minor_relocation() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let mut s = alloc_set(&mut heap).unwrap();
        let mut key = young_object_value(&mut heap);
        let before = key.as_raw_gc().unwrap();

        set_add(s, &mut heap, key).unwrap();

        let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visitor(std::ptr::addr_of_mut!(s) as *mut RawGc);
            visitor(std::ptr::addr_of_mut!(key) as *mut RawGc);
        };
        heap.collect_minor_with_roots(&mut roots).expect("minor GC");

        let after = key.as_raw_gc().unwrap();
        assert_ne!(after, before);
        assert!(set_has(s, &heap, &key));
        assert_eq!(set_values(s, &heap), vec![key]);
    }

    #[test]
    fn weakmap_rejects_primitive_keys() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let wm = alloc_weak_map(&mut heap).unwrap();
        let err = weak_map_set(wm, &mut heap, n(1), Value::boolean(true)).unwrap_err();
        assert!(matches!(err, CollectionError::NonObjectKey));
    }

    #[test]
    fn weakmap_object_key_roundtrips() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let wm = alloc_weak_map(&mut heap).unwrap();
        let obj = Value::object(crate::object::alloc_object_old_for_fixture(&mut heap).unwrap());
        weak_map_set(wm, &mut heap, obj, n(42)).unwrap();
        assert!(weak_map_has(wm, &mut heap, &obj).unwrap());
        assert_eq!(weak_map_get(wm, &mut heap, &obj).unwrap(), Some(n(42)));
        let other = Value::object(crate::object::alloc_object_old_for_fixture(&mut heap).unwrap());
        assert!(!weak_map_has(wm, &mut heap, &other).unwrap());
    }

    #[test]
    fn weakmap_young_key_and_value_survive_minor_relocation_when_key_rooted() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let mut wm = alloc_weak_map(&mut heap).unwrap();
        let mut key = young_object_value(&mut heap);
        let value = young_object_value(&mut heap);
        let key_before = key.as_raw_gc().unwrap();
        let value_before = value.as_raw_gc().unwrap();

        weak_map_set(wm, &mut heap, key, value).unwrap();

        let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visitor(std::ptr::addr_of_mut!(wm) as *mut RawGc);
            visitor(std::ptr::addr_of_mut!(key) as *mut RawGc);
        };
        heap.collect_minor_with_roots(&mut roots).expect("minor GC");

        let key_after = key.as_raw_gc().unwrap();
        let value_after = weak_map_get(wm, &mut heap, &key)
            .unwrap()
            .and_then(|value| value.as_raw_gc())
            .unwrap();
        assert_ne!(key_after, key_before);
        assert_ne!(value_after, value_before);
        assert!(weak_map_has(wm, &mut heap, &key).unwrap());
    }

    #[test]
    fn weakmap_dead_young_key_is_not_observable_after_minor_gc() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let mut wm = alloc_weak_map(&mut heap).unwrap();
        let key = young_object_value(&mut heap);
        let value = young_object_value(&mut heap);

        weak_map_set(wm, &mut heap, key, value).unwrap();

        let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visitor(std::ptr::addr_of_mut!(wm) as *mut RawGc);
        };
        heap.collect_minor_with_roots(&mut roots).expect("minor GC");

        assert_eq!(weak_map_len(wm, &heap), 0);
    }

    #[test]
    fn weakset_young_key_survives_minor_relocation_when_rooted() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let mut ws = alloc_weak_set(&mut heap).unwrap();
        let mut key = young_object_value(&mut heap);
        let before = key.as_raw_gc().unwrap();

        weak_set_add(ws, &mut heap, key).unwrap();

        let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visitor(std::ptr::addr_of_mut!(ws) as *mut RawGc);
            visitor(std::ptr::addr_of_mut!(key) as *mut RawGc);
        };
        heap.collect_minor_with_roots(&mut roots).expect("minor GC");

        let after = key.as_raw_gc().unwrap();
        assert_ne!(after, before);
        assert!(weak_set_has(ws, &mut heap, &key).unwrap());
    }

    #[test]
    fn map_string_keys() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let m = alloc_map(&mut gc_heap).unwrap();
        let key = Value::string(JsString::from_str("k", &mut gc_heap).unwrap());
        map_set(m, &mut gc_heap, key, n(1)).unwrap();
        assert_eq!(map_get(m, &gc_heap, &key), Some(n(1)),);
    }

    /// Growth carries every entry across table generations, swap-remove
    /// keeps the survivors findable through re-threaded chains, and a
    /// minor collection in the middle (which relocates the addresses
    /// every identity hash derives from) does not lose an entry.
    #[test]
    fn weakmap_survives_growth_deletion_and_relocation() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let mut wm = alloc_weak_map(&mut heap).unwrap();
        let mut keys: Vec<Value> = Vec::new();
        for i in 0..64 {
            let key =
                Value::object(crate::object::alloc_object_old_for_fixture(&mut heap).unwrap());
            weak_map_set(wm, &mut heap, key, n(i)).unwrap();
            keys.push(key);
        }
        assert_eq!(weak_map_len(wm, &heap), 64);
        // Delete every other key; swap-remove reorders and staleness must
        // not lose the survivors.
        for key in keys.iter().step_by(2) {
            assert!(weak_map_delete(wm, &mut heap, key).unwrap());
        }
        assert_eq!(weak_map_len(wm, &heap), 32);
        // Relocation invalidates the identity hashes behind the chains.
        let keys_base = keys.as_mut_ptr();
        let keys_len = keys.len();
        let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visitor(std::ptr::addr_of_mut!(wm) as *mut RawGc);
            for index in 0..keys_len {
                // SAFETY: `index < keys_len`, and the vec outlives the walk.
                unsafe { (*keys_base.add(index)).trace_value_slot_mut(visitor) };
            }
        };
        heap.collect_minor_with_roots(&mut roots).expect("minor GC");
        for (i, key) in keys.iter().enumerate() {
            let got = weak_map_get(wm, &mut heap, key).unwrap();
            if i % 2 == 0 {
                assert_eq!(got, None, "deleted key {i} resurfaced");
            } else {
                assert_eq!(got, Some(n(i as i32)), "surviving key {i} lost");
            }
        }
    }

    /// The set variant of the growth / deletion / relocation walk.
    #[test]
    fn weakset_survives_growth_deletion_and_relocation() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let mut ws = alloc_weak_set(&mut heap).unwrap();
        let mut keys: Vec<Value> = Vec::new();
        for _ in 0..64 {
            let key =
                Value::object(crate::object::alloc_object_old_for_fixture(&mut heap).unwrap());
            weak_set_add(ws, &mut heap, key).unwrap();
            keys.push(key);
        }
        assert_eq!(weak_set_len(ws, &heap), 64);
        for key in keys.iter().step_by(2) {
            assert!(weak_set_delete(ws, &mut heap, key).unwrap());
        }
        assert_eq!(weak_set_len(ws, &heap), 32);
        let keys_base = keys.as_mut_ptr();
        let keys_len = keys.len();
        let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visitor(std::ptr::addr_of_mut!(ws) as *mut RawGc);
            for index in 0..keys_len {
                // SAFETY: `index < keys_len`, and the vec outlives the walk.
                unsafe { (*keys_base.add(index)).trace_value_slot_mut(visitor) };
            }
        };
        heap.collect_minor_with_roots(&mut roots).expect("minor GC");
        for (i, key) in keys.iter().enumerate() {
            let has = weak_set_has(ws, &mut heap, key).unwrap();
            assert_eq!(has, i % 2 != 0, "wrong membership for key {i}");
        }
    }
}
