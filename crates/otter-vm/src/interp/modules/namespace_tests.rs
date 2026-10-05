//! Namespace absence, allocation refusal and live export resolution.
//!
//! # Contents
//! - Unknown environments and cached hits stay distinct from actual heap-cap OOM.
//! - Namespace re-exports preserve the allocator cause through Get/GetOwn.
//! - Deferred namespace re-exports retain their own allocation failure.
//!
//! # Invariants
//! - Pressure uses the real heap's retained-byte ledger after a full collection.
//! - Failed creation publishes no namespace cache entry; a later retry succeeds.
//! - Module environments and successful namespaces use the existing traced maps.
//! - Source-local namespace allocation failures retain JavaScript disposition.
//!
//! # See also
//! - `super::Interpreter::get_or_create_module_namespace` owns cache publication.
//! - `crate::object_internal_ops::get` owns the namespace Get fork.

use crate::activation_stack::ActivationStack;
use crate::native_abi::CommittedValueError;
use crate::{Interpreter, Value, VmError, VmGetOutcome, VmPropertyKey, object};
use std::collections::BTreeMap;
use std::sync::Arc;

const CAP: u64 = 4 * 1024 * 1024;

fn vm_oom(error: VmError) -> (u64, u64) {
    match error {
        VmError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => {
            assert!(requested_bytes > 0, "retain the actual allocation request");
            assert_eq!(heap_limit_bytes, CAP);
            (requested_bytes, heap_limit_bytes)
        }
        other => panic!("expected actual namespace allocation OOM, got {other:?}"),
    }
}

fn local_oom(error: CommittedValueError) -> (u64, u64) {
    match error {
        CommittedValueError::JavaScript(error) => vm_oom(error),
        CommittedValueError::Fatal(error) => {
            panic!("local namespace allocation became terminal: {error:?}")
        }
    }
}

fn fill_cap(vm: &mut Interpreter) -> u64 {
    vm.force_gc()
        .expect("settle live bootstrap and module roots");
    let reserved = CAP - vm.gc_heap.tracked_bytes();
    vm.gc_heap
        .reserve_bytes_no_collect(reserved)
        .expect("retain exact headroom without setup collection");
    assert_eq!(vm.gc_heap.tracked_bytes(), CAP);
    reserved
}

#[test]
fn unknown_environment_cached_hit_and_actual_refusal_have_distinct_results() {
    let mut vm = Interpreter::with_string_heap_cap(CAP).expect("fixture bootstrap");
    vm.gc_heap.set_gc_stress(0, false);
    vm.with_handle_scope(|vm, scope| {
        let env = vm.scoped_object_bare(scope).unwrap();
        let marker = vm.scoped_string(scope, "live module binding").unwrap();
        vm.scoped_set(scope, env, "marker", marker).unwrap();
        let env = vm.escape_scoped(env).as_object().unwrap();
        vm.register_module_env(Arc::from("otter:namespace-target"), env);

        let reserved = fill_cap(vm);
        let before = vm.gc_heap.gc_cycle_counts();
        assert!(
            vm.get_or_create_module_namespace("otter:unknown")
                .unwrap()
                .is_none()
        );
        assert_eq!(vm.gc_heap.gc_cycle_counts(), before);
        let cause = vm
            .get_or_create_module_namespace("otter:namespace-target")
            .expect_err("actual namespace creation must exceed the live cap");
        assert!(cause.requested_bytes() > 0);
        assert_eq!(cause.heap_limit_bytes(), CAP);
        assert!(vm.gc_heap.gc_cycle_counts().1 > before.1);
        assert!(!vm.module_namespaces.contains_key("otter:namespace-target"));
        vm.gc_heap.release_bytes(reserved);

        let namespace = vm
            .get_or_create_module_namespace("otter:namespace-target")
            .unwrap()
            .expect("retry uses the registered environment");
        let namespace = vm.scoped_value(scope, Value::object(namespace));
        let env = object::module_namespace_env(
            vm.escape_scoped(namespace).as_object().unwrap(),
            &vm.gc_heap,
        )
        .unwrap();
        assert_eq!(vm.module_env("otter:namespace-target"), Some(env));
        assert_eq!(
            object::get(env, &vm.gc_heap, "marker"),
            Some(vm.escape_scoped(marker))
        );

        let reserved = fill_cap(vm);
        let before = vm.gc_heap.gc_cycle_counts();
        let cached = vm
            .get_or_create_module_namespace("otter:namespace-target")
            .unwrap()
            .unwrap();
        assert_eq!(Value::object(cached), vm.escape_scoped(namespace));
        assert!(
            vm.get_or_create_module_namespace("otter:unknown")
                .unwrap()
                .is_none()
        );
        assert_eq!(vm.gc_heap.gc_cycle_counts(), before);
        vm.gc_heap.release_bytes(reserved);
    });
}

#[test]
fn namespace_and_deferred_reexports_preserve_actual_local_allocation_causes() {
    let mut vm = Interpreter::with_string_heap_cap(CAP).expect("fixture bootstrap");
    vm.gc_heap.set_gc_stress(0, false);
    vm.with_handle_scope(|vm, scope| {
        for url in ["otter:namespace-target", "otter:namespace-facade"] {
            let env = vm.scoped_object_bare(scope).unwrap();
            let env = vm.escape_scoped(env).as_object().unwrap();
            vm.register_module_env(Arc::from(url), env);
        }
        vm.register_module_resolved_exports(
            Arc::from("otter:namespace-facade"),
            BTreeMap::from([
                (
                    "ns".into(),
                    (Arc::from("otter:namespace-target"), "*namespace*".into()),
                ),
                (
                    "deferred".into(),
                    (
                        Arc::from("otter:deferred-target"),
                        "*deferred-namespace*".into(),
                    ),
                ),
            ]),
        );
        let facade = vm
            .get_or_create_module_namespace("otter:namespace-facade")
            .unwrap()
            .unwrap();
        let facade = vm.scoped_value(scope, Value::object(facade));
        let reserved = fill_cap(vm);
        let cause = vm
            .get_or_create_module_namespace("otter:namespace-target")
            .expect_err("uncached namespace at a full live cap");
        let cause = (cause.requested_bytes(), cause.heap_limit_bytes());
        assert!(cause.0 > 0);
        assert_eq!(cause.1, CAP);
        assert_eq!(
            vm_oom(
                vm.resolve_module_binding("otter:namespace-facade", "ns")
                    .expect_err("binding resolution cannot erase allocation failure")
            ),
            cause
        );
        let facade_object = vm.escape_scoped(facade).as_object().unwrap();
        assert_eq!(
            vm_oom(
                vm.module_namespace_get_binding(facade_object, "ns")
                    .expect_err("namespace binding cannot become undefined")
            ),
            cause
        );
        let mut stack = ActivationStack::new();
        let current = vm.escape_scoped(facade);
        let get_error = match vm.ordinary_get_value(
            &mut stack,
            None,
            current,
            current,
            &VmPropertyKey::String("ns"),
            0,
        ) {
            Err(error) => error,
            Ok(_) => panic!("Get erased the actual namespace OOM"),
        };
        assert_eq!(local_oom(get_error), cause);
        let current = vm.escape_scoped(facade);
        let descriptor_error = match vm.ordinary_get_own_property_descriptor_value(
            &mut stack,
            None,
            current,
            &VmPropertyKey::String("ns"),
            0,
        ) {
            Err(error) => error,
            Ok(_) => panic!("GetOwn erased the actual namespace OOM"),
        };
        assert_eq!(local_oom(descriptor_error), cause);
        vm_oom(
            vm.resolve_module_binding("otter:namespace-facade", "deferred")
                .expect_err("deferred allocation cannot become an absent export"),
        );
        assert!(!vm.module_namespaces.contains_key("otter:namespace-target"));
        assert!(!vm.deferred_namespaces.contains_key("otter:deferred-target"));
        assert!(
            vm.resolve_module_binding("otter:namespace-facade", "unknown")
                .unwrap()
                .is_none()
        );
        vm.gc_heap.release_bytes(reserved);

        let current = vm.escape_scoped(facade);
        let value = match vm
            .ordinary_get_value(
                &mut stack,
                None,
                current,
                current,
                &VmPropertyKey::String("ns"),
                0,
            )
            .unwrap()
        {
            VmGetOutcome::Value(value) => value,
            VmGetOutcome::InvokeGetter { .. } => panic!("namespace export is a live data binding"),
        };
        assert!(value.as_object().is_some());
        assert_eq!(
            vm.get_or_create_module_namespace("otter:namespace-target")
                .unwrap()
                .map(Value::object),
            Some(value)
        );
    });
}
