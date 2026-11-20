//! Importing Node builtins from an ES module.
//!
//! # Contents
//! - A builtin backed by a CommonJS value is importable, default and named.
//! - The default export is the value `require` returns, so a callable builtin
//!   stays callable.
//!
//! - A CommonJS file is importable from an ES module.
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

#[test]
fn a_commonjs_file_is_importable_from_an_es_module() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("plain.js"),
        "exports.alpha = 1;\nexports.beta = function () { return 2; };\n",
    )
    .expect("write dependency");
    std::fs::write(
        dir.path().join("replaced.js"),
        "module.exports = { solo: 42 };\n",
    )
    .expect("write dependency");
    let entry = write_module(
        dir.path(),
        r#"
        import plain, { alpha, beta } from "./plain.js";
        import replaced from "./replaced.js";

        if (alpha !== 1) throw new Error("alpha was " + alpha);
        if (beta() !== 2) throw new Error("beta() was " + beta());
        if (plain.alpha !== 1) throw new Error("default export lost its keys");
        if (replaced.solo !== 42) throw new Error("module.exports replacement lost");
        "#,
    );

    runtime()
        .run_file(&entry)
        .expect("a CommonJS dependency imports as default plus its assigned names");
}

#[test]
fn a_class_exposes_its_static_members_as_named_exports() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = write_module(
        dir.path(),
        r#"
        import module_, { createRequire, isBuiltin } from "node:module";

        if (typeof createRequire !== "function") {
            throw new Error("createRequire is " + typeof createRequire);
        }
        if (typeof isBuiltin !== "function") throw new Error("isBuiltin is missing");
        if (module_.createRequire !== createRequire) {
            throw new Error("the named export is not the default's own member");
        }
        "#,
    );

    runtime()
        .run_file(&entry)
        .expect("a class-valued module publishes its statics as named exports");
}
