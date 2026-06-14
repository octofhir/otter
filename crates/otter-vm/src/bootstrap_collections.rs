//! ECMA-262 §24 Keyed Collections bootstrap installers.
//!
//! Installs the JS-visible `Map`, `Set`, `WeakMap`, and `WeakSet`
//! constructor + prototype pairs into `globalThis`. Each constructor
//! is a real callable [`crate::native_function::NativeFunction`]
//! with `[[Construct]]`; the prototype object carries own data
//! properties for every spec-listed method, plus a `size` accessor
//! on `Map.prototype` / `Set.prototype` and a `constructor` back
//! pointer. `@@toStringTag` and `@@iterator` are installed by
//! [`install_collection_well_knowns_post_bootstrap`] once the
//! per-realm [`crate::WellKnownSymbols`] table exists.
//!
//! # Contents
//! - [`install_map`] / [`install_set`] /
//!   [`install_weak_map`] / [`install_weak_set`] — bootstrap entries.
//! - [`install_collection_well_knowns_post_bootstrap`] — symbol fixup.
//!
//! # Invariants
//! - Constructors throw a `TypeError` when called without `new`.
//! - `Map.prototype` / `Set.prototype` chain to `%Object.prototype%`.
//! - Seed iteration calls `Map.prototype.set` (or
//!   `Set.prototype.add`) via property lookup, so user overrides on
//!   the prototype are honoured per §24.1.1.2 AddEntriesFromIterable.
//! - On abrupt completion from the adder, the iterator is closed via
//!   §7.4.8 IteratorClose before the abrupt propagates.
//! - `WeakMap` / `WeakSet` reject primitive keys with `TypeError`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-map-constructor>
//! - <https://tc39.es/ecma262/#sec-set-constructor>
//! - <https://tc39.es/ecma262/#sec-weakmap-constructor>
//! - <https://tc39.es/ecma262/#sec-weakset-constructor>
//! - <https://tc39.es/ecma262/#sec-add-entries-from-iterable>

use smallvec::SmallVec;

use crate::collections::{self, CollectionError};
use crate::js_surface::JsSurfaceError;
use crate::object::{self, JsObject, PartialPropertyDescriptor};
use crate::{
    NativeCtx, NativeError, Value, VmError, VmGetOutcome, VmPropertyKey, descriptor_value,
};

// ---------------------------------------------------------------
// Public bootstrap install entry points
// ---------------------------------------------------------------

// Four collection intrinsics — Map / Set / WeakMap / WeakSet. Each
// gets its own `couch!` invocation. Shared `install_collection`
// helper deleted along with the per-kind `match` in
// `install_prototype_methods`; the macro emits the right install
// body inline per call.
//
// `size` accessors on Map / Set declared inline via the `accessors`
// field; `Map.groupBy` static spec'd here; `entries → @@iterator`
// and `keys → values` alias fixups (identity-preserving) stay in
// `install_collection_well_knowns_post_bootstrap`.

otter_macros::couch! {
    name = "Map",
    string_tag = "Map",
    feature = CORE,
    intrinsic = MapIntrinsic,
    constructor = (length = 0, call = map_ctor_call),
    statics = {
        "groupBy" / 2 => map_group_by_native,
    },
    prototype = {
        methods = {
            "get"     / 1 => map_proto_get,
            "set"     / 2 => map_proto_set,
            "has"     / 1 => map_proto_has,
            "delete"  / 1 => map_proto_delete,
            "clear"   / 0 => map_proto_clear,
            "keys"    / 0 => map_proto_keys,
            "values"  / 0 => map_proto_values,
            "entries" / 0 => map_proto_entries,
            "forEach" / 1 => map_proto_for_each,
            "getOrInsert"         / 2 => map_proto_get_or_insert,
            "getOrInsertComputed" / 2 => map_proto_get_or_insert_computed,
        },
        accessors = [
            ("size", get = map_size_get),
        ],
    },
}

otter_macros::couch! {
    name = "Set",
    string_tag = "Set",
    feature = CORE,
    intrinsic = SetIntrinsic,
    constructor = (length = 0, call = set_ctor_call),
    prototype = {
        methods = {
            "add"                 / 1 => set_proto_add,
            "has"                 / 1 => set_proto_has,
            "delete"              / 1 => set_proto_delete,
            "clear"               / 0 => set_proto_clear,
            "keys"                / 0 => set_proto_keys,
            "values"              / 0 => set_proto_values,
            "entries"             / 0 => set_proto_entries,
            "forEach"             / 1 => set_proto_for_each,
            "union"               / 1 => set_proto_union,
            "intersection"        / 1 => set_proto_intersection,
            "difference"          / 1 => set_proto_difference,
            "symmetricDifference" / 1 => set_proto_symmetric_difference,
            "isSubsetOf"          / 1 => set_proto_is_subset_of,
            "isSupersetOf"        / 1 => set_proto_is_superset_of,
            "isDisjointFrom"      / 1 => set_proto_is_disjoint_from,
        },
        accessors = [
            ("size", get = set_size_get),
        ],
    },
}

otter_macros::couch! {
    name = "WeakMap",
    string_tag = "WeakMap",
    feature = CORE,
    intrinsic = WeakMapIntrinsic,
    constructor = (length = 0, call = weak_map_ctor_call),
    prototype = {
        methods = {
            "get"    / 1 => weak_map_proto_get,
            "set"    / 2 => weak_map_proto_set,
            "has"    / 1 => weak_map_proto_has,
            "delete" / 1 => weak_map_proto_delete,
            "getOrInsert"         / 2 => weak_map_proto_get_or_insert,
            "getOrInsertComputed" / 2 => weak_map_proto_get_or_insert_computed,
        },
    },
}

otter_macros::couch! {
    name = "WeakSet",
    string_tag = "WeakSet",
    feature = CORE,
    intrinsic = WeakSetIntrinsic,
    constructor = (length = 0, call = weak_set_ctor_call),
    prototype = {
        methods = {
            "add"    / 1 => weak_set_proto_add,
            "has"    / 1 => weak_set_proto_has,
            "delete" / 1 => weak_set_proto_delete,
        },
    },
}

/// Post-bootstrap fixup: install `@@toStringTag` on each
/// collection prototype, and `@@iterator` on `Map.prototype` /
/// `Set.prototype` (aliased to `entries` / `values` per
/// §24.1.3.12 / §24.2.3.11).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-map.prototype-@@iterator>
/// - <https://tc39.es/ecma262/#sec-set.prototype-@@iterator>
/// - <https://tc39.es/ecma262/#sec-map.prototype-@@tostringtag>
/// - <https://tc39.es/ecma262/#sec-set.prototype-@@tostringtag>
pub fn install_collection_well_knowns_post_bootstrap(
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    use crate::symbol::WellKnown;

    let iterator_sym = well_known.get(WellKnown::Iterator);

    for (ctor_name, alias_method, alias_kind) in [
        ("Map", Some("entries"), Some(CollectionKind::Map)),
        ("Set", Some("values"), Some(CollectionKind::Set)),
        ("WeakMap", None, None),
        ("WeakSet", None, None),
    ] {
        let Some(prototype) = ctor_prototype(global, heap, ctor_name) else {
            continue;
        };
        // §24.1.3.12 / §24.2.3.11 — `@@iterator` aliases `entries`
        // (Map) or `values` (Set). Same NativeFunction value so
        // identity (`Map.prototype.entries === Map.prototype[@@iterator]`)
        // is preserved.
        if let (Some(method_name), Some(_)) = (alias_method, alias_kind)
            && let Some(method_value) = object::get(prototype, heap, method_name)
        {
            object::define_own_symbol_property_partial(
                prototype,
                heap,
                iterator_sym,
                PartialPropertyDescriptor {
                    value: Some(method_value),
                    writable: Some(true),
                    enumerable: Some(false),
                    configurable: Some(true),
                    ..Default::default()
                },
            );
        }
        // §24.2.3.6 — `Set.prototype.keys` is the same function
        // object as `Set.prototype.values`. The individual `method`
        // builder allocates a fresh NativeFunction per name; overwrite
        // `keys` to point at the same value so `Set.prototype.keys
        // === Set.prototype.values`.
        if matches!(alias_kind, Some(CollectionKind::Set))
            && let Some(values_value) = object::get(prototype, heap, "values")
        {
            object::define_own_property_partial(
                prototype,
                heap,
                "keys",
                PartialPropertyDescriptor {
                    value: Some(values_value),
                    writable: Some(true),
                    enumerable: Some(false),
                    configurable: Some(true),
                    ..Default::default()
                },
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CollectionKind {
    Map,
    Set,
    WeakMap,
    WeakSet,
}

impl CollectionKind {
    const fn name(self) -> &'static str {
        match self {
            Self::Map => "Map",
            Self::Set => "Set",
            Self::WeakMap => "WeakMap",
            Self::WeakSet => "WeakSet",
        }
    }

    const fn adder_name(self) -> &'static str {
        match self {
            Self::Map | Self::WeakMap => "set",
            Self::Set | Self::WeakSet => "add",
        }
    }

    const fn is_pair(self) -> bool {
        matches!(self, Self::Map | Self::WeakMap)
    }
}

fn ctor_prototype(
    global: JsObject,
    heap: &mut otter_gc::GcHeap,
    ctor_name: &str,
) -> Option<JsObject> {
    let f = object::get(global, heap, ctor_name)?.as_native_function()?;
    let descriptor = f
        .own_property_descriptor(&mut *heap, "prototype")
        .ok()
        .flatten()?;
    match descriptor.kind {
        crate::object::DescriptorKind::Data { value } => value.as_object(),
        _ => None,
    }
}

/// §24.1.2.1 `Map.groupBy(items, callbackfn)` — drain `items`
/// into groups keyed by callback return value, store result in
/// a new Map.
fn map_group_by_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let items = args.first().cloned().unwrap_or(Value::undefined());
    let callback = args.get(1).cloned().unwrap_or(Value::undefined());
    if items.is_undefined() || items.is_null() {
        return Err(NativeError::TypeError {
            name: "Map.groupBy",
            reason: "items must be iterable".to_string(),
        });
    }
    if !ctx.cx.interp.is_callable_runtime(&callback) {
        return Err(NativeError::TypeError {
            name: "Map.groupBy",
            reason: "callback must be a function".to_string(),
        });
    }
    let exec_ctx = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "Map.groupBy",
            reason: "missing execution context".to_string(),
        })?;
    let items_snapshot = ctx
        .cx
        .interp
        .iterator_to_list_sync(&exec_ctx, &items)
        .map_err(map_group_by_vm_error)?;
    let result = ctx.alloc_map().map_err(|_| NativeError::TypeError {
        name: "Map.groupBy",
        reason: "out of memory".to_string(),
    })?;
    let result_value = Value::map(result);
    for (idx, item) in items_snapshot.iter().enumerate() {
        let mut cb_args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
        cb_args.push(*item);
        cb_args.push(Value::number(crate::number::NumberValue::from_f64(
            idx as f64,
        )));
        let key = ctx
            .cx
            .interp
            .run_callable_sync(&exec_ctx, &callback, Value::undefined(), cb_args)
            .map_err(map_group_by_vm_error)?;
        let existing = crate::collections::map_get(result, ctx.heap(), &key);
        let group_arr = if let Some(arr) = existing.and_then(|v| v.as_array()) {
            arr
        } else {
            let arr = ctx
                .array_from_elements_with_roots(
                    std::iter::empty(),
                    &[&result_value, &key, item],
                    &[items_snapshot.as_slice()],
                )
                .map_err(|_| NativeError::TypeError {
                    name: "Map.groupBy",
                    reason: "out of memory".to_string(),
                })?;
            crate::collections::map_set(result, ctx.heap_mut(), key, Value::array(arr)).map_err(
                |_| NativeError::TypeError {
                    name: "Map.groupBy",
                    reason: "out of memory".to_string(),
                },
            )?;
            arr
        };
        let arr_value = Value::array(group_arr);
        let len = crate::array::len(group_arr, ctx.heap());
        let roots = ctx.collect_native_roots();
        let item_clone = *item;
        let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            arr_value.trace_value_slots(visitor);
            item_clone.trace_value_slots(visitor);
        };
        crate::array::set_with_roots(group_arr, ctx.heap_mut(), len, *item, &mut visit).map_err(
            |_| NativeError::TypeError {
                name: "Map.groupBy",
                reason: "out of memory".to_string(),
            },
        )?;
    }
    Ok(result_value)
}

fn map_group_by_vm_error(err: crate::VmError) -> NativeError {
    match err {
        crate::VmError::Uncaught { value } => NativeError::Thrown {
            name: "Map.groupBy",
            message: value,
        },
        other => NativeError::TypeError {
            name: "Map.groupBy",
            reason: other.to_string(),
        },
    }
}

// ---------------------------------------------------------------
// Constructor bodies
// ---------------------------------------------------------------

fn map_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    construct_collection(ctx, args, CollectionKind::Map)
}

fn set_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    construct_collection(ctx, args, CollectionKind::Set)
}

fn weak_map_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    construct_collection(ctx, args, CollectionKind::WeakMap)
}

fn weak_set_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    construct_collection(ctx, args, CollectionKind::WeakSet)
}

fn construct_collection(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    kind: CollectionKind,
) -> Result<Value, NativeError> {
    let name = kind.name();
    if !ctx.is_construct_call() {
        return Err(NativeError::TypeError {
            name,
            reason: format!("constructor {name} requires 'new'"),
        });
    }
    let target = alloc_collection(ctx, kind)?;
    apply_collection_new_target_prototype(ctx, &target, kind)?;
    let iterable = args.first().cloned().unwrap_or(Value::undefined());
    if iterable.is_undefined() || iterable.is_null() {
        return Ok(target);
    }
    add_entries_from_iterable(ctx, &target, &iterable, kind)?;
    Ok(target)
}

fn alloc_collection(ctx: &mut NativeCtx<'_>, kind: CollectionKind) -> Result<Value, NativeError> {
    let name = kind.name();
    match kind {
        CollectionKind::Map => ctx.alloc_map().map(Value::map).map_err(|_| oom(name)),
        CollectionKind::Set => ctx.alloc_set().map(Value::set).map_err(|_| oom(name)),
        CollectionKind::WeakMap => ctx
            .alloc_weak_map()
            .map(Value::weak_map)
            .map_err(|_| oom(name)),
        CollectionKind::WeakSet => ctx
            .alloc_weak_set()
            .map(Value::weak_set)
            .map_err(|_| oom(name)),
    }
}

fn apply_collection_new_target_prototype(
    ctx: &mut NativeCtx<'_>,
    target: &Value,
    kind: CollectionKind,
) -> Result<(), NativeError> {
    let Some(new_target) = ctx.new_target().cloned() else {
        return Ok(());
    };
    let proto = if let Some(class) = new_target.as_class_constructor() {
        Some(Value::object(class.prototype(ctx.heap())))
    } else if let Some(obj) = new_target.as_object() {
        object::get(obj, ctx.heap(), "prototype")
            .filter(|value| value.is_object_type() || value.is_proxy())
    } else if let Some(native) = new_target.as_native_function() {
        native
            .own_property_descriptor(ctx.heap_mut(), "prototype")
            .map_err(|err| NativeError::TypeError {
                name: kind.name(),
                reason: err.to_string(),
            })?
            .map(|descriptor| descriptor_value(&descriptor))
            .filter(|value| value.is_object_type() || value.is_proxy())
    } else {
        None
    };
    let Some(proto) = proto else {
        return Ok(());
    };
    if let Some(map) = target.as_map() {
        collections::set_map_prototype_override(map, ctx.heap_mut(), Some(proto));
    } else if let Some(set) = target.as_set() {
        collections::set_set_prototype_override(set, ctx.heap_mut(), Some(proto));
    } else if let Some(map) = target.as_weak_map() {
        collections::set_weak_map_prototype_override(map, ctx.heap_mut(), Some(proto));
    } else if let Some(set) = target.as_weak_set() {
        collections::set_weak_set_prototype_override(set, ctx.heap_mut(), Some(proto));
    }
    Ok(())
}

/// §24.1.1.2 AddEntriesFromIterable — fetch the adder method via
/// `Get`, walk the iterable via §7.4 protocol, call the adder on
/// each entry. On abrupt completion the iterator is closed.
///
/// Two paths: built-in iterables (`Array` / `Map` / `Set` /
/// `Generator` / `String`) hit
/// [`crate::Interpreter::iterator_to_list_sync`]'s fast paths,
/// which materialise the entries before any adder call. User-
/// defined iterables (`Value::Object` carrying `@@iterator`) go
/// through `GetIterator` / `IteratorStep` lazily so spec tests
/// like `iterator-close-after-set-failure.js` observe the
/// iterator-close ladder.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-add-entries-from-iterable>
fn add_entries_from_iterable(
    ctx: &mut NativeCtx<'_>,
    target: &Value,
    iterable: &Value,
    kind: CollectionKind,
) -> Result<(), NativeError> {
    let ctor_name = kind.name();
    let context = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: ctor_name,
            reason: "no active execution context".to_string(),
        })?;

    // Spec step 6 — adder = ? Get(target, kind.adder_name())
    let adder_name = kind.adder_name();
    let adder = {
        let interp = ctx.interp_mut();
        let outcome = interp
            .ordinary_get_value(
                &context,
                *target,
                *target,
                &VmPropertyKey::String(adder_name),
                0,
            )
            .map_err(|e| vm_to_native(e, ctor_name))?;
        match outcome {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => interp
                .run_callable_sync(&context, &getter, *target, SmallVec::new())
                .map_err(|e| vm_to_native(e, ctor_name))?,
        }
    };
    if !ctx.interp_mut().is_callable_runtime(&adder) {
        return Err(NativeError::TypeError {
            name: ctor_name,
            reason: format!("{adder_name} method is not callable"),
        });
    }

    if iterable_uses_fast_materialization(iterable) {
        return add_entries_eager(ctx, &context, target, iterable, kind, &adder);
    }
    add_entries_lazy(ctx, &context, target, iterable, kind, &adder)
}

/// `true` when the iterable matches one of
/// [`crate::Interpreter::iterator_to_list_sync`]'s fast-path
/// branches.
fn iterable_uses_fast_materialization(iterable: &Value) -> bool {
    iterable.is_array()
        || iterable.is_string()
        || iterable.is_map()
        || iterable.is_set()
        || iterable.is_generator()
}

fn add_entries_eager(
    ctx: &mut NativeCtx<'_>,
    context: &crate::ExecutionContext,
    target: &Value,
    iterable: &Value,
    kind: CollectionKind,
    adder: &Value,
) -> Result<(), NativeError> {
    let ctor_name = kind.name();
    let entries = {
        let interp = ctx.interp_mut();
        interp
            .iterator_to_list_sync(context, iterable)
            .map_err(|e| vm_to_native(e, ctor_name))?
    };
    for next in entries {
        let call_args = build_adder_args(ctx, context, &next, kind, None)?;
        let interp = ctx.interp_mut();
        interp
            .run_callable_sync(context, adder, *target, call_args)
            .map_err(|e| vm_to_native(e, ctor_name))?;
    }
    Ok(())
}

fn add_entries_lazy(
    ctx: &mut NativeCtx<'_>,
    context: &crate::ExecutionContext,
    target: &Value,
    iterable: &Value,
    kind: CollectionKind,
    adder: &Value,
) -> Result<(), NativeError> {
    let ctor_name = kind.name();
    let (iterator, next_method) = {
        let interp = ctx.interp_mut();
        interp
            .get_iterator_sync(context, iterable)
            .map_err(|e| vm_to_native(e, ctor_name))?
    };

    loop {
        let stepped = {
            let interp = ctx.interp_mut();
            interp.iterator_step_sync(context, &iterator, &next_method)
        };
        let next = match stepped {
            Ok(Some(value)) => value,
            Ok(None) => return Ok(()),
            Err(err) => return Err(vm_to_native(err, ctor_name)),
        };

        let call_args = build_adder_args(ctx, context, &next, kind, Some(&iterator))?;

        let call_result = {
            let interp = ctx.interp_mut();
            interp.run_callable_sync(context, adder, *target, call_args)
        };
        if let Err(err) = call_result {
            let original_throw = ctx.interp_mut().take_pending_uncaught_throw();
            let _ = ctx.interp_mut().iterator_close_sync(context, &iterator);
            if let Some(value) = original_throw {
                ctx.interp_mut().set_pending_uncaught_throw(value);
            }
            return Err(vm_to_native(err, ctor_name));
        }
    }
}

/// Convert one iterator value into the `[k, v]` (Map/WeakMap) or
/// `[v]` (Set/WeakSet) argument list for the adder. When
/// `iterator_for_close` is provided, the iterator is closed before
/// returning an abrupt completion.
fn build_adder_args(
    ctx: &mut NativeCtx<'_>,
    context: &crate::ExecutionContext,
    next: &Value,
    kind: CollectionKind,
    iterator_for_close: Option<&Value>,
) -> Result<SmallVec<[Value; 8]>, NativeError> {
    if !kind.is_pair() {
        return Ok(smallvec::smallvec![*next]);
    }
    let ctor_name = kind.name();
    if !value_is_object_like(next) {
        if let Some(iterator) = iterator_for_close {
            let _ = ctx.interp_mut().iterator_close_sync(context, iterator);
        }
        return Err(NativeError::TypeError {
            name: ctor_name,
            reason: "iterator value is not an object".to_string(),
        });
    }
    let key = match read_indexed_property(ctx, context, next, "0") {
        Ok(v) => v,
        Err(err) => {
            if let Some(iterator) = iterator_for_close {
                let original_throw = ctx.interp_mut().take_pending_uncaught_throw();
                let _ = ctx.interp_mut().iterator_close_sync(context, iterator);
                if let Some(value) = original_throw {
                    ctx.interp_mut().set_pending_uncaught_throw(value);
                }
            }
            return Err(vm_to_native(err, ctor_name));
        }
    };
    let value = match read_indexed_property(ctx, context, next, "1") {
        Ok(v) => v,
        Err(err) => {
            if let Some(iterator) = iterator_for_close {
                let original_throw = ctx.interp_mut().take_pending_uncaught_throw();
                let _ = ctx.interp_mut().iterator_close_sync(context, iterator);
                if let Some(value) = original_throw {
                    ctx.interp_mut().set_pending_uncaught_throw(value);
                }
            }
            return Err(vm_to_native(err, ctor_name));
        }
    };
    Ok(smallvec::smallvec![key, value])
}

fn read_indexed_property(
    ctx: &mut NativeCtx<'_>,
    context: &crate::ExecutionContext,
    target: &Value,
    name: &str,
) -> Result<Value, VmError> {
    let interp = ctx.interp_mut();
    let outcome =
        interp.ordinary_get_value(context, *target, *target, &VmPropertyKey::String(name), 0)?;
    match outcome {
        VmGetOutcome::Value(v) => Ok(v),
        VmGetOutcome::InvokeGetter { getter } => {
            interp.run_callable_sync(context, &getter, *target, SmallVec::new())
        }
    }
}

// ---------------------------------------------------------------
// Map prototype method bodies
// ---------------------------------------------------------------

fn map_proto_get(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let m = receiver_map(ctx, "Map.prototype.get")?;
    let key = args.first().cloned().unwrap_or(Value::undefined());
    Ok(collections::map_get(m, ctx.heap(), &key).unwrap_or(Value::undefined()))
}

fn map_proto_set(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let mut m = receiver_map(ctx, "Map.prototype.set")?;
    let key = args.first().cloned().unwrap_or(Value::undefined());
    let value = args.get(1).cloned().unwrap_or(Value::undefined());
    ctx.map_set(&mut m, key, value)
        .map_err(|_| oom("Map.prototype.set"))?;
    Ok(Value::map(m))
}

fn map_proto_has(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let m = receiver_map(ctx, "Map.prototype.has")?;
    let key = args.first().cloned().unwrap_or(Value::undefined());
    Ok(Value::boolean(collections::map_has(m, ctx.heap(), &key)))
}

fn map_proto_delete(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let m = receiver_map(ctx, "Map.prototype.delete")?;
    let key = args.first().cloned().unwrap_or(Value::undefined());
    Ok(Value::boolean(collections::map_delete(
        m,
        ctx.heap_mut(),
        &key,
    )))
}

fn map_proto_clear(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let m = receiver_map(ctx, "Map.prototype.clear")?;
    collections::map_clear(m, ctx.heap_mut());
    Ok(Value::undefined())
}

fn map_proto_keys(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let m = receiver_map(ctx, "Map.prototype.keys")?;
    make_map_iterator(ctx, m, MapIterKind::Keys)
}

fn map_proto_values(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let m = receiver_map(ctx, "Map.prototype.values")?;
    make_map_iterator(ctx, m, MapIterKind::Values)
}

fn map_proto_entries(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let m = receiver_map(ctx, "Map.prototype.entries")?;
    make_map_iterator(ctx, m, MapIterKind::Entries)
}

/// CanonicalizeKeyedCollectionKey — normalize a key's `-0` to `+0`;
/// every other value passes through with its identity intact.
fn canonicalize_collection_key(key: Value) -> Value {
    if let Some(n) = key.as_number() {
        let f = n.as_f64();
        if f == 0.0 && f.is_sign_negative() {
            return Value::number(crate::number::NumberValue::from_f64(0.0));
        }
    }
    key
}

/// Map.prototype.getOrInsert(key, value) — Map.prototype.upsert
/// proposal. Returns the existing value for `key`, otherwise inserts
/// `value` and returns it.
fn map_proto_get_or_insert(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let mut m = receiver_map(ctx, "Map.prototype.getOrInsert")?;
    let key = args.first().cloned().unwrap_or(Value::undefined());
    let value = args.get(1).cloned().unwrap_or(Value::undefined());
    if let Some(existing) = collections::map_get(m, ctx.heap(), &key) {
        return Ok(existing);
    }
    ctx.map_set(&mut m, key, value)
        .map_err(|_| oom("Map.prototype.getOrInsert"))?;
    Ok(value)
}

/// Map.prototype.getOrInsertComputed(key, callbackfn) — like
/// `getOrInsert` but the value is produced by `callbackfn(key)`, called
/// only when the key is absent. The callback receives the canonicalized
/// key and may mutate the map; the final write overwrites any entry it
/// added (re-scan step), keeping the returned value authoritative.
fn map_proto_get_or_insert_computed(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let mut m = receiver_map(ctx, "Map.prototype.getOrInsertComputed")?;
    let key = args.first().cloned().unwrap_or(Value::undefined());
    let callback = args.get(1).cloned().unwrap_or(Value::undefined());
    if !ctx.interp_mut().is_callable_runtime(&callback) {
        return Err(NativeError::TypeError {
            name: "Map.prototype.getOrInsertComputed",
            reason: "callbackfn is not callable".to_string(),
        });
    }
    // Present key short-circuits before the callback runs.
    if let Some(existing) = collections::map_get(m, ctx.heap(), &key) {
        return Ok(existing);
    }
    let canonical = canonicalize_collection_key(key);
    let context = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "Map.prototype.getOrInsertComputed",
            reason: "no active execution context".to_string(),
        })?;
    let value = ctx
        .interp_mut()
        .run_callable_sync(
            &context,
            &callback,
            Value::undefined(),
            smallvec::smallvec![canonical],
        )
        .map_err(|e| vm_to_native(e, "Map.prototype.getOrInsertComputed"))?;
    // Re-scan / insert: `map_set` overwrites an entry the callback may
    // have added for `key`, else appends a fresh one.
    ctx.map_set(&mut m, key, value)
        .map_err(|_| oom("Map.prototype.getOrInsertComputed"))?;
    Ok(value)
}

/// §24.1.3.5 Map.prototype.forEach.
fn map_proto_for_each(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let m = receiver_map(ctx, "Map.prototype.forEach")?;
    let callback = args.first().cloned().unwrap_or(Value::undefined());
    if !ctx.interp_mut().is_callable_runtime(&callback) {
        return Err(NativeError::TypeError {
            name: "Map.prototype.forEach",
            reason: "callback is not callable".to_string(),
        });
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
    let context = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "Map.prototype.forEach",
            reason: "no active execution context".to_string(),
        })?;
    let map_value = Value::map(m);
    let mut index = 0;
    while index < collections::map_raw_len(m, ctx.heap()) {
        let Some((k, v)) = collections::map_entry_at(m, ctx.heap(), index) else {
            index += 1;
            continue;
        };
        index += 1;
        let interp = ctx.interp_mut();
        interp
            .run_callable_sync(
                &context,
                &callback,
                this_arg,
                smallvec::smallvec![v, k, map_value],
            )
            .map_err(|e| vm_to_native(e, "Map.prototype.forEach"))?;
    }
    Ok(Value::undefined())
}

fn map_size_get(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let m = receiver_map(ctx, "get Map.prototype.size")?;
    Ok(Value::number(crate::number::NumberValue::from_i32(
        collections::map_len(m, ctx.heap()) as i32,
    )))
}

// ---------------------------------------------------------------
// Set prototype method bodies
// ---------------------------------------------------------------

fn set_proto_add(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let mut s = receiver_set(ctx, "Set.prototype.add")?;
    let v = args.first().cloned().unwrap_or(Value::undefined());
    ctx.set_add(&mut s, v)
        .map_err(|_| oom("Set.prototype.add"))?;
    Ok(Value::set(s))
}

fn set_proto_has(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let s = receiver_set(ctx, "Set.prototype.has")?;
    let v = args.first().cloned().unwrap_or(Value::undefined());
    Ok(Value::boolean(collections::set_has(s, ctx.heap(), &v)))
}

fn set_proto_delete(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let s = receiver_set(ctx, "Set.prototype.delete")?;
    let v = args.first().cloned().unwrap_or(Value::undefined());
    Ok(Value::boolean(collections::set_delete(
        s,
        ctx.heap_mut(),
        &v,
    )))
}

fn set_proto_clear(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let s = receiver_set(ctx, "Set.prototype.clear")?;
    collections::set_clear(s, ctx.heap_mut());
    Ok(Value::undefined())
}

fn set_proto_keys(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    set_proto_values(ctx, _args)
}

fn set_proto_values(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let s = receiver_set(ctx, "Set.prototype.values")?;
    let set_value = Value::set(s);
    let iter = ctx
        .alloc_iterator_state(
            crate::IteratorState::SetCollection {
                set: s,
                index: 0,
                kind: crate::SetIteratorKind::Value,
            },
            &[&set_value],
            &[],
        )
        .map_err(|_| oom("Set.prototype.values"))?;
    Ok(Value::iterator(iter))
}

fn set_proto_entries(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let s = receiver_set(ctx, "Set.prototype.entries")?;
    let set_value = Value::set(s);
    let iter = ctx
        .alloc_iterator_state(
            crate::IteratorState::SetCollection {
                set: s,
                index: 0,
                kind: crate::SetIteratorKind::Entry,
            },
            &[&set_value],
            &[],
        )
        .map_err(|_| oom("Set.prototype.entries"))?;
    Ok(Value::iterator(iter))
}

/// §24.2.3.6 Set.prototype.forEach.
fn set_proto_for_each(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let s = receiver_set(ctx, "Set.prototype.forEach")?;
    let callback = args.first().cloned().unwrap_or(Value::undefined());
    if !ctx.interp_mut().is_callable_runtime(&callback) {
        return Err(NativeError::TypeError {
            name: "Set.prototype.forEach",
            reason: "callback is not callable".to_string(),
        });
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
    let context = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "Set.prototype.forEach",
            reason: "no active execution context".to_string(),
        })?;
    let set_value = Value::set(s);
    let mut index = 0;
    while index < collections::set_raw_len(s, ctx.heap()) {
        let Some(v) = collections::set_value_at(s, ctx.heap(), index) else {
            index += 1;
            continue;
        };
        index += 1;
        let interp = ctx.interp_mut();
        interp
            .run_callable_sync(
                &context,
                &callback,
                this_arg,
                smallvec::smallvec![v, v, set_value],
            )
            .map_err(|e| vm_to_native(e, "Set.prototype.forEach"))?;
    }
    Ok(Value::undefined())
}

/// Whether `name` belongs to the ES set-methods surface that needs
/// `GetSetRecord` and may call user-provided `has` / `keys`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-getsetrecord>
/// - <https://tc39.es/ecma262/#sec-set.prototype.union>
#[must_use]
pub(crate) fn is_set_method_name(name: &str) -> bool {
    matches!(
        name,
        "union"
            | "intersection"
            | "difference"
            | "symmetricDifference"
            | "isSubsetOf"
            | "isSupersetOf"
            | "isDisjointFrom"
    )
}

/// §24.2.4.7 `Set.prototype.union`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-set.prototype.union>
fn set_proto_union(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let this = receiver_set(ctx, "Set.prototype.union")?;
    let other = args.first().cloned().unwrap_or(Value::undefined());
    let other_rec = get_set_record(ctx, other, "Set.prototype.union")?;
    let mut result = ctx.alloc_set().map_err(|_| oom("Set.prototype.union"))?;
    for value in collections::set_values(this, ctx.heap()) {
        ctx.set_add(&mut result, value)
            .map_err(|_| oom("Set.prototype.union"))?;
    }
    let context = execution_context(ctx, "Set.prototype.union")?;
    let mut keys = set_record_keys(ctx, &context, &other_rec, "Set.prototype.union")?;
    while let Some(value) = set_record_next_key(ctx, &context, &mut keys, "Set.prototype.union")? {
        ctx.set_add(&mut result, normalize_set_key(value))
            .map_err(|_| oom("Set.prototype.union"))?;
    }
    Ok(Value::set(result))
}

/// §24.2.4.5 `Set.prototype.intersection`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-set.prototype.intersection>
fn set_proto_intersection(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let this = receiver_set(ctx, "Set.prototype.intersection")?;
    let other = args.first().cloned().unwrap_or(Value::undefined());
    let other_rec = get_set_record(ctx, other, "Set.prototype.intersection")?;
    let mut result = ctx
        .alloc_set()
        .map_err(|_| oom("Set.prototype.intersection"))?;
    let context = execution_context(ctx, "Set.prototype.intersection")?;
    let this_size = collections::set_len(this, ctx.heap()) as f64;
    if this_size <= other_rec.size() {
        let mut index = 0;
        while index < collections::set_raw_len(this, ctx.heap()) {
            let Some(value) = collections::set_value_at(this, ctx.heap(), index) else {
                index += 1;
                continue;
            };
            index += 1;
            if set_record_has(
                ctx,
                &context,
                &other_rec,
                &value,
                "Set.prototype.intersection",
            )? {
                ctx.set_add(&mut result, value)
                    .map_err(|_| oom("Set.prototype.intersection"))?;
            }
        }
    } else {
        let mut keys = set_record_keys(ctx, &context, &other_rec, "Set.prototype.intersection")?;
        while let Some(value) =
            set_record_next_key(ctx, &context, &mut keys, "Set.prototype.intersection")?
        {
            let value = normalize_set_key(value);
            if collections::set_has(this, ctx.heap(), &value) {
                ctx.set_add(&mut result, value)
                    .map_err(|_| oom("Set.prototype.intersection"))?;
            }
        }
    }
    Ok(Value::set(result))
}

/// §24.2.4.4 `Set.prototype.difference`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-set.prototype.difference>
fn set_proto_difference(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let this = receiver_set(ctx, "Set.prototype.difference")?;
    let other = args.first().cloned().unwrap_or(Value::undefined());
    let other_rec = get_set_record(ctx, other, "Set.prototype.difference")?;
    let mut result = ctx
        .alloc_set()
        .map_err(|_| oom("Set.prototype.difference"))?;
    let this_values = collections::set_values(this, ctx.heap());
    for value in &this_values {
        ctx.set_add(&mut result, *value)
            .map_err(|_| oom("Set.prototype.difference"))?;
    }
    let context = execution_context(ctx, "Set.prototype.difference")?;
    if (this_values.len() as f64) <= other_rec.size() {
        for value in this_values {
            if set_record_has(
                ctx,
                &context,
                &other_rec,
                &value,
                "Set.prototype.difference",
            )? {
                collections::set_delete(result, ctx.heap_mut(), &value);
            }
        }
    } else {
        let mut keys = set_record_keys(ctx, &context, &other_rec, "Set.prototype.difference")?;
        while let Some(value) =
            set_record_next_key(ctx, &context, &mut keys, "Set.prototype.difference")?
        {
            collections::set_delete(result, ctx.heap_mut(), &normalize_set_key(value));
        }
    }
    Ok(Value::set(result))
}

/// §24.2.4.6 `Set.prototype.symmetricDifference`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-set.prototype.symmetricdifference>
fn set_proto_symmetric_difference(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let this = receiver_set(ctx, "Set.prototype.symmetricDifference")?;
    let other = args.first().cloned().unwrap_or(Value::undefined());
    let other_rec = get_set_record(ctx, other, "Set.prototype.symmetricDifference")?;
    let mut result = ctx
        .alloc_set()
        .map_err(|_| oom("Set.prototype.symmetricDifference"))?;
    for value in collections::set_values(this, ctx.heap()) {
        ctx.set_add(&mut result, value)
            .map_err(|_| oom("Set.prototype.symmetricDifference"))?;
    }
    let context = execution_context(ctx, "Set.prototype.symmetricDifference")?;
    let mut keys = set_record_keys(
        ctx,
        &context,
        &other_rec,
        "Set.prototype.symmetricDifference",
    )?;
    while let Some(value) = set_record_next_key(
        ctx,
        &context,
        &mut keys,
        "Set.prototype.symmetricDifference",
    )? {
        let value = normalize_set_key(value);
        let already_in_result = collections::set_has(result, ctx.heap(), &value);
        if collections::set_has(this, ctx.heap(), &value) {
            if already_in_result {
                collections::set_delete(result, ctx.heap_mut(), &value);
            }
        } else if !already_in_result {
            ctx.set_add(&mut result, value)
                .map_err(|_| oom("Set.prototype.symmetricDifference"))?;
        }
    }
    Ok(Value::set(result))
}

/// §24.2.4.10 `Set.prototype.isSubsetOf`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-set.prototype.issubsetof>
fn set_proto_is_subset_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let this = receiver_set(ctx, "Set.prototype.isSubsetOf")?;
    let other = args.first().cloned().unwrap_or(Value::undefined());
    let other_rec = get_set_record(ctx, other, "Set.prototype.isSubsetOf")?;
    if (collections::set_len(this, ctx.heap()) as f64) > other_rec.size() {
        return Ok(Value::boolean(false));
    }
    let context = execution_context(ctx, "Set.prototype.isSubsetOf")?;
    let mut index = 0;
    while index < collections::set_raw_len(this, ctx.heap()) {
        let Some(value) = collections::set_value_at(this, ctx.heap(), index) else {
            index += 1;
            continue;
        };
        index += 1;
        if !set_record_has(
            ctx,
            &context,
            &other_rec,
            &value,
            "Set.prototype.isSubsetOf",
        )? {
            return Ok(Value::boolean(false));
        }
    }
    Ok(Value::boolean(true))
}

/// §24.2.4.11 `Set.prototype.isSupersetOf`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-set.prototype.issupersetof>
fn set_proto_is_superset_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let this = receiver_set(ctx, "Set.prototype.isSupersetOf")?;
    let other = args.first().cloned().unwrap_or(Value::undefined());
    let other_rec = get_set_record(ctx, other, "Set.prototype.isSupersetOf")?;
    if (collections::set_len(this, ctx.heap()) as f64) < other_rec.size() {
        return Ok(Value::boolean(false));
    }
    let context = execution_context(ctx, "Set.prototype.isSupersetOf")?;
    let mut keys = set_record_keys(ctx, &context, &other_rec, "Set.prototype.isSupersetOf")?;
    while let Some(value) =
        set_record_next_key(ctx, &context, &mut keys, "Set.prototype.isSupersetOf")?
    {
        let value = normalize_set_key(value);
        if !collections::set_has(this, ctx.heap(), &value) {
            set_record_close(ctx, &context, &mut keys, "Set.prototype.isSupersetOf")?;
            return Ok(Value::boolean(false));
        }
    }
    Ok(Value::boolean(true))
}

/// §24.2.4.9 `Set.prototype.isDisjointFrom`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-set.prototype.isdisjointfrom>
fn set_proto_is_disjoint_from(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let this = receiver_set(ctx, "Set.prototype.isDisjointFrom")?;
    let other = args.first().cloned().unwrap_or(Value::undefined());
    let other_rec = get_set_record(ctx, other, "Set.prototype.isDisjointFrom")?;
    let context = execution_context(ctx, "Set.prototype.isDisjointFrom")?;
    if (collections::set_len(this, ctx.heap()) as f64) <= other_rec.size() {
        let mut index = 0;
        while index < collections::set_raw_len(this, ctx.heap()) {
            let Some(value) = collections::set_value_at(this, ctx.heap(), index) else {
                index += 1;
                continue;
            };
            index += 1;
            if set_record_has(
                ctx,
                &context,
                &other_rec,
                &value,
                "Set.prototype.isDisjointFrom",
            )? {
                return Ok(Value::boolean(false));
            }
        }
    } else {
        let mut keys = set_record_keys(ctx, &context, &other_rec, "Set.prototype.isDisjointFrom")?;
        while let Some(value) =
            set_record_next_key(ctx, &context, &mut keys, "Set.prototype.isDisjointFrom")?
        {
            let value = normalize_set_key(value);
            if collections::set_has(this, ctx.heap(), &value) {
                set_record_close(ctx, &context, &mut keys, "Set.prototype.isDisjointFrom")?;
                return Ok(Value::boolean(false));
            }
        }
    }
    Ok(Value::boolean(true))
}

fn set_size_get(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let s = receiver_set(ctx, "get Set.prototype.size")?;
    Ok(Value::number(crate::number::NumberValue::from_i32(
        collections::set_len(s, ctx.heap()) as i32,
    )))
}

// ---------------------------------------------------------------
// WeakMap prototype method bodies
// ---------------------------------------------------------------

fn weak_map_proto_get(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let m = receiver_weak_map(ctx, "WeakMap.prototype.get")?;
    let key = args.first().cloned().unwrap_or(Value::undefined());
    match collections::weak_map_get(m, ctx.heap(), &key) {
        Ok(Some(v)) => Ok(v),
        _ => Ok(Value::undefined()),
    }
}

fn weak_map_proto_set(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let mut m = receiver_weak_map(ctx, "WeakMap.prototype.set")?;
    let key = args.first().cloned().unwrap_or(Value::undefined());
    let value = args.get(1).cloned().unwrap_or(Value::undefined());
    ctx.weak_map_set(&mut m, key, value)
        .map_err(|e| collection_to_native(e, "WeakMap.prototype.set"))?;
    Ok(Value::weak_map(m))
}

fn weak_map_proto_has(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let m = receiver_weak_map(ctx, "WeakMap.prototype.has")?;
    let key = args.first().cloned().unwrap_or(Value::undefined());
    match collections::weak_map_has(m, ctx.heap(), &key) {
        Ok(b) => Ok(Value::boolean(b)),
        Err(_) => Ok(Value::boolean(false)),
    }
}

fn weak_map_proto_delete(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let m = receiver_weak_map(ctx, "WeakMap.prototype.delete")?;
    let key = args.first().cloned().unwrap_or(Value::undefined());
    match collections::weak_map_delete(m, ctx.heap_mut(), &key) {
        Ok(b) => Ok(Value::boolean(b)),
        Err(_) => Ok(Value::boolean(false)),
    }
}

/// WeakMap.prototype.getOrInsert(key, value) — upsert proposal. A key
/// that cannot be held weakly throws (`weak_map_get` surfaces it).
fn weak_map_proto_get_or_insert(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let mut m = receiver_weak_map(ctx, "WeakMap.prototype.getOrInsert")?;
    let key = args.first().cloned().unwrap_or(Value::undefined());
    let value = args.get(1).cloned().unwrap_or(Value::undefined());
    match collections::weak_map_get(m, ctx.heap(), &key) {
        Ok(Some(existing)) => return Ok(existing),
        Ok(None) => {}
        Err(e) => return Err(collection_to_native(e, "WeakMap.prototype.getOrInsert")),
    }
    ctx.weak_map_set(&mut m, key, value)
        .map_err(|e| collection_to_native(e, "WeakMap.prototype.getOrInsert"))?;
    Ok(value)
}

/// WeakMap.prototype.getOrInsertComputed(key, callbackfn) — computes the
/// value via the callback only when the key is absent (§ upsert).
fn weak_map_proto_get_or_insert_computed(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let mut m = receiver_weak_map(ctx, "WeakMap.prototype.getOrInsertComputed")?;
    let key = args.first().cloned().unwrap_or(Value::undefined());
    let callback = args.get(1).cloned().unwrap_or(Value::undefined());
    // step 2 — the key must be able to be held weakly.
    let present = collections::weak_map_get(m, ctx.heap(), &key)
        .map_err(|e| collection_to_native(e, "WeakMap.prototype.getOrInsertComputed"))?;
    // step 3 — callbackfn must be callable (checked even when present).
    if !ctx.interp_mut().is_callable_runtime(&callback) {
        return Err(NativeError::TypeError {
            name: "WeakMap.prototype.getOrInsertComputed",
            reason: "callbackfn is not callable".to_string(),
        });
    }
    if let Some(existing) = present {
        return Ok(existing);
    }
    let context = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "WeakMap.prototype.getOrInsertComputed",
            reason: "no active execution context".to_string(),
        })?;
    let value = ctx
        .interp_mut()
        .run_callable_sync(
            &context,
            &callback,
            Value::undefined(),
            smallvec::smallvec![key],
        )
        .map_err(|e| vm_to_native(e, "WeakMap.prototype.getOrInsertComputed"))?;
    ctx.weak_map_set(&mut m, key, value)
        .map_err(|e| collection_to_native(e, "WeakMap.prototype.getOrInsertComputed"))?;
    Ok(value)
}

// ---------------------------------------------------------------
// WeakSet prototype method bodies
// ---------------------------------------------------------------

fn weak_set_proto_add(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let mut s = receiver_weak_set(ctx, "WeakSet.prototype.add")?;
    let v = args.first().cloned().unwrap_or(Value::undefined());
    ctx.weak_set_add(&mut s, v)
        .map_err(|e| collection_to_native(e, "WeakSet.prototype.add"))?;
    Ok(Value::weak_set(s))
}

fn weak_set_proto_has(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let s = receiver_weak_set(ctx, "WeakSet.prototype.has")?;
    let v = args.first().cloned().unwrap_or(Value::undefined());
    match collections::weak_set_has(s, ctx.heap(), &v) {
        Ok(b) => Ok(Value::boolean(b)),
        Err(_) => Ok(Value::boolean(false)),
    }
}

fn weak_set_proto_delete(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let s = receiver_weak_set(ctx, "WeakSet.prototype.delete")?;
    let v = args.first().cloned().unwrap_or(Value::undefined());
    match collections::weak_set_delete(s, ctx.heap_mut(), &v) {
        Ok(b) => Ok(Value::boolean(b)),
        Err(_) => Ok(Value::boolean(false)),
    }
}

// ---------------------------------------------------------------
// Receivers + shared helpers
// ---------------------------------------------------------------

fn receiver_map(ctx: &NativeCtx<'_>, name: &'static str) -> Result<crate::JsMap, NativeError> {
    ctx.this_value()
        .as_map()
        .ok_or_else(|| NativeError::TypeError {
            name,
            reason: "this is not a Map".to_string(),
        })
}

fn receiver_set(ctx: &NativeCtx<'_>, name: &'static str) -> Result<crate::JsSet, NativeError> {
    ctx.this_value()
        .as_set()
        .ok_or_else(|| NativeError::TypeError {
            name,
            reason: "this is not a Set".to_string(),
        })
}

fn receiver_weak_map(
    ctx: &NativeCtx<'_>,
    name: &'static str,
) -> Result<crate::JsWeakMap, NativeError> {
    ctx.this_value()
        .as_weak_map()
        .ok_or_else(|| NativeError::TypeError {
            name,
            reason: "this is not a WeakMap".to_string(),
        })
}

fn receiver_weak_set(
    ctx: &NativeCtx<'_>,
    name: &'static str,
) -> Result<crate::JsWeakSet, NativeError> {
    ctx.this_value()
        .as_weak_set()
        .ok_or_else(|| NativeError::TypeError {
            name,
            reason: "this is not a WeakSet".to_string(),
        })
}

#[derive(Debug, Clone, Copy)]
enum MapIterKind {
    Keys,
    Values,
    Entries,
}

impl MapIterKind {
    fn iterator_kind(self) -> crate::MapIteratorKind {
        match self {
            Self::Keys => crate::MapIteratorKind::Key,
            Self::Values => crate::MapIteratorKind::Value,
            Self::Entries => crate::MapIteratorKind::Entry,
        }
    }
}

#[derive(Clone)]
enum SetRecord {
    Set {
        set: crate::JsSet,
        size: f64,
    },
    Map {
        map: crate::JsMap,
        size: f64,
    },
    Dynamic {
        set: Value,
        size: f64,
        has: Value,
        keys: Value,
    },
}

impl SetRecord {
    fn size(&self) -> f64 {
        match self {
            Self::Set { size, .. } | Self::Map { size, .. } | Self::Dynamic { size, .. } => *size,
        }
    }
}

enum SetRecordKeys {
    Snapshot {
        values: Vec<Value>,
        index: usize,
    },
    Generator {
        handle: crate::generator::JsGenerator,
    },
    Dynamic {
        iterator: Value,
        next_method: Value,
    },
}

fn make_map_iterator(
    ctx: &mut NativeCtx<'_>,
    m: crate::JsMap,
    kind: MapIterKind,
) -> Result<Value, NativeError> {
    let map_value = Value::map(m);
    let iter = ctx
        .alloc_iterator_state(
            crate::IteratorState::MapCollection {
                map: m,
                index: 0,
                kind: kind.iterator_kind(),
            },
            &[&map_value],
            &[],
        )
        .map_err(|_| oom("Map iterator"))?;
    Ok(Value::iterator(iter))
}

/// ECMA-262 `GetSetRecord` for the Set methods.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-getsetrecord>
fn get_set_record(
    ctx: &mut NativeCtx<'_>,
    other: Value,
    name: &'static str,
) -> Result<SetRecord, NativeError> {
    if let Some(set) = other.as_set() {
        return Ok(SetRecord::Set {
            set,
            size: collections::set_len(set, ctx.heap()) as f64,
        });
    }
    if let Some(map) = other.as_map() {
        return Ok(SetRecord::Map {
            map,
            size: collections::map_len(map, ctx.heap()) as f64,
        });
    }
    let value = other;
    {
        {
            if !value_is_object_like(&value) {
                return Err(NativeError::TypeError {
                    name,
                    reason: "other is not an object".to_string(),
                });
            }
            let context = execution_context(ctx, name)?;
            let raw_size = read_property(ctx, &context, &value, "size", name)?;
            let size = to_number_runtime(ctx, &context, &raw_size, name)?;
            if size.is_nan() {
                return Err(NativeError::TypeError {
                    name,
                    reason: "set-like size is NaN".to_string(),
                });
            }
            let has = read_property(ctx, &context, &value, "has", name)?;
            if !ctx.interp_mut().is_callable_runtime(&has) {
                return Err(NativeError::TypeError {
                    name,
                    reason: "set-like has is not callable".to_string(),
                });
            }
            let keys = read_property(ctx, &context, &value, "keys", name)?;
            if !ctx.interp_mut().is_callable_runtime(&keys) {
                return Err(NativeError::TypeError {
                    name,
                    reason: "set-like keys is not callable".to_string(),
                });
            }
            Ok(SetRecord::Dynamic {
                set: value,
                size,
                has,
                keys,
            })
        }
    }
}

fn set_record_has(
    ctx: &mut NativeCtx<'_>,
    context: &crate::ExecutionContext,
    record: &SetRecord,
    value: &Value,
    name: &'static str,
) -> Result<bool, NativeError> {
    match record {
        SetRecord::Set { set, .. } => Ok(collections::set_has(*set, ctx.heap(), value)),
        SetRecord::Map { map, .. } => Ok(collections::map_has(*map, ctx.heap(), value)),
        SetRecord::Dynamic { set, has, .. } => {
            let result = ctx
                .interp_mut()
                .run_callable_sync(context, has, *set, smallvec::smallvec![*value])
                .map_err(|err| vm_to_native(err, name))?;
            Ok(result.to_boolean(ctx.heap()))
        }
    }
}

fn set_record_keys(
    ctx: &mut NativeCtx<'_>,
    context: &crate::ExecutionContext,
    record: &SetRecord,
    name: &'static str,
) -> Result<SetRecordKeys, NativeError> {
    match record {
        SetRecord::Set { set, .. } => Ok(SetRecordKeys::Snapshot {
            values: collections::set_values(*set, ctx.heap()),
            index: 0,
        }),
        SetRecord::Map { map, .. } => Ok(SetRecordKeys::Snapshot {
            values: collections::map_entries(*map, ctx.heap())
                .into_iter()
                .map(|(key, _)| key)
                .collect(),
            index: 0,
        }),
        SetRecord::Dynamic { set, keys, .. } => {
            let iterator = ctx
                .interp_mut()
                .run_callable_sync(context, keys, *set, SmallVec::new())
                .map_err(|err| vm_to_native(err, name))?;
            if let Some(handle) = iterator.as_generator() {
                return Ok(SetRecordKeys::Generator { handle });
            }
            if iterator.is_iterator() {
                let values = ctx
                    .interp_mut()
                    .iterator_to_list_sync(context, &iterator)
                    .map_err(|err| vm_to_native(err, name))?;
                return Ok(SetRecordKeys::Snapshot { values, index: 0 });
            }
            if iterator_has_callable_iterator(ctx, context, &iterator, name)? {
                let values = ctx
                    .interp_mut()
                    .iterator_to_list_sync(context, &iterator)
                    .map_err(|err| vm_to_native(err, name))?;
                return Ok(SetRecordKeys::Snapshot { values, index: 0 });
            }
            if !value_is_object_like(&iterator) {
                return Err(NativeError::TypeError {
                    name,
                    reason: "set-like keys did not return an object".to_string(),
                });
            }
            let next_method = read_property(ctx, context, &iterator, "next", name)?;
            if !ctx.interp_mut().is_callable_runtime(&next_method) {
                return Err(NativeError::TypeError {
                    name,
                    reason: "set-like keys iterator next is not callable".to_string(),
                });
            }
            Ok(SetRecordKeys::Dynamic {
                iterator,
                next_method,
            })
        }
    }
}

fn set_record_next_key(
    ctx: &mut NativeCtx<'_>,
    context: &crate::ExecutionContext,
    keys: &mut SetRecordKeys,
    name: &'static str,
) -> Result<Option<Value>, NativeError> {
    match keys {
        SetRecordKeys::Snapshot { values, index } => {
            let Some(value) = values.get(*index).cloned() else {
                return Ok(None);
            };
            *index += 1;
            Ok(Some(value))
        }
        SetRecordKeys::Generator { handle } => {
            let result = ctx
                .interp_mut()
                .resume_generator(
                    context,
                    handle,
                    crate::GeneratorResumeKind::Next(Value::undefined()),
                )
                .map_err(|err| vm_to_native(err, name))?;
            let Some(record) = result.as_object() else {
                return Err(NativeError::TypeError {
                    name,
                    reason: "generator next did not return an object".to_string(),
                });
            };
            let done = crate::object::get(record, ctx.heap(), "done")
                .unwrap_or(Value::undefined())
                .to_boolean(ctx.heap());
            if done {
                return Ok(None);
            }
            Ok(Some(
                crate::object::get(record, ctx.heap(), "value").unwrap_or(Value::undefined()),
            ))
        }
        SetRecordKeys::Dynamic {
            iterator,
            next_method,
        } => ctx
            .interp_mut()
            .iterator_step_sync(context, iterator, next_method)
            .map_err(|err| vm_to_native(err, name)),
    }
}

fn set_record_close(
    ctx: &mut NativeCtx<'_>,
    context: &crate::ExecutionContext,
    keys: &mut SetRecordKeys,
    name: &'static str,
) -> Result<(), NativeError> {
    if let SetRecordKeys::Dynamic { iterator, .. } = keys {
        ctx.interp_mut()
            .iterator_close_sync(context, iterator)
            .map_err(|err| vm_to_native(err, name))?;
    }
    Ok(())
}

fn execution_context(
    ctx: &NativeCtx<'_>,
    name: &'static str,
) -> Result<crate::ExecutionContext, NativeError> {
    ctx.execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name,
            reason: "no active execution context".to_string(),
        })
}

fn read_property(
    ctx: &mut NativeCtx<'_>,
    context: &crate::ExecutionContext,
    target: &Value,
    property: &'static str,
    name: &'static str,
) -> Result<Value, NativeError> {
    let interp = ctx.interp_mut();
    let outcome = interp
        .ordinary_get_value(
            context,
            *target,
            *target,
            &VmPropertyKey::String(property),
            0,
        )
        .map_err(|err| vm_to_native(err, name))?;
    match outcome {
        VmGetOutcome::Value(value) => Ok(value),
        VmGetOutcome::InvokeGetter { getter } => interp
            .run_callable_sync(context, &getter, *target, SmallVec::new())
            .map_err(|err| vm_to_native(err, name)),
    }
}

fn iterator_has_callable_iterator(
    ctx: &mut NativeCtx<'_>,
    context: &crate::ExecutionContext,
    target: &Value,
    name: &'static str,
) -> Result<bool, NativeError> {
    let iterator_sym = ctx
        .interp_mut()
        .well_known_symbols()
        .get(crate::symbol::WellKnown::Iterator);
    let interp = ctx.interp_mut();
    let outcome = interp
        .ordinary_get_value(
            context,
            *target,
            *target,
            &VmPropertyKey::Symbol(iterator_sym),
            0,
        )
        .map_err(|err| vm_to_native(err, name))?;
    let method = match outcome {
        VmGetOutcome::Value(value) => value,
        VmGetOutcome::InvokeGetter { getter } => interp
            .run_callable_sync(context, &getter, *target, SmallVec::new())
            .map_err(|err| vm_to_native(err, name))?,
    };
    Ok(
        !method.is_undefined()
            && !method.is_null()
            && ctx.interp_mut().is_callable_runtime(&method),
    )
}

fn to_number_runtime(
    ctx: &mut NativeCtx<'_>,
    context: &crate::ExecutionContext,
    value: &Value,
    name: &'static str,
) -> Result<f64, NativeError> {
    let primitive = if crate::abstract_ops::is_primitive(value) {
        *value
    } else {
        ctx.interp_mut()
            .evaluate_to_primitive(context, value, crate::abstract_ops::ToPrimitiveHint::Number)
            .map_err(|err| vm_to_native(err, name))?
    };
    if primitive.is_symbol() || primitive.is_big_int() {
        return Err(NativeError::TypeError {
            name,
            reason: "cannot convert value to number".to_string(),
        });
    }
    Ok(crate::number::to_number_value(&primitive, ctx.heap()))
}

fn normalize_set_key(value: Value) -> Value {
    if let Some(crate::NumberValue::Double(d)) = value.as_number()
        && d == 0.0
        && d.is_sign_negative()
    {
        return Value::number_i32(0);
    }
    value
}

fn value_is_object_like(v: &Value) -> bool {
    // ECMA-262 §6.1.7 `Type(value) is Object` per §24.1.1.2 step 8.c
    // (Map / Set entry iteration), §24.2.5 / §24.3.5 set-like
    // operations. Spec `Object` covers callable / exotic targets, not
    // just `TAG_PTR_OBJECT`.
    v.is_object_type()
}

fn oom(name: &'static str) -> NativeError {
    NativeError::TypeError {
        name,
        reason: "out of memory".to_string(),
    }
}

fn collection_to_native(err: CollectionError, name: &'static str) -> NativeError {
    match err {
        CollectionError::BadReceiver { expected } => NativeError::TypeError {
            name,
            reason: format!("expected {expected}"),
        },
        CollectionError::NonObjectKey => NativeError::TypeError {
            name,
            reason: "key must be an object".to_string(),
        },
        CollectionError::OutOfMemory { .. } => oom(name),
    }
}

fn vm_to_native(err: VmError, name: &'static str) -> NativeError {
    match err {
        VmError::TypeError { message } => NativeError::TypeError {
            name,
            reason: message,
        },
        VmError::TypeMismatch => NativeError::TypeError {
            name,
            reason: "type mismatch".to_string(),
        },
        VmError::SyntaxError { message } => NativeError::SyntaxError {
            name,
            reason: message,
        },
        VmError::RangeError { message } => NativeError::RangeError {
            name,
            reason: message,
        },
        VmError::NotCallable => NativeError::TypeError {
            name,
            reason: "value is not callable".to_string(),
        },
        VmError::Uncaught { value } => NativeError::Thrown {
            name,
            message: value,
        },
        VmError::OutOfMemory { .. } => NativeError::TypeError {
            name,
            reason: "out of memory".to_string(),
        },
        VmError::Exit { code } => NativeError::Exit { code },
        other => NativeError::TypeError {
            name,
            reason: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Interpreter, NativeCallInfo};

    #[test]
    fn native_set_iterator_uses_rooted_iterator_state_allocation() {
        let mut interp = Interpreter::new();
        let set = collections::alloc_set(interp.gc_heap_mut()).expect("set");
        collections::set_add(set, interp.gc_heap_mut(), Value::number_i32(1)).expect("seed");
        let before = interp.gc_heap().stats().old_allocated_bytes;

        let result = {
            let mut ctx =
                NativeCtx::new_with_call_info(&mut interp, NativeCallInfo::call(Value::set(set)));
            set_proto_values(&mut ctx, &[]).expect("set values")
        };

        let after = interp.gc_heap().stats().old_allocated_bytes;
        assert!(
            after > before,
            "Set iterator native path should allocate its iterator state in non-moving old space"
        );
        assert!(result.is_iterator());
    }

    #[test]
    fn native_map_iterator_uses_rooted_iterator_state_allocation() {
        let mut interp = Interpreter::new();
        let map = collections::alloc_map(interp.gc_heap_mut()).expect("map");
        collections::map_set(
            map,
            interp.gc_heap_mut(),
            Value::number_i32(1),
            Value::number_i32(2),
        )
        .expect("seed");
        let before = interp.gc_heap().stats().old_allocated_bytes;

        let result = {
            let mut ctx =
                NativeCtx::new_with_call_info(&mut interp, NativeCallInfo::call(Value::map(map)));
            make_map_iterator(&mut ctx, map, MapIterKind::Entries).expect("map entries")
        };

        let after = interp.gc_heap().stats().old_allocated_bytes;
        assert!(
            after > before,
            "Map iterator native path should allocate its iterator state in non-moving old space"
        );
        assert!(result.is_iterator());
    }
}
