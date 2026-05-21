//! NaN-box tag layout for [`crate::value::Value`].
//!
//! # Encoding
//!
//! Every `Value` is a single `u64`. We use the IEEE-754 quiet-NaN payload
//! region (`0x7FF8_0000_0000_0000..=0xFFFF_FFFF_FFFF_FFFF`) to store typed,
//! non-double payloads. Doubles that are not NaN occupy the rest of the
//! `u64` space verbatim; the single canonical NaN (`f64::NAN`) is
//! encoded as `CANONICAL_NAN`.
//!
//! The high 16 bits select the tag. The low 48 bits carry the payload.
//!
//! | High 16 bits     | Meaning                                                       |
//! |------------------|----------------------------------------------------------------|
//! | not `0x7FFx`     | IEEE-754 double, used verbatim via [`f64::from_bits`]          |
//! | `0x7FF8`         | canonical quiet NaN = `Number::Double(NaN)`                    |
//! | `0x7FF9`         | INT32 immediate (low 32 bits = signed i32 payload)             |
//! | `0x7FFA`         | SPECIAL immediate (low 4 bits select Undef/Null/Hole/Bool kind)|
//! | `0x7FFB`         | FUNCTION_ID immediate (low 32 bits = bytecode function id)     |
//! | `0x7FFC`         | TAG_PTR_OBJECT   (low 32 bits = `Gc<…>` offset, type-tag disc) |
//! | `0x7FFD`         | TAG_PTR_STRING   (low 32 bits = `Gc<JsStringBody>` offset)     |
//! | `0x7FFE`         | TAG_PTR_FUNCTION (low 32 bits = callable body `Gc<…>` offset)  |
//! | `0x7FFF`         | TAG_PTR_OTHER    (low 32 bits = misc body `Gc<…>` offset)      |
//!
//! Pointer payloads carry the 32-bit GC compressed offset
//! ([`otter_gc::Gc::offset`]); type discrimination on `TAG_PTR_OBJECT`
//! and `TAG_PTR_OTHER` happens through [`otter_gc::header::GcHeader::type_tag`]
//! lookup, matching MEMORY.md's `0x7FFC/0x7FFD/0x7FFE/0x7FFF` scheme.
//!
//! # Invariants
//!
//! - Any incoming `f64::NAN` is normalized to [`CANONICAL_NAN`] before
//!   storage.
//! - Pointer tags must always carry a 32-bit GC offset payload — bits
//!   32..48 are reserved and must be zero so `(value.0 & 0xFFFF_FFFF) as u32`
//!   round-trips through [`otter_gc::Gc::from_offset`].
//! - The `SPECIAL` kind discriminant occupies the low 4 bits; the rest
//!   of the payload must be zero so equality compares structurally.
//!
//! # Spec
//!
//! - ECMA-262 §6.1 ECMAScript Language Types
//! - ECMA-262 §6.1.6.1 The Number Type (NaN canonicalisation)

/// High-16-bit base of the IEEE-754 positive quiet-NaN range.
pub const QNAN_BASE: u64 = 0x7FF8_0000_0000_0000;

/// Canonical quiet NaN — the single bit pattern used for any
/// `Number(NaN)` value. Constants from `f64::NAN` would also work but
/// we pin the exact pattern so cross-platform behaviour stays stable.
pub const CANONICAL_NAN: u64 = QNAN_BASE;

/// Canonical quiet-NaN high-16-bit pattern.
pub const TAG_NAN: u16 = 0x7FF8;
/// 32-bit integer immediate tag (payload = signed `i32` in low 32 bits).
pub const TAG_INT32: u16 = 0x7FF9;
/// Special immediate tag (Undefined / Null / Hole / Boolean).
pub const TAG_SPECIAL: u16 = 0x7FFA;
/// Bytecode-function-id immediate tag (no closure captured).
pub const TAG_FUNCTION_ID: u16 = 0x7FFB;
/// Object-family pointer tag (object, array, map, set, weak*, promise,
/// proxy, regexp, typed/buffer/data-view, temporal, intl, iterator,
/// generator, finalization-registry).
pub const TAG_PTR_OBJECT: u16 = 0x7FFC;
/// String body pointer tag.
pub const TAG_PTR_STRING: u16 = 0x7FFD;
/// Callable body pointer tag (closure, bound, native, class).
pub const TAG_PTR_FUNCTION: u16 = 0x7FFE;
/// Miscellaneous primitive body pointer tag (symbol, bigint).
pub const TAG_PTR_OTHER: u16 = 0x7FFF;

/// First non-double tag. Any `Value` whose top 16 bits are `>= TAG_INT32`
/// is a tagged immediate or pointer, *unless* the bits are exactly
/// [`CANONICAL_NAN`] which represents `Number(NaN)`.
pub const FIRST_NONDOUBLE_TAG: u16 = TAG_INT32;

/// `SPECIAL` sub-kind: `undefined`.
pub const SPECIAL_UNDEFINED: u64 = 0;
/// `SPECIAL` sub-kind: `null`.
pub const SPECIAL_NULL: u64 = 1;
/// `SPECIAL` sub-kind: internal array hole sentinel.
pub const SPECIAL_HOLE: u64 = 2;
/// `SPECIAL` sub-kind: `false`.
pub const SPECIAL_FALSE: u64 = 3;
/// `SPECIAL` sub-kind: `true`.
pub const SPECIAL_TRUE: u64 = 4;

/// Build a u64 from a 16-bit tag and a 48-bit payload.
#[inline(always)]
pub const fn pack(tag: u16, payload48: u64) -> u64 {
    debug_assert!(payload48 <= 0x0000_FFFF_FFFF_FFFF);
    ((tag as u64) << 48) | (payload48 & 0x0000_FFFF_FFFF_FFFF)
}

/// Extract the 16-bit tag from a `Value` bit pattern.
#[inline(always)]
pub const fn top_tag(bits: u64) -> u16 {
    (bits >> 48) as u16
}

/// Extract the low-48-bit payload.
#[inline(always)]
pub const fn payload48(bits: u64) -> u64 {
    bits & 0x0000_FFFF_FFFF_FFFF
}

/// Extract the low-32-bit payload (used for pointers, int32, function-id).
#[inline(always)]
pub const fn payload32(bits: u64) -> u32 {
    bits as u32
}

/// `true` if the bit pattern is a double (including canonical NaN,
/// ±Infinity, ±0).
///
/// Non-doubles occupy the contiguous high-tag window
/// `[TAG_INT32 ..= TAG_PTR_OTHER]` (`0x7FF9..=0x7FFF`). Every other
/// 16-bit prefix — positive finite/inf, the canonical NaN at
/// `0x7FF8`, and the entire negative half `0x8000..=0xFFFF` — is a
/// valid IEEE-754 double.
#[inline(always)]
pub const fn is_double_bits(bits: u64) -> bool {
    let tag = top_tag(bits);
    tag < TAG_INT32 || tag > TAG_PTR_OTHER
}
