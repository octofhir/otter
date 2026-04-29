//! `Map`, `Set`, `WeakMap`, `WeakSet` collection value types.
//!
//! `Map` and `Set` preserve insertion order (ECMA-262 §24.1 /
//! §24.2). `WeakMap` and `WeakSet` are object-keyed and would
//! release entries once their key becomes unreachable; the
//! foundation has no tracing GC yet (task 57), so weak collections
//! behave as strong-keyed today and entries live until cleared
//! explicitly. The module docstring is the canonical place to
//! note this gap so the GC slice can wire eviction in without
//! reshaping the public API.
//!
//! # Contents
//! - [`JsMap`] — heap-shared, `IndexMap`-backed associative store.
//! - [`JsSet`] — heap-shared, `IndexSet`-backed unique-element store.
//! - [`JsWeakMap`] — object-keyed map (strong-ref today, weak when
//!   GC ships).
//! - [`JsWeakSet`] — object-keyed set with the same caveat.
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

use std::cell::{Ref, RefCell, RefMut};
use std::rc::Rc;

use indexmap::IndexMap;

use crate::Value;
use crate::object::JsObject;
use crate::string::JsString;
use crate::symbol::JsSymbol;

/// Equality key for [`JsMap`] / [`JsSet`].
///
/// Implements ECMA-262 SameValueZero (§7.2.12): `+0` and `-0` map
/// to the same key, `NaN` matches itself, strings compare by
/// content, symbols compare by identity, objects/arrays/functions
/// compare by handle (`Rc::ptr_eq` via raw-pointer hashing).
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
    /// Heap object identity (`Rc::as_ptr`).
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
    /// 2. Object-shaped values map to [`MapKey::ObjectPtr`] keyed
    ///    on the underlying `Rc`'s data pointer; the originating
    ///    `Value` is stashed in [`MapKey::ObjectValue`] alongside it
    ///    via [`Self::from_value_with_origin`].
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
            Value::Object(o) => MapKey::ObjectPtr(o.identity_addr()),
            Value::Array(a) => MapKey::ObjectPtr(a.identity_addr()),
            Value::RegExp(r) => MapKey::ObjectPtr(r.identity_addr()),
            Value::Promise(_) => {
                // Promises do not yet expose a stable `identity_addr`
                // helper; fall through to display-based hashing —
                // distinct handles still compare unequal because
                // `Value::eq` uses `ptr_eq` on the pointer pair.
                MapKey::ObjectValue(value.clone())
            }
            // Functions, closures, bound functions, native callables,
            // class constructors, iterators — all compare via the
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
#[derive(Debug, Clone)]
pub struct JsMap {
    inner: Rc<RefCell<MapBody>>,
}

#[derive(Debug, Default)]
struct MapBody {
    entries: IndexMap<MapKey, (Value, Value)>,
}

impl JsMap {
    /// Construct an empty `JsMap`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.borrow().entries.len()
    }

    /// `true` when empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `Map.prototype.get` — Spec §24.1.3.6.
    #[must_use]
    pub fn get(&self, key: &Value) -> Option<Value> {
        let k = MapKey::from_value(key);
        self.inner.borrow().entries.get(&k).map(|(_, v)| v.clone())
    }

    /// `Map.prototype.has` — Spec §24.1.3.7.
    #[must_use]
    pub fn has(&self, key: &Value) -> bool {
        let k = MapKey::from_value(key);
        self.inner.borrow().entries.contains_key(&k)
    }

    /// `Map.prototype.set` — Spec §24.1.3.9. Updates in place
    /// without changing insertion order; new keys append.
    pub fn set(&self, key: Value, value: Value) {
        let k = MapKey::from_value(&key);
        let mut body = self.inner.borrow_mut();
        if let Some(slot) = body.entries.get_mut(&k) {
            slot.1 = value;
        } else {
            body.entries.insert(k, (key, value));
        }
    }

    /// `Map.prototype.delete` — Spec §24.1.3.3. Returns `true` when
    /// the entry existed.
    pub fn delete(&self, key: &Value) -> bool {
        let k = MapKey::from_value(key);
        // `shift_remove` preserves the order of remaining entries
        // (matches spec iteration semantics).
        self.inner.borrow_mut().entries.shift_remove(&k).is_some()
    }

    /// `Map.prototype.clear` — Spec §24.1.3.2.
    pub fn clear(&self) {
        self.inner.borrow_mut().entries.clear();
    }

    /// Snapshot key list (in insertion order).
    #[must_use]
    pub fn keys(&self) -> Vec<Value> {
        self.inner
            .borrow()
            .entries
            .values()
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Snapshot value list (in insertion order).
    #[must_use]
    pub fn values(&self) -> Vec<Value> {
        self.inner
            .borrow()
            .entries
            .values()
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// Snapshot entry list as `[key, value]` arrays.
    #[must_use]
    pub fn entries(&self) -> Vec<(Value, Value)> {
        self.inner
            .borrow()
            .entries
            .values()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Identity comparison.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Default for JsMap {
    fn default() -> Self {
        Self {
            inner: Rc::new(RefCell::new(MapBody::default())),
        }
    }
}

/// JS `Set` — ordered unique-element store.
#[derive(Debug, Clone)]
pub struct JsSet {
    inner: Rc<RefCell<SetBody>>,
}

#[derive(Debug, Default)]
struct SetBody {
    /// `IndexMap<MapKey, Value>` rather than `IndexSet`: the
    /// original `Value` lives in the map slot so iteration returns
    /// the live handle (matching what `Map`'s tuple gives us).
    entries: IndexMap<MapKey, Value>,
}

impl JsSet {
    /// Construct an empty `JsSet`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of unique entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.borrow().entries.len()
    }

    /// `true` when empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `Set.prototype.has` — Spec §24.2.3.7.
    #[must_use]
    pub fn has(&self, value: &Value) -> bool {
        let k = MapKey::from_value(value);
        self.inner.borrow().entries.contains_key(&k)
    }

    /// `Set.prototype.add` — Spec §24.2.3.1.
    pub fn add(&self, value: Value) {
        let k = MapKey::from_value(&value);
        let mut body = self.inner.borrow_mut();
        if !body.entries.contains_key(&k) {
            body.entries.insert(k, value);
        }
    }

    /// `Set.prototype.delete` — Spec §24.2.3.4.
    pub fn delete(&self, value: &Value) -> bool {
        let k = MapKey::from_value(value);
        self.inner.borrow_mut().entries.shift_remove(&k).is_some()
    }

    /// `Set.prototype.clear` — Spec §24.2.3.3.
    pub fn clear(&self) {
        self.inner.borrow_mut().entries.clear();
    }

    /// Snapshot value list in insertion order.
    #[must_use]
    pub fn values(&self) -> Vec<Value> {
        self.inner.borrow().entries.values().cloned().collect()
    }

    /// Identity comparison.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Default for JsSet {
    fn default() -> Self {
        Self {
            inner: Rc::new(RefCell::new(SetBody::default())),
        }
    }
}

/// Object-keyed `WeakMap`.
///
/// **Foundation gap:** the runtime has no tracing GC yet, so
/// entries are kept by **strong** reference. Once task 57 lands,
/// the inner storage migrates to `Weak` handles and entries
/// disappear when the key becomes unreachable. The public surface
/// will stay the same.
#[derive(Debug, Clone)]
pub struct JsWeakMap {
    inner: Rc<RefCell<WeakMapBody>>,
}

#[derive(Debug, Default)]
struct WeakMapBody {
    entries: IndexMap<*const (), (Value, Value)>,
}

impl JsWeakMap {
    /// Construct an empty `JsWeakMap`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `WeakMap.prototype.get` — Spec §24.3.3.3.
    pub fn get(&self, key: &Value) -> Result<Option<Value>, CollectionError> {
        let ptr = object_identity(key)?;
        Ok(self
            .inner
            .borrow()
            .entries
            .get(&ptr)
            .map(|(_, v)| v.clone()))
    }

    /// `WeakMap.prototype.has` — Spec §24.3.3.4.
    pub fn has(&self, key: &Value) -> Result<bool, CollectionError> {
        let ptr = object_identity(key)?;
        Ok(self.inner.borrow().entries.contains_key(&ptr))
    }

    /// `WeakMap.prototype.set` — Spec §24.3.3.5.
    pub fn set(&self, key: Value, value: Value) -> Result<(), CollectionError> {
        let ptr = object_identity(&key)?;
        let mut body = self.inner.borrow_mut();
        if let Some(slot) = body.entries.get_mut(&ptr) {
            slot.1 = value;
        } else {
            body.entries.insert(ptr, (key, value));
        }
        Ok(())
    }

    /// `WeakMap.prototype.delete` — Spec §24.3.3.2.
    pub fn delete(&self, key: &Value) -> Result<bool, CollectionError> {
        let ptr = object_identity(key)?;
        Ok(self.inner.borrow_mut().entries.shift_remove(&ptr).is_some())
    }

    /// Identity comparison.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Default for JsWeakMap {
    fn default() -> Self {
        Self {
            inner: Rc::new(RefCell::new(WeakMapBody::default())),
        }
    }
}

/// Object-keyed `WeakSet`. Same foundation gap as
/// [`JsWeakMap`] — strong-keyed today.
#[derive(Debug, Clone)]
pub struct JsWeakSet {
    inner: Rc<RefCell<WeakSetBody>>,
}

#[derive(Debug, Default)]
struct WeakSetBody {
    entries: IndexMap<*const (), Value>,
}

impl JsWeakSet {
    /// Construct an empty `JsWeakSet`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `WeakSet.prototype.has` — Spec §24.4.3.4.
    pub fn has(&self, value: &Value) -> Result<bool, CollectionError> {
        let ptr = object_identity(value)?;
        Ok(self.inner.borrow().entries.contains_key(&ptr))
    }

    /// `WeakSet.prototype.add` — Spec §24.4.3.1.
    pub fn add(&self, value: Value) -> Result<(), CollectionError> {
        let ptr = object_identity(&value)?;
        let mut body = self.inner.borrow_mut();
        if !body.entries.contains_key(&ptr) {
            body.entries.insert(ptr, value);
        }
        Ok(())
    }

    /// `WeakSet.prototype.delete` — Spec §24.4.3.3.
    pub fn delete(&self, value: &Value) -> Result<bool, CollectionError> {
        let ptr = object_identity(value)?;
        Ok(self.inner.borrow_mut().entries.shift_remove(&ptr).is_some())
    }

    /// Identity comparison.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Default for JsWeakSet {
    fn default() -> Self {
        Self {
            inner: Rc::new(RefCell::new(WeakSetBody::default())),
        }
    }
}

/// Project an object-shaped [`Value`] to its identity pointer for
/// use as a `WeakMap`/`WeakSet` key.
fn object_identity(value: &Value) -> Result<*const (), CollectionError> {
    match value {
        Value::Object(o) => Ok(o.identity_addr()),
        Value::Array(a) => Ok(a.identity_addr()),
        Value::RegExp(r) => Ok(r.identity_addr()),
        _ => Err(CollectionError::NonObjectKey),
    }
}

/// Borrow helpers used by the iterator state machine.
impl JsMap {
    /// Read-only view of the entry list. Used by the iterator
    /// machinery to walk in insertion order without snapshotting.
    pub fn borrow_entries(&self) -> Ref<'_, IndexMap<MapKey, (Value, Value)>> {
        Ref::map(self.inner.borrow(), |b| &b.entries)
    }

    /// Mutable view (rare; only used by collection methods).
    pub fn borrow_entries_mut(&self) -> RefMut<'_, IndexMap<MapKey, (Value, Value)>> {
        RefMut::map(self.inner.borrow_mut(), |b| &mut b.entries)
    }
}

impl JsSet {
    /// Read-only view of the value list.
    pub fn borrow_values(&self) -> Ref<'_, IndexMap<MapKey, Value>> {
        Ref::map(self.inner.borrow(), |b| &b.entries)
    }
}

impl JsObject {
    // Symbol-keyed access already lives on JsObject; nothing to add
    // for collections beyond the borrow helpers above.
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
        let m = JsMap::new();
        m.set(n(1), Value::Boolean(true));
        m.set(n(2), Value::Boolean(false));
        m.set(n(1), Value::Boolean(false)); // update
        let keys = m.keys();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].as_number().unwrap().as_smi(), Some(1));
        assert_eq!(keys[1].as_number().unwrap().as_smi(), Some(2));
        assert_eq!(m.get(&n(1)), Some(Value::Boolean(false)));
    }

    #[test]
    fn map_samevaluezero_zero_collapse() {
        let m = JsMap::new();
        m.set(Value::Number(NumberValue::from_f64(-0.0)), n(7));
        let v = m.get(&Value::Number(NumberValue::from_f64(0.0)));
        assert_eq!(v, Some(n(7)));
    }

    #[test]
    fn map_samevaluezero_nan_matches() {
        let m = JsMap::new();
        m.set(Value::Number(NumberValue::from_f64(f64::NAN)), n(9));
        let v = m.get(&Value::Number(NumberValue::from_f64(f64::NAN)));
        assert_eq!(v, Some(n(9)));
    }

    #[test]
    fn set_dedupes() {
        let s = JsSet::new();
        s.add(n(1));
        s.add(n(1));
        s.add(n(2));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn weakmap_rejects_primitive_keys() {
        let wm = JsWeakMap::new();
        let err = wm.set(n(1), Value::Boolean(true)).unwrap_err();
        assert!(matches!(err, CollectionError::NonObjectKey));
    }

    #[test]
    fn weakmap_object_key_roundtrips() {
        let wm = JsWeakMap::new();
        let obj = Value::Object(JsObject::new());
        wm.set(obj.clone(), n(42)).unwrap();
        assert!(wm.has(&obj).unwrap());
        assert_eq!(wm.get(&obj).unwrap(), Some(n(42)));
        let other = Value::Object(JsObject::new());
        assert!(!wm.has(&other).unwrap());
    }

    #[test]
    fn map_string_keys() {
        let heap = StringHeap::default();
        let m = JsMap::new();
        m.set(Value::String(JsString::from_str("k", &heap).unwrap()), n(1));
        assert_eq!(
            m.get(&Value::String(JsString::from_str("k", &heap).unwrap())),
            Some(n(1)),
        );
    }
}
