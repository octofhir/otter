//! Machine IR binding-family coverage.
//!
//! # Contents
//! - Captured, global-lexical, and global-object reads and writes in one hot
//!   loop.
//! - Bounded eval-chain lookup over a shadowed capture, including delete.
//! - Production-tier entry/deopt assertions after the binding proofs settle.
//!
//! # Invariants
//! - Binding operations do not reject an otherwise supported Machine body.
//! - Generated hit paths and committed cold paths both preserve exact
//!   interpreter results without deoptimizing and replaying the access.
//! - A shadowed binding visits exactly its schema-encoded eval depth before
//!   applying the captured-owner fallback; it cannot leak into an older eval
//!   environment.

use otter_runtime::{JitSelection, Runtime, RuntimeExecutionStats, SourceInput};

const SETUP: &str = r#"
let machineBindingLexical = 1;
globalThis.machineBindingObject = 2;

function makeMachineBindingHot() {
  let captured = 3;
  return function machineBindingHot(limit) {
    let checksum = 0;
    for (let index = 0; index < limit; index++) {
      captured += 1;
      machineBindingLexical += 2;
      machineBindingObject += 3;
      checksum += captured + machineBindingLexical + machineBindingObject;
    }
    return checksum;
  };
}

let machineBindingHot = makeMachineBindingHot();
for (let warm = 0; warm < 32; warm++) machineBindingHot(64);
"#;

const PROBE: &str = "machineBindingHot(256);";

const SHADOW_SETUP: &str = r#"
function makeMachineShadowHot() {
  let captured = 5;
  function install() {
    eval("var captured = 7");
    return function machineShadowHot(limit) {
      let checksum = 0;
      for (let index = 0; index < limit; index++) {
        captured += 2;
        checksum += captured;
      }
      return checksum;
    };
  }
  return [install(), function machineShadowOuter() { return captured; }];
}

let machineShadowPair = makeMachineShadowHot();
let machineShadowHot = machineShadowPair[0];
let machineShadowOuter = machineShadowPair[1];
for (let warm = 0; warm < 32; warm++) machineShadowHot(64);
"#;

const SHADOW_PROBE: &str = "JSON.stringify([machineShadowHot(256), machineShadowOuter()]);";

const EXACT_DEPTH_SETUP: &str = r#"
function makeMachineExactDepth() {
  eval("var x = 1");

  function outerRead() { return x; }
  function owner() {
    let x = 2;
    function ownerRead() { return x; }
    function installer() {
      eval("var y = 0");
      return function machineExactDepthHot(limit) {
        let checksum = 0;
        for (let index = 0; index < limit; index++) {
          x += 1;
          checksum += x;
          if (delete x) checksum += 1000000;
        }
        return checksum;
      };
    }
    return [installer(), ownerRead];
  }

  const owned = owner();
  return [owned[0], owned[1], outerRead];
}

const machineExactDepth = makeMachineExactDepth();
const machineExactDepthHot = machineExactDepth[0];
const machineExactDepthOwner = machineExactDepth[1];
const machineExactDepthOuter = machineExactDepth[2];
for (let warm = 0; warm < 32; warm++) machineExactDepthHot(64);
"#;

const EXACT_DEPTH_PROBE: &str = r#"
machineExactDepthHot(256) + ":" + machineExactDepthOwner() + ":" + machineExactDepthOuter();
"#;

fn run(selection: JitSelection) -> (String, RuntimeExecutionStats, RuntimeExecutionStats) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .build()
        .expect("Machine binding runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(SETUP.to_owned()),
            "jit-machine-bindings-setup.js",
        )
        .expect("Machine binding setup");
    let before = runtime.execution_stats();
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(PROBE.to_owned()),
            "jit-machine-bindings-probe.js",
        )
        .expect("Machine binding probe")
        .completion_string()
        .to_owned();
    (completion, before, runtime.execution_stats())
}

#[test]
fn binding_reads_and_writes_stay_in_machine_without_deopt() {
    let (oracle, _, _) = run(JitSelection::InterpreterOnly);
    assert_eq!(oracle, "3344640");
    let (compiled, before, after) = run(JitSelection::ProductionTiered);
    assert_eq!(compiled, oracle);
    assert!(
        after.jit_optimized_entries > before.jit_optimized_entries,
        "the probe must enter the optimized binding body: {before:?} -> {after:?}"
    );
    assert_eq!(
        after.jit_optimized_deopts, before.jit_optimized_deopts,
        "stable binding proofs must not deopt: {before:?} -> {after:?}"
    );
}

#[test]
fn inherited_eval_shadow_stays_committed_inside_machine() {
    fn run_shadow(
        selection: JitSelection,
    ) -> (String, RuntimeExecutionStats, RuntimeExecutionStats) {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .build()
            .expect("Machine shadowed-binding runtime");
        runtime
            .run_script(
                SourceInput::from_javascript(SHADOW_SETUP.to_owned()),
                "jit-machine-shadowed-bindings-setup.js",
            )
            .expect("Machine shadowed-binding setup");
        let before = runtime.execution_stats();
        let completion = runtime
            .run_script(
                SourceInput::from_javascript(SHADOW_PROBE.to_owned()),
                "jit-machine-shadowed-bindings-probe.js",
            )
            .expect("Machine shadowed-binding probe")
            .completion_string()
            .to_owned();
        (completion, before, runtime.execution_stats())
    }

    let (oracle, _, _) = run_shadow(JitSelection::InterpreterOnly);
    assert_eq!(oracle, "[1116160,5]");
    let (compiled, before, after) = run_shadow(JitSelection::ProductionTiered);
    assert_eq!(compiled, oracle);
    assert!(
        after.jit_optimized_entries > before.jit_optimized_entries,
        "the descendant shadowed-binding body must enter Machine: {before:?} -> {after:?}"
    );
    assert!(
        after.jit_reentrant_stub_transitions > before.jit_reentrant_stub_transitions,
        "shadowed access must use committed cold calls: {before:?} -> {after:?}"
    );
    assert_eq!(
        after.jit_optimized_deopts, before.jit_optimized_deopts,
        "shadowed semantics must not deopt/replay: {before:?} -> {after:?}"
    );
}

#[test]
fn shadowed_binding_stops_at_the_schema_encoded_eval_depth() {
    fn run_exact_depth(
        selection: JitSelection,
    ) -> (String, RuntimeExecutionStats, RuntimeExecutionStats) {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .build()
            .expect("Machine exact-depth runtime");
        runtime
            .run_script(
                SourceInput::from_javascript(EXACT_DEPTH_SETUP.to_owned()),
                "jit-machine-exact-depth-setup.js",
            )
            .expect("Machine exact-depth setup");
        let before = runtime.execution_stats();
        let completion = runtime
            .run_script(
                SourceInput::from_javascript(EXACT_DEPTH_PROBE.to_owned()),
                "jit-machine-exact-depth-probe.js",
            )
            .expect("Machine exact-depth probe")
            .completion_string()
            .to_owned();
        (completion, before, runtime.execution_stats())
    }

    let (oracle, _, _) = run_exact_depth(JitSelection::InterpreterOnly);
    assert_eq!(oracle, "557696:2306:1");
    let (compiled, before, after) = run_exact_depth(JitSelection::ProductionTiered);
    assert_eq!(compiled, oracle);
    assert!(
        after.jit_optimized_entries > before.jit_optimized_entries,
        "the exact-depth descendant must enter Machine: {before:?} -> {after:?}"
    );
    assert!(
        after.jit_reentrant_stub_transitions > before.jit_reentrant_stub_transitions,
        "bounded shadowed read/write/delete must stay committed: {before:?} -> {after:?}"
    );
    assert_eq!(
        after.jit_optimized_deopts, before.jit_optimized_deopts,
        "bounded eval traversal must not deopt/replay: {before:?} -> {after:?}"
    );
}
