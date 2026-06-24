//! CLI coverage for `node:events` helpers over Web `EventTarget`.
//!
//! # Contents
//! - Static `getEventListeners` over the Web bootstrap listener table.
//! - AbortSignal-specific max listener defaults.
//!
//! # Invariants
//! - The CLI installs Node and Web APIs on the same runtime path used by
//!   node-compat tests.

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

#[test]
fn node_events_reads_web_event_target_listeners() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("main.js"),
        r#"
const assert = require('node:assert');
const { getEventListeners, getMaxListeners, setMaxListeners } = require('node:events');

const target = new EventTarget();
function first() {}
function second() {}
target.addEventListener('foo', first);
target.addEventListener('foo', second);
target.addEventListener('foo', first);
assert.deepStrictEqual(getEventListeners(target, 'foo'), [first, second]);
assert.deepStrictEqual(getEventListeners(target, 'missing'), []);
assert.throws(() => getEventListeners('bad'), { code: 'ERR_INVALID_ARG_TYPE' });

const controller = new AbortController();
assert.strictEqual(getMaxListeners(controller.signal), 0);
setMaxListeners(7, controller.signal);
assert.strictEqual(getMaxListeners(controller.signal), 7);
"#,
    )
    .expect("write main");

    let output = otter_command(tmp.path())
        .arg("run")
        .arg("main.js")
        .output()
        .expect("run node events");
    assert_success(output);
}
