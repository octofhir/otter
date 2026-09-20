//! `arguments` bodies entered through generated direct calls.
//!
//! # Contents
//! - A `Class.create`-style constructor that forwards `arguments` to an
//!   `initialize` method, constructed from a hot compiled caller.
//! - Sloppy mapped `arguments` aliasing its parameter and strict unmapped
//!   `arguments` with more actuals than formals, both called from compiled
//!   callers.
//!
//! # Invariants
//! - The generated caller publishes every actual argument after the callee's
//!   register window, so the callee's `arguments` object sees the exact
//!   call-site list without a materialized interpreter frame.
//! - Every such site receives an available direct-call plan; the callee never
//!   side exits merely because it materializes `arguments`.
//! - Every tier returns the interpreter's completion.

use otter_runtime::{
    JitDebugEvent, JitDebugRequest, JitDirectCallKind, JitDirectCallPlanOutcome, JitSelection,
    Runtime, SourceInput,
};

const SOURCE: &str = r#"
var Class = {
  create: function() {
    return function() { this.initialize.apply(this, arguments); };
  }
};
var Point = Class.create();
Point.prototype.initialize = function(x, y, z) {
  this.x = x;
  this.y = y;
  this.z = z === undefined ? 0 : z;
  this.n = arguments.length;
};
function build(i) { return new Point(i, i + 1); }
function sloppyMapped(a, b) {
  arguments[0] = a + b;
  return a + "," + arguments[0] + "," + arguments.length;
}
function strictUnmapped(a, b) {
  "use strict";
  arguments[0] = 99;
  return a + "," + arguments[0] + "," + arguments.length + "," + arguments[2];
}
function tagged(o, s) {
  // Heap values sit in the published window while the arguments object is
  // allocated; a moving collection must retarget them there.
  return arguments[0].k + arguments[1] + arguments.length + arguments[2].length;
}
function drive(rounds) {
  let acc = 0;
  let tail = "";
  for (let i = 0; i < rounds; i++) {
    const p = build(i);
    acc += p.x + p.y + p.z + p.n;
    tail = sloppyMapped(i, 1) + "|" + strictUnmapped(i, 2, 3) + "|" +
      tagged({ k: i }, "s" + i, [i, i]);
  }
  return acc + "|" + tail;
}
drive(3000);
"#;

struct Run {
    completion: String,
    generated_callees: Vec<(JitDirectCallKind, String)>,
    bails: Vec<String>,
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
            "jit-generated-arguments.js",
        )
        .expect("arguments bodies through generated calls");
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
    let mut generated_callees = Vec::new();
    let mut bails = Vec::new();
    for event in report.events() {
        match event {
            JitDebugEvent::DirectCallPlan {
                call_kind,
                callee_function_id,
                outcome: JitDirectCallPlanOutcome::Available { .. },
                ..
            } => {
                let name = names
                    .get(callee_function_id)
                    .cloned()
                    .unwrap_or_else(|| format!("f{callee_function_id}"));
                generated_callees.push((*call_kind, name));
            }
            JitDebugEvent::Bail {
                function_name,
                op_debug,
                ..
            } => bails.push(format!("{function_name}@{op_debug:?}")),
            _ => {}
        }
    }
    Run {
        completion,
        generated_callees,
        bails,
    }
}

#[test]
fn arguments_bodies_match_the_interpreter_on_every_tier() {
    let oracle = run(JitSelection::InterpreterOnly).completion;
    assert_eq!(oracle, "9006000|3000,3000,2|2999,99,3,3|2999s299932");
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let compiled = run(selection).completion;
        assert_eq!(compiled, oracle, "{selection:?}");
    }
}

#[test]
fn arguments_bodies_take_generated_direct_linkage_without_side_exits() {
    let run = run(JitSelection::ProductionTiered);
    for (kind, name) in [
        (JitDirectCallKind::Construct, "<anonymous>"),
        (JitDirectCallKind::Plain, "sloppyMapped"),
        (JitDirectCallKind::Plain, "strictUnmapped"),
        (JitDirectCallKind::Plain, "tagged"),
    ] {
        assert!(
            run.generated_callees
                .iter()
                .any(|(generated_kind, generated)| {
                    *generated_kind == kind && generated.contains(name)
                }),
            "{name} materializes `arguments` and must still be linked directly: {:?}",
            run.generated_callees
        );
    }
    let collect_bails: Vec<_> = run
        .bails
        .iter()
        .filter(|bail| bail.contains("CollectArguments"))
        .collect();
    assert!(
        collect_bails.is_empty(),
        "CollectArguments must complete inside generated frames: {collect_bails:?}"
    );
}
