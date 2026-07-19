//! CLI coverage for the active `node:fs` hosted module.
//!
//! # Contents
//! - Deny-by-default read behavior.
//! - Positive read/write behavior through explicit CLI capabilities.
//!
//! # Invariants
//! - The CLI installs active hosted modules on the same runtime path as normal
//!   file execution.
//! - Filesystem access remains deny-by-default and capability-gated.

use std::process::Command;

use serde_json::Value;

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
fn node_fs_read_is_denied_by_default() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data = tmp.path().join("data.txt");
    std::fs::write(&data, "secret").expect("write data");
    std::fs::write(
        tmp.path().join("main.ts"),
        format!(
            r#"
import {{ readFileSync }} from "node:fs";
readFileSync({path:?}, "utf8");
"#,
            path = data.to_string_lossy()
        ),
    )
    .expect("write main");

    let output = otter_command(tmp.path())
        .arg("--json")
        .arg("run")
        .arg("main.ts")
        .output()
        .expect("run denied fs");

    assert!(
        !output.status.success(),
        "expected failure\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value = serde_json::from_slice(&output.stderr).expect("json diagnostic");
    assert_eq!(envelope["error"]["kind"], "runtime");
    assert_eq!(envelope["error"]["diagnostic"]["code"], "UNCAUGHT");
}

#[test]
fn node_fs_read_write_respects_cli_allow_flags() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data = tmp.path().join("data.txt");
    std::fs::write(
        tmp.path().join("main.ts"),
        format!(
            r#"
import {{ existsSync, readFileSync, writeFileSync }} from "node:fs";
writeFileSync({path:?}, "hello", "utf8");
function fail() {{ process.exit(61); }}
if (!existsSync({path:?})) fail();
if (readFileSync({path:?}, "utf8") !== "hello") fail();
"#,
            path = data.to_string_lossy()
        ),
    )
    .expect("write main");

    let allow_root = tmp.path().to_string_lossy().to_string();
    let output = otter_command(tmp.path())
        .arg(format!("--allow-read={allow_root}"))
        .arg(format!("--allow-write={allow_root}"))
        .arg("run")
        .arg("main.ts")
        .output()
        .expect("run allowed fs");
    assert_success(output);
    assert_eq!(std::fs::read_to_string(data).expect("read data"), "hello");
}
