//! `Map`, `Set`, `WeakMap`, `WeakSet` collection value types.
//!
//! `Map` and `Set` preserve insertion order (ECMA-262 §24.1 /
//! §24.2). `WeakMap` and `WeakSet` are object-keyed and keep
//! entries through ephemeron tables: keys are weak, and values
//! are marked only when their key is already reachable through
//! another path.
//!
//! # Contents
//! - [`JsMap`] — heap-shared, tombstone-list associative store.
//! - [`JsSet`] — heap-shared, tombstone-list unique-element store.
//! - [`JsWeakMap`] — GC-managed object-keyed weak map.
//! - [`JsWeakSet`] — GC-managed object-keyed weak set.
//! - [`MapKey`] — equality key used by `JsMap`/`JsSet`. Implements
//!   ECMA-262 SameValueZero so `+0` / `-0` collapse and `NaN`
//!   matches itself.
//!
//! # Invariants
//! - `JsMap::set` / `JsSet::add` preserve insertion order; updating
//!   an existing key does not change its position.
//! - Two `JsMap` handles cloned from the same heap object share
//!   storage — both observe subsequent mutations.
//! - `JsWeakMap` / `JsWeakSet` reject non-object keys with
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
    /// Symbols compare by `Rc::ptr_eq` identity.
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
    pub fn from_value(value: &Value) -> Self {
        match value {
            Value::Undefined => MapKey::Undefined,
            Value::Null => MapKey::Null,
            Value::Boolean(b) => MapKey::Boolean(*b),
            Value::Number(n) => {
                let f = n.as_f64();
                // SameValueZero: collapse −0 to +0; preserve NaN
                // bits — equality below treats any NaN as equal.
                let normalised = if f == 0.0 { 0.0 } else { f };
                MapKey::Number(normalised)
            }
            Value::BigInt(b) => MapKey::BigInt(b.clone()),
            Value::String(s) => MapKey::String(s.clone()),
            Value::Symbol(s) => MapKey::Symbol(s.clone()),
            Value::Object(_)
            | Value::Array(_)
            | Value::RegExp(_)
            | Value::Promise(_)
            | Value::Iterator(_)
            | Value::Generator(_)
            | Value::BoundFunction(_)
            | Value::NativeFunction(_) => MapKey::ObjectValue(value.clone()),
            // Functions, closures, class constructors, and other reference
            // wrappers compare via the originating `Value`'s `PartialEq`,
            // which is identity on every callable shape.
            _ => MapKey::ObjectValue(value.clone()),
        }
    }
}

impl PartialEq for MapKey {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (MapKey::Undefined, MapKey::Undefined) => true,
            (MapKey::Null, MapKey::Null) => true,
            (MapKey::Boolean(a), MapKey::Boolean(b)) => a == b,
            // SameValueZero on numbers: `NaN == NaN`, sign-only
            // differences on zero already normalised in `from_value`.
            (MapKey::Number(a), MapKey::Number(b)) => {
                if a.is_nan() && b.is_nan() {
                    true
                } else {
                    a == b
                }
            }
            (MapKey::BigInt(a), MapKey::BigInt(b)) => a == b,
            (MapKey::String(a), MapKey::String(b)) => a.equals(b),
            (MapKey::Symbol(a), MapKey::Symbol(b)) => a.ptr_eq(b),
            (MapKey::ObjectValue(a), MapKey::ObjectValue(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for MapKey {}

impl std::hash::Hash for MapKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            MapKey::Undefined | MapKey::Null => {}
            MapKey::Boolean(b) => b.hash(state),
            MapKey::Number(f) => {
                // Canonicalise NaN bits so distinct NaN payloads
                // hash identically (matching SameValueZero).
                if f.is_nan() {
                    f64::NAN.to_bits().hash(state);
                } else {
                    f.to_bits().hash(state);
                }
            }
            MapKey::BigInt(b) => b.to_decimal_string().hash(state),
            MapKey::String(s) => s.to_lossy_string().hash(state),
            MapKey::Symbol(s) => s.identity_addr().hash(state),
            MapKey::ObjectValue(_) => {
                // Identity-based fallback: hash by discriminant alone
                // and rely on `eq` to disambiguate. This intentionally avoids
                // hashing moving heap addresses.
            }
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
    /// `WeakMap` / `WeakSet` rejects primitive keys.
    #[error("WeakMap / WeakSet keys must be objects")]
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

#[derive(Debug, Default)]
/// GC-allocated storage backing every [`JsMap`] handle.
pub struct MapBody {
    entries: Vec<MapEntry>,
    prototype_override: Option<Value>,
}

#[derive(Debug)]
struct MapEntry {
    key_hash: Option<MapKey>,
    key: Option<Value>,
    value: Option<Value>,
}

impl MapEntry {
    fn live(key_hash: MapKey, key: Value, value: Value) -> Self {
        Self {
            key_hash: Some(key_hash),
            key: Some(key),
            value: Some(value),
        }
    }

    fn key_matches(&self, key: &MapKey) -> bool {
        self.value.is_some() && self.key_hash.as_ref().is_some_and(|stored| stored == key)
    }

    fn pair(&self) -> Option<(Value, Value)> {
        Some((self.key.as_ref()?.clone(), self.value.as_ref()?.clone()))
    }

    fn clear(&mut self) {
        self.key_hash = None;
        self.key = None;
        self.value = None;
    }
}

impl otter_gc::SafeTraceable for MapBody {
    const TYPE_TAG: u8 = MAP_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        for entry in &self.entries {
            if let Some(key_hash) = &entry.key_hash {
                trace_map_key(key_hash, visitor);
            }
            if let Some(key) = &entry.key {
                key.trace_value_slots(visitor);
            }
            if let Some(value) = &entry.value {
                value.trace_value_slots(visitor);
            }
        }
        if let Some(proto) = &self.prototype_override {
            proto.trace_value_slots(visitor);
        }
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
    heap.read_payload(map, |body| body.prototype_override.clone())
}

pub(crate) fn set_map_prototype_override(
    map: JsMap,
    heap: &mut otter_gc::GcHeap,
    proto: Option<Value>,
) {
    let barrier_value = proto.clone();
    heap.with_payload(map, |body| {
        body.prototype_override = proto;
    });
    if let Some(value) = &barrier_value {
        heap.record_write(map, value);
    }
}

/// Number of entries.
#[must_use]
pub fn map_len(map: JsMap, heap: &otter_gc::GcHeap) -> usize {
    heap.read_payload(map, |body| {
        body.entries
            .iter()
            .filter(|entry| entry.value.is_some())
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
    let k = MapKey::from_value(key);
    heap.read_payload(map, |body| {
        body.entries
            .iter()
            .find(|entry| entry.key_matches(&k))
            .and_then(|entry| entry.value.clone())
    })
}

/// `Map.prototype.has` — Spec §24.1.3.7.
#[must_use]
pub fn map_has(map: JsMap, heap: &otter_gc::GcHeap, key: &Value) -> bool {
    let k = MapKey::from_value(key);
    heap.read_payload(map, |body| {
        body.entries.iter().any(|entry| entry.key_matches(&k))
    })
}

/// `Map.prototype.set` — Spec §24.1.3.9. Updates in place
/// without changing insertion order; new keys append.
pub fn map_set(
    mut map: JsMap,
    heap: &mut otter_gc::GcHeap,
    key: Value,
    value: Value,
) -> Result<(), otter_gc::OutOfMemory> {
    let barrier_key = key.clone();
    let barrier_value = value.clone();
    let k = MapKey::from_value(&key);
    let exists = heap.read_payload(map, |body| {
        body.entries.iter().any(|entry| entry.key_matches(&k))
    });
    if !exists {
        let target_len = heap.read_payload(map, |body| body.entries.len().saturating_add(1));
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            key.trace_value_slots(visitor);
            value.trace_value_slots(visitor);
        };
        reserve_map_for_target_len_with_roots(&mut map, heap, target_len, &mut reserve_roots)?;
    }
    let k = MapKey::from_value(&key);
    heap.with_payload(map, |body| {
        if let Some(slot) = body.entries.iter_mut().find(|entry| entry.key_matches(&k)) {
            slot.value = Some(value);
        } else {
            body.entries.push(MapEntry::live(k, key, value));
        }
    });
    if !exists {
        heap.record_write(map, &barrier_key);
    }
    heap.record_write(map, &barrier_value);
    Ok(())
}

/// `Map.prototype.set` for stack-visible VM construction paths.
pub(crate) fn map_set_with_roots(
    map: &mut JsMap,
    heap: &mut otter_gc::GcHeap,
    key: Value,
    value: Value,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    let barrier_key = key.clone();
    let barrier_value = value.clone();
    let k = MapKey::from_value(&key);
    let exists = heap.read_payload(*map, |body| {
        body.entries.iter().any(|entry| entry.key_matches(&k))
    });
    if !exists {
        let target_len = heap.read_payload(*map, |body| body.entries.len().saturating_add(1));
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            key.trace_value_slots(visitor);
            value.trace_value_slots(visitor);
        };
        reserve_map_for_target_len_with_roots(map, heap, target_len, &mut reserve_roots)?;
    }
    heap.with_payload(*map, |body| {
        if let Some(slot) = body.entries.iter_mut().find(|entry| entry.key_matches(&k)) {
            slot.value = Some(value);
        } else {
            body.entries.push(MapEntry::live(k, key, value));
        }
    });
    if !exists {
        heap.record_write(*map, &barrier_key);
    }
    heap.record_write(*map, &barrier_value);
    Ok(())
}

/// `Map.prototype.delete` — Spec §24.1.3.3. Returns `true` when
/// the entry existed.
pub fn map_delete(map: JsMap, heap: &mut otter_gc::GcHeap, key: &Value) -> bool {
    let k = MapKey::from_value(key);
    heap.with_payload(map, |body| {
        if let Some(entry) = body.entries.iter_mut().find(|entry| entry.key_matches(&k)) {
            entry.clear();
            true
        } else {
            false
        }
    })
}

/// `Map.prototype.clear` — Spec §24.1.3.2.
pub fn map_clear(map: JsMap, heap: &mut otter_gc::GcHeap) {
    heap.with_payload(map, |body| {
        for entry in &mut body.entries {
            entry.clear();
        }
    });
}

/// Snapshot key list (in insertion order).
#[must_use]
pub fn map_keys(map: JsMap, heap: &otter_gc::GcHeap) -> Vec<Value> {
    heap.read_payload(map, |body| {
        body.entries
            .iter()
            .filter_map(|entry| entry.key.clone())
            .collect()
    })
}

/// Snapshot value list (in insertion order).
#[must_use]
pub fn map_values(map: JsMap, heap: &otter_gc::GcHeap) -> Vec<Value> {
    heap.read_payload(map, |body| {
        body.entries
            .iter()
            .filter_map(|entry| entry.value.clone())
            .collect()
    })
}

/// Snapshot entry list.
#[must_use]
pub fn map_entries(map: JsMap, heap: &otter_gc::GcHeap) -> Vec<(Value, Value)> {
    heap.read_payload(map, |body| {
        body.entries.iter().filter_map(MapEntry::pair).collect()
    })
}

/// Raw backing-list length, including deleted tombstone slots.
#[must_use]
pub(crate) fn map_raw_len(map: JsMap, heap: &otter_gc::GcHeap) -> usize {
    heap.read_payload(map, |body| body.entries.len())
}

/// Read the raw entry currently at `index` in insertion order.
#[must_use]
pub(crate) fn map_entry_at(
    map: JsMap,
    heap: &otter_gc::GcHeap,
    index: usize,
) -> Option<(Value, Value)> {
    heap.read_payload(map, |body| body.entries.get(index).and_then(MapEntry::pair))
}

/// Identity comparison.
#[must_use]
pub fn map_ptr_eq(a: JsMap, b: JsMap) -> bool {
    a == b
}

/// JS `Set` — ordered unique-element store.
pub type JsSet = otter_gc::Gc<SetBody>;

#[derive(Debug, Default)]
/// GC-allocated storage backing every [`JsSet`] handle.
pub struct SetBody {
    /// Insertion-ordered `[[SetData]]` list. Deleted entries become
    /// tombstones so active iterators and `forEach` observe later
    /// additions before exhaustion.
    entries: Vec<SetEntry>,
    prototype_override: Option<Value>,
}

#[derive(Debug)]
struct SetEntry {
    key_hash: Option<MapKey>,
    value: Option<Value>,
}

impl SetEntry {
    fn live(key_hash: MapKey, value: Value) -> Self {
        Self {
            key_hash: Some(key_hash),
            value: Some(value),
        }
    }

    fn key_matches(&self, key: &MapKey) -> bool {
        self.value.is_some() && self.key_hash.as_ref().is_some_and(|stored| stored == key)
    }

    fn clear(&mut self) {
        self.key_hash = None;
        self.value = None;
    }
}

impl otter_gc::SafeTraceable for SetBody {
    const TYPE_TAG: u8 = SET_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        for entry in &self.entries {
            if let Some(key_hash) = &entry.key_hash {
                trace_map_key(key_hash, visitor);
            }
            if let Some(value) = &entry.value {
                value.trace_value_slots(visitor);
            }
        }
        if let Some(proto) = &self.prototype_override {
            proto.trace_value_slots(visitor);
        }
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
    heap.read_payload(set, |body| body.prototype_override.clone())
}

pub(crate) fn set_set_prototype_override(
    set: JsSet,
    heap: &mut otter_gc::GcHeap,
    proto: Option<Value>,
) {
    let barrier_value = proto.clone();
    heap.with_payload(set, |body| {
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
        body.entries
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
    let k = MapKey::from_value(value);
    heap.read_payload(set, |body| {
        body.entries.iter().any(|entry| entry.key_matches(&k))
    })
}

/// `Set.prototype.add` — Spec §24.2.3.1.
pub fn set_add(
    mut set: JsSet,
    heap: &mut otter_gc::GcHeap,
    value: Value,
) -> Result<(), otter_gc::OutOfMemory> {
    let barrier_value = value.clone();
    let k = MapKey::from_value(&value);
    let exists = heap.read_payload(set, |body| {
        body.entries.iter().any(|entry| entry.key_matches(&k))
    });
    if !exists {
        let target_len = heap.read_payload(set, |body| body.entries.len().saturating_add(1));
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            value.trace_value_slots(visitor);
        };
        reserve_set_for_target_len_with_roots(&mut set, heap, target_len, &mut reserve_roots)?;
    }
    let k = MapKey::from_value(&value);
    heap.with_payload(set, |body| {
        if !body.entries.iter().any(|entry| entry.key_matches(&k)) {
            body.entries.push(SetEntry::live(k, value));
        }
    });
    if !exists {
        heap.record_write(set, &barrier_value);
    }
    Ok(())
}

/// `Set.prototype.add` for stack-visible VM construction paths.
pub(crate) fn set_add_with_roots(
    set: &mut JsSet,
    heap: &mut otter_gc::GcHeap,
    value: Value,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    let barrier_value = value.clone();
    let k = MapKey::from_value(&value);
    let exists = heap.read_payload(*set, |body| {
        body.entries.iter().any(|entry| entry.key_matches(&k))
    });
    if !exists {
        let target_len = heap.read_payload(*set, |body| body.entries.len().saturating_add(1));
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            value.trace_value_slots(visitor);
        };
        reserve_set_for_target_len_with_roots(set, heap, target_len, &mut reserve_roots)?;
    }
    heap.with_payload(*set, |body| {
        if !body.entries.iter().any(|entry| entry.key_matches(&k)) {
            body.entries.push(SetEntry::live(k, value));
        }
    });
    if !exists {
        heap.record_write(*set, &barrier_value);
    }
    Ok(())
}

/// `Set.prototype.delete` — Spec §24.2.3.4.
pub fn set_delete(set: JsSet, heap: &mut otter_gc::GcHeap, value: &Value) -> bool {
    let k = MapKey::from_value(value);
    heap.with_payload(set, |body| {
        if let Some(entry) = body.entries.iter_mut().find(|entry| entry.key_matches(&k)) {
            entry.clear();
            true
        } else {
            false
        }
    })
}

/// `Set.prototype.clear` — Spec §24.2.3.3.
pub fn set_clear(set: JsSet, heap: &mut otter_gc::GcHeap) {
    heap.with_payload(set, |body| {
        for entry in &mut body.entries {
            entry.clear();
        }
    });
}

/// Snapshot value list in insertion order.
#[must_use]
pub fn set_values(set: JsSet, heap: &otter_gc::GcHeap) -> Vec<Value> {
    heap.read_payload(set, |body| {
        body.entries
            .iter()
            .filter_map(|entry| entry.value.clone())
            .collect()
    })
}

/// Raw backing-list length, including deleted tombstone slots.
#[must_use]
pub(crate) fn set_raw_len(set: JsSet, heap: &otter_gc::GcHeap) -> usize {
    heap.read_payload(set, |body| body.entries.len())
}

/// Read the raw set value currently at `index` in insertion order.
#[must_use]
pub(crate) fn set_value_at(set: JsSet, heap: &otter_gc::GcHeap, index: usize) -> Option<Value> {
    heap.read_payload(set, |body| {
        body.entries
            .get(index)
            .and_then(|entry| entry.value.clone())
    })
}

/// Identity comparison.
#[must_use]
pub fn set_ptr_eq(a: JsSet, b: JsSet) -> bool {
    a == b
}

/// JS `WeakMap` — object-keyed ephemeron table.
pub type JsWeakMap = otter_gc::Gc<WeakMapBody>;

#[derive(Debug, Default)]
/// GC-allocated storage backing every [`JsWeakMap`] handle.
pub struct WeakMapBody {
    entries: Vec<(RawGc, Value)>,
    prototype_override: Option<Value>,
}

impl otter_gc::SafeTraceable for WeakMapBody {
    const TYPE_TAG: u8 = WEAK_MAP_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        // Ephemeron entries are not ordinary strong edges. The VM
        // fixpoint marks values only after the key is already live.
        if let Some(proto) = &self.prototype_override {
            proto.trace_value_slots(visitor);
        }
    }

    fn trace_ephemeron_slots_safe(&mut self, visitor: &mut otter_gc::trace::EphemeronVisitor<'_>) {
        for (key, value) in &mut self.entries {
            let key_slot = key as *mut RawGc;
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
    heap.read_payload(map, |body| body.prototype_override.clone())
}

pub(crate) fn set_weak_map_prototype_override(
    map: JsWeakMap,
    heap: &mut otter_gc::GcHeap,
    proto: Option<Value>,
) {
    let barrier_value = proto.clone();
    heap.with_payload(map, |body| {
        body.prototype_override = proto;
    });
    if let Some(value) = &barrier_value {
        heap.record_write(map, value);
    }
}

/// `WeakMap.prototype.get` — Spec §24.3.3.3.
pub fn weak_map_get(
    map: JsWeakMap,
    heap: &otter_gc::GcHeap,
    key: &Value,
) -> Result<Option<Value>, CollectionError> {
    let key = object_gc_key(key)?;
    Ok(heap.read_payload(map, |body| {
        body.entries
            .iter()
            .find_map(|(entry_key, value)| (*entry_key == key).then(|| value.clone()))
    }))
}

/// `WeakMap.prototype.has` — Spec §24.3.3.4.
pub fn weak_map_has(
    map: JsWeakMap,
    heap: &otter_gc::GcHeap,
    key: &Value,
) -> Result<bool, CollectionError> {
    let key = object_gc_key(key)?;
    Ok(heap.read_payload(map, |body| {
        body.entries.iter().any(|(entry_key, _)| *entry_key == key)
    }))
}

/// Number of weak-map entries currently stored.
#[must_use]
pub fn weak_map_len(map: JsWeakMap, heap: &otter_gc::GcHeap) -> usize {
    heap.read_payload(map, |body| {
        body.entries
            .iter()
            .filter(|(entry_key, _)| !entry_key.is_null())
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
    let barrier_value = value.clone();
    let key_root = key.clone();
    let value_root = value.clone();
    let key_raw = object_gc_key(&key)?;
    let exists = heap.read_payload(map, |body| {
        body.entries
            .iter()
            .any(|(entry_key, _)| *entry_key == key_raw)
    });
    if !exists {
        let target_len = weak_map_len(map, heap).saturating_add(1);
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            key_root.trace_value_slots(visitor);
            value_root.trace_value_slots(visitor);
        };
        reserve_weak_map_for_target_len_with_roots(&mut map, heap, target_len, &mut reserve_roots)?;
    }
    let key = object_gc_key(&key)?;
    heap.with_payload(map, |body| {
        if let Some((_, existing)) = body
            .entries
            .iter_mut()
            .find(|(entry_key, _)| *entry_key == key)
        {
            *existing = value;
        } else {
            body.entries.push((key, value));
        }
    });
    heap.record_write(map, &barrier_value);
    Ok(())
}

/// `WeakMap.prototype.set` for stack/native-visible VM construction paths.
pub(crate) fn weak_map_set_with_roots(
    map: &mut JsWeakMap,
    heap: &mut otter_gc::GcHeap,
    key: Value,
    value: Value,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), CollectionError> {
    let barrier_value = value.clone();
    let key_root = key.clone();
    let value_root = value.clone();
    let key = object_gc_key(&key)?;
    let exists = heap.read_payload(*map, |body| {
        body.entries.iter().any(|(entry_key, _)| *entry_key == key)
    });
    if !exists {
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            key_root.trace_value_slots(visitor);
            value_root.trace_value_slots(visitor);
        };
        reserve_weak_map_for_target_len_with_roots(
            map,
            heap,
            weak_map_len(*map, heap).saturating_add(1),
            &mut reserve_roots,
        )?;
    }
    heap.with_payload(*map, |body| {
        if let Some((_, existing)) = body
            .entries
            .iter_mut()
            .find(|(entry_key, _)| *entry_key == key)
        {
            *existing = value;
        } else {
            body.entries.push((key, value));
        }
    });
    heap.record_write(*map, &barrier_value);
    Ok(())
}

/// `WeakMap.prototype.delete` — Spec §24.3.3.2.
pub fn weak_map_delete(
    map: JsWeakMap,
    heap: &mut otter_gc::GcHeap,
    key: &Value,
) -> Result<bool, CollectionError> {
    let key = object_gc_key(key)?;
    Ok(heap.with_payload(map, |body| {
        if let Some(index) = body
            .entries
            .iter()
            .position(|(entry_key, _)| *entry_key == key)
        {
            body.entries.remove(index);
            true
        } else {
            false
        }
    }))
}

/// JS `WeakSet` — object-keyed weak set.
pub type JsWeakSet = otter_gc::Gc<WeakSetBody>;

#[derive(Debug, Default)]
/// GC-allocated storage backing every [`JsWeakSet`] handle.
pub struct WeakSetBody {
    entries: Vec<RawGc>,
    prototype_override: Option<Value>,
}

impl otter_gc::SafeTraceable for WeakSetBody {
    const TYPE_TAG: u8 = WEAK_SET_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        // WeakSet keys are weak and never traced as strong edges.
        if let Some(proto) = &self.prototype_override {
            proto.trace_value_slots(visitor);
        }
    }

    fn trace_ephemeron_slots_safe(&mut self, visitor: &mut otter_gc::trace::EphemeronVisitor<'_>) {
        for key in &mut self.entries {
            let key_slot = key as *mut RawGc;
            let mut visit_value_slots = |_slot_visitor: &mut SlotVisitor<'_>| {};
            visitor(key_slot, &mut visit_value_slots);
        }
    }
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
    heap.read_payload(set, |body| body.prototype_override.clone())
}

pub(crate) fn set_weak_set_prototype_override(
    set: JsWeakSet,
    heap: &mut otter_gc::GcHeap,
    proto: Option<Value>,
) {
    let barrier_value = proto.clone();
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
    heap: &otter_gc::GcHeap,
    value: &Value,
) -> Result<bool, CollectionError> {
    let key = object_gc_key(value)?;
    Ok(heap.read_payload(set, |body| {
        body.entries.iter().any(|entry_key| *entry_key == key)
    }))
}

/// Number of weak-set entries currently stored.
#[must_use]
pub fn weak_set_len(set: JsWeakSet, heap: &otter_gc::GcHeap) -> usize {
    heap.read_payload(set, |body| {
        body.entries
            .iter()
            .filter(|entry_key| !entry_key.is_null())
            .count()
    })
}

/// `WeakSet.prototype.add` — Spec §24.4.3.1.
pub fn weak_set_add(
    mut set: JsWeakSet,
    heap: &mut otter_gc::GcHeap,
    value: Value,
) -> Result<(), CollectionError> {
    let value_root = value.clone();
    let key_raw = object_gc_key(&value)?;
    let exists = heap.read_payload(set, |body| body.entries.contains(&key_raw));
    if !exists {
        let target_len = weak_set_len(set, heap).saturating_add(1);
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            value_root.trace_value_slots(visitor);
        };
        reserve_weak_set_for_target_len_with_roots(&mut set, heap, target_len, &mut reserve_roots)?;
    }
    let key = object_gc_key(&value)?;
    heap.with_payload(set, |body| {
        if !body.entries.contains(&key) {
            body.entries.push(key);
        }
    });
    Ok(())
}

/// `WeakSet.prototype.add` for stack/native-visible VM construction paths.
pub(crate) fn weak_set_add_with_roots(
    set: &mut JsWeakSet,
    heap: &mut otter_gc::GcHeap,
    value: Value,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), CollectionError> {
    let value_root = value.clone();
    let key = object_gc_key(&value)?;
    let exists = heap.read_payload(*set, |body| body.entries.contains(&key));
    if !exists {
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            value_root.trace_value_slots(visitor);
        };
        reserve_weak_set_for_target_len_with_roots(
            set,
            heap,
            weak_set_len(*set, heap).saturating_add(1),
            &mut reserve_roots,
        )?;
    }
    heap.with_payload(*set, |body| {
        if !body.entries.contains(&key) {
            body.entries.push(key);
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
    let key = object_gc_key(value)?;
    Ok(heap.with_payload(set, |body| {
        if let Some(index) = body.entries.iter().position(|entry_key| *entry_key == key) {
            body.entries.remove(index);
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
            match heap.raw_type_tag(raw) {
                Some(WEAK_MAP_BODY_TYPE_TAG) => {
                    let Some(map) = heap.cast_raw_if_type::<WeakMapBody>(raw) else {
                        continue;
                    };
                    heap.read_payload(map, |body| {
                        for (key, value) in body.entries.iter().filter(|(key, _)| !key.is_null()) {
                            if heap.is_marked(*key)
                                && let Some(value_raw) = value.as_gc_raw()
                            {
                                additions.push(value_raw);
                            }
                        }
                    });
                }
                Some(WEAK_SET_BODY_TYPE_TAG) => {}
                _ => {}
            }
        }
        if !heap.mark_additional(additions) {
            break;
        }
    }

    for raw in heap.ephemeron_tables_snapshot() {
        if !heap.is_marked(raw) {
            continue;
        }
        match heap.raw_type_tag(raw) {
            Some(WEAK_MAP_BODY_TYPE_TAG) => {
                let Some(map) = heap.cast_raw_if_type::<WeakMapBody>(raw) else {
                    continue;
                };
                let live_keys: std::collections::HashSet<_> = heap
                    .read_payload(map, |body| {
                        body.entries.iter().map(|(key, _)| *key).collect::<Vec<_>>()
                    })
                    .into_iter()
                    .filter(|key| !key.is_null())
                    .filter(|key| heap.is_marked(*key))
                    .collect();
                heap.with_payload(map, |body| {
                    body.entries.retain(|(key, _)| live_keys.contains(key));
                });
            }
            Some(WEAK_SET_BODY_TYPE_TAG) => {
                let Some(set) = heap.cast_raw_if_type::<WeakSetBody>(raw) else {
                    continue;
                };
                let live_keys: std::collections::HashSet<_> = heap
                    .read_payload(set, |body| body.entries.clone())
                    .into_iter()
                    .filter(|key| !key.is_null())
                    .filter(|key| heap.is_marked(*key))
                    .collect();
                heap.with_payload(set, |body| {
                    body.entries.retain(|key| live_keys.contains(key));
                });
            }
            _ => {}
        }
    }
}

/// Project an object-shaped [`Value`] to a migrated GC key.
fn object_gc_key(value: &Value) -> Result<RawGc, CollectionError> {
    value.as_gc_raw().ok_or(CollectionError::NonObjectKey)
}

fn trace_map_key(key: &MapKey, visitor: &mut SlotVisitor<'_>) {
    if let MapKey::ObjectValue(value) = key {
        value.trace_value_slots(visitor);
    }
}

fn reserve_map_for_target_len_with_roots(
    map: &mut JsMap,
    heap: &mut otter_gc::GcHeap,
    target_len: usize,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    let capacity = heap.read_payload(*map, |body| body.entries.capacity());
    if target_len <= capacity {
        return Ok(());
    }
    let before = map_capacity_bytes(capacity);
    let after = map_capacity_bytes(target_len);
    if after > before {
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            visitor(std::ptr::addr_of_mut!(*map) as *mut RawGc);
        };
        heap.reserve_bytes_with_roots((after - before) as u64, &mut reserve_roots)?;
    }
    heap.with_payload(*map, |body| {
        body.entries
            .reserve(target_len.saturating_sub(body.entries.len()));
    });
    Ok(())
}

fn reserve_set_for_target_len_with_roots(
    set: &mut JsSet,
    heap: &mut otter_gc::GcHeap,
    target_len: usize,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    let capacity = heap.read_payload(*set, |body| body.entries.capacity());
    if target_len <= capacity {
        return Ok(());
    }
    let before = set_capacity_bytes(capacity);
    let after = set_capacity_bytes(target_len);
    if after > before {
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            visitor(std::ptr::addr_of_mut!(*set) as *mut RawGc);
        };
        heap.reserve_bytes_with_roots((after - before) as u64, &mut reserve_roots)?;
    }
    heap.with_payload(*set, |body| {
        body.entries
            .reserve(target_len.saturating_sub(body.entries.len()));
    });
    Ok(())
}

fn reserve_weak_map_for_target_len_with_roots(
    map: &mut JsWeakMap,
    heap: &mut otter_gc::GcHeap,
    target_len: usize,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    let capacity = heap.read_payload(*map, |body| body.entries.capacity());
    if target_len <= capacity {
        return Ok(());
    }
    let before = weak_map_capacity_bytes(capacity);
    let after = weak_map_capacity_bytes(target_len);
    if after > before {
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            visitor(std::ptr::addr_of_mut!(*map) as *mut RawGc);
        };
        heap.reserve_bytes_with_roots((after - before) as u64, &mut reserve_roots)?;
    }
    heap.with_payload(*map, |body| {
        body.entries
            .reserve(target_len.saturating_sub(body.entries.len()));
    });
    Ok(())
}

fn reserve_weak_set_for_target_len_with_roots(
    set: &mut JsWeakSet,
    heap: &mut otter_gc::GcHeap,
    target_len: usize,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    let capacity = heap.read_payload(*set, |body| body.entries.capacity());
    if target_len <= capacity {
        return Ok(());
    }
    let before = weak_set_capacity_bytes(capacity);
    let after = weak_set_capacity_bytes(target_len);
    if after > before {
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            visitor(std::ptr::addr_of_mut!(*set) as *mut RawGc);
        };
        heap.reserve_bytes_with_roots((after - before) as u64, &mut reserve_roots)?;
    }
    heap.with_payload(*set, |body| {
        body.entries
            .reserve(target_len.saturating_sub(body.entries.len()));
    });
    Ok(())
}

fn map_capacity_bytes(capacity: usize) -> usize {
    capacity.saturating_mul(std::mem::size_of::<MapEntry>())
}

fn set_capacity_bytes(capacity: usize) -> usize {
    capacity.saturating_mul(std::mem::size_of::<SetEntry>())
}

fn weak_map_capacity_bytes(capacity: usize) -> usize {
    capacity.saturating_mul(std::mem::size_of::<(RawGc, Value)>())
}

fn weak_set_capacity_bytes(capacity: usize) -> usize {
    capacity.saturating_mul(std::mem::size_of::<RawGc>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::number::NumberValue;
    use crate::string::StringHeap;

    fn n(i: i32) -> Value {
        Value::Number(NumberValue::from_i32(i))
    }

    fn young_object_value(heap: &mut otter_gc::GcHeap) -> Value {
        let mut no_roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        Value::Object(crate::object::alloc_object_with_roots(heap, &mut no_roots).unwrap())
    }

    #[test]
    fn map_insertion_order_preserved() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let m = alloc_map(&mut heap).unwrap();
        map_set(m, &mut heap, n(1), Value::Boolean(true)).unwrap();
        map_set(m, &mut heap, n(2), Value::Boolean(false)).unwrap();
        map_set(m, &mut heap, n(1), Value::Boolean(false)).unwrap(); // update
        let keys = map_keys(m, &heap);
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].as_number().unwrap().as_smi(), Some(1));
        assert_eq!(keys[1].as_number().unwrap().as_smi(), Some(2));
        assert_eq!(map_get(m, &heap, &n(1)), Some(Value::Boolean(false)));
    }

    #[test]
    fn map_samevaluezero_zero_collapse() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let m = alloc_map(&mut heap).unwrap();
        map_set(
            m,
            &mut heap,
            Value::Number(NumberValue::from_f64(-0.0)),
            n(7),
        )
        .unwrap();
        let v = map_get(m, &heap, &Value::Number(NumberValue::from_f64(0.0)));
        assert_eq!(v, Some(n(7)));
    }

    #[test]
    fn map_samevaluezero_nan_matches() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let m = alloc_map(&mut heap).unwrap();
        map_set(
            m,
            &mut heap,
            Value::Number(NumberValue::from_f64(f64::NAN)),
            n(9),
        )
        .unwrap();
        let v = map_get(m, &heap, &Value::Number(NumberValue::from_f64(f64::NAN)));
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
    fn map_object_key_survives_minor_relocation() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let mut m = alloc_map(&mut heap).unwrap();
        let key = young_object_value(&mut heap);
        let before = key.as_gc_raw().unwrap();

        map_set(m, &mut heap, key.clone(), n(42)).unwrap();

        let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visitor(std::ptr::addr_of_mut!(m) as *mut RawGc);
            key.trace_value_slots(visitor);
        };
        heap.collect_minor_with_roots(&mut roots);

        let after = key.as_gc_raw().unwrap();
        assert_ne!(after, before);
        assert!(map_has(m, &heap, &key));
        assert_eq!(map_get(m, &heap, &key), Some(n(42)));
        assert_eq!(map_keys(m, &heap), vec![key]);
    }

    #[test]
    fn set_object_key_survives_minor_relocation() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let mut s = alloc_set(&mut heap).unwrap();
        let key = young_object_value(&mut heap);
        let before = key.as_gc_raw().unwrap();

        set_add(s, &mut heap, key.clone()).unwrap();

        let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visitor(std::ptr::addr_of_mut!(s) as *mut RawGc);
            key.trace_value_slots(visitor);
        };
        heap.collect_minor_with_roots(&mut roots);

        let after = key.as_gc_raw().unwrap();
        assert_ne!(after, before);
        assert!(set_has(s, &heap, &key));
        assert_eq!(set_values(s, &heap), vec![key]);
    }

    #[test]
    fn weakmap_rejects_primitive_keys() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let wm = alloc_weak_map(&mut heap).unwrap();
        let err = weak_map_set(wm, &mut heap, n(1), Value::Boolean(true)).unwrap_err();
        assert!(matches!(err, CollectionError::NonObjectKey));
    }

    #[test]
    fn weakmap_object_key_roundtrips() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let wm = alloc_weak_map(&mut heap).unwrap();
        let obj = Value::Object(crate::object::alloc_object_old_for_fixture(&mut heap).unwrap());
        weak_map_set(wm, &mut heap, obj.clone(), n(42)).unwrap();
        assert!(weak_map_has(wm, &heap, &obj).unwrap());
        assert_eq!(weak_map_get(wm, &heap, &obj).unwrap(), Some(n(42)));
        let other = Value::Object(crate::object::alloc_object_old_for_fixture(&mut heap).unwrap());
        assert!(!weak_map_has(wm, &heap, &other).unwrap());
    }

    #[test]
    fn weakmap_young_key_and_value_survive_minor_relocation_when_key_rooted() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let mut wm = alloc_weak_map(&mut heap).unwrap();
        let key = young_object_value(&mut heap);
        let value = young_object_value(&mut heap);
        let key_before = key.as_gc_raw().unwrap();
        let value_before = value.as_gc_raw().unwrap();

        weak_map_set(wm, &mut heap, key.clone(), value).unwrap();

        let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visitor(std::ptr::addr_of_mut!(wm) as *mut RawGc);
            key.trace_value_slots(visitor);
        };
        heap.collect_minor_with_roots(&mut roots);

        let key_after = key.as_gc_raw().unwrap();
        let value_after = weak_map_get(wm, &heap, &key)
            .unwrap()
            .and_then(|value| value.as_gc_raw())
            .unwrap();
        assert_ne!(key_after, key_before);
        assert_ne!(value_after, value_before);
        assert!(weak_map_has(wm, &heap, &key).unwrap());
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
        heap.collect_minor_with_roots(&mut roots);

        assert_eq!(weak_map_len(wm, &heap), 0);
    }

    #[test]
    fn weakset_young_key_survives_minor_relocation_when_rooted() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let mut ws = alloc_weak_set(&mut heap).unwrap();
        let key = young_object_value(&mut heap);
        let before = key.as_gc_raw().unwrap();

        weak_set_add(ws, &mut heap, key.clone()).unwrap();

        let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visitor(std::ptr::addr_of_mut!(ws) as *mut RawGc);
            key.trace_value_slots(visitor);
        };
        heap.collect_minor_with_roots(&mut roots);

        let after = key.as_gc_raw().unwrap();
        assert_ne!(after, before);
        assert!(weak_set_has(ws, &heap, &key).unwrap());
    }

    #[test]
    fn map_string_keys() {
        let string_heap = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let m = alloc_map(&mut gc_heap).unwrap();
        map_set(
            m,
            &mut gc_heap,
            Value::String(JsString::from_str("k", &string_heap).unwrap()),
            n(1),
        )
        .unwrap();
        assert_eq!(
            map_get(
                m,
                &gc_heap,
                &Value::String(JsString::from_str("k", &string_heap).unwrap())
            ),
            Some(n(1)),
        );
    }
}
