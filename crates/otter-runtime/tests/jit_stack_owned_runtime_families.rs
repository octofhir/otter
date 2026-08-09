//! Stack-owned scalar, value-load, and class runtime-family coverage.
//!
//! # Contents
//! - Direct generated calls combining scalar queries and allocating value loads.
//! - Dynamic class heritage and computed function naming in a generated callee.
//! - Observable `ToPropertyKey` success and throw paths with exact effect counts.
//!
//! # Invariants
//! - Supported operations complete against the published native frame without
//!   materializing or replaying the callee.
//! - Interpreter and production-tiered results are byte-identical.
//! - A coercion hook runs once per specification operation, including abrupt completion.

use otter_runtime::{JitSelection, Runtime, RuntimeExecutionStats, SourceInput};

const DIRECT_FAMILIES: &str = r#"
function BoundaryBase(value) { this.total = value + 1; }
function scalarBoundary(value) {
  const array = [value];
  const tag = typeof value;
  const character = "otter"[1];
  const bigintSame = 1n === 1n;
  return value + array.length + (tag === "number") +
    (character === "t") + bigintSame;
}
function classBoundary(Base, key, value) {
  class Local extends Base {
    [key]() { return value + 1; }
  }
  return typeof Local === "function" ? value + 1 : 0;
}
for (let i = 0; i < 1000; i++) {
  scalarBoundary(i);
  classBoundary(BoundaryBase, "bump", i);
}
function run(rounds) {
  let checksum = 0;
  for (let i = 0; i < rounds; i++) {
    checksum += scalarBoundary(i);
    checksum += classBoundary(BoundaryBase, "bump", i);
  }
  return String(checksum);
}
run(1000);
"#;

fn run(source: &str, selection: JitSelection) -> (String, RuntimeExecutionStats) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(8)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(source.to_string()),
            "jit-stack-owned-runtime-families.js",
        )
        .expect("runtime-family completion")
        .completion_string()
        .to_owned();
    (completion, runtime.execution_stats())
}

#[test]
fn direct_generated_runtime_families_match_interpreter_without_deopt() {
    let (oracle, _) = run(DIRECT_FAMILIES, JitSelection::InterpreterOnly);
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let (compiled, stats) = run(DIRECT_FAMILIES, selection);
        assert_eq!(compiled, oracle);
        assert_eq!(compiled, "1004000");
        assert!(
            stats.jit_generated_template_returns > 0,
            "both direct targets must complete generated entries: {stats:?}"
        );
        assert_eq!(
            stats.jit_generated_call_deopts, 0,
            "typed runtime operations must not materialize the generated callee"
        );
        assert_eq!(
            stats.jit_generated_template_deopts, 0,
            "template callees must complete on their stack-owned windows"
        );
        assert_eq!(
            stats.jit_to_rust_call_transitions, 0,
            "stable direct targets must not fall back to the generic call boundary"
        );
    }
}

const OBSERVABLE_COERCION: &str = r#"
let effects = 0;
function keyed(key) {
  const object = { [key]: 1 };
  return object[key];
}
for (let i = 0; i < 1000; i++) keyed("stable");
const successful = {
  [Symbol.toPrimitive]() { effects++; return "stable"; }
};
const abrupt = {
  [Symbol.toPrimitive]() { effects++; throw new Error("stop"); }
};
const value = keyed(successful);
let caught = false;
try { keyed(abrupt); } catch (error) { caught = error.message === "stop"; }
JSON.stringify([value, effects, caught]);
"#;

#[test]
fn observable_coercion_commits_or_throws_without_replay() {
    let (oracle, _) = run(OBSERVABLE_COERCION, JitSelection::InterpreterOnly);
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let (compiled, stats) = run(OBSERVABLE_COERCION, selection);
        assert_eq!(compiled, oracle);
        assert_eq!(compiled, "[1,3,true]");
        assert!(
            stats.jit_generated_calls > 0,
            "warmup must establish generated linkage before observable inputs"
        );
        assert_eq!(
            stats.jit_generated_call_deopts, 0,
            "started coercions must complete or throw, never replay through deopt"
        );
    }
}
