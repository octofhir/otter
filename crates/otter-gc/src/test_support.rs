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
//! - [`OpaqueVector`] — a body whose references live in trailing
//!   storage allocated in the same cell; type tag
//!   [`OPAQUE_VECTOR_TAG`].
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

/// Reserved `type_tag` for [`OpaqueVector`].
pub const OPAQUE_VECTOR_TAG: u8 = 0xFC;

/// Body with a variable-length reference array stored in the same cell.
///
/// The elements follow the header field in memory rather than living in
/// a `Vec`, which is what makes the object self-contained: the whole
/// thing is one GC cell, so a page image carries it and a relocation
/// pass can find every slot.
#[derive(Debug)]
pub struct OpaqueVector {
    /// Elements in the trailing array.
    len: u32,
}

impl OpaqueVector {
    /// Trailing bytes needed for `len` elements.
    #[must_use]
    pub fn trailing_bytes(len: usize) -> usize {
        len * std::mem::size_of::<crate::compressed::RawGc>()
    }

    /// A header for `len` elements. Pass [`Self::trailing_bytes`] as the
    /// extra size to [`crate::heap::GcHeap::alloc_variable_with_roots`].
    #[must_use]
    pub fn new(len: usize) -> Self {
        Self { len: len as u32 }
    }

    /// Elements in the trailing array.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether the array is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn elements(&self) -> *mut crate::compressed::RawGc {
        // SAFETY: the allocation reserved `trailing_bytes(len)` after the
        // header field, so the array starts one `Self` past `self`.
        unsafe {
            (self as *const Self as *mut u8)
                .add(std::mem::size_of::<Self>())
                .cast()
        }
    }

    /// Read element `index`.
    ///
    /// # Panics
    /// If `index` is out of range.
    #[must_use]
    pub fn get(&self, index: usize) -> crate::compressed::RawGc {
        assert!(index < self.len(), "index out of range");
        // SAFETY: bounds checked above; the array is live for `len`.
        unsafe { *self.elements().add(index) }
    }

    /// Write element `index`.
    ///
    /// # Panics
    /// If `index` is out of range.
    pub fn set(&mut self, index: usize, value: crate::compressed::RawGc) {
        assert!(index < self.len(), "index out of range");
        // SAFETY: bounds checked above; the array is live for `len`.
        unsafe { *self.elements().add(index) = value };
    }
}

impl Traceable for OpaqueVector {
    const TYPE_TAG: u8 = OPAQUE_VECTOR_TAG;

    /// The trailing array lives in the heap cell, not in this body, so a
    /// pending copy on the stack has nothing to trace.
    unsafe fn trace_pending_slots(_this: *mut Self, _v: &mut SlotVisitor<'_>) {}

    unsafe fn trace_slots(this: *mut Self, v: &mut SlotVisitor<'_>) {
        // SAFETY: `this` precedes a valid header plus its trailing array.
        unsafe {
            let len = (*this).len();
            let base = (*this).elements();
            for index in 0..len {
                v(base.add(index));
            }
        }
    }
}
