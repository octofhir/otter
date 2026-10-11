//! Enduring intrinsic identity for canonical Array prototype objects.
//!
//! # Contents
//! - The canonical Array bootstrap assigns one immutable exotic scalar.
//! - Runtime classification reads that scalar without retaining a realm.
//! - Exact Array length reads share one non-observable metadata owner.
//!
//! # Invariants
//! - Only an actual canonical `%Array.prototype%` birth assigns the brand.
//! - Property, shape and prototype mutations cannot change intrinsic identity.
//! - Moving GC and heap restore preserve the scalar in existing exotic metadata.
//! - Ordinary object creation does not inherit a prototype's intrinsic brand.
//! - Canonical birth also acquires the monotonic prototype shape role, which
//!   excludes ordinary generated stores without restricting holder reads.
//!
//! # See also
//! - [`crate::intrinsics::array`] owns canonical prototype birth.
//! - [`crate::realm_intrinsics`] owns runtime prototype classification.

use super::JsObject;

/// Assign the intrinsic Array identity at canonical prototype birth.
pub(crate) fn mark_array_prototype(
    prototype: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
) -> Result<(), otter_gc::OutOfMemory> {
    super::ensure_exotic(prototype, heap)?;
    let state = super::shape_body::state_of(super::shape(*prototype, heap));
    super::state_transition::transition_state(prototype, heap, state.with_prototype_role(true))?;
    heap.with_payload(*prototype, |body| {
        body.exotic_mut().array_prototype_brand = true;
    });
    Ok(())
}

/// Read the canonical prototype's enduring intrinsic identity.
pub(crate) fn is_array_prototype(object: JsObject, heap: &otter_gc::GcHeap) -> bool {
    heap.read_payload(object, |body| {
        body.exotic()
            .is_some_and(|exotic| exotic.array_prototype_brand)
    })
}

impl crate::Interpreter {
    /// Logical length of an exact Array body or canonical Array prototype.
    ///
    /// Proxies do not pass this metadata operation. A canonical prototype has
    /// an own non-configurable data length, so reading it cannot enter a getter.
    pub(crate) fn exact_array_length(&self, value: crate::Value) -> Option<usize> {
        if let Some(array) = value.as_array() {
            return Some(crate::array::len(array, &self.gc_heap));
        }
        let object = value.as_object()?;
        if !self.is_array_prototype_object(object) {
            return None;
        }
        let descriptor = super::get_own_descriptor(object, &self.gc_heap, "length")?;
        let super::DescriptorKind::Data { value } = descriptor.kind else {
            return None;
        };
        let length = value.as_f64()?;
        (length >= 0.0 && length <= f64::from(u32::MAX) && length.fract() == 0.0)
            .then_some(length as usize)
    }
}

#[cfg(test)]
mod tests {
    use crate::{Interpreter, Value};

    #[test]
    fn escaped_array_prototype_retains_identity_after_realm_disposal_and_gc() {
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let realm = vm.create_host_realm().expect("additional realm");
        let (prototype_root, proxy_root) = vm
            .with_host_realm(realm, |vm| {
                Ok(vm.with_handle_scope(|vm, scope| {
                    let prototype = vm.realm_intrinsics.array_prototype().unwrap();
                    let prototype = vm.scoped_value(scope, Value::object(prototype));
                    let handler = vm.scoped_object(scope).expect("proxy handler");
                    let proxy = vm.scoped_proxy(scope, prototype, handler).expect("proxy");
                    let prototype_root = vm.persistent_root_insert(vm.escape_scoped(prototype));
                    let proxy_root = vm.persistent_root_insert(vm.escape_scoped(proxy));
                    (prototype_root, proxy_root)
                }))
            })
            .expect("retain foreign prototype");
        assert!(vm.dispose_host_realm(realm));
        assert!(vm.with_host_realm(realm, |_| Ok(())).is_err());
        vm.force_gc().expect("collect after realm disposal");
        let prototype = vm.persistent_root_get(prototype_root).unwrap();
        let proxy = vm.persistent_root_get(proxy_root).unwrap();
        assert!(vm.is_array(&prototype).unwrap());
        assert!(vm.is_array(&proxy).unwrap());
        assert!(crate::object::state(prototype.as_object().unwrap(), &vm.gc_heap).is_prototype());
        assert_eq!(vm.exact_array_length(prototype), Some(0));
        assert_eq!(vm.exact_array_length(proxy), None);
        vm.with_handle_scope(|vm, scope| {
            let prototype = vm.scoped_value(scope, vm.persistent_root_get(prototype_root).unwrap());
            let ordinary = vm
                .scoped_object_with_proto(scope, prototype)
                .expect("ordinary object inheriting Array prototype");
            assert!(!vm.is_array(&vm.escape_scoped(ordinary)).unwrap());
            assert_eq!(vm.exact_array_length(vm.escape_scoped(ordinary)), None);
            assert!(!crate::object::state(
                vm.escape_scoped(ordinary).as_object().unwrap(),
                &vm.gc_heap,
            ).is_prototype());
        });
        vm.persistent_root_remove(prototype_root).unwrap();
        vm.persistent_root_remove(proxy_root).unwrap();
    }

    #[test]
    fn canonical_array_brand_survives_snapshot_without_rebranding() {
        let snapshot = {
            let mut donor = Interpreter::new().expect("fixture interpreter bootstrap");
            donor.force_gc().expect("tenure before image capture");
            donor.capture_isolate_snapshot().expect("capture image")
        };
        let mut restored = Interpreter::from_isolate_snapshot(
            &snapshot,
            &otter_resource::ResourceAccount::default(),
        )
        .expect("restore image without bootstrap");
        restored.force_gc().expect("collect restored image");
        let prototype = restored.default_array_prototype().unwrap();
        assert!(super::is_array_prototype(prototype, &restored.gc_heap));
        assert!(crate::object::state(prototype, &restored.gc_heap).is_prototype());
        assert!(restored.is_array(&Value::object(prototype)).unwrap());
        assert_eq!(
            restored.exact_array_length(Value::object(prototype)),
            Some(0)
        );
    }
}
