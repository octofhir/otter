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
