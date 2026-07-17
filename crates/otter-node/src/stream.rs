//! `node:stream` / `stream` hosted module.
//!
//! A practical subset of Node streams (Readable/Writable/Duplex/Transform/
//! PassThrough + finished/pipeline), implemented as a JS shim on top of the
//! `events` and `buffer` shims (injected as dependencies). It is the keystone
//! dependency of fs/net/http/zlib/readline.

use otter_runtime::{CapabilitySet, RuntimeNativeError as NativeError, RuntimeTaskSpawner};
use otter_vm::{Local, NativeScope};

const SHIM: &str = include_str!("stream.js");
const WEB_SHIM: &str = include_str!("stream_web.js");
const CONSUMERS_SHIM: &str = include_str!("stream_consumers.js");
const PROMISES_SHIM: &str = include_str!("stream_promises.js");

/// CommonJS export: the `stream` namespace (the `Stream` base with the stream
/// classes and helpers attached).
pub fn stream_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    caps: &CapabilitySet,
    runtime_task_spawner: Option<RuntimeTaskSpawner>,
) -> Result<Local<'scope>, NativeError> {
    let events = crate::events::events_cjs_value(scope, caps, runtime_task_spawner.clone())?;
    let buffer = crate::buffer::buffer_cjs_value(scope, caps, runtime_task_spawner)?;
    otter_runtime::run_builtin_cjs_shim(
        scope,
        "node:stream",
        SHIM,
        &[("events", events), ("buffer", buffer)],
    )
}

/// CommonJS export: the WHATWG `stream/web` namespace.
pub fn stream_web_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:stream/web", WEB_SHIM, &[])
}

/// CommonJS export: `stream/consumers` (collect a stream into a value).
pub fn stream_consumers_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    caps: &CapabilitySet,
    runtime_task_spawner: Option<RuntimeTaskSpawner>,
) -> Result<Local<'scope>, NativeError> {
    let buffer = crate::buffer::buffer_cjs_value(scope, caps, runtime_task_spawner)?;
    otter_runtime::run_builtin_cjs_shim(
        scope,
        "node:stream/consumers",
        CONSUMERS_SHIM,
        &[("buffer", buffer)],
    )
}

/// CommonJS export: `stream/promises` (promise-returning finished/pipeline).
pub fn stream_promises_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    caps: &CapabilitySet,
    runtime_task_spawner: Option<RuntimeTaskSpawner>,
) -> Result<Local<'scope>, NativeError> {
    let stream = stream_cjs_value(scope, caps, runtime_task_spawner)?;
    otter_runtime::run_builtin_cjs_shim(
        scope,
        "node:stream/promises",
        PROMISES_SHIM,
        &[("stream", stream)],
    )
}
