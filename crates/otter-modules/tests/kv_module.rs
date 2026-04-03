use std::time::{SystemTime, UNIX_EPOCH};

use otter_modules::modules_extension;
use otter_runtime::{ModuleLoaderConfig, ObjectHandle, OtterRuntime, RegisterValue};

fn temp_test_dir(name: &str) -> std::path::PathBuf {
    let mut dir = std::env::temp_dir();
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should move forward")
        .as_nanos();
    dir.push(format!("otter-modules-{name}-{unique}"));
    std::fs::create_dir_all(&dir).expect("temp dir should exist");
    dir
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
fn esm_can_import_otter_kv_module() {
    let dir = temp_test_dir("esm-kv");
    std::fs::write(
        dir.join("main.mjs"),
        "import { kv } from 'otter:kv'; const store = kv(':memory:'); store.set('answer', { value: 40, nested: { ok: true }, list: [1, 2] }); const entry = store.get('answer'); export default entry.value + entry.list[1];",
    )
    .expect("main should write");

    let mut runtime = OtterRuntime::builder()
        .module_loader(ModuleLoaderConfig {
            base_dir: dir,
            ..Default::default()
        })
        .extension(modules_extension())
        .build();

    let result = runtime
        .run_entry_specifier("./main.mjs", None)
        .expect("esm otter:kv import should execute");

    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "default"),
        RegisterValue::from_i32(42)
    );
}

#[test]
fn commonjs_can_require_otter_kv_module() {
    let dir = temp_test_dir("cjs-kv");
    std::fs::write(
        dir.join("main.cjs"),
        "const kv = require('otter:kv').default; const store = kv(':memory:'); store.set('name', 'otter'); module.exports = store.get('name');",
    )
    .expect("main should write");

    let mut runtime = OtterRuntime::builder()
        .module_loader(ModuleLoaderConfig {
            base_dir: dir,
            ..Default::default()
        })
        .extension(modules_extension())
        .build();

    let result = runtime
        .run_entry_specifier("./main.cjs", None)
        .expect("cjs otter:kv require should execute");

    assert_eq!(
        runtime
            .state_mut()
            .js_to_string_infallible(result.return_value())
            .into_string(),
        "otter"
    );
}

#[test]
fn kv_store_exposes_management_properties_and_close_state() {
    let dir = temp_test_dir("kv-management");
    std::fs::write(
        dir.join("main.mjs"),
        "import openKv from 'otter:kv'; const store = openKv(':memory:'); store.set('a', 1); store.set('b', 2); const before = store.size + (store.isMemory ? 10 : 0) + (store.closed ? 100 : 0) + store.path.length; store.close(); export default before + (store.closed ? 1000 : 0);",
    )
    .expect("main should write");

    let mut runtime = OtterRuntime::builder()
        .module_loader(ModuleLoaderConfig {
            base_dir: dir,
            ..Default::default()
        })
        .extension(modules_extension())
        .build();

    let result = runtime
        .run_entry_specifier("./main.mjs", None)
        .expect("kv management script should execute");

    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "default"),
        RegisterValue::from_i32(1020)
    );
}

#[test]
fn kv_store_throws_after_close() {
    let dir = temp_test_dir("kv-close-error");
    std::fs::write(
        dir.join("main.mjs"),
        "import { kv } from 'otter:kv'; const store = kv(':memory:'); store.close(); store.get('a'); export default 1;",
    )
    .expect("main should write");

    let mut runtime = OtterRuntime::builder()
        .module_loader(ModuleLoaderConfig {
            base_dir: dir,
            ..Default::default()
        })
        .extension(modules_extension())
        .build();

    let error = runtime
        .run_entry_specifier("./main.mjs", None)
        .expect_err("closed store should throw");
    assert!(error.to_string().contains("RuntimeError:"));
}
