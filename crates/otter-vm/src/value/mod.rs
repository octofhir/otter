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

use crate::array::{ArrayBody, JsArray};
use crate::closure::{JS_CLOSURE_BODY_TYPE_TAG, JsClosureBody};
use crate::collections::{JsMap, JsSet, JsWeakMap, JsWeakSet, MapBody, SetBody, WeakMapBody, WeakSetBody};
use crate::generator::{GeneratorBody, JsGenerator};
use crate::native_function::NativeFunctionBody;
use crate::object::{JsObject, ObjectBody};
use crate::promise::{JsPromiseHandle, PurePromise, PurePromiseBody};
use crate::regexp::{JsRegExp, JsRegExpBody};
use crate::weak_refs::{FinalizationRegistryBody, JsFinalizationRegistry, JsWeakRef, WeakRefBody};
use crate::{
    BoundFunction, BoundFunctionBody, ClassConstructor, ClassConstructorBody, IteratorHandle,
    IteratorState, JsClosure, NativeFunction, NumberValue,
};

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

    /// Build a closure value. Packs the [`JsClosure`] handle under
    /// `TAG_PTR_FUNCTION`. Disambiguation back to a closure happens
    /// through [`crate::closure::JS_CLOSURE_BODY_TYPE_TAG`] on the
    /// GC header.
    #[inline]
    #[must_use]
    pub fn closure(c: JsClosure) -> Self {
        Self::from_function_gc(c.raw())
    }

    /// Ordinary object value.
    #[inline]
    #[must_use]
    pub fn object(o: JsObject) -> Self {
        Self::from_object_gc(o.raw())
    }

    /// Array value.
    #[inline]
    #[must_use]
    pub fn array(a: JsArray) -> Self {
        Self::from_object_gc(a.raw())
    }

    /// Map value.
    #[inline]
    #[must_use]
    pub fn map(m: JsMap) -> Self {
        Self::from_object_gc(m.raw())
    }

    /// Set value.
    #[inline]
    #[must_use]
    pub fn set(s: JsSet) -> Self {
        Self::from_object_gc(s.raw())
    }

    /// WeakMap value.
    #[inline]
    #[must_use]
    pub fn weak_map(m: JsWeakMap) -> Self {
        Self::from_object_gc(m.raw())
    }

    /// WeakSet value.
    #[inline]
    #[must_use]
    pub fn weak_set(s: JsWeakSet) -> Self {
        Self::from_object_gc(s.raw())
    }

    /// WeakRef value.
    #[inline]
    #[must_use]
    pub fn weak_ref(w: JsWeakRef) -> Self {
        Self::from_object_gc(w.raw())
    }

    /// FinalizationRegistry value.
    #[inline]
    #[must_use]
    pub fn finalization_registry(r: JsFinalizationRegistry) -> Self {
        Self::from_object_gc(r.raw())
    }

    /// Bound function value (result of `Function.prototype.bind`).
    #[inline]
    #[must_use]
    pub fn bound_function(b: BoundFunction) -> Self {
        Self::from_function_gc(b.raw())
    }

    /// Host-implemented callable.
    #[inline]
    #[must_use]
    pub fn native_function(n: NativeFunction) -> Self {
        Self::from_function_gc(n.raw())
    }

    /// `class` value — constructor + prototype + statics wrapper.
    #[inline]
    #[must_use]
    pub fn class_constructor(c: ClassConstructor) -> Self {
        Self::from_function_gc(c.raw())
    }

    /// Iterator handle.
    #[inline]
    #[must_use]
    pub fn iterator(i: IteratorHandle) -> Self {
        Self::from_object_gc(i.raw())
    }

    /// Generator handle.
    #[inline]
    #[must_use]
    pub fn generator(g: JsGenerator) -> Self {
        Self::from_object_gc(g.raw())
    }

    /// Compiled RegExp handle.
    #[inline]
    #[must_use]
    pub fn regexp(r: JsRegExp) -> Self {
        Self::from_object_gc(r.raw())
    }

    /// Promise handle. Foundation slice only routes through
    /// [`PurePromise`]; host-bridged promise representations
    /// (`PromiseRepr::*`) plug in through the same `TAG_PTR_OBJECT`
    /// payload because [`PurePromiseBody`] is the only spec body
    /// today.
    #[inline]
    #[must_use]
    pub fn promise(p: JsPromiseHandle) -> Self {
        Self::from_object_gc(p.raw())
    }

    /// Recover a closure handle when this value carries one.
    ///
    /// Returns `None` for any other callable family (bytecode
    /// function id, bound, native, class constructor wrapper).
    #[inline]
    #[must_use]
    pub fn as_closure(self) -> Option<JsClosure> {
        if top_tag(self.0) != TAG_PTR_FUNCTION {
            return None;
        }
        let raw = self.as_raw_gc()?;
        if raw.header_type_tag()? != JS_CLOSURE_BODY_TYPE_TAG {
            return None;
        }
        raw.checked_cast::<JsClosureBody>()
    }

    /// Ordinary object handle.
    #[inline]
    #[must_use]
    pub fn as_object(self) -> Option<JsObject> {
        if !self.is_object_like() {
            return None;
        }
        self.as_raw_gc()?.checked_cast::<ObjectBody>()
    }

    /// Array handle.
    #[inline]
    #[must_use]
    pub fn as_array(self) -> Option<JsArray> {
        if !self.is_object_like() {
            return None;
        }
        self.as_raw_gc()?.checked_cast::<ArrayBody>()
    }

    /// Map handle.
    #[inline]
    #[must_use]
    pub fn as_map(self) -> Option<JsMap> {
        if !self.is_object_like() {
            return None;
        }
        self.as_raw_gc()?.checked_cast::<MapBody>()
    }

    /// Set handle.
    #[inline]
    #[must_use]
    pub fn as_set(self) -> Option<JsSet> {
        if !self.is_object_like() {
            return None;
        }
        self.as_raw_gc()?.checked_cast::<SetBody>()
    }

    /// WeakMap handle.
    #[inline]
    #[must_use]
    pub fn as_weak_map(self) -> Option<JsWeakMap> {
        if !self.is_object_like() {
            return None;
        }
        self.as_raw_gc()?.checked_cast::<WeakMapBody>()
    }

    /// WeakSet handle.
    #[inline]
    #[must_use]
    pub fn as_weak_set(self) -> Option<JsWeakSet> {
        if !self.is_object_like() {
            return None;
        }
        self.as_raw_gc()?.checked_cast::<WeakSetBody>()
    }

    /// WeakRef handle.
    #[inline]
    #[must_use]
    pub fn as_weak_ref(self) -> Option<JsWeakRef> {
        if !self.is_object_like() {
            return None;
        }
        self.as_raw_gc()?.checked_cast::<WeakRefBody>()
    }

    /// FinalizationRegistry handle.
    #[inline]
    #[must_use]
    pub fn as_finalization_registry(self) -> Option<JsFinalizationRegistry> {
        if !self.is_object_like() {
            return None;
        }
        self.as_raw_gc()?.checked_cast::<FinalizationRegistryBody>()
    }

    /// Bound function handle.
    #[inline]
    #[must_use]
    pub fn as_bound_function(self) -> Option<BoundFunction> {
        if top_tag(self.0) != TAG_PTR_FUNCTION {
            return None;
        }
        let gc = self.as_raw_gc()?.checked_cast::<BoundFunctionBody>()?;
        Some(BoundFunction::from_gc(gc))
    }

    /// Native function handle.
    #[inline]
    #[must_use]
    pub fn as_native_function(self) -> Option<NativeFunction> {
        if top_tag(self.0) != TAG_PTR_FUNCTION {
            return None;
        }
        let gc = self.as_raw_gc()?.checked_cast::<NativeFunctionBody>()?;
        Some(NativeFunction::from_gc(gc))
    }

    /// Class-constructor handle.
    #[inline]
    #[must_use]
    pub fn as_class_constructor(self) -> Option<ClassConstructor> {
        if top_tag(self.0) != TAG_PTR_FUNCTION {
            return None;
        }
        let gc = self.as_raw_gc()?.checked_cast::<ClassConstructorBody>()?;
        Some(ClassConstructor::from_gc(gc))
    }

    /// Iterator handle.
    #[inline]
    #[must_use]
    pub fn as_iterator(self) -> Option<IteratorHandle> {
        if !self.is_object_like() {
            return None;
        }
        self.as_raw_gc()?.checked_cast::<IteratorState>()
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
    // Per-type predicates (object-family). Each consults the body's
    // `GcHeader::type_tag` so a `Value::array` never reports itself as
    // a `Value::map` and vice versa.
    // -----------------------------------------------------------------------

    /// Ordinary object body (`ObjectBody` type tag).
    #[inline]
    #[must_use]
    pub fn is_object(self) -> bool {
        self.is_object_like()
            && self.read_gc_type_tag() == Some(<ObjectBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// Array body.
    #[inline]
    #[must_use]
    pub fn is_array(self) -> bool {
        self.is_object_like()
            && self.read_gc_type_tag() == Some(<ArrayBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// Map body.
    #[inline]
    #[must_use]
    pub fn is_map(self) -> bool {
        self.is_object_like()
            && self.read_gc_type_tag() == Some(<MapBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// Set body.
    #[inline]
    #[must_use]
    pub fn is_set(self) -> bool {
        self.is_object_like()
            && self.read_gc_type_tag() == Some(<SetBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// WeakMap body.
    #[inline]
    #[must_use]
    pub fn is_weak_map(self) -> bool {
        self.is_object_like()
            && self.read_gc_type_tag() == Some(<WeakMapBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// WeakSet body.
    #[inline]
    #[must_use]
    pub fn is_weak_set(self) -> bool {
        self.is_object_like()
            && self.read_gc_type_tag() == Some(<WeakSetBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// WeakRef body.
    #[inline]
    #[must_use]
    pub fn is_weak_ref(self) -> bool {
        self.is_object_like()
            && self.read_gc_type_tag() == Some(<WeakRefBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// FinalizationRegistry body.
    #[inline]
    #[must_use]
    pub fn is_finalization_registry(self) -> bool {
        self.is_object_like()
            && self.read_gc_type_tag()
                == Some(<FinalizationRegistryBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// Promise body.
    #[inline]
    #[must_use]
    pub fn is_promise(self) -> bool {
        self.is_object_like()
            && self.read_gc_type_tag()
                == Some(<PurePromiseBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// RegExp body.
    #[inline]
    #[must_use]
    pub fn is_regexp(self) -> bool {
        self.is_object_like()
            && self.read_gc_type_tag() == Some(<JsRegExpBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// Generator body.
    #[inline]
    #[must_use]
    pub fn is_generator(self) -> bool {
        self.is_object_like()
            && self.read_gc_type_tag()
                == Some(<GeneratorBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// Iterator body.
    #[inline]
    #[must_use]
    pub fn is_iterator(self) -> bool {
        self.is_object_like()
            && self.read_gc_type_tag()
                == Some(<IteratorState as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    // -----------------------------------------------------------------------
    // Per-type predicates (function-family).
    // -----------------------------------------------------------------------

    /// Closure body.
    #[inline]
    #[must_use]
    pub fn is_closure(self) -> bool {
        top_tag(self.0) == TAG_PTR_FUNCTION
            && self.read_gc_type_tag() == Some(JS_CLOSURE_BODY_TYPE_TAG)
    }

    /// Bound function body.
    #[inline]
    #[must_use]
    pub fn is_bound_function(self) -> bool {
        top_tag(self.0) == TAG_PTR_FUNCTION
            && self.read_gc_type_tag()
                == Some(<BoundFunctionBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// Native function body.
    #[inline]
    #[must_use]
    pub fn is_native_function(self) -> bool {
        top_tag(self.0) == TAG_PTR_FUNCTION
            && self.read_gc_type_tag()
                == Some(<NativeFunctionBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// Class-constructor wrapper body.
    #[inline]
    #[must_use]
    pub fn is_class_constructor(self) -> bool {
        top_tag(self.0) == TAG_PTR_FUNCTION
            && self.read_gc_type_tag()
                == Some(<ClassConstructorBody as otter_gc::SafeTraceable>::TYPE_TAG)
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

    // -----------------------------------------------------------------------
    // Spec coercions that need no heap access.
    // -----------------------------------------------------------------------

    /// ECMA-262 §7.1.2 ToBoolean for the part of the value model that
    /// is decidable without a heap read.
    ///
    /// Cases that need a heap to consult the payload (string emptiness,
    /// BigInt zero check) return [`None`] so the caller can hop to the
    /// legacy heap-aware coercion path. Once the string / bigint /
    /// symbol primitives migrate off `Rc`/`Arc` into GC bodies, this
    /// helper resolves them inline and the heap-aware path retires.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-toboolean>
    #[inline]
    #[must_use]
    pub fn to_boolean_pure(self) -> Option<bool> {
        if self.is_undefined() || self.is_null() || self.is_hole() {
            return Some(false);
        }
        if let Some(b) = self.as_boolean() {
            return Some(b);
        }
        if let Some(n) = self.as_number() {
            return Some(match n {
                NumberValue::Smi(i) => i != 0,
                NumberValue::Double(d) => !(d.is_nan() || d == 0.0),
            });
        }
        // Callables, object-like references, function-id immediates:
        // always truthy per ECMA-262 §7.1.2 step 6/7.
        if self.is_callable() || self.is_object_like() {
            return Some(true);
        }
        // TAG_PTR_STRING / TAG_PTR_OTHER (symbol, bigint) require a
        // heap-aware coercion until those primitives migrate to a GC
        // body whose payload (length / zero) is readable through the
        // tagged value directly.
        None
    }

    /// Generator handle.
    #[inline]
    #[must_use]
    pub fn as_generator(self) -> Option<JsGenerator> {
        if !self.is_object_like() {
            return None;
        }
        let gc = self.as_raw_gc()?.checked_cast::<GeneratorBody>()?;
        Some(JsGenerator::from_gc(gc))
    }

    /// Compiled RegExp handle.
    #[inline]
    #[must_use]
    pub fn as_regexp(self) -> Option<JsRegExp> {
        if !self.is_object_like() {
            return None;
        }
        let gc = self.as_raw_gc()?.checked_cast::<JsRegExpBody>()?;
        Some(JsRegExp::from_gc(gc))
    }

    /// Promise handle. Today this maps directly through
    /// [`PurePromise`] (the only `PromiseRepr` body) — once host-
    /// bridged promise representations land, this accessor selects
    /// the right inner repr by body type tag.
    #[inline]
    #[must_use]
    pub fn as_promise(self) -> Option<JsPromiseHandle> {
        if !self.is_object_like() {
            return None;
        }
        let gc = self.as_raw_gc()?.checked_cast::<PurePromiseBody>()?;
        Some(JsPromiseHandle::from_pure(PurePromise::from_gc(gc)))
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
    fn closure_round_trip_via_real_heap() {
        use crate::{alloc_closure, alloc_upvalue, Value as LegacyValue};
        use otter_gc::GcHeap;

        let mut heap = GcHeap::new().expect("heap");
        let cell = alloc_upvalue(&mut heap, LegacyValue::Undefined).expect("cell");
        let upvalues = vec![cell].into_boxed_slice();
        let closure = alloc_closure(&mut heap, 99, upvalues, None).expect("alloc");
        let v = Value::closure(closure);
        assert!(v.is_callable());
        assert!(!v.is_function_id());
        assert_eq!(v.as_closure(), Some(closure));
        assert_eq!(v.kind(), ValueKind::PtrFunction);
    }

    #[test]
    fn object_round_trip_via_real_heap() {
        use crate::object::alloc_object_with_roots;
        use otter_gc::GcHeap;
        use otter_gc::raw::RawGc;

        let mut heap = GcHeap::new().expect("heap");
        let mut roots = |_v: &mut dyn FnMut(*mut RawGc)| {};
        let obj = alloc_object_with_roots(&mut heap, &mut roots).expect("alloc");
        let v = Value::object(obj);
        assert!(v.is_object_like());
        assert!(v.is_object());
        assert!(!v.is_array());
        assert!(!v.is_map());
        assert!(!v.is_promise());
        assert!(!v.is_closure());
        assert_eq!(v.as_object(), Some(obj));
        assert_eq!(v.as_array(), None);
        assert_eq!(v.as_map(), None);
        assert_eq!(v.as_set(), None);
        assert_eq!(v.as_closure(), None);
    }

    #[test]
    fn to_boolean_pure_covers_immediates_numbers_and_pointers() {
        // Falsy immediates.
        assert_eq!(Value::undefined().to_boolean_pure(), Some(false));
        assert_eq!(Value::null().to_boolean_pure(), Some(false));
        assert_eq!(Value::hole().to_boolean_pure(), Some(false));
        assert_eq!(Value::boolean(false).to_boolean_pure(), Some(false));
        assert_eq!(Value::number_i32(0).to_boolean_pure(), Some(false));
        assert_eq!(Value::number_f64(0.0).to_boolean_pure(), Some(false));
        assert_eq!(Value::number_f64(-0.0).to_boolean_pure(), Some(false));
        assert_eq!(Value::number_f64(f64::NAN).to_boolean_pure(), Some(false));

        // Truthy immediates and numbers.
        assert_eq!(Value::boolean(true).to_boolean_pure(), Some(true));
        assert_eq!(Value::number_i32(1).to_boolean_pure(), Some(true));
        assert_eq!(Value::number_i32(-1).to_boolean_pure(), Some(true));
        assert_eq!(Value::number_f64(1.5).to_boolean_pure(), Some(true));
        assert_eq!(Value::number_f64(f64::INFINITY).to_boolean_pure(), Some(true));

        // Callables / object-like references are always truthy.
        assert_eq!(Value::function_id(0).to_boolean_pure(), Some(true));
        let raw = otter_gc::raw::RawGc(1);
        assert_eq!(Value::from_object_gc(raw).to_boolean_pure(), Some(true));
        assert_eq!(Value::from_function_gc(raw).to_boolean_pure(), Some(true));

        // String / Other still need heap awareness.
        assert_eq!(Value::from_string_gc(raw).to_boolean_pure(), None);
        assert_eq!(Value::from_other_gc(raw).to_boolean_pure(), None);
    }

    #[test]
    fn predicates_disambiguate_object_and_function_families() {
        use crate::{alloc_closure, alloc_upvalue, Value as LegacyValue};
        use crate::object::alloc_object_with_roots;
        use otter_gc::GcHeap;
        use otter_gc::raw::RawGc;

        let mut heap = GcHeap::new().expect("heap");
        let mut roots = |_v: &mut dyn FnMut(*mut RawGc)| {};
        let obj = alloc_object_with_roots(&mut heap, &mut roots).expect("alloc");
        let cell = alloc_upvalue(&mut heap, LegacyValue::Undefined).expect("cell");
        let closure = alloc_closure(&mut heap, 1, vec![cell].into_boxed_slice(), None)
            .expect("closure");

        let vo = Value::object(obj);
        let vc = Value::closure(closure);
        let vfid = Value::function_id(7);

        // Object positive / negative.
        assert!(vo.is_object());
        assert!(!vo.is_closure());
        assert!(!vo.is_function_id());
        assert!(!vo.is_callable());

        // Closure positive / negative.
        assert!(vc.is_callable());
        assert!(vc.is_closure());
        assert!(!vc.is_function_id());
        assert!(!vc.is_bound_function());
        assert!(!vc.is_native_function());
        assert!(!vc.is_class_constructor());
        assert!(!vc.is_object());

        // FunctionId positive / negative.
        assert!(vfid.is_callable());
        assert!(vfid.is_function_id());
        assert!(!vfid.is_closure());
        assert!(!vfid.is_object_like());

        // Immediates report no object-family / callable membership.
        assert!(!Value::null().is_object());
        assert!(!Value::undefined().is_object_like());
        assert!(!Value::boolean(true).is_callable());
        assert!(!Value::number_i32(0).is_object());
    }

    #[test]
    fn as_closure_rejects_non_closure_function_id() {
        let v = Value::function_id(0);
        assert_eq!(v.as_closure(), None);
        assert!(v.is_callable());
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
