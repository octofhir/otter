//! JavaScript object value with hidden-class shape storage and
//! ECMA-262 §6.1.7.1 property descriptors.
//!
//! Each property carries the canonical attribute triple
//! `(writable, enumerable, configurable)` plus a body that is either
//! a `[[Value]]` (data property) or a `([[Get]], [[Set]])` accessor
//! pair. The `Shape`-based hidden-class model stays — keys are still
//! shared across literals — and a per-slot [`PropertySlot`] now sits
//! alongside the `Value` slot the foundation used in earlier slices.
//!
//! # Contents
//! - [`PropertyFlags`] — packed `(writable, enumerable, configurable)`
//!   bitfield.
//! - [`PropertyDescriptor`] / [`DescriptorKind`] — public descriptor
//!   surface used by `Object.defineProperty` and friends.
//! - [`PropertyLookup`] — the result of an own-property probe (data
//!   value, accessor descriptor, or absent).
//! - [`SetOutcome`] — what the runtime should do after a property
//!   write resolved through the prototype chain (write data, invoke
//!   setter, or reject).
//! - [`JsObject`] / [`Shape`] / [`Properties`] — the public object
//!   handle, its hidden class, and the read-only view used by JSON
//!   serialisation and `Object.keys` enumeration.
//!
//! # Invariants
//! - Insertion order is encoded by the shape's key vector and shared
//!   between `Shape` and the slot table (slot at offset `i` describes
//!   the key at `keys[i]`).
//! - A frozen object's slots all carry `writable = false` (data) and
//!   `configurable = false`; in addition the object is non-extensible.
//! - A sealed object's slots all carry `configurable = false` and the
//!   object is non-extensible (writable may still be true).
//! - Accessor descriptors never carry a `writable` bit — its slot is
//!   reused as a discriminator (always `false`).
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-property-attributes>
//! - <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots>
//! - <https://tc39.es/ecma262/#sec-ordinarydefineownproperty>
//! - <https://tc39.es/ecma262/#sec-ordinaryset>

use std::cell::{Ref, RefCell, RefMut};
use std::collections::HashMap;
use std::rc::Rc;

use smallvec::SmallVec;

use crate::Value;
use crate::string::JsString;
use crate::symbol::JsSymbol;

// ---------- property attribute flags ---------------------------------------

/// Packed `(writable, enumerable, configurable)` bitfield. Stored as
/// a single byte alongside each slot.
///
/// # See also
/// - <https://tc39.es/ecma262/#table-default-attributes>
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct PropertyFlags(u8);

impl PropertyFlags {
    /// `[[Writable]]` bit.
    pub const WRITABLE: u8 = 0b0001;
    /// `[[Enumerable]]` bit.
    pub const ENUMERABLE: u8 = 0b0010;
    /// `[[Configurable]]` bit.
    pub const CONFIGURABLE: u8 = 0b0100;

    /// All three attributes set — the default for an object-literal
    /// data property created by source like `{ x: 1 }`.
    #[must_use]
    pub const fn data_default() -> Self {
        Self(Self::WRITABLE | Self::ENUMERABLE | Self::CONFIGURABLE)
    }

    /// Every attribute clear — the default `Object.defineProperty`
    /// shape per §10.1.6.3 (`writable / enumerable / configurable`
    /// each default to `false` when absent from the supplied
    /// descriptor).
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Build flags from individual bits.
    #[must_use]
    pub const fn new(writable: bool, enumerable: bool, configurable: bool) -> Self {
        let mut bits = 0u8;
        if writable {
            bits |= Self::WRITABLE;
        }
        if enumerable {
            bits |= Self::ENUMERABLE;
        }
        if configurable {
            bits |= Self::CONFIGURABLE;
        }
        Self(bits)
    }

    /// `true` when the `[[Writable]]` bit is set.
    #[must_use]
    pub const fn writable(self) -> bool {
        self.0 & Self::WRITABLE != 0
    }

    /// `true` when the `[[Enumerable]]` bit is set.
    #[must_use]
    pub const fn enumerable(self) -> bool {
        self.0 & Self::ENUMERABLE != 0
    }

    /// `true` when the `[[Configurable]]` bit is set.
    #[must_use]
    pub const fn configurable(self) -> bool {
        self.0 & Self::CONFIGURABLE != 0
    }

    /// Build a fresh value with `[[Writable]]` overridden.
    #[must_use]
    pub fn with_writable(mut self, value: bool) -> Self {
        if value {
            self.0 |= Self::WRITABLE;
        } else {
            self.0 &= !Self::WRITABLE;
        }
        self
    }

    /// Build a fresh value with `[[Enumerable]]` overridden.
    #[must_use]
    pub fn with_enumerable(mut self, value: bool) -> Self {
        if value {
            self.0 |= Self::ENUMERABLE;
        } else {
            self.0 &= !Self::ENUMERABLE;
        }
        self
    }

    /// Build a fresh value with `[[Configurable]]` overridden.
    #[must_use]
    pub fn with_configurable(mut self, value: bool) -> Self {
        if value {
            self.0 |= Self::CONFIGURABLE;
        } else {
            self.0 &= !Self::CONFIGURABLE;
        }
        self
    }
}

// ---------- public descriptor type ----------------------------------------

/// One property descriptor — either a data property with a stored
/// value or an accessor pair.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-property-descriptor-specification-type>
#[derive(Debug, Clone)]
pub struct PropertyDescriptor {
    /// Body — data slot or accessor pair.
    pub kind: DescriptorKind,
    /// Attribute flags. The `[[Writable]]` bit is meaningful only
    /// when [`kind`](Self::kind) is [`DescriptorKind::Data`]; for
    /// accessors it is ignored.
    pub flags: PropertyFlags,
}

/// Body of a [`PropertyDescriptor`].
#[derive(Debug, Clone)]
pub enum DescriptorKind {
    /// Data property — stores the value directly.
    Data {
        /// Stored value.
        value: Value,
    },
    /// Accessor property — the runtime invokes the relevant function
    /// on read (`getter`) and write (`setter`).
    Accessor {
        /// `Some(callable)` for a `[[Get]]` slot, `None` when absent.
        getter: Option<Value>,
        /// `Some(callable)` for a `[[Set]]` slot, `None` when absent.
        setter: Option<Value>,
    },
}

impl PropertyDescriptor {
    /// Build a data descriptor.
    #[must_use]
    pub fn data(value: Value, writable: bool, enumerable: bool, configurable: bool) -> Self {
        Self {
            kind: DescriptorKind::Data { value },
            flags: PropertyFlags::new(writable, enumerable, configurable),
        }
    }

    /// Build an accessor descriptor.
    #[must_use]
    pub fn accessor(
        getter: Option<Value>,
        setter: Option<Value>,
        enumerable: bool,
        configurable: bool,
    ) -> Self {
        Self {
            kind: DescriptorKind::Accessor { getter, setter },
            // accessor flags never carry the writable bit
            flags: PropertyFlags::new(false, enumerable, configurable),
        }
    }

    /// `true` when this is a data descriptor.
    #[must_use]
    pub fn is_data(&self) -> bool {
        matches!(self.kind, DescriptorKind::Data { .. })
    }

    /// `true` when this is an accessor descriptor.
    #[must_use]
    pub fn is_accessor(&self) -> bool {
        matches!(self.kind, DescriptorKind::Accessor { .. })
    }

    /// Convenience: `[[Configurable]]` bit.
    #[must_use]
    pub fn configurable(&self) -> bool {
        self.flags.configurable()
    }

    /// Convenience: `[[Enumerable]]` bit.
    #[must_use]
    pub fn enumerable(&self) -> bool {
        self.flags.enumerable()
    }

    /// Convenience: `[[Writable]]` bit (meaningful only on data
    /// descriptors).
    #[must_use]
    pub fn writable(&self) -> bool {
        self.flags.writable()
    }
}

/// Result of an own-property probe.
#[derive(Debug, Clone)]
pub enum PropertyLookup {
    /// No own property of that key exists.
    Absent,
    /// Data property — the stored value plus its attribute flags.
    Data {
        /// Stored value.
        value: Value,
        /// Attribute flags.
        flags: PropertyFlags,
    },
    /// Accessor property.
    Accessor {
        /// `[[Get]]` slot, if any.
        getter: Option<Value>,
        /// `[[Set]]` slot, if any.
        setter: Option<Value>,
        /// Attribute flags. The writable bit is meaningless here.
        flags: PropertyFlags,
    },
}

/// What the runtime should do after `[[Set]]` resolves through the
/// prototype chain (§10.1.9 OrdinarySet).
#[derive(Debug, Clone)]
pub enum SetOutcome {
    /// The own / inherited slot is a writable data slot. The runtime
    /// should write `value` into the receiver as a data property.
    AssignData,
    /// An accessor with a setter was found. The runtime should call
    /// `setter(value)` with `this = receiver`.
    InvokeSetter {
        /// The setter callable.
        setter: Value,
    },
    /// The set must be rejected — non-writable data, accessor with no
    /// setter, or the receiver is non-extensible and the property is
    /// missing. In sloppy mode this is silently dropped; in strict
    /// mode it would surface as a `TypeError`.
    Reject {
        /// Stable rejection reason (used by future strict-mode wiring).
        reason: SetRejectReason,
    },
}

/// Why a `[[Set]]` was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SetRejectReason {
    /// Existing data property is non-writable.
    NonWritable,
    /// Accessor descriptor has no `[[Set]]`.
    AccessorWithoutSetter,
    /// Receiver is non-extensible and the property is absent.
    NonExtensible,
}

// ---------- internal slot type --------------------------------------------

/// Slot stored alongside each shape key. Mirrors the public
/// [`PropertyDescriptor`] layout.
#[derive(Debug, Clone)]
struct PropertySlot {
    flags: PropertyFlags,
    body: SlotBody,
}

#[derive(Debug, Clone)]
enum SlotBody {
    Data {
        value: Value,
    },
    Accessor {
        getter: Option<Value>,
        setter: Option<Value>,
    },
}

impl PropertySlot {
    fn data_default(value: Value) -> Self {
        Self {
            flags: PropertyFlags::data_default(),
            body: SlotBody::Data { value },
        }
    }

    fn from_descriptor(desc: PropertyDescriptor) -> Self {
        Self {
            flags: desc.flags,
            body: match desc.kind {
                DescriptorKind::Data { value } => SlotBody::Data { value },
                DescriptorKind::Accessor { getter, setter } => {
                    SlotBody::Accessor { getter, setter }
                }
            },
        }
    }

    fn to_descriptor(&self) -> PropertyDescriptor {
        PropertyDescriptor {
            flags: self.flags,
            kind: match &self.body {
                SlotBody::Data { value } => DescriptorKind::Data {
                    value: value.clone(),
                },
                SlotBody::Accessor { getter, setter } => DescriptorKind::Accessor {
                    getter: getter.clone(),
                    setter: setter.clone(),
                },
            },
        }
    }

    fn to_lookup(&self) -> PropertyLookup {
        match &self.body {
            SlotBody::Data { value } => PropertyLookup::Data {
                value: value.clone(),
                flags: self.flags,
            },
            SlotBody::Accessor { getter, setter } => PropertyLookup::Accessor {
                getter: getter.clone(),
                setter: setter.clone(),
                flags: self.flags,
            },
        }
    }
}

// ---------- shape (hidden class) ------------------------------------------

/// A hidden-class node. Shapes form a tree rooted at the empty
/// shape; each non-root shape records the parent plus the single
/// key added to reach it.
#[derive(Debug)]
pub struct Shape {
    #[allow(dead_code)]
    parent: Option<Rc<Shape>>,
    #[allow(dead_code)]
    key: Option<String>,
    keys: Vec<String>,
    offsets: RefCell<Option<HashMap<String, u16>>>,
    transitions: RefCell<HashMap<String, Rc<Shape>>>,
}

impl Shape {
    /// Construct the root (empty) shape.
    #[must_use]
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
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// `true` for the empty (root) shape.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Look up a property's slot offset.
    #[must_use]
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
    #[must_use]
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
    /// Cheap shared root for empty objects.
    static ROOT_SHAPE: Rc<Shape> = Shape::root();
}

// ---------- JsObject ------------------------------------------------------

/// Heap-shared object handle. Cloning shares storage.
#[derive(Debug, Clone)]
pub struct JsObject {
    inner: Rc<RefCell<ObjectBody>>,
}

#[derive(Debug)]
struct ObjectBody {
    shape: Rc<Shape>,
    slots: SmallVec<[PropertySlot; 4]>,
    prototype: Option<JsObject>,
    /// Symbol-keyed own data properties. Symbol-keyed accessors are
    /// not modelled in the foundation slice — `Object.defineProperty`
    /// only accepts string keys today.
    symbol_props: Vec<(JsSymbol, Value)>,
    /// `[[Extensible]]` internal slot. New keys are rejected when
    /// this is `false`. Toggled by
    /// [`JsObject::prevent_extensions`] / [`JsObject::seal`] /
    /// [`JsObject::freeze`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-ordinarypreventextensions>
    extensible: bool,
}

/// Maximum prototype-chain hops a property lookup will follow.
pub const PROTO_CHAIN_HARD_CAP: usize = 1024;

impl JsObject {
    /// Allocate a fresh empty extensible object on the root shape.
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

    /// Set or overwrite an own property as a default-attributes data
    /// slot (`writable / enumerable / configurable` all `true`).
    /// This is the construction-time path used by object literals,
    /// runtime intrinsics, and prototype scaffolding — it bypasses
    /// the §10.1.9 [[Set]] ladder entirely.
    ///
    /// # Algorithm
    /// 1. If the key already lives on this object, overwrite the
    ///    slot's value, preserving the slot's existing flags. This
    ///    matches the `O[k] = v` shape for an existing data property
    ///    that has not been re-configured by `defineProperty`.
    /// 2. Otherwise, append a new default-attributes data slot.
    ///
    /// Construction-time callers do not respect the extensibility
    /// flag: this path is only used by code that owns the object and
    /// is allowed to seed it (`Error.prototype.message`, etc.).
    pub fn set(&self, key: &str, value: Value) {
        let mut body = self.inner.borrow_mut();
        if let Some(offset) = body.shape.offset_of(key) {
            // Preserve flags but write into the data body.
            let slot = &mut body.slots[offset as usize];
            slot.body = SlotBody::Data { value };
            return;
        }
        let new_shape = Shape::add_property(&body.shape, key);
        body.shape = new_shape;
        body.slots.push(PropertySlot::data_default(value));
    }

    /// Read an **own** property with an accessor short-circuit:
    /// returns `Some(value)` for data slots, `Some(undefined)` for
    /// accessor slots (callers that need to invoke the getter must
    /// use [`Self::lookup_own`] / [`Self::get_own_descriptor`]).
    #[must_use]
    pub fn get_own(&self, key: &str) -> Option<Value> {
        let body = self.inner.borrow();
        body.shape
            .offset_of(key)
            .map(|offset| match &body.slots[offset as usize].body {
                SlotBody::Data { value } => value.clone(),
                SlotBody::Accessor { .. } => Value::Undefined,
            })
    }

    /// Read a property, walking the prototype chain on miss.
    /// Accessors collapse to `undefined` here for backward-compat
    /// with construction-time call sites; the dispatch loop's
    /// `LoadProperty` handler invokes accessors through
    /// [`Self::lookup`] instead.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Value> {
        match self.lookup(key) {
            PropertyLookup::Absent => None,
            PropertyLookup::Data { value, .. } => Some(value),
            PropertyLookup::Accessor { .. } => Some(Value::Undefined),
        }
    }

    /// Probe for an own property (no proto-chain walk). The result
    /// distinguishes data, accessor, and absent.
    #[must_use]
    pub fn lookup_own(&self, key: &str) -> PropertyLookup {
        let body = self.inner.borrow();
        match body.shape.offset_of(key) {
            Some(offset) => body.slots[offset as usize].to_lookup(),
            None => PropertyLookup::Absent,
        }
    }

    /// Probe for a property with full prototype-chain walk. Returns
    /// the first hit's descriptor body; useful for the LoadProperty
    /// dispatch path which needs to know whether to invoke a getter
    /// at any depth.
    #[must_use]
    pub fn lookup(&self, key: &str) -> PropertyLookup {
        match self.lookup_own(key) {
            PropertyLookup::Absent => {}
            hit => return hit,
        }
        let mut current = self.prototype();
        let mut hops = 0;
        while let Some(proto) = current {
            if hops >= PROTO_CHAIN_HARD_CAP {
                return PropertyLookup::Absent;
            }
            hops += 1;
            match proto.lookup_own(key) {
                PropertyLookup::Absent => {}
                hit => return hit,
            }
            current = proto.prototype();
        }
        PropertyLookup::Absent
    }

    /// Read the descriptor for an own property.
    #[must_use]
    pub fn get_own_descriptor(&self, key: &str) -> Option<PropertyDescriptor> {
        let body = self.inner.borrow();
        body.shape
            .offset_of(key)
            .map(|offset| body.slots[offset as usize].to_descriptor())
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

    /// Remove an own property. Per ECMA-262 §10.1.10 OrdinaryDelete:
    /// returns `true` when the property is absent or successfully
    /// removed; returns `false` only when the property exists and is
    /// non-configurable.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-ordinarydelete>
    pub fn delete(&self, key: &str) -> bool {
        let mut body = self.inner.borrow_mut();
        let Some(offset) = body.shape.offset_of(key) else {
            // Spec step 2: missing → true.
            return true;
        };
        if !body.slots[offset as usize].flags.configurable() {
            return false;
        }
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

    /// Look up an **own** symbol-keyed property.
    #[must_use]
    pub fn get_own_symbol(&self, key: &JsSymbol) -> Option<Value> {
        self.inner
            .borrow()
            .symbol_props
            .iter()
            .find(|(k, _)| k.ptr_eq(key))
            .map(|(_, v)| v.clone())
    }

    /// Look up a symbol-keyed property with prototype-chain walk.
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

    /// Set or overwrite a symbol-keyed own property.
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

    /// Remove a symbol-keyed own property.
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

    /// Identity comparison.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }

    /// Raw `Rc` data-pointer for use as a hash key in cycle detection.
    #[must_use]
    pub fn identity_addr(&self) -> *const () {
        Rc::as_ptr(&self.inner).cast()
    }

    /// Borrow the current hidden class.
    #[must_use]
    pub fn shape(&self) -> Rc<Shape> {
        Rc::clone(&self.inner.borrow().shape)
    }

    // ---------- descriptor surface ----------------------------------------

    /// `Object.defineProperty` core — performs §10.1.6
    /// OrdinaryDefineOwnProperty, returning `true` on success and
    /// `false` when the request is rejected (non-configurable
    /// re-definition, etc.).
    ///
    /// # Algorithm
    /// Per ECMA-262 §10.1.6.3 ValidateAndApplyPropertyDescriptor:
    /// 1. If the property is absent and the object is non-extensible
    ///    return `false`.
    /// 2. If absent and extensible, install the descriptor (filling
    ///    in default attribute bits with `false`).
    /// 3. If present, validate against the existing descriptor:
    ///    - Same descriptor → no-op success.
    ///    - Existing non-configurable rejects: configurable→true,
    ///      enumerable change, kind change, or (data) writable→true
    ///      / value change while non-writable.
    ///    - Otherwise overwrite the slot with the merged result of
    ///      the supplied + existing descriptors.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-ordinarydefineownproperty>
    /// - <https://tc39.es/ecma262/#sec-validateandapplypropertydescriptor>
    pub fn define_own_property(&self, key: &str, descriptor: PropertyDescriptor) -> bool {
        let mut body = self.inner.borrow_mut();
        if let Some(offset) = body.shape.offset_of(key) {
            let existing = body.slots[offset as usize].clone();
            match validate_and_apply(&existing, &descriptor) {
                Some(merged) => {
                    body.slots[offset as usize] = merged;
                    true
                }
                None => false,
            }
        } else {
            if !body.extensible {
                return false;
            }
            let new_shape = Shape::add_property(&body.shape, key);
            body.shape = new_shape;
            body.slots.push(PropertySlot::from_descriptor(descriptor));
            true
        }
    }

    /// Resolve a `[[Set]]` against this receiver — walks the
    /// prototype chain to detect inherited accessors and
    /// non-writable shadows, but writes happen on `self` (the
    /// receiver) only. Per §10.1.9 OrdinarySet.
    ///
    /// Returns a [`SetOutcome`] describing the action the dispatch
    /// loop should take.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-ordinaryset>
    /// - <https://tc39.es/ecma262/#sec-ordinarysetwithowndescriptor>
    pub fn resolve_set(&self, key: &str) -> SetOutcome {
        // Walk own + prototype chain looking for an accessor or a
        // non-writable shadow.
        let own = self.lookup_own(key);
        match own {
            PropertyLookup::Data { flags, .. } => {
                if flags.writable() {
                    return SetOutcome::AssignData;
                }
                return SetOutcome::Reject {
                    reason: SetRejectReason::NonWritable,
                };
            }
            PropertyLookup::Accessor { setter, .. } => {
                return match setter {
                    Some(setter) => SetOutcome::InvokeSetter { setter },
                    None => SetOutcome::Reject {
                        reason: SetRejectReason::AccessorWithoutSetter,
                    },
                };
            }
            PropertyLookup::Absent => {}
        }
        // Walk prototype chain.
        let mut current = self.prototype();
        let mut hops = 0;
        while let Some(proto) = current {
            if hops >= PROTO_CHAIN_HARD_CAP {
                break;
            }
            hops += 1;
            match proto.lookup_own(key) {
                PropertyLookup::Data { flags, .. } => {
                    if flags.writable() {
                        // Inherited writable data — receiver gets a
                        // shadow; honour receiver extensibility.
                        if !self.is_extensible() {
                            return SetOutcome::Reject {
                                reason: SetRejectReason::NonExtensible,
                            };
                        }
                        return SetOutcome::AssignData;
                    }
                    return SetOutcome::Reject {
                        reason: SetRejectReason::NonWritable,
                    };
                }
                PropertyLookup::Accessor { setter, .. } => {
                    return match setter {
                        Some(setter) => SetOutcome::InvokeSetter { setter },
                        None => SetOutcome::Reject {
                            reason: SetRejectReason::AccessorWithoutSetter,
                        },
                    };
                }
                PropertyLookup::Absent => {}
            }
            current = proto.prototype();
        }
        // Nothing on the chain — install a fresh data slot.
        if !self.is_extensible() {
            return SetOutcome::Reject {
                reason: SetRejectReason::NonExtensible,
            };
        }
        SetOutcome::AssignData
    }

    /// `[[IsExtensible]]` — `false` after
    /// [`Self::prevent_extensions`] / [`Self::seal`] / [`Self::freeze`].
    #[must_use]
    pub fn is_extensible(&self) -> bool {
        self.inner.borrow().extensible
    }

    /// `Object.preventExtensions(o)` core — clears the
    /// `[[Extensible]]` slot. Always succeeds for ordinary objects.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-ordinarypreventextensions>
    pub fn prevent_extensions(&self) {
        self.inner.borrow_mut().extensible = false;
    }

    /// `Object.seal(o)` core — clears `[[Extensible]]` and toggles
    /// `[[Configurable]]` to `false` on every own property.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-setintegritylevel>
    pub fn seal(&self) {
        let mut body = self.inner.borrow_mut();
        body.extensible = false;
        for slot in body.slots.iter_mut() {
            slot.flags = slot.flags.with_configurable(false);
        }
    }

    /// `Object.freeze(o)` core — clears `[[Extensible]]`, then for
    /// every own property: data slots become non-writable and
    /// non-configurable; accessor slots become non-configurable.
    pub fn freeze(&self) {
        let mut body = self.inner.borrow_mut();
        body.extensible = false;
        for slot in body.slots.iter_mut() {
            slot.flags = slot.flags.with_configurable(false);
            if matches!(slot.body, SlotBody::Data { .. }) {
                slot.flags = slot.flags.with_writable(false);
            }
        }
    }

    /// `Object.isSealed(o)` — `true` when the object is
    /// non-extensible and every own property is non-configurable.
    #[must_use]
    pub fn is_sealed(&self) -> bool {
        let body = self.inner.borrow();
        if body.extensible {
            return false;
        }
        body.slots.iter().all(|s| !s.flags.configurable())
    }

    /// `Object.isFrozen(o)` — `true` when the object is sealed and
    /// every data slot is non-writable.
    #[must_use]
    pub fn is_frozen(&self) -> bool {
        let body = self.inner.borrow();
        if body.extensible {
            return false;
        }
        for slot in body.slots.iter() {
            if slot.flags.configurable() {
                return false;
            }
            if let SlotBody::Data { .. } = slot.body {
                if slot.flags.writable() {
                    return false;
                }
            }
        }
        true
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
                extensible: true,
            })),
        }
    }
}

// ---------- ValidateAndApplyPropertyDescriptor ----------------------------

/// Implements §10.1.6.3 ValidateAndApplyPropertyDescriptor for an
/// existing slot. Returns `Some(updated)` on success, `None` to
/// reject.
fn validate_and_apply(
    existing: &PropertySlot,
    incoming: &PropertyDescriptor,
) -> Option<PropertySlot> {
    let existing_kind_is_data = matches!(existing.body, SlotBody::Data { .. });
    let incoming_kind_is_data = matches!(incoming.kind, DescriptorKind::Data { .. });

    // 4.a: every field of `incoming` is identical to `existing` →
    // no-op success. Skipped for simplicity — we always apply.

    if !existing.flags.configurable() {
        // 4.b: configurable cannot transition to true.
        if incoming.flags.configurable() && !existing.flags.configurable() {
            return None;
        }
        // 4.c: enumerable cannot change.
        if incoming.flags.enumerable() != existing.flags.enumerable() {
            return None;
        }
        // 4.d: kind cannot change (data ↔ accessor).
        if existing_kind_is_data != incoming_kind_is_data {
            return None;
        }
        // 4.e: data with non-writable rejects writable→true / value change.
        if existing_kind_is_data {
            if !existing.flags.writable() {
                if incoming.flags.writable() {
                    return None;
                }
                if let DescriptorKind::Data { value: incoming_v } = &incoming.kind {
                    if let SlotBody::Data { value: existing_v } = &existing.body {
                        if !same_value(existing_v, incoming_v) {
                            return None;
                        }
                    }
                }
            }
        } else {
            // 4.f: accessor — get / set cannot change.
            if let DescriptorKind::Accessor {
                getter: in_get,
                setter: in_set,
            } = &incoming.kind
            {
                if let SlotBody::Accessor {
                    getter: ex_get,
                    setter: ex_set,
                } = &existing.body
                {
                    if !optional_value_eq(ex_get, in_get) || !optional_value_eq(ex_set, in_set) {
                        return None;
                    }
                }
            }
        }
    }

    // Build merged slot.
    Some(PropertySlot::from_descriptor(PropertyDescriptor {
        flags: incoming.flags,
        kind: incoming.kind.clone(),
    }))
}

fn optional_value_eq(a: &Option<Value>, b: &Option<Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => same_value(x, y),
        _ => false,
    }
}

/// Light SameValue check — distinguishes objects by identity, falls
/// back to display-string equality for primitives. Used only by the
/// validator above; the canonical SameValue lives in
/// [`crate::abstract_ops::same_value`] but pulling that in here would
/// create an awkward cycle for the slice scope.
fn same_value(a: &Value, b: &Value) -> bool {
    crate::abstract_ops::same_value(a, b)
}

// ---------- read-only iteration ------------------------------------------

/// Read-only iteration over an object's properties in insertion
/// order. Used by debug rendering, JSON serialisation, and
/// `Object.keys`.
pub struct Properties<'a> {
    body: Ref<'a, ObjectBody>,
}

impl<'a> Properties<'a> {
    /// Iterate every `(key, data-value)` pair in insertion order,
    /// regardless of enumerability. Accessor slots are surfaced as
    /// the sentinel `Value::Undefined` — callers that need accessor
    /// fidelity must consult `JsObject::get_own_descriptor` directly.
    pub fn iter(&self) -> impl Iterator<Item = (&str, Value)> {
        self.body
            .shape
            .keys()
            .zip(self.body.slots.iter())
            .map(|(k, slot)| {
                let value = match &slot.body {
                    SlotBody::Data { value } => value.clone(),
                    SlotBody::Accessor { .. } => Value::Undefined,
                };
                (k.as_str(), value)
            })
    }

    /// Iterate keys in insertion order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.body.shape.keys().map(String::as_str)
    }

    /// Iterate symbol-keyed own properties in insertion order.
    /// Used by `Object.getOwnPropertySymbols` (§20.1.2.13) and
    /// `Reflect.ownKeys` (§28.1.16) to surface symbol keys.
    pub fn symbol_keys(&self) -> impl Iterator<Item = JsSymbol> + '_ {
        self.body.symbol_props.iter().map(|(k, _)| k.clone())
    }

    /// Iterate `(key, data-value)` pairs, skipping accessor and
    /// non-enumerable slots. Used by JSON.stringify and `for…in`
    /// once it lands.
    pub fn enumerable_data_iter(&self) -> impl Iterator<Item = (&str, Value)> {
        self.body
            .shape
            .keys()
            .zip(self.body.slots.iter())
            .filter_map(|(k, slot)| {
                if !slot.flags.enumerable() {
                    return None;
                }
                match &slot.body {
                    SlotBody::Data { value } => Some((k.as_str(), value.clone())),
                    SlotBody::Accessor { .. } => None,
                }
            })
    }

    /// Iterate enumerable own-key names (string-keyed only).
    pub fn enumerable_keys(&self) -> impl Iterator<Item = &str> {
        self.body
            .shape
            .keys()
            .zip(self.body.slots.iter())
            .filter_map(|(k, slot)| slot.flags.enumerable().then_some(k.as_str()))
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
        // §10.1.10 — deleting a missing property still reports
        // success (returns true).
        assert!(o.delete("x"));
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
        let a = JsObject::new();
        a.set("x", Value::Boolean(true));
        a.set("y", Value::Null);
        let b = JsObject::new();
        b.set("x", Value::Boolean(false));
        b.set("y", Value::Undefined);
        assert!(Rc::ptr_eq(&a.shape(), &b.shape()));
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
        assert!(!Rc::ptr_eq(&before, &after));
        assert_eq!(o.len(), 1);
        assert!(o.get("a").is_none());
        assert_eq!(o.get("b"), Some(Value::Null));
    }

    #[test]
    fn define_property_with_default_attrs() {
        let o = JsObject::new();
        let desc = PropertyDescriptor::data(Value::Boolean(true), false, false, false);
        assert!(o.define_own_property("x", desc));
        let got = o.get_own_descriptor("x").unwrap();
        assert!(got.is_data());
        assert!(!got.writable());
        assert!(!got.enumerable());
        assert!(!got.configurable());
    }

    #[test]
    fn define_property_rejects_non_configurable_kind_change() {
        let o = JsObject::new();
        o.define_own_property(
            "x",
            PropertyDescriptor::data(Value::Boolean(true), true, true, false),
        );
        // Try to switch the data slot to an accessor — must fail.
        let accessor = PropertyDescriptor::accessor(None, None, true, false);
        assert!(!o.define_own_property("x", accessor));
    }

    #[test]
    fn freeze_makes_object_non_writable() {
        let o = JsObject::new();
        o.set("x", Value::Boolean(true));
        o.freeze();
        assert!(o.is_frozen());
        assert!(o.is_sealed());
        assert!(!o.is_extensible());
        // `set` is the construction-time path that doesn't honour
        // attribute flags, so it doesn't apply here. The dispatch
        // layer reaches this through `resolve_set`.
        match o.resolve_set("x") {
            SetOutcome::Reject {
                reason: SetRejectReason::NonWritable,
            } => {}
            other => panic!("expected NonWritable rejection, got {other:?}"),
        }
    }

    #[test]
    fn seal_blocks_new_properties() {
        let o = JsObject::new();
        o.set("a", Value::Null);
        o.seal();
        assert!(o.is_sealed());
        assert!(!o.is_frozen());
        match o.resolve_set("b") {
            SetOutcome::Reject {
                reason: SetRejectReason::NonExtensible,
            } => {}
            other => panic!("expected NonExtensible rejection, got {other:?}"),
        }
    }

    #[test]
    fn delete_respects_configurable() {
        let o = JsObject::new();
        o.define_own_property(
            "x",
            PropertyDescriptor::data(Value::Boolean(true), true, true, false),
        );
        assert!(!o.delete("x"));
        assert!(o.get("x").is_some());
    }
}
