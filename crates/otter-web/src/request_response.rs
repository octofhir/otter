//! Fetch Request and Response host-side records.

use otter_runtime::{
    RuntimeAttr as Attr, RuntimeClassSpec as ClassSpec, RuntimeHostObjectError,
    RuntimeJsObject as JsObject, RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError,
    RuntimeNumberValue as NumberValue, RuntimeObjectBuilder as ObjectBuilder,
    RuntimeValue as Value, runtime_class, runtime_constructor, runtime_method,
    runtime_optional_arg_to_string, runtime_this_object, runtime_with_host_data,
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
static REQUEST_PROTOTYPE_METHODS: &[otter_runtime::RuntimeMethodSpec] =
    &[runtime_method("clone", 0, request_clone_native)];

pub static REQUEST_CLASS_SPEC: ClassSpec = runtime_class(
    runtime_constructor(
        "Request",
        1,
        request_constructor_native,
        &[],
        REQUEST_PROTOTYPE_METHODS,
        Attr::global_binding(),
    ),
    &[],
);

/// Static Response class spec.
static RESPONSE_STATIC_METHODS: &[otter_runtime::RuntimeMethodSpec] =
    &[runtime_method("json", 1, response_json_native)];

static RESPONSE_PROTOTYPE_METHODS: &[otter_runtime::RuntimeMethodSpec] =
    &[runtime_method("clone", 0, response_clone_native)];

pub static RESPONSE_CLASS_SPEC: ClassSpec = runtime_class(
    runtime_constructor(
        "Response",
        0,
        response_constructor_native,
        RESPONSE_STATIC_METHODS,
        RESPONSE_PROTOTYPE_METHODS,
        Attr::global_binding(),
    ),
    &[],
);

fn request_constructor_native(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let input = crate::arg_string(args, 0);
    let method = runtime_optional_arg_to_string(args, 1);
    let body =
        runtime_optional_arg_to_string(args, 2).map(|value| Blob::new(value.into_bytes(), ""));
    let request = Request::new(&input, method.as_deref(), body)
        .map_err(|err| crate::type_error("Request", err.to_string()))?;
    request_object(ctx, request)
}

fn request_receiver(ctx: &NativeCtx<'_>, name: &'static str) -> Result<JsObject, NativeError> {
    runtime_this_object(ctx, name, "Request")
}

fn response_receiver(ctx: &NativeCtx<'_>, name: &'static str) -> Result<JsObject, NativeError> {
    runtime_this_object(ctx, name, "Response")
}

fn host_error(name: &'static str, err: RuntimeHostObjectError) -> NativeError {
    crate::type_error(name, err.to_string())
}

fn request_snapshot(ctx: &NativeCtx<'_>, name: &'static str) -> Result<Request, NativeError> {
    let object = request_receiver(ctx, name)?;
    runtime_with_host_data::<Request, _>(ctx, object, Clone::clone)
        .map_err(|err| host_error(name, err))
}

fn response_snapshot(ctx: &NativeCtx<'_>, name: &'static str) -> Result<Response, NativeError> {
    let object = response_receiver(ctx, name)?;
    runtime_with_host_data::<Response, _>(ctx, object, Clone::clone)
        .map_err(|err| host_error(name, err))
}

fn request_clone_native(ctx: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
    request_object(ctx, request_snapshot(ctx, "Request.prototype.clone")?)
}

fn response_constructor_native(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let body =
        runtime_optional_arg_to_string(args, 0).map(|value| Blob::new(value.into_bytes(), ""));
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
    let mut builder = ObjectBuilder::from_host_data(ctx, state)?;
    builder
        .readonly_property("method", method)
        .and_then(|builder| builder.readonly_property("url", url))
        .and_then(|builder| builder.readonly_property("headers", headers))
        .and_then(|builder| builder.readonly_property("body", body))
        .and_then(|builder| builder.builtin_method("clone", 0, request_clone_native))
        .map_err(|err| crate::type_error("Request", err.to_string()))?;
    Ok(Value::Object(builder.build()))
}

fn response_object(ctx: &mut NativeCtx<'_>, state: Response) -> Result<Value, NativeError> {
    let status = Value::Number(NumberValue::from_f64(state.status() as f64));
    let status_text = crate::string_value(ctx, state.status_text())?;
    let headers = crate::headers::headers_object(ctx, state.headers().clone())?;
    let body = match state.body() {
        Some(body) => crate::blob::blob_object(ctx, body.clone())?,
        None => Value::Null,
    };
    let mut builder = ObjectBuilder::from_host_data(ctx, state)?;
    builder
        .readonly_property("status", status)
        .and_then(|builder| builder.readonly_property("statusText", status_text))
        .and_then(|builder| builder.readonly_property("headers", headers))
        .and_then(|builder| builder.readonly_property("body", body))
        .and_then(|builder| builder.builtin_method("clone", 0, response_clone_native))
        .map_err(|err| crate::type_error("Response", err.to_string()))?;
    Ok(Value::Object(builder.build()))
}
