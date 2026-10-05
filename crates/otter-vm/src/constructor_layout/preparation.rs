//! Successful source field preparation owned by one finalized receiver family.
//!
//! # Contents
//! - The existing prototype-chain proof paired with its reserved field count.
//! - Exact-root reuse and publication consumed by canonical preparation and JIT baking.
//!
//! # Invariants
//! This record contains no GC handle, receiver, function value or code owner.
//! Its enclosing old GC body accounts its physical bytes and releases it when
//! the actual family dies. Root replacement clears the record; descriptor,
//! deletion and prototype mutation retire the same address-stable proof.
//! Partial preparation and provisional roots never publish a reusable result.
//! Page restoration severs the copied Arc bytes before any trace or drop;
//! restored families establish independent current proofs on first preparation.
//! Reuse occurs only after the ordinary constructor prototype lookup and exact
//! actual-new.target family selection have already completed. First publication
//! follows complete ordinary-chain migration and shape/watchpoint registration.
//! A matching root/prototype and still-valid owned proof need no repeated
//! registration or separate current-proof capture; invalid cells never revive.
//!
//! # See also
//! - `crate::call_ops::constructor` owns the rooted canonical preparation.
//! - `crate::interp::jit_compile` bakes the same proof for both native targets.

use std::sync::Arc;

use super::ConstructorLayoutBody;
use crate::{
    Value,
    object::{ShapeHandle, prototype_validity::PrototypeValidity},
};

#[derive(Debug)]
pub(super) struct ConstructorPreparation {
    validity: Arc<PrototypeValidity>,
    reserved_fields: usize,
}

impl ConstructorLayoutBody {
    /// Migration can stop on allocation refusal. Only a complete ordinary
    /// chain admits a reusable result; a partially migrated chain retains
    /// canonical preparation and retries migration on its next use.
    /// This bounded read runs only at cold publication, never on a cached hit.
    pub(crate) fn preparation_chain_is_ordinary(
        selected_prototype: Value,
        heap: &otter_gc::GcHeap,
    ) -> bool {
        let Some(mut current) = selected_prototype.as_object() else {
            return false;
        };
        for _ in 0..crate::object::PROTO_CHAIN_HARD_CAP {
            let state = crate::object::state(current, heap);
            if state.is_dictionary() || state.is_opaque() {
                return false;
            }
            match crate::object::prototype_value(current, heap) {
                None => return true,
                Some(value) => match value.as_object() {
                    Some(next) => current = next,
                    None => return false,
                },
            }
        }
        false
    }

    /// Read a complete result only for this finalized current root and the
    /// actual prototype selected by canonical lookup. Both stored GC aliases
    /// are rewritten by the collector; no old moving identity is accepted.
    /// The owned proof was registered over the complete ordinary chain before
    /// publication, and every relevant mutation invalidates it permanently.
    pub(crate) fn prepared_capacity(
        &self,
        root: ShapeHandle,
        selected_prototype: Value,
    ) -> Option<usize> {
        if self.root != root || self.prototype != selected_prototype || !self.finalized() {
            return None;
        }
        self.preparation
            .as_ref()
            .filter(|prepared| prepared.validity.is_valid())
            .map(|prepared| prepared.reserved_fields)
    }

    /// Publish after entire-chain migration/registration and complete source
    /// field preparation. This cannot GC; the only stored pointer is a Rust
    /// Arc, never a moving heap offset.
    pub(crate) fn publish_preparation(
        &mut self,
        root: ShapeHandle,
        reserved_fields: usize,
        validity: Arc<PrototypeValidity>,
    ) {
        assert_eq!(
            self.root, root,
            "preparation must name the exact family root"
        );
        assert!(
            self.finalized(),
            "provisional roots cannot cache preparation"
        );
        assert!(
            validity.is_valid(),
            "publication requires the current chain proof"
        );
        self.preparation = Some(ConstructorPreparation {
            validity,
            reserved_fields,
        });
    }

    /// The shared baked allocation plan consumes this same current proof.
    pub(crate) fn preparation_validity(&self) -> Option<Arc<PrototypeValidity>> {
        self.finalized().then_some(())?;
        self.preparation
            .as_ref()
            .filter(|prepared| prepared.validity.is_valid())
            .map(|prepared| prepared.validity.clone())
    }
}

impl otter_gc::trace::SeverRestoredPayload for ConstructorLayoutBody {
    fn sever_restored_payload(&mut self) {
        // SAFETY: the page image copied a donor-owned Arc without cloning it.
        // Clear those copied bytes without reading or dropping the alias.
        // The donor remains its sole Rust owner; restored preparation rebuilds
        // against the restored prototype's independent watchpoints on use.
        unsafe { std::ptr::write(&mut self.preparation, None) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Interpreter, Value,
        native_abi::{Frame, VmFrameHeader},
        object,
    };
    use std::cell::Cell;

    #[test]
    fn family_source_bound_runs_only_for_creation_and_finalization_does_not_replay_it() {
        let mut vm = Interpreter::new().expect("constructor fixture interpreter");
        vm.with_handle_scope(|vm, scope| {
            let prototype = vm.scoped_object(scope).unwrap();
            let alternative = vm.scoped_object(scope).unwrap();
            let matches = Cell::new(0);
            let bound = |_: &Interpreter, _: Value| {
                matches.set(matches.get() + 1);
                4
            };
            let target = Value::function(1);
            let before = vm.gc_heap.gc_stats().by_type
                [super::super::CONSTRUCTOR_LAYOUT_BODY_TYPE_TAG as usize];
            let layout = vm
                .constructor_layout_for_receiver(1, target, vm.escape_scoped(prototype), bound)
                .unwrap();
            let after = vm.gc_heap.gc_stats().by_type
                [super::super::CONSTRUCTOR_LAYOUT_BODY_TYPE_TAG as usize];
            assert_eq!(after.alloc_count_total, before.alloc_count_total + 1);
            assert_eq!(
                after.alloc_bytes_total - before.alloc_bytes_total,
                otter_gc::space::align_alloc_size(
                    otter_gc::header::HEADER_SIZE + std::mem::size_of::<ConstructorLayoutBody>()
                ) as u64,
                "the sole old GC owner charges its complete preparation payload"
            );
            assert_eq!(matches.get(), 1);
            let provisional = vm.gc_heap.read_payload(layout, ConstructorLayoutBody::root);
            for _ in 0..7 {
                let receiver = vm
                    .alloc_runtime_rooted_object_with_shape(provisional, &[], &[])
                    .unwrap();
                let receiver = vm.scoped_value(scope, Value::object(receiver));
                let mut frame = Frame::new(
                    VmFrameHeader::interpreter(1, 0),
                    0,
                    target,
                    vm.escape_scoped(receiver),
                );
                frame.set_construct();
                frame.construct_layout = layout;
                frame.construct_receiver = vm.escape_scoped(receiver);
                vm.complete_constructor_layout(&mut frame);
                assert!(frame.construct_layout.is_null());
            }
            let finalized = vm
                .constructor_layout_for_receiver(1, target, vm.escape_scoped(prototype), bound)
                .unwrap();
            assert_eq!(finalized, layout);
            assert_eq!(
                matches.get(),
                1,
                "deferred finalization reuses the immutable bound"
            );
            assert!(
                vm.gc_heap
                    .read_payload(layout, ConstructorLayoutBody::finalized)
            );
            assert_eq!(
                object::shape_body::inline_capacity_of(
                    vm.gc_heap.read_payload(layout, ConstructorLayoutBody::root)
                ),
                4
            );
            let replaced = vm
                .constructor_layout_for_receiver(1, target, vm.escape_scoped(alternative), bound)
                .unwrap();
            assert_ne!(
                replaced, layout,
                "prototype replacement selects an actual new family"
            );
            assert_eq!(matches.get(), 2);
            assert_eq!(
                vm.gc_heap
                    .read_payload(layout, ConstructorLayoutBody::samples_remaining),
                0
            );
            assert_eq!(
                vm.gc_heap
                    .read_payload(replaced, ConstructorLayoutBody::samples_remaining),
                7
            );
        });
    }

    #[test]
    fn family_preparation_reuses_exact_proof_and_retires_on_real_descriptor_changes() {
        let mut vm = Interpreter::new().expect("constructor fixture interpreter");
        vm.gc_heap.set_gc_stress(0, true);
        vm.with_handle_scope(|vm, scope| {
            let prototype = vm.scoped_object(scope).unwrap();
            let target = Value::function(1);
            let layout = vm
                .constructor_layout_for_receiver(1, target, vm.escape_scoped(prototype), |_, _| 4)
                .unwrap();
            let provisional = vm.gc_heap.read_payload(layout, ConstructorLayoutBody::root);
            assert!(
                vm.gc_heap
                    .read_payload(layout, |body| body.preparation_validity())
                    .is_none()
            );
            for _ in 0..7 {
                let receiver = vm
                    .alloc_runtime_rooted_object_with_shape(provisional, &[], &[])
                    .unwrap();
                let receiver = vm.scoped_value(scope, Value::object(receiver));
                let mut frame = Frame::new(
                    VmFrameHeader::interpreter(1, 0),
                    0,
                    target,
                    vm.escape_scoped(receiver),
                );
                frame.set_construct();
                frame.construct_layout = layout;
                frame.construct_receiver = vm.escape_scoped(receiver);
                vm.complete_constructor_layout(&mut frame);
            }
            vm.constructor_layout_for_receiver(1, target, vm.escape_scoped(prototype), |_, _| {
                panic!("existing family does not match source again")
            })
            .unwrap();
            let root = vm.gc_heap.read_payload(layout, ConstructorLayoutBody::root);
            let mut proto = vm.escape_scoped(prototype).as_object().unwrap();
            vm.migrate_slow_to_fast(&mut proto);
            let proof = object::prototype_validity::chain_validity(proto, &vm.gc_heap).unwrap();
            assert!(ConstructorLayoutBody::preparation_chain_is_ordinary(Value::object(proto), &vm.gc_heap));
            vm.gc_heap.with_payload(layout, |body| {
                body.publish_preparation(root, 4, proof.clone())
            });
            assert_eq!(
                vm.gc_heap
                    .read_payload(layout, |body| body.prepared_capacity(root, Value::object(proto))),
                Some(4)
            );
            assert!(Arc::ptr_eq(
                &vm.gc_heap
                    .read_payload(layout, |body| body.preparation_validity())
                    .unwrap(),
                &proof
            ));
            assert_eq!(
                vm.gc_heap
                    .read_payload(layout, |body| body.prepared_capacity(provisional, Value::object(proto))),
                None
            );
            assert_eq!(
                vm.gc_heap.read_payload(layout, |body| body.prepared_capacity(root, Value::NULL)),
                None,
                "another selected prototype cannot reuse this family's preparation"
            );
            let before_proto = proto;
            let before_gc = vm.gc_heap.gc_stats().clone();
            vm.force_gc().expect("actual collection of rooted family/prototype");
            proto = vm.escape_scoped(prototype).as_object().unwrap();
            assert_ne!(proto, before_proto, "the selected young prototype actually moved");
            assert!(vm.gc_heap.gc_stats().gc_cycles > before_gc.gc_cycles);
            assert_eq!(vm.gc_heap.read_payload(layout, |body| body.prototype), Value::object(proto));
            assert!(matches!(object::shape_body::prototype_of(root), object::shape_body::ShapePrototype::Object(owner) if owner == proto), "the immutable root and family retain the same rewritten prototype");
            assert_eq!(vm.gc_heap.read_payload(layout, |body| body.prepared_capacity(root, Value::object(proto))), Some(4), "actual collection preserves the nonmoving validity owner");
            assert_eq!(vm.gc_heap.read_payload(layout, |body| body.prepared_capacity(root, Value::object(before_proto))), None, "pre-collection prototype identity is not the actual selected alias");
            let donor_count = Arc::strong_count(&proof);
            let family_id = vm.gc_heap.read_payload(layout, ConstructorLayoutBody::family_id);
            // Capture carries old space only: tenure every survivor first.
            vm.gc_heap.set_tenure_all(true);
            vm.force_gc().expect("tenuring collection before capture");
            vm.gc_heap.set_tenure_all(false);
            proto = vm.escape_scoped(prototype).as_object().unwrap();
            let snapshot = vm.capture_isolate_snapshot().expect("captured real finalized family");
            let restored = Interpreter::from_isolate_snapshot(&snapshot, &otter_resource::ResourceAccount::default()).expect("independent restored family");
            let mut saw_family = false;
            restored.gc_heap.for_each_live_payload::<ConstructorLayoutBody, _>(|_, body| {
                if body.family_id() == family_id {
                    saw_family = true;
                    assert!(body.finalized());
                    assert!(body.preparation_validity().is_none(), "restored page bytes must not own the donor's Arc proof");
                    assert!(body.prepared_capacity(body.root(), body.prototype).is_none(), "restore cannot reuse donor-owned preparation even with exact restored root/prototype");
                }
            });
            assert!(saw_family, "the snapshot contains the actual family payload");
            drop(restored);
            assert_eq!(Arc::strong_count(&proof), donor_count, "restore/drop cannot decrement an uncloned donor proof");
            assert_eq!(vm.gc_heap.read_payload(layout, |body| body.prepared_capacity(root, Value::object(proto))), Some(4));
            assert!(
                object::define_own_property_in_place(
                    &mut proto,
                    &mut vm.gc_heap,
                    "field",
                    object::PropertyDescriptor::data(Value::number_i32(7), true, true, true)
                )
                .unwrap()
            );
            assert!(!proof.is_valid());
            assert!(
                vm.gc_heap
                    .read_payload(layout, |body| body.preparation_validity())
                    .is_none()
            );
            assert_eq!(
                vm.gc_heap
                    .read_payload(layout, |body| body.prepared_capacity(root, Value::object(proto))),
                None
            );
            // Deletion retires the rebuilt proof as well. Scalar receiver
            // preparation cannot launder it into the new chain's identity.
            assert!(!ConstructorLayoutBody::preparation_chain_is_ordinary(Value::object(proto), &vm.gc_heap), "heap-only descriptor mutation leaves real dictionary metadata that must not admit a reusable result");
            let dictionary_proof = object::prototype_validity::chain_validity(proto, &vm.gc_heap).unwrap();
            assert!(dictionary_proof.is_valid(), "a chain proof alone does not prove migration completed");
            assert_eq!(vm.gc_heap.read_payload(layout, |body| body.prepared_capacity(root, Value::object(proto))), None, "dictionary-chain proof cannot revive retired family preparation");
            vm.migrate_slow_to_fast(&mut proto);
            assert!(ConstructorLayoutBody::preparation_chain_is_ordinary(Value::object(proto), &vm.gc_heap), "complete canonical migration admits later cold publication");
            assert!(!dictionary_proof.is_valid(), "adoption retires the partial-chain proof before publication");
            let rebuilt = object::prototype_validity::chain_validity(proto, &vm.gc_heap).unwrap();
            vm.gc_heap.with_payload(layout, |body| {
                body.publish_preparation(root, 0, rebuilt.clone())
            });
            assert_eq!(
                vm.gc_heap
                    .read_payload(layout, |body| body.prepared_capacity(root, Value::object(proto))),
                Some(0)
            );
            assert!(object::delete(&mut proto, &mut vm.gc_heap, "field").unwrap());
            assert!(!rebuilt.is_valid());
            assert_eq!(
                vm.gc_heap
                    .read_payload(layout, |body| body.prepared_capacity(root, Value::object(proto))),
                None
            );
        });
    }
}
