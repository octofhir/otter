use std::time::{SystemTime, UNIX_EPOCH};

use otter_modules::modules_extension;
use otter_runtime::{
    CapabilitiesBuilder, ModuleLoaderConfig, ObjectHandle, OtterRuntime, RegisterValue,
};

const TEST_FFI_LIB_PATH: &str = env!("TEST_FFI_LIB_PATH");

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

fn configure_runtime(base_dir: std::path::PathBuf, allow_ffi: bool) -> OtterRuntime {
    let mut builder = OtterRuntime::builder()
        .module_loader(ModuleLoaderConfig {
            base_dir,
            ..Default::default()
        })
        .extension(modules_extension());
    if allow_ffi {
        builder = builder.capabilities(CapabilitiesBuilder::new().allow_ffi().build());
    }
    builder.build()
}

#[test]
fn esm_can_import_otter_ffi_module_and_call_symbols() {
    let dir = temp_test_dir("esm-ffi");
    let lib_path = serde_json::to_string(TEST_FFI_LIB_PATH).expect("library path should serialize");
    std::fs::write(
        dir.join("main.mjs"),
        format!(
            "import {{ dlopen, FFIType, read, suffix, ptr, CString, toArrayBuffer, toBuffer }} from 'otter:ffi'; \
             const lib = dlopen({lib_path}, {{ \
               add: {{ args: ['i32', 'i32'], returns: 'i32' }}, \
               multiply: {{ args: ['f64', 'f64'], returns: 'f64' }}, \
               hello: {{ args: [], returns: 'ptr' }}, \
               negate: {{ args: ['i32'], returns: FFIType.i32 }} \
             }}); \
             const bytes = new Uint8Array([7, 8, 9]); \
             const bytePtr = ptr(bytes); \
             const helloPtr = lib.symbols.hello(); \
             export const add = lib.symbols.add(40, 2); \
             export const multiply = lib.symbols.multiply(6, 7); \
             export const hello = read.cstring(helloPtr); \
             export const helloViaCtor = CString(helloPtr); \
             export const secondByte = read.u8(bytePtr, 1); \
             export const firstChar = new Uint8Array(toArrayBuffer(helloPtr, 0, 1))[0]; \
             export const secondChar = new Uint8Array(toBuffer(helloPtr, 1, 1))[0]; \
             export const negated = lib.symbols.negate(5); \
             export const moduleSuffix = suffix; \
             export default add + negated;"
        ),
    )
    .expect("main should write");

    let mut runtime = configure_runtime(dir, true);
    let result = runtime
        .run_entry_specifier("./main.mjs", None)
        .expect("esm otter:ffi import should execute");

    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "add"),
        RegisterValue::from_i32(42)
    );
    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "multiply"),
        RegisterValue::from_number(42.0)
    );
    let hello = namespace_property(&mut runtime, result.return_value(), "hello");
    assert_eq!(
        runtime
            .state_mut()
            .js_to_string_infallible(hello)
            .into_string(),
        "Hello from C!"
    );
    let hello_via_ctor = namespace_property(&mut runtime, result.return_value(), "helloViaCtor");
    assert_eq!(
        runtime
            .state_mut()
            .js_to_string_infallible(hello_via_ctor)
            .into_string(),
        "Hello from C!"
    );
    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "secondByte"),
        RegisterValue::from_i32(8)
    );
    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "firstChar"),
        RegisterValue::from_i32(i32::from(b'H'))
    );
    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "secondChar"),
        RegisterValue::from_i32(i32::from(b'e'))
    );
    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "negated"),
        RegisterValue::from_i32(-5)
    );
    let suffix = namespace_property(&mut runtime, result.return_value(), "moduleSuffix");
    let suffix = runtime
        .state_mut()
        .js_to_string_infallible(suffix)
        .into_string();
    assert!(!suffix.is_empty());
    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "default"),
        RegisterValue::from_i32(37)
    );
}

#[test]
fn commonjs_can_require_otter_ffi_module() {
    let dir = temp_test_dir("cjs-ffi");
    std::fs::write(
        dir.join("main.cjs"),
        "const ffi = require('otter:ffi'); module.exports = typeof ffi.dlopen === 'function' && ffi.FFIType.i32 === 5 && typeof ffi.read.cstring === 'function';",
    )
    .expect("main should write");

    let mut runtime = configure_runtime(dir, true);
    let result = runtime
        .run_entry_specifier("./main.cjs", None)
        .expect("cjs otter:ffi require should execute");

    assert_eq!(result.return_value(), RegisterValue::from_bool(true));
}

#[test]
fn ffi_library_exposes_close_and_closed_state() {
    let dir = temp_test_dir("ffi-close");
    let lib_path = serde_json::to_string(TEST_FFI_LIB_PATH).expect("library path should serialize");
    std::fs::write(
        dir.join("main.mjs"),
        format!(
            "import {{ dlopen }} from 'otter:ffi'; \
             const lib = dlopen({lib_path}, {{ add: {{ args: ['i32', 'i32'], returns: 'i32' }} }}); \
             const before = lib.closed; \
             lib.close(); \
             export default {{ before, after: lib.closed }};"
        ),
    )
    .expect("main should write");

    let mut runtime = configure_runtime(dir, true);
    let result = runtime
        .run_entry_specifier("./main.mjs", None)
        .expect("ffi close script should execute");

    let default = namespace_property(&mut runtime, result.return_value(), "default");
    assert_eq!(
        namespace_property(&mut runtime, default, "before"),
        RegisterValue::from_bool(false)
    );
    assert_eq!(
        namespace_property(&mut runtime, default, "after"),
        RegisterValue::from_bool(true)
    );
}

#[test]
fn ffi_can_create_cfunction_and_link_symbols_from_raw_pointers() {
    let dir = temp_test_dir("ffi-cfunction-linksymbols");
    let lib_path = serde_json::to_string(TEST_FFI_LIB_PATH).expect("library path should serialize");
    std::fs::write(
        dir.join("main.mjs"),
        format!(
            "import ffi from 'otter:ffi'; \
             const meta = ffi.dlopen({lib_path}, {{ \
               add_ptr: {{ args: [], returns: 'ptr' }}, \
               negate_ptr: {{ args: [], returns: 'ptr' }} \
             }}); \
             const add = ffi.CFunction({{ ptr: meta.symbols.add_ptr(), args: ['i32', 'i32'], returns: 'i32' }}); \
             const linked = ffi.linkSymbols({{ \
               negate: {{ ptr: meta.symbols.negate_ptr(), args: ['i32'], returns: 'i32' }} \
             }}); \
             export const addResult = add(10, 32); \
             export const negateResult = linked.symbols.negate(9); \
             linked.close(); \
             meta.close(); \
             export default addResult + negateResult;"
        ),
    )
    .expect("main should write");

    let mut runtime = configure_runtime(dir, true);
    let result = runtime
        .run_entry_specifier("./main.mjs", None)
        .expect("ffi CFunction/linkSymbols should execute");

    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "addResult"),
        RegisterValue::from_i32(42)
    );
    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "negateResult"),
        RegisterValue::from_i32(-9)
    );
    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "default"),
        RegisterValue::from_i32(33)
    );
}

#[test]
fn ffi_can_use_js_callback_through_native_function_arguments() {
    let dir = temp_test_dir("ffi-jscallback");
    let lib_path = serde_json::to_string(TEST_FFI_LIB_PATH).expect("library path should serialize");
    std::fs::write(
        dir.join("main.mjs"),
        format!(
            "import ffi from 'otter:ffi'; \
             const lib = ffi.dlopen({lib_path}, {{ \
               apply_binop: {{ args: ['i32', 'i32', 'function'], returns: 'i32' }}, \
               transform_array: {{ args: ['ptr', 'i32', 'function'], returns: 'void' }} \
             }}); \
             const sum = ffi.JSCallback((a, b) => a + b, {{ args: ['i32', 'i32'], returns: 'i32' }}); \
             const double = ffi.JSCallback((x) => x * 2, {{ args: ['i32'], returns: 'i32' }}); \
             const values = new Int32Array([1, 2, 3]); \
             export const binop = lib.symbols.apply_binop(19, 23, sum); \
             lib.symbols.transform_array(ffi.ptr(values), values.length, double); \
             export const first = values[0]; \
             export const second = values[1]; \
             export const third = values[2]; \
             export const threadsafe = sum.threadsafe; \
             sum.close(); \
             double.close(); \
             lib.close(); \
             export default binop + values[0] + values[1] + values[2];"
        ),
    )
    .expect("main should write");

    let mut runtime = configure_runtime(dir, true);
    let result = runtime
        .run_entry_specifier("./main.mjs", None)
        .expect("ffi JSCallback should execute");

    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "binop"),
        RegisterValue::from_i32(42)
    );
    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "first"),
        RegisterValue::from_i32(2)
    );
    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "second"),
        RegisterValue::from_i32(4)
    );
    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "third"),
        RegisterValue::from_i32(6)
    );
    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "threadsafe"),
        RegisterValue::from_bool(false)
    );
    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "default"),
        RegisterValue::from_i32(54)
    );
}

#[test]
fn ffi_access_requires_allow_ffi() {
    let dir = temp_test_dir("ffi-permission");
    let lib_path = serde_json::to_string(TEST_FFI_LIB_PATH).expect("library path should serialize");
    std::fs::write(
        dir.join("main.mjs"),
        format!(
            "import {{ dlopen }} from 'otter:ffi'; dlopen({lib_path}, {{ add: {{ args: ['i32', 'i32'], returns: 'i32' }} }});"
        ),
    )
    .expect("main should write");

    let mut runtime = configure_runtime(dir, false);
    let error = runtime
        .run_entry_specifier("./main.mjs", None)
        .expect_err("ffi access should be denied without capabilities");

    assert!(error.to_string().contains("RuntimeError:"));
}
