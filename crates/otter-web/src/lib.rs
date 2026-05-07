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
    GlobalClass,
    module_api::{JsString, NativeCtx, NativeError, Value},
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
        spec: GlobalClass::from_raw(&url::URL_CLASS_SPEC),
    },
    WebApiClass {
        name: "Headers",
        spec: GlobalClass::from_raw(&headers::HEADERS_CLASS_SPEC),
    },
    WebApiClass {
        name: "Blob",
        spec: GlobalClass::from_raw(&blob::BLOB_CLASS_SPEC),
    },
    WebApiClass {
        name: "Request",
        spec: GlobalClass::from_raw(&request_response::REQUEST_CLASS_SPEC),
    },
    WebApiClass {
        name: "Response",
        spec: GlobalClass::from_raw(&request_response::RESPONSE_CLASS_SPEC),
    },
];

/// Return active Web API specs.
#[must_use]
pub const fn web_api_classes() -> &'static [WebApiClass] {
    WEB_API_CLASSES
}

pub(crate) fn type_error(name: &'static str, reason: impl Into<String>) -> NativeError {
    NativeError::TypeError {
        name,
        reason: reason.into(),
    }
}

pub(crate) fn arg_string(args: &[Value], index: usize) -> String {
    match args.get(index) {
        Some(Value::String(value)) => value.to_lossy_string(),
        Some(Value::Undefined) | None => String::new(),
        Some(value) => value.display_string(),
    }
}

pub(crate) fn string_value(ctx: &mut NativeCtx<'_>, value: &str) -> Result<Value, NativeError> {
    let heap = ctx.interp_mut().string_heap_clone();
    Ok(Value::String(
        JsString::from_str(value, &heap).map_err(|err| type_error("string", err.to_string()))?,
    ))
}
