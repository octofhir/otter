//! Actual rooted family admission, relocation and mutation retirement.
//!
//! # Contents
//! - First-seven refusal, actual finalized own-prototype admission and GC.
//! - Wrong-base/lazy/prototype refusal and exact class-owner admission.
//!
//! # Invariants
//! The production semantic Probe is called on real collector-owned cells.
//! Every value that crosses allocation/collection lives in the native handle
//! arena; the returned integer ticket is used only in the no-GC test extent.

use super::*;
use crate::{
    Interpreter,
    constructor_layout::ConstructorLayoutBody,
    native_abi::{Frame, NativeResultDomain, NativeResultStatus, VmFrameHeader},
};

fn probe(vm: &Interpreter, target: Value, fid: u32) -> NativeResultPair {
    constructor_receiver_probe(
        &vm.gc_heap,
        target.to_bits(),
        Value::function(fid).to_bits(),
    )
}
fn miss(pair: NativeResultPair) {
    assert_eq!(
        pair.validate(NativeResultDomain::Probe),
        Some(NativeResultStatus::SideExit)
    );
    assert_eq!(pair.payload_bits(), 0);
}
fn hit(pair: NativeResultPair, layout: ConstructorLayout) {
    assert_eq!(
        pair.validate(NativeResultDomain::Probe),
        Some(NativeResultStatus::Success)
    );
    assert_eq!(
        pair.payload_value().as_i32().map(|n| n as u32),
        Some(layout.offset())
    );
}
fn finalize(vm: &mut Interpreter, target: Value, prototype: Value, fid: u32) -> ConstructorLayout {
    vm.with_handle_scope(|vm, scope| {
        let target = vm.scoped_value(scope, target);
        let prototype = vm.scoped_value(scope, prototype);
        let layout = vm
            .constructor_layout_for_receiver(
                fid,
                vm.escape_scoped(target),
                vm.escape_scoped(prototype),
                |_, _| 4,
            )
            .unwrap();
        let provisional = vm.gc_heap.read_payload(layout, ConstructorLayoutBody::root);
        for _ in 0..7 {
            let receiver = vm
                .alloc_runtime_rooted_object_with_shape(provisional, &[], &[])
                .unwrap();
            let receiver = vm.scoped_value(scope, Value::object(receiver));
            let mut frame = Frame::new(
                VmFrameHeader::interpreter(fid, 0),
                0,
                vm.escape_scoped(target),
                vm.escape_scoped(receiver),
            );
            frame.set_construct();
            frame.construct_layout = layout;
            frame.construct_receiver = vm.escape_scoped(receiver);
            vm.complete_constructor_layout(&mut frame);
        }
        vm.constructor_layout_for_receiver(
            fid,
            vm.escape_scoped(target),
            vm.escape_scoped(prototype),
            |_, _| panic!("existing family bound never reruns"),
        )
        .unwrap();
        let root = vm.gc_heap.read_payload(layout, ConstructorLayoutBody::root);
        let mut proto = vm.escape_scoped(prototype).as_object().unwrap();
        vm.migrate_slow_to_fast(&mut proto);
        assert!(ConstructorLayoutBody::preparation_chain_is_ordinary(
            Value::object(proto),
            &vm.gc_heap
        ));
        let validity = object::prototype_validity::chain_validity(proto, &vm.gc_heap).unwrap();
        vm.gc_heap
            .with_payload(layout, |body| body.publish_preparation(root, 4, validity));
        layout
    })
}

#[test]
fn pure_receiver_probe_uses_current_moving_aliases_and_revoked_real_proof() {
    let mut vm = Interpreter::new().expect("receiver probe fixture");
    vm.gc_heap.set_gc_stress(0, true);
    vm.with_handle_scope(|vm, scope| {
        let prototype = vm.scoped_object(scope).unwrap();
        let closure = crate::closure::alloc_closure_with_roots(
            &mut vm.gc_heap,
            9,
            Value::UNDEFINED,
            None,
            None,
            &mut |_| {},
        )
        .unwrap();
        let target = vm.scoped_value(scope, Value::closure(closure));
        miss(probe(vm, vm.escape_scoped(target), 9));
        let provisional = vm
            .constructor_layout_for_receiver(
                9,
                vm.escape_scoped(target),
                vm.escape_scoped(prototype),
                |_, _| 4,
            )
            .unwrap();
        let target_now = vm.escape_scoped(target).as_closure(&vm.gc_heap).unwrap();
        let proto_now = vm.escape_scoped(prototype);
        target_now.set_prototype_value(&mut vm.gc_heap, proto_now);
        miss(probe(vm, vm.escape_scoped(target), 9));
        let layout = finalize(vm, vm.escape_scoped(target), vm.escape_scoped(prototype), 9);
        assert_eq!(layout, provisional);
        hit(probe(vm, vm.escape_scoped(target), 9), layout);
        miss(probe(vm, vm.escape_scoped(target), 10));
        miss(probe(vm, Value::function(9), 9));
        let old_target = vm.escape_scoped(target);
        let old_prototype = vm.escape_scoped(prototype);
        let before = vm.gc_heap.gc_stats().gc_cycles;
        vm.force_gc().expect("actual moving family collection");
        assert!(vm.gc_heap.gc_stats().gc_cycles > before);
        assert_ne!(vm.escape_scoped(target), old_target);
        assert_ne!(vm.escape_scoped(prototype), old_prototype);
        hit(probe(vm, vm.escape_scoped(target), 9), layout);
        // A different current own prototype cannot reuse the old family,
        // even while its original instance-chain proof remains valid.
        let replacement = vm.scoped_object(scope).unwrap();
        let target_now = vm.escape_scoped(target).as_closure(&vm.gc_heap).unwrap();
        let replacement_now = vm.escape_scoped(replacement);
        target_now.set_prototype_value(&mut vm.gc_heap, replacement_now);
        miss(probe(vm, vm.escape_scoped(target), 9));
        let target_now = vm.escape_scoped(target).as_closure(&vm.gc_heap).unwrap();
        let prototype_now = vm.escape_scoped(prototype);
        target_now.set_prototype_value(&mut vm.gc_heap, prototype_now);
        hit(probe(vm, vm.escape_scoped(target), 9), layout);
        let mut proto = vm.escape_scoped(prototype).as_object().unwrap();
        assert!(
            object::define_own_property_in_place(
                &mut proto,
                &mut vm.gc_heap,
                "changed",
                object::PropertyDescriptor::data(Value::number_i32(1), true, true, true)
            )
            .unwrap()
        );
        miss(probe(vm, vm.escape_scoped(target), 9));
        let target_now = vm.escape_scoped(target).as_closure(&vm.gc_heap).unwrap();
        target_now.set_prototype_value(&mut vm.gc_heap, Value::NULL);
        miss(probe(vm, vm.escape_scoped(target), 9));
    });
}

#[test]
fn actual_class_owner_proves_split_base_independently_of_static_chain() {
    let mut vm = Interpreter::new().expect("class probe fixture");
    vm.gc_heap.set_gc_stress(0, true);
    vm.with_handle_scope(|vm, scope| {
        let prototype = vm.scoped_object(scope).unwrap();
        let statics = vm.scoped_object(scope).unwrap();
        let proto_now = vm.escape_scoped(prototype).as_object().unwrap();
        let statics_now = vm.escape_scoped(statics).as_object().unwrap();
        let base = crate::class_constructor::ClassConstructor::new_with_roots(
            &mut vm.gc_heap,
            Value::function(9),
            proto_now,
            statics_now,
            &mut |_| {},
        )
        .unwrap();
        let base = vm.scoped_value(scope, Value::class_constructor(base));
        let proto_now = vm.escape_scoped(prototype).as_object().unwrap();
        let statics_now = vm.escape_scoped(statics).as_object().unwrap();
        let child = crate::class_constructor::ClassConstructor::new_with_roots(
            &mut vm.gc_heap,
            Value::function(10),
            proto_now,
            statics_now,
            &mut |_| {},
        )
        .unwrap();
        let child = vm.scoped_value(scope, Value::class_constructor(child));
        // No static ancestry connects ctor 10 to the entered base 9. The
        // actual child owner nevertheless has a complete canonical base-9
        // family, exactly as with a split Reflect.construct new.target.
        let layout = finalize(vm, vm.escape_scoped(child), vm.escape_scoped(prototype), 9);
        hit(probe(vm, vm.escape_scoped(child), 9), layout);
        let child_now = vm.escape_scoped(child).as_class_constructor().unwrap();
        let base_now = vm.escape_scoped(base);
        child_now.set_ctor_proto(&mut vm.gc_heap, base_now);
        hit(probe(vm, vm.escape_scoped(child), 9), layout);
        miss(probe(vm, vm.escape_scoped(child), 11));
        // The static chain is not the instance chain: even a synthetic cycle
        // cannot revoke this exact entered-base/own-prototype family proof.
        let child_now = vm.escape_scoped(child);
        child_now
            .as_class_constructor()
            .unwrap()
            .set_ctor_proto(&mut vm.gc_heap, child_now);
        hit(probe(vm, vm.escape_scoped(child), 9), layout);
        let mut proto = vm.escape_scoped(prototype).as_object().unwrap();
        assert!(
            object::define_own_property_in_place(
                &mut proto,
                &mut vm.gc_heap,
                "changed",
                object::PropertyDescriptor::data(Value::number_i32(1), true, true, true),
            )
            .unwrap()
        );
        miss(probe(vm, vm.escape_scoped(child), 9));
        let pair = constructor_receiver_probe(
            std::ptr::null(),
            child_now.to_bits(),
            Value::function(9).to_bits(),
        );
        miss(pair);
    });
}

#[test]
fn receiver_leaves_use_one_existing_pair_and_exact_physical_argument_domains() {
    use crate::native_abi::{
        RuntimeStubClass, RuntimeStubResultAbi, RuntimeStubSafepoint, RuntimeStubSignature,
    };
    let probe = crate::native_abi::STUB_CONSTRUCTOR_RECEIVER_PROBE;
    assert_eq!(probe.class, RuntimeStubClass::LeafNoAlloc);
    assert_eq!(probe.signature, RuntimeStubSignature::LeafValue2);
    assert_eq!(probe.argument_count, 2);
    assert_eq!(probe.result_domain, NativeResultDomain::Probe);
    assert_eq!(probe.result_abi, RuntimeStubResultAbi::NativePair);
    assert_eq!(probe.safepoint, RuntimeStubSafepoint::Forbidden);
    assert_eq!(probe.effects.bits(), 0);
    let commit = crate::native_abi::STUB_CONSTRUCTOR_RECEIVER_COMMIT;
    assert_eq!(commit.class, RuntimeStubClass::LeafNoAlloc);
    assert_eq!(commit.signature, RuntimeStubSignature::ContextWords);
    assert_eq!(commit.argument_count, 1);
    assert_eq!(commit.result_domain, NativeResultDomain::Committed);
    assert_eq!(commit.result_abi, RuntimeStubResultAbi::NativePair);
    assert_eq!(commit.safepoint, RuntimeStubSafepoint::Forbidden);
    assert!(!commit.class.can_allocate());
    assert!(!commit.class.can_reenter_js());
}
