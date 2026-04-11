use otter_macros::{dive, lodge};
use otter_runtime::{
    HostedNativeModule, HostedNativeModuleLoader, ObjectHandle, RegisterValue, RuntimeState,
};

#[dive(name = "ping", length = 0)]
fn ping(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut RuntimeState,
) -> Result<RegisterValue, otter_runtime::VmNativeCallError> {
    Ok(RegisterValue::from_i32(7))
}

lodge!(
    demo_module,
    module_specifiers = ["otter:demo", "demo"],
    default = object,
    functions = [("ping", ping)],
    values = [("answer", RegisterValue::from_i32(42))],
);

lodge!(
    demo_commonjs_module,
    module_specifiers = ["node:demo-commonjs"],
    kind = commonjs,
    default = value(RegisterValue::from_i32(42)),
);

#[test]
fn lodge_generates_hosted_module_loader_for_active_runtime() {
    let entries = demo_module_entries();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].specifier, "otter:demo");
    assert_eq!(entries[1].specifier, "demo");

    let mut runtime = RuntimeState::new();
    let module = DemoModule;
    let HostedNativeModule::Esm(namespace) = module.load(&mut runtime).expect("module should load")
    else {
        panic!("expected esm module");
    };

    let default_prop = runtime.intern_property_name("default");
    let answer_prop = runtime.intern_property_name("answer");
    let ping_prop = runtime.intern_property_name("ping");
    let length_prop = runtime.intern_property_name("length");
    let name_prop = runtime.intern_property_name("name");

    let default = runtime
        .own_property_value(namespace, default_prop)
        .expect("default export should exist");
    let default = default
        .as_object_handle()
        .map(ObjectHandle)
        .expect("default export should be object");

    let answer = runtime
        .own_property_value(namespace, answer_prop)
        .expect("answer export should exist");
    assert_eq!(answer, RegisterValue::from_i32(42));
    assert_eq!(
        runtime
            .own_property_value(default, answer_prop)
            .expect("default object should mirror answer"),
        RegisterValue::from_i32(42)
    );

    let ping_handle = runtime
        .own_property_value(namespace, ping_prop)
        .expect("ping export should exist")
        .as_object_handle()
        .map(ObjectHandle)
        .expect("ping export should be object");
    let default_ping_handle = runtime
        .own_property_value(default, ping_prop)
        .expect("default object should mirror ping")
        .as_object_handle()
        .map(ObjectHandle)
        .expect("mirrored ping should be object");

    let ping_length = runtime
        .own_property_value(ping_handle, length_prop)
        .expect("function length should exist");
    assert_eq!(ping_length, RegisterValue::from_i32(0));

    let default_ping_name = runtime
        .own_property_value(default_ping_handle, name_prop)
        .expect("function name should exist");
    assert_eq!(
        runtime
            .js_to_string_infallible(default_ping_name)
            .into_string(),
        "ping"
    );
}

#[test]
fn lodge_can_generate_commonjs_loader() {
    let mut runtime = RuntimeState::new();
    let module = DemoCommonjsModule;
    let HostedNativeModule::CommonJs(exports) =
        module.load(&mut runtime).expect("module should load")
    else {
        panic!("expected commonjs module");
    };

    assert_eq!(exports, RegisterValue::from_i32(42));
}
