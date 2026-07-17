//! `node:child_process` native core.
//!
//! A capability-gated synchronous spawn primitive; the async `spawn`/`exec`
//! surface and the `ChildProcess` class are layered on top in
//! `child_process.js`. Process output crosses the boundary as latin1 strings
//! (the same bridge the `fs` core uses), so the JS layer can present Buffers.
//!
//! # Invariants
//! - The `run` (subprocess) capability is checked before any process starts.
//! - Explicit child environments are enumerated through JavaScript internal
//!   methods, so filtered `process.env` proxies cannot leak hidden host values.
//! - No VM state is retained across the spawn.

use std::process::{Command, Stdio};

use otter_runtime::{
    CapabilitySet, RuntimeLocal as Local, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeNativeScope as NativeScope, RuntimeTaskSpawner,
    RuntimeValue as Value, runtime_arg_to_string,
};
use otter_vm::object;

const SHIM: &str = include_str!("child_process.js");

/// CommonJS export: the `child_process` namespace built by `child_process.js`.
pub fn child_process_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:child_process", SHIM, module, require)
}

/// Hidden CommonJS row supplying the capability-gated spawn primitive.
pub fn child_process_native_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    _module: Local<'scope>,
    _require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    native_value(scope, caps)
}

fn native_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    caps: &CapabilitySet,
) -> Result<Local<'scope>, NativeError> {
    let object = scope.object()?;
    let caps = caps.clone();
    let method = scope.native_closure(
        "spawnSyncRaw",
        3,
        &[],
        move |ctx: &mut NativeCtx<'_>, args: &[Value], _captures: &[Value]| {
            spawn_sync_raw(ctx, args, &caps)
        },
    )?;
    scope.set(object, "spawnSyncRaw", method)?;
    Ok(object)
}

fn bytes_to_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

fn latin1_to_bytes(s: &str) -> Vec<u8> {
    s.chars().map(|c| c as u32 as u8).collect()
}

/// Read a JS array of strings into a `Vec<String>`.
fn read_string_array(ctx: &mut NativeCtx<'_>, value: Value) -> Vec<String> {
    let Some(arr) = value.as_array() else {
        return Vec::new();
    };
    let len = otter_vm::array::len(arr, ctx.heap());
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let v = otter_vm::array::get(arr, ctx.heap(), i);
        out.push(v.display_string(ctx.heap()));
    }
    out
}

fn opt_string(ctx: &mut NativeCtx<'_>, opts: Option<Value>, key: &str) -> Option<String> {
    let obj = opts?.as_object()?;
    let v = object::get(obj, ctx.heap(), key)?;
    if v.is_string() {
        Some(v.display_string(ctx.heap()))
    } else {
        None
    }
}

fn opt_env(
    ctx: &mut NativeCtx<'_>,
    opts: Option<Value>,
) -> Result<Option<Vec<(String, String)>>, NativeError> {
    let Some(opts_obj) = opts.and_then(Value::as_object) else {
        return Ok(None);
    };
    let Some(env) = object::get(opts_obj, ctx.heap(), "env") else {
        return Ok(None);
    };
    if !env.is_object_type() {
        return Ok(None);
    }
    ctx.scope(|mut scope| {
        let env = scope.value(env);
        let keys = scope.enumerable_own_string_keys(env)?;
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            let value = scope.get(env, &key)?;
            if !scope.is_undefined(value) && !scope.is_null(value) {
                out.push((key, scope.display_string(value)));
            }
        }
        Ok(Some(out))
    })
}

fn current_exec_path(ctx: &mut NativeCtx<'_>) -> Option<String> {
    let global = *ctx.interp_mut().global_this();
    let process = object::get(global, ctx.heap(), "process")?.as_object()?;
    let exec_path = object::get(process, ctx.heap(), "execPath")?;
    Some(exec_path.display_string(ctx.heap()))
}

fn should_propagate_allow_all(
    ctx: &mut NativeCtx<'_>,
    command: &str,
    caps: &CapabilitySet,
) -> bool {
    caps.read.is_allow_all()
        && caps.write.is_allow_all()
        && caps.net.is_allow_all()
        && caps.env.is_allow_all()
        && caps.run.is_allow_all()
        && caps.ffi.is_allow_all()
        && current_exec_path(ctx).as_deref() == Some(command)
}

fn spawn_sync_raw(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let command = runtime_arg_to_string(args, 0, ctx.heap());
    if command.is_empty() {
        return Err(crate::type_error("child_process", "command is required"));
    }
    if !caps.run.matches(&command) {
        return Err(NativeError::Coded {
            kind: otter_vm::ErrorKind::Error,
            code: "EACCES",
            message: format!("EACCES: subprocess capability denied for '{command}'"),
        });
    }

    let mut argv = args
        .get(1)
        .copied()
        .map(|v| read_string_array(ctx, v))
        .unwrap_or_default();
    let opts = args.get(2).copied();
    let cwd = opt_string(ctx, opts, "cwd");
    let input = opt_string(ctx, opts, "input");
    let env = opt_env(ctx, opts)?;
    if should_propagate_allow_all(ctx, &command, caps) {
        argv.insert(0, "--allow-all".to_string());
    }

    let mut cmd = Command::new(&command);
    cmd.args(&argv);
    if let Some(dir) = &cwd {
        cmd.current_dir(dir);
    }
    if let Some(env) = env {
        cmd.env_clear();
        cmd.envs(env);
    }
    cmd.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let spawn_result = cmd.spawn();
    let mut child = match spawn_result {
        Ok(child) => child,
        Err(err) => return spawn_error_result(ctx, &command, &err),
    };
    let pid = child.id();

    if let (Some(input), Some(mut stdin)) = (&input, child.stdin.take()) {
        use std::io::Write;
        let _ = stdin.write_all(&latin1_to_bytes(input));
    }

    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(err) => return spawn_error_result(ctx, &command, &err),
    };

    let status_code = output.status.code();
    let signal = exit_signal(&output.status);
    let stdout = bytes_to_latin1(&output.stdout);
    let stderr = bytes_to_latin1(&output.stderr);

    ctx.scope(|mut scope| {
        let object = scope.object()?;
        set_number(&mut scope, object, "pid", f64::from(pid))?;
        match status_code {
            Some(code) => set_number(&mut scope, object, "status", f64::from(code))?,
            None => set_null(&mut scope, object, "status")?,
        }
        match signal {
            Some(signal) => {
                let signal = scope.string(&signal)?;
                scope.set(object, "signal", signal)?;
            }
            None => set_null(&mut scope, object, "signal")?,
        }
        let stdout = scope.string(&stdout)?;
        scope.set(object, "stdout", stdout)?;
        let stderr = scope.string(&stderr)?;
        scope.set(object, "stderr", stderr)?;
        set_null(&mut scope, object, "error")?;
        Ok(scope.finish(object))
    })
}

fn spawn_error_result(
    ctx: &mut NativeCtx<'_>,
    command: &str,
    err: &std::io::Error,
) -> Result<Value, NativeError> {
    let code = if err.kind() == std::io::ErrorKind::NotFound {
        "ENOENT"
    } else {
        "EIO"
    };
    let message = format!("{code}: spawn {command} {err}");
    ctx.scope(|mut scope| {
        let object = scope.object()?;
        set_null(&mut scope, object, "pid")?;
        set_null(&mut scope, object, "status")?;
        set_null(&mut scope, object, "signal")?;
        let stdout = scope.string("")?;
        scope.set(object, "stdout", stdout)?;
        let stderr = scope.string("")?;
        scope.set(object, "stderr", stderr)?;
        let error = scope.string(&message)?;
        scope.set(object, "error", error)?;
        let error_code = scope.string(code)?;
        scope.set(object, "errorCode", error_code)?;
        Ok(scope.finish(object))
    })
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|s| signal_name(s).to_string())
}
#[cfg(not(unix))]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<String> {
    None
}

#[cfg(unix)]
fn signal_name(sig: i32) -> &'static str {
    match sig {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        6 => "SIGABRT",
        8 => "SIGFPE",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        15 => "SIGTERM",
        _ => "SIGTERM",
    }
}

fn set_null(
    scope: &mut NativeScope<'_, '_>,
    object: Local<'_>,
    key: &str,
) -> Result<(), NativeError> {
    let value = scope.null();
    scope.set(object, key, value)
}

fn set_number(
    scope: &mut NativeScope<'_, '_>,
    object: Local<'_>,
    key: &str,
    value: f64,
) -> Result<(), NativeError> {
    let value = scope.number(value);
    scope.set(object, key, value)
}
