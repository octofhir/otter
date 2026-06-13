use otter_node::{NodeApiBuilderExt, hosted_modules};
use otter_runtime::{CapabilitySet, Permission, Runtime};

#[test]
fn hosted_node_module_specs_are_static_and_ordered() {
    let specs = hosted_modules();
    // `fs` leads the list; the exact length grows as modules are ported.
    assert_eq!(specs[0].specifier(), "node:fs");
    assert_eq!(specs[1].specifier(), "fs");

    // Every `node:`-prefixed builtin has a matching bare specifier.
    let names: Vec<&str> = specs.iter().map(|m| m.specifier()).collect();
    for name in &names {
        if let Some(bare) = name.strip_prefix("node:") {
            assert!(names.contains(&bare), "missing bare specifier for {name}");
        }
    }
    // Core modules ported so far are registered.
    for expected in ["os", "node:os", "node:test", "assert", "path"] {
        assert!(
            names.contains(&expected),
            "missing hosted module {expected}"
        );
    }
}

#[test]
fn node_fs_requires_read_permission() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    let data = dir.path().join("data.txt");
    std::fs::write(&data, "secret").unwrap();
    std::fs::write(
        &main,
        r#"
            import { readFileSync } from "node:fs";
            readFileSync("data.txt", "utf8");
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder().with_node_apis().build().unwrap();
    let err = runtime.run_module(&main).unwrap_err();
    assert!(err.to_string().contains("permission denied"));
}

#[test]
fn node_fs_read_write_round_trips_with_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    let data = dir.path().join("data.txt");
    std::fs::write(
        &main,
        format!(
            r#"
            import {{ existsSync, readFileSync, writeFileSync }} from "node:fs";
            writeFileSync({path:?}, "hello", "utf8");
            if (!existsSync({path:?})) {{
                throw new Error("exists failed");
            }}
            if (readFileSync({path:?}, "utf8") !== "hello") {{
                throw new Error("read failed");
            }}
        "#,
            path = data.to_string_lossy()
        ),
    )
    .unwrap();
    let caps = CapabilitySet {
        read: Permission::allow([data.clone()]),
        write: Permission::allow([data]),
        ..CapabilitySet::sandbox()
    };

    let mut runtime = Runtime::builder()
        .capabilities(caps)
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}
