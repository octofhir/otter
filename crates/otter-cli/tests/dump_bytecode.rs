//! CLI regression tests for bytecode dump output.
//!
//! # Contents
//! - JSON dump metadata checks for package-manager-backed module graphs.
//!
//! # Invariants
//! - The CLI entrypoint, not a direct runtime helper, owns these checks.
//! - Dump output stays valid JSON with bytecode plus per-source metadata.

use std::process::Command;

use serde_json::Value;

#[test]
fn dump_bytecode_json_includes_module_metadata_for_development_loop_fixture() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crate lives under workspace crates/");
    let entry = repo_root.join("tests/fixtures/pkg/development-loop/entry.ts");

    let output = Command::new(env!("CARGO_BIN_EXE_otter"))
        .arg("--dump-bytecode=json")
        .arg(&entry)
        .output()
        .expect("run otter dump");

    assert!(
        output.status.success(),
        "dump failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let dump: Value = serde_json::from_slice(&output.stdout).expect("valid dump json");
    assert!(
        dump["module"]
            .as_str()
            .is_some_and(|url| url.ends_with("/entry.ts"))
    );
    assert!(dump["functions"].is_array());
    assert!(
        dump["entry_url"]
            .as_str()
            .is_some_and(|url| url.ends_with("/entry.ts"))
    );

    let metadata = dump["metadata"].as_array().expect("metadata array");
    assert_eq!(metadata.len(), 4);
    let entry_metadata = metadata
        .iter()
        .find(|metadata| {
            metadata["source_url"]
                .as_str()
                .is_some_and(|url| url.ends_with("/entry.ts"))
        })
        .expect("entry metadata");
    let imports = entry_metadata["imports"].as_array().expect("imports array");
    let import_specifiers: Vec<_> = imports
        .iter()
        .map(|import| import["specifier"].as_str().expect("specifier"))
        .collect();
    assert_eq!(
        import_specifiers,
        ["./data.json", "fixture-tool", "workspace-lib"]
    );
}
