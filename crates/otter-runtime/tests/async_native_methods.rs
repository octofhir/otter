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

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use otter_macros::{HostClass, js_class};
use otter_runtime::{
    ConsoleLevel, ConsoleSink, GlobalClass, HostCompletionAdmission, HostCompletionJob,
    HostCompletionOutcome, HostCompletionSink, NativeCtx, NativeError, Otter, OtterError, Runtime,
    SourceInput, Value,
};
use otter_vm::marshal::{JsError, MarshalCx};

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

static ADMISSION_PROBE_POLLED: AtomicBool = AtomicBool::new(false);
static HOSTLESS_FACTORY_CALLED: AtomicBool = AtomicBool::new(false);

fn hostless_factory_probe(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    ctx.scope(|scope| {
        let mut cx = MarshalCx::new(scope);
        let promise = cx
            .promise_from_future(|| {
                HOSTLESS_FACTORY_CALLED.store(true, Ordering::Release);
                std::future::pending::<Result<f64, JsError>>()
            })
            .map_err(|error| error.into_native("hostlessFactoryProbe"))?;
        Ok(cx.escape(promise))
    })
}

/// Host class whose async body records whether completion admission happened first.
#[derive(Debug, Clone, HostClass)]
pub struct AdmissionProbe {
    _marker: (),
}

#[js_class(name = "AdmissionProbe", feature = WEB)]
impl AdmissionProbe {
    #[constructor]
    fn js_new() -> AdmissionProbe {
        AdmissionProbe { _marker: () }
    }

    #[method(name = "start")]
    async fn js_start(self) -> f64 {
        ADMISSION_PROBE_POLLED.store(true, Ordering::Release);
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        1.0
    }
}

fn build_otter(capture: Arc<LogCapture>) -> Otter {
    Otter::builder()
        .console_sink(capture)
        .global_classes([GlobalClass::from_intrinsic::<SleeperIntrinsic>()])
        .build()
        .expect("otter")
}

struct ChannelCompletionSink {
    handle: tokio::runtime::Handle,
    tx: Sender<(
        HostCompletionAdmission,
        HostCompletionJob,
        HostCompletionOutcome,
    )>,
}

impl HostCompletionSink for ChannelCompletionSink {
    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>) {
        self.handle.spawn(future);
    }

    fn complete(
        &self,
        admission: HostCompletionAdmission,
        job: HostCompletionJob,
        outcome: HostCompletionOutcome,
    ) -> Result<(), String> {
        self.tx
            .send((admission, job, outcome))
            .map_err(|_| "Layer A completion receiver closed".to_string())
    }

    fn finish_inline(
        &self,
        admission: HostCompletionAdmission,
        _outcome: HostCompletionOutcome,
    ) -> Result<(), String> {
        drop(admission);
        Ok(())
    }

    fn admit(&self) -> Result<HostCompletionAdmission, String> {
        Ok(HostCompletionAdmission::new(Box::new(())))
    }

    fn with_executor_context(&self, f: &mut dyn FnMut()) {
        let _guard = self.handle.enter();
        f();
    }
}

fn build_layer_a(
    capture: Arc<LogCapture>,
    handle: tokio::runtime::Handle,
) -> (
    Runtime,
    Receiver<(
        HostCompletionAdmission,
        HostCompletionJob,
        HostCompletionOutcome,
    )>,
) {
    let mut runtime = Runtime::builder()
        .console_sink(capture)
        .global_classes([GlobalClass::from_intrinsic::<SleeperIntrinsic>()])
        .build()
        .expect("runtime");
    let (tx, rx) = channel();
    runtime.install_host_completion_sink(Arc::new(ChannelCompletionSink { handle, tx }));
    (runtime, rx)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhausted_host_capacity_rejects_before_the_future_is_polled() -> Result<(), OtterError> {
    ADMISSION_PROBE_POLLED.store(false, Ordering::Release);
    let capture = LogCapture::new();
    let otter = Otter::builder()
        .completion_capacities(0, 0, 1)
        .console_sink(capture.clone())
        .global_classes([GlobalClass::from_intrinsic::<AdmissionProbeIntrinsic>()])
        .build()
        .expect("zero completion capacity is a valid fail-closed runtime");

    otter
        .handle()
        .run_script(
            SourceInput::from_javascript(
                r#"
                try {
                    new AdmissionProbe().start();
                    console.log("unexpected-success");
                } catch (error) {
                    console.log(error.name + ":" + /capacity/.test(error.message));
                }
                "#,
            ),
            "<async-admission-exhausted>",
        )
        .await?;

    assert!(!ADMISSION_PROBE_POLLED.load(Ordering::Acquire));
    assert_eq!(capture.snapshot(), vec!["TypeError:true".to_string()]);
    assert_eq!(otter.activity_stats().pending_ref_host_ops, 0);
    Ok(())
}

#[test]
fn hostless_embedding_rejects_before_constructing_the_future() {
    HOSTLESS_FACTORY_CALLED.store(false, Ordering::Release);
    let mut runtime = Runtime::builder().build().expect("hostless runtime");
    runtime
        .install_native_global("hostlessFactoryProbe", 0, hostless_factory_probe)
        .expect("probe installs");

    let result = runtime
        .eval(SourceInput::from_javascript(
            r#"
            let outcome;
            try {
                hostlessFactoryProbe();
                outcome = "unexpected-success";
            } catch (error) {
                outcome = error.name + ":" + /not available/.test(error.message);
            }
            outcome;
            "#,
        ))
        .expect("hostless admission failure is catchable");

    assert_eq!(result.completion_string(), "TypeError:true");
    assert!(!HOSTLESS_FACTORY_CALLED.load(Ordering::Acquire));
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
    let activity = otter.activity_stats();
    assert_eq!(activity.completed_host_ops, 1);
    assert_eq!(activity.cancelled_host_ops, 0);
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

#[test]
fn layer_a_embedder_delivers_async_completion_on_its_own_thread() {
    let executor = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("Tokio runtime");
    let capture = LogCapture::new();
    let (mut runtime, completions) = build_layer_a(capture.clone(), executor.handle().clone());

    runtime
        .run_script(
            SourceInput::from_javascript(
                "new Sleeper('browser').wait(1).then(value => console.log(value));",
            ),
            "<layer-a-async>",
        )
        .expect("script starts async work");

    let (admission, job, outcome) = completions
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("executor posts the completion to the embedder queue");
    assert_eq!(outcome, HostCompletionOutcome::Completed);
    runtime.run_host_completion(job);
    drop(admission);

    assert_eq!(capture.snapshot(), vec!["browser+1".to_string()]);
}

#[test]
fn build_handle_uses_an_explicit_embedder_tokio_runtime() {
    let executor = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("Tokio runtime");
    let capture = LogCapture::new();
    let otter = Otter::builder()
        .tokio_handle(executor.handle().clone())
        .console_sink(capture.clone())
        .global_classes([GlobalClass::from_intrinsic::<SleeperIntrinsic>()])
        .build()
        .expect("otter");

    executor
        .block_on(otter.handle().run_script(
            SourceInput::from_javascript(
                "new Sleeper('shared').wait(1).then(value => console.log(value));",
            ),
            "<explicit-tokio>",
        ))
        .expect("script completes on the supplied executor");

    assert_eq!(capture.snapshot(), vec!["shared+1".to_string()]);
}
