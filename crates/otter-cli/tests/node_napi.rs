//! End-to-end coverage for Node-API native addon loading.
//!
//! # Contents
//! - A real C shared library loaded through bare-package `node_modules`
//!   resolution and `package.json#main`.
//! - Both symbol-based and constructor-based Node-API registration.
//! - Value, property, callback, JS re-entry, array, exception, external,
//!   buffer, external-memory accounting, Promise, and asynchronous-work calls.
//! - Independent deny-by-default checks for `read` and `ffi`.
//!
//! # Invariants
//! - The fixture is compiled against the stable C ABI, not Node headers.
//! - Native code never loads unless both filesystem read and FFI are granted.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ADDON_SOURCE: &str = include_str!("fixtures/napi_addon.c");
const LEGACY_ADDON_SOURCE: &str = include_str!("fixtures/napi_legacy_addon.c");

fn build_addon(root: &Path) -> PathBuf {
    build_addon_source(root, ADDON_SOURCE)
}

fn build_addon_source(root: &Path, source_text: &str) -> PathBuf {
    let source = root.join("addon.c");
    let output = root.join("addon.node");
    std::fs::write(&source, source_text).expect("write addon source");
    let mut cc = Command::new("cc");
    #[cfg(target_os = "macos")]
    cc.args(["-dynamiclib", "-undefined", "dynamic_lookup"]);
    #[cfg(not(target_os = "macos"))]
    cc.args(["-shared", "-fPIC"]);
    let status = cc
        .arg("-O2")
        .arg("-o")
        .arg(&output)
        .arg(&source)
        .status()
        .expect("run C compiler");
    assert!(status.success(), "C compiler failed with {status}");
    output
}

#[test]
fn node_api_addon_supports_constructor_registration() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let addon = build_addon_source(tmp.path(), LEGACY_ADDON_SOURCE);
    std::fs::write(
        tmp.path().join("main.js"),
        "const addon = require('./addon.node');\n\
         if (addon.registration !== 'constructor') throw new Error('registration');\n\
         console.log('napi-constructor-ok');",
    )
    .expect("write main");

    let root = std::fs::canonicalize(tmp.path())
        .expect("canonical root")
        .to_string_lossy()
        .into_owned();
    let addon = std::fs::canonicalize(addon)
        .expect("canonical addon")
        .to_string_lossy()
        .into_owned();
    let output = run(
        tmp.path(),
        &[
            format!("--allow-read={root}"),
            format!("--allow-ffi={addon}"),
            "run".into(),
            "main.js".into(),
        ],
    );
    assert!(output.status.success(), "{}", diagnostic(&output));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("napi-constructor-ok"),
        "{}",
        diagnostic(&output)
    );
}

fn run(root: &Path, args: &[String]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_otter"))
        .current_dir(root)
        .args(args)
        .output()
        .expect("run otter")
}

fn diagnostic(output: &Output) -> String {
    format!(
        "status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn node_api_addon_executes_stable_c_abi() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let package = tmp.path().join("node_modules/napi-fixture");
    std::fs::create_dir_all(&package).expect("create package directory");
    std::fs::write(
        package.join("package.json"),
        r#"{"name":"napi-fixture","main":"addon.node"}"#,
    )
    .expect("write package manifest");
    let addon = build_addon(&package);
    std::fs::write(
        tmp.path().join("main.js"),
        r#"
const addon = require('napi-fixture');
if (addon.version !== '1.0.0') throw new Error('version');
if (addon.add(20, 22) !== 42) throw new Error('add');
if (addon.makeArray().join(':') !== 'otter:napi') throw new Error('array');
if (addon.callJs((value) => value + 1) !== 42) throw new Error('callJs');
function Box(value) { this.value = value; }
const boxed = addon.constructJs(Box, 42);
if (!(boxed instanceof Box) || boxed.value !== 42) throw new Error('constructJs');
if (addon.missingArgIsUndefined() !== 1) throw new Error('missing argument');
if (addon.externalRoundTrip() !== 42) throw new Error('external');
if (addon.inspectBuffer(new Uint8Array([7, 8, 9])) !== 10) throw new Error('buffer');
if (addon.coerceObject(42) !== 6) throw new Error('coerce object');
if (addon.accountExternal() !== 4096) throw new Error('external memory');
if (addon.inspectCollections() !== 42) throw new Error('collection predicates');
if (addon.lifecycleHooks() !== 42) throw new Error('lifecycle hooks');
if (addon.inspectDescriptors() !== 42) throw new Error('descriptors');
let message = '';
try { addon.fail(); } catch (error) { message = error.message; }
if (message !== 'native boom') throw new Error('throw: ' + message);
addon.asyncAnswer().then(value => {
  if (value !== 42) throw new Error('async: ' + value);
  console.log('napi-async-ok');
});
"#,
    )
    .expect("write main");

    let root = std::fs::canonicalize(tmp.path())
        .expect("canonical root")
        .to_string_lossy()
        .into_owned();
    let addon = std::fs::canonicalize(addon)
        .expect("canonical addon")
        .to_string_lossy()
        .into_owned();
    let args = vec![
        format!("--allow-read={root}"),
        format!("--allow-ffi={addon}"),
        "run".to_string(),
        "main.js".to_string(),
    ];
    let output = run(tmp.path(), &args);
    assert!(output.status.success(), "{}", diagnostic(&output));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("napi-async-ok"),
        "{}",
        diagnostic(&output)
    );
}

#[test]
fn node_api_addon_requires_read_and_ffi_capabilities() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let addon = build_addon(tmp.path());
    std::fs::write(tmp.path().join("main.js"), "require('./addon.node');").expect("write main");

    let denied = run(tmp.path(), &["run".into(), "main.js".into()]);
    assert!(!denied.status.success(), "unexpected success without caps");

    let root = std::fs::canonicalize(tmp.path())
        .expect("canonical root")
        .to_string_lossy()
        .into_owned();
    let read_only = run(
        tmp.path(),
        &[
            format!("--allow-read={root}"),
            "run".into(),
            "main.js".into(),
        ],
    );
    assert!(
        !read_only.status.success(),
        "unexpected success without ffi: {}",
        diagnostic(&read_only)
    );
    assert!(
        String::from_utf8_lossy(&read_only.stderr).contains("ffi permission denied"),
        "{}",
        diagnostic(&read_only)
    );

    let addon = std::fs::canonicalize(addon)
        .expect("canonical addon")
        .to_string_lossy()
        .into_owned();
    let ffi_only = run(
        tmp.path(),
        &[
            format!("--allow-ffi={addon}"),
            "run".into(),
            "main.js".into(),
        ],
    );
    assert!(
        !ffi_only.status.success(),
        "unexpected success without read"
    );
}
