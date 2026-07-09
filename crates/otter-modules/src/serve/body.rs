//! HTTP body transport for `Otter.serve`.
//!
//! `serve.rs` owns HTTP parsing and Fetch object conversion, but body storage
//! needs a narrow boundary so buffered bootstrap behavior can evolve into
//! Web Streams without changing request dispatch or runtime task plumbing.
//!
//! # Contents
//! - [`ServeBody`] - request/response body payload passed across serve tasks.
//!
//! # Invariants
//! - Body data crossing worker/runtime boundaries is owned and `Send`.
//! - Native code does not store VM values inside body transport state.
//! - JS-visible request bodies are exposed as bytes, not lossy UTF-8 strings.
//!
//! # See also
//! - [`crate::serve`]

use otter_runtime::{
    RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError, RuntimeValue as Value,
    runtime_type_error,
};

/// Owned HTTP body payload for server request/response dispatch.
#[derive(Clone, Debug, Default)]
pub(crate) enum ServeBody {
    /// No body bytes are present.
    #[default]
    Empty,
    /// Fully buffered body bytes.
    Buffered(Vec<u8>),
}

impl ServeBody {
    /// Build a body from already-buffered bytes.
    #[must_use]
    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Self {
        if bytes.is_empty() {
            Self::Empty
        } else {
            Self::Buffered(bytes)
        }
    }

    /// Borrow the buffered request/response bytes.
    #[must_use]
    pub(crate) fn as_buffered_bytes(&self) -> &[u8] {
        match self {
            Self::Empty => &[],
            Self::Buffered(bytes) => bytes,
        }
    }

    /// Convert this request body into the JS value written to the native
    /// `Request`'s `kBodyBytes` slot: a `Uint8Array` for a buffered body, or
    /// `null` when the request carried no body.
    pub(crate) fn to_js_body(&self, ctx: &mut NativeCtx<'_>) -> Result<Value, NativeError> {
        match self {
            Self::Empty => Ok(Value::null()),
            Self::Buffered(bytes) => bytes_to_uint8_array(ctx, bytes.clone(), "serve.request"),
        }
    }

    /// Extract a buffered response body from the JS Fetch internals surface.
    pub(crate) fn from_js_value(
        ctx: &mut NativeCtx<'_>,
        value: Value,
    ) -> Result<Self, NativeError> {
        if value.is_null() || value.is_undefined() {
            return Ok(Self::Empty);
        }
        if let Some(string) = value.as_string(ctx.heap()) {
            return Ok(Self::from_bytes(
                string.to_lossy_string(ctx.heap()).into_bytes(),
            ));
        }
        if let Some(typed_array) = value.as_typed_array(ctx.heap()) {
            let offset = typed_array.byte_offset(ctx.heap());
            let len = typed_array.byte_length(ctx.heap());
            let bytes = typed_array
                .buffer(ctx.heap())
                .with_bytes(ctx.heap(), |bytes| {
                    bytes.get(offset..offset + len).map(<[u8]>::to_vec)
                })
                .unwrap_or_default();
            return Ok(Self::from_bytes(bytes));
        }
        if let Some(buffer) = value.as_array_buffer() {
            return Ok(Self::from_bytes(
                buffer.with_bytes(ctx.heap(), |bytes| bytes.to_vec()),
            ));
        }
        Err(runtime_type_error(
            "serve",
            "Response body streams are not supported yet; return a buffered Response body",
        ))
    }
}

fn bytes_to_uint8_array(
    ctx: &mut NativeCtx<'_>,
    bytes: Vec<u8>,
    name: &'static str,
) -> Result<Value, NativeError> {
    let buffer = ctx
        .array_buffer_from_bytes_rooted(bytes, &[], &[])
        .map_err(|err| runtime_type_error(name, err.to_string()))?;
    let ctor = ctx
        .global_value("Uint8Array")
        .ok_or_else(|| runtime_type_error(name, "Uint8Array is unavailable"))?;
    ctx.construct(ctor, &[Value::array_buffer(buffer)])
}
