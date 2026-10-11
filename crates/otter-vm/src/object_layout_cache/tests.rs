//! Fixed RegExp layout retention and realm-independent record birth.
//!
//! # Contents
//! - Completed scopes retain all three layouts without record instances.
//! - Default and additional realms share only their null-prototype signatures.
//! - Warm hits leave the dynamic cache and heap allocation counters unchanged.
//!
//! # Invariants
//! Layout tokens contain ids rather than collector handles. Only the production
//! cache roots their final shapes after native scopes and realm execution end.
//! Every inspected instance is built through current native handles after full GC.
//!
//! # See also
//! - `super::ObjectLayoutCache` owns both fixed and dynamic layout roots.
//! - `crate::Interpreter::regexp_record_layout` prepares cold lineages.

use super::RegExpRecordLayout;
use crate::{Interpreter, NativeCallInfo, NativeCtx, ObjectLayout, object};

const KINDS: [RegExpRecordLayout; 3] = [
    RegExpRecordLayout::Match,
    RegExpRecordLayout::MatchWithIndices,
    RegExpRecordLayout::Indices,
];
const KEYS: [&[&str]; 3] = [
    &["index", "input", "groups"],
    &["index", "input", "groups", "indices"],
    &["groups"],
];

fn learn(vm: &mut Interpreter) -> [ObjectLayout; 3] {
    let dynamic_entries = vm.object_layout_cache.layouts.len();
    let layouts = NativeCtx::with_host_context(vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| {
            KINDS.map(|kind| scope.regexp_record_layout(kind).expect("fixed layout"))
        })
    });
    assert_eq!(vm.object_layout_cache.layouts.len(), dynamic_entries);
    for (layout, keys) in layouts.iter().zip(KEYS) {
        assert_eq!(layout.len(), keys.len());
    }
    layouts
}

fn inspect(vm: &mut Interpreter, layouts: [ObjectLayout; 3]) {
    NativeCtx::with_host_context(vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| {
            for (layout, keys) in layouts.into_iter().zip(KEYS) {
                let values: Vec<_> = keys
                    .iter()
                    .map(|key| scope.string(key).expect("rooted initial field"))
                    .collect();
                let record = scope
                    .object_with_layout(layout, &values)
                    .expect("final-shape record");
                scope.with_turn_parts(|vm, _| {
                    let record = vm.escape_scoped(record).as_object().unwrap();
                    let heap = vm.gc_heap();
                    assert!(object::prototype_value(record, heap).is_none());
                    assert_eq!(object::shape_id(record, heap), layout.shape_id());
                    let actual_keys: Vec<_> = object::shaped_string_key_values(record, heap)
                        .expect("shaped key order")
                        .into_iter()
                        .map(|key| key.as_string(heap).unwrap().to_lossy_string(heap))
                        .collect();
                    assert_eq!(actual_keys, keys);
                    assert!(object::own_symbol_key_values(record, heap).is_empty());
                    for key in keys {
                        let descriptor = object::get_own_descriptor(record, heap, key).unwrap();
                        assert!(descriptor.writable());
                        assert!(descriptor.enumerable());
                        assert!(descriptor.configurable());
                        let value = object::get_own(record, heap, key).unwrap();
                        assert_eq!(value.as_string(heap).unwrap().to_lossy_string(heap), *key);
                    }
                });
            }
        });
    });
}

#[test]
fn fixed_layouts_survive_completed_scopes_and_full_gc_without_instances() {
    for learn_in_additional_realm in [false, true] {
        let mut vm = Interpreter::new().expect("layout fixture bootstrap");
        let realm = vm.create_host_realm().expect("additional realm");
        let layouts = if learn_in_additional_realm {
            vm.with_host_realm(realm, |vm| Ok(learn(vm)))
                .expect("foreign layout preparation")
        } else {
            learn(&mut vm)
        };
        let shared = vm
            .with_host_realm(realm, |vm| Ok(learn(vm)))
            .expect("foreign layout reuse");
        assert_eq!(layouts, shared);
        assert_eq!(layouts, learn(&mut vm));
        assert_ne!(layouts[0], layouts[1]);
        assert_ne!(layouts[0], layouts[2]);
        assert_ne!(layouts[1], layouts[2]);
        assert!(vm.dispose_host_realm(realm));
        let full_before = vm.gc_heap_mut().gc_stats().gc_cycles;
        vm.force_gc().expect("full collection without records");
        assert!(vm.gc_heap_mut().gc_stats().gc_cycles > full_before);
        for layout in layouts {
            assert!(vm.shape_runtime.handle_for_id(layout.shape_id()).is_some());
            assert!(
                vm.object_layout_cache
                    .shape_ids()
                    .any(|id| id == layout.shape_id())
            );
        }
        assert_eq!(layouts, learn(&mut vm));
        inspect(&mut vm, layouts);
    }
}

#[test]
fn fixed_layout_hits_do_not_prepare_roots_or_dynamic_signatures() {
    let mut vm = Interpreter::new().expect("layout fixture bootstrap");
    let layouts = learn(&mut vm);
    let entries = vm.object_layout_cache.layouts.len();
    let before = vm.gc_heap().stats();
    let cycles = vm.gc_heap().gc_cycle_counts();
    for _ in 0..128 {
        for (kind, layout) in KINDS.into_iter().zip(layouts) {
            assert_eq!(vm.regexp_record_layout(kind).unwrap(), layout);
        }
    }
    let after = vm.gc_heap().stats();
    assert_eq!(after.allocated_bytes, before.allocated_bytes);
    assert_eq!(after.tracked_bytes, before.tracked_bytes);
    assert_eq!(after.reserved_bytes, before.reserved_bytes);
    assert_eq!(vm.gc_heap().gc_cycle_counts(), cycles);
    assert_eq!(vm.object_layout_cache.layouts.len(), entries);
    let cache = super::ObjectLayoutCache::default();
    assert!(KINDS.into_iter().all(|kind| cache.regexp(kind).is_none()));
    assert_eq!(cache.shape_ids().count(), 0);
}
