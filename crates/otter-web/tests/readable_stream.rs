use std::sync::Arc;

use otter_runtime::{ModuleLoaderConfig, OtterRuntime};
use otter_vm::console::CaptureConsoleBackend;
use otter_web::web_extension;

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

#[test]
fn request_and_response_bodies_expose_readable_streams() {
    let (mut runtime, capture) = configure_runtime_with_capture();
    runtime
        .run_script(
            "var response = new Response('hello'); \
             var empty = new Response(''); \
             var missing = new Response(); \
             console.log(response.body === response.body, response.body instanceof ReadableStream, empty.body === null, missing.body === null); \
             var reader = response.body.getReader(); \
             console.log(response.body.locked, response.bodyUsed, reader instanceof ReadableStreamDefaultReader); \
             reader.read() \
               .then(function(result) { console.log(result.done, new TextDecoder().decode(result.value), response.bodyUsed); return reader.read(); }) \
               .then(function(result) { console.log(result.done, result.value === undefined); });",
            "main.js",
        )
        .expect("readable stream script should execute");

    assert_eq!(
        capture.text(),
        "true true false true\ntrue false true\nfalse hello true\ntrue true"
    );
}

#[test]
fn body_readers_respect_lock_and_disturb_semantics() {
    let (mut runtime, capture) = configure_runtime_with_capture();
    runtime
        .run_script(
            "var locked = new Response('hello'); \
             var lockedReader = locked.body.getReader(); \
             locked.text() \
               .then(function() { console.log('unexpected'); }, function(error) { \
                 console.log(error instanceof TypeError); \
                 lockedReader.releaseLock(); \
                 return locked.text(); \
               }) \
               .then(function(text) { \
                 console.log(text); \
                 var disturbed = new Response('bye'); \
                 var disturbedReader = disturbed.body.getReader(); \
                 return disturbedReader.read().then(function() { \
                   disturbedReader.releaseLock(); \
                   return disturbed.text(); \
                 }); \
               }) \
               .then(function() { console.log('unexpected'); }, function(error) { console.log(error instanceof TypeError); });",
            "main.js",
        )
        .expect("body lock script should execute");

    assert_eq!(capture.text(), "true\nhello\ntrue");
}

#[test]
fn readable_stream_async_iterator_returns_promise_iter_results() {
    let (mut runtime, capture) = configure_runtime_with_capture();
    runtime
        .run_script(
            "var iterator = new Response('iter').body[Symbol.asyncIterator](); \
             iterator.next() \
               .then(function(result) { console.log(result.done, new TextDecoder().decode(result.value)); return iterator.next(); }) \
               .then(function(result) { console.log(result.done, result.value === undefined); return iterator.return(); }) \
               .then(function(result) { console.log(result.done, result.value === undefined); });",
            "main.js",
        )
        .expect("async iterator script should execute");

    assert_eq!(capture.text(), "false iter\ntrue true\ntrue true");
}
