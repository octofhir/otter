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
//! - Missing-index reads return `Value::Undefined`.
//! - Element growth goes through helpers that reserve off-slot
//!   `SmallVec` capacity against the heap cap before resizing.
//!
//! # See also
//!
//! - <https://tc39.es/ecma262/#sec-array-exotic-objects>
//! - [GC API](../../../docs/book/src/engine/gc-api.md)

use std::collections::HashMap;
use std::mem;
use std::sync::Arc;

use smallvec::SmallVec;

use crate::Value;
use crate::number::NumberValue;
use otter_gc::raw::SlotVisitor;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`ArrayBody`].
///
/// Distinct from task-76 upvalues (`0x10`) and task-77 objects
/// (`0x11`).
pub const ARRAY_BODY_TYPE_TAG: u8 = 0x12;

/// Heap-shared array handle.
pub type JsArray = otter_gc::Gc<ArrayBody>;

/// GC-allocated storage backing every [`JsArray`] handle.
#[derive(Debug, Default)]
pub struct ArrayBody {
    /// Dense element storage. Crate-internal callers must go through
    /// this module's helpers so growth is heap-accounted.
    pub(crate) elements: SmallVec<[Value; 4]>,
    /// Sparse array-indexed own elements.
    ///
    /// This is intentionally separate from string-keyed
    /// `named_properties`: array indices have different `length`
    /// semantics in ECMA-262, but storing huge holes densely would
    /// violate the task-84 survivability gate.
    pub(crate) sparse_elements: Option<HashMap<usize, Value>>,
    /// Optional non-index string-keyed own properties.
    pub(crate) named_properties: Option<HashMap<String, Value>>,
    /// Verbatim slice of input text captured by `JSON.parse` for the
    /// lazy stringify memcpy fast-path. `Some` only when the array
    /// originated from `JSON.parse`; the slice spans the closing
    /// brackets `[…]` exactly.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-json.stringify> §25.5.2
    pub(crate) source_bytes: Option<Arc<[u8]>>,
    /// `true` once the array has been mutated since `source_bytes`
    /// was captured. Always `false` while `source_bytes` is `None`
    /// (no fast path is in play to invalidate).
    pub(crate) dirty: bool,
}

impl otter_gc::SafeTraceable for ArrayBody {
    const TYPE_TAG: u8 = ARRAY_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        for element in &self.elements {
            element.trace_value_slots(visitor);
        }
        if let Some(sparse) = &self.sparse_elements {
            for value in sparse.values() {
                value.trace_value_slots(visitor);
            }
        }
        if let Some(named) = &self.named_properties {
            for value in named.values() {
                value.trace_value_slots(visitor);
            }
        }
    }
}

/// Allocate a fresh empty array.
///
/// # Errors
///
/// Returns [`otter_gc::OutOfMemory`] if the array shell allocation
/// would exceed the configured heap cap.
pub fn alloc_array(heap: &mut otter_gc::GcHeap) -> Result<JsArray, otter_gc::OutOfMemory> {
    heap.alloc_old(ArrayBody::default())
}

/// Construct an array from initial elements.
///
/// # Errors
///
/// Returns [`otter_gc::OutOfMemory`] if either the array shell or
/// off-slot dense storage reservation would exceed the heap cap.
pub fn from_elements(
    heap: &mut otter_gc::GcHeap,
    values: impl IntoIterator<Item = Value>,
) -> Result<JsArray, otter_gc::OutOfMemory> {
    let collected: Vec<Value> = values.into_iter().collect();
    let mut body = ArrayBody::default();
    reserve_elements_for_len(&mut body, heap, collected.len())?;
    body.elements.extend(collected);
    heap.alloc_old(body)
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
pub fn from_elements_with_source(
    heap: &mut otter_gc::GcHeap,
    values: impl IntoIterator<Item = Value>,
    source_bytes: Arc<[u8]>,
) -> Result<JsArray, otter_gc::OutOfMemory> {
    let collected: Vec<Value> = values.into_iter().collect();
    let mut body = ArrayBody::default();
    reserve_elements_for_len(&mut body, heap, collected.len())?;
    body.elements.extend(collected);
    body.source_bytes = Some(source_bytes);
    body.dirty = false;
    heap.alloc_old(body)
}

/// Length in elements (O(1)).
#[must_use]
pub fn len(arr: JsArray, heap: &otter_gc::GcHeap) -> usize {
    heap.read_payload(arr, |body| body.elements.len())
}

/// `true` for an empty array.
#[must_use]
pub fn is_empty(arr: JsArray, heap: &otter_gc::GcHeap) -> bool {
    len(arr, heap) == 0
}

/// Read element at `idx`. Out-of-range and array-hole slots both
/// return [`Value::Undefined`] per ECMA-262 §10.4.2 OrdinaryGet —
/// the internal [`Value::Hole`] sentinel never escapes the array.
#[must_use]
pub fn get(arr: JsArray, heap: &otter_gc::GcHeap, idx: usize) -> Value {
    heap.read_payload(arr, |body| {
        let raw = body
            .elements
            .get(idx)
            .cloned()
            .or_else(|| {
                body.sparse_elements
                    .as_ref()
                    .and_then(|sparse| sparse.get(&idx).cloned())
            })
            .unwrap_or(Value::Undefined);
        match raw {
            Value::Hole => Value::Undefined,
            other => other,
        }
    })
}

/// Spec [HasProperty](https://tc39.es/ecma262/#sec-array-exotic-objects)
/// for array-indexed slots: a missing dense element ([`Value::Hole`])
/// or an absent sparse entry returns `false`, even when the index
/// is below `length`. Returns `true` only when an explicit value
/// occupies the slot.
#[must_use]
pub fn has_own_element(arr: JsArray, heap: &otter_gc::GcHeap, idx: usize) -> bool {
    heap.read_payload(arr, |body| {
        if let Some(slot) = body.elements.get(idx) {
            return !matches!(slot, Value::Hole);
        }
        body.sparse_elements
            .as_ref()
            .is_some_and(|sparse| sparse.contains_key(&idx))
    })
}

/// Write element at `idx`, extending with the internal
/// [`Value::Hole`] sentinel when `idx > len` so absent slots remain
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
    let barrier_value = value.clone();
    let target_len = idx.saturating_add(1);
    if should_store_sparse(arr, heap, idx) {
        heap.with_payload(arr, |body| {
            let sparse = body.sparse_elements.get_or_insert_with(HashMap::new);
            sparse.insert(idx, value);
            body.dirty = true;
        });
        heap.record_write(arr, &barrier_value);
        return Ok(());
    }
    reserve_for_target_len(arr, heap, target_len)?;
    heap.with_payload(arr, |body| {
        if idx < body.elements.len() {
            body.elements[idx] = value;
            body.dirty = true;
            return;
        }
        body.elements
            .reserve_exact(target_len.saturating_sub(body.elements.len()));
        while body.elements.len() < idx {
            body.elements.push(Value::Hole);
        }
        body.elements.push(value);
        body.dirty = true;
    });
    heap.record_write(arr, &barrier_value);
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
    let barrier_value = value.clone();
    let target_len = len(arr, heap).saturating_add(1);
    reserve_for_target_len(arr, heap, target_len)?;
    let new_len = heap.with_payload(arr, |body| {
        body.elements
            .reserve_exact(target_len.saturating_sub(body.elements.len()));
        body.elements.push(value);
        body.dirty = true;
        body.elements.len()
    });
    heap.record_write(arr, &barrier_value);
    Ok(new_len)
}

/// Pop from the tail. Returns `Value::Undefined` for an empty array
/// and for slots that hold the internal [`Value::Hole`] sentinel.
#[must_use]
pub fn pop(arr: JsArray, heap: &mut otter_gc::GcHeap) -> Value {
    heap.with_payload(arr, |body| {
        let popped = body.elements.pop();
        if popped.is_some() {
            body.dirty = true;
        }
        match popped {
            Some(Value::Hole) | None => Value::Undefined,
            Some(other) => other,
        }
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
    if let Ok(idx) = key.parse::<usize>() {
        return set(arr, heap, idx, value);
    }
    let barrier_value = value.clone();
    heap.with_payload(arr, |body| {
        let map = body.named_properties.get_or_insert_with(HashMap::new);
        map.insert(key.to_string(), value);
        body.dirty = true;
    });
    heap.record_write(arr, &barrier_value);
    Ok(())
}

/// Read a string-keyed own property. Numeric strings route to indexed
/// elements; `length` returns the array length.
#[must_use]
pub fn get_named_property(arr: JsArray, heap: &otter_gc::GcHeap, key: &str) -> Option<Value> {
    if key == "length" {
        return Some(Value::Number(crate::number::NumberValue::from_i32(
            len(arr, heap) as i32,
        )));
    }
    if let Ok(idx) = key.parse::<usize>() {
        return heap.read_payload(arr, |body| {
            body.elements
                .get(idx)
                .filter(|v| !matches!(v, Value::Hole))
                .cloned()
                .or_else(|| {
                    body.sparse_elements
                        .as_ref()
                        .and_then(|sparse| sparse.get(&idx).cloned())
                })
        });
    }
    heap.read_payload(arr, |body| {
        body.named_properties
            .as_ref()
            .and_then(|m| m.get(key).cloned())
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
    f: impl FnOnce(&mut SmallVec<[Value; 4]>) -> R,
) -> R {
    let (out, children) = heap.with_payload(arr, |body| {
        let out = f(&mut body.elements);
        body.dirty = true;
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
        if body.dirty {
            return None;
        }
        let source = body.source_bytes.as_ref()?;
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
    matches!(
        v,
        Value::Null
            | Value::Boolean(_)
            | Value::Number(_)
            | Value::String(_)
    )
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

/// Stable identity token for hash tables that still key object-like
/// values by address. Once Map/Set migrate in task 79 this can become
/// a `Gc`-native key instead of a pointer-shaped token.
#[must_use]
pub fn identity_addr(arr: JsArray) -> *const () {
    (arr.offset() as usize) as *const ()
}

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
        let a = from_elements(
            &mut heap,
            [Value::Boolean(true), Value::Null, Value::Boolean(false)],
        )
        .unwrap();
        assert_eq!(len(a, &heap), 3);
        assert_eq!(get(a, &heap, 0), Value::Boolean(true));
        assert_eq!(get(a, &heap, 1), Value::Null);
        assert_eq!(get(a, &heap, 2), Value::Boolean(false));
    }

    #[test]
    fn out_of_range_read_is_undefined() {
        let mut heap = fresh_heap();
        let a = alloc_array(&mut heap).unwrap();
        assert_eq!(get(a, &heap, 0), Value::Undefined);
    }

    #[test]
    fn out_of_range_write_extends_with_holes() {
        let mut heap = fresh_heap();
        let a = alloc_array(&mut heap).unwrap();
        set(a, &mut heap, 2, Value::Boolean(true)).unwrap();
        assert_eq!(len(a, &heap), 3);
        // Public reads observe `Value::Undefined` for absent slots,
        // even though the body stores `Value::Hole` internally.
        assert_eq!(get(a, &heap, 0), Value::Undefined);
        assert_eq!(get(a, &heap, 1), Value::Undefined);
        assert_eq!(get(a, &heap, 2), Value::Boolean(true));
        // `has_own_element` distinguishes the two: holes report `false`,
        // explicit values report `true`.
        assert!(!has_own_element(a, &heap, 0));
        assert!(!has_own_element(a, &heap, 1));
        assert!(has_own_element(a, &heap, 2));
        // Out-of-range index is also absent.
        assert!(!has_own_element(a, &heap, 99));
    }

    #[test]
    fn explicit_undefined_distinguished_from_hole() {
        let mut heap = fresh_heap();
        let a = from_elements(&mut heap, [Value::Undefined]).unwrap();
        // Explicit undefined is a real own element.
        assert!(has_own_element(a, &heap, 0));
        assert_eq!(get(a, &heap, 0), Value::Undefined);
    }

    #[test]
    fn hole_does_not_escape_via_pop() {
        let mut heap = fresh_heap();
        let a = alloc_array(&mut heap).unwrap();
        set(a, &mut heap, 1, Value::Boolean(true)).unwrap();
        // Tail is the explicit value.
        assert_eq!(pop(a, &mut heap), Value::Boolean(true));
        // Next pop pulls the leading hole — must surface as
        // `undefined`, never as the internal sentinel.
        assert_eq!(pop(a, &mut heap), Value::Undefined);
        assert!(is_empty(a, &heap));
    }

    #[test]
    fn named_property_lookup_skips_holes() {
        let mut heap = fresh_heap();
        let a = alloc_array(&mut heap).unwrap();
        set(a, &mut heap, 2, Value::Boolean(true)).unwrap();
        // Hole index — own-property lookup returns `None` so
        // callers can fall back to the prototype chain.
        assert_eq!(get_named_property(a, &heap, "0"), None);
        // Filled index — own-property lookup returns the value.
        assert_eq!(
            get_named_property(a, &heap, "2"),
            Some(Value::Boolean(true))
        );
    }

    #[test]
    fn push_and_pop() {
        let mut heap = fresh_heap();
        let a = alloc_array(&mut heap).unwrap();
        assert_eq!(push(a, &mut heap, Value::Boolean(true)).unwrap(), 1);
        assert_eq!(push(a, &mut heap, Value::Null).unwrap(), 2);
        assert_eq!(pop(a, &mut heap), Value::Null);
        assert_eq!(pop(a, &mut heap), Value::Boolean(true));
        assert_eq!(pop(a, &mut heap), Value::Undefined);
        assert!(is_empty(a, &heap));
    }

    #[test]
    fn clean_source_bytes_fast_path_for_unmutated_primitive_array() {
        let mut heap = fresh_heap();
        let bytes: Arc<[u8]> = Arc::from(&b"[1,2,3]"[..]);
        let a = from_elements_with_source(
            &mut heap,
            [
                Value::Number(NumberValue::from_i32(1)),
                Value::Number(NumberValue::from_i32(2)),
                Value::Number(NumberValue::from_i32(3)),
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
        let a = from_elements_with_source(
            &mut heap,
            [
                Value::Number(NumberValue::from_i32(1)),
                Value::Number(NumberValue::from_i32(2)),
                Value::Number(NumberValue::from_i32(3)),
            ],
            Arc::clone(&bytes),
        )
        .unwrap();
        push(a, &mut heap, Value::Number(NumberValue::from_i32(99))).unwrap();
        assert!(clean_source_bytes(a, &heap).is_none());
    }

    #[test]
    fn clean_source_bytes_disqualified_when_holding_compound_element() {
        let mut heap = fresh_heap();
        // An array containing a nested array is *not* eligible for
        // the fast path even when its own dirty bit is clear, because
        // the nested array can mutate independently and would render
        // the captured `[…]` slice stale.
        let inner = alloc_array(&mut heap).unwrap();
        let bytes: Arc<[u8]> = Arc::from(&b"[[]]"[..]);
        let outer =
            from_elements_with_source(&mut heap, [Value::Array(inner)], Arc::clone(&bytes))
                .unwrap();
        assert!(clean_source_bytes(outer, &heap).is_none());
    }

    #[test]
    fn copying_handle_shares_storage() {
        let mut heap = fresh_heap();
        let a = alloc_array(&mut heap).unwrap();
        let b = a;
        push(a, &mut heap, Value::Boolean(true)).unwrap();
        assert!(ptr_eq(a, b));
        assert_eq!(get(b, &heap, 0), Value::Boolean(true));
    }
}
