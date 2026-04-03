use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

use otter_runtime::{CapabilitiesBuilder, ModuleLoaderConfig, OtterRuntime};
use otter_vm::console::CaptureConsoleBackend;
use otter_web::web_extension;

fn configure_runtime_with_capture(
    allow_net: impl IntoIterator<Item = String>,
) -> (OtterRuntime, Arc<CaptureConsoleBackend>) {
    let capture = Arc::new(CaptureConsoleBackend::new());
    let runtime = OtterRuntime::builder()
        .module_loader(ModuleLoaderConfig::default())
        .capabilities(CapabilitiesBuilder::new().allow_net(allow_net).build())
        .console(CaptureForTest(capture.clone()))
        .extension(web_extension())
        .build();
    (runtime, capture)
}

fn configure_runtime_without_net() -> (OtterRuntime, Arc<CaptureConsoleBackend>) {
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

fn spawn_test_server(
    response: &'static str,
    seen_request: Arc<Mutex<Option<String>>>,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener should bind");
    let address = listener
        .local_addr()
        .expect("listener address should exist");
    let url = format!("http://127.0.0.1:{}/test", address.port());
    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("server should accept");
        let mut reader = BufReader::new(stream.try_clone().expect("stream should clone"));

        let mut raw_request = String::new();
        let mut request_line = String::new();
        reader
            .read_line(&mut request_line)
            .expect("request line should read");
        raw_request.push_str(&request_line);

        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .expect("header line should read");
            raw_request.push_str(&line);
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }

        if content_length > 0 {
            let mut body = vec![0_u8; content_length];
            reader.read_exact(&mut body).expect("body should read");
            raw_request.push_str(&String::from_utf8_lossy(&body));
        }

        *seen_request.lock().expect("request mutex should lock") = Some(raw_request);

        let mut stream = stream;
        stream
            .write_all(response.as_bytes())
            .expect("response should write");
        stream.flush().expect("response should flush");
    });
    (url, handle)
}

#[test]
fn fetch_sends_request_and_resolves_response() {
    let seen_request = Arc::new(Mutex::new(None));
    let (url, server) = spawn_test_server(
        "HTTP/1.1 201 Created\r\nContent-Length: 4\r\nContent-Type: text/plain\r\nX-Reply: yes\r\nConnection: close\r\n\r\npong",
        seen_request.clone(),
    );
    let (mut runtime, capture) = configure_runtime_with_capture(["127.0.0.1".to_string()]);

    runtime
        .run_script(
            &format!(
                "fetch('{url}', {{ method: 'POST', headers: {{ 'X-Test': '1' }}, body: 'ping' }}) \
                   .then(function(response) {{ console.log(response.status, response.ok, response.headers.get('x-reply')); return response.text(); }}) \
                   .then(function(text) {{ console.log(text); }});"
            ),
            "main.js",
        )
        .expect("fetch script should execute");

    server.join().expect("server thread should join");
    assert_eq!(capture.text(), "201 true yes\npong");

    let seen = seen_request
        .lock()
        .expect("request mutex should lock")
        .clone()
        .expect("request should be captured");
    assert!(seen.starts_with("POST /test HTTP/1.1\r\n"));
    assert!(seen.contains("x-test: 1\r\n") || seen.contains("X-Test: 1\r\n"));
    assert!(
        seen.contains("content-type: text/plain;charset=UTF-8\r\n")
            || seen.contains("Content-Type: text/plain;charset=UTF-8\r\n")
    );
    assert!(seen.ends_with("ping"));
}

#[test]
fn fetch_resolves_on_http_error_status() {
    let seen_request = Arc::new(Mutex::new(None));
    let (url, server) = spawn_test_server(
        "HTTP/1.1 404 Not Found\r\nContent-Length: 7\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nmissing",
        seen_request,
    );
    let (mut runtime, capture) = configure_runtime_with_capture(["127.0.0.1".to_string()]);

    runtime
        .run_script(
            &format!(
                "fetch('{url}') \
                   .then(function(response) {{ console.log(response.status, response.ok); return response.text(); }}) \
                   .then(function(text) {{ console.log(text); }});"
            ),
            "main.js",
        )
        .expect("fetch script should execute");

    server.join().expect("server thread should join");
    assert_eq!(capture.text(), "404 false\nmissing");
}

#[test]
fn fetch_rejects_when_network_capability_is_missing() {
    let (mut runtime, capture) = configure_runtime_without_net();
    runtime
        .run_script(
            "fetch('http://127.0.0.1:1/test') \
               .catch(function(error) { console.log(String(error).includes('PermissionDenied')); });",
            "main.js",
        )
        .expect("fetch script should execute");

    assert_eq!(capture.text(), "true");
}
