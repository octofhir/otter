//! CLI integration coverage for owned runtime execution configuration.
//!
//! # Contents
//! - Explicit execution-tier selection through the public CLI.
//! - Shared trace and CPU-profiler configuration on the synchronous runtime path.
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
