//! Array fill range, numeric bulk stores and work control at the runtime boundary.
//!
//! # Contents
//! - Shared live Set/coercion protocol and exact abrupt completion.
//! - Full-range numeric stores and guarded bulk admission.
//! - Reject, yield and cancellation checks for generic and ordinary dense inputs.
//!
//! # Invariants
//! - Every generic Set attempt consumes work before its observable operation.
//! - Fatal rejection bypasses source catches and preserves committed stores.
//! - Yielding does not drain pending microtasks inside the fill operation.
//!
//! # See also
//! - `otter-vm/src/array_prototype/mutate.rs` owns the generic fill loop.

use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicUsize, Ordering},
};

use otter_runtime::{
    InterruptHandle, JitSelection, OtterError, Runtime, RuntimeExtensionInstaller,
    RuntimeNativeCall, RuntimeValue, SourceInput, WorkBudget, WorkBudgetExceededAction,
};

const SELECTIONS: [JitSelection; 3] = [
    JitSelection::InterpreterOnly,
    JitSelection::Template,
    JitSelection::ProductionTiered,
];

#[test]
fn generic_fill_keeps_order_bounds_values_and_abrupt_identity() {
    for selection in SELECTIONS {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        let result = runtime
            .run_script(
                SourceInput::from_javascript(include_str!(
                    "../../otter-difftest/corpus/array_fill_control.js"
                )),
                "fill-control.js",
            )
            .unwrap();
        assert_eq!(result.completion_string(), "ok:fill-control");
    }
}

#[test]
fn borrowed_numeric_fill_keeps_conversions_ranges_and_live_buffer_state() {
    for selection in SELECTIONS {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        let result = runtime
            .run_script(
                SourceInput::from_javascript(include_str!(
                    "../../otter-difftest/corpus/array_fill_numeric.js"
                )),
                "fill-numeric.js",
            )
            .unwrap();
        assert_eq!(
            result.completion_string(),
            "ok:fill-numeric",
            "{selection:?}"
        );
    }
}

#[test]
fn borrowed_fill_reaches_past_the_old_materialization_ceiling() {
    for selection in SELECTIONS {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        let result = runtime.run_script(SourceInput::from_javascript(
            "{ const input = new Uint8Array(2 ** 25 + 1); const result = Array.prototype.fill.call(input, 7); JSON.stringify([result === input, input[0], input[2 ** 25 - 1], input[2 ** 25], input.length]); }"
        ), "fill-full-range.js").unwrap();
        assert_eq!(
            result.completion_string(),
            "[true,7,7,7,33554433]",
            "{selection:?}"
        );
        assert!(result.stats().work_units_executed >= 33_554_433);
    }
}

#[test]
fn observing_dense_fill_accounts_for_the_full_span() {
    for selection in SELECTIONS {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        runtime.run_script(SourceInput::from_javascript(
            "globalThis.fillDenseInput = new Array(20000); globalThis.fillDenseValue = {text: '\\ud800fill\\udc00'};"
        ), "fill-dense-input.js").unwrap();
        let before = runtime.work_budget_stats().work_units_executed;
        let result = runtime.run_script(SourceInput::from_javascript(
            "Array.prototype.fill.call(fillDenseInput, fillDenseValue); fillDenseInput[0] === fillDenseValue && fillDenseInput[19999] === fillDenseValue;"
        ), "fill-dense-observe.js").unwrap();
        assert_eq!(result.completion_string(), "true", "{selection:?}");
        assert!(runtime.work_budget_stats().work_units_executed - before >= 20_000);
    }
}

#[test]
fn stores_reject_work_and_preserve_only_the_committed_prefix() {
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
        for input in [
            "{length: 20000}",
            "Object.setPrototypeOf(new Array(20000), Object.create(Array.prototype))",
            "new Array(20000)",
        ] {
            runtime.run_script(SourceInput::from_javascript(format!(
                "globalThis.fillControlInput = {input}; globalThis.fillControlValue = {{text: '\\ud800fill\\udc00'}}; globalThis.fillControlCaught = false; globalThis.fillControlAfter = false;"
            )), "fill-budget-input.js").unwrap();
            let before = runtime.work_budget_stats().budget_rejections;
            let error = runtime.run_script(SourceInput::from_javascript(
                "try { Array.prototype.fill.call(fillControlInput, fillControlValue); } catch(error) { fillControlCaught = true; } fillControlAfter = true;"
            ), "fill-budget.js").expect_err("each store attempt must consume work");
            match error {
                OtterError::Runtime { diagnostic } => {
                    assert_eq!(diagnostic.code, "BUDGET_EXCEEDED")
                }
                error => panic!("{selection:?}: {error:?}"),
            }
            assert_eq!(runtime.work_budget_stats().budget_rejections, before + 1);
            runtime.force_gc().unwrap();
            let result = runtime.eval(SourceInput::from_javascript(
                "{ const count = Object.keys(fillControlInput).length - (Array.isArray(fillControlInput) ? 0 : 1); JSON.stringify([fillControlCaught, fillControlAfter, count > 0 && count <= 1000, fillControlInput[0] === fillControlValue && fillControlInput[count - 1] === fillControlValue && !(count in fillControlInput), !(1000 in fillControlInput), fillControlInput.length]); }"
            )).unwrap();
            assert_eq!(
                result.completion_string(),
                "[false,false,true,true,true,20000]",
                "{selection:?}"
            );
        }
    }
}

#[test]
fn yielding_fill_keeps_the_turn_and_microtask_order() {
    for selection in SELECTIONS {
        for (input, value) in [
            ("{length: 5000}", "{marker: 719}"),
            ("new Array(5000)", "{marker: 719}"),
            ("new Uint8Array(5000)", "19"),
        ] {
            let mut runtime = Runtime::builder()
                .jit_selection(selection)
                .work_budget(WorkBudget {
                    on_exceeded: WorkBudgetExceededAction::Yield,
                    max_work_units_per_turn: Some(128),
                    ..WorkBudget::default()
                })
                .build()
                .unwrap();
            runtime.run_script(SourceInput::from_javascript(format!(
            "globalThis.fillYieldInput = {input}; globalThis.fillYieldValue = {value}; globalThis.fillYieldLog = [];"
        )), "fill-yield-input.js").unwrap();
            let before = runtime.work_budget_stats().forced_yields;
            let result = runtime.run_script(SourceInput::from_javascript(
            "queueMicrotask(() => fillYieldLog.push('microtask')); const result = Array.prototype.fill.call(fillYieldInput, fillYieldValue); fillYieldLog.push(result[0] === fillYieldValue, result[4999] === fillYieldValue, 'after'); result === fillYieldInput;"
        ), "fill-yield.js").unwrap();
            assert_eq!(result.completion_string(), "true");
            assert!(runtime.work_budget_stats().forced_yields >= before + 30);
            assert_eq!(runtime.work_budget_stats().budget_rejections, 0);
            assert_eq!(
                runtime
                    .eval(SourceInput::from_javascript("JSON.stringify(fillYieldLog)"))
                    .unwrap()
                    .completion_string(),
                "[true,true,\"after\",\"microtask\"]"
            );
        }
    }
}

#[test]
fn numeric_typed_stores_reject_at_the_committed_prefix() {
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
        runtime.run_script(SourceInput::from_javascript(
            "globalThis.fillNumbers = new Uint8Array(20000); globalThis.fillNumbersCaught = false; globalThis.fillNumbersAfter = false;"
        ), "fill-numbers-input.js").unwrap();
        let error = runtime.run_script(SourceInput::from_javascript(
            "try { Array.prototype.fill.call(fillNumbers, 19); } catch (error) { fillNumbersCaught = true; } fillNumbersAfter = true;"
        ), "fill-numbers-budget.js").expect_err("borrowed numeric fill must remain controlled");
        assert!(
            matches!(error, OtterError::Runtime { diagnostic } if diagnostic.code == "BUDGET_EXCEEDED")
        );
        runtime.force_gc().unwrap();
        let result = runtime.eval(SourceInput::from_javascript(
            "{ const prefix = fillNumbers.indexOf(0); JSON.stringify([prefix > 0 && prefix <= 1000, fillNumbers[prefix - 1] === 19, fillNumbers[1000], fillNumbers[19999], fillNumbersCaught, fillNumbersAfter]); }"
        )).unwrap();
        assert_eq!(
            result.completion_string(),
            "[true,true,0,0,false,false]",
            "{selection:?}"
        );
    }
}

#[derive(Default)]
struct Cancellation {
    interrupt: OnceLock<InterruptHandle>,
    calls: AtomicUsize,
}

#[test]
fn cancellation_in_a_native_setter_stops_before_the_next_store() {
    for selection in SELECTIONS {
        let state = Arc::new(Cancellation::default());
        let installer = {
            let state = state.clone();
            RuntimeExtensionInstaller::new(move |realm| {
                let state = state.clone();
                realm.install_native_global_call(
                    "interruptFill",
                    1,
                    RuntimeNativeCall::Dynamic(Arc::new(move |_ctx, _args, _capture| {
                        state.calls.fetch_add(1, Ordering::Relaxed);
                        state.interrupt.get().unwrap().interrupt();
                        Ok(RuntimeValue::undefined())
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
        runtime.run_script(SourceInput::from_javascript(
            "globalThis.fillCancelInput = {length: 20000}; Object.defineProperty(fillCancelInput, '0', {set: interruptFill}); globalThis.fillCancelAfter = false;"
        ), "fill-cancel-input.js").unwrap();
        let error = runtime
            .run_script(
                SourceInput::from_javascript(
                    "Array.prototype.fill.call(fillCancelInput, 719); fillCancelAfter = true;",
                ),
                "fill-cancel.js",
            )
            .expect_err("generic fill must observe native cancellation");
        assert!(
            matches!(error, OtterError::Interrupted),
            "{selection:?}: {error:?}"
        );
        assert_eq!(state.calls.load(Ordering::Relaxed), 1);
        runtime.interrupt_handle().reset();
        runtime.force_gc().unwrap();
        assert_eq!(runtime.eval(SourceInput::from_javascript(
            "JSON.stringify([!(1 in fillCancelInput), !(19999 in fillCancelInput), fillCancelAfter, 6 * 7])"
        )).unwrap().completion_string(), "[true,true,false,42]");
    }
}
