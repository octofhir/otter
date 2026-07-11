//! End-to-end coverage for the Web `fetch()` global.
//!
//! `fetch()` normalizes its arguments through the `Request` constructor, gates
//! the request on the `net` capability, drives the reqwest transport off-thread
//! through the async completion protocol, and resolves with a real `Response`.
//! These tests exercise a live loopback server (buffered GET with a forwarded
//! header, and a POST whose body round-trips) and the deny-by-default gate.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

use otter_runtime::{
    CapabilitySet, ConsoleLevel, ConsoleSink, Otter, OtterError, Permission, SourceInput,
};
use otter_web::WebApiBuilderExt;

#[derive(Debug, Default)]
struct LogCapture {
    events: Mutex<Vec<String>>,
}

impl LogCapture {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn snapshot(&self) -> Vec<String> {
        self.events.lock().expect("log mutex").clone()
    }
}

impl ConsoleSink for LogCapture {
    fn write(&self, level: ConsoleLevel, fields: &[String]) {
        if matches!(level, ConsoleLevel::Log) {
            self.events
                .lock()
                .expect("log mutex")
                .push(fields.join(" "));
        }
    }
}

/// A capability set that permits outbound network but nothing else.
fn allow_net() -> CapabilitySet {
    let mut caps = CapabilitySet::sandbox();
    caps.net = Permission::AllowAll;
    caps
}

/// Spawn a one-shot loopback HTTP/1.1 server. The handler receives the raw
/// request bytes and returns the full raw response; the bound address is
/// returned so the test can build its URL.
fn spawn_one_shot<F>(handler: F) -> (String, thread::JoinHandle<()>)
where
    F: FnOnce(&str) -> String + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let join = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 8192];
        let read = stream.read(&mut buf).unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..read]).into_owned();
        let response = handler(&request);
        stream
            .write_all(response.as_bytes())
            .expect("write response");
        let _ = stream.flush();
    });
    (format!("http://{addr}/"), join)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_performs_buffered_get() -> Result<(), OtterError> {
    let (url, server) = spawn_one_shot(|request| {
        // Echo whether the custom header arrived so the test can assert headers
        // are forwarded.
        assert!(
            request.to_ascii_lowercase().contains("x-probe: otter"),
            "request headers not forwarded: {request}"
        );
        let body = "hello fetch";
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
    });

    let capture = LogCapture::new();
    let otter = Otter::builder()
        .with_web_apis()
        .capabilities(allow_net())
        .console_sink(capture.clone())
        .build()?;
    otter
        .handle()
        .run_script(
            SourceInput::from_javascript(format!(
                r#"
                fetch("{url}", {{ headers: {{ "X-Probe": "otter" }} }})
                  .then((response) =>
                    response.text().then((text) =>
                      console.log(
                        "ok:" + response.status + ":" + response.statusText + ":" +
                        response.headers.get("content-type") + ":" + text + ":" + response.ok
                      )
                    )
                  )
                  .catch((error) => console.log("err:" + error));
                "#
            )),
            "<fetch-get>",
        )
        .await?;
    server.join().expect("server thread");
    assert_eq!(
        capture.snapshot(),
        vec!["ok:200:OK:text/plain:hello fetch:true".to_string()]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_response_body_is_a_readable_stream() -> Result<(), OtterError> {
    let (url, server) = spawn_one_shot(|_request| {
        let body = "chunk-a chunk-b";
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
    });

    let capture = LogCapture::new();
    let otter = Otter::builder()
        .with_web_apis()
        .capabilities(allow_net())
        .console_sink(capture.clone())
        .build()?;
    otter
        .handle()
        .run_script(
            SourceInput::from_javascript(format!(
                r#"
                fetch("{url}")
                  .then(async (response) => {{
                    const isStream = response.body instanceof ReadableStream;
                    const reader = response.body.getReader();
                    const decoder = new TextDecoder();
                    let text = "";
                    for (;;) {{
                      const {{ done, value }} = await reader.read();
                      if (done) break;
                      text += decoder.decode(value);
                    }}
                    console.log("stream:" + isStream + ":" + text);
                  }})
                  .catch((error) => console.log("err:" + error));
                "#
            )),
            "<fetch-stream>",
        )
        .await?;
    server.join().expect("server thread");
    assert_eq!(
        capture.snapshot(),
        vec!["stream:true:chunk-a chunk-b".to_string()]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_forwards_request_body_on_post() -> Result<(), OtterError> {
    let (url, server) = spawn_one_shot(|request| {
        assert!(
            request.starts_with("POST "),
            "expected POST request line: {request}"
        );
        let body = request.rsplit("\r\n\r\n").next().unwrap_or("");
        assert!(
            body.contains("ping=pong"),
            "request body missing: {request}"
        );
        let reply = "received";
        format!(
            "HTTP/1.1 201 Created\r\nContent-Length: {}\r\n\r\n{}",
            reply.len(),
            reply
        )
    });

    let capture = LogCapture::new();
    let otter = Otter::builder()
        .with_web_apis()
        .capabilities(allow_net())
        .console_sink(capture.clone())
        .build()?;
    otter
        .handle()
        .run_script(
            SourceInput::from_javascript(format!(
                r#"
                fetch("{url}", {{ method: "POST", body: "ping=pong" }})
                  .then((response) =>
                    response.text().then((text) =>
                      console.log("ok:" + response.status + ":" + text)
                    )
                  )
                  .catch((error) => console.log("err:" + error));
                "#
            )),
            "<fetch-post>",
        )
        .await?;
    server.join().expect("server thread");
    assert_eq!(capture.snapshot(), vec!["ok:201:received".to_string()]);
    Ok(())
}

/// A one-shot server that answers with a 302 redirect to `location`.
fn spawn_redirect_server(location: &'static str) -> (String, thread::JoinHandle<()>) {
    spawn_one_shot(move |_request| {
        format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n")
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_redirect_error_rejects() -> Result<(), OtterError> {
    let (url, server) = spawn_redirect_server("http://127.0.0.1:9/next");
    let capture = LogCapture::new();
    let otter = Otter::builder()
        .with_web_apis()
        .capabilities(allow_net())
        .console_sink(capture.clone())
        .build()?;
    otter
        .handle()
        .run_script(
            SourceInput::from_javascript(format!(
                r#"
                fetch("{url}", {{ redirect: "error" }})
                  .then(() => console.log("resolved"))
                  .catch((error) => console.log("rejected:" + (error instanceof TypeError)));
                "#
            )),
            "<fetch-redirect-error>",
        )
        .await?;
    server.join().expect("server thread");
    assert_eq!(capture.snapshot(), vec!["rejected:true".to_string()]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_redirect_manual_returns_response() -> Result<(), OtterError> {
    let (url, server) = spawn_redirect_server("http://127.0.0.1:9/next");
    let capture = LogCapture::new();
    let otter = Otter::builder()
        .with_web_apis()
        .capabilities(allow_net())
        .console_sink(capture.clone())
        .build()?;
    otter
        .handle()
        .run_script(
            SourceInput::from_javascript(format!(
                r#"
                fetch("{url}", {{ redirect: "manual" }})
                  .then((response) => console.log(
                    "manual:" + response.status + ":" + response.headers.get("location")
                  ))
                  .catch((error) => console.log("err:" + error));
                "#
            )),
            "<fetch-redirect-manual>",
        )
        .await?;
    server.join().expect("server thread");
    assert_eq!(
        capture.snapshot(),
        vec!["manual:302:http://127.0.0.1:9/next".to_string()]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_aborts_in_flight_request() -> Result<(), OtterError> {
    // A bound listener that never accepts: reqwest completes the TCP handshake
    // (kernel backlog) and then waits forever for a response, so the request is
    // reliably in-flight when the signal aborts. The listener is held for the
    // test's duration to keep the port bound.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let url = format!("http://{}/", listener.local_addr().expect("addr"));

    let capture = LogCapture::new();
    let otter = Otter::builder()
        .with_web_apis()
        .capabilities(allow_net())
        .console_sink(capture.clone())
        .build()?;
    otter
        .handle()
        .run_script(
            SourceInput::from_javascript(format!(
                r#"
                const controller = new AbortController();
                const p = fetch("{url}", {{ signal: controller.signal }});
                controller.abort();
                p.then(
                  () => console.log("resolved"),
                  (error) => console.log(
                    "aborted:" + (error && error.name) + ":" + (error instanceof DOMException)
                  ),
                );
                "#
            )),
            "<fetch-abort>",
        )
        .await?;
    drop(listener);
    assert_eq!(
        capture.snapshot(),
        vec!["aborted:AbortError:true".to_string()]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_rejects_without_net_capability() -> Result<(), OtterError> {
    let capture = LogCapture::new();
    // Default sandbox: `net` is denied. No server is contacted.
    let otter = Otter::builder()
        .with_web_apis()
        .console_sink(capture.clone())
        .build()?;
    otter
        .handle()
        .run_script(
            SourceInput::from_javascript(
                r#"
                fetch("http://127.0.0.1:9/denied")
                  .then(() => console.log("resolved"))
                  .catch((error) =>
                    console.log(
                      "rejected:" + (error instanceof TypeError) + ":" +
                      String(error.message).includes("not allowed")
                    )
                  );
                "#,
            ),
            "<fetch-denied>",
        )
        .await?;
    assert_eq!(capture.snapshot(), vec!["rejected:true:true".to_string()]);
    Ok(())
}
