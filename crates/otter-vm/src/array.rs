//! JavaScript array value with dense and sparse indexed storage.
//!
//! Arrays live on the page-based tracing GC. The public handle is a compressed
//! [`otter_gc::Gc<ArrayBody>`]; every read or mutation takes an
//! explicit [`otter_gc::GcHeap`] reference so no thread-local heap
//! lookup can hide a safepoint.
//!
//! # Invariants
//!
//! - Low, contiguous indices live in `elements`.
//! - Large sparse indices live in `sparse_elements` so Array-index
//!   semantics do not force host-sized dense allocations.
//! - Missing-index reads return `undefined`.
//! - Element growth goes through helpers that reserve off-slot dense
//!   `Vec` capacity against the heap cap before resizing.
//! - Rare array state (sparse/named/accessor/symbol properties,
//!   descriptor flags, captured JSON source text, and per-instance
//!   prototype overrides) lives behind one sidecar so plain dense
//!   arrays keep a small hot body.
//!
//! # See also
//!
//! - <https://tc39.es/ecma262/#sec-array-exotic-objects>
//! - [GC API](../../../docs/book/src/engine/gc-api.md)

use std::collections::{BTreeSet, HashMap};
use std::mem;
use std::sync::Arc;

use smallvec::SmallVec;

use crate::Value;
use crate::number::NumberValue;
use crate::object::PropertyFlags;
use otter_gc::GcHeap;
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::{RawGc, SlotVisitor};

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`ArrayBody`].
///
/// Distinct from task-76 upvalues (`0x10`) and task-77 objects
/// (`0x11`).
pub const ARRAY_BODY_TYPE_TAG: u8 = 0x12;

/// Heap-shared array handle.
pub type JsArray = otter_gc::Gc<ArrayBody>;

/// GC-allocated storage backing every [`JsArray`] handle.
#[derive(Debug, Default, otter_macros::Pelt)]
#[pelt(tag = ARRAY_BODY_TYPE_TAG)]
pub struct ArrayBody {
    /// Dense element storage. Crate-internal callers must go through
    /// this module's helpers so growth is heap-accounted.
    ///
    /// A plain `Vec<Value>` (not `SmallVec`) so the baseline JIT can read
    /// the backing data pointer and length at a stable, probe-discovered
    /// layout — the same mechanism it uses for typed-array buffers — and
    /// inline dense element loads without re-entering the interpreter.
    pub(crate) elements: Vec<Value>,
    /// Logical `length` property. This may be larger than dense
    /// storage when `length` is assigned directly or when sparse
    /// elements are written.
    #[pelt(skip)]
    pub(crate) length: usize,
    /// Lazily-allocated rare/exotic array state. `None` for ordinary dense
    /// arrays with default extensibility and no named/sparse/symbol/accessor
    /// baggage, so the GC cell only carries the JIT-visible dense storage and
    /// logical length in the common case.
    #[pelt(via = trace_array_exotic_slots)]
    pub(crate) exotic: Option<Box<ArrayExoticSlots>>,
}

/// Cold sidecar for Array exotic features that are absent from ordinary dense
/// arrays.
#[derive(Debug, Default)]
pub(crate) struct ArrayExoticSlots {
    /// Sparse array-indexed own elements.
    ///
    /// This is intentionally separate from string-keyed
    /// `named_properties`: array indices have different `length`
    /// semantics in ECMA-262, but storing huge holes densely would
    /// violate the task-84 survivability gate.
    sparse_elements: Option<HashMap<usize, Value>>,
    /// Optional non-index string-keyed own properties.
    named_properties: Option<HashMap<String, Value>>,
    /// Accessor descriptors installed via
    /// `Object.defineProperty` on the array. Keyed by string key
    /// (covers both indexed and named keys). `(getter, setter)` —
    /// either may be `None`. Indexed accessors override the dense /
    /// sparse element value for that slot; named accessors override
    /// the `named_properties` data entry. Spec: §10.4.2.1
    /// ArrayExoticObject [[DefineOwnProperty]].
    accessors: Option<HashMap<String, (Option<Value>, Option<Value>)>>,
    /// Descriptor flags for properties installed through
    /// `Object.defineProperty`. Missing entries use the ordinary
    /// array defaults for data properties.
    property_flags: Option<HashMap<String, PropertyFlags>>,
    /// Symbol-keyed own properties. Stored as a vector of
    /// `(JsSymbol, Value)` pairs (mirroring `JsObject::symbol_props`)
    /// because `JsSymbol` is identity-based — `ptr_eq` is the
    /// authoritative comparator. Typical arrays have zero entries,
    /// so the `Option` keeps the inline footprint at one word.
    symbol_properties: Option<Vec<(crate::symbol::JsSymbol, Value)>>,
    /// Symbol-keyed accessor descriptors installed via
    /// `Object.defineProperty(arr, sym, { get, set })`. Kept separate
    /// from `symbol_properties` (which is data-only); a given symbol is
    /// in exactly one table. `(getter, setter)` — either may be `None`.
    /// Spec: §10.4.2.1 ArrayExoticObject [[DefineOwnProperty]].
    symbol_accessors: Option<Vec<(crate::symbol::JsSymbol, (Option<Value>, Option<Value>))>>,
    /// Verbatim slice of input text captured by `JSON.parse` for the
    /// lazy stringify memcpy fast-path. `Some` only when the array
    /// originated from `JSON.parse`; the slice spans the closing
    /// brackets `[…]` exactly.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-json.stringify> §25.5.2
    source_bytes: Option<Arc<[u8]>>,
    /// `true` once the array has been mutated since `source_bytes`
    /// was captured. Always `false` while `source_bytes` is `None`
    /// (no fast path is in play to invalidate).
    dirty: bool,
    /// `[[Extensible]]` internal slot per §10.1.3. Starts `true`
    /// (`Default::default()`); flipped to `false` by
    /// `Object.preventExtensions` / `seal` / `freeze` on the array
    /// exotic. New string-keyed writes against a non-extensible
    /// array are rejected by the foundation OrdinarySet path.
    extensible: ExtensibleFlag,
    /// Per-instance `[[Prototype]]` override for Array exotic
    /// objects constructed through subclassing. Plain arrays leave
    /// this unset and resolve to the realm `%Array.prototype%`.
    prototype_override: Option<Value>,
}

/// Byte offset of [`ArrayBody::elements`] from the body start (after the GC
/// header). `offset_of!`-derived, so it reflects the actual `repr(Rust)`
/// placement; the baseline JIT adds the GC header size and the probed
/// `Vec<Value>` data-pointer / length sub-offsets to read dense elements
/// inline.
pub const ARRAY_BODY_ELEMENTS_OFFSET: usize = std::mem::offset_of!(ArrayBody, elements);
/// Byte offset of [`ArrayBody::length`] from the body start (after the GC
/// header). The baseline JIT reads this for the `arr.length` fast path after
/// guarding the receiver body tag.
pub const ARRAY_BODY_LENGTH_OFFSET: usize = std::mem::offset_of!(ArrayBody, length);

// Keep the JIT-visible dense array shell small and offset-stable: cold array
// state must stay in `ArrayExoticSlots`, not creep back into `ArrayBody`.
const _: () = assert!(std::mem::offset_of!(ArrayBody, elements) == 0);
const _: () = assert!(std::mem::size_of::<ArrayBody>() <= 48);

/// Trace helper for symbol-keyed own properties: only `Value` parts
/// of each `(JsSymbol, Value)` pair carry GC slots — the `JsSymbol`
/// wrapper itself flows through ordinary roots / well-known
/// installation, not through array body trace.
fn trace_array_symbol_properties(
    field: &Option<Vec<(crate::symbol::JsSymbol, Value)>>,
    visitor: &mut SlotVisitor<'_>,
) {
    if let Some(entries) = field {
        for (_sym, value) in entries {
            value.trace_value_slots(visitor);
        }
    }
}

/// Trace helper for symbol-keyed accessor descriptors: only the
/// getter / setter `Value` slots carry GC references.
fn trace_array_symbol_accessors(
    field: &Option<Vec<(crate::symbol::JsSymbol, (Option<Value>, Option<Value>))>>,
    visitor: &mut SlotVisitor<'_>,
) {
    if let Some(entries) = field {
        for (_sym, (getter, setter)) in entries {
            if let Some(g) = getter {
                g.trace_value_slots(visitor);
            }
            if let Some(s) = setter {
                s.trace_value_slots(visitor);
            }
        }
    }
}

fn trace_array_exotic_slots(field: &Option<Box<ArrayExoticSlots>>, visitor: &mut SlotVisitor<'_>) {
    let Some(exotic) = field.as_ref() else {
        return;
    };
    if let Some(sparse) = &exotic.sparse_elements {
        for value in sparse.values() {
            value.trace_value_slots(visitor);
        }
    }
    if let Some(named) = &exotic.named_properties {
        for value in named.values() {
            value.trace_value_slots(visitor);
        }
    }
    if let Some(accessors) = &exotic.accessors {
        for (getter, setter) in accessors.values() {
            if let Some(g) = getter {
                g.trace_value_slots(visitor);
            }
            if let Some(s) = setter {
                s.trace_value_slots(visitor);
            }
        }
    }
    trace_array_symbol_properties(&exotic.symbol_properties, visitor);
    trace_array_symbol_accessors(&exotic.symbol_accessors, visitor);
    if let Some(proto) = exotic.prototype_override {
        proto.trace_value_slots(visitor);
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
        self.exotic.as_deref()
    }

    #[inline]
    fn exotic_mut(&mut self) -> &mut ArrayExoticSlots {
        self.exotic
            .get_or_insert_with(|| Box::new(ArrayExoticSlots::default()))
    }

    #[inline]
    fn sparse_elements(&self) -> Option<&HashMap<usize, Value>> {
        self.exotic().and_then(|e| e.sparse_elements.as_ref())
    }

    #[inline]
    fn named_properties(&self) -> Option<&HashMap<String, Value>> {
        self.exotic().and_then(|e| e.named_properties.as_ref())
    }

    #[inline]
    fn accessors(&self) -> Option<&HashMap<String, (Option<Value>, Option<Value>)>> {
        self.exotic().and_then(|e| e.accessors.as_ref())
    }

    #[inline]
    fn property_flags(&self) -> Option<&HashMap<String, PropertyFlags>> {
        self.exotic().and_then(|e| e.property_flags.as_ref())
    }

    #[inline]
    fn symbol_properties(&self) -> Option<&Vec<(crate::symbol::JsSymbol, Value)>> {
        self.exotic().and_then(|e| e.symbol_properties.as_ref())
    }

    #[inline]
    fn symbol_accessors(
        &self,
    ) -> Option<&Vec<(crate::symbol::JsSymbol, (Option<Value>, Option<Value>))>> {
        self.exotic().and_then(|e| e.symbol_accessors.as_ref())
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
        if let Some(exotic) = self.exotic.as_deref_mut() {
            exotic.dirty = true;
        }
    }
}

/// Read the Array exotic's per-instance `[[Prototype]]` override.
pub(crate) fn prototype_override(arr: JsArray, heap: &GcHeap) -> Option<Value> {
    heap.read_payload(arr, |body| body.prototype_override())
}

/// Set the Array exotic's per-instance `[[Prototype]]` override.
///
/// Spec: §10.4.2 Array exotic objects still have ordinary
/// `[[GetPrototypeOf]]` / `[[SetPrototypeOf]]`; subclassing Array
/// needs a per-object slot rather than a realm-level intrinsic
/// fallback.
///
/// <https://tc39.es/ecma262/#sec-array-exotic-objects>
pub(crate) fn set_prototype_override(arr: JsArray, heap: &mut GcHeap, proto: Option<Value>) {
    let barrier_value = proto;
    heap.with_payload(arr, |body| {
        body.exotic_mut().prototype_override = proto;
    });
    if let Some(value) = &barrier_value {
        heap.record_write(arr, value);
    }
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
    let collected: Vec<Value> = values.into_iter().collect();
    let mut body = ArrayBody::default();
    reserve_elements_for_len(&mut body, heap, collected.len())?;
    body.length = collected.len();
    body.elements.extend(collected);
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
    let collected: Vec<Value> = values.into_iter().collect();
    from_vec_with_roots(heap, collected, external_visit)
}

/// Construct an array from an already materialized element vector through the
/// young-generation allocation path.
///
/// This is the array-literal fast boundary: bytecode/JIT callers have already
/// copied source registers into owned storage, so taking the vector directly
/// avoids a second transient argument container before the GC allocation.
pub(crate) fn from_vec_with_roots(
    heap: &mut GcHeap,
    collected: Vec<Value>,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsArray, otter_gc::OutOfMemory> {
    let body = ArrayBody {
        length: collected.len(),
        elements: collected,
        ..Default::default()
    };
    alloc_body_with_adopted_elements(heap, body, external_visit)
}

/// Allocate an [`ArrayBody`] whose dense element vector has already been
/// materialized and moved into the body.
///
/// The vector's backing store is accounted exactly once. When there is no hard
/// heap cap, the common path reserves those bytes without a safepoint and tries
/// a no-collect young allocation for the array shell. Any miss falls back to
/// the rooted allocator, which traces the pending body and its dense elements.
fn alloc_body_with_adopted_elements(
    heap: &mut GcHeap,
    mut body: ArrayBody,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsArray, otter_gc::OutOfMemory> {
    let element_bytes = spilled_capacity_bytes(body.elements.capacity());
    let can_reserve_without_collect = heap.max_heap_bytes() == 0;
    let elements_reserved = element_bytes == 0
        || (can_reserve_without_collect
            && heap.reserve_bytes_no_collect(element_bytes as u64).is_ok());
    if elements_reserved {
        match heap.try_alloc_no_collect_or_return(body) {
            Ok(array) => return Ok(array),
            Err(returned) => body = returned,
        }
    } else {
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            for value in &body.elements {
                value.trace_value_slots(visitor);
            }
        };
        heap.reserve_bytes_with_roots(element_bytes as u64, &mut reserve_roots)?;
    }

    heap.alloc_with_roots(body, external_visit)
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
    let collected: Vec<Value> = values.into_iter().collect();
    let mut body = ArrayBody {
        length: collected.len(),
        exotic: Some(Box::new(ArrayExoticSlots {
            source_bytes: Some(source_bytes),
            dirty: false,
            ..ArrayExoticSlots::default()
        })),
        ..Default::default()
    };
    reserve_elements_for_len(&mut body, heap, collected.len())?;
    body.elements.extend(collected);
    heap.alloc_old(body)
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
    let collected: Vec<Value> = values.into_iter().collect();
    let body = ArrayBody {
        length: collected.len(),
        elements: collected,
        exotic: Some(Box::new(ArrayExoticSlots {
            source_bytes: Some(source_bytes),
            dirty: false,
            ..ArrayExoticSlots::default()
        })),
    };
    alloc_body_with_adopted_elements(heap, body, external_visit)
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

/// Read element at `idx`. Out-of-range and array-hole slots both
/// return `undefined` per ECMA-262 §10.4.2 OrdinaryGet —
/// internal hole sentinel never escapes the array.
#[must_use]
pub fn get(arr: JsArray, heap: &otter_gc::GcHeap, idx: usize) -> Value {
    heap.read_payload(arr, |body| {
        let raw = body
            .elements
            .get(idx)
            .cloned()
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
        if let Some(slot) = body.elements.get(idx) {
            return !slot.is_hole();
        }
        body.sparse_elements()
            .is_some_and(|sparse| sparse.contains_key(&idx))
    })
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
    arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    idx: usize,
    value: Value,
    enforce_writable: bool,
) -> Result<(), otter_gc::OutOfMemory> {
    // Only the writability gate needs the stringified index; the
    // definition path (`define_index_value`) skips it, so don't pay the
    // per-write `String` allocation there.
    if enforce_writable && !can_write_array_property(arr, heap, &idx.to_string()) {
        return Ok(());
    }
    if !has_own_element(arr, heap, idx) && !is_extensible(arr, heap) {
        return Ok(());
    }
    let barrier_value = value;
    let target_len = idx.saturating_add(1);
    if should_store_sparse(arr, heap, idx) {
        heap.with_payload(arr, |body| {
            let sparse = body
                .exotic_mut()
                .sparse_elements
                .get_or_insert_with(HashMap::new);
            sparse.insert(idx, value);
            body.length = body.length.max(target_len);
            body.mark_dirty();
        });
        heap.record_write(arr, &barrier_value);
        return Ok(());
    }
    reserve_for_target_len(arr, heap, target_len)?;
    heap.with_payload(arr, |body| {
        if idx < body.elements.len() {
            body.elements[idx] = value;
            body.length = body.length.max(target_len);
            body.mark_dirty();
            return;
        }
        body.elements
            .reserve_exact(target_len.saturating_sub(body.elements.len()));
        while body.elements.len() < idx {
            body.elements.push(Value::hole());
        }
        body.elements.push(value);
        body.length = body.length.max(target_len);
        body.mark_dirty();
    });
    heap.record_write(arr, &barrier_value);
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
    let key = idx.to_string();
    if !can_write_array_property(arr, heap, &key) {
        return Ok(());
    }
    if !has_own_element(arr, heap, idx) && !is_extensible(arr, heap) {
        return Ok(());
    }
    let barrier_value = value;
    let target_len = idx.saturating_add(1);
    if should_store_sparse(arr, heap, idx) {
        heap.with_payload(arr, |body| {
            let sparse = body
                .exotic_mut()
                .sparse_elements
                .get_or_insert_with(HashMap::new);
            sparse.insert(idx, value);
            body.length = body.length.max(target_len);
            body.mark_dirty();
        });
        heap.record_write(arr, &barrier_value);
        return Ok(());
    }
    {
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            value.trace_value_slots(visitor);
        };
        reserve_for_target_len_with_roots(&mut arr, heap, target_len, &mut reserve_roots)?;
    }
    heap.with_payload(arr, |body| {
        if idx < body.elements.len() {
            body.elements[idx] = value;
            body.length = body.length.max(target_len);
            body.mark_dirty();
            return;
        }
        body.elements
            .reserve_exact(target_len.saturating_sub(body.elements.len()));
        while body.elements.len() < idx {
            body.elements.push(Value::hole());
        }
        body.elements.push(value);
        body.length = body.length.max(target_len);
        body.mark_dirty();
    });
    heap.record_write(arr, &barrier_value);
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
        if start.saturating_sub(body.elements.len()) > 1024 {
            return false;
        }
        let in_range = |key: &str| {
            crate::object::array_index_property_name(key)
                .and_then(|idx| usize::try_from(idx).ok())
                .is_some_and(|idx| (start..end).contains(&idx))
        };
        if body
            .accessors()
            .is_some_and(|accessors| accessors.keys().any(|key| in_range(key)))
        {
            return false;
        }
        if body
            .property_flags()
            .is_some_and(|flags| flags.keys().any(|key| in_range(key)))
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
        reserve_for_target_len_with_roots(&mut arr, heap, end, &mut reserve_roots)?;
    }
    heap.with_payload(arr, |body| {
        body.elements
            .reserve_exact(end.saturating_sub(body.elements.len()));
        while body.elements.len() < start {
            body.elements.push(Value::hole());
        }
        let existing_end = end.min(body.elements.len());
        for idx in start..existing_end {
            body.elements[idx] = value;
        }
        while body.elements.len() < end {
            body.elements.push(value);
        }
        body.length = body.length.max(end);
        body.mark_dirty();
    });
    heap.record_write(arr, &value);
    Ok(())
}

/// Push to the tail. Returns the new length.
///
/// # Errors
///
/// Returns [`otter_gc::OutOfMemory`] if growing dense storage would
/// exceed the configured heap cap.
pub fn push(
    arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    value: Value,
) -> Result<usize, otter_gc::OutOfMemory> {
    let barrier_value = value;
    let target_len = len(arr, heap).saturating_add(1);
    reserve_for_target_len(arr, heap, target_len)?;
    let new_len = heap.with_payload(arr, |body| {
        body.elements
            .reserve_exact(target_len.saturating_sub(body.elements.len()));
        while body.elements.len() + 1 < target_len {
            body.elements.push(Value::hole());
        }
        body.elements.push(value);
        body.length = target_len;
        body.mark_dirty();
        body.length
    });
    heap.record_write(arr, &barrier_value);
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
    let barrier_value = value;
    let target_len = len(arr, heap).saturating_add(1);
    {
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            value.trace_value_slots(visitor);
        };
        reserve_for_target_len_with_roots(&mut arr, heap, target_len, &mut reserve_roots)?;
    }
    let new_len = heap.with_payload(arr, |body| {
        body.elements
            .reserve_exact(target_len.saturating_sub(body.elements.len()));
        while body.elements.len() + 1 < target_len {
            body.elements.push(Value::hole());
        }
        body.elements.push(value);
        body.length = target_len;
        body.mark_dirty();
        body.length
    });
    heap.record_write(arr, &barrier_value);
    Ok(new_len)
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
    arr: JsArray,
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
            body.elements.truncate(new_len);
            if let Some(exotic) = body.exotic.as_deref_mut()
                && let Some(sparse) = exotic.sparse_elements.as_mut()
            {
                sparse.retain(|k, _| *k < new_len);
                if sparse.is_empty() {
                    exotic.sparse_elements = None;
                }
            }
            if let Some(exotic) = body.exotic.as_deref_mut()
                && let Some(accessors) = exotic.accessors.as_mut()
            {
                accessors.retain(|key, _| !array_index_at_or_above(key, new_len));
                if accessors.is_empty() {
                    exotic.accessors = None;
                }
            }
            if let Some(exotic) = body.exotic.as_deref_mut()
                && let Some(flags) = exotic.property_flags.as_mut()
            {
                flags.retain(|key, _| key == "length" || !array_index_at_or_above(key, new_len));
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
        reserve_for_target_len(arr, heap, new_len)?;
    }
    heap.with_payload(arr, |body| {
        if new_len <= MAX_DENSE_LENGTH_GROWTH {
            body.elements
                .reserve_exact(new_len.saturating_sub(body.elements.len()));
            while body.elements.len() < new_len {
                body.elements.push(Value::hole());
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
        let dense_hi = body.elements.len().min(cur);
        for idx in new_len..dense_hi {
            if !body.elements[idx].is_hole() {
                present.push(idx);
            }
        }
        if let Some(sparse) = body.sparse_elements() {
            present.extend(sparse.keys().copied().filter(|&k| k >= new_len && k < cur));
        }
        if let Some(accessors) = body.accessors() {
            for key in accessors.keys() {
                if let Some(idx) = crate::object::array_index_property_name(key) {
                    let idx = idx as usize;
                    if idx >= new_len && idx < cur {
                        present.push(idx);
                    }
                }
            }
        }
        present.sort_unstable();
        present.dedup();
        for &idx in present.iter().rev() {
            let key = idx.to_string();
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
pub(crate) fn length_flags(arr: JsArray, heap: &otter_gc::GcHeap) -> PropertyFlags {
    get_property_flags(arr, heap, "length")
        .unwrap_or_else(|| PropertyFlags::new(true, false, false))
}

#[must_use]
pub(crate) fn length_writable(arr: JsArray, heap: &otter_gc::GcHeap) -> bool {
    length_flags(arr, heap).writable()
}

pub(crate) fn set_length_writable(arr: JsArray, heap: &mut otter_gc::GcHeap, writable: bool) {
    let flags = length_flags(arr, heap).with_writable(writable);
    set_property_flags(arr, heap, "length", flags);
}

fn delete_array_body_index(body: &mut ArrayBody, idx: usize) {
    if let Some(slot) = body.elements.get_mut(idx) {
        *slot = Value::hole();
    }
    if let Some(exotic) = body.exotic.as_deref_mut()
        && let Some(sparse) = exotic.sparse_elements.as_mut()
    {
        sparse.remove(&idx);
        if sparse.is_empty() {
            exotic.sparse_elements = None;
        }
    }
    let key = idx.to_string();
    if let Some(exotic) = body.exotic.as_deref_mut()
        && let Some(accessors) = exotic.accessors.as_mut()
    {
        accessors.remove(&key);
        if accessors.is_empty() {
            exotic.accessors = None;
        }
    }
    if let Some(exotic) = body.exotic.as_deref_mut()
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
    body.elements.truncate(len);
    if let Some(exotic) = body.exotic.as_deref_mut()
        && let Some(sparse) = exotic.sparse_elements.as_mut()
    {
        sparse.retain(|idx, _| *idx < len);
        if sparse.is_empty() {
            exotic.sparse_elements = None;
        }
    }
    if let Some(exotic) = body.exotic.as_deref_mut()
        && let Some(accessors) = exotic.accessors.as_mut()
    {
        accessors.retain(|key, _| !array_index_at_or_above(key, len));
        if accessors.is_empty() {
            exotic.accessors = None;
        }
    }
    if let Some(exotic) = body.exotic.as_deref_mut()
        && let Some(flags) = exotic.property_flags.as_mut()
    {
        flags.retain(|key, _| key == "length" || !array_index_at_or_above(key, len));
        if flags.is_empty() {
            exotic.property_flags = None;
        }
    }
    body.mark_dirty();
}

fn array_index_at_or_above(key: &str, limit: usize) -> bool {
    crate::object::array_index_property_name(key).is_some_and(|idx| idx as usize >= limit)
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
        let popped = if idx < body.elements.len() {
            body.elements.get(idx).cloned()
        } else {
            body.exotic
                .as_deref_mut()
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
pub fn prevent_extensions(arr: JsArray, heap: &mut otter_gc::GcHeap) {
    heap.with_payload(arr, |body| {
        body.exotic_mut().extensible = ExtensibleFlag(false);
    });
}

/// §7.3.16 SetIntegrityLevel on the array exotic — prevent
/// extensions and clamp every own property's attributes ("sealed":
/// configurable = false; "frozen": data writability off too).
pub fn set_integrity_level(arr: JsArray, heap: &mut otter_gc::GcHeap, frozen: bool) {
    prevent_extensions(arr, heap);
    heap.with_payload(arr, |body| {
        let mut keys: Vec<(String, bool)> = Vec::new();
        for (i, v) in body.elements.iter().enumerate() {
            if !v.is_hole() {
                keys.push((i.to_string(), false));
            }
        }
        if let Some(sparse) = body.sparse_elements() {
            keys.extend(sparse.keys().map(|k| (k.to_string(), false)));
        }
        if let Some(named) = body.named_properties() {
            keys.extend(named.keys().map(|k| (k.clone(), false)));
        }
        keys.push(("length".to_string(), true));
        let flags = body
            .exotic_mut()
            .property_flags
            .get_or_insert_with(HashMap::new);
        for (key, is_length) in keys {
            let entry = flags.entry(key).or_insert_with(|| {
                if is_length {
                    PropertyFlags::new(true, false, false)
                } else {
                    PropertyFlags::new(true, true, true)
                }
            });
            let writable = if frozen { false } else { entry.writable() };
            *entry = PropertyFlags::new(writable, entry.enumerable(), false);
        }
    });
}

/// §7.3.17 TestIntegrityLevel on the array exotic.
#[must_use]
pub fn test_integrity_level(arr: JsArray, heap: &otter_gc::GcHeap, frozen: bool) -> bool {
    if is_extensible(arr, heap) {
        return false;
    }
    heap.read_payload(arr, |body| {
        let mut keys: Vec<String> = Vec::new();
        for (i, v) in body.elements.iter().enumerate() {
            if !v.is_hole() {
                keys.push(i.to_string());
            }
        }
        if let Some(sparse) = body.sparse_elements() {
            keys.extend(sparse.keys().map(|k| k.to_string()));
        }
        if let Some(named) = body.named_properties() {
            keys.extend(named.keys().cloned());
        }
        keys.push("length".to_string());
        keys.iter().all(|key| {
            let entry = body
                .property_flags()
                .and_then(|flags| flags.get(key))
                .copied()
                .unwrap_or_else(|| {
                    if key == "length" {
                        PropertyFlags::new(true, false, false)
                    } else {
                        PropertyFlags::new(true, true, true)
                    }
                });
            !entry.configurable() && (!frozen || !entry.writable())
        })
    })
}

/// Install attribute flags for a named own property (used by
/// template-object construction for the non-enumerable `.raw`).
pub fn set_named_property_flags(
    arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    new_flags: PropertyFlags,
) {
    heap.with_payload(arr, |body| {
        body.exotic_mut()
            .property_flags
            .get_or_insert_with(HashMap::new)
            .insert(key.to_string(), new_flags);
    });
}

/// §10.1.3 `[[IsExtensible]]` on the array exotic.
#[must_use]
pub fn is_extensible(arr: JsArray, heap: &otter_gc::GcHeap) -> bool {
    heap.read_payload(arr, |body| body.extensible())
}

/// Install a symbol-keyed own property on the array exotic body.
/// Replaces the existing slot if the symbol is already present —
/// matching JsObject's symbol-property semantics. Used by the
/// `StoreElement` dispatch and reflective `Object.defineProperty`
/// when the key is a `Symbol`.
pub fn set_symbol_property(
    arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    key: crate::symbol::JsSymbol,
    value: Value,
) {
    let barrier_value = value;
    heap.with_payload(arr, |body| {
        // A symbol is in exactly one table — installing a data value
        // removes any accessor previously held for the same key.
        if let Some(exotic) = body.exotic.as_deref_mut()
            && let Some(accessors) = exotic.symbol_accessors.as_mut()
        {
            accessors.retain(|(k, _)| !k.ptr_eq(key));
            if accessors.is_empty() {
                exotic.symbol_accessors = None;
            }
        }
        let table = body
            .exotic_mut()
            .symbol_properties
            .get_or_insert_with(Vec::new);
        if let Some(slot) = table.iter_mut().find(|(k, _)| k.ptr_eq(key)) {
            slot.1 = value;
        } else {
            table.push((key, value));
        }
        body.mark_dirty();
    });
    heap.record_write(arr, &barrier_value);
}

/// Install a symbol-keyed accessor descriptor, removing any data slot
/// previously held for the same key (a symbol is in one table only).
pub fn set_symbol_accessor(
    arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    key: crate::symbol::JsSymbol,
    getter: Option<Value>,
    setter: Option<Value>,
) {
    heap.with_payload(arr, |body| {
        if let Some(exotic) = body.exotic.as_deref_mut()
            && let Some(table) = exotic.symbol_properties.as_mut()
        {
            table.retain(|(k, _)| !k.ptr_eq(key));
            if table.is_empty() {
                exotic.symbol_properties = None;
            }
        }
        let accessors = body
            .exotic_mut()
            .symbol_accessors
            .get_or_insert_with(Vec::new);
        if let Some(slot) = accessors.iter_mut().find(|(k, _)| k.ptr_eq(key)) {
            slot.1 = (getter, setter);
        } else {
            accessors.push((key, (getter, setter)));
        }
        body.mark_dirty();
    });
    if let Some(g) = &getter {
        heap.record_write(arr, g);
    }
    if let Some(s) = &setter {
        heap.record_write(arr, s);
    }
}

/// Read a symbol-keyed accessor descriptor. Returns `None` when no
/// accessor is installed for `key`.
#[must_use]
pub fn get_symbol_accessor(
    arr: JsArray,
    heap: &otter_gc::GcHeap,
    key: crate::symbol::JsSymbol,
) -> Option<(Option<Value>, Option<Value>)> {
    heap.read_payload(arr, |body| {
        body.symbol_accessors()
            .and_then(|table| table.iter().find(|(k, _)| k.ptr_eq(key)).map(|(_, v)| *v))
    })
}

/// Read a symbol-keyed own property. Returns `None` when the slot
/// is absent.
#[must_use]
pub fn get_symbol_property(
    arr: JsArray,
    heap: &otter_gc::GcHeap,
    key: crate::symbol::JsSymbol,
) -> Option<Value> {
    heap.read_payload(arr, |body| {
        body.symbol_properties()
            .and_then(|table| table.iter().find(|(k, _)| k.ptr_eq(key)).map(|(_, v)| *v))
    })
}

/// Remove a symbol-keyed own property. Returns `true` when the
/// slot was present and removed (matches `OrdinaryDelete`
/// success). Returns `true` when absent (spec step 2: missing →
/// success).
pub fn delete_symbol_property(
    arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    key: crate::symbol::JsSymbol,
) -> bool {
    heap.with_payload(arr, |body| {
        if let Some(exotic) = body.exotic.as_deref_mut()
            && let Some(table) = exotic.symbol_properties.as_mut()
            && let Some(pos) = table.iter().position(|(k, _)| k.ptr_eq(key))
        {
            table.remove(pos);
            if table.is_empty() {
                exotic.symbol_properties = None;
            }
            body.mark_dirty();
        }
        if let Some(exotic) = body.exotic.as_deref_mut()
            && let Some(table) = exotic.symbol_accessors.as_mut()
            && let Some(pos) = table.iter().position(|(k, _)| k.ptr_eq(key))
        {
            table.remove(pos);
            if table.is_empty() {
                exotic.symbol_accessors = None;
            }
            body.mark_dirty();
        }
        true
    })
}

/// Iterate own symbol-keyed property keys in insertion order. Used
/// by `Object.getOwnPropertySymbols(arr)` and the ownKeys ladder.
#[must_use]
pub fn own_symbol_keys(arr: JsArray, heap: &otter_gc::GcHeap) -> Vec<crate::symbol::JsSymbol> {
    heap.read_payload(arr, |body| {
        let mut keys: Vec<crate::symbol::JsSymbol> = body
            .symbol_properties()
            .map_or_else(Vec::new, |t| t.iter().map(|(k, _)| *k).collect());
        if let Some(accessors) = body.symbol_accessors() {
            keys.extend(accessors.iter().map(|(k, _)| *k));
        }
        keys
    })
}

/// Set a string-keyed own property. Numeric strings route into dense
/// indexed storage.
///
/// # Errors
///
/// Returns [`otter_gc::OutOfMemory`] if numeric-index growth would
/// exceed the configured heap cap.
pub fn set_named_property(
    arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    value: Value,
) -> Result<(), otter_gc::OutOfMemory> {
    if key == "length" {
        if !length_writable(arr, heap) {
            return Ok(());
        }
        let number_len =
            crate::number::NumberValue::from_f64(crate::number::to_number_value(&value, heap));
        let new_len = crate::number::bitwise::to_uint32(number_len);
        if (new_len as f64) != number_len.as_f64() {
            return Ok(());
        }
        let _ = set_length_checked(arr, heap, new_len as usize)?;
        return Ok(());
    }
    if let Some(idx) = crate::object::array_index_property_name(key) {
        return set(arr, heap, idx as usize, value);
    }
    if !can_write_array_property(arr, heap, key) {
        return Ok(());
    }
    // §10.4.2 — non-extensible Array exotic rejects fresh keys.
    // Updating an existing key still succeeds (the spec routes
    // through OrdinaryDefineOwnProperty which only fails when the
    // property is absent and the object is non-extensible).
    let absent = heap.read_payload(arr, |body| {
        body.named_properties().is_none_or(|m| !m.contains_key(key))
    });
    if absent && !is_extensible(arr, heap) {
        return Ok(());
    }
    let barrier_value = value;
    heap.with_payload(arr, |body| {
        let map = body
            .exotic_mut()
            .named_properties
            .get_or_insert_with(HashMap::new);
        map.insert(key.to_string(), value);
        body.mark_dirty();
    });
    heap.record_write(arr, &barrier_value);
    Ok(())
}

/// Store a non-index string-keyed data property as part of
/// `[[DefineOwnProperty]]`.
///
/// Unlike assignment, descriptor definition has already validated
/// compatibility with the current descriptor, so this bypasses the
/// `[[Writable]]` check used by [`set_named_property`].
pub(crate) fn define_named_data_property(
    arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    value: Value,
) {
    let barrier_value = value;
    heap.with_payload(arr, |body| {
        let map = body
            .exotic_mut()
            .named_properties
            .get_or_insert_with(HashMap::new);
        map.insert(key.to_string(), value);
        body.mark_dirty();
    });
    heap.record_write(arr, &barrier_value);
}

/// Read descriptor flags installed for a string-keyed array own property.
#[must_use]
pub(crate) fn get_property_flags(
    arr: JsArray,
    heap: &otter_gc::GcHeap,
    key: &str,
) -> Option<PropertyFlags> {
    heap.read_payload(arr, |body| {
        body.property_flags()
            .and_then(|flags| flags.get(key).copied())
    })
}

/// Store descriptor flags for a string-keyed array own property.
pub(crate) fn set_property_flags(
    arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    flags: PropertyFlags,
) {
    heap.with_payload(arr, |body| {
        let map = body
            .exotic_mut()
            .property_flags
            .get_or_insert_with(HashMap::new);
        map.insert(key.to_string(), flags);
        body.mark_dirty();
    });
}

/// Read a string-keyed own property. Numeric strings route to indexed
/// elements; `length` returns the array length.
#[must_use]
pub fn get_named_property(arr: JsArray, heap: &otter_gc::GcHeap, key: &str) -> Option<Value> {
    if key == "length" {
        return Some(Value::number(crate::number::NumberValue::from_f64(
            len(arr, heap) as f64,
        )));
    }
    if let Some(idx) = crate::object::array_index_property_name(key) {
        let idx = idx as usize;
        return heap.read_payload(arr, |body| {
            body.elements
                .get(idx)
                .filter(|v| !v.is_hole())
                .cloned()
                .or_else(|| {
                    body.sparse_elements()
                        .and_then(|sparse| sparse.get(&idx).cloned())
                })
        });
    }
    heap.read_payload(arr, |body| {
        body.named_properties().and_then(|m| m.get(key).cloned())
    })
}

/// Read a non-index string-keyed own data property from the array side table.
///
/// Callers that already handled `"length"` and array-index keys use this to
/// avoid depending on the cold sidecar's storage layout.
#[must_use]
pub(crate) fn get_own_named_data_property(
    arr: JsArray,
    heap: &otter_gc::GcHeap,
    key: &str,
) -> Option<Value> {
    heap.read_payload(arr, |body| {
        body.named_properties().and_then(|m| m.get(key).cloned())
    })
}

/// Install an accessor descriptor on the array at `key`. Used by
/// `Object.defineProperty(arr, key, { get, set, … })`. Indexed
/// accessors and named accessors are both stored here; the read /
/// write paths consult this table before falling back to dense
/// element storage or the `named_properties` side-table.
pub fn set_accessor(
    arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    getter: Option<Value>,
    setter: Option<Value>,
) {
    heap.with_payload(arr, |body| {
        let map = body.exotic_mut().accessors.get_or_insert_with(HashMap::new);
        map.insert(key.to_string(), (getter, setter));
        // Hide the underlying dense / sparse / named data slot so
        // subsequent ordinary reads see the accessor instead of the
        // previous data value.
        if let Some(idx) = crate::object::array_index_property_name(key) {
            let idx = idx as usize;
            if let Some(slot) = body.elements.get_mut(idx) {
                *slot = Value::hole();
            }
            if let Some(exotic) = body.exotic.as_deref_mut()
                && let Some(sparse) = exotic.sparse_elements.as_mut()
            {
                sparse.remove(&idx);
            }
        }
        if let Some(exotic) = body.exotic.as_deref_mut()
            && let Some(named) = exotic.named_properties.as_mut()
        {
            named.remove(key);
        }
        body.mark_dirty();
    });
    if let Some(g) = &getter {
        heap.record_write(arr, g);
    }
    if let Some(s) = &setter {
        heap.record_write(arr, s);
    }
}

/// Cheap probe for the presence of any string-keyed accessor
/// descriptors. Lets per-element hot paths skip the keyed
/// [`get_accessor`] lookup (and its key allocation) for plain arrays.
#[must_use]
pub fn has_accessors(arr: JsArray, heap: &otter_gc::GcHeap) -> bool {
    heap.read_payload(arr, |body| body.accessors().is_some())
}

/// Look up an accessor descriptor previously installed via
/// [`set_accessor`]. Returns `Some((getter, setter))` when an entry
/// exists; either slot may be `None`.
#[must_use]
pub fn get_accessor(
    arr: JsArray,
    heap: &otter_gc::GcHeap,
    key: &str,
) -> Option<(Option<Value>, Option<Value>)> {
    heap.read_payload(arr, |body| {
        body.accessors().and_then(|m| m.get(key).cloned())
    })
}

/// Remove a previously installed accessor descriptor. Returns `true`
/// when an entry existed and was removed.
pub fn delete_accessor(arr: JsArray, heap: &mut otter_gc::GcHeap, key: &str) -> bool {
    heap.with_payload(arr, |body| {
        let removed = body
            .exotic
            .as_deref_mut()
            .and_then(|exotic| exotic.accessors.as_mut())
            .is_some_and(|m| m.remove(key).is_some());
        if removed {
            body.mark_dirty();
        }
        removed
    })
}

/// Delete a string-keyed own property from an array exotic.
#[must_use]
pub fn delete_named_property(arr: JsArray, heap: &mut otter_gc::GcHeap, key: &str) -> bool {
    if key == "length" {
        return false;
    }
    if !can_delete_array_property(arr, heap, key) {
        return false;
    }
    if let Some(idx) = crate::object::array_index_property_name(key) {
        let idx = idx as usize;
        return heap.with_payload(arr, |body| {
            if let Some(exotic) = body.exotic.as_deref_mut()
                && let Some(accessors) = exotic.accessors.as_mut()
            {
                accessors.remove(key);
                if accessors.is_empty() {
                    exotic.accessors = None;
                }
            }
            if let Some(slot) = body.elements.get_mut(idx) {
                *slot = Value::hole();
            }
            if let Some(exotic) = body.exotic.as_deref_mut()
                && let Some(sparse) = exotic.sparse_elements.as_mut()
            {
                sparse.remove(&idx);
            }
            if let Some(exotic) = body.exotic.as_deref_mut()
                && let Some(flags) = exotic.property_flags.as_mut()
            {
                flags.remove(key);
                if flags.is_empty() {
                    exotic.property_flags = None;
                }
            }
            body.mark_dirty();
            true
        });
    }
    heap.with_payload(arr, |body| {
        if let Some(exotic) = body.exotic.as_deref_mut()
            && let Some(accessors) = exotic.accessors.as_mut()
        {
            accessors.remove(key);
            if accessors.is_empty() {
                exotic.accessors = None;
            }
        }
        if let Some(exotic) = body.exotic.as_deref_mut()
            && let Some(props) = exotic.named_properties.as_mut()
        {
            props.remove(key);
            if props.is_empty() {
                exotic.named_properties = None;
            }
        }
        if let Some(exotic) = body.exotic.as_deref_mut()
            && let Some(flags) = exotic.property_flags.as_mut()
        {
            flags.remove(key);
            if flags.is_empty() {
                exotic.property_flags = None;
            }
        }
        body.mark_dirty();
        true
    })
}

/// Own array-index keys plus non-index string keys in array own-key order.
///
/// Dense/sparse/accessor numeric keys are returned as indices; non-index data
/// and accessor names are deduplicated in insertion-table order.
#[must_use]
pub(crate) fn own_index_and_string_keys(
    arr: JsArray,
    heap: &otter_gc::GcHeap,
) -> (BTreeSet<usize>, Vec<String>) {
    heap.read_payload(arr, |body| {
        let mut indices = BTreeSet::new();
        for (idx, value) in body.elements.iter().enumerate() {
            if !value.is_hole() {
                indices.insert(idx);
            }
        }
        if let Some(sparse) = body.sparse_elements() {
            indices.extend(sparse.keys().copied());
        }
        let mut string_keys = Vec::new();
        if let Some(named) = body.named_properties() {
            string_keys.extend(named.keys().cloned());
        }
        if let Some(accessors) = body.accessors() {
            for key in accessors.keys() {
                if let Some(index) = crate::object::array_index_property_name(key) {
                    indices.insert(index as usize);
                } else if !string_keys.iter().any(|existing| existing == key) {
                    string_keys.push(key.clone());
                }
            }
        }
        (indices, string_keys)
    })
}

/// Non-index own string keys from named data properties and accessors.
#[must_use]
pub(crate) fn own_non_index_string_keys(arr: JsArray, heap: &otter_gc::GcHeap) -> Vec<String> {
    let (_, keys) = own_index_and_string_keys(arr, heap);
    keys
}

pub(crate) fn can_write_array_property(arr: JsArray, heap: &otter_gc::GcHeap, key: &str) -> bool {
    heap.read_payload(arr, |body| {
        if body.accessors().is_some_and(|m| m.contains_key(key)) {
            return false;
        }
        body.property_flags()
            .and_then(|flags| flags.get(key))
            .is_none_or(|flags| flags.writable())
    })
}

fn can_delete_array_property(arr: JsArray, heap: &otter_gc::GcHeap, key: &str) -> bool {
    heap.read_payload(arr, |body| {
        body.property_flags()
            .and_then(|flags| flags.get(key))
            .is_none_or(|flags| flags.configurable())
    })
}

/// Read-only access to dense elements for call sites that need to
/// derive an aggregate result without exposing the body borrow.
pub fn with_elements<R>(arr: JsArray, heap: &otter_gc::GcHeap, f: impl FnOnce(&[Value]) -> R) -> R {
    heap.read_payload(arr, |body| f(&body.elements))
}

/// Crate-internal mutable access to dense elements for in-place
/// rewrites that do not grow capacity.
///
/// The helper conservatively fires write barriers for every
/// GC-bearing element left in the array after the mutation. This keeps
/// internal algorithms such as `reverse`, `sort`, and `splice` from
/// having to duplicate barrier bookkeeping while preventing external
/// code from storing untraced values through an arbitrary closure.
pub(crate) fn with_elements_mut<R>(
    arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    f: impl FnOnce(&mut Vec<Value>) -> R,
) -> R {
    let (out, children) = heap.with_payload(arr, |body| {
        let out = f(&mut body.elements);
        body.length = body.elements.len();
        body.mark_dirty();
        let children: SmallVec<[Value; 8]> = body.elements.iter().cloned().collect();
        (out, children)
    });
    for child in children {
        heap.record_write(arr, &child);
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
        if !body.elements.iter().all(is_render_stable_primitive) {
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

#[cfg(test)]
fn reserve_elements_for_len(
    body: &mut ArrayBody,
    heap: &mut otter_gc::GcHeap,
    target_len: usize,
) -> Result<(), otter_gc::OutOfMemory> {
    if target_len <= body.elements.capacity() {
        return Ok(());
    }

    let before = spilled_capacity_bytes(body.elements.capacity());
    let after = spilled_capacity_bytes(target_len);
    if after > before {
        heap.reserve_bytes_no_collect((after - before) as u64)?;
    }
    body.elements
        .reserve_exact(target_len.saturating_sub(body.elements.len()));
    Ok(())
}

fn reserve_for_target_len(
    arr: JsArray,
    heap: &mut otter_gc::GcHeap,
    target_len: usize,
) -> Result<(), otter_gc::OutOfMemory> {
    let current_capacity = heap.read_payload(arr, |body| body.elements.capacity());
    if target_len <= current_capacity {
        return Ok(());
    }

    let before = spilled_capacity_bytes(current_capacity);
    let after = spilled_capacity_bytes(target_len);
    if after > before {
        heap.reserve_bytes_no_collect((after - before) as u64)?;
    }
    // The actual reserve is performed under `with_payload` after the
    // cap check succeeds; keep this helper allocation-free.
    Ok(())
}

fn reserve_for_target_len_with_roots(
    arr: &mut JsArray,
    heap: &mut otter_gc::GcHeap,
    target_len: usize,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    let current_capacity = heap.read_payload(*arr, |body| body.elements.capacity());
    if target_len <= current_capacity {
        return Ok(());
    }

    let before = spilled_capacity_bytes(current_capacity);
    let after = spilled_capacity_bytes(target_len);
    if after > before {
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            visitor(std::ptr::addr_of_mut!(*arr) as *mut RawGc);
        };
        heap.reserve_bytes_with_roots((after - before) as u64, &mut reserve_roots)?;
    }
    // The actual reserve is performed under `with_payload` after the
    // cap check succeeds; keep this helper allocation-free.
    Ok(())
}

fn should_store_sparse(arr: JsArray, heap: &otter_gc::GcHeap, idx: usize) -> bool {
    const MAX_DENSE_GAP: usize = 1024;
    const MAX_DENSE_INDEX: usize = 1 << 20;

    heap.read_payload(arr, |body| {
        idx >= MAX_DENSE_INDEX || idx.saturating_sub(body.elements.len()) > MAX_DENSE_GAP
    })
}

fn spilled_capacity_bytes(capacity: usize) -> usize {
    let inline = 4;
    if capacity <= inline {
        0
    } else {
        capacity.saturating_mul(mem::size_of::<Value>())
    }
}

impl ArrayBody {
    /// Iterate over elements.
    pub fn iter(&self) -> impl Iterator<Item = &Value> {
        self.elements.iter()
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
        let a = from_vec_with_roots(
            &mut heap,
            vec![
                Value::number_i32(0),
                Value::number_i32(1),
                Value::number_i32(2),
                Value::number_i32(3),
                Value::number_i32(4),
                Value::number_i32(5),
            ],
            &mut external_visit,
        )
        .unwrap();

        assert_eq!(len(a, &heap), 6);
        assert_eq!(get(a, &heap, 0), Value::number_i32(0));
        assert_eq!(get(a, &heap, 5), Value::number_i32(5));
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
        set_accessor(a, &mut heap, "2", Some(Value::undefined()), None);
        set_property_flags(a, &mut heap, "2", PropertyFlags::new(false, false, false));

        set_named_property(a, &mut heap, "length", Value::number_i32(2)).unwrap();

        assert_eq!(len(a, &heap), 3);
        assert!(get_accessor(a, &heap, "2").is_some());
        assert_eq!(
            get_property_flags(a, &heap, "2"),
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
    fn named_property_lookup_skips_holes() {
        let mut heap = fresh_heap();
        let a = alloc_array_old_for_fixture(&mut heap).unwrap();
        set(a, &mut heap, 2, Value::boolean(true)).unwrap();
        // Hole index — own-property lookup returns `None` so
        // callers can fall back to the prototype chain.
        assert_eq!(get_named_property(a, &heap, "0"), None);
        // Filled index — own-property lookup returns the value.
        assert_eq!(
            get_named_property(a, &heap, "2"),
            Some(Value::boolean(true))
        );
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
