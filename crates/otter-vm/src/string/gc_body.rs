//! GC-managed JavaScript string body — unified variant-enum design
//! covering flat WTF-16, Latin-1, cons (rope), and width-preserving sliced
//! views
//! variants in a single GC body type.
//!
//! Replaces the earlier chunked-storage scaffold: every string lives
//! in exactly one [`JsStringBody`] on the GC heap. Short flat strings keep
//! their bytes/code units inside the body; longer strings use `Vec<u16>` /
//! `Vec<u8>` side storage. Cons / sliced variants reference children through
//! [`JsStringHandle`] so the collector can trace them transitively.
//!
//! # Contents
//! - [`JsStringId`] — stable intern-table identity for shape keys.
//! - [`JsStringBody`] / [`JsStringBodyRepr`] — variant-enum body.
//! - Stable representation tags and payload offsets consumed by generated
//!   code for contiguous string operations.
//! - `alloc_*` helpers and heap-level
//!   [`concat`] / [`slice`] / [`flatten`] / [`equals`] / [`to_utf16_vec`].
//!
//! # Invariants
//! - String bytes/code units live on the GC heap (Vec inside body).
//!   No `Rc` / `Arc` / `Box` / `Cell` / `RefCell` inside the body.
//! - Bodies are allocated in old-space. `JsString` is a copied handle
//!   wrapper used heavily by native builtins; keeping string bodies
//!   non-moving preserves those local handles across GC.
//! - `len` is precomputed at construction and is O(1) heap-free at
//!   the body level (callers read it via `heap.read_payload`).
//! - `hash` is the FNV-1a hash over the materialised UTF-16 code
//!   units. Cons / sliced bodies cache the hash at construction so
//!   later atom-table probes never re-walk the rope.
//! - Cons depth never exceeds [`MAX_ROPE_DEPTH`]; concatenations
//!   that would exceed it flatten the deeper child eagerly.
//! - Slicing a flat body is O(1) for either storage width. Slicing a `Cons`
//!   materialises only the requested span; slicing a `Sliced` collapses into a
//!   single view (no `Sliced(Sliced(...))`).
//! - [`JsStringBodyRepr`] uses `repr(C, u8)`: its tag byte and aligned payload
//!   union are an explicit generated-code contract. Changing either requires
//!   updating the baked JIT layout and its machine-code consumers together.
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
/// UTF-16 code units stored directly inside a flat string body.
pub const INLINE_FLAT_CAP: usize = 12;
/// Latin-1 bytes stored directly inside a Latin-1 string body.
pub const INLINE_LATIN1_CAP: usize = 24;

/// Inline UTF-16 representation tag exposed to generated code.
pub const STRING_REPR_INLINE_FLAT: u8 = 0;
/// Sequential UTF-16 representation tag exposed to generated code.
pub const STRING_REPR_SEQ_FLAT: u8 = 1;
/// Inline Latin-1 representation tag exposed to generated code.
pub const STRING_REPR_INLINE_LATIN1: u8 = 2;
/// Sequential Latin-1 representation tag exposed to generated code.
pub const STRING_REPR_SEQ_LATIN1: u8 = 3;
/// Rope representation tag; generated code must use the cold path.
pub const STRING_REPR_CONS: u8 = 4;
/// Slice representation tag; generated code must use the cold path.
pub const STRING_REPR_SLICED: u8 = 5;

/// GC handle to a JavaScript string body. `Copy`. Packs into
/// [`crate::Value`] under `TAG_PTR_STRING`.
pub type JsStringHandle = otter_gc::Gc<JsStringBody>;

/// Stable identity assigned by the VM-side string interner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JsStringId(u32);

impl JsStringId {
    /// Construct an interner-local string id.
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Raw numeric representation for diagnostics and compact side
    /// tables.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Internal representation of a [`JsStringBody`].
#[derive(Debug)]
#[repr(C, u8)]
pub enum JsStringBodyRepr {
    /// Small flat WTF-16 code units stored inside the GC body. The live prefix
    /// length is [`JsStringBody::len`].
    InlineFlat([u16; INLINE_FLAT_CAP]) = STRING_REPR_INLINE_FLAT,
    /// Flat WTF-16 code units stored in the body's own trailing storage.
    /// The live length is [`JsStringBody::len`]; the units start one body
    /// past the body, in the same GC cell.
    SeqFlat = STRING_REPR_SEQ_FLAT,
    /// Small Latin-1 code units stored inside the GC body. The live prefix
    /// length is [`JsStringBody::len`].
    InlineLatin1([u8; INLINE_LATIN1_CAP]) = STRING_REPR_INLINE_LATIN1,
    /// Latin-1 code units stored in the body's own trailing storage. Each
    /// byte zero-extends to a `u16` on read. The live length is
    /// [`JsStringBody::len`].
    SeqLatin1 = STRING_REPR_SEQ_LATIN1,
    /// Rope concatenation node. Tracing visits both children.
    Cons {
        /// Left child.
        left: JsStringHandle,
        /// Right child.
        right: JsStringHandle,
        /// Maximum depth of either child plus one. Bounded by
        /// [`MAX_ROPE_DEPTH`].
        depth: u8,
    } = STRING_REPR_CONS,
    /// Slice view over a parent string. Tracing visits the parent.
    Sliced {
        /// Parent body.
        parent: JsStringHandle,
        /// Start offset (code units) into the parent.
        start: u32,
    } = STRING_REPR_SLICED,
}

/// Byte offset from the start of [`JsStringBodyRepr`] to the payload union.
///
/// `repr(C, u8)` lays the explicit tag first and aligns the payload to the
/// enum's alignment. Generated code reads only the four contiguous variants;
/// cons and sliced bodies retain the Rust walker.
pub const STRING_REPR_PAYLOAD_BYTE: usize = std::mem::align_of::<JsStringBodyRepr>();

/// Materialising a large rope / Latin-1 body into `Vec<u16>` is O(len).
/// Subjects re-scanned many times (a `/g` regex `exec` loop re-widens the
/// same subject on every match) amortise that once via
/// [`JsStringBody::utf16_cache`]. Below this length the widen is cheap
/// enough that caching only wastes memory, so small strings stay uncached.
const UTF16_CACHE_MIN_LEN: u32 = 256;

// A cached widening is only worth taking when the subject is long, and any
// string that long allocates its units sequentially rather than inline. That
// is what lets `with_utf16` read a filled cache as `SeqFlat` directly.
const _: () = assert!(UTF16_CACHE_MIN_LEN as usize > INLINE_FLAT_CAP);

/// GC-managed JavaScript string body.
///
/// `repr(C)` because the sequential variants keep their code units in
/// trailing storage immediately after the body, in the same GC cell, and
/// that only has a defined address with a fixed layout.
#[derive(Debug)]
#[repr(C)]
pub struct JsStringBody {
    /// Stable interner identity. Defaults to `JsStringId::new(0)` for
    /// uninterned strings.
    pub id: JsStringId,
    /// Code-unit length (UTF-16). O(1) heap-free at the body level.
    pub len: u32,
    /// Stable FNV-1a hash over the materialised UTF-16 code units.
    pub hash: u64,
    /// Variant-specific payload.
    pub repr: JsStringBodyRepr,
    /// Lazily-filled widened view of this string, as a flat string body.
    /// Null until [`ensure_utf16_cache`] fills it.
    ///
    /// The cache is a string rather than a side buffer so it owns nothing
    /// outside the heap: it is traced, it moves with the collector, and a
    /// page image carries it like any other body. String content is
    /// immutable — a representation may collapse rope→flat, but the code
    /// units never change — so once filled it stays correct for the body's
    /// lifetime.
    utf16_cache: JsStringHandle,
}

const _: () = assert!(std::mem::size_of::<JsStringBodyRepr>() <= 32);
const _: () = assert!(std::mem::size_of::<JsStringBody>() <= 64);

impl JsStringBody {
    /// Stable interner identity.
    #[must_use]
    pub const fn id(&self) -> JsStringId {
        self.id
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

    /// Stable FNV-1a hash over the string's UTF-16 code units.
    #[must_use]
    pub const fn hash(&self) -> u64 {
        self.hash
    }

    /// Trailing bytes a sequential body of `len` code units needs.
    #[must_use]
    pub fn trailing_bytes(latin1: bool, len: usize) -> usize {
        if latin1 { len } else { len * 2 }
    }

    /// Base of the trailing code units. Only meaningful for
    /// [`JsStringBodyRepr::SeqFlat`] / [`JsStringBodyRepr::SeqLatin1`].
    fn trailing_ptr(&self) -> *const u8 {
        // SAFETY: a sequential body was allocated with
        // `trailing_bytes(..)` reserved immediately after it, so the
        // units start one `Self` past `self`.
        unsafe { (self as *const Self as *const u8).add(std::mem::size_of::<Self>()) }
    }

    /// The WTF-16 units of a [`JsStringBodyRepr::SeqFlat`] body.
    #[must_use]
    pub fn seq_flat_units(&self) -> &[u16] {
        debug_assert!(matches!(self.repr, JsStringBodyRepr::SeqFlat));
        // SAFETY: the trailing array holds exactly `len` `u16`s, written
        // at allocation and immutable thereafter.
        unsafe { std::slice::from_raw_parts(self.trailing_ptr().cast::<u16>(), self.len as usize) }
    }

    /// The Latin-1 bytes of a [`JsStringBodyRepr::SeqLatin1`] body.
    #[must_use]
    pub fn seq_latin1_bytes(&self) -> &[u8] {
        debug_assert!(matches!(self.repr, JsStringBodyRepr::SeqLatin1));
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
        match &self.repr {
            JsStringBodyRepr::Cons { depth, .. } => *depth,
            _ => 0,
        }
    }
}

impl otter_gc::SafeTraceable for JsStringBody {
    const TYPE_TAG: u8 = JS_STRING_BODY_TYPE_TAG;

    fn trace_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>) {
        // The widened cache is an ordinary string body, so it is traced like
        // any other outgoing handle regardless of this body's variant.
        if !self.utf16_cache.is_null() {
            let p = &mut self.utf16_cache as *mut JsStringHandle as *mut RawGc;
            visitor(p);
        }
        match &mut self.repr {
            JsStringBodyRepr::InlineFlat(_)
            | JsStringBodyRepr::SeqFlat
            | JsStringBodyRepr::InlineLatin1(_)
            | JsStringBodyRepr::SeqLatin1 => {}
            JsStringBodyRepr::Cons { left, right, .. } => {
                if !left.is_null() {
                    let p = left as *mut JsStringHandle as *mut RawGc;
                    visitor(p);
                }
                if !right.is_null() {
                    let p = right as *mut JsStringHandle as *mut RawGc;
                    visitor(p);
                }
            }
            JsStringBodyRepr::Sliced { parent, .. } => {
                if !parent.is_null() {
                    let p = parent as *mut JsStringHandle as *mut RawGc;
                    visitor(p);
                }
            }
        }
    }
}

/// Allocate a flat WTF-16 string body.
///
/// # Errors
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
pub fn alloc_flat_string_body_with_roots(
    heap: &mut GcHeap,
    id: JsStringId,
    units: &[u16],
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsStringHandle, otter_gc::OutOfMemory> {
    let len = units.len() as u32;
    let hash = hash_utf16(units);
    if units.len() <= INLINE_FLAT_CAP {
        let mut inline = [0u16; INLINE_FLAT_CAP];
        inline[..units.len()].copy_from_slice(units);
        return heap.alloc_old_with_roots(
            JsStringBody {
                id,
                len,
                hash,
                repr: JsStringBodyRepr::InlineFlat(inline),
                utf16_cache: JsStringHandle::null(),
            },
            external_visit,
        );
    }
    // The code units live in the same cell as the body, so they are part of
    // the GC allocation and need no separate cap reservation. Old space,
    // like every other string body: see the module invariant.
    let handle = heap.alloc_variable_with_roots(
        JsStringBody {
            id,
            len,
            hash,
            repr: JsStringBodyRepr::SeqFlat,
            utf16_cache: JsStringHandle::null(),
        },
        JsStringBody::trailing_bytes(false, units.len()),
        external_visit,
    )?;
    heap.with_payload(handle, |body| {
        body.init_seq_flat(units);
        true
    });
    Ok(handle)
}

/// Allocate a Latin-1 string body.
///
/// # Errors
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
pub fn alloc_latin1_string_body_with_roots(
    heap: &mut GcHeap,
    id: JsStringId,
    bytes: &[u8],
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsStringHandle, otter_gc::OutOfMemory> {
    let len = bytes.len() as u32;
    let hash = hash_latin1(bytes);
    if bytes.len() <= INLINE_LATIN1_CAP {
        let mut inline = [0u8; INLINE_LATIN1_CAP];
        inline[..bytes.len()].copy_from_slice(bytes);
        return heap.alloc_old_with_roots(
            JsStringBody {
                id,
                len,
                hash,
                repr: JsStringBodyRepr::InlineLatin1(inline),
                utf16_cache: JsStringHandle::null(),
            },
            external_visit,
        );
    }
    // Same as the flat path: the bytes are inside the GC allocation, and
    // the body is old-space like every other string body.
    let handle = heap.alloc_variable_with_roots(
        JsStringBody {
            id,
            len,
            hash,
            repr: JsStringBodyRepr::SeqLatin1,
            utf16_cache: JsStringHandle::null(),
        },
        JsStringBody::trailing_bytes(true, bytes.len()),
        external_visit,
    )?;
    heap.with_payload(handle, |body| {
        body.init_seq_latin1(bytes);
        true
    });
    Ok(handle)
}

/// Concatenate two GC string bodies into a `Cons` rope node.
///
/// Cheap: bounded by the depth-bound check plus one allocation. If
/// the resulting depth would exceed [`MAX_ROPE_DEPTH`], the deeper
/// child is flattened first.
///
/// # Errors
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
pub fn concat_string_bodies(
    heap: &mut GcHeap,
    left: JsStringHandle,
    right: JsStringHandle,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsStringHandle, otter_gc::OutOfMemory> {
    let (left_len, left_depth, left_hash) = heap.read_payload(left, |b| (b.len, b.depth(), b.hash));
    let (right_len, right_depth, right_hash) =
        heap.read_payload(right, |b| (b.len, b.depth(), b.hash));

    if right_len == 0 {
        return Ok(left);
    }
    if left_len == 0 {
        return Ok(right);
    }

    let new_len = left_len.saturating_add(right_len);

    // Short-result fast path: when the concatenation fits an inline flat body
    // and both sides are already materialised (non-cons/non-sliced), build the
    // flat result directly. A `Cons` node for a tiny result (`"k" + i` keys,
    // small id/label joins) is pure overhead — it retains both children and
    // forces a rope walk on every later hash/compare and an eventual flatten.
    // The inline-flat result needs the same single body allocation but stores
    // its bytes inline and is read in O(1).
    if (new_len as usize) <= INLINE_LATIN1_CAP {
        let mut units = [0u16; INLINE_LATIN1_CAP];
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
        if both_flat {
            return if all_latin1 {
                let mut bytes = [0u8; INLINE_LATIN1_CAP];
                for (dst, &unit) in bytes.iter_mut().zip(units[..n].iter()) {
                    *dst = unit as u8;
                }
                alloc_latin1_string_body_with_roots(
                    heap,
                    JsStringId::new(0),
                    &bytes[..n],
                    external_visit,
                )
            } else {
                alloc_flat_string_body_with_roots(
                    heap,
                    JsStringId::new(0),
                    &units[..n],
                    external_visit,
                )
            };
        }
    }

    let projected_depth = left_depth.max(right_depth).saturating_add(1);

    // Flatten deeper side eagerly if we'd exceed the depth budget.
    let (left, right, left_depth, right_depth) = if projected_depth > MAX_ROPE_DEPTH {
        if left_depth >= right_depth {
            let flat = flatten_string_body(heap, left, external_visit)?;
            (flat, right, 0u8, right_depth)
        } else {
            let flat = flatten_string_body(heap, right, external_visit)?;
            (left, flat, left_depth, 0u8)
        }
    } else {
        (left, right, left_depth, right_depth)
    };

    let final_depth = left_depth.max(right_depth).saturating_add(1);

    // Compose hashes by re-hashing left bytes then right bytes
    // through FNV-1a; cheaper than walking the materialised rope on
    // every later equality probe. This matches `hash_utf16(left ++
    // right)` because FNV-1a is a streaming hash.
    let combined_hash = fnv_combine(left_hash, right_hash, right_len as usize);

    heap.alloc_old_with_roots(
        JsStringBody {
            id: JsStringId::new(0),
            len: new_len,
            hash: combined_hash,
            repr: JsStringBodyRepr::Cons {
                left,
                right,
                depth: final_depth,
            },
            utf16_cache: JsStringHandle::null(),
        },
        external_visit,
    )
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
        return alloc_flat_string_body_with_roots(heap, JsStringId::new(0), &[], external_visit);
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
    let src = heap.read_payload(string, |b| match &b.repr {
        JsStringBodyRepr::InlineFlat(_)
        | JsStringBodyRepr::SeqFlat
        | JsStringBodyRepr::InlineLatin1(_)
        | JsStringBodyRepr::SeqLatin1 => SliceSource::Flat,
        JsStringBodyRepr::Sliced {
            parent,
            start: pstart,
        } => SliceSource::SlicedCollapse {
            parent: *parent,
            abs_start: pstart + start,
        },
        JsStringBodyRepr::Cons { .. } => SliceSource::Cons,
    });
    match src {
        SliceSource::Flat => {
            // Hash over the sliced units, computed before the alloc
            // so the body lands with `hash` already populated.
            let hash = heap.read_payload(string, |b| {
                match flat_content_range(b, start, length)
                    .expect("flat source has contiguous content")
                {
                    FlatContent::Latin1(bytes) => hash_latin1(bytes),
                    FlatContent::Wide(units) => hash_utf16(units),
                }
            });
            heap.alloc_old_with_roots(
                JsStringBody {
                    id: JsStringId::new(0),
                    len: length,
                    hash,
                    repr: JsStringBodyRepr::Sliced {
                        parent: string,
                        start,
                    },
                    utf16_cache: JsStringHandle::null(),
                },
                external_visit,
            )
        }
        SliceSource::SlicedCollapse { parent, abs_start } => {
            // Compose into a single Sliced view over the original
            // parent; never produce Sliced(Sliced(...)).
            // A collapsed slice always points directly at a contiguous parent,
            // so hash that range in place. Materialising UTF-16 here would
            // quietly turn repeated zero-copy slices into one allocation and
            // one payload copy per field.
            let hash = heap.read_payload(parent, |body| {
                match flat_content_range(body, abs_start, length)
                    .expect("collapsed slice parent has contiguous content")
                {
                    FlatContent::Latin1(bytes) => hash_latin1(bytes),
                    FlatContent::Wide(units) => hash_utf16(units),
                }
            });
            heap.alloc_old_with_roots(
                JsStringBody {
                    id: JsStringId::new(0),
                    len: length,
                    hash,
                    repr: JsStringBodyRepr::Sliced {
                        parent,
                        start: abs_start,
                    },
                    utf16_cache: JsStringHandle::null(),
                },
                external_visit,
            )
        }
        SliceSource::Cons => {
            // Avoid flattening the whole rope for small substrings. Parsers
            // commonly slice thousands of short fields out of one large
            // concatenated input string; materialising the full source for
            // each field turns that workload into quadratic heap pressure.
            let units = to_utf16_vec_slice(heap, string, start, length);
            alloc_flat_string_body_with_roots(heap, JsStringId::new(0), &units, external_visit)
        }
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
    let is_flat = heap.read_payload(string, |b| {
        matches!(
            b.repr,
            JsStringBodyRepr::InlineFlat(_) | JsStringBodyRepr::SeqFlat
        )
    });
    if is_flat {
        return Ok(string);
    }
    let units = to_utf16_vec(heap, string);
    alloc_flat_string_body_with_roots(heap, JsStringId::new(0), &units, external_visit)
}

/// Flatten a cons rope / slice view **in place**: materialize its contents once
/// and rewrite the body's `repr` to a flat body (Latin-1 when every unit fits in
/// a byte, else WTF-16). A no-op for already-flat bodies.
///
/// This is the rope-flattening that production engines (V8, JSC) perform on
/// first content access: a string built incrementally (`s += chunk`) is a rope,
/// and every scan over it (`indexOf`, `includes`, `split`, …) would otherwise
/// re-walk and re-materialize the whole rope. Flattening the shared body once
/// makes every later access — and every later call on the same handle — hit the
/// O(1) flat / Latin-1 fast paths with no allocation. The body keeps its `id`,
/// `len`, and `hash` (content is unchanged), so every existing handle and
/// interner entry stays valid; the old child handles simply become unreachable.
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
    // widen itself in place. Allocate the flat string and turn the rope node
    // into a view over it: same length, same hash, same identity, and every
    // later access takes the flat fast path through one hop. Short results
    // still collapse straight into the body's inline array.
    if latin1 {
        let bytes: Vec<u8> = units.iter().map(|&u| u as u8).collect();
        if bytes.len() <= INLINE_LATIN1_CAP {
            let mut inline = [0u8; INLINE_LATIN1_CAP];
            inline[..bytes.len()].copy_from_slice(&bytes);
            heap.with_payload(rooted, |b| {
                b.repr = JsStringBodyRepr::InlineLatin1(inline);
                true
            });
            return Ok(());
        }
        let flat =
            alloc_latin1_string_body_with_roots(heap, JsStringId::new(0), &bytes, &mut visit)?;
        heap.with_payload(rooted, |b| {
            b.repr = JsStringBodyRepr::Sliced {
                parent: flat,
                start: 0,
            };
            true
        });
        heap.record_write(rooted, &flat);
        return Ok(());
    }
    if units.len() <= INLINE_FLAT_CAP {
        let mut inline = [0u16; INLINE_FLAT_CAP];
        inline[..units.len()].copy_from_slice(&units);
        heap.with_payload(rooted, |b| {
            b.repr = JsStringBodyRepr::InlineFlat(inline);
            true
        });
        return Ok(());
    }
    let flat = alloc_flat_string_body_with_roots(heap, JsStringId::new(0), &units, &mut visit)?;
    heap.with_payload(rooted, |b| {
        b.repr = JsStringBodyRepr::Sliced {
            parent: flat,
            start: 0,
        };
        true
    });
    heap.record_write(rooted, &flat);
    Ok(())
}

/// Compare a JS string against a UTF-8 `&str` for code-unit equality
/// **without allocating** for the common single-segment case.
///
/// Property keys are overwhelmingly short, interned, single-segment
/// `Latin1` or `Flat` bodies, so the hot path (shape-key validation in
/// the property IC) compares in place against `key.encode_utf16()`.
/// Only the rare `Cons` / `Sliced` rope shapes fall back to the
/// allocating [`to_utf16_vec`] materialiser.
#[must_use]
pub fn eq_str(heap: &GcHeap, string: JsStringHandle, key: &str) -> bool {
    enum Fast {
        Mismatch,
        Match,
        Rope,
    }
    let fast = heap.read_payload(string, |b| match &b.repr {
        JsStringBodyRepr::InlineLatin1(bytes) => {
            let mut units = key.encode_utf16();
            for &byte in &bytes[..b.len as usize] {
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
        JsStringBodyRepr::SeqLatin1 => {
            let bytes = b.seq_latin1_bytes();
            // Latin-1 byte values are Unicode scalar values 0..=255,
            // so each zero-extends straight to a UTF-16 code unit.
            let mut units = key.encode_utf16();
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
        JsStringBodyRepr::InlineFlat(code_units) => {
            let mut units = key.encode_utf16();
            for &unit in &code_units[..b.len as usize] {
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
        JsStringBodyRepr::SeqFlat => {
            let code_units = b.seq_flat_units();
            let mut units = key.encode_utf16();
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
                    Some(FlatContent::Latin1(bytes)) => {
                        bytes.iter().map(|&x| u16::from(x)).eq(key.encode_utf16())
                    }
                    Some(FlatContent::Wide(units)) => units.iter().copied().eq(key.encode_utf16()),
                    None => unreachable!("resolved view stores its units contiguously"),
                })
            }
            None => to_utf16_vec(heap, string)
                .into_iter()
                .eq(key.encode_utf16()),
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

/// Whether the string's code units can be borrowed through a direct body or a
/// collapsed slice view, without materialising a rope.
#[must_use]
pub fn is_contiguous(heap: &GcHeap, string: JsStringHandle) -> bool {
    contiguous_view(heap, string).is_some()
}

/// Resolve `string` to `(body holding the units, start, len)` when its code
/// units can be read in place, or `None` when they must be materialised.
///
/// Three shapes qualify. The body stores its own units contiguously. It is a
/// slice view over one — which is what a flattened rope becomes, so the view
/// is a range of the parent's contiguous units and needs no copy either. Or
/// it carries a filled widened cache.
///
/// Missing the slice case is expensive in a specific way: since flattening
/// rewrites a rope into a view, *every* read of an already-flattened rope
/// would re-materialise the whole string, turning a `/g` regex loop over a
/// long subject quadratic.
fn contiguous_view(heap: &GcHeap, string: JsStringHandle) -> Option<(JsStringHandle, u32, u32)> {
    let (own, len, parent, cache) = heap.read_payload(string, |b| {
        let parent = match &b.repr {
            JsStringBodyRepr::Sliced { parent, start } => Some((*parent, *start)),
            _ => None,
        };
        (stores_units_inline(&b.repr), b.len, parent, b.utf16_cache)
    });
    if own {
        return Some((string, 0, len));
    }
    if let Some((parent, start)) = parent
        && heap.read_payload(parent, |p| stores_units_inline(&p.repr))
    {
        return Some((parent, start, len));
    }
    (!cache.is_null()).then_some((cache, 0, len))
}

/// Whether a body holds its own code units contiguously.
fn stores_units_inline(repr: &JsStringBodyRepr) -> bool {
    matches!(
        repr,
        JsStringBodyRepr::InlineFlat(_)
            | JsStringBodyRepr::SeqFlat
            | JsStringBodyRepr::InlineLatin1(_)
            | JsStringBodyRepr::SeqLatin1
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

/// Fill `string`'s widened cache so later [`with_utf16`] reads are in-place.
///
/// Materialising a large rope or Latin-1 body into UTF-16 is O(len), and a
/// subject re-scanned many times — a `/g` regex `exec` loop walks the same
/// subject once per match — should pay that once. Callers that are about to
/// re-scan a subject call this first, while they still hold `&mut GcHeap`;
/// [`with_utf16`] itself stays a read, because widening is an allocation and
/// its call sites hold the heap shared.
///
/// A no-op for short subjects, for bodies that already store their units
/// contiguously, and for an already-filled cache.
///
/// # Errors
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
pub fn ensure_utf16_cache(
    heap: &mut GcHeap,
    string: JsStringHandle,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    // Only a body whose wide units cannot already be read in place is worth
    // caching, and only when it is long enough for the walk to matter.
    let long_enough = heap.read_payload(string, |b| {
        b.len >= UTF16_CACHE_MIN_LEN && b.utf16_cache.is_null()
    });
    let already_wide = contiguous_view(heap, string).is_some_and(|(target, start, len)| {
        heap.read_payload(target, |b| {
            matches!(
                flat_content_range(b, start, len),
                Some(FlatContent::Wide(_))
            )
        })
    });
    if !long_enough || already_wide {
        return Ok(());
    }
    let units = materialize_utf16_vec(heap, string);
    // Allocating the cache can collect, so the subject travels as a root and
    // the handle we install into is the forwarded one.
    let mut rooted = string;
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        let p = &mut rooted as *mut JsStringHandle as *mut RawGc;
        visitor(p);
        external_visit(visitor);
    };
    let widened = alloc_flat_string_body_with_roots(heap, JsStringId::new(0), &units, &mut visit)?;
    heap.with_payload(rooted, |b| {
        b.utf16_cache = widened;
        true
    });
    heap.record_write(rooted, &widened);
    Ok(())
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
        let resolved = heap.read_payload(node, |b| match &b.repr {
            JsStringBodyRepr::InlineFlat(units) => {
                let live = &units[..b.len as usize];
                let s = (start as usize).min(live.len());
                let e = s.saturating_add(length as usize).min(live.len());
                out.extend_from_slice(&live[s..e]);
                Resolved::Flat
            }
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
            JsStringBodyRepr::InlineLatin1(bytes) => {
                let live = &bytes[..b.len as usize];
                let s = (start as usize).min(live.len());
                let e = s.saturating_add(length as usize).min(live.len());
                out.extend(live[s..e].iter().map(|&b| u16::from(b)));
                Resolved::Latin1
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
                parent: *parent,
                abs_start: pstart + start,
            },
            JsStringBodyRepr::Cons { left, right, .. } => Resolved::Cons {
                left: *left,
                right: *right,
                left_len: heap.read_payload(*left, |lb| lb.len),
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
        let resolved = heap.read_payload(node, |b| match &b.repr {
            JsStringBodyRepr::InlineFlat(units) => {
                let units = &units[..b.len as usize];
                let lo = s as usize;
                let hi = lo + l as usize;
                out.extend_from_slice(&units[lo..hi]);
                Resolved::Done
            }
            JsStringBodyRepr::SeqFlat => {
                let units = b.seq_flat_units();
                let lo = s as usize;
                let hi = lo + l as usize;
                out.extend_from_slice(&units[lo..hi]);
                Resolved::Done
            }
            JsStringBodyRepr::InlineLatin1(bytes) => {
                let bytes = &bytes[..b.len as usize];
                let lo = s as usize;
                let hi = lo + l as usize;
                out.extend(bytes[lo..hi].iter().map(|&b| u16::from(b)));
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
                parent: *parent,
                abs_start: pstart + s,
            },
            JsStringBodyRepr::Cons { left, right, .. } => Resolved::Cons {
                left: *left,
                right: *right,
                left_len: heap.read_payload(*left, |lb| lb.len),
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
    let len = body.len as usize;
    match &body.repr {
        JsStringBodyRepr::InlineLatin1(buf) => Some(FlatContent::Latin1(&buf[..len])),
        JsStringBodyRepr::SeqLatin1 => Some(FlatContent::Latin1(body.seq_latin1_bytes())),
        JsStringBodyRepr::InlineFlat(buf) => Some(FlatContent::Wide(&buf[..len])),
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
    // One nested read over both bodies handles the whole flat (inline / side
    // storage) case — the dominant Map/Set-key shape — in a single pass: length
    // and (non-cons) hash reject, then a direct content compare that avoids two
    // throwaway `to_utf16_vec` allocations. `body.hash` matches the FNV-1a of the
    // flattened content only when neither side is a cons rope (cons bodies carry
    // a placeholder hash), so the hash reject is gated on both being non-cons. A
    // `Cons` / `Sliced` body yields `None` and falls through to the materialising
    // walk. This reads each body's payload once instead of twice.
    if let Some(answer) = heap.read_payload(a, |ba| {
        heap.read_payload(b, |bb| {
            if ba.len != bb.len {
                return Some(false);
            }
            if ba.len == 0 {
                return Some(true);
            }
            let a_is_cons = matches!(ba.repr, JsStringBodyRepr::Cons { .. });
            let b_is_cons = matches!(bb.repr, JsStringBodyRepr::Cons { .. });
            if !a_is_cons && !b_is_cons && ba.hash != bb.hash {
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
    to_utf16_vec(heap, a) == to_utf16_vec(heap, b)
}

/// Lexicographic code-unit ordering of two string bodies. Both sides flat
/// (inline or side storage, either width) compare in place; a `Cons` /
/// `Sliced` operand falls back to materialising both sides.
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
    to_utf16_vec(heap, a).cmp(&to_utf16_vec(heap, b))
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

/// FNV-1a hash over UTF-16 code units. Stable across runs; used for
/// atom-table probes and `Cons` hash composition.
#[must_use]
pub fn hash_utf16(units: &[u16]) -> u64 {
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

/// FNV-1a hash over Latin-1 bytes after zero-extension to `u16`
/// little-endian. Matches `hash_utf16(&units)` where `units[i] =
/// bytes[i] as u16`.
#[must_use]
pub fn hash_latin1(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET;
    for &byte in bytes {
        // Each Latin-1 byte → `[byte, 0]` LE bytes for the
        // corresponding `u16`.
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
        // The zero high-byte.
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Combine two prefix FNV-1a hashes into the hash of their
/// concatenation, given the right side's UTF-16 code-unit count.
/// Because FNV-1a is a streaming hash with the recurrence
/// `H(s ++ t) = H(s)` re-fed with `t`'s bytes, we cannot recompose
/// from the two hashes alone — this helper re-folds `right_hash`'s
/// transformation under the assumption that `left_hash` already
/// observed all of `left`. The right hash bytes are unknown here,
/// so callers re-hash the concatenated stream when an exact answer
/// is needed; this combinator is a placeholder until atom-table
/// rehashing replaces it.
#[must_use]
fn fnv_combine(left_hash: u64, right_hash: u64, _right_len_units: usize) -> u64 {
    // Treat the cons hash as `(left ^ right)` shifted — not the
    // true FNV-1a of the concatenation, but stable and
    // sufficient as a probe key while the lazy rope keeps cons
    // bodies addressable. Atom tables rehash on demand via
    // `to_utf16_vec` + `hash_utf16` when an exact match is
    // required.
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    left_hash.wrapping_mul(FNV_PRIME) ^ right_hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_roots(_v: &mut dyn FnMut(*mut RawGc)) {}

    #[test]
    fn contiguous_repr_tags_match_generated_code_contract() {
        fn tag(repr: &JsStringBodyRepr) -> u8 {
            // SAFETY: `JsStringBodyRepr` is declared `repr(C, u8)`, which
            // stores its discriminant as the first byte for every variant.
            unsafe { std::ptr::from_ref(repr).cast::<u8>().read() }
        }

        assert_eq!(
            tag(&JsStringBodyRepr::InlineFlat([0; INLINE_FLAT_CAP])),
            STRING_REPR_INLINE_FLAT
        );
        assert_eq!(tag(&JsStringBodyRepr::SeqFlat), STRING_REPR_SEQ_FLAT);
        assert_eq!(
            tag(&JsStringBodyRepr::InlineLatin1([0; INLINE_LATIN1_CAP])),
            STRING_REPR_INLINE_LATIN1
        );
        assert_eq!(tag(&JsStringBodyRepr::SeqLatin1), STRING_REPR_SEQ_LATIN1);
        assert_eq!(
            STRING_REPR_PAYLOAD_BYTE,
            std::mem::align_of::<JsStringBodyRepr>()
        );
    }

    #[test]
    fn allocates_empty_flat_string() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let s = alloc_flat_string_body_with_roots(&mut heap, JsStringId::new(1), &[], &mut roots)
            .expect("flat");
        heap.read_payload(s, |b| {
            assert_eq!(b.id().get(), 1);
            assert_eq!(b.len(), 0);
            assert!(b.is_empty());
            assert!(matches!(b.repr, JsStringBodyRepr::InlineFlat(_)));
        });
        assert!(to_utf16_vec(&heap, s).is_empty());
    }

    #[test]
    fn allocates_long_flat_string() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let units: Vec<u16> = (0..200).map(|i| (i % 0xd7ff) as u16).collect();
        let s =
            alloc_flat_string_body_with_roots(&mut heap, JsStringId::new(7), &units, &mut roots)
                .expect("flat");
        heap.read_payload(s, |b| {
            assert_eq!(b.len(), units.len() as u32);
            assert_eq!(b.hash(), hash_utf16(&units));
            assert!(matches!(b.repr, JsStringBodyRepr::SeqFlat));
        });
        assert_eq!(to_utf16_vec(&heap, s), units);
    }

    #[test]
    fn cons_concat_round_trips_to_utf16() {
        // Results within the inline-latin1 cap flatten in place; build operands
        // long enough that the concatenation stays an unflattened `Cons` rope.
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let left_units: Vec<u16> = std::iter::repeat_n(b'a' as u16, 16).collect();
        let right_units: Vec<u16> = std::iter::repeat_n(b'b' as u16, 16).collect();
        let left = alloc_flat_string_body_with_roots(
            &mut heap,
            JsStringId::new(0),
            &left_units,
            &mut roots,
        )
        .expect("left");
        let right = alloc_flat_string_body_with_roots(
            &mut heap,
            JsStringId::new(0),
            &right_units,
            &mut roots,
        )
        .expect("right");
        let cons = concat_string_bodies(&mut heap, left, right, &mut roots).expect("cons");
        heap.read_payload(cons, |b| {
            assert_eq!(b.len(), 32);
            assert!(matches!(b.repr, JsStringBodyRepr::Cons { .. }));
        });
        let mut expected = left_units.clone();
        expected.extend_from_slice(&right_units);
        assert_eq!(to_utf16_vec(&heap, cons), expected);
    }

    #[test]
    fn sliced_view_round_trips() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let units: Vec<u16> = b"hello world".iter().map(|&b| b as u16).collect();
        let flat =
            alloc_flat_string_body_with_roots(&mut heap, JsStringId::new(0), &units, &mut roots)
                .expect("flat");
        let view = slice_string_body(&mut heap, flat, 6, 5, &mut roots).expect("slice");
        heap.read_payload(view, |b| {
            assert_eq!(b.len(), 5);
            assert!(matches!(b.repr, JsStringBodyRepr::Sliced { .. }));
        });
        let world: Vec<u16> = b"world".iter().map(|&b| b as u16).collect();
        assert_eq!(to_utf16_vec(&heap, view), world);
    }

    #[test]
    fn latin1_body_round_trips() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let s = alloc_latin1_string_body_with_roots(
            &mut heap,
            JsStringId::new(0),
            b"hello",
            &mut roots,
        )
        .expect("latin1");
        heap.read_payload(s, |b| {
            assert_eq!(b.len(), 5);
            assert!(matches!(b.repr, JsStringBodyRepr::InlineLatin1(_)));
        });
        let expected: Vec<u16> = b"hello".iter().map(|&b| b as u16).collect();
        assert_eq!(to_utf16_vec(&heap, s), expected);
    }

    #[test]
    fn latin1_slice_is_a_width_preserving_view() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let bytes = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let parent =
            alloc_latin1_string_body_with_roots(&mut heap, JsStringId::new(0), bytes, &mut roots)
                .expect("latin1 parent");
        let view = slice_string_body(&mut heap, parent, 10, 10, &mut roots).expect("slice");
        heap.read_payload(view, |body| {
            assert_eq!(body.len(), 10);
            assert_eq!(body.hash(), hash_latin1(b"abcdefghij"));
            assert!(matches!(
                body.repr,
                JsStringBodyRepr::Sliced {
                    parent: actual,
                    start: 10
                } if actual == parent
            ));
        });
        assert_eq!(
            with_latin1(&heap, view, |slice| slice.to_vec()),
            Some(b"abcdefghij".to_vec())
        );
        assert_eq!(
            to_utf16_vec(&heap, view),
            b"abcdefghij"
                .iter()
                .map(|&byte| u16::from(byte))
                .collect::<Vec<_>>()
        );
        let string = crate::string::JsString::from_gc_handle(&heap, view).expect("wrapper");
        assert_eq!(string.char_code_at(3, &heap), Some(u16::from(b'd')));
        assert_eq!(string.to_lossy_string(&heap), "abcdefghij");
    }

    #[test]
    fn slicing_a_latin1_slice_collapses_to_its_original_parent() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let parent = alloc_latin1_string_body_with_roots(
            &mut heap,
            JsStringId::new(0),
            b"0123456789abcdefghijklmnopqrstuvwxyz",
            &mut roots,
        )
        .expect("latin1 parent");
        let outer = slice_string_body(&mut heap, parent, 10, 20, &mut roots).expect("outer");
        let inner = slice_string_body(&mut heap, outer, 5, 5, &mut roots).expect("inner");
        heap.read_payload(inner, |body| {
            assert!(matches!(
                body.repr,
                JsStringBodyRepr::Sliced {
                    parent: actual,
                    start: 15
                } if actual == parent
            ));
        });
        assert_eq!(
            with_latin1(&heap, inner, |slice| slice.to_vec()),
            Some(b"fghij".to_vec())
        );
        assert_eq!(
            heap.read_payload(inner, JsStringBody::hash),
            hash_latin1(b"fghij")
        );
    }

    #[test]
    fn flatten_realises_cons_into_flat() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let left = alloc_flat_string_body_with_roots(
            &mut heap,
            JsStringId::new(0),
            &[b'a' as u16, b'b' as u16],
            &mut roots,
        )
        .expect("left");
        let right = alloc_flat_string_body_with_roots(
            &mut heap,
            JsStringId::new(0),
            &[b'c' as u16],
            &mut roots,
        )
        .expect("right");
        let cons = concat_string_bodies(&mut heap, left, right, &mut roots).expect("cons");
        let flat = flatten_string_body(&mut heap, cons, &mut roots).expect("flat");
        heap.read_payload(flat, |b| {
            assert!(matches!(b.repr, JsStringBodyRepr::InlineFlat(_)));
            assert_eq!(b.len(), 3);
        });
    }

    #[test]
    fn equals_string_bodies_short_circuits() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = empty_roots;
        let a = alloc_flat_string_body_with_roots(
            &mut heap,
            JsStringId::new(0),
            &[1, 2, 3],
            &mut roots,
        )
        .expect("a");
        let b = alloc_flat_string_body_with_roots(
            &mut heap,
            JsStringId::new(0),
            &[1, 2, 3],
            &mut roots,
        )
        .expect("b");
        let c = alloc_flat_string_body_with_roots(
            &mut heap,
            JsStringId::new(0),
            &[1, 2, 4],
            &mut roots,
        )
        .expect("c");
        assert!(equals_string_bodies(&heap, a, a));
        assert!(equals_string_bodies(&heap, a, b));
        assert!(!equals_string_bodies(&heap, a, c));
    }
}
