//! End-to-end CLI diagnostic rendering tests.
//!
//! These tests invoke the `otter` binary on small fixture scripts and snapshot
//! the stderr output with `insta`. The snapshots cover the three paths that
//! matter for the user-facing reporter:
//!
//!   * Plain JavaScript throw — V8-style stack header + miette snippet.
//!   * TypeScript throw — same shape, but rendered against the ORIGINAL
//!     pre-strip TS source (type annotations intact).
//!   * A non-Error thrown primitive — falls back to the plain legacy error
//!     line (no miette snippet), so the CLI still shows something sane.
//!
//! Running the snapshots:
//!
//! ```bash
//! # Review / accept updates interactively
//! cargo insta test -p otterjs
//! cargo insta review
//!
//! # Or straight-through (snapshots as a green/red test):
//! cargo test -p otterjs --test cli_diagnostic
//! ```

use assert_cmd::Command;
use insta::assert_snapshot;
use std::fs;
use std::io::Write;

/// Runs `otter run <fixture>` with a fixed, deterministic filename and
/// returns `(exit_code, stderr)`. The filename is derived from the caller's
/// `name` argument so two tests never clobber the same path, but it does
/// NOT include a random suffix — this keeps the snapshot stable.
///
/// Platform-specific noise (TMPDIR path, ANSI codes, terminal width) is
/// suppressed via env vars so the captured stderr is byte-for-byte
/// reproducible.
fn run_fixture(name: &str, extension: &str, source: &str) -> (i32, String) {
    // Use a per-test subdirectory under the target's tempdir so parallel
    // tests don't race on the same filename.
    let tmp_dir = std::env::temp_dir().join(format!("otter_cli_diag_{name}"));
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).expect("create tmp dir");
    let path = tmp_dir.join(format!("fixture{extension}"));
    let mut file = fs::File::create(&path).expect("create fixture file");
    file.write_all(source.as_bytes()).expect("write fixture");
    drop(file);

    let output = Command::cargo_bin("otter")
        .expect("otter binary built")
        .arg("run")
        .arg(&path)
        // Disable color and force a wide terminal so miette emits a single
        // unwrapped diagnostic — without this the snippet lines wrap at
        // whatever the developer's terminal width happens to be.
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0")
        .env("TERM", "dumb")
        .env("MIETTE_WIDTH", "200")
        .env("COLUMNS", "200")
        .output()
        .expect("run otter");

    let code = output.status.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    // Best-effort cleanup. Snapshot success doesn't depend on this working.
    let _ = fs::remove_dir_all(&tmp_dir);

    (code, stderr)
}

/// Applies snapshot-stability filters. Since `run_fixture` uses a fixed
/// parent directory name and a fixed basename, the only variable bit left
/// is the OS-specific `/tmp` / `/private/tmp` / `/var/folders/...` prefix.
/// We collapse that to `<TMPDIR>/` wherever it appears.
fn insta_filters() -> Vec<(&'static str, &'static str)> {
    vec![
        // `file:///...path.../otter_cli_diag_<test>/fixture.ext` →
        // `file://<TMPDIR>/fixture.ext`.
        (
            r"file:///[^\s:\]]*otter_cli_diag_[a-z_0-9]+/",
            "file://<TMPDIR>/",
        ),
        // Same but bare (no file:// prefix) — miette's `[path:line:col]`.
        (r"/[^\s\[:\]]*otter_cli_diag_[a-z_0-9]+/", "<TMPDIR>/"),
    ]
}

#[test]
fn js_uncaught_type_error_renders_miette_snippet() {
    let src = r#"function doStuff() {
  throw new TypeError("boom");
}
function main() {
  doStuff();
}
main();
"#;
    let (code, stderr) = run_fixture("js_type_error", ".js", src);
    assert_eq!(code, 1, "otter should exit 1 on uncaught throw");

    insta::with_settings!({
        filters => insta_filters(),
        description => "V8 stack header + miette snippet pointing at the throw site",
    }, {
        assert_snapshot!("js_uncaught_type_error", stderr);
    });
}

#[test]
fn ts_uncaught_throw_shows_original_typescript_source() {
    // A TS fixture with interface + generic + type assertion. The snapshot
    // MUST show the pre-strip TS source (types intact) — not the post-oxc
    // JavaScript the compiler actually parsed. This is the load-bearing
    // assertion for Phase 0/1/2 of CLI_REPORTER_PLAN.md.
    let src = r#"interface Box<T> { value: T }
function unbox<T>(b: Box<T>): T {
  throw new TypeError("boom");
}
function main(): void {
  unbox<number>({ value: 1 });
}
main();
"#;
    let (code, stderr) = run_fixture("ts_throw", ".ts", src);
    assert_eq!(code, 1, "otter should exit 1 on uncaught throw from TS");

    insta::with_settings!({
        filters => insta_filters(),
        description => "TS source (types intact) rendered via miette",
    }, {
        assert_snapshot!("ts_uncaught_throw", stderr);
    });
}

#[test]
fn js_property_access_on_undefined_underlines_member_expression() {
    // Reading a property off `undefined` is the spec's RequireObjectCoercible
    // failure (§7.1.18) — should produce a TypeError whose `^` lands on the
    // member-access expression itself, NOT the enclosing statement. Tests
    // that the per-opcode source-map entries on `GetProperty` are recorded
    // correctly and survive the dispatch-level error promotion.
    let src = r#"function inner() {
  let x;
  return x.foo;
}
function outer() {
  return inner();
}
outer();
"#;
    let (code, stderr) = run_fixture("js_undef_member", ".js", src);
    assert_eq!(
        code, 1,
        "otter should exit 1 on TypeError from undefined access"
    );

    insta::with_settings!({
        filters => insta_filters(),
        description => "TypeError underline lands on `x.foo`, all 3 frames present",
    }, {
        assert_snapshot!("js_undefined_member_access", stderr);
    });
}

#[test]
fn js_uncaught_primitive_throw_falls_back_to_plain_error() {
    // Throwing a primitive (string) doesn't produce a structured Error with a
    // frames slot, so we fall back to the legacy `error: ...` line. This is
    // intentional — miette's snippet needs a (name, message, source) triple
    // that only Error-like objects carry.
    let src = "throw 'plain string';\n";
    let (code, stderr) = run_fixture("js_primitive", ".js", src);
    assert_eq!(code, 1);

    insta::with_settings!({
        filters => insta_filters(),
        description => "Plain primitive throw — no miette snippet, legacy line",
    }, {
        assert_snapshot!("js_primitive_throw_plain", stderr);
    });
}
