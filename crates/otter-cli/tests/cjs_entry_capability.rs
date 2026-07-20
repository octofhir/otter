//! `read` gates filesystem access, not code loading.
//!
//! The CommonJS entry arrives with its source already in hand, so running it
//! opens no file and needs no `read` capability — the same way the ESM entry
//! path behaves. Every dependency that is actually read off disk stays gated.

use std::process::Command;

fn otter(root: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_otter"));
    command.current_dir(root);
    command
}

#[test]
fn a_commonjs_entry_runs_without_a_read_capability() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("main.js"),
        "const assert = require('node:assert');\nassert.strictEqual(1 + 1, 2);\n",
    )
    .expect("write entry");

    let output = otter(tmp.path())
        .arg("run")
        .arg("main.js")
        .output()
        .expect("run entry");

    assert!(
        output.status.success(),
        "entry must not need `read`\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn requiring_a_file_off_disk_still_needs_the_read_capability() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("dep.js"), "module.exports = 7;\n").expect("write dep");
    std::fs::write(
        tmp.path().join("main.js"),
        "const dep = require('./dep.js');\nif (dep !== 7) { throw new Error('bad dep'); }\n",
    )
    .expect("write entry");

    let denied = otter(tmp.path())
        .arg("run")
        .arg("main.js")
        .output()
        .expect("run entry");
    assert!(
        !denied.status.success(),
        "a dependency read off disk must stay gated by `read`"
    );
    let stderr = String::from_utf8_lossy(&denied.stderr);
    assert!(
        stderr.contains("permission denied"),
        "the diagnostic names the denied read: {stderr}"
    );

    let allowed = otter(tmp.path())
        .arg("run")
        .arg(format!("--allow-read={}", tmp.path().to_string_lossy()))
        .arg("main.js")
        .output()
        .expect("run entry with read");
    assert!(
        allowed.status.success(),
        "granting `read` admits the dependency\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&allowed.stdout),
        String::from_utf8_lossy(&allowed.stderr)
    );
}
