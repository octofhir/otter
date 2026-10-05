//! Exact source-resource failures across scalar, owned and native boundaries.
//!
//! # Contents
//! - A real ledger refusal keeps its one typed ResourceError through projection.
//! - Malformed scalar/detail pairs are structural failures, without fake causes.
//!
//! # Invariants
//! - Fatal source limits never allocate or synthesize JavaScript exceptions.
//! - Every resource cause is produced by the existing ledger admission owner.
//!
//! # See also
//! - `crate::interp::errors` publishes the scalar/detail pair synchronously.
//! - `crate::native_function::vm_to_native_error` owns its native projection.

use crate::{ActivationStack, ErrorDetail, Interpreter, NativeError, VmError};
use otter_resource::{ResourceAccount, ResourceClass, ResourceLimits};

#[test]
fn actual_source_refusal_keeps_exact_owned_cause_without_materializing_js() {
    let account = ResourceAccount::new(
        ResourceLimits::builder()
            .limit(ResourceClass::SourceModuleBytes, 31)
            .build(),
    );
    let live = account
        .reserve_exact(ResourceClass::SourceModuleBytes, 23)
        .unwrap();
    let cause = account
        .reserve_exact(ResourceClass::SourceModuleBytes, 9)
        .unwrap_err();
    assert_eq!(cause.requested(), 9);
    assert_eq!(cause.in_use(), Some(23));
    let mut vm = Interpreter::new().expect("source boundary bootstrap");
    let before = vm.gc_heap.stats();
    let before_cycles = vm.gc_heap.gc_cycle_counts();
    let error = vm.err_resource(cause.clone());
    assert!(error.is_fatal());
    assert_eq!(error, VmError::ResourceLimit);
    assert_eq!(
        vm.error_detail(),
        Some(ErrorDetail::Resource(cause.clone()))
    );
    assert_eq!(vm.render_vm_error(&error), cause.to_string());
    let native = crate::native_function::vm_to_native_error(&vm, error, "source admission");
    assert_eq!(
        native,
        NativeError::Resource {
            error: cause.clone()
        }
    );
    let imported = crate::marshal::JsError::from_native(native);
    assert_eq!(
        imported.clone().into_native("outer boundary"),
        NativeError::Resource {
            error: cause.clone()
        }
    );
    let restored =
        crate::error_ops::native_to_vm_error(&mut vm, imported.into_native("outer boundary"));
    assert_eq!(restored, error);
    assert_eq!(
        vm.error_detail(),
        Some(ErrorDetail::Resource(cause.clone()))
    );
    assert_eq!(
        vm.vm_error_to_throwable_with_stack_roots(None, &ActivationStack::new(), &restored),
        Err(error),
    );
    assert_eq!(
        vm.error_detail(),
        Some(ErrorDetail::Resource(cause.clone())),
        "fatal projection leaves exact cause in flight"
    );
    assert_eq!(vm.gc_heap.stats().allocated_bytes, before.allocated_bytes);
    assert_eq!(vm.gc_heap.gc_cycle_counts(), before_cycles);
    drop(live);
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::SourceModuleBytes)
            .current(),
        0
    );
}

#[test]
fn malformed_resource_pair_uses_structural_identity_instead_of_stale_detail() {
    let vm = Interpreter::new().expect("source boundary bootstrap");
    assert_eq!(vm.take_error_detail(), None);
    assert_eq!(
        crate::native_function::vm_to_native_error(&vm, VmError::ResourceLimit, "missing detail"),
        NativeError::InvalidOperand,
    );
    vm.err_syntax("stale handled syntax".into());
    assert_eq!(
        crate::native_function::vm_to_native_error(&vm, VmError::ResourceLimit, "wrong detail"),
        NativeError::InvalidOperand,
    );
    assert!(
        !vm.render_vm_error(&VmError::ResourceLimit)
            .contains("stale handled syntax")
    );
    assert!(matches!(vm.error_detail(), Some(ErrorDetail::Message(_))));
}
