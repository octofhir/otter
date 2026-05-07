//! Fetch Request and Response host-side records.

use std::sync::{Arc, Mutex};

use otter_vm::{
    Attr, ClassSpec, ConstructorSpec, MethodSpec, NativeCall, NativeCtx, NativeError,
    ObjectBuilder, Value,
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
    request_object(ctx, Arc::new(Mutex::new(request)))
}

fn request_clone_native(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
    Err(crate::type_error(
        "Request.prototype.clone",
        "invalid Request receiver",
    ))
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
    response_object(ctx, Arc::new(Mutex::new(response)))
}

fn response_clone_native(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
    Err(crate::type_error(
        "Response.prototype.clone",
        "invalid Response receiver",
    ))
}

fn response_json_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let body = crate::arg_string(args, 0);
    let response = Response::new(
        200,
        "OK",
        Some(Blob::new(body.into_bytes(), "application/json")),
    )
    .map_err(|err| crate::type_error("Response.json", err.to_string()))?;
    response_object(ctx, Arc::new(Mutex::new(response)))
}

fn request_object(
    ctx: &mut NativeCtx<'_>,
    state: Arc<Mutex<Request>>,
) -> Result<Value, NativeError> {
    let snapshot = state
        .lock()
        .map_err(|_| crate::type_error("Request", "Request state lock poisoned"))?
        .clone();
    let method = crate::string_value(ctx, snapshot.method())?;
    let url = crate::string_value(ctx, &snapshot.url())?;
    let headers =
        crate::headers::headers_object(ctx, Arc::new(Mutex::new(snapshot.headers().clone())))?;
    let body = match snapshot.body() {
        Some(body) => crate::blob::blob_object(ctx, Arc::new(Mutex::new(body.clone())))?,
        None => Value::Null,
    };
    let mut builder = ObjectBuilder::new_in_ctx(ctx)?;
    builder
        .property("method", method, Attr::read_only())
        .and_then(|builder| builder.property("url", url, Attr::read_only()))
        .and_then(|builder| builder.property("headers", headers, Attr::read_only()))
        .and_then(|builder| builder.property("body", body, Attr::read_only()))
        .and_then(|builder| {
            builder.method(
                "clone",
                0,
                NativeCall::Dynamic(Arc::new({
                    let state = state.clone();
                    move |ctx, _args, _captures| {
                        let request = state
                            .lock()
                            .map_err(|_| {
                                crate::type_error(
                                    "Request.prototype.clone",
                                    "Request state lock poisoned",
                                )
                            })?
                            .clone();
                        request_object(ctx, Arc::new(Mutex::new(request)))
                    }
                })),
                Attr::builtin_function(),
            )
        })
        .map_err(|err| crate::type_error("Request", err.to_string()))?;
    Ok(Value::Object(builder.build()))
}

fn response_object(
    ctx: &mut NativeCtx<'_>,
    state: Arc<Mutex<Response>>,
) -> Result<Value, NativeError> {
    let snapshot = state
        .lock()
        .map_err(|_| crate::type_error("Response", "Response state lock poisoned"))?
        .clone();
    let status = Value::Number(otter_vm::NumberValue::from_f64(snapshot.status() as f64));
    let status_text = crate::string_value(ctx, snapshot.status_text())?;
    let headers =
        crate::headers::headers_object(ctx, Arc::new(Mutex::new(snapshot.headers().clone())))?;
    let body = match snapshot.body() {
        Some(body) => crate::blob::blob_object(ctx, Arc::new(Mutex::new(body.clone())))?,
        None => Value::Null,
    };
    let mut builder = ObjectBuilder::new_in_ctx(ctx)?;
    builder
        .property("status", status, Attr::read_only())
        .and_then(|builder| builder.property("statusText", status_text, Attr::read_only()))
        .and_then(|builder| builder.property("headers", headers, Attr::read_only()))
        .and_then(|builder| builder.property("body", body, Attr::read_only()))
        .and_then(|builder| {
            builder.method(
                "clone",
                0,
                NativeCall::Dynamic(Arc::new({
                    let state = state.clone();
                    move |ctx, _args, _captures| {
                        let response = state
                            .lock()
                            .map_err(|_| {
                                crate::type_error(
                                    "Response.prototype.clone",
                                    "Response state lock poisoned",
                                )
                            })?
                            .clone();
                        response_object(ctx, Arc::new(Mutex::new(response)))
                    }
                })),
                Attr::builtin_function(),
            )
        })
        .map_err(|err| crate::type_error("Response", err.to_string()))?;
    Ok(Value::Object(builder.build()))
}
