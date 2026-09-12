//! `callee.apply(this, arguments)` forwarding without an arguments object.
//!
//! # Contents
//! - A `Class.create`-style trampoline constructed from a hot caller.
//! - A strict body forwarding extra actuals to a sloppy callee.
//! - Two forwards in one activation whose `apply` methods are overridden,
//!   observing the mapped arguments object and its identity.
//! - An arrow referencing the enclosing activation's `arguments`.
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
