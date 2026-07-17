//! Moving-GC invariants for Node module value builders.
//!
//! # Contents
//! - `node:path` namespace links and parse records.
//! - `node:os` constant and native-method descriptors.
//! - `node:fs` Stats and Dirent records.
//! - `node:child_process` failed-spawn records.
//!
//! # Invariants
//! - Product modules build through the runtime's single `NativeScope` arena.
//! - Result prototypes, key order, descriptors, and cross-links survive a
//!   collection at every allocation boundary.
//! - Replacing the global `Object` binding cannot change the active realm
//!   prototype used for native result objects.

use otter_node::NodeApiBuilderExt;
use otter_runtime::{CapabilitySet, Runtime};

#[test]
fn node_module_builders_preserve_shapes_and_values() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("entry.txt"), "otter").expect("fixture");
    let directory = format!("{:?}", temp.path().to_string_lossy());
    let source = format!(
        r#"
        function load(specifier) {{
            try {{
                return require(specifier);
            }} catch (error) {{
                throw new Error("load " + specifier + ": " + error);
            }}
        }}

        const path = load("node:path");
        const originalObject = Object;
        const objectPrototype = Object.prototype;
        const getPrototypeOf = Object.getPrototypeOf;

        if (path !== path.posix || path.win32.posix !== path ||
            path.win32.win32 !== path.win32) {{
            throw new Error("path namespace links");
        }}

        globalThis.Object = function ReplacedObject() {{}};
        const parsed = path.parse("/alpha/beta.txt");
        globalThis.Object = originalObject;
        if (getPrototypeOf(parsed) !== objectPrototype) {{
            throw new Error("path parse prototype");
        }}
        if (Reflect.ownKeys(parsed).join(",") !== "root,dir,base,ext,name" ||
            parsed.root !== "/" || parsed.dir !== "/alpha" ||
            parsed.base !== "beta.txt" || parsed.ext !== ".txt" ||
            parsed.name !== "beta") {{
            throw new Error("path parse record");
        }}

        const os = load("node:os");
        const eol = Object.getOwnPropertyDescriptor(os, "EOL");
        if (eol.writable !== false || eol.enumerable !== true ||
            eol.configurable !== true) {{
            throw new Error("os EOL descriptor");
        }}
        const coercer = Object.getOwnPropertyDescriptor(os.arch, "toString");
        const valueOf = Object.getOwnPropertyDescriptor(os.arch, "valueOf");
        if (coercer.value !== valueOf.value ||
            coercer.value !== os.platform.toString ||
            coercer.writable !== true || coercer.enumerable !== false ||
            coercer.configurable !== true ||
            String(os.arch) !== os.arch()) {{
            throw new Error("os method coercer");
        }}

        const util = load("node:util");
        const utilTypes = load("node:util/types");
        if (utilTypes !== util.types || typeof util.format !== "function") {{
            throw new Error("util scoped cache identity");
        }}
        const assert = load("node:assert");
        const strictAssert = load("node:assert/strict");
        if (strictAssert !== assert.strict) {{
            throw new Error("assert strict identity");
        }}
        assert.deepStrictEqual({{ nested: [1, 2, 3] }}, {{ nested: [1, 2, 3] }});

        if (typeof URL === "undefined") globalThis.URL = function URL() {{}};
        const fs = load("node:fs");
        if (load("fs") !== fs) {{
            throw new Error("fs canonical cache identity");
        }}
        const stats = fs.statSync({directory});
        if (!stats.isDirectory() || typeof stats.size !== "number") {{
            throw new Error("fs stats record");
        }}
        const entries = fs.readdirSync({directory}, {{ withFileTypes: true }});
        const entry = entries.find((item) => item.name === "entry.txt");
        if (!entry || !entry.isFile() || entry.isDirectory()) {{
            throw new Error("fs dirent record");
        }}

        const childProcess = load("node:child_process");
        const failed = childProcess.spawnSync(
            "otter-definitely-missing-command",
            [],
            {{ encoding: "utf8" }}
        );
        if (failed.pid !== null || failed.status !== null ||
            failed.signal !== null || failed.stdout !== null ||
            failed.stderr !== null || !failed.error ||
            failed.error.code !== "ENOENT") {{
            throw new Error("child_process failed-spawn record");
        }}
        "#,
    );
    let entry = temp.path().join("main.js");
    std::fs::write(&entry, source).expect("write CommonJS fixture");

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .expect("runtime");
    runtime.run_file(&entry).expect("Node module fixture");
}
