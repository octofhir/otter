use otter_runtime::{ModuleLoaderConfig, ObjectHandle, OtterRuntime, RegisterValue};
use otter_web::web_extension;

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
        .expect("property should exist")
}

fn global_property(runtime: &mut OtterRuntime, name: &str) -> RegisterValue {
    let global = runtime.state_mut().intrinsics().global_object();
    let property = runtime.state_mut().intern_property_name(name);
    runtime
        .state_mut()
        .own_property_value(global, property)
        .expect("global property should exist")
}

fn string_property(runtime: &mut OtterRuntime, object: RegisterValue, name: &str) -> String {
    let value = namespace_property(runtime, object, name);
    runtime
        .state_mut()
        .js_to_string_infallible(value)
        .into_string()
}

fn string_global(runtime: &mut OtterRuntime, name: &str) -> String {
    let value = global_property(runtime, name);
    runtime
        .state_mut()
        .js_to_string_infallible(value)
        .into_string()
}

fn configure_runtime() -> OtterRuntime {
    OtterRuntime::builder()
        .module_loader(ModuleLoaderConfig::default())
        .extension(web_extension())
        .build()
}

#[test]
fn headers_support_record_init_and_normalization() {
    let mut runtime = configure_runtime();
    runtime
        .run_script(
            "const headers = new Headers({ 'Content-Type': ' application/json ', 'X-Test': '42' }); \
             var __hasContentType = headers.has('content-type'); \
             var __contentType = headers.get('CONTENT-TYPE'); \
             var __xTest = headers.get('x-test');",
            "main.js",
        )
        .expect("headers script should execute");

    assert_eq!(
        global_property(&mut runtime, "__hasContentType"),
        RegisterValue::from_bool(true)
    );
    assert_eq!(
        string_global(&mut runtime, "__contentType"),
        "application/json"
    );
    assert_eq!(string_global(&mut runtime, "__xTest"), "42");
}

#[test]
fn headers_support_sequence_init_and_mutation_methods() {
    let mut runtime = configure_runtime();
    runtime
        .run_script(
            "const headers = new Headers([['Set-Cookie', 'a=1'], ['X-Test', '1']]); \
             headers.append('set-cookie', 'b=2'); \
             headers.append('x-test', '2'); \
             headers.set('x-test', '3'); \
             var __joined = headers.get('x-test'); \
             var __cookieLength = headers.getSetCookie().length; \
             var __firstCookie = headers.getSetCookie()[0]; \
             var __secondCookie = headers.getSetCookie()[1]; \
             headers.delete('x-test'); \
             var __hasDeleted = headers.has('x-test');",
            "main.js",
        )
        .expect("headers mutation script should execute");

    assert_eq!(string_global(&mut runtime, "__joined"), "3");
    assert_eq!(
        global_property(&mut runtime, "__cookieLength"),
        RegisterValue::from_i32(2)
    );
    assert_eq!(string_global(&mut runtime, "__firstCookie"), "a=1");
    assert_eq!(string_global(&mut runtime, "__secondCookie"), "b=2");
    assert_eq!(
        global_property(&mut runtime, "__hasDeleted"),
        RegisterValue::from_bool(false)
    );
}

#[test]
fn headers_can_clone_from_existing_headers() {
    let mut runtime = configure_runtime();
    runtime
        .run_script(
            "const original = new Headers({ 'X-Test': '1' }); \
             const clone = new Headers(original); \
             clone.append('x-test', '2'); \
             var __otter = { \
               original: original.get('x-test'), \
               clone: clone.get('x-test'), \
             };",
            "main.js",
        )
        .expect("headers clone script should execute");

    let value = global_property(&mut runtime, "__otter");
    assert_eq!(string_property(&mut runtime, value, "original"), "1");
    assert_eq!(string_property(&mut runtime, value, "clone"), "1, 2");
}
