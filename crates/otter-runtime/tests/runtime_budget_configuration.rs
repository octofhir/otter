//! The per-turn execution budget is part of the runtime's configuration and
//! its telemetry, not a knob bolted onto a built isolate.
//!
//! The resource ledger meters memory, retained bytes, queued work, and worker
//! slots; the VM meters CPU work per turn. These tests hold the two halves to
//! one contract: the CPU policy is configured before construction, it is in
//! force before the first turn, a child isolate inherits it, and both halves
//! are read through one report — from a handle, without a command reaching
//! the isolate.

use otter_runtime::{
    JitSelection, Otter, OtterError, ResourceAccount, ResourceClass, ResourceLimits, Runtime,
    RuntimeBudget, RuntimeBudgetExceededAction, SourceInput,
};

fn rejecting_budget(max_reductions: u64) -> RuntimeBudget {
    RuntimeBudget {
        on_exceeded: RuntimeBudgetExceededAction::Reject,
        max_reductions_per_turn: Some(max_reductions),
        ..RuntimeBudget::default()
    }
}

#[test]
fn a_configured_budget_is_in_force_before_the_first_turn() {
    let mut rt = Runtime::builder()
        .runtime_budget(rejecting_budget(1))
        .build()
        .expect("bootstrap is host work and is not metered against the policy");

    let error = rt
        .run_script(
            SourceInput::from_javascript(
                "let total = 0; for (let i = 0; i < 100; i += 1) { total += i; } total;",
            ),
            "<configured-budget>",
        )
        .expect_err("the first user turn crosses the configured reduction limit");

    match error {
        OtterError::Runtime { diagnostic } => assert_eq!(diagnostic.code, "BUDGET_EXCEEDED"),
        other => panic!("expected a budget diagnostic, got {other:?}"),
    }

    let report = rt.budget_report();
    assert_eq!(report.limits, rejecting_budget(1));
    assert_eq!(report.execution.budget_rejections, 1);
}

#[test]
fn bootstrap_work_is_not_charged_to_the_first_turn() {
    let rt = Runtime::builder()
        .runtime_budget(RuntimeBudget {
            max_reductions_per_turn: Some(1),
            ..RuntimeBudget::default()
        })
        .build()
        .expect("runtime");

    // Installing the policy also zeroes the counters, so an embedder reading
    // the report before running anything sees its own workload from zero
    // rather than the shims the isolate was built from.
    let report = rt.budget_report();
    assert_eq!(report.execution.reductions_executed, 0);
    assert_eq!(report.execution.turns_started, 0);
    assert_eq!(report.execution.budget_limit_observations, 0);
}

#[test]
fn one_report_carries_the_cpu_counters_and_the_resource_ledger() {
    let account = ResourceAccount::new(
        ResourceLimits::builder()
            .limit(ResourceClass::Isolates, 4)
            .build(),
    );
    let mut rt = Runtime::builder()
        .resource_account(account)
        .build()
        .expect("runtime");

    rt.run_script(
        SourceInput::from_javascript("function f(x) { return x + 1; } f(1);"),
        "<budget-report>",
    )
    .expect("script ran");

    let report = rt.budget_report();
    assert!(report.execution.reductions_executed > 0);
    assert_eq!(
        report.execution.turns_started,
        report.execution.turns_finished
    );
    let isolates = report
        .resources
        .entries()
        .iter()
        .find(|entry| entry.class() == ResourceClass::Isolates)
        .expect("the ledger reports the isolate class");
    assert_eq!(isolates.current(), 1);
}

#[test]
fn telemetry_outlives_a_borrow_of_the_runtime() {
    let mut rt = Runtime::builder().build().expect("runtime");
    let telemetry = rt.budget_telemetry();
    assert_eq!(telemetry.snapshot().turns_finished, 0);

    rt.run_script(SourceInput::from_javascript("1 + 1;"), "<telemetry-cell>")
        .expect("script ran");

    // The cell is the isolate's own, published at the turn boundary.
    assert!(telemetry.snapshot().turns_finished >= 1);
    assert_eq!(
        telemetry.snapshot().reductions_executed,
        rt.runtime_budget_stats().reductions_executed
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handle_reports_its_isolate_budget_without_a_command() {
    let otter = Otter::builder()
        .jit_selection(JitSelection::InterpreterOnly)
        .runtime_budget(RuntimeBudget {
            max_reductions_per_turn: Some(4),
            ..RuntimeBudget::default()
        })
        .build()
        .expect("managed runtime");

    let before = otter.budget_report();
    assert_eq!(before.limits.max_reductions_per_turn, Some(4));
    assert_eq!(before.execution.turns_finished, 0);

    otter
        .eval("function f(x) { return x + 1; } f(1);")
        .await
        .expect("script ran");

    // The counters crossed the isolate thread through the published cell, not
    // through the command inbox.
    let after = otter.budget_report();
    assert!(after.execution.turns_finished >= 1);
    assert!(after.execution.reductions_executed > before.execution.reductions_executed);
    assert!(after.execution.budget_limit_observations >= 1);

    otter.handle().shutdown_and_wait().await;
}
