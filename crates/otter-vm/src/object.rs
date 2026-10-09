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
//! the type system. Assignment takes the actual mutable receiver slot and
//! returns `Result<bool, OutOfMemory>`: rejection and allocator refusal remain
//! distinct. Construction enters descriptor installation with explicit flags.
//! No thread-local heap lookup is permitted in this module.
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
//!   guarded StoreProperty replay records and
//!   their allocation-free native subset.
//! - [`ShapeState`] / [`LookupFact`] — immutable hidden-class semantics.
//! - `ordinary_set` and `descriptor_install` — assignment gates and the sole
//!   rooted descriptor publication owner.
//! - `symbol_table` — one managed ordered descriptor table for objects/arrays.
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
//! - Runtime ICs and native guards treat provisional lineages as ordinary
//!   shaped layouts; only allocation plans reject a provisional root. Delete
//!   changes the actual shape or dictionary identity without a separate latch.
//! - Generated shape proofs reject opaque chain links: non-ordinary
//!   prototypes, String-wrapper virtual keys, and host payloads whose property
//!   semantics can substitute values outside the ordinary slot table.
//! - Runtime transaction rollback may force-remove only the own data slot that
//!   still holds its expected published value; it never invokes an accessor or
//!   removes a replacement installed by re-entrant code.
//! - Every object has a shape, and the shape fixes its `[[Prototype]]`
//!   (V8 maps, JSC structures): a keyed shape of its prototype's lineage, or
//!   that lineage's dictionary shape in dictionary mode. A prototype change is
//!   a shape change; nothing else stores the prototype.
//! - A shape fixes inline capacity as well as prototype. Overflow slabs hold
//!   only the suffix; inline prefix words never migrate. Capacity roots are
//!   cached per prototype, capacity and complete immutable state.
//! - GC shape bodies are immutable after allocation; transition tables and
//!   offset maps live in interpreter-owned side caches.
//! - Dictionary string-slot redefinitions and bulk integrity changes retire
//!   the structural identity and any changed watched-slot layout proof before
//!   publishing descriptors. A bulk change retires each proof domain once.
//! - Every store of a `Gc<…>`-bearing `Value` into a slot, every
//!   prototype-changing shape install, and every symbol-property write records
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
use std::sync::atomic::{AtomicU64, Ordering};

use crate::Value;
use crate::bigint::BigIntValue;
use crate::number::NumberValue;
use crate::property_atom::{AtomId, AtomizedPropertyKey};
use crate::proxy::JsProxy;
use crate::string::{JsString, to_utf16_vec};
use crate::symbol::JsSymbol;
use otter_gc::GcHeap;
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::{RawGc, SlotVisitor};
use smallvec::SmallVec;

mod descriptor;
mod descriptor_core;
pub(crate) use descriptor_core::validate_descriptor_partial;
mod descriptor_install;
mod descriptor_mutation;
mod field_location;
mod ordinary_set;
mod own_slot_cache;
pub(crate) use own_slot_cache::OwnSlotCache;
pub(crate) mod symbol_table;
pub use field_location::{FieldLayout, FieldLocation};
use symbol_table::{SymbolPropsBody, SymbolPropsHandle, body_of as symbol_props_body_of};
mod integrity_transition;
mod key_order;
mod lookup;
#[cfg(test)]
mod persistent_fields_tests;
pub(crate) mod prototype_validity;
pub(crate) mod shape_body;
mod shape_cache;
#[cfg(test)]
mod shape_lookup_tests;
mod shape_runtime;
mod shape_state;
mod state_transition;
pub use shape_state::{LookupFact, ShapeState};
mod shape_transition;
#[cfg(test)]
mod shaped_fixtures;
#[cfg(test)]
pub(crate) use shaped_fixtures::append_shaped_data_for_fixture;
pub mod slot_slab;

pub use descriptor::{
    DescriptorKind, PartialPropertyDescriptor, PropertyDescriptor, PropertyFlags,
};
pub(crate) use key_order::array_index_property_name;
pub use lookup::{PropertyLookup, SetOutcome, SetRejectReason};
pub(crate) use shape_body::ShapeBody;
pub(crate) use shape_body::ShapeHandle;
pub(crate) use shape_body::shape_offset_of_str;
pub(crate) use shape_body::{
    SHAPE_BODY_ID_OFFSET, SHAPE_BODY_INLINE_CAPACITY_OFFSET, SHAPE_BODY_PROPERTY_COUNT_OFFSET,
    SHAPE_BODY_PROTOTYPE_OFFSET, SHAPE_BODY_STATE_OFFSET,
};
pub(crate) use shape_runtime::ShapeRuntime;
#[cfg(test)]
pub(crate) use shape_transition::capture_store_property_transition;
pub(crate) use shape_transition::{
    StorePropertyTransition, StorePropertyTransitionKind,
    capture_store_property_transition_with_shape, replay_store_property_transition,
};

/// Id 0 is reserved: [`ShapeId::UNASSIGNED`].
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

/// One mapped arguments index aliased to a slot of the parameter-scope
/// context (§10.4.4.7 CreateMappedArgumentsObject ParameterMap).
#[derive(Debug, Clone)]
pub(crate) struct MappedArgumentEntry {
    pub(crate) key: String,
    pub(crate) slot: u16,
}

/// The ParameterMap of a mapped arguments object: every entry aliases a slot
/// of the one parameter-scope context.
#[derive(Debug, Clone)]
pub(crate) struct MappedArguments {
    pub(crate) context: crate::context::ContextHandle,
    pub(crate) entries: Vec<MappedArgumentEntry>,
}

#[derive(Debug)]
struct MappedArgumentsData {
    context: crate::context::ContextHandle,
    entries: Box<[MappedArgumentEntry]>,
}

/// A mapped argument's aliased binding: the parameter-scope context and slot.
type MappedCell = (crate::context::ContextHandle, u16);

fn mapped_read(heap: &otter_gc::GcHeap, (context, slot): MappedCell) -> Value {
    crate::context::read_slot(heap, context, slot).unwrap_or_else(Value::undefined)
}

fn mapped_write(heap: &mut otter_gc::GcHeap, (context, slot): MappedCell, value: Value) {
    crate::context::write_slot(heap, context, slot, value);
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
/// Next free tag after `EVAL_EXTENSION_BODY_TYPE_TAG = 0x2E`; distinct from every
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
    /// authoritative source for dictionary-mode
    /// objects whose slots have diverged from the shape.
    is_accessor: bool,
    /// `true` once a compiled proof reads this dictionary slot's value
    /// directly (see [`watch_dictionary_slot`]). Redefining a watched slot's
    /// kind or attributes advances the object's slot-layout epoch; an
    /// unwatched slot's redefinition leaves every other key's proof intact.
    watched: bool,
}

impl SlotMeta {
    /// Metadata for a default-attributes data slot
    /// (`writable / enumerable / configurable` all `true`).
    fn data_default() -> Self {
        Self {
            flags: PropertyFlags::data_default(),
            is_accessor: false,
            watched: false,
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
                    watched: false,
                },
                self.value,
            )),
            SlotKind::Accessor(pair) => {
                let cell = alloc_accessor_cell(heap, obj, pair.getter, pair.setter)?;
                Ok((
                    SlotMeta {
                        flags: self.flags,
                        is_accessor: true,
                        watched: false,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(transparent)]
pub(crate) struct ShapeId(u64);

impl ShapeId {
    /// No identity: an empty cache way, or a sidecar that never held a
    /// dictionary object's identity.
    pub(crate) const UNASSIGNED: Self = Self(0);

    /// Raw VM-local id. Exposed to the [`crate::inspect`] snapshot
    /// surface so embedder DTOs can carry a stable identity without
    /// publishing the wrapper type itself.
    #[must_use]
    pub(crate) const fn raw(self) -> u64 {
        self.0
    }

    /// Rebuild a stable VM-local identity in tests.
    #[cfg(test)]
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
#[repr(C)]
pub(crate) struct AtomOwnPropertyHit {
    /// Shape observed on the receiver object.
    pub(crate) shape_id: ShapeId,
    /// GC handle of the observed shape, the fast reject of a cached hit.
    /// Not traced: hidden classes are collectable and a later shape may take
    /// a collected one's cell, so every consumer also compares
    /// [`Self::shape_id`], which is never reused (the shared lookup cache is
    /// pruned instead, because generated probes compare only the handle).
    /// `Gc::null()` in dictionary mode.
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
/// a list of symbol-keyed own properties. Its hidden class owns immutable
/// extensibility and lookup facts. Mutation flows through [`otter_gc::GcHeap::with_payload`]
/// (writers) and reads through [`otter_gc::GcHeap::read_payload`]
/// (readers). Every store of a `Gc<…>`-bearing field is recorded through
/// [`otter_gc::GcHeap::record_write`].
///
/// The fixed body is 16 bytes — shape, slab and sidecar handles plus padding —
/// and is followed in the same cell by [`Self::inline_capacity`] in-object
/// value slots, sized per allocation site (object literals by their property
/// count, constructor receivers by the learned field count): the JSC
/// `JSFinalObject` / V8 in-object layout. The shape owns immutable capacity;
/// immutable semantic state lives in its hidden class; GC body-owned bytes are zero
/// ([`otter_gc::header::HEADER_BODY_BYTES_OFFSET`]). The live slot count is the shape's property
/// count, or the sidecar's dictionary slot count for a dictionary-mode
/// object, so appending to a shaped object stores a slot and a shape and
/// nothing else.
/// String-keyed slot `i` stays inline when `i < shape.inline_capacity`.
/// Otherwise it occupies suffix word `i - shape.inline_capacity`. Slab growth
/// never moves or copies the inline prefix.
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots>
/// - <https://tc39.es/ecma262/#sec-ordinarypreventextensions>
#[repr(C, align(8))]
pub struct ObjectBody {
    /// GC-managed hidden class for fast ordinary objects. First field so
    /// the JIT can read the shape token at a fixed byte offset
    /// ([`OBJECT_BODY_SHAPE_OFFSET`]) for monomorphic guard checks.
    shape: ShapeHandle,
    /// Out-of-line string-keyed own-property values once the object grows
    /// past its in-object capacity, indexed relative to the overflow suffix. A data slot
    /// stores its `[[Value]]` directly; an accessor slot stores a handle to
    /// its [`AccessorCellBody`]. Slot flags and data/accessor kind live in
    /// the shape for ordinary shaped objects, or in materialized metadata
    /// for dictionary objects.
    ///
    /// Null until an overflow suffix is needed. The slab is a GC body carrying its
    /// words in the same cell ([`slot_slab`]), not a `Vec`: an object that
    /// owned malloc storage could not be captured into a page image, and a
    /// restored copy would alias the original buffer.
    slab: slot_slab::SlotSlabHandle,
    /// Lazily-allocated rare/exotic slots — symbol-keyed properties, host
    /// data, native `[[Call]]`/`[[Construct]]`, primitive-wrapper internal
    /// slots, the Date/Error/raw-JSON/arguments markers, and a dictionary-mode
    /// object's keys, slot count and structural identity. Null for plain
    /// objects and class instances (the overwhelming common case), so an
    /// ordinary object never pays for these ~140 bytes.
    exotic: ExoticSlot,
}

impl ObjectBody {
    /// Retire dependent proofs before changing a watched prototype.
    #[inline]
    fn invalidate_prototype_proofs(&self) {
        if self.state().is_prototype() {
            self.exotic()
                .expect("prototype sidecar")
                .prototype_watchpoints
                .invalidate();
        }
    }

    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        for value in &self.inline_values()[..self.slot_count().min(self.inline_capacity())] {
            crate::code_liveness::visit_value(value, visitor);
        }
    }

    /// The cell header in front of this body. Valid only for a body that
    /// lives in its heap cell. Pending object bodies never use this accessor;
    /// allocation initializes the two body-owned header bytes to zero.
    #[inline]
    fn cell_header(&self) -> *mut otter_gc::GcHeader {
        // SAFETY: address computation only; every heap-resident body follows
        // its header in the same cell.
        unsafe {
            (self as *const Self)
                .cast::<u8>()
                .sub(otter_gc::header::HEADER_SIZE)
                .cast::<otter_gc::GcHeader>()
                .cast_mut()
        }
    }

    /// Immutable semantic facts of this exact hidden class.
    #[inline]
    pub fn state(&self) -> ShapeState {
        shape_body::state_of(self.shape)
    }

    /// `[[Extensible]]`, owned by the hidden class.
    #[inline]
    pub(crate) fn extensible(&self) -> bool {
        self.state().is_extensible()
    }

    /// Whether ordinary named-slot lookup is insufficient for this object.
    #[inline]
    pub(crate) fn chain_link_opaque(&self) -> bool {
        self.state().is_opaque()
    }

    /// Number of in-object value slots in this cell.
    #[inline]
    pub(crate) fn inline_capacity(&self) -> usize {
        shape_body::inline_capacity_of(self.shape)
    }

    /// Write the header bytes of a freshly allocated cell, before any
    /// collection can observe it.
    #[inline]
    fn init_cell_bytes(&mut self) {
        // SAFETY: called by the allocation initializer on the body's final
        // address.
        unsafe { otter_gc::GcHeader::set_body_bytes(self.cell_header(), [0, 0]) };
        self.debug_verify_field_layout();
    }

    /// Base of the in-object slots trailing the fixed body.
    #[inline]
    fn inline_values_ptr(&self) -> *mut Value {
        // SAFETY: computes the tail address only; every allocation path
        // reserves `inline_capacity` words after the fixed body.
        unsafe {
            self.cell_header()
                .cast::<u8>()
                .add(FieldLayout::current().inline_values_byte as usize)
                .cast()
        }
    }

    /// Persistent in-object words; only the counted prefix holds live fields.
    #[inline]
    fn inline_values(&self) -> &[Value] {
        // SAFETY: an allocated body owns `inline_capacity` words after the
        // fixed part. All spare words start as undefined; only live fields
        // are traced or read as slots.
        unsafe { std::slice::from_raw_parts(self.inline_values_ptr(), self.inline_capacity()) }
    }
}

/// Largest shape-owned persistent inline prefix. Larger objects retain this
/// prefix and keep only the remaining fields in an overflow slab.
pub(crate) const MAX_INLINE_CAPACITY: usize = 64;

/// Persistent prefix capacity for empty objects without a size hint:
/// `{}`, `Object.create`, and incrementally built runtime records.
pub(crate) const DEFAULT_INLINE_CAPACITY: usize = 4;

/// In-object capacity for an allocation site that knows its property count.
#[inline]
pub(crate) fn inline_capacity_for(count: usize) -> usize {
    count.min(MAX_INLINE_CAPACITY)
}

/// In-object capacity for a constructor receiver expected to hold `fields`
/// own properties: exactly that many, or the empty-object default while the
/// constructor's instance size is still unknown.
#[inline]
pub(crate) fn receiver_inline_capacity(fields: usize) -> usize {
    if fields == 0 {
        DEFAULT_INLINE_CAPACITY
    } else {
        inline_capacity_for(fields)
    }
}

/// Cell bytes of an ordinary object with `capacity` in-object slots,
/// header included.
#[inline]
pub(crate) const fn object_cell_bytes(capacity: usize) -> usize {
    FieldLayout::current().cell_bytes(capacity)
}

/// Rarely-used `ObjectBody` slots, boxed out of the hot object so plain
/// objects stay small. Every field here is absent on a plain `{}` / class
/// instance; presence implies a wrapper object, host object, callable/
/// constructor builtin, Date, Error, raw-JSON, or arguments exotic.
/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`ExoticSlots`].
pub const EXOTIC_SLOTS_TYPE_TAG: u8 = 0x37;

/// Handle to an object's rare/exotic sidecar.
pub type ExoticHandle = otter_gc::Gc<ExoticSlots>;

/// The sidecar handle as [`ObjectBody`] stores it: one compressed handle,
/// zero when the object has no sidecar.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ExoticSlot {
    handle: ExoticHandle,
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
/// objects stay small. GC handles live in this sidecar; host payloads and
/// prototype watchpoints own Rust storage and are severed on snapshot restore.
#[derive(Default)]
pub struct ExoticSlots {
    /// Mutation subscriptions and the current proof of this prototype chain.
    prototype_watchpoints: prototype_validity::PrototypeWatchpoints,
    /// Root shape of the objects whose `[[Prototype]]` is this object (V8's
    /// `PrototypeInfo::ObjectCreateMap`), created on first use. The root holds
    /// this object as its prototype, so the pair lives and dies together.
    instance_root: ShapeHandle,
    /// Insertion-ordered dictionary keys with their content-hash index,
    /// in a [`DictKeysBody`] of their own — null until the object leaves
    /// fast-shape mode.
    dictionary_keys: DictKeysHandle,
    /// Materialized per-slot metadata (flags + `is_accessor` discriminator),
    /// index-aligned with the flat value array. Present and authoritative only
    /// for dictionary-mode objects. Null for ordinary shape-owned attributes in the common
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
    /// The `stack` string rendered at its first read; cleared when frames
    /// are captured again.
    error_stack_string: Option<JsString>,
    /// `[[ParameterMap]]` presence marker for arguments-exotic objects
    /// (§10.4.4); mapping data itself lives in `host_data`.
    is_arguments_object: bool,
    /// Live slot count of a dictionary-mode object (null shape). A shaped
    /// object's count is its shape's property count, so this is meaningful
    /// only while the owner is in dictionary mode.
    dictionary_slot_count: u32,
    /// Slot-layout epoch of a dictionary-mode object: which key occupies
    /// which slot with which kind and attributes. Unlike
    /// [`Self::dictionary_shape_id`] it survives appending a new key to an
    /// object already in dictionary mode — no existing key moves or changes
    /// kind — and advances on every other structural change: entering
    /// dictionary mode, deleting a key, or redefining a slot a compiled proof
    /// watches ([`watch_dictionary_slot`]). Generated proofs about one
    /// existing key (a global binding, a dictionary prototype's method) guard
    /// this word, so unrelated globals added after compilation do not retire
    /// them. `0` means "never in dictionary mode"; the counter saturates at
    /// `u32::MAX`, a value no proof may capture.
    dictionary_layout: u32,
    /// Structural identity of a dictionary-mode object, refreshed on every
    /// key-set change; [`ShapeId::UNASSIGNED`] once it adopts a shape.
    dictionary_shape_id: ShapeId,
}

impl otter_gc::trace::SeverRestoredPayload for ExoticSlots {
    /// Sever foreign ownership on a snapshot restore. Host payloads and
    /// prototype watchpoints belong to the capture isolate. Restored chains
    /// establish fresh cells and subscriptions on their first lookup.
    fn sever_restored_payload(&mut self) {
        // SAFETY: overwriting without dropping (or reading) severs the
        // alias; the capture isolate remains the owner.
        unsafe {
            std::ptr::write(&mut self.host_data, None);
            std::ptr::write(&mut self.prototype_watchpoints, Default::default());
        }
    }
}

impl otter_gc::SafeTraceable for ExoticSlots {
    const TYPE_TAG: u8 = EXOTIC_SLOTS_TYPE_TAG;

    fn trace_slots_safe(&mut self, v: &mut SlotVisitor<'_>) {
        if !self.instance_root.is_null() {
            let slot = &mut self.instance_root as *mut ShapeHandle as *mut RawGc;
            v(slot);
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
        if let Some(stack) = &mut self.error_stack_string {
            stack.trace_handle_slot(v);
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
            let p = &mut data.context as *mut crate::context::ContextHandle as *mut RawGc;
            v(p);
        }
        if let Some(data) = self.host_data.as_mut() {
            data.trace_gc_slots(v);
        }
        // Wrapper internal slots hold heap handles that may be young.
        if let Some(string) = &self.string_data {
            string.trace_handle_slot(v);
        }
        if let Some(symbol) = &self.symbol_data {
            symbol.trace_value_slots(v);
        }
        if let Some(bigint) = &mut self.bigint_data {
            bigint.trace_handle_slot(v);
        }
    }
}

impl ExoticSlots {
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
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
    fn find(&self, key: &str) -> Option<u32> {
        let bucket = (dict_key_hash(key) as u32 & self.bucket_mask) as usize;
        // SAFETY: `bucket < bucket_count`; buckets were initialised.
        let mut current = unsafe { *self.buckets_ptr().add(bucket) };
        while current != DICT_CHAIN_END {
            let index = current as usize;
            if self.key_at(index) == key {
                return Some(u32::try_from(index).expect("dictionary index exceeds u32"));
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
    name_offset: usize,
    name_len: usize,
    module_offset: usize,
    module_len: usize,
    span_lo: u32,
    span_hi: u32,
    has_source: bool,
    line_number: u32,
    start_column: u32,
    source_offset: usize,
    source_len: usize,
}

/// Captured `Error` stack frames: fixed records followed by a UTF-8
/// arena holding every frame's function/module name and the top frame's
/// captured source line. Written once
/// at capture, never mutated, no GC references — the page image
/// carries it whole where the old `Vec<StackFrameSnapshot>` (owned
/// `String`s) could not ride at all.
#[repr(C, align(8))]
pub struct ErrorStackBody {
    frame_count: usize,
    byte_len: usize,
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

    fn str_at(&self, offset: usize, len: usize) -> &str {
        // SAFETY: written from `&str` at capture; inside `byte_len`.
        unsafe {
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                self.bytes_ptr().add(offset as usize).cast_const(),
                len as usize,
            ))
        }
    }

    /// Reconstruct owned frames, admitting copied source lines before retention.
    /// SharedSource clones then keep one charge per actual copied allocation.
    fn to_frames(
        &self,
        account: &otter_resource::ResourceAccount,
    ) -> Result<Vec<crate::run_control::StackFrameSnapshot>, otter_resource::SharedSourceError>
    {
        (0..self.frame_count)
            .map(|i| {
                // SAFETY: i is inside the fully published immutable record extent.
                let record = unsafe { *self.records_ptr().add(i) };
                let module = self
                    .str_at(record.module_offset, record.module_len)
                    .to_owned();
                let source_position = if record.has_source {
                    Some(crate::ErrorSourcePosition {
                        script_name: module.clone(),
                        line_number: record.line_number,
                        start_column: record.start_column,
                        source_line: otter_resource::SharedSource::read_utf8(
                            account,
                            self.str_at(record.source_offset, record.source_len)
                                .as_bytes(),
                        )?,
                    })
                } else {
                    None
                };
                Ok(crate::run_control::StackFrameSnapshot {
                    function_id: record.function_id,
                    function_name: self.str_at(record.name_offset, record.name_len).to_owned(),
                    module,
                    span: (record.span_lo, record.span_hi),
                    source_position,
                })
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

/// Byte offset, from the sidecar's cell header, of the symbol-keyed
/// property table handle in [`ExoticSlots`]. Generated proofs that no
/// symbol-keyed own property exists read it through the object's sidecar.
pub const EXOTIC_SLOTS_SYMBOL_PROPS_BYTE: u32 =
    (otter_gc::header::HEADER_SIZE + std::mem::offset_of!(ExoticSlots, symbol_props)) as u32;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`SlotMetaBody`].
pub const SLOT_META_BODY_TYPE_TAG: u8 = 0x3c;

/// Handle to an object's materialized per-slot metadata table.
pub(crate) type SlotMetaHandle = otter_gc::Gc<SlotMetaBody>;

/// Header for materialized per-slot attribute metadata; the
/// [`SlotMeta`] records follow it in the same cell. Metadata holds no
/// GC references, so the body traces nothing — it exists purely so a
/// dictionary-mode object owns its metadata
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
    let mut table = heap.read_payload(*object, |body| {
        body.exotic().expect("reserved sidecar").symbol_props
    });
    let old = table;
    let owner_slot = std::ptr::from_mut(object).cast::<RawGc>();
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        visitor(owner_slot);
    };
    symbol_table::reserve_table(&mut table, heap, &mut visit)?;
    if table != old {
        let sidecar = heap.read_payload(*object, |body| body.exotic.get());
        heap.with_payload(sidecar, |exotic| exotic.symbol_props = table);
        heap.record_write(sidecar, &table);
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
            symbol_table::record_write(heap, table, value);
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
        // A dictionary object leaves the shared empty identity the moment it
        // can hold keys: every sidecar-bearing dictionary object has an id
        // of its own, and its slot-layout epoch starts at the first provable
        // value (appends keep it; `0` would make it unprovable forever).
        if body.is_dictionary() {
            let exotic = body.exotic_mut();
            exotic.dictionary_shape_id = next_shape_id();
            exotic.dictionary_layout = 1;
        }
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

/// Byte offset of a dictionary-mode object's slot-layout epoch inside its
/// [`ExoticSlots`] payload. Generated proofs about one existing key of a
/// dictionary-mode object compare this `u32` after proving the shape is a
/// dictionary shape and the sidecar present.
pub(crate) const EXOTIC_SLOTS_DICTIONARY_LAYOUT_OFFSET: usize =
    std::mem::offset_of!(ExoticSlots, dictionary_layout);

/// Offset of the instance root a prototype caches for its instances, which
/// generated receiver allocation installs as a fresh receiver's shape.
pub(crate) const EXOTIC_SLOTS_INSTANCE_ROOT_OFFSET: usize =
    std::mem::offset_of!(ExoticSlots, instance_root);
/// Byte offset of the 4-byte rare-state GC handle inside [`ExoticSlot`].
/// A zero word proves the complete sidecar is absent.
pub(crate) const OBJECT_BODY_EXOTIC_HANDLE_OFFSET: usize =
    std::mem::offset_of!(ObjectBody, exotic) + std::mem::offset_of!(ExoticSlot, handle);

// The JIT bakes these offsets into emitted property loads, receiver
// allocation and prototype-chain guards, so they are a frozen ABI: pin every
// one to its EXACT value (not `>=` / `%`) so an accidental field reorder is a
// compile error rather than a frozen JIT baking garbage. Update these literals
// deliberately, in lockstep with the JIT, when the body changes.
const _: () = assert!(OBJECT_BODY_SHAPE_OFFSET == 0);
const _: () = assert!(FieldLayout::current().slab_handle_byte == 12);
const _: () = assert!(OBJECT_BODY_EXOTIC_HANDLE_OFFSET == 8);
const _: () = assert!(FieldLayout::current().inline_values_byte == 24);
// The in-object slots must stay 8-aligned for the JIT's word loads.
const _: () = assert!(
    FieldLayout::current()
        .inline_values_byte
        .is_multiple_of(FieldLocation::WORD_BYTES)
);
const _: () = assert!(std::mem::align_of::<ObjectBody>() == 8);
const _: () = assert!(MAX_INLINE_CAPACITY <= u8::MAX as usize);

// Pin the fixed footprint: shape, slab and sidecar handles (the prototype is
// the shape's), padded to the slot alignment. Every in-object slot follows in
// the same cell.
const _: () = assert!(std::mem::size_of::<ObjectBody>() == 16);

impl ObjectBody {
    /// Enter (or stay in) dictionary mode for a key-set change: carry the
    /// live slot count into the sidecar, assign a fresh structural id, and
    /// publish the prepared dictionary shape with the same capacity and state.
    ///
    /// `append` marks a change that only appends a new key: when the object
    /// is already in dictionary mode every existing key keeps its slot, kind
    /// and attributes, so the slot-layout epoch
    /// ([`ExoticSlots::dictionary_layout`]) survives. Every other change —
    /// entering dictionary mode, deleting a key, clearing the key set — also
    /// advances the epoch. A dictionary object keeps its keys in the
    /// sidecar, so the sidecar exists whenever the object has a slot; an
    /// empty object without one carries its dictionary shape's own id.
    fn enter_dictionary_mode(&mut self, append: bool) {
        let advance_layout = !append || !self.is_dictionary();
        self.enter_dictionary_mode_as(next_shape_id(), advance_layout);
    }

    /// [`Self::enter_dictionary_mode`] under a structural id the caller
    /// chose (a captured transition replays its id), advancing the
    /// slot-layout epoch when `advance_layout`.
    pub(super) fn enter_dictionary_mode_as(&mut self, id: ShapeId, advance_layout: bool) {
        self.invalidate_prototype_proofs();
        let count = self.slot_count();
        let dictionary = shape_body::dictionary_of(self.shape);
        if self.exotic.is_null() {
            debug_assert_eq!(count, 0, "a dictionary object with slots owns a sidecar");
            self.shape = dictionary;
            return;
        }
        let exotic = self.exotic_mut();
        exotic.dictionary_slot_count = u32::try_from(count).expect("slot count exceeds u32");
        exotic.dictionary_shape_id = id;
        if advance_layout {
            exotic.dictionary_layout = exotic.dictionary_layout.saturating_add(1);
        }
        self.shape = dictionary;
    }

    /// Advance a dictionary-mode object's slot-layout epoch, saturating at
    /// the unprovable `u32::MAX`.
    pub(super) fn advance_dictionary_layout(&mut self) {
        if !self.exotic.is_null() {
            let exotic = self.exotic_mut();
            exotic.dictionary_layout = exotic.dictionary_layout.saturating_add(1);
        }
    }

    /// Structural id of a dictionary-mode object: the sidecar's, or the
    /// dictionary shape's own id while the object has no key.
    fn dictionary_shape_id(&self) -> ShapeId {
        let empty = shape_body::id_of(self.shape);
        exotic_body_of(self.exotic.get()).map_or(empty, |exotic| {
            // SAFETY: a non-null handle names a live sidecar payload.
            let id = unsafe { (*exotic).dictionary_shape_id };
            if id == ShapeId::UNASSIGNED { empty } else { id }
        })
    }

    /// A dictionary-mode object's slot-layout epoch; `0` when it has none.
    fn dictionary_layout(&self) -> u32 {
        // SAFETY: a non-null handle names a live sidecar payload.
        exotic_body_of(self.exotic.get()).map_or(0, |exotic| unsafe { (*exotic).dictionary_layout })
    }

    /// Number of live string-keyed slots: the shape's property count, or the
    /// sidecar's dictionary slot count in dictionary mode.
    #[inline]
    pub(crate) fn slot_count(&self) -> usize {
        if !self.is_dictionary() {
            return shape_body::property_count_of(self.shape) as usize;
        }
        // SAFETY: a non-null handle names a live sidecar payload.
        exotic_body_of(self.exotic.get())
            .map_or(0, |exotic| unsafe { (*exotic).dictionary_slot_count }
                as usize)
    }

    /// Set a dictionary-mode object's slot count.
    #[inline]
    fn set_dictionary_slot_count(&mut self, count: usize) {
        debug_assert!(
            self.is_dictionary(),
            "a shaped object's count is its shape's"
        );
        self.exotic_mut().dictionary_slot_count =
            u32::try_from(count).expect("slot count exceeds u32");
    }

    /// Total logical slots reserved by the persistent prefix and suffix slab.
    #[inline]
    fn slab_capacity(&self) -> usize {
        self.inline_capacity()
            + if self.slab.is_null() {
                0
            } else {
                // SAFETY: the non-null handle names the live suffix slab.
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

    /// Select the immutable storage bank of one shape-owned location.
    #[inline]
    fn field_ptr(&self, field: FieldLocation) -> *mut Value {
        let base = if field.is_inline() {
            self.inline_values_ptr()
        } else {
            // SAFETY: every live overflow field has a reserved suffix slab.
            unsafe { (*self.slab_body_ptr()).words_ptr() }
        };
        // SAFETY: caller supplies a live/reserved location in this bank.
        unsafe { field.word_ptr(base) }
    }

    #[inline]
    fn location_for_slot(&self, index: usize) -> FieldLocation {
        FieldLocation::for_slot(
            u32::try_from(index).expect("slot exceeds u32"),
            self.inline_capacity(),
        )
    }

    /// Check storage bounds in a stable mutator view. Collector and image
    /// visitors must first finish rewriting fixed handles and do not call it.
    #[inline]
    fn debug_verify_field_layout(&self) {
        #[cfg(debug_assertions)]
        {
            debug_assert_object_shape_handle(self.shape, "field layout verification");
            shape_body::debug_verify_field_locations(self.shape);
            // SAFETY: this method is called only for a resident object while
            // the mutator owns its stable fixed handles.
            let header = unsafe { &*self.cell_header() };
            assert_eq!(header.type_tag(), OBJECT_BODY_TYPE_TAG);
            let slab = if self.slab.is_null() {
                None
            } else {
                // SAFETY: the resident slab handle names its live header and
                // payload. No allocation or collection occurs in this check.
                let slab_header = unsafe { &*self.slab.as_header_ptr() };
                assert_eq!(slab_header.type_tag(), slot_slab::SLOT_SLAB_BODY_TYPE_TAG);
                let capacity = unsafe { (*self.slab_body_ptr()).capacity() };
                Some((capacity, slab_header.size_bytes() as usize))
            };
            FieldLayout::current().debug_verify(
                self.inline_capacity(),
                header.size_bytes() as usize,
                self.slot_count(),
                slab,
            );
        }
    }

    /// Read the value word for string-keyed slot `i`.
    #[inline]
    fn slot_word(&self, i: usize) -> Value {
        self.debug_verify_field_layout();
        debug_assert!(i < self.slot_count(), "slab read out of range");
        // SAFETY: `i` is below the live slot count, which never exceeds the
        // active buffer's capacity.
        unsafe { *self.field_ptr(self.location_for_slot(i)) }
    }

    /// Read the data value for string-keyed slot `i`.
    #[inline]
    fn data_value(&self, _heap: &otter_gc::GcHeap, i: usize) -> Value {
        self.slot_word(i)
    }

    /// Write a value into string-keyed slot `i`.
    #[inline]
    fn set_data_value(&mut self, i: usize, value: Value) {
        self.debug_verify_field_layout();
        self.invalidate_prototype_proofs();
        debug_assert!(i < self.slot_count(), "slab write out of range");
        // SAFETY: same in-range word as `slot_word`.
        unsafe { *self.field_ptr(self.location_for_slot(i)) = value };
    }

    /// Write slot word `index` of an append into the active buffer. The
    /// caller reserved the room through [`reserve_slot_capacity`], which
    /// grows only the suffix when the shape-owned capacity is exhausted.
    /// A shaped object's count follows from the shape its caller installs
    /// (before or after this write, with no safepoint between); a
    /// dictionary object's count advances here.
    #[inline]
    fn push_slab_word(&mut self, index: usize, value: Value) {
        self.debug_verify_field_layout();
        self.invalidate_prototype_proofs();
        debug_assert!(
            index < self.slab_capacity(),
            "slab append past reserved capacity: index={index} capacity={}; \
             the caller must reserve through `reserve_slot_capacity` first",
            self.slab_capacity(),
        );
        // SAFETY: the append index is inside the reserved capacity of the
        // active buffer.
        unsafe { *self.field_ptr(self.location_for_slot(index)) = value };
        if self.is_dictionary() {
            debug_assert_eq!(self.slot_count(), index, "dictionary append desynced");
            self.set_dictionary_slot_count(index + 1);
        } else {
            let count = self.slot_count();
            debug_assert!(
                count == index || count == index + 1,
                "shaped append at {index} under a shape of {count} slots"
            );
        }
    }

    /// Remove a dictionary slot and shift later words across the inline/suffix
    /// boundary without changing the shape-owned allocation geometry.
    #[inline]
    fn remove_slab_word(&mut self, i: usize) {
        self.invalidate_prototype_proofs();
        debug_assert!(self.is_dictionary(), "only dictionary objects remove slots");
        let len = self.slot_count();
        // A dictionary shift can cross the persistent-prefix boundary.
        for index in i..len - 1 {
            let value = self.slot_word(index + 1);
            unsafe { *self.field_ptr(self.location_for_slot(index)) = value };
        }
        unsafe { *self.field_ptr(self.location_for_slot(len - 1)) = Value::undefined() };
        self.set_dictionary_slot_count(len - 1);
    }

    /// Install a fully initialized larger out-of-line slab.
    ///
    /// The body never grows its own storage: growth is an allocation, and
    /// an allocation inside a property store is where an object gets moved
    /// out from under its own mutation. So the caller reserves first
    /// ([`reserve_slot_capacity`]) and the body only ever writes into
    /// capacity that already exists.
    fn adopt_slab(&mut self, slab: slot_slab::SlotSlabHandle) {
        debug_assert!(!slab.is_null(), "adopting a null slab");
        self.slab = slab;
        self.debug_verify_field_layout();
    }

    /// Append a new string-keyed own slot at flat index `index` (the pre-append
    /// property count). For a shaped object the hidden class records all slot
    /// attributes, so only the flat value is written. Dictionary storage also
    /// pushes `meta` onto its per-slot metadata table so it stays
    /// index-aligned with the value array. For an accessor slot `value` is the
    /// [`AccessorCellBody`] handle produced by [`SlotData::into_flat`].
    fn push_slot(&mut self, index: usize, meta: SlotMeta, value: Value) {
        self.push_slab_word(index, value);
        if self.slots_materialized() {
            debug_assert_eq!(self.slots().len(), index, "materialized slots desynced");
            self.slots_mut().push(meta);
        }
    }

    /// Store a descriptor and flat value. A prepared attribute shape owns
    /// ordinary descriptors; the no-shape path requires normalized dictionary
    /// metadata and retires watched proofs before descriptor publication.
    fn set_slot(
        &mut self,
        i: usize,
        meta: SlotMeta,
        value: Value,
        attr_shape: Option<ShapeHandle>,
    ) {
        match attr_shape {
            Some(shape) => {
                self.set_data_value(i, value);
                debug_assert_object_shape_handle(shape, "slot attribute shape install");
                debug_assert_object_shape_handle(shape, "shape-slot store");
                self.shape = shape;
                assert!(
                    !self.is_dictionary(),
                    "attribute shape owns all descriptors"
                );
            }
            None => {
                assert!(
                    self.is_dictionary(),
                    "in-place attributes require dictionary storage"
                );
                debug_assert!(
                    self.slots_materialized(),
                    "set_slot(None) needs materialized slots"
                );
                let previous = self.slots()[i];
                let changes = descriptor_mutation::DescriptorChanges::for_slot(previous, meta);
                changes.retire(self);
                self.set_data_value(i, value);
                self.slots_mut().entries_mut()[i] = SlotMeta {
                    watched: previous.watched && !changes.changed(),
                    ..meta
                };
            }
        }
    }

    /// Read descriptor facts from their sole shape or dictionary owner.
    #[inline]
    fn slot_attrs(&self, heap: &otter_gc::GcHeap, i: usize) -> (PropertyFlags, bool) {
        if !self.is_dictionary()
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
        self.slot_lookup_with(heap, i, flags, is_accessor)
    }

    /// [`PropertyLookup`] for the string-keyed slot at `i` whose attributes
    /// the caller already holds.
    fn slot_lookup_with(
        &self,
        heap: &otter_gc::GcHeap,
        i: usize,
        flags: PropertyFlags,
        is_accessor: bool,
    ) -> PropertyLookup {
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
        debug_assert_eq!(self.slot_count(), len, "value slab metadata desynced");
        self.remove_slab_word(i);
        self.slots_mut().remove(i);
    }

    // --- Lazily-boxed exotic slots -----------------------------------------
    // Reads return the field's default when no `ExoticSlots` is allocated;
    // mutators allocate the box on first write. Plain objects never touch it.

    /// The `[[Prototype]]`, which the shape fixes.
    #[inline]
    fn prototype(&self) -> ObjectPrototype {
        match shape_body::prototype_of(self.shape) {
            shape_body::ShapePrototype::Null => ObjectPrototype::Null,
            shape_body::ShapePrototype::Object(object) => ObjectPrototype::Object(object),
            shape_body::ShapePrototype::Value(value) => match value.as_proxy() {
                Some(proxy) => ObjectPrototype::Proxy(proxy),
                None => ObjectPrototype::Value(value),
            },
        }
    }

    /// `true` in dictionary mode: the object's keys live in its sidecar and
    /// its shape is its lineage's dictionary shape.
    #[inline]
    pub(crate) fn is_dictionary(&self) -> bool {
        shape_body::is_dictionary_of(self.shape)
    }

    /// The hidden class that fixes this object's layout, or null in
    /// dictionary mode, where no shape does: every dictionary object of a
    /// lineage shares one dictionary shape whatever its keys.
    #[inline]
    pub(crate) fn keyed_shape(&self) -> ShapeHandle {
        if self.is_dictionary() {
            ShapeHandle::null()
        } else {
            self.shape
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
    fn dictionary_index_get(&self, key: &str) -> Option<u32> {
        self.dict_keys().and_then(|table| table.find(key))
    }

    /// `true` when per-slot metadata is materialized in [`ExoticSlots::slots`]
    /// and is the authoritative attribute source. Dictionary-mode objects
    /// materialize; the common shaped object
    /// derives attributes from the hidden class and carries none.
    #[inline]
    fn slots_materialized(&self) -> bool {
        self.is_dictionary()
    }

    /// Per-slot metadata as a slice (`&[]` when no table exists). A prepared
    /// integrity transaction can populate its tables immediately before
    /// publishing dictionary state, without exposing a second lookup owner.
    #[inline]
    fn slots(&self) -> &[SlotMeta] {
        self.exotic()
            .and_then(|e| slot_meta_body_of(e.slots))
            // SAFETY: a non-null handle names a live table whose prefix
            // outlives this borrow of the object body.
            .map_or(&[], |table| unsafe { (*table).entries() })
    }

    /// Exclusive access to a pre-reserved metadata table. This never allocates;
    /// callers own dictionary storage or a nonallocating integrity publication
    /// transaction which installs its final dictionary state before returning.
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
            .field("has_shape", &!self.is_dictionary())
            .field("dictionary_len", &self.dict_key_count())
            .field("shape_state", &self.state())
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
            .field("extensible", &self.extensible())
            .finish()
    }
}

impl ObjectBody {
    /// Trace the fixed body's handles: shape, prototype, slab, sidecar.
    fn trace_fixed_slots(&mut self, v: &mut SlotVisitor<'_>) {
        // No shape-validity assert here: an image restore traces bodies
        // while their handles still carry the capture isolate's offsets,
        // so a trace-entry read of the shape cell would dereference
        // pre-relocation state. The store paths keep the assert.
        // Every object names its shape, a dictionary object its lineage's
        // dictionary shape; the shape keeps the prototype alive.
        if !self.shape.is_null() {
            let p = &mut self.shape as *mut ShapeHandle as *mut RawGc;
            v(p);
        }
        // The out-of-line slab is an ordinary GC body: trace the handle so a
        // moving collection rewrites it, and let the slab trace its own
        // words.
        if !self.slab.is_null() {
            debug_assert!(
                self.slab.offset() >= 0x1000 && self.slab.offset().is_multiple_of(8),
                "ObjectBody.slab holds a garbage handle {:#x}",
                self.slab.offset(),
            );
            let p = &mut self.slab as *mut slot_slab::SlotSlabHandle as *mut RawGc;
            v(p);
        }
        // The exotic sidecar is its own GC body: trace the handle so a
        // moving collection rewrites it, and let the sidecar trace its own
        // slots through its `Traceable` impl.
        if !self.exotic.is_null() {
            v(self.exotic.slot_ptr());
        }
    }
}

impl otter_gc::SafeTraceable for ObjectBody {
    const TYPE_TAG: u8 = OBJECT_BODY_TYPE_TAG;

    /// Walk every outgoing GC reference held by `self`:
    /// - the shape, `[[Prototype]]`, slab and sidecar handles;
    /// - every live slot `Value` (a data value, or an accessor slot's
    ///   [`AccessorCellBody`] handle), in-object or in the slab.
    ///
    /// The slab's words are walked through the object as well as by the slab
    /// body: a remembered old object is re-traced on a scavenge, and the young
    /// values its slot stores recorded may sit in an old slab the scavenge
    /// never visits on its own. Once an object has spilled, its in-object
    /// words remain live and are traced independently of the overflow suffix.
    fn trace_slots_safe(&mut self, v: &mut SlotVisitor<'_>) {
        self.trace_fixed_slots(v);
        // Fixed handles have been rewritten before capacity/count are read.
        for i in 0..self.slot_count() {
            unsafe { (*self.field_ptr(self.location_for_slot(i))).trace_value_slot_mut(v) };
        }
    }

    /// The pending payload has no in-object tail yet: its slot values are
    /// the allocation caller's rooted buffer, copied in by the initializer.
    fn trace_pending_slots_safe(&mut self, v: &mut SlotVisitor<'_>) {
        self.trace_fixed_slots(v);
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
        .max(DEFAULT_INLINE_CAPACITY * 2);
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
    let inline_capacity = heap.read_payload(*object, ObjectBody::inline_capacity);
    let suffix_capacity = grown - inline_capacity;
    let slab = heap.alloc_variable_with_roots_initialized(
        slot_slab::SlotSlabBody::new(suffix_capacity),
        slot_slab::SlotSlabBody::trailing_bytes(suffix_capacity),
        &mut visit,
        |target| {
            // The allocator has already rewritten the rooted owner. Read its
            // live suffix now; no interior pointer crosses the collection.
            // SAFETY: object_slot is the rooted ordinary owner's handle.
            let owner = unsafe { (*object_slot).cast::<ObjectBody>() };
            let body = unsafe {
                &*owner
                    .as_header_ptr()
                    .cast::<u8>()
                    .add(otter_gc::header::HEADER_SIZE)
                    .cast::<ObjectBody>()
            };
            let live = body.slot_count().saturating_sub(inline_capacity);
            debug_assert!(live <= suffix_capacity);
            if live != 0 {
                // Copy only suffix words before the allocation's edge scan;
                // the new slab owns the copied young/marking edges itself.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        (*body.slab_body_ptr()).words_ptr(),
                        target.words_ptr(),
                        live,
                    )
                };
            }
            for index in live..suffix_capacity {
                unsafe { *target.words_ptr().add(index) = Value::undefined() };
            }
        },
    )?;
    let owner = *object;
    heap.with_payload(owner, |body| {
        body.adopt_slab(slab);
        true
    });
    // Record publication of the new slab independently of its initialized
    // child edges. The allocator scanned those edges before returning.
    heap.record_write(owner, &slab);
    Ok(())
}

/// Make room for `needed` materialized per-slot metadata records.
///
/// A no-op for the shaped object that keeps its
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
    // Every full collection of a VM heap applies JavaScript weak semantics
    // between strong marking and sweep, whatever triggered it.
    heap.set_post_mark_processor(crate::weak_refs::post_mark_processor);
    heap.register_host_release::<crate::native_function::NativeFunctionBody>();
    heap.register_sever_restored::<ExoticSlots>();
    heap.register_sever_restored::<crate::constructor_layout::ConstructorLayoutBody>();
    heap.register_sever_restored::<crate::array::ArrayExoticSlots>();
    heap.register_sever_restored::<crate::weak_refs::WeakRefBody>();
    heap.register_sever_restored::<crate::binary::typed_array::TypedArrayBodyGc>();
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
        crate::closure_construct::ClosureRareBody,
        crate::constructor_layout::ConstructorLayoutBody,
        crate::collections::MapBody,
        crate::collections::table::OrderedTableBody<crate::collections::MapEntry>,
        crate::collections::SetBody,
        crate::collections::table::OrderedTableBody<crate::collections::SetEntry>,
        crate::collections::WeakMapBody,
        crate::collections::WeakSetBody,
        crate::collections::weak_table::WeakTableBody<crate::collections::weak_table::MapKind>,
        crate::collections::weak_table::WeakTableBody<crate::collections::weak_table::SetKind>,
        crate::context::ContextBody,
        crate::eval_env::EvalExtensionBody,
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

/// An object body about to be allocated. State lives only in the hidden class;
/// its immutable shape determines the trailing inline allocation size.
struct PendingObject {
    body: ObjectBody,
}

impl PendingObject {
    /// Trailing in-object slot bytes of the cell.
    fn trailing_bytes(&self) -> usize {
        FieldLocation::words_bytes(shape_body::inline_capacity_of(self.body.shape))
    }
}

/// An empty body with `shape` installed: a keyed or root shape, or a
/// dictionary shape. The shape fixes the object's prototype.
fn empty_object_body(shape: ShapeHandle) -> PendingObject {
    debug_assert_object_shape_handle(shape, "object allocation shape");
    PendingObject {
        body: ObjectBody {
            shape,
            slab: otter_gc::Gc::null(),
            exotic: ExoticSlot::null(),
        },
    }
}

fn debug_assert_object_shape_handle(shape: ShapeHandle, context: &str) {
    debug_assert!(!shape.is_null(), "an object always has a shape ({context})");
    if cfg!(debug_assertions) {
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

/// Allocate an ordinary object body in the young generation (old when the
/// heap tenures), reserving its in-object slots.
fn alloc_object_body_with_roots(
    heap: &mut GcHeap,
    pending: PendingObject,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    let extra = pending.trailing_bytes();
    let PendingObject { body } = pending;
    heap.alloc_trailing_with_roots_initialized(body, extra, external_visit, |body| {
        body.init_cell_bytes();
        for index in 0..body.inline_capacity() {
            unsafe {
                *FieldLocation::inline(index as u32).word_ptr(body.inline_values_ptr()) =
                    Value::undefined()
            };
        }
    })
}

/// Allocate an ordinary object body directly in old space.
fn alloc_object_body_old(
    heap: &mut GcHeap,
    pending: PendingObject,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    let extra = pending.trailing_bytes();
    let PendingObject { body } = pending;
    let mut no_roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
    heap.alloc_variable_with_roots_initialized(body, extra, &mut no_roots, |body| {
        body.init_cell_bytes();
        for index in 0..body.inline_capacity() {
            unsafe {
                *FieldLocation::inline(index as u32).word_ptr(body.inline_values_ptr()) =
                    Value::undefined()
            };
        }
    })
}

/// Allocate an old-space object for raw GC fixtures.
///
/// Production VM allocation paths must use stack/runtime/native root contracts.
#[cfg(test)]
pub(crate) fn alloc_object_old_for_fixture(
    heap: &mut GcHeap,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    let root = fixture_root_shape(heap)?;
    alloc_object_body_old(heap, empty_object_body(root))
}

/// The heap's `null`-prototype root for raw GC fixtures, created and
/// installed on a heap no interpreter owns.
#[cfg(test)]
pub(crate) fn fixture_root_shape(heap: &mut GcHeap) -> Result<ShapeHandle, otter_gc::OutOfMemory> {
    if shape_body::null_root(heap).is_null() {
        let mut no_roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let root = shape_body::alloc_root_shape_body_with_roots(
            heap,
            shape_body::ShapePrototype::Null,
            DEFAULT_INLINE_CAPACITY,
            ShapeHandle::null(),
            ShapeState::ORDINARY,
            &mut no_roots,
        )?;
        shape_body::set_null_root(heap, root);
    }
    Ok(shape_body::null_root(heap))
}

/// A raw heap for fixtures that run heap-only VM code without an
/// interpreter: it carries the `null`-prototype root that code allocates on.
#[cfg(test)]
pub(crate) fn fixture_heap() -> GcHeap {
    let mut heap = GcHeap::new().expect("heap");
    fixture_root_shape(&mut heap).expect("null-prototype root");
    heap
}

/// An empty `null`-prototype object for GC fixtures, on a heap with or
/// without an interpreter.
#[cfg(test)]
pub(crate) fn alloc_fixture_object_with_roots(
    heap: &mut GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    let root = fixture_root_shape(heap)?;
    alloc_object_with_roots(heap, root, external_visit)
}

/// Allocate an empty object directly in non-moving old space.
///
/// For permanent singleton roots — the realm global object — that live for
/// the whole isolate. Pinning them in old space keeps every handle stable
/// across young scavenges and avoids copying a large, long-lived object on
/// every minor collection. The empty body holds no GC edges, so no caller
/// roots are required across the allocation.
pub(crate) fn alloc_object_old(
    heap: &mut GcHeap,
    root: ShapeHandle,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    alloc_object_body_old(heap, empty_object_body(root))
}

/// Allocate a fresh empty object of `root`'s lineage — whose prototype the
/// root fixes — through the young-generation allocation path.
///
/// Callers must provide every stack/register root the scavenger may need to
/// rewrite if allocation triggers a minor collection.
pub(crate) fn alloc_object_with_roots(
    heap: &mut GcHeap,
    root: ShapeHandle,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    alloc_object_body_with_roots(heap, empty_object_body(root), external_visit)
}

/// Allocate a fresh empty dictionary-mode object with a `null` prototype:
/// the heap-only allocation, for callers without a shape runtime to take
/// transitions through.
pub(crate) fn alloc_dictionary_object_with_roots(
    heap: &mut GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    let shape = shape_body::dictionary_of(shape_body::null_root(heap));
    alloc_object_with_roots(heap, shape, external_visit)
}

/// Allocate a fresh object using exactly its hidden class's inline capacity.
/// Counted fields are initialized before publication; overflow is suffix-only.
pub(crate) fn alloc_object_with_shape_roots(
    heap: &mut GcHeap,
    shape: ShapeHandle,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    let mut values = vec![Value::undefined(); shape_property_count(shape, heap) as usize];
    alloc_object_with_shape_and_values_roots(heap, shape, &mut values, external_visit)
}

/// Allocate a fresh shaped object whose complete data-slot prefix is installed
/// before the object becomes reachable.
///
/// Values that fit the in-object capacity are copied into the young object's
/// own slots. Wider objects first allocate an old-space slab with its
/// collector-rewritten values already in the trailing words, then publish the
/// object that owns that slab. In either case no empty property is exposed and
/// no later per-property mutator store is required.
pub(crate) fn alloc_object_with_shape_and_values_roots(
    heap: &mut GcHeap,
    mut shape: ShapeHandle,
    values: &mut [Value],
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    debug_assert_eq!(
        shape_property_count(shape, heap) as usize,
        values.len(),
        "shape slot count and initialization values diverged"
    );

    let shape_slot = std::ptr::addr_of_mut!(shape).cast::<RawGc>();
    let all_values = values.as_mut_ptr();
    let all_values_len = values.len();
    let mut visit_owner_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        visitor(shape_slot);
        for index in 0..all_values_len {
            unsafe { (*all_values.add(index)).trace_value_slot_mut(visitor) };
        }
    };

    let capacity = shape_body::inline_capacity_of(shape);
    let prefix = values.len().min(capacity);
    let slab = if values.len() <= capacity {
        slot_slab::SlotSlabHandle::null()
    } else {
        slot_slab::alloc_slot_slab_with_values(
            heap,
            values.len() - capacity,
            &mut values[capacity..],
            &mut visit_owner_roots,
        )?
    };
    let PendingObject { mut body } = empty_object_body(shape);
    body.slab = slab;
    let values_base = values.as_mut_ptr();
    let values_len = values.len();
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        // The pending body owns and traces the slab handle. Only the input
        // buffer needs extra roots; do not expose an immutable local as a
        // collector-writable root.
        for index in 0..values_len {
            unsafe { (*values_base.add(index)).trace_value_slot_mut(visitor) };
        }
    };
    heap.alloc_trailing_with_roots_initialized(
        body,
        FieldLocation::words_bytes(capacity),
        &mut visit,
        |body| {
            body.init_cell_bytes();
            for index in 0..capacity {
                let value = if index < prefix {
                    unsafe { *values_base.add(index) }
                } else {
                    Value::undefined()
                };
                unsafe {
                    *FieldLocation::inline(index as u32).word_ptr(body.inline_values_ptr()) = value
                };
            }
        },
    )
}

/// Install `shape` on a fresh, slotless object together with a value for
/// every slot it names, in shape order, reserving room for `capacity` slots.
///
/// The shape fixes the object's slot count, so it goes on in the same payload
/// borrow that writes the slots, after the only allocation (the slab
/// reservation): no collection ever sees a counted slot unwritten. Capacity
/// is not observable — later fields become visible at their own
/// `StoreProperty` operations.
pub(crate) fn install_fresh_shape_with_slots(
    obj: JsObject,
    heap: &mut GcHeap,
    shape: ShapeHandle,
    values: &[Value],
    capacity: usize,
) {
    debug_assert_object_shape_handle(shape, "fresh object shape install");
    debug_assert!(!shape.is_null(), "a fresh shape install needs a shape");
    debug_assert_eq!(
        shape_body::property_count_of(shape) as usize,
        values.len(),
        "shape slot count and init value count diverged"
    );
    let mut obj = obj;
    let mut stored = SmallVec::<[Value; 8]>::from_slice(values);
    debug_assert_eq!(heap.read_payload(obj, ObjectBody::slot_count), 0);
    if reserve_slot_capacity(&mut obj, heap, capacity.max(stored.len()), &mut stored).is_err() {
        return;
    }
    heap.with_payload(obj, |body| {
        debug_assert_eq!(
            body.slot_count(),
            0,
            "shape install requires a fresh object"
        );
        body.invalidate_prototype_proofs();
        assert_eq!(
            body.inline_capacity(),
            shape_body::inline_capacity_of(shape)
        );
        for (index, &value) in stored.iter().enumerate() {
            unsafe { *body.field_ptr(body.location_for_slot(index)) = value };
        }
        body.invalidate_prototype_proofs();
        assert_eq!(
            body.inline_capacity(),
            shape_body::inline_capacity_of(shape),
            "shape transition changed object footprint"
        );
        body.shape = shape;
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
    debug_assert_eq!(heap.read_payload(*obj, ObjectBody::slot_count), 0);
    reserve_slot_capacity(obj, heap, capacity, &mut [])
}

/// Try a fresh shape-owned inline allocation without a GC safepoint.
/// A shape whose live fields need overflow is declined before allocation.
pub(crate) fn try_alloc_object_with_shape_no_collect(
    heap: &mut GcHeap,
    shape: ShapeHandle,
) -> Option<JsObject> {
    if shape_body::property_count_of(shape) as usize > shape_body::inline_capacity_of(shape) {
        return None;
    }
    let pending = empty_object_body(shape);
    let extra = pending.trailing_bytes();
    let PendingObject { body } = pending;
    heap.try_alloc_trailing_no_collect_or_return(body, extra, |body| {
        body.init_cell_bytes();
        for index in 0..body.inline_capacity() {
            unsafe {
                *FieldLocation::inline(index as u32).word_ptr(body.inline_values_ptr()) =
                    Value::undefined()
            };
        }
    })
    .ok()
}

/// Allocate a fresh empty object for diagnostic delivery after the
/// heap cap has already fired.
///
/// This uses [`otter_gc::GcHeap::alloc_old_diagnostic_trailing`] so the VM
/// can throw a catchable `RangeError` for an allocation failure instead of
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
pub(crate) fn alloc_diagnostic_object(
    heap: &mut GcHeap,
    root: ShapeHandle,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    let PendingObject { body } = empty_object_body(root);
    let extra = FieldLocation::words_bytes(shape_body::inline_capacity_of(root));
    let object = heap.alloc_old_diagnostic_trailing(body, extra)?;
    // No safepoint separates the allocation from this write.
    heap.with_payload(object, |body| {
        body.init_cell_bytes();
        for index in 0..body.inline_capacity() {
            unsafe {
                *FieldLocation::inline(index as u32).word_ptr(body.inline_values_ptr()) =
                    Value::undefined()
            };
        }
    });
    Ok(object)
}

/// A host object body over `sidecar`: opaque as a prototype-chain link,
/// since its host data can supply properties outside its slots.
fn host_object_body(shape: ShapeHandle, sidecar: ExoticSlot) -> PendingObject {
    assert!(
        shape_body::state_of(shape).is_opaque(),
        "host shape prepared before allocation"
    );
    PendingObject {
        body: ObjectBody {
            shape,
            slab: otter_gc::Gc::null(),
            exotic: sidecar,
        },
    }
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
    let shape = shape_body::dictionary_of(fixture_root_shape(heap)?);
    // SAFETY: this scope ends before the heap and no Local escapes.
    let scope = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let source = scope.local(shape);
    let host_state = shape_body::state_of(source.get()).with_lookup(LookupFact::HostLookup, true);
    let shape =
        state_transition::prepare_state_shape(heap, source.get(), host_state, external_visit)?;
    let shape = scope.local(shape);
    let mut sidecar: ExoticHandle =
        heap.alloc_variable_with_roots(ExoticSlots::default(), 0, external_visit)?;
    let sidecar_slot = std::ptr::addr_of_mut!(sidecar);
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        visitor(sidecar_slot.cast::<RawGc>());
    };
    let mut slot = ExoticSlot::null();
    slot.set(sidecar);
    let object =
        alloc_object_body_with_roots(heap, host_object_body(shape.get(), slot), &mut visit)?;
    heap.with_payload(sidecar, |exotic| {
        exotic.host_data = Some(HostData::Untraced(Box::new(data)));
        exotic.dictionary_layout = 1;
        exotic.dictionary_shape_id = next_shape_id();
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
    // SAFETY: the heap's arena outlives preparation and no Local escapes.
    let scope = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let source = scope.local(shape);
    let host_state = shape_body::state_of(source.get()).with_lookup(LookupFact::HostLookup, true);
    let shape =
        state_transition::prepare_state_shape(heap, source.get(), host_state, external_visit)?;
    let shape = scope.local(shape);
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
    let object =
        alloc_object_body_with_roots(heap, host_object_body(shape.get(), slot), &mut visit)?;
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
    // SAFETY: the heap's arena outlives preparation and no Local escapes.
    let scope = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let source = scope.local(shape);
    let mut pending_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        data.trace_gc_slots(&mut HostDataTracer { visitor });
    };
    let host_state = shape_body::state_of(source.get()).with_lookup(LookupFact::HostLookup, true);
    let shape =
        state_transition::prepare_state_shape(heap, source.get(), host_state, &mut pending_roots)?;
    let shape = scope.local(shape);
    // The sidecar is allocated before the object exists, so installing
    // the host payload needs no second allocation point inside a borrow.
    let mut sidecar: ExoticHandle =
        heap.alloc_variable_with_roots(ExoticSlots::default(), 0, &mut pending_roots)?;
    let sidecar_slot = std::ptr::addr_of_mut!(sidecar);
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        pending_roots(visitor);
        visitor(sidecar_slot.cast::<RawGc>());
    };
    let mut slot = ExoticSlot::null();
    slot.set(sidecar);
    let object =
        alloc_object_body_with_roots(heap, host_object_body(shape.get(), slot), &mut visit)?;
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
pub fn mark_as_arguments_object(
    obj: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
) -> Result<(), otter_gc::OutOfMemory> {
    // The sidecar allocation may move the object; the caller's handle
    // is updated in place.
    ensure_exotic(obj, heap)?;
    heap.with_payload(*obj, |body| {
        body.exotic_mut().is_arguments_object = true;
    });
    Ok(())
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
        if body.is_dictionary() {
            return None;
        }
        let count = shape_body::shape_property_count(heap, body.shape) as usize;
        count.checked_sub(2).map(|argc| (body.shape, argc))
    })
}

/// Install the ParameterMap. The caller passes the current context handle:
/// `mapped.context` must be rooted across every allocation that preceded
/// this call (the arguments object and its shape).
pub(crate) fn install_mapped_arguments(
    obj: &mut JsObject,
    heap: &mut GcHeap,
    mapped: MappedArguments,
) -> Result<(), otter_gc::OutOfMemory> {
    use crate::rooting::RootScopeExt;
    let MappedArguments {
        mut context,
        entries,
    } = mapped;
    if entries.is_empty() {
        return Ok(());
    }
    let mut roots = otter_gc::RootScope::new(heap);
    // SAFETY: both live slots precede the scope and stay stationary until the
    // sidecar owns the map. Cap-triggered collection can rewrite both in place.
    unsafe {
        roots.add_object(obj);
        roots.add_raw_slot(std::ptr::addr_of_mut!(context).cast::<RawGc>());
    }
    ensure_exotic(obj, heap)?;
    let target = state(*obj, heap).with_lookup(LookupFact::MappedArguments, true);
    state_transition::transition_state(obj, heap, target)?;
    heap.with_payload(*obj, |body| {
        body.exotic_mut().host_data = Some(HostData::Untraced(Box::new(MappedArgumentsData {
            context,
            entries: entries.into_boxed_slice(),
        })));
    });
    let sidecar = heap.read_payload(*obj, |body| body.exotic.get());
    heap.record_write(sidecar, &context);
    Ok(())
}

fn mapped_argument_cell(body: &ObjectBody, key: &str) -> Option<MappedCell> {
    let data = body
        .host_data_ref()?
        .downcast_ref::<MappedArgumentsData>()?;
    data.entries
        .iter()
        .find(|entry| entry.key == key)
        .map(|entry| (data.context, entry.slot))
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
                        context: mapped.context,
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
    existing_offset: Option<u32>,
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
        mapped_write(heap, cell, value);
    }

    if descriptor.writable == Some(false) {
        if descriptor.value.is_none() {
            let current = mapped_read(heap, cell);
            let stored = current;
            if let Some(offset) = existing_offset {
                let is_data_slot = heap.read_payload(obj, |body| {
                    ((offset as usize) < body_property_count(heap, body))
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

/// Immutable semantic state of an object's exact hidden class.
#[must_use]
pub fn state(obj: JsObject, heap: &GcHeap) -> ShapeState {
    heap.read_payload(obj, ObjectBody::state)
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
/// Slot-layout epoch of a dictionary-mode `obj`, or `None` when it is shaped,
/// was never in dictionary mode, or has saturated past any provable value.
/// See [`ObjectBody::dictionary_layout`].
pub(crate) fn dictionary_layout(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<u32> {
    heap.read_payload(obj, |body| {
        let layout = body.dictionary_layout();
        (body.is_dictionary() && layout != 0 && layout != u32::MAX).then_some(layout)
    })
}

/// Mark `slot` of dictionary-mode `obj` as read directly by a compiled
/// slot-layout proof.
///
/// A proof captures [`dictionary_layout`] and then reads the slot's value
/// without re-checking its kind or attributes; redefining a watched slot
/// advances the layout epoch and so retires the proof. Returns `false` — and
/// the caller must not emit the proof — when `obj` is not in dictionary mode
/// or `slot` has no metadata record.
pub(crate) fn watch_dictionary_slot(obj: JsObject, heap: &mut otter_gc::GcHeap, slot: u16) -> bool {
    heap.with_payload(obj, |body| {
        if !body.is_dictionary() || !body.slots_materialized() {
            return false;
        }
        match body.slots_mut().entries_mut().get_mut(usize::from(slot)) {
            Some(entry) if !entry.is_accessor => {
                entry.watched = true;
                true
            }
            _ => false,
        }
    })
}

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
    if !body.is_dictionary() {
        return heap.read_payload(body.shape, shape_body::ShapeBody::id);
    }
    body.dictionary_shape_id()
}

fn body_property_count(heap: &otter_gc::GcHeap, body: &ObjectBody) -> usize {
    if !body.is_dictionary() {
        return shape_body::shape_property_count(heap, body.shape) as usize;
    }
    body.dict_key_count()
}

pub(super) fn body_offset_of(heap: &otter_gc::GcHeap, body: &ObjectBody, key: &str) -> Option<u32> {
    if !body.is_dictionary() {
        debug_assert_object_shape_handle(body.shape, "property offset lookup");
        return shape_body::shape_offset_of_str(heap, body.shape, key);
    }
    // O(1) dictionary lookup via the maintained index — a linear scan
    // here makes bulk property addition O(n²).
    body.dictionary_index_get(key)
}

/// [`body_offset_of`] for an atomized key: a shaped object answers with one
/// `u32` compare per shape-chain link and never touches a heap string.
/// Dictionary storage keys by spelling, so it still hashes the name. A key
/// whose spelling was never interned ([`crate::property_atom::AtomId::NONE`],
/// the root shape's own marker) names no shaped property.
pub(super) fn body_offset_of_atom(
    heap: &otter_gc::GcHeap,
    body: &ObjectBody,
    key: AtomizedPropertyKey<'_>,
) -> Option<u32> {
    if !body.is_dictionary() {
        if key.atom().id() == crate::property_atom::AtomId::NONE {
            return None;
        }
        debug_assert_object_shape_handle(body.shape, "property offset lookup");
        return shape_body::shape_offset_of_atom(heap, body.shape, key.atom().id());
    }
    body.dictionary_index_get(key.name())
}

/// Number of own string-keyed properties recorded in a fast-mode
/// shape (`0` for a dictionary shape). Used to decide when an object should
/// normalize to dictionary storage.
pub(crate) fn shape_property_count(shape: ShapeHandle, heap: &otter_gc::GcHeap) -> u32 {
    shape_body::shape_property_count(heap, shape)
}

/// Maximum number of own properties an object keeps in fast
/// transition-shape storage when a property arrives through a keyed or
/// generic store, before it normalizes to dictionary mode. Objects filled
/// with computed keys are dictionaries in all but name; bounding their
/// transition chains keeps lookup and bulk addition O(1). V8's
/// `JSObject::kMaxFastProperties`.
pub(crate) const MAX_FAST_PROPERTIES: u32 = 128;

/// Maximum number of own properties a *named* store (`o.x = v`) keeps in
/// fast storage. Named stores describe an object's fixed layout, as a
/// constructor building a large state object does, so they stay on shapes
/// (and shape ICs) until V8's descriptor-array limit,
/// `kMaxNumberOfDescriptors`.
pub(crate) const MAX_NAMED_FAST_PROPERTIES: u32 = 1020;

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

fn body_key_matches(heap: &otter_gc::GcHeap, body: &ObjectBody, offset: usize, key: &str) -> bool {
    if !body.is_dictionary() {
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
            return Some(mapped_read(heap, cell));
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
                *value = mapped_read(heap, cell);
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
                *value = mapped_read(heap, cell);
            }
            // A cache slot is a `u16`; a dictionary key past it is looked up
            // by name every time.
            let hit = u16::try_from(offset).ok().map(|slot| OwnPropertySlotHit {
                shape_id: body_shape_id(heap, body),
                slot,
            });
            (hit, lookup)
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
    lookup_own_atom_with(obj, heap, key, |shape, atom| {
        shape_body::shape_slot_of_atom(heap, shape, atom)
    })
}

/// [`lookup_own_atom`] resolving a shaped object's slot and attributes for
/// an interned key through `slot_of`, which a caller may answer from a cache
/// of earlier walks: a shape's slots never change.
pub(crate) fn lookup_own_atom_with(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
    slot_of: impl FnOnce(ShapeHandle, crate::property_atom::AtomId) -> Option<shape_body::ShapeSlot>,
) -> AtomPropertyLookup {
    heap.read_payload(obj, |body| {
        let resolved = if body.is_dictionary() {
            body.dictionary_index_get(key.name()).map(|offset| {
                let (flags, is_accessor) = body.slot_attrs(heap, offset as usize);
                (offset, flags, is_accessor)
            })
        } else if key.atom().id() == crate::property_atom::AtomId::NONE {
            None
        } else {
            debug_assert_object_shape_handle(body.shape, "property offset lookup");
            slot_of(body.shape, key.atom().id())
                .map(|slot| (slot.offset, slot.flags, slot.is_accessor))
        };
        lookup_resolved_slot(heap, body, key, resolved)
    })
}

fn lookup_resolved_slot(
    heap: &otter_gc::GcHeap,
    body: &ObjectBody,
    key: AtomizedPropertyKey<'_>,
    resolved: Option<(u32, PropertyFlags, bool)>,
) -> AtomPropertyLookup {
    match resolved {
        Some((offset, flags, is_accessor)) => {
            let mut lookup = body.slot_lookup_with(heap, offset as usize, flags, is_accessor);
            if let Some(cell) = mapped_argument_cell(body, key.name())
                && let PropertyLookup::Data { value, .. } = &mut lookup
            {
                *value = mapped_read(heap, cell);
            }
            // A cache slot is a `u16`; a dictionary key past it is looked up
            // by name every time.
            let hit = u16::try_from(offset).ok().map(|slot| AtomOwnPropertyHit {
                shape_id: body_shape_id(heap, body),
                shape: body.keyed_shape(),
                atom_id: key.atom().id(),
                slot,
                is_data: matches!(lookup, PropertyLookup::Data { .. }),
            });
            AtomPropertyLookup { hit, lookup }
        }
        None => AtomPropertyLookup {
            hit: None,
            lookup: PropertyLookup::Absent,
        },
    }
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
        // A shaped object's handle plus the shape's never-reused id prove the
        // identical layout (the handle alone could name a later shape in a
        // collected shape's cell); no key compare. The shape also fixes the
        // slot, so the cached `slot` is valid. Dictionary mode (null handle)
        // reuses a per-object shape id that does not bump on every slot
        // mutation, so it still confirms the id and the key by name.
        let shaped = !body.is_dictionary();
        let shape_ok = if shaped {
            shaped_hit_matches(body.shape, &hit)
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
        // Use the same immutable ordinary-state proof as generated named loads. Symbol
        // and native-call sidecars preserve shape-derived slot semantics;
        // opaque host/mapped/String state does not.
        // A matching fast shape fixes the slot kind and bounds, so neither
        // per-slot attributes nor the property count need consulting.
        //
        // Accessor-ness is part of the shape, and `hit.is_data` was recorded
        // against this very shape handle, so a matched shape cannot have turned the slot into an accessor. Asserting
        // that keeps the release hit off the shape body entirely.
        if shaped && hit.is_data && !body.chain_link_opaque() {
            debug_assert!(
                !body.slot_attrs(heap, offset).1,
                "shape-matched data hit resolved to an accessor slot"
            );
            debug_assert!(offset < body_property_count(heap, body));
            return Some(body.data_value(heap, offset));
        }
        if let Some(cell) = mapped_argument_cell(body, key.name()) {
            return Some(mapped_read(heap, cell));
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

/// Whether a live object's `shape` is the one a cached `hit` was recorded
/// under: the handle for the fast reject, then the id, since hidden classes
/// are collectable and a later shape may occupy a collected one's cell.
#[inline]
fn shaped_hit_matches(shape: ShapeHandle, hit: &AtomOwnPropertyHit) -> bool {
    shape == hit.shape && shape_body::id_of(shape) == hit.shape_id
}

/// Read a cached own data slot guarded by shape identity alone.
///
/// A shape-handle match fixes both the slot and its key, so the atom compare
/// [`load_own_data_slot_atom`] performs is redundant here. Used by the
/// monomorphic method-call IC, whose cached `hit` was recorded against this same
/// shape: the hot path is a single offset compare plus a slab read, with no atom
/// resolution and no stub walk. It shares the generated ordinary-state proof:
/// ordinary immutable lookup state. Benign
/// symbol/native sidecars remain eligible; a miss uses full method resolution.
pub(crate) fn load_own_data_slot_by_shape(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    hit: AtomOwnPropertyHit,
) -> Option<Value> {
    heap.read_payload(obj, |body| {
        if body.is_dictionary()
            || !shaped_hit_matches(body.shape, &hit)
            || !hit.is_data
            || body.chain_link_opaque()
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
        let shape_ok = if !body.is_dictionary() {
            shaped_hit_matches(body.shape, &hit)
        } else {
            body_shape_id(heap, body) == hit.shape_id
        };
        if !shape_ok || key.atom().id() != hit.atom_id {
            return false;
        }
        // Store hits are installed only for a writable data slot, and a
        // matching fast shape fixes every slot's attributes, so neither the
        // chain walk for this slot's attributes nor the bounds need
        // consulting. Dictionary storage still does.
        if !body.is_dictionary() {
            return true;
        }
        let key_matches = !body.is_dictionary() || body_key_matches(heap, body, offset, key.name());
        let slot_attrs =
            (offset < body_property_count(heap, body)).then(|| body.slot_attrs(heap, offset));
        key_matches
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
        mapped_write(heap, cell, *value);
    }
    record_slot_write(heap, obj, stored);
    Some(())
}

/// Read a data slot authorized by a live prototype validity proof.
/// No allocation or JavaScript may occur between checking the proof and reading.
pub(crate) fn load_proven_data_slot(obj: JsObject, heap: &GcHeap, slot: u16) -> Value {
    heap.read_payload(obj, |body| body.slot_word(usize::from(slot)))
}

/// Write a writable own data slot whose receiver shape an IC handler already
/// matched. The shape fixes the slot's attributes and storage bank, so the
/// write needs no descriptor or key check; the generational barrier runs.
pub(crate) fn store_proven_data_slot(obj: JsObject, heap: &mut GcHeap, slot: u16, value: Value) {
    heap.with_payload(obj, |body| body.set_data_value(usize::from(slot), value));
    record_slot_write(heap, obj, value);
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
                *value = mapped_read(heap, cell);
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

/// Whether `obj` has an own property keyed by the well-known symbol `tag`,
/// which every realm shares.
#[must_use]
pub(crate) fn has_own_well_known_symbol(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    tag: crate::symbol::WellKnown,
) -> bool {
    heap.read_payload(obj, |body| {
        body.symbol_props()
            .iter()
            .any(|(key, _)| key.well_known_tag() == Some(tag))
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
pub fn set_boolean_data(
    obj: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    value: bool,
) -> Result<(), otter_gc::OutOfMemory> {
    // The sidecar allocation may move the object; the caller's handle
    // is updated in place.
    ensure_exotic(obj, heap)?;
    heap.with_payload(*obj, |body| {
        body.exotic_mut().boolean_data = Some(value);
    });
    Ok(())
}

/// Read the `[[BooleanData]]` internal slot for a Boolean wrapper.
#[must_use]
pub fn boolean_data(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<bool> {
    heap.read_payload(obj, |body| body.boolean_data())
}

/// Store the `[[NumberData]]` internal slot for a Number wrapper.
pub fn set_number_data(
    obj: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    value: NumberValue,
) -> Result<(), otter_gc::OutOfMemory> {
    // The sidecar allocation may move the object; the caller's handle
    // is updated in place.
    ensure_exotic(obj, heap)?;
    heap.with_payload(*obj, |body| {
        body.exotic_mut().number_data = Some(value);
    });
    Ok(())
}

/// Read the `[[NumberData]]` internal slot for a Number wrapper.
#[must_use]
pub fn number_data(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<NumberValue> {
    heap.read_payload(obj, |body| body.number_data())
}

/// Store the `[[StringData]]` internal slot for a String wrapper.
pub fn set_string_data(
    obj: &mut JsObject,
    heap: &mut GcHeap,
    value: JsString,
) -> Result<(), otter_gc::OutOfMemory> {
    use crate::rooting::RootScopeExt;
    let mut pending = Value::string(value);
    let mut roots = otter_gc::RootScope::new(heap);
    // SAFETY: both stationary slots precede the scope. Shape preparation and
    // sidecar allocation may collect, rewriting the actual caller receiver.
    unsafe {
        roots.add_object(obj);
        roots.add_value(&mut pending);
    }
    ensure_exotic(obj, heap)?;
    let target = state(*obj, heap).with_lookup(LookupFact::StringWrapper, true);
    state_transition::transition_state(obj, heap, target)?;
    let value = pending
        .as_string(heap)
        .expect("rooted pending String value");
    heap.with_payload(*obj, |body| body.exotic_mut().string_data = Some(value));
    record_exotic_write(heap, *obj, &value);
    Ok(())
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
) -> Result<(), otter_gc::OutOfMemory> {
    // The sidecar allocation may move both the object and the symbol;
    // the caller's handle is updated in place and the symbol rides the
    // pending-root list.
    let mut pending = [Value::symbol(value)];
    ensure_exotic_with_pending_values(obj, heap, &mut pending)?;
    let value = pending[0]
        .as_symbol(heap)
        .expect("pending symbol survives rooting");
    heap.with_payload(*obj, |body| {
        body.exotic_mut().symbol_data = Some(value);
    });
    // The slot lives in the old-space sidecar.
    record_exotic_write(heap, *obj, &value);
    Ok(())
}

/// Read the `[[SymbolData]]` internal slot for a Symbol wrapper.
#[must_use]
pub fn symbol_data(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<crate::symbol::JsSymbol> {
    heap.read_payload(obj, |body| body.symbol_data())
}

/// Store the `[[BigIntData]]` internal slot for a BigInt wrapper.
pub fn set_bigint_data(
    obj: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    value: BigIntValue,
) -> Result<(), otter_gc::OutOfMemory> {
    // The sidecar allocation may move both the object and the bigint;
    // the caller's handle is updated in place and the bigint rides the
    // pending-root list.
    let mut pending = [Value::big_int(value)];
    ensure_exotic_with_pending_values(obj, heap, &mut pending)?;
    let value = pending[0]
        .as_big_int()
        .expect("pending bigint survives rooting");
    heap.with_payload(*obj, |body| {
        body.exotic_mut().bigint_data = Some(value);
    });
    // The slot lives in the old-space sidecar.
    record_exotic_write(heap, *obj, &Value::big_int(value));
    Ok(())
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
pub fn set_error_data(
    obj: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
) -> Result<(), otter_gc::OutOfMemory> {
    // The sidecar allocation may move the object; the caller's handle
    // is updated in place.
    ensure_exotic(obj, heap)?;
    heap.with_payload(*obj, |body| {
        body.exotic_mut().error_data = true;
    });
    Ok(())
}

/// `true` when the object has the `[[ErrorData]]` internal slot. Unlike
/// a prototype-chain probe this is exact: `Object.create(Error.prototype)`
/// returns `false`.
#[must_use]
pub fn has_error_data(obj: JsObject, heap: &otter_gc::GcHeap) -> bool {
    heap.read_payload(obj, |body| body.error_data())
}

/// Captured frames on their way into an [`ErrorStackBody`]: fixed records
/// and one UTF-8 arena, filled from borrowed frame views without a
/// per-frame allocation. Only the top frame keeps its source line, the one
/// line an uncaught-error report shows.
#[derive(Default)]
pub(crate) struct ErrorStackDraft {
    records: Vec<ErrorFrameRecord>,
    bytes: Vec<u8>,
}

impl ErrorStackDraft {
    /// No frame was captured.
    pub(crate) fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Append the next frame, top of stack first.
    pub(crate) fn push(&mut self, frame: crate::stack_snapshot::StackFrameSnapshotView<'_>) {
        let position = frame.source.map(|source| {
            let (line, column) = source.line_col(frame.span.0);
            let text = if self.records.is_empty() {
                source.line_text(line).unwrap_or_default()
            } else {
                ""
            };
            (line, column.saturating_sub(1), text)
        });
        self.push_parts(
            frame.function_id,
            frame.function_name,
            frame.module,
            frame.span,
            position,
        );
    }

    /// Append an owned snapshot, top of stack first.
    pub(crate) fn push_snapshot(&mut self, frame: &crate::run_control::StackFrameSnapshot) {
        let top = self.records.is_empty();
        let position = frame.source_position.as_ref().map(|position| {
            let text: &str = if top { &position.source_line } else { "" };
            (position.line_number, position.start_column, text)
        });
        self.push_parts(
            frame.function_id,
            &frame.function_name,
            &frame.module,
            frame.span,
            position,
        );
    }

    fn push_parts(
        &mut self,
        function_id: u32,
        name: &str,
        module: &str,
        span: (u32, u32),
        position: Option<(u32, u32, &str)>,
    ) {
        let name_offset = self.bytes.len();
        self.bytes.extend_from_slice(name.as_bytes());
        let module_offset = self.bytes.len();
        self.bytes.extend_from_slice(module.as_bytes());
        let source_offset = self.bytes.len();
        let (line_number, start_column) = position.map_or((0, 0), |(line, column, text)| {
            self.bytes.extend_from_slice(text.as_bytes());
            (line, column)
        });
        self.records.push(ErrorFrameRecord {
            function_id,
            name_offset,
            name_len: name.len(),
            module_offset,
            module_len: module.len(),
            span_lo: span.0,
            span_hi: span.1,
            has_source: position.is_some(),
            line_number,
            start_column,
            source_offset,
            source_len: self.bytes.len() - source_offset,
        });
    }
}

/// Record the captured JS call-stack frames for an error object
/// (top-of-stack first). Replaces any previously captured frames and the
/// rendered `stack` string, as `Error.captureStackTrace` may re-capture onto
/// an existing target.
pub fn set_error_stack_frames(
    obj: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    frames: Vec<crate::run_control::StackFrameSnapshot>,
) -> Result<(), otter_gc::OutOfMemory> {
    let mut draft = ErrorStackDraft::default();
    for frame in &frames {
        draft.push_snapshot(frame);
    }
    set_error_stack(obj, heap, &draft)
}

/// Publish captured frames on an error object; see [`set_error_stack_frames`].
pub(crate) fn set_error_stack(
    obj: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    draft: &ErrorStackDraft,
) -> Result<(), otter_gc::OutOfMemory> {
    let frame_count = draft.records.len();
    let bytes = draft.bytes.len();
    let unaligned = (ErrorStackBody::trailing_bytes(frame_count, bytes) as u64)
        .saturating_add(std::mem::size_of::<ErrorStackBody>() as u64)
        .saturating_add(std::mem::size_of::<otter_gc::GcHeader>() as u64);
    let alignment = otter_gc::OBJECT_ALIGNMENT as u64;
    let requested = unaligned.saturating_add(alignment - 1) & !(alignment - 1);
    let max_bytes = u64::from(u32::MAX) & !(alignment - 1);
    if requested > max_bytes {
        return Err(otter_gc::OutOfMemory::AllocationTooLarge {
            requested_bytes: requested,
            max_bytes,
        });
    }
    ensure_exotic(obj, heap)?;
    let object_slot = std::ptr::from_mut(obj);
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        visitor(object_slot.cast::<RawGc>());
    };
    let stack = heap.alloc_variable_with_roots::<ErrorStackBody>(
        ErrorStackBody {
            frame_count,
            byte_len: bytes,
        },
        ErrorStackBody::trailing_bytes(frame_count, bytes),
        &mut visit,
    )?;
    // SAFETY: exact record and UTF-8 extents were admitted before allocation;
    // initialization performs no JS allocation or collection.
    unsafe {
        let body = error_stack_body_of(stack).expect("fresh stack body");
        std::ptr::copy_nonoverlapping(draft.records.as_ptr(), (*body).records_ptr(), frame_count);
        std::ptr::copy_nonoverlapping(draft.bytes.as_ptr(), (*body).bytes_ptr(), bytes);
    }
    heap.with_payload(*obj, |body| {
        let exotic = body.exotic_mut();
        exotic.error_stack_frames = stack;
        exotic.error_stack_string = None;
    });
    let sidecar = heap.read_payload(*obj, |body| body.exotic.get());
    heap.record_write(sidecar, &stack);
    Ok(())
}

/// The `stack` string rendered at the error's first `stack` read.
pub(crate) fn error_stack_string(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<JsString> {
    heap.read_payload(obj, |body| body.exotic().and_then(|e| e.error_stack_string))
}

/// Keep the rendered `stack` string of an error that has a sidecar.
pub(crate) fn set_error_stack_string(obj: JsObject, heap: &mut otter_gc::GcHeap, stack: JsString) {
    let sidecar = heap.with_payload(obj, |body| {
        body.exotic_mut().error_stack_string = Some(stack);
        body.exotic.get()
    });
    heap.record_write(sidecar, &Value::string(stack));
}

/// Read owned captured frames. Source-line copies are admitted before return;
/// a resource failure leaves the immutable managed snapshot unchanged.
pub fn error_stack_frames(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    account: &otter_resource::ResourceAccount,
) -> Result<Option<Vec<crate::run_control::StackFrameSnapshot>>, otter_resource::SharedSourceError>
{
    heap.read_payload(obj, |body| {
        body.exotic()
            .and_then(|e| error_stack_body_of(e.error_stack_frames))
            // SAFETY: a non-null handle names a live immutable body. Source
            // admission allocates only Rust-owned data, never a GC heap cell.
            .map(|stack| unsafe { (*stack).to_frames(account) })
            .transpose()
    })
}

/// Visit the one managed captured stack without retaining copied source lines.
/// The immutable arena stays borrowed for the callback; callers cannot collect
/// through the shared heap reference. This is the diagnostic formatting path,
/// not an unaccounted reconstruction of the owned source-position API.
pub(crate) fn visit_error_stack_frames(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    mut visitor: impl FnMut(&str, &str, Option<(u32, u32)>),
) -> bool {
    heap.read_payload(obj, |body| {
        let Some(stack) = body
            .exotic()
            .and_then(|e| error_stack_body_of(e.error_stack_frames))
        else {
            return false;
        };
        // SAFETY: the sidecar owns this live immutable managed body. No mutable
        // heap access or GC allocation occurs while the borrowed arena is read.
        let stack = unsafe { &*stack };
        for index in 0..stack.frame_count {
            // SAFETY: fully initialized records lie inside the published extent.
            let record = unsafe { *stack.records_ptr().add(index) };
            visitor(
                stack.str_at(record.name_offset, record.name_len),
                stack.str_at(record.module_offset, record.module_len),
                record
                    .has_source
                    .then_some((record.line_number, record.start_column)),
            );
        }
        stack.frame_count != 0
    })
}

/// `true` when the object carries captured stack frames.
#[must_use]
pub fn has_error_stack_frames(obj: JsObject, heap: &otter_gc::GcHeap) -> bool {
    heap.read_payload(obj, |body| body.has_error_stack_frames())
}

/// Tag an object as carrying the `[[IsRawJSON]]` internal slot
/// (§25.5.3 `JSON.rawJSON`).
///
/// # Errors
///
/// Preserves the actual sidecar allocation refusal before slot publication.
pub fn set_is_raw_json(
    obj: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    value: bool,
) -> Result<(), otter_gc::OutOfMemory> {
    // The sidecar allocation may move the object; the caller's handle
    // is updated in place before the nonallocating slot publication.
    ensure_exotic(obj, heap)?;
    heap.with_payload(*obj, |body| {
        body.exotic_mut().is_raw_json = value;
    });
    Ok(())
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

/// The object's hidden class: a keyed shape, or its lineage's dictionary
/// shape in dictionary mode.
#[must_use]
pub(crate) fn shape(obj: JsObject, heap: &otter_gc::GcHeap) -> ShapeHandle {
    heap.read_payload(obj, |body| body.shape)
}

/// The hidden class that fixes the object's layout, or null in dictionary
/// mode. Caches and generated guards that prove a layout by shape identity
/// must name this, never the lineage's shared dictionary shape.
#[must_use]
pub(crate) fn keyed_shape(obj: JsObject, heap: &otter_gc::GcHeap) -> ShapeHandle {
    heap.read_payload(obj, ObjectBody::keyed_shape)
}

/// `true` when the object is in dictionary mode.
#[must_use]
pub(crate) fn is_dictionary(obj: JsObject, heap: &otter_gc::GcHeap) -> bool {
    heap.read_payload(obj, ObjectBody::is_dictionary)
}

/// Invariant check after a shape-advancing append: the hidden class must
/// record `(flags, is_accessor)` for the freshly appended slot at the new last
/// offset. A shaped object carries no per-slot metadata of its own, so the
/// shape is the sole attribute source and must own that offset. Dictionary-mode
/// objects are skipped. Debug-only.
#[cfg(debug_assertions)]
pub(crate) fn debug_assert_appended_shape_slot(obj: JsObject, heap: &otter_gc::GcHeap) {
    let shape = shape(obj, heap);
    if shape_body::is_dictionary_of(shape) {
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
    heap.read_payload(obj, |body| body.extensible())
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
        if body.extensible() {
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
        if body.extensible() {
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

/// The data-write half of ordinary `[[Set]]` after resolver selection.
///
/// Existing writable data keeps its attributes; an accessor or readonly data
/// rejects. A missing property receives default data attributes only when the
/// receiver is extensible. The receiver slot is refreshed across collection.
/// Allocation failure retains its actual cause independently from rejection.
///
/// # Spec
/// - <https://tc39.es/ecma262/#sec-ordinarysetwithowndescriptor>
pub fn ordinary_set_data_property(
    obj: &mut JsObject,
    heap: &mut GcHeap,
    key: &str,
    value: Value,
) -> Result<bool, otter_gc::OutOfMemory> {
    ordinary_set::string(obj, heap, key, value, None)
}

/// Ordinary data assignment with a previously prepared append shape.
/// The shape owns the slot index and is rooted by descriptor preparation.
pub(crate) fn ordinary_set_data_property_with_shape(
    obj: &mut JsObject,
    heap: &mut GcHeap,
    key: &str,
    value: Value,
    next_shape: ShapeHandle,
) -> Result<bool, otter_gc::OutOfMemory> {
    ordinary_set::string(obj, heap, key, value, Some(next_shape))
}

/// Head of the traced immutable capacity-root chain cached on `proto`.
#[must_use]
pub(crate) fn cached_instance_root(
    proto: JsObject,
    heap: &otter_gc::GcHeap,
) -> Option<ShapeHandle> {
    heap.read_payload(proto, |body| {
        body.exotic()
            .map(|exotic| exotic.instance_root)
            .filter(|root| !root.is_null())
    })
}

/// Cache `root` on the prototype `proto` for its instances; the sidecar may
/// be allocated, moving `proto`.
pub(crate) fn cache_instance_root(
    proto: &mut JsObject,
    heap: &mut GcHeap,
    mut root: ShapeHandle,
) -> Result<(), otter_gc::OutOfMemory> {
    use crate::rooting::RootScopeExt;
    let mut roots = otter_gc::RootScope::new(heap);
    // SAFETY: the receiver and prepared root slots precede this registration
    // and remain stationary until both publications and barriers complete.
    unsafe {
        roots.add_object(proto);
        roots.add_raw_slot(std::ptr::addr_of_mut!(root).cast::<RawGc>());
    }
    ensure_exotic(proto, heap)?;
    let target = state(*proto, heap).with_prototype_role(true);
    state_transition::transition_state(proto, heap, target)?;
    heap.with_payload(*proto, |body| body.exotic_mut().instance_root = root);
    let sidecar = heap.read_payload(*proto, |body| body.exotic.get());
    heap.record_write(sidecar, &root);
    Ok(())
}

/// What §10.1.2.1 OrdinarySetPrototypeOf decides before a prototype change.
#[derive(Debug, Clone)]
pub(crate) enum PrototypeChange {
    /// Step 4: the prototype already is `proto`.
    Unchanged,
    /// Steps 5 and 8: the object is non-extensible, or `proto`'s chain
    /// reaches the object.
    Rejected,
    /// The object's shape moves to `proto`'s lineage.
    To(ObjectPrototype),
}

/// §10.1.2.1 OrdinarySetPrototypeOf steps 4–8 for `obj` and `proto`
/// (`None` or a `null` value for `null`). A primitive `proto` is rejected;
/// callers raise the spec's `TypeError` for a `false` answer.
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-ordinarysetprototypeof>
pub(crate) fn prototype_change(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    proto: Option<Value>,
) -> PrototypeChange {
    let new_proto = match proto {
        None => ObjectPrototype::Null,
        Some(value) if value.is_null() => ObjectPrototype::Null,
        Some(value) => {
            if let Some(o) = value.as_object() {
                ObjectPrototype::Object(o)
            } else if let Some(p) = value.as_proxy() {
                ObjectPrototype::Proxy(p)
            } else if value.is_object_type() {
                ObjectPrototype::Value(value)
            } else {
                return PrototypeChange::Rejected;
            }
        }
    };
    // Step 4 — `SameValue(V, current) is true → return true`.
    let current = heap.read_payload(obj, |body| body.prototype());
    if prototype_same(&current, &new_proto) {
        return PrototypeChange::Unchanged;
    }
    // Step 5 — non-extensible objects reject any change.
    if !is_extensible(obj, heap) {
        return PrototypeChange::Rejected;
    }
    // Step 8 — walk the new chain; abort when a hop lands back on `obj`
    // (cycle) or strays past `PROTO_CHAIN_HARD_CAP` (a safety net for
    // adversarial inputs). Non-ordinary prototypes (Proxy / Value) end the
    // walk per step 8.c.i: their `[[GetPrototypeOf]]` is not
    // `OrdinaryGetPrototypeOf`.
    let mut cursor = new_proto.clone();
    let mut hops = 0usize;
    loop {
        match cursor {
            ObjectPrototype::Null => break,
            ObjectPrototype::Object(p) => {
                if p == obj || hops >= PROTO_CHAIN_HARD_CAP {
                    return PrototypeChange::Rejected;
                }
                hops += 1;
                cursor = heap.read_payload(p, |body| body.prototype());
            }
            ObjectPrototype::Proxy(_) | ObjectPrototype::Value(_) => break,
        }
    }
    PrototypeChange::To(new_proto)
}

/// Install `shape` — the object's layout replayed on a new prototype's
/// lineage, or that lineage's dictionary shape for a dictionary object —
/// completing a prototype change. A dictionary object takes a fresh
/// structural identity and slot-layout epoch, since its old identity named
/// the old prototype.
pub(crate) fn install_prototype_shape(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    shape: ShapeHandle,
) {
    debug_assert_object_shape_handle(shape, "prototype change");
    heap.with_payload(obj, |body| {
        let dictionary = body.is_dictionary();
        debug_assert_eq!(
            dictionary,
            shape_body::is_dictionary_of(shape),
            "a prototype change keeps the object's storage mode",
        );
        debug_assert!(
            dictionary || body.slot_count() == shape_body::property_count_of(shape) as usize,
            "a prototype change keeps the object's slots",
        );
        body.invalidate_prototype_proofs();
        assert_eq!(
            body.inline_capacity(),
            shape_body::inline_capacity_of(shape),
            "shape transition changed object footprint"
        );
        body.shape = shape;
        if dictionary {
            body.enter_dictionary_mode_as(next_shape_id(), true);
        }
    });
    // The new lineage may be unmarked while the object is already marked.
    heap.record_write(obj, &shape);
}

/// §10.1.2.1 OrdinarySetPrototypeOf without a shape runtime — for heap-only
/// installers. `None` or a `null` value detaches the chain; `false` is the
/// spec's rejection, which callers raise as a `TypeError`.
///
/// The object moves to the new prototype's lineage: an object with no keys
/// yet to its root, a dictionary object to its dictionary shape. With no
/// transition table to replay keys through, a keyed object normalizes to
/// dictionary storage; [`crate::Interpreter::set_ordinary_prototype`] keeps it
/// keyed.
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-ordinarysetprototypeof>
pub fn set_prototype_value(
    obj: &mut JsObject,
    heap: &mut GcHeap,
    proto: Option<Value>,
) -> Result<bool, otter_gc::OutOfMemory> {
    use crate::rooting::RootScopeExt;
    let mut new_proto = proto.unwrap_or(Value::null());
    let mut roots = otter_gc::RootScope::new(heap);
    // SAFETY: the caller receiver and prototype value remain stationary for the
    // whole operation, including shape/table allocations and final publication.
    unsafe {
        roots.add_object(obj);
        roots.add_value(&mut new_proto);
    }
    let new_proto = match prototype_change(*obj, heap, Some(new_proto)) {
        PrototypeChange::Unchanged => return Ok(true),
        PrototypeChange::Rejected => return Ok(false),
        PrototypeChange::To(prototype) => prototype,
    };
    let source = shape(*obj, heap);
    let target = shape_body::state_of(source).with_dictionary(false);
    let root = heap_instance_root(
        new_proto,
        heap,
        shape_body::inline_capacity_of(source),
        target,
        &mut |_| {},
    )?;
    // The prepared root stays alive through dictionary table allocations.
    // SAFETY: the heap arena outlives this scope and no Local escapes.
    let scope = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let root = scope.local(root);
    if !shape_body::is_dictionary_of(source) && shape_body::property_count_of(source) == 0 {
        install_prototype_shape(*obj, heap, root.get());
        return Ok(true);
    }
    normalize_to_dictionary(obj, heap)?;
    install_prototype_shape(*obj, heap, shape_body::dictionary_of(root.get()));
    Ok(true)
}

/// [`set_prototype_value`] for an ordinary object or `null` prototype.
pub fn set_prototype(
    obj: &mut JsObject,
    heap: &mut GcHeap,
    proto: Option<JsObject>,
) -> Result<bool, otter_gc::OutOfMemory> {
    set_prototype_value(obj, heap, proto.map(Value::object))
}

/// The root shape of objects created with the ordinary `prototype` (or
/// `null`) in a heap-only context. Explicit roots cover collecting allocation.
pub(crate) fn root_for_prototype(
    heap: &mut GcHeap,
    prototype: Option<JsObject>,
    state: ShapeState,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<ShapeHandle, otter_gc::OutOfMemory> {
    heap_instance_root(
        prototype.map_or(ObjectPrototype::Null, ObjectPrototype::Object),
        heap,
        DEFAULT_INLINE_CAPACITY,
        state,
        external_visit,
    )
}

/// The root shape of objects created with the prototype value `proto`
/// (`None` or `null` for none) in a heap-only context with explicit roots.
pub(crate) fn root_for_prototype_value(
    heap: &mut GcHeap,
    proto: Option<Value>,
    state: ShapeState,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<ShapeHandle, otter_gc::OutOfMemory> {
    heap_instance_root(
        object_prototype_of_value(proto),
        heap,
        DEFAULT_INLINE_CAPACITY,
        state,
        external_visit,
    )
}

/// The prototype value `proto` (`None`, `null` or `undefined` for none, else
/// an object value) as an object's `[[Prototype]]`.
pub(crate) fn object_prototype_of_value(proto: Option<Value>) -> ObjectPrototype {
    match proto {
        None => ObjectPrototype::Null,
        Some(value) if value.is_null() || value.is_undefined() => ObjectPrototype::Null,
        Some(value) => {
            if let Some(object) = value.as_object() {
                ObjectPrototype::Object(object)
            } else if let Some(proxy) = value.as_proxy() {
                ObjectPrototype::Proxy(proxy)
            } else {
                ObjectPrototype::Value(value)
            }
        }
    }
}

/// `prototype`'s root shape without a shape runtime: the `null`-prototype
/// root, the one cached on an ordinary prototype (created and cached on first
/// use), or a fresh root. The runtime registers such a root when it next
/// meets it.
pub(crate) fn heap_instance_root(
    prototype: ObjectPrototype,
    heap: &mut GcHeap,
    capacity: usize,
    state: ShapeState,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<ShapeHandle, otter_gc::OutOfMemory> {
    use crate::rooting::RootScopeExt;
    let mut prototype_value = match prototype {
        ObjectPrototype::Null => Value::null(),
        ObjectPrototype::Object(object) => Value::object(object),
        ObjectPrototype::Proxy(proxy) => Value::proxy(proxy),
        ObjectPrototype::Value(value) => value,
    };
    let mut ordinary_proto = prototype_value.as_object().unwrap_or_else(JsObject::null);
    let mut roots = otter_gc::RootScope::new(heap);
    // SAFETY: both local slots precede the scope and stay stationary; pending
    // shape allocation can collect and rewrites these exact values in place.
    unsafe {
        roots.add_value(&mut prototype_value);
        roots.add_object(&mut ordinary_proto);
    }
    let state = state.with_dictionary(false).with_lookup(
        LookupFact::NonOrdinaryPrototype,
        !prototype_value.is_null() && ordinary_proto.is_null(),
    );
    if state.is_provisional() {
        let prototype = if prototype_value.is_null() {
            shape_body::ShapePrototype::Null
        } else if !ordinary_proto.is_null() {
            shape_body::ShapePrototype::Object(ordinary_proto)
        } else {
            shape_body::ShapePrototype::Value(prototype_value)
        };
        return shape_body::alloc_root_shape_body_with_roots(
            heap,
            prototype,
            capacity,
            ShapeHandle::null(),
            state,
            external_visit,
        );
    }
    if prototype_value.is_null() {
        let head = shape_body::null_root_head(heap);
        if let Some(root) = shape_body::root_for_layout(head, capacity, state) {
            return Ok(root);
        }
        let root = shape_body::alloc_root_shape_body_with_roots(
            heap,
            shape_body::ShapePrototype::Null,
            capacity,
            head,
            state,
            external_visit,
        )?;
        shape_body::set_null_root(heap, root);
        return Ok(root);
    }
    if !ordinary_proto.is_null() {
        let previous = cached_instance_root(ordinary_proto, heap).unwrap_or_else(ShapeHandle::null);
        if let Some(root) = shape_body::root_for_layout(previous, capacity, state) {
            return Ok(root);
        }
        let root = shape_body::alloc_root_shape_body_with_roots(
            heap,
            shape_body::ShapePrototype::Object(ordinary_proto),
            capacity,
            previous,
            state,
            external_visit,
        )?;
        cache_instance_root(&mut ordinary_proto, heap, root)?;
        return Ok(root);
    }
    shape_body::alloc_root_shape_body_with_roots(
        heap,
        shape_body::ShapePrototype::Value(prototype_value),
        capacity,
        ShapeHandle::null(),
        state,
        external_visit,
    )
}

/// A shape prototype in the ordinary mutation owner's representation.
fn object_prototype_of_shape(prototype: shape_body::ShapePrototype) -> ObjectPrototype {
    match prototype {
        shape_body::ShapePrototype::Null => ObjectPrototype::Null,
        shape_body::ShapePrototype::Object(object) => ObjectPrototype::Object(object),
        shape_body::ShapePrototype::Value(value) => object_prototype_of_value(Some(value)),
    }
}

/// Out-of-object properties past which a keyed store adds no further
/// property to a fast object (V8's `kFastPropertiesSoftLimit`).
const KEYED_FAST_PROPERTIES: usize = 12;

/// Leave `obj` in dictionary mode before a keyed store adds a property to it
/// once its out-of-object properties reach [`KEYED_FAST_PROPERTIES`], as
/// V8's `Map::TooManyFastProperties` does for a keyed store origin: computed
/// keys make hash tables, and every key appended to a transition chain makes
/// each later lookup walk it. A prototype, a non-extensible or exotic object
/// keeps its layout.
pub(crate) fn normalize_before_keyed_addition(
    obj: &mut JsObject,
    heap: &mut GcHeap,
) -> Result<(), otter_gc::OutOfMemory> {
    if !keyed_addition_normalizes(*obj, heap) {
        return Ok(());
    }
    normalize_to_dictionary(obj, heap)
}

/// Whether a keyed store adding a property to `obj` first leaves it in
/// dictionary mode ([`normalize_before_keyed_addition`]).
pub(crate) fn keyed_addition_normalizes(obj: JsObject, heap: &GcHeap) -> bool {
    let shape = shape(obj, heap);
    let state = shape_body::state_of(shape);
    if state.is_dictionary() || !state.is_extensible() || state.is_prototype() || state.is_opaque()
    {
        return false;
    }
    let count = shape_body::property_count_of(shape) as usize;
    count >= shape_body::inline_capacity_of(shape) + KEYED_FAST_PROPERTIES
}

/// Move a keyed object's string-keyed properties into dictionary storage,
/// keeping their order, values and attributes. A dictionary object is left
/// as is.
fn normalize_to_dictionary(
    obj: &mut JsObject,
    heap: &mut GcHeap,
) -> Result<(), otter_gc::OutOfMemory> {
    materialize_slots_with_pending_values(obj, heap, &mut [])
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

/// Remove an own property. Per ECMA-262 §10.1.10 OrdinaryDelete:
/// returns `true` when the property is absent or successfully
/// removed; returns `false` only when the property exists and is
/// non-configurable.
///
/// # Spec
///
/// - <https://tc39.es/ecma262/#sec-ordinarydelete>
pub fn delete(
    obj: &mut JsObject,
    heap: &mut GcHeap,
    key: &str,
) -> Result<bool, otter_gc::OutOfMemory> {
    let Some(offset) = heap.read_payload(*obj, |body| body_offset_of(heap, body, key)) else {
        return Ok(true);
    };
    let (flags, _) = heap.read_payload(*obj, |body| body.slot_attrs(heap, offset as usize));
    if !flags.configurable() {
        return Ok(false);
    }
    let source = shape(*obj, heap);
    if !shape_body::is_dictionary_of(source) && offset + 1 == shape_body::property_count_of(source)
    {
        let parent = heap.read_payload(source, shape_body::ShapeBody::parent);
        assert!(
            !parent.is_null(),
            "a counted final property has a shape parent"
        );
        heap.with_payload(*obj, |body| {
            body.invalidate_prototype_proofs();
            // No counted slot can ever retain the deleted moving child.
            unsafe {
                *body.field_ptr(body.location_for_slot(offset as usize)) = Value::undefined();
            }
            body.shape = parent;
            remove_mapped_argument(body, key);
            body.debug_verify_field_layout();
        });
        heap.record_write(*obj, &parent);
        return Ok(true);
    }
    // Non-final deletion compacts actual key/value/descriptor order. All tables
    // are prepared with the receiver rooted; no state latch survives migration.
    materialize_slots(obj, heap)?;
    let mut keys = heap.read_payload(*obj, |body| string_keys_in_shape_order(heap, body));
    keys.remove(offset as usize);
    let table = dict_keys_table_for_install(obj, heap, &Some(keys), "", &mut [])?
        .expect("Some keys prepare a replacement table");
    heap.with_payload(*obj, |body| {
        body.enter_dictionary_mode(false);
        body.remove_slot(offset as usize);
        body.exotic_mut().dictionary_keys = table;
        remove_mapped_argument(body, key);
    });
    let sidecar = heap.read_payload(*obj, |body| body.exotic.get());
    heap.record_write(sidecar, &table);
    Ok(true)
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
    obj: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    mut expected: Value,
) -> Result<bool, otter_gc::OutOfMemory> {
    use crate::rooting::RootScopeExt;
    let mut roots = otter_gc::RootScope::new(heap);
    // SAFETY: both receiver and pending comparison value remain stationary
    // through metadata allocation and the guarded transaction rollback.
    unsafe {
        roots.add_object(obj);
        roots.add_value(&mut expected);
    }
    let existing_offset = heap.read_payload(*obj, |body| body_offset_of(heap, body, key));
    let Some(offset) = existing_offset else {
        return Ok(false);
    };

    let matches_expected = heap.read_payload(*obj, |body| {
        let offset = offset as usize;
        !body.slot_attrs(heap, offset).1
            && crate::abstract_ops::is_strictly_equal(
                &body.data_value(heap, offset),
                &expected,
                heap,
            )
    });
    if !matches_expected {
        return Ok(false);
    }
    materialize_slots(obj, heap)?;

    let replacement_keys = heap.read_payload(*obj, |body| {
        let mut keys = string_keys_in_shape_order(heap, body);
        let offset = offset as usize;
        if offset < keys.len() {
            keys.remove(offset);
        }
        keys
    });
    let replacement_table =
        dict_keys_table_for_install(obj, heap, &Some(replacement_keys), "", &mut [])?;
    heap.with_payload(*obj, |body| {
        body.enter_dictionary_mode(false);
        body.remove_slot(offset as usize);
        if let Some(table) = replacement_table {
            body.exotic_mut().dictionary_keys = table;
        }
        remove_mapped_argument(body, key);
    });
    if let Some(table) = replacement_table {
        let sidecar = heap.read_payload(*obj, |body| body.exotic.get());
        heap.record_write(sidecar, &table);
    }
    Ok(true)
}

/// Set or overwrite a symbol-keyed own data property through the
/// same descriptor-aware `[[Set]]` data-write core as string keys.
///
/// Fires the GC write barrier when `value` carries a `Gc<…>`
/// handle.
pub fn ordinary_set_symbol_data_property(
    obj: &mut JsObject,
    heap: &mut GcHeap,
    key: JsSymbol,
    value: Value,
) -> Result<bool, otter_gc::OutOfMemory> {
    ordinary_set::symbol(obj, heap, key, value)
}

/// Remove a symbol-keyed own property.
pub fn delete_symbol(obj: JsObject, heap: &mut otter_gc::GcHeap, key: JsSymbol) -> bool {
    heap.with_payload(obj, |body| {
        if let Some(pos) = body.symbol_props().iter().position(|(k, _)| k.ptr_eq(key)) {
            if !body.symbol_props()[pos].1.flags.configurable() {
                return false;
            }
            body.invalidate_prototype_proofs();
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
) -> Result<bool, otter_gc::OutOfMemory> {
    descriptor_install::define_string(obj_ref, heap, key, descriptor)
}

pub(crate) fn define_own_property_partial_with_shape(
    obj_ref: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    descriptor: PartialPropertyDescriptor,
    next_shape: ShapeHandle,
) -> Result<bool, otter_gc::OutOfMemory> {
    descriptor_install::define_string_with_shape(obj_ref, heap, key, descriptor, next_shape)
}

/// Field-presence-aware §10.1.6.3 for symbol-keyed properties.
/// Allocation failures preserve their typed cause; `false` means rejection.
pub fn define_own_symbol_property_partial(
    obj_ref: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    key: JsSymbol,
    descriptor: PartialPropertyDescriptor,
) -> Result<bool, otter_gc::OutOfMemory> {
    descriptor_install::define_symbol(obj_ref, heap, key, descriptor)
}

/// §10.1.6.3 for a fully specified string-keyed descriptor.
/// Both full and partial forms use the same field-presence validation owner.
pub fn define_own_property(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    descriptor: PropertyDescriptor,
) -> Result<bool, otter_gc::OutOfMemory> {
    define_own_property_in_place(&mut { obj }, heap, key, descriptor)
}

/// Define a full descriptor and update the caller's moving receiver slot.
/// Allocation failures preserve their typed cause; `false` means rejection.
pub fn define_own_property_in_place(
    obj_ref: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    descriptor: PropertyDescriptor,
) -> Result<bool, otter_gc::OutOfMemory> {
    define_own_property_partial(
        obj_ref,
        heap,
        key,
        PartialPropertyDescriptor::from_full(&descriptor),
    )
}

/// Define a full symbol descriptor through the same partial validation owner.
pub fn define_own_symbol_property(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: JsSymbol,
    descriptor: PropertyDescriptor,
) -> Result<bool, otter_gc::OutOfMemory> {
    define_own_symbol_property_partial(
        &mut { obj },
        heap,
        key,
        PartialPropertyDescriptor::from_full(&descriptor),
    )
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
                        *value = mapped_read(heap, cell);
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
pub fn prevent_extensions(
    obj: &mut JsObject,
    heap: &mut GcHeap,
) -> Result<(), otter_gc::OutOfMemory> {
    let target = state(*obj, heap).with_extensible(false);
    state_transition::transition_state(obj, heap, target)
}

/// `Object.seal(o)` core — clears `[[Extensible]]` and toggles
/// `[[Configurable]]` to `false` on every own property.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-setintegritylevel>
pub fn seal(obj: &mut JsObject, heap: &mut GcHeap) -> Result<(), otter_gc::OutOfMemory> {
    integrity_transition::apply(obj, heap, descriptor_mutation::IntegrityLevel::Sealed)
}

/// `Object.seal` for a shaped object, transitioning to `new_shape` — the
/// attribute-encoding hidden class that records every slot as non-configurable
/// and carries non-extensible state. Shape descriptors are the only ordinary
/// attribute authority; symbol-keyed slots mutate through the retirement owner.
pub(crate) fn seal_with_shape(obj: JsObject, heap: &mut otter_gc::GcHeap, new_shape: ShapeHandle) {
    heap.with_payload(obj, |body| {
        debug_assert_object_shape_handle(new_shape, "shape-slot store");
        assert!(
            !shape_body::state_of(new_shape).is_extensible(),
            "integrity shape is non-extensible"
        );
        assert_eq!(
            body.inline_capacity(),
            shape_body::inline_capacity_of(new_shape),
            "shape transition changed object footprint"
        );
        descriptor_mutation::apply_integrity_level(
            body,
            descriptor_mutation::IntegrityLevel::Sealed,
        );
        if body.shape != new_shape {
            body.invalidate_prototype_proofs();
            body.shape = new_shape;
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
pub fn freeze(obj: &mut JsObject, heap: &mut GcHeap) -> Result<(), otter_gc::OutOfMemory> {
    integrity_transition::apply(obj, heap, descriptor_mutation::IntegrityLevel::Frozen)
}

/// `Object.freeze` for a shaped object, transitioning to `new_shape` — the
/// attribute-encoding hidden class that records data slots as
/// non-writable/non-configurable and accessor slots as non-configurable, with
/// immutable non-extensible state. Symbols use the common retirement owner.
pub(crate) fn freeze_with_shape(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    new_shape: ShapeHandle,
) {
    heap.with_payload(obj, |body| {
        debug_assert_object_shape_handle(new_shape, "shape-slot store");
        assert!(
            !shape_body::state_of(new_shape).is_extensible(),
            "integrity shape is non-extensible"
        );
        assert_eq!(
            body.inline_capacity(),
            shape_body::inline_capacity_of(new_shape),
            "shape transition changed object footprint"
        );
        descriptor_mutation::apply_integrity_level(
            body,
            descriptor_mutation::IntegrityLevel::Frozen,
        );
        if body.shape != new_shape {
            body.invalidate_prototype_proofs();
            body.shape = new_shape;
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
    let insertion_order = if !body.is_dictionary() {
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
) -> Option<(PropertyFlags, bool, u32)> {
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
        if !body.is_dictionary() || body.state().is_opaque() {
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
        body.invalidate_prototype_proofs();
        assert_eq!(
            body.inline_capacity(),
            shape_body::inline_capacity_of(shape),
            "shape transition changed object footprint"
        );
        body.shape = shape;
        if let Some(exotic) = exotic_body_of(body.exotic.get()).map(|e|
            // SAFETY: a non-null handle names a live sidecar payload.
            unsafe { &mut *e })
        {
            exotic.dictionary_shape_id = ShapeId::UNASSIGNED;
            exotic.dictionary_slot_count = 0;
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
    if !body.is_dictionary() {
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
    existing_offset: Option<u32>,
) -> Option<Vec<String>> {
    if existing_offset.is_some() {
        return None;
    }
    heap.read_payload(obj, |body| {
        (!body.is_dictionary()).then(|| string_keys_in_shape_order(heap, body))
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
    existing_offset: Option<u32>,
) -> Option<Vec<SlotMeta>> {
    if existing_offset.is_some() {
        return None;
    }
    heap.read_payload(obj, |body| {
        (!body.is_dictionary()).then(|| materialized_slot_metas(heap, body))
    })
}

/// Read every current slot's `(flags, is_accessor)` from the authoritative
/// source into an index-aligned [`SlotMeta`] vector.
fn materialized_slot_metas(heap: &otter_gc::GcHeap, body: &ObjectBody) -> Vec<SlotMeta> {
    let count = body_property_count(heap, body);
    (0..count)
        .map(|i| {
            let (flags, is_accessor) = body.slot_attrs(heap, i);
            SlotMeta {
                flags,
                is_accessor,
                watched: false,
            }
        })
        .collect()
}

/// Normalize a shaped object to its same-geometry dictionary companion before
/// a no-shape descriptor mutation. Values stay in their existing field banks.
fn materialize_slots(obj: &mut JsObject, heap: &mut GcHeap) -> Result<(), otter_gc::OutOfMemory> {
    materialize_slots_with_pending_values(obj, heap, &mut [])
}

/// Prepare dictionary tables with actual receiver/pending roots before one
/// nonallocating publication. OOM leaves the old shape and descriptors intact.
fn materialize_slots_with_pending_values(
    obj: &mut JsObject,
    heap: &mut GcHeap,
    pending: &mut [Value],
) -> Result<(), otter_gc::OutOfMemory> {
    let Some((slots, keys)) = prepare_dictionary_tables(obj, heap, pending)? else {
        return Ok(());
    };
    heap.with_payload(*obj, |body| {
        body.enter_dictionary_mode(false);
        body.exotic_mut().slots = slots;
        body.exotic_mut().dictionary_keys = keys;
    });
    let sidecar = heap.read_payload(*obj, |body| body.exotic.get());
    heap.record_write(sidecar, &slots);
    heap.record_write(sidecar, &keys);
    Ok(())
}

/// Prepare same-geometry dictionary tables without publishing descriptor or
/// storage state. The caller roots their returned handles before allocating.
fn prepare_dictionary_tables(
    obj: &mut JsObject,
    heap: &mut GcHeap,
    pending: &mut [Value],
) -> Result<Option<(SlotMetaHandle, DictKeysHandle)>, otter_gc::OutOfMemory> {
    if is_dictionary(*obj, heap) {
        return Ok(None);
    }
    ensure_exotic_with_pending_values(obj, heap, pending)?;
    let (keys, metas, count) = heap.read_payload(*obj, |body| {
        (
            string_keys_in_shape_order(heap, body),
            materialized_slot_metas(heap, body),
            body_property_count(heap, body),
        )
    });
    let mut slots = slot_meta_table_for_install(obj, heap, &Some(metas), count, pending)?
        .expect("Some metadata always prepares a table");
    let mut prepared = otter_gc::RootScope::new(heap);
    // SAFETY: this stationary local precedes the scope and is returned without
    // another allocation. The next key-table allocation may full-collect it.
    unsafe {
        prepared.add_raw_slot(std::ptr::addr_of_mut!(slots).cast::<RawGc>());
    }
    let keys = dict_keys_table_for_install(obj, heap, &Some(keys), "", pending)?
        .expect("Some keys always prepare a table");
    Ok(Some((slots, keys)))
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

/// The string keys of a shaped object as the hidden class's own key
/// strings, in ordinary own-key order: array indices ascending, then every
/// other key in insertion order. Nothing is allocated on the heap, so the
/// values need no rooting while the caller collects them. `None` for a
/// dictionary-mode object, whose keys are not string cells.
pub(crate) fn shaped_string_key_values(obj: JsObject, heap: &GcHeap) -> Option<Vec<Value>> {
    heap.read_payload(obj, |body| {
        if body.is_dictionary() {
            return None;
        }
        let keys = shape_body::shape_keys_ordered(heap, body.shape);
        let mut indices = Vec::new();
        let mut names = Vec::with_capacity(keys.len());
        for (key, _) in keys {
            let index = crate::string::gc_body::with_latin1(
                heap,
                key,
                key_order::array_index_property_bytes,
            )
            .unwrap_or_else(|| {
                // A wide key holds a non-digit; a rope is read in full.
                let units = to_utf16_vec(heap, key);
                let bytes: Option<Vec<u8>> =
                    units.iter().map(|&unit| u8::try_from(unit).ok()).collect();
                bytes.and_then(|bytes| key_order::array_index_property_bytes(&bytes))
            });
            match index {
                Some(index) => indices.push((index, key)),
                None => names.push(key),
            }
        }
        indices.sort_by_key(|&(index, _)| index);
        Some(
            indices
                .into_iter()
                .map(|(_, key)| key)
                .chain(names)
                .map(|key| Value::string(JsString::from_handle(key, heap)))
                .collect(),
        )
    })
}

/// The symbol keys of `obj`'s own properties, Private Names excluded, in
/// insertion order.
pub(crate) fn own_symbol_key_values(obj: JsObject, heap: &GcHeap) -> Vec<Value> {
    heap.read_payload(obj, |body| {
        body.symbol_props()
            .iter()
            .map(|(key, _)| *key)
            .filter(|key| !key.is_private_name())
            .map(Value::symbol)
            .collect()
    })
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
        assert_eq!(std::mem::size_of::<ShapeState>(), 1);
        assert_eq!(OBJECT_BODY_EXOTIC_HANDLE_OFFSET, 8);
        assert_eq!(std::mem::size_of::<ExoticHandle>(), 4);
        let bits = [
            ShapeState::DICTIONARY_MASK,
            ShapeState::EXTENSIBLE_MASK,
            ShapeState::PROTOTYPE_MASK,
            ShapeState::STRING_WRAPPER_MASK,
            ShapeState::MAPPED_ARGUMENTS_MASK,
            ShapeState::HOST_LOOKUP_MASK,
            ShapeState::OPAQUE_PROTOTYPE_MASK,
            ShapeState::PROVISIONAL_MASK,
        ];
        assert_eq!(bits.iter().fold(0u8, |all, bit| all | bit), u8::MAX);
        assert_eq!(object_cell_bytes(2), 40);
    }

    #[test]
    fn empty_object_starts_with_zero_props() {
        let mut heap = fresh_heap();
        let o = alloc_object_old_for_fixture(&mut heap).unwrap();
        assert!(is_empty(o, &heap));
        assert_eq!(len(o, &heap), 0);
        assert_eq!(shape(o, &heap), shape_body::null_root(&heap));
    }

    #[test]
    fn runtime_object_allocation_installs_shape_root() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");

        assert_eq!(shape(o, interp.gc_heap()), interp.null_prototype_root());
    }

    #[test]
    fn runtime_data_assignment_advances_shape() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        interp.with_handle_scope(|interp, scope| {
            let receiver = interp.scoped_object_bare(scope).expect("object");
            let o = interp
                .escape_scoped(receiver)
                .as_object()
                .expect("object receiver");

            assert!(
                interp
                    .ordinary_set_data_property(o, "x", Value::boolean(true))
                    .expect("set")
            );

            // Assignment takes a value copy; the handle remains the current
            // receiver authority after a collecting hidden-class transition.
            let o = interp
                .escape_scoped(receiver)
                .as_object()
                .expect("current object receiver");
            let shape_handle = shape(o, interp.gc_heap());
            assert_eq!(interp.shape_offset_of(shape_handle, "x"), Some(0));
            assert_eq!(
                interp
                    .gc_heap()
                    .read_payload(o, |body| body.dict_key_count()),
                0
            );
        });
    }

    #[test]
    fn runtime_construction_set_advances_shape() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let mut o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");

        interp
            .create_data_property(&mut o, "value", Value::number_i32(1))
            .expect("set value");
        interp
            .create_data_property(&mut o, "done", Value::boolean(false))
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
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let mut o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");

        for i in 0..MAX_FAST_PROPERTIES {
            let key = format!("p{i}");
            interp
                .create_data_property(&mut o, &key, Value::number_i32(i as i32))
                .expect("set fast property");
        }
        assert!(!is_dictionary(o, interp.gc_heap()));

        assert!(
            define_own_property_in_place(
                &mut o,
                interp.gc_heap_mut(),
                "overflow",
                PropertyDescriptor::data(Value::boolean(true), true, true, true)
            )
            .expect("fixture property allocation")
        );

        assert!(is_dictionary(o, interp.gc_heap()));
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
    fn keyed_additions_past_the_soft_limit_normalize_only_ordinary_objects() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let mut o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");
        let limit =
            shape_body::inline_capacity_of(shape(o, interp.gc_heap())) + KEYED_FAST_PROPERTIES;
        for i in 0..limit {
            interp
                .create_data_property(&mut o, &format!("k{i}"), Value::number_i32(i as i32))
                .expect("fast property");
            if i + 1 < limit {
                normalize_before_keyed_addition(&mut o, interp.gc_heap_mut()).expect("check");
                assert!(!is_dictionary(o, interp.gc_heap()), "below the limit at {i}");
            }
        }
        normalize_before_keyed_addition(&mut o, interp.gc_heap_mut()).expect("normalize");
        assert!(is_dictionary(o, interp.gc_heap()));
        assert_eq!(get_own(o, interp.gc_heap(), "k0"), Some(Value::number_i32(0)));
        let keys: Vec<String> = with_properties(o, interp.gc_heap(), |p| {
            p.keys().map(str::to_string).collect()
        });
        assert_eq!(keys.len(), limit);
        assert_eq!(keys.last().map(String::as_str), Some(format!("k{}", limit - 1).as_str()));

        let mut prototype = interp
            .realm_intrinsics
            .object_prototype()
            .expect("realm Object.prototype");
        let was_dictionary = is_dictionary(prototype, interp.gc_heap());
        normalize_before_keyed_addition(&mut prototype, interp.gc_heap_mut()).expect("check");
        assert_eq!(is_dictionary(prototype, interp.gc_heap()), was_dictionary);
    }

    #[test]
    fn dictionary_redefinition_retires_its_structural_id() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let mut prototype = interp
            .realm_intrinsics
            .string_prototype()
            .expect("realm String.prototype");
        assert!(
            is_dictionary(prototype, interp.gc_heap()),
            "a String wrapper prototype stays in dictionary mode"
        );
        let original = shape_id(prototype, interp.gc_heap());

        assert!(
            ordinary_set_data_property(
                &mut prototype,
                interp.gc_heap_mut(),
                "charCodeAt",
                Value::number_i32(1)
            )
            .expect("fixture assignment allocation")
        );
        assert_eq!(
            shape_id(prototype, interp.gc_heap()),
            original,
            "a same-slot value write keeps the key/slot/attribute layout"
        );

        assert!(
            define_own_property(
                prototype,
                interp.gc_heap_mut(),
                "charCodeAt",
                PropertyDescriptor::data(Value::number_i32(2), false, false, true),
            )
            .expect("descriptor fixture allocation")
        );
        assert_ne!(
            shape_id(prototype, interp.gc_heap()),
            original,
            "an in-place attribute change must assign a fresh structural id"
        );
    }

    #[test]
    fn shape_id_prefers_installed_shape() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let mut o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");

        interp
            .create_data_property(&mut o, "x", Value::boolean(true))
            .expect("set x");

        let shape_handle = shape(o, interp.gc_heap());
        let installed_shape_id = interp
            .gc_heap()
            .read_payload(shape_handle, shape_body::ShapeBody::id);
        assert_eq!(shape_id(o, interp.gc_heap()), installed_shape_id);
    }

    #[test]
    fn own_property_reads_prefer_installed_shape_offsets() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let mut o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");

        interp
            .create_data_property(&mut o, "x", Value::boolean(true))
            .expect("set x");
        interp.gc_heap_mut().with_payload(o, |body| {
            dict_clear_keys(body);
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

        assert!(
            ordinary_set_data_property(&mut o, interp.gc_heap_mut(), "x", Value::boolean(false))
                .expect("fixture assignment allocation")
        );

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
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
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
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let mut o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");

        interp
            .create_data_property(&mut o, "a", Value::boolean(true))
            .expect("set a");
        interp
            .create_data_property(&mut o, "b", Value::null())
            .expect("set b");

        let before = shape(o, interp.gc_heap());
        assert!(!before.is_null());
        assert_eq!(interp.shape_offset_of(before, "b"), Some(1));

        assert!(delete(&mut o, interp.gc_heap_mut(), "a").expect("delete fixture"));

        assert!(is_dictionary(o, interp.gc_heap()));
        assert!(get(o, interp.gc_heap(), "a").is_none());
        assert!(get(o, interp.gc_heap(), "b").is_some_and(|v| v.is_null()));
    }

    #[test]
    fn runtime_store_transition_invalidates_shape() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
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

        assert!(is_dictionary(first, interp.gc_heap()));
        assert_eq!(
            get_own(first, interp.gc_heap(), "x"),
            Some(Value::boolean(true))
        );

        let second = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("second object");
        assert_eq!(
            shape(second, interp.gc_heap()),
            interp.null_prototype_root()
        );

        assert_eq!(
            replay_store_property_transition(
                second,
                interp.gc_heap_mut(),
                key,
                transition.from_shape_id,
                transition.atom_id,
                transition.to_shape_id,
                || transition.to_shape.get(),
                &transition.kind,
                transition.slot,
                &Value::null(),
            )
            .expect("transition replay allocation"),
            Some(())
        );

        assert!(is_dictionary(second, interp.gc_heap()));
        assert_eq!(get_own(second, interp.gc_heap(), "x"), Some(Value::null()));
    }

    #[test]
    fn raw_set_invalidates_shape_for_new_property() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let mut o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");
        assert_eq!(shape(o, interp.gc_heap()), interp.null_prototype_root());

        assert!(
            ordinary_set_data_property(&mut o, interp.gc_heap_mut(), "x", Value::boolean(true))
                .expect("fixture assignment allocation")
        );

        assert!(is_dictionary(o, interp.gc_heap()));
        assert_eq!(
            get_own(o, interp.gc_heap(), "x"),
            Some(Value::boolean(true))
        );
    }

    #[test]
    fn raw_ordinary_set_invalidates_shape_for_new_property() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let mut o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");
        assert_eq!(shape(o, interp.gc_heap()), interp.null_prototype_root());

        assert!(
            ordinary_set_data_property(&mut o, interp.gc_heap_mut(), "x", Value::boolean(true))
                .expect("fixture assignment allocation")
        );

        assert!(is_dictionary(o, interp.gc_heap()));
        assert_eq!(
            get_own(o, interp.gc_heap(), "x"),
            Some(Value::boolean(true))
        );
    }

    #[test]
    fn raw_define_property_invalidates_shape_for_new_property() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");
        assert_eq!(shape(o, interp.gc_heap()), interp.null_prototype_root());

        assert!(
            define_own_property(
                o,
                interp.gc_heap_mut(),
                "x",
                PropertyDescriptor::data(Value::boolean(true), true, true, true),
            )
            .expect("descriptor fixture allocation")
        );

        assert!(is_dictionary(o, interp.gc_heap()));
        assert_eq!(
            get_own(o, interp.gc_heap(), "x"),
            Some(Value::boolean(true))
        );
    }

    #[test]
    fn raw_define_property_partial_invalidates_shape_for_new_property() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let mut o = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");
        assert_eq!(shape(o, interp.gc_heap()), interp.null_prototype_root());
        let descriptor = PartialPropertyDescriptor {
            value: Some(Value::boolean(true)),
            writable: Some(true),
            enumerable: Some(true),
            configurable: Some(true),
            ..PartialPropertyDescriptor::default()
        };

        assert!(
            define_own_property_partial(&mut o, interp.gc_heap_mut(), "x", descriptor,)
                .expect("descriptor fixture allocation")
        );

        assert!(is_dictionary(o, interp.gc_heap()));
        assert_eq!(
            get_own(o, interp.gc_heap(), "x"),
            Some(Value::boolean(true))
        );
    }

    #[test]
    fn set_then_get_roundtrip() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        assert!(
            ordinary_set_data_property(&mut o, &mut heap, "x", Value::boolean(true))
                .expect("fixture assignment allocation")
        );
        assert!(get(o, &heap, "x").is_some_and(|v| v.as_boolean() == Some(true)));
    }

    #[test]
    fn atom_lookup_reports_shape_and_slot_metadata() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "x",
                PropertyDescriptor::data(Value::boolean(true), true, true, true)
            )
            .expect("fixture property allocation")
        );
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
                // Heap-only descriptor construction normalizes the object to
                // dictionary mode (no keyed shape handle).
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
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "x",
                PropertyDescriptor::data(Value::boolean(true), true, true, true)
            )
            .expect("fixture property allocation")
        );
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_global(7)),
            "x",
        );
        let hit = lookup_own_atom(o, &heap, key).hit.expect("atom hit");
        assert_eq!(
            load_own_data_slot_atom(o, &heap, key, hit),
            Some(Value::boolean(true))
        );

        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "y",
                PropertyDescriptor::data(Value::null(), true, true, true)
            )
            .expect("fixture property allocation")
        );

        assert_eq!(load_own_data_slot_atom(o, &heap, key, hit), None);
    }

    #[test]
    fn atom_slot_store_updates_guarded_data_slot() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "x",
                PropertyDescriptor::data(Value::boolean(true), true, true, true)
            )
            .expect("fixture property allocation")
        );
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

        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "y",
                PropertyDescriptor::data(Value::null(), true, true, true)
            )
            .expect("fixture property allocation")
        );

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

    /// Install one ordinary shape slot through the real transition owner.
    /// Build the actual immutable append; the dictionary-only test helper
    /// intentionally cannot supply this ordinary IC premise.
    fn shaped_data_fixture(
        object: JsObject,
        heap: &mut otter_gc::GcHeap,
        name: &str,
        value: Value,
    ) {
        let atom = match name {
            "own" => 6,
            "x" => 7,
            "y" => 8,
            "z" => 9,
            _ => panic!("fixture atom name"),
        };
        append_shaped_data_for_fixture(
            object,
            heap,
            AtomizedPropertyKey::new(
                crate::property_atom::PropertyAtom::new(AtomId::from_global(atom)),
                name,
            ),
            value,
        );
    }

    #[test]
    fn raw_atom_add_transition_rejects_unshared_dictionary_shape() {
        let mut heap = fresh_heap();
        let proto = alloc_object_old_for_fixture(&mut heap).unwrap();
        let mut first = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(&mut first, &mut heap, Some(proto)).expect("set_prototype fixture");
        // Install the proof on an ordinary receiver; the replay target below
        // genuinely enters its own unshared dictionary layout.
        shaped_data_fixture(first, &mut heap, "own", Value::null());
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_global(7)),
            "x",
        );
        let transition =
            capture_store_property_transition(first, &mut heap, key, &Value::boolean(true))
                .expect("transition install");
        assert!(matches!(
            transition.kind,
            StorePropertyTransitionKind::PrototypeChainMissing { .. }
        ));

        let mut second = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(&mut second, &mut heap, Some(proto)).expect("set_prototype fixture");
        assert!(
            define_own_property_in_place(
                &mut second,
                &mut heap,
                "own",
                PropertyDescriptor::data(Value::null(), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(is_dictionary(second, &heap));
        assert_eq!(get_own(second, &heap, "own"), Some(Value::null()));
        assert!(
            capture_store_property_transition(second, &mut heap, key, &Value::null()).is_none(),
            "an unshared dictionary cannot supply a new ordinary transition"
        );

        assert_eq!(
            replay_store_property_transition(
                second,
                &mut heap,
                key,
                transition.from_shape_id,
                transition.atom_id,
                transition.to_shape_id,
                || transition.to_shape.get(),
                &transition.kind,
                transition.slot,
                &Value::boolean(false),
            )
            .expect("transition replay allocation"),
            None
        );
        assert_eq!(get_own(second, &heap, "x"), None);
    }

    #[test]
    fn atom_add_transition_rejects_changed_direct_prototype_shape() {
        let mut heap = fresh_heap();
        let mut proto = alloc_object_old_for_fixture(&mut heap).unwrap();
        let mut first = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(&mut first, &mut heap, Some(proto)).expect("set_prototype fixture");
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_global(7)),
            "x",
        );
        let transition =
            capture_store_property_transition(first, &mut heap, key, &Value::boolean(true))
                .expect("transition install");
        assert!(
            define_own_property_in_place(
                &mut proto,
                &mut heap,
                "x",
                PropertyDescriptor::data(Value::null(), true, true, true)
            )
            .expect("fixture property allocation")
        );

        let mut second = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(&mut second, &mut heap, Some(proto)).expect("set_prototype fixture");

        let allocations_before_miss = total_allocations(&mut heap);
        assert_eq!(
            replay_store_property_transition(
                second,
                &mut heap,
                key,
                transition.from_shape_id,
                transition.atom_id,
                transition.to_shape_id,
                || transition.to_shape.get(),
                &transition.kind,
                transition.slot,
                &Value::number_f64(1.25),
            )
            .expect("transition replay allocation"),
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
        let mut proto = alloc_object_old_for_fixture(&mut heap).unwrap();
        let mut first = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(&mut first, &mut heap, Some(proto)).expect("set_prototype fixture");
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_global(7)),
            "x",
        );
        let transition =
            capture_store_property_transition(first, &mut heap, key, &Value::boolean(true))
                .expect("transition install");
        let deep_proto = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(&mut proto, &mut heap, Some(deep_proto)).expect("set_prototype fixture");

        let mut second = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(&mut second, &mut heap, Some(proto)).expect("set_prototype fixture");

        assert_eq!(
            replay_store_property_transition(
                second,
                &mut heap,
                key,
                transition.from_shape_id,
                transition.atom_id,
                transition.to_shape_id,
                || transition.to_shape.get(),
                &transition.kind,
                transition.slot,
                &Value::boolean(false),
            )
            .expect("transition replay allocation"),
            None
        );
    }

    #[test]
    fn raw_atom_add_transition_rejects_unshared_inherited_dictionary_shape() {
        let mut heap = fresh_heap();
        let mut proto = alloc_object_old_for_fixture(&mut heap).unwrap();
        shaped_data_fixture(proto, &mut heap, "x", Value::boolean(true));
        let mut first = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(&mut first, &mut heap, Some(proto)).expect("set_prototype fixture");
        // Install the proof on an ordinary receiver; the replay target below
        // genuinely enters its own unshared dictionary layout.
        shaped_data_fixture(first, &mut heap, "own", Value::null());
        let source = shape(first, &heap);
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_global(7)),
            "x",
        );
        let transition =
            capture_store_property_transition(first, &mut heap, key, &Value::boolean(false))
                .expect("transition install");
        assert!(matches!(
            transition.kind,
            StorePropertyTransitionKind::PrototypeWritableData { .. }
        ));

        let mut second = alloc_object_old(&mut heap, source).expect("same actual ordinary shape");
        assert!(
            define_own_property_in_place(
                &mut second,
                &mut heap,
                "own",
                PropertyDescriptor::data(Value::null(), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert_eq!(
            shape_id(second, &heap),
            transition.from_shape_id,
            "the receiver still satisfies the original ordinary shape proof"
        );
        assert!(
            define_own_property_in_place(
                &mut proto,
                &mut heap,
                "dictionary-marker",
                PropertyDescriptor::data(Value::null(), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(is_dictionary(proto, &heap));
        assert!(!is_dictionary(second, &heap));
        assert!(
            capture_store_property_transition(second, &mut heap, key, &Value::null()).is_none(),
            "an inherited dictionary cannot supply a new ordinary transition"
        );

        assert_eq!(
            replay_store_property_transition(
                second,
                &mut heap,
                key,
                transition.from_shape_id,
                transition.atom_id,
                transition.to_shape_id,
                || transition.to_shape.get(),
                &transition.kind,
                transition.slot,
                &Value::null(),
            )
            .expect("transition replay allocation"),
            None
        );
        assert_eq!(get_own(second, &heap, "x"), None);
        assert_eq!(get_own(proto, &heap, "x"), Some(Value::boolean(true)));
    }

    #[test]
    fn atom_add_transition_rejects_inherited_data_after_writable_change() {
        let mut heap = fresh_heap();
        let proto = alloc_object_old_for_fixture(&mut heap).unwrap();
        shaped_data_fixture(proto, &mut heap, "x", Value::boolean(true));
        let mut first = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(&mut first, &mut heap, Some(proto)).expect("set_prototype fixture");
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_global(7)),
            "x",
        );
        let transition =
            capture_store_property_transition(first, &mut heap, key, &Value::boolean(false))
                .expect("transition install");
        assert!(
            define_own_property(
                proto,
                &mut heap,
                "x",
                PropertyDescriptor::data(Value::boolean(true), false, true, true),
            )
            .expect("descriptor fixture allocation")
        );

        let mut second = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(&mut second, &mut heap, Some(proto)).expect("set_prototype fixture");

        assert_eq!(
            replay_store_property_transition(
                second,
                &mut heap,
                key,
                transition.from_shape_id,
                transition.atom_id,
                transition.to_shape_id,
                || transition.to_shape.get(),
                &transition.kind,
                transition.slot,
                &Value::null(),
            )
            .expect("transition replay allocation"),
            None
        );
    }

    #[test]
    fn atom_add_transition_rejects_inherited_non_writable_data() {
        let mut heap = fresh_heap();
        let proto = alloc_object_old_for_fixture(&mut heap).unwrap();
        assert!(
            define_own_property(
                proto,
                &mut heap,
                "x",
                PropertyDescriptor::data(Value::boolean(true), false, true, true),
            )
            .expect("descriptor fixture allocation")
        );
        let mut receiver = alloc_object_old_for_fixture(&mut heap).unwrap();
        set_prototype(&mut receiver, &mut heap, Some(proto)).expect("set_prototype fixture");
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
        assert!(
            ordinary_set_data_property(&mut o, &mut heap, "x", Value::boolean(true))
                .expect("fixture assignment allocation")
        );
        let with_x = shape_id(o, &heap);
        assert!(
            ordinary_set_data_property(&mut o, &mut heap, "x", Value::boolean(false))
                .expect("fixture assignment allocation")
        );

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
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "a",
                PropertyDescriptor::data(Value::boolean(true), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "b",
                PropertyDescriptor::data(Value::boolean(false), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "c",
                PropertyDescriptor::data(Value::null(), true, true, true)
            )
            .expect("fixture property allocation")
        );
        let keys: Vec<String> =
            with_properties(o, &heap, |p| p.keys().map(str::to_string).collect());
        assert_eq!(keys, vec!["a", "b", "c"]);
    }

    #[test]
    fn integer_index_keys_sort_before_strings() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "b",
                PropertyDescriptor::data(Value::boolean(true), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "10",
                PropertyDescriptor::data(Value::boolean(true), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "2",
                PropertyDescriptor::data(Value::boolean(true), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "a",
                PropertyDescriptor::data(Value::boolean(true), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "1",
                PropertyDescriptor::data(Value::boolean(true), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "01",
                PropertyDescriptor::data(Value::boolean(true), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "4294967295",
                PropertyDescriptor::data(Value::boolean(true), true, true, true)
            )
            .expect("fixture property allocation")
        );

        let keys: Vec<String> =
            with_properties(o, &heap, |p| p.keys().map(str::to_string).collect());
        assert_eq!(keys, vec!["1", "2", "10", "b", "a", "01", "4294967295"]);
    }

    #[test]
    fn delete_removes_property() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "x",
                PropertyDescriptor::data(Value::boolean(true), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(delete(&mut o, &mut heap, "x").expect("delete fixture"));
        assert!(get(o, &heap, "x").is_none());
        // §10.1.10 — deleting a missing property still reports
        // success (returns true).
        assert!(delete(&mut o, &mut heap, "x").expect("delete fixture"));
    }

    #[test]
    fn handle_copy_shares_storage() {
        let mut heap = fresh_heap();
        let mut a = alloc_object_old_for_fixture(&mut heap).unwrap();
        let b = a; // Copy
        assert!(
            define_own_property_in_place(
                &mut a,
                &mut heap,
                "x",
                PropertyDescriptor::data(Value::boolean(true), true, true, true)
            )
            .expect("fixture property allocation")
        );
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
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "x",
                PropertyDescriptor::data(Value::boolean(true), true, true, true)
            )
            .expect("fixture property allocation")
        );
        let s1 = shape_id(o, &heap);
        assert!(
            ordinary_set_data_property(&mut o, &mut heap, "x", Value::null())
                .expect("fixture assignment allocation")
        );
        let s2 = shape_id(o, &heap);
        assert_eq!(s1, s2);
        assert_eq!(len(o, &heap), 1);
    }

    #[test]
    fn middle_delete_normalizes_and_migration_restores_ordinary_eligibility() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let mut o = alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();
        interp
            .create_data_property(&mut o, "a", Value::boolean(true))
            .unwrap();
        interp
            .create_data_property(&mut o, "b", Value::null())
            .unwrap();
        let before = shape_id(o, interp.gc_heap());
        assert!(supports_fast_property_ic(o, interp.gc_heap()));
        assert!(delete(&mut o, interp.gc_heap_mut(), "a").expect("middle delete"));
        assert_ne!(shape_id(o, interp.gc_heap()), before);
        assert!(!supports_fast_property_ic(o, interp.gc_heap()));
        assert_eq!(len(o, interp.gc_heap()), 1);
        assert!(get(o, interp.gc_heap(), "a").is_none());
        assert!(get(o, interp.gc_heap(), "b").is_some_and(|v| v.is_null()));
        interp.migrate_slow_to_fast(&mut o);
        assert!(
            supports_fast_property_ic(o, interp.gc_heap()),
            "delete leaves no permanent eligibility latch"
        );
        assert_eq!(get(o, interp.gc_heap(), "b"), Some(Value::null()));
    }

    #[test]
    fn delete_middle_preserves_later_dictionary_offsets() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "locale",
                PropertyDescriptor::data(Value::number_i32(1), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "style",
                PropertyDescriptor::data(Value::number_i32(2), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "type",
                PropertyDescriptor::data(Value::number_i32(3), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "fallback",
                PropertyDescriptor::data(Value::number_i32(4), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "languageDisplay",
                PropertyDescriptor::data(Value::number_i32(5), true, true, true)
            )
            .expect("fixture property allocation")
        );

        assert!(delete(&mut o, &mut heap, "style").expect("delete fixture"));

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
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "locale",
                PropertyDescriptor::data(Value::number_i32(1), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "style",
                PropertyDescriptor::data(Value::number_i32(2), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "type",
                PropertyDescriptor::data(Value::number_i32(3), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "fallback",
                PropertyDescriptor::data(Value::number_i32(4), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "languageDisplay",
                PropertyDescriptor::data(Value::number_i32(5), true, true, true)
            )
            .expect("fixture property allocation")
        );

        assert!(
            ordinary_set_data_property(&mut o, &mut heap, "style", Value::number_i32(20))
                .expect("fixture assignment allocation")
        );
        assert!(
            ordinary_set_data_property(&mut o, &mut heap, "style", Value::number_i32(2))
                .expect("fixture assignment allocation")
        );
        assert!(delete(&mut o, &mut heap, "style").expect("delete fixture"));

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
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "locale",
                PropertyDescriptor::data(Value::number_i32(1), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "style",
                PropertyDescriptor::data(Value::number_i32(2), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "type",
                PropertyDescriptor::data(Value::number_i32(3), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "fallback",
                PropertyDescriptor::data(Value::number_i32(4), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "languageDisplay",
                PropertyDescriptor::data(Value::number_i32(5), true, true, true)
            )
            .expect("fixture property allocation")
        );

        assert!(delete(&mut o, &mut heap, "style").expect("delete fixture"));
        assert_eq!(
            get(o, &heap, "type").and_then(|v| v.as_number()),
            Some(NumberValue::from_i32(3))
        );
        assert_eq!(
            get(o, &heap, "fallback").and_then(|v| v.as_number()),
            Some(NumberValue::from_i32(4))
        );
        assert!(delete(&mut o, &mut heap, "type").expect("delete fixture"));

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
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "locale",
                PropertyDescriptor::data(Value::number_i32(1), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "style",
                PropertyDescriptor::data(Value::number_i32(2), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "type",
                PropertyDescriptor::data(Value::number_i32(3), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "fallback",
                PropertyDescriptor::data(Value::number_i32(4), true, true, true)
            )
            .expect("fixture property allocation")
        );

        assert!(delete(&mut o, &mut heap, "style").expect("delete fixture"));
        assert!(delete(&mut o, &mut heap, "type").expect("delete fixture"));

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
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "locale",
                PropertyDescriptor::data(Value::number_i32(1), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "style",
                PropertyDescriptor::data(Value::number_i32(2), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "type",
                PropertyDescriptor::data(Value::number_i32(3), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "fallback",
                PropertyDescriptor::data(Value::number_i32(4), true, true, true)
            )
            .expect("fixture property allocation")
        );

        assert!(delete(&mut o, &mut heap, "style").expect("delete fixture"));
        assert!(
            ordinary_set_data_property(&mut o, &mut heap, "type", Value::number_i32(30))
                .expect("fixture assignment allocation")
        );
        assert!(
            ordinary_set_data_property(&mut o, &mut heap, "type", Value::number_i32(3))
                .expect("fixture assignment allocation")
        );
        assert_eq!(
            get(o, &heap, "fallback").and_then(|v| v.as_number()),
            Some(NumberValue::from_i32(4))
        );
        assert!(delete(&mut o, &mut heap, "type").expect("delete fixture"));

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
        assert!(
            define_own_property(o, &mut heap, "x", desc).expect("descriptor fixture allocation")
        );
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
        )
        .expect("descriptor fixture allocation");
        // Try to switch the data slot to an accessor — must fail.
        let accessor = PropertyDescriptor::accessor(None, None, true, false);
        assert!(
            !define_own_property(o, &mut heap, "x", accessor)
                .expect("descriptor fixture allocation")
        );
    }

    #[test]
    fn ordinary_set_data_property_preserves_existing_attrs() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        assert!(
            define_own_property(
                o,
                &mut heap,
                "x",
                PropertyDescriptor::data(Value::boolean(false), true, false, false),
            )
            .expect("descriptor fixture allocation")
        );

        assert!(
            ordinary_set_data_property(&mut o, &mut heap, "x", Value::boolean(true))
                .expect("fixture assignment allocation")
        );

        let got = get_own_descriptor(o, &heap, "x").unwrap();
        assert!(get(o, &heap, "x").is_some_and(|v| v.as_boolean() == Some(true)));
        assert!(got.writable());
        assert!(!got.enumerable());
        assert!(!got.configurable());
    }

    #[test]
    fn ordinary_set_data_property_rejects_non_writable_data() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        assert!(
            define_own_property(
                o,
                &mut heap,
                "x",
                PropertyDescriptor::data(Value::boolean(false), false, true, true),
            )
            .expect("descriptor fixture allocation")
        );

        assert!(
            !ordinary_set_data_property(&mut o, &mut heap, "x", Value::boolean(true))
                .expect("fixture assignment allocation")
        );

        assert!(get(o, &heap, "x").is_some_and(|v| v.as_boolean() == Some(false)));
    }

    #[test]
    fn ordinary_set_data_property_respects_extensibility_for_new_keys() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();

        assert!(
            ordinary_set_data_property(&mut o, &mut heap, "x", Value::null())
                .expect("fixture assignment allocation")
        );
        assert!(get(o, &heap, "x").is_some_and(|v| v.is_null()));

        prevent_extensions(&mut o, &mut heap).expect("prevent_extensions fixture");
        assert!(
            !ordinary_set_data_property(&mut o, &mut heap, "y", Value::boolean(true))
                .expect("fixture assignment allocation")
        );
        assert!(get(o, &heap, "y").is_none());
    }

    #[test]
    fn freeze_makes_object_non_writable() {
        let mut heap = fresh_heap();
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "x",
                PropertyDescriptor::data(Value::boolean(true), true, true, true)
            )
            .expect("fixture property allocation")
        );
        freeze(&mut o, &mut heap).expect("freeze fixture");
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
        assert!(
            define_own_property_in_place(
                &mut o,
                &mut heap,
                "a",
                PropertyDescriptor::data(Value::null(), true, true, true)
            )
            .expect("fixture property allocation")
        );
        seal(&mut o, &mut heap).expect("seal fixture");
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
        let mut o = alloc_object_old_for_fixture(&mut heap).unwrap();
        define_own_property(
            o,
            &mut heap,
            "x",
            PropertyDescriptor::data(Value::boolean(true), true, true, false),
        )
        .expect("descriptor fixture allocation");
        assert!(!delete(&mut o, &mut heap, "x").expect("delete fixture"));
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
        )
        .expect("descriptor fixture allocation");
        assert!(
            delete_if_same_data(&mut cache, &mut heap, "module", record_value)
                .expect("delete_if_same_data fixture")
        );
        assert!(get(cache, &heap, "module").is_none());

        assert!(
            ordinary_set_data_property(&mut cache, &mut heap, "module", replacement_value)
                .expect("fixture assignment allocation")
        );
        assert!(
            !delete_if_same_data(&mut cache, &mut heap, "module", record_value)
                .expect("delete_if_same_data fixture")
        );
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

/// Resolve one logical slot against the exact immutable shape layout.
pub(crate) fn field_location(shape: ShapeHandle, slot: u32) -> FieldLocation {
    FieldLocation::for_slot(slot, shape_body::inline_capacity_of(shape))
}

/// Resolve a dictionary or shaped object's immutable prefix layout.
pub(crate) fn field_location_at(object: JsObject, heap: &GcHeap, slot: u32) -> FieldLocation {
    field_location(self::shape(object, heap), slot)
}

#[cfg(test)]
#[path = "object/error_stack_source_tests.rs"]
mod error_stack_source_tests;
