use std::sync::{Arc, Mutex};

use otter_runtime::current_capabilities;
use otter_vm::descriptors::VmNativeCallError;
use otter_vm::object::{HeapValueKind, ObjectHandle};
use otter_vm::payload::{VmTrace, VmValueTracer};
use otter_vm::{RegisterValue, RuntimeState};
use reqwest::Method;
use url::Url;

use crate::blob_api::{
    BlobPayload, FormDataPayload, alloc_array_buffer, alloc_blob_instance, require_blob_payload,
};
use crate::headers_api::{alloc_headers_instance, header_entries, parse_headers_init};
use crate::url_api::serialize_url_search_params_value;
use crate::{
    alloc_constructor, bytes_from_buffer_source, class_prototype, has_global, install_getter,
    install_method, link_constructor_and_prototype, type_error,
};

pub(crate) fn install(runtime: &mut RuntimeState) -> Result<(), String> {
    install_request(runtime)?;
    install_response(runtime)?;
    install_fetch(runtime)?;
    Ok(())
}

#[derive(Debug, Clone)]
struct BodyState {
    bytes: Vec<u8>,
    used: bool,
}

#[derive(Debug, Clone)]
struct RequestPayload {
    method: String,
    url: String,
    headers: RegisterValue,
    body: Arc<Mutex<BodyState>>,
}

impl VmTrace for RequestPayload {
    fn trace(&self, tracer: &mut dyn VmValueTracer) {
        self.headers.trace(tracer);
    }
}

#[derive(Debug, Clone)]
struct ResponsePayload {
    status: u16,
    status_text: String,
    headers: RegisterValue,
    body: Arc<Mutex<BodyState>>,
    url: String,
}

impl VmTrace for ResponsePayload {
    fn trace(&self, tracer: &mut dyn VmValueTracer) {
        self.headers.trace(tracer);
    }
}

#[derive(Debug, Clone)]
struct ParsedBodyInit {
    bytes: Vec<u8>,
    content_type: Option<String>,
    present: bool,
}

struct FetchRequestState {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct FetchResponseState {
    url: String,
    status: u16,
    status_text: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn install_request(runtime: &mut RuntimeState) -> Result<(), String> {
    if has_global(runtime, "Request") {
        return Ok(());
    }

    let prototype = runtime.alloc_object();
    for (name, callback, arity, context) in [
        (
            "arrayBuffer",
            request_or_response_array_buffer as _,
            0,
            "Request.prototype.arrayBuffer",
        ),
        (
            "blob",
            request_or_response_blob as _,
            0,
            "Request.prototype.blob",
        ),
        ("clone", request_clone as _, 0, "Request.prototype.clone"),
        (
            "json",
            request_or_response_json as _,
            0,
            "Request.prototype.json",
        ),
        (
            "text",
            request_or_response_text as _,
            0,
            "Request.prototype.text",
        ),
    ] {
        install_method(runtime, prototype, name, arity, callback, context)?;
    }
    for (name, callback, context) in [
        ("body", request_get_body as _, "Request.prototype.body"),
        (
            "bodyUsed",
            request_get_body_used as _,
            "Request.prototype.bodyUsed",
        ),
        (
            "headers",
            request_get_headers as _,
            "Request.prototype.headers",
        ),
        (
            "method",
            request_get_method as _,
            "Request.prototype.method",
        ),
        ("url", request_get_url as _, "Request.prototype.url"),
    ] {
        install_getter(runtime, prototype, name, callback, context)?;
    }

    let constructor = alloc_constructor(runtime, "Request", 1, request_constructor);
    link_constructor_and_prototype(runtime, constructor, prototype)?;
    runtime.install_global_value("Request", RegisterValue::from_object_handle(constructor.0));
    Ok(())
}

fn install_fetch(runtime: &mut RuntimeState) -> Result<(), String> {
    if has_global(runtime, "fetch") {
        return Ok(());
    }

    let descriptor = otter_vm::NativeFunctionDescriptor::method("fetch", 1, fetch_global);
    let id = runtime.register_native_function(descriptor);
    let function = runtime.alloc_host_function(id);
    runtime.install_global_value("fetch", RegisterValue::from_object_handle(function.0));
    Ok(())
}

fn install_response(runtime: &mut RuntimeState) -> Result<(), String> {
    if has_global(runtime, "Response") {
        return Ok(());
    }

    let prototype = runtime.alloc_object();
    for (name, callback, arity, context) in [
        (
            "arrayBuffer",
            request_or_response_array_buffer as _,
            0,
            "Response.prototype.arrayBuffer",
        ),
        (
            "blob",
            request_or_response_blob as _,
            0,
            "Response.prototype.blob",
        ),
        ("clone", response_clone as _, 0, "Response.prototype.clone"),
        (
            "json",
            request_or_response_json as _,
            0,
            "Response.prototype.json",
        ),
        (
            "text",
            request_or_response_text as _,
            0,
            "Response.prototype.text",
        ),
    ] {
        install_method(runtime, prototype, name, arity, callback, context)?;
    }
    for (name, callback, context) in [
        ("body", response_get_body as _, "Response.prototype.body"),
        (
            "bodyUsed",
            response_get_body_used as _,
            "Response.prototype.bodyUsed",
        ),
        (
            "headers",
            response_get_headers as _,
            "Response.prototype.headers",
        ),
        ("ok", response_get_ok as _, "Response.prototype.ok"),
        (
            "redirected",
            response_get_redirected as _,
            "Response.prototype.redirected",
        ),
        (
            "status",
            response_get_status as _,
            "Response.prototype.status",
        ),
        (
            "statusText",
            response_get_status_text as _,
            "Response.prototype.statusText",
        ),
        ("type", response_get_type as _, "Response.prototype.type"),
        ("url", response_get_url as _, "Response.prototype.url"),
    ] {
        install_getter(runtime, prototype, name, callback, context)?;
    }

    let constructor = alloc_constructor(runtime, "Response", 0, response_constructor);
    link_constructor_and_prototype(runtime, constructor, prototype)?;
    install_method(
        runtime,
        constructor,
        "json",
        2,
        response_json_static,
        "Response.json",
    )?;
    runtime.install_global_value("Response", RegisterValue::from_object_handle(constructor.0));
    Ok(())
}

fn request_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let input = args
        .first()
        .copied()
        .ok_or_else(|| type_error(runtime, "Request constructor requires an input"))?;
    let init = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let payload = build_request_payload(runtime, input, init)?;
    let prototype = class_prototype(runtime, "Request")?;
    let instance = runtime.alloc_native_object_with_prototype(Some(prototype), payload);
    Ok(RegisterValue::from_object_handle(instance.0))
}

fn response_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let body = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let init = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let payload = build_response_payload(runtime, body, init)?;
    alloc_response_instance(runtime, payload)
}

fn request_get_method(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload = require_request_payload(runtime, this)?;
    Ok(string_value(runtime, payload.method))
}

fn request_get_url(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload = require_request_payload(runtime, this)?;
    Ok(string_value(runtime, payload.url))
}

fn request_get_headers(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload = require_request_payload(runtime, this)?;
    Ok(payload.headers)
}

fn request_get_body(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let _ = require_request_payload(runtime, this)?;
    request_or_response_body(this, args, runtime)
}

fn request_get_body_used(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let _ = require_request_payload(runtime, this)?;
    request_or_response_body_used(this, args, runtime)
}

fn request_clone(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload = require_request_payload(runtime, this)?;
    if body_is_used(&payload.body)? {
        return Err(type_error(
            runtime,
            "Cannot clone a Request whose body is already used",
        ));
    }
    let cloned = RequestPayload {
        method: payload.method,
        url: payload.url,
        headers: clone_headers(runtime, payload.headers)?,
        body: clone_body_state(&payload.body)?,
    };
    let prototype = class_prototype(runtime, "Request")?;
    let instance = runtime.alloc_native_object_with_prototype(Some(prototype), cloned);
    Ok(RegisterValue::from_object_handle(instance.0))
}

fn response_get_status(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload = require_response_payload(runtime, this)?;
    Ok(RegisterValue::from_i32(i32::from(payload.status)))
}

fn response_get_status_text(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload = require_response_payload(runtime, this)?;
    Ok(string_value(runtime, payload.status_text))
}

fn response_get_ok(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload = require_response_payload(runtime, this)?;
    Ok(RegisterValue::from_bool(
        (200..=299).contains(&payload.status),
    ))
}

fn response_get_headers(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload = require_response_payload(runtime, this)?;
    Ok(payload.headers)
}

fn response_get_url(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload = require_response_payload(runtime, this)?;
    Ok(string_value(runtime, payload.url))
}

fn response_get_type(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let _ = require_response_payload(runtime, this)?;
    Ok(string_value(runtime, "default"))
}

fn response_get_redirected(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let _ = require_response_payload(runtime, this)?;
    Ok(RegisterValue::from_bool(false))
}

fn response_get_body(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let _ = require_response_payload(runtime, this)?;
    request_or_response_body(this, args, runtime)
}

fn response_get_body_used(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let _ = require_response_payload(runtime, this)?;
    request_or_response_body_used(this, args, runtime)
}

fn response_clone(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload = require_response_payload(runtime, this)?;
    if body_is_used(&payload.body)? {
        return Err(type_error(
            runtime,
            "Cannot clone a Response whose body is already used",
        ));
    }
    let headers = clone_headers(runtime, payload.headers)?;
    let cloned = ResponsePayload {
        status: payload.status,
        status_text: payload.status_text,
        headers,
        body: clone_body_state(&payload.body)?,
        url: payload.url,
    };
    alloc_response_instance(runtime, cloned)
}

fn response_json_static(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let init = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let stringified = call_json_method(runtime, "stringify", &[value])?;
    if stringified == RegisterValue::undefined() {
        return Err(type_error(
            runtime,
            "Response.json value is not JSON serializable",
        ));
    }
    let text = runtime.js_to_string_infallible(stringified).into_string();
    let body = string_value(runtime, text.clone());
    let mut payload = build_response_payload(runtime, body, init)?;
    payload.headers = replace_content_type_header(runtime, payload.headers, "application/json")?;
    payload.body = Arc::new(Mutex::new(BodyState {
        bytes: text.into_bytes(),
        used: false,
    }));
    alloc_response_instance(runtime, payload)
}

fn fetch_global(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let input = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let init = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let request = match build_request_payload(runtime, input, init) {
        Ok(payload) => payload,
        Err(VmNativeCallError::Thrown(reason)) => return rejected_promise_value(runtime, reason),
        Err(VmNativeCallError::Internal(message)) => {
            let reason = type_error_value(runtime, &message)?;
            return rejected_promise_value(runtime, reason);
        }
    };

    let parsed_url = match Url::parse(&request.url) {
        Ok(url) => url,
        Err(_) => {
            let reason = type_error_value(runtime, "fetch URL is invalid")?;
            return rejected_promise_value(runtime, reason);
        }
    };
    let Some(host) = parsed_url.host_str() else {
        let reason = type_error_value(runtime, "fetch URL must include a host")?;
        return rejected_promise_value(runtime, reason);
    };
    if let Err(error) = current_capabilities(runtime).require_net(host) {
        let reason = type_error_value(runtime, &error.to_string())?;
        return rejected_promise_value(runtime, reason);
    }

    let promise = runtime.alloc_vm_promise();
    let reservation = runtime.host_callback_sender().reserve();
    let request = FetchRequestState {
        method: request.method,
        url: request.url,
        headers: header_entries(runtime, &request.headers)?,
        body: clone_body_bytes(&request.body)?,
    };

    std::thread::spawn(move || {
        let result = perform_fetch(request);
        let _ = reservation.enqueue(move |runtime| match result {
            Ok(response) => {
                if let Ok(headers) = alloc_headers_instance(runtime, response.headers.clone()) {
                    let response_value = alloc_response_instance(
                        runtime,
                        ResponsePayload {
                            status: response.status,
                            status_text: response.status_text,
                            headers,
                            body: Arc::new(Mutex::new(BodyState {
                                bytes: response.body,
                                used: false,
                            })),
                            url: response.url,
                        },
                    );
                    match response_value {
                        Ok(response_value) => {
                            let _ = runtime.fulfill_vm_promise(promise, response_value);
                        }
                        Err(error) => reject_with_error(runtime, promise, error),
                    }
                } else {
                    reject_with_type_error(runtime, promise, "fetch failed to allocate headers");
                }
            }
            Err(message) => reject_with_type_error(runtime, promise, &message),
        });
    });

    Ok(promise.promise_value())
}

fn request_or_response_body(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Ok(RegisterValue::null())
}

fn request_or_response_body_used(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let state = require_body_owner(runtime, this)?;
    Ok(RegisterValue::from_bool(body_is_used(state.body())?))
}

fn request_or_response_text(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let consumed = match consume_body_bytes(runtime, this) {
        Ok(bytes) => bytes,
        Err(BodyReadError::Rejected(reason)) => return rejected_promise_value(runtime, reason),
        Err(BodyReadError::Thrown(error)) => return Err(error),
    };
    let text = string_value(runtime, String::from_utf8_lossy(&consumed).into_owned());
    fulfilled_promise_value(runtime, text)
}

fn request_or_response_array_buffer(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let consumed = match consume_body_bytes(runtime, this) {
        Ok(bytes) => bytes,
        Err(BodyReadError::Rejected(reason)) => return rejected_promise_value(runtime, reason),
        Err(BodyReadError::Thrown(error)) => return Err(error),
    };
    let buffer = alloc_array_buffer(runtime, consumed);
    fulfilled_promise_value(runtime, RegisterValue::from_object_handle(buffer.0))
}

fn request_or_response_blob(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let owner = require_body_owner(runtime, this)?;
    let content_type = header_entries(runtime, &owner.headers())?
        .into_iter()
        .find_map(|(name, value)| (name == "content-type").then_some(value));
    let consumed = match consume_body_bytes(runtime, this) {
        Ok(bytes) => bytes,
        Err(BodyReadError::Rejected(reason)) => return rejected_promise_value(runtime, reason),
        Err(BodyReadError::Thrown(error)) => return Err(error),
    };
    let blob = alloc_blob_instance(
        runtime,
        "Blob",
        BlobPayload {
            bytes: consumed,
            media_type: content_type.unwrap_or_default(),
            file_name: None,
            last_modified: 0.0,
        },
    )?;
    fulfilled_promise_value(runtime, blob)
}

fn request_or_response_json(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let consumed = match consume_body_bytes(runtime, this) {
        Ok(bytes) => bytes,
        Err(BodyReadError::Rejected(reason)) => return rejected_promise_value(runtime, reason),
        Err(BodyReadError::Thrown(error)) => return Err(error),
    };
    let text = string_value(runtime, String::from_utf8_lossy(&consumed).into_owned());
    match call_json_method(runtime, "parse", &[text]) {
        Ok(value) => fulfilled_promise_value(runtime, value),
        Err(VmNativeCallError::Thrown(reason)) => rejected_promise_value(runtime, reason),
        Err(VmNativeCallError::Internal(message)) => Err(VmNativeCallError::Internal(message)),
    }
}

fn build_request_payload(
    runtime: &mut RuntimeState,
    input: RegisterValue,
    init: RegisterValue,
) -> Result<RequestPayload, VmNativeCallError> {
    let mut method = "GET".to_string();
    let mut url = runtime.js_to_string_infallible(input).into_string();
    let mut headers = alloc_headers_instance(runtime, Vec::new())?;
    let mut body = ParsedBodyInit {
        bytes: Vec::new(),
        content_type: None,
        present: false,
    };

    if let Ok(source) = runtime
        .native_payload_from_value::<RequestPayload>(&input)
        .cloned()
    {
        method = source.method;
        url = source.url;
        headers = clone_headers(runtime, source.headers)?;
        let source_body_bytes = clone_body_bytes(&source.body)?;
        body = ParsedBodyInit {
            present: !source_body_bytes.is_empty(),
            bytes: source_body_bytes,
            content_type: None,
        };
        if body_is_used(&source.body)? && !has_own_property(init, runtime, "body")? {
            return Err(type_error(
                runtime,
                "Cannot construct a Request from one whose body is already used",
            ));
        }
    }

    if init != RegisterValue::undefined() && init != RegisterValue::null() {
        let init_object = require_object(runtime, init, "Request init must be an object")?;

        let method_value = own_property_value(runtime, init_object, "method")?;
        if method_value != RegisterValue::undefined() {
            method = normalize_method(runtime, method_value)?;
        }

        let headers_value = own_property_value(runtime, init_object, "headers")?;
        if headers_value != RegisterValue::undefined() {
            let entries = parse_headers_init(runtime, headers_value)?;
            headers = alloc_headers_instance(runtime, entries)?;
        }

        let body_value = own_property_value(runtime, init_object, "body")?;
        if body_value != RegisterValue::undefined() {
            body = parse_body_init(runtime, body_value)?;
        }
    }

    if matches!(method.as_str(), "GET" | "HEAD") && body.present {
        return Err(type_error(
            runtime,
            "Request with GET/HEAD method cannot have a body",
        ));
    }

    headers = ensure_content_type_header(runtime, headers, body.content_type.as_deref())?;

    Ok(RequestPayload {
        method,
        url,
        headers,
        body: Arc::new(Mutex::new(BodyState {
            bytes: body.bytes,
            used: false,
        })),
    })
}

fn build_response_payload(
    runtime: &mut RuntimeState,
    body: RegisterValue,
    init: RegisterValue,
) -> Result<ResponsePayload, VmNativeCallError> {
    let body = parse_body_init(runtime, body)?;
    let mut status = 200_u16;
    let mut status_text = String::new();
    let mut headers = alloc_headers_instance(runtime, Vec::new())?;

    if init != RegisterValue::undefined() && init != RegisterValue::null() {
        let init_object = require_object(runtime, init, "Response init must be an object")?;

        let status_value = own_property_value(runtime, init_object, "status")?;
        if status_value != RegisterValue::undefined() {
            let number = runtime.js_to_number(status_value).map_err(|_| {
                type_error(runtime, "Response status must be coercible to a number")
            })?;
            let number = number.trunc() as i32;
            if !(200..=599).contains(&number) {
                return Err(type_error(
                    runtime,
                    "Response status must be in the range 200 to 599",
                ));
            }
            status = number as u16;
        }

        let status_text_value = own_property_value(runtime, init_object, "statusText")?;
        if status_text_value != RegisterValue::undefined() {
            status_text = runtime
                .js_to_string_infallible(status_text_value)
                .into_string();
            validate_status_text(runtime, &status_text)?;
        }

        let headers_value = own_property_value(runtime, init_object, "headers")?;
        if headers_value != RegisterValue::undefined() {
            let entries = parse_headers_init(runtime, headers_value)?;
            headers = alloc_headers_instance(runtime, entries)?;
        }
    }

    if matches!(status, 101 | 103 | 204 | 205 | 304) && body.present {
        return Err(type_error(
            runtime,
            "Response with null body status cannot have a body",
        ));
    }

    headers = ensure_content_type_header(runtime, headers, body.content_type.as_deref())?;

    Ok(ResponsePayload {
        status,
        status_text,
        headers,
        body: Arc::new(Mutex::new(BodyState {
            bytes: body.bytes,
            used: false,
        })),
        url: String::new(),
    })
}

fn alloc_response_instance(
    runtime: &mut RuntimeState,
    payload: ResponsePayload,
) -> Result<RegisterValue, VmNativeCallError> {
    let prototype = class_prototype(runtime, "Response")?;
    let instance = runtime.alloc_native_object_with_prototype(Some(prototype), payload);
    Ok(RegisterValue::from_object_handle(instance.0))
}

fn parse_body_init(
    runtime: &mut RuntimeState,
    value: RegisterValue,
) -> Result<ParsedBodyInit, VmNativeCallError> {
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return Ok(ParsedBodyInit {
            bytes: Vec::new(),
            content_type: None,
            present: false,
        });
    }

    if let Ok(blob) = require_blob_payload(runtime, &value, "") {
        return Ok(ParsedBodyInit {
            bytes: blob.bytes,
            content_type: (!blob.media_type.is_empty()).then_some(blob.media_type),
            present: true,
        });
    }

    if runtime
        .native_payload_from_value::<FormDataPayload>(&value)
        .is_ok()
    {
        return Err(type_error(
            runtime,
            "Request/Response FormData bodies are not implemented yet",
        ));
    }

    if let Some(encoded) = serialize_url_search_params_value(runtime, &value)? {
        return Ok(ParsedBodyInit {
            bytes: encoded.into_bytes(),
            content_type: Some("application/x-www-form-urlencoded;charset=UTF-8".into()),
            present: true,
        });
    }

    if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
        match runtime.objects().kind(handle) {
            Ok(
                HeapValueKind::ArrayBuffer | HeapValueKind::TypedArray | HeapValueKind::DataView,
            ) => {
                return Ok(ParsedBodyInit {
                    bytes: bytes_from_buffer_source(runtime, value)?,
                    content_type: None,
                    present: true,
                });
            }
            _ => {}
        }
    }

    let string = runtime.js_to_string_infallible(value).into_string();
    Ok(ParsedBodyInit {
        bytes: string.clone().into_bytes(),
        content_type: Some("text/plain;charset=UTF-8".into()),
        present: true,
    })
}

fn ensure_content_type_header(
    runtime: &mut RuntimeState,
    headers: RegisterValue,
    content_type: Option<&str>,
) -> Result<RegisterValue, VmNativeCallError> {
    let Some(content_type) = content_type else {
        return Ok(headers);
    };
    if content_type.is_empty() {
        return Ok(headers);
    }
    let mut entries = header_entries(runtime, &headers)?;
    if entries.iter().all(|(name, _)| name != "content-type") {
        entries.push(("content-type".into(), content_type.into()));
        return alloc_headers_instance(runtime, entries);
    }
    Ok(headers)
}

fn replace_content_type_header(
    runtime: &mut RuntimeState,
    headers: RegisterValue,
    content_type: &str,
) -> Result<RegisterValue, VmNativeCallError> {
    let mut entries = header_entries(runtime, &headers)?;
    entries.retain(|(name, _)| name != "content-type");
    entries.push(("content-type".into(), content_type.into()));
    alloc_headers_instance(runtime, entries)
}

fn reject_with_error(
    runtime: &mut RuntimeState,
    promise: otter_vm::VmPromise,
    error: VmNativeCallError,
) {
    match error {
        VmNativeCallError::Thrown(reason) => {
            let _ = runtime.reject_vm_promise(promise, reason);
        }
        VmNativeCallError::Internal(message) => reject_with_type_error(runtime, promise, &message),
    }
}

fn reject_with_type_error(runtime: &mut RuntimeState, promise: otter_vm::VmPromise, message: &str) {
    if let Ok(reason) = type_error_value(runtime, message) {
        let _ = runtime.reject_vm_promise(promise, reason);
    }
}

fn perform_fetch(request: FetchRequestState) -> Result<FetchResponseState, String> {
    let method = Method::from_bytes(request.method.as_bytes())
        .map_err(|error| format!("fetch method is invalid: {error}"))?;
    let client = reqwest::blocking::Client::builder()
        .build()
        .map_err(|error| format!("fetch client initialization failed: {error}"))?;
    let mut builder = client.request(method, &request.url);
    for (name, value) in &request.headers {
        builder = builder.header(name, value);
    }
    if !request.body.is_empty() {
        builder = builder.body(request.body);
    }
    let response = builder
        .send()
        .map_err(|error| format!("fetch failed: {error}"))?;
    let url = response.url().as_str().to_string();
    let status = response.status();
    let status_text = status.canonical_reason().unwrap_or("").to_string();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect::<Vec<_>>();
    let body = response
        .bytes()
        .map_err(|error| format!("fetch failed to read response body: {error}"))?
        .to_vec();

    Ok(FetchResponseState {
        url,
        status: status.as_u16(),
        status_text,
        headers,
        body,
    })
}

fn clone_headers(
    runtime: &mut RuntimeState,
    headers: RegisterValue,
) -> Result<RegisterValue, VmNativeCallError> {
    let entries = header_entries(runtime, &headers)?;
    alloc_headers_instance(runtime, entries)
}

fn normalize_method(
    runtime: &mut RuntimeState,
    value: RegisterValue,
) -> Result<String, VmNativeCallError> {
    let method = runtime.js_to_string_infallible(value).into_string();
    if method.is_empty() || !method.bytes().all(is_token_byte) {
        return Err(type_error(
            runtime,
            "Request method is not a valid HTTP token",
        ));
    }
    let upper = method.to_ascii_uppercase();
    if matches!(upper.as_str(), "CONNECT" | "TRACE" | "TRACK") {
        return Err(type_error(runtime, "Request method is forbidden"));
    }
    Ok(match upper.as_str() {
        "DELETE" | "GET" | "HEAD" | "OPTIONS" | "POST" | "PUT" => upper,
        _ => method,
    })
}

fn validate_status_text(
    runtime: &mut RuntimeState,
    status_text: &str,
) -> Result<(), VmNativeCallError> {
    if status_text
        .bytes()
        .any(|byte| matches!(byte, b'\r' | b'\n' | 0) || !(0x20..=0x7E).contains(&byte))
    {
        return Err(type_error(
            runtime,
            "Response statusText contains invalid characters",
        ));
    }
    Ok(())
}

fn require_request_payload(
    runtime: &mut RuntimeState,
    value: &RegisterValue,
) -> Result<RequestPayload, VmNativeCallError> {
    runtime
        .native_payload_from_value::<RequestPayload>(value)
        .cloned()
        .map_err(|_| type_error(runtime, "Request method called on incompatible receiver"))
}

fn require_response_payload(
    runtime: &mut RuntimeState,
    value: &RegisterValue,
) -> Result<ResponsePayload, VmNativeCallError> {
    runtime
        .native_payload_from_value::<ResponsePayload>(value)
        .cloned()
        .map_err(|_| type_error(runtime, "Response method called on incompatible receiver"))
}

enum BodyOwner {
    Request(RequestPayload),
    Response(ResponsePayload),
}

impl BodyOwner {
    fn body(&self) -> &Arc<Mutex<BodyState>> {
        match self {
            BodyOwner::Request(payload) => &payload.body,
            BodyOwner::Response(payload) => &payload.body,
        }
    }

    fn headers(&self) -> RegisterValue {
        match self {
            BodyOwner::Request(payload) => payload.headers,
            BodyOwner::Response(payload) => payload.headers,
        }
    }
}

fn require_body_owner(
    runtime: &mut RuntimeState,
    value: &RegisterValue,
) -> Result<BodyOwner, VmNativeCallError> {
    if let Ok(payload) = runtime
        .native_payload_from_value::<RequestPayload>(value)
        .cloned()
    {
        return Ok(BodyOwner::Request(payload));
    }
    if let Ok(payload) = runtime
        .native_payload_from_value::<ResponsePayload>(value)
        .cloned()
    {
        return Ok(BodyOwner::Response(payload));
    }
    Err(type_error(
        runtime,
        "Body method called on incompatible receiver",
    ))
}

enum BodyReadError {
    Rejected(RegisterValue),
    Thrown(VmNativeCallError),
}

fn consume_body_bytes(
    runtime: &mut RuntimeState,
    value: &RegisterValue,
) -> Result<Vec<u8>, BodyReadError> {
    let owner = require_body_owner(runtime, value).map_err(BodyReadError::Thrown)?;
    let mut state = owner.body().lock().map_err(|_| {
        BodyReadError::Thrown(VmNativeCallError::Internal(
            "Body state mutex poisoned".into(),
        ))
    })?;
    if state.used {
        let reason = type_error_value(runtime, "Body has already been consumed")
            .map_err(BodyReadError::Thrown)?;
        return Err(BodyReadError::Rejected(reason));
    }
    state.used = true;
    Ok(state.bytes.clone())
}

fn body_is_used(body: &Arc<Mutex<BodyState>>) -> Result<bool, VmNativeCallError> {
    let state = body
        .lock()
        .map_err(|_| VmNativeCallError::Internal("Body state mutex poisoned".into()))?;
    Ok(state.used)
}

fn clone_body_bytes(body: &Arc<Mutex<BodyState>>) -> Result<Vec<u8>, VmNativeCallError> {
    let state = body
        .lock()
        .map_err(|_| VmNativeCallError::Internal("Body state mutex poisoned".into()))?;
    Ok(state.bytes.clone())
}

fn clone_body_state(
    body: &Arc<Mutex<BodyState>>,
) -> Result<Arc<Mutex<BodyState>>, VmNativeCallError> {
    Ok(Arc::new(Mutex::new(BodyState {
        bytes: clone_body_bytes(body)?,
        used: false,
    })))
}

fn fulfilled_promise_value(
    runtime: &mut RuntimeState,
    value: RegisterValue,
) -> Result<RegisterValue, VmNativeCallError> {
    let promise = runtime.alloc_fulfilled_vm_promise(value)?;
    Ok(promise.promise_value())
}

fn rejected_promise_value(
    runtime: &mut RuntimeState,
    reason: RegisterValue,
) -> Result<RegisterValue, VmNativeCallError> {
    let promise = runtime.alloc_rejected_vm_promise(reason)?;
    Ok(promise.promise_value())
}

fn type_error_value(
    runtime: &mut RuntimeState,
    message: &str,
) -> Result<RegisterValue, VmNativeCallError> {
    let error = runtime
        .alloc_type_error(message)
        .map_err(|_| VmNativeCallError::Internal(message.into()))?;
    Ok(RegisterValue::from_object_handle(error.0))
}

fn call_json_method(
    runtime: &mut RuntimeState,
    method: &str,
    args: &[RegisterValue],
) -> Result<RegisterValue, VmNativeCallError> {
    let global = runtime.intrinsics().global_object();
    let json_property = runtime.intern_property_name("JSON");
    let json_value = runtime
        .own_property_value(global, json_property)
        .map_err(|_| type_error(runtime, "JSON intrinsic is unavailable"))?;
    let json = json_value
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| type_error(runtime, "JSON intrinsic is invalid"))?;
    let method_property = runtime.intern_property_name(method);
    let callable = runtime
        .own_property_value(json, method_property)
        .map_err(|_| type_error(runtime, "JSON method is unavailable"))?
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| type_error(runtime, "JSON method is invalid"))?;
    runtime.call_host_function(
        Some(callable),
        RegisterValue::from_object_handle(json.0),
        args,
    )
}

fn own_property_value(
    runtime: &mut RuntimeState,
    object: ObjectHandle,
    name: &str,
) -> Result<RegisterValue, VmNativeCallError> {
    let property = runtime.intern_property_name(name);
    Ok(runtime
        .own_property_value(object, property)
        .unwrap_or_else(|_| RegisterValue::undefined()))
}

fn has_own_property(
    value: RegisterValue,
    runtime: &mut RuntimeState,
    name: &str,
) -> Result<bool, VmNativeCallError> {
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return Ok(false);
    }
    let object = require_object(runtime, value, "init must be an object")?;
    let property = runtime.intern_property_name(name);
    Ok(runtime
        .objects()
        .has_own_property(object, property)
        .unwrap_or(false))
}

fn require_object(
    runtime: &mut RuntimeState,
    value: RegisterValue,
    message: &str,
) -> Result<ObjectHandle, VmNativeCallError> {
    value
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| type_error(runtime, message))
}

fn is_token_byte(byte: u8) -> bool {
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
