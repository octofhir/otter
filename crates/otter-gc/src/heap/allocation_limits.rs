//! Old-generation allocation budgets from collector survival and throughput.
//!
//! # Contents
//! - One common bounded limit for startup survival and post-major throughput.
//! - One-shot feedback from the first successful nursery evacuation.
//! - The explicit startup override and concurrent-marking start occupancy.
//!
//! # Invariants
//! - Startup feedback is consumed once; later minors never move the limit.
//! - An explicit override or completed major budget supersedes startup feedback.
//! - Copied nursery cells augment old occupancy; promoted cells are already old.
//! - The post-major formula and speed history use the existing physical ledger.
//!
//! # See also
//! - [`super::GcHeap::collect_minor_work`] commits survival after nursery growth.
//! - [`super::GcHeap::collect_full_work`] recomputes after the completed sweep.

use std::time::Instant;

use super::GcHeap;
use crate::compressed::cage_size;
use crate::scavenger::ScavengeStats;

/// Default old-generation budget before collector survival feedback. The
/// first successful minor may lower it once, while an explicit embedder
/// override persists until the first major recomputes the budget. Every
/// page this limit lets old space touch can stay resident after collection,
/// so the startup budget bounds promotion-driven peak footprint.
pub(super) const INITIAL_OLD_GENERATION_SIZE: u64 = 256 * 1024 * 1024;
/// Lower bound of every recomputed old-generation limit.
const MIN_OLD_GENERATION_SIZE: u64 = 16 * 1024 * 1024;
/// The growing-factor bounds and the mutator utilization the limit targets
/// (at most 3% of mutator time in mark-sweep).
const MIN_GROWING_FACTOR: f64 = 1.1;
const MAX_GROWING_FACTOR: f64 = 4.0;
const TARGET_MUTATOR_UTILIZATION: f64 = 0.97;
/// A limit always leaves this much old-generation room above the surviving
/// set.
const MINIMUM_ALLOCATION_LIMIT_GROWING_STEP: u64 = 8 * 1024 * 1024;
/// External (backing-store) bytes that may accumulate above the surviving
/// reservations before they force a mark-sweep. Objects owning them are
/// small cells with drop glue, so page occupancy barely moves while their
/// payloads can pin gigabytes of native memory; the separate limit reclaims
/// dead payloads without letting a large live external set license
/// proportional garbage.
pub(super) const EXTERNAL_ALLOCATION_SOFT_LIMIT: u64 = 64 * 1024 * 1024;
/// Near-cage hard ceiling on the old-generation limit (numerator/256
/// of the cage). A cap-less heap always attempts a full GC before cage
/// exhaustion, while a live set that legitimately approaches it settles
/// just under rather than thrashing (the limit — not occupancy — is what's
/// clamped; see `major_gc_due`). ~92% leaves headroom for the young
/// semispaces + per-page slack.
const MAJOR_GC_CAGE_SOFTCAP_NUM: u64 = 236; // 236/256 ≈ 92% of cage

/// Old-generation ceiling inside the cage ([`MAJOR_GC_CAGE_SOFTCAP_NUM`]).
pub(super) fn cage_hardcap_bytes() -> u64 {
    (cage_size() as u64) / 256 * MAJOR_GC_CAGE_SOFTCAP_NUM
}

/// The old-generation growing factor that keeps mark-sweep at
/// `1 - TARGET_MUTATOR_UTILIZATION` of the time if collection and allocation
/// keep their measured speeds (bytes per millisecond).
/// `F = R(1 - MU) / (R(1 - MU) - MU)` with `R = gc / mutator`, bounded to
/// `[MIN_GROWING_FACTOR, MAX_GROWING_FACTOR]`; without both speeds the factor
/// is the maximum.
fn dynamic_growing_factor(gc_speed: f64, mutator_speed: f64) -> f64 {
    if !(gc_speed > 0.0 && mutator_speed > 0.0) {
        return MAX_GROWING_FACTOR;
    }
    let ratio = gc_speed / mutator_speed;
    let a = ratio * (1.0 - TARGET_MUTATOR_UTILIZATION);
    let b = a - TARGET_MUTATOR_UTILIZATION;
    let factor = if a < b * MAX_GROWING_FACTOR {
        a / b
    } else {
        MAX_GROWING_FACTOR
    };
    factor.max(MIN_GROWING_FACTOR)
}

/// Apply the controller's unchanged growing-factor, headroom and cage bounds.
fn bounded_limit(working: u64, factor: f64, nursery_capacity: u64) -> u64 {
    let computed = (working as f64 * factor) as u64;
    let limit = computed
        .max(working.saturating_add(MINIMUM_ALLOCATION_LIMIT_GROWING_STEP))
        .saturating_add(nursery_capacity);
    limit.max(MIN_OLD_GENERATION_SIZE).min(cage_hardcap_bytes())
}

/// Old-generation occupancy at which a concurrent marking cycle starts
/// between the `live` bytes the last mark-sweep kept and the `limit` the
/// next one runs at: seven eighths of the way, leaving the last eighth of
/// the growth for the mutator to allocate while the helpers mark.
pub(super) fn concurrent_marking_start(live: u64, limit: u64) -> u64 {
    live.saturating_add(limit.saturating_sub(live) / 8 * 7)
}

impl GcHeap {
    /// Seed only the default initial budget from a completed evacuation.
    /// Old occupancy includes ordinary and large promotions already; only
    /// copied cells remain outside it. Nursery growth precedes this commit.
    pub(super) fn seed_initial_old_generation_limit(&mut self, stats: &ScavengeStats) {
        if !std::mem::take(&mut self.initial_old_limit_pending) {
            return;
        }
        let old_after = self.old_generation_consumed_bytes();
        let working = old_after.saturating_add(stats.copied_bytes as u64);
        let candidate = bounded_limit(
            working,
            dynamic_growing_factor(0.0, 0.0),
            self.new_space.capacity_bytes() as u64,
        );
        self.old_generation_limit = self.old_generation_limit.min(candidate);
        self.concurrent_marking_start =
            concurrent_marking_start(old_after, self.old_generation_limit);
    }

    /// Recompute the old-generation and external limits after a
    /// mark-sweep.
    ///
    /// The collection speed is the old generation it processed per
    /// millisecond of this pause; the mutator speed is the old generation
    /// allocated since the previous mark-sweep per millisecond of mutator
    /// time, scavenge pauses excluded. The limit is the surviving cells'
    /// bytes times their growing factor, at least the minimum growing step
    /// above them, plus the nursery's capacity, bounded below by
    /// [`MIN_OLD_GENERATION_SIZE`] and above by the cage ceiling, where a
    /// legitimately large live set settles instead of collecting on every
    /// allocation.
    pub(super) fn recompute_allocation_limits(
        &mut self,
        pause_start: Instant,
        old_generation_before: u64,
    ) {
        self.initial_old_limit_pending = false;
        let now = Instant::now();
        let live = self.old_generation_consumed_bytes();
        let pause_ms = now.duration_since(pause_start).as_secs_f64() * 1000.0;
        let minor_ms = self
            .gc_stats
            .minor_pause_ns_total
            .saturating_sub(self.minor_pause_ns_at_last_major_gc) as f64
            / 1.0e6;
        let mutator_ms = (pause_start
            .duration_since(self.last_major_gc_end)
            .as_secs_f64()
            * 1000.0
            - minor_ms)
            .max(0.0);
        let allocated = old_generation_before.saturating_sub(self.old_generation_bytes_at_last_gc);
        let gc_speed = if pause_ms > 0.0 {
            old_generation_before as f64 / pause_ms
        } else {
            0.0
        };
        let mutator_speed = if mutator_ms > 0.0 {
            allocated as f64 / mutator_ms
        } else {
            0.0
        };
        let factor = dynamic_growing_factor(gc_speed, mutator_speed);
        let new_space_capacity =
            (self.new_space.from_pages().len() * crate::page::PAGE_SIZE) as u64;
        self.old_generation_limit = bounded_limit(live, factor, new_space_capacity);
        self.concurrent_marking_start = concurrent_marking_start(live, self.old_generation_limit);
        // The completed sweep has already reconciled released backing
        // stores; only the surviving reservations remain.
        self.external_limit = self
            .reserved_bytes
            .saturating_add(EXTERNAL_ALLOCATION_SOFT_LIMIT);
        self.old_generation_bytes_at_last_gc = live;
        self.last_major_gc_end = now;
        self.minor_pause_ns_at_last_major_gc = self.gc_stats.minor_pause_ns_total;
    }

    /// Set the old-generation allocation limit that holds until the first
    /// mark-sweep. Embedders with small heaps lower it from
    /// [`INITIAL_OLD_GENERATION_SIZE`]; it has no effect once a mark-sweep
    /// has recomputed the limits.
    pub fn set_initial_old_generation_size(&mut self, bytes: u64) {
        if self.gc_stats.gc_cycles == 0 {
            self.initial_old_limit_pending = false;
            self.old_generation_limit = bytes.min(cage_hardcap_bytes());
            self.concurrent_marking_start = concurrent_marking_start(0, self.old_generation_limit);
        }
    }
}

#[cfg(test)]
#[path = "allocation_limits_tests.rs"]
mod tests;
