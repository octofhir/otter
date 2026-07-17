//! CLI integration coverage for owned runtime execution configuration.
//!
//! # Contents
//! - Explicit execution-tier selection through the public CLI.
//! - Shared trace and CPU-profiler configuration on the synchronous runtime path.
//! - Structured JIT event capture through an explicit default-off target.
//!
//! # Invariants
//! - Tests invoke the built binary instead of private configuration helpers.
//! - Enabling CPU profiling must not bypass global runtime execution settings.

use std::process::{Command, Output};

fn otter_command(root: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_otter"));
    command.current_dir(root);
    command
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "otter failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn explicit_jit_tier_modes_are_accepted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for tier in ["production-tiered", "template", "interpreter"] {
        let info = otter_command(tmp.path())
            .arg(format!("--jit-tier={tier}"))
            .arg("--json")
            .arg("info")
            .output()
            .expect("report explicit JIT tier");
        assert_success(&info);
        let info: serde_json::Value =
            serde_json::from_slice(&info.stdout).expect("valid info JSON");
        assert_eq!(info["jit_tier"], tier);
        assert_eq!(info["interpreter_only"], tier == "interpreter");

        let output = otter_command(tmp.path())
            .arg(format!("--jit-tier={tier}"))
            .arg("--print")
            .arg("40 + 2")
            .output()
            .expect("run explicit JIT tier");
        assert_success(&output);
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
    }
    assert!(
        !tmp.path().join("otter-jit-events.json").exists(),
        "default-off runs must not create a diagnostics artifact"
    );
}

#[test]
fn structured_jit_events_are_versioned_and_typed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let events_path = tmp.path().join("otter-jit-events.json");
    let output = otter_command(tmp.path())
        .env("OTTER_JIT_OSR_THRESHOLD", "1")
        .arg("--jit-events")
        .arg("--jit-tier=template")
        .arg("--print")
        .arg(
            "function hot(n) { let s = 0; for (let i = 0; i < n; i = i + 1) \
             { s = s + i; } return s; } hot(40)",
        )
        .output()
        .expect("run with structured JIT events");
    assert_success(&output);
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "780");

    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(events_path).expect("read JIT events"))
            .expect("valid JIT events JSON");
    assert_eq!(report["otterJitDebugSchemaVersion"], 1);
    let events = report["events"].as_array().expect("events array");
    assert_eq!(report["droppedEvents"], 0);
    assert_eq!(report["truncated"], false);
    assert!(events.len() >= 2);
    assert_eq!(events[0]["type"], "compilePrepared");
    assert_eq!(events[0]["tier"], "template");
    assert_eq!(events[0]["target"]["kind"], "osr");
    assert!(events[0]["functionId"].is_u64());
    assert!(events[0].get("function_id").is_none());
    assert_eq!(events[1]["type"], "compileFinished");
    assert_eq!(events[1]["outcome"]["kind"], "compiled");
    assert!(events[1]["outcome"]["codeBytes"].is_u64());
}

#[test]
fn abrupt_failure_still_writes_partial_jit_events() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let events_path = tmp.path().join("jit-events-error.json");
    let output = otter_command(tmp.path())
        .env("OTTER_JIT_OSR_THRESHOLD", "1")
        .arg(format!("--jit-events={}", events_path.display()))
        .arg("--jit-tier=template")
        .arg("--eval")
        .arg(
            "function hot(n) { let s = 0; for (let i = 0; i < n; i = i + 1) \
             { s = s + i; } return s; } hot(40); throw new Error('expected');",
        )
        .output()
        .expect("run abrupt JIT event capture");
    assert!(!output.status.success(), "fixture must fail");

    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(events_path).expect("read partial JIT events"))
            .expect("valid partial JIT events JSON");
    assert_eq!(report["otterJitDebugSchemaVersion"], 1);
    assert!(
        report["events"].as_array().is_some_and(|events| events
            .iter()
            .any(|event| event["type"] == "compileFinished")),
        "partial report retains compile events"
    );
}

#[test]
fn command_timeout_does_not_fabricate_an_empty_jit_report() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let events_path = tmp.path().join("jit-events-timeout.json");
    let output = otter_command(tmp.path())
        .arg("--timeout=1")
        .arg(format!("--jit-events={}", events_path.display()))
        .arg("--jit-tier=template")
        .arg("--eval")
        .arg("while (true) {}")
        .output()
        .expect("run timed-out JIT event capture");

    assert!(!output.status.success(), "fixture must time out");
    assert!(
        !events_path.exists(),
        "timeout without an isolate report must not create a valid-looking empty artifact"
    );
}

#[test]
fn late_timeout_preserves_earlier_file_jit_events() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let first = tmp.path().join("first.js");
    let second = tmp.path().join("second.js");
    let events_path = tmp.path().join("jit-events-sequence-timeout.json");
    std::fs::write(
        &first,
        "function hot(n) { let s = 0; for (let i = 0; i < n; i = i + 1) \
         { s = s + i; } return s; } hot(40);",
    )
    .expect("write first entry");
    std::fs::write(&second, "while (true) {}").expect("write timed-out entry");

    let output = otter_command(tmp.path())
        .env("OTTER_JIT_OSR_THRESHOLD", "1")
        .arg("--timeout=1")
        .arg(format!("--jit-events={}", events_path.display()))
        .arg("--jit-tier=template")
        .arg("--allow-read=*")
        .arg("first.js")
        .arg("second.js")
        .output()
        .expect("run multi-file timeout capture");

    assert!(!output.status.success(), "second fixture must time out");
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(events_path).expect("read retained JIT events"))
            .expect("valid retained JIT events JSON");
    assert!(
        report["events"].as_array().is_some_and(|events| events
            .iter()
            .any(|event| event["type"] == "compileFinished")),
        "the first file's compile events survive a later report-less timeout"
    );
}

#[test]
fn late_input_error_preserves_earlier_file_jit_events() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let first = tmp.path().join("first.js");
    let invalid = tmp.path().join("invalid.js");
    let events_path = tmp.path().join("jit-events-sequence-input-error.json");
    std::fs::write(
        &first,
        "function hot(n) { let s = 0; for (let i = 0; i < n; i = i + 1) \
         { s = s + i; } return s; } hot(40);",
    )
    .expect("write first entry");
    std::fs::create_dir(&invalid).expect("create invalid source directory");

    let output = otter_command(tmp.path())
        .env("OTTER_JIT_OSR_THRESHOLD", "1")
        .arg(format!("--jit-events={}", events_path.display()))
        .arg("--jit-tier=template")
        .arg("--allow-read=*")
        .arg("first.js")
        .arg("invalid.js")
        .output()
        .expect("run multi-file input failure capture");

    assert!(!output.status.success(), "second fixture must fail to load");
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(events_path).expect("read retained JIT events"))
            .expect("valid retained JIT events JSON");
    assert!(
        report["events"].as_array().is_some_and(|events| events
            .iter()
            .any(|event| event["type"] == "compileFinished")),
        "the first file's compile events survive a later pre-execution input error"
    );
}

#[test]
fn cpu_profile_path_preserves_trace_configuration() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let entry = tmp.path().join("entry.js");
    let trace = tmp.path().join("execution.trace");
    let profile_dir = tmp.path().join("profiles");
    std::fs::write(
        &entry,
        r#"
let total = 0;
for (let index = 0; index < 200; index = index + 1) {
  total = total + index;
}
if (total !== 19900) throw new Error("bad total");
"#,
    )
    .expect("write entry");

    let output = otter_command(tmp.path())
        .arg(format!("--trace={}", trace.display()))
        .arg("--jit-tier=interpreter")
        .arg("--allow-read=*")
        .arg("run")
        .arg("entry.js")
        .arg("--cpu-prof")
        .arg("--cpu-prof-dir")
        .arg(&profile_dir)
        .arg("--cpu-prof-interval")
        .arg("1")
        .arg("--cpu-prof-name")
        .arg("owned-config")
        .output()
        .expect("run traced CPU profile");
    assert_success(&output);

    let trace_text = std::fs::read_to_string(&trace).expect("read trace");
    assert!(trace_text.starts_with("; otter step trace v1\n"));
    assert!(trace_text.lines().count() > 1, "trace contains VM steps");

    let cpuprofile = profile_dir.join("owned-config.cpuprofile");
    let folded = profile_dir.join("owned-config.folded");
    let profile: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cpuprofile).expect("read cpuprofile"))
            .expect("valid Chrome CPU profile");
    assert!(profile["nodes"].is_array());
    assert!(profile["samples"].is_array());
    assert!(folded.is_file(), "folded stack artifact exists");
}
