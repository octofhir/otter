//! Shared CommonJS resolver/cache invariants for Node hosted installers.
//!
//! # Invariants
//! - Embedded Node shims resolve every native and public dependency through
//!   the importing module's `require` function.
//! - Bare and `node:` aliases converge on one canonical module record.
//! - Transitive installer dependencies are visible as loaded records in the
//!   same `require.cache`, with their live export identities intact.

use otter_node::NodeApiBuilderExt;
use otter_runtime::{CapabilitySet, Runtime};

#[test]
fn node_installers_share_canonical_dependency_records() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = dir.path().join("entry.cjs");
    std::fs::write(
        &entry,
        r#"
        const fs = require("node:fs");
        const crypto = require("crypto");
        const zlib = require("node:zlib");
        const childProcess = require("child_process");

        if (require("fs") !== fs ||
            require("node:crypto") !== crypto ||
            require("zlib") !== zlib ||
            require("node:child_process") !== childProcess) {
            throw new Error("Node aliases did not preserve export identity");
        }

        const expected = [
            "node:fs",
            "node:crypto",
            "node:zlib",
            "node:child_process",
            "__fsnative",
            "__cryptonative",
            "__zlibnative",
            "__cpnative",
            "node:buffer",
            "node:events",
            "node:stream"
        ];
        for (const key of expected) {
            const record = require.cache[key];
            if (!record || record.id !== key || record.filename !== key ||
                record.loaded !== true || record.exports === undefined) {
                throw new Error("missing shared dependency record: " + key);
            }
        }

        if (require.cache["node:fs"].exports !== fs ||
            require.cache["node:crypto"].exports !== crypto ||
            require.cache["node:zlib"].exports !== zlib ||
            require.cache["node:child_process"].exports !== childProcess) {
            throw new Error("canonical records do not hold live public exports");
        }
        for (const alias of ["fs", "crypto", "zlib", "child_process"]) {
            if (require.cache[alias] !== undefined) {
                throw new Error("alias produced a duplicate cache record: " + alias);
            }
        }
        "#,
    )
    .expect("write CommonJS dependency fixture");

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .expect("runtime with Node APIs");
    runtime
        .run_file(&entry)
        .expect("Node shared dependency cache fixture");
}
