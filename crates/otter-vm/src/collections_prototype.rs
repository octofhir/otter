//! Collection prototype metadata and property helpers.
//!
//! Executable `Map` / `Set` / `WeakMap` / `WeakSet` prototype
//! methods are installed by [`crate::bootstrap_collections`] through
//! the `couch!` native surface. This module keeps only the direct-call
//! guards and non-method property helpers that are still shared by
//! property dispatch.
//!
//! # Contents
//! - `is_*_builtin_method` — `CallMethodValue` guard predicates.
//! - [`load_property_with_heap`] — `size` accessor fast path.
//! - Iterator factory helpers for collection `@@iterator` property
//!   reads.
//!
//! # Invariants
//! - JS-visible methods come from the bootstrap/native surface.
//! - `WeakMap` / `WeakSet` deliberately expose no `size` accessor.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-map-prototype-object>
//! - <https://tc39.es/ecma262/#sec-set-prototype-object>
//! - <https://tc39.es/ecma262/#sec-weakmap-prototype-object>
//! - <https://tc39.es/ecma262/#sec-weakset-prototype-object>

use crate::Value;
use crate::collections::{self, JsMap, JsSet};
use smallvec::SmallVec;

#[derive(Debug, Clone, Copy)]
enum MapIterKind {
    Entries,
}

/// Whether `name` is installed on `Map.prototype`.
#[must_use]
pub fn is_map_builtin_method(name: &str) -> bool {
    matches!(
        name,
        "get" | "set" | "has" | "delete" | "clear" | "keys" | "values" | "entries" | "forEach"
    )
}

/// Whether `name` is installed on `Set.prototype`.
#[must_use]
pub fn is_set_builtin_method(name: &str) -> bool {
    matches!(
        name,
        "add"
            | "has"
            | "delete"
            | "clear"
            | "keys"
            | "values"
            | "entries"
            | "forEach"
            | "union"
            | "intersection"
            | "difference"
            | "symmetricDifference"
            | "isSubsetOf"
            | "isSupersetOf"
            | "isDisjointFrom"
    )
}

/// Whether `name` is installed on `WeakMap.prototype`.
#[must_use]
pub fn is_weak_map_builtin_method(name: &str) -> bool {
    matches!(name, "get" | "set" | "has" | "delete")
}

/// Whether `name` is installed on `WeakSet.prototype`.
#[must_use]
pub fn is_weak_set_builtin_method(name: &str) -> bool {
    matches!(name, "add" | "has" | "delete")
}

/// Read a non-method property off a collection receiver. Foundation
/// exposes only the heap-aware `size` accessor through
/// [`load_property_with_heap`].
#[must_use]
pub fn load_property(value: &Value, name: &str) -> Value {
    let _ = (value, name);
    Value::undefined()
}

/// Heap-aware version of [`load_property`].
#[must_use]
pub fn load_property_with_heap(value: &Value, name: &str, heap: &otter_gc::GcHeap) -> Value {
    if name != "size" {
        return Value::undefined();
    }
    if let Some(m) = value.as_map() {
        return Value::number_i32(collections::map_len(m, heap) as i32);
    }
    if let Some(s) = value.as_set() {
        return Value::number_i32(collections::set_len(s, heap) as i32);
    }
    Value::undefined()
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
        smallvec::smallvec![Value::map(map)],
        |ctx, _, captures| {
            let Some(map) = captures.first().and_then(|v| v.as_map()) else {
                return Err(crate::NativeError::TypeError {
                    name: "Map[Symbol.iterator]",
                    reason: "missing traced map capture".to_string(),
                });
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
        smallvec::smallvec![Value::set(set)],
        |ctx, _, captures| {
            let Some(set) = captures.first().and_then(|v| v.as_set()) else {
                return Err(crate::NativeError::TypeError {
                    name: "Set[Symbol.iterator]",
                    reason: "missing traced set capture".to_string(),
                });
            };
            Ok(make_native_iter_value(
                ctx,
                crate::IteratorState::SetCollection {
                    set,
                    index: 0,
                    kind: crate::SetIteratorKind::Value,
                },
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
            MapIterKind::Entries => {
                let pair =
                    ctx.array_from_elements_with_roots([k, v], &[], &[snapshot.as_slice()])?;
                Value::array(pair)
            }
        });
    }
    let arr = ctx.array_from_elements(snapshot)?;
    Ok(crate::IteratorState::Array {
        array: arr,
        index: 0,
        origin: crate::BuiltinIteratorOrigin::Map,
    })
}

fn make_native_iter_value(
    ctx: &mut crate::NativeCtx<'_>,
    state: crate::IteratorState,
) -> Result<Value, otter_gc::OutOfMemory> {
    Ok(Value::iterator(ctx.alloc_iterator_state(
        state,
        &[],
        &[],
    )?))
}
