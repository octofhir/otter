//! Capability-filtered Node-compatible `process.env` proxy.
//!
//! The proxy owns a JavaScript snapshot of environment variables admitted by
//! the runtime capability boundary. Writes mutate only that JS-owned snapshot;
//! they never call `set_var` or otherwise alter the embedding process.
//!
//! # Contents
//! - [`build`] filters the host snapshot and creates the proxy.
//! - Proxy `set` coercion matching Node's string-valued environment surface.
//! - Proxy `defineProperty` validation for Node's descriptor restrictions.
//!
//! # Invariants
//! - Host values cross the boundary only after `RuntimeCapability::Env` checks.
//! - Built-in secret-name filters cannot be overridden by custom hooks.
//! - All JS values are built and mutated through [`NativeCtx`] handle scopes.
//! - JS writes remain isolate-local and cannot mutate the host environment.
//!
//! # See also
//! - [`crate::CapabilitySet::env_allows`]
//! - [`crate::hooks::check_capability_with_hooks`]

use otter_vm::{Attr, ErrorKind, HandleScope, NativeCall, NativeCtx, NativeError, Scoped, Value};

use crate::{CapabilityRequest, CapabilitySet, RuntimeCapability, RuntimeHooks};

const DEPRECATION_MESSAGE: &str = "Assigning any value other than a string, number, or boolean to a process.env property is deprecated. Please make sure to convert the value to a string before setting process.env with it.";

pub(crate) fn build<'s>(
    ctx: &mut NativeCtx<'_>,
    scope: &'s HandleScope,
    capabilities: &CapabilitySet,
    hooks: &RuntimeHooks,
) -> Result<Scoped<'s>, NativeError> {
    let target = ctx.scoped_object(scope)?;
    for (name, value) in std::env::vars() {
        if crate::hooks::check_capability_with_hooks(
            hooks,
            capabilities,
            RuntimeCapability::Env,
            &CapabilityRequest::EnvVar(&name),
        ) {
            let value = ctx.scoped_string(scope, &value)?;
            ctx.scoped_set(scope, target, &name, value)?;
        }
    }

    let handler = ctx.scoped_object_bare(scope)?;
    for (name, length, call) in [
        ("set", 4, env_set as _),
        ("defineProperty", 3, env_define_property as _),
    ] {
        let trap = ctx.scoped_native_call(scope, name, length, NativeCall::Static(call))?;
        ctx.scoped_define_data(
            scope,
            handler,
            name,
            trap,
            Attr::builtin_function().to_flags(),
        )?;
    }
    ctx.scoped_proxy(scope, target, handler)
}

fn coded_type_error(code: &'static str, message: impl Into<String>) -> NativeError {
    NativeError::Coded {
        kind: ErrorKind::TypeError,
        code,
        message: message.into(),
    }
}

fn property_name(ctx: &NativeCtx<'_>, key: Value) -> Result<String, NativeError> {
    if key.is_symbol() {
        return Err(NativeError::TypeError {
            name: "process.env",
            reason: "Cannot convert a Symbol value to a string".to_string(),
        });
    }
    key.as_string(ctx.heap())
        .map(|name| name.to_lossy_string(ctx.heap()))
        .ok_or_else(|| NativeError::TypeError {
            name: "process.env",
            reason: "property key must be a string".to_string(),
        })
}

fn emit_deprecation_warning(
    ctx: &mut NativeCtx<'_>,
    scope: &HandleScope,
) -> Result<(), NativeError> {
    let process = ctx
        .global_value("process")
        .ok_or_else(|| NativeError::TypeError {
            name: "process.env",
            reason: "process global is unavailable".to_string(),
        })?;
    let process = ctx.scoped_value(scope, process);
    let emit_warning = ctx.scoped_get(scope, process, "emitWarning")?;
    let message = ctx.scoped_string(scope, DEPRECATION_MESSAGE)?;
    let warning_type = ctx.scoped_string(scope, "DeprecationWarning")?;
    let code = ctx.scoped_string(scope, "DEP0104")?;
    ctx.call(
        ctx.escape(emit_warning),
        ctx.escape(process),
        &[
            ctx.escape(message),
            ctx.escape(warning_type),
            ctx.escape(code),
        ],
    )?;
    Ok(())
}

fn store_env_value(
    ctx: &mut NativeCtx<'_>,
    scope: &HandleScope,
    target: Scoped<'_>,
    name: &str,
    value: Scoped<'_>,
) -> Result<(), NativeError> {
    if name.is_empty() && !cfg!(windows) {
        return Ok(());
    }
    let raw_value = ctx.escape(value);
    if raw_value.is_symbol() {
        return Err(NativeError::TypeError {
            name: "process.env",
            reason: "Cannot convert a Symbol value to a string".to_string(),
        });
    }
    if !raw_value.is_string() && !raw_value.is_number() && !raw_value.is_boolean() {
        emit_deprecation_warning(ctx, scope)?;
    }
    let string = ctx
        .global_value("String")
        .ok_or_else(|| NativeError::TypeError {
            name: "process.env",
            reason: "String constructor is unavailable".to_string(),
        })?;
    let string = ctx.scoped_value(scope, string);
    let undefined = ctx.scoped_undefined(scope);
    let converted = ctx.call(
        ctx.escape(string),
        ctx.escape(undefined),
        &[ctx.escape(value)],
    )?;
    let converted = ctx.scoped_value(scope, converted);
    ctx.scoped_set(scope, target, name, converted)
}

fn env_set(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let target = args.first().copied().unwrap_or_else(Value::undefined);
    let key = args.get(1).copied().unwrap_or_else(Value::undefined);
    let value = args.get(2).copied().unwrap_or_else(Value::undefined);
    let name = property_name(ctx, key)?;
    ctx.scope(|ctx, scope| {
        let target = ctx.scoped_value(scope, target);
        let value = ctx.scoped_value(scope, value);
        store_env_value(ctx, scope, target, &name, value)?;
        Ok(Value::boolean(true))
    })
}

fn env_define_property(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let target = args.first().copied().unwrap_or_else(Value::undefined);
    let key = args.get(1).copied().unwrap_or_else(Value::undefined);
    let descriptor = args.get(2).copied().unwrap_or_else(Value::undefined);
    let name = property_name(ctx, key)?;
    ctx.scope(|ctx, scope| {
        let target = ctx.scoped_value(scope, target);
        let descriptor = ctx.scoped_value(scope, descriptor);
        let getter = ctx.scoped_get(scope, descriptor, "get")?;
        let setter = ctx.scoped_get(scope, descriptor, "set")?;
        if !ctx.escape(getter).is_undefined() || !ctx.escape(setter).is_undefined() {
            return Err(coded_type_error(
                "ERR_INVALID_OBJECT_DEFINE_PROPERTY",
                "'process.env' does not accept an accessor(getter/setter) descriptor",
            ));
        }
        for attribute in ["configurable", "writable", "enumerable"] {
            let value = ctx.scoped_get(scope, descriptor, attribute)?;
            if ctx.escape(value).as_boolean() != Some(true) {
                return Err(coded_type_error(
                    "ERR_INVALID_OBJECT_DEFINE_PROPERTY",
                    "'process.env' only accepts a configurable, writable, and enumerable data descriptor",
                ));
            }
        }
        let value = ctx.scoped_get(scope, descriptor, "value")?;
        store_env_value(ctx, scope, target, &name, value)?;
        Ok(Value::boolean(true))
    })
}
