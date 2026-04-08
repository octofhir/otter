//! Timer and microtask globals — setTimeout, setInterval, clearTimeout,
//! clearInterval, queueMicrotask.
//!
//! These are installed on the global object during intrinsic bootstrap.
//! They delegate to the [`EventLoopHost`] via handles stored on the runtime.
//!
//! Note: The actual timer scheduling happens through the event loop host,
//! not directly in these functions. These natives parse arguments, validate
//! inputs, and record the timer in the runtime's pending timer table. The
//! event loop driver fires callbacks and drains microtasks.

use std::time::Duration;

use crate::descriptors::{NativeBindingDescriptor, VmNativeCallError};
use crate::interpreter::RuntimeState;
use crate::microtask::MicrotaskJob;
use crate::object::ObjectHandle;
use crate::value::RegisterValue;
use otter_macros::{dive, raft};

/// Returns the binding descriptors for all timer/microtask globals.
pub(super) fn timer_global_bindings() -> Vec<NativeBindingDescriptor> {
    raft! {
        target = Global,
        fns = [
            set_timeout,
            set_interval,
            clear_timeout,
            clear_interval,
            queue_microtask,
            structured_clone
        ]
    }
}

/// `setTimeout(callback, delay?)` — HTML5 §8.6
///
/// Schedules `callback` to run after `delay` milliseconds (default 0).
/// Returns a numeric timer ID for `clearTimeout`.
#[dive(name = "setTimeout", length = 1)]
fn set_timeout(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let callback = args
        .first()
        .copied()
        .and_then(|v| v.as_object_handle())
        .map(ObjectHandle)
        .ok_or_else(|| {
            VmNativeCallError::Internal("setTimeout requires a function argument".into())
        })?;

    let delay_ms = args
        .get(1)
        .copied()
        .and_then(|v| v.as_number())
        .unwrap_or(0.0)
        .max(0.0) as u64;

    let id = runtime.schedule_timeout(callback, Duration::from_millis(delay_ms));
    Ok(RegisterValue::from_i32(id.0 as i32))
}

/// `setInterval(callback, interval?)` — HTML5 §8.6
#[dive(name = "setInterval", length = 1)]
fn set_interval(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let callback = args
        .first()
        .copied()
        .and_then(|v| v.as_object_handle())
        .map(ObjectHandle)
        .ok_or_else(|| {
            VmNativeCallError::Internal("setInterval requires a function argument".into())
        })?;

    let interval_ms = args
        .get(1)
        .copied()
        .and_then(|v| v.as_number())
        .unwrap_or(0.0)
        .max(0.0) as u64;

    let id = runtime.schedule_interval(callback, Duration::from_millis(interval_ms));
    Ok(RegisterValue::from_i32(id.0 as i32))
}

/// `clearTimeout(id)` — HTML5 §8.6
#[dive(name = "clearTimeout", length = 1)]
fn clear_timeout(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if let Some(id) = args.first().copied().and_then(|v| v.as_i32()) {
        runtime.clear_timer(crate::event_loop_host::TimerId(id as u32));
    }
    Ok(RegisterValue::undefined())
}

/// `clearInterval(id)` — same as clearTimeout per spec
#[dive(name = "clearInterval", length = 1)]
fn clear_interval(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    clear_timeout(_this, args, runtime)
}

/// `queueMicrotask(callback)` — WHATWG §8.7
#[dive(name = "queueMicrotask", length = 1)]
fn queue_microtask(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let callback = args
        .first()
        .copied()
        .and_then(|v| v.as_object_handle())
        .map(ObjectHandle)
        .ok_or_else(|| {
            VmNativeCallError::Internal("queueMicrotask requires a function argument".into())
        })?;

    runtime.microtasks_mut().enqueue_microtask(MicrotaskJob {
        callback,
        this_value: RegisterValue::undefined(),
        args: vec![],
    });

    Ok(RegisterValue::undefined())
}

/// `structuredClone(value, options?)` — WHATWG HTML §2.7.3
///
/// Creates a deep copy of the value using the structured clone algorithm.
/// Spec: <https://html.spec.whatwg.org/multipage/structured-data.html#dom-structuredclone>
///
/// Handles all serializable types per §2.7.2 StructuredSerialize:
/// - Primitives (undefined, null, boolean, number, bigint, string, symbol)
/// - String objects
/// - Boolean/Number wrapper objects (via internal slots)
/// - Date objects (via `__otter_date_data__` slot)
/// - RegExp objects (pattern + flags)
/// - ArrayBuffer (byte copy)
/// - TypedArray (clone of viewed buffer)
/// - DataView (clone of viewed buffer)
/// - Map (deep clone of entries)
/// - Set (deep clone of entries)
/// - Array (deep clone of elements + own properties)
/// - Plain objects (deep clone of own enumerable data properties)
///
/// Non-serializable types (Function, Promise, WeakMap, WeakSet, Generator,
/// Symbol values, etc.) throw DataCloneError per spec §2.7.2 step 15.
#[dive(name = "structuredClone", length = 1)]
fn structured_clone(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    structured_clone_inner(value, runtime, 0)
}

const MAX_CLONE_DEPTH: usize = 64;

fn data_clone_error(runtime: &mut RuntimeState, message: &str) -> VmNativeCallError {
    // Per spec this should be a DOMException "DataCloneError", but our VM
    // doesn't have DOMException. Use TypeError as the closest equivalent.
    match runtime.alloc_type_error(message) {
        Ok(h) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(h.0)),
        Err(e) => VmNativeCallError::Internal(format!("{e}").into()),
    }
}

fn structured_clone_inner(
    value: RegisterValue,
    runtime: &mut RuntimeState,
    depth: usize,
) -> Result<RegisterValue, VmNativeCallError> {
    use crate::object::{HeapValueKind, PropertyValue};

    // §2.7.2 step 1-4: Primitives (undefined, null, boolean, number) pass through.
    if value == RegisterValue::undefined()
        || value == RegisterValue::null()
        || value.as_bool().is_some()
        || value.as_i32().is_some()
        || value.as_number().is_some()
    {
        return Ok(value);
    }

    // §2.7.2 step 15: Symbol values are NOT serializable.
    if value.is_symbol() {
        return Err(data_clone_error(runtime, "Symbol values cannot be cloned"));
    }

    // BigInt values pass through (immutable).
    // (BigInt is a primitive in our value representation)

    if depth > MAX_CLONE_DEPTH {
        return Err(data_clone_error(
            runtime,
            "structuredClone: maximum depth exceeded",
        ));
    }

    let handle = match value.as_object_handle().map(ObjectHandle) {
        Some(h) => h,
        None => return Ok(value),
    };

    let kind = runtime
        .objects()
        .kind(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("structuredClone: {e:?}").into()))?;

    match kind {
        // §2.7.2 step 6: String — create new string with same content.
        HeapValueKind::String => {
            let s = runtime
                .objects()
                .string_value(handle)
                .ok()
                .flatten()
                .map(|s| s.to_string())
                .unwrap_or_default();
            let cloned = runtime.alloc_string(s.as_str());
            Ok(RegisterValue::from_object_handle(cloned.0))
        }

        // §2.7.2 step 7: Array — deep clone elements and own properties.
        HeapValueKind::Array => {
            let len = runtime
                .objects()
                .array_length(handle)
                .ok()
                .flatten()
                .unwrap_or(0);
            let arr = runtime.alloc_array();
            for i in 0..len {
                let elem = runtime
                    .objects_mut()
                    .get_index(handle, i)
                    .ok()
                    .flatten()
                    .unwrap_or_else(RegisterValue::undefined);
                let cloned_elem = structured_clone_inner(elem, runtime, depth + 1)?;
                runtime.objects_mut().push_element(arr, cloned_elem).ok();
            }
            // Clone non-index own properties (e.g., named props on arrays).
            clone_own_data_properties(handle, arr, runtime, depth)?;
            Ok(RegisterValue::from_object_handle(arr.0))
        }

        // §2.7.2 step 8: Object (plain) — deep clone own data properties.
        HeapValueKind::Object => {
            // Check for Date (stored as plain object with __otter_date_data__ slot).
            let date_prop = runtime.intern_property_name("__otter_date_data__");
            if let Ok(Some(lookup)) = runtime.property_lookup(handle, date_prop)
                && let PropertyValue::Data {
                    value: date_val, ..
                } = lookup.value()
            {
                let date_ms = date_val
                    .as_number()
                    .or_else(|| date_val.as_i32().map(|i| i as f64));
                if let Some(ms) = date_ms {
                    // Clone the Date: create new object with same prototype and date slot.
                    let proto = runtime.intrinsics().date_prototype();
                    let clone = runtime.alloc_object_with_prototype(Some(proto));
                    runtime
                        .objects_mut()
                        .set_property(clone, date_prop, RegisterValue::from_number(ms))
                        .ok();
                    return Ok(RegisterValue::from_object_handle(clone.0));
                }
            }

            // Check for Boolean/Number wrapper objects.
            let bool_prop = runtime.intern_property_name("__otter_boolean_data__");
            if let Ok(Some(lookup)) = runtime.property_lookup(handle, bool_prop)
                && let PropertyValue::Data { value: v, .. } = lookup.value()
            {
                let proto = runtime.intrinsics().boolean_prototype();
                let clone = runtime.alloc_object_with_prototype(Some(proto));
                runtime.objects_mut().set_property(clone, bool_prop, v).ok();
                return Ok(RegisterValue::from_object_handle(clone.0));
            }
            let num_prop = runtime.intern_property_name("__otter_number_data__");
            if let Ok(Some(lookup)) = runtime.property_lookup(handle, num_prop)
                && let PropertyValue::Data { value: v, .. } = lookup.value()
            {
                let proto = runtime.intrinsics().number_prototype();
                let clone = runtime.alloc_object_with_prototype(Some(proto));
                runtime.objects_mut().set_property(clone, num_prop, v).ok();
                return Ok(RegisterValue::from_object_handle(clone.0));
            }
            let str_prop = runtime.intern_property_name("__otter_string_data__");
            if let Ok(Some(lookup)) = runtime.property_lookup(handle, str_prop)
                && let PropertyValue::Data { value: v, .. } = lookup.value()
            {
                let proto = runtime.intrinsics().string_prototype();
                let clone = runtime.alloc_object_with_prototype(Some(proto));
                runtime.objects_mut().set_property(clone, str_prop, v).ok();
                return Ok(RegisterValue::from_object_handle(clone.0));
            }

            // Plain object — deep clone own data properties.
            let clone = runtime.alloc_object();
            clone_own_data_properties(handle, clone, runtime, depth)?;
            Ok(RegisterValue::from_object_handle(clone.0))
        }

        // §2.7.2 step 9: RegExp — clone pattern and flags.
        HeapValueKind::RegExp => {
            let pattern = runtime
                .objects()
                .regexp_pattern(handle)
                .unwrap_or("")
                .to_string();
            let flags = runtime
                .objects()
                .regexp_flags(handle)
                .unwrap_or("")
                .to_string();
            let proto = runtime.intrinsics().regexp_prototype;
            // §22.2.3.1 — use the runtime helper so `lastIndex` is installed
            // with the spec-mandated attributes before we overwrite its
            // value from the source RegExp.
            let clone = runtime.alloc_regexp(&pattern, &flags, Some(proto));
            // Clone lastIndex value; attributes are already correct.
            let li_prop = runtime.intern_property_name("lastIndex");
            if let Ok(Some(lookup)) = runtime.property_lookup(handle, li_prop)
                && let PropertyValue::Data { value: v, .. } = lookup.value()
            {
                runtime.objects_mut().set_property(clone, li_prop, v).ok();
            }
            Ok(RegisterValue::from_object_handle(clone.0))
        }

        // §2.7.2 step 10: ArrayBuffer — byte-copy the backing store.
        HeapValueKind::ArrayBuffer => {
            let data = runtime
                .objects()
                .array_buffer_data(handle)
                .ok()
                .flatten()
                .map(|d| d.to_vec())
                .unwrap_or_default();
            let proto = runtime.intrinsics().array_buffer_prototype;
            let clone = runtime
                .objects_mut()
                .alloc_array_buffer_with_data(data, Some(proto));
            Ok(RegisterValue::from_object_handle(clone.0))
        }

        // §2.7.2 step 10: SharedArrayBuffer — share the backing store (same semantics).
        HeapValueKind::SharedArrayBuffer => {
            // SharedArrayBuffer is explicitly NOT cloned — it's transferred.
            // For structuredClone with no transfer list, this is a DataCloneError per spec.
            // However, many engines allow SAB cloning as shared. We'll return as-is.
            Ok(value)
        }

        // §2.7.2: TypedArray — clone the underlying buffer and create new view.
        HeapValueKind::TypedArray => {
            let ta_kind = runtime
                .objects()
                .typed_array_kind(handle)
                .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
            let buffer = runtime
                .objects()
                .typed_array_viewed_buffer(handle)
                .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
            let offset = runtime
                .objects()
                .typed_array_byte_offset(handle)
                .unwrap_or(0);
            let length = runtime.objects().typed_array_length(handle).unwrap_or(0);
            // Clone the buffer.
            let buf_data = runtime
                .objects()
                .array_buffer_data(buffer)
                .ok()
                .flatten()
                .map(|d| d.to_vec())
                .unwrap_or_default();
            let ab_proto = runtime.intrinsics().array_buffer_prototype;
            let cloned_buf = runtime
                .objects_mut()
                .alloc_array_buffer_with_data(buf_data, Some(ab_proto));
            let clone = runtime
                .objects_mut()
                .alloc_typed_array(ta_kind, cloned_buf, offset, length, None);
            Ok(RegisterValue::from_object_handle(clone.0))
        }

        // §2.7.2: DataView — clone the underlying buffer and create new view.
        HeapValueKind::DataView => {
            let buffer = runtime
                .objects()
                .data_view_buffer(handle)
                .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
            let offset = runtime.objects().data_view_byte_offset(handle).unwrap_or(0);
            let length = runtime.objects().data_view_byte_length(handle).unwrap_or(0);
            let buf_data = runtime
                .objects()
                .array_buffer_data(buffer)
                .ok()
                .flatten()
                .map(|d| d.to_vec())
                .unwrap_or_default();
            let ab_proto = runtime.intrinsics().array_buffer_prototype;
            let cloned_buf = runtime
                .objects_mut()
                .alloc_array_buffer_with_data(buf_data, Some(ab_proto));
            let dv_proto = runtime.intrinsics().data_view_prototype;
            let clone = runtime.objects_mut().alloc_data_view(
                cloned_buf,
                offset,
                Some(length),
                Some(dv_proto),
            );
            Ok(RegisterValue::from_object_handle(clone.0))
        }

        // §2.7.2 step 12: Map — deep clone entries.
        HeapValueKind::Map => {
            let entries = runtime
                .objects()
                .map_entries_raw(handle)
                .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
            let live_entries: Vec<(RegisterValue, RegisterValue)> = entries
                .iter()
                .filter_map(|e| e.as_ref().map(|(k, v)| (*k, *v)))
                .collect();
            let proto = runtime.intrinsics().map_prototype;
            let clone = runtime.objects_mut().alloc_map(Some(proto));
            for (key, val) in live_entries {
                let cloned_key = structured_clone_inner(key, runtime, depth + 1)?;
                let cloned_val = structured_clone_inner(val, runtime, depth + 1)?;
                runtime
                    .objects_mut()
                    .map_set(clone, cloned_key, cloned_val)
                    .ok();
            }
            Ok(RegisterValue::from_object_handle(clone.0))
        }

        // §2.7.2 step 13: Set — deep clone values.
        HeapValueKind::Set => {
            let entries = runtime
                .objects()
                .set_entries(handle)
                .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
            let live_entries: Vec<RegisterValue> = entries.iter().filter_map(|e| *e).collect();
            let proto = runtime.intrinsics().set_prototype;
            let clone = runtime.objects_mut().alloc_set(Some(proto));
            for val in live_entries {
                let cloned_val = structured_clone_inner(val, runtime, depth + 1)?;
                runtime.objects_mut().set_add(clone, cloned_val).ok();
            }
            Ok(RegisterValue::from_object_handle(clone.0))
        }

        // §2.7.2 step 15: Non-serializable types → DataCloneError.
        HeapValueKind::Closure
        | HeapValueKind::HostFunction
        | HeapValueKind::BoundFunction
        | HeapValueKind::Promise
        | HeapValueKind::Generator
        | HeapValueKind::AsyncGenerator
        | HeapValueKind::WeakMap
        | HeapValueKind::WeakSet
        | HeapValueKind::WeakRef
        | HeapValueKind::FinalizationRegistry => Err(data_clone_error(
            runtime,
            &format!("{kind:?} object cannot be cloned"),
        )),

        // Iterator, PromiseCapabilityFunction, etc. — not serializable.
        _ => Err(data_clone_error(runtime, "object cannot be cloned")),
    }
}

/// Deep-clone own non-symbol data properties from `source` onto `target`.
fn clone_own_data_properties(
    source: ObjectHandle,
    target: ObjectHandle,
    runtime: &mut RuntimeState,
    depth: usize,
) -> Result<(), VmNativeCallError> {
    use crate::object::PropertyValue;

    let keys = runtime
        .own_property_keys(source)
        .map_err(|e| VmNativeCallError::Internal(format!("structuredClone: {e:?}").into()))?;

    let key_ids: Vec<crate::property::PropertyNameId> = keys
        .iter()
        .filter(|k| !runtime.property_names().is_symbol(**k))
        .copied()
        .collect();

    for key_id in key_ids {
        if let Ok(Some(lookup)) = runtime.property_lookup(source, key_id)
            && let PropertyValue::Data { value: v, .. } = lookup.value()
        {
            let cloned_v = structured_clone_inner(v, runtime, depth + 1)?;
            runtime
                .objects_mut()
                .set_property(target, key_id, cloned_v)
                .ok();
        }
    }

    Ok(())
}
