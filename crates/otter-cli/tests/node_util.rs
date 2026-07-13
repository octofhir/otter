//! CLI coverage for `node:util`, `util/types`, and `node:tty`.
//!
//! # Contents
//! - `util/types` reference identity with `util.types`.
//! - ANSI named, nested, and hexadecimal style composition.
//! - Deterministic non-TTY stream behavior.
//!
//! # Invariants
//! - Hosted aliases share one cached util export per runtime.
//! - TTY probing opens no host descriptor and defaults to no color.

use std::process::Command;

#[test]
fn node_util_aliases_and_styles_match_node_shape() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("main.js"),
        r#"
const util = require('node:util');
if (require('util/types') !== util.types) throw new Error('util/types identity');
if (util.styleText('#ffcc00', 'x', { validateStream: false }) !==
    '\x1b[38;2;255;204;0mx\x1b[39m') throw new Error('hex style');
if (util.styleText(['bold', 'red'], 'x', { validateStream: false }) !==
    '\x1b[1m\x1b[31mx\x1b[39m\x1b[22m') throw new Error('nested style');
const { WriteStream, isatty } = require('node:tty');
const stream = new WriteStream(1);
if (stream.isTTY !== false || isatty(1) !== false) throw new Error('tty shape');
if (util.styleText('red', 'x', { stream }) !== 'x') throw new Error('non-tty color');
"#,
    )
    .expect("write util fixture");

    let mut command = Command::new(env!("CARGO_BIN_EXE_otter"));
    let output = command
        .current_dir(tmp.path())
        .arg("run")
        .arg("main.js")
        .output()
        .expect("run util fixture");
    assert!(
        output.status.success(),
        "otter failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
