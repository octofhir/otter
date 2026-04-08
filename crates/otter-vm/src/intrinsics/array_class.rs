use crate::builders::ClassBuilder;
use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::{HeapValueKind, ObjectHandle};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics, WellKnownSymbol,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan},
};

pub(super) static ARRAY_INTRINSIC: ArrayIntrinsic = ArrayIntrinsic;

pub(super) struct ArrayIntrinsic;

impl IntrinsicInstaller for ArrayIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let descriptor = array_class_descriptor();
        let plan = ClassBuilder::from_descriptor(&descriptor)
            .expect("Array class descriptors should normalize")
            .build();

        let constructor = if let Some(descriptor) = plan.constructor() {
            let host_function = cx.native_functions.register(descriptor.clone());
            cx.alloc_intrinsic_host_function(host_function, intrinsics.function_prototype())?
        } else {
            cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?
        };

        intrinsics.array_constructor = constructor;
        install_class_plan(
            intrinsics.array_prototype(),
            intrinsics.array_constructor(),
            &plan,
            intrinsics.function_prototype(),
            cx,
        )?;

        // §23.1.3.35: Array.prototype[@@iterator] is the same function object as values().
        // Spec: <https://tc39.es/ecma262/#sec-array.prototype-@@iterator>
        let values_prop = cx.property_names.intern("values");
        let values_fn = match cx
            .heap
            .get_property(intrinsics.array_prototype(), values_prop)
        {
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
            .set_property(intrinsics.array_prototype(), sym_iterator, values_fn)?;

        // §23.1.3.38 Array.prototype[@@unscopables]
        // Spec: <https://tc39.es/ecma262/#sec-array.prototype-%symbol.unscopables%>
        let unscopables_obj = cx.heap.alloc_object(); // null prototype per spec
        let true_val = RegisterValue::from_bool(true);
        for name in [
            "at",
            "copyWithin",
            "entries",
            "fill",
            "find",
            "findIndex",
            "findLast",
            "findLastIndex",
            "flat",
            "flatMap",
            "includes",
            "keys",
            "toReversed",
            "toSorted",
            "toSpliced",
            "values",
        ] {
            let prop = cx.property_names.intern(name);
            cx.heap.set_property(unscopables_obj, prop, true_val)?;
        }
        let sym_unscopables = cx
            .property_names
            .intern_symbol(WellKnownSymbol::Unscopables.stable_id());
        cx.heap.define_own_property(
            intrinsics.array_prototype(),
            sym_unscopables,
            crate::object::PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(unscopables_obj.0),
                crate::object::PropertyAttributes::from_flags(false, false, true),
            ),
        )?;

        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        cx.install_global_value(
            intrinsics,
            "Array",
            RegisterValue::from_object_handle(intrinsics.array_constructor().0),
        )
    }
}

fn array_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("Array")
        .with_constructor(
            NativeFunctionDescriptor::constructor("Array", 1, array_constructor)
                .with_default_intrinsic(crate::intrinsics::IntrinsicKey::ArrayPrototype),
        )
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("isArray", 1, array_is_array),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("push", 1, array_push),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("join", 1, array_join),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("indexOf", 1, array_index_of),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("concat", 1, array_concat),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("slice", 2, array_slice),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("map", 1, array_map),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("filter", 1, array_filter),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("forEach", 1, array_for_each),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("reduce", 1, array_reduce),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("find", 1, array_find),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("findIndex", 1, array_find_index),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("some", 1, array_some),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("every", 1, array_every),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("includes", 1, array_includes),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("fill", 1, array_fill),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("reverse", 0, array_reverse),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("pop", 0, array_pop),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("shift", 0, array_shift),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("unshift", 1, array_unshift),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("splice", 2, array_splice),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("lastIndexOf", 1, array_last_index_of),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("from", 1, array_from),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("of", 0, array_of),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("sort", 1, array_sort),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("reduceRight", 1, array_reduce_right),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("findLast", 1, array_find_last),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("findLastIndex", 1, array_find_last_index),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("flat", 0, array_flat),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("flatMap", 1, array_flat_map),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toString", 0, array_to_string),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toLocaleString", 0, array_to_locale_string),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("copyWithin", 2, array_copy_within),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("at", 1, array_at),
        ))
        // §23.1.3.30 toReversed() — <https://tc39.es/ecma262/#sec-array.prototype.toreversed>
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toReversed", 0, array_to_reversed),
        ))
        // §23.1.3.31 toSorted() — <https://tc39.es/ecma262/#sec-array.prototype.tosorted>
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toSorted", 1, array_to_sorted),
        ))
        // §23.1.3.32 toSpliced() — <https://tc39.es/ecma262/#sec-array.prototype.tospliced>
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toSpliced", 2, array_to_spliced),
        ))
        // §23.1.3.37 with() — <https://tc39.es/ecma262/#sec-array.prototype.with>
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("with", 2, array_with),
        ))
        // §23.1.3.16 keys() — <https://tc39.es/ecma262/#sec-array.prototype.keys>
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("keys", 0, array_keys_iterator),
        ))
        // §23.1.3.34 values() — <https://tc39.es/ecma262/#sec-array.prototype.values>
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("values", 0, array_values_iterator),
        ))
        // §23.1.3.7 entries() — <https://tc39.es/ecma262/#sec-array.prototype.entries>
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("entries", 0, array_entries_iterator),
        ))
}

fn array_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let array = runtime.alloc_array();

    if let [length] = args {
        if let Some(length) = length.as_i32() {
            if length < 0 {
                return Err(invalid_array_length_error(runtime));
            }
            runtime
                .objects_mut()
                .set_array_length(array, usize::try_from(length).unwrap_or(usize::MAX))
                .map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("Array constructor length setup failed: {error:?}").into(),
                    )
                })?;
            return Ok(RegisterValue::from_object_handle(array.0));
        }

        if let Some(length) = length.as_number() {
            if !is_valid_array_length(length) {
                return Err(invalid_array_length_error(runtime));
            }
            runtime
                .objects_mut()
                .set_array_length(array, length as usize)
                .map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("Array constructor length setup failed: {error:?}").into(),
                    )
                })?;
            return Ok(RegisterValue::from_object_handle(array.0));
        }
    }

    for (index, value) in args.iter().copied().enumerate() {
        runtime
            .objects_mut()
            .set_index(array, index, value)
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("Array constructor element store failed: {error:?}").into(),
                )
            })?;
    }

    Ok(RegisterValue::from_object_handle(array.0))
}

fn array_is_array(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let is_array = args
        .first()
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
        .map(|handle| matches!(runtime.objects().kind(handle), Ok(HeapValueKind::Array)))
        .unwrap_or(false);
    Ok(RegisterValue::from_bool(is_array))
}

fn array_push(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.push requires array receiver".into())
    })?;
    if !matches!(runtime.objects().kind(receiver), Ok(HeapValueKind::Array)) {
        return Err(VmNativeCallError::Internal(
            "Array.prototype.push requires array receiver".into(),
        ));
    }

    let start = runtime
        .objects()
        .array_length(receiver)
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("Array.prototype.push length lookup failed: {error:?}").into(),
            )
        })?
        .ok_or_else(|| {
            VmNativeCallError::Internal("Array.prototype.push requires array receiver".into())
        })?;

    for (offset, value) in args.iter().copied().enumerate() {
        runtime
            .objects_mut()
            .set_index(receiver, start.saturating_add(offset), value)
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("Array.prototype.push element store failed: {error:?}").into(),
                )
            })?;
    }

    Ok(RegisterValue::from_i32(
        i32::try_from(start.saturating_add(args.len())).unwrap_or(i32::MAX),
    ))
}

/// ES2024 §23.1.3.15 Array.prototype.join(separator)
///
/// The per-call `Vec::with_capacity(length)` was the single largest OOM
/// source in test262 Array tests — a sparse array with `length = 2^32-1`
/// would reserve ~96 GB of `String` slots even though only a handful of
/// real values exist. We now use the capped helper [`safe_with_capacity`]
/// and poll [`RuntimeState::check_oom`] every 4K iterations so the script
/// fails fast with a catchable `RangeError` when the heap cap is exceeded.
fn array_join(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.join requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.join")?;

    let separator = if let Some(sep_arg) = args.first().copied() {
        if sep_arg == RegisterValue::undefined() {
            ",".to_string()
        } else {
            runtime.js_to_string_infallible(sep_arg).to_string()
        }
    } else {
        ",".to_string()
    };

    let mut parts: Vec<String> = safe_with_capacity(length);
    for index in 0..length {
        if index % OOM_POLL_INTERVAL == 0 {
            runtime.check_oom()?;
        }
        let value = array_index_value(receiver, index, runtime, "Array.prototype.join")?;
        let part = match value {
            None => String::new(),
            Some(value)
                if value == RegisterValue::undefined() || value == RegisterValue::null() =>
            {
                String::new()
            }
            Some(value) => runtime.js_to_string_infallible(value).to_string(),
        };
        parts.push(part);
    }

    let result = parts.join(&separator);
    let handle = runtime.alloc_string(result);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// Upper bound for a single `Vec::with_capacity(n)` call inside array
/// intrinsics. Picked to stay well under the spec cap while still covering
/// every realistic test262 working set. Larger collections will grow the
/// Vec on demand (amortised doubling) so the eventual allocation can still
/// reach `MAX_ARRAY_LENGTH` when the heap cap allows, but we never reserve
/// dozens of gigabytes up front for a sparse length probe.
const WITH_CAPACITY_CAP: usize = 64 * 1024;

/// Interval at which tight native loops poll the OOM flag so the
/// interpreter can surface a `RangeError` without waiting for the next GC
/// safepoint.
const OOM_POLL_INTERVAL: usize = 4096;

/// Capped variant of `Vec::with_capacity` that never reserves more than
/// [`WITH_CAPACITY_CAP`] elements up front. Prevents a single pathological
/// `length` from triggering a multi-GB reservation that would abort the
/// process before our heap-cap machinery can react.
#[inline]
pub(crate) fn safe_with_capacity<T>(len: usize) -> Vec<T> {
    Vec::with_capacity(len.min(WITH_CAPACITY_CAP))
}

/// ES2024 §23.1.3.14 Array.prototype.indexOf(searchElement [, fromIndex])
fn array_index_of(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.indexOf requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.indexOf")?;

    let search = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let from = args
        .get(1)
        .copied()
        .and_then(RegisterValue::as_i32)
        .unwrap_or(0);
    let start = if from < 0 {
        (length as i32 + from).max(0) as usize
    } else {
        from as usize
    };

    for index in start..length {
        let Some(elem) = array_index_value(receiver, index, runtime, "Array.prototype.indexOf")?
        else {
            continue;
        };
        if elem == search {
            return Ok(RegisterValue::from_i32(index as i32));
        }
    }
    Ok(RegisterValue::from_i32(-1))
}

/// ES2024 §23.1.3.1 Array.prototype.concat(...items)
/// Spec: <https://tc39.es/ecma262/#sec-array.prototype.concat>
///
/// Pre-computes the result length across all items and throws `RangeError`
/// if it would exceed the spec-mandated `2^32 - 1` limit, so that we never
/// reach `Vec::resize(2^33, ..)` on pathological `{length: 2**32}` inputs.
fn array_concat(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    use crate::object::MAX_ARRAY_LENGTH;

    // ---- Pass 1: plan. Walk items, compute per-item length, check cap. ----
    let this_val = *this;
    let mut plans: Vec<(RegisterValue, Option<(ObjectHandle, usize)>)> = Vec::new();
    let mut total: usize = 0;
    for item in std::iter::once(&this_val).chain(args.iter()) {
        if is_concat_spreadable(*item, runtime)? {
            let handle = item.as_object_handle().map(ObjectHandle).ok_or_else(|| {
                VmNativeCallError::Internal("concat: spreadable value must be an object".into())
            })?;
            let len = length_of_array_like(handle, runtime)?;
            // Spec cap: result length must fit in uint32.
            let next_total = total.saturating_add(len);
            if next_total > MAX_ARRAY_LENGTH {
                return Err(invalid_array_length_error(runtime));
            }
            total = next_total;
            plans.push((*item, Some((handle, len))));
        } else {
            let next_total = total.saturating_add(1);
            if next_total > MAX_ARRAY_LENGTH {
                return Err(invalid_array_length_error(runtime));
            }
            total = next_total;
            plans.push((*item, None));
        }
    }

    // ---- Pass 2: allocate + copy. ----
    let result = runtime.alloc_array();
    // Pre-size the result once — this is the single `Vec::resize` call. If
    // the heap cap is exceeded, surface the OOM as a catchable RangeError.
    if let Err(err) = runtime.objects_mut().set_array_length(result, total) {
        return Err(object_error_to_vm_error(runtime, err));
    }
    let mut next_index: usize = 0;
    for (item, plan) in plans {
        match plan {
            Some((handle, len)) => {
                for offset in 0..len {
                    // Spec: HasProperty + Get. Missing slots become holes.
                    if let Some(elem) =
                        array_index_value(handle, offset, runtime, "Array.prototype.concat")?
                        && let Err(err) = runtime.objects_mut().set_index(
                            result,
                            next_index.saturating_add(offset),
                            elem,
                        )
                    {
                        return Err(object_error_to_vm_error(runtime, err));
                    }
                }
                next_index = next_index.saturating_add(len);
            }
            None => {
                if let Err(err) = runtime.objects_mut().set_index(result, next_index, item) {
                    return Err(object_error_to_vm_error(runtime, err));
                }
                next_index = next_index.saturating_add(1);
            }
        }
    }
    Ok(RegisterValue::from_object_handle(result.0))
}

/// Converts an `ObjectError` raised by a mutation helper (e.g. `set_index`,
/// `set_array_length`) into the appropriate native-call error. Array length
/// violations become catchable `RangeError`, heap-cap exhaustion becomes a
/// `RangeError: out of memory`, and everything else is treated as an
/// internal error.
fn object_error_to_vm_error(
    runtime: &mut crate::interpreter::RuntimeState,
    error: crate::object::ObjectError,
) -> VmNativeCallError {
    use crate::object::ObjectError;
    match error {
        ObjectError::InvalidArrayLength => invalid_array_length_error(runtime),
        ObjectError::OutOfMemory => runtime.throw_range_error("out of memory: heap limit exceeded"),
        ObjectError::TypeError(msg) => {
            let msg = msg.into_string();
            type_error(runtime, &msg)
        }
        ObjectError::InvalidHandle | ObjectError::InvalidKind | ObjectError::InvalidIndex => {
            VmNativeCallError::Internal("object mutation failed".into())
        }
    }
}

/// ES2024 §22.1.3.1.1 IsConcatSpreadable(O)
/// Spec: <https://tc39.es/ecma262/#sec-isconcatspreadable>
///
/// 1. If O is not an Object, return false.
/// 2. Let spreadable be ? Get(O, @@isConcatSpreadable).
/// 3. If spreadable is not undefined, return ToBoolean(spreadable).
/// 4. Return ? IsArray(O).
fn is_concat_spreadable(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<bool, VmNativeCallError> {
    let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
        return Ok(false);
    };
    let sym_prop =
        runtime.intern_symbol_property_name(WellKnownSymbol::IsConcatSpreadable.stable_id());
    let spreadable = runtime.ordinary_get(handle, sym_prop, value)?;
    if spreadable != RegisterValue::undefined() {
        return Ok(spreadable.is_truthy());
    }
    // Fallback: IsArray(O).
    Ok(matches!(
        runtime.objects().kind(handle),
        Ok(HeapValueKind::Array)
    ))
}

/// ES2024 §23.1.3.26 Array.prototype.slice(start, end)
fn array_slice(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.slice requires array receiver".into())
    })?;
    let len = array_length(receiver, runtime, "Array.prototype.slice")? as i32;

    let raw_start = args.first().and_then(|v| v.as_i32()).unwrap_or(0);
    let start = if raw_start < 0 {
        (len + raw_start).max(0) as usize
    } else {
        raw_start.min(len) as usize
    };

    let raw_end = args
        .get(1)
        .and_then(|v| {
            if *v == RegisterValue::undefined() {
                None
            } else {
                v.as_i32()
            }
        })
        .unwrap_or(len);
    let end = if raw_end < 0 {
        (len + raw_end).max(0) as usize
    } else {
        raw_end.min(len) as usize
    };

    let result = runtime.alloc_array();
    let count = end.saturating_sub(start);
    runtime.objects_mut().set_array_length(result, count).ok();
    for (offset, index) in (start..end).enumerate() {
        if let Some(elem) = array_index_value(receiver, index, runtime, "Array.prototype.slice")? {
            runtime.objects_mut().set_index(result, offset, elem).ok();
        }
    }
    Ok(RegisterValue::from_object_handle(result.0))
}

fn array_length(
    receiver: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
    op: &str,
) -> Result<usize, VmNativeCallError> {
    // Per §23.1.3 every Array.prototype method (except a few that
    // explicitly require a real Array) walks through
    //   1. Let O be ? ToObject(this value).
    //   2. Let len be ? LengthOfArrayLike(O).
    // — i.e. they are all generic over array-like receivers. If the
    // receiver happens to be a real dense Array the fast internal
    // `array_length` returns the tracked length in O(1); otherwise we
    // fall back to `LengthOfArrayLike` which reads the `"length"`
    // property and applies ToLength. This makes
    // `Array.prototype.reduce.call({0:'a',1:'b',length:2}, ...)`,
    // `Array.prototype.map.call(arguments, ...)`, and every other
    // `Array.prototype.*.call(arrayLike, ...)` idiom work.
    //
    // Spec: <https://tc39.es/ecma262/#sec-array-prototype-object>
    //       <https://tc39.es/ecma262/#sec-lengthofarraylike>
    if let Ok(Some(len)) = runtime.objects().array_length(receiver) {
        return Ok(len);
    }
    let len = length_of_array_like(receiver, runtime)?;
    // Pragmatic cap for non-Array array-like receivers. Real engines
    // (V8, SpiderMonkey) walk sparse property keys instead of iterating
    // 0..len when the spec-clamped length would otherwise force a
    // 9 quadrillion iteration loop on objects like
    // `{ length: Infinity, 0: 'a' }`. Until that sparse-aware path is
    // implemented (TODO), we surface a catchable RangeError so the
    // runner doesn't hang for hours on tests like
    // `built-ins/Array/prototype/includes/length-boundaries.js`.
    const NON_ARRAY_LEN_CAP: usize = 1 << 24; // 16M iterations
    if len > NON_ARRAY_LEN_CAP {
        return Err(runtime.throw_range_error(&format!(
            "{op}: array-like receiver length {len} exceeds the {NON_ARRAY_LEN_CAP} iteration cap"
        )));
    }
    Ok(len)
}

/// ES2024 §7.3.2 LengthOfArrayLike(obj)
/// Spec: <https://tc39.es/ecma262/#sec-lengthofarraylike>
///
/// Works for any object with a "length" property (arrays, array-like objects,
/// arguments objects, etc.).
fn length_of_array_like(
    obj: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<usize, VmNativeCallError> {
    // 1. Return ℝ(? ToLength(? Get(obj, "length"))).
    let length_prop = runtime.intern_property_name("length");
    let length_val =
        runtime.ordinary_get(obj, length_prop, RegisterValue::from_object_handle(obj.0))?;
    // ToLength: undefined/NaN → 0, number → clamp to [0, 2^53 - 1]
    if length_val == RegisterValue::undefined() || length_val == RegisterValue::null() {
        return Ok(0);
    }
    if let Some(n) = length_val.as_i32() {
        return Ok(n.max(0) as usize);
    }
    if let Some(n) = length_val.as_number() {
        if n.is_nan() || n < 0.0 {
            return Ok(0);
        }
        return Ok(n.min(((1u64 << 53) - 1) as f64) as usize);
    }
    Ok(0)
}

fn array_index_value(
    receiver: ObjectHandle,
    index: usize,
    runtime: &mut crate::interpreter::RuntimeState,
    _op: &str,
) -> Result<Option<RegisterValue>, VmNativeCallError> {
    runtime.get_array_index_value(receiver, index)
}

fn invalid_array_length_error(runtime: &mut crate::interpreter::RuntimeState) -> VmNativeCallError {
    let prototype = runtime.intrinsics().range_error_prototype;
    let handle = runtime.alloc_object_with_prototype(Some(prototype));
    let message = runtime.alloc_string("Invalid array length");
    let message_prop = runtime.intern_property_name("message");
    runtime
        .objects_mut()
        .set_property(
            handle,
            message_prop,
            RegisterValue::from_object_handle(message.0),
        )
        .ok();
    VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0))
}

fn is_valid_array_length(length: f64) -> bool {
    length.is_finite() && length >= 0.0 && length.fract() == 0.0 && length <= (u32::MAX - 1) as f64
}

/// Helper: resolve callback and thisArg from args[0..2].
fn callback_and_this_arg(
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
    method: &str,
) -> Result<(ObjectHandle, RegisterValue), VmNativeCallError> {
    let callback = args
        .first()
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
        .filter(|h| runtime.objects().is_callable(*h))
        .ok_or_else(|| {
            let msg = format!("{method} callback is not a function");
            type_error(runtime, &msg)
        })?;
    let this_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    Ok((callback, this_arg))
}

fn type_error(runtime: &mut crate::interpreter::RuntimeState, message: &str) -> VmNativeCallError {
    match runtime.alloc_type_error(message) {
        Ok(handle) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0)),
        Err(error) => VmNativeCallError::Internal(format!("{error}").into()),
    }
}

/// ES2024 §23.1.3.18 Array.prototype.map(callbackfn [, thisArg])
fn array_map(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.map requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.map")?;
    let (callback, this_arg) = callback_and_this_arg(args, runtime, "Array.prototype.map")?;

    let result = runtime.alloc_array();
    runtime.objects_mut().set_array_length(result, length).ok();

    for index in 0..length {
        let Some(value) = array_index_value(receiver, index, runtime, "Array.prototype.map")?
        else {
            continue; // hole — skip
        };
        let mapped = runtime.call_callable(
            callback,
            this_arg,
            &[value, RegisterValue::from_i32(index as i32), *this],
        )?;
        runtime.objects_mut().set_index(result, index, mapped).ok();
    }

    Ok(RegisterValue::from_object_handle(result.0))
}

/// ES2024 §23.1.3.7 Array.prototype.filter(callbackfn [, thisArg])
fn array_filter(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.filter requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.filter")?;
    let (callback, this_arg) = callback_and_this_arg(args, runtime, "Array.prototype.filter")?;

    let result = runtime.alloc_array();
    let mut to = 0usize;

    for index in 0..length {
        let Some(value) = array_index_value(receiver, index, runtime, "Array.prototype.filter")?
        else {
            continue;
        };
        let selected = runtime.call_callable(
            callback,
            this_arg,
            &[value, RegisterValue::from_i32(index as i32), *this],
        )?;
        if runtime
            .js_to_boolean(selected)
            .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?
        {
            runtime.objects_mut().set_index(result, to, value).ok();
            to += 1;
        }
    }

    Ok(RegisterValue::from_object_handle(result.0))
}

/// ES2024 §23.1.3.10 Array.prototype.forEach(callbackfn [, thisArg])
fn array_for_each(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.forEach requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.forEach")?;
    let (callback, this_arg) = callback_and_this_arg(args, runtime, "Array.prototype.forEach")?;

    for index in 0..length {
        let Some(value) = array_index_value(receiver, index, runtime, "Array.prototype.forEach")?
        else {
            continue;
        };
        runtime.call_callable(
            callback,
            this_arg,
            &[value, RegisterValue::from_i32(index as i32), *this],
        )?;
    }

    Ok(RegisterValue::undefined())
}

/// ES2024 §23.1.3.22 Array.prototype.reduce(callbackfn [, initialValue])
fn array_reduce(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.reduce requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.reduce")?;
    let callback = args
        .first()
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
        .filter(|h| runtime.objects().is_callable(*h))
        .ok_or_else(|| type_error(runtime, "Array.prototype.reduce callback is not a function"))?;

    let mut accumulator;
    let mut start;

    if let Some(initial) = args.get(1).copied() {
        accumulator = initial;
        start = 0;
    } else {
        // Find the first non-hole element.
        let mut found = false;
        accumulator = RegisterValue::undefined();
        start = 0;
        for index in 0..length {
            if let Some(value) =
                array_index_value(receiver, index, runtime, "Array.prototype.reduce")?
            {
                accumulator = value;
                start = index + 1;
                found = true;
                break;
            }
        }
        if !found {
            return Err(type_error(
                runtime,
                "Reduce of empty array with no initial value",
            ));
        }
    }

    for index in start..length {
        let Some(value) = array_index_value(receiver, index, runtime, "Array.prototype.reduce")?
        else {
            continue;
        };
        accumulator = runtime.call_callable(
            callback,
            RegisterValue::undefined(),
            &[
                accumulator,
                value,
                RegisterValue::from_i32(index as i32),
                *this,
            ],
        )?;
    }

    Ok(accumulator)
}

/// ES2024 §23.1.3.8 Array.prototype.find(predicate [, thisArg])
fn array_find(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.find requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.find")?;
    let (callback, this_arg) = callback_and_this_arg(args, runtime, "Array.prototype.find")?;

    for index in 0..length {
        let value = array_index_value(receiver, index, runtime, "Array.prototype.find")?
            .unwrap_or_else(RegisterValue::undefined);
        let test_result = runtime.call_callable(
            callback,
            this_arg,
            &[value, RegisterValue::from_i32(index as i32), *this],
        )?;
        if runtime
            .js_to_boolean(test_result)
            .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?
        {
            return Ok(value);
        }
    }

    Ok(RegisterValue::undefined())
}

/// ES2024 §23.1.3.9 Array.prototype.findIndex(predicate [, thisArg])
fn array_find_index(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.findIndex requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.findIndex")?;
    let (callback, this_arg) = callback_and_this_arg(args, runtime, "Array.prototype.findIndex")?;

    for index in 0..length {
        let value = array_index_value(receiver, index, runtime, "Array.prototype.findIndex")?
            .unwrap_or_else(RegisterValue::undefined);
        let test_result = runtime.call_callable(
            callback,
            this_arg,
            &[value, RegisterValue::from_i32(index as i32), *this],
        )?;
        if runtime
            .js_to_boolean(test_result)
            .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?
        {
            return Ok(RegisterValue::from_i32(index as i32));
        }
    }

    Ok(RegisterValue::from_i32(-1))
}

/// ES2024 §23.1.3.27 Array.prototype.some(callbackfn [, thisArg])
fn array_some(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.some requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.some")?;
    let (callback, this_arg) = callback_and_this_arg(args, runtime, "Array.prototype.some")?;

    for index in 0..length {
        let Some(value) = array_index_value(receiver, index, runtime, "Array.prototype.some")?
        else {
            continue;
        };
        let test_result = runtime.call_callable(
            callback,
            this_arg,
            &[value, RegisterValue::from_i32(index as i32), *this],
        )?;
        if runtime
            .js_to_boolean(test_result)
            .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?
        {
            return Ok(RegisterValue::from_bool(true));
        }
    }

    Ok(RegisterValue::from_bool(false))
}

/// ES2024 §23.1.3.5 Array.prototype.every(callbackfn [, thisArg])
fn array_every(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.every requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.every")?;
    let (callback, this_arg) = callback_and_this_arg(args, runtime, "Array.prototype.every")?;

    for index in 0..length {
        let Some(value) = array_index_value(receiver, index, runtime, "Array.prototype.every")?
        else {
            continue;
        };
        let test_result = runtime.call_callable(
            callback,
            this_arg,
            &[value, RegisterValue::from_i32(index as i32), *this],
        )?;
        if !runtime
            .js_to_boolean(test_result)
            .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?
        {
            return Ok(RegisterValue::from_bool(false));
        }
    }

    Ok(RegisterValue::from_bool(true))
}

/// ES2024 §23.1.3.12 Array.prototype.includes(searchElement [, fromIndex])
fn array_includes(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.includes requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.includes")?;

    let search = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let from = args
        .get(1)
        .copied()
        .and_then(RegisterValue::as_i32)
        .unwrap_or(0);
    let start = if from < 0 {
        (length as i32 + from).max(0) as usize
    } else {
        from as usize
    };

    for index in start..length {
        let value = array_index_value(receiver, index, runtime, "Array.prototype.includes")?
            .unwrap_or_else(RegisterValue::undefined);
        // SameValueZero comparison.
        let equal = crate::abstract_ops::same_value_zero(runtime.objects(), value, search)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
        if equal {
            return Ok(RegisterValue::from_bool(true));
        }
    }
    Ok(RegisterValue::from_bool(false))
}

/// ES2024 §23.1.3.6 Array.prototype.fill(value [, start [, end]])
fn array_fill(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.fill requires array receiver".into())
    })?;
    let len = array_length(receiver, runtime, "Array.prototype.fill")? as i32;

    let value = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let raw_start = args.get(1).and_then(|v| v.as_i32()).unwrap_or(0);
    let start = if raw_start < 0 {
        (len + raw_start).max(0) as usize
    } else {
        raw_start.min(len) as usize
    };
    let raw_end = args
        .get(2)
        .and_then(|v| {
            if *v == RegisterValue::undefined() {
                None
            } else {
                v.as_i32()
            }
        })
        .unwrap_or(len);
    let end = if raw_end < 0 {
        (len + raw_end).max(0) as usize
    } else {
        raw_end.min(len) as usize
    };

    // `start..end` can span up to `MAX_ARRAY_LENGTH` iterations on sparse
    // arrays; the previous `.ok()` silently swallowed every `set_index`
    // failure, so a heap-cap violation or invalid array-length only showed
    // up as a runaway allocation. Propagate those errors and poll the OOM
    // flag every few thousand iterations so the script can exit quickly
    // with a catchable `RangeError`.
    for index in start..end {
        if index % OOM_POLL_INTERVAL == 0 {
            runtime.check_oom()?;
        }
        if let Err(err) = runtime.objects_mut().set_index(receiver, index, value) {
            return Err(object_error_to_vm_error(runtime, err));
        }
    }

    Ok(*this)
}

/// ES2024 §23.1.3.24 Array.prototype.reverse()
fn array_reverse(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.reverse requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.reverse")?;

    let mut lower = 0usize;
    let mut upper = length.saturating_sub(1);
    while lower < upper {
        let lower_val = array_index_value(receiver, lower, runtime, "Array.prototype.reverse")?;
        let upper_val = array_index_value(receiver, upper, runtime, "Array.prototype.reverse")?;
        match (lower_val, upper_val) {
            (Some(lv), Some(uv)) => {
                runtime.objects_mut().set_index(receiver, lower, uv).ok();
                runtime.objects_mut().set_index(receiver, upper, lv).ok();
            }
            (None, Some(uv)) => {
                runtime.objects_mut().set_index(receiver, lower, uv).ok();
                let prop = runtime.intern_property_name(&upper.to_string());
                let names = runtime.property_names().clone();
                runtime
                    .objects_mut()
                    .delete_property_with_registry(receiver, prop, &names)
                    .ok();
            }
            (Some(lv), None) => {
                runtime.objects_mut().set_index(receiver, upper, lv).ok();
                let prop = runtime.intern_property_name(&lower.to_string());
                let names = runtime.property_names().clone();
                runtime
                    .objects_mut()
                    .delete_property_with_registry(receiver, prop, &names)
                    .ok();
            }
            (None, None) => {}
        }
        lower += 1;
        upper = upper.saturating_sub(1);
    }

    Ok(*this)
}

/// ES2024 §23.1.3.20 Array.prototype.pop()
fn array_pop(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.pop requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.pop")?;
    if length == 0 {
        return Ok(RegisterValue::undefined());
    }
    let last_index = length - 1;
    let value = array_index_value(receiver, last_index, runtime, "Array.prototype.pop")?
        .unwrap_or_else(RegisterValue::undefined);
    runtime
        .objects_mut()
        .set_array_length(receiver, last_index)
        .ok();
    Ok(value)
}

/// ES2024 §23.1.3.25 Array.prototype.shift()
fn array_shift(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.shift requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.shift")?;
    if length == 0 {
        return Ok(RegisterValue::undefined());
    }
    let first = array_index_value(receiver, 0, runtime, "Array.prototype.shift")?
        .unwrap_or_else(RegisterValue::undefined);
    // Shift elements left.
    for index in 1..length {
        if let Some(value) = array_index_value(receiver, index, runtime, "Array.prototype.shift")? {
            runtime
                .objects_mut()
                .set_index(receiver, index - 1, value)
                .ok();
        }
    }
    runtime
        .objects_mut()
        .set_array_length(receiver, length - 1)
        .ok();
    Ok(first)
}

/// ES2024 §23.1.3.31 Array.prototype.unshift(...items)
fn array_unshift(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.unshift requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.unshift")?;
    let arg_count = args.len();

    if arg_count > 0 {
        // Shift existing elements right by arg_count.
        for index in (0..length).rev() {
            if let Some(value) =
                array_index_value(receiver, index, runtime, "Array.prototype.unshift")?
            {
                runtime
                    .objects_mut()
                    .set_index(receiver, index + arg_count, value)
                    .ok();
            }
        }
        // Insert new items at the beginning.
        for (offset, value) in args.iter().copied().enumerate() {
            runtime
                .objects_mut()
                .set_index(receiver, offset, value)
                .ok();
        }
    }

    let new_length = length + arg_count;
    Ok(RegisterValue::from_i32(new_length as i32))
}

/// ES2024 §23.1.3.29 Array.prototype.splice(start, deleteCount, ...items)
fn array_splice(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.splice requires array receiver".into())
    })?;
    let len = array_length(receiver, runtime, "Array.prototype.splice")? as i32;

    let raw_start = args.first().and_then(|v| v.as_i32()).unwrap_or(0);
    let actual_start = if raw_start < 0 {
        (len + raw_start).max(0) as usize
    } else {
        raw_start.min(len) as usize
    };

    let delete_count = if args.len() == 1 {
        (len as usize).saturating_sub(actual_start)
    } else {
        args.get(1)
            .and_then(|v| v.as_i32())
            .unwrap_or(0)
            .max(0)
            .min(len - actual_start as i32) as usize
    };

    let items = if args.len() > 2 { &args[2..] } else { &[] };

    // Build deleted elements array.
    let deleted = runtime.alloc_array();
    for offset in 0..delete_count {
        if let Some(value) = array_index_value(
            receiver,
            actual_start + offset,
            runtime,
            "Array.prototype.splice",
        )? {
            runtime.objects_mut().set_index(deleted, offset, value).ok();
        }
    }

    let item_count = items.len();
    let current_len = len as usize;

    if item_count < delete_count {
        // Shrinking: shift elements left.
        let shift = delete_count - item_count;
        for index in (actual_start + delete_count)..current_len {
            if let Some(value) =
                array_index_value(receiver, index, runtime, "Array.prototype.splice")?
            {
                runtime
                    .objects_mut()
                    .set_index(receiver, index - shift, value)
                    .ok();
            }
        }
        runtime
            .objects_mut()
            .set_array_length(receiver, current_len - shift)
            .ok();
    } else if item_count > delete_count {
        // Growing: shift elements right.
        let shift = item_count - delete_count;
        let new_len = current_len + shift;
        runtime
            .objects_mut()
            .set_array_length(receiver, new_len)
            .ok();
        for index in (actual_start + delete_count..current_len).rev() {
            if let Some(value) =
                array_index_value(receiver, index, runtime, "Array.prototype.splice")?
            {
                runtime
                    .objects_mut()
                    .set_index(receiver, index + shift, value)
                    .ok();
            }
        }
    }

    // Insert new items.
    for (offset, value) in items.iter().copied().enumerate() {
        runtime
            .objects_mut()
            .set_index(receiver, actual_start + offset, value)
            .ok();
    }

    Ok(RegisterValue::from_object_handle(deleted.0))
}

/// ES2024 §23.1.3.17 Array.prototype.lastIndexOf(searchElement [, fromIndex])
fn array_last_index_of(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.lastIndexOf requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.lastIndexOf")?;
    if length == 0 {
        return Ok(RegisterValue::from_i32(-1));
    }

    let search = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let from = args
        .get(1)
        .copied()
        .and_then(RegisterValue::as_i32)
        .unwrap_or(length as i32 - 1);
    let start = if from < 0 {
        (length as i32 + from).max(-1)
    } else {
        from.min(length as i32 - 1)
    };

    let mut index = start;
    while index >= 0 {
        let i = index as usize;
        if let Some(elem) = array_index_value(receiver, i, runtime, "Array.prototype.lastIndexOf")?
            && elem == search
        {
            return Ok(RegisterValue::from_i32(index));
        }
        index -= 1;
    }
    Ok(RegisterValue::from_i32(-1))
}

/// ES2024 §23.1.2.1 Array.from(items [, mapfn [, thisArg]])
fn array_from(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let items = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let map_fn = args
        .get(1)
        .copied()
        .and_then(|v| {
            if v == RegisterValue::undefined() {
                None
            } else {
                v.as_object_handle().map(ObjectHandle)
            }
        })
        .filter(|h| runtime.objects().is_callable(*h));
    let this_arg = args
        .get(2)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let Some(source_handle) = items.as_object_handle().map(ObjectHandle) else {
        return Ok(RegisterValue::from_object_handle(runtime.alloc_array().0));
    };

    if matches!(
        runtime.objects().kind(source_handle),
        Ok(HeapValueKind::Array)
    ) {
        let length = array_length(source_handle, runtime, "Array.from")?;
        let result = runtime.alloc_array();
        runtime.objects_mut().set_array_length(result, length).ok();
        for index in 0..length {
            let value = array_index_value(source_handle, index, runtime, "Array.from")?
                .unwrap_or_else(RegisterValue::undefined);
            let mapped = if let Some(callback) = map_fn {
                runtime.call_callable(
                    callback,
                    this_arg,
                    &[value, RegisterValue::from_i32(index as i32)],
                )?
            } else {
                value
            };
            runtime.objects_mut().set_index(result, index, mapped).ok();
        }
        return Ok(RegisterValue::from_object_handle(result.0));
    }

    // For non-array objects, try "length" property.
    let len_prop = runtime.intern_property_name("length");
    let len_val = runtime
        .ordinary_get(source_handle, len_prop, items)
        .unwrap_or_else(|_| RegisterValue::from_i32(0));
    let length = len_val.as_i32().unwrap_or(0).max(0) as usize;

    let result = runtime.alloc_array();
    runtime.objects_mut().set_array_length(result, length).ok();
    for index in 0..length {
        let idx_prop = runtime.intern_property_name(&index.to_string());
        let value = runtime
            .ordinary_get(source_handle, idx_prop, items)
            .unwrap_or_else(|_| RegisterValue::undefined());
        let mapped = if let Some(callback) = map_fn {
            runtime.call_callable(
                callback,
                this_arg,
                &[value, RegisterValue::from_i32(index as i32)],
            )?
        } else {
            value
        };
        runtime.objects_mut().set_index(result, index, mapped).ok();
    }
    Ok(RegisterValue::from_object_handle(result.0))
}

/// ES2024 §23.1.2.3 Array.of(...items)
fn array_of(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let result = runtime.alloc_array();
    for (index, value) in args.iter().copied().enumerate() {
        runtime.objects_mut().set_index(result, index, value).ok();
    }
    Ok(RegisterValue::from_object_handle(result.0))
}

/// Array.prototype.values() / Array.prototype\[@@iterator\]()
/// Spec: <https://tc39.es/ecma262/#sec-array.prototype.values>
/// Returns a new Array Iterator whose [[ArrayIteratorKind]] is `values`.
fn array_values_iterator(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    create_array_iterator(this, crate::object::ArrayIteratorKind::Values, runtime)
}

/// Array.prototype.keys()
/// Spec: <https://tc39.es/ecma262/#sec-array.prototype.keys>
/// Returns a new Array Iterator whose [[ArrayIteratorKind]] is `keys`.
fn array_keys_iterator(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    create_array_iterator(this, crate::object::ArrayIteratorKind::Keys, runtime)
}

/// Array.prototype.entries()
/// Spec: <https://tc39.es/ecma262/#sec-array.prototype.entries>
/// Returns a new Array Iterator whose [[ArrayIteratorKind]] is `entries`.
fn array_entries_iterator(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    create_array_iterator(this, crate::object::ArrayIteratorKind::Entries, runtime)
}

/// §23.1.5.1 CreateArrayIterator(array, kind)
/// Spec: <https://tc39.es/ecma262/#sec-createarrayiterator>
fn create_array_iterator(
    this: &RegisterValue,
    kind: crate::object::ArrayIteratorKind,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array iterator requires object receiver".into())
    })?;
    let iterator = runtime.objects_mut().alloc_array_iterator(handle, kind);
    // Set prototype to %ArrayIteratorPrototype%.
    let proto = runtime.intrinsics().array_iterator_prototype();
    runtime
        .objects_mut()
        .set_prototype(iterator, Some(proto))
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_object_handle(iterator.0))
}

/// ES2024 §23.1.3.28 Array.prototype.sort(comparefn)
fn array_sort(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.sort requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.sort")?;

    let comparefn = args
        .first()
        .copied()
        .filter(|v| *v != RegisterValue::undefined())
        .and_then(|v| v.as_object_handle().map(ObjectHandle))
        .filter(|h| runtime.objects().is_callable(*h));

    // Collect non-hole elements.
    let mut items: Vec<RegisterValue> = safe_with_capacity(length);
    for index in 0..length {
        if index % OOM_POLL_INTERVAL == 0 {
            runtime.check_oom()?;
        }
        if let Some(v) = array_index_value(receiver, index, runtime, "Array.prototype.sort")? {
            items.push(v);
        }
    }

    // Sort with a simple insertion sort (stable, handles comparefn errors).
    for i in 1..items.len() {
        let key = items[i];
        let mut j = i;
        while j > 0 {
            let cmp = sort_compare(items[j - 1], key, comparefn, runtime)?;
            if cmp <= 0.0 {
                break;
            }
            items[j] = items[j - 1];
            j -= 1;
        }
        items[j] = key;
    }

    // Write back sorted items, then holes.
    for (index, value) in items.iter().copied().enumerate() {
        runtime.objects_mut().set_index(receiver, index, value).ok();
    }
    // Clear remaining slots (holes).
    for index in items.len()..length {
        let prop = runtime.intern_property_name(&index.to_string());
        let names = runtime.property_names().clone();
        runtime
            .objects_mut()
            .delete_property_with_registry(receiver, prop, &names)
            .ok();
    }

    Ok(*this)
}

fn sort_compare(
    x: RegisterValue,
    y: RegisterValue,
    comparefn: Option<ObjectHandle>,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<f64, VmNativeCallError> {
    if x == RegisterValue::undefined() && y == RegisterValue::undefined() {
        return Ok(0.0);
    }
    if x == RegisterValue::undefined() {
        return Ok(1.0);
    }
    if y == RegisterValue::undefined() {
        return Ok(-1.0);
    }
    if let Some(callback) = comparefn {
        let result = runtime.call_callable(callback, RegisterValue::undefined(), &[x, y])?;
        let n = result
            .as_number()
            .or_else(|| result.as_i32().map(|i| i as f64))
            .unwrap_or(0.0);
        if n.is_nan() {
            return Ok(0.0);
        }
        return Ok(n);
    }
    // Default: compare as strings.
    let xs = runtime.js_to_string_infallible(x).to_string();
    let ys = runtime.js_to_string_infallible(y).to_string();
    Ok(match xs.cmp(&ys) {
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Equal => 0.0,
        std::cmp::Ordering::Greater => 1.0,
    })
}

/// ES2024 §23.1.3.23 Array.prototype.reduceRight(callbackfn [, initialValue])
fn array_reduce_right(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.reduceRight requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.reduceRight")?;
    let callback = args
        .first()
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
        .filter(|h| runtime.objects().is_callable(*h))
        .ok_or_else(|| {
            type_error(
                runtime,
                "Array.prototype.reduceRight callback is not a function",
            )
        })?;

    let mut accumulator;
    let mut start: i64;

    if let Some(initial) = args.get(1).copied() {
        accumulator = initial;
        start = length as i64 - 1;
    } else {
        let mut found = false;
        accumulator = RegisterValue::undefined();
        start = length as i64 - 1;
        while start >= 0 {
            if let Some(value) = array_index_value(
                receiver,
                start as usize,
                runtime,
                "Array.prototype.reduceRight",
            )? {
                accumulator = value;
                start -= 1;
                found = true;
                break;
            }
            start -= 1;
        }
        if !found {
            return Err(type_error(
                runtime,
                "Reduce of empty array with no initial value",
            ));
        }
    }

    while start >= 0 {
        let index = start as usize;
        if let Some(value) =
            array_index_value(receiver, index, runtime, "Array.prototype.reduceRight")?
        {
            accumulator = runtime.call_callable(
                callback,
                RegisterValue::undefined(),
                &[
                    accumulator,
                    value,
                    RegisterValue::from_i32(index as i32),
                    *this,
                ],
            )?;
        }
        start -= 1;
    }

    Ok(accumulator)
}

/// ES2024 §23.1.3.8.1 Array.prototype.findLast(predicate [, thisArg])
fn array_find_last(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.findLast requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.findLast")?;
    let (callback, this_arg) = callback_and_this_arg(args, runtime, "Array.prototype.findLast")?;

    for index in (0..length).rev() {
        let value = array_index_value(receiver, index, runtime, "Array.prototype.findLast")?
            .unwrap_or_else(RegisterValue::undefined);
        let test_result = runtime.call_callable(
            callback,
            this_arg,
            &[value, RegisterValue::from_i32(index as i32), *this],
        )?;
        if runtime
            .js_to_boolean(test_result)
            .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?
        {
            return Ok(value);
        }
    }
    Ok(RegisterValue::undefined())
}

/// ES2024 §23.1.3.9.1 Array.prototype.findLastIndex(predicate [, thisArg])
fn array_find_last_index(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.findLastIndex requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.findLastIndex")?;
    let (callback, this_arg) =
        callback_and_this_arg(args, runtime, "Array.prototype.findLastIndex")?;

    for index in (0..length).rev() {
        let value = array_index_value(receiver, index, runtime, "Array.prototype.findLastIndex")?
            .unwrap_or_else(RegisterValue::undefined);
        let test_result = runtime.call_callable(
            callback,
            this_arg,
            &[value, RegisterValue::from_i32(index as i32), *this],
        )?;
        if runtime
            .js_to_boolean(test_result)
            .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?
        {
            return Ok(RegisterValue::from_i32(index as i32));
        }
    }
    Ok(RegisterValue::from_i32(-1))
}

/// ES2024 §23.1.3.11 Array.prototype.flat([depth])
fn array_flat(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.flat requires array receiver".into())
    })?;
    let depth = args
        .first()
        .copied()
        .and_then(|v| {
            if v == RegisterValue::undefined() {
                None
            } else {
                v.as_i32()
            }
        })
        .unwrap_or(1)
        .max(0) as usize;

    let result = runtime.alloc_array();
    flatten_into_array(receiver, result, depth, runtime)?;
    Ok(RegisterValue::from_object_handle(result.0))
}

/// Hard cap on `Array.prototype.flat` recursion. Neither ECMA-262 nor any
/// browser engine specifies a numeric limit — V8/SpiderMonkey rely on
/// native stack exhaustion — but we set an explicit cap so a pathological
/// input aborts with a catchable `RangeError` long before the Rust stack
/// unwinds.
const MAX_FLAT_DEPTH: usize = 1024;

fn flatten_into_array(
    source: ObjectHandle,
    target: ObjectHandle,
    depth: usize,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<(), VmNativeCallError> {
    flatten_into_array_bounded(source, target, depth, 0, runtime)
}

fn flatten_into_array_bounded(
    source: ObjectHandle,
    target: ObjectHandle,
    depth: usize,
    call_depth: usize,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<(), VmNativeCallError> {
    if call_depth >= MAX_FLAT_DEPTH {
        return Err(
            runtime.throw_range_error("Array.prototype.flat: maximum nesting depth exceeded")
        );
    }
    let length = array_length(source, runtime, "flat")?;
    for index in 0..length {
        if index % OOM_POLL_INTERVAL == 0 {
            runtime.check_oom()?;
        }
        let Some(value) = array_index_value(source, index, runtime, "flat")? else {
            continue;
        };
        if depth > 0
            && let Some(h) = value.as_object_handle().map(ObjectHandle)
            && matches!(runtime.objects().kind(h), Ok(HeapValueKind::Array))
        {
            flatten_into_array_bounded(h, target, depth - 1, call_depth + 1, runtime)?;
            continue;
        }
        if let Err(err) = runtime.objects_mut().push_element(target, value) {
            return Err(object_error_to_vm_error(runtime, err));
        }
    }
    Ok(())
}

/// ES2024 §23.1.3.10.1 Array.prototype.flatMap(mapperFunction [, thisArg])
fn array_flat_map(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.flatMap requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.flatMap")?;
    let (callback, this_arg) = callback_and_this_arg(args, runtime, "Array.prototype.flatMap")?;

    let result = runtime.alloc_array();
    for index in 0..length {
        let Some(value) = array_index_value(receiver, index, runtime, "Array.prototype.flatMap")?
        else {
            continue;
        };
        let mapped = runtime.call_callable(
            callback,
            this_arg,
            &[value, RegisterValue::from_i32(index as i32), *this],
        )?;
        if let Some(h) = mapped.as_object_handle().map(ObjectHandle)
            && matches!(runtime.objects().kind(h), Ok(HeapValueKind::Array))
        {
            flatten_into_array(h, result, 0, runtime)?;
            continue;
        }
        runtime.objects_mut().push_element(result, mapped).ok();
    }
    Ok(RegisterValue::from_object_handle(result.0))
}

/// ES2024 §23.1.3.30 Array.prototype.toString()
fn array_to_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // Equivalent to this.join().
    array_join(this, &[], runtime)
}

/// ECMA-402 §21.1.1 Array.prototype.toLocaleString([locales [, options]])
///
/// Calls toLocaleString() on each element and joins with locale-appropriate separator.
/// Spec: <https://tc39.es/ecma402/#sup-array.prototype.tolocalestring>
fn array_to_locale_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.toLocaleString requires array receiver".into())
    })?;
    let len = array_length(receiver, runtime, "Array.prototype.toLocaleString")?;

    let mut parts: Vec<String> = safe_with_capacity(len);
    for i in 0..len {
        if i % OOM_POLL_INTERVAL == 0 {
            runtime.check_oom()?;
        }
        let prop = runtime.intern_property_name(&i.to_string());
        let elem_receiver = RegisterValue::from_object_handle(receiver.0);
        let elem = runtime.ordinary_get(receiver, prop, elem_receiver)?;

        if elem == RegisterValue::undefined() || elem == RegisterValue::null() {
            parts.push(String::new());
        } else {
            // Call toLocaleString() on the element.
            let elem_str = call_to_locale_string(elem, runtime)?;
            parts.push(elem_str);
        }
    }

    let joined = parts.join(",");
    let handle = runtime.alloc_string(joined);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// Helper: call `.toLocaleString()` on a value.
fn call_to_locale_string(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<String, VmNativeCallError> {
    // Get the toLocaleString property.
    if let Some(h) = value.as_object_handle().map(ObjectHandle) {
        let prop = runtime.intern_property_name("toLocaleString");
        let receiver = RegisterValue::from_object_handle(h.0);
        let method = runtime.ordinary_get(h, prop, receiver)?;
        if method != RegisterValue::undefined() {
            let callable = method.as_object_handle().map(ObjectHandle);
            let result = runtime.call_host_function(callable, value, &[])?;
            let s = runtime.js_to_string(result).map_err(|e| {
                VmNativeCallError::Internal(format!("toLocaleString result: {e}").into())
            })?;
            return Ok(s.to_string());
        }
    }
    // Fallback: coerce to string.
    let s = runtime
        .js_to_string(value)
        .map_err(|e| VmNativeCallError::Internal(format!("toLocaleString coerce: {e}").into()))?;
    Ok(s.to_string())
}

/// ES2024 §23.1.3.3 Array.prototype.copyWithin(target, start [, end])
fn array_copy_within(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.copyWithin requires array receiver".into())
    })?;
    let len = array_length(receiver, runtime, "Array.prototype.copyWithin")? as i32;

    let raw_target = args.first().and_then(|v| v.as_i32()).unwrap_or(0);
    let to = if raw_target < 0 {
        (len + raw_target).max(0) as usize
    } else {
        raw_target.min(len) as usize
    };
    let raw_start = args.get(1).and_then(|v| v.as_i32()).unwrap_or(0);
    let from = if raw_start < 0 {
        (len + raw_start).max(0) as usize
    } else {
        raw_start.min(len) as usize
    };
    let raw_end = args
        .get(2)
        .and_then(|v| {
            if *v == RegisterValue::undefined() {
                None
            } else {
                v.as_i32()
            }
        })
        .unwrap_or(len);
    let fin = if raw_end < 0 {
        (len + raw_end).max(0) as usize
    } else {
        raw_end.min(len) as usize
    };

    let count = (fin.saturating_sub(from)).min((len as usize).saturating_sub(to));

    // Collect values first to avoid aliasing issues.
    let mut vals: Vec<Option<RegisterValue>> = safe_with_capacity(count);
    for i in 0..count {
        if i % OOM_POLL_INTERVAL == 0 {
            runtime.check_oom()?;
        }
        vals.push(array_index_value(
            receiver,
            from + i,
            runtime,
            "copyWithin",
        )?);
    }
    for (i, val) in vals.into_iter().enumerate() {
        if let Some(v) = val
            && let Err(err) = runtime.objects_mut().set_index(receiver, to + i, v)
        {
            return Err(object_error_to_vm_error(runtime, err));
        }
    }
    Ok(*this)
}

// ────────────────────────────────────────────────────────────────────────────
// ES2023 Change-by-Copy methods
// ────────────────────────────────────────────────────────────────────────────

/// `Array.prototype.toReversed()` — §23.1.3.30
/// <https://tc39.es/ecma262/#sec-array.prototype.toreversed>
///
/// Returns a new array with elements in reverse order. Does not mutate the
/// original array.
fn array_to_reversed(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.toReversed requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.toReversed")?;

    let mut elements: Vec<RegisterValue> = safe_with_capacity(length);
    for i in (0..length).rev() {
        if i % OOM_POLL_INTERVAL == 0 {
            runtime.check_oom()?;
        }
        let val = array_index_value(receiver, i, runtime, "Array.prototype.toReversed")?
            .unwrap_or_else(RegisterValue::undefined);
        elements.push(val);
    }
    let result = runtime.alloc_array_with_elements(&elements);
    Ok(RegisterValue::from_object_handle(result.0))
}

/// `Array.prototype.toSorted(compareFn?)` — §23.1.3.31
/// <https://tc39.es/ecma262/#sec-array.prototype.tosorted>
///
/// Returns a new array with elements sorted. Does not mutate the original.
fn array_to_sorted(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.toSorted requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.toSorted")?;

    let comparefn = args
        .first()
        .copied()
        .filter(|v| *v != RegisterValue::undefined())
        .and_then(|v| v.as_object_handle().map(ObjectHandle))
        .filter(|h| runtime.objects().is_callable(*h));

    // Collect all elements (holes become undefined per spec).
    let mut items: Vec<RegisterValue> = safe_with_capacity(length);
    for i in 0..length {
        if i % OOM_POLL_INTERVAL == 0 {
            runtime.check_oom()?;
        }
        let val = array_index_value(receiver, i, runtime, "Array.prototype.toSorted")?
            .unwrap_or_else(RegisterValue::undefined);
        items.push(val);
    }

    // Stable insertion sort (same as Array.prototype.sort).
    for i in 1..items.len() {
        let key = items[i];
        let mut j = i;
        while j > 0 {
            let cmp = sort_compare(items[j - 1], key, comparefn, runtime)?;
            if cmp <= 0.0 {
                break;
            }
            items[j] = items[j - 1];
            j -= 1;
        }
        items[j] = key;
    }

    let result = runtime.alloc_array_with_elements(&items);
    Ok(RegisterValue::from_object_handle(result.0))
}

/// `Array.prototype.toSpliced(start, deleteCount, ...items)` — §23.1.3.32
/// <https://tc39.es/ecma262/#sec-array.prototype.tospliced>
///
/// Returns a new array with elements removed/replaced/inserted. Does not
/// mutate the original.
fn array_to_spliced(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.toSpliced requires array receiver".into())
    })?;
    let len = array_length(receiver, runtime, "Array.prototype.toSpliced")?;
    let len_i = len as i64;

    // Step 3-5: RelativeStart.
    let raw_start = args
        .first()
        .map(|v| runtime.js_to_number(*v).map(|n| n as i64).unwrap_or(0))
        .unwrap_or(0);
    let actual_start = if raw_start < 0 {
        (len_i + raw_start).max(0) as usize
    } else {
        (raw_start as usize).min(len)
    };

    // Step 6-8: ActualDeleteCount.
    let insert_items = if args.len() > 2 { &args[2..] } else { &[] };
    let actual_delete_count = if args.is_empty() {
        0
    } else if args.len() == 1 {
        // No deleteCount → remove everything from start.
        len - actual_start
    } else {
        let raw_dc = runtime.js_to_number(args[1]).unwrap_or(0.0);
        let dc = raw_dc.max(0.0) as usize;
        dc.min(len - actual_start)
    };

    // Build new array: [0..actual_start] + insert_items + [actual_start+actual_delete_count..len].
    let new_len = len - actual_delete_count + insert_items.len();
    // Spec cap (§22.1.3.1 analogue): result length must fit in uint32.
    if new_len > crate::object::MAX_ARRAY_LENGTH {
        return Err(invalid_array_length_error(runtime));
    }
    let mut elements: Vec<RegisterValue> = safe_with_capacity(new_len);

    // Copy before splice point.
    for i in 0..actual_start {
        if i % OOM_POLL_INTERVAL == 0 {
            runtime.check_oom()?;
        }
        let val = array_index_value(receiver, i, runtime, "Array.prototype.toSpliced")?
            .unwrap_or_else(RegisterValue::undefined);
        elements.push(val);
    }

    // Insert new items.
    elements.extend_from_slice(insert_items);

    // Copy after splice point.
    for i in (actual_start + actual_delete_count)..len {
        if i % OOM_POLL_INTERVAL == 0 {
            runtime.check_oom()?;
        }
        let val = array_index_value(receiver, i, runtime, "Array.prototype.toSpliced")?
            .unwrap_or_else(RegisterValue::undefined);
        elements.push(val);
    }

    let result = runtime.alloc_array_with_elements(&elements);
    Ok(RegisterValue::from_object_handle(result.0))
}

/// `Array.prototype.with(index, value)` — §23.1.3.37
/// <https://tc39.es/ecma262/#sec-array.prototype.with>
///
/// Returns a new array identical to the original except the element at `index`
/// is replaced with `value`. Throws **RangeError** if the index is out of
/// bounds.
fn array_with(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.with requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.with")?;
    let len_i = length as i64;

    // Step 3: Let relativeIndex = ToIntegerOrInfinity(index).
    let raw_index = args
        .first()
        .map(|v| runtime.js_to_number(*v).map(|n| n as i64).unwrap_or(0))
        .unwrap_or(0);
    let actual_index = if raw_index < 0 {
        len_i + raw_index
    } else {
        raw_index
    };

    // Step 5: If actualIndex >= len or actualIndex < 0, throw a RangeError.
    if actual_index < 0 || actual_index >= len_i {
        return Err(invalid_array_length_error(runtime));
    }
    let actual_index = actual_index as usize;

    let new_value = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    // Build new array with the replacement.
    let mut elements: Vec<RegisterValue> = safe_with_capacity(length);
    for i in 0..length {
        if i % OOM_POLL_INTERVAL == 0 {
            runtime.check_oom()?;
        }
        if i == actual_index {
            elements.push(new_value);
        } else {
            let val = array_index_value(receiver, i, runtime, "Array.prototype.with")?
                .unwrap_or_else(RegisterValue::undefined);
            elements.push(val);
        }
    }

    let result = runtime.alloc_array_with_elements(&elements);
    Ok(RegisterValue::from_object_handle(result.0))
}

/// ES2024 §23.1.3.1 Array.prototype.at(index)
fn array_at(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.at requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.at")? as i32;
    let index = args.first().and_then(|v| v.as_i32()).unwrap_or(0);
    let actual = if index < 0 { length + index } else { index };
    if actual < 0 || actual >= length {
        return Ok(RegisterValue::undefined());
    }
    Ok(
        array_index_value(receiver, actual as usize, runtime, "Array.prototype.at")?
            .unwrap_or_else(RegisterValue::undefined),
    )
}
