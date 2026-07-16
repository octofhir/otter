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

use otter_vm::{Attr, ErrorKind, Local, NativeCall, NativeCtx, NativeError, NativeScope, Value};

use crate::{CapabilityRequest, CapabilitySet, RuntimeCapability, RuntimeHooks};

const DEPRECATION_MESSAGE: &str = "Assigning any value other than a string, number, or boolean to a process.env property is deprecated. Please make sure to convert the value to a string before setting process.env with it.";

pub(crate) fn build<'s>(
    scope: &mut NativeScope<'s, '_>,
    capabilities: &CapabilitySet,
    hooks: &RuntimeHooks,
) -> Result<Local<'s>, NativeError> {
    let target = scope.object()?;
    for (name, value) in std::env::vars() {
        if crate::hooks::check_capability_with_hooks(
            hooks,
            capabilities,
            RuntimeCapability::Env,
            &CapabilityRequest::EnvVar(&name),
        ) {
            let value = scope.string(&value)?;
            scope.set(target, &name, value)?;
        }
    }

    let handler = scope.bare_object()?;
    for (name, length, call) in [
        ("set", 4, env_set as _),
        ("defineProperty", 3, env_define_property as _),
    ] {
        let trap = scope.native_call(name, length, NativeCall::Static(call))?;
        scope.define(handler, name, trap, Attr::builtin_function().to_flags())?;
    }
    scope.proxy(target, handler)
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
    scope: &mut NativeScope<'_, '_>,
    process: Local<'_>,
) -> Result<(), NativeError> {
    let emit_warning = scope.get(process, "emitWarning")?;
    let message = scope.string(DEPRECATION_MESSAGE)?;
    let warning_type = scope.string("DeprecationWarning")?;
    let code = scope.string("DEP0104")?;
    scope.call(emit_warning, process, &[message, warning_type, code])?;
    Ok(())
}

fn store_env_value(
    scope: &mut NativeScope<'_, '_>,
    target: Local<'_>,
    name: &str,
    value: Local<'_>,
    string_constructor: Local<'_>,
    process: Local<'_>,
    emit_warning: bool,
) -> Result<(), NativeError> {
    if name.is_empty() && !cfg!(windows) {
        return Ok(());
    }
    if emit_warning {
        emit_deprecation_warning(scope, process)?;
    }
    let undefined = scope.undefined();
    let converted = scope.call(string_constructor, undefined, &[value])?;
    scope.set(target, name, converted)
}

fn env_set(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let target = args.first().copied().unwrap_or_else(Value::undefined);
    let key = args.get(1).copied().unwrap_or_else(Value::undefined);
    let value = args.get(2).copied().unwrap_or_else(Value::undefined);
    let name = property_name(ctx, key)?;
    if value.is_symbol() {
        return Err(NativeError::TypeError {
            name: "process.env",
            reason: "Cannot convert a Symbol value to a string".to_string(),
        });
    }
    let emit_warning = !value.is_string() && !value.is_number() && !value.is_boolean();
    let string_constructor = ctx
        .global_value("String")
        .ok_or_else(|| NativeError::TypeError {
            name: "process.env",
            reason: "String constructor is unavailable".to_string(),
        })?;
    let process = ctx
        .global_value("process")
        .ok_or_else(|| NativeError::TypeError {
            name: "process.env",
            reason: "process global is unavailable".to_string(),
        })?;
    ctx.scope(|mut scope| {
        let target = scope.value(target);
        let value = scope.value(value);
        let string_constructor = scope.value(string_constructor);
        let process = scope.value(process);
        store_env_value(
            &mut scope,
            target,
            &name,
            value,
            string_constructor,
            process,
            emit_warning,
        )?;
        let result = scope.boolean(true);
        Ok(scope.finish(result))
    })
}

fn env_define_property(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let target = args.first().copied().unwrap_or_else(Value::undefined);
    let key = args.get(1).copied().unwrap_or_else(Value::undefined);
    let descriptor = args.get(2).copied().unwrap_or_else(Value::undefined);
    let name = property_name(ctx, key)?;
    let value = ctx.scope(|mut scope| {
        let descriptor = scope.value(descriptor);
        let getter = scope.get(descriptor, "get")?;
        let setter = scope.get(descriptor, "set")?;
        if !scope.is_undefined(getter) || !scope.is_undefined(setter) {
            return Err(coded_type_error(
                "ERR_INVALID_OBJECT_DEFINE_PROPERTY",
                "'process.env' does not accept an accessor(getter/setter) descriptor",
            ));
        }
        for attribute in ["configurable", "writable", "enumerable"] {
            let value = scope.get(descriptor, attribute)?;
            if scope.boolean_value(value).ok() != Some(true) {
                return Err(coded_type_error(
                    "ERR_INVALID_OBJECT_DEFINE_PROPERTY",
                    "'process.env' only accepts a configurable, writable, and enumerable data descriptor",
                ));
            }
        }
        let value = scope.get(descriptor, "value")?;
        Ok::<Value, NativeError>(scope.finish(value))
    })?;
    if value.is_symbol() {
        return Err(NativeError::TypeError {
            name: "process.env",
            reason: "Cannot convert a Symbol value to a string".to_string(),
        });
    }
    let emit_warning = !value.is_string() && !value.is_number() && !value.is_boolean();
    let string_constructor = ctx
        .global_value("String")
        .ok_or_else(|| NativeError::TypeError {
            name: "process.env",
            reason: "String constructor is unavailable".to_string(),
        })?;
    let process = ctx
        .global_value("process")
        .ok_or_else(|| NativeError::TypeError {
            name: "process.env",
            reason: "process global is unavailable".to_string(),
        })?;
    ctx.scope(|mut scope| {
        let target = scope.value(target);
        let value = scope.value(value);
        let string_constructor = scope.value(string_constructor);
        let process = scope.value(process);
        store_env_value(
            &mut scope,
            target,
            &name,
            value,
            string_constructor,
            process,
            emit_warning,
        )?;
        let result = scope.boolean(true);
        Ok(scope.finish(result))
    })
}
