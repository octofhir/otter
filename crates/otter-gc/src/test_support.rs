//! Public test-support types for downstream crates.
//!
//! Downstream crates (`otter-runtime`, integration tests) keep
//! the workspace-wide `forbid(unsafe_code)` lint and therefore
//! cannot implement [`crate::trace::Traceable`] themselves —
//! every `Traceable` impl needs an `unsafe fn trace_slots`.
//! The helpers in this module sidestep that by providing one
//! ready-made leaf type so tests of GC-integration code (heap
//! stats, snapshots, cap behaviour) can allocate and observe
//! the heap without redeclaring an unsafe trait.
//!
//! # Contents
//!
//! - [`OpaqueLeaf`] — payload-only Traceable with no outgoing
//!   references; type tag [`OPAQUE_LEAF_TAG`].
//! - [`OpaquePair`] — two outgoing references, so pointer-rewriting
//!   paths have something to rewrite; type tag [`OPAQUE_PAIR_TAG`].
//!
//! # Invariants
//!
//! - [`OPAQUE_LEAF_TAG`] and [`OPAQUE_PAIR_TAG`] are reserved for these
//!   types. Production GC types must pick different tags.
//!
//! # See also
//!
//! - GC architecture plan §6.1 (unsafe boundary kept inside
//!   `otter-gc`).
//! - Task 74 — GC stats, heap snapshot, retained-size walker.

use crate::trace::{SlotVisitor, Traceable};

/// Reserved `type_tag` for [`OpaqueLeaf`]. Production types
/// must pick a tag distinct from this one.
pub const OPAQUE_LEAF_TAG: u8 = 0xFE;

/// Leaf GC object with no outgoing references. The payload is
/// a single `u64` so the allocation footprint is the GC header
/// (8 B) plus 8 B of payload.
///
/// Intended for downstream tests that need to allocate against
/// a `GcHeap` without re-implementing [`Traceable`] (which
/// would require lifting their own `forbid(unsafe_code)`).
#[derive(Debug, Clone, Copy)]
pub struct OpaqueLeaf {
    /// Caller-chosen payload — useful as a sentinel value when
    /// asserting which allocations were observed.
    pub payload: u64,
}

impl Traceable for OpaqueLeaf {
    const TYPE_TAG: u8 = OPAQUE_LEAF_TAG;
    unsafe fn trace_slots(_this: *mut Self, _v: &mut SlotVisitor<'_>) {}
}

/// Reserved `type_tag` for [`OpaquePair`].
pub const OPAQUE_PAIR_TAG: u8 = 0xFD;

/// Two-slot GC object, the smallest body that exercises pointer
/// rewriting: relocation, evacuation, and image restore all have to
/// find both slots through the trace table.
#[derive(Debug, Clone, Copy)]
pub struct OpaquePair {
    /// First outgoing reference.
    pub first: crate::compressed::RawGc,
    /// Second outgoing reference.
    pub second: crate::compressed::RawGc,
}

impl Traceable for OpaquePair {
    const TYPE_TAG: u8 = OPAQUE_PAIR_TAG;
    unsafe fn trace_slots(this: *mut Self, v: &mut SlotVisitor<'_>) {
        // SAFETY: `this` precedes a valid `OpaquePair` payload, so both
        // slot addresses are inside the same allocation.
        unsafe {
            v(std::ptr::addr_of_mut!((*this).first));
            v(std::ptr::addr_of_mut!((*this).second));
        }
    }
}
