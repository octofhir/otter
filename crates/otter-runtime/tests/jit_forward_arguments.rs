//! `callee.apply(this, arguments)` forwarding without an arguments object.
//!
//! # Contents
//! - A `Class.create`-style trampoline constructed from a hot caller.
//! - A strict body forwarding extra actuals to a sloppy callee.
//! - Two forwards in one activation whose `apply` methods are overridden,
//!   observing the mapped arguments object and its identity.
//! - An arrow referencing the enclosing activation's `arguments`.
//! - Live mapped parameters and mutated/materialized argument lists, including
//!   allocating length coercion and index getters.
//! - Target growth after tier-up invalidates and replans forwarding feedback.
//! - Native polymorphic hits, dynamic actual windows, bounded fallback, moving
//!   roots and exact exceptions after warmup.
//!
//! # Invariants
//! - Bodies that only forward `arguments` never materialize the object on
//!   the intrinsic path, in the interpreter and in generated code, and never
//!   side-exit for it.
//! - A non-intrinsic `apply` observes exactly the object a materialized
//!   `arguments` binding would have produced, once per activation.
//! - Every tier returns the interpreter's completion.

#![cfg(target_arch = "aarch64")]

use otter_runtime::{JitDebugEvent, JitDebugRequest, JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
var Class = { create: function() { return function() { this.initialize.apply(this, arguments); }; } };
var Point = Class.create();
Point.prototype.initialize = function(x, y, z) { this.x = x; this.y = y; this.z = z === undefined ? 0 : z; this.n = arguments.length; };
function twice(a, b) {
  seen.length = 0;
  first.apply(this, arguments);
  second.apply(this, arguments);
  return a + "," + b + "," + (seen[0] === seen[1]);
}
var seen = [];
function first() { return 1; }
function second() { return 2; }
first.apply = function(receiver, list) { seen.push(list); list[0] = "A"; return 0; };
second.apply = function(receiver, list) { seen.push(list); return 0; };
function strictForward(a, b) { "use strict"; return target.apply(null, arguments); }
function target() { return arguments.length + ":" + arguments[0] + ":" + (this === globalThis); }
function arrowRef(a) { const f = () => target.apply(null, arguments); return f(); }
let acc = 0;
let tail = "";
for (let i = 0; i < 3000; i++) {
  const p = new Point(i, i + 1);
  acc += p.x + p.y + p.z + p.n;
  tail = twice(i, 1) + "|" + strictForward(i, 2, 3) + "|" + arrowRef(i);
}
acc + "|" + tail;
"#;

struct Run {
    completion: String,
    forward_bails: Vec<String>,
    forward_deopts: Vec<String>,
}

fn run(selection: JitSelection) -> Run {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_debug(JitDebugRequest::events())
        .build()
        .expect("runtime");
    let result = runtime
        .run_script(
            SourceInput::from_javascript(SOURCE.to_string()),
            "jit-forward-arguments.js",
        )
        .expect("forwarded apply");
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
    let forwarding = |name: &str| name == "<anonymous>" || name == "strictForward";
    let mut forward_bails = Vec::new();
    let mut forward_deopts = Vec::new();
    for event in report.events() {
        match event {
            JitDebugEvent::Bail {
                function_name,
                op_debug,
                ..
            } if forwarding(function_name) => {
                forward_bails.push(format!("{function_name}@{op_debug:?}"));
            }
            JitDebugEvent::GeneratedCallDeopt {
                callee_function_id,
                callee_resume_pc,
                ..
            } => {
                let name = names.get(callee_function_id).cloned().unwrap_or_default();
                if forwarding(&name) {
                    forward_deopts.push(format!("{name}@{callee_resume_pc}"));
                }
            }
            _ => {}
        }
    }
    Run {
        completion,
        forward_bails,
        forward_deopts,
    }
}

#[test]
fn forwarded_apply_matches_the_interpreter_on_every_tier() {
    let oracle = run(JitSelection::InterpreterOnly).completion;
    assert_eq!(oracle, "9006000|A,1,true|3:2999:true|1:2999:true");
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let compiled = run(selection).completion;
        assert_eq!(compiled, oracle, "{selection:?}");
    }
}

#[test]
fn intrinsic_forwards_complete_in_generated_code_without_side_exits() {
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let run = run(selection);
        assert!(
            run.forward_bails.is_empty() && run.forward_deopts.is_empty(),
            "{selection:?}: intrinsic forwards must not leave generated code: bails={:?} deopts={:?}",
            run.forward_bails,
            run.forward_deopts
        );
    }
}

#[test]
fn apply_lookup_precedes_uninitialized_derived_this() {
    let source = r#"
let reads = 0;
function target() {}
Object.defineProperty(target, "apply", {
  get() { reads++; throw new Error("lookup"); }
});
class Base {}
class Derived extends Base {
  constructor() { target.apply(this, arguments); }
}
let result = "";
try { new Derived(); } catch (error) { result = error.message; }
reads + "|" + result;
"#;
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .build()
            .expect("runtime");
        let result = runtime
            .run_script(
                SourceInput::from_javascript(source.to_owned()),
                "apply-order.js",
            )
            .expect("caught lookup");
        assert_eq!(result.completion_string(), "1|lookup", "{selection:?}");
    }
}

#[test]
fn overridden_apply_getter_runs_once_per_generated_forward() {
    let source = r#"
let reads = 0;
let calls = 0;
function target() {}
Object.defineProperty(target, "apply", {
  get() {
    reads++;
    return function(receiver, args) { calls++; return args[0]; };
  }
});
function forward(value) { return target.apply(null, arguments); }
let sum = 0;
for (let i = 0; i < 6000; i++) sum += forward(i);
reads + "|" + calls + "|" + sum;
"#;
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .build()
            .expect("runtime");
        let result = runtime
            .run_script(
                SourceInput::from_javascript(source.to_owned()),
                "apply-once.js",
            )
            .unwrap_or_else(|error| panic!("{selection:?}: {error:?}"));
        assert_eq!(
            result.completion_string(),
            "6000|6000|17997000",
            "{selection:?}"
        );
    }
}

#[test]
fn forwarded_arguments_read_live_mappings_and_materialized_objects() {
    let source = include_str!("../../otter-difftest/corpus/forward_arguments_live.js");
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .build()
            .expect("runtime");
        let result = runtime
            .run_script(SourceInput::from_javascript(source), "forward-live.js")
            .unwrap_or_else(|error| panic!("{selection:?}: {error:?}"));
        assert_eq!(
            result.completion_string(),
            "[210000,[\"42\",\"1,9,3\",\"1\",\"1\",\"1\",\"43,99\"],64,128,64,64,0]",
            "{selection:?}"
        );
    }
}

#[test]
fn generated_forwarding_replans_when_the_resolved_target_changes() {
    let source = r#"
function first(value) { return value + 1; }
function second(value) { return value + 2; }
let selected = first;
function forward(value) { return selected.apply(null, arguments); }
let sum = 0;
for (let i = 0; i < 5000; i++) sum += forward(i);
selected = second;
for (let i = 0; i < 5000; i++) sum += forward(i);
sum;
"#;
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::events())
            .build()
            .expect("runtime");
        let result = runtime
            .run_script(
                SourceInput::from_javascript(source.to_owned()),
                "forward-target-growth.js",
            )
            .expect("forwarded target switch");
        assert_eq!(result.completion_string(), "25010000", "{selection:?}");
        let report = result.jit_debug_report().expect("events");
        let forward_id = report
            .events()
            .iter()
            .find_map(|event| match event {
                JitDebugEvent::CompilePrepared {
                    function_id,
                    function_name,
                    ..
                } if function_name == "forward" => Some(*function_id),
                _ => None,
            })
            .expect("forward must enter generated code before the target changes");
        let mut monomorphic = false;
        let mut grew_after_compile = false;
        for event in report.events() {
            if let JitDebugEvent::InlineCandidate {
                caller_function_id,
                callee_function_id,
                bake_rejection,
                ..
            } = event
                && *caller_function_id == forward_id
            {
                if callee_function_id.is_some() {
                    monomorphic = true;
                }
                if matches!(
                    bake_rejection,
                    Some(otter_runtime::JitInlineRejectionReason::Polymorphic)
                ) {
                    grew_after_compile |= monomorphic;
                }
            }
        }
        assert!(
            grew_after_compile,
            "{selection:?}: compiled forwarding must publish its new target and replan the monomorphic snapshot"
        );
    }
}

#[test]
fn native_forwarding_copies_full_live_windows_and_preserves_hot_throws() {
    const SOURCE: &str = include_str!("../../otter-difftest/corpus/forward_arguments_native.js");
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .build()
            .expect("runtime");
        let result = runtime
            .run_script(
                SourceInput::from_javascript(SOURCE.to_owned()),
                "forward-native.js",
            )
            .unwrap_or_else(|error| panic!("{selection:?}: {error:?}"));
        assert_eq!(
            result.completion_string(),
            "[4290560,[600,1,0],1024,1024]",
            "{selection:?}"
        );
        if selection == JitSelection::InterpreterOnly {
            continue;
        }
        let mut dynamic = false;
        let mut polymorphic = false;
        let mut count_target = false;
        for bundle in result.jit_artifacts().expect("artifacts").bundles() {
            if bundle.manifest().function_name() != "forward" {
                continue;
            }
            let Some(file) = bundle.file(otter_runtime::JitArtifactFileName::CodeMap) else {
                continue;
            };
            let code_map = std::str::from_utf8(file.contents()).expect("code map UTF-8");
            if code_map.contains("\"argumentMode\": \"forward\"") {
                dynamic |= code_map.contains("\"linkageBytes\": null")
                    && code_map.contains("\"reservedStackBytes\": null");
                polymorphic |= code_map.contains("\"targetCount\": 2");
                count_target |= code_map.contains("\"targetCount\": 3");
            }
        }
        assert!(
            dynamic && polymorphic && count_target,
            "{selection:?}: forward must publish dynamic windows, both native candidates and the warmed arity target"
        );
        let stats = runtime.execution_stats();
        assert!(stats.jit_generated_calls > 0, "{selection:?}");
    }
}

#[test]
fn settled_polymorphic_forwards_avoid_rooted_runtime_calls() {
    let setup = r#"
function first(a, b) { return a + b + arguments[2] + arguments.length; }
function second(a, b) { 'use strict'; return a * 2 + b + arguments[2] + arguments.length; }
var chosen = first;
function forward(a, b, c) { return chosen.apply(null, arguments); }
for (var i = 0; i < 2048; i++) forward(i, 2, 3);
chosen = second;
for (var i = 0; i < 2048; i++) forward(i, 2, 3);
"#;
    let probe = r#"
var checksum = 0;
for (var j = 0; j < 256; j++) {
  chosen = (j & 1) ? second : first;
  checksum += forward(j, 2, 3);
}
checksum;
"#;
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .build()
            .expect("runtime");
        runtime
            .run_script(SourceInput::from_javascript(setup), "forward-setup.js")
            .expect("warm targets");
        let before = runtime.execution_stats();
        let result = runtime
            .run_script(SourceInput::from_javascript(probe), "forward-probe.js")
            .expect("settled forwards");
        assert_eq!(result.completion_string(), "51072", "{selection:?}");
        let after = runtime.execution_stats();
        let runtime_calls =
            after.jit_to_rust_call_transitions - before.jit_to_rust_call_transitions;
        let native_calls = after.jit_generated_calls - before.jit_generated_calls;
        assert!(
            native_calls >= 256,
            "{selection:?}: native calls={native_calls}"
        );
        assert!(
            runtime_calls < 64,
            "{selection:?}: rooted runtime calls={runtime_calls}"
        );
    }
}
