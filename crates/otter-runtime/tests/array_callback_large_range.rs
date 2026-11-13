//! Complete ordered ranges for Array callback methods that skip holes.
//!
//! # Contents
//! - Shared differential source covering live HasProperty/Get and species.
//! - Work limits for native walks whose holes invoke no JavaScript callback.
//! - Cancellation from HasProperty before a second observable index attempt.
//!
//! # Invariants
//! - Large lengths never permit preliminary key or prototype enumeration.
//! - Every attempted index, including an absent one, remains controllable.
//! - Abrupt walks release roots and leave the runtime reusable.
//!
//! # See also
//! - `otter-difftest/corpus/array_callback_large_range.js` owns the JS contract.
//! - `array_find_large_range` proves normally admitted native predicate work.

use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicUsize, Ordering},
};

use otter_runtime::{
    InterruptHandle, JitSelection, OtterError, Runtime, RuntimeExtensionInstaller,
    RuntimeNativeCall, RuntimeNativeError, RuntimeValue, SourceInput, WorkBudget,
    WorkBudgetExceededAction,
};

const METHODS: [&str; 8] = [
    "some",
    "every",
    "forEach",
    "map",
    "filter",
    "reduce",
    "reduceRight",
    "flatMap",
];
const SELECTIONS: [JitSelection; 3] = [
    JitSelection::InterpreterOnly,
    JitSelection::Template,
    JitSelection::ProductionTiered,
];

#[test]
fn large_callbacks_observe_live_order_species_and_abrupt_completion() {
    for selection in SELECTIONS {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        let result = runtime
            .run_script(
                SourceInput::from_javascript(include_str!(
                    "../../otter-difftest/corpus/array_callback_large_range.js"
                )),
                "callback-large-range.js",
            )
            .unwrap_or_else(|error| panic!("{selection:?}: {error:?}"));
        assert_eq!(result.completion_string(), "ok:callback-large-range");
    }
}

#[test]
fn absent_indices_exhaust_the_budget_without_entering_a_callback() {
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
        for name in METHODS {
            // Map's explicit species allows the full ToLength range without
            // asking the default Array constructor for an invalid length.
            let receiver = if name == "map" {
                "new Proxy([], {get(t,k) { if(k === 'length') return Number.MAX_SAFE_INTEGER; if(k === 'constructor') return {[Symbol.species]: function(){return {};}}; }, has(t,k){if(k === '10000') throw 'safety sentinel'; return false;}})"
            } else {
                "{length: Number.MAX_SAFE_INTEGER}"
            };
            let sentinel = if name == "reduceRight" {
                "Number.MAX_SAFE_INTEGER - 10001"
            } else {
                "10000"
            };
            runtime
                .run_script(
                    SourceInput::from_javascript(format!(
                        "globalThis.callbackRangeInput = {receiver}; Object.defineProperty(callbackRangeInput, String({sentinel}), {{get(){{throw 'safety sentinel';}}}}); globalThis.callbackRangeCaught = false; globalThis.callbackRangeAfter = false;"
                    )),
                    "callback-hole-input.js",
                )
                .unwrap();
            let rejections = runtime.work_budget_stats().budget_rejections;
            let source = format!(
                "try {{ Array.prototype.{name}.call(callbackRangeInput, Boolean, 0); }} catch(error) {{ callbackRangeCaught = true; }} callbackRangeAfter = true;"
            );
            let error = runtime
                .run_script(
                    SourceInput::from_javascript(source),
                    "callback-hole-budget.js",
                )
                .expect_err("a hole still consumes one index attempt");
            match error {
                OtterError::Runtime { diagnostic } => {
                    assert_eq!(diagnostic.code, "BUDGET_EXCEEDED")
                }
                error => panic!("{selection:?} {name}: {error:?}"),
            }
            assert_eq!(
                runtime.work_budget_stats().budget_rejections,
                rejections + 1
            );
            runtime.force_gc().unwrap();
            assert_eq!(
                runtime
                    .eval(SourceInput::from_javascript(
                        "JSON.stringify([callbackRangeCaught, callbackRangeAfter])"
                    ))
                    .unwrap()
                    .completion_string(),
                "[false,false]",
                "{selection:?} {name}"
            );
        }
    }
}

#[derive(Default)]
struct Cancellation {
    interrupt: OnceLock<InterruptHandle>,
    attempts: AtomicUsize,
}

#[test]
fn has_property_cancellation_stops_before_the_next_index() {
    for selection in SELECTIONS {
        let state = Arc::new(Cancellation::default());
        let installer = {
            let state = state.clone();
            RuntimeExtensionInstaller::new(move |realm| {
                let state = state.clone();
                realm.install_native_global_call(
                    "interruptCallbackRange",
                    0,
                    RuntimeNativeCall::Dynamic(Arc::new(move |_ctx, _args, _capture| {
                        if state.attempts.fetch_add(1, Ordering::Relaxed) != 0 {
                            // Bound a broken interrupt implementation without
                            // relying on the flag this test is checking.
                            return Err(RuntimeNativeError::InvalidOperand);
                        }
                        state.interrupt.get().unwrap().interrupt();
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
        for name in METHODS {
            state.attempts.store(0, Ordering::Relaxed);
            let source = format!(
                "Array.prototype.{name}.call(new Proxy([], {{get(t,k) {{ if(k === 'length') return Number.MAX_SAFE_INTEGER; if(k === 'constructor') return {{[Symbol.species]: function(){{return {{}};}}}}; }}, has: interruptCallbackRange}}), Boolean, 0);"
            );
            let error = runtime
                .run_script(
                    SourceInput::from_javascript(source),
                    "callback-range-cancel.js",
                )
                .expect_err("a native range must observe cancellation");
            assert!(
                matches!(error, OtterError::Interrupted),
                "{selection:?} {name}: {error:?}"
            );
            assert_eq!(state.attempts.load(Ordering::Relaxed), 1, "{name}");
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
}
