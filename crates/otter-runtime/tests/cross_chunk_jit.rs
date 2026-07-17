//! Cross-chunk compiled-entry regression coverage.
//!
//! A long-lived runtime links every script into one shared code space, so a
//! hot closure defined by one script is routinely entered from a sibling
//! script's dispatch tick. The activation published to compiled code (and
//! every constant-pool read a reentrant transition performs) must resolve the
//! chunk owning the entered frame — decoding the callee's constant indices
//! against the caller's tables silently corrupts execution.
//!
//! The fixture is the exact test262 driver shape that exposed this: the
//! harness scripts define the hot `testWithTypedArrayConstructors` driver
//! (with a large constant pool), and the second script's callback tiers up
//! and materializes a builtin error constructor — a chunk-local constant
//! read that used to decode against the harness chunk.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const BODY: &str = r#"
testWithTypedArrayConstructors(function(TA, makeCtorArg) {
  assert.throws(TypeError, function() { throw new TypeError(); });
});
"#;

fn harness_source() -> Option<String> {
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/test262/harness");
    let mut out = String::new();
    for file in ["assert.js", "sta.js", "testTypedArray.js"] {
        out.push_str(&std::fs::read_to_string(root.join(file)).ok()?);
        out.push('\n');
    }
    Some(out)
}

fn run_pair(harness: &str, selection: JitSelection) -> Result<String, String> {
    let mut rt = Runtime::builder()
        .timeout(std::time::Duration::from_secs(10))
        .jit_selection(selection)
        .process_global(false)
        .worker_global(false)
        .build()
        .expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(harness.to_string()),
        "test262-harness.js",
    )
    .expect("harness script");
    rt.run_script(SourceInput::from_javascript(BODY.to_string()), "main.js")
        .map(|r| r.completion_string().to_string())
        .map_err(|e| format!("{e:?}"))
}

#[test]
fn compiled_entries_resolve_the_owning_chunk() {
    let Some(harness) = harness_source() else {
        // The vendored test262 harness is absent (submodule not initialised);
        // nothing to drive.
        return;
    };
    let oracle = run_pair(&harness, JitSelection::InterpreterOnly).expect("interpreter oracle");
    assert_eq!(
        run_pair(&harness, JitSelection::ProductionTiered).as_deref(),
        Ok(oracle.as_str())
    );
    assert_eq!(
        run_pair(&harness, JitSelection::Template).as_deref(),
        Ok(oracle.as_str())
    );
}
