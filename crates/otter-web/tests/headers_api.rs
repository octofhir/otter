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
             var __otter = { \
               hasContentType: headers.has('content-type'), \
               contentType: headers.get('CONTENT-TYPE'), \
               xTest: headers.get('x-test'), \
             };",
            "main.js",
        )
        .expect("headers script should execute");

    let value = global_property(&mut runtime, "__otter");
    assert_eq!(
        namespace_property(&mut runtime, value, "hasContentType"),
        RegisterValue::from_bool(true)
    );
    assert_eq!(
        string_property(&mut runtime, value, "contentType"),
        "application/json"
    );
    assert_eq!(string_property(&mut runtime, value, "xTest"), "42");
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
             var __otter = { \
               joined: headers.get('x-test'), \
               cookieLength: headers.getSetCookie().length, \
               firstCookie: headers.getSetCookie()[0], \
               secondCookie: headers.getSetCookie()[1], \
             }; \
             headers.delete('x-test'); \
             __otter.hasDeleted = headers.has('x-test');",
            "main.js",
        )
        .expect("headers mutation script should execute");

    let value = global_property(&mut runtime, "__otter");
    assert_eq!(string_property(&mut runtime, value, "joined"), "3");
    assert_eq!(
        namespace_property(&mut runtime, value, "cookieLength"),
        RegisterValue::from_i32(2)
    );
    assert_eq!(string_property(&mut runtime, value, "firstCookie"), "a=1");
    assert_eq!(string_property(&mut runtime, value, "secondCookie"), "b=2");
    assert_eq!(
        namespace_property(&mut runtime, value, "hasDeleted"),
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
