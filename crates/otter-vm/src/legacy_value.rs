//! Legacy 30-variant enum value model.
//!
//! **Migration target.** The 8-byte tagged replacement lives at
//! [`crate::value::Value`]. Once every `Value::Variant(…)` pattern
//! across the workspace migrates to the tagged accessor surface
//! (`Value::variant(…)` / `Value::as_variant(…)` / `Value::is_variant`),
//! this enum is deleted and `pub use crate::value::Value` becomes
//! the canonical crate root export.
//!
//! Tracking:
//! - `docs/value-cutover-plan.md` — ordered migration list.
//! - `docs/architecture-refactor-plan-2026-05.md` §Phase 1 — design.
//!
//! # Contents
//! - [`Value`] — the 30-variant enum itself.
//! - `impl Value` — Phase-1 GC tracing helpers
//!   (`as_gc_raw` / `trace_value_slots`) plus convenience ctors and
//!   accessors that the rest of the crate calls.
//! - [`otter_gc::GcStore`] impl — exposes outgoing GC edges to the
//!   collector.
//! - `impl PartialEq for Value` — identity-shaped strict-equality
//!   used by `===` (spec value-equality for strings / BigInt flows
//!   through `abstract_ops::strict_equality` with heap access).

use otter_gc::raw::{RawGc, SlotVisitor};

use crate::array::JsArray;
use crate::bigint;
use crate::bound_function::BoundFunction;
use crate::class_constructor::ClassConstructor;
use crate::collections::{JsMap, JsSet, JsWeakMap, JsWeakSet};
use crate::iterator_state::IteratorHandle;
use crate::native_function::NativeFunction;
use crate::number::{self, NumberValue};
use crate::object::JsObject;
use crate::promise::{JsPromise, JsPromiseHandle};
use crate::regexp::JsRegExp;
use crate::string::JsString;
use crate::symbol::JsSymbol;
use crate::weak_refs::{JsFinalizationRegistry, JsWeakRef};
use crate::{JsIntl, JsTemporal};

/// Legacy 30-variant enum value model.
///
/// **Migration target.** See module-level docs.
#[derive(Debug, Clone, Copy)]
pub enum Value {
    /// JS `undefined`.
    Undefined,
    /// Internal "array hole" sentinel used by sparse arrays.
    ///
    /// Distinguishes a missing dense slot from an explicit
    /// `undefined` element so `in`, `Object.keys`, and
    /// `Array.prototype` callbacks (`forEach`, `map`, `filter`, …)
    /// can skip absent indices per ECMA-262 §10.4.2 / §23.1.3.
    /// User code never observes this variant: every public read
    /// path (`array::get`, `array::get_named_property`,
    /// JSON.stringify, etc.) maps it back to `Value::Undefined`.
    /// Display / typeof / coercion behave like `Undefined` as a
    /// defensive fallback in case an internal leak occurs.
    Hole,
    /// JS `null`.
    Null,
    /// JS `true` / `false`.
    Boolean(bool),
    /// JS Number (smi + double; see [`NumberValue`]).
    Number(NumberValue),
    /// JS BigInt — arbitrary-precision integer.
    BigInt(bigint::BigIntValue),
    /// JS string. Storage is WTF-16 with cons / sliced ropes.
    String(JsString),
    /// JS Symbol primitive.
    Symbol(JsSymbol),
    /// JS function — closure-less reference to a bytecode function.
    Function {
        /// Index into [`otter_bytecode::BytecodeModule::functions`].
        function_id: u32,
    },
    /// JS object — heap-shared, mutable.
    Object(JsObject),
    /// JS array — dense, heap-shared.
    Array(JsArray),
    /// Closure — function with captured upvalues.
    Closure(crate::closure::JsClosure),
    /// Result of `Function.prototype.bind`.
    BoundFunction(BoundFunction),
    /// Host-implemented callable.
    NativeFunction(NativeFunction),
    /// Internal iterator state.
    Iterator(IteratorHandle),
    /// Compiled regular-expression value.
    RegExp(JsRegExp),
    /// JS Promise.
    Promise(JsPromiseHandle),
    /// JS `Map`.
    Map(JsMap),
    /// JS `Set`.
    Set(JsSet),
    /// JS `WeakMap`.
    WeakMap(JsWeakMap),
    /// JS `WeakSet`.
    WeakSet(JsWeakSet),
    /// JS `WeakRef`.
    WeakRef(JsWeakRef),
    /// JS `FinalizationRegistry`.
    FinalizationRegistry(JsFinalizationRegistry),
    /// `Temporal.*` value.
    Temporal(JsTemporal),
    /// `Intl.*` value.
    Intl(JsIntl),
    /// JS `ArrayBuffer`.
    ArrayBuffer(crate::binary::JsArrayBuffer),
    /// JS `DataView`.
    DataView(crate::binary::JsDataView),
    /// JS `TypedArray`.
    TypedArray(crate::binary::JsTypedArray),
    /// Generator object.
    Generator(crate::generator::JsGenerator),
    /// JS Proxy.
    Proxy(crate::proxy::JsProxy),
    /// Class value — wraps the constructor callable, the prototype,
    /// and the static-side object.
    ClassConstructor(ClassConstructor),
}

impl Value {
    // -----------------------------------------------------------------
    // Phase-C cut-over compat layer.
    //
    // Snake-case constructors mirror the tagged `value::Value` surface
    // so call sites can migrate `Value::Variant(x)` → `Value::variant(x)`
    // mechanically before the legacy enum itself is retired. Each
    // helper is a trivial wrapper around the matching enum variant —
    // no behavioural change.
    // -----------------------------------------------------------------

    /// `Value::Undefined` constructor.
    #[inline]
    #[must_use]
    pub const fn undefined() -> Self {
        Self::Undefined
    }

    /// `Value::Null` constructor.
    #[inline]
    #[must_use]
    pub const fn null() -> Self {
        Self::Null
    }

    /// `Value::Hole` constructor.
    #[inline]
    #[must_use]
    pub const fn hole() -> Self {
        Self::Hole
    }

    /// `Value::Boolean(b)` constructor.
    #[inline]
    #[must_use]
    pub const fn boolean(b: bool) -> Self {
        Self::Boolean(b)
    }

    /// `Value::Number(n)` constructor.
    #[inline]
    #[must_use]
    pub const fn number(n: NumberValue) -> Self {
        Self::Number(n)
    }

    /// Convenience: `Value::Number(NumberValue::from_i32(n))`.
    #[inline]
    #[must_use]
    pub const fn number_i32(n: i32) -> Self {
        Self::Number(NumberValue::from_i32(n))
    }

    /// Convenience: `Value::Number(NumberValue::from_f64(d))`.
    #[inline]
    #[must_use]
    pub fn number_f64(d: f64) -> Self {
        Self::Number(NumberValue::from_f64(d))
    }

    /// `Value::BigInt(b)` constructor.
    #[inline]
    #[must_use]
    pub const fn big_int(b: bigint::BigIntValue) -> Self {
        Self::BigInt(b)
    }

    /// `Value::String(s)` constructor.
    #[inline]
    #[must_use]
    pub const fn string(s: JsString) -> Self {
        Self::String(s)
    }

    /// `Value::Symbol(s)` constructor.
    #[inline]
    #[must_use]
    pub const fn symbol(s: JsSymbol) -> Self {
        Self::Symbol(s)
    }

    /// `Value::Function { function_id }` constructor.
    #[inline]
    #[must_use]
    pub const fn function(function_id: u32) -> Self {
        Self::Function { function_id }
    }

    /// `Value::Object(o)` constructor.
    #[inline]
    #[must_use]
    pub const fn object(o: JsObject) -> Self {
        Self::Object(o)
    }

    /// `Value::Array(a)` constructor.
    #[inline]
    #[must_use]
    pub const fn array(a: JsArray) -> Self {
        Self::Array(a)
    }

    /// `Value::Closure(c)` constructor.
    #[inline]
    #[must_use]
    pub const fn closure(c: crate::closure::JsClosure) -> Self {
        Self::Closure(c)
    }

    /// `Value::BoundFunction(b)` constructor.
    #[inline]
    #[must_use]
    pub const fn bound_function(b: BoundFunction) -> Self {
        Self::BoundFunction(b)
    }

    /// `Value::NativeFunction(n)` constructor.
    #[inline]
    #[must_use]
    pub const fn native_function(n: NativeFunction) -> Self {
        Self::NativeFunction(n)
    }

    /// `Value::Iterator(i)` constructor.
    #[inline]
    #[must_use]
    pub const fn iterator(i: IteratorHandle) -> Self {
        Self::Iterator(i)
    }

    /// `Value::RegExp(r)` constructor.
    #[inline]
    #[must_use]
    pub const fn regexp(r: JsRegExp) -> Self {
        Self::RegExp(r)
    }

    /// `Value::Promise(p)` constructor.
    #[inline]
    #[must_use]
    pub const fn promise(p: JsPromiseHandle) -> Self {
        Self::Promise(p)
    }

    /// `Value::Map(m)` constructor.
    #[inline]
    #[must_use]
    pub const fn map(m: JsMap) -> Self {
        Self::Map(m)
    }

    /// `Value::Set(s)` constructor.
    #[inline]
    #[must_use]
    pub const fn set(s: JsSet) -> Self {
        Self::Set(s)
    }

    /// `Value::WeakMap(m)` constructor.
    #[inline]
    #[must_use]
    pub const fn weak_map(m: JsWeakMap) -> Self {
        Self::WeakMap(m)
    }

    /// `Value::WeakSet(s)` constructor.
    #[inline]
    #[must_use]
    pub const fn weak_set(s: JsWeakSet) -> Self {
        Self::WeakSet(s)
    }

    /// `Value::WeakRef(w)` constructor.
    #[inline]
    #[must_use]
    pub const fn weak_ref(w: JsWeakRef) -> Self {
        Self::WeakRef(w)
    }

    /// `Value::FinalizationRegistry(r)` constructor.
    #[inline]
    #[must_use]
    pub const fn finalization_registry(r: JsFinalizationRegistry) -> Self {
        Self::FinalizationRegistry(r)
    }

    /// `Value::Temporal(t)` constructor.
    #[inline]
    #[must_use]
    pub const fn temporal(t: JsTemporal) -> Self {
        Self::Temporal(t)
    }

    /// `Value::Intl(i)` constructor.
    #[inline]
    #[must_use]
    pub const fn intl(i: JsIntl) -> Self {
        Self::Intl(i)
    }

    /// `Value::ArrayBuffer(b)` constructor.
    #[inline]
    #[must_use]
    pub const fn array_buffer(b: crate::binary::JsArrayBuffer) -> Self {
        Self::ArrayBuffer(b)
    }

    /// `Value::DataView(v)` constructor.
    #[inline]
    #[must_use]
    pub const fn data_view(v: crate::binary::JsDataView) -> Self {
        Self::DataView(v)
    }

    /// `Value::TypedArray(t)` constructor.
    #[inline]
    #[must_use]
    pub const fn typed_array(t: crate::binary::JsTypedArray) -> Self {
        Self::TypedArray(t)
    }

    /// `Value::Generator(g)` constructor.
    #[inline]
    #[must_use]
    pub const fn generator(g: crate::generator::JsGenerator) -> Self {
        Self::Generator(g)
    }

    /// `Value::Proxy(p)` constructor.
    #[inline]
    #[must_use]
    pub const fn proxy(p: crate::proxy::JsProxy) -> Self {
        Self::Proxy(p)
    }

    /// `Value::ClassConstructor(c)` constructor.
    #[inline]
    #[must_use]
    pub const fn class_constructor(c: ClassConstructor) -> Self {
        Self::ClassConstructor(c)
    }

    // -----------------------------------------------------------------
    // Phase-C cut-over compat layer — predicates.
    //
    // Mirror the tagged `value::Value::is_*` surface so call sites can
    // migrate boolean tests independently of pattern matches.
    // -----------------------------------------------------------------

    /// `true` if `self == Value::Undefined`.
    #[inline]
    #[must_use]
    pub const fn is_undefined(&self) -> bool {
        matches!(self, Value::Undefined)
    }

    /// `true` if `self == Value::Null`.
    #[inline]
    #[must_use]
    pub const fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// `true` if `self == Value::Hole`.
    #[inline]
    #[must_use]
    pub const fn is_hole(&self) -> bool {
        matches!(self, Value::Hole)
    }

    /// `true` if `self` is a boolean.
    #[inline]
    #[must_use]
    pub const fn is_boolean(&self) -> bool {
        matches!(self, Value::Boolean(_))
    }

    /// `true` if `self` is numeric.
    #[inline]
    #[must_use]
    pub const fn is_number(&self) -> bool {
        matches!(self, Value::Number(_))
    }

    /// `true` if `self` is a BigInt.
    #[inline]
    #[must_use]
    pub const fn is_big_int(&self) -> bool {
        matches!(self, Value::BigInt(_))
    }

    /// `true` if `self` is a string.
    #[inline]
    #[must_use]
    pub const fn is_string(&self) -> bool {
        matches!(self, Value::String(_))
    }

    /// `true` if `self` is a symbol.
    #[inline]
    #[must_use]
    pub const fn is_symbol(&self) -> bool {
        matches!(self, Value::Symbol(_))
    }

    /// `true` if `self` is a bytecode function reference.
    #[inline]
    #[must_use]
    pub const fn is_function(&self) -> bool {
        matches!(self, Value::Function { .. })
    }

    /// `true` if `self` is an ordinary object.
    #[inline]
    #[must_use]
    pub const fn is_object(&self) -> bool {
        matches!(self, Value::Object(_))
    }

    /// `true` if `self` has object identity (any object-like variant).
    /// Matches the spec `Type(value) is Object` classification.
    #[inline]
    #[must_use]
    pub const fn is_object_like(&self) -> bool {
        matches!(
            self,
            Value::Object(_)
                | Value::Array(_)
                | Value::Function { .. }
                | Value::Closure(_)
                | Value::NativeFunction(_)
                | Value::BoundFunction(_)
                | Value::ClassConstructor(_)
                | Value::Promise(_)
                | Value::Iterator(_)
                | Value::RegExp(_)
                | Value::Map(_)
                | Value::Set(_)
                | Value::WeakMap(_)
                | Value::WeakSet(_)
                | Value::WeakRef(_)
                | Value::FinalizationRegistry(_)
                | Value::Temporal(_)
                | Value::Intl(_)
                | Value::ArrayBuffer(_)
                | Value::DataView(_)
                | Value::TypedArray(_)
                | Value::Generator(_)
                | Value::Proxy(_)
        )
    }

    /// `true` if `self` is an array.
    #[inline]
    #[must_use]
    pub const fn is_array(&self) -> bool {
        matches!(self, Value::Array(_))
    }

    /// `true` if `self` is a closure.
    #[inline]
    #[must_use]
    pub const fn is_closure(&self) -> bool {
        matches!(self, Value::Closure(_))
    }

    /// `true` if `self` is a bound function.
    #[inline]
    #[must_use]
    pub const fn is_bound_function(&self) -> bool {
        matches!(self, Value::BoundFunction(_))
    }

    /// `true` if `self` is a native function.
    #[inline]
    #[must_use]
    pub const fn is_native_function(&self) -> bool {
        matches!(self, Value::NativeFunction(_))
    }

    /// `true` if `self` is an iterator handle.
    #[inline]
    #[must_use]
    pub const fn is_iterator(&self) -> bool {
        matches!(self, Value::Iterator(_))
    }

    /// `true` if `self` is a regexp.
    #[inline]
    #[must_use]
    pub const fn is_regexp(&self) -> bool {
        matches!(self, Value::RegExp(_))
    }

    /// `true` if `self` is a promise.
    #[inline]
    #[must_use]
    pub const fn is_promise(&self) -> bool {
        matches!(self, Value::Promise(_))
    }

    /// `true` if `self` is a `Map`.
    #[inline]
    #[must_use]
    pub const fn is_map(&self) -> bool {
        matches!(self, Value::Map(_))
    }

    /// `true` if `self` is a `Set`.
    #[inline]
    #[must_use]
    pub const fn is_set(&self) -> bool {
        matches!(self, Value::Set(_))
    }

    /// `true` if `self` is a `WeakMap`.
    #[inline]
    #[must_use]
    pub const fn is_weak_map(&self) -> bool {
        matches!(self, Value::WeakMap(_))
    }

    /// `true` if `self` is a `WeakSet`.
    #[inline]
    #[must_use]
    pub const fn is_weak_set(&self) -> bool {
        matches!(self, Value::WeakSet(_))
    }

    /// `true` if `self` is a `WeakRef`.
    #[inline]
    #[must_use]
    pub const fn is_weak_ref(&self) -> bool {
        matches!(self, Value::WeakRef(_))
    }

    /// `true` if `self` is a `FinalizationRegistry`.
    #[inline]
    #[must_use]
    pub const fn is_finalization_registry(&self) -> bool {
        matches!(self, Value::FinalizationRegistry(_))
    }

    /// `true` if `self` is a `Temporal.*` value.
    #[inline]
    #[must_use]
    pub const fn is_temporal(&self) -> bool {
        matches!(self, Value::Temporal(_))
    }

    /// `true` if `self` is an `Intl.*` value.
    #[inline]
    #[must_use]
    pub const fn is_intl(&self) -> bool {
        matches!(self, Value::Intl(_))
    }

    /// `true` if `self` is an `ArrayBuffer`.
    #[inline]
    #[must_use]
    pub const fn is_array_buffer(&self) -> bool {
        matches!(self, Value::ArrayBuffer(_))
    }

    /// `true` if `self` is a `DataView`.
    #[inline]
    #[must_use]
    pub const fn is_data_view(&self) -> bool {
        matches!(self, Value::DataView(_))
    }

    /// `true` if `self` is a `TypedArray`.
    #[inline]
    #[must_use]
    pub const fn is_typed_array(&self) -> bool {
        matches!(self, Value::TypedArray(_))
    }

    /// `true` if `self` is a `Generator`.
    #[inline]
    #[must_use]
    pub const fn is_generator(&self) -> bool {
        matches!(self, Value::Generator(_))
    }

    /// `true` if `self` is a `Proxy`.
    #[inline]
    #[must_use]
    pub const fn is_proxy(&self) -> bool {
        matches!(self, Value::Proxy(_))
    }

    /// `true` if `self` is a class constructor.
    #[inline]
    #[must_use]
    pub const fn is_class_constructor(&self) -> bool {
        matches!(self, Value::ClassConstructor(_))
    }

    /// If `self` directly carries a `Gc<…>` handle, return its
    /// compressed offset for write-barrier dispatch.
    #[must_use]
    pub(crate) fn as_gc_raw(&self) -> Option<RawGc> {
        match self {
            Value::Object(o) => Some(o.raw()),
            Value::Array(a) => Some(a.raw()),
            Value::Map(m) => Some(m.raw()),
            Value::Set(s) => Some(s.raw()),
            Value::WeakMap(m) => Some(m.raw()),
            Value::WeakSet(s) => Some(s.raw()),
            Value::WeakRef(w) => Some(w.raw()),
            Value::FinalizationRegistry(r) => Some(r.raw()),
            Value::Promise(p) => Some(p.raw()),
            Value::Iterator(i) => Some(i.raw()),
            Value::Generator(g) => Some(g.raw()),
            Value::BoundFunction(b) => Some(b.raw()),
            Value::NativeFunction(n) => Some(n.raw()),
            Value::RegExp(r) => Some(r.raw()),
            Value::ClassConstructor(c) => Some(c.raw()),
            _ => None,
        }
    }

    /// Walk every `Gc<…>` slot held directly inside `self` and yield
    /// its slot pointer to `visitor`.
    pub(crate) fn trace_value_slots(&self, visitor: &mut SlotVisitor<'_>) {
        match self {
            Value::Closure(c) => c.trace_value_slots(visitor),
            Value::Object(o) => {
                let p = o as *const JsObject as *mut RawGc;
                visitor(p);
            }
            Value::Array(a) => {
                let p = a as *const JsArray as *mut RawGc;
                visitor(p);
            }
            Value::Map(m) => {
                let p = m as *const JsMap as *mut RawGc;
                visitor(p);
            }
            Value::Set(s) => {
                let p = s as *const JsSet as *mut RawGc;
                visitor(p);
            }
            Value::WeakMap(m) => {
                let p = m as *const JsWeakMap as *mut RawGc;
                visitor(p);
            }
            Value::WeakSet(s) => {
                let p = s as *const JsWeakSet as *mut RawGc;
                visitor(p);
            }
            Value::WeakRef(w) => {
                let p = w as *const JsWeakRef as *mut RawGc;
                visitor(p);
            }
            Value::FinalizationRegistry(r) => {
                let p = r as *const JsFinalizationRegistry as *mut RawGc;
                visitor(p);
            }
            Value::Promise(promise) => {
                let p = promise as *const JsPromiseHandle as *mut RawGc;
                visitor(p);
            }
            Value::Iterator(iterator) => {
                let p = iterator as *const IteratorHandle as *mut RawGc;
                visitor(p);
            }
            Value::Generator(generator) => generator.trace_value_slots(visitor),
            Value::BoundFunction(bound) => bound.trace_value_slots(visitor),
            Value::NativeFunction(native) => native.trace_value_slots(visitor),
            Value::RegExp(regexp) => regexp.trace_value_slots(visitor),
            Value::ClassConstructor(class_constructor) => {
                class_constructor.trace_value_slots(visitor);
            }
            Value::Proxy(proxy) => proxy.trace_value_slots(visitor),
            Value::Temporal(temporal) => temporal.trace_value_slots(visitor),
            Value::Symbol(symbol) => symbol.trace_value_slots(visitor),
            Value::BigInt(big) => big.trace_value_slots(visitor),
            Value::ArrayBuffer(buf) => buf.trace_value_slots(visitor),
            Value::DataView(view) => view.trace_value_slots(visitor),
            Value::TypedArray(ta) => ta.trace_value_slots(visitor),
            _ => {}
        }
    }

    /// Convenience: shared empty-string constant. Allocates only on
    /// first call per heap.
    pub fn empty_string(heap: &mut otter_gc::GcHeap) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self::String(JsString::empty(heap)?))
    }

    /// Render the value as a debug-style string suitable for CLI
    /// preview output. The BigInt arm reads the body through `heap`
    /// to render its decimal form; every other primitive
    /// short-circuits without touching the heap.
    #[must_use]
    pub fn display_string(&self, heap: &otter_gc::GcHeap) -> String {
        match self {
            Value::Undefined | Value::Hole => "undefined".to_string(),
            Value::Null => "null".to_string(),
            Value::Boolean(b) => if *b { "true" } else { "false" }.to_string(),
            Value::Number(n) => n.to_display_string(),
            Value::BigInt(b) => b.to_decimal_string(heap),
            Value::String(s) => s.to_lossy_string(heap),
            Value::Symbol(s) => s.descriptive_string(heap),
            Value::Function { function_id }
            | Value::Closure(crate::closure::JsClosure {
                cached_function_id: function_id,
                ..
            }) => {
                format!("[Function #{function_id}]")
            }
            Value::BoundFunction(_) => "[BoundFunction]".to_string(),
            Value::NativeFunction(_) => "[NativeFunction]".to_string(),
            Value::Iterator(_) => "[object Iterator]".to_string(),
            Value::RegExp(_) => "[object RegExp]".to_string(),
            Value::Promise(_) => "[object Promise]".to_string(),
            Value::ClassConstructor(_) => "[class]".to_string(),
            Value::Map(_) => "[object Map]".to_string(),
            Value::Set(_) => "[object Set]".to_string(),
            Value::WeakMap(_) => "[object WeakMap]".to_string(),
            Value::WeakSet(_) => "[object WeakSet]".to_string(),
            Value::WeakRef(_) => "[object WeakRef]".to_string(),
            Value::FinalizationRegistry(_) => "[object FinalizationRegistry]".to_string(),
            Value::Temporal(t) => format!("[object Temporal.{}]", t.kind().class_name()),
            Value::Intl(i) => format!("[object Intl.{}]", i.kind().class_name()),
            Value::ArrayBuffer(b) => {
                if b.is_shared() {
                    "[object SharedArrayBuffer]".to_string()
                } else {
                    "[object ArrayBuffer]".to_string()
                }
            }
            Value::DataView(_) => "[object DataView]".to_string(),
            Value::TypedArray(t) => format!("[object {}]", t.kind().name()),
            Value::Generator(_) => "[object Generator]".to_string(),
            Value::Proxy(_) => "[object Proxy]".to_string(),
            Value::Object(_) => "[object Object]".to_string(),
            Value::Array(_) => "[object Array]".to_string(),
        }
    }

    /// Spec [`ToBoolean`](https://tc39.es/ecma262/#sec-toboolean).
    #[must_use]
    pub fn to_boolean(&self, heap: &otter_gc::GcHeap) -> bool {
        match self {
            Value::Undefined | Value::Null | Value::Hole => false,
            Value::Boolean(b) => *b,
            Value::Number(n) => {
                if n.is_nan() {
                    false
                } else {
                    n.as_f64() != 0.0
                }
            }
            Value::BigInt(b) => !b.is_zero(heap),
            Value::String(s) => !s.is_empty(),
            Value::Symbol(_)
            | Value::Function { .. }
            | Value::Closure(_)
            | Value::BoundFunction(_)
            | Value::NativeFunction(_)
            | Value::Object(_)
            | Value::Array(_)
            | Value::Iterator(_)
            | Value::RegExp(_)
            | Value::Promise(_)
            | Value::ClassConstructor(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::WeakMap(_)
            | Value::WeakSet(_)
            | Value::WeakRef(_)
            | Value::FinalizationRegistry(_)
            | Value::Temporal(_)
            | Value::Intl(_)
            | Value::ArrayBuffer(_)
            | Value::DataView(_)
            | Value::TypedArray(_)
            | Value::Generator(_)
            | Value::Proxy(_) => true,
        }
    }

    /// Spec "is nullish" (`null` or `undefined`).
    #[must_use]
    pub fn is_nullish(&self) -> bool {
        matches!(self, Value::Undefined | Value::Null)
    }

    /// Borrow as a [`JsString`] when the value is a string.
    #[must_use]
    pub fn as_string(&self) -> Option<&JsString> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// Borrow as a [`NumberValue`] when the value is numeric.
    #[must_use]
    pub fn as_number(&self) -> Option<NumberValue> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// Borrow as a `bool` when the value is a boolean.
    #[must_use]
    pub fn as_boolean(&self) -> Option<bool> {
        match self {
            Value::Boolean(b) => Some(*b),
            _ => None,
        }
    }

    /// Borrow as a [`JsSymbol`] when the value is a symbol.
    #[must_use]
    pub fn as_symbol(&self) -> Option<&JsSymbol> {
        match self {
            Value::Symbol(s) => Some(s),
            _ => None,
        }
    }

    /// Borrow as a [`JsTemporal`] when the value is a Temporal
    /// instance.
    #[must_use]
    pub fn as_temporal(&self) -> Option<&JsTemporal> {
        match self {
            Value::Temporal(t) => Some(t),
            _ => None,
        }
    }

    /// `JsObject` handle when this value is an ordinary object.
    #[must_use]
    pub fn as_object(&self) -> Option<JsObject> {
        match self {
            Value::Object(o) => Some(*o),
            _ => None,
        }
    }

    /// `JsArray` handle when this value is an array.
    #[must_use]
    pub fn as_array(&self) -> Option<JsArray> {
        match self {
            Value::Array(a) => Some(*a),
            _ => None,
        }
    }

    /// `BigIntValue` when this value is a BigInt.
    #[must_use]
    pub fn as_big_int(&self) -> Option<bigint::BigIntValue> {
        match self {
            Value::BigInt(b) => Some(*b),
            _ => None,
        }
    }

    /// `JsClosure` when this value is a closure.
    #[must_use]
    pub fn as_closure(&self) -> Option<crate::closure::JsClosure> {
        match self {
            Value::Closure(c) => Some(*c),
            _ => None,
        }
    }

    /// `BoundFunction` when this value is a bound function.
    #[must_use]
    pub fn as_bound_function(&self) -> Option<BoundFunction> {
        match self {
            Value::BoundFunction(b) => Some(*b),
            _ => None,
        }
    }

    /// `NativeFunction` when this value is a host-implemented callable.
    #[must_use]
    pub fn as_native_function(&self) -> Option<NativeFunction> {
        match self {
            Value::NativeFunction(n) => Some(*n),
            _ => None,
        }
    }

    /// `IteratorHandle` when this value is an iterator state.
    #[must_use]
    pub fn as_iterator(&self) -> Option<IteratorHandle> {
        match self {
            Value::Iterator(i) => Some(*i),
            _ => None,
        }
    }

    /// `JsRegExp` when this value is a compiled regexp.
    #[must_use]
    pub fn as_regexp(&self) -> Option<JsRegExp> {
        match self {
            Value::RegExp(r) => Some(*r),
            _ => None,
        }
    }

    /// `JsPromiseHandle` when this value is a promise.
    #[must_use]
    pub fn as_promise(&self) -> Option<JsPromiseHandle> {
        match self {
            Value::Promise(p) => Some(*p),
            _ => None,
        }
    }

    /// `JsMap` when this value is a Map.
    #[must_use]
    pub fn as_map(&self) -> Option<JsMap> {
        match self {
            Value::Map(m) => Some(*m),
            _ => None,
        }
    }

    /// `JsSet` when this value is a Set.
    #[must_use]
    pub fn as_set(&self) -> Option<JsSet> {
        match self {
            Value::Set(s) => Some(*s),
            _ => None,
        }
    }

    /// `JsWeakMap` when this value is a WeakMap.
    #[must_use]
    pub fn as_weak_map(&self) -> Option<JsWeakMap> {
        match self {
            Value::WeakMap(m) => Some(*m),
            _ => None,
        }
    }

    /// `JsWeakSet` when this value is a WeakSet.
    #[must_use]
    pub fn as_weak_set(&self) -> Option<JsWeakSet> {
        match self {
            Value::WeakSet(s) => Some(*s),
            _ => None,
        }
    }

    /// `JsWeakRef` when this value is a WeakRef.
    #[must_use]
    pub fn as_weak_ref(&self) -> Option<JsWeakRef> {
        match self {
            Value::WeakRef(w) => Some(*w),
            _ => None,
        }
    }

    /// `JsFinalizationRegistry` when this value is a FinalizationRegistry.
    #[must_use]
    pub fn as_finalization_registry(&self) -> Option<JsFinalizationRegistry> {
        match self {
            Value::FinalizationRegistry(r) => Some(*r),
            _ => None,
        }
    }

    /// `JsIntl` when this value is an Intl.* instance.
    #[must_use]
    pub fn as_intl(&self) -> Option<JsIntl> {
        match self {
            Value::Intl(i) => Some(*i),
            _ => None,
        }
    }

    /// `JsProxy` when this value is a Proxy.
    #[must_use]
    pub fn as_proxy(&self) -> Option<crate::proxy::JsProxy> {
        match self {
            Value::Proxy(p) => Some(*p),
            _ => None,
        }
    }

    /// `JsDataView` when this value is a DataView.
    #[must_use]
    pub fn as_data_view(&self) -> Option<crate::binary::JsDataView> {
        match self {
            Value::DataView(v) => Some(*v),
            _ => None,
        }
    }

    /// `JsTypedArray` when this value is a TypedArray.
    #[must_use]
    pub fn as_typed_array(&self) -> Option<crate::binary::JsTypedArray> {
        match self {
            Value::TypedArray(t) => Some(*t),
            _ => None,
        }
    }

    /// `JsArrayBuffer` when this value is an ArrayBuffer / SharedArrayBuffer.
    #[must_use]
    pub fn as_array_buffer(&self) -> Option<crate::binary::JsArrayBuffer> {
        match self {
            Value::ArrayBuffer(b) => Some(*b),
            _ => None,
        }
    }

    /// `JsGenerator` when this value is a generator.
    #[must_use]
    pub fn as_generator(&self) -> Option<crate::generator::JsGenerator> {
        match self {
            Value::Generator(g) => Some(*g),
            _ => None,
        }
    }

    /// `ClassConstructor` when this value is a class.
    #[must_use]
    pub fn as_class_constructor(&self) -> Option<ClassConstructor> {
        match self {
            Value::ClassConstructor(c) => Some(*c),
            _ => None,
        }
    }

    /// Bytecode function id when this value is `Value::Function`.
    #[must_use]
    pub fn as_function(&self) -> Option<u32> {
        match self {
            Value::Function { function_id } => Some(*function_id),
            _ => None,
        }
    }

    /// Spec [`typeof`](https://tc39.es/ecma262/#sec-typeof-operator)
    /// — return the JS-visible type tag string.
    #[must_use]
    pub fn typeof_string(&self) -> &'static str {
        match self {
            Value::Undefined | Value::Hole => "undefined",
            Value::Null => "object",
            Value::Boolean(_) => "boolean",
            Value::Number(_) => "number",
            Value::BigInt(_) => "bigint",
            Value::String(_) => "string",
            Value::Symbol(_) => "symbol",
            Value::Function { .. }
            | Value::Closure(_)
            | Value::BoundFunction(_)
            | Value::NativeFunction(_)
            | Value::ClassConstructor(_) => "function",
            Value::Object(_)
            | Value::Array(_)
            | Value::Iterator(_)
            | Value::RegExp(_)
            | Value::Promise(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::WeakMap(_)
            | Value::WeakSet(_)
            | Value::WeakRef(_)
            | Value::FinalizationRegistry(_)
            | Value::Temporal(_)
            | Value::Intl(_)
            | Value::ArrayBuffer(_)
            | Value::DataView(_)
            | Value::TypedArray(_)
            | Value::Generator(_)
            | Value::Proxy(_) => "object",
        }
    }

    /// `typeof` when the VM heap is available. Ordinary objects can
    /// carry a hidden native `[[Call]]` slot, so their visible tag is
    /// `"function"` even though the public value variant is
    /// `Value::Object`.
    #[must_use]
    pub fn typeof_string_with_heap(&self, heap: &otter_gc::GcHeap) -> &'static str {
        match self {
            Value::Object(obj)
                if matches!(
                    crate::object::call_native(*obj, heap),
                    Some(Value::NativeFunction(_))
                ) =>
            {
                "function"
            }
            _ => self.typeof_string(),
        }
    }

    /// Construct a string value from in-memory text. Convenience for
    /// tests and the compiler's literal table.
    ///
    /// # Errors
    /// See [`JsString::from_str`].
    pub fn from_str(s: &str, heap: &mut otter_gc::GcHeap) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self::String(JsString::from_str(s, heap)?))
    }
}

impl otter_gc::GcStore for Value {
    fn visit_gc_edges(&self, visitor: &mut dyn FnMut(otter_gc::GcEdge)) {
        if let Value::Closure(c) = self
            && let Some(edge) = otter_gc::GcEdge::from_gc(c.handle)
        {
            visitor(edge);
        }
        if let Some(raw) = self.as_gc_raw()
            && let Some(edge) = otter_gc::GcEdge::from_raw(raw)
        {
            visitor(edge);
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Undefined, Value::Undefined) => true,
            (Value::Hole, Value::Hole) => true,
            (Value::Null, Value::Null) => true,
            (Value::Boolean(a), Value::Boolean(b)) => a == b,
            (Value::Number(a), Value::Number(b)) => number::equals(*a, *b),
            (Value::BigInt(a), Value::BigInt(b)) => a.ptr_eq(*b),
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Symbol(a), Value::Symbol(b)) => a.ptr_eq(b),
            (Value::Object(a), Value::Object(b)) => a == b,
            (Value::Array(a), Value::Array(b)) => crate::array::ptr_eq(*a, *b),
            (Value::Function { function_id: a }, Value::Function { function_id: b }) => a == b,
            (Value::Closure(a), Value::Closure(b)) => a.ptr_eq(*b),
            (Value::BoundFunction(a), Value::BoundFunction(b)) => a.ptr_eq(b),
            (Value::NativeFunction(a), Value::NativeFunction(b)) => a.ptr_eq(b),
            (Value::Promise(a), Value::Promise(b)) => a.ptr_eq(b as &dyn JsPromise),
            (Value::Iterator(a), Value::Iterator(b)) => a == b,
            (Value::RegExp(a), Value::RegExp(b)) => a.ptr_eq(b),
            (Value::ClassConstructor(a), Value::ClassConstructor(b)) => a.ptr_eq(*b),
            (Value::Map(a), Value::Map(b)) => crate::collections::map_ptr_eq(*a, *b),
            (Value::Set(a), Value::Set(b)) => crate::collections::set_ptr_eq(*a, *b),
            (Value::WeakMap(a), Value::WeakMap(b)) => a == b,
            (Value::WeakSet(a), Value::WeakSet(b)) => a == b,
            (Value::WeakRef(a), Value::WeakRef(b)) => a == b,
            (Value::FinalizationRegistry(a), Value::FinalizationRegistry(b)) => a == b,
            (Value::Temporal(a), Value::Temporal(b)) => a.ptr_eq(*b),
            (Value::Intl(a), Value::Intl(b)) => a.ptr_eq(*b),
            (Value::ArrayBuffer(a), Value::ArrayBuffer(b)) => a.ptr_eq(*b),
            (Value::DataView(a), Value::DataView(b)) => a.ptr_eq(*b),
            (Value::TypedArray(a), Value::TypedArray(b)) => a.ptr_eq(*b),
            (Value::Generator(a), Value::Generator(b)) => a.ptr_eq(b),
            (Value::Proxy(a), Value::Proxy(b)) => a.ptr_eq(*b),
            _ => false,
        }
    }
}

impl Eq for Value {}
