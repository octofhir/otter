//! Shared isolate-resource admission across runtime construction paths.
//!
//! # Contents
//! - Direct, sendable, pool, realm, worker, and snapshot admission tests.
//! - Typed error and rollback assertions.
//!
//! # Invariants
//! - Every live runtime owns exactly one `Isolates` charge plus the exact
//!   stack/worker charges selected by its execution role.
//! - Role admission is atomic, precedes construction effects, and every failed
//!   path rolls back the complete tuple.
//! - In-process runtime snapshots retain their donor account.
//!
//! # See also
//! - `otter_resource::ResourceAccount`

use otter_runtime::{
    JitSelection, Otter, OtterError, OtterPool, RUNTIME_THREAD_STACK_BYTES, ResourceAccount,
    ResourceClass, ResourceError, ResourceLimits, ResourceSnapshotEntry, Runtime, RuntimeBuilder,
    RuntimeGlobalInstaller, SnapshotRuntimeOptions, SourceInput, Worker,
};

fn isolate_account(limit: u64) -> ResourceAccount {
    ResourceAccount::new(
        ResourceLimits::builder()
            .limit(ResourceClass::Isolates, limit)
            .build(),
    )
}

fn isolate_entry(account: &ResourceAccount) -> ResourceSnapshotEntry {
    *account.snapshot().get(ResourceClass::Isolates)
}

fn resource_entry(account: &ResourceAccount, class: ResourceClass) -> ResourceSnapshotEntry {
    *account.snapshot().get(class)
}

fn minimal_builder(account: ResourceAccount) -> RuntimeBuilder {
    Runtime::builder()
        .resource_account(account)
        .process_global(false)
        .worker_global(false)
        .jit_selection(JitSelection::InterpreterOnly)
}

fn assert_isolate_exhausted(error: OtterError, in_use: u64, limit: u64) {
    assert!(matches!(
        error,
        OtterError::Resource {
            error: ResourceError::Exhausted {
                class: ResourceClass::Isolates,
                requested: 1,
                in_use: actual_in_use,
                limit: actual_limit,
            }
        } if actual_in_use == in_use && actual_limit == limit
    ));
}

#[test]
fn direct_runtimes_share_an_exact_isolate_limit_and_release_on_drop() {
    let account = isolate_account(1);
    let builder = minimal_builder(account.clone());

    let first = builder.clone().build().expect("first isolate");
    let entry = isolate_entry(&account);
    assert_eq!(
        (entry.current(), entry.peak(), entry.rejections()),
        (1, 1, 0)
    );

    let error = builder
        .clone()
        .build()
        .expect_err("second isolate rejected");
    assert_isolate_exhausted(error, 1, 1);
    let entry = isolate_entry(&account);
    assert_eq!(
        (entry.current(), entry.peak(), entry.rejections()),
        (1, 1, 1)
    );

    drop(first);
    assert_eq!(isolate_entry(&account).current(), 0);
    drop(builder.build().expect("slot reusable after drop"));
    assert_eq!(isolate_entry(&account).current(), 0);
}

#[test]
fn runtime_roles_retain_their_exact_resource_tuples() {
    let direct_account = ResourceAccount::default();
    let direct = minimal_builder(direct_account.clone())
        .build()
        .expect("direct runtime");
    assert_eq!(isolate_entry(&direct_account).current(), 1);
    assert_eq!(
        resource_entry(&direct_account, ResourceClass::Workers).current(),
        0
    );
    assert_eq!(
        resource_entry(&direct_account, ResourceClass::WorkerStackBytes).current(),
        0
    );
    drop(direct);

    let handle_account = ResourceAccount::default();
    let handle = minimal_builder(handle_account.clone())
        .build_handle()
        .expect("handle runtime");
    assert_eq!(isolate_entry(&handle_account).current(), 1);
    assert_eq!(
        resource_entry(&handle_account, ResourceClass::Workers).current(),
        0
    );
    assert_eq!(
        resource_entry(&handle_account, ResourceClass::WorkerStackBytes).current(),
        RUNTIME_THREAD_STACK_BYTES as u64
    );
    drop(handle);

    let worker_account = ResourceAccount::default();
    let worker = Worker::builder()
        .resource_account(worker_account.clone())
        .build()
        .expect("worker runtime");
    assert_eq!(isolate_entry(&worker_account).current(), 1);
    assert_eq!(
        resource_entry(&worker_account, ResourceClass::Workers).current(),
        1
    );
    assert_eq!(
        resource_entry(&worker_account, ResourceClass::WorkerStackBytes).current(),
        RUNTIME_THREAD_STACK_BYTES as u64
    );
    drop(worker);

    for account in [direct_account, handle_account, worker_account] {
        assert_eq!(isolate_entry(&account).current(), 0);
        assert_eq!(
            resource_entry(&account, ResourceClass::Workers).current(),
            0
        );
        assert_eq!(
            resource_entry(&account, ResourceClass::WorkerStackBytes).current(),
            0
        );
    }
}

#[test]
fn aggregate_admission_rejection_publishes_no_partial_peak() {
    let account = ResourceAccount::new(
        ResourceLimits::builder()
            .limit(ResourceClass::Isolates, 1)
            .limit(ResourceClass::WorkerStackBytes, 0)
            .build(),
    );
    let error = minimal_builder(account.clone())
        .build_handle()
        .expect_err("native stack limit must reject the handle");
    assert!(matches!(
        error,
        OtterError::Resource {
            error: ResourceError::Exhausted {
                class: ResourceClass::WorkerStackBytes,
                requested,
                in_use: 0,
                limit: 0,
            }
        } if requested == RUNTIME_THREAD_STACK_BYTES as u64
    ));
    let isolate = isolate_entry(&account);
    let stack = resource_entry(&account, ResourceClass::WorkerStackBytes);
    assert_eq!((isolate.current(), isolate.peak()), (0, 0));
    assert_eq!(
        (stack.current(), stack.peak(), stack.rejections()),
        (0, 0, 1)
    );
}

#[test]
fn validation_happens_before_resource_admission() {
    let account = isolate_account(0);
    let error = minimal_builder(account.clone())
        .max_stack_depth(0)
        .build()
        .expect_err("invalid config");
    assert!(matches!(error, OtterError::Config { .. }));
    let entry = isolate_entry(&account);
    assert_eq!(
        (entry.current(), entry.peak(), entry.rejections()),
        (0, 0, 0)
    );
}

#[test]
fn handle_limit_rejects_before_event_loop_channel_or_thread_construction() {
    let account = isolate_account(0);
    let error = minimal_builder(account.clone())
        .build_handle()
        .expect_err("zero isolate limit");

    assert_isolate_exhausted(error, 0, 0);
    let entry = isolate_entry(&account);
    assert_eq!(
        (entry.current(), entry.peak(), entry.rejections()),
        (0, 0, 1)
    );
}

#[test]
fn async_builder_without_tokio_rolls_admission_back() {
    use std::future::Future as _;

    let account = isolate_account(1);
    let mut future = Box::pin(minimal_builder(account.clone()).build_handle_async());
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    let error = match future.as_mut().poll(&mut context) {
        std::task::Poll::Ready(Err(error)) => error,
        std::task::Poll::Ready(Ok(_)) => panic!("async build unexpectedly found a Tokio host"),
        std::task::Poll::Pending => panic!("missing Tokio host must fail before suspension"),
    };

    assert!(matches!(error, OtterError::Internal { .. }));
    let entry = isolate_entry(&account);
    assert_eq!(
        (entry.current(), entry.peak(), entry.rejections()),
        (0, 1, 0)
    );
}

#[test]
fn failed_bootstrap_rolls_admission_back() {
    let account = isolate_account(1);
    let installer = RuntimeGlobalInstaller::new(|_| {
        Err(OtterError::Internal {
            code: "TEST_RESOURCE_ROLLBACK".to_string(),
            message: "intentional installer failure".to_string(),
        })
    });

    let error = minimal_builder(account.clone())
        .global_installer(installer)
        .build()
        .expect_err("installer must fail");
    assert!(matches!(error, OtterError::Internal { .. }));
    let entry = isolate_entry(&account);
    assert_eq!(
        (entry.current(), entry.peak(), entry.rejections()),
        (0, 1, 0)
    );
}

#[test]
fn failed_handle_bootstrap_is_joined_and_releases_before_return() {
    let account = isolate_account(1);
    let installer = RuntimeGlobalInstaller::new(|_| {
        Err(OtterError::Internal {
            code: "TEST_HANDLE_RESOURCE_ROLLBACK".to_string(),
            message: "intentional installer failure".to_string(),
        })
    });

    let error = minimal_builder(account.clone())
        .global_installer(installer)
        .build_handle()
        .expect_err("isolate thread bootstrap must fail");
    assert!(matches!(error, OtterError::Internal { .. }));
    let entry = isolate_entry(&account);
    assert_eq!(
        (entry.current(), entry.peak(), entry.rejections()),
        (0, 1, 0)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handle_clones_do_not_charge_and_shutdown_releases_before_last_clone() {
    let account = isolate_account(1);
    let builder = minimal_builder(account.clone());
    let handle = builder.clone().build_handle().expect("handle isolate");
    let surviving_clone = handle.clone();

    assert_eq!(isolate_entry(&account).current(), 1);
    let error = builder
        .build_handle()
        .expect_err("shared handle limit must reject");
    assert_isolate_exhausted(error, 1, 1);

    handle.shutdown_and_wait().await;
    assert_eq!(isolate_entry(&account).current(), 0);
    assert_eq!(
        surviving_clone
            .resource_snapshot()
            .get(ResourceClass::Isolates)
            .current(),
        0
    );
}

#[test]
fn pool_build_failure_releases_already_started_isolates() {
    let account = isolate_account(1);
    let error = OtterPool::builder()
        .workers(2)
        .resource_account(account.clone())
        .build()
        .expect_err("second pool isolate must be rejected");
    assert_isolate_exhausted(error, 1, 1);
    let entry = isolate_entry(&account);
    assert_eq!(
        (entry.current(), entry.peak(), entry.rejections()),
        (0, 1, 1)
    );
    let workers = resource_entry(&account, ResourceClass::Workers);
    assert_eq!(
        (workers.current(), workers.peak(), workers.rejections()),
        (0, 1, 0)
    );
    let stacks = resource_entry(&account, ResourceClass::WorkerStackBytes);
    assert_eq!(
        (stacks.current(), stacks.peak(), stacks.rejections()),
        (0, RUNTIME_THREAD_STACK_BYTES as u64, 0)
    );
}

#[test]
fn additional_realms_do_not_count_as_isolates() {
    let account = isolate_account(1);
    let mut runtime = minimal_builder(account.clone()).build().expect("runtime");
    let realm = runtime.create_realm().expect("realm");
    assert_eq!(isolate_entry(&account).current(), 1);
    runtime.dispose_realm(realm).expect("dispose realm");
    assert_eq!(isolate_entry(&account).current(), 1);
    drop(runtime);
    assert_eq!(isolate_entry(&account).current(), 0);
}

#[test]
fn javascript_worker_requires_a_managed_runtime() {
    let account = isolate_account(1);
    let mut parent = Runtime::builder()
        .resource_account(account.clone())
        .process_global(false)
        .jit_selection(JitSelection::InterpreterOnly)
        .build()
        .expect("parent isolate");

    let error = parent
        .eval(SourceInput::from_javascript(
            "new Worker('resource-limit-worker.js')",
        ))
        .expect_err("a direct runtime has no managed task spawner");
    assert!(
        error
            .to_string()
            .contains("Worker requires a managed RuntimeHandle/Otter runtime"),
        "unexpected error: {error}"
    );
    // The refusal is synchronous and produces no resource effect: nothing was
    // reserved and no rejection was published on any class.
    let entry = isolate_entry(&account);
    assert_eq!(
        (entry.current(), entry.peak(), entry.rejections()),
        (1, 1, 0)
    );
    for class in [
        ResourceClass::Workers,
        ResourceClass::WorkerStackBytes,
        ResourceClass::QueuedMessages,
        ResourceClass::QueuedMessageBytes,
    ] {
        let entry = resource_entry(&account, class);
        assert_eq!(
            (entry.current(), entry.peak(), entry.rejections()),
            (0, 0, 0)
        );
    }

    drop(parent);
    assert_eq!(isolate_entry(&account).current(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn javascript_worker_role_rejection_precedes_child_thread_and_channels() {
    let account = ResourceAccount::new(
        ResourceLimits::builder()
            .limit(ResourceClass::Isolates, 2)
            .limit(ResourceClass::Workers, 0)
            .limit(
                ResourceClass::WorkerStackBytes,
                2 * RUNTIME_THREAD_STACK_BYTES as u64,
            )
            .build(),
    );
    let otter = Otter::builder()
        .resource_account(account.clone())
        .jit_selection(JitSelection::InterpreterOnly)
        .build()
        .expect("managed parent");

    otter
        .eval("new Worker('worker-role-limit.js')")
        .await
        .expect_err("worker slot limit must reject before child setup");

    let isolates = isolate_entry(&account);
    assert_eq!(
        (isolates.current(), isolates.peak(), isolates.rejections()),
        (1, 1, 0)
    );
    let workers = resource_entry(&account, ResourceClass::Workers);
    assert_eq!(
        (workers.current(), workers.peak(), workers.rejections()),
        (0, 0, 1)
    );
    // The rejected worker never spawned a thread: only the parent handle's
    // stack charge is visible.
    let stacks = resource_entry(&account, ResourceClass::WorkerStackBytes);
    assert_eq!(
        (stacks.current(), stacks.peak(), stacks.rejections()),
        (
            RUNTIME_THREAD_STACK_BYTES as u64,
            RUNTIME_THREAD_STACK_BYTES as u64,
            0
        )
    );

    otter.handle().shutdown_and_wait().await;
    assert_eq!(isolate_entry(&account).current(), 0);
}

#[test]
fn in_process_snapshot_restore_always_joins_the_donor_account() {
    let donor_account = isolate_account(1);
    let donor = minimal_builder(donor_account.clone())
        .build()
        .expect("donor");
    let snapshot = donor.capture_isolate_snapshot().expect("snapshot");

    let error = Runtime::from_isolate_snapshot_with(
        &snapshot,
        SnapshotRuntimeOptions {
            jit_selection: JitSelection::InterpreterOnly,
            ..SnapshotRuntimeOptions::default()
        },
    )
    .expect_err("live donor occupies its only isolate slot");
    assert_isolate_exhausted(error, 1, 1);

    drop(donor);
    let restored = Runtime::from_isolate_snapshot(&snapshot).expect("restore after donor drop");
    assert_eq!(isolate_entry(&donor_account).current(), 1);
    assert_eq!(
        restored
            .resource_account()
            .snapshot()
            .get(ResourceClass::Isolates)
            .current(),
        1
    );
    drop(restored);
    assert_eq!(isolate_entry(&donor_account).current(), 0);
}

#[test]
fn failed_in_process_restore_releases_its_admission() {
    let account = isolate_account(1);
    let snapshot = {
        let donor = minimal_builder(account.clone()).build().expect("donor");
        donor.capture_isolate_snapshot().expect("snapshot")
    };
    assert_eq!(isolate_entry(&account).current(), 0);

    let error = Runtime::from_isolate_snapshot_with(
        &snapshot,
        SnapshotRuntimeOptions {
            max_heap_bytes: 1,
            jit_selection: JitSelection::InterpreterOnly,
            ..SnapshotRuntimeOptions::default()
        },
    )
    .expect_err("captured image cannot fit in a one-byte heap cap");
    assert!(matches!(
        error,
        OtterError::OutOfMemory {
            heap_limit_bytes: 1,
            ..
        }
    ));
    let entry = isolate_entry(&account);
    assert_eq!(
        (entry.current(), entry.peak(), entry.rejections()),
        (0, 1, 0)
    );
}

#[test]
fn public_resource_diagnostics_are_send_sync() {
    fn assert_send_sync<T: Send + Sync + 'static>() {}

    assert_send_sync::<ResourceAccount>();
    assert_send_sync::<otter_runtime::ResourceSnapshot>();
    assert_send_sync::<otter_runtime::RuntimeHandle>();
}

#[test]
fn module_source_bytes_are_charged_while_retained_and_released_on_drop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dep = dir.path().join("dep.js");
    std::fs::write(&dep, "export const value = 41;").expect("write dep");
    let entry = dir.path().join("entry.js");
    std::fs::write(
        &entry,
        "import { value } from './dep.js'; globalThis.out = value + 1;",
    )
    .expect("write entry");

    let account = ResourceAccount::default();
    let mut runtime = Runtime::builder()
        .resource_account(account.clone())
        .capabilities(otter_runtime::CapabilitySet::allow_all())
        .jit_selection(JitSelection::InterpreterOnly)
        .build()
        .expect("runtime");
    runtime.run_file(&entry).expect("entry runs");

    let retained = resource_entry(&account, ResourceClass::SourceModuleBytes);
    let source_bytes =
        (std::fs::metadata(&entry).unwrap().len() + std::fs::metadata(&dep).unwrap().len()) as u64;
    assert!(
        retained.current() >= source_bytes,
        "retained {} < loaded source bytes {}",
        retained.current(),
        source_bytes
    );
    assert_eq!(retained.rejections(), 0);

    drop(runtime);
    let after = resource_entry(&account, ResourceClass::SourceModuleBytes);
    assert_eq!(
        after.current(),
        0,
        "source charge must die with the isolate"
    );
}

#[test]
fn source_byte_limit_rejects_an_oversized_module_before_retention() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = dir.path().join("entry.js");
    std::fs::write(&entry, "globalThis.out = 'x'.repeat(10);").expect("write entry");

    let account = ResourceAccount::new(
        ResourceLimits::builder()
            .limit(ResourceClass::SourceModuleBytes, 8)
            .build(),
    );
    let mut runtime = Runtime::builder()
        .resource_account(account.clone())
        .capabilities(otter_runtime::CapabilitySet::allow_all())
        .jit_selection(JitSelection::InterpreterOnly)
        .build()
        .expect("runtime");
    runtime
        .run_file(&entry)
        .expect_err("an 8-byte source budget cannot admit the module");
    let entry_stats = resource_entry(&account, ResourceClass::SourceModuleBytes);
    assert!(entry_stats.rejections() >= 1);
    assert_eq!(
        entry_stats.current(),
        0,
        "a rejected source must not stay charged"
    );
}
