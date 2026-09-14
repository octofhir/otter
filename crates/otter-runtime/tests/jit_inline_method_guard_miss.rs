//! A monomorphic inlined method whose receiver shape later changes.
//!
//! # Contents
//! - A hot caller whose method site inlines a pure numeric method for one
//!   receiver shape, then receives objects of a second shape that share the
//!   prototype and therefore the same method.
//!
//! # Invariants
//! - The inlined body's receiver guard miss falls through to the direct call
//!   and the generic packet; the caller never side-exits for the new shape.
//! - Every tier returns the interpreter's completion.

#![cfg(target_arch = "aarch64")]

use otter_runtime::{JitDebugEvent, JitDebugRequest, JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function Pt(x, y) { this.x = x; this.y = y; }
Pt.prototype.len2 = function() { return this.x * this.x + this.y * this.y; };
function total(points) {
  let acc = 0;
  for (let i = 0; i < points.length; i++) acc += points[i].len2();
  return acc;
}
const same = [];
for (let i = 0; i < 64; i++) same.push(new Pt(i, i + 1));
let acc = 0;
for (let r = 0; r < 200; r++) acc += total(same);
// Same prototype and method, different own-property order: a second shape.
const other = [];
for (let i = 0; i < 64; i++) {
  const p = Object.create(Pt.prototype);
  p.y = i + 1;
  p.x = i;
  other.push(p);
}
for (let r = 0; r < 200; r++) acc += total(other);
acc;
"#;

fn run(selection: JitSelection) -> (String, usize) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_debug(JitDebugRequest::events())
        .build()
        .expect("runtime");
    let result = runtime
        .run_script(
            SourceInput::from_javascript(SOURCE.to_string()),
            "jit-inline-method-guard-miss.js",
        )
        .expect("inline method guard miss");
    let completion = result.completion_string().to_owned();
    let report = result.jit_debug_report().expect("events enabled");
    let total_bails = report
        .events()
        .iter()
        .filter(|event| {
            matches!(
                event,
                JitDebugEvent::Bail { function_name, .. } if function_name == "total"
            )
        })
        .count();
    (completion, total_bails)
}

#[test]
fn an_inlined_method_guard_miss_falls_through_without_a_side_exit() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let (compiled, total_bails) = run(selection);
        assert_eq!(compiled, oracle, "{selection:?}");
        assert!(
            total_bails <= 1,
            "{selection:?}: a second receiver shape must reach the direct or generic layer, not exit the caller: {total_bails} bails"
        );
    }
}
