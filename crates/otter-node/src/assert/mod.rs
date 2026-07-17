//! `node:assert` / `assert` hosted module.
//!
//! Assert is largely a JavaScript surface in Node (a real `AssertionError`
//! class, matcher validation, deep equality, `rejects`/`doesNotReject`,
//! `CallTracker`), so it ships as embedded JS run through
//! [`otter_runtime::run_builtin_cjs_shim`]. The implementation is split to
//! mirror Node's own layout (and to keep each file well under the
//! split-at-1000-lines threshold):
//!
//! # Contents
//! - [`assert_cjs_value`] - the callable `assert` namespace (`assert.js`),
//!   resolving `util` and `internal/assert/calltracker` through CommonJS.
//! - [`calltracker_cjs_value`] - the internal `CallTracker` factory.
//! - [`myers_diff_cjs_value`] - `internal/assert/myers_diff` exposed as its own
//!   requirable module (the conformance suite imports it directly under
//!   `--expose-internals`).
//!
//! # See also
//! - `assert/assert.js`, `assert/calltracker.js`, `assert/myers_diff.js`.

use otter_runtime::{
    CapabilitySet, RuntimeLocal as Local, RuntimeNativeError as NativeError,
    RuntimeNativeScope as NativeScope, RuntimeTaskSpawner,
};

/// Embedded `assert` surface.
const ASSERT_JS: &str = include_str!("assert.js");
/// Embedded `internal/assert/calltracker` factory.
const CALLTRACKER_JS: &str = include_str!("calltracker.js");
/// Embedded `internal/assert/myers_diff`.
const MYERS_DIFF_JS: &str = include_str!("myers_diff.js");

/// CommonJS export: the callable `assert` namespace.
pub fn assert_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "assert", ASSERT_JS, module, require)
}

/// CommonJS export: strict assert namespace (`node:assert/strict`).
pub fn assert_strict_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    _module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    let assert = otter_runtime::require_commonjs_dependency(scope, require, "assert")?;
    scope.get(assert, "strict")
}

/// CommonJS export for `internal/assert/calltracker`.
pub fn calltracker_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(
        scope,
        "internal/assert/calltracker",
        CALLTRACKER_JS,
        module,
        require,
    )
}

/// CommonJS export for `internal/assert/myers_diff` (`--expose-internals`).
pub fn myers_diff_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(
        scope,
        "internal/assert/myers_diff",
        MYERS_DIFF_JS,
        module,
        require,
    )
}
