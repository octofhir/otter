//! GC-managed JavaScript string body: one compact fixed part and a trailing
//! payload sized for its representation — the code units of a sequential
//! string, or the two words of a rope or a slice.
//!
//! Every string lives in exactly one [`JsStringBody`] cell on the GC heap.
//! Sequential strings keep their Latin-1 bytes or WTF-16 units in the cell's
//! trailing storage, exactly as many as the string has. Cons and sliced
//! strings keep their child handles in two trailing words, so the collector
//! traces them transitively.
//!
//! # Contents
//! - [`JsStringBody`] — the 20-byte fixed part: map word, recorded atom,
//!   length, hash, representation kind and rope depth.
//! - [`JsStringBodyRepr`] — the by-value view of a body's representation.
//! - Stable representation tags and payload offsets consumed by generated
//!   code for contiguous string operations.
//! - `alloc_*` helpers and heap-level
//!   [`concat`] / [`slice`] / [`flatten`] / [`equals`] / [`to_utf16_vec`].
//! - [`splice_string_body`] — one sequential string of runs taken from
//!   contiguous sources: one allocation, one copy per run.
//!
//! # Invariants
//! - String bytes/code units live in the cell's trailing storage. No `Rc` /
//!   `Arc` / `Box` / `Cell` / `RefCell` inside the body, and nothing outside
//!   the heap.
//! - A body's trailing size is fixed by its kind at allocation: `len` bytes
//!   (Latin-1), `2 * len` bytes (WTF-16) or two words (cons, sliced). Only a
//!   cons is rewritten in place, into a sliced view of its flattened copy,
//!   which has the same trailing size.
//! - Ordinary bodies start in the moving young generation. Every handle
//!   held across an allocation is exposed through a traced slot or scoped VM
//!   handle; a rope's or slice's children are rooted by its allocator until
//!   the initializer writes them at the cell's final address.
//! - `len` is precomputed at construction and is O(1) heap-free at
//!   the body level (callers read it via `heap.read_payload`).
//! - A body length is always exact in `u32`. Concatenation rejects a sum beyond
//!   that representation before flattening or allocation; it never saturates
//!   or truncates a rope's logical length.
//! - `hash` is zero until first requested; then it is the exact FNV-1a hash
//!   of the code units for every representation, so equal content always has
//!   equal hashes. Allocation, slicing, concatenation and flattening never
//!   hash; rope equality and ordering stream the immutable code units.
//! - A short slice is copied into a sequential body: no parent retention.
//! - A splice is Latin-1 exactly when every unit of every run fits a byte.
//! - Cons depth never exceeds [`MAX_ROPE_DEPTH`]; concatenations
//!   that would exceed it flatten the deeper child eagerly.
//! - Slicing a flat body is O(1) for either storage width. Slicing a `Cons`
//!   materialises only the requested span; slicing a `Sliced` collapses into a
//!   single view (no `Sliced(Sliced(...))`).
//! - The fixed part's field offsets and the representation tags are an
//!   explicit generated-code contract ([`jit_layout`]). Changing either
//!   requires updating the baked JIT layout and its machine-code consumers
//!   together.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-ecmascript-language-types-string-type>

use otter_gc::GcHeap;
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::{RawGc, SlotVisitor};

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`JsStringBody`].
pub const JS_STRING_BODY_TYPE_TAG: u8 = 0x20;

/// Maximum depth of an unflattened cons rope. Concatenations that
/// would exceed this trigger an eager flatten before the new `Cons`
/// node is built.
pub const MAX_ROPE_DEPTH: u8 = 254;
/// Longest concatenation of two flat Latin-1 strings that copies into a
/// sequential result instead of building a rope.
pub const SHORT_CONCAT_LATIN1: usize = 24;
/// Longest concatenation of two flat strings, one of them wide, that copies
/// into a sequential result.
pub const SHORT_CONCAT_FLAT: usize = 12;

/// Sequential UTF-16 representation tag exposed to generated code.
pub const STRING_REPR_SEQ_FLAT: u8 = 1;
/// Sequential Latin-1 representation tag exposed to generated code.
pub const STRING_REPR_SEQ_LATIN1: u8 = 3;
/// Rope representation tag; generated code must use the cold path.
pub const STRING_REPR_CONS: u8 = 4;
/// Slice representation tag; generated code must use the cold path.
pub const STRING_REPR_SLICED: u8 = 5;

/// Trailing bytes of a cons or sliced body: its two words.
const PAIR_BYTES: usize = 8;

/// GC handle to a JavaScript string body. `Copy`. Packs into
/// [`crate::Value`] under `TAG_PTR_STRING`.
pub type JsStringHandle = otter_gc::Gc<JsStringBody>;

/// Failure modes specific to joining two already-coerced strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum StringConcatError {
    /// The exact UTF-16 length cannot be represented by [`JsStringBody::len`].
    #[error("string length overflow: {left_len} + {right_len} UTF-16 code units exceeds u32::MAX")]
    StringTooLong {
        /// Left operand's exact UTF-16 length.
        left_len: u32,
        /// Right operand's exact UTF-16 length.
        right_len: u32,
    },
    /// The collector refused the result body allocation.
    #[error(transparent)]
    OutOfMemory(#[from] otter_gc::OutOfMemory),
}

/// The representation of a [`JsStringBody`], read by value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsStringBodyRepr {
    /// Flat WTF-16 code units in the body's trailing storage; the live length
    /// is [`JsStringBody::len`].
    SeqFlat,
    /// Latin-1 code units in the body's trailing storage. Each byte
    /// zero-extends to a `u16` on read.
    SeqLatin1,
    /// Rope concatenation node. Tracing visits both children.
    Cons {
        /// Left child.
        left: JsStringHandle,
        /// Right child.
        right: JsStringHandle,
        /// Maximum depth of either child plus one. Bounded by
        /// [`MAX_ROPE_DEPTH`].
        depth: u8,
    },
    /// Slice view over a parent string. Tracing visits the parent.
    Sliced {
        /// Parent body.
        parent: JsStringHandle,
        /// Start offset (code units) into the parent.
        start: u32,
    },
}

/// GC-managed JavaScript string body: the fixed part, followed in its cell
/// by the trailing payload its kind sizes.
///
/// `repr(C)` because the trailing payload starts one body past the body, in
/// the same GC cell, and that only has a defined address with a fixed layout.
#[derive(Debug)]
#[repr(C)]
pub struct JsStringBody {
    /// Map word: this value cell's type, at body offset 0.
    pub(crate) map: otter_gc::cell_map::TypeMap<JS_STRING_BODY_TYPE_TAG>,
    /// The interned atom of this string's content plus one, recorded the
    /// first time a property key or shape transition interned it; zero until
    /// then. Content never changes, so a recorded atom stays exact.
    atom: u32,
    /// Code-unit length (UTF-16). O(1) heap-free at the body level.
    pub len: u32,
    /// FNV-1a hash of the code units, folded to 32 bits, once
    /// [`string_hash`] computed it; zero until then.
    pub hash: u32,
    /// One of the `STRING_REPR_*` tags.
    kind: u8,
    /// A cons's rope depth; zero for every other kind.
    depth: u8,
    reserved: [u8; 2],
}

const _: () = assert!(std::mem::offset_of!(JsStringBody, map) == 0);
const _: () = assert!(std::mem::size_of::<JsStringBody>() == 20);
const _: () = assert!(std::mem::align_of::<JsStringBody>() == 4);

/// Measure native read/allocation geometry from the sole current VM body.
/// No engine object allocation or GC occurs while constructing this scalar plan.
pub(crate) fn jit_layout() -> crate::jit::JitStringLayout {
    let header = otter_gc::header::HEADER_SIZE as u32;
    let payload = header + std::mem::size_of::<JsStringBody>() as u32;
    let pair_cell_bytes =
        otter_gc::page::align_up(payload as usize + PAIR_BYTES, otter_gc::page::CELL_SIZE) as u32;
    crate::jit::JitStringLayout {
        string_type_tag: JS_STRING_BODY_TYPE_TAG,
        string_len_byte: header + std::mem::offset_of!(JsStringBody, len) as u32,
        string_repr_byte: header + std::mem::offset_of!(JsStringBody, kind) as u32,
        string_body_size: payload,
        pair_cell_bytes,
        young_header_word: u64::from(JS_STRING_BODY_TYPE_TAG)
            | (u64::from(crate::jit::JIT_GC_YOUNG_FLAG)
                << (8 * otter_gc::header::HEADER_FLAGS_BYTE_OFFSET)),
        cell_size_shift: (8 * otter_gc::header::HEADER_SIZE_BYTES_OFFSET) as u8,
        map_atom_word: u64::from(
            otter_gc::cell_map::CellMap::of_type(JS_STRING_BODY_TYPE_TAG).word(),
        ),
        atom_byte: header + std::mem::offset_of!(JsStringBody, atom) as u32,
        hash_byte: header + std::mem::offset_of!(JsStringBody, hash) as u32,
        cons_left_byte: payload,
        cons_right_byte: payload + 4,
        cons_depth_byte: header + std::mem::offset_of!(JsStringBody, depth) as u32,
        sliced_parent_byte: payload,
        sliced_start_byte: payload + 4,
        short_concat_flat: SHORT_CONCAT_FLAT as u8,
        short_concat_latin1: SHORT_CONCAT_LATIN1 as u8,
        max_rope_depth: MAX_ROPE_DEPTH,
        seq_flat_tag: STRING_REPR_SEQ_FLAT,
        seq_latin1_tag: STRING_REPR_SEQ_LATIN1,
        cons_tag: STRING_REPR_CONS,
        sliced_tag: STRING_REPR_SLICED,
    }
}

impl JsStringBody {
    /// The fixed part of a sequential body of `len` code units.
    const fn seq(latin1: bool, len: u32) -> Self {
        Self {
            map: otter_gc::cell_map::TypeMap::NEW,
            atom: 0,
            len,
            hash: 0,
            kind: if latin1 {
                STRING_REPR_SEQ_LATIN1
            } else {
                STRING_REPR_SEQ_FLAT
            },
            depth: 0,
            reserved: [0; 2],
        }
    }

    /// The fixed part of a cons or sliced body; its initializer writes the
    /// pair ([`Self::init_pair`]).
    const fn pair(kind: u8, len: u32, depth: u8) -> Self {
        Self {
            map: otter_gc::cell_map::TypeMap::NEW,
            atom: 0,
            len,
            hash: 0,
            kind,
            depth,
            reserved: [0; 2],
        }
    }

    /// The representation, read by value.
    #[must_use]
    pub fn repr(&self) -> JsStringBodyRepr {
        match self.kind {
            STRING_REPR_SEQ_FLAT => JsStringBodyRepr::SeqFlat,
            STRING_REPR_SEQ_LATIN1 => JsStringBodyRepr::SeqLatin1,
            STRING_REPR_CONS => {
                let (left, right) = self.pair_words();
                JsStringBodyRepr::Cons {
                    left,
                    right,
                    depth: self.depth,
                }
            }
            STRING_REPR_SLICED => {
                let (parent, _) = self.pair_words();
                // SAFETY: a sliced body's second word is its start.
                let start = unsafe { self.trailing_ptr().cast::<u32>().add(1).read() };
                JsStringBodyRepr::Sliced { parent, start }
            }
            kind => unreachable!("string body kind {kind}"),
        }
    }

    /// Whether the body is a cons rope.
    #[must_use]
    pub fn is_cons(&self) -> bool {
        self.kind == STRING_REPR_CONS
    }

    /// The two handle words of a cons or sliced body.
    fn pair_words(&self) -> (JsStringHandle, JsStringHandle) {
        debug_assert!(matches!(self.kind, STRING_REPR_CONS | STRING_REPR_SLICED));
        // SAFETY: a cons or sliced body reserved two words of trailing
        // storage, and handles are four-byte compressed offsets.
        unsafe {
            let words = self.trailing_ptr().cast::<JsStringHandle>();
            (words.read(), words.add(1).read())
        }
    }

    /// The trailing pair's slot addresses, for tracing.
    fn pair_slots(&mut self) -> *mut RawGc {
        self.trailing_ptr().cast_mut().cast::<RawGc>()
    }

    /// Write a fresh cons's children or a fresh slice's parent and start.
    fn init_pair(&mut self, first: JsStringHandle, second: u32) {
        // SAFETY: the allocation reserved two trailing words.
        unsafe {
            let words = self.trailing_ptr().cast_mut().cast::<u32>();
            words.cast::<JsStringHandle>().write(first);
            words.add(1).write(second);
        }
    }

    /// Rewrite a cons in place into a view over the whole of `flat`, its
    /// flattened copy: the same length, hash and identity, the same trailing
    /// size. The kind is written last.
    fn become_slice_of(&mut self, flat: JsStringHandle) {
        debug_assert_eq!(self.kind, STRING_REPR_CONS);
        self.init_pair(flat, 0);
        self.depth = 0;
        self.kind = STRING_REPR_SLICED;
    }

    /// The interned atom of this string's content, when one was recorded.
    #[must_use]
    pub(crate) fn cached_atom(&self) -> Option<crate::property_atom::AtomId> {
        self.atom
            .checked_sub(1)
            .map(crate::property_atom::AtomId::from_global)
    }

    /// Record the interned atom of this string's content.
    pub(crate) fn set_cached_atom(&mut self, atom: crate::property_atom::AtomId) {
        self.atom = atom.raw().wrapping_add(1);
    }

    /// String length in UTF-16 code units.
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.len
    }

    /// `true` when the string has zero UTF-16 code units.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Trailing bytes a sequential body of `len` code units needs.
    #[must_use]
    pub fn trailing_bytes(latin1: bool, len: usize) -> usize {
        if latin1 { len } else { len * 2 }
    }

    /// Base of the trailing payload.
    fn trailing_ptr(&self) -> *const u8 {
        // SAFETY: every body was allocated with its kind's trailing storage
        // reserved immediately after it, so the payload starts one `Self`
        // past `self`.
        unsafe { (self as *const Self as *const u8).add(std::mem::size_of::<Self>()) }
    }

    /// The WTF-16 units of a [`JsStringBodyRepr::SeqFlat`] body.
    #[must_use]
    pub fn seq_flat_units(&self) -> &[u16] {
        debug_assert_eq!(self.kind, STRING_REPR_SEQ_FLAT);
        // SAFETY: the trailing array holds exactly `len` `u16`s, written
        // at allocation and immutable thereafter; the payload is two-byte
        // aligned (the fixed part is 20 bytes past an eight-aligned cell).
        unsafe { std::slice::from_raw_parts(self.trailing_ptr().cast::<u16>(), self.len as usize) }
    }

    /// The Latin-1 bytes of a [`JsStringBodyRepr::SeqLatin1`] body.
    #[must_use]
    pub fn seq_latin1_bytes(&self) -> &[u8] {
        debug_assert_eq!(self.kind, STRING_REPR_SEQ_LATIN1);
        // SAFETY: the trailing array holds exactly `len` bytes.
        unsafe { std::slice::from_raw_parts(self.trailing_ptr(), self.len as usize) }
    }

    /// Write the trailing WTF-16 units of a freshly allocated body.
    fn init_seq_flat(&mut self, units: &[u16]) {
        debug_assert_eq!(units.len(), self.len as usize);
        // SAFETY: the allocation reserved `2 * len` trailing bytes, and
        // the source slice cannot alias a cell this heap just carved.
        unsafe {
            std::ptr::copy_nonoverlapping(
                units.as_ptr(),
                self.trailing_ptr().cast::<u16>().cast_mut(),
                units.len(),
            );
        }
    }

    /// Copy `len` units from `data` (Latin-1 bytes, or WTF-16 units when
    /// `wide`) into this sequential body's units starting at `at`, narrowing
    /// into a Latin-1 body.
    ///
    /// # Safety
    /// `data` holds `len` readable units of its width outside this cell,
    /// `at + len` is within this body's length, and a Latin-1 body receives
    /// only units that fit a byte.
    unsafe fn write_units_at(&mut self, at: usize, data: *const u8, wide: bool, len: usize) {
        debug_assert!(at + len <= self.len as usize);
        let base = self.trailing_ptr().cast_mut();
        // SAFETY: per the caller's contract.
        unsafe {
            match (self.kind == STRING_REPR_SEQ_LATIN1, wide) {
                (true, false) => std::ptr::copy_nonoverlapping(data, base.add(at), len),
                (true, true) => {
                    let units = std::slice::from_raw_parts(data.cast::<u16>(), len);
                    let out = std::slice::from_raw_parts_mut(base.add(at), len);
                    for (byte, &unit) in out.iter_mut().zip(units) {
                        *byte = unit as u8;
                    }
                }
                (false, false) => {
                    let bytes = std::slice::from_raw_parts(data, len);
                    let out = std::slice::from_raw_parts_mut(base.cast::<u16>().add(at), len);
                    for (unit, &byte) in out.iter_mut().zip(bytes) {
                        *unit = u16::from(byte);
                    }
                }
                (false, true) => std::ptr::copy_nonoverlapping(
                    data.cast::<u16>(),
                    base.cast::<u16>().add(at),
                    len,
                ),
            }
        }
    }

    /// Write the trailing Latin-1 bytes of a freshly allocated body.
    fn init_seq_latin1(&mut self, bytes: &[u8]) {
        debug_assert_eq!(bytes.len(), self.len as usize);
        // SAFETY: the allocation reserved `len` trailing bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.trailing_ptr().cast_mut(),
                bytes.len(),
            );
        }
    }

    /// Rope depth: `0` for flat / latin1 / sliced, `1..=MAX_ROPE_DEPTH`
    /// for cons.
    #[must_use]
    pub fn depth(&self) -> u8 {
        self.depth
    }
}

impl otter_gc::SafeTraceable for JsStringBody {
    const MAP_WORD: otter_gc::cell_map::MapWord =
        otter_gc::cell_map::MapWord::of_type(JS_STRING_BODY_TYPE_TAG);
    const TYPE_TAG: u8 = JS_STRING_BODY_TYPE_TAG;

    fn trace_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>) {
        match self.kind {
            STRING_REPR_CONS => {
                let slots = self.pair_slots();
                visitor(slots);
                // SAFETY: a cons reserved two trailing handle words.
                visitor(unsafe { slots.add(1) });
            }
            STRING_REPR_SLICED => visitor(self.pair_slots()),
            _ => {}
        }
    }

    /// A pending body is the fixed part alone: a rope's children are written
    /// by its initializer, rooted by its allocator until then.
    fn trace_pending_slots_safe(&mut self, _visitor: &mut SlotVisitor<'_>) {}

    /// Visited concurrently under the layout lock: flattening rewrites a
    /// rope's pointer words in place through
    /// [`GcHeap::with_payload_relayout`], which holds the same lock.
    const CONCURRENT_MARKING: bool = true;
    const LAYOUT_LOCKED: bool = true;
}

/// Allocate a flat WTF-16 string body.
///
/// # Errors
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
pub fn alloc_flat_string_body_with_roots(
    heap: &mut GcHeap,
    units: &[u16],
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsStringHandle, otter_gc::OutOfMemory> {
    alloc_flat_string_body_at(
        heap,
        units,
        otter_gc::SitePlacement::Young(0),
        external_visit,
    )
}

/// Allocate a sequential string body of `units` directly in old space, for
/// a string that lives as long as the isolate: no scavenge copies it, and a
/// root naming it need not be visited by one. It is Latin-1 when every unit
/// fits a byte, as every other sequential string is.
///
/// # Errors
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
pub(crate) fn alloc_old_string_body_with_roots(
    heap: &mut GcHeap,
    units: &[u16],
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsStringHandle, otter_gc::OutOfMemory> {
    let latin1 = units.iter().all(|&unit| unit <= 0xFF);
    let handle = heap.alloc_old_trailing_with_roots(
        JsStringBody::seq(latin1, units.len() as u32),
        JsStringBody::trailing_bytes(latin1, units.len()),
        external_visit,
    )?;
    heap.with_payload(handle, |body| {
        // SAFETY: the body reserved `units.len()` units of its width, every
        // unit fits a byte when it is Latin-1, and `units` is Rust-owned.
        unsafe { body.write_units_at(0, units.as_ptr().cast::<u8>(), true, units.len()) };
    });
    Ok(handle)
}

/// Allocate a Latin-1 string body.
///
/// # Errors
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
pub fn alloc_latin1_string_body_with_roots(
    heap: &mut GcHeap,
    bytes: &[u8],
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsStringHandle, otter_gc::OutOfMemory> {
    alloc_latin1_string_body_at(
        heap,
        bytes,
        otter_gc::SitePlacement::Young(0),
        external_visit,
    )
}

/// [`alloc_latin1_string_body_with_roots`] for an allocation site.
///
/// # Errors
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
pub(crate) fn alloc_latin1_string_body_at(
    heap: &mut GcHeap,
    bytes: &[u8],
    placement: otter_gc::SitePlacement,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsStringHandle, otter_gc::OutOfMemory> {
    heap.alloc_trailing_at_site_with_roots_initialized(
        JsStringBody::seq(true, bytes.len() as u32),
        JsStringBody::trailing_bytes(true, bytes.len()),
        placement,
        external_visit,
        |body| body.init_seq_latin1(bytes),
    )
}

/// Allocate a flat WTF-16 string body for an allocation site.
///
/// # Errors
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
pub(crate) fn alloc_flat_string_body_at(
    heap: &mut GcHeap,
    units: &[u16],
    placement: otter_gc::SitePlacement,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsStringHandle, otter_gc::OutOfMemory> {
    heap.alloc_trailing_at_site_with_roots_initialized(
        JsStringBody::seq(false, units.len() as u32),
        JsStringBody::trailing_bytes(false, units.len()),
        placement,
        external_visit,
        |body| body.init_seq_flat(units),
    )
}

/// Allocate a cons (`kind` [`STRING_REPR_CONS`], `second` the right child)
/// or sliced body (`second` the start) whose children the allocation keeps
/// rooted until its initializer writes them.
fn alloc_pair_body(
    heap: &mut GcHeap,
    body: JsStringBody,
    first: JsStringHandle,
    second: PairSecond,
    placement: otter_gc::SitePlacement,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsStringHandle, otter_gc::OutOfMemory> {
    let mut first = first;
    let mut second_handle = match second {
        PairSecond::Handle(handle) => handle,
        PairSecond::Word(_) => JsStringHandle::null(),
    };
    let first_slot: *mut JsStringHandle = &mut first;
    let second_slot: *mut JsStringHandle = &mut second_handle;
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        visitor(first_slot.cast::<RawGc>());
        visitor(second_slot.cast::<RawGc>());
        external_visit(visitor);
    };
    heap.alloc_trailing_at_site_with_roots_initialized(
        body,
        PAIR_BYTES,
        placement,
        &mut visit,
        |body| {
            // SAFETY: the slots live on this frame; a collection during the
            // allocation rewrote them, and none runs between there and here.
            let (first, second_handle) = unsafe { (*first_slot, *second_slot) };
            let second = match second {
                PairSecond::Handle(_) => second_handle.offset(),
                PairSecond::Word(word) => word,
            };
            body.init_pair(first, second);
        },
    )
}

/// The second word of a pair body.
#[derive(Clone, Copy)]
enum PairSecond {
    /// A cons's right child, rooted like the first.
    Handle(JsStringHandle),
    /// A slice's start.
    Word(u32),
}

/// Allocate a Latin-1 string body directly in old space through the heap's
/// diagnostic allocator, which neither collects nor charges the heap cap:
/// the message of an out-of-memory error is built while the nursery and
/// the cap are exhausted, beside its [`crate::object::alloc_diagnostic_object`]
/// instance.
///
/// # Errors
/// Surfaces [`otter_gc::OutOfMemory`] when old space has no room either.
pub(crate) fn alloc_latin1_string_body_diagnostic(
    heap: &mut GcHeap,
    bytes: &[u8],
) -> Result<JsStringHandle, otter_gc::OutOfMemory> {
    let handle = heap.alloc_old_diagnostic_trailing(
        JsStringBody::seq(true, bytes.len() as u32),
        JsStringBody::trailing_bytes(true, bytes.len()),
    )?;
    heap.with_payload(handle, |body| body.init_seq_latin1(bytes));
    Ok(handle)
}

/// Concatenate two GC string bodies into a `Cons` rope node.
///
/// Cheap: bounded by the depth-bound check plus one allocation. If
/// the resulting depth would exceed [`MAX_ROPE_DEPTH`], the deeper
/// child is flattened first.
///
/// # Errors
/// Returns [`StringConcatError::StringTooLong`] before flattening or allocation
/// when the exact UTF-16 length would exceed `u32::MAX`, or wraps the
/// collector's allocation failure in [`StringConcatError::OutOfMemory`].
pub fn concat_string_bodies(
    heap: &mut GcHeap,
    left: JsStringHandle,
    right: JsStringHandle,
    placement: otter_gc::SitePlacement,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsStringHandle, StringConcatError> {
    let (left_len, left_depth) = heap.read_payload(left, |b| (b.len, b.depth()));
    let (right_len, right_depth) = heap.read_payload(right, |b| (b.len, b.depth()));

    if right_len == 0 {
        return Ok(left);
    }
    if left_len == 0 {
        return Ok(right);
    }

    let new_len = left_len
        .checked_add(right_len)
        .ok_or(StringConcatError::StringTooLong {
            left_len,
            right_len,
        })?;

    // Short-result fast path: when the concatenation is short and both sides
    // are already materialised (non-cons/non-sliced), build the flat result
    // directly. A `Cons` node for a tiny result (`"k" + i` keys, small
    // id/label joins) is pure overhead — it retains both children and forces
    // a rope walk on every later hash/compare and an eventual flatten. The
    // sequential result is no larger than the rope and is read in O(1).
    if (new_len as usize) <= SHORT_CONCAT_LATIN1 {
        let mut units = [0u16; SHORT_CONCAT_LATIN1];
        let mut n = 0usize;
        let mut all_latin1 = true;
        let mut both_flat = true;
        for handle in [left, right] {
            let flat = heap.read_payload(handle, |b| match flat_content(b) {
                Some(FlatContent::Latin1(bytes)) => {
                    for &byte in bytes {
                        units[n] = u16::from(byte);
                        n += 1;
                    }
                    true
                }
                Some(FlatContent::Wide(wide)) => {
                    all_latin1 = false;
                    for &unit in wide {
                        units[n] = unit;
                        n += 1;
                    }
                    true
                }
                None => false,
            });
            if !flat {
                both_flat = false;
                break;
            }
        }
        if both_flat && (all_latin1 || n <= SHORT_CONCAT_FLAT) {
            return Ok(if all_latin1 {
                let mut bytes = [0u8; SHORT_CONCAT_LATIN1];
                for (dst, &unit) in bytes.iter_mut().zip(units[..n].iter()) {
                    *dst = unit as u8;
                }
                alloc_latin1_string_body_at(heap, &bytes[..n], placement, external_visit)?
            } else {
                alloc_flat_string_body_at(heap, &units[..n], placement, external_visit)?
            });
        }
    }

    let projected_depth = left_depth.max(right_depth).saturating_add(1);

    // Flatten deeper side eagerly if we'd exceed the depth budget. The
    // flattened copy allocates, so the other side stays in a rooted slot.
    let (left, right, left_depth, right_depth) = if projected_depth > MAX_ROPE_DEPTH {
        let flatten_left = left_depth >= right_depth;
        let (deeper, mut other) = if flatten_left {
            (left, right)
        } else {
            (right, left)
        };
        let other_slot: *mut JsStringHandle = &mut other;
        let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visitor(other_slot.cast::<RawGc>());
            external_visit(visitor);
        };
        let flat = flatten_string_body(heap, deeper, &mut visit)?;
        if flatten_left {
            (flat, other, 0u8, right_depth)
        } else {
            (other, flat, left_depth, 0u8)
        }
    } else {
        (left, right, left_depth, right_depth)
    };

    let final_depth = left_depth.max(right_depth).saturating_add(1);

    Ok(alloc_pair_body(
        heap,
        JsStringBody::pair(STRING_REPR_CONS, new_len, final_depth),
        left,
        PairSecond::Handle(right),
        placement,
        external_visit,
    )?)
}

/// Take an O(1) substring view over a contiguous parent. Cons sources
/// materialise only the requested range. Bounds are clamped to `[0, len()]`.
///
/// # Errors
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
pub fn slice_string_body(
    heap: &mut GcHeap,
    string: JsStringHandle,
    start: u32,
    length: u32,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsStringHandle, otter_gc::OutOfMemory> {
    let total = heap.read_payload(string, |b| b.len);
    let start = start.min(total);
    let length = length.min(total.saturating_sub(start));
    if length == 0 {
        return alloc_flat_string_body_with_roots(heap, &[], external_visit);
    }
    // Inspect the source variant once; for Sliced / Cons we may
    // need to allocate, so we collect the inputs first and avoid
    // holding a payload borrow across the alloc.
    enum SliceSource {
        Flat,
        SlicedCollapse {
            parent: JsStringHandle,
            abs_start: u32,
        },
        Cons,
    }
    let src = heap.read_payload(string, |b| match b.repr() {
        JsStringBodyRepr::SeqFlat | JsStringBodyRepr::SeqLatin1 => SliceSource::Flat,
        JsStringBodyRepr::Sliced {
            parent,
            start: pstart,
        } => SliceSource::SlicedCollapse {
            parent,
            abs_start: pstart + start,
        },
        JsStringBodyRepr::Cons { .. } => SliceSource::Cons,
    });
    match src {
        SliceSource::Flat => slice_view(heap, string, start, length, external_visit),
        // Compose into a single view over the original parent; never
        // produce Sliced(Sliced(...)).
        SliceSource::SlicedCollapse { parent, abs_start } => {
            slice_view(heap, parent, abs_start, length, external_visit)
        }
        SliceSource::Cons => {
            // Avoid flattening the whole rope for small substrings. Parsers
            // commonly slice thousands of short fields out of one large
            // concatenated input string; materialising the full source for
            // each field turns that workload into quadratic heap pressure.
            let units = to_utf16_vec_slice(heap, string, start, length);
            alloc_flat_string_body_with_roots(heap, &units, external_visit)
        }
    }
}

/// One run of a [`splice_string_body`] result: `len` units of source
/// `source` from its unit `start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SpliceRun {
    pub(crate) source: u8,
    pub(crate) start: u32,
    pub(crate) len: u32,
}

/// The sequential string of `runs` in order, each a span of one of
/// `sources`, built with a single allocation of the exact length and one
/// copy per run. Every source reads contiguously (a caller flattens a rope
/// first) and its runs lie within it; `sources` are rooted across the
/// allocation and updated in place if it moves them. The result is Latin-1
/// when every unit of every run fits a byte, and WTF-16 otherwise.
///
/// # Errors
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
pub(crate) fn splice_string_body(
    heap: &mut GcHeap,
    sources: &mut [JsStringHandle],
    runs: &[SpliceRun],
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsStringHandle, otter_gc::OutOfMemory> {
    let run_content = |heap: &GcHeap, sources: &[JsStringHandle], run: SpliceRun| {
        let (parent, base, _) = contiguous_view(heap, sources[usize::from(run.source)])
            .expect("splice source reads contiguously");
        (parent, base + run.start, run.len)
    };
    let mut total = 0usize;
    let mut latin1 = true;
    for &run in runs {
        total += run.len as usize;
        if latin1 {
            let (parent, start, len) = run_content(heap, sources, run);
            latin1 = heap.read_payload(parent, |body| {
                match flat_content_range(body, start, len).expect("sequential parent") {
                    FlatContent::Latin1(_) => true,
                    FlatContent::Wide(units) => units.iter().all(|&unit| unit <= 0xFF),
                }
            });
        }
    }
    let length = u32::try_from(total).expect("a splice is no longer than its sources");
    let slots = sources.as_mut_ptr();
    let count = sources.len();
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        for index in 0..count {
            // SAFETY: `sources` outlives the allocation that visits it.
            visitor(unsafe { slots.add(index) }.cast::<RawGc>());
        }
        external_visit(visitor);
    };
    let handle = heap.alloc_trailing_with_roots(
        JsStringBody::seq(latin1, length),
        JsStringBody::trailing_bytes(latin1, total),
        &mut visit,
    )?;
    // Nothing below allocates: the source and destination cells stay put.
    let mut at = 0usize;
    for &run in runs {
        let (parent, start, len) = run_content(heap, sources, run);
        let (data, wide) = heap.read_payload(parent, |body| {
            match flat_content_range(body, start, len).expect("sequential parent") {
                FlatContent::Latin1(bytes) => (bytes.as_ptr(), false),
                FlatContent::Wide(units) => (units.as_ptr().cast::<u8>(), true),
            }
        });
        heap.with_payload(handle, |body| {
            // SAFETY: the run lies within its distinct, live source cell;
            // the destination reserved `total` units and `at + len <= total`.
            unsafe { body.write_units_at(at, data, wide, len as usize) };
        });
        at += len as usize;
    }
    Ok(handle)
}

/// Allocation-buffer bytes [`try_substrings_no_collect`] takes per substring
/// at most: a copied short substring's cell, which is at least a view's.
pub(crate) const SUBSTRING_CELL_BYTES: usize =
    GcHeap::cell_bytes::<JsStringBody>(SHORT_CONCAT_LATIN1);
const _: () = assert!(SUBSTRING_CELL_BYTES >= GcHeap::cell_bytes::<JsStringBody>(PAIR_BYTES));

/// How a substring of a contiguous parent is built.
enum SubstringBody<'a> {
    /// Copied into a sequential body: short, so it retains no parent.
    Latin1(&'a [u8]),
    /// Copied wide units.
    Wide(&'a [u16]),
    /// A view sharing the parent.
    View,
}

/// The body a substring of `length` units of `content` takes.
fn substring_body(content: FlatContent<'_>) -> SubstringBody<'_> {
    match content {
        FlatContent::Latin1(bytes) if bytes.len() <= SHORT_CONCAT_LATIN1 => {
            SubstringBody::Latin1(bytes)
        }
        FlatContent::Wide(units) if units.len() <= SHORT_CONCAT_FLAT => SubstringBody::Wide(units),
        _ => SubstringBody::View,
    }
}

/// The substring of the contiguous `string` at each range, `undefined` for a
/// `None` range, appended to `out`, each in one cell from the allocation
/// buffer within the bytes the caller reserved ([`SUBSTRING_CELL_BYTES`] per
/// substring): short units copied straight from the subject, resolved once,
/// longer ones viewed in place. `None` when the string does not read
/// contiguously or the buffer runs out; `out` then holds a prefix.
pub(crate) fn try_substrings_no_collect(
    heap: &mut GcHeap,
    string: JsStringHandle,
    ranges: impl Iterator<Item = Option<std::ops::Range<usize>>>,
    out: &mut smallvec::SmallVec<[crate::Value; 16]>,
) -> Option<()> {
    let (parent, base, _) = contiguous_view(heap, string)?;
    let (data, latin1) = heap.read_payload(parent, |body| {
        (
            body.trailing_ptr(),
            body.repr() == JsStringBodyRepr::SeqLatin1,
        )
    });
    for range in ranges {
        let Some(range) = range else {
            out.push(crate::Value::undefined());
            continue;
        };
        let length = u32::try_from(range.len()).ok()?;
        let start = base.checked_add(u32::try_from(range.start).ok()?)?;
        // Nothing below collects or moves a cell, so `data` stays the
        // parent's units for the whole loop.
        let handle = if latin1 && range.len() <= SHORT_CONCAT_LATIN1 {
            heap.try_alloc_trailing_no_collect_or_return(
                JsStringBody::seq(true, length),
                range.len(),
                |body| {
                    // SAFETY: `start + length` lies inside the parent's
                    // Latin-1 payload, which no allocation here moves.
                    let bytes = unsafe {
                        std::slice::from_raw_parts(data.add(start as usize), range.len())
                    };
                    body.init_seq_latin1(bytes);
                },
            )
        } else if !latin1 && range.len() <= SHORT_CONCAT_FLAT {
            heap.try_alloc_trailing_no_collect_or_return(
                JsStringBody::seq(false, length),
                range.len() * 2,
                |body| {
                    // SAFETY: as above, for the parent's WTF-16 payload.
                    let units = unsafe {
                        std::slice::from_raw_parts(
                            data.cast::<u16>().add(start as usize),
                            range.len(),
                        )
                    };
                    body.init_seq_flat(units);
                },
            )
        } else {
            heap.try_alloc_trailing_no_collect_or_return(
                JsStringBody::pair(STRING_REPR_SLICED, length, 0),
                PAIR_BYTES,
                |body| body.init_pair(parent, start),
            )
        }
        .ok()?;
        out.push(crate::Value::string(crate::string::JsString::from_handle(
            handle, heap,
        )));
    }
    Some(())
}

/// `length` units at `start` of the contiguous `parent`: copied into a
/// sequential body when short, otherwise a view sharing the parent.
fn slice_view(
    heap: &mut GcHeap,
    parent: JsStringHandle,
    start: u32,
    length: u32,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsStringHandle, otter_gc::OutOfMemory> {
    let mut latin1 = [0u8; SHORT_CONCAT_LATIN1];
    let mut wide = [0u16; SHORT_CONCAT_FLAT];
    let built = heap.read_payload(parent, |body| {
        match substring_body(
            flat_content_range(body, start, length).expect("slice parent has contiguous content"),
        ) {
            SubstringBody::Latin1(bytes) => {
                latin1[..bytes.len()].copy_from_slice(bytes);
                Some((true, bytes.len()))
            }
            SubstringBody::Wide(units) => {
                wide[..units.len()].copy_from_slice(units);
                Some((false, units.len()))
            }
            SubstringBody::View => None,
        }
    });
    match built {
        Some((true, len)) => {
            alloc_latin1_string_body_with_roots(heap, &latin1[..len], external_visit)
        }
        Some((false, len)) => alloc_flat_string_body_with_roots(heap, &wide[..len], external_visit),
        None => alloc_pair_body(
            heap,
            JsStringBody::pair(STRING_REPR_SLICED, length, 0),
            parent,
            PairSecond::Word(start),
            otter_gc::SitePlacement::Young(0),
            external_visit,
        ),
    }
}

/// Realise a rope or sliced body into a fresh flat body. O(n) over
/// the length; iterative DFS, no recursion.
///
/// # Errors
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
pub fn flatten_string_body(
    heap: &mut GcHeap,
    string: JsStringHandle,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsStringHandle, otter_gc::OutOfMemory> {
    // Fast path — already flat.
    let is_flat = heap.read_payload(string, |b| b.repr() == JsStringBodyRepr::SeqFlat);
    if is_flat {
        return Ok(string);
    }
    let units = to_utf16_vec(heap, string);
    alloc_flat_string_body_with_roots(heap, &units, external_visit)
}

/// Flatten a cons rope / slice view **in place**: materialize its contents once
/// and rewrite the body's `repr` to a flat body (Latin-1 when every unit fits in
/// a byte, else WTF-16). A no-op for already-flat bodies.
///
/// Ropes are flattened on first content access: a string built incrementally
/// (`s += chunk`) is a rope, and every scan over it (`indexOf`, `includes`,
/// `split`, …) would otherwise re-walk and re-materialize the whole rope.
/// Flattening the shared body once makes every later access — and every later
/// call on the same handle — hit the O(1) flat / Latin-1 fast paths with no
/// allocation. The body keeps its `id`, `len`, and `hash` (content is
/// unchanged), so every existing handle and interner entry stays valid; the
/// old child handles simply become unreachable.
pub fn flatten_in_place(
    heap: &mut GcHeap,
    string: JsStringHandle,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    // "Already flat" means the units can be read contiguously, which a slice
    // over a sequential parent can. Testing the variant instead would call a
    // flattened rope indirect — it is rewritten into exactly such a view — and
    // re-flatten it, allocating a fresh copy of the whole string on every
    // read. That is quadratic for the `/g` loop this exists to speed up.
    if contiguous_view(heap, string).is_some() {
        return Ok(());
    }
    let units = to_utf16_vec(heap, string);
    let latin1 = units.iter().all(|&u| u <= 0xFF);
    // Keep `string` rooted: allocating the flattened body can scavenge, and
    // the handle we are about to rewrite must be the forwarded one.
    let mut rooted = string;
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        let p = &mut rooted as *mut JsStringHandle as *mut RawGc;
        visitor(p);
        external_visit(visitor);
    };
    // A sequential body sizes its storage at allocation, so the rope cannot
    // become one in place. Allocate the flat string and turn the rope node
    // into a view over it: same length, same hash, same identity, the same
    // two trailing words, and every later access takes the flat fast path
    // through one hop. The content is unchanged, so a hash computed earlier
    // stays exact.
    let flat = if latin1 {
        let bytes: Vec<u8> = units.iter().map(|&u| u as u8).collect();
        alloc_latin1_string_body_with_roots(heap, &bytes, &mut visit)?
    } else {
        alloc_flat_string_body_with_roots(heap, &units, &mut visit)?
    };
    heap.with_payload_relayout(rooted, |b| b.become_slice_of(flat));
    heap.record_write(rooted, &flat);
    Ok(())
}

/// Compare a JS string against a property-key spelling (see
/// [`crate::wtf16_escape`]) for code-unit equality **without allocating**
/// for the common single-segment case.
///
/// Property keys are overwhelmingly short, interned, single-segment
/// `Latin1` or `Flat` bodies, so the hot path (shape-key validation in
/// the property IC) compares in place against the spelling's code units.
/// Only the rare `Cons` / `Sliced` rope shapes fall back to the
/// allocating [`to_utf16_vec`] materialiser.
#[must_use]
pub fn eq_str(heap: &GcHeap, string: JsStringHandle, key: &str) -> bool {
    enum Fast {
        Mismatch,
        Match,
        Rope,
    }
    // An ASCII key (every property name in practice) has one UTF-16 unit
    // per byte: a length mismatch rejects at once, and a Latin-1 body
    // compares with one `memcmp`.
    let ascii = key.is_ascii();
    let fast = heap.read_payload(string, |b| match b.repr() {
        _ if ascii && b.len as usize != key.len() => Fast::Mismatch,
        JsStringBodyRepr::SeqLatin1 if ascii => {
            if b.seq_latin1_bytes() == key.as_bytes() {
                Fast::Match
            } else {
                Fast::Mismatch
            }
        }
        JsStringBodyRepr::SeqLatin1 => {
            let bytes = b.seq_latin1_bytes();
            // Latin-1 byte values are Unicode scalar values 0..=255,
            // so each zero-extends straight to a UTF-16 code unit.
            let mut units = crate::wtf16_escape::units(key);
            for &byte in bytes {
                match units.next() {
                    Some(u) if u == u16::from(byte) => {}
                    _ => return Fast::Mismatch,
                }
            }
            if units.next().is_none() {
                Fast::Match
            } else {
                Fast::Mismatch
            }
        }
        JsStringBodyRepr::SeqFlat => {
            let code_units = b.seq_flat_units();
            let mut units = crate::wtf16_escape::units(key);
            for &unit in code_units {
                match units.next() {
                    Some(u) if u == unit => {}
                    _ => return Fast::Mismatch,
                }
            }
            if units.next().is_none() {
                Fast::Match
            } else {
                Fast::Mismatch
            }
        }
        JsStringBodyRepr::Cons { .. } | JsStringBodyRepr::Sliced { .. } => Fast::Rope,
    });
    match fast {
        Fast::Match => true,
        Fast::Mismatch => false,
        // A slice over a contiguous parent — what a flattened rope is — still
        // compares in place; only a genuine cons rope has to materialise.
        Fast::Rope => match contiguous_view(heap, string) {
            Some((target, start, len)) => {
                heap.read_payload(target, |b| match flat_content_range(b, start, len) {
                    Some(FlatContent::Latin1(bytes)) => bytes
                        .iter()
                        .map(|&x| u16::from(x))
                        .eq(crate::wtf16_escape::units(key)),
                    Some(FlatContent::Wide(units)) => {
                        units.iter().copied().eq(crate::wtf16_escape::units(key))
                    }
                    None => unreachable!("resolved view stores its units contiguously"),
                })
            }
            None => to_utf16_vec(heap, string)
                .into_iter()
                .eq(crate::wtf16_escape::units(key)),
        },
    }
}

/// Materialise a string body into a fresh `Vec<u16>` of UTF-16 code
/// units. Cold path: hot lookups should compare handles / ids.
///
/// Large subjects (≥ [`UTF16_CACHE_MIN_LEN`]) memoise their widened units
/// on the body's [`JsStringBody::utf16_cache`], so repeated materialisation
/// (a `/g` regex `exec` loop re-scanning the same subject on every match)
/// serves a `memcpy` from the cache instead of re-walking the rope /
/// re-widening Latin-1 on every call.
#[must_use]
pub fn to_utf16_vec(heap: &GcHeap, string: JsStringHandle) -> Vec<u16> {
    materialize_utf16_vec(heap, string)
}

/// Ensure a large subject's widened UTF-16 units are cached on its body,
/// then run `f` over a borrow of them. Repeated calls (a `/g` regex `exec`
/// loop re-scanning the same subject on every match) reuse the buffer, so
/// only the first call walks the rope / widens Latin-1 — every later call is
/// O(1) plus whatever `f` copies out. Small subjects skip the cache and
/// materialise a throwaway `Vec` (the widen is cheap, the memory is not
/// worth it).
///
/// The borrow handed to `f` is valid only for the call. `f` MUST NOT trigger
/// a heap allocation that could move this body (the borrow would dangle);
/// callers needing the units across an allocation copy the required ranges
/// out inside `f` first. Reading other bodies (e.g. the compiled regex) is
/// fine — that is a shared borrow, not a mutation.
pub fn with_utf16<R>(heap: &GcHeap, string: JsStringHandle, f: impl FnOnce(&[u16]) -> R) -> R {
    // Latin-1 units are one byte each and the caller wants `u16`, so only a
    // wide view can be handed over borrowed; a Latin-1 one still widens.
    let wide = contiguous_view(heap, string).filter(|&(target, start, len)| {
        heap.read_payload(target, |b| {
            matches!(
                flat_content_range(b, start, len),
                Some(FlatContent::Wide(_))
            )
        })
    });
    if let Some((target, start, len)) = wide {
        return heap.read_payload(target, |b| match flat_content_range(b, start, len) {
            Some(FlatContent::Wide(units)) => f(units),
            _ => unreachable!("view was checked to be wide"),
        });
    }
    let units = materialize_utf16_vec(heap, string);
    f(&units)
}

/// A string's code units borrowed at the width they are stored in.
#[derive(Debug, Clone, Copy)]
pub enum CodeUnits<'a> {
    /// One byte per code unit; every unit is below `0x100`.
    Latin1(&'a [u8]),
    /// UTF-16 code units.
    Utf16(&'a [u16]),
}

impl CodeUnits<'_> {
    /// Number of code units.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Latin1(bytes) => bytes.len(),
            Self::Utf16(units) => units.len(),
        }
    }

    /// Whether there are no code units.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The code unit at `index`, widened to UTF-16.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<u16> {
        match self {
            Self::Latin1(bytes) => bytes.get(index).map(|&byte| u16::from(byte)),
            Self::Utf16(units) => units.get(index).copied(),
        }
    }

    /// Whether some unit equals `unit`.
    #[must_use]
    pub fn contains(&self, unit: u16) -> bool {
        match self {
            Self::Latin1(bytes) => u8::try_from(unit).is_ok_and(|byte| bytes.contains(&byte)),
            Self::Utf16(units) => units.contains(&unit),
        }
    }

    /// The units in `range`, at the same width.
    #[must_use]
    pub fn slice(&self, range: std::ops::Range<usize>) -> Self {
        match self {
            Self::Latin1(bytes) => CodeUnits::Latin1(&bytes[range]),
            Self::Utf16(units) => CodeUnits::Utf16(&units[range]),
        }
    }
}

/// Run `f` over the string's code units at their stored width.
///
/// A contiguous body — flat, a collapsed slice, or one with a filled widened
/// cache — is borrowed in place, so a one-byte string is never widened; any
/// other body is materialised as UTF-16 first. `f` must not allocate on the
/// heap while holding the borrow.
pub fn with_code_units<R>(
    heap: &GcHeap,
    string: JsStringHandle,
    f: impl FnOnce(CodeUnits<'_>) -> R,
) -> R {
    let mut f = Some(f);
    if let Some((target, start, len)) = contiguous_view(heap, string)
        && let Some(result) = heap.read_payload(target, |b| {
            let units = match flat_content_range(b, start, len)? {
                FlatContent::Latin1(bytes) => CodeUnits::Latin1(bytes),
                FlatContent::Wide(units) => CodeUnits::Utf16(units),
            };
            Some((f.take().expect("unused reader"))(units))
        })
    {
        return result;
    }
    let units = materialize_utf16_vec(heap, string);
    (f.take().expect("unused reader"))(CodeUnits::Utf16(&units))
}

/// Run `f` over a contiguous Latin-1 view, following one collapsed slice hop.
///
/// Direct and sliced Latin-1 bodies therefore share the same allocation-free
/// reader path. Wide bodies and cons ropes return `None`.
pub fn with_latin1<R>(
    heap: &GcHeap,
    string: JsStringHandle,
    f: impl FnOnce(&[u8]) -> R,
) -> Option<R> {
    let (target, start, len) = contiguous_view(heap, string)?;
    heap.read_payload(target, |body| match flat_content_range(body, start, len) {
        Some(FlatContent::Latin1(bytes)) => Some(f(bytes)),
        _ => None,
    })
}

/// The atom interned for the string's content, looked up from its units in
/// place. `None` when the content is a rope no read has flattened, whose
/// units would first have to be materialised.
#[must_use]
pub(crate) fn lookup_atom_in_place(
    heap: &GcHeap,
    string: JsStringHandle,
    names: &crate::property_atom::NameInterner,
) -> Option<crate::property_atom::AtomId> {
    use crate::property_atom::NameUnits;
    let (target, start, len) = contiguous_view(heap, string)?;
    heap.read_payload(target, |body| {
        Some(
            names.lookup_units(match flat_content_range(body, start, len)? {
                FlatContent::Latin1(bytes) => NameUnits::Latin1(bytes),
                FlatContent::Wide(units) => NameUnits::Utf16(units),
            }),
        )
    })
}

/// The interner's hash of the string's code units
/// ([`crate::property_atom::spelling_hash`] of its spelling), read in place.
/// `None` for a rope no read has flattened.
#[must_use]
pub(crate) fn spelling_hash_in_place(heap: &GcHeap, string: JsStringHandle) -> Option<u64> {
    use crate::property_atom::NameUnits;
    let (target, start, len) = contiguous_view(heap, string)?;
    heap.read_payload(target, |body| {
        Some(match flat_content_range(body, start, len)? {
            FlatContent::Latin1(bytes) => NameUnits::Latin1(bytes).hash(),
            FlatContent::Wide(units) => NameUnits::Utf16(units).hash(),
        })
    })
}

/// Whether the string's code units can be borrowed through a direct body or a
/// collapsed slice view, without materialising a rope.
#[must_use]
pub fn is_contiguous(heap: &GcHeap, string: JsStringHandle) -> bool {
    contiguous_view(heap, string).is_some()
}

/// Resolve `string` to `(body holding the units, start, len)` when its code
/// units can be read in place, or `None` when they must be materialised.
///
/// Two shapes qualify. The body stores its own units contiguously. Or it is
/// a slice view over one — which is what a flattened rope becomes, so the
/// view is a range of the parent's contiguous units and needs no copy either.
///
/// Missing the slice case is expensive in a specific way: since flattening
/// rewrites a rope into a view, *every* read of an already-flattened rope
/// would re-materialise the whole string, turning a `/g` regex loop over a
/// long subject quadratic.
fn contiguous_view(heap: &GcHeap, string: JsStringHandle) -> Option<(JsStringHandle, u32, u32)> {
    let (repr, len) = heap.read_payload(string, |b| (b.repr(), b.len));
    match repr {
        JsStringBodyRepr::SeqFlat | JsStringBodyRepr::SeqLatin1 => Some((string, 0, len)),
        JsStringBodyRepr::Sliced { parent, start }
            if heap.read_payload(parent, |p| stores_units_sequentially(p.repr())) =>
        {
            Some((parent, start, len))
        }
        _ => None,
    }
}

/// Whether a body holds its own code units contiguously.
fn stores_units_sequentially(repr: JsStringBodyRepr) -> bool {
    matches!(
        repr,
        JsStringBodyRepr::SeqFlat | JsStringBodyRepr::SeqLatin1
    )
}

/// Borrow `body`'s contiguous units narrowed to `start..start + len`.
fn flat_content_range(body: &JsStringBody, start: u32, len: u32) -> Option<FlatContent<'_>> {
    let (s, e) = (start as usize, start as usize + len as usize);
    Some(match flat_content(body)? {
        FlatContent::Latin1(bytes) => FlatContent::Latin1(&bytes[s..e]),
        FlatContent::Wide(units) => FlatContent::Wide(&units[s..e]),
    })
}

/// Uncached body walk backing [`to_utf16_vec`]. Always re-materialises.
fn materialize_utf16_vec(heap: &GcHeap, string: JsStringHandle) -> Vec<u16> {
    let len = heap.read_payload(string, |b| b.len);
    let mut out: Vec<u16> = Vec::with_capacity(len as usize);
    let mut stack: Vec<(JsStringHandle, u32, u32)> = Vec::new();
    stack.push((string, 0, len));
    while let Some((node, start, length)) = stack.pop() {
        enum Resolved {
            Flat,
            Latin1,
            Sliced {
                parent: JsStringHandle,
                abs_start: u32,
            },
            Cons {
                left: JsStringHandle,
                right: JsStringHandle,
                left_len: u32,
            },
        }
        let resolved = heap.read_payload(node, |b| match b.repr() {
            JsStringBodyRepr::SeqFlat => {
                let units = b.seq_flat_units();
                // Clamp the view to the body's actual length. A
                // sliced body may carry a `start` that exceeds the
                // flat parent's length when the parent was replaced
                // (e.g. interned via `from_str` and a smaller body
                // now lives at the same handle), or when callers
                // build an out-of-bounds substring through the
                // pre-existing `String.prototype.slice` clamping.
                let s = (start as usize).min(units.len());
                let e = s.saturating_add(length as usize).min(units.len());
                out.extend_from_slice(&units[s..e]);
                Resolved::Flat
            }
            JsStringBodyRepr::SeqLatin1 => {
                let bytes = b.seq_latin1_bytes();
                let s = (start as usize).min(bytes.len());
                let e = s.saturating_add(length as usize).min(bytes.len());
                out.extend(bytes[s..e].iter().map(|&b| u16::from(b)));
                Resolved::Latin1
            }
            JsStringBodyRepr::Sliced {
                parent,
                start: pstart,
            } => Resolved::Sliced {
                parent,
                abs_start: pstart + start,
            },
            JsStringBodyRepr::Cons { left, right, .. } => Resolved::Cons {
                left,
                right,
                left_len: heap.read_payload(left, |lb| lb.len),
            },
        });
        match resolved {
            Resolved::Flat | Resolved::Latin1 => {}
            Resolved::Sliced { parent, abs_start } => {
                stack.push((parent, abs_start, length));
            }
            Resolved::Cons {
                left,
                right,
                left_len,
            } => {
                // Compute how `[start, start+length)` splits across
                // the left / right children. Push right then left
                // so the left is processed first (LIFO).
                let split = left_len.min(start.saturating_add(length));
                let left_take = split.saturating_sub(start);
                let right_take = length.saturating_sub(left_take);
                let right_start = start.saturating_sub(left_len);
                if right_take > 0 {
                    stack.push((right, right_start, right_take));
                }
                if left_take > 0 {
                    stack.push((left, start, left_take));
                }
            }
        }
    }
    out
}

/// Substring view as `Vec<u16>` — internal helper for hash
/// computation on collapsed slices.
fn to_utf16_vec_slice(heap: &GcHeap, parent: JsStringHandle, start: u32, length: u32) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::with_capacity(length as usize);
    let mut stack: Vec<(JsStringHandle, u32, u32)> = Vec::new();
    stack.push((parent, start, length));
    while let Some((node, s, l)) = stack.pop() {
        enum Resolved {
            Done,
            Sliced {
                parent: JsStringHandle,
                abs_start: u32,
            },
            Cons {
                left: JsStringHandle,
                right: JsStringHandle,
                left_len: u32,
            },
        }
        let resolved = heap.read_payload(node, |b| match b.repr() {
            JsStringBodyRepr::SeqFlat => {
                let units = b.seq_flat_units();
                let lo = s as usize;
                let hi = lo + l as usize;
                out.extend_from_slice(&units[lo..hi]);
                Resolved::Done
            }
            JsStringBodyRepr::SeqLatin1 => {
                let bytes = b.seq_latin1_bytes();
                let lo = s as usize;
                let hi = lo + l as usize;
                out.extend(bytes[lo..hi].iter().map(|&b| u16::from(b)));
                Resolved::Done
            }
            JsStringBodyRepr::Sliced {
                parent,
                start: pstart,
            } => Resolved::Sliced {
                parent,
                abs_start: pstart + s,
            },
            JsStringBodyRepr::Cons { left, right, .. } => Resolved::Cons {
                left,
                right,
                left_len: heap.read_payload(left, |lb| lb.len),
            },
        });
        match resolved {
            Resolved::Done => {}
            Resolved::Sliced { parent, abs_start } => {
                stack.push((parent, abs_start, l));
            }
            Resolved::Cons {
                left,
                right,
                left_len,
            } => {
                let split = left_len.min(s.saturating_add(l));
                let left_take = split.saturating_sub(s);
                let right_take = l.saturating_sub(left_take);
                let right_start = s.saturating_sub(left_len);
                if right_take > 0 {
                    stack.push((right, right_start, right_take));
                }
                if left_take > 0 {
                    stack.push((left, s, left_take));
                }
            }
        }
    }
    out
}

/// Borrowed view over a directly-stored (non-cons, non-sliced) flat body's
/// code units. Lets two flat bodies compare content in place instead of each
/// allocating a throwaway `to_utf16_vec`.
enum FlatContent<'a> {
    Latin1(&'a [u8]),
    Wide(&'a [u16]),
}

/// Content view for the four directly-stored variants; `None` for `Cons` /
/// `Sliced`, which carry no contiguous buffer *of their own*. A slice over
/// a sequential parent does have contiguous units, but they belong to the
/// parent body; reading those in place goes through
/// [`sequential_utf16_view`], which can resolve the hop.
fn flat_content(body: &JsStringBody) -> Option<FlatContent<'_>> {
    match body.repr() {
        JsStringBodyRepr::SeqLatin1 => Some(FlatContent::Latin1(body.seq_latin1_bytes())),
        JsStringBodyRepr::SeqFlat => Some(FlatContent::Wide(body.seq_flat_units())),
        JsStringBodyRepr::Cons { .. } | JsStringBodyRepr::Sliced { .. } => None,
    }
}

impl FlatContent<'_> {
    /// Lexicographic code-unit ordering across any pairing of Latin-1 /
    /// WTF-16 storage. Latin-1 bytes zero-extend to their code units, so
    /// byte order and code-unit order agree and the two-byte case compares
    /// the raw slices directly.
    fn content_cmp(&self, other: &FlatContent<'_>) -> std::cmp::Ordering {
        match (self, other) {
            (FlatContent::Latin1(a), FlatContent::Latin1(b)) => a.cmp(b),
            (FlatContent::Wide(a), FlatContent::Wide(b)) => a.cmp(b),
            (FlatContent::Latin1(bytes), FlatContent::Wide(units)) => bytes
                .iter()
                .map(|&b| u16::from(b))
                .cmp(units.iter().copied()),
            (FlatContent::Wide(units), FlatContent::Latin1(bytes)) => units
                .iter()
                .copied()
                .cmp(bytes.iter().map(|&b| u16::from(b))),
        }
    }

    /// Code-unit equality across any pairing of Latin-1 / WTF-16 storage. A
    /// Latin-1 byte zero-extends to its `u16` code unit.
    fn content_eq(&self, other: &FlatContent<'_>) -> bool {
        match (self, other) {
            (FlatContent::Latin1(a), FlatContent::Latin1(b)) => a == b,
            (FlatContent::Wide(a), FlatContent::Wide(b)) => a == b,
            (FlatContent::Latin1(bytes), FlatContent::Wide(units))
            | (FlatContent::Wide(units), FlatContent::Latin1(bytes)) => {
                bytes.len() == units.len()
                    && bytes
                        .iter()
                        .zip(units.iter())
                        .all(|(&byte, &unit)| u16::from(byte) == unit)
            }
        }
    }
}

/// Two-string equality on UTF-16 code units. Fast paths:
/// - identity (`Gc::eq`);
/// - length mismatch returns `false` immediately;
/// - hash mismatch returns `false` immediately;
/// - both sides flat: direct in-place content compare (no materialisation).
#[must_use]
pub fn equals_string_bodies(heap: &GcHeap, a: JsStringHandle, b: JsStringHandle) -> bool {
    if a == b {
        return true;
    }
    // Length, recorded atoms and, when both are computed, hash decide first;
    // a `Cons` / `Sliced` body falls through to the in-place or streaming
    // compare.
    if let Some(answer) = heap.read_payload(a, |ba| {
        heap.read_payload(b, |bb| {
            if ba.len != bb.len {
                return Some(false);
            }
            if ba.len == 0 {
                return Some(true);
            }
            if ba.atom != 0 && bb.atom != 0 {
                return Some(ba.atom == bb.atom);
            }
            if ba.hash != 0 && bb.hash != 0 && ba.hash != bb.hash {
                return Some(false);
            }
            None
        })
    }) {
        return answer;
    }
    // Resolve each side through at most one slice hop, so a flattened rope —
    // which is a view over a contiguous parent — compares in place instead of
    // materialising both operands.
    if let (Some((ta, sa, la)), Some((tb, sb, lb))) =
        (contiguous_view(heap, a), contiguous_view(heap, b))
    {
        return heap.read_payload(ta, |ba| {
            heap.read_payload(tb, |bb| {
                match (
                    flat_content_range(ba, sa, la),
                    flat_content_range(bb, sb, lb),
                ) {
                    (Some(va), Some(vb)) => va.content_eq(&vb),
                    _ => unreachable!("resolved views store their units contiguously"),
                }
            })
        });
    }
    super::code_units::CodeUnits::new(heap, a).eq(super::code_units::CodeUnits::new(heap, b))
}

/// Lexicographic code-unit ordering of two string bodies. Both sides flat
/// (inline or side storage, either width) compare in place; a `Cons` /
/// `Sliced` operand streams the immutable rope content with a bounded frontier.
#[must_use]
pub fn compare_string_bodies(
    heap: &GcHeap,
    a: JsStringHandle,
    b: JsStringHandle,
) -> std::cmp::Ordering {
    if a == b {
        return std::cmp::Ordering::Equal;
    }
    if let (Some((ta, sa, la)), Some((tb, sb, lb))) =
        (contiguous_view(heap, a), contiguous_view(heap, b))
    {
        return heap.read_payload(ta, |ba| {
            heap.read_payload(tb, |bb| {
                match (
                    flat_content_range(ba, sa, la),
                    flat_content_range(bb, sb, lb),
                ) {
                    (Some(va), Some(vb)) => va.content_cmp(&vb),
                    _ => unreachable!("resolved views store their units contiguously"),
                }
            })
        });
    }
    super::code_units::CodeUnits::new(heap, a).cmp(super::code_units::CodeUnits::new(heap, b))
}

/// Copy a string body's flat Latin-1 bytes into `out` when it is a short,
/// non-wide flat body (inline or side-storage Latin-1) that fits. Returns the
/// byte length, or `None` for a wide, cons/sliced, or over-long body — the
/// caller then takes its general path. Used by the `string + number` concat
/// fast path to build the result in a single allocation.
pub fn read_short_flat_latin1(
    heap: &GcHeap,
    handle: JsStringHandle,
    out: &mut [u8; 32],
) -> Option<usize> {
    let (target, start, len) = contiguous_view(heap, handle)?;
    heap.read_payload(target, |body| match flat_content_range(body, start, len) {
        Some(FlatContent::Latin1(bytes)) if bytes.len() <= out.len() => {
            out[..bytes.len()].copy_from_slice(bytes);
            Some(bytes.len())
        }
        _ => None,
    })
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a over UTF-16 code units, one code unit per step.
#[must_use]
pub fn hash_utf16(units: &[u16]) -> u64 {
    units.iter().fold(FNV_OFFSET, |hash, &unit| {
        (hash ^ u64::from(unit)).wrapping_mul(FNV_PRIME)
    })
}

/// [`hash_utf16`] of the zero-extended Latin-1 bytes.
#[must_use]
pub fn hash_latin1(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV_OFFSET, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    })
}

/// Content hash of `string`, computed on first request and cached in the
/// body.
pub fn string_hash(heap: &mut GcHeap, string: JsStringHandle) -> u32 {
    let cached = heap.read_payload(string, |body| body.hash);
    if cached != 0 {
        return cached;
    }
    let hash = content_hash(heap, string);
    heap.with_payload(string, |body| body.hash = hash);
    hash
}

/// [`string_hash`] for a caller holding only a shared heap: the cached hash
/// when present, otherwise computed without caching.
#[must_use]
pub fn peek_string_hash(heap: &GcHeap, string: JsStringHandle) -> u32 {
    let cached = heap.read_payload(string, |body| body.hash);
    if cached != 0 {
        cached
    } else {
        content_hash(heap, string)
    }
}

/// Exact hash of the code units of any representation, folded to 32 bits;
/// never zero, which marks a hash not yet computed.
fn content_hash(heap: &GcHeap, string: JsStringHandle) -> u32 {
    let hash = match contiguous_view(heap, string) {
        Some((target, start, len)) => heap.read_payload(target, |body| {
            match flat_content_range(body, start, len)
                .expect("resolved views store their units contiguously")
            {
                FlatContent::Latin1(bytes) => hash_latin1(bytes),
                FlatContent::Wide(units) => hash_utf16(units),
            }
        }),
        None => super::code_units::CodeUnits::new(heap, string).fold(FNV_OFFSET, |hash, unit| {
            (hash ^ u64::from(unit)).wrapping_mul(FNV_PRIME)
        }),
    };
    fold_hash(hash)
}

/// A 64-bit code-unit hash folded to the nonzero 32-bit cached form.
#[must_use]
pub(crate) const fn fold_hash(hash: u64) -> u32 {
    let folded = ((hash ^ (hash >> 32)) & 0xFFFF_FFFF) as u32;
    if folded == 0 { 1 } else { folded }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_roots(_v: &mut dyn FnMut(*mut RawGc)) {}

    fn is_young(handle: JsStringHandle) -> bool {
        // SAFETY: each tested handle names a live body in its owning heap.
        unsafe { (*handle.as_header_ptr()).is_young() }
    }

    #[test]
    fn ordinary_string_representations_start_in_young_space() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let inline = alloc_flat_string_body_with_roots(&mut heap, &[0x1234], &mut roots)
            .expect("inline flat");
        assert!(is_young(inline));

        let sequential_units = vec![0x1234; SHORT_CONCAT_FLAT + 1];
        let sequential =
            alloc_flat_string_body_with_roots(&mut heap, &sequential_units, &mut roots)
                .expect("sequential flat");
        assert!(is_young(sequential));

        let latin1 = alloc_latin1_string_body_with_roots(&mut heap, b"latin1", &mut roots)
            .expect("inline latin1");
        assert!(is_young(latin1));

        let sequential_bytes = vec![b'x'; SHORT_CONCAT_LATIN1 + 1];
        let sequential_latin1 =
            alloc_latin1_string_body_with_roots(&mut heap, &sequential_bytes, &mut roots)
                .expect("sequential latin1");
        assert!(is_young(sequential_latin1));
    }

    #[test]
    fn rooted_string_representations_survive_a_minor_collection() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let mut inline_wide = JsStringHandle::null();
        let mut sequential_wide = JsStringHandle::null();
        let mut inline_latin1 = JsStringHandle::null();
        let mut sequential_latin1 = JsStringHandle::null();
        let mut cons_left = JsStringHandle::null();
        let mut cons_right = JsStringHandle::null();
        let mut cons = JsStringHandle::null();
        let mut sliced = JsStringHandle::null();
        let mut scope = otter_gc::RootScope::new(&mut heap);
        // SAFETY: every slot precedes the scope and remains stationary until
        // after the explicit scavenge and all representation checks.
        unsafe {
            for slot in [
                &mut inline_wide,
                &mut sequential_wide,
                &mut inline_latin1,
                &mut sequential_latin1,
                &mut cons_left,
                &mut cons_right,
                &mut cons,
                &mut sliced,
            ] {
                scope.add_raw_slot((slot as *mut JsStringHandle).cast::<RawGc>());
            }
        }

        inline_wide = alloc_flat_string_body_with_roots(&mut heap, &[0x1234], &mut roots)
            .expect("inline wide");
        let wide_units = vec![0x2345; SHORT_CONCAT_FLAT + 1];
        sequential_wide = alloc_flat_string_body_with_roots(&mut heap, &wide_units, &mut roots)
            .expect("sequential wide");
        inline_latin1 = alloc_latin1_string_body_with_roots(&mut heap, b"latin1", &mut roots)
            .expect("inline latin1");
        let latin1_bytes = vec![b'x'; SHORT_CONCAT_LATIN1 + 1];
        sequential_latin1 =
            alloc_latin1_string_body_with_roots(&mut heap, &latin1_bytes, &mut roots)
                .expect("sequential latin1");
        cons_left = alloc_latin1_string_body_with_roots(&mut heap, b"abcdefghijklmnop", &mut roots)
            .expect("cons left");
        cons_right =
            alloc_latin1_string_body_with_roots(&mut heap, b"qrstuvwxyz012345", &mut roots)
                .expect("cons right");
        cons = concat_string_bodies(
            &mut heap,
            cons_left,
            cons_right,
            otter_gc::SitePlacement::Young(0),
            &mut roots,
        )
        .expect("cons");
        sliced =
            slice_string_body(&mut heap, sequential_latin1, 5, 10, &mut roots).expect("sliced");

        heap.collect_minor_with_roots(&mut roots)
            .expect("explicit minor collection");

        assert_eq!(to_utf16_vec(&heap, inline_wide), vec![0x1234]);
        assert_eq!(to_utf16_vec(&heap, sequential_wide), wide_units);
        assert_eq!(
            with_latin1(&heap, inline_latin1, |v| v.to_vec()),
            Some(b"latin1".to_vec())
        );
        assert_eq!(
            with_latin1(&heap, sequential_latin1, |v| v.to_vec()),
            Some(latin1_bytes)
        );
        assert_eq!(
            to_utf16_vec(&heap, cons),
            b"abcdefghijklmnopqrstuvwxyz012345"
                .iter()
                .map(|&byte| u16::from(byte))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            with_latin1(&heap, sliced, |v| v.to_vec()),
            Some(vec![b'x'; 10])
        );
    }

    #[test]
    fn cells_hold_exactly_their_kind_payload() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let size = |heap: &GcHeap, handle: JsStringHandle| {
            heap.read_payload(handle, |_| ());
            // SAFETY: the handle names a live body.
            unsafe { (*handle.as_header_ptr()).size_bytes() }
        };
        let short = alloc_latin1_string_body_with_roots(&mut heap, b"key", &mut roots).unwrap();
        assert_eq!(size(&heap, short), 32, "header, fixed part, three bytes");
        let wide = alloc_flat_string_body_with_roots(&mut heap, &[0x100; 6], &mut roots).unwrap();
        assert_eq!(size(&heap, wide), 40);
        let left = alloc_latin1_string_body_with_roots(&mut heap, &[b'a'; 30], &mut roots).unwrap();
        let cons = concat_string_bodies(
            &mut heap,
            left,
            left,
            otter_gc::SitePlacement::Young(0),
            &mut roots,
        )
        .unwrap();
        assert_eq!(size(&heap, cons), 40, "header, fixed part, two child words");
        heap.read_payload(cons, |b| {
            assert!(matches!(b.repr(), JsStringBodyRepr::Cons { depth: 1, .. }));
        });
    }

    #[test]
    fn allocates_empty_flat_string() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let s = alloc_flat_string_body_with_roots(&mut heap, &[], &mut roots).expect("flat");
        heap.read_payload(s, |b| {
            assert_eq!(b.cached_atom(), None);
            assert_eq!(b.len(), 0);
            assert!(b.is_empty());
            assert!(matches!(b.repr(), JsStringBodyRepr::SeqFlat));
        });
        assert!(to_utf16_vec(&heap, s).is_empty());
    }

    #[test]
    fn allocates_long_flat_string() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let units: Vec<u16> = (0..200).map(|i| (i % 0xd7ff) as u16).collect();
        let s = alloc_flat_string_body_with_roots(&mut heap, &units, &mut roots).expect("flat");
        heap.read_payload(s, |b| {
            assert_eq!(b.len(), units.len() as u32);
            assert_eq!(b.hash, 0, "allocation never hashes");
            assert!(matches!(b.repr(), JsStringBodyRepr::SeqFlat));
        });
        assert_eq!(to_utf16_vec(&heap, s), units);
    }

    #[test]
    fn cons_concat_round_trips_to_utf16() {
        // Results within the inline-latin1 cap flatten in place; build operands
        // long enough that the concatenation stays an unflattened `Cons` rope.
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let mut left = JsStringHandle::null();
        let mut right = JsStringHandle::null();
        let mut cons = JsStringHandle::null();
        let mut scope = otter_gc::RootScope::new(&mut heap);
        // SAFETY: all three slots precede the scope and remain stationary.
        unsafe {
            scope.add_raw_slot((&mut left as *mut JsStringHandle).cast::<RawGc>());
            scope.add_raw_slot((&mut right as *mut JsStringHandle).cast::<RawGc>());
            scope.add_raw_slot((&mut cons as *mut JsStringHandle).cast::<RawGc>());
        }
        let left_units: Vec<u16> = std::iter::repeat_n(b'a' as u16, 16).collect();
        let right_units: Vec<u16> = std::iter::repeat_n(b'b' as u16, 16).collect();
        left = alloc_flat_string_body_with_roots(&mut heap, &left_units, &mut roots).expect("left");
        right =
            alloc_flat_string_body_with_roots(&mut heap, &right_units, &mut roots).expect("right");
        cons = concat_string_bodies(
            &mut heap,
            left,
            right,
            otter_gc::SitePlacement::Young(0),
            &mut roots,
        )
        .expect("cons");
        heap.read_payload(cons, |b| {
            assert_eq!(b.len(), 32);
            assert!(matches!(b.repr(), JsStringBodyRepr::Cons { .. }));
        });
        let mut expected = left_units.clone();
        expected.extend_from_slice(&right_units);
        assert_eq!(to_utf16_vec(&heap, cons), expected);
    }

    #[test]
    fn sliced_view_round_trips() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let mut flat = JsStringHandle::null();
        let mut view = JsStringHandle::null();
        let mut scope = otter_gc::RootScope::new(&mut heap);
        // SAFETY: both slots precede the scope and remain stationary.
        unsafe {
            scope.add_raw_slot((&mut flat as *mut JsStringHandle).cast::<RawGc>());
            scope.add_raw_slot((&mut view as *mut JsStringHandle).cast::<RawGc>());
        }
        let units: Vec<u16> = b"hello world, hello sliced view of a wide parent"
            .iter()
            .map(|&b| b as u16)
            .collect();
        flat = alloc_flat_string_body_with_roots(&mut heap, &units, &mut roots).expect("flat");
        view = slice_string_body(&mut heap, flat, 6, 20, &mut roots).expect("slice");
        heap.read_payload(view, |b| {
            assert_eq!(b.len(), 20);
            assert!(matches!(b.repr(), JsStringBodyRepr::Sliced { .. }));
        });
        assert_eq!(to_utf16_vec(&heap, view), units[6..26]);
    }

    #[test]
    fn latin1_body_round_trips() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let s =
            alloc_latin1_string_body_with_roots(&mut heap, b"hello", &mut roots).expect("latin1");
        heap.read_payload(s, |b| {
            assert_eq!(b.len(), 5);
            assert!(matches!(b.repr(), JsStringBodyRepr::SeqLatin1));
        });
        let expected: Vec<u16> = b"hello".iter().map(|&b| b as u16).collect();
        assert_eq!(to_utf16_vec(&heap, s), expected);
    }

    #[test]
    fn latin1_slice_is_a_width_preserving_view() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let mut parent = JsStringHandle::null();
        let mut view = JsStringHandle::null();
        let mut scope = otter_gc::RootScope::new(&mut heap);
        // SAFETY: both slots precede the scope and remain stationary.
        unsafe {
            scope.add_raw_slot((&mut parent as *mut JsStringHandle).cast::<RawGc>());
            scope.add_raw_slot((&mut view as *mut JsStringHandle).cast::<RawGc>());
        }
        let bytes = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
        parent = alloc_latin1_string_body_with_roots(&mut heap, bytes, &mut roots)
            .expect("latin1 parent");
        view = slice_string_body(&mut heap, parent, 10, 30, &mut roots).expect("slice");
        let expected = &bytes[10..40];
        heap.read_payload(view, |body| {
            assert_eq!(body.len(), 30);
            assert_eq!(body.hash, 0, "slicing never hashes");
            assert!(matches!(
                body.repr(),
                JsStringBodyRepr::Sliced {
                    parent: actual,
                    start: 10
                } if actual == parent
            ));
        });
        assert_eq!(
            string_hash(&mut heap, view),
            fold_hash(hash_latin1(expected))
        );
        assert_eq!(
            with_latin1(&heap, view, |slice| slice.to_vec()),
            Some(expected.to_vec())
        );
        let string = crate::string::JsString::from_gc_handle(&heap, view).expect("wrapper");
        assert_eq!(string.char_code_at(3, &heap), Some(u16::from(b'd')));
        assert_eq!(
            string.to_lossy_string(&heap),
            std::str::from_utf8(expected).expect("ascii")
        );
    }

    #[test]
    fn short_slice_is_an_inline_copy() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let mut parent = JsStringHandle::null();
        let mut scope = otter_gc::RootScope::new(&mut heap);
        // SAFETY: the slot precedes the scope and remains stationary.
        unsafe {
            scope.add_raw_slot((&mut parent as *mut JsStringHandle).cast::<RawGc>());
        }
        parent = alloc_latin1_string_body_with_roots(
            &mut heap,
            b"0123456789abcdefghijklmnopqrstuvwxyz",
            &mut roots,
        )
        .expect("latin1 parent");
        let short = slice_string_body(&mut heap, parent, 10, 5, &mut roots).expect("slice");
        heap.read_payload(short, |body| {
            assert!(matches!(body.repr(), JsStringBodyRepr::SeqLatin1));
        });
        assert_eq!(
            with_latin1(&heap, short, |slice| slice.to_vec()),
            Some(b"abcde".to_vec())
        );
        assert_eq!(
            peek_string_hash(&heap, short),
            fold_hash(hash_latin1(b"abcde"))
        );
    }

    #[test]
    fn slicing_a_latin1_slice_collapses_to_its_original_parent() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let mut parent = JsStringHandle::null();
        let mut outer = JsStringHandle::null();
        let mut inner = JsStringHandle::null();
        let mut scope = otter_gc::RootScope::new(&mut heap);
        // SAFETY: all three slots precede the scope and remain stationary.
        unsafe {
            scope.add_raw_slot((&mut parent as *mut JsStringHandle).cast::<RawGc>());
            scope.add_raw_slot((&mut outer as *mut JsStringHandle).cast::<RawGc>());
            scope.add_raw_slot((&mut inner as *mut JsStringHandle).cast::<RawGc>());
        }
        let bytes = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
        parent = alloc_latin1_string_body_with_roots(&mut heap, bytes, &mut roots)
            .expect("latin1 parent");
        outer = slice_string_body(&mut heap, parent, 10, 50, &mut roots).expect("outer");
        inner = slice_string_body(&mut heap, outer, 5, 30, &mut roots).expect("inner");
        heap.read_payload(inner, |body| {
            assert!(matches!(
                body.repr(),
                JsStringBodyRepr::Sliced {
                    parent: actual,
                    start: 15
                } if actual == parent
            ));
        });
        assert_eq!(
            with_latin1(&heap, inner, |slice| slice.to_vec()),
            Some(bytes[15..45].to_vec())
        );
        assert_eq!(
            string_hash(&mut heap, inner),
            fold_hash(hash_latin1(&bytes[15..45]))
        );
    }

    #[test]
    fn flatten_realises_cons_into_flat() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let mut left = JsStringHandle::null();
        let mut right = JsStringHandle::null();
        let mut cons = JsStringHandle::null();
        let mut flat = JsStringHandle::null();
        let mut scope = otter_gc::RootScope::new(&mut heap);
        // SAFETY: all slots precede the scope and remain stationary.
        unsafe {
            scope.add_raw_slot((&mut left as *mut JsStringHandle).cast::<RawGc>());
            scope.add_raw_slot((&mut right as *mut JsStringHandle).cast::<RawGc>());
            scope.add_raw_slot((&mut cons as *mut JsStringHandle).cast::<RawGc>());
            scope.add_raw_slot((&mut flat as *mut JsStringHandle).cast::<RawGc>());
        }
        left =
            alloc_flat_string_body_with_roots(&mut heap, &[b'a' as u16, b'b' as u16], &mut roots)
                .expect("left");
        right = alloc_flat_string_body_with_roots(&mut heap, &[b'c' as u16], &mut roots)
            .expect("right");
        cons = concat_string_bodies(
            &mut heap,
            left,
            right,
            otter_gc::SitePlacement::Young(0),
            &mut roots,
        )
        .expect("cons");
        flat = flatten_string_body(&mut heap, cons, &mut roots).expect("flat");
        heap.read_payload(flat, |b| {
            assert!(matches!(b.repr(), JsStringBodyRepr::SeqFlat));
            assert_eq!(b.len(), 3);
        });
    }

    #[test]
    fn equals_string_bodies_short_circuits() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let mut a = JsStringHandle::null();
        let mut b = JsStringHandle::null();
        let mut c = JsStringHandle::null();
        let mut scope = otter_gc::RootScope::new(&mut heap);
        // SAFETY: all slots precede the scope and remain stationary.
        unsafe {
            scope.add_raw_slot((&mut a as *mut JsStringHandle).cast::<RawGc>());
            scope.add_raw_slot((&mut b as *mut JsStringHandle).cast::<RawGc>());
            scope.add_raw_slot((&mut c as *mut JsStringHandle).cast::<RawGc>());
        }
        a = alloc_flat_string_body_with_roots(&mut heap, &[1, 2, 3], &mut roots).expect("a");
        b = alloc_flat_string_body_with_roots(&mut heap, &[1, 2, 3], &mut roots).expect("b");
        c = alloc_flat_string_body_with_roots(&mut heap, &[1, 2, 4], &mut roots).expect("c");
        assert!(equals_string_bodies(&heap, a, a));
        assert!(equals_string_bodies(&heap, a, b));
        assert!(!equals_string_bodies(&heap, a, c));
    }
}
