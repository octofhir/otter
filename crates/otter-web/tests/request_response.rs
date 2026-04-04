use std::sync::Arc;

use otter_runtime::{ModuleLoaderConfig, ObjectHandle, OtterRuntime, RegisterValue};
use otter_vm::console::CaptureConsoleBackend;
use otter_web::web_extension;

fn configure_runtime() -> OtterRuntime {
    OtterRuntime::builder()
        .module_loader(ModuleLoaderConfig::default())
        .extension(web_extension())
        .build()
}

fn configure_runtime_with_capture() -> (OtterRuntime, Arc<CaptureConsoleBackend>) {
    let capture = Arc::new(CaptureConsoleBackend::new());
    let runtime = OtterRuntime::builder()
        .module_loader(ModuleLoaderConfig::default())
        .console(CaptureForTest(capture.clone()))
        .extension(web_extension())
        .build();
    (runtime, capture)
}

struct CaptureForTest(Arc<CaptureConsoleBackend>);

impl otter_vm::console::ConsoleBackend for CaptureForTest {
    fn log(&self, msg: &str) {
        self.0.log(msg);
    }

    fn warn(&self, msg: &str) {
        self.0.warn(msg);
    }

    fn error(&self, msg: &str) {
        self.0.error(msg);
    }
}

fn global_property(runtime: &mut OtterRuntime, name: &str) -> RegisterValue {
    let global = runtime.state_mut().intrinsics().global_object();
    let property = runtime.state_mut().intern_property_name(name);
    runtime
        .state_mut()
        .own_property_value(global, property)
        .expect("global property should exist")
}

fn global_string_property(runtime: &mut OtterRuntime, name: &str) -> String {
    let value = global_property(runtime, name);
    runtime
        .state_mut()
        .js_to_string_infallible(value)
        .into_string()
}

fn global_object_handle(runtime: &mut OtterRuntime, name: &str) -> ObjectHandle {
    global_property(runtime, name)
        .as_object_handle()
        .map(ObjectHandle)
        .expect("global should be an object")
}

fn property_value(runtime: &mut OtterRuntime, object: ObjectHandle, name: &str) -> RegisterValue {
    let property = runtime.state_mut().intern_property_name(name);
    runtime
        .state_mut()
        .own_property_value(object, property)
        .expect("property should exist")
}

fn bool_property(runtime: &mut OtterRuntime, object: ObjectHandle, name: &str) -> bool {
    property_value(runtime, object, name)
        .as_bool()
        .expect("property should be boolean")
}

fn assert_global_error(runtime: &mut OtterRuntime) {
    let error = global_property(runtime, "__error");
    if error != RegisterValue::undefined() {
        let message = runtime
            .state_mut()
            .js_to_string_infallible(error)
            .into_string();
        let step = global_property(runtime, "__step")
            .as_number()
            .unwrap_or(-1.0);
        panic!("script failed at step {step}: {message}");
    }
}

fn call_zero_arg_method(
    runtime: &mut OtterRuntime,
    receiver: RegisterValue,
    name: &str,
) -> Result<RegisterValue, otter_vm::descriptors::VmNativeCallError> {
    let object = receiver
        .as_object_handle()
        .map(ObjectHandle)
        .expect("receiver should be an object");
    let method = property_value(runtime, object, name)
        .as_object_handle()
        .map(ObjectHandle)
        .expect("method should be callable");
    runtime
        .state_mut()
        .call_host_function(Some(method), receiver, &[])
}

fn fulfilled_promise_value(runtime: &mut OtterRuntime, promise: RegisterValue) -> RegisterValue {
    let promise_handle = promise
        .as_object_handle()
        .map(ObjectHandle)
        .expect("value should be a promise object");
    runtime
        .state_mut()
        .objects()
        .get_promise(promise_handle)
        .expect("value should be a promise")
        .fulfilled_value()
        .expect("promise should be fulfilled")
}

fn js_string(runtime: &mut OtterRuntime, value: &str) -> RegisterValue {
    RegisterValue::from_object_handle(runtime.state_mut().alloc_string(value).0)
}

#[test]
fn body_readers_return_fulfilled_js_promises() {
    let mut runtime = configure_runtime();
    runtime
        .run_script("var __response = new Response('hello');", "main.js")
        .expect("setup script should execute");

    let response = global_property(&mut runtime, "__response");
    let promise = call_zero_arg_method(&mut runtime, response, "text")
        .expect("Response.text should return a promise");
    let promise_handle = promise
        .as_object_handle()
        .map(ObjectHandle)
        .expect("Response.text should return a JS object");
    assert!(
        runtime
            .state_mut()
            .objects()
            .get_promise(promise_handle)
            .expect("Response.text result should be a promise")
            .is_fulfilled()
    );
}

#[test]
fn body_reader_promises_chain_through_js_microtasks() {
    let (mut runtime, capture) = configure_runtime_with_capture();
    runtime
        .run_script(
            "var response = Response.json({ ok: true, count: 2 }, { status: 201 }); \
             var responseClone = response.clone(); \
             var paramsResponse = new Response(new URLSearchParams([['a', '1'], ['b', '2']])); \
             response \
               .text() \
               .then(function(text) { console.log(text); return responseClone.json(); }) \
               .then(function(value) { console.log(value.ok, value.count); return paramsResponse.text(); }) \
               .then(function(text) { console.log(text); });",
            "main.js",
        )
        .expect("promise chain should execute");

    assert_eq!(capture.text(), "{\"ok\":true,\"count\":2}\ntrue 2\na=1&b=2");
}

#[test]
fn request_follows_clone_body_used_and_init_semantics() {
    let mut runtime = configure_runtime();
    runtime
        .run_script(
            "var __step = 0; \
             var __error = undefined; \
             try { \
               __step = 1; \
               var __source = new Request('https://example.com/api?x=1', { \
                 method: 'post', \
                 headers: { 'X-Test': '1' }, \
                 body: new URLSearchParams([['a', '1'], ['b', '2']]), \
               }); \
               __step = 2; \
               var __clone = __source.clone(); \
               var __override = new Request(__source, { method: 'PUT', body: 'payload' }); \
               var __sourceMethod = __source.method; \
               var __sourceUrl = __source.url; \
               var __sourceContentType = __source.headers.get('content-type'); \
               var __cloneBodyUsedBefore = __clone.bodyUsed; \
               var __overrideMethod = __override.method; \
               var __overrideContentType = __override.headers.get('content-type'); \
               var __overrideBodyUsedBefore = __override.bodyUsed; \
               var __getInit = { method: 'GET', body: 'x' }; \
              } catch (error) { \
               __error = error; \
              }",
            "main.js",
        )
        .expect("request script should execute");

    assert_global_error(&mut runtime);
    assert_eq!(
        global_string_property(&mut runtime, "__sourceMethod"),
        "POST"
    );
    assert_eq!(
        global_string_property(&mut runtime, "__sourceUrl"),
        "https://example.com/api?x=1"
    );
    assert_eq!(
        global_string_property(&mut runtime, "__sourceContentType"),
        "application/x-www-form-urlencoded;charset=UTF-8"
    );
    assert_eq!(
        global_property(&mut runtime, "__cloneBodyUsedBefore"),
        RegisterValue::from_bool(false)
    );
    assert_eq!(
        global_string_property(&mut runtime, "__overrideMethod"),
        "PUT"
    );
    assert_eq!(
        global_string_property(&mut runtime, "__overrideContentType"),
        "application/x-www-form-urlencoded;charset=UTF-8"
    );
    assert_eq!(
        global_property(&mut runtime, "__overrideBodyUsedBefore"),
        RegisterValue::from_bool(false)
    );
    let clone = global_property(&mut runtime, "__clone");
    let clone_handle = global_object_handle(&mut runtime, "__clone");
    let clone_text_promise =
        call_zero_arg_method(&mut runtime, clone, "text").expect("Request.text should succeed");
    let clone_text = fulfilled_promise_value(&mut runtime, clone_text_promise);
    assert_eq!(
        runtime
            .state_mut()
            .js_to_string_infallible(clone_text)
            .into_string(),
        "a=1&b=2"
    );
    assert!(bool_property(&mut runtime, clone_handle, "bodyUsed"));
    assert!(
        call_zero_arg_method(&mut runtime, clone, "clone").is_err(),
        "used Request clone() should throw"
    );

    let request_ctor = global_object_handle(&mut runtime, "Request");
    let get_init = global_property(&mut runtime, "__getInit");
    let get_url = js_string(&mut runtime, "https://example.com/get");
    assert!(
        runtime
            .state_mut()
            .construct_callable(request_ctor, &[get_url, get_init], request_ctor,)
            .is_err(),
        "GET request with body should throw"
    );

    assert!(
        runtime
            .state_mut()
            .construct_callable(request_ctor, &[clone], request_ctor)
            .is_err(),
        "constructing Request from used input should throw"
    );

    let override_request = global_property(&mut runtime, "__override");
    let override_handle = global_object_handle(&mut runtime, "__override");
    let override_text_promise = call_zero_arg_method(&mut runtime, override_request, "text")
        .expect("override.text should succeed");
    let override_text = fulfilled_promise_value(&mut runtime, override_text_promise);
    assert_eq!(
        runtime
            .state_mut()
            .js_to_string_infallible(override_text)
            .into_string(),
        "payload"
    );
    assert!(bool_property(&mut runtime, override_handle, "bodyUsed"));
}

#[test]
fn response_follows_json_clone_and_body_reader_semantics() {
    let mut runtime = configure_runtime();
    runtime
        .run_script(
            "var __step = 0; \
             var __error = undefined; \
             try { \
               __step = 1; \
               var __response = Response.json({ ok: true, count: 2 }, { status: 201, headers: { 'X-Test': '1' } }); \
               var __responseClone = __response.clone(); \
               var __blobResponse = new Response(new Blob(['hello'], { type: 'text/plain' })); \
               var __paramsResponse = new Response(new URLSearchParams([['a', '1'], ['b', '2']])); \
               var __status = __response.status; \
               var __ok = __response.ok; \
               var __type = __response.type; \
               var __redirected = __response.redirected; \
               var __url = __response.url; \
               var __hasBodyStream = __response.body instanceof ReadableStream; \
               var __jsonContentType = __response.headers.get('content-type'); \
               var __bodyUsedBefore = __response.bodyUsed; \
               var __blobContentType = __blobResponse.headers.get('content-type'); \
               var __paramsContentType = __paramsResponse.headers.get('content-type'); \
               var __nullBodyStatusInit = { status: 204 }; \
              } catch (error) { \
               __error = error; \
              }",
            "main.js",
        )
        .expect("response script should execute");

    assert_global_error(&mut runtime);
    assert_eq!(
        global_property(&mut runtime, "__status"),
        RegisterValue::from_i32(201)
    );
    assert_eq!(
        global_property(&mut runtime, "__ok"),
        RegisterValue::from_bool(true)
    );
    assert_eq!(global_string_property(&mut runtime, "__type"), "default");
    assert_eq!(
        global_property(&mut runtime, "__redirected"),
        RegisterValue::from_bool(false)
    );
    assert_eq!(global_string_property(&mut runtime, "__url"), "");
    assert_eq!(
        global_property(&mut runtime, "__hasBodyStream"),
        RegisterValue::from_bool(true)
    );
    assert_eq!(
        global_string_property(&mut runtime, "__jsonContentType"),
        "application/json"
    );
    assert_eq!(
        global_property(&mut runtime, "__bodyUsedBefore"),
        RegisterValue::from_bool(false)
    );
    assert_eq!(
        global_string_property(&mut runtime, "__blobContentType"),
        "text/plain"
    );
    assert_eq!(
        global_string_property(&mut runtime, "__paramsContentType"),
        "application/x-www-form-urlencoded;charset=UTF-8"
    );
    let response = global_property(&mut runtime, "__response");
    let response_handle = global_object_handle(&mut runtime, "__response");
    let text_promise =
        call_zero_arg_method(&mut runtime, response, "text").expect("Response.text should succeed");
    let text = fulfilled_promise_value(&mut runtime, text_promise);
    assert_eq!(
        runtime
            .state_mut()
            .js_to_string_infallible(text)
            .into_string(),
        "{\"ok\":true,\"count\":2}"
    );
    assert!(bool_property(&mut runtime, response_handle, "bodyUsed"));
    assert!(
        call_zero_arg_method(&mut runtime, response, "clone").is_err(),
        "used Response clone() should throw"
    );

    let response_ctor = global_object_handle(&mut runtime, "Response");
    let null_body_status_init = global_property(&mut runtime, "__nullBodyStatusInit");
    let response_body = js_string(&mut runtime, "x");
    assert!(
        runtime
            .state_mut()
            .construct_callable(
                response_ctor,
                &[response_body, null_body_status_init],
                response_ctor,
            )
            .is_err(),
        "null-body status with body should throw"
    );

    let params_response = global_property(&mut runtime, "__paramsResponse");
    let params_text_promise = call_zero_arg_method(&mut runtime, params_response, "text")
        .expect("paramsResponse.text should succeed");
    let params_text = fulfilled_promise_value(&mut runtime, params_text_promise);
    assert_eq!(
        runtime
            .state_mut()
            .js_to_string_infallible(params_text)
            .into_string(),
        "a=1&b=2"
    );
}
