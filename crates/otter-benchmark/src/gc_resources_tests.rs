//! Trace units, exact collector-cycle coverage and nearest-rank tails.

use super::*;
use otter_runtime::{GcPauseRecord, GcPauseTrigger};

fn record(sequence: u64, duration: u64) -> GcPauseRecord {
    GcPauseRecord {
        sequence,
        start_ns: sequence * 10_000,
        duration_ns: duration,
        kind: GcPauseKind::Minor,
        trigger: GcPauseTrigger::NurseryCapacity,
        outcome: GcPauseOutcome::Completed,
        full_cycles_before: 0,
        full_cycles_after: 0,
        minor_cycles_before: sequence - 1,
        minor_cycles_after: sequence,
    }
}

fn capture(records: Vec<GcPauseRecord>) -> GcPauseCapture {
    GcPauseCapture {
        records,
        dropped_records: 0,
        incomplete: false,
        contains_split_phases: false,
    }
}

fn counters(full: u64, minor: u64) -> RuntimeExecutionStats {
    RuntimeExecutionStats {
        gc_cycles: full,
        gc_minor_cycles: minor,
        ..Default::default()
    }
}

#[test]
fn portable_trace_preserves_exact_units_and_declared_nearest_rank() {
    let trace = trace_document(
        &capture((1..=100).map(|i| record(i, i * 101)).collect()),
        17,
        &counters(0, 0),
        &counters(0, 100),
    );
    let event = &trace["traceEvents"][0];
    assert_eq!(event["pid"], 17);
    assert_eq!(event["ph"], "X");
    assert_eq!(event["ts"], 10.0);
    assert_eq!(event["dur"], 0.101);
    assert_eq!(event["args"]["durationNs"], 101);
    let summary = &trace["metadata"]["descriptiveSummaries"][0];
    assert_eq!(summary["p50Ns"], 50 * 101);
    assert_eq!(summary["p95Ns"], 95 * 101);
    assert_eq!(summary["p99Ns"], 99 * 101);
    assert_eq!(summary["tailSummaryAvailable"], true);
    assert_eq!(trace["metadata"]["coverageComplete"], true);
    assert_eq!(trace["metadata"]["tailCoverageComplete"], false);
    assert_eq!(trace["metadata"]["descriptiveSummaries"][1]["count"], 0);
    assert!(trace["metadata"]["descriptiveSummaries"][1]["p99Ns"].is_null());
    assert_eq!(
        trace["metadata"]["descriptiveSummaries"][1]["tailSummaryAvailable"],
        false
    );
    assert_eq!(
        trace["metadata"]["tailCoverageGaps"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn complete_full_record_covers_its_nested_minor_before_next_outer_minor() {
    let full = GcPauseRecord {
        kind: GcPauseKind::Full,
        trigger: GcPauseTrigger::GrowthBudget,
        full_cycles_before: 4,
        full_cycles_after: 5,
        minor_cycles_before: 10,
        minor_cycles_after: 12,
        ..record(1, 5_000)
    };
    let minor = GcPauseRecord {
        full_cycles_before: 5,
        full_cycles_after: 5,
        minor_cycles_before: 12,
        minor_cycles_after: 13,
        ..record(2, 7_000)
    };
    let trace = trace_document(
        &capture(vec![full, minor]),
        1,
        &counters(4, 10),
        &counters(5, 13),
    );
    assert_eq!(trace["metadata"]["coverageComplete"], true);
    assert_eq!(trace["metadata"]["tailCoverageComplete"], true);
    assert_eq!(
        trace["metadata"]["counterCoverage"]["recordedFullCycles"],
        1
    );
    assert_eq!(
        trace["metadata"]["counterCoverage"]["recordedMinorCyclesIncludingNested"],
        3
    );
    assert_eq!(trace["traceEvents"].as_array().unwrap().len(), 2);
}

#[test]
fn omitted_cycles_cannot_become_a_complete_or_available_tail_sample() {
    for records in [vec![], vec![record(1, 101)]] {
        let trace = trace_document(&capture(records), 1, &counters(0, 0), &counters(0, 2));
        assert_eq!(trace["metadata"]["coverageComplete"], false);
        assert_eq!(trace["metadata"]["tailCoverageComplete"], false);
        assert_eq!(
            trace["metadata"]["descriptiveSummaries"][0]["tailSummaryAvailable"],
            false
        );
        assert!(trace["metadata"]["descriptiveSummaries"][0]["p50Ns"].is_null());
        assert!(
            !trace["metadata"]["coverageGaps"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    let second = GcPauseRecord {
        minor_cycles_before: 2,
        minor_cycles_after: 3,
        ..record(2, 101)
    };
    let trace = trace_document(
        &capture(vec![record(1, 101), second]),
        1,
        &counters(0, 0),
        &counters(0, 3),
    );
    assert_eq!(
        trace["metadata"]["coverageComplete"], false,
        "gap between captured outer events"
    );
}

#[test]
fn malformed_sequence_time_kind_and_regressing_counts_retain_raw_events() {
    let valid = record(1, 101);
    let malformed = [
        GcPauseRecord {
            sequence: 2,
            ..valid
        },
        GcPauseRecord {
            start_ns: u64::MAX,
            duration_ns: 2,
            ..valid
        },
        GcPauseRecord {
            kind: GcPauseKind::Full,
            ..valid
        },
        GcPauseRecord {
            minor_cycles_before: 2,
            minor_cycles_after: 1,
            ..valid
        },
        GcPauseRecord {
            outcome: GcPauseOutcome::CompletedAllocationRefused,
            ..valid
        },
    ];
    for event in malformed {
        let trace = trace_document(&capture(vec![event]), 1, &counters(0, 0), &counters(0, 1));
        assert_eq!(trace["metadata"]["coverageComplete"], false);
        assert_eq!(trace["traceEvents"].as_array().unwrap().len(), 1);
        assert_eq!(trace["traceEvents"][0]["args"]["startNs"], event.start_ns);
    }
    let overlapping = GcPauseRecord {
        start_ns: 10_100,
        ..record(2, 101)
    };
    let trace = trace_document(
        &capture(vec![valid, overlapping]),
        1,
        &counters(0, 0),
        &counters(0, 2),
    );
    assert_eq!(trace["metadata"]["coverageComplete"], false);
    let trace = trace_document(&capture(vec![]), 1, &counters(1, 2), &counters(0, 1));
    assert_eq!(
        trace["metadata"]["coverageComplete"], false,
        "runtime counters cannot regress"
    );
}

#[test]
fn empty_but_exact_capture_reports_missing_classes_without_zero_quantiles() {
    let trace = trace_document(&capture(vec![]), 1, &counters(1, 2), &counters(1, 2));
    assert_eq!(trace["metadata"]["coverageComplete"], true);
    assert_eq!(trace["metadata"]["tailCoverageComplete"], false);
    assert_eq!(
        trace["metadata"]["tailCoverageGaps"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    for summary in trace["metadata"]["descriptiveSummaries"]
        .as_array()
        .unwrap()
    {
        assert_eq!(summary["count"], 0);
        assert!(summary["p50Ns"].is_null());
        assert!(summary["p95Ns"].is_null());
        assert!(summary["p99Ns"].is_null());
        assert!(summary["maxNs"].is_null());
        assert_eq!(summary["tailSummaryAvailable"], false);
    }
}

#[test]
fn incomplete_failed_and_dropped_records_remain_visible_and_unscoreable() {
    let event = GcPauseRecord {
        outcome: GcPauseOutcome::CollectionFailed,
        minor_cycles_after: 0,
        ..record(1, 51)
    };
    let trace = trace_document(
        &GcPauseCapture {
            records: vec![event],
            dropped_records: 2,
            incomplete: true,
            contains_split_phases: true,
        },
        1,
        &counters(0, 0),
        &counters(0, 0),
    );
    assert_eq!(trace["traceEvents"].as_array().unwrap().len(), 1);
    assert_eq!(trace["metadata"]["coverageComplete"], false);
    assert_eq!(
        trace["metadata"]["coverageGaps"].as_array().unwrap().len(),
        4
    );
    assert!(trace["metadata"]["descriptiveSummaries"][0]["p50Ns"].is_null());
}
