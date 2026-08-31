//! End-to-end coverage for the active `Otter.serve` transport.
//!
//! # Contents
//! - A loopback POST round trip through the Web `Request` and `Response` body APIs.
//!
//! # Invariants
//! - The public server API remains a plain options object with no resource or
//!   capability configuration added to the JavaScript call.
//! - Native request/response buffering is transparent to Fetch consumers.

use std::sync::{Arc, Mutex};

use otter_modules::OtterModulesBuilderExt;
use otter_runtime::{CapabilitySet, ConsoleLevel, ConsoleSink, Otter, OtterError, SourceInput};
use otter_web::WebApiBuilderExt;

#[derive(Debug, Default)]
struct LogCapture {
    events: Mutex<Vec<String>>,
}

impl LogCapture {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_round_trips_a_buffered_fetch_body() -> Result<(), OtterError> {
    let capture = Arc::new(LogCapture::default());
    let otter = Otter::builder()
        .with_web_apis()
        .with_otter_modules()
        .capabilities(CapabilitySet::allow_all())
        .console_sink(capture.clone())
        .build()?;

    otter
        .handle()
        .run_script(
            SourceInput::from_javascript(
                r#"
                const server = Otter.serve({
                  hostname: "127.0.0.1",
                  port: 0,
                  async fetch(request) {
                    const text = await request.text();
                    return new Response("echo:" + text, {
                      status: 201,
                      headers: { "x-otter": "served" },
                    });
                  },
                });

                fetch(server.url, { method: "POST", body: "hé🦦" })
                  .then(async (response) => {
                    const text = await response.text();
                    console.log(response.status + ":" + response.headers.get("x-otter") + ":" + text);
                  })
                  .finally(() => server.stop());
                "#,
            ),
            "serve-round-trip.js",
        )
        .await?;

    assert_eq!(capture.snapshot(), vec!["201:served:echo:hé🦦"]);
    Ok(())
}
