//! Native generations unrelated to reclaimed source retain their exact entries.
//!
//! # Contents
//! - Normal-policy admission of a caller and its own-arguments callee.
//! - Cold eval reclamation followed by a traced generated call and full GC reuse.
//!
//! # Invariants
//! - Background compilation remains enabled and all pending ACKs settle before
//!   the measured reclamation; compiler source leases cannot mask the result.
//! - Exact current Known linkage and absent subject interpreter dispatch prove
//!   execution, independently of aggregate generation counters.
//!
//! # See also
//! - `otter_vm::interp::exec` owns between-turn source reclamation.

use otter_runtime::{
    JitArtifactBundle, JitDebugEvent, JitDebugRequest, JitSelection, Runtime, SourceInput,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::{
    JitCodeGenerationSnapshot,
    native_abi::{CodeLifetimeState, NativeFrameKind},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[path = "../support/return_sites.rs"]
mod return_sites;

#[derive(Default)]
struct Trace {
    recording: bool,
    ids: Vec<u32>,
}
struct Tracer(Arc<Mutex<Trace>>);
impl StepTracer for Tracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        let mut trace = self.0.lock().unwrap();
        if trace.recording {
            assert!(trace.ids.len() < 16_384, "complete bounded probe trace");
            trace.ids.push(event.function_id);
        }
    }
}

fn current(
    runtime: &Runtime,
    artifacts: &BTreeMap<u64, JitArtifactBundle>,
    name: &str,
) -> Option<JitCodeGenerationSnapshot> {
    runtime
        .jit_code_generation_snapshot()
        .into_iter()
        .find(|generation| {
            generation.lifecycle == CodeLifetimeState::Installed
                && generation.linked
                && generation.current_entry
                && artifacts
                    .get(&generation.code_object_id)
                    .is_some_and(|bundle| bundle.manifest().function_name() == name)
        })
}

#[test]
fn reclaiming_cold_eval_preserves_unrelated_current_generated_calls() {
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .build()
            .unwrap();
        let trace = Arc::new(Mutex::new(Trace::default()));
        runtime.set_tracer(Some(Box::new(Tracer(trace.clone()))));
        let mut artifacts = BTreeMap::new();
        let definitions = r#"
function retainedLeaf(value) {
  if (arguments === value) throw "own arguments";
  return value + 1;
}
function retainedCaller(fn, value) {
  if (arguments === value) throw "own caller arguments";
  const result = fn(value); return result + 1;
}
for (let warm = 0; warm < 1024; warm++) {
  retainedLeaf(warm & 31); retainedCaller(retainedLeaf, warm & 31);
}
"#;
        let names = ["retainedCaller", "retainedLeaf"];
        let expected_tier = if selection == JitSelection::Template {
            NativeFrameKind::Baseline
        } else {
            NativeFrameKind::Optimizing
        };
        for batch in 0..=128 {
            let source = if batch == 0 {
                definitions.to_owned()
            } else {
                "retainedCaller(retainedLeaf, 7); retainedLeaf(7);\n".repeat(32)
            };
            let result = runtime
                .run_script(
                    SourceInput::from_javascript(source),
                    &format!("retained-native-{batch}.js"),
                )
                .unwrap();
            let captured = result.jit_artifacts().unwrap();
            assert!(!captured.truncated());
            for bundle in captured.bundles() {
                artifacts.insert(bundle.manifest().code_object_id(), bundle.clone());
            }
            if runtime.jit_code_residency().pending_compilations == 0
                && names.iter().all(|name| {
                    current(&runtime, &artifacts, name)
                        .is_some_and(|code| code.tier == expected_tier)
                })
            {
                break;
            }
            assert!(batch < 128, "normal-policy native admission");
        }
        let before: Vec<_> = names
            .iter()
            .map(|name| current(&runtime, &artifacts, name).unwrap())
            .collect();
        return_sites::assert_known(&artifacts[&before[0].code_object_id], before[1].function_id);
        runtime
            .run_script(
                SourceInput::from_javascript(
                    "eval('(function coldDead() { return 7; })'); undefined;",
                ),
                "dead-eval.js",
            )
            .unwrap();
        assert_eq!(runtime.jit_code_residency().pending_compilations, 0);
        runtime.set_code_eviction_high_water_bytes(0);
        let old_evicted = runtime.code_eviction_stats().evicted_chunks;
        runtime.reclaim_dynamic_code().unwrap();
        assert_eq!(
            runtime.code_eviction_stats().evicted_chunks,
            old_evicted + 1
        );
        assert_eq!(runtime.code_eviction_stats().retained_bytes, 0);
        for repetition in 0..2 {
            if repetition != 0 {
                runtime.force_gc().unwrap();
            }
            for (name, original) in names.iter().zip(&before) {
                let after = current(&runtime, &artifacts, name).unwrap_or_else(|| {
                    panic!(
                        "{selection:?}: reclamation retired unrelated {name}: {:?}",
                        runtime.jit_code_generation_snapshot()
                    )
                });
                assert_eq!(after.code_object_id, original.code_object_id);
                assert_eq!(after.generated_deopts, original.generated_deopts);
                assert_eq!(after.active_count, 0);
            }
            {
                let mut trace = trace.lock().unwrap();
                trace.ids.clear();
                trace.recording = true;
            }
            let result = runtime
                .run_script(
                    SourceInput::from_javascript("retainedCaller(retainedLeaf, 7);"),
                    "retained-probe.js",
                )
                .unwrap();
            trace.lock().unwrap().recording = false;
            assert_eq!(result.completion_string(), "9");
            let report = result.jit_debug_report().unwrap();
            assert!(!report.truncated());
            assert_eq!(report.dropped_events(), 0);
            assert!(
                !report.events().iter().any(|event| matches!(
                    event,
                    JitDebugEvent::CompilePrepared { .. }
                        | JitDebugEvent::Bail { .. }
                        | JitDebugEvent::EnteredGenerationDeopt { .. }
                        | JitDebugEvent::InlineDeoptFrame { .. }
                )),
                "{:?}",
                report.events()
            );
            let trace = trace.lock().unwrap();
            assert!(!trace.ids.is_empty());
            for generation in &before {
                assert!(
                    !trace.ids.contains(&generation.function_id),
                    "retained native body must not dispatch interpreted instructions"
                );
            }
        }
    }
}
