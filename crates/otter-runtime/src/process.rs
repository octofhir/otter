//! Node-compatible `process` global installed by the runtime.
//!
//! # Contents
//! - [`default_argv`] builds the runtime's default `process.argv` snapshot.
//! - [`default_cwd`] builds the runtime's default `process.cwd()` snapshot.
//! - [`install_global`] materializes the JS-visible `process` object.
//! - [`crate::process_events`] owns EventEmitter and warning behavior.
//! - [`crate::process_flags`] owns the immutable NODE_OPTIONS allowlist.
//!
//! # Invariants
//! - `process.env` is capability-filtered at install time and never bypasses
//!   the runtime's deny-by-default policy or secret denylist.
//! - Host data is copied into JS-owned values. This module does not expose VM
//!   internals across the public runtime boundary.
//! - `process.binding()` is present for Node shape compatibility but remains
//!   deny-by-default; it never exposes Otter or host internals.
//! - Event listeners and warning jobs use scoped handles and JS-owned records.
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

use crate::{CapabilitySet, DiagnosticCode, OtterError, RuntimeHooks};

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
    hooks: &RuntimeHooks,
) -> Result<(), OtterError> {
    let snapshot = runtime_process_snapshot();
    let uptime_base_secs = snapshot.run_time_secs;
    let start = Instant::now();
    let mut ctx = NativeCtx::new_with_call_info_and_context(
        interp,
        otter_vm::NativeCallInfo::default_call(),
        None,
    );
    let result: Result<(), NativeError> = ctx.scope(|ctx, scope| {
        let process = ctx.scoped_object_bare(scope)?;

        let process_tag = ctx.scoped_string(scope, "process")?;
        let tag_sym = ctx
            .interp_mut()
            .well_known_symbols()
            .get(otter_vm::symbol::WellKnown::ToStringTag);
        let tag_sym = ctx.scoped_value(scope, Value::symbol(tag_sym));
        ctx.scoped_define_symbol(
            scope,
            process,
            tag_sym,
            process_tag,
            Attr {
                writable: false,
                enumerable: false,
                configurable: true,
            }
            .to_flags(),
        )?;

        let argv = ctx.scoped_array(scope, process_argv.len())?;
        for (index, arg) in process_argv.iter().enumerate() {
            let arg = ctx.scoped_string(scope, arg)?;
            ctx.scoped_set_index(scope, argv, index, arg)?;
        }
        ctx.scoped_set(scope, process, "argv", argv)?;
        let exec_argv = ctx.scoped_array(scope, 0)?;
        ctx.scoped_set(scope, process, "execArgv", exec_argv)?;

        for (name, value) in [
            (
                "argv0",
                process_argv.first().map(String::as_str).unwrap_or("otter"),
            ),
            ("execPath", snapshot.exec_path.as_str()),
            ("platform", node_platform()),
            ("arch", node_arch()),
            ("version", concat!("v", env!("CARGO_PKG_VERSION"))),
        ] {
            let value = ctx.scoped_string(scope, value)?;
            ctx.scoped_set(scope, process, name, value)?;
        }

        let versions = ctx.scoped_object_bare(scope)?;
        for (name, value) in [
            ("otter", env!("CARGO_PKG_VERSION")),
            ("node", env!("CARGO_PKG_VERSION")),
            ("openssl", "3.0.0"),
            ("v8", "12.0.0"),
        ] {
            let value = ctx.scoped_string(scope, value)?;
            ctx.scoped_set(scope, versions, name, value)?;
        }
        ctx.scoped_set(scope, process, "versions", versions)?;

        let release = ctx.scoped_object_bare(scope)?;
        let release_name = ctx.scoped_string(scope, "node")?;
        ctx.scoped_set(scope, release, "name", release_name)?;
        ctx.scoped_set(scope, process, "release", release)?;

        let pid = ctx.scoped_number(scope, f64::from(pid_to_i32(snapshot.pid)));
        ctx.scoped_set(scope, process, "pid", pid)?;
        let ppid = ctx.scoped_number(scope, f64::from(pid_to_i32(snapshot.ppid.unwrap_or(0))));
        ctx.scoped_set(scope, process, "ppid", ppid)?;
        let undefined = ctx.scoped_undefined(scope);
        ctx.scoped_set(scope, process, "exitCode", undefined)?;

        let env = crate::process_env::build(ctx, scope, capabilities, hooks)?;
        ctx.scoped_set(scope, process, "env", env)?;
        let allowed_flags = crate::process_flags::build(ctx, scope)?;
        ctx.scoped_set(scope, process, "allowedNodeEnvironmentFlags", allowed_flags)?;

        for (name, length, call) in [
            (
                "cwd",
                0,
                cwd_call(process_cwd.to_string_lossy().to_string()),
            ),
            ("exit", 1, NativeCall::Static(process_exit)),
            ("nextTick", 1, NativeCall::Static(process_next_tick)),
            ("binding", 1, NativeCall::Static(process_binding)),
            ("uptime", 0, uptime_call(start, uptime_base_secs)),
            ("cpuUsage", 1, NativeCall::Static(process_cpu_usage)),
            ("memoryUsage", 0, NativeCall::Static(process_memory_usage)),
            (
                "availableMemory",
                0,
                NativeCall::Static(process_available_memory),
            ),
            (
                "constrainedMemory",
                0,
                NativeCall::Static(process_constrained_memory),
            ),
        ] {
            define_process_method(ctx, scope, process, name, length, call)?;
        }
        let hrtime = hrtime_value(ctx, scope, start)?;
        ctx.scoped_set(scope, process, "hrtime", hrtime)?;
        install_stdio_streams(ctx, scope, process)?;
        define_process_method(
            ctx,
            scope,
            process,
            "umask",
            1,
            NativeCall::Static(process_umask),
        )?;

        crate::process_events::install(ctx, scope, process)?;

        let config = ctx.scoped_object_bare(scope)?;
        let variables = ctx.scoped_object_bare(scope)?;
        let disabled = ctx.scoped_boolean(scope, false);
        ctx.scoped_set(scope, variables, "v8_enable_i18n_support", disabled)?;
        ctx.scoped_define_data(
            scope,
            config,
            "variables",
            variables,
            Attr {
                writable: false,
                enumerable: true,
                configurable: false,
            }
            .to_flags(),
        )?;
        ctx.scoped_set(scope, process, "config", config)?;

        let features = ctx.scoped_object_bare(scope)?;
        for (name, on) in [
            ("inspector", false),
            ("quic", false),
            ("tls", false),
            ("debug", false),
            ("uv", true),
            ("ipv6", true),
            ("openssl_is_boringssl", false),
            ("tls_alpn", false),
            ("tls_sni", false),
            ("tls_ocsp", false),
            ("cached_builtins", false),
            ("require_module", true),
            ("typescript", false),
        ] {
            let value = ctx.scoped_boolean(scope, on);
            ctx.scoped_set(scope, features, name, value)?;
        }
        ctx.scoped_set(scope, process, "features", features)?;

        let global_object = *ctx.interp_mut().global_this();
        let global = ctx.scoped_value(scope, Value::object(global_object));
        ctx.scoped_define_data(
            scope,
            global,
            "process",
            process,
            Attr::global_binding().to_flags(),
        )
    });
    result.map_err(process_bootstrap_error)
}

fn process_bootstrap_error(error: NativeError) -> OtterError {
    OtterError::Internal {
        code: DiagnosticCode::GlobalClassBootstrap.as_str().to_string(),
        message: format!("process bootstrap failed: {error}"),
    }
}

/// `process.umask([mask])` — returns the previous mask. Otter does not change
/// the process umask; it reports `0` so harness setup code proceeds.
fn process_umask(
    _ctx: &mut NativeCtx<'_>,
    _args: &[otter_vm::Value],
) -> Result<otter_vm::Value, NativeError> {
    Ok(Value::number(NumberValue::from_i32(0)))
}

/// Install `process.stdout` / `process.stderr` / `process.stdin` as minimal
/// stream-like objects. Many tests gate on `process.stdout.isTTY` (reading a
/// property off `undefined` otherwise throws) and write through
/// `process.stdout.write`; the EventEmitter-style methods are no-ops that
/// return the stream for chaining.
fn install_stdio_streams(
    ctx: &mut NativeCtx<'_>,
    scope: &otter_vm::HandleScope,
    process: otter_vm::Scoped<'_>,
) -> Result<(), NativeError> {
    install_one_stdio(
        ctx,
        scope,
        process,
        "stdout",
        1,
        false,
        NativeCall::Static(stdout_write),
    )?;
    install_one_stdio(
        ctx,
        scope,
        process,
        "stderr",
        2,
        false,
        NativeCall::Static(stderr_write),
    )?;
    install_one_stdio(
        ctx,
        scope,
        process,
        "stdin",
        0,
        true,
        NativeCall::Static(stdio_return_this),
    )?;
    Ok(())
}

fn install_one_stdio(
    ctx: &mut NativeCtx<'_>,
    scope: &otter_vm::HandleScope,
    process: otter_vm::Scoped<'_>,
    name: &'static str,
    fd: i32,
    readable: bool,
    write_call: NativeCall,
) -> Result<(), NativeError> {
    let stream = ctx.scoped_object_bare(scope)?;
    for (key, value) in [
        ("isTTY", ctx.scoped_boolean(scope, false)),
        ("fd", ctx.scoped_number(scope, f64::from(fd))),
        ("writable", ctx.scoped_boolean(scope, !readable)),
        ("readable", ctx.scoped_boolean(scope, readable)),
        ("columns", ctx.scoped_number(scope, 80.0)),
        ("rows", ctx.scoped_number(scope, 24.0)),
    ] {
        ctx.scoped_set(scope, stream, key, value)?;
    }

    define_method_on(ctx, scope, stream, "write", 1, write_call)?;
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
            ctx,
            scope,
            stream,
            method,
            0,
            NativeCall::Static(stdio_return_this),
        )?;
    }
    ctx.scoped_set(scope, process, name, stream)
}

fn define_method_on(
    ctx: &mut NativeCtx<'_>,
    scope: &otter_vm::HandleScope,
    target: otter_vm::Scoped<'_>,
    name: &'static str,
    length: u8,
    call: NativeCall,
) -> Result<(), NativeError> {
    let value = ctx.scoped_native_call(scope, name, length, call)?;
    ctx.scoped_define_data(
        scope,
        target,
        name,
        value,
        Attr::builtin_function().to_flags(),
    )
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
    ctx: &mut NativeCtx<'_>,
    scope: &otter_vm::HandleScope,
    process: otter_vm::Scoped<'_>,
    name: &'static str,
    length: u8,
    call: NativeCall,
) -> Result<(), NativeError> {
    define_method_on(ctx, scope, process, name, length, call)
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
        return Err(NativeError::Coded {
            kind: otter_vm::ErrorKind::TypeError,
            code: "ERR_INVALID_ARG_TYPE",
            message: "The \"callback\" argument must be of type function. Received undefined"
                .to_string(),
        });
    };
    if !callee.is_callable() {
        return Err(NativeError::Coded {
            kind: otter_vm::ErrorKind::TypeError,
            code: "ERR_INVALID_ARG_TYPE",
            message: "The \"callback\" argument must be of type function".to_string(),
        });
    }
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
    ctx.scope(|ctx, scope| {
        let object = ctx.scoped_object_bare(scope)?;
        for (name, value) in [
            ("rss", rss),
            ("heapTotal", heap_total),
            ("heapUsed", heap_used),
            ("external", 0.0),
            ("arrayBuffers", 0.0),
        ] {
            let value = ctx.scoped_number(scope, value);
            ctx.scoped_set(scope, object, name, value)?;
        }
        Ok(ctx.escape(object))
    })
}

fn process_binding(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let name = args.first().copied().unwrap_or_else(Value::undefined);
    if !name.is_string() {
        return Err(NativeError::Coded {
            kind: otter_vm::ErrorKind::TypeError,
            code: "ERR_INVALID_ARG_TYPE",
            message: format!(
                "The \"module\" argument must be of type string.{}",
                invalid_arg_type_suffix(&name, ctx.heap())
            ),
        });
    }
    Err(NativeError::Error {
        message: format!("No such module: {}", name.display_string(ctx.heap())),
    })
}

fn process_cpu_usage(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let previous = match args.first() {
        None => None,
        Some(value) => {
            let Some(object) = value.as_object() else {
                return Err(NativeError::Coded {
                    kind: otter_vm::ErrorKind::TypeError,
                    code: "ERR_INVALID_ARG_TYPE",
                    message: format!(
                        "The \"prevValue\" argument must be of type object.{}",
                        invalid_arg_type_suffix(value, ctx.heap())
                    ),
                });
            };
            let user =
                otter_vm::object::get(object, ctx.heap(), "user").unwrap_or_else(Value::undefined);
            let system = otter_vm::object::get(object, ctx.heap(), "system")
                .unwrap_or_else(Value::undefined);
            let user = cpu_usage_field(ctx, "user", user)?;
            let system = cpu_usage_field(ctx, "system", system)?;
            Some((user, system))
        }
    };

    let (mut user, mut system) = process_cpu_times_micros();
    if let Some((previous_user, previous_system)) = previous {
        user = (user - previous_user).max(0.0);
        system = (system - previous_system).max(0.0);
    }

    ctx.scope(|ctx, scope| {
        let result = ctx.scoped_object_bare(scope)?;
        let user = ctx.scoped_number(scope, user);
        ctx.scoped_set(scope, result, "user", user)?;
        let system = ctx.scoped_number(scope, system);
        ctx.scoped_set(scope, result, "system", system)?;
        Ok(ctx.escape(result))
    })
}

fn cpu_usage_field(ctx: &NativeCtx<'_>, name: &str, value: Value) -> Result<f64, NativeError> {
    let Some(value_number) = value.as_number() else {
        return Err(NativeError::Coded {
            kind: otter_vm::ErrorKind::TypeError,
            code: "ERR_INVALID_ARG_TYPE",
            message: format!(
                "The \"prevValue.{name}\" property must be of type number.{}",
                invalid_arg_type_suffix(&value, ctx.heap())
            ),
        });
    };
    let number = match value_number {
        NumberValue::Smi(value) => f64::from(value),
        NumberValue::Double(value) => value,
    };
    if !number.is_finite() || number < 0.0 {
        return Err(NativeError::Coded {
            kind: otter_vm::ErrorKind::RangeError,
            code: "ERR_INVALID_ARG_VALUE",
            message: format!(
                "The property 'prevValue.{name}' is invalid. Received {}",
                value.display_string(ctx.heap())
            ),
        });
    }
    Ok(number)
}

#[cfg(unix)]
fn process_cpu_times_micros() -> (f64, f64) {
    use nix::sys::time::TimeValLike;

    nix::sys::resource::getrusage(nix::sys::resource::UsageWho::RUSAGE_SELF)
        .map(|usage| {
            (
                usage.user_time().num_microseconds().max(0) as f64,
                usage.system_time().num_microseconds().max(0) as f64,
            )
        })
        .unwrap_or((0.0, 0.0))
}

#[cfg(not(unix))]
fn process_cpu_times_micros() -> (f64, f64) {
    let mut system = System::new();
    system.refresh_processes(
        ProcessesToUpdate::Some(&[sysinfo::get_current_pid().unwrap()]),
        true,
    );
    let total = sysinfo::get_current_pid()
        .ok()
        .and_then(|pid| system.process(pid))
        .map(|process| process.accumulated_cpu_time() as f64 * 1_000.0)
        .unwrap_or(0.0);
    // `sysinfo` exposes a portable total but no portable user/system split.
    (total, 0.0)
}

fn process_available_memory(
    _ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let mut system = System::new();
    system.refresh_memory();
    Ok(Value::number_f64(system.available_memory() as f64))
}

fn process_constrained_memory(
    _ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    // Zero is Node's documented sentinel when no cgroup/job-object limit is
    // visible to the runtime.
    Ok(Value::number_f64(0.0))
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

fn hrtime_value<'s>(
    ctx: &mut NativeCtx<'_>,
    scope: &'s otter_vm::HandleScope,
    start: Instant,
) -> Result<otter_vm::Scoped<'s>, NativeError> {
    let function = ctx.scoped_native_call(scope, "hrtime", 1, hrtime_call(start))?;
    let bigint = ctx.scoped_native_call(scope, "bigint", 0, hrtime_bigint_call(start))?;
    let object = ctx.scoped_object_bare(scope)?;
    ctx.scoped_set_call_native(scope, object, function)?;
    ctx.scoped_set(scope, object, "bigint", bigint)?;
    // `process.hrtime` is a callable host object (so it can carry the
    // `.bigint` own property), but a host object defaults to a null
    // `[[Prototype]]`. A callable with no `%Function.prototype%` in its
    // chain has no `toString` / `valueOf`, so `String(process.hrtime)`
    // (and any `ToPrimitive`) throws instead of yielding the native
    // form. Re-seat it on `%Function.prototype%` to match an ordinary
    // function object.
    if let Some(function_prototype) = function_prototype_object(ctx.interp_mut()) {
        let function_prototype = ctx.scoped_value(scope, Value::object(function_prototype));
        ctx.scoped_set_prototype(scope, object, Some(function_prototype))?;
    }
    Ok(object)
}

fn hrtime_call(start: Instant) -> NativeCall {
    let call: Arc<NativeFn> = Arc::new(move |ctx, args, _captures| {
        let elapsed = start.elapsed();
        let mut seconds = elapsed.as_secs() as i64;
        let mut nanos = elapsed.subsec_nanos() as i64;
        if let Some(argument) = args.first() {
            let Some(previous) = argument.as_array() else {
                return Err(NativeError::Coded {
                    kind: otter_vm::ErrorKind::TypeError,
                    code: "ERR_INVALID_ARG_TYPE",
                    message: format!(
                        "The \"time\" argument must be an instance of Array.{}",
                        invalid_arg_type_suffix(argument, ctx.heap())
                    ),
                });
            };
            let length = otter_vm::array::len(previous, ctx.heap());
            if length != 2 {
                return Err(NativeError::Coded {
                    kind: otter_vm::ErrorKind::RangeError,
                    code: "ERR_OUT_OF_RANGE",
                    message: format!(
                        "The value of \"time\" is out of range. It must be 2. Received {length}"
                    ),
                });
            }
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
        let array = ctx.array_from_elements(values)?;
        Ok(Value::array(array))
    });
    NativeCall::Dynamic(call)
}

fn hrtime_bigint_call(start: Instant) -> NativeCall {
    let call: Arc<NativeFn> = Arc::new(move |ctx, _args, _captures| {
        let nanos = start.elapsed().as_nanos().min(i128::MAX as u128) as i128;
        ctx.scope(|ctx, scope| {
            let value = ctx.scoped_bigint_i128(scope, nanos)?;
            Ok(ctx.escape(value))
        })
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

fn number_to_i64(value: &Value) -> Option<i64> {
    match value.as_number()? {
        NumberValue::Smi(n) => Some(i64::from(n)),
        NumberValue::Double(n) if n.is_finite() => Some(n as i64),
        _ => None,
    }
}

fn invalid_arg_type_suffix(value: &Value, heap: &otter_gc::GcHeap) -> String {
    if value.is_undefined() {
        " Received undefined".to_string()
    } else if value.is_null() {
        " Received null".to_string()
    } else if value.is_string() {
        format!(" Received type string ('{}')", value.display_string(heap))
    } else if value.is_boolean() {
        format!(" Received type boolean ({})", value.display_string(heap))
    } else if value.is_number() {
        format!(" Received type number ({})", value.display_string(heap))
    } else {
        format!(" Received {}", value.display_string(heap))
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
    fn process_env_capability_hook_cannot_bypass_secret_filter() {
        if std::env::var_os("PATH").is_none() {
            return;
        }
        let otter = Otter::builder()
            .capabilities(CapabilitySet::sandbox())
            .capability_hook(
                |_capabilities: &CapabilitySet,
                 capability: crate::RuntimeCapability,
                 _request: &crate::CapabilityRequest<'_>| {
                    capability == crate::RuntimeCapability::Env
                },
            )
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
    fn process_env_coerces_values_and_deletes_properties() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_script(
                r#"
process.env.TEXT = 'value';
process.env.NUMBER = 42;
process.env.BOOLEAN = false;
process.env.MISSING = undefined;
delete process.env.TEXT;
[
  process.env.TEXT,
  process.env.NUMBER,
  process.env.BOOLEAN,
  process.env.MISSING,
  Object.getPrototypeOf(process.env) === Object.prototype
].join(':')
"#,
            )
            .unwrap();
        assert_eq!(result.completion_string(), ":42:false:undefined:true");
    }

    #[test]
    fn process_env_rejects_symbols_and_restricted_descriptors() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_script(
                r#"
const symbol = Symbol('env');
const results = [];
try { process.env[symbol] = 1; } catch (error) { results.push(error.name); }
try { process.env.VALUE = symbol; } catch (error) { results.push(error.name); }
try {
  Object.defineProperty(process.env, 'BAD', { value: 'bad' });
} catch (error) {
  results.push(error.code);
}
Object.defineProperty(process.env, 'GOOD', {
  value: 7,
  configurable: true,
  writable: true,
  enumerable: true
});
results.push(process.env.GOOD, symbol in process.env, delete process.env[symbol]);
results.join(':')
"#,
            )
            .unwrap();
        assert_eq!(
            result.completion_string(),
            "TypeError:TypeError:ERR_INVALID_OBJECT_DEFINE_PROPERTY:7:false:true"
        );
    }

    #[test]
    fn process_allowed_node_environment_flags_is_readonly() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_script(
                r#"
const flags = process.allowedNodeEnvironmentFlags;
const size = flags.size;
flags.add('foo');
Set.prototype.add.call(flags, 'bar');
flags.delete('-r');
Set.prototype.clear.call(flags);
[
  Object.isFrozen(flags),
  flags.size === size,
  flags.has('-r'),
  flags.has('r'),
  flags.has('--perf_basic_prof'),
  flags.has('--stack-trace-limit=100'),
  flags.has('--cheeseburgers')
].join(':')
"#,
            )
            .unwrap();
        assert_eq!(
            result.completion_string(),
            "true:true:true:true:true:true:false"
        );
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
  typeof process.binding,
  typeof process.uptime,
  typeof process.cpuUsage,
  typeof process.memoryUsage,
  typeof process.availableMemory,
  typeof process.constrainedMemory,
  typeof process.hrtime,
  typeof process.hrtime.bigint
].join(":")
"#,
            )
            .unwrap();
        assert_eq!(
            result.completion_string(),
            "string:custom-otter:string:0:string:string:number:number:v:string:string:node:undefined:function:function:function:function:function:function:function:function:function"
        );
    }

    #[test]
    fn process_runtime_info_methods_are_available() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_script(
                r#"
const memory = process.memoryUsage();
const usage = process.cpuUsage();
const diff = process.cpuUsage(usage);
const hrtime = process.hrtime();
[
  typeof process.uptime(),
  typeof usage.user,
  typeof usage.system,
  typeof diff.user,
  typeof diff.system,
  typeof memory.rss,
  typeof memory.heapTotal,
  typeof memory.heapUsed,
  typeof memory.external,
  typeof memory.arrayBuffers,
  typeof process.availableMemory(),
  typeof process.constrainedMemory(),
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
            "number:number:number:number:number:number:number:number:number:number:number:number:2:number:number:bigint"
        );
    }

    #[test]
    fn process_cpu_usage_validates_previous_snapshot() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_script(
                r#"
const errors = [];
for (const value of [
  1,
  {},
  { user: 1, system: null },
  { user: -1, system: 0 },
  { user: 1, system: -1 }
]) {
  try {
    process.cpuUsage(value);
  } catch (error) {
    errors.push(error.name + ':' + error.code);
  }
}
errors.join(',')
"#,
            )
            .unwrap();
        assert_eq!(
            result.completion_string(),
            "TypeError:ERR_INVALID_ARG_TYPE,TypeError:ERR_INVALID_ARG_TYPE,TypeError:ERR_INVALID_ARG_TYPE,RangeError:ERR_INVALID_ARG_VALUE,RangeError:ERR_INVALID_ARG_VALUE"
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
    fn process_hrtime_validates_previous_tuple() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_script(
                r#"
const codes = [];
for (const value of [1, [], [1], [1, 2, 3]]) {
  try {
    process.hrtime(value);
  } catch (error) {
    codes.push(error.name + ':' + error.code);
  }
}
codes.join(',')
"#,
            )
            .unwrap();
        assert_eq!(
            result.completion_string(),
            "TypeError:ERR_INVALID_ARG_TYPE,RangeError:ERR_OUT_OF_RANGE,RangeError:ERR_OUT_OF_RANGE,RangeError:ERR_OUT_OF_RANGE"
        );
    }

    #[test]
    fn process_features_match_the_supported_node_shape() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_script("Object.keys(process.features).sort().join(',')")
            .unwrap();
        assert_eq!(
            result.completion_string(),
            "cached_builtins,debug,inspector,ipv6,openssl_is_boringssl,quic,require_module,tls,tls_alpn,tls_ocsp,tls_sni,typescript,uv"
        );
    }

    #[test]
    fn process_config_variables_cannot_be_replaced() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_script(
                r#"
'use strict';
let errorName;
try {
  process.config.variables = 42;
} catch (error) {
  errorName = error.name;
}
[errorName, typeof process.config.variables].join(':')
"#,
            )
            .unwrap();
        assert_eq!(result.completion_string(), "TypeError:object");
    }

    #[test]
    fn process_binding_is_present_but_does_not_expose_internals() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_script(
                r#"
let message;
try {
  process.binding('test');
} catch (error) {
  message = error.message;
}
message
"#,
            )
            .unwrap();
        assert_eq!(result.completion_string(), "No such module: test");
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

    #[test]
    fn process_event_emitter_preserves_order_once_and_symbols() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_script(
                r#"
const seen = [];
const symbol = Symbol('event');
const removed = () => seen.push('removed');
process.on('data', () => seen.push('tail'));
process.prependOnceListener('data', () => seen.push('once'));
process.on('data', removed);
process.removeListener('data', removed);
process.once(symbol, (value) => seen.push(value));
process.emit('data');
process.emit('data');
process.emit(symbol, 'symbol');
process.emit(symbol, 'again');
[seen.join(','), process.listenerCount('data'), process.eventNames().length,
 process._eventsCount].join(':')
"#,
            )
            .unwrap();
        assert_eq!(result.completion_string(), "once,tail,tail,symbol:1:1:1");
    }

    #[test]
    fn process_emit_warning_is_deferred_and_coded() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_script(
                r#"
let observed = 'pending';
process.emitWarning('careful', {
  type: 'CustomWarning',
  code: 'OTTER001',
  detail: 'detail'
});
process.once('warning', (warning) => {
  observed = [warning.name, warning.message, warning.code, warning.detail].join(':');
});
process.nextTick(() => { process.exitCode = observed ===
  'CustomWarning:careful:OTTER001:detail' ? 0 : 91; });
observed
"#,
            )
            .unwrap();
        assert_eq!(result.completion_string(), "pending");
        assert_eq!(result.exit_code(), 0);
    }
}
