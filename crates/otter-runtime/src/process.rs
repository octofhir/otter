//! Node-compatible `process` global installed by the runtime.
//!
//! # Contents
//! - [`default_argv`] builds the runtime's default `process.argv` snapshot.
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

use otter_vm::{Attr, Interpreter, JsString, NativeCall, NativeCtx, NativeError, ObjectBuilder};

use crate::{
    CapabilityRequest, CapabilitySet, DiagnosticCode, OtterError, RuntimeCapability,
    default_check_capability, gc_oom_to_error, string_oom_to_error,
};

pub(crate) fn default_argv() -> Vec<String> {
    std::env::current_exe()
        .ok()
        .map(|path| vec![path.to_string_lossy().to_string()])
        .unwrap_or_else(|| vec!["otter".to_string()])
}

pub(crate) fn install_global(
    interp: &mut Interpreter,
    process_argv: &[String],
    capabilities: &CapabilitySet,
) -> Result<(), OtterError> {
    let process = otter_vm::object::alloc_object(interp.gc_heap_mut()).map_err(gc_oom_to_error)?;
    let argv_values = process_argv
        .iter()
        .map(|arg| {
            JsString::from_str(arg, &interp.string_heap_clone())
                .map(otter_vm::Value::String)
                .map_err(string_oom_to_error)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let argv = otter_vm::array::from_elements(interp.gc_heap_mut(), argv_values)
        .map_err(gc_oom_to_error)?;
    otter_vm::object::set(
        process,
        interp.gc_heap_mut(),
        "argv",
        otter_vm::Value::Array(argv),
    );

    let exec_argv =
        otter_vm::array::from_elements(interp.gc_heap_mut(), []).map_err(gc_oom_to_error)?;
    otter_vm::object::set(
        process,
        interp.gc_heap_mut(),
        "execArgv",
        otter_vm::Value::Array(exec_argv),
    );

    let argv0 = process_argv.first().map(String::as_str).unwrap_or("otter");
    let argv0 = string_value(interp, argv0)?;
    otter_vm::object::set(process, interp.gc_heap_mut(), "argv0", argv0);

    let exec_path = std::env::current_exe()
        .ok()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| "otter".to_string());
    let exec_path = string_value(interp, &exec_path)?;
    otter_vm::object::set(process, interp.gc_heap_mut(), "execPath", exec_path);

    let platform = string_value(interp, node_platform())?;
    otter_vm::object::set(process, interp.gc_heap_mut(), "platform", platform);

    let version = string_value(interp, concat!("v", env!("CARGO_PKG_VERSION")))?;
    otter_vm::object::set(process, interp.gc_heap_mut(), "version", version);

    let versions = otter_vm::object::alloc_object(interp.gc_heap_mut()).map_err(gc_oom_to_error)?;
    let otter_version = string_value(interp, env!("CARGO_PKG_VERSION"))?;
    otter_vm::object::set(versions, interp.gc_heap_mut(), "otter", otter_version);
    otter_vm::object::set(
        process,
        interp.gc_heap_mut(),
        "versions",
        otter_vm::Value::Object(versions),
    );

    otter_vm::object::set(
        process,
        interp.gc_heap_mut(),
        "exitCode",
        otter_vm::Value::Undefined,
    );

    let env = otter_vm::object::alloc_object(interp.gc_heap_mut()).map_err(gc_oom_to_error)?;
    for (name, value) in std::env::vars() {
        if !default_check_capability(
            capabilities,
            RuntimeCapability::Env,
            &CapabilityRequest::EnvVar(&name),
        ) {
            continue;
        }
        let value = JsString::from_str(&value, &interp.string_heap_clone())
            .map(otter_vm::Value::String)
            .map_err(string_oom_to_error)?;
        otter_vm::object::set(env, interp.gc_heap_mut(), &name, value);
    }
    otter_vm::object::set(
        process,
        interp.gc_heap_mut(),
        "env",
        otter_vm::Value::Object(env),
    );

    ObjectBuilder::from_object(interp.gc_heap_mut(), process)
        .method(
            "cwd",
            0,
            NativeCall::Static(process_cwd),
            Attr::builtin_function(),
        )
        .map_err(|err| OtterError::Internal {
            code: DiagnosticCode::GlobalClassBootstrap.as_str().to_string(),
            message: err.to_string(),
        })?;
    interp.set_global("process", otter_vm::Value::Object(process));
    Ok(())
}

fn string_value(interp: &mut Interpreter, value: &str) -> Result<otter_vm::Value, OtterError> {
    Ok(otter_vm::Value::String(
        JsString::from_str(value, &interp.string_heap_clone()).map_err(string_oom_to_error)?,
    ))
}

fn process_cwd(
    ctx: &mut NativeCtx<'_>,
    _args: &[otter_vm::Value],
) -> Result<otter_vm::Value, NativeError> {
    let cwd = std::env::current_dir().map_err(|err| NativeError::TypeError {
        name: "process.cwd",
        reason: err.to_string(),
    })?;
    let heap = ctx.interp_mut().string_heap_clone();
    Ok(otter_vm::Value::String(
        JsString::from_str(&cwd.to_string_lossy(), &heap).map_err(|err| {
            NativeError::TypeError {
                name: "process.cwd",
                reason: err.to_string(),
            }
        })?,
    ))
}

fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
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
  process.version[0],
  typeof process.versions.otter,
  typeof process.exitCode
].join(":")
"#,
            )
            .unwrap();
        assert_eq!(
            result.completion_string(),
            "string:custom-otter:string:0:string:v:string:undefined"
        );
    }
}
