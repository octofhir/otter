//! CLI integration coverage for the unified `otter run` execution path.
//!
//! # Contents
//! - Direct file shorthand and explicit `run` file execution.
//! - Package script execution through the runtime path.
//! - Workspace package bin execution through the runtime path.
//!
//! # Invariants
//! - These tests invoke the built `otter` binary instead of private helpers.
//! - Script and bin execution must observe Node-like `process.argv` and
//!   project-root `process.cwd()` without shell subprocess fallback.

use std::process::Command;

use serde_json::Value;

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crate lives under workspace crates/")
        .to_path_buf()
}

fn otter_command(root: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_otter"));
    command.current_dir(root);
    command
}

fn assert_success(output: std::process::Output) {
    assert!(
        output.status.success(),
        "otter failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn file_shorthand_and_run_use_same_runtime_argv_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("entry.ts"),
        r#"
function fail() { process.exit(41); }
if (process.argv[1].indexOf("entry.ts") === -1) fail();
if (process.argv[2] !== "alpha") fail();
"#,
    )
    .expect("write entry");

    let shorthand = otter_command(tmp.path())
        .arg("entry.ts")
        .arg("alpha")
        .output()
        .expect("run otter shorthand");
    assert_success(shorthand);

    let explicit = otter_command(tmp.path())
        .arg("run")
        .arg("entry.ts")
        .arg("alpha")
        .output()
        .expect("run otter run");
    assert_success(explicit);
}

#[test]
fn run_fixture_covers_ts_imports_workspace_import_and_json_module() {
    let fixture = repo_root().join("tests/fixtures/pkg/development-loop");

    let output = otter_command(&fixture)
        .arg("run")
        .arg("entry.ts")
        .output()
        .expect("run fixture entry");
    assert_success(output);
}

#[test]
fn package_script_runs_file_through_runtime_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("scripts")).expect("mkdir scripts");
    std::fs::write(
        tmp.path().join("package.json"),
        r#"{"name":"app","type":"module","scripts":{"check":"node scripts/check.ts from-script"}}"#,
    )
    .expect("write package");
    let cwd = tmp.path().canonicalize().expect("canonical cwd");
    let cwd_literal = serde_json::to_string(&cwd.to_string_lossy()).expect("json cwd");
    std::fs::write(
        tmp.path().join("scripts/check.ts"),
        format!(
            r#"
function fail() {{ process.exit(42); }}
if (process.cwd() !== {cwd_literal}) fail();
if (process.argv[1].indexOf("check.ts") === -1) fail();
if (process.argv[2] !== "from-script") fail();
if (process.argv[3] !== "from-cli") fail();
"#
        ),
    )
    .expect("write script");

    let output = otter_command(tmp.path())
        .arg("run")
        .arg("check")
        .arg("from-cli")
        .output()
        .expect("run package script");
    assert_success(output);
}

#[test]
fn failing_run_script_emits_stable_json_diagnostic() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("boom.ts"),
        r#"throw new Error("stable boom");"#,
    )
    .expect("write boom");

    let output = otter_command(tmp.path())
        .arg("--json")
        .arg("run")
        .arg("boom.ts")
        .output()
        .expect("run failing script");

    assert!(
        !output.status.success(),
        "expected failure\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value =
        serde_json::from_slice(&output.stderr).expect("stderr is JSON diagnostic envelope");
    assert_eq!(envelope["error_schema_version"], 1);
    assert_eq!(envelope["error"]["kind"], "runtime");
    assert_eq!(envelope["error"]["diagnostic"]["code"], "UNCAUGHT");
}

#[test]
fn workspace_package_bin_runs_through_runtime_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("packages/tool")).expect("mkdir package");
    std::fs::write(
        tmp.path().join("package.json"),
        r#"{"name":"app","type":"module","workspaces":["packages/*"]}"#,
    )
    .expect("write root package");
    std::fs::write(
        tmp.path().join("packages/tool/package.json"),
        r#"{"name":"tool","version":"1.0.0","type":"module","bin":"./tool.ts"}"#,
    )
    .expect("write tool package");
    std::fs::write(
        tmp.path().join("packages/tool/tool.ts"),
        r#"
function fail() { process.exit(43); }
if (process.argv[1].indexOf("tool.ts") === -1) fail();
if (process.argv[2] !== "from-cli") fail();
"#,
    )
    .expect("write tool");

    let output = otter_command(tmp.path())
        .arg("run")
        .arg("--bin")
        .arg("tool")
        .arg("from-cli")
        .output()
        .expect("run workspace bin");
    assert_success(output);
}

#[test]
fn package_script_resolves_bare_local_bin_command() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("packages/tool")).expect("mkdir package");
    std::fs::write(
        tmp.path().join("package.json"),
        r#"{"name":"app","type":"module","workspaces":["packages/*"],"scripts":{"use-tool":"tool from-script"}}"#,
    )
    .expect("write root package");
    std::fs::write(
        tmp.path().join("packages/tool/package.json"),
        r#"{"name":"tool","version":"1.0.0","type":"module","bin":"./tool.ts"}"#,
    )
    .expect("write tool package");
    std::fs::write(
        tmp.path().join("packages/tool/tool.ts"),
        r#"
function fail() { process.exit(44); }
if (process.argv[1].indexOf("tool.ts") === -1) fail();
if (process.argv[2] !== "from-script") fail();
if (process.argv[3] !== "from-cli") fail();
"#,
    )
    .expect("write tool");

    let output = otter_command(tmp.path())
        .arg("run")
        .arg("use-tool")
        .arg("from-cli")
        .output()
        .expect("run package script local bin");
    assert_success(output);
}
