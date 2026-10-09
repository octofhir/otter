//! Constructor-owned seven-completion receiver layouts.
//!
//! # Contents
//! - [`ConstructorLayoutBody`] owns one exact base/new.target/prototype family.
//! - `owner` selects and publishes families on actual constructor objects.
//! - `completion` consumes canonical frame tickets without allocating.
//! - [`ConstructorFamilies`] is the isolate's weak family-identity index.
//!
//! # Invariants
//! - A family starts with 64 persistent in-object words on a provisional shape
//!   lineage. Its shapes are ordinary immutable layouts for every guard; no
//!   allocation plan ever allocates from a provisional root.
//! - Exactly seven terminal outer constructions sample their allocated receiver,
//!   including abrupt completion and explicit replacement-object returns. An
//!   ordinary call or a derived construction with no receiver supplies no sample.
//! - The family retains shapes/prototype and a non-owning base-id key, never an instance. Only
//!   the canonical active construct packet/frame owns the allocated receiver.
//! - Successful field preparation belongs to the exact family root. Its
//!   existing prototype proof invalidates on mutation; root publication clears
//!   the result. No per-root side map retains dead families or code.
//! - Completion records scalar slack only. A later rooted preparation may
//!   replace the active root with a newly allocated final lineage; metadata OOM
//!   leaves the provisional root usable and cannot change a completed result.
//! - First-seven cells and every old shape retain their immutable capacity.
//! - Layout cells live in old space. Their monotonic identity is the generated
//!   guard; no reclaimable or moving GC address is baked as family identity.
//! - Family identities are isolate-local and never reused. The weak index
//!   resolves an identity in O(1); the full collection's weak pass forgets
//!   unmarked cells, and a restored image re-registers its cells once.
//! - Each family records its actual owner (closure or class constructor and
//!   its function id) at creation; compilation never searches the heap.
//!
//! # See also
//! - [`crate::call_ops`] for receiver preparation.
//! - [`crate::native_abi::Frame`] for the canonical construct ticket.
//! - [`crate::object::ShapeState`] for lineage bakeability.

mod code_eviction;
mod compile;
mod completion;
mod lexical;
mod native_receiver;
pub use native_receiver::{constructor_receiver_commit, constructor_receiver_probe};
mod owner;
mod preparation;
pub use completion::constructor_terminal;
#[cfg(test)]
mod tests;

use crate::{
    Value,
    object::{ShapeHandle, ShapeState},
};
use otter_gc::raw::SlotVisitor;
use otter_gc::{Gc, GcHeap};
use rustc_hash::FxHashMap;
use std::cell::{Cell, RefCell};

/// Reserved private engine GC payload tag.
pub(crate) const CONSTRUCTOR_LAYOUT_BODY_TYPE_TAG: u8 = 0x40;
/// Number of allocated receivers whose terminal completion teaches slack.
pub(crate) const CONSTRUCTOR_LAYOUT_SAMPLES: u8 = 7;
/// The first-seven physical footprint, independent of static field matching.
pub(crate) const CONSTRUCTOR_PROVISIONAL_CAPACITY: u8 = 64;
/// Sole constructor family handle, traced by the owning function/class and
/// by each live canonical construction ticket.
pub(crate) type ConstructorLayout = Gc<ConstructorLayoutBody>;

/// Actual constructor object kind owning one family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum ConstructorFamilyOwner {
    /// A bound, proxy, native or bare-function owner: no generated fit.
    Other = 0,
    /// An ordinary closure owner.
    Closure = 1,
    /// A class constructor owner.
    Class = 2,
}

/// One actual constructor object's base-function/prototype family.
#[derive(Debug)]
#[repr(C)]
pub(crate) struct ConstructorLayoutBody {
    /// Never reused, even when a layout cell is collected.
    family_id: u64,
    /// The resolved prototype selected before receiver allocation.
    prototype: Value,
    /// The owner's next distinct base-function family, or null.
    next: ConstructorLayout,
    /// Provisional root initially; fresh final root after deferred publication.
    root: ShapeHandle,
    /// Exact source body allocating the receiver, not the wrapper's template id.
    base_function_id: u32,
    /// Function id of the actual new.target owner (closure or class ctor).
    owner_function_id: u32,
    /// Kind of the actual new.target owner.
    owner: ConstructorFamilyOwner,
    /// Set once the owner's head list drops this family (prototype
    /// replacement); a detached family never seeds a generated plan.
    detached: bool,
    /// Seven initially; zero permanently after the final terminal sample.
    samples_remaining: u8,
    /// Minimum unused words among the first seven allocated receivers.
    minimum_unused: u8,
    /// Source-proven static slots required by the whole base/derived family.
    required_slots: u8,
    /// Successful preparation of this exact finalized root. No receiver or
    /// moving handle is retained; mutation retires its existing chain proof.
    preparation: Option<preparation::ConstructorPreparation>,
}

pub(crate) const CONSTRUCTOR_LAYOUT_FAMILY_ID_OFFSET: usize =
    std::mem::offset_of!(ConstructorLayoutBody, family_id);
pub(crate) const CONSTRUCTOR_LAYOUT_ROOT_OFFSET: usize =
    std::mem::offset_of!(ConstructorLayoutBody, root);

impl ConstructorLayoutBody {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        family_id: u64,
        base_function_id: u32,
        owner: ConstructorFamilyOwner,
        owner_function_id: u32,
        required_slots: usize,
        prototype: Value,
        root: ShapeHandle,
        next: ConstructorLayout,
    ) -> Self {
        assert_eq!(
            crate::object::shape_body::inline_capacity_of(root),
            usize::from(CONSTRUCTOR_PROVISIONAL_CAPACITY)
        );
        assert!(crate::object::shape_body::state_of(root).is_provisional());
        Self {
            family_id,
            prototype,
            next,
            root,
            base_function_id,
            owner_function_id,
            owner,
            detached: false,
            samples_remaining: CONSTRUCTOR_LAYOUT_SAMPLES,
            minimum_unused: CONSTRUCTOR_PROVISIONAL_CAPACITY,
            required_slots: required_slots.min(usize::from(CONSTRUCTOR_PROVISIONAL_CAPACITY)) as u8,
            preparation: None,
        }
    }
    pub(crate) fn family_id(&self) -> u64 {
        self.family_id
    }
    pub(crate) fn root(&self) -> ShapeHandle {
        self.root
    }
    pub(crate) fn prototype(&self) -> Value {
        self.prototype
    }
    pub(crate) fn base_function_id(&self) -> u32 {
        self.base_function_id
    }
    pub(crate) fn owner(&self) -> (ConstructorFamilyOwner, u32) {
        (self.owner, self.owner_function_id)
    }
    pub(crate) fn detached(&self) -> bool {
        self.detached
    }
    pub(crate) fn samples_remaining(&self) -> u8 {
        self.samples_remaining
    }
    pub(crate) fn finalized(&self) -> bool {
        self.samples_remaining() == 0
            && !crate::object::shape_body::state_of(self.root).is_provisional()
    }
    pub(super) fn final_capacity(&self) -> usize {
        usize::from(
            (CONSTRUCTOR_PROVISIONAL_CAPACITY - self.minimum_unused).max(self.required_slots),
        )
    }
    /// Nonallocating, bounded, one-shot completion state change. Tickets own
    /// at-most-once delivery; samples after the seventh never change layout.
    pub(super) fn record_terminal(&mut self, own_slots: usize) -> bool {
        if self.samples_remaining == 0 {
            return false;
        }
        let used = own_slots.min(usize::from(CONSTRUCTOR_PROVISIONAL_CAPACITY)) as u8;
        self.minimum_unused = self
            .minimum_unused
            .min(CONSTRUCTOR_PROVISIONAL_CAPACITY - used);
        self.samples_remaining -= 1;
        self.samples_remaining == 0
    }
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        crate::code_liveness::visit_value(&self.prototype, visitor);
    }
}

impl otter_gc::SafeTraceable for ConstructorLayoutBody {
    const TYPE_TAG: u8 = CONSTRUCTOR_LAYOUT_BODY_TYPE_TAG;
    fn trace_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>) {
        use crate::pelt::PeltField as _;
        self.prototype.pelt_trace(visitor);
        self.next.pelt_trace(visitor);
        self.root.pelt_trace(visitor);
    }
}

/// Isolate-owned weak index from family identity to its old-space cell.
///
/// The index never roots a family: the owner's head list, live construction
/// tickets and compiled code roots do. Lookups between collections may return
/// an unmarked cell whose memory is still intact; callers that retain it root
/// it through their own owners before the next full collection.
#[derive(Debug)]
pub(crate) struct ConstructorFamilies {
    next_id: Cell<u64>,
    by_id: RefCell<FxHashMap<u64, ConstructorLayout>>,
}

impl Default for ConstructorFamilies {
    fn default() -> Self {
        Self {
            next_id: Cell::new(1),
            by_id: RefCell::new(FxHashMap::default()),
        }
    }
}

impl ConstructorFamilies {
    /// Reserve the next never-reused family identity.
    pub(crate) fn allocate_id(&self) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(
            id.checked_add(1)
                .expect("constructor family identity exhausted"),
        );
        id
    }

    /// Index one freshly allocated family cell under its identity.
    pub(crate) fn register(&self, heap: &GcHeap, layout: ConstructorLayout) {
        let id = heap.read_payload(layout, ConstructorLayoutBody::family_id);
        let replaced = self.by_id.borrow_mut().insert(id, layout);
        debug_assert!(replaced.is_none(), "family identities are never reused");
    }

    #[must_use]
    pub(crate) fn get(&self, family_id: u64) -> Option<ConstructorLayout> {
        self.by_id.borrow().get(&family_id).copied()
    }

    /// The full collection's weak pass: forget every family the marking left
    /// unreached before the sweep frees its cell.
    pub(crate) fn sweep_dead(&self, heap: &GcHeap) {
        self.by_id
            .borrow_mut()
            .retain(|_, layout| heap.is_marked(layout.raw()));
    }

    /// Rebuild the index from a restored heap; identities continue after the
    /// largest restored one.
    pub(crate) fn restored(heap: &GcHeap) -> Self {
        let families = Self::default();
        let cage_base = otter_gc::cage_base() as usize;
        let mut restored = Vec::new();
        heap.for_each_live_payload::<ConstructorLayoutBody, _>(|_space, body| {
            let header = (body as *const ConstructorLayoutBody as usize)
                - std::mem::size_of::<otter_gc::GcHeader>();
            let offset = (header - cage_base) as u32;
            // SAFETY: the payload walk visited a live layout cell at this
            // cage offset.
            let layout: ConstructorLayout = unsafe { Gc::from_offset(offset) };
            restored.push((body.family_id, layout));
        });
        let mut next = 1u64;
        {
            let mut by_id = families.by_id.borrow_mut();
            for (id, layout) in restored {
                next = next.max(id.saturating_add(1));
                by_id.insert(id, layout);
            }
        }
        families.next_id.set(next);
        families
    }
}

/// Detach every family of one owner's list starting at `head`, after its
/// prototype changed: none may seed a generated plan again, and the caller
/// clears the head so no generated family hit survives the change. In-flight
/// canonical tickets keep their detached family alive. Old layout cells and
/// no allocation: the walk cannot collect.
pub(crate) fn detach_families(heap: &mut GcHeap, head: ConstructorLayout) {
    let mut current = head;
    while !current.is_null() {
        current = heap.with_payload(current, |body| {
            body.detached = true;
            body.next
        });
    }
}

/// Best-effort deferred finalization. `layout` is reachable from a rooted
/// actual constructor or canonical construction packet throughout this call.
/// Returning false is metadata refusal, not a JavaScript error.
pub(crate) fn try_finalize_root(
    layout: ConstructorLayout,
    heap: &mut GcHeap,
    shapes: &crate::object::ShapeRuntime,
    roots: &mut otter_gc::heap::RootSlotVisitor<'_>,
) -> bool {
    let Some((prototype, capacity)) = heap.read_payload(layout, |body| {
        (body.samples_remaining == 0 && !body.finalized())
            .then(|| (body.prototype, body.final_capacity()))
    }) else {
        return false;
    };
    let prototype = match prototype.as_object() {
        Some(object) => crate::object::shape_body::ShapePrototype::Object(object),
        None if prototype.is_null() => crate::object::shape_body::ShapePrototype::Null,
        None => crate::object::shape_body::ShapePrototype::Value(prototype),
    };
    let Ok(root) = shapes.new_root(
        heap,
        prototype,
        capacity,
        ShapeHandle::null(),
        ShapeState::ORDINARY,
        roots,
    ) else {
        return false;
    };
    // No receiver/old shape is rewritten. The fresh final root deliberately
    // has no previous-layout link to the first-seven provisional lineage.
    heap.with_payload(layout, |body| {
        body.preparation = None;
        body.root = root;
    });
    heap.record_write(layout, &root);
    true
}
