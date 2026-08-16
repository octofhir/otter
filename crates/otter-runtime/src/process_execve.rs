//! `process.execve()` — replace the running process image.
//!
//! # Contents
//! - [`install`] defines the member on an already-built `process` object.
//! - Argument validation in Node's own order, with its codes and messages.
//!
//! # Invariants
//! - The `run` capability is checked against the target program before the
//!   image is replaced; a denied call throws instead of executing.
//! - Every argument and environment entry is rejected if it carries a NUL,
//!   which the syscall cannot represent, before any of them is converted.
//! - On success the call does not return: the new program owns the process.
//!
//! # See also
//! - [`crate::process_control`]

use std::ffi::CString;

use otter_vm::{
    ErrorKind, Local, NativeCall, NativeCtx, NativeError, NativeFn, NativeScope, Value,
};

use crate::CapabilitySet;

pub(crate) fn install(
    scope: &mut NativeScope<'_, '_>,
    process: Local<'_>,
    capabilities: &CapabilitySet,
) -> Result<(), NativeError> {
    if !cfg!(unix) {
        return Ok(());
    }
    crate::process::define_process_method(scope, process, "execve", 3, execve_call(capabilities))
}

fn execve_call(capabilities: &CapabilitySet) -> NativeCall {
    let capabilities = capabilities.clone();
    let call: std::sync::Arc<NativeFn> = std::sync::Arc::new(move |ctx, args, _captures| {
        let exec_path = args.first().copied().unwrap_or_else(Value::undefined);
        let Some(exec_path) = exec_path.as_string(ctx.heap()) else {
            return Err(invalid_arg_type(ctx, "execPath", "string", exec_path));
        };
        let exec_path = exec_path.to_lossy_string(ctx.heap());

        let argv = collect_args(ctx, args.get(1).copied())?;
        let env = collect_env(ctx, args.get(2).copied())?;

        if !capabilities.run.matches(&exec_path) {
            return Err(NativeError::Coded {
                kind: ErrorKind::Error,
                code: "EACCES",
                message: format!("EACCES: permission denied, execve '{exec_path}'"),
            });
        }

        replace_process_image(&exec_path, &argv, &env)
    });
    NativeCall::Dynamic(call)
}

/// Read the `args` array. Node requires an array of strings without NUL and
/// names the offending index in the message.
fn collect_args(ctx: &mut NativeCtx<'_>, value: Option<Value>) -> Result<Vec<String>, NativeError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_undefined() {
        return Ok(Vec::new());
    }
    if !ctx.is_array(value)? {
        return Err(NativeError::Coded {
            kind: ErrorKind::TypeError,
            code: "ERR_INVALID_ARG_TYPE",
            message: format!(
                "The \"args\" argument must be an instance of Array.{}",
                received_suffix(ctx, value)
            ),
        });
    }
    let length = ctx.array_length(value).unwrap_or(0);
    let mut collected = Vec::with_capacity(length);
    for index in 0..length {
        let entry = ctx.get_value_property(value, &index.to_string())?;
        let text = string_without_nul(ctx, entry).ok_or_else(|| NativeError::Coded {
            kind: ErrorKind::TypeError,
            code: "ERR_INVALID_ARG_VALUE",
            message: format!(
                "The argument 'args[{index}]' must be a string without null bytes. Received {}",
                inspect(ctx, entry)
            ),
        })?;
        collected.push(text);
    }
    Ok(collected)
}

/// Read the `env` object as `KEY=VALUE` entries. Node rejects the whole object
/// when any key or value is not a NUL-free string.
fn collect_env(ctx: &mut NativeCtx<'_>, value: Option<Value>) -> Result<Vec<String>, NativeError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_undefined() {
        return Ok(Vec::new());
    }
    // `process.env` is a proxy, and Node accepts it here: anything that is not
    // a primitive is an object for this check.
    if is_primitive(ctx, value) {
        return Err(NativeError::Coded {
            kind: ErrorKind::TypeError,
            code: "ERR_INVALID_ARG_TYPE",
            message: format!(
                "The \"env\" argument must be of type object.{}",
                received_suffix(ctx, value)
            ),
        });
    }

    let invalid = |ctx: &mut NativeCtx<'_>, value: Value| NativeError::Coded {
        kind: ErrorKind::TypeError,
        code: "ERR_INVALID_ARG_VALUE",
        message: format!(
            "The argument 'env' must be an object with string keys and values without \
             null bytes. Received {}",
            inspect(ctx, value)
        ),
    };

    let keys = ctx.enumerable_own_string_keys(value)?;
    let mut collected = Vec::with_capacity(keys.len());
    for key in keys {
        if key.contains('\0') {
            return Err(invalid(ctx, value));
        }
        let entry = ctx.get_value_property(value, &key)?;
        let Some(text) = string_without_nul(ctx, entry) else {
            return Err(invalid(ctx, value));
        };
        collected.push(format!("{key}={text}"));
    }
    Ok(collected)
}

/// Whether the value is a primitive — the negative of "object" for an argument
/// check that must accept proxies and functions alike.
fn is_primitive(ctx: &mut NativeCtx<'_>, value: Value) -> bool {
    value.is_undefined()
        || value.is_null()
        || value.as_boolean().is_some()
        || value.as_number().is_some()
        || value.as_string(ctx.heap()).is_some()
        || value.as_symbol(ctx.heap()).is_some()
        || value.as_big_int().is_some()
}

fn string_without_nul(ctx: &mut NativeCtx<'_>, value: Value) -> Option<String> {
    let text = value.as_string(ctx.heap())?.to_lossy_string(ctx.heap());
    (!text.contains('\0')).then_some(text)
}

#[cfg(unix)]
fn replace_process_image(
    exec_path: &str,
    argv: &[String],
    env: &[String],
) -> Result<Value, NativeError> {
    let to_c = |text: &String| CString::new(text.as_str()).expect("checked for NUL above");
    let path = CString::new(exec_path).map_err(|_| NativeError::Coded {
        kind: ErrorKind::TypeError,
        code: "ERR_INVALID_ARG_VALUE",
        message: "The argument 'execPath' must be a string without null bytes".to_string(),
    })?;
    let argv: Vec<CString> = argv.iter().map(to_c).collect();
    let env: Vec<CString> = env.iter().map(to_c).collect();

    // On success this never returns: the new program owns the process.
    let errno =
        nix::unistd::execve(&path, &argv, &env).expect_err("execve returns only on failure");
    let code = crate::process_control::errno_code_from_raw(errno as i32);
    Err(NativeError::Syscall {
        code,
        message: format!("{code}, {}", errno.desc()),
        syscall: "execve",
        path: Some(exec_path.to_string()),
        dest: None,
        errno: errno as i32,
    })
}

#[cfg(not(unix))]
fn replace_process_image(
    _exec_path: &str,
    _argv: &[String],
    _env: &[String],
) -> Result<Value, NativeError> {
    Err(NativeError::Coded {
        kind: ErrorKind::Error,
        code: "ERR_METHOD_NOT_IMPLEMENTED",
        message: "process.execve is not available on this platform".to_string(),
    })
}

fn invalid_arg_type(
    ctx: &mut NativeCtx<'_>,
    argument: &str,
    expected: &str,
    value: Value,
) -> NativeError {
    NativeError::Coded {
        kind: ErrorKind::TypeError,
        code: "ERR_INVALID_ARG_TYPE",
        message: format!(
            "The \"{argument}\" argument must be of type {expected}.{}",
            received_suffix(ctx, value)
        ),
    }
}

fn received_suffix(ctx: &mut NativeCtx<'_>, value: Value) -> String {
    crate::process_control::received_suffix(ctx, value)
}

/// Render a value the way the messages these errors carry render it: numbers
/// bare, strings quoted with control characters escaped, flat objects as
/// `{ key: value }`.
fn inspect(ctx: &mut NativeCtx<'_>, value: Value) -> String {
    if let Some(text) = value.as_string(ctx.heap()) {
        return quote(&text.to_lossy_string(ctx.heap()));
    }
    if !is_primitive(ctx, value) {
        let keys = ctx.enumerable_own_string_keys(value).unwrap_or_default();
        if keys.is_empty() {
            return "{}".to_string();
        }
        let rendered: Vec<String> = keys
            .iter()
            .map(|key| {
                let entry = ctx
                    .get_value_property(value, key)
                    .unwrap_or_else(|_| Value::undefined());
                format!("{key}: {}", inspect(ctx, entry))
            })
            .collect();
        return format!("{{ {} }}", rendered.join(", "));
    }
    value.display_string(ctx.heap())
}

fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for character in text.chars() {
        match character {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\x{:02x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('\'');
    out
}
