use std::time::{SystemTime, UNIX_EPOCH};

use otter_runtime::{
    HostedExtension, HostedExtensionModule, HostedNativeModule, HostedNativeModuleKind,
    HostedNativeModuleLoader, ModuleLoaderConfig, ObjectHandle, OtterRuntime, RegisterValue,
    RuntimeProfile, RuntimeState,
};

fn temp_test_dir(name: &str) -> std::path::PathBuf {
    let mut dir = std::env::temp_dir();
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should move forward")
        .as_nanos();
    dir.push(format!("otter-runtime-{name}-{unique}"));
    std::fs::create_dir_all(&dir).expect("temp dir should exist");
    dir
}

fn configure_runtime(base_dir: std::path::PathBuf) -> OtterRuntime {
    OtterRuntime::builder()
        .module_loader(ModuleLoaderConfig {
            base_dir,
            ..Default::default()
        })
        .build()
}

#[derive(Debug)]
struct NativeEsmTestModule;

impl HostedNativeModuleLoader for NativeEsmTestModule {
    fn load(&self, runtime: &mut RuntimeState) -> Result<HostedNativeModule, String> {
        let namespace = runtime.alloc_object();
        let default = runtime.alloc_object();
        let answer = runtime.intern_property_name("answer");
        let default_prop = runtime.intern_property_name("default");
        runtime
            .objects_mut()
            .set_property(default, answer, RegisterValue::from_i32(40))
            .map_err(|error| format!("failed to populate native ESM default export: {error:?}"))?;
        runtime
            .objects_mut()
            .set_property(namespace, answer, RegisterValue::from_i32(40))
            .map_err(|error| format!("failed to populate native ESM module: {error:?}"))?;
        runtime
            .objects_mut()
            .set_property(
                namespace,
                default_prop,
                RegisterValue::from_object_handle(default.0),
            )
            .map_err(|error| format!("failed to populate native ESM default binding: {error:?}"))?;
        Ok(HostedNativeModule::Esm(namespace))
    }
}

#[derive(Debug)]
struct NativeCjsTestModule;

impl HostedNativeModuleLoader for NativeCjsTestModule {
    fn kind(&self) -> HostedNativeModuleKind {
        HostedNativeModuleKind::CommonJs
    }

    fn load(&self, runtime: &mut RuntimeState) -> Result<HostedNativeModule, String> {
        let exports = runtime.alloc_object();
        let answer = runtime.intern_property_name("answer");
        runtime
            .objects_mut()
            .set_property(exports, answer, RegisterValue::from_i32(40))
            .map_err(|error| format!("failed to populate native CJS module: {error:?}"))?;
        Ok(HostedNativeModule::CommonJs(
            RegisterValue::from_object_handle(exports.0),
        ))
    }
}

#[derive(Debug)]
struct HostedExtensionTestModule;

impl HostedNativeModuleLoader for HostedExtensionTestModule {
    fn load(&self, runtime: &mut RuntimeState) -> Result<HostedNativeModule, String> {
        let namespace = runtime.alloc_object();
        let default = runtime.alloc_object();
        let answer = runtime.intern_property_name("answer");
        let default_prop = runtime.intern_property_name("default");
        runtime
            .objects_mut()
            .set_property(default, answer, RegisterValue::from_i32(40))
            .map_err(|error| format!("failed to populate extension default export: {error:?}"))?;
        runtime
            .objects_mut()
            .set_property(namespace, answer, RegisterValue::from_i32(40))
            .map_err(|error| format!("failed to populate extension namespace: {error:?}"))?;
        runtime
            .objects_mut()
            .set_property(
                namespace,
                default_prop,
                RegisterValue::from_object_handle(default.0),
            )
            .map_err(|error| format!("failed to populate extension default binding: {error:?}"))?;
        Ok(HostedNativeModule::Esm(namespace))
    }
}

#[derive(Debug)]
struct HostedTestExtension;

impl HostedExtension for HostedTestExtension {
    fn name(&self) -> &str {
        "test-extension"
    }

    fn profiles(&self) -> &[RuntimeProfile] {
        &[RuntimeProfile::Full]
    }

    fn install(&self, runtime: &mut RuntimeState) -> Result<(), String> {
        runtime.install_global_value("__ext_answer", RegisterValue::from_i32(40));
        Ok(())
    }

    fn native_modules(&self) -> Vec<HostedExtensionModule> {
        vec![HostedExtensionModule {
            specifier: "otter:test-extension".to_string(),
            loader: std::sync::Arc::new(HostedExtensionTestModule),
        }]
    }
}

fn namespace_property(
    runtime: &mut OtterRuntime,
    value: RegisterValue,
    name: &str,
) -> RegisterValue {
    let object = value
        .as_object_handle()
        .map(ObjectHandle)
        .expect("value should be an object");
    let property = runtime.state_mut().intern_property_name(name);
    runtime
        .state_mut()
        .own_property_value(object, property)
        .expect("object property should be readable")
}

#[test]
fn executes_multi_module_esm_graph() {
    let dir = temp_test_dir("esm-graph");
    std::fs::write(dir.join("dep.mjs"), "export const value = 40;").expect("dep should write");
    std::fs::write(
        dir.join("main.mjs"),
        "import { value } from './dep.mjs'; export default value + 2;",
    )
    .expect("main should write");

    let mut runtime = configure_runtime(dir);
    let result = runtime
        .run_entry_specifier("./main.mjs", None)
        .expect("esm graph should execute");

    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "default"),
        RegisterValue::from_i32(42)
    );
}

#[test]
fn executes_commonjs_require_graph() {
    let dir = temp_test_dir("cjs-graph");
    std::fs::write(dir.join("dep.cjs"), "module.exports = { value: 40 };")
        .expect("dep should write");
    std::fs::write(
        dir.join("main.cjs"),
        "const dep = require('./dep.cjs'); module.exports = dep.value + 2;",
    )
    .expect("main should write");

    let mut runtime = configure_runtime(dir);
    let result = runtime
        .run_entry_specifier("./main.cjs", None)
        .expect("cjs graph should execute");

    assert_eq!(result.return_value(), RegisterValue::from_i32(42));
}

#[test]
fn esm_can_import_commonjs_namespace() {
    let dir = temp_test_dir("esm-import-cjs");
    std::fs::write(
        dir.join("dep.cjs"),
        "module.exports = { answer: 40, extra: 2 };",
    )
    .expect("dep should write");
    std::fs::write(
        dir.join("main.mjs"),
        "import dep, { answer, extra } from './dep.cjs'; export default dep.answer + answer + extra;",
    )
    .expect("main should write");

    let mut runtime = configure_runtime(dir);
    let result = runtime
        .run_entry_specifier("./main.mjs", None)
        .expect("esm importing cjs should execute");

    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "default"),
        RegisterValue::from_i32(82)
    );
}

#[test]
fn commonjs_can_require_esm_namespace() {
    let dir = temp_test_dir("cjs-require-esm");
    std::fs::write(
        dir.join("dep.mjs"),
        "export const extra = 2; export default 40;",
    )
    .expect("dep should write");
    std::fs::write(
        dir.join("main.cjs"),
        "const dep = require('./dep.mjs'); module.exports = dep.default + dep.extra;",
    )
    .expect("main should write");

    let mut runtime = configure_runtime(dir);
    let result = runtime
        .run_entry_specifier("./main.cjs", None)
        .expect("cjs requiring esm should execute");

    assert_eq!(result.return_value(), RegisterValue::from_i32(42));
}

#[test]
fn executes_json_entry_module() {
    let dir = temp_test_dir("json-entry");
    std::fs::write(dir.join("data.json"), r#"{"answer":40,"extra":2}"#).expect("json should write");

    let mut runtime = configure_runtime(dir);
    let result = runtime
        .run_entry_specifier("./data.json", None)
        .expect("json entry should execute");

    let default = namespace_property(&mut runtime, result.return_value(), "default");
    assert_eq!(
        namespace_property(&mut runtime, default, "answer"),
        RegisterValue::from_i32(40)
    );
    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "extra"),
        RegisterValue::from_i32(2)
    );
}

#[test]
fn commonjs_can_require_json_module() {
    let dir = temp_test_dir("cjs-require-json");
    std::fs::write(dir.join("data.json"), r#"{"answer":40}"#).expect("json should write");
    std::fs::write(
        dir.join("main.cjs"),
        "const data = require('./data.json'); module.exports = data.answer + 2;",
    )
    .expect("main should write");

    let mut runtime = configure_runtime(dir);
    let result = runtime
        .run_entry_specifier("./main.cjs", None)
        .expect("cjs requiring json should execute");

    assert_eq!(result.return_value(), RegisterValue::from_i32(42));
}

#[test]
fn esm_can_import_json_default() {
    let dir = temp_test_dir("esm-import-json");
    std::fs::write(dir.join("data.json"), r#"{"answer":40}"#).expect("json should write");
    std::fs::write(
        dir.join("main.mjs"),
        "import data from './data.json'; export default data.answer + 2;",
    )
    .expect("main should write");

    let mut runtime = configure_runtime(dir);
    let result = runtime
        .run_entry_specifier("./main.mjs", None)
        .expect("esm importing json should execute");

    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "default"),
        RegisterValue::from_i32(42)
    );
}

#[test]
fn esm_can_import_native_hosted_module() {
    let dir = temp_test_dir("esm-native");
    std::fs::write(
        dir.join("main.mjs"),
        "import native from 'otter:test-native'; export default native.answer + 2;",
    )
    .expect("main should write");

    let mut runtime = OtterRuntime::builder()
        .module_loader(ModuleLoaderConfig {
            base_dir: dir,
            ..Default::default()
        })
        .native_module("otter:test-native", NativeEsmTestModule)
        .build();

    let result = runtime
        .run_entry_specifier("./main.mjs", None)
        .expect("esm importing native hosted module should execute");

    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "default"),
        RegisterValue::from_i32(42)
    );
}

#[test]
fn commonjs_can_require_native_hosted_module() {
    let dir = temp_test_dir("cjs-native");
    std::fs::write(
        dir.join("main.cjs"),
        "const native = require('otter:test-native-cjs'); module.exports = native.answer + 2;",
    )
    .expect("main should write");

    let mut runtime = OtterRuntime::builder()
        .module_loader(ModuleLoaderConfig {
            base_dir: dir,
            ..Default::default()
        })
        .native_module("otter:test-native-cjs", NativeCjsTestModule)
        .build();

    let result = runtime
        .run_entry_specifier("./main.cjs", None)
        .expect("cjs requiring native hosted module should execute");

    assert_eq!(result.return_value(), RegisterValue::from_i32(42));
}

#[test]
fn extension_can_install_global_and_otter_namespace_module() {
    let dir = temp_test_dir("extension-registry");
    std::fs::write(
        dir.join("main.mjs"),
        "import ext from 'otter:test-extension'; export default ext.answer + __ext_answer + 2;",
    )
    .expect("main should write");

    let mut runtime = OtterRuntime::builder()
        .profile(RuntimeProfile::Full)
        .module_loader(ModuleLoaderConfig {
            base_dir: dir,
            ..Default::default()
        })
        .extension(HostedTestExtension)
        .build();

    let result = runtime
        .run_entry_specifier("./main.mjs", None)
        .expect("extension-backed otter namespace module should execute");

    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "default"),
        RegisterValue::from_i32(82)
    );
}
