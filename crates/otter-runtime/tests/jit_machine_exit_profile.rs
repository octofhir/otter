//! Optimized exits feed the next generation's speculation.
//!
//! # Contents
//! - A hot function whose int32 multiply overflows on most calls, so the
//!   int32-only operand feedback keeps describing a site whose result leaves
//!   the int32 range.
//!
//! # Invariants
//! - The exit PCs recorded by the first optimized generation widen the baked
//!   feedback of those instructions, so the rebuilt generation lowers the site
//!   as Float64 arithmetic and stops exiting; the function keeps its
//!   optimizing generation instead of being abandoned.
//! - Every tier returns the interpreter's completion.

use otter_runtime::{JitDebugEvent, JitDebugRequest, JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
// Both operands are always int32; only the product leaves the int32 range,
// which operand feedback cannot observe. The loop keeps `grow` out of its
// caller's inline tree so the exit is attributed to `grow` itself.
function grow(seed) {
  let v = 0;
  for (let k = 0; k < 2; k++) v = (seed + k) * 100003;
  return v;
}
function drive(rounds) {
  let acc = 0;
  for (let i = 0; i < rounds; i++) acc += grow(i + 30000);
  return acc;
}
"#;

fn run(
    selection: JitSelection,
) -> (
    String,
    otter_runtime::RuntimeExecutionStats,
    Vec<JitDebugEvent>,
) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_debug(JitDebugRequest::events())
        .build()
        .expect("runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(SOURCE.to_string()),
            "jit-machine-exit-profile.js",
        )
        .expect("install overflow workload");
    let mut completion = String::new();
    let mut events = Vec::new();
    // Returning to the VM between activations lets the cold policy consume
    // the generated-entry mailbox without adding a poll to the success path.
    for round in 0..3 {
        let result = runtime
            .run_script(
                SourceInput::from_javascript("drive(30000);".to_string()),
                &format!("jit-machine-exit-profile-{round}.js"),
            )
            .expect("run overflow workload");
        completion = result.completion_string().to_owned();
        events.extend(
            result
                .jit_debug_report()
                .expect("events enabled")
                .events()
                .iter()
                .cloned(),
        );
    }
    (completion, runtime.execution_stats(), events)
}

#[test]
fn an_overflowing_int32_site_exits_once_per_budget_and_then_stays_float() {
    let (oracle, _, _) = run(JitSelection::InterpreterOnly);
    let (compiled, stats, events) = run(JitSelection::ProductionTiered);
    let diagnostic_events = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                JitDebugEvent::CompilePrepared { .. }
                    | JitDebugEvent::CompileFinished { .. }
                    | JitDebugEvent::Bail { .. }
                    | JitDebugEvent::GeneratedCallDeopt { .. }
            )
        })
        .take(30)
        .collect::<Vec<_>>();
    assert_eq!(compiled, oracle);
    assert!(
        stats.jit_optimized_entries + stats.jit_generated_optimizing_entries > 1000,
        "the function must keep an optimizing generation: {stats:?}; events={diagnostic_events:?}"
    );
    // The first generated entry generation can exit once on overflow.
    // Widening must prevent any later callee generation from repeating it;
    // caller identity invalidation is an independent exit profile.
    assert!(
        stats.jit_generated_call_deopts <= 1,
        "the rebuilt generation must not repeat the exited speculation: {stats:?}; events={diagnostic_events:?}"
    );
}
