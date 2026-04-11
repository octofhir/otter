use std::path::PathBuf;

use otter_macros::lodge;
use otter_runtime::{ObjectHandle, RegisterValue, RuntimeState, VmNativeCallError};

use crate::support::{
    install_method, install_readonly_value, string_value, type_error, value_to_string,
};

const URL_EXPORT_SLOT: &str = "__otter_node_url_export";

lodge!(
    url_module,
    module_specifiers = ["node:url", "url"],
    kind = commonjs,
    default = value(url_export_value(runtime)?),
);

fn url_export_value(runtime: &mut RuntimeState) -> Result<RegisterValue, String> {
    if let Ok(value) = read_global_slot(runtime, URL_EXPORT_SLOT) {
        return Ok(value);
    }

    let export = runtime.alloc_object();
    let global = runtime.intrinsics().global_object();
    for name in ["URL", "URLSearchParams"] {
        let property = runtime.intern_property_name(name);
        let value = runtime
            .own_property_value(global, property)
            .map_err(|error| format!("failed to read global {name}: {error:?}"))?;
        install_readonly_value(runtime, export, name, value)?;
    }
    install_method(
        runtime,
        export,
        "pathToFileURL",
        1,
        url_path_to_file_url,
        "url.pathToFileURL",
    )?;
    install_method(
        runtime,
        export,
        "fileURLToPath",
        1,
        url_file_url_to_path,
        "url.fileURLToPath",
    )?;
    runtime.install_global_value(URL_EXPORT_SLOT, RegisterValue::from_object_handle(export.0));
    Ok(RegisterValue::from_object_handle(export.0))
}

fn url_path_to_file_url(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let path = args
        .first()
        .copied()
        .ok_or_else(|| type_error(runtime, "url.pathToFileURL requires a path"))?;
    let path = value_to_string(runtime, path);
    let absolute = std::fs::canonicalize(&path).unwrap_or_else(|_| {
        let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        cwd.push(path);
        cwd
    });
    let mut href = "file://".to_string();
    href.push_str(&absolute.to_string_lossy().replace(' ', "%20"));
    let global = runtime.intrinsics().global_object();
    let property = runtime.intern_property_name("URL");
    let url_ctor = runtime
        .own_property_value(global, property)
        .map_err(|_| type_error(runtime, "URL global is missing"))?;
    let callable = url_ctor
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| type_error(runtime, "URL constructor is invalid"))?;
    let href_value = string_value(runtime, href);
    runtime.call_callable(callable, RegisterValue::undefined(), &[href_value])
}

fn url_file_url_to_path(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = args
        .first()
        .copied()
        .ok_or_else(|| type_error(runtime, "url.fileURLToPath requires a URL"))?;
    let href = value_to_string(runtime, value);
    let path = href
        .strip_prefix("file://")
        .unwrap_or(href.as_str())
        .replace("%20", " ");
    Ok(string_value(runtime, path))
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
