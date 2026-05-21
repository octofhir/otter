//! Eight-byte tagged JavaScript runtime value.
//!
//! `Value` is a [`Copy`] `#[repr(transparent)] u64` using NaN-box encoding.
//! Every register slot, every property store, every argument vector is
//! exactly 8 bytes — no enum discriminant, no `Rc`/`Arc` refcount on
//! the hot path. See [`tag`] for the bit-layout contract.
//!
//! # Construction surface
//!
//! - Immediates: [`Value::undefined`], [`Value::null`], [`Value::hole`],
//!   [`Value::boolean`], [`Value::number_i32`], [`Value::number_f64`],
//!   [`Value::number`], [`Value::function_id`].
//! - Heap-backed: every JS object family converts through a single
//!   compressed 32-bit GC offset, packed under one of the four
//!   `TAG_PTR_*` tags. Per-type wrapper structs (`JsObject`, `JsArray`,
//!   …) call [`Value::from_object_gc`] / [`Value::from_string_gc`] /
//!   [`Value::from_function_gc`] / [`Value::from_other_gc`] on their
//!   own raw offset. Type discrimination back to the original wrapper
//!   goes through [`otter_gc::header::GcHeader::type_tag`].
//!
//! # Inspection surface
//!
//! Use the typed accessors (`as_i32`, `as_boolean`, `as_number`,
//! `as_raw_gc`, `read_gc_type_tag`, …) and predicates (`is_undefined`,
//! `is_callable`, …). Pattern matching against the legacy
//! `Value::Object(…)` enum form is unsupported — call sites move to
//! accessors.
//!
//! # Invariants
//!
//! - `size_of::<Value>() == 8` and `align_of::<Value>() == 8` (static
//!   asserts below).
//! - `Value::default()` is [`Value::UNDEFINED`].
//! - Every incoming NaN is canonicalised to [`tag::CANONICAL_NAN`].
//! - Pointer payloads always store the 32-bit GC offset returned by
//!   [`otter_gc::Gc::offset`]; bits 32..48 stay zero.
//! - GC type discrimination for pointer tags goes through
//!   [`otter_gc::header::GcHeader::type_tag`], not the NaN-box tag —
//!   the four pointer tags only select the *family* (object-like,
//!   string, callable, other).
//!
//! # See also
//!
//! - <https://tc39.es/ecma262/#sec-ecmascript-language-types>
//! - `docs/architecture-refactor-plan-2026-05.md` Phase 1.1
//! - `docs/architecture-audit-2026-05.md` §1 (value model audit)

pub mod tag;

use crate::NumberValue;

use tag::*;

/// Eight-byte tagged JavaScript value.
///
/// `#[repr(transparent)] u64`. See module docs for the encoding
/// contract.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Value(u64);

// ---------------------------------------------------------------------------
// Layout guards (Phase 1.1 — load-bearing).
// ---------------------------------------------------------------------------
const _: () = {
    if std::mem::size_of::<Value>() != 8 {
        panic!("Value must be exactly 8 bytes");
    }
    if std::mem::align_of::<Value>() != 8 {
        panic!("Value must be 8-byte aligned");
    }
};

/// Coarse value family used by [`Value::kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// IEEE-754 double (including canonical NaN, ±Infinity, ±0).
    Number,
    /// 32-bit small integer fast path.
    Int32,
    /// Special immediate (Undefined, Null, Hole, Boolean).
    Special,
    /// Bytecode function id (no closure captured).
    FunctionId,
    /// Object-like reference: ordinary object, array, map, set,
    /// weak*, typed/buffer/data-view, iterator, generator, promise,
    /// proxy, regexp, temporal, intl, finalization-registry.
    PtrObject,
    /// String body reference.
    PtrString,
    /// Callable body reference: closure, bound, native function,
    /// or class-constructor wrapper.
    PtrFunction,
    /// Misc body reference: symbol, bigint.
    PtrOther,
}

impl Value {
    // -----------------------------------------------------------------------
    // Canonical immediates
    // -----------------------------------------------------------------------

    /// `undefined`.
    pub const UNDEFINED: Value = Value(pack(TAG_SPECIAL, SPECIAL_UNDEFINED));
    /// `null`.
    pub const NULL: Value = Value(pack(TAG_SPECIAL, SPECIAL_NULL));
    /// Internal "array hole" sentinel — never observed by user code.
    pub const HOLE: Value = Value(pack(TAG_SPECIAL, SPECIAL_HOLE));
    /// `false`.
    pub const FALSE: Value = Value(pack(TAG_SPECIAL, SPECIAL_FALSE));
    /// `true`.
    pub const TRUE: Value = Value(pack(TAG_SPECIAL, SPECIAL_TRUE));

    // -----------------------------------------------------------------------
    // Bit-level access (audited helpers; not part of the public stable
    // surface).
    // -----------------------------------------------------------------------

    /// Construct from raw bits. **Caller** must uphold the encoding
    /// contract in [`tag`].
    #[doc(hidden)]
    #[inline(always)]
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// Raw bit pattern. Diagnostic only.
    #[doc(hidden)]
    #[inline(always)]
    pub const fn to_bits(self) -> u64 {
        self.0
    }

    // -----------------------------------------------------------------------
    // Constructors — immediates
    // -----------------------------------------------------------------------

    /// `undefined`.
    #[inline]
    #[must_use]
    pub const fn undefined() -> Self {
        Self::UNDEFINED
    }

    /// `null`.
    #[inline]
    #[must_use]
    pub const fn null() -> Self {
        Self::NULL
    }

    /// Internal "array hole" sentinel.
    #[inline]
    #[must_use]
    pub const fn hole() -> Self {
        Self::HOLE
    }

    /// `true` / `false`.
    #[inline]
    #[must_use]
    pub const fn boolean(b: bool) -> Self {
        if b { Self::TRUE } else { Self::FALSE }
    }

    /// Number from a 32-bit integer fast path.
    #[inline]
    #[must_use]
    pub const fn number_i32(n: i32) -> Self {
        Self(pack(TAG_INT32, n as u32 as u64))
    }

    /// Number from an `f64`. NaNs are canonicalised; integer-valued
    /// finite doubles are *not* automatically demoted to int32 — pass
    /// through [`NumberValue::canonicalize`] first if you want that.
    #[inline]
    #[must_use]
    pub fn number_f64(d: f64) -> Self {
        if d.is_nan() {
            return Self(CANONICAL_NAN);
        }
        Self(d.to_bits())
    }

    /// Number from the runtime [`NumberValue`] view, preferring the
    /// int32 fast path.
    #[inline]
    #[must_use]
    pub fn number(n: NumberValue) -> Self {
        match n {
            NumberValue::Smi(i) => Self::number_i32(i),
            NumberValue::Double(d) => Self::number_f64(d),
        }
    }

    /// Bytecode function reference (closure-less).
    #[inline]
    #[must_use]
    pub const fn function_id(id: u32) -> Self {
        Self(pack(TAG_FUNCTION_ID, id as u64))
    }

    // -----------------------------------------------------------------------
    // Constructors — pointer-tagged heap handles
    //
    // These take a `RawGc` (32-bit compressed offset) and the type-
    // family tag. Per-type wrappers (`JsObject`, `JsArray`, `JsString`,
    // …) construct values through these helpers using their already
    // GC-backed handle.
    // -----------------------------------------------------------------------

    /// Build an object-family value (`TAG_PTR_OBJECT`). The caller
    /// guarantees the body's `GcHeader::type_tag` belongs to the
    /// object family.
    #[inline]
    #[must_use]
    pub fn from_object_gc(raw: otter_gc::raw::RawGc) -> Self {
        Self(pack(TAG_PTR_OBJECT, raw.0 as u64))
    }

    /// Build a string-family value (`TAG_PTR_STRING`).
    #[inline]
    #[must_use]
    pub fn from_string_gc(raw: otter_gc::raw::RawGc) -> Self {
        Self(pack(TAG_PTR_STRING, raw.0 as u64))
    }

    /// Build a callable-family value (`TAG_PTR_FUNCTION`).
    #[inline]
    #[must_use]
    pub fn from_function_gc(raw: otter_gc::raw::RawGc) -> Self {
        Self(pack(TAG_PTR_FUNCTION, raw.0 as u64))
    }

    /// Build a "other primitive" value (`TAG_PTR_OTHER`) — symbols,
    /// bigints.
    #[inline]
    #[must_use]
    pub fn from_other_gc(raw: otter_gc::raw::RawGc) -> Self {
        Self(pack(TAG_PTR_OTHER, raw.0 as u64))
    }

    // -----------------------------------------------------------------------
    // Coarse classification.
    // -----------------------------------------------------------------------

    /// Coarse value family. See [`ValueKind`].
    #[inline]
    #[must_use]
    pub fn kind(self) -> ValueKind {
        if is_double_bits(self.0) {
            return ValueKind::Number;
        }
        match top_tag(self.0) {
            TAG_INT32 => ValueKind::Int32,
            TAG_SPECIAL => ValueKind::Special,
            TAG_FUNCTION_ID => ValueKind::FunctionId,
            TAG_PTR_OBJECT => ValueKind::PtrObject,
            TAG_PTR_STRING => ValueKind::PtrString,
            TAG_PTR_FUNCTION => ValueKind::PtrFunction,
            TAG_PTR_OTHER => ValueKind::PtrOther,
            // Folded into double / unreachable by construction.
            _ => ValueKind::Number,
        }
    }

    // -----------------------------------------------------------------------
    // Predicates
    // -----------------------------------------------------------------------

    /// `undefined`.
    #[inline]
    #[must_use]
    pub const fn is_undefined(self) -> bool {
        self.0 == Self::UNDEFINED.0
    }

    /// `null`.
    #[inline]
    #[must_use]
    pub const fn is_null(self) -> bool {
        self.0 == Self::NULL.0
    }

    /// Internal array-hole sentinel.
    #[inline]
    #[must_use]
    pub const fn is_hole(self) -> bool {
        self.0 == Self::HOLE.0
    }

    /// `null` or `undefined`.
    #[inline]
    #[must_use]
    pub const fn is_nullish(self) -> bool {
        self.is_null() || self.is_undefined()
    }

    /// Boolean immediate.
    #[inline]
    #[must_use]
    pub const fn is_boolean(self) -> bool {
        self.0 == Self::TRUE.0 || self.0 == Self::FALSE.0
    }

    /// Number (int32 or double, including NaN/±Infinity).
    #[inline]
    #[must_use]
    pub fn is_number(self) -> bool {
        top_tag(self.0) == TAG_INT32 || is_double_bits(self.0)
    }

    /// Int32 fast-path number.
    #[inline]
    #[must_use]
    pub fn is_int32(self) -> bool {
        top_tag(self.0) == TAG_INT32
    }

    /// String reference.
    #[inline]
    #[must_use]
    pub fn is_string(self) -> bool {
        top_tag(self.0) == TAG_PTR_STRING
    }

    /// Anything callable: bytecode function id, closure, bound, native,
    /// class-constructor wrapper.
    #[inline]
    #[must_use]
    pub fn is_callable(self) -> bool {
        let t = top_tag(self.0);
        t == TAG_FUNCTION_ID || t == TAG_PTR_FUNCTION
    }

    /// Bytecode function reference (no closure).
    #[inline]
    #[must_use]
    pub fn is_function_id(self) -> bool {
        top_tag(self.0) == TAG_FUNCTION_ID
    }

    /// Any reference that occupies the `PTR_OBJECT` family — object,
    /// array, map, set, promise, etc. Distinguish via
    /// [`Self::read_gc_type_tag`].
    #[inline]
    #[must_use]
    pub fn is_object_like(self) -> bool {
        top_tag(self.0) == TAG_PTR_OBJECT
    }

    /// `TAG_PTR_OTHER` family — symbol / bigint.
    #[inline]
    #[must_use]
    pub fn is_other_primitive(self) -> bool {
        top_tag(self.0) == TAG_PTR_OTHER
    }

    // -----------------------------------------------------------------------
    // Accessors
    // -----------------------------------------------------------------------

    /// Boolean payload.
    #[inline]
    #[must_use]
    pub fn as_boolean(self) -> Option<bool> {
        if self.is_boolean() {
            Some(self.0 == Self::TRUE.0)
        } else {
            None
        }
    }

    /// Number as the runtime [`NumberValue`] view.
    #[inline]
    #[must_use]
    pub fn as_number(self) -> Option<NumberValue> {
        if top_tag(self.0) == TAG_INT32 {
            return Some(NumberValue::Smi(payload32(self.0) as i32));
        }
        if is_double_bits(self.0) {
            return Some(NumberValue::Double(f64::from_bits(self.0)));
        }
        None
    }

    /// `f64` directly. Returns `None` for non-numbers.
    #[inline]
    #[must_use]
    pub fn as_f64(self) -> Option<f64> {
        self.as_number().map(NumberValue::as_f64)
    }

    /// Int32 fast path.
    #[inline]
    #[must_use]
    pub fn as_i32(self) -> Option<i32> {
        if top_tag(self.0) == TAG_INT32 {
            Some(payload32(self.0) as i32)
        } else {
            None
        }
    }

    /// Bytecode function id.
    #[inline]
    #[must_use]
    pub fn as_function_id(self) -> Option<u32> {
        if top_tag(self.0) == TAG_FUNCTION_ID {
            Some(payload32(self.0))
        } else {
            None
        }
    }

    /// Decode the underlying `RawGc` for any pointer-tag payload.
    #[inline]
    #[must_use]
    pub fn as_raw_gc(self) -> Option<otter_gc::raw::RawGc> {
        let t = top_tag(self.0);
        if matches!(
            t,
            TAG_PTR_OBJECT | TAG_PTR_STRING | TAG_PTR_FUNCTION | TAG_PTR_OTHER
        ) {
            Some(otter_gc::raw::RawGc(payload32(self.0)))
        } else {
            None
        }
    }

    /// Read the underlying `GcHeader::type_tag`. `None` if the value
    /// is not a pointer-tagged variant or the payload is null.
    #[inline]
    #[must_use]
    pub fn read_gc_type_tag(self) -> Option<u8> {
        self.as_raw_gc()?.header_type_tag()
    }
}

/// Default to `undefined`.
impl Default for Value {
    #[inline]
    fn default() -> Self {
        Self::UNDEFINED
    }
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind() {
            ValueKind::Number => write!(f, "Value::Number({:?})", self.as_number().unwrap()),
            ValueKind::Int32 => write!(f, "Value::Int32({})", self.as_i32().unwrap()),
            ValueKind::Special => {
                let s = match self.0 {
                    x if x == Self::UNDEFINED.0 => "undefined",
                    x if x == Self::NULL.0 => "null",
                    x if x == Self::HOLE.0 => "<hole>",
                    x if x == Self::TRUE.0 => "true",
                    x if x == Self::FALSE.0 => "false",
                    _ => "<special?>",
                };
                write!(f, "Value::{}", s)
            }
            ValueKind::FunctionId => {
                write!(f, "Value::FunctionId({})", self.as_function_id().unwrap())
            }
            ValueKind::PtrObject => write!(f, "Value::PtrObject(0x{:08x})", payload32(self.0)),
            ValueKind::PtrString => write!(f, "Value::PtrString(0x{:08x})", payload32(self.0)),
            ValueKind::PtrFunction => write!(f, "Value::PtrFunction(0x{:08x})", payload32(self.0)),
            ValueKind::PtrOther => write!(f, "Value::PtrOther(0x{:08x})", payload32(self.0)),
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_eight_bytes() {
        assert_eq!(std::mem::size_of::<Value>(), 8);
        assert_eq!(std::mem::align_of::<Value>(), 8);
    }

    #[test]
    fn immediates_round_trip() {
        assert!(Value::undefined().is_undefined());
        assert!(Value::null().is_null());
        assert!(Value::hole().is_hole());
        assert_eq!(Value::boolean(true).as_boolean(), Some(true));
        assert_eq!(Value::boolean(false).as_boolean(), Some(false));
    }

    #[test]
    fn int32_round_trips() {
        for n in [0_i32, 1, -1, i32::MIN, i32::MAX, 42, -42] {
            let v = Value::number_i32(n);
            assert_eq!(v.as_i32(), Some(n));
            assert_eq!(v.as_number(), Some(NumberValue::Smi(n)));
            assert!(v.is_int32());
            assert!(v.is_number());
        }
    }

    #[test]
    fn doubles_round_trip_and_canonicalise_nan() {
        for d in [0.0_f64, -0.0, 1.5, -1.5, f64::INFINITY, f64::NEG_INFINITY] {
            let v = Value::number_f64(d);
            assert!(v.is_number(), "{d}");
            assert_eq!(v.as_f64().unwrap().to_bits(), d.to_bits());
        }
        let nan_a = Value::number_f64(f64::NAN);
        let nan_b = Value::number_f64(f64::from_bits(0x7FFC_0000_0000_0001));
        assert_eq!(nan_a, nan_b, "all NaNs canonicalise to the same bit pattern");
        assert!(nan_a.is_number());
        assert!(nan_a.as_f64().unwrap().is_nan());
    }

    #[test]
    fn function_id_round_trip() {
        let v = Value::function_id(0x1234_5678);
        assert_eq!(v.as_function_id(), Some(0x1234_5678));
        assert!(v.is_callable());
        assert!(v.is_function_id());
    }

    #[test]
    fn nullish_predicate() {
        assert!(Value::undefined().is_nullish());
        assert!(Value::null().is_nullish());
        assert!(!Value::boolean(false).is_nullish());
        assert!(!Value::number_i32(0).is_nullish());
    }

    #[test]
    fn ptr_tags_round_trip() {
        // We only test the tag encoding here; the actual GC body
        // wiring happens through type-specific wrappers.
        let raw = otter_gc::raw::RawGc(0xDEAD_BEEF);
        let v = Value::from_object_gc(raw);
        assert!(v.is_object_like());
        assert_eq!(v.as_raw_gc().unwrap().0, 0xDEAD_BEEF);

        let s = Value::from_string_gc(raw);
        assert!(s.is_string());
        assert!(!s.is_object_like());

        let f = Value::from_function_gc(raw);
        assert!(f.is_callable());
        assert!(!f.is_function_id());

        let o = Value::from_other_gc(raw);
        assert!(o.is_other_primitive());
    }

    #[test]
    fn kind_returns_expected_family() {
        assert_eq!(Value::undefined().kind(), ValueKind::Special);
        assert_eq!(Value::null().kind(), ValueKind::Special);
        assert_eq!(Value::boolean(true).kind(), ValueKind::Special);
        assert_eq!(Value::number_i32(7).kind(), ValueKind::Int32);
        assert_eq!(Value::number_f64(1.5).kind(), ValueKind::Number);
        assert_eq!(Value::number_f64(f64::NAN).kind(), ValueKind::Number);
        assert_eq!(Value::function_id(0).kind(), ValueKind::FunctionId);
        let raw = otter_gc::raw::RawGc(1);
        assert_eq!(Value::from_object_gc(raw).kind(), ValueKind::PtrObject);
        assert_eq!(Value::from_string_gc(raw).kind(), ValueKind::PtrString);
        assert_eq!(Value::from_function_gc(raw).kind(), ValueKind::PtrFunction);
        assert_eq!(Value::from_other_gc(raw).kind(), ValueKind::PtrOther);
    }
}
