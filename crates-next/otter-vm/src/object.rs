//! JavaScript object value with hidden-class / shape storage.
//!
//! Slice 17 introduced a flat `IndexMap<String, Value>` per
//! object. Slice 18 replaces that with a real shape model:
//!
//! - **`Shape`** — an immutable, `Rc`-shared description of a
//!   property layout: an ordered list of property names plus a
//!   transition map keyed by `(parent_shape, new_key)`. Adding the
//!   same key to the same parent always returns the same child
//!   shape, so two literals `{ a: 1, b: 2 }` and `{ a: 9, b: 8 }`
//!   share the same hidden class and identical property offsets.
//! - **`JsObject`** — `{ shape: Rc<Shape>, slots: SmallVec<[Value; 4]> }`.
//!   Property reads / writes go through the shape's offset table.
//!
//! Public API stays the same as in slice 17 (`set` / `get` /
//! `delete` / `len` / `borrow_props`). Existing fixtures continue
//! to pass without modification.
//!
//! # Contents
//! - [`JsObject`] — cheap-to-clone object handle.
//! - [`Shape`] — hidden class.
//! - [`Properties`] — read-only view used by debug rendering and
//!   future JSON serialization. Iterates in insertion order.
//!
//! # Invariants
//! - Insertion order is encoded by the shape's key vector.
//! - `JsObject::set("k", v)` for a key already present writes the
//!   value into the same slot; for a new key it transitions the
//!   shape and pushes a slot.
//! - `JsObject::delete("k")` switches the object to dictionary
//!   mode (a fresh, never-shared shape) so the surviving objects
//!   on the original shape don't pay for the rare delete path.
//! - Shape transitions are content-addressed: the shape tree
//!   dedupes children, so `Rc::ptr_eq` on shapes is a true
//!   "do these objects have the same hidden class" test.
//!
//! # See also
//! - foundation plan §M8.

use std::cell::{Ref, RefCell, RefMut};
use std::collections::HashMap;
use std::rc::Rc;

use smallvec::SmallVec;

use crate::Value;
use crate::string::JsString;
use crate::symbol::JsSymbol;

/// A hidden-class node. Shapes form a tree rooted at the empty
/// shape; each non-root shape records the **parent** plus the
/// **single key** added to reach it. The full ordered key vector
/// is materialized once and cached.
#[derive(Debug)]
pub struct Shape {
    /// Parent shape, or `None` for the root (empty) shape.
    /// Reserved for future shape-tree debugging and proto-chain
    /// hooks (slice 19); not read on the hot path.
    #[allow(dead_code)]
    parent: Option<Rc<Shape>>,
    /// Key added to `parent` to reach this shape. `None` for root.
    /// Same rationale as `parent`.
    #[allow(dead_code)]
    key: Option<String>,
    /// Full ordered list of keys held by this shape. Cached so
    /// every read takes O(1) instead of walking parents.
    keys: Vec<String>,
    /// Lazily-populated cache of `(key → slot index)` for O(1)
    /// lookups. Filled on first access.
    offsets: RefCell<Option<HashMap<String, u16>>>,
    /// Transitions: `key` → child shape. Keeps shapes content-
    /// addressed so two literals with the same key sequence share
    /// the same hidden class.
    transitions: RefCell<HashMap<String, Rc<Shape>>>,
}

impl Shape {
    /// Construct the root (empty) shape.
    pub fn root() -> Rc<Shape> {
        Rc::new(Shape {
            parent: None,
            key: None,
            keys: Vec::new(),
            offsets: RefCell::new(None),
            transitions: RefCell::new(HashMap::new()),
        })
    }

    /// Number of properties carried by this shape.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// `true` for the empty (root) shape.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Look up a property's slot offset. Lazily populates the
    /// `offsets` cache on first miss.
    pub fn offset_of(&self, key: &str) -> Option<u16> {
        let mut cache = self.offsets.borrow_mut();
        if cache.is_none() {
            let map: HashMap<String, u16> = self
                .keys
                .iter()
                .enumerate()
                .map(|(i, k)| (k.clone(), i as u16))
                .collect();
            *cache = Some(map);
        }
        cache.as_ref().and_then(|m| m.get(key).copied())
    }

    /// Iterate keys in insertion order.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.keys.iter()
    }

    /// Append `key` and return the resulting child shape, sharing
    /// it with prior callers when possible.
    pub fn add_property(self_rc: &Rc<Shape>, key: &str) -> Rc<Shape> {
        if let Some(existing) = self_rc.transitions.borrow().get(key) {
            return Rc::clone(existing);
        }
        let mut keys = self_rc.keys.clone();
        keys.push(key.to_string());
        let child = Rc::new(Shape {
            parent: Some(Rc::clone(self_rc)),
            key: Some(key.to_string()),
            keys,
            offsets: RefCell::new(None),
            transitions: RefCell::new(HashMap::new()),
        });
        self_rc
            .transitions
            .borrow_mut()
            .insert(key.to_string(), Rc::clone(&child));
        child
    }
}

thread_local! {
    /// Cheap shared root for empty objects so `JsObject::new()` is
    /// O(1) and dedupes the empty shape across the runtime.
    static ROOT_SHAPE: Rc<Shape> = Shape::root();
}

/// Heap-shared object handle. Cloning shares storage — both
/// handles see subsequent mutations.
#[derive(Debug, Clone)]
pub struct JsObject {
    inner: Rc<RefCell<ObjectBody>>,
}

#[derive(Debug)]
struct ObjectBody {
    shape: Rc<Shape>,
    slots: SmallVec<[Value; 4]>,
    /// Prototype object. `None` means the chain ends here (the
    /// object's `[[Prototype]]` is `null`). The default-empty
    /// prototype object will arrive when the runtime registers
    /// `Object.prototype`; until then, fresh objects start with
    /// `None`.
    prototype: Option<JsObject>,
    /// Symbol-keyed own properties. Foundation uses a flat vector
    /// keyed by [`JsSymbol`] identity (`ptr_eq`); typical objects
    /// carry zero symbol-keyed entries, so the linear scan is
    /// effectively constant-time.
    ///
    /// Stored separately from the string-keyed shape model so the
    /// hot string-key path (`obj.foo`) stays untouched. Spec
    /// behaviour matches §10.1 [[OwnPropertyKeys]] — symbol keys
    /// enumerate after string keys.
    symbol_props: Vec<(JsSymbol, Value)>,
}

/// Maximum prototype-chain hops a property lookup will follow
/// before raising a `RangeError`. Pinned at `1024` so pathological
/// cycles are caught quickly without affecting normal code.
pub const PROTO_CHAIN_HARD_CAP: usize = 1024;

impl JsObject {
    /// Allocate a fresh empty object on the root shape.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of own properties.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.borrow().shape.len()
    }

    /// `true` when there are no own properties.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Set or overwrite a property. Existing key writes into the
    /// same slot; new key transitions the shape and appends a slot.
    pub fn set(&self, key: &str, value: Value) {
        let mut body = self.inner.borrow_mut();
        if let Some(offset) = body.shape.offset_of(key) {
            body.slots[offset as usize] = value;
            return;
        }
        let new_shape = Shape::add_property(&body.shape, key);
        body.shape = new_shape;
        body.slots.push(value);
    }

    /// Read an **own** property. Does not walk the prototype
    /// chain. Returns a clone — `Value` is cheap to clone
    /// (`Arc` / `Rc` payloads).
    #[must_use]
    pub fn get_own(&self, key: &str) -> Option<Value> {
        let body = self.inner.borrow();
        body.shape
            .offset_of(key)
            .map(|offset| body.slots[offset as usize].clone())
    }

    /// Read a property, walking the prototype chain on miss.
    /// Bounded by [`PROTO_CHAIN_HARD_CAP`]; if the cap is hit,
    /// returns `None` (the runtime translates this into a
    /// `RangeError` if needed).
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Value> {
        if let Some(v) = self.get_own(key) {
            return Some(v);
        }
        let mut current = self.prototype();
        let mut hops = 0;
        while let Some(proto) = current {
            if hops >= PROTO_CHAIN_HARD_CAP {
                return None;
            }
            hops += 1;
            if let Some(v) = proto.get_own(key) {
                return Some(v);
            }
            current = proto.prototype();
        }
        None
    }

    /// Borrow the current prototype, if any.
    #[must_use]
    pub fn prototype(&self) -> Option<JsObject> {
        self.inner.borrow().prototype.clone()
    }

    /// Replace the prototype. `None` detaches the chain.
    pub fn set_prototype(&self, proto: Option<JsObject>) {
        self.inner.borrow_mut().prototype = proto;
    }

    /// `true` when this object has `target` somewhere in its
    /// prototype chain. Used by `instanceof`.
    #[must_use]
    pub fn has_in_proto_chain(&self, target: &JsObject) -> bool {
        let mut current = self.prototype();
        let mut hops = 0;
        while let Some(proto) = current {
            if hops >= PROTO_CHAIN_HARD_CAP {
                return false;
            }
            hops += 1;
            if proto.ptr_eq(target) {
                return true;
            }
            current = proto.prototype();
        }
        false
    }

    /// Look up by a [`JsString`] key. Convenience for dispatcher
    /// sites that already hold the WTF-16 form.
    #[must_use]
    pub fn get_jsstring(&self, key: &JsString) -> Option<Value> {
        let utf8 = key.to_lossy_string();
        self.get(&utf8)
    }

    /// Remove a property. Returns `true` when it was present.
    /// Switches this object to a **fresh, never-shared shape** so
    /// the surviving siblings on the original shape don't pay the
    /// cost of the delete.
    pub fn delete(&self, key: &str) -> bool {
        let mut body = self.inner.borrow_mut();
        let Some(offset) = body.shape.offset_of(key) else {
            return false;
        };
        let mut new_keys = body.shape.keys.clone();
        new_keys.remove(offset as usize);
        body.slots.remove(offset as usize);
        body.shape = Rc::new(Shape {
            parent: None,
            key: None,
            keys: new_keys,
            offsets: RefCell::new(None),
            transitions: RefCell::new(HashMap::new()),
        });
        true
    }

    /// Look up an **own** symbol-keyed property. Identity comparison
    /// uses [`JsSymbol::ptr_eq`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-ordinarygetownproperty>
    #[must_use]
    pub fn get_own_symbol(&self, key: &JsSymbol) -> Option<Value> {
        self.inner
            .borrow()
            .symbol_props
            .iter()
            .find(|(k, _)| k.ptr_eq(key))
            .map(|(_, v)| v.clone())
    }

    /// Look up a symbol-keyed property, walking the prototype chain
    /// on miss. Bounded by [`PROTO_CHAIN_HARD_CAP`].
    #[must_use]
    pub fn get_symbol(&self, key: &JsSymbol) -> Option<Value> {
        if let Some(v) = self.get_own_symbol(key) {
            return Some(v);
        }
        let mut current = self.prototype();
        let mut hops = 0;
        while let Some(proto) = current {
            if hops >= PROTO_CHAIN_HARD_CAP {
                return None;
            }
            hops += 1;
            if let Some(v) = proto.get_own_symbol(key) {
                return Some(v);
            }
            current = proto.prototype();
        }
        None
    }

    /// Set or overwrite a symbol-keyed own property. Existing key
    /// (matched by identity) is updated in place; otherwise a fresh
    /// entry is appended.
    pub fn set_symbol(&self, key: JsSymbol, value: Value) {
        let mut body = self.inner.borrow_mut();
        for (existing_key, slot) in body.symbol_props.iter_mut() {
            if existing_key.ptr_eq(&key) {
                *slot = value;
                return;
            }
        }
        body.symbol_props.push((key, value));
    }

    /// Remove a symbol-keyed own property. Returns `true` when the
    /// entry was present.
    pub fn delete_symbol(&self, key: &JsSymbol) -> bool {
        let mut body = self.inner.borrow_mut();
        if let Some(pos) = body.symbol_props.iter().position(|(k, _)| k.ptr_eq(key)) {
            body.symbol_props.remove(pos);
            true
        } else {
            false
        }
    }

    /// Borrow a read-only view of the property table.
    #[must_use]
    pub fn borrow_props(&self) -> Properties<'_> {
        Properties {
            body: self.inner.borrow(),
        }
    }

    /// Borrow a mutable view (rarely needed outside the VM core).
    #[must_use]
    pub fn borrow_props_mut(&self) -> PropertiesMut<'_> {
        PropertiesMut {
            body: self.inner.borrow_mut(),
        }
    }

    /// Identity comparison — true iff both handles wrap the same
    /// heap object.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }

    /// Raw `Rc` data-pointer for use as a hash key in cycle
    /// detection (`JSON.stringify`, structuredClone). Anchor the
    /// originating handle for the lifetime of the pointer — it
    /// dangles once the last `Rc` is dropped.
    #[must_use]
    pub fn identity_addr(&self) -> *const () {
        Rc::as_ptr(&self.inner).cast()
    }

    /// Borrow the current hidden class. Used by debug / shape-
    /// identity tests; production code should not introspect it.
    #[must_use]
    pub fn shape(&self) -> Rc<Shape> {
        Rc::clone(&self.inner.borrow().shape)
    }
}

impl Default for JsObject {
    fn default() -> Self {
        let shape = ROOT_SHAPE.with(Rc::clone);
        Self {
            inner: Rc::new(RefCell::new(ObjectBody {
                shape,
                slots: SmallVec::new(),
                prototype: None,
                symbol_props: Vec::new(),
            })),
        }
    }
}

/// Read-only iteration over an object's properties in insertion
/// order. Used by debug rendering, future JSON serialization, and
/// `Object.keys` enumeration.
pub struct Properties<'a> {
    body: Ref<'a, ObjectBody>,
}

impl<'a> Properties<'a> {
    /// Iterate `(key, value)` pairs in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.body
            .shape
            .keys()
            .zip(self.body.slots.iter())
            .map(|(k, v)| (k.as_str(), v))
    }

    /// Iterate keys in insertion order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.body.shape.keys().map(String::as_str)
    }
}

/// Mutable view; rarely used outside the VM core.
pub struct PropertiesMut<'a> {
    #[allow(dead_code)]
    body: RefMut<'a, ObjectBody>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_object_starts_with_zero_props() {
        let o = JsObject::new();
        assert!(o.is_empty());
        assert_eq!(o.len(), 0);
    }

    #[test]
    fn set_then_get_roundtrip() {
        let o = JsObject::new();
        o.set("x", Value::Boolean(true));
        assert_eq!(o.get("x"), Some(Value::Boolean(true)));
    }

    #[test]
    fn missing_key_is_none() {
        let o = JsObject::new();
        assert!(o.get("missing").is_none());
    }

    #[test]
    fn insertion_order_is_preserved() {
        let o = JsObject::new();
        o.set("a", Value::Boolean(true));
        o.set("b", Value::Boolean(false));
        o.set("c", Value::Null);
        let props = o.borrow_props();
        let keys: Vec<&str> = props.keys().collect();
        assert_eq!(keys, vec!["a", "b", "c"]);
    }

    #[test]
    fn delete_removes_property() {
        let o = JsObject::new();
        o.set("x", Value::Boolean(true));
        assert!(o.delete("x"));
        assert!(o.get("x").is_none());
        assert!(!o.delete("x"));
    }

    #[test]
    fn cloning_shares_storage() {
        let a = JsObject::new();
        let b = a.clone();
        a.set("x", Value::Boolean(true));
        assert!(b.ptr_eq(&a));
        assert_eq!(b.get("x"), Some(Value::Boolean(true)));
    }

    #[test]
    fn two_literals_share_shape() {
        // Build two objects with the same key sequence; their
        // shapes must be the **same** Rc instance.
        let a = JsObject::new();
        a.set("x", Value::Boolean(true));
        a.set("y", Value::Null);
        let b = JsObject::new();
        b.set("x", Value::Boolean(false));
        b.set("y", Value::Undefined);
        assert!(Rc::ptr_eq(&a.shape(), &b.shape()));
        // Different key order → different shape.
        let c = JsObject::new();
        c.set("y", Value::Null);
        c.set("x", Value::Boolean(true));
        assert!(!Rc::ptr_eq(&a.shape(), &c.shape()));
    }

    #[test]
    fn overwrite_does_not_grow_shape() {
        let o = JsObject::new();
        o.set("x", Value::Boolean(true));
        let s1 = o.shape();
        o.set("x", Value::Null);
        let s2 = o.shape();
        assert!(Rc::ptr_eq(&s1, &s2));
        assert_eq!(o.len(), 1);
    }

    #[test]
    fn delete_switches_to_dictionary_shape() {
        let o = JsObject::new();
        o.set("a", Value::Boolean(true));
        o.set("b", Value::Null);
        let before = o.shape();
        o.delete("a");
        let after = o.shape();
        // The post-delete shape is fresh — the cached transition
        // tree must not be reused.
        assert!(!Rc::ptr_eq(&before, &after));
        assert_eq!(o.len(), 1);
        assert!(o.get("a").is_none());
        assert_eq!(o.get("b"), Some(Value::Null));
    }
}
