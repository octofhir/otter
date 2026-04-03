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

fn configure_runtime() -> OtterRuntime {
    OtterRuntime::builder()
        .module_loader(ModuleLoaderConfig::default())
        .extension(web_extension())
        .build()
}

#[test]
fn text_encoder_and_decoder_work_on_new_web_extension() {
    let mut runtime = configure_runtime();
    runtime
        .run_script(
            "const enc = new TextEncoder(); \
             const dec = new TextDecoder(); \
             const bytes = enc.encode('Привет'); \
             var __otter = { \
               encName: enc.encoding, \
               decName: dec.encoding, \
               first: bytes[0], \
               len: bytes.length, \
               roundtrip: dec.decode(bytes), \
             };",
            "main.js",
        )
        .expect("text codec script should execute");

    let value = global_property(&mut runtime, "__otter");
    let enc_name = namespace_property(&mut runtime, value, "encName");
    assert_eq!(
        runtime
            .state_mut()
            .js_to_string_infallible(enc_name)
            .into_string(),
        "utf-8"
    );
    let dec_name = namespace_property(&mut runtime, value, "decName");
    assert_eq!(
        runtime
            .state_mut()
            .js_to_string_infallible(dec_name)
            .into_string(),
        "utf-8"
    );
    assert_eq!(
        namespace_property(&mut runtime, value, "first"),
        RegisterValue::from_i32(208)
    );
    assert_eq!(
        namespace_property(&mut runtime, value, "len"),
        RegisterValue::from_i32(12)
    );
    let roundtrip = namespace_property(&mut runtime, value, "roundtrip");
    assert_eq!(
        runtime
            .state_mut()
            .js_to_string_infallible(roundtrip)
            .into_string(),
        "Привет"
    );
}

#[test]
fn text_decoder_supports_array_buffer_and_options() {
    let mut runtime = configure_runtime();
    runtime
        .run_script(
            "const bytes = new Uint8Array([0xEF, 0xBB, 0xBF, 65, 66]); \
             const buffer = bytes.buffer; \
             const dec = new TextDecoder('utf-8', { fatal: true, ignoreBOM: false }); \
             var __otter = { \
               fatal: dec.fatal, \
               ignoreBOM: dec.ignoreBOM, \
               decoded: dec.decode(buffer), \
             };",
            "main.js",
        )
        .expect("text decoder options script should execute");

    let value = global_property(&mut runtime, "__otter");
    assert_eq!(
        namespace_property(&mut runtime, value, "fatal"),
        RegisterValue::from_bool(true)
    );
    assert_eq!(
        namespace_property(&mut runtime, value, "ignoreBOM"),
        RegisterValue::from_bool(false)
    );
    let decoded = namespace_property(&mut runtime, value, "decoded");
    assert_eq!(
        runtime
            .state_mut()
            .js_to_string_infallible(decoded)
            .into_string(),
        "AB"
    );
}
