//! Exact callee deoptimization after physical Graph tail transfer.
//!
//! # Contents
//! - Fitting and growing actual spans followed by arithmetic reconstruction.
//!
//! # Invariants
//! - Normal background compilation admits every current own entry.
//! - The transferred child owns the one exit; the bridge and retired tail
//!   remain native, and the child's preceding observable effect runs once.
//! - Reusing the bridge after full collection preserves its exact generation.
//!
//! # See also
//! - The parent fixture proves moving-GC suspended sources and native tail bytes.

use super::*;
use otter_vm::native_abi::{ExitAction, ExitReason};

#[test]
fn transferred_child_owns_overflow_without_replaying_the_tail_or_its_effects() {
    for actuals in ["value, payload", "value, payload, 7, 8, 9"] {
        let mut fixture = Fixture::new(JitSelection::ProductionTiered);
        fixture.setup(&format!(
            r#"
function tailDeoptChild(value, payload) {{
  if (arguments === payload) throw "distinct child arguments";
  payload.calls = payload.calls + 1;
  return value + 1;
}}
function tailDeoptHop(value, payload) {{
  "use strict";
  if (arguments === payload) throw "distinct tail arguments";
  return tailDeoptChild({actuals});
}}
function tailDeoptBridge(value, payload) {{
  const result = tailDeoptHop(value, payload); return result;
}}
globalThis.tailDeoptPayload = {{calls: 0}};
for (let warm = 0; warm < 5000; warm++) {{
  tailDeoptChild(warm & 31, tailDeoptPayload);
  tailDeoptHop(warm & 31, tailDeoptPayload);
  tailDeoptBridge(warm & 31, tailDeoptPayload);
}}
"#
        ));
        let names = ["tailDeoptBridge", "tailDeoptHop", "tailDeoptChild"];
        for batch in 0..128 {
            if fixture.runtime.jit_code_residency().pending_compilations == 0
                && names.iter().all(|name| {
                    fixture
                        .current(name)
                        .is_some_and(|generation| generation.tier == NativeFrameKind::Optimizing)
                })
            {
                break;
            }
            fixture.setup(&"tailDeoptBridge(7, tailDeoptPayload); tailDeoptHop(7, tailDeoptPayload); tailDeoptChild(7, tailDeoptPayload);\n".repeat(32));
            assert!(batch < 127, "bounded ordinary native admission");
        }
        fixture.edge(names[0], names[1]);
        fixture.tail_edge(names[1], names[2]);
        let generations: Vec<_> = names
            .iter()
            .map(|name| fixture.current(name).unwrap())
            .collect();
        fixture.run("tailDeoptPayload.calls = 0;", "tail-deopt-reset.js");
        {
            let mut trace = fixture.trace.lock().unwrap();
            trace.ticks.clear();
            trace.dropped = 0;
            trace.recording = true;
        }
        let before = fixture.runtime.execution_stats();
        let result = fixture.run("JSON.stringify([tailDeoptBridge(2147483647, tailDeoptPayload), tailDeoptPayload.calls]);", "tail-deopt-probe.js");
        fixture.trace.lock().unwrap().recording = false;
        assert_eq!(result.completion_string(), "[2147483648,1]");
        let report = result.jit_debug_report().unwrap();
        assert!(!report.truncated());
        assert_eq!(report.dropped_events(), 0);
        let exits: Vec<_> = report
            .events()
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    JitDebugEvent::Bail { .. }
                        | JitDebugEvent::EnteredGenerationDeopt { .. }
                        | JitDebugEvent::InlineDeoptFrame { .. }
                )
            })
            .collect();
        assert_eq!(exits.len(), 1, "{exits:?}");
        assert!(
            matches!(exits[0], JitDebugEvent::EnteredGenerationDeopt {
            callee_function_id, callee_code_object_id, exit_reason: ExitReason::Int32Overflow,
            exit_action: ExitAction::Recompile, ..
        } if *callee_function_id == generations[2].function_id && *callee_code_object_id == generations[2].code_object_id),
            "{exits:?}"
        );
        let trace = fixture.trace.lock().unwrap();
        assert_eq!(trace.dropped, 0);
        assert!(
            trace
                .ticks
                .iter()
                .any(|(fid, _, op)| *fid == generations[2].function_id && *op == Op::AddImm)
        );
        for (name, generation) in names[..2].iter().zip(&generations) {
            assert!(
                !trace
                    .ticks
                    .iter()
                    .any(|(fid, _, _)| *fid == generation.function_id)
            );
            let after = fixture.current(name).unwrap();
            assert_eq!(after.code_object_id, generation.code_object_id);
            assert_eq!(after.generated_deopts, generation.generated_deopts);
            assert_eq!(after.active_count, 0);
        }
        drop(trace);
        let after = fixture.runtime.execution_stats();
        assert_eq!(after.jit_optimized_deopts - before.jit_optimized_deopts, 1);
        fixture.runtime.force_gc().unwrap();
        let result = fixture.run(
            "JSON.stringify([tailDeoptBridge(7, tailDeoptPayload), tailDeoptPayload.calls]);",
            "tail-deopt-reuse.js",
        );
        assert_eq!(result.completion_string(), "[8,2]");
        for (name, generation) in names[..2].iter().zip(&generations) {
            let after = fixture.current(name).unwrap();
            assert_eq!(after.code_object_id, generation.code_object_id);
            assert_eq!(after.generated_deopts, generation.generated_deopts);
            assert_eq!(after.active_count, 0);
        }
    }
}
