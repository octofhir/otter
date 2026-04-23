//! Map and Set intrinsics — ES2024 §24.1 and §24.2.

use crate::builders::ClassBuilder;
use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::{HeapValueKind, ObjectHandle};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan},
};

pub(super) static MAP_SET_INTRINSIC: MapSetIntrinsic = MapSetIntrinsic;

pub(super) struct MapSetIntrinsic;

impl IntrinsicInstaller for MapSetIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        install_map(intrinsics, cx)?;
        install_set(intrinsics, cx)?;
        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        cx.install_global_value(
            intrinsics,
            "Map",
            RegisterValue::from_object_handle(intrinsics.map_constructor.0),
        )?;
        cx.install_global_value(
            intrinsics,
            "Set",
            RegisterValue::from_object_handle(intrinsics.set_constructor.0),
        )
    }
}

fn proto(
    name: &str,
    arity: u16,
    f: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
) -> NativeBindingDescriptor {
    NativeBindingDescriptor::new(
        NativeBindingTarget::Prototype,
        NativeFunctionDescriptor::method(name, arity, f),
    )
}

// ═══════════════════════════════════════════════════════════════════════════
//  Map
// ═══════════════════════════════════════════════════════════════════════════

fn map_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("Map")
        .with_constructor(
            NativeFunctionDescriptor::constructor("Map", 0, map_constructor)
                .with_default_intrinsic(crate::intrinsics::IntrinsicKey::MapPrototype),
        )
        .with_binding(proto("get", 1, map_get))
        .with_binding(proto("set", 2, map_set))
        .with_binding(proto("has", 1, map_has))
        .with_binding(proto("delete", 1, map_delete))
        .with_binding(proto("clear", 0, map_clear))
        .with_binding(proto("forEach", 1, map_for_each))
        .with_binding(proto("keys", 0, map_keys))
        .with_binding(proto("values", 0, map_values))
        .with_binding(proto("entries", 0, map_entries))
        // §24.1.2.2 Map.groupBy(items, callbackfn)
        // <https://tc39.es/ecma262/#sec-map.groupby>
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("groupBy", 2, map_group_by),
        ))
}

fn install_map(
    intrinsics: &mut VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let descriptor = map_class_descriptor();
    let plan = ClassBuilder::from_descriptor(&descriptor)
        .expect("Map class descriptors should normalize")
        .build();

    let constructor = if let Some(desc) = plan.constructor() {
        let host_function = cx.native_functions.register(desc.clone());
        cx.alloc_intrinsic_host_function(host_function, intrinsics.function_prototype())?
    } else {
        cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?
    };

    intrinsics.map_constructor = constructor;

    let map_prototype = cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?;
    intrinsics.map_prototype = map_prototype;

    install_class_plan(
        map_prototype,
        constructor,
        &plan,
        intrinsics.function_prototype(),
        cx,
    )?;

    // Install size getter.
    let size_desc = NativeFunctionDescriptor::method("get size", 0, map_size_getter);
    let size_id = cx.native_functions.register(size_desc);
    let size_fn = cx.alloc_intrinsic_host_function(size_id, intrinsics.function_prototype())?;
    let size_prop = cx.property_names.intern("size");
    cx.heap.define_own_property(
        map_prototype,
        size_prop,
        crate::object::PropertyValue::Accessor {
            getter: Some(size_fn),
            setter: None,
            attributes: crate::object::PropertyAttributes::from_flags(false, true, true),
        },
    )?;

    // §24.1.3.12: Map.prototype[@@iterator] is the same function as entries().
    // Spec: <https://tc39.es/ecma262/#sec-map.prototype-@@iterator>
    let entries_prop = cx.property_names.intern("entries");
    let entries_fn = match cx.heap.get_property(map_prototype, entries_prop) {
        Ok(Some(lookup)) => match lookup.value() {
            crate::object::PropertyValue::Data { value, .. } => value,
            _ => RegisterValue::undefined(),
        },
        _ => RegisterValue::undefined(),
    };
    let sym_iterator = cx
        .property_names
        .intern_symbol(super::WellKnownSymbol::Iterator.stable_id());
    cx.heap
        .set_property(map_prototype, sym_iterator, entries_fn)?;

    Ok(())
}

fn map_constructor(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let prototype =
        runtime.subclass_prototype_or_default(*this, runtime.intrinsics().map_prototype);
    let handle = runtime.objects_mut().alloc_map(Some(prototype));

    // If iterable argument provided, add entries.
    if let Some(iterable) = args.first().copied()
        && iterable != RegisterValue::undefined()
        && iterable != RegisterValue::null()
        && let Some(arr_handle) = iterable.as_object_handle().map(ObjectHandle)
        && matches!(runtime.objects().kind(arr_handle), Ok(HeapValueKind::Array))
    {
        let length = runtime
            .objects()
            .array_length(arr_handle)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0);
        for i in 0..length {
            // S1: Map construction absorbs an arbitrarily long array.
            // Poll every MAP_SET_POLL_INTERVAL iterations so a 16M-entry
            // iterable can be interrupted cooperatively.
            if i.is_multiple_of(MAP_SET_POLL_INTERVAL) {
                runtime.check_interrupt()?;
            }
            let entry = runtime.get_array_index_value(arr_handle, i)?;
            if let Some(entry_val) = entry
                && let Some(eh) = entry_val.as_object_handle().map(ObjectHandle)
            {
                // §24.1.1.1 step 8.d: Get "0" and "1" from entry object.
                // Entry can be any object (not just Array).
                let key_prop = runtime.intern_property_name("0");
                let key = runtime
                    .ordinary_get(eh, key_prop, entry_val)
                    .unwrap_or_else(|_| RegisterValue::undefined());
                let val_prop = runtime.intern_property_name("1");
                let value = runtime
                    .ordinary_get(eh, val_prop, entry_val)
                    .unwrap_or_else(|_| RegisterValue::undefined());
                runtime
                    .objects_mut()
                    .map_set(handle, key, value)
                    .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
            }
        }
    }

    Ok(RegisterValue::from_object_handle(handle.0))
}

/// S1: interval at which the Map/Set constructor absorption loops poll
/// the interrupt flag. Value matches `OOM_POLL_INTERVAL` in
/// `array_class.rs` so the cost per iteration is identical across hot
/// native loops.
const MAP_SET_POLL_INTERVAL: usize = 4096;

fn map_get(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_map(*this, runtime)?;
    let key = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    runtime
        .objects()
        .map_get(handle, key)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))
}

fn map_set(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_map(*this, runtime)?;
    let key = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let value = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    runtime
        .objects_mut()
        .map_set(handle, key, value)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(*this)
}

fn map_has(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_map(*this, runtime)?;
    let key = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let result = runtime
        .objects()
        .map_has(handle, key)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_bool(result))
}

fn map_delete(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_map(*this, runtime)?;
    let key = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let result = runtime
        .objects_mut()
        .map_delete(handle, key)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_bool(result))
}

fn map_clear(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_map(*this, runtime)?;
    runtime
        .objects_mut()
        .map_clear(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::undefined())
}

fn map_size_getter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_map(*this, runtime)?;
    let size = runtime
        .objects()
        .map_size(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_i32(size as i32))
}

fn map_for_each(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_map(*this, runtime)?;
    let callback = args
        .first()
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
        .filter(|h| runtime.objects().is_callable(*h))
        .ok_or_else(|| {
            VmNativeCallError::Internal("Map.prototype.forEach callback is not a function".into())
        })?;
    let this_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let entries = runtime
        .objects()
        .map_entries(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    for (key, value) in entries {
        runtime.call_callable(callback, this_arg, &[value, key, *this])?;
    }
    Ok(RegisterValue::undefined())
}

/// Map.prototype.keys()
/// Spec: <https://tc39.es/ecma262/#sec-map.prototype.keys>
fn map_keys(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    create_map_iterator(this, crate::object::MapIteratorKind::Keys, runtime)
}

/// Map.prototype.values()
/// Spec: <https://tc39.es/ecma262/#sec-map.prototype.values>
fn map_values(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    create_map_iterator(this, crate::object::MapIteratorKind::Values, runtime)
}

/// Map.prototype.entries() / Map.prototype\[@@iterator\]()
/// Spec: <https://tc39.es/ecma262/#sec-map.prototype.entries>
fn map_entries(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    create_map_iterator(this, crate::object::MapIteratorKind::Entries, runtime)
}

/// §24.1.5.1 CreateMapIterator(map, kind)
/// Spec: <https://tc39.es/ecma262/#sec-createmapiterator>
fn create_map_iterator(
    this: &RegisterValue,
    kind: crate::object::MapIteratorKind,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_map(*this, runtime)?;
    let iterator = runtime.objects_mut().alloc_map_iterator(handle, kind);
    let proto = runtime.intrinsics().map_iterator_prototype();
    runtime
        .objects_mut()
        .set_prototype(iterator, Some(proto))
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_object_handle(iterator.0))
}

fn require_map(
    this: RegisterValue,
    runtime: &crate::interpreter::RuntimeState,
) -> Result<ObjectHandle, VmNativeCallError> {
    this.as_object_handle()
        .map(ObjectHandle)
        .filter(|h| matches!(runtime.objects().kind(*h), Ok(HeapValueKind::Map)))
        .ok_or_else(|| VmNativeCallError::Internal("Method requires a Map receiver".into()))
}

// ═══════════════════════════════════════════════════════════════════════════
//  Set
// ═══════════════════════════════════════════════════════════════════════════

fn set_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("Set")
        .with_constructor(
            NativeFunctionDescriptor::constructor("Set", 0, set_constructor)
                .with_default_intrinsic(crate::intrinsics::IntrinsicKey::SetPrototype),
        )
        .with_binding(proto("add", 1, set_add))
        .with_binding(proto("has", 1, set_has))
        .with_binding(proto("delete", 1, set_delete))
        .with_binding(proto("clear", 0, set_clear))
        .with_binding(proto("forEach", 1, set_for_each))
        .with_binding(proto("values", 0, set_values))
        .with_binding(proto("keys", 0, set_values)) // keys === values for Set
        .with_binding(proto("entries", 0, set_entries))
        // ES2025 Set methods — §24.2.3
        .with_binding(proto("intersection", 1, set_intersection))
        .with_binding(proto("union", 1, set_union))
        .with_binding(proto("difference", 1, set_difference))
        .with_binding(proto("symmetricDifference", 1, set_symmetric_difference))
        .with_binding(proto("isSubsetOf", 1, set_is_subset_of))
        .with_binding(proto("isSupersetOf", 1, set_is_superset_of))
        .with_binding(proto("isDisjointFrom", 1, set_is_disjoint_from))
}

fn install_set(
    intrinsics: &mut VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let descriptor = set_class_descriptor();
    let plan = ClassBuilder::from_descriptor(&descriptor)
        .expect("Set class descriptors should normalize")
        .build();

    let constructor = if let Some(desc) = plan.constructor() {
        let host_function = cx.native_functions.register(desc.clone());
        cx.alloc_intrinsic_host_function(host_function, intrinsics.function_prototype())?
    } else {
        cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?
    };

    intrinsics.set_constructor = constructor;

    let set_prototype = cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?;
    intrinsics.set_prototype = set_prototype;

    install_class_plan(
        set_prototype,
        constructor,
        &plan,
        intrinsics.function_prototype(),
        cx,
    )?;

    // Install size getter.
    let size_desc = NativeFunctionDescriptor::method("get size", 0, set_size_getter);
    let size_id = cx.native_functions.register(size_desc);
    let size_fn = cx.alloc_intrinsic_host_function(size_id, intrinsics.function_prototype())?;
    let size_prop = cx.property_names.intern("size");
    cx.heap.define_own_property(
        set_prototype,
        size_prop,
        crate::object::PropertyValue::Accessor {
            getter: Some(size_fn),
            setter: None,
            attributes: crate::object::PropertyAttributes::from_flags(false, true, true),
        },
    )?;

    // §24.2.3.10: Set.prototype[@@iterator] is the same function as values().
    // Spec: <https://tc39.es/ecma262/#sec-set.prototype-@@iterator>
    let values_prop = cx.property_names.intern("values");
    let values_fn = match cx.heap.get_property(set_prototype, values_prop) {
        Ok(Some(lookup)) => match lookup.value() {
            crate::object::PropertyValue::Data { value, .. } => value,
            _ => RegisterValue::undefined(),
        },
        _ => RegisterValue::undefined(),
    };
    let sym_iterator = cx
        .property_names
        .intern_symbol(super::WellKnownSymbol::Iterator.stable_id());
    cx.heap
        .set_property(set_prototype, sym_iterator, values_fn)?;

    Ok(())
}

fn set_constructor(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let prototype =
        runtime.subclass_prototype_or_default(*this, runtime.intrinsics().set_prototype);
    let handle = runtime.objects_mut().alloc_set(Some(prototype));

    if let Some(iterable) = args.first().copied()
        && iterable != RegisterValue::undefined()
        && iterable != RegisterValue::null()
        && let Some(arr_handle) = iterable.as_object_handle().map(ObjectHandle)
        && matches!(runtime.objects().kind(arr_handle), Ok(HeapValueKind::Array))
    {
        let length = runtime
            .objects()
            .array_length(arr_handle)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0);
        for i in 0..length {
            // S1: same rationale as map_constructor above — bound-user
            // iterable absorption must be interruptible.
            if i.is_multiple_of(MAP_SET_POLL_INTERVAL) {
                runtime.check_interrupt()?;
            }
            if let Some(value) = runtime.get_array_index_value(arr_handle, i)? {
                runtime
                    .objects_mut()
                    .set_add(handle, value)
                    .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
            }
        }
    }

    Ok(RegisterValue::from_object_handle(handle.0))
}

fn set_add(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_set(*this, runtime)?;
    let value = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    runtime
        .objects_mut()
        .set_add(handle, value)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(*this)
}

fn set_has(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_set(*this, runtime)?;
    let value = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let result = runtime
        .objects()
        .set_has(handle, value)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_bool(result))
}

fn set_delete(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_set(*this, runtime)?;
    let value = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let result = runtime
        .objects_mut()
        .set_delete(handle, value)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_bool(result))
}

fn set_clear(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_set(*this, runtime)?;
    runtime
        .objects_mut()
        .set_clear(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::undefined())
}

fn set_size_getter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_set(*this, runtime)?;
    let size = runtime
        .objects()
        .set_size(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_i32(size as i32))
}

fn set_for_each(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_set(*this, runtime)?;
    let callback = args
        .first()
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
        .filter(|h| runtime.objects().is_callable(*h))
        .ok_or_else(|| {
            VmNativeCallError::Internal("Set.prototype.forEach callback is not a function".into())
        })?;
    let this_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let values = runtime
        .objects()
        .set_values(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    for value in values {
        runtime.call_callable(callback, this_arg, &[value, value, *this])?;
    }
    Ok(RegisterValue::undefined())
}

/// Set.prototype.values() / Set.prototype.keys() / Set.prototype\[@@iterator\]()
/// Spec: <https://tc39.es/ecma262/#sec-set.prototype.values>
fn set_values(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    create_set_iterator(this, crate::object::SetIteratorKind::Values, runtime)
}

/// Set.prototype.entries()
/// Spec: <https://tc39.es/ecma262/#sec-set.prototype.entries>
fn set_entries(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    create_set_iterator(this, crate::object::SetIteratorKind::Entries, runtime)
}

/// §24.2.5.1 CreateSetIterator(set, kind)
/// Spec: <https://tc39.es/ecma262/#sec-createsetiterator>
fn create_set_iterator(
    this: &RegisterValue,
    kind: crate::object::SetIteratorKind,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_set(*this, runtime)?;
    let iterator = runtime.objects_mut().alloc_set_iterator(handle, kind);
    let proto = runtime.intrinsics().set_iterator_prototype();
    runtime
        .objects_mut()
        .set_prototype(iterator, Some(proto))
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_object_handle(iterator.0))
}

// ────────────────────────────────────────────────────────────────────────────
// ES2025 Set methods — §24.2.3
// ────────────────────────────────────────────────────────────────────────────

/// `Set.prototype.intersection(other)` — §24.2.3.5
/// <https://tc39.es/ecma262/#sec-set.prototype.intersection>
fn set_intersection(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let this_set = require_set(*this, runtime)?;
    let other_set = require_set_arg(args, runtime)?;

    let this_entries = runtime
        .objects()
        .set_entries(this_set)
        .map_err(map_obj_err)?;
    let set_proto = runtime.intrinsics().set_prototype;
    let result = runtime.objects_mut().alloc_set(Some(set_proto));

    for entry in this_entries.into_iter().flatten() {
        if runtime.objects().set_has(other_set, entry).unwrap_or(false) {
            runtime
                .objects_mut()
                .set_add(result, entry)
                .map_err(map_obj_err)?;
        }
    }

    Ok(RegisterValue::from_object_handle(result.0))
}

/// `Set.prototype.union(other)` — §24.2.3.12
/// <https://tc39.es/ecma262/#sec-set.prototype.union>
fn set_union(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let this_set = require_set(*this, runtime)?;
    let other_set = require_set_arg(args, runtime)?;

    let set_proto = runtime.intrinsics().set_prototype;
    let result = runtime.objects_mut().alloc_set(Some(set_proto));

    // Add all from this.
    let this_entries = runtime
        .objects()
        .set_entries(this_set)
        .map_err(map_obj_err)?;
    for entry in this_entries.into_iter().flatten() {
        runtime
            .objects_mut()
            .set_add(result, entry)
            .map_err(map_obj_err)?;
    }

    // Add all from other (set_add deduplicates).
    let other_entries = runtime
        .objects()
        .set_entries(other_set)
        .map_err(map_obj_err)?;
    for entry in other_entries.into_iter().flatten() {
        runtime
            .objects_mut()
            .set_add(result, entry)
            .map_err(map_obj_err)?;
    }

    Ok(RegisterValue::from_object_handle(result.0))
}

/// `Set.prototype.difference(other)` — §24.2.3.1
/// <https://tc39.es/ecma262/#sec-set.prototype.difference>
fn set_difference(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let this_set = require_set(*this, runtime)?;
    let other_set = require_set_arg(args, runtime)?;

    let this_entries = runtime
        .objects()
        .set_entries(this_set)
        .map_err(map_obj_err)?;
    let set_proto = runtime.intrinsics().set_prototype;
    let result = runtime.objects_mut().alloc_set(Some(set_proto));

    for entry in this_entries.into_iter().flatten() {
        if !runtime.objects().set_has(other_set, entry).unwrap_or(false) {
            runtime
                .objects_mut()
                .set_add(result, entry)
                .map_err(map_obj_err)?;
        }
    }

    Ok(RegisterValue::from_object_handle(result.0))
}

/// `Set.prototype.symmetricDifference(other)` — §24.2.3.10
/// <https://tc39.es/ecma262/#sec-set.prototype.symmetricdifference>
fn set_symmetric_difference(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let this_set = require_set(*this, runtime)?;
    let other_set = require_set_arg(args, runtime)?;

    let set_proto = runtime.intrinsics().set_prototype;
    let result = runtime.objects_mut().alloc_set(Some(set_proto));

    // Elements in this but not in other.
    let this_entries = runtime
        .objects()
        .set_entries(this_set)
        .map_err(map_obj_err)?;
    for entry in this_entries.into_iter().flatten() {
        if !runtime.objects().set_has(other_set, entry).unwrap_or(false) {
            runtime
                .objects_mut()
                .set_add(result, entry)
                .map_err(map_obj_err)?;
        }
    }

    // Elements in other but not in this.
    let other_entries = runtime
        .objects()
        .set_entries(other_set)
        .map_err(map_obj_err)?;
    for entry in other_entries.into_iter().flatten() {
        if !runtime.objects().set_has(this_set, entry).unwrap_or(false) {
            runtime
                .objects_mut()
                .set_add(result, entry)
                .map_err(map_obj_err)?;
        }
    }

    Ok(RegisterValue::from_object_handle(result.0))
}

/// `Set.prototype.isSubsetOf(other)` — §24.2.3.7
/// <https://tc39.es/ecma262/#sec-set.prototype.issubsetof>
fn set_is_subset_of(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let this_set = require_set(*this, runtime)?;
    let other_set = require_set_arg(args, runtime)?;

    let this_entries = runtime
        .objects()
        .set_entries(this_set)
        .map_err(map_obj_err)?;
    for entry in this_entries.into_iter().flatten() {
        if !runtime.objects().set_has(other_set, entry).unwrap_or(false) {
            return Ok(RegisterValue::from_bool(false));
        }
    }

    Ok(RegisterValue::from_bool(true))
}

/// `Set.prototype.isSupersetOf(other)` — §24.2.3.8
/// <https://tc39.es/ecma262/#sec-set.prototype.issupersetof>
fn set_is_superset_of(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let this_set = require_set(*this, runtime)?;
    let other_set = require_set_arg(args, runtime)?;

    let other_entries = runtime
        .objects()
        .set_entries(other_set)
        .map_err(map_obj_err)?;
    for entry in other_entries.into_iter().flatten() {
        if !runtime.objects().set_has(this_set, entry).unwrap_or(false) {
            return Ok(RegisterValue::from_bool(false));
        }
    }

    Ok(RegisterValue::from_bool(true))
}

/// `Set.prototype.isDisjointFrom(other)` — §24.2.3.6
/// <https://tc39.es/ecma262/#sec-set.prototype.isdisjointfrom>
fn set_is_disjoint_from(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let this_set = require_set(*this, runtime)?;
    let other_set = require_set_arg(args, runtime)?;

    let this_entries = runtime
        .objects()
        .set_entries(this_set)
        .map_err(map_obj_err)?;
    for entry in this_entries.into_iter().flatten() {
        if runtime.objects().set_has(other_set, entry).unwrap_or(false) {
            return Ok(RegisterValue::from_bool(false));
        }
    }

    Ok(RegisterValue::from_bool(true))
}

/// Extract the first argument as a Set.
fn require_set_arg(
    args: &[RegisterValue],
    runtime: &crate::interpreter::RuntimeState,
) -> Result<ObjectHandle, VmNativeCallError> {
    args.first()
        .and_then(|v| v.as_object_handle().map(ObjectHandle))
        .filter(|h| matches!(runtime.objects().kind(*h), Ok(HeapValueKind::Set)))
        .ok_or_else(|| VmNativeCallError::Internal("Argument is not a Set".into()))
}

fn map_obj_err(e: crate::object::ObjectError) -> VmNativeCallError {
    VmNativeCallError::Internal(format!("{e:?}").into())
}

fn require_set(
    this: RegisterValue,
    runtime: &crate::interpreter::RuntimeState,
) -> Result<ObjectHandle, VmNativeCallError> {
    this.as_object_handle()
        .map(ObjectHandle)
        .filter(|h| matches!(runtime.objects().kind(*h), Ok(HeapValueKind::Set)))
        .ok_or_else(|| VmNativeCallError::Internal("Method requires a Set receiver".into()))
}

// ---------------------------------------------------------------------------
// §24.1.2.2 Map.groupBy(items, callbackfn)
// ---------------------------------------------------------------------------

/// `Map.groupBy(items, callbackfn)` — §24.1.2.2
/// <https://tc39.es/ecma262/#sec-map.groupby>
///
/// Groups elements of an iterable into a Map whose keys are the results of
/// `callbackfn(element, index)` (not string-coerced — keys preserve identity).
fn map_group_by(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let items = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let callback = args
        .get(1)
        .and_then(|v| v.as_object_handle().map(ObjectHandle))
        .filter(|h| runtime.objects().is_callable(*h))
        .ok_or_else(|| {
            VmNativeCallError::Internal("Map.groupBy: callbackfn is not a function".into())
        })?;

    let source = items
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| VmNativeCallError::Internal("Map.groupBy: items is not an object".into()))?;

    let length = runtime
        .objects()
        .array_length(source)
        .ok()
        .flatten()
        .unwrap_or(0);

    // Result is a new Map.
    let map_proto = runtime.intrinsics().map_prototype;
    let result = runtime.objects_mut().alloc_map(Some(map_proto));

    // We track group keys in order. For each callback result, we check if we
    // already have a Map entry for that key (using SameValueZero). If yes,
    // push to the existing array. If not, create a new array.
    //
    // We maintain a local Vec of (key, array_handle) pairs since we can't
    // easily iterate Map entries during construction.
    let mut groups: Vec<(RegisterValue, ObjectHandle)> = Vec::new();

    for i in 0..length {
        let value = runtime
            .get_array_index_value(source, i)?
            .unwrap_or_else(RegisterValue::undefined);

        let key = runtime.call_callable(
            callback,
            RegisterValue::undefined(),
            &[value, RegisterValue::from_i32(i as i32)],
        )?;

        // Find existing group by SameValueZero.
        let existing = groups
            .iter()
            .find(|(k, _)| same_value_zero_with_runtime(*k, key, runtime));
        let group = if let Some((_, arr)) = existing {
            *arr
        } else {
            let arr = runtime.alloc_array();
            groups.push((key, arr));
            arr
        };

        runtime
            .objects_mut()
            .push_element(group, value)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    }

    // Now populate the Map with the groups.
    for (key, arr) in groups {
        runtime
            .objects_mut()
            .map_set(result, key, RegisterValue::from_object_handle(arr.0))
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    }

    Ok(RegisterValue::from_object_handle(result.0))
}

/// §7.2.10 SameValueZero(x, y) — simplified for groupBy keys.
/// <https://tc39.es/ecma262/#sec-samevaluezero>
///
/// For Map.groupBy, keys are typically strings returned by the callback. Since
/// each call may allocate a new string handle, we compare string content.
fn same_value_zero_with_runtime(
    x: RegisterValue,
    y: RegisterValue,
    runtime: &crate::interpreter::RuntimeState,
) -> bool {
    if x == y {
        return true;
    }
    // Both NaN → true.
    if let (Some(a), Some(b)) = (x.as_number(), y.as_number())
        && a.is_nan()
        && b.is_nan()
    {
        return true;
    }
    // String content comparison (different handles, same content).
    if let (Some(xh), Some(yh)) = (x.as_object_handle(), y.as_object_handle())
        && let (Ok(Some(xs)), Ok(Some(ys))) = (
            runtime.objects().string_value(ObjectHandle(xh)),
            runtime.objects().string_value(ObjectHandle(yh)),
        )
    {
        return xs == ys;
    }
    false
}
