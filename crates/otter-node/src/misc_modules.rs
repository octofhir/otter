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

use otter_runtime::CapabilitySet;
use otter_vm::{NativeCtx, Value};

const PERF_HOOKS_SHIM: &str = include_str!("perf_hooks.js");
const V8_SHIM: &str = include_str!("v8.js");
const MODULE_SHIM: &str = include_str!("module_builtin.js");
const CLUSTER_SHIM: &str = include_str!("cluster.js");
const INTERNAL_UTIL_SHIM: &str = include_str!("internal_util.js");
const VM_SHIM: &str = include_str!("vm.js");
const INTERNAL_URL_SHIM: &str = "'use strict'; module.exports = { isURL(value) { return typeof URL !== 'undefined' && value instanceof URL; } };";

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
pub fn internal_event_target_cjs_value(
    ctx: &mut NativeCtx<'_>,
    _caps: &CapabilitySet,
) -> Result<Value, String> {
    otter_runtime::run_builtin_cjs_shim(
        ctx,
        "internal/event_target",
        INTERNAL_EVENT_TARGET_SHIM,
        &[],
    )
}

/// `node:cluster` — single-process stub (always primary, no workers).
pub fn cluster_cjs_value(ctx: &mut NativeCtx<'_>, caps: &CapabilitySet) -> Result<Value, String> {
    let events = crate::events::events_cjs_value(ctx, caps)?;
    otter_runtime::run_builtin_cjs_shim(ctx, "node:cluster", CLUSTER_SHIM, &[("events", events)])
}

/// `node:perf_hooks` — performance timeline subset.
pub fn perf_hooks_cjs_value(
    ctx: &mut NativeCtx<'_>,
    _caps: &CapabilitySet,
) -> Result<Value, String> {
    otter_runtime::run_builtin_cjs_shim(ctx, "node:perf_hooks", PERF_HOOKS_SHIM, &[])
}

/// `node:v8` — heap statistics + serialize/deserialize subset.
pub fn v8_cjs_value(ctx: &mut NativeCtx<'_>, caps: &CapabilitySet) -> Result<Value, String> {
    let buffer = crate::buffer::buffer_cjs_value(ctx, caps)?;
    otter_runtime::run_builtin_cjs_shim(ctx, "node:v8", V8_SHIM, &[("buffer", buffer)])
}

/// `node:module` — builtin-module metadata + a minimal Module class.
pub fn module_cjs_value(ctx: &mut NativeCtx<'_>, _caps: &CapabilitySet) -> Result<Value, String> {
    otter_runtime::run_builtin_cjs_shim(ctx, "node:module", MODULE_SHIM, &[])
}

/// `node:process` / `process` — the exact `globalThis.process` object.
pub fn process_cjs_value(ctx: &mut NativeCtx<'_>, _caps: &CapabilitySet) -> Result<Value, String> {
    ctx.global_value("process")
        .ok_or_else(|| "process global is not installed".to_string())
}

/// `internal/util` — the `--expose-internals` subset (sleep,
/// warnings, deprecation wrappers, and kEmptyObject).
pub fn internal_util_cjs_value(
    ctx: &mut NativeCtx<'_>,
    _caps: &CapabilitySet,
) -> Result<Value, String> {
    otter_runtime::run_builtin_cjs_shim(ctx, "internal/util", INTERNAL_UTIL_SHIM, &[])
}

/// `node:vm` — best-effort in-realm sandbox (with-scoped Proxy).
pub fn vm_cjs_value(ctx: &mut NativeCtx<'_>, _caps: &CapabilitySet) -> Result<Value, String> {
    otter_runtime::run_builtin_cjs_shim(ctx, "node:vm", VM_SHIM, &[])
}

/// `internal/url` brand predicate used by Node's own URL tests.
pub fn internal_url_cjs_value(
    ctx: &mut NativeCtx<'_>,
    _caps: &CapabilitySet,
) -> Result<Value, String> {
    otter_runtime::run_builtin_cjs_shim(ctx, "internal/url", INTERNAL_URL_SHIM, &[])
}

/// ESM namespace install — CommonJS is the supported surface for now.
pub fn install_noop(_ctx: &mut otter_runtime::HostedModuleCtx<'_>) -> Result<(), String> {
    Ok(())
}
