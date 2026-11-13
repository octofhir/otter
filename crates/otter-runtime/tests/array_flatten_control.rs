//! Controllable flattening through the public Array runtime boundary.
//!
//! # Contents
//! - Shared live property/species/rooting protocol in every execution policy.
//! - Budget rejection over top-level and nested holes without JS callbacks.
//! - Catchable depth exhaustion and runtime reuse after cyclic input.
//!
//! # Invariants
//! - Safety getters bound a broken walk independently of budget enforcement.
//! - Native work rejection bypasses source catch handlers and later effects.
//! - Cycles never consume the host's Rust stack.
//!
//! # See also
//! - `otter-difftest/corpus/array_flatten_protocol.js` owns shared JS cases.
//! - `otter-vm/src/array_prototype/flatten.rs` owns the iterative walk.

use otter_runtime::{
    JitSelection, OtterError, Runtime, SourceInput, WorkBudget, WorkBudgetExceededAction,
};

const SELECTIONS: [JitSelection; 3] = [
    JitSelection::InterpreterOnly,
    JitSelection::Template,
    JitSelection::ProductionTiered,
];

#[test]
fn flattening_keeps_live_depth_first_protocol_and_roots() {
    for selection in SELECTIONS {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        let result = runtime
            .run_script(
                SourceInput::from_javascript(include_str!(
                    "../../otter-difftest/corpus/array_flatten_protocol.js"
                )),
                "flatten-protocol.js",
            )
            .unwrap_or_else(|error| panic!("{selection:?}: {error:?}"));
        assert_eq!(result.completion_string(), "ok:flatten-protocol");
    }
}

#[test]
fn top_level_and_nested_holes_exhaust_work_without_callbacks() {
    for selection in SELECTIONS {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .work_budget(WorkBudget {
                on_exceeded: WorkBudgetExceededAction::Reject,
                max_work_units_per_turn: Some(1_000),
                ..WorkBudget::default()
            })
            .build()
            .unwrap();
        for method in ["flat", "flatMap"] {
            for nested in [false, true] {
                let receiver = if nested {
                    "const input = []; input.length = 2 ** 26;"
                } else {
                    "const input = {length: Number.MAX_SAFE_INTEGER};"
                };
                runtime.run_script(SourceInput::from_javascript(format!(
                    "{{ {receiver} Object.defineProperty(input, '10000', {{get(){{throw 'safety sentinel';}}}}); globalThis.flattenInput = {}; }} globalThis.flattenCaught = false; globalThis.flattenAfter = false; function flattenIdentity(value){{return value;}}",
                    if nested { "[input]" } else { "input" }
                )), "flatten-input.js").unwrap();
                let before = runtime.work_budget_stats().budget_rejections;
                let source = format!(
                    "try {{ Array.prototype.{method}.call(flattenInput, {}); }} catch(error) {{ flattenCaught = true; }} flattenAfter = true;",
                    if method == "flat" {
                        "Infinity"
                    } else {
                        "flattenIdentity"
                    }
                );
                let error = runtime
                    .run_script(
                        SourceInput::from_javascript(source),
                        "flatten-hole-budget.js",
                    )
                    .expect_err("every flatten index attempt must consume work");
                match error {
                    OtterError::Runtime { diagnostic } => {
                        assert_eq!(diagnostic.code, "BUDGET_EXCEEDED")
                    }
                    error => panic!("{selection:?} {method} nested={nested}: {error:?}"),
                }
                assert_eq!(runtime.work_budget_stats().budget_rejections, before + 1);
                runtime.force_gc().unwrap();
                assert_eq!(
                    runtime
                        .eval(SourceInput::from_javascript(
                            "JSON.stringify([flattenCaught, flattenAfter])"
                        ))
                        .unwrap()
                        .completion_string(),
                    "[false,false]"
                );
            }
        }
    }
}

#[test]
fn cyclic_depth_exhaustion_is_catchable_and_the_runtime_remains_reusable() {
    for selection in SELECTIONS {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        let result = runtime
            .run_script(
                SourceInput::from_javascript(
                    r#"
const cycle = []; cycle[0] = cycle;
let failures = 0;
for (let attempt = 0; attempt < 3; attempt++) {
    try { cycle.flat(Infinity); }
    catch(error) { if (!(error instanceof RangeError)) throw error; failures++; }
}

let boundary = 719;
for (let depth = 0; depth < 512; depth++) boundary = [boundary];
if (boundary.flat(Infinity)[0] !== 719) throw new Error('supported deep input');
try { [boundary].flat(Infinity); throw new Error('missing depth bound'); }
catch(error) { if (!(error instanceof RangeError)) throw error; }
if (failures !== 3 || [1, [2]].flat().join(',') !== '1,2') throw new Error('flatten cleanup');
failures;
"#,
                ),
                "flatten-depth.js",
            )
            .unwrap_or_else(|error| panic!("{selection:?}: {error:?}"));
        assert_eq!(result.completion_string(), "3");
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

#[test]
fn yielding_hole_walks_keep_the_turn_and_pending_microtask_order() {
    for selection in SELECTIONS {
        for method in ["flat", "flatMap"] {
            let mut runtime = Runtime::builder()
                .jit_selection(selection)
                .work_budget(WorkBudget {
                    on_exceeded: WorkBudgetExceededAction::Yield,
                    max_work_units_per_turn: Some(128),
                    ..WorkBudget::default()
                })
                .build()
                .unwrap();
            runtime.run_script(SourceInput::from_javascript(
                "globalThis.flattenYieldLog = []; globalThis.flattenYieldInput = {length: 5000, 4999: 719}; function flattenYieldIdentity(value){return value;}"
            ), "flatten-yield-input.js").unwrap();
            let before = runtime.work_budget_stats().forced_yields;
            let source = format!(
                "queueMicrotask(() => flattenYieldLog.push('microtask')); const result = Array.prototype.{method}.call(flattenYieldInput, {}); flattenYieldLog.push(result[0], 'after'); result.length;",
                if method == "flat" {
                    "1"
                } else {
                    "flattenYieldIdentity"
                }
            );
            let result = runtime
                .run_script(SourceInput::from_javascript(source), "flatten-yield.js")
                .unwrap();
            assert_eq!(result.completion_string(), "1");
            assert!(
                runtime.work_budget_stats().forced_yields >= before + 30,
                "hole attempts must rotate the owning slice"
            );
            assert_eq!(runtime.work_budget_stats().budget_rejections, 0);
            assert_eq!(
                runtime
                    .eval(SourceInput::from_javascript(
                        "JSON.stringify(flattenYieldLog)"
                    ))
                    .unwrap()
                    .completion_string(),
                "[719,\"after\",\"microtask\"]"
            );
        }
    }
}
