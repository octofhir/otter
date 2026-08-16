//! `node:stream` / `stream` hosted module.
//!
//! A practical subset of Node streams (Readable/Writable/Duplex/Transform/
//! PassThrough + finished/pipeline), implemented as a JS shim on top of the
//! `events` and `buffer` shims resolved through the shared CommonJS loader. It
//! is the keystone dependency of fs/net/http/zlib/readline.

use otter_runtime::{CapabilitySet, RuntimeNativeError as NativeError, RuntimeTaskSpawner};
use otter_vm::{Local, NativeScope};

const WEB_SHIM: &str = include_str!("stream_web.js");
const CONSUMERS_SHIM: &str = include_str!("stream_consumers.js");

/// CommonJS export: the WHATWG `stream/web` namespace.
pub fn stream_web_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:stream/web", WEB_SHIM, module, require)
}

/// CommonJS export: `stream/consumers` (collect a stream into a value).
pub fn stream_consumers_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(
        scope,
        "node:stream/consumers",
        CONSUMERS_SHIM,
        module,
        require,
    )
}