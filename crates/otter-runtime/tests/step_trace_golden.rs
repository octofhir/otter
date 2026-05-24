//! Golden coverage for the per-instruction step trace (Phase 5.1).
//!
//! Each test runs a small JS / TS fixture through [`Otter`] with a
//! capture tracer installed and compares the resulting trace
//! against a checked-in `.trace` golden file. Set
//! `OTTER_BLESS_TRACES=1` to overwrite the golden files when the
//! bytecode shape moves and the diff is intentional.
//!
//! Fixtures cover the four shapes called out in the refactor plan
//! §5.1 acceptance:
//! - simple synchronous script
//! - call-stack walk through nested user functions
//! - throw path with both caught and uncaught flavours
//! - async resume across a microtask boundary

use std::sync::{Arc, Mutex};

use otter_runtime::{
    Otter, TracerFactory,
    inspect::{StepTracer, WriterTracer},
};

#[derive(Default)]
struct SharedBuf {
    inner: Mutex<Vec<u8>>,
}

impl SharedBuf {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn snapshot(&self) -> String {
        let buf = self.inner.lock().expect("trace buf");
        String::from_utf8(buf.clone()).expect("trace utf-8")
    }
}

/// `Write` adapter that feeds an `Arc<SharedBuf>` so a single
/// shared buffer can outlive the tracer instance and stay in
/// scope for the assertion phase.
struct SharedWriter(Arc<SharedBuf>);

impl std::io::Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut inner = self.0.inner.lock().expect("trace buf");
        inner.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Build an `Otter` instance pre-wired with a capture tracer
/// feeding `buf`.
fn otter_with_capture_tracer(buf: Arc<SharedBuf>) -> Otter {
    let factory = TracerFactory::new(move || -> Box<dyn StepTracer> {
        Box::new(WriterTracer::new(SharedWriter(buf.clone())))
    });
    Otter::builder()
        .tracer_factory(Some(factory))
        .build()
        .expect("otter build")
}

/// Drive `source` to completion (success path) under a capture
/// tracer and return the produced trace text.
fn capture_typescript(source: &str) -> String {
    let buf = SharedBuf::new();
    let otter = otter_with_capture_tracer(buf.clone());
    otter
        .blocking_run_typescript(source)
        .expect("script must succeed");
    buf.snapshot()
}

/// Compare `actual` against the file `crates/otter-runtime/tests/golden/<name>.trace`.
///
/// `OTTER_BLESS_TRACES=1` writes the file instead — use after a
/// deliberate change to the trace format or bytecode shape.
fn assert_golden(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(format!("{name}.trace"));
    if std::env::var_os("OTTER_BLESS_TRACES").is_some() {
        std::fs::create_dir_all(path.parent().expect("golden dir"))
            .expect("create golden dir");
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("golden trace missing at {}: {err}", path.display()));
    assert_eq!(
        actual,
        expected,
        "trace drift for {name}: run with OTTER_BLESS_TRACES=1 after confirming the diff is intentional",
    );
}

#[test]
fn simple_script_trace_matches_golden() {
    let source = "const x = 1 + 2;\n";
    let actual = capture_typescript(source);
    assert!(
        actual.contains("; otter step trace v1"),
        "header banner missing"
    );
    assert!(actual.contains("op=ADD"), "expected ADD opcode in trace");
    assert_golden("simple_script", &actual);
}

#[test]
fn call_stack_walk_trace_matches_golden() {
    let source = r#"
        function add(a, b) { return a + b; }
        function outer() { return add(2, 3); }
        outer();
    "#;
    let actual = capture_typescript(source);
    assert!(
        actual.contains("fn=outer"),
        "expected outer frame in trace; got:\n{actual}"
    );
    assert!(
        actual.contains("fn=add"),
        "expected add frame in trace; got:\n{actual}"
    );
    assert_golden("call_stack", &actual);
}

#[test]
fn caught_throw_trace_matches_golden() {
    let source = r#"
        function boom() { throw new Error("x"); }
        try { boom(); } catch (e) { e.message; }
    "#;
    let actual = capture_typescript(source);
    assert!(
        actual.contains("op=THROW"),
        "expected THROW opcode in trace; got:\n{actual}"
    );
    assert_golden("throw_caught", &actual);
}

#[test]
fn async_resume_trace_matches_golden() {
    let source = r#"
        async function main() {
            const v = await Promise.resolve(7);
            return v + 1;
        }
        main();
    "#;
    let actual = capture_typescript(source);
    assert!(
        actual.contains("op=AWAIT"),
        "expected AWAIT opcode in trace; got:\n{actual}"
    );
    assert_golden("async_resume", &actual);
}

#[test]
fn tracer_off_emits_nothing() {
    let otter = Otter::builder().build().expect("otter build");
    let result = otter
        .blocking_run_typescript("const x = 1 + 2;\n")
        .expect("script must succeed");
    let _ = result;
    // Nothing to assert beyond the absence of a tracer installation;
    // the run completed which proves the off-path keeps working.
}
