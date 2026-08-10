//! Type-tag → trace function dispatch table.
//!
//! Tracing is the hot path of every GC cycle. Phase-1 dispatch is a
//! single indexed load + indirect call: the [`TraceTable`] is a
//! `[Option<TraceFn>; 256]` keyed by [`crate::header::GcHeader::type_tag`]
//! — no `Box<dyn>`, no `dyn Any`, no downcast.
//!
//! # Contents
//!
//! - [`TraceFn`] / [`EphemeronTraceFn`] — function-pointer
//!   signatures stored in the table.
//! - [`SlotVisitor`] — visitor type alias the marker / scavenger
//!   pass to a `TraceFn`. Each call hands the visitor a `*mut
//!   RawGc` so the GC can update the slot in place when an object
//!   moves.
//! - [`TraceTable`] — the 256-entry dispatch array; `register::<T>`
//!   is the public entry point.
//!
//! # Invariants
//!
//! - Two registrations under the same `T::TYPE_TAG` must agree on
//!   the trace function. `register` enforces this with a
//!   `debug_assert`.
//! - A trace function may not allocate, may not run user JS, and
//!   may not enter the same heap recursively. Ordinary tracing
//!   must visit every strong [`crate::compressed::RawGc`] / `Gc<T>`
//!   slot. Ephemeron tracing must expose weak keys separately from
//!   conditionally-strong values.
//!
//! # See also
//!
//! - GC architecture plan §2.3 (TraceTable row), §6.1 (unsafe
//!   boundary).

use crate::compressed::RawGc;
use crate::header::GcHeader;

/// Visitor passed into a [`TraceFn`]. The argument is a pointer
/// to a slot holding a compressed offset; the GC may read the
/// offset, mark/copy the referenced object, and rewrite the slot
/// in place when the scavenger relocates an object.
pub type SlotVisitor<'a> = dyn FnMut(*mut RawGc) + 'a;

/// Visits value slots associated with one ephemeron key.
pub type EphemeronValueVisitor<'a> = dyn FnMut(&mut SlotVisitor<'_>) + 'a;

/// Visitor passed into an ephemeron trace function. The first
/// argument is a weak key slot; the second callback visits the
/// value slots that become strong only if that key has already
/// survived through ordinary reachability.
pub type EphemeronVisitor<'a> = dyn FnMut(*mut RawGc, &mut EphemeronValueVisitor<'_>) + 'a;

/// Function-pointer signature for `type_tag → trace` table
/// entries. The function reads the object's slots and yields
/// `*mut RawGc` to the visitor, one per child reference.
///
/// # Safety
///
/// Implementations require `header` to be a valid pointer to a
/// `GcHeader` whose payload is a `T` for which `T::TYPE_TAG`
/// matches `(*header).type_tag()`. The wrapper [`TraceTable::register`]
/// enforces this invariant by storing only generated wrappers
/// keyed by the registering type.
pub type TraceFn = unsafe fn(header: *mut GcHeader, visitor: &mut SlotVisitor<'_>);

/// Function-pointer signature for type-specific ephemeron tracing.
pub type EphemeronTraceFn = unsafe fn(header: *mut GcHeader, visitor: &mut EphemeronVisitor<'_>);

/// Trait every heap-allocated type implements so the GC knows how
/// to (a) tag its allocations and (b) walk its outgoing
/// references.
///
/// Implementations are registered with the GC through
/// [`crate::heap::GcHeap::register_traceable`], which wires
/// `T::TRACE_FN` into a [`TraceTable`] slot keyed by
/// `T::TYPE_TAG`.
///
/// **Downstream crates that keep `forbid(unsafe_code)`** (every
/// `crates/*` crate except `otter-gc` itself) cannot impl
/// this trait directly — `trace_slots` is `unsafe fn`. Such
/// crates impl [`SafeTraceable`] instead; a blanket impl below
/// lifts that into a `Traceable`.
pub trait Traceable: 'static {
    /// Unique 8-bit type tag — the table index. Implementations
    /// must coordinate to avoid collisions.
    const TYPE_TAG: u8;

    /// Walk every outgoing GC reference held by `self`, yielding
    /// the slot's address (`*mut RawGc`) to the visitor.
    ///
    /// # Safety
    ///
    /// `this` must be a valid pointer to a fully-constructed
    /// `Self` allocated by the GC. The implementation must:
    /// - not allocate inside the heap,
    /// - not retain references to the visitor,
    /// - not read past the object's payload.
    unsafe fn trace_slots(this: *mut Self, visitor: &mut SlotVisitor<'_>);

    /// Walk the outgoing references of a *pending* payload: the
    /// stack-resident value an allocation is about to copy into the
    /// heap, which a cap-triggered collection may have to see before
    /// the cell it will live in exists.
    ///
    /// This differs from [`Self::trace_slots`] for a body that carries
    /// a trailing array. That storage is part of the heap cell, not of
    /// `Self`, so a body whose trace walks it through a stored count
    /// must not walk it here: past the fixed part lies the caller's
    /// stack, and handing those words to the collector as slots hands
    /// it garbage. Such bodies override this to trace only their fixed
    /// part — and if that part is all count, to trace nothing.
    ///
    /// # Safety
    ///
    /// `this` must reference a fully-constructed `Self`, which unlike
    /// [`Self::trace_slots`] need not be in the heap. The same
    /// no-allocate, no-retain, no-read-past-the-payload rules apply,
    /// where "the payload" is `size_of::<Self>()` bytes.
    unsafe fn trace_pending_slots(this: *mut Self, visitor: &mut SlotVisitor<'_>) {
        // SAFETY: the caller upholds the contract above, which is the
        // `trace_slots` contract minus the in-heap requirement.
        unsafe { Self::trace_slots(this, visitor) }
    }

    /// Walk weak ephemeron entries. The default is no ephemeron
    /// edges. Collectors must not treat keys as ordinary strong
    /// slots; values become strong only when the key has already
    /// survived through another path.
    ///
    /// # Safety
    ///
    /// Same payload-validity contract as [`Self::trace_slots`].
    unsafe fn trace_ephemeron_slots(_this: *mut Self, _visitor: &mut EphemeronVisitor<'_>) {}
}

/// Reclamation-time finalizer hook for GC bodies.
///
/// The collector invokes [`Self::finalize_safe`] once on every dead body during
/// nursery reclamation or the full-GC sweep, **before** the body's `Drop` impl
/// runs and before its storage is reclaimed.
///
/// Most heap-allocated bodies do not need a finalizer — Rust's
/// `Drop` is enough to release per-field resources. Bodies impl
/// `SafeFinalize` only when they own GC-ordered cleanup work that
/// must observe the post-mark live set (host registry pruning,
/// external counter decrements, fast-flag teardown, …).
///
/// Bodies typically derive `SafeFinalize` through
/// `#[derive(Groom)]` in `otter-macros`; the derive emits a
/// finalizer that walks each non-skipped field through
/// `GroomField::groom`.
///
/// Bodies that opt in must register themselves with the heap via
/// [`crate::heap::GcHeap::register_finalize`] (typically through
/// the helper emitted alongside the derive). Unregistered bodies
/// skip the finalize step entirely — the sweep dispatch only fires
/// when the type-tag slot is populated.
/// Signature of a sweep-time host-ref release wrapper.
pub type HostReleaseFn = unsafe fn(*mut GcHeader, &mut crate::host_refs::HostRefTable);

/// Signature of a restore-time foreign-ownership sever wrapper.
pub type SeverRestoredFn = unsafe fn(*mut GcHeader);

/// Sweep-time hook for bodies that name entries in the isolate's
/// [`crate::host_refs::HostRefTable`]. A finalizer cannot release the
/// slot — it runs against the body alone, with no heap in reach — so
/// the sweep invokes this with the table before finalize and drop.
pub trait ReleaseHostRefs: SafeTraceable {
    /// Release every host-ref index this body holds.
    fn release_host_refs(&mut self, table: &mut crate::host_refs::HostRefTable);
}

/// Restore-time hook for bodies that own storage outside the heap.
/// A restored page carries the capture isolate's malloc pointers and
/// vtables verbatim; dereferencing any of them in another process is
/// unsound — including from the body's own trace impl during the
/// restore's relocation walk. The restore invokes this on every such
/// body BEFORE its first trace; the implementation must overwrite the
/// foreign fields (`std::ptr::write` — no drop, no read through the
/// old values) with owned, process-local state.
pub trait SeverRestoredPayload: SafeTraceable {
    /// Replace every foreign-owned field with process-local state.
    fn sever_restored_payload(&mut self);
}

pub trait SafeFinalize: SafeTraceable {
    /// Called by the sweeper on a dead body before
    /// `core::ptr::drop_in_place` runs. Must not allocate inside
    /// the GC heap, must not run user JavaScript, and must not
    /// re-enter the same heap.
    fn finalize_safe(&mut self);
}

/// Safe-only counterpart of [`Traceable`] — the trait downstream
/// crates that keep `forbid(unsafe_code)` (e.g. `otter-vm`) impl
/// to register a GC-managed type.
///
/// The blanket impl below converts every `SafeTraceable` into a
/// `Traceable`, so types only need to spell one trait. The
/// unsafe-fn body lives entirely in this crate.
pub trait SafeTraceable: 'static {
    /// Unique 8-bit type tag — the table index. Implementations
    /// must coordinate to avoid collisions.
    const TYPE_TAG: u8;

    /// Walk every outgoing GC reference owned by `self`,
    /// yielding the slot's address (`*mut RawGc`) to `visitor`.
    /// Must not allocate or retain the visitor (same contract
    /// as [`Traceable::trace_slots`], minus the pointer-validity
    /// precondition).
    fn trace_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>);

    /// Safe counterpart to [`Traceable::trace_pending_slots`]: what to
    /// trace when `self` is still the allocation's stack-resident
    /// payload and its trailing storage does not exist yet.
    ///
    /// A body with no trailing array keeps this default. A body that
    /// traces a trailing array must override it, or the collector walks
    /// the stack past the payload.
    fn trace_pending_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>) {
        self.trace_slots_safe(visitor);
    }

    /// Safe counterpart to [`Traceable::trace_ephemeron_slots`].
    /// Most heap objects are not ephemeron tables and keep this
    /// no-op implementation.
    fn trace_ephemeron_slots_safe(&mut self, _visitor: &mut EphemeronVisitor<'_>) {}
}

impl<T: SafeTraceable> Traceable for T {
    const TYPE_TAG: u8 = <Self as SafeTraceable>::TYPE_TAG;

    /// Bridge from the safe trait to the unsafe-fn `Traceable`.
    ///
    /// # Safety
    ///
    /// Inherits the [`Traceable::trace_slots`] contract — the
    /// caller (the GC's mark / scavenge dispatch) upholds it.
    unsafe fn trace_slots(this: *mut Self, visitor: &mut SlotVisitor<'_>) {
        // SAFETY: per the Traceable contract, `this` references
        // a fully-constructed `Self`; we re-borrow as `&Self`
        // for the duration of the safe call.
        unsafe {
            (*this).trace_slots_safe(visitor);
        }
    }

    unsafe fn trace_pending_slots(this: *mut Self, visitor: &mut SlotVisitor<'_>) {
        // SAFETY: same bridge contract as `trace_slots`, minus the
        // in-heap requirement.
        unsafe {
            (*this).trace_pending_slots_safe(visitor);
        }
    }

    unsafe fn trace_ephemeron_slots(this: *mut Self, visitor: &mut EphemeronVisitor<'_>) {
        // SAFETY: same bridge contract as `trace_slots`.
        unsafe {
            (*this).trace_ephemeron_slots_safe(visitor);
        }
    }
}

/// A 256-entry array of [`TraceFn`] pointers, indexed by
/// [`GcHeader::type_tag`]. Empty slots stay `None`.
pub struct TraceTable {
    table: [Option<TraceFn>; 256],
    ephemeron_table: [Option<EphemeronTraceFn>; 256],
    /// Drop functions used by the sweeper to invoke `T`'s `Drop`
    /// on dead objects (so e.g. boxed strings get their backing
    /// freed). `None` for plain-old-data types.
    drop_table: [Option<unsafe fn(*mut GcHeader)>; 256],
    /// Sweep-time host-ref release hooks, `None` for every other tag.
    /// Fires before `finalize_table`.
    host_release_table: [Option<HostReleaseFn>; 256],
    sever_restored_table: [Option<SeverRestoredFn>; 256],
    /// Sweep-time finalizers for bodies that impl [`SafeFinalize`].
    /// `None` for every other tag. Fires *before* `drop_table`.
    finalize_table: [Option<unsafe fn(*mut GcHeader)>; 256],
    /// Rust type name behind each tag, captured at registration.
    /// Diagnostics only — [`crate::census`] renders it so a heap
    /// table reads as type names rather than bare tag bytes.
    name_table: [Option<&'static str>; 256],
}

impl Default for TraceTable {
    fn default() -> Self {
        Self::new()
    }
}

impl TraceTable {
    /// Construct an empty table.
    pub const fn new() -> Self {
        Self {
            table: [None; 256],
            ephemeron_table: [None; 256],
            drop_table: [None; 256],
            finalize_table: [None; 256],
            host_release_table: [None; 256],
            sever_restored_table: [None; 256],
            name_table: [None; 256],
        }
    }

    /// Register a [`Traceable`] implementation. The wrapper
    /// `trace_wrapper` casts the raw header pointer to `*mut T`
    /// (skipping the header) and forwards to `T::trace_slots`.
    pub fn register<T: Traceable>(&mut self) {
        debug_assert!(
            T::TYPE_TAG != crate::header::FREE_TAG,
            "FREE_TAG is reserved for free-space fillers"
        );
        unsafe fn trace_wrapper<T: Traceable>(
            header: *mut GcHeader,
            visitor: &mut SlotVisitor<'_>,
        ) {
            // SAFETY: by the [`Traceable`] safety contract,
            // `header` precedes a valid `T` payload.
            unsafe {
                let payload = (header as *mut u8)
                    .add(std::mem::size_of::<GcHeader>())
                    .cast::<T>();
                T::trace_slots(payload, visitor);
            }
        }
        unsafe fn drop_wrapper<T: Traceable>(header: *mut GcHeader) {
            // SAFETY: header precedes a valid T payload.
            unsafe {
                let payload = (header as *mut u8)
                    .add(std::mem::size_of::<GcHeader>())
                    .cast::<T>();
                core::ptr::drop_in_place(payload);
            }
        }
        unsafe fn ephemeron_wrapper<T: Traceable>(
            header: *mut GcHeader,
            visitor: &mut EphemeronVisitor<'_>,
        ) {
            // SAFETY: by the [`Traceable`] safety contract,
            // `header` precedes a valid `T` payload.
            unsafe {
                let payload = (header as *mut u8)
                    .add(std::mem::size_of::<GcHeader>())
                    .cast::<T>();
                T::trace_ephemeron_slots(payload, visitor);
            }
        }
        let tag = T::TYPE_TAG as usize;
        if let Some(existing) = self.table[tag] {
            assert!(
                existing as *const () == trace_wrapper::<T> as *const (),
                "trace tag {tag} already registered with a different fn",
            );
        }
        self.table[tag] = Some(trace_wrapper::<T>);
        self.ephemeron_table[tag] = Some(ephemeron_wrapper::<T>);
        self.name_table[tag] = Some(std::any::type_name::<T>());
        // Only set drop if needed — saves one indirect call per
        // dead object on plain-old-data types.
        if std::mem::needs_drop::<T>() {
            self.drop_table[tag] = Some(drop_wrapper::<T>);
        }
    }

    /// Look up the trace function for a given type tag.
    #[inline]
    pub fn get(&self, tag: u8) -> Option<TraceFn> {
        self.table[tag as usize]
    }

    /// Rust type name registered under `tag`, or `None` when no
    /// type has been registered there yet.
    #[inline]
    #[must_use]
    pub fn name(&self, tag: u8) -> Option<&'static str> {
        self.name_table[tag as usize]
    }

    /// Whether a header could belong to a live body of a registered
    /// type.
    ///
    /// The debug verifiers use this to decide whether a slot holds a real
    /// reference or a stale word, so it must reject garbage without ever
    /// rejecting a body the heap can actually produce. The load-bearing
    /// test is the type tag: every allocated body registers one, and an
    /// arbitrary byte pattern almost never lands on a registered tag.
    ///
    /// Size is only checked for the two things it cannot be — zero, or
    /// running past the end of the cage. It deliberately carries no
    /// upper bound beyond that: a large-object region is sized to its
    /// body, so a string that owns its code units is legitimately
    /// megabytes long, and a fixed ceiling here would report every one of
    /// them as corruption.
    #[inline]
    #[must_use]
    pub fn header_could_be_live(&self, offset: u32, size_bytes: u32, type_tag: u8) -> bool {
        if size_bytes == 0 || self.get(type_tag).is_none() {
            return false;
        }
        u64::from(offset) + u64::from(size_bytes) <= crate::compressed::cage_size() as u64
    }

    /// Whether the type registered under `tag` needs dropping.
    ///
    /// A GC body needs dropping exactly when it owns something the heap
    /// does not — a `Vec`, a `Box`, an `Arc`, a hash table. That makes
    /// this the completeness check the slot walk cannot do: a field with
    /// no GC handles in it still holds a buffer a page image cannot
    /// carry, and a restored copy would be its second owner.
    #[inline]
    #[must_use]
    pub fn owns_outside_storage(&self, tag: u8) -> bool {
        self.drop_table[tag as usize].is_some()
    }

    /// Look up the drop function for a given type tag.
    #[inline]
    pub fn get_drop(&self, tag: u8) -> Option<unsafe fn(*mut GcHeader)> {
        self.drop_table[tag as usize]
    }

    /// Look up the finalize function for a given type tag.
    /// `None` when the tag has no [`SafeFinalize`] registration.
    #[inline]
    pub fn get_finalize(&self, tag: u8) -> Option<unsafe fn(*mut GcHeader)> {
        self.finalize_table[tag as usize]
    }

    /// Look up the host-ref release function for a given type tag.
    /// `None` when the tag has no [`ReleaseHostRefs`] registration.
    #[inline]
    pub fn get_host_release(&self, tag: u8) -> Option<HostReleaseFn> {
        self.host_release_table[tag as usize]
    }

    /// Register the host-ref release wrapper for a type that opts into
    /// [`ReleaseHostRefs`]. Must be paired with an earlier
    /// [`Self::register`] call for the same type tag.
    pub fn register_host_release<T: Traceable + ReleaseHostRefs>(&mut self) {
        unsafe fn release_wrapper<T: Traceable + ReleaseHostRefs>(
            header: *mut GcHeader,
            table: &mut crate::host_refs::HostRefTable,
        ) {
            // SAFETY: by the [`Traceable`] safety contract,
            // `header` precedes a valid `T` payload.
            unsafe {
                let payload = (header as *mut u8)
                    .add(std::mem::size_of::<GcHeader>())
                    .cast::<T>();
                (*payload).release_host_refs(table);
            }
        }
        let tag = <T as Traceable>::TYPE_TAG as usize;
        if let Some(existing) = self.host_release_table[tag] {
            debug_assert!(
                existing as *const () == release_wrapper::<T> as *const (),
                "host-release tag {tag} already registered with a different fn",
            );
        }
        self.host_release_table[tag] = Some(release_wrapper::<T>);
    }

    /// Look up the restore-time sever function for a type tag. `None`
    /// when the tag has no [`SeverRestoredPayload`] registration.
    #[inline]
    pub fn get_sever_restored(&self, tag: u8) -> Option<SeverRestoredFn> {
        self.sever_restored_table[tag as usize]
    }

    /// Register the restore-time sever wrapper for a type that opts
    /// into [`SeverRestoredPayload`]. Must be paired with an earlier
    /// [`Self::register`] call for the same type tag.
    pub fn register_sever_restored<T: Traceable + SeverRestoredPayload>(&mut self) {
        unsafe fn sever_wrapper<T: Traceable + SeverRestoredPayload>(header: *mut GcHeader) {
            // SAFETY: by the [`Traceable`] safety contract, `header`
            // precedes a valid `T` payload.
            unsafe {
                let payload = (header as *mut u8)
                    .add(std::mem::size_of::<GcHeader>())
                    .cast::<T>();
                (*payload).sever_restored_payload();
            }
        }
        let tag = <T as Traceable>::TYPE_TAG as usize;
        if let Some(existing) = self.sever_restored_table[tag] {
            debug_assert!(
                existing as *const () == sever_wrapper::<T> as *const (),
                "sever-restored tag {tag} already registered with a different fn",
            );
        }
        self.sever_restored_table[tag] = Some(sever_wrapper::<T>);
    }

    /// Register the finalize wrapper for a type that opts into
    /// [`SafeFinalize`]. Must be paired with an earlier
    /// [`Self::register`] call for the same type tag.
    pub fn register_finalize<T: Traceable + SafeFinalize>(&mut self) {
        unsafe fn finalize_wrapper<T: Traceable + SafeFinalize>(header: *mut GcHeader) {
            // SAFETY: by the [`Traceable`] safety contract,
            // `header` precedes a valid `T` payload.
            unsafe {
                let payload = (header as *mut u8)
                    .add(std::mem::size_of::<GcHeader>())
                    .cast::<T>();
                (*payload).finalize_safe();
            }
        }
        let tag = <T as Traceable>::TYPE_TAG as usize;
        if let Some(existing) = self.finalize_table[tag] {
            debug_assert!(
                existing as *const () == finalize_wrapper::<T> as *const (),
                "finalize tag {tag} already registered with a different fn",
            );
        }
        self.finalize_table[tag] = Some(finalize_wrapper::<T>);
    }

    /// Invoke the trace function for `header`, if registered.
    ///
    /// # Safety
    ///
    /// `header` must point to a valid `GcHeader` whose type tag
    /// matches a registered entry; the same contract as
    /// [`Traceable::trace_slots`].
    #[inline]
    pub unsafe fn trace(&self, header: *mut GcHeader, visitor: &mut SlotVisitor<'_>) {
        // SAFETY: precondition delegated to the caller.
        unsafe {
            let tag = (*header).type_tag();
            if let Some(f) = self.table[tag as usize] {
                f(header, visitor);
            }
        }
    }

    /// Invoke the ephemeron trace function for `header`, if registered.
    ///
    /// # Safety
    ///
    /// Same payload-validity contract as [`Self::trace`].
    #[inline]
    pub unsafe fn trace_ephemerons(
        &self,
        header: *mut GcHeader,
        visitor: &mut EphemeronVisitor<'_>,
    ) {
        // SAFETY: precondition delegated to the caller.
        unsafe {
            let tag = (*header).type_tag();
            if let Some(f) = self.ephemeron_table[tag as usize] {
                f(header, visitor);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compressed::Gc;

    struct Leaf;

    impl Traceable for Leaf {
        const TYPE_TAG: u8 = 0xA0;
        unsafe fn trace_slots(_this: *mut Self, _v: &mut SlotVisitor<'_>) {}
    }

    struct Node {
        next: Gc<Node>,
    }

    impl Traceable for Node {
        const TYPE_TAG: u8 = 0xA1;
        unsafe fn trace_slots(this: *mut Self, v: &mut SlotVisitor<'_>) {
            unsafe {
                let slot = core::ptr::addr_of_mut!((*this).next) as *mut RawGc;
                v(slot);
            }
        }
    }

    #[test]
    fn register_and_lookup() {
        let mut t = TraceTable::new();
        t.register::<Leaf>();
        t.register::<Node>();
        assert!(t.get(Leaf::TYPE_TAG).is_some());
        assert!(t.get(Node::TYPE_TAG).is_some());
        assert!(t.get(0).is_none());
    }
}
