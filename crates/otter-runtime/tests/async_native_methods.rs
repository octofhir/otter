//! End-to-end coverage for the async native-method protocol.
//!
//! A `#[js_class]` method declared `async fn` compiles to the promise
//! protocol: sync prologue (brand snapshot + argument extraction),
//! a `Send` future on the shared Tokio runtime, and a completion job
//! that converts the result and settles on the isolate thread. The
//! event loop must stay alive while the future is outstanding (the
//! completer holds a liveness ref), immediately-ready futures must
//! settle with no executor round-trip, and rejections must surface as
//! real error instances.

use std::sync::{Arc, Mutex};

use otter_macros::{HostClass, js_class};
use otter_runtime::{ConsoleLevel, ConsoleSink, GlobalClass, Otter, OtterError, SourceInput};
use otter_vm::marshal::JsError;

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
        if !matches!(level, ConsoleLevel::Log) {
            return;
        }
        self.events
            .lock()
            .expect("log mutex")
            .push(fields.join(" "));
    }
}

/// Test host class: real async work (Tokio sleep), an
/// immediately-ready async method, and an async rejection.
#[derive(Debug, Clone, HostClass)]
pub struct Sleeper {
    label: String,
}

#[js_class(name = "Sleeper", feature = WEB)]
impl Sleeper {
    #[constructor]
    fn js_new(label: otter_vm::marshal::USVString) -> Sleeper {
        Sleeper {
            label: label.into_string(),
        }
    }

    #[method(name = "wait")]
    async fn js_wait(self, ms: f64) -> String {
        tokio::time::sleep(std::time::Duration::from_millis(ms as u64)).await;
        format!("{}+{}", self.label, ms)
    }

    #[method(name = "quick")]
    async fn js_quick(self) -> f64 {
        // No await point: the glue's poll-once fast path settles this
        // without touching the executor.
        self.label.len() as f64
    }

    #[method(name = "boom")]
    async fn js_boom(self) -> Result<f64, JsError> {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        Err(JsError::Range(format!("{} exploded", self.label)))
    }
}

fn build_otter(capture: Arc<LogCapture>) -> Otter {
    Otter::builder()
        .console_sink(capture)
        .global_classes([GlobalClass::from_intrinsic::<SleeperIntrinsic>()])
        .build()
        .expect("otter")
}

/// Real async: the future parks on Tokio, the event loop stays alive
/// until the completer settles, and the reaction sees the converted
/// Rust value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_method_settles_after_real_await() -> Result<(), OtterError> {
    let capture = LogCapture::new();
    let otter = build_otter(capture.clone());
    otter
        .handle()
        .run_script(
            SourceInput::from_javascript(
                r#"
                const s = new Sleeper("nap");
                const p = s.wait(20);
                if (typeof p.then !== "function") console.log("not-a-promise");
                p.then(
                    (v) => console.log("ok:" + v),
                    (e) => console.log("err:" + e),
                );
                "#,
            ),
            "<async-wait>",
        )
        .await?;
    assert_eq!(capture.snapshot(), vec!["ok:nap+20".to_string()]);
    Ok(())
}

/// Immediately-ready future: settles through the pre-settled promise
/// path (works even without the executor round-trip) and reactions
/// run on the ordinary microtask drain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_method_fast_path_settles_ready_future() -> Result<(), OtterError> {
    let capture = LogCapture::new();
    let otter = build_otter(capture.clone());
    otter
        .handle()
        .run_script(
            SourceInput::from_javascript(
                r#"
                new Sleeper("abcd").quick().then((v) => console.log("quick:" + v));
                "#,
            ),
            "<async-quick>",
        )
        .await?;
    assert_eq!(capture.snapshot(), vec!["quick:4".to_string()]);
    Ok(())
}

/// Async rejection surfaces as a real RangeError instance with the
/// body's message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_method_rejection_is_a_real_error_instance() -> Result<(), OtterError> {
    let capture = LogCapture::new();
    let otter = build_otter(capture.clone());
    otter
        .handle()
        .run_script(
            SourceInput::from_javascript(
                r#"
                new Sleeper("kaboom").boom().then(
                    (v) => console.log("ok:" + v),
                    (e) => console.log("rejected:" + (e instanceof RangeError) + ":" + e.message),
                );
                "#,
            ),
            "<async-boom>",
        )
        .await?;
    assert_eq!(
        capture.snapshot(),
        vec!["rejected:true:kaboom exploded".to_string()]
    );
    Ok(())
}

/// await inside an async function over an async native method — the
/// composed path (VM await machinery + host completion) end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_method_composes_with_js_await() -> Result<(), OtterError> {
    let capture = LogCapture::new();
    let otter = build_otter(capture.clone());
    otter
        .handle()
        .run_script(
            SourceInput::from_javascript(
                r#"
                async function main() {
                    const a = await new Sleeper("a").wait(5);
                    const b = await new Sleeper("b").wait(1);
                    console.log("seq:" + a + "|" + b);
                }
                main();
                "#,
            ),
            "<async-compose>",
        )
        .await?;
    assert_eq!(capture.snapshot(), vec!["seq:a+5|b+1".to_string()]);
    Ok(())
}
