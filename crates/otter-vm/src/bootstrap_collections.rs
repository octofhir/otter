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
use crate::js_surface::{Attr, JsSurfaceError, ObjectBuilder};
use crate::object::{self, JsObject, PartialPropertyDescriptor, PropertyDescriptor};
use crate::{NativeCtx, NativeError, Value, VmError, VmGetOutcome, VmPropertyKey};

// ---------------------------------------------------------------
// Public bootstrap install entry points
// ---------------------------------------------------------------

/// `BuiltinIntrinsic` adapter for `Map`. Routes through the shared
/// `install_collection` helper with `CollectionKind::Map`.
pub struct MapIntrinsic;

impl crate::intrinsic_install::BuiltinIntrinsic for MapIntrinsic {
    const NAME: &'static str = "Map";
    const FEATURE: crate::bootstrap::BootstrapFeatures = crate::bootstrap::BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_collection(Self::NAME, heap, global, CollectionKind::Map)
    }
}

/// `BuiltinIntrinsic` adapter for `Set`.
pub struct SetIntrinsic;

impl crate::intrinsic_install::BuiltinIntrinsic for SetIntrinsic {
    const NAME: &'static str = "Set";
    const FEATURE: crate::bootstrap::BootstrapFeatures = crate::bootstrap::BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_collection(Self::NAME, heap, global, CollectionKind::Set)
    }
}

/// `BuiltinIntrinsic` adapter for `WeakMap`.
pub struct WeakMapIntrinsic;

impl crate::intrinsic_install::BuiltinIntrinsic for WeakMapIntrinsic {
    const NAME: &'static str = "WeakMap";
    const FEATURE: crate::bootstrap::BootstrapFeatures = crate::bootstrap::BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_collection(Self::NAME, heap, global, CollectionKind::WeakMap)
    }
}

/// `BuiltinIntrinsic` adapter for `WeakSet`.
pub struct WeakSetIntrinsic;

impl crate::intrinsic_install::BuiltinIntrinsic for WeakSetIntrinsic {
    const NAME: &'static str = "WeakSet";
    const FEATURE: crate::bootstrap::BootstrapFeatures = crate::bootstrap::BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_collection(Self::NAME, heap, global, CollectionKind::WeakSet)
    }
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
    string_heap: &crate::string::StringHeap,
    global: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    use crate::symbol::WellKnown;

    let to_string_tag = well_known.get(WellKnown::ToStringTag);
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
        // §24.*.3.* — @@toStringTag = ctor_name, non-writable,
        // non-enumerable, configurable.
        let tag = crate::string::JsString::from_str(ctor_name, string_heap)
            .map_err(|_| JsSurfaceError::OutOfMemory)?;
        object::define_own_symbol_property_partial(
            prototype,
            heap,
            &to_string_tag,
            PartialPropertyDescriptor {
                value: Some(Value::String(tag)),
                writable: Some(false),
                enumerable: Some(false),
                configurable: Some(true),
                ..Default::default()
            },
        );
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
                &iterator_sym,
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

fn ctor_prototype(global: JsObject, heap: &otter_gc::GcHeap, ctor_name: &str) -> Option<JsObject> {
    let ctor = object::get(global, heap, ctor_name)?;
    let Value::NativeFunction(f) = ctor else {
        return None;
    };
    let string_heap = crate::string::StringHeap::default();
    let descriptor = f
        .own_property_descriptor(heap, &string_heap, "prototype")
        .ok()
        .flatten()?;
    match descriptor.kind {
        crate::object::DescriptorKind::Data {
            value: Value::Object(obj),
        } => Some(obj),
        _ => None,
    }
}

fn install_collection(
    name: &'static str,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
    kind: CollectionKind,
) -> Result<(), JsSurfaceError> {
    // §24.*.2 Properties of the *Map* / *Set* / *WeakMap* / *WeakSet*
    // Prototype Object — ordinary object linked to %Object.prototype%.
    let global_root = Value::Object(global);
    let prototype = crate::bootstrap::alloc_object_with_value_roots(heap, &[&global_root])?;
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(prototype, heap, Some(object_proto));
    }

    install_prototype_methods(heap, prototype, kind, vec![global_root.clone()])?;

    // §24.1.1 / §24.2.1 / §24.3.1 / §24.4.1 — constructor proper.
    let ctor_name = kind.name();
    let ctor_call: crate::native_function::NativeFastFn = match kind {
        CollectionKind::Map => map_ctor_call,
        CollectionKind::Set => set_ctor_call,
        CollectionKind::WeakMap => weak_map_ctor_call,
        CollectionKind::WeakSet => weak_set_ctor_call,
    };
    let prototype_root = Value::Object(prototype);
    let ctor = crate::bootstrap::native_constructor_static_with_value_roots(
        heap,
        ctor_name,
        0,
        ctor_call,
        &[&global_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let string_heap = crate::string::StringHeap::default();

    // §24.1.2.1 / §24.2.2.1 — `prototype` own data property:
    // non-writable, non-enumerable, non-configurable.
    let proto_desc = PropertyDescriptor::data(Value::Object(prototype), false, false, false);
    if !ctor.define_own_property(heap, &string_heap, "prototype", proto_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("prototype"));
    }

    // Prototype `constructor` back pointer (§24.1.3.1 / §24.2.3.1).
    object::define_own_property(
        prototype,
        heap,
        "constructor",
        PropertyDescriptor::data(Value::NativeFunction(ctor), true, false, true),
    );

    // §24.1.2.1 `Map.groupBy(items, callback)` static.
    // §24.2.2.1 `Set.groupBy` was rejected by TC39; only Map has
    // it. Object.groupBy already handled elsewhere.
    if matches!(kind, CollectionKind::Map) {
        let ctor_root = Value::NativeFunction(ctor);
        let group_by_fn = crate::bootstrap::native_static_with_value_roots(
            heap,
            "groupBy",
            2,
            map_group_by_native,
            &[&global_root, &ctor_root],
        )
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
        let desc = PropertyDescriptor::data(Value::NativeFunction(group_by_fn), true, false, true);
        if !ctor.define_own_property(heap, &string_heap, "groupBy", desc) {
            return Err(JsSurfaceError::DefinePropertyFailed("groupBy"));
        }
    }

    crate::bootstrap::define_global_value(global, heap, name, Value::NativeFunction(ctor));
    Ok(())
}

/// §24.1.2.1 `Map.groupBy(items, callbackfn)` — drain `items`
/// into groups keyed by callback return value, store result in
/// a new Map.
fn map_group_by_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let items = args.first().cloned().unwrap_or(Value::Undefined);
    let callback = args.get(1).cloned().unwrap_or(Value::Undefined);
    if matches!(items, Value::Undefined | Value::Null) {
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
    let result = ctx.alloc_map().map_err(|_| NativeError::TypeError {
        name: "Map.groupBy",
        reason: "out of memory".to_string(),
    })?;
    let result_value = Value::Map(result);
    let items_snapshot: Vec<Value> = match &items {
        Value::Array(arr) => {
            crate::array::with_elements(*arr, ctx.heap(), |elements| elements.to_vec())
        }
        Value::Object(obj) => {
            let length = crate::object::get(*obj, ctx.heap(), "length").unwrap_or(Value::Undefined);
            let len = crate::number::to_number_value(&length);
            let n = if len.is_nan() || len <= 0.0 {
                0
            } else {
                len.min(9_007_199_254_740_991.0) as usize
            };
            let mut out: Vec<Value> = Vec::with_capacity(n);
            for i in 0..n {
                let key = i.to_string();
                out.push(crate::object::get(*obj, ctx.heap(), &key).unwrap_or(Value::Undefined));
            }
            out
        }
        _ => {
            return Err(NativeError::TypeError {
                name: "Map.groupBy",
                reason: "items is not iterable".to_string(),
            });
        }
    };
    for (idx, item) in items_snapshot.iter().enumerate() {
        let mut cb_args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
        cb_args.push(item.clone());
        cb_args.push(Value::Number(crate::number::NumberValue::from_f64(
            idx as f64,
        )));
        let key = ctx
            .cx
            .interp
            .run_callable_sync(&exec_ctx, &callback, Value::Undefined, cb_args)
            .map_err(|e| NativeError::TypeError {
                name: "Map.groupBy",
                reason: e.to_string(),
            })?;
        let existing = crate::collections::map_get(result, ctx.heap(), &key);
        let group_arr = match existing {
            Some(Value::Array(arr)) => arr,
            _ => {
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
                crate::collections::map_set(result, ctx.heap_mut(), key.clone(), Value::Array(arr))
                    .map_err(|_| NativeError::TypeError {
                        name: "Map.groupBy",
                        reason: "out of memory".to_string(),
                    })?;
                arr
            }
        };
        let arr_value = Value::Array(group_arr);
        let len = crate::array::len(group_arr, ctx.heap());
        let roots = ctx.collect_native_roots();
        let item_clone = item.clone();
        let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            arr_value.trace_value_slots(visitor);
            item_clone.trace_value_slots(visitor);
        };
        crate::array::set_with_roots(group_arr, ctx.heap_mut(), len, item.clone(), &mut visit)
            .map_err(|_| NativeError::TypeError {
                name: "Map.groupBy",
                reason: "out of memory".to_string(),
            })?;
    }
    Ok(result_value)
}

fn install_prototype_methods(
    heap: &mut otter_gc::GcHeap,
    prototype: JsObject,
    kind: CollectionKind,
    value_roots: Vec<Value>,
) -> Result<(), JsSurfaceError> {
    use crate::native_function::NativeCall;

    let extra_roots = value_roots.clone();
    let mut builder = ObjectBuilder::from_object_with_value_roots(heap, prototype, value_roots);
    match kind {
        CollectionKind::Map => {
            builder.method(
                "get",
                1,
                NativeCall::Static(map_proto_get),
                Attr::builtin_function(),
            )?;
            builder.method(
                "set",
                2,
                NativeCall::Static(map_proto_set),
                Attr::builtin_function(),
            )?;
            builder.method(
                "has",
                1,
                NativeCall::Static(map_proto_has),
                Attr::builtin_function(),
            )?;
            builder.method(
                "delete",
                1,
                NativeCall::Static(map_proto_delete),
                Attr::builtin_function(),
            )?;
            builder.method(
                "clear",
                0,
                NativeCall::Static(map_proto_clear),
                Attr::builtin_function(),
            )?;
            builder.method(
                "keys",
                0,
                NativeCall::Static(map_proto_keys),
                Attr::builtin_function(),
            )?;
            builder.method(
                "values",
                0,
                NativeCall::Static(map_proto_values),
                Attr::builtin_function(),
            )?;
            builder.method(
                "entries",
                0,
                NativeCall::Static(map_proto_entries),
                Attr::builtin_function(),
            )?;
            builder.method(
                "forEach",
                1,
                NativeCall::Static(map_proto_for_each),
                Attr::builtin_function(),
            )?;
        }
        CollectionKind::Set => {
            builder.method(
                "add",
                1,
                NativeCall::Static(set_proto_add),
                Attr::builtin_function(),
            )?;
            builder.method(
                "has",
                1,
                NativeCall::Static(set_proto_has),
                Attr::builtin_function(),
            )?;
            builder.method(
                "delete",
                1,
                NativeCall::Static(set_proto_delete),
                Attr::builtin_function(),
            )?;
            builder.method(
                "clear",
                0,
                NativeCall::Static(set_proto_clear),
                Attr::builtin_function(),
            )?;
            builder.method(
                "keys",
                0,
                NativeCall::Static(set_proto_keys),
                Attr::builtin_function(),
            )?;
            builder.method(
                "values",
                0,
                NativeCall::Static(set_proto_values),
                Attr::builtin_function(),
            )?;
            builder.method(
                "entries",
                0,
                NativeCall::Static(set_proto_entries),
                Attr::builtin_function(),
            )?;
            builder.method(
                "forEach",
                1,
                NativeCall::Static(set_proto_for_each),
                Attr::builtin_function(),
            )?;
            builder.method(
                "union",
                1,
                NativeCall::Static(set_proto_union),
                Attr::builtin_function(),
            )?;
            builder.method(
                "intersection",
                1,
                NativeCall::Static(set_proto_intersection),
                Attr::builtin_function(),
            )?;
            builder.method(
                "difference",
                1,
                NativeCall::Static(set_proto_difference),
                Attr::builtin_function(),
            )?;
            builder.method(
                "symmetricDifference",
                1,
                NativeCall::Static(set_proto_symmetric_difference),
                Attr::builtin_function(),
            )?;
            builder.method(
                "isSubsetOf",
                1,
                NativeCall::Static(set_proto_is_subset_of),
                Attr::builtin_function(),
            )?;
            builder.method(
                "isSupersetOf",
                1,
                NativeCall::Static(set_proto_is_superset_of),
                Attr::builtin_function(),
            )?;
            builder.method(
                "isDisjointFrom",
                1,
                NativeCall::Static(set_proto_is_disjoint_from),
                Attr::builtin_function(),
            )?;
        }
        CollectionKind::WeakMap => {
            builder.method(
                "get",
                1,
                NativeCall::Static(weak_map_proto_get),
                Attr::builtin_function(),
            )?;
            builder.method(
                "set",
                2,
                NativeCall::Static(weak_map_proto_set),
                Attr::builtin_function(),
            )?;
            builder.method(
                "has",
                1,
                NativeCall::Static(weak_map_proto_has),
                Attr::builtin_function(),
            )?;
            builder.method(
                "delete",
                1,
                NativeCall::Static(weak_map_proto_delete),
                Attr::builtin_function(),
            )?;
        }
        CollectionKind::WeakSet => {
            builder.method(
                "add",
                1,
                NativeCall::Static(weak_set_proto_add),
                Attr::builtin_function(),
            )?;
            builder.method(
                "has",
                1,
                NativeCall::Static(weak_set_proto_has),
                Attr::builtin_function(),
            )?;
            builder.method(
                "delete",
                1,
                NativeCall::Static(weak_set_proto_delete),
                Attr::builtin_function(),
            )?;
        }
    }
    // §24.1.3.11 / §24.2.3.11 — `size` accessor on Map/Set
    // prototypes. WeakMap/WeakSet deliberately omit `size`.
    match kind {
        CollectionKind::Map | CollectionKind::Set => {
            let getter_call: crate::native_function::NativeFastFn = match kind {
                CollectionKind::Map => map_size_get,
                CollectionKind::Set => set_size_get,
                _ => unreachable!(),
            };
            let prototype_root = Value::Object(prototype);
            let mut roots = Vec::with_capacity(extra_roots.len() + 1);
            roots.push(&prototype_root);
            roots.extend(extra_roots.iter());
            let getter = crate::bootstrap::native_static_with_value_roots(
                heap,
                "get size",
                0,
                getter_call,
                roots.as_slice(),
            )
            .map_err(|_| JsSurfaceError::OutOfMemory)?;
            let desc = PropertyDescriptor::accessor(
                Some(Value::NativeFunction(getter)),
                None,
                false,
                true,
            );
            if !object::define_own_property(prototype, heap, "size", desc) {
                return Err(JsSurfaceError::DefinePropertyFailed("size"));
            }
        }
        _ => {}
    }
    Ok(())
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
    let iterable = args.first().cloned().unwrap_or(Value::Undefined);
    if matches!(iterable, Value::Undefined | Value::Null) {
        return Ok(target);
    }
    add_entries_from_iterable(ctx, &target, &iterable, kind)?;
    Ok(target)
}

fn alloc_collection(ctx: &mut NativeCtx<'_>, kind: CollectionKind) -> Result<Value, NativeError> {
    let name = kind.name();
    match kind {
        CollectionKind::Map => ctx.alloc_map().map(Value::Map).map_err(|_| oom(name)),
        CollectionKind::Set => ctx.alloc_set().map(Value::Set).map_err(|_| oom(name)),
        CollectionKind::WeakMap => ctx
            .alloc_weak_map()
            .map(Value::WeakMap)
            .map_err(|_| oom(name)),
        CollectionKind::WeakSet => ctx
            .alloc_weak_set()
            .map(Value::WeakSet)
            .map_err(|_| oom(name)),
    }
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
                target.clone(),
                target.clone(),
                &VmPropertyKey::String(adder_name),
                0,
            )
            .map_err(|e| vm_to_native(e, ctor_name))?;
        match outcome {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => interp
                .run_callable_sync(&context, &getter, target.clone(), SmallVec::new())
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
    matches!(
        iterable,
        Value::Array(_) | Value::String(_) | Value::Map(_) | Value::Set(_) | Value::Generator(_)
    )
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
            .run_callable_sync(context, adder, target.clone(), call_args)
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

        let call_args = match build_adder_args(ctx, context, &next, kind, Some(&iterator)) {
            Ok(args) => args,
            Err(err) => return Err(err),
        };

        let call_result = {
            let interp = ctx.interp_mut();
            interp.run_callable_sync(context, adder, target.clone(), call_args)
        };
        if let Err(err) = call_result {
            let _ = ctx.interp_mut().iterator_close_sync(context, &iterator);
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
        return Ok(smallvec::smallvec![next.clone()]);
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
                let _ = ctx.interp_mut().iterator_close_sync(context, iterator);
            }
            return Err(vm_to_native(err, ctor_name));
        }
    };
    let value = match read_indexed_property(ctx, context, next, "1") {
        Ok(v) => v,
        Err(err) => {
            if let Some(iterator) = iterator_for_close {
                let _ = ctx.interp_mut().iterator_close_sync(context, iterator);
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
    let outcome = interp.ordinary_get_value(
        context,
        target.clone(),
        target.clone(),
        &VmPropertyKey::String(name),
        0,
    )?;
    match outcome {
        VmGetOutcome::Value(v) => Ok(v),
        VmGetOutcome::InvokeGetter { getter } => {
            interp.run_callable_sync(context, &getter, target.clone(), SmallVec::new())
        }
    }
}

// ---------------------------------------------------------------
// Map prototype method bodies
// ---------------------------------------------------------------

fn map_proto_get(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let m = receiver_map(ctx, "Map.prototype.get")?;
    let key = args.first().cloned().unwrap_or(Value::Undefined);
    Ok(collections::map_get(m, ctx.heap(), &key).unwrap_or(Value::Undefined))
}

fn map_proto_set(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let mut m = receiver_map(ctx, "Map.prototype.set")?;
    let key = args.first().cloned().unwrap_or(Value::Undefined);
    let value = args.get(1).cloned().unwrap_or(Value::Undefined);
    ctx.map_set(&mut m, key, value)
        .map_err(|_| oom("Map.prototype.set"))?;
    Ok(Value::Map(m))
}

fn map_proto_has(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let m = receiver_map(ctx, "Map.prototype.has")?;
    let key = args.first().cloned().unwrap_or(Value::Undefined);
    Ok(Value::Boolean(collections::map_has(m, ctx.heap(), &key)))
}

fn map_proto_delete(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let m = receiver_map(ctx, "Map.prototype.delete")?;
    let key = args.first().cloned().unwrap_or(Value::Undefined);
    Ok(Value::Boolean(collections::map_delete(
        m,
        ctx.heap_mut(),
        &key,
    )))
}

fn map_proto_clear(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let m = receiver_map(ctx, "Map.prototype.clear")?;
    collections::map_clear(m, ctx.heap_mut());
    Ok(Value::Undefined)
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

/// §24.1.3.5 Map.prototype.forEach.
fn map_proto_for_each(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let m = receiver_map(ctx, "Map.prototype.forEach")?;
    let callback = args.first().cloned().unwrap_or(Value::Undefined);
    if !ctx.interp_mut().is_callable_runtime(&callback) {
        return Err(NativeError::TypeError {
            name: "Map.prototype.forEach",
            reason: "callback is not callable".to_string(),
        });
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    let entries = collections::map_entries(m, ctx.heap_mut());
    let context = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "Map.prototype.forEach",
            reason: "no active execution context".to_string(),
        })?;
    let map_value = Value::Map(m);
    for (k, v) in entries {
        let interp = ctx.interp_mut();
        interp
            .run_callable_sync(
                &context,
                &callback,
                this_arg.clone(),
                smallvec::smallvec![v, k, map_value.clone()],
            )
            .map_err(|e| vm_to_native(e, "Map.prototype.forEach"))?;
    }
    Ok(Value::Undefined)
}

fn map_size_get(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let m = receiver_map(ctx, "get Map.prototype.size")?;
    Ok(Value::Number(crate::number::NumberValue::from_i32(
        collections::map_len(m, ctx.heap()) as i32,
    )))
}

// ---------------------------------------------------------------
// Set prototype method bodies
// ---------------------------------------------------------------

fn set_proto_add(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let mut s = receiver_set(ctx, "Set.prototype.add")?;
    let v = args.first().cloned().unwrap_or(Value::Undefined);
    ctx.set_add(&mut s, v)
        .map_err(|_| oom("Set.prototype.add"))?;
    Ok(Value::Set(s))
}

fn set_proto_has(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let s = receiver_set(ctx, "Set.prototype.has")?;
    let v = args.first().cloned().unwrap_or(Value::Undefined);
    Ok(Value::Boolean(collections::set_has(s, ctx.heap(), &v)))
}

fn set_proto_delete(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let s = receiver_set(ctx, "Set.prototype.delete")?;
    let v = args.first().cloned().unwrap_or(Value::Undefined);
    Ok(Value::Boolean(collections::set_delete(
        s,
        ctx.heap_mut(),
        &v,
    )))
}

fn set_proto_clear(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let s = receiver_set(ctx, "Set.prototype.clear")?;
    collections::set_clear(s, ctx.heap_mut());
    Ok(Value::Undefined)
}

fn set_proto_keys(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    set_proto_values(ctx, _args)
}

fn set_proto_values(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let s = receiver_set(ctx, "Set.prototype.values")?;
    let snapshot: SmallVec<[Value; 4]> =
        collections::set_values(s, ctx.heap()).into_iter().collect();
    let array = ctx
        .array_from_elements(snapshot)
        .map_err(|_| oom("Set.prototype.values"))?;
    let array_value = Value::Array(array);
    let iter = ctx
        .alloc_iterator_state(
            crate::IteratorState::Array {
                array,
                index: 0,
                origin: crate::BuiltinIteratorOrigin::Set,
            },
            &[&array_value],
            &[],
        )
        .map_err(|_| oom("Set.prototype.values"))?;
    Ok(Value::Iterator(iter))
}

fn set_proto_entries(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let s = receiver_set(ctx, "Set.prototype.entries")?;
    let values: Vec<Value> = collections::set_values(s, ctx.heap());
    let mut snap: SmallVec<[Value; 4]> = SmallVec::new();
    for v in values {
        let pair = ctx
            .array_from_elements([v.clone(), v])
            .map_err(|_| oom("Set.prototype.entries"))?;
        snap.push(Value::Array(pair));
    }
    let array = ctx
        .array_from_elements(snap)
        .map_err(|_| oom("Set.prototype.entries"))?;
    let array_value = Value::Array(array);
    let iter = ctx
        .alloc_iterator_state(
            crate::IteratorState::Array {
                array,
                index: 0,
                origin: crate::BuiltinIteratorOrigin::Set,
            },
            &[&array_value],
            &[],
        )
        .map_err(|_| oom("Set.prototype.entries"))?;
    Ok(Value::Iterator(iter))
}

/// §24.2.3.6 Set.prototype.forEach.
fn set_proto_for_each(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let s = receiver_set(ctx, "Set.prototype.forEach")?;
    let callback = args.first().cloned().unwrap_or(Value::Undefined);
    if !ctx.interp_mut().is_callable_runtime(&callback) {
        return Err(NativeError::TypeError {
            name: "Set.prototype.forEach",
            reason: "callback is not callable".to_string(),
        });
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    let values = collections::set_values(s, ctx.heap());
    let context = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "Set.prototype.forEach",
            reason: "no active execution context".to_string(),
        })?;
    let set_value = Value::Set(s);
    for v in values {
        let interp = ctx.interp_mut();
        interp
            .run_callable_sync(
                &context,
                &callback,
                this_arg.clone(),
                smallvec::smallvec![v.clone(), v, set_value.clone()],
            )
            .map_err(|e| vm_to_native(e, "Set.prototype.forEach"))?;
    }
    Ok(Value::Undefined)
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

/// Native dispatch entry used by `Op::CallMethodValue` for direct
/// `set.method(...)` calls. The intrinsic table cannot re-enter JS,
/// while these algorithms must execute `GetSetRecord`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-getsetrecord>
/// - <https://tc39.es/ecma262/#sec-properties-of-the-set-prototype-object>
pub(crate) fn set_method_call(
    ctx: &mut NativeCtx<'_>,
    name: &str,
    args: &[Value],
) -> Result<Value, NativeError> {
    match name {
        "union" => set_proto_union(ctx, args),
        "intersection" => set_proto_intersection(ctx, args),
        "difference" => set_proto_difference(ctx, args),
        "symmetricDifference" => set_proto_symmetric_difference(ctx, args),
        "isSubsetOf" => set_proto_is_subset_of(ctx, args),
        "isSupersetOf" => set_proto_is_superset_of(ctx, args),
        "isDisjointFrom" => set_proto_is_disjoint_from(ctx, args),
        _ => Err(NativeError::TypeError {
            name: "Set.prototype",
            reason: format!("unknown Set method {name}"),
        }),
    }
}

/// §24.2.4.7 `Set.prototype.union`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-set.prototype.union>
fn set_proto_union(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let this = receiver_set(ctx, "Set.prototype.union")?;
    let other = args.first().cloned().unwrap_or(Value::Undefined);
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
    Ok(Value::Set(result))
}

/// §24.2.4.5 `Set.prototype.intersection`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-set.prototype.intersection>
fn set_proto_intersection(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let this = receiver_set(ctx, "Set.prototype.intersection")?;
    let other = args.first().cloned().unwrap_or(Value::Undefined);
    let other_rec = get_set_record(ctx, other, "Set.prototype.intersection")?;
    let mut result = ctx
        .alloc_set()
        .map_err(|_| oom("Set.prototype.intersection"))?;
    let context = execution_context(ctx, "Set.prototype.intersection")?;
    let this_size = collections::set_len(this, ctx.heap()) as f64;
    if this_size <= other_rec.size() {
        for value in collections::set_values(this, ctx.heap()) {
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
    Ok(Value::Set(result))
}

/// §24.2.4.4 `Set.prototype.difference`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-set.prototype.difference>
fn set_proto_difference(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let this = receiver_set(ctx, "Set.prototype.difference")?;
    let other = args.first().cloned().unwrap_or(Value::Undefined);
    let other_rec = get_set_record(ctx, other, "Set.prototype.difference")?;
    let mut result = ctx
        .alloc_set()
        .map_err(|_| oom("Set.prototype.difference"))?;
    let this_values = collections::set_values(this, ctx.heap());
    for value in &this_values {
        ctx.set_add(&mut result, value.clone())
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
    Ok(Value::Set(result))
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
    let other = args.first().cloned().unwrap_or(Value::Undefined);
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
        if collections::set_has(result, ctx.heap(), &value) {
            collections::set_delete(result, ctx.heap_mut(), &value);
        } else {
            ctx.set_add(&mut result, value)
                .map_err(|_| oom("Set.prototype.symmetricDifference"))?;
        }
    }
    Ok(Value::Set(result))
}

/// §24.2.4.10 `Set.prototype.isSubsetOf`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-set.prototype.issubsetof>
fn set_proto_is_subset_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let this = receiver_set(ctx, "Set.prototype.isSubsetOf")?;
    let other = args.first().cloned().unwrap_or(Value::Undefined);
    let other_rec = get_set_record(ctx, other, "Set.prototype.isSubsetOf")?;
    if (collections::set_len(this, ctx.heap()) as f64) > other_rec.size() {
        return Ok(Value::Boolean(false));
    }
    let context = execution_context(ctx, "Set.prototype.isSubsetOf")?;
    for value in collections::set_values(this, ctx.heap()) {
        if !set_record_has(
            ctx,
            &context,
            &other_rec,
            &value,
            "Set.prototype.isSubsetOf",
        )? {
            return Ok(Value::Boolean(false));
        }
    }
    Ok(Value::Boolean(true))
}

/// §24.2.4.11 `Set.prototype.isSupersetOf`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-set.prototype.issupersetof>
fn set_proto_is_superset_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let this = receiver_set(ctx, "Set.prototype.isSupersetOf")?;
    let other = args.first().cloned().unwrap_or(Value::Undefined);
    let other_rec = get_set_record(ctx, other, "Set.prototype.isSupersetOf")?;
    if (collections::set_len(this, ctx.heap()) as f64) < other_rec.size() {
        return Ok(Value::Boolean(false));
    }
    let context = execution_context(ctx, "Set.prototype.isSupersetOf")?;
    let mut keys = set_record_keys(ctx, &context, &other_rec, "Set.prototype.isSupersetOf")?;
    while let Some(value) =
        set_record_next_key(ctx, &context, &mut keys, "Set.prototype.isSupersetOf")?
    {
        let value = normalize_set_key(value);
        if !collections::set_has(this, ctx.heap(), &value) {
            set_record_close(ctx, &context, &mut keys, "Set.prototype.isSupersetOf")?;
            return Ok(Value::Boolean(false));
        }
    }
    Ok(Value::Boolean(true))
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
    let other = args.first().cloned().unwrap_or(Value::Undefined);
    let other_rec = get_set_record(ctx, other, "Set.prototype.isDisjointFrom")?;
    let context = execution_context(ctx, "Set.prototype.isDisjointFrom")?;
    if (collections::set_len(this, ctx.heap()) as f64) <= other_rec.size() {
        for value in collections::set_values(this, ctx.heap()) {
            if set_record_has(
                ctx,
                &context,
                &other_rec,
                &value,
                "Set.prototype.isDisjointFrom",
            )? {
                return Ok(Value::Boolean(false));
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
                return Ok(Value::Boolean(false));
            }
        }
    }
    Ok(Value::Boolean(true))
}

fn set_size_get(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let s = receiver_set(ctx, "get Set.prototype.size")?;
    Ok(Value::Number(crate::number::NumberValue::from_i32(
        collections::set_len(s, ctx.heap()) as i32,
    )))
}

// ---------------------------------------------------------------
// WeakMap prototype method bodies
// ---------------------------------------------------------------

fn weak_map_proto_get(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let m = receiver_weak_map(ctx, "WeakMap.prototype.get")?;
    let key = args.first().cloned().unwrap_or(Value::Undefined);
    match collections::weak_map_get(m, ctx.heap(), &key) {
        Ok(Some(v)) => Ok(v),
        _ => Ok(Value::Undefined),
    }
}

fn weak_map_proto_set(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let mut m = receiver_weak_map(ctx, "WeakMap.prototype.set")?;
    let key = args.first().cloned().unwrap_or(Value::Undefined);
    let value = args.get(1).cloned().unwrap_or(Value::Undefined);
    ctx.weak_map_set(&mut m, key, value)
        .map_err(|e| collection_to_native(e, "WeakMap.prototype.set"))?;
    Ok(Value::WeakMap(m))
}

fn weak_map_proto_has(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let m = receiver_weak_map(ctx, "WeakMap.prototype.has")?;
    let key = args.first().cloned().unwrap_or(Value::Undefined);
    match collections::weak_map_has(m, ctx.heap(), &key) {
        Ok(b) => Ok(Value::Boolean(b)),
        Err(_) => Ok(Value::Boolean(false)),
    }
}

fn weak_map_proto_delete(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let m = receiver_weak_map(ctx, "WeakMap.prototype.delete")?;
    let key = args.first().cloned().unwrap_or(Value::Undefined);
    match collections::weak_map_delete(m, ctx.heap_mut(), &key) {
        Ok(b) => Ok(Value::Boolean(b)),
        Err(_) => Ok(Value::Boolean(false)),
    }
}

// ---------------------------------------------------------------
// WeakSet prototype method bodies
// ---------------------------------------------------------------

fn weak_set_proto_add(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let mut s = receiver_weak_set(ctx, "WeakSet.prototype.add")?;
    let v = args.first().cloned().unwrap_or(Value::Undefined);
    ctx.weak_set_add(&mut s, v)
        .map_err(|e| collection_to_native(e, "WeakSet.prototype.add"))?;
    Ok(Value::WeakSet(s))
}

fn weak_set_proto_has(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let s = receiver_weak_set(ctx, "WeakSet.prototype.has")?;
    let v = args.first().cloned().unwrap_or(Value::Undefined);
    match collections::weak_set_has(s, ctx.heap(), &v) {
        Ok(b) => Ok(Value::Boolean(b)),
        Err(_) => Ok(Value::Boolean(false)),
    }
}

fn weak_set_proto_delete(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let s = receiver_weak_set(ctx, "WeakSet.prototype.delete")?;
    let v = args.first().cloned().unwrap_or(Value::Undefined);
    match collections::weak_set_delete(s, ctx.heap_mut(), &v) {
        Ok(b) => Ok(Value::Boolean(b)),
        Err(_) => Ok(Value::Boolean(false)),
    }
}

// ---------------------------------------------------------------
// Receivers + shared helpers
// ---------------------------------------------------------------

fn receiver_map(ctx: &NativeCtx<'_>, name: &'static str) -> Result<crate::JsMap, NativeError> {
    match ctx.this_value() {
        Value::Map(m) => Ok(*m),
        _ => Err(NativeError::TypeError {
            name,
            reason: "this is not a Map".to_string(),
        }),
    }
}

fn receiver_set(ctx: &NativeCtx<'_>, name: &'static str) -> Result<crate::JsSet, NativeError> {
    match ctx.this_value() {
        Value::Set(s) => Ok(*s),
        _ => Err(NativeError::TypeError {
            name,
            reason: "this is not a Set".to_string(),
        }),
    }
}

fn receiver_weak_map(
    ctx: &NativeCtx<'_>,
    name: &'static str,
) -> Result<crate::JsWeakMap, NativeError> {
    match ctx.this_value() {
        Value::WeakMap(m) => Ok(*m),
        _ => Err(NativeError::TypeError {
            name,
            reason: "this is not a WeakMap".to_string(),
        }),
    }
}

fn receiver_weak_set(
    ctx: &NativeCtx<'_>,
    name: &'static str,
) -> Result<crate::JsWeakSet, NativeError> {
    match ctx.this_value() {
        Value::WeakSet(s) => Ok(*s),
        _ => Err(NativeError::TypeError {
            name,
            reason: "this is not a WeakSet".to_string(),
        }),
    }
}

#[derive(Debug, Clone, Copy)]
enum MapIterKind {
    Keys,
    Values,
    Entries,
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
    let entries = collections::map_entries(m, ctx.heap_mut());
    let mut snapshot: SmallVec<[Value; 4]> = SmallVec::with_capacity(entries.len());
    for (k, v) in entries {
        let element = match kind {
            MapIterKind::Keys => k,
            MapIterKind::Values => v,
            MapIterKind::Entries => {
                let pair = ctx
                    .array_from_elements([k, v])
                    .map_err(|_| oom("Map.prototype.entries"))?;
                Value::Array(pair)
            }
        };
        snapshot.push(element);
    }
    let array = ctx
        .array_from_elements(snapshot)
        .map_err(|_| oom("Map iterator"))?;
    let array_value = Value::Array(array);
    let iter = ctx
        .alloc_iterator_state(
            crate::IteratorState::Array {
                array,
                index: 0,
                origin: crate::BuiltinIteratorOrigin::Map,
            },
            &[&array_value],
            &[],
        )
        .map_err(|_| oom("Map iterator"))?;
    Ok(Value::Iterator(iter))
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
    match other {
        Value::Set(set) => Ok(SetRecord::Set {
            set,
            size: collections::set_len(set, ctx.heap()) as f64,
        }),
        Value::Map(map) => Ok(SetRecord::Map {
            map,
            size: collections::map_len(map, ctx.heap()) as f64,
        }),
        value => {
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
                .run_callable_sync(
                    context,
                    has,
                    set.clone(),
                    smallvec::smallvec![value.clone()],
                )
                .map_err(|err| vm_to_native(err, name))?;
            Ok(result.to_boolean())
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
                .run_callable_sync(context, keys, set.clone(), SmallVec::new())
                .map_err(|err| vm_to_native(err, name))?;
            if let Value::Generator(handle) = iterator {
                return Ok(SetRecordKeys::Generator { handle });
            }
            if matches!(iterator, Value::Iterator(_)) {
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
                    crate::GeneratorResumeKind::Next(Value::Undefined),
                )
                .map_err(|err| vm_to_native(err, name))?;
            let Value::Object(record) = result else {
                return Err(NativeError::TypeError {
                    name,
                    reason: "generator next did not return an object".to_string(),
                });
            };
            let done = crate::object::get(record, ctx.heap(), "done")
                .unwrap_or(Value::Undefined)
                .to_boolean();
            if done {
                return Ok(None);
            }
            Ok(Some(
                crate::object::get(record, ctx.heap(), "value").unwrap_or(Value::Undefined),
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
            target.clone(),
            target.clone(),
            &VmPropertyKey::String(property),
            0,
        )
        .map_err(|err| vm_to_native(err, name))?;
    match outcome {
        VmGetOutcome::Value(value) => Ok(value),
        VmGetOutcome::InvokeGetter { getter } => interp
            .run_callable_sync(context, &getter, target.clone(), SmallVec::new())
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
            target.clone(),
            target.clone(),
            &VmPropertyKey::Symbol(iterator_sym),
            0,
        )
        .map_err(|err| vm_to_native(err, name))?;
    let method = match outcome {
        VmGetOutcome::Value(value) => value,
        VmGetOutcome::InvokeGetter { getter } => interp
            .run_callable_sync(context, &getter, target.clone(), SmallVec::new())
            .map_err(|err| vm_to_native(err, name))?,
    };
    Ok(!matches!(method, Value::Undefined | Value::Null)
        && ctx.interp_mut().is_callable_runtime(&method))
}

fn to_number_runtime(
    ctx: &mut NativeCtx<'_>,
    context: &crate::ExecutionContext,
    value: &Value,
    name: &'static str,
) -> Result<f64, NativeError> {
    let primitive = if crate::abstract_ops::is_primitive(value) {
        value.clone()
    } else {
        ctx.interp_mut()
            .evaluate_to_primitive(context, value, crate::abstract_ops::ToPrimitiveHint::Number)
            .map_err(|err| vm_to_native(err, name))?
    };
    match primitive {
        Value::Symbol(_) | Value::BigInt(_) => Err(NativeError::TypeError {
            name,
            reason: "cannot convert value to number".to_string(),
        }),
        value => Ok(crate::number::to_number_value(&value)),
    }
}

fn normalize_set_key(value: Value) -> Value {
    match value {
        Value::Number(crate::NumberValue::Double(n)) if n == 0.0 && n.is_sign_negative() => {
            Value::Number(crate::NumberValue::from_i32(0))
        }
        value => value,
    }
}

fn value_is_object_like(v: &Value) -> bool {
    matches!(
        v,
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
    use crate::{Interpreter, NativeCallInfo, NumberValue};

    #[test]
    fn native_set_iterator_uses_rooted_iterator_state_allocation() {
        let mut interp = Interpreter::new();
        let set = collections::alloc_set(interp.gc_heap_mut()).expect("set");
        collections::set_add(
            set,
            interp.gc_heap_mut(),
            Value::Number(NumberValue::from_i32(1)),
        )
        .expect("seed");
        let before = interp.gc_heap().stats().new_allocated_bytes;

        let result = {
            let mut ctx =
                NativeCtx::new_with_call_info(&mut interp, NativeCallInfo::call(Value::Set(set)));
            set_proto_values(&mut ctx, &[]).expect("set values")
        };

        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Set iterator native path should allocate snapshot array and iterator state in young space"
        );
        assert!(matches!(result, Value::Iterator(_)));
    }

    #[test]
    fn native_map_iterator_uses_rooted_iterator_state_allocation() {
        let mut interp = Interpreter::new();
        let map = collections::alloc_map(interp.gc_heap_mut()).expect("map");
        collections::map_set(
            map,
            interp.gc_heap_mut(),
            Value::Number(NumberValue::from_i32(1)),
            Value::Number(NumberValue::from_i32(2)),
        )
        .expect("seed");
        let before = interp.gc_heap().stats().new_allocated_bytes;

        let result = {
            let mut ctx =
                NativeCtx::new_with_call_info(&mut interp, NativeCallInfo::call(Value::Map(map)));
            make_map_iterator(&mut ctx, map, MapIterKind::Entries).expect("map entries")
        };

        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Map iterator native path should allocate snapshot arrays and iterator state in young space"
        );
        assert!(matches!(result, Value::Iterator(_)));
    }
}
