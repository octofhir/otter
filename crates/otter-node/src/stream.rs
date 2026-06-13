//! `node:stream` / `stream` hosted module.
//!
//! A practical subset of Node streams (Readable/Writable/Duplex/Transform/
//! PassThrough + finished/pipeline), implemented as a JS shim on top of the
//! `events` and `buffer` shims (injected as dependencies). It is the keystone
//! dependency of fs/net/http/zlib/readline.

use otter_runtime::CapabilitySet;
use otter_vm::{NativeCtx, Value};

const SHIM: &str = include_str!("stream.js");
const WEB_SHIM: &str = include_str!("stream_web.js");
const CONSUMERS_SHIM: &str = include_str!("stream_consumers.js");
const PROMISES_SHIM: &str = include_str!("stream_promises.js");

/// CommonJS export: the `stream` namespace (the `Stream` base with the stream
/// classes and helpers attached).
pub fn stream_cjs_value(ctx: &mut NativeCtx<'_>, caps: &CapabilitySet) -> Result<Value, String> {
    let events = crate::events::events_cjs_value(ctx, caps)?;
    let buffer = crate::buffer::buffer_cjs_value(ctx, caps)?;
    otter_runtime::run_builtin_cjs_shim(
        ctx,
        "node:stream",
        SHIM,
        &[("events", events), ("buffer", buffer)],
    )
}

/// CommonJS export: the WHATWG `stream/web` namespace.
pub fn stream_web_cjs_value(
    ctx: &mut NativeCtx<'_>,
    _caps: &CapabilitySet,
) -> Result<Value, String> {
    otter_runtime::run_builtin_cjs_shim(ctx, "node:stream/web", WEB_SHIM, &[])
}

/// CommonJS export: `stream/consumers` (collect a stream into a value).
pub fn stream_consumers_cjs_value(
    ctx: &mut NativeCtx<'_>,
    caps: &CapabilitySet,
) -> Result<Value, String> {
    let buffer = crate::buffer::buffer_cjs_value(ctx, caps)?;
    otter_runtime::run_builtin_cjs_shim(
        ctx,
        "node:stream/consumers",
        CONSUMERS_SHIM,
        &[("buffer", buffer)],
    )
}

/// CommonJS export: `stream/promises` (promise-returning finished/pipeline).
pub fn stream_promises_cjs_value(
    ctx: &mut NativeCtx<'_>,
    caps: &CapabilitySet,
) -> Result<Value, String> {
    let stream = stream_cjs_value(ctx, caps)?;
    otter_runtime::run_builtin_cjs_shim(
        ctx,
        "node:stream/promises",
        PROMISES_SHIM,
        &[("stream", stream)],
    )
}

/// ESM namespace install — CommonJS is the supported surface for now.
pub fn install_stream_module(_ctx: &mut otter_runtime::HostedModuleCtx<'_>) -> Result<(), String> {
    Ok(())
}
