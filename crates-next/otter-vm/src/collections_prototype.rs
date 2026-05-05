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

use crate::collections::{self, CollectionError, JsMap, JsSet, JsWeakMap, JsWeakSet};
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::number::NumberValue;
use crate::{Value, native_value};
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

fn impl_map_get(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_map(args)?;
    let key = args.args.first().cloned().unwrap_or(Value::Undefined);
    let heap = args.gc_heap.borrow();
    Ok(collections::map_get(m, &heap, &key).unwrap_or(Value::Undefined))
}

fn impl_map_set(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_map(args)?;
    let key = args.args.first().cloned().unwrap_or(Value::Undefined);
    let value = args.args.get(1).cloned().unwrap_or(Value::Undefined);
    let mut heap = args.gc_heap.borrow_mut();
    collections::map_set(m, &mut heap, key, value)?;
    Ok(Value::Map(m))
}

fn impl_map_has(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_map(args)?;
    let key = args.args.first().cloned().unwrap_or(Value::Undefined);
    let heap = args.gc_heap.borrow();
    Ok(Value::Boolean(collections::map_has(m, &heap, &key)))
}

fn impl_map_delete(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_map(args)?;
    let key = args.args.first().cloned().unwrap_or(Value::Undefined);
    let mut heap = args.gc_heap.borrow_mut();
    Ok(Value::Boolean(collections::map_delete(m, &mut heap, &key)))
}

fn impl_map_clear(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_map(args)?;
    let mut heap = args.gc_heap.borrow_mut();
    collections::map_clear(m, &mut heap);
    Ok(Value::Undefined)
}

/// `Map.prototype.keys` — returns a foundation iterator factory
/// closure-bearing native function. Spec §24.1.3.8.
fn impl_map_keys(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_map(args)?;
    let mut heap = args.gc_heap.borrow_mut();
    Ok(make_iter_value(map_iter_state(
        MapIterKind::Keys,
        m,
        &mut heap,
    )?))
}

/// `Map.prototype.values` — Spec §24.1.3.10.
fn impl_map_values(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_map(args)?;
    let mut heap = args.gc_heap.borrow_mut();
    Ok(make_iter_value(map_iter_state(
        MapIterKind::Values,
        m,
        &mut heap,
    )?))
}

/// `Map.prototype.entries` — Spec §24.1.3.4.
fn impl_map_entries(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_map(args)?;
    let mut heap = args.gc_heap.borrow_mut();
    Ok(make_iter_value(map_iter_state(
        MapIterKind::Entries,
        m,
        &mut heap,
    )?))
}

#[derive(Debug, Clone, Copy)]
enum MapIterKind {
    Keys,
    Values,
    Entries,
}

fn map_iter_state(
    kind: MapIterKind,
    m: JsMap,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<crate::IteratorState, otter_gc::OutOfMemory> {
    let entries = collections::map_entries(m, gc_heap);
    let mut snapshot: SmallVec<[Value; 4]> = SmallVec::with_capacity(entries.len());
    for (k, v) in entries {
        snapshot.push(match kind {
            MapIterKind::Keys => k,
            MapIterKind::Values => v,
            MapIterKind::Entries => {
                let mut sv: SmallVec<[Value; 4]> = SmallVec::new();
                sv.push(k);
                sv.push(v);
                Value::Array(crate::array::from_elements(gc_heap, sv)?)
            }
        });
    }
    let arr = crate::array::from_elements(gc_heap, snapshot)?;
    Ok(crate::IteratorState::Array {
        array: arr,
        index: 0,
    })
}

fn make_iter_value(state: crate::IteratorState) -> Value {
    Value::Iterator(std::rc::Rc::new(std::cell::RefCell::new(state)))
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

fn impl_set_add(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_set(args)?;
    let v = args.args.first().cloned().unwrap_or(Value::Undefined);
    let mut heap = args.gc_heap.borrow_mut();
    collections::set_add(s, &mut heap, v)?;
    Ok(Value::Set(s))
}

fn impl_set_has(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_set(args)?;
    let v = args.args.first().cloned().unwrap_or(Value::Undefined);
    let heap = args.gc_heap.borrow();
    Ok(Value::Boolean(collections::set_has(s, &heap, &v)))
}

fn impl_set_delete(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_set(args)?;
    let v = args.args.first().cloned().unwrap_or(Value::Undefined);
    let mut heap = args.gc_heap.borrow_mut();
    Ok(Value::Boolean(collections::set_delete(s, &mut heap, &v)))
}

fn impl_set_clear(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_set(args)?;
    let mut heap = args.gc_heap.borrow_mut();
    collections::set_clear(s, &mut heap);
    Ok(Value::Undefined)
}

fn impl_set_values(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_set(args)?;
    let mut heap = args.gc_heap.borrow_mut();
    let snap: SmallVec<[Value; 4]> = collections::set_values(s, &heap).into_iter().collect();
    Ok(make_iter_value(crate::IteratorState::Array {
        array: crate::array::from_elements(&mut heap, snap)?,
        index: 0,
    }))
}

/// `Set.prototype.keys` is the same as `values` per spec
/// §24.2.3.8 / §24.2.3.10.
fn impl_set_keys(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    impl_set_values(args)
}

/// `Set.prototype.entries` — yields `[v, v]` pairs per
/// §24.2.3.5.
fn impl_set_entries(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_set(args)?;
    let mut heap = args.gc_heap.borrow_mut();
    let mut snap: SmallVec<[Value; 4]> = SmallVec::new();
    for v in collections::set_values(s, &heap) {
        let pair = crate::array::from_elements(&mut heap, [v.clone(), v])?;
        snap.push(Value::Array(pair));
    }
    Ok(make_iter_value(crate::IteratorState::Array {
        array: crate::array::from_elements(&mut heap, snap)?,
        index: 0,
    }))
}

// ---------------------------------------------------------------
// WeakMap.prototype
// ---------------------------------------------------------------

fn receiver_weak_map<'a>(args: &'a IntrinsicArgs<'_>) -> Result<&'a JsWeakMap, IntrinsicError> {
    match args.receiver {
        Value::WeakMap(m) => Ok(m),
        _ => Err(IntrinsicError::BadReceiver {
            expected: "WeakMap",
        }),
    }
}

fn impl_weak_map_get(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_weak_map(args)?;
    let key = args.args.first().cloned().unwrap_or(Value::Undefined);
    match m.get(&key) {
        Ok(Some(v)) => Ok(v),
        Ok(None) | Err(CollectionError::NonObjectKey) => Ok(Value::Undefined),
        Err(_) => Ok(Value::Undefined),
    }
}

fn impl_weak_map_has(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_weak_map(args)?;
    let key = args.args.first().cloned().unwrap_or(Value::Undefined);
    match m.has(&key) {
        Ok(b) => Ok(Value::Boolean(b)),
        Err(CollectionError::NonObjectKey) => Ok(Value::Boolean(false)),
        Err(_) => Ok(Value::Boolean(false)),
    }
}

fn impl_weak_map_set(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_weak_map(args)?;
    let key = args.args.first().cloned().unwrap_or(Value::Undefined);
    let value = args.args.get(1).cloned().unwrap_or(Value::Undefined);
    m.set(key, value).map_err(weak_collection_to_intrinsic)?;
    Ok(Value::WeakMap(m.clone()))
}

fn impl_weak_map_delete(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let m = receiver_weak_map(args)?;
    let key = args.args.first().cloned().unwrap_or(Value::Undefined);
    match m.delete(&key) {
        Ok(b) => Ok(Value::Boolean(b)),
        Err(CollectionError::NonObjectKey) => Ok(Value::Boolean(false)),
        Err(_) => Ok(Value::Boolean(false)),
    }
}

// ---------------------------------------------------------------
// WeakSet.prototype
// ---------------------------------------------------------------

fn receiver_weak_set<'a>(args: &'a IntrinsicArgs<'_>) -> Result<&'a JsWeakSet, IntrinsicError> {
    match args.receiver {
        Value::WeakSet(s) => Ok(s),
        _ => Err(IntrinsicError::BadReceiver {
            expected: "WeakSet",
        }),
    }
}

fn impl_weak_set_add(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_weak_set(args)?;
    let v = args.args.first().cloned().unwrap_or(Value::Undefined);
    s.add(v).map_err(weak_collection_to_intrinsic)?;
    Ok(Value::WeakSet(s.clone()))
}

fn impl_weak_set_has(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_weak_set(args)?;
    let v = args.args.first().cloned().unwrap_or(Value::Undefined);
    match s.has(&v) {
        Ok(b) => Ok(Value::Boolean(b)),
        Err(CollectionError::NonObjectKey) => Ok(Value::Boolean(false)),
        Err(_) => Ok(Value::Boolean(false)),
    }
}

fn impl_weak_set_delete(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = receiver_weak_set(args)?;
    let v = args.args.first().cloned().unwrap_or(Value::Undefined);
    match s.delete(&v) {
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
#[must_use]
pub fn make_map_iterator_factory(map: JsMap) -> Value {
    native_value("Map[Symbol.iterator]", move |vm, _| {
        Ok(make_iter_value(map_iter_state(
            MapIterKind::Entries,
            map,
            vm.gc_heap_mut(),
        )?))
    })
}

/// Build the native callable that `Set.prototype[Symbol.iterator]`
/// resolves to (alias of `values`, Spec §24.2.3.11).
#[must_use]
pub fn make_set_iterator_factory(set: JsSet) -> Value {
    native_value("Set[Symbol.iterator]", move |vm, _| {
        let snap: SmallVec<[Value; 4]> = collections::set_values(set, vm.gc_heap())
            .into_iter()
            .collect();
        Ok(make_iter_value(crate::IteratorState::Array {
            array: crate::array::from_elements(vm.gc_heap_mut(), snap)?,
            index: 0,
        }))
    })
}
