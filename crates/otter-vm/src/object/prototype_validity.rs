//! Shared proofs of ordinary prototype chains.
//!
//! # Contents
//! - [`PrototypeValidity`] owns one address-stable validity word.
//! - [`PrototypeWatchpoints`] records a prototype's current chain proof and
//!   weak subscriptions to proofs which depend on that prototype.
//! - [`chain_validity`] captures or reuses a complete ordinary-chain proof.
//!
//! # Invariants
//! - Invalid cells never become valid again. Rebuilding a proof allocates a
//!   different cell, and every compiled consumer retains its own cell.
//! - Registrations contain no GC pointers. Moving objects retain their
//!   sidecars, and weak subscriptions cannot keep a dead proof alive.
//! - Capture and mutation run on the mutator. The atomic validity word is the
//!   only state a compiler worker or generated guard may read.
//! - Capture neither allocates GC cells nor invokes JavaScript. Every ordinary
//!   prototype already owns a sidecar installed with its instance root.
//! - Snapshot restore starts with empty watchpoints; cells and subscriptions
//!   are owned by one isolate and never copied from a captured heap image.
//!
//! # See also
//! - [`super::shape_body`] for prototype identity and traced holder references.
//! - [`super::shape_transition`] for stores depending on absent inherited keys.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};

use super::{JsObject, ObjectPrototype, ShapeId};

/// Address-stable proof retained by ICs and code generations.
#[derive(Debug)]
pub(crate) struct PrototypeValidity {
    valid: AtomicU32,
    identity: ShapeId,
}

impl PartialEq for PrototypeValidity {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Eq for PrototypeValidity {}

/// Receiver-owned slot or a chain-proven inherited method holder.
#[derive(Debug, Clone)]
pub(crate) struct MethodLookupProof {
    pub(crate) validity: Option<Arc<PrototypeValidity>>,
    pub(crate) holder_root: ShapeId,
}

impl MethodLookupProof {
    #[cfg(test)]
    pub(crate) fn own() -> Self {
        Self {
            validity: None,
            holder_root: ShapeId::UNASSIGNED,
        }
    }

    pub(crate) fn same(&self, other: &Self) -> bool {
        self.holder_root == other.holder_root && self.validity == other.validity
    }
}

impl PrototypeValidity {
    fn new(identity: ShapeId) -> Arc<Self> {
        Arc::new(Self {
            valid: AtomicU32::new(1),
            identity,
        })
    }

    pub(crate) fn is_valid(&self) -> bool {
        self.valid.load(Ordering::Acquire) != 0
    }

    fn invalidate(&self) {
        self.valid.store(0, Ordering::Release);
    }

    pub(crate) fn address(&self) -> usize {
        &self.valid as *const AtomicU32 as usize
    }

    pub(crate) fn identity(&self) -> u64 {
        self.identity.raw()
    }
}

#[derive(Debug, Default)]
struct WatchpointState {
    current: Option<Arc<PrototypeValidity>>,
    subscribers: Vec<Weak<PrototypeValidity>>,
}

/// Cold mutation and capture state, owned by a prototype's sidecar.
#[derive(Debug, Default)]
pub(super) struct PrototypeWatchpoints(Mutex<WatchpointState>);

impl PrototypeWatchpoints {
    pub(super) fn invalidate(&self) {
        let mut state = self.0.lock().expect("prototype watchpoints");
        if let Some(cell) = state.current.take() {
            cell.invalidate();
        }
        for subscriber in state.subscribers.drain(..) {
            if let Some(cell) = subscriber.upgrade() {
                cell.invalidate();
            }
        }
    }

    fn current(&self) -> Option<Arc<PrototypeValidity>> {
        self.0
            .lock()
            .expect("prototype watchpoints")
            .current
            .as_ref()
            .filter(|cell| cell.is_valid())
            .cloned()
    }

    fn subscribe(&self, cell: &Arc<PrototypeValidity>) {
        let mut state = self.0.lock().expect("prototype watchpoints");
        state
            .subscribers
            .retain(|weak| weak.upgrade().is_some_and(|cell| cell.is_valid()));
        state.subscribers.push(Arc::downgrade(cell));
    }
}

/// Capture the chain starting at `first`, including that prototype itself.
/// An opaque or cyclic chain cannot supply an ordinary-chain proof.
pub(crate) fn chain_validity(
    first: JsObject,
    heap: &otter_gc::GcHeap,
) -> Option<Arc<PrototypeValidity>> {
    if let Some(cell) =
        heap.read_payload(first, |body| body.exotic()?.prototype_watchpoints.current())
    {
        return Some(cell);
    }

    let mut prototypes = Vec::new();
    let mut current = first;
    loop {
        if prototypes.len() == super::PROTO_CHAIN_HARD_CAP {
            return None;
        }
        let next = heap.read_payload(current, |body| {
            if body.chain_link_opaque() || body.exotic()?.instance_root.is_null() {
                return None;
            }
            Some(body.prototype())
        })?;
        prototypes.push(current);
        match next {
            ObjectPrototype::Null => break,
            ObjectPrototype::Object(next) => current = next,
            ObjectPrototype::Proxy(_) | ObjectPrototype::Value(_) => return None,
        }
    }

    let root = super::cached_instance_root(first, heap)?;
    let identity = heap.read_payload(root, super::shape_body::ShapeBody::id);
    let cell = PrototypeValidity::new(identity);
    for prototype in prototypes {
        heap.read_payload(prototype, |body| {
            body.exotic()
                .expect("registered prototype sidecar")
                .prototype_watchpoints
                .subscribe(&cell);
        });
    }
    heap.read_payload(first, |body| {
        body.exotic()
            .expect("registered prototype sidecar")
            .prototype_watchpoints
            .0
            .lock()
            .expect("prototype watchpoints")
            .current = Some(cell.clone());
    });
    Some(cell)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalidation_retires_every_dependent_chain() {
        let watches = PrototypeWatchpoints::default();
        let left = PrototypeValidity::new(ShapeId::UNASSIGNED);
        let right = PrototypeValidity::new(ShapeId::UNASSIGNED);
        watches.subscribe(&left);
        watches.subscribe(&right);
        watches.invalidate();
        assert!(!left.is_valid());
        assert!(!right.is_valid());
        let rebuilt = PrototypeValidity::new(ShapeId::UNASSIGNED);
        watches.subscribe(&rebuilt);
        assert!(rebuilt.is_valid());
        assert!(!left.is_valid());
        assert_ne!(left.address(), rebuilt.address());
    }

    #[test]
    fn subscriptions_do_not_retain_unused_proofs() {
        let watches = PrototypeWatchpoints::default();
        let proof = PrototypeValidity::new(ShapeId::UNASSIGNED);
        let weak = Arc::downgrade(&proof);
        watches.subscribe(&proof);
        drop(proof);
        assert!(weak.upgrade().is_none());
        watches.invalidate();
    }

    #[test]
    fn restored_prototypes_own_independent_watchpoints() {
        let mut donor = crate::Interpreter::new();
        donor.gc_heap_mut().set_tenure_all(true);
        let mut prototype = donor.realm_intrinsics.object_prototype().unwrap();
        donor.migrate_slow_to_fast(&mut prototype);
        let original = chain_validity(prototype, donor.gc_heap()).unwrap();
        let snapshot = donor.capture_isolate_snapshot().unwrap();
        for _ in 0..3 {
            let mut restored = crate::Interpreter::from_isolate_snapshot(&snapshot).unwrap();
            let mut prototype = restored.realm_intrinsics.object_prototype().unwrap();
            let proof = chain_validity(prototype, restored.gc_heap()).unwrap();
            assert!(!Arc::ptr_eq(&proof, &original));
            super::super::set(
                &mut prototype,
                restored.gc_heap_mut(),
                "restoredProof",
                crate::Value::boolean(true),
            );
            assert!(!proof.is_valid());
            assert!(original.is_valid());
            drop(restored);
            assert!(original.is_valid());
        }
    }
}
