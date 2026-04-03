use std::sync::{Arc, Mutex};

use otter_vm::descriptors::VmNativeCallError;
use otter_vm::object::{HeapValueKind, ObjectHandle};
use otter_vm::payload::{VmTrace, VmValueTracer};
use otter_vm::{RegisterValue, RuntimeState};

use crate::{
    alloc_constructor, class_prototype, has_global, install_method, link_constructor_and_prototype,
    type_error,
};

pub(crate) fn install(runtime: &mut RuntimeState) -> Result<(), String> {
    if has_global(runtime, "Headers") {
        return Ok(());
    }

    let prototype = runtime.alloc_object();
    for (name, callback, arity, context) in [
        ("append", headers_append as _, 2, "Headers.prototype.append"),
        ("delete", headers_delete as _, 1, "Headers.prototype.delete"),
        ("get", headers_get as _, 1, "Headers.prototype.get"),
        (
            "getSetCookie",
            headers_get_set_cookie as _,
            0,
            "Headers.prototype.getSetCookie",
        ),
        ("has", headers_has as _, 1, "Headers.prototype.has"),
        ("set", headers_set as _, 2, "Headers.prototype.set"),
    ] {
        install_method(runtime, prototype, name, arity, callback, context)?;
    }

    let constructor = alloc_constructor(runtime, "Headers", 1, headers_constructor);
    link_constructor_and_prototype(runtime, constructor, prototype)?;
    runtime.install_global_value("Headers", RegisterValue::from_object_handle(constructor.0));
    Ok(())
}

#[derive(Debug, Clone)]
struct HeadersPayload {
    entries: Arc<Mutex<Vec<(String, String)>>>,
}

impl VmTrace for HeadersPayload {
    fn trace(&self, _tracer: &mut dyn VmValueTracer) {}
}

fn headers_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let init = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let entries = parse_headers_init(runtime, init)?;
    alloc_headers_instance(runtime, entries)
}

fn headers_append(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let name = normalize_header_name(runtime, args.first().copied())?;
    let value = normalize_header_value(runtime, args.get(1).copied())?;
    with_headers_mut(runtime, this, |entries| entries.push((name, value)))?;
    Ok(RegisterValue::undefined())
}

fn headers_delete(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let name = normalize_header_name(runtime, args.first().copied())?;
    with_headers_mut(runtime, this, |entries| {
        entries.retain(|(key, _)| key != &name)
    })?;
    Ok(RegisterValue::undefined())
}

fn headers_get(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let name = normalize_header_name(runtime, args.first().copied())?;
    let value = collect_header_values(&header_entries(runtime, this)?, &name);
    Ok(match value {
        Some(value) => string_value(runtime, value),
        None => RegisterValue::null(),
    })
}

fn headers_get_set_cookie(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let values: Vec<_> = header_entries(runtime, this)?
        .into_iter()
        .filter_map(|(name, value)| (name == "set-cookie").then_some(string_value(runtime, value)))
        .collect();
    let array = runtime.alloc_array_with_elements(&values);
    Ok(RegisterValue::from_object_handle(array.0))
}

fn headers_has(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let name = normalize_header_name(runtime, args.first().copied())?;
    Ok(RegisterValue::from_bool(
        header_entries(runtime, this)?
            .into_iter()
            .any(|(key, _)| key == name),
    ))
}

fn headers_set(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let name = normalize_header_name(runtime, args.first().copied())?;
    let value = normalize_header_value(runtime, args.get(1).copied())?;
    with_headers_mut(runtime, this, |entries| {
        if let Some(index) = entries.iter().position(|(key, _)| key == &name) {
            let mut next = Vec::with_capacity(entries.len());
            for (entry_index, (key, current)) in entries.drain(..).enumerate() {
                if key != name {
                    next.push((key, current));
                } else if entry_index == index {
                    next.push((key, value.clone()));
                }
            }
            *entries = next;
        } else {
            entries.push((name, value.clone()));
        }
    })?;
    Ok(RegisterValue::undefined())
}

pub(crate) fn parse_headers_init(
    runtime: &mut RuntimeState,
    value: RegisterValue,
) -> Result<Vec<(String, String)>, VmNativeCallError> {
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return Ok(Vec::new());
    }

    if let Some(entries) = runtime
        .native_payload_from_value::<HeadersPayload>(&value)
        .ok()
        .map(|payload| payload.entries.clone())
    {
        let entries = entries
            .lock()
            .map_err(|_| VmNativeCallError::Internal("Headers state mutex poisoned".into()))?;
        return Ok(entries.clone());
    }

    let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
        return Err(type_error(
            runtime,
            "Headers constructor init must be a Headers, object, or sequence",
        ));
    };

    match runtime.objects().kind(handle) {
        Ok(HeapValueKind::Array) => parse_headers_sequence(runtime, handle),
        Ok(HeapValueKind::String) => Err(type_error(
            runtime,
            "Headers constructor does not accept a string init",
        )),
        _ => parse_headers_record(runtime, handle),
    }
}

fn parse_headers_sequence(
    runtime: &mut RuntimeState,
    handle: ObjectHandle,
) -> Result<Vec<(String, String)>, VmNativeCallError> {
    let values = runtime.array_to_args(handle)?;
    let mut entries = Vec::with_capacity(values.len());
    for value in values {
        let tuple = value.as_object_handle().map(ObjectHandle).ok_or_else(|| {
            type_error(
                runtime,
                "Headers sequence init requires [name, value] tuples",
            )
        })?;
        if runtime.objects().kind(tuple) != Ok(HeapValueKind::Array) {
            return Err(type_error(
                runtime,
                "Headers sequence init requires [name, value] tuples",
            ));
        }
        let parts = runtime.array_to_args(tuple)?;
        if parts.len() < 2 {
            return Err(type_error(
                runtime,
                "Headers tuple init requires [name, value]",
            ));
        }
        entries.push((
            normalize_header_name(runtime, Some(parts[0]))?,
            normalize_header_value(runtime, Some(parts[1]))?,
        ));
    }
    Ok(entries)
}

fn parse_headers_record(
    runtime: &mut RuntimeState,
    handle: ObjectHandle,
) -> Result<Vec<(String, String)>, VmNativeCallError> {
    let mut entries = Vec::new();
    for key in runtime.enumerable_own_property_keys(handle)? {
        let Some(name) = runtime.property_names().get(key).map(str::to_owned) else {
            continue;
        };
        let value = runtime
            .own_property_value(handle, key)
            .unwrap_or_else(|_| RegisterValue::undefined());
        entries.push((name, normalize_header_value(runtime, Some(value))?));
    }
    Ok(entries
        .into_iter()
        .map(|(name, value)| normalize_parsed_header(runtime, name, value))
        .collect::<Result<Vec<_>, _>>()?)
}

fn normalize_parsed_header(
    runtime: &mut RuntimeState,
    name: String,
    value: String,
) -> Result<(String, String), VmNativeCallError> {
    Ok((
        normalize_header_name_string(runtime, name)?,
        normalize_header_value_string(runtime, value)?,
    ))
}

fn require_headers_entries(
    runtime: &mut RuntimeState,
    value: &RegisterValue,
) -> Result<Arc<Mutex<Vec<(String, String)>>>, VmNativeCallError> {
    let Ok(payload) = runtime.native_payload_from_value::<HeadersPayload>(value) else {
        return Err(type_error(
            runtime,
            "Headers method called on incompatible receiver",
        ));
    };
    Ok(payload.entries.clone())
}

pub(crate) fn header_entries(
    runtime: &mut RuntimeState,
    value: &RegisterValue,
) -> Result<Vec<(String, String)>, VmNativeCallError> {
    let entries = require_headers_entries(runtime, value)?;
    let entries = entries
        .lock()
        .map_err(|_| VmNativeCallError::Internal("Headers state mutex poisoned".into()))?;
    Ok(entries.clone())
}

pub(crate) fn alloc_headers_instance(
    runtime: &mut RuntimeState,
    entries: Vec<(String, String)>,
) -> Result<RegisterValue, VmNativeCallError> {
    let prototype = class_prototype(runtime, "Headers")?;
    let instance = runtime.alloc_native_object_with_prototype(
        Some(prototype),
        HeadersPayload {
            entries: Arc::new(Mutex::new(entries)),
        },
    );
    Ok(RegisterValue::from_object_handle(instance.0))
}

fn with_headers_mut(
    runtime: &mut RuntimeState,
    value: &RegisterValue,
    mutate: impl FnOnce(&mut Vec<(String, String)>),
) -> Result<(), VmNativeCallError> {
    let entries = require_headers_entries(runtime, value)?;
    let mut entries = entries
        .lock()
        .map_err(|_| VmNativeCallError::Internal("Headers state mutex poisoned".into()))?;
    mutate(&mut entries);
    Ok(())
}

fn collect_header_values(entries: &[(String, String)], name: &str) -> Option<String> {
    let mut iter = entries
        .iter()
        .filter_map(|(key, value)| (key == name).then_some(value.as_str()));
    let first = iter.next()?;
    let mut combined = first.to_string();
    for value in iter {
        combined.push_str(", ");
        combined.push_str(value);
    }
    Some(combined)
}

fn normalize_header_name(
    runtime: &mut RuntimeState,
    value: Option<RegisterValue>,
) -> Result<String, VmNativeCallError> {
    let value = value.ok_or_else(|| type_error(runtime, "Headers header name is required"))?;
    let name = runtime.js_to_string_infallible(value).into_string();
    normalize_header_name_string(runtime, name)
}

fn normalize_header_name_string(
    runtime: &mut RuntimeState,
    name: String,
) -> Result<String, VmNativeCallError> {
    if name.is_empty() || !name.bytes().all(is_header_name_byte) {
        return Err(type_error(
            runtime,
            "Headers contains an invalid header name",
        ));
    }
    Ok(name.to_ascii_lowercase())
}

fn normalize_header_value(
    runtime: &mut RuntimeState,
    value: Option<RegisterValue>,
) -> Result<String, VmNativeCallError> {
    let value = value.ok_or_else(|| type_error(runtime, "Headers header value is required"))?;
    let value = runtime.js_to_string_infallible(value).into_string();
    normalize_header_value_string(runtime, value)
}

fn normalize_header_value_string(
    runtime: &mut RuntimeState,
    value: String,
) -> Result<String, VmNativeCallError> {
    if value.contains('\r') || value.contains('\n') || value.contains('\0') {
        return Err(type_error(
            runtime,
            "Headers contains an invalid header value",
        ));
    }
    Ok(value.trim_matches([' ', '\t']).to_string())
}

fn is_header_name_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'\''
            | b'*'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~'
    ) || byte.is_ascii_alphanumeric()
}

fn string_value(runtime: &mut RuntimeState, value: impl Into<Box<str>>) -> RegisterValue {
    RegisterValue::from_object_handle(runtime.alloc_string(value).0)
}
