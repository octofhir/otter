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
//! - [`StorePropertyTransition`] / [`StorePropertyTransitionKind`] and
//!   [`LowerableStoreTransition`] — guarded StoreProperty replay records and
//!   their allocation-free native subset.
//! - [`ShapeCacheMode`] — fast-shape eligibility marker for current and future
//!   dictionary-compatible object storage.
//! - [`JsObject`] / [`ObjectBody`] / [`Properties`] — the public object handle,
//!   the GC-allocated storage, and the read-only view used by JSON
//!   serialisation and `Object.keys` enumeration.
//! - [`HostDataTracer`] / [`HostCodeLivenessTracer`] — safe paired visitors for
//!   host payloads that retain JavaScript values.
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
//! - Runtime transaction rollback may force-remove only the own data slot that
//!   still holds its expected published value; it never invokes an accessor or
//!   removes a replacement installed by re-entrant code.
//! - GC shape bodies are immutable after allocation; transition tables and
//!   offset maps live in interpreter-owned side caches.
//! - Every store of a `Gc<…>`-bearing `Value` into a slot, every
//!   prototype assignment, and every symbol-property write records
//!   the store through [`otter_gc::GcHeap::record_write`] so the
//!   generational and incremental marker observe the new pointer.
//! - Traced host payloads enumerate the same strong slots for moving-GC
//!   tracing and the allocation-free dynamic-code liveness census.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-property-attributes>
//! - <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots>
//! - <https://tc39.es/ecma262/#sec-ordinarydefineownproperty>
//! - <https://tc39.es/ecma262/#sec-ordinaryset>
//! - [GC API](../../../docs/book/src/engine/gc-api.md)
//! - [Event loop](../../../docs/book/src/engine/event-loop.md)

use std::any::Any;
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::bigint::BigIntValue;
use crate::number::NumberValue;
use crate::property_atom::{AtomId, AtomizedPropertyKey};
use crate::proxy::JsProxy;
use crate::string::{JsString, to_utf16_vec};
use crate::symbol::JsSymbol;
use crate::{UpvalueCell, Value, read_upvalue, store_upvalue};
use otter_gc::GcHeap;
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::{RawGc, SlotVisitor};
use smallvec::SmallVec;

mod descriptor;
mod descriptor_core;
mod key_order;
mod lookup;
mod shape_body;
mod shape_cache;
mod shape_runtime;
mod shape_transition;
pub mod slot_slab;

pub use descriptor::{
    DescriptorKind, PartialPropertyDescriptor, PropertyDescriptor, PropertyFlags,
};
pub(crate) use key_order::array_index_property_name;
pub use lookup::{PropertyLookup, SetOutcome, SetRejectReason};
pub(crate) use shape_body::ShapeBody;
pub(crate) use shape_body::ShapeHandle;
pub(crate) use shape_body::shape_offset_of_str;
pub(crate) use shape_cache::{SHAPE_CACHE_MODE_FAST, ShapeCacheInvalidation, ShapeCacheMode};
pub(crate) use shape_runtime::ShapeRuntime;
#[cfg(test)]
pub(crate) use shape_transition::capture_store_property_transition;
pub(crate) use shape_transition::{
    LowerableStoreTransition, StorePropertyTransition, StorePropertyTransitionKind,
    capture_store_property_transition_with_shape, replay_store_property_transition,
};

static NEXT_SHAPE_ID: AtomicU64 = AtomicU64::new(1);

fn next_shape_id() -> ShapeId {
    ShapeId(NEXT_SHAPE_ID.fetch_add(1, Ordering::Relaxed))
}

/// The next shape id this process would mint — captured into a
/// snapshot so a restoring process never re-issues an id the image's
/// shapes (or dictionary objects) already carry. Shape ids key the
/// property caches; a collision hands one object another's slots.
pub(crate) fn snapshot_next_shape_id() -> u64 {
    NEXT_SHAPE_ID.load(Ordering::Relaxed)
}

/// Advance the shape-id counter past every id a restored image
/// carries. `fetch_max` keeps it monotonic when several isolates
/// restore into one process.
pub(crate) fn bump_next_shape_id_to(minimum: u64) {
    NEXT_SHAPE_ID.fetch_max(minimum, Ordering::Relaxed);
}

/// Rust-owned, non-traced data attached to a JavaScript object.
///
/// This convenience marker is for payloads that contain no JavaScript values.
/// Payloads which retain JavaScript references use
/// [`TracedHostObjectData`] and [`HostValueSlot`] instead. The explicit empty
/// implementation is intentional: choosing the untraced path must be visible
/// at the payload type, rather than silently applying to every Rust type.
pub trait HostObjectData: Any {}

/// One collector-rewritten JavaScript reference inside traced host data.
///
/// The representation is deliberately private. A slot can only be populated
/// from, or materialized into, a rooted [`crate::Local`] through
/// [`crate::NativeScope`]. This prevents an extension from retaining a raw VM
/// value across an allocating safepoint.
pub struct HostValueSlot {
    value: Value,
}

impl HostValueSlot {
    /// Create an empty (`undefined`) host slot.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            value: Value::undefined(),
        }
    }

    /// Remove the strong JavaScript reference from this slot.
    pub fn clear(&mut self) {
        self.value = Value::undefined();
    }

    /// Whether the slot currently contains no JavaScript reference.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.value.is_undefined()
    }

    pub(crate) fn value(&self) -> Value {
        self.value
    }

    pub(crate) fn replace(&mut self, value: Value) {
        self.value = value;
    }

    pub(crate) fn from_value(value: Value) -> Self {
        Self { value }
    }
}

impl Default for HostValueSlot {
    fn default() -> Self {
        Self::empty()
    }
}

impl std::fmt::Debug for HostValueSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostValueSlot")
            .field("empty", &self.is_empty())
            .finish_non_exhaustive()
    }
}

/// Safe visitor exposed to [`TracedHostObjectData`] implementations.
///
/// It only accepts [`HostValueSlot`] fields and never exposes the collector's
/// raw slot visitor or heap. Implementations must enumerate each strong slot
/// exactly once and must not allocate or re-enter JavaScript while tracing.
pub struct HostDataTracer<'a> {
    visitor: &'a mut SlotVisitor<'a>,
}

/// Allocation-free function-id visitor paired with [`HostDataTracer`].
pub struct HostCodeLivenessTracer<'a> {
    visitor: &'a mut dyn FnMut(u32),
}

impl HostCodeLivenessTracer<'_> {
    /// Visit one strong host slot for dynamic-code reachability.
    pub fn trace(&mut self, slot: &HostValueSlot) {
        crate::code_liveness::visit_value(&slot.value, self.visitor);
    }
}

impl HostDataTracer<'_> {
    /// Trace one strong host slot.
    pub fn trace(&mut self, slot: &mut HostValueSlot) {
        slot.value.trace_value_slots(self.visitor);
    }
}

/// Explicit tracing contract for Rust host-object payloads that retain
/// JavaScript values.
///
/// Extensions store references only in [`HostValueSlot`] fields and enumerate
/// those fields from [`Self::trace_gc_slots`]. The collector invokes the method
/// while the mutator is stopped, allowing moving collections to rewrite the
/// opaque slots in place without exposing any raw GC API to the extension.
pub trait TracedHostObjectData: Any {
    /// Enumerate every strong JavaScript slot owned by this payload.
    fn trace_gc_slots(&mut self, tracer: &mut HostDataTracer<'_>);

    /// Enumerate the same strong slots without allocating during a code census.
    fn visit_function_ids(&self, tracer: &mut HostCodeLivenessTracer<'_>);
}

trait ErasedTracedHostObjectData {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn trace_gc_slots_erased(&mut self, visitor: &mut SlotVisitor<'_>);
    fn visit_function_ids_erased(&self, visitor: &mut dyn FnMut(u32));
}

impl<T: TracedHostObjectData> ErasedTracedHostObjectData for T {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn trace_gc_slots_erased(&mut self, visitor: &mut SlotVisitor<'_>) {
        let mut tracer = HostDataTracer { visitor };
        self.trace_gc_slots(&mut tracer);
    }

    fn visit_function_ids_erased(&self, visitor: &mut dyn FnMut(u32)) {
        let mut tracer = HostCodeLivenessTracer { visitor };
        self.visit_function_ids(&mut tracer);
    }
}

enum HostData {
    Untraced(Box<dyn Any>),
    Traced(Box<dyn ErasedTracedHostObjectData>),
}

impl HostData {
    fn downcast_ref<T: Any>(&self) -> Option<&T> {
        match self {
            Self::Untraced(data) => data.downcast_ref::<T>(),
            Self::Traced(data) => data.as_any().downcast_ref::<T>(),
        }
    }

    fn downcast_mut<T: Any>(&mut self) -> Option<&mut T> {
        match self {
            Self::Untraced(data) => data.downcast_mut::<T>(),
            Self::Traced(data) => data.as_any_mut().downcast_mut::<T>(),
        }
    }

    fn into_untraced<T: Any>(self) -> Result<Box<T>, Self> {
        match self {
            Self::Untraced(data) => data.downcast::<T>().map_err(Self::Untraced),
            traced => Err(traced),
        }
    }

    fn trace_gc_slots(&mut self, visitor: &mut SlotVisitor<'_>) {
        if let Self::Traced(data) = self {
            data.trace_gc_slots_erased(visitor);
        }
    }

    fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        if let Self::Traced(data) = self {
            data.visit_function_ids_erased(visitor);
        }
    }
}

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
            Self::Object(obj) => Some(Value::object(*obj)),
            Self::Value(value) => Some(*value),
            Self::Proxy(proxy) => Some(Value::proxy(*proxy)),
        }
    }
}

// ---------- internal slot storage -----------------------------------------

/// `[[Get]]`/`[[Set]]` pair for an accessor property. The owned form used
/// by descriptor interchange and symbol-keyed slots; string-keyed accessor
/// slots store the pair as an [`AccessorCellBody`] GC cell in the flat value
/// array instead (see [`alloc_accessor_cell`]).
#[derive(Debug, Clone)]
struct AccessorPair {
    getter: Option<Value>,
    setter: Option<Value>,
}

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`AccessorCellBody`].
///
/// Next free tag after `EVAL_ENV_BODY_TYPE_TAG = 0x2E`; distinct from every
/// other GC body so the `type_tag → trace` table dispatch stays unambiguous.
pub const ACCESSOR_CELL_TYPE_TAG: u8 = 0x2F;

/// GC-allocated `([[Get]], [[Set]])` pair for a string-keyed accessor slot.
///
/// A string-keyed accessor slot stores a handle to this cell in the object's
/// flat value array (the same place a data slot stores its value), so
/// per-object slot metadata ([`SlotMeta`]) carries only a `is_accessor`
/// discriminator and never the getter/setter payload. `undefined` encodes an
/// absent getter/setter — an accessor descriptor cannot carry an `undefined`
/// function, so the sentinel is unambiguous.
#[derive(otter_macros::Pelt)]
#[pelt(tag = ACCESSOR_CELL_TYPE_TAG)]
pub struct AccessorCellBody {
    /// `[[Get]]` — a callable, or `undefined` when absent.
    pub getter: Value,
    /// `[[Set]]` — a callable, or `undefined` when absent.
    pub setter: Value,
}

impl AccessorCellBody {
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        crate::code_liveness::visit_value(&self.getter, visitor);
        crate::code_liveness::visit_value(&self.setter, visitor);
    }
}

/// Allocate an [`AccessorCellBody`] for an accessor pair and return a
/// pointer-tagged [`Value`] referencing it, suitable for the flat value
/// array. `obj` is rooted across the allocation: the cell alloc is a GC
/// safepoint that can relocate young objects, so the receiver handle is
/// yielded as a rewriteable root and read back relocated.
pub(crate) fn alloc_accessor_cell(
    heap: &mut GcHeap,
    obj: &mut JsObject,
    getter: Option<Value>,
    setter: Option<Value>,
) -> Result<Value, otter_gc::OutOfMemory> {
    let body = AccessorCellBody {
        getter: getter.unwrap_or(Value::undefined()),
        setter: setter.unwrap_or(Value::undefined()),
    };
    let mut roots = |visit: &mut dyn FnMut(*mut RawGc)| {
        visit(obj as *mut JsObject as *mut RawGc);
    };
    let cell = heap.alloc_with_roots(body, &mut roots)?;
    Ok(Value::from_object_gc(cell.raw()))
}

/// Read the `(getter, setter)` pair from an accessor slot's flat value,
/// mapping the `undefined` sentinel back to `None`.
fn read_accessor_cell(heap: &GcHeap, cell_value: Value) -> (Option<Value>, Option<Value>) {
    let Some(cell) = cell_value
        .as_raw_gc()
        .and_then(|raw| raw.checked_cast::<AccessorCellBody>())
    else {
        return (None, None);
    };
    heap.read_payload(cell, |body| {
        let getter = (!body.getter.is_undefined()).then_some(body.getter);
        let setter = (!body.setter.is_undefined()).then_some(body.setter);
        (getter, setter)
    })
}

/// Property kind discriminant. The data **value** is stored out-of-line —
/// in the object's flat value array for string-keyed slots, or in
/// [`SlotData::value`] for symbol slots and descriptor interchange — so the
/// JIT can read a monomorphic data property by fixed byte offset.
#[derive(Debug, Clone)]
enum SlotKind {
    /// Data property; the value lives in the flat value array.
    Data,
    /// Accessor property; getter/setter boxed (cold path).
    Accessor(AccessorPair),
}

impl SlotKind {
    fn accessor(getter: Option<Value>, setter: Option<Value>) -> Self {
        SlotKind::Accessor(AccessorPair { getter, setter })
    }

    fn is_data(&self) -> bool {
        matches!(self, SlotKind::Data)
    }
}

/// Per-slot metadata for a string-keyed own property. The matching value
/// lives at the same index in the object's flat value array
/// ([`ObjectBody::data_value`]): a data property's `[[Value]]`, or a handle
/// to the slot's [`AccessorCellBody`] when `is_accessor` is set.
#[derive(Debug, Clone, Copy)]
struct SlotMeta {
    flags: PropertyFlags,
    /// `true` when the flat value at this index is an [`AccessorCellBody`]
    /// handle rather than a data value. The hidden class records the same
    /// discriminator (`own_is_accessor`); this per-slot copy is the
    /// authoritative source for attribute-overridden and dictionary-mode
    /// objects whose slots have diverged from the shape.
    is_accessor: bool,
}

impl SlotMeta {
    /// Metadata for a default-attributes data slot
    /// (`writable / enumerable / configurable` all `true`).
    fn data_default() -> Self {
        Self {
            flags: PropertyFlags::data_default(),
            is_accessor: false,
        }
    }
}

/// Owned `(flags, kind, value)` triple. Used as the storage form for
/// symbol-keyed own properties (never JIT-hot, so they keep the value
/// inline) and as the interchange form for descriptor validation and
/// merges. `value` is meaningful only when `kind` is [`SlotKind::Data`].
#[derive(Debug, Clone)]
struct SlotData {
    flags: PropertyFlags,
    kind: SlotKind,
    value: Value,
}

impl SlotData {
    fn data_default(value: Value) -> Self {
        Self {
            flags: PropertyFlags::data_default(),
            kind: SlotKind::Data,
            value,
        }
    }

    fn from_descriptor(desc: PropertyDescriptor) -> Self {
        match desc.kind {
            DescriptorKind::Data { value } => Self {
                flags: desc.flags,
                kind: SlotKind::Data,
                value,
            },
            DescriptorKind::Accessor { getter, setter } => Self {
                flags: desc.flags,
                kind: SlotKind::accessor(getter, setter),
                value: Value::undefined(),
            },
        }
    }

    /// Lower into index-aligned `(metadata, flat value)` for storage in the
    /// object's `slots` + value array. A data slot stores its `[[Value]]`
    /// directly; an accessor slot allocates an [`AccessorCellBody`] and
    /// stores the cell handle, rooting `obj` across the allocation.
    fn into_flat(
        self,
        heap: &mut GcHeap,
        obj: &mut JsObject,
    ) -> Result<(SlotMeta, Value), otter_gc::OutOfMemory> {
        match self.kind {
            SlotKind::Data => Ok((
                SlotMeta {
                    flags: self.flags,
                    is_accessor: false,
                },
                self.value,
            )),
            SlotKind::Accessor(pair) => {
                let cell = alloc_accessor_cell(heap, obj, pair.getter, pair.setter)?;
                Ok((
                    SlotMeta {
                        flags: self.flags,
                        is_accessor: true,
                    },
                    cell,
                ))
            }
        }
    }

    fn to_descriptor(&self) -> PropertyDescriptor {
        slot_descriptor(self.flags, &self.kind, self.value)
    }

    fn to_lookup(&self) -> PropertyLookup {
        slot_lookup(self.flags, &self.kind, self.value)
    }
}

/// Build a [`PropertyDescriptor`] from split slot parts (`value` ignored
/// for accessors).
fn slot_descriptor(flags: PropertyFlags, kind: &SlotKind, value: Value) -> PropertyDescriptor {
    PropertyDescriptor {
        flags,
        kind: match kind {
            SlotKind::Data => DescriptorKind::Data { value },
            SlotKind::Accessor(pair) => DescriptorKind::Accessor {
                getter: pair.getter,
                setter: pair.setter,
            },
        },
    }
}

/// Build a [`PropertyLookup`] from split slot parts (`value` ignored for
/// accessors).
fn slot_lookup(flags: PropertyFlags, kind: &SlotKind, value: Value) -> PropertyLookup {
    match kind {
        SlotKind::Data => PropertyLookup::Data { value, flags },
        SlotKind::Accessor(pair) => PropertyLookup::Accessor {
            getter: pair.getter,
            setter: pair.setter,
            flags,
        },
    }
}

// ---------- shape (hidden class) ------------------------------------------

/// VM-local hidden-class identity for interpreter inline-cache guards.
///
/// Shape ids are internal metadata only. They are not serialized and have no
/// JavaScript-observable meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ShapeId(u64);

impl ShapeId {
    /// Placeholder for fast-shaped objects that have never needed dictionary
    /// identity. Shape-backed objects read identity from the installed GC shape;
    /// dictionary-mode transitions overwrite this with [`next_shape_id`].
    pub(crate) const UNASSIGNED: Self = Self(0);

    /// Raw VM-local id. Exposed to the [`crate::inspect`] snapshot
    /// surface so embedder DTOs can carry a stable identity without
    /// publishing the wrapper type itself.
    #[must_use]
    pub(crate) const fn raw(self) -> u64 {
        self.0
    }

    /// Rebuild a stable VM-local identity read from an atomic feedback slot.
    #[must_use]
    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    #[cfg(test)]
    pub(crate) const fn for_test(raw: u64) -> Self {
        Self::from_raw(raw)
    }
}

/// Atom-aware own-property hit metadata.
///
/// This keeps the first inline-cache slice small: named property opcodes can
/// learn the receiver shape, property atom, and slot offset without changing
/// object storage or descriptor semantics yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AtomOwnPropertyHit {
    /// Shape observed on the receiver object.
    pub(crate) shape_id: ShapeId,
    /// GC handle of the observed shape. Carried so the JIT can bake the
    /// shape's (stable) compressed offset into a monomorphic property guard.
    /// Not traced: shapes are immortal (rooted forever by the transition
    /// tables) and pinned in non-moving old space, so the handle never
    /// dangles or relocates. `Gc::null()` in dictionary mode.
    pub(crate) shape: ShapeHandle,
    /// Atomized named-property key from the executable context.
    pub(crate) atom_id: AtomId,
    /// String-keyed own-property slot offset.
    pub(crate) slot: u16,
    /// `true` when the slot held a plain data property at install. Under a
    /// matching shape with attributes not overridden in place, the slot kind
    /// is fixed, so a data hit reads the value without consulting the shape's
    /// per-slot attributes.
    pub(crate) is_data: bool,
}

impl AtomOwnPropertyHit {
    /// Filler for an empty cache way. Its shape id matches no object, so it
    /// can only ever be read after the owning entry's own key compare fails.
    pub(crate) const PLACEHOLDER: Self = Self {
        shape_id: ShapeId::UNASSIGNED,
        shape: ShapeHandle::null(),
        atom_id: AtomId::NONE,
        slot: 0,
        is_data: false,
    };
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
/// flag. All of those fields live here directly. Mutation flows through
/// [`otter_gc::GcHeap::with_payload`] (writers) and reads through
/// [`otter_gc::GcHeap::read_payload`] (readers). Every store of a
/// `Gc<…>`-bearing field is recorded through
/// [`otter_gc::GcHeap::record_write`].
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots>
/// - <https://tc39.es/ecma262/#sec-ordinarypreventextensions>
#[repr(C)]
pub struct ObjectBody {
    /// GC-managed hidden class for fast ordinary objects. First field so
    /// the JIT can read the shape token at a fixed byte offset
    /// ([`OBJECT_BODY_SHAPE_OFFSET`]) for monomorphic guard checks.
    shape: ShapeHandle,
    /// Cached base pointer for the contiguous string-keyed value slab. The JIT
    /// reads this field after the shape guard and indexes it by slot byte
    /// offset, so every own data slot has the same inline path regardless of
    /// slot number. `null` means the object currently has no string-keyed
    /// slots.
    ///
    /// Frozen ABI: string-keyed slot `i` lives at **forward** index `i`
    /// (header-relative, single growth direction — no split low/high regions),
    /// reached uniformly through this base whether inline or spilled. The
    /// base is an **always-current** invariant — refreshed after every move,
    /// grow, shrink, or spill ([`Self::refresh_values_ptr`]) and verified at
    /// every slab access in debug ([`Self::values_ptr_is_current`]).
    values_ptr: Cell<*mut Value>,
    /// Out-of-line string-keyed own-property values once the object grows
    /// past [`INLINE_SLOT_CAP`], indexed by shape slot offset. A data slot
    /// stores its `[[Value]]` directly; an accessor slot stores a handle to
    /// its [`AccessorCellBody`]. Slot flags and data/accessor kind live in
    /// the shape for ordinary shaped objects, or in materialized metadata
    /// for dictionary/attribute-overridden objects.
    ///
    /// Null while the slab is inline. The slab is a GC body carrying its
    /// words in the same cell ([`slot_slab`]), not a `Vec`: an object that
    /// owned malloc storage could not be captured into a page image, and a
    /// restored copy would alias the original buffer.
    slab: slot_slab::SlotSlabHandle,
    /// Fallback/dictionary identity used only when [`Self::shape`] is null.
    /// Fast shaped objects keep this as [`ShapeId::UNASSIGNED`] so allocation
    /// does not need per-object unique metadata; conversion to dictionary mode
    /// assigns a fresh id before clearing the shape.
    dictionary_shape_id: ShapeId,
    /// Whether string-keyed shape assumptions are IC-compatible.
    ///
    /// Ordinary shape transitions stay in [`ShapeCacheMode::Fast`].
    /// Deleting string-keyed own properties marks the object
    /// [`ShapeCacheMode::DictionaryCompatible`] so future dictionary storage
    /// can keep the same invalidation contract without installing stale ICs.
    shape_cache_mode: ShapeCacheMode,
    /// `[[Prototype]]` — the single source of truth for the common Null /
    /// ordinary-object case: a bare [`JsObject`] handle, or
    /// [`otter_gc::Gc::null()`] for a `null` prototype. A non-ordinary
    /// prototype (`Value` / `Proxy`) sets this to null and stores the real
    /// prototype in [`ExoticSlots::proto_override`]; [`ObjectBody::prototype`]
    /// reconstructs the full [`ObjectPrototype`]. The fixed offset
    /// ([`OBJECT_BODY_JIT_PROTO_OFFSET`]) lets the method-inline guard read the
    /// handle from machine code and chase the prototype's shape without a
    /// per-call resolve bridge. Sole writer is [`set_prototype_value`]; traced
    /// as a distinct GC slot.
    jit_proto: JsObject,
    /// `[[Extensible]]` internal slot. New keys are rejected when
    /// this is `false`.
    extensible: bool,
    /// `true` once an in-place attribute mutation (defineProperty on an
    /// existing slot, `seal`, `freeze`) has changed a shaped slot's
    /// flags/kind without transitioning the hidden class. While `false`, a
    /// shaped object's per-slot attributes are guaranteed to match the shape
    /// (every shaped slot reached the object via an attribute-recording
    /// transition), so attribute reads short-circuit to the shape. Once
    /// `true`, `slots` is the only authoritative attribute source and reads
    /// fall back to it. Always `false` for dictionary-mode objects (their
    /// shape is null and reads use `slots` regardless). Lives in the byte of
    /// padding beside [`Self::extensible`], so it adds no object size.
    ///
    /// When `true` (or in dictionary mode) the per-slot metadata is
    /// *materialized* in [`ExoticSlots::slots`]; the common shaped object
    /// carries no per-slot metadata at all and derives everything from the
    /// hidden class.
    slot_attrs_overridden: bool,
    /// Lazily-allocated rare/exotic slots — symbol-keyed properties, host
    /// data, native `[[Call]]`/`[[Construct]]`, primitive-wrapper internal
    /// slots, and the Date/Error/raw-JSON/arguments markers. `None` for plain
    /// objects and class instances (the overwhelming common case), so an
    /// ordinary object never pays for these ~140 bytes. Allocated on first
    /// write through [`ObjectBody::exotic_mut`].
    exotic: ExoticSlot,
    /// In-body storage for the first [`INLINE_SLOT_CAP`] string-keyed slots, so
    /// a small object needs no separate slab allocation and keeps its hot slots
    /// in the same cache line as the shape and `values_ptr` — the
    /// allocation-locality reason to keep a small inline region at all.
    /// Active while `slab_len <= INLINE_SLOT_CAP`; growth past the cap migrates
    /// every slot wholesale into `values` and leaves this array unused.
    /// `values_ptr` always points at whichever buffer is active, so the slot
    /// access path and the JIT both index it uniformly.
    inline_values: [Value; INLINE_SLOT_CAP],
    /// Count of live string-keyed slots, across `inline_values` or `values`.
    slab_len: u16,
}

impl ObjectBody {
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        if self.slab.is_null() {
            for value in &self.inline_values[..self.slab_len as usize] {
                crate::code_liveness::visit_value(value, visitor);
            }
        }
    }
}

/// In-body inline string-keyed slot capacity. Objects with this many own data
/// properties or fewer carry their slab in [`ObjectBody::inline_values`]; larger
/// objects spill the whole slab to the out-of-line `values` vector.
///
/// Three direct `Value` words preserve the previous 88-byte hot-object
/// footprint while covering the common small record. A fourth own property
/// spills to the GC-managed slab; increasing this cap would charge every empty
/// object another eight bytes per slot.
pub(crate) const INLINE_SLOT_CAP: usize = 3;

/// Rarely-used `ObjectBody` slots, boxed out of the hot object so plain
/// objects stay small. Every field here is absent on a plain `{}` / class
/// instance; presence implies a wrapper object, host object, callable/
/// constructor builtin, Date, Error, raw-JSON, or arguments exotic.
/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`ExoticSlots`].
pub const EXOTIC_SLOTS_TYPE_TAG: u8 = 0x37;

/// Handle to an object's rare/exotic sidecar.
pub type ExoticHandle = otter_gc::Gc<ExoticSlots>;

/// The sidecar handle as [`ObjectBody`] stores it.
///
/// Eight bytes aligned to eight, exactly what the `Box` it replaced
/// occupied, so every offset the frozen JIT ABI pins below stays put and
/// no backend has to re-bake `INLINE_VALUES_BYTE`. The handle itself is
/// four bytes; the rest is deliberate slack.
#[repr(C, align(8))]
#[derive(Clone, Copy, Default)]
pub struct ExoticSlot {
    handle: ExoticHandle,
    _pad: u32,
}

impl ExoticSlot {
    /// An object with no sidecar.
    #[must_use]
    pub fn null() -> Self {
        Self::default()
    }

    /// The sidecar handle.
    #[must_use]
    pub fn get(self) -> ExoticHandle {
        self.handle
    }

    /// `true` when the object has no sidecar.
    #[must_use]
    pub fn is_null(self) -> bool {
        self.handle.is_null()
    }

    /// Install a sidecar.
    pub fn set(&mut self, handle: ExoticHandle) {
        self.handle = handle;
    }

    /// Address of the handle, for the tracer.
    fn slot_ptr(&mut self) -> *mut RawGc {
        &mut self.handle as *mut ExoticHandle as *mut RawGc
    }
}

/// Rare/exotic object state, kept out of [`ObjectBody`] so ordinary
/// objects stay small. Its own GC body, so an object that has one still
/// owns nothing outside the heap.
#[derive(Default)]
pub struct ExoticSlots {
    /// Non-ordinary `[[Prototype]]` (a `Value` or `Proxy`). `None` for the
    /// common Null / ordinary-object prototype, which is encoded entirely by
    /// `ObjectBody::jit_proto` (null handle == `null` prototype).
    proto_override: Option<ObjectPrototype>,
    /// Insertion-ordered dictionary keys with their content-hash index,
    /// in a [`DictKeysBody`] of their own — null until the object leaves
    /// fast-shape mode.
    dictionary_keys: DictKeysHandle,
    /// Materialized per-slot metadata (flags + `is_accessor` discriminator),
    /// index-aligned with the flat value array. Present and authoritative only
    /// for dictionary-mode objects (null shape) and attribute-overridden
    /// objects (`ObjectBody::slot_attrs_overridden`). Null for the common
    /// shaped object, which derives per-slot attributes from the hidden
    /// class. Holds no GC handles; the handle is traced so the table
    /// relocates with the graph.
    slots: SlotMetaHandle,
    /// Symbol-keyed own properties, in a [`SymbolPropsBody`] of their
    /// own — null until the first symbol property is defined.
    symbol_props: SymbolPropsHandle,
    /// Rust-owned payload for host-backed objects and VM-internal side data.
    host_data: Option<HostData>,
    /// Native `[[Call]]` for builtin callable ordinary objects.
    call_native: Option<Value>,
    /// Native `[[Construct]]` for constructor-shaped builtins (`Number`, …).
    constructor_native: Option<Value>,
    /// `[[BooleanData]]` for Boolean wrapper objects.
    boolean_data: Option<bool>,
    /// `[[NumberData]]` for Number wrapper objects.
    number_data: Option<NumberValue>,
    /// `[[StringData]]` for String wrapper objects.
    string_data: Option<JsString>,
    /// `[[SymbolData]]` for Symbol wrapper objects.
    symbol_data: Option<crate::symbol::JsSymbol>,
    /// `[[BigIntData]]` for BigInt wrapper objects.
    bigint_data: Option<BigIntValue>,
    /// `[[IsRawJSON]]` marker (`JSON.rawJSON`, ECMA-262 §25.5.3).
    is_raw_json: bool,
    /// `[[DateValue]]` for Date instances (UTC epoch ms, or NaN). §21.4.5.
    date_data: Option<f64>,
    /// `[[ErrorData]]` presence marker (§20.5).
    error_data: bool,
    /// Captured JS call-stack frames (top-of-stack first) recorded at
    /// the moment this error object was constructed, or installed by
    /// `Error.captureStackTrace`. Drives `Error.prototype.stack` and
    /// `util.getCallSites`. `None` until captured; holds only owned
    /// `String`/offset data (no GC handles), so it needs no tracing.
    error_stack_frames: ErrorStackHandle,
    /// `[[ParameterMap]]` presence marker for arguments-exotic objects
    /// (§10.4.4); mapping data itself lives in `host_data`.
    is_arguments_object: bool,
}

impl otter_gc::trace::SeverRestoredPayload for ExoticSlots {
    /// Sever foreign ownership on a snapshot restore: the copied host payload
    /// box aliases the source isolate's allocation and would become a second
    /// owner, so even this body's own trace impl must never see it. Capturable
    /// graphs carry no live host payloads.
    fn sever_restored_payload(&mut self) {
        // SAFETY: overwriting without dropping (or reading) severs the
        // alias; the capture isolate remains the owner.
        unsafe { std::ptr::write(&mut self.host_data, None) };
    }
}

impl otter_gc::SafeTraceable for ExoticSlots {
    const TYPE_TAG: u8 = EXOTIC_SLOTS_TYPE_TAG;

    fn trace_slots_safe(&mut self, v: &mut SlotVisitor<'_>) {
        match &mut self.proto_override {
            None | Some(ObjectPrototype::Null) | Some(ObjectPrototype::Object(_)) => {}
            Some(ObjectPrototype::Value(value)) => value.trace_value_slot_mut(v),
            Some(ObjectPrototype::Proxy(proxy)) => proxy.trace_value_slots_mut(v),
        }
        if !self.symbol_props.is_null() {
            let slot = &mut self.symbol_props as *mut SymbolPropsHandle as *mut RawGc;
            v(slot);
        }
        // The metadata records hold no GC references, but the table CELL
        // is a heap object this sidecar keeps alive — an untraced handle
        // here is a table the next full collection frees out from under
        // the object.
        if !self.slots.is_null() {
            let slot = &mut self.slots as *mut SlotMetaHandle as *mut RawGc;
            v(slot);
        }
        if !self.dictionary_keys.is_null() {
            let slot = &mut self.dictionary_keys as *mut DictKeysHandle as *mut RawGc;
            v(slot);
        }
        if !self.error_stack_frames.is_null() {
            let slot = &mut self.error_stack_frames as *mut ErrorStackHandle as *mut RawGc;
            v(slot);
        }
        if let Some(native) = &mut self.call_native {
            native.trace_value_slot_mut(v);
        }
        if let Some(native) = &mut self.constructor_native {
            native.trace_value_slot_mut(v);
        }
        if let Some(data) = self
            .host_data
            .as_mut()
            .and_then(|data| data.downcast_mut::<MappedArgumentsData>())
        {
            for entry in data.entries.iter_mut() {
                let p = &mut entry.cell as *mut UpvalueCell as *mut RawGc;
                v(p);
            }
        }
        if let Some(data) = self.host_data.as_mut() {
            data.trace_gc_slots(v);
        }
    }
}

impl ExoticSlots {
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        if let Some(ObjectPrototype::Value(value)) = &self.proto_override {
            crate::code_liveness::visit_value(value, visitor);
        }
        if let Some(value) = &self.call_native {
            crate::code_liveness::visit_value(value, visitor);
        }
        if let Some(value) = &self.constructor_native {
            crate::code_liveness::visit_value(value, visitor);
        }
        if let Some(data) = &self.host_data {
            data.visit_function_ids(visitor);
        }
    }
}

/// The sidecar payload behind `handle`, or `None` for a null handle.
///
/// The one place an exotic handle is decoded without going through the
/// heap — an object body holding a payload borrow has no heap to ask.
#[must_use]
fn exotic_body_of(handle: ExoticHandle) -> Option<*mut ExoticSlots> {
    if handle.is_null() {
        return None;
    }
    let header = handle.as_header_ptr();
    // SAFETY: a non-null handle names a live cell whose payload is an
    // `ExoticSlots` one header past the start.
    Some(unsafe {
        header
            .cast::<u8>()
            .add(std::mem::size_of::<otter_gc::GcHeader>())
            .cast::<ExoticSlots>()
    })
}

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`DictKeysBody`].
pub const DICT_KEYS_BODY_TYPE_TAG: u8 = 0x3d;

/// Handle to a dictionary-mode object's key table.
pub(crate) type DictKeysHandle = otter_gc::Gc<DictKeysBody>;

/// One dictionary key record: where its UTF-8 bytes sit in the table's
/// byte arena, and the next entry in its hash chain.
#[repr(C)]
#[derive(Clone, Copy)]
struct DictKeyEntry {
    byte_offset: u32,
    byte_len: u32,
    /// Next entry index in the same bucket, or `u32::MAX`.
    next: u32,
}

/// Header for a dictionary-mode object's insertion-ordered key table.
///
/// The trailing storage holds, in order: the bucket array (one `u32`
/// head per bucket), the entry records, and a byte arena the keys'
/// UTF-8 lives in. Everything the old `Vec<String>` +
/// `FxHashMap<String, u16>` pair provided, in one cell the page image
/// carries whole. Keys hash by content, which no collection changes,
/// so the chains never go stale — unlike identity-keyed tables.
///
/// Slot offsets are entry indices: the table is append-only between
/// wholesale rebuilds (delete compaction replaces the table), exactly
/// like the `Vec` it replaces.
#[repr(C, align(8))]
pub struct DictKeysBody {
    /// Entries the table can hold.
    capacity: u32,
    /// Entries written.
    len: u32,
    /// `bucket_count - 1`; bucket count is a power of two.
    bucket_mask: u32,
    /// Bytes of key arena capacity.
    byte_capacity: u32,
    /// Bytes of key arena used.
    byte_len: u32,
    _pad: u32,
}

/// End of a dictionary hash chain.
const DICT_CHAIN_END: u32 = u32::MAX;

/// Content hash for dictionary keys — stable across collections.
fn dict_key_hash(key: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    key.as_bytes().hash(&mut hasher);
    hasher.finish()
}

impl DictKeysBody {
    fn bucket_count_for(capacity: usize) -> usize {
        capacity.max(1).next_power_of_two()
    }

    /// Trailing bytes for `capacity` entries and `byte_capacity` bytes
    /// of key arena.
    fn trailing_bytes(capacity: usize, byte_capacity: usize) -> usize {
        Self::bucket_count_for(capacity) * std::mem::size_of::<u32>()
            + capacity * std::mem::size_of::<DictKeyEntry>()
            + byte_capacity
    }

    fn new(capacity: usize, byte_capacity: usize) -> Self {
        Self {
            capacity: u32::try_from(capacity).expect("dict key capacity exceeds u32"),
            len: 0,
            bucket_mask: u32::try_from(Self::bucket_count_for(capacity) - 1)
                .expect("bucket count exceeds u32"),
            byte_capacity: u32::try_from(byte_capacity).expect("dict key bytes exceed u32"),
            byte_len: 0,
            _pad: 0,
        }
    }

    fn capacity(&self) -> usize {
        self.capacity as usize
    }

    fn len(&self) -> usize {
        self.len as usize
    }

    fn bucket_count(&self) -> usize {
        self.bucket_mask as usize + 1
    }

    fn buckets_ptr(&self) -> *mut u32 {
        // SAFETY: the allocation reserved the bucket array immediately
        // after this header.
        unsafe {
            (self as *const Self as *mut u8)
                .add(std::mem::size_of::<Self>())
                .cast()
        }
    }

    fn entries_ptr(&self) -> *mut DictKeyEntry {
        // SAFETY: the entry records follow the bucket array; both start
        // 4-aligned inside an 8-aligned cell.
        unsafe {
            self.buckets_ptr()
                .add(self.bucket_count())
                .cast::<DictKeyEntry>()
        }
    }

    fn bytes_ptr(&self) -> *mut u8 {
        // SAFETY: the key arena follows the entry records.
        unsafe { self.entries_ptr().add(self.capacity()).cast::<u8>() }
    }

    /// Point every bucket at nothing. Trailing storage is not zeroed.
    fn init_buckets(&mut self) {
        for i in 0..self.bucket_count() {
            // SAFETY: `i < bucket_count`.
            unsafe { *self.buckets_ptr().add(i) = DICT_CHAIN_END };
        }
    }

    /// The key at entry `index`.
    fn key_at(&self, index: usize) -> &str {
        debug_assert!(index < self.len());
        // SAFETY: the entry was written before the table became
        // reachable, and its bytes are valid UTF-8 copied from a `&str`.
        unsafe {
            let entry = *self.entries_ptr().add(index);
            let bytes = std::slice::from_raw_parts(
                self.bytes_ptr()
                    .add(entry.byte_offset as usize)
                    .cast_const(),
                entry.byte_len as usize,
            );
            std::str::from_utf8_unchecked(bytes)
        }
    }

    /// Entry index of `key`, or `None`.
    fn find(&self, key: &str) -> Option<u16> {
        let bucket = (dict_key_hash(key) as u32 & self.bucket_mask) as usize;
        // SAFETY: `bucket < bucket_count`; buckets were initialised.
        let mut current = unsafe { *self.buckets_ptr().add(bucket) };
        while current != DICT_CHAIN_END {
            let index = current as usize;
            if self.key_at(index) == key {
                return Some(index as u16);
            }
            // SAFETY: chain indices always name written entries.
            current = unsafe { (*self.entries_ptr().add(index)).next };
        }
        None
    }

    /// Append `key`. The caller must have reserved entry and byte room:
    /// growth allocates, and a payload borrow has no heap.
    fn push(&mut self, key: &str) {
        let index = self.len();
        debug_assert!(
            index < self.capacity(),
            "dict key push without a reservation"
        );
        debug_assert!(
            self.byte_len as usize + key.len() <= self.byte_capacity as usize,
            "dict key bytes push without a reservation"
        );
        let byte_offset = self.byte_len;
        // SAFETY: the reservation above covers `key.len()` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                key.as_ptr(),
                self.bytes_ptr().add(byte_offset as usize),
                key.len(),
            );
        }
        self.byte_len += key.len() as u32;
        let bucket = (dict_key_hash(key) as u32 & self.bucket_mask) as usize;
        // SAFETY: `bucket < bucket_count`; `index < capacity`.
        unsafe {
            let head = *self.buckets_ptr().add(bucket);
            self.entries_ptr().add(index).write(DictKeyEntry {
                byte_offset,
                byte_len: key.len() as u32,
                next: head,
            });
            *self.buckets_ptr().add(bucket) = index as u32;
        }
        self.len += 1;
    }

    /// Drop every entry and byte.
    fn clear(&mut self) {
        self.len = 0;
        self.byte_len = 0;
        self.init_buckets();
    }
}

const _: () =
    assert!(std::mem::size_of::<DictKeysBody>().is_multiple_of(std::mem::align_of::<u32>()));

impl otter_gc::SafeTraceable for DictKeysBody {
    const TYPE_TAG: u8 = DICT_KEYS_BODY_TYPE_TAG;

    /// Deliberately empty: the table holds only key bytes and indices.
    fn trace_slots_safe(&mut self, _v: &mut SlotVisitor<'_>) {}
}

/// The table payload behind `handle`, or `None` for a null handle.
#[must_use]
fn dict_keys_body_of(handle: DictKeysHandle) -> Option<*mut DictKeysBody> {
    if handle.is_null() {
        return None;
    }
    let header = handle.as_header_ptr();
    // SAFETY: a non-null handle names a live cell whose payload is a
    // `DictKeysBody` one header past the start.
    Some(unsafe {
        header
            .cast::<u8>()
            .add(std::mem::size_of::<otter_gc::GcHeader>())
            .cast::<DictKeysBody>()
    })
}

/// Allocate a key table holding `keys` (in order) with headroom for
/// `extra_entries` more entries and `extra_bytes` more key bytes.
fn dict_keys_table_from(
    heap: &mut otter_gc::GcHeap,
    keys: &[String],
    extra_entries: usize,
    extra_bytes: usize,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<DictKeysHandle, otter_gc::OutOfMemory> {
    let capacity = (keys.len() + extra_entries).max(4);
    let byte_capacity = (keys.iter().map(String::len).sum::<usize>() + extra_bytes)
        .next_multiple_of(8)
        .max(32);
    let table: DictKeysHandle = heap.alloc_variable_with_roots(
        DictKeysBody::new(capacity, byte_capacity),
        DictKeysBody::trailing_bytes(capacity, byte_capacity),
        external_visit,
    )?;
    // SAFETY: the handle names the table just allocated.
    let body = dict_keys_body_of(table).expect("fresh table");
    unsafe {
        (*body).init_buckets();
        for key in keys {
            (*body).push(key);
        }
    }
    Ok(table)
}

/// Pre-build the key table a mutation is about to need.
///
/// `keys: Some(existing)` — the caller is demoting a shaped object and
/// will install a table holding `existing` plus the key it is about to
/// push. `keys: None` — the object is already dictionary-mode; make
/// sure its table has room for one more `pending_key`, growing by copy
/// when it does not. Either way, `Some(table)` out means the payload
/// borrow must install it before pushing.
fn dict_keys_table_for_install(
    object: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    keys: &Option<Vec<String>>,
    pending_key: &str,
    pending: &mut [Value],
) -> Result<Option<DictKeysHandle>, otter_gc::OutOfMemory> {
    let object_slot = (object as *mut JsObject).cast::<otter_gc::raw::RawGc>();
    let pending_base = pending.as_mut_ptr();
    let pending_len = pending.len();
    let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        visitor(object_slot);
        for index in 0..pending_len {
            // SAFETY: `index < pending_len`, and the slice outlives this
            // call; `Value` rewrites its embedded moving offset in place.
            unsafe { (*pending_base.add(index)).trace_value_slot_mut(visitor) };
        }
    };
    if let Some(keys) = keys {
        let table = dict_keys_table_from(heap, keys, 1, pending_key.len(), &mut visit)?;
        return Ok(Some(table));
    }
    // Already dictionary-mode: grow the existing table when the pending
    // key does not fit.
    let current = heap.read_payload(*object, |body| {
        body.exotic().map(|e| e.dictionary_keys).unwrap_or_default()
    });
    let (fits, existing) = match dict_keys_body_of(current) {
        // SAFETY: a non-null handle names a live table.
        Some(table) => unsafe {
            let fits = (*table).len() < (*table).capacity()
                && (*table).byte_len as usize + pending_key.len()
                    <= (*table).byte_capacity as usize;
            if fits {
                (true, Vec::new())
            } else {
                let keys: Vec<String> = (0..(*table).len())
                    .map(|i| (*table).key_at(i).to_owned())
                    .collect();
                (false, keys)
            }
        },
        None => (false, Vec::new()),
    };
    if fits {
        return Ok(None);
    }
    let extra_entries = existing.len().max(4);
    let extra_bytes = (existing.iter().map(String::len).sum::<usize>() + pending_key.len()).max(32);
    let table = dict_keys_table_from(heap, &existing, extra_entries, extra_bytes, &mut visit)?;
    Ok(Some(table))
}

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`ErrorStackBody`].
pub const ERROR_STACK_BODY_TYPE_TAG: u8 = 0x3e;

/// Handle to an error object's captured stack frames.
pub(crate) type ErrorStackHandle = otter_gc::Gc<ErrorStackBody>;

/// One captured frame record; the name/module bytes live in the body's
/// arena.
#[repr(C)]
#[derive(Clone, Copy)]
struct ErrorFrameRecord {
    function_id: u32,
    name_offset: u32,
    name_len: u32,
    module_offset: u32,
    module_len: u32,
    span_lo: u32,
    span_hi: u32,
}

/// Captured `Error` stack frames: fixed records followed by a UTF-8
/// arena holding every frame's function and module name. Written once
/// at capture, never mutated, no GC references — the page image
/// carries it whole where the old `Vec<StackFrameSnapshot>` (owned
/// `String`s) could not ride at all.
#[repr(C, align(8))]
pub struct ErrorStackBody {
    frame_count: u32,
    byte_len: u32,
}

impl ErrorStackBody {
    fn trailing_bytes(frames: usize, bytes: usize) -> usize {
        frames * std::mem::size_of::<ErrorFrameRecord>() + bytes
    }

    fn records_ptr(&self) -> *mut ErrorFrameRecord {
        // SAFETY: the records follow this header.
        unsafe {
            (self as *const Self as *mut u8)
                .add(std::mem::size_of::<Self>())
                .cast()
        }
    }

    fn bytes_ptr(&self) -> *mut u8 {
        // SAFETY: the arena follows the records.
        unsafe {
            self.records_ptr()
                .add(self.frame_count as usize)
                .cast::<u8>()
        }
    }

    fn str_at(&self, offset: u32, len: u32) -> &str {
        // SAFETY: written from `&str` at capture; inside `byte_len`.
        unsafe {
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                self.bytes_ptr().add(offset as usize).cast_const(),
                len as usize,
            ))
        }
    }

    /// Reconstruct the captured frames.
    fn to_frames(&self) -> Vec<crate::run_control::StackFrameSnapshot> {
        (0..self.frame_count as usize)
            .map(|i| {
                // SAFETY: `i < frame_count`; records were fully written
                // before the body became reachable.
                let record = unsafe { *self.records_ptr().add(i) };
                crate::run_control::StackFrameSnapshot {
                    function_id: record.function_id,
                    function_name: self.str_at(record.name_offset, record.name_len).to_owned(),
                    module: self
                        .str_at(record.module_offset, record.module_len)
                        .to_owned(),
                    span: (record.span_lo, record.span_hi),
                }
            })
            .collect()
    }
}

impl otter_gc::SafeTraceable for ErrorStackBody {
    const TYPE_TAG: u8 = ERROR_STACK_BODY_TYPE_TAG;

    /// Deliberately empty: records and name bytes hold no GC references.
    fn trace_slots_safe(&mut self, _v: &mut SlotVisitor<'_>) {}
}

/// The payload behind `handle`, or `None` for a null handle.
#[must_use]
fn error_stack_body_of(handle: ErrorStackHandle) -> Option<*mut ErrorStackBody> {
    if handle.is_null() {
        return None;
    }
    let header = handle.as_header_ptr();
    // SAFETY: a non-null handle names a live cell whose payload is an
    // `ErrorStackBody` one header past the start.
    Some(unsafe {
        header
            .cast::<u8>()
            .add(std::mem::size_of::<otter_gc::GcHeader>())
            .cast::<ErrorStackBody>()
    })
}

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`SymbolPropsBody`].
pub const SYMBOL_PROPS_BODY_TYPE_TAG: u8 = 0x3b;

/// Handle to an object's symbol-keyed property table.
pub(crate) type SymbolPropsHandle = otter_gc::Gc<SymbolPropsBody>;

/// One symbol-keyed own property.
type SymbolProp = (crate::symbol::JsSymbol, SlotData);

/// Header for an object's symbol-keyed own properties; the
/// `(symbol, slot)` records follow it in the same cell. Symbols are
/// compared by identity and kept alive by the realm's well-known /
/// registry roots, so only each slot's values are traced — the same
/// contract the sidecar's `Vec` had.
#[repr(C, align(8))]
pub struct SymbolPropsBody {
    /// Records the trailing array can hold.
    capacity: u32,
    /// Records written, and therefore traced.
    len: u32,
}

impl SymbolPropsBody {
    /// Trailing bytes a table of `capacity` records needs.
    #[must_use]
    fn trailing_bytes(capacity: usize) -> usize {
        capacity * std::mem::size_of::<SymbolProp>()
    }

    fn new(capacity: usize) -> Self {
        Self {
            capacity: u32::try_from(capacity).expect("symbol prop capacity exceeds u32"),
            len: 0,
        }
    }

    fn capacity(&self) -> usize {
        self.capacity as usize
    }

    fn len(&self) -> usize {
        self.len as usize
    }

    fn entries_ptr(&self) -> *mut SymbolProp {
        // SAFETY: the allocation reserved `trailing_bytes(capacity)`
        // immediately after this header.
        unsafe {
            (self as *const Self as *mut u8)
                .add(std::mem::size_of::<Self>())
                .cast()
        }
    }

    /// The written records.
    fn entries(&self) -> &[SymbolProp] {
        // SAFETY: the first `len` records were written before the table
        // became reachable.
        unsafe { std::slice::from_raw_parts(self.entries_ptr().cast_const(), self.len()) }
    }

    /// The written records, mutably.
    fn entries_mut(&mut self) -> &mut [SymbolProp] {
        // SAFETY: as in `entries`.
        unsafe { std::slice::from_raw_parts_mut(self.entries_ptr(), self.len()) }
    }

    /// Append a record. The caller must have reserved capacity: growth
    /// allocates, and a payload borrow has no heap to allocate from.
    fn push(&mut self, entry: SymbolProp) {
        let index = self.len();
        debug_assert!(
            index < self.capacity(),
            "symbol prop push without a reservation"
        );
        // SAFETY: `index < capacity`, so the slot is inside the table.
        unsafe { self.entries_ptr().add(index).write(entry) };
        self.len += 1;
    }

    /// Remove the record at `index`, sliding later records down.
    fn remove(&mut self, index: usize) {
        let len = self.len();
        debug_assert!(index < len);
        for i in index..len - 1 {
            // SAFETY: both slots are inside the written prefix.
            unsafe {
                let next = self.entries_ptr().add(i + 1).read();
                self.entries_ptr().add(i).write(next);
            }
        }
        self.len -= 1;
    }
}

const _: () = assert!(
    std::mem::size_of::<SymbolPropsBody>().is_multiple_of(std::mem::align_of::<SymbolProp>())
);

impl otter_gc::SafeTraceable for SymbolPropsBody {
    const TYPE_TAG: u8 = SYMBOL_PROPS_BODY_TYPE_TAG;

    fn trace_slots_safe(&mut self, v: &mut SlotVisitor<'_>) {
        for (sym, slot) in self.entries_mut() {
            // The key's symbol handle (and its description string) must
            // relocate with the table: symbol property lookup is handle
            // identity, so an unvisited key would compare unequal to the
            // relocated well-known symbol after a snapshot restore.
            sym.trace_value_slots(v);
            match &mut slot.kind {
                SlotKind::Data => slot.value.trace_value_slot_mut(v),
                SlotKind::Accessor(pair) => {
                    if let Some(g) = &mut pair.getter {
                        g.trace_value_slot_mut(v);
                    }
                    if let Some(s) = &mut pair.setter {
                        s.trace_value_slot_mut(v);
                    }
                }
            }
        }
    }

    /// The trailing array lives in the heap cell, not in this body, so a
    /// pending copy on the stack has nothing to trace: everything
    /// `trace_slots_safe` walks is storage that does not exist yet.
    fn trace_pending_slots_safe(&mut self, _visitor: &mut SlotVisitor<'_>) {}
}

impl SymbolPropsBody {
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        for (_, slot) in self.entries() {
            match &slot.kind {
                SlotKind::Data => crate::code_liveness::visit_value(&slot.value, visitor),
                SlotKind::Accessor(pair) => {
                    if let Some(value) = &pair.getter {
                        crate::code_liveness::visit_value(value, visitor);
                    }
                    if let Some(value) = &pair.setter {
                        crate::code_liveness::visit_value(value, visitor);
                    }
                }
            }
        }
    }
}

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`SlotMetaBody`].
pub const SLOT_META_BODY_TYPE_TAG: u8 = 0x3c;

/// Handle to an object's materialized per-slot metadata table.
pub(crate) type SlotMetaHandle = otter_gc::Gc<SlotMetaBody>;

/// Header for materialized per-slot attribute metadata; the
/// [`SlotMeta`] records follow it in the same cell. Metadata holds no
/// GC references, so the body traces nothing — it exists purely so a
/// dictionary-mode or attribute-overridden object owns its metadata
/// inside the cage.
#[repr(C)]
pub struct SlotMetaBody {
    /// Records the trailing array can hold.
    capacity: u32,
    /// Records written.
    len: u32,
}

impl SlotMetaBody {
    fn trailing_bytes(capacity: usize) -> usize {
        capacity * std::mem::size_of::<SlotMeta>()
    }

    fn new(capacity: usize) -> Self {
        Self {
            capacity: u32::try_from(capacity).expect("slot meta capacity exceeds u32"),
            len: 0,
        }
    }

    fn capacity(&self) -> usize {
        self.capacity as usize
    }

    fn len(&self) -> usize {
        self.len as usize
    }

    fn entries_ptr(&self) -> *mut SlotMeta {
        // SAFETY: the allocation reserved `trailing_bytes(capacity)`
        // immediately after this header.
        unsafe {
            (self as *const Self as *mut u8)
                .add(std::mem::size_of::<Self>())
                .cast()
        }
    }

    fn entries(&self) -> &[SlotMeta] {
        // SAFETY: the first `len` records were written before the table
        // became reachable.
        unsafe { std::slice::from_raw_parts(self.entries_ptr().cast_const(), self.len()) }
    }

    fn entries_mut(&mut self) -> &mut [SlotMeta] {
        // SAFETY: as in `entries`.
        unsafe { std::slice::from_raw_parts_mut(self.entries_ptr(), self.len()) }
    }

    fn push(&mut self, meta: SlotMeta) {
        let index = self.len();
        debug_assert!(
            index < self.capacity(),
            "slot meta push without a reservation"
        );
        // SAFETY: `index < capacity`.
        unsafe { self.entries_ptr().add(index).write(meta) };
        self.len += 1;
    }

    fn remove(&mut self, index: usize) {
        let len = self.len();
        debug_assert!(index < len);
        for i in index..len - 1 {
            // SAFETY: both slots are inside the written prefix.
            unsafe {
                let next = self.entries_ptr().add(i + 1).read();
                self.entries_ptr().add(i).write(next);
            }
        }
        self.len -= 1;
    }

    fn clear(&mut self) {
        self.len = 0;
    }
}

impl otter_gc::SafeTraceable for SlotMetaBody {
    const TYPE_TAG: u8 = SLOT_META_BODY_TYPE_TAG;

    /// Deliberately empty: [`SlotMeta`] holds no GC references.
    fn trace_slots_safe(&mut self, _v: &mut SlotVisitor<'_>) {}
}

/// The table payload behind `handle`, or `None` for a null handle.
#[must_use]
fn slot_meta_body_of(handle: SlotMetaHandle) -> Option<*mut SlotMetaBody> {
    if handle.is_null() {
        return None;
    }
    let header = handle.as_header_ptr();
    // SAFETY: a non-null handle names a live cell whose payload is a
    // `SlotMetaBody` one header past the start.
    Some(unsafe {
        header
            .cast::<u8>()
            .add(std::mem::size_of::<otter_gc::GcHeader>())
            .cast::<SlotMetaBody>()
    })
}

/// Allocate a slot-meta table holding `metas`, with the caller's roots
/// live across the allocation. The records are plain attribute bits, so
/// the copy needs no barriers.
fn slot_meta_table_from(
    heap: &mut otter_gc::GcHeap,
    metas: &[SlotMeta],
    capacity: usize,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<SlotMetaHandle, otter_gc::OutOfMemory> {
    let capacity = capacity.max(metas.len()).max(1);
    let table: SlotMetaHandle = heap.alloc_variable_with_roots(
        SlotMetaBody::new(capacity),
        SlotMetaBody::trailing_bytes(capacity),
        external_visit,
    )?;
    if !metas.is_empty() {
        // SAFETY: the handle names the table just allocated.
        let body = slot_meta_body_of(table).expect("fresh table");
        for meta in metas {
            // SAFETY: capacity covers every record.
            unsafe { (*body).push(*meta) };
        }
    }
    Ok(table)
}

/// The table payload behind `handle`, or `None` for a null handle.
#[must_use]
fn symbol_props_body_of(handle: SymbolPropsHandle) -> Option<*mut SymbolPropsBody> {
    if handle.is_null() {
        return None;
    }
    let header = handle.as_header_ptr();
    // SAFETY: a non-null handle names a live cell whose payload is a
    // `SymbolPropsBody` one header past the start.
    Some(unsafe {
        header
            .cast::<u8>()
            .add(std::mem::size_of::<otter_gc::GcHeader>())
            .cast::<SymbolPropsBody>()
    })
}

/// Make room for one more symbol property on `object`.
///
/// Ensures the sidecar and grows the symbol table when it is full, with
/// `object` and the caller's pending values rooted across both
/// allocations. Entries are copied behind the mutator's back, so the
/// edges the copy creates are remembered against the new table before
/// this returns.
///
/// # Errors
/// Propagates [`otter_gc::OutOfMemory`].
fn reserve_symbol_prop_capacity(
    object: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    ensure_exotic_with_roots(object, heap, external_visit)?;
    let sidecar = heap.read_payload(*object, |body| body.exotic.get());
    // SAFETY: `ensure_exotic` above guarantees a live sidecar.
    let (current, len, capacity) = {
        let exotic = exotic_body_of(sidecar).expect("sidecar reserved above");
        // SAFETY: live sidecar payload; read-only peek.
        let handle = unsafe { (*exotic).symbol_props };
        match symbol_props_body_of(handle) {
            // SAFETY: non-null handle names a live table.
            Some(table) => unsafe { (handle, (*table).len(), (*table).capacity()) },
            None => (handle, 0, 0),
        }
    };
    if len < capacity {
        return Ok(());
    }
    let grown = (capacity * 2).max(2);
    let owner_slot = std::ptr::addr_of_mut!(*object);
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        visitor(owner_slot.cast::<RawGc>());
    };
    let table: SymbolPropsHandle = heap.alloc_variable_with_roots(
        SymbolPropsBody::new(grown),
        SymbolPropsBody::trailing_bytes(grown),
        &mut visit,
    )?;
    // Carry the old records over and install the new table. The old
    // table did not move (old space), so `current` still names it.
    if let Some(old) = symbol_props_body_of(current) {
        // SAFETY: both tables are live; the new one has room for every
        // old record.
        unsafe {
            let new_body = symbol_props_body_of(table).expect("fresh table");
            for entry in (*old).entries() {
                (*new_body).push(entry.clone());
            }
        }
    }
    let owner = *object;
    let sidecar = heap.read_payload(owner, |body| body.exotic.get());
    heap.with_payload(sidecar, |exotic| {
        exotic.symbol_props = table;
        true
    });
    heap.record_write(sidecar, &table);
    // The copied-in records hold edges the barrier never saw.
    if let Some(new_body) = symbol_props_body_of(table) {
        // SAFETY: live table payload.
        let entries: Vec<SymbolProp> = unsafe { (*new_body).entries().to_vec() };
        for (sym, slot) in entries {
            heap.record_write(table, &sym);
            heap.record_write(table, &slot.value);
            if let SlotKind::Accessor(pair) = &slot.kind {
                if let Some(g) = &pair.getter {
                    heap.record_write(table, g);
                }
                if let Some(s) = &pair.setter {
                    heap.record_write(table, s);
                }
            }
        }
    }
    Ok(())
}

/// Remember a symbol-keyed entry write against the table that holds it.
///
/// The key is an edge in its own right — [`SymbolPropsBody`] traces the
/// symbol body handle and its description string — so recording only the
/// descriptor would leave the key's own old→young edges invisible to the
/// scavenger.
fn record_symbol_entry_write<V>(
    heap: &mut otter_gc::GcHeap,
    object: JsObject,
    key: &crate::symbol::JsSymbol,
    value: &V,
) where
    V: otter_gc::GcStore + ?Sized,
{
    record_symbol_prop_write(heap, object, key);
    record_symbol_prop_write(heap, object, value);
}

/// Remember a write against the symbol table that actually holds it,
/// and against the sidecar and object above it.
fn record_symbol_prop_write<V>(heap: &mut otter_gc::GcHeap, object: JsObject, value: &V)
where
    V: otter_gc::GcStore + ?Sized,
{
    record_exotic_write(heap, object, value);
    let sidecar = heap.read_payload(object, |body| body.exotic.get());
    if let Some(exotic) = exotic_body_of(sidecar) {
        // SAFETY: live sidecar payload; read-only peek at the handle.
        let table = unsafe { (*exotic).symbol_props };
        if !table.is_null() {
            heap.record_write(table, value);
        }
    }
}

/// Give `object` an exotic sidecar if it does not have one.
///
/// Creating the sidecar allocates, and an allocation can move the object,
/// so this runs outside the payload borrow with `object` rooted — the
/// same split property-slab growth uses. A no-op after the first call,
/// which is every write but one.
///
/// # Errors
/// Propagates [`otter_gc::OutOfMemory`].
pub fn ensure_exotic(
    object: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
) -> Result<(), otter_gc::OutOfMemory> {
    ensure_exotic_with_roots(object, heap, &mut |_| {})
}

/// [`ensure_exotic`], with pending caller values rooted across the
/// sidecar allocation.
///
/// # Errors
/// Propagates [`otter_gc::OutOfMemory`].
pub(crate) fn ensure_exotic_with_roots(
    object: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    if !heap.read_payload(*object, |body| body.exotic.is_null()) {
        return Ok(());
    }
    let owner_slot = std::ptr::addr_of_mut!(*object);
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        visitor(owner_slot.cast::<RawGc>());
    };
    let sidecar: ExoticHandle =
        heap.alloc_variable_with_roots(ExoticSlots::default(), 0, &mut visit)?;
    let owner = *object;
    heap.with_payload(owner, |body| {
        body.exotic.set(sidecar);
        true
    });
    // Installed by a raw payload write, so record the edge the mutator
    // barrier would have.
    heap.record_write(owner, &sidecar);
    Ok(())
}

/// [`ensure_exotic`], with direct `Value` words kept live across the
/// sidecar allocation.
///
/// Property stores call this before the values have entered a traced object.
/// A young referent may therefore move while the sidecar is allocated; tracing
/// the caller-owned words here rewrites their embedded offsets in place.
///
/// # Errors
/// Propagates [`otter_gc::OutOfMemory`].
pub(crate) fn ensure_exotic_with_pending_values(
    object: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    pending: &mut [Value],
) -> Result<(), otter_gc::OutOfMemory> {
    let pending_base = pending.as_mut_ptr();
    let pending_len = pending.len();
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        for index in 0..pending_len {
            // SAFETY: `index < pending_len`, and `pending` outlives the
            // allocation. `Value` rewrites a moving cell offset in place.
            unsafe { (*pending_base.add(index)).trace_value_slot_mut(visitor) };
        }
    };
    ensure_exotic_with_roots(object, heap, &mut visit)
}

/// Remember a write against the sidecar that actually holds it.
///
/// The exotic slots are their own old-space body, so the object is not
/// the parent of what they hold: a scavenge re-tracing the object finds
/// one edge, sees an old child, and stops. The object is remembered too,
/// because the same call sites also write slots it owns.
pub(crate) fn record_exotic_write<V>(heap: &mut otter_gc::GcHeap, object: JsObject, value: &V)
where
    V: otter_gc::GcStore + ?Sized,
{
    heap.record_write(object, value);
    let sidecar = heap.read_payload(object, |body| body.exotic.get());
    if !sidecar.is_null() {
        heap.record_write(sidecar, value);
    }
}

/// Byte offset of the shape token within an [`ObjectBody`] payload. The
/// JIT reads the shape handle here for the monomorphic IC guard.
pub(crate) const OBJECT_BODY_SHAPE_OFFSET: usize = std::mem::offset_of!(ObjectBody, shape);

/// Byte offset of the structural identity used when [`ObjectBody::shape`] is
/// null. Generated dictionary-mode guards compare this word after proving the
/// ordinary shape handle is absent.
pub(crate) const OBJECT_BODY_DICTIONARY_SHAPE_ID_OFFSET: usize =
    std::mem::offset_of!(ObjectBody, dictionary_shape_id);

/// Byte offset of the string-keyed value slab pointer within an [`ObjectBody`]
/// payload. The JIT reads this pointer after its shape guard and then indexes
/// the contiguous slab by `slot * size_of::<Value>()` (8 bytes). The loaded
/// word is already the runtime `Value`; no property-slot codec is involved.
pub(crate) const OBJECT_BODY_VALUES_PTR_OFFSET: usize =
    std::mem::offset_of!(ObjectBody, values_ptr);

/// Byte offset of the flat [`ObjectBody::jit_proto`] mirror within an
/// [`ObjectBody`] payload. The method-inline guard reads the receiver's
/// prototype handle here to chase the prototype chain in machine code.
pub(crate) const OBJECT_BODY_JIT_PROTO_OFFSET: usize = std::mem::offset_of!(ObjectBody, jit_proto);

/// Byte offset of the in-body inline slab [`ObjectBody::inline_values`]. The
/// JIT bakes the inline `New` store sequence (and an inline read for a small
/// object whose `slab_len <= INLINE_SLOT_CAP`) against this offset.
pub(crate) const OBJECT_BODY_INLINE_VALUES_OFFSET: usize =
    std::mem::offset_of!(ObjectBody, inline_values);

/// Byte offset of the [`ObjectBody::slab_len`] counter. The JIT reads it to
/// branch inline-vs-overflow and to bounds-check an inline slot store.
pub(crate) const OBJECT_BODY_SLAB_LEN_OFFSET: usize = std::mem::offset_of!(ObjectBody, slab_len);

/// Byte offset of the out-of-line slab handle. The JIT reads it to branch
/// inline-vs-overflow: a null handle means the slots live in
/// [`ObjectBody::inline_values`]. `slab_len` cannot decide this — the
/// capacity model can move a `len <= INLINE_SLOT_CAP` object's slots out of
/// line (an existing-slot slow store reserves ahead), and a spilled slab
/// that shrinks back stays out of line.
pub(crate) const OBJECT_BODY_SLAB_HANDLE_OFFSET: usize = std::mem::offset_of!(ObjectBody, slab);
/// Byte offset of the ordinary `[[Extensible]]` flag.
pub(crate) const OBJECT_BODY_EXTENSIBLE_OFFSET: usize =
    std::mem::offset_of!(ObjectBody, extensible);
/// Byte offset of the `u8` fast-shape eligibility discriminant.
pub(crate) const OBJECT_BODY_SHAPE_CACHE_MODE_OFFSET: usize =
    std::mem::offset_of!(ObjectBody, shape_cache_mode);
/// Byte offset of the in-place descriptor-override Boolean.
pub(crate) const OBJECT_BODY_SLOT_ATTRS_OVERRIDDEN_OFFSET: usize =
    std::mem::offset_of!(ObjectBody, slot_attrs_overridden);
/// Byte offset of the 4-byte rare-state GC handle inside [`ExoticSlot`].
/// A zero word proves the complete sidecar is absent.
pub(crate) const OBJECT_BODY_EXOTIC_HANDLE_OFFSET: usize =
    std::mem::offset_of!(ObjectBody, exotic) + std::mem::offset_of!(ExoticSlot, handle);
/// Total fixed cell bytes for an ordinary object, including its GC header.
pub(crate) const OBJECT_BODY_CELL_BYTES: usize = align_object_cell_bytes();

const fn align_object_cell_bytes() -> usize {
    let bytes = otter_gc::header::HEADER_SIZE + std::mem::size_of::<ObjectBody>();
    (bytes + otter_gc::OBJECT_ALIGNMENT - 1) & !(otter_gc::OBJECT_ALIGNMENT - 1)
}

// The JIT bakes these offsets into emitted property loads, the inline `New`
// store sequence, and the deopt frame-state record, so they are a frozen ABI:
// pin every one to its EXACT value (not `>=` / `%`) so an accidental field
// reorder is a compile error rather than a frozen JIT baking garbage. Update
// these literals deliberately, in lockstep with the JIT, when the body changes.
const _: () = assert!(OBJECT_BODY_SHAPE_OFFSET == 0);
const _: () = assert!(OBJECT_BODY_VALUES_PTR_OFFSET == 8);
const _: () = assert!(OBJECT_BODY_DICTIONARY_SHAPE_ID_OFFSET == 24);
const _: () = assert!(OBJECT_BODY_JIT_PROTO_OFFSET == 36);
const _: () = assert!(OBJECT_BODY_INLINE_VALUES_OFFSET == 56);
const _: () = assert!(OBJECT_BODY_SLAB_LEN_OFFSET == 80);
const _: () = assert!(OBJECT_BODY_SLAB_HANDLE_OFFSET == 16);
const _: () = assert!(OBJECT_BODY_EXTENSIBLE_OFFSET == 40);
const _: () = assert!(OBJECT_BODY_SHAPE_CACHE_MODE_OFFSET == 32);
const _: () = assert!(OBJECT_BODY_SLOT_ATTRS_OVERRIDDEN_OFFSET == 41);
const _: () = assert!(OBJECT_BODY_EXOTIC_HANDLE_OFFSET == 48);
const _: () = assert!(OBJECT_BODY_CELL_BYTES == 96);
// The shape guard word must sit at offset 0 (single-compare guard) and the
// slab base must stay 8-aligned for the JIT's pointer load.
const _: () = assert!(OBJECT_BODY_VALUES_PTR_OFFSET.is_multiple_of(8));

// Pin the hot object footprint. Per-slot metadata lives out of line only for
// dictionary-mode / attribute-overridden objects, while string-keyed values use
// one contiguous slab addressed from a cached pointer. Three 8-byte inline
// values keep the complete body at the old 88-byte footprint.
const _: () = assert!(std::mem::size_of::<ObjectBody>() == 88);

impl ObjectBody {
    /// Number of live string-keyed slots.
    #[inline]
    fn slab_len(&self) -> usize {
        self.slab_len as usize
    }

    /// Whether the slab is held inline in the body (small object) rather than
    /// in an out-of-line [`slot_slab::SlotSlabBody`].
    #[inline]
    fn slab_is_inline(&self) -> bool {
        self.slab.is_null()
    }

    /// Words the current slab can hold without growing.
    #[inline]
    fn slab_capacity(&self) -> usize {
        if self.slab.is_null() {
            INLINE_SLOT_CAP
        } else {
            // SAFETY: a non-null slab handle addresses a live
            // `SlotSlabBody`; reading its capacity field touches only the
            // body header.
            unsafe { (*self.slab_body_ptr()).capacity() }
        }
    }

    /// Raw pointer to the out-of-line slab body. Only valid when
    /// [`Self::slab`] is non-null.
    #[inline]
    fn slab_body_ptr(&self) -> *mut slot_slab::SlotSlabBody {
        debug_assert!(!self.slab.is_null());
        // SAFETY: a handle decompresses to its header without consulting
        // the heap, and the payload follows the header.
        unsafe {
            (self.slab.as_header_ptr() as *mut u8)
                .add(otter_gc::header::HEADER_SIZE)
                .cast()
        }
    }

    /// Read the value word for string-keyed slot `i`.
    #[inline]
    fn slot_word(&self, i: usize) -> Value {
        debug_assert!(
            self.values_ptr_is_current(),
            "stale values_ptr on slab read: body={:p} values_ptr={:p} expected={:p} slab_len={}",
            self,
            self.values_ptr.get(),
            self.expected_values_ptr(),
            self.slab_len,
        );
        debug_assert!(i < self.slab_len(), "slab read out of range");
        // SAFETY: the base is the always-current slab base and `i` is in
        // range, so this addresses a live word in whichever buffer is
        // active.
        unsafe { *self.values_ptr.get().add(i) }
    }

    /// Read the data value for string-keyed slot `i`.
    #[inline]
    fn data_value(&self, _heap: &otter_gc::GcHeap, i: usize) -> Value {
        self.slot_word(i)
    }

    /// Write a value into string-keyed slot `i`.
    #[inline]
    fn set_data_value(&mut self, i: usize, value: Value) {
        debug_assert!(
            self.values_ptr_is_current(),
            "stale values_ptr on slab write"
        );
        debug_assert!(i < self.slab_len(), "slab write out of range");
        // SAFETY: same in-range word as `slot_word`.
        unsafe { *self.values_ptr.get().add(i) = value };
    }

    /// Append one slab word, migrating the inline slab to `values` on the
    /// transition past [`INLINE_SLOT_CAP`].
    #[inline]
    fn push_slab_word(&mut self, value: Value) {
        let len = self.slab_len();
        // Branch on the actual storage location, not on `len`: a slab that
        // spilled and then shrank back to `INLINE_SLOT_CAP` via
        // `remove_slab_word` stays out of line, so a length-based spill
        // test here would re-copy the inline array over the live overflow
        // vector and duplicate slots (delete-then-add on a 3+-property
        // object silently corrupted the slab in release; debug builds
        // panicked with 'overflow slab already populated').
        debug_assert!(
            len < self.slab_capacity(),
            "slab append past reserved capacity: len={len} capacity={}; \
             the caller must reserve through `reserve_slot_capacity` first",
            self.slab_capacity(),
        );
        self.slab_len += 1;
        self.refresh_values_ptr();
        // SAFETY: the append index is inside the reserved capacity, and
        // the base was just refreshed for the new length.
        unsafe { *self.values_ptr.get().add(len) = value };
    }

    /// Remove the slab word at `i`, shifting later words down. Stays out of line
    /// once spilled (delete normalizes to dictionary mode, an uncommon path).
    #[inline]
    fn remove_slab_word(&mut self, i: usize) {
        let len = self.slab_len();
        // SAFETY: `i < len <= capacity`; the shift stays inside the live
        // words of whichever buffer is active.
        unsafe {
            let base = self.values_ptr.get();
            std::ptr::copy(base.add(i + 1), base.add(i), len - i - 1);
            *base.add(len - 1) = Value::default();
        }
        self.slab_len -= 1;
        self.refresh_values_ptr();
    }

    /// Install a larger out-of-line slab, copying the live words across.
    ///
    /// The body never grows its own storage: growth is an allocation, and
    /// an allocation inside a property store is where an object gets moved
    /// out from under its own mutation. So the caller reserves first
    /// ([`reserve_slot_capacity`]) and the body only ever writes into
    /// capacity that already exists.
    fn adopt_slab(&mut self, slab: slot_slab::SlotSlabHandle) {
        debug_assert!(!slab.is_null(), "adopting a null slab");
        let live = self.slab_len();
        let source = self.values_ptr.get();
        // SAFETY: the new slab was allocated with capacity for at least
        // `live` words and does not overlap the current buffer.
        unsafe {
            let target = (*(((slab.as_header_ptr() as *mut u8)
                .add(otter_gc::header::HEADER_SIZE))
            .cast::<slot_slab::SlotSlabBody>()))
            .words_ptr();
            if live != 0 {
                std::ptr::copy_nonoverlapping(source, target, live);
            }
        }
        self.slab = slab;
        self.refresh_values_ptr();
    }

    /// Append a new string-keyed own slot at flat index `index` (the pre-append
    /// property count). For a shaped, non-overridden object the hidden class
    /// already records the slot's attributes, so only the flat value is
    /// written; a materialized object (dictionary-mode or attribute-overridden)
    /// also pushes `meta` onto its per-slot metadata vector so it stays
    /// index-aligned with the value array. For an accessor slot `value` is the
    /// [`AccessorCellBody`] handle produced by [`SlotData::into_flat`].
    fn push_slot(&mut self, index: usize, meta: SlotMeta, value: Value) {
        debug_assert_eq!(self.slab_len(), index, "value slab append desynced");
        self.push_slab_word(value);
        if self.slots_materialized() {
            debug_assert_eq!(self.slots().len(), index, "materialized slots desynced");
            self.slots_mut().push(meta);
        }
    }

    /// Overwrite the string-keyed slot at `i` with new metadata + flat value.
    ///
    /// Used by the `defineProperty`-on-existing merge paths, which change a
    /// slot's attributes or data↔accessor kind. `attr_shape` is the
    /// attribute-encoding hidden class the object transitions to so the shape
    /// keeps recording the slot's attributes (the common, fast case): a shaped,
    /// non-overridden object stores nothing per-slot. A previously
    /// attribute-overridden object keeps its materialized metadata in lockstep.
    /// `None` — only for dictionary-mode (null shape) or internal construction
    /// paths without a shape runtime — keeps the materialized metadata
    /// authoritative; the caller must have materialized it first
    /// ([`materialize_slots`]).
    fn set_slot(
        &mut self,
        i: usize,
        meta: SlotMeta,
        value: Value,
        attr_shape: Option<ShapeHandle>,
    ) {
        self.set_data_value(i, value);
        match attr_shape {
            Some(shape) => {
                debug_assert_object_shape_handle(shape, "slot attribute shape install");
                debug_assert_object_shape_handle(shape, "shape-slot store");
                self.shape = shape;
                // A previously overridden object keeps reading from its
                // materialized metadata, so keep that entry current; a
                // non-overridden object reads the rebuilt shape and stores none.
                if self.slot_attrs_overridden {
                    self.slots_mut().entries_mut()[i] = meta;
                }
            }
            None => {
                if !self.shape.is_null() {
                    self.slot_attrs_overridden = true;
                }
                debug_assert!(
                    self.slots_materialized(),
                    "set_slot(None) needs materialized slots"
                );
                self.slots_mut().entries_mut()[i] = meta;
            }
        }
    }

    /// Per-slot `(flags, is_accessor)` for the string-keyed slot at `i`.
    ///
    /// Reads from the hidden class for a shaped object whose attributes have
    /// not diverged (the common case — every shaped slot recorded its
    /// attributes on the transition that created it), and falls back to the
    /// authoritative materialized metadata for dictionary-mode or
    /// attribute-overridden objects.
    #[inline]
    fn slot_attrs(&self, heap: &otter_gc::GcHeap, i: usize) -> (PropertyFlags, bool) {
        if !self.shape.is_null()
            && !self.slot_attrs_overridden
            && let Some(attrs) = shape_body::shape_slot_attrs(heap, self.shape, i as u32)
        {
            return attrs;
        }
        let meta = &self.slots()[i];
        (meta.flags, meta.is_accessor)
    }

    /// Snapshot the string-keyed slot at `i` as an owned [`SlotData`],
    /// reconstructing the getter/setter pair from the accessor cell.
    fn slot_data(&self, heap: &otter_gc::GcHeap, i: usize) -> SlotData {
        let (flags, is_accessor) = self.slot_attrs(heap, i);
        if is_accessor {
            let (getter, setter) = read_accessor_cell(heap, self.data_value(heap, i));
            SlotData {
                flags,
                kind: SlotKind::accessor(getter, setter),
                value: Value::undefined(),
            }
        } else {
            SlotData {
                flags,
                kind: SlotKind::Data,
                value: self.data_value(heap, i),
            }
        }
    }

    /// [`PropertyLookup`] for the string-keyed slot at `i`.
    fn slot_lookup_at(&self, heap: &otter_gc::GcHeap, i: usize) -> PropertyLookup {
        let (flags, is_accessor) = self.slot_attrs(heap, i);
        if is_accessor {
            let (getter, setter) = read_accessor_cell(heap, self.data_value(heap, i));
            return PropertyLookup::Accessor {
                getter,
                setter,
                flags,
            };
        }
        PropertyLookup::Data {
            value: self.data_value(heap, i),
            flags,
        }
    }

    /// [`PropertyDescriptor`] for the string-keyed slot at `i`.
    fn slot_descriptor_at(&self, heap: &otter_gc::GcHeap, i: usize) -> PropertyDescriptor {
        let (flags, is_accessor) = self.slot_attrs(heap, i);
        if is_accessor {
            let (getter, setter) = read_accessor_cell(heap, self.data_value(heap, i));
            return PropertyDescriptor {
                flags,
                kind: DescriptorKind::Accessor { getter, setter },
            };
        }
        PropertyDescriptor {
            flags,
            kind: DescriptorKind::Data {
                value: self.data_value(heap, i),
            },
        }
    }

    /// Remove the string-keyed slot at `i`, shifting later values down so the
    /// materialized metadata and the flat value array stay index-aligned. Only
    /// reached on a materialized object (delete normalizes to dictionary mode,
    /// materializing per-slot metadata first), so `slots()` is authoritative.
    fn remove_slot(&mut self, i: usize) {
        let len = self.slots().len();
        debug_assert_eq!(self.slab_len(), len, "value slab metadata desynced");
        self.remove_slab_word(i);
        self.slots_mut().remove(i);
    }

    /// Refresh the cached slab base after any operation that may move the slab
    /// (inline ↔ out-of-line migration, vector realloc, body relocation). Points
    /// at the active buffer — the in-body inline array for a small object, the
    /// out-of-line vector once spilled — so slot access and the JIT both index
    /// `values_ptr` uniformly. An inline slotless object has no stable base and
    /// keeps this null; a fresh object with reserved out-of-line capacity
    /// already points at that slab before its first slot is published.
    ///
    /// This is the **sole** writer of the always-current `values_ptr` base
    /// invariant: no code path may leave `values_ptr` aimed at a stale
    /// buffer once the mutator can observe the body. Every mutation that grows,
    /// shrinks, spills, or relocates the slab calls this; the relocating
    /// scavenger calls it from `trace_slots_safe` after the body memcpy. A
    /// future JIT bakes a single `values_ptr` load as the slab base for every
    /// own-data slot, so a stale base is a silently-baked wild load — hence
    /// [`Self::values_ptr_is_current`] verifies it at every slab access in debug.
    #[inline]
    fn refresh_values_ptr(&self) {
        self.values_ptr.set(self.expected_values_ptr().cast_mut());
    }

    /// Debug verifier for the always-current `values_ptr` base invariant:
    /// the cached base must equal what [`Self::refresh_values_ptr`] would
    /// recompute right now. Asserted at every slab access in debug
    /// (compiled out in release) so any new relocation/grow/shrink path that
    /// forgets to refresh fails deterministically under `OTTER_GC_STRESS`
    /// instead of baking a wild JIT load. Reads never run mid-relocation (STW
    /// pauses the mutator, and `trace_slots_safe` refreshes before yielding),
    /// so a current pointer here is the steady-state contract, not a race.
    #[inline]
    fn values_ptr_is_current(&self) -> bool {
        self.values_ptr.get().cast_const() == self.expected_values_ptr()
    }

    #[inline]
    fn expected_values_ptr(&self) -> *const Value {
        if !self.slab_is_inline() {
            // SAFETY: the handle is non-null, so it addresses a live slab
            // whose words follow its header.
            unsafe { (*self.slab_body_ptr()).words_ptr().cast_const() }
        } else if self.slab_len == 0 {
            std::ptr::null()
        } else {
            self.inline_values.as_ptr()
        }
    }

    // --- Lazily-boxed exotic slots -----------------------------------------
    // Reads return the field's default when no `ExoticSlots` is allocated;
    // mutators allocate the box on first write. Plain objects never touch it.

    /// Reconstruct the `[[Prototype]]`. Common case (Null / ordinary object)
    /// reads only `jit_proto`; a boxed `proto_override` covers Value / Proxy.
    #[inline]
    fn prototype(&self) -> ObjectPrototype {
        if let Some(over) = self.exotic().and_then(|e| e.proto_override.as_ref()) {
            return over.clone();
        }
        if self.jit_proto.is_null() {
            ObjectPrototype::Null
        } else {
            ObjectPrototype::Object(self.jit_proto)
        }
    }

    /// Shared ref to the boxed exotic slots, if any.
    #[inline]
    fn exotic(&self) -> Option<&ExoticSlots> {
        // SAFETY: a non-null handle names a live sidecar payload that
        // outlives this borrow of the object body.
        exotic_body_of(self.exotic.get()).map(|body| unsafe { &*body })
    }

    /// Exclusive ref to the exotic sidecar.
    ///
    /// The sidecar is a GC body, so creating one allocates and a payload
    /// borrow has no heap. Every mutating path calls [`ensure_exotic`]
    /// first, outside the borrow; arriving here with no sidecar is a
    /// caller that forgot.
    #[inline]
    fn exotic_mut(&mut self) -> &mut ExoticSlots {
        let body = exotic_body_of(self.exotic.get())
            .expect("exotic slots written without ensure_exotic reserving them");
        // SAFETY: as in `exotic`; `&mut self` rules out an aliasing read
        // through this object body.
        unsafe { &mut *body }
    }

    #[inline]
    fn host_data_ref(&self) -> Option<&HostData> {
        self.exotic().and_then(|e| e.host_data.as_ref())
    }
    #[inline]
    fn host_data_mut_opt(&mut self) -> Option<&mut HostData> {
        // SAFETY: a non-null handle names a live sidecar payload.
        exotic_body_of(self.exotic.get()).and_then(|body| unsafe { (*body).host_data.as_mut() })
    }
    #[inline]
    fn boolean_data(&self) -> Option<bool> {
        self.exotic().and_then(|e| e.boolean_data)
    }
    #[inline]
    fn number_data(&self) -> Option<NumberValue> {
        self.exotic().and_then(|e| e.number_data)
    }
    #[inline]
    fn string_data(&self) -> Option<JsString> {
        self.exotic().and_then(|e| e.string_data)
    }
    #[inline]
    fn symbol_data(&self) -> Option<crate::symbol::JsSymbol> {
        self.exotic().and_then(|e| e.symbol_data)
    }
    #[inline]
    fn bigint_data(&self) -> Option<BigIntValue> {
        self.exotic().and_then(|e| e.bigint_data)
    }
    #[inline]
    fn date_data(&self) -> Option<f64> {
        self.exotic().and_then(|e| e.date_data)
    }
    #[inline]
    fn is_raw_json(&self) -> bool {
        self.exotic().is_some_and(|e| e.is_raw_json)
    }
    #[inline]
    fn error_data(&self) -> bool {
        self.exotic().is_some_and(|e| e.error_data)
    }
    #[inline]
    fn has_error_stack_frames(&self) -> bool {
        self.exotic()
            .is_some_and(|e| !e.error_stack_frames.is_null())
    }
    #[inline]
    fn is_arguments_object(&self) -> bool {
        self.exotic().is_some_and(|e| e.is_arguments_object)
    }
    #[inline]
    fn call_native(&self) -> Option<Value> {
        self.exotic().and_then(|e| e.call_native)
    }
    #[inline]
    fn constructor_native(&self) -> Option<Value> {
        self.exotic().and_then(|e| e.constructor_native)
    }
    /// Symbol-keyed own props as a slice (`&[]` when no table).
    #[inline]
    fn symbol_props(&self) -> &[(JsSymbol, SlotData)] {
        self.exotic()
            .and_then(|e| symbol_props_body_of(e.symbol_props))
            // SAFETY: a non-null handle names a live table whose prefix
            // outlives this borrow of the object body.
            .map_or(&[], |table| unsafe { (*table).entries() })
    }

    /// Symbol-keyed own props, mutably (`None` when no table).
    #[inline]
    fn symbol_props_mut(&mut self) -> Option<&mut SymbolPropsBody> {
        self.exotic()
            .and_then(|e| symbol_props_body_of(e.symbol_props))
            // SAFETY: as in `symbol_props`; `&mut self` rules out an
            // aliasing read through this object body.
            .map(|table| unsafe { &mut *table })
    }
    /// Dictionary-mode string keys as a slice (`&[]` when no box / fast-shape).
    #[inline]
    fn dict_keys(&self) -> Option<&DictKeysBody> {
        self.exotic()
            .and_then(|e| dict_keys_body_of(e.dictionary_keys))
            // SAFETY: a non-null handle names a live table whose prefix
            // outlives this borrow of the object body.
            .map(|table| unsafe { &*table })
    }

    fn dict_key_count(&self) -> usize {
        self.dict_keys().map_or(0, DictKeysBody::len)
    }
    /// Dictionary-mode `key → slot offset`, or `None`.
    ///
    /// A small dictionary keeps no hash index (see [`DICT_LINEAR_SCAN_MAX`]):
    /// a linear scan over its few short key strings is faster than hashing and
    /// avoids allocating/maintaining a `FxHashMap` per object. This matters for
    /// `JSON.parse`, which builds large numbers of small dictionary objects.
    #[inline]
    fn dictionary_index_get(&self, key: &str) -> Option<u16> {
        self.dict_keys().and_then(|table| table.find(key))
    }

    /// `true` when per-slot metadata is materialized in [`ExoticSlots::slots`]
    /// and is the authoritative attribute source. Dictionary-mode (null shape)
    /// and attribute-overridden objects materialize; the common shaped object
    /// derives attributes from the hidden class and carries none.
    #[inline]
    fn slots_materialized(&self) -> bool {
        self.shape.is_null() || self.slot_attrs_overridden
    }

    /// Materialized per-slot metadata as a slice (`&[]` when the shape is the
    /// authoritative source).
    #[inline]
    fn slots(&self) -> &[SlotMeta] {
        self.exotic()
            .and_then(|e| slot_meta_body_of(e.slots))
            // SAFETY: a non-null handle names a live table whose prefix
            // outlives this borrow of the object body.
            .map_or(&[], |table| unsafe { (*table).entries() })
    }

    /// Exclusive ref to the materialized per-slot metadata vector, allocating
    /// the exotic box on first use. Callers must only reach this on a
    /// materialized object (dictionary-mode or attribute-overridden).
    #[inline]
    fn slots_mut(&mut self) -> &mut SlotMetaBody {
        let table = self
            .exotic()
            .map(|e| e.slots)
            .unwrap_or_else(SlotMetaHandle::null);
        let body = slot_meta_body_of(table)
            .expect("materialized slot metadata written without a reserved table");
        // SAFETY: a non-null handle names a live table; `&mut self`
        // rules out an aliasing read through this object body.
        unsafe { &mut *body }
    }
}

impl std::fmt::Debug for ObjectBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectBody")
            .field("has_shape", &!self.shape.is_null())
            .field("dictionary_len", &self.dict_key_count())
            .field("shape_cache_mode", &self.shape_cache_mode)
            .field("slot_count", &self.slots().len())
            .field(
                "has_prototype",
                &!matches!(self.prototype(), ObjectPrototype::Null),
            )
            .field("symbol_props", &self.symbol_props().len())
            .field("has_host_data", &self.host_data_ref().is_some())
            .field(
                "mapped_arguments",
                &self
                    .host_data_ref()
                    .and_then(|data| data.downcast_ref::<MappedArgumentsData>())
                    .map_or(0, |data| data.entries.len()),
            )
            .field("has_call_native", &self.call_native().is_some())
            .field(
                "has_constructor_native",
                &self.constructor_native().is_some(),
            )
            .field("has_boolean_data", &self.boolean_data().is_some())
            .field("has_number_data", &self.number_data().is_some())
            .field("has_string_data", &self.string_data().is_some())
            .field("has_symbol_data", &self.symbol_data().is_some())
            .field("has_date_data", &self.date_data().is_some())
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
    fn trace_slots_safe(&mut self, v: &mut SlotVisitor<'_>) {
        // No shape-validity assert here: an image restore traces bodies
        // while their handles still carry the capture isolate's offsets,
        // so a trace-entry read of the shape cell would dereference
        // pre-relocation state. The store paths keep the assert.
        if !self.shape.is_null() {
            let p = &mut self.shape as *mut ShapeHandle as *mut RawGc;
            v(p);
        }
        // The ordinary-object / null prototype lives solely in the flat
        // `jit_proto` handle (null == `[[Prototype]]` null); the moving collector
        // forwards it here so a baked inline guard never decompresses a stale
        // offset. Non-ordinary (Value / Proxy) prototypes live in the boxed
        // `proto_override` and are traced in the exotic block below.
        if !self.jit_proto.is_null() {
            let p = &mut self.jit_proto as *mut JsObject as *mut RawGc;
            v(p);
        }
        // The out-of-line slab is an ordinary GC body: trace the handle so a
        // moving collection rewrites it, and let the slab trace its own
        // words. Tracing the handle before `refresh_values_ptr` below is
        // what keeps the cached base pointing at the post-move slab.
        if !self.slab.is_null() {
            debug_assert!(
                self.slab.offset() >= 0x1000 && self.slab.offset().is_multiple_of(8),
                "ObjectBody.slab holds a garbage handle {:#x}",
                self.slab.offset(),
            );
            let p = &mut self.slab as *mut slot_slab::SlotSlabHandle as *mut RawGc;
            v(p);
        }
        // String-keyed property slots: the value slab holds each slot's data
        // value, or — for an accessor slot — a handle to its `AccessorCellBody`.
        // Trace cells in place so the moving scavenger rewrites the live slot,
        // not a copy; an accessor cell traces its own getter/setter through its
        // `Traceable` impl.
        // The relocating scavenger memcpy's this body, which leaves the cached
        // `values_ptr` aimed at the pre-move inline array. Recompute it from the
        // post-move body so both the slot path and a baked JIT load read the live
        // base; the inline slab lives in the body and therefore moves with it.
        self.refresh_values_ptr();
        let base = self.values_ptr.get();
        for i in 0..self.slab_len() {
            // SAFETY: `base` is the live slab base and `i < slab_len`.
            let word = unsafe { base.add(i) };
            // SAFETY: same live in-range value word. `Value` skips immediates
            // and rewrites the low-word GC offset of cells in place.
            unsafe { (*word).trace_value_slot_mut(v) };
        }
        // The exotic sidecar is its own GC body: trace the handle so a
        // moving collection rewrites it, and let the sidecar trace its own
        // slots through its `Traceable` impl.
        if !self.exotic.is_null() {
            v(self.exotic.slot_ptr());
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

/// Make sure `object` can hold `needed` string-keyed slots without growing.
///
/// Growth is the object model's one allocation point on the property-store
/// path, and it is deliberately separated from the store itself: the store
/// runs inside a payload borrow where the heap is unavailable, and an
/// allocation there could move the very object being written. Callers
/// reserve first, with their roots live, then mutate.
///
/// A no-op while the object still fits inline or inside its current slab,
/// which is every store except the one that crosses a capacity boundary.
///
/// # Errors
/// Propagates [`otter_gc::OutOfMemory`] from the slab allocation.
pub(crate) fn reserve_slot_capacity(
    object: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    needed: usize,
    pending: &mut [Value],
) -> Result<(), otter_gc::OutOfMemory> {
    // A materialized object appends per-slot metadata in lockstep with
    // the value it appends, so its meta table must keep pace with the
    // slab — including when the slab itself already has room.
    reserve_slot_meta_capacity(object, heap, needed, pending)?;
    let capacity = heap.read_payload(*object, ObjectBody::slab_capacity);
    if needed <= capacity {
        return Ok(());
    }
    // Double from the current capacity so a run of appends pays for growth
    // a logarithmic number of times, never per append.
    let grown = needed
        .max(capacity.saturating_mul(2))
        .max(INLINE_SLOT_CAP * 2);
    let object_slot = (object as *mut JsObject).cast::<otter_gc::raw::RawGc>();
    let pending_base = pending.as_mut_ptr();
    let pending_len = pending.len();
    let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        // The object is about to receive the slab, so it must survive the
        // allocation that produces it — and the caller's handle is what the
        // collector rewrites, so the caller sees the moved object.
        visitor(object_slot);
        // Pending values are not in the object yet, so trace them as explicit
        // roots across the slab allocation.
        for index in 0..pending_len {
            // SAFETY: `index < pending_len`, and the slice outlives this call.
            unsafe { (*pending_base.add(index)).trace_value_slot_mut(visitor) };
        }
    };
    let slab = slot_slab::alloc_slot_slab(heap, grown, &mut visit)?;
    let owner = *object;
    heap.with_payload(owner, |body| {
        body.adopt_slab(slab);
        true
    });
    // The slab handle was installed by a raw payload write, so record the
    // old-to-young edge the mutator barrier would have. The child of that
    // edge is the slab: naming the owner as its own child makes the barrier
    // read the owner's generation, find it old, and record nothing, leaving
    // an old object pointing at a young slab the next scavenge never visits.
    heap.record_write(owner, &slab);
    Ok(())
}

/// Make room for `needed` materialized per-slot metadata records.
///
/// A no-op for the shaped, non-overridden object that keeps its
/// attributes in the hidden class. The records are plain bits, so only
/// `object` and the caller's pending words need rooting.
///
/// # Errors
/// Propagates [`otter_gc::OutOfMemory`].
pub(crate) fn reserve_slot_meta_capacity(
    object: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    needed: usize,
    pending: &mut [Value],
) -> Result<(), otter_gc::OutOfMemory> {
    let state = heap.read_payload(*object, |body| {
        if !body.slots_materialized() {
            return None;
        }
        let table = body.exotic().map(|e| e.slots).unwrap_or_default();
        let (len, capacity) = match slot_meta_body_of(table) {
            // SAFETY: a non-null handle names a live table.
            Some(body) => unsafe { ((*body).len(), (*body).capacity()) },
            None => (0, 0),
        };
        Some((table, len, capacity))
    });
    let Some((current, len, capacity)) = state else {
        return Ok(());
    };
    if needed <= capacity {
        return Ok(());
    }
    let grown = needed.max(capacity.saturating_mul(2)).max(4);
    let object_slot = (object as *mut JsObject).cast::<otter_gc::raw::RawGc>();
    let pending_base = pending.as_mut_ptr();
    let pending_len = pending.len();
    let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        visitor(object_slot);
        for index in 0..pending_len {
            // SAFETY: `index < pending_len`, and the slice outlives this
            // call; rewrite the embedded moving offset in place.
            unsafe { (*pending_base.add(index)).trace_value_slot_mut(visitor) };
        }
    };
    // The old table is old space and does not move; copy after the
    // allocation, then swap the sidecar's handle.
    let existing: Vec<SlotMeta> = match slot_meta_body_of(current) {
        // SAFETY: live table payload.
        Some(body) => unsafe { (*body).entries().to_vec() },
        None => Vec::new(),
    };
    let _ = len;
    let table = slot_meta_table_from(heap, &existing, grown, &mut visit)?;
    let owner = *object;
    let sidecar = heap.read_payload(owner, |body| body.exotic.get());
    debug_assert!(!sidecar.is_null(), "materialized slots imply a sidecar");
    heap.with_payload(sidecar, |exotic| {
        exotic.slots = table;
        true
    });
    heap.record_write(sidecar, &table);
    Ok(())
}

/// Pre-build the slot-meta table a demotion is about to install.
///
/// The demoting payload borrow cannot allocate, so the table holding the
/// materialized metadata (plus room for the record the same borrow will
/// push) is built here, with `object` and the caller's pending words
/// rooted. `None` in, `None` out.
fn slot_meta_table_for_install(
    object: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    metas: &Option<Vec<SlotMeta>>,
    capacity: usize,
    pending: &mut [Value],
) -> Result<Option<SlotMetaHandle>, otter_gc::OutOfMemory> {
    let Some(metas) = metas else {
        return Ok(None);
    };
    let object_slot = (object as *mut JsObject).cast::<otter_gc::raw::RawGc>();
    let pending_base = pending.as_mut_ptr();
    let pending_len = pending.len();
    let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        visitor(object_slot);
        for index in 0..pending_len {
            // SAFETY: `index < pending_len`, and the slice outlives this
            // call; rewrite the embedded moving offset in place.
            unsafe { (*pending_base.add(index)).trace_value_slot_mut(visitor) };
        }
    };
    let table = slot_meta_table_from(heap, metas, capacity, &mut visit)?;
    Ok(Some(table))
}

/// Register GC layouts that object allocation paths may publish without going
/// through `GcHeap::alloc<T>`.
pub fn register_gc_traceables(heap: &mut otter_gc::GcHeap) {
    macro_rules! register {
        ($($body:ty,)*) => {$( heap.register_traceable::<$body>(); )*};
    }
    // Every GC body type this VM can allocate. Registering the whole set
    // up front rather than on first allocation is what lets a heap
    // describe objects it did not itself create — a restored image is
    // walked by type tag before anything has been allocated into it.
    // `every_allocated_type_tag_is_registered` fails if this list falls
    // behind the types a real build produces.
    heap.register_host_release::<crate::native_function::NativeFunctionBody>();
    heap.register_sever_restored::<ExoticSlots>();
    heap.register_sever_restored::<crate::array::ArrayExoticSlots>();
    heap.register_sever_restored::<crate::weak_refs::WeakRefBody>();
    heap.register_sever_restored::<crate::weak_refs::FinalizationRegistryBody>();
    register! {
        crate::array::ArrayBody,
        crate::array::elements::ElementSlabBody,
        crate::value_slab::ValueSlabBody,
        crate::bigint::gc_body::BigIntBody,
        crate::binary::array_buffer::LocalArrayBufferBodyGc,
        crate::binary::array_buffer::SharedArrayBufferBodyGc,
        crate::binary::data_view::DataViewBodyGc,
        crate::binary::typed_array::TypedArrayBodyGc,
        crate::bound_function::BoundFunctionBody,
        crate::class_constructor::ClassConstructorBody,
        crate::closure::JsClosureBody,
        crate::collections::MapBody,
        crate::collections::table::OrderedTableBody<crate::collections::MapEntry>,
        crate::collections::SetBody,
        crate::collections::table::OrderedTableBody<crate::collections::SetEntry>,
        crate::collections::WeakMapBody,
        crate::collections::WeakSetBody,
        crate::collections::weak_table::WeakTableBody<crate::collections::weak_table::MapKind>,
        crate::collections::weak_table::WeakTableBody<crate::collections::weak_table::SetKind>,
        crate::eval_env::EvalEnvBody,
        crate::generator::GeneratorBody,
        crate::generator::ParkedFrameBody,
        crate::intl::payload::IntlBody,
        crate::iterator_state::IteratorState,
        crate::native_function::NativeFunctionBody,
        crate::promise::PurePromiseBody,
        crate::proxy::ProxyBodyGc,
        crate::proxy::PrivateSlotsBody,
        crate::regexp::JsRegExpBody,
        crate::string::gc_body::JsStringBody,
        crate::symbol::SymbolBody,
        crate::temporal::payload::TemporalBody,
        crate::upvalue::UpvalueCellBody,
        crate::upvalue_spine::UpvalueSpineBody,
        ExoticSlots,
        SymbolPropsBody,
        SlotMetaBody,
        DictKeysBody,
        ErrorStackBody,
        crate::array::ArrayExoticSlots,
        crate::weak_refs::FinalizationRegistryBody,
        crate::weak_refs::WeakRefBody,
        AccessorCellBody,
        ObjectBody,
        shape_body::ShapeBody,
        slot_slab::SlotSlabBody,
    }
}

fn empty_object_body() -> ObjectBody {
    ObjectBody {
        shape: ShapeHandle::null(),
        values_ptr: Cell::new(std::ptr::null_mut()),
        slab: otter_gc::Gc::null(),
        inline_values: [Value::default(); INLINE_SLOT_CAP],
        slab_len: 0,
        dictionary_shape_id: ShapeId::UNASSIGNED,
        shape_cache_mode: ShapeCacheMode::Fast,
        jit_proto: otter_gc::Gc::null(),
        extensible: true,
        slot_attrs_overridden: false,
        exotic: ExoticSlot::null(),
    }
}

fn empty_object_body_with_shape(shape: ShapeHandle) -> ObjectBody {
    debug_assert_object_shape_handle(shape, "object allocation shape");
    let mut body = empty_object_body();
    debug_assert_object_shape_handle(shape, "shape-slot store");
    body.shape = shape;
    body
}

fn debug_assert_object_shape_handle(shape: ShapeHandle, context: &str) {
    if cfg!(debug_assertions) && !shape.is_null() {
        // SAFETY: debug-only invariant check; a non-null shape handle stored in
        // ObjectBody must always address a live ShapeBody cell.
        unsafe {
            debug_assert_eq!(
                (*shape.as_header_ptr()).type_tag(),
                shape_body::SHAPE_BODY_TYPE_TAG,
                "ObjectBody shape points at non-shape cell during {context}: shape={:?} tag={:#x} swept={}",
                shape,
                (*shape.as_header_ptr()).type_tag(),
                (*shape.as_header_ptr()).is_swept()
            );
        }
    }
}

fn empty_dictionary_object_body() -> ObjectBody {
    let mut body = empty_object_body();
    body.dictionary_shape_id = next_shape_id();
    body
}

/// Allocate an old-space object for raw GC fixtures.
///
/// Production VM allocation paths must use stack/runtime/native root contracts.
#[cfg(test)]
pub(crate) fn alloc_object_old_for_fixture(
    heap: &mut GcHeap,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    heap.alloc_old(empty_dictionary_object_body())
}

/// Allocate an empty object directly in non-moving old space.
///
/// For permanent singleton roots — the realm global object — that live for
/// the whole isolate. Pinning them in old space keeps every handle stable
/// across young scavenges and avoids copying a large, long-lived object on
/// every minor collection. The empty body holds no GC edges, so no caller
/// roots are required across the allocation.
pub(crate) fn alloc_object_old(heap: &mut GcHeap) -> Result<JsObject, otter_gc::OutOfMemory> {
    heap.alloc_old(empty_dictionary_object_body())
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
    heap.alloc_with_roots(empty_dictionary_object_body(), external_visit)
}

/// Allocate a fresh empty object with the root hidden class installed.
pub(crate) fn alloc_object_with_shape_roots(
    heap: &mut GcHeap,
    shape: ShapeHandle,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    heap.alloc_with_roots(empty_object_body_with_shape(shape), external_visit)
}

/// Allocate a fresh shaped object whose complete data-slot prefix is installed
/// before the object becomes reachable.
///
/// Inline values live directly in the young object body. Wider objects first
/// allocate an old-space slab with its collector-rewritten values already in
/// the trailing words, then publish the object that owns that slab. In either
/// case no empty property is exposed and no later per-property mutator store is
/// required.
pub(crate) fn alloc_object_with_shape_and_values_roots(
    heap: &mut GcHeap,
    mut shape: ShapeHandle,
    mut prototype: Option<JsObject>,
    values: &mut [Value],
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    debug_assert_eq!(
        shape_property_count(shape, heap) as usize,
        values.len(),
        "shape slot count and initialization values diverged"
    );

    let shape_slot = std::ptr::addr_of_mut!(shape).cast::<RawGc>();
    let prototype_slot = prototype
        .as_mut()
        .map(|prototype| std::ptr::from_mut(prototype).cast::<RawGc>());
    let mut visit_owner_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        visitor(shape_slot);
        if let Some(prototype_slot) = prototype_slot {
            visitor(prototype_slot);
        }
    };

    let slab = if values.len() > INLINE_SLOT_CAP {
        let capacity = values.len().max(INLINE_SLOT_CAP * 2);
        slot_slab::alloc_slot_slab_with_values(heap, capacity, values, &mut visit_owner_roots)?
    } else {
        slot_slab::SlotSlabHandle::null()
    };

    let mut inline_values = [Value::default(); INLINE_SLOT_CAP];
    if slab.is_null() {
        inline_values[..values.len()].copy_from_slice(values);
    }
    let body = ObjectBody {
        shape,
        values_ptr: Cell::new(std::ptr::null_mut()),
        slab,
        inline_values,
        slab_len: u16::try_from(values.len()).expect("object layout exceeds u16 slots"),
        dictionary_shape_id: ShapeId::UNASSIGNED,
        shape_cache_mode: ShapeCacheMode::Fast,
        jit_proto: prototype.unwrap_or_default(),
        extensible: true,
        slot_attrs_overridden: false,
        exotic: ExoticSlot::null(),
    };
    body.refresh_values_ptr();

    let slab_slot = (!slab.is_null()).then(|| std::ptr::addr_of!(slab).cast_mut().cast::<RawGc>());
    let values_base = values.as_mut_ptr();
    let values_len = values.len();
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        if let Some(slab_slot) = slab_slot {
            visitor(slab_slot);
        }
        for index in 0..values_len {
            // SAFETY: `index < values_len`; the caller-owned pending buffer
            // outlives this allocation and is rewritten in place.
            unsafe { (*values_base.add(index)).trace_value_slot_mut(visitor) };
        }
    };
    let object = heap.alloc_with_roots(body, &mut visit)?;
    // A young allocation is not post-scanned by the allocator because it
    // needs no remembered-set edges. Its cached inline base was copied from
    // the pending stack body, though, so retarget it to the final cell before
    // returning the first observable handle.
    heap.with_payload(object, |body| body.refresh_values_ptr());
    Ok(object)
}

/// Initialize a freshly allocated shaped object with the values for every
/// hidden-class data slot, in shape order.
pub(crate) fn initialize_shaped_data_slots(obj: JsObject, heap: &mut GcHeap, values: &[Value]) {
    initialize_shaped_data_slots_with_capacity(obj, heap, values, values.len());
}

/// Initialize the visible prefix of a freshly shaped object while reserving
/// room for later constructor-owned transitions.
///
/// Capacity is not observable: `slab_len` advances only for `values`, and each
/// later field becomes visible at its original `StoreProperty` operation.
pub(crate) fn initialize_shaped_data_slots_with_capacity(
    obj: JsObject,
    heap: &mut GcHeap,
    values: &[Value],
    capacity: usize,
) {
    let mut obj = obj;
    let stored = SmallVec::<[Value; 8]>::from_slice(values);
    let expected = heap.read_payload(obj, |body| body_property_count(heap, body));
    let mut stored = stored;
    if reserve_slot_capacity(&mut obj, heap, capacity.max(stored.len()), &mut stored).is_err() {
        return;
    }
    heap.with_payload(obj, |body| {
        debug_assert!(
            !body.shape.is_null(),
            "bulk slot init only applies to shaped objects"
        );
        debug_assert!(body.slab_len == 0, "bulk slot init requires a fresh object");
        debug_assert_eq!(
            expected,
            stored.len(),
            "shape slot count and init value count diverged"
        );
        for (index, &value) in stored.iter().enumerate() {
            body.push_slot(index, SlotMeta::data_default(), value);
        }
    });
    for &value in &stored {
        record_slot_write(heap, obj, value);
    }
}

/// Reserve an empty fresh object's hidden property slab without publishing a
/// property or changing its root hidden class.
pub(crate) fn reserve_fresh_object_slot_capacity(
    obj: &mut JsObject,
    heap: &mut GcHeap,
    capacity: usize,
) -> Result<(), otter_gc::OutOfMemory> {
    debug_assert_eq!(heap.read_payload(*obj, ObjectBody::slab_len), 0);
    reserve_slot_capacity(obj, heap, capacity, &mut [])
}

/// Replace the root hidden class on a fresh, slotless object before bulk
/// constructor initialization.
pub(crate) fn set_fresh_object_shape(obj: JsObject, heap: &mut GcHeap, shape: ShapeHandle) {
    heap.with_payload(obj, |body| {
        debug_assert!(
            body.slab_len == 0,
            "fast constructor shape install requires a fresh object"
        );
        debug_assert!(
            !shape.is_null(),
            "fast constructor shape install requires a shaped target"
        );
        debug_assert_object_shape_handle(shape, "fresh object shape install");
        debug_assert_object_shape_handle(shape, "shape-slot store");
        body.shape = shape;
    });
}

/// Try to allocate a fresh shaped object without running a GC safepoint.
pub(crate) fn try_alloc_object_with_shape_no_collect(
    heap: &mut GcHeap,
    shape: ShapeHandle,
) -> Option<JsObject> {
    heap.try_alloc_no_collect(empty_object_body_with_shape(shape))
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
    heap.alloc_old_diagnostic(empty_dictionary_object_body())
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
    let mut sidecar: ExoticHandle =
        heap.alloc_variable_with_roots(ExoticSlots::default(), 0, external_visit)?;
    let sidecar_slot = std::ptr::addr_of_mut!(sidecar);
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        visitor(sidecar_slot.cast::<RawGc>());
    };
    let mut slot = ExoticSlot::null();
    slot.set(sidecar);
    let object = heap.alloc_with_roots(
        ObjectBody {
            shape: ShapeHandle::null(),
            values_ptr: Cell::new(std::ptr::null_mut()),
            slab: otter_gc::Gc::null(),
            inline_values: [Value::default(); INLINE_SLOT_CAP],
            slab_len: 0,
            dictionary_shape_id: next_shape_id(),
            shape_cache_mode: ShapeCacheMode::Fast,
            jit_proto: otter_gc::Gc::null(),
            extensible: true,
            slot_attrs_overridden: false,
            exotic: slot,
        },
        &mut visit,
    )?;
    heap.with_payload(sidecar, |exotic| {
        exotic.host_data = Some(HostData::Untraced(Box::new(data)));
        true
    });
    heap.record_write(object, &sidecar);
    Ok(object)
}

/// Allocate a fresh host-data object with the root hidden class installed.
pub(crate) fn alloc_host_object_with_shape_roots<T: HostObjectData>(
    heap: &mut otter_gc::GcHeap,
    shape: ShapeHandle,
    data: T,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    // The sidecar is allocated before the object exists, so installing
    // the host payload needs no second allocation point inside a borrow.
    let mut sidecar: ExoticHandle =
        heap.alloc_variable_with_roots(ExoticSlots::default(), 0, external_visit)?;
    let sidecar_slot = std::ptr::addr_of_mut!(sidecar);
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        visitor(sidecar_slot.cast::<RawGc>());
    };
    let mut slot = ExoticSlot::null();
    slot.set(sidecar);
    let object = heap.alloc_with_roots(
        ObjectBody {
            shape,
            values_ptr: Cell::new(std::ptr::null_mut()),
            slab: otter_gc::Gc::null(),
            inline_values: [Value::default(); INLINE_SLOT_CAP],
            slab_len: 0,
            dictionary_shape_id: ShapeId::UNASSIGNED,
            shape_cache_mode: ShapeCacheMode::Fast,
            jit_proto: otter_gc::Gc::null(),
            extensible: true,
            slot_attrs_overridden: false,
            exotic: slot,
        },
        &mut visit,
    )?;
    heap.with_payload(sidecar, |exotic| {
        exotic.host_data = Some(HostData::Untraced(Box::new(data)));
        true
    });
    heap.record_write(object, &sidecar);
    Ok(object)
}

/// Allocate a fresh host object whose payload explicitly traces JavaScript
/// references through [`HostValueSlot`] fields.
pub(crate) fn alloc_traced_host_object_with_shape_roots<T: TracedHostObjectData>(
    heap: &mut otter_gc::GcHeap,
    shape: ShapeHandle,
    mut data: T,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    // The sidecar is allocated before the object exists, so installing
    // the host payload needs no second allocation point inside a borrow.
    let mut sidecar: ExoticHandle =
        heap.alloc_variable_with_roots(ExoticSlots::default(), 0, external_visit)?;
    let sidecar_slot = std::ptr::addr_of_mut!(sidecar);
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        visitor(sidecar_slot.cast::<RawGc>());
    };
    let mut slot = ExoticSlot::null();
    slot.set(sidecar);
    let object = heap.alloc_with_roots(
        ObjectBody {
            shape,
            values_ptr: Cell::new(std::ptr::null_mut()),
            slab: otter_gc::Gc::null(),
            inline_values: [Value::default(); INLINE_SLOT_CAP],
            slab_len: 0,
            dictionary_shape_id: ShapeId::UNASSIGNED,
            shape_cache_mode: ShapeCacheMode::Fast,
            jit_proto: otter_gc::Gc::null(),
            extensible: true,
            slot_attrs_overridden: false,
            exotic: slot,
        },
        &mut visit,
    )?;
    // The sidecar is an old-space body from birth, so the host slots just
    // installed never crossed the mutator write barrier. Record each
    // child edge now, or an old(sidecar)→young(child) reference is
    // missing from the remembered set and the first scavenge strands the
    // slot on the vacated from-space copy.
    let mut children: smallvec::SmallVec<[RawGc; 4]> = smallvec::SmallVec::new();
    {
        let mut collect = |slot: *mut RawGc| {
            // SAFETY: the tracer hands pointers to live slots inside `data`.
            let child = unsafe { *slot };
            if !child.is_null() {
                children.push(child);
            }
        };
        let mut tracer = HostDataTracer {
            visitor: &mut collect,
        };
        data.trace_gc_slots(&mut tracer);
    }
    heap.with_payload(sidecar, |exotic| {
        exotic.host_data = Some(HostData::Traced(Box::new(data)));
        true
    });
    heap.record_write(object, &sidecar);
    for child in children {
        heap.record_write_edge(sidecar, child);
    }
    Ok(object)
}

/// Mark an object as an ECMA-262 §10.4.4 arguments-exotic object so
/// reflective probes (`Object.prototype.toString.call(arguments)`)
/// emit the spec `"Arguments"` builtin tag per §20.1.3.6 step 14.b.
/// Called from `arguments_object::initialize_{mapped,unmapped}` after
/// the body's slot table is set up.
pub fn mark_as_arguments_object(obj: &mut JsObject, heap: &mut otter_gc::GcHeap) {
    // The sidecar allocation may move the object; the caller's handle
    // is updated in place.
    ensure_exotic(obj, heap).expect("exotic sidecar");
    heap.with_payload(*obj, |body| {
        body.exotic_mut().is_arguments_object = true;
    });
}

/// `true` when the object was tagged as an arguments-exotic body by
/// [`mark_as_arguments_object`]. Reads the body slot through the GC
/// `read_payload` accessor so callers do not have to expose
/// [`ObjectBody`]'s internals.
#[must_use]
pub fn is_arguments_object(obj: JsObject, heap: &otter_gc::GcHeap) -> bool {
    heap.read_payload(obj, |body| body.is_arguments_object())
}

/// Snapshot for the apply/spread argv fast path: the current shape and the
/// arity it encodes, provided the object is an arguments exotic with no
/// mapped parameter aliases and a live (non-null) shape holding the built
/// `argc + 2` slots.
#[must_use]
pub(crate) fn arguments_direct_snapshot(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
) -> Option<(ShapeHandle, usize)> {
    heap.read_payload(obj, |body| {
        if !body.is_arguments_object() || body.host_data_ref().is_some() {
            return None;
        }
        if body.shape.is_null() {
            return None;
        }
        let count = shape_body::shape_property_count(heap, body.shape) as usize;
        count.checked_sub(2).map(|argc| (body.shape, argc))
    })
}

pub(crate) fn install_mapped_arguments(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    entries: Vec<MappedArgumentEntry>,
) {
    // The sidecar allocates, so it is reserved here, outside the payload
    // borrow below. This may move `obj`, which is why the local is `mut`.
    let mut obj = obj;
    ensure_exotic(&mut obj, heap).expect("exotic sidecar");
    if entries.is_empty() {
        return;
    }
    let cells: Vec<UpvalueCell> = entries.iter().map(|entry| entry.cell).collect();
    heap.with_payload(obj, |body| {
        body.exotic_mut().host_data = Some(HostData::Untraced(Box::new(MappedArgumentsData {
            entries: entries.into_boxed_slice(),
        })));
    });
    // The parameter cells are held by the sidecar, which is its own old-space
    // body, so the edge a scavenge has to re-trace starts there and not at the
    // object: re-tracing the object finds one edge to an old child and stops
    // before it ever reaches the cells. Recording the object instead leaves a
    // young cell unevacuated and the sidecar holding its pre-move offset.
    let sidecar = heap.read_payload(obj, |body| body.exotic.get());
    for cell in cells {
        heap.record_write(sidecar, &cell);
    }
}

fn mapped_argument_cell(body: &ObjectBody, key: &str) -> Option<UpvalueCell> {
    body.host_data_ref()?
        .downcast_ref::<MappedArgumentsData>()?
        .entries
        .iter()
        .find(|entry| entry.key == key)
        .map(|entry| entry.cell)
}

fn remove_mapped_argument(body: &mut ObjectBody, key: &str) {
    let Some(data) = exotic_body_of(body.exotic.get()).and_then(|e|
        // SAFETY: a non-null handle names a live sidecar payload.
        unsafe { (*e).host_data.take() })
    else {
        return;
    };
    match data.into_untraced::<MappedArgumentsData>() {
        Ok(mapped) => {
            let retained: Vec<_> = mapped
                .entries
                .into_vec()
                .into_iter()
                .filter(|entry| entry.key != key)
                .collect();
            if !retained.is_empty() {
                body.exotic_mut().host_data =
                    Some(HostData::Untraced(Box::new(MappedArgumentsData {
                        entries: retained.into_boxed_slice(),
                    })));
            }
        }
        Err(other) => {
            body.exotic_mut().host_data = Some(other);
        }
    }
}

fn apply_mapped_arguments_partial_define(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    descriptor: PartialPropertyDescriptor,
    existing_offset: Option<u16>,
) {
    let mapped_cell = heap.read_payload(obj, |body| mapped_argument_cell(body, key));
    let Some(cell) = mapped_cell else {
        return;
    };

    // §10.4.4.2 steps 5-6 — consult the partial descriptor:
    // only a present [[Value]] writes through the map, and only an
    // accessor or an explicit writable:false unmaps. If
    // writable:false is present without [[Value]], the unmapped own
    // data property must first capture the current parameter value.
    if descriptor.is_accessor() {
        heap.with_payload(obj, |body| remove_mapped_argument(body, key));
        return;
    }

    if let Some(value) = descriptor.value {
        store_upvalue(heap, cell, value);
    }

    if descriptor.writable == Some(false) {
        if descriptor.value.is_none() {
            let current = read_upvalue(heap, cell);
            let stored = current;
            if let Some(offset) = existing_offset {
                let is_data_slot = heap.read_payload(obj, |body| {
                    (usize::from(offset) < body_property_count(heap, body))
                        && !body.slot_attrs(heap, offset as usize).1
                });
                if is_data_slot {
                    heap.with_payload(obj, |body| {
                        body.set_data_value(offset as usize, stored);
                    });
                    record_slot_write(heap, obj, stored);
                }
            }
        }
        heap.with_payload(obj, |body| remove_mapped_argument(body, key));
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

/// Read the own data value at flat slot index `slot` (inline or overflow
/// storage). The caller must guarantee `slot` indexes a live data slot under
/// the object's current shape — JSON.stringify's fast path obtains the index
/// from [`Properties::enumerable_string_data_offsets`] and re-validates the
/// shape id per key before calling, so a structural mutation can never make
/// this read a stale slot.
pub(crate) fn data_value_at(obj: JsObject, heap: &otter_gc::GcHeap, slot: u16) -> Value {
    heap.read_payload(obj, |body| {
        let i = slot as usize;
        if i < body_property_count(heap, body) && !body.slot_attrs(heap, i).1 {
            body.data_value(heap, i)
        } else {
            Value::undefined()
        }
    })
}

fn body_shape_id(heap: &otter_gc::GcHeap, body: &ObjectBody) -> ShapeId {
    if !body.shape.is_null() {
        return heap.read_payload(body.shape, shape_body::ShapeBody::id);
    }
    debug_assert_ne!(
        body.dictionary_shape_id,
        ShapeId::UNASSIGNED,
        "dictionary-mode object needs assigned shape id"
    );
    body.dictionary_shape_id
}

fn body_property_count(heap: &otter_gc::GcHeap, body: &ObjectBody) -> usize {
    if !body.shape.is_null() {
        return shape_body::shape_property_count(heap, body.shape) as usize;
    }
    body.dict_key_count()
}

pub(super) fn body_offset_of(heap: &otter_gc::GcHeap, body: &ObjectBody, key: &str) -> Option<u16> {
    if !body.shape.is_null() {
        debug_assert_object_shape_handle(body.shape, "property offset lookup");
        return shape_body::shape_offset_of_str(heap, body.shape, key)
            .and_then(|offset| u16::try_from(offset).ok());
    }
    // O(1) dictionary lookup via the maintained index — a linear scan
    // here makes bulk property addition O(n²).
    body.dictionary_index_get(key)
}

/// [`body_offset_of`] for an atomized key: a shaped object answers with one
/// `u32` compare per shape-chain link and never touches a heap string.
/// Dictionary storage keys by spelling, so it still hashes the name.
pub(super) fn body_offset_of_atom(
    heap: &otter_gc::GcHeap,
    body: &ObjectBody,
    key: AtomizedPropertyKey<'_>,
) -> Option<u16> {
    if !body.shape.is_null() {
        debug_assert_object_shape_handle(body.shape, "property offset lookup");
        return shape_body::shape_offset_of_atom(heap, body.shape, key.atom().id())
            .and_then(|offset| u16::try_from(offset).ok());
    }
    body.dictionary_index_get(key.name())
}

/// Number of own string-keyed properties recorded in a fast-mode
/// shape (`0` for the null/dictionary shape). Used to decide when an
/// object should normalize to dictionary storage.
pub(crate) fn shape_property_count(shape: ShapeHandle, heap: &otter_gc::GcHeap) -> u32 {
    if shape.is_null() {
        0
    } else {
        shape_body::shape_property_count(heap, shape)
    }
}

/// Maximum number of own properties an object keeps in fast
/// transition-shape storage before it normalizes to dictionary mode.
/// Beyond this, growing the shape transition chain makes property
/// lookup O(n) (and bulk addition O(n²)); dictionary mode keeps both
/// O(1). Mirrors the fast-property cap used by production engines.
pub(crate) const MAX_FAST_PROPERTIES: u32 = 128;

/// Append a dictionary key. The caller pushes the matching slot
/// separately; the new offset is the pre-push length (slots and keys
/// stay aligned), and the caller must have reserved room in the key
/// table — growth allocates, and a payload borrow has no heap.
pub(super) fn dict_push_key(body: &mut ObjectBody, key: String) {
    let exotic = body.exotic_mut();
    let table = dict_keys_body_of(exotic.dictionary_keys)
        .expect("dictionary key pushed without a reserved table");
    // SAFETY: a non-null handle names a live table; the sidecar borrow
    // rules out an aliasing read.
    unsafe { (*table).push(&key) };
}

/// Clear all dictionary keys and the index together.
#[cfg(test)]
pub(super) fn dict_clear_keys(body: &mut ObjectBody) {
    if let Some(exotic) = exotic_body_of(body.exotic.get()).map(|e|
            // SAFETY: a non-null handle names a live sidecar payload.
            unsafe { &mut *e })
        && let Some(table) = dict_keys_body_of(exotic.dictionary_keys)
    {
        // SAFETY: a non-null handle names a live table.
        unsafe { (*table).clear() };
    }
}

fn body_has_key_at(heap: &otter_gc::GcHeap, body: &ObjectBody, offset: usize) -> bool {
    if !body.shape.is_null() {
        return u32::try_from(offset)
            .ok()
            .and_then(|offset| shape_body::shape_key_at_offset(heap, body.shape, offset))
            .is_some();
    }
    body.dict_keys().is_some_and(|table| offset < table.len())
}

fn body_key_matches(heap: &otter_gc::GcHeap, body: &ObjectBody, offset: usize, key: &str) -> bool {
    if !body.shape.is_null() {
        return u32::try_from(offset).ok().is_some_and(|offset| {
            shape_body::shape_key_matches_str(heap, body.shape, offset, key)
        });
    }
    body.dict_keys()
        .is_some_and(|table| offset < table.len() && table.key_at(offset) == key)
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
        body_offset_of(heap, body, key).map(|offset| {
            let i = offset as usize;
            if !body.slot_attrs(heap, i).1 {
                body.data_value(heap, i)
            } else {
                Value::undefined()
            }
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
        PropertyLookup::Accessor { .. } => Some(Value::undefined()),
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
            let mut lookup = body.slot_lookup_at(heap, offset as usize);
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
            let mut lookup = body.slot_lookup_at(heap, offset as usize);
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

/// Read the data value at a known own-slot offset without re-resolving the
/// key. Returns `None` when the offset is an accessor or out of range. Callers
/// must first confirm the object's [`shape_id`] still matches the one the slot
/// offset was captured under, so the offset still names the same key.
#[must_use]
pub(crate) fn data_slot_value_at(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    slot: u16,
) -> Option<Value> {
    heap.read_payload(obj, |body| {
        if slot as usize >= body_property_count(heap, body) {
            return None;
        }
        match body.slot_lookup_at(heap, slot as usize) {
            PropertyLookup::Data { value, .. } => Some(value),
            _ => None,
        }
    })
}

/// Atom-aware own-property probe for named property bytecodes.
#[must_use]
pub(crate) fn lookup_own_atom(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
) -> AtomPropertyLookup {
    heap.read_payload(obj, |body| match body_offset_of_atom(heap, body, key) {
        Some(offset) => {
            let mut lookup = body.slot_lookup_at(heap, offset as usize);
            if let Some(cell) = mapped_argument_cell(body, key.name())
                && let PropertyLookup::Data { value, .. } = &mut lookup
            {
                *value = read_upvalue(heap, cell);
            }
            AtomPropertyLookup {
                hit: Some(AtomOwnPropertyHit {
                    shape_id: body_shape_id(heap, body),
                    shape: body.shape,
                    atom_id: key.atom().id(),
                    slot: offset,
                    is_data: matches!(lookup, PropertyLookup::Data { .. }),
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
        // A shaped object's shape handle is interned and immortal, so a handle
        // match proves identical layout with a single offset compare — no
        // deref into the shape body to read its id, and no key compare. The
        // handle also fixes the slot, so the cached `slot` is valid. Dictionary
        // mode (null handle) reuses a per-object shape id that does not bump on
        // every slot mutation, so it still confirms the id and the key by name.
        let shaped = !body.shape.is_null();
        let shape_ok = if shaped {
            body.shape == hit.shape
        } else {
            body_shape_id(heap, body) == hit.shape_id
        };
        if !shape_ok || key.atom().id() != hit.atom_id {
            return None;
        }
        let offset = hit.slot as usize;
        if !shaped && !body_key_matches(heap, body, offset, key.name()) {
            return None;
        }
        debug_assert!(
            body_key_matches(heap, body, offset, key.name()),
            "shape-id hit resolved to a slot whose key differs from the request"
        );
        // Fast path: an ordinary shaped object (no exotic slots, so no mapped
        // arguments) whose attributes have not been overridden in place reads a
        // baked data slot straight from the slab — the matching shape fixes the
        // slot kind and bounds, so neither the per-slot attributes nor the
        // property count need consulting.
        //
        // Accessor-ness is part of the shape, and `hit.is_data` was recorded
        // against this very shape handle, so a matched shape with unoverridden
        // attributes cannot have turned the slot into an accessor. Asserting
        // that keeps the release hit off the shape body entirely.
        if shaped && hit.is_data && !body.slot_attrs_overridden && body.exotic.is_null() {
            debug_assert!(
                !body.slot_attrs(heap, offset).1,
                "shape-matched data hit resolved to an accessor slot"
            );
            debug_assert!(offset < body_property_count(heap, body));
            return Some(body.data_value(heap, offset));
        }
        if let Some(cell) = mapped_argument_cell(body, key.name()) {
            return Some(read_upvalue(heap, cell));
        }
        if offset >= body_property_count(heap, body) {
            return None;
        }
        if !body.slot_attrs(heap, offset).1 {
            Some(body.data_value(heap, offset))
        } else {
            None
        }
    })
}

/// Read a cached own data slot guarded by shape identity alone.
///
/// A shape-handle match fixes both the slot and its key, so the atom compare
/// [`load_own_data_slot_atom`] performs is redundant here. Used by the
/// monomorphic method-call IC, whose cached `hit` was recorded against this same
/// shape: the hot path is a single offset compare plus a slab read, with no atom
/// resolution and no stub walk. Only the shaped, non-overridden, non-exotic data
/// fast path is served; anything else returns `None` so the caller falls back to
/// full method resolution.
pub(crate) fn load_own_data_slot_by_shape(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    hit: AtomOwnPropertyHit,
) -> Option<Value> {
    heap.read_payload(obj, |body| {
        if body.shape.is_null()
            || body.shape != hit.shape
            || !hit.is_data
            || body.slot_attrs_overridden
            || !body.exotic.is_null()
        {
            return None;
        }
        let offset = hit.slot as usize;
        debug_assert!(offset < body_property_count(heap, body));
        debug_assert!(
            !body.slot_attrs(heap, offset).1,
            "shape-matched data hit resolved to an accessor slot"
        );
        Some(body.data_value(heap, offset))
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
    // Validate before compressing. Boxing a double or wide int32 may scavenge
    // and relocate `obj`; a failed IC probe cannot communicate that relocated
    // handle to its caller, which still owns the canonical rooted slot. A miss
    // must therefore remain allocation-free so fallback can safely re-read or
    // reuse the caller's receiver.
    let guard_matches = heap.read_payload(obj, |body| {
        let offset = hit.slot as usize;
        let shape_ok = if !body.shape.is_null() {
            body.shape == hit.shape
        } else {
            body_shape_id(heap, body) == hit.shape_id
        };
        let key_matches = !body.shape.is_null() || body_key_matches(heap, body, offset, key.name());
        let slot_attrs =
            (offset < body_property_count(heap, body)).then(|| body.slot_attrs(heap, offset));
        shape_ok
            && key.atom().id() == hit.atom_id
            && key_matches
            && slot_attrs.is_some_and(|(flags, is_accessor)| flags.writable() && !is_accessor)
    });
    if !guard_matches {
        return None;
    }
    debug_assert!(
        heap.read_payload(obj, |body| body_key_matches(
            heap,
            body,
            hit.slot as usize,
            key.name()
        )),
        "shape-id store hit resolved to a slot whose key differs from the request"
    );

    let stored = *value;
    let mapped_cell = heap.read_payload(obj, |body| mapped_argument_cell(body, key.name()));
    heap.with_payload(obj, |body| {
        let offset = hit.slot as usize;
        body.set_data_value(offset, stored);
    });
    if let Some(cell) = mapped_cell {
        store_upvalue(heap, cell, *value);
    }
    record_slot_write(heap, obj, stored);
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
        if offset >= body_property_count(heap, body) {
            return false;
        }
        let (flags, is_accessor) = body.slot_attrs(heap, offset);
        flags.writable() && !is_accessor
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
            let mut descriptor = body.slot_descriptor_at(heap, offset as usize);
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
    heap.read_payload(obj, |body| match &body.prototype() {
        ObjectPrototype::Object(proto) => Some(*proto),
        ObjectPrototype::Null | ObjectPrototype::Value(_) | ObjectPrototype::Proxy(_) => None,
    })
}

/// Borrow the current prototype as a JS value, if any.
#[must_use]
pub fn prototype_value(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<Value> {
    heap.read_payload(obj, |body| body.prototype().as_value())
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
pub fn get_jsstring(obj: JsObject, heap: &otter_gc::GcHeap, key: JsString) -> Option<Value> {
    let utf8 = key.to_lossy_string(heap);
    get(obj, heap, &utf8)
}

/// Look up an **own** symbol-keyed property.
#[must_use]
pub fn get_own_symbol(obj: JsObject, heap: &otter_gc::GcHeap, key: JsSymbol) -> Option<Value> {
    heap.read_payload(obj, |body| {
        body.symbol_props()
            .iter()
            .find(|(k, _)| k.ptr_eq(key))
            .map(|(_, slot)| {
                if slot.kind.is_data() {
                    slot.value
                } else {
                    Value::undefined()
                }
            })
    })
}

/// Probe for an **own** symbol-keyed property descriptor body.
#[must_use]
pub fn lookup_own_symbol(obj: JsObject, heap: &otter_gc::GcHeap, key: JsSymbol) -> PropertyLookup {
    heap.read_payload(obj, |body| {
        body.symbol_props()
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
pub fn has_own_symbol(obj: JsObject, heap: &otter_gc::GcHeap, key: JsSymbol) -> bool {
    !matches!(lookup_own_symbol(obj, heap, key), PropertyLookup::Absent)
}

/// Look up a symbol-keyed property with prototype-chain walk.
#[must_use]
pub fn get_symbol(obj: JsObject, heap: &otter_gc::GcHeap, key: JsSymbol) -> Option<Value> {
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
pub fn lookup_symbol(obj: JsObject, heap: &otter_gc::GcHeap, key: JsSymbol) -> PropertyLookup {
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
    key: JsSymbol,
) -> Option<PropertyDescriptor> {
    heap.read_payload(obj, |body| {
        body.symbol_props()
            .iter()
            .find(|(k, _)| k.ptr_eq(key))
            .map(|(_, slot)| slot.to_descriptor())
    })
}

/// Store the internal native `[[Call]]` slot for callable ordinary
/// objects.
pub fn set_call_native(obj: &mut JsObject, heap: &mut otter_gc::GcHeap, native: Value) {
    // The sidecar allocation may move both the object and the value;
    // the caller's handle is updated in place and the value rides the
    // pending-root list.
    let mut pending = [native];
    ensure_exotic_with_pending_values(obj, heap, &mut pending).expect("exotic sidecar");
    let native = pending[0];
    heap.with_payload(*obj, |body| {
        body.exotic_mut().call_native = Some(native);
    });
    record_exotic_write(heap, *obj, &native);
}

/// Read the internal native `[[Call]]` slot.
#[must_use]
pub fn call_native(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<Value> {
    heap.read_payload(obj, |body| body.call_native())
}

/// Store the internal native `[[Construct]]` slot for constructor-shaped
/// builtin objects. Current builtin constructor objects are callable
/// too, so this also installs the same callback as `[[Call]]`.
pub fn set_constructor_native(obj: &mut JsObject, heap: &mut otter_gc::GcHeap, native: Value) {
    // The sidecar allocation may move both the object and the value;
    // the caller's handle is updated in place and the value rides the
    // pending-root list.
    let mut pending = [native];
    ensure_exotic_with_pending_values(obj, heap, &mut pending).expect("exotic sidecar");
    let native = pending[0];
    heap.with_payload(*obj, |body| {
        body.exotic_mut().call_native = Some(native);
        body.exotic_mut().constructor_native = Some(native);
    });
    record_exotic_write(heap, *obj, &native);
}

/// Read the internal native `[[Construct]]` slot.
#[must_use]
pub fn constructor_native(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<Value> {
    heap.read_payload(obj, |body| body.constructor_native())
}

/// Store the `[[BooleanData]]` internal slot for a Boolean wrapper.
pub fn set_boolean_data(obj: &mut JsObject, heap: &mut otter_gc::GcHeap, value: bool) {
    // The sidecar allocation may move the object; the caller's handle
    // is updated in place.
    ensure_exotic(obj, heap).expect("exotic sidecar");
    heap.with_payload(*obj, |body| {
        body.exotic_mut().boolean_data = Some(value);
    });
}

/// Read the `[[BooleanData]]` internal slot for a Boolean wrapper.
#[must_use]
pub fn boolean_data(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<bool> {
    heap.read_payload(obj, |body| body.boolean_data())
}

/// Store the `[[NumberData]]` internal slot for a Number wrapper.
pub fn set_number_data(obj: &mut JsObject, heap: &mut otter_gc::GcHeap, value: NumberValue) {
    // The sidecar allocation may move the object; the caller's handle
    // is updated in place.
    ensure_exotic(obj, heap).expect("exotic sidecar");
    heap.with_payload(*obj, |body| {
        body.exotic_mut().number_data = Some(value);
    });
}

/// Read the `[[NumberData]]` internal slot for a Number wrapper.
#[must_use]
pub fn number_data(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<NumberValue> {
    heap.read_payload(obj, |body| body.number_data())
}

/// Store the `[[StringData]]` internal slot for a String wrapper.
pub fn set_string_data(obj: &mut JsObject, heap: &mut otter_gc::GcHeap, value: JsString) {
    // The sidecar allocation may move both the object and the string;
    // the caller's handle is updated in place and the string rides the
    // pending-root list.
    let mut pending = [Value::string(value)];
    ensure_exotic_with_pending_values(obj, heap, &mut pending).expect("exotic sidecar");
    let value = pending[0]
        .as_string(heap)
        .expect("pending string survives rooting");
    heap.with_payload(*obj, |body| {
        body.exotic_mut().string_data = Some(value);
    });
    heap.record_write(*obj, &value);
}

/// Read the `[[StringData]]` internal slot for a String wrapper.
#[must_use]
pub fn string_data(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<JsString> {
    heap.read_payload(obj, |body| body.string_data())
}

/// Store the `[[SymbolData]]` internal slot for a Symbol wrapper.
pub fn set_symbol_data(
    obj: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    value: crate::symbol::JsSymbol,
) {
    // The sidecar allocation may move both the object and the symbol;
    // the caller's handle is updated in place and the symbol rides the
    // pending-root list.
    let mut pending = [Value::symbol(value)];
    ensure_exotic_with_pending_values(obj, heap, &mut pending).expect("exotic sidecar");
    let value = pending[0]
        .as_symbol(heap)
        .expect("pending symbol survives rooting");
    heap.with_payload(*obj, |body| {
        body.exotic_mut().symbol_data = Some(value);
    });
    heap.record_write(*obj, &value);
}

/// Read the `[[SymbolData]]` internal slot for a Symbol wrapper.
#[must_use]
pub fn symbol_data(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<crate::symbol::JsSymbol> {
    heap.read_payload(obj, |body| body.symbol_data())
}

/// Store the `[[BigIntData]]` internal slot for a BigInt wrapper.
pub fn set_bigint_data(obj: &mut JsObject, heap: &mut otter_gc::GcHeap, value: BigIntValue) {
    // The sidecar allocation may move both the object and the bigint;
    // the caller's handle is updated in place and the bigint rides the
    // pending-root list.
    let mut pending = [Value::big_int(value)];
    ensure_exotic_with_pending_values(obj, heap, &mut pending).expect("exotic sidecar");
    let value = pending[0]
        .as_big_int()
        .expect("pending bigint survives rooting");
    heap.with_payload(*obj, |body| {
        body.exotic_mut().bigint_data = Some(value);
    });
}

/// Read the `[[BigIntData]]` internal slot for a BigInt wrapper.
#[must_use]
pub fn bigint_data(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<BigIntValue> {
    heap.read_payload(obj, |body| body.bigint_data())
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
        let clipped = ms.trunc();
        if clipped == 0.0 { 0.0 } else { clipped }
    }
}

/// Store the `[[DateValue]]` internal slot for a Date instance.
/// Applies §21.4.1.6 TimeClip before writing.
pub fn set_date_data(obj: &mut JsObject, heap: &mut otter_gc::GcHeap, value: f64) {
    // The sidecar allocates and may move the heap; the caller's handle
    // is updated in place so it keeps naming the live object (a copied
    // local would hand a stale cage offset back to the caller — the
    // classic recycled-slot brand loss).
    ensure_exotic(obj, heap).expect("exotic sidecar");
    let clipped = clip_date_value(value);
    heap.with_payload(*obj, |body| {
        body.exotic_mut().date_data = Some(clipped);
    });
}

/// Read the `[[DateValue]]` internal slot for a Date instance.
/// Returns `None` for non-Date objects so callers can detect a
/// receiver-brand mismatch (§21.4.1.1 `thisTimeValue` step 3).
#[must_use]
pub fn date_data(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<f64> {
    heap.read_payload(obj, |body| body.date_data())
}

/// Mark an object as carrying the `[[ErrorData]]` internal slot
/// (§20.5) — set when an error constructor produces the instance.
pub fn set_error_data(obj: &mut JsObject, heap: &mut otter_gc::GcHeap) {
    // The sidecar allocation may move the object; the caller's handle
    // is updated in place.
    ensure_exotic(obj, heap).expect("exotic sidecar");
    heap.with_payload(*obj, |body| {
        body.exotic_mut().error_data = true;
    });
}

/// `true` when the object has the `[[ErrorData]]` internal slot. Unlike
/// a prototype-chain probe this is exact: `Object.create(Error.prototype)`
/// returns `false`.
#[must_use]
pub fn has_error_data(obj: JsObject, heap: &otter_gc::GcHeap) -> bool {
    heap.read_payload(obj, |body| body.error_data())
}

/// Record the captured JS call-stack frames for an error object
/// (top-of-stack first). Replaces any previously captured frames, as
/// `Error.captureStackTrace` may re-capture onto an existing target.
pub fn set_error_stack_frames(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    frames: Vec<crate::run_control::StackFrameSnapshot>,
) {
    // The sidecar and the frame body allocate, so both happen here,
    // outside the payload borrow below. This may move `obj`, which is
    // why the local is `mut`. The frames are plain owned data — no
    // rooting needed beyond the receiver.
    let mut obj = obj;
    ensure_exotic(&mut obj, heap).expect("exotic sidecar");
    let bytes: usize = frames
        .iter()
        .map(|f| f.function_name.len() + f.module.len())
        .sum();
    let object_slot = std::ptr::addr_of_mut!(obj);
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        visitor(object_slot.cast::<RawGc>());
    };
    let Ok(stack) = heap.alloc_variable_with_roots::<ErrorStackBody>(
        ErrorStackBody {
            frame_count: frames.len() as u32,
            byte_len: bytes as u32,
        },
        ErrorStackBody::trailing_bytes(frames.len(), bytes),
        &mut visit,
    ) else {
        // Out of memory capturing a stack leaves the error without one;
        // the throw itself still proceeds.
        return;
    };
    // SAFETY: the handle names the body just allocated; capacity covers
    // every record and byte written below.
    unsafe {
        let body = error_stack_body_of(stack).expect("fresh stack body");
        let mut offset: u32 = 0;
        for (i, frame) in frames.iter().enumerate() {
            let name_offset = offset;
            std::ptr::copy_nonoverlapping(
                frame.function_name.as_ptr(),
                (*body).bytes_ptr().add(offset as usize),
                frame.function_name.len(),
            );
            offset += frame.function_name.len() as u32;
            let module_offset = offset;
            std::ptr::copy_nonoverlapping(
                frame.module.as_ptr(),
                (*body).bytes_ptr().add(offset as usize),
                frame.module.len(),
            );
            offset += frame.module.len() as u32;
            (*body).records_ptr().add(i).write(ErrorFrameRecord {
                function_id: frame.function_id,
                name_offset,
                name_len: frame.function_name.len() as u32,
                module_offset,
                module_len: frame.module.len() as u32,
                span_lo: frame.span.0,
                span_hi: frame.span.1,
            });
        }
    }
    let owner = obj;
    heap.with_payload(owner, |body| {
        body.exotic_mut().error_stack_frames = stack;
    });
    let sidecar = heap.read_payload(owner, |body| body.exotic.get());
    heap.record_write(sidecar, &stack);
}

/// Read a clone of the captured stack frames, if any were recorded.
#[must_use]
pub fn error_stack_frames(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
) -> Option<Vec<crate::run_control::StackFrameSnapshot>> {
    heap.read_payload(obj, |body| {
        body.exotic()
            .and_then(|e| error_stack_body_of(e.error_stack_frames))
            // SAFETY: a non-null handle names a live body.
            .map(|stack| unsafe { (*stack).to_frames() })
    })
}

/// `true` when the object carries captured stack frames.
#[must_use]
pub fn has_error_stack_frames(obj: JsObject, heap: &otter_gc::GcHeap) -> bool {
    heap.read_payload(obj, |body| body.has_error_stack_frames())
}

/// Tag an object as carrying the `[[IsRawJSON]]` internal slot
/// (§25.5.3 `JSON.rawJSON`).
pub fn set_is_raw_json(obj: &mut JsObject, heap: &mut otter_gc::GcHeap, value: bool) {
    // The sidecar allocation may move the object; the caller's handle
    // is updated in place.
    ensure_exotic(obj, heap).expect("exotic sidecar");
    heap.with_payload(*obj, |body| {
        body.exotic_mut().is_raw_json = value;
    });
}

/// `true` when `obj` carries the `[[IsRawJSON]]` internal slot.
#[must_use]
pub fn is_raw_json(obj: JsObject, heap: &otter_gc::GcHeap) -> bool {
    heap.read_payload(obj, |body| body.is_raw_json())
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
    T: Any,
{
    heap.read_payload(obj, |body| {
        let data = body.host_data_ref().ok_or(HostObjectError::Missing)?;
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
    T: Any,
{
    heap.with_payload(obj, |body| {
        let data = body.host_data_mut_opt().ok_or(HostObjectError::Missing)?;
        let typed = data
            .downcast_mut::<T>()
            .ok_or_else(|| HostObjectError::TypeMismatch {
                expected: std::any::type_name::<T>(),
                found: "<unknown host data>",
            })?;
        Ok(f(typed))
    })
}

/// Side data marking a *deferred* module namespace exotic object
/// (TC39 import defer). The object carries `@@toStringTag` from
/// creation; its export data properties are installed lazily by
/// "populating" it the first time a triggering access evaluates the
/// wrapped module identified by `target_url`.
#[derive(Debug)]
pub(crate) struct DeferredNamespaceData {
    pub(crate) target_url: std::sync::Arc<str>,
    /// `true` once the module has been evaluated and export properties
    /// installed; the object then behaves as an ordinary frozen-shaped
    /// namespace.
    pub(crate) populated: std::cell::Cell<bool>,
}

impl HostObjectData for DeferredNamespaceData {}

/// Side data marking a Module Namespace Exotic Object (ECMA-262
/// §10.4.6). The object is a thin exotic view over the wrapped module
/// environment `env` (an ordinary object that holds the live export
/// values): property reads resolve through `env` so the namespace
/// reflects late and cyclic writes, while writes / defines / deletes
/// fail and the key set is the env's exported names (sorted) plus the
/// namespace's own symbol keys (`@@toStringTag`).
#[derive(Debug)]
pub(crate) struct ModuleNamespaceData {
    /// The module's own environment object. Kept for GC reachability
    /// and as the fallback key source for unmodeled (host/builtin)
    /// modules that carry no ResolveExport table.
    env: HostValueSlot,
    /// Canonical URL of the module this namespace exposes. Used to look
    /// up the module's §16.2.1.6 ResolveExport table so re-exported and
    /// star-exported names resolve to the defining module's live env.
    pub(crate) module_url: std::sync::Arc<str>,
}

impl ModuleNamespaceData {
    pub(crate) fn new(env: JsObject, module_url: std::sync::Arc<str>) -> Self {
        Self {
            env: HostValueSlot::from_value(Value::object(env)),
            module_url,
        }
    }
}

impl TracedHostObjectData for ModuleNamespaceData {
    fn trace_gc_slots(&mut self, tracer: &mut HostDataTracer<'_>) {
        tracer.trace(&mut self.env);
    }

    fn visit_function_ids(&self, tracer: &mut HostCodeLivenessTracer<'_>) {
        tracer.trace(&self.env);
    }
}

/// Wrapped module environment when `obj` is a Module Namespace Exotic
/// Object, else `None`.
#[must_use]
pub(crate) fn module_namespace_env(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<JsObject> {
    heap.read_payload(obj, |body| {
        body.host_data_ref()
            .and_then(|d| d.downcast_ref::<ModuleNamespaceData>())
            .and_then(|d| d.env.value().as_object())
    })
}

/// Canonical module URL when `obj` is a Module Namespace Exotic Object,
/// else `None`.
#[must_use]
pub(crate) fn module_namespace_url(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
) -> Option<std::sync::Arc<str>> {
    heap.read_payload(obj, |body| {
        body.host_data_ref()
            .and_then(|d| d.downcast_ref::<ModuleNamespaceData>())
            .map(|d| d.module_url.clone())
    })
}

/// Exported string keys of a module namespace's environment, sorted in
/// ascending code-unit order per §10.4.6.13 \[\[OwnPropertyKeys]].
#[must_use]
pub(crate) fn module_namespace_sorted_string_keys(
    env: JsObject,
    heap: &otter_gc::GcHeap,
) -> Vec<String> {
    let mut names: Vec<String> = with_properties(env, heap, |p| {
        p.enumerable_keys().map(str::to_string).collect()
    });
    names.sort_unstable();
    names
}

/// Target module URL when `obj` is a deferred module namespace, else
/// `None`.
#[must_use]
pub(crate) fn deferred_namespace_target(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
) -> Option<std::sync::Arc<str>> {
    heap.read_payload(obj, |body| {
        body.host_data_ref()
            .and_then(|d| d.downcast_ref::<DeferredNamespaceData>())
            .map(|d| d.target_url.clone())
    })
}

/// `true` when `obj` is a deferred namespace whose module has been
/// evaluated and export properties installed.
#[must_use]
pub(crate) fn deferred_namespace_is_populated(obj: JsObject, heap: &otter_gc::GcHeap) -> bool {
    heap.read_payload(obj, |body| {
        body.host_data_ref()
            .and_then(|d| d.downcast_ref::<DeferredNamespaceData>())
            .is_some_and(|d| d.populated.get())
    })
}

/// Mark a deferred namespace as populated.
pub(crate) fn set_deferred_namespace_populated(obj: JsObject, heap: &otter_gc::GcHeap) {
    heap.read_payload(obj, |body| {
        if let Some(d) = body
            .host_data_ref()
            .and_then(|d| d.downcast_ref::<DeferredNamespaceData>())
        {
            d.populated.set(true);
        }
    });
}

/// Borrow the GC-managed hidden class, if installed.
#[must_use]
pub(crate) fn shape(obj: JsObject, heap: &otter_gc::GcHeap) -> ShapeHandle {
    heap.read_payload(obj, |body| body.shape)
}

/// Invariant check after a shape-advancing append: the hidden class must
/// record `(flags, is_accessor)` for the freshly appended slot at the new last
/// offset. A shaped object carries no per-slot metadata of its own, so the
/// shape is the sole attribute source and must own that offset. Dictionary-mode
/// objects (null shape) are skipped. Debug-only.
#[cfg(debug_assertions)]
pub(crate) fn debug_assert_appended_shape_slot(obj: JsObject, heap: &otter_gc::GcHeap) {
    let shape = shape(obj, heap);
    if shape.is_null() {
        return;
    }
    heap.read_payload(obj, |body| {
        let Some(i) = body_property_count(heap, body).checked_sub(1) else {
            return;
        };
        if shape_body::shape_slot_attrs(heap, shape, i as u32).is_none() {
            panic!("shape missing attrs for appended slot {i}");
        }
    });
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
        if !(0..body_property_count(heap, body)).all(|i| !body.slot_attrs(heap, i).0.configurable())
        {
            return false;
        }
        // §7.3.16 — symbol-keyed own properties count too (Private
        // Name carriers are not properties).
        body.symbol_props()
            .iter()
            .all(|(key, slot)| key.is_private_name() || !slot.flags.configurable())
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
        for i in 0..body_property_count(heap, body) {
            let (flags, is_accessor) = body.slot_attrs(heap, i);
            if flags.configurable() {
                return false;
            }
            if !is_accessor && flags.writable() {
                return false;
            }
        }
        // §7.3.16 — symbol-keyed own properties count too (Private
        // Name carriers are not properties).
        body.symbol_props().iter().all(|(key, slot)| {
            key.is_private_name()
                || (!slot.flags.configurable() && (!slot.kind.is_data() || !slot.flags.writable()))
        })
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
/// Fire the generational/incremental barrier for the value just stored in an
/// object slot. Immediate values expose no outgoing edge through `GcStore`.
/// Append `value` as the object's slot `index`, for a bulk builder that has
/// already installed the whole hidden class and reserved the slab.
///
/// No presence lookup and no shape work: the layout the caller installed
/// already says this slot exists and which name it answers to.
pub(crate) fn push_layout_slot(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    index: usize,
    value: Value,
) {
    heap.with_payload(obj, |body| {
        body.push_slot(index, SlotMeta::data_default(), value);
    });
    record_slot_write(heap, obj, value);
}

/// Overwrite slot `index` of an object whose hidden class already names it.
pub(crate) fn write_layout_slot(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    index: usize,
    value: Value,
) {
    heap.with_payload(obj, |body| body.set_data_value(index, value));
    record_slot_write(heap, obj, value);
}

/// Read slot `index` of an object whose hidden class already names it.
pub(crate) fn layout_slot(obj: JsObject, heap: &otter_gc::GcHeap, index: usize) -> Value {
    heap.read_payload(obj, |body| body.data_value(heap, index))
}

fn record_slot_write(heap: &mut otter_gc::GcHeap, obj: JsObject, slot: Value) {
    heap.record_write(obj, &slot);
}

/// Records the GC store when `value` carries a `Gc<…>` handle so the
/// marker / scavenger see the new edge.
/// Store `value` under string key `key` on `obj`, taking `obj` to dictionary
/// mode if the key is new.
///
/// `obj` is a `&mut` handle because sidecar or slab allocation can trigger a
/// moving GC that relocates a young receiver. The relocation is reflected back
/// into the caller's handle so a sequence of `set` calls on a freshly allocated
/// object never writes through a stale handle.
pub fn set(obj: &mut JsObject, heap: &mut otter_gc::GcHeap, key: &str, value: Value) {
    set_inner(obj, heap, key, None, value);
}

/// [`set`] with an isolate atom for shaped-object presence lookup.
///
/// Dictionary append and storage retain the spelling because dictionary keys
/// are owned strings. Existing shaped slots, however, resolve through the
/// shape's atom chain and never compare property text.
pub(crate) fn set_atomized(
    obj: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
    value: Value,
) {
    set_inner(obj, heap, key.name(), Some(key), value);
}

fn set_inner(
    obj: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    atomized: Option<AtomizedPropertyKey<'_>>,
    value: Value,
) {
    let mut stored = value;
    let existing_offset = heap.read_payload(*obj, |body| match atomized {
        Some(atomized) => body_offset_of_atom(heap, body, atomized),
        None => body_offset_of(heap, body, key),
    });
    if existing_offset.is_none() {
        // A fresh key demotes this object to dictionary mode, and the
        // key list and slot metadata demotion writes live in the
        // sidecar. Reserved here, outside every payload borrow, because
        // creating it allocates — and only for the append that will
        // actually write it: an in-place update never touches the
        // sidecar, and reserving on every store gave most of the
        // bootstrap graph a sidecar it never used.
        ensure_exotic_with_pending_values(obj, heap, std::slice::from_mut(&mut stored))
            .expect("exotic sidecar");
    }
    if let Some(offset) = existing_offset {
        let i = offset as usize;
        // Overwriting an accessor slot with a data value diverges this slot
        // from its hidden class (which still records the accessor) without a
        // shape transition, so per-slot metadata must be materialized and the
        // shape can no longer be trusted for attribute reads.
        let is_accessor = heap.read_payload(*obj, |body| body.slot_attrs(heap, i).1);
        if is_accessor {
            materialize_slots(*obj, heap);
        }
        heap.with_payload(*obj, |body| {
            if is_accessor {
                body.slots_mut().entries_mut()[i].is_accessor = false;
            }
            body.set_data_value(i, stored);
        });
        record_slot_write(heap, *obj, stored);
        return;
    }
    let dictionary_keys = dictionary_keys_for_shape_transition(heap, *obj, existing_offset);
    let slot_metas = slot_metas_for_shape_transition(heap, *obj, existing_offset);
    let index = heap.read_payload(*obj, |body| body_property_count(heap, body));
    if reserve_slot_capacity(obj, heap, index + 1, std::slice::from_mut(&mut stored)).is_err() {
        return;
    }
    let Ok(slot_meta_table) = slot_meta_table_for_install(
        obj,
        heap,
        &slot_metas,
        index + 1,
        std::slice::from_mut(&mut stored),
    ) else {
        return;
    };
    let Ok(dict_table) = dict_keys_table_for_install(
        obj,
        heap,
        &dictionary_keys,
        key,
        std::slice::from_mut(&mut stored),
    ) else {
        return;
    };
    heap.with_payload(*obj, |body| {
        body.dictionary_shape_id = next_shape_id();
        if let Some(table) = dict_table {
            body.exotic_mut().dictionary_keys = table;
        }
        if let Some(table) = slot_meta_table {
            body.exotic_mut().slots = table;
        }
        dict_push_key(body, key.to_owned());
        body.shape = ShapeHandle::null();
        body.push_slot(index, SlotMeta::data_default(), stored);
    });
    let sidecar = heap.read_payload(*obj, |body| body.exotic.get());
    if let Some(table) = slot_meta_table {
        heap.record_write(sidecar, &table);
    }
    if let Some(table) = dict_table {
        heap.record_write(sidecar, &table);
    }
    record_slot_write(heap, *obj, stored);
}

/// Construction-time data store for callers that already allocated the next
/// GC-managed hidden class. `append_index` is the slot the new property
/// occupies — the object's property count before the append, which the caller
/// already knows from the shape it transitioned from — so the hot append path
/// performs no extra shape read.
pub(crate) fn set_with_shape(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    value: Value,
    next_shape: ShapeHandle,
    append_index: usize,
) {
    let mut obj = obj;
    let stored = value;
    let existing_offset = heap.read_payload(obj, |body| body_offset_of(heap, body, key));
    if let Some(offset) = existing_offset {
        let i = offset as usize;
        // Overwriting an accessor slot with a data value diverges this slot
        // from its hidden class (which still records the accessor) without a
        // shape transition, so per-slot metadata must be materialized and the
        // shape can no longer be trusted for attribute reads.
        let is_accessor = heap.read_payload(obj, |body| body.slot_attrs(heap, i).1);
        if is_accessor {
            materialize_slots(obj, heap);
        }
        heap.with_payload(obj, |body| {
            if is_accessor {
                body.slots_mut().entries_mut()[i].is_accessor = false;
            }
            body.set_data_value(i, stored);
        });
        record_slot_write(heap, obj, stored);
        return;
    }
    let index = append_index;
    debug_assert_eq!(
        index,
        shape_body::shape_property_count(heap, next_shape) as usize - 1
    );
    let mut stored = stored;
    if reserve_slot_capacity(&mut obj, heap, index + 1, std::slice::from_mut(&mut stored)).is_err()
    {
        return;
    }
    heap.with_payload(obj, |body| {
        debug_assert_object_shape_handle(next_shape, "shape-slot store");
        body.shape = next_shape;
        body.push_slot(index, SlotMeta::data_default(), stored);
    });
    record_slot_write(heap, obj, stored);
    heap.record_write(obj, &next_shape);
    #[cfg(debug_assertions)]
    debug_assert_appended_shape_slot(obj, heap);
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
    let success = descriptor_core::ordinary_set_data_property(obj, heap, key, value);
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
    append_index: usize,
) -> bool {
    let mut obj = obj;
    let mapped_cell = heap.read_payload(obj, |body| mapped_argument_cell(body, key));
    let success = descriptor_core::ordinary_set_data_property_with_shape(
        &mut obj,
        heap,
        key,
        value,
        next_shape,
        append_index,
    );
    if success && let Some(cell) = mapped_cell {
        store_upvalue(heap, cell, value);
    }
    #[cfg(debug_assertions)]
    if success {
        // `obj` was relocated in place if slot growth scavenged; the assertion
        // reads the live handle.
        debug_assert_appended_shape_slot(obj, heap);
    }
    success
}

/// Replace the prototype with a spec-legal value. `None` or
/// `Some(Value::null())` detaches the chain.
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
    let mut obj = obj;
    let new_proto = if let Some(value) = proto {
        if value.is_null() {
            ObjectPrototype::Null
        } else if let Some(o) = value.as_object() {
            ObjectPrototype::Object(o)
        } else if let Some(p) = value.as_proxy() {
            ObjectPrototype::Proxy(p)
        } else if value.is_object_type() {
            ObjectPrototype::Value(value)
        } else {
            return false;
        }
    } else {
        ObjectPrototype::Null
    };
    if matches!(
        new_proto,
        ObjectPrototype::Value(_) | ObjectPrototype::Proxy(_)
    ) {
        // Only a non-ordinary prototype lives in the sidecar; Null and
        // ordinary objects are encoded entirely by `jit_proto`, and
        // clearing a stale override needs no sidecar to exist. Reserved
        // here, outside the payload borrow, because creating it
        // allocates — and this may move `obj`, hence the `mut` local.
        ensure_exotic(&mut obj, heap).expect("exotic sidecar");
    }
    // §10.1.2.1 step 4 — `SameValue(V, current) is true → return true`.
    let current = heap.read_payload(obj, |body| body.prototype());
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
                cursor = heap.read_payload(p, |body| body.prototype());
            }
            ObjectPrototype::Proxy(_) | ObjectPrototype::Value(_) => break,
        }
    }
    let barrier_value = new_proto.as_value();
    let jit_proto = match &new_proto {
        ObjectPrototype::Object(o) => *o,
        ObjectPrototype::Null | ObjectPrototype::Value(_) | ObjectPrototype::Proxy(_) => {
            otter_gc::Gc::null()
        }
    };
    heap.with_payload(obj, |body| {
        body.jit_proto = jit_proto;
        match &new_proto {
            // Common case: encoded entirely by `jit_proto`; drop any stale
            // non-ordinary override so the object carries no exotic box for it.
            ObjectPrototype::Null | ObjectPrototype::Object(_) => {
                if let Some(exotic) = exotic_body_of(body.exotic.get()).map(|e|
            // SAFETY: a non-null handle names a live sidecar payload.
            unsafe { &mut *e })
                {
                    exotic.proto_override = None;
                }
            }
            // Non-ordinary prototype: store it in the boxed override.
            ObjectPrototype::Value(_) | ObjectPrototype::Proxy(_) => {
                body.exotic_mut().proto_override = Some(new_proto.clone());
            }
        }
    });
    if let Some(value) = &barrier_value {
        record_exotic_write(heap, obj, value);
    }
    true
}

fn prototype_same(a: &ObjectPrototype, b: &ObjectPrototype) -> bool {
    match (a, b) {
        (ObjectPrototype::Null, ObjectPrototype::Null) => true,
        (ObjectPrototype::Object(x), ObjectPrototype::Object(y)) => x == y,
        (ObjectPrototype::Proxy(x), ObjectPrototype::Proxy(y)) => x.ptr_eq(*y),
        (ObjectPrototype::Value(x), ObjectPrototype::Value(y)) => same_prototype_value(x, y),
        _ => false,
    }
}

fn same_prototype_value(a: &Value, b: &Value) -> bool {
    if let (Some(x), Some(y)) = (a.as_object(), b.as_object()) {
        return x == y;
    }
    if let (Some(x), Some(y)) = (a.as_array(), b.as_array()) {
        return crate::array::ptr_eq(x, y);
    }
    false
}

/// Replace the prototype with an ordinary object or `null`.
///
/// This compatibility helper preserves existing call sites that do
/// not need Proxy-as-prototype support.
pub fn set_prototype(obj: JsObject, heap: &mut otter_gc::GcHeap, proto: Option<JsObject>) {
    let value = proto.map(Value::object);
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
    // Delete normalizes to dictionary storage, which keeps per-slot metadata
    // materialized; snapshot the shaped object's attributes from the hidden
    // class before the in-place removal so the configurability check and the
    // value-array shift operate on a populated metadata vector.
    if existing_offset.is_some() {
        materialize_slots(obj, heap);
    }
    let mut obj_for_table = obj;
    let Ok(replacement_table) = dict_keys_table_for_install(
        &mut obj_for_table,
        heap,
        &Some(replacement_keys),
        "",
        &mut [],
    ) else {
        return false;
    };
    let obj = obj_for_table;
    heap.with_payload(obj, |body| {
        let Some(offset) = existing_offset else {
            // Spec step 2: missing → true.
            return true;
        };
        if !body.slots()[offset as usize].flags.configurable() {
            return false;
        }
        body.remove_slot(offset as usize);
        body.dictionary_shape_id = next_shape_id();
        if let Some(table) = replacement_table {
            body.exotic_mut().dictionary_keys = table;
        }
        body.shape = ShapeHandle::null();
        shape_cache::invalidate_fast_shape_assumptions(
            body,
            ShapeCacheInvalidation::DeleteOwnProperty,
        );
        remove_mapped_argument(body, key);
        true
    })
}

/// Force-remove an own data property only while it still holds `expected`.
///
/// Runtime publication transactions use this after an abrupt completion. The
/// identity guard prevents rollback from deleting a replacement installed by
/// re-entrant JavaScript. When the guarded slot is still present, removal
/// deliberately bypasses `configurable`: JavaScript may have tightened the
/// descriptor after publication, but that must not pin a failed transaction in
/// an internal cache.
///
/// Accessor properties never match, so rollback does not invoke user code.
pub(crate) fn delete_if_same_data(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    expected: Value,
) -> bool {
    let existing_offset = heap.read_payload(obj, |body| body_offset_of(heap, body, key));
    let Some(offset) = existing_offset else {
        return false;
    };

    materialize_slots(obj, heap);
    let matches_expected = heap.read_payload(obj, |body| {
        let offset = offset as usize;
        !body.slots()[offset].is_accessor
            && crate::abstract_ops::is_strictly_equal(
                &body.data_value(heap, offset),
                &expected,
                heap,
            )
    });
    if !matches_expected {
        return false;
    }

    let replacement_keys = heap.read_payload(obj, |body| {
        let mut keys = string_keys_in_shape_order(heap, body);
        let offset = offset as usize;
        if offset < keys.len() {
            keys.remove(offset);
        }
        keys
    });
    let mut obj_for_table = obj;
    let Ok(replacement_table) = dict_keys_table_for_install(
        &mut obj_for_table,
        heap,
        &Some(replacement_keys),
        "",
        &mut [],
    ) else {
        return false;
    };
    let obj = obj_for_table;
    heap.with_payload(obj, |body| {
        body.remove_slot(offset as usize);
        body.dictionary_shape_id = next_shape_id();
        if let Some(table) = replacement_table {
            body.exotic_mut().dictionary_keys = table;
        }
        body.shape = ShapeHandle::null();
        shape_cache::invalidate_fast_shape_assumptions(
            body,
            ShapeCacheInvalidation::DeleteOwnProperty,
        );
        remove_mapped_argument(body, key);
    });
    true
}

/// Set or overwrite a symbol-keyed own data property through the
/// same descriptor-aware `[[Set]]` data-write core as string keys.
///
/// Fires the GC write barrier when `value` carries a `Gc<…>`
/// handle.
pub fn set_symbol(obj: JsObject, heap: &mut otter_gc::GcHeap, key: JsSymbol, value: Value) -> bool {
    descriptor_core::ordinary_set_symbol_data_property(obj, heap, key, value)
}

/// Remove a symbol-keyed own property.
pub fn delete_symbol(obj: JsObject, heap: &mut otter_gc::GcHeap, key: JsSymbol) -> bool {
    heap.with_payload(obj, |body| {
        if let Some(pos) = body.symbol_props().iter().position(|(k, _)| k.ptr_eq(key)) {
            if !body.symbol_props()[pos].1.flags.configurable() {
                return false;
            }
            body.symbol_props_mut()
                .expect("existing symbol slot implies a table")
                .remove(pos);
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
///
/// Field-presence-aware §10.1.6.3 OrdinaryDefineOwnProperty for
/// string-keyed properties. A `PropertyDescriptor`-based
/// `[[DefineOwnProperty]]`: missing fields preserve the existing value,
/// missing-and-new defaults to spec defaults (§10.1.6.3 step 5).
pub fn define_own_property_partial(
    obj_ref: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    descriptor: PartialPropertyDescriptor,
) -> bool {
    // The sidecar allocates, so it is reserved here, outside the payload
    // borrow below. This may move the receiver; every relocation below is
    // reflected back through `obj_ref` so the caller's handle stays live.
    let mut descriptor = descriptor;
    {
        let descriptor_slot = &mut descriptor;
        let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            crate::pelt::PeltField::pelt_trace(&mut *descriptor_slot, visitor);
        };
        ensure_exotic_with_roots(obj_ref, heap, &mut roots).expect("exotic sidecar");
    }
    let completed = descriptor.complete_for_new_property();
    let existing_offset = heap.read_payload(*obj_ref, |body| body_offset_of(heap, body, key));
    let dictionary_keys = dictionary_keys_for_shape_transition(heap, *obj_ref, existing_offset);
    let slot_metas = slot_metas_for_shape_transition(heap, *obj_ref, existing_offset);
    let append_index = heap.read_payload(*obj_ref, |body| body_property_count(heap, body));
    // §10.1.6.3 ValidateAndApplyPropertyDescriptor runs outside the
    // mutable body borrow so the BigInt-BigInt SameValue arm can
    // read both bodies through `heap`. Distinct GC handles holding
    // the same numeric value must compare equal per spec.
    let merged_for_existing = if let Some(offset) = existing_offset {
        let existing = heap.read_payload(*obj_ref, |body| body.slot_data(heap, offset as usize));
        match descriptor_core::validate_and_apply_partial(&existing, &descriptor, heap) {
            Some(merged) => Some(merged),
            None => return false,
        }
    } else {
        None
    };
    // Lower the slot to its flat `(meta, value)` form before taking the body
    // borrow: an accessor allocates its cell here (rooting the receiver), so
    // the mutation closure never allocates.
    let slot_source = match merged_for_existing {
        Some(merged) => merged,
        None => SlotData::from_descriptor(completed),
    };
    let (meta, stored) = match slot_source.into_flat(heap, obj_ref) {
        Ok(parts) => parts,
        Err(_) => return false,
    };
    let mut stored = stored;
    // Redefining an existing shaped slot without a shape transition diverges
    // its attributes from the hidden class. Materialization allocates, so keep
    // the flattened direct value rooted while it runs.
    if existing_offset.is_some() {
        materialize_slots_with_pending_values(obj_ref, heap, std::slice::from_mut(&mut stored));
    }
    if reserve_slot_capacity(
        obj_ref,
        heap,
        append_index + 1,
        std::slice::from_mut(&mut stored),
    )
    .is_err()
    {
        return false;
    }
    let Ok(slot_meta_table) = slot_meta_table_for_install(
        obj_ref,
        heap,
        &slot_metas,
        append_index + 1,
        std::slice::from_mut(&mut stored),
    ) else {
        return false;
    };
    let dict_table = if existing_offset.is_none() {
        let Ok(table) = dict_keys_table_for_install(
            obj_ref,
            heap,
            &dictionary_keys,
            key,
            std::slice::from_mut(&mut stored),
        ) else {
            return false;
        };
        table
    } else {
        None
    };
    let success = heap.with_payload(*obj_ref, |body| {
        if let Some(offset) = existing_offset {
            body.set_slot(offset as usize, meta, stored, None);
            true
        } else {
            if !body.extensible {
                return false;
            }
            body.dictionary_shape_id = next_shape_id();
            if let Some(table) = dict_table {
                body.exotic_mut().dictionary_keys = table;
            }
            if let Some(table) = slot_meta_table {
                body.exotic_mut().slots = table;
            }
            dict_push_key(body, key.to_owned());
            body.shape = ShapeHandle::null();
            body.push_slot(append_index, meta, stored);
            true
        }
    });
    let sidecar = heap.read_payload(*obj_ref, |body| body.exotic.get());
    if let Some(table) = slot_meta_table {
        heap.record_write(sidecar, &table);
    }
    if let Some(table) = dict_table {
        heap.record_write(sidecar, &table);
    }
    if success {
        // Mapped arguments only consume a present data `[[Value]]`; keep that
        // field synchronized with the collector-rewritten slot word.
        if descriptor.value.is_some() && !meta.is_accessor {
            descriptor.value = Some(stored);
        }
        apply_mapped_arguments_partial_define(*obj_ref, heap, key, descriptor, existing_offset);
        record_slot_write(heap, *obj_ref, stored);
    }
    success
}

pub(crate) fn define_own_property_partial_with_shape(
    obj_ref: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    descriptor: PartialPropertyDescriptor,
    next_shape: ShapeHandle,
) -> bool {
    let completed = descriptor.complete_for_new_property();
    let existing_offset = heap.read_payload(*obj_ref, |body| body_offset_of(heap, body, key));
    let merged_for_existing = if let Some(offset) = existing_offset {
        let existing = heap.read_payload(*obj_ref, |body| body.slot_data(heap, offset as usize));
        match descriptor_core::validate_and_apply_partial(&existing, &descriptor, heap) {
            Some(merged) => Some(merged),
            None => return false,
        }
    } else {
        None
    };
    let slot_source = match merged_for_existing {
        Some(merged) => merged,
        None => SlotData::from_descriptor(completed),
    };
    let (meta, stored) = match slot_source.into_flat(heap, obj_ref) {
        Ok(parts) => parts,
        Err(_) => return false,
    };
    // The appended slot's flat index is the new shape's last offset.
    let append_index = shape_body::shape_property_count(heap, next_shape) as usize - 1;
    let mut stored = stored;
    if reserve_slot_capacity(
        obj_ref,
        heap,
        append_index + 1,
        std::slice::from_mut(&mut stored),
    )
    .is_err()
    {
        return false;
    }
    let success = heap.with_payload(*obj_ref, |body| {
        if let Some(offset) = existing_offset {
            // Redefine: `next_shape` is the attribute-encoding class that
            // records this slot's new flags/kind (computed by the caller).
            body.set_slot(offset as usize, meta, stored, Some(next_shape));
            true
        } else {
            if !body.extensible {
                return false;
            }
            debug_assert_object_shape_handle(next_shape, "shape-slot store");
            body.shape = next_shape;
            body.push_slot(append_index, meta, stored);
            true
        }
    });
    if success {
        apply_mapped_arguments_partial_define(*obj_ref, heap, key, descriptor, existing_offset);
        record_slot_write(heap, *obj_ref, stored);
        record_exotic_write(heap, *obj_ref, &next_shape);
        #[cfg(debug_assertions)]
        if existing_offset.is_none() {
            debug_assert_appended_shape_slot(*obj_ref, heap);
        }
    }
    success
}

/// Field-presence-aware §10.1.6.3 for symbol-keyed properties.
pub fn define_own_symbol_property_partial(
    obj_ref: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    key: JsSymbol,
    descriptor: PartialPropertyDescriptor,
) -> bool {
    // The sidecar and the symbol table allocate, so both are reserved
    // here, outside the payload borrow below. This may move the receiver;
    // the relocation is reflected back through `obj_ref` so the caller's
    // handle stays live. The descriptor's values are rooted across the
    // reservation.
    let mut descriptor = descriptor;
    {
        let descriptor_slot = &mut descriptor;
        let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            crate::pelt::PeltField::pelt_trace(&mut *descriptor_slot, visitor);
        };
        reserve_symbol_prop_capacity(obj_ref, heap, &mut roots).expect("symbol prop table");
    }
    let obj = *obj_ref;
    let completed = descriptor.complete_for_new_property();
    let barrier_descriptor = completed.clone();
    let existing_pos_and_slot = heap.read_payload(obj, |body| {
        body.symbol_props()
            .iter()
            .position(|(k, _)| k.ptr_eq(key))
            .map(|pos| (pos, body.symbol_props()[pos].1.clone()))
    });
    let merged_for_existing = if let Some((_, ref existing)) = existing_pos_and_slot {
        match descriptor_core::validate_and_apply_partial(existing, &descriptor, heap) {
            Some(merged) => Some(merged),
            None => return false,
        }
    } else {
        None
    };
    let existing_pos = existing_pos_and_slot.as_ref().map(|(p, _)| *p);
    let success = heap.with_payload(obj, |body| {
        if let Some(pos) = existing_pos {
            body.symbol_props_mut()
                .expect("existing symbol slot implies a table")
                .entries_mut()[pos]
                .1 = merged_for_existing.unwrap();
            true
        } else {
            if !body.extensible {
                return false;
            }
            body.symbol_props_mut()
                .expect("symbol table reserved before the borrow")
                .push((key, SlotData::from_descriptor(completed.clone())));
            true
        }
    });
    if success {
        record_symbol_entry_write(heap, obj, &key, &barrier_descriptor);
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
    let mut obj = obj;
    define_own_property_in_place(&mut obj, heap, key, descriptor)
}

/// Like [`define_own_property`], but reflects any relocation the write's own
/// allocation drove back into the caller's handle.
///
/// Sidecar or slab growth can move a young receiver; the caller's `obj` must be
/// refreshed so a following write on the same builder (for example,
/// [`crate::ObjectBuilder`] chaining several properties) never dereferences a
/// vacated cell.
pub fn define_own_property_in_place(
    obj_ref: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    descriptor: PropertyDescriptor,
) -> bool {
    let mut descriptor = descriptor;
    let existing_offset = heap.read_payload(*obj_ref, |body| body_offset_of(heap, body, key));
    if existing_offset.is_none() {
        // A fresh key demotes this object to dictionary mode, and the
        // key list and slot metadata demotion writes live in the
        // sidecar. Reserved here, outside every payload borrow, because
        // creating it allocates. Redefinition of an existing slot goes
        // through `materialize_slots`, which reserves for itself.
        let descriptor_slot = &mut descriptor;
        let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            crate::pelt::PeltField::pelt_trace(&mut *descriptor_slot, visitor);
        };
        ensure_exotic_with_roots(obj_ref, heap, &mut roots).expect("exotic sidecar");
    }
    let mut obj = *obj_ref;
    let map_is_data = descriptor.is_data();
    let map_writable = descriptor.writable();
    let dictionary_keys = dictionary_keys_for_shape_transition(heap, obj, existing_offset);
    let slot_metas = slot_metas_for_shape_transition(heap, obj, existing_offset);
    let append_index = heap.read_payload(obj, |body| body_property_count(heap, body));
    let merged_for_existing = if let Some(offset) = existing_offset {
        let existing = heap.read_payload(obj, |body| body.slot_data(heap, offset as usize));
        match descriptor_core::validate_and_apply(&existing, &descriptor, heap) {
            Some(merged) => Some(merged),
            None => return false,
        }
    } else {
        None
    };
    let slot_source = match merged_for_existing {
        Some(merged) => merged,
        None => SlotData::from_descriptor(descriptor),
    };
    let (meta, stored) = match slot_source.into_flat(heap, &mut obj) {
        Ok(parts) => parts,
        Err(_) => {
            // `into_flat` may have relocated the receiver before failing; reflect
            // it so the caller's handle is never left pointing at a vacated cell.
            *obj_ref = obj;
            return false;
        }
    };
    let mut stored = stored;
    // Materialization may allocate; the flattened direct value has not entered
    // the object yet, so keep it in the pending-root slice.
    if existing_offset.is_some() {
        materialize_slots_with_pending_values(&mut obj, heap, std::slice::from_mut(&mut stored));
    }
    if reserve_slot_capacity(
        &mut obj,
        heap,
        append_index + 1,
        std::slice::from_mut(&mut stored),
    )
    .is_err()
    {
        return false;
    }
    let Ok(slot_meta_table) = slot_meta_table_for_install(
        &mut obj,
        heap,
        &slot_metas,
        append_index + 1,
        std::slice::from_mut(&mut stored),
    ) else {
        return false;
    };
    let dict_table = if existing_offset.is_none() {
        let Ok(table) = dict_keys_table_for_install(
            &mut obj,
            heap,
            &dictionary_keys,
            key,
            std::slice::from_mut(&mut stored),
        ) else {
            return false;
        };
        table
    } else {
        None
    };
    let success = heap.with_payload(obj, |body| {
        if let Some(offset) = existing_offset {
            body.set_slot(offset as usize, meta, stored, None);
            true
        } else {
            if !body.extensible {
                return false;
            }
            body.dictionary_shape_id = next_shape_id();
            if let Some(table) = dict_table {
                body.exotic_mut().dictionary_keys = table;
            }
            if let Some(table) = slot_meta_table {
                body.exotic_mut().slots = table;
            }
            dict_push_key(body, key.to_owned());
            body.shape = ShapeHandle::null();
            body.push_slot(append_index, meta, stored);
            true
        }
    });
    let sidecar = heap.read_payload(obj, |body| body.exotic.get());
    if let Some(table) = slot_meta_table {
        heap.record_write(sidecar, &table);
    }
    if let Some(table) = dict_table {
        heap.record_write(sidecar, &table);
    }
    if success {
        let mapped_cell = heap.read_payload(obj, |body| mapped_argument_cell(body, key));
        if let Some(cell) = mapped_cell {
            if map_is_data {
                store_upvalue(heap, cell, stored);
                if !map_writable {
                    heap.with_payload(obj, |body| remove_mapped_argument(body, key));
                }
            } else {
                heap.with_payload(obj, |body| remove_mapped_argument(body, key));
            }
        }
        record_slot_write(heap, obj, stored);
    }
    // Reflect any relocation the write drove back into the caller's handle.
    *obj_ref = obj;
    success
}

/// Symbol-keyed counterpart to [`define_own_property`].
pub fn define_own_symbol_property(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: JsSymbol,
    descriptor: PropertyDescriptor,
) -> bool {
    // The sidecar and the symbol table allocate, so both are reserved
    // here, outside the payload borrow below. This may move `obj`,
    // which is why the local is `mut`; the descriptor's values are
    // rooted across the reservation.
    let mut obj = obj;
    let mut descriptor = descriptor;
    {
        let descriptor_slot = &mut descriptor;
        let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            crate::pelt::PeltField::pelt_trace(&mut *descriptor_slot, visitor);
        };
        reserve_symbol_prop_capacity(&mut obj, heap, &mut roots).expect("symbol prop table");
    }
    let barrier_descriptor = descriptor.clone();
    let existing_pos_and_slot = heap.read_payload(obj, |body| {
        body.symbol_props()
            .iter()
            .position(|(k, _)| k.ptr_eq(key))
            .map(|pos| (pos, body.symbol_props()[pos].1.clone()))
    });
    let merged_for_existing = if let Some((_, ref existing)) = existing_pos_and_slot {
        match descriptor_core::validate_and_apply(existing, &descriptor, heap) {
            Some(merged) => Some(merged),
            None => return false,
        }
    } else {
        None
    };
    let existing_pos = existing_pos_and_slot.as_ref().map(|(p, _)| *p);
    let success = heap.with_payload(obj, |body| {
        if let Some(pos) = existing_pos {
            body.symbol_props_mut()
                .expect("existing symbol slot implies a table")
                .entries_mut()[pos]
                .1 = merged_for_existing.unwrap();
            true
        } else {
            if !body.extensible {
                return false;
            }
            body.symbol_props_mut()
                .expect("symbol table reserved before the borrow")
                .push((key, SlotData::from_descriptor(descriptor)));
            true
        }
    });
    if success {
        record_symbol_entry_write(heap, obj, &key, &barrier_descriptor);
    }
    success
}

/// Validate one descriptor update against an existing descriptor using
/// the same `ValidateAndApplyPropertyDescriptor` core as ordinary objects.
pub(crate) fn validate_descriptor_update(
    existing: &PropertyDescriptor,
    incoming: &PropertyDescriptor,
    heap: &otter_gc::GcHeap,
) -> Option<PropertyDescriptor> {
    descriptor_core::validate_descriptor_update(existing, incoming, heap)
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

/// The [`resolve_set`] variant for an atomized key: every own-lookup on the
/// receiver-and-prototype walk resolves by atom compare.
pub(crate) fn resolve_set_atomized(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
) -> SetOutcome {
    resolve_set_inner(obj, heap, key.name(), Some(key))
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
    resolve_set_inner(obj, heap, key, None)
}

fn resolve_set_inner(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    key: &str,
    atomized: Option<AtomizedPropertyKey<'_>>,
) -> SetOutcome {
    let lookup = |target: JsObject| {
        heap.read_payload(target, |body| {
            let offset = match atomized {
                Some(atomized) => body_offset_of_atom(heap, body, atomized),
                None => body_offset_of(heap, body, key),
            };
            match offset {
                Some(offset) => {
                    let mut found = body.slot_lookup_at(heap, offset as usize);
                    if let Some(cell) = mapped_argument_cell(body, key)
                        && let PropertyLookup::Data { value, .. } = &mut found
                    {
                        *value = read_upvalue(heap, cell);
                    }
                    found
                }
                None => PropertyLookup::Absent,
            }
        })
    };
    // Walk own + prototype chain looking for an accessor or a
    // non-writable shadow.
    let own = lookup(obj);
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
    if let Some(parent) = exotic_prototype_value(obj, heap) {
        return SetOutcome::ExoticParent { parent };
    }
    let mut node = obj;
    let mut current = prototype(obj, heap);
    let mut hops = 0;
    while let Some(proto) = current {
        if hops >= PROTO_CHAIN_HARD_CAP {
            break;
        }
        hops += 1;
        match lookup(proto) {
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
        node = proto;
        if let Some(parent) = exotic_prototype_value(node, heap) {
            return SetOutcome::ExoticParent { parent };
        }
        current = prototype(proto, heap);
    }
    let _ = node;
    // Nothing on the chain — install a fresh data slot.
    if !is_extensible(obj, heap) {
        return SetOutcome::Reject {
            reason: SetRejectReason::NonExtensible,
        };
    }
    SetOutcome::AssignData
}

/// A stored `[[Prototype]]` that is NOT an ordinary `JsObject` —
/// e.g. a TypedArray or Proxy value installed via
/// `Object.create(exotic)` / `Object.setPrototypeOf`. Ordinary-walk
/// helpers must stop there and let the value-level funnel dispatch
/// the exotic's own internal methods.
fn exotic_prototype_value(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<Value> {
    let stored = prototype_value(obj, heap)?;
    if stored.as_object().is_some() || stored.is_null() || stored.is_undefined() {
        return None;
    }
    Some(stored)
}

/// Symbol-keyed counterpart to [`resolve_set`].
pub fn resolve_symbol_set(obj: JsObject, heap: &otter_gc::GcHeap, key: JsSymbol) -> SetOutcome {
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
    // In-place fallback (dictionary mode, or callers without a shape runtime):
    // materialize per-slot metadata so the attribute change is recorded on a
    // populated vector instead of diverging silently from the hidden class.
    materialize_slots(obj, heap);
    heap.with_payload(obj, |body| {
        body.extensible = false;
        if let Some(exotic) = exotic_body_of(body.exotic.get()).map(|e|
            // SAFETY: a non-null handle names a live sidecar payload.
            unsafe { &mut *e })
        {
            for slot in slot_meta_body_of(exotic.slots)
                // SAFETY: a non-null handle names a live table.
                .map_or(&mut [][..], |table| unsafe { (*table).entries_mut() })
                .iter_mut()
            {
                slot.flags = slot.flags.with_configurable(false);
            }
            for (key, slot) in symbol_props_body_of(exotic.symbol_props)
                // SAFETY: a non-null handle names a live table.
                .map_or(&mut [][..], |table| unsafe { (*table).entries_mut() })
                .iter_mut()
            {
                // §6.2.12 — Private Name carriers are not properties;
                // SetIntegrityLevel never touches them.
                if key.is_private_name() {
                    continue;
                }
                slot.flags = slot.flags.with_configurable(false);
            }
        }
    });
}

/// `Object.seal` for a shaped object, transitioning to `new_shape` — the
/// attribute-encoding hidden class that records every slot as
/// non-configurable. A non-overridden shaped object stores no per-slot
/// metadata, so the shape transition alone records the change; a previously
/// overridden object keeps reading from its materialized metadata, which is
/// updated in lockstep. Symbol-keyed slots (not part of the shape) mutate in
/// place.
pub(crate) fn seal_with_shape(obj: JsObject, heap: &mut otter_gc::GcHeap, new_shape: ShapeHandle) {
    heap.with_payload(obj, |body| {
        body.extensible = false;
        debug_assert_object_shape_handle(new_shape, "shape-slot store");
        body.shape = new_shape;
        if let Some(exotic) = exotic_body_of(body.exotic.get()).map(|e|
            // SAFETY: a non-null handle names a live sidecar payload.
            unsafe { &mut *e })
        {
            for slot in slot_meta_body_of(exotic.slots)
                // SAFETY: a non-null handle names a live table.
                .map_or(&mut [][..], |table| unsafe { (*table).entries_mut() })
                .iter_mut()
            {
                slot.flags = slot.flags.with_configurable(false);
            }
            for (key, slot) in symbol_props_body_of(exotic.symbol_props)
                // SAFETY: a non-null handle names a live table.
                .map_or(&mut [][..], |table| unsafe { (*table).entries_mut() })
                .iter_mut()
            {
                // §6.2.12 — Private Name carriers are not properties.
                if key.is_private_name() {
                    continue;
                }
                slot.flags = slot.flags.with_configurable(false);
            }
        }
    });
    heap.record_write(obj, &new_shape);
}

/// `Object.freeze(o)` core — clears `[[Extensible]]`, then for
/// every own property: data slots become non-writable and
/// non-configurable; accessor slots become non-configurable.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-setintegritylevel>
pub fn freeze(obj: JsObject, heap: &mut otter_gc::GcHeap) {
    // In-place fallback: materialize per-slot metadata first (see [`seal`]).
    materialize_slots(obj, heap);
    heap.with_payload(obj, |body| {
        body.extensible = false;
        if let Some(exotic) = exotic_body_of(body.exotic.get()).map(|e|
            // SAFETY: a non-null handle names a live sidecar payload.
            unsafe { &mut *e })
        {
            for slot in slot_meta_body_of(exotic.slots)
                // SAFETY: a non-null handle names a live table.
                .map_or(&mut [][..], |table| unsafe { (*table).entries_mut() })
                .iter_mut()
            {
                slot.flags = slot.flags.with_configurable(false);
                if !slot.is_accessor {
                    slot.flags = slot.flags.with_writable(false);
                }
            }
            for (key, slot) in symbol_props_body_of(exotic.symbol_props)
                // SAFETY: a non-null handle names a live table.
                .map_or(&mut [][..], |table| unsafe { (*table).entries_mut() })
                .iter_mut()
            {
                // §6.2.12 — Private Name carriers are not properties.
                if key.is_private_name() {
                    continue;
                }
                slot.flags = slot.flags.with_configurable(false);
                if slot.kind.is_data() {
                    slot.flags = slot.flags.with_writable(false);
                }
            }
        }
    });
}

/// `Object.freeze` for a shaped object, transitioning to `new_shape` — the
/// attribute-encoding hidden class that records data slots as
/// non-writable/non-configurable and accessor slots as non-configurable. A
/// non-overridden shaped object stores no per-slot metadata (the shape
/// transition records the change); a previously overridden object keeps its
/// materialized metadata current. Symbol-keyed slots mutate in place.
pub(crate) fn freeze_with_shape(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    new_shape: ShapeHandle,
) {
    heap.with_payload(obj, |body| {
        body.extensible = false;
        debug_assert_object_shape_handle(new_shape, "shape-slot store");
        body.shape = new_shape;
        if let Some(exotic) = exotic_body_of(body.exotic.get()).map(|e|
            // SAFETY: a non-null handle names a live sidecar payload.
            unsafe { &mut *e })
        {
            for slot in slot_meta_body_of(exotic.slots)
                // SAFETY: a non-null handle names a live table.
                .map_or(&mut [][..], |table| unsafe { (*table).entries_mut() })
                .iter_mut()
            {
                slot.flags = slot.flags.with_configurable(false);
                if !slot.is_accessor {
                    slot.flags = slot.flags.with_writable(false);
                }
            }
            for (key, slot) in symbol_props_body_of(exotic.symbol_props)
                // SAFETY: a non-null handle names a live table.
                .map_or(&mut [][..], |table| unsafe { (*table).entries_mut() })
                .iter_mut()
            {
                // §6.2.12 — Private Name carriers are not properties.
                if key.is_private_name() {
                    continue;
                }
                slot.flags = slot.flags.with_configurable(false);
                if slot.kind.is_data() {
                    slot.flags = slot.flags.with_writable(false);
                }
            }
        }
    });
    heap.record_write(obj, &new_shape);
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
    /// Heap used to resolve shape and accessor metadata during iteration.
    heap: &'a otter_gc::GcHeap,
    /// `(key, flat slot index, flags, is_accessor)` in ordinary own-key order.
    /// Per-slot attributes are captured at build time (where the hidden class
    /// is reachable through the heap) so iteration needs no further shape walk
    /// and the common shaped object carries no per-slot metadata.
    string_keys: Vec<(String, usize, PropertyFlags, bool)>,
}

impl<'a> Properties<'a> {
    /// Iterate every `(key, data-value)` pair in ordinary own-key
    /// order, regardless of enumerability. Accessor slots are
    /// surfaced as the sentinel `Value::Undefined` — callers that
    /// need accessor fidelity must consult [`get_own_descriptor`]
    /// directly.
    pub fn iter(&self) -> impl Iterator<Item = (&str, Value)> {
        self.string_keys.iter().map(|(key, idx, _, is_accessor)| {
            let value = if !*is_accessor {
                self.body.data_value(self.heap, *idx)
            } else {
                Value::undefined()
            };
            (key.as_str(), value)
        })
    }

    /// Iterate string keys in ordinary own-key order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.string_keys.iter().map(|(key, _, _, _)| key.as_str())
    }

    /// Iterate symbol-keyed own properties in insertion order.
    /// Used by `Object.getOwnPropertySymbols` (§20.1.2.13) and
    /// `Reflect.ownKeys` (§28.1.16) to surface symbol keys.
    pub fn symbol_keys(&self) -> impl Iterator<Item = JsSymbol> + '_ {
        self.body.symbol_props().iter().map(|(k, _)| *k)
    }

    /// Iterate `(key, data-value)` pairs in ordinary own-key order,
    /// skipping accessor and non-enumerable slots. Used by
    /// JSON.stringify and `for…in` once it lands.
    pub fn enumerable_data_iter(&self) -> impl Iterator<Item = (&str, Value)> {
        self.string_keys
            .iter()
            .filter_map(|(key, idx, flags, is_accessor)| {
                if !flags.enumerable() {
                    return None;
                }
                if !*is_accessor {
                    Some((key.as_str(), self.body.data_value(self.heap, *idx)))
                } else {
                    None
                }
            })
    }

    /// `(key, flat slot index)` for every enumerable own **string-keyed
    /// data** property, in ordinary own-key order — or `None` if any
    /// enumerable own string property is an accessor.
    ///
    /// JSON.stringify's fast object path uses this to read each value
    /// directly by slot offset (re-validated against the live shape per
    /// key) instead of re-resolving the key through `[[Get]]`. `None`
    /// forces the observable `[[Get]]` path, since an enumerable getter
    /// has side effects the fast path must not skip.
    pub fn enumerable_string_data_offsets(&self) -> Option<Vec<(String, u16)>> {
        let mut out = Vec::with_capacity(self.string_keys.len());
        for (key, idx, flags, is_accessor) in &self.string_keys {
            if !flags.enumerable() {
                continue;
            }
            if *is_accessor {
                return None;
            }
            out.push((key.clone(), u16::try_from(*idx).ok()?));
        }
        Some(out)
    }

    /// Iterate enumerable own-key names (string-keyed only) in
    /// ordinary own-key order.
    pub fn enumerable_keys(&self) -> impl Iterator<Item = &str> {
        self.string_keys
            .iter()
            .filter_map(|(key, _, flags, _)| flags.enumerable().then_some(key.as_str()))
    }

    /// Iterate `(symbol, data-value)` pairs over enumerable
    /// symbol-keyed own data properties in insertion order. Used by
    /// `Object.assign` (§20.1.2.1 step 4.c.ii) which copies every
    /// enumerable own string *and* symbol key from the source.
    pub fn enumerable_symbol_data_iter(&self) -> impl Iterator<Item = (JsSymbol, Value)> + '_ {
        self.body.symbol_props().iter().filter_map(|(sym, slot)| {
            if !slot.flags.enumerable() {
                return None;
            }
            if slot.kind.is_data() {
                Some((*sym, slot.value))
            } else {
                None
            }
        })
    }

    /// Return dense own data values for integer indices `0..len`.
    ///
    /// Accessors and holes return `None` so callers can fall back to
    /// ordinary `[[Get]]` and preserve observable getter/prototype
    /// behaviour.
    pub fn dense_indexed_data_values(&self, len: usize) -> Option<Vec<Value>> {
        let mut values = vec![None; len];
        let mut seen = 0usize;
        for (key, idx, _, is_accessor) in &self.string_keys {
            let Some(array_index) = key_order::array_index_property_name(key) else {
                continue;
            };
            let Ok(index) = usize::try_from(array_index) else {
                continue;
            };
            if index >= len {
                continue;
            }
            if !*is_accessor {
                if values[index].is_none() {
                    seen += 1;
                }
                values[index] = Some(self.body.data_value(self.heap, *idx));
            } else {
                return None;
            }
        }
        if seen != len {
            return None;
        }
        values.into_iter().collect()
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
        body.dict_keys().map_or_else(Vec::new, |table| {
            (0..table.len())
                .map(|slot| (table.key_at(slot).to_string(), slot))
                .collect()
        })
    };

    order_string_key_entries(insertion_order)
}

/// Ordered `(key, flags, is_accessor)` for every string-keyed slot of a shaped
/// hidden class, in slot-offset order.
///
/// Drives attribute-encoding shape transitions (`freeze` / `seal` /
/// `defineProperty` redefine): the runtime rebuilds the class by replaying
/// these slots with modified attributes, so an in-place attribute change
/// re-points the object at a matching shape instead of diverging from it.
/// For a `defineProperty` redefine of an existing string-keyed slot, return
/// the post-merge `(flags, is_accessor, offset)` so the caller can transition
/// the hidden class to record the change. `None` when the key is absent or the
/// redefine is rejected — the in-place path then handles those (rejection
/// returns `false`; an absent key is an append handled elsewhere).
#[must_use]
pub(crate) fn redefine_merged_attrs(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    key: &str,
    descriptor: &PartialPropertyDescriptor,
) -> Option<(PropertyFlags, bool, u16)> {
    let offset = heap.read_payload(obj, |body| body_offset_of(heap, body, key))?;
    let existing = heap.read_payload(obj, |body| body.slot_data(heap, offset as usize));
    let merged = descriptor_core::validate_and_apply_partial(&existing, descriptor, heap)?;
    Some((merged.flags, !merged.kind.is_data(), offset))
}

/// Ordered `(key, flags, is_accessor)` for every own string-keyed slot of a
/// dictionary-mode object, in slot order, when its storage can be described by
/// a hidden class instead.
///
/// `None` when the object already has one, when it left fast-shape mode (a
/// delete makes the append-only key history a lie), or when it holds more slots
/// than fast storage keeps. The caller replays the returned slots from the
/// empty root to obtain the class, then installs it with [`adopt_fast_shape`].
#[must_use]
pub(crate) fn dictionary_ordered_slot_attrs(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
) -> Option<Vec<(String, PropertyFlags, bool)>> {
    heap.read_payload(obj, |body| {
        if !body.shape.is_null() || !shape_cache::supports_fast_property_ic(body) {
            return None;
        }
        let count = body.dict_key_count();
        if count > MAX_FAST_PROPERTIES as usize || body.slots().len() != count {
            return None;
        }
        Some(
            (0..count)
                .map(|offset| {
                    let key = body
                        .dict_keys()
                        .expect("counted keys imply a table")
                        .key_at(offset)
                        .to_string();
                    let (flags, is_accessor) = body.slot_attrs(heap, offset);
                    (key.clone(), flags, is_accessor)
                })
                .collect(),
        )
    })
}

/// Re-point a dictionary-mode object at `shape`, which must record exactly its
/// current slots, in order, with their current attributes.
///
/// The object leaves dictionary storage entirely: the key vector, the key index
/// and the materialized per-slot metadata are dropped and the hidden class
/// becomes the sole source of slot offsets and attributes. That is what lets an
/// inline cache name the receiver — a guard identifies a receiver by its shape,
/// and a dictionary object has none.
pub(crate) fn adopt_fast_shape(obj: JsObject, heap: &mut otter_gc::GcHeap, shape: ShapeHandle) {
    debug_assert_object_shape_handle(shape, "slow-to-fast migration");
    debug_assert_eq!(
        heap.read_payload(obj, |body| body.dict_key_count()),
        shape_body::shape_property_count(heap, shape) as usize,
        "migrated hidden class must record every dictionary slot"
    );
    heap.with_payload(obj, |body| {
        body.shape = shape;
        body.slot_attrs_overridden = false;
        body.dictionary_shape_id = ShapeId::UNASSIGNED;
        if let Some(exotic) = exotic_body_of(body.exotic.get()).map(|e|
            // SAFETY: a non-null handle names a live sidecar payload.
            unsafe { &mut *e })
        {
            if let Some(table) = dict_keys_body_of(exotic.dictionary_keys) {
                // SAFETY: a non-null handle names a live table.
                unsafe { (*table).clear() };
            }
            if let Some(table) = slot_meta_body_of(exotic.slots) {
                // SAFETY: a non-null handle names a live table.
                unsafe { (*table).clear() };
            }
        }
    });
    heap.record_write(obj, &shape);
}

#[must_use]
pub(crate) fn shape_ordered_slot_attrs(
    heap: &otter_gc::GcHeap,
    shape: ShapeHandle,
) -> Vec<(String, PropertyFlags, bool)> {
    let mut keyed = shape_body::shape_keys_ordered(heap, shape);
    keyed.sort_by_key(|(_, offset)| *offset);
    keyed
        .into_iter()
        .map(|(key, offset)| {
            let (flags, is_accessor) = shape_body::shape_slot_attrs(heap, shape, offset)
                .unwrap_or((PropertyFlags::data_default(), false));
            (
                String::from_utf16_lossy(&to_utf16_vec(heap, key)),
                flags,
                is_accessor,
            )
        })
        .collect()
}

fn string_keys_in_shape_order(heap: &otter_gc::GcHeap, body: &ObjectBody) -> Vec<String> {
    if !body.shape.is_null() {
        return shape_body::shape_keys_ordered(heap, body.shape)
            .into_iter()
            .map(|(key, _)| String::from_utf16_lossy(&to_utf16_vec(heap, key)))
            .collect();
    }
    body.dict_keys().map_or_else(Vec::new, |table| {
        (0..table.len())
            .map(|i| table.key_at(i).to_string())
            .collect()
    })
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

/// Materialize the existing slots' metadata from the hidden class for an append
/// that normalizes a shaped object to dictionary storage. Returns `Some` only
/// when appending (`existing_offset` is `None`) to a shaped object; dictionary
/// objects already carry materialized metadata and shaped redefines use a shape
/// transition instead. The returned vector is installed into
/// [`ExoticSlots::slots`] before the new slot is pushed so the dictionary-mode
/// object's per-slot metadata stays index-aligned with its value array.
fn slot_metas_for_shape_transition(
    heap: &otter_gc::GcHeap,
    obj: JsObject,
    existing_offset: Option<u16>,
) -> Option<Vec<SlotMeta>> {
    if existing_offset.is_some() {
        return None;
    }
    heap.read_payload(obj, |body| {
        (!body.shape.is_null()).then(|| materialized_slot_metas(heap, body))
    })
}

/// Read every current slot's `(flags, is_accessor)` from the authoritative
/// source into an index-aligned [`SlotMeta`] vector.
fn materialized_slot_metas(heap: &otter_gc::GcHeap, body: &ObjectBody) -> Vec<SlotMeta> {
    let count = body_property_count(heap, body);
    (0..count)
        .map(|i| {
            let (flags, is_accessor) = body.slot_attrs(heap, i);
            SlotMeta { flags, is_accessor }
        })
        .collect()
}

/// Ensure per-slot metadata is materialized in [`ExoticSlots::slots`] and the
/// object reads attributes from it (sets `slot_attrs_overridden`).
///
/// No-op when the object is already materialized — dictionary mode (null
/// shape) or a prior override. Otherwise it snapshots the shaped object's
/// per-slot attributes from the hidden class, so it must run *before* an
/// in-place attribute mutation that does not transition the class
/// (construction accessor→data overwrite, the no-shape `defineProperty` /
/// `freeze` / `seal` fallbacks, `delete`).
fn materialize_slots(obj: JsObject, heap: &mut otter_gc::GcHeap) {
    let mut obj = obj;
    materialize_slots_with_pending_values(&mut obj, heap, &mut []);
}

/// [`materialize_slots`], reflecting receiver relocation and tracing values
/// which have not entered the object yet across sidecar/table allocation.
fn materialize_slots_with_pending_values(
    obj: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    pending: &mut [Value],
) {
    // The sidecar allocates, so it is reserved here, outside the payload
    // borrow below. This may move `obj` and every pending cell value.
    ensure_exotic_with_pending_values(obj, heap, pending).expect("exotic sidecar");
    let metas = heap.read_payload(*obj, |body| {
        (!body.slots_materialized()).then(|| materialized_slot_metas(heap, body))
    });
    if let Some(metas) = metas {
        let metas_for_install = Some(metas);
        let table = slot_meta_table_for_install(obj, heap, &metas_for_install, 0, pending)
            .expect("slot meta table")
            .expect("metas present");
        heap.with_payload(*obj, |body| {
            body.exotic_mut().slots = table;
            body.slot_attrs_overridden = true;
        });
        let sidecar = heap.read_payload(*obj, |body| body.exotic.get());
        heap.record_write(sidecar, &table);
    }
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
        let string_keys = ordinary_string_key_entries(heap, body)
            .into_iter()
            .map(|(key, idx)| {
                let (flags, is_accessor) = body.slot_attrs(heap, idx);
                (key, idx, flags, is_accessor)
            })
            .collect();
        f(Properties {
            body,
            heap,
            string_keys,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_gc::GcHeap;

    fn fresh_heap() -> GcHeap {
        GcHeap::new().expect("init heap")
    }

    fn total_allocations(heap: &mut GcHeap) -> u64 {
        heap.gc_stats()
            .by_type
            .iter()
            .map(|row| row.alloc_count_total)
            .sum()
    }

    #[test]
    fn jit_semantic_guard_layout_is_frozen() {
        assert_eq!(std::mem::size_of::<ShapeCacheMode>(), 1);
        assert_eq!(SHAPE_CACHE_MODE_FAST, 0);
        assert_eq!(OBJECT_BODY_SHAPE_CACHE_MODE_OFFSET, 32);
        assert_eq!(OBJECT_BODY_EXTENSIBLE_OFFSET, 40);
        assert_eq!(OBJECT_BODY_SLOT_ATTRS_OVERRIDDEN_OFFSET, 41);
        assert_eq!(OBJECT_BODY_EXOTIC_HANDLE_OFFSET, 48);
        assert_eq!(std::mem::size_of::<ExoticHandle>(), 4);
        assert_eq!(std::mem::size_of::<bool>(), 1);
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
                .ordinary_set_data_property(o, "x", Value::boolean(true))
                .expect("set")
        );

        let shape_handle = shape(o, interp.gc_heap());
        assert_eq!(interp.shape_offset_of(shape_handle, "x"), Some(0));
        assert_eq!(
            interp
                .gc_heap()
                .read_payload(o, |body| body.dict_key_count()),
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
            .set_property(o, "value", Value::number_i32(1))
            .expect("set value");
        interp
            .set_property(o, "done", Value::boolean(false))
            .expect("set done");

        let shape_handle = shape(o, interp.gc_heap());
        assert_eq!(interp.shape_offset_of(shape_handle, "value"), Some(0));
        assert_eq!(interp.shape_offset_of(shape_handle, "done"), Some(1));
        assert_eq!(
            interp
                .gc_heap()
                .read_payload(o, |body| body.dict_key_count()),
            0
        );
    }

    #[test]
    fn runtime_construction_set_preserves_slots_when_fast_shape_overflows() {
        let mut interp = crate::Interpreter::new();
        let mut o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");

        for i in 0..MAX_FAST_PROPERTIES {
            let key = format!("p{i}");
            interp
                .set_property(o, &key, Value::number_i32(i as i32))
                .expect("set fast property");
        }
        assert!(!shape(o, interp.gc_heap()).is_null());

        set(
            &mut o,
            interp.gc_heap_mut(),
            "overflow",
            Value::boolean(true),
        );

        assert!(shape(o, interp.gc_heap()).is_null());
        assert_eq!(
            get_own(o, interp.gc_heap(), "p0"),
            Some(Value::number_i32(0))
        );
        assert_eq!(
            get_own(o, interp.gc_heap(), "p127"),
            Some(Value::number_i32(127))
        );
        assert_eq!(
            get_own(o, interp.gc_heap(), "overflow"),
            Some(Value::boolean(true))
        );
        let keys: Vec<String> = with_properties(o, interp.gc_heap(), |p| {
            p.keys().map(str::to_string).collect()
        });
        assert_eq!(keys.len(), MAX_FAST_PROPERTIES as usize + 1);
        assert_eq!(keys.first().map(String::as_str), Some("p0"));
        assert_eq!(keys.last().map(String::as_str), Some("overflow"));
    }

    #[test]
    fn shape_id_prefers_installed_shape() {
        let mut interp = crate::Interpreter::new();
        let o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");

        interp
            .set_property(o, "x", Value::boolean(true))
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
        let mut o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");

        interp
            .set_property(o, "x", Value::boolean(true))
            .expect("set x");
        interp.gc_heap_mut().with_payload(o, |body| {
            dict_clear_keys(body);
            body.dictionary_shape_id = next_shape_id();
        });

        assert_eq!(len(o, interp.gc_heap()), 1);
        assert_eq!(
            get_own(o, interp.gc_heap(), "x"),
            Some(Value::boolean(true))
        );
        assert!(matches!(
            lookup_own(o, interp.gc_heap(), "x"),
            PropertyLookup::Data { value, .. } if value.as_boolean() == Some(true)
        ));
        assert!(get_own_descriptor(o, interp.gc_heap(), "x").is_some());
        let keys: Vec<String> = with_properties(o, interp.gc_heap(), |p| {
            p.keys().map(str::to_string).collect()
        });
        assert_eq!(keys, vec!["x"]);

        set(&mut o, interp.gc_heap_mut(), "x", Value::boolean(false));

        assert_eq!(
            get_own(o, interp.gc_heap(), "x"),
            Some(Value::boolean(false))
        );
        // The object stays shaped (its hidden class is the count/attribute
        // source), so it carries no materialized per-slot metadata; the own
        // property count comes from the shape.
        assert_eq!(len(o, interp.gc_heap()), 1);
    }

    #[test]
    fn runtime_define_property_advances_shape() {
        let mut interp = crate::Interpreter::new();
        let mut o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");
        let descriptor = PartialPropertyDescriptor {
            value: Some(Value::number_i32(42)),
            writable: Some(true),
            enumerable: Some(true),
            configurable: Some(true),
            ..PartialPropertyDescriptor::default()
        };

        assert!(
            interp
                .define_own_property_partial(&mut o, "answer", descriptor)
                .expect("define")
        );

        let shape_handle = shape(o, interp.gc_heap());
        assert_eq!(interp.shape_offset_of(shape_handle, "answer"), Some(0));
        assert_eq!(
            interp
                .gc_heap()
                .read_payload(o, |body| body.dict_key_count()),
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
            .set_property(o, "a", Value::boolean(true))
            .expect("set a");
        interp.set_property(o, "b", Value::null()).expect("set b");

        let before = shape(o, interp.gc_heap());
        assert!(!before.is_null());
        assert_eq!(interp.shape_offset_of(before, "b"), Some(1));

        assert!(delete(o, interp.gc_heap_mut(), "a"));

        assert!(shape(o, interp.gc_heap()).is_null());
        assert!(get(o, interp.gc_heap(), "a").is_none());
        assert!(get(o, interp.gc_heap(), "b").is_some_and(|v| v.is_null()));
    }

    #[test]
    fn runtime_store_transition_invalidates_shape() {
        let mut interp = crate::Interpreter::new();
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_global(7)),
            "x",
        );
        let first = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("first object");

        let transition = capture_store_property_transition(
            first,
            interp.gc_heap_mut(),
            key,
            &Value::boolean(true),
        )
        .expect("transition install");

        assert!(shape(first, interp.gc_heap()).is_null());
        assert_eq!(
            get_own(first, interp.gc_heap(), "x"),
            Some(Value::boolean(true))
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
                &Value::null(),
            ),
            Some(())
        );

        assert!(shape(second, interp.gc_heap()).is_null());
        assert_eq!(get_own(second, interp.gc_heap(), "x"), Some(Value::null()));
    }

    #[test]
    fn raw_set_invalidates_shape_for_new_property() {
        let mut interp = crate::Interpreter::new();
        let mut o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");
        assert_eq!(shape(o, interp.gc_heap()), interp.shape_root());

        set(&mut o, interp.gc_heap_mut(), "x", Value::boolean(true));

        assert!(shape(o, interp.gc_heap()).is_null());
        assert_eq!(
            get_own(o, interp.gc_heap(), "x"),
            Some(Value::boolean(true))
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
            Value::boolean(true)
        ));

        assert!(shape(o, interp.gc_heap()).is_null());
        assert_eq!(
            get_own(o, interp.gc_heap(), "x"),
            Some(Value::boolean(true))
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
            PropertyDescriptor::data(Value::boolean(true), true, true, true),
        ));

        assert!(shape(o, interp.gc_heap()).is_null());
        assert_eq!(
            get_own(o, interp.gc_heap(), "x"),
            Some(Value::boolean(true))
        );
    }

    #[test]
    fn raw_define_property_partial_invalidates_shape_for_new_property() {
        let mut interp = crate::Interpreter::new();
        let mut o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");
        assert_eq!(shape(o, interp.gc_heap()), interp.shape_root());
        let descriptor = PartialPropertyDescriptor {
            value: Some(Value::boolean(true)),
            writable: Some(true),
            enumerable: Some(true),
            configurable: Some(true),
            ..PartialPropertyDescriptor::default()
        };

        assert!(define_own_property_partial(
            &mut o,
            interp.gc_heap_mut(),
            "x",
            descriptor,
        ));

        assert!(shape(o, interp.gc_heap()).is_null());
        assert_eq!(
            get_own(o, interp.gc_heap(), "x"),
            Some(Value::boolean(true))
        );
    }

    #[test]
    fn set_then_get_roundtrip() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(&mut o, &mut heap, "x", Value::boolean(true));
        assert!(get(o, &heap, "x").is_some_and(|v| v.as_boolean() == Some(true)));
    }

    #[test]
    fn atom_lookup_reports_shape_and_slot_metadata() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(&mut o, &mut heap, "x", Value::boolean(true));
        let shape = shape_id(o, &heap);
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_global(7)),
            "x",
        );

        let hit = lookup_own_atom(o, &heap, key);

        assert_eq!(
            hit.hit,
            Some(AtomOwnPropertyHit {
                shape_id: shape,
                // `set` of a new key moves the object to dictionary mode
                // (null shape handle).
                shape: ShapeHandle::null(),
                atom_id: key.atom().id(),
                slot: 0,
                is_data: true,
            })
        );
        assert!(matches!(
            hit.lookup,
            PropertyLookup::Data { value, .. } if value.as_boolean() == Some(true)
        ));
    }

    #[test]
    fn atom_slot_guard_rejects_shape_change() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(&mut o, &mut heap, "x", Value::boolean(true));
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_global(7)),
            "x",
        );
        let hit = lookup_own_atom(o, &heap, key).hit.expect("atom hit");
        assert_eq!(
            load_own_data_slot_atom(o, &heap, key, hit),
            Some(Value::boolean(true))
        );

        set(&mut o, &mut heap, "y", Value::null());

        assert_eq!(load_own_data_slot_atom(o, &heap, key, hit), None);
    }

    #[test]
    fn atom_slot_store_updates_guarded_data_slot() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(&mut o, &mut heap, "x", Value::boolean(true));
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_global(7)),
            "x",
        );
        let hit = lookup_own_atom(o, &heap, key).hit.expect("atom hit");

        let allocations_before_hit = total_allocations(&mut heap);
        assert_eq!(
            store_own_data_slot_atom(o, &mut heap, key, hit, &Value::number_f64(1.25)),
            Some(())
        );
        assert_eq!(
            total_allocations(&mut heap),
            allocations_before_hit,
            "a matching store IC writes the complete numeric Value without allocation"
        );
        assert_eq!(
            load_own_data_slot_atom(o, &heap, key, hit)
                .and_then(|value| value.as_number())
                .map(|number| number.as_f64()),
            Some(1.25)
        );

        set(&mut o, &mut heap, "y", Value::null());

        let allocations_before_miss = total_allocations(&mut heap);
        assert_eq!(
            store_own_data_slot_atom(o, &mut heap, key, hit, &Value::number_f64(1.25)),
            None
        );
        assert_eq!(
            total_allocations(&mut heap),
            allocations_before_miss,
            "a rejected store IC must not allocate"
        );
    }

    #[test]
    fn raw_atom_add_transition_rejects_unshared_dictionary_shape() {
        let mut heap = fresh_heap();
        let proto = alloc_object_old_for_fixture(&mut heap).unwrap();
        let first = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(first, &mut heap, Some(proto));
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_global(7)),
            "x",
        );
        let transition =
            capture_store_property_transition(first, &mut heap, key, &Value::boolean(true))
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
                &Value::boolean(false),
            ),
            None
        );
        assert_eq!(get_own(second, &heap, "x"), None);
    }

    #[test]
    fn atom_add_transition_rejects_changed_direct_prototype_shape() {
        let mut heap = fresh_heap();
        let mut proto = alloc_object_old_for_fixture(&mut heap).unwrap();
        let first = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(first, &mut heap, Some(proto));
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_global(7)),
            "x",
        );
        let transition =
            capture_store_property_transition(first, &mut heap, key, &Value::boolean(true))
                .expect("transition install");
        set(&mut proto, &mut heap, "x", Value::null());

        let second = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(second, &mut heap, Some(proto));

        let allocations_before_miss = total_allocations(&mut heap);
        assert_eq!(
            replay_store_property_transition(
                second,
                &mut heap,
                key,
                &transition,
                &Value::number_f64(1.25),
            ),
            None
        );
        assert_eq!(
            total_allocations(&mut heap),
            allocations_before_miss,
            "a rejected transition replay must not allocate"
        );
    }

    #[test]
    fn atom_add_transition_rejects_deeper_prototype_after_mutation() {
        let mut heap = fresh_heap();
        let proto = alloc_object_old_for_fixture(&mut heap).unwrap();
        let first = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(first, &mut heap, Some(proto));
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_global(7)),
            "x",
        );
        let transition =
            capture_store_property_transition(first, &mut heap, key, &Value::boolean(true))
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
                &Value::boolean(false),
            ),
            None
        );
    }

    #[test]
    fn raw_atom_add_transition_rejects_unshared_inherited_dictionary_shape() {
        let mut heap = fresh_heap();
        let mut proto = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(&mut proto, &mut heap, "x", Value::boolean(true));
        let first = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(first, &mut heap, Some(proto));
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_global(7)),
            "x",
        );
        let transition =
            capture_store_property_transition(first, &mut heap, key, &Value::boolean(false))
                .expect("transition install");
        assert!(matches!(
            transition.kind,
            StorePropertyTransitionKind::DirectPrototypeWritableData { .. }
        ));

        let second = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(second, &mut heap, Some(proto));

        assert_eq!(
            replay_store_property_transition(second, &mut heap, key, &transition, &Value::null(),),
            None
        );
        assert_eq!(get_own(second, &heap, "x"), None);
        assert_eq!(get_own(proto, &heap, "x"), Some(Value::boolean(true)));
    }

    #[test]
    fn atom_add_transition_rejects_inherited_data_after_writable_change() {
        let mut heap = fresh_heap();
        let mut proto = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(&mut proto, &mut heap, "x", Value::boolean(true));
        let first = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(first, &mut heap, Some(proto));
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_global(7)),
            "x",
        );
        let transition =
            capture_store_property_transition(first, &mut heap, key, &Value::boolean(false))
                .expect("transition install");
        assert!(define_own_property(
            proto,
            &mut heap,
            "x",
            PropertyDescriptor::data(Value::boolean(true), false, true, true),
        ));

        let second = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(second, &mut heap, Some(proto));

        assert_eq!(
            replay_store_property_transition(second, &mut heap, key, &transition, &Value::null(),),
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
            PropertyDescriptor::data(Value::boolean(true), false, true, true),
        ));
        let receiver = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(receiver, &mut heap, Some(proto));
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_global(7)),
            "x",
        );

        assert!(
            capture_store_property_transition(receiver, &mut heap, key, &Value::null()).is_none()
        );
        assert!(get_own(receiver, &heap, "x").is_none());
    }

    #[test]
    fn shape_id_changes_on_new_property_not_overwrite() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        let empty = shape_id(o, &heap);
        set(&mut o, &mut heap, "x", Value::boolean(true));
        let with_x = shape_id(o, &heap);
        set(&mut o, &mut heap, "x", Value::boolean(false));

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
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(&mut o, &mut heap, "a", Value::boolean(true));
        set(&mut o, &mut heap, "b", Value::boolean(false));
        set(&mut o, &mut heap, "c", Value::null());
        let keys: Vec<String> =
            with_properties(o, &heap, |p| p.keys().map(str::to_string).collect());
        assert_eq!(keys, vec!["a", "b", "c"]);
    }

    #[test]
    fn integer_index_keys_sort_before_strings() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(&mut o, &mut heap, "b", Value::boolean(true));
        set(&mut o, &mut heap, "10", Value::boolean(true));
        set(&mut o, &mut heap, "2", Value::boolean(true));
        set(&mut o, &mut heap, "a", Value::boolean(true));
        set(&mut o, &mut heap, "1", Value::boolean(true));
        set(&mut o, &mut heap, "01", Value::boolean(true));
        set(&mut o, &mut heap, "4294967295", Value::boolean(true));

        let keys: Vec<String> =
            with_properties(o, &heap, |p| p.keys().map(str::to_string).collect());
        assert_eq!(keys, vec!["1", "2", "10", "b", "a", "01", "4294967295"]);
    }

    #[test]
    fn delete_removes_property() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(&mut o, &mut heap, "x", Value::boolean(true));
        assert!(delete(o, &mut heap, "x"));
        assert!(get(o, &heap, "x").is_none());
        // §10.1.10 — deleting a missing property still reports
        // success (returns true).
        assert!(delete(o, &mut heap, "x"));
    }

    #[test]
    fn handle_copy_shares_storage() {
        let mut heap = fresh_heap();
        let mut a = alloc_object_old_for_fixture(&mut heap).unwrap();
        let b = a; // Copy
        set(&mut a, &mut heap, "x", Value::boolean(true));
        assert_eq!(a, b);
        assert!(get(b, &heap, "x").is_some_and(|v| v.as_boolean() == Some(true)));
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Counter {
        value: u32,
    }

    impl HostObjectData for Counter {}

    struct WrongHostData(String);

    impl HostObjectData for WrongHostData {}

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
        let wrong = WrongHostData("not a counter".to_string());
        assert_eq!(wrong.0, "not a counter");
        let object = alloc_host_object_with_roots(&mut heap, wrong, &mut roots).unwrap();
        let err = with_host_data::<Counter, _>(object, &heap, |_| ()).unwrap_err();
        assert!(matches!(err, HostObjectError::TypeMismatch { .. }));
    }

    #[test]
    fn overwrite_does_not_grow_shape() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(&mut o, &mut heap, "x", Value::boolean(true));
        let s1 = shape_id(o, &heap);
        set(&mut o, &mut heap, "x", Value::null());
        let s2 = shape_id(o, &heap);
        assert_eq!(s1, s2);
        assert_eq!(len(o, &heap), 1);
    }

    #[test]
    fn delete_switches_to_dictionary_shape() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(&mut o, &mut heap, "a", Value::boolean(true));
        set(&mut o, &mut heap, "b", Value::null());
        let before = shape_id(o, &heap);
        assert!(supports_fast_property_ic(o, &heap));
        delete(o, &mut heap, "a");
        let after = shape_id(o, &heap);
        assert_ne!(before, after);
        assert!(!supports_fast_property_ic(o, &heap));
        assert_eq!(len(o, &heap), 1);
        assert!(get(o, &heap, "a").is_none());
        assert!(get(o, &heap, "b").is_some_and(|v| v.is_null()));
    }

    #[test]
    fn delete_middle_preserves_later_dictionary_offsets() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(&mut o, &mut heap, "locale", Value::number_i32(1));
        set(&mut o, &mut heap, "style", Value::number_i32(2));
        set(&mut o, &mut heap, "type", Value::number_i32(3));
        set(&mut o, &mut heap, "fallback", Value::number_i32(4));
        set(&mut o, &mut heap, "languageDisplay", Value::number_i32(5));

        assert!(delete(o, &mut heap, "style"));

        assert!(get(o, &heap, "style").is_none());
        assert_eq!(
            get(o, &heap, "type").and_then(|v| v.as_number()),
            Some(NumberValue::from_i32(3))
        );
        assert_eq!(
            get(o, &heap, "fallback").and_then(|v| v.as_number()),
            Some(NumberValue::from_i32(4))
        );
        assert_eq!(
            get(o, &heap, "languageDisplay").and_then(|v| v.as_number()),
            Some(NumberValue::from_i32(5))
        );
    }

    #[test]
    fn overwrite_then_delete_middle_preserves_later_dictionary_offsets() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(&mut o, &mut heap, "locale", Value::number_i32(1));
        set(&mut o, &mut heap, "style", Value::number_i32(2));
        set(&mut o, &mut heap, "type", Value::number_i32(3));
        set(&mut o, &mut heap, "fallback", Value::number_i32(4));
        set(&mut o, &mut heap, "languageDisplay", Value::number_i32(5));

        set(&mut o, &mut heap, "style", Value::number_i32(20));
        set(&mut o, &mut heap, "style", Value::number_i32(2));
        assert!(delete(o, &mut heap, "style"));

        assert!(get(o, &heap, "style").is_none());
        assert_eq!(
            get(o, &heap, "type").and_then(|v| v.as_number()),
            Some(NumberValue::from_i32(3))
        );
        assert_eq!(
            get(o, &heap, "fallback").and_then(|v| v.as_number()),
            Some(NumberValue::from_i32(4))
        );
        assert_eq!(
            get(o, &heap, "languageDisplay").and_then(|v| v.as_number()),
            Some(NumberValue::from_i32(5))
        );
    }

    #[test]
    fn sequential_middle_deletes_preserve_later_dictionary_offsets() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(&mut o, &mut heap, "locale", Value::number_i32(1));
        set(&mut o, &mut heap, "style", Value::number_i32(2));
        set(&mut o, &mut heap, "type", Value::number_i32(3));
        set(&mut o, &mut heap, "fallback", Value::number_i32(4));
        set(&mut o, &mut heap, "languageDisplay", Value::number_i32(5));

        assert!(delete(o, &mut heap, "style"));
        assert_eq!(
            get(o, &heap, "type").and_then(|v| v.as_number()),
            Some(NumberValue::from_i32(3))
        );
        assert_eq!(
            get(o, &heap, "fallback").and_then(|v| v.as_number()),
            Some(NumberValue::from_i32(4))
        );
        assert!(delete(o, &mut heap, "type"));

        assert!(get(o, &heap, "style").is_none());
        assert!(get(o, &heap, "type").is_none());
        assert_eq!(
            get(o, &heap, "fallback").and_then(|v| v.as_number()),
            Some(NumberValue::from_i32(4))
        );
        assert_eq!(
            get(o, &heap, "languageDisplay").and_then(|v| v.as_number()),
            Some(NumberValue::from_i32(5))
        );
    }

    #[test]
    fn sequential_middle_deletes_preserve_later_offsets_without_tail_optional() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(&mut o, &mut heap, "locale", Value::number_i32(1));
        set(&mut o, &mut heap, "style", Value::number_i32(2));
        set(&mut o, &mut heap, "type", Value::number_i32(3));
        set(&mut o, &mut heap, "fallback", Value::number_i32(4));

        assert!(delete(o, &mut heap, "style"));
        assert!(delete(o, &mut heap, "type"));

        assert!(get(o, &heap, "style").is_none());
        assert!(get(o, &heap, "type").is_none());
        assert_eq!(
            get(o, &heap, "fallback").and_then(|v| v.as_number()),
            Some(NumberValue::from_i32(4))
        );
    }

    #[test]
    fn overwrite_between_middle_deletes_preserves_later_dictionary_offsets() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(&mut o, &mut heap, "locale", Value::number_i32(1));
        set(&mut o, &mut heap, "style", Value::number_i32(2));
        set(&mut o, &mut heap, "type", Value::number_i32(3));
        set(&mut o, &mut heap, "fallback", Value::number_i32(4));

        assert!(delete(o, &mut heap, "style"));
        set(&mut o, &mut heap, "type", Value::number_i32(30));
        set(&mut o, &mut heap, "type", Value::number_i32(3));
        assert_eq!(
            get(o, &heap, "fallback").and_then(|v| v.as_number()),
            Some(NumberValue::from_i32(4))
        );
        assert!(delete(o, &mut heap, "type"));

        assert!(get(o, &heap, "style").is_none());
        assert!(get(o, &heap, "type").is_none());
        assert_eq!(
            get(o, &heap, "fallback").and_then(|v| v.as_number()),
            Some(NumberValue::from_i32(4))
        );
    }

    #[test]
    fn define_property_with_default_attrs() {
        let mut heap = fresh_heap();
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
        let desc = PropertyDescriptor::data(Value::boolean(true), false, false, false);
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
            PropertyDescriptor::data(Value::boolean(true), true, true, false),
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
            PropertyDescriptor::data(Value::boolean(false), true, false, false),
        ));

        assert!(ordinary_set_data_property(
            o,
            &mut heap,
            "x",
            Value::boolean(true)
        ));

        let got = get_own_descriptor(o, &heap, "x").unwrap();
        assert!(get(o, &heap, "x").is_some_and(|v| v.as_boolean() == Some(true)));
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
            PropertyDescriptor::data(Value::boolean(false), false, true, true),
        ));

        assert!(!ordinary_set_data_property(
            o,
            &mut heap,
            "x",
            Value::boolean(true)
        ));

        assert!(get(o, &heap, "x").is_some_and(|v| v.as_boolean() == Some(false)));
    }

    #[test]
    fn ordinary_set_data_property_respects_extensibility_for_new_keys() {
        let mut heap = fresh_heap();
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();

        assert!(ordinary_set_data_property(o, &mut heap, "x", Value::null()));
        assert!(get(o, &heap, "x").is_some_and(|v| v.is_null()));

        prevent_extensions(o, &mut heap);
        assert!(!ordinary_set_data_property(
            o,
            &mut heap,
            "y",
            Value::boolean(true)
        ));
        assert!(get(o, &heap, "y").is_none());
    }

    #[test]
    fn freeze_makes_object_non_writable() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(&mut o, &mut heap, "x", Value::boolean(true));
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
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        set(&mut o, &mut heap, "a", Value::null());
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
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
        define_own_property(
            o,
            &mut heap,
            "x",
            PropertyDescriptor::data(Value::boolean(true), true, true, false),
        );
        assert!(!delete(o, &mut heap, "x"));
        assert!(get(o, &heap, "x").is_some());
    }

    #[test]
    fn transactional_delete_is_identity_guarded_and_ignores_configurable() {
        let mut heap = fresh_heap();
        let mut cache = alloc_object_old_for_fixture(&mut heap).unwrap();
        let record = alloc_object_old_for_fixture(&mut heap).unwrap();
        let replacement = alloc_object_old_for_fixture(&mut heap).unwrap();
        let record_value = Value::object(record);
        let replacement_value = Value::object(replacement);

        define_own_property(
            cache,
            &mut heap,
            "module",
            PropertyDescriptor::data(record_value, true, true, false),
        );
        assert!(delete_if_same_data(
            cache,
            &mut heap,
            "module",
            record_value
        ));
        assert!(get(cache, &heap, "module").is_none());

        set(&mut cache, &mut heap, "module", replacement_value);
        assert!(!delete_if_same_data(
            cache,
            &mut heap,
            "module",
            record_value
        ));
        assert_eq!(get(cache, &heap, "module"), Some(replacement_value));
    }

    #[test]
    fn delete_symbol_missing_key_succeeds() {
        let mut heap = fresh_heap();
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
        let sym = JsSymbol::new(&mut heap, None).unwrap();
        assert!(delete_symbol(o, &mut heap, sym));
    }
}
