//! Regression repro for the test262 runner-only crash on
//! `built-ins/TypedArrayConstructors/internals/Set/bigint-tonumber.js`:
//! harness and body execute as two scripts on one runtime with a
//! deadline and heap cap installed (the runner's exact shape), and the
//! BigInt store throw path surfaced `VM_BYTECODE_INVARIANT: invalid
//! operand` under the template tier.

use std::time::Duration;

use otter_runtime::{JitSelection, Runtime, SourceInput};

fn harness_source() -> String {
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/test262/harness");
    let mut source = String::new();
    for file in ["assert.js", "sta.js", "testTypedArray.js"] {
        source.push_str(
            &std::fs::read_to_string(root.join(file)).expect("vendored harness file reads"),
        );
        source.push('\n');
    }
    source
}

const BODY: &str = r#"
testWithTypedArrayConstructors(function(TA, makeCtorArg) {
  var typedArray = new TA(makeCtorArg(1));
  assert.throws(TypeError, function() {
    typedArray[0] = 1n;
  });
}, null, null, ["immutable"]);
"#;

fn run_variant(jit: JitSelection, timeout: Duration) -> Result<(), String> {
    let mut runtime = Runtime::builder()
        .timeout(timeout)
        .max_heap_bytes(512 * 1024 * 1024)
        .jit_selection(jit)
        .process_global(false)
        .worker_global(false)
        .build()
        .map_err(|error| format!("build: {error}"))?;
    runtime
        .run_script(
            SourceInput::from_javascript(harness_source()),
            "test262-harness.js",
        )
        .map_err(|error| format!("harness: {error}"))?;
    runtime
        .run_script(
            SourceInput::from_javascript(BODY.to_string()),
            "bigint-tonumber.js",
        )
        .map_err(|error| format!("body: {error}"))?;
    Ok(())
}

#[test]
fn bigint_typed_array_throw_survives_template_tier_with_deadline() {
    run_variant(JitSelection::Template, Duration::from_secs(5)).expect("template tier");
}

#[test]
fn bigint_typed_array_throw_survives_tiered_with_deadline() {
    run_variant(JitSelection::ProductionTiered, Duration::from_secs(5)).expect("tiered");
}

#[test]
fn bigint_typed_array_throw_survives_interpreter_with_deadline() {
    run_variant(JitSelection::InterpreterOnly, Duration::from_secs(5)).expect("interpreter");
}
