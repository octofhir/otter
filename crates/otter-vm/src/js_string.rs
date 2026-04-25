//! JavaScript string subsystem (C2).
//!
//! Implements a V8/JSC-parity tagged-variant string hierarchy:
//!
//! | Shape         | Storage                                          | Purpose                          |
//! |---------------|--------------------------------------------------|----------------------------------|
//! | `SeqOneByte`  | `Box<[u8]>` Latin-1 / ASCII bytes                | Contiguous ≤ U+00FF strings      |
//! | `SeqTwoByte`  | `Box<[u16]>` WTF-16 code units                   | Contiguous strings (incl. lone surrogates) |
//! | `Cons`        | `(ObjectHandle, ObjectHandle, depth)`            | Lazy concat (V8 ConsString)      |
//! | `Sliced`      | `(ObjectHandle, offset)`                         | Lazy substring view              |
//! | `Thin`        | `ObjectHandle`                                   | Forwarder after in-place flatten |
//!
//! The type integrates with the unified [`crate::object::ObjectHeap`] —
//! `HeapValue::String { value: JsString }` is the heap entry. Cons/Sliced/Thin
//! variants reference *other* `HeapValue::String` slots by `ObjectHandle`, so
//! they appear as outgoing references during GC tracing.
//!
//! Spec: <https://tc39.es/ecma262/#sec-ecmascript-language-types-string-type>
//!
//! ## Design / phasing
//!
//! See `docs/c2-string-hierarchy-design.md` for the full design. Phase 1
//! (this commit) lands the data type and exposes the new repr; Cons / Sliced
//! / Thin variants are defined but not yet *constructed* by the standard
//! `from_*` helpers — those still produce `SeqTwoByte`. Lazy concat / slice /
//! flatten / Latin-1 detection / hash caching land in subsequent phases.

use std::borrow::Cow;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

use crate::object::ObjectHandle;

// ── Limits and tuning constants ────────────────────────────────────────────

/// Maximum string length, in UTF-16 code units. Mirrors V8's
/// `String::kMaxLength` on 64-bit (`(1<<29) - 24`). Operations that would
/// produce a longer result throw `RangeError("Invalid string length")`.
///
/// §22.1.3 Properties of the String Prototype Object — implementation choice;
/// the spec allows up to `2^53 - 1` but no engine ships that.
pub const MAX_STRING_LENGTH: u32 = (1 << 29) - 24;

/// Concat results of length ≤ this allocate a flat `Seq*` directly instead of
/// a `Cons` node. Below this threshold the bookkeeping overhead of a Cons
/// outweighs the copy cost. V8 uses 13.
pub const MIN_CONS_LENGTH: u32 = 13;

/// Maximum cons depth before eagerly flattening. Bounds worst-case flatten
/// cost to O(n) — without this an attacker can build a 1M-deep left chain
/// that takes O(n²) to flatten. V8 = 32.
pub const MAX_CONS_DEPTH: u8 = 32;

// ── Flag bits ──────────────────────────────────────────────────────────────

/// Bit 0: this string's content fits in Latin-1 (all code units ≤ 0xFF).
const FLAG_ONE_BYTE: u8 = 0b0000_0001;
/// Bit 1: this string is the canonical interned representative of its content.
const FLAG_INTERNALIZED: u8 = 0b0000_0010;
/// Bit 2: this string contains an unpaired surrogate.
const FLAG_LONE_SURROGATE: u8 = 0b0000_0100;

// ── Repr ───────────────────────────────────────────────────────────────────

/// The five concrete string representations.
///
/// Phase 1 only constructs `SeqTwoByte`. The other variants are defined for
/// forward-compatibility with phases 3 (Cons / Sliced / Thin) and 4
/// (`SeqOneByte`).
#[derive(Debug)]
pub enum JsStringRepr {
    /// Contiguous Latin-1 / ASCII bytes (each byte == one UTF-16 code unit).
    SeqOneByte(Box<[u8]>),

    /// Contiguous WTF-16 code units, including lone surrogates.
    SeqTwoByte(Box<[u16]>),

    /// Lazy concatenation node. `length = left.length + right.length`.
    /// Children are referenced by `ObjectHandle` into the heap; flatten and
    /// GC trace must follow them.
    Cons {
        left: ObjectHandle,
        right: ObjectHandle,
        depth: u8,
    },

    /// Lazy substring view. Reads `[offset, offset + length)` of `parent`.
    Sliced {
        parent: ObjectHandle,
        offset: u32,
    },

    /// Forwarder. Set after in-place flatten — original `Cons` / `Sliced`
    /// slots rewrite their `repr` to `Thin { forward }` so subsequent reads
    /// hop one pointer to the canonical `Seq*` slot.
    Thin { forward: ObjectHandle },
}

impl Clone for JsStringRepr {
    fn clone(&self) -> Self {
        match self {
            Self::SeqOneByte(b) => Self::SeqOneByte(b.clone()),
            Self::SeqTwoByte(u) => Self::SeqTwoByte(u.clone()),
            Self::Cons { left, right, depth } => Self::Cons {
                left: *left,
                right: *right,
                depth: *depth,
            },
            Self::Sliced { parent, offset } => Self::Sliced {
                parent: *parent,
                offset: *offset,
            },
            Self::Thin { forward } => Self::Thin { forward: *forward },
        }
    }
}

// ── JsString ───────────────────────────────────────────────────────────────

/// A JavaScript string value.
///
/// Stores the length, a lazy hash cache, flag bits, and the discriminated
/// representation. The header fields are hoisted out of the variants so
/// `len()` / `is_one_byte()` are branchless in the common case.
///
/// §6.1.4 The String Type — sequence of u16 code units, lone surrogates
/// allowed (WTF-16).
/// Spec: <https://tc39.es/ecma262/#sec-ecmascript-language-types-string-type>
pub struct JsString {
    /// Length in UTF-16 code units. O(1) regardless of representation.
    /// `SeqOneByte` reports `len = byte_count` because each byte maps to one
    /// UTF-16 code unit (Latin-1 occupies U+0000..U+00FF).
    length: u32,

    /// Lazy FNV-1a hash; `0` is the sentinel "not computed".
    hash: AtomicU32,

    /// See `FLAG_*` constants. Atomic only because hash is set lazily and we
    /// want the same `&self` access pattern.
    flags: AtomicU8,

    /// Discriminated representation.
    repr: JsStringRepr,
}

impl JsString {
    // ── Construction ────────────────────────────────────────────────────

    /// Creates a `JsString` from a UTF-8 `&str`.
    ///
    /// C2 Phase 4: auto-detects Latin-1 content. ASCII strings (`s.is_ascii()`)
    /// allocate as [`JsStringRepr::SeqOneByte`] directly from `s.as_bytes()` —
    /// halving the memory cost of source code, JSON keys, identifiers.
    /// Strings containing characters above ASCII but still within Latin-1
    /// (U+0000..U+00FF) also collapse to 1-byte. Anything else stays
    /// [`JsStringRepr::SeqTwoByte`].
    #[inline]
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        // ASCII fast path — `is_ascii` is SIMD-vectorized in std.
        if s.is_ascii() {
            return Self::from_one_byte(s.as_bytes().to_vec().into_boxed_slice());
        }
        // General path: encode to UTF-16 and let the auto-detect helper pick
        // the optimal repr.
        let units: Vec<u16> = s.encode_utf16().collect();
        Self::auto_from_units(units.into_boxed_slice())
    }

    /// Creates a `JsString` from raw UTF-16 / WTF-16 code units.
    ///
    /// C2 Phase 4: auto-detects Latin-1. If all units fit in 0x00..=0xFF
    /// (no surrogates) the result is `SeqOneByte`. Lone surrogates force
    /// `SeqTwoByte`.
    #[inline]
    pub fn from_utf16(units: impl Into<Box<[u16]>>) -> Self {
        Self::auto_from_units(units.into())
    }

    /// Creates a `JsString` from a `Vec<u16>` of WTF-16 code units.
    #[inline]
    pub fn from_utf16_vec(units: Vec<u16>) -> Self {
        Self::auto_from_units(units.into_boxed_slice())
    }

    /// Internal: pick `SeqOneByte` (when all units ≤ 0xFF) or `SeqTwoByte`.
    fn auto_from_units(units: Box<[u16]>) -> Self {
        if units.iter().all(|u| *u <= 0xFF) {
            // All units fit Latin-1 — compress to bytes.
            let bytes: Box<[u8]> =
                units.iter().map(|u| *u as u8).collect::<Vec<u8>>().into_boxed_slice();
            return Self::from_one_byte(bytes);
        }
        Self::seq_two_byte(units)
    }

    /// Creates an empty `JsString`.
    #[inline]
    pub fn empty() -> Self {
        Self::seq_two_byte(Box::new([]))
    }

    /// Allocates a `SeqOneByte` directly. Reserved for the Latin-1 fast path
    /// (Phase 4); not yet wired into `from_str`.
    pub fn from_one_byte(bytes: impl Into<Box<[u8]>>) -> Self {
        let bytes = bytes.into();
        let length = bytes.len() as u32;
        debug_assert!(
            length as usize == bytes.len(),
            "string length exceeds u32 range — caller must enforce MAX_STRING_LENGTH"
        );
        Self {
            length,
            hash: AtomicU32::new(0),
            flags: AtomicU8::new(FLAG_ONE_BYTE),
            repr: JsStringRepr::SeqOneByte(bytes),
        }
    }

    /// Decodes an oxc-encoded string with lone surrogates.
    ///
    /// oxc encodes lone surrogates in `StringLiteral.value` as `\u{FFFD}XXXX`
    /// where XXXX is the surrogate code unit in hex. The literal U+FFFD itself
    /// is encoded as `\u{FFFD}fffd`.
    pub fn from_oxc_encoded(value: &str) -> Self {
        let mut units: Vec<u16> = Vec::with_capacity(value.len());
        let chars: Vec<char> = value.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '\u{FFFD}' {
                if i + 4 < chars.len() {
                    let hex_str: String = chars[i + 1..i + 5].iter().collect();
                    if let Ok(code_unit) = u16::from_str_radix(&hex_str, 16) {
                        units.push(code_unit);
                        i += 5;
                        continue;
                    }
                }
                units.push(0xFFFD);
                i += 1;
            } else {
                let ch = chars[i];
                let mut buf = [0u16; 2];
                let encoded = ch.encode_utf16(&mut buf);
                units.extend_from_slice(encoded);
                i += 1;
            }
        }
        Self::seq_two_byte(units.into_boxed_slice())
    }

    /// Internal: build a `SeqTwoByte` JsString. Sets `contains_lone_surrogate`
    /// eagerly so `is_well_formed()` is O(1) post-construction.
    fn seq_two_byte(units: Box<[u16]>) -> Self {
        let length = units.len() as u32;
        debug_assert!(
            length as usize == units.len(),
            "string length exceeds u32 range — caller must enforce MAX_STRING_LENGTH"
        );
        let mut flags = 0u8;
        if scan_lone_surrogate(&units) {
            flags |= FLAG_LONE_SURROGATE;
        }
        Self {
            length,
            hash: AtomicU32::new(0),
            flags: AtomicU8::new(flags),
            repr: JsStringRepr::SeqTwoByte(units),
        }
    }

    /// Internal: build a `Cons` JsString. Caller is responsible for the empty
    /// elision / length cap / short-circuit-flat / depth-bound rules — see
    /// `crate::object::ObjectHeap::concat_strings`.
    pub(crate) fn cons(
        left: ObjectHandle,
        right: ObjectHandle,
        length: u32,
        depth: u8,
        is_one_byte: bool,
        contains_lone_surrogate: bool,
    ) -> Self {
        let mut flags = 0u8;
        if is_one_byte {
            flags |= FLAG_ONE_BYTE;
        }
        if contains_lone_surrogate {
            flags |= FLAG_LONE_SURROGATE;
        }
        Self {
            length,
            hash: AtomicU32::new(0),
            flags: AtomicU8::new(flags),
            repr: JsStringRepr::Cons { left, right, depth },
        }
    }

    /// Internal: build a `Sliced` JsString. Caller responsible for empty /
    /// whole / sliced-of-sliced collapse — see
    /// `crate::object::ObjectHeap::slice_string`.
    pub(crate) fn sliced(
        parent: ObjectHandle,
        offset: u32,
        length: u32,
        is_one_byte: bool,
        contains_lone_surrogate: bool,
    ) -> Self {
        let mut flags = 0u8;
        if is_one_byte {
            flags |= FLAG_ONE_BYTE;
        }
        if contains_lone_surrogate {
            flags |= FLAG_LONE_SURROGATE;
        }
        Self {
            length,
            hash: AtomicU32::new(0),
            flags: AtomicU8::new(flags),
            repr: JsStringRepr::Sliced { parent, offset },
        }
    }

    /// Internal: rewrite this string's `repr` to a `Thin` forwarder pointing
    /// at `forward`. Used by `flatten` to redirect future reads. Header
    /// fields (`length`, `flags`) are preserved; `hash` is unchanged (the
    /// content is identical so the hash is still valid).
    pub(crate) fn rewrite_to_thin(&mut self, forward: ObjectHandle) {
        self.repr = JsStringRepr::Thin { forward };
    }

    /// Internal: replace `self` with a fresh `SeqOneByte` of identical
    /// content. Used by `flatten` when rewriting in-place avoids a Thin hop
    /// (e.g. when the original slot is the canonical heap entry).
    pub(crate) fn become_seq_one_byte(&mut self, bytes: Box<[u8]>) {
        debug_assert_eq!(bytes.len() as u32, self.length);
        let mut flags = self.flags.load(Ordering::Relaxed);
        flags |= FLAG_ONE_BYTE;
        flags &= !FLAG_LONE_SURROGATE; // 1-byte strings cannot contain surrogates
        self.flags.store(flags, Ordering::Relaxed);
        self.repr = JsStringRepr::SeqOneByte(bytes);
    }

    /// Internal: replace `self` with a fresh `SeqTwoByte` of identical
    /// content.
    pub(crate) fn become_seq_two_byte(&mut self, units: Box<[u16]>) {
        debug_assert_eq!(units.len() as u32, self.length);
        let mut flags = self.flags.load(Ordering::Relaxed);
        flags &= !FLAG_ONE_BYTE;
        if scan_lone_surrogate(&units) {
            flags |= FLAG_LONE_SURROGATE;
        } else {
            flags &= !FLAG_LONE_SURROGATE;
        }
        self.flags.store(flags, Ordering::Relaxed);
        self.repr = JsStringRepr::SeqTwoByte(units);
    }

    // ── Header access (O(1)) ────────────────────────────────────────────

    /// Returns the underlying representation. For trace / flatten use.
    #[inline]
    pub fn repr(&self) -> &JsStringRepr {
        &self.repr
    }

    /// Returns the length in UTF-16 code units (= JS `.length`).
    ///
    /// §22.1.3.3 get String.prototype.length
    /// Spec: <https://tc39.es/ecma262/#sec-properties-of-string-instances-length>
    #[inline]
    pub fn len(&self) -> usize {
        self.length as usize
    }

    /// Returns `true` if the string is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Returns `true` if all code units are ≤ 0xFF (Latin-1 fits).
    #[inline]
    pub fn is_one_byte(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & FLAG_ONE_BYTE != 0
    }

    /// Returns `true` if this string is interned (canonical handle).
    #[inline]
    pub fn is_internalized(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & FLAG_INTERNALIZED != 0
    }

    /// Mark this string as interned. Idempotent.
    #[inline]
    pub fn mark_internalized(&self) {
        self.flags
            .fetch_or(FLAG_INTERNALIZED, Ordering::Relaxed);
    }

    /// Returns `true` if this string contains an unpaired surrogate. O(1).
    #[inline]
    pub fn contains_lone_surrogate(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & FLAG_LONE_SURROGATE != 0
    }

    /// Loads the cached hash. Returns `0` if not yet computed.
    #[inline]
    pub fn cached_hash(&self) -> u32 {
        self.hash.load(Ordering::Relaxed)
    }

    /// Stores the computed hash. `value` may not be `0` (the sentinel) and
    /// has bit 0 cleared (reserved for "is integer index"). Idempotent under
    /// the assumption that all callers compute the same hash function.
    #[inline]
    pub fn set_cached_hash(&self, value: u32) {
        debug_assert_ne!(value, 0, "hash 0 is reserved as 'not computed' sentinel");
        self.hash.store(value, Ordering::Relaxed);
    }

    // ── Direct unit access (Phase 1: only valid on Seq*) ────────────────

    /// Returns the WTF-16 code units. **Only valid on `SeqTwoByte`**.
    ///
    /// `SeqOneByte` callers must first call
    /// [`Self::ensure_two_byte`] (`&mut self` access) which upcasts in place.
    /// Cons / Sliced / Thin must be flattened via
    /// [`crate::object::ObjectHeap::flatten_string`] before calling this.
    ///
    /// Panics on any non-2-byte representation. Use
    /// [`Self::as_utf16_cow`] for the auto-upcasting variant that returns a
    /// `Cow` (one-shot owned for SeqOneByte, borrowed for SeqTwoByte).
    pub fn as_utf16(&self) -> &[u16] {
        match &self.repr {
            JsStringRepr::SeqTwoByte(u) => u,
            JsStringRepr::SeqOneByte(_) => panic!(
                "JsString::as_utf16 called on SeqOneByte — call ensure_two_byte first or use as_utf16_cow"
            ),
            JsStringRepr::Cons { .. } => panic!(
                "JsString::as_utf16 called on Cons — caller must flatten via ObjectHeap"
            ),
            JsStringRepr::Sliced { .. } => panic!(
                "JsString::as_utf16 called on Sliced — caller must flatten via ObjectHeap"
            ),
            JsStringRepr::Thin { .. } => panic!(
                "JsString::as_utf16 called on Thin — caller must follow forward via ObjectHeap"
            ),
        }
    }

    /// Returns the WTF-16 code units, transparently upcasting `SeqOneByte`
    /// content to a one-shot owned `Vec<u16>`.
    ///
    /// * `SeqTwoByte` → borrowed `&[u16]`, zero copy.
    /// * `SeqOneByte` → owned `Vec<u16>` (one-shot upcast from Latin-1 bytes).
    /// * `Cons` / `Sliced` / `Thin` → **panics**.
    pub fn as_utf16_cow(&self) -> Cow<'_, [u16]> {
        match &self.repr {
            JsStringRepr::SeqTwoByte(u) => Cow::Borrowed(u),
            JsStringRepr::SeqOneByte(b) => {
                Cow::Owned(b.iter().map(|byte| u16::from(*byte)).collect())
            }
            _ => panic!("JsString::as_utf16_cow called on non-flat repr"),
        }
    }

    /// In-place upcast: `SeqOneByte` → `SeqTwoByte`. No-op for already-2-byte.
    /// Panics on Cons / Sliced / Thin (caller must flatten first).
    pub fn ensure_two_byte(&mut self) {
        let bytes = match &self.repr {
            JsStringRepr::SeqOneByte(b) => b.clone(),
            JsStringRepr::SeqTwoByte(_) => return,
            _ => panic!("ensure_two_byte called on non-flat repr — flatten via ObjectHeap first"),
        };
        let units: Box<[u16]> = bytes.iter().map(|b| u16::from(*b)).collect::<Vec<u16>>().into_boxed_slice();
        self.become_seq_two_byte(units);
    }

    /// Returns the Latin-1 bytes — **only valid on `SeqOneByte`**.
    pub fn as_one_byte(&self) -> &[u8] {
        match &self.repr {
            JsStringRepr::SeqOneByte(b) => b,
            _ => panic!("JsString::as_one_byte called on non-SeqOneByte"),
        }
    }

    /// Returns the UTF-16 code unit at the given index — **only valid on
    /// flat representations**. For Cons/Sliced/Thin go through
    /// `ObjectHeap::js_string_code_unit_at`.
    ///
    /// §22.1.3.2 String.prototype.charCodeAt(pos)
    /// Spec: <https://tc39.es/ecma262/#sec-string.prototype.charcodeat>
    #[inline]
    pub fn code_unit_at(&self, index: usize) -> Option<u16> {
        match &self.repr {
            JsStringRepr::SeqTwoByte(u) => u.get(index).copied(),
            JsStringRepr::SeqOneByte(b) => b.get(index).map(|byte| u16::from(*byte)),
            _ => panic!("JsString::code_unit_at called on non-flat repr"),
        }
    }

    /// Returns the Unicode code point starting at the given UTF-16 index.
    ///
    /// §22.1.3.3 String.prototype.codePointAt(pos)
    /// Spec: <https://tc39.es/ecma262/#sec-string.prototype.codepointat>
    pub fn code_point_at(&self, index: usize) -> Option<(u32, usize)> {
        let lead = self.code_unit_at(index)?;
        if (0xD800..=0xDBFF).contains(&lead)
            && let Some(trail) = self.code_unit_at(index + 1)
            && (0xDC00..=0xDFFF).contains(&trail)
        {
            let cp = 0x10000 + ((lead as u32 - 0xD800) << 10) + (trail as u32 - 0xDC00);
            return Some((cp, 2));
        }
        Some((lead as u32, 1))
    }

    // ── Conversion ─────────────────────────────────────────────────────

    /// Converts to a Rust `String`, replacing lone surrogates with U+FFFD.
    ///
    /// Phase 1: only valid on flat representations. Phase 3+ callers should
    /// flatten first via `ObjectHeap::flatten_string`.
    #[inline]
    pub fn to_rust_string(&self) -> String {
        match &self.repr {
            JsStringRepr::SeqTwoByte(u) => String::from_utf16_lossy(u),
            JsStringRepr::SeqOneByte(b) => {
                // Latin-1 → UTF-8. Each byte is a separate code point.
                let mut out = String::with_capacity(b.len());
                for byte in b.iter() {
                    out.push(char::from(*byte));
                }
                out
            }
            _ => panic!("JsString::to_rust_string called on non-flat repr"),
        }
    }

    /// Converts to a Rust `String` if the string is valid UTF-16.
    ///
    /// Returns `None` if the string contains lone surrogates.
    #[inline]
    pub fn to_rust_string_lossless(&self) -> Option<String> {
        if self.contains_lone_surrogate() {
            return None;
        }
        Some(self.to_rust_string())
    }

    /// Returns `true` if this string is well-formed UTF-16 (no lone surrogates).
    /// O(1) — the flag is set at construction.
    ///
    /// §22.1.3.9 String.prototype.isWellFormed()
    pub fn is_well_formed(&self) -> bool {
        !self.contains_lone_surrogate()
    }

    /// Returns a new string with lone surrogates replaced by U+FFFD.
    ///
    /// §22.1.3.33 String.prototype.toWellFormed()
    pub fn to_well_formed(&self) -> JsString {
        if !self.contains_lone_surrogate() {
            return self.clone();
        }
        // Slow path: requires inspecting the units.
        let cow = self.as_utf16_cow();
        let units: &[u16] = &cow;
        let mut result = Vec::with_capacity(units.len());
        let mut i = 0;
        while i < units.len() {
            let code = units[i];
            if (0xD800..=0xDBFF).contains(&code) {
                if i + 1 < units.len() && (0xDC00..=0xDFFF).contains(&units[i + 1]) {
                    result.push(code);
                    result.push(units[i + 1]);
                    i += 2;
                } else {
                    result.push(0xFFFD);
                    i += 1;
                }
            } else if (0xDC00..=0xDFFF).contains(&code) {
                result.push(0xFFFD);
                i += 1;
            } else {
                result.push(code);
                i += 1;
            }
        }
        Self::seq_two_byte(result.into_boxed_slice())
    }

    // ── Substring / Slice (eager, Phase 1; lazy in Phase 3) ────────────

    /// Returns a substring by UTF-16 code unit indices.
    ///
    /// Phase 1: eager copy. Phase 3 will route through
    /// `ObjectHeap::slice_string` for lazy `Sliced` construction.
    pub fn substring(&self, start: usize, end: usize) -> JsString {
        let cow = self.as_utf16_cow();
        let units: &[u16] = &cow;
        let start = start.min(units.len());
        let end = end.min(units.len());
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        Self::auto_from_units(units[start..end].into())
    }

    /// Returns a slice by UTF-16 code unit range (for `String.prototype.slice`).
    pub fn slice(&self, start: usize, end: usize) -> JsString {
        let cow = self.as_utf16_cow();
        let units: &[u16] = &cow;
        if start >= end || start >= units.len() {
            return JsString::empty();
        }
        let end = end.min(units.len());
        Self::auto_from_units(units[start..end].into())
    }

    // ── Search ─────────────────────────────────────────────────────────

    /// §22.1.3.9 String.prototype.indexOf(searchString, position)
    pub fn index_of(&self, search: &JsString, from_index: usize) -> Option<usize> {
        let cow = self.as_utf16_cow();
        let units: &[u16] = &cow;
        let cow_n = search.as_utf16_cow();
        let needle: &[u16] = &cow_n;
        if needle.is_empty() {
            return Some(from_index.min(units.len()));
        }
        if from_index + needle.len() > units.len() {
            return None;
        }
        for i in from_index..=(units.len() - needle.len()) {
            if units[i..i + needle.len()] == *needle {
                return Some(i);
            }
        }
        None
    }

    /// §22.1.3.10 String.prototype.lastIndexOf
    pub fn last_index_of(&self, search: &JsString, from_index: usize) -> Option<usize> {
        let cow = self.as_utf16_cow();
        let units: &[u16] = &cow;
        let cow_n = search.as_utf16_cow();
        let needle: &[u16] = &cow_n;
        if needle.is_empty() {
            return Some(from_index.min(units.len()));
        }
        if needle.len() > units.len() {
            return None;
        }
        let max_start = from_index.min(units.len() - needle.len());
        for i in (0..=max_start).rev() {
            if units[i..i + needle.len()] == *needle {
                return Some(i);
            }
        }
        None
    }

    pub fn starts_with(&self, prefix: &JsString) -> bool {
        let me_cow = self.as_utf16_cow();
        let p_cow = prefix.as_utf16_cow();
        let me: &[u16] = &me_cow;
        let p: &[u16] = &p_cow;
        me.starts_with(p)
    }

    pub fn ends_with(&self, suffix: &JsString) -> bool {
        let me_cow = self.as_utf16_cow();
        let s_cow = suffix.as_utf16_cow();
        let me: &[u16] = &me_cow;
        let s: &[u16] = &s_cow;
        me.ends_with(s)
    }

    pub fn contains(&self, search: &JsString) -> bool {
        self.index_of(search, 0).is_some()
    }

    // ── Concatenation (eager, Phase 1; lazy via ObjectHeap::concat_strings in Phase 3) ─

    /// Eagerly concatenates two `JsString` values into a new flat string.
    ///
    /// Phase 1: O(n+m) memcpy. Phase 3 introduces
    /// `ObjectHeap::concat_strings` that produces a `Cons` for non-trivial
    /// inputs.
    pub fn concat(&self, other: &JsString) -> JsString {
        let lhs_cow = self.as_utf16_cow();
        let rhs_cow = other.as_utf16_cow();
        let lhs: &[u16] = &lhs_cow;
        let rhs: &[u16] = &rhs_cow;
        let mut units = Vec::with_capacity(lhs.len() + rhs.len());
        units.extend_from_slice(lhs);
        units.extend_from_slice(rhs);
        Self::auto_from_units(units.into_boxed_slice())
    }

    // ── Repeat ─────────────────────────────────────────────────────────

    /// §22.1.3.17 String.prototype.repeat(count)
    pub fn repeat(&self, count: usize) -> JsString {
        let cow = self.as_utf16_cow();
        let units: &[u16] = &cow;
        let mut out = Vec::with_capacity(units.len().saturating_mul(count));
        for _ in 0..count {
            out.extend_from_slice(units);
        }
        Self::auto_from_units(out.into_boxed_slice())
    }

    // ── Case conversion (ASCII fast path) ──────────────────────────────

    pub fn to_lowercase(&self) -> JsString {
        let s = self.to_rust_string();
        Self::from_str(&s.to_lowercase())
    }

    pub fn to_uppercase(&self) -> JsString {
        let s = self.to_rust_string();
        Self::from_str(&s.to_uppercase())
    }

    // ── Trim ───────────────────────────────────────────────────────────

    pub fn trim(&self) -> JsString {
        let s = self.to_rust_string();
        Self::from_str(s.trim())
    }

    pub fn trim_start(&self) -> JsString {
        let s = self.to_rust_string();
        Self::from_str(s.trim_start())
    }

    pub fn trim_end(&self) -> JsString {
        let s = self.to_rust_string();
        Self::from_str(s.trim_end())
    }
}

// ── Internals ──────────────────────────────────────────────────────────────

/// Returns `true` if `units` contains an unpaired surrogate.
fn scan_lone_surrogate(units: &[u16]) -> bool {
    let mut i = 0;
    while i < units.len() {
        let code = units[i];
        if (0xD800..=0xDBFF).contains(&code) {
            if i + 1 >= units.len() || !(0xDC00..=0xDFFF).contains(&units[i + 1]) {
                return true;
            }
            i += 2;
        } else if (0xDC00..=0xDFFF).contains(&code) {
            return true;
        } else {
            i += 1;
        }
    }
    false
}

// ── Trait implementations ──────────────────────────────────────────────────

impl Clone for JsString {
    fn clone(&self) -> Self {
        Self {
            length: self.length,
            hash: AtomicU32::new(self.hash.load(Ordering::Relaxed)),
            flags: AtomicU8::new(self.flags.load(Ordering::Relaxed)),
            repr: self.repr.clone(),
        }
    }
}

impl Eq for JsString {}

impl PartialEq for JsString {
    fn eq(&self, other: &Self) -> bool {
        // Length differs → not equal.
        if self.length != other.length {
            return false;
        }
        // Both flat → compare units. (Phase 1: only Seq* are constructed.)
        match (&self.repr, &other.repr) {
            (JsStringRepr::SeqTwoByte(a), JsStringRepr::SeqTwoByte(b)) => a == b,
            (JsStringRepr::SeqOneByte(a), JsStringRepr::SeqOneByte(b)) => a == b,
            (JsStringRepr::SeqOneByte(a), JsStringRepr::SeqTwoByte(b)) => one_byte_eq_two_byte(a, b),
            (JsStringRepr::SeqTwoByte(a), JsStringRepr::SeqOneByte(b)) => one_byte_eq_two_byte(b, a),
            // Cons / Sliced / Thin: comparison requires heap access. The
            // public API path goes through `ObjectHeap::strings_equal`. This
            // fallback only triggers if a Cons string somehow leaks past the
            // heap boundary, which would be a bug.
            _ => panic!("JsString::eq called with non-flat repr — go through ObjectHeap::strings_equal"),
        }
    }
}

fn one_byte_eq_two_byte(a: &[u8], b: &[u16]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for (lhs, rhs) in a.iter().zip(b.iter()) {
        if u16::from(*lhs) != *rhs {
            return false;
        }
    }
    true
}

impl PartialEq<str> for JsString {
    fn eq(&self, other: &str) -> bool {
        match &self.repr {
            JsStringRepr::SeqOneByte(_) | JsStringRepr::SeqTwoByte(_) => {
                let other_utf16: Vec<u16> = other.encode_utf16().collect();
                if other_utf16.len() as u32 != self.length {
                    return false;
                }
                match &self.repr {
                    JsStringRepr::SeqTwoByte(u) => *u.as_ref() == *other_utf16,
                    JsStringRepr::SeqOneByte(b) => one_byte_eq_two_byte(b, &other_utf16),
                    _ => unreachable!(),
                }
            }
            _ => panic!("JsString::eq<str> called with non-flat repr"),
        }
    }
}

impl PartialEq<&str> for JsString {
    fn eq(&self, other: &&str) -> bool {
        self == *other
    }
}

impl Hash for JsString {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Hash by content. For Seq* we hash the units / bytes (uniformly as
        // u16 so 1-byte and 2-byte same-content strings hash identically).
        match &self.repr {
            JsStringRepr::SeqTwoByte(u) => {
                state.write_u32(self.length);
                for unit in u.iter() {
                    state.write_u16(*unit);
                }
            }
            JsStringRepr::SeqOneByte(b) => {
                state.write_u32(self.length);
                for byte in b.iter() {
                    state.write_u16(u16::from(*byte));
                }
            }
            _ => panic!("JsString::hash called with non-flat repr"),
        }
    }
}

impl fmt::Debug for JsString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.repr {
            JsStringRepr::SeqOneByte(_) | JsStringRepr::SeqTwoByte(_) => {
                write!(f, "JsString({:?})", self.to_rust_string())
            }
            JsStringRepr::Cons { left, right, depth } => write!(
                f,
                "JsString::Cons{{left={:?},right={:?},len={},depth={}}}",
                left, right, self.length, depth
            ),
            JsStringRepr::Sliced { parent, offset } => write!(
                f,
                "JsString::Sliced{{parent={:?},offset={},len={}}}",
                parent, offset, self.length
            ),
            JsStringRepr::Thin { forward } => {
                write!(f, "JsString::Thin{{forward={:?}}}", forward)
            }
        }
    }
}

impl fmt::Display for JsString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.repr {
            JsStringRepr::SeqOneByte(_) | JsStringRepr::SeqTwoByte(_) => {
                write!(f, "{}", self.to_rust_string())
            }
            _ => write!(f, "<unflattened-string len={}>", self.length),
        }
    }
}

impl From<&str> for JsString {
    #[inline]
    fn from(s: &str) -> Self {
        JsString::from_str(s)
    }
}

impl From<String> for JsString {
    #[inline]
    fn from(s: String) -> Self {
        JsString::from_str(&s)
    }
}

impl From<Box<str>> for JsString {
    #[inline]
    fn from(s: Box<str>) -> Self {
        JsString::from_str(&s)
    }
}

impl From<Vec<u16>> for JsString {
    #[inline]
    fn from(units: Vec<u16>) -> Self {
        JsString::from_utf16_vec(units)
    }
}

impl From<Box<[u16]>> for JsString {
    #[inline]
    fn from(units: Box<[u16]>) -> Self {
        JsString::seq_two_byte(units)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_ascii() {
        let s = JsString::from_str("hello");
        assert_eq!(s.len(), 5);
        // C2 Phase 4: ASCII strings auto-detect to SeqOneByte. Use the
        // upcasting accessor to compare against u16 units.
        assert_eq!(s.as_utf16_cow().as_ref(), &[104u16, 101, 108, 108, 111][..]);
        assert!(s.is_one_byte());
    }

    #[test]
    fn from_str_emoji() {
        // U+1F600 "😀" — surrogate pair D83D DE00
        let s = JsString::from_str("😀");
        assert_eq!(s.len(), 2); // Two UTF-16 code units
        // Surrogate pair → SeqTwoByte, so as_utf16 is borrowed.
        assert_eq!(s.as_utf16(), &[0xD83Du16, 0xDE00]);
        assert!(!s.is_one_byte());
    }

    #[test]
    fn lone_surrogate_via_utf16() {
        let s = JsString::from_utf16(vec![0xD800]);
        assert_eq!(s.len(), 1);
        assert!(!s.is_well_formed());
        assert!(s.contains_lone_surrogate());
    }

    #[test]
    fn is_well_formed_valid() {
        let s = JsString::from_str("hello 😀");
        assert!(s.is_well_formed());
    }

    #[test]
    fn is_well_formed_lone_high() {
        let s = JsString::from_utf16(vec![0xD800]);
        assert!(!s.is_well_formed());
    }

    #[test]
    fn is_well_formed_lone_low() {
        let s = JsString::from_utf16(vec![0xDC00]);
        assert!(!s.is_well_formed());
    }

    #[test]
    fn is_well_formed_reversed_pair() {
        let s = JsString::from_utf16(vec![0xDC00, 0xD800]);
        assert!(!s.is_well_formed());
    }

    #[test]
    fn is_well_formed_valid_pair() {
        let s = JsString::from_utf16(vec![0xD800, 0xDC00]);
        assert!(s.is_well_formed());
    }

    #[test]
    fn to_well_formed_replaces_lone() {
        let s = JsString::from_utf16(vec![0x61, 0xD800, 0x62]);
        let well = s.to_well_formed();
        assert_eq!(well.as_utf16(), &[0x61, 0xFFFD, 0x62]);
    }

    #[test]
    fn to_well_formed_preserves_valid() {
        let s = JsString::from_utf16(vec![0xD800, 0xDC00]);
        let well = s.to_well_formed();
        assert_eq!(well.as_utf16(), &[0xD800, 0xDC00]);
    }

    #[test]
    fn oxc_decode_lone_surrogate() {
        let encoded = "\u{FFFD}d800";
        let s = JsString::from_oxc_encoded(encoded);
        assert_eq!(s.as_utf16(), &[0xD800]);
        assert!(!s.is_well_formed());
    }

    #[test]
    fn oxc_decode_literal_fffd() {
        let encoded = "\u{FFFD}fffd";
        let s = JsString::from_oxc_encoded(encoded);
        assert_eq!(s.as_utf16(), &[0xFFFD]);
    }

    #[test]
    fn oxc_decode_mixed() {
        let encoded = "abc\u{FFFD}d800def";
        let s = JsString::from_oxc_encoded(encoded);
        assert_eq!(s.len(), 7);
        assert_eq!(s.as_utf16()[3], 0xD800);
        assert!(!s.is_well_formed());
    }

    #[test]
    fn index_of_basic() {
        let s = JsString::from_str("hello world");
        let search = JsString::from_str("world");
        assert_eq!(s.index_of(&search, 0), Some(6));
    }

    #[test]
    fn last_index_of_basic() {
        let s = JsString::from_str("aba");
        let search = JsString::from_str("a");
        assert_eq!(s.last_index_of(&search, 3), Some(2));
    }

    #[test]
    fn equality() {
        let a = JsString::from_str("hello");
        let b = JsString::from_str("hello");
        assert_eq!(a, b);
    }

    #[test]
    fn equality_with_surrogates() {
        let a = JsString::from_utf16(vec![0xD800]);
        let b = JsString::from_utf16(vec![0xD800]);
        assert_eq!(a, b);
    }

    #[test]
    fn inequality_different_surrogates() {
        let a = JsString::from_utf16(vec![0xD800]);
        let b = JsString::from_utf16(vec![0xD801]);
        assert_ne!(a, b);
    }

    #[test]
    fn code_point_at_bmp() {
        let s = JsString::from_str("abc");
        assert_eq!(s.code_point_at(0), Some((0x61, 1)));
    }

    #[test]
    fn code_point_at_surrogate_pair() {
        let s = JsString::from_str("😀");
        assert_eq!(s.code_point_at(0), Some((0x1F600, 2)));
    }

    #[test]
    fn code_point_at_lone_surrogate() {
        let s = JsString::from_utf16(vec![0xD800]);
        assert_eq!(s.code_point_at(0), Some((0xD800, 1)));
    }

    // ── C2: new repr surface ─────────────────────────────────────────────

    #[test]
    fn c2_seq_one_byte_round_trip() {
        let s = JsString::from_one_byte(b"hello".to_vec());
        assert_eq!(s.len(), 5);
        assert!(s.is_one_byte());
        assert!(!s.contains_lone_surrogate());
        assert_eq!(s.as_one_byte(), b"hello");
        // Latin-1 1-byte string upcasts to UTF-8 via char::from
        assert_eq!(s.to_rust_string(), "hello");
    }

    #[test]
    fn c2_seq_one_byte_extended_latin1() {
        // 0xE9 is U+00E9 'é' in Latin-1.
        let s = JsString::from_one_byte(vec![0xE9, b'a']);
        assert_eq!(s.len(), 2);
        assert!(s.is_one_byte());
        assert_eq!(s.code_unit_at(0), Some(0x00E9));
        assert_eq!(s.code_unit_at(1), Some(b'a' as u16));
        assert_eq!(s.to_rust_string(), "éa");
    }

    #[test]
    fn c2_one_byte_eq_two_byte_same_content() {
        let one = JsString::from_one_byte(b"abc".to_vec());
        let two = JsString::from_utf16(vec![b'a' as u16, b'b' as u16, b'c' as u16]);
        assert_eq!(one, two);
        assert_eq!(two, one);
    }

    #[test]
    fn c2_repr_accessor_matches_construction() {
        // Phase 4: ASCII-only strings auto-detect to SeqOneByte.
        let s = JsString::from_str("ok");
        match s.repr() {
            JsStringRepr::SeqOneByte(_) => {}
            _ => panic!("from_str(ASCII) should produce SeqOneByte after Phase 4"),
        }

        // Surrogate pair forces 2-byte storage.
        let two = JsString::from_str("😀");
        match two.repr() {
            JsStringRepr::SeqTwoByte(_) => {}
            _ => panic!("from_str(emoji) should produce SeqTwoByte"),
        }

        let one = JsString::from_one_byte(b"ok".to_vec());
        match one.repr() {
            JsStringRepr::SeqOneByte(_) => {}
            _ => panic!("from_one_byte should produce SeqOneByte"),
        }
    }

    #[test]
    fn c2_lone_surrogate_flag_eager() {
        let s = JsString::from_utf16(vec![0x61, 0xD800, 0x62]);
        assert!(s.contains_lone_surrogate());
        // is_well_formed is now O(1) — just reads the flag.
        assert!(!s.is_well_formed());
    }

    #[test]
    fn c2_clone_preserves_hash_and_flags() {
        let s = JsString::from_str("xyz");
        s.set_cached_hash(42);
        assert_eq!(s.cached_hash(), 42);
        let copy = s.clone();
        assert_eq!(copy.cached_hash(), 42);
        assert_eq!(copy.len(), 3);
    }

    #[test]
    fn c2_max_string_length_constant() {
        // V8 parity: (1<<29) - 24 = 536_870_888.
        assert_eq!(MAX_STRING_LENGTH, (1u32 << 29) - 24);
    }
}
