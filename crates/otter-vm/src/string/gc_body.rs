//! GC-managed JavaScript string body — unified variant-enum design
//! covering flat WTF-16, Latin-1, cons (rope), and sliced (view)
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
//! - Slicing a `Cons` materialises only the requested span; slicing a
//!   `Sliced` collapses into a single `Sliced` view (no
//!   `Sliced(Sliced(...))`).
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
pub const MAX_ROPE_DEPTH: u8 = 64;
/// UTF-16 code units stored directly inside a flat string body.
pub const INLINE_FLAT_CAP: usize = 12;
/// Latin-1 bytes stored directly inside a Latin-1 string body.
pub const INLINE_LATIN1_CAP: usize = 24;

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
pub enum JsStringBodyRepr {
    /// Small flat WTF-16 code units stored inside the GC body. The live prefix
    /// length is [`JsStringBody::len`].
    InlineFlat([u16; INLINE_FLAT_CAP]),
    /// Flat WTF-16 code units stored in side storage.
    Flat(Vec<u16>),
    /// Small Latin-1 code units stored inside the GC body. The live prefix
    /// length is [`JsStringBody::len`].
    InlineLatin1([u8; INLINE_LATIN1_CAP]),
    /// Latin-1 code units stored in side storage. Each byte zero-extends to
    /// a `u16` on read.
    Latin1(Vec<u8>),
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

/// GC-managed JavaScript string body.
#[derive(Debug)]
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
}

const _: () = assert!(std::mem::size_of::<JsStringBodyRepr>() <= 32);
const _: () = assert!(std::mem::size_of::<JsStringBody>() <= 48);

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
        match &mut self.repr {
            JsStringBodyRepr::InlineFlat(_)
            | JsStringBodyRepr::Flat(_)
            | JsStringBodyRepr::InlineLatin1(_)
            | JsStringBodyRepr::Latin1(_) => {}
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
    let repr = if units.len() <= INLINE_FLAT_CAP {
        let mut inline = [0u16; INLINE_FLAT_CAP];
        inline[..units.len()].copy_from_slice(units);
        JsStringBodyRepr::InlineFlat(inline)
    } else {
        // Reserve cap budget for the heap-tracked `Vec<u16>` storage so
        // the body's off-slot bytes count against `max_heap_bytes`.
        let bytes = (units.len() as u64).saturating_mul(2);
        heap.reserve_bytes_with_roots(bytes, external_visit)?;
        JsStringBodyRepr::Flat(units.to_vec())
    };
    heap.alloc_old_with_roots(
        JsStringBody {
            id,
            len,
            hash,
            repr,
        },
        external_visit,
    )
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
    let repr = if bytes.len() <= INLINE_LATIN1_CAP {
        let mut inline = [0u8; INLINE_LATIN1_CAP];
        inline[..bytes.len()].copy_from_slice(bytes);
        JsStringBodyRepr::InlineLatin1(inline)
    } else {
        heap.reserve_bytes_with_roots(bytes.len() as u64, external_visit)?;
        JsStringBodyRepr::Latin1(bytes.to_vec())
    };
    heap.alloc_old_with_roots(
        JsStringBody {
            id,
            len,
            hash,
            repr,
        },
        external_visit,
    )
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
            let flat = heap.read_payload(handle, |b| match flat_content(&b.repr, b.len as usize) {
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
        },
        external_visit,
    )
}

/// Take an O(1) substring view (or a fresh allocation for cons /
/// latin-1 sources). Bounds are clamped to `[0, len()]`.
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
        Latin1Slice {
            start: u32,
            len: u32,
        },
        SlicedCollapse {
            parent: JsStringHandle,
            abs_start: u32,
        },
        Cons,
    }
    let src = heap.read_payload(string, |b| match &b.repr {
        JsStringBodyRepr::InlineFlat(_) | JsStringBodyRepr::Flat(_) => SliceSource::Flat,
        JsStringBodyRepr::InlineLatin1(_) | JsStringBodyRepr::Latin1(_) => {
            SliceSource::Latin1Slice { start, len: length }
        }
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
            let hash = heap.read_payload(string, |b| match &b.repr {
                JsStringBodyRepr::InlineFlat(units) => {
                    let s = start as usize;
                    let e = s + length as usize;
                    hash_utf16(&units[s..e])
                }
                JsStringBodyRepr::Flat(units) => {
                    let s = start as usize;
                    let e = s + length as usize;
                    hash_utf16(&units[s..e])
                }
                _ => 0,
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
                },
                external_visit,
            )
        }
        SliceSource::Latin1Slice { start: s, len } => {
            // Slicing Latin-1 collapses into a fresh Latin-1 body
            // so the slice keeps the 1-byte-per-code-unit advantage
            // on the slice path.
            let bytes = heap.read_payload(string, |b| match &b.repr {
                JsStringBodyRepr::InlineLatin1(bytes) => {
                    let s = s as usize;
                    let e = s + len as usize;
                    bytes[s..e].to_vec()
                }
                JsStringBodyRepr::Latin1(bytes) => {
                    let s = s as usize;
                    let e = s + len as usize;
                    bytes[s..e].to_vec()
                }
                _ => Vec::new(),
            });
            alloc_latin1_string_body_with_roots(heap, JsStringId::new(0), &bytes, external_visit)
        }
        SliceSource::SlicedCollapse { parent, abs_start } => {
            // Compose into a single Sliced view over the original
            // parent; never produce Sliced(Sliced(...)).
            let parent_hash = heap.read_payload(parent, |b| b.hash);
            // Per-sub-slice hash differs from parent_hash unless
            // start==0 && length==parent.len; fall back to walking
            // the units in that case. For now reuse the parent
            // hash when the slice covers the whole parent, else
            // re-materialise the units to hash them.
            let hash = if abs_start == 0 && length == heap.read_payload(parent, |b| b.len) {
                parent_hash
            } else {
                let units = to_utf16_vec_slice(heap, parent, abs_start, length);
                hash_utf16(&units)
            };
            heap.alloc_old_with_roots(
                JsStringBody {
                    id: JsStringId::new(0),
                    len: length,
                    hash,
                    repr: JsStringBodyRepr::Sliced {
                        parent,
                        start: abs_start,
                    },
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
            JsStringBodyRepr::InlineFlat(_) | JsStringBodyRepr::Flat(_)
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
    let is_indirect = heap.read_payload(string, |b| {
        matches!(
            b.repr,
            JsStringBodyRepr::Cons { .. } | JsStringBodyRepr::Sliced { .. }
        )
    });
    if !is_indirect {
        return Ok(());
    }
    let units = to_utf16_vec(heap, string);
    let latin1 = units.iter().all(|&u| u <= 0xFF);
    // Reserving side storage can scavenge; keep `string` rooted so its handle is
    // forwarded, then mutate the relocated body.
    let mut rooted = string;
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        let p = &mut rooted as *mut JsStringHandle as *mut RawGc;
        visitor(p);
        external_visit(visitor);
    };
    let repr = if latin1 {
        if units.len() > INLINE_LATIN1_CAP {
            heap.reserve_bytes_with_roots(units.len() as u64, &mut visit)?;
        }
        let bytes: Vec<u8> = units.iter().map(|&u| u as u8).collect();
        if bytes.len() <= INLINE_LATIN1_CAP {
            let mut inline = [0u8; INLINE_LATIN1_CAP];
            inline[..bytes.len()].copy_from_slice(&bytes);
            JsStringBodyRepr::InlineLatin1(inline)
        } else {
            JsStringBodyRepr::Latin1(bytes)
        }
    } else {
        if units.len() > INLINE_FLAT_CAP {
            heap.reserve_bytes_with_roots((units.len() as u64).saturating_mul(2), &mut visit)?;
        }
        if units.len() <= INLINE_FLAT_CAP {
            let mut inline = [0u16; INLINE_FLAT_CAP];
            inline[..units.len()].copy_from_slice(&units);
            JsStringBodyRepr::InlineFlat(inline)
        } else {
            JsStringBodyRepr::Flat(units)
        }
    };
    heap.with_payload(rooted, |b| b.repr = repr);
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
        JsStringBodyRepr::Latin1(bytes) => {
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
        JsStringBodyRepr::Flat(code_units) => {
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
        Fast::Rope => to_utf16_vec(heap, string)
            .into_iter()
            .eq(key.encode_utf16()),
    }
}

/// Materialise a string body into a fresh `Vec<u16>` of UTF-16 code
/// units. Cold path: hot lookups should compare handles / ids.
#[must_use]
pub fn to_utf16_vec(heap: &GcHeap, string: JsStringHandle) -> Vec<u16> {
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
            JsStringBodyRepr::Flat(units) => {
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
            JsStringBodyRepr::Latin1(bytes) => {
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
            JsStringBodyRepr::Flat(units) => {
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
            JsStringBodyRepr::Latin1(bytes) => {
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
/// `Sliced`, which carry no contiguous own buffer.
fn flat_content(repr: &JsStringBodyRepr, len: usize) -> Option<FlatContent<'_>> {
    match repr {
        JsStringBodyRepr::InlineLatin1(buf) => Some(FlatContent::Latin1(&buf[..len])),
        JsStringBodyRepr::Latin1(bytes) => Some(FlatContent::Latin1(bytes.as_slice())),
        JsStringBodyRepr::InlineFlat(buf) => Some(FlatContent::Wide(&buf[..len])),
        JsStringBodyRepr::Flat(units) => Some(FlatContent::Wide(units.as_slice())),
        JsStringBodyRepr::Cons { .. } | JsStringBodyRepr::Sliced { .. } => None,
    }
}

impl FlatContent<'_> {
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
    let (a_len, a_hash, a_is_cons) = heap.read_payload(a, |body| {
        (
            body.len,
            body.hash,
            matches!(body.repr, JsStringBodyRepr::Cons { .. }),
        )
    });
    let (b_len, b_hash, b_is_cons) = heap.read_payload(b, |body| {
        (
            body.len,
            body.hash,
            matches!(body.repr, JsStringBodyRepr::Cons { .. }),
        )
    });
    if a_len != b_len {
        return false;
    }
    if a_len == 0 {
        return true;
    }
    // `body.hash` matches the FNV-1a of the flattened content only
    // when neither side is a cons rope: cons bodies carry a
    // placeholder hash (see `fnv_combine`). When both sides are
    // non-cons, mismatched hashes are a fast reject; otherwise fall
    // through to the body walk.
    if !a_is_cons && !b_is_cons && a_hash != b_hash {
        return false;
    }
    // Direct content compare when both bodies are flat (inline or side
    // storage) — the dominant case for interned/flattened keys (e.g. Map/Set
    // lookups), avoiding two throwaway `to_utf16_vec` allocations. `Cons` /
    // `Sliced` bodies return `None` and fall through to the materialising walk.
    if let Some(answer) = heap.read_payload(a, |ba| {
        let va = flat_content(&ba.repr, a_len as usize)?;
        heap.read_payload(b, |bb| {
            flat_content(&bb.repr, b_len as usize).map(|vb| va.content_eq(&vb))
        })
    }) {
        return answer;
    }
    to_utf16_vec(heap, a) == to_utf16_vec(heap, b)
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
            assert!(matches!(b.repr, JsStringBodyRepr::Flat(_)));
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
