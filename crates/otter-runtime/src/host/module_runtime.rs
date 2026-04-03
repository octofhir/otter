use std::collections::BTreeMap;

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType as OxcSourceType};

use otter_vm::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
use otter_vm::object::ObjectHandle;
use otter_vm::payload::VmTrace;
use otter_vm::{Interpreter, RegisterValue, RuntimeState};

use super::{
    HostedNativeModule, HostedNativeModuleRegistry, ImportContext, ModuleGraph, ModuleGraphNode,
    ModuleLoader, ModuleLoaderConfig, ModuleType, SourceType,
};

const MODULE_HELPER_NAME: &str = "__otter_module";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModuleRuntimeSession {
    helper: ObjectHandle,
}

impl ModuleRuntimeSession {
    const fn new(helper: ObjectHandle) -> Self {
        Self { helper }
    }

    const fn helper(self) -> ObjectHandle {
        self.helper
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostedEvaluationState {
    Pending,
    Evaluating,
    Evaluated,
    Error,
}

#[derive(Debug, Clone)]
struct HostedModuleRecord {
    module_type: ModuleType,
    source_type: SourceType,
    transformed_source: String,
    state: HostedEvaluationState,
    namespace: Option<ObjectHandle>,
    cjs_exports: Option<RegisterValue>,
}

impl VmTrace for HostedModuleRecord {
    fn trace(&self, tracer: &mut dyn otter_vm::payload::VmValueTracer) {
        self.namespace.trace(tracer);
        self.cjs_exports.trace(tracer);
    }
}

#[derive(Debug, Clone)]
pub struct ModuleRuntimePayload {
    loader_config: ModuleLoaderConfig,
    native_modules: HostedNativeModuleRegistry,
    modules: BTreeMap<String, HostedModuleRecord>,
}

impl ModuleRuntimePayload {
    fn new(loader_config: ModuleLoaderConfig, native_modules: HostedNativeModuleRegistry) -> Self {
        Self {
            loader_config,
            native_modules,
            modules: BTreeMap::new(),
        }
    }
}

impl VmTrace for ModuleRuntimePayload {
    fn trace(&self, tracer: &mut dyn otter_vm::payload::VmValueTracer) {
        for record in self.modules.values() {
            record.trace(tracer);
        }
    }
}

pub(crate) fn install_module_runtime_session(
    runtime: &mut RuntimeState,
    loader_config: ModuleLoaderConfig,
    native_modules: HostedNativeModuleRegistry,
) -> ModuleRuntimeSession {
    let helper =
        runtime.alloc_native_object(ModuleRuntimePayload::new(loader_config, native_modules));
    install_method(runtime, helper, "import", module_import);
    install_method(runtime, helper, "require", module_require);
    install_method(runtime, helper, "export", module_export);
    install_method(runtime, helper, "exportAll", module_export_all);
    install_method(runtime, helper, "commitCjs", module_commit_cjs);
    install_method(runtime, helper, "beginCjs", module_begin_cjs);
    runtime.install_global_value(
        MODULE_HELPER_NAME,
        RegisterValue::from_object_handle(helper.0),
    );
    ModuleRuntimeSession::new(helper)
}

pub(crate) fn preload_module_graph(
    runtime: &mut RuntimeState,
    session: ModuleRuntimeSession,
    loader_config: ModuleLoaderConfig,
    graph: &ModuleGraph,
) -> Result<(), String> {
    let helper = session.helper();
    let payload = runtime
        .native_payload_mut::<ModuleRuntimePayload>(helper)
        .map_err(|error| error.to_string())?;
    payload.loader_config = loader_config.clone();

    let loader = ModuleLoader::new(loader_config);
    for node in graph.nodes().values() {
        if payload.modules.contains_key(&node.module.url) {
            continue;
        }
        if node.module.url.starts_with("otter:")
            && !payload.native_modules.contains(&node.module.url)
        {
            return Err(format!(
                "native hosted module '{}' is not registered on this runtime",
                node.module.url
            ));
        }
        let transformed_source = transform_module_source(&loader, node)?;
        let module_type = payload
            .native_modules
            .kind_for(&node.module.url)
            .map(|kind| match kind {
                super::HostedNativeModuleKind::Esm => ModuleType::Esm,
                super::HostedNativeModuleKind::CommonJs => ModuleType::CommonJs,
            })
            .unwrap_or(node.module.module_type);
        payload.modules.insert(
            node.module.url.clone(),
            HostedModuleRecord {
                module_type,
                source_type: node.module.source_type,
                transformed_source,
                state: HostedEvaluationState::Pending,
                namespace: None,
                cjs_exports: None,
            },
        );
    }

    Ok(())
}

pub(crate) fn execute_preloaded_entry(
    runtime: &mut RuntimeState,
    session: ModuleRuntimeSession,
    entry_url: &str,
) -> Result<otter_vm::interpreter::ExecutionResult, String> {
    let helper = session.helper();
    ensure_loaded_module(runtime, helper, entry_url).map_err(|error| error.to_string())?;
    evaluate_loaded_module(runtime, helper, entry_url).map_err(|error| error.to_string())?;

    let payload = runtime
        .native_payload::<ModuleRuntimePayload>(helper)
        .map_err(|error| error.to_string())?;
    let record = payload
        .modules
        .get(entry_url)
        .ok_or_else(|| format!("entry module '{entry_url}' is not present in runtime registry"))?;

    match record.source_type {
        SourceType::Json => Ok(otter_vm::interpreter::ExecutionResult::new(
            RegisterValue::from_object_handle(
                record
                    .namespace
                    .ok_or_else(|| format!("JSON namespace missing for '{entry_url}'"))?
                    .0,
            ),
        )),
        _ => match record.module_type {
            ModuleType::CommonJs => Ok(otter_vm::interpreter::ExecutionResult::new(
                record.cjs_exports.unwrap_or_else(RegisterValue::undefined),
            )),
            ModuleType::Esm => Ok(otter_vm::interpreter::ExecutionResult::new(
                RegisterValue::from_object_handle(
                    record
                        .namespace
                        .ok_or_else(|| format!("ESM namespace missing for '{entry_url}'"))?
                        .0,
                ),
            )),
        },
    }
}

fn install_method(
    runtime: &mut RuntimeState,
    helper: ObjectHandle,
    name: &str,
    callback: otter_vm::descriptors::VmNativeFunction,
) {
    let descriptor = NativeFunctionDescriptor::method(name, 0, callback);
    let id = runtime.register_native_function(descriptor);
    let function = runtime.alloc_host_function(id);
    let property = runtime.intern_property_name(name);
    let _ = runtime.objects_mut().set_property(
        helper,
        property,
        RegisterValue::from_object_handle(function.0),
    );
}

fn module_import(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let helper = helper_handle(this)?;
    let specifier = arg_string(runtime, args.first(), "import: missing specifier")?;
    let referrer = args
        .get(1)
        .map(|value| runtime.js_to_string_infallible(*value).into_string())
        .unwrap_or_default();
    ensure_loaded_module(runtime, helper, &specifier)?;
    let module_url = if referrer.is_empty() {
        specifier
    } else {
        specifier
    };
    evaluate_loaded_module(runtime, helper, &module_url)?;
    let namespace = namespace_value(runtime, helper, &module_url)?;
    Ok(RegisterValue::from_object_handle(namespace.0))
}

fn module_require(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let helper = helper_handle(this)?;
    let specifier = arg_string(runtime, args.first(), "require: missing specifier")?;
    let referrer = arg_string_opt(runtime, args.get(1)).unwrap_or_default();
    let url = resolve_specifier(runtime, helper, &specifier, &referrer, ImportContext::Cjs)?;
    ensure_loaded_module(runtime, helper, &url)?;
    evaluate_loaded_module(runtime, helper, &url)?;

    let payload = runtime
        .native_payload::<ModuleRuntimePayload>(helper)
        .map_err(native_internal)?;
    let record = payload
        .modules
        .get(&url)
        .ok_or_else(|| VmNativeCallError::Internal(format!("module '{url}' not found").into()))?;

    match record.source_type {
        SourceType::Json => Ok(record.cjs_exports.unwrap_or_else(RegisterValue::undefined)),
        _ => match record.module_type {
            ModuleType::CommonJs => Ok(record.cjs_exports.unwrap_or_else(RegisterValue::undefined)),
            ModuleType::Esm => Ok(RegisterValue::from_object_handle(
                namespace_value(runtime, helper, &url)?.0,
            )),
        },
    }
}

fn module_export(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let helper = helper_handle(this)?;
    let url = arg_string(runtime, args.first(), "export: missing module url")?;
    let export_name = arg_string(runtime, args.get(1), "export: missing export name")?;
    let value = *args
        .get(2)
        .ok_or_else(|| VmNativeCallError::Internal("export: missing export value".into()))?;
    let namespace = ensure_namespace_object(runtime, helper, &url)?;
    let property = runtime.intern_property_name(&export_name);
    runtime
        .objects_mut()
        .set_property(namespace, property, value)
        .map_err(|error| VmNativeCallError::Internal(format!("export failed: {error:?}").into()))?;
    Ok(value)
}

fn module_export_all(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let helper = helper_handle(this)?;
    let url = arg_string(
        runtime,
        args.first(),
        "exportAll: missing target module url",
    )?;
    let source = arg_string(runtime, args.get(1), "exportAll: missing source module url")?;
    ensure_loaded_module(runtime, helper, &source)?;
    evaluate_loaded_module(runtime, helper, &source)?;

    let target = ensure_namespace_object(runtime, helper, &url)?;
    let source_ns = namespace_value(runtime, helper, &source)?;
    let keys = runtime.enumerable_own_property_keys(source_ns)?;
    for key in keys {
        let Some(name) = runtime.property_names().get(key) else {
            continue;
        };
        if name == "default" {
            continue;
        }
        let value = runtime.own_property_value(source_ns, key)?;
        runtime
            .objects_mut()
            .set_property(target, key, value)
            .map_err(|error| {
                VmNativeCallError::Internal(format!("exportAll failed: {error:?}").into())
            })?;
    }
    Ok(RegisterValue::from_object_handle(target.0))
}

fn module_commit_cjs(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let helper = helper_handle(this)?;
    let url = arg_string(runtime, args.first(), "commitCjs: missing module url")?;
    let exports = *args
        .get(1)
        .ok_or_else(|| VmNativeCallError::Internal("commitCjs: missing exports value".into()))?;
    commit_cjs_exports(runtime, helper, &url, exports)?;
    Ok(exports)
}

fn module_begin_cjs(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let helper = helper_handle(this)?;
    let url = arg_string(runtime, args.first(), "beginCjs: missing module url")?;
    let exports = ensure_cjs_exports_object(runtime, helper, &url)?;
    let module = runtime.alloc_object();
    let exports_prop = runtime.intern_property_name("exports");
    runtime
        .objects_mut()
        .set_property(
            module,
            exports_prop,
            RegisterValue::from_object_handle(exports.0),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(format!("beginCjs failed: {error:?}").into())
        })?;
    Ok(RegisterValue::from_object_handle(module.0))
}

fn helper_handle(this: &RegisterValue) -> Result<ObjectHandle, VmNativeCallError> {
    this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("module helper receiver must be an object".into())
    })
}

fn arg_string(
    runtime: &mut RuntimeState,
    value: Option<&RegisterValue>,
    message: &str,
) -> Result<String, VmNativeCallError> {
    Ok(runtime
        .js_to_string_infallible(*value.ok_or_else(|| VmNativeCallError::Internal(message.into()))?)
        .into_string())
}

fn arg_string_opt(runtime: &mut RuntimeState, value: Option<&RegisterValue>) -> Option<String> {
    value.map(|value| runtime.js_to_string_infallible(*value).into_string())
}

fn ensure_loaded_module(
    runtime: &mut RuntimeState,
    helper: ObjectHandle,
    url_or_specifier: &str,
) -> Result<(), VmNativeCallError> {
    let already_loaded = runtime
        .native_payload::<ModuleRuntimePayload>(helper)
        .map_err(native_internal)?
        .modules
        .contains_key(url_or_specifier);
    if already_loaded {
        return Ok(());
    }

    let payload = runtime
        .native_payload::<ModuleRuntimePayload>(helper)
        .map_err(native_internal)?;
    let loader = ModuleLoader::new(payload.loader_config.clone());
    let graph = loader
        .load_graph(url_or_specifier, None)
        .map_err(|error| VmNativeCallError::Internal(error.to_string().into()))?;
    let config = payload.loader_config.clone();
    let _ = payload;
    preload_module_graph(runtime, ModuleRuntimeSession::new(helper), config, &graph)
        .map_err(|error| VmNativeCallError::Internal(error.into()))
}

fn resolve_specifier(
    runtime: &mut RuntimeState,
    helper: ObjectHandle,
    specifier: &str,
    referrer: &str,
    context: ImportContext,
) -> Result<String, VmNativeCallError> {
    let payload = runtime
        .native_payload::<ModuleRuntimePayload>(helper)
        .map_err(native_internal)?;
    ModuleLoader::new(payload.loader_config.clone())
        .resolve_with_context(
            specifier,
            if referrer.is_empty() {
                None
            } else {
                Some(referrer)
            },
            context,
        )
        .map_err(|error| VmNativeCallError::Internal(error.to_string().into()))
}

fn evaluate_loaded_module(
    runtime: &mut RuntimeState,
    helper: ObjectHandle,
    url: &str,
) -> Result<(), VmNativeCallError> {
    {
        let payload = runtime
            .native_payload::<ModuleRuntimePayload>(helper)
            .map_err(native_internal)?;
        let record = payload.modules.get(url).ok_or_else(|| {
            VmNativeCallError::Internal(format!("module '{url}' not preloaded").into())
        })?;
        match record.state {
            HostedEvaluationState::Evaluated | HostedEvaluationState::Evaluating => return Ok(()),
            HostedEvaluationState::Pending | HostedEvaluationState::Error => {}
        }
    }

    if url.starts_with("otter:") {
        return evaluate_native_module(runtime, helper, url);
    }

    let needs_namespace = {
        let payload = runtime
            .native_payload_mut::<ModuleRuntimePayload>(helper)
            .map_err(native_internal)?;
        let record = payload.modules.get_mut(url).ok_or_else(|| {
            VmNativeCallError::Internal(format!("module '{url}' not preloaded").into())
        })?;
        record.state = HostedEvaluationState::Evaluating;
        record.namespace.is_none()
    };
    let allocated_namespace = needs_namespace.then(|| runtime.alloc_object());
    let needs_exports = {
        let payload = runtime
            .native_payload_mut::<ModuleRuntimePayload>(helper)
            .map_err(native_internal)?;
        let record = payload.modules.get_mut(url).ok_or_else(|| {
            VmNativeCallError::Internal(format!("module '{url}' not preloaded").into())
        })?;
        if let Some(namespace) = allocated_namespace {
            record.namespace = Some(namespace);
        }
        record.module_type == ModuleType::CommonJs && record.cjs_exports.is_none()
    };
    let allocated_exports = needs_exports.then(|| runtime.alloc_object());
    let (module_type, source_type, transformed_source) = {
        let payload = runtime
            .native_payload_mut::<ModuleRuntimePayload>(helper)
            .map_err(native_internal)?;
        let record = payload.modules.get_mut(url).ok_or_else(|| {
            VmNativeCallError::Internal(format!("module '{url}' not preloaded").into())
        })?;
        if let Some(exports) = allocated_exports {
            record.cjs_exports = Some(RegisterValue::from_object_handle(exports.0));
        }
        (
            record.module_type,
            record.source_type,
            record.transformed_source.clone(),
        )
    };

    let result = if matches!(source_type, SourceType::Json) {
        evaluate_json_module(runtime, helper, url, &transformed_source)
    } else {
        let module =
            compile_transformed_module(&transformed_source, url, module_type, source_type)?;
        Interpreter::new()
            .execute_module(&module, runtime)
            .map(|_| ())
            .map_err(|error| VmNativeCallError::Internal(error.to_string().into()))
    };

    let payload = runtime
        .native_payload_mut::<ModuleRuntimePayload>(helper)
        .map_err(native_internal)?;
    let record = payload
        .modules
        .get_mut(url)
        .ok_or_else(|| VmNativeCallError::Internal(format!("module '{url}' disappeared").into()))?;
    record.state = if result.is_ok() {
        HostedEvaluationState::Evaluated
    } else {
        HostedEvaluationState::Error
    };

    result
}

fn evaluate_native_module(
    runtime: &mut RuntimeState,
    helper: ObjectHandle,
    url: &str,
) -> Result<(), VmNativeCallError> {
    let loader = {
        let payload = runtime
            .native_payload::<ModuleRuntimePayload>(helper)
            .map_err(native_internal)?;
        payload.native_modules.get(url).cloned().ok_or_else(|| {
            VmNativeCallError::Internal(
                format!("native hosted module '{url}' is not registered").into(),
            )
        })?
    };

    match loader
        .load(runtime)
        .map_err(|error| VmNativeCallError::Internal(error.into()))?
    {
        HostedNativeModule::Esm(namespace) => {
            let payload = runtime
                .native_payload_mut::<ModuleRuntimePayload>(helper)
                .map_err(native_internal)?;
            let record = payload.modules.get_mut(url).ok_or_else(|| {
                VmNativeCallError::Internal(format!("module '{url}' not found").into())
            })?;
            record.namespace = Some(namespace);
            record.state = HostedEvaluationState::Evaluated;
            Ok(())
        }
        HostedNativeModule::CommonJs(exports) => commit_cjs_exports(runtime, helper, url, exports),
    }
}

fn evaluate_json_module(
    runtime: &mut RuntimeState,
    helper: ObjectHandle,
    url: &str,
    source: &str,
) -> Result<(), VmNativeCallError> {
    let parsed = serde_json::from_str::<serde_json::Value>(source).map_err(|error| {
        VmNativeCallError::Internal(format!("invalid JSON module '{url}': {error}").into())
    })?;
    let value = json_value_to_register(runtime, &parsed)?;
    commit_cjs_exports(runtime, helper, url, value)
}

fn json_value_to_register(
    runtime: &mut RuntimeState,
    value: &serde_json::Value,
) -> Result<RegisterValue, VmNativeCallError> {
    match value {
        serde_json::Value::Null => Ok(RegisterValue::null()),
        serde_json::Value::Bool(value) => Ok(RegisterValue::from_bool(*value)),
        serde_json::Value::Number(value) => {
            let Some(number) = value.as_f64() else {
                return Err(VmNativeCallError::Internal(
                    "JSON number is not representable as f64".into(),
                ));
            };
            Ok(RegisterValue::from_number(number))
        }
        serde_json::Value::String(value) => {
            let string = runtime.alloc_string(value.clone());
            Ok(RegisterValue::from_object_handle(string.0))
        }
        serde_json::Value::Array(values) => {
            let mut elements = Vec::with_capacity(values.len());
            for value in values {
                elements.push(json_value_to_register(runtime, value)?);
            }
            let array = runtime.alloc_array_with_elements(&elements);
            Ok(RegisterValue::from_object_handle(array.0))
        }
        serde_json::Value::Object(entries) => {
            let object = runtime.alloc_object();
            for (key, value) in entries {
                let property = runtime.intern_property_name(key);
                let value = json_value_to_register(runtime, value)?;
                runtime
                    .objects_mut()
                    .set_property(object, property, value)
                    .map_err(|error| {
                        VmNativeCallError::Internal(
                            format!("JSON object property write failed: {error:?}").into(),
                        )
                    })?;
            }
            Ok(RegisterValue::from_object_handle(object.0))
        }
    }
}

fn compile_transformed_module(
    source: &str,
    url: &str,
    module_type: ModuleType,
    source_type: SourceType,
) -> Result<otter_vm::Module, VmNativeCallError> {
    match (module_type, source_type) {
        (_, SourceType::Json) => Err(VmNativeCallError::Internal(
            format!("JSON modules are not executable yet on hosted path: {url}").into(),
        )),
        (ModuleType::Esm, _) => otter_vm::source::compile_module(source, url)
            .map_err(|error| VmNativeCallError::Internal(error.to_string().into())),
        (ModuleType::CommonJs, SourceType::TypeScript) => {
            otter_vm::source::compile_module(source, url)
                .map_err(|error| VmNativeCallError::Internal(error.to_string().into()))
        }
        (ModuleType::CommonJs, _) => otter_vm::source::compile_script(source, url)
            .map_err(|error| VmNativeCallError::Internal(error.to_string().into())),
    }
}

fn ensure_namespace_object(
    runtime: &mut RuntimeState,
    helper: ObjectHandle,
    url: &str,
) -> Result<ObjectHandle, VmNativeCallError> {
    let payload = runtime
        .native_payload_mut::<ModuleRuntimePayload>(helper)
        .map_err(native_internal)?;
    let record = payload
        .modules
        .get_mut(url)
        .ok_or_else(|| VmNativeCallError::Internal(format!("module '{url}' not found").into()))?;
    if let Some(namespace) = record.namespace {
        return Ok(namespace);
    }
    let _ = record;
    let namespace = runtime.alloc_object();
    let payload = runtime
        .native_payload_mut::<ModuleRuntimePayload>(helper)
        .map_err(native_internal)?;
    let record = payload
        .modules
        .get_mut(url)
        .ok_or_else(|| VmNativeCallError::Internal(format!("module '{url}' not found").into()))?;
    record.namespace = Some(namespace);
    Ok(namespace)
}

fn ensure_cjs_exports_object(
    runtime: &mut RuntimeState,
    helper: ObjectHandle,
    url: &str,
) -> Result<ObjectHandle, VmNativeCallError> {
    let payload = runtime
        .native_payload_mut::<ModuleRuntimePayload>(helper)
        .map_err(native_internal)?;
    let record = payload
        .modules
        .get_mut(url)
        .ok_or_else(|| VmNativeCallError::Internal(format!("module '{url}' not found").into()))?;
    if let Some(exports) = record
        .cjs_exports
        .and_then(|value| value.as_object_handle())
        .map(ObjectHandle)
    {
        return Ok(exports);
    }
    let _ = record;
    let exports = runtime.alloc_object();
    let payload = runtime
        .native_payload_mut::<ModuleRuntimePayload>(helper)
        .map_err(native_internal)?;
    let record = payload
        .modules
        .get_mut(url)
        .ok_or_else(|| VmNativeCallError::Internal(format!("module '{url}' not found").into()))?;
    record.cjs_exports = Some(RegisterValue::from_object_handle(exports.0));
    Ok(exports)
}

fn namespace_value(
    runtime: &mut RuntimeState,
    helper: ObjectHandle,
    url: &str,
) -> Result<ObjectHandle, VmNativeCallError> {
    let module_type = runtime
        .native_payload::<ModuleRuntimePayload>(helper)
        .map_err(native_internal)?
        .modules
        .get(url)
        .ok_or_else(|| VmNativeCallError::Internal(format!("module '{url}' not found").into()))?
        .module_type;

    match module_type {
        ModuleType::Esm => ensure_namespace_object(runtime, helper, url),
        ModuleType::CommonJs => {
            let namespace = ensure_namespace_object(runtime, helper, url)?;
            let payload = runtime
                .native_payload::<ModuleRuntimePayload>(helper)
                .map_err(native_internal)?;
            let record = payload.modules.get(url).ok_or_else(|| {
                VmNativeCallError::Internal(format!("module '{url}' not found").into())
            })?;
            let exports = record.cjs_exports.unwrap_or_else(RegisterValue::undefined);
            let default_prop = runtime.intern_property_name("default");
            runtime
                .objects_mut()
                .set_property(namespace, default_prop, exports)
                .map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("namespace default failed: {error:?}").into(),
                    )
                })?;
            if let Some(handle) = exports.as_object_handle().map(ObjectHandle) {
                let keys = runtime.enumerable_own_property_keys(handle)?;
                for key in keys {
                    let value = runtime.own_property_value(handle, key)?;
                    runtime
                        .objects_mut()
                        .set_property(namespace, key, value)
                        .map_err(|error| {
                            VmNativeCallError::Internal(
                                format!("namespace property failed: {error:?}").into(),
                            )
                        })?;
                }
            }
            Ok(namespace)
        }
    }
}

fn commit_cjs_exports(
    runtime: &mut RuntimeState,
    helper: ObjectHandle,
    url: &str,
    exports: RegisterValue,
) -> Result<(), VmNativeCallError> {
    let payload = runtime
        .native_payload_mut::<ModuleRuntimePayload>(helper)
        .map_err(native_internal)?;
    let record = payload
        .modules
        .get_mut(url)
        .ok_or_else(|| VmNativeCallError::Internal(format!("module '{url}' not found").into()))?;
    record.cjs_exports = Some(exports);
    let needs_namespace = record.namespace.is_none();
    record.state = HostedEvaluationState::Evaluated;
    let existing_namespace = record.namespace;
    let _ = record;
    let allocated_namespace = needs_namespace.then(|| runtime.alloc_object());
    let payload = runtime
        .native_payload_mut::<ModuleRuntimePayload>(helper)
        .map_err(native_internal)?;
    let record = payload
        .modules
        .get_mut(url)
        .ok_or_else(|| VmNativeCallError::Internal(format!("module '{url}' not found").into()))?;
    if let Some(namespace) = allocated_namespace {
        record.namespace = Some(namespace);
    }
    let namespace = record
        .namespace
        .or(existing_namespace)
        .expect("namespace should exist");
    let default_prop = runtime.intern_property_name("default");
    runtime
        .objects_mut()
        .set_property(namespace, default_prop, exports)
        .map_err(|error| {
            VmNativeCallError::Internal(format!("commitCjs default failed: {error:?}").into())
        })?;
    if let Some(handle) = exports.as_object_handle().map(ObjectHandle) {
        let keys = runtime.enumerable_own_property_keys(handle)?;
        for key in keys {
            let value = runtime.own_property_value(handle, key)?;
            runtime
                .objects_mut()
                .set_property(namespace, key, value)
                .map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("commitCjs property failed: {error:?}").into(),
                    )
                })?;
        }
    }
    Ok(())
}

fn transform_module_source(
    loader: &ModuleLoader,
    node: &ModuleGraphNode,
) -> Result<String, String> {
    if node.module.url.starts_with("otter:") || matches!(node.module.source_type, SourceType::Json)
    {
        return Ok(node.module.source.clone());
    }

    match node.module.module_type {
        ModuleType::CommonJs => Ok(wrap_commonjs_source(&node.module.url, &node.module.source)?),
        ModuleType::Esm => transform_esm_source(loader, node),
    }
}

fn wrap_commonjs_source(url: &str, source: &str) -> Result<String, String> {
    let normalized = url.strip_prefix("file://").unwrap_or(url);
    let path = std::path::Path::new(normalized);
    let dirname = path
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string());
    let filename = normalized.to_string();
    let url_lit = serde_json::to_string(url).map_err(|e| e.to_string())?;
    let filename_lit = serde_json::to_string(&filename).map_err(|e| e.to_string())?;
    let dirname_lit = serde_json::to_string(&dirname).map_err(|e| e.to_string())?;

    Ok(format!(
        r#"
var __otter_cjs_url = {url_lit};
var __otter_cjs_module = __otter_module.beginCjs(__otter_cjs_url);
var module = __otter_cjs_module;
var exports = module.exports;
var require = function(specifier) {{
    return __otter_module.require(specifier, __otter_cjs_url);
}};
var __filename = {filename_lit};
var __dirname = {dirname_lit};
try {{
{source}
}} finally {{
    __otter_module.commitCjs(__otter_cjs_url, module.exports);
}}
"#
    ))
}

fn transform_esm_source(loader: &ModuleLoader, node: &ModuleGraphNode) -> Result<String, String> {
    let allocator = Allocator::default();
    let path_hint = node
        .module
        .url
        .strip_prefix("file://")
        .unwrap_or(&node.module.url);
    let mut source_type = OxcSourceType::from_path(path_hint)
        .unwrap_or_default()
        .with_module(true);
    if matches!(node.module.source_type, SourceType::TypeScript) {
        source_type = source_type.with_typescript(true);
    }
    let parsed = Parser::new(&allocator, &node.module.source, source_type).parse();
    if parsed.panicked {
        return Err(format!("failed to parse ESM source '{}'", node.module.url));
    }
    if let Some(error) = parsed.errors.first() {
        return Err(error.to_string());
    }

    let mut output = String::new();
    let mut cursor = 0usize;
    let source = &node.module.source;

    for (index, stmt) in parsed.program.body.iter().enumerate() {
        let span = stmt.span();
        let start = usize::try_from(span.start).unwrap_or(0);
        let end = usize::try_from(span.end).unwrap_or(source.len());
        output.push_str(&source[cursor..start]);

        match stmt {
            Statement::ImportDeclaration(decl) => {
                output.push_str(&emit_import_statement(
                    loader,
                    &node.module.url,
                    decl,
                    index,
                )?);
            }
            Statement::ExportNamedDeclaration(decl) => {
                output.push_str(&emit_export_named_statement(
                    loader,
                    &node.module.url,
                    source,
                    decl,
                    index,
                )?);
            }
            Statement::ExportDefaultDeclaration(decl) => {
                output.push_str(&emit_export_default_statement(
                    &node.module.url,
                    source,
                    decl,
                    index,
                )?);
            }
            Statement::ExportAllDeclaration(decl) => {
                let resolved = loader
                    .resolve_with_context(
                        &decl.source.value,
                        Some(&node.module.url),
                        ImportContext::Esm,
                    )
                    .map_err(|error| error.to_string())?;
                output.push_str(&format!(
                    "__otter_module.exportAll({}, {});\n",
                    json_string(&node.module.url)?,
                    json_string(&resolved)?
                ));
            }
            _ => output.push_str(&source[start..end]),
        }

        cursor = end;
    }

    output.push_str(&source[cursor..]);
    Ok(output)
}

fn emit_import_statement(
    loader: &ModuleLoader,
    referrer: &str,
    decl: &ImportDeclaration<'_>,
    index: usize,
) -> Result<String, String> {
    let resolved = loader
        .resolve_with_context(&decl.source.value, Some(referrer), ImportContext::Esm)
        .map_err(|error| error.to_string())?;
    let Some(specifiers) = &decl.specifiers else {
        return Ok(format!(
            "__otter_module.import({});\n",
            json_string(&resolved)?
        ));
    };
    if specifiers.is_empty() {
        return Ok(format!(
            "__otter_module.import({});\n",
            json_string(&resolved)?
        ));
    }

    let temp = format!("__otter_import_{index}");
    let mut code = format!(
        "const {temp} = __otter_module.import({});\n",
        json_string(&resolved)?
    );
    for specifier in specifiers {
        match specifier {
            ImportDeclarationSpecifier::ImportSpecifier(spec) => {
                code.push_str(&format!(
                    "const {} = {}[{}];\n",
                    spec.local.name,
                    temp,
                    json_string(spec.imported.name().as_str())?
                ));
            }
            ImportDeclarationSpecifier::ImportDefaultSpecifier(spec) => {
                code.push_str(&format!(
                    "const {} = {}[\"default\"];\n",
                    spec.local.name, temp
                ));
            }
            ImportDeclarationSpecifier::ImportNamespaceSpecifier(spec) => {
                code.push_str(&format!("const {} = {};\n", spec.local.name, temp));
            }
        }
    }
    Ok(code)
}

fn emit_export_named_statement(
    loader: &ModuleLoader,
    module_url: &str,
    source: &str,
    decl: &ExportNamedDeclaration<'_>,
    index: usize,
) -> Result<String, String> {
    let mut code = String::new();
    if let Some(declaration) = &decl.declaration {
        let span = declaration.span();
        let start = usize::try_from(span.start).unwrap_or(0);
        let end = usize::try_from(span.end).unwrap_or(source.len());
        code.push_str(&source[start..end]);
        code.push('\n');
        for name in exported_names_from_declaration(declaration) {
            code.push_str(&format!(
                "__otter_module.export({}, {}, {});\n",
                json_string(module_url)?,
                json_string(&name)?,
                name
            ));
        }
        return Ok(code);
    }

    if let Some(reexport_source) = &decl.source {
        let resolved = loader
            .resolve_with_context(&reexport_source.value, Some(module_url), ImportContext::Esm)
            .map_err(|error| error.to_string())?;
        let temp = format!("__otter_reexport_{index}");
        code.push_str(&format!(
            "const {temp} = __otter_module.import({});\n",
            json_string(&resolved)?
        ));
        for specifier in &decl.specifiers {
            let exported = specifier.exported.name();
            let local = specifier.local.name();
            code.push_str(&format!(
                "__otter_module.export({}, {}, {}[{}]);\n",
                json_string(module_url)?,
                json_string(exported.as_str())?,
                temp,
                json_string(local.as_str())?
            ));
        }
        return Ok(code);
    }

    for specifier in &decl.specifiers {
        let exported = specifier.exported.name();
        let local = specifier.local.name();
        code.push_str(&format!(
            "__otter_module.export({}, {}, {});\n",
            json_string(module_url)?,
            json_string(exported.as_str())?,
            local.as_str()
        ));
    }
    Ok(code)
}

fn emit_export_default_statement(
    module_url: &str,
    source: &str,
    decl: &ExportDefaultDeclaration<'_>,
    index: usize,
) -> Result<String, String> {
    let mut code = String::new();
    match &decl.declaration {
        ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
            let span = function.span();
            let start = usize::try_from(span.start).unwrap_or(0);
            let end = usize::try_from(span.end).unwrap_or(source.len());
            code.push_str(&source[start..end]);
            code.push('\n');
            let local = function
                .id
                .as_ref()
                .map(|id| id.name.to_string())
                .unwrap_or_else(|| format!("__otter_default_fn_{index}"));
            if function.id.is_none() {
                code = format!("const {local} = {};\n", &source[start..end]);
            }
            code.push_str(&format!(
                "__otter_module.export({}, \"default\", {});\n",
                json_string(module_url)?,
                local
            ));
        }
        ExportDefaultDeclarationKind::ClassDeclaration(class) => {
            let span = class.span();
            let start = usize::try_from(span.start).unwrap_or(0);
            let end = usize::try_from(span.end).unwrap_or(source.len());
            code.push_str(&source[start..end]);
            code.push('\n');
            let local = class
                .id
                .as_ref()
                .map(|id| id.name.to_string())
                .unwrap_or_else(|| format!("__otter_default_class_{index}"));
            if class.id.is_none() {
                code = format!("const {local} = {};\n", &source[start..end]);
            }
            code.push_str(&format!(
                "__otter_module.export({}, \"default\", {});\n",
                json_string(module_url)?,
                local
            ));
        }
        _ => {
            let expr = decl
                .declaration
                .as_expression()
                .ok_or_else(|| "unsupported export default declaration shape".to_string())?;
            let span = expr.span();
            let start = usize::try_from(span.start).unwrap_or(0);
            let end = usize::try_from(span.end).unwrap_or(source.len());
            let local = format!("__otter_default_{index}");
            code.push_str(&format!("const {local} = {};\n", &source[start..end]));
            code.push_str(&format!(
                "__otter_module.export({}, \"default\", {});\n",
                json_string(module_url)?,
                local
            ));
        }
    }
    Ok(code)
}

fn exported_names_from_declaration(declaration: &Declaration<'_>) -> Vec<String> {
    match declaration {
        Declaration::VariableDeclaration(decl) => decl
            .declarations
            .iter()
            .flat_map(|declarator| binding_names(&declarator.id))
            .collect(),
        Declaration::FunctionDeclaration(decl) => decl
            .id
            .as_ref()
            .map(|id| vec![id.name.to_string()])
            .unwrap_or_default(),
        Declaration::ClassDeclaration(decl) => decl
            .id
            .as_ref()
            .map(|id| vec![id.name.to_string()])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn binding_names(pattern: &BindingPattern<'_>) -> Vec<String> {
    match pattern {
        BindingPattern::BindingIdentifier(id) => vec![id.name.to_string()],
        BindingPattern::AssignmentPattern(assign) => binding_names(&assign.left),
        BindingPattern::ObjectPattern(object) => {
            let mut names = Vec::new();
            for property in &object.properties {
                names.extend(binding_names(&property.value));
            }
            if let Some(rest) = &object.rest {
                names.extend(binding_names(&rest.argument));
            }
            names
        }
        BindingPattern::ArrayPattern(array) => {
            let mut names = Vec::new();
            for element in array.elements.iter().flatten() {
                names.extend(binding_names(element));
            }
            if let Some(rest) = &array.rest {
                names.extend(binding_names(&rest.argument));
            }
            names
        }
    }
}

fn json_string(value: &str) -> Result<String, String> {
    serde_json::to_string(value).map_err(|error| error.to_string())
}

fn native_internal(error: impl std::fmt::Display) -> VmNativeCallError {
    VmNativeCallError::Internal(error.to_string().into())
}
