//! Runtime parity coverage for destination-aware declaration initializers.
//!
//! # Contents
//! - Literal, logical, conditional, coalescing, and sequence initializers.
//! - Observable short-circuit side effects across all execution tiers.
//!
//! # Invariants
//! - Reusing a binding register changes only the final result location.
//! - Operand evaluation order and short-circuit behavior remain unchanged.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function exercise(flag) {
  let effects = 0;
  let literal = 7;
  let logical = flag && (effects = effects + 1);
  let fallback = flag || (effects = effects + 2);
  let coalesce = null ?? (effects = effects + 4);
  let conditional = flag ? (effects = effects + 8) : (effects = effects + 16);
  let sequence = (effects = effects + 32, effects);
  return [literal, logical, fallback, coalesce, conditional, sequence, effects];
}

for (let i = 0; i < 100; i++) {
  exercise(false);
  exercise(true);
}

JSON.stringify([exercise(false), exercise(true)]);
"#;

fn run(jit: JitSelection) -> String {
    let mut runtime = Runtime::builder()
        .jit_selection(jit)
        .jit_osr_threshold(1)
        .build()
        .expect("runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(SOURCE),
            "destination-aware-initializers.js",
        )
        .expect("initializer script")
        .completion_string()
        .to_owned()
}

#[test]
fn destination_aware_initializers_match_across_execution_tiers() {
    let expected = "[[7,false,2,6,22,54,54],[7,1,true,5,13,45,45]]";
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        assert_eq!(run(selection), expected, "{selection:?}");
    }
}
