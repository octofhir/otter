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

    crate::bootstrap::define_global_value(global, heap, name, Value::NativeFunction(ctor));
    Ok(())
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
            crate::IteratorState::Array { array, index: 0 },
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
            crate::IteratorState::Array { array, index: 0 },
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
            crate::IteratorState::Array { array, index: 0 },
            &[&array_value],
            &[],
        )
        .map_err(|_| oom("Map iterator"))?;
    Ok(Value::Iterator(iter))
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
            | Value::Date(_)
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
