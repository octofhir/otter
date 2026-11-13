//! Full FindViaPredicate ranges through the public runtime boundary.
//!
//! # Contents
//! - Large forward/reverse ranges, ordered Get, mutation and abrupt completion.
//! - Bounded Find/reduce work and cancellation with native and admitted JS predicates.
//!
//! # Invariants
//! - A large length cannot introduce enumeration or truncate absent indices.
//! - Budget rejection cannot be caught by JavaScript or retire the predicate.
//! - Every safety sentinel bounds a broken walk independently of its budget.
//! - Compiled predicates retain their exact installed generation.
//!
//! # See also
//! - `otter-difftest/corpus/array_find_large_range.js` owns the shared JS contract.
//! - `otter-vm/src/array_prototype.rs` owns the ordered callback driver.

use std::collections::BTreeSet;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicUsize, Ordering},
};

use otter_runtime::inspect::{StepEvent, StepTracer};
use otter_runtime::{
    InterruptHandle, JitDebugEvent, JitDebugRequest, JitSelection, OtterError, Runtime,
    RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeValue, SourceInput, WorkBudget,
    WorkBudgetExceededAction,
};
use otter_vm::JitCodeGenerationSnapshot;
use otter_vm::native_abi::{CodeLifetimeState, NativeFrameKind};

const SELECTIONS: [JitSelection; 3] = [
    JitSelection::InterpreterOnly,
    JitSelection::Template,
    JitSelection::ProductionTiered,
];
const BUDGET: u64 = 10_000;
const PREDICATE: &str = "findRangeNever";

#[derive(Default)]
struct TraceCounts {
    steps: AtomicUsize,
    predicate_steps: AtomicUsize,
}

struct Trace(Arc<TraceCounts>);

impl StepTracer for Trace {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        self.0.steps.fetch_add(1, Ordering::Relaxed);
        if event.function_name == PREDICATE {
            self.0.predicate_steps.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn warm_predicate(
    runtime: &mut Runtime,
    selection: JitSelection,
) -> Option<JitCodeGenerationSnapshot> {
    runtime
        .run_script(
            SourceInput::from_javascript("let findRangeVisits = 0; function findRangeNever() { findRangeVisits++; return false; }"),
            "find-range-predicate.js",
        )
        .unwrap();
    let mut code_ids = BTreeSet::new();
    for _ in 0..64 {
        let result = runtime
            .run_script(
                SourceInput::from_javascript("for (let n = 0; n < 256; n++) findRangeNever();"),
                "find-range-warm.js",
            )
            .unwrap();
        for bundle in result.jit_artifacts().unwrap().bundles() {
            if bundle.manifest().function_name() == PREDICATE
                && bundle.manifest().module() == "find-range-predicate.js"
            {
                code_ids.insert(bundle.manifest().code_object_id());
            }
        }
    }
    let generations = runtime.jit_code_generation_snapshot();
    if selection == JitSelection::InterpreterOnly {
        assert!(generations.is_empty());
        return None;
    }
    let proof = generations
        .into_iter()
        .find(|g| {
            g.current_entry
                && g.linked
                && g.lifecycle == CodeLifetimeState::Installed
                && code_ids.contains(&g.code_object_id)
        })
        .expect("ordinary warmup must admit this exact predicate");
    let expected_tier = match selection {
        JitSelection::Template => NativeFrameKind::Baseline,
        JitSelection::ProductionTiered => NativeFrameKind::Optimizing,
        JitSelection::InterpreterOnly => unreachable!(),
    };
    assert_eq!(
        proof.tier, expected_tier,
        "the requested normal tier must admit its own body"
    );
    assert!(
        proof.call_entry_offset.is_some(),
        "the generation retains its private JS entry"
    );
    Some(proof)
}

fn assert_predicate(runtime: &Runtime, before: &JitCodeGenerationSnapshot, max_entries: u64) {
    let generations = runtime.jit_code_generation_snapshot();
    let after = generations
        .iter()
        .find(|g| g.code_object_id == before.code_object_id)
        .unwrap();
    assert!(after.current_entry && after.linked);
    assert_eq!(after.lifecycle, CodeLifetimeState::Installed);
    assert_eq!(after.function_id, before.function_id);
    assert_eq!(after.tier, before.tier);
    assert_eq!(after.call_entry_offset, before.call_entry_offset);
    assert_eq!(after.active_count, 0);
    assert_eq!(after.generated_deopts, before.generated_deopts);
    let entries = after.generated_entries - before.generated_entries;
    assert!(
        entries <= max_entries,
        "the driver must stop before its safety sentinel: {entries}"
    );
}

#[test]
fn large_ranges_keep_order_live_get_and_early_completion_in_every_tier() {
    let source = include_str!("../../otter-difftest/corpus/array_find_large_range.js");
    let outcomes = SELECTIONS.map(|selection| {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        (
            selection,
            runtime
                .run_script(SourceInput::from_javascript(source), "large-find-range.js")
                .map(|r| r.completion_string().to_owned()),
        )
    });
    for (selection, outcome) in outcomes {
        assert_eq!(
            outcome.unwrap_or_else(|e| panic!("{selection:?}: {e:?}")),
            "ok:large-find-range"
        );
    }
}

#[test]
fn large_walks_reject_work_with_native_and_current_compiled_predicates() {
    for selection in SELECTIONS {
        for callback in ["Boolean", PREDICATE] {
            for method in ["findLast", "reduce", "reduceRight"] {
                let mut runtime = Runtime::builder()
                    .jit_selection(selection)
                    .jit_compile_threads(0)
                    .jit_debug(JitDebugRequest::artifacts().with_events(true))
                    .work_budget(WorkBudget {
                        on_exceeded: WorkBudgetExceededAction::Reject,
                        max_work_units_per_turn: Some(BUDGET),
                        ..WorkBudget::default()
                    })
                    .build()
                    .unwrap();
                let proof = if callback == PREDICATE {
                    warm_predicate(&mut runtime, selection)
                } else {
                    None
                };
                runtime
                    .run_script(
                        SourceInput::from_javascript(
                            r#"
globalThis.findRangeCaught = false; globalThis.findRangeAfter = false;
const receiver = {length: 2 ** 26, 0: undefined, [2 ** 26 - 1]: undefined};
Object.defineProperty(receiver, '20000', {get() { throw 'forward safety sentinel'; }});
Object.defineProperty(receiver, String(2 ** 26 - 20_001), {get() { throw 'safety sentinel'; }});
"#,
                        ),
                        "find-range-input.js",
                    )
                    .unwrap();
                if callback == PREDICATE {
                    runtime
                        .eval(SourceInput::from_javascript("findRangeVisits = 0"))
                        .unwrap();
                }
                let trace = Arc::new(TraceCounts::default());
                runtime.set_tracer(Some(Box::new(Trace(trace.clone()))));
                let source = format!(
                    r#"
try {{ Array.prototype.{method}.call(receiver, {callback}, 0); }} catch (error) {{ findRangeCaught = true; }}
findRangeAfter = true;
"#
                );
                let error = runtime
                    .run_script(SourceInput::from_javascript(source), "find-range-budget.js")
                    .expect_err("a large walk must exhaust its configured budget");
                match error {
                    OtterError::Runtime { diagnostic } => {
                        assert_eq!(diagnostic.code, "BUDGET_EXCEEDED")
                    }
                    other => panic!("{selection:?} {method} {callback}: {other:?}"),
                }
                assert_eq!(runtime.work_budget_stats().budget_rejections, 1);
                let report = runtime.take_jit_debug_report().unwrap();
                assert!(!report.truncated());
                assert_eq!(report.dropped_events(), 0);
                assert!(
                    !report.events().iter().any(|event| matches!(
                        event,
                        JitDebugEvent::Bail { .. }
                            | JitDebugEvent::EnteredGenerationDeopt { .. }
                            | JitDebugEvent::InlineDeoptFrame { .. }
                    )),
                    "the predicate must not deopt: {:?}",
                    report.events()
                );
                if callback == PREDICATE && selection == JitSelection::InterpreterOnly {
                    assert!(
                        trace.predicate_steps.load(Ordering::Relaxed) > 0,
                        "the oracle validates the callback's tracer identity"
                    );
                }
                if let Some(proof) = proof {
                    assert_predicate(&runtime, &proof, BUDGET);
                    assert!(trace.steps.load(Ordering::Relaxed) > 0);
                    assert_eq!(
                        trace.predicate_steps.load(Ordering::Relaxed),
                        0,
                        "the subject must not fall back to interpreter execution"
                    );
                    let visits = runtime
                        .eval(SourceInput::from_javascript("findRangeVisits"))
                        .unwrap()
                        .completion_string()
                        .parse::<u64>()
                        .unwrap();
                    assert!(
                        visits > 0 && visits <= BUDGET,
                        "bounded native predicate effects: {visits}"
                    );
                    // Graph omits generated-entry counters. Its exact retained
                    // generation, observable effects, no interpreter steps and no
                    // deopt events jointly prove this actual native execution.
                    if proof.tier == NativeFrameKind::Baseline {
                        let after = runtime
                            .jit_code_generation_snapshot()
                            .into_iter()
                            .find(|g| g.code_object_id == proof.code_object_id)
                            .unwrap();
                        assert_eq!(after.generated_entries - proof.generated_entries, visits);
                    }
                }
                runtime.force_gc().unwrap();
                let result = runtime
                    .eval(SourceInput::from_javascript(
                        "JSON.stringify([findRangeCaught, findRangeAfter])",
                    ))
                    .unwrap();
                assert_eq!(result.completion_string(), "[false,false]");
            }
        }
    }
}

#[test]
fn yielding_a_large_native_walk_preserves_the_turn_and_exact_early_index() {
    for selection in SELECTIONS {
        let calls = Arc::new(AtomicUsize::new(0));
        let installer = {
            let calls = calls.clone();
            RuntimeExtensionInstaller::new(move |realm| {
                let calls = calls.clone();
                realm.install_native_global_call(
                    "stopAfterSeventeen",
                    0,
                    RuntimeNativeCall::Dynamic(Arc::new(move |_ctx, _args, _capture| {
                        Ok(RuntimeValue::boolean(
                            calls.fetch_add(1, Ordering::Relaxed) == 16,
                        ))
                    })),
                )
            })
        };
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .extension_installer(installer)
            .work_budget(WorkBudget {
                on_exceeded: WorkBudgetExceededAction::Yield,
                max_work_units_per_turn: Some(32),
                ..WorkBudget::default()
            })
            .build()
            .unwrap();
        let result = runtime.run_script(SourceInput::from_javascript(r#"
var findYieldLog = [];
queueMicrotask(() => findYieldLog.push('microtask'));
var index = Array.prototype.findLastIndex.call({length: Number.MAX_SAFE_INTEGER}, stopAfterSeventeen);
findYieldLog.push(index, 'after');
index;
"#), "find-range-yield.js").unwrap();
        assert_eq!(result.completion_string(), "9007199254740974");
        assert_eq!(calls.load(Ordering::Relaxed), 17);
        let stats = runtime.work_budget_stats();
        assert!(stats.forced_yields > 0);
        assert_eq!(stats.budget_rejections, 0);
        assert_eq!(
            runtime
                .eval(SourceInput::from_javascript("JSON.stringify(findYieldLog)"))
                .unwrap()
                .completion_string(),
            "[9007199254740974,\"after\",\"microtask\"]"
        );
    }
}

#[derive(Default)]
struct Cancellation {
    interrupt: OnceLock<InterruptHandle>,
    calls: AtomicUsize,
}

#[test]
fn cancellation_stops_native_and_compiled_predicate_walks_before_a_second_entry() {
    for selection in SELECTIONS {
        for compiled in [false, true] {
            let state = Arc::new(Cancellation::default());
            let installer = {
                let state = state.clone();
                RuntimeExtensionInstaller::new(move |realm| {
                    let state = state.clone();
                    realm.install_native_global_call(
                        "interruptFind",
                        0,
                        RuntimeNativeCall::Dynamic(Arc::new(move |_ctx, _args, _capture| {
                            state.calls.fetch_add(1, Ordering::Relaxed);
                            state.interrupt.get().unwrap().interrupt();
                            Ok(RuntimeValue::boolean(false))
                        })),
                    )
                })
            };
            let mut runtime = Runtime::builder()
                .jit_selection(selection)
                .jit_compile_threads(0)
                .jit_debug(JitDebugRequest::artifacts().with_events(true))
                .extension_installer(installer)
                .build()
                .unwrap();
            state.interrupt.set(runtime.interrupt_handle()).unwrap();
            let proof = if compiled {
                warm_predicate(&mut runtime, selection)
            } else {
                None
            };
            if compiled {
                runtime
                    .eval(SourceInput::from_javascript("findRangeVisits = 0"))
                    .unwrap();
            }
            let callback = if compiled { PREDICATE } else { "interruptFind" };
            let getter = if compiled {
                "Object.defineProperty(receiver, String(2 ** 26 - 1), {get: interruptFind});"
            } else {
                ""
            };
            let source = format!(
                r#"
const receiver = {{length: 2 ** 26}};
Object.defineProperty(receiver, String(2 ** 26 - 20_001), {{get() {{ throw 'safety sentinel'; }}}});
{getter}
Array.prototype.findLast.call(receiver, {callback});
"#
            );
            let error = runtime
                .run_script(SourceInput::from_javascript(source), "find-range-cancel.js")
                .expect_err("the native walk must observe cancellation");
            assert!(matches!(error, OtterError::Interrupted), "{error:?}");
            assert_eq!(state.calls.load(Ordering::Relaxed), 1);
            if let Some(proof) = proof {
                assert_predicate(&runtime, &proof, 1);
            }
            runtime.interrupt_handle().reset();
            if compiled {
                let visits = runtime
                    .eval(SourceInput::from_javascript("findRangeVisits"))
                    .unwrap()
                    .completion_string()
                    .parse::<u64>()
                    .unwrap();
                assert!(
                    visits <= 1,
                    "cancellation must stop native callback effects: {visits}"
                );
            }
            runtime.force_gc().unwrap();
            assert_eq!(
                runtime
                    .eval(SourceInput::from_javascript("6 * 7"))
                    .unwrap()
                    .completion_string(),
                "42"
            );
        }
    }
}
