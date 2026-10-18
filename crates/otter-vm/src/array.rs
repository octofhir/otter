//! JavaScript array value with dense and sparse indexed storage.
//!
//! Arrays live on the page-based tracing GC. The public handle is a compressed
//! [`otter_gc::Gc<ArrayBody>`]; every read or mutation takes an
//! explicit [`otter_gc::GcHeap`] reference so no thread-local heap
//! lookup can hide a safepoint.
//!
//! # Contents
//! - Dense and sparse indexed storage with array-exotic descriptor state.
//! - GC-root-aware growth, mutation, bulk-fill, and structural helpers.
//! - JIT-visible dense base/length caches plus allocation-free plain-slot
//!   completion used after guarded native element misses.
//! - Cached eligibility for present own dense slots with source-realm prototypes.
//! - Allocation-free ordinary tail appends with one length/kind publication.
//!
//! # Invariants
//!
//! - Low, contiguous indices live in one GC-owned element slab.
//! - Large sparse indices live in `sparse_elements` so Array-index
//!   semantics do not force host-sized dense allocations.
//! - Missing-index reads return `undefined`.
//! - Element growth allocates a rooted replacement slab, copies the live
//!   prefix semantically, then republishes its base, length, and kind.
//! - Only indexed mutation invalidates captured JSON element bytes; named bags
//!   cannot alias an index, and the semantic JSON owner resolves toJSON first.
//! - Non-index string and symbol properties live in one managed ordinary
//!   null-prototype expando object; it stores properties, never the receiver.
//! - Rare indexed descriptors, sparse elements, captured JSON text and explicit
//!   prototype overrides live behind a separate sidecar.
//! - The rare and expando compressed handles form one aligned 8-byte pair;
//!   a whole prototype lookup proves both absent with one zero-word read.
//! - Plain-slot completion never grows storage; it may only replace an existing
//!   dense slot and always records the generational write barrier.
//! - Native own-element hits admit a prototype-only sidecar after proving the
//!   slot present; descriptor/accessor/sparse baggage invalidates eligibility.
//!
//! # See also
//!
//! - <https://tc39.es/ecma262/#sec-array-exotic-objects>
//! - [GC API](../../../docs/book/src/engine/gc-api.md)

mod dense_guard;
mod dense_push;
pub(crate) mod elements;
mod expando;
mod indexed_snapshot;

pub(crate) use dense_guard::element_family as dense_element_family;
pub(crate) use dense_push::try_push_spare;
pub(crate) use elements::DenseElementKind;
pub(crate) use expando::{ArrayPropertyKey, classify_property_key, expando_object, own_indices};
pub(crate) use indexed_snapshot::ArrayIndexedSnapshot;

use std::collections::HashMap;
use std::sync::Arc;

use smallvec::SmallVec;
use std::cell::Cell;

use crate::Value;
use crate::number::NumberValue;
use crate::object::PropertyFlags;
use otter_gc::GcHeap;
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::{RawGc, SlotVisitor};

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`ArrayBody`].
///
/// Distinct from global-lexical cells (`0x10`) and objects (`0x11`).
pub const ARRAY_BODY_TYPE_TAG: u8 = 0x12;

/// Heap-shared array handle.
pub type JsArray = otter_gc::Gc<ArrayBody>;

/// GC-allocated storage backing every [`JsArray`] handle.
///
/// Hand-written trace (no Pelt derive): the cached `elements_ptr`
/// must be recomputed from the slab handle after every visit, so a
/// relocation — a snapshot restore in particular — leaves compiled
/// fast paths reading the slab's current address rather than the
/// captured one.
/// The C field order places rare and expando handles in one aligned word;
/// whole named lookup reads it only as a null predicate, never as a GC handle.
#[derive(Debug)]
#[repr(C)]
pub struct ArrayBody {
    /// Logical `length` property. This may be larger than dense
    /// storage when `length` is assigned directly or when sparse
    /// elements are written.
    pub(crate) length: usize,
    /// Always-current base of the dense element buffer, kept in a fixed body
    /// field so compiled code can address elements without knowing where the
    /// slab lives. Growth and every traced relocation refresh this address;
    /// compiled code must reload it after an operation that may collect.
    elements_ptr: Cell<*mut u8>,
    /// Lazily-allocated rare/exotic array state. Null for ordinary dense
    /// arrays with default extensibility and no sparse/indexed-accessor
    /// state, so the GC cell only carries the JIT-visible dense storage and
    /// logical length in the common case. The sidecar is its own GC body;
    /// every mutating path reserves it via [`ensure_exotic`] outside the
    /// payload borrow, because creating it allocates.
    pub(crate) exotic: ArrayExoticHandle,
    /// Ordinary null-prototype own storage for non-index string and symbol
    /// properties. Null until the first such property is created. The original
    /// array remains the receiver for every property operation.
    pub(crate) expando: crate::object::JsObject,
    /// Dense element storage in its own GC cell. Null while the array has no
    /// dense storage. Numeric slabs hold raw doubles plus a hole bitmap;
    /// tagged slabs hold ordinary `Value` words.
    ///
    /// Crate-internal callers must go through this module's helpers so
    /// growth allocates. Native code reads the refreshed cache fields and
    /// runtime allocation plans derive this handle's offset from its owner.
    pub(crate) slab: elements::ElementSlabHandle,
    /// Live dense length. Authoritative, with a fixed body offset so a
    /// compiled bounds check reads it as one 32-bit load. Dense storage
    /// beyond `u32::MAX` elements is unreachable in practice — growth helpers
    /// cap dense storage far below it.
    dense_len: Cell<u32>,
    /// Values the current slab can hold. Cached beside the base for the same
    /// reason the length is: a push under a payload borrow has no heap to ask.
    dense_cap: Cell<u32>,
    /// JIT-visible [`DenseElementKind`] discriminant for the current slab.
    /// The slab header is authoritative; this byte is refreshed with the base
    /// and capacity after every adoption or restore.
    dense_kind: Cell<u8>,
    /// Zero while a proved present dense slot has its default own data
    /// semantics. A prototype-only sidecar is legal; other exotic state
    /// blocks native own-slot access. Refreshed by the sidecar mutation owner.
    dense_own_guard: Cell<u8>,
}

impl otter_gc::SafeTraceable for ArrayBody {
    const TYPE_TAG: u8 = ARRAY_BODY_TYPE_TAG;

    /// Visit the slab, sidecar and expando handles separately, then republish
    /// the element base. The refresh must follow the visits: a restore relocates
    /// the slab handle here, and the cached absolute pointer has to be
    /// recomputed from the relocated value.
    fn trace_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>) {
        if !self.slab.is_null() {
            let p = &mut self.slab as *mut elements::ElementSlabHandle as *mut otter_gc::raw::RawGc;
            visitor(p);
        }
        if !self.exotic.is_null() {
            let p = &mut self.exotic as *mut ArrayExoticHandle as *mut otter_gc::raw::RawGc;
            visitor(p);
        }
        if !self.expando.is_null() {
            visitor(std::ptr::from_mut(&mut self.expando).cast());
        }
        self.refresh_element_cache();
    }
}

impl Default for ArrayBody {
    fn default() -> Self {
        Self {
            slab: elements::ElementSlabHandle::null(),
            length: 0,
            exotic: ArrayExoticHandle::null(),
            expando: crate::object::JsObject::null(),
            elements_ptr: Cell::new(std::ptr::null_mut()),
            dense_len: Cell::new(0),
            dense_cap: Cell::new(0),
            dense_kind: Cell::new(DenseElementKind::Empty as u8),
            dense_own_guard: Cell::new(0),
        }
    }
}

impl ArrayBody {
    /// Recompute the base and capacity the slab currently provides.
    ///
    /// The live length is not derived here — it is this body's own state,
    /// and adopting a larger slab must not change it.
    #[inline]
    pub(crate) fn refresh_element_cache(&self) {
        self.elements_ptr.set(elements::data_base(self.slab));
        self.dense_cap
            .set(u32::try_from(elements::capacity_of(self.slab)).unwrap_or(u32::MAX));
        self.dense_kind.set(elements::kind_of(self.slab) as u8);
        self.refresh_dense_own_guard();
    }

    /// Debug verifier for the always-current element cache: the cached
    /// triple must agree with the slab the body holds, so any new mutation
    /// path that forgets to refresh fails deterministically.
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn element_cache_is_current(&self) -> bool {
        let slab_len = elements::len_of(self.slab);
        self.elements_ptr.get() == elements::data_base(self.slab)
            && self.dense_cap.get() as usize == elements::capacity_of(self.slab)
            && self.dense_len.get() as usize == slab_len
            && self.dense_kind() == elements::kind_of(self.slab)
            && self.dense_own_guard.get() == self.current_dense_own_guard()
    }

    /// Read one raw dense slot, including the internal hole sentinel.
    #[inline]
    pub(crate) fn dense_value(&self, index: usize) -> Option<Value> {
        let body = elements::body_of(self.slab)?;
        // SAFETY: the array body keeps the slab reachable for this borrow.
        unsafe { (*body).get(index) }
    }

    /// Materialize the live dense prefix as tagged values.
    #[inline]
    pub(crate) fn dense_values(&self) -> Vec<Value> {
        let Some(body) = elements::body_of(self.slab) else {
            return Vec::new();
        };
        // SAFETY: the array body keeps the slab reachable for this borrow.
        unsafe { (*body).values_vec() }
    }

    #[inline]
    fn slab_body_mut(&mut self) -> Option<&mut elements::ElementSlabBody> {
        let body = elements::body_of(self.slab)?;
        // SAFETY: `&mut self` excludes another mutation through this owner.
        Some(unsafe { &mut *body })
    }

    #[inline]
    fn write_dense_value(&mut self, index: usize, value: Value) {
        self.slab_body_mut()
            .expect("dense write without a slab")
            .set(index, value);
        self.refresh_element_cache();
    }

    #[inline]
    fn truncate_dense(&mut self, len: usize) {
        if let Some(slab) = self.slab_body_mut() {
            slab.truncate(len);
        } else {
            debug_assert_eq!(len, 0, "an array with no slab has no dense elements");
        }
        self.dense_len
            .set(u32::try_from(len).expect("dense length exceeds u32"));
        self.refresh_element_cache();
    }

    #[inline]
    pub(crate) fn dense_kind(&self) -> DenseElementKind {
        DenseElementKind::from_raw(self.dense_kind.get()).expect("invalid dense element kind")
    }

    /// Number of live dense elements.
    #[inline]
    pub(crate) fn dense_len(&self) -> usize {
        self.dense_len.get() as usize
    }

    /// Values the current slab can hold without growing.
    #[inline]
    pub(crate) fn dense_capacity(&self) -> usize {
        self.dense_cap.get() as usize
    }

    /// Adopt `slab` as this array's dense storage, with its first `len`
    /// values already written.
    #[inline]
    pub(crate) fn adopt_slab(&mut self, slab: elements::ElementSlabHandle, len: usize) {
        self.slab = slab;
        self.refresh_element_cache();
        self.set_dense_len(len);
    }

    /// Publish a new live length to both the slab and the cache.
    ///
    /// The slab is the authority — it is what the collector consults —
    /// and the body's copy is what a compiled bounds check reads, so the
    /// two must never be written apart.
    #[inline]
    pub(crate) fn set_dense_len(&self, len: usize) {
        if let Some(body) = elements::body_of(self.slab) {
            // SAFETY: the handle names a live slab, and no other borrow
            // of its payload is open while this body holds it.
            unsafe { (*body).set_len(len) };
        } else {
            debug_assert_eq!(len, 0, "an array with no slab has no dense elements");
        }
        self.dense_len
            .set(u32::try_from(len).expect("dense length exceeds u32"));
        self.dense_kind.set(elements::kind_of(self.slab) as u8);
    }
}

/// Truncate dense elements, clearing the values that leave the live
/// prefix so the slab's tracer stops keeping them alive.
#[inline]
fn body_elements_truncate(body: &mut ArrayBody, len: usize) {
    let current = body.dense_len();
    if len >= current {
        return;
    }
    body.truncate_dense(len);
}

/// Insert one dense element at `index`, shifting the tail up. The slab
/// must already have room.
#[inline]
fn body_elements_insert(body: &mut ArrayBody, index: usize, value: Value) {
    let len = body.dense_len();
    debug_assert!(index <= len && len < body.dense_capacity());
    if index > len || len >= body.dense_capacity() {
        return;
    }
    // A non-number makes tagged storage terminal. Widen before shifting so
    // the newly initialized tail slot participates in tagged representation;
    // converting after the shift would only visit the old live prefix.
    if body.dense_kind().is_numeric() && !value.is_number() && !value.is_hole() {
        body.slab_body_mut()
            .expect("dense insert without a slab")
            .convert_to_tagged();
        body.refresh_element_cache();
    }
    for source in (index..len).rev() {
        let moved = body.dense_value(source).expect("live dense slot");
        body.write_dense_value(source + 1, moved);
    }
    body.write_dense_value(index, value);
    body.set_dense_len(len + 1);
}

/// Remove the dense element at `index`, shifting the tail down.
#[inline]
fn body_elements_remove(body: &mut ArrayBody, index: usize) -> Value {
    let len = body.dense_len();
    assert!(index < len, "dense remove out of range");
    let removed = body.dense_value(index).expect("live dense slot");
    for source in index + 1..len {
        let moved = body.dense_value(source).expect("live dense slot");
        body.write_dense_value(source - 1, moved);
    }
    body.truncate_dense(len - 1);
    removed
}

/// Byte offset of the cached dense-element base within [`ArrayBody`].
pub(crate) const ARRAY_BODY_ELEMENTS_PTR_OFFSET: usize =
    std::mem::offset_of!(ArrayBody, elements_ptr);

/// Byte offset of the cached dense length within [`ArrayBody`].
pub(crate) const ARRAY_BODY_DENSE_LEN_OFFSET: usize = std::mem::offset_of!(ArrayBody, dense_len);

/// Byte offset of the cached [`DenseElementKind`] discriminant.
pub(crate) const ARRAY_BODY_DENSE_KIND_OFFSET: usize = std::mem::offset_of!(ArrayBody, dense_kind);
/// Byte offset of the cached present-own-slot eligibility guard.
pub(crate) const ARRAY_BODY_DENSE_OWN_GUARD_OFFSET: usize =
    std::mem::offset_of!(ArrayBody, dense_own_guard);

pub(crate) const ARRAY_BODY_SLAB_OFFSET: usize = std::mem::offset_of!(ArrayBody, slab);
/// Byte offset of the compressed exotic-sidecar handle.
pub(crate) const ARRAY_BODY_EXOTIC_OFFSET: usize = std::mem::offset_of!(ArrayBody, exotic);
/// Byte offset of the compressed own non-index storage handle.
pub(crate) const ARRAY_BODY_EXPANDO_OFFSET: usize = std::mem::offset_of!(ArrayBody, expando);
pub(crate) const ARRAY_BODY_DENSE_CAP_OFFSET: usize = std::mem::offset_of!(ArrayBody, dense_cap);

// The whole-lookup word contains exactly two initialized compressed handles;
// it never includes slab state or padding, and both edges remain separately traced.
const _: () = {
    assert!(std::mem::size_of::<ArrayExoticHandle>() == 4);
    assert!(std::mem::size_of::<crate::object::JsObject>() == 4);
    assert!(ARRAY_BODY_EXPANDO_OFFSET == ARRAY_BODY_EXOTIC_OFFSET + 4);
    assert!((otter_gc::header::HEADER_SIZE + ARRAY_BODY_EXOTIC_OFFSET).is_multiple_of(8));
    assert!(std::mem::align_of::<ArrayBody>() == 8);
    assert!(std::mem::size_of::<ArrayBody>() == 40);
};

/// Native guard literal for terminal tagged dense storage.
pub(crate) const DENSE_ELEMENT_KIND_TAGGED: u32 = DenseElementKind::Tagged as u32;

/// Native guard literal for a hole-free unboxed-double dense prefix.
pub(crate) const DENSE_ELEMENT_KIND_PACKED_DOUBLE: u32 = DenseElementKind::PackedDouble as u32;
/// JIT-visible discriminant of numeric dense storage with holes.
pub(crate) const DENSE_ELEMENT_KIND_HOLEY_DOUBLE: u32 = DenseElementKind::HoleyDouble as u32;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`ArrayExoticSlots`].
pub const ARRAY_EXOTIC_SLOTS_TYPE_TAG: u8 = 0x38;

/// Handle to an array's rare/exotic sidecar.
pub(crate) type ArrayExoticHandle = otter_gc::Gc<ArrayExoticSlots>;

/// Cold sidecar for Array exotic features that are absent from ordinary dense
/// arrays. Its own GC body, so an array that has one still owns nothing
/// outside the heap.
#[derive(Debug, Default)]
pub(crate) struct ArrayExoticSlots {
    /// Sparse array-indexed own elements.
    ///
    /// Large holes do not force a dense allocation proportional to length.
    sparse_elements: Option<HashMap<usize, Value>>,
    /// Indexed accessor descriptors; each optional callable is collector traced.
    accessors: Option<HashMap<u32, (Option<Value>, Option<Value>)>>,
    /// Explicit indexed descriptor attributes; absence means data defaults.
    property_flags: Option<HashMap<u32, PropertyFlags>>,
    /// The Array length descriptor has become non-writable.
    length_nonwritable: bool,
    /// Verbatim slice of input text captured by `JSON.parse` for the
    /// lazy stringify memcpy fast-path. `Some` only when the array
    /// originated from `JSON.parse`; the slice spans the closing
    /// brackets `[…]` exactly.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-json.stringify> §25.5.2
    source_bytes: Option<Arc<[u8]>>,
    /// Mutation latch checked when captured JSON source bytes are present.
    dirty: bool,
    /// `[[Extensible]]` internal slot per §10.1.3. Starts `true`
    /// (`Default::default()`); flipped to `false` by
    /// `Object.preventExtensions` / `seal` / `freeze` on the array
    /// exotic. New string-keyed writes against a non-extensible
    /// array are rejected by the canonical Set/Define owner.
    extensible: ExtensibleFlag,
    /// Per-instance `[[Prototype]]` override for Array exotic
    /// objects constructed through subclassing. Plain arrays leave
    /// this unset and resolve to the realm `%Array.prototype%`.
    prototype_override: Option<Value>,
}

/// Byte offset of [`ArrayBody::length`] from the body start (after the GC
/// header). The baseline JIT reads this for the `arr.length` fast path after
/// guarding the receiver body tag.
pub const ARRAY_BODY_LENGTH_OFFSET: usize = std::mem::offset_of!(ArrayBody, length);

// Keep the JIT-visible dense array shell small: cold array state must stay in
// `ArrayExoticSlots`, not creep back into `ArrayBody`. The element base and
// length do not need a fixed position — compiled code reads them at the
// offsets `ARRAY_BODY_ELEMENTS_PTR_OFFSET` / `ARRAY_BODY_DENSE_LEN_OFFSET`
// publish — but they do need to stay in the shell rather than behind a hop.
const _: () = assert!(std::mem::size_of::<ArrayBody>() <= 64);

impl ArrayExoticSlots {
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        if let Some(entries) = &self.sparse_elements {
            for value in entries.values() {
                crate::code_liveness::visit_value(value, visitor);
            }
        }
        if let Some(entries) = &self.accessors {
            for (getter, setter) in entries.values() {
                if let Some(value) = getter {
                    crate::code_liveness::visit_value(value, visitor);
                }
                if let Some(value) = setter {
                    crate::code_liveness::visit_value(value, visitor);
                }
            }
        }
        if let Some(value) = &self.prototype_override {
            crate::code_liveness::visit_value(value, visitor);
        }
    }

    pub(crate) fn snapshot_content_summary(&self) -> String {
        format!(
            "sparse:{} accessors:{} flags:{} src:{}",
            self.sparse_elements.as_ref().map_or(0, |m| m.len()),
            self.accessors.as_ref().map_or(0, |m| m.len()),
            self.property_flags.as_ref().map_or(0, |m| m.len()),
            self.source_bytes.is_some()
        )
    }

    pub(crate) fn is_empty_for_snapshot(&self) -> bool {
        self.sparse_elements.is_none() && self.accessors.is_none() && self.source_bytes.is_none()
    }
}

impl otter_gc::trace::SeverRestoredPayload for ArrayExoticSlots {
    /// Sever foreign ownership on a snapshot restore. The copied containers
    /// alias the source isolate's mallocs and would be second owners, so they
    /// are overwritten with `None` without being read or dropped. The capture
    /// side guarantees they were empty ([`Self::is_empty_for_snapshot`]).
    fn sever_restored_payload(&mut self) {
        // SAFETY: overwriting without dropping (or reading) severs the
        // alias; the capture isolate remains the owner.
        unsafe {
            std::ptr::write(&mut self.sparse_elements, None);
            std::ptr::write(&mut self.accessors, None);
            std::ptr::write(&mut self.property_flags, None);
            std::ptr::write(&mut self.source_bytes, None);
        }
    }
}

impl otter_gc::SafeTraceable for ArrayExoticSlots {
    const TYPE_TAG: u8 = ARRAY_EXOTIC_SLOTS_TYPE_TAG;

    fn trace_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>) {
        // Mutable visits throughout so moving collections and page-image
        // relocation can rewrite every slot in place.
        if let Some(sparse) = &mut self.sparse_elements {
            for value in sparse.values_mut() {
                value.trace_value_slot_mut(visitor);
            }
        }
        if let Some(accessors) = &mut self.accessors {
            for (getter, setter) in accessors.values_mut() {
                if let Some(g) = getter {
                    g.trace_value_slot_mut(visitor);
                }
                if let Some(s) = setter {
                    s.trace_value_slot_mut(visitor);
                }
            }
        }
        if let Some(proto) = &mut self.prototype_override {
            proto.trace_value_slot_mut(visitor);
        }
    }
}

/// The sidecar payload behind `handle`, or `None` for a null handle.
///
/// The one place an exotic handle is decoded without going through the
/// heap — an array body holding a payload borrow has no heap to ask.
#[must_use]
fn exotic_body_of(handle: ArrayExoticHandle) -> Option<*mut ArrayExoticSlots> {
    if handle.is_null() {
        return None;
    }
    let header = handle.as_header_ptr();
    // SAFETY: a non-null handle names a live cell whose payload is an
    // `ArrayExoticSlots` one header past the start.
    Some(unsafe {
        header
            .cast::<u8>()
            .add(std::mem::size_of::<otter_gc::GcHeader>())
            .cast::<ArrayExoticSlots>()
    })
}

/// Give `arr` an exotic sidecar if it does not have one.
///
/// Creating the sidecar allocates, and an allocation can move the array,
/// so this runs outside the payload borrow with `arr` rooted. Pending
/// values the caller is about to store go through `external_visit`, which
/// is live for the same allocation. A no-op after the first call.
///
/// # Errors
/// Propagates [`otter_gc::OutOfMemory`].
pub(crate) fn ensure_exotic(
    arr: &mut JsArray,
    heap: &mut GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    if !heap.read_payload(*arr, |body| body.exotic.is_null()) {
        return Ok(());
    }
    let owner_slot = std::ptr::addr_of_mut!(*arr);
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        visitor(owner_slot.cast::<RawGc>());
    };
    let sidecar: ArrayExoticHandle =
        heap.alloc_variable_with_roots(ArrayExoticSlots::default(), 0, &mut visit)?;
    let owner = *arr;
    heap.with_payload(owner, |body| {
        body.exotic = sidecar;
    });
    // Installed by a raw payload write, so record the edge the mutator
    // barrier would have.
    heap.record_write(owner, &sidecar);
    Ok(())
}

/// Remember a write against the sidecar that actually holds it.
///
/// The exotic slots are their own old-space body, so the array is not
/// the parent of what they hold: a scavenge re-tracing the array finds
/// one edge, sees an old child, and stops. The array is remembered too,
/// because several call sites also clear the dense slot the sidecar
/// entry shadows.
fn record_exotic_array_write<V>(heap: &mut GcHeap, arr: JsArray, value: &V)
where
    V: otter_gc::GcStore + ?Sized,
{
    heap.record_write(arr, value);
    let sidecar = heap.read_payload(arr, |body| body.exotic);
    if !sidecar.is_null() {
        heap.record_write(sidecar, value);
    }
}

/// One-byte `[[Extensible]]` slot. Wrapper around `bool` with a
/// `Default = true` impl so [`ArrayBody::default()`] keeps the spec
/// initial state without spelling the field on every constructor.
#[derive(Debug, Clone, Copy)]
pub struct ExtensibleFlag(pub bool);

impl Default for ExtensibleFlag {
    fn default() -> Self {
        Self(true)
    }
}

impl ArrayBody {
    #[inline]
    fn exotic(&self) -> Option<&ArrayExoticSlots> {
        // SAFETY: a non-null handle names a live sidecar payload that
        // outlives this borrow of the array body.
        exotic_body_of(self.exotic).map(|body| unsafe { &*body })
    }

    /// Exclusive ref to the exotic sidecar.
    ///
    /// The sidecar is a GC body, so creating one allocates and a payload
    /// borrow has no heap. Every mutating path calls [`ensure_exotic`]
    /// first, outside the borrow; arriving here with no sidecar is a
    /// caller that forgot.
    #[inline]
    fn exotic_mut(&mut self) -> &mut ArrayExoticSlots {
        // Mutable sidecar access can introduce arbitrary baggage. Invalidate
        // before exposing it; completed mutations refresh their final state.
        self.dense_own_guard.set(1);
        let body = exotic_body_of(self.exotic)
            .expect("array exotic slots written without ensure_exotic reserving them");
        // SAFETY: as in `exotic`; `&mut self` rules out an aliasing read
        // through this array body.
        unsafe { &mut *body }
    }

    /// Exclusive ref to the sidecar when it exists, for removal-shaped
    /// mutations that must not create one.
    #[inline]
    fn exotic_opt_mut(&mut self) -> Option<&mut ArrayExoticSlots> {
        // SAFETY: as in `exotic_mut`.
        exotic_body_of(self.exotic).map(|body| unsafe { &mut *body })
    }

    #[inline]
    fn sparse_elements(&self) -> Option<&HashMap<usize, Value>> {
        self.exotic().and_then(|e| e.sparse_elements.as_ref())
    }

    #[inline]
    fn accessors(&self) -> Option<&HashMap<u32, (Option<Value>, Option<Value>)>> {
        self.exotic().and_then(|e| e.accessors.as_ref())
    }

    #[inline]
    fn property_flags(&self) -> Option<&HashMap<u32, PropertyFlags>> {
        self.exotic().and_then(|e| e.property_flags.as_ref())
    }

    #[inline]
    fn prototype_override(&self) -> Option<Value> {
        self.exotic().and_then(|e| e.prototype_override)
    }

    #[inline]
    fn extensible(&self) -> bool {
        self.exotic().is_none_or(|e| e.extensible.0)
    }

    #[inline]
    fn mark_dirty(&mut self) {
        if let Some(exotic) = self.exotic_opt_mut() {
            exotic.dirty = true;
        }
        self.refresh_dense_own_guard();
    }
}

/// Read the Array exotic's per-instance `[[Prototype]]` override.
pub(crate) fn prototype_override(arr: JsArray, heap: &GcHeap) -> Option<Value> {
    heap.read_payload(arr, |body| body.prototype_override())
}

/// Prove default-realm prototype ownership and absence of instance property state.
/// Both rare indexed state and the managed named bag must be absent before an
/// inherited method path can skip receiver lookup.
pub(crate) fn is_ordinary_dense(arr: JsArray, heap: &GcHeap) -> bool {
    heap.read_payload(arr, |body| body.exotic.is_null() && body.expando.is_null())
}

/// Prove the absence of rare indexed state after the callable is already resolved.
/// Named storage cannot affect dense index data or default prototype ownership.
pub(crate) fn is_ordinary_indexed(arr: JsArray, heap: &GcHeap) -> bool {
    heap.read_payload(arr, |body| body.exotic.is_null())
}

/// Set the Array exotic's per-instance `[[Prototype]]` override.
///
/// Spec: §10.4.2 Array exotic objects still have ordinary
/// `[[GetPrototypeOf]]` / `[[SetPrototypeOf]]`; subclassing Array
/// needs a per-object slot rather than a realm-level intrinsic
/// fallback.
///
/// <https://tc39.es/ecma262/#sec-array-exotic-objects>
pub(crate) fn set_prototype_override(
    arr: JsArray,
    heap: &mut GcHeap,
    proto: Option<Value>,
) -> Result<(), otter_gc::OutOfMemory> {
    // The sidecar allocates, so it is reserved here, outside the payload
    // borrow below. This may move `arr`, which is why the local is `mut`.
    let mut arr = arr;
    let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
        if let Some(value) = &proto {
            value.trace_value_slots(visitor);
        }
    };
    ensure_exotic(&mut arr, heap, &mut roots)?;
    let barrier_value = proto;
    heap.with_payload(arr, |body| {
        body.exotic_mut().prototype_override = proto;
        body.refresh_dense_own_guard();
    });
    if let Some(value) = &barrier_value {
        record_exotic_array_write(heap, arr, value);
    }
    Ok(())
}

/// Allocate an old-space empty array for raw GC fixtures.
///
/// # Errors
///
/// Returns [`otter_gc::OutOfMemory`] if the array shell allocation
/// would exceed the configured heap cap.
#[cfg(test)]
pub(crate) fn alloc_array_old_for_fixture(
    heap: &mut GcHeap,
) -> Result<JsArray, otter_gc::OutOfMemory> {
    heap.alloc_old(ArrayBody::default())
}

/// Allocate a fresh empty array while exposing caller-owned roots.
pub(crate) fn alloc_array_with_roots(
    heap: &mut GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsArray, otter_gc::OutOfMemory> {
    heap.alloc_with_roots(ArrayBody::default(), external_visit)
}

/// Construct an old-space fixture array from initial elements.
///
/// # Errors
///
/// Returns [`otter_gc::OutOfMemory`] if either the array shell or
/// off-slot dense storage reservation would exceed the heap cap.
#[cfg(test)]
pub(crate) fn from_elements_old_for_fixture(
    heap: &mut GcHeap,
    values: impl IntoIterator<Item = Value>,
) -> Result<JsArray, otter_gc::OutOfMemory> {
    let mut collected: Vec<Value> = values.into_iter().collect();
    let length = collected.len();
    let slab = slab_from_values(
        heap,
        &mut collected,
        otter_gc::SitePlacement::Young(0),
        &mut |_| {},
    )?;
    let mut body = ArrayBody {
        length,
        ..Default::default()
    };
    body.adopt_slab(slab, 0);
    copy_values_into(&mut body, &collected);
    record_slab_contents(heap, slab, length);
    heap.alloc_old(body)
}

/// Construct an array from initial elements through the young-generation
/// allocation path.
///
/// The caller-provided roots cover interpreter/runtime slots. The allocation
/// API also traces the pending [`ArrayBody`] payload itself, so the copied
/// element values are visible if a collection runs before the array shell is
/// materialised.
pub(crate) fn from_elements_with_roots(
    heap: &mut GcHeap,
    values: impl IntoIterator<Item = Value>,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsArray, otter_gc::OutOfMemory> {
    let mut collected: smallvec::SmallVec<[Value; 8]> = values.into_iter().collect();
    from_values_with_roots(
        heap,
        &mut collected,
        otter_gc::SitePlacement::Young(0),
        external_visit,
    )
}

/// Construct an array from an already materialized element vector at
/// `placement`: the shell carries a tracking site, and a tenured site's slab
/// and shell are both born old.
///
/// This is the array-literal fast boundary: bytecode/JIT callers have already
/// copied source registers into owned storage, so taking the vector directly
/// avoids a second transient argument container before the GC allocation.
pub(crate) fn from_values_with_roots(
    heap: &mut GcHeap,
    collected: &mut [Value],
    placement: otter_gc::SitePlacement,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsArray, otter_gc::OutOfMemory> {
    let length = collected.len();
    let slab_placement = if placement == otter_gc::SitePlacement::Old {
        otter_gc::SitePlacement::Old
    } else {
        otter_gc::SitePlacement::Young(0)
    };
    let slab = slab_from_values(heap, collected, slab_placement, external_visit)?;
    let mut body = ArrayBody {
        length,
        ..Default::default()
    };
    body.adopt_slab(slab, 0);
    copy_values_into(&mut body, collected);
    record_slab_contents(heap, slab, length);
    alloc_body_with_adopted_elements(heap, body, placement, external_visit)
}

/// Allocate a slab sized to `values` and copy them in.
///
/// The values are rooted across the allocation — they are ordinary
/// `Value`s on the caller's stack, and a collection here would otherwise
/// leave the copies naming pre-move objects.
fn slab_from_values(
    heap: &mut GcHeap,
    values: &mut [Value],
    placement: otter_gc::SitePlacement,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<elements::ElementSlabHandle, otter_gc::OutOfMemory> {
    if values.is_empty() {
        return Ok(elements::ElementSlabHandle::null());
    }
    let count = values.len();
    let base = values.as_mut_ptr();
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        for index in 0..count {
            // SAFETY: `index < count` and the caller's slice outlives
            // this call, so the entry is a live slot to rewrite.
            let value = unsafe { &mut *base.add(index) };
            value.trace_value_slot_mut(visitor);
        }
    };
    let kind = initial_dense_kind(values);
    elements::alloc_element_slab(heap, count, kind, placement, &mut visit)
}

fn initial_dense_kind(values: &[Value]) -> DenseElementKind {
    let mut saw_hole = false;
    for value in values {
        if value.is_hole() {
            saw_hole = true;
        } else if !value.is_number() {
            return DenseElementKind::Tagged;
        }
    }
    if saw_hole {
        DenseElementKind::HoleyDouble
    } else {
        DenseElementKind::PackedDouble
    }
}

#[inline]
fn dense_kind_for_value(value: Value) -> DenseElementKind {
    if value.is_hole() {
        DenseElementKind::HoleyDouble
    } else if value.is_number() {
        DenseElementKind::PackedDouble
    } else {
        DenseElementKind::Tagged
    }
}

/// Copy `values` into a body that has already adopted a slab big enough
/// for them.
fn copy_values_into(body: &mut ArrayBody, values: &[Value]) {
    debug_assert!(body.dense_capacity() >= values.len());
    if let Some(slab) = body.slab_body_mut() {
        for (index, value) in values.iter().copied().enumerate() {
            slab.set(index, value);
        }
    }
    body.set_dense_len(values.len());
    // One refresh for the whole copy: a numeric slab that took a tagged value
    // changed kind in place.
    body.refresh_element_cache();
}

/// Allocate an [`ArrayBody`] whose dense element slab has already been
/// materialized and adopted by the body.
///
/// The slab was accounted when it was allocated. The common path now tries a
/// no-collect young allocation for the array shell; any miss falls back to the
/// rooted allocator, which traces the pending body and its slab handle.
fn alloc_body_with_adopted_elements(
    heap: &mut GcHeap,
    body: ArrayBody,
    placement: otter_gc::SitePlacement,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsArray, otter_gc::OutOfMemory> {
    // The elements are a GC body of their own now, so they were counted
    // when they were allocated. All that is left is the array shell.
    if placement != otter_gc::SitePlacement::Young(0) {
        return heap.alloc_trailing_at_site_with_roots_initialized(
            body,
            0,
            placement,
            external_visit,
            |_| {},
        );
    }
    match heap.try_alloc_no_collect_or_return(body) {
        Ok(array) => Ok(array),
        Err(returned) => heap.alloc_with_roots(returned, external_visit),
    }
}

/// Construct an array from initial elements **and** attach the
/// verbatim slice of input text the elements were parsed from.
///
/// Used exclusively by `JSON.parse`: the captured `source_bytes`
/// powers the lazy stringify memcpy fast-path that re-emits the
/// original textual representation without iterating elements
/// when the array has not been mutated.
///
/// Spec: <https://tc39.es/ecma262/#sec-json.parse> §25.5.1
///
/// # Errors
///
/// Returns [`otter_gc::OutOfMemory`] if either the array shell or
/// off-slot dense storage reservation would exceed the heap cap.
#[cfg(test)]
fn from_elements_with_source_old_for_fixture(
    heap: &mut GcHeap,
    values: impl IntoIterator<Item = Value>,
    source_bytes: Arc<[u8]>,
) -> Result<JsArray, otter_gc::OutOfMemory> {
    let mut collected: Vec<Value> = values.into_iter().collect();
    let length = collected.len();
    let mut sidecar = alloc_source_sidecar(heap, source_bytes, &mut collected, &mut |_| {})?;
    let sidecar_slot = std::ptr::addr_of_mut!(sidecar);
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        visitor(sidecar_slot.cast::<RawGc>());
    };
    let slab = slab_from_values(
        heap,
        &mut collected,
        otter_gc::SitePlacement::Young(0),
        &mut visit,
    )?;
    let mut body = ArrayBody {
        length,
        exotic: sidecar,
        ..Default::default()
    };
    body.adopt_slab(slab, 0);
    copy_values_into(&mut body, &collected);
    record_slab_contents(heap, slab, length);
    heap.alloc_old(body)
}

/// Allocate a source-bytes sidecar before its array shell exists.
///
/// The sidecar holds no GC references at this point, but allocating it
/// can move the caller's pending element values, so they are rooted for
/// the duration.
fn alloc_source_sidecar(
    heap: &mut GcHeap,
    source_bytes: Arc<[u8]>,
    pending: &mut [Value],
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<ArrayExoticHandle, otter_gc::OutOfMemory> {
    let count = pending.len();
    let base = pending.as_mut_ptr();
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        for index in 0..count {
            // SAFETY: `index < count` and the caller's slice outlives
            // this call, so the entry is a live slot to rewrite.
            let value = unsafe { &mut *base.add(index) };
            value.trace_value_slot_mut(visitor);
        }
    };
    heap.alloc_variable_with_roots(
        ArrayExoticSlots {
            source_bytes: Some(source_bytes),
            dirty: false,
            ..ArrayExoticSlots::default()
        },
        0,
        &mut visit,
    )
}

/// Construct an array from initial elements, attach source bytes, and expose
/// caller-owned roots during dense-storage reservation and array shell
/// allocation.
pub(crate) fn from_elements_with_source_and_roots(
    heap: &mut GcHeap,
    values: impl IntoIterator<Item = Value>,
    source_bytes: Arc<[u8]>,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsArray, otter_gc::OutOfMemory> {
    let mut collected: Vec<Value> = values.into_iter().collect();
    let length = collected.len();
    let mut sidecar = alloc_source_sidecar(heap, source_bytes, &mut collected, external_visit)?;
    let sidecar_slot = std::ptr::addr_of_mut!(sidecar);
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        visitor(sidecar_slot.cast::<RawGc>());
    };
    let slab = slab_from_values(
        heap,
        &mut collected,
        otter_gc::SitePlacement::Young(0),
        &mut visit,
    )?;
    let mut body = ArrayBody {
        length,
        exotic: sidecar,
        ..Default::default()
    };
    body.adopt_slab(slab, 0);
    copy_values_into(&mut body, &collected);
    record_slab_contents(heap, slab, length);
    alloc_body_with_adopted_elements(heap, body, otter_gc::SitePlacement::Young(0), &mut visit)
}

/// Length in elements (O(1)).
#[must_use]
pub fn len(arr: JsArray, heap: &otter_gc::GcHeap) -> usize {
    heap.read_payload(arr, |body| body.length)
}

/// `true` for an empty array.
#[must_use]
pub fn is_empty(arr: JsArray, heap: &otter_gc::GcHeap) -> bool {
    len(arr, heap) == 0
}

/// Current physical representation of the array's dense prefix.
#[must_use]
#[cfg(test)]
pub(crate) fn dense_element_kind(arr: JsArray, heap: &otter_gc::GcHeap) -> DenseElementKind {
    heap.read_payload(arr, ArrayBody::dense_kind)
}

/// Read element at `idx`. Out-of-range and array-hole slots both
/// return `undefined` per ECMA-262 §10.4.2 OrdinaryGet —
/// internal hole sentinel never escapes the array.
#[must_use]
pub fn get(arr: JsArray, heap: &otter_gc::GcHeap, idx: usize) -> Value {
    heap.read_payload(arr, |body| {
        #[cfg(debug_assertions)]
        debug_assert!(
            body.element_cache_is_current(),
            "stale dense-element cache on array read",
        );
        let raw = body
            .dense_value(idx)
            .or_else(|| {
                body.sparse_elements()
                    .and_then(|sparse| sparse.get(&idx).cloned())
            })
            .unwrap_or(Value::undefined());
        if raw.is_hole() {
            Value::undefined()
        } else {
            raw
        }
    })
}

/// Spec [HasProperty](https://tc39.es/ecma262/#sec-array-exotic-objects)
/// for array-indexed slots: a missing dense element (hole)
/// or an absent sparse entry returns `false`, even when the index
/// is below `length`. Returns `true` only when an explicit value
/// occupies the slot.
#[must_use]
pub fn has_own_element(arr: JsArray, heap: &otter_gc::GcHeap, idx: usize) -> bool {
    heap.read_payload(arr, |body| {
        if let Some(slot) = body.dense_value(idx) {
            return !slot.is_hole();
        }
        body.sparse_elements()
            .is_some_and(|sparse| sparse.contains_key(&idx))
    })
}

/// Fast read for callback-heavy dense array algorithms.
///
/// Returns an own data element only when the array has no indexed accessors.
/// `None` means the caller must run the ordinary property path so holes,
/// accessors, inherited indices, and sparse misses stay observable.
#[must_use]
pub(crate) fn own_data_element_without_accessors(
    arr: JsArray,
    heap: &otter_gc::GcHeap,
    idx: usize,
) -> Option<Value> {
    heap.read_payload(arr, |body| {
        if body.accessors().is_some() {
            return None;
        }
        let value = body.dense_value(idx).or_else(|| {
            body.sparse_elements()
                .and_then(|sparse| sparse.get(&idx).cloned())
        })?;
        if value.is_hole() { None } else { Some(value) }
    })
}

/// Own dense data element of a plain array, when reading it is unobservable.
///
/// `Some` proves the array carries no exotic sidecar and no indexed accessor
/// and that `idx` holds a real element, so the value is the complete
/// `[[Get]]` answer: no hole can escape to the prototype chain and no getter
/// can run. `None` sends the caller to the ordinary property path.
#[must_use]
pub(crate) fn plain_dense_element(
    arr: JsArray,
    heap: &otter_gc::GcHeap,
    idx: usize,
) -> Option<Value> {
    heap.read_payload(arr, |body| {
        if !body.exotic.is_null() || body.accessors().is_some() {
            return None;
        }
        let value = body.dense_value(idx)?;
        if value.is_hole() { None } else { Some(value) }
    })
}

/// Whether `idx` names no own element of a plain dense array with no
/// receiver-local exotic state: a hole, or an index past its dense elements.
///
/// Prototype observability is deliberately not decided here: callers must
/// separately prove that inherited indexed accessors cannot answer the hole.
#[must_use]
pub(crate) fn is_plain_dense_absent(arr: JsArray, heap: &otter_gc::GcHeap, idx: usize) -> bool {
    heap.read_payload(arr, |body| {
        body.exotic.is_null() && body.dense_value(idx).is_none_or(Value::is_hole)
    })
}

/// `HasProperty(arr, idx)` from a plain array's own storage, when that
/// settles it: `Some(true)` for a present element; `Some(false)` for a hole
/// or an index past the end when `no_inherited_indices` proves neither
/// %Array.prototype% nor %Object.prototype% holds an index. `None` leaves
/// the ordinary prototype walk authoritative.
#[must_use]
pub(crate) fn plain_dense_has(
    arr: JsArray,
    heap: &otter_gc::GcHeap,
    idx: usize,
    no_inherited_indices: bool,
) -> Option<bool> {
    heap.read_payload(arr, |body| {
        if !body.exotic.is_null() {
            return None;
        }
        match body.dense_value(idx) {
            Some(value) if !value.is_hole() => Some(true),
            _ if no_inherited_indices => Some(false),
            _ => None,
        }
    })
}

/// Replace one slot in a plain dense array without allocation or key creation.
///
/// Existing data slots are always eligible. A hole is eligible only when the
/// caller has proved that inherited indexed accessors cannot observe the
/// assignment. `false` leaves the complete `OrdinarySet` path authoritative.
pub(crate) fn set_plain_dense_slot(
    arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    idx: usize,
    value: Value,
    allow_hole: bool,
) -> bool {
    let stored = heap.with_payload(arr, |body| {
        if !body.exotic.is_null() {
            return false;
        }
        let Some(previous) = body.dense_value(idx) else {
            return false;
        };
        if previous.is_hole() && !allow_hole {
            return false;
        }
        body.write_dense_value(idx, value);
        body.length = body.length.max(idx.saturating_add(1));
        body.mark_dirty();
        true
    });
    if stored {
        record_array_write(heap, arr, &value);
    }
    stored
}

/// Write element at `idx`, extending with the internal
/// hole sentinel when `idx > len` so absent slots remain
/// distinguishable from explicit `undefined` per ECMA-262 §10.4.2.
///
/// # Errors
///
/// Returns [`otter_gc::OutOfMemory`] if extending dense storage would
/// exceed the configured heap cap.
pub fn set(
    arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    idx: usize,
    value: Value,
) -> Result<(), otter_gc::OutOfMemory> {
    set_index_value(arr, heap, idx, value, true)
}

/// Define an indexed data property after descriptor validation has
/// already approved the write.
pub(crate) fn define_index_value(
    arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    idx: usize,
    value: Value,
) -> Result<(), otter_gc::OutOfMemory> {
    set_index_value(arr, heap, idx, value, false)
}

fn set_index_value(
    mut arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    idx: usize,
    value: Value,
    enforce_writable: bool,
) -> Result<(), otter_gc::OutOfMemory> {
    // Definitions have already validated attributes; assignments consult the
    // numeric indexed descriptor without allocating a property-name string.
    if enforce_writable && !can_write_index_property(arr, heap, idx as u32) {
        return Ok(());
    }
    if !has_own_element(arr, heap, idx) && !is_extensible(arr, heap) {
        return Ok(());
    }
    let target_len = idx.saturating_add(1);
    if should_store_sparse(arr, heap, idx) {
        let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            value.trace_value_slots(visitor);
        };
        ensure_exotic(&mut arr, heap, &mut roots)?;
        heap.with_payload(arr, |body| {
            let sparse = body
                .exotic_mut()
                .sparse_elements
                .get_or_insert_with(HashMap::new);
            sparse.insert(idx, value);
            body.length = body.length.max(target_len);
            body.mark_dirty();
        });
        record_exotic_array_write(heap, arr, &value);
        return Ok(());
    }
    // The reservation may trigger an emergency collection: `value` must be
    // traced through it or a moved heap object is written back stale.
    let preferred_kind = dense_kind_for_value(value);
    {
        let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            value.trace_value_slots(visitor);
        };
        reserve_dense_capacity(&mut arr, heap, target_len, preferred_kind, &mut roots)?;
    }
    heap.with_payload(arr, |body| {
        if idx < body.dense_len() {
            body.write_dense_value(idx, value);
            body.length = body.length.max(target_len);
            body.mark_dirty();
            return;
        }
        body.refresh_element_cache();
        while body.dense_len() < idx {
            body.push_dense(Value::hole());
        }
        body.push_dense(value);
        body.length = body.length.max(target_len);
        body.mark_dirty();
    });
    record_array_write(heap, arr, &value);
    Ok(())
}

/// Write an indexed element while exposing caller-owned roots during any
/// dense-storage reservation.
///
/// This mirrors [`set`] for VM stack-owned mutation sites. Sparse writes do
/// not reserve dense backing storage and therefore keep the ordinary path; low
/// dense writes trace the receiver handle plus pending value before a possible
/// reservation-triggered emergency collection.
pub(crate) fn set_with_roots(
    mut arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    idx: usize,
    value: Value,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    let key = idx as u32;
    if !can_write_index_property(arr, heap, key) {
        return Ok(());
    }
    if !has_own_element(arr, heap, idx) && !is_extensible(arr, heap) {
        return Ok(());
    }
    let target_len = idx.saturating_add(1);
    if should_store_sparse(arr, heap, idx) {
        let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            value.trace_value_slots(visitor);
        };
        ensure_exotic(&mut arr, heap, &mut roots)?;
        heap.with_payload(arr, |body| {
            let sparse = body
                .exotic_mut()
                .sparse_elements
                .get_or_insert_with(HashMap::new);
            sparse.insert(idx, value);
            body.length = body.length.max(target_len);
            body.mark_dirty();
        });
        record_exotic_array_write(heap, arr, &value);
        return Ok(());
    }
    {
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            value.trace_value_slots(visitor);
        };
        reserve_dense_capacity(
            &mut arr,
            heap,
            target_len,
            dense_kind_for_value(value),
            &mut reserve_roots,
        )?;
    }
    heap.with_payload(arr, |body| {
        if idx < body.dense_len() {
            body.write_dense_value(idx, value);
            body.length = body.length.max(target_len);
            body.mark_dirty();
            return;
        }
        body.refresh_element_cache();
        while body.dense_len() < idx {
            body.push_dense(Value::hole());
        }
        body.push_dense(value);
        body.length = body.length.max(target_len);
        body.mark_dirty();
    });
    record_array_write(heap, arr, &value);
    Ok(())
}

/// Return whether a dense bulk fill can bypass generic `[[Set]]`.
///
/// Callers must still prove the prototype chain has no indexed
/// properties in the target range. This helper only checks receiver
/// state that would make direct data writes observably different from
/// ordinary array assignment.
#[must_use]
pub(crate) fn can_fast_fill_dense_range(
    arr: JsArray,
    heap: &otter_gc::GcHeap,
    start: usize,
    end: usize,
) -> bool {
    if start >= end {
        return true;
    }
    const MAX_DENSE_INDEX: usize = 1 << 20;
    heap.read_payload(arr, |body| {
        if !body.extensible() || body.prototype_override().is_some() || end > MAX_DENSE_INDEX {
            return false;
        }
        if start.saturating_sub(body.dense_len()) > 1024 {
            return false;
        }
        let in_range = |key: &u32| (start..end).contains(&(*key as usize));
        if body
            .accessors()
            .is_some_and(|accessors| accessors.keys().any(&in_range))
        {
            return false;
        }
        if body
            .property_flags()
            .is_some_and(|flags| flags.keys().any(in_range))
        {
            return false;
        }
        true
    })
}

/// Fill a proven-plain dense array range with one data value.
///
/// The caller owns the spec checks; this function performs only the
/// rooted reservation and contiguous writes.
pub(crate) fn fill_dense_range_with_roots(
    mut arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    start: usize,
    end: usize,
    value: Value,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    if start >= end {
        return Ok(());
    }
    {
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            value.trace_value_slots(visitor);
        };
        reserve_dense_capacity(
            &mut arr,
            heap,
            end,
            dense_kind_for_value(value),
            &mut reserve_roots,
        )?;
    }
    heap.with_payload(arr, |body| {
        body.refresh_element_cache();
        while body.dense_len() < start {
            body.push_dense(Value::hole());
        }
        let existing_end = end.min(body.dense_len());
        for idx in start..existing_end {
            body.write_dense_value(idx, value);
        }
        while body.dense_len() < end {
            body.push_dense(value);
        }
        body.length = body.length.max(end);
        body.mark_dirty();
    });
    record_array_write(heap, arr, &value);
    Ok(())
}

/// Push to the tail. Returns the new length.
///
/// # Errors
///
/// Returns [`otter_gc::OutOfMemory`] if growing dense storage would
/// exceed the configured heap cap.
pub fn push(
    mut arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    value: Value,
) -> Result<usize, otter_gc::OutOfMemory> {
    let barrier_value = value;
    let target_len = len(arr, heap).saturating_add(1);
    reserve_dense_capacity(
        &mut arr,
        heap,
        target_len,
        dense_kind_for_value(value),
        &mut |_| {},
    )?;
    let new_len = heap.with_payload(arr, |body| {
        while body.dense_len() + 1 < target_len {
            body.push_dense(Value::hole());
        }
        body.push_dense(value);
        body.length = target_len;
        body.mark_dirty();
        body.length
    });
    record_array_write(heap, arr, &barrier_value);
    Ok(new_len)
}

/// Push to the tail while exposing caller-owned roots during any
/// off-slot dense-storage reservation.
///
/// This mirrors [`push`] but is reserved for VM stack-owned mutation
/// sites. The pending value and the receiver handle are traced along
/// with the caller-provided roots if reservation-triggered emergency
/// collection runs before the dense vector grows.
pub(crate) fn push_with_roots(
    mut arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    value: Value,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<usize, otter_gc::OutOfMemory> {
    let target_len = len(arr, heap).saturating_add(1);
    {
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            value.trace_value_slots(visitor);
        };
        reserve_dense_capacity(
            &mut arr,
            heap,
            target_len,
            dense_kind_for_value(value),
            &mut reserve_roots,
        )?;
    }
    let new_len = heap.with_payload(arr, |body| {
        while body.dense_len() + 1 < target_len {
            body.push_dense(Value::hole());
        }
        body.push_dense(value);
        body.length = target_len;
        body.mark_dirty();
        body.length
    });
    record_array_write(heap, arr, &value);
    Ok(new_len)
}

/// Append `values` to the tail with one storage reservation, tracing the
/// pending values and the caller's roots across it.
pub(crate) fn extend_with_roots(
    mut arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    values: &[Value],
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<usize, otter_gc::OutOfMemory> {
    let start = len(arr, heap);
    let target_len = start.saturating_add(values.len());
    let kind = values
        .iter()
        .map(|value| dense_kind_for_value(*value))
        .max_by_key(|kind| *kind as u8)
        .unwrap_or(DenseElementKind::PackedDouble);
    {
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            for value in values {
                value.trace_value_slots(visitor);
            }
        };
        reserve_dense_capacity(&mut arr, heap, target_len, kind, &mut reserve_roots)?;
    }
    heap.with_payload(arr, |body| {
        body.refresh_element_cache();
        while body.dense_len() < start {
            body.push_dense(Value::hole());
        }
        for value in values {
            body.push_dense(*value);
        }
        body.length = target_len;
        body.mark_dirty();
    });
    for value in values {
        record_array_write(heap, arr, value);
    }
    Ok(target_len)
}

/// Spec §10.4.2.4 step 17 truncation / step 9 growth of the dense
/// element storage backing `Array.prototype.length`. Shrinks below
/// the current length by dropping dense and sparse slots whose index
/// is ≥ `new_len`; grows above the current length by extending the
/// dense vector with hole sentinel so absent indices remain
/// distinguishable from explicit `undefined`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-arraysetlength>
pub fn set_length(
    mut arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    new_len: usize,
) -> Result<(), otter_gc::OutOfMemory> {
    let cur = len(arr, heap);
    if cur == new_len {
        return Ok(());
    }
    if new_len < cur {
        heap.with_payload(arr, |body| {
            body.length = new_len;
            body_elements_truncate(body, new_len);
            if let Some(exotic) = body.exotic_opt_mut()
                && let Some(sparse) = exotic.sparse_elements.as_mut()
            {
                sparse.retain(|k, _| *k < new_len);
                if sparse.is_empty() {
                    exotic.sparse_elements = None;
                }
            }
            if let Some(exotic) = body.exotic_opt_mut()
                && let Some(accessors) = exotic.accessors.as_mut()
            {
                accessors.retain(|key, _| (*key as usize) < new_len);
                if accessors.is_empty() {
                    exotic.accessors = None;
                }
            }
            if let Some(exotic) = body.exotic_opt_mut()
                && let Some(flags) = exotic.property_flags.as_mut()
            {
                flags.retain(|key, _| (*key as usize) < new_len);
                if flags.is_empty() {
                    exotic.property_flags = None;
                }
            }
            body.mark_dirty();
        });
        return Ok(());
    }
    const MAX_DENSE_LENGTH_GROWTH: usize = 1 << 20;
    if new_len <= MAX_DENSE_LENGTH_GROWTH {
        reserve_dense_capacity(
            &mut arr,
            heap,
            new_len,
            DenseElementKind::HoleyDouble,
            &mut |_| {},
        )?;
    }
    heap.with_payload(arr, |body| {
        if new_len <= MAX_DENSE_LENGTH_GROWTH {
            body.refresh_element_cache();
            while body.dense_len() < new_len {
                body.push_dense(Value::hole());
            }
        }
        body.length = new_len;
        body.mark_dirty();
    });
    Ok(())
}

/// Shrink `length` through ArraySetLength deletion semantics.
///
/// Returns `false` when a non-configurable indexed property blocks
/// deletion. In that case all higher configurable elements have been
/// removed and `length` is left at the blocked index + 1, matching
/// §10.4.2.4 step 17.
pub(crate) fn set_length_checked(
    arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    new_len: usize,
) -> Result<bool, otter_gc::OutOfMemory> {
    let cur = len(arr, heap);
    if new_len >= cur {
        set_length(arr, heap, new_len)?;
        return Ok(true);
    }
    let ok = heap.with_payload(arr, |body| {
        // §10.4.2.4 deletes from `cur - 1` down to `new_len`, stopping at
        // the highest non-configurable index. Only *present* indices can
        // block or need deleting, so gather those instead of walking the
        // whole `[new_len, cur)` range — a sparse `length` (e.g. `2**32-1`)
        // would otherwise spin over billions of holes.
        let mut present: Vec<usize> = Vec::new();
        let dense_hi = body.dense_len().min(cur);
        for idx in new_len..dense_hi {
            if body.dense_value(idx).is_some_and(|value| !value.is_hole()) {
                present.push(idx);
            }
        }
        if let Some(sparse) = body.sparse_elements() {
            present.extend(sparse.keys().copied().filter(|&k| k >= new_len && k < cur));
        }
        if let Some(accessors) = body.accessors() {
            for key in accessors.keys() {
                let idx = *key as usize;
                if idx >= new_len && idx < cur {
                    present.push(idx);
                }
            }
        }
        present.sort_unstable();
        present.dedup();
        for &idx in present.iter().rev() {
            let key = idx as u32;
            let configurable = body
                .property_flags()
                .and_then(|flags| flags.get(&key))
                .is_none_or(|flags| flags.configurable());
            if !configurable {
                truncate_array_body_to(body, idx + 1);
                return false;
            }
            delete_array_body_index(body, idx);
        }
        truncate_array_body_to(body, new_len);
        true
    });
    Ok(ok)
}

#[must_use]
pub(crate) fn length_flags(arr: JsArray, heap: &GcHeap) -> PropertyFlags {
    heap.read_payload(arr, |body| {
        PropertyFlags::new(
            body.exotic().is_none_or(|e| !e.length_nonwritable),
            false,
            false,
        )
    })
}

#[must_use]
pub(crate) fn length_writable(arr: JsArray, heap: &otter_gc::GcHeap) -> bool {
    length_flags(arr, heap).writable()
}

pub(crate) fn set_length_writable(
    mut arr: JsArray,
    heap: &mut GcHeap,
    writable: bool,
) -> Result<(), otter_gc::OutOfMemory> {
    if length_writable(arr, heap) == writable {
        return Ok(());
    }
    ensure_exotic(&mut arr, heap, &mut |_| {})?;
    heap.with_payload(arr, |body| {
        body.exotic_mut().length_nonwritable = !writable;
        body.mark_dirty();
    });
    Ok(())
}

fn delete_array_body_index(body: &mut ArrayBody, idx: usize) {
    if body.dense_value(idx).is_some() {
        body.write_dense_value(idx, Value::hole());
    }
    if let Some(exotic) = body.exotic_opt_mut()
        && let Some(sparse) = exotic.sparse_elements.as_mut()
    {
        sparse.remove(&idx);
        if sparse.is_empty() {
            exotic.sparse_elements = None;
        }
    }
    let key = idx as u32;
    if let Some(exotic) = body.exotic_opt_mut()
        && let Some(accessors) = exotic.accessors.as_mut()
    {
        accessors.remove(&key);
        if accessors.is_empty() {
            exotic.accessors = None;
        }
    }
    if let Some(exotic) = body.exotic_opt_mut()
        && let Some(flags) = exotic.property_flags.as_mut()
    {
        flags.remove(&key);
        if flags.is_empty() {
            exotic.property_flags = None;
        }
    }
    body.mark_dirty();
}

fn truncate_array_body_to(body: &mut ArrayBody, len: usize) {
    body.length = len;
    body_elements_truncate(body, len);
    if let Some(exotic) = body.exotic_opt_mut()
        && let Some(sparse) = exotic.sparse_elements.as_mut()
    {
        sparse.retain(|idx, _| *idx < len);
        if sparse.is_empty() {
            exotic.sparse_elements = None;
        }
    }
    if let Some(exotic) = body.exotic_opt_mut()
        && let Some(accessors) = exotic.accessors.as_mut()
    {
        accessors.retain(|key, _| (*key as usize) < len);
        if accessors.is_empty() {
            exotic.accessors = None;
        }
    }
    if let Some(exotic) = body.exotic_opt_mut()
        && let Some(flags) = exotic.property_flags.as_mut()
    {
        flags.retain(|key, _| (*key as usize) < len);
        if flags.is_empty() {
            exotic.property_flags = None;
        }
    }
    body.mark_dirty();
}

/// Remove the head element of a fully dense array, sliding the rest down.
///
/// Callers must have established that the array is dense over its whole
/// logical length (`dense_shift_applies`); the head is removed from the
/// element buffer directly, so no per-index `[[Set]]` / `[[Delete]]` protocol
/// runs. Never allocates: the buffer only shrinks.
#[must_use]
pub(crate) fn dense_shift(arr: JsArray, heap: &mut otter_gc::GcHeap) -> Value {
    heap.with_payload(arr, |body| {
        debug_assert!(body.exotic.is_null(), "dense shift over an exotic array");
        if body.dense_len() == 0 {
            return Value::undefined();
        }
        let head = body_elements_remove(body, 0);
        body.length = body.dense_len();
        body.refresh_element_cache();
        if head.is_hole() {
            Value::undefined()
        } else {
            head
        }
    })
}

/// Insert `value` at the head of a fully dense array, sliding the rest up.
///
/// Mirrors [`dense_shift`]; may grow the element buffer, so the receiver
/// handle is threaded through the reservation and republished.
///
/// # Errors
///
/// Returns [`otter_gc::OutOfMemory`] if growing dense storage would exceed the
/// configured heap cap.
pub(crate) fn dense_unshift_with_roots(
    mut arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    value: Value,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<usize, otter_gc::OutOfMemory> {
    let barrier_value = value;
    let target_len = len(arr, heap).saturating_add(1);
    {
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            value.trace_value_slots(visitor);
        };
        reserve_dense_capacity(
            &mut arr,
            heap,
            target_len,
            dense_kind_for_value(value),
            &mut reserve_roots,
        )?;
    }
    let new_len = heap.with_payload(arr, |body| {
        debug_assert!(body.exotic.is_null(), "dense unshift over an exotic array");
        body_elements_insert(body, 0, value);
        body.length = body.dense_len();
        body.refresh_element_cache();
        body.length
    });
    record_array_write(heap, arr, &barrier_value);
    Ok(new_len)
}

/// `true` when the array is dense over its whole logical length with no hole
/// sentinel — the precondition that makes [`dense_shift`] /
/// [`dense_unshift_with_roots`] observationally identical to the spec loop.
///
/// A hole would make the spec's per-index `Get` read an inherited value off the
/// prototype chain, and a sparse tail would leave indices the element buffer
/// does not cover, so both keep the generic path.
#[must_use]
pub(crate) fn is_fully_dense(arr: JsArray, heap: &otter_gc::GcHeap) -> bool {
    heap.read_payload(arr, |body| {
        body.exotic.is_null()
            && body.length == body.dense_len()
            && match body.dense_kind() {
                DenseElementKind::Empty | DenseElementKind::PackedDouble => true,
                DenseElementKind::HoleyDouble => false,
                DenseElementKind::Tagged => !(0..body.dense_len())
                    .any(|index| body.dense_value(index).is_some_and(Value::is_hole)),
            }
    })
}

/// Pop from the tail. Returns `undefined` for an empty array
/// and for slots that hold the internal hole sentinel.
#[must_use]
pub fn pop(arr: JsArray, heap: &mut otter_gc::GcHeap) -> Value {
    heap.with_payload(arr, |body| {
        if body.length == 0 {
            return Value::undefined();
        }
        let idx = body.length - 1;
        let popped = if idx < body.dense_len() {
            body.dense_value(idx)
        } else {
            body.exotic_opt_mut()
                .and_then(|exotic| exotic.sparse_elements.as_mut())
                .and_then(|sparse| sparse.remove(&idx))
        };
        truncate_array_body_to(body, idx);
        match popped {
            Some(v) if !v.is_hole() => v,
            _ => Value::undefined(),
        }
    })
}

/// §10.1.4 `[[PreventExtensions]]` on the array exotic. Flips the
/// `[[Extensible]]` slot to `false`. Idempotent.
pub fn prevent_extensions(
    arr: JsArray,
    heap: &mut otter_gc::GcHeap,
) -> Result<(), otter_gc::OutOfMemory> {
    let mut arr = arr;
    ensure_exotic(&mut arr, heap, &mut |_| {})?;
    heap.with_payload(arr, |body| {
        body.exotic_mut().extensible = ExtensibleFlag(false);
    });
    Ok(())
}

/// §10.1.3 `[[IsExtensible]]` on the array exotic.
#[must_use]
pub fn is_extensible(arr: JsArray, heap: &otter_gc::GcHeap) -> bool {
    heap.read_payload(arr, |body| body.extensible())
}

/// Read explicit descriptor attributes for an own index.
#[must_use]
pub(crate) fn get_index_flags(
    arr: JsArray,
    heap: &otter_gc::GcHeap,
    key: u32,
) -> Option<PropertyFlags> {
    heap.read_payload(arr, |body| {
        body.property_flags()
            .and_then(|flags| flags.get(&key).copied())
    })
}

/// Store explicit descriptor attributes for an own index.
pub(crate) fn set_index_flags(
    arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    key: u32,
    flags: PropertyFlags,
) -> Result<(), otter_gc::OutOfMemory> {
    let mut arr = arr;
    ensure_exotic(&mut arr, heap, &mut |_| {})?;
    heap.with_payload(arr, |body| {
        let map = body
            .exotic_mut()
            .property_flags
            .get_or_insert_with(HashMap::new);
        map.insert(key, flags);
        body.mark_dirty();
    });
    Ok(())
}

/// Drop explicit indexed attributes, so
/// it reads back with the attributes its kind implies when absent. Never
/// creates the exotic sidecar.
pub(crate) fn clear_index_flags(arr: JsArray, heap: &mut otter_gc::GcHeap, key: u32) {
    heap.with_payload(arr, |body| {
        if let Some(exotic) = body.exotic_opt_mut()
            && let Some(flags) = exotic.property_flags.as_mut()
            && flags.remove(&key).is_some()
        {
            if flags.is_empty() {
                exotic.property_flags = None;
            }
            body.mark_dirty();
        }
    });
}

/// Complete an ordinary `[[Set]]` that creates index `idx` on a plain dense
/// array: the `CreateDataProperty` of a writable, enumerable, configurable
/// element, growing dense storage when needed.
///
/// The caller must have proved that no prototype of the array can observe
/// the index (the array-index accessor protector is intact). A plain array has
/// no sidecar, so it is extensible, its `length` is writable and its prototype
/// is the realm's `%Array.prototype%`. Returns `Ok(false)` without effect for
/// any other array, or when the index belongs in sparse storage.
///
/// # Errors
///
/// Returns [`otter_gc::OutOfMemory`] if growing dense storage would exceed
/// the configured heap cap.
pub(crate) fn create_plain_dense_element(
    arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    idx: usize,
    value: Value,
) -> Result<bool, otter_gc::OutOfMemory> {
    if !heap.read_payload(arr, |body| body.exotic.is_null()) || should_store_sparse(arr, heap, idx)
    {
        return Ok(false);
    }
    set_index_value(arr, heap, idx, value, false)?;
    Ok(true)
}

/// Whether any own indexed accessor needs protocol dispatch.
#[must_use]
pub fn has_index_accessors(arr: JsArray, heap: &otter_gc::GcHeap) -> bool {
    heap.read_payload(arr, |body| body.accessors().is_some())
}

/// Look up an accessor descriptor previously installed via
/// [`set_index_accessor`]. Returns `Some((getter, setter))` when an entry
/// exists; either slot may be `None`.
#[must_use]
pub fn get_index_accessor(
    arr: JsArray,
    heap: &otter_gc::GcHeap,
    key: u32,
) -> Option<(Option<Value>, Option<Value>)> {
    heap.read_payload(arr, |body| {
        body.accessors().and_then(|m| m.get(&key).cloned())
    })
}

/// Install an indexed accessor, clearing the underlying own data slot.
pub(crate) fn set_index_accessor(
    mut arr: JsArray,
    heap: &mut GcHeap,
    index: u32,
    getter: Option<Value>,
    setter: Option<Value>,
) -> Result<(), otter_gc::OutOfMemory> {
    let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
        if let Some(value) = &getter {
            value.trace_value_slots(visitor);
        }
        if let Some(value) = &setter {
            value.trace_value_slots(visitor);
        }
    };
    ensure_exotic(&mut arr, heap, &mut roots)?;
    heap.with_payload(arr, |body| {
        body.exotic_mut()
            .accessors
            .get_or_insert_with(HashMap::new)
            .insert(index, (getter, setter));
        let idx = index as usize;
        if body.dense_value(idx).is_some() {
            body.write_dense_value(idx, Value::hole());
        }
        if let Some(exotic) = body.exotic_opt_mut()
            && let Some(sparse) = exotic.sparse_elements.as_mut()
        {
            sparse.remove(&idx);
        }
        body.mark_dirty();
    });
    if let Some(value) = &getter {
        record_exotic_array_write(heap, arr, value);
    }
    if let Some(value) = &setter {
        record_exotic_array_write(heap, arr, value);
    }
    Ok(())
}

/// Remove an indexed accessor without creating rare state.
pub(crate) fn delete_index_accessor(arr: JsArray, heap: &mut GcHeap, index: u32) -> bool {
    heap.with_payload(arr, |body| {
        let removed = body
            .exotic_opt_mut()
            .and_then(|e| e.accessors.as_mut())
            .is_some_and(|m| m.remove(&index).is_some());
        if removed {
            if let Some(e) = body.exotic_opt_mut()
                && e.accessors.as_ref().is_some_and(HashMap::is_empty)
            {
                e.accessors = None;
            }
            body.mark_dirty();
        }
        removed
    })
}

/// Delete an own index after proving its descriptor configurable.
pub(crate) fn delete_index_property(arr: JsArray, heap: &mut GcHeap, index: u32) -> bool {
    if !can_delete_index_property(arr, heap, index) {
        return false;
    }
    heap.with_payload(arr, |body| delete_array_body_index(body, index as usize));
    true
}

/// Clamp indexed descriptors and length; the semantic owner also clamps the expando.
pub(crate) fn set_indexed_integrity_level(
    mut arr: JsArray,
    heap: &mut GcHeap,
    frozen: bool,
) -> Result<(), otter_gc::OutOfMemory> {
    let indices = own_indices(arr, heap);
    ensure_exotic(&mut arr, heap, &mut |_| {})?;
    heap.with_payload(arr, |body| {
        let exotic = body.exotic_mut();
        exotic.extensible = ExtensibleFlag(false);
        exotic.length_nonwritable |= frozen;
        let flags = exotic.property_flags.get_or_insert_with(HashMap::new);
        for index in indices {
            let entry = flags
                .entry(index)
                .or_insert_with(|| PropertyFlags::new(true, true, true));
            *entry = PropertyFlags::new(!frozen && entry.writable(), entry.enumerable(), false);
        }
        body.mark_dirty();
    });
    Ok(())
}

/// Test indexed descriptors and length independently of the ordinary expando.
pub(crate) fn test_indexed_integrity_level(arr: JsArray, heap: &GcHeap, frozen: bool) -> bool {
    !is_extensible(arr, heap)
        && (!frozen || !length_writable(arr, heap))
        && own_indices(arr, heap).into_iter().all(|index| {
            let flags = get_index_flags(arr, heap, index)
                .unwrap_or_else(|| PropertyFlags::new(true, true, true));
            !flags.configurable()
                && (!frozen || get_index_accessor(arr, heap, index).is_some() || !flags.writable())
        })
}

pub(crate) fn can_write_index_property(arr: JsArray, heap: &otter_gc::GcHeap, key: u32) -> bool {
    heap.read_payload(arr, |body| {
        if body.accessors().is_some_and(|m| m.contains_key(&key)) {
            return false;
        }
        body.property_flags()
            .and_then(|flags| flags.get(&key))
            .is_none_or(|flags| flags.writable())
    })
}

fn can_delete_index_property(arr: JsArray, heap: &otter_gc::GcHeap, key: u32) -> bool {
    heap.read_payload(arr, |body| {
        body.property_flags()
            .and_then(|flags| flags.get(&key))
            .is_none_or(|flags| flags.configurable())
    })
}

/// Read-only access to dense elements for call sites that need to
/// derive an aggregate result without exposing the body borrow.
pub fn with_elements<R>(arr: JsArray, heap: &otter_gc::GcHeap, f: impl FnOnce(&[Value]) -> R) -> R {
    heap.read_payload(arr, |body| {
        let Some(slab) = elements::body_of(body.slab) else {
            return f(&[]);
        };
        // SAFETY: the array body keeps the slab reachable for the duration of
        // this payload borrow.
        if let Some(values) = unsafe { (*slab).tagged_slice() } {
            f(values)
        } else {
            // Cold borrowed-slice consumers materialize the numeric prefix;
            // exposing raw `f64` words as `Value` would violate the API.
            let values = unsafe { (*slab).values_vec() };
            f(&values)
        }
    })
}

/// Feed a plain array's elements, in order, to `f`.
///
/// `None` when the array has exotic state, an indexed accessor, a hole, or
/// a length past its dense prefix: then reading `length` and every index is
/// not the whole `CreateListFromArrayLike` answer.
pub(crate) fn with_plain_dense_values<R>(
    arr: JsArray,
    heap: &otter_gc::GcHeap,
    f: impl FnOnce(&mut dyn Iterator<Item = Value>) -> R,
) -> Option<R> {
    heap.read_payload(arr, |body| {
        let count = body.dense_len();
        if !body.exotic.is_null() || body.accessors().is_some() || body.length != count {
            return None;
        }
        let Some(slab) = elements::body_of(body.slab) else {
            return (count == 0).then(|| f(&mut std::iter::empty()));
        };
        // SAFETY: the array body keeps the slab reachable for this borrow.
        let slab = unsafe { &*slab };
        if let Some(values) = slab.tagged_slice() {
            let values = values.get(..count)?;
            if values.iter().any(|value| value.is_hole()) {
                return None;
            }
            return Some(f(&mut values.iter().copied()));
        }
        if (0..count).any(|index| slab.get(index).is_none_or(Value::is_hole)) {
            return None;
        }
        Some(f(
            &mut (0..count).map(|index| slab.get(index).unwrap_or(Value::UNDEFINED))
        ))
    })
}

/// Clone a proven plain dense prefix for builtin fast paths.
///
/// Returns `None` if any observable array exotic state, sparse storage,
/// length/dense mismatch, or hole would make a raw element walk differ from
/// ordinary `HasProperty` / `Get` semantics.
pub(crate) fn plain_dense_prefix_values(
    arr: JsArray,
    heap: &otter_gc::GcHeap,
    len: usize,
) -> Option<Vec<Value>> {
    heap.read_payload(arr, |body| {
        if !body.exotic.is_null() || body.length != len || body.dense_len() < len {
            return None;
        }
        (0..len)
            .map(|index| body.dense_value(index).filter(|value| !value.is_hole()))
            .collect()
    })
}

/// Append `arr`'s elements to `out` when it is a plain dense array (no
/// sidecar, no holes, length equal to its dense prefix): exactly what
/// CreateListFromArrayLike or IteratorToList over it would read. `false`
/// leaves `out` untouched.
pub(crate) fn copy_plain_dense(
    arr: JsArray,
    heap: &otter_gc::GcHeap,
    out: &mut smallvec::SmallVec<[Value; 8]>,
) -> bool {
    heap.read_payload(arr, |body| {
        let len = body.dense_len();
        if !body.exotic.is_null() || body.length != len {
            return false;
        }
        let start = out.len();
        out.reserve(len);
        for index in 0..len {
            match body.dense_value(index) {
                Some(value) if !value.is_hole() => out.push(value),
                _ => {
                    out.truncate(start);
                    return false;
                }
            }
        }
        true
    })
}

/// Append `src`'s elements to `dst` when `src` is a plain dense array (no
/// sidecar, no holes, nothing past its dense prefix): one reservation and no
/// intermediate list. `Ok(false)` leaves both untouched.
pub(crate) fn append_plain_dense(
    mut dst: JsArray,
    mut src: JsArray,
    heap: &mut otter_gc::GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<bool, otter_gc::OutOfMemory> {
    let Some((count, kind)) = heap.read_payload(src, |body| {
        let count = body.dense_len();
        let plain = body.exotic.is_null()
            && body.length == count
            && (0..count).all(|index| body.dense_value(index).is_some_and(|v| !v.is_hole()));
        plain.then(|| (count, body.dense_kind()))
    }) else {
        return Ok(false);
    };
    let start = len(dst, heap);
    let target_len = start.saturating_add(count);
    {
        let src_slot = std::ptr::addr_of_mut!(src);
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            visitor(src_slot.cast::<RawGc>());
        };
        reserve_dense_capacity(&mut dst, heap, target_len, kind, &mut reserve_roots)?;
    }
    const CHUNK: usize = 32;
    let mut copied = 0;
    while copied < count {
        let n = (count - copied).min(CHUNK);
        let mut chunk = [Value::undefined(); CHUNK];
        heap.read_payload(src, |body| {
            for (offset, slot) in chunk[..n].iter_mut().enumerate() {
                *slot = body.dense_value(copied + offset).expect("plain dense slot");
            }
        });
        heap.with_payload(dst, |body| {
            body.refresh_element_cache();
            while body.dense_len() < start {
                body.push_dense(Value::hole());
            }
            for value in &chunk[..n] {
                body.push_dense(*value);
            }
        });
        for value in &chunk[..n] {
            record_array_write(heap, dst, value);
        }
        copied += n;
    }
    heap.with_payload(dst, |body| {
        body.length = target_len;
        body.mark_dirty();
    });
    Ok(true)
}

/// Write a caller-proven plain dense range with per-element values.
///
/// The caller owns the same spec checks as [`fill_dense_range_with_roots`].
pub(crate) fn write_dense_range_with_roots(
    mut arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    start: usize,
    values: &[Value],
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    if values.is_empty() {
        return Ok(());
    }
    let end = start.saturating_add(values.len());
    {
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            for value in values {
                value.trace_value_slots(visitor);
            }
        };
        reserve_dense_capacity(
            &mut arr,
            heap,
            end,
            initial_dense_kind(values),
            &mut reserve_roots,
        )?;
    }
    heap.with_payload(arr, |body| {
        body.refresh_element_cache();
        while body.dense_len() < start {
            body.push_dense(Value::hole());
        }
        let existing_end = end.min(body.dense_len());
        for (index, value) in values
            .iter()
            .copied()
            .take(existing_end.saturating_sub(start))
            .enumerate()
        {
            body.write_dense_value(start + index, value);
        }
        for value in &values[existing_end.saturating_sub(start)..] {
            body.push_dense(*value);
        }
        body.length = body.length.max(end);
        body.mark_dirty();
    });
    for value in values {
        record_array_write(heap, arr, value);
    }
    Ok(())
}

/// Crate-internal mutable access to the fixed dense element slice.
///
/// The helper conservatively fires write barriers for every
/// GC-bearing element left in the array after the mutation. This keeps
/// fixed-length rewrites from duplicating barrier bookkeeping. Exposing a
/// slice instead of the backing slab makes growth impossible: allocation and
/// the JIT-visible dense cache must stay owned by this module's rooted
/// construction and mutation APIs.
pub(crate) fn with_elements_rewrite<R>(
    arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    f: impl FnOnce(&mut [Value]) -> R,
) -> R {
    let (out, children) = heap.with_payload(arr, |body| {
        let (out, children) = if let Some(slab) = body.slab_body_mut() {
            slab.convert_to_tagged();
            let values = slab.tagged_slice_mut().expect("converted tagged slab");
            let out = f(values);
            let children: SmallVec<[Value; 8]> = values.iter().copied().collect();
            (out, children)
        } else {
            let out = f(&mut []);
            (out, SmallVec::new())
        };
        body.refresh_element_cache();
        body.mark_dirty();
        (out, children)
    });
    for child in children {
        record_array_write(heap, arr, &child);
    }
    out
}

/// Identity comparison.
#[must_use]
pub fn ptr_eq(a: JsArray, b: JsArray) -> bool {
    a == b
}

/// Return a clone of the verbatim source-text bytes captured from
/// `JSON.parse` iff the array still matches that snapshot — i.e.
/// `source_bytes` is set, the body has not been mutated, and every
/// element is a primitive whose textual form cannot have drifted
/// from the captured render (numbers, strings, booleans, null).
///
/// Nested arrays / objects mutate independently of their parent, so
/// the parent's `source_bytes` would render stale data after such a
/// mutation; we therefore disqualify the fast path here rather than
/// performing a recursive freshness walk.
#[must_use]
pub fn clean_source_bytes(arr: JsArray, heap: &otter_gc::GcHeap) -> Option<Arc<[u8]>> {
    heap.read_payload(arr, |body| {
        let exotic = body.exotic()?;
        if exotic.dirty {
            return None;
        }
        let source = exotic.source_bytes.as_ref()?;
        if !body.dense_values().iter().all(is_render_stable_primitive) {
            return None;
        }
        Some(Arc::clone(source))
    })
}

/// `true` for value variants whose JSON serialisation can be read
/// off the value alone, with no dependency on a separately-mutable
/// nested object or array. Used by [`clean_source_bytes`] to decide
/// whether a captured source slice is still safe to re-emit.
#[inline]
fn is_render_stable_primitive(v: &Value) -> bool {
    v.is_null() || v.is_boolean() || v.is_number() || v.is_string()
}

/// Convert a numeric computed-property key to an Array index.
///
/// ECMA-262 array indices are uint32 values except `2^32 - 1`.
/// The VM's small-integer fast path only covers `i32`, so this helper
/// accepts integral `Double` values as well.
#[must_use]
pub fn index_from_number(n: NumberValue) -> Option<usize> {
    let raw = n.as_f64();
    if !raw.is_finite() || raw < 0.0 || raw.fract() != 0.0 || raw >= u32::MAX as f64 {
        return None;
    }
    Some(raw as usize)
}

/// Stable identity token for legacy address-keyed tables.
#[must_use]
pub fn identity_addr(arr: JsArray) -> *const () {
    (arr.offset() as usize) as *const ()
}

/// Remember an element write against the body that actually holds it.
///
/// Dense elements live in a separate generational slab. Its slots own the
/// element edges, so barriers remember the slab as well as the array. The
/// array also owns the slab/sidecar handles and any exotic-state edges.
fn record_array_write<V>(heap: &mut GcHeap, arr: JsArray, value: &V)
where
    V: otter_gc::GcStore + ?Sized,
{
    heap.record_write(arr, value);
    let slab = heap.read_payload(arr, |body| body.slab);
    if !slab.is_null() {
        heap.record_write(slab, value);
    }
}

/// Remember every value a freshly filled slab holds.
///
/// Construction and growth copy values in behind the mutator's back, so
/// the edges they create need recording in one pass afterwards.
fn record_slab_contents(heap: &mut GcHeap, slab: elements::ElementSlabHandle, len: usize) {
    if slab.is_null() || elements::kind_of(slab) != DenseElementKind::Tagged {
        return;
    }
    let body = elements::body_of(slab).expect("live element slab");
    for index in 0..len {
        // SAFETY: `index < len`, the tagged prefix the caller just wrote.
        let value = unsafe { (*body).get(index).expect("live tagged slot") };
        heap.record_write(slab, &value);
    }
}

/// Make room for `target_len` dense elements, growing the slab if the
/// current one is too small.
///
/// Growth is the array's one allocation point on the element-store path
/// and is deliberately separate from the store: a store runs inside a
/// payload borrow with no heap available, and allocating there could move
/// the array being written. So callers reserve first — which roots the
/// array across the allocation — and then mutate.
///
/// Capacity doubles, so a run of appends pays for growth a logarithmic
/// number of times.
fn reserve_dense_capacity(
    arr: &mut JsArray,
    heap: &mut otter_gc::GcHeap,
    target_len: usize,
    preferred_kind: DenseElementKind,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    let (current_capacity, live_len, current_kind) = heap.read_payload(*arr, |body| {
        (body.dense_capacity(), body.dense_len(), body.dense_kind())
    });
    if target_len <= current_capacity {
        return Ok(());
    }
    let grown = target_len.max(current_capacity.saturating_mul(2)).max(4);
    let target_kind = match (current_kind, preferred_kind) {
        (DenseElementKind::Tagged, _) | (_, DenseElementKind::Tagged) => DenseElementKind::Tagged,
        (DenseElementKind::Empty, preferred) => preferred,
        (current, _) => current,
    };
    debug_assert_ne!(target_kind, DenseElementKind::Empty);

    // The array roots the outgoing slab, and the slab roots every value in
    // it, so rooting the array is enough to carry the elements being
    // copied across this allocation.
    let owner_slot = std::ptr::addr_of_mut!(*arr);
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        visitor(owner_slot.cast::<RawGc>());
    };
    let slab = elements::alloc_element_slab(
        heap,
        grown,
        target_kind,
        otter_gc::SitePlacement::Young(0),
        &mut visit,
    )?;

    let owner = *arr;
    heap.with_payload(owner, |body| {
        let old_slab = elements::body_of(body.slab);
        let new_slab = elements::body_of(slab).expect("fresh element slab");
        for index in 0..live_len {
            // SAFETY: both handles remain live through the owner payload;
            // `index` lies in the old prefix and the new capacity is `grown`.
            let value = unsafe {
                (*old_slab.expect("non-empty prefix has a slab"))
                    .get(index)
                    .expect("live dense slot")
            };
            unsafe { (*new_slab).set(index, value) };
        }
        // SAFETY: every word in the copied prefix is now initialized.
        unsafe { (*new_slab).set_len(live_len) };
        body.adopt_slab(slab, live_len);
        true
    });
    // The slab handle was installed by a raw payload write, and the values
    // were copied in behind the mutator's back, so record both edges the
    // barrier would have.
    heap.record_write(owner, &slab);
    record_slab_contents(heap, slab, live_len);
    Ok(())
}

fn should_store_sparse(arr: JsArray, heap: &otter_gc::GcHeap, idx: usize) -> bool {
    const MAX_DENSE_GAP: usize = 1024;
    const MAX_DENSE_INDEX: usize = 1 << 20;

    heap.read_payload(arr, |body| {
        idx >= MAX_DENSE_INDEX || idx.saturating_sub(body.dense_len()) > MAX_DENSE_GAP
    })
}

impl ArrayBody {
    /// Iterate over elements.
    pub fn iter(&self) -> impl Iterator<Item = Value> + '_ {
        (0..self.dense_len()).filter_map(|index| self.dense_value(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_heap() -> otter_gc::GcHeap {
        otter_gc::GcHeap::new().expect("gc heap")
    }

    #[test]
    fn literal_constructor() {
        let mut heap = fresh_heap();
        let a = from_elements_old_for_fixture(
            &mut heap,
            [Value::boolean(true), Value::null(), Value::boolean(false)],
        )
        .unwrap();
        assert_eq!(len(a, &heap), 3);
        assert_eq!(get(a, &heap, 0), Value::boolean(true));
        assert_eq!(get(a, &heap, 1), Value::null());
        assert_eq!(get(a, &heap, 2), Value::boolean(false));
    }

    #[test]
    fn from_vec_with_roots_adopts_dense_elements() {
        let mut heap = fresh_heap();
        let mut external_visit = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let a = from_values_with_roots(
            &mut heap,
            &mut [
                Value::number_i32(0),
                Value::number_i32(1),
                Value::number_i32(2),
                Value::number_i32(3),
                Value::number_i32(4),
                Value::number_i32(5),
            ],
            otter_gc::SitePlacement::Young(0),
            &mut external_visit,
        )
        .unwrap();

        assert_eq!(len(a, &heap), 6);
        assert_eq!(dense_element_kind(a, &heap), DenseElementKind::PackedDouble);
        assert_eq!(get(a, &heap, 0).as_f64(), Some(0.0));
        assert_eq!(get(a, &heap, 5).as_f64(), Some(5.0));
    }

    #[test]
    fn numeric_holes_fill_to_packed_and_tagged_is_terminal() {
        let mut heap = fresh_heap();
        let a = alloc_array_old_for_fixture(&mut heap).unwrap();
        set_length(a, &mut heap, 70).unwrap();
        assert_eq!(dense_element_kind(a, &heap), DenseElementKind::HoleyDouble);

        for index in 0..70 {
            set(a, &mut heap, index, Value::number_f64(index as f64 + 0.25)).unwrap();
        }
        assert_eq!(dense_element_kind(a, &heap), DenseElementKind::PackedDouble);
        assert_eq!(get(a, &heap, 69).as_f64(), Some(69.25));

        set(a, &mut heap, 7, Value::boolean(true)).unwrap();
        assert_eq!(dense_element_kind(a, &heap), DenseElementKind::Tagged);
        set(a, &mut heap, 7, Value::number_i32(7)).unwrap();
        assert_eq!(dense_element_kind(a, &heap), DenseElementKind::Tagged);
        assert_eq!(get(a, &heap, 7), Value::number_i32(7));
    }

    #[test]
    fn numeric_growth_preserves_values_and_bitmap_transitions() {
        let mut heap = fresh_heap();
        let a = from_elements_old_for_fixture(
            &mut heap,
            [Value::number_f64(-0.0), Value::number_f64(f64::NAN)],
        )
        .unwrap();
        assert_eq!(dense_element_kind(a, &heap), DenseElementKind::PackedDouble);

        set(a, &mut heap, 5, Value::number_f64(5.5)).unwrap();
        assert_eq!(dense_element_kind(a, &heap), DenseElementKind::HoleyDouble);
        assert!(get(a, &heap, 0).as_f64().unwrap().is_sign_negative());
        assert!(get(a, &heap, 1).as_f64().unwrap().is_nan());
        for index in 2..5 {
            set(a, &mut heap, index, Value::number_f64(index as f64)).unwrap();
        }
        assert_eq!(dense_element_kind(a, &heap), DenseElementKind::PackedDouble);
        assert_eq!(get(a, &heap, 5).as_f64(), Some(5.5));

        assert!(delete_index_property(a, &mut heap, 3));
        assert_eq!(dense_element_kind(a, &heap), DenseElementKind::HoleyDouble);
        assert!(!has_own_element(a, &heap, 3));
        set(a, &mut heap, 3, Value::number_i32(3)).unwrap();
        assert_eq!(dense_element_kind(a, &heap), DenseElementKind::PackedDouble);
    }

    #[test]
    fn numeric_truncate_republishes_dense_cache() {
        let mut heap = fresh_heap();
        let a = from_elements_old_for_fixture(
            &mut heap,
            [Value::number_f64(1.25), Value::number_f64(2.5)],
        )
        .unwrap();

        set_length(a, &mut heap, 5).unwrap();
        assert_eq!(dense_element_kind(a, &heap), DenseElementKind::HoleyDouble);
        set_length(a, &mut heap, 2).unwrap();
        assert_eq!(dense_element_kind(a, &heap), DenseElementKind::PackedDouble);
        heap.read_payload(a, |body| {
            assert_eq!(body.dense_len(), 2);
            assert_eq!(elements::len_of(body.slab), 2);
            assert_eq!(body.elements_ptr.get(), elements::data_base(body.slab));
            assert_eq!(body.dense_capacity(), elements::capacity_of(body.slab));
            assert_eq!(body.dense_kind(), elements::kind_of(body.slab));
        });

        assert_eq!(push(a, &mut heap, Value::number_f64(3.75)).unwrap(), 3);
        assert_eq!(get(a, &heap, 2).as_f64(), Some(3.75));
    }

    #[test]
    fn out_of_range_read_is_undefined() {
        let mut heap = fresh_heap();
        let a = alloc_array_old_for_fixture(&mut heap).unwrap();
        assert_eq!(get(a, &heap, 0), Value::undefined());
    }

    #[test]
    fn out_of_range_write_extends_with_holes() {
        let mut heap = fresh_heap();
        let a = alloc_array_old_for_fixture(&mut heap).unwrap();
        set(a, &mut heap, 2, Value::boolean(true)).unwrap();
        assert_eq!(len(a, &heap), 3);
        // Public reads observe `Value::undefined()` for absent slots,
        // even though the body stores `hole` internally.
        assert_eq!(get(a, &heap, 0), Value::undefined());
        assert_eq!(get(a, &heap, 1), Value::undefined());
        assert_eq!(get(a, &heap, 2), Value::boolean(true));
        // `has_own_element` distinguishes the two: holes report `false`,
        // explicit values report `true`.
        assert!(!has_own_element(a, &heap, 0));
        assert!(!has_own_element(a, &heap, 1));
        assert!(has_own_element(a, &heap, 2));
        // Out-of-range index is also absent.
        assert!(!has_own_element(a, &heap, 99));
    }

    #[test]
    fn length_assignment_preserves_non_configurable_index() {
        let mut heap = fresh_heap();
        let a = from_elements_old_for_fixture(
            &mut heap,
            [
                Value::number_i32(0),
                Value::number_i32(1),
                Value::number_i32(2),
                Value::number_i32(3),
            ],
        )
        .unwrap();
        set_index_accessor(a, &mut heap, 2, Some(Value::undefined()), None).unwrap();
        set_index_flags(a, &mut heap, 2, PropertyFlags::new(false, false, false)).unwrap();

        assert!(!set_length_checked(a, &mut heap, 2).unwrap());

        assert_eq!(len(a, &heap), 3);
        assert!(get_index_accessor(a, &heap, 2).is_some());
        assert_eq!(
            get_index_flags(a, &heap, 2),
            Some(PropertyFlags::new(false, false, false))
        );
        assert!(!has_own_element(a, &heap, 3));
    }

    #[test]
    fn explicit_undefined_distinguished_from_hole() {
        let mut heap = fresh_heap();
        let a = from_elements_old_for_fixture(&mut heap, [Value::undefined()]).unwrap();
        // Explicit undefined is a real own element.
        assert!(has_own_element(a, &heap, 0));
        assert_eq!(get(a, &heap, 0), Value::undefined());
    }

    #[test]
    fn hole_does_not_escape_via_pop() {
        let mut heap = fresh_heap();
        let a = alloc_array_old_for_fixture(&mut heap).unwrap();
        set(a, &mut heap, 1, Value::boolean(true)).unwrap();
        // Tail is the explicit value.
        assert_eq!(pop(a, &mut heap), Value::boolean(true));
        // Next pop pulls the leading hole — must surface as
        // `undefined`, never as the internal sentinel.
        assert_eq!(pop(a, &mut heap), Value::undefined());
        assert!(is_empty(a, &heap));
    }

    #[test]
    fn indexed_presence_skips_holes() {
        let mut heap = fresh_heap();
        let a = alloc_array_old_for_fixture(&mut heap).unwrap();
        set(a, &mut heap, 2, Value::boolean(true)).unwrap();
        // Hole index — own-property lookup returns `None` so
        // callers can fall back to the prototype chain.
        assert!(!has_own_element(a, &heap, 0));
        // Filled index — own-property lookup returns the value.
        assert!(has_own_element(a, &heap, 2));
        assert_eq!(get(a, &heap, 2), Value::boolean(true));
    }

    #[test]
    fn push_and_pop() {
        let mut heap = fresh_heap();
        let a = alloc_array_old_for_fixture(&mut heap).unwrap();
        assert_eq!(push(a, &mut heap, Value::boolean(true)).unwrap(), 1);
        assert_eq!(push(a, &mut heap, Value::null()).unwrap(), 2);
        assert_eq!(pop(a, &mut heap), Value::null());
        assert_eq!(pop(a, &mut heap), Value::boolean(true));
        assert_eq!(pop(a, &mut heap), Value::undefined());
        assert!(is_empty(a, &heap));
    }

    #[test]
    fn clean_source_bytes_fast_path_for_unmutated_primitive_array() {
        let mut heap = fresh_heap();
        let bytes: Arc<[u8]> = Arc::from(&b"[1,2,3]"[..]);
        let a = from_elements_with_source_old_for_fixture(
            &mut heap,
            [
                Value::number(NumberValue::from_i32(1)),
                Value::number(NumberValue::from_i32(2)),
                Value::number(NumberValue::from_i32(3)),
            ],
            Arc::clone(&bytes),
        )
        .unwrap();
        // Fresh, unmutated, all primitives → fast path applies.
        let snapshot = clean_source_bytes(a, &heap).expect("fast path eligible");
        assert_eq!(&*snapshot, b"[1,2,3]");
    }

    #[test]
    fn clean_source_bytes_disqualified_after_mutation() {
        let mut heap = fresh_heap();
        let bytes: Arc<[u8]> = Arc::from(&b"[1,2,3]"[..]);
        let a = from_elements_with_source_old_for_fixture(
            &mut heap,
            [
                Value::number(NumberValue::from_i32(1)),
                Value::number(NumberValue::from_i32(2)),
                Value::number(NumberValue::from_i32(3)),
            ],
            Arc::clone(&bytes),
        )
        .unwrap();
        push(a, &mut heap, Value::number(NumberValue::from_i32(99))).unwrap();
        assert!(clean_source_bytes(a, &heap).is_none());
    }

    #[test]
    fn clean_source_bytes_disqualified_when_holding_compound_element() {
        let mut heap = fresh_heap();
        // An array containing a nested array is *not* eligible for
        // the fast path even when its own dirty bit is clear, because
        // the nested array can mutate independently and would render
        // the captured `[…]` slice stale.
        let inner = alloc_array_old_for_fixture(&mut heap).unwrap();
        let bytes: Arc<[u8]> = Arc::from(&b"[[]]"[..]);
        let outer = from_elements_with_source_old_for_fixture(
            &mut heap,
            [Value::array(inner)],
            Arc::clone(&bytes),
        )
        .unwrap();
        assert!(clean_source_bytes(outer, &heap).is_none());
    }

    #[test]
    fn copying_handle_shares_storage() {
        let mut heap = fresh_heap();
        let a = alloc_array_old_for_fixture(&mut heap).unwrap();
        let b = a;
        push(a, &mut heap, Value::boolean(true)).unwrap();
        assert!(ptr_eq(a, b));
        assert_eq!(get(b, &heap, 0), Value::boolean(true));
    }
}
