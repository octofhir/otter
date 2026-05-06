//! `Map`, `Set`, `WeakMap`, `WeakSet` collection value types.
//!
//! `Map` and `Set` preserve insertion order (ECMA-262 §24.1 /
//! §24.2). `WeakMap` and `WeakSet` are object-keyed and keep
//! entries through ephemeron tables: keys are weak, and values
//! are marked only when their key is already reachable through
//! another path.
//!
//! # Contents
//! - [`JsMap`] — heap-shared, `IndexMap`-backed associative store.
//! - [`JsSet`] — heap-shared, `IndexSet`-backed unique-element store.
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

use indexmap::IndexMap;

use crate::Value;
use crate::string::JsString;
use crate::symbol::JsSymbol;

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
    /// Heap object identity.
    ObjectPtr(*const ()),
    /// The original [`Value`] for the object key — kept so iteration
    /// can hand back the live key reference.
    ObjectValue(Value),
}

impl MapKey {
    /// Project a [`Value`] into its [`MapKey`] form.
    ///
    /// # Algorithm
    /// 1. Primitives map to a structural variant (number normalises
    ///    `-0.0 → 0.0`).
    /// 2. Migrated object-shaped values map to [`MapKey::ObjectPtr`]
    ///    keyed on their heap identity; non-migrated object-shaped
    ///    values fall back to [`MapKey::ObjectValue`].
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
            Value::Object(o) => MapKey::ObjectPtr(o.as_header_ptr() as *const ()),
            Value::Array(a) => MapKey::ObjectPtr(crate::array::identity_addr(*a)),
            Value::RegExp(r) => MapKey::ObjectPtr(r.identity_addr()),
            Value::Promise(p) => MapKey::ObjectPtr(p.identity_addr()),
            Value::Iterator(i) => MapKey::ObjectPtr(i.as_header_ptr() as *const ()),
            Value::Generator(g) => MapKey::ObjectPtr(g.identity_addr()),
            Value::BoundFunction(b) => MapKey::ObjectPtr(b.identity_addr()),
            Value::NativeFunction(n) => MapKey::ObjectPtr(n.identity_addr()),
            // Functions, closures, class constructors, and other
            // non-GC reference wrappers — all compare via the
            // originating `Value`'s `PartialEq`, which is identity
            // on every callable shape.
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
            (MapKey::ObjectPtr(a), MapKey::ObjectPtr(b)) => a == b,
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
            MapKey::ObjectPtr(p) => p.hash(state),
            MapKey::ObjectValue(_) => {
                // Identity-based fallback: hash by discriminant alone
                // and rely on `eq` to disambiguate. Collisions are
                // rare (only callable values land here).
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
}

/// JS `Map` — ordered associative store.
///
/// Cloning shares storage. Storage is `IndexMap<MapKey, (Value, Value)>`
/// where the tuple holds `(original_key_value, value)` so iteration
/// can hand back the original key (e.g. the actual object handle,
/// not the pointer-projected key form).
pub type JsMap = otter_gc::Gc<MapBody>;

#[derive(Debug, Default)]
/// GC-allocated storage backing every [`JsMap`] handle.
pub struct MapBody {
    entries: IndexMap<MapKey, (Value, Value)>,
}

impl otter_gc::SafeTraceable for MapBody {
    const TYPE_TAG: u8 = MAP_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut otter_gc::SlotVisitor<'_>) {
        for (key, (original_key, value)) in &self.entries {
            trace_map_key(key, visitor);
            original_key.trace_value_slots(visitor);
            value.trace_value_slots(visitor);
        }
    }
}

/// Allocate a fresh empty `Map`.
pub fn alloc_map(heap: &mut otter_gc::GcHeap) -> Result<JsMap, otter_gc::OutOfMemory> {
    heap.alloc_old(MapBody::default())
}

/// Number of entries.
#[must_use]
pub fn map_len(map: JsMap, heap: &otter_gc::GcHeap) -> usize {
    heap.read_payload(map, |body| body.entries.len())
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
    heap.read_payload(map, |body| body.entries.get(&k).map(|(_, v)| v.clone()))
}

/// `Map.prototype.has` — Spec §24.1.3.7.
#[must_use]
pub fn map_has(map: JsMap, heap: &otter_gc::GcHeap, key: &Value) -> bool {
    let k = MapKey::from_value(key);
    heap.read_payload(map, |body| body.entries.contains_key(&k))
}

/// `Map.prototype.set` — Spec §24.1.3.9. Updates in place
/// without changing insertion order; new keys append.
pub fn map_set(
    map: JsMap,
    heap: &mut otter_gc::GcHeap,
    key: Value,
    value: Value,
) -> Result<(), otter_gc::OutOfMemory> {
    let key_raw = key.as_gc_raw();
    let value_raw = value.as_gc_raw();
    let k = MapKey::from_value(&key);
    let exists = heap.read_payload(map, |body| body.entries.contains_key(&k));
    if !exists {
        reserve_map_for_target_len(map, heap, map_len(map, heap).saturating_add(1))?;
    }
    heap.with_payload(map, |body| {
        if let Some(slot) = body.entries.get_mut(&k) {
            slot.1 = value;
        } else {
            body.entries.insert(k, (key, value));
        }
    });
    if !exists {
        if let Some(child) = key_raw {
            heap.write_barrier_raw(map, map_payload_slot(map), child);
        }
    }
    if let Some(child) = value_raw {
        heap.write_barrier_raw(map, map_payload_slot(map), child);
    }
    Ok(())
}

/// `Map.prototype.delete` — Spec §24.1.3.3. Returns `true` when
/// the entry existed.
pub fn map_delete(map: JsMap, heap: &mut otter_gc::GcHeap, key: &Value) -> bool {
    let k = MapKey::from_value(key);
    // `shift_remove` preserves the order of remaining entries
    // (matches spec iteration semantics).
    heap.with_payload(map, |body| body.entries.shift_remove(&k).is_some())
}

/// `Map.prototype.clear` — Spec §24.1.3.2.
pub fn map_clear(map: JsMap, heap: &mut otter_gc::GcHeap) {
    heap.with_payload(map, |body| body.entries.clear());
}

/// Snapshot key list (in insertion order).
#[must_use]
pub fn map_keys(map: JsMap, heap: &otter_gc::GcHeap) -> Vec<Value> {
    heap.read_payload(map, |body| {
        body.entries.values().map(|(k, _)| k.clone()).collect()
    })
}

/// Snapshot value list (in insertion order).
#[must_use]
pub fn map_values(map: JsMap, heap: &otter_gc::GcHeap) -> Vec<Value> {
    heap.read_payload(map, |body| {
        body.entries.values().map(|(_, v)| v.clone()).collect()
    })
}

/// Snapshot entry list.
#[must_use]
pub fn map_entries(map: JsMap, heap: &otter_gc::GcHeap) -> Vec<(Value, Value)> {
    heap.read_payload(map, |body| {
        body.entries
            .values()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    })
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
    /// `IndexMap<MapKey, Value>` rather than `IndexSet`: the
    /// original `Value` lives in the map slot so iteration returns
    /// the live handle (matching what `Map`'s tuple gives us).
    entries: IndexMap<MapKey, Value>,
}

impl otter_gc::SafeTraceable for SetBody {
    const TYPE_TAG: u8 = SET_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut otter_gc::SlotVisitor<'_>) {
        for (key, value) in &self.entries {
            trace_map_key(key, visitor);
            value.trace_value_slots(visitor);
        }
    }
}

/// Allocate a fresh empty `Set`.
pub fn alloc_set(heap: &mut otter_gc::GcHeap) -> Result<JsSet, otter_gc::OutOfMemory> {
    heap.alloc_old(SetBody::default())
}

/// Number of unique entries.
#[must_use]
pub fn set_len(set: JsSet, heap: &otter_gc::GcHeap) -> usize {
    heap.read_payload(set, |body| body.entries.len())
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
    heap.read_payload(set, |body| body.entries.contains_key(&k))
}

/// `Set.prototype.add` — Spec §24.2.3.1.
pub fn set_add(
    set: JsSet,
    heap: &mut otter_gc::GcHeap,
    value: Value,
) -> Result<(), otter_gc::OutOfMemory> {
    let value_raw = value.as_gc_raw();
    let k = MapKey::from_value(&value);
    let exists = heap.read_payload(set, |body| body.entries.contains_key(&k));
    if !exists {
        reserve_set_for_target_len(set, heap, set_len(set, heap).saturating_add(1))?;
    }
    heap.with_payload(set, |body| {
        if !body.entries.contains_key(&k) {
            body.entries.insert(k, value);
        }
    });
    if !exists {
        if let Some(child) = value_raw {
            heap.write_barrier_raw(set, set_payload_slot(set), child);
        }
    }
    Ok(())
}

/// `Set.prototype.delete` — Spec §24.2.3.4.
pub fn set_delete(set: JsSet, heap: &mut otter_gc::GcHeap, value: &Value) -> bool {
    let k = MapKey::from_value(value);
    heap.with_payload(set, |body| body.entries.shift_remove(&k).is_some())
}

/// `Set.prototype.clear` — Spec §24.2.3.3.
pub fn set_clear(set: JsSet, heap: &mut otter_gc::GcHeap) {
    heap.with_payload(set, |body| body.entries.clear());
}

/// Snapshot value list in insertion order.
#[must_use]
pub fn set_values(set: JsSet, heap: &otter_gc::GcHeap) -> Vec<Value> {
    heap.read_payload(set, |body| body.entries.values().cloned().collect())
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
    entries: IndexMap<otter_gc::RawGc, Value>,
}

impl otter_gc::SafeTraceable for WeakMapBody {
    const TYPE_TAG: u8 = WEAK_MAP_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, _visitor: &mut otter_gc::SlotVisitor<'_>) {
        // Ephemeron entries are not ordinary strong edges. The VM
        // fixpoint marks values only after the key is already live.
    }
}

/// Allocate a fresh empty `WeakMap`.
pub fn alloc_weak_map(heap: &mut otter_gc::GcHeap) -> Result<JsWeakMap, otter_gc::OutOfMemory> {
    let map = heap.alloc_old(WeakMapBody::default())?;
    heap.register_ephemeron_table(map);
    Ok(map)
}

/// `WeakMap.prototype.get` — Spec §24.3.3.3.
pub fn weak_map_get(
    map: JsWeakMap,
    heap: &otter_gc::GcHeap,
    key: &Value,
) -> Result<Option<Value>, CollectionError> {
    let key = object_gc_key(key)?;
    Ok(heap.read_payload(map, |body| body.entries.get(&key).cloned()))
}

/// `WeakMap.prototype.has` — Spec §24.3.3.4.
pub fn weak_map_has(
    map: JsWeakMap,
    heap: &otter_gc::GcHeap,
    key: &Value,
) -> Result<bool, CollectionError> {
    let key = object_gc_key(key)?;
    Ok(heap.read_payload(map, |body| body.entries.contains_key(&key)))
}

/// `WeakMap.prototype.set` — Spec §24.3.3.5.
pub fn weak_map_set(
    map: JsWeakMap,
    heap: &mut otter_gc::GcHeap,
    key: Value,
    value: Value,
) -> Result<(), CollectionError> {
    let key = object_gc_key(&key)?;
    heap.with_payload(map, |body| {
        body.entries.insert(key, value);
    });
    Ok(())
}

/// `WeakMap.prototype.delete` — Spec §24.3.3.2.
pub fn weak_map_delete(
    map: JsWeakMap,
    heap: &mut otter_gc::GcHeap,
    key: &Value,
) -> Result<bool, CollectionError> {
    let key = object_gc_key(key)?;
    Ok(heap.with_payload(map, |body| body.entries.shift_remove(&key).is_some()))
}

/// JS `WeakSet` — object-keyed weak set.
pub type JsWeakSet = otter_gc::Gc<WeakSetBody>;

#[derive(Debug, Default)]
/// GC-allocated storage backing every [`JsWeakSet`] handle.
pub struct WeakSetBody {
    entries: IndexMap<otter_gc::RawGc, ()>,
}

impl otter_gc::SafeTraceable for WeakSetBody {
    const TYPE_TAG: u8 = WEAK_SET_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, _visitor: &mut otter_gc::SlotVisitor<'_>) {
        // WeakSet keys are weak and never traced as strong edges.
    }
}

/// Allocate a fresh empty `WeakSet`.
pub fn alloc_weak_set(heap: &mut otter_gc::GcHeap) -> Result<JsWeakSet, otter_gc::OutOfMemory> {
    let set = heap.alloc_old(WeakSetBody::default())?;
    heap.register_ephemeron_table(set);
    Ok(set)
}

/// `WeakSet.prototype.has` — Spec §24.4.3.4.
pub fn weak_set_has(
    set: JsWeakSet,
    heap: &otter_gc::GcHeap,
    value: &Value,
) -> Result<bool, CollectionError> {
    let key = object_gc_key(value)?;
    Ok(heap.read_payload(set, |body| body.entries.contains_key(&key)))
}

/// `WeakSet.prototype.add` — Spec §24.4.3.1.
pub fn weak_set_add(
    set: JsWeakSet,
    heap: &mut otter_gc::GcHeap,
    value: Value,
) -> Result<(), CollectionError> {
    let key = object_gc_key(&value)?;
    heap.with_payload(set, |body| {
        body.entries.insert(key, ());
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
    Ok(heap.with_payload(set, |body| body.entries.shift_remove(&key).is_some()))
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
                        for (key, value) in &body.entries {
                            if heap.is_marked(*key) {
                                if let Some(value_raw) = value.as_gc_raw() {
                                    additions.push(value_raw);
                                }
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
                    .read_payload(map, |body| body.entries.keys().copied().collect::<Vec<_>>())
                    .into_iter()
                    .filter(|key| heap.is_marked(*key))
                    .collect();
                heap.with_payload(map, |body| {
                    body.entries.retain(|key, _| live_keys.contains(key));
                });
            }
            Some(WEAK_SET_BODY_TYPE_TAG) => {
                let Some(set) = heap.cast_raw_if_type::<WeakSetBody>(raw) else {
                    continue;
                };
                let live_keys: std::collections::HashSet<_> = heap
                    .read_payload(set, |body| body.entries.keys().copied().collect::<Vec<_>>())
                    .into_iter()
                    .filter(|key| heap.is_marked(*key))
                    .collect();
                heap.with_payload(set, |body| {
                    body.entries.retain(|key, _| live_keys.contains(key));
                });
            }
            _ => {}
        }
    }
}

/// Project an object-shaped [`Value`] to a migrated GC key.
fn object_gc_key(value: &Value) -> Result<otter_gc::RawGc, CollectionError> {
    value.as_gc_raw().ok_or(CollectionError::NonObjectKey)
}

fn trace_map_key(key: &MapKey, visitor: &mut otter_gc::SlotVisitor<'_>) {
    if let MapKey::ObjectValue(value) = key {
        value.trace_value_slots(visitor);
    }
}

fn map_payload_slot(map: JsMap) -> *mut otter_gc::RawGc {
    (map.as_header_ptr() as *mut u8).wrapping_add(std::mem::size_of::<otter_gc::GcHeader>())
        as *mut otter_gc::RawGc
}

fn set_payload_slot(set: JsSet) -> *mut otter_gc::RawGc {
    (set.as_header_ptr() as *mut u8).wrapping_add(std::mem::size_of::<otter_gc::GcHeader>())
        as *mut otter_gc::RawGc
}

fn reserve_map_for_target_len(
    map: JsMap,
    heap: &mut otter_gc::GcHeap,
    target_len: usize,
) -> Result<(), otter_gc::OutOfMemory> {
    let capacity = heap.read_payload(map, |body| body.entries.capacity());
    if target_len <= capacity {
        return Ok(());
    }
    let before = map_capacity_bytes(capacity);
    let after = map_capacity_bytes(target_len);
    if after > before {
        heap.reserve_bytes((after - before) as u64)?;
    }
    heap.with_payload(map, |body| {
        body.entries
            .reserve(target_len.saturating_sub(body.entries.len()));
    });
    Ok(())
}

fn reserve_set_for_target_len(
    set: JsSet,
    heap: &mut otter_gc::GcHeap,
    target_len: usize,
) -> Result<(), otter_gc::OutOfMemory> {
    let capacity = heap.read_payload(set, |body| body.entries.capacity());
    if target_len <= capacity {
        return Ok(());
    }
    let before = set_capacity_bytes(capacity);
    let after = set_capacity_bytes(target_len);
    if after > before {
        heap.reserve_bytes((after - before) as u64)?;
    }
    heap.with_payload(set, |body| {
        body.entries
            .reserve(target_len.saturating_sub(body.entries.len()));
    });
    Ok(())
}

fn map_capacity_bytes(capacity: usize) -> usize {
    capacity.saturating_mul(std::mem::size_of::<(MapKey, (Value, Value))>())
}

fn set_capacity_bytes(capacity: usize) -> usize {
    capacity.saturating_mul(std::mem::size_of::<(MapKey, Value)>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::number::NumberValue;
    use crate::string::StringHeap;

    fn n(i: i32) -> Value {
        Value::Number(NumberValue::from_i32(i))
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
        let obj = Value::Object(crate::object::alloc_object(&mut heap).unwrap());
        weak_map_set(wm, &mut heap, obj.clone(), n(42)).unwrap();
        assert!(weak_map_has(wm, &heap, &obj).unwrap());
        assert_eq!(weak_map_get(wm, &heap, &obj).unwrap(), Some(n(42)));
        let other = Value::Object(crate::object::alloc_object(&mut heap).unwrap());
        assert!(!weak_map_has(wm, &heap, &other).unwrap());
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
