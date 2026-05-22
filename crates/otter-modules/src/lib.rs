//! Active Otter-hosted modules.
//!
//! This crate ports the host-owned parts of selected `otter:*` modules onto
//! the active runtime dependency graph. The JavaScript-visible shape is
//! installed through static hosted-module registration and builder APIs,
//! without a dynamic hot-path registry.
//!
//! # Contents
//! - [`kv`] - permission-gated key/value storage.
//! - [`sql`] - permission-gated SQLite access.
//! - [`ffi`] - permission-gated native library loading metadata.
//! - [`HOSTED_MODULES`] - static hosted-module specs.
//!
//! # Invariants
//! - Permission checks happen at the Rust boundary before host resources open.
//! - Host state is owned Rust data; no VM values, handles, or contexts are
//!   stored in futures or long-lived module state.
//! - JS surfaces use runtime-owned static native method helpers.
//!
//! # See also
//! - [Hosted modules](../../../docs/book/src/extensions/hosted-modules.md)

pub mod ffi;
pub mod kv;
pub mod sql;

use otter_runtime::{
    HostedModule, HostedModuleInstall, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeNumberValue as NumberValue, RuntimeValue as Value,
    runtime_arg_to_string, runtime_string_value, runtime_type_error,
};
use serde_json::{Number as JsonNumber, Value as JsonValue};

/// Active `otter:*` hosted modules in deterministic install order.
pub const HOSTED_MODULES: &[HostedModule] = &[
    HostedModule::new("otter:kv", HostedModuleInstall::new(kv::install_kv_module)),
    HostedModule::new(
        "otter:sql",
        HostedModuleInstall::new(sql::install_sql_module),
    ),
    HostedModule::new(
        "otter:ffi",
        HostedModuleInstall::new(ffi::install_ffi_module),
    ),
];

/// Return active hosted module installers.
#[must_use]
pub const fn hosted_modules() -> &'static [HostedModule] {
    HOSTED_MODULES
}

fn type_error(name: &'static str, reason: impl Into<String>) -> NativeError {
    runtime_type_error(name, reason)
}

fn arg_string(
    args: &[Value],
    index: usize,
    _name: &'static str,
    heap: &otter_runtime::otter_gc::GcHeap,
) -> Result<String, NativeError> {
    Ok(runtime_arg_to_string(args, index, heap))
}

fn string_value(ctx: &mut NativeCtx<'_>, value: &str) -> Result<Value, NativeError> {
    runtime_string_value(ctx, value)
}

fn json_to_value(ctx: &mut NativeCtx<'_>, value: JsonValue) -> Result<Value, NativeError> {
    match value {
        JsonValue::Null => Ok(Value::null()),
        JsonValue::Bool(value) => Ok(Value::boolean(value)),
        JsonValue::Number(value) => Ok(Value::number(NumberValue::from_f64(
            value.as_f64().unwrap_or(f64::NAN),
        ))),
        JsonValue::String(value) => string_value(ctx, &value),
        other => string_value(ctx, &other.to_string()),
    }
}

fn value_to_json(
    value: &Value,
    heap: &otter_runtime::otter_gc::GcHeap,
) -> Result<JsonValue, NativeError> {
    if value.is_undefined() || value.is_null() {
        return Ok(JsonValue::Null);
    }
    if let Some(b) = value.as_boolean() {
        return Ok(JsonValue::Bool(b));
    }
    if let Some(n) = value.as_number() {
        return JsonNumber::from_f64(n.as_f64())
            .map(JsonValue::Number)
            .ok_or_else(|| type_error("json", "number is not finite JSON"));
    }
    if let Some(s) = value.as_string(heap) {
        return Ok(JsonValue::String(s.to_lossy_string(heap)));
    }
    Err(type_error(
        "json",
        format!("cannot convert {} to JSON", value.display_string(heap)),
    ))
}
