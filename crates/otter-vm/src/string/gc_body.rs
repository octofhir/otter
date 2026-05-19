//! GC-managed JavaScript string payloads for interned engine metadata.
//!
//! This module is the first step toward moving string identity used by VM
//! metadata onto the collector heap. It intentionally starts with flat
//! WTF-16 atoms because shape keys need stable, traceable identity without
//! `Rc`, `Arc`, `Box`, or host-owned buffers inside the shape node.
//!
//! # Contents
//! - [`JsStringId`] — stable intern-table identity for shape keys.
//! - [`JsStringBody`] — traceable flat string header.
//! - [`JsStringChunkBody`] — fixed-size WTF-16 storage chunk.
//! - [`alloc_flat_string_body_with_roots`] — allocation helper that preserves
//!   caller roots across allocation-triggered GC.
//!
//! # Invariants
//! - String bytes/code units are stored on the GC heap, not in Rust-owned
//!   `Arc`/`Box` containers embedded in shape metadata.
//! - The linked chunk chain is immutable after allocation.
//! - `Gc::null()` in [`JsStringBody::first_chunk`] represents the empty string.
//! - Allocation exposes every pending `Gc` through `alloc_with_roots`; callers
//!   must pass any live VM stack/register roots in `external_visit`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-ecmascript-language-types-string-type>
//! - <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots>

use otter_gc::GcHeap;
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::{RawGc, SlotVisitor};

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`JsStringBody`].
pub const JS_STRING_BODY_TYPE_TAG: u8 = 0x20;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`JsStringChunkBody`].
pub const JS_STRING_CHUNK_BODY_TYPE_TAG: u8 = 0x21;

/// Number of UTF-16 code units stored in one GC string chunk.
///
/// Keeping the chunk fixed-size lets the current sized-payload GC represent
/// variable-length strings without embedding Rust-owned buffers in traced
/// objects. The value keeps one chunk comfortably cache-line sized together
/// with the compressed `next` handle and length byte.
pub const JS_STRING_CHUNK_UNITS: usize = 24;

/// GC handle to a flat JavaScript string payload.
pub type JsStringHandle = otter_gc::Gc<JsStringBody>;

/// Stable identity assigned by the VM-side string interner.
///
/// The id is stored in the GC body so shape side tables can key transitions and
/// flattened offset caches without depending on a moving `Gc` cage offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JsStringId(u32);

impl JsStringId {
    #[must_use]
    /// Construct an interner-local string id.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    #[must_use]
    /// Raw numeric representation for diagnostics and compact side tables.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Fixed-size WTF-16 storage node for a GC-managed flat string.
#[derive(Debug, Clone)]
pub struct JsStringChunkBody {
    /// Next chunk in logical string order, or `Gc::null()` for the tail.
    next: otter_gc::Gc<JsStringChunkBody>,
    /// Number of initialized entries in [`Self::units`].
    len: u8,
    /// UTF-16 code units. Entries at `len..` are zero padding.
    units: [u16; JS_STRING_CHUNK_UNITS],
}

impl JsStringChunkBody {
    #[must_use]
    fn new(next: otter_gc::Gc<Self>, input: &[u16]) -> Self {
        debug_assert!(input.len() <= JS_STRING_CHUNK_UNITS);
        let mut units = [0; JS_STRING_CHUNK_UNITS];
        units[..input.len()].copy_from_slice(input);
        Self {
            next,
            len: input.len() as u8,
            units,
        }
    }

    #[must_use]
    /// Next chunk in logical order.
    pub const fn next(&self) -> otter_gc::Gc<Self> {
        self.next
    }

    #[must_use]
    /// Initialized UTF-16 code units in this chunk.
    pub fn units(&self) -> &[u16] {
        &self.units[..usize::from(self.len)]
    }
}

impl otter_gc::SafeTraceable for JsStringChunkBody {
    const TYPE_TAG: u8 = JS_STRING_CHUNK_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        if !self.next.is_null() {
            let p = &self.next as *const otter_gc::Gc<JsStringChunkBody> as *mut RawGc;
            visitor(p);
        }
    }
}

/// GC-managed flat string header.
#[derive(Debug, Clone)]
pub struct JsStringBody {
    /// Stable interner identity.
    id: JsStringId,
    /// Code-unit length.
    len: u32,
    /// Stable FNV-1a hash over UTF-16 code units for atom tables.
    hash: u64,
    /// First chunk in logical string order, or `Gc::null()` for empty strings.
    first_chunk: otter_gc::Gc<JsStringChunkBody>,
}

impl JsStringBody {
    #[must_use]
    /// Stable interner identity.
    pub const fn id(&self) -> JsStringId {
        self.id
    }

    #[must_use]
    /// String length in UTF-16 code units.
    pub const fn len(&self) -> u32 {
        self.len
    }

    #[must_use]
    /// `true` when the string has zero UTF-16 code units.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    /// Stable FNV-1a hash over the string's UTF-16 code units.
    pub const fn hash(&self) -> u64 {
        self.hash
    }

    #[must_use]
    /// First chunk in logical order, or `Gc::null()` for empty strings.
    pub const fn first_chunk(&self) -> otter_gc::Gc<JsStringChunkBody> {
        self.first_chunk
    }
}

impl otter_gc::SafeTraceable for JsStringBody {
    const TYPE_TAG: u8 = JS_STRING_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        if !self.first_chunk.is_null() {
            let p = &self.first_chunk as *const otter_gc::Gc<JsStringChunkBody> as *mut RawGc;
            visitor(p);
        }
    }
}

/// Allocate a GC-managed flat string body from UTF-16 code units.
///
/// The helper builds chunks from tail to head so the pending allocation payload
/// always owns the previously-built chain during any allocation-triggered GC.
pub fn alloc_flat_string_body_with_roots(
    heap: &mut GcHeap,
    id: JsStringId,
    units: &[u16],
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsStringHandle, otter_gc::OutOfMemory> {
    let mut head = otter_gc::Gc::<JsStringChunkBody>::null();
    for chunk in units.rchunks(JS_STRING_CHUNK_UNITS) {
        let value = JsStringChunkBody::new(head, chunk);
        head = heap.alloc_with_roots(value, external_visit)?;
    }

    heap.alloc_with_roots(
        JsStringBody {
            id,
            len: units.len() as u32,
            hash: hash_utf16(units),
            first_chunk: head,
        },
        external_visit,
    )
}

/// Collect a GC-managed flat string back into code units.
///
/// This is intentionally a helper for cold paths, tests, and side-cache
/// validation. Hot shape lookup should compare atom handles/ids instead of
/// reconstructing strings.
#[must_use]
pub fn to_utf16_vec(heap: &GcHeap, string: JsStringHandle) -> Vec<u16> {
    let (len, mut chunk) = heap.read_payload(string, |body| (body.len(), body.first_chunk()));
    let mut out = Vec::with_capacity(len as usize);
    while !chunk.is_null() {
        let next = heap.read_payload(chunk, |body| {
            out.extend_from_slice(body.units());
            body.next()
        });
        chunk = next;
    }
    out
}

#[must_use]
fn hash_utf16(units: &[u16]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET;
    for unit in units {
        for byte in unit.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_empty_gc_string() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let string =
            alloc_flat_string_body_with_roots(&mut heap, JsStringId::new(1), &[], &mut roots)
                .expect("string");

        heap.read_payload(string, |body| {
            assert_eq!(body.id().get(), 1);
            assert_eq!(body.len(), 0);
            assert!(body.is_empty());
            assert!(body.first_chunk().is_null());
        });
        assert!(to_utf16_vec(&heap, string).is_empty());
    }

    #[test]
    fn allocates_multichunk_gc_string_in_order() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let units: Vec<u16> = (0..(JS_STRING_CHUNK_UNITS * 3 + 5))
            .map(|i| (i % 0xd7ff) as u16)
            .collect();
        let string =
            alloc_flat_string_body_with_roots(&mut heap, JsStringId::new(7), &units, &mut roots)
                .expect("string");

        heap.read_payload(string, |body| {
            assert_eq!(body.id().get(), 7);
            assert_eq!(body.len(), units.len() as u32);
            assert_eq!(body.hash(), hash_utf16(&units));
            assert!(!body.first_chunk().is_null());
        });
        assert_eq!(to_utf16_vec(&heap, string), units);
    }
}
