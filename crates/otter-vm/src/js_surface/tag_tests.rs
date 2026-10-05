//! Realm string-tag installation through real moving collection.
//!
//! # Contents
//! - Receiver relocation at every stress stride from 1 through 16.
//! - Standard attributes and honest descriptor rejection.
//!
//! # Invariants
//! - Priming cannot collect; installation must perform the observed collection.
//! - All values and runtime symbol tables use the production root visitor.

use super::*;

struct StressPrime {
    _word: u64,
}

impl otter_gc::SafeTraceable for StressPrime {
    const TYPE_TAG: u8 = 0xf7;
    fn trace_slots_safe(&mut self, _visitor: &mut otter_gc::raw::SlotVisitor<'_>) {}
}

#[test]
fn string_tag_builder_keeps_receiver_current_at_strides_1_to_16() {
    for stride in 1..=16 {
        let mut vm = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        vm.gc_heap.set_gc_stress(0, false);
        vm.with_runtime_roots(|vm| {
            let object = vm.alloc_runtime_rooted_object_with_roots(&[], &[]).unwrap();
            let mut receiver = Value::object(object);
            let original = receiver.as_raw_gc().unwrap().0;
            let mut roots = otter_gc::RootScope::new(&mut vm.gc_heap);
            // SAFETY: the receiver slot precedes this guard and stays stationary.
            unsafe { roots.add_value(&mut receiver) };
            vm.gc_heap.set_gc_stress(stride, false);
            let before = vm.gc_heap.gc_stats().clone();
            for _ in 1..stride {
                vm.gc_heap.alloc(StressPrime { _word: 0 }).unwrap();
            }
            assert_eq!(
                vm.gc_heap.gc_stats().minor_gc_cycles,
                before.minor_gc_cycles
            );
            let mut builder =
                ObjectBuilder::from_object(&mut vm.gc_heap, receiver.as_object().unwrap());
            builder
                .to_string_tag(&vm.well_known_symbols, "MovingNamespace")
                .unwrap();
            receiver = Value::object(builder.build());
            assert!(vm.gc_heap.gc_stats().minor_gc_cycles > before.minor_gc_cycles);
            assert_ne!(receiver.as_raw_gc().unwrap().0, original, "stride {stride}");
            let descriptor = object::get_own_symbol_descriptor(
                receiver.as_object().unwrap(),
                &vm.gc_heap,
                vm.well_known_symbols
                    .get(crate::symbol::WellKnown::ToStringTag),
            )
            .expect("installed realm tag");
            assert!(
                !descriptor.writable() && !descriptor.enumerable() && descriptor.configurable()
            );
            let object::DescriptorKind::Data { value } = descriptor.kind else {
                panic!("tag must be a data property");
            };
            assert_eq!(
                value
                    .as_string(&vm.gc_heap)
                    .unwrap()
                    .to_lossy_string(&vm.gc_heap),
                "MovingNamespace"
            );
        });
    }
}

#[test]
fn string_tag_builder_reports_non_extensible_rejection() {
    let mut vm = crate::Interpreter::new().expect("fixture interpreter bootstrap");
    vm.with_runtime_roots(|vm| {
        let mut object = vm.alloc_runtime_rooted_object_with_roots(&[], &[]).unwrap();
        object::prevent_extensions(&mut object, &mut vm.gc_heap).unwrap();
        let mut builder = ObjectBuilder::from_object(&mut vm.gc_heap, object);
        assert!(matches!(
            builder.to_string_tag(&vm.well_known_symbols, "Refused"),
            Err(JsSurfaceError::DefinePropertyFailed("@@toStringTag"))
        ));
        let object = builder.build();
        assert!(
            object::get_own_symbol_descriptor(
                object,
                &vm.gc_heap,
                vm.well_known_symbols
                    .get(crate::symbol::WellKnown::ToStringTag)
            )
            .is_none()
        );
    });
}
