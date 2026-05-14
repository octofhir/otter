//! Runtime budget DTOs and VM resource counters are visible through
//! the direct runtime surface.
//!
//! This covers the first observational slice only: configured limits record
//! exceedance observations without preempting or rejecting execution.

use otter_runtime::{OtterError, Runtime, RuntimeBudget, RuntimeBudgetExceededAction, SourceInput};

#[test]
fn runtime_budget_stats_are_visible_after_script_run() {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.set_runtime_budget(RuntimeBudget {
        max_reductions_per_turn: Some(1),
        ..RuntimeBudget::default()
    });

    rt.run_script(
        SourceInput::from_javascript("function f(x) { return x + 1; } f(1);"),
        "<budget-smoke>",
    )
    .expect("script ran");

    let stats = rt.runtime_budget_stats();
    assert!(stats.turns_started >= 1);
    assert_eq!(stats.turns_started, stats.turns_finished);
    assert!(stats.reductions_executed > 1);
    assert!(stats.bytecode_calls >= 1);
    assert!(stats.budget_limit_observations >= 1);
}

#[test]
fn runtime_budget_stats_can_be_reset() {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript("1 + 1;"), "<budget-reset>")
        .expect("script ran");
    assert!(rt.runtime_budget_stats().reductions_executed > 0);

    rt.reset_runtime_budget_stats();
    assert_eq!(rt.runtime_budget_stats().reductions_executed, 0);
    assert_eq!(rt.runtime_budget_stats().turns_started, 0);
}

#[test]
fn runtime_budget_stats_include_microtasks_and_heap_observations() {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.set_runtime_budget(RuntimeBudget {
        max_microtasks_per_drain: Some(0),
        ..RuntimeBudget::default()
    });

    rt.run_script(
        SourceInput::from_javascript(
            "queueMicrotask(() => { globalThis.budgetObject = { ok: true }; }); undefined;",
        ),
        "<budget-microtask>",
    )
    .expect("script ran");

    let stats = rt.runtime_budget_stats();
    assert!(stats.microtask_drains >= 1);
    assert!(stats.microtasks_executed >= 1);
    assert!(stats.allocated_objects_observed >= 1);
    assert!(stats.allocated_bytes_observed > 0);
    assert!(stats.max_live_heap_bytes > 0);
    assert!(stats.budget_limit_observations >= 1);
}

#[test]
fn runtime_budget_can_reject_script_execution() {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.set_runtime_budget(RuntimeBudget {
        on_exceeded: RuntimeBudgetExceededAction::Reject,
        max_reductions_per_turn: Some(0),
        ..RuntimeBudget::default()
    });

    let err = rt
        .run_script(SourceInput::from_javascript("1 + 1;"), "<budget-reject>")
        .expect_err("budget should reject");

    match err {
        OtterError::Runtime { diagnostic } => {
            assert_eq!(diagnostic.code, "BUDGET_EXCEEDED");
        }
        other => panic!("expected runtime budget error, got {other:?}"),
    }
    assert_eq!(rt.runtime_budget_stats().budget_rejections, 1);
}
