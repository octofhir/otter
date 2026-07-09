//! End-to-end coverage for `#[js_module]` — typed hosted-module
//! exports.
//!
//! A marker-type impl block declares the module's exports with typed
//! signatures: extraction/construction ride the marshalling layer,
//! `async fn` exports run the promise protocol, `raw` keeps the
//! lodge-native signature, and `capabilities = true` threads the
//! install-time snapshot into exports that ask for it.

use std::io::Write as _;
use std::sync::{Arc, Mutex};

use otter_macros::js_module;
use otter_runtime::marshal::{JsError, USVString, Uint8Array};
use otter_runtime::{
    CapabilitySet, ConsoleLevel, ConsoleSink, Otter, OtterError, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeValue as Value,
};

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

/// Marker type for the typed test module.
pub struct MathModule;

#[js_module(prefix = "test", name = "math", capabilities = true)]
impl MathModule {
    #[export(name = "add")]
    fn add(a: f64, b: f64) -> f64 {
        a + b
    }

    #[export(name = "shout")]
    fn shout(text: USVString) -> Result<String, JsError> {
        if text.as_str().is_empty() {
            return Err(JsError::Range("empty input".to_string()));
        }
        Ok(text.as_str().to_uppercase())
    }

    #[export(name = "bytesOf")]
    fn bytes_of(text: USVString) -> Uint8Array {
        Uint8Array(text.as_str().as_bytes().to_vec())
    }

    #[export(name = "slowDouble")]
    async fn slow_double(n: f64) -> f64 {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        n * 2.0
    }

    #[export(name = "canUseNet")]
    fn can_use_net(caps: &CapabilitySet) -> bool {
        !matches!(caps.net, otter_runtime::Permission::Deny)
    }

    #[export(name = "rawEcho", length = 1, raw)]
    fn raw_echo(
        _ctx: &mut NativeCtx<'_>,
        args: &[Value],
        _caps: &CapabilitySet,
    ) -> Result<Value, NativeError> {
        Ok(args.first().copied().unwrap_or_else(Value::undefined))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_module_exports_work_end_to_end() -> Result<(), OtterError> {
    let capture = LogCapture::new();
    let otter = Otter::builder()
        .console_sink(capture.clone())
        .hosted_module(MATH_HOSTED_MODULE)
        .build()
        .expect("otter");

    let dir = tempfile::tempdir().expect("tempdir");
    let entry = dir.path().join("main.mjs");
    let mut file = std::fs::File::create(&entry).expect("entry file");
    file.write_all(
        br#"
        import { add, shout, bytesOf, slowDouble, canUseNet, rawEcho } from "test:math";
        console.log("add", add(2, 40.5));
        console.log("shout", shout("quiet"));
        try { shout(""); } catch (e) { console.log("err", e instanceof RangeError, e.message.includes("empty")); }
        const bytes = bytesOf("ab");
        console.log("bytes", bytes instanceof Uint8Array, bytes.length);
        console.log("caps", canUseNet());
        console.log("raw", rawEcho("echoed"));
        const doubled = await slowDouble(21);
        console.log("async", doubled);
        "#,
    )
    .expect("write entry");
    drop(file);

    otter.handle().run_module(&entry).await?;
    assert_eq!(
        capture.snapshot(),
        vec![
            "add 42.5".to_string(),
            "shout QUIET".to_string(),
            "err true true".to_string(),
            "bytes true 2".to_string(),
            "caps false".to_string(),
            "raw echoed".to_string(),
            "async 42".to_string(),
        ]
    );
    Ok(())
}
