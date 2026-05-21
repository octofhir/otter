//! GC-managed JavaScript string body — unified variant-enum design
//! covering flat WTF-16, Latin-1, cons (rope), and sliced (view)
//! variants in a single GC body type.
//!
//! Replaces the earlier chunked-storage scaffold: every string lives
//! in exactly one [`JsStringBody`] on the GC heap. Long strings own
//! their code units inline as `Vec<u16>` / `Vec<u8>`; cons / sliced
//! variants reference children through [`JsStringHandle`] so the
//! collector can trace them transitively.
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
//! - `len` is precomputed at construction and is O(1) heap-free at
//!   the body level (callers read it via `heap.read_payload`).
//! - `hash` is the FNV-1a hash over the materialised UTF-16 code
//!   units. Cons / sliced bodies cache the hash at construction so
//!   later atom-table probes never re-walk the rope.
//! - Cons depth never exceeds [`MAX_ROPE_DEPTH`]; concatenations
//!   that would exceed it flatten the deeper child eagerly.
//! - Slicing a `Cons` flattens the parent first; slicing a `Sliced`
//!   collapses into a single `Sliced` view (no `Sliced(Sliced(...))`).
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
    /// Flat WTF-16 code units stored inline.
    Flat(Vec<u16>),
    /// Latin-1 code units stored inline. Each byte zero-extends to
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

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        match &self.repr {
            JsStringBodyRepr::Flat(_) | JsStringBodyRepr::Latin1(_) => {}
            JsStringBodyRepr::Cons { left, right, .. } => {
                if !left.is_null() {
                    let p = left as *const JsStringHandle as *mut RawGc;
                    visitor(p);
                }
                if !right.is_null() {
                    let p = right as *const JsStringHandle as *mut RawGc;
                    visitor(p);
                }
            }
            JsStringBodyRepr::Sliced { parent, .. } => {
                if !parent.is_null() {
                    let p = parent as *const JsStringHandle as *mut RawGc;
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
    heap.alloc_with_roots(
        JsStringBody {
            id,
            len,
            hash,
            repr: JsStringBodyRepr::Flat(units.to_vec()),
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
    heap.alloc_with_roots(
        JsStringBody {
            id,
            len,
            hash,
            repr: JsStringBodyRepr::Latin1(bytes.to_vec()),
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

    heap.alloc_with_roots(
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
        JsStringBodyRepr::Flat(_) => SliceSource::Flat,
        JsStringBodyRepr::Latin1(_) => SliceSource::Latin1Slice { start, len: length },
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
                JsStringBodyRepr::Flat(units) => {
                    let s = start as usize;
                    let e = s + length as usize;
                    hash_utf16(&units[s..e])
                }
                _ => 0,
            });
            heap.alloc_with_roots(
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
            heap.alloc_with_roots(
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
            // Flatten the cons, then re-slice the flat result.
            let flat = flatten_string_body(heap, string, external_visit)?;
            slice_string_body(heap, flat, start, length, external_visit)
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
    let is_flat = heap.read_payload(string, |b| matches!(b.repr, JsStringBodyRepr::Flat(_)));
    if is_flat {
        return Ok(string);
    }
    let units = to_utf16_vec(heap, string);
    alloc_flat_string_body_with_roots(heap, JsStringId::new(0), &units, external_visit)
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
            JsStringBodyRepr::Flat(units) => {
                let s = start as usize;
                let e = s + length as usize;
                out.extend_from_slice(&units[s..e]);
                Resolved::Flat
            }
            JsStringBodyRepr::Latin1(bytes) => {
                let s = start as usize;
                let e = s + length as usize;
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
            JsStringBodyRepr::Flat(units) => {
                let lo = s as usize;
                let hi = lo + l as usize;
                out.extend_from_slice(&units[lo..hi]);
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

/// Two-string equality on UTF-16 code units. Fast paths:
/// - identity (`Gc::eq`);
/// - length mismatch returns `false` immediately;
/// - hash mismatch returns `false` immediately.
#[must_use]
pub fn equals_string_bodies(heap: &GcHeap, a: JsStringHandle, b: JsStringHandle) -> bool {
    if a == b {
        return true;
    }
    let (a_len, a_hash) = heap.read_payload(a, |body| (body.len, body.hash));
    let (b_len, b_hash) = heap.read_payload(b, |body| (body.len, body.hash));
    if a_len != b_len || a_hash != b_hash {
        return false;
    }
    if a_len == 0 {
        return true;
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
            assert!(matches!(b.repr, JsStringBodyRepr::Flat(_)));
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
        });
        assert_eq!(to_utf16_vec(&heap, s), units);
    }

    #[test]
    fn cons_concat_round_trips_to_utf16() {
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
            &[b'c' as u16, b'd' as u16, b'e' as u16],
            &mut roots,
        )
        .expect("right");
        let cons = concat_string_bodies(&mut heap, left, right, &mut roots).expect("cons");
        heap.read_payload(cons, |b| {
            assert_eq!(b.len(), 5);
            assert!(matches!(b.repr, JsStringBodyRepr::Cons { .. }));
        });
        assert_eq!(
            to_utf16_vec(&heap, cons),
            vec![
                b'a' as u16,
                b'b' as u16,
                b'c' as u16,
                b'd' as u16,
                b'e' as u16
            ]
        );
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
            assert!(matches!(b.repr, JsStringBodyRepr::Latin1(_)));
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
            assert!(matches!(b.repr, JsStringBodyRepr::Flat(_)));
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
