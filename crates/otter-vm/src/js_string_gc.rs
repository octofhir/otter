//! VM-side API for [`otter_gc::types::string::JsStringGc`].
//!
//! `JsStringGc` is the GC-managed payload struct (defined in `otter-gc`
//! because its trace function needs controlled unsafe access — see
//! `crates/otter-gc/src/types/string.rs`). This module provides the
//! safe, allocator-aware API that the rest of the VM uses to construct,
//! inspect, and transform strings: equivalent to the legacy `JsString`
//! impl in `js_string.rs` but operating on
//! [`GcRef<JsStringGc>`] / [`Local<'gc, JsStringGc>`].
//!
//! The legacy `JsString` type stays in [`crate::js_string`] until every
//! caller migrates over (Phase 2 step 6). During the migration the two
//! coexist; bridge helpers convert in either direction so callers can
//! upgrade incrementally.
//!
//! # Allocation discipline
//!
//! Every constructor that returns a new `Local` takes a
//! [`HandleScope<'gc>`] so the result is rooted automatically. Readers
//! that do not allocate take a [`GcRef<JsStringGc>`] and require no
//! scope.
//!
//! # Spec
//!
//! Spec: <https://tc39.es/ecma262/#sec-ecmascript-language-types-string-type>.

use std::borrow::Cow;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

use otter_gc::gc_ref::{GcRef, type_tag};
use otter_gc::local::{HandleScope, Local};
use otter_gc::typed::OutOfMemory;
use otter_gc::types::string::{
    FLAG_INTERNALIZED, FLAG_LONE_SURROGATE, FLAG_ONE_BYTE, JsStringGc, JsStringRepr,
    MAX_STRING_LENGTH,
};

// ── Constructors ───────────────────────────────────────────────────────────

/// Allocates an empty string.
pub fn empty<'gc>(scope: &mut HandleScope<'gc>) -> Result<Local<'gc, JsStringGc>, OutOfMemory> {
    seq_two_byte(scope, Box::new([]))
}

/// Allocates a `JsStringGc` from a UTF-8 `&str`.
///
/// ASCII fast path: `is_ascii()` strings allocate as
/// [`JsStringRepr::SeqOneByte`] directly from `s.as_bytes()`. Non-ASCII
/// inputs encode to UTF-16 and pass through [`auto_from_units`] which
/// downgrades to Latin-1 when every code unit fits in `0x00..=0xFF`.
pub fn from_str<'gc>(
    scope: &mut HandleScope<'gc>,
    s: &str,
) -> Result<Local<'gc, JsStringGc>, OutOfMemory> {
    if s.is_ascii() {
        return from_one_byte(scope, s.as_bytes().to_vec().into_boxed_slice());
    }
    let units: Vec<u16> = s.encode_utf16().collect();
    auto_from_units(scope, units.into_boxed_slice())
}

/// Allocates a `JsStringGc` from raw WTF-16 code units, auto-selecting
/// the optimal repr (Latin-1 vs UTF-16).
pub fn from_utf16<'gc>(
    scope: &mut HandleScope<'gc>,
    units: impl Into<Box<[u16]>>,
) -> Result<Local<'gc, JsStringGc>, OutOfMemory> {
    auto_from_units(scope, units.into())
}

/// Allocates a `JsStringGc` from a `Vec<u16>` of WTF-16 code units.
pub fn from_utf16_vec<'gc>(
    scope: &mut HandleScope<'gc>,
    units: Vec<u16>,
) -> Result<Local<'gc, JsStringGc>, OutOfMemory> {
    auto_from_units(scope, units.into_boxed_slice())
}

/// Allocates a `SeqOneByte` directly. Caller asserts each byte fits
/// in Latin-1 (every byte is one UTF-16 code unit).
pub fn from_one_byte<'gc>(
    scope: &mut HandleScope<'gc>,
    bytes: impl Into<Box<[u8]>>,
) -> Result<Local<'gc, JsStringGc>, OutOfMemory> {
    let bytes = bytes.into();
    let length = bytes.len() as u32;
    debug_assert!(
        length as usize == bytes.len(),
        "string length exceeds u32 range — caller must enforce MAX_STRING_LENGTH",
    );
    debug_assert!(length <= MAX_STRING_LENGTH);

    scope.alloc_typed(
        type_tag::STRING,
        JsStringGc {
            length,
            hash: AtomicU32::new(0),
            flags: AtomicU8::new(FLAG_ONE_BYTE),
            _padding: [0; 3],
            repr: JsStringRepr::SeqOneByte(bytes),
        },
    )
}

/// Decodes oxc's lone-surrogate encoding (`\u{FFFD}XXXX` → U+XXXX).
pub fn from_oxc_encoded<'gc>(
    scope: &mut HandleScope<'gc>,
    value: &str,
) -> Result<Local<'gc, JsStringGc>, OutOfMemory> {
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
    seq_two_byte(scope, units.into_boxed_slice())
}

/// Auto-detect helper: drops to `SeqOneByte` when every unit ≤ 0xFF,
/// otherwise allocates `SeqTwoByte`.
fn auto_from_units<'gc>(
    scope: &mut HandleScope<'gc>,
    units: Box<[u16]>,
) -> Result<Local<'gc, JsStringGc>, OutOfMemory> {
    if units.iter().all(|u| *u <= 0xFF) {
        let bytes: Box<[u8]> = units
            .iter()
            .map(|u| *u as u8)
            .collect::<Vec<u8>>()
            .into_boxed_slice();
        return from_one_byte(scope, bytes);
    }
    seq_two_byte(scope, units)
}

/// Allocates a `SeqTwoByte` directly. Sets the `FLAG_LONE_SURROGATE`
/// bit when the units contain unpaired surrogates so `is_well_formed`
/// runs in O(1).
fn seq_two_byte<'gc>(
    scope: &mut HandleScope<'gc>,
    units: Box<[u16]>,
) -> Result<Local<'gc, JsStringGc>, OutOfMemory> {
    let length = units.len() as u32;
    debug_assert!(
        length as usize == units.len(),
        "string length exceeds u32 range — caller must enforce MAX_STRING_LENGTH",
    );
    debug_assert!(length <= MAX_STRING_LENGTH);

    let mut flags = 0u8;
    if scan_lone_surrogate(&units) {
        flags |= FLAG_LONE_SURROGATE;
    }

    scope.alloc_typed(
        type_tag::STRING,
        JsStringGc {
            length,
            hash: AtomicU32::new(0),
            flags: AtomicU8::new(flags),
            _padding: [0; 3],
            repr: JsStringRepr::SeqTwoByte(units),
        },
    )
}

// ── Header readers (no allocation) ─────────────────────────────────────────

/// Returns the length in UTF-16 code units (= JS `.length`).
#[inline]
pub fn len(s: GcRef<JsStringGc>) -> usize {
    s.payload().length as usize
}

/// Returns `true` when the string is empty.
#[inline]
pub fn is_empty(s: GcRef<JsStringGc>) -> bool {
    s.payload().length == 0
}

/// Returns `true` when the string fits in Latin-1.
#[inline]
pub fn is_one_byte(s: GcRef<JsStringGc>) -> bool {
    s.payload().flags.load(Ordering::Relaxed) & FLAG_ONE_BYTE != 0
}

/// Returns `true` when the string is the canonical interned copy.
#[inline]
pub fn is_internalized(s: GcRef<JsStringGc>) -> bool {
    s.payload().flags.load(Ordering::Relaxed) & FLAG_INTERNALIZED != 0
}

/// Marks the string as the canonical interned copy. Idempotent.
#[inline]
pub fn mark_internalized(s: GcRef<JsStringGc>) {
    s.payload()
        .flags
        .fetch_or(FLAG_INTERNALIZED, Ordering::Relaxed);
}

/// Returns `true` when the string contains an unpaired surrogate.
#[inline]
pub fn contains_lone_surrogate(s: GcRef<JsStringGc>) -> bool {
    s.payload().flags.load(Ordering::Relaxed) & FLAG_LONE_SURROGATE != 0
}

/// Returns the cached FNV-1a hash, or `0` when not yet computed.
#[inline]
pub fn cached_hash(s: GcRef<JsStringGc>) -> u32 {
    s.payload().hash.load(Ordering::Relaxed)
}

/// Stores the cached hash. `value` must not be `0` (the sentinel).
#[inline]
pub fn set_cached_hash(s: GcRef<JsStringGc>, value: u32) {
    debug_assert_ne!(value, 0, "0 is reserved as the cached_hash sentinel");
    s.payload().hash.store(value, Ordering::Relaxed);
}

/// Returns `true` when the string is well-formed UTF-16 (no lone
/// surrogates). O(1) — the flag is set at construction.
#[inline]
pub fn is_well_formed(s: GcRef<JsStringGc>) -> bool {
    !contains_lone_surrogate(s)
}

// ── Direct unit access (panics on non-flat) ────────────────────────────────

/// Returns WTF-16 code units. Panics on non-flat representations
/// (`Cons` / `Sliced` / `Thin` need flatten first — that lives in
/// the in-progress `flatten.rs` of the GC migration).
pub fn as_utf16_cow(s: GcRef<JsStringGc>) -> Cow<'static, [u16]> {
    // SAFETY of returning `Cow<'static, _>`: the borrow lifetime is
    // tied to the heap which outlives every safepoint between calls.
    // Strictly, this should be a lifetime tied to the scope; we use
    // `'static` as a temporary measure during migration. Phase 2 step 6
    // tightens this once every caller is migrated. The single-mutator
    // model + handle-stack rooting means the underlying bytes cannot
    // move while a non-allocating caller holds the cow.
    let payload = s.payload();
    match &payload.repr {
        JsStringRepr::SeqTwoByte(u) => {
            // Borrow then upcast to 'static via slice copy when callers
            // hold the result across allocs. For now we copy: cheap
            // for the common case, correct under all GC scenarios.
            Cow::Owned(u.to_vec())
        }
        JsStringRepr::SeqOneByte(b) => Cow::Owned(b.iter().map(|byte| u16::from(*byte)).collect()),
        _ => panic!("as_utf16_cow on non-flat repr — caller must flatten first"),
    }
}

/// Returns the WTF-16 code unit at `index` — only valid on flat reprs.
#[inline]
pub fn code_unit_at(s: GcRef<JsStringGc>, index: usize) -> Option<u16> {
    match &s.payload().repr {
        JsStringRepr::SeqTwoByte(u) => u.get(index).copied(),
        JsStringRepr::SeqOneByte(b) => b.get(index).map(|byte| u16::from(*byte)),
        _ => panic!("code_unit_at on non-flat repr"),
    }
}

/// Returns the Unicode code point starting at `index`. Decodes
/// surrogate pairs.
pub fn code_point_at(s: GcRef<JsStringGc>, index: usize) -> Option<(u32, usize)> {
    let lead = code_unit_at(s, index)?;
    if (0xD800..=0xDBFF).contains(&lead)
        && let Some(trail) = code_unit_at(s, index + 1)
        && (0xDC00..=0xDFFF).contains(&trail)
    {
        let cp = 0x10000 + ((lead as u32 - 0xD800) << 10) + (trail as u32 - 0xDC00);
        return Some((cp, 2));
    }
    Some((lead as u32, 1))
}

/// Converts to a Rust `String`, lossily replacing lone surrogates with
/// U+FFFD. Panics on non-flat reprs.
pub fn to_rust_string(s: GcRef<JsStringGc>) -> String {
    match &s.payload().repr {
        JsStringRepr::SeqTwoByte(u) => String::from_utf16_lossy(u),
        JsStringRepr::SeqOneByte(b) => {
            let mut out = String::with_capacity(b.len());
            for byte in b.iter() {
                out.push(char::from(*byte));
            }
            out
        }
        _ => panic!("to_rust_string on non-flat repr"),
    }
}

/// Converts to a Rust `String` only when the string is valid UTF-16
/// (no lone surrogates).
pub fn to_rust_string_lossless(s: GcRef<JsStringGc>) -> Option<String> {
    if contains_lone_surrogate(s) {
        return None;
    }
    Some(to_rust_string(s))
}

// ── Allocators (return new strings) ────────────────────────────────────────

/// Returns a new string with lone surrogates replaced by U+FFFD.
///
/// §22.1.3.33 String.prototype.toWellFormed
pub fn to_well_formed<'gc>(
    scope: &mut HandleScope<'gc>,
    s: GcRef<JsStringGc>,
) -> Result<Local<'gc, JsStringGc>, OutOfMemory> {
    if !contains_lone_surrogate(s) {
        // No surrogates → just re-allocate from the same units.
        return clone_flat(scope, s);
    }
    let cow = as_utf16_cow(s);
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
    seq_two_byte(scope, result.into_boxed_slice())
}

/// Allocates a fresh copy of the string. Used in the no-op branch of
/// `to_well_formed` and as a generic "duplicate this" helper. Only
/// valid on flat reprs.
fn clone_flat<'gc>(
    scope: &mut HandleScope<'gc>,
    s: GcRef<JsStringGc>,
) -> Result<Local<'gc, JsStringGc>, OutOfMemory> {
    match &s.payload().repr {
        JsStringRepr::SeqOneByte(b) => from_one_byte(scope, b.clone()),
        JsStringRepr::SeqTwoByte(u) => seq_two_byte(scope, u.clone()),
        _ => panic!("clone_flat on non-flat repr"),
    }
}

/// §22.1.3.21 String.prototype.substring(start, end)
pub fn substring<'gc>(
    scope: &mut HandleScope<'gc>,
    s: GcRef<JsStringGc>,
    start: usize,
    end: usize,
) -> Result<Local<'gc, JsStringGc>, OutOfMemory> {
    let cow = as_utf16_cow(s);
    let units: &[u16] = &cow;
    let start = start.min(units.len());
    let end = end.min(units.len());
    let (start, end) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    auto_from_units(scope, units[start..end].into())
}

/// §22.1.3.20 String.prototype.slice(start, end)
pub fn slice<'gc>(
    scope: &mut HandleScope<'gc>,
    s: GcRef<JsStringGc>,
    start: usize,
    end: usize,
) -> Result<Local<'gc, JsStringGc>, OutOfMemory> {
    let cow = as_utf16_cow(s);
    let units: &[u16] = &cow;
    if start >= end || start >= units.len() {
        return empty(scope);
    }
    let end = end.min(units.len());
    auto_from_units(scope, units[start..end].into())
}

/// Eagerly concatenates two strings.
pub fn concat<'gc>(
    scope: &mut HandleScope<'gc>,
    lhs: GcRef<JsStringGc>,
    rhs: GcRef<JsStringGc>,
) -> Result<Local<'gc, JsStringGc>, OutOfMemory> {
    let lhs_cow = as_utf16_cow(lhs);
    let rhs_cow = as_utf16_cow(rhs);
    let l: &[u16] = &lhs_cow;
    let r: &[u16] = &rhs_cow;
    let mut units = Vec::with_capacity(l.len() + r.len());
    units.extend_from_slice(l);
    units.extend_from_slice(r);
    auto_from_units(scope, units.into_boxed_slice())
}

/// §22.1.3.17 String.prototype.repeat(count)
pub fn repeat<'gc>(
    scope: &mut HandleScope<'gc>,
    s: GcRef<JsStringGc>,
    count: usize,
) -> Result<Local<'gc, JsStringGc>, OutOfMemory> {
    let cow = as_utf16_cow(s);
    let units: &[u16] = &cow;
    let mut out = Vec::with_capacity(units.len().saturating_mul(count));
    for _ in 0..count {
        out.extend_from_slice(units);
    }
    auto_from_units(scope, out.into_boxed_slice())
}

/// §22.1.3.27 String.prototype.toLowerCase
pub fn to_lowercase<'gc>(
    scope: &mut HandleScope<'gc>,
    s: GcRef<JsStringGc>,
) -> Result<Local<'gc, JsStringGc>, OutOfMemory> {
    let rust = to_rust_string(s);
    from_str(scope, &rust.to_lowercase())
}

/// §22.1.3.29 String.prototype.toUpperCase
pub fn to_uppercase<'gc>(
    scope: &mut HandleScope<'gc>,
    s: GcRef<JsStringGc>,
) -> Result<Local<'gc, JsStringGc>, OutOfMemory> {
    let rust = to_rust_string(s);
    from_str(scope, &rust.to_uppercase())
}

/// §22.1.3.32 String.prototype.trim
pub fn trim<'gc>(
    scope: &mut HandleScope<'gc>,
    s: GcRef<JsStringGc>,
) -> Result<Local<'gc, JsStringGc>, OutOfMemory> {
    let rust = to_rust_string(s);
    from_str(scope, rust.trim())
}

/// §22.1.3.34 String.prototype.trimStart
pub fn trim_start<'gc>(
    scope: &mut HandleScope<'gc>,
    s: GcRef<JsStringGc>,
) -> Result<Local<'gc, JsStringGc>, OutOfMemory> {
    let rust = to_rust_string(s);
    from_str(scope, rust.trim_start())
}

/// §22.1.3.31 String.prototype.trimEnd
pub fn trim_end<'gc>(
    scope: &mut HandleScope<'gc>,
    s: GcRef<JsStringGc>,
) -> Result<Local<'gc, JsStringGc>, OutOfMemory> {
    let rust = to_rust_string(s);
    from_str(scope, rust.trim_end())
}

// ── Search ─────────────────────────────────────────────────────────────────

/// §22.1.3.9 String.prototype.indexOf(searchString, position)
pub fn index_of(
    haystack: GcRef<JsStringGc>,
    needle: GcRef<JsStringGc>,
    from_index: usize,
) -> Option<usize> {
    let h_cow = as_utf16_cow(haystack);
    let n_cow = as_utf16_cow(needle);
    let h: &[u16] = &h_cow;
    let n: &[u16] = &n_cow;
    if n.is_empty() {
        return Some(from_index.min(h.len()));
    }
    if from_index + n.len() > h.len() {
        return None;
    }
    for i in from_index..=(h.len() - n.len()) {
        if h[i..i + n.len()] == *n {
            return Some(i);
        }
    }
    None
}

/// §22.1.3.10 String.prototype.lastIndexOf
pub fn last_index_of(
    haystack: GcRef<JsStringGc>,
    needle: GcRef<JsStringGc>,
    from_index: usize,
) -> Option<usize> {
    let h_cow = as_utf16_cow(haystack);
    let n_cow = as_utf16_cow(needle);
    let h: &[u16] = &h_cow;
    let n: &[u16] = &n_cow;
    if n.is_empty() {
        return Some(from_index.min(h.len()));
    }
    if n.len() > h.len() {
        return None;
    }
    let max_start = from_index.min(h.len() - n.len());
    for i in (0..=max_start).rev() {
        if h[i..i + n.len()] == *n {
            return Some(i);
        }
    }
    None
}

pub fn starts_with(s: GcRef<JsStringGc>, prefix: GcRef<JsStringGc>) -> bool {
    let s_cow = as_utf16_cow(s);
    let p_cow = as_utf16_cow(prefix);
    let s_slice: &[u16] = &s_cow;
    let p_slice: &[u16] = &p_cow;
    s_slice.starts_with(p_slice)
}

pub fn ends_with(s: GcRef<JsStringGc>, suffix: GcRef<JsStringGc>) -> bool {
    let s_cow = as_utf16_cow(s);
    let q_cow = as_utf16_cow(suffix);
    let s_slice: &[u16] = &s_cow;
    let q_slice: &[u16] = &q_cow;
    s_slice.ends_with(q_slice)
}

pub fn contains(s: GcRef<JsStringGc>, needle: GcRef<JsStringGc>) -> bool {
    index_of(s, needle, 0).is_some()
}

// ── Equality / hashing ─────────────────────────────────────────────────────

/// Content equality. Pointer-equal references short-circuit. Otherwise
/// compares lengths and units.
pub fn equals(a: GcRef<JsStringGc>, b: GcRef<JsStringGc>) -> bool {
    if a.ptr_eq(&b) {
        return true;
    }
    let pa = a.payload();
    let pb = b.payload();
    if pa.length != pb.length {
        return false;
    }
    match (&pa.repr, &pb.repr) {
        (JsStringRepr::SeqTwoByte(x), JsStringRepr::SeqTwoByte(y)) => x == y,
        (JsStringRepr::SeqOneByte(x), JsStringRepr::SeqOneByte(y)) => x == y,
        (JsStringRepr::SeqOneByte(x), JsStringRepr::SeqTwoByte(y)) => one_byte_eq_two_byte(x, y),
        (JsStringRepr::SeqTwoByte(x), JsStringRepr::SeqOneByte(y)) => one_byte_eq_two_byte(y, x),
        _ => panic!("equals called with non-flat repr — flatten first"),
    }
}

/// Compares the string against a Rust UTF-8 `&str`.
pub fn equals_str(s: GcRef<JsStringGc>, other: &str) -> bool {
    let payload = s.payload();
    match &payload.repr {
        JsStringRepr::SeqOneByte(_) | JsStringRepr::SeqTwoByte(_) => {
            let other_units: Vec<u16> = other.encode_utf16().collect();
            if other_units.len() as u32 != payload.length {
                return false;
            }
            match &payload.repr {
                JsStringRepr::SeqTwoByte(u) => *u.as_ref() == *other_units,
                JsStringRepr::SeqOneByte(b) => one_byte_eq_two_byte(b, &other_units),
                _ => unreachable!(),
            }
        }
        _ => panic!("equals_str on non-flat repr"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use otter_gc::heap::{GcConfig, GcHeap};
    use otter_gc::types;

    fn fresh_heap() -> GcHeap {
        let mut heap = GcHeap::new(GcConfig {
            young_gen_size: 1024 * 1024,
            old_gen_threshold: 512 * 1024,
            ..GcConfig::default()
        });
        types::register_all(&mut heap);
        heap
    }

    #[test]
    fn from_str_ascii_uses_one_byte() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        let s = from_str(&mut scope, "hello").expect("alloc");
        assert_eq!(len(s.as_ref()), 5);
        assert!(is_one_byte(s.as_ref()));
        assert_eq!(to_rust_string(s.as_ref()), "hello");
    }

    #[test]
    fn from_str_emoji_uses_two_byte_with_surrogate_pair() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        // "😀" = U+1F600 = surrogate pair D83D DE00.
        let s = from_str(&mut scope, "😀").expect("alloc");
        assert_eq!(len(s.as_ref()), 2);
        assert!(!is_one_byte(s.as_ref()));
        assert!(is_well_formed(s.as_ref()));
        assert_eq!(code_unit_at(s.as_ref(), 0), Some(0xD83D));
        assert_eq!(code_unit_at(s.as_ref(), 1), Some(0xDE00));
        assert_eq!(code_point_at(s.as_ref(), 0), Some((0x1F600, 2)));
    }

    #[test]
    fn from_utf16_with_lone_surrogate_sets_flag() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        let s = from_utf16_vec(&mut scope, vec![0xD83D]).expect("alloc"); // unpaired high surrogate
        assert_eq!(len(s.as_ref()), 1);
        assert!(contains_lone_surrogate(s.as_ref()));
        assert!(!is_well_formed(s.as_ref()));
    }

    #[test]
    fn concat_combines_two_strings() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        let a = from_str(&mut scope, "foo").expect("a");
        let b = from_str(&mut scope, "bar").expect("b");
        let c = concat(&mut scope, a.as_ref(), b.as_ref()).expect("concat");
        assert_eq!(to_rust_string(c.as_ref()), "foobar");
        assert!(is_one_byte(c.as_ref()));
    }

    #[test]
    fn slice_returns_substring() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        let s = from_str(&mut scope, "abcdef").expect("alloc");
        let sub = slice(&mut scope, s.as_ref(), 1, 4).expect("slice");
        assert_eq!(to_rust_string(sub.as_ref()), "bcd");
    }

    #[test]
    fn substring_swaps_inverted_indices() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        let s = from_str(&mut scope, "abcdef").expect("alloc");
        let sub = substring(&mut scope, s.as_ref(), 4, 1).expect("substring");
        assert_eq!(to_rust_string(sub.as_ref()), "bcd");
    }

    #[test]
    fn repeat_replicates_units() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        let s = from_str(&mut scope, "ab").expect("alloc");
        let r = repeat(&mut scope, s.as_ref(), 3).expect("repeat");
        assert_eq!(to_rust_string(r.as_ref()), "ababab");
    }

    #[test]
    fn case_conversion_round_trips_ascii() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        let s = from_str(&mut scope, "Hello").expect("alloc");
        let lower = to_lowercase(&mut scope, s.as_ref()).expect("lower");
        let upper = to_uppercase(&mut scope, s.as_ref()).expect("upper");
        assert_eq!(to_rust_string(lower.as_ref()), "hello");
        assert_eq!(to_rust_string(upper.as_ref()), "HELLO");
    }

    #[test]
    fn trim_removes_whitespace() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        let s = from_str(&mut scope, "  hi  ").expect("alloc");
        let t = trim(&mut scope, s.as_ref()).expect("trim");
        assert_eq!(to_rust_string(t.as_ref()), "hi");
    }

    #[test]
    fn search_index_of_basic() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        let h = from_str(&mut scope, "abcabc").expect("h");
        let n = from_str(&mut scope, "bc").expect("n");
        assert_eq!(index_of(h.as_ref(), n.as_ref(), 0), Some(1));
        assert_eq!(index_of(h.as_ref(), n.as_ref(), 2), Some(4));
        assert_eq!(last_index_of(h.as_ref(), n.as_ref(), 6), Some(4));
    }

    #[test]
    fn equals_and_equals_str() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        let a = from_str(&mut scope, "hello").expect("a");
        let b = from_str(&mut scope, "hello").expect("b");
        let c = from_str(&mut scope, "world").expect("c");
        assert!(equals(a.as_ref(), b.as_ref())); // distinct allocations, same content
        assert!(!equals(a.as_ref(), c.as_ref()));
        assert!(equals_str(a.as_ref(), "hello"));
        assert!(!equals_str(a.as_ref(), "world"));
    }

    #[test]
    fn empty_returns_zero_length() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        let e = empty(&mut scope).expect("empty");
        assert_eq!(len(e.as_ref()), 0);
        assert!(is_empty(e.as_ref()));
    }

    #[test]
    fn from_oxc_encoded_decodes_lone_surrogate_marker() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        // oxc encodes a lone surrogate D83D as the literal U+FFFD followed by
        // four hex chars "d83d".
        let s = from_oxc_encoded(&mut scope, "\u{FFFD}d83d").expect("alloc");
        assert_eq!(len(s.as_ref()), 1);
        assert!(contains_lone_surrogate(s.as_ref()));
        assert_eq!(code_unit_at(s.as_ref(), 0), Some(0xD83D));
    }

    #[test]
    fn starts_with_and_ends_with() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        let s = from_str(&mut scope, "abcdef").expect("s");
        let p = from_str(&mut scope, "abc").expect("p");
        let q = from_str(&mut scope, "def").expect("q");
        assert!(starts_with(s.as_ref(), p.as_ref()));
        assert!(ends_with(s.as_ref(), q.as_ref()));
        assert!(!starts_with(s.as_ref(), q.as_ref()));
    }

    #[test]
    fn flags_round_trip() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        let s = from_str(&mut scope, "x").expect("s");
        let r = s.as_ref();
        assert!(is_one_byte(r));
        assert!(!is_internalized(r));
        mark_internalized(r);
        assert!(is_internalized(r));
        assert_eq!(cached_hash(r), 0);
        set_cached_hash(r, 0xCAFE);
        assert_eq!(cached_hash(r), 0xCAFE);
    }

    #[test]
    fn to_well_formed_replaces_lone_surrogates() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        let s = from_utf16_vec(&mut scope, vec![0x41, 0xD83D, 0x42]).expect("s"); // A, lone, B
        let cleaned = to_well_formed(&mut scope, s.as_ref()).expect("clean");
        assert_eq!(to_rust_string(cleaned.as_ref()), "A\u{FFFD}B");
    }
}
