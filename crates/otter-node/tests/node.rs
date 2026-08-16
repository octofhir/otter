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

/// The filesystem members added for Node's own tests: a private temporary
/// directory, both link kinds, ownership-free timestamp changes, and the
/// descriptor operations that used to be silent no-ops.
#[test]
fn node_fs_links_timestamps_and_descriptor_operations() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        format!(
            r#"
            import {{
                closeSync, existsSync, fsyncSync, ftruncateSync, linkSync, mkdtempSync,
                openSync, readFileSync, readlinkSync, statSync, symlinkSync, utimesSync,
                writeFileSync,
            }} from "node:fs";
            import {{ join }} from "node:path";

            const root = {root:?};
            const scratch = mkdtempSync(join(root, "otter-"));
            if (!existsSync(scratch)) throw new Error("mkdtemp did not create a directory");
            if (!scratch.startsWith(join(root, "otter-"))) {{
                throw new Error("mkdtemp ignored its prefix: " + scratch);
            }}
            if (mkdtempSync(join(root, "otter-")) === scratch) {{
                throw new Error("mkdtemp handed out the same name twice");
            }}

            const file = join(scratch, "a.txt");
            writeFileSync(file, "hello");
            const hard = join(scratch, "hard.txt");
            linkSync(file, hard);
            if (readFileSync(hard, "utf8") !== "hello") throw new Error("hard link lost content");

            const soft = join(scratch, "soft.txt");
            symlinkSync(file, soft);
            if (readlinkSync(soft) !== file) throw new Error("symlink target: " + readlinkSync(soft));

            utimesSync(file, 1600000000, 1600000001);
            const stamped = statSync(file);
            if (Math.round(stamped.mtimeMs / 1000) !== 1600000001) {{
                throw new Error("mtime was " + stamped.mtimeMs);
            }}

            const fd = openSync(file, "r+");
            ftruncateSync(fd, 1);
            fsyncSync(fd);
            closeSync(fd);
            if (readFileSync(file, "utf8") !== "h") {{
                throw new Error("ftruncate left " + readFileSync(file, "utf8"));
            }}
        "#,
            root = dir.path().to_string_lossy()
        ),
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}

/// `fsPromises.open` hands out a handle whose reads, writes, metadata, and
/// web stream all address the same descriptor, and whose methods refuse to run
/// once it is closed.
#[test]
fn node_fs_promises_file_handle_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    let data = dir.path().join("data.txt");
    std::fs::write(&data, "hello world").unwrap();
    std::fs::write(
        &main,
        format!(
            r#"
            import {{ open }} from "node:fs/promises";

            const handle = await open({path:?}, "r");
            if (typeof handle.fd !== "number") throw new Error("fd was " + typeof handle.fd);

            const {{ bytesRead, buffer }} = await handle.read(Buffer.alloc(5), 0, 5, 0);
            if (bytesRead !== 5) throw new Error("bytesRead was " + bytesRead);
            if (buffer.toString("utf8", 0, bytesRead) !== "hello") {{
                throw new Error("read " + buffer.toString("utf8", 0, bytesRead));
            }}

            const stat = await handle.stat();
            if (stat.size !== 11) throw new Error("size was " + stat.size);

            await handle.close();
            let afterClose;
            try {{
                await handle.read(Buffer.alloc(1), 0, 1, 0);
            }} catch (error) {{
                afterClose = error;
            }}
            if (afterClose?.code !== "EBADF") {{
                throw new Error("a closed handle answered " + afterClose?.code);
            }}
            await handle.close();
        "#,
            path = data.to_string_lossy()
        ),
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}
