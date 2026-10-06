//! `require` resolves by Node's CommonJS algorithm.
//!
//! # Contents
//! - A dotted file name gains an extension instead of losing its last
//!   component.
//! - `.` and `..` name directory entries.
//! - A package's `exports` map selects the `require` condition and maps
//!   subpaths; `#imports` resolve inside the package.
//! - `require.resolve` answers the same canonical path.
//!
//! # Invariants
//! - `require` and `import` share one resolver configuration; only the
//!   conditions differ.

use std::path::{Path, PathBuf};

use otter_runtime::{CapabilitySet, Runtime, SourceInput};

fn write_fixture(dir: &Path, name: &str, source: &str) -> PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create fixture directory");
    }
    std::fs::write(&path, source).expect("write fixture");
    path
}

fn run_entry(entry: &Path) -> String {
    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_nodejs_modules()
        .build()
        .expect("CommonJS runtime");
    runtime.run_file(entry).expect("CommonJS entry");
    runtime
        .run_script(
            SourceInput::from_javascript("globalThis.__resolution;"),
            "resolution-probe.js",
        )
        .expect("read resolution result")
        .completion_string()
        .to_owned()
}

#[test]
fn require_follows_node_commonjs_resolution() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write_fixture(root, "lib/app.config.js", "module.exports = 'config';");
    write_fixture(root, "lib/app.js", "module.exports = 'app';");
    write_fixture(root, "lib/index.js", "module.exports = 'lib-index';");
    write_fixture(root, "lib/nested/index.js", "module.exports = require('..');");
    write_fixture(
        root,
        "node_modules/dual/package.json",
        r##"{
            "name": "dual",
            "exports": {
                ".": { "import": "./esm.mjs", "require": "./cjs.js" },
                "./feature": { "require": "./feature-cjs.js" }
            },
            "imports": { "#internal": "./internal.js" }
        }"##,
    );
    write_fixture(root, "node_modules/dual/esm.mjs", "export default 'esm';");
    write_fixture(
        root,
        "node_modules/dual/cjs.js",
        "module.exports = 'cjs+' + require('#internal');",
    );
    write_fixture(root, "node_modules/dual/internal.js", "module.exports = 'internal';");
    write_fixture(root, "node_modules/dual/feature-cjs.js", "module.exports = 'feature';");
    let entry = write_fixture(
        root,
        "entry.cjs",
        r#"
        globalThis.__resolution = JSON.stringify([
            require("./lib/app.config"),
            require("./lib/app"),
            require("./lib/nested"),
            require("./lib"),
            require("dual"),
            require("dual/feature"),
            require.resolve("dual").slice(__dirname.length + 1),
            require.resolve("./lib/app.config").slice(__dirname.length + 1),
        ]);
        "#,
    );
    assert_eq!(
        run_entry(&entry),
        r#"["config","app","lib-index","lib-index","cjs+internal","feature","node_modules/dual/cjs.js","lib/app.config.js"]"#
    );
}

#[test]
fn require_dot_names_the_directory_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write_fixture(root, "pkg/index.js", "module.exports = 'pkg-index';");
    let entry = write_fixture(
        root,
        "pkg/main.cjs",
        "globalThis.__resolution = require('.');",
    );
    assert_eq!(run_entry(&entry), "pkg-index");
}
