//! Behavioral coverage for the resource ledger.
//!
//! # Contents
//! - Boundary, rollback, commit, sharing, atomic multi-class, concurrency,
//!   overflow, snapshot, poison recovery, and shared-source ownership tests.
//!
//! # Invariants
//! - Every test leaves all successfully acquired leases released.
//! - Concurrency tests observe only snapshots captured through the ledger's
//!   shared lock and retain owned guards until their intended release point.
//!
//! # See also
//! - [`crate::ResourceAccount`] is the tested public entry point.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use crate::class::RESOURCE_CLASS_COUNT;
use crate::{
    ResourceAccount, ResourceClass, ResourceError, ResourceLimits, SharedSource,
    SharedSourceBuilder, SharedSourceError,
};

fn account_with_limit(class: ResourceClass, limit: u64) -> ResourceAccount {
    ResourceAccount::new(ResourceLimits::builder().limit(class, limit).build())
}

#[test]
fn shared_source_charges_once_across_all_clones() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SharedSource>();

    let account = account_with_limit(ResourceClass::SourceModuleBytes, 3);
    let source = SharedSource::admit(&account, "éx".to_owned()).unwrap();

    assert_eq!(source.len(), 3);
    assert!(!source.is_empty());
    assert_eq!(source.as_ref(), "éx");
    assert_eq!(&*source, "éx");
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::SourceModuleBytes)
            .current(),
        3
    );

    let alias = source.clone();
    assert_eq!(source.as_ptr(), alias.as_ptr());
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::SourceModuleBytes)
            .current(),
        3
    );

    drop(source);
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::SourceModuleBytes)
            .current(),
        3
    );
    drop(alias);
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::SourceModuleBytes)
            .current(),
        0
    );
}

#[test]
fn shared_source_exact_admission_rejects_without_retained_charge() {
    let account = account_with_limit(ResourceClass::SourceModuleBytes, 2);
    let error = SharedSource::admit(&account, "three".to_owned()).unwrap_err();

    assert!(matches!(
        error,
        SharedSourceError::Resource(ResourceError::Exhausted {
            class: ResourceClass::SourceModuleBytes,
            requested: 5,
            in_use: 0,
            limit: 2,
        })
    ));
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::SourceModuleBytes)
            .current(),
        0
    );
}

#[test]
fn shared_source_builder_tracks_partial_bytes_and_rolls_back_on_drop() {
    let account = account_with_limit(ResourceClass::SourceModuleBytes, 6);
    {
        let mut builder = SharedSourceBuilder::new(&account);
        assert!(builder.is_empty());
        builder.push_bytes(b"abc").unwrap();
        assert_eq!(builder.len(), 3);
        assert_eq!(
            account
                .snapshot()
                .get(ResourceClass::SourceModuleBytes)
                .current(),
            3
        );

        let error = builder.push_bytes(b"defg").unwrap_err();
        assert!(matches!(
            error,
            SharedSourceError::Resource(ResourceError::Exhausted {
                class: ResourceClass::SourceModuleBytes,
                requested: 7,
                in_use: 0,
                limit: 6,
            })
        ));
        assert_eq!(builder.len(), 3);
        assert_eq!(
            account
                .snapshot()
                .get(ResourceClass::SourceModuleBytes)
                .current(),
            3
        );
    }
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::SourceModuleBytes)
            .current(),
        0
    );
}

#[test]
fn shared_source_builder_accepts_split_utf8_and_transfers_its_lease() {
    let account = account_with_limit(ResourceClass::SourceModuleBytes, 4);
    let mut builder = SharedSourceBuilder::new(&account);
    builder.push_bytes(&[0xf0, 0x9f]).unwrap();
    builder.push_bytes(&[0xa6, 0xa6]).unwrap();

    let source = builder.finish_utf8().unwrap();
    assert_eq!(source.as_ref(), "🦦");
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::SourceModuleBytes)
            .current(),
        4
    );
    drop(source);
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::SourceModuleBytes)
            .current(),
        0
    );
}

#[test]
fn invalid_utf8_drops_source_bytes_and_charge_before_returning() {
    let account = account_with_limit(ResourceClass::SourceModuleBytes, 3);
    let mut builder = SharedSourceBuilder::new(&account);
    builder.push_bytes(&[0xf0, 0x28, 0x8c]).unwrap();

    assert!(matches!(
        builder.finish_utf8(),
        Err(SharedSourceError::InvalidUtf8(_))
    ));
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::SourceModuleBytes)
            .current(),
        0
    );
}

#[test]
fn fixed_chunk_reader_rolls_back_after_partial_io_failure() {
    struct PartialFailure {
        bytes: Option<&'static [u8]>,
    }

    impl io::Read for PartialFailure {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if let Some(bytes) = self.bytes.take() {
                let read = bytes.len().min(output.len());
                output[..read].copy_from_slice(&bytes[..read]);
                Ok(read)
            } else {
                Err(io::Error::other("provider failed"))
            }
        }
    }

    let account = account_with_limit(ResourceClass::SourceModuleBytes, 32);
    let error = SharedSource::read_utf8(
        &account,
        PartialFailure {
            bytes: Some(b"partial"),
        },
    )
    .unwrap_err();

    assert!(matches!(error, SharedSourceError::Io(_)));
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::SourceModuleBytes)
            .current(),
        0
    );
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::SourceModuleBytes)
            .peak(),
        7
    );
}

#[test]
fn fixed_chunk_reader_admits_the_exact_complete_size() {
    let input = vec![b'x'; 16 * 1024 + 19];
    let account = account_with_limit(
        ResourceClass::SourceModuleBytes,
        u64::try_from(input.len()).unwrap(),
    );
    let source = SharedSource::read_utf8(&account, io::Cursor::new(&input)).unwrap();

    assert_eq!(source.len(), input.len());
    assert_eq!(source.as_bytes(), input);
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::SourceModuleBytes)
            .current(),
        u64::try_from(input.len()).unwrap()
    );

    drop(source);
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::SourceModuleBytes)
            .current(),
        0
    );
}

#[test]
fn exact_limit_is_admitted_and_next_unit_is_rejected() {
    let account = account_with_limit(ResourceClass::HeapBytes, 10);
    let lease = account.reserve_exact(ResourceClass::HeapBytes, 10).unwrap();

    assert_eq!(
        account
            .reserve_exact(ResourceClass::HeapBytes, 1)
            .unwrap_err(),
        ResourceError::Exhausted {
            class: ResourceClass::HeapBytes,
            requested: 1,
            in_use: 10,
            limit: 10,
        }
    );
    assert_eq!(
        account.snapshot().get(ResourceClass::HeapBytes).current(),
        10
    );

    drop(lease);
    assert_eq!(
        account.snapshot().get(ResourceClass::HeapBytes).current(),
        0
    );
}

#[test]
fn dropped_reservation_rolls_back() {
    let account = account_with_limit(ResourceClass::ExternalBytes, 20);
    {
        let reservation = account.reserve(ResourceClass::ExternalBytes, 13).unwrap();
        assert_eq!(reservation.amount(), 13);
        assert_eq!(
            account
                .snapshot()
                .get(ResourceClass::ExternalBytes)
                .current(),
            13
        );
    }
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::ExternalBytes)
            .current(),
        0
    );
}

#[test]
fn commit_can_shrink_or_grow_and_failed_growth_rolls_back() {
    let account = account_with_limit(ResourceClass::SourceModuleBytes, 10);

    let shrunk = account
        .reserve(ResourceClass::SourceModuleBytes, 8)
        .unwrap()
        .commit_exact(3)
        .unwrap();
    assert_eq!(shrunk.amount(), 3);
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::SourceModuleBytes)
            .current(),
        3
    );
    drop(shrunk);

    let grown = account
        .reserve(ResourceClass::SourceModuleBytes, 3)
        .unwrap()
        .commit_exact(9)
        .unwrap();
    assert_eq!(grown.amount(), 9);
    drop(grown);

    let retained = account
        .reserve_exact(ResourceClass::SourceModuleBytes, 2)
        .unwrap();
    let error = account
        .reserve(ResourceClass::SourceModuleBytes, 6)
        .unwrap()
        .commit_exact(9)
        .unwrap_err();
    assert_eq!(
        error,
        ResourceError::Exhausted {
            class: ResourceClass::SourceModuleBytes,
            requested: 9,
            in_use: 2,
            limit: 10,
        }
    );
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::SourceModuleBytes)
            .current(),
        2
    );
    drop(retained);
}

#[test]
fn lease_resize_is_atomic_and_preserves_old_charge_on_failure() {
    let account = account_with_limit(ResourceClass::GeneratedCodeBytes, 10);
    let mut resized = account
        .reserve_exact(ResourceClass::GeneratedCodeBytes, 4)
        .unwrap();
    let sibling = account
        .reserve_exact(ResourceClass::GeneratedCodeBytes, 5)
        .unwrap();

    assert_eq!(
        resized.resize(6).unwrap_err(),
        ResourceError::Exhausted {
            class: ResourceClass::GeneratedCodeBytes,
            requested: 6,
            in_use: 5,
            limit: 10,
        }
    );
    assert_eq!(resized.amount(), 4);
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::GeneratedCodeBytes)
            .current(),
        9
    );

    drop(sibling);
    resized.resize(6).unwrap();
    resized.resize(2).unwrap();
    assert_eq!(resized.amount(), 2);
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::GeneratedCodeBytes)
            .current(),
        2
    );
    drop(resized);
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::GeneratedCodeBytes)
            .current(),
        0
    );
}

#[test]
fn clones_share_aggregate_usage() {
    let account = account_with_limit(ResourceClass::Workers, 2);
    let clone = account.clone();
    let first = account.reserve_exact(ResourceClass::Workers, 1).unwrap();
    let second = clone.reserve_exact(ResourceClass::Workers, 1).unwrap();

    assert!(matches!(
        account.reserve_exact(ResourceClass::Workers, 1),
        Err(ResourceError::Exhausted { in_use: 2, .. })
    ));
    drop(first);
    assert_eq!(clone.snapshot().get(ResourceClass::Workers).current(), 1);
    drop(second);
    assert_eq!(account.snapshot().get(ResourceClass::Workers).current(), 0);
}

#[test]
fn exact_many_charges_and_releases_every_class_together() {
    let account = ResourceAccount::default();
    let lease = account
        .reserve_exact_many(&[
            (ResourceClass::Workers, 2),
            (ResourceClass::WorkerStackBytes, 32),
            (ResourceClass::QueuedMessages, 0),
        ])
        .unwrap();

    assert_eq!(lease.amount(ResourceClass::Workers), 2);
    assert_eq!(lease.amount(ResourceClass::WorkerStackBytes), 32);
    assert_eq!(lease.amount(ResourceClass::QueuedMessages), 0);
    assert!(!lease.is_empty());
    let debug = format!("{lease:?}");
    assert!(debug.find("Workers").unwrap() < debug.find("WorkerStackBytes").unwrap());

    let snapshot = account.snapshot();
    assert_eq!(snapshot.get(ResourceClass::Workers).current(), 2);
    assert_eq!(snapshot.get(ResourceClass::WorkerStackBytes).current(), 32);

    drop(lease);
    let snapshot = account.snapshot();
    assert_eq!(snapshot.get(ResourceClass::Workers).current(), 0);
    assert_eq!(snapshot.get(ResourceClass::WorkerStackBytes).current(), 0);

    let empty = account
        .reserve_exact_many(&[(ResourceClass::Workers, 0)])
        .unwrap();
    assert!(empty.is_empty());
    assert_eq!(empty.amount(ResourceClass::Workers), 0);
    drop(empty);
}

#[test]
fn exact_many_take_transfers_one_class_without_mutating_the_ledger() {
    let account = ResourceAccount::default();
    let mut set = account
        .reserve_exact_many(&[
            (ResourceClass::QueuedTasks, 1),
            (ResourceClass::HostOperations, 1),
        ])
        .unwrap();
    let host = set
        .take(ResourceClass::HostOperations)
        .expect("non-zero class produces a lease");
    assert_eq!(host.class(), ResourceClass::HostOperations);
    assert_eq!(host.amount(), 1);
    assert_eq!(set.amount(ResourceClass::HostOperations), 0);
    assert!(set.take(ResourceClass::HostOperations).is_none());

    drop(set);
    let snapshot = account.snapshot();
    assert_eq!(snapshot.get(ResourceClass::QueuedTasks).current(), 0);
    assert_eq!(snapshot.get(ResourceClass::HostOperations).current(), 1);

    drop(host);
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::HostOperations)
            .current(),
        0
    );
}

#[test]
fn exact_many_second_class_failure_changes_no_current_or_peak() {
    let limits = ResourceLimits::builder()
        .limit(ResourceClass::Workers, 8)
        .limit(ResourceClass::WorkerStackBytes, 10)
        .build();
    let account = ResourceAccount::new(limits);
    let workers = account.reserve_exact(ResourceClass::Workers, 1).unwrap();
    let stack = account
        .reserve_exact(ResourceClass::WorkerStackBytes, 2)
        .unwrap();

    let error = account
        .reserve_exact_many(&[
            (ResourceClass::Workers, 3),
            (ResourceClass::WorkerStackBytes, 9),
        ])
        .unwrap_err();
    assert_eq!(
        error,
        ResourceError::Exhausted {
            class: ResourceClass::WorkerStackBytes,
            requested: 9,
            in_use: 2,
            limit: 10,
        }
    );

    let snapshot = account.snapshot();
    let worker_entry = snapshot.get(ResourceClass::Workers);
    assert_eq!(worker_entry.current(), 1);
    assert_eq!(worker_entry.peak(), 1);
    assert_eq!(worker_entry.rejections(), 0);
    let stack_entry = snapshot.get(ResourceClass::WorkerStackBytes);
    assert_eq!(stack_entry.current(), 2);
    assert_eq!(stack_entry.peak(), 2);
    assert_eq!(stack_entry.rejections(), 1);

    drop((workers, stack));
}

#[test]
fn exact_many_aggregates_duplicates_and_rejects_aggregate_overflow() {
    let account = ResourceAccount::default();
    let lease = account
        .reserve_exact_many(&[
            (ResourceClass::QueuedMessages, 2),
            (ResourceClass::QueuedMessages, 3),
            (ResourceClass::QueuedMessages, 0),
        ])
        .unwrap();
    assert_eq!(lease.amount(ResourceClass::QueuedMessages), 5);
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::QueuedMessages)
            .current(),
        5
    );
    drop(lease);

    let error = account
        .reserve_exact_many(&[
            (ResourceClass::QueuedMessages, u64::MAX),
            (ResourceClass::QueuedMessages, 1),
        ])
        .unwrap_err();
    assert_eq!(
        error,
        ResourceError::Overflow {
            class: ResourceClass::QueuedMessages,
            requested: 1,
            in_use: u64::MAX,
            limit: None,
        }
    );
    let snapshot = account.snapshot();
    let entry = snapshot.get(ResourceClass::QueuedMessages);
    assert_eq!(entry.current(), 0);
    assert_eq!(entry.peak(), 5);
    assert_eq!(entry.rejections(), 1);
}

#[test]
fn concurrent_mixed_reservations_never_publish_partial_state() {
    const THREADS: usize = 12;
    const ITERATIONS: usize = 500;
    const BYTES_PER_MESSAGE: u64 = 17;

    let limits = ResourceLimits::builder()
        .limit(ResourceClass::QueuedMessages, THREADS as u64)
        .limit(
            ResourceClass::QueuedMessageBytes,
            THREADS as u64 * BYTES_PER_MESSAGE,
        )
        .build();
    let account = ResourceAccount::new(limits);
    let start = Arc::new(Barrier::new(THREADS + 1));
    let remaining = Arc::new(AtomicUsize::new(THREADS));

    let observer_account = account.clone();
    let observer_start = Arc::clone(&start);
    let observer_remaining = Arc::clone(&remaining);
    let observer = thread::spawn(move || {
        observer_start.wait();
        let mut observations = 0_usize;
        loop {
            let snapshot = observer_account.snapshot();
            let messages = snapshot.get(ResourceClass::QueuedMessages).current();
            let bytes = snapshot.get(ResourceClass::QueuedMessageBytes).current();
            assert_eq!(bytes, messages * BYTES_PER_MESSAGE);
            observations += 1;
            if observer_remaining.load(Ordering::Acquire) == 0 {
                break;
            }
            thread::yield_now();
        }
        observations
    });

    let mut reservers = Vec::with_capacity(THREADS);
    for _ in 0..THREADS {
        let account = account.clone();
        let start = Arc::clone(&start);
        let remaining = Arc::clone(&remaining);
        reservers.push(thread::spawn(move || {
            start.wait();
            for _ in 0..ITERATIONS {
                let lease = account
                    .reserve_exact_many(&[
                        (ResourceClass::QueuedMessages, 1),
                        (ResourceClass::QueuedMessageBytes, BYTES_PER_MESSAGE),
                    ])
                    .unwrap();
                thread::yield_now();
                drop(lease);
            }
            remaining.fetch_sub(1, Ordering::Release);
        }));
    }

    for reserver in reservers {
        reserver.join().unwrap();
    }
    assert!(observer.join().unwrap() > 0);

    let snapshot = account.snapshot();
    let messages = snapshot.get(ResourceClass::QueuedMessages);
    let bytes = snapshot.get(ResourceClass::QueuedMessageBytes);
    assert_eq!(messages.current(), 0);
    assert_eq!(bytes.current(), 0);
    assert_eq!(bytes.peak(), messages.peak() * BYTES_PER_MESSAGE);
}

#[test]
fn concurrent_storm_never_exceeds_cap_and_returns_to_zero() {
    const THREADS: usize = 32;
    const LIMIT: u64 = 8;

    let account = account_with_limit(ResourceClass::HostOperations, LIMIT);
    let barrier = Arc::new(Barrier::new(THREADS));
    let mut threads = Vec::with_capacity(THREADS);
    for _ in 0..THREADS {
        let account = account.clone();
        let barrier = Arc::clone(&barrier);
        threads.push(thread::spawn(move || {
            let lease = account.reserve_exact(ResourceClass::HostOperations, 1).ok();
            barrier.wait();
            assert!(
                account
                    .snapshot()
                    .get(ResourceClass::HostOperations)
                    .current()
                    <= LIMIT
            );
            u64::from(lease.is_some())
        }));
    }

    let admitted: u64 = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .sum();
    assert_eq!(admitted, LIMIT);
    let snapshot = account.snapshot();
    let entry = snapshot.get(ResourceClass::HostOperations);
    assert_eq!(entry.current(), 0);
    assert_eq!(entry.peak(), LIMIT);
    assert_eq!(entry.rejections(), THREADS as u64 - LIMIT);
}

#[test]
fn overflow_is_typed_and_does_not_change_usage() {
    let account = ResourceAccount::default();
    let lease = account
        .reserve_exact(ResourceClass::GeneratedCodeBytes, u64::MAX)
        .unwrap();

    let error = account
        .reserve_exact(ResourceClass::GeneratedCodeBytes, 1)
        .unwrap_err();
    assert_eq!(
        error,
        ResourceError::Overflow {
            class: ResourceClass::GeneratedCodeBytes,
            requested: 1,
            in_use: u64::MAX,
            limit: None,
        }
    );
    assert_eq!(error.class(), ResourceClass::GeneratedCodeBytes);
    assert_eq!(error.requested(), 1);
    assert_eq!(error.in_use(), u64::MAX);
    assert_eq!(error.limit(), None);
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::GeneratedCodeBytes)
            .current(),
        u64::MAX
    );
    drop(lease);
}

#[test]
fn snapshots_track_peaks_rejections_limits_and_stable_order() {
    let limits = ResourceLimits::builder()
        .limit(ResourceClass::QueuedMessageBytes, 10)
        .limit(ResourceClass::Timers, 4)
        .build();
    assert_eq!(limits.get(ResourceClass::QueuedMessageBytes), Some(10));
    assert_eq!(limits.get(ResourceClass::HeapBytes), None);

    let account = ResourceAccount::new(limits);
    let lease = account
        .reserve_exact(ResourceClass::QueuedMessageBytes, 7)
        .unwrap();
    assert!(
        account
            .reserve_exact(ResourceClass::QueuedMessageBytes, 4)
            .is_err()
    );

    let snapshot = account.snapshot();
    let entry = snapshot.get(ResourceClass::QueuedMessageBytes);
    assert_eq!(entry.current(), 7);
    assert_eq!(entry.peak(), 7);
    assert_eq!(entry.rejections(), 1);
    assert_eq!(entry.limit(), Some(10));
    assert_eq!(snapshot.entries().len(), RESOURCE_CLASS_COUNT);
    assert!(
        snapshot
            .iter()
            .zip(ResourceClass::ALL)
            .all(|(entry, class)| entry.class() == class)
    );

    drop(lease);
    let snapshot = account.snapshot();
    let entry = snapshot.get(ResourceClass::QueuedMessageBytes);
    assert_eq!(entry.current(), 0);
    assert_eq!(entry.peak(), 7);
    assert_eq!(entry.rejections(), 1);
    assert_eq!(entry.limit(), Some(10));
}

#[test]
fn poisoned_mutex_is_recovered_without_losing_state() {
    let account = account_with_limit(ResourceClass::QueuedTasks, 2);
    let poisoner = account.clone();
    assert!(
        thread::spawn(move || poisoner.poison_for_test())
            .join()
            .is_err()
    );

    let lease = account
        .reserve_exact(ResourceClass::QueuedTasks, 2)
        .unwrap();
    assert_eq!(
        account.snapshot().get(ResourceClass::QueuedTasks).current(),
        2
    );
    drop(lease);
    assert_eq!(
        account.snapshot().get(ResourceClass::QueuedTasks).current(),
        0
    );
}

#[test]
fn exact_many_recovers_a_poisoned_mutex_and_releases_as_one_set() {
    let limits = ResourceLimits::builder()
        .limit(ResourceClass::QueuedMessages, 2)
        .limit(ResourceClass::QueuedMessageBytes, 32)
        .build();
    let account = ResourceAccount::new(limits);
    let poisoner = account.clone();
    assert!(
        thread::spawn(move || poisoner.poison_for_test())
            .join()
            .is_err()
    );

    let lease = account
        .reserve_exact_many(&[
            (ResourceClass::QueuedMessages, 2),
            (ResourceClass::QueuedMessageBytes, 32),
        ])
        .unwrap();
    let snapshot = account.snapshot();
    assert_eq!(snapshot.get(ResourceClass::QueuedMessages).current(), 2);
    assert_eq!(
        snapshot.get(ResourceClass::QueuedMessageBytes).current(),
        32
    );

    drop(lease);
    let snapshot = account.snapshot();
    assert_eq!(snapshot.get(ResourceClass::QueuedMessages).current(), 0);
    assert_eq!(snapshot.get(ResourceClass::QueuedMessageBytes).current(), 0);
}
