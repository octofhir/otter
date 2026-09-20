//! Optimizing-tier promotion of callees entered only through generated calls.
//!
//! # Contents
//! - A hot loop that compiles early and thereafter calls its callee through
//!   the generated direct-call edge, so the callee is never entered from the
//!   interpreter again.
//!
//! # Invariants
//! - The callee's generated entries count toward optimizing-tier hotness, and
//!   the compiled loop's own poll drains the promotion, so the callee reaches
//!   the optimizing tier without a single interpreter entry after tier-up.
//! - Every tier returns the interpreter's completion.

use otter_runtime::{
    JitDebugCompileOutcome, JitDebugEvent, JitDebugRequest, JitDebugTier, JitSelection, Runtime,
    SourceInput,
};

const SOURCE: &str = r#"
function Node(v) { this.v = v; this.next = null; }
function nodeSum(acc) {
  if (acc < 0) return nodeSum(-acc);
  return acc + this.v;
}
Node.prototype.sum = nodeSum;
function Holder(node) { this.node = node; this.total = 0; }
// The loop keeps `step` out of the caller's inline tree, so every call is a
// real generated entry. `nodeSum` retains an unreachable-in-this-fixture
// recursive edge so it cannot be spliced into `step`; every post-warmup
// execution must pass through its own entry and contribute tiering feedback.
function step(h) {
  let t = 0;
  for (let k = 0; k < 2; k++) t = h.node.sum(h.total);
  h.total = t;
  return t;
}
function drive(rounds) {
  const holder = new Holder(new Node(3));
  let acc = 0;
  for (let i = 0; i < rounds; i++) {
    acc += step(holder);
    if (holder.total > 1000000) holder.total = 0;
  }
  return acc + "|" + holder.total;
}
drive(60000);
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
            "jit-generated-callee-promotion.js",
        )
        .expect("generated callee loop");
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
fn generated_callees_match_the_interpreter_on_every_tier() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let (compiled, _) = run(selection);
        assert_eq!(compiled, oracle, "{selection:?}");
    }
}

#[test]
fn a_callee_entered_only_through_generated_calls_still_promotes() {
    let (_, optimized) = run(JitSelection::ProductionTiered);
    for name in ["step", "nodeSum"] {
        assert!(
            optimized.iter().any(|compiled| compiled == name),
            "{name} is entered only through generated calls and must still reach the optimizing tier: {optimized:?}"
        );
    }
}
