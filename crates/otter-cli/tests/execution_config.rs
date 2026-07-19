//! CLI integration coverage for owned runtime execution configuration.
//!
//! # Contents
//! - Explicit execution-tier selection through the public CLI.
//! - Shared trace and CPU-profiler configuration on the synchronous runtime path.
//! - Structured JIT event capture through an explicit default-off target.
//! - Atomic current-format JIT artifact bundle persistence.
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

#[cfg(target_arch = "aarch64")]
fn assert_persisted_assembly(path: &std::path::Path) {
    let assembly = std::fs::read_to_string(path).expect("read UTF-8 annotated assembly");
    let mut lines = assembly.lines();
    assert_eq!(
        lines.next(),
        Some("; otter jit aarch64 assembly"),
        "current assembly header"
    );
    assert_eq!(
        lines.next(),
        Some("; offset-basis=code.bin"),
        "assembly offsets use code.bin"
    );
    assert!(
        assembly.lines().any(|line| {
            line.strip_prefix('L')
                .and_then(|label| label.strip_suffix(':'))
                .is_some_and(|offset| {
                    offset.len() == 8 && offset.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
        }),
        "assembly retains stable branch labels:\n{assembly}"
    );
    assert!(
        assembly.lines().any(|line| {
            line.strip_prefix("+0x")
                .and_then(|instruction| instruction.split_once(':'))
                .is_some_and(|(offset, _)| {
                    offset.len() == 8 && offset.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
        }),
        "assembly retains exact native offsets:\n{assembly}"
    );
    assert!(
        assembly.contains("pc=")
            && assembly.contains("tier-op=")
            && assembly.contains("relocation "),
        "assembly retains code-map and symbolic relocation annotations:\n{assembly}"
    );
    for line in assembly.lines().filter(|line| line.contains("relocation ")) {
        let (_, rendered) = line
            .split_once(':')
            .unwrap_or_else(|| panic!("relocation lacks an exact offset: {line}"));
        assert!(
            rendered.trim_start().starts_with("relocation ") && !rendered.contains("0x"),
            "relocation must replace address-bearing instructions with a symbol: {line}"
        );
    }
    assert!(
        !assembly.contains("resolvedValue") && !assembly.contains("0xdead"),
        "assembly must redact resolved process values:\n{assembly}"
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
    assert!(
        !tmp.path().join("otter-jit-artifacts").exists(),
        "default-off runs must not create a JIT artifact directory"
    );
}

#[test]
fn structured_jit_events_are_typed() {
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

#[cfg(target_arch = "aarch64")]
#[test]
fn jit_artifacts_are_complete_and_offset_consistent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let artifacts_path = tmp.path().join("artifacts");
    let output = otter_command(tmp.path())
        .env("OTTER_JIT_OSR_THRESHOLD", "1")
        .arg(format!("--jit-artifacts={}", artifacts_path.display()))
        .arg("--jit-tier=template")
        .arg("--print")
        .arg(
            "function hot(n) { let s = 0; for (let i = 0; i < n; i = i + 1) \
             { s = s + i; } return s; } hot(40)",
        )
        .output()
        .expect("run with JIT artifact capture");
    assert_success(&output);
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "780");

    let index: serde_json::Value = serde_json::from_slice(
        &std::fs::read(artifacts_path.join("index.json")).expect("read artifact index"),
    )
    .expect("valid artifact index");
    assert_eq!(index["droppedBundles"], 0);
    assert_eq!(index["droppedBytes"], 0);
    assert_eq!(index["truncated"], false);
    assert!(
        index["retainedBytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0)
    );
    let directories = index["bundles"].as_array().expect("bundle directories");
    assert!(!directories.is_empty(), "at least one native compile");
    let directory_name = directories[0].as_str().expect("directory name");
    assert!(directory_name.starts_with("jit-0000-template-f"));
    assert!(directory_name.contains("-c"));
    let directory = artifacts_path.join(directory_name);

    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(directory.join("manifest.json")).expect("read artifact manifest"),
    )
    .expect("valid artifact manifest");
    assert_eq!(manifest["tier"], "template");
    assert_eq!(manifest["entry"]["kind"], "osr");
    assert_eq!(manifest["module"], "<eval>");
    assert!(manifest["functionId"].is_u64());
    assert!(manifest["codeObjectId"].is_u64());
    assert!(
        manifest["bytecodeBytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0)
    );
    let code_bytes = manifest["codeBytes"].as_u64().expect("native code size");
    assert!(code_bytes > 0);
    assert_eq!(manifest["exactCodeIsRuntimeLocal"], true);

    let present = manifest["filesPresent"].as_array().expect("present files");
    for name in [
        "manifest.json",
        "bytecode.txt",
        "template-plan.txt",
        "code.bin",
        "code-normalized.bin",
        "asm.txt",
        "code-map.json",
        "relocations.json",
        "safepoints.json",
    ] {
        assert!(
            present.iter().any(|value| value == name),
            "missing present inventory entry {name}"
        );
        assert!(directory.join(name).is_file(), "missing payload {name}");
    }
    let absent = manifest["filesAbsent"].as_array().expect("absent files");
    for name in ["optimized-ir.txt", "deopt.json"] {
        assert!(
            absent.iter().any(|value| value == name),
            "missing absent inventory entry {name}"
        );
        assert!(!directory.join(name).exists(), "unexpected payload {name}");
    }

    let code = std::fs::read(directory.join("code.bin")).expect("read code.bin");
    assert_eq!(code.len() as u64, code_bytes);
    let normalized =
        std::fs::read(directory.join("code-normalized.bin")).expect("read normalized code");
    assert!(normalized.starts_with(b"OTJNCODE"));
    assert_persisted_assembly(&directory.join("asm.txt"));
    let relocations_bytes =
        std::fs::read(directory.join("relocations.json")).expect("read relocations");
    let relocations: serde_json::Value =
        serde_json::from_slice(&relocations_bytes).expect("valid relocations");
    assert_eq!(relocations["offsetBasis"], "code.bin");
    assert!(relocations["relocations"].is_array());
    assert!(
        !String::from_utf8_lossy(&relocations_bytes).contains("resolvedValue"),
        "symbolic relocation output must not expose process addresses"
    );
    assert!(
        std::fs::read_to_string(directory.join("bytecode.txt"))
            .expect("read bytecode")
            .starts_with("; otter bytecode\n")
    );
    assert!(
        std::fs::read_to_string(directory.join("template-plan.txt"))
            .expect("read template plan")
            .starts_with("; otter template plan\n")
    );

    let code_map: serde_json::Value = serde_json::from_slice(
        &std::fs::read(directory.join("code-map.json")).expect("read code map"),
    )
    .expect("valid code map");
    let regions = code_map["regions"].as_array().expect("code-map regions");
    assert!(!regions.is_empty());
    for region in regions {
        let start = region["startOffset"].as_u64().expect("region start");
        let end = region["endOffset"].as_u64().expect("region end");
        assert!(start <= end && end <= code_bytes, "invalid region {region}");
    }
    let osr_pc = manifest["entry"]["pc"].as_u64().expect("manifest OSR PC");
    assert!(
        code_map["osrEntries"]
            .as_array()
            .is_some_and(|entries| entries
                .iter()
                .any(|entry| entry["logicalPc"].as_u64() == Some(osr_pc))),
        "code map must contain the manifest OSR entry"
    );

    let safepoints: serde_json::Value = serde_json::from_slice(
        &std::fs::read(directory.join("safepoints.json")).expect("read safepoints"),
    )
    .expect("valid safepoints");
    assert!(safepoints["safepoints"].is_array());
}

#[cfg(target_arch = "aarch64")]
fn capture_portable_template_code(
    root: &std::path::Path,
    directory_name: &str,
) -> (Vec<u8>, Vec<serde_json::Value>) {
    let artifacts_path = root.join(directory_name);
    let output = otter_command(root)
        .env("OTTER_JIT_OSR_THRESHOLD", "1")
        .arg(format!("--jit-artifacts={}", artifacts_path.display()))
        .arg("--jit-tier=template")
        .arg("--print")
        .arg(
            "function hot(n) { let s = 0; for (let i = 0; i < n; i = i + 1) \
             { s = s + i; } return s; } hot(40)",
        )
        .output()
        .expect("run portable artifact capture");
    assert_success(&output);

    let index: serde_json::Value = serde_json::from_slice(
        &std::fs::read(artifacts_path.join("index.json")).expect("read artifact index"),
    )
    .expect("valid artifact index");
    let bundle = index["bundles"][0].as_str().expect("first bundle");
    let bundle_path = artifacts_path.join(bundle);
    let normalized =
        std::fs::read(bundle_path.join("code-normalized.bin")).expect("read normalized code");
    let relocations: serde_json::Value = serde_json::from_slice(
        &std::fs::read(bundle_path.join("relocations.json")).expect("read relocations"),
    )
    .expect("valid relocations");
    let targets = relocations["relocations"]
        .as_array()
        .expect("relocation records")
        .iter()
        .map(|relocation| relocation["target"].clone())
        .collect();
    (normalized, targets)
}

#[cfg(target_arch = "aarch64")]
#[test]
fn normalized_jit_code_is_stable_across_processes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let first = capture_portable_template_code(tmp.path(), "portable-first");
    let second = capture_portable_template_code(tmp.path(), "portable-second");

    assert_eq!(first.0, second.0, "portable code changed across processes");
    assert_eq!(
        first.1, second.1,
        "symbolic target sequence changed across processes"
    );
}

#[cfg(target_arch = "aarch64")]
#[test]
fn abrupt_failure_still_writes_partial_jit_artifacts() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let artifacts_path = tmp.path().join("abrupt-artifacts");
    let output = otter_command(tmp.path())
        .env("OTTER_JIT_OSR_THRESHOLD", "1")
        .arg(format!("--jit-artifacts={}", artifacts_path.display()))
        .arg("--jit-tier=template")
        .arg("--eval")
        .arg(
            "function hot(n) { let s = 0; for (let i = 0; i < n; i = i + 1) \
             { s = s + i; } return s; } hot(40); throw new Error('expected-artifact-error');",
        )
        .output()
        .expect("run abrupt artifact capture");

    assert!(!output.status.success(), "fixture must fail");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("expected-artifact-error"),
        "original runtime error must remain primary: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let index: serde_json::Value = serde_json::from_slice(
        &std::fs::read(artifacts_path.join("index.json")).expect("read partial artifact index"),
    )
    .expect("valid partial artifact index");
    assert!(
        index["bundles"]
            .as_array()
            .is_some_and(|bundles| !bundles.is_empty()),
        "partial artifact batch retains successful compiles"
    );
    let bundle = index["bundles"][0]
        .as_str()
        .expect("first partial artifact bundle");
    assert_persisted_assembly(&artifacts_path.join(bundle).join("asm.txt"));
}

#[cfg(target_arch = "aarch64")]
#[test]
fn existing_artifact_target_is_not_overwritten() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let artifacts_path = tmp.path().join("existing-artifacts");
    std::fs::create_dir(&artifacts_path).expect("create existing target");
    let sentinel = artifacts_path.join("sentinel.txt");
    std::fs::write(&sentinel, "keep").expect("write sentinel");

    let output = otter_command(tmp.path())
        .env("OTTER_JIT_OSR_THRESHOLD", "1")
        .arg(format!("--jit-artifacts={}", artifacts_path.display()))
        .arg("--jit-tier=template")
        .arg("--print")
        .arg(
            "function hot(n) { let s = 0; for (let i = 0; i < n; i = i + 1) \
             { s = s + i; } return s; } hot(40)",
        )
        .output()
        .expect("run against existing artifact target");

    assert!(!output.status.success(), "existing target must fail closed");
    assert_eq!(
        std::fs::read_to_string(&sentinel).expect("read sentinel"),
        "keep"
    );
    assert_eq!(
        std::fs::read_dir(&artifacts_path)
            .expect("read existing target")
            .count(),
        1,
        "writer must not add files to an existing target"
    );
    assert!(
        std::fs::read_dir(tmp.path())
            .expect("read parent")
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".existing-artifacts.tmp-")),
        "writer must not leave a temporary directory"
    );
}

#[cfg(target_arch = "aarch64")]
#[test]
fn artifact_persistence_still_runs_when_event_write_fails() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let invalid_events_target = tmp.path().join("events-directory");
    let artifacts_path = tmp.path().join("artifacts-after-event-error");
    std::fs::create_dir(&invalid_events_target).expect("create invalid event target");

    let output = otter_command(tmp.path())
        .env("OTTER_JIT_OSR_THRESHOLD", "1")
        .arg(format!("--jit-events={}", invalid_events_target.display()))
        .arg(format!("--jit-artifacts={}", artifacts_path.display()))
        .arg("--jit-tier=template")
        .arg("--print")
        .arg(
            "function hot(n) { let s = 0; for (let i = 0; i < n; i = i + 1) \
             { s = s + i; } return s; } hot(40)",
        )
        .output()
        .expect("run independent diagnostic writes");

    assert!(!output.status.success(), "invalid event target must fail");
    let index: serde_json::Value = serde_json::from_slice(
        &std::fs::read(artifacts_path.join("index.json"))
            .expect("artifact channel still writes its index"),
    )
    .expect("valid artifact index");
    assert!(
        index["bundles"]
            .as_array()
            .is_some_and(|bundles| !bundles.is_empty()),
        "event persistence failure must not suppress artifact persistence"
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
    assert!(trace_text.starts_with("; otter step trace\n"));
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
