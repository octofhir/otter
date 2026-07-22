//! End-to-end semantics for production loop-invariant property versioning.
//!
//! # Contents
//! - Production/interpreter A/B execution of own-data, accessor, branch, and
//!   repeated-loop property reads.
//! - Structured-event proof that the production run entered native code.
//!
//! # Invariants
//! - Every source and input is identical between the production and `--jitless`
//!   processes.
//! - Accessor misses may mutate an otherwise cacheable property without making
//!   the versioned loop observe stale state.

use std::process::{Command, Output};

const SOURCE: &str = r#"
var stable = { left: 0.5, right: 8 };
function stableLoop(count) {
  var sum = 0;
  for (var index = 0; index < count; index = index + 1) {
    sum = sum + stable.left * stable.right;
  }
  return sum;
}

var observed = { value: 1 };
Object.defineProperty(observed, "tick", {
  get: function () {
    observed.value = observed.value + 1;
    return 0;
  }
});
function accessorLoop(count) {
  var sum = 0;
  for (var index = 0; index < count; index = index + 1) {
    sum = sum + observed.value + observed.tick;
  }
  return sum;
}

var branched = { left: 3, right: 5 };
function branchLoop(count) {
  var sum = 0;
  for (var index = 0; index < count; index = index + 1) {
    if ((index & 1) === 0) sum = sum + branched.left;
    sum = sum + branched.right;
  }
  return sum;
}

var repeated = { value: 2 };
function repeatedLoop(count) {
  var sum = 0;
  for (var index = 0; index < count; index = index + 1) {
    sum = sum + repeated.value;
  }
  return sum;
}

var first = repeatedLoop(200);
repeated.value = 7;
var second = repeatedLoop(200);
[stableLoop(200), accessorLoop(200), branchLoop(200), first, second].join(",")
"#;

fn run(root: &std::path::Path, jitless: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_otter"));
    command
        .current_dir(root)
        .env("OTTER_JIT_OSR_THRESHOLD", "1")
        .arg("--print")
        .arg(SOURCE);
    if jitless {
        command.arg("--jitless");
    } else {
        command.arg(format!(
            "--jit-events={}",
            root.join("jit-events.json").display()
        ));
    }
    command.output().expect("run loop-invariant fixture")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "otter failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_arch = "aarch64")]
#[test]
fn property_loop_versioning_matches_the_interpreter_oracle() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let production = run(tmp.path(), false);
    let interpreter = run(tmp.path(), true);
    assert_success(&production);
    assert_success(&interpreter);
    assert_eq!(production.stdout, interpreter.stdout);
    assert_eq!(
        String::from_utf8_lossy(&production.stdout).trim(),
        "800,20100,1300,400,1400"
    );

    let report: serde_json::Value = serde_json::from_slice(
        &std::fs::read(tmp.path().join("jit-events.json")).expect("read JIT events"),
    )
    .expect("valid JIT events");
    assert!(
        report["events"].as_array().is_some_and(|events| events
            .iter()
            .any(|event| event["type"] == "compileFinished"
                && event["tier"] == "optimizing"
                && event["outcome"]["kind"] == "compiled")),
        "production run must execute an optimizing code object"
    );
}
