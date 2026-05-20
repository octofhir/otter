//! JavaScript object value with hidden-class shape storage and
//! ECMA-262 §6.1.7.1 property descriptors.
//!
//! Each property carries the canonical attribute triple
//! `(writable, enumerable, configurable)` plus a body that is either
//! a `[[Value]]` (data property) or a `([[Get]], [[Set]])` accessor
//! pair. Ordinary fast objects use collector-owned [`shape_body::ShapeBody`]
//! hidden classes; raw heap fixtures and delete-shaped objects fall back to a
//! per-object dictionary key list.
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
//! - [`StorePropertyTransition`] / [`StorePropertyTransitionKind`] — guarded
//!   transition records used by StoreProperty IC replay.
//! - [`ShapeCacheMode`] — fast-shape eligibility marker for current and future
//!   dictionary-compatible object storage.
//! - [`JsObject`] / [`ObjectBody`] / [`Properties`] — the public object handle,
//!   the GC-allocated storage, and the read-only view used by JSON
//!   serialisation and `Object.keys` enumeration.
//!
//! # Invariants
//! - Insertion order is encoded by the GC shape chain, or by
//!   `dictionary_keys` when an object has left fast-shape mode.
//! - A frozen object's slots all carry `writable = false` (data) and
//!   `configurable = false`; in addition the object is non-extensible.
//! - A sealed object's slots all carry `configurable = false` and the
//!   object is non-extensible (writable may still be true).
//! - Accessor descriptors never carry a `writable` bit — its slot is
//!   reused as a discriminator (always `false`).
//! - Hidden-class ICs may cache only [`ShapeCacheMode::Fast`] objects;
//!   string-keyed delete moves an object to dictionary-compatible mode.
//! - GC shape bodies are immutable after allocation; transition tables and
//!   offset maps live in interpreter-owned side caches.
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
use std::sync::atomic::{AtomicU64, Ordering};

use smallvec::SmallVec;

use crate::number::NumberValue;
use crate::property_atom::{AtomId, AtomizedPropertyKey};
use crate::proxy::JsProxy;
use crate::string::{JsString, to_utf16_vec};
use crate::symbol::JsSymbol;
use crate::{UpvalueCell, Value, read_upvalue, store_upvalue};
use otter_gc::GcHeap;
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::{RawGc, SlotVisitor};

mod descriptor;
mod descriptor_core;
mod key_order;
mod lookup;
mod shape_body;
mod shape_cache;
mod shape_runtime;
mod shape_transition;

pub use descriptor::{
    DescriptorKind, PartialPropertyDescriptor, PropertyDescriptor, PropertyFlags,
};
pub use lookup::{PropertyLookup, SetOutcome, SetRejectReason};
pub(crate) use shape_body::ShapeHandle;
pub(crate) use shape_cache::{ShapeCacheInvalidation, ShapeCacheMode};
pub(crate) use shape_runtime::ShapeRuntime;
#[cfg(test)]
pub(crate) use shape_transition::capture_store_property_transition;
pub(crate) use shape_transition::{
    StorePropertyTransition, StorePropertyTransitionKind,
    capture_store_property_transition_with_shape, replay_store_property_transition,
};

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

/// Own-property slot metadata for non-atomized named-property ICs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OwnPropertySlotHit {
    /// Shape observed on the receiver object.
    pub(crate) shape_id: ShapeId,
    /// String-keyed own-property slot offset.
    pub(crate) slot: u16,
}

/// Atom-aware property lookup result.
#[derive(Debug, Clone)]
pub(crate) struct AtomPropertyLookup {
    /// Metadata for the slot that produced [`Self::lookup`], if the hit was a
    /// string-keyed ordinary object property.
    pub(crate) hit: Option<AtomOwnPropertyHit>,
    /// Descriptor-shaped lookup result used by today's interpreter semantics.
    pub(crate) lookup: PropertyLookup,
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
    /// GC-managed hidden class for fast ordinary objects.
    shape: ShapeHandle,
    /// Fallback/dictionary identity used only when [`Self::shape`] is null.
    dictionary_shape_id: ShapeId,
    /// Fallback/dictionary string-key order used only when [`Self::shape`]
    /// is null, for raw heap fixtures and delete-shaped objects.
    dictionary_keys: Vec<String>,
    /// Whether string-keyed shape assumptions are IC-compatible.
    ///
    /// Ordinary shape transitions stay in [`ShapeCacheMode::Fast`].
    /// Deleting string-keyed own properties marks the object
    /// [`ShapeCacheMode::DictionaryCompatible`] so future dictionary storage
    /// can keep the same invalidation contract without installing stale ICs.
    shape_cache_mode: ShapeCacheMode,
    /// Slot table aligned with the GC shape chain or `dictionary_keys`.
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
    /// `[[SymbolData]]` internal slot for Symbol wrapper objects.
    symbol_data: Option<crate::symbol::JsSymbol>,
    /// `[[DateValue]]` internal slot for Date instances per
    /// ECMA-262 §21.4.5. Holds the time value as UTC epoch
    /// milliseconds (or `NaN` for an Invalid Date). Mutation goes
    /// through [`set_date_data`] which applies TimeClip
    /// (§21.4.1.6).
    date_data: Option<f64>,
    /// `[[Extensible]]` internal slot. New keys are rejected when
    /// this is `false`.
    extensible: bool,
    /// `[[ParameterMap]]` presence marker for ECMA-262 §10.4.4
    /// arguments-exotic objects. The mapping data itself lives in
    /// `host_data` (`MappedArgumentsData`) when the function uses
    /// sloppy parameter aliasing; this flag is set for both sloppy
    /// and strict arguments objects so reflective probes
    /// (`Object.prototype.toString.call(arguments)`) report the
    /// `"Arguments"` builtin tag per §20.1.3.6 step 14.b.
    is_arguments_object: bool,
}

impl std::fmt::Debug for ObjectBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectBody")
            .field("has_shape", &!self.shape.is_null())
            .field("dictionary_len", &self.dictionary_keys.len())
            .field("shape_cache_mode", &self.shape_cache_mode)
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
            .field("has_symbol_data", &self.symbol_data.is_some())
            .field("has_date_data", &self.date_data.is_some())
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
    /// The GC-managed shape handle is traced directly; dictionary keys are
    /// owned Rust strings and need no GC tracing.
    fn trace_slots_safe(&self, v: &mut SlotVisitor<'_>) {
        if !self.shape.is_null() {
            let p = &self.shape as *const ShapeHandle as *mut RawGc;
            v(p);
        }
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

fn empty_object_body() -> ObjectBody {
    ObjectBody {
        shape: ShapeHandle::null(),
        dictionary_shape_id: next_shape_id(),
        dictionary_keys: Vec::new(),
        shape_cache_mode: ShapeCacheMode::Fast,
        slots: SmallVec::new(),
        prototype: ObjectPrototype::Null,
        symbol_props: Vec::new(),
        host_data: None,
        call_native: None,
        constructor_native: None,
        boolean_data: None,
        number_data: None,
        string_data: None,
        symbol_data: None,
        date_data: None,
        extensible: true,
        is_arguments_object: false,
    }
}

fn empty_object_body_with_shape(shape: ShapeHandle) -> ObjectBody {
    let mut body = empty_object_body();
    body.shape = shape;
    body
}

/// Allocate an old-space object for raw GC fixtures.
///
/// Production VM allocation paths must use stack/runtime/native root contracts.
#[cfg(test)]
pub(crate) fn alloc_object_old_for_fixture(
    heap: &mut GcHeap,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    heap.alloc_old(empty_object_body())
}

/// Allocate a fresh empty object through the young-generation allocation path.
///
/// This is intentionally narrower than [`alloc_object`]: callers must provide
/// every stack/register root the scavenger may need to rewrite if allocation
/// triggers a minor collection. Use only at VM bytecode allocation sites that
/// can expose the live frame stack.
pub(crate) fn alloc_object_with_roots(
    heap: &mut GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    heap.alloc_with_roots(empty_object_body(), external_visit)
}

/// Allocate a fresh empty object with the root hidden class installed.
pub(crate) fn alloc_object_with_shape_roots(
    heap: &mut GcHeap,
    shape: ShapeHandle,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    heap.alloc_with_roots(empty_object_body_with_shape(shape), external_visit)
}

/// Allocate a fresh empty object for diagnostic delivery after the
/// heap cap has already fired.
///
/// This uses [`otter_gc::GcHeap::alloc_old_diagnostic`] so the VM can throw a
/// catchable `RangeError` for an allocation failure instead of immediately
/// losing the error object to the same cap.
///
/// # Errors
///
/// Surfaces cage exhaustion; heap-cap exhaustion is intentionally
/// bypassed for this diagnostic object only.
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-error-objects>
pub(crate) fn alloc_diagnostic_object(
    heap: &mut GcHeap,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    heap.alloc_old_diagnostic(empty_object_body())
}

/// Allocate a fresh object backed by Rust-owned host data.
///
/// The host data is isolate-local and intentionally not traced. It must not own
/// JS `Value` / `Gc` handles. Native methods should access it through
/// [`with_host_data`] / [`with_host_data_mut`] using the receiver from
/// [`crate::NativeCtx::this_value`].
/// Allocate a fresh host-data object while exposing caller-owned roots.
#[cfg(test)]
pub(crate) fn alloc_host_object_with_roots<T: HostObjectData>(
    heap: &mut otter_gc::GcHeap,
    data: T,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    heap.alloc_with_roots(
        ObjectBody {
            shape: ShapeHandle::null(),
            dictionary_shape_id: next_shape_id(),
            dictionary_keys: Vec::new(),
            shape_cache_mode: ShapeCacheMode::Fast,
            slots: SmallVec::new(),
            prototype: ObjectPrototype::Null,
            symbol_props: Vec::new(),
            host_data: Some(Box::new(data)),
            call_native: None,
            constructor_native: None,
            boolean_data: None,
            number_data: None,
            string_data: None,
            symbol_data: None,
            date_data: None,
            extensible: true,
            is_arguments_object: false,
        },
        external_visit,
    )
}

/// Allocate a fresh host-data object with the root hidden class installed.
pub(crate) fn alloc_host_object_with_shape_roots<T: HostObjectData>(
    heap: &mut otter_gc::GcHeap,
    shape: ShapeHandle,
    data: T,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    heap.alloc_with_roots(
        ObjectBody {
            shape,
            dictionary_shape_id: next_shape_id(),
            dictionary_keys: Vec::new(),
            shape_cache_mode: ShapeCacheMode::Fast,
            slots: SmallVec::new(),
            prototype: ObjectPrototype::Null,
            symbol_props: Vec::new(),
            host_data: Some(Box::new(data)),
            call_native: None,
            constructor_native: None,
            boolean_data: None,
            number_data: None,
            string_data: None,
            symbol_data: None,
            date_data: None,
            extensible: true,
            is_arguments_object: false,
        },
        external_visit,
    )
}

/// Mark an object as an ECMA-262 §10.4.4 arguments-exotic object so
/// reflective probes (`Object.prototype.toString.call(arguments)`)
/// emit the spec `"Arguments"` builtin tag per §20.1.3.6 step 14.b.
/// Called from `arguments_object::initialize_{mapped,unmapped}` after
/// the body's slot table is set up.
pub fn mark_as_arguments_object(obj: JsObject, heap: &mut otter_gc::GcHeap) {
    heap.with_payload(obj, |body| {
        body.is_arguments_object = true;
    });
}

/// `true` when the object was tagged as an arguments-exotic body by
/// [`mark_as_arguments_object`]. Reads the body slot through the GC
/// `read_payload` accessor so callers do not have to expose
/// [`ObjectBody`]'s internals.
#[must_use]
pub fn is_arguments_object(obj: JsObject, heap: &otter_gc::GcHeap) -> bool {
    heap.read_payload(obj, |body| body.is_arguments_object)
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
    heap.read_payload(obj, |body| body_property_count(heap, body))
}

/// `true` when the object has no string-keyed own properties.
#[must_use]
pub fn is_empty(obj: JsObject, heap: &otter_gc::GcHeap) -> bool {
    len(obj, heap) == 0
}

/// Return the object's current hidden-class id.
#[must_use]
pub(crate) fn shape_id(obj: JsObject, heap: &otter_gc::GcHeap) -> ShapeId {
    heap.read_payload(obj, |body| body_shape_id(heap, body))
}

fn body_shape_id(heap: &otter_gc::GcHeap, body: &ObjectBody) -> ShapeId {
    if !body.shape.is_null() {
        return heap.read_payload(body.shape, shape_body::ShapeBody::id);
    }
    body.dictionary_shape_id
}

fn body_property_count(heap: &otter_gc::GcHeap, body: &ObjectBody) -> usize {
    if !body.shape.is_null() {
        return shape_body::shape_property_count(heap, body.shape) as usize;
    }
    body.dictionary_keys.len()
}

pub(super) fn body_offset_of(heap: &otter_gc::GcHeap, body: &ObjectBody, key: &str) -> Option<u16> {
    if !body.shape.is_null() {
        return shape_body::shape_offset_of_str(heap, body.shape, key)
            .and_then(|offset| u16::try_from(offset).ok());
    }
    body.dictionary_keys
        .iter()
        .position(|candidate| candidate == key)
        .and_then(|offset| u16::try_from(offset).ok())
}

fn body_has_key_at(heap: &otter_gc::GcHeap, body: &ObjectBody, offset: usize) -> bool {
    if !body.shape.is_null() {
        return u32::try_from(offset)
            .ok()
            .and_then(|offset| shape_body::shape_key_at_offset(heap, body.shape, offset))
            .is_some();
    }
    body.dictionary_keys.get(offset).is_some()
}

fn body_key_matches(heap: &otter_gc::GcHeap, body: &ObjectBody, offset: usize, key: &str) -> bool {
    if !body.shape.is_null() {
        return u32::try_from(offset).ok().is_some_and(|offset| {
            shape_body::shape_key_matches_str(heap, body.shape, offset, key)
        });
    }
    matches!(body.dictionary_keys.get(offset), Some(name) if name == key)
}

/// `true` when hidden-class ICs may cache this object's string-keyed slots.
///
/// This excludes string exotic wrappers and objects that have taken delete-like
/// mutations reserved for future dictionary storage.
#[must_use]
pub(crate) fn supports_fast_property_ic(obj: JsObject, heap: &otter_gc::GcHeap) -> bool {
    heap.read_payload(obj, shape_cache::supports_fast_property_ic)
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
        body_offset_of(heap, body, key).map(|offset| match &body.slots[offset as usize].body {
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
    heap.read_payload(obj, |body| match body_offset_of(heap, body, key) {
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

/// Own-property probe that also returns shape/slot metadata for IC install.
#[must_use]
pub(crate) fn lookup_own_slot(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    key: &str,
) -> (Option<OwnPropertySlotHit>, PropertyLookup) {
    heap.read_payload(obj, |body| match body_offset_of(heap, body, key) {
        Some(offset) => {
            let mut lookup = body.slots[offset as usize].to_lookup();
            if let Some(cell) = mapped_argument_cell(body, key)
                && let PropertyLookup::Data { value, .. } = &mut lookup
            {
                *value = read_upvalue(heap, cell);
            }
            (
                Some(OwnPropertySlotHit {
                    shape_id: body_shape_id(heap, body),
                    slot: offset,
                }),
                lookup,
            )
        }
        None => (None, PropertyLookup::Absent),
    })
}

/// Atom-aware own-property probe for named property bytecodes.
#[must_use]
pub(crate) fn lookup_own_atom(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
) -> AtomPropertyLookup {
    heap.read_payload(obj, |body| match body_offset_of(heap, body, key.name()) {
        Some(offset) => {
            let mut lookup = body.slots[offset as usize].to_lookup();
            if let Some(cell) = mapped_argument_cell(body, key.name())
                && let PropertyLookup::Data { value, .. } = &mut lookup
            {
                *value = read_upvalue(heap, cell);
            }
            AtomPropertyLookup {
                hit: Some(AtomOwnPropertyHit {
                    shape_id: body_shape_id(heap, body),
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

/// Validate that a cached own-property slot is still present.
#[must_use]
pub(crate) fn has_own_slot(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    hit: OwnPropertySlotHit,
) -> bool {
    heap.read_payload(obj, |body| {
        if body_shape_id(heap, body) != hit.shape_id {
            return false;
        }
        let offset = hit.slot as usize;
        body_has_key_at(heap, body, offset) && body.slots.get(offset).is_some()
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
        if body_shape_id(heap, body) != hit.shape_id || key.atom().id() != hit.atom_id {
            return None;
        }
        let offset = hit.slot as usize;
        if !body_key_matches(heap, body, offset, key.name()) {
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
    value: &Value,
) -> Option<()> {
    let mapped_cell = heap.read_payload(obj, |body| mapped_argument_cell(body, key.name()));
    let current_shape_id = shape_id(obj, heap);
    let key_matches = heap.read_payload(obj, |body| {
        body_key_matches(heap, body, hit.slot as usize, key.name())
    });
    let success = heap.with_payload(obj, |body| {
        let offset = hit.slot as usize;
        if current_shape_id != hit.shape_id || key.atom().id() != hit.atom_id || !key_matches {
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
        *stored = value.clone();
        true
    });
    if !success {
        return None;
    }
    if let Some(cell) = mapped_cell {
        store_upvalue(heap, cell, value.clone());
    }
    heap.record_write(obj, value);
    Some(())
}

fn has_writable_own_data_slot_atom(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    atom_id: AtomId,
    hit: AtomOwnPropertyHit,
) -> bool {
    heap.read_payload(obj, |body| {
        if body_shape_id(heap, body) != hit.shape_id || atom_id != hit.atom_id {
            return false;
        }
        let offset = hit.slot as usize;
        if !body_has_key_at(heap, body, offset) {
            return false;
        }
        let Some(slot) = body.slots.get(offset) else {
            return false;
        };
        slot.flags.writable() && matches!(slot.body, SlotBody::Data { .. })
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
        body_offset_of(heap, body, key).map(|offset| {
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

/// Store the `[[SymbolData]]` internal slot for a Symbol wrapper.
pub fn set_symbol_data(obj: JsObject, heap: &mut otter_gc::GcHeap, value: crate::symbol::JsSymbol) {
    heap.with_payload(obj, |body| {
        body.symbol_data = Some(value);
    });
}

/// Read the `[[SymbolData]]` internal slot for a Symbol wrapper.
#[must_use]
pub fn symbol_data(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<crate::symbol::JsSymbol> {
    heap.read_payload(obj, |body| body.symbol_data.clone())
}

/// §21.4.1.6 TimeClip — every store into a `[[DateValue]]` internal
/// slot must clip non-finite values and values past ±8.64×10¹⁵ ms
/// to `NaN`, then truncate toward zero so the spec invariant "the
/// time value is an integer" holds.
#[must_use]
pub fn clip_date_value(ms: f64) -> f64 {
    if !ms.is_finite() || ms.abs() > 8.64e15 {
        f64::NAN
    } else {
        ms.trunc()
    }
}

/// Store the `[[DateValue]]` internal slot for a Date instance.
/// Applies §21.4.1.6 TimeClip before writing.
pub fn set_date_data(obj: JsObject, heap: &mut otter_gc::GcHeap, value: f64) {
    let clipped = clip_date_value(value);
    heap.with_payload(obj, |body| {
        body.date_data = Some(clipped);
    });
}

/// Read the `[[DateValue]]` internal slot for a Date instance.
/// Returns `None` for non-Date objects so callers can detect a
/// receiver-brand mismatch (§21.4.1.1 `thisTimeValue` step 3).
#[must_use]
pub fn date_data(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<f64> {
    heap.read_payload(obj, |body| body.date_data)
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

/// Borrow the GC-managed hidden class, if installed.
#[must_use]
pub(crate) fn shape(obj: JsObject, heap: &otter_gc::GcHeap) -> ShapeHandle {
    heap.read_payload(obj, |body| body.shape)
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
    let existing_offset = heap.read_payload(obj, |body| body_offset_of(heap, body, key));
    heap.with_payload(obj, |body| {
        if let Some(offset) = existing_offset {
            let slot = &mut body.slots[offset as usize];
            slot.body = SlotBody::Data { value };
            return;
        }
        body.dictionary_shape_id = next_shape_id();
        body.dictionary_keys.push(key.to_owned());
        body.shape = ShapeHandle::null();
        body.slots.push(PropertySlot::data_default(value));
    });
    heap.record_write(obj, &barrier_value);
}

/// Construction-time data store for callers that already allocated the next
/// GC-managed hidden class.
pub(crate) fn set_with_shape(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    value: Value,
    next_shape: ShapeHandle,
) {
    let barrier_value = value.clone();
    let existing_offset = heap.read_payload(obj, |body| body_offset_of(heap, body, key));
    heap.with_payload(obj, |body| {
        if let Some(offset) = existing_offset {
            let slot = &mut body.slots[offset as usize];
            slot.body = SlotBody::Data { value };
            return;
        }
        body.shape = next_shape;
        body.slots.push(PropertySlot::data_default(value));
    });
    heap.record_write(obj, &barrier_value);
    heap.record_write(obj, &next_shape);
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

pub(crate) fn ordinary_set_data_property_with_shape(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    value: Value,
    next_shape: ShapeHandle,
) -> bool {
    let mapped_cell = heap.read_payload(obj, |body| mapped_argument_cell(body, key));
    let success = descriptor_core::ordinary_set_data_property_with_shape(
        obj,
        heap,
        key,
        value.clone(),
        next_shape,
    );
    if success && let Some(cell) = mapped_cell {
        store_upvalue(heap, cell, value);
    }
    success
}

/// Replace the prototype with a spec-legal value. `None` or
/// `Some(Value::Null)` detaches the chain.
///
/// Implements `OrdinarySetPrototypeOf` per ECMA-262 §10.1.2.1 — the
/// `SameValue(V, current)` early-return, the non-extensibility
/// guard, and the new-prototype cycle walk. Returns `false` for any
/// abrupt outcome so callers (the `__proto__` setter,
/// `Object.setPrototypeOf`, `Reflect.setPrototypeOf`) can raise the
/// spec-mandated `TypeError`.
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
    // §10.1.2.1 step 4 — `SameValue(V, current) is true → return true`.
    let current = heap.read_payload(obj, |body| body.prototype.clone());
    if prototype_same(&current, &new_proto) {
        return true;
    }
    // §10.1.2.1 step 5 — non-extensible objects reject any change.
    if !is_extensible(obj, heap) {
        return false;
    }
    // §10.1.2.1 step 8 — walk the new chain; abort with `false` if
    // any hop lands back on `obj` (cycle) or strays past
    // `PROTO_CHAIN_HARD_CAP` (foundation safety net for adversarial
    // inputs). Non-ordinary prototypes (Proxy / Value variants)
    // terminate the walk per step 8.c.i — their `[[GetPrototypeOf]]`
    // is not `OrdinaryGetPrototypeOf`, so the spec stops following
    // the chain.
    let mut cursor = new_proto.clone();
    let mut hops = 0usize;
    loop {
        match cursor {
            ObjectPrototype::Null => break,
            ObjectPrototype::Object(p) => {
                if p == obj {
                    return false;
                }
                if hops >= PROTO_CHAIN_HARD_CAP {
                    return false;
                }
                hops += 1;
                cursor = heap.read_payload(p, |body| body.prototype.clone());
            }
            ObjectPrototype::Proxy(_) | ObjectPrototype::Value(_) => break,
        }
    }
    let barrier_value = new_proto.as_value();
    heap.with_payload(obj, |body| {
        body.prototype = new_proto;
    });
    if let Some(value) = &barrier_value {
        heap.record_write(obj, value);
    }
    true
}

fn prototype_same(a: &ObjectPrototype, b: &ObjectPrototype) -> bool {
    match (a, b) {
        (ObjectPrototype::Null, ObjectPrototype::Null) => true,
        (ObjectPrototype::Object(x), ObjectPrototype::Object(y)) => x == y,
        (ObjectPrototype::Proxy(x), ObjectPrototype::Proxy(y)) => x.ptr_eq(y),
        (ObjectPrototype::Value(x), ObjectPrototype::Value(y)) => same_prototype_value(x, y),
        _ => false,
    }
}

fn same_prototype_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => x == y,
        (Value::Array(x), Value::Array(y)) => crate::array::ptr_eq(*x, *y),
        _ => false,
    }
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
    let existing_offset = heap.read_payload(obj, |body| body_offset_of(heap, body, key));
    let replacement_keys = heap.read_payload(obj, |body| {
        let mut keys = string_keys_in_shape_order(heap, body);
        if let Some(offset) = existing_offset {
            let offset = offset as usize;
            if offset < keys.len() {
                keys.remove(offset);
            }
        }
        keys
    });
    heap.with_payload(obj, |body| {
        let Some(offset) = existing_offset else {
            // Spec step 2: missing → true.
            return true;
        };
        if !body.slots[offset as usize].flags.configurable() {
            return false;
        }
        body.slots.remove(offset as usize);
        body.dictionary_shape_id = next_shape_id();
        body.dictionary_keys = replacement_keys;
        body.shape = ShapeHandle::null();
        shape_cache::invalidate_fast_shape_assumptions(
            body,
            ShapeCacheInvalidation::DeleteOwnProperty,
        );
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
    let existing_offset = heap.read_payload(obj, |body| body_offset_of(heap, body, key));
    let dictionary_keys = dictionary_keys_for_shape_transition(heap, obj, existing_offset);
    let success = heap.with_payload(obj, |body| {
        if let Some(offset) = existing_offset {
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
            body.dictionary_shape_id = next_shape_id();
            if let Some(dictionary_keys) = dictionary_keys {
                body.dictionary_keys = dictionary_keys;
            }
            body.dictionary_keys.push(key.to_owned());
            body.shape = ShapeHandle::null();
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

pub(crate) fn define_own_property_partial_with_shape(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    descriptor: PartialPropertyDescriptor,
    next_shape: ShapeHandle,
) -> bool {
    let completed = descriptor.complete_for_new_property();
    let barrier_descriptor = completed.clone();
    let map_descriptor = completed.clone();
    let existing_offset = heap.read_payload(obj, |body| body_offset_of(heap, body, key));
    let success = heap.with_payload(obj, |body| {
        if let Some(offset) = existing_offset {
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
            body.shape = next_shape;
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
        heap.record_write(obj, &next_shape);
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
    let existing_offset = heap.read_payload(obj, |body| body_offset_of(heap, body, key));
    let dictionary_keys = dictionary_keys_for_shape_transition(heap, obj, existing_offset);
    let success = heap.with_payload(obj, |body| {
        if let Some(offset) = existing_offset {
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
            body.dictionary_shape_id = next_shape_id();
            if let Some(dictionary_keys) = dictionary_keys {
                body.dictionary_keys = dictionary_keys;
            }
            body.dictionary_keys.push(key.to_owned());
            body.shape = ShapeHandle::null();
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
    string_keys: Vec<(String, usize)>,
}

impl<'a> Properties<'a> {
    /// Iterate every `(key, data-value)` pair in ordinary own-key
    /// order, regardless of enumerability. Accessor slots are
    /// surfaced as the sentinel `Value::Undefined` — callers that
    /// need accessor fidelity must consult [`get_own_descriptor`]
    /// directly.
    pub fn iter(&self) -> impl Iterator<Item = (&str, Value)> {
        self.string_keys.iter().map(|(key, idx)| {
            let slot = &self.body.slots[*idx];
            let value = match &slot.body {
                SlotBody::Data { value } => value.clone(),
                SlotBody::Accessor { .. } => Value::Undefined,
            };
            (key.as_str(), value)
        })
    }

    /// Iterate string keys in ordinary own-key order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.string_keys.iter().map(|(key, _)| key.as_str())
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
        self.string_keys.iter().filter_map(|(key, idx)| {
            let slot = &self.body.slots[*idx];
            if !slot.flags.enumerable() {
                return None;
            }
            match &slot.body {
                SlotBody::Data { value } => Some((key.as_str(), value.clone())),
                SlotBody::Accessor { .. } => None,
            }
        })
    }

    /// Iterate enumerable own-key names (string-keyed only) in
    /// ordinary own-key order.
    pub fn enumerable_keys(&self) -> impl Iterator<Item = &str> {
        self.string_keys.iter().filter_map(|(key, idx)| {
            self.body.slots[*idx]
                .flags
                .enumerable()
                .then_some(key.as_str())
        })
    }

    /// Iterate `(symbol, data-value)` pairs over enumerable
    /// symbol-keyed own data properties in insertion order. Used by
    /// `Object.assign` (§20.1.2.1 step 4.c.ii) which copies every
    /// enumerable own string *and* symbol key from the source.
    pub fn enumerable_symbol_data_iter(&self) -> impl Iterator<Item = (JsSymbol, Value)> + '_ {
        self.body.symbol_props.iter().filter_map(|(sym, slot)| {
            if !slot.flags.enumerable() {
                return None;
            }
            match &slot.body {
                SlotBody::Data { value } => Some((sym.clone(), value.clone())),
                SlotBody::Accessor { .. } => None,
            }
        })
    }
}

fn ordinary_string_key_entries(heap: &otter_gc::GcHeap, body: &ObjectBody) -> Vec<(String, usize)> {
    let insertion_order = if !body.shape.is_null() {
        shape_body::shape_keys_ordered(heap, body.shape)
            .into_iter()
            .map(|(key, offset)| {
                (
                    String::from_utf16_lossy(&to_utf16_vec(heap, key)),
                    offset as usize,
                )
            })
            .collect()
    } else {
        body.dictionary_keys
            .iter()
            .enumerate()
            .map(|(slot, key)| (key.to_string(), slot))
            .collect()
    };

    order_string_key_entries(insertion_order)
}

fn string_keys_in_shape_order(heap: &otter_gc::GcHeap, body: &ObjectBody) -> Vec<String> {
    if !body.shape.is_null() {
        return shape_body::shape_keys_ordered(heap, body.shape)
            .into_iter()
            .map(|(key, _)| String::from_utf16_lossy(&to_utf16_vec(heap, key)))
            .collect();
    }
    body.dictionary_keys.clone()
}

fn dictionary_keys_for_shape_transition(
    heap: &otter_gc::GcHeap,
    obj: JsObject,
    existing_offset: Option<u16>,
) -> Option<Vec<String>> {
    if existing_offset.is_some() {
        return None;
    }
    heap.read_payload(obj, |body| {
        (!body.shape.is_null()).then(|| string_keys_in_shape_order(heap, body))
    })
}

fn order_string_key_entries(entries: Vec<(String, usize)>) -> Vec<(String, usize)> {
    let mut integer_indices = Vec::new();
    let mut string_keys = Vec::new();

    for (key, slot) in entries {
        if let Some(array_index) = key_order::array_index_property_name(&key) {
            integer_indices.push((array_index, key, slot));
        } else {
            string_keys.push((key, slot));
        }
    }

    integer_indices.sort_by_key(|(array_index, _, _)| *array_index);

    let mut ordered = Vec::with_capacity(integer_indices.len() + string_keys.len());
    ordered.extend(
        integer_indices
            .into_iter()
            .map(|(_, key, slot)| (key, slot)),
    );
    ordered.extend(string_keys);
    ordered
}

/// Run `f` with a [`Properties`] snapshot of `obj`'s string-keyed
/// and symbol-keyed own properties. The view does not escape the
/// closure scope.
pub fn with_properties<R>(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    f: impl FnOnce(Properties<'_>) -> R,
) -> R {
    heap.read_payload(obj, |body| {
        let string_keys = ordinary_string_key_entries(heap, body);
        f(Properties { body, string_keys })
    })
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
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
        assert!(is_empty(o, &heap));
        assert_eq!(len(o, &heap), 0);
        assert!(shape(o, &heap).is_null());
    }

    #[test]
    fn runtime_object_allocation_installs_shape_root() {
        let mut interp = crate::Interpreter::new();
        let o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");

        assert_eq!(shape(o, interp.gc_heap()), interp.shape_root());
    }

    #[test]
    fn runtime_data_assignment_advances_shape() {
        let mut interp = crate::Interpreter::new();
        let o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");

        assert!(
            interp
                .ordinary_set_data_property(o, "x", Value::Boolean(true))
                .expect("set")
        );

        let shape_handle = shape(o, interp.gc_heap());
        assert_eq!(interp.shape_offset_of(shape_handle, "x"), Some(0));
        assert_eq!(
            interp
                .gc_heap()
                .read_payload(o, |body| body.dictionary_keys.len()),
            0
        );
    }

    #[test]
    fn runtime_construction_set_advances_shape() {
        let mut interp = crate::Interpreter::new();
        let o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");

        interp
            .set_property(o, "value", Value::Number(NumberValue::from_i32(1)))
            .expect("set value");
        interp
            .set_property(o, "done", Value::Boolean(false))
            .expect("set done");

        let shape_handle = shape(o, interp.gc_heap());
        assert_eq!(interp.shape_offset_of(shape_handle, "value"), Some(0));
        assert_eq!(interp.shape_offset_of(shape_handle, "done"), Some(1));
        assert_eq!(
            interp
                .gc_heap()
                .read_payload(o, |body| body.dictionary_keys.len()),
            0
        );
    }

    #[test]
    fn shape_id_prefers_installed_shape() {
        let mut interp = crate::Interpreter::new();
        let o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");

        interp
            .set_property(o, "x", Value::Boolean(true))
            .expect("set x");

        let shape_handle = shape(o, interp.gc_heap());
        let installed_shape_id = interp
            .gc_heap()
            .read_payload(shape_handle, shape_body::ShapeBody::id);
        assert_eq!(shape_id(o, interp.gc_heap()), installed_shape_id);
        assert_ne!(
            shape_id(o, interp.gc_heap()),
            interp
                .gc_heap()
                .read_payload(o, |body| body.dictionary_shape_id)
        );
    }

    #[test]
    fn own_property_reads_prefer_installed_shape_offsets() {
        let mut interp = crate::Interpreter::new();
        let o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");

        interp
            .set_property(o, "x", Value::Boolean(true))
            .expect("set x");
        interp.gc_heap_mut().with_payload(o, |body| {
            body.dictionary_keys.clear();
            body.dictionary_shape_id = next_shape_id();
        });

        assert_eq!(len(o, interp.gc_heap()), 1);
        assert_eq!(
            get_own(o, interp.gc_heap(), "x"),
            Some(Value::Boolean(true))
        );
        assert!(matches!(
            lookup_own(o, interp.gc_heap(), "x"),
            PropertyLookup::Data {
                value: Value::Boolean(true),
                ..
            }
        ));
        assert!(get_own_descriptor(o, interp.gc_heap(), "x").is_some());
        let keys: Vec<String> = with_properties(o, interp.gc_heap(), |p| {
            p.keys().map(str::to_string).collect()
        });
        assert_eq!(keys, vec!["x"]);

        set(o, interp.gc_heap_mut(), "x", Value::Boolean(false));

        assert_eq!(
            get_own(o, interp.gc_heap(), "x"),
            Some(Value::Boolean(false))
        );
        assert_eq!(interp.gc_heap().read_payload(o, |body| body.slots.len()), 1);
    }

    #[test]
    fn runtime_define_property_advances_shape() {
        let mut interp = crate::Interpreter::new();
        let o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");
        let descriptor = PartialPropertyDescriptor {
            value: Some(Value::Number(NumberValue::from_i32(42))),
            writable: Some(true),
            enumerable: Some(true),
            configurable: Some(true),
            ..PartialPropertyDescriptor::default()
        };

        assert!(
            interp
                .define_own_property_partial(o, "answer", descriptor)
                .expect("define")
        );

        let shape_handle = shape(o, interp.gc_heap());
        assert_eq!(interp.shape_offset_of(shape_handle, "answer"), Some(0));
        assert_eq!(
            interp
                .gc_heap()
                .read_payload(o, |body| body.dictionary_keys.len()),
            0
        );
    }

    #[test]
    fn runtime_delete_invalidates_shape() {
        let mut interp = crate::Interpreter::new();
        let o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");

        interp
            .set_property(o, "a", Value::Boolean(true))
            .expect("set a");
        interp.set_property(o, "b", Value::Null).expect("set b");

        let before = shape(o, interp.gc_heap());
        assert!(!before.is_null());
        assert_eq!(interp.shape_offset_of(before, "b"), Some(1));

        assert!(delete(o, interp.gc_heap_mut(), "a"));

        assert!(shape(o, interp.gc_heap()).is_null());
        assert!(get(o, interp.gc_heap(), "a").is_none());
        assert!(matches!(get(o, interp.gc_heap(), "b"), Some(Value::Null)));
    }

    #[test]
    fn runtime_store_transition_invalidates_shape() {
        let mut interp = crate::Interpreter::new();
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_constant_index(7)),
            "x",
        );
        let first = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("first object");

        let transition = capture_store_property_transition(
            first,
            interp.gc_heap_mut(),
            key,
            &Value::Boolean(true),
        )
        .expect("transition install");

        assert!(shape(first, interp.gc_heap()).is_null());
        assert_eq!(
            get_own(first, interp.gc_heap(), "x"),
            Some(Value::Boolean(true))
        );

        let second = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("second object");
        assert_eq!(shape(second, interp.gc_heap()), interp.shape_root());

        assert_eq!(
            replay_store_property_transition(
                second,
                interp.gc_heap_mut(),
                key,
                &transition,
                &Value::Null,
            ),
            Some(())
        );

        assert!(shape(second, interp.gc_heap()).is_null());
        assert_eq!(get_own(second, interp.gc_heap(), "x"), Some(Value::Null));
    }

    #[test]
    fn raw_set_invalidates_shape_for_new_property() {
        let mut interp = crate::Interpreter::new();
        let o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");
        assert_eq!(shape(o, interp.gc_heap()), interp.shape_root());

        set(o, interp.gc_heap_mut(), "x", Value::Boolean(true));

        assert!(shape(o, interp.gc_heap()).is_null());
        assert_eq!(
            get_own(o, interp.gc_heap(), "x"),
            Some(Value::Boolean(true))
        );
    }

    #[test]
    fn raw_ordinary_set_invalidates_shape_for_new_property() {
        let mut interp = crate::Interpreter::new();
        let o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");
        assert_eq!(shape(o, interp.gc_heap()), interp.shape_root());

        assert!(ordinary_set_data_property(
            o,
            interp.gc_heap_mut(),
            "x",
            Value::Boolean(true)
        ));

        assert!(shape(o, interp.gc_heap()).is_null());
        assert_eq!(
            get_own(o, interp.gc_heap(), "x"),
            Some(Value::Boolean(true))
        );
    }

    #[test]
    fn raw_define_property_invalidates_shape_for_new_property() {
        let mut interp = crate::Interpreter::new();
        let o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");
        assert_eq!(shape(o, interp.gc_heap()), interp.shape_root());

        assert!(define_own_property(
            o,
            interp.gc_heap_mut(),
            "x",
            PropertyDescriptor::data(Value::Boolean(true), true, true, true),
        ));

        assert!(shape(o, interp.gc_heap()).is_null());
        assert_eq!(
            get_own(o, interp.gc_heap(), "x"),
            Some(Value::Boolean(true))
        );
    }

    #[test]
    fn raw_define_property_partial_invalidates_shape_for_new_property() {
        let mut interp = crate::Interpreter::new();
        let o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");
        assert_eq!(shape(o, interp.gc_heap()), interp.shape_root());
        let descriptor = PartialPropertyDescriptor {
            value: Some(Value::Boolean(true)),
            writable: Some(true),
            enumerable: Some(true),
            configurable: Some(true),
            ..PartialPropertyDescriptor::default()
        };

        assert!(define_own_property_partial(
            o,
            interp.gc_heap_mut(),
            "x",
            descriptor,
        ));

        assert!(shape(o, interp.gc_heap()).is_null());
        assert_eq!(
            get_own(o, interp.gc_heap(), "x"),
            Some(Value::Boolean(true))
        );
    }

    #[test]
    fn set_then_get_roundtrip() {
        let mut heap = fresh_heap();
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(o, &mut heap, "x", Value::Boolean(true));
        assert!(matches!(get(o, &heap, "x"), Some(Value::Boolean(true))));
    }

    #[test]
    fn atom_lookup_reports_shape_and_slot_metadata() {
        let mut heap = fresh_heap();
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
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
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
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
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(o, &mut heap, "x", Value::Boolean(true));
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_constant_index(7)),
            "x",
        );
        let hit = lookup_own_atom(o, &heap, key).hit.expect("atom hit");

        assert_eq!(
            store_own_data_slot_atom(o, &mut heap, key, hit, &Value::Boolean(false)),
            Some(())
        );
        assert_eq!(
            load_own_data_slot_atom(o, &heap, key, hit),
            Some(Value::Boolean(false))
        );

        set(o, &mut heap, "y", Value::Null);

        assert_eq!(
            store_own_data_slot_atom(o, &mut heap, key, hit, &Value::Boolean(true)),
            None
        );
    }

    #[test]
    fn raw_atom_add_transition_rejects_unshared_dictionary_shape() {
        let mut heap = fresh_heap();
        let proto = alloc_object_old_for_fixture(&mut heap).unwrap();
        let first = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(first, &mut heap, Some(proto));
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_constant_index(7)),
            "x",
        );
        let transition =
            capture_store_property_transition(first, &mut heap, key, &Value::Boolean(true))
                .expect("transition install");
        assert!(matches!(
            transition.kind,
            StorePropertyTransitionKind::DirectPrototypeMissing { .. }
        ));

        let second = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(second, &mut heap, Some(proto));

        assert_eq!(
            replay_store_property_transition(
                second,
                &mut heap,
                key,
                &transition,
                &Value::Boolean(false),
            ),
            None
        );
        assert_eq!(get_own(second, &heap, "x"), None);
    }

    #[test]
    fn atom_add_transition_rejects_changed_direct_prototype_shape() {
        let mut heap = fresh_heap();
        let proto = alloc_object_old_for_fixture(&mut heap).unwrap();
        let first = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(first, &mut heap, Some(proto));
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_constant_index(7)),
            "x",
        );
        let transition =
            capture_store_property_transition(first, &mut heap, key, &Value::Boolean(true))
                .expect("transition install");
        set(proto, &mut heap, "x", Value::Null);

        let second = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(second, &mut heap, Some(proto));

        assert_eq!(
            replay_store_property_transition(
                second,
                &mut heap,
                key,
                &transition,
                &Value::Boolean(false),
            ),
            None
        );
    }

    #[test]
    fn atom_add_transition_rejects_deeper_prototype_after_mutation() {
        let mut heap = fresh_heap();
        let proto = alloc_object_old_for_fixture(&mut heap).unwrap();
        let first = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(first, &mut heap, Some(proto));
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_constant_index(7)),
            "x",
        );
        let transition =
            capture_store_property_transition(first, &mut heap, key, &Value::Boolean(true))
                .expect("transition install");
        let deep_proto = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(proto, &mut heap, Some(deep_proto));

        let second = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(second, &mut heap, Some(proto));

        assert_eq!(
            replay_store_property_transition(
                second,
                &mut heap,
                key,
                &transition,
                &Value::Boolean(false),
            ),
            None
        );
    }

    #[test]
    fn raw_atom_add_transition_rejects_unshared_inherited_dictionary_shape() {
        let mut heap = fresh_heap();
        let proto = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(proto, &mut heap, "x", Value::Boolean(true));
        let first = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(first, &mut heap, Some(proto));
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_constant_index(7)),
            "x",
        );
        let transition =
            capture_store_property_transition(first, &mut heap, key, &Value::Boolean(false))
                .expect("transition install");
        assert!(matches!(
            transition.kind,
            StorePropertyTransitionKind::DirectPrototypeWritableData { .. }
        ));

        let second = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(second, &mut heap, Some(proto));

        assert_eq!(
            replay_store_property_transition(second, &mut heap, key, &transition, &Value::Null,),
            None
        );
        assert_eq!(get_own(second, &heap, "x"), None);
        assert_eq!(get_own(proto, &heap, "x"), Some(Value::Boolean(true)));
    }

    #[test]
    fn atom_add_transition_rejects_inherited_data_after_writable_change() {
        let mut heap = fresh_heap();
        let proto = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(proto, &mut heap, "x", Value::Boolean(true));
        let first = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(first, &mut heap, Some(proto));
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_constant_index(7)),
            "x",
        );
        let transition =
            capture_store_property_transition(first, &mut heap, key, &Value::Boolean(false))
                .expect("transition install");
        assert!(define_own_property(
            proto,
            &mut heap,
            "x",
            PropertyDescriptor::data(Value::Boolean(true), false, true, true),
        ));

        let second = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(second, &mut heap, Some(proto));

        assert_eq!(
            replay_store_property_transition(second, &mut heap, key, &transition, &Value::Null,),
            None
        );
    }

    #[test]
    fn atom_add_transition_rejects_inherited_non_writable_data() {
        let mut heap = fresh_heap();
        let proto = alloc_object_old_for_fixture(&mut heap).unwrap();
        assert!(define_own_property(
            proto,
            &mut heap,
            "x",
            PropertyDescriptor::data(Value::Boolean(true), false, true, true),
        ));
        let receiver = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(receiver, &mut heap, Some(proto));
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_constant_index(7)),
            "x",
        );

        assert!(
            capture_store_property_transition(receiver, &mut heap, key, &Value::Null).is_none()
        );
        assert!(get_own(receiver, &heap, "x").is_none());
    }

    #[test]
    fn shape_id_changes_on_new_property_not_overwrite() {
        let mut heap = fresh_heap();
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
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
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
        assert!(get(o, &heap, "missing").is_none());
    }

    #[test]
    fn insertion_order_is_preserved() {
        let mut heap = fresh_heap();
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
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
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
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
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
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
        let a = alloc_object_old_for_fixture(&mut heap).unwrap();
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
        let mut roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let object =
            alloc_host_object_with_roots(&mut heap, Counter { value: 1 }, &mut roots).unwrap();

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
        let ordinary = alloc_object_old_for_fixture(&mut heap).unwrap();
        assert_eq!(
            with_host_data::<Counter, _>(ordinary, &heap, |_| ()).unwrap_err(),
            HostObjectError::Missing
        );

        let mut roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let object =
            alloc_host_object_with_roots(&mut heap, "not a counter".to_string(), &mut roots)
                .unwrap();
        let err = with_host_data::<Counter, _>(object, &heap, |_| ()).unwrap_err();
        assert!(matches!(err, HostObjectError::TypeMismatch { .. }));
    }

    #[test]
    fn overwrite_does_not_grow_shape() {
        let mut heap = fresh_heap();
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(o, &mut heap, "x", Value::Boolean(true));
        let s1 = shape_id(o, &heap);
        set(o, &mut heap, "x", Value::Null);
        let s2 = shape_id(o, &heap);
        assert_eq!(s1, s2);
        assert_eq!(len(o, &heap), 1);
    }

    #[test]
    fn delete_switches_to_dictionary_shape() {
        let mut heap = fresh_heap();
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(o, &mut heap, "a", Value::Boolean(true));
        set(o, &mut heap, "b", Value::Null);
        let before = shape_id(o, &heap);
        assert!(supports_fast_property_ic(o, &heap));
        delete(o, &mut heap, "a");
        let after = shape_id(o, &heap);
        assert_ne!(before, after);
        assert!(!supports_fast_property_ic(o, &heap));
        assert_eq!(len(o, &heap), 1);
        assert!(get(o, &heap, "a").is_none());
        assert!(matches!(get(o, &heap, "b"), Some(Value::Null)));
    }

    #[test]
    fn define_property_with_default_attrs() {
        let mut heap = fresh_heap();
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
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
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
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
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
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
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
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
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();

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
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(o, &mut heap, "x", Value::Boolean(true));
        freeze(o, &mut heap);
        assert!(is_frozen(o, &heap));
        assert!(is_sealed(o, &heap));
        assert!(!is_extensible(o, &heap));
        // `set` is the construction-time path that doesn't honour
        // attribute flags, so it doesn't apply here. The dispatch
        // layer reaches this through `resolve_set`.
        match resolve_set(o, &mut heap, "x") {
            SetOutcome::Reject {
                reason: SetRejectReason::NonWritable,
            } => {}
            other => panic!("expected NonWritable rejection, got {other:?}"),
        }
    }

    #[test]
    fn seal_blocks_new_properties() {
        let mut heap = fresh_heap();
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(o, &mut heap, "a", Value::Null);
        seal(o, &mut heap);
        assert!(is_sealed(o, &heap));
        assert!(!is_frozen(o, &heap));
        match resolve_set(o, &mut heap, "b") {
            SetOutcome::Reject {
                reason: SetRejectReason::NonExtensible,
            } => {}
            other => panic!("expected NonExtensible rejection, got {other:?}"),
        }
    }

    #[test]
    fn delete_respects_configurable() {
        let mut heap = fresh_heap();
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
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
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
        let sym = JsSymbol::new(None);
        assert!(delete_symbol(o, &mut heap, &sym));
    }
}
