//! `Map` / `Set` / `WeakMap` / `WeakSet` prototype method tables.
//!
//! Mirrors the `String.prototype` / `Array.prototype` tables but
//! splits across four [`crate::intrinsics::IntrinsicReceiver`]
//! kinds so the dispatcher can route by receiver type.
//!
//! The `forEach` family is **not** registered through the static
//! intrinsic table — its callback dispatch needs the
//! [`crate::Interpreter`] to push a frame, so it lives in a
//! dedicated dispatcher in [`crate::lib`] (`collections_call_for_each`).
//!
//! # Contents
//! - [`MAP_PROTOTYPE_TABLE`] — `Map.prototype` synchronous methods.
//! - [`SET_PROTOTYPE_TABLE`] — `Set.prototype` synchronous methods.
//! - [`WEAK_MAP_PROTOTYPE_TABLE`] — `WeakMap.prototype`.
//! - [`WEAK_SET_PROTOTYPE_TABLE`] — `WeakSet.prototype`.
//! - [`load_property`] — accessor reads (`size`).
//!
//! # Invariants
//! - All methods reject the wrong receiver type with
//!   [`crate::IntrinsicError::BadReceiver`].
//! - `WeakMap` / `WeakSet` reject primitive keys with
//!   [`crate::IntrinsicError::BadArgument`] mapping the
//!   `CollectionError::NonObjectKey` path through.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-map-prototype-object>
//! - <https://tc39.es/ecma262/#sec-set-prototype-object>
//! - <https://tc39.es/ecma262/#sec-weakmap-prototype-object>
//! - <https://tc39.es/ecma262/#sec-weakset-prototype-object>

use crate::Value;
use crate::collections::{self, CollectionError, JsMap, JsSet, JsWeakMap, JsWeakSet};
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::number::NumberValue;
use smallvec::SmallVec;

// ---------------------------------------------------------------
// Map.prototype
// ---------------------------------------------------------------

fn receiver_map(args: &IntrinsicArgs<'_>) -> Result<JsMap, IntrinsicError> {
    match args.receiver {
        Value::Map(m) => Ok(*m),
        _ => Err(IntrinsicError::BadReceiver { expected: "Map" }),
    }
}

fn impl_map_get(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_map(args)?;
    let key = args.args.first().cloned().unwrap_or(Value::Undefined);
    let heap = &*args.gc_heap;
    Ok(collections::map_get(m, heap, &key).unwrap_or(Value::Undefined))
}

fn impl_map_set(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let mut m = receiver_map(args)?;
    let key = args.args.first().cloned().unwrap_or(Value::Undefined);
    let value = args.args.get(1).cloned().unwrap_or(Value::Undefined);
    args.map_set_rooted(&mut m, key, value)?;
    Ok(Value::Map(m))
}

fn impl_map_has(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_map(args)?;
    let key = args.args.first().cloned().unwrap_or(Value::Undefined);
    let heap = &*args.gc_heap;
    Ok(Value::Boolean(collections::map_has(m, heap, &key)))
}

fn impl_map_delete(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_map(args)?;
    let key = args.args.first().cloned().unwrap_or(Value::Undefined);
    let heap = &mut *args.gc_heap;
    Ok(Value::Boolean(collections::map_delete(m, heap, &key)))
}

fn impl_map_clear(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_map(args)?;
    let heap = &mut *args.gc_heap;
    collections::map_clear(m, heap);
    Ok(Value::Undefined)
}

/// `Map.prototype.keys` — returns a foundation iterator factory
/// closure-bearing native function. Spec §24.1.3.8.
fn impl_map_keys(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_map(args)?;
    let state = map_iter_state(args, MapIterKind::Keys, m)?;
    Ok(make_iter_value(args, state)?)
}

/// `Map.prototype.values` — Spec §24.1.3.10.
fn impl_map_values(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_map(args)?;
    let state = map_iter_state(args, MapIterKind::Values, m)?;
    Ok(make_iter_value(args, state)?)
}

/// `Map.prototype.entries` — Spec §24.1.3.4.
fn impl_map_entries(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_map(args)?;
    let state = map_iter_state(args, MapIterKind::Entries, m)?;
    Ok(make_iter_value(args, state)?)
}

#[derive(Debug, Clone, Copy)]
enum MapIterKind {
    Keys,
    Values,
    Entries,
}

fn map_iter_state(
    args: &mut IntrinsicArgs<'_>,
    kind: MapIterKind,
    m: JsMap,
) -> Result<crate::IteratorState, otter_gc::OutOfMemory> {
    let entries = collections::map_entries(m, &*args.gc_heap);
    let mut snapshot: SmallVec<[Value; 4]> = SmallVec::with_capacity(entries.len());
    for (k, v) in entries {
        snapshot.push(match kind {
            MapIterKind::Keys => k,
            MapIterKind::Values => v,
            MapIterKind::Entries => {
                let pair = args.array_from_elements_rooted([k, v], &[], &[snapshot.as_slice()])?;
                Value::Array(pair)
            }
        });
    }
    let arr = args.array_from_elements_rooted(snapshot, &[], &[])?;
    Ok(crate::IteratorState::Array {
        array: arr,
        index: 0,
    })
}

fn make_iter_value(
    args: &mut IntrinsicArgs<'_>,
    state: crate::IteratorState,
) -> Result<Value, otter_gc::OutOfMemory> {
    Ok(Value::Iterator(args.alloc_iterator_state_rooted(
        state,
        &[],
        &[],
    )?))
}

// ---------------------------------------------------------------
// Set.prototype
// ---------------------------------------------------------------

fn receiver_set(args: &IntrinsicArgs<'_>) -> Result<JsSet, IntrinsicError> {
    match args.receiver {
        Value::Set(s) => Ok(*s),
        _ => Err(IntrinsicError::BadReceiver { expected: "Set" }),
    }
}

fn impl_set_add(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let mut s = receiver_set(args)?;
    let v = args.args.first().cloned().unwrap_or(Value::Undefined);
    args.set_add_rooted(&mut s, v)?;
    Ok(Value::Set(s))
}

fn impl_set_has(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_set(args)?;
    let v = args.args.first().cloned().unwrap_or(Value::Undefined);
    let heap = &*args.gc_heap;
    Ok(Value::Boolean(collections::set_has(s, heap, &v)))
}

fn impl_set_delete(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_set(args)?;
    let v = args.args.first().cloned().unwrap_or(Value::Undefined);
    let heap = &mut *args.gc_heap;
    Ok(Value::Boolean(collections::set_delete(s, heap, &v)))
}

fn impl_set_clear(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_set(args)?;
    let heap = &mut *args.gc_heap;
    collections::set_clear(s, heap);
    Ok(Value::Undefined)
}

fn impl_set_values(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_set(args)?;
    let snap: SmallVec<[Value; 4]> = collections::set_values(s, &*args.gc_heap)
        .into_iter()
        .collect();
    let array = args.array_from_elements_rooted(snap, &[], &[])?;
    Ok(make_iter_value(
        args,
        crate::IteratorState::Array { array, index: 0 },
    )?)
}

/// `Set.prototype.keys` is the same as `values` per spec
/// §24.2.3.8 / §24.2.3.10.
fn impl_set_keys(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    impl_set_values(args)
}

/// Extract a snapshot of values from `other` for the ES2025
/// set-method `other` operand. Per the
/// [set-methods proposal](https://tc39.es/proposal-set-methods/),
/// `other` must be a "set-like" — an object exposing `size: Number`,
/// `has(value): boolean`, and `keys(): Iterator`. Foundation
/// supports the two most common shapes natively:
///
/// - `Value::Set` — direct insertion-order snapshot.
/// - `Value::Map` — insertion-order keys (matches the
///   `Map.prototype.keys` iterator).
///
/// Arbitrary set-likes that need `.has` / `.keys` invocation go
/// through the slow path in `set_method_other_snapshot_dynamic` once
/// it lands; the foundation fall-back rejects with `TypeMismatch`.
fn set_method_other_snapshot(
    other: &Value,
    heap: &otter_gc::GcHeap,
) -> Result<Vec<Value>, IntrinsicError> {
    match other {
        Value::Set(s) => Ok(collections::set_values(*s, heap)),
        Value::Map(m) => Ok(collections::map_entries(*m, heap)
            .into_iter()
            .map(|(k, _)| k)
            .collect()),
        _ => Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "must be a Set or Map",
        }),
    }
}

/// §24.2.4.7 `Set.prototype.union(other)` — return a new Set with
/// every value of `this` followed by every value of `other` not
/// already present (insertion order preserved).
fn impl_set_union(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_set(args)?;
    let other = args.args.first().cloned().unwrap_or(Value::Undefined);
    let other_values = set_method_other_snapshot(&other, &*args.gc_heap)?;
    // Walk `this` first; then merge `other` entries skipping
    // already-present values via the existing
    // SameValueZero-aware `set_has` probe.
    let mut new_set = {
        let mut visitor = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            for v in &other_values {
                v.trace_value_slots(visit);
            }
        };
        collections::alloc_set_with_roots(args.gc_heap, &mut visitor)?
    };
    let existing = collections::set_values(s, &*args.gc_heap);
    for v in existing {
        args.set_add_rooted(&mut new_set, v)?;
    }
    for v in other_values {
        if collections::set_has(new_set, &*args.gc_heap, &v) {
            continue;
        }
        args.set_add_rooted(&mut new_set, v)?;
    }
    Ok(Value::Set(new_set))
}

/// §24.2.4.5 `Set.prototype.intersection(other)` — values present in
/// both `this` and `other`. Insertion order follows the smaller set
/// per spec step 5 (foundation always iterates `this` for
/// determinism).
fn impl_set_intersection(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_set(args)?;
    let other = args.args.first().cloned().unwrap_or(Value::Undefined);
    let other_values = set_method_other_snapshot(&other, &*args.gc_heap)?;
    let mut new_set = {
        let mut visitor = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            for v in &other_values {
                v.trace_value_slots(visit);
            }
        };
        collections::alloc_set_with_roots(args.gc_heap, &mut visitor)?
    };
    // Use `other_values` as the membership probe — saves one
    // `Vec<Value>` alloc for `this`'s snapshot. For SameValueZero
    // semantics we still rely on the `set_has` insertion check
    // against the under-construction `new_set` to deduplicate
    // (other_values may contain the same value twice if it's a
    // Map keys iteration).
    let this_values = collections::set_values(s, &*args.gc_heap);
    for v in this_values {
        let in_other = other_values
            .iter()
            .any(|o| crate::abstract_ops::same_value_zero(o, &v));
        if !in_other {
            continue;
        }
        if collections::set_has(new_set, &*args.gc_heap, &v) {
            continue;
        }
        args.set_add_rooted(&mut new_set, v)?;
    }
    Ok(Value::Set(new_set))
}

/// §24.2.4.4 `Set.prototype.difference(other)` — values in `this`
/// not in `other`.
fn impl_set_difference(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_set(args)?;
    let other = args.args.first().cloned().unwrap_or(Value::Undefined);
    let other_values = set_method_other_snapshot(&other, &*args.gc_heap)?;
    let mut new_set = {
        let mut visitor = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            for v in &other_values {
                v.trace_value_slots(visit);
            }
        };
        collections::alloc_set_with_roots(args.gc_heap, &mut visitor)?
    };
    let this_values = collections::set_values(s, &*args.gc_heap);
    for v in this_values {
        let in_other = other_values
            .iter()
            .any(|o| crate::abstract_ops::same_value_zero(o, &v));
        if in_other {
            continue;
        }
        args.set_add_rooted(&mut new_set, v)?;
    }
    Ok(Value::Set(new_set))
}

/// §24.2.4.6 `Set.prototype.symmetricDifference(other)` — values in
/// `this` xor `other`.
fn impl_set_symmetric_difference(
    args: &mut IntrinsicArgs<'_>,
) -> Result<Value, IntrinsicError> {
    let s = receiver_set(args)?;
    let other = args.args.first().cloned().unwrap_or(Value::Undefined);
    let other_values = set_method_other_snapshot(&other, &*args.gc_heap)?;
    let mut new_set = {
        let mut visitor = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            for v in &other_values {
                v.trace_value_slots(visit);
            }
        };
        collections::alloc_set_with_roots(args.gc_heap, &mut visitor)?
    };
    let this_values = collections::set_values(s, &*args.gc_heap);
    for v in &this_values {
        let in_other = other_values
            .iter()
            .any(|o| crate::abstract_ops::same_value_zero(o, v));
        if !in_other {
            args.set_add_rooted(&mut new_set, v.clone())?;
        }
    }
    for v in other_values {
        let in_this = this_values
            .iter()
            .any(|t| crate::abstract_ops::same_value_zero(t, &v));
        if in_this {
            continue;
        }
        if collections::set_has(new_set, &*args.gc_heap, &v) {
            continue;
        }
        args.set_add_rooted(&mut new_set, v)?;
    }
    Ok(Value::Set(new_set))
}

/// §24.2.4.10 `Set.prototype.isSubsetOf(other)` — every value of
/// `this` is in `other`.
fn impl_set_is_subset_of(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_set(args)?;
    let other = args.args.first().cloned().unwrap_or(Value::Undefined);
    let other_values = set_method_other_snapshot(&other, &*args.gc_heap)?;
    let heap = &*args.gc_heap;
    let this_values = collections::set_values(s, heap);
    let all_in_other = this_values.iter().all(|v| {
        other_values
            .iter()
            .any(|o| crate::abstract_ops::same_value_zero(o, v))
    });
    Ok(Value::Boolean(all_in_other))
}

/// §24.2.4.11 `Set.prototype.isSupersetOf(other)` — every value of
/// `other` is in `this`.
fn impl_set_is_superset_of(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_set(args)?;
    let other = args.args.first().cloned().unwrap_or(Value::Undefined);
    let other_values = set_method_other_snapshot(&other, &*args.gc_heap)?;
    let heap = &*args.gc_heap;
    let all_in_this = other_values.iter().all(|v| collections::set_has(s, heap, v));
    Ok(Value::Boolean(all_in_this))
}

/// §24.2.4.9 `Set.prototype.isDisjointFrom(other)` — no shared
/// value.
fn impl_set_is_disjoint_from(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_set(args)?;
    let other = args.args.first().cloned().unwrap_or(Value::Undefined);
    let other_values = set_method_other_snapshot(&other, &*args.gc_heap)?;
    let heap = &*args.gc_heap;
    let any_shared = other_values.iter().any(|v| collections::set_has(s, heap, v));
    Ok(Value::Boolean(!any_shared))
}

/// `Set.prototype.entries` — yields `[v, v]` pairs per
/// §24.2.3.5.
fn impl_set_entries(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_set(args)?;
    let mut snap: SmallVec<[Value; 4]> = SmallVec::new();
    for v in collections::set_values(s, &*args.gc_heap) {
        let pair = args.array_from_elements_rooted([v.clone(), v], &[], &[snap.as_slice()])?;
        snap.push(Value::Array(pair));
    }
    let array = args.array_from_elements_rooted(snap, &[], &[])?;
    Ok(make_iter_value(
        args,
        crate::IteratorState::Array { array, index: 0 },
    )?)
}

// ---------------------------------------------------------------
// WeakMap.prototype
// ---------------------------------------------------------------

fn receiver_weak_map(args: &IntrinsicArgs<'_>) -> Result<JsWeakMap, IntrinsicError> {
    match args.receiver {
        Value::WeakMap(m) => Ok(*m),
        _ => Err(IntrinsicError::BadReceiver {
            expected: "WeakMap",
        }),
    }
}

fn impl_weak_map_get(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_weak_map(args)?;
    let key = args.args.first().cloned().unwrap_or(Value::Undefined);
    let heap = &*args.gc_heap;
    match collections::weak_map_get(m, heap, &key) {
        Ok(Some(v)) => Ok(v),
        Ok(None) | Err(CollectionError::NonObjectKey) => Ok(Value::Undefined),
        Err(_) => Ok(Value::Undefined),
    }
}

fn impl_weak_map_has(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_weak_map(args)?;
    let key = args.args.first().cloned().unwrap_or(Value::Undefined);
    let heap = &*args.gc_heap;
    match collections::weak_map_has(m, heap, &key) {
        Ok(b) => Ok(Value::Boolean(b)),
        Err(CollectionError::NonObjectKey) => Ok(Value::Boolean(false)),
        Err(_) => Ok(Value::Boolean(false)),
    }
}

fn impl_weak_map_set(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let mut m = receiver_weak_map(args)?;
    let key = args.args.first().cloned().unwrap_or(Value::Undefined);
    let value = args.args.get(1).cloned().unwrap_or(Value::Undefined);
    args.weak_map_set_rooted(&mut m, key, value)
        .map_err(weak_collection_to_intrinsic)?;
    Ok(Value::WeakMap(m))
}

fn impl_weak_map_delete(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_weak_map(args)?;
    let key = args.args.first().cloned().unwrap_or(Value::Undefined);
    let heap = &mut *args.gc_heap;
    match collections::weak_map_delete(m, heap, &key) {
        Ok(b) => Ok(Value::Boolean(b)),
        Err(CollectionError::NonObjectKey) => Ok(Value::Boolean(false)),
        Err(_) => Ok(Value::Boolean(false)),
    }
}

// ---------------------------------------------------------------
// WeakSet.prototype
// ---------------------------------------------------------------

fn receiver_weak_set(args: &IntrinsicArgs<'_>) -> Result<JsWeakSet, IntrinsicError> {
    match args.receiver {
        Value::WeakSet(s) => Ok(*s),
        _ => Err(IntrinsicError::BadReceiver {
            expected: "WeakSet",
        }),
    }
}

fn impl_weak_set_add(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let mut s = receiver_weak_set(args)?;
    let v = args.args.first().cloned().unwrap_or(Value::Undefined);
    args.weak_set_add_rooted(&mut s, v)
        .map_err(weak_collection_to_intrinsic)?;
    Ok(Value::WeakSet(s))
}

fn impl_weak_set_has(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_weak_set(args)?;
    let v = args.args.first().cloned().unwrap_or(Value::Undefined);
    let heap = &*args.gc_heap;
    match collections::weak_set_has(s, heap, &v) {
        Ok(b) => Ok(Value::Boolean(b)),
        Err(CollectionError::NonObjectKey) => Ok(Value::Boolean(false)),
        Err(_) => Ok(Value::Boolean(false)),
    }
}

fn impl_weak_set_delete(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_weak_set(args)?;
    let v = args.args.first().cloned().unwrap_or(Value::Undefined);
    let heap = &mut *args.gc_heap;
    match collections::weak_set_delete(s, heap, &v) {
        Ok(b) => Ok(Value::Boolean(b)),
        Err(CollectionError::NonObjectKey) => Ok(Value::Boolean(false)),
        Err(_) => Ok(Value::Boolean(false)),
    }
}

fn weak_collection_to_intrinsic(err: CollectionError) -> IntrinsicError {
    match err {
        CollectionError::BadReceiver { expected } => IntrinsicError::BadReceiver { expected },
        CollectionError::NonObjectKey => IntrinsicError::BadArgument {
            index: 0,
            reason: "must be an object",
        },
        CollectionError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => IntrinsicError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
    }
}

// ---------------------------------------------------------------
// Static tables
// ---------------------------------------------------------------

/// `Map.prototype` synchronous method table.
pub static MAP_PROTOTYPE_TABLE: std::sync::LazyLock<IntrinsicTable> =
    std::sync::LazyLock::new(|| {
        crate::intrinsics!(
            Map,
            "get"     / 1 => impl_map_get,
            "set"     / 2 => impl_map_set,
            "has"     / 1 => impl_map_has,
            "delete"  / 1 => impl_map_delete,
            "clear"   / 0 => impl_map_clear,
            "keys"    / 0 => impl_map_keys,
            "values"  / 0 => impl_map_values,
            "entries" / 0 => impl_map_entries,
        )
    });

/// `Set.prototype` synchronous method table.
pub static SET_PROTOTYPE_TABLE: std::sync::LazyLock<IntrinsicTable> =
    std::sync::LazyLock::new(|| {
        crate::intrinsics!(
            Set,
            "add"     / 1 => impl_set_add,
            "has"     / 1 => impl_set_has,
            "delete"  / 1 => impl_set_delete,
            "clear"   / 0 => impl_set_clear,
            "keys"    / 0 => impl_set_keys,
            "values"  / 0 => impl_set_values,
            "entries" / 0 => impl_set_entries,
            // ES2025 set-methods proposal (
            // <https://tc39.es/proposal-set-methods/>) — combinators
            // returning a new Set, and predicate methods returning
            // Boolean.
            "union"               / 1 => impl_set_union,
            "intersection"        / 1 => impl_set_intersection,
            "difference"          / 1 => impl_set_difference,
            "symmetricDifference" / 1 => impl_set_symmetric_difference,
            "isSubsetOf"          / 1 => impl_set_is_subset_of,
            "isSupersetOf"        / 1 => impl_set_is_superset_of,
            "isDisjointFrom"      / 1 => impl_set_is_disjoint_from,
        )
    });

/// `WeakMap.prototype` table.
pub static WEAK_MAP_PROTOTYPE_TABLE: std::sync::LazyLock<IntrinsicTable> =
    std::sync::LazyLock::new(|| {
        crate::intrinsics!(
            WeakMap,
            "get"    / 1 => impl_weak_map_get,
            "set"    / 2 => impl_weak_map_set,
            "has"    / 1 => impl_weak_map_has,
            "delete" / 1 => impl_weak_map_delete,
        )
    });

/// `WeakSet.prototype` table.
pub static WEAK_SET_PROTOTYPE_TABLE: std::sync::LazyLock<IntrinsicTable> =
    std::sync::LazyLock::new(|| {
        crate::intrinsics!(
            WeakSet,
            "add"    / 1 => impl_weak_set_add,
            "has"    / 1 => impl_weak_set_has,
            "delete" / 1 => impl_weak_set_delete,
        )
    });

/// Lookup for `Map.prototype.<name>`.
#[must_use]
pub fn lookup_map(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    MAP_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Map, name)
}

/// Lookup for `Set.prototype.<name>`.
#[must_use]
pub fn lookup_set(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    SET_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Set, name)
}

/// Lookup for `WeakMap.prototype.<name>`.
#[must_use]
pub fn lookup_weak_map(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    WEAK_MAP_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::WeakMap, name)
}

/// Lookup for `WeakSet.prototype.<name>`.
#[must_use]
pub fn lookup_weak_set(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    WEAK_SET_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::WeakSet, name)
}

/// Read a non-method property off a collection receiver. Foundation
/// exposes only the `size` accessor on `Map` / `Set` (Spec §24.1.3.11
/// / §24.2.3.11). `WeakMap` / `WeakSet` do not have `size` (the spec
/// omits it deliberately because the entries can vanish under GC).
#[must_use]
pub fn load_property(value: &Value, name: &str) -> Value {
    let _ = (value, name);
    Value::Undefined
}

/// Heap-aware version of [`load_property`].
#[must_use]
pub fn load_property_with_heap(value: &Value, name: &str, heap: &otter_gc::GcHeap) -> Value {
    if name == "size" {
        match value {
            Value::Map(m) => {
                Value::Number(NumberValue::from_i32(collections::map_len(*m, heap) as i32))
            }
            Value::Set(s) => {
                Value::Number(NumberValue::from_i32(collections::set_len(*s, heap) as i32))
            }
            _ => Value::Undefined,
        }
    } else {
        Value::Undefined
    }
}

/// Build the native callable that `Map.prototype[Symbol.iterator]`
/// resolves to. Returning the same iterator factory as `entries()`
/// matches Spec §24.1.3.12 (`@@iterator` aliases `entries`).
pub fn make_map_iterator_factory(
    map: JsMap,
    heap: &mut otter_gc::GcHeap,
) -> Result<Value, otter_gc::OutOfMemory> {
    crate::native_value_with_captures(
        heap,
        "Map[Symbol.iterator]",
        smallvec::smallvec![Value::Map(map)],
        |ctx, _, captures| {
            let map = match captures.first() {
                Some(Value::Map(map)) => *map,
                _ => {
                    return Err(crate::NativeError::TypeError {
                        name: "Map[Symbol.iterator]",
                        reason: "missing traced map capture".to_string(),
                    });
                }
            };
            let state = map_iter_state_native(MapIterKind::Entries, map, ctx)?;
            Ok(make_native_iter_value(ctx, state)?)
        },
    )
}

/// Build the native callable that `Set.prototype[Symbol.iterator]`
/// resolves to (alias of `values`, Spec §24.2.3.11).
pub fn make_set_iterator_factory(
    set: JsSet,
    heap: &mut otter_gc::GcHeap,
) -> Result<Value, otter_gc::OutOfMemory> {
    crate::native_value_with_captures(
        heap,
        "Set[Symbol.iterator]",
        smallvec::smallvec![Value::Set(set)],
        |ctx, _, captures| {
            let set = match captures.first() {
                Some(Value::Set(set)) => *set,
                _ => {
                    return Err(crate::NativeError::TypeError {
                        name: "Set[Symbol.iterator]",
                        reason: "missing traced set capture".to_string(),
                    });
                }
            };
            let snap: SmallVec<[Value; 4]> = collections::set_values(set, ctx.heap())
                .into_iter()
                .collect();
            let array = ctx.array_from_elements(snap)?;
            Ok(make_native_iter_value(
                ctx,
                crate::IteratorState::Array { array, index: 0 },
            )?)
        },
    )
}

fn map_iter_state_native(
    kind: MapIterKind,
    m: JsMap,
    ctx: &mut crate::NativeCtx<'_>,
) -> Result<crate::IteratorState, otter_gc::OutOfMemory> {
    let entries = collections::map_entries(m, ctx.heap());
    let mut snapshot: SmallVec<[Value; 4]> = SmallVec::with_capacity(entries.len());
    for (k, v) in entries {
        snapshot.push(match kind {
            MapIterKind::Keys => k,
            MapIterKind::Values => v,
            MapIterKind::Entries => {
                let pair =
                    ctx.array_from_elements_with_roots([k, v], &[], &[snapshot.as_slice()])?;
                Value::Array(pair)
            }
        });
    }
    let arr = ctx.array_from_elements(snapshot)?;
    Ok(crate::IteratorState::Array {
        array: arr,
        index: 0,
    })
}

fn make_native_iter_value(
    ctx: &mut crate::NativeCtx<'_>,
    state: crate::IteratorState,
) -> Result<Value, otter_gc::OutOfMemory> {
    Ok(Value::Iterator(ctx.alloc_iterator_state(
        state,
        &[],
        &[],
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::number::NumberValue;
    use crate::string::StringHeap;

    #[test]
    fn map_set_uses_intrinsic_rooted_reservation() {
        let strings = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let map = collections::alloc_map(&mut gc_heap).expect("map");
        let receiver = Value::Map(map);
        let args = [
            Value::Number(NumberValue::from_i32(1)),
            Value::Number(NumberValue::from_i32(2)),
        ];
        let before = gc_heap.stats().reserved_bytes;

        let result = impl_map_set(&mut IntrinsicArgs {
            receiver: &receiver,
            args: &args,
            string_heap: &strings,
            gc_heap: &mut gc_heap,
            allocation_roots: &[],
        })
        .expect("map set");

        let after = gc_heap.stats().reserved_bytes;
        assert!(
            after > before,
            "Map.prototype.set should reserve backing storage through intrinsic roots"
        );
        assert!(matches!(result, Value::Map(_)));
    }

    #[test]
    fn set_add_uses_intrinsic_rooted_reservation() {
        let strings = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let set = collections::alloc_set(&mut gc_heap).expect("set");
        let receiver = Value::Set(set);
        let args = [Value::Number(NumberValue::from_i32(3))];
        let before = gc_heap.stats().reserved_bytes;

        let result = impl_set_add(&mut IntrinsicArgs {
            receiver: &receiver,
            args: &args,
            string_heap: &strings,
            gc_heap: &mut gc_heap,
            allocation_roots: &[],
        })
        .expect("set add");

        let after = gc_heap.stats().reserved_bytes;
        assert!(
            after > before,
            "Set.prototype.add should reserve backing storage through intrinsic roots"
        );
        assert!(matches!(result, Value::Set(_)));
    }
}
