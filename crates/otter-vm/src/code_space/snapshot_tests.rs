//! Physical code/source ownership through fallible capture, eviction and restore.
//!
//! # Contents
//! - Snapshot retention excludes donor mutable payload/executable/atom owners.
//! - Fresh restore tables have independent feedback, work counters and atoms.
//! - Rejected admissions preserve donor topology and release unpublished bytes.
//! - Escaped blocks and source-work cells retain their exact individual leases.
//!
//! # Invariants
//! - Every observed charge belongs to the actual allocation's resource account.
//! - Donor compiler/source storage is shared once; mutable tables are copied.
//! - Final owner drops release the remaining physical charge exactly once.
//!
//! # See also
//! - `super::CodeSpaceSnapshot` owns captured physical metadata and execution.

use super::*;
use crate::property_atom::NameInterner;
use otter_bytecode::{Constant, FunctionCodeBuilder, Op, Operand};
use otter_resource::ResourceLimits;

fn current(account: &ResourceAccount) -> u64 {
    account
        .snapshot()
        .get(ResourceClass::SourceModuleBytes)
        .current()
}

fn limited(limit: u64) -> ResourceAccount {
    ResourceAccount::new(
        ResourceLimits::builder()
            .limit(ResourceClass::SourceModuleBytes, limit)
            .build(),
    )
}

fn module() -> BytecodeModule {
    let mut module = crate::test_support::minimal_bytecode_module("snapshot.js");
    let mut code = FunctionCodeBuilder::new();
    code.push(Op::LoadUndefined, &[Operand::Register(0)]);
    code.push(
        Op::LoadProperty,
        &[
            Operand::Register(1),
            Operand::Register(0),
            Operand::ConstIndex(0),
        ],
    );
    code.push(Op::ReturnUndefined, &[]);
    module.functions[0].code = code.finish();
    module.constants.push(Constant::String {
        utf16: "payload".encode_utf16().collect(),
    });
    module
}

fn payload(space: &CodeSpace) -> Arc<ChunkPayload> {
    let ChunkResolution::Live { payload, .. } = space.resolve_chunk(0) else {
        panic!("live first payload")
    };
    payload
}

fn property_slot(context: &ExecutionContext) -> crate::feedback::PropertyFeedbackSlot<'_> {
    context
        .property_feedback_slot(0, 1, crate::property_ic::PropertyIcKind::Load)
        .unwrap()
}

#[test]
fn captured_code_shares_compiler_but_not_donor_execution_and_restores_independent_state() {
    let donor_account = ResourceAccount::default();
    let capture_account = ResourceAccount::default();
    let first_account = ResourceAccount::default();
    let second_account = ResourceAccount::default();
    let donor = Arc::new(CodeSpace::default());
    let context = donor
        .link_evictable_module(module(), SourceRegistry::default(), &donor_account)
        .unwrap();
    donor.resolve_atoms(&NameInterner::default());
    property_slot(&context).install(crate::property_ic::IcHandler::fixture_load(8));
    context.exec_function(0).unwrap().source_work().charge(37);
    let original = payload(&donor);
    let compiler = Arc::downgrade(&original.module);
    let old_payload = Arc::downgrade(&original);
    let old_executable = Arc::downgrade(&original.executable);
    let old_atoms = Arc::downgrade(&original.atoms);
    let compiler_bytes = original.module._lease.amount();
    let snapshot = donor.capture(&capture_account).unwrap();
    let captured = snapshot.chunks[0].payload.as_ref().unwrap();
    assert!(Arc::ptr_eq(&captured.module, &original.module));
    assert!(!Arc::ptr_eq(&captured.executable, &original.executable));
    assert_eq!(
        captured
            .executable
            .function(0)
            .unwrap()
            .property_feedback_at(1, crate::property_ic::PropertyIcKind::Load)
            .unwrap()
            .entry_count(),
        0
    );
    assert_eq!(
        captured
            .executable
            .function(0)
            .unwrap()
            .source_work()
            .total(),
        0
    );
    assert!(current(&capture_account) > 0);
    drop(original);
    drop(context);
    let candidate = donor.eviction_candidates().into_iter().next().unwrap();
    assert_eq!(
        donor.evict_candidate(candidate),
        ChunkEvictionResult::Evicted {
            retained_bytes: candidate.retained_bytes
        }
    );
    assert!(old_payload.upgrade().is_none());
    assert!(old_executable.upgrade().is_none());
    assert!(old_atoms.upgrade().is_none());
    assert!(compiler.upgrade().is_some());
    drop(donor);
    assert_eq!(
        current(&donor_account),
        compiler_bytes,
        "live snapshot keeps only donor immutable compiler allocation"
    );

    let first_names = NameInterner::default();
    let second_names = NameInterner::default();
    let _ = second_names.intern("unrelated preceding atom");
    let first = snapshot.restore(&first_names, &first_account).unwrap();
    let second = snapshot.restore(&second_names, &second_account).unwrap();
    let a = payload(&first);
    let b = payload(&second);
    assert!(Arc::ptr_eq(&a.module, &b.module));
    assert!(!Arc::ptr_eq(&a.executable, &b.executable));
    assert!(!Arc::ptr_eq(&a.atoms, &b.atoms));
    assert_ne!(
        first_names.lookup("payload"),
        second_names.lookup("payload")
    );
    let a_context = ExecutionContext::from_chunk_payload(Arc::clone(&a), 0, Arc::clone(&first));
    let b_context = ExecutionContext::from_chunk_payload(Arc::clone(&b), 0, Arc::clone(&second));
    assert_eq!(property_slot(&a_context).entry_count(), 0);
    assert_eq!(property_slot(&b_context).entry_count(), 0);
    property_slot(&a_context).install(crate::property_ic::IcHandler::fixture_load(8));
    a.executable.function(0).unwrap().source_work().charge(11);
    assert_eq!(property_slot(&a_context).entry_count(), 1);
    assert_eq!(property_slot(&b_context).entry_count(), 0);
    assert_eq!(b.executable.function(0).unwrap().source_work().total(), 0);
    assert_eq!(
        captured
            .executable
            .function(0)
            .unwrap()
            .source_work()
            .total(),
        0
    );
    drop(snapshot);
    assert_eq!(current(&capture_account), 0);
    assert_eq!(
        current(&donor_account),
        compiler_bytes,
        "restored compiler owners retain original single lease"
    );
    drop(a_context);
    drop(a);
    drop(first);
    assert_eq!(current(&first_account), 0);
    assert_eq!(current(&donor_account), compiler_bytes);
    drop(b_context);
    drop(b);
    drop(second);
    assert_eq!(current(&second_account), 0);
    assert_eq!(current(&donor_account), 0);
    assert!(compiler.upgrade().is_none());
}

#[test]
fn failed_capture_and_restore_admission_leave_original_tables_and_ids_unchanged() {
    let donor_account = ResourceAccount::default();
    let capture_account = ResourceAccount::default();
    let donor = Arc::new(CodeSpace::default());
    let context = donor
        .link_module(module(), SourceRegistry::default(), &donor_account)
        .unwrap();
    property_slot(&context).install(crate::property_ic::IcHandler::fixture_load(8));
    context.exec_function(0).unwrap().source_work().charge(37);
    let before = (
        donor.epoch.load(Ordering::Acquire),
        donor.chunks().len(),
        current(&donor_account),
    );
    let snapshot = donor.capture(&capture_account).unwrap();
    let capture_bytes = current(&capture_account);
    let refused_capture = limited(capture_bytes - 1);
    assert!(matches!(
        donor.capture(&refused_capture),
        Err(ResourceError::Exhausted { .. })
    ));
    assert_eq!(
        current(&refused_capture),
        0,
        "all unpublished capture owners roll back"
    );
    assert_eq!(
        (
            donor.epoch.load(Ordering::Acquire),
            donor.chunks().len(),
            current(&donor_account)
        ),
        before
    );
    assert_eq!(property_slot(&context).entry_count(), 1);
    assert_eq!(context.exec_function(0).unwrap().source_work().total(), 37);

    let probe_account = ResourceAccount::default();
    let probe = snapshot
        .restore(&NameInterner::default(), &probe_account)
        .unwrap();
    let restore_bytes = current(&probe_account);
    drop(probe);
    assert_eq!(current(&probe_account), 0);
    let refused_restore = limited(restore_bytes - 1);
    let names = NameInterner::default();
    assert!(matches!(
        snapshot.restore(&names, &refused_restore),
        Err(ResourceError::Exhausted { .. })
    ));
    assert_eq!(
        current(&refused_restore),
        0,
        "partial restored directory and tables remain unpublished"
    );
    assert_eq!(names.len(), 0, "failed restore never publishes atom IDs");
    assert_eq!(
        (
            donor.epoch.load(Ordering::Acquire),
            donor.chunks().len(),
            current(&donor_account)
        ),
        before
    );
    let fresh = snapshot.restore(&names, &probe_account).unwrap();
    assert!(matches!(
        fresh.resolve_chunk(0),
        ChunkResolution::Live {
            function_base: 0,
            ..
        }
    ));
    assert_eq!(fresh.chunks()[0].property_ic_site_base, 0);
    drop(fresh);
    drop(snapshot);
    drop(context);
    drop(donor);
    assert_eq!(current(&probe_account), 0);
    assert_eq!(current(&capture_account), 0);
    assert_eq!(current(&donor_account), 0);
}

#[test]
fn escaped_code_block_and_native_source_work_keep_their_individual_physical_leases() {
    let account = ResourceAccount::default();
    let space = Arc::new(CodeSpace::default());
    let context = space
        .link_evictable_module(module(), SourceRegistry::default(), &account)
        .unwrap();
    let block = context.executable_module().function_arc(0).unwrap();
    let work = Arc::clone(block.source_work());
    let block_bytes =
        std::mem::size_of::<crate::executable::CodeBlock>() as u64 + block.retained_bytes();
    let work_bytes = work.retained_bytes();
    let address = work.native_address();
    drop(context);
    let candidate = space.eviction_candidates().into_iter().next().unwrap();
    assert_eq!(
        space.evict_candidate(candidate),
        ChunkEvictionResult::Evicted {
            retained_bytes: candidate.retained_bytes
        }
    );
    drop(space);
    assert_eq!(current(&account), block_bytes + work_bytes);
    assert_eq!(work.native_address(), address);
    work.charge(41);
    assert_eq!(block.source_work().total(), 41);
    drop(block);
    assert_eq!(
        current(&account),
        work_bytes,
        "policy/native Arc owns the actual counter lease"
    );
    assert_eq!(work.native_address(), address);
    assert_eq!(work.total(), 41);
    drop(work);
    assert_eq!(current(&account), 0);
}
