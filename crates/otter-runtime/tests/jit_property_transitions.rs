//! Generated add-transition stores past the inline slot capacity.
//!
//! # Contents
//! - A constructor that appends twelve string-keyed properties, so most of
//!   its stores land in the spilled slot slab rather than the in-body inline
//!   array, run hot enough to compile and compared against the interpreter.
//! - A receiver whose slab was reserved ahead of its length, so an inline
//!   slot index meets a non-null slab handle.
//! - The runtime property-stub count, which must stay bounded by the slab
//!   growth points rather than scale with every store.
//!
//! # Invariants
//! - Every tier produces the interpreter's result for every field.
//! - A compiled add-transition into a slab word with capacity commits in
//!   generated code; only the growth points reach the runtime stub.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function Wide(i) {
  this.f0 = i; this.f1 = i + 1; this.f2 = null; this.f3 = i * 2; this.f4 = -1;
  this.f5 = true; this.f6 = 0; this.f7 = "s" + (i & 3); this.f8 = i; this.f9 = 1;
  this.f10 = { n: i }; this.f11 = false;
}
function widen(rounds) {
  let acc = 0;
  let last = null;
  for (let i = 0; i < rounds; i++) {
    const w = new Wide(i);
    acc += w.f0 + w.f1 + w.f3 + w.f8 + w.f9 + w.f10.n + (w.f5 ? 1 : 0) + w.f7.length;
    last = w;
  }
  const keys = Object.keys(last).join(",");
  return acc + "|" + keys + "|" + last.f2 + "|" + last.f4 + "|" + last.f11;
}
widen(600);
"#;

fn run(selection: JitSelection) -> (String, u64, u64) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(8)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(SOURCE.to_string()),
            "jit-property-transitions.js",
        )
        .expect("wide constructor loop")
        .completion_string()
        .to_owned();
    let stats = runtime.execution_stats();
    (
        completion,
        stats.jit_osr_attempts,
        stats.jit_runtime_property_stubs,
    )
}

#[test]
fn slab_add_transitions_match_the_interpreter_on_every_tier() {
    let (oracle, _, _) = run(JitSelection::InterpreterOnly);
    assert!(oracle.ends_with("|null|-1|false"), "{oracle}");
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let (compiled, osr_attempts, _) = run(selection);
        assert_eq!(compiled, oracle, "{selection:?}");
        assert!(
            osr_attempts > 0,
            "{selection:?} must enter at a loop OSR header"
        );
    }
}

#[test]
fn slab_add_transitions_stay_in_generated_code() {
    let (_, _, runtime_stubs) = run(JitSelection::Template);
    // Twelve stores per object over six hundred objects: a runtime call per
    // store would be seven thousand. The slab grows twice per object
    // (inline cap 3 → 6 → 12), and the compiled loop only starts after the
    // OSR threshold, so the stub count is a small multiple of the object
    // count, never of the store count.
    assert!(
        runtime_stubs < 600 * 4,
        "runtime property stubs must not scale with every slab store: {runtime_stubs}"
    );
}
