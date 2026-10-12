//! `--trace-gc` report: one line per collection pause and a summary.
//!
//! # Contents
//! - [`render`] — the text the CLI writes to stderr when a run ends. Old
//!   generation sizes print as page bytes with the bytes cells occupy in
//!   parentheses.
//!
//! # Invariants
//! - Formatting runs after the run, never inside a collection.
//! - Percentiles are nearest-rank over the pauses of one kind; a kind that
//!   never ran prints nothing.
//!
//! # See also
//! - `otter_runtime::GcPauseCapture` — the collector's pause records.

use std::fmt::Write as _;

use otter_runtime::{GcPauseCapture, GcPauseKind, GcPauseOutcome, GcPauseTrigger};

const MB: f64 = 1024.0 * 1024.0;

fn kind_name(kind: GcPauseKind) -> &'static str {
    match kind {
        GcPauseKind::Minor => "Scavenge",
        GcPauseKind::Full => "Mark-sweep",
        GcPauseKind::SplitMark => "Mark",
        GcPauseKind::SplitStep => "Mark step",
        GcPauseKind::SplitSweep => "Sweep",
        GcPauseKind::SplitWeak => "Weak processing",
    }
}

fn trigger_name(trigger: GcPauseTrigger) -> &'static str {
    match trigger {
        GcPauseTrigger::Explicit => "explicit",
        GcPauseTrigger::Stress => "stress",
        GcPauseTrigger::NurseryCapacity => "nursery full",
        GcPauseTrigger::HeapCap => "heap cap",
        GcPauseTrigger::GrowthBudget => "old generation limit",
        GcPauseTrigger::CodeReclamation => "code reclamation",
    }
}

fn ms(ns: u64) -> f64 {
    ns as f64 / 1.0e6
}

fn nearest_rank(sorted: &[u64], numerator: usize, denominator: usize) -> u64 {
    let rank = (sorted.len() * numerator).div_ceil(denominator).max(1);
    sorted[rank - 1]
}

/// The report for `capture`.
#[must_use]
pub(crate) fn render(capture: &GcPauseCapture) -> String {
    let mut out = String::new();
    for record in &capture.records {
        let outcome = match record.outcome {
            GcPauseOutcome::Completed => "",
            GcPauseOutcome::CollectionFailed => " failed",
            GcPauseOutcome::CompletedAllocationRefused => " allocation refused",
        };
        let _ = writeln!(
            out,
            "[gc] {:>10.3} ms: {} ({}){} {:.3} ms; old {:.1} ({:.1}) -> {:.1} ({:.1}) MB, young {:.1} -> {:.1} MB",
            ms(record.start_ns),
            kind_name(record.kind),
            trigger_name(record.trigger),
            outcome,
            ms(record.duration_ns),
            record.before.old_bytes as f64 / MB,
            record.before.old_allocated_bytes as f64 / MB,
            record.after.old_bytes as f64 / MB,
            record.after.old_allocated_bytes as f64 / MB,
            record.before.young_bytes as f64 / MB,
            record.after.young_bytes as f64 / MB,
        );
    }
    for kind in [
        GcPauseKind::Minor,
        GcPauseKind::Full,
        GcPauseKind::SplitMark,
        GcPauseKind::SplitStep,
        GcPauseKind::SplitSweep,
        GcPauseKind::SplitWeak,
    ] {
        let mut pauses: Vec<u64> = capture
            .records
            .iter()
            .filter(|record| record.kind == kind)
            .map(|record| record.duration_ns)
            .collect();
        if pauses.is_empty() {
            continue;
        }
        pauses.sort_unstable();
        let total: u64 = pauses.iter().sum();
        let _ = writeln!(
            out,
            "[gc] {}: {} pauses, {:.3} ms total; p50 {:.3}, p99 {:.3}, max {:.3} ms",
            kind_name(kind),
            pauses.len(),
            ms(total),
            ms(nearest_rank(&pauses, 1, 2)),
            ms(nearest_rank(&pauses, 99, 100)),
            ms(pauses[pauses.len() - 1]),
        );
    }
    let peak_old = capture
        .records
        .iter()
        .map(|record| record.before.old_bytes.max(record.after.old_bytes))
        .max();
    let retained = capture
        .records
        .iter()
        .rev()
        .find(|record| record.kind == GcPauseKind::Full)
        .map(|record| record.after.old_bytes);
    if let Some(peak_old) = peak_old {
        match retained {
            Some(retained) => {
                let _ = writeln!(
                    out,
                    "[gc] old generation: {:.1} MB after the last mark-sweep, {:.1} MB at most",
                    retained as f64 / MB,
                    peak_old as f64 / MB,
                );
            }
            None => {
                let _ = writeln!(
                    out,
                    "[gc] old generation: no mark-sweep, {:.1} MB at most",
                    peak_old as f64 / MB,
                );
            }
        }
    }
    if capture.dropped_records != 0 {
        let _ = writeln!(
            out,
            "[gc] {} pauses not recorded: the record buffer was full",
            capture.dropped_records
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use otter_runtime::{GcPauseRecord, HeapOccupancy};

    use super::*;

    fn record(kind: GcPauseKind, duration_ns: u64, old_after: u64) -> GcPauseRecord {
        GcPauseRecord {
            sequence: 1,
            start_ns: 1_000_000,
            duration_ns,
            kind,
            trigger: GcPauseTrigger::NurseryCapacity,
            outcome: GcPauseOutcome::Completed,
            full_cycles_before: 0,
            full_cycles_after: 0,
            minor_cycles_before: 0,
            minor_cycles_after: 1,
            before: HeapOccupancy {
                old_bytes: 4 << 20,
                old_allocated_bytes: 3 << 20,
                young_bytes: 8 << 20,
            },
            after: HeapOccupancy {
                old_bytes: old_after,
                old_allocated_bytes: old_after / 2,
                young_bytes: 1 << 20,
            },
        }
    }

    #[test]
    fn report_lists_each_pause_and_summarizes_each_kind() {
        let capture = GcPauseCapture {
            records: vec![
                record(GcPauseKind::Minor, 2_000_000, 5 << 20),
                record(GcPauseKind::Minor, 1_000_000, 6 << 20),
                record(GcPauseKind::Full, 9_000_000, 3 << 20),
            ],
            dropped_records: 0,
            incomplete: false,
            contains_split_phases: false,
        };
        let report = render(&capture);
        assert_eq!(report.lines().count(), 6, "{report}");
        assert!(report.contains("Scavenge (nursery full) 2.000 ms; old 4.0 (3.0) -> 5.0 (2.5) MB"));
        assert!(report.contains(
            "[gc] Scavenge: 2 pauses, 3.000 ms total; p50 1.000, p99 2.000, max 2.000 ms"
        ));
        assert!(
            report
                .contains("[gc] old generation: 3.0 MB after the last mark-sweep, 6.0 MB at most")
        );
    }
}
