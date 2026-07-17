//! `node:querystring` / `querystring` hosted module — classic query-string
//! parse/stringify, a faithful JS port of Node v24 `lib/querystring.js` (with
//! the `internal/querystring` encoder helpers inlined). Requires `buffer`.

use otter_runtime::{CapabilitySet, RuntimeNativeError as NativeError, RuntimeTaskSpawner};
use otter_vm::{Local, NativeScope};

const SHIM: &str = include_str!("querystring.js");

/// CommonJS export: the `querystring` namespace.
pub fn querystring_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:querystring", SHIM, module, require)
}
