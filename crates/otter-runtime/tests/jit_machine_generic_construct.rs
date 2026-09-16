//! Machine IR constructs without a generated edge and coercive loose equality.
//!
//! # Contents
//! - A hot function whose `new` site alternates between two constructors, so
//!   no monomorphic construct plan exists; it completes through the generic
//!   construct call instead of keeping the function off the optimizing tier.
//! - A loose comparison whose operands are strings and objects; its feedback
//!   is coercive, so the site takes the committed canonical comparison while
//!   a nullish comparison in the same function keeps its direct lowering.
//!
//! # Invariants
//! - Every tier returns the interpreter's completion.
//! - Both fixture functions compile on the optimizing tier.

use otter_runtime::{
    JitDebugCompileOutcome, JitDebugEvent, JitDebugRequest, JitDebugTier, JitSelection, Runtime,
    SourceInput,
};

const SOURCE: &str = r#"
function Point(x) { this.x = x; }
function Tag(x) { this.tag = x; }
function buildPoly(i) {
  const made = (i & 1) === 0 ? new Point(i) : new Tag(i);
  return made.x === undefined ? made.tag : made.x;
}
const shared = { id: 1 };
function compareCoercive(i, other) {
  let hits = 0;
  if (String(i & 3) == "1") hits += 1;
  if (other == shared) hits += 10;
  if (other != null) hits += 100;
  return hits;
}
let acc = 0;
for (let i = 0; i < 7000; i++) {
  acc += buildPoly(i);
  acc += compareCoercive(i, (i & 7) === 0 ? shared : { id: i });
  acc += compareCoercive(i, null);
}
acc;
"#;

fn run(selection: JitSelection) -> (String, Vec<String>) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_debug(JitDebugRequest::events())
        .build()
        .expect("runtime");
    let result = runtime
        .run_script(
            SourceInput::from_javascript(SOURCE.to_string()),
            "jit-machine-generic-construct.js",
        )
        .expect("generic construct and coercive equality");
    let completion = result.completion_string().to_owned();
    let report = result.jit_debug_report().expect("events enabled");
    let mut names = std::collections::BTreeMap::new();
    for event in report.events() {
        if let JitDebugEvent::CompilePrepared {
            function_id,
            function_name,
            ..
        } = event
        {
            names.insert(*function_id, function_name.clone());
        }
    }
    let optimized = report
        .events()
        .iter()
        .filter_map(|event| match event {
            JitDebugEvent::CompileFinished {
                function_id,
                tier: JitDebugTier::Optimizing,
                outcome: JitDebugCompileOutcome::Compiled { .. },
                ..
            } => names.get(function_id).cloned(),
            _ => None,
        })
        .collect();
    (completion, optimized)
}

#[test]
fn generic_constructs_and_coercive_equality_match_the_interpreter() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let (compiled, _) = run(selection);
        assert_eq!(compiled, oracle, "{selection:?}");
    }
}

#[test]
fn unsettled_constructs_and_coercive_equality_stay_on_the_optimizing_tier() {
    let (_, optimized) = run(JitSelection::ProductionTiered);
    for name in ["buildPoly", "compareCoercive"] {
        assert!(
            optimized.iter().any(|compiled| compiled == name),
            "{name} must compile on the optimizing tier: {optimized:?}"
        );
    }
}
