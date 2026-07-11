//! Active Web API slices.
//!
//! This crate ports Web platform behavior onto the active engine dependency
//! graph. Native host classes (URL, Blob) are static specs installed through
//! the runtime builder path; the Fetch classes (Headers, Request, Response)
//! and the wider pure-JS surface (Event, TextEncoder/Decoder, streams, …) are
//! JS shims evaluated lazily on first global touch (see [`globals`]).
//!
//! # Contents
//! - [`url`] - URL parsing and mutation.
//! - [`blob`] - owned byte blobs.
//! - [`crypto`] - native CSPRNG + digest backing for the `crypto` global.
//! - [`globals`] - function globals plus the lazy JS shim surface
//!   (`web_bootstrap.js`, `web_streams.js`, `web_fetch.js`).
//! - [`WEB_API_CLASSES`] - static class specs.
//!
//! # Invariants
//! - Web API state is owned Rust data and contains no VM contexts or handles.
//! - Global/class installation is described by static specs.
//! - Network work is outside this crate. The Fetch classes store owned body
//!   data only; server/network integrations exchange plain data with them
//!   through the hidden `__otterFetchInternals` factory in `web_fetch.js`.
//!
//! # See also
//! - [Web API contribution workflow](../../../docs/book/src/web/contributing.md)

extern crate otter_runtime as otter_vm;

pub mod blob;
pub mod crypto;
pub mod fetch_ext;
pub mod globals;
pub mod url;

use otter_runtime::{GlobalClass, OtterBuilder, RuntimeBuilder};

otter_macros::romp! {
    name = "web",
    ident = WEB_EXTENSION,
    // File's install resolves Blob off the global, so Blob precedes it.
    classes = [
        url::WebUrlIntrinsic,
        blob::BlobIntrinsic,
        blob::FileIntrinsic,
        crypto::WebCryptoIntrinsic,
    ],
    js = [
        (include_str!("web_bootstrap.js"), defines = [
            "AbortController", "AbortSignal", "BroadcastChannel", "CloseEvent",
            "CustomEvent", "DOMException", "ErrorEvent", "Event",
            "EventTarget", "FormData", "MessageChannel", "MessageEvent",
            "MessagePort", "Navigator", "performance", "Performance",
            "ProgressEvent", "PromiseRejectionEvent", "reportError",
            "TextDecoder", "TextEncoder", "URLSearchParams",
        ]),
        // Streams precede fetch: the fetch body getter wraps buffered
        // bodies in a ReadableStream.
        (include_str!("web_streams.js"), defines = [
            "ByteLengthQueuingStrategy", "CompressionStream",
            "CountQueuingStrategy", "DecompressionStream", "ReadableStream",
            "ReadableByteStreamController", "ReadableStreamBYOBReader",
            "ReadableStreamBYOBRequest", "ReadableStreamDefaultController",
            "ReadableStreamDefaultReader",
            "TextDecoderStream", "TextEncoderStream", "TransformStream",
            "TransformStreamDefaultController", "WritableStream",
            "WritableStreamDefaultController", "WritableStreamDefaultWriter",
        ]),
        (include_str!("web_fetch.js"), defines = ["fetch", "Headers", "Request", "Response"]),
        // URLPattern needs URL (a native class) + RegExp; both exist eagerly.
        (include_str!("web_urlpattern.js"), defines = ["URLPattern"]),
    ],
}

/// Return active Web API class specs (declaration order).
#[must_use]
pub const fn web_api_classes() -> &'static [GlobalClass] {
    WEB_EXTENSION.classes
}

/// Register active Web API globals on a runtime builder.
#[must_use]
pub fn with_web_apis(builder: RuntimeBuilder) -> RuntimeBuilder {
    builder
        .extension(&WEB_EXTENSION)
        .global_installer(globals::web_globals_installer())
}

/// Register active Web API globals on a Layer-A builder.
#[must_use]
pub fn with_web_apis_for_otter(builder: OtterBuilder) -> OtterBuilder {
    builder
        .extension(&WEB_EXTENSION)
        .global_installer(globals::web_globals_installer())
}

/// Ergonomic extension trait for enabling Web APIs on builders.
pub trait WebApiBuilderExt: Sized {
    /// Register the Web platform globals (URL, Blob, the lazy JS shim
    /// surface including Headers/Request/Response, and function globals).
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
