//! Shared fixed-array ledger and RAII accounting guards.
//!
//! # Contents
//! - [`ResourceAccount`] owns the shared counters.
//! - [`ResourceReservation`] represents provisional charged capacity.
//! - [`ResourceLease`] represents exact retained usage.
//! - [`ResourceLeaseSet`] represents an atomic set of exact retained charges.
//!
//! # Invariants
//! - All counter checks and mutations occur under the ledger's single mutex.
//! - Every guard releases exactly the amount it currently owns on drop.
//! - Multi-class reservations aggregate duplicate classes, preflight every
//!   class, and publish all counters together or none of them.
//! - Failed reservation replacement removes its provisional charge atomically.
//! - Failed lease replacement preserves its prior exact charge atomically.
//! - Mutex poisoning is cleared while preserving the protected state.
//!
//! # See also
//! - [`crate::ResourceLimits`] configures caps.
//! - [`crate::ResourceSnapshot`] exposes copied usage statistics.

use std::array;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::class::{RESOURCE_CLASS_COUNT, ResourceClass};
use crate::{ResourceError, ResourceLimits, ResourceSnapshot, ResourceSnapshotEntry};

#[derive(Debug, Clone, Copy)]
struct ClassState {
    current: u64,
    peak: u64,
    rejections: u64,
    limit: Option<u64>,
}

type LedgerState = [ClassState; RESOURCE_CLASS_COUNT];
type SharedState = Arc<Mutex<LedgerState>>;

/// A cloneable handle to one shared resource ledger.
///
/// Clones aggregate into the same counters. Each successful call to
/// [`ResourceAccount::reserve`], [`ResourceAccount::reserve_exact`], or
/// [`ResourceAccount::reserve_exact_many`] updates the relevant counters while
/// holding the ledger mutex, before returning the RAII guard to the caller.
#[derive(Debug, Clone)]
pub struct ResourceAccount {
    shared: SharedState,
}

impl ResourceAccount {
    /// Create an empty account with the supplied immutable limits.
    #[must_use]
    pub fn new(limits: ResourceLimits) -> Self {
        let state = array::from_fn(|index| ClassState {
            current: 0,
            peak: 0,
            rejections: 0,
            limit: limits.limits[index],
        });
        Self {
            shared: Arc::new(Mutex::new(state)),
        }
    }

    /// Return the immutable limits installed when this account was created.
    #[must_use]
    pub fn limits(&self) -> ResourceLimits {
        let state = lock_state(&self.shared);
        ResourceLimits {
            limits: array::from_fn(|index| state[index].limit),
        }
    }

    /// Atomically charge a provisional amount before a guarded effect begins.
    ///
    /// Dropping the returned reservation rolls back the entire amount. Use
    /// [`ResourceReservation::commit_exact`] after the effect to retain its
    /// actual resource usage in a [`ResourceLease`].
    pub fn reserve(
        &self,
        class: ResourceClass,
        requested: u64,
    ) -> Result<ResourceReservation, ResourceError> {
        reserve_increment(&self.shared, class, requested)?;
        Ok(ResourceReservation {
            shared: Some(Arc::clone(&self.shared)),
            class,
            amount: requested,
        })
    }

    /// Atomically charge an exact amount and return its RAII lease.
    ///
    /// This is the direct convenience path when no estimate/commit phase is
    /// needed. Dropping the returned lease releases the amount.
    pub fn reserve_exact(
        &self,
        class: ResourceClass,
        requested: u64,
    ) -> Result<ResourceLease, ResourceError> {
        reserve_increment(&self.shared, class, requested)?;
        Ok(ResourceLease {
            shared: Arc::clone(&self.shared),
            class,
            amount: requested,
        })
    }

    /// Atomically charge exact amounts across multiple resource classes.
    ///
    /// Duplicate classes are aggregated with checked arithmetic. Every class
    /// is preflighted in stable [`ResourceClass::ALL`] order while the ledger
    /// mutex is held. If any aggregate overflows or any resulting counter
    /// would exceed its limit, no current or peak counter is changed; only the
    /// rejection counter for the deterministically selected failing class is
    /// incremented. Zero-valued entries have no effect.
    ///
    /// Dropping the returned non-cloneable [`ResourceLeaseSet`] releases every
    /// retained amount together under one ledger lock.
    pub fn reserve_exact_many(
        &self,
        requests: &[(ResourceClass, u64)],
    ) -> Result<ResourceLeaseSet, ResourceError> {
        let (amounts, aggregation_overflows) = aggregate_requests(requests);
        let mut state = lock_state(&self.shared);
        let mut next_currents = [0; RESOURCE_CLASS_COUNT];

        for class in ResourceClass::ALL {
            let index = class.index();
            let class_state = &mut state[index];

            if let Some((requested, aggregated)) = aggregation_overflows[index] {
                return Err(reject(class_state, class, requested, aggregated));
            }

            let requested = amounts[index];
            let in_use = class_state.current;
            let Some(next) = class_state.current.checked_add(requested) else {
                return Err(reject(class_state, class, requested, in_use));
            };
            if class_state.limit.is_some_and(|limit| next > limit) {
                return Err(reject(class_state, class, requested, in_use));
            }
            next_currents[index] = next;
        }

        for class in ResourceClass::ALL {
            let index = class.index();
            let class_state = &mut state[index];
            let next = next_currents[index];
            class_state.current = next;
            class_state.peak = class_state.peak.max(next);
        }
        drop(state);

        Ok(ResourceLeaseSet {
            shared: Arc::clone(&self.shared),
            amounts,
        })
    }

    /// Capture current usage and cumulative statistics in stable class order.
    #[must_use]
    pub fn snapshot(&self) -> ResourceSnapshot {
        let state = lock_state(&self.shared);
        let entries = ResourceClass::ALL.map(|class| {
            let class_state = state[class.index()];
            ResourceSnapshotEntry::new(
                class,
                class_state.current,
                class_state.peak,
                class_state.rejections,
                class_state.limit,
            )
        });
        ResourceSnapshot::new(entries)
    }

    #[cfg(test)]
    pub(crate) fn poison_for_test(&self) {
        let _state = self.shared.lock().unwrap();
        panic!("poison ledger for recovery test");
    }
}

impl Default for ResourceAccount {
    fn default() -> Self {
        Self::new(ResourceLimits::default())
    }
}

/// A provisional, already-charged resource reservation.
///
/// The reservation is intentionally not cloneable. Dropping it releases the
/// provisional amount. Committing it consumes the reservation and returns a
/// non-cloneable [`ResourceLease`] for the exact amount.
#[must_use = "dropping the reservation immediately rolls its resource charge back"]
pub struct ResourceReservation {
    shared: Option<SharedState>,
    class: ResourceClass,
    amount: u64,
}

impl ResourceReservation {
    /// Return the resource class charged by this reservation.
    #[must_use]
    pub const fn class(&self) -> ResourceClass {
        self.class
    }

    /// Return the provisional charged amount.
    #[must_use]
    pub const fn amount(&self) -> u64 {
        self.amount
    }

    /// Replace the provisional charge with `actual` and return an exact lease.
    ///
    /// Shrinking releases the difference. Growing atomically checks the exact
    /// total against overflow and the class limit. If growth fails, the entire
    /// provisional charge is rolled back before the error is returned.
    pub fn commit_exact(mut self, actual: u64) -> Result<ResourceLease, ResourceError> {
        let shared = self
            .shared
            .as_ref()
            .expect("a live reservation always owns shared ledger state");
        let result = replace_reservation(shared, self.class, self.amount, actual);

        // `replace_reservation` either installed `actual` or rolled back the
        // provisional charge, so this reservation must never release it again.
        self.amount = 0;
        result?;

        let shared = self
            .shared
            .take()
            .expect("a live reservation always owns shared ledger state");
        Ok(ResourceLease {
            shared,
            class: self.class,
            amount: actual,
        })
    }
}

impl fmt::Debug for ResourceReservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResourceReservation")
            .field("class", &self.class)
            .field("amount", &self.amount)
            .finish_non_exhaustive()
    }
}

impl Drop for ResourceReservation {
    fn drop(&mut self) {
        if self.amount == 0 {
            return;
        }
        if let Some(shared) = &self.shared {
            release(shared, self.class, self.amount);
        }
    }
}

/// An exact, already-charged resource lease.
///
/// The lease is intentionally not cloneable. Dropping it releases its full
/// amount from the shared account.
#[must_use = "the resource remains charged only while this lease is retained"]
pub struct ResourceLease {
    shared: SharedState,
    class: ResourceClass,
    amount: u64,
}

impl ResourceLease {
    /// Return the resource class charged by this lease.
    #[must_use]
    pub const fn class(&self) -> ResourceClass {
        self.class
    }

    /// Return the exact charged amount.
    #[must_use]
    pub const fn amount(&self) -> u64 {
        self.amount
    }

    /// Atomically replace this lease's charge with `new_amount`.
    ///
    /// Shrinking releases the difference. Growing checks the replacement
    /// total before changing the ledger. Unlike a provisional reservation
    /// commit, a failed resize preserves both the old charge and this lease,
    /// so the already-published resource remains accounted for.
    ///
    /// # Errors
    /// Returns [`ResourceError`] when the replacement would overflow or exceed
    /// the class limit. The lease and ledger are unchanged on failure.
    pub fn resize(&mut self, new_amount: u64) -> Result<(), ResourceError> {
        if new_amount == self.amount {
            return Ok(());
        }
        replace_lease(&self.shared, self.class, self.amount, new_amount)?;
        self.amount = new_amount;
        Ok(())
    }
}

impl fmt::Debug for ResourceLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResourceLease")
            .field("class", &self.class)
            .field("amount", &self.amount)
            .finish_non_exhaustive()
    }
}

impl Drop for ResourceLease {
    fn drop(&mut self) {
        release(&self.shared, self.class, self.amount);
    }
}

/// An atomic set of exact, already-charged resource amounts.
///
/// The set is intentionally not cloneable. Amounts are stored in stable
/// [`ResourceClass::ALL`] order, and dropping the set releases all non-zero
/// amounts together under the shared ledger's single mutex.
#[must_use = "the resources remain charged only while this lease set is retained"]
pub struct ResourceLeaseSet {
    shared: SharedState,
    amounts: [u64; RESOURCE_CLASS_COUNT],
}

impl ResourceLeaseSet {
    /// Return the exact charged amount owned for `class`.
    #[must_use]
    pub const fn amount(&self, class: ResourceClass) -> u64 {
        self.amounts[class.index()]
    }

    /// Return whether the set owns no non-zero resource charge.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.amounts.iter().all(|amount| *amount == 0)
    }

    /// Move one class charge out of this aggregate into an independent lease.
    ///
    /// The ledger is not mutated: ownership of the already-published charge is
    /// transferred from this set to the returned guard. This is useful when an
    /// operation was admitted atomically across several classes but those
    /// classes have different release points.
    #[must_use = "the extracted charge remains retained only while the lease is held"]
    pub fn take(&mut self, class: ResourceClass) -> Option<ResourceLease> {
        let amount = std::mem::take(&mut self.amounts[class.index()]);
        (amount != 0).then(|| ResourceLease {
            shared: Arc::clone(&self.shared),
            class,
            amount,
        })
    }
}

impl fmt::Debug for ResourceLeaseSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResourceLeaseSet")
            .field("amounts", &LeaseSetAmounts(&self.amounts))
            .finish_non_exhaustive()
    }
}

impl Drop for ResourceLeaseSet {
    fn drop(&mut self) {
        release_many(&self.shared, &self.amounts);
    }
}

struct LeaseSetAmounts<'a>(&'a [u64; RESOURCE_CLASS_COUNT]);

impl fmt::Debug for LeaseSetAmounts<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut amounts = formatter.debug_map();
        for class in ResourceClass::ALL {
            let amount = self.0[class.index()];
            if amount != 0 {
                amounts.entry(&class, &amount);
            }
        }
        amounts.finish()
    }
}

fn lock_state(shared: &SharedState) -> MutexGuard<'_, LedgerState> {
    match shared.lock() {
        Ok(state) => state,
        Err(poisoned) => {
            // Retain all accounting state. Poison signals a prior panic, not a
            // broken numeric invariant, and callers must still be able to
            // release already-held resources while unwinding or recovering.
            shared.clear_poison();
            poisoned.into_inner()
        }
    }
}

fn reject(
    state: &mut ClassState,
    class: ResourceClass,
    requested: u64,
    in_use: u64,
) -> ResourceError {
    state.rejections = state.rejections.saturating_add(1);
    match in_use.checked_add(requested) {
        None => ResourceError::Overflow {
            class,
            requested,
            in_use,
            limit: state.limit,
        },
        Some(_) => ResourceError::Exhausted {
            class,
            requested,
            in_use,
            limit: state
                .limit
                .expect("only a finite limit can reject non-overflowing usage"),
        },
    }
}

fn reserve_increment(
    shared: &SharedState,
    class: ResourceClass,
    requested: u64,
) -> Result<(), ResourceError> {
    let mut state = lock_state(shared);
    let class_state = &mut state[class.index()];
    let in_use = class_state.current;
    let Some(next) = class_state.current.checked_add(requested) else {
        return Err(reject(class_state, class, requested, in_use));
    };
    if class_state.limit.is_some_and(|limit| next > limit) {
        return Err(reject(class_state, class, requested, in_use));
    }
    class_state.current = next;
    class_state.peak = class_state.peak.max(next);
    Ok(())
}

fn aggregate_requests(
    requests: &[(ResourceClass, u64)],
) -> (
    [u64; RESOURCE_CLASS_COUNT],
    [Option<(u64, u64)>; RESOURCE_CLASS_COUNT],
) {
    let mut amounts = [0_u64; RESOURCE_CLASS_COUNT];
    let mut overflows = [None; RESOURCE_CLASS_COUNT];

    for &(class, requested) in requests {
        let index = class.index();
        if overflows[index].is_some() {
            continue;
        }
        match amounts[index].checked_add(requested) {
            Some(aggregated) => amounts[index] = aggregated,
            None => overflows[index] = Some((requested, amounts[index])),
        }
    }

    (amounts, overflows)
}

fn replace_reservation(
    shared: &SharedState,
    class: ResourceClass,
    reserved: u64,
    actual: u64,
) -> Result<(), ResourceError> {
    let mut state = lock_state(shared);
    let class_state = &mut state[class.index()];
    debug_assert!(class_state.current >= reserved);
    let in_use = class_state.current.saturating_sub(reserved);

    let Some(next) = in_use.checked_add(actual) else {
        class_state.current = in_use;
        return Err(reject(class_state, class, actual, in_use));
    };
    if class_state.limit.is_some_and(|limit| next > limit) {
        class_state.current = in_use;
        return Err(reject(class_state, class, actual, in_use));
    }

    class_state.current = next;
    class_state.peak = class_state.peak.max(next);
    Ok(())
}

fn replace_lease(
    shared: &SharedState,
    class: ResourceClass,
    old_amount: u64,
    new_amount: u64,
) -> Result<(), ResourceError> {
    let mut state = lock_state(shared);
    let class_state = &mut state[class.index()];
    debug_assert!(class_state.current >= old_amount);
    let independently_in_use = class_state.current.saturating_sub(old_amount);

    let Some(next) = independently_in_use.checked_add(new_amount) else {
        return Err(reject(class_state, class, new_amount, independently_in_use));
    };
    if class_state.limit.is_some_and(|limit| next > limit) {
        return Err(reject(class_state, class, new_amount, independently_in_use));
    }

    class_state.current = next;
    class_state.peak = class_state.peak.max(next);
    Ok(())
}

fn release(shared: &SharedState, class: ResourceClass, amount: u64) {
    let mut state = lock_state(shared);
    let class_state = &mut state[class.index()];
    debug_assert!(class_state.current >= amount);
    class_state.current = class_state.current.saturating_sub(amount);
}

fn release_many(shared: &SharedState, amounts: &[u64; RESOURCE_CLASS_COUNT]) {
    if amounts.iter().all(|amount| *amount == 0) {
        return;
    }

    let mut state = lock_state(shared);
    for class in ResourceClass::ALL {
        let index = class.index();
        let amount = amounts[index];
        let class_state = &mut state[index];
        debug_assert!(class_state.current >= amount);
        class_state.current = class_state.current.saturating_sub(amount);
    }
}
