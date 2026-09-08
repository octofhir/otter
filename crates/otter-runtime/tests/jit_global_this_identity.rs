//! `globalThis` read through generated code keeps object identity.
//!
//! # Contents
//! - A hot function returning `globalThis`, compared by `===` and `Object.is`
//!   against the interpreter-materialized global object, on every tier.
//!
//! # Invariants
//! - A generated `globalThis` read produces the same Value bits as the
//!   interpreter: the isolate publishes the global object as a compressed
//!   cage offset, and both tiers must rebase it to the full address before it
//!   reaches a register that `===` bit-compares and the collector traces.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
const g = globalThis;
function reads() { return globalThis; }
let same = true;
for (let i = 0; i < 20000; i++) {
  if (reads() !== g || !Object.is(reads(), g)) same = false;
}
String([same, reads() === g, Object.is(reads(), g), reads().Math === Math]);
"#;

fn run(selection: JitSelection) -> String {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(8)
        .build()
        .expect("runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(SOURCE.to_string()),
            "jit-global-this-identity.js",
        )
        .expect("globalThis identity loop")
        .completion_string()
        .to_owned()
}

#[test]
fn generated_global_this_reads_keep_identity_on_every_tier() {
    assert_eq!(run(JitSelection::InterpreterOnly), "true,true,true,true");
    assert_eq!(run(JitSelection::Template), "true,true,true,true");
    assert_eq!(run(JitSelection::ProductionTiered), "true,true,true,true");
}
