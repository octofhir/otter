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
//! - [`docs/book/src/engine/architecture.md`](../../../../docs/book/src/engine/architecture.md)
//!   — the value-model section.

pub mod tag;

use crate::array::{ArrayBody, JsArray};
use crate::bigint::{BigIntBody, BigIntHandle};
use crate::binary::{
    DataViewBodyGc, DataViewHandle, LocalArrayBufferBodyGc, LocalArrayBufferHandle,
    SharedArrayBufferBodyGc, SharedArrayBufferHandle, TypedArrayBodyGc, TypedArrayHandle,
};
use crate::closure::{JS_CLOSURE_BODY_TYPE_TAG, JsClosureBody};
use crate::collections::{
    JsMap, JsSet, JsWeakMap, JsWeakSet, MapBody, SetBody, WeakMapBody, WeakSetBody,
};
use crate::generator::{GeneratorBody, JsGenerator};
use crate::intl::{IntlBody, IntlHandle};
use crate::native_function::NativeFunctionBody;
use crate::object::{JsObject, ObjectBody};
use crate::promise::{JsPromiseHandle, PurePromise, PurePromiseBody};
use crate::proxy::{ProxyBodyGc, ProxyHandle};
use crate::regexp::{JsRegExp, JsRegExpBody};
use crate::string::{JsStringBody, JsStringHandle};
use crate::symbol::{SymbolBody, SymbolHandle};
use crate::temporal::{TemporalBody, TemporalHandle};
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
///
/// `Value` is explicitly `!Send + !Sync`: even though the bit pattern
/// is a plain `u64`, every pointer-tagged payload aliases a GC handle
/// owned by exactly one isolate. The `PhantomData<*const ()>` second
/// field (ZST) removes the auto-Send / auto-Sync impls without
/// changing the runtime layout.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Value(u64, std::marker::PhantomData<*const ()>);

const _NOT_SEND: std::marker::PhantomData<*const ()> = std::marker::PhantomData;

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

/// Per-body classification for object-family / function-family /
/// other-family pointer payloads. Returned by
/// [`Value::object_family_kind`] / [`Value::function_family_kind`] /
/// [`Value::other_family_kind`] so call sites can dispatch through a
/// single match instead of N predicate calls. Cheaper than calling
/// `is_array() || is_map() || …` because it reads `GcHeader::type_tag`
/// once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectFamilyKind {
    /// Ordinary object body (`ObjectBody`).
    Object,
    /// Dense array body (`ArrayBody`).
    Array,
    /// `Map` body.
    Map,
    /// `Set` body.
    Set,
    /// `WeakMap` body.
    WeakMap,
    /// `WeakSet` body.
    WeakSet,
    /// `WeakRef` body.
    WeakRef,
    /// `FinalizationRegistry` body.
    FinalizationRegistry,
    /// Promise body.
    Promise,
    /// Iterator state body.
    Iterator,
    /// Generator body.
    Generator,
    /// RegExp body.
    RegExp,
    /// Temporal body.
    Temporal,
    /// Intl body.
    Intl,
    /// Proxy body.
    Proxy,
    /// DataView body.
    DataView,
    /// TypedArray body.
    TypedArray,
    /// Non-shared `ArrayBuffer` body.
    LocalArrayBuffer,
    /// `SharedArrayBuffer` body.
    SharedArrayBuffer,
    /// Tag matched `TAG_PTR_OBJECT` but the body type tag is not
    /// one of the families above. Indicates a future body kind or
    /// a stale GC reference; callers should treat as opaque.
    Unknown,
}

/// Per-body classification for callable-family pointer payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionFamilyKind {
    /// Closure body (`JsClosureBody`).
    Closure,
    /// Bound function body (`BoundFunctionBody`).
    Bound,
    /// Native (host-implemented) function body.
    Native,
    /// Class-constructor wrapper body.
    ClassConstructor,
    /// `TAG_PTR_FUNCTION` with an unknown body type tag.
    Unknown,
}

/// Coarse pointer family for a heap cell, read from
/// [`otter_gc::header::GcHeader::type_tag`]. Partitions every heap cell
/// into exactly one family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PtrFamily {
    /// Ordinary object and every exotic object body.
    Object,
    /// String body.
    String,
    /// Callable body: closure, bound, native, class-constructor wrapper.
    Function,
    /// Misc primitive body: symbol, bigint.
    Other,
}

/// Per-body classification for the `TAG_PTR_OTHER` family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtherFamilyKind {
    /// Symbol body.
    Symbol,
    /// BigInt body.
    BigInt,
    /// `TAG_PTR_OTHER` with an unknown body type tag.
    Unknown,
}

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
    /// Raw NaN-box bits for VM-native ABI records and runtime-stub results.
    #[must_use]
    pub(crate) const fn to_abi_bits(self) -> u64 {
        self.0
    }

    /// Rebuild a [`Value`] from raw VM-native ABI bits.
    ///
    /// Callers must only pass bit patterns produced by [`Self::to_abi_bits`]
    /// or by generated code that follows the value layout contract in this
    /// module.
    #[must_use]
    pub(crate) const fn from_abi_bits(bits: u64) -> Self {
        Value(bits, _NOT_SEND)
    }

    // -----------------------------------------------------------------------
    // Canonical immediates
    // -----------------------------------------------------------------------

    /// `undefined`.
    pub const UNDEFINED: Value = Value(VALUE_UNDEFINED, _NOT_SEND);
    /// `null`.
    pub const NULL: Value = Value(VALUE_NULL, _NOT_SEND);
    /// Internal "array hole" sentinel — never observed by user code.
    pub const HOLE: Value = Value(VALUE_HOLE, _NOT_SEND);
    /// `false`.
    pub const FALSE: Value = Value(VALUE_FALSE, _NOT_SEND);
    /// `true`.
    pub const TRUE: Value = Value(VALUE_TRUE, _NOT_SEND);

    // -----------------------------------------------------------------------
    // Bit-level access (audited helpers; not part of the public stable
    // surface).
    // -----------------------------------------------------------------------

    /// Construct from raw bits. **Caller** must uphold the encoding
    /// contract in [`tag`].
    #[doc(hidden)]
    #[inline(always)]
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits, _NOT_SEND)
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
        Self(box_int32(n), _NOT_SEND)
    }

    /// Number from an `f64`. NaNs are purified to the canonical pattern
    /// before the double offset is applied (so no boxed double aliases
    /// the cell space); integer-valued finite doubles are *not*
    /// automatically demoted to int32 — pass through
    /// [`NumberValue::canonicalize`] first if you want that.
    #[inline]
    #[must_use]
    pub fn number_f64(d: f64) -> Self {
        let bits = if d.is_nan() {
            CANONICAL_NAN
        } else {
            d.to_bits()
        };
        Self(box_double(bits), _NOT_SEND)
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
        Self(box_function_id(id), _NOT_SEND)
    }

    /// Build a heap-cell `Value` from a [`otter_gc::raw::RawGc`] compressed
    /// offset by widening it to the full `cage_base + offset` address.
    /// Because the cage is 4 GiB-aligned, `cage_base | offset` equals
    /// `cage_base + offset` and the offset stays verbatim in the low 32
    /// bits. This is the single funnel every pointer-family constructor
    /// flows through.
    #[inline]
    #[must_use]
    fn from_cell_offset(offset: u32) -> Self {
        let addr = (otter_gc::cage_base() as u64) | (offset as u64);
        Self(addr, _NOT_SEND)
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
        Self::from_cell_offset(raw.0)
    }

    /// Build a string-family value. Recovered later through
    /// [`otter_gc::header::GcHeader::type_tag`], not a value tag.
    #[inline]
    #[must_use]
    pub fn from_string_gc(raw: otter_gc::raw::RawGc) -> Self {
        Self::from_cell_offset(raw.0)
    }

    /// Build a callable-family value.
    #[inline]
    #[must_use]
    pub fn from_function_gc(raw: otter_gc::raw::RawGc) -> Self {
        Self::from_cell_offset(raw.0)
    }

    /// Build a "other primitive" value — symbols, bigints.
    #[inline]
    #[must_use]
    pub fn from_other_gc(raw: otter_gc::raw::RawGc) -> Self {
        Self::from_cell_offset(raw.0)
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

    /// String value. Packs the [`JsStringHandle`] under `TAG_PTR_STRING`.
    #[inline]
    #[must_use]
    pub fn string_gc(s: JsStringHandle) -> Self {
        Self::from_cell_offset(s.offset())
    }

    /// BigInt value. Packs the [`BigIntHandle`] under `TAG_PTR_OTHER`.
    #[inline]
    #[must_use]
    pub fn big_int_gc(b: BigIntHandle) -> Self {
        Self::from_cell_offset(b.offset())
    }

    /// Symbol value. Packs the [`SymbolHandle`] under `TAG_PTR_OTHER`.
    #[inline]
    #[must_use]
    pub fn symbol_gc(s: SymbolHandle) -> Self {
        Self::from_cell_offset(s.offset())
    }

    /// `Temporal.*` value. Object-shaped per Temporal proposal §8.
    #[inline]
    #[must_use]
    pub fn temporal_gc(t: TemporalHandle) -> Self {
        Self::from_object_gc(t.raw())
    }

    /// `Intl.*` value. Object-shaped per ECMA-402.
    #[inline]
    #[must_use]
    pub fn intl_gc(i: IntlHandle) -> Self {
        Self::from_object_gc(i.raw())
    }

    /// `Proxy` value per ECMA-262 §28.2.
    #[inline]
    #[must_use]
    pub fn proxy_gc(p: ProxyHandle) -> Self {
        Self::from_object_gc(p.raw())
    }

    /// `DataView` value per ECMA-262 §25.3.
    #[inline]
    #[must_use]
    pub fn data_view_gc(v: DataViewHandle) -> Self {
        Self::from_object_gc(v.raw())
    }

    /// `TypedArray` value per ECMA-262 §23.2.
    #[inline]
    #[must_use]
    pub fn typed_array_gc(t: TypedArrayHandle) -> Self {
        Self::from_object_gc(t.raw())
    }

    /// Non-shared `ArrayBuffer` value per ECMA-262 §25.1.
    #[inline]
    #[must_use]
    pub fn local_array_buffer_gc(b: LocalArrayBufferHandle) -> Self {
        Self::from_object_gc(b.raw())
    }

    /// `SharedArrayBuffer` value per ECMA-262 §25.2. The body owns
    /// an `Arc<SharedBody>` so the cross-thread bytes stay outside
    /// the single-mutator GC cage.
    #[inline]
    #[must_use]
    pub fn shared_array_buffer_gc(b: SharedArrayBufferHandle) -> Self {
        Self::from_object_gc(b.raw())
    }

    /// Recover a closure handle when this value carries one.
    ///
    /// Returns `None` for any other callable family (bytecode
    /// function id, bound, native, class constructor wrapper).
    #[inline]
    #[must_use]
    pub fn as_closure(self, heap: &otter_gc::GcHeap) -> Option<JsClosure> {
        let handle = self.as_raw_gc()?.checked_cast::<JsClosureBody>()?;
        let function_id = heap.read_payload(handle, |body| body.function_id);
        Some(JsClosure::from_parts(handle, function_id))
    }

    /// Ordinary object handle.
    #[inline]
    #[must_use]
    pub fn as_object(self) -> Option<JsObject> {
        self.as_raw_gc()?.checked_cast::<ObjectBody>()
    }

    /// Array handle.
    #[inline]
    #[must_use]
    pub fn as_array(self) -> Option<JsArray> {
        self.as_raw_gc()?.checked_cast::<ArrayBody>()
    }

    /// Map handle.
    #[inline]
    #[must_use]
    pub fn as_map(self) -> Option<JsMap> {
        self.as_raw_gc()?.checked_cast::<MapBody>()
    }

    /// Set handle.
    #[inline]
    #[must_use]
    pub fn as_set(self) -> Option<JsSet> {
        self.as_raw_gc()?.checked_cast::<SetBody>()
    }

    /// WeakMap handle.
    #[inline]
    #[must_use]
    pub fn as_weak_map(self) -> Option<JsWeakMap> {
        self.as_raw_gc()?.checked_cast::<WeakMapBody>()
    }

    /// WeakSet handle.
    #[inline]
    #[must_use]
    pub fn as_weak_set(self) -> Option<JsWeakSet> {
        self.as_raw_gc()?.checked_cast::<WeakSetBody>()
    }

    /// WeakRef handle.
    #[inline]
    #[must_use]
    pub fn as_weak_ref(self) -> Option<JsWeakRef> {
        self.as_raw_gc()?.checked_cast::<WeakRefBody>()
    }

    /// FinalizationRegistry handle.
    #[inline]
    #[must_use]
    pub fn as_finalization_registry(self) -> Option<JsFinalizationRegistry> {
        self.as_raw_gc()?.checked_cast::<FinalizationRegistryBody>()
    }

    /// Bound function handle.
    #[inline]
    #[must_use]
    pub fn as_bound_function(self) -> Option<BoundFunction> {
        let gc = self.as_raw_gc()?.checked_cast::<BoundFunctionBody>()?;
        Some(BoundFunction::from_gc(gc))
    }

    /// Native function handle.
    #[inline]
    #[must_use]
    pub fn as_native_function(self) -> Option<NativeFunction> {
        let gc = self.as_raw_gc()?.checked_cast::<NativeFunctionBody>()?;
        Some(NativeFunction::from_gc(gc))
    }

    /// Class-constructor handle.
    #[inline]
    #[must_use]
    pub fn as_class_constructor(self) -> Option<ClassConstructor> {
        let gc = self.as_raw_gc()?.checked_cast::<ClassConstructorBody>()?;
        Some(ClassConstructor::from_gc(gc))
    }

    /// Iterator handle.
    #[inline]
    #[must_use]
    pub fn as_iterator(self) -> Option<IteratorHandle> {
        self.as_raw_gc()?.checked_cast::<IteratorState>()
    }

    // -----------------------------------------------------------------------
    // Heap-cell decoding (the pointer-cheap core).
    // -----------------------------------------------------------------------

    /// `true` if this value is a heap-cell pointer (full address,
    /// top 16 bits clear, `OTHER_TAG` clear). A raw bit test, no heap
    /// access.
    #[inline]
    #[must_use]
    pub fn is_cell(self) -> bool {
        is_cell_bits(self.0)
    }

    /// Classify a heap cell into its pointer family by reading
    /// [`otter_gc::header::GcHeader::type_tag`]. `None` for non-cells.
    /// Only string / callable / symbol / bigint bodies leave the object
    /// family; every other body is an object-family member.
    #[inline]
    #[must_use]
    fn ptr_family(self) -> Option<PtrFamily> {
        let tag = self.read_gc_type_tag()?;
        Some(
            if tag == <JsStringBody as otter_gc::SafeTraceable>::TYPE_TAG {
                PtrFamily::String
            } else if tag == JS_CLOSURE_BODY_TYPE_TAG
                || tag == <BoundFunctionBody as otter_gc::SafeTraceable>::TYPE_TAG
                || tag == <NativeFunctionBody as otter_gc::SafeTraceable>::TYPE_TAG
                || tag == <ClassConstructorBody as otter_gc::SafeTraceable>::TYPE_TAG
            {
                PtrFamily::Function
            } else if tag == <SymbolBody as otter_gc::SafeTraceable>::TYPE_TAG
                || tag == <BigIntBody as otter_gc::SafeTraceable>::TYPE_TAG
            {
                PtrFamily::Other
            } else {
                PtrFamily::Object
            },
        )
    }

    // -----------------------------------------------------------------------
    // Coarse classification.
    // -----------------------------------------------------------------------

    /// Coarse value family. See [`ValueKind`].
    #[inline]
    #[must_use]
    pub fn kind(self) -> ValueKind {
        if is_int32_bits(self.0) {
            return ValueKind::Int32;
        }
        if is_number_bits(self.0) {
            return ValueKind::Number;
        }
        if is_function_id_bits(self.0) {
            return ValueKind::FunctionId;
        }
        match self.ptr_family() {
            Some(PtrFamily::Object) => ValueKind::PtrObject,
            Some(PtrFamily::String) => ValueKind::PtrString,
            Some(PtrFamily::Function) => ValueKind::PtrFunction,
            Some(PtrFamily::Other) => ValueKind::PtrOther,
            // Immediates (null / undefined / bool / hole).
            None => ValueKind::Special,
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
        is_number_bits(self.0)
    }

    /// Int32 fast-path number.
    #[inline]
    #[must_use]
    pub fn is_int32(self) -> bool {
        is_int32_bits(self.0)
    }

    /// String reference. Reads the body's `GcHeader::type_tag`.
    #[inline]
    #[must_use]
    pub fn is_string(self) -> bool {
        self.ptr_family() == Some(PtrFamily::String)
    }

    /// Anything callable: bytecode function id, closure, bound, native,
    /// class-constructor wrapper.
    #[inline]
    #[must_use]
    pub fn is_callable(self) -> bool {
        is_function_id_bits(self.0) || self.ptr_family() == Some(PtrFamily::Function)
    }

    /// Bytecode function reference (no closure). A raw bit test.
    #[inline]
    #[must_use]
    pub fn is_function_id(self) -> bool {
        is_function_id_bits(self.0)
    }

    /// Any reference in the object family — object, array, map, set,
    /// promise, etc. Reads the body's `GcHeader::type_tag`; distinguish
    /// the concrete body via [`Self::read_gc_type_tag`].
    #[inline]
    #[must_use]
    pub fn is_object_like(self) -> bool {
        self.ptr_family() == Some(PtrFamily::Object)
    }

    /// ECMA-262 `Type(value) is Object` — any heap-backed reference
    /// type (plain object, callable, exotic). Wider than
    /// [`Self::is_object_like`], which is narrowed to TAG_PTR_OBJECT
    /// only. Use this when implementing spec predicates that say
    /// "If V is an Object" (e.g. `isPrototypeOf`, `instanceof`,
    /// `OrdinaryCreateFromConstructor`, `IsConstructor` validation,
    /// the `Object` ToPrimitive path).
    #[inline]
    #[must_use]
    pub fn is_object_type(self) -> bool {
        self.is_object_like() || self.is_callable()
    }

    /// Misc-primitive family — symbol / bigint.
    #[inline]
    #[must_use]
    pub fn is_other_primitive(self) -> bool {
        self.ptr_family() == Some(PtrFamily::Other)
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
        self.read_gc_type_tag() == Some(<ObjectBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// Array body.
    #[inline]
    #[must_use]
    pub fn is_array(self) -> bool {
        self.read_gc_type_tag() == Some(<ArrayBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// Map body.
    #[inline]
    #[must_use]
    pub fn is_map(self) -> bool {
        self.read_gc_type_tag() == Some(<MapBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// Set body.
    #[inline]
    #[must_use]
    pub fn is_set(self) -> bool {
        self.read_gc_type_tag() == Some(<SetBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// WeakMap body.
    #[inline]
    #[must_use]
    pub fn is_weak_map(self) -> bool {
        self.read_gc_type_tag() == Some(<WeakMapBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// WeakSet body.
    #[inline]
    #[must_use]
    pub fn is_weak_set(self) -> bool {
        self.read_gc_type_tag() == Some(<WeakSetBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// WeakRef body.
    #[inline]
    #[must_use]
    pub fn is_weak_ref(self) -> bool {
        self.read_gc_type_tag() == Some(<WeakRefBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// FinalizationRegistry body.
    #[inline]
    #[must_use]
    pub fn is_finalization_registry(self) -> bool {
        self.read_gc_type_tag()
            == Some(<FinalizationRegistryBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// Promise body.
    #[inline]
    #[must_use]
    pub fn is_promise(self) -> bool {
        self.read_gc_type_tag() == Some(<PurePromiseBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// RegExp body.
    #[inline]
    #[must_use]
    pub fn is_regexp(self) -> bool {
        self.read_gc_type_tag() == Some(<JsRegExpBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// Generator body.
    #[inline]
    #[must_use]
    pub fn is_generator(self) -> bool {
        self.read_gc_type_tag() == Some(<GeneratorBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// Iterator body.
    #[inline]
    #[must_use]
    pub fn is_iterator(self) -> bool {
        self.read_gc_type_tag() == Some(<IteratorState as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    // -----------------------------------------------------------------------
    // Per-type predicates (function-family).
    // -----------------------------------------------------------------------

    /// Closure body.
    #[inline]
    #[must_use]
    pub fn is_closure(self) -> bool {
        self.read_gc_type_tag() == Some(JS_CLOSURE_BODY_TYPE_TAG)
    }

    /// Bound function body.
    #[inline]
    #[must_use]
    pub fn is_bound_function(self) -> bool {
        self.read_gc_type_tag() == Some(<BoundFunctionBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// Native function body.
    #[inline]
    #[must_use]
    pub fn is_native_function(self) -> bool {
        self.read_gc_type_tag() == Some(<NativeFunctionBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// Class-constructor wrapper body.
    #[inline]
    #[must_use]
    pub fn is_class_constructor(self) -> bool {
        self.read_gc_type_tag() == Some(<ClassConstructorBody as otter_gc::SafeTraceable>::TYPE_TAG)
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
        if is_int32_bits(self.0) {
            return Some(NumberValue::Smi(unbox_int32(self.0)));
        }
        if is_number_bits(self.0) {
            return Some(NumberValue::Double(f64::from_bits(unbox_double(self.0))));
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
        if is_int32_bits(self.0) {
            Some(unbox_int32(self.0))
        } else {
            None
        }
    }

    /// Bytecode function id.
    #[inline]
    #[must_use]
    pub fn as_function_id(self) -> Option<u32> {
        if is_function_id_bits(self.0) {
            Some(unbox_function_id(self.0))
        } else {
            None
        }
    }

    /// Decode the compressed [`otter_gc::raw::RawGc`] offset of a heap
    /// cell — the low 32 bits of the full address.
    #[inline]
    #[must_use]
    pub fn as_raw_gc(self) -> Option<otter_gc::raw::RawGc> {
        if is_cell_bits(self.0) {
            Some(otter_gc::raw::RawGc(cell_offset(self.0)))
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

    /// ECMA-262 §13.5.3 `typeof` for cases decidable without a heap
    /// read. Returns `None` for ordinary `JsObject` (a native
    /// `[[Call]]` slot would surface as `"function"` per §7.2.3
    /// IsCallable) and `TAG_PTR_OTHER` (Symbol vs BigInt body tag).
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-typeof-operator>
    #[inline]
    #[must_use]
    pub fn typeof_pure(self) -> Option<&'static str> {
        if self.is_undefined() || self.is_hole() {
            return Some("undefined");
        }
        if self.is_null() {
            return Some("object");
        }
        if self.is_boolean() {
            return Some("boolean");
        }
        if self.is_number() {
            return Some("number");
        }
        if self.is_callable() {
            return Some("function");
        }
        // Object-family pointer: typeof is "object" for the body kinds
        // we control (array / map / set / promise / regexp / iterator /
        // generator / weak* / finalization registry / typed views /
        // temporal / intl) — none of those declare a hidden callable
        // slot today. Plain `JsObject` may carry a `[[Call]]` slot and
        // must surface as "function"; signal `None` for that case so
        // the caller hops to `typeof_with_heap`.
        match self.kind() {
            ValueKind::PtrObject => {
                if self.is_object() {
                    // Plain object — caller must check `[[Call]]`.
                    None
                } else {
                    Some("object")
                }
            }
            // String / Other still need heap-side primitives.
            ValueKind::PtrString => Some("string"),
            ValueKind::PtrOther => None,
            _ => None,
        }
    }

    /// ECMA-262 §7.1.2 ToBoolean for cases decidable without a heap
    /// read. Returns `None` for `TAG_PTR_STRING` (length probe needed)
    /// and `TAG_PTR_OTHER` (BigInt zero / Symbol always-true) — the
    /// caller threads a heap and inspects the body.
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
        // §7.1.2 steps 6/7 — callables and ordinary objects are truthy.
        if self.is_callable() || self.is_object_like() {
            return Some(true);
        }
        None
    }

    /// Spec [`ToBoolean`](https://tc39.es/ecma262/#sec-toboolean).
    ///
    /// Full ladder including the heap-touching arms:
    /// - String → false iff length is zero (cached on the handle).
    /// - BigInt → false iff body decodes to 0 ([`crate::bigint::BigIntValue::is_zero`]).
    /// - Symbol → always true.
    /// - Annex B `[[IsHTMLDDA]]` host value → false.
    ///
    /// Everything decidable without heap is delegated to
    /// [`Self::to_boolean_pure`].
    #[inline]
    #[must_use]
    #[allow(dead_code)] // Wired up at Phase-1 swap; ~50 call sites flip over from legacy::Value.
    pub fn to_boolean(self, heap: &otter_gc::GcHeap) -> bool {
        if self.is_html_dda(heap) {
            return false;
        }
        if let Some(b) = self.to_boolean_pure() {
            return b;
        }
        if self.is_string() {
            return self.as_string(heap).map(|s| !s.is_empty()).unwrap_or(false);
        }
        if self.is_big_int() {
            return self.as_big_int().map(|b| !b.is_zero(heap)).unwrap_or(false);
        }
        // Remaining TAG_PTR_OTHER residue (Symbol, plus any future
        // `other`-family body) is truthy per §7.1.2 step 7.
        true
    }

    /// `true` for the Test262 host object that emulates Annex B
    /// `[[IsHTMLDDA]]`.
    #[inline]
    #[must_use]
    pub fn is_html_dda(self, heap: &otter_gc::GcHeap) -> bool {
        self.as_native_function()
            .is_some_and(|native| native.name(heap) == "__otter_is_htmldda")
    }

    /// Spec [`typeof`](https://tc39.es/ecma262/#sec-typeof-operator) —
    /// the JS-visible type tag. Heap-free except for the GC-header
    /// `type_tag` probe used to discriminate Symbol vs BigInt under
    /// `TAG_PTR_OTHER`.
    ///
    /// An ordinary `JsObject` with a hidden native `[[Call]]` slot
    /// still surfaces here as `"object"`; the heap-aware variant
    /// [`Self::typeof_string_with_heap`] upgrades that case to
    /// `"function"`.
    #[must_use]
    #[allow(dead_code)] // Wired up at Phase-1 swap.
    pub fn typeof_string(self) -> &'static str {
        if let Some(t) = self.typeof_pure() {
            return t;
        }
        match self.kind() {
            ValueKind::PtrOther => match self.other_family_kind() {
                Some(OtherFamilyKind::Symbol) => "symbol",
                Some(OtherFamilyKind::BigInt) => "bigint",
                _ => "object",
            },
            _ => "object",
        }
    }

    /// `typeof` when the VM heap is available. Ordinary objects can
    /// carry a hidden native `[[Call]]` slot, so their visible tag is
    /// `"function"` even though the value kind is `PtrObject`.
    #[must_use]
    #[allow(dead_code)] // Wired up at Phase-1 swap.
    pub fn typeof_string_with_heap(self, heap: &otter_gc::GcHeap) -> &'static str {
        if self.is_html_dda(heap) {
            return "undefined";
        }
        if let Some(obj) = self.as_object()
            && crate::object::call_native(obj, heap).is_some_and(|v| v.is_native_function())
        {
            return "function";
        }
        // §10.5.15 ProxyCreate installs [[Call]] iff the target was
        // callable at creation, and typeof reflects that slot
        // (Table 35) — including after revocation nulls the target.
        if let Some(p) = self.as_proxy() {
            return if p.is_callable(heap) {
                "function"
            } else {
                "object"
            };
        }
        self.typeof_string()
    }

    /// Convenience: shared empty-string constant. Allocates only on
    /// first call per heap.
    ///
    /// # Errors
    /// Surfaces [`otter_gc::OutOfMemory`] verbatim.
    #[allow(dead_code)] // Wired up at Phase-1 swap.
    pub fn empty_string(heap: &mut otter_gc::GcHeap) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self::string(crate::string::JsString::empty(heap)?))
    }

    /// Construct a string value from in-memory text. Convenience for
    /// tests and the compiler's literal table.
    ///
    /// # Errors
    /// See [`crate::string::JsString::from_str`].
    #[allow(dead_code)] // Wired up at Phase-1 swap.
    pub fn from_str(s: &str, heap: &mut otter_gc::GcHeap) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self::string(crate::string::JsString::from_str(s, heap)?))
    }

    /// Render the value as a debug-style string suitable for CLI
    /// preview output. The BigInt / String / Symbol arms read the body
    /// through `heap`; every other primitive short-circuits without
    /// touching the heap.
    #[must_use]
    #[allow(dead_code)] // Wired up at Phase-1 swap; ~25 call sites flip over from legacy::Value.
    pub fn display_string(self, heap: &otter_gc::GcHeap) -> String {
        match self.kind() {
            ValueKind::Number | ValueKind::Int32 => self
                .as_number()
                .map(|n| n.to_display_string())
                .unwrap_or_default(),
            ValueKind::Special => {
                if self.is_undefined() || self.is_hole() {
                    "undefined".to_string()
                } else if self.is_null() {
                    "null".to_string()
                } else if self == Self::TRUE {
                    "true".to_string()
                } else {
                    "false".to_string()
                }
            }
            ValueKind::FunctionId => {
                format!("[Function #{}]", self.as_function_id().unwrap_or(0))
            }
            ValueKind::PtrString => self
                .as_string(heap)
                .map(|s| s.to_lossy_string(heap))
                .unwrap_or_default(),
            ValueKind::PtrFunction => match self.function_family_kind() {
                Some(FunctionFamilyKind::Closure) => {
                    let id = self.as_closure(heap).map(|c| c.function_id()).unwrap_or(0);
                    format!("[Function #{id}]")
                }
                Some(FunctionFamilyKind::Bound) => "[BoundFunction]".to_string(),
                Some(FunctionFamilyKind::Native) => "[NativeFunction]".to_string(),
                Some(FunctionFamilyKind::ClassConstructor) => "[class]".to_string(),
                _ => "[Function]".to_string(),
            },
            ValueKind::PtrObject => match self.object_family_kind() {
                Some(ObjectFamilyKind::Object) => "[object Object]".to_string(),
                Some(ObjectFamilyKind::Array) => "[object Array]".to_string(),
                Some(ObjectFamilyKind::Map) => "[object Map]".to_string(),
                Some(ObjectFamilyKind::Set) => "[object Set]".to_string(),
                Some(ObjectFamilyKind::WeakMap) => "[object WeakMap]".to_string(),
                Some(ObjectFamilyKind::WeakSet) => "[object WeakSet]".to_string(),
                Some(ObjectFamilyKind::WeakRef) => "[object WeakRef]".to_string(),
                Some(ObjectFamilyKind::FinalizationRegistry) => {
                    "[object FinalizationRegistry]".to_string()
                }
                Some(ObjectFamilyKind::Promise) => "[object Promise]".to_string(),
                Some(ObjectFamilyKind::Iterator) => "[object Iterator]".to_string(),
                Some(ObjectFamilyKind::Generator) => "[object Generator]".to_string(),
                Some(ObjectFamilyKind::RegExp) => "[object RegExp]".to_string(),
                Some(ObjectFamilyKind::Temporal) => self
                    .as_temporal(heap)
                    .map(|t| format!("[object Temporal.{}]", t.kind().class_name()))
                    .unwrap_or_else(|| "[object Temporal]".to_string()),
                Some(ObjectFamilyKind::Intl) => self
                    .as_intl(heap)
                    .map(|i| format!("[object Intl.{}]", i.kind().class_name()))
                    .unwrap_or_else(|| "[object Intl]".to_string()),
                Some(ObjectFamilyKind::Proxy) => "[object Proxy]".to_string(),
                Some(ObjectFamilyKind::DataView) => "[object DataView]".to_string(),
                Some(ObjectFamilyKind::TypedArray) => self
                    .as_typed_array(heap)
                    .map(|t| format!("[object {}]", t.kind().name()))
                    .unwrap_or_else(|| "[object TypedArray]".to_string()),
                Some(ObjectFamilyKind::LocalArrayBuffer) => "[object ArrayBuffer]".to_string(),
                Some(ObjectFamilyKind::SharedArrayBuffer) => {
                    "[object SharedArrayBuffer]".to_string()
                }
                _ => "[object Object]".to_string(),
            },
            ValueKind::PtrOther => match self.other_family_kind() {
                Some(OtherFamilyKind::Symbol) => self
                    .as_symbol(heap)
                    .map(|s| s.descriptive_string(heap))
                    .unwrap_or_default(),
                Some(OtherFamilyKind::BigInt) => self
                    .as_big_int()
                    .map(|b| b.to_decimal_string(heap))
                    .unwrap_or_default(),
                _ => String::new(),
            },
        }
    }

    /// Generator handle.
    #[inline]
    #[must_use]
    pub fn as_generator(self) -> Option<JsGenerator> {
        let gc = self.as_raw_gc()?.checked_cast::<GeneratorBody>()?;
        Some(JsGenerator::from_gc(gc))
    }

    /// Compiled RegExp handle.
    #[inline]
    #[must_use]
    pub fn as_regexp(self) -> Option<JsRegExp> {
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
        let gc = self.as_raw_gc()?.checked_cast::<PurePromiseBody>()?;
        Some(JsPromiseHandle::from_pure(PurePromise::from_gc(gc)))
    }

    /// GC-managed string body handle.
    #[inline]
    #[must_use]
    pub fn as_string_gc(self) -> Option<JsStringHandle> {
        self.as_raw_gc()?.checked_cast::<JsStringBody>()
    }

    /// GC-managed BigInt body handle.
    #[inline]
    #[must_use]
    pub fn as_big_int_gc(self) -> Option<BigIntHandle> {
        self.as_raw_gc()?.checked_cast::<BigIntBody>()
    }

    /// `true` when the value points at a [`BigIntBody`].
    #[inline]
    #[must_use]
    pub fn is_big_int_gc(self) -> bool {
        self.read_gc_type_tag() == Some(<BigIntBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// GC-managed Symbol body handle.
    #[inline]
    #[must_use]
    pub fn as_symbol_gc(self) -> Option<SymbolHandle> {
        self.as_raw_gc()?.checked_cast::<SymbolBody>()
    }

    /// `true` when the value is a GC-managed Symbol body.
    #[inline]
    #[must_use]
    pub fn is_symbol_gc(self) -> bool {
        self.read_gc_type_tag() == Some(<SymbolBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// GC-managed Temporal body handle.
    #[inline]
    #[must_use]
    pub fn as_temporal_gc(self) -> Option<TemporalHandle> {
        self.as_raw_gc()?.checked_cast::<TemporalBody>()
    }

    /// `true` when the value is a GC-managed Temporal body.
    #[inline]
    #[must_use]
    pub fn is_temporal_gc(self) -> bool {
        self.read_gc_type_tag() == Some(<TemporalBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// GC-managed Intl body handle.
    #[inline]
    #[must_use]
    pub fn as_intl_gc(self) -> Option<IntlHandle> {
        self.as_raw_gc()?.checked_cast::<IntlBody>()
    }

    /// `true` when the value is a GC-managed Intl body.
    #[inline]
    #[must_use]
    pub fn is_intl_gc(self) -> bool {
        self.read_gc_type_tag() == Some(<IntlBody as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// GC-managed Proxy body handle.
    #[inline]
    #[must_use]
    pub fn as_proxy_gc(self) -> Option<ProxyHandle> {
        self.as_raw_gc()?.checked_cast::<ProxyBodyGc>()
    }

    /// `true` when the value is a GC-managed Proxy body.
    #[inline]
    #[must_use]
    pub fn is_proxy_gc(self) -> bool {
        self.read_gc_type_tag() == Some(<ProxyBodyGc as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// GC-managed DataView body handle.
    #[inline]
    #[must_use]
    pub fn as_data_view_gc(self) -> Option<DataViewHandle> {
        self.as_raw_gc()?.checked_cast::<DataViewBodyGc>()
    }

    /// `true` when the value is a GC-managed DataView body.
    #[inline]
    #[must_use]
    pub fn is_data_view_gc(self) -> bool {
        self.read_gc_type_tag() == Some(<DataViewBodyGc as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// GC-managed TypedArray body handle.
    #[inline]
    #[must_use]
    pub fn as_typed_array_gc(self) -> Option<TypedArrayHandle> {
        self.as_raw_gc()?.checked_cast::<TypedArrayBodyGc>()
    }

    /// `true` when the value is a GC-managed TypedArray body.
    #[inline]
    #[must_use]
    pub fn is_typed_array_gc(self) -> bool {
        self.read_gc_type_tag() == Some(<TypedArrayBodyGc as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// GC-managed non-shared `ArrayBuffer` body handle.
    #[inline]
    #[must_use]
    pub fn as_local_array_buffer_gc(self) -> Option<LocalArrayBufferHandle> {
        self.as_raw_gc()?.checked_cast::<LocalArrayBufferBodyGc>()
    }

    /// `true` when the value is a GC-managed Local ArrayBuffer body.
    #[inline]
    #[must_use]
    pub fn is_local_array_buffer_gc(self) -> bool {
        self.read_gc_type_tag()
            == Some(<LocalArrayBufferBodyGc as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// GC-managed `SharedArrayBuffer` body handle.
    #[inline]
    #[must_use]
    pub fn as_shared_array_buffer_gc(self) -> Option<SharedArrayBufferHandle> {
        self.as_raw_gc()?.checked_cast::<SharedArrayBufferBodyGc>()
    }

    /// `true` when the value is a GC-managed Shared ArrayBuffer body.
    #[inline]
    #[must_use]
    pub fn is_shared_array_buffer_gc(self) -> bool {
        self.read_gc_type_tag()
            == Some(<SharedArrayBufferBodyGc as otter_gc::SafeTraceable>::TYPE_TAG)
    }

    /// `true` when the value is either Local or Shared ArrayBuffer.
    #[inline]
    #[must_use]
    pub fn is_array_buffer_gc(self) -> bool {
        self.is_local_array_buffer_gc() || self.is_shared_array_buffer_gc()
    }

    // -----------------------------------------------------------------------
    // Wrapper-shaped constructors / extractors.
    //
    // Convenience layer over the `*_gc` handle-level surface for call
    // sites that already hold (or want to recover) the legacy wrapper
    // type (`JsString`, `BigIntValue`, `JsSymbol`, …). Constructors
    // unwrap to the underlying handle; extractors rebuild the wrapper
    // from the handle plus any side-cached state the wrapper carries.
    //
    // These are the call-site shapes the Phase C cut-over rewrites
    // `Value::Variant(…)` patterns into.
    // -----------------------------------------------------------------------

    /// String wrapper constructor. Equivalent to legacy
    /// `Value::String(s)`.
    #[inline]
    #[must_use]
    pub fn string(s: crate::string::JsString) -> Self {
        Self::string_gc(s.handle())
    }

    /// Wrapper-level string extractor. Returns the legacy
    /// [`crate::string::JsString`] (handle + cached len/hash) when this
    /// value is in the string family; reads the heap once to prime the
    /// cached fields.
    #[inline]
    #[must_use]
    pub fn as_string(self, heap: &otter_gc::GcHeap) -> Option<crate::string::JsString> {
        let handle = self.as_string_gc()?;
        crate::string::JsString::from_gc_handle(heap, handle).ok()
    }

    /// BigInt wrapper constructor. Equivalent to legacy
    /// `Value::BigInt(b)`.
    #[inline]
    #[must_use]
    pub fn big_int(b: crate::bigint::BigIntValue) -> Self {
        Self::big_int_gc(b.handle())
    }

    /// BigInt wrapper extractor. Returns the legacy
    /// [`crate::bigint::BigIntValue`] when this value carries a BigInt
    /// body.
    #[inline]
    #[must_use]
    pub fn as_big_int(self) -> Option<crate::bigint::BigIntValue> {
        if !self.is_big_int_gc() {
            return None;
        }
        Some(crate::bigint::BigIntValue::from_handle(
            self.as_big_int_gc()?,
        ))
    }

    /// Symbol wrapper constructor. Equivalent to legacy
    /// `Value::Symbol(s)`.
    #[inline]
    #[must_use]
    pub fn symbol(s: crate::symbol::JsSymbol) -> Self {
        Self::symbol_gc(s.handle())
    }

    /// Symbol wrapper extractor. Rebuilds the [`crate::symbol::JsSymbol`]
    /// wrapper (handle + cached description/well-known/registered) by
    /// reading the body once.
    #[inline]
    #[must_use]
    pub fn as_symbol(self, heap: &otter_gc::GcHeap) -> Option<crate::symbol::JsSymbol> {
        let handle = self.as_symbol_gc()?;
        Some(crate::symbol::JsSymbol::from_handle(heap, handle))
    }

    /// Temporal wrapper constructor. Equivalent to legacy
    /// `Value::Temporal(t)`.
    #[inline]
    #[must_use]
    pub fn temporal(t: crate::temporal::JsTemporal) -> Self {
        Self::temporal_gc(t.handle())
    }

    /// Temporal wrapper extractor.
    #[inline]
    #[must_use]
    pub fn as_temporal(self, heap: &otter_gc::GcHeap) -> Option<crate::temporal::JsTemporal> {
        let handle = self.as_temporal_gc()?;
        Some(crate::temporal::JsTemporal::from_handle(heap, handle))
    }

    /// Intl wrapper constructor. Equivalent to legacy `Value::Intl(i)`.
    #[inline]
    #[must_use]
    pub fn intl(i: crate::intl::JsIntl) -> Self {
        Self::intl_gc(i.handle())
    }

    /// Intl wrapper extractor.
    #[inline]
    #[must_use]
    pub fn as_intl(self, heap: &otter_gc::GcHeap) -> Option<crate::intl::JsIntl> {
        let handle = self.as_intl_gc()?;
        Some(crate::intl::JsIntl::from_handle(heap, handle))
    }

    /// Proxy wrapper constructor. Equivalent to legacy
    /// `Value::Proxy(p)`.
    #[inline]
    #[must_use]
    pub fn proxy(p: crate::proxy::JsProxy) -> Self {
        Self::proxy_gc(p.handle())
    }

    /// Proxy wrapper extractor.
    #[inline]
    #[must_use]
    pub fn as_proxy(self) -> Option<crate::proxy::JsProxy> {
        Some(crate::proxy::JsProxy::from_handle(self.as_proxy_gc()?))
    }

    /// DataView wrapper constructor. Equivalent to legacy
    /// `Value::DataView(v)`.
    #[inline]
    #[must_use]
    pub fn data_view(v: crate::binary::JsDataView) -> Self {
        Self::data_view_gc(v.handle())
    }

    /// DataView wrapper extractor.
    #[inline]
    #[must_use]
    pub fn as_data_view(self) -> Option<crate::binary::JsDataView> {
        Some(crate::binary::JsDataView::from_handle(
            self.as_data_view_gc()?,
        ))
    }

    /// TypedArray wrapper constructor. Equivalent to legacy
    /// `Value::TypedArray(t)`.
    #[inline]
    #[must_use]
    pub fn typed_array(t: crate::binary::JsTypedArray) -> Self {
        Self::typed_array_gc(t.handle())
    }

    /// TypedArray wrapper extractor. Reads the body once for the
    /// cached element-kind discriminator.
    #[inline]
    #[must_use]
    pub fn as_typed_array(self, heap: &otter_gc::GcHeap) -> Option<crate::binary::JsTypedArray> {
        let handle = self.as_typed_array_gc()?;
        Some(crate::binary::JsTypedArray::from_handle_with_heap(
            heap, handle,
        ))
    }

    /// ArrayBuffer wrapper constructor. Equivalent to legacy
    /// `Value::ArrayBuffer(b)` — dispatches on `storage` to the
    /// Local / Shared GC family.
    #[inline]
    #[must_use]
    pub fn array_buffer(b: crate::binary::JsArrayBuffer) -> Self {
        match b.storage() {
            crate::binary::BufferStorage::Local(h) => Self::local_array_buffer_gc(h),
            crate::binary::BufferStorage::Shared(h) => Self::shared_array_buffer_gc(h),
        }
    }

    /// ArrayBuffer wrapper extractor. Recognises both Local and
    /// Shared GC bodies and rebuilds the wrapper.
    #[inline]
    #[must_use]
    pub fn as_array_buffer(self) -> Option<crate::binary::JsArrayBuffer> {
        if let Some(h) = self.as_local_array_buffer_gc() {
            return Some(crate::binary::JsArrayBuffer::from_local_handle(h));
        }
        if let Some(h) = self.as_shared_array_buffer_gc() {
            return Some(crate::binary::JsArrayBuffer::from_shared_handle(h));
        }
        None
    }

    /// `true` when the value is in the BigInt body family.
    #[inline]
    #[must_use]
    pub fn is_big_int(self) -> bool {
        self.is_big_int_gc()
    }

    /// `true` when the value is in the Symbol body family.
    #[inline]
    #[must_use]
    pub fn is_symbol(self) -> bool {
        self.is_symbol_gc()
    }

    /// `true` when the value is in the Temporal body family.
    #[inline]
    #[must_use]
    pub fn is_temporal(self) -> bool {
        self.is_temporal_gc()
    }

    /// `true` when the value is in the Intl body family.
    #[inline]
    #[must_use]
    pub fn is_intl(self) -> bool {
        self.is_intl_gc()
    }

    /// `true` when the value is in the Proxy body family.
    #[inline]
    #[must_use]
    pub fn is_proxy(self) -> bool {
        self.is_proxy_gc()
    }

    /// `true` when the value is in the DataView body family.
    #[inline]
    #[must_use]
    pub fn is_data_view(self) -> bool {
        self.is_data_view_gc()
    }

    /// `true` when the value is in the TypedArray body family.
    #[inline]
    #[must_use]
    pub fn is_typed_array(self) -> bool {
        self.is_typed_array_gc()
    }

    /// `true` when the value is in either Local or Shared ArrayBuffer
    /// body family.
    #[inline]
    #[must_use]
    pub fn is_array_buffer(self) -> bool {
        self.is_array_buffer_gc()
    }

    /// `true` when the value is a bytecode function reference.
    /// Alias for [`Self::is_function_id`] matching the legacy
    /// `Value::Function { function_id }` shape.
    #[inline]
    #[must_use]
    pub fn is_function(self) -> bool {
        self.is_function_id()
    }

    /// Convenience: bytecode function id (alias for
    /// [`Self::as_function_id`]).
    #[inline]
    #[must_use]
    pub fn as_function(self) -> Option<u32> {
        self.as_function_id()
    }

    /// Convenience: build a `Value::Function { function_id }`
    /// equivalent. Alias for [`Self::function_id`].
    #[inline]
    #[must_use]
    pub const fn function(function_id: u32) -> Self {
        Self::function_id(function_id)
    }

    // -----------------------------------------------------------------------
    // Coarse family-kind dispatch.
    //
    // Single match against `GcHeader::type_tag()` returning a typed
    // enum. Cheaper than calling `is_array() || is_map() || …` because
    // the header read happens once. Use these when the call site is
    // about to switch on multiple body kinds.
    // -----------------------------------------------------------------------

    /// Classify a `TAG_PTR_OBJECT` value into its concrete body kind.
    /// Returns `None` when the value is not in the object family.
    #[inline]
    #[must_use]
    pub fn object_family_kind(self) -> Option<ObjectFamilyKind> {
        if !self.is_object_like() {
            return None;
        }
        let tag = self.read_gc_type_tag()?;
        Some(match tag {
            t if t == <ObjectBody as otter_gc::SafeTraceable>::TYPE_TAG => ObjectFamilyKind::Object,
            t if t == <ArrayBody as otter_gc::SafeTraceable>::TYPE_TAG => ObjectFamilyKind::Array,
            t if t == <MapBody as otter_gc::SafeTraceable>::TYPE_TAG => ObjectFamilyKind::Map,
            t if t == <SetBody as otter_gc::SafeTraceable>::TYPE_TAG => ObjectFamilyKind::Set,
            t if t == <WeakMapBody as otter_gc::SafeTraceable>::TYPE_TAG => {
                ObjectFamilyKind::WeakMap
            }
            t if t == <WeakSetBody as otter_gc::SafeTraceable>::TYPE_TAG => {
                ObjectFamilyKind::WeakSet
            }
            t if t == <WeakRefBody as otter_gc::SafeTraceable>::TYPE_TAG => {
                ObjectFamilyKind::WeakRef
            }
            t if t == <FinalizationRegistryBody as otter_gc::SafeTraceable>::TYPE_TAG => {
                ObjectFamilyKind::FinalizationRegistry
            }
            t if t == <PurePromiseBody as otter_gc::SafeTraceable>::TYPE_TAG => {
                ObjectFamilyKind::Promise
            }
            t if t == <IteratorState as otter_gc::SafeTraceable>::TYPE_TAG => {
                ObjectFamilyKind::Iterator
            }
            t if t == <GeneratorBody as otter_gc::SafeTraceable>::TYPE_TAG => {
                ObjectFamilyKind::Generator
            }
            t if t == <JsRegExpBody as otter_gc::SafeTraceable>::TYPE_TAG => {
                ObjectFamilyKind::RegExp
            }
            t if t == <TemporalBody as otter_gc::SafeTraceable>::TYPE_TAG => {
                ObjectFamilyKind::Temporal
            }
            t if t == <IntlBody as otter_gc::SafeTraceable>::TYPE_TAG => ObjectFamilyKind::Intl,
            t if t == <ProxyBodyGc as otter_gc::SafeTraceable>::TYPE_TAG => ObjectFamilyKind::Proxy,
            t if t == <DataViewBodyGc as otter_gc::SafeTraceable>::TYPE_TAG => {
                ObjectFamilyKind::DataView
            }
            t if t == <TypedArrayBodyGc as otter_gc::SafeTraceable>::TYPE_TAG => {
                ObjectFamilyKind::TypedArray
            }
            t if t == <LocalArrayBufferBodyGc as otter_gc::SafeTraceable>::TYPE_TAG => {
                ObjectFamilyKind::LocalArrayBuffer
            }
            t if t == <SharedArrayBufferBodyGc as otter_gc::SafeTraceable>::TYPE_TAG => {
                ObjectFamilyKind::SharedArrayBuffer
            }
            _ => ObjectFamilyKind::Unknown,
        })
    }

    /// Classify a callable heap cell into its concrete body kind.
    /// Returns `None` for non-callable values (including `function_id`
    /// immediates, which are bytecode-only).
    #[inline]
    #[must_use]
    pub fn function_family_kind(self) -> Option<FunctionFamilyKind> {
        if self.ptr_family() != Some(PtrFamily::Function) {
            return None;
        }
        let tag = self.read_gc_type_tag()?;
        Some(match tag {
            JS_CLOSURE_BODY_TYPE_TAG => FunctionFamilyKind::Closure,
            t if t == <BoundFunctionBody as otter_gc::SafeTraceable>::TYPE_TAG => {
                FunctionFamilyKind::Bound
            }
            t if t == <NativeFunctionBody as otter_gc::SafeTraceable>::TYPE_TAG => {
                FunctionFamilyKind::Native
            }
            t if t == <ClassConstructorBody as otter_gc::SafeTraceable>::TYPE_TAG => {
                FunctionFamilyKind::ClassConstructor
            }
            _ => FunctionFamilyKind::Unknown,
        })
    }

    /// Classify a `TAG_PTR_OTHER` value into its concrete body kind.
    /// Returns `None` for non-other-family values.
    #[inline]
    #[must_use]
    pub fn other_family_kind(self) -> Option<OtherFamilyKind> {
        if !self.is_other_primitive() {
            return None;
        }
        let tag = self.read_gc_type_tag()?;
        Some(match tag {
            t if t == <SymbolBody as otter_gc::SafeTraceable>::TYPE_TAG => OtherFamilyKind::Symbol,
            t if t == <BigIntBody as otter_gc::SafeTraceable>::TYPE_TAG => OtherFamilyKind::BigInt,
            _ => OtherFamilyKind::Unknown,
        })
    }

    // -----------------------------------------------------------------------
    // GC tracing
    // -----------------------------------------------------------------------

    /// Walk the `Gc<…>` slot held directly inside `self` and yield its
    /// slot pointer to `visitor`.
    ///
    /// A heap-cell `Value` stores the full `cage_base + offset` address;
    /// its low 32 bits are the compressed GC offset (the cage is 4 GiB-
    /// aligned) and its high 32 bits are the constant cage prefix. The
    /// low word lives at byte offset `0..4` on little-endian targets, so
    /// `&self.0` cast to `*mut RawGc` is a valid offset slot: the
    /// collector rewrites those 4 bytes in place on relocation and the
    /// full address tracks the move automatically.
    ///
    /// Immediates and numbers hold no GC slot and are skipped.
    #[allow(dead_code)]
    pub fn trace_value_slots(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        if is_cell_bits(self.0) {
            let slot = &self.0 as *const u64 as *mut otter_gc::raw::RawGc;
            visitor(slot);
        }
    }

    /// Visit this value as an explicitly mutable root slot.
    pub(crate) fn trace_value_slot_mut(&mut self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        if is_cell_bits(self.0) {
            let slot = &mut self.0 as *mut u64 as *mut otter_gc::raw::RawGc;
            visitor(slot);
        }
    }
}

/// Default to `undefined`.
impl Default for Value {
    #[inline]
    fn default() -> Self {
        Self::UNDEFINED
    }
}

/// Outgoing GC edge enumeration for the write barrier.
///
/// Every pointer-tagged variant emits at most one edge — the
/// 32-bit GC offset packed in the low 32 bits of `self.0`.
/// Immediate variants emit none.
impl otter_gc::GcStore for Value {
    fn visit_gc_edges(&self, visitor: &mut dyn FnMut(otter_gc::GcEdge)) {
        if let Some(raw) = self.as_raw_gc()
            && let Some(edge) = otter_gc::GcEdge::from_raw(raw)
        {
            visitor(edge);
        }
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
            ValueKind::PtrObject => write!(f, "Value::PtrObject(0x{:08x})", cell_offset(self.0)),
            ValueKind::PtrString => write!(f, "Value::PtrString(0x{:08x})", cell_offset(self.0)),
            ValueKind::PtrFunction => {
                write!(f, "Value::PtrFunction(0x{:08x})", cell_offset(self.0))
            }
            ValueKind::PtrOther => write!(f, "Value::PtrOther(0x{:08x})", cell_offset(self.0)),
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
        assert_eq!(
            nan_a, nan_b,
            "all NaNs canonicalise to the same bit pattern"
        );
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
    fn cell_offset_round_trips_through_full_address() {
        use crate::object::alloc_object_with_roots;
        use otter_gc::GcHeap;
        use otter_gc::raw::RawGc;

        let mut heap = GcHeap::new().expect("heap");
        let mut roots = |_v: &mut dyn FnMut(*mut RawGc)| {};
        let obj = alloc_object_with_roots(&mut heap, &mut roots).expect("alloc");
        let offset = obj.raw().0;
        let v = Value::object(obj);
        // A cell value carries the full cage_base + offset address with the
        // compressed offset verbatim in its low 32 bits.
        assert!(v.is_cell());
        assert_eq!(v.as_raw_gc().unwrap().0, offset);
        assert!(v.is_object_like());
        // Immediates and numbers are never cells.
        assert!(!Value::undefined().is_cell());
        assert!(!Value::number_i32(7).is_cell());
        assert!(!Value::number_f64(1.5).is_cell());
        assert!(!Value::function_id(3).is_cell());
    }

    #[test]
    fn closure_round_trip_via_real_heap() {
        use crate::{Value as LegacyValue, alloc_closure, alloc_upvalue};
        use otter_gc::GcHeap;

        let mut heap = GcHeap::new().expect("heap");
        let cell = alloc_upvalue(&mut heap, LegacyValue::undefined()).expect("cell");
        let upvalues = vec![cell];
        let closure =
            alloc_closure(&mut heap, 99, upvalues, None, None, None, None).expect("alloc");
        let v = Value::closure(closure);
        assert!(v.is_callable());
        assert!(!v.is_function_id());
        assert_eq!(v.as_closure(&heap), Some(closure));
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
        assert_eq!(v.as_closure(&heap), None);
    }

    #[test]
    fn family_kind_dispatch_separates_object_function_other() {
        use crate::object::alloc_object_with_roots;
        use crate::{Value as LegacyValue, alloc_closure, alloc_upvalue};
        use otter_gc::GcHeap;
        use otter_gc::raw::RawGc;

        let mut heap = GcHeap::new().expect("heap");
        let mut roots = |_v: &mut dyn FnMut(*mut RawGc)| {};
        let obj = alloc_object_with_roots(&mut heap, &mut roots).expect("obj");
        let cell = alloc_upvalue(&mut heap, LegacyValue::undefined()).expect("cell");
        let closure =
            alloc_closure(&mut heap, 1, vec![cell], None, None, None, None).expect("closure");

        let vobj = Value::object(obj);
        let vclo = Value::closure(closure);
        let vfid = Value::function_id(0);

        // Object dispatch.
        assert_eq!(vobj.object_family_kind(), Some(ObjectFamilyKind::Object));
        assert_eq!(vobj.function_family_kind(), None);
        assert_eq!(vobj.other_family_kind(), None);

        // Function dispatch.
        assert_eq!(
            vclo.function_family_kind(),
            Some(FunctionFamilyKind::Closure)
        );
        assert_eq!(vclo.object_family_kind(), None);
        assert_eq!(vclo.other_family_kind(), None);

        // function_id doesn't classify as TAG_PTR_FUNCTION.
        assert_eq!(vfid.function_family_kind(), None);
        assert_eq!(vfid.object_family_kind(), None);

        // Immediates don't classify either.
        assert_eq!(Value::undefined().object_family_kind(), None);
        assert_eq!(Value::number_i32(0).function_family_kind(), None);
    }

    #[test]
    fn symbol_gc_round_trip_via_real_heap_and_disambiguates_from_bigint() {
        use crate::bigint::alloc_big_int;
        use crate::symbol::{WellKnown, alloc_symbol};
        use num_bigint::BigInt;
        use otter_gc::GcHeap;

        let mut heap = GcHeap::new().expect("heap");
        let sym = alloc_symbol(&mut heap, None, Some(WellKnown::Iterator), false).expect("sym");
        let big = alloc_big_int(&mut heap, BigInt::from(7)).expect("big");

        let vsym = Value::symbol_gc(sym);
        let vbig = Value::big_int_gc(big);

        // Both share TAG_PTR_OTHER but disambiguate via GcHeader::type_tag.
        assert!(vsym.is_other_primitive());
        assert!(vbig.is_other_primitive());
        assert!(vsym.is_symbol_gc());
        assert!(!vsym.is_big_int_gc());
        assert!(vbig.is_big_int_gc());
        assert!(!vbig.is_symbol_gc());

        // Each accessor recovers its own body and rejects the foreign one.
        assert_eq!(vsym.as_symbol_gc(), Some(sym));
        assert_eq!(vsym.as_big_int_gc(), None);
        assert_eq!(vbig.as_big_int_gc(), Some(big));
        assert_eq!(vbig.as_symbol_gc(), None);
    }

    #[test]
    fn big_int_gc_round_trip_via_real_heap() {
        use crate::bigint::alloc_big_int;
        use num_bigint::BigInt;
        use otter_gc::GcHeap;

        let mut heap = GcHeap::new().expect("heap");
        let payload = BigInt::from(2_i128.pow(70));
        let handle = alloc_big_int(&mut heap, payload.clone()).expect("alloc");
        let v = Value::big_int_gc(handle);
        assert!(v.is_other_primitive());
        assert!(v.is_big_int_gc());
        assert_eq!(v.kind(), ValueKind::PtrOther);
        assert_eq!(v.as_big_int_gc(), Some(handle));
        // Not a string, not callable, not object-like.
        assert_eq!(v.as_string_gc(), None);
        assert!(!v.is_callable());
        assert!(!v.is_object_like());
        // Payload survives round-trip.
        heap.read_payload(handle, |body| assert_eq!(body.inner, payload));
    }

    #[test]
    fn string_gc_round_trip_via_real_heap() {
        use crate::string::{JsStringId, alloc_flat_string_body_with_roots};
        use otter_gc::GcHeap;
        use otter_gc::raw::RawGc;

        let mut heap = GcHeap::new().expect("heap");
        let mut roots = |_v: &mut dyn FnMut(*mut RawGc)| {};
        let units = [b'a' as u16, b'b' as u16, b'c' as u16];
        let body =
            alloc_flat_string_body_with_roots(&mut heap, JsStringId::new(1), &units, &mut roots)
                .expect("string");
        let v = Value::string_gc(body);
        assert!(v.is_string());
        assert_eq!(v.kind(), ValueKind::PtrString);
        assert_eq!(v.as_string_gc(), Some(body));
        // String must not collapse into object-family or callable-family.
        assert_eq!(v.as_object(), None);
        assert_eq!(v.as_closure(&heap), None);
        // Typeof reads purely from the tag.
        assert_eq!(v.typeof_pure(), Some("string"));
    }

    #[test]
    fn typeof_pure_returns_spec_strings_for_decidable_kinds() {
        assert_eq!(Value::undefined().typeof_pure(), Some("undefined"));
        assert_eq!(Value::hole().typeof_pure(), Some("undefined"));
        assert_eq!(Value::null().typeof_pure(), Some("object"));
        assert_eq!(Value::boolean(true).typeof_pure(), Some("boolean"));
        assert_eq!(Value::number_i32(0).typeof_pure(), Some("number"));
        assert_eq!(Value::number_f64(f64::NAN).typeof_pure(), Some("number"));
        assert_eq!(Value::function_id(0).typeof_pure(), Some("function"));

        use crate::string::{JsStringId, alloc_flat_string_body_with_roots};
        use crate::symbol::{WellKnown, alloc_symbol};
        use crate::{Value as LegacyValue, alloc_closure, alloc_upvalue};
        use otter_gc::GcHeap;
        use otter_gc::raw::RawGc;
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = |_v: &mut dyn FnMut(*mut RawGc)| {};
        let cell = alloc_upvalue(&mut heap, LegacyValue::undefined()).expect("cell");
        let closure = alloc_closure(&mut heap, 1, vec![cell], None, None, None, None).expect("clo");
        assert_eq!(Value::closure(closure).typeof_pure(), Some("function"));
        let body = alloc_flat_string_body_with_roots(
            &mut heap,
            JsStringId::new(1),
            &[b'a' as u16],
            &mut roots,
        )
        .expect("string");
        assert_eq!(Value::string_gc(body).typeof_pure(), Some("string"));
        // Symbol / bigint need heap-side primitives to finish typeof.
        let sym = alloc_symbol(&mut heap, None, Some(WellKnown::Iterator), false).expect("sym");
        assert_eq!(Value::symbol_gc(sym).typeof_pure(), None);
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
        assert_eq!(
            Value::number_f64(f64::INFINITY).to_boolean_pure(),
            Some(true)
        );

        // Callables / object-like references are always truthy.
        assert_eq!(Value::function_id(0).to_boolean_pure(), Some(true));

        use crate::object::alloc_object_with_roots;
        use crate::string::{JsStringId, alloc_flat_string_body_with_roots};
        use crate::{Value as LegacyValue, alloc_closure, alloc_upvalue};
        use otter_gc::GcHeap;
        use otter_gc::raw::RawGc;
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = |_v: &mut dyn FnMut(*mut RawGc)| {};
        let obj = alloc_object_with_roots(&mut heap, &mut roots).expect("obj");
        let cell = alloc_upvalue(&mut heap, LegacyValue::undefined()).expect("cell");
        let closure = alloc_closure(&mut heap, 1, vec![cell], None, None, None, None).expect("clo");
        assert_eq!(Value::object(obj).to_boolean_pure(), Some(true));
        assert_eq!(Value::closure(closure).to_boolean_pure(), Some(true));

        // Strings need a length probe to finish ToBoolean.
        let body = alloc_flat_string_body_with_roots(
            &mut heap,
            JsStringId::new(1),
            &[b'a' as u16],
            &mut roots,
        )
        .expect("string");
        assert_eq!(Value::string_gc(body).to_boolean_pure(), None);
    }

    #[test]
    fn predicates_disambiguate_object_and_function_families() {
        use crate::object::alloc_object_with_roots;
        use crate::{Value as LegacyValue, alloc_closure, alloc_upvalue};
        use otter_gc::GcHeap;
        use otter_gc::raw::RawGc;

        let mut heap = GcHeap::new().expect("heap");
        let mut roots = |_v: &mut dyn FnMut(*mut RawGc)| {};
        let obj = alloc_object_with_roots(&mut heap, &mut roots).expect("alloc");
        let cell = alloc_upvalue(&mut heap, LegacyValue::undefined()).expect("cell");
        let closure =
            alloc_closure(&mut heap, 1, vec![cell], None, None, None, None).expect("closure");

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
        let heap = otter_gc::GcHeap::new().expect("heap");
        let v = Value::function_id(0);
        assert_eq!(v.as_closure(&heap), None);
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

        use crate::bigint::alloc_big_int;
        use crate::object::alloc_object_with_roots;
        use crate::string::{JsStringId, alloc_flat_string_body_with_roots};
        use crate::{Value as LegacyValue, alloc_closure, alloc_upvalue};
        use num_bigint::BigInt;
        use otter_gc::GcHeap;
        use otter_gc::raw::RawGc;
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = |_v: &mut dyn FnMut(*mut RawGc)| {};
        let obj = alloc_object_with_roots(&mut heap, &mut roots).expect("obj");
        let cell = alloc_upvalue(&mut heap, LegacyValue::undefined()).expect("cell");
        let closure = alloc_closure(&mut heap, 1, vec![cell], None, None, None, None).expect("clo");
        let body = alloc_flat_string_body_with_roots(
            &mut heap,
            JsStringId::new(1),
            &[b'a' as u16],
            &mut roots,
        )
        .expect("string");
        let big = alloc_big_int(&mut heap, BigInt::from(7)).expect("big");
        assert_eq!(Value::object(obj).kind(), ValueKind::PtrObject);
        assert_eq!(Value::string_gc(body).kind(), ValueKind::PtrString);
        assert_eq!(Value::closure(closure).kind(), ValueKind::PtrFunction);
        assert_eq!(Value::big_int_gc(big).kind(), ValueKind::PtrOther);
    }
}
