//! Lean callback JIT re-entry lifecycle invariants.
//!
//! # Contents
//! - A hot `Array.prototype.map` callback that enters compiled code with an
//!   integer-specialized register window, then bails out on doubles.
//! - Allocation churn before the bailout so GC-stress runs relocate the
//!   callback, receiver, and activation-owned values.
//!
//! # Invariants
//! - The recycled lean callback frame is published through the collector root
//!   walk before compiled execution starts.
//! - Bailout resumes above the callback's activation floor and releases every
//!   frame/register window before the next callback invocation.
//! - A later lean invocation remains reusable after the bailout.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function addOne(value) {
  return value + 1;
}

const ints = [1, 2, 3, 4];
let warmChecksum = 0;
for (let i = 0; i < 192; i++) {
  const mapped = ints.map(addOne);
  warmChecksum += mapped[0] + mapped[3];
}

// Force moving-GC opportunities after the callback frame has become reusable.
const churn = [];
for (let i = 0; i < 256; i++) {
  churn.push({ index: i, text: "value-" + i });
}

// The integer-specialized callback must bail to the interpreter for doubles,
// then remain valid for a later compiled integer invocation.
const doubles = [1.5, 2.5, 3.5].map(addOne);
const again = [10, 20, 30].map(addOne);
JSON.stringify([warmChecksum, doubles, again, churn.length]);
"#;

fn run(selection: JitSelection) -> (String, u64) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(u32::MAX)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(SOURCE.to_string()),
            "jit-lean-callback-lifecycle.js",
        )
        .expect("lean callback fixture")
        .completion_string()
        .to_owned();
    (completion, runtime.execution_stats().jit_compile_attempts)
}

#[test]
fn lean_callback_bailout_releases_to_its_activation_floor() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    let (compiled, compile_attempts) = run(JitSelection::Template);

    assert_eq!(compiled, oracle);
    assert_eq!(compiled, "[1344,[2.5,3.5,4.5],[11,21,31],256]");
    assert!(compile_attempts > 0, "fixture must tier the lean callback");
}
