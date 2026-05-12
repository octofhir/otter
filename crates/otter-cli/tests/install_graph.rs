//! CLI integration coverage for install-to-runtime package graph flow.
//!
//! # Contents
//! - Local `otter install` for workspace and `file:` dependencies.
//! - Second install no-op / byte-stable lockfile check.
//! - Runtime consumption of the installed graph through `otter run`.
//!
//! # Invariants
//! - Package-manager commands are trusted CLI operations, independent of
//!   runtime capability flags.
//! - Runtime loading reads the package graph produced by install; it does not
//!   mutate package-manager state.

use std::process::Command;

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
fn install_file_and_workspace_deps_then_run_entry_and_bin() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::create_dir_all(root.join("packages/workspace-lib")).expect("mkdir workspace");
    std::fs::create_dir_all(root.join("tools/file-tool")).expect("mkdir file dep");
    std::fs::write(
        root.join("package.json"),
        r#"{
          "name": "app",
          "version": "0.1.0",
          "type": "module",
          "workspaces": ["packages/*"],
          "dependencies": {
            "workspace-lib": "workspace:*",
            "file-tool": "file:tools/file-tool"
          }
        }"#,
    )
    .expect("write root package");
    std::fs::write(
        root.join("entry.ts"),
        r#"
import { workspaceValue } from "workspace-lib";
import { fileValue } from "file-tool";
function fail() { process.exit(51); }
if (workspaceValue + fileValue !== 101) fail();
"#,
    )
    .expect("write entry");
    std::fs::write(
        root.join("packages/workspace-lib/package.json"),
        r#"{"name":"workspace-lib","version":"1.0.0","type":"module","main":"index.ts"}"#,
    )
    .expect("write workspace package");
    std::fs::write(
        root.join("packages/workspace-lib/index.ts"),
        "export const workspaceValue = 40;\n",
    )
    .expect("write workspace index");
    std::fs::write(
        root.join("tools/file-tool/package.json"),
        r#"{"name":"file-tool","version":"2.0.0","type":"module","main":"index.ts","bin":{"file-tool":"./cli.ts"}}"#,
    )
    .expect("write file package");
    std::fs::write(
        root.join("tools/file-tool/index.ts"),
        "export const fileValue = 61;\n",
    )
    .expect("write file index");
    std::fs::write(
        root.join("tools/file-tool/cli.ts"),
        r#"
function fail() { process.exit(52); }
if (process.argv[1].indexOf("cli.ts") === -1) fail();
if (process.argv[2] !== "from-cli") fail();
"#,
    )
    .expect("write file bin");

    let first_install = otter_command(root)
        .arg("install")
        .output()
        .expect("first install");
    assert_success(first_install);
    let first_lock = std::fs::read_to_string(root.join("otter.lock")).expect("read first lock");

    let second_install = otter_command(root)
        .arg("install")
        .output()
        .expect("second install");
    assert_success(second_install);
    let second_lock = std::fs::read_to_string(root.join("otter.lock")).expect("read second lock");
    assert_eq!(first_lock, second_lock);

    let run_entry = otter_command(root)
        .arg("run")
        .arg("entry.ts")
        .output()
        .expect("run entry");
    assert_success(run_entry);

    let run_bin = otter_command(root)
        .arg("run")
        .arg("--bin")
        .arg("file-tool")
        .arg("from-cli")
        .output()
        .expect("run file dep bin");
    assert_success(run_bin);
}

#[test]
fn installed_registry_graph_feeds_run_imports_and_bins() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::create_dir_all(root.join("node_modules/tool")).expect("mkdir tool");
    std::fs::create_dir_all(root.join("node_modules/.bin")).expect("mkdir bin");
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"app","version":"0.1.0","type":"module","dependencies":{"tool":"^1.0.0"}}"#,
    )
    .expect("write root package");
    std::fs::write(
        root.join("otter.lock"),
        r#"lockfile_version = 1

[packages."tool@npm:^1.0.0"]
name = "tool"
version = "1.0.0"
integrity = "sha512-test"

[packages."tool@npm:^1.0.0".resolved]
kind = "registry"
reference = "https://registry.npmjs.org/tool/-/tool-1.0.0.tgz"

[packages."tool@npm:^1.0.0".lifecycle]
trust = "untrusted"

[packages."app@workspace:."]
name = "app"
version = "0.1.0"

[packages."app@workspace:.".resolved]
kind = "workspace"
reference = "."

[packages."app@workspace:.".lifecycle]
trust = "trusted"

[packages."app@workspace:.".dependencies]
tool = "tool@npm:^1.0.0"
"#,
    )
    .expect("write lock");
    std::fs::write(
        root.join("entry.ts"),
        r#"
import { value } from "tool";
function fail() { process.exit(53); }
if (value !== 77) fail();
"#,
    )
    .expect("write entry");
    std::fs::write(
        root.join("node_modules/tool/package.json"),
        r#"{"name":"tool","version":"1.0.0","type":"module","main":"index.ts","bin":{"tool":"./cli.ts"}}"#,
    )
    .expect("write package");
    std::fs::write(
        root.join("node_modules/tool/index.ts"),
        "export const value = 77;\n",
    )
    .expect("write index");
    std::fs::write(
        root.join("node_modules/tool/cli.ts"),
        r#"
function fail() { process.exit(54); }
if (process.argv[1].indexOf("cli.ts") === -1) fail();
if (process.argv[2] !== "from-cli") fail();
"#,
    )
    .expect("write cli");
    std::fs::write(root.join("node_modules/.bin/tool"), "# shim placeholder\n")
        .expect("write bin link");

    let run_entry = otter_command(root)
        .arg("run")
        .arg("entry.ts")
        .output()
        .expect("run registry import");
    assert_success(run_entry);

    let run_bin = otter_command(root)
        .arg("run")
        .arg("--bin")
        .arg("tool")
        .arg("from-cli")
        .output()
        .expect("run registry bin");
    assert_success(run_bin);
}
