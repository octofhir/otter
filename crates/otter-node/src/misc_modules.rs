//! Small `node:` module shims grouped together: `perf_hooks`, `v8`, `module`,
//! and aliases such as `node:process` that expose an existing runtime global.
//!
//! # Contents
//! - CommonJS shims for small Node namespaces.
//! - Global aliases whose identity must match the corresponding global.
//!
//! # Invariants
//! - Aliases return the already-rooted global value; they do not clone state.
//! - Capability-bearing behavior remains in the Rust implementation that owns
//!   the underlying global/module.

use otter_runtime::{
    CapabilitySet, RuntimeLocal as Local, RuntimeNativeError as NativeError,
    RuntimeNativeScope as NativeScope, RuntimeTaskSpawner,
};

const PERF_HOOKS_SHIM: &str = include_str!("perf_hooks.js");
const V8_SHIM: &str = include_str!("v8.js");
const MODULE_SHIM: &str = include_str!("module_builtin.js");
const CLUSTER_SHIM: &str = include_str!("cluster.js");
const INTERNAL_UTIL_SHIM: &str = include_str!("internal_util.js");
const VM_SHIM: &str = include_str!("vm.js");
const INTERNAL_URL_SHIM: &str = r#"'use strict';
module.exports = {
  URL: globalThis.URL,
  isURL(value) { return typeof URL !== 'undefined' && value instanceof URL; },
  pathToFileURL(path) { return new URL(`file://${String(path)}`); },
};
"#;

/// `internal/event_target` (exposed under `--expose-internals`) — re-exports the
/// `Event` / `EventTarget` / `CustomEvent` globals (installed by the Web API
/// bootstrap) plus the internal symbols Node's tests reach for.
const INTERNAL_EVENT_TARGET_SHIM: &str = "\
'use strict';
const g = globalThis;
module.exports = {
  Event: g.Event,
  EventTarget: g.EventTarget,
  CustomEvent: g.CustomEvent,
  NodeEventTarget: g.EventTarget,
  kWeakHandler: Symbol('kWeakHandler'),
  kEvents: Symbol('events'),
  kMaxEventTargetListeners: Symbol('kMaxEventTargetListeners'),
  kMaxEventTargetListenersWarned: Symbol('kMaxEventTargetListenersWarned'),
  initEventTarget(self) { return self; },
  defineEventHandler() {},
  isEventTarget(v) { return v instanceof g.EventTarget; },
};
";

/// `internal/event_target` CommonJS export.
pub fn internal_event_target_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(
        scope,
        "internal/event_target",
        INTERNAL_EVENT_TARGET_SHIM,
        module,
        require,
    )
}

/// `node:cluster` — single-process stub (always primary, no workers).
pub fn cluster_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:cluster", CLUSTER_SHIM, module, require)
}

/// `node:http` — HTTP/1.1 framing over `node:net`.
///
/// # Errors
/// Returns a native error when the shim fails to allocate or evaluate.
pub fn http_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(
        scope,
        "node:http",
        include_str!("http.js"),
        module,
        require,
    )
}

/// `node:https` — the http surface with TLS defaults (port 443).
///
/// # Errors
/// Returns a native error when the shim fails to allocate or evaluate.
pub fn https_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(
        scope,
        "node:https",
        include_str!("https.js"),
        module,
        require,
    )
}

/// The `_http_*` internals Node exposes as real modules: each is a narrow
/// re-export over the hosted `http` module, exactly as Bun ships them.
macro_rules! http_internal_shim {
    ($fn_name:ident, $specifier:literal, $source:literal) => {
        /// One `_http_*` internal module: a re-export over `http`.
        ///
        /// # Errors
        /// Returns a native error when the shim fails to allocate or evaluate.
        pub fn $fn_name<'scope>(
            scope: &mut NativeScope<'scope, '_>,
            _caps: &CapabilitySet,
            _runtime_task_spawner: Option<RuntimeTaskSpawner>,
            module: Local<'scope>,
            require: Local<'scope>,
        ) -> Result<Local<'scope>, NativeError> {
            otter_runtime::run_builtin_cjs_shim(scope, $specifier, $source, module, require)
        }
    };
}

http_internal_shim!(
    http_common_cjs_value,
    "node:_http_common",
    r#"'use strict';
const http = require('http');
module.exports = {
  _checkIsHttpToken: http._checkIsHttpToken,
  _checkInvalidHeaderChar: http._checkInvalidHeaderChar,
  methods: http.METHODS,
  chunkExpression: /(?:^|\W)chunked(?:$|\W)/i,
};
"#
);

http_internal_shim!(
    http_agent_cjs_value,
    "node:_http_agent",
    r#"'use strict';
const http = require('http');
module.exports = { Agent: http.Agent, globalAgent: http.globalAgent };
"#
);

http_internal_shim!(
    http_client_cjs_value,
    "node:_http_client",
    r#"'use strict';
const http = require('http');
module.exports = { ClientRequest: http.ClientRequest };
"#
);

http_internal_shim!(
    http_incoming_cjs_value,
    "node:_http_incoming",
    r#"'use strict';
const http = require('http');
module.exports = { IncomingMessage: http.IncomingMessage };
"#
);

http_internal_shim!(
    http_outgoing_cjs_value,
    "node:_http_outgoing",
    r#"'use strict';
const http = require('http');
module.exports = { OutgoingMessage: http.OutgoingMessage };
"#
);

http_internal_shim!(
    http_server_cjs_value,
    "node:_http_server",
    r#"'use strict';
const http = require('http');
module.exports = {
  Server: http.Server,
  ServerResponse: http.ServerResponse,
  STATUS_CODES: http.STATUS_CODES,
};
"#
);

http_internal_shim!(
    internal_http_cjs_value,
    "node:internal/http",
    r#"'use strict';
const http = require('http');
module.exports = { kOutHeaders: http._kOutHeaders };
"#
);

/// `node:perf_hooks` — performance timeline subset.
pub fn perf_hooks_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:perf_hooks", PERF_HOOKS_SHIM, module, require)
}

/// `node:v8` — heap statistics + serialize/deserialize subset.
pub fn v8_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:v8", V8_SHIM, module, require)
}

/// `node:domain` — group asynchronous work so its errors reach one handler.
///
/// # Errors
/// Returns a native error when the shim fails to evaluate.
pub fn domain_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(
        scope,
        "node:domain",
        include_str!("domain.js"),
        module,
        require,
    )
}

/// `node:console` — the global console itself, which is what Node's own
/// `require('console')` answers with.
///
/// # Errors
/// Returns a native error when the shim fails to evaluate.
pub fn console_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(
        scope,
        "node:console",
        "'use strict';\nmodule.exports = globalThis.console;\n",
        module,
        require,
    )
}

/// `node:module` — builtin-module metadata + a minimal Module class.
pub fn module_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:module", MODULE_SHIM, module, require)
}

/// `node:process` / `process` — the exact `globalThis.process` object.
pub fn process_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    _module: Local<'scope>,
    _require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    scope
        .global("process")
        .ok_or_else(|| crate::type_error("process", "process global is not installed"))
}

/// `internal/util` — the `--expose-internals` subset (sleep,
/// warnings, deprecation wrappers, and kEmptyObject).
pub fn internal_util_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "internal/util", INTERNAL_UTIL_SHIM, module, require)
}

/// `node:vm` — best-effort in-realm sandbox (with-scoped Proxy).
pub fn vm_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:vm", VM_SHIM, module, require)
}

/// `internal/url` brand predicate used by Node's own URL tests.
pub fn internal_url_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "internal/url", INTERNAL_URL_SHIM, module, require)
}
