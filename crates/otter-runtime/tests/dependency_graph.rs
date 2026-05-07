//! Build graph regression tests for active runtime product crates.

use std::path::Path;

#[test]
fn active_product_crates_do_not_depend_on_otter_vm_directly() {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("runtime crate should live under crates/");

    for crate_name in ["otter-modules", "otter-web"] {
        let manifest_path = workspace_root
            .join("crates")
            .join(crate_name)
            .join("Cargo.toml");
        let manifest = std::fs::read_to_string(&manifest_path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", manifest_path.display()));

        assert!(
            !manifest
                .lines()
                .any(|line| line.trim_start().starts_with("otter-vm")),
            "{crate_name} must depend on otter-runtime APIs, not directly on otter-vm"
        );
    }
}
