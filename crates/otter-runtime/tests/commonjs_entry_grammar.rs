//! Grammar of a CommonJS entry file.
//!
//! # Contents
//! - A `.js` entry runs under the CommonJS wrapper, where a top-level `return`
//!   is legal.
//! - A genuine parse failure in that entry is still reported as a syntax error.
//!
//! # Invariants
//! - Module detection never turns CommonJS-only syntax into a parse failure: a
//!   source that does not parse as a module is CommonJS, which is what Node's
//!   own `.js` resolution says.
//! - The wrapper compile is what reports a real syntax error, so the diagnostic
//!   keeps its own code rather than arriving as a loader failure.

use std::path::Path;

use otter_runtime::{CapabilitySet, OtterError, Runtime};

fn commonjs_runtime() -> Runtime {
    Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_nodejs_modules()
        .build()
        .expect("CommonJS runtime")
}

fn write_entry(dir: &Path, name: &str, source: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, source).expect("write entry");
    path
}

#[test]
fn top_level_return_ends_a_javascript_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = write_entry(
        dir.path(),
        "entry.js",
        r#"
        globalThis.__reached = "before";
        if (globalThis.__reached === "before") {
            return;
        }
        globalThis.__reached = "after";
        "#,
    );

    commonjs_runtime()
        .run_file(&entry)
        .expect("a top-level return is legal inside the CommonJS wrapper");
}

#[test]
fn broken_javascript_entry_reports_a_syntax_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = write_entry(dir.path(), "entry.js", "const value = ;\n");

    let error = commonjs_runtime()
        .run_file(&entry)
        .expect_err("a malformed entry must not load");
    match error {
        OtterError::Internal { code, message } => {
            assert_eq!(code, "SYNTAX_ERROR", "message was: {message}");
            assert!(
                !message.contains("CompileError") && !message.contains("diagnostics:"),
                "the diagnostic must read as a parser message, got: {message}"
            );
        }
        other => panic!("expected a syntax diagnostic, got {other:?}"),
    }
}
