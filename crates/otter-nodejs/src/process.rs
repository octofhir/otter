use std::io::Write;
use std::path::PathBuf;

use otter_macros::{burrow, dive, lodge};
use otter_runtime::{
    ObjectHandle, RegisterValue, RuntimeState, VmNativeCallError, current_capabilities,
    current_process,
};
use otter_vm::microtask::MicrotaskJob;
use otter_vm::payload::{VmTrace, VmValueTracer};

use crate::support::{
    install_method, install_readonly_value, install_value, own_property, string_value,
    throw_type_error_with_code, type_error, value_to_string,
};
use crate::util::util_types_value;

const PROCESS_GLOBAL_SLOT: &str = "__otter_node_process_object";
const PROCESS_STATE_SLOT: &str = "__otter_node_process_state";
const OTTER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone)]
struct ProcessListener {
    event: RegisterValue,
    callback: ObjectHandle,
    once: bool,
}

impl VmTrace for ProcessListener {
    fn trace(&self, tracer: &mut dyn VmValueTracer) {
        self.event.trace(tracer);
        self.callback.trace(tracer);
    }
}

#[derive(Debug, Default, Clone)]
struct NodeProcessState {
    cwd: PathBuf,
    umask: u32,
    listeners: Vec<ProcessListener>,
}

impl VmTrace for NodeProcessState {
    fn trace(&self, tracer: &mut dyn VmValueTracer) {
        for listener in &self.listeners {
            listener.trace(tracer);
        }
    }
}

lodge!(
    process_module,
    module_specifiers = ["node:process", "process"],
    kind = commonjs,
    default = value(process_export_value(runtime)?),
);

pub(crate) fn install_process_global(runtime: &mut RuntimeState) -> Result<(), String> {
    let process = process_export_value(runtime)?;
    runtime.install_global_value("process", process);
    Ok(())
}

pub(crate) fn current_node_cwd(runtime: &mut RuntimeState) -> Option<PathBuf> {
    process_state(runtime).map(|state| state.cwd.clone())
}

fn process_export_value(runtime: &mut RuntimeState) -> Result<RegisterValue, String> {
    ensure_process_object(runtime).map(|handle| RegisterValue::from_object_handle(handle.0))
}

fn ensure_process_object(runtime: &mut RuntimeState) -> Result<ObjectHandle, String> {
    let property = runtime.intern_property_name(PROCESS_GLOBAL_SLOT);
    let global = runtime.intrinsics().global_object();
    if let Ok(value) = runtime.own_property_value(global, property)
        && let Some(handle) = value.as_object_handle().map(ObjectHandle)
    {
        return Ok(handle);
    }

    let object = runtime.alloc_object();
    let members = burrow! {
        fns = [
            process_cwd,
            process_chdir,
            process_next_tick,
            process_umask,
            process_on,
            process_add_listener,
            process_once,
            process_emit,
            process_remove_listener,
            process_off,
            process_remove_all_listeners,
            process_raw_debug,
            process_binding
        ]
    };
    runtime
        .install_burrow(object, &members)
        .map_err(|error| format!("failed to install process methods: {error}"))?;

    let (process, env_store) = current_process(runtime)
        .ok_or_else(|| "host process metadata is not installed".to_string())?;
    let capabilities = current_capabilities(runtime);
    let argv = string_array_value(runtime, &process.argv);
    let exec_argv = string_array_value(runtime, &process.exec_argv);
    let exec_path = string_value(runtime, &process.exec_path);
    let argv0 = string_value(
        runtime,
        process
            .argv
            .first()
            .map(String::as_str)
            .unwrap_or(process.exec_path.as_str()),
    );
    let arch = string_value(runtime, node_arch());
    let platform = string_value(runtime, node_platform());
    let pid = RegisterValue::from_number(f64::from(std::process::id()));
    let version = string_value(runtime, &format!("v{OTTER_VERSION}"));
    let release = release_value(runtime)?;
    let versions = versions_value(runtime)?;
    let env = env_value(runtime, &capabilities, env_store.as_ref())?;
    let features = features_value(runtime)?;
    let config = config_value(runtime)?;
    let stdout = stream_value(runtime, "stdout", false)?;
    let stderr = stream_value(runtime, "stderr", false)?;

    install_value(runtime, object, "argv", argv)?;
    install_value(runtime, object, "execArgv", exec_argv)?;
    install_value(runtime, object, "execPath", exec_path)?;
    install_value(runtime, object, "argv0", argv0)?;
    install_value(runtime, object, "arch", arch)?;
    install_value(runtime, object, "platform", platform)?;
    install_value(runtime, object, "pid", pid)?;
    install_value(runtime, object, "version", version)?;
    install_value(runtime, object, "release", release)?;
    install_value(runtime, object, "versions", versions)?;
    install_value(runtime, object, "env", env)?;
    install_value(runtime, object, "features", features)?;
    install_value(runtime, object, "config", config)?;
    install_value(runtime, object, "stdout", stdout)?;
    install_value(runtime, object, "stderr", stderr)?;
    install_value(runtime, object, "_exiting", RegisterValue::from_bool(false))?;

    let state = runtime.alloc_native_object(NodeProcessState {
        cwd: process.cwd.clone(),
        umask: 0o022,
        listeners: Vec::new(),
    });
    install_readonly_value(
        runtime,
        object,
        PROCESS_STATE_SLOT,
        RegisterValue::from_object_handle(state.0),
    )?;

    runtime.install_global_value(
        PROCESS_GLOBAL_SLOT,
        RegisterValue::from_object_handle(object.0),
    );
    Ok(object)
}

#[dive(name = "cwd", length = 0)]
fn process_cwd(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let _ = process_handle(this, runtime)?;
    let cwd = process_state(runtime)
        .map(|state| state.cwd)
        .ok_or_else(|| VmNativeCallError::Internal("process state is not installed".into()))?;
    Ok(string_value(runtime, cwd.to_string_lossy()))
}

#[dive(name = "chdir", length = 1)]
fn process_chdir(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let _ = process_handle(this, runtime)?;
    let value = args.first().copied().ok_or_else(|| {
        throw_type_error_with_code(
            runtime,
            "The \"directory\" argument must be of type string. Received undefined",
            "ERR_INVALID_ARG_TYPE",
        )
    })?;
    if value.as_object_handle().is_some()
        && value.as_number().is_none()
        && value.as_bool().is_none()
    {
        return Err(throw_type_error_with_code(
            runtime,
            "The \"directory\" argument must be of type string",
            "ERR_INVALID_ARG_TYPE",
        ));
    }
    let target = PathBuf::from(value_to_string(runtime, value));
    let cwd = if target.is_absolute() {
        target
    } else {
        let mut current = process_state(runtime)
            .map(|state| state.cwd)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        current.push(target);
        current
    };
    if !cwd.is_dir() {
        return Err(type_error(
            runtime,
            "process.chdir target must be a directory",
        ));
    }
    with_process_state(runtime, |state| state.cwd = cwd);
    Ok(RegisterValue::undefined())
}

#[dive(name = "nextTick", length = 1)]
fn process_next_tick(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let callback = args
        .first()
        .copied()
        .and_then(|value| value.as_object_handle())
        .map(ObjectHandle)
        .ok_or_else(|| type_error(runtime, "process.nextTick requires a callable argument"))?;
    if !runtime.objects().is_callable(callback) {
        return Err(type_error(
            runtime,
            "process.nextTick requires a callable argument",
        ));
    }

    runtime.microtasks_mut().enqueue_next_tick(MicrotaskJob {
        callback,
        this_value: RegisterValue::undefined(),
        args: args.iter().skip(1).copied().collect(),
    });
    Ok(RegisterValue::undefined())
}

#[dive(name = "umask", length = 1)]
fn process_umask(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let _ = process_handle(this, runtime)?;
    let previous = process_state(runtime)
        .map(|state| state.umask)
        .ok_or_else(|| VmNativeCallError::Internal("process state is not installed".into()))?;
    let Some(value) = args.first().copied() else {
        return Ok(RegisterValue::from_i32(previous as i32));
    };
    if value == RegisterValue::undefined() {
        return Ok(RegisterValue::from_i32(previous as i32));
    }

    let next = if let Some(number) = value.as_number() {
        if number < 0.0 {
            return Err(throw_type_error_with_code(
                runtime,
                "The value of \"mask\" is out of range",
                "ERR_INVALID_ARG_VALUE",
            ));
        }
        number as u32
    } else {
        let rendered = value_to_string(runtime, value);
        if !rendered.chars().all(|ch| ('0'..='7').contains(&ch)) {
            return Err(throw_type_error_with_code(
                runtime,
                &format!("The value of \"mask\" is invalid. Received '{rendered}'"),
                "ERR_INVALID_ARG_VALUE",
            ));
        }
        u32::from_str_radix(&rendered, 8).map_err(|_| {
            throw_type_error_with_code(
                runtime,
                &format!("The value of \"mask\" is invalid. Received '{rendered}'"),
                "ERR_INVALID_ARG_VALUE",
            )
        })?
    };
    with_process_state(runtime, |state| state.umask = next);
    Ok(RegisterValue::from_i32(previous as i32))
}

#[dive(name = "on", length = 2)]
fn process_on(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    add_listener(this, args, runtime, false)
}

#[dive(name = "addListener", length = 2)]
fn process_add_listener(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    add_listener(this, args, runtime, false)
}

#[dive(name = "once", length = 2)]
fn process_once(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    add_listener(this, args, runtime, true)
}

fn add_listener(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
    once: bool,
) -> Result<RegisterValue, VmNativeCallError> {
    let process = process_handle(this, runtime)?;
    let event = args
        .first()
        .copied()
        .ok_or_else(|| type_error(runtime, "process listener event is required"))?;
    let callback = args
        .get(1)
        .copied()
        .and_then(|value| value.as_object_handle())
        .map(ObjectHandle)
        .ok_or_else(|| type_error(runtime, "process listener must be callable"))?;
    if !runtime.objects().is_callable(callback) {
        return Err(type_error(runtime, "process listener must be callable"));
    }
    with_process_state(runtime, |state| {
        state.listeners.push(ProcessListener {
            event,
            callback,
            once,
        });
    });
    Ok(RegisterValue::from_object_handle(process.0))
}

#[dive(name = "emit", length = 1)]
fn process_emit(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let process = process_handle(this, runtime)?;
    let event = args
        .first()
        .copied()
        .ok_or_else(|| type_error(runtime, "process.emit requires an event name"))?;
    let emit_args: Vec<_> = args.iter().skip(1).copied().collect();
    let listeners = process_state(runtime)
        .map(|state| {
            state
                .listeners
                .iter()
                .filter(|listener| listener.event == event)
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    for listener in &listeners {
        runtime.call_callable(
            listener.callback,
            RegisterValue::from_object_handle(process.0),
            &emit_args,
        )?;
    }

    if !listeners.is_empty() {
        with_process_state(runtime, |state| {
            state.listeners.retain(|listener| {
                !(listener.event == event
                    && listener.once
                    && listeners.iter().any(|candidate| {
                        candidate.callback == listener.callback && candidate.once == listener.once
                    }))
            });
        });
    }

    Ok(RegisterValue::from_bool(!listeners.is_empty()))
}

#[dive(name = "removeListener", length = 2)]
fn process_remove_listener(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    remove_listener_impl(this, args, runtime)
}

#[dive(name = "off", length = 2)]
fn process_off(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    remove_listener_impl(this, args, runtime)
}

fn remove_listener_impl(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let process = process_handle(this, runtime)?;
    let event = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let callback = args
        .get(1)
        .and_then(|value| value.as_object_handle())
        .map(ObjectHandle);
    with_process_state(runtime, |state| {
        if let Some(callback) = callback {
            if let Some(index) = state
                .listeners
                .iter()
                .position(|listener| listener.event == event && listener.callback == callback)
            {
                state.listeners.remove(index);
            }
        }
    });
    Ok(RegisterValue::from_object_handle(process.0))
}

#[dive(name = "removeAllListeners", length = 1)]
fn process_remove_all_listeners(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let process = process_handle(this, runtime)?;
    if let Some(event) = args.first().copied() {
        with_process_state(runtime, |state| {
            state.listeners.retain(|listener| listener.event != event);
        });
    } else {
        with_process_state(runtime, |state| state.listeners.clear());
    }
    Ok(RegisterValue::from_object_handle(process.0))
}

#[dive(name = "_rawDebug", length = 0)]
fn process_raw_debug(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let message = args
        .iter()
        .copied()
        .map(|value| value_to_string(runtime, value))
        .collect::<Vec<_>>()
        .join(" ");
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{message}");
    Ok(RegisterValue::undefined())
}

#[dive(name = "binding", length = 1)]
fn process_binding(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let name = args
        .first()
        .copied()
        .map(|value| value_to_string(runtime, value))
        .unwrap_or_default();
    match name.as_str() {
        "util" => {
            util_types_value(runtime).map_err(|error| VmNativeCallError::Internal(error.into()))
        }
        _ => Err(type_error(
            runtime,
            &format!("process.binding('{name}') is not implemented"),
        )),
    }
}

fn env_value(
    runtime: &mut RuntimeState,
    capabilities: &otter_runtime::Capabilities,
    env_store: &otter_runtime::IsolatedEnvStore,
) -> Result<RegisterValue, String> {
    let object = runtime.alloc_object();
    let mut entries: Vec<_> = env_store
        .to_hash_map()
        .into_iter()
        .filter(|(key, _)| capabilities.can_env(key))
        .collect();
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    for (key, value) in entries {
        let value = string_value(runtime, &value);
        install_value(runtime, object, &key, value)?;
    }

    Ok(RegisterValue::from_object_handle(object.0))
}

fn versions_value(runtime: &mut RuntimeState) -> Result<RegisterValue, String> {
    let object = runtime.alloc_object();
    let node = string_value(runtime, OTTER_VERSION);
    let otter = string_value(runtime, OTTER_VERSION);
    install_readonly_value(runtime, object, "node", node)?;
    install_readonly_value(runtime, object, "otter", otter)?;
    Ok(RegisterValue::from_object_handle(object.0))
}

fn release_value(runtime: &mut RuntimeState) -> Result<RegisterValue, String> {
    let object = runtime.alloc_object();
    let name = string_value(runtime, "node");
    install_readonly_value(runtime, object, "name", name)?;
    Ok(RegisterValue::from_object_handle(object.0))
}

fn features_value(runtime: &mut RuntimeState) -> Result<RegisterValue, String> {
    let object = runtime.alloc_object();
    for (name, value) in [
        ("inspector", false),
        ("debug", false),
        ("uv", true),
        ("ipv6", true),
        ("openssl_is_boringssl", false),
        ("tls_alpn", false),
        ("tls_sni", false),
        ("tls_ocsp", false),
        ("tls", false),
        ("cached_builtins", false),
        ("require_module", false),
    ] {
        install_readonly_value(runtime, object, name, RegisterValue::from_bool(value))?;
    }
    let typescript = string_value(runtime, "none");
    install_readonly_value(runtime, object, "typescript", typescript)?;
    Ok(RegisterValue::from_object_handle(object.0))
}

fn config_value(runtime: &mut RuntimeState) -> Result<RegisterValue, String> {
    let config = runtime.alloc_object();
    let variables = runtime.alloc_object();
    install_readonly_value(
        runtime,
        variables,
        "v8_enable_i18n_support",
        RegisterValue::from_i32(0),
    )?;
    install_readonly_value(runtime, variables, "node_quic", RegisterValue::from_i32(0))?;
    install_readonly_value(runtime, variables, "asan", RegisterValue::from_i32(0))?;
    let napi_build_version = string_value(runtime, "9");
    install_readonly_value(runtime, variables, "napi_build_version", napi_build_version)?;
    let builtin_shareables =
        RegisterValue::from_object_handle(runtime.alloc_array_with_elements(&[]).0);
    install_readonly_value(
        runtime,
        variables,
        "node_builtin_shareable_builtins",
        builtin_shareables,
    )?;
    let target_defaults = runtime.alloc_object();
    let default_configuration = string_value(runtime, "Release");
    install_readonly_value(
        runtime,
        target_defaults,
        "default_configuration",
        default_configuration,
    )?;
    install_readonly_value(
        runtime,
        config,
        "variables",
        RegisterValue::from_object_handle(variables.0),
    )?;
    install_readonly_value(
        runtime,
        config,
        "target_defaults",
        RegisterValue::from_object_handle(target_defaults.0),
    )?;
    Ok(RegisterValue::from_object_handle(config.0))
}

fn stream_value(
    runtime: &mut RuntimeState,
    name: &str,
    is_tty: bool,
) -> Result<RegisterValue, String> {
    let object = runtime.alloc_object();
    install_readonly_value(runtime, object, "isTTY", RegisterValue::from_bool(is_tty))?;
    let callback = match name {
        "stdout" => process_stdout_write,
        _ => process_stderr_write,
    };
    install_method(
        runtime,
        object,
        "write",
        1,
        callback,
        &format!("process.{name}.write"),
    )?;
    Ok(RegisterValue::from_object_handle(object.0))
}

#[dive(name = "write", length = 1)]
fn process_stdout_write(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let text = args
        .first()
        .copied()
        .map(|value| value_to_string(runtime, value))
        .unwrap_or_default();
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(text.as_bytes()).map_err(|error| {
        VmNativeCallError::Internal(format!("stdout write failed: {error}").into())
    })?;
    Ok(RegisterValue::from_bool(true))
}

#[dive(name = "write", length = 1)]
fn process_stderr_write(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let text = args
        .first()
        .copied()
        .map(|value| value_to_string(runtime, value))
        .unwrap_or_default();
    let mut stderr = std::io::stderr().lock();
    stderr.write_all(text.as_bytes()).map_err(|error| {
        VmNativeCallError::Internal(format!("stderr write failed: {error}").into())
    })?;
    Ok(RegisterValue::from_bool(true))
}

fn process_handle(
    this: &RegisterValue,
    runtime: &mut RuntimeState,
) -> Result<ObjectHandle, VmNativeCallError> {
    this.as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| type_error(runtime, "process receiver must be an object"))
}

fn process_state(runtime: &mut RuntimeState) -> Option<NodeProcessState> {
    let process = read_global_process(runtime)?;
    let state = own_property(runtime, process, PROCESS_STATE_SLOT)?;
    runtime
        .native_payload_from_value::<NodeProcessState>(&state)
        .ok()
        .cloned()
}

fn with_process_state(runtime: &mut RuntimeState, f: impl FnOnce(&mut NodeProcessState)) {
    let Some(process) = read_global_process(runtime) else {
        return;
    };
    let Some(state) = own_property(runtime, process, PROCESS_STATE_SLOT) else {
        return;
    };
    if let Ok(payload) = runtime.native_payload_mut_from_value::<NodeProcessState>(&state) {
        f(payload);
    }
}

fn read_global_process(runtime: &mut RuntimeState) -> Option<ObjectHandle> {
    let global = runtime.intrinsics().global_object();
    let property = runtime.intern_property_name(PROCESS_GLOBAL_SLOT);
    runtime
        .own_property_value(global, property)
        .ok()
        .and_then(|value| value.as_object_handle())
        .map(ObjectHandle)
}

fn string_array_value(runtime: &mut RuntimeState, values: &[String]) -> RegisterValue {
    let values: Vec<_> = values
        .iter()
        .map(|value| string_value(runtime, value))
        .collect();
    RegisterValue::from_object_handle(runtime.alloc_array_with_elements(&values).0)
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
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use otter_runtime::{
        CapabilitiesBuilder, ModuleLoaderConfig, ObjectHandle, OtterRuntime, RegisterValue,
        RuntimeState,
    };

    use crate::nodejs_extension;

    fn temp_test_dir(name: &str) -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        dir.push(format!("otter-nodejs-{name}-{unique}"));
        std::fs::create_dir_all(&dir).expect("temp dir should exist");
        dir
    }

    fn own_property(
        runtime: &mut RuntimeState,
        object: RegisterValue,
        name: &str,
    ) -> RegisterValue {
        let object = object
            .as_object_handle()
            .map(ObjectHandle)
            .expect("value should be object");
        let property = runtime.intern_property_name(name);
        runtime
            .own_property_value(object, property)
            .expect("property should exist")
    }

    fn global_property(runtime: &mut OtterRuntime, name: &str) -> RegisterValue {
        let global = runtime.state().intrinsics().global_object();
        own_property(
            runtime.state_mut(),
            RegisterValue::from_object_handle(global.0),
            name,
        )
    }

    #[test]
    fn global_process_is_available_from_extension() {
        let mut runtime = OtterRuntime::builder()
            .profile(otter_runtime::RuntimeProfile::Full)
            .extension(nodejs_extension())
            .build();

        let result = runtime
            .run_script(
                "globalThis.__process_ok = typeof process.cwd === 'function';",
                "main.js",
            )
            .expect("script should execute");
        let _ = result;
        assert_eq!(
            global_property(&mut runtime, "__process_ok").as_bool(),
            Some(true)
        );
    }

    #[test]
    fn commonjs_process_module_shares_global_identity_and_argv() {
        let dir = temp_test_dir("process-cjs");
        std::fs::write(
            dir.join("main.cjs"),
            "const proc = require('node:process'); module.exports = [proc === process, proc.argv[2]].join(':');",
        )
        .expect("script should write");

        let mut runtime = OtterRuntime::builder()
            .profile(otter_runtime::RuntimeProfile::Full)
            .module_loader(ModuleLoaderConfig {
                base_dir: dir.clone(),
                ..Default::default()
            })
            .process_argv([
                "/bin/otter".to_string(),
                dir.join("main.cjs").to_string_lossy().to_string(),
                "user-arg".to_string(),
            ])
            .extension(nodejs_extension())
            .build();

        let result = runtime
            .run_entry_specifier("./main.cjs", None)
            .expect("cjs should execute");
        assert_eq!(
            runtime
                .state_mut()
                .js_to_string_infallible(result.return_value())
                .into_string(),
            "true:user-arg"
        );
    }

    #[test]
    fn process_env_respects_capabilities_and_deny_patterns() {
        let mut runtime = OtterRuntime::builder()
            .profile(otter_runtime::RuntimeProfile::Full)
            .capabilities(
                CapabilitiesBuilder::new()
                    .allow_env(["VISIBLE".to_string(), "PATH".to_string()])
                    .build(),
            )
            .env(|builder| {
                builder
                    .explicit("VISIBLE", "ok")
                    .explicit("HIDDEN", "nope")
                    .passthrough_var("PATH")
                    .deny_pattern("PATH")
            })
            .extension(nodejs_extension())
            .build();

        runtime
            .run_script(
                "globalThis.__env = [String(process.env.VISIBLE), String(process.env.HIDDEN), String(process.env.PATH)].join('|');",
                "env.js",
            )
            .expect("script should execute");

        let value = global_property(&mut runtime, "__env");
        assert_eq!(
            runtime
                .state_mut()
                .js_to_string_infallible(value)
                .into_string(),
            "ok|undefined|undefined"
        );
    }

    #[test]
    fn process_next_tick_runs_before_queue_microtask() {
        let mut runtime = OtterRuntime::builder()
            .profile(otter_runtime::RuntimeProfile::Full)
            .extension(nodejs_extension())
            .build();

        runtime
            .run_script(
                "const order = []; queueMicrotask(() => order.push('micro')); process.nextTick(() => order.push('tick')); order.push('sync'); globalThis.__order = order;",
                "next-tick.js",
            )
            .expect("script should execute");

        let order = global_property(&mut runtime, "__order");
        let first = own_property(runtime.state_mut(), order, "0");
        let second = own_property(runtime.state_mut(), order, "1");
        let third = own_property(runtime.state_mut(), order, "2");
        assert_eq!(
            runtime
                .state_mut()
                .js_to_string_infallible(first)
                .into_string(),
            "sync"
        );
        assert_eq!(
            runtime
                .state_mut()
                .js_to_string_infallible(second)
                .into_string(),
            "tick"
        );
        assert_eq!(
            runtime
                .state_mut()
                .js_to_string_infallible(third)
                .into_string(),
            "micro"
        );
    }
}
