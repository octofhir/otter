//! Heap-boxed numeric `Value` for the 32-bit object slab.
//!
//! # Contents
//!
//! - [`HEAP_NUMBER_TYPE_TAG`] — the GC type tag.
//! - [`HeapNumberBody`] — a single boxed numeric `Value` (an offset double
//!   or an out-of-range int32) stored as raw bits.
//! - [`alloc_heap_number`] / [`read_heap_number`] — allocate and read.
//!
//! # Invariants
//!
//! - The boxed bits always decode to a [`crate::Value`] that is a number
//!   (double or int32) and never a heap cell. A cell fits the 32-bit slot
//!   inline as its compressed offset, so it is never boxed; the body
//!   therefore holds no GC edge and traces to nothing.
//! - The bits round-trip a `Value` exactly, so an out-of-range int32 keeps
//!   its int32 representation instead of collapsing to a double.
//!
//! # See also
//!
//! - [`crate::value::compressed`] — the 32-bit slot codec that boxes here.

use otter_macros::Pelt;

/// GC type tag for [`HeapNumberBody`].
pub const HEAP_NUMBER_TYPE_TAG: u8 = 0x30;

/// GC-allocated box holding one numeric [`crate::Value`] as raw bits.
///
/// Referenced from a 32-bit object slot by its compressed offset when a
/// property value is a double or an out-of-range int32 that does not fit
/// the slot inline.
#[derive(Pelt)]
#[pelt(tag = HEAP_NUMBER_TYPE_TAG)]
pub struct HeapNumberBody {
    /// Raw [`crate::Value`] bits of the boxed number. Never a cell, so the
    /// field carries no GC edge.
    pub bits: u64,
}

/// Compressed handle to a [`HeapNumberBody`].
pub type HeapNumber = otter_gc::Gc<HeapNumberBody>;

/// Allocate a fresh [`HeapNumber`] boxing `bits`.
///
/// # Errors
///
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
pub fn alloc_heap_number(
    heap: &mut otter_gc::GcHeap,
    bits: u64,
) -> Result<HeapNumber, otter_gc::OutOfMemory> {
    heap.alloc(HeapNumberBody { bits })
}

/// Read the boxed `Value` bits of `boxed`.
#[must_use]
pub fn read_heap_number(heap: &otter_gc::GcHeap, boxed: HeapNumber) -> u64 {
    heap.read_payload(boxed, |body| body.bits)
}
