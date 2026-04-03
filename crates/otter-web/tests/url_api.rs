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
fn url_exposes_basic_components() {
    let mut runtime = configure_runtime();
    runtime
        .run_script(
            "const url = new URL('/a/b?x=1#hash', 'https://example.com:8080/root'); \
             var __otter = { \
               href: url.href, \
               protocol: url.protocol, \
               host: url.host, \
               hostname: url.hostname, \
               port: url.port, \
               pathname: url.pathname, \
               search: url.search, \
               hash: url.hash, \
               origin: url.origin, \
               stringified: url.toString(), \
             };",
            "main.js",
        )
        .expect("url script should execute");

    let value = global_property(&mut runtime, "__otter");
    for (name, expected) in [
        ("href", "https://example.com:8080/a/b?x=1#hash"),
        ("protocol", "https:"),
        ("host", "example.com:8080"),
        ("hostname", "example.com"),
        ("port", "8080"),
        ("pathname", "/a/b"),
        ("search", "?x=1"),
        ("hash", "#hash"),
        ("origin", "https://example.com:8080"),
        ("stringified", "https://example.com:8080/a/b?x=1#hash"),
    ] {
        let actual = string_property(&mut runtime, value, name);
        assert_eq!(actual, expected, "{name} should match");
    }
}

#[test]
fn url_search_params_supports_standalone_and_live_url_binding() {
    let mut runtime = configure_runtime();
    runtime
        .run_script(
            "const params = new URLSearchParams('?a=1&a=2'); \
             params.append('b', '3'); \
             params.set('a', '4'); \
             const url = new URL('https://example.com/?x=1'); \
             const searchParams = url.searchParams; \
             searchParams.append('y', '2'); \
             searchParams.set('x', '5'); \
             var __otter = { \
               standalone: params.toString(), \
               getA: params.get('a'), \
               allA: params.getAll('a'), \
               hasB: params.has('b'), \
               sameObject: searchParams === url.searchParams, \
               linked: searchParams.toString(), \
               search: url.search, \
               href: url.href, \
             };",
            "main.js",
        )
        .expect("url search params script should execute");

    let value = global_property(&mut runtime, "__otter");
    let standalone = string_property(&mut runtime, value, "standalone");
    assert_eq!(standalone, "a=4&b=3");
    let get_a = string_property(&mut runtime, value, "getA");
    assert_eq!(get_a, "4");

    let all_a = namespace_property(&mut runtime, value, "allA");
    assert_eq!(
        namespace_property(&mut runtime, all_a, "length"),
        RegisterValue::from_i32(1)
    );
    assert_eq!(
        namespace_property(&mut runtime, value, "hasB"),
        RegisterValue::from_bool(true)
    );
    assert_eq!(
        namespace_property(&mut runtime, value, "sameObject"),
        RegisterValue::from_bool(true)
    );
    let linked = string_property(&mut runtime, value, "linked");
    assert_eq!(linked, "x=5&y=2");
    let search = string_property(&mut runtime, value, "search");
    assert_eq!(search, "?x=5&y=2");
    let href = string_property(&mut runtime, value, "href");
    assert_eq!(href, "https://example.com/?x=5&y=2");
}
