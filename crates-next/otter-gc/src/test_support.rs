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
//!
//! # Invariants
//!
//! - [`OPAQUE_LEAF_TAG`] is reserved for this leaf type.
//!   Production GC types must pick a different tag.
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
