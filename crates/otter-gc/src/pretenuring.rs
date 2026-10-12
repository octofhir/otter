//! Allocation-site survival feedback and pretenuring decisions.
//!
//! # Contents
//! - [`AllocationSites`] — the isolate's site records: per-site young cells
//!   created and found surviving since the last scavenge, and the decision.
//! - [`SiteRecord`] — one address-stable record generated code updates.
//! - [`PretenureDecision`] — the per-site placement state.
//!
//! # Invariants
//! - Site ids are dense, nonzero and at most `u16::MAX`; zero names no site.
//!   A young cell's header body bytes carry its site id until its first
//!   evacuation clears them, so each cell is found at most once.
//! - Records live in fixed chunks and never move: generated code may bake a
//!   record address for the heap's lifetime.
//! - A scavenge digests every record: a site that created at least
//!   [`MINIMUM_CREATED`] cells decides from its survival ratio, then all
//!   counters restart. Survival of at least [`PRETENURE_RATIO`] tenures the
//!   site once the nursery already reached its maximum size; below that the
//!   site waits as `MaybeTenure`. Lower survival never tenures.
//! - `Tenure` and `DontTenure` are final, and only they have bit 1 set:
//!   tracking code tests that bit alone to leave a decided site.
//!
//! # See also
//! - [`crate::scavenger`] counts found cells and clears their site tags.
//! - [`crate::heap::GcHeap::allocation_sites_mut`] owns the table.

/// Smallest number of cells created between scavenges that lets a site decide.
pub const MINIMUM_CREATED: u32 = 100;

/// Found/created ratio at or above which a site's cells are allocated old.
pub const PRETENURE_RATIO: f64 = 0.85;

/// Records per address-stable chunk.
const CHUNK: usize = 256;

/// Placement state of one allocation site.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PretenureDecision {
    /// Too little feedback yet; cells are tagged and counted.
    Undecided = 0,
    /// High survival seen before the nursery reached its maximum size.
    MaybeTenure = 1,
    /// Final: cells are allocated directly in old space.
    Tenure = 2,
    /// Final: cells are allocated young without tags.
    DontTenure = 3,
}

/// Bit that only the final decisions carry.
pub const DECIDED_BIT: u8 = 0b10;

impl PretenureDecision {
    fn from_byte(byte: u8) -> Self {
        match byte {
            1 => Self::MaybeTenure,
            2 => Self::Tenure,
            3 => Self::DontTenure,
            _ => Self::Undecided,
        }
    }

    /// Whether cells of this site are still tagged and counted.
    #[must_use]
    pub const fn tracks(self) -> bool {
        (self as u8) & DECIDED_BIT == 0
    }
}

/// One site's counters and decision, at a fixed address.
#[repr(C)]
#[derive(Debug, Default)]
pub struct SiteRecord {
    /// Young cells created since the last scavenge.
    pub created: u32,
    /// Of those, cells the last scavenge found alive.
    pub found: u32,
    /// [`PretenureDecision`] as a byte.
    pub decision: u8,
}

/// Byte offset of [`SiteRecord::created`].
pub const SITE_CREATED_OFFSET: usize = std::mem::offset_of!(SiteRecord, created);
/// Byte offset of [`SiteRecord::decision`].
pub const SITE_DECISION_OFFSET: usize = std::mem::offset_of!(SiteRecord, decision);

/// Concurrent access to the found counters of an [`AllocationSites`] table
/// during one scavenge, which keeps the table borrowed.
pub(crate) struct FoundCounters {
    chunks: Vec<*mut SiteRecord>,
    len: u16,
}

// SAFETY: the chunk pointers name records that never move while the table
// is borrowed for the scavenge, and `record_found` touches only their
// `found` words, atomically.
unsafe impl Sync for FoundCounters {}

impl FoundCounters {
    /// Count one cell of `site` found alive.
    pub(crate) fn record_found(&self, site: u16) {
        if site == 0 || site > self.len {
            return;
        }
        let index = usize::from(site);
        // SAFETY: `site` is registered, so its chunk exists; the record stays
        // in place for the borrow, and its `found` word is 4-aligned.
        unsafe {
            let record = self.chunks[index / CHUNK].add(index % CHUNK);
            let found = std::ptr::addr_of_mut!((*record).found);
            std::sync::atomic::AtomicU32::from_ptr(found)
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// The isolate's allocation sites.
#[derive(Debug, Default)]
pub struct AllocationSites {
    chunks: Vec<Box<[SiteRecord]>>,
    len: u16,
}

impl AllocationSites {
    /// Register a new site; `0` once every id is taken.
    pub fn register(&mut self) -> u16 {
        if self.len == u16::MAX {
            return 0;
        }
        self.len += 1;
        let index = usize::from(self.len);
        if index / CHUNK >= self.chunks.len() {
            self.chunks
                .push((0..CHUNK).map(|_| SiteRecord::default()).collect());
        }
        self.len
    }

    /// Whether `site` names a registered site.
    #[must_use]
    pub fn contains(&self, site: u16) -> bool {
        site != 0 && site <= self.len
    }

    fn record(&self, site: u16) -> &SiteRecord {
        let index = usize::from(site);
        &self.chunks[index / CHUNK][index % CHUNK]
    }

    fn record_mut(&mut self, site: u16) -> &mut SiteRecord {
        let index = usize::from(site);
        &mut self.chunks[index / CHUNK][index % CHUNK]
    }

    /// Address of `site`'s record, stable for the table's lifetime.
    #[must_use]
    pub fn record_address(&self, site: u16) -> u64 {
        debug_assert!(self.contains(site));
        std::ptr::from_ref(self.record(site)) as u64
    }

    /// Current decision of a registered site.
    #[must_use]
    pub fn decision(&self, site: u16) -> PretenureDecision {
        PretenureDecision::from_byte(self.record(site).decision)
    }

    /// Count one young cell created at a tracking site.
    pub fn record_created(&mut self, site: u16) {
        let record = self.record_mut(site);
        record.created = record.created.saturating_add(1);
    }

    /// Count one tagged cell the current scavenge found alive.
    #[inline]
    pub fn record_found(&mut self, site: u16) {
        if self.contains(site) {
            let record = self.record_mut(site);
            record.found = record.found.saturating_add(1);
        }
    }

    /// The found counters of every registered site, for scavenger threads
    /// that count evacuated cells concurrently.
    pub(crate) fn found_counters(&mut self) -> FoundCounters {
        FoundCounters {
            chunks: self
                .chunks
                .iter_mut()
                .map(|chunk| chunk.as_mut_ptr())
                .collect(),
            len: self.len,
        }
    }

    /// Decide every site with enough feedback, then restart all counters.
    /// `nursery_at_maximum` reports whether this scavenge ran with the
    /// nursery at its largest size. Returns how many sites became final.
    pub fn digest(&mut self, nursery_at_maximum: bool) -> usize {
        let mut decided = 0;
        for site in 1..=self.len {
            let record = self.record_mut(site);
            let decision = PretenureDecision::from_byte(record.decision);
            if decision.tracks() && record.created >= MINIMUM_CREATED {
                let ratio = f64::from(record.found) / f64::from(record.created);
                let next = if ratio < PRETENURE_RATIO {
                    PretenureDecision::DontTenure
                } else if nursery_at_maximum {
                    PretenureDecision::Tenure
                } else {
                    PretenureDecision::MaybeTenure
                };
                decided += usize::from(!next.tracks());
                record.decision = next as u8;
            }
            record.created = 0;
            record.found = 0;
        }
        decided
    }

    /// Install a decision for isolated fixtures; production decisions come
    /// only from [`Self::digest`].
    #[doc(hidden)]
    pub fn set_decision_for_test(&mut self, site: u16, decision: PretenureDecision) {
        self.record_mut(site).decision = decision as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_stay_at_stable_addresses_across_growth() {
        let mut sites = AllocationSites::default();
        let first = sites.register();
        let address = sites.record_address(first);
        for _ in 0..1000 {
            sites.register();
        }
        assert_eq!(sites.record_address(first), address);
        assert_eq!(first, 1);
        assert!(sites.contains(1001) && !sites.contains(1002) && !sites.contains(0));
    }

    #[test]
    fn digest_decides_from_survival_and_restarts_counters() {
        let mut sites = AllocationSites::default();
        let [low, high, early, few] = [(); 4].map(|()| sites.register());
        for (site, created, found) in [
            (low, 200, 100),
            (high, 200, 180),
            (early, 200, 199),
            (few, 99, 99),
        ] {
            for _ in 0..created {
                sites.record_created(site);
            }
            for _ in 0..found {
                sites.record_found(site);
            }
        }
        // `early` reaches the threshold while the nursery still grows.
        assert_eq!(sites.digest(false), 1);
        assert_eq!(sites.decision(low), PretenureDecision::DontTenure);
        assert_eq!(sites.decision(high), PretenureDecision::MaybeTenure);
        assert_eq!(sites.decision(early), PretenureDecision::MaybeTenure);
        assert_eq!(sites.decision(few), PretenureDecision::Undecided);
        assert!(
            (1..=4).all(|site| sites.record(site).created == 0 && sites.record(site).found == 0)
        );
        for _ in 0..100 {
            sites.record_created(high);
            sites.record_found(high);
            sites.record_created(low);
        }
        assert_eq!(sites.digest(true), 1);
        assert_eq!(sites.decision(high), PretenureDecision::Tenure);
        assert_eq!(sites.decision(low), PretenureDecision::DontTenure, "final");
    }

    #[test]
    fn only_final_decisions_carry_the_decided_bit() {
        for (decision, tracks) in [
            (PretenureDecision::Undecided, true),
            (PretenureDecision::MaybeTenure, true),
            (PretenureDecision::Tenure, false),
            (PretenureDecision::DontTenure, false),
        ] {
            assert_eq!(decision.tracks(), tracks);
            assert_eq!((decision as u8) & DECIDED_BIT == 0, tracks);
        }
        assert_eq!(SITE_CREATED_OFFSET, 0);
        assert_eq!(SITE_DECISION_OFFSET, 8);
    }

    #[test]
    fn site_ids_saturate_without_reuse() {
        let mut sites = AllocationSites {
            chunks: Vec::new(),
            len: u16::MAX - 1,
        };
        sites
            .chunks
            .resize_with(usize::from(u16::MAX) / CHUNK + 1, || {
                (0..CHUNK).map(|_| SiteRecord::default()).collect()
            });
        assert_eq!(sites.register(), u16::MAX);
        assert_eq!(sites.register(), 0);
        sites.record_found(0);
    }
}
