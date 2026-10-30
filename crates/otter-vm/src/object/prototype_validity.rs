//! Shared proofs of ordinary prototype chains.
//!
//! # Contents
//! - [`PrototypeValidity`] owns one address-stable validity word and the
//!   flags of the generations subscribed to it.
//! - [`PrototypeWatchpoints`] records a prototype's current chain proof and
//!   weak subscriptions to proofs which depend on that prototype.
//! - [`chain_validity`] captures or reuses a complete ordinary-chain proof.
//!
//! # Invariants
//! - Invalid cells never become valid again. Rebuilding a proof allocates a
//!   different cell, and every compiled consumer retains its own cell.
//! - Invalidation sets every subscribed generation's dependency flag once.
//! - Registrations contain no GC pointers. Moving objects retain their
//!   sidecars, and weak subscriptions cannot keep a dead proof alive.
//! - Capture and mutation run on the mutator. The atomic validity word is the
//!   only state a compiler worker or generated guard may read.
//! - Capture neither allocates GC cells nor invokes JavaScript. Acquired
//!   immutable prototype-role state proves the sidecar/watchpoint registration
//!   published with its instance root; opaque links refuse the proof.
//! - Snapshot restore starts with empty watchpoints; cells and subscriptions
//!   are owned by one isolate and never copied from a captured heap image.
//!
//! # See also
//! - [`super::shape_body`] for prototype identity and traced holder references.
//! - [`super::shape_transition`] for stores depending on absent inherited keys.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};

use super::{JsObject, ObjectPrototype, ShapeId};
use crate::code_dependencies::DependencyFlag;

/// Address-stable proof retained by ICs and code generations.
///
/// `valid` is the first field, so a raw `Arc` pointer is also the address of
/// the validity word generated code loads.
#[derive(Debug)]
#[repr(C)]
pub(crate) struct PrototypeValidity {
    valid: AtomicU32,
    identity: ShapeId,
    /// Flags of installed generations that rely on this proof without
    /// reading `valid`; invalidation sets each one.
    subscribers: Mutex<Vec<Arc<DependencyFlag>>>,
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
    /// The proof of a slot the receiver owns: its hidden class alone fixes it.
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
            subscribers: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn is_valid(&self) -> bool {
        self.valid.load(Ordering::Acquire) != 0
    }

    fn invalidate(&self) {
        if self.valid.swap(0, Ordering::Release) != 0 {
            for flag in self
                .subscribers
                .lock()
                .expect("prototype validity subscribers")
                .drain(..)
            {
                flag.set();
            }
        }
    }

    /// Subscribe `flag` to this proof's invalidation.
    pub(crate) fn subscribe(&self, flag: &Arc<DependencyFlag>) {
        self.subscribers
            .lock()
            .expect("prototype validity subscribers")
            .push(Arc::clone(flag));
    }

    /// Drop every subscription of `flag`.
    pub(crate) fn unsubscribe(&self, flag: &Arc<DependencyFlag>) {
        self.subscribers
            .lock()
            .expect("prototype validity subscribers")
            .retain(|subscriber| !Arc::ptr_eq(subscriber, flag));
    }

    pub(crate) fn address(&self) -> usize {
        &self.valid as *const AtomicU32 as usize
    }

    pub(crate) fn identity(&self) -> u64 {
        self.identity.raw()
    }

    /// Transfer one strong count into a raw word an IC entry owns. The word
    /// is the validity word's address.
    pub(crate) fn into_raw(cell: Arc<Self>) -> u64 {
        Arc::into_raw(cell) as u64
    }

    /// Borrow the proof a raw IC word owns.
    ///
    /// # Safety
    /// `raw` must come from [`Self::into_raw`] and still own its count.
    pub(crate) unsafe fn borrow_raw<'a>(raw: u64) -> &'a Self {
        // SAFETY: the caller guarantees the word still owns a strong count.
        unsafe { &*(raw as *const Self) }
    }

    /// A new strong reference to the proof a raw IC word owns.
    ///
    /// # Safety
    /// As for [`Self::borrow_raw`].
    pub(crate) unsafe fn clone_raw(raw: u64) -> Arc<Self> {
        // SAFETY: the word owns one count, so the allocation is live.
        unsafe {
            Arc::increment_strong_count(raw as *const Self);
            Arc::from_raw(raw as *const Self)
        }
    }

    /// Release the count a raw IC word owns.
    ///
    /// # Safety
    /// `raw` must come from [`Self::into_raw`]; the word is dead afterwards.
    pub(crate) unsafe fn release_raw(raw: u64) {
        // SAFETY: the caller transfers the word's count back.
        drop(unsafe { Arc::from_raw(raw as *const Self) });
    }
}

const _: () = assert!(std::mem::offset_of!(PrototypeValidity, valid) == 0);

#[derive(Debug, Default)]
struct WatchpointState {
    current: Option<Arc<PrototypeValidity>>,
    subscribers: Vec<Weak<PrototypeValidity>>,
    /// Subscriber count at which the next subscription drops dead and
    /// invalidated entries: twice the live count the last sweep kept, so
    /// sweeps cost amortized O(1) per subscription.
    sweep_at: usize,
}

/// Fewest subscribers a prototype keeps before it sweeps dead entries.
const MIN_SWEEP_SUBSCRIBERS: usize = 16;

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
        if state.subscribers.len() >= state.sweep_at {
            state
                .subscribers
                .retain(|weak| weak.upgrade().is_some_and(|cell| cell.is_valid()));
            state.sweep_at = (state.subscribers.len() * 2).max(MIN_SWEEP_SUBSCRIBERS);
        }
        state.subscribers.push(Arc::downgrade(cell));
    }
}

/// A dictionary-mode object's slot-layout proof: one validity cell for the
/// current [`super::ExoticSlots`] slot-layout epoch, invalidated the moment
/// the epoch advances. Generated code that reads an existing key's slot
/// through a subscribed cell needs no epoch, shape or bounds guard of its own.
#[derive(Debug, Default)]
pub(super) struct LayoutWatchpoint(Mutex<Option<Arc<PrototypeValidity>>>);

impl LayoutWatchpoint {
    /// The layout changed: retire the current proof and its subscribers.
    pub(super) fn invalidate(&self) {
        if let Some(cell) = self.0.lock().expect("layout watchpoint").take() {
            cell.invalidate();
        }
    }

    /// The proof of the current layout, if one was created and still holds.
    pub(super) fn current(&self) -> Option<Arc<PrototypeValidity>> {
        let current = self.0.lock().expect("layout watchpoint");
        current.as_ref().filter(|cell| cell.is_valid()).cloned()
    }

    /// The proof of the current layout, created on first use.
    pub(super) fn current_or_new(&self, identity: ShapeId) -> Arc<PrototypeValidity> {
        let mut current = self.0.lock().expect("layout watchpoint");
        match current.as_ref().filter(|cell| cell.is_valid()) {
            Some(cell) => Arc::clone(cell),
            None => Arc::clone(current.insert(PrototypeValidity::new(identity))),
        }
    }
}

/// The proof the chain starting at `first` already has, creating none.
pub(crate) fn current_chain_validity(
    first: JsObject,
    heap: &impl crate::heap_read::HeapRead,
) -> Option<Arc<PrototypeValidity>> {
    heap.read_payload(first, |body| body.exotic()?.prototype_watchpoints.current())
}

/// Capture the chain starting at `first`, including that prototype itself.
/// An opaque or cyclic chain cannot supply an ordinary-chain proof.
pub(crate) fn chain_validity(
    first: JsObject,
    heap: &impl crate::heap_read::HeapRead,
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
            if !body.state().is_prototype()
                || body.chain_link_opaque()
                || body.exotic()?.instance_root.is_null()
            {
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
    fn invalidation_sets_subscribed_dependency_flags_once() {
        let watches = PrototypeWatchpoints::default();
        let proof = PrototypeValidity::new(ShapeId::UNASSIGNED);
        watches.subscribe(&proof);
        let subscribed = DependencyFlag::new();
        let retired = DependencyFlag::new();
        proof.subscribe(&subscribed);
        proof.subscribe(&retired);
        proof.unsubscribe(&retired);
        watches.invalidate();
        assert!(subscribed.is_set());
        assert!(!retired.is_set());
        assert!(proof.subscribers.lock().unwrap().is_empty());
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
        let mut donor = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        donor.gc_heap_mut().set_tenure_all(true);
        let mut prototype = donor.realm_intrinsics.object_prototype().unwrap();
        donor.migrate_slow_to_fast(&mut prototype);
        let original = chain_validity(prototype, donor.gc_heap()).unwrap();
        let snapshot = donor.capture_isolate_snapshot().unwrap();
        for _ in 0..3 {
            let mut restored = crate::Interpreter::from_isolate_snapshot(
                &snapshot,
                &otter_resource::ResourceAccount::default(),
            )
            .unwrap();
            let mut prototype = restored.realm_intrinsics.object_prototype().unwrap();
            let proof = chain_validity(prototype, restored.gc_heap()).unwrap();
            assert!(!Arc::ptr_eq(&proof, &original));
            restored
                .create_data_property(&mut prototype, "restoredProof", crate::Value::boolean(true))
                .expect("restored prototype property construction");
            assert!(!proof.is_valid());
            assert!(original.is_valid());
            drop(restored);
            assert!(original.is_valid());
        }
    }
}
