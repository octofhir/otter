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

use smallvec::SmallVec;

use crate::Value;
use crate::string::JsString;
use crate::symbol::JsSymbol;
use otter_gc::raw::{RawGc, SlotVisitor};

/// Rust-owned data attached to a JavaScript object.
///
/// Host object data is isolate-local object state. It must not hold VM `Value`,
/// `Gc`, `Local`, `NativeCtx`, or async futures; if JS values need to be held
/// across GC, use explicit GC-managed payloads and trace hooks instead.
pub trait HostObjectData: Any {}

impl<T: Any> HostObjectData for T {}

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
    prototype: JsObject,
    /// Symbol-keyed own data properties. Symbol-keyed accessors are
    /// not modelled in this slice — `Object.defineProperty` only
    /// accepts string keys today.
    symbol_props: Vec<(JsSymbol, Value)>,
    /// Rust-owned payload for host-backed objects.
    host_data: Option<Box<dyn Any>>,
    /// `[[Extensible]]` internal slot. New keys are rejected when
    /// this is `false`.
    extensible: bool,
}

impl std::fmt::Debug for ObjectBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectBody")
            .field("shape_len", &self.shape.len())
            .field("slot_count", &self.slots.len())
            .field("has_prototype", &!self.prototype.is_null())
            .field("symbol_props", &self.symbol_props.len())
            .field("has_host_data", &self.host_data.is_some())
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
        // Prototype: `Gc<ObjectBody>` (null ≡ no prototype).
        // `Gc<T>` is `#[repr(transparent)]` over `u32`, so the
        // field's storage address is a `*mut RawGc` slot the
        // scavenger may rewrite.
        if !self.prototype.is_null() {
            let p = &self.prototype as *const JsObject as *mut RawGc;
            v(p);
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
        for (_sym, val) in self.symbol_props.iter() {
            val.trace_value_slots(v);
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
        prototype: otter_gc::Gc::null(),
        symbol_props: Vec::new(),
        host_data: None,
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
        prototype: otter_gc::Gc::null(),
        symbol_props: Vec::new(),
        host_data: None,
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
        prototype: otter_gc::Gc::null(),
        symbol_props: Vec::new(),
        host_data: Some(Box::new(data)),
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

/// Read an **own** property with an accessor short-circuit:
/// returns `Some(value)` for data slots, `Some(undefined)` for
/// accessor slots (callers that need to invoke the getter must
/// use [`lookup_own`] / [`get_own_descriptor`]).
#[must_use]
pub fn get_own(obj: JsObject, heap: &otter_gc::GcHeap, key: &str) -> Option<Value> {
    heap.read_payload(obj, |body| {
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
        Some(offset) => body.slots[offset as usize].to_lookup(),
        None => PropertyLookup::Absent,
    })
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
        body.shape
            .offset_of(key)
            .map(|offset| body.slots[offset as usize].to_descriptor())
    })
}

/// Borrow the current prototype, if any.
///
/// Returns `None` when the stored handle is [`otter_gc::Gc::null()`]
/// (the in-payload encoding for JS `null`).
#[must_use]
pub fn prototype(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<JsObject> {
    heap.read_payload(obj, |body| {
        if body.prototype.is_null() {
            None
        } else {
            Some(body.prototype)
        }
    })
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
            .map(|(_, v)| v.clone())
    })
}

/// Return whether `obj` has an own symbol-keyed property.
///
/// This is the symbol-keyed counterpart to [`lookup_own`]'s
/// `PropertyLookup::Absent` probe and intentionally does not walk
/// the prototype chain.
#[must_use]
pub fn has_own_symbol(obj: JsObject, heap: &otter_gc::GcHeap, key: &JsSymbol) -> bool {
    heap.read_payload(obj, |body| {
        body.symbol_props.iter().any(|(k, _)| k.ptr_eq(key))
    })
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

/// Replace the prototype. `None` detaches the chain (encoded as
/// [`otter_gc::Gc::null()`] inside the body).
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-ordinarysetprototypeof>
pub fn set_prototype(obj: JsObject, heap: &mut otter_gc::GcHeap, proto: Option<JsObject>) {
    let new_proto = proto.unwrap_or_else(otter_gc::Gc::null);
    heap.with_payload(obj, |body| {
        body.prototype = new_proto;
    });
    heap.record_write(obj, &new_proto);
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
            parent: None,
            key: None,
            keys: new_keys,
            offsets: OnceCell::new(),
            transitions: Cell::new(HashMap::new()),
        });
        true
    })
}

/// Set or overwrite a symbol-keyed own property.
///
/// Fires the GC write barrier when `value` carries a `Gc<…>`
/// handle.
pub fn set_symbol(obj: JsObject, heap: &mut otter_gc::GcHeap, key: JsSymbol, value: Value) {
    let barrier_value = value.clone();
    heap.with_payload(obj, |body| {
        for (existing_key, slot) in body.symbol_props.iter_mut() {
            if existing_key.ptr_eq(&key) {
                *slot = value;
                return;
            }
        }
        body.symbol_props.push((key, value));
    });
    heap.record_write(obj, &barrier_value);
}

/// Remove a symbol-keyed own property.
pub fn delete_symbol(obj: JsObject, heap: &mut otter_gc::GcHeap, key: &JsSymbol) -> bool {
    heap.with_payload(obj, |body| {
        if let Some(pos) = body.symbol_props.iter().position(|(k, _)| k.ptr_eq(key)) {
            body.symbol_props.remove(pos);
            true
        } else {
            false
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
pub fn define_own_property(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    descriptor: PropertyDescriptor,
) -> bool {
    let barrier_descriptor = descriptor.clone();
    let success = heap.with_payload(obj, |body| {
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
    });
    if success {
        heap.record_write(obj, &barrier_descriptor);
    }
    success
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
    /// Iterate every `(key, data-value)` pair in insertion order,
    /// regardless of enumerability. Accessor slots are surfaced as
    /// the sentinel `Value::Undefined` — callers that need accessor
    /// fidelity must consult [`get_own_descriptor`] directly.
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
                if let DescriptorKind::Data { value: incoming_v } = &incoming.kind
                    && let SlotBody::Data { value: existing_v } = &existing.body
                    && !same_value(existing_v, incoming_v)
                {
                    return None;
                }
            }
        } else {
            // 4.f: accessor — get / set cannot change.
            if let DescriptorKind::Accessor {
                getter: in_get,
                setter: in_set,
            } = &incoming.kind
                && let SlotBody::Accessor {
                    getter: ex_get,
                    setter: ex_set,
                } = &existing.body
                && (!optional_value_eq(ex_get, in_get) || !optional_value_eq(ex_set, in_set))
            {
                return None;
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
}
