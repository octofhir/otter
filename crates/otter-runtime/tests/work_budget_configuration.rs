//! The per-slice work budget is part of the runtime's configuration and
//! its telemetry, not a knob bolted onto a built isolate.
//!
//! The resource ledger meters memory, retained bytes, queued work, and worker
//! slots; the VM meters isolate work per slice. These tests hold the two halves to
//! one contract: the CPU policy is configured before construction, it is in
//! force before the first turn, a child isolate inherits it, and both halves
//! are read through one report — from a handle, without a command reaching
//! the isolate.

use otter_runtime::{
    JitSelection, Otter, OtterError, ResourceAccount, ResourceClass, ResourceLimits, Runtime,
    SourceInput, WorkBudget, WorkBudgetExceededAction,
};

fn rejecting_budget(max_work_units: u64) -> WorkBudget {
    WorkBudget {
        on_exceeded: WorkBudgetExceededAction::Reject,
        max_work_units_per_turn: Some(max_work_units),
        ..WorkBudget::default()
    }
}

#[test]
fn a_configured_budget_is_in_force_before_the_first_turn() {
    let mut rt = Runtime::builder()
        .work_budget(rejecting_budget(1))
        .build()
        .expect("bootstrap is host work and is not metered against the policy");

    let error = rt
        .run_script(
            SourceInput::from_javascript(
                "let total = 0; for (let i = 0; i < 100; i += 1) { total += i; } total;",
            ),
            "<configured-budget>",
        )
        .expect_err("the first user turn crosses the configured work limit");

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
        .work_budget(WorkBudget {
            max_work_units_per_turn: Some(1),
            ..WorkBudget::default()
        })
        .build()
        .expect("runtime");

    // Installing the policy also zeroes the counters, so an embedder reading
    // the report before running anything sees its own workload from zero
    // rather than the shims the isolate was built from.
    let report = rt.budget_report();
    assert_eq!(report.execution.work_units_executed, 0);
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
    assert!(report.execution.work_units_executed > 0);
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
fn regexp_backtracking_is_charged_to_the_shared_work_ledger() {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(r#"/(a+)+$/.test("aaaaaaaaaaaaaaaa!");"#),
        "<regexp-budget-accounting>",
    )
    .expect("bounded regexp ran");

    let stats = rt.budget_report().execution;
    assert!(stats.regex_backtrack_steps > 0);
    assert!(stats.work_units_executed >= stats.regex_backtrack_steps);
}

#[test]
fn regexp_default_limit_surfaces_the_runtime_resource_error() {
    let mut rt = Runtime::builder().build().expect("runtime");
    let error = rt
        .run_script(
            SourceInput::from_javascript(r#"/(a|aa)+b/.test("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");"#),
            "<regexp-default-budget>",
        )
        .expect_err("catastrophic backtracking must exhaust the finite default");

    match error {
        OtterError::Runtime { diagnostic } => assert_eq!(diagnostic.code, "BUDGET_EXCEEDED"),
        other => panic!("expected a budget diagnostic, got {other:?}"),
    }
    let stats = rt.budget_report().execution;
    assert!(stats.regex_backtrack_steps >= 1_000_000);
    assert!(stats.work_units_executed >= stats.regex_backtrack_steps);
}

#[test]
fn cooperative_yields_preserve_results_in_every_tier() {
    const SOURCE: &str = r#"
        let total = 0;
        for (let i = 0; i < 20_000; i += 1) total += i;
        total;
    "#;

    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut rt = Runtime::builder()
            .jit_selection(selection)
            .work_budget(WorkBudget {
                on_exceeded: WorkBudgetExceededAction::Yield,
                max_work_units_per_turn: Some(64),
                ..WorkBudget::default()
            })
            .build()
            .expect("runtime");

        let result = rt
            .run_script(SourceInput::from_javascript(SOURCE), "<work-yield-tier>")
            .expect("yielding must preserve the turn");
        assert_eq!(result.completion_string(), "199990000", "{selection:?}");
        let budget = rt.work_budget_stats();
        assert!(budget.forced_yields > 0, "{selection:?}");
        if selection != JitSelection::InterpreterOnly {
            let jit = rt.execution_stats();
            assert!(jit.jit_osr_attempts > 0, "{selection:?}: {jit:?}");
            assert!(jit.jit_code_generations > 0, "{selection:?}: {jit:?}");
            assert!(jit.jit_leaf_stub_transitions > 0, "{selection:?}: {jit:?}");
            assert!(
                budget.forced_yields >= jit.jit_leaf_stub_transitions,
                "{selection:?}: budget={budget:?} jit={jit:?}"
            );
        }
    }
}

#[test]
fn yielding_microtask_chain_keeps_generation_order() {
    let mut rt = Runtime::builder()
        .work_budget(WorkBudget {
            on_exceeded: WorkBudgetExceededAction::Yield,
            max_work_units_per_turn: Some(4),
            ..WorkBudget::default()
        })
        .build()
        .expect("runtime");

    rt.run_script(
        SourceInput::from_javascript(
            r#"
                globalThis.workOrder = [];
                queueMicrotask(() => {
                    workOrder.push(1);
                    queueMicrotask(() => workOrder.push(3));
                });
                queueMicrotask(() => workOrder.push(2));
            "#,
        ),
        "<work-yield-microtasks>",
    )
    .expect("microtask chain");

    let order = rt
        .run_script(
            SourceInput::from_javascript("JSON.stringify(workOrder);"),
            "<work-yield-order>",
        )
        .expect("read order");
    assert_eq!(order.completion_string(), "[1,2,3]");
    assert!(rt.work_budget_stats().forced_yields > 0);
}

#[test]
fn regexp_and_gc_charge_the_same_work_counter() {
    let mut rt = Runtime::builder()
        .max_heap_bytes(16 * 1024 * 1024)
        .work_budget(WorkBudget {
            on_exceeded: WorkBudgetExceededAction::Yield,
            max_work_units_per_turn: Some(10_000),
            ..WorkBudget::default()
        })
        .build()
        .expect("runtime");

    let result = rt
        .run_script(
            SourceInput::from_javascript(
                r#"
                    const matched = /(a+)+$/.test("aaaaaaaaaaaaaaaa!");
                    const keep = [];
                    for (let i = 0; i < 150_000; i += 1) {
                        keep[i % 1024] = { i, text: "allocation-" + i };
                    }
                    matched;
                "#,
            ),
            "<work-yield-regexp-gc>",
        )
        .expect("bounded work producers");
    assert_eq!(result.completion_string(), "false");

    let stats = rt.work_budget_stats();
    assert!(stats.regex_backtrack_steps > 10_000);
    assert!(stats.gc_work_units > 0, "{stats:?}");
    assert!(stats.work_units_executed >= stats.regex_backtrack_steps + stats.gc_work_units);
    assert!(stats.forced_yields > 0);
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
        telemetry.snapshot().work_units_executed,
        rt.work_budget_stats().work_units_executed
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handle_reports_its_isolate_budget_without_a_command() {
    let otter = Otter::builder()
        .jit_selection(JitSelection::InterpreterOnly)
        .work_budget(WorkBudget {
            max_work_units_per_turn: Some(4),
            ..WorkBudget::default()
        })
        .build()
        .expect("managed runtime");

    let before = otter.budget_report();
    assert_eq!(before.limits.max_work_units_per_turn, Some(4));
    assert_eq!(before.execution.turns_finished, 0);

    otter
        .eval("function f(x) { return x + 1; } f(1);")
        .await
        .expect("script ran");

    // The counters crossed the isolate thread through the published cell, not
    // through the command inbox.
    let after = otter.budget_report();
    assert!(after.execution.turns_finished >= 1);
    assert!(after.execution.work_units_executed > before.execution.work_units_executed);
    assert!(after.execution.budget_limit_observations >= 1);

    otter.handle().shutdown_and_wait().await;
}
