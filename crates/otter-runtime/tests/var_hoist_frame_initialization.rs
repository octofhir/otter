//! Runtime coverage for hoisted `var` frame initialization.
//!
//! # Contents
//! - Register-backed reads before a source declaration.
//! - Captured-cell reads before a source declaration.
//! - Interpreter, template, and production-tier parity.
//!
//! # Invariants
//! - Every fresh register window starts as `undefined`, so compiler lowering
//!   need not emit stores for register-backed hoisted `var` bindings.
//! - Captured bindings still initialize their upvalue cells explicitly.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function localBeforeDeclaration(early) {
  if (early) return value;
  var value = 42;
  return value;
}

function makeReader() {
  function read() {
    return captured;
  }
  return read;
  var captured = 42;
}

for (let i = 0; i < 100; i++) {
  if (localBeforeDeclaration(false) !== 42) {
    throw new Error("warmup mismatch");
  }
}

const read = makeReader();
JSON.stringify([
  localBeforeDeclaration(true) === undefined,
  read() === undefined
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
            "var-hoist-frame-initialization.js",
        )
        .expect("var hoist script")
        .completion_string()
        .to_owned()
}

#[test]
fn hoisted_vars_observe_undefined_in_every_tier() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        assert_eq!(run(selection), "[true,true]", "{selection:?}");
    }
}
