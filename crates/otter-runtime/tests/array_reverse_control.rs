//! Controllable complete reverse walks through the public runtime boundary.
//!
//! # Contents
//! - Shared live protocol and exact abrupt-completion source.
//! - Full-range completion beyond the former native probe ceiling.
//! - Reject, yield and cancellation checks for native pair attempts.
//!
//! # Invariants
//! - Absent indices still consume work before observable pair operations.
//! - Fatal rejection bypasses source catches and leaves the runtime reusable.
//! - Yield rotates the slice without draining microtasks inside reverse.
//!
//! # See also
//! - `otter-vm/src/array_prototype/mutate.rs` owns the reverse walk.

use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicUsize, Ordering},
};

use otter_runtime::{
    InterruptHandle, JitSelection, OtterError, Runtime, RuntimeExtensionInstaller,
    RuntimeNativeCall, RuntimeNativeError, RuntimeValue, SourceInput, WorkBudget,
    WorkBudgetExceededAction,
};

const SELECTIONS: [JitSelection; 3] = [
    JitSelection::InterpreterOnly,
    JitSelection::Template,
    JitSelection::ProductionTiered,
];

#[test]
fn reverse_preserves_live_order_and_exact_abrupt_completion() {
    for selection in SELECTIONS {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        let result = runtime
            .run_script(
                SourceInput::from_javascript(include_str!(
                    "../../otter-difftest/corpus/array_reverse_control.js"
                )),
                "reverse-control.js",
            )
            .unwrap();
        assert_eq!(result.completion_string(), "ok:reverse-control");
    }
}

#[test]
fn reverse_reaches_the_middle_pair_beyond_the_former_probe_limit() {
    for selection in SELECTIONS {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        let result = runtime.run_script(SourceInput::from_javascript(
            "const receiver = {length: 2 ** 25 + 2}; const sentinel = {}; Object.defineProperty(receiver, String(2 ** 24), {get(){throw sentinel;}}); let caught; try { Array.prototype.reverse.call(receiver); } catch(error) { caught = error; } caught === sentinel;"
        ), "reverse-full-range.js").unwrap();
        assert_eq!(result.completion_string(), "true", "{selection:?}");
    }
}

#[test]
fn reverse_holes_exhaust_work_without_running_javascript() {
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
        for receiver in ["{length: 20000}", "new Array(20000)"] {
            runtime.run_script(SourceInput::from_javascript(format!(
                "globalThis.reverseInput = {receiver}; globalThis.reverseCaught = false; globalThis.reverseAfter = false;"
            )), "reverse-budget-input.js").unwrap();
            let before = runtime.work_budget_stats().budget_rejections;
            let error = runtime.run_script(SourceInput::from_javascript(
                "try { Array.prototype.reverse.call(reverseInput); } catch(error) { reverseCaught = true; } reverseAfter = true;"
            ), "reverse-budget.js").expect_err("absent pairs must consume native work");
            match error {
                OtterError::Runtime { diagnostic } => {
                    assert_eq!(diagnostic.code, "BUDGET_EXCEEDED")
                }
                error => panic!("{selection:?}: {error:?}"),
            }
            assert_eq!(runtime.work_budget_stats().budget_rejections, before + 1);
            runtime.force_gc().unwrap();
            assert_eq!(
                runtime
                    .eval(SourceInput::from_javascript(
                        "JSON.stringify([reverseCaught, reverseAfter])"
                    ))
                    .unwrap()
                    .completion_string(),
                "[false,false]"
            );
        }
    }
}

#[test]
fn yielding_reverse_keeps_the_turn_and_pending_microtask_order() {
    for selection in SELECTIONS {
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
            "globalThis.reverseYieldLog = []; globalThis.reverseYieldInput = {length: 10000, 9999: 719};"
        ), "reverse-yield-input.js").unwrap();
        let before = runtime.work_budget_stats().forced_yields;
        let result = runtime.run_script(SourceInput::from_javascript(
            "queueMicrotask(() => reverseYieldLog.push('microtask')); const result = Array.prototype.reverse.call(reverseYieldInput); reverseYieldLog.push(result[0], 'after'); result === reverseYieldInput;"
        ), "reverse-yield.js").unwrap();
        assert_eq!(result.completion_string(), "true");
        assert!(runtime.work_budget_stats().forced_yields >= before + 30);
        assert_eq!(runtime.work_budget_stats().budget_rejections, 0);
        assert_eq!(
            runtime
                .eval(SourceInput::from_javascript(
                    "JSON.stringify(reverseYieldLog)"
                ))
                .unwrap()
                .completion_string(),
            "[719,\"after\",\"microtask\"]"
        );
    }
}

#[derive(Default)]
struct Cancellation {
    interrupt: OnceLock<InterruptHandle>,
    attempts: AtomicUsize,
}

#[test]
fn cancellation_after_an_absent_pair_stops_before_the_next_pair() {
    for selection in SELECTIONS {
        let state = Arc::new(Cancellation::default());
        let installer = {
            let state = state.clone();
            RuntimeExtensionInstaller::new(move |realm| {
                let state = state.clone();
                realm.install_native_global_call(
                    "reverseHas",
                    2,
                    RuntimeNativeCall::Dynamic(Arc::new(move |_ctx, _args, _capture| {
                        match state.attempts.fetch_add(1, Ordering::Relaxed) {
                            0 => {}
                            1 => state.interrupt.get().unwrap().interrupt(),
                            _ => return Err(RuntimeNativeError::InvalidOperand),
                        }
                        Ok(RuntimeValue::boolean(false))
                    })),
                )
            })
        };
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .extension_installer(installer)
            .build()
            .unwrap();
        state.interrupt.set(runtime.interrupt_handle()).unwrap();
        let error = runtime.run_script(SourceInput::from_javascript(
            "Array.prototype.reverse.call(new Proxy({length: Number.MAX_SAFE_INTEGER}, {has: reverseHas}));"
        ), "reverse-cancel.js").expect_err("reverse must poll between native pairs");
        assert!(
            matches!(error, OtterError::Interrupted),
            "{selection:?}: {error:?}"
        );
        assert_eq!(state.attempts.load(Ordering::Relaxed), 2);
        runtime.interrupt_handle().reset();
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
