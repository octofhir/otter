//! Work-budget DTOs, enforcement modes, and VM resource counters are visible
//! through the direct runtime surface.

use otter_runtime::{OtterError, Runtime, SourceInput, WorkBudget, WorkBudgetExceededAction};

#[test]
fn work_budget_stats_are_visible_after_script_run() {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.set_work_budget(WorkBudget {
        max_work_units_per_turn: Some(1),
        ..WorkBudget::default()
    });

    rt.run_script(
        SourceInput::from_javascript("function f(x) { return x + 1; } f(1);"),
        "<budget-smoke>",
    )
    .expect("script ran");

    let stats = rt.work_budget_stats();
    assert!(stats.turns_started >= 1);
    assert_eq!(stats.turns_started, stats.turns_finished);
    assert!(stats.work_units_executed > 1);
    assert!(stats.bytecode_calls >= 1);
    assert!(stats.budget_limit_observations >= 1);
}

#[test]
fn work_budget_stats_can_be_reset() {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript("1 + 1;"), "<budget-reset>")
        .expect("script ran");
    assert!(rt.work_budget_stats().work_units_executed > 0);

    rt.reset_work_budget_stats();
    assert_eq!(rt.work_budget_stats().work_units_executed, 0);
    assert_eq!(rt.work_budget_stats().turns_started, 0);
}

#[test]
fn work_budget_stats_include_microtasks_and_heap_observations() {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.set_work_budget(WorkBudget {
        max_work_units_per_turn: Some(1),
        ..WorkBudget::default()
    });

    rt.run_script(
        SourceInput::from_javascript(
            "queueMicrotask(() => { globalThis.budgetObject = { ok: true }; }); undefined;",
        ),
        "<budget-microtask>",
    )
    .expect("script ran");

    let stats = rt.work_budget_stats();
    assert!(stats.microtask_drains >= 1);
    assert!(stats.microtasks_executed >= 1);
    assert!(stats.allocated_objects_observed >= 1);
    assert!(stats.allocated_bytes_observed > 0);
    assert!(stats.max_live_heap_bytes > 0);
    assert!(stats.budget_limit_observations >= 1);
}

#[test]
fn execution_result_carries_compact_stats_snapshot() {
    let mut rt = Runtime::builder().build().expect("runtime");
    let result = rt
        .run_script(
            SourceInput::from_javascript("function f(x) { return x + 1; } f(1);"),
            "<execution-stats>",
        )
        .expect("script ran");

    let stats = result.stats();
    assert!(stats.work_units_executed > 0);
    assert!(stats.bytecode_calls >= 1);
    assert!(stats.gc_alloc_bytes_total > 0);
}

#[test]
fn work_budget_can_reject_script_execution() {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.set_work_budget(WorkBudget {
        on_exceeded: WorkBudgetExceededAction::Reject,
        max_work_units_per_turn: Some(0),
        ..WorkBudget::default()
    });

    let err = rt
        .run_script(SourceInput::from_javascript("1 + 1;"), "<budget-reject>")
        .expect_err("budget should reject");

    match err {
        OtterError::Runtime { diagnostic } => {
            assert_eq!(diagnostic.code, "BUDGET_EXCEEDED");
        }
        other => panic!("expected work-budget error, got {other:?}"),
    }
    assert_eq!(rt.work_budget_stats().budget_rejections, 1);
}
