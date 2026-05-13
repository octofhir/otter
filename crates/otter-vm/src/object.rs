//! JavaScript object value with hidden-class shape storage and
//! ECMA-262 §6.1.7.1 property descriptors.
//!
//! Each property carries the canonical attribute triple
//! `(writable, enumerable, configurable)` plus a body that is either
//! a `[[Value]]` (data property) or a `([[Get]], [[Set]])` accessor
//! pair. The `Shape`-based hidden-class model stays — keys are still
//! shared across literals — but as of task 77 the per-object body
//! (slots, prototype, symbol props, extensibility) lives on the tracing GC
//! heap as a [`Gc<ObjectBody>`] payload.
//!
//! # Storage
//!
//! Every read / write / write-barrier path takes an explicit
//! `&otter_gc::GcHeap` (or `&mut`) so the single-mutator invariant is visible in
//! the type system. Method signatures are of the shape `obj.get(heap, key)` and
//! `obj.set(heap, key, value)` — the heap is **not** thread-local. No
//! thread-local heap lookup is permitted in this module.
//!
//! `JsObject` is therefore a 4-byte compressed offset
//! ([`otter_gc::Gc<ObjectBody>`]); cloning a handle is `Copy`.
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
//! - [`JsObject`] / [`Shape`] / [`ObjectBody`] / [`Properties`] —
//!   the public object handle, its hidden class, the GC-allocated
//!   storage, and the read-only view used by JSON serialisation and
//!   `Object.keys` enumeration.
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
//! - Every store of a `Gc<…>`-bearing `Value` into a slot, every
//!   prototype assignment, and every symbol-property write records
//!   the store through [`otter_gc::GcHeap::record_write`] so the
//!   generational and incremental marker observe the new pointer.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-property-attributes>
//! - <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots>
//! - <https://tc39.es/ecma262/#sec-ordinarydefineownproperty>
//! - <https://tc39.es/ecma262/#sec-ordinaryset>
//! - [GC API](../../../docs/book/src/engine/gc-api.md)
//! - [Event loop](../../../docs/book/src/engine/event-loop.md)

use std::any::Any;
use std::cell::{Cell, OnceCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use smallvec::SmallVec;

use crate::number::NumberValue;
use crate::property_atom::{AtomId, AtomizedPropertyKey};
use crate::proxy::JsProxy;
use crate::string::JsString;
use crate::symbol::JsSymbol;
use crate::{UpvalueCell, Value, read_upvalue, store_upvalue};
use otter_gc::raw::{RawGc, SlotVisitor};

mod descriptor;
mod descriptor_core;
mod key_order;
mod lookup;

pub use descriptor::{
    DescriptorKind, PartialPropertyDescriptor, PropertyDescriptor, PropertyFlags,
};
pub use lookup::{PropertyLookup, SetOutcome, SetRejectReason};

static NEXT_SHAPE_ID: AtomicU64 = AtomicU64::new(1);

fn next_shape_id() -> ShapeId {
    ShapeId(NEXT_SHAPE_ID.fetch_add(1, Ordering::Relaxed))
}

/// Rust-owned data attached to a JavaScript object.
///
/// Host object data is isolate-local object state. It must not hold VM `Value`,
/// `Gc`, `Local`, `NativeCtx`, or async futures; if JS values need to be held
/// across GC, use explicit GC-managed payloads and trace hooks instead.
pub trait HostObjectData: Any {}

impl<T: Any> HostObjectData for T {}

#[derive(Debug, Clone)]
pub(crate) struct MappedArgumentEntry {
    pub(crate) key: String,
    pub(crate) cell: UpvalueCell,
}

#[derive(Debug)]
struct MappedArgumentsData {
    entries: Box<[MappedArgumentEntry]>,
}

/// Host object access failure.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostObjectError {
    /// Object has no host-owned payload.
    #[error("object has no host data")]
    Missing,
    /// Object has host data, but not the requested Rust type.
    #[error("host data type mismatch: expected {expected}, found {found}")]
    TypeMismatch {
        /// Requested Rust type.
        expected: &'static str,
        /// Stored Rust type.
        found: &'static str,
    },
}

/// Legal `[[Prototype]]` slot values.
#[derive(Debug, Clone)]
pub enum ObjectPrototype {
    /// `null` prototype.
    Null,
    /// Ordinary object prototype.
    Object(JsObject),
    /// Non-ordinary object-like prototype represented outside
    /// [`JsObject`], such as a function value.
    Value(Value),
    /// Proxy object prototype.
    Proxy(JsProxy),
}

impl ObjectPrototype {
    fn as_value(&self) -> Option<Value> {
        match self {
            Self::Null => None,
            Self::Object(obj) => Some(Value::Object(*obj)),
            Self::Value(value) => Some(value.clone()),
            Self::Proxy(proxy) => Some(Value::Proxy(proxy.clone())),
        }
    }
}

fn is_prototype_object_value(value: &Value) -> bool {
    matches!(
        value,
        Value::Object(_)
            | Value::Array(_)
            | Value::Function { .. }
            | Value::Closure { .. }
            | Value::NativeFunction(_)
            | Value::BoundFunction(_)
            | Value::ClassConstructor(_)
            | Value::Promise(_)
            | Value::Iterator(_)
            | Value::RegExp(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::WeakMap(_)
            | Value::WeakSet(_)
            | Value::WeakRef(_)
            | Value::FinalizationRegistry(_)
            | Value::Temporal(_)
            | Value::Date(_)
            | Value::Intl(_)
            | Value::ArrayBuffer(_)
            | Value::DataView(_)
            | Value::TypedArray(_)
            | Value::Generator(_)
            | Value::Proxy(_)
    )
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

/// VM-local hidden-class identity for interpreter inline-cache guards.
///
/// Shape ids are internal metadata only. They are not serialized and have no
/// JavaScript-observable meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ShapeId(u64);

/// Atom-aware own-property hit metadata.
///
/// This keeps the first inline-cache slice small: named property opcodes can
/// learn the receiver shape, property atom, and slot offset without changing
/// object storage or descriptor semantics yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AtomOwnPropertyHit {
    /// Shape observed on the receiver object.
    pub(crate) shape_id: ShapeId,
    /// Atomized named-property key from the executable context.
    pub(crate) atom_id: AtomId,
    /// String-keyed own-property slot offset.
    pub(crate) slot: u16,
}

/// Atom-aware property lookup result.
#[derive(Debug, Clone)]
pub(crate) struct AtomPropertyLookup {
    /// Metadata for the slot that produced [`Self::lookup`], if the hit was a
    /// string-keyed ordinary object property.
    #[allow(dead_code)]
    pub(crate) hit: Option<AtomOwnPropertyHit>,
    /// Descriptor-shaped lookup result used by today's interpreter semantics.
    pub(crate) lookup: PropertyLookup,
}

/// A hidden-class node. Shapes form a tree rooted at the empty
/// shape; each non-root shape records the parent plus the single
/// key added to reach it.
///
/// Shapes are immutable after construction. Two cache fields use
/// interior mutability:
/// - `offsets` — a write-once [`OnceCell`] populated lazily by
///   [`Self::offset_of`].
/// - `transitions` — a [`Cell`]-wrapped `HashMap` that records
///   shared child shapes by added key. Mutations swap the map out,
///   modify, and swap it back — there is no `RefCell`-style
///   borrow checker here because nothing observes a partial write
///   (the call is single-threaded and re-entrancy is impossible:
///   neither path calls back into shape mutation).
///
/// Per the GC architecture plan §4.1 shapes are NOT GC-managed —
/// they are `Rc`-shared leaf metadata and the [`ObjectBody`] tracer
/// deliberately does not walk into them.
pub struct Shape {
    id: ShapeId,
    #[allow(dead_code)]
    parent: Option<Rc<Shape>>,
    #[allow(dead_code)]
    key: Option<String>,
    keys: Vec<String>,
    offsets: OnceCell<HashMap<String, u16>>,
    transitions: Cell<HashMap<String, Rc<Shape>>>,
}

impl std::fmt::Debug for Shape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shape")
            .field("len", &self.keys.len())
            .finish()
    }
}

impl Shape {
    /// Construct the root (empty) shape.
    #[must_use]
    pub fn root() -> Rc<Shape> {
        Rc::new(Shape {
            id: next_shape_id(),
            parent: None,
            key: None,
            keys: Vec::new(),
            offsets: OnceCell::new(),
            transitions: Cell::new(HashMap::new()),
        })
    }

    /// Number of properties carried by this shape.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Stable identity for this hidden-class node.
    #[must_use]
    pub(crate) const fn id(&self) -> ShapeId {
        self.id
    }

    /// `true` for the empty (root) shape.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Look up a property's slot offset. Lazily builds the
    /// keys → offset cache on first call.
    #[must_use]
    pub fn offset_of(&self, key: &str) -> Option<u16> {
        let cache = self.offsets.get_or_init(|| {
            self.keys
                .iter()
                .enumerate()
                .map(|(i, k)| (k.clone(), i as u16))
                .collect()
        });
        cache.get(key).copied()
    }

    /// Iterate keys in insertion order.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.keys.iter()
    }

    /// Append `key` and return the resulting child shape, sharing
    /// it with prior callers when possible.
    ///
    /// Mutates the parent's transition table by swapping the
    /// `Cell`-stored map out, inserting, and swapping it back —
    /// safe because nothing observes the swap window (single
    /// mutator, no re-entrancy from the called code).
    #[must_use]
    pub fn add_property(self_rc: &Rc<Shape>, key: &str) -> Rc<Shape> {
        // Probe for an existing transition.
        let mut transitions = self_rc.transitions.take();
        if let Some(existing) = transitions.get(key) {
            let hit = Rc::clone(existing);
            self_rc.transitions.set(transitions);
            return hit;
        }
        let mut keys = self_rc.keys.clone();
        keys.push(key.to_string());
        let child = Rc::new(Shape {
            id: next_shape_id(),
            parent: Some(Rc::clone(self_rc)),
            key: Some(key.to_string()),
            keys,
            offsets: OnceCell::new(),
            transitions: Cell::new(HashMap::new()),
        });
        transitions.insert(key.to_string(), Rc::clone(&child));
        self_rc.transitions.set(transitions);
        child
    }
}

thread_local! {
    /// Cheap shared root for empty objects.
    static ROOT_SHAPE: Rc<Shape> = Shape::root();
}

// ---------- JsObject ------------------------------------------------------

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`ObjectBody`].
///
/// Distinct from `UPVALUE_CELL_TYPE_TAG = 0x10` (task 76).
pub const OBJECT_BODY_TYPE_TAG: u8 = 0x11;

/// GC-allocated storage backing every [`JsObject`] handle.
///
/// Per ECMA-262 §10.1, ordinary objects carry a hidden-class
/// [`Shape`], an aligned slot table, an optional `[[Prototype]]`,
/// a list of symbol-keyed own properties, and an `[[Extensible]]`
/// flag. All of those fields live here directly — task 77 retired
/// the pre-77 `Rc<RefCell<…>>` envelope. Mutation flows through
/// [`otter_gc::GcHeap::with_payload`] (writers) and reads through
/// [`otter_gc::GcHeap::read_payload`] (readers). Every store of a
/// `Gc<…>`-bearing field is recorded through
/// [`otter_gc::GcHeap::record_write`].
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots>
/// - <https://tc39.es/ecma262/#sec-ordinarypreventextensions>
pub struct ObjectBody {
    /// Hidden class. `Rc`-shared because shapes are immutable
    /// post-transition; per architecture plan §4.1 shapes are NOT
    /// GC-managed.
    shape: Rc<Shape>,
    /// Slot table aligned with [`Shape::keys`].
    slots: SmallVec<[PropertySlot; 4]>,
    /// `[[Prototype]]` — [`otter_gc::Gc::null()`] encodes JS
    /// `null` (no prototype). Stored as a bare `JsObject` rather
    /// than `Option<JsObject>` so the slot has a stable address
    /// the GC can yield to its scavenger / marker (`Option<u32>`
    /// has no niche and the discriminant offset would not give a
    /// `RawGc`-aligned slot).
    prototype: ObjectPrototype,
    /// Symbol-keyed own properties. Stored outside the string-keyed
    /// shape because symbols are identity keys, but values still use
    /// the same descriptor slot representation.
    symbol_props: Vec<(JsSymbol, PropertySlot)>,
    /// Rust-owned payload for host-backed objects and VM-internal
    /// exotic side data.
    host_data: Option<Box<dyn Any>>,
    /// Native `[[Call]]` implementation for builtin ordinary
    /// objects that are callable without using a `Value::NativeFunction`
    /// as their public representation.
    call_native: Option<Value>,
    /// Native `[[Construct]]` implementation for constructor-shaped
    /// builtin objects such as `Number` and `Boolean`.
    constructor_native: Option<Value>,
    /// `[[BooleanData]]` internal slot for Boolean wrapper objects.
    boolean_data: Option<bool>,
    /// `[[NumberData]]` internal slot for Number wrapper objects.
    number_data: Option<NumberValue>,
    /// `[[StringData]]` internal slot for String wrapper objects.
    string_data: Option<JsString>,
    /// `[[Extensible]]` internal slot. New keys are rejected when
    /// this is `false`.
    extensible: bool,
}

impl std::fmt::Debug for ObjectBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectBody")
            .field("shape_len", &self.shape.len())
            .field("slot_count", &self.slots.len())
            .field(
                "has_prototype",
                &!matches!(self.prototype, ObjectPrototype::Null),
            )
            .field("symbol_props", &self.symbol_props.len())
            .field("has_host_data", &self.host_data.is_some())
            .field(
                "mapped_arguments",
                &self
                    .host_data
                    .as_ref()
                    .and_then(|data| data.downcast_ref::<MappedArgumentsData>())
                    .map_or(0, |data| data.entries.len()),
            )
            .field("has_call_native", &self.call_native.is_some())
            .field("has_constructor_native", &self.constructor_native.is_some())
            .field("has_boolean_data", &self.boolean_data.is_some())
            .field("has_number_data", &self.number_data.is_some())
            .field("has_string_data", &self.string_data.is_some())
            .field("extensible", &self.extensible)
            .finish()
    }
}

impl otter_gc::SafeTraceable for ObjectBody {
    const TYPE_TAG: u8 = OBJECT_BODY_TYPE_TAG;

    /// Walk every outgoing GC reference held by `self`:
    /// - the `[[Prototype]]` handle (if any);
    /// - every `Value` inside a data slot or accessor pair;
    /// - every `Value` inside symbol-keyed own properties.
    ///
    /// Shape keys are interned `String` leaves and their containing
    /// [`Shape`] is `Rc`-shared, not GC-managed, so they are not
    /// traced — see the type doc on [`ObjectBody`].
    fn trace_slots_safe(&self, v: &mut SlotVisitor<'_>) {
        match &self.prototype {
            ObjectPrototype::Null => {}
            ObjectPrototype::Object(proto) => {
                let p = proto as *const JsObject as *mut RawGc;
                v(p);
            }
            ObjectPrototype::Value(value) => {
                value.trace_value_slots(v);
            }
            ObjectPrototype::Proxy(proxy) => {
                proxy.trace_value_slots(v);
            }
        }
        // Property slots.
        for slot in self.slots.iter() {
            match &slot.body {
                SlotBody::Data { value } => value.trace_value_slots(v),
                SlotBody::Accessor { getter, setter } => {
                    if let Some(g) = getter {
                        g.trace_value_slots(v);
                    }
                    if let Some(s) = setter {
                        s.trace_value_slots(v);
                    }
                }
            }
        }
        // Symbol-keyed own properties.
        for (_sym, slot) in self.symbol_props.iter() {
            match &slot.body {
                SlotBody::Data { value } => value.trace_value_slots(v),
                SlotBody::Accessor { getter, setter } => {
                    if let Some(g) = getter {
                        g.trace_value_slots(v);
                    }
                    if let Some(s) = setter {
                        s.trace_value_slots(v);
                    }
                }
            }
        }
        if let Some(native) = &self.call_native {
            native.trace_value_slots(v);
        }
        if let Some(native) = &self.constructor_native {
            native.trace_value_slots(v);
        }
        if let Some(data) = self
            .host_data
            .as_ref()
            .and_then(|data| data.downcast_ref::<MappedArgumentsData>())
        {
            for entry in data.entries.iter() {
                let p = &entry.cell as *const UpvalueCell as *mut RawGc;
                v(p);
            }
        }
    }
}

/// Heap-shared object handle.
///
/// As of task 77 this is a 4-byte compressed
/// [`otter_gc::Gc<ObjectBody>`]. The handle is `Copy + Eq + Hash`
/// (inherited from [`otter_gc::Gc`]); identity comparison is the
/// default `==`.
///
/// Every method that reads or mutates the body takes an explicit
/// `&otter_gc::GcHeap` (read) or `&mut otter_gc::GcHeap` (mutate).
/// There is no thread-local heap lookup in this module; per
/// every borrow path threads the heap.
pub type JsObject = otter_gc::Gc<ObjectBody>;

/// Maximum prototype-chain hops a property lookup will follow.
pub const PROTO_CHAIN_HARD_CAP: usize = 1024;

/// Allocate a fresh empty extensible object on the root shape.
///
/// Routes through [`otter_gc::GcHeap::alloc_old`] so the body is
/// allocated directly in old-space — Phase-1 callers may still hold
/// raw `JsObject` slots inside `Rc`-shared containers that the
/// young-gen scavenger cannot rewrite. Phase 2 may switch back to
/// [`otter_gc::GcHeap::alloc`] once every container slot is walked.
///
/// # Errors
///
/// Surfaces [`otter_gc::OutOfMemory`] verbatim; runtime callers
/// translate it into [`crate::VmError::OutOfMemory`].
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-ordinaryobjectcreate>
pub fn alloc_object(heap: &mut otter_gc::GcHeap) -> Result<JsObject, otter_gc::OutOfMemory> {
    let shape = ROOT_SHAPE.with(Rc::clone);
    heap.alloc_old(ObjectBody {
        shape,
        slots: SmallVec::new(),
        prototype: ObjectPrototype::Null,
        symbol_props: Vec::new(),
        host_data: None,
        call_native: None,
        constructor_native: None,
        boolean_data: None,
        number_data: None,
        string_data: None,
        extensible: true,
    })
}

/// Allocate a fresh empty object for diagnostic delivery after the
/// heap cap has already fired.
///
/// This mirrors [`alloc_object`] but uses
/// [`otter_gc::GcHeap::alloc_old_diagnostic`] so the VM can throw a
/// catchable `RangeError` for an allocation failure instead of
/// immediately losing the error object to the same cap.
///
/// # Errors
///
/// Surfaces cage exhaustion; heap-cap exhaustion is intentionally
/// bypassed for this diagnostic object only.
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-error-objects>
pub fn alloc_diagnostic_object(
    heap: &mut otter_gc::GcHeap,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    let shape = ROOT_SHAPE.with(Rc::clone);
    heap.alloc_old_diagnostic(ObjectBody {
        shape,
        slots: SmallVec::new(),
        prototype: ObjectPrototype::Null,
        symbol_props: Vec::new(),
        host_data: None,
        call_native: None,
        constructor_native: None,
        boolean_data: None,
        number_data: None,
        string_data: None,
        extensible: true,
    })
}

/// Allocate a fresh object backed by Rust-owned host data.
///
/// The host data is isolate-local and intentionally not traced. It must not own
/// JS `Value` / `Gc` handles. Native methods should access it through
/// [`with_host_data`] / [`with_host_data_mut`] using the receiver from
/// [`crate::NativeCtx::this_value`].
pub fn alloc_host_object<T: HostObjectData>(
    heap: &mut otter_gc::GcHeap,
    data: T,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    let shape = ROOT_SHAPE.with(Rc::clone);
    heap.alloc_old(ObjectBody {
        shape,
        slots: SmallVec::new(),
        prototype: ObjectPrototype::Null,
        symbol_props: Vec::new(),
        host_data: Some(Box::new(data)),
        call_native: None,
        constructor_native: None,
        boolean_data: None,
        number_data: None,
        string_data: None,
        extensible: true,
    })
}

/// Allocate a fresh empty object whose prototype is `proto`.
///
/// Convenience wrapper around [`alloc_object`] that fires the
/// generational write barrier on the freshly-installed prototype
/// link.
///
/// # Errors
///
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-ordinaryobjectcreate>
pub fn alloc_object_with_proto(
    heap: &mut otter_gc::GcHeap,
    proto: Option<JsObject>,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    let obj = alloc_object(heap)?;
    if let Some(p) = proto {
        set_prototype(obj, heap, Some(p));
    }
    Ok(obj)
}

pub(crate) fn install_mapped_arguments(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    entries: Vec<MappedArgumentEntry>,
) {
    heap.with_payload(obj, |body| {
        if !entries.is_empty() {
            body.host_data = Some(Box::new(MappedArgumentsData {
                entries: entries.into_boxed_slice(),
            }));
        }
    });
}

fn mapped_argument_cell(body: &ObjectBody, key: &str) -> Option<UpvalueCell> {
    body.host_data
        .as_ref()?
        .downcast_ref::<MappedArgumentsData>()?
        .entries
        .iter()
        .find(|entry| entry.key == key)
        .map(|entry| entry.cell)
}

fn remove_mapped_argument(body: &mut ObjectBody, key: &str) {
    let Some(data) = body.host_data.take() else {
        return;
    };
    match data.downcast::<MappedArgumentsData>() {
        Ok(mapped) => {
            let retained: Vec<_> = mapped
                .entries
                .into_vec()
                .into_iter()
                .filter(|entry| entry.key != key)
                .collect();
            if !retained.is_empty() {
                body.host_data = Some(Box::new(MappedArgumentsData {
                    entries: retained.into_boxed_slice(),
                }));
            }
        }
        Err(other) => {
            body.host_data = Some(other);
        }
    }
}

// ---------- read accessors -----------------------------------------------

/// Number of own (string-keyed) properties.
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-ordinaryownpropertykeys>
#[must_use]
pub fn len(obj: JsObject, heap: &otter_gc::GcHeap) -> usize {
    heap.read_payload(obj, |body| body.shape.len())
}

/// `true` when the object has no string-keyed own properties.
#[must_use]
pub fn is_empty(obj: JsObject, heap: &otter_gc::GcHeap) -> bool {
    len(obj, heap) == 0
}

/// Return the object's current hidden-class id.
#[must_use]
#[allow(dead_code)]
pub(crate) fn shape_id(obj: JsObject, heap: &otter_gc::GcHeap) -> ShapeId {
    heap.read_payload(obj, |body| body.shape.id())
}

/// Read an **own** property with an accessor short-circuit:
/// returns `Some(value)` for data slots, `Some(undefined)` for
/// accessor slots (callers that need to invoke the getter must
/// use [`lookup_own`] / [`get_own_descriptor`]).
#[must_use]
pub fn get_own(obj: JsObject, heap: &otter_gc::GcHeap, key: &str) -> Option<Value> {
    heap.read_payload(obj, |body| {
        if let Some(cell) = mapped_argument_cell(body, key) {
            return Some(read_upvalue(heap, cell));
        }
        body.shape
            .offset_of(key)
            .map(|offset| match &body.slots[offset as usize].body {
                SlotBody::Data { value } => value.clone(),
                SlotBody::Accessor { .. } => Value::Undefined,
            })
    })
}

/// Read a property, walking the prototype chain on miss.
/// Accessors collapse to `undefined` here for backward-compat
/// with construction-time call sites; the dispatch loop's
/// `LoadProperty` handler invokes accessors through [`lookup`]
/// instead.
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-ordinaryget>
#[must_use]
pub fn get(obj: JsObject, heap: &otter_gc::GcHeap, key: &str) -> Option<Value> {
    match lookup(obj, heap, key) {
        PropertyLookup::Absent => None,
        PropertyLookup::Data { value, .. } => Some(value),
        PropertyLookup::Accessor { .. } => Some(Value::Undefined),
    }
}

/// Probe for an own property (no proto-chain walk). The result
/// distinguishes data, accessor, and absent.
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-ordinarygetownproperty>
#[must_use]
pub fn lookup_own(obj: JsObject, heap: &otter_gc::GcHeap, key: &str) -> PropertyLookup {
    heap.read_payload(obj, |body| match body.shape.offset_of(key) {
        Some(offset) => {
            let mut lookup = body.slots[offset as usize].to_lookup();
            if let Some(cell) = mapped_argument_cell(body, key)
                && let PropertyLookup::Data { value, .. } = &mut lookup
            {
                *value = read_upvalue(heap, cell);
            }
            lookup
        }
        None => PropertyLookup::Absent,
    })
}

/// Atom-aware own-property probe for named property bytecodes.
#[must_use]
pub(crate) fn lookup_own_atom(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
) -> AtomPropertyLookup {
    heap.read_payload(obj, |body| match body.shape.offset_of(key.name()) {
        Some(offset) => {
            let mut lookup = body.slots[offset as usize].to_lookup();
            if let Some(cell) = mapped_argument_cell(body, key.name())
                && let PropertyLookup::Data { value, .. } = &mut lookup
            {
                *value = read_upvalue(heap, cell);
            }
            AtomPropertyLookup {
                hit: Some(AtomOwnPropertyHit {
                    shape_id: body.shape.id(),
                    atom_id: key.atom().id(),
                    slot: offset,
                }),
                lookup,
            }
        }
        None => AtomPropertyLookup {
            hit: None,
            lookup: PropertyLookup::Absent,
        },
    })
}

/// Load a cached own data slot after validating shape and atom guards.
#[must_use]
pub(crate) fn load_own_data_slot_atom(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
    hit: AtomOwnPropertyHit,
) -> Option<Value> {
    heap.read_payload(obj, |body| {
        if body.shape.id() != hit.shape_id || key.atom().id() != hit.atom_id {
            return None;
        }
        let offset = hit.slot as usize;
        if !matches!(body.shape.keys.get(offset), Some(name) if name == key.name()) {
            return None;
        }
        if let Some(cell) = mapped_argument_cell(body, key.name()) {
            return Some(read_upvalue(heap, cell));
        }
        match &body.slots.get(offset)?.body {
            SlotBody::Data { value } => Some(value.clone()),
            SlotBody::Accessor { .. } => None,
        }
    })
}

/// Store through a cached own data slot after validating shape and atom guards.
///
/// Returns `Some(())` only when the write was completed. `None` means the
/// cache no longer applies and callers must fall back to ordinary `[[Set]]`.
pub(crate) fn store_own_data_slot_atom(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
    hit: AtomOwnPropertyHit,
    value: Value,
) -> Option<()> {
    let mapped_cell = heap.read_payload(obj, |body| mapped_argument_cell(body, key.name()));
    let barrier_value = value.clone();
    let success = heap.with_payload(obj, |body| {
        if body.shape.id() != hit.shape_id || key.atom().id() != hit.atom_id {
            return false;
        }
        let offset = hit.slot as usize;
        if !matches!(body.shape.keys.get(offset), Some(name) if name == key.name()) {
            return false;
        }
        let Some(slot) = body.slots.get_mut(offset) else {
            return false;
        };
        if !slot.flags.writable() {
            return false;
        }
        let SlotBody::Data { value: stored } = &mut slot.body else {
            return false;
        };
        *stored = value;
        true
    });
    if !success {
        return None;
    }
    if let Some(cell) = mapped_cell {
        store_upvalue(heap, cell, barrier_value.clone());
    }
    heap.record_write(obj, &barrier_value);
    Some(())
}

/// Probe for a property with full prototype-chain walk. Returns
/// the first hit's descriptor body; useful for the LoadProperty
/// dispatch path which needs to know whether to invoke a getter
/// at any depth.
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-ordinaryget>
#[must_use]
pub fn lookup(obj: JsObject, heap: &otter_gc::GcHeap, key: &str) -> PropertyLookup {
    match lookup_own(obj, heap, key) {
        PropertyLookup::Absent => {}
        hit => return hit,
    }
    let mut current = prototype(obj, heap);
    let mut hops = 0;
    while let Some(proto) = current {
        if hops >= PROTO_CHAIN_HARD_CAP {
            return PropertyLookup::Absent;
        }
        hops += 1;
        match lookup_own(proto, heap, key) {
            PropertyLookup::Absent => {}
            hit => return hit,
        }
        current = prototype(proto, heap);
    }
    PropertyLookup::Absent
}

/// Atom-aware property probe with a prototype-chain walk.
#[must_use]
#[allow(dead_code)]
pub(crate) fn lookup_atom(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
) -> AtomPropertyLookup {
    let own = lookup_own_atom(obj, heap, key);
    if !matches!(own.lookup, PropertyLookup::Absent) {
        return own;
    }
    let mut current = prototype(obj, heap);
    let mut hops = 0;
    while let Some(proto) = current {
        if hops >= PROTO_CHAIN_HARD_CAP {
            return AtomPropertyLookup {
                hit: None,
                lookup: PropertyLookup::Absent,
            };
        }
        hops += 1;
        let hit = lookup_own_atom(proto, heap, key);
        if !matches!(hit.lookup, PropertyLookup::Absent) {
            return hit;
        }
        current = prototype(proto, heap);
    }
    AtomPropertyLookup {
        hit: None,
        lookup: PropertyLookup::Absent,
    }
}

/// Read the descriptor for an own property.
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-ordinarygetownproperty>
#[must_use]
pub fn get_own_descriptor(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    key: &str,
) -> Option<PropertyDescriptor> {
    heap.read_payload(obj, |body| {
        body.shape.offset_of(key).map(|offset| {
            let mut descriptor = body.slots[offset as usize].to_descriptor();
            if let Some(cell) = mapped_argument_cell(body, key)
                && let DescriptorKind::Data { value } = &mut descriptor.kind
            {
                *value = read_upvalue(heap, cell);
            }
            descriptor
        })
    })
}

/// Borrow the current prototype, if any.
///
/// Returns `None` when the stored handle is [`otter_gc::Gc::null()`]
/// (the in-payload encoding for JS `null`).
#[must_use]
pub fn prototype(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<JsObject> {
    heap.read_payload(obj, |body| match &body.prototype {
        ObjectPrototype::Object(proto) => Some(*proto),
        ObjectPrototype::Null | ObjectPrototype::Value(_) | ObjectPrototype::Proxy(_) => None,
    })
}

/// Borrow the current prototype as a JS value, if any.
#[must_use]
pub fn prototype_value(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<Value> {
    heap.read_payload(obj, |body| body.prototype.as_value())
}

/// `true` when `obj` has `target` somewhere in its prototype chain.
/// Used by `instanceof`.
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-ordinaryhasinstance>
#[must_use]
pub fn has_in_proto_chain(obj: JsObject, heap: &otter_gc::GcHeap, target: JsObject) -> bool {
    let mut current = prototype(obj, heap);
    let mut hops = 0;
    while let Some(proto) = current {
        if hops >= PROTO_CHAIN_HARD_CAP {
            return false;
        }
        hops += 1;
        if proto == target {
            return true;
        }
        current = prototype(proto, heap);
    }
    false
}

/// Look up by a [`JsString`] key. Convenience for dispatcher
/// sites that already hold the WTF-16 form.
#[must_use]
pub fn get_jsstring(obj: JsObject, heap: &otter_gc::GcHeap, key: &JsString) -> Option<Value> {
    let utf8 = key.to_lossy_string();
    get(obj, heap, &utf8)
}

/// Look up an **own** symbol-keyed property.
#[must_use]
pub fn get_own_symbol(obj: JsObject, heap: &otter_gc::GcHeap, key: &JsSymbol) -> Option<Value> {
    heap.read_payload(obj, |body| {
        body.symbol_props
            .iter()
            .find(|(k, _)| k.ptr_eq(key))
            .map(|(_, slot)| match &slot.body {
                SlotBody::Data { value } => value.clone(),
                SlotBody::Accessor { .. } => Value::Undefined,
            })
    })
}

/// Probe for an **own** symbol-keyed property descriptor body.
#[must_use]
pub fn lookup_own_symbol(obj: JsObject, heap: &otter_gc::GcHeap, key: &JsSymbol) -> PropertyLookup {
    heap.read_payload(obj, |body| {
        body.symbol_props
            .iter()
            .find(|(k, _)| k.ptr_eq(key))
            .map_or(PropertyLookup::Absent, |(_, slot)| slot.to_lookup())
    })
}

/// Return whether `obj` has an own symbol-keyed property.
///
/// This is the symbol-keyed counterpart to [`lookup_own`]'s
/// `PropertyLookup::Absent` probe and intentionally does not walk
/// the prototype chain.
#[must_use]
pub fn has_own_symbol(obj: JsObject, heap: &otter_gc::GcHeap, key: &JsSymbol) -> bool {
    !matches!(lookup_own_symbol(obj, heap, key), PropertyLookup::Absent)
}

/// Look up a symbol-keyed property with prototype-chain walk.
#[must_use]
pub fn get_symbol(obj: JsObject, heap: &otter_gc::GcHeap, key: &JsSymbol) -> Option<Value> {
    if let Some(v) = get_own_symbol(obj, heap, key) {
        return Some(v);
    }
    let mut current = prototype(obj, heap);
    let mut hops = 0;
    while let Some(proto) = current {
        if hops >= PROTO_CHAIN_HARD_CAP {
            return None;
        }
        hops += 1;
        if let Some(v) = get_own_symbol(proto, heap, key) {
            return Some(v);
        }
        current = prototype(proto, heap);
    }
    None
}

/// Symbol-keyed property lookup with prototype-chain walk.
#[must_use]
pub fn lookup_symbol(obj: JsObject, heap: &otter_gc::GcHeap, key: &JsSymbol) -> PropertyLookup {
    match lookup_own_symbol(obj, heap, key) {
        PropertyLookup::Absent => {}
        hit => return hit,
    }
    let mut current = prototype(obj, heap);
    let mut hops = 0;
    while let Some(proto) = current {
        if hops >= PROTO_CHAIN_HARD_CAP {
            return PropertyLookup::Absent;
        }
        hops += 1;
        match lookup_own_symbol(proto, heap, key) {
            PropertyLookup::Absent => {}
            hit => return hit,
        }
        current = prototype(proto, heap);
    }
    PropertyLookup::Absent
}

/// Read the descriptor for an own symbol-keyed property.
#[must_use]
pub fn get_own_symbol_descriptor(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    key: &JsSymbol,
) -> Option<PropertyDescriptor> {
    heap.read_payload(obj, |body| {
        body.symbol_props
            .iter()
            .find(|(k, _)| k.ptr_eq(key))
            .map(|(_, slot)| slot.to_descriptor())
    })
}

/// Store the internal native `[[Call]]` slot for callable ordinary
/// objects.
pub fn set_call_native(obj: JsObject, heap: &mut otter_gc::GcHeap, native: Value) {
    heap.with_payload(obj, |body| {
        body.call_native = Some(native.clone());
    });
    heap.record_write(obj, &native);
}

/// Read the internal native `[[Call]]` slot.
#[must_use]
pub fn call_native(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<Value> {
    heap.read_payload(obj, |body| body.call_native.clone())
}

/// Store the internal native `[[Construct]]` slot for constructor-shaped
/// builtin objects. Current builtin constructor objects are callable
/// too, so this also installs the same callback as `[[Call]]`.
pub fn set_constructor_native(obj: JsObject, heap: &mut otter_gc::GcHeap, native: Value) {
    heap.with_payload(obj, |body| {
        body.call_native = Some(native.clone());
        body.constructor_native = Some(native.clone());
    });
    heap.record_write(obj, &native);
}

/// Read the internal native `[[Construct]]` slot.
#[must_use]
pub fn constructor_native(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<Value> {
    heap.read_payload(obj, |body| body.constructor_native.clone())
}

/// Store the `[[BooleanData]]` internal slot for a Boolean wrapper.
pub fn set_boolean_data(obj: JsObject, heap: &mut otter_gc::GcHeap, value: bool) {
    heap.with_payload(obj, |body| {
        body.boolean_data = Some(value);
    });
}

/// Read the `[[BooleanData]]` internal slot for a Boolean wrapper.
#[must_use]
pub fn boolean_data(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<bool> {
    heap.read_payload(obj, |body| body.boolean_data)
}

/// Store the `[[NumberData]]` internal slot for a Number wrapper.
pub fn set_number_data(obj: JsObject, heap: &mut otter_gc::GcHeap, value: NumberValue) {
    heap.with_payload(obj, |body| {
        body.number_data = Some(value);
    });
}

/// Read the `[[NumberData]]` internal slot for a Number wrapper.
#[must_use]
pub fn number_data(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<NumberValue> {
    heap.read_payload(obj, |body| body.number_data)
}

/// Store the `[[StringData]]` internal slot for a String wrapper.
pub fn set_string_data(obj: JsObject, heap: &mut otter_gc::GcHeap, value: JsString) {
    heap.with_payload(obj, |body| {
        body.string_data = Some(value);
    });
}

/// Read the `[[StringData]]` internal slot for a String wrapper.
#[must_use]
pub fn string_data(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<JsString> {
    heap.read_payload(obj, |body| body.string_data.clone())
}

/// Borrow typed host data attached to `obj`.
///
/// The callback runs under an immutable object-payload borrow. Do not attempt
/// to re-enter object mutation from inside `f`.
pub fn with_host_data<T, R>(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    f: impl FnOnce(&T) -> R,
) -> Result<R, HostObjectError>
where
    T: HostObjectData,
{
    heap.read_payload(obj, |body| {
        let data = body.host_data.as_ref().ok_or(HostObjectError::Missing)?;
        data.downcast_ref::<T>()
            .map(f)
            .ok_or_else(|| HostObjectError::TypeMismatch {
                expected: std::any::type_name::<T>(),
                found: "<unknown host data>",
            })
    })
}

/// Mutably borrow typed host data attached to `obj`.
///
/// The callback runs under a mutable object-payload borrow. Native methods
/// should copy primitive results out before allocating new JS values.
pub fn with_host_data_mut<T, R>(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    f: impl FnOnce(&mut T) -> R,
) -> Result<R, HostObjectError>
where
    T: HostObjectData,
{
    heap.with_payload(obj, |body| {
        let data = body.host_data.as_mut().ok_or(HostObjectError::Missing)?;
        let typed = data
            .downcast_mut::<T>()
            .ok_or_else(|| HostObjectError::TypeMismatch {
                expected: std::any::type_name::<T>(),
                found: "<unknown host data>",
            })?;
        Ok(f(typed))
    })
}

/// Borrow the current hidden class.
#[must_use]
pub fn shape(obj: JsObject, heap: &otter_gc::GcHeap) -> Rc<Shape> {
    heap.read_payload(obj, |body| Rc::clone(&body.shape))
}

/// `[[IsExtensible]]` — `false` after [`prevent_extensions`] /
/// [`seal`] / [`freeze`].
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-ordinaryisextensible>
#[must_use]
pub fn is_extensible(obj: JsObject, heap: &otter_gc::GcHeap) -> bool {
    heap.read_payload(obj, |body| body.extensible)
}

/// `Object.isSealed(o)` — `true` when the object is non-extensible
/// and every own property is non-configurable.
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-testintegritylevel>
#[must_use]
pub fn is_sealed(obj: JsObject, heap: &otter_gc::GcHeap) -> bool {
    heap.read_payload(obj, |body| {
        if body.extensible {
            return false;
        }
        body.slots.iter().all(|s| !s.flags.configurable())
    })
}

/// `Object.isFrozen(o)` — `true` when the object is sealed and
/// every data slot is non-writable.
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-testintegritylevel>
#[must_use]
pub fn is_frozen(obj: JsObject, heap: &otter_gc::GcHeap) -> bool {
    heap.read_payload(obj, |body| {
        if body.extensible {
            return false;
        }
        for slot in body.slots.iter() {
            if slot.flags.configurable() {
                return false;
            }
            if let SlotBody::Data { .. } = slot.body
                && slot.flags.writable()
            {
                return false;
            }
        }
        true
    })
}

// ---------- mutation -----------------------------------------------------

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
///
/// Records the GC store when `value` carries a `Gc<…>` handle so the
/// marker / scavenger see the new edge.
pub fn set(obj: JsObject, heap: &mut otter_gc::GcHeap, key: &str, value: Value) {
    let barrier_value = value.clone();
    heap.with_payload(obj, |body| {
        if let Some(offset) = body.shape.offset_of(key) {
            let slot = &mut body.slots[offset as usize];
            slot.body = SlotBody::Data { value };
            return;
        }
        let new_shape = Shape::add_property(&body.shape, key);
        body.shape = new_shape;
        body.slots.push(PropertySlot::data_default(value));
    });
    heap.record_write(obj, &barrier_value);
}

/// Apply the data-write half of ordinary `[[Set]]` after
/// [`resolve_set`] has selected [`SetOutcome::AssignData`].
///
/// Existing own data properties keep their current attributes and
/// only replace `[[Value]]`. Missing properties are created with
/// default ordinary data attributes, but only when the receiver is
/// extensible. Accessor slots and non-writable data slots reject.
///
/// This is the runtime assignment path. Construction/bootstrap code
/// that owns a fresh object may still use [`set`] to seed internal
/// scaffolding; user-visible assignment should route through this
/// function after the `[[Set]]` resolver.
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-ordinarysetwithowndescriptor>
pub fn ordinary_set_data_property(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    value: Value,
) -> bool {
    let mapped_cell = heap.read_payload(obj, |body| mapped_argument_cell(body, key));
    let success = descriptor_core::ordinary_set_data_property(obj, heap, key, value.clone());
    if success && let Some(cell) = mapped_cell {
        store_upvalue(heap, cell, value);
    }
    success
}

/// Replace the prototype with a spec-legal value. `None` or
/// `Some(Value::Null)` detaches the chain.
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-ordinarysetprototypeof>
pub fn set_prototype_value(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    proto: Option<Value>,
) -> bool {
    let new_proto = match proto {
        Some(Value::Object(proto)) => ObjectPrototype::Object(proto),
        Some(Value::Proxy(proxy)) => ObjectPrototype::Proxy(proxy),
        Some(Value::Null) | None => ObjectPrototype::Null,
        Some(value) if is_prototype_object_value(&value) => ObjectPrototype::Value(value),
        _ => return false,
    };
    let barrier_value = new_proto.as_value();
    heap.with_payload(obj, |body| {
        body.prototype = new_proto;
    });
    if let Some(value) = &barrier_value {
        heap.record_write(obj, value);
    }
    true
}

/// Replace the prototype with an ordinary object or `null`.
///
/// This compatibility helper preserves existing call sites that do
/// not need Proxy-as-prototype support.
pub fn set_prototype(obj: JsObject, heap: &mut otter_gc::GcHeap, proto: Option<JsObject>) {
    let value = proto.map(Value::Object);
    set_prototype_value(obj, heap, value);
}

/// Remove an own property. Per ECMA-262 §10.1.10 OrdinaryDelete:
/// returns `true` when the property is absent or successfully
/// removed; returns `false` only when the property exists and is
/// non-configurable.
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-ordinarydelete>
pub fn delete(obj: JsObject, heap: &mut otter_gc::GcHeap, key: &str) -> bool {
    heap.with_payload(obj, |body| {
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
            id: next_shape_id(),
            parent: None,
            key: None,
            keys: new_keys,
            offsets: OnceCell::new(),
            transitions: Cell::new(HashMap::new()),
        });
        remove_mapped_argument(body, key);
        true
    })
}

/// Set or overwrite a symbol-keyed own data property through the
/// same descriptor-aware `[[Set]]` data-write core as string keys.
///
/// Fires the GC write barrier when `value` carries a `Gc<…>`
/// handle.
pub fn set_symbol(obj: JsObject, heap: &mut otter_gc::GcHeap, key: JsSymbol, value: Value) -> bool {
    descriptor_core::ordinary_set_symbol_data_property(obj, heap, &key, value)
}

/// Remove a symbol-keyed own property.
pub fn delete_symbol(obj: JsObject, heap: &mut otter_gc::GcHeap, key: &JsSymbol) -> bool {
    heap.with_payload(obj, |body| {
        if let Some(pos) = body.symbol_props.iter().position(|(k, _)| k.ptr_eq(key)) {
            if !body.symbol_props[pos].1.flags.configurable() {
                return false;
            }
            body.symbol_props.remove(pos);
            true
        } else {
            true
        }
    })
}

// ---------- descriptor surface --------------------------------------------

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
/// Fires the GC write barrier on every stored `Value` carrying a
/// `Gc<…>` handle.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-ordinarydefineownproperty>
/// - <https://tc39.es/ecma262/#sec-validateandapplypropertydescriptor>
/// Field-presence-aware §10.1.6.3 OrdinaryDefineOwnProperty for
/// string-keyed properties. Mirrors V8 / JSC's
/// `PropertyDescriptor`-based `[[DefineOwnProperty]]`: missing fields
/// preserve the existing value, missing-and-new defaults to spec
/// defaults (§10.1.6.3 step 5).
pub fn define_own_property_partial(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    descriptor: PartialPropertyDescriptor,
) -> bool {
    let completed = descriptor.complete_for_new_property();
    let barrier_descriptor = completed.clone();
    let map_descriptor = completed.clone();
    let success = heap.with_payload(obj, |body| {
        if let Some(offset) = body.shape.offset_of(key) {
            let existing = body.slots[offset as usize].clone();
            match descriptor_core::validate_and_apply_partial(&existing, &descriptor) {
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
            body.slots
                .push(PropertySlot::from_descriptor(completed.clone()));
            true
        }
    });
    if success {
        let mapped_cell = heap.read_payload(obj, |body| mapped_argument_cell(body, key));
        if let Some(cell) = mapped_cell {
            match &map_descriptor.kind {
                DescriptorKind::Data { value } => {
                    store_upvalue(heap, cell, value.clone());
                    if !map_descriptor.writable() {
                        heap.with_payload(obj, |body| remove_mapped_argument(body, key));
                    }
                }
                DescriptorKind::Accessor { .. } => {
                    heap.with_payload(obj, |body| remove_mapped_argument(body, key));
                }
            }
        }
        heap.record_write(obj, &barrier_descriptor);
    }
    success
}

/// Field-presence-aware §10.1.6.3 for symbol-keyed properties.
pub fn define_own_symbol_property_partial(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &JsSymbol,
    descriptor: PartialPropertyDescriptor,
) -> bool {
    let completed = descriptor.complete_for_new_property();
    let barrier_descriptor = completed.clone();
    let success = heap.with_payload(obj, |body| {
        if let Some(pos) = body.symbol_props.iter().position(|(k, _)| k.ptr_eq(key)) {
            let existing = body.symbol_props[pos].1.clone();
            match descriptor_core::validate_and_apply_partial(&existing, &descriptor) {
                Some(merged) => {
                    body.symbol_props[pos].1 = merged;
                    true
                }
                None => false,
            }
        } else {
            if !body.extensible {
                return false;
            }
            body.symbol_props.push((
                key.clone(),
                PropertySlot::from_descriptor(completed.clone()),
            ));
            true
        }
    });
    if success {
        heap.record_write(obj, &barrier_descriptor);
    }
    success
}

/// §10.1.6.3 OrdinaryDefineOwnProperty for a fully-specified
/// descriptor. Legacy entry point — prefer
/// [`define_own_property_partial`] for new callers so field-presence
/// is preserved.
pub fn define_own_property(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    descriptor: PropertyDescriptor,
) -> bool {
    let barrier_descriptor = descriptor.clone();
    let map_descriptor = descriptor.clone();
    let success = heap.with_payload(obj, |body| {
        if let Some(offset) = body.shape.offset_of(key) {
            let existing = body.slots[offset as usize].clone();
            match descriptor_core::validate_and_apply(&existing, &descriptor) {
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
    });
    if success {
        let mapped_cell = heap.read_payload(obj, |body| mapped_argument_cell(body, key));
        if let Some(cell) = mapped_cell {
            match &map_descriptor.kind {
                DescriptorKind::Data { value } => {
                    store_upvalue(heap, cell, value.clone());
                    if !map_descriptor.writable() {
                        heap.with_payload(obj, |body| remove_mapped_argument(body, key));
                    }
                }
                DescriptorKind::Accessor { .. } => {
                    heap.with_payload(obj, |body| remove_mapped_argument(body, key));
                }
            }
        }
        heap.record_write(obj, &barrier_descriptor);
    }
    success
}

/// Symbol-keyed counterpart to [`define_own_property`].
pub fn define_own_symbol_property(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &JsSymbol,
    descriptor: PropertyDescriptor,
) -> bool {
    let barrier_descriptor = descriptor.clone();
    let success = heap.with_payload(obj, |body| {
        if let Some(pos) = body.symbol_props.iter().position(|(k, _)| k.ptr_eq(key)) {
            let existing = body.symbol_props[pos].1.clone();
            match descriptor_core::validate_and_apply(&existing, &descriptor) {
                Some(merged) => {
                    body.symbol_props[pos].1 = merged;
                    true
                }
                None => false,
            }
        } else {
            if !body.extensible {
                return false;
            }
            body.symbol_props
                .push((key.clone(), PropertySlot::from_descriptor(descriptor)));
            true
        }
    });
    if success {
        heap.record_write(obj, &barrier_descriptor);
    }
    success
}

/// Validate one descriptor update against an existing descriptor using
/// the same `ValidateAndApplyPropertyDescriptor` core as ordinary objects.
pub(crate) fn validate_descriptor_update(
    existing: &PropertyDescriptor,
    incoming: &PropertyDescriptor,
) -> Option<PropertyDescriptor> {
    descriptor_core::validate_descriptor_update(existing, incoming)
}

impl otter_gc::GcStore for PropertyDescriptor {
    fn visit_gc_edges(&self, visitor: &mut dyn FnMut(otter_gc::GcEdge)) {
        match &self.kind {
            DescriptorKind::Data { value } => value.visit_gc_edges(visitor),
            DescriptorKind::Accessor { getter, setter } => {
                if let Some(getter) = getter {
                    getter.visit_gc_edges(visitor);
                }
                if let Some(setter) = setter {
                    setter.visit_gc_edges(visitor);
                }
            }
        }
    }
}

/// Resolve a `[[Set]]` against `obj` as receiver — walks the
/// prototype chain to detect inherited accessors and
/// non-writable shadows, but writes happen on `obj` (the
/// receiver) only. Per §10.1.9 OrdinarySet.
///
/// Returns a [`SetOutcome`] describing the action the dispatch
/// loop should take.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-ordinaryset>
/// - <https://tc39.es/ecma262/#sec-ordinarysetwithowndescriptor>
pub fn resolve_set(obj: JsObject, heap: &otter_gc::GcHeap, key: &str) -> SetOutcome {
    // Walk own + prototype chain looking for an accessor or a
    // non-writable shadow.
    let own = lookup_own(obj, heap, key);
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
    let mut current = prototype(obj, heap);
    let mut hops = 0;
    while let Some(proto) = current {
        if hops >= PROTO_CHAIN_HARD_CAP {
            break;
        }
        hops += 1;
        match lookup_own(proto, heap, key) {
            PropertyLookup::Data { flags, .. } => {
                if flags.writable() {
                    if !is_extensible(obj, heap) {
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
        current = prototype(proto, heap);
    }
    // Nothing on the chain — install a fresh data slot.
    if !is_extensible(obj, heap) {
        return SetOutcome::Reject {
            reason: SetRejectReason::NonExtensible,
        };
    }
    SetOutcome::AssignData
}

/// Symbol-keyed counterpart to [`resolve_set`].
pub fn resolve_symbol_set(obj: JsObject, heap: &otter_gc::GcHeap, key: &JsSymbol) -> SetOutcome {
    match lookup_own_symbol(obj, heap, key) {
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
    let mut current = prototype(obj, heap);
    let mut hops = 0;
    while let Some(proto) = current {
        if hops >= PROTO_CHAIN_HARD_CAP {
            break;
        }
        hops += 1;
        match lookup_own_symbol(proto, heap, key) {
            PropertyLookup::Data { flags, .. } => {
                if flags.writable() {
                    if !is_extensible(obj, heap) {
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
        current = prototype(proto, heap);
    }
    if !is_extensible(obj, heap) {
        return SetOutcome::Reject {
            reason: SetRejectReason::NonExtensible,
        };
    }
    SetOutcome::AssignData
}

/// `Object.preventExtensions(o)` core — clears the
/// `[[Extensible]]` slot. Always succeeds for ordinary objects.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-ordinarypreventextensions>
pub fn prevent_extensions(obj: JsObject, heap: &mut otter_gc::GcHeap) {
    heap.with_payload(obj, |body| body.extensible = false);
}

/// `Object.seal(o)` core — clears `[[Extensible]]` and toggles
/// `[[Configurable]]` to `false` on every own property.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-setintegritylevel>
pub fn seal(obj: JsObject, heap: &mut otter_gc::GcHeap) {
    heap.with_payload(obj, |body| {
        body.extensible = false;
        for slot in body.slots.iter_mut() {
            slot.flags = slot.flags.with_configurable(false);
        }
        for (_, slot) in body.symbol_props.iter_mut() {
            slot.flags = slot.flags.with_configurable(false);
        }
    });
}

/// `Object.freeze(o)` core — clears `[[Extensible]]`, then for
/// every own property: data slots become non-writable and
/// non-configurable; accessor slots become non-configurable.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-setintegritylevel>
pub fn freeze(obj: JsObject, heap: &mut otter_gc::GcHeap) {
    heap.with_payload(obj, |body| {
        body.extensible = false;
        for slot in body.slots.iter_mut() {
            slot.flags = slot.flags.with_configurable(false);
            if matches!(slot.body, SlotBody::Data { .. }) {
                slot.flags = slot.flags.with_writable(false);
            }
        }
        for (_, slot) in body.symbol_props.iter_mut() {
            slot.flags = slot.flags.with_configurable(false);
            if matches!(slot.body, SlotBody::Data { .. }) {
                slot.flags = slot.flags.with_writable(false);
            }
        }
    });
}

// ---------- iteration view -----------------------------------------------

/// Read-only snapshot of an object's properties in insertion
/// order. Used by debug rendering, JSON serialisation, and
/// `Object.keys`.
///
/// Built by [`with_properties`] under a `read_payload` borrow so
/// callers can iterate without copying the slot vector. The view
/// borrows from a transient `&ObjectBody` reference; it cannot
/// outlive the closure scope.
pub struct Properties<'a> {
    body: &'a ObjectBody,
}

impl<'a> Properties<'a> {
    /// Iterate every `(key, data-value)` pair in ordinary own-key
    /// order, regardless of enumerability. Accessor slots are
    /// surfaced as the sentinel `Value::Undefined` — callers that
    /// need accessor fidelity must consult [`get_own_descriptor`]
    /// directly.
    pub fn iter(&self) -> impl Iterator<Item = (&str, Value)> {
        key_order::ordinary_own_string_key_indices(self.body)
            .into_iter()
            .map(|idx| {
                let k = self.body.shape.keys[idx].as_str();
                let slot = &self.body.slots[idx];
                let value = match &slot.body {
                    SlotBody::Data { value } => value.clone(),
                    SlotBody::Accessor { .. } => Value::Undefined,
                };
                (k, value)
            })
    }

    /// Iterate string keys in ordinary own-key order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        key_order::ordinary_own_string_key_indices(self.body)
            .into_iter()
            .map(|idx| self.body.shape.keys[idx].as_str())
    }

    /// Iterate symbol-keyed own properties in insertion order.
    /// Used by `Object.getOwnPropertySymbols` (§20.1.2.13) and
    /// `Reflect.ownKeys` (§28.1.16) to surface symbol keys.
    pub fn symbol_keys(&self) -> impl Iterator<Item = JsSymbol> + '_ {
        self.body.symbol_props.iter().map(|(k, _)| k.clone())
    }

    /// Iterate `(key, data-value)` pairs in ordinary own-key order,
    /// skipping accessor and non-enumerable slots. Used by
    /// JSON.stringify and `for…in` once it lands.
    pub fn enumerable_data_iter(&self) -> impl Iterator<Item = (&str, Value)> {
        key_order::ordinary_own_string_key_indices(self.body)
            .into_iter()
            .filter_map(|idx| {
                let k = self.body.shape.keys[idx].as_str();
                let slot = &self.body.slots[idx];
                if !slot.flags.enumerable() {
                    return None;
                }
                match &slot.body {
                    SlotBody::Data { value } => Some((k, value.clone())),
                    SlotBody::Accessor { .. } => None,
                }
            })
    }

    /// Iterate enumerable own-key names (string-keyed only) in
    /// ordinary own-key order.
    pub fn enumerable_keys(&self) -> impl Iterator<Item = &str> {
        key_order::ordinary_own_string_key_indices(self.body)
            .into_iter()
            .filter_map(|idx| {
                self.body.slots[idx]
                    .flags
                    .enumerable()
                    .then_some(self.body.shape.keys[idx].as_str())
            })
    }
}

/// Run `f` with a [`Properties`] snapshot of `obj`'s string-keyed
/// and symbol-keyed own properties. The view does not escape the
/// closure scope.
pub fn with_properties<R>(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    f: impl FnOnce(Properties<'_>) -> R,
) -> R {
    heap.read_payload(obj, |body| f(Properties { body }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_gc::GcHeap;

    fn fresh_heap() -> GcHeap {
        GcHeap::new().expect("init heap")
    }

    #[test]
    fn empty_object_starts_with_zero_props() {
        let mut heap = fresh_heap();
        let o = alloc_object(&mut heap).unwrap();
        assert!(is_empty(o, &heap));
        assert_eq!(len(o, &heap), 0);
    }

    #[test]
    fn set_then_get_roundtrip() {
        let mut heap = fresh_heap();
        let o = alloc_object(&mut heap).unwrap();
        set(o, &mut heap, "x", Value::Boolean(true));
        assert!(matches!(get(o, &heap, "x"), Some(Value::Boolean(true))));
    }

    #[test]
    fn atom_lookup_reports_shape_and_slot_metadata() {
        let mut heap = fresh_heap();
        let o = alloc_object(&mut heap).unwrap();
        set(o, &mut heap, "x", Value::Boolean(true));
        let shape = shape_id(o, &heap);
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_constant_index(7)),
            "x",
        );

        let hit = lookup_own_atom(o, &heap, key);

        assert_eq!(
            hit.hit,
            Some(AtomOwnPropertyHit {
                shape_id: shape,
                atom_id: key.atom().id(),
                slot: 0,
            })
        );
        assert!(matches!(
            hit.lookup,
            PropertyLookup::Data {
                value: Value::Boolean(true),
                ..
            }
        ));
    }

    #[test]
    fn atom_slot_guard_rejects_shape_change() {
        let mut heap = fresh_heap();
        let o = alloc_object(&mut heap).unwrap();
        set(o, &mut heap, "x", Value::Boolean(true));
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_constant_index(7)),
            "x",
        );
        let hit = lookup_own_atom(o, &heap, key).hit.expect("atom hit");
        assert_eq!(
            load_own_data_slot_atom(o, &heap, key, hit),
            Some(Value::Boolean(true))
        );

        set(o, &mut heap, "y", Value::Null);

        assert_eq!(load_own_data_slot_atom(o, &heap, key, hit), None);
    }

    #[test]
    fn atom_slot_store_updates_guarded_data_slot() {
        let mut heap = fresh_heap();
        let o = alloc_object(&mut heap).unwrap();
        set(o, &mut heap, "x", Value::Boolean(true));
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_constant_index(7)),
            "x",
        );
        let hit = lookup_own_atom(o, &heap, key).hit.expect("atom hit");

        assert_eq!(
            store_own_data_slot_atom(o, &mut heap, key, hit, Value::Boolean(false)),
            Some(())
        );
        assert_eq!(
            load_own_data_slot_atom(o, &heap, key, hit),
            Some(Value::Boolean(false))
        );

        set(o, &mut heap, "y", Value::Null);

        assert_eq!(
            store_own_data_slot_atom(o, &mut heap, key, hit, Value::Boolean(true)),
            None
        );
    }

    #[test]
    fn shape_id_changes_on_new_property_not_overwrite() {
        let mut heap = fresh_heap();
        let o = alloc_object(&mut heap).unwrap();
        let empty = shape_id(o, &heap);
        set(o, &mut heap, "x", Value::Boolean(true));
        let with_x = shape_id(o, &heap);
        set(o, &mut heap, "x", Value::Boolean(false));

        assert_ne!(empty, with_x);
        assert_eq!(shape_id(o, &heap), with_x);
    }

    #[test]
    fn missing_key_is_none() {
        let mut heap = fresh_heap();
        let o = alloc_object(&mut heap).unwrap();
        assert!(get(o, &heap, "missing").is_none());
    }

    #[test]
    fn insertion_order_is_preserved() {
        let mut heap = fresh_heap();
        let o = alloc_object(&mut heap).unwrap();
        set(o, &mut heap, "a", Value::Boolean(true));
        set(o, &mut heap, "b", Value::Boolean(false));
        set(o, &mut heap, "c", Value::Null);
        let keys: Vec<String> =
            with_properties(o, &heap, |p| p.keys().map(str::to_string).collect());
        assert_eq!(keys, vec!["a", "b", "c"]);
    }

    #[test]
    fn integer_index_keys_sort_before_strings() {
        let mut heap = fresh_heap();
        let o = alloc_object(&mut heap).unwrap();
        set(o, &mut heap, "b", Value::Boolean(true));
        set(o, &mut heap, "10", Value::Boolean(true));
        set(o, &mut heap, "2", Value::Boolean(true));
        set(o, &mut heap, "a", Value::Boolean(true));
        set(o, &mut heap, "1", Value::Boolean(true));
        set(o, &mut heap, "01", Value::Boolean(true));
        set(o, &mut heap, "4294967295", Value::Boolean(true));

        let keys: Vec<String> =
            with_properties(o, &heap, |p| p.keys().map(str::to_string).collect());
        assert_eq!(keys, vec!["1", "2", "10", "b", "a", "01", "4294967295"]);
    }

    #[test]
    fn delete_removes_property() {
        let mut heap = fresh_heap();
        let o = alloc_object(&mut heap).unwrap();
        set(o, &mut heap, "x", Value::Boolean(true));
        assert!(delete(o, &mut heap, "x"));
        assert!(get(o, &heap, "x").is_none());
        // §10.1.10 — deleting a missing property still reports
        // success (returns true).
        assert!(delete(o, &mut heap, "x"));
    }

    #[test]
    fn handle_copy_shares_storage() {
        let mut heap = fresh_heap();
        let a = alloc_object(&mut heap).unwrap();
        let b = a; // Copy
        set(a, &mut heap, "x", Value::Boolean(true));
        assert_eq!(a, b);
        assert!(matches!(get(b, &heap, "x"), Some(Value::Boolean(true))));
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Counter {
        value: u32,
    }

    #[test]
    fn host_object_data_downcasts_and_mutates() {
        let mut heap = fresh_heap();
        let object = alloc_host_object(&mut heap, Counter { value: 1 }).unwrap();

        assert_eq!(
            with_host_data::<Counter, _>(object, &heap, |counter| counter.value).unwrap(),
            1
        );
        with_host_data_mut::<Counter, _>(object, &mut heap, |counter| {
            counter.value += 41;
        })
        .unwrap();
        assert_eq!(
            with_host_data::<Counter, _>(object, &heap, |counter| counter.value).unwrap(),
            42
        );
    }

    #[test]
    fn host_object_data_reports_missing_or_wrong_type() {
        let mut heap = fresh_heap();
        let ordinary = alloc_object(&mut heap).unwrap();
        assert_eq!(
            with_host_data::<Counter, _>(ordinary, &heap, |_| ()).unwrap_err(),
            HostObjectError::Missing
        );

        let object = alloc_host_object(&mut heap, "not a counter".to_string()).unwrap();
        let err = with_host_data::<Counter, _>(object, &heap, |_| ()).unwrap_err();
        assert!(matches!(err, HostObjectError::TypeMismatch { .. }));
    }

    #[test]
    fn two_literals_share_shape() {
        let mut heap = fresh_heap();
        let a = alloc_object(&mut heap).unwrap();
        set(a, &mut heap, "x", Value::Boolean(true));
        set(a, &mut heap, "y", Value::Null);
        let b = alloc_object(&mut heap).unwrap();
        set(b, &mut heap, "x", Value::Boolean(false));
        set(b, &mut heap, "y", Value::Undefined);
        assert!(Rc::ptr_eq(&shape(a, &heap), &shape(b, &heap)));
        let c = alloc_object(&mut heap).unwrap();
        set(c, &mut heap, "y", Value::Null);
        set(c, &mut heap, "x", Value::Boolean(true));
        assert!(!Rc::ptr_eq(&shape(a, &heap), &shape(c, &heap)));
    }

    #[test]
    fn overwrite_does_not_grow_shape() {
        let mut heap = fresh_heap();
        let o = alloc_object(&mut heap).unwrap();
        set(o, &mut heap, "x", Value::Boolean(true));
        let s1 = shape(o, &heap);
        set(o, &mut heap, "x", Value::Null);
        let s2 = shape(o, &heap);
        assert!(Rc::ptr_eq(&s1, &s2));
        assert_eq!(len(o, &heap), 1);
    }

    #[test]
    fn delete_switches_to_dictionary_shape() {
        let mut heap = fresh_heap();
        let o = alloc_object(&mut heap).unwrap();
        set(o, &mut heap, "a", Value::Boolean(true));
        set(o, &mut heap, "b", Value::Null);
        let before = shape(o, &heap);
        delete(o, &mut heap, "a");
        let after = shape(o, &heap);
        assert!(!Rc::ptr_eq(&before, &after));
        assert_eq!(len(o, &heap), 1);
        assert!(get(o, &heap, "a").is_none());
        assert!(matches!(get(o, &heap, "b"), Some(Value::Null)));
    }

    #[test]
    fn define_property_with_default_attrs() {
        let mut heap = fresh_heap();
        let o = alloc_object(&mut heap).unwrap();
        let desc = PropertyDescriptor::data(Value::Boolean(true), false, false, false);
        assert!(define_own_property(o, &mut heap, "x", desc));
        let got = get_own_descriptor(o, &heap, "x").unwrap();
        assert!(got.is_data());
        assert!(!got.writable());
        assert!(!got.enumerable());
        assert!(!got.configurable());
    }

    #[test]
    fn define_property_rejects_non_configurable_kind_change() {
        let mut heap = fresh_heap();
        let o = alloc_object(&mut heap).unwrap();
        define_own_property(
            o,
            &mut heap,
            "x",
            PropertyDescriptor::data(Value::Boolean(true), true, true, false),
        );
        // Try to switch the data slot to an accessor — must fail.
        let accessor = PropertyDescriptor::accessor(None, None, true, false);
        assert!(!define_own_property(o, &mut heap, "x", accessor));
    }

    #[test]
    fn ordinary_set_data_property_preserves_existing_attrs() {
        let mut heap = fresh_heap();
        let o = alloc_object(&mut heap).unwrap();
        assert!(define_own_property(
            o,
            &mut heap,
            "x",
            PropertyDescriptor::data(Value::Boolean(false), true, false, false),
        ));

        assert!(ordinary_set_data_property(
            o,
            &mut heap,
            "x",
            Value::Boolean(true)
        ));

        let got = get_own_descriptor(o, &heap, "x").unwrap();
        assert!(matches!(get(o, &heap, "x"), Some(Value::Boolean(true))));
        assert!(got.writable());
        assert!(!got.enumerable());
        assert!(!got.configurable());
    }

    #[test]
    fn ordinary_set_data_property_rejects_non_writable_data() {
        let mut heap = fresh_heap();
        let o = alloc_object(&mut heap).unwrap();
        assert!(define_own_property(
            o,
            &mut heap,
            "x",
            PropertyDescriptor::data(Value::Boolean(false), false, true, true),
        ));

        assert!(!ordinary_set_data_property(
            o,
            &mut heap,
            "x",
            Value::Boolean(true)
        ));

        assert!(matches!(get(o, &heap, "x"), Some(Value::Boolean(false))));
    }

    #[test]
    fn ordinary_set_data_property_respects_extensibility_for_new_keys() {
        let mut heap = fresh_heap();
        let o = alloc_object(&mut heap).unwrap();

        assert!(ordinary_set_data_property(o, &mut heap, "x", Value::Null));
        assert!(matches!(get(o, &heap, "x"), Some(Value::Null)));

        prevent_extensions(o, &mut heap);
        assert!(!ordinary_set_data_property(
            o,
            &mut heap,
            "y",
            Value::Boolean(true)
        ));
        assert!(get(o, &heap, "y").is_none());
    }

    #[test]
    fn freeze_makes_object_non_writable() {
        let mut heap = fresh_heap();
        let o = alloc_object(&mut heap).unwrap();
        set(o, &mut heap, "x", Value::Boolean(true));
        freeze(o, &mut heap);
        assert!(is_frozen(o, &heap));
        assert!(is_sealed(o, &heap));
        assert!(!is_extensible(o, &heap));
        // `set` is the construction-time path that doesn't honour
        // attribute flags, so it doesn't apply here. The dispatch
        // layer reaches this through `resolve_set`.
        match resolve_set(o, &heap, "x") {
            SetOutcome::Reject {
                reason: SetRejectReason::NonWritable,
            } => {}
            other => panic!("expected NonWritable rejection, got {other:?}"),
        }
    }

    #[test]
    fn seal_blocks_new_properties() {
        let mut heap = fresh_heap();
        let o = alloc_object(&mut heap).unwrap();
        set(o, &mut heap, "a", Value::Null);
        seal(o, &mut heap);
        assert!(is_sealed(o, &heap));
        assert!(!is_frozen(o, &heap));
        match resolve_set(o, &heap, "b") {
            SetOutcome::Reject {
                reason: SetRejectReason::NonExtensible,
            } => {}
            other => panic!("expected NonExtensible rejection, got {other:?}"),
        }
    }

    #[test]
    fn delete_respects_configurable() {
        let mut heap = fresh_heap();
        let o = alloc_object(&mut heap).unwrap();
        define_own_property(
            o,
            &mut heap,
            "x",
            PropertyDescriptor::data(Value::Boolean(true), true, true, false),
        );
        assert!(!delete(o, &mut heap, "x"));
        assert!(get(o, &heap, "x").is_some());
    }

    #[test]
    fn delete_symbol_missing_key_succeeds() {
        let mut heap = fresh_heap();
        let o = alloc_object(&mut heap).unwrap();
        let sym = JsSymbol::new(None);
        assert!(delete_symbol(o, &mut heap, &sym));
    }
}
