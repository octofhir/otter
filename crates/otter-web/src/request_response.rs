//! Fetch Request and Response host-side records.

use otter_runtime::module_api::{
    Attr, ClassSpec, ConstructorSpec, JsObject, MethodSpec, NativeCall, NativeCtx, NativeError,
    NumberValue, ObjectBuilder, Value, object,
};

use crate::blob::Blob;
use crate::headers::Headers;
use crate::url::{UrlError, WebUrl};

/// Request/Response construction errors.
#[derive(Debug, thiserror::Error)]
pub enum FetchRecordError {
    /// Invalid URL.
    #[error("{0}")]
    Url(#[from] UrlError),
    /// Invalid status.
    #[error("invalid response status {0}")]
    InvalidStatus(u16),
}

/// Result alias for fetch-shaped records.
pub type FetchRecordResult<T> = Result<T, FetchRecordError>;

/// Owned Request record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    method: String,
    url: WebUrl,
    headers: Headers,
    body: Option<Blob>,
}

impl Request {
    /// Create a request.
    pub fn new(input: &str, method: Option<&str>, body: Option<Blob>) -> FetchRecordResult<Self> {
        Ok(Self {
            method: method
                .filter(|value| !value.is_empty())
                .unwrap_or("GET")
                .to_ascii_uppercase(),
            url: WebUrl::parse(input, None)?,
            headers: Headers::new(),
            body,
        })
    }

    /// HTTP method.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Serialized URL.
    #[must_use]
    pub fn url(&self) -> String {
        self.url.href()
    }

    /// Headers.
    #[must_use]
    pub fn headers(&self) -> &Headers {
        &self.headers
    }

    /// Body.
    #[must_use]
    pub fn body(&self) -> Option<&Blob> {
        self.body.as_ref()
    }
}

/// Owned Response record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    status: u16,
    status_text: String,
    headers: Headers,
    body: Option<Blob>,
}

impl Response {
    /// Create a response.
    pub fn new(
        status: u16,
        status_text: impl Into<String>,
        body: Option<Blob>,
    ) -> FetchRecordResult<Self> {
        if !(200..=599).contains(&status) {
            return Err(FetchRecordError::InvalidStatus(status));
        }
        Ok(Self {
            status,
            status_text: status_text.into(),
            headers: Headers::new(),
            body,
        })
    }

    /// HTTP status.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Status text.
    #[must_use]
    pub fn status_text(&self) -> &str {
        &self.status_text
    }

    /// Headers.
    #[must_use]
    pub fn headers(&self) -> &Headers {
        &self.headers
    }

    /// Body.
    #[must_use]
    pub fn body(&self) -> Option<&Blob> {
        self.body.as_ref()
    }
}

/// Static Request class spec.
pub static REQUEST_CLASS_SPEC: ClassSpec = ClassSpec {
    constructor: ConstructorSpec {
        name: "Request",
        length: 1,
        call: NativeCall::Static(request_constructor_native),
        static_methods: &[],
        prototype_methods: &[method("clone", 0, request_clone_native)],
        attrs: Attr::global_binding(),
    },
    prototype_accessors: &[],
};

/// Static Response class spec.
pub static RESPONSE_CLASS_SPEC: ClassSpec = ClassSpec {
    constructor: ConstructorSpec {
        name: "Response",
        length: 0,
        call: NativeCall::Static(response_constructor_native),
        static_methods: &[method("json", 1, response_json_native)],
        prototype_methods: &[method("clone", 0, response_clone_native)],
        attrs: Attr::global_binding(),
    },
    prototype_accessors: &[],
};

const fn method(
    name: &'static str,
    length: u8,
    call: for<'rt> fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>,
) -> MethodSpec {
    MethodSpec {
        name,
        length,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(call),
    }
}

fn request_constructor_native(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let input = crate::arg_string(args, 0);
    let method = match args.get(1) {
        Some(Value::String(value)) => Some(value.to_lossy_string()),
        Some(Value::Undefined) | None => None,
        Some(value) => Some(value.display_string()),
    };
    let body = match args.get(2) {
        Some(Value::String(value)) => Some(Blob::new(value.to_lossy_string().into_bytes(), "")),
        _ => None,
    };
    let request = Request::new(&input, method.as_deref(), body)
        .map_err(|err| crate::type_error("Request", err.to_string()))?;
    request_object(ctx, request)
}

fn request_receiver(ctx: &NativeCtx<'_>, name: &'static str) -> Result<JsObject, NativeError> {
    match ctx.this_value().clone() {
        Value::Object(object) => Ok(object),
        _ => Err(crate::type_error(name, "invalid Request receiver")),
    }
}

fn response_receiver(ctx: &NativeCtx<'_>, name: &'static str) -> Result<JsObject, NativeError> {
    match ctx.this_value().clone() {
        Value::Object(object) => Ok(object),
        _ => Err(crate::type_error(name, "invalid Response receiver")),
    }
}

fn host_error(name: &'static str, err: object::HostObjectError) -> NativeError {
    crate::type_error(name, err.to_string())
}

fn request_snapshot(ctx: &NativeCtx<'_>, name: &'static str) -> Result<Request, NativeError> {
    let object = request_receiver(ctx, name)?;
    object::with_host_data::<Request, _>(object, ctx.heap(), Clone::clone)
        .map_err(|err| host_error(name, err))
}

fn response_snapshot(ctx: &NativeCtx<'_>, name: &'static str) -> Result<Response, NativeError> {
    let object = response_receiver(ctx, name)?;
    object::with_host_data::<Response, _>(object, ctx.heap(), Clone::clone)
        .map_err(|err| host_error(name, err))
}

fn request_clone_native(ctx: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
    request_object(ctx, request_snapshot(ctx, "Request.prototype.clone")?)
}

fn response_constructor_native(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let body = match args.first() {
        Some(Value::String(value)) => Some(Blob::new(value.to_lossy_string().into_bytes(), "")),
        _ => None,
    };
    let status = match args.get(1) {
        Some(Value::Number(value)) => value.as_f64() as u16,
        _ => 200,
    };
    let status_text = crate::arg_string(args, 2);
    let response = Response::new(status, status_text, body)
        .map_err(|err| crate::type_error("Response", err.to_string()))?;
    response_object(ctx, response)
}

fn response_clone_native(ctx: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
    response_object(ctx, response_snapshot(ctx, "Response.prototype.clone")?)
}

fn response_json_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let body = crate::arg_string(args, 0);
    let response = Response::new(
        200,
        "OK",
        Some(Blob::new(body.into_bytes(), "application/json")),
    )
    .map_err(|err| crate::type_error("Response.json", err.to_string()))?;
    response_object(ctx, response)
}

fn request_object(ctx: &mut NativeCtx<'_>, state: Request) -> Result<Value, NativeError> {
    let method = crate::string_value(ctx, state.method())?;
    let url = crate::string_value(ctx, &state.url())?;
    let headers = crate::headers::headers_object(ctx, state.headers().clone())?;
    let body = match state.body() {
        Some(body) => crate::blob::blob_object(ctx, body.clone())?,
        None => Value::Null,
    };
    let object = object::alloc_host_object(ctx.interp_mut().gc_heap_mut(), state)?;
    let mut builder = ObjectBuilder::from_object(ctx.interp_mut().gc_heap_mut(), object);
    builder
        .property("method", method, Attr::read_only())
        .and_then(|builder| builder.property("url", url, Attr::read_only()))
        .and_then(|builder| builder.property("headers", headers, Attr::read_only()))
        .and_then(|builder| builder.property("body", body, Attr::read_only()))
        .and_then(|builder| {
            builder.method(
                "clone",
                0,
                NativeCall::Static(request_clone_native),
                Attr::builtin_function(),
            )
        })
        .map_err(|err| crate::type_error("Request", err.to_string()))?;
    Ok(Value::Object(object))
}

fn response_object(ctx: &mut NativeCtx<'_>, state: Response) -> Result<Value, NativeError> {
    let status = Value::Number(NumberValue::from_f64(state.status() as f64));
    let status_text = crate::string_value(ctx, state.status_text())?;
    let headers = crate::headers::headers_object(ctx, state.headers().clone())?;
    let body = match state.body() {
        Some(body) => crate::blob::blob_object(ctx, body.clone())?,
        None => Value::Null,
    };
    let object = object::alloc_host_object(ctx.interp_mut().gc_heap_mut(), state)?;
    let mut builder = ObjectBuilder::from_object(ctx.interp_mut().gc_heap_mut(), object);
    builder
        .property("status", status, Attr::read_only())
        .and_then(|builder| builder.property("statusText", status_text, Attr::read_only()))
        .and_then(|builder| builder.property("headers", headers, Attr::read_only()))
        .and_then(|builder| builder.property("body", body, Attr::read_only()))
        .and_then(|builder| {
            builder.method(
                "clone",
                0,
                NativeCall::Static(response_clone_native),
                Attr::builtin_function(),
            )
        })
        .map_err(|err| crate::type_error("Response", err.to_string()))?;
    Ok(Value::Object(object))
}
