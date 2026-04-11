use std::path::{Component, Path, PathBuf};

use otter_macros::lodge;
use otter_runtime::{RegisterValue, RuntimeState, VmNativeCallError, current_process};

use crate::process::current_node_cwd;
use crate::support::{install_method, install_readonly_value, string_value, value_to_string};

const PATH_EXPORT_SLOT: &str = "__otter_node_path_export";

lodge!(
    path_module,
    module_specifiers = ["node:path", "path"],
    kind = commonjs,
    default = value(path_export_value(runtime)?),
);

fn path_export_value(runtime: &mut RuntimeState) -> Result<RegisterValue, String> {
    if let Ok(value) = read_global_slot(runtime, PATH_EXPORT_SLOT) {
        return Ok(value);
    }

    let export = runtime.alloc_object();
    for (name, arity, callback) in [
        ("resolve", 1, path_resolve as _),
        ("join", 2, path_join as _),
        ("relative", 2, path_relative as _),
        ("dirname", 1, path_dirname as _),
        ("basename", 1, path_basename as _),
        ("extname", 1, path_extname as _),
        ("isAbsolute", 1, path_is_absolute as _),
        ("normalize", 1, path_normalize as _),
    ] {
        install_method(
            runtime,
            export,
            name,
            arity,
            callback,
            &format!("path.{name}"),
        )?;
    }
    let sep = string_value(runtime, std::path::MAIN_SEPARATOR.to_string());
    install_readonly_value(runtime, export, "sep", sep)?;
    let delimiter = string_value(runtime, if cfg!(windows) { ";" } else { ":" });
    install_readonly_value(runtime, export, "delimiter", delimiter)?;

    runtime.install_global_value(
        PATH_EXPORT_SLOT,
        RegisterValue::from_object_handle(export.0),
    );
    Ok(RegisterValue::from_object_handle(export.0))
}

fn path_resolve(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let mut resolved = current_cwd(runtime);
    for value in args.iter().copied() {
        let segment = value_to_string(runtime, value);
        let path = PathBuf::from(segment);
        if path.is_absolute() {
            resolved = path;
        } else {
            resolved.push(path);
        }
    }
    Ok(string_value(runtime, normalize_path_string(&resolved)))
}

fn path_join(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let mut path = PathBuf::new();
    for value in args.iter().copied() {
        let segment = value_to_string(runtime, value);
        if !segment.is_empty() {
            path.push(segment);
        }
    }
    Ok(string_value(runtime, normalize_path_string(&path)))
}

fn path_relative(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let from = args
        .first()
        .copied()
        .map(|value| value_to_string(runtime, value))
        .unwrap_or_default();
    let to = args
        .get(1)
        .copied()
        .map(|value| value_to_string(runtime, value))
        .unwrap_or_default();
    let from = normalize_path(PathBuf::from(from));
    let to = normalize_path(PathBuf::from(to));

    let from_components: Vec<_> = from.components().collect();
    let to_components: Vec<_> = to.components().collect();
    let mut shared = 0usize;
    while shared < from_components.len()
        && shared < to_components.len()
        && from_components[shared] == to_components[shared]
    {
        shared += 1;
    }

    let mut relative = PathBuf::new();
    for component in &from_components[shared..] {
        if matches!(component, Component::Normal(_)) {
            relative.push("..");
        }
    }
    for component in &to_components[shared..] {
        relative.push(component.as_os_str());
    }

    let rendered = if relative.as_os_str().is_empty() {
        ".".to_string()
    } else {
        normalize_path_string(&relative)
    };
    Ok(string_value(runtime, rendered))
}

fn path_dirname(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let input = args
        .first()
        .copied()
        .map(|value| value_to_string(runtime, value))
        .unwrap_or_default();
    let path = PathBuf::from(input);
    let rendered = path
        .parent()
        .map(normalize_path_string)
        .unwrap_or_else(|| ".".to_string());
    Ok(string_value(runtime, rendered))
}

fn path_basename(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let input = args
        .first()
        .copied()
        .map(|value| value_to_string(runtime, value))
        .unwrap_or_default();
    let rendered = Path::new(&input)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(string_value(runtime, rendered))
}

fn path_extname(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let input = args
        .first()
        .copied()
        .map(|value| value_to_string(runtime, value))
        .unwrap_or_default();
    let rendered = Path::new(&input)
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_default();
    Ok(string_value(runtime, rendered))
}

fn path_is_absolute(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let input = args
        .first()
        .copied()
        .map(|value| value_to_string(runtime, value))
        .unwrap_or_default();
    Ok(RegisterValue::from_bool(Path::new(&input).is_absolute()))
}

fn path_normalize(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let input = args
        .first()
        .copied()
        .map(|value| value_to_string(runtime, value))
        .unwrap_or_default();
    Ok(string_value(
        runtime,
        normalize_path_string(&PathBuf::from(input)),
    ))
}

fn current_cwd(runtime: &mut RuntimeState) -> PathBuf {
    current_node_cwd(runtime)
        .or_else(|| current_process(runtime).map(|(process, _)| process.cwd))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn normalize_path(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

fn normalize_path_string(path: &Path) -> String {
    normalize_path(path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

fn read_global_slot(runtime: &mut RuntimeState, slot: &str) -> Result<RegisterValue, String> {
    let global = runtime.intrinsics().global_object();
    let property = runtime.intern_property_name(slot);
    let value = runtime
        .own_property_value(global, property)
        .map_err(|error| format!("failed to read global slot '{slot}': {error:?}"))?;
    if value == RegisterValue::undefined() {
        return Err(format!("global slot '{slot}' is undefined"));
    }
    Ok(value)
}
