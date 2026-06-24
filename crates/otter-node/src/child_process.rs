//! `node:child_process` native core.
//!
//! A capability-gated synchronous spawn primitive; the async `spawn`/`exec`
//! surface and the `ChildProcess` class are layered on top in
//! `child_process.js`. Process output crosses the boundary as latin1 strings
//! (the same bridge the `fs` core uses), so the JS layer can present Buffers.
//!
//! # Invariants
//! - The `run` (subprocess) capability is checked before any process starts.
//! - No VM state is retained across the spawn.

use std::process::{Command, Stdio};
use std::sync::Arc;

use otter_runtime::module_scope::{ModuleScope, Rooted};
use otter_runtime::{
    CapabilitySet, RuntimeAttr, RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError,
    RuntimeObjectBuilder, RuntimeValue as Value, runtime_alloc_object, runtime_arg_to_string,
    runtime_native_dynamic,
};
use otter_vm::object;

const SHIM: &str = include_str!("child_process.js");

/// CommonJS export: the `child_process` namespace built by `child_process.js`.
pub fn child_process_cjs_value(
    ctx: &mut NativeCtx<'_>,
    caps: &CapabilitySet,
) -> Result<Value, String> {
    let native = native_value(ctx, caps)?;
    let buffer = crate::buffer::buffer_cjs_value(ctx, caps)?;
    let events = crate::events::events_cjs_value(ctx, caps)?;
    let stream = crate::stream::stream_cjs_value(ctx, caps)?;
    otter_runtime::run_builtin_cjs_shim(
        ctx,
        "node:child_process",
        SHIM,
        &[
            ("__cpnative", native),
            ("buffer", buffer),
            ("events", events),
            ("stream", stream),
        ],
    )
}

/// ESM namespace install — CommonJS is the supported surface.
pub fn install_child_process_module(
    _ctx: &mut otter_runtime::HostedModuleCtx<'_>,
) -> Result<(), String> {
    Ok(())
}

fn native_value(ctx: &mut NativeCtx<'_>, caps: &CapabilitySet) -> Result<Value, String> {
    let object = runtime_alloc_object(ctx).map_err(|e| e.to_string())?;
    let mut builder = RuntimeObjectBuilder::from_object(ctx, object);
    let caps = caps.clone();
    builder
        .method(
            "spawnSyncRaw",
            3,
            runtime_native_dynamic(Arc::new(
                move |ctx: &mut NativeCtx<'_>, args: &[Value], _c: &[Value]| {
                    spawn_sync_raw(ctx, args, &caps)
                },
            )),
            RuntimeAttr::builtin_function(),
        )
        .map_err(|e| e.to_string())?;
    Ok(Value::object(builder.build()))
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

fn opt_env(ctx: &mut NativeCtx<'_>, opts: Option<Value>) -> Option<Vec<(String, String)>> {
    let opts_obj = opts?.as_object()?;
    let env = object::get(opts_obj, ctx.heap(), "env")?.as_object()?;
    let keys: Vec<String> = object::with_properties(env, ctx.heap(), |p| {
        p.enumerable_keys().map(str::to_string).collect()
    });
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        if let Some(value) = object::get(env, ctx.heap(), &key)
            && !value.is_undefined()
            && !value.is_null()
        {
            out.push((key, value.display_string(ctx.heap())));
        }
    }
    Some(out)
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
    let env = opt_env(ctx, opts);
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

    let mut scope = ModuleScope::new(ctx);
    let obj = scope.object().map_err(oom)?;
    scope.set_number(obj, "pid", f64::from(pid));
    match status_code {
        Some(code) => scope.set_number(obj, "status", f64::from(code)),
        None => set_null(&mut scope, obj, "status"),
    }
    match signal {
        Some(sig) => scope.set_string(obj, "signal", &sig).map_err(oom)?,
        None => set_null(&mut scope, obj, "signal"),
    }
    scope.set_string(obj, "stdout", &stdout).map_err(oom)?;
    scope.set_string(obj, "stderr", &stderr).map_err(oom)?;
    set_null(&mut scope, obj, "error");
    Ok(scope.finish(obj))
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
    let mut scope = ModuleScope::new(ctx);
    let obj = scope.object().map_err(oom)?;
    set_null(&mut scope, obj, "pid");
    set_null(&mut scope, obj, "status");
    set_null(&mut scope, obj, "signal");
    scope.set_string(obj, "stdout", "").map_err(oom)?;
    scope.set_string(obj, "stderr", "").map_err(oom)?;
    scope.set_string(obj, "error", &message).map_err(oom)?;
    scope.set_string(obj, "errorCode", code).map_err(oom)?;
    Ok(scope.finish(obj))
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

fn set_null(scope: &mut ModuleScope<'_, '_>, obj: Rooted, key: &str) {
    let v = scope.null();
    scope.set(obj, key, v);
}

fn oom(err: String) -> NativeError {
    crate::type_error("child_process", err)
}
