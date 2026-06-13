//! `node:stream` / `stream` hosted module.
//!
//! A practical subset of Node streams (Readable/Writable/Duplex/Transform/
//! PassThrough + finished/pipeline), implemented as a JS shim on top of the
//! `events` and `buffer` shims (injected as dependencies). It is the keystone
//! dependency of fs/net/http/zlib/readline.

use otter_runtime::CapabilitySet;
use otter_vm::{NativeCtx, Value};

const SHIM: &str = include_str!("stream.js");

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

/// ESM namespace install — CommonJS is the supported surface for now.
pub fn install_stream_module(_ctx: &mut otter_runtime::HostedModuleCtx<'_>) -> Result<(), String> {
    Ok(())
}
