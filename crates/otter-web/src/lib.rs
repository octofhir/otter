//! Active Web API slices.
//!
//! This crate ports URL, Headers, Request/Response, and Blob behavior onto the
//! active engine dependency graph. JavaScript-visible surfaces are static specs
//! installed through the runtime builder/global bootstrap path.
//!
//! # Contents
//! - [`url`] - URL parsing and mutation.
//! - [`headers`] - ordered, normalized header list.
//! - [`blob`] - owned byte blobs.
//! - [`request_response`] - Fetch-shaped request/response records.
//! - [`WEB_API_CLASSES`] - static class specs.
//!
//! # Invariants
//! - Web API state is owned Rust data and contains no VM contexts or handles.
//! - Global/class installation is described by static specs.
//! - Network work is outside this crate. Fetch-like records store owned request
//!   data only; async network integrations must copy that owned data into
//!   futures and resolve on the isolate.
//!
//! # See also
//! - [Web API contribution workflow](../../../docs/book/src/web/contributing.md)

pub mod blob;
pub mod headers;
pub mod request_response;
pub mod url;

use otter_runtime::{
    GlobalClass, OtterBuilder, RuntimeBuilder, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeValue as Value, runtime_arg_to_string,
    runtime_string_value, runtime_type_error,
};

/// Static descriptor for a Web API class/global.
#[derive(Debug, Clone, Copy)]
pub struct WebApiClass {
    /// Global constructor name.
    pub name: &'static str,
    /// Runtime-owned global class surface.
    pub spec: GlobalClass,
}

/// Active Web API class specs in deterministic bootstrap order.
pub static WEB_API_CLASSES: &[WebApiClass] = &[
    WebApiClass {
        name: "URL",
        spec: GlobalClass::from_runtime(&url::URL_CLASS_SPEC),
    },
    WebApiClass {
        name: "Headers",
        spec: GlobalClass::from_runtime(&headers::HEADERS_CLASS_SPEC),
    },
    WebApiClass {
        name: "Blob",
        spec: GlobalClass::from_runtime(&blob::BLOB_CLASS_SPEC),
    },
    WebApiClass {
        name: "Request",
        spec: GlobalClass::from_runtime(&request_response::REQUEST_CLASS_SPEC),
    },
    WebApiClass {
        name: "Response",
        spec: GlobalClass::from_runtime(&request_response::RESPONSE_CLASS_SPEC),
    },
];

/// Return active Web API specs.
#[must_use]
pub const fn web_api_classes() -> &'static [WebApiClass] {
    WEB_API_CLASSES
}

/// Register active Web API globals on a runtime builder.
#[must_use]
pub fn with_web_apis(builder: RuntimeBuilder) -> RuntimeBuilder {
    builder.global_classes(WEB_API_CLASSES.iter().map(|class| class.spec))
}

/// Register active Web API globals on a Layer-A builder.
#[must_use]
pub fn with_web_apis_for_otter(builder: OtterBuilder) -> OtterBuilder {
    builder.global_classes(WEB_API_CLASSES.iter().map(|class| class.spec))
}

/// Ergonomic extension trait for enabling Web APIs on builders.
pub trait WebApiBuilderExt: Sized {
    /// Register URL, Headers, Blob, Request, and Response globals.
    #[must_use]
    fn with_web_apis(self) -> Self;
}

impl WebApiBuilderExt for RuntimeBuilder {
    fn with_web_apis(self) -> Self {
        with_web_apis(self)
    }
}

impl WebApiBuilderExt for OtterBuilder {
    fn with_web_apis(self) -> Self {
        with_web_apis_for_otter(self)
    }
}

pub(crate) fn type_error(name: &'static str, reason: impl Into<String>) -> NativeError {
    runtime_type_error(name, reason)
}

pub(crate) fn arg_string(args: &[Value], index: usize) -> String {
    runtime_arg_to_string(args, index)
}

pub(crate) fn string_value(ctx: &mut NativeCtx<'_>, value: &str) -> Result<Value, NativeError> {
    runtime_string_value(ctx, value)
}
