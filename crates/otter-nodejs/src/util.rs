use otter_macros::lodge;
use otter_runtime::{ObjectHandle, RegisterValue, RuntimeState, VmNativeCallError};
use otter_vm::object::HeapValueKind;

use crate::support::{
    install_method, install_readonly_value, string_value, type_error, value_to_string,
};

const UTIL_EXPORT_SLOT: &str = "__otter_node_util_export";
const UTIL_TYPES_SLOT: &str = "__otter_node_util_types";

lodge!(
    util_module,
    module_specifiers = ["node:util", "util"],
    kind = commonjs,
    default = value(util_export_value(runtime)?),
);

pub(crate) fn util_types_value(runtime: &mut RuntimeState) -> Result<RegisterValue, String> {
    ensure_util_exports(runtime)?;
    read_global_slot(runtime, UTIL_TYPES_SLOT)
}

fn util_export_value(runtime: &mut RuntimeState) -> Result<RegisterValue, String> {
    ensure_util_exports(runtime)?;
    read_global_slot(runtime, UTIL_EXPORT_SLOT)
}

fn ensure_util_exports(runtime: &mut RuntimeState) -> Result<(), String> {
    if read_global_slot(runtime, UTIL_EXPORT_SLOT).is_ok()
        && read_global_slot(runtime, UTIL_TYPES_SLOT).is_ok()
    {
        return Ok(());
    }

    let types = runtime.alloc_object();
    install_method(
        runtime,
        types,
        "isAnyArrayBuffer",
        1,
        util_is_any_array_buffer,
        "util.types.isAnyArrayBuffer",
    )?;
    install_method(
        runtime,
        types,
        "isArrayBuffer",
        1,
        util_is_array_buffer,
        "util.types.isArrayBuffer",
    )?;
    install_method(
        runtime,
        types,
        "isArrayBufferView",
        1,
        util_is_array_buffer_view,
        "util.types.isArrayBufferView",
    )?;
    install_method(
        runtime,
        types,
        "isAsyncFunction",
        1,
        util_is_async_function,
        "util.types.isAsyncFunction",
    )?;
    install_method(
        runtime,
        types,
        "isDataView",
        1,
        util_is_data_view,
        "util.types.isDataView",
    )?;
    install_method(
        runtime,
        types,
        "isDate",
        1,
        util_is_date,
        "util.types.isDate",
    )?;
    install_method(
        runtime,
        types,
        "isExternal",
        1,
        util_is_external,
        "util.types.isExternal",
    )?;
    install_method(runtime, types, "isMap", 1, util_is_map, "util.types.isMap")?;
    install_method(
        runtime,
        types,
        "isMapIterator",
        1,
        util_is_map_iterator,
        "util.types.isMapIterator",
    )?;
    install_method(
        runtime,
        types,
        "isNativeError",
        1,
        util_is_native_error,
        "util.types.isNativeError",
    )?;
    install_method(
        runtime,
        types,
        "isPromise",
        1,
        util_is_promise,
        "util.types.isPromise",
    )?;
    install_method(
        runtime,
        types,
        "isRegExp",
        1,
        util_is_regexp,
        "util.types.isRegExp",
    )?;
    install_method(runtime, types, "isSet", 1, util_is_set, "util.types.isSet")?;
    install_method(
        runtime,
        types,
        "isSetIterator",
        1,
        util_is_set_iterator,
        "util.types.isSetIterator",
    )?;
    install_method(
        runtime,
        types,
        "isTypedArray",
        1,
        util_is_typed_array,
        "util.types.isTypedArray",
    )?;
    install_method(
        runtime,
        types,
        "isUint8Array",
        1,
        util_is_uint8_array,
        "util.types.isUint8Array",
    )?;

    let export = runtime.alloc_object();
    install_method(runtime, export, "inspect", 1, util_inspect, "util.inspect")?;
    install_method(runtime, export, "format", 1, util_format, "util.format")?;
    install_method(
        runtime,
        export,
        "debuglog",
        1,
        util_debuglog,
        "util.debuglog",
    )?;
    install_method(
        runtime,
        export,
        "getCallSites",
        0,
        util_get_call_sites,
        "util.getCallSites",
    )?;
    install_readonly_value(
        runtime,
        export,
        "types",
        RegisterValue::from_object_handle(types.0),
    )?;

    runtime.install_global_value(
        UTIL_EXPORT_SLOT,
        RegisterValue::from_object_handle(export.0),
    );
    runtime.install_global_value(UTIL_TYPES_SLOT, RegisterValue::from_object_handle(types.0));
    Ok(())
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

fn first_arg(args: &[RegisterValue]) -> RegisterValue {
    args.first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined)
}

fn heap_kind(runtime: &mut RuntimeState, value: RegisterValue) -> Option<HeapValueKind> {
    value
        .as_object_handle()
        .map(ObjectHandle)
        .and_then(|handle| runtime.objects().kind(handle).ok())
}

fn bool_result(value: bool) -> Result<RegisterValue, VmNativeCallError> {
    Ok(RegisterValue::from_bool(value))
}

fn util_inspect(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = first_arg(args);
    let rendered = if value == RegisterValue::undefined() {
        "undefined".to_string()
    } else if value == RegisterValue::null() {
        "null".to_string()
    } else if let Some(boolean) = value.as_bool() {
        if boolean { "true" } else { "false" }.to_string()
    } else if let Some(number) = value.as_number() {
        number.to_string()
    } else {
        value_to_string(runtime, value)
    };
    Ok(string_value(runtime, rendered))
}

fn util_format(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if args.is_empty() {
        return Ok(string_value(runtime, ""));
    }

    let rendered = args
        .iter()
        .copied()
        .map(|value| value_to_string(runtime, value))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(string_value(runtime, rendered))
}

fn util_debuglog(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let descriptor = otter_vm::descriptors::NativeFunctionDescriptor::method(
        "debuglog",
        0,
        util_debuglog_logger,
    );
    let id = runtime.register_native_function(descriptor);
    let function = runtime.alloc_host_function(id);
    Ok(RegisterValue::from_object_handle(function.0))
}

fn util_debuglog_logger(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Ok(RegisterValue::undefined())
}

fn util_get_call_sites(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Ok(RegisterValue::from_object_handle(
        runtime.alloc_array_with_elements(&[]).0,
    ))
}

fn util_is_any_array_buffer(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    bool_result(matches!(
        heap_kind(runtime, first_arg(args)),
        Some(HeapValueKind::ArrayBuffer | HeapValueKind::SharedArrayBuffer)
    ))
}

fn util_is_array_buffer(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    bool_result(matches!(
        heap_kind(runtime, first_arg(args)),
        Some(HeapValueKind::ArrayBuffer)
    ))
}

fn util_is_array_buffer_view(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    bool_result(matches!(
        heap_kind(runtime, first_arg(args)),
        Some(HeapValueKind::TypedArray | HeapValueKind::DataView)
    ))
}

fn util_is_async_function(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    bool_result(false)
}

fn util_is_data_view(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    bool_result(matches!(
        heap_kind(runtime, first_arg(args)),
        Some(HeapValueKind::DataView)
    ))
}

fn util_is_date(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let Some(handle) = first_arg(args).as_object_handle().map(ObjectHandle) else {
        return bool_result(false);
    };
    let prototype = runtime.objects().get_prototype(handle).ok().flatten();
    bool_result(prototype == Some(runtime.intrinsics().date_prototype()))
}

fn util_is_external(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    bool_result(false)
}

fn util_is_map(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    bool_result(matches!(
        heap_kind(runtime, first_arg(args)),
        Some(HeapValueKind::Map)
    ))
}

fn util_is_map_iterator(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    bool_result(matches!(
        heap_kind(runtime, first_arg(args)),
        Some(HeapValueKind::MapIterator)
    ))
}

fn util_is_native_error(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    bool_result(false)
}

fn util_is_promise(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    bool_result(matches!(
        heap_kind(runtime, first_arg(args)),
        Some(HeapValueKind::Promise)
    ))
}

fn util_is_regexp(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    bool_result(matches!(
        heap_kind(runtime, first_arg(args)),
        Some(HeapValueKind::RegExp)
    ))
}

fn util_is_set(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    bool_result(matches!(
        heap_kind(runtime, first_arg(args)),
        Some(HeapValueKind::Set)
    ))
}

fn util_is_set_iterator(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    bool_result(matches!(
        heap_kind(runtime, first_arg(args)),
        Some(HeapValueKind::SetIterator)
    ))
}

fn util_is_typed_array(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    bool_result(matches!(
        heap_kind(runtime, first_arg(args)),
        Some(HeapValueKind::TypedArray)
    ))
}

fn util_is_uint8_array(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let Some(handle) = first_arg(args).as_object_handle().map(ObjectHandle) else {
        return bool_result(false);
    };
    if !matches!(
        runtime.objects().kind(handle),
        Ok(HeapValueKind::TypedArray)
    ) {
        return bool_result(false);
    }
    let kind = runtime
        .objects()
        .typed_array_kind(handle)
        .map_err(|_| type_error(runtime, "util.types.isUint8Array: invalid typed array"))?;
    bool_result(kind == otter_vm::object::TypedArrayKind::Uint8)
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use otter_runtime::{ModuleLoaderConfig, OtterRuntime};

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

    #[test]
    fn util_module_exposes_types_and_binding_identity() {
        let dir = temp_test_dir("util-cjs");
        std::fs::write(
            dir.join("main.cjs"),
            "const util = require('util'); const binding = process.binding('util'); module.exports = String(util.types === binding) + ':' + typeof util.inspect + ':' + typeof util.getCallSites;",
        )
        .expect("script should write");

        let mut runtime = OtterRuntime::builder()
            .profile(otter_runtime::RuntimeProfile::Full)
            .module_loader(ModuleLoaderConfig {
                base_dir: dir.clone(),
                ..Default::default()
            })
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
            "true:function:function"
        );
    }
}
