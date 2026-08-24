//! Bounded admission for runtime-owned terminal completions.
//!
//! # Contents
//! - [`CompletionAdmissionPool`] owns one finite per-isolate slot pool.
//! - [`CompletionAdmission`] is the unique pre-effect credit carried until
//!   dispatch.
//! - [`ActiveCompletion`] retains only the origin resource while dispatch runs.
//!
//! # Invariants
//! - Hard slots are finite even when the shared resource ledger is unlimited.
//! - `QueuedTasks` and the origin class are admitted atomically before an
//!   observable host effect begins.
//! - Admission guards are unique and non-cloneable. Dropping them releases
//!   every hard and ledger charge exactly once.
//! - Beginning dispatch releases the hard backlog slot and `QueuedTasks` while
//!   retaining the origin resource until dispatch completes.
//!
//! # See also
//! - [`crate::handle`] carries admitted completions through its wakeable inbox.
//! - [`otter_resource::ResourceAccount`] owns aggregate resource accounting.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{DiagnosticCode, OtterError, ResourceAccount, ResourceClass, ResourceLease};

/// Finite default number of runtime-owned completions that may be outstanding
/// in one isolate, including operations whose terminal payload is still being
/// produced off-isolate.
pub const DEFAULT_GUARANTEED_COMPLETION_CAPACITY: usize = 1_024;
/// Finite default number of in-flight host operations in one isolate.
pub const DEFAULT_HOST_OPERATION_CAPACITY: usize = 1_024;
/// Finite default number of live timers in one isolate.
pub const DEFAULT_TIMER_CAPACITY: usize = 4_096;

/// Independent physical capacities for queue credits and their origins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompletionCapacities {
    pub(crate) guaranteed: usize,
    pub(crate) host_operations: usize,
    pub(crate) timers: usize,
}

/// Resource origin paired atomically with one reserved terminal task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionOrigin {
    HostOperation,
    OneShotTimer,
}

impl CompletionOrigin {
    const fn resource_class(self) -> ResourceClass {
        match self {
            Self::HostOperation => ResourceClass::HostOperations,
            Self::OneShotTimer => ResourceClass::Timers,
        }
    }
}

/// Per-isolate hard admission pool backed by the shared aggregate ledger.
#[derive(Clone)]
pub(crate) struct CompletionAdmissionPool {
    guaranteed_slots: Arc<Semaphore>,
    host_operation_slots: Arc<Semaphore>,
    timer_slots: Arc<Semaphore>,
    resources: ResourceAccount,
    closed: Arc<AtomicBool>,
}

impl CompletionAdmissionPool {
    pub(crate) fn new(resources: ResourceAccount, capacities: CompletionCapacities) -> Self {
        Self {
            guaranteed_slots: Arc::new(Semaphore::new(capacities.guaranteed)),
            host_operation_slots: Arc::new(Semaphore::new(capacities.host_operations)),
            timer_slots: Arc::new(Semaphore::new(capacities.timers)),
            resources,
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    fn ensure_open(&self) -> Result<(), OtterError> {
        if self.closed.load(Ordering::Acquire) {
            Err(OtterError::Internal {
                code: DiagnosticCode::RuntimeShutdown.as_str().to_string(),
                message: "runtime completion admission is closed".to_string(),
            })
        } else {
            Ok(())
        }
    }

    /// Reserve a hard slot and the complete ledger tuple before the producer
    /// allocates or publishes terminal completion state.
    pub(crate) fn admit(
        &self,
        origin: CompletionOrigin,
    ) -> Result<CompletionAdmission, OtterError> {
        self.ensure_open()?;
        let hard_permit = self
            .guaranteed_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| OtterError::Internal {
                code: DiagnosticCode::RuntimeBackpressure.as_str().to_string(),
                message: "runtime guaranteed-completion capacity is exhausted".to_string(),
            })?;
        let origin_class = origin.resource_class();
        let origin_permit = self.acquire_origin(origin)?;
        let mut leases = self
            .resources
            .reserve_exact_many(&[(ResourceClass::QueuedTasks, 1), (origin_class, 1)])?;
        let queued_task = leases
            .take(ResourceClass::QueuedTasks)
            .expect("non-zero queued-task admission owns a lease");
        let origin = leases
            .take(origin_class)
            .expect("non-zero completion-origin admission owns a lease");
        debug_assert!(leases.is_empty());
        drop(leases);
        self.ensure_open()?;
        Ok(CompletionAdmission {
            hard_permit: Some(hard_permit),
            queued_task: Some(queued_task),
            origin: Some(ActiveCompletion {
                owner: self.closed.clone(),
                _hard_permit: origin_permit,
                origin,
            }),
        })
    }

    /// Reserve a live resource that never enters the guaranteed overflow FIFO.
    /// Repeating timer ticks use this path because they are coalesced under
    /// pressure and therefore cannot accumulate terminal messages.
    pub(crate) fn admit_origin_only(
        &self,
        class: ResourceClass,
    ) -> Result<ActiveCompletion, OtterError> {
        self.ensure_open()?;
        let origin = match class {
            ResourceClass::HostOperations => CompletionOrigin::HostOperation,
            ResourceClass::Timers => CompletionOrigin::OneShotTimer,
            _ => {
                return Err(OtterError::Internal {
                    code: DiagnosticCode::RuntimeBackpressure.as_str().to_string(),
                    message: format!("{class} is not a completion-origin resource"),
                });
            }
        };
        let hard_permit = self.acquire_origin(origin)?;
        let origin = self.resources.reserve_exact(class, 1)?;
        self.ensure_open()?;
        Ok(ActiveCompletion {
            owner: self.closed.clone(),
            _hard_permit: hard_permit,
            origin,
        })
    }

    fn acquire_origin(&self, origin: CompletionOrigin) -> Result<OwnedSemaphorePermit, OtterError> {
        let (slots, label) = match origin {
            CompletionOrigin::HostOperation => (&self.host_operation_slots, "host-operation"),
            CompletionOrigin::OneShotTimer => (&self.timer_slots, "timer"),
        };
        slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| OtterError::Internal {
                code: DiagnosticCode::RuntimeBackpressure.as_str().to_string(),
                message: format!("runtime {label} capacity is exhausted"),
            })
    }

    #[cfg(test)]
    pub(crate) fn available_slots(&self) -> usize {
        self.guaranteed_slots.available_permits()
    }
}

/// Unique credit for one guaranteed terminal completion.
pub(crate) struct CompletionAdmission {
    hard_permit: Option<OwnedSemaphorePermit>,
    queued_task: Option<ResourceLease>,
    origin: Option<ActiveCompletion>,
}

impl CompletionAdmission {
    /// Whether this carrier was issued by this exact per-isolate pool.
    pub(crate) fn belongs_to(&self, pool: &CompletionAdmissionPool) -> bool {
        self.origin
            .as_ref()
            .is_some_and(|origin| origin.belongs_to(pool))
    }

    /// Mark the admitted completion as dispatched. Queue retention ends here;
    /// the origin resource remains live until the returned guard is dropped.
    pub(crate) fn begin_dispatch(mut self) -> ActiveCompletion {
        drop(self.hard_permit.take());
        drop(self.queued_task.take());
        self.origin
            .take()
            .expect("admitted completion owns its origin lease")
    }
}

impl std::fmt::Debug for CompletionAdmission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CompletionAdmission")
            .field("hard_slot", &self.hard_permit.is_some())
            .field(
                "queued_task",
                &self.queued_task.as_ref().map(ResourceLease::amount),
            )
            .field(
                "origin",
                &self.origin.as_ref().map(|origin| origin.origin.class()),
            )
            .finish()
    }
}

/// Origin resource retained while the admitted terminal task dispatches.
pub(crate) struct ActiveCompletion {
    owner: Arc<AtomicBool>,
    _hard_permit: OwnedSemaphorePermit,
    origin: ResourceLease,
}

impl ActiveCompletion {
    /// Whether this live-origin carrier was issued by this exact isolate pool.
    pub(crate) fn belongs_to(&self, pool: &CompletionAdmissionPool) -> bool {
        Arc::ptr_eq(&self.owner, &pool.closed)
    }
}

impl std::fmt::Debug for ActiveCompletion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ActiveCompletion")
            .field("origin", &self.origin.class())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ResourceError, ResourceLimits};

    fn capacities(
        guaranteed: usize,
        host_operations: usize,
        timers: usize,
    ) -> CompletionCapacities {
        CompletionCapacities {
            guaranteed,
            host_operations,
            timers,
        }
    }

    #[test]
    fn zero_capacity_is_an_intentional_fail_closed_mode() {
        let account = ResourceAccount::default();
        let pool = CompletionAdmissionPool::new(account.clone(), capacities(0, 1, 1));

        assert!(pool.admit(CompletionOrigin::HostOperation).is_err());
        assert_eq!(
            account.snapshot().get(ResourceClass::QueuedTasks).current(),
            0
        );
        assert_eq!(
            account
                .snapshot()
                .get(ResourceClass::HostOperations)
                .current(),
            0
        );
    }

    #[test]
    fn failed_origin_permit_rolls_back_the_guaranteed_slot() {
        let pool = CompletionAdmissionPool::new(ResourceAccount::default(), capacities(1, 0, 1));

        assert!(pool.admit(CompletionOrigin::HostOperation).is_err());
        assert_eq!(pool.available_slots(), 1);
    }

    #[test]
    fn ledger_tuple_rejection_never_leaves_a_queued_task_charge() {
        let account = ResourceAccount::new(
            ResourceLimits::builder()
                .limit(ResourceClass::QueuedTasks, 1)
                .limit(ResourceClass::HostOperations, 0)
                .build(),
        );
        let pool = CompletionAdmissionPool::new(account.clone(), capacities(1, 1, 1));

        assert!(matches!(
            pool.admit(CompletionOrigin::HostOperation),
            Err(OtterError::Resource {
                error: ResourceError::Exhausted {
                    class: ResourceClass::HostOperations,
                    ..
                }
            })
        ));
        let snapshot = account.snapshot();
        assert_eq!(snapshot.get(ResourceClass::QueuedTasks).current(), 0);
        assert_eq!(snapshot.get(ResourceClass::HostOperations).current(), 0);
        assert_eq!(pool.available_slots(), 1);
    }

    #[test]
    fn dispatch_releases_queue_credit_but_retains_origin_credit() {
        let account = ResourceAccount::default();
        let pool = CompletionAdmissionPool::new(account.clone(), capacities(1, 1, 1));
        let admission = pool
            .admit(CompletionOrigin::HostOperation)
            .expect("completion admission");
        let snapshot = account.snapshot();
        assert_eq!(snapshot.get(ResourceClass::QueuedTasks).current(), 1);
        assert_eq!(snapshot.get(ResourceClass::HostOperations).current(), 1);
        assert_eq!(pool.available_slots(), 0);

        let active = admission.begin_dispatch();
        let snapshot = account.snapshot();
        assert_eq!(snapshot.get(ResourceClass::QueuedTasks).current(), 0);
        assert_eq!(snapshot.get(ResourceClass::HostOperations).current(), 1);
        assert_eq!(pool.available_slots(), 1);

        drop(active);
        assert_eq!(
            account
                .snapshot()
                .get(ResourceClass::HostOperations)
                .current(),
            0
        );
    }

    #[test]
    fn repeating_timers_are_physically_bounded_with_unlimited_ledger() {
        let pool = CompletionAdmissionPool::new(ResourceAccount::default(), capacities(1, 1, 1));
        let first = pool
            .admit_origin_only(ResourceClass::Timers)
            .expect("first repeating timer");
        assert!(pool.admit_origin_only(ResourceClass::Timers).is_err());
        drop(first);
        assert!(pool.admit_origin_only(ResourceClass::Timers).is_ok());
    }

    #[test]
    fn carriers_are_scoped_to_the_exact_isolate_pool() {
        let account = ResourceAccount::default();
        let first = CompletionAdmissionPool::new(account.clone(), capacities(1, 1, 1));
        let second = CompletionAdmissionPool::new(account, capacities(1, 1, 1));
        let admission = first
            .admit(CompletionOrigin::HostOperation)
            .expect("first-pool admission");

        assert!(admission.belongs_to(&first));
        assert!(!admission.belongs_to(&second));
    }

    #[test]
    fn panic_during_dispatch_releases_the_origin_permit() {
        let account = ResourceAccount::default();
        let pool = CompletionAdmissionPool::new(account.clone(), capacities(1, 1, 1));
        let active = pool
            .admit(CompletionOrigin::HostOperation)
            .expect("completion admission")
            .begin_dispatch();

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _active = active;
            panic!("simulated dispatch panic");
        }));

        assert!(outcome.is_err());
        assert_eq!(
            account
                .snapshot()
                .get(ResourceClass::HostOperations)
                .current(),
            0
        );
        assert!(pool.admit(CompletionOrigin::HostOperation).is_ok());
    }

    #[tokio::test]
    async fn aborting_a_future_drops_queued_and_origin_permits() {
        let account = ResourceAccount::default();
        let pool = CompletionAdmissionPool::new(account.clone(), capacities(1, 1, 1));
        let admission = pool
            .admit(CompletionOrigin::HostOperation)
            .expect("completion admission");
        let task = tokio::spawn(async move {
            let _admission = admission;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;

        task.abort();
        assert!(task.await.expect_err("task is aborted").is_cancelled());
        let snapshot = account.snapshot();
        assert_eq!(snapshot.get(ResourceClass::QueuedTasks).current(), 0);
        assert_eq!(snapshot.get(ResourceClass::HostOperations).current(), 0);
        assert_eq!(pool.available_slots(), 1);
        assert!(pool.admit(CompletionOrigin::HostOperation).is_ok());
    }
}
