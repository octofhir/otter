//! Importing Node builtins from an ES module.
//!
//! # Contents
//! - A builtin backed by a CommonJS value is importable, default and named.
//! - The default export is the value `require` returns, so a callable builtin
//!   stays callable.
//!
//! # Invariants
//! - Every hosted builtin the CommonJS loader serves is importable: the ESM
//!   namespace is synthesized from the same value, not from a second
//!   implementation.

use std::path::Path;

use otter_node::NodeApiBuilderExt;
use otter_runtime::{CapabilitySet, Runtime};

fn runtime() -> Runtime {
    Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_nodejs_modules()
        .with_node_apis()
        .build()
        .expect("runtime")
}

fn write_module(dir: &Path, source: &str) -> std::path::PathBuf {
    let path = dir.join("entry.mjs");
    std::fs::write(&path, source).expect("write module");
    path
}

#[test]
fn commonjs_backed_builtins_import_as_default_and_named() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = write_module(
        dir.path(),
        r#"
        import assert, { strictEqual } from "node:assert";
        import util, { format } from "node:util";
        import events, { EventEmitter } from "node:events";
        import path from "node:path";

        strictEqual(typeof assert, "function");
        assert(true);
        strictEqual(format("%s!", "x"), "x!");
        strictEqual(typeof util.inspect, "function");
        strictEqual(events, EventEmitter);
        strictEqual(typeof path.join, "function");
        strictEqual(path.join("a", "b"), "a/b");
        "#,
    );

    runtime()
        .run_file(&entry)
        .expect("Node builtins are importable from an ES module");
}

#[test]
fn the_default_export_carries_the_modules_own_properties() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = write_module(
        dir.path(),
        r#"
        import assert from "node:assert";
        import buffer from "node:buffer";

        if (typeof assert.strictEqual !== "function") {
            throw new Error("assert.strictEqual is " + typeof assert.strictEqual);
        }
        if (typeof buffer.Buffer.from !== "function") {
            throw new Error("Buffer.from is " + typeof buffer.Buffer?.from);
        }
        "#,
    );

    runtime()
        .run_file(&entry)
        .expect("the default export is the module's own exports object");
}
