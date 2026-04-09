use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use otter_vm::descriptors::VmNativeCallError;
use otter_vm::object::{HeapValueKind, ObjectHandle};
use otter_vm::payload::{VmTrace, VmValueTracer};
use otter_vm::{RegisterValue, RuntimeState};

use crate::{
    alloc_constructor, bytes_from_buffer_source, class_prototype, has_global, install_getter,
    install_method, link_constructor_and_prototype, type_error,
};

pub(crate) fn install(runtime: &mut RuntimeState) -> Result<(), String> {
    install_blob(runtime)?;
    install_file(runtime)?;
    install_form_data(runtime)?;
    Ok(())
}

#[derive(Debug, Clone)]
pub(crate) struct BlobPayload {
    pub(crate) bytes: Vec<u8>,
    pub(crate) media_type: String,
    pub(crate) file_name: Option<String>,
    pub(crate) last_modified: f64,
}

impl VmTrace for BlobPayload {
    fn trace(&self, _tracer: &mut dyn VmValueTracer) {}
}

#[derive(Debug, Clone)]
struct FormDataEntry {
    name: String,
    value: RegisterValue,
    filename: Option<String>,
}

impl VmTrace for FormDataEntry {
    fn trace(&self, tracer: &mut dyn VmValueTracer) {
        self.value.trace(tracer);
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FormDataPayload {
    entries: Arc<Mutex<Vec<FormDataEntry>>>,
}

impl VmTrace for FormDataPayload {
    fn trace(&self, tracer: &mut dyn VmValueTracer) {
        if let Ok(entries) = self.entries.lock() {
            entries.trace(tracer);
        }
    }
}

fn install_blob(runtime: &mut RuntimeState) -> Result<(), String> {
    if has_global(runtime, "Blob") {
        return Ok(());
    }

    let prototype = runtime.alloc_object();
    for (name, callback, arity, context) in [
        (
            "arrayBuffer",
            blob_array_buffer as _,
            0,
            "Blob.prototype.arrayBuffer",
        ),
        ("slice", blob_slice as _, 3, "Blob.prototype.slice"),
        ("text", blob_text as _, 0, "Blob.prototype.text"),
    ] {
        install_method(runtime, prototype, name, arity, callback, context)?;
    }
    for (name, callback, context) in [
        ("size", blob_get_size as _, "Blob.prototype.size"),
        ("type", blob_get_type as _, "Blob.prototype.type"),
    ] {
        install_getter(runtime, prototype, name, callback, context)?;
    }

    let constructor = alloc_constructor(runtime, "Blob", 0, blob_constructor);
    link_constructor_and_prototype(runtime, constructor, prototype)?;
    runtime.install_global_value("Blob", RegisterValue::from_object_handle(constructor.0));
    Ok(())
}

fn install_file(runtime: &mut RuntimeState) -> Result<(), String> {
    if has_global(runtime, "File") {
        return Ok(());
    }

    let prototype = runtime.alloc_object();
    let blob_prototype = class_prototype(runtime, "Blob")
        .map_err(|error| format!("failed to resolve Blob.prototype: {error:?}"))?;
    let linked = runtime
        .objects_mut()
        .set_prototype(prototype, Some(blob_prototype))
        .map_err(|error| format!("failed to wire File.prototype inheritance: {error:?}"))?;
    if !linked {
        return Err("failed to wire File.prototype inheritance".into());
    }

    for (name, callback, context) in [
        (
            "lastModified",
            file_get_last_modified as _,
            "File.prototype.lastModified",
        ),
        ("name", file_get_name as _, "File.prototype.name"),
        (
            "webkitRelativePath",
            file_get_webkit_relative_path as _,
            "File.prototype.webkitRelativePath",
        ),
    ] {
        install_getter(runtime, prototype, name, callback, context)?;
    }

    let constructor = alloc_constructor(runtime, "File", 2, file_constructor);
    link_constructor_and_prototype(runtime, constructor, prototype)?;
    runtime.install_global_value("File", RegisterValue::from_object_handle(constructor.0));
    Ok(())
}

fn install_form_data(runtime: &mut RuntimeState) -> Result<(), String> {
    if has_global(runtime, "FormData") {
        return Ok(());
    }

    let prototype = runtime.alloc_object();
    for (name, callback, arity, context) in [
        (
            "append",
            form_data_append as _,
            3,
            "FormData.prototype.append",
        ),
        (
            "delete",
            form_data_delete as _,
            1,
            "FormData.prototype.delete",
        ),
        ("get", form_data_get as _, 1, "FormData.prototype.get"),
        (
            "getAll",
            form_data_get_all as _,
            1,
            "FormData.prototype.getAll",
        ),
        ("has", form_data_has as _, 1, "FormData.prototype.has"),
        ("set", form_data_set as _, 3, "FormData.prototype.set"),
    ] {
        install_method(runtime, prototype, name, arity, callback, context)?;
    }

    let constructor = alloc_constructor(runtime, "FormData", 0, form_data_constructor);
    link_constructor_and_prototype(runtime, constructor, prototype)?;
    runtime.install_global_value("FormData", RegisterValue::from_object_handle(constructor.0));
    Ok(())
}

fn blob_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let parts = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let options = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let payload = BlobPayload {
        bytes: parse_blob_parts(runtime, parts)?,
        media_type: parse_blob_type_option(runtime, options)?,
        file_name: None,
        last_modified: 0.0,
    };
    alloc_blob_instance(runtime, "Blob", payload)
}

fn file_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let bits = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let name = normalize_file_name(string_arg(
        runtime,
        args.get(1).copied(),
        "File constructor requires a file name",
    )?);
    let options = args
        .get(2)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let payload = BlobPayload {
        bytes: parse_blob_parts(runtime, bits)?,
        media_type: parse_blob_type_option(runtime, options)?,
        file_name: Some(name),
        last_modified: parse_last_modified_option(runtime, options)?,
    };
    alloc_blob_instance(runtime, "File", payload)
}

fn form_data_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let init = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    if init != RegisterValue::undefined() && init != RegisterValue::null() {
        return Err(type_error(
            runtime,
            "FormData constructor does not yet support HTML form initialization",
        ));
    }

    let prototype = class_prototype(runtime, "FormData")?;
    let instance = runtime.alloc_native_object_with_prototype(
        Some(prototype),
        FormDataPayload {
            entries: Arc::new(Mutex::new(Vec::new())),
        },
    );
    Ok(RegisterValue::from_object_handle(instance.0))
}

fn blob_get_size(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload = require_blob_payload(runtime, this, "Blob.size called on incompatible receiver")?;
    Ok(RegisterValue::from_number(payload.bytes.len() as f64))
}

fn blob_get_type(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload = require_blob_payload(runtime, this, "Blob.type called on incompatible receiver")?;
    Ok(string_value(runtime, payload.media_type))
}

fn blob_text(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload = require_blob_payload(runtime, this, "Blob.text called on incompatible receiver")?;
    let text = string_value(
        runtime,
        String::from_utf8_lossy(&payload.bytes).into_owned(),
    );
    resolved_promise_value(runtime, text)
}

fn blob_array_buffer(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload = require_blob_payload(
        runtime,
        this,
        "Blob.arrayBuffer called on incompatible receiver",
    )?;
    let buffer = alloc_array_buffer(runtime, payload.bytes);
    resolved_promise_value(runtime, RegisterValue::from_object_handle(buffer.0))
}

fn blob_slice(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload =
        require_blob_payload(runtime, this, "Blob.slice called on incompatible receiver")?;
    let len = payload.bytes.len();
    let start = normalize_blob_index(runtime, args.first().copied(), len, false)?;
    let end = normalize_blob_index(runtime, args.get(1).copied(), len, true)?;
    let end = end.max(start);
    let content_type = sanitize_blob_type(&match args.get(2).copied() {
        Some(value) if value != RegisterValue::undefined() => {
            runtime.js_to_string_infallible(value).into_string()
        }
        _ => String::new(),
    });
    let payload = BlobPayload {
        bytes: payload.bytes[start..end].to_vec(),
        media_type: content_type,
        file_name: None,
        last_modified: 0.0,
    };
    alloc_blob_instance(runtime, "Blob", payload)
}

fn file_get_name(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload = require_file_payload(runtime, this)?;
    Ok(string_value(runtime, payload.file_name.unwrap_or_default()))
}

fn file_get_last_modified(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload = require_file_payload(runtime, this)?;
    Ok(RegisterValue::from_number(payload.last_modified))
}

fn file_get_webkit_relative_path(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let _ = require_file_payload(runtime, this)?;
    Ok(string_value(runtime, ""))
}

fn form_data_append(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let entry = build_form_data_entry(runtime, args)?;
    with_form_data_entries(runtime, this, |entries| entries.push(entry))?;
    Ok(RegisterValue::undefined())
}

fn form_data_delete(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let name = string_arg(
        runtime,
        args.first().copied(),
        "FormData.delete requires a field name",
    )?;
    with_form_data_entries(runtime, this, |entries| {
        entries.retain(|entry| entry.name != name)
    })?;
    Ok(RegisterValue::undefined())
}

fn form_data_get(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let name = string_arg(
        runtime,
        args.first().copied(),
        "FormData.get requires a field name",
    )?;
    let value = form_data_entries(runtime, this)?
        .into_iter()
        .find(|entry| entry.name == name)
        .map(|entry| materialize_form_data_value(runtime, &entry))
        .transpose()?;
    Ok(value.unwrap_or_else(RegisterValue::null))
}

fn form_data_get_all(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let name = string_arg(
        runtime,
        args.first().copied(),
        "FormData.getAll requires a field name",
    )?;
    let values: Vec<_> = form_data_entries(runtime, this)?
        .into_iter()
        .filter(|entry| entry.name == name)
        .map(|entry| materialize_form_data_value(runtime, &entry))
        .collect::<Result<Vec<_>, _>>()?;
    let array = runtime.alloc_array_with_elements(&values);
    Ok(RegisterValue::from_object_handle(array.0))
}

fn form_data_has(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let name = string_arg(
        runtime,
        args.first().copied(),
        "FormData.has requires a field name",
    )?;
    Ok(RegisterValue::from_bool(
        form_data_entries(runtime, this)?
            .into_iter()
            .any(|entry| entry.name == name),
    ))
}

fn form_data_set(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let entry = build_form_data_entry(runtime, args)?;
    let field_name = entry.name.clone();
    with_form_data_entries(runtime, this, |entries| {
        let mut next = Vec::with_capacity(entries.len().max(1));
        let mut replaced = false;
        for current in entries.drain(..) {
            if current.name != field_name {
                next.push(current);
                continue;
            }
            if !replaced {
                next.push(entry.clone());
                replaced = true;
            }
        }
        if !replaced {
            next.push(entry);
        }
        *entries = next;
    })?;
    Ok(RegisterValue::undefined())
}

fn build_form_data_entry(
    runtime: &mut RuntimeState,
    args: &[RegisterValue],
) -> Result<FormDataEntry, VmNativeCallError> {
    let name = string_arg(
        runtime,
        args.first().copied(),
        "FormData entry requires a field name",
    )?;
    let value = args
        .get(1)
        .copied()
        .ok_or_else(|| type_error(runtime, "FormData entry requires a value"))?;
    let filename = args
        .get(2)
        .copied()
        .filter(|value| *value != RegisterValue::undefined())
        .map(|value| normalize_file_name(runtime.js_to_string_infallible(value).into_string()));
    Ok(FormDataEntry {
        name,
        value: coerce_form_data_value(runtime, value)?,
        filename,
    })
}

fn coerce_form_data_value(
    runtime: &mut RuntimeState,
    value: RegisterValue,
) -> Result<RegisterValue, VmNativeCallError> {
    if runtime
        .native_payload_from_value::<BlobPayload>(&value)
        .is_ok()
    {
        return Ok(value);
    }

    let stringified = runtime.js_to_string_infallible(value).into_string();
    Ok(string_value(runtime, stringified))
}

fn materialize_form_data_value(
    runtime: &mut RuntimeState,
    entry: &FormDataEntry,
) -> Result<RegisterValue, VmNativeCallError> {
    if let Some(filename) = &entry.filename
        && let Ok(payload) = runtime
            .native_payload_from_value::<BlobPayload>(&entry.value)
            .cloned()
    {
        let last_modified = if payload.file_name.is_some() {
            payload.last_modified
        } else {
            current_time_millis()
        };
        return alloc_blob_instance(
            runtime,
            "File",
            BlobPayload {
                bytes: payload.bytes,
                media_type: payload.media_type,
                file_name: Some(filename.clone()),
                last_modified,
            },
        );
    }
    Ok(entry.value)
}

pub(crate) fn alloc_blob_instance(
    runtime: &mut RuntimeState,
    class_name: &str,
    payload: BlobPayload,
) -> Result<RegisterValue, VmNativeCallError> {
    let prototype = class_prototype(runtime, class_name)?;
    let instance = runtime.alloc_native_object_with_prototype(Some(prototype), payload);
    Ok(RegisterValue::from_object_handle(instance.0))
}

pub(crate) fn require_blob_payload(
    runtime: &mut RuntimeState,
    value: &RegisterValue,
    message: &str,
) -> Result<BlobPayload, VmNativeCallError> {
    runtime
        .native_payload_from_value::<BlobPayload>(value)
        .cloned()
        .map_err(|_| type_error(runtime, message))
}

fn require_file_payload(
    runtime: &mut RuntimeState,
    value: &RegisterValue,
) -> Result<BlobPayload, VmNativeCallError> {
    let payload = require_blob_payload(
        runtime,
        value,
        "File method called on incompatible receiver",
    )?;
    if payload.file_name.is_none() {
        return Err(type_error(
            runtime,
            "File method called on non-File receiver",
        ));
    }
    Ok(payload)
}

fn parse_blob_parts(
    runtime: &mut RuntimeState,
    value: RegisterValue,
) -> Result<Vec<u8>, VmNativeCallError> {
    runtime.check_interrupt()?;
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return Ok(Vec::new());
    }

    let mut bytes = Vec::new();
    if let Some(handle) = value.as_object_handle().map(ObjectHandle)
        && runtime.objects().kind(handle) == Ok(HeapValueKind::Array)
    {
        for part in runtime.array_to_args(handle)? {
            runtime.check_interrupt()?;
            append_blob_part(runtime, &mut bytes, part)?;
        }
        return Ok(bytes);
    }

    append_blob_part(runtime, &mut bytes, value)?;
    Ok(bytes)
}

fn append_blob_part(
    runtime: &mut RuntimeState,
    target: &mut Vec<u8>,
    part: RegisterValue,
) -> Result<(), VmNativeCallError> {
    runtime.check_interrupt()?;
    if let Ok(payload) = runtime.native_payload_from_value::<BlobPayload>(&part) {
        target.extend_from_slice(&payload.bytes);
        return Ok(());
    }

    if let Some(handle) = part.as_object_handle().map(ObjectHandle)
        && let Ok(HeapValueKind::ArrayBuffer | HeapValueKind::TypedArray | HeapValueKind::DataView) =
            runtime.objects().kind(handle)
    {
        target.extend_from_slice(&bytes_from_buffer_source(runtime, part)?);
        return Ok(());
    }

    target.extend_from_slice(
        runtime
            .js_to_string_infallible(part)
            .into_string()
            .as_bytes(),
    );
    Ok(())
}

fn parse_blob_type_option(
    runtime: &mut RuntimeState,
    options: RegisterValue,
) -> Result<String, VmNativeCallError> {
    if options == RegisterValue::undefined() || options == RegisterValue::null() {
        return Ok(String::new());
    }
    let handle = options
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| type_error(runtime, "Blob options must be an object"))?;
    let property = runtime.intern_property_name("type");
    let value = runtime
        .own_property_value(handle, property)
        .unwrap_or_else(|_| RegisterValue::undefined());
    if value == RegisterValue::undefined() {
        return Ok(String::new());
    }
    Ok(sanitize_blob_type(
        &runtime.js_to_string_infallible(value).into_string(),
    ))
}

fn parse_last_modified_option(
    runtime: &mut RuntimeState,
    options: RegisterValue,
) -> Result<f64, VmNativeCallError> {
    if options == RegisterValue::undefined() || options == RegisterValue::null() {
        return Ok(current_time_millis());
    }
    let handle = options
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| type_error(runtime, "File options must be an object"))?;
    let property = runtime.intern_property_name("lastModified");
    let value = runtime
        .own_property_value(handle, property)
        .unwrap_or_else(|_| RegisterValue::undefined());
    if value == RegisterValue::undefined() {
        return Ok(current_time_millis());
    }
    let millis = runtime
        .js_to_number(value)
        .map_err(|_| type_error(runtime, "File lastModified must be coercible to a number"))?;
    Ok(if millis.is_finite() {
        millis.trunc()
    } else {
        0.0
    })
}

fn normalize_blob_index(
    runtime: &mut RuntimeState,
    value: Option<RegisterValue>,
    size: usize,
    default_to_size: bool,
) -> Result<usize, VmNativeCallError> {
    let size = size as i64;
    let Some(value) = value else {
        return Ok(if default_to_size { size as usize } else { 0 });
    };
    if value == RegisterValue::undefined() {
        return Ok(if default_to_size { size as usize } else { 0 });
    }
    let relative = runtime
        .js_to_number(value)
        .map_err(|_| type_error(runtime, "Blob slice bounds must be coercible to numbers"))?;
    if relative.is_nan() {
        return Ok(0);
    }
    let relative = relative.trunc() as i64;
    let offset = if relative < 0 {
        (size + relative).max(0)
    } else {
        relative.min(size)
    };
    Ok(offset as usize)
}

fn form_data_entries(
    runtime: &mut RuntimeState,
    value: &RegisterValue,
) -> Result<Vec<FormDataEntry>, VmNativeCallError> {
    let payload = match runtime.native_payload_from_value::<FormDataPayload>(value) {
        Ok(payload) => payload,
        Err(_) => {
            return Err(type_error(
                runtime,
                "FormData method called on incompatible receiver",
            ));
        }
    };
    let entries = payload
        .entries
        .lock()
        .map_err(|_| VmNativeCallError::Internal("FormData state mutex poisoned".into()))?;
    Ok(entries.clone())
}

fn with_form_data_entries(
    runtime: &mut RuntimeState,
    value: &RegisterValue,
    mutate: impl FnOnce(&mut Vec<FormDataEntry>),
) -> Result<(), VmNativeCallError> {
    let payload = match runtime.native_payload_from_value::<FormDataPayload>(value) {
        Ok(payload) => payload,
        Err(_) => {
            return Err(type_error(
                runtime,
                "FormData method called on incompatible receiver",
            ));
        }
    };
    let mut entries = payload
        .entries
        .lock()
        .map_err(|_| VmNativeCallError::Internal("FormData state mutex poisoned".into()))?;
    mutate(&mut entries);
    Ok(())
}

pub(crate) fn alloc_array_buffer(runtime: &mut RuntimeState, bytes: Vec<u8>) -> ObjectHandle {
    let prototype = Some(runtime.intrinsics().array_buffer_prototype());
    runtime
        .objects_mut()
        .alloc_array_buffer_with_data(bytes, prototype)
}

pub(crate) fn resolved_promise_value(
    runtime: &mut RuntimeState,
    value: RegisterValue,
) -> Result<RegisterValue, VmNativeCallError> {
    let promise = runtime.alloc_fulfilled_vm_promise(value)?;
    Ok(promise.promise_value())
}

fn sanitize_blob_type(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    if !input.bytes().all(|byte| (0x20..=0x7E).contains(&byte)) {
        return String::new();
    }
    input.to_ascii_lowercase()
}

fn normalize_file_name(name: String) -> String {
    name.replace('/', ":")
}

fn string_arg(
    runtime: &mut RuntimeState,
    value: Option<RegisterValue>,
    message: &str,
) -> Result<String, VmNativeCallError> {
    let value = value.ok_or_else(|| type_error(runtime, message))?;
    Ok(runtime.js_to_string_infallible(value).into_string())
}

fn string_value(runtime: &mut RuntimeState, value: impl Into<Box<str>>) -> RegisterValue {
    RegisterValue::from_object_handle(runtime.alloc_string(value).0)
}

fn current_time_millis() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as f64)
        .unwrap_or(0.0)
}
