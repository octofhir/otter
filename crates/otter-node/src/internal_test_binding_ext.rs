//! `internal/test/binding` hosted module for Node compatibility tests.
//!
//! # Contents
//! - [`internal_test_binding_cjs_value`] returns the CommonJS test-binding shim.
//!
//! # Invariants
//! - This module is harness-only. It exposes selected Node test hooks but is
//!   not a public `node:*` API.
//! - Hooks are stored as JS-owned global data; no VM handles cross native
//!   thread or async boundaries.
//!
//! # See also
//! - `internal_test_binding.js`

use otter_runtime::{CapabilitySet, RuntimeTaskSpawner};
use otter_vm::{Local, NativeScope};

const SHIM: &str = include_str!("internal_test_binding.js");

pub fn internal_test_binding_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, otter_vm::NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "internal/test/binding", SHIM, module, require)
}
