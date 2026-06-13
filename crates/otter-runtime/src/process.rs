//! Node-compatible `process` global installed by the runtime.
//!
//! # Contents
//! - [`default_argv`] builds the runtime's default `process.argv` snapshot.
//! - [`default_cwd`] builds the runtime's default `process.cwd()` snapshot.
//! - [`install_global`] materializes the JS-visible `process` object.
//!
//! # Invariants
//! - `process.env` is capability-filtered at install time and never bypasses
//!   the runtime's deny-by-default policy or secret denylist.
//! - Host data is copied into JS-owned values. This module does not expose VM
//!   internals across the public runtime boundary.
//!
//! # See also
//! - [`crate::RuntimeBuilder::process_argv`]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use otter_vm::{
    Attr, Interpreter, JsString, NativeCall, NativeCtx, NativeError, NativeFn, NumberValue, Value,
};
use sysinfo::{ProcessesToUpdate, System};

use crate::{
    CapabilityRequest, CapabilitySet, DiagnosticCode, OtterError, RuntimeCapability,
    default_check_capability, gc_oom_to_error, string_oom_to_error,
};

pub(crate) fn default_argv() -> Vec<String> {
    vec![runtime_process_snapshot().exec_path]
}

pub(crate) fn default_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

pub(crate) fn install_global(
    interp: &mut Interpreter,
    process_argv: &[String],
    process_cwd: &Path,
    capabilities: &CapabilitySet,
) -> Result<(), OtterError> {
    let snapshot = runtime_process_snapshot();
    let uptime_base_secs = snapshot.run_time_secs;
    let start = Instant::now();
    let process = interp
        .alloc_host_object_with_roots(&[], &[])
        .map_err(gc_oom_to_error)?;
    let process_root = Value::object(process);
    let argv_values = process_argv
        .iter()
        .map(|arg| {
            JsString::from_str(arg, interp.gc_heap_mut())
                .map(otter_vm::Value::string)
                .map_err(string_oom_to_error)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let argv = interp
        .array_from_elements_host_rooted(
            argv_values.iter().cloned(),
            &[&process_root],
            &[&argv_values],
        )
        .map_err(gc_oom_to_error)?;
    otter_vm::object::set(
        process,
        interp.gc_heap_mut(),
        "argv",
        otter_vm::Value::array(argv),
    );

    let exec_argv = interp
        .array_from_elements_host_rooted([], &[&process_root], &[])
        .map_err(gc_oom_to_error)?;
    otter_vm::object::set(
        process,
        interp.gc_heap_mut(),
        "execArgv",
        otter_vm::Value::array(exec_argv),
    );

    let argv0 = process_argv.first().map(String::as_str).unwrap_or("otter");
    let argv0 = string_value(interp, argv0)?;
    otter_vm::object::set(process, interp.gc_heap_mut(), "argv0", argv0);

    let exec_path = string_value(interp, &snapshot.exec_path)?;
    otter_vm::object::set(process, interp.gc_heap_mut(), "execPath", exec_path);

    let platform = string_value(interp, node_platform())?;
    otter_vm::object::set(process, interp.gc_heap_mut(), "platform", platform);

    let arch = string_value(interp, node_arch())?;
    otter_vm::object::set(process, interp.gc_heap_mut(), "arch", arch);

    let version = string_value(interp, concat!("v", env!("CARGO_PKG_VERSION")))?;
    otter_vm::object::set(process, interp.gc_heap_mut(), "version", version);

    let versions = interp
        .alloc_host_object_with_roots(&[&process_root], &[])
        .map_err(gc_oom_to_error)?;
    let otter_version = string_value(interp, env!("CARGO_PKG_VERSION"))?;
    otter_vm::object::set(versions, interp.gc_heap_mut(), "otter", otter_version);
    let node_version = string_value(interp, env!("CARGO_PKG_VERSION"))?;
    otter_vm::object::set(versions, interp.gc_heap_mut(), "node", node_version);
    otter_vm::object::set(
        process,
        interp.gc_heap_mut(),
        "versions",
        otter_vm::Value::object(versions),
    );

    let release = interp
        .alloc_host_object_with_roots(&[&process_root], &[])
        .map_err(gc_oom_to_error)?;
    let release_name = string_value(interp, "node")?;
    otter_vm::object::set(release, interp.gc_heap_mut(), "name", release_name);
    otter_vm::object::set(
        process,
        interp.gc_heap_mut(),
        "release",
        otter_vm::Value::object(release),
    );

    otter_vm::object::set(
        process,
        interp.gc_heap_mut(),
        "pid",
        Value::number(NumberValue::from_i32(pid_to_i32(snapshot.pid))),
    );
    otter_vm::object::set(
        process,
        interp.gc_heap_mut(),
        "ppid",
        Value::number(NumberValue::from_i32(pid_to_i32(
            snapshot.ppid.unwrap_or(0),
        ))),
    );

    otter_vm::object::set(
        process,
        interp.gc_heap_mut(),
        "exitCode",
        otter_vm::Value::undefined(),
    );

    let env = interp
        .alloc_host_object_with_roots(&[&process_root], &[])
        .map_err(gc_oom_to_error)?;
    for (name, value) in std::env::vars() {
        if !default_check_capability(
            capabilities,
            RuntimeCapability::Env,
            &CapabilityRequest::EnvVar(&name),
        ) {
            continue;
        }
        let value = JsString::from_str(&value, interp.gc_heap_mut())
            .map(otter_vm::Value::string)
            .map_err(string_oom_to_error)?;
        otter_vm::object::set(env, interp.gc_heap_mut(), &name, value);
    }
    otter_vm::object::set(
        process,
        interp.gc_heap_mut(),
        "env",
        otter_vm::Value::object(env),
    );

    define_process_method(
        interp,
        process,
        &process_root,
        "cwd",
        0,
        cwd_call(process_cwd.to_string_lossy().to_string()),
    )?;
    define_process_method(
        interp,
        process,
        &process_root,
        "exit",
        1,
        NativeCall::Static(process_exit),
    )?;
    define_process_method(
        interp,
        process,
        &process_root,
        "nextTick",
        1,
        NativeCall::Static(process_next_tick),
    )?;
    define_process_method(
        interp,
        process,
        &process_root,
        "uptime",
        0,
        uptime_call(start, uptime_base_secs),
    )?;
    define_process_method(
        interp,
        process,
        &process_root,
        "memoryUsage",
        0,
        NativeCall::Static(process_memory_usage),
    )?;
    let hrtime = hrtime_value(interp, start).map_err(gc_oom_to_error)?;
    otter_vm::object::set(process, interp.gc_heap_mut(), "hrtime", hrtime);

    install_stdio_streams(interp, process, &process_root)?;

    define_process_method(
        interp,
        process,
        &process_root,
        "umask",
        1,
        NativeCall::Static(process_umask),
    )?;

    // Minimal EventEmitter surface so harness/test code that registers process
    // event listeners loads. TODO: real event dispatch (exit/uncaughtException).
    for name in [
        "on",
        "once",
        "off",
        "addListener",
        "removeListener",
        "prependListener",
        "prependOnceListener",
        "removeAllListeners",
        "emit",
        "listenerCount",
        "listeners",
        "setMaxListeners",
    ] {
        define_process_method(
            interp,
            process,
            &process_root,
            name,
            1,
            NativeCall::Static(process_event_noop),
        )?;
    }

    // `process.config.variables.*` is read by the Node test harness at load.
    let config = interp
        .alloc_host_object_with_roots(&[&process_root], &[])
        .map_err(gc_oom_to_error)?;
    let variables = interp
        .alloc_host_object_with_roots(&[&process_root], &[])
        .map_err(gc_oom_to_error)?;
    otter_vm::object::set(
        variables,
        interp.gc_heap_mut(),
        "v8_enable_i18n_support",
        Value::boolean(false),
    );
    otter_vm::object::set(
        config,
        interp.gc_heap_mut(),
        "variables",
        otter_vm::Value::object(variables),
    );
    otter_vm::object::set(
        process,
        interp.gc_heap_mut(),
        "config",
        otter_vm::Value::object(config),
    );

    // `process.features.*` — feature flags read by the Node test harness.
    let features = interp
        .alloc_host_object_with_roots(&[&process_root], &[])
        .map_err(gc_oom_to_error)?;
    for (name, on) in [
        ("inspector", false),
        ("quic", false),
        ("tls", false),
        ("debug", false),
        ("uv", true),
        ("ipv6", true),
        ("cached_builtins", false),
        ("require_module", true),
        ("typescript", false),
    ] {
        otter_vm::object::set(features, interp.gc_heap_mut(), name, Value::boolean(on));
    }
    otter_vm::object::set(
        process,
        interp.gc_heap_mut(),
        "features",
        otter_vm::Value::object(features),
    );

    interp.set_global("process", otter_vm::Value::object(process));
    Ok(())
}

/// `process.umask([mask])` — returns the previous mask. Otter does not change
/// the process umask; it reports `0` so harness setup code proceeds.
fn process_umask(
    _ctx: &mut NativeCtx<'_>,
    _args: &[otter_vm::Value],
) -> Result<otter_vm::Value, NativeError> {
    Ok(Value::number(NumberValue::from_i32(0)))
}

/// Placeholder for the `process` EventEmitter methods — accepts the call and
/// does nothing. Returns `process` so chained calls work.
fn process_event_noop(
    ctx: &mut NativeCtx<'_>,
    _args: &[otter_vm::Value],
) -> Result<otter_vm::Value, NativeError> {
    Ok(*ctx.this_value())
}

/// Install `process.stdout` / `process.stderr` / `process.stdin` as minimal
/// stream-like objects. Many tests gate on `process.stdout.isTTY` (reading a
/// property off `undefined` otherwise throws) and write through
/// `process.stdout.write`; the EventEmitter-style methods are no-ops that
/// return the stream for chaining.
fn install_stdio_streams(
    interp: &mut Interpreter,
    process: otter_vm::object::JsObject,
    process_root: &Value,
) -> Result<(), OtterError> {
    install_one_stdio(
        interp,
        process,
        process_root,
        "stdout",
        1,
        false,
        NativeCall::Static(stdout_write),
    )?;
    install_one_stdio(
        interp,
        process,
        process_root,
        "stderr",
        2,
        false,
        NativeCall::Static(stderr_write),
    )?;
    install_one_stdio(
        interp,
        process,
        process_root,
        "stdin",
        0,
        true,
        NativeCall::Static(stdio_return_this),
    )?;
    Ok(())
}

fn install_one_stdio(
    interp: &mut Interpreter,
    process: otter_vm::object::JsObject,
    process_root: &Value,
    name: &'static str,
    fd: i32,
    readable: bool,
    write_call: NativeCall,
) -> Result<(), OtterError> {
    let stream = interp
        .alloc_host_object_with_roots(&[process_root], &[])
        .map_err(gc_oom_to_error)?;
    let stream_root = Value::object(stream);
    let i32n = |n: i32| Value::number(NumberValue::from_i32(n));
    otter_vm::object::set(stream, interp.gc_heap_mut(), "isTTY", Value::boolean(false));
    otter_vm::object::set(stream, interp.gc_heap_mut(), "fd", i32n(fd));
    otter_vm::object::set(
        stream,
        interp.gc_heap_mut(),
        "writable",
        Value::boolean(!readable),
    );
    otter_vm::object::set(
        stream,
        interp.gc_heap_mut(),
        "readable",
        Value::boolean(readable),
    );
    otter_vm::object::set(stream, interp.gc_heap_mut(), "columns", i32n(80));
    otter_vm::object::set(stream, interp.gc_heap_mut(), "rows", i32n(24));

    define_method_on(
        interp,
        stream,
        &stream_root,
        process_root,
        "write",
        1,
        write_call,
    )?;
    for method in [
        "end",
        "cork",
        "uncork",
        "destroy",
        "on",
        "once",
        "addListener",
        "removeListener",
        "removeAllListeners",
        "emit",
        "setEncoding",
        "pause",
        "resume",
        "ref",
        "unref",
    ] {
        define_method_on(
            interp,
            stream,
            &stream_root,
            process_root,
            method,
            0,
            NativeCall::Static(stdio_return_this),
        )?;
    }
    otter_vm::object::set(process, interp.gc_heap_mut(), name, stream_root);
    Ok(())
}

fn define_method_on(
    interp: &mut Interpreter,
    target: otter_vm::object::JsObject,
    target_root: &Value,
    extra_root: &Value,
    name: &'static str,
    length: u8,
    call: NativeCall,
) -> Result<(), OtterError> {
    let value = interp
        .native_function_from_call_host_rooted(name, length, call, &[target_root, extra_root], &[])
        .map_err(gc_oom_to_error)?;
    let descriptor = otter_vm::object::PropertyDescriptor {
        kind: otter_vm::object::DescriptorKind::Data { value },
        flags: Attr::builtin_function().to_flags(),
    };
    otter_vm::object::define_own_property(target, interp.gc_heap_mut(), name, descriptor);
    Ok(())
}

fn stdout_write(
    ctx: &mut NativeCtx<'_>,
    args: &[otter_vm::Value],
) -> Result<otter_vm::Value, NativeError> {
    use std::io::Write;
    let text = crate::runtime_arg_to_string(args, 0, ctx.heap());
    let mut out = std::io::stdout();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
    Ok(Value::boolean(true))
}

fn stderr_write(
    ctx: &mut NativeCtx<'_>,
    args: &[otter_vm::Value],
) -> Result<otter_vm::Value, NativeError> {
    use std::io::Write;
    let text = crate::runtime_arg_to_string(args, 0, ctx.heap());
    let mut err = std::io::stderr();
    let _ = err.write_all(text.as_bytes());
    let _ = err.flush();
    Ok(Value::boolean(true))
}

/// No-op stream method that returns the receiver, so `stream.on(...).on(...)`
/// and similar chains do not break.
fn stdio_return_this(
    ctx: &mut NativeCtx<'_>,
    _args: &[otter_vm::Value],
) -> Result<otter_vm::Value, NativeError> {
    Ok(*ctx.this_value())
}

fn define_process_method(
    interp: &mut Interpreter,
    process: otter_vm::object::JsObject,
    process_root: &Value,
    name: &'static str,
    length: u8,
    call: NativeCall,
) -> Result<(), OtterError> {
    let value = interp
        .native_function_from_call_host_rooted(name, length, call, &[process_root], &[])
        .map_err(gc_oom_to_error)?;
    let descriptor = otter_vm::object::PropertyDescriptor {
        kind: otter_vm::object::DescriptorKind::Data { value },
        flags: Attr::builtin_function().to_flags(),
    };
    if otter_vm::object::define_own_property(process, interp.gc_heap_mut(), name, descriptor) {
        Ok(())
    } else {
        Err(OtterError::Internal {
            code: DiagnosticCode::GlobalClassBootstrap.as_str().to_string(),
            message: format!("failed to define process.{name}"),
        })
    }
}

pub(crate) fn exit_code(interp: &Interpreter) -> u8 {
    let Some(process) = otter_vm::object::get(*interp.global_this(), interp.gc_heap(), "process")
        .and_then(|v| v.as_object())
    else {
        return 0;
    };
    let Some(value) = otter_vm::object::get(process, interp.gc_heap(), "exitCode") else {
        return 0;
    };
    normalize_exit_code(&value).unwrap_or(0)
}

fn string_value(interp: &mut Interpreter, value: &str) -> Result<otter_vm::Value, OtterError> {
    Ok(otter_vm::Value::string(
        JsString::from_str(value, interp.gc_heap_mut()).map_err(string_oom_to_error)?,
    ))
}

fn cwd_call(cwd: String) -> NativeCall {
    let call: Arc<NativeFn> = Arc::new(move |ctx, _args, _captures| {
        Ok(otter_vm::Value::string(
            JsString::from_str(&cwd, ctx.heap_mut()).map_err(|err| NativeError::TypeError {
                name: "process.cwd",
                reason: err.to_string(),
            })?,
        ))
    });
    NativeCall::Dynamic(call)
}

fn process_exit(
    _ctx: &mut NativeCtx<'_>,
    args: &[otter_vm::Value],
) -> Result<otter_vm::Value, NativeError> {
    let code =
        normalize_exit_code(args.first().unwrap_or(&Value::undefined())).ok_or_else(|| {
            NativeError::TypeError {
                name: "process.exit",
                reason: "exit code must be a finite number between 0 and 255".to_string(),
            }
        })?;
    Err(NativeError::Exit { code })
}

fn process_next_tick(
    ctx: &mut NativeCtx<'_>,
    args: &[otter_vm::Value],
) -> Result<otter_vm::Value, NativeError> {
    let Some(callee) = args.first().cloned() else {
        return Err(NativeError::TypeError {
            name: "process.nextTick",
            reason: "callback is required".to_string(),
        });
    };
    ctx.queue_microtask(callee, args.iter().skip(1).cloned())
        .map_err(|err| match err {
            NativeError::TypeError { reason, .. } => NativeError::TypeError {
                name: "process.nextTick",
                reason,
            },
            other => other,
        })?;
    Ok(Value::undefined())
}

fn uptime_call(start: Instant, base_secs: Option<u64>) -> NativeCall {
    let call: Arc<NativeFn> = Arc::new(move |_ctx, _args, _captures| {
        let seconds = base_secs.map_or(0.0, |secs| secs as f64) + start.elapsed().as_secs_f64();
        Ok(Value::number(NumberValue::from_f64(seconds)))
    });
    NativeCall::Dynamic(call)
}

fn process_memory_usage(
    ctx: &mut NativeCtx<'_>,
    _args: &[otter_vm::Value],
) -> Result<otter_vm::Value, NativeError> {
    let snapshot = runtime_process_snapshot();
    let rss = snapshot.memory_bytes.unwrap_or(0) as f64;
    let heap_used = ctx.interp_mut().gc_heap_mut().gc_stats().live_bytes as f64;
    let heap_total = heap_used;
    let object = ctx.alloc_object()?;
    let interp = ctx.interp_mut();
    set_number_property(interp, object, "rss", rss);
    set_number_property(interp, object, "heapTotal", heap_total);
    set_number_property(interp, object, "heapUsed", heap_used);
    set_number_property(interp, object, "external", 0.0);
    set_number_property(interp, object, "arrayBuffers", 0.0);
    Ok(Value::object(object))
}

/// Resolve `%Function.prototype%` through the realm's `Function`
/// constructor on `globalThis`. The constructor is a `NativeFunction`
/// (its `prototype` lives in the native own-property table, not on a
/// backing `JsObject`), so read it through the descriptor; a plain
/// `JsObject` constructor is also handled. Returns `None` only if the
/// global graph has not been bootstrapped.
fn function_prototype_object(interp: &mut Interpreter) -> Option<otter_vm::object::JsObject> {
    let global = *interp.global_this();
    let function_ctor = otter_vm::object::get(global, interp.gc_heap(), "Function")?;
    if let Some(native) = function_ctor.as_native_function() {
        return native
            .own_property_descriptor(interp.gc_heap_mut(), "prototype")
            .ok()
            .flatten()
            .and_then(|desc| match desc.kind {
                otter_vm::object::DescriptorKind::Data { value } => value.as_object(),
                _ => None,
            });
    }
    let ctor = function_ctor.as_object()?;
    otter_vm::object::get(ctor, interp.gc_heap(), "prototype")?.as_object()
}

fn hrtime_value(interp: &mut Interpreter, start: Instant) -> Result<Value, otter_gc::OutOfMemory> {
    let function =
        interp.native_function_from_call_host_rooted("hrtime", 1, hrtime_call(start), &[], &[])?;
    let bigint = interp.native_function_from_call_host_rooted(
        "bigint",
        0,
        hrtime_bigint_call(start),
        &[&function],
        &[],
    )?;
    let object = interp.alloc_host_object_with_roots(&[&function, &bigint], &[])?;
    otter_vm::object::set_call_native(object, interp.gc_heap_mut(), function);
    otter_vm::object::set(object, interp.gc_heap_mut(), "bigint", bigint);
    // `process.hrtime` is a callable host object (so it can carry the
    // `.bigint` own property), but a host object defaults to a null
    // `[[Prototype]]`. A callable with no `%Function.prototype%` in its
    // chain has no `toString` / `valueOf`, so `String(process.hrtime)`
    // (and any `ToPrimitive`) throws instead of yielding the native
    // form. Re-seat it on `%Function.prototype%` to match an ordinary
    // function object.
    if let Some(function_prototype) = function_prototype_object(interp) {
        otter_vm::object::set_prototype(object, interp.gc_heap_mut(), Some(function_prototype));
    }
    Ok(Value::object(object))
}

fn hrtime_call(start: Instant) -> NativeCall {
    let call: Arc<NativeFn> = Arc::new(move |ctx, args, _captures| {
        let elapsed = start.elapsed();
        let mut seconds = elapsed.as_secs() as i64;
        let mut nanos = elapsed.subsec_nanos() as i64;
        if let Some(previous) = args.first().and_then(|v| v.as_array()) {
            let heap = ctx.heap_mut();
            let prev_seconds = number_to_i64(&otter_vm::array::get(previous, heap, 0));
            let prev_nanos = number_to_i64(&otter_vm::array::get(previous, heap, 1));
            if let (Some(prev_seconds), Some(prev_nanos)) = (prev_seconds, prev_nanos) {
                seconds -= prev_seconds;
                nanos -= prev_nanos;
                if nanos < 0 {
                    seconds -= 1;
                    nanos += 1_000_000_000;
                }
            }
        }
        let values = [
            Value::number(NumberValue::from_f64(seconds.max(0) as f64)),
            Value::number(NumberValue::from_f64(nanos.max(0) as f64)),
        ];
        let array = ctx.array_from_elements_with_roots(values, &[], &[args])?;
        Ok(Value::array(array))
    });
    NativeCall::Dynamic(call)
}

fn hrtime_bigint_call(start: Instant) -> NativeCall {
    let call: Arc<NativeFn> = Arc::new(move |ctx, _args, _captures| {
        let nanos = start.elapsed().as_nanos().min(i128::MAX as u128) as i128;
        let handle =
            otter_vm::bigint::BigIntValue::from_i128(ctx.interp_mut().gc_heap_mut(), nanos)
                .map_err(|e| otter_vm::NativeError::TypeError {
                    name: "process.hrtime.bigint",
                    reason: format!(
                        "out of memory: {} bytes requested, limit {}",
                        e.requested_bytes(),
                        e.heap_limit_bytes(),
                    ),
                })?;
        Ok(Value::big_int(handle))
    });
    NativeCall::Dynamic(call)
}

fn normalize_exit_code(value: &Value) -> Option<u8> {
    if value.is_undefined() {
        return Some(0);
    }
    match value.as_number()? {
        NumberValue::Smi(n) => Some(n.clamp(0, 255) as u8),
        NumberValue::Double(n) if n.is_finite() => Some((n as i32).clamp(0, 255) as u8),
        _ => None,
    }
}

fn set_number_property(
    interp: &mut Interpreter,
    object: otter_vm::JsObject,
    name: &str,
    value: f64,
) {
    otter_vm::object::set(
        object,
        interp.gc_heap_mut(),
        name,
        Value::number(NumberValue::from_f64(value)),
    );
}

fn number_to_i64(value: &Value) -> Option<i64> {
    match value.as_number()? {
        NumberValue::Smi(n) => Some(i64::from(n)),
        NumberValue::Double(n) if n.is_finite() => Some(n as i64),
        _ => None,
    }
}

fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
}

#[derive(Debug, Clone)]
struct RuntimeProcessSnapshot {
    pid: u32,
    ppid: Option<u32>,
    exec_path: String,
    run_time_secs: Option<u64>,
    memory_bytes: Option<u64>,
}

fn runtime_process_snapshot() -> RuntimeProcessSnapshot {
    let fallback_pid = std::process::id();
    let fallback_exec_path = std::env::current_exe()
        .ok()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| "otter".to_string());
    let Ok(pid) = sysinfo::get_current_pid() else {
        return RuntimeProcessSnapshot {
            pid: fallback_pid,
            ppid: None,
            exec_path: fallback_exec_path,
            run_time_secs: None,
            memory_bytes: None,
        };
    };

    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    let Some(process) = system.process(pid) else {
        return RuntimeProcessSnapshot {
            pid: fallback_pid,
            ppid: None,
            exec_path: fallback_exec_path,
            run_time_secs: None,
            memory_bytes: None,
        };
    };

    RuntimeProcessSnapshot {
        pid: pid.as_u32(),
        ppid: process.parent().map(|pid| pid.as_u32()),
        exec_path: process
            .exe()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or(fallback_exec_path),
        run_time_secs: Some(process.run_time()),
        memory_bytes: Some(process.memory()),
    }
}

fn pid_to_i32(pid: u32) -> i32 {
    pid.min(i32::MAX as u32) as i32
}

#[cfg(test)]
mod tests {
    use crate::{CapabilitySet, Otter};

    #[test]
    fn process_argv_uses_configured_snapshot() {
        let otter = Otter::builder()
            .process_argv(["otter", "entry.ts", "alpha"])
            .build()
            .unwrap();
        let result = otter
            .blocking_run_script("process.argv[1] + ':' + process.argv[2]")
            .unwrap();
        assert_eq!(result.completion_string(), "entry.ts:alpha");
    }

    #[test]
    fn process_cwd_uses_configured_snapshot() {
        let otter = Otter::builder()
            .process_cwd("/tmp/otter-app")
            .build()
            .unwrap();
        let result = otter.blocking_run_script("process.cwd()").unwrap();
        assert_eq!(result.completion_string(), "/tmp/otter-app");
    }

    #[test]
    fn process_env_is_deny_by_default() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_script("typeof process.env.PATH")
            .unwrap();
        assert_eq!(result.completion_string(), "undefined");
    }

    #[test]
    fn process_env_respects_allow_env_and_secret_denylist() {
        if std::env::var_os("PATH").is_none() {
            return;
        }
        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        let result = otter
            .blocking_run_script(
                "typeof process.env.PATH + ':' + typeof process.env.OPENAI_API_KEY",
            )
            .unwrap();
        assert_eq!(result.completion_string(), "string:undefined");
    }

    #[test]
    fn process_minimum_node_shape_is_available() {
        let otter = Otter::builder()
            .process_argv(["custom-otter", "entry.ts"])
            .build()
            .unwrap();
        let result = otter
            .blocking_run_script(
                r#"
[
  typeof process.cwd(),
  process.argv0,
  typeof process.execPath,
  process.execArgv.length,
  typeof process.platform,
  typeof process.arch,
  typeof process.pid,
  typeof process.ppid,
  process.version[0],
  typeof process.versions.otter,
  typeof process.versions.node,
  process.release.name,
  typeof process.exitCode,
  typeof process.nextTick,
  typeof process.uptime,
  typeof process.memoryUsage,
  typeof process.hrtime,
  typeof process.hrtime.bigint
].join(":")
"#,
            )
            .unwrap();
        assert_eq!(
            result.completion_string(),
            "string:custom-otter:string:0:string:string:number:number:v:string:string:node:undefined:function:function:function:function:function"
        );
    }

    #[test]
    fn process_runtime_info_methods_are_available() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_script(
                r#"
const memory = process.memoryUsage();
const hrtime = process.hrtime();
[
  typeof process.uptime(),
  typeof memory.rss,
  typeof memory.heapTotal,
  typeof memory.heapUsed,
  typeof memory.external,
  typeof memory.arrayBuffers,
  hrtime.length,
  typeof hrtime[0],
  typeof hrtime[1],
  typeof process.hrtime.bigint()
].join(":")
"#,
            )
            .unwrap();
        assert_eq!(
            result.completion_string(),
            "number:number:number:number:number:number:2:number:number:bigint"
        );
    }

    #[test]
    fn process_hrtime_accepts_previous_tuple() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_script(
                r#"
const previous = process.hrtime();
const diff = process.hrtime(previous);
[diff.length, typeof diff[0], typeof diff[1]].join(":")
"#,
            )
            .unwrap();
        assert_eq!(result.completion_string(), "2:number:number");
    }

    #[test]
    fn process_exit_stops_execution_and_sets_result_code() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_script("process.exit(7); throw new Error('after exit');")
            .unwrap();
        assert_eq!(result.completion_string(), "undefined");
        assert_eq!(result.exit_code(), 7);
    }

    #[test]
    fn process_exit_is_not_catchable_js_throw() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_script("try { process.exit(7); } catch (e) { process.exitCode = 1; }")
            .unwrap();
        assert_eq!(result.exit_code(), 7);
    }

    #[test]
    fn process_exit_code_property_sets_result_code_after_completion() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_script("process.exitCode = 9; 42;")
            .unwrap();
        assert_eq!(result.completion_string(), "42");
        assert_eq!(result.exit_code(), 9);
    }

    #[test]
    fn process_next_tick_runs_at_microtask_checkpoint() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_script("process.nextTick(() => { process.exitCode = 6; }); 1;")
            .unwrap();
        assert_eq!(result.completion_string(), "1");
        assert_eq!(result.exit_code(), 6);
    }

    #[test]
    fn process_next_tick_forwards_arguments() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_script("process.nextTick((a, b) => { process.exitCode = a + b; }, 2, 3);")
            .unwrap();
        assert_eq!(result.exit_code(), 5);
    }

    #[test]
    fn process_next_tick_exit_stops_checkpoint() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_script(
                "process.nextTick(() => process.exit(11)); process.nextTick(() => process.exitCode = 1);",
            )
            .unwrap();
        assert_eq!(result.exit_code(), 11);
    }
}
