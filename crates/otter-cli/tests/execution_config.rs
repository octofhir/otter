//! CLI integration coverage for owned runtime execution configuration.
//!
//! # Contents
//! - Explicit execution-tier selection through the public CLI.
//! - Shared trace and CPU-profiler configuration on the synchronous runtime path.
//! - Structured JIT event capture through an explicit default-off target.
//! - Atomic current-format JIT artifact bundle persistence.
//! - `-p`/`-e` output to a standard stream whose reader has gone (a closed
//!   pipe, as `| head -1` leaves it): the exit code and stderr Node gives.
//!
//! # Invariants
//! - Tests invoke the built binary instead of private configuration helpers.
//! - Enabling CPU profiling must not bypass global runtime execution settings.
//! - A closed-pipe case writes far more than a pipe buffers, so the process is
//!   still writing when its reader leaves, however the two are scheduled.

use std::io::Read;
use std::process::{Command, Output, Stdio};

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

/// The standard stream whose reader leaves after its first byte.
#[derive(Clone, Copy)]
enum ClosedStream {
    Stdout,
    Stderr,
}

/// Run `otter` with `args`, read one byte of `closed` and close that pipe
/// while the process still has more to write to it — what `| head -c 1` does
/// — then collect how the process ends.
fn output_after_reader_leaves(
    root: &std::path::Path,
    args: &[&str],
    closed: ClosedStream,
) -> Output {
    let mut child = otter_command(root)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn otter");
    let mut first = [0_u8; 1];
    match closed {
        ClosedStream::Stdout => {
            let mut stdout = child.stdout.take().expect("piped stdout");
            stdout.read_exact(&mut first).expect("otter writes stdout");
        }
        ClosedStream::Stderr => {
            let mut stderr = child.stderr.take().expect("piped stderr");
            stderr.read_exact(&mut first).expect("otter writes stderr");
        }
    }
    child.wait_with_output().expect("otter exits")
}

/// The process left with `code` and wrote nothing to standard error: no
/// panic, no report of the broken pipe.
fn assert_quiet_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "stderr:\n{}",
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
        assembly.lines().any(|line| {
            line.trim_start().starts_with("; region kind=") && line.contains(" range=+0x")
        }) && assembly.contains("relocation "),
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
fn production_is_the_only_normal_cli_policy() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let info = otter_command(tmp.path())
        .arg("--json")
        .arg("info")
        .output()
        .expect("report production tier");
    assert_success(&info);
    let info: serde_json::Value = serde_json::from_slice(&info.stdout).expect("valid info JSON");
    assert_eq!(info["execution_mode"], "production");

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
fn jitless_selects_the_tier_below_optimizing_compilation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let info = otter_command(tmp.path())
        .arg("--jitless")
        .arg("--json")
        .arg("info")
        .output()
        .expect("report jitless tier");
    assert_success(&info);
    let info: serde_json::Value = serde_json::from_slice(&info.stdout).expect("valid info JSON");
    assert_eq!(info["execution_mode"], "jitless");

    let output = otter_command(tmp.path())
        .arg("--jitless")
        .arg("--print")
        .arg("40 + 2")
        .output()
        .expect("run without native tiers");
    assert_success(&output);
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
fn structured_jit_events_are_typed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let events_path = tmp.path().join("otter-jit-events.json");
    let output = otter_command(tmp.path())
        .arg("--jit-events")
        .arg("--print")
        .arg(
            "function hot(n) { let s = 0; for (let i = 0; i < n; i = i + 1) \
             { s = s + i; } return s; } hot(20000)",
        )
        .output()
        .expect("run with structured JIT events");
    assert_success(&output);
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "199990000");

    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(events_path).expect("read JIT events"))
            .expect("valid JIT events JSON");
    let events = report["events"].as_array().expect("events array");
    assert_eq!(report["droppedEvents"], 0);
    assert_eq!(report["truncated"], false);
    assert!(events.len() >= 2);
    assert_eq!(events[0]["type"], "compilePrepared");
    assert!(matches!(
        events[0]["tier"].as_str(),
        Some("template" | "optimizing")
    ));
    assert_eq!(events[0]["target"]["kind"], "osr");
    assert!(events[0]["functionId"].is_u64());
    assert!(events[0].get("function_id").is_none());
    // An optimizing compile runs in the background, so a baseline compile
    // may be prepared and finish in between: a finish follows its own
    // preparation, not the event before it.
    let finished = events
        .iter()
        .position(|event| event["type"] == "compileFinished")
        .expect("a finished compile");
    assert_eq!(events[finished]["outcome"]["kind"], "compiled");
    assert!(events[finished]["outcome"]["codeBytes"].is_u64());
    assert!(events[..finished].iter().any(|event| {
        event["type"] == "compilePrepared"
            && event["tier"] == events[finished]["tier"]
            && event["functionId"] == events[finished]["functionId"]
    }));
}

#[test]
fn abrupt_failure_still_writes_partial_jit_events() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let events_path = tmp.path().join("jit-events-error.json");
    let output = otter_command(tmp.path())
        .arg(format!("--jit-events={}", events_path.display()))
        .arg("--eval")
        .arg(
            "function hot(n) { let s = 0; for (let i = 0; i < n; i = i + 1) \
             { s = s + i; } return s; } hot(20000); throw new Error('expected');",
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
         { s = s + i; } return s; } hot(20000);",
    )
    .expect("write first entry");
    std::fs::write(&second, "while (true) {}").expect("write timed-out entry");

    let output = otter_command(tmp.path())
        .arg("--timeout=1")
        .arg(format!("--jit-events={}", events_path.display()))
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
         { s = s + i; } return s; } hot(20000);",
    )
    .expect("write first entry");
    std::fs::create_dir(&invalid).expect("create invalid source directory");

    let output = otter_command(tmp.path())
        .arg(format!("--jit-events={}", events_path.display()))
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
        .arg(format!("--jit-artifacts={}", artifacts_path.display()))
        .arg("--print")
        .arg(
            "function hot(n) { let s = 0; for (let i = 0; i < n; i = i + 1) \
             { s = s + i; } return s; } hot(20000)",
        )
        .output()
        .expect("run with JIT artifact capture");
    assert_success(&output);
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "199990000");

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
    assert!(directory_name.starts_with("jit-0000-"));
    assert!(directory_name.contains("-c"));
    let directory = artifacts_path.join(directory_name);

    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(directory.join("manifest.json")).expect("read artifact manifest"),
    )
    .expect("valid artifact manifest");
    assert!(matches!(
        manifest["tier"].as_str(),
        Some("template" | "optimizing")
    ));
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
    let tier_file = if manifest["tier"] == "template" {
        "template-plan.txt"
    } else {
        "optimized-ir.txt"
    };
    assert!(
        present.iter().any(|value| value == tier_file),
        "missing tier input {tier_file}"
    );
    assert!(directory.join(tier_file).is_file());
    if manifest["tier"] == "optimizing" {
        assert!(directory.join("deopt.json").is_file());
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
    let tier_input = std::fs::read_to_string(directory.join(tier_file)).expect("read tier input");
    if manifest["tier"] == "template" {
        assert!(tier_input.starts_with("; otter template plan\n"));
    } else {
        assert!(tier_input.starts_with("; otter graph\n"));
    }

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
    assert!(safepoints["records"].is_array());
}

#[cfg(target_arch = "aarch64")]
fn capture_portable_production_code(
    root: &std::path::Path,
    directory_name: &str,
) -> (Vec<u8>, Vec<serde_json::Value>) {
    let artifacts_path = root.join(directory_name);
    let output = otter_command(root)
        .arg(format!("--jit-artifacts={}", artifacts_path.display()))
        .arg("--print")
        .arg(
            "function hot(n) { let s = 0; for (let i = 0; i < n; i = i + 1) \
             { s = s + i; } return s; } hot(20000)",
        )
        .output()
        .expect("run portable artifact capture");
    assert_success(&output);

    let index: serde_json::Value = serde_json::from_slice(
        &std::fs::read(artifacts_path.join("index.json")).expect("read artifact index"),
    )
    .expect("valid artifact index");
    // The baseline compile runs on the isolate thread; an optimizing one in
    // the background may or may not finish before the process ends.
    let bundle = index["bundles"]
        .as_array()
        .expect("bundle directories")
        .iter()
        .filter_map(|bundle| bundle.as_str())
        .find(|bundle| bundle.contains("-template-"))
        .expect("a baseline bundle");
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
    let first = capture_portable_production_code(tmp.path(), "portable-first");
    let second = capture_portable_production_code(tmp.path(), "portable-second");

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
        .arg(format!("--jit-artifacts={}", artifacts_path.display()))
        .arg("--eval")
        .arg(
            "function hot(n) { let s = 0; for (let i = 0; i < n; i = i + 1) \
             { s = s + i; } return s; } hot(20000); throw new Error('expected-artifact-error');",
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
        .arg(format!("--jit-artifacts={}", artifacts_path.display()))
        .arg("--print")
        .arg(
            "function hot(n) { let s = 0; for (let i = 0; i < n; i = i + 1) \
             { s = s + i; } return s; } hot(20000)",
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
        .arg(format!("--jit-events={}", invalid_events_target.display()))
        .arg(format!("--jit-artifacts={}", artifacts_path.display()))
        .arg("--print")
        .arg(
            "function hot(n) { let s = 0; for (let i = 0; i < n; i = i + 1) \
             { s = s + i; } return s; } hot(20000)",
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
        .arg("--jitless")
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
    assert!(profile["xOtterDroppedSamples"].is_u64());
    assert!(profile["xOtterTruncatedFrames"].is_u64());
    assert!(folded.is_file(), "folded stack artifact exists");
}

#[test]
fn a_print_result_its_reader_left_is_dropped() {
    // `node -p '"x".repeat(1 << 20)' | head -c 1` exits 0 and says nothing:
    // Node prints the result with `console.log`, which drops what the reader
    // no longer takes.
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = output_after_reader_leaves(
        tmp.path(),
        &["-p", "'x'.repeat(1 << 20)"],
        ClosedStream::Stdout,
    );
    assert_quiet_exit(&output, 0);
}

#[test]
fn console_lines_their_reader_left_are_dropped() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let stdout = output_after_reader_leaves(
        tmp.path(),
        &["-e", "for (let i = 0; i < 100000; i++) console.log(i)"],
        ClosedStream::Stdout,
    );
    assert_quiet_exit(&stdout, 0);

    let stderr = output_after_reader_leaves(
        tmp.path(),
        &["-e", "for (let i = 0; i < 100000; i++) console.error(i)"],
        ClosedStream::Stderr,
    );
    assert_quiet_exit(&stderr, 0);
}

#[test]
fn an_uncaught_error_its_reader_left_still_exits_1() {
    // Node reports the error onto the closed stderr, which drops it, and
    // leaves with the uncaught exception's 1.
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = output_after_reader_leaves(
        tmp.path(),
        &[
            "-e",
            "for (let i = 0; i < 100000; i++) console.error(i); throw new Error('boom')",
        ],
        ClosedStream::Stderr,
    );
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn writes_their_reader_left_raise_epipe() {
    // Node's standard streams report a refused write as `Error: write EPIPE`:
    // unhandled, it ends the run with 1; a listener receives it.
    let tmp = tempfile::tempdir().expect("tempdir");
    let unhandled = output_after_reader_leaves(
        tmp.path(),
        &[
            "-e",
            "for (let i = 0; i < 100000; i++) process.stdout.write(i + '\\n')",
        ],
        ClosedStream::Stdout,
    );
    let stderr = String::from_utf8_lossy(&unhandled.stderr);
    assert_eq!(unhandled.status.code(), Some(1), "stderr:\n{stderr}");
    assert!(stderr.contains("Error: write EPIPE"), "stderr:\n{stderr}");

    let handled = output_after_reader_leaves(
        tmp.path(),
        &[
            "-e",
            "process.stdout.on('error', (e) => { process.exitCode = \
             e.code === 'EPIPE' && e.syscall === 'write' && e.errno === -32 ? 3 : 4 }); \
             for (let i = 0; i < 100000; i++) process.stdout.write(i + '\\n')",
        ],
        ClosedStream::Stdout,
    );
    assert_quiet_exit(&handled, 3);

    let stderr_write = output_after_reader_leaves(
        tmp.path(),
        &[
            "-e",
            "for (let i = 0; i < 100000; i++) process.stderr.write(i + '\\n')",
        ],
        ClosedStream::Stderr,
    );
    assert_eq!(stderr_write.status.code(), Some(1));
}

#[test]
fn a_later_console_write_its_reader_left_ends_the_run() {
    // The console swallows the first refused line; the stream's next failure
    // reaches no listener, so a program printing from timers ends with 1
    // rather than printing into the closed pipe for ever.
    let tmp = tempfile::tempdir().expect("tempdir");
    let timers = output_after_reader_leaves(
        tmp.path(),
        &[
            "-e",
            "let n = 0; (function next() { console.log('x'); if (++n < 200000) setImmediate(next) })()",
        ],
        ClosedStream::Stdout,
    );
    let stderr = String::from_utf8_lossy(&timers.stderr);
    assert_eq!(timers.status.code(), Some(1), "stderr:\n{stderr}");
    assert!(stderr.contains("Error: write EPIPE"), "stderr:\n{stderr}");

    // A warning prints through the same console.
    let warning = output_after_reader_leaves(
        tmp.path(),
        &[
            "-e",
            "for (let i = 0; i < 100000; i++) console.error(i); process.emitWarning('w')",
        ],
        ClosedStream::Stderr,
    );
    assert_eq!(warning.status.code(), Some(1));
}
