//! `otter --json <file>` per-category parity coverage for
//! ENGINE_REFACTOR_EXECUTION_PLAN §P2.3.
//!
//! Each fixture trips a diagnostic in a different plan-mandated
//! category. The CLI is invoked with `--json`, stdout/stderr is
//! parsed via `serde_json`, and we assert the typed wire shape:
//!
//! - `error_schema_version == 1`,
//! - error envelope deserializes into a known
//!   [`otter_runtime::OtterError`] variant,
//! - the embedded diagnostic (when present) carries a
//!   [`otter_runtime::DiagnosticCode`] from the closed set
//!   matching the expected category.
//!
//! Errors print to stderr, not stdout — `--json` is the
//! machine-readable channel even for failures, so the CLI flushes
//! the envelope to stderr and exits non-zero.

use std::process::Command;

use otter_runtime::{DiagnosticCategory, DiagnosticCode, OtterError};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Envelope {
    error_schema_version: u32,
    error: OtterError,
}

fn run_with_json(source: &str, file_name: &str) -> (i32, Envelope) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(file_name);
    std::fs::write(&path, source).expect("write fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_otter"))
        .arg("--json")
        .arg(&path)
        .output()
        .expect("spawn otter");
    let code = output.status.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Envelope = serde_json::from_str(stderr.trim()).unwrap_or_else(|err| {
        panic!(
            "stderr is not a valid `--json` envelope: {err}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            stderr
        )
    });
    assert_eq!(envelope.error_schema_version, 1);
    (code, envelope)
}

fn assert_compile_category(env: Envelope, expected: DiagnosticCategory) {
    let diagnostics = match env.error {
        OtterError::Compile { diagnostics } => diagnostics,
        other => panic!("expected Compile error, got {other:?}"),
    };
    let first = diagnostics.first().expect("at least one diagnostic");
    let code = DiagnosticCode::parse(&first.code)
        .unwrap_or_else(|| panic!("code {:?} not in closed set", first.code));
    assert_eq!(
        code.category(),
        expected,
        "category mismatch for {} (got {:?})",
        first.code,
        code.category()
    );
}

#[test]
fn parse_category_json_envelope() {
    let (status, env) = run_with_json("const = ;\n", "entry.ts");
    assert_ne!(status, 0);
    assert_compile_category(env, DiagnosticCategory::Parse);
}

#[test]
fn parse_category_ts_unsupported_envelope() {
    // `enum` lowering rejected — `TS_UNSUPPORTED` lives in the
    // Parse bucket per the plan's category mapping.
    let (status, env) = run_with_json("enum E { A }\n", "entry.ts");
    assert_ne!(status, 0);
    assert_compile_category(env, DiagnosticCategory::Parse);
}

#[test]
fn resolve_category_json_envelope() {
    // Missing module triggers MODULE_RESOLUTION_ERROR.
    let (status, env) = run_with_json(
        "import { x } from \"./does-not-exist.ts\";\nconsole.log(x);\n",
        "entry.ts",
    );
    assert_ne!(status, 0);
    assert_compile_category(env, DiagnosticCategory::Resolve);
}

#[test]
fn runtime_category_json_envelope() {
    let (status, env) = run_with_json("throw new Error(\"boom\");\n", "entry.ts");
    assert_ne!(status, 0);
    let diagnostic = match env.error {
        OtterError::Runtime { diagnostic } => *diagnostic,
        other => panic!("expected Runtime error, got {other:?}"),
    };
    let code = DiagnosticCode::parse(&diagnostic.code)
        .unwrap_or_else(|| panic!("code {:?} not in closed set", diagnostic.code));
    assert_eq!(code.category(), DiagnosticCategory::Runtime);
    assert_eq!(code, DiagnosticCode::Uncaught);
}

#[test]
fn permission_category_json_envelope() {
    // Net capability denied: HTTPS dynamic import triggers
    // MODULE_CAPABILITY_DENIED. The default capability set
    // denies `net`, so a static-resolved https import surfaces
    // through `map_graph_error` as a Compile-side capability
    // diagnostic.
    let (status, env) = run_with_json(
        "import * as ns from \"https://example.com/x.ts\";\nconsole.log(ns);\n",
        "entry.ts",
    );
    assert_ne!(status, 0);
    assert_compile_category(env, DiagnosticCategory::Permission);
}
