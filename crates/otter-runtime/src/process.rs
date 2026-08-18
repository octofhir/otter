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
    Attr, Interpreter, JsString, Local, NativeCall, NativeCtx, NativeError, NativeFn, NativeScope,
    NumberValue, Value,
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
    process_env_overlay: &std::collections::BTreeMap<String, String>,
    capabilities: &CapabilitySet,
    hooks: &RuntimeHooks,
    runtime_task_spawner: Option<&crate::RuntimeTaskSpawner>,
) -> Result<(), OtterError> {
    // Claimed before `process.env` is built, so the variable naming the
    // channel is gone from it — a process this one launches must not believe
    // it inherited the same channel.
    let channel = inherited_channel(runtime_task_spawner);
    let snapshot = runtime_process_snapshot();
    let uptime_base_secs = snapshot.run_time_secs;
    let start = Instant::now();
    let function_prototype = function_prototype_object(interp);
    let global_object = *interp.global_this();
    let process_tag_symbol = interp
        .well_known_symbols()
        .get(otter_vm::symbol::WellKnown::ToStringTag);
    let result: Result<(), NativeError> = NativeCtx::with_host_context(
        interp,
        otter_vm::NativeCallInfo::default_call(),
        None,
        |ctx| {
            ctx.scope(|mut scope| {
                // Park the realm values before the first allocation. Bootstrap
                // allocates enough young objects to scavenge under GC stress;
                // retaining raw handles here would otherwise leave the final
                // global write, @@toStringTag key, or hrtime prototype stale.
                let process_tag_symbol = scope.value(Value::symbol(process_tag_symbol));
                let global_object = scope.value(Value::object(global_object));
                let function_prototype =
                    function_prototype.map(|prototype| scope.value(Value::object(prototype)));

                let process = scope.bare_object()?;

                let process_tag = scope.string("process")?;
                scope.define_symbol(
                    process,
                    process_tag_symbol,
                    process_tag,
                    Attr {
                        writable: false,
                        enumerable: false,
                        configurable: true,
                    }
                    .to_flags(),
                )?;

                let argv = scope.array(process_argv.len())?;
                for (index, arg) in process_argv.iter().enumerate() {
                    // Node resolves `argv[0]` to `process.execPath`; the raw
                    // spawn-time first argument survives only as `argv0`.
                    let arg = if index == 0 {
                        scope.string(&snapshot.exec_path)?
                    } else {
                        scope.string(arg)?
                    };
                    scope.set_index(argv, index, arg)?;
                }
                scope.set(process, "argv", argv)?;
                let exec_argv = scope.array(0)?;
                scope.set(process, "execArgv", exec_argv)?;

                for (name, value) in [
                    (
                        "argv0",
                        process_argv.first().map(String::as_str).unwrap_or("otter"),
                    ),
                    ("execPath", snapshot.exec_path.as_str()),
                    // Node's default title is the spawn path; a plain writable
                    // property is enough until a real setproctitle lands.
                    ("title", snapshot.exec_path.as_str()),
                    ("platform", node_platform()),
                    ("arch", node_arch()),
                    ("version", concat!("v", env!("CARGO_PKG_VERSION"))),
                ] {
                    let value = scope.string(value)?;
                    scope.set(process, name, value)?;
                }

                let versions = scope.bare_object()?;
                for (name, value) in [
                    ("otter", env!("CARGO_PKG_VERSION")),
                    ("node", env!("CARGO_PKG_VERSION")),
                    ("openssl", "3.0.0"),
                    ("v8", "12.0.0"),
                ] {
                    let value = scope.string(value)?;
                    scope.set(versions, name, value)?;
                }
                scope.set(process, "versions", versions)?;

                let release = scope.bare_object()?;
                let release_name = scope.string("node")?;
                scope.set(release, "name", release_name)?;
                scope.set(process, "release", release)?;

                let pid = scope.number(f64::from(pid_to_i32(snapshot.pid)));
                scope.set(process, "pid", pid)?;
                let ppid = scope.number(f64::from(pid_to_i32(snapshot.ppid.unwrap_or(0))));
                scope.set(process, "ppid", ppid)?;
                // `exitCode` is an accessor with Node's validation; the value
                // itself lives in a hidden slot so native readers can reach
                // it without running the getter.
                let undefined = scope.undefined();
                scope.define(
                    process,
                    EXIT_CODE_SLOT,
                    undefined,
                    Attr {
                        writable: true,
                        enumerable: false,
                        configurable: false,
                    }
                    .to_flags(),
                )?;
                let getter =
                    scope.native_call("exitCode", 0, NativeCall::Static(exit_code_getter))?;
                let setter =
                    scope.native_call("exitCode", 1, NativeCall::Static(exit_code_setter))?;
                scope.define_accessor(
                    process,
                    "exitCode",
                    getter,
                    setter,
                    otter_vm::object::PropertyFlags::new(false, true, false),
                )?;

                let env = crate::process_env::build(
                    &mut scope,
                    process_env_overlay,
                    capabilities,
                    hooks,
                )?;
                scope.set(process, "env", env)?;
                let allowed_flags = crate::process_flags::build(&mut scope)?;
                scope.set(process, "allowedNodeEnvironmentFlags", allowed_flags)?;

                let working_directory =
                    crate::process_control::WorkingDirectory::new(process_cwd.to_path_buf());
                for (name, length, call) in [
                    ("cwd", 0, cwd_call(working_directory.clone())),
                    ("exit", 1, NativeCall::Static(process_exit)),
                    ("reallyExit", 1, NativeCall::Static(process_really_exit)),
                    ("_rawDebug", 0, NativeCall::Static(process_raw_debug)),
                    (
                        "setSourceMapsEnabled",
                        1,
                        NativeCall::Static(process_set_source_maps_enabled),
                    ),
                    ("nextTick", 1, NativeCall::Static(process_next_tick)),
                    ("binding", 1, NativeCall::Static(process_binding)),
                    ("uptime", 0, uptime_call(start, uptime_base_secs)),
                    ("cpuUsage", 1, NativeCall::Static(process_cpu_usage)),
                    (
                        "threadCpuUsage",
                        1,
                        NativeCall::Static(process_thread_cpu_usage),
                    ),
                    (
                        "loadEnvFile",
                        1,
                        load_env_file_call(working_directory.clone()),
                    ),
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
                    define_process_method(&mut scope, process, name, length, call)?;
                }
                if let Some(channel) = &channel {
                    crate::process_ipc::install(&mut scope, process, channel)?;
                }
                let hrtime = hrtime_value(&mut scope, start, function_prototype)?;
                scope.set(process, "hrtime", hrtime)?;
                install_stdio_streams(&mut scope, process)?;
                define_process_method(
                    &mut scope,
                    process,
                    "umask",
                    1,
                    NativeCall::Static(process_umask),
                )?;
                // Host-only exit hook: hidden from enumeration so `process`
                // keeps Node's own key surface.
                let emit_exit = scope.native_call(
                    "__otterEmitExit",
                    1,
                    NativeCall::Static(process_emit_exit),
                )?;
                scope.define(
                    process,
                    "__otterEmitExit",
                    emit_exit,
                    Attr {
                        writable: false,
                        enumerable: false,
                        configurable: false,
                    }
                    .to_flags(),
                )?;

                crate::process_control::install(
                    &mut scope,
                    process,
                    capabilities,
                    &working_directory,
                )?;
                crate::process_events::install(&mut scope, process)?;

                let config = scope.bare_object()?;
                let variables = scope.bare_object()?;
                let disabled = scope.boolean(false);
                scope.set(variables, "v8_enable_i18n_support", disabled)?;
                scope.define(
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
                scope.set(process, "config", config)?;

                let features = scope.bare_object()?;
                for (name, on) in [
                    ("inspector", false),
                    ("quic", false),
                    ("tls", false),
                    ("debug", false),
                    ("uv", true),
                    ("ipv6", true),
                    ("dtls", false),
                    ("openssl_is_boringssl", false),
                    ("tls_alpn", false),
                    ("tls_sni", false),
                    ("tls_ocsp", false),
                    ("cached_builtins", false),
                    ("require_module", true),
                    ("typescript", false),
                ] {
                    let value = scope.boolean(on);
                    scope.set(features, name, value)?;
                }
                scope.set(process, "features", features)?;

                scope.define(
                    global_object,
                    "process",
                    process,
                    Attr::global_binding().to_flags(),
                )
            })
        },
    );
    result.map_err(process_bootstrap_error)
}

fn process_bootstrap_error(error: NativeError) -> OtterError {
    OtterError::Internal {
        code: DiagnosticCode::GlobalClassBootstrap.as_str().to_string(),
        message: format!("process bootstrap failed: {error}"),
    }
}

/// `process.umask([mask])` — read or set the process file-creation mask.
/// A missing argument reads the mask without changing it (set to zero, then
/// restore — the only portable read). The argument is a 32-bit unsigned
/// integer or an octal string, exactly Node's `validateMode` contract.
fn process_umask(
    ctx: &mut NativeCtx<'_>,
    args: &[otter_vm::Value],
) -> Result<otter_vm::Value, NativeError> {
    let value = args.first().copied().unwrap_or_else(Value::undefined);
    if value.is_undefined() {
        #[cfg(unix)]
        {
            let current = nix::sys::stat::umask(nix::sys::stat::Mode::empty());
            nix::sys::stat::umask(current);
            return Ok(Value::number_i32(current.bits() as i32));
        }
        #[cfg(not(unix))]
        return Ok(Value::number(NumberValue::from_i32(0)));
    }

    let mask = if let Some(number) = value.as_number() {
        let raw = number.as_f64();
        if raw.fract() != 0.0 || !(0.0..=u32::MAX as f64).contains(&raw) {
            return Err(umask_invalid_value(ctx, value));
        }
        raw as u32
    } else if let Some(string) = value.as_string(ctx.heap()) {
        let text = string.to_lossy_string(ctx.heap());
        if text.is_empty() || !text.bytes().all(|byte| (b'0'..=b'7').contains(&byte)) {
            return Err(umask_invalid_value(ctx, value));
        }
        u32::from_str_radix(&text, 8).map_err(|_| umask_invalid_value(ctx, value))?
    } else {
        return Err(NativeError::Coded {
            kind: otter_vm::ErrorKind::TypeError,
            code: "ERR_INVALID_ARG_TYPE",
            message: format!(
                "The \"mask\" argument must be of type number or string.{}",
                crate::process_control::received_suffix(ctx, value)
            ),
        });
    };

    #[cfg(unix)]
    {
        let previous = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(mask as _));
        Ok(Value::number_i32(previous.bits() as i32))
    }
    #[cfg(not(unix))]
    {
        let _ = mask;
        Ok(Value::number(NumberValue::from_i32(0)))
    }
}

fn umask_invalid_value(ctx: &mut NativeCtx<'_>, value: Value) -> NativeError {
    NativeError::Coded {
        kind: otter_vm::ErrorKind::RangeError,
        code: "ERR_INVALID_ARG_VALUE",
        message: format!(
            "The argument 'mask' must be a 32-bit unsigned integer or an octal string.{}",
            crate::process_control::received_suffix(ctx, value)
        ),
    }
}

/// Install `process.stdout` / `process.stderr` / `process.stdin` as minimal
/// stream-like objects. Many tests gate on `process.stdout.isTTY` (reading a
/// property off `undefined` otherwise throws) and write through
/// `process.stdout.write`; the EventEmitter-style methods are no-ops that
/// return the stream for chaining.
fn install_stdio_streams(
    scope: &mut NativeScope<'_, '_>,
    process: Local<'_>,
) -> Result<(), NativeError> {
    install_one_stdio(
        scope,
        process,
        "stdout",
        1,
        false,
        NativeCall::Static(stdout_write),
    )?;
    install_one_stdio(
        scope,
        process,
        "stderr",
        2,
        false,
        NativeCall::Static(stderr_write),
    )?;
    install_one_stdio(
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
    scope: &mut NativeScope<'_, '_>,
    process: Local<'_>,
    name: &'static str,
    fd: i32,
    readable: bool,
    write_call: NativeCall,
) -> Result<(), NativeError> {
    let stream = scope.bare_object()?;
    for (key, value) in [
        ("isTTY", scope.boolean(false)),
        ("fd", scope.number(f64::from(fd))),
        ("writable", scope.boolean(!readable)),
        ("readable", scope.boolean(readable)),
        ("columns", scope.number(80.0)),
        ("rows", scope.number(24.0)),
    ] {
        scope.set(stream, key, value)?;
    }

    define_method_on(scope, stream, "write", 1, write_call)?;
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
            scope,
            stream,
            method,
            0,
            NativeCall::Static(stdio_return_this),
        )?;
    }
    scope.set(process, name, stream)
}

fn define_method_on(
    scope: &mut NativeScope<'_, '_>,
    target: Local<'_>,
    name: &'static str,
    length: u8,
    call: NativeCall,
) -> Result<(), NativeError> {
    let value = scope.native_call(name, length, call)?;
    scope.define(target, name, value, Attr::builtin_function().to_flags())
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

pub(crate) fn define_process_method(
    scope: &mut NativeScope<'_, '_>,
    process: Local<'_>,
    name: &'static str,
    length: u8,
    call: NativeCall,
) -> Result<(), NativeError> {
    define_method_on(scope, process, name, length, call)
}

pub(crate) fn exit_code(interp: &Interpreter) -> u8 {
    let Some(process) = otter_vm::object::get(*interp.global_this(), interp.gc_heap(), "process")
        .and_then(|v| v.as_object())
    else {
        return 0;
    };
    // The accessor's backing slot — `object::get` cannot run the getter.
    let Some(value) = otter_vm::object::get(process, interp.gc_heap(), EXIT_CODE_SLOT) else {
        return 0;
    };
    normalize_exit_code(&value, interp.gc_heap()).unwrap_or(0)
}

/// Patch the per-run values a restored `process` object carries from
/// its donor: argv / argv0 / execPath, env (this process's variables
/// under the current capability policy), and pid / ppid. Everything
/// else on `process` is build-static and correct as restored; `cwd()`
/// and the clocks are dynamic natives the restore resolver already
/// re-created against the current configuration.
pub(crate) fn reattach_after_restore(
    interp: &mut Interpreter,
    process_argv: &[String],
    process_env_overlay: &std::collections::BTreeMap<String, String>,
    capabilities: &CapabilitySet,
    hooks: &RuntimeHooks,
    working_directory: &crate::process_control::WorkingDirectory,
    runtime_task_spawner: Option<crate::RuntimeTaskSpawner>,
) -> Result<(), OtterError> {
    // A channel belongs to this launch, not to the donor the snapshot was
    // taken from, so it is claimed here for the same reason `env` is rebuilt.
    let channel = inherited_channel(runtime_task_spawner.as_ref());
    let snapshot = runtime_process_snapshot();
    let global_object = *interp.global_this();
    let result: Result<(), NativeError> = NativeCtx::with_host_context(
        interp,
        otter_vm::NativeCallInfo::default_call(),
        None,
        |ctx| {
            ctx.scope(|mut scope| {
                let global_object = scope.value(Value::object(global_object));
                let process = scope.get(global_object, "process")?;
                if !scope.is_object(process) {
                    return Ok(());
                }

                let argv = scope.array(process_argv.len())?;
                for (index, arg) in process_argv.iter().enumerate() {
                    let arg = scope.string(arg)?;
                    scope.set_index(argv, index, arg)?;
                }
                scope.set(process, "argv", argv)?;

                for (name, value) in [
                    (
                        "argv0",
                        process_argv.first().map(String::as_str).unwrap_or("otter"),
                    ),
                    ("execPath", snapshot.exec_path.as_str()),
                ] {
                    let value = scope.string(value)?;
                    scope.set(process, name, value)?;
                }

                let pid = scope.number(f64::from(pid_to_i32(snapshot.pid)));
                scope.set(process, "pid", pid)?;
                let ppid = scope.number(f64::from(pid_to_i32(snapshot.ppid.unwrap_or(0))));
                scope.set(process, "ppid", ppid)?;

                let env = crate::process_env::build(
                    &mut scope,
                    process_env_overlay,
                    capabilities,
                    hooks,
                )?;
                scope.set(process, "env", env)?;
                // `chdir` carries this process's capabilities and shares the
                // restored `cwd` closure's cell, so it is rebuilt here rather
                // than restored from the donor.
                crate::process_control::install(
                    &mut scope,
                    process,
                    capabilities,
                    working_directory,
                )?;
                if let Some(channel) = &channel {
                    crate::process_ipc::install(&mut scope, process, channel)?;
                }
                Ok(())
            })
        },
    );
    result.map_err(|err| OtterError::Internal {
        code: crate::DiagnosticCode::IsolateStart.as_str().to_string(),
        message: format!("process reattach failed: {err:?}"),
    })
}

/// Re-create one of this module's dynamic-native closures by its
/// captured display name — the process half of a snapshot-restore
/// resolver. Clock closures start from a fresh `Instant`, which is
/// what a new process's `uptime()`/`hrtime()` should measure anyway;
/// `cwd` reads the restoring configuration, not the captured one.
pub(crate) fn dynamic_native_payload(
    name: &str,
    working_directory: &crate::process_control::WorkingDirectory,
) -> Option<otter_vm::snapshot::DynamicNativePayload> {
    let snapshot = runtime_process_snapshot();
    let call = match name {
        "cwd" => cwd_call(working_directory.clone()),
        "uptime" => uptime_call(Instant::now(), snapshot.run_time_secs),
        "hrtime" => hrtime_call(Instant::now()),
        "bigint" => hrtime_bigint_call(Instant::now()),
        _ => return None,
    };
    match call {
        NativeCall::Dynamic(arc) => Some(otter_vm::snapshot::DynamicNativePayload::Shared(arc)),
        NativeCall::Static(_) | NativeCall::VmIntrinsic(_) => None,
    }
}

/// `process.cwd()` reads the same cell `process.chdir()` writes, so a move is
/// visible to the very next call.
fn cwd_call(cwd: crate::process_control::WorkingDirectory) -> NativeCall {
    let call: Arc<NativeFn> = Arc::new(move |ctx, _args, _captures| {
        let current = cwd.get();
        Ok(otter_vm::Value::string(
            JsString::from_str(&current.to_string_lossy(), ctx.heap_mut()).map_err(|err| {
                NativeError::TypeError {
                    name: "process.cwd",
                    reason: err.to_string(),
                }
            })?,
        ))
    });
    NativeCall::Dynamic(call)
}

/// `process.exit([code])` — record the code on `exitCode` and dispatch
/// through `this.reallyExit`, which is the documented seam a test replaces to
/// observe an exit without performing one. When `reallyExit` has been
/// replaced, the call returns and execution continues, exactly as in Node.
fn process_exit(
    ctx: &mut NativeCtx<'_>,
    args: &[otter_vm::Value],
) -> Result<otter_vm::Value, NativeError> {
    let value = args.first().copied().unwrap_or_else(Value::undefined);
    let code = normalize_exit_code(&value, ctx.heap()).ok_or_else(|| NativeError::TypeError {
        name: "process.exit",
        reason: "exit code must be a finite number between 0 and 255".to_string(),
    })?;
    let this_value = *ctx.this_value();
    ctx.scope(|mut scope| {
        let process = scope.value(this_value);
        let code_value = scope.number(f64::from(code));
        scope.set(process, EXIT_CODE_SLOT, code_value)?;
        let really_exit = scope.get(process, "reallyExit")?;
        if scope.is_callable(really_exit) {
            let code_value = scope.number(f64::from(code));
            scope.call(really_exit, process, &[code_value])?;
            return Ok(Value::undefined());
        }
        Err(NativeError::Exit { code })
    })
}

/// `process._rawDebug(...)` — write the rendered arguments straight to
/// stderr with a newline, bypassing every stream layer. The harness uses it
/// to report from contexts where `console.error` is hijacked.
fn process_raw_debug(
    ctx: &mut NativeCtx<'_>,
    args: &[otter_vm::Value],
) -> Result<otter_vm::Value, NativeError> {
    use std::io::Write;
    let rendered: Vec<String> = args
        .iter()
        .map(|value| value.display_string(ctx.heap()))
        .collect();
    // `util.format`'s placeholder core: consume arguments for %s/%d/%i/%j,
    // append the leftovers space-separated.
    let mut out = String::new();
    let mut next = 1;
    if let Some(first) = rendered.first() {
        let mut chars = first.chars().peekable();
        while let Some(character) = chars.next() {
            if character == '%' {
                match chars.peek() {
                    Some('%') => {
                        chars.next();
                        out.push('%');
                        continue;
                    }
                    Some('s' | 'd' | 'i' | 'j' | 'o' | 'O') if next < rendered.len() => {
                        chars.next();
                        out.push_str(&rendered[next]);
                        next += 1;
                        continue;
                    }
                    _ => {}
                }
            }
            out.push(character);
        }
    }
    for rest in &rendered[next.min(rendered.len())..] {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(rest);
    }
    let mut err = std::io::stderr();
    let _ = writeln!(err, "{out}");
    let _ = err.flush();
    Ok(Value::undefined())
}

/// `process.setSourceMapsEnabled(val)` — validate the flag Node's way and
/// record it; source-map support itself is compile-time in this engine, so
/// the setter only stores the observable state.
fn process_set_source_maps_enabled(
    ctx: &mut NativeCtx<'_>,
    args: &[otter_vm::Value],
) -> Result<otter_vm::Value, NativeError> {
    let value = args.first().copied().unwrap_or_else(Value::undefined);
    if value.as_boolean().is_none() {
        return Err(NativeError::Coded {
            kind: otter_vm::ErrorKind::TypeError,
            code: "ERR_INVALID_ARG_TYPE",
            message: format!(
                "The \"val\" argument must be of type boolean.{}",
                crate::process_control::received_suffix(ctx, value)
            ),
        });
    }
    let enabled = value.as_boolean().unwrap_or(false);
    let this_value = *ctx.this_value();
    ctx.scope(|mut scope| {
        let process = scope.value(this_value);
        let enabled = scope.boolean(enabled);
        scope.define(
            process,
            "__otter_source_maps_enabled__",
            enabled,
            Attr {
                writable: true,
                enumerable: false,
                configurable: false,
            }
            .to_flags(),
        )?;
        Ok(Value::undefined())
    })
}

/// `process.reallyExit(code)` — the exit syscall seam. The builtin unwinds
/// the run with the code; a test that replaces this property turns
/// `process.exit` into an observable no-op.
fn process_really_exit(
    ctx: &mut NativeCtx<'_>,
    args: &[otter_vm::Value],
) -> Result<otter_vm::Value, NativeError> {
    let value = args.first().copied().unwrap_or_else(Value::undefined);
    let code = normalize_exit_code(&value, ctx.heap()).unwrap_or(0);
    Err(NativeError::Exit { code })
}

/// Slot marking that the `'exit'` event already ran, so a second completion
/// path cannot re-emit it.
const EXIT_EMITTED_SLOT: &str = "__otter_exit_emitted__";

/// Hidden storage behind the `process.exitCode` accessor.
const EXIT_CODE_SLOT: &str = "__otter_exit_code__";

fn exit_code_getter(
    ctx: &mut NativeCtx<'_>,
    _args: &[otter_vm::Value],
) -> Result<otter_vm::Value, NativeError> {
    let this_value = *ctx.this_value();
    ctx.scope(|mut scope| {
        let process = scope.value(this_value);
        let value = scope.get(process, EXIT_CODE_SLOT)?;
        Ok(scope.finish(value))
    })
}

/// `process.exitCode = value` — Node validation: `undefined`/`null` reset,
/// an integer (or integer-shaped string) is stored, everything else throws
/// `ERR_INVALID_ARG_TYPE` / `ERR_OUT_OF_RANGE`.
fn exit_code_setter(
    ctx: &mut NativeCtx<'_>,
    args: &[otter_vm::Value],
) -> Result<otter_vm::Value, NativeError> {
    let value = args.first().copied().unwrap_or_else(Value::undefined);
    if !(value.is_undefined() || value.is_null()) {
        if let Some(string) = value.as_string(ctx.heap()) {
            let text = string.to_lossy_string(ctx.heap());
            if text.trim().is_empty() || text.trim().parse::<i64>().is_err() {
                return Err(NativeError::Coded {
                    kind: otter_vm::ErrorKind::TypeError,
                    code: "ERR_INVALID_ARG_TYPE",
                    message: format!(
                        "The \"code\" argument must be of type number.{}",
                        crate::process_control::received_suffix(ctx, value)
                    ),
                });
            }
        } else if let Some(number) = value.as_number() {
            let raw = number.as_f64();
            if !raw.is_finite() || raw.fract() != 0.0 {
                let rendered = value.display_string(ctx.heap());
                return Err(NativeError::Coded {
                    kind: otter_vm::ErrorKind::RangeError,
                    code: "ERR_OUT_OF_RANGE",
                    message: format!(
                        "The value of \"code\" is out of range. It must be an integer. Received {rendered}"
                    ),
                });
            }
        } else {
            return Err(NativeError::Coded {
                kind: otter_vm::ErrorKind::TypeError,
                code: "ERR_INVALID_ARG_TYPE",
                message: format!(
                    "The \"code\" argument must be of type number.{}",
                    crate::process_control::received_suffix(ctx, value)
                ),
            });
        }
    }
    let this_value = *ctx.this_value();
    ctx.scope(|mut scope| {
        let process = scope.value(this_value);
        let value = scope.value(value);
        scope.set(process, EXIT_CODE_SLOT, value)?;
        Ok(Value::undefined())
    })
}

/// `process.__otterEmitExit(code)` — host hook the embedder calls once when a
/// run completes. Emits the `'exit'` event exactly once with the final code
/// and answers the (possibly listener-updated) exit code. A nested
/// `process.exit(newCode)` inside a listener unwinds through here and the
/// embedder reads the replacement code off that unwind instead.
fn process_emit_exit(
    ctx: &mut NativeCtx<'_>,
    args: &[otter_vm::Value],
) -> Result<otter_vm::Value, NativeError> {
    let value = args.first().copied().unwrap_or_else(Value::undefined);
    let code = normalize_exit_code(&value, ctx.heap()).unwrap_or(0);
    // A failing run (an uncaught exception) stamps its code onto
    // `process.exitCode` the way Node's fatal path does; a clean completion
    // leaves the property exactly as the program left it — `undefined` when
    // it was never assigned.
    let from_failure = args
        .get(1)
        .copied()
        .and_then(|value| value.as_boolean())
        .unwrap_or(false);
    let this_value = *ctx.this_value();
    ctx.scope(|mut scope| {
        let process = scope.value(this_value);
        let emitted = scope.get(process, EXIT_EMITTED_SLOT)?;
        if scope.boolean_value(emitted).unwrap_or(false) {
            return Ok(Value::number_i32(i32::from(code)));
        }
        let flag = scope.boolean(true);
        scope.define(
            process,
            EXIT_EMITTED_SLOT,
            flag,
            Attr {
                writable: true,
                enumerable: false,
                configurable: false,
            }
            .to_flags(),
        )?;
        if from_failure {
            let code_value = scope.number(f64::from(code));
            scope.set(process, EXIT_CODE_SLOT, code_value)?;
        }
        let emit = scope.get(process, "emit")?;
        if scope.is_callable(emit) {
            let event = scope.string("exit")?;
            let code_value = scope.number(f64::from(code));
            scope.call(emit, process, &[event, code_value])?;
        }
        // A listener may have reassigned `process.exitCode`.
        let final_code = scope.get(process, EXIT_CODE_SLOT)?;
        let final_code = scope
            .number_value(final_code)
            .ok()
            .filter(|n| n.is_finite())
            .map(|n| (n as i32).clamp(0, 255) as u8)
            .unwrap_or(code);
        Ok(Value::number_i32(i32::from(final_code)))
    })
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
    ctx.scope(|mut scope| {
        let object = scope.bare_object()?;
        for (name, value) in [
            ("rss", rss),
            ("heapTotal", heap_total),
            ("heapUsed", heap_used),
            ("external", 0.0),
            ("arrayBuffers", 0.0),
        ] {
            let value = scope.number(value);
            scope.set(object, name, value)?;
        }
        Ok(scope.finish(object))
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
                crate::process_control::received_suffix(ctx, name)
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
                        crate::process_control::received_suffix(ctx, *value)
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

    ctx.scope(|mut scope| {
        let result = scope.bare_object()?;
        let user = scope.number(user);
        scope.set(result, "user", user)?;
        let system = scope.number(system);
        scope.set(result, "system", system)?;
        Ok(scope.finish(result))
    })
}

/// `process.threadCpuUsage([previousValue])` — CPU time of the calling
/// thread, split into `user` / `system` microseconds. Linux reads the thread
/// itself (`RUSAGE_THREAD`); platforms without a per-thread getrusage answer
/// the process-wide numbers, which satisfy the same finite-and-monotonic
/// contract the callers rely on.
fn process_thread_cpu_usage(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let previous = match args.first() {
        None => None,
        Some(value) if value.is_undefined() => None,
        Some(value) => {
            let Some(object) = value.as_object() else {
                return Err(NativeError::Coded {
                    kind: otter_vm::ErrorKind::TypeError,
                    code: "ERR_INVALID_ARG_TYPE",
                    message: format!(
                        "The \"prevValue\" argument must be of type object.{}",
                        crate::process_control::received_suffix(ctx, *value)
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

    let (mut user, mut system) = thread_cpu_times_micros();
    if let Some((previous_user, previous_system)) = previous {
        user = (user - previous_user).max(0.0);
        system = (system - previous_system).max(0.0);
    }

    ctx.scope(|mut scope| {
        let result = scope.bare_object()?;
        let user = scope.number(user);
        scope.set(result, "user", user)?;
        let system = scope.number(system);
        scope.set(result, "system", system)?;
        Ok(scope.finish(result))
    })
}

/// `process.loadEnvFile([path])` — read a dotenv file and fold its entries
/// into `process.env`, skipping variables the environment already defines
/// (Node's `--env-file` precedence). The default path is `./.env` under the
/// process working directory.
fn load_env_file_call(cwd: crate::process_control::WorkingDirectory) -> NativeCall {
    let call: Arc<NativeFn> = Arc::new(move |ctx, args, _captures| {
        let path_value = args.first().copied().unwrap_or_else(Value::undefined);
        // Node reports the path exactly as the caller supplied it (the bare
        // `.env` default included), while opening resolves against cwd.
        let (path, reported_path) = if path_value.is_undefined() || path_value.is_null() {
            (cwd.get().join(".env"), ".env".to_string())
        } else if let Some(string) = path_value.as_string(ctx.heap()) {
            let text = string.to_lossy_string(ctx.heap());
            let candidate = PathBuf::from(&text);
            let resolved = if candidate.is_absolute() {
                candidate
            } else {
                cwd.get().join(candidate)
            };
            (resolved, text)
        } else {
            return Err(NativeError::Coded {
                kind: otter_vm::ErrorKind::TypeError,
                code: "ERR_INVALID_ARG_TYPE",
                message: format!(
                    "The \"path\" argument must be of type string.{}",
                    crate::process_control::received_suffix(ctx, path_value)
                ),
            });
        };

        let content = std::fs::read_to_string(&path).map_err(|err| NativeError::Syscall {
            code: match err.kind() {
                std::io::ErrorKind::NotFound => "ENOENT",
                std::io::ErrorKind::PermissionDenied => "EACCES",
                _ => "EIO",
            },
            message: format!("{err}"),
            syscall: "open",
            path: Some(reported_path.clone()),
            dest: None,
            errno: err.raw_os_error().map(|raw| -raw).unwrap_or(-5),
        })?;
        let parsed = parse_env_content(&content);

        let this_value = *ctx.this_value();
        // `process.env` is a Proxy; writes must run through its trap, so the
        // stores go through `Reflect.set` rather than a plain object write.
        let reflect = ctx.global_value("Reflect").ok_or(NativeError::TypeError {
            name: "process.loadEnvFile",
            reason: "Reflect is not available".to_string(),
        })?;
        ctx.scope(|mut scope| {
            let process = scope.value(this_value);
            let env = scope.get(process, "env")?;
            let reflect = scope.value(reflect);
            let reflect_get = scope.get(reflect, "get")?;
            let reflect_set = scope.get(reflect, "set")?;
            for (key, value) in &parsed {
                let key = scope.string(key)?;
                let receiver = scope.undefined();
                let existing = scope.call(reflect_get, receiver, &[env, key])?;
                if !scope.is_undefined(existing) {
                    continue;
                }
                let value = scope.string(value)?;
                let receiver = scope.undefined();
                scope.call(reflect_set, receiver, &[env, key, value])?;
            }
            Ok(Value::undefined())
        })
    });
    NativeCall::Dynamic(call)
}

/// Parse dotenv content with Node's rules: optional `export ` prefix,
/// `#` comment lines, quoted values (`"`, `'`, `` ` ``) running to the
/// matching close quote across newlines, `\n`/`\r` unescaped inside double
/// quotes, and inline `#` comments stripped from unquoted values.
fn parse_env_content(source: &str) -> Vec<(String, String)> {
    let src: Vec<char> = source.chars().collect();
    let n = src.len();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    let find = |from: usize, needle: char| -> Option<usize> {
        src[from.min(n)..]
            .iter()
            .position(|&c| c == needle)
            .map(|p| from + p)
    };
    while i < n {
        while i < n && matches!(src[i], ' ' | '\t' | '\r' | '\n') {
            i += 1;
        }
        if i >= n {
            break;
        }
        let line_end = find(i, '\n').unwrap_or(n);
        if src[i] == '#' {
            i = line_end + 1;
            continue;
        }
        let Some(eq) = find(i, '=').filter(|&eq| eq <= line_end) else {
            i = line_end + 1;
            continue;
        };
        let mut key: String = src[i..eq].iter().collect::<String>().trim().to_string();
        if let Some(stripped) = key.strip_prefix("export ") {
            key = stripped.trim().to_string();
        }
        let mut j = eq + 1;
        while j < n && matches!(src[j], ' ' | '\t') {
            j += 1;
        }
        if j < n && matches!(src[j], '"' | '\'' | '`') {
            let quote = src[j];
            if let Some(close) = find(j + 1, quote) {
                let mut value: String = src[j + 1..close].iter().collect();
                if quote == '"' {
                    value = value.replace("\\n", "\n").replace("\\r", "\r");
                }
                if !key.is_empty() {
                    out.push((key, value));
                }
                i = find(close, '\n').map(|p| p + 1).unwrap_or(n);
                continue;
            }
        }
        let mut value: String = src[j..line_end]
            .iter()
            .collect::<String>()
            .trim()
            .to_string();
        if let Some(hash) = value.find('#') {
            value = value[..hash].trim().to_string();
        }
        if !key.is_empty() {
            out.push((key, value));
        }
        i = line_end + 1;
    }
    out
}

#[cfg(target_os = "linux")]
fn thread_cpu_times_micros() -> (f64, f64) {
    use nix::sys::time::TimeValLike;

    nix::sys::resource::getrusage(nix::sys::resource::UsageWho::RUSAGE_THREAD)
        .map(|usage| {
            (
                usage.user_time().num_microseconds().max(0) as f64,
                usage.system_time().num_microseconds().max(0) as f64,
            )
        })
        .unwrap_or((0.0, 0.0))
}

#[cfg(not(target_os = "linux"))]
fn thread_cpu_times_micros() -> (f64, f64) {
    process_cpu_times_micros()
}

fn cpu_usage_field(ctx: &mut NativeCtx<'_>, name: &str, value: Value) -> Result<f64, NativeError> {
    let Some(value_number) = value.as_number() else {
        return Err(NativeError::Coded {
            kind: otter_vm::ErrorKind::TypeError,
            code: "ERR_INVALID_ARG_TYPE",
            message: format!(
                "The \"prevValue.{name}\" property must be of type number.{}",
                crate::process_control::received_suffix(ctx, value)
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
    scope: &mut NativeScope<'s, '_>,
    start: Instant,
    function_prototype: Option<Local<'_>>,
) -> Result<Local<'s>, NativeError> {
    let function = scope.native_call("hrtime", 1, hrtime_call(start))?;
    let bigint = scope.native_call("bigint", 0, hrtime_bigint_call(start))?;
    let object = scope.bare_object()?;
    scope.set_callable(object, function)?;
    scope.set(object, "bigint", bigint)?;
    // `process.hrtime` is a callable host object (so it can carry the
    // `.bigint` own property), but a host object defaults to a null
    // `[[Prototype]]`. A callable with no `%Function.prototype%` in its
    // chain has no `toString` / `valueOf`, so `String(process.hrtime)`
    // (and any `ToPrimitive`) throws instead of yielding the native
    // form. Re-seat it on `%Function.prototype%` to match an ordinary
    // function object.
    if let Some(function_prototype) = function_prototype {
        scope.set_prototype(object, Some(function_prototype))?;
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
                        crate::process_control::received_suffix(ctx, *argument)
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
        ctx.scope(|mut scope| {
            let value = scope.bigint_i128(nanos)?;
            Ok(scope.finish(value))
        })
    });
    NativeCall::Dynamic(call)
}

fn normalize_exit_code(value: &Value, heap: &otter_gc::GcHeap) -> Option<u8> {
    if value.is_undefined() || value.is_null() {
        return Some(0);
    }
    if let Some(string) = value.as_string(heap) {
        // Node accepts an integer-shaped string ('2' exits with 2); anything
        // else is a validation error.
        let text = string.to_lossy_string(heap);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        let parsed: i64 = trimmed.parse().ok()?;
        return Some(parsed.clamp(0, 255) as u8);
    }
    match value.as_number()? {
        NumberValue::Smi(n) => Some(n.clamp(0, 255) as u8),
        NumberValue::Double(n) if n.is_finite() && n.fract() == 0.0 => {
            Some((n as i64).clamp(0, 255) as u8)
        }
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
    // Node's `process.execPath` is the fully resolved binary path
    // (`fs.realpathSync(process.execPath)` is an identity); a relative or
    // symlinked spawn path must not leak through.
    let fallback_exec_path = std::env::current_exe()
        .ok()
        .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
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
            .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or(fallback_exec_path),
        run_time_secs: Some(process.run_time()),
        memory_bytes: Some(process.memory()),
    }
}

fn pid_to_i32(pid: u32) -> i32 {
    pid.min(i32::MAX as u32) as i32
}

/// Open the channel this process was launched with, if it was launched with
/// one and the host runs an event loop to carry it.
fn inherited_channel(
    runtime_task_spawner: Option<&crate::RuntimeTaskSpawner>,
) -> Option<std::sync::Arc<crate::ipc::IpcChannel>> {
    let spawner = runtime_task_spawner?;
    let address = std::env::var(crate::ipc::CHANNEL_VAR).ok()?;
    crate::ipc::IpcChannel::join(
        Path::new(&address),
        spawner,
        crate::process_ipc::ProcessIpcEvent::new,
    )
    .ok()
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
            "cached_builtins,debug,dtls,inspector,ipv6,openssl_is_boringssl,quic,require_module,tls,tls_alpn,tls_ocsp,tls_sni,typescript,uv"
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
