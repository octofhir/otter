//! Stack-owned scalar, value-load, and class runtime-family coverage.
//!
//! # Contents
//! - Direct generated calls combining scalar queries and allocating value loads.
//! - Dynamic class heritage and computed function naming in a generated callee.
//! - Observable `ToPropertyKey` success and throw paths with exact effect counts.
//! - Plain, method, and construct linkage with fresh plus inherited upvalues.
//! - Exact direct-call artifact spine counts and abrupt side-exit behavior.
//!
//! # Invariants
//! - Supported operations complete against the published native frame without
//!   materializing or replaying the callee.
//! - Interpreter and production-tiered results are byte-identical.
//! - A coercion hook runs once per specification operation, including abrupt completion.

use otter_runtime::{
    JitArtifactFileName, JitDebugEvent, JitDebugRequest, JitDirectCallKind,
    JitDirectCallLoweringOutcome, JitSelection, Runtime, RuntimeExecutionStats, SourceInput,
};

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
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(source.to_string()),
            "jit-stack-owned-runtime-families.js",
        )
        .expect("runtime-family completion");
    let completion = completion.completion_string().to_owned();
    (completion, runtime.execution_stats())
}

fn run_with_events(source: &str, selection: JitSelection) -> (String, Vec<JitDebugEvent>) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_debug(JitDebugRequest::events())
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(source.to_string()),
            "jit-stack-owned-runtime-families-events.js",
        )
        .expect("runtime-family completion");
    let events = completion
        .jit_debug_report()
        .expect("event report")
        .events()
        .to_vec();
    (completion.completion_string().to_owned(), events)
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
        // Only sites without a direct target take the generic boundary: the
        // top-level `run(1000)` entered from the OSR-compiled script body and
        // the native `String(checksum)` call. Both direct targets stay native.
        assert!(
            stats.jit_to_rust_call_transitions <= 2,
            "stable direct targets must not fall back to the generic call boundary: {stats:?}"
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

const UPVALUE_CALL_FAMILIES: &str = r#"
function makePlain(offset) {
  return function plain(value) {
    let captured = value;
    function read() { return captured + offset; }
    return read();
  };
}
function makeHolder(offset) {
  return {
    method(value) {
      let captured = value;
      function read() { return captured + offset; }
      return read();
    }
  };
}
function makeBox(offset) {
  return function Box(value) {
    let captured = value;
    function read() { return captured + offset; }
    this.value = read();
  };
}
const plain = makePlain(1);
const holder = makeHolder(2);
const Box = makeBox(3);
for (let i = 0; i < 1000; i++) {
  plain(i);
  holder.method(i);
  new Box(i);
}
function run(rounds) {
  let checksum = 0;
  for (let i = 0; i < rounds; i++) {
    checksum += plain(i);
    checksum += holder.method(i);
    checksum += new Box(i).value;
  }
  return String(checksum);
}
run(1000);
"#;

#[test]
fn generated_plain_method_and_construct_calls_own_stack_upvalue_spines() {
    let (oracle, _) = run(UPVALUE_CALL_FAMILIES, JitSelection::InterpreterOnly);
    assert_eq!(oracle, "1504500");
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let (compiled, stats) = run(UPVALUE_CALL_FAMILIES, selection);
        assert_eq!(compiled, oracle);
        assert!(
            stats.jit_generated_calls > 1000,
            "plain/method/construct families must enter generated linkage: {stats:?}"
        );
        assert_eq!(
            stats.jit_generated_call_deopts, 0,
            "{selection:?}: {stats:?}"
        );
        assert_eq!(
            stats.jit_generated_template_deopts, 0,
            "{selection:?}: {stats:?}"
        );
        if selection == JitSelection::Template {
            // The hot plain/method/construct families stay native; only the
            // cold top-level `run(1000)` call and native `String(checksum)`
            // may cross the generic boundary.
            assert!(
                stats.jit_to_rust_call_transitions <= 2,
                "the template caller must keep every hot call family native: {stats:?}"
            );
        }

        let (event_completion, events) = run_with_events(UPVALUE_CALL_FAMILIES, selection);
        assert_eq!(event_completion, oracle);
        for expected in [
            JitDirectCallKind::Plain,
            JitDirectCallKind::Method,
            JitDirectCallKind::Construct,
        ] {
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    JitDebugEvent::DirectCallLowered {
                        call_kind,
                        outcome: JitDirectCallLoweringOutcome::Generated { .. },
                        ..
                    } if *call_kind == expected
                )),
                "{selection:?} did not generate {expected:?} linkage"
            );
        }
    }
}

#[test]
fn direct_call_artifacts_publish_exact_upvalue_spine_contracts() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::Template)
        .jit_debug(JitDebugRequest::artifacts())
        .build()
        .expect("artifact runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(UPVALUE_CALL_FAMILIES),
            "jit-upvalue-call-family-artifacts.js",
        )
        .expect("artifact completion");
    assert_eq!(completion.completion_string(), "1504500");
    let artifacts = completion.jit_artifacts().expect("artifact batch");
    let mut generated_kinds = std::collections::BTreeSet::new();
    for bundle in artifacts.bundles() {
        let Some(file) = bundle.file(JitArtifactFileName::CodeMap) else {
            continue;
        };
        let map: serde_json::Value =
            serde_json::from_slice(file.contents()).expect("valid code-map JSON");
        let Some(regions) = map["regions"].as_array() else {
            continue;
        };
        for direct in regions
            .iter()
            .filter_map(|region| region.get("directCall"))
            .filter(|direct| direct["ownUpvalueCount"] == 1 && direct["inheritedUpvalueCount"] == 1)
        {
            if let Some(kind) = direct["callKind"].as_str() {
                generated_kinds.insert(kind.to_owned());
            }
        }
    }
    assert_eq!(
        generated_kinds,
        ["construct", "method", "plain"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        "every generated outer edge must expose its exact fresh/inherited spine"
    );
}

const UPVALUE_THROW_FAMILIES: &str = r#"
let effects = 0;
function makePlain(offset) {
  return function plain(value) {
    let captured = value;
    function read() { return captured + offset; }
    const result = read();
    if (result < 0) { effects++; throw new Error("plain"); }
    return result;
  };
}
function makeHolder(offset) {
  return { method(value) {
    let captured = value;
    function read() { return captured + offset; }
    const result = read();
    if (result < 0) { effects++; throw new Error("method"); }
    return result;
  } };
}
function makeBox(offset) {
  return function Box(value) {
    let captured = value;
    function read() { return captured + offset; }
    const result = read();
    if (result < 0) { effects++; throw new Error("construct"); }
    this.value = result;
  };
}
const plain = makePlain(1);
const holder = makeHolder(2);
const Box = makeBox(3);
let caught = 0;
for (let i = 0; i <= 1000; i++) {
  const value = i === 1000 ? -10 : i;
  try { plain(value); } catch (error) { if (error.message === "plain") caught++; }
  try { holder.method(value); } catch (error) { if (error.message === "method") caught++; }
  try { new Box(value); } catch (error) { if (error.message === "construct") caught++; }
}
JSON.stringify([effects, caught]);
"#;

#[test]
fn upvalue_spine_abrupt_paths_commit_once_across_generated_side_exits() {
    let (oracle, _) = run(UPVALUE_THROW_FAMILIES, JitSelection::InterpreterOnly);
    assert_eq!(oracle, "[3,3]");
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let (compiled, stats) = run(UPVALUE_THROW_FAMILIES, selection);
        assert_eq!(compiled, oracle);
        assert!(stats.jit_generated_calls > 1000, "{selection:?}: {stats:?}");
        // A callee throw unwinds through the throw-routing transition straight
        // to the caller's handler; it is neither a replayed call nor a deopt.
        assert!(
            stats.jit_generated_call_deopts <= 3,
            "each first abrupt family may side-exit once, never replay: {stats:?}"
        );
    }
}
