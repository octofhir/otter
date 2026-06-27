//! GC-backed JavaScript string handle.
//!
//! Phase B of the JsString migration: the public [`JsString`] is now
//! a 16-byte `Copy` value pairing a 4-byte [`JsStringHandle`]
//! (`Gc<JsStringBody>`) with a `u32` cached length and a `u32`
//! truncated FNV-1a hash. All payload data — flat WTF-16, Latin-1,
//! cons-rope, sliced views — lives on the GC heap inside
//! [`JsStringBody`]; tracing reaches every body through the handle
//! stored on the wrapper.
//!
//! # Contents
//! - [`JsString`] — the public string handle (`Copy + Eq + Hash` by
//!   handle identity).
//! - [`MAX_ROPE_DEPTH`] — re-export of the body-level depth bound.
//! - [`Interrupted`] / [`INDEX_OF_INTERRUPT_BUDGET`] —
//!   interrupt-aware search primitives shared with the prototype.
//!
//! # Invariants
//! - `len()` is O(1) heap-free via the cached field; constructors
//!   prime it from the body at allocation time and never re-read the
//!   heap.
//! - Derived [`PartialEq`] / [`Hash`] are **handle identity**.
//!   Spec-shaped value equality flows through
//!   [`JsString::equals(other, heap)`] / [`gc_body::equals_string_bodies`].
//! - Reader methods (`to_utf16_vec`, `to_lossy_string`,
//!   `char_code_at`, `index_of`, …) require an explicit
//!   `&otter_gc::GcHeap` parameter; no thread-local heap.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-ecmascript-language-types-string-type>
//!
//! # Submodules
//! - [`dispatch`] — compile-time `String.<static>` dispatcher used by
//!   the `StringCall` opcode.
//! - [`exotic`] — String exotic object virtual own-property helpers.
//! - [`ops`] — lower-level VM string opcodes (concat, slice, etc.).
//! - [`prototype`] — `String.prototype.*` intrinsic implementations.
//! - [`statics`] — JS-visible static method specs installed on the
//!   `String` constructor object (`fromCharCode`, `fromCodePoint`).

pub mod dispatch;
pub(crate) mod exotic;
pub mod gc_body;
pub mod intrinsic;
pub mod ops;
pub mod prototype;
pub mod statics;

use otter_gc::{GcHeap, OutOfMemory};

pub use gc_body::{
    JS_STRING_BODY_TYPE_TAG, JsStringBody, JsStringBodyRepr, JsStringHandle, JsStringId,
    MAX_ROPE_DEPTH as GC_MAX_ROPE_DEPTH, alloc_flat_string_body_with_roots,
    alloc_latin1_string_body_with_roots, concat_string_bodies, eq_str, equals_string_bodies,
    flatten_string_body, hash_latin1, hash_utf16, slice_string_body, to_utf16_vec,
};

/// Maximum depth of an unflattened cons rope. Re-export of
/// [`gc_body::MAX_ROPE_DEPTH`] cast to `usize` for callers that still
/// reason in `Vec::with_capacity` units.
pub const MAX_ROPE_DEPTH: usize = gc_body::MAX_ROPE_DEPTH as usize;

/// GC-backed JavaScript string handle.
///
/// 16 bytes (`JsStringHandle` + `u32` len + `u32` cached hash).
/// `Copy`. Derived [`PartialEq`] / [`Hash`] are handle identity —
/// spec value equality goes through [`Self::equals`]; the cached
/// hash exposed by [`Self::cached_hash`] is heap-free and stable
/// across distinct allocations of the same content.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct JsString {
    /// Strong handle to the body. Traced through the wrapper at every
    /// GC root.
    handle: JsStringHandle,
    /// O(1) heap-free length in UTF-16 code units. Primed at
    /// construction from the body and never re-read.
    cached_len: u32,
    /// O(1) heap-free FNV-1a hash truncated to 32 bits. Distinct
    /// allocations of the same code-unit content produce the same
    /// `cached_hash`; collisions are possible but rare.
    cached_hash: u32,
}

fn no_extra_roots(_v: &mut dyn FnMut(*mut otter_gc::raw::RawGc)) {}

/// Truncate the body's 64-bit FNV-1a hash to 32 bits. Kept consistent
/// across every cached-hash priming site so callers comparing
/// `cached_hash` values get the same answer regardless of which
/// constructor produced the wrapper.
#[inline]
const fn hash_to_u32(h: u64) -> u32 {
    ((h ^ (h >> 32)) & 0xFFFF_FFFF) as u32
}

fn latin1_bytes_from_utf16(units: &[u16]) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(units.len());
    for &unit in units {
        if unit > u8::MAX as u16 {
            return None;
        }
        bytes.push(unit as u8);
    }
    Some(bytes)
}

fn latin1_bytes_from_str(s: &str) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(s.len());
    for ch in s.chars() {
        let code = ch as u32;
        if code > u8::MAX as u32 {
            return None;
        }
        bytes.push(code as u8);
    }
    Some(bytes)
}

impl JsString {
    fn from_handle(handle: JsStringHandle, heap: &GcHeap) -> Self {
        let (cached_len, cached_hash) = heap.read_payload(handle, |b| (b.len, hash_to_u32(b.hash)));
        Self {
            handle,
            cached_len,
            cached_hash,
        }
    }

    /// Strong handle to the underlying body. Used by GC tracing and by
    /// downstream collectors that need to compare strings by identity.
    #[must_use]
    pub fn handle(self) -> JsStringHandle {
        self.handle
    }

    /// Construct a flat WTF-16 string body.
    ///
    /// # Errors
    /// Surfaces [`OutOfMemory`] verbatim.
    pub fn from_utf16_units(units: &[u16], heap: &mut GcHeap) -> Result<Self, OutOfMemory> {
        let mut roots = no_extra_roots;
        Self::from_utf16_units_with_roots(units, heap, &mut roots)
    }

    /// Construct a flat WTF-16 string body while exposing caller roots.
    ///
    /// # Errors
    /// Surfaces [`OutOfMemory`] verbatim.
    pub(crate) fn from_utf16_units_with_roots(
        units: &[u16],
        heap: &mut GcHeap,
        external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
    ) -> Result<Self, OutOfMemory> {
        if let Some(bytes) = latin1_bytes_from_utf16(units) {
            return Self::from_latin1_with_roots(&bytes, heap, external_visit);
        }
        let handle = gc_body::alloc_flat_string_body_with_roots(
            heap,
            JsStringId::new(0),
            units,
            external_visit,
        )?;
        let cached_hash = hash_to_u32(gc_body::hash_utf16(units));
        Ok(Self {
            handle,
            cached_len: units.len() as u32,
            cached_hash,
        })
    }

    /// Construct from a Rust `&str`.
    ///
    /// # Errors
    /// See [`Self::from_utf16_units`].
    pub fn from_str(s: &str, heap: &mut GcHeap) -> Result<Self, OutOfMemory> {
        // ASCII is valid Latin-1 verbatim: byte `i` equals UTF-16 code unit
        // `i`. Take the compact body directly from the UTF-8 bytes.
        if s.is_ascii() {
            return Self::from_latin1(s.as_bytes(), heap);
        }
        if let Some(bytes) = latin1_bytes_from_str(s) {
            return Self::from_latin1(&bytes, heap);
        }
        let units: Vec<u16> = s.encode_utf16().collect();
        Self::from_utf16_units(&units, heap)
    }

    /// Construct from a Rust `&str` while exposing caller roots.
    ///
    /// # Errors
    /// See [`Self::from_utf16_units_with_roots`].
    pub(crate) fn from_str_with_roots(
        s: &str,
        heap: &mut GcHeap,
        external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
    ) -> Result<Self, OutOfMemory> {
        if s.is_ascii() {
            return Self::from_latin1_with_roots(s.as_bytes(), heap, external_visit);
        }
        if let Some(bytes) = latin1_bytes_from_str(s) {
            return Self::from_latin1_with_roots(&bytes, heap, external_visit);
        }
        let units: Vec<u16> = s.encode_utf16().collect();
        Self::from_utf16_units_with_roots(&units, heap, external_visit)
    }

    /// Construct from a Latin-1 / ASCII byte slice. Each byte
    /// zero-extends to a `u16` on read.
    ///
    /// # Errors
    /// Surfaces [`OutOfMemory`] verbatim.
    pub fn from_latin1(bytes: &[u8], heap: &mut GcHeap) -> Result<Self, OutOfMemory> {
        let mut roots = no_extra_roots;
        Self::from_latin1_with_roots(bytes, heap, &mut roots)
    }

    /// Construct from a Latin-1 / ASCII byte slice while exposing caller roots.
    ///
    /// # Errors
    /// Surfaces [`OutOfMemory`] verbatim.
    pub(crate) fn from_latin1_with_roots(
        bytes: &[u8],
        heap: &mut GcHeap,
        external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
    ) -> Result<Self, OutOfMemory> {
        let handle = gc_body::alloc_latin1_string_body_with_roots(
            heap,
            JsStringId::new(0),
            bytes,
            external_visit,
        )?;
        let cached_hash = hash_to_u32(gc_body::hash_latin1(bytes));
        Ok(Self {
            handle,
            cached_len: bytes.len() as u32,
            cached_hash,
        })
    }

    /// Empty string convenience constructor.
    ///
    /// # Errors
    /// See [`Self::from_utf16_units`].
    pub fn empty(heap: &mut GcHeap) -> Result<Self, OutOfMemory> {
        Self::from_utf16_units(&[], heap)
    }

    /// `"undefined"` convenience constructor. Used by every
    /// `ToString(undefined)` / missing-arg coercion site.
    ///
    /// # Errors
    /// See [`Self::from_latin1`].
    pub fn undefined_str(heap: &mut GcHeap) -> Result<Self, OutOfMemory> {
        Self::from_latin1(b"undefined", heap)
    }

    /// `"null"` convenience constructor. Used by `ToString(null)`.
    ///
    /// # Errors
    /// See [`Self::from_latin1`].
    pub fn null_str(heap: &mut GcHeap) -> Result<Self, OutOfMemory> {
        Self::from_latin1(b"null", heap)
    }

    /// Length in WTF-16 code units (O(1), heap-free).
    #[must_use]
    pub fn len(self) -> u32 {
        self.cached_len
    }

    /// `true` when [`Self::len`] is zero. Heap-free.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.cached_len == 0
    }

    /// Heap-free FNV-1a hash truncated to 32 bits. Distinct
    /// allocations of the same code-unit content share the same value
    /// — suitable for hashing key projections (e.g. [`crate::MapKey`]).
    /// Collisions are possible; pair with [`Self::equals`] before
    /// concluding two strings are equal.
    #[must_use]
    pub fn cached_hash(self) -> u32 {
        self.cached_hash
    }

    /// Concatenate two strings; produces a cons-rope body unless one
    /// side is empty (then returns the other handle unchanged).
    ///
    /// # Errors
    /// Surfaces [`OutOfMemory`] verbatim.
    pub fn concat(left: JsString, right: JsString, heap: &mut GcHeap) -> Result<Self, OutOfMemory> {
        if right.is_empty() {
            return Ok(left);
        }
        if left.is_empty() {
            return Ok(right);
        }
        let mut roots = no_extra_roots;
        let handle = gc_body::concat_string_bodies(heap, left.handle, right.handle, &mut roots)?;
        let (cached_len, cached_hash) = heap.read_payload(handle, |b| (b.len, hash_to_u32(b.hash)));
        Ok(Self {
            handle,
            cached_len,
            cached_hash,
        })
    }

    /// O(1) substring view (or fresh body when the source is cons /
    /// latin-1; see [`gc_body::slice_string_body`]).
    ///
    /// # Errors
    /// Surfaces [`OutOfMemory`] verbatim.
    pub fn slice(self, start: u32, length: u32, heap: &mut GcHeap) -> Result<Self, OutOfMemory> {
        let mut roots = no_extra_roots;
        let handle = gc_body::slice_string_body(heap, self.handle, start, length, &mut roots)?;
        let (cached_len, cached_hash) = heap.read_payload(handle, |b| (b.len, hash_to_u32(b.hash)));
        Ok(Self {
            handle,
            cached_len,
            cached_hash,
        })
    }

    /// Realise a rope into a flat body. Returns `self` unchanged when
    /// already flat.
    ///
    /// # Errors
    /// Surfaces [`OutOfMemory`] verbatim.
    pub fn flatten(self, heap: &mut GcHeap) -> Result<Self, OutOfMemory> {
        let mut roots = no_extra_roots;
        let handle = gc_body::flatten_string_body(heap, self.handle, &mut roots)?;
        Ok(Self {
            handle,
            cached_len: self.cached_len,
            cached_hash: self.cached_hash,
        })
    }

    /// Flatten a rope / slice body **in place** so this handle (and every other
    /// handle to the same body) reads as a flat string thereafter. A no-op for
    /// already-flat strings. Used before repeated scans (`indexOf`, `includes`,
    /// `split`) so the body materializes once instead of on every call.
    ///
    /// # Errors
    /// Surfaces [`OutOfMemory`] verbatim.
    pub fn flatten_in_place(self, heap: &mut GcHeap) -> Result<(), OutOfMemory> {
        let mut roots = no_extra_roots;
        gc_body::flatten_in_place(heap, self.handle, &mut roots)
    }

    /// Body handle for the legacy bridge. Phase B: the wrapper *is*
    /// the handle, so this is a copy.
    ///
    /// # Errors
    /// Never fails today; the result type is preserved so callers can
    /// stay shaped like the Phase A bridge.
    pub fn to_gc_handle(self, _heap: &mut GcHeap) -> Result<JsStringHandle, OutOfMemory> {
        Ok(self.handle)
    }

    /// Build a wrapper around an existing GC body handle. Reads the
    /// body once to prime [`Self::cached_len`].
    ///
    /// # Errors
    /// Never fails today; the result type is preserved for legacy
    /// bridge callers.
    pub fn from_gc_handle(heap: &GcHeap, handle: JsStringHandle) -> Result<Self, OutOfMemory> {
        Ok(Self::from_handle(handle, heap))
    }

    /// Materialise the string into a freshly-allocated `Vec<u16>` of
    /// code units.
    #[must_use]
    pub fn to_utf16_vec(self, heap: &GcHeap) -> Vec<u16> {
        gc_body::to_utf16_vec(heap, self.handle)
    }

    /// Render as a lossy Rust `String` for display / diagnostics.
    /// Lone surrogates round-trip via `String::from_utf16_lossy`.
    #[must_use]
    pub fn to_lossy_string(self, heap: &GcHeap) -> String {
        // A Latin-1 body maps one byte per code point, so it never carries a
        // surrogate: an all-ASCII body is already valid UTF-8 (a single memcpy),
        // and any other Latin-1 body widens byte→char directly. Both avoid the
        // general path's widen-to-`u16`-then-narrow-`char` round trip — the
        // dominant cost when, e.g., `JSON.parse` lifts a large ASCII input.
        if let Some(s) = self.with_latin1(heap, |bytes| {
            if bytes.is_ascii() {
                // SAFETY: ASCII bytes are valid UTF-8.
                unsafe { String::from_utf8_unchecked(bytes.to_vec()) }
            } else {
                bytes.iter().map(|&b| b as char).collect()
            }
        }) {
            return s;
        }
        let units = gc_body::to_utf16_vec(heap, self.handle);
        String::from_utf16_lossy(&units)
    }

    /// Borrow the underlying Latin-1 bytes when the body is the
    /// Latin-1 variant. The closure receives a borrow scoped to the
    /// payload read.
    #[must_use]
    pub fn with_latin1<F, R>(self, heap: &GcHeap, f: F) -> Option<R>
    where
        F: FnOnce(&[u8]) -> R,
    {
        heap.read_payload(self.handle, |body| match &body.repr {
            JsStringBodyRepr::InlineLatin1(bytes) => Some(f(&bytes[..body.len as usize])),
            JsStringBodyRepr::Latin1(bytes) => Some(f(bytes)),
            _ => None,
        })
    }

    /// `true` when content comparisons can read this body in place without
    /// materialising a temporary UTF-16 vector.
    #[must_use]
    pub fn is_flat_or_latin1(self, heap: &GcHeap) -> bool {
        heap.read_payload(self.handle, |body| {
            matches!(
                body.repr,
                JsStringBodyRepr::InlineFlat(_)
                    | JsStringBodyRepr::Flat(_)
                    | JsStringBodyRepr::InlineLatin1(_)
                    | JsStringBodyRepr::Latin1(_)
            )
        })
    }

    /// Code-unit at `index`, or `None` for out-of-range. Walks the
    /// body iteratively — no allocation.
    #[must_use]
    pub fn char_code_at(self, index: u32, heap: &GcHeap) -> Option<u16> {
        if index >= self.cached_len {
            return None;
        }
        enum Step {
            Found(u16),
            Descend(JsStringHandle, u32),
        }
        let mut handle = self.handle;
        let mut idx = index;
        loop {
            let step = heap.read_payload(handle, |body| match &body.repr {
                JsStringBodyRepr::InlineFlat(units) => Step::Found(units[idx as usize]),
                JsStringBodyRepr::Flat(units) => Step::Found(units[idx as usize]),
                JsStringBodyRepr::InlineLatin1(bytes) => {
                    Step::Found(u16::from(bytes[idx as usize]))
                }
                JsStringBodyRepr::Latin1(bytes) => Step::Found(u16::from(bytes[idx as usize])),
                JsStringBodyRepr::Sliced { parent, start } => Step::Descend(*parent, *start + idx),
                JsStringBodyRepr::Cons { left, right, .. } => {
                    let left_len = heap.read_payload(*left, |b| b.len);
                    if idx < left_len {
                        Step::Descend(*left, idx)
                    } else {
                        Step::Descend(*right, idx - left_len)
                    }
                }
            });
            match step {
                Step::Found(unit) => return Some(unit),
                Step::Descend(h, i) => {
                    handle = h;
                    idx = i;
                }
            }
        }
    }

    /// Value equality on code units. Fast-paths handle identity and
    /// cached-length mismatch before delegating to
    /// [`gc_body::equals_string_bodies`].
    #[must_use]
    pub fn equals(self, other: JsString, heap: &GcHeap) -> bool {
        if self.handle == other.handle {
            return true;
        }
        if self.cached_len != other.cached_len {
            return false;
        }
        // Cannot short-circuit on `cached_hash` mismatch: cons-rope
        // hashes use a non-FNV composition that does not match the
        // FNV-1a of the flattened content, so two semantically equal
        // strings can carry distinct cached hashes. Fall through to
        // the body walk which compares code units directly.
        gc_body::equals_string_bodies(heap, self.handle, other.handle)
    }

    /// Find `needle` starting at code-unit `from`. Returns the match
    /// position or `None`.
    ///
    /// `interrupt` is consulted every
    /// [`INDEX_OF_INTERRUPT_BUDGET`] iterations; a tripped flag
    /// produces an [`Interrupted`] sentinel.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-string.prototype.indexof>
    pub fn index_of(
        self,
        needle: JsString,
        from: u32,
        interrupt: Option<&crate::InterruptFlag>,
        heap: &GcHeap,
    ) -> Result<Option<u32>, Interrupted> {
        let n_len = needle.cached_len;
        if n_len == 0 {
            return Ok(Some(from.min(self.cached_len)));
        }
        let h_len = self.cached_len;
        if h_len < n_len {
            return Ok(None);
        }
        let last_start = h_len - n_len;
        if from > last_start {
            return Ok(None);
        }
        if let Some(result) = try_with_two_latin1(self, needle, heap, |h_bytes, n_bytes| {
            latin1_index_of(
                h_bytes,
                n_bytes,
                from as usize,
                last_start as usize,
                interrupt,
            )
        }) {
            return result;
        }
        let haystack = gc_body::to_utf16_vec(heap, self.handle);
        let needle_units = gc_body::to_utf16_vec(heap, needle.handle);
        utf16_index_of(
            &haystack,
            &needle_units,
            from as usize,
            last_start as usize,
            interrupt,
        )
    }

    /// Find the **last** occurrence of `needle` at or before
    /// `position`. Returns `None` when no match exists.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-string.prototype.lastindexof>
    pub fn last_index_of(
        self,
        needle: JsString,
        position: u32,
        interrupt: Option<&crate::InterruptFlag>,
        heap: &GcHeap,
    ) -> Result<Option<u32>, Interrupted> {
        let n_len = needle.cached_len;
        let h_len = self.cached_len;
        if n_len == 0 {
            return Ok(Some(position.min(h_len)));
        }
        if h_len < n_len {
            return Ok(None);
        }
        let max_start = h_len - n_len;
        let last_start = position.min(max_start);
        if let Some(result) = try_with_two_latin1(self, needle, heap, |h_bytes, n_bytes| {
            latin1_last_index_of(h_bytes, n_bytes, last_start as usize, interrupt)
        }) {
            return result;
        }
        let haystack = gc_body::to_utf16_vec(heap, self.handle);
        let needle_units = gc_body::to_utf16_vec(heap, needle.handle);
        utf16_last_index_of(&haystack, &needle_units, last_start as usize, interrupt)
    }

    /// `true` when this string starts with `prefix` at offset `from`.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-string.prototype.startswith>
    #[must_use]
    pub fn starts_with(self, prefix: JsString, from: u32, heap: &GcHeap) -> bool {
        let p_len = prefix.cached_len;
        if p_len == 0 {
            return true;
        }
        if from.saturating_add(p_len) > self.cached_len {
            return false;
        }
        if let Some(result) = try_with_two_latin1(self, prefix, heap, |h, p| {
            let from = from as usize;
            let p_len = p_len as usize;
            h[from..from + p_len] == *p
        }) {
            return result;
        }
        let haystack = gc_body::to_utf16_vec(heap, self.handle);
        let prefix_units = gc_body::to_utf16_vec(heap, prefix.handle);
        let from = from as usize;
        let p_len = p_len as usize;
        haystack[from..from + p_len] == prefix_units[..]
    }

    /// `true` when this string ends with `suffix`. `end_position`
    /// caps the haystack to the first `end_position` code units —
    /// matches `String.prototype.endsWith` semantics.
    #[must_use]
    pub fn ends_with(self, suffix: JsString, end_position: u32, heap: &GcHeap) -> bool {
        let total = self.cached_len.min(end_position);
        let s_len = suffix.cached_len;
        if s_len > total {
            return false;
        }
        self.starts_with(suffix, total - s_len, heap)
    }

    /// Lexicographic code-unit comparison used by `<`, `<=`, `>`,
    /// `>=` for two strings.
    #[must_use]
    pub fn compare_lex(self, other: JsString, heap: &GcHeap) -> std::cmp::Ordering {
        let a = gc_body::to_utf16_vec(heap, self.handle);
        let b = gc_body::to_utf16_vec(heap, other.handle);
        a.cmp(&b)
    }
}

fn try_with_two_latin1<F, R>(a: JsString, b: JsString, heap: &GcHeap, f: F) -> Option<R>
where
    F: FnOnce(&[u8], &[u8]) -> R,
{
    heap.read_payload(a.handle, |a_body| {
        let a_bytes = match &a_body.repr {
            JsStringBodyRepr::InlineLatin1(bytes) => &bytes[..a_body.len as usize],
            JsStringBodyRepr::Latin1(bytes) => bytes.as_slice(),
            _ => return None,
        };
        heap.read_payload(b.handle, |b_body| {
            let b_bytes = match &b_body.repr {
                JsStringBodyRepr::InlineLatin1(bytes) => &bytes[..b_body.len as usize],
                JsStringBodyRepr::Latin1(bytes) => bytes.as_slice(),
                _ => return None,
            };
            Some(f(a_bytes, b_bytes))
        })
    })
}

/// Loop-iteration budget at which `index_of` checks the interrupt
/// flag. Matches the foundation plan's "every 4096 iterations" rule
/// for native loops.
pub const INDEX_OF_INTERRUPT_BUDGET: u32 = 4096;

/// Latin-1 index_of fast path: SWAR scan for the needle's first
/// byte, then verify the candidate via slice equality.
fn latin1_index_of(
    haystack: &[u8],
    needle: &[u8],
    from: usize,
    last_start: usize,
    interrupt: Option<&crate::InterruptFlag>,
) -> Result<Option<u32>, Interrupted> {
    debug_assert!(!needle.is_empty());
    debug_assert!(last_start < haystack.len());
    debug_assert!(last_start + needle.len() <= haystack.len());
    let first = needle[0];
    let n_len = needle.len();
    let mut search_start = from;
    let mut steps: u32 = 0;
    while search_start <= last_start {
        let Some(rel) = crate::swar::find_byte(&haystack[search_start..=last_start], first, 0)
        else {
            return Ok(None);
        };
        let i = search_start + rel;
        if haystack[i..i + n_len] == *needle {
            return Ok(Some(i as u32));
        }
        steps = steps.saturating_add(rel as u32 + 1);
        if steps >= INDEX_OF_INTERRUPT_BUDGET {
            if let Some(flag) = interrupt
                && flag.is_set()
            {
                return Err(Interrupted);
            }
            steps = 0;
        }
        search_start = i + 1;
    }
    Ok(None)
}

/// Latin-1 last_index_of fast path: SWAR rfind for the needle's
/// first byte, then verify the candidate via slice equality.
fn latin1_last_index_of(
    haystack: &[u8],
    needle: &[u8],
    last_start: usize,
    interrupt: Option<&crate::InterruptFlag>,
) -> Result<Option<u32>, Interrupted> {
    debug_assert!(!needle.is_empty());
    debug_assert!(last_start + needle.len() <= haystack.len());
    let first = needle[0];
    let n_len = needle.len();
    let mut search_end = last_start + 1;
    let mut steps: u32 = 0;
    while search_end > 0 {
        let Some(i) = crate::swar::rfind_byte(&haystack[..search_end], first) else {
            return Ok(None);
        };
        if haystack[i..i + n_len] == *needle {
            return Ok(Some(i as u32));
        }
        steps = steps.saturating_add((search_end - i) as u32);
        if steps >= INDEX_OF_INTERRUPT_BUDGET {
            if let Some(flag) = interrupt
                && flag.is_set()
            {
                return Err(Interrupted);
            }
            steps = 0;
        }
        if i == 0 {
            return Ok(None);
        }
        search_end = i;
    }
    Ok(None)
}

/// UTF-16 last_index_of with SWAR-assisted reverse candidate scan.
fn utf16_last_index_of(
    haystack: &[u16],
    needle: &[u16],
    last_start: usize,
    interrupt: Option<&crate::InterruptFlag>,
) -> Result<Option<u32>, Interrupted> {
    debug_assert!(!needle.is_empty());
    debug_assert!(last_start + needle.len() <= haystack.len());
    let first = needle[0];
    let n_len = needle.len();
    let mut search_end = last_start + 1;
    let mut steps: u32 = 0;
    while search_end > 0 {
        let Some(i) = crate::swar::rfind_u16(&haystack[..search_end], first) else {
            return Ok(None);
        };
        if haystack[i..i + n_len] == *needle {
            return Ok(Some(i as u32));
        }
        steps = steps.saturating_add((search_end - i) as u32);
        if steps >= INDEX_OF_INTERRUPT_BUDGET {
            if let Some(flag) = interrupt
                && flag.is_set()
            {
                return Err(Interrupted);
            }
            steps = 0;
        }
        if i == 0 {
            return Ok(None);
        }
        search_end = i;
    }
    Ok(None)
}

/// UTF-16 index_of with SWAR-assisted candidate scan.
fn utf16_index_of(
    haystack: &[u16],
    needle: &[u16],
    from: usize,
    last_start: usize,
    interrupt: Option<&crate::InterruptFlag>,
) -> Result<Option<u32>, Interrupted> {
    debug_assert!(!needle.is_empty());
    debug_assert!(last_start < haystack.len());
    debug_assert!(last_start + needle.len() <= haystack.len());
    let first = needle[0];
    let n_len = needle.len();
    let mut search_start = from;
    let mut steps: u32 = 0;
    while search_start <= last_start {
        let Some(rel) = crate::swar::find_u16(&haystack[search_start..=last_start], first, 0)
        else {
            return Ok(None);
        };
        let i = search_start + rel;
        if haystack[i..i + n_len] == *needle {
            return Ok(Some(i as u32));
        }
        steps = steps.saturating_add(rel as u32 + 1);
        if steps >= INDEX_OF_INTERRUPT_BUDGET {
            if let Some(flag) = interrupt
                && flag.is_set()
            {
                return Err(Interrupted);
            }
            steps = 0;
        }
        search_start = i + 1;
    }
    Ok(None)
}

/// Sentinel returned by [`JsString::index_of`] when the runtime
/// interrupt flag was observed. Carries no payload — callers
/// translate it to `VmError::Interrupted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("interrupted")
    }
}

impl std::error::Error for Interrupted {}

#[cfg(test)]
mod tests {
    use super::*;

    fn h() -> GcHeap {
        GcHeap::new().expect("gc heap")
    }

    #[test]
    fn from_str_roundtrip() {
        let mut heap = h();
        let s = JsString::from_str("abc", &mut heap).unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s.to_lossy_string(&heap), "abc");
    }

    #[test]
    fn from_str_latin1_supplement_uses_latin1_body() {
        let mut heap = h();
        let s = JsString::from_str("éÿ", &mut heap).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s.to_lossy_string(&heap), "éÿ");
        heap.read_payload(s.handle, |body| match &body.repr {
            JsStringBodyRepr::InlineLatin1(bytes) => assert_eq!(&bytes[..2], &[0xe9, 0xff]),
            other => panic!("expected inline latin1 body, got {other:?}"),
        });
    }

    #[test]
    fn from_utf16_units_compacts_latin1_body() {
        let mut heap = h();
        let s = JsString::from_utf16_units(&[0x41, 0x00ff], &mut heap).unwrap();
        assert_eq!(s.to_utf16_vec(&heap), vec![0x41, 0x00ff]);
        heap.read_payload(s.handle, |body| match &body.repr {
            JsStringBodyRepr::InlineLatin1(bytes) => assert_eq!(&bytes[..2], &[0x41, 0xff]),
            other => panic!("expected inline latin1 body, got {other:?}"),
        });
    }

    #[test]
    fn long_utf16_latin1_units_use_byte_storage() {
        let mut heap = h();
        let units = vec![0x00e9; 64];
        let s = JsString::from_utf16_units(&units, &mut heap).unwrap();
        assert_eq!(s.to_utf16_vec(&heap), units);
        heap.read_payload(s.handle, |body| match &body.repr {
            JsStringBodyRepr::Latin1(bytes) => {
                assert_eq!(bytes.len(), 64);
                assert!(bytes.iter().all(|&b| b == 0xe9));
            }
            other => panic!("expected latin1 byte storage, got {other:?}"),
        });
    }

    #[test]
    fn to_gc_handle_round_trips_through_real_heap() {
        let mut gc = GcHeap::new().expect("gc heap");
        let original = JsString::from_str("hello world", &mut gc).unwrap();
        let handle = original.to_gc_handle(&mut gc).expect("to_gc_handle");
        let back = JsString::from_gc_handle(&gc, handle).expect("from_gc_handle");
        assert_eq!(back.len(), original.len());
        assert_eq!(back.to_lossy_string(&gc), original.to_lossy_string(&gc));
    }

    #[test]
    fn latin1_thin_roundtrip() {
        let mut heap = h();
        let thin = JsString::from_latin1(b"abc", &mut heap).unwrap();
        assert_eq!(thin.len(), 3);
        assert_eq!(thin.to_lossy_string(&heap), "abc");
        assert_eq!(thin.char_code_at(0, &heap), Some(b'a' as u16));
        assert_eq!(thin.char_code_at(1, &heap), Some(b'b' as u16));
        assert_eq!(thin.char_code_at(2, &heap), Some(b'c' as u16));
        assert_eq!(thin.char_code_at(3, &heap), None);
    }

    #[test]
    fn latin1_equals_flat_for_ascii() {
        let mut heap = h();
        let thin = JsString::from_latin1(b"hello", &mut heap).unwrap();
        let flat = JsString::from_str("hello", &mut heap).unwrap();
        assert!(thin.equals(flat, &heap));
        assert!(flat.equals(thin, &heap));
    }

    #[test]
    fn latin1_slice_stays_thin() {
        let mut heap = h();
        let thin = JsString::from_latin1(b"abcdef", &mut heap).unwrap();
        let mid = thin.slice(1, 3, &mut heap).unwrap();
        assert_eq!(mid.len(), 3);
        assert_eq!(mid.to_lossy_string(&heap), "bcd");
    }

    #[test]
    fn equality_on_flat() {
        let mut heap = h();
        let a = JsString::from_str("abc", &mut heap).unwrap();
        let b = JsString::from_str("abc", &mut heap).unwrap();
        let c = JsString::from_str("abd", &mut heap).unwrap();
        assert!(a.equals(b, &heap));
        assert!(!a.equals(c, &heap));
    }

    #[test]
    fn concat_produces_cons_node() {
        let mut heap = h();
        let a = JsString::from_str("a", &mut heap).unwrap();
        let b = JsString::from_str("b", &mut heap).unwrap();
        let ab = JsString::concat(a, b, &mut heap).unwrap();
        assert_eq!(ab.len(), 2);
        assert_eq!(ab.to_lossy_string(&heap), "ab");
        heap.read_payload(ab.handle(), |body| {
            assert!(matches!(body.repr, JsStringBodyRepr::Cons { .. }));
        });
    }

    #[test]
    fn concat_loop_is_linear() {
        // Build s += "abcd" 1000 times. Each step is O(1) cons work.
        let mut heap = h();
        let mut s = JsString::empty(&mut heap).unwrap();
        let piece = JsString::from_str("abcd", &mut heap).unwrap();
        for _ in 0..1_000 {
            s = JsString::concat(s, piece, &mut heap).unwrap();
        }
        assert_eq!(s.len(), 4_000);
        assert_eq!(s.to_lossy_string(&heap).len(), 4_000);
    }

    #[test]
    fn slice_returns_view_for_flat_parent() {
        let mut heap = h();
        // Include one non-Latin-1 code unit so the constructor must keep a
        // Flat UTF-16 parent; slicing Latin-1 collapses into a fresh compact
        // body and would not exercise the Sliced-over-Flat path.
        let units = vec![0x0100, b'a' as u16, b'b' as u16, b'c' as u16, b'd' as u16];
        let s = JsString::from_utf16_units(&units, &mut heap).unwrap();
        let sliced = s.slice(1, 3, &mut heap).unwrap();
        assert_eq!(sliced.len(), 3);
        assert_eq!(sliced.to_lossy_string(&heap), "abc");
        heap.read_payload(sliced.handle(), |body| {
            assert!(matches!(body.repr, JsStringBodyRepr::Sliced { .. }));
        });
    }

    #[test]
    fn slice_of_cons_flattens() {
        let mut heap = h();
        let a = JsString::from_str("ab", &mut heap).unwrap();
        let b = JsString::from_str("cd", &mut heap).unwrap();
        let cons = JsString::concat(a, b, &mut heap).unwrap();
        let sliced = cons.slice(1, 2, &mut heap).unwrap();
        assert_eq!(sliced.to_lossy_string(&heap), "bc");
    }

    #[test]
    fn char_code_at_walks_rope() {
        let mut heap = h();
        let a = JsString::from_str("ab", &mut heap).unwrap();
        let b = JsString::from_str("cd", &mut heap).unwrap();
        let cons = JsString::concat(a, b, &mut heap).unwrap();
        assert_eq!(cons.char_code_at(0, &heap), Some(b'a' as u16));
        assert_eq!(cons.char_code_at(2, &heap), Some(b'c' as u16));
        assert_eq!(cons.char_code_at(4, &heap), None);
    }

    #[test]
    fn surrogate_round_trip() {
        let mut heap = h();
        let units: [u16; 2] = [0xD800, 0xDC00];
        let s = JsString::from_utf16_units(&units, &mut heap).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s.char_code_at(0, &heap), Some(0xD800));
        assert_eq!(s.char_code_at(1, &heap), Some(0xDC00));
        assert_eq!(s.to_utf16_vec(&heap), units);
    }

    #[test]
    fn flatten_is_iterative_on_deep_rope() {
        let mut heap = h();
        let leaf = JsString::from_str("ab", &mut heap).unwrap();
        let mut acc = leaf;
        for _ in 0..(MAX_ROPE_DEPTH * 2) {
            acc = JsString::concat(acc, leaf, &mut heap).unwrap();
        }
        let len = acc.len();
        let flat = acc.flatten(&mut heap).unwrap();
        assert_eq!(flat.len(), len);
    }

    #[test]
    fn empty_concat_is_identity() {
        let mut heap = h();
        let a = JsString::from_str("abc", &mut heap).unwrap();
        let empty = JsString::empty(&mut heap).unwrap();
        let r1 = JsString::concat(a, empty, &mut heap).unwrap();
        let r2 = JsString::concat(empty, a, &mut heap).unwrap();
        assert_eq!(r1.to_lossy_string(&heap), "abc");
        assert_eq!(r2.to_lossy_string(&heap), "abc");
    }
}
