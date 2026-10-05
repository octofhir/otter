//! Transactional code-directory capacity migration between real resource accounts.
//!
//! # Contents
//! - Replacement buffers are fully admitted while the original remains charged.
//! - Refusal keeps physical directory, published ranges/epoch and both ledgers.
//! - Successful migration releases the actual original buffer exactly once.
//!
//! # Invariants
//! - Tests use the production link/build/publish path with verified bytecode.
//! - No logical-row proxy or artificial failed-allocation hook is involved.
//! - Persistent tombstone/node and buffer charges last until actual owner drop.

use super::*;
use otter_resource::ResourceLimits;

fn usage(account: &ResourceAccount) -> u64 {
    account
        .snapshot()
        .get(ResourceClass::SourceModuleBytes)
        .current()
}
fn module() -> BytecodeModule {
    crate::test_support::minimal_bytecode_module("directory.js")
}

#[test]
fn cross_account_buffer_growth_refusal_is_atomic_and_success_releases_only_old_capacity() {
    let old_account = ResourceAccount::default();
    let measured_account = ResourceAccount::default();
    let measured_space = Arc::new(CodeSpace::default());
    let old = measured_space
        .link_module(module(), SourceRegistry::default(), &old_account)
        .unwrap();
    let second = measured_space
        .link_module(module(), SourceRegistry::default(), &measured_account)
        .unwrap();
    let new_bytes = usage(&measured_account);
    let actual_capacity = measured_space.chunks().capacity();
    assert!(actual_capacity >= 2);
    drop(old);
    drop(second);
    drop(measured_space);
    assert_eq!(usage(&old_account), 0);
    assert_eq!(usage(&measured_account), 0);

    let space = Arc::new(CodeSpace::default());
    let first = space
        .link_module(module(), SourceRegistry::default(), &old_account)
        .unwrap();
    let old_bases = space.next_bases().unwrap();
    let (old_pointer, old_capacity, old_epoch, old_usage) = {
        let directory = space.chunks();
        assert!(directory.lease.as_ref().unwrap().belongs_to(&old_account));
        (
            directory.chunks.as_ptr(),
            directory.capacity(),
            space.epoch.load(Ordering::Acquire),
            usage(&old_account),
        )
    };
    let denied = ResourceAccount::new(
        ResourceLimits::builder()
            .limit(ResourceClass::SourceModuleBytes, new_bytes - 1)
            .build(),
    );
    let error = space
        .link_module(module(), SourceRegistry::default(), &denied)
        .unwrap_err();
    let BytecodeLinkError::RetainedBytes(ResourceError::Exhausted {
        requested,
        in_use,
        limit,
        class,
    }) = error
    else {
        panic!("exact physical replacement admission")
    };
    assert_eq!(class, ResourceClass::SourceModuleBytes);
    assert_eq!(
        requested,
        (old_capacity * 2 * std::mem::size_of::<Arc<CodeChunk>>()) as u64
    );
    assert_eq!(in_use + requested, new_bytes);
    assert_eq!(limit, new_bytes - 1);
    assert_eq!(
        usage(&denied),
        0,
        "unpublished module/executable/node owners all roll back"
    );
    assert_eq!(
        usage(&old_account),
        old_usage,
        "the old physical buffer keeps its original lease after refusal"
    );
    let directory = space.chunks();
    assert_eq!(directory.chunks.as_ptr(), old_pointer);
    assert_eq!(directory.capacity(), old_capacity);
    assert_eq!(directory.len(), 1);
    assert!(directory.lease.as_ref().unwrap().belongs_to(&old_account));
    drop(directory);
    assert_eq!(space.epoch.load(Ordering::Acquire), old_epoch);
    assert_eq!(space.next_bases().unwrap(), old_bases);
    assert!(matches!(
        space.resolve_chunk(old_bases.0),
        ChunkResolution::Unlinked
    ));

    let new_account = ResourceAccount::default();
    let second = space
        .link_module(module(), SourceRegistry::default(), &new_account)
        .unwrap();
    assert_eq!(second.function_base(), old_bases.0);
    assert_eq!(usage(&new_account), new_bytes);
    let directory = space.chunks();
    assert_eq!(directory.capacity(), actual_capacity);
    assert!(directory.lease.as_ref().unwrap().belongs_to(&new_account));
    assert_eq!(
        directory.lease.as_ref().unwrap().amount(),
        (directory.capacity() * std::mem::size_of::<Arc<CodeChunk>>()) as u64
    );
    assert_eq!(
        usage(&old_account),
        old_usage - (old_capacity * std::mem::size_of::<Arc<CodeChunk>>()) as u64
    );
    drop(directory);
    drop(first);
    drop(second);
    drop(space);
    assert_eq!(usage(&old_account), 0);
    assert_eq!(usage(&new_account), 0);
}
