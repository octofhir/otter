//! The template's contract, end to end: every declared surface works
//! through real JS, including a JS subclass and the async protocol.
//!
//! GC-stress discipline for any new surface you add: run your
//! exercise script under every stride and compare exit codes AND line
//! counts (silent deaths add no output line):
//!
//! ```bash
//! for s in 0 1 2 4 8 16; do OTTER_GC_STRESS=$s otter run exercise.mjs; done
//! ```

use std::io::Write as _;
use std::sync::{Arc, Mutex};

use otter_extension_template::{ACME_EXTENSION, UTIL_HOSTED_MODULE};
use otter_runtime::{
    ConsoleLevel, ConsoleSink, Otter, OtterError, Runtime, RuntimeBuilder, SourceInput,
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

fn eval_string(runtime: &mut Runtime, source: &str) -> String {
    runtime
        .eval(SourceInput::from_javascript(source))
        .unwrap()
        .completion_string()
        .to_string()
}

/// Class, namespace, attached JS, statics, JS subclassing — the
/// direct-mode (no event loop) surface.
#[test]
fn extension_surfaces_work_in_direct_mode() {
    let mut runtime = RuntimeBuilder::default()
        .extension(&ACME_EXTENSION)
        .build()
        .expect("runtime");
    let result = eval_string(
        &mut runtime,
        r#"
        var out = [];
        const c = new Counter("clicks", 40);
        out.push(c instanceof Counter, c.label, c.value);
        out.push(c.increment(), c.increment(1.5));
        out.push(c.describe());                       // attached JS half
        out.push(Object.prototype.toString.call(c));  // toStringTag
        const f = Counter.fromValue("factory", 7);
        out.push(f instanceof Counter, f.value);
        class Mine extends Counter { double() { return this.value * 2; } }
        const m = new Mine("sub", 4);
        out.push(m instanceof Mine, m instanceof Counter, m.double());
        out.push(Acme.version().length > 0, Acme.greet("otter"));
        try { Acme.greet(""); } catch (e) { out.push(e instanceof TypeError); }
        out.join("|")
        "#,
    );
    assert_eq!(
        result,
        "true|clicks|40|41|42.5|clicks=42.5|[object Counter]|true|7|true|true|8|true|hello, otter|true"
    );
}

/// Module import + the async protocol need the full handle/event-loop
/// embedding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extension_module_and_async_work_through_the_handle() -> Result<(), OtterError> {
    let capture = LogCapture::new();
    let otter = Otter::builder()
        .console_sink(capture.clone())
        .extension(&ACME_EXTENSION)
        .hosted_module(UTIL_HOSTED_MODULE)
        .build()
        .expect("otter");

    let dir = tempfile::tempdir().expect("tempdir");
    let entry = dir.path().join("main.mjs");
    let mut file = std::fs::File::create(&entry).expect("entry");
    file.write_all(
        br#"
        import { shout, canReadEnv } from "acme:util";
        console.log("shout", shout("quiet"));
        console.log("env", canReadEnv("HOME"));
        const c = new Counter("bytes");
        const snapshot = await c.snapshotBytes();
        console.log("async", snapshot instanceof Uint8Array, snapshot.length);
        "#,
    )
    .expect("write");
    drop(file);

    otter.handle().run_module(&entry).await?;
    assert_eq!(
        capture.snapshot(),
        vec![
            "shout QUIET".to_string(),
            "env false".to_string(),
            "async true 5".to_string(),
        ]
    );
    Ok(())
}
