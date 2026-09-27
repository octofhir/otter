//! Allocation census for a complete production-tiered script.
//!
//! # Contents
//! - Existing per-type GC counters bracket one unchanged source execution.
//! - JSON records allocation traffic, collection counts and reported live bytes.
//!
//! # Invariants
//! - No diagnostic work executes in the measured script or allocation path.
//! - Runtime bootstrap is excluded from allocation deltas.
//! - Live bytes are collector accounting at completion, not a forced-GC census.
//! - Script errors fail the process; partial execution is never a successful row.
//!
//! # See also
//! - `scripts/dev/fixed-work.py` measures production process instructions and RSS.

use std::{error::Error, path::Path};

use otter_runtime::{JitSelection, Runtime, SourceInput};
use serde_json::json;

pub(super) fn run(path: &Path) -> Result<(), Box<dyn Error>> {
    let source = std::fs::read_to_string(path)?;
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .build()?;
    let before = runtime.heap_stats().clone();
    let result = runtime.run_script(
        SourceInput::from_javascript(&source),
        &path.to_string_lossy(),
    )?;
    let after = runtime.heap_stats();
    let rows = after
        .by_type
        .iter()
        .zip(&before.by_type)
        .enumerate()
        .filter_map(|(tag, (after, before))| {
            let allocations = after.alloc_count_total - before.alloc_count_total;
            (allocations != 0).then(|| {
                json!({
                    "tag": tag,
                    "allocations": allocations,
                    "allocatedBytes": after.alloc_bytes_total - before.alloc_bytes_total,
                    "reportedLiveBytes": after.live_bytes,
                })
            })
        })
        .collect::<Vec<_>>();
    println!(
        "{}",
        json!({
            "source": path,
            "tier": "production",
            "completion": result.completion_string(),
            "allocatedBytes": after.alloc_bytes_total - before.alloc_bytes_total,
            "minorCollections": after.minor_gc_cycles - before.minor_gc_cycles,
            "fullCollections": after.gc_cycles - before.gc_cycles,
            "minorPauseNs": after.minor_pause_ns_total - before.minor_pause_ns_total,
            "fullPauseNs": after.full_pause_ns_total - before.full_pause_ns_total,
            "reportedLiveBytes": after.live_bytes,
            "byType": rows,
        })
    );
    Ok(())
}
