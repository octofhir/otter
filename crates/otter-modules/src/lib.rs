//! Active Otter-hosted modules.
//!
//! This crate ports the host-owned parts of `otter:kv`, `otter:sql`, and
//! `otter:ffi` onto the active runtime dependency graph. The JavaScript-visible
//! shape is installed through static hosted-module registration and builder
//! APIs, without a dynamic hot-path registry.
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
//! - JS surfaces are static specs using `NativeCall::Static`.
//!
//! # See also
//! - [Hosted modules](../../../docs/book/src/extensions/hosted-modules.md)

pub mod ffi;
pub mod kv;
pub mod sql;

use otter_runtime::module_api::{JsString, NativeCtx, NativeError, NumberValue, Value};
use otter_runtime::{HostedModule, HostedModuleInstall};
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
    NativeError::TypeError {
        name,
        reason: reason.into(),
    }
}

fn arg_string(args: &[Value], index: usize, _name: &'static str) -> Result<String, NativeError> {
    match args.get(index) {
        Some(Value::String(value)) => Ok(value.to_lossy_string()),
        Some(Value::Undefined) | None => Ok(String::new()),
        Some(value) => Ok(value.display_string()),
    }
}

fn string_value(ctx: &mut NativeCtx<'_>, value: &str) -> Result<Value, NativeError> {
    let heap = ctx.interp_mut().string_heap_clone();
    Ok(Value::String(
        JsString::from_str(value, &heap).map_err(|err| type_error("string", err.to_string()))?,
    ))
}

fn json_to_value(ctx: &mut NativeCtx<'_>, value: JsonValue) -> Result<Value, NativeError> {
    match value {
        JsonValue::Null => Ok(Value::Null),
        JsonValue::Bool(value) => Ok(Value::Boolean(value)),
        JsonValue::Number(value) => Ok(Value::Number(NumberValue::from_f64(
            value.as_f64().unwrap_or(f64::NAN),
        ))),
        JsonValue::String(value) => string_value(ctx, &value),
        other => string_value(ctx, &other.to_string()),
    }
}

fn value_to_json(value: &Value) -> Result<JsonValue, NativeError> {
    match value {
        Value::Undefined | Value::Null => Ok(JsonValue::Null),
        Value::Boolean(value) => Ok(JsonValue::Bool(*value)),
        Value::Number(value) => JsonNumber::from_f64(value.as_f64())
            .map(JsonValue::Number)
            .ok_or_else(|| type_error("json", "number is not finite JSON")),
        Value::String(value) => Ok(JsonValue::String(value.to_lossy_string())),
        other => Err(type_error(
            "json",
            format!("cannot convert {} to JSON", other.display_string()),
        )),
    }
}
