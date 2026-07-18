//! Runtime coverage for direct formal-parameter register bindings.
//!
//! # Contents
//! - Plain reads and writes through incoming argument registers.
//! - Sloppy duplicate formal parameters.
//! - Captured and mapped-arguments parameters that remain upvalue-backed.
//! - Expression reads snapshot a register before later operands can mutate it.
//! - Interpreter, template, and production-tier parity.
//!
//! # Invariants
//! - An uncaptured simple formal aliases its ABI argument register.
//! - Duplicate formals bind the last incoming occurrence.
//! - Captured and mapped formals retain stable upvalue cells.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function add(left, right) {
  return left + right;
}

function increment(value) {
  value = value + 1;
  return value;
}

function duplicate(value, value) {
  return value;
}

function capture(value) {
  return () => value;
}

function mapped(value) {
  arguments[0] = 9;
  return value;
}

function first(value) {
  return value;
}

function binarySnapshot() {
  let value = 1;
  return value + (value = 2);
}

function argumentSnapshot() {
  let value = 1;
  return first(value, value = 2);
}

function initializerSnapshot() {
  let value = 1;
  let result = value + (value = 2);
  return result;
}

for (let i = 0; i < 100; i++) {
  add(i, 1);
  increment(i);
  duplicate(i, i + 1);
  capture(i)();
  mapped(i);
  binarySnapshot();
  argumentSnapshot();
  initializerSnapshot();
}

JSON.stringify([
  add(20, 22),
  increment(1),
  duplicate(1, 2),
  capture(7)(),
  mapped(1),
  binarySnapshot(),
  argumentSnapshot(),
  initializerSnapshot()
]);
"#;

fn run(selection: JitSelection) -> String {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(1)
        .build()
        .expect("runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(SOURCE),
            "parameter-register-alias.js",
        )
        .expect("parameter alias script")
        .completion_string()
        .to_owned()
}

#[test]
fn parameter_storage_matches_across_execution_tiers() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        assert_eq!(run(selection), "[42,2,2,7,9,3,1,3]", "{selection:?}");
    }
}
