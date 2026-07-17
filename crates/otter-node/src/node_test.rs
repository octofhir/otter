//! `node:test` / `test` hosted module — a minimal test-runner shim.
//!
//! Node's own `test/parallel` files increasingly drive their assertions through
//! `node:test` (`const { test } = require('node:test')`). The runner itself is
//! naturally expressed in JavaScript, so it ships as an embedded CommonJS shim
//! ([`SHIM`]) executed through [`otter_runtime::run_builtin_cjs_shim`]. The shim
//! depends only on `assert`, resolved through the shared CommonJS loader.
//!
//! # Contents
//! - [`node_test_cjs_value`] - run the shim with the canonical `require`.
//!
//! # Invariants
//! - A failing test sets `process.exitCode = 1`; the conformance harness reads
//!   the process exit code, so all-pass leaves it at 0.

use otter_runtime::{CapabilitySet, RuntimeNativeError as NativeError, RuntimeTaskSpawner};
use otter_vm::{Local, NativeScope};

/// Embedded `node:test` runner implementation.
const SHIM: &str = include_str!("node_test.js");

/// CommonJS export: the `test` function with `it`/`describe`/`suite`/hooks.
pub fn node_test_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:test", SHIM, module, require)
}
