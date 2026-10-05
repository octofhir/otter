//! Collection-service pause summaries and portable trace export.
//!
//! # Contents
//! - Nearest-rank descriptive quantiles of complete nonoverlapping service.
//! - Exact sequence, time and full/nested-minor cycle coverage validation.
//! - Chrome Trace Event JSON produced after collection capture has stopped.
//! - Owned collector-space/RSS snapshots for separately forced retention phases.
//!
//! # Invariants
//! - Missing classes have no quantiles; overflow, failures and split calls are
//!   retained as explicit coverage gaps.
//! - No formatting, RSS inspection or quantile sorting runs inside collection.
//! - Reserved off-slot bytes are not added to reconciled live GC-cell bytes.
//! - These service intervals are not a whole-mutator-blocked comparison to Bun.
//!
//! # See also
//! - `otter_runtime::GcPauseCapture` is the collector's only observation carrier.

use otter_runtime::{GcPauseCapture, GcPauseKind, GcPauseOutcome, Runtime, RuntimeExecutionStats};
use serde_json::{Value, json};
use sysinfo::{ProcessesToUpdate, System};

/// RSS and distinct collector accounting at a quiescent phase boundary.
pub fn memory_snapshot(runtime: &mut Runtime) -> Result<Value, String> {
    let gc = runtime.heap_stats().clone();
    let spaces = runtime.heap_accounting_stats();
    let pid = sysinfo::get_current_pid().map_err(|error| error.to_string())?;
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    let rss = system
        .process(pid)
        .map(sysinfo::Process::memory)
        .filter(|bytes| *bytes > 0)
        .ok_or_else(|| "process RSS unavailable".to_string())?;
    let by_type: Vec<_> = gc
        .by_type
        .iter()
        .enumerate()
        .filter(|(_, row)| row.live_bytes != 0)
        .map(|(tag, row)| json!({"tag": tag, "liveCellBytes": row.live_bytes}))
        .collect();
    Ok(json!({
        "rssBytes": rss,
        "reconciledLiveCellBytes": gc.live_bytes,
        "liveObjects": gc.live_objects,
        "allocatedSpaceBytes": spaces.allocated_bytes,
        "reservedOffSlotBytes": spaces.reserved_bytes,
        "fullCycles": gc.gc_cycles,
        "minorCycles": gc.minor_gc_cycles,
        "byType": by_type,
        "gcOwnedExternalBytes": null,
        "externalCoverageGap": "no independently reconciled GC-owned backing subtotal",
    }))
}

fn nearest_rank(sorted: &[u64], numerator: usize, denominator: usize) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (sorted.len() * numerator).div_ceil(denominator);
    Some(sorted[rank.saturating_sub(1)])
}

/// Preserve raw portable events and annotate whether whole-service summaries
/// have complete coverage. Quantiles remain descriptive of this one capture.
/// The counter snapshots must immediately surround this exact capture window.
pub fn trace_document(
    capture: &GcPauseCapture,
    pid: u32,
    before: &RuntimeExecutionStats,
    after: &RuntimeExecutionStats,
) -> Value {
    let mut gaps = Vec::new();
    if capture.dropped_records != 0 {
        gaps.push("record buffer overflow".to_string());
    }
    if capture.incomplete {
        gaps.push("service was incomplete when capture stopped".to_string());
    }
    if capture.contains_split_phases
        || capture
            .records
            .iter()
            .any(|record| !matches!(record.kind, GcPauseKind::Minor | GcPauseKind::Full))
    {
        gaps.push("split service is not a whole collection pause".to_string());
    }
    if capture
        .records
        .iter()
        .any(|record| record.outcome == GcPauseOutcome::CollectionFailed)
    {
        gaps.push("collector service failed".to_string());
    }
    let mut full_cycles = before.gc_cycles;
    let mut minor_cycles = before.gc_minor_cycles;
    let mut recorded_full = 0_u64;
    let mut recorded_minor = 0_u64;
    let mut previous_end = 0_u64;
    for (index, record) in capture.records.iter().enumerate() {
        let expected_sequence = index as u64 + 1;
        if record.sequence != expected_sequence {
            gaps.push(format!(
                "record {index} has a missing or unordered sequence"
            ));
        }
        if record.start_ns < previous_end {
            gaps.push(format!("record {index} overlaps its preceding service"));
        }
        match record.start_ns.checked_add(record.duration_ns) {
            Some(end) => previous_end = end,
            None => gaps.push(format!("record {index} interval overflows nanoseconds")),
        }
        if record.full_cycles_before != full_cycles || record.minor_cycles_before != minor_cycles {
            gaps.push(format!("record {index} skips or repeats collector cycles"));
        }
        if record.outcome == GcPauseOutcome::CompletedAllocationRefused
            && (record.kind != GcPauseKind::Full
                || record.trigger != otter_runtime::GcPauseTrigger::HeapCap)
        {
            gaps.push(format!(
                "record {index} allocation refusal has no full heap-cap service"
            ));
        }
        let full_delta = record
            .full_cycles_after
            .checked_sub(record.full_cycles_before);
        let minor_delta = record
            .minor_cycles_after
            .checked_sub(record.minor_cycles_before);
        match (full_delta, minor_delta) {
            (Some(full), Some(minor)) => {
                match (
                    recorded_full.checked_add(full),
                    recorded_minor.checked_add(minor),
                ) {
                    (Some(all_full), Some(all_minor)) => {
                        recorded_full = all_full;
                        recorded_minor = all_minor;
                    }
                    _ => gaps.push("recorded cycle totals overflow".to_string()),
                }
                if record.outcome != GcPauseOutcome::CollectionFailed {
                    let kind_matches = match record.kind {
                        GcPauseKind::Minor => full == 0 && minor == 1,
                        GcPauseKind::Full => full >= 1 && minor >= 1,
                        _ => true,
                    };
                    if !kind_matches {
                        gaps.push(format!(
                            "record {index} completion contradicts its cycle counts"
                        ));
                    }
                }
            }
            _ => gaps.push(format!("record {index} has regressing collector counters")),
        }
        full_cycles = record.full_cycles_after;
        minor_cycles = record.minor_cycles_after;
    }
    let full_delta = after.gc_cycles.checked_sub(before.gc_cycles);
    let minor_delta = after.gc_minor_cycles.checked_sub(before.gc_minor_cycles);
    if full_delta != Some(recorded_full)
        || minor_delta != Some(recorded_minor)
        || full_cycles != after.gc_cycles
        || minor_cycles != after.gc_minor_cycles
    {
        gaps.push(
            "recorded outer and nested cycles do not cover the exact runtime counter delta"
                .to_string(),
        );
    }
    let coverage_complete = gaps.is_empty();
    let events: Vec<_> = capture
        .records
        .iter()
        .map(|record| {
            json!({
                "ph": "X", "cat": "gc", "name": format!("{:?}", record.kind),
                "pid": pid, "tid": 0,
                "ts": record.start_ns as f64 / 1000.0,
                "dur": record.duration_ns as f64 / 1000.0,
                "args": {
                    "sequence": record.sequence,
                    "startNs": record.start_ns, "durationNs": record.duration_ns,
                    "trigger": format!("{:?}", record.trigger),
                    "outcome": format!("{:?}", record.outcome),
                    "fullCyclesBefore": record.full_cycles_before,
                    "fullCyclesAfter": record.full_cycles_after,
                    "minorCyclesBefore": record.minor_cycles_before,
                    "minorCyclesAfter": record.minor_cycles_after,
                }
            })
        })
        .collect();
    let mut missing_classes = Vec::new();
    let summaries: Vec<_> = [GcPauseKind::Minor, GcPauseKind::Full].into_iter().map(|kind| {
        let mut durations: Vec<_> = capture.records.iter()
            .filter(|record| record.kind == kind && record.outcome != GcPauseOutcome::CollectionFailed)
            .map(|record| record.duration_ns).collect();
        durations.sort_unstable();
        if durations.is_empty() {
            missing_classes.push(format!("no observed {kind:?} collections"));
        }
        json!({
            "kind": format!("{kind:?}"), "count": durations.len(),
            "tailSummaryAvailable": coverage_complete && !durations.is_empty(),
            "p50Ns": if coverage_complete { nearest_rank(&durations, 50, 100) } else { None },
            "p95Ns": if coverage_complete { nearest_rank(&durations, 95, 100) } else { None },
            "p99Ns": if coverage_complete { nearest_rank(&durations, 99, 100) } else { None },
            "maxNs": if coverage_complete { durations.last() } else { None },
            "coverageGap": if durations.is_empty() { Some("no observed collections of this class") } else { None },
        })
    }).collect();
    json!({
        "traceEvents": events,
        "displayTimeUnit": "ns",
        "metadata": {
            "boundary": "complete synchronous collection service including trigger accounting",
            "clock": "monotonic; capture-local origin",
            "eventUnits": "microseconds; exact nanoseconds preserved in args",
            "nestedMinorPolicy": "included in outer full service",
            "quantileMethod": "nearest rank, ceil(p*n)-1, one capture, no pooling",
            "droppedRecords": capture.dropped_records,
            "incomplete": capture.incomplete,
            "containsSplitPhases": capture.contains_split_phases,
            "coverageComplete": coverage_complete,
            "coverageGaps": gaps,
            "counterCoverage": {
                "fullCyclesBefore": before.gc_cycles,
                "fullCyclesAfter": after.gc_cycles,
                "minorCyclesBefore": before.gc_minor_cycles,
                "minorCyclesAfter": after.gc_minor_cycles,
                "recordedFullCycles": recorded_full,
                "recordedMinorCyclesIncludingNested": recorded_minor,
            },
            "tailCoverageComplete": coverage_complete && missing_classes.is_empty(),
            "tailCoverageGaps": missing_classes,
            "descriptiveSummaries": summaries,
            "bunWholeMutatorPauseComparison": null,
            "comparisonGap": "validated equivalent Bun mutator boundaries are unavailable",
        }
    })
}

#[cfg(test)]
#[path = "gc_resources_tests.rs"]
mod tests;
