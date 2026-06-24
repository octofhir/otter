//! CLI coverage for `node:path` through the active Node compatibility stack.
//!
//! # Contents
//! - JIT-visible `Function.prototype.apply` calls into Node path natives.
//! - `assert.throws` object matchers over errors thrown through compiled frames.
//!
//! # Invariants
//! - JIT runtime helpers resolve constant-pool operands through the executing
//!   function's owning chunk, not the ambient caller context.

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
fn node_path_apply_errors_survive_jit_and_assert_matchers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("main.js"),
        r#"
const assert = require('node:assert');
const path = require('node:path');

function make(fn) {
  const args = [7];
  return () => fn.apply(null, args);
}

const callJoin = make(path.posix.join);
for (let i = 0; i < 80; i++) {
  assert.throws(callJoin, {
    code: 'ERR_INVALID_ARG_TYPE',
    name: 'TypeError',
  });
}
"#,
    )
    .expect("write main");

    let output = otter_command(tmp.path())
        .arg("run")
        .arg("main.js")
        .output()
        .expect("run node path");
    assert_success(output);
}
