//! Inlined constructors: receivers allocated in the caller's graph.
//!
//! # Contents
//! - Hot `new` sites of plain constructors whose bodies the graph tier
//!   splices into the caller, with the receiver allocated in place.
//! - Mid-body deopts, Object returns, a throw out of the spliced body and a
//!   replaced `prototype`, each against the interpreter oracle.
//!
//! # Invariants
//! - A spliced constructor resumes as a construct activation: a non-Object
//!   return completes with its receiver, an Object return replaces it.
//! - A replaced prototype fails the receiver proof and resumes the `new`.
//! - Every tier returns the interpreter's completion.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

use otter_runtime::{JitDebugEvent, JitDebugRequest, JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function Pair(car, cdr) { this.car = car; this.cdr = cdr; }
function Sum(a, b) { this.a = a; this.b = a + b; }
function Swap(a) { this.a = a; if (a === 1777) return { swapped: a }; }
function Fail(a) { this.a = a; if (a === 1500) throw new Error("fail " + a); this.b = 1; }
function Late(a) { this.k = a; }
function build(i) {
  const pair = new Pair(i, null);
  const sum = new Sum(i, i < 1900 ? i : "s" + i);
  const swap = new Swap(i);
  const late = new Late(i);
  return [pair.car, sum.b, swap.a, swap.swapped, late.k, late.extra, Object.keys(sum).join()].join("|");
}
function failing(i) { return new Fail(i).b; }
let log = [];
for (let i = 0; i < 2600; i++) {
  if (i === 2100) Late.prototype.extra = "proto";
  if (i === 2300) Late.prototype = { extra: "replaced" };
  let line = build(i);
  try { line += "|" + failing(i); } catch (e) { line += "|" + e.message; }
  if (i % 211 === 0 || [1500, 1777, 1899, 1900, 2100, 2300].includes(i)) log.push(line);
}
log.join("\n");
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
            "jit-graph-construct-inline.js",
        )
        .expect("construct inline matrix");
    let completion = result.completion_string().to_owned();
    let report = result.jit_debug_report().expect("events enabled");
    let names: std::collections::BTreeMap<u32, String> = report
        .events()
        .iter()
        .filter_map(|event| match event {
            JitDebugEvent::CompilePrepared {
                function_id,
                function_name,
                ..
            } => Some((*function_id, function_name.clone())),
            _ => None,
        })
        .collect();
    let inlined = report
        .events()
        .iter()
        .filter_map(|event| match event {
            JitDebugEvent::InlineLowered {
                callee_function_id, ..
            } => names.get(callee_function_id).cloned(),
            _ => None,
        })
        .collect();
    (completion, inlined)
}

#[test]
fn inlined_constructors_match_the_interpreter() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    let (compiled, inlined) = run(JitSelection::ProductionTiered);
    assert_eq!(compiled, oracle);
    for constructor in ["Pair", "Sum", "Swap", "Late"] {
        assert!(
            inlined.iter().any(|name| name == constructor),
            "{constructor} is spliced into its caller: {inlined:?}"
        );
    }
}
