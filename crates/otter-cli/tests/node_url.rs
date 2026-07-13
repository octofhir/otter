//! CLI coverage for `node:url` through the active Node compatibility stack.
//!
//! # Contents
//! - CommonJS WHATWG constructor and file-URL helper interoperability.
//! - Node formatting options, including non-boolean truthy/falsy values.
//! - Named ESM file-URL helpers used by Vite-style loaders.
//!
//! # Invariants
//! - URL conversion performs no filesystem access and needs no capability.
//! - CommonJS and ESM aliases expose equivalent file URL strings.

use std::process::Command;

fn otter_command(root: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_otter"));
    command.current_dir(root);
    command
}

fn assert_success(output: std::process::Output) {
    assert!(
        output.status.success(),
        "otter failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_fixture(name: &str, source: &str) {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join(name), source).expect("write node:url fixture");
    let output = otter_command(tmp.path())
        .arg("run")
        .arg(name)
        .output()
        .expect("run node:url fixture");
    assert_success(output);
}

#[test]
fn node_url_commonjs_file_helpers_round_trip() {
    run_fixture(
        "commonjs.js",
        r#"
const url = require('node:url');
const { URL, pathToFileURL, fileURLToPath, urlToHttpOptions } = url;
const href = pathToFileURL('/tmp/otter url.js').href;
if (href !== 'file:///tmp/otter%20url.js') throw new Error(href);
if (fileURLToPath(new URL(href)) !== '/tmp/otter url.js') throw new Error('round trip failed');
const options = urlToHttpOptions(new URL('http://user:pass@example.com:8080/a?q=1'));
if (options.auth !== 'user:pass') throw new Error('auth failed');
if (options.path !== '/a?q=1') throw new Error('options failed');
const formatted = new URL('http://user:pass@example.com/a?q=1#hash');
if (url.format(formatted, { auth: 0 }) !== 'http://example.com/a?q=1#hash') {
  throw new Error('falsy auth option failed');
}
if (url.format(formatted, { search: '', fragment: 0 }) !== 'http://user:pass@example.com/a') {
  throw new Error('falsy search/fragment options failed');
}
if (url.format({
  protocol: 'http', host: 'a.com', pathname: 'a/b', search: 'q=1', hash: 'h'
}) !== 'http://a.com/a/b?q=1#h') {
  throw new Error('legacy object formatting failed');
}
if (url.format({ protocol: 'coap:', auth: 'u:p', hostname: '::1', port: '61616' }) !==
    'coap:u:p@[::1]:61616') {
  throw new Error('legacy IPv6 formatting failed');
}
"#,
    );
}

#[test]
fn node_url_esm_file_helpers_round_trip() {
    run_fixture(
        "module.mjs",
        r#"
import { pathToFileURL, fileURLToPath } from 'node:url';
const url = pathToFileURL('/tmp/otter esm.js');
if (!(url instanceof URL)) throw new Error('not a branded URL');
const href = url.href;
if (href !== 'file:///tmp/otter%20esm.js') throw new Error(href);
if (fileURLToPath(url) !== '/tmp/otter esm.js') throw new Error('round trip failed');
if (fileURLToPath('FILE://LOCALHOST/tmp/a?ignored#fragment') !== '/tmp/a') {
  throw new Error('WHATWG normalization failed');
}
let rejectedPlainObject = false;
try { fileURLToPath({ href }); } catch (error) {
  rejectedPlainObject = error && error.code === 'ERR_INVALID_ARG_TYPE';
}
if (!rejectedPlainObject) throw new Error('plain object accepted as URL');
"#,
    );
}
