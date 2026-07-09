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
pub mod globals;
pub mod url;

use otter_runtime::{
    GlobalClass, OtterBuilder, RuntimeBuilder, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeValue as Value, runtime_arg_to_string,
    runtime_string_value, runtime_type_error,
};

/// Active Web API class globals in deterministic bootstrap order.
/// Each entry is the `couch!`-generated `BuiltinIntrinsic` for one
/// Web class. The runtime installs them via the same fn-pointer
/// path as bootstrap registry entries.
pub static WEB_API_CLASSES: &[GlobalClass] = &[
    GlobalClass::from_intrinsic::<url::Intrinsic>(),
    // File's install resolves Blob off the global, so Blob precedes it.
    GlobalClass::from_intrinsic::<blob::BlobIntrinsic>(),
    GlobalClass::from_intrinsic::<blob::FileIntrinsic>(),
];

/// Return active Web API specs.
#[must_use]
pub const fn web_api_classes() -> &'static [GlobalClass] {
    WEB_API_CLASSES
}

/// Register active Web API globals on a runtime builder.
#[must_use]
pub fn with_web_apis(builder: RuntimeBuilder) -> RuntimeBuilder {
    builder
        .global_classes(WEB_API_CLASSES.iter().copied())
        .global_installer(globals::web_globals_installer())
}

/// Register active Web API globals on a Layer-A builder.
#[must_use]
pub fn with_web_apis_for_otter(builder: OtterBuilder) -> OtterBuilder {
    builder
        .global_classes(WEB_API_CLASSES.iter().copied())
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

pub(crate) fn type_error(name: &'static str, reason: impl Into<String>) -> NativeError {
    runtime_type_error(name, reason)
}

pub(crate) fn arg_string(
    args: &[Value],
    index: usize,
    heap: &otter_runtime::otter_gc::GcHeap,
) -> String {
    runtime_arg_to_string(args, index, heap)
}

pub(crate) fn string_value(ctx: &mut NativeCtx<'_>, value: &str) -> Result<Value, NativeError> {
    runtime_string_value(ctx, value)
}

/// Re-parent a native host instance to its class prototype.
///
/// Native host classes build and return their own instance object from the
/// constructor, which bypasses the default `new.target.prototype` linkage the
/// engine applies to ordinary `new` expressions. Without this the instance has
/// a null prototype, so `instanceof` fails and prototype methods are invisible.
///
/// The class prototype is `globalThis[class_name].prototype`. Subclasses
/// (e.g. `File extends Blob`) re-home their instance to the subclass prototype
/// themselves in JS via `Object.setPrototypeOf`, since a name-based lookup here
/// cannot see `new.target`.
pub(crate) fn link_class_prototype(ctx: &mut NativeCtx<'_>, instance: Value, class_name: &str) {
    let Some(object) = instance.as_object() else {
        return;
    };
    let Some(prototype) = ctx.class_instance_prototype(class_name) else {
        return;
    };
    otter_runtime::object::set_prototype_value(object, ctx.heap_mut(), Some(prototype));
}
