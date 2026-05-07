//! Type-tag → trace function dispatch table.
//!
//! Tracing is the hot path of every GC cycle. Phase-1 dispatch is a
//! single indexed load + indirect call: the [`TraceTable`] is a
//! `[Option<TraceFn>; 256]` keyed by [`crate::header::GcHeader::type_tag`]
//! — no `Box<dyn>`, no `dyn Any`, no downcast.
//!
//! # Contents
//!
//! - [`TraceFn`] — the function-pointer signature stored in the
//!   table.
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
//! - A `TraceFn` may not allocate, may not run user JS, and may
//!   not enter the same heap recursively. It must visit every
//!   slot that holds a [`crate::compressed::RawGc`] / `Gc<T>`.
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
    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>);
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
}

/// A 256-entry array of [`TraceFn`] pointers, indexed by
/// [`GcHeader::type_tag`]. Empty slots stay `None`.
pub struct TraceTable {
    table: [Option<TraceFn>; 256],
    /// Drop functions used by the sweeper to invoke `T`'s `Drop`
    /// on dead objects (so e.g. boxed strings get their backing
    /// freed). `None` for plain-old-data types.
    drop_table: [Option<unsafe fn(*mut GcHeader)>; 256],
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
            drop_table: [None; 256],
        }
    }

    /// Register a [`Traceable`] implementation. The wrapper
    /// `trace_wrapper` casts the raw header pointer to `*mut T`
    /// (skipping the header) and forwards to `T::trace_slots`.
    pub fn register<T: Traceable>(&mut self) {
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
        let tag = T::TYPE_TAG as usize;
        if let Some(existing) = self.table[tag] {
            debug_assert!(
                existing as *const () == trace_wrapper::<T> as *const (),
                "trace tag {tag} already registered with a different fn",
            );
        }
        self.table[tag] = Some(trace_wrapper::<T>);
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

    /// Look up the drop function for a given type tag.
    #[inline]
    pub fn get_drop(&self, tag: u8) -> Option<unsafe fn(*mut GcHeader)> {
        self.drop_table[tag as usize]
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
