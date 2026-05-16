//! Interpreter and value model for the new Otter engine.
//!
//! Foundation phase is **interpreter-only** (foundation plan §15).
//! No JIT, no GC integration yet — values for the harness slice are
//! plain `Value::Undefined`. Slice tasks `09`+ extend the value
//! model.
//!
//! # Contents
//! - [`Value`] — opaque runtime value (foundation: only `Undefined`).
//! - [`Frame`] — compact call frame.
//! - [`Interpreter`] — match-based dispatch loop over the frozen
//!   executable view inside [`ExecutionContext`].
//! - [`InterruptFlag`] — atomic flag observed at back-edges; cheap.
//! - [`VmError`] — the small enum of runtime errors the interpreter
//!   can raise.
//!
//! # Invariants
//! - One thread, one [`Interpreter`]. `Send`/`Sync` are not
//!   implemented.
//! - The dispatch loop polls [`InterruptFlag`] before every
//!   instruction in the harness slice (back-edges arrive in slice
//!   `12`).
//!
//! # See also
//! - [Runtime architecture](../../../docs/book/src/engine/architecture.md)
//! - [Frontend and bytecode dumps](../../../docs/book/src/engine/frontend.md)

pub mod abstract_ops;
mod allocation_ops;
mod argument_window;
pub mod arguments_object;
mod arithmetic_dispatch;
pub mod array;
mod array_ops;
pub mod array_prototype;
pub mod array_statics;
mod async_ops;
pub mod atomics;
pub mod atomics_wait;
pub mod bigint;
pub mod binary;
pub mod boolean_prototype;
mod call_ops;
mod collection_ops;
pub mod collections;
pub mod collections_prototype;
pub mod console;
mod constant_ops;
mod conversion;
pub mod date;
// `date` is a directory module — see `date/mod.rs`.
pub mod bootstrap;
pub mod bootstrap_array_buffer;
pub mod bootstrap_bigint;
pub mod bootstrap_collections;
pub mod bootstrap_data_view;
pub mod bootstrap_promise;
pub mod bootstrap_regexp;
pub mod bootstrap_typed_array;
pub mod bootstrap_weak_refs;
pub mod dynamic_import;
pub mod error_classes;
mod error_ops;
mod eval_ops;
mod executable;
pub mod execution_context;
mod frame_ops;
mod frame_state;
pub mod function_metadata;
mod function_ops;
pub mod function_prototype;
pub mod gc_trace;
pub mod generator;
pub mod global_functions;
mod global_ops;
pub mod intl;
mod intl_ops;
pub mod intrinsics;
mod iterator_ops;
pub mod js_surface;
pub mod json;
pub mod math;
mod method_ops;
pub mod microtask;
mod module_ops;
pub mod native_function;
pub mod number;
pub mod object;
mod object_internal_ops;
pub mod object_statics;
mod operand_decode;
pub mod promise;
pub mod promise_dispatch;
mod promise_ops;
mod property_atom;
mod property_dispatch;
mod property_ic;
pub mod proxy;
pub mod reflect;
mod reflect_ops;
pub mod regexp;
pub mod regexp_prototype;
pub mod run_control;
pub mod runtime_budget;
pub mod runtime_cx;
pub mod runtime_state;
mod static_call_ops;
mod static_load_ops;
pub mod string;
pub mod string_dispatch;
mod string_ops;
pub mod string_prototype;
pub mod swar;
pub mod symbol;
pub mod symbol_dispatch;
pub mod symbol_prototype;
pub mod temporal;
pub mod timers;
pub mod weak_refs;

#[cfg(test)]
mod gc_invariants;
#[cfg(test)]
mod test_support;

pub use execution_context::ExecutionContext;
pub use frame_state::{
    AsyncFrameState, Frame, PendingBindFunction, PendingBindStage, PendingGetIterator,
    PendingIteratorNext, PendingToPrimitive, ToPrimitiveStage, TryHandler,
};
pub use property_ic::PropertyIcStats;
pub use run_control::{
    DEFAULT_MAX_STACK_DEPTH, InterruptFlag, NO_HANDLER_OFFSET, RunError, StackFrameSnapshot,
    VmError,
};

use std::sync::Arc;

use otter_bytecode::{ArgumentBindingStorage, ArgumentsObjectKind, BytecodeModule, Op};
use smallvec::SmallVec;

use crate::intrinsics::{IntrinsicArgs, IntrinsicError};
use arithmetic_dispatch::{
    bigint_and_op, bigint_mul_op, bigint_or_op, bigint_sub_op, bigint_xor_op,
};
pub(crate) use error_ops::{
    intrinsic_to_vm_error, json_to_vm_error, math_to_vm_error, native_to_vm_error,
    render_thrown_value, snapshot_frames, symbol_to_vm_error, temporal_to_vm_error,
    vm_err_to_value,
};
use executable::ExecutableFunction;
use operand_decode::{apply_branch, register_operand};

pub use array::JsArray;
pub use collections::{CollectionError, JsMap, JsSet, JsWeakMap, JsWeakSet, MapKey};
pub use console::{ConsoleLevel, ConsoleSink, ConsoleSinkHandle, StdConsoleSink};
pub use dynamic_import::{DynamicImportLoader, DynamicImportLoaderHandle, DynamicImportRegistry};
pub use error_classes::{ErrorClassRegistry, ErrorKind};
pub use intl::{IntlKind, IntlPayload, JsIntl};
pub use js_surface::{
    AccessorSpec, Attr, ClassBuilder, ClassSpec, ConstSpec, ConstValue, ConstructorBuilder,
    ConstructorSpec, JsSurfaceError, MethodSpec, NamespaceBuilder, NamespaceSpec, ObjectBuilder,
    PropertySpec,
};
pub use microtask::{Microtask, MicrotaskError, MicrotaskKind, MicrotaskQueue};
pub use native_function::{
    NativeCall, NativeError, NativeFastFn, NativeFn, NativeFunction, VmIntrinsicFunction,
    native_value, native_value_static, native_value_with_captures,
};
pub use number::{NumberValue, NumericOrdering};
pub use object::JsObject;
pub use promise::{
    JsPromise, JsPromiseHandle, PromiseCapability, PromiseReaction, PromiseSettleJobs,
    PromiseState, PromiseThenOutcome, PurePromise, ReactionKind,
};
pub use regexp::{JsRegExp, RegExpError, RegExpFlags};
pub use string::{JsString, MAX_ROPE_DEPTH, StringError, StringHeap, StringRepr};
pub use symbol::{JsSymbol, SymbolBody, SymbolRegistry, WellKnown, WellKnownSymbols};
pub use temporal::{JsTemporal, TemporalKind, TemporalPayload};
pub use timers::{TimerCallbacks, TimerEntry, TimerScheduler, TimerSchedulerHandle};
pub use weak_refs::{JsFinalizationRegistry, JsWeakRef};

pub use runtime_budget::{RuntimeBudget, RuntimeBudgetExceededAction, RuntimeBudgetStats};
pub use runtime_cx::{NativeCallInfo, NativeCtx};

use runtime_budget::RuntimeHeapSnapshot;

use otter_gc::raw::{RawGc, SlotVisitor};

// ---------------------------------------------------------------------------
// `!Send + !Sync` static assertions for the new-engine VM.
//
// The VM and GC stay explicit-context and single-mutator: the
// interpreter, every GC handle, and every borrowed-context type must
// be `!Send + !Sync` so compile-fail tests reject any future edit
// that accidentally moves a VM handle into `tokio::spawn` or holds a
// `&mut RuntimeCx` across `.await`.
//
// Spec:
// - <https://tc39.es/ecma262/#sec-agents>
// ---------------------------------------------------------------------------
static_assertions::assert_not_impl_any!(Interpreter: Send, Sync);
static_assertions::assert_not_impl_any!(crate::runtime_cx::NativeCtx<'static>: Send, Sync);
// `RuntimeCx<'_>` is `pub(crate)` so we cannot name it directly in
// a `pub`-visible macro. The bound is enforced transitively because
// `RuntimeCx<'rt>` holds `&'rt mut Interpreter`, and `Interpreter`
// is `!Send + !Sync` per the assertion above.

/// Foundation runtime value.
///
/// Slice 09 introduced `String`; slice 11 adds `Number` and
/// `Boolean`. Later slices add `Null`, `Object`, etc. The foundation
/// `Value` is intentionally **not** `Copy` — `JsString` owns an
/// `Arc` payload.
#[derive(Debug, Clone)]
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
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-array-exotic-objects>
    /// - <https://tc39.es/ecma262/#sec-array.prototype.foreach>
    Hole,
    /// JS `null`.
    Null,
    /// JS `true` / `false`.
    Boolean(bool),
    /// JS Number (smi + double; see [`NumberValue`]).
    Number(NumberValue),
    /// JS BigInt — arbitrary-precision integer. Distinct from
    /// `Number`; mixing the two through arithmetic is a spec
    /// `TypeError`. See [`bigint::BigIntValue`].
    BigInt(bigint::BigIntValue),
    /// JS string. Storage is WTF-16 with cons / sliced ropes; see
    /// [`JsString`].
    String(JsString),
    /// JS Symbol primitive. Identity-shared via `Rc<SymbolBody>`;
    /// each ordinary `Symbol(desc)` allocation produces a distinct
    /// value even when descriptions match. See [`JsSymbol`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-symbol-objects>
    Symbol(JsSymbol),
    /// JS function. Foundation slice 13: a closure-less reference
    /// to a [`otter_bytecode::Function`] in the loaded module.
    /// Real closures (captured upvalues) arrive in a later slice.
    Function {
        /// Index into [`otter_bytecode::BytecodeModule::functions`].
        function_id: u32,
    },
    /// JS object — heap-shared, mutable. See [`JsObject`].
    Object(JsObject),
    /// JS array — dense, heap-shared. See [`JsArray`].
    Array(JsArray),
    /// Closure — function with captured upvalues. See
    /// [`UpvalueCell`].
    Closure {
        /// Index into [`otter_bytecode::BytecodeModule::functions`].
        function_id: u32,
        /// Captured cells, in declaration order. The compiler emits
        /// `MakeFunction` for closure-less, non-arrow functions and
        /// reserves `MakeClosure` for the capture path and for all
        /// arrow expressions.
        upvalues: std::rc::Rc<[UpvalueCell]>,
        /// `Some(this)` for arrow closures: the lexically-captured
        /// receiver always wins over whatever the call site passes.
        /// `None` for non-arrow closures, which take their `this`
        /// from the call site.
        bound_this: Option<Box<Value>>,
    },
    /// Result of `Function.prototype.bind(thisArg, ...prefix)`. When
    /// invoked, forwards to `target` with `this = bound_this` and
    /// `prefix ++ call_args` as the argument list. Cheap to clone:
    /// the wrapper is a GC handle.
    BoundFunction(BoundFunction),
    /// Host-implemented callable. Used by `Promise` resolve/reject
    /// closures, the `Promise.all` aggregator-functions, and any
    /// other native shape that needs to be JS-callable without
    /// going through bytecode. See [`crate::NativeFunction`].
    NativeFunction(NativeFunction),
    /// Internal iterator state, produced by [`otter_bytecode::Op::GetIterator`]
    /// and driven by [`otter_bytecode::Op::IteratorNext`]. Until
    /// task 37 adds real `Symbol.iterator` lookup, the foundation
    /// models iterators out-of-band as a dedicated value variant
    /// — they are not addressable via `o[@@iterator]` from user
    /// code.
    Iterator(IteratorHandle),
    /// Compiled regular-expression value, produced by
    /// [`otter_bytecode::Op::LoadRegExp`] reading a pooled
    /// [`otter_bytecode::Constant::RegExp`]. Identity is by handle:
    /// `===` follows `Rc::ptr_eq` semantics.
    RegExp(JsRegExp),
    /// JS Promise. Concrete handle (tagged enum inside) so
    /// foundation `PurePromise` and future host-bridged promise
    /// types share one `Value` variant **without** vtable
    /// indirection on the hot path. Implements [`JsPromise`] for
    /// the method contract. Identity (`===`) goes through
    /// [`JsPromise::ptr_eq`]. Long-term path: GC migration (task
    /// 57) replaces the inner `Rc` with a `Gc<>` handle.
    Promise(JsPromiseHandle),
    /// JS `Map` — ordered associative store. See [`JsMap`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-map-objects>
    Map(JsMap),
    /// JS `Set` — ordered unique-element store. See [`JsSet`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-set-objects>
    Set(JsSet),
    /// JS `WeakMap` — object-keyed ephemeron map.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-weakmap-objects>
    WeakMap(JsWeakMap),
    /// JS `WeakSet` — object-keyed weak set.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-weakset-objects>
    WeakSet(JsWeakSet),
    /// JS `WeakRef` — weak target reference.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-weak-ref-objects>
    WeakRef(JsWeakRef),
    /// JS `FinalizationRegistry` — post-GC cleanup registry.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-finalization-registry-objects>
    FinalizationRegistry(JsFinalizationRegistry),
    /// `Temporal.*` value — `Instant` / `Duration` / `PlainDate` /
    /// `PlainTime` / `PlainDateTime`. Backed by `temporal_rs`.
    ///
    /// # See also
    /// - <https://tc39.es/proposal-temporal/>
    Temporal(JsTemporal),
    /// JS `Date` — mutable epoch-millisecond timestamp per
    /// ECMA-262 §21.4. See [`crate::date::JsDate`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-date-objects>
    Date(crate::date::JsDate),
    /// `Intl.*` value — `Collator` / `NumberFormat` /
    /// `DateTimeFormat`. Backed by ICU 4X. See [`JsIntl`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma402/>
    Intl(JsIntl),
    /// JS `ArrayBuffer` — heap-shared raw byte storage per
    /// ECMA-262 §25.1. See [`crate::binary::JsArrayBuffer`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-arraybuffer-objects>
    ArrayBuffer(crate::binary::JsArrayBuffer),
    /// JS `DataView` — typed view over an `ArrayBuffer` with
    /// explicit byte-order control per ECMA-262 §25.3. See
    /// [`crate::binary::JsDataView`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-dataview-objects>
    DataView(crate::binary::JsDataView),
    /// JS `TypedArray` — element-typed view over an `ArrayBuffer`
    /// per ECMA-262 §23.2. The view's
    /// [`crate::binary::TypedArrayKind`] selects the element-type
    /// behaviour shared across all eleven concrete classes.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-typedarray-objects>
    TypedArray(crate::binary::JsTypedArray),
    /// Generator object produced by calling a `function*` body. The
    /// handle owns the suspended frame state; `.next(arg)` /
    /// `.return(arg)` / `.throw(reason)` resume it.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-generator-objects>
    Generator(crate::generator::JsGenerator),
    /// JS Proxy — handler-trapped object surface per ECMA-262 §28.2.
    /// Property loads / stores / has-tests / call-as-function go
    /// through the handler's traps when present; otherwise fall
    /// through to the target.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-proxy-objects>
    Proxy(crate::proxy::JsProxy),
    /// Class value: the result of evaluating a `class` declaration
    /// or expression. Wraps the underlying constructor callable,
    /// the prototype object that fresh instances inherit from, and
    /// a static-side object that holds class statics (and chains
    /// through `extends`). The dispatcher unwraps a class to its
    /// inner constructor for `Op::Call` / `Op::New`, but treats
    /// `LoadProperty` / `StoreProperty` against the class as
    /// operations on the static side (with `"prototype"` aliased
    /// to the prototype object directly).
    ClassConstructor(ClassConstructor),
}

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for
/// [`ClassConstructorBody`].
pub const CLASS_CONSTRUCTOR_BODY_TYPE_TAG: u8 = 0x1f;

/// GC-allocated payload backing every [`Value::ClassConstructor`].
/// Holds the callable, the instance prototype, and the static-side
/// object the class exposes.
#[derive(Debug)]
pub struct ClassConstructorBody {
    /// The actual callable (`Value::Function` / `Value::Closure` /
    /// `Value::NativeFunction`) the runtime invokes for `new C(...)`
    /// or `super(...)`. Constructed by the compiler's class-lowering
    /// pass.
    pub ctor: Value,
    /// `C.prototype` — every instance built by `new C(...)`
    /// inherits from this object, and instance methods live here.
    pub prototype: JsObject,
    /// Static side: own static methods/properties live here, and
    /// when `class D extends C` the static object's
    /// `[[Prototype]]` chains to `C`'s static object so static
    /// inheritance just falls out of the existing prototype walker.
    pub statics: JsObject,
}

pub(crate) enum VmPropertyKey<'a> {
    Atom(property_atom::AtomizedPropertyKey<'a>),
    String(&'a str),
    OwnedString(String),
    Symbol(symbol::JsSymbol),
}

impl<'a> VmPropertyKey<'a> {
    #[must_use]
    pub(crate) const fn atom(key: property_atom::AtomizedPropertyKey<'a>) -> Self {
        Self::Atom(key)
    }

    #[must_use]
    pub(crate) fn string_name(&self) -> Option<&str> {
        match self {
            Self::Atom(key) => Some(key.name()),
            Self::String(key) => Some(key),
            Self::OwnedString(key) => Some(key.as_str()),
            Self::Symbol(_) => None,
        }
    }
}

pub(crate) enum VmGetOutcome {
    Value(Value),
    InvokeGetter { getter: Value },
}

impl otter_gc::SafeTraceable for ClassConstructorBody {
    const TYPE_TAG: u8 = CLASS_CONSTRUCTOR_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        self.ctor.trace_value_slots(visitor);
        // `JsObject` is `#[repr(transparent)]` over a `u32` GC
        // offset; expose its storage to the scavenger so a moving
        // collector can rewrite the slot.
        if !self.prototype.is_null() {
            let p = &self.prototype as *const JsObject as *mut RawGc;
            visitor(p);
        }
        if !self.statics.is_null() {
            let p = &self.statics as *const JsObject as *mut RawGc;
            visitor(p);
        }
    }
}

/// Cheap-to-clone class-constructor handle. Wraps a
/// `Gc<ClassConstructorBody>` so `Value::ClassConstructor` stays a
/// 4-byte payload and the underlying body is GC-managed (no
/// `Rc`-shared mutable state).
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct ClassConstructor {
    inner: otter_gc::Gc<ClassConstructorBody>,
}

impl ClassConstructor {
    /// Allocate a class constructor while exposing caller-owned roots
    /// across the body allocation.
    pub(crate) fn new_with_roots(
        heap: &mut otter_gc::GcHeap,
        ctor: Value,
        prototype: JsObject,
        statics: JsObject,
        external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let prototype_root = Value::Object(prototype);
        let statics_root = Value::Object(statics);
        let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            ctor.trace_value_slots(visitor);
            prototype_root.trace_value_slots(visitor);
            statics_root.trace_value_slots(visitor);
        };
        Ok(Self {
            inner: heap.alloc_with_roots(
                ClassConstructorBody {
                    ctor: ctor.clone(),
                    prototype,
                    statics,
                },
                &mut visit,
            )?,
        })
    }

    /// Identity comparison — `===` follows the GC handle's
    /// 32-bit-offset equality.
    #[inline]
    #[must_use]
    pub fn ptr_eq(self, other: Self) -> bool {
        self.inner == other.inner
    }

    /// Read the underlying callable (Function / Closure / native).
    #[inline]
    #[must_use]
    pub fn ctor(self, heap: &otter_gc::GcHeap) -> Value {
        heap.read_payload(self.inner, |body| body.ctor.clone())
    }

    /// Read `C.prototype`.
    #[inline]
    #[must_use]
    pub fn prototype(self, heap: &otter_gc::GcHeap) -> JsObject {
        heap.read_payload(self.inner, |body| body.prototype)
    }

    /// Read the static-side object.
    #[inline]
    #[must_use]
    pub fn statics(self, heap: &otter_gc::GcHeap) -> JsObject {
        heap.read_payload(self.inner, |body| body.statics)
    }

    /// GC root — used by VM tracing roots when a class constructor
    /// sits in a register or environment slot.
    #[doc(hidden)]
    #[inline]
    pub fn raw(self) -> RawGc {
        self.inner.raw()
    }
}

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`IteratorState`].
pub const ITERATOR_STATE_TYPE_TAG: u8 = 0x1c;

/// Heap-shared iterator state handle.
pub type IteratorHandle = otter_gc::Gc<IteratorState>;

/// Foundation iterator-state machine. Each variant carries the
/// minimum information needed to advance one step at a time. Once
/// the iterator reports `done`, subsequent calls keep returning
/// `done = true` with `value = undefined` (per spec §7.4.2 step 6).
#[derive(Debug)]
pub enum IteratorState {
    /// Walks `array`'s dense storage in insertion order.
    Array {
        /// Backing array — held by `JsArray`'s GC handle so
        /// mutation through the original handle is observable.
        array: JsArray,
        /// Next element index to read. Compared against the
        /// array's `len()` at every step so resizing the array
        /// during iteration is observed correctly.
        index: usize,
    },
    /// Walks `string`'s WTF-16 code units, yielding one-unit
    /// strings. Surrogate pairs split (matches `String[@@iterator]`
    /// only loosely; full code-point iteration arrives with task
    /// 30's string completion).
    String {
        /// Backing string.
        string: JsString,
        /// Next code-unit index.
        index: u32,
    },
    /// User-defined iterable: the result of calling
    /// `obj[@@iterator]()`. The contained `Value` is the iterator
    /// object; the dispatcher invokes its `next()` method on every
    /// `Op::IteratorNext`, unpacks `{ value, done }` from the
    /// returned record, and transitions to [`Self::Exhausted`]
    /// when `done` becomes truthy. Per ECMA-262 §7.4.2 step 6 a
    /// `done` iterator stays `done` forever.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-getiterator>
    /// - <https://tc39.es/ecma262/#sec-iteratornext>
    User {
        /// Iterator object returned by `obj[@@iterator]()`.
        iterator: Value,
    },
    /// Permanently exhausted iterator — every step returns
    /// `done = true`. The runtime transitions any iterator to this
    /// state once it observes `done`, so re-driving an exhausted
    /// iterator is a no-op rather than a re-iteration.
    Exhausted,
    /// Lazy `Iterator.prototype.map(fn)` wrapper per the
    /// [iterator-helpers proposal](https://tc39.es/proposal-iterator-helpers/#sec-iteratorprototype.map).
    /// Pulls from `source` and applies `mapper` on every step.
    Map {
        /// Underlying iterator handle.
        source: IteratorHandle,
        /// Per-element mapper. Must be callable.
        mapper: Value,
    },
    /// Lazy `Iterator.prototype.filter(predicate)` wrapper.
    /// Skips elements for which `predicate` returns falsey.
    Filter {
        /// Underlying iterator handle.
        source: IteratorHandle,
        /// Per-element predicate. Must be callable.
        predicate: Value,
    },
    /// Lazy `Iterator.prototype.take(n)` wrapper. Yields at most
    /// `remaining` more elements from `source`.
    Take {
        /// Underlying iterator handle.
        source: IteratorHandle,
        /// Steps still allowed before the wrapper reports `done`.
        remaining: u64,
    },
    /// Lazy `Iterator.prototype.drop(n)` wrapper. Discards the
    /// first `to_drop` elements of `source` then forwards the rest.
    Drop {
        /// Underlying iterator handle.
        source: IteratorHandle,
        /// Elements still to discard before forwarding kicks in.
        to_drop: u64,
    },
    /// `Value::Generator` driven through the iterator protocol.
    /// Each step calls `gen.next()` via the runtime's
    /// [`Interpreter::resume_generator`] helper; once `done` is
    /// observed, transitions to [`Self::Exhausted`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-generator-prototype-object>
    Generator {
        /// Underlying generator handle.
        handle: crate::generator::JsGenerator,
    },
    /// Lazy `Iterator.prototype.flatMap(mapper)` wrapper. Applies
    /// `mapper` to each source element; the returned value is
    /// flattened: arrays and iterators contribute their elements,
    /// other values flow through directly.
    FlatMap {
        /// Underlying iterator handle.
        source: IteratorHandle,
        /// Per-element mapper. Must be callable.
        mapper: Value,
        /// Inner iterator currently being drained, when the last
        /// `mapper` call produced an iterable.
        inner: Option<IteratorHandle>,
    },
}

impl otter_gc::SafeTraceable for IteratorState {
    const TYPE_TAG: u8 = ITERATOR_STATE_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        match self {
            IteratorState::Array { array, .. } => {
                let p = array as *const JsArray as *mut RawGc;
                visitor(p);
            }
            IteratorState::String { .. } | IteratorState::Exhausted => {}
            IteratorState::User { iterator } => iterator.trace_value_slots(visitor),
            IteratorState::Map { source, mapper } => {
                let p = source as *const IteratorHandle as *mut RawGc;
                visitor(p);
                mapper.trace_value_slots(visitor);
            }
            IteratorState::Filter { source, predicate } => {
                let p = source as *const IteratorHandle as *mut RawGc;
                visitor(p);
                predicate.trace_value_slots(visitor);
            }
            IteratorState::Take { source, .. } | IteratorState::Drop { source, .. } => {
                let p = source as *const IteratorHandle as *mut RawGc;
                visitor(p);
            }
            IteratorState::Generator { handle } => handle.trace_value_slots(visitor),
            IteratorState::FlatMap {
                source,
                mapper,
                inner,
            } => {
                let p = source as *const IteratorHandle as *mut RawGc;
                visitor(p);
                mapper.trace_value_slots(visitor);
                if let Some(inner) = inner {
                    let p = inner as *const IteratorHandle as *mut RawGc;
                    visitor(p);
                }
            }
        }
    }
}

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for
/// [`BoundFunctionBody`].
pub const BOUND_FUNCTION_BODY_TYPE_TAG: u8 = 0x1c;

/// Own metadata-property state for bound function objects.
#[derive(Debug, Clone)]
pub(crate) enum BoundFunctionMetadataProperty {
    /// The spec-created `name` / `length` property is still present.
    Builtin,
    /// The configurable own property was deleted.
    Deleted,
    /// The property was redefined through `Object.defineProperty`.
    Overridden(object::PropertyDescriptor),
}

/// GC-allocated storage for `Value::BoundFunction`. Constructed by
/// the `Op::BindFunction` opcode and consumed by every call dispatch
/// path (`Op::Call`, `Op::CallWithThis`, `Op::CallMethodValue`).
#[derive(Debug, Clone)]
pub struct BoundFunctionBody {
    /// Underlying callable. Foundation slice keeps this as a
    /// `Value`; chained `bind` flattens by re-wrapping at call
    /// time without unbounded recursion (one hop per layer).
    pub target: Value,
    /// The `this` value the bound call receives. Overrides any
    /// receiver the caller supplies.
    pub bound_this: Value,
    /// Arguments prepended to the caller's argument list at every
    /// invocation. Stored inline up to four entries to keep the
    /// usual `f.bind(t, a, b)` shape off the heap.
    bound_args: SmallVec<[Value; 4]>,
    /// Bound function builtin `name`, computed once by `bind`.
    builtin_name: String,
    /// Bound function builtin `length`, computed once by `bind`.
    builtin_length: NumberValue,
    /// Own `name` metadata property state.
    name_property: BoundFunctionMetadataProperty,
    /// Own `length` metadata property state.
    length_property: BoundFunctionMetadataProperty,
    /// Ordinary own properties added after bind creation.
    own_properties: JsObject,
}

impl otter_gc::SafeTraceable for BoundFunctionBody {
    const TYPE_TAG: u8 = BOUND_FUNCTION_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        self.target.trace_value_slots(visitor);
        self.bound_this.trace_value_slots(visitor);
        for arg in &self.bound_args {
            arg.trace_value_slots(visitor);
        }
        trace_bound_metadata_property(&self.name_property, visitor);
        trace_bound_metadata_property(&self.length_property, visitor);
        let p = &self.own_properties as *const JsObject as *mut RawGc;
        visitor(p);
    }
}

fn trace_bound_metadata_property(
    property: &BoundFunctionMetadataProperty,
    visitor: &mut SlotVisitor<'_>,
) {
    let BoundFunctionMetadataProperty::Overridden(desc) = property else {
        return;
    };
    match &desc.kind {
        object::DescriptorKind::Data { value } => value.trace_value_slots(visitor),
        object::DescriptorKind::Accessor { getter, setter } => {
            if let Some(getter) = getter {
                getter.trace_value_slots(visitor);
            }
            if let Some(setter) = setter {
                setter.trace_value_slots(visitor);
            }
        }
    }
}

fn no_extra_roots(_: &mut dyn FnMut(*mut RawGc)) {}

/// Cheap-to-clone handle for [`BoundFunctionBody`].
#[repr(transparent)]
#[derive(Debug, Clone, Copy)]
pub struct BoundFunction {
    inner: otter_gc::Gc<BoundFunctionBody>,
}

impl BoundFunction {
    /// Allocate a bound-function body on the GC heap.
    pub fn new(
        heap: &mut otter_gc::GcHeap,
        target: Value,
        bound_this: Value,
        bound_args: SmallVec<[Value; 4]>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Self::new_with_metadata(
            heap,
            target,
            bound_this,
            bound_args,
            function_metadata::BoundFunctionCreateMetadata {
                name: "bound ".to_string(),
                length: NumberValue::from_i32(0),
            },
        )
    }

    /// Build a bound function with spec-computed `name` / `length`
    /// metadata captured at bind time.
    pub(crate) fn new_with_metadata(
        heap: &mut otter_gc::GcHeap,
        target: Value,
        bound_this: Value,
        bound_args: SmallVec<[Value; 4]>,
        metadata: function_metadata::BoundFunctionCreateMetadata,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let mut external_visit = no_extra_roots;
        Self::new_with_metadata_and_roots(
            heap,
            target,
            bound_this,
            bound_args,
            metadata,
            &mut external_visit,
        )
    }

    /// Build a bound function while exposing caller-owned roots
    /// across the function's ordinary property bag and body
    /// allocations.
    pub(crate) fn new_with_metadata_and_roots(
        heap: &mut otter_gc::GcHeap,
        target: Value,
        bound_this: Value,
        bound_args: SmallVec<[Value; 4]>,
        metadata: function_metadata::BoundFunctionCreateMetadata,
        external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let own_properties = {
            let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                external_visit(visitor);
                target.trace_value_slots(visitor);
                bound_this.trace_value_slots(visitor);
                for arg in &bound_args {
                    arg.trace_value_slots(visitor);
                }
            };
            object::alloc_object_with_roots(heap, &mut visit)?
        };
        let own_properties_root = Value::Object(own_properties);
        let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            own_properties_root.trace_value_slots(visitor);
            target.trace_value_slots(visitor);
            bound_this.trace_value_slots(visitor);
            for arg in &bound_args {
                arg.trace_value_slots(visitor);
            }
        };
        Ok(Self {
            inner: heap.alloc_with_roots(
                BoundFunctionBody {
                    target: target.clone(),
                    bound_this: bound_this.clone(),
                    bound_args: bound_args.clone(),
                    builtin_name: metadata.name,
                    builtin_length: metadata.length,
                    name_property: BoundFunctionMetadataProperty::Builtin,
                    length_property: BoundFunctionMetadataProperty::Builtin,
                    own_properties,
                },
                &mut visit,
            )?,
        })
    }

    /// Raw handle used by root tracing and write barriers.
    #[must_use]
    pub(crate) fn raw(&self) -> RawGc {
        self.inner.raw()
    }

    /// Stable identity token.
    #[must_use]
    pub fn identity_addr(&self) -> *const () {
        self.inner.as_header_ptr() as *const ()
    }

    /// Identity comparison.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }

    /// Clone the callable parts so dispatch can release the heap
    /// borrow before continuing with mutable interpreter work.
    #[must_use]
    pub fn parts(&self, heap: &otter_gc::GcHeap) -> (Value, Value, SmallVec<[Value; 4]>) {
        heap.read_payload(self.inner, |body| {
            (
                body.target.clone(),
                body.bound_this.clone(),
                body.bound_args.clone(),
            )
        })
    }

    /// Trace this handle as a root slot.
    pub(crate) fn trace_value_slots(&self, visitor: &mut SlotVisitor<'_>) {
        let p = self as *const BoundFunction as *mut RawGc;
        visitor(p);
    }
}

/// One captured-variable cell. Cloning shares the same heap slot
/// so multiple closures + the original outer scope all see
/// mutations through it.
///
/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for
/// [`UpvalueCellBody`].
pub const UPVALUE_CELL_TYPE_TAG: u8 = 0x10;

/// GC-allocated payload backing every [`UpvalueCell`] handle.
///
/// Holds a single captured `Value`. Mutation flows through
/// [`store_upvalue`]; reads through [`read_upvalue`]; allocation
/// through [`alloc_upvalue`].
///
/// # Layout
///
/// One `Value` field. After task 76 the body is the only place
/// the captured value lives — every closure handle stores a
/// `Gc<UpvalueCellBody>` (4-byte compressed offset) instead of
/// the previous ref-counted mutable cell (8-byte pointer +
/// allocation overhead).
///
/// # Spec
///
/// Captured-binding semantics — ECMA-262
/// §9.1.1.1.4 (CreateMutableBinding) + §9.1.1.1.5
/// (InitializeBinding); the closure spine that holds these
/// cells is built by `Op::MakeClosure` per §15.2.5
/// (FunctionDeclarationInstantiation). Upvalue migration
/// rationale lives in the mdBook GC API chapter.
pub struct UpvalueCellBody {
    /// Captured `Value`. Phase 1: arbitrary `Value`; once
    /// `Value` carries `Gc<…>` variants (tasks 77+),
    /// [`store_upvalue`] fires
    /// [`otter_gc::GcHeap::write_barrier`] for every store
    /// whose RHS holds a GC handle.
    pub value: Value,
}

impl otter_gc::SafeTraceable for UpvalueCellBody {
    const TYPE_TAG: u8 = UPVALUE_CELL_TYPE_TAG;

    /// Walk the inner `Value` for any outgoing GC reference.
    ///
    /// Phase 1: `Value` carries no direct `Gc<…>` variants yet,
    /// but [`Value::Closure`] holds an `Rc<[UpvalueCell]>` whose
    /// elements are GC handles — those slots get yielded via
    /// [`Value::trace_value_slots`]. Each subsequent migration
    /// task (77–83) adds its variant arm there and the trace
    /// here picks it up automatically.
    fn trace_slots_safe(&self, v: &mut SlotVisitor<'_>) {
        self.value.trace_value_slots(v);
    }
}

/// Compressed handle to an [`UpvalueCellBody`] — replaces the
/// pre-task-76 ref-counted mutable cell. `Copy + Eq + Hash`
/// (inherited from [`otter_gc::Gc`]); identity comparison via
/// `cell == other`.
pub type UpvalueCell = otter_gc::Gc<UpvalueCellBody>;

/// Allocate a fresh [`UpvalueCell`] pre-populated with
/// `value` on the GC heap.
///
/// Routes through [`otter_gc::GcHeap::alloc_old`] so the body
/// is allocated directly in old-space — Phase-1 closure spines
/// (`Rc<[UpvalueCell]>`) cannot yet be rewritten by the
/// scavenger, and old-space objects do not move. Phase 2 may
/// switch back to [`otter_gc::GcHeap::alloc`] once the
/// scavenger walks every closure spine slot.
///
/// # Errors
///
/// Surfaces [`otter_gc::OutOfMemory`] verbatim; runtime callers
/// translate it into [`VmError::OutOfMemory`].
pub fn alloc_upvalue(
    heap: &mut otter_gc::GcHeap,
    value: Value,
) -> Result<UpvalueCell, otter_gc::OutOfMemory> {
    heap.alloc_old(UpvalueCellBody { value })
}

/// Read the captured value of `cell` (clones the payload).
#[must_use]
pub fn read_upvalue(heap: &otter_gc::GcHeap, cell: UpvalueCell) -> Value {
    heap.read_payload(cell, |body| body.value.clone())
}

/// Write `value` into `cell`, firing the generational write
/// barrier so the scavenger sees any newly-established
/// old → young pointer.
///
/// Phase 1: the barrier call is structurally present but
/// semantically a no-op for non-`Gc`-bearing `Value` variants.
/// As tasks 77+ add `Gc<…>` arms to [`Value`], the barrier
/// becomes load-bearing without changes to this call site.
pub fn store_upvalue(heap: &mut otter_gc::GcHeap, cell: UpvalueCell, value: Value) {
    let barrier_value = value.clone();
    heap.with_payload(cell, |body| {
        body.value = value;
    });
    heap.record_write(cell, &barrier_value);
}

impl Value {
    /// If `self` directly carries a `Gc<…>` handle (post-task-77
    /// variants), return its compressed offset for write-barrier
    /// dispatch. Phase 1: every variant returns `None` — `Value`
    /// holds only `Rc`-shared or POD payloads — so all stores
    /// route through the no-op-barrier path.
    ///
    /// Each per-type GC migration task adds its variant arm
    /// here so [`store_upvalue`] (and any future barrier
    /// caller) starts firing automatically.
    #[must_use]
    pub(crate) fn as_gc_raw(&self) -> Option<RawGc> {
        match self {
            // Task 77 — `JsObject` is a `Gc<ObjectBody>` handle.
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
            // Phase-1 stub for the rest. Subsequent migrations
            // (78+) add variant arms here as their handle types
            // move to `Gc<…>`.
            _ => None,
        }
    }

    /// Walk every `Gc<…>` slot held directly inside `self` and
    /// yield its slot pointer to `visitor`.
    ///
    /// Phase-1 special case: even though no `Value` variant
    /// carries a direct `Gc<…>` handle yet, [`Value::Closure`]
    /// holds an `Rc<[UpvalueCell]>` whose elements are
    /// `Gc<UpvalueCellBody>` handles (task 76). Each slot is
    /// surfaced through the visitor so the GC can mark every
    /// upvalue body reachable from this closure.
    ///
    /// # Safety contract for callers
    ///
    /// Implementations cast `&self` field addresses to
    /// `*mut RawGc` (raw cast, safe). The visitor is the GC's
    /// slot visitor — it does not need to write through the
    /// pointer for old-space objects (no movement), but Phase 2
    /// scavenger may rewrite slots.
    pub(crate) fn trace_value_slots(&self, visitor: &mut SlotVisitor<'_>) {
        match self {
            Value::Closure { upvalues, .. } => {
                for slot in upvalues.iter() {
                    let p = slot as *const UpvalueCell as *mut RawGc;
                    visitor(p);
                }
            }
            // Task 77 — `JsObject` is a `Gc<ObjectBody>` handle.
            // Yield the slot's storage address so the scavenger
            // can rewrite the offset in place when the body
            // moves (Phase 2; today old-space objects pinned).
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
            Value::Generator(generator) => {
                generator.trace_value_slots(visitor);
            }
            Value::BoundFunction(bound) => {
                bound.trace_value_slots(visitor);
            }
            Value::NativeFunction(native) => {
                native.trace_value_slots(visitor);
            }
            Value::RegExp(regexp) => {
                regexp.trace_value_slots(visitor);
            }
            Value::Proxy(proxy) => {
                proxy.trace_value_slots(visitor);
            }
            _ => {}
        }
    }

    /// Convenience: shared empty-string constant. Allocates only on
    /// first call per heap.
    pub fn empty_string(heap: &StringHeap) -> Result<Self, StringError> {
        Ok(Self::String(JsString::empty(heap)?))
    }

    /// Render the value as a debug-style string suitable for CLI
    /// preview output (e.g., `otter -p '"abc"'`).
    #[must_use]
    pub fn display_string(&self) -> String {
        match self {
            Value::Undefined | Value::Hole => "undefined".to_string(),
            Value::Null => "null".to_string(),
            Value::Boolean(b) => if *b { "true" } else { "false" }.to_string(),
            Value::Number(n) => n.to_display_string(),
            // BigInt rendering matches `BigInt.prototype.toString`:
            // decimal digits, no `n` suffix.
            Value::BigInt(b) => b.to_decimal_string(),
            Value::String(s) => s.to_lossy_string(),
            Value::Symbol(s) => s.descriptive_string(),
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
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
            Value::Date(d) => date::to_iso_string(d.time())
                .map(|s| format!("Date({s})"))
                .unwrap_or_else(|| "Invalid Date".to_string()),
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

    /// Spec [`ToBoolean`](https://tc39.es/ecma262/#sec-toboolean)
    /// for the foundation subset.
    #[must_use]
    pub fn to_boolean(&self) -> bool {
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
            // Spec ToBoolean(BigInt): false iff zero.
            Value::BigInt(b) => !b.as_inner().sign().eq(&num_bigint::Sign::NoSign),
            Value::String(s) => !s.is_empty(),
            // Symbol is always truthy per ECMA-262 §7.1.2; same for
            // every object-shaped reference type.
            Value::Symbol(_)
            | Value::Function { .. }
            | Value::Closure { .. }
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
            | Value::Date(_)
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
    /// instance. Used by the `Temporal` prototype dispatcher to
    /// pick the right per-kind table.
    #[must_use]
    pub fn as_temporal(&self) -> Option<&JsTemporal> {
        match self {
            Value::Temporal(t) => Some(t),
            _ => None,
        }
    }

    /// Spec [`typeof`](https://tc39.es/ecma262/#sec-typeof-operator)
    /// — return the JS-visible type tag string.
    ///
    /// # Algorithm
    /// 1. `undefined` → `"undefined"`.
    /// 2. `null` → `"object"` (the historical wart preserved by the
    ///    spec).
    /// 3. `boolean` → `"boolean"`; `number` → `"number"`;
    ///    `bigint` → `"bigint"`; `string` → `"string"`;
    ///    `symbol` → `"symbol"`.
    /// 4. Every callable (function / closure / bound / native /
    ///    class) → `"function"`.
    /// 5. Anything else → `"object"`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-typeof-operator>
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
            | Value::Closure { .. }
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
            | Value::Date(_)
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

    /// Construct a string value from in-memory text. Convenience
    /// for tests and the compiler's literal table.
    ///
    /// # Errors
    /// See [`JsString::from_str`].
    pub fn from_str(s: &str, heap: &StringHeap) -> Result<Self, StringError> {
        Ok(Self::String(JsString::from_str(s, heap)?))
    }
}

impl otter_gc::GcStore for Value {
    fn visit_gc_edges(&self, visitor: &mut dyn FnMut(otter_gc::GcEdge)) {
        if let Value::Closure { upvalues, .. } = self {
            for cell in upvalues.iter() {
                if let Some(edge) = otter_gc::GcEdge::from_gc(*cell) {
                    visitor(edge);
                }
            }
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
            // Strict equality across Number / BigInt is always
            // `false` per spec; the wildcard arm below handles
            // the cross-kind case.
            (Value::BigInt(a), Value::BigInt(b)) => a == b,
            (Value::String(a), Value::String(b)) => a.equals(b),
            // Symbol identity is ptr_eq on the inner Rc — distinct
            // `Symbol("x")` calls compare unequal even with matching
            // descriptions.
            (Value::Symbol(a), Value::Symbol(b)) => a.ptr_eq(b),
            (Value::Object(a), Value::Object(b)) => a == b,
            (Value::Array(a), Value::Array(b)) => crate::array::ptr_eq(*a, *b),
            (Value::Function { function_id: a }, Value::Function { function_id: b }) => a == b,
            (
                Value::Closure {
                    function_id: a,
                    upvalues: ua,
                    ..
                },
                Value::Closure {
                    function_id: b,
                    upvalues: ub,
                    ..
                },
            ) => a == b && std::rc::Rc::ptr_eq(ua, ub),
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
            (Value::Temporal(a), Value::Temporal(b)) => a.ptr_eq(b),
            (Value::Intl(a), Value::Intl(b)) => a.ptr_eq(b),
            (Value::ArrayBuffer(a), Value::ArrayBuffer(b)) => a.ptr_eq(b),
            (Value::DataView(a), Value::DataView(b)) => a.ptr_eq(b),
            (Value::TypedArray(a), Value::TypedArray(b)) => a.ptr_eq(b),
            (Value::Generator(a), Value::Generator(b)) => a.ptr_eq(b),
            (Value::Proxy(a), Value::Proxy(b)) => a.ptr_eq(b),
            _ => false,
        }
    }
}

impl Eq for Value {}

/// Match-based dispatch loop. The harness baseline; slice tasks may
/// later switch to threaded dispatch after benchmark-driven review
/// (foundation plan §"Interpreter requirements").
pub struct Interpreter {
    interrupt: InterruptFlag,
    string_heap: Arc<StringHeap>,
    /// Per-isolate GC heap. Owned here so allocator-bearing
    /// opcodes (e.g. `Op::MakeClosure`'s upvalue alloc since
    /// task 76) reach it through `&mut self`. The `Runtime`
    /// layer delegates `gc_heap` / `heap_stats` /
    /// `heap_snapshot` / `force_gc` accessors here.
    gc_heap: otter_gc::GcHeap,
    max_stack_depth: u32,
    /// Per-interpreter microtask queue. Plain field — accessed
    /// only through `&mut self`. The dispatch loop threads
    /// `&mut self.microtasks` alongside `&mut stack` (split-borrow)
    /// so `Op::QueueMicrotask` writes the deque without going
    /// through interior mutability. See `microtask::MicrotaskQueue`
    /// for the full contract; task 33 ships the sync side and
    /// reserves the async-inbox slot for task 35.
    microtasks: MicrotaskQueue,
    /// Per-run module-environment registry: module URL →
    /// `module_env` JsObject populated by that module's
    /// `<module-init>`. Written by the synthesised `<entry>`
    /// driver as it walks the topological order; read by
    /// [`otter_bytecode::Op::ImportNamespace`] when a closure
    /// inside one module needs the env of another.
    ///
    /// Cleared between top-level `run` invocations on the same
    /// interpreter so a fresh script doesn't observe stale
    /// modules.
    module_environments: std::collections::HashMap<std::rc::Rc<str>, JsObject>,
    /// Cached `(referrer, specifier) → target` lookup, built
    /// lazily from [`otter_bytecode::BytecodeModule::module_resolutions`]
    /// the first time the running module is observed. Cleared
    /// alongside `module_environments`.
    module_resolution_cache:
        std::collections::HashMap<(std::rc::Rc<str>, String), std::rc::Rc<str>>,
    /// Monomorphic `LoadProperty` inline caches keyed by
    /// dense executable IC site id. These are interpreter-local
    /// hints and never affect bytecode dumps or JS-visible semantics.
    load_property_ics: Vec<property_ic::PropertyIcEntry<property_ic::LoadPropertyIc>>,
    /// Monomorphic `StoreProperty` inline caches keyed by
    /// dense executable IC site id. These only cover ordinary own writable
    /// data slots; every miss falls back to full `[[Set]]` semantics.
    store_property_ics: Vec<property_ic::PropertyIcEntry<property_ic::StorePropertyIc>>,
    /// Monomorphic `HasProperty` inline caches keyed by dense executable IC
    /// site id. These only cover ordinary own/direct-prototype data presence.
    has_property_ics: Vec<property_ic::PropertyIcEntry<property_ic::HasPropertyIc>>,
    /// Cheap aggregate counters for interpreter property IC behavior.
    property_ic_stats: property_ic::PropertyIcStats,
    /// Optional per-turn resource policy. This slice records observations but
    /// does not yet yield or reject when a limit is exceeded.
    runtime_budget: RuntimeBudget,
    /// Aggregate VM resource counters for diagnostics and embedding policy
    /// work.
    runtime_budget_stats: RuntimeBudgetStats,
    /// Nested dispatch loops share one root-turn accounting window.
    runtime_budget_depth: u32,
    runtime_budget_turn_started_at: Option<std::time::Instant>,
    runtime_budget_heap_start: Option<RuntimeHeapSnapshot>,
    /// Per-interpreter table of well-known symbol singletons
    /// (ECMA-262 §6.1.5.1). Populated in [`Self::new`]; constant
    /// across an interpreter's lifetime.
    well_known_symbols: WellKnownSymbols,
    /// Global symbol registry backing `Symbol.for` / `Symbol.keyFor`
    /// (ECMA-262 §20.4.2.4 / §20.4.2.6).
    symbol_registry: SymbolRegistry,
    /// Per-interpreter registry of the seven canonical error
    /// classes (`Error`, `TypeError`, `RangeError`, `SyntaxError`,
    /// `ReferenceError`, `URIError`, `EvalError`) — ECMA-262 §19.3.
    /// Allocated once at startup; every `Op::NewError` /
    /// `Op::NewBuiltinError` / `Op::LoadBuiltinError` dispatch reads
    /// from this table so prototype identity (and therefore
    /// `instanceof`) stays stable across the interpreter's lifetime.
    error_classes: ErrorClassRegistry,
    /// Per-interpreter shared `globalThis` object — every
    /// `Op::LoadGlobalThis` returns a clone of this handle. Lazily
    /// allocated; the foundation seeds it with a self-reference
    /// (`globalThis.globalThis === globalThis`) so identity tests
    /// observe the standard shape.
    /// <https://tc39.es/ecma262/#sec-globalthis>
    global_this: JsObject,
    /// Optional embedder hook for `Op::Eval` / `Op::NewFunction`.
    /// Wired by the runtime layer at construction time to parse +
    /// compile a source string into a fresh [`BytecodeModule`].
    /// When `None`, both opcodes raise a SyntaxError so embedders
    /// can opt out of dynamic code.
    #[allow(clippy::type_complexity)]
    eval_hook: Option<EvalHook>,
    /// Side-channel for an unhandled JS-level throw originating
    /// inside a generator body that resumed via
    /// [`Self::resume_generator`]. The unwind machinery on the
    /// generator's sub-stack converts the throw into
    /// [`VmError::Uncaught`] (which loses the original `Value`); we
    /// preserve the original here so the calling
    /// [`Op::CallMethodValue`] arm can re-throw it on the outer
    /// stack and let user-level `try` / `catch` observe the right
    /// payload.
    pending_generator_throw: Option<Value>,
    /// Side-channel for an unhandled JS-level throw escaping a
    /// synchronous sub-dispatch such as a Proxy trap or callback
    /// invoked via [`Self::run_callable_sync`]. The sub-stack can
    /// only return [`VmError::Uncaught`] to its caller, so the
    /// original thrown value is preserved here until the outer
    /// dispatch loop re-throws it on the still-live caller stack.
    pending_uncaught_throw: Option<Value>,
    /// Stack-frame snapshot captured at the moment of the
    /// originating `Op::Throw` (before [`Self::unwind_throw`]
    /// pops handler-less frames). Surfaces as [`RunError::frames`]
    /// for [`VmError::Uncaught`] so embedders see the call site,
    /// not the empty post-unwind stack. Cleared at every `run_*`
    /// entry and at every successful catch.
    pending_uncaught_frames: Option<Vec<StackFrameSnapshot>>,
    /// Per-function user-property bag (§20.2.4 Function-instance
    /// properties + ordinary [[Set]] semantics for callables).
    /// `function_id` → `JsObject` carrying anything the user wrote
    /// via `f.foo = bar` / `Ctor.prototype.x = …` / etc. Lazily
    /// allocated on first write. Closures share the bag with their
    /// underlying function so writes through any closure handle
    /// land on the same place.
    function_user_props: std::collections::HashMap<u32, JsObject>,
    /// Deleted virtual `name` / `length` own properties for ordinary
    /// bytecode functions. Stored separately from the user bag so
    /// deleting built-in function metadata does not resurrect the
    /// intrinsic fallback on later reads.
    function_deleted_metadata: std::collections::HashSet<(u32, &'static str)>,
    /// Embedder-overridable sink behind the `console` namespace.
    /// Defaults to `println!` / `eprintln!` via
    /// [`console::StdConsoleSink`].
    console_sink: console::ConsoleSinkHandle,
    /// Host-side timer scheduler. Wired by the runtime layer so
    /// `setTimeout` / `clearTimeout` / `setInterval` /
    /// `clearInterval` natives can talk to the event loop without
    /// otter-vm depending on Tokio. `None` when the embedder did
    /// not install a scheduler — the natives raise a TypeError on
    /// call in that case.
    timer_scheduler: Option<timers::TimerSchedulerHandle>,
    /// Per-isolate map from host-issued timer token to JS callback +
    /// extra arguments. Populated by `setTimeout` / `setInterval`,
    /// drained by the runtime layer when a `TimerFired` inbox
    /// message arrives.
    timer_callbacks: timers::TimerCallbacks,
    /// Host-side dynamic-import scheduler. Wired by the runtime
    /// layer so `Op::ImportNamespaceDynamic` can register a
    /// pending promise and schedule on-demand module loading
    /// without otter-vm depending on the loader or Tokio. `None`
    /// when the embedder did not install one — the opcode then
    /// rejects with a `TypeError` for any unresolved specifier.
    dynamic_import_loader: Option<dynamic_import::DynamicImportLoaderHandle>,
    /// Per-isolate registry of pending dynamic-import promises
    /// (`u64 → JsPromiseHandle`). Populated by
    /// `Op::ImportNamespaceDynamic`, drained by the runtime layer
    /// when the host loader settles a token through
    /// [`Self::settle_dynamic_import`].
    dynamic_import_registry: dynamic_import::DynamicImportRegistry,
}

impl std::fmt::Debug for Interpreter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Interpreter")
            .field("max_stack_depth", &self.max_stack_depth)
            .field("eval_hook_installed", &self.eval_hook.is_some())
            .finish_non_exhaustive()
    }
}

/// Compile-time options for dynamic source text.
#[derive(Debug, Clone, Copy, Default)]
pub struct EvalCompileOptions {
    /// `true` for direct eval executed from strict code. The
    /// compiler stores the resulting strict bit on `<main>` so
    /// nested functions inherit it normally.
    pub force_strict: bool,
}

/// Embedder-supplied parse + compile callback used by
/// [`Op::Eval`] / [`Op::NewFunction`]. Returns a freshly linked
/// [`BytecodeModule`] whose `<main>` completion value becomes the
/// dispatch result.
pub type EvalHook = std::rc::Rc<dyn Fn(&str, EvalCompileOptions) -> Result<BytecodeModule, String>>;

struct StartupPhaseTimer {
    enabled: bool,
    start: std::time::Instant,
}

impl StartupPhaseTimer {
    fn from_env() -> Self {
        Self {
            enabled: std::env::var_os("OTTER_CLI_STARTUP_TIMINGS").is_some(),
            start: std::time::Instant::now(),
        }
    }

    fn mark(&self, label: &str) {
        if self.enabled {
            eprintln!(
                "otter_cli_startup phase={label} elapsed_us={}",
                self.start.elapsed().as_micros()
            );
        }
    }
}

impl Interpreter {
    /// Construct a fresh interpreter with its own interrupt flag,
    /// a no-cap string heap, the default stack-depth limit, and a
    /// fresh GC heap.
    #[must_use]
    pub fn new() -> Self {
        Self::with_string_heap_cap(0)
    }

    /// Construct an interpreter with a string heap cap (`0` =
    /// unlimited). The same cap is honoured by the interpreter's
    /// GC heap.
    #[must_use]
    pub fn with_string_heap_cap(cap_bytes: u64) -> Self {
        let startup_timer = StartupPhaseTimer::from_env();
        let string_heap = Arc::new(StringHeap::with_cap(cap_bytes));
        startup_timer.mark("vm_string_heap");
        let well_known_symbols = WellKnownSymbols::new(&string_heap)
            .expect("well-known symbol descriptions fit within any positive cap");
        startup_timer.mark("vm_well_known_symbols");
        let mut gc_heap = otter_gc::GcHeap::with_max_heap_bytes(cap_bytes)
            .expect("GcHeap construction never fails on the default cage");
        startup_timer.mark("vm_gc_heap");
        let error_classes = ErrorClassRegistry::new(&string_heap, &mut gc_heap)
            .expect("error class prototypes fit within any positive cap");
        startup_timer.mark("vm_error_classes");
        let global_this = bootstrap::build_global_this(&mut gc_heap)
            .expect("global_this fits within any positive cap");
        startup_timer.mark("vm_global_this");
        // §20.4.2 — install well-known symbols on the realm's
        // `Symbol` constructor + `Symbol.prototype[@@toPrimitive]`.
        // Bootstrap allocates the ctor + prototype objects; this
        // hook attaches the per-realm singleton symbols once
        // `WellKnownSymbols` exists.
        bootstrap::install_symbol_well_knowns_post_bootstrap(
            &mut gc_heap,
            &string_heap,
            global_this,
            &well_known_symbols,
        )
        .expect("Symbol well-known properties fit within any positive cap");
        // §20.2.3.6 — install `Function.prototype[@@hasInstance]`.
        // Bootstrap can't see `WellKnownSymbols`, so we wire the
        // realm-local @@hasInstance after both Function.prototype
        // and the symbol table exist.
        let function_prototype_handle = if let Some(Value::Object(function_ctor)) =
            object::get(global_this, &gc_heap, "Function")
            && let Some(Value::Object(function_proto)) =
                object::get(function_ctor, &gc_heap, "prototype")
        {
            let has_instance = well_known_symbols.get(symbol::WellKnown::HasInstance);
            let global_root = Value::Object(global_this);
            function_prototype::install_symbol_has_instance(
                &mut gc_heap,
                function_proto,
                has_instance,
                &[&global_root],
            )
            .expect("Function.prototype[@@hasInstance] fits within any positive cap");
            Some(function_proto)
        } else {
            None
        };
        // §20.5.6 — finalize the native error class hierarchy now
        // that `%Function.prototype%` and `%Object.prototype%` are
        // installed: link constructor and prototype `[[Prototype]]`
        // chains and surface every error constructor on `globalThis`
        // as a writable, non-enumerable, configurable data property.
        if let Some(function_prototype) = function_prototype_handle
            && let Some(Value::Object(object_ctor)) = object::get(global_this, &gc_heap, "Object")
            && let Some(Value::Object(object_prototype)) =
                object::get(object_ctor, &gc_heap, "prototype")
        {
            error_classes.finalize_after_bootstrap(
                &mut gc_heap,
                function_prototype,
                object_prototype,
                global_this,
            );
        }
        Self {
            interrupt: InterruptFlag::new(),
            string_heap,
            gc_heap,
            max_stack_depth: DEFAULT_MAX_STACK_DEPTH,
            microtasks: MicrotaskQueue::new(),
            module_environments: std::collections::HashMap::new(),
            module_resolution_cache: std::collections::HashMap::new(),
            load_property_ics: Vec::new(),
            store_property_ics: Vec::new(),
            has_property_ics: Vec::new(),
            property_ic_stats: property_ic::PropertyIcStats::default(),
            runtime_budget: RuntimeBudget::default(),
            runtime_budget_stats: RuntimeBudgetStats::default(),
            runtime_budget_depth: 0,
            runtime_budget_turn_started_at: None,
            runtime_budget_heap_start: None,
            well_known_symbols,
            symbol_registry: SymbolRegistry::new(),
            error_classes,
            global_this,
            eval_hook: None,
            pending_generator_throw: None,
            pending_uncaught_throw: None,
            pending_uncaught_frames: None,
            function_user_props: std::collections::HashMap::new(),
            function_deleted_metadata: std::collections::HashSet::new(),
            console_sink: console::default_console_sink(),
            timer_scheduler: None,
            timer_callbacks: timers::TimerCallbacks::new(),
            dynamic_import_loader: None,
            dynamic_import_registry: dynamic_import::DynamicImportRegistry::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn load_property_ic_count(&self) -> usize {
        self.load_property_ics
            .iter()
            .filter(|entry| entry.is_monomorphic())
            .count()
    }

    #[cfg(test)]
    pub(crate) fn store_property_ic_count(&self) -> usize {
        self.store_property_ics
            .iter()
            .filter(|entry| entry.is_monomorphic())
            .count()
    }

    /// Return aggregate property inline-cache counters.
    #[must_use]
    pub fn property_ic_stats(&self) -> property_ic::PropertyIcStats {
        self.property_ic_stats
    }

    /// Return the current observational runtime budget policy.
    #[must_use]
    pub fn runtime_budget(&self) -> RuntimeBudget {
        self.runtime_budget
    }

    /// Set the observational runtime budget policy.
    ///
    /// The current VM records exceedance observations but does not preempt,
    /// yield, or reject when limits are crossed.
    pub fn set_runtime_budget(&mut self, budget: RuntimeBudget) {
        self.runtime_budget = budget;
    }

    /// Return aggregate runtime budget/resource counters.
    #[must_use]
    pub fn runtime_budget_stats(&self) -> RuntimeBudgetStats {
        self.runtime_budget_stats
    }

    /// Reset aggregate runtime budget/resource counters.
    pub fn reset_runtime_budget_stats(&mut self) {
        self.runtime_budget_stats = RuntimeBudgetStats::default();
        self.runtime_budget_depth = 0;
        self.runtime_budget_turn_started_at = None;
        self.runtime_budget_heap_start = None;
    }

    fn begin_runtime_budget_turn(&mut self) {
        if self.runtime_budget_depth == 0 {
            self.runtime_budget_stats.begin_turn();
            self.runtime_budget_turn_started_at = Some(std::time::Instant::now());
            let heap = RuntimeHeapSnapshot::from_heap(&mut self.gc_heap);
            self.runtime_budget_heap_start = Some(heap);
        }
        self.runtime_budget_depth = self.runtime_budget_depth.saturating_add(1);
    }

    fn finish_runtime_budget_turn(&mut self) {
        self.runtime_budget_depth = self.runtime_budget_depth.saturating_sub(1);
        if self.runtime_budget_depth == 0
            && let Some(started_at) = self.runtime_budget_turn_started_at.take()
        {
            if let Some(start_heap) = self.runtime_budget_heap_start.take() {
                let end_heap = RuntimeHeapSnapshot::from_heap(&mut self.gc_heap);
                self.runtime_budget_stats
                    .record_turn_heap_delta(start_heap, end_heap);
            }
            self.runtime_budget_stats
                .finish_turn(started_at.elapsed(), self.runtime_budget);
        }
    }

    fn record_runtime_reductions(&mut self, units: u64) {
        self.runtime_budget_stats.record_reductions(units);
    }

    fn enforce_runtime_budget_checkpoint(&mut self) -> Result<(), VmError> {
        if !self.runtime_budget.rejects_on_exceedance() {
            return Ok(());
        }
        let Some(started_at) = self.runtime_budget_turn_started_at else {
            return Ok(());
        };
        if self.runtime_budget.has_heap_checkpoint_limits()
            && let Some(start_heap) = self.runtime_budget_heap_start
        {
            let end_heap = RuntimeHeapSnapshot::from_heap(&mut self.gc_heap);
            self.runtime_budget_stats
                .observe_current_turn_heap_delta(start_heap, end_heap);
        }
        let elapsed_nanos = u64::try_from(started_at.elapsed().as_nanos()).unwrap_or(u64::MAX);
        if runtime_budget::budget_exceeded(
            self.runtime_budget_stats.current_turn_reductions,
            self.runtime_budget_stats.current_turn_allocated_bytes,
            self.runtime_budget_stats.current_turn_host_ops,
            elapsed_nanos,
            self.runtime_budget_stats.current_external_bytes,
            self.runtime_budget,
        ) {
            self.runtime_budget_stats.record_budget_rejection();
            return Err(VmError::BudgetExceeded {
                message: "runtime budget exceeded".to_string(),
            });
        }
        Ok(())
    }

    fn observe_runtime_stack_depth(&mut self, depth: usize) {
        self.runtime_budget_stats.observe_stack_depth(depth);
    }

    fn record_runtime_bytecode_call(&mut self) {
        self.runtime_budget_stats.record_bytecode_call();
    }

    fn record_runtime_native_call(&mut self) {
        self.runtime_budget_stats.record_native_call();
    }

    fn record_runtime_construct_call(&mut self) {
        self.runtime_budget_stats.record_construct_call();
    }

    pub(crate) fn record_runtime_host_op_enqueued(&mut self) {
        self.runtime_budget_stats.record_host_op_enqueued();
    }

    fn record_runtime_microtask_drain_started(&mut self) {
        self.runtime_budget_stats.record_microtask_drain_started();
    }

    fn record_runtime_microtask_executed(&mut self) {
        self.runtime_budget_stats.record_microtask_executed();
    }

    fn observe_runtime_microtask_budget(&mut self, microtasks_this_drain: u64) -> bool {
        if self
            .runtime_budget
            .max_microtasks_per_drain
            .is_some_and(|limit| microtasks_this_drain > limit)
        {
            self.runtime_budget_stats.record_budget_limit_observation();
            true
        } else {
            false
        }
    }

    fn ensure_property_ic_capacity(&mut self, context: &ExecutionContext) {
        let site_count = context.property_ic_site_count();
        if self.load_property_ics.len() < site_count {
            self.load_property_ics
                .resize(site_count, property_ic::PropertyIcEntry::Empty);
        }
        if self.store_property_ics.len() < site_count {
            self.store_property_ics
                .resize(site_count, property_ic::PropertyIcEntry::Empty);
        }
        if self.has_property_ics.len() < site_count {
            self.has_property_ics
                .resize(site_count, property_ic::PropertyIcEntry::Empty);
        }
    }

    /// Install the host-side timer scheduler. Called by the
    /// runtime layer at construction time so the JS-visible
    /// `setTimeout` / `setInterval` natives can route through the
    /// event-loop scheduler.
    pub fn set_timer_scheduler(&mut self, scheduler: timers::TimerSchedulerHandle) {
        self.timer_scheduler = Some(scheduler);
    }

    /// Clone the installed timer scheduler, if any. Native-function
    /// implementations of `setTimeout` / `clearTimeout` use this to
    /// schedule and cancel without holding `&mut self` over the
    /// host-side call.
    #[must_use]
    pub fn timer_scheduler(&self) -> Option<timers::TimerSchedulerHandle> {
        self.timer_scheduler.clone()
    }

    /// Mutable handle to the timer-callback registry.
    pub fn timer_callbacks_mut(&mut self) -> &mut timers::TimerCallbacks {
        &mut self.timer_callbacks
    }

    /// Read-only view of the timer-callback registry.
    #[must_use]
    pub fn timer_callbacks(&self) -> &timers::TimerCallbacks {
        &self.timer_callbacks
    }

    /// Install the host-side dynamic-import scheduler.
    pub fn set_dynamic_import_loader(&mut self, loader: dynamic_import::DynamicImportLoaderHandle) {
        self.dynamic_import_loader = Some(loader);
    }

    /// Clone the installed dynamic-import scheduler, if any.
    #[must_use]
    pub fn dynamic_import_loader(&self) -> Option<dynamic_import::DynamicImportLoaderHandle> {
        self.dynamic_import_loader.clone()
    }

    /// Read-only view of the dynamic-import registry.
    #[must_use]
    pub fn dynamic_import_registry(&self) -> &dynamic_import::DynamicImportRegistry {
        &self.dynamic_import_registry
    }

    /// Mutable handle to the dynamic-import registry.
    pub fn dynamic_import_registry_mut(&mut self) -> &mut dynamic_import::DynamicImportRegistry {
        &mut self.dynamic_import_registry
    }

    /// Settle a pending dynamic-import promise registered under
    /// `token`. Routes through the standard promise dispatch path
    /// so reactions land on the per-isolate microtask queue;
    /// callers are expected to drain microtasks after calling
    /// this. A missing or already-settled token is a silent no-op.
    pub fn settle_dynamic_import(
        &mut self,
        token: u64,
        outcome: Result<Value, Value>,
    ) -> Option<ExecutionContext> {
        let entry = match self.dynamic_import_registry.take(token) {
            Some(entry) => entry,
            None => return None,
        };
        let jobs = match outcome {
            Ok(value) => crate::JsPromise::fulfill(&entry.promise, &mut self.gc_heap, value),
            Err(reason) => crate::JsPromise::reject(&entry.promise, &mut self.gc_heap, reason),
        };
        for j in jobs.jobs {
            self.microtasks.enqueue(j);
        }
        Some(entry.context)
    }

    /// Replace the sink used by `console.*` methods.
    pub fn set_console_sink(&mut self, sink: console::ConsoleSinkHandle) {
        self.console_sink = sink;
    }

    /// Clone the sink used by `console.*` methods.
    #[must_use]
    pub fn console_sink(&self) -> console::ConsoleSinkHandle {
        self.console_sink.clone()
    }

    /// Return the realm's shared `%ThrowTypeError%` function.
    ///
    /// Bootstrap installs it as the getter/setter for
    /// `Function.prototype.caller`; unmapped arguments objects reuse
    /// that exact function object for `callee` so Test262's
    /// well-known-intrinsic identity checks observe one realm-local
    /// intrinsic.
    fn restricted_throw_type_error(&self) -> Result<Value, VmError> {
        let prototype = self.function_prototype_object()?;
        match object::get_own_descriptor(prototype, &self.gc_heap, "caller") {
            Some(object::PropertyDescriptor {
                kind:
                    object::DescriptorKind::Accessor {
                        getter: Some(getter),
                        ..
                    },
                ..
            }) => Ok(getter),
            _ => Err(VmError::TypeMismatch),
        }
    }

    /// `[[GetPrototypeOf]]` for non-Proxy heap values. Centralises
    /// the foundation rule that constructor-shaped Objects whose
    /// stored `[[Prototype]]` is missing — or is the realm's
    /// `%Object.prototype%` (the default link from many bootstrap
    /// installers) — surface as `%Function.prototype%`. Explicit
    /// proto links to anything else (e.g. `Error.[[Prototype]]` =
    /// `%Function.prototype%`, `TypeError.[[Prototype]]` = `Error`)
    /// are honoured verbatim.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-ordinarygetprototypeof>
    pub(crate) fn get_prototype_for_op(&self, value: &Value) -> Result<Value, VmError> {
        match value {
            Value::Object(obj) => {
                let stored = object::prototype_value(*obj, &self.gc_heap);
                let has_construct = object_has_construct_slot(&Value::Object(*obj), &self.gc_heap);
                if has_construct {
                    let function_proto = self.function_prototype_object().ok();
                    let object_proto = self.object_prototype_object_opt();
                    match &stored {
                        // No stored proto on a callable Object →
                        // foundation fallback to %Function.prototype%.
                        None => {
                            if let Some(fp) = function_proto {
                                return Ok(Value::Object(fp));
                            }
                        }
                        // Stored proto is %Object.prototype% — the
                        // bootstrap installers use it as a default;
                        // hoist to %Function.prototype% to keep the
                        // observable spec shape on built-ins like
                        // `Number`, `Boolean`, `Date`, `Array`, etc.
                        Some(Value::Object(p)) if object_proto.is_some_and(|op| op == *p) => {
                            if let Some(fp) = function_proto {
                                return Ok(Value::Object(fp));
                            }
                        }
                        _ => {}
                    }
                }
                Ok(stored.unwrap_or(Value::Null))
            }
            Value::NativeFunction(_)
            | Value::Function { .. }
            | Value::Closure { .. }
            | Value::BoundFunction(_)
            | Value::ClassConstructor(_) => Ok(Value::Object(self.function_prototype_object()?)),
            // §10.4 exotic objects (Array, Map, Set, WeakMap,
            // WeakSet, WeakRef, FinalizationRegistry, Promise,
            // ArrayBuffer, SharedArrayBuffer, DataView, TypedArray,
            // RegExp) carry their own per-class realm prototype.
            // Route through `intrinsic_prototype_object_for` so
            // `Object.getPrototypeOf(buf)` / `__proto__` walks
            // hit `%<Kind>.prototype%` instead of `TypeError:
            // operand type mismatch`. Spec: §10.1.1 [[GetPrototypeOf]]
            // is "OrdinaryGetPrototypeOf", which reads the slot the
            // class set at allocation time.
            // <https://tc39.es/ecma262/#sec-ordinarygetprototypeof>
            Value::Array(_)
            | Value::RegExp(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::WeakMap(_)
            | Value::WeakSet(_)
            | Value::WeakRef(_)
            | Value::FinalizationRegistry(_)
            | Value::Promise(_)
            | Value::ArrayBuffer(_)
            | Value::DataView(_)
            | Value::TypedArray(_) => match self.intrinsic_prototype_object_for(value) {
                Some(o) => Ok(Value::Object(o)),
                None => Ok(Value::Null),
            },
            other => Err(VmError::TypeMismatchAt {
                op: "Object.getPrototypeOf",
                kind: value_kind_name(other),
            }),
        }
    }

    fn object_prototype_object_opt(&self) -> Option<JsObject> {
        match object::get(self.global_this, &self.gc_heap, "Object") {
            Some(Value::Object(ctor)) => match object::get(ctor, &self.gc_heap, "prototype") {
                Some(Value::Object(p)) => Some(p),
                _ => None,
            },
            _ => None,
        }
    }

    pub(crate) fn function_prototype_object(&self) -> Result<JsObject, VmError> {
        let function_ctor = match object::get(self.global_this, &self.gc_heap, "Function") {
            Some(Value::Object(obj)) => obj,
            _ => return Err(VmError::TypeMismatch),
        };
        match object::get(function_ctor, &self.gc_heap, "prototype") {
            Some(Value::Object(obj)) => Ok(obj),
            _ => Err(VmError::TypeMismatch),
        }
    }

    fn is_callable_runtime(&self, value: &Value) -> bool {
        is_callable(value) || object_has_call_slot(value, &self.gc_heap)
    }

    /// Resolve a property read on a `Value::Function` /
    /// `Value::Closure`. Honours user-installed properties via the
    /// `function_user_props` side table, lazily allocates
    /// `Function.prototype` on first access (§9.2.10
    /// MakeConstructor), and falls back to `name` / `length`
    /// intrinsics. Unknown names return `undefined` per §10.1.8
    /// OrdinaryGet step 4.
    /// Borrow the per-interpreter table of well-known symbol
    /// singletons. The table is constant across the interpreter's
    /// lifetime.
    #[must_use]
    pub fn well_known_symbols(&self) -> &WellKnownSymbols {
        &self.well_known_symbols
    }

    /// Borrow the global symbol registry backing `Symbol.for` /
    /// `Symbol.keyFor`. Returns the same instance across calls.
    #[must_use]
    pub fn symbol_registry(&self) -> &SymbolRegistry {
        &self.symbol_registry
    }

    /// Register or overwrite a module's `module_env` object so
    /// later [`Op::ImportNamespace`] dispatches can resolve
    /// references to it.
    ///
    /// Called by the runtime's module-graph driver as it walks
    /// the topological order — once a module's `<module-init>`
    /// has run and populated its env, the driver records it
    /// here keyed by canonical URL.
    pub fn register_module_env(&mut self, url: std::rc::Rc<str>, env: JsObject) {
        self.module_environments.insert(url, env);
    }

    /// Borrow a module's `module_env` JsObject by URL. Returns
    /// `None` when the URL is unknown — the runtime surfaces
    /// that as a catchable diagnostic upstream rather than
    /// silently filling with `undefined`.
    #[must_use]
    pub fn module_env(&self, url: &str) -> Option<JsObject> {
        self.module_environments.get(url).cloned()
    }

    /// Drop every recorded module environment + resolution
    /// cache entry. Called between top-level `run` invocations
    /// on the same interpreter so a fresh script never observes
    /// stale modules.
    pub fn reset_module_state(&mut self) {
        self.module_environments.clear();
        self.module_resolution_cache.clear();
    }

    /// Resolve a specifier seen by the running module to the
    /// target module's `module_env`. Returns `None` when the
    /// linker did not register a resolution for the
    /// `(referrer, specifier)` pair, or when the resolution
    /// pointed at a URL that no `module_env` has been recorded
    /// for yet.
    ///
    /// # Algorithm
    /// 1. Look in `module_resolution_cache` keyed by
    ///    `(referrer, specifier)`. Fast path: pre-built entry,
    ///    one hashmap probe.
    /// 2. On miss, scan
    ///    [`otter_bytecode::BytecodeModule::module_resolutions`]
    ///    for the matching triple, populate the cache, return.
    /// 3. With the resolved target URL in hand, look up the
    ///    `module_env` in `module_environments`.
    ///
    /// # Invariants
    /// - `module_resolutions` is small (one entry per actual
    ///   import edge in the graph), so the linear scan on
    ///   miss is cheap. Real engines reach for a hashmap;
    ///   the foundation prefers a flat vector that round-trips
    ///   cleanly through the bytecode dump.
    fn resolve_module_namespace(
        &mut self,
        context: &ExecutionContext,
        referrer: &str,
        specifier: &str,
    ) -> Option<JsObject> {
        let referrer_rc: std::rc::Rc<str> = std::rc::Rc::from(referrer);
        let key = (referrer_rc.clone(), specifier.to_string());
        let target_url = if let Some(hit) = self.module_resolution_cache.get(&key) {
            hit.clone()
        } else {
            let target = context.module_resolution_target(referrer, specifier)?;
            let target_rc: std::rc::Rc<str> = std::rc::Rc::from(target);
            self.module_resolution_cache.insert(key, target_rc.clone());
            target_rc
        };
        self.module_environments.get(target_url.as_ref()).cloned()
    }

    /// Mutable handle to the isolate-local microtask queue.
    /// Host-side async callbacks must re-enter the isolate before
    /// enqueueing GC-bearing [`Microtask`] values.
    pub fn microtasks_mut(&mut self) -> &mut MicrotaskQueue {
        &mut self.microtasks
    }

    /// Read-only view of the microtask queue.
    #[must_use]
    pub fn microtasks(&self) -> &MicrotaskQueue {
        &self.microtasks
    }

    /// Override the stack-depth limit. `0` is treated as the
    /// configured default (foundation slice rejects an explicit
    /// `0` limit at the `RuntimeBuilder` boundary, so this
    /// fall-through is defensive).
    pub fn set_max_stack_depth(&mut self, depth: u32) {
        self.max_stack_depth = if depth == 0 {
            DEFAULT_MAX_STACK_DEPTH
        } else {
            depth
        };
    }

    /// Install the parse + compile callback used by `Op::Eval` and
    /// `Op::NewFunction`. The runtime layer hooks the otter-compiler
    /// in here at construction time. Pass `None` (the default) to
    /// disable dynamic code; both opcodes will raise SyntaxError
    /// when invoked without a hook.
    pub fn set_eval_hook(&mut self, hook: Option<EvalHook>) {
        self.eval_hook = hook;
    }

    /// Cloneable handle for cooperative cancellation.
    #[must_use]
    pub fn interrupt_handle(&self) -> InterruptFlag {
        self.interrupt.clone()
    }

    /// Borrow the string heap accountant. Tests use this to assert
    /// counter behavior on rejected allocations.
    #[must_use]
    pub fn string_heap(&self) -> &StringHeap {
        &self.string_heap
    }

    /// Clone-out the string heap handle. Used by native closures
    /// (e.g. `Promise.allSettled`) that need to allocate strings
    /// from a deferred microtask without re-borrowing the
    /// interpreter.
    #[must_use]
    pub fn string_heap_clone(&self) -> Arc<StringHeap> {
        self.string_heap.clone()
    }

    /// Clone-out the error-class registry. Used by native closures
    /// (e.g. `Promise.any`) that need to build error instances from
    /// a deferred microtask.
    #[must_use]
    pub fn error_classes_clone(&self) -> ErrorClassRegistry {
        self.error_classes.clone()
    }

    /// Borrow the shared `globalThis` object. Used by the GC
    /// root walker (task 75) and by any embedder reading the
    /// foundation seed identity (`globalThis.globalThis ===
    /// globalThis`).
    #[must_use]
    pub fn global_this(&self) -> &JsObject {
        &self.global_this
    }

    /// Install `value` as the `name` property on `globalThis` with
    /// the standard `{ writable: true, enumerable: false,
    /// configurable: true }` data-descriptor attributes used by
    /// every default-global binding (§17 + §19). Public entry for
    /// embedders that need to inject a runtime-side value into
    /// scripts (e.g. host-bound promises, capability tokens).
    pub fn set_global(&mut self, name: &str, value: Value) {
        let descriptor = crate::object::PropertyDescriptor::data(value, true, false, true);
        let _ = crate::object::define_own_property(
            self.global_this,
            &mut self.gc_heap,
            name,
            descriptor,
        );
    }

    fn primitive_wrapper_prototype(&self, constructor_name: &str) -> Result<JsObject, VmError> {
        let constructor = object::get(self.global_this, &self.gc_heap, constructor_name)
            .ok_or(VmError::InvalidOperand)?;
        let prototype = match &constructor {
            Value::Object(ctor) => object::get(*ctor, &self.gc_heap, "prototype"),
            Value::NativeFunction(native) => {
                let desc = native
                    .own_property_descriptor(&self.gc_heap, &self.string_heap, "prototype")
                    .map_err(|_| VmError::InvalidOperand)?;
                desc.and_then(|d| match d.kind {
                    object::DescriptorKind::Data { value } => Some(value),
                    _ => None,
                })
            }
            _ => None,
        };
        match prototype {
            Some(Value::Object(p)) => Ok(p),
            _ => Err(VmError::InvalidOperand),
        }
    }

    fn box_sloppy_this_primitive_runtime_rooted(
        &mut self,
        this_value: Value,
        slice_roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let object = match &this_value {
            Value::Boolean(value) => {
                let proto = self.primitive_wrapper_prototype("Boolean")?;
                let obj = self.alloc_runtime_rooted_object_with_proto(
                    proto,
                    &[&this_value],
                    slice_roots,
                )?;
                object::set_boolean_data(obj, &mut self.gc_heap, *value);
                obj
            }
            Value::Number(value) => {
                let proto = self.primitive_wrapper_prototype("Number")?;
                let obj = self.alloc_runtime_rooted_object_with_proto(
                    proto,
                    &[&this_value],
                    slice_roots,
                )?;
                object::set_number_data(obj, &mut self.gc_heap, *value);
                obj
            }
            Value::String(value) => {
                let proto = self.primitive_wrapper_prototype("String")?;
                let obj = self.alloc_runtime_rooted_object_with_proto(
                    proto,
                    &[&this_value],
                    slice_roots,
                )?;
                object::set_string_data(obj, &mut self.gc_heap, value.clone());
                obj
            }
            Value::Symbol(_) => {
                let proto = self.primitive_wrapper_prototype("Symbol")?;
                self.alloc_runtime_rooted_object_with_proto(proto, &[&this_value], slice_roots)?
            }
            Value::BigInt(_) => {
                let proto = self.primitive_wrapper_prototype("BigInt")?;
                self.alloc_runtime_rooted_object_with_proto(proto, &[&this_value], slice_roots)?
            }
            _ => return Ok(this_value),
        };
        Ok(Value::Object(object))
    }

    fn box_sloppy_this_primitive_stack_rooted(
        &mut self,
        stack: &SmallVec<[Frame; 8]>,
        this_value: Value,
        slice_roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let object = match &this_value {
            Value::Boolean(value) => {
                let proto = self.primitive_wrapper_prototype("Boolean")?;
                let obj = self.alloc_stack_rooted_object_with_proto(
                    stack,
                    proto,
                    &[&this_value],
                    slice_roots,
                )?;
                object::set_boolean_data(obj, &mut self.gc_heap, *value);
                obj
            }
            Value::Number(value) => {
                let proto = self.primitive_wrapper_prototype("Number")?;
                let obj = self.alloc_stack_rooted_object_with_proto(
                    stack,
                    proto,
                    &[&this_value],
                    slice_roots,
                )?;
                object::set_number_data(obj, &mut self.gc_heap, *value);
                obj
            }
            Value::String(value) => {
                let proto = self.primitive_wrapper_prototype("String")?;
                let obj = self.alloc_stack_rooted_object_with_proto(
                    stack,
                    proto,
                    &[&this_value],
                    slice_roots,
                )?;
                object::set_string_data(obj, &mut self.gc_heap, value.clone());
                obj
            }
            Value::Symbol(_) => {
                let proto = self.primitive_wrapper_prototype("Symbol")?;
                self.alloc_stack_rooted_object_with_proto(
                    stack,
                    proto,
                    &[&this_value],
                    slice_roots,
                )?
            }
            Value::BigInt(_) => {
                let proto = self.primitive_wrapper_prototype("BigInt")?;
                self.alloc_stack_rooted_object_with_proto(
                    stack,
                    proto,
                    &[&this_value],
                    slice_roots,
                )?
            }
            _ => return Ok(this_value),
        };
        Ok(Value::Object(object))
    }

    fn object_for_primitive_property_base_stack_rooted(
        &mut self,
        stack: &SmallVec<[Frame; 8]>,
        value: &Value,
    ) -> Result<Option<JsObject>, VmError> {
        let object = match value {
            Value::Boolean(v) => {
                let proto = self.primitive_wrapper_prototype("Boolean")?;
                let obj = self.alloc_stack_rooted_object_with_proto(stack, proto, &[value], &[])?;
                object::set_boolean_data(obj, &mut self.gc_heap, *v);
                obj
            }
            Value::Number(v) => {
                let proto = self.primitive_wrapper_prototype("Number")?;
                let obj = self.alloc_stack_rooted_object_with_proto(stack, proto, &[value], &[])?;
                object::set_number_data(obj, &mut self.gc_heap, *v);
                obj
            }
            Value::String(v) => {
                let proto = self.primitive_wrapper_prototype("String")?;
                let obj = self.alloc_stack_rooted_object_with_proto(stack, proto, &[value], &[])?;
                object::set_string_data(obj, &mut self.gc_heap, v.clone());
                obj
            }
            Value::Symbol(_) => {
                let proto = self.primitive_wrapper_prototype("Symbol")?;
                self.alloc_stack_rooted_object_with_proto(stack, proto, &[value], &[])?
            }
            Value::BigInt(_) => {
                let proto = self.primitive_wrapper_prototype("BigInt")?;
                self.alloc_stack_rooted_object_with_proto(stack, proto, &[value], &[])?
            }
            _ => return Ok(None),
        };
        Ok(Some(object))
    }

    fn this_for_bytecode_call_runtime_rooted(
        &mut self,
        function: &ExecutableFunction,
        this_value: Value,
        slice_roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        if function.is_strict || function.is_arrow {
            return Ok(this_value);
        }
        match this_value {
            Value::Undefined | Value::Null => Ok(Value::Object(self.global_this)),
            other => self.box_sloppy_this_primitive_runtime_rooted(other, slice_roots),
        }
    }

    fn this_for_bytecode_call_stack_rooted(
        &mut self,
        function: &ExecutableFunction,
        stack: &SmallVec<[Frame; 8]>,
        this_value: Value,
        slice_roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        if function.is_strict || function.is_arrow {
            return Ok(this_value);
        }
        match this_value {
            Value::Undefined | Value::Null => Ok(Value::Object(self.global_this)),
            other => self.box_sloppy_this_primitive_stack_rooted(stack, other, slice_roots),
        }
    }

    /// Install a class-shaped global from a static JS surface spec.
    ///
    /// Product crates use this for centralized bootstrap wiring:
    /// specs stay static, while the actual object allocation and
    /// global mutation happen during one mutator turn.
    pub fn install_global_class(&mut self, spec: &'static ClassSpec) -> Result<(), JsSurfaceError> {
        let raw_roots = self.collect_runtime_roots();
        let global_root = Value::Object(self.global_this);
        let value = ClassBuilder::from_spec_with_raw_and_value_roots(
            &mut self.gc_heap,
            spec,
            raw_roots,
            vec![global_root],
        )
        .build()?;
        let descriptor = crate::object::PropertyDescriptor::data(
            value,
            spec.constructor.attrs.writable,
            spec.constructor.attrs.enumerable,
            spec.constructor.attrs.configurable,
        );
        if crate::object::define_own_property(
            self.global_this,
            &mut self.gc_heap,
            spec.constructor.name,
            descriptor,
        ) {
            Ok(())
        } else {
            Err(JsSurfaceError::DefinePropertyFailed(spec.constructor.name))
        }
    }

    /// Iterator over every `module_env` object in the per-run
    /// module-environment registry. Used by the GC root
    /// walker (task 75) — values are `JsObject`s holding
    /// live module bindings.
    pub fn module_environments_for_trace(&self) -> impl Iterator<Item = &JsObject> {
        self.module_environments.values()
    }

    /// Borrow the well-known symbol singleton table. Used by
    /// the GC root walker (task 75).
    #[must_use]
    pub fn well_known_symbols_for_trace(&self) -> &WellKnownSymbols {
        &self.well_known_symbols
    }

    /// Borrow the error-class registry. Used by the GC root
    /// walker (task 75); embedder-facing reads should prefer
    /// [`Self::error_classes_clone`].
    #[must_use]
    pub fn error_classes_for_trace(&self) -> &ErrorClassRegistry {
        &self.error_classes
    }

    /// Borrow the symbol registry. Used by the GC root walker
    /// (task 75); see also [`Self::symbol_registry`] which is
    /// the older spelling kept for back-compat.
    #[must_use]
    pub fn symbol_registry_for_trace(&self) -> &SymbolRegistry {
        &self.symbol_registry
    }

    /// Iterator over every per-function user-property bag.
    /// Used by the GC root walker (task 75) — each value is a
    /// `JsObject` carrying user-side `f.foo = bar` writes.
    pub fn function_user_props_for_trace(&self) -> impl Iterator<Item = &JsObject> {
        self.function_user_props.values()
    }

    /// Borrow the pending-generator-throw side-channel slot.
    /// Used by the GC root walker (task 75); the body of the
    /// trace stays empty until task 76 (when `Value` carries
    /// its first `Gc<…>`-shaped variant).
    #[must_use]
    pub fn pending_generator_throw_for_trace(&self) -> Option<&Value> {
        self.pending_generator_throw.as_ref()
    }

    /// Borrow the pending uncaught throw side-channel slot for GC
    /// root tracing.
    #[must_use]
    pub fn pending_uncaught_throw_for_trace(&self) -> Option<&Value> {
        self.pending_uncaught_throw.as_ref()
    }

    /// Consume the pending uncaught-throw payload, if any. Embedder
    /// callers that catch a `VmError::Uncaught` at a sync entry
    /// point use this to recover the original thrown
    /// [`Value`] (an `Error` instance, a string, etc.) instead of
    /// the lossy `Display` rendering carried by the `VmError`.
    pub fn take_pending_uncaught_throw(&mut self) -> Option<Value> {
        self.pending_uncaught_throw.take()
    }

    /// Borrow the per-isolate GC heap (read-only).
    #[must_use]
    pub fn gc_heap(&self) -> &otter_gc::GcHeap {
        &self.gc_heap
    }

    /// Mutable borrow of the per-isolate GC heap.
    #[must_use]
    pub fn gc_heap_mut(&mut self) -> &mut otter_gc::GcHeap {
        &mut self.gc_heap
    }

    /// `pub(crate)` alias used by [`crate::runtime_cx::RuntimeCx`]
    /// to forward the heap borrow without rebinding through a
    /// public method. Tracks the explicit-context migration in
    /// task 76A.
    #[must_use]
    pub(crate) fn gc_heap_for_cx(&self) -> &otter_gc::GcHeap {
        &self.gc_heap
    }

    /// `pub(crate)` mutable alias — see [`Self::gc_heap_for_cx`].
    #[must_use]
    pub(crate) fn gc_heap_for_cx_mut(&mut self) -> &mut otter_gc::GcHeap {
        &mut self.gc_heap
    }

    /// Force a full GC cycle. Pre-collects every root slot via
    /// [`crate::runtime_state::RuntimeState::trace_roots`] before
    /// handing them to [`otter_gc::GcHeap::collect_full`] — so
    /// the same `&mut self` borrow can satisfy both the heap
    /// (mutably) and the root walker (immutably) without
    /// resorting to unsafe split-borrow tricks.
    ///
    /// **Debug / test only** — production embedders let the GC
    /// trigger itself.
    pub fn force_gc(&mut self) {
        let mut roots: Vec<*mut RawGc> = Vec::new();
        {
            let state = crate::runtime_state::RuntimeState::new(self);
            state.trace_roots(&mut |slot| roots.push(slot));
        }
        let mut visit = |sv: &mut dyn FnMut(*mut RawGc)| {
            for &p in &roots {
                sv(p);
            }
        };
        self.gc_heap.mark_phase(&mut visit);
        crate::collections::run_ephemeron_fixpoint(&mut self.gc_heap);
        let finalization_jobs =
            crate::weak_refs::process_weak_refs_and_finalizers(&mut self.gc_heap);
        for job in finalization_jobs {
            let mut args = SmallVec::new();
            args.push(job.held_value);
            self.microtasks.enqueue(Microtask {
                callee: job.cleanup_callback,
                this_value: Value::Undefined,
                args,
                context: job.context,
                result_capability: None,
                kind: MicrotaskKind::FinalizationCallback,
            });
        }
        self.gc_heap.sweep_phase();
    }

    /// Execute `<main>` of `module` and return its completion value.
    ///
    /// # Errors
    /// Returns [`RunError`] (a `VmError` plus a stack-frame
    /// snapshot) on bytecode malformation, type mismatch, OOM,
    /// interrupt, or stack overflow.
    pub fn run(&mut self, context: &ExecutionContext) -> Result<Value, RunError> {
        self.pending_uncaught_throw = None;
        self.pending_uncaught_frames = None;
        self.ensure_property_ic_capacity(context);
        match self.run_inner(context) {
            Ok(v) => Ok(v),
            Err((error, frames)) => Err(RunError { error, frames }),
        }
    }

    /// Drain the microtask queue until empty (or
    /// [`microtask::MAX_DRAIN_ITERS`] is hit).
    ///
    /// Each task is executed by invoking its callee with `this`
    /// and `args` set up at enqueue time. Tasks pushed during the
    /// drain go on the **next** generation, mirroring V8 / JSC.
    ///
    /// Foundation exception policy: the **first** error wins.
    /// The remaining queue is left in place so a follow-up
    /// `drain_microtasks` after the embedder recovers picks up
    /// where this drain stopped. Once the `Promise` constructor
    /// lands (task 34), this flips to spec semantics ("rejected
    /// promise, continue draining").
    pub fn drain_microtasks(&mut self, context: &ExecutionContext) -> Result<(), RunError> {
        self.drain_microtasks_with_default(Some(context.clone()))
    }

    /// Drain queued microtasks using each task's origin context,
    /// falling back to the caller-supplied context for jobs created
    /// inside the same VM turn. Host-settlement paths pass `None`
    /// so missing task origin is reported as an engine error.
    pub fn drain_microtasks_with_default(
        &mut self,
        default_context: Option<ExecutionContext>,
    ) -> Result<(), RunError> {
        self.record_runtime_microtask_drain_started();
        let mut iters: u32 = 0;
        let mut observed_microtask_budget = false;
        loop {
            let Some(batch) = self.microtasks.begin_drain() else {
                return Ok(());
            };
            if batch.tasks.is_empty() {
                self.microtasks.end_drain();
                return Ok(());
            }
            for task in batch.tasks {
                if iters >= microtask::MAX_DRAIN_ITERS {
                    self.microtasks.end_drain();
                    return Err(RunError {
                        error: VmError::JsonError {
                            // Reusing the structured-error channel
                            // until task 34 introduces a real
                            // microtask-error code.
                            code: "MICROTASK_RUNAWAY",
                            message: format!(
                                "microtask drain exceeded {} iterations",
                                microtask::MAX_DRAIN_ITERS
                            ),
                        },
                        frames: Vec::new(),
                    });
                }
                iters += 1;
                self.record_runtime_microtask_executed();
                if !observed_microtask_budget {
                    observed_microtask_budget =
                        self.observe_runtime_microtask_budget(u64::from(iters));
                    if observed_microtask_budget && self.runtime_budget.rejects_on_exceedance() {
                        self.runtime_budget_stats.record_budget_rejection();
                        self.microtasks.end_drain();
                        return Err(RunError {
                            error: VmError::BudgetExceeded {
                                message: "runtime microtask budget exceeded".to_string(),
                            },
                            frames: Vec::new(),
                        });
                    }
                }
                let context = task.context.clone().or_else(|| default_context.clone());
                let Some(context) = context else {
                    self.microtasks.end_drain();
                    return Err(RunError {
                        error: VmError::InvalidOperand,
                        frames: Vec::new(),
                    });
                };
                if let Err(err) = self.invoke_microtask(&context, task) {
                    self.microtasks.end_drain();
                    return Err(err);
                }
            }
            self.microtasks.end_drain();
            // Loop continues: any tasks pushed during this
            // generation get picked up by the next `begin_drain`.
            if !self.microtasks.has_any_pending() {
                return Ok(());
            }
        }
    }

    /// Invoke one microtask top-level. Builds a fresh frame stack
    /// containing just the task's callee; runs `dispatch_loop`
    /// until it returns. Errors include the snapshot of frames
    /// the task accumulated when it failed.
    fn invoke_microtask(
        &mut self,
        context: &ExecutionContext,
        task: Microtask,
    ) -> Result<(), RunError> {
        // Reaction-mode rejection forwarding (§27.2.1.3.2) reads the
        // abrupt completion's [[Value]] from `pending_uncaught_throw`
        // after `dispatch_loop` returns. Clear any stale payload
        // carried over from a prior microtask so we cannot read a
        // foreign reaction's value into this one.
        self.pending_uncaught_throw = None;
        // Async-resume tasks bypass callee resolution entirely:
        // the parked frame replaces a fresh callee invocation,
        // so route them to `run_async_resume` directly.
        if let MicrotaskKind::AsyncResume {
            frame,
            await_dst,
            fulfilled,
        } = task.kind
        {
            let value = task.args.into_iter().next().unwrap_or(Value::Undefined);
            return self.run_async_resume(context, frame, await_dst, fulfilled, value);
        }
        if let MicrotaskKind::AsyncGenResume {
            frame,
            await_dst,
            fulfilled,
            owner,
        } = task.kind
        {
            let value = task.args.into_iter().next().unwrap_or(Value::Undefined);
            return self.run_async_gen_resume(context, frame, await_dst, fulfilled, value, owner);
        }
        // Resolve callee → function_id + upvalues. Mirrors the
        // unwrap loop inside `invoke`, but for a top-level call
        // (no caller frame to write back into).
        let result_capability = task.result_capability.clone();
        let mut current = task.callee;
        let mut effective_this = task.this_value;
        let mut effective_args: SmallVec<[Value; 8]> = task.args.into_iter().collect();
        let mut hops: u32 = 0;
        loop {
            if hops >= self.max_stack_depth {
                return Err(RunError {
                    error: VmError::StackOverflow {
                        limit: self.max_stack_depth,
                    },
                    frames: Vec::new(),
                });
            }
            match current {
                Value::BoundFunction(bound) => {
                    hops += 1;
                    let (target, bound_this, bound_args) = bound.parts(&self.gc_heap);
                    let mut combined: SmallVec<[Value; 8]> =
                        SmallVec::with_capacity(bound_args.len() + effective_args.len());
                    combined.extend(bound_args);
                    combined.extend(effective_args);
                    effective_this = bound_this;
                    effective_args = combined;
                    current = target;
                }
                Value::ClassConstructor(cc) => {
                    hops += 1;
                    current = cc.ctor(&self.gc_heap).clone();
                }
                _ => break,
            }
        }
        // Native callables run inline at the drain site: no frame
        // push, no return register. Errors propagate as RunError.
        if let Value::NativeFunction(native) = &current {
            let call = native.call_target(&self.gc_heap);
            if let crate::native_function::NativeCallTarget::VmIntrinsic(intrinsic) = call {
                return match self.run_vm_intrinsic_sync(
                    context,
                    intrinsic,
                    effective_this,
                    effective_args,
                ) {
                    Ok(value) => {
                        self.settle_microtask_capability(context, result_capability, Ok(value));
                        Ok(())
                    }
                    Err(vm_err) => {
                        if result_capability.is_some() {
                            let reason = vm_err_to_value(&vm_err);
                            self.settle_microtask_capability(
                                context,
                                result_capability,
                                Err(reason),
                            );
                            Ok(())
                        } else {
                            Err(RunError {
                                error: vm_err,
                                frames: Vec::new(),
                            })
                        }
                    }
                };
            }
            let call_info = NativeCallInfo::call(effective_this.clone());
            self.record_runtime_native_call();
            let mut ctx =
                NativeCtx::new_with_call_info_and_context(self, call_info, Some(context.clone()));
            return match call.invoke(&mut ctx, effective_args.as_slice()) {
                Ok(value) => {
                    self.settle_microtask_capability(context, result_capability, Ok(value));
                    Ok(())
                }
                Err(err) => {
                    let vm_err = native_to_vm_error(err);
                    if result_capability.is_some() {
                        // Reaction-mode: route the error into the
                        // downstream promise as a rejection rather
                        // than aborting the drain.
                        let reason = vm_err_to_value(&vm_err);
                        self.settle_microtask_capability(context, result_capability, Err(reason));
                        Ok(())
                    } else {
                        Err(RunError {
                            error: vm_err,
                            frames: Vec::new(),
                        })
                    }
                }
            };
        }
        let (function_id, parent_upvalues, this_for_callee) =
            match Self::bytecode_call_target_parts(current, effective_this) {
                Ok(parts) => parts,
                Err(error) => {
                    return Err(RunError {
                        error,
                        frames: Vec::new(),
                    });
                }
            };
        let function = match context.exec_function(function_id) {
            Some(f) => f,
            None => {
                return Err(RunError {
                    error: VmError::InvalidOperand,
                    frames: Vec::new(),
                });
            }
        };
        let upvalues =
            match Frame::build_upvalues_for_exec(&mut self.gc_heap, function, parent_upvalues) {
                Ok(u) => u,
                Err(oom) => {
                    return Err(RunError {
                        error: VmError::from(oom),
                        frames: Vec::new(),
                    });
                }
            };
        let this_for_callee = match self.this_for_bytecode_call_runtime_rooted(
            function,
            this_for_callee,
            &[effective_args.as_slice()],
        ) {
            Ok(value) => value,
            Err(error) => {
                return Err(RunError {
                    error,
                    frames: Vec::new(),
                });
            }
        };
        let mut new_frame = Frame::with_exec_return_upvalues_and_this(
            function,
            None, // top-level — no return register
            upvalues,
            this_for_callee,
        );
        Self::bind_bytecode_call_arguments(function, &mut new_frame, effective_args).map_err(
            |error| RunError {
                error,
                frames: Vec::new(),
            },
        )?;
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(new_frame);
        match self.dispatch_loop(context, &mut stack) {
            Ok(value) => {
                // Reaction job: settle the downstream promise with
                // the handler's return value (spec §27.2.5.4).
                self.settle_microtask_capability(context, result_capability, Ok(value));
                Ok(())
            }
            Err(error) => {
                if result_capability.is_some() {
                    // Reaction-mode unwind: route the abrupt
                    // completion's [[Value]] into the downstream
                    // promise as a rejection per ECMA-262
                    // §27.2.1.3.2 PromiseReactionJob step 1.f.iii.
                    // Spec requires the *original* thrown value, not
                    // a stringified `VmError::Uncaught` rendering;
                    // [`Self::unwind_throw_with_uncaught`] preserves
                    // it on `pending_uncaught_throw` for exactly this
                    // hop.
                    let reason = self
                        .pending_uncaught_throw
                        .take()
                        .unwrap_or_else(|| vm_err_to_value(&error));
                    self.settle_microtask_capability(context, result_capability, Err(reason));
                    Ok(())
                } else {
                    let frames = snapshot_frames(context, &stack);
                    Err(RunError { error, frames })
                }
            }
        }
    }

    /// Resolve / reject the downstream promise that a reaction
    /// job belongs to. No-op when `cap` is `None` (plain
    /// `queueMicrotask` callbacks).
    fn settle_microtask_capability(
        &mut self,
        context: &ExecutionContext,
        cap: Option<microtask::MicrotaskCapability>,
        outcome: Result<Value, Value>,
    ) {
        let Some(cap) = cap else {
            return;
        };
        let (callee, args): (Value, SmallVec<[Value; 4]>) = match outcome {
            Ok(v) => (cap.resolve, smallvec::smallvec![v]),
            Err(reason) => (cap.reject, smallvec::smallvec![reason]),
        };
        // Settling enqueues another microtask so the resolve/
        // reject native runs in a fresh job (matches spec
        // ordering — the next reaction picks it up on the next
        // generation).
        self.microtasks.enqueue(Microtask {
            callee,
            this_value: Value::Undefined,
            args,
            context: Some(context.clone()),
            result_capability: None,
            kind: microtask::MicrotaskKind::Call,
        });
    }

    /// Internal driver. Pulls the snapshot capture out of the
    /// dispatch loop so the hot path remains allocation-free; the
    /// snapshot is built only when a `VmError` actually escapes.
    fn run_inner(
        &mut self,
        context: &ExecutionContext,
    ) -> Result<Value, (VmError, Vec<StackFrameSnapshot>)> {
        let main = context.exec_main();
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let upvalues =
            Frame::build_upvalues_for_exec(&mut self.gc_heap, main, Frame::empty_upvalues())
                .map_err(|oom| (VmError::from(oom), Vec::new()))?;
        let entry_this = if main.is_module || main.is_strict {
            Value::Undefined
        } else {
            Value::Object(self.global_this)
        };
        let entry = Frame::with_exec_return_upvalues_and_this(main, None, upvalues, entry_this);
        let entry_is_async = main.is_async;
        stack.push(entry);
        // §16.2.1.7 ModuleDeclarationInstantiation step 5 — when the
        // entry function carries top-level await, wire up an async
        // result promise so `Op::Await` can park / resume normally.
        // The dispatch loop's exit returns the result promise's
        // resolved value once microtasks drain.
        let entry_promise = if entry_is_async {
            let result = promise_dispatch::PromiseBuilder::with_context(context.clone())
                .pending_stack_rooted(self, &stack, &[], &[])
                .map_err(|oom| (VmError::from(oom), Vec::new()))?;
            stack
                .last_mut()
                .expect("entry frame was just pushed")
                .async_state = Some(AsyncFrameState {
                result_promise: result,
            });
            Some(result)
        } else {
            None
        };

        let dispatch_result = self.dispatch_loop(context, &mut stack);
        match dispatch_result {
            Ok(value) => {
                if let Some(promise) = entry_promise {
                    // Drain microtasks until the entry promise
                    // settles. The settled value (or rejection)
                    // becomes the program's completion value.
                    if let Err(err) = self.drain_microtasks_with_default(Some(context.clone())) {
                        return Err((err.error, err.frames));
                    }
                    match promise.state(&self.gc_heap) {
                        crate::promise::PromiseState::Fulfilled(v) => return Ok(v),
                        crate::promise::PromiseState::Rejected(reason) => {
                            return Err((
                                VmError::Uncaught {
                                    value: render_thrown_value(&reason, &self.gc_heap),
                                },
                                Vec::new(),
                            ));
                        }
                        crate::promise::PromiseState::Pending => return Ok(Value::Undefined),
                    }
                }
                Ok(value)
            }
            Err(err) => {
                let frames = self
                    .pending_uncaught_frames
                    .take()
                    .unwrap_or_else(|| snapshot_frames(context, &stack));
                Err((err, frames))
            }
        }
    }

    /// Drive the dispatch loop, converting convertible `VmError`
    /// variants (TypeMismatch, NotCallable, TemporalDeadZone,
    /// OutOfMemory, etc.)
    /// into typed `Error` instances that flow through `unwind_throw`
    /// — so user code can `try { … } catch (e) { e instanceof
    /// TypeError }` and observe the same shape it would in any
    /// spec-conforming engine. Variants that aren't user-recoverable
    /// (StackOverflow, Interrupted, Uncaught, MissingReturn,
    /// InvalidOperand) propagate as-is.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-error-objects>
    /// - <https://tc39.es/ecma262/#sec-native-error-types-used-in-this-standard>
    fn dispatch_loop(
        &mut self,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
    ) -> Result<Value, VmError> {
        self.ensure_property_ic_capacity(context);
        self.begin_runtime_budget_turn();
        let result = loop {
            match self.dispatch_loop_inner(context, stack) {
                Ok(value) => break Ok(value),
                Err(err) => {
                    if matches!(err, VmError::Uncaught { .. })
                        && !stack.is_empty()
                        && let Some(thrown) = self.pending_uncaught_throw.take()
                    {
                        self.pending_uncaught_frames = Some(snapshot_frames(context, stack));
                        let unwind = self.unwind_throw(stack, thrown);
                        if unwind.is_ok() {
                            self.pending_uncaught_frames = None;
                        }
                        unwind?;
                        if stack.is_empty() {
                            break Ok(Value::Undefined);
                        }
                        continue;
                    }
                    if let Some(thrown) = self.vm_error_to_throwable_with_stack_roots(stack, &err) {
                        let uncaught = if matches!(err, VmError::OutOfMemory { .. }) {
                            Some(err.clone())
                        } else {
                            None
                        };
                        self.pending_uncaught_frames = Some(snapshot_frames(context, stack));
                        let unwind = self.unwind_throw_with_uncaught(stack, thrown, uncaught);
                        if unwind.is_ok() {
                            self.pending_uncaught_frames = None;
                        }
                        unwind?;
                        if stack.is_empty() {
                            break Ok(Value::Undefined);
                        }
                        continue;
                    }
                    break Err(err);
                }
            }
        };
        self.finish_runtime_budget_turn();
        result
    }

    fn dispatch_loop_inner(
        &mut self,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
    ) -> Result<Value, VmError> {
        loop {
            if self.interrupt.is_set() {
                return Err(VmError::Interrupted);
            }
            if stack.is_empty() {
                // Defensive: unwind paths (throw / finally) can
                // pop the last frame without writing back to a
                // caller register. Surface `Value::Undefined` so
                // the dispatch loop terminates cleanly instead of
                // panicking on the next `stack.len() - 1`. Tests
                // that rely on the throw escape will already have
                // flowed through `unwind_throw` and surfaced as
                // `VmError::Uncaught`; this guard catches the
                // residual "fell off the bottom" path and treats
                // it as completion.
                return Ok(Value::Undefined);
            }
            let top_idx = stack.len() - 1;
            let function_id = stack[top_idx].function_id;
            let function = context
                .exec_function(function_id)
                .ok_or(VmError::InvalidOperand)?;
            let pc = stack[top_idx].pc;
            let instr = function
                .code
                .get(pc as usize)
                .ok_or(VmError::MissingReturn)?;
            let op = instr.op();
            self.record_runtime_reductions(runtime_budget::opcode_reductions(op));
            self.enforce_runtime_budget_checkpoint()?;
            self.observe_runtime_stack_depth(stack.len());

            // Stack-modifying opcodes go first so we don't hold a
            // `&mut Frame` borrow while pushing / popping.
            match op {
                Op::ReturnValue | Op::Return => {
                    let src = register_operand(context.exec_operand(instr, 0))?;
                    let value = stack[top_idx]
                        .registers
                        .get(src as usize)
                        .cloned()
                        .ok_or(VmError::InvalidOperand)?;
                    if let Some(popped) = self.pop_frame(stack, value)? {
                        return Ok(popped);
                    }
                    continue;
                }
                Op::ReturnUndefined => {
                    if let Some(popped) = self.pop_frame(stack, Value::Undefined)? {
                        return Ok(popped);
                    }
                    continue;
                }
                Op::Call => {
                    let operands = context.exec_operands(instr);
                    self.do_call(stack, context, operands)?;
                    continue;
                }
                Op::CallWithThis => {
                    let operands = context.exec_operands(instr);
                    self.do_call_with_this(stack, context, operands)?;
                    continue;
                }
                Op::CallMethodValue => {
                    let operands = context.exec_operands(instr);
                    self.do_call_method_value(stack, context, operands)?;
                    continue;
                }
                Op::CallSpread => {
                    let operands = context.exec_operands(instr);
                    self.do_call_spread(stack, context, operands)?;
                    continue;
                }
                Op::New => {
                    let operands = context.exec_operands(instr);
                    self.do_construct(stack, context, operands)?;
                    continue;
                }
                Op::NewSpread => {
                    let operands = context.exec_operands(instr);
                    self.do_construct_spread(stack, context, operands)?;
                    continue;
                }
                Op::Throw => {
                    let src = register_operand(context.exec_operand(instr, 0))?;
                    let value = stack[top_idx]
                        .registers
                        .get(src as usize)
                        .cloned()
                        .ok_or(VmError::InvalidOperand)?;
                    // Capture frames at the originating throw site
                    // before `unwind_throw` pops handler-less
                    // frames. If a catch absorbs the throw the
                    // unwind path clears `pending_uncaught_frames`
                    // through [`Self::clear_pending_uncaught_frames`].
                    self.pending_uncaught_frames = Some(snapshot_frames(context, stack));
                    let unwind = self.unwind_throw(stack, value);
                    if unwind.is_ok() {
                        self.pending_uncaught_frames = None;
                    }
                    unwind?;
                    continue;
                }
                Op::EndFinally => {
                    if let Some(value) = stack[top_idx].pending_throw.take() {
                        self.pending_uncaught_frames = Some(snapshot_frames(context, stack));
                        let unwind = self.unwind_throw(stack, value);
                        if unwind.is_ok() {
                            self.pending_uncaught_frames = None;
                        }
                        unwind?;
                    } else {
                        stack[top_idx].pc = stack[top_idx]
                            .pc
                            .checked_add(1)
                            .ok_or(VmError::InvalidOperand)?;
                    }
                    continue;
                }
                Op::Await => {
                    let dst = register_operand(context.exec_operand(instr, 0))?;
                    let src = register_operand(context.exec_operand(instr, 1))?;
                    let awaited = read_register(&stack[top_idx], src)?.clone();
                    self.do_await(stack, context, dst, awaited)?;
                    if stack.is_empty() {
                        return Ok(Value::Undefined);
                    }
                    continue;
                }
                // §27.5 generator suspension. Yield reads the value
                // operand, advances pc past itself, pops the frame
                // off the active stack, stashes it back onto the
                // owning [`crate::generator::JsGenerator`], records
                // the dst register so a future `.next(arg)` can
                // deposit `arg` there, and returns control to the
                // resume site (i.e. the enclosing
                // [`Self::resume_generator`] call).
                // <https://tc39.es/ecma262/#sec-yield>
                Op::Yield => {
                    let dst = register_operand(context.exec_operand(instr, 0))?;
                    let src = register_operand(context.exec_operand(instr, 1))?;
                    let yielded = read_register(&stack[top_idx], src)?.clone();
                    let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                    let owner = frame.generator_owner.ok_or(VmError::TypeMismatch)?;
                    frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                    let popped = stack.pop().expect("frame present");
                    owner.park_after_yield(&mut self.gc_heap, popped, dst, yielded.clone());
                    let pending_request = if owner.is_async(&self.gc_heap) {
                        owner.take_pending_request(&mut self.gc_heap)
                    } else {
                        None
                    };
                    // §27.6 — async-generator yield settles the
                    // outer `.next()` promise immediately with
                    // `{value, done: false}`. Sync generators bubble
                    // the yielded value out so the `resume_generator`
                    // caller can shape it.
                    if let Some(cap) = pending_request {
                        let record = self.make_runtime_rooted_iter_result(
                            yielded.clone(),
                            false,
                            &[&cap.resolve],
                            &[],
                        )?;
                        let capability_context =
                            cap.context.clone().unwrap_or_else(|| context.clone());
                        self.run_callable_sync(
                            &capability_context,
                            &cap.resolve,
                            Value::Undefined,
                            smallvec::smallvec![record],
                        )?;
                    }
                    return Ok(yielded);
                }
                // ToNumber on an object whose `[Symbol.toPrimitive]`
                // is callable must invoke that hook (ECMA-262
                // §7.1.1 OrdinaryToPrimitive). The synchronous path
                // pushes a frame, so the dispatch happens here —
                // outside the in-frame mutable borrow below.
                Op::ToNumber => {
                    let operands = context.exec_operands(instr);
                    if let Some(()) = self.try_to_primitive_dispatch(stack, context, operands)? {
                        continue;
                    }
                }
                // §7.1.1 `ToPrimitive` ladder. Each invocation of
                // the dispatch loop either advances pc with a
                // primitive in `dst` or pushes a frame for
                // `[Symbol.toPrimitive]` / `valueOf` / `toString`
                // and parks the ladder state on the running frame.
                // Stack-modifying so it has to happen before the
                // in-frame mutable borrow below. Always re-enters
                // the dispatch loop afterwards — the in-frame
                // match below has no arm for `Op::ToPrimitive`.
                Op::ToPrimitive => {
                    let operands = context.exec_operands(instr);
                    self.drive_to_primitive(stack, context, operands)?;
                    continue;
                }
                // §7.4.3 `GetIterator`. Built-in iterables fall
                // through to the in-frame fast path; user objects
                // route through the call-frame ladder.
                // <https://tc39.es/ecma262/#sec-getiterator>
                Op::GetIterator => {
                    let operands = context.exec_operands(instr);
                    if self.drive_get_iterator(stack, context, operands)? {
                        continue;
                    }
                }
                // §7.4.5 `IteratorNext`. Built-in iterators step
                // synchronously; user iterators push a call to
                // `iter.next()` and resume to extract `value` /
                // `done`.
                // <https://tc39.es/ecma262/#sec-iteratornext>
                Op::IteratorNext => {
                    let operands = context.exec_operands(instr);
                    if self.drive_iterator_next(stack, context, operands)? {
                        continue;
                    }
                }
                // §10.1.8 [[Get]] — when the resolved property is an
                // accessor descriptor at any depth in the prototype
                // chain, the runtime invokes the getter with `this`
                // bound to the original receiver. Stack-modifying so
                // it must run outside the in-frame mutable borrow
                // below.
                // <https://tc39.es/ecma262/#sec-ordinaryget>
                Op::LoadProperty => {
                    let operands = context.exec_operands(instr);
                    if self.drive_load_property(stack, context, operands)? {
                        continue;
                    }
                }
                Op::LoadElement => {
                    let operands = context.exec_operands(instr);
                    if self.drive_load_element(stack, context, operands)? {
                        continue;
                    }
                }
                // §10.1.9 [[Set]] — accessor setter dispatch follows
                // the same pattern as `LoadProperty`. Non-writable
                // and non-extensible rejections surface here too.
                // <https://tc39.es/ecma262/#sec-ordinaryset>
                Op::StoreProperty => {
                    let operands = context.exec_operands(instr);
                    if self.drive_store_property(stack, context, operands)? {
                        continue;
                    }
                }
                Op::StoreElement => {
                    let operands = context.exec_operands(instr);
                    if self.drive_store_element(stack, context, operands)? {
                        continue;
                    }
                }
                Op::Instanceof => {
                    let operands = context.exec_operands(instr);
                    if self.drive_instanceof(stack, context, operands)? {
                        continue;
                    }
                }
                // §28.2.4.7 / .10 Proxy.[[HasProperty]] /
                // [[Delete]] — invoke `has` / `deleteProperty`
                // traps when the receiver is a Proxy.
                Op::HasProperty => {
                    let operands = context.exec_operands(instr);
                    if self.drive_has_property_proxy(stack, context, operands)? {
                        continue;
                    }
                }
                Op::DeleteProperty => {
                    let operands = context.exec_operands(instr);
                    if self.drive_delete_property_proxy(stack, context, operands)? {
                        continue;
                    }
                }
                Op::DeleteElement => {
                    let operands = context.exec_operands(instr);
                    if self.drive_delete_element_proxy(stack, context, operands)? {
                        continue;
                    }
                }
                // §28.2.4.1 / .2 Proxy.[[GetPrototypeOf]] /
                // [[SetPrototypeOf]] — invoke `getPrototypeOf` /
                // `setPrototypeOf` traps when the receiver is a
                // Proxy.
                Op::GetPrototype => {
                    let operands = context.exec_operands(instr);
                    if self.drive_get_prototype_proxy(stack, context, operands)? {
                        continue;
                    }
                }
                Op::SetPrototype => {
                    let operands = context.exec_operands(instr);
                    if self.drive_set_prototype_proxy(stack, context, operands)? {
                        continue;
                    }
                }
                // §19.4.1 indirect eval — recursively dispatches a
                // freshly compiled module on a sub-stack, then
                // writes the completion value into `dst`. Stack-
                // modifying so it has to run before the in-frame
                // borrow below.
                Op::Eval => {
                    let operands = context.exec_operands(instr);
                    self.run_eval_operands(context, stack, operands)?;
                    continue;
                }
                // §20.2.1.1 — `new Function(args, body)` recurses
                // into the eval hook with a synthesised wrapper.
                Op::NewFunction => {
                    let operands = context.exec_operands(instr);
                    self.run_new_function_operands(context, stack, operands)?;
                    continue;
                }
                Op::CollectArguments => {
                    // §10.4.4 Arguments exotic objects. This path
                    // runs before the in-frame borrow so we can look
                    // up realm intrinsics and allocate the
                    // descriptor-backed arguments object.
                    let dst = register_operand(context.exec_operand(instr, 0))?;
                    let (elements, kind, mapped_entries, callee) = {
                        let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                        let function = context
                            .exec_function(frame.function_id)
                            .ok_or(VmError::InvalidOperand)?;
                        let elements = std::mem::take(&mut frame.incoming_args);
                        let mapped_entries = if function.arguments_object_kind
                            == ArgumentsObjectKind::Mapped
                        {
                            function
                                .mapped_argument_bindings
                                .iter()
                                .filter_map(|binding| {
                                    if binding.argument_index as usize >= elements.len() {
                                        return None;
                                    }
                                    let ArgumentBindingStorage::Upvalue { idx } = binding.storage
                                    else {
                                        return None;
                                    };
                                    let cell = *frame.upvalues.get(idx as usize)?;
                                    Some(crate::object::MappedArgumentEntry {
                                        key: binding.argument_index.to_string(),
                                        cell,
                                    })
                                })
                                .collect()
                        } else {
                            Vec::new()
                        };
                        let callee = Value::Function {
                            function_id: frame.function_id,
                        };
                        (
                            elements,
                            function.arguments_object_kind,
                            mapped_entries,
                            callee,
                        )
                    };
                    let obj = if kind == ArgumentsObjectKind::Mapped {
                        let obj = self.alloc_stack_rooted_object_with_value_roots(
                            stack,
                            &[&callee],
                            &elements,
                        )?;
                        crate::arguments_object::initialize_mapped(
                            obj,
                            &mut self.gc_heap,
                            elements,
                            callee,
                            mapped_entries,
                        );
                        obj
                    } else {
                        let thrower = self.restricted_throw_type_error()?;
                        let obj = self.alloc_stack_rooted_object_with_value_roots(
                            stack,
                            &[&thrower],
                            &elements,
                        )?;
                        crate::arguments_object::initialize_unmapped(
                            obj,
                            &mut self.gc_heap,
                            elements,
                            thrower,
                        );
                        obj
                    };
                    let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                    write_register(frame, dst, Value::Object(obj))?;
                    frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                    continue;
                }
                _ => {}
            }

            match op {
                Op::Nop => {
                    stack[top_idx].pc += 1;
                    continue;
                }
                Op::LoadUndefined => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::Undefined)?;
                    frame.pc += 1;
                    continue;
                }
                Op::LoadHole => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::Hole)?;
                    frame.pc += 1;
                    continue;
                }
                Op::LoadTrue => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::Boolean(true))?;
                    frame.pc += 1;
                    continue;
                }
                Op::LoadFalse => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::Boolean(false))?;
                    frame.pc += 1;
                    continue;
                }
                Op::LoadNull => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::Null)?;
                    frame.pc += 1;
                    continue;
                }
                Op::LoadInt32 => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let imm = context
                        .exec_imm32(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::Number(NumberValue::Smi(imm)))?;
                    frame.pc += 1;
                    continue;
                }
                Op::LoadNumber => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let bits = context
                        .number_constant_bits(idx)
                        .ok_or(VmError::InvalidOperand)?;
                    let value = NumberValue::from_f64(f64::from_bits(bits));
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::Number(value))?;
                    frame.pc += 1;
                    continue;
                }
                Op::LoadString => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let units = context
                        .string_constant_units(idx)
                        .ok_or(VmError::InvalidOperand)?;
                    let s = JsString::from_utf16_units(units, &self.string_heap)?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::String(s))?;
                    frame.pc += 1;
                    continue;
                }
                Op::LoadLength => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    let s = read_register(frame, src)?
                        .as_string()
                        .ok_or(VmError::TypeMismatch)?;
                    let len = NumberValue::from_i32(s.len() as i32);
                    write_register(frame, dst, Value::Number(len))?;
                    frame.pc += 1;
                    continue;
                }
                Op::LogicalNot => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    let truthy = read_register(frame, src)?.to_boolean();
                    write_register(frame, dst, Value::Boolean(!truthy))?;
                    frame.pc += 1;
                    continue;
                }
                Op::ToBoolean => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    let truthy = read_register(frame, src)?.to_boolean();
                    write_register(frame, dst, Value::Boolean(truthy))?;
                    frame.pc += 1;
                    continue;
                }
                Op::ToNumber => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_to_number_regs(frame, dst, src)?;
                    continue;
                }
                Op::GetStringIndex => {
                    let (dst, recv, idx) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_get_string_index_regs(frame, dst, recv, idx)?;
                    continue;
                }
                Op::TypeOf => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_typeof_regs(frame, dst, src)?;
                    continue;
                }
                Op::LoadThis => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_load_this_reg(frame, dst)?;
                    continue;
                }
                Op::LoadNewTarget => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_load_new_target_reg(frame, dst)?;
                    continue;
                }
                Op::DeleteProperty => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let obj_reg = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 2)
                        .ok_or(VmError::InvalidOperand)?;
                    let key = context
                        .property_atom(name_idx)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_delete_property_reg(frame, dst, obj_reg, key)?;
                    continue;
                }
                Op::DeleteElement => {
                    let (dst, obj_reg, idx_reg) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_delete_element_regs(frame, dst, obj_reg, idx_reg)?;
                    continue;
                }
                Op::GetPrototype => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_get_prototype_regs(frame, dst, src)?;
                    continue;
                }
                Op::SetPrototype => {
                    let obj_reg = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let proto_reg = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_set_prototype_regs(context, frame, obj_reg, proto_reg)?;
                    continue;
                }
                Op::NewObject => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    self.run_new_object_reg(&mut *stack, top_idx, dst)?;
                    continue;
                }
                Op::NewArray => {
                    let operands = context.exec_operands(instr);
                    self.run_new_array_operands(&mut *stack, top_idx, operands)?;
                    continue;
                }
                Op::LoadRegExp => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_load_regexp_reg(context, frame, dst, idx)?;
                    continue;
                }
                Op::LoadBigInt => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_load_bigint_reg(context, frame, dst, idx)?;
                    continue;
                }
                Op::LoadUpvalue => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let idx = context
                        .exec_imm32(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_load_upvalue_reg(frame, dst, idx)?;
                    continue;
                }
                Op::StoreUpvalue => {
                    let src = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let idx = context
                        .exec_imm32(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_store_upvalue_reg(frame, src, idx)?;
                    continue;
                }
                Op::CollectRest => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    self.run_collect_rest_reg(&mut *stack, top_idx, dst)?;
                    continue;
                }
                Op::CollectArguments => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_collect_arguments_reg(frame, dst)?;
                    continue;
                }
                Op::LoadProperty => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let obj_reg = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 2)
                        .ok_or(VmError::InvalidOperand)?;
                    let key = context
                        .property_atom(name_idx)
                        .ok_or(VmError::InvalidOperand)?;
                    self.run_load_property_reg(context, &mut *stack, top_idx, dst, obj_reg, key)?;
                    continue;
                }
                Op::StoreProperty => {
                    let obj_reg = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 2)
                        .ok_or(VmError::InvalidOperand)?;
                    let key = context
                        .property_atom(name_idx)
                        .ok_or(VmError::InvalidOperand)?;
                    self.run_store_property_reg(context, &mut *stack, top_idx, obj_reg, key, src)?;
                    continue;
                }
                Op::LoadElement => {
                    let (dst, recv_reg, idx_reg) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_load_element_regs(context, frame, dst, recv_reg, idx_reg)?;
                    continue;
                }
                Op::StoreElement => {
                    let recv_reg = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let idx_reg = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let src_reg = context
                        .exec_register(instr, 2)
                        .ok_or(VmError::InvalidOperand)?;
                    self.run_store_element_regs(
                        context,
                        &mut *stack,
                        top_idx,
                        recv_reg,
                        idx_reg,
                        src_reg,
                    )?;
                    continue;
                }
                Op::GetIterator => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    self.run_get_iterator_regs(&mut *stack, top_idx, dst, src)?;
                    continue;
                }
                Op::IteratorNext => {
                    let value_dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let done_dst = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let iter_reg = context
                        .exec_register(instr, 2)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_iterator_next_regs(frame, value_dst, done_dst, iter_reg)?;
                    continue;
                }
                Op::MakeFunction => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_make_function_reg(context, frame, dst, idx)?;
                    continue;
                }
                Op::MakeClass => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let ctor_reg = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let proto_reg = context
                        .exec_register(instr, 2)
                        .ok_or(VmError::InvalidOperand)?;
                    let statics_reg = context
                        .exec_register(instr, 3)
                        .ok_or(VmError::InvalidOperand)?;
                    self.run_make_class_regs(
                        &mut *stack,
                        top_idx,
                        dst,
                        ctor_reg,
                        proto_reg,
                        statics_reg,
                    )?;
                    continue;
                }
                Op::NewError => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let msg_reg = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    self.run_new_error_regs(&mut *stack, top_idx, dst, msg_reg)?;
                    continue;
                }
                Op::NewBuiltinError => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let kind_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let msg_reg = context
                        .exec_register(instr, 2)
                        .ok_or(VmError::InvalidOperand)?;
                    self.run_new_builtin_error_regs(
                        context,
                        &mut *stack,
                        top_idx,
                        dst,
                        kind_idx,
                        msg_reg,
                    )?;
                    continue;
                }
                Op::LoadBuiltinError => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let kind_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_load_builtin_error_reg(context, frame, dst, kind_idx)?;
                    continue;
                }
                Op::LoadGlobalThis => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_load_global_this_reg(frame, dst)?;
                    continue;
                }
                Op::LoadGlobalOrThrow => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_load_global_or_throw_reg(context, frame, dst, name_idx)?;
                    continue;
                }
                Op::LoadGlobalOrUndefined => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_load_global_or_undefined_reg(context, frame, dst, name_idx)?;
                    continue;
                }
                Op::ImportNamespace => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let spec_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_import_namespace_reg(context, frame, dst, spec_idx)?;
                    continue;
                }
                Op::ImportMetaResolve => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let spec_reg = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_import_meta_resolve_regs(frame, dst, spec_reg)?;
                    continue;
                }
                Op::PromiseFulfilledOf => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    self.run_promise_fulfilled_of_regs(context, stack, top_idx, dst, src)?;
                    continue;
                }
                Op::ArrayPush => {
                    let arr_reg = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let value_reg = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    self.run_array_push_regs(&mut *stack, top_idx, arr_reg, value_reg)?;
                    continue;
                }
                Op::NewWeakRef => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let target_reg = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    self.run_new_weak_ref_regs(&mut *stack, top_idx, dst, target_reg)?;
                    continue;
                }
                Op::NewFinalizationRegistry => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let callback_reg = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    self.run_new_finalization_registry_regs(
                        context,
                        &mut *stack,
                        top_idx,
                        dst,
                        callback_reg,
                    )?;
                    continue;
                }
                Op::NewCollection => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let kind_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let iter_reg = context
                        .exec_register(instr, 2)
                        .ok_or(VmError::InvalidOperand)?;
                    self.run_new_collection_regs(
                        context,
                        &mut *stack,
                        top_idx,
                        dst,
                        kind_idx,
                        iter_reg,
                    )?;
                    continue;
                }
                Op::NewIntl => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let class_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let locale_reg = context
                        .exec_register(instr, 2)
                        .ok_or(VmError::InvalidOperand)?;
                    let options_reg = context
                        .exec_register(instr, 3)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_new_intl_regs(
                        context,
                        frame,
                        dst,
                        class_idx,
                        locale_reg,
                        options_reg,
                    )?;
                    continue;
                }
                Op::MathLoad => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_math_load_reg(context, frame, dst, name_idx)?;
                    continue;
                }
                Op::SymbolLoad => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_symbol_load_reg(context, frame, dst, name_idx)?;
                    continue;
                }
                Op::TemporalLoad => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_temporal_load_reg(context, frame, dst, name_idx)?;
                    continue;
                }
                Op::EnterTry => {
                    let catch_off = context
                        .exec_imm32(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let finally_off = context
                        .exec_imm32(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let exc_register = context
                        .exec_register(instr, 2)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_enter_try_regs(frame, catch_off, finally_off, exc_register)?;
                    continue;
                }
                Op::LeaveTry => {
                    let frame = &mut stack[top_idx];
                    self.run_leave_try(frame)?;
                    continue;
                }
                Op::Jump => {
                    let offset = context
                        .exec_imm32(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    apply_branch(frame, offset, &self.interrupt)?;
                    continue;
                }
                Op::JumpIfTrue => {
                    let offset = context
                        .exec_imm32(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let cond = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    if read_register(frame, cond)?.to_boolean() {
                        apply_branch(frame, offset, &self.interrupt)?;
                    } else {
                        frame.pc += 1;
                    }
                    continue;
                }
                Op::JumpIfFalse => {
                    let offset = context
                        .exec_imm32(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let cond = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    if !read_register(frame, cond)?.to_boolean() {
                        apply_branch(frame, offset, &self.interrupt)?;
                    } else {
                        frame.pc += 1;
                    }
                    continue;
                }
                Op::JumpIfNullish => {
                    let offset = context
                        .exec_imm32(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let cond = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    if read_register(frame, cond)?.is_nullish() {
                        apply_branch(frame, offset, &self.interrupt)?;
                    } else {
                        frame.pc += 1;
                    }
                    continue;
                }
                Op::LoadLocal => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let idx = context
                        .exec_imm32(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    let value = read_register(frame, idx as u16)?.clone();
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
                    continue;
                }
                Op::StoreLocal => {
                    let src = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let idx = context
                        .exec_imm32(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    let value = read_register(frame, src)?.clone();
                    write_register(frame, idx as u16, value)?;
                    frame.pc += 1;
                    continue;
                }
                Op::TdzError => {
                    let local_index = context
                        .exec_imm32(instr, 0)
                        .ok_or(VmError::InvalidOperand)?
                        as u32;
                    return Err(VmError::TemporalDeadZone { local_index });
                }
                Op::Add => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_add_regs(frame, dst, lhs, rhs)?;
                    continue;
                }
                Op::Sub => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_numeric_regs(frame, dst, lhs, rhs, number::sub, bigint_sub_op)?;
                    continue;
                }
                Op::Mul => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_numeric_regs(frame, dst, lhs, rhs, number::mul, bigint_mul_op)?;
                    continue;
                }
                Op::Div => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_numeric_regs(frame, dst, lhs, rhs, number::div, bigint::ops::div)?;
                    continue;
                }
                Op::Rem => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_numeric_regs(frame, dst, lhs, rhs, number::rem, bigint::ops::rem)?;
                    continue;
                }
                Op::Pow => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_numeric_regs(frame, dst, lhs, rhs, number::pow, bigint::ops::pow)?;
                    continue;
                }
                Op::BitwiseAnd => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_numeric_regs(
                        frame,
                        dst,
                        lhs,
                        rhs,
                        number::bitwise_and,
                        bigint_and_op,
                    )?;
                    continue;
                }
                Op::BitwiseOr => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_numeric_regs(frame, dst, lhs, rhs, number::bitwise_or, bigint_or_op)?;
                    continue;
                }
                Op::BitwiseXor => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_numeric_regs(
                        frame,
                        dst,
                        lhs,
                        rhs,
                        number::bitwise_xor,
                        bigint_xor_op,
                    )?;
                    continue;
                }
                Op::Shl => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_numeric_regs(frame, dst, lhs, rhs, number::shl, bigint::ops::shl)?;
                    continue;
                }
                Op::Shr => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_numeric_regs(
                        frame,
                        dst,
                        lhs,
                        rhs,
                        number::shr_arith,
                        bigint::ops::shr,
                    )?;
                    continue;
                }
                Op::LessThan | Op::LessEq | Op::GreaterThan | Op::GreaterEq => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_compare_regs(frame, dst, lhs, rhs, op)?;
                    continue;
                }
                Op::Ushr => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_ushr_regs(frame, dst, lhs, rhs)?;
                    continue;
                }
                Op::Neg => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_neg_regs(frame, dst, src)?;
                    continue;
                }
                Op::BitwiseNot => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_bitwise_not_regs(frame, dst, src)?;
                    continue;
                }
                Op::Equal | Op::NotEqual | Op::LooseEqual | Op::LooseNotEqual | Op::SameValue => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    match op {
                        Op::Equal => self.run_equal_regs(frame, dst, lhs, rhs, false)?,
                        Op::NotEqual => self.run_equal_regs(frame, dst, lhs, rhs, true)?,
                        Op::LooseEqual => self.run_loose_equal_regs(frame, dst, lhs, rhs, false)?,
                        Op::LooseNotEqual => {
                            self.run_loose_equal_regs(frame, dst, lhs, rhs, true)?;
                        }
                        Op::SameValue => self.run_same_value_regs(frame, dst, lhs, rhs)?,
                        _ => unreachable!("equality opcode group"),
                    }
                    continue;
                }
                Op::ArrayLength => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    let arr = match read_register(frame, src)? {
                        Value::Array(a) => *a,
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let n = NumberValue::from_i32(crate::array::len(arr, &self.gc_heap) as i32);
                    write_register(frame, dst, Value::Number(n))?;
                    frame.pc += 1;
                    continue;
                }
                Op::IsArray => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    let value = read_register(frame, src)?.clone();
                    let result = abstract_ops::is_array(&value);
                    write_register(frame, dst, Value::Boolean(result))?;
                    frame.pc += 1;
                    continue;
                }
                Op::Instanceof => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_instanceof_legacy_regs(frame, dst, lhs, rhs)?;
                    continue;
                }
                Op::HasProperty => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_has_property_regs(frame, dst, lhs, rhs)?;
                    continue;
                }
                _ => {}
            }

            let operands = context.exec_operands(instr);
            let frame = &mut stack[top_idx];
            match op {
                Op::Eval | Op::NewFunction => {
                    unreachable!("stack-modifying ops handled earlier in this loop")
                }
                Op::Return
                | Op::ReturnValue
                | Op::ReturnUndefined
                | Op::Call
                | Op::CallWithThis
                | Op::CallMethodValue
                | Op::CallSpread
                | Op::New
                | Op::NewSpread
                | Op::Throw
                | Op::EndFinally
                | Op::Await
                | Op::Yield => {
                    unreachable!("stack-modifying ops handled earlier in this loop")
                }
                Op::Nop
                | Op::LoadUndefined
                | Op::LoadHole
                | Op::LoadString
                | Op::LoadLength
                | Op::LoadNumber
                | Op::LoadInt32
                | Op::LoadTrue
                | Op::LoadFalse
                | Op::LoadNull
                | Op::LogicalNot
                | Op::ToBoolean
                | Op::ToNumber
                | Op::GetStringIndex
                | Op::TypeOf
                | Op::LoadThis
                | Op::LoadNewTarget
                | Op::DeleteProperty
                | Op::DeleteElement
                | Op::GetPrototype
                | Op::SetPrototype
                | Op::NewObject
                | Op::NewArray
                | Op::LoadRegExp
                | Op::LoadBigInt
                | Op::LoadUpvalue
                | Op::StoreUpvalue
                | Op::LoadProperty
                | Op::StoreProperty
                | Op::LoadElement
                | Op::StoreElement
                | Op::GetIterator
                | Op::IteratorNext
                | Op::MakeFunction
                | Op::MakeClass
                | Op::NewError
                | Op::NewBuiltinError
                | Op::LoadBuiltinError
                | Op::LoadGlobalThis
                | Op::LoadGlobalOrThrow
                | Op::LoadGlobalOrUndefined
                | Op::CollectRest
                | Op::CollectArguments
                | Op::ImportNamespace
                | Op::ImportMetaResolve
                | Op::PromiseFulfilledOf
                | Op::ArrayPush
                | Op::NewWeakRef
                | Op::NewFinalizationRegistry
                | Op::NewCollection
                | Op::NewIntl
                | Op::MathLoad
                | Op::SymbolLoad
                | Op::TemporalLoad
                | Op::EnterTry
                | Op::LeaveTry
                | Op::Jump
                | Op::JumpIfTrue
                | Op::JumpIfFalse
                | Op::JumpIfNullish
                | Op::LoadLocal
                | Op::StoreLocal
                | Op::TdzError => {
                    unreachable!("fixed-width ops handled earlier in this loop")
                }
                Op::MakeClosure => {
                    self.run_make_closure_operands(context, frame, operands)?;
                }
                Op::ArrayLength | Op::IsArray | Op::Instanceof | Op::HasProperty => {
                    unreachable!("property predicates handled earlier in this loop")
                }
                Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Div
                | Op::Rem
                | Op::Pow
                | Op::BitwiseAnd
                | Op::BitwiseOr
                | Op::BitwiseXor
                | Op::Shl
                | Op::Shr
                | Op::Ushr
                | Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Neg
                | Op::BitwiseNot
                | Op::Equal
                | Op::NotEqual
                | Op::LooseEqual
                | Op::LooseNotEqual
                | Op::SameValue => {
                    unreachable!("register binary ops handled earlier in this loop")
                }
                Op::JsonCall => {
                    self.run_json_static_call_operands(stack, operands)?;
                    continue;
                }
                Op::ArrayBufferCall => {
                    self.run_array_buffer_static_call_operands(stack, operands)?;
                    continue;
                }
                Op::TypedArrayCall => {
                    self.run_typed_array_static_call_operands(stack, operands)?;
                    continue;
                }
                Op::SharedArrayBufferCall => {
                    self.run_shared_array_buffer_static_call_operands(stack, operands)?;
                    continue;
                }
                Op::MathCall | Op::DateCall | Op::BigIntCall | Op::DataViewCall => {
                    self.run_static_call_operands(op, context, frame, operands)?;
                }
                Op::ProxyCall => {
                    self.run_proxy_static_call_operands(stack, operands)?;
                    continue;
                }
                Op::IteratorCall => {
                    self.run_iterator_static_call_operands(stack, operands)?;
                    continue;
                }
                // §28.1 Reflect static surface — single dispatcher
                // covering every spec method.
                // <https://tc39.es/ecma262/#sec-reflect-object>
                Op::ReflectCall => {
                    self.run_reflect_call_operands(context, stack, operands)?;
                    continue;
                }
                // §23.1.1 / §23.1.2 — typed Array static dispatch.
                // No string indirection: each shape has its own
                // opcode with `dst, argc, args...` operands.
                Op::ArrayConstruct | Op::ArrayFrom | Op::ArrayOf => {
                    self.run_array_static_operands(op, context, stack, operands)?;
                    continue;
                }
                Op::ObjectCall => {
                    self.run_object_static_call_operands(context, stack, operands)?;
                    continue;
                }
                Op::QueueMicrotask => {
                    self.run_queue_microtask_operands(context, frame, operands)?;
                }
                Op::PromiseNew => {
                    self.run_promise_new_operands(context, stack, operands)?;
                    continue;
                }
                Op::PromiseCall => {
                    self.run_promise_call_operands(context, stack, operands)?;
                    continue;
                }
                Op::GlobalCall => {
                    self.run_static_call_operands(op, context, frame, operands)?;
                }
                Op::ImportNamespaceDynamic => {
                    self.run_import_namespace_dynamic_operands(context, stack, top_idx, operands)?;
                    continue;
                }
                Op::BindFunction => {
                    self.drive_bind_function(stack, context, &operands)?;
                    continue;
                }
                Op::SymbolCall => {
                    self.run_static_call_operands(op, context, frame, operands)?;
                }
                Op::TemporalCall => {
                    self.run_static_call_operands(op, context, frame, operands)?;
                }
                Op::ToPrimitive => {
                    // Stack-modifying ladder dispatched in the
                    // pre-frame-borrow block above.
                    unreachable!("Op::ToPrimitive is handled by the pre-dispatch ladder")
                }
            }
        }
    }
}

impl Interpreter {
    /// Pop the top frame and route its completion value.
    ///
    /// # Algorithm
    /// 1. If the popped frame was entered via `Op::New`, apply the
    ///    `OrdinaryConstruct` step-11 substitution: a non-object
    ///    return reuses the freshly allocated `this`.
    /// 2. If the popped frame is an **async** frame, settle its
    ///    `result_promise` as fulfilled with the resolved value
    ///    and drain the resulting reaction jobs into the
    ///    microtask queue. The caller's destination register was
    ///    populated with the promise at call entry, so we do not
    ///    write to it again. When the stack is now empty (an
    ///    async-resume mini-stack just finished) return
    ///    `Ok(Some(Undefined))` so the surrounding driver loop
    ///    exits cleanly; otherwise return `Ok(None)` to continue
    ///    in the caller frame.
    /// 3. For non-async frames, write the resolved value into the
    ///    caller's `return_register`. Top-of-stack `<main>` falls
    ///    through with `return_register = None` and surfaces the
    ///    completion as `Some(value)`.
    ///
    /// # Errors
    /// - [`VmError::InvalidOperand`] when the stack is empty or
    ///   the caller's return register is out of bounds.
    fn pop_frame(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        value: Value,
    ) -> Result<Option<Value>, VmError> {
        let popped = stack.pop().ok_or(VmError::InvalidOperand)?;
        let resolved = match popped.construct_target {
            Some(_) if constructor_return_is_object(&value) => value,
            Some(target) => Value::Object(target),
            None => value,
        };
        if let Some(state) = popped.async_state {
            let jobs = state.result_promise.fulfill(&mut self.gc_heap, resolved);
            for j in jobs.jobs {
                self.microtasks.enqueue(j);
            }
            if stack.is_empty() {
                return Ok(Some(Value::Undefined));
            }
            return Ok(None);
        }
        let Some(return_reg) = popped.return_register else {
            return Ok(Some(resolved));
        };
        let caller = stack.last_mut().ok_or(VmError::InvalidOperand)?;
        write_register(caller, return_reg, resolved)?;
        // Caller's pc was set to the next instruction at call time;
        // nothing to advance here.
        Ok(None)
    }
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve `specifier` against `referrer`, mirroring the WHATWG URL
/// join semantics used by `import.meta.resolve`. Foundation handles:
///
/// - Absolute URLs (any scheme `xxx://`) and `file://` paths pass
///   through unchanged.
/// - Relative paths (`./foo`, `../bar`, `bar.ts`) join against the
///   referrer's directory.
/// - Bare specifiers without a referrer return as-is so the embedder's
///   resolver can pick them up.
///
/// # See also
/// - <https://html.spec.whatwg.org/multipage/webappapis.html#resolve-a-module-specifier>
fn resolve_relative_url(referrer: Option<&str>, specifier: &str) -> String {
    // Absolute URLs / data: URIs etc. pass through.
    if specifier.contains("://") || specifier.starts_with("data:") {
        return specifier.to_string();
    }
    let Some(referrer) = referrer else {
        return specifier.to_string();
    };
    if referrer.is_empty() {
        return specifier.to_string();
    }
    if specifier.starts_with('/') {
        // Replace path component of referrer.
        if let Some(scheme_end) = referrer.find("://") {
            let after = scheme_end + 3;
            let host_end = referrer[after..]
                .find('/')
                .map(|i| after + i)
                .unwrap_or(referrer.len());
            return format!("{}{}", &referrer[..host_end], specifier);
        }
        return specifier.to_string();
    }
    // Relative path — pop referrer's last path segment and join.
    let dir_end = referrer.rfind('/').unwrap_or(referrer.len());
    let dir = &referrer[..dir_end];
    let mut parts: Vec<&str> = if dir.contains("://") {
        let scheme_end = dir.find("://").map(|i| i + 3).unwrap_or(0);
        let mut acc = vec![&dir[..scheme_end]];
        acc.extend(dir[scheme_end..].split('/'));
        acc
    } else {
        dir.split('/').collect()
    };
    for component in specifier.split('/') {
        match component {
            "" | "." => continue,
            ".." => {
                if parts.last().is_some_and(|s| !s.contains("://")) {
                    parts.pop();
                }
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// Foundation §20.1.3 `Object.prototype.<method>` interception for
/// ordinary objects. Returns `Ok(Some(value))` when the call was
/// dispatched here, `Ok(None)` when the method is not one of the
/// prototype names so the caller falls through to the regular lookup.
///
/// Handles: `hasOwnProperty`, `propertyIsEnumerable`,
/// `isPrototypeOf`, `toString`, `valueOf`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-properties-of-the-object-prototype-object>
fn object_prototype_intercept(
    obj: &object::JsObject,
    name: &str,
    args: &SmallVec<[Value; 8]>,
    string_heap: &string::StringHeap,
    gc_heap: &otter_gc::GcHeap,
    function_prototype: Option<object::JsObject>,
) -> Result<Option<Value>, VmError> {
    match name {
        // §20.1.3.2 Object.prototype.hasOwnProperty(V)
        // <https://tc39.es/ecma262/#sec-object.prototype.hasownproperty>
        "hasOwnProperty" => {
            let key = property_key_from_arg(args.first())?;
            let present = !matches!(
                object::lookup_own(*obj, gc_heap, &key),
                object::PropertyLookup::Absent
            );
            Ok(Some(Value::Boolean(present)))
        }
        // §20.1.3.4 Object.prototype.propertyIsEnumerable(V)
        // <https://tc39.es/ecma262/#sec-object.prototype.propertyisenumerable>
        "propertyIsEnumerable" => {
            let key = property_key_from_arg(args.first())?;
            let result = match object::lookup_own(*obj, gc_heap, &key) {
                object::PropertyLookup::Data { flags, .. } => flags.enumerable(),
                object::PropertyLookup::Accessor { flags, .. } => flags.enumerable(),
                object::PropertyLookup::Absent => false,
            };
            Ok(Some(Value::Boolean(result)))
        }
        // §20.1.3.3 Object.prototype.isPrototypeOf(V)
        // <https://tc39.es/ecma262/#sec-object.prototype.isprototypeof>
        "isPrototypeOf" => {
            let result = args.first().is_some_and(|value| {
                value_has_prototype_in_chain(value, *obj, gc_heap, function_prototype)
            });
            Ok(Some(Value::Boolean(result)))
        }
        // §20.1.3.6 / §20.5.3.4 — `toString()`. Error instances
        // override Object.prototype.toString to return
        // `<name>: <message>`; plain objects fall back to
        // `[object Object]`. The Error path routes through
        // [`error_classes::render_error_to_string`] so the
        // user-facing call and the unwind diagnostic share one
        // implementation.
        // <https://tc39.es/ecma262/#sec-object.prototype.tostring>
        // <https://tc39.es/ecma262/#sec-error.prototype.tostring>
        "toString" => {
            let recv_value = Value::Object(*obj);
            let has_error_shape = object::get(*obj, gc_heap, "name").is_some()
                || object::get(*obj, gc_heap, "message").is_some();
            let display = if has_error_shape {
                let rendered = error_classes::render_error_to_string(&recv_value, gc_heap);
                if rendered.is_empty() {
                    "[object Object]".to_string()
                } else {
                    rendered
                }
            } else {
                "[object Object]".to_string()
            };
            let s = JsString::from_str(&display, string_heap).map_err(|_| VmError::TypeMismatch)?;
            Ok(Some(Value::String(s)))
        }
        // §20.1.3.7 Object.prototype.valueOf() — returns the receiver.
        // <https://tc39.es/ecma262/#sec-object.prototype.valueof>
        "valueOf" => Ok(Some(Value::Object(*obj))),
        _ => Ok(None),
    }
}

fn value_has_prototype_in_chain(
    value: &Value,
    target: object::JsObject,
    gc_heap: &otter_gc::GcHeap,
    function_prototype: Option<object::JsObject>,
) -> bool {
    match value {
        Value::Object(_) if object_has_construct_slot(value, gc_heap) => {
            function_value_has_prototype_in_chain(target, gc_heap, function_prototype)
        }
        Value::Object(obj) => object::has_in_proto_chain(*obj, gc_heap, target),
        Value::Function { .. }
        | Value::Closure { .. }
        | Value::BoundFunction(_)
        | Value::NativeFunction(_)
        | Value::ClassConstructor(_) => {
            function_value_has_prototype_in_chain(target, gc_heap, function_prototype)
        }
        _ => false,
    }
}

fn function_value_has_prototype_in_chain(
    target: object::JsObject,
    gc_heap: &otter_gc::GcHeap,
    function_prototype: Option<object::JsObject>,
) -> bool {
    let Some(function_prototype) = function_prototype else {
        return false;
    };
    function_prototype == target || object::has_in_proto_chain(function_prototype, gc_heap, target)
}

fn native_function_object_prototype_intercept(
    native: &NativeFunction,
    name: &str,
    args: &SmallVec<[Value; 8]>,
    gc_heap: &otter_gc::GcHeap,
    string_heap: &StringHeap,
) -> Result<Option<Value>, VmError> {
    match name {
        "hasOwnProperty" => {
            let key = property_key_from_arg(args.first())?;
            Ok(Some(Value::Boolean(
                native
                    .own_property_descriptor(gc_heap, string_heap, &key)?
                    .is_some(),
            )))
        }
        "propertyIsEnumerable" => {
            let _key = property_key_from_arg(args.first())?;
            Ok(Some(Value::Boolean(false)))
        }
        "isPrototypeOf" => Ok(Some(Value::Boolean(false))),
        _ => Ok(None),
    }
}

fn bound_function_object_prototype_intercept(
    bound: &BoundFunction,
    name: &str,
    args: &SmallVec<[Value; 8]>,
    gc_heap: &otter_gc::GcHeap,
) -> Result<Option<Value>, VmError> {
    match name {
        "hasOwnProperty" => {
            let key = property_key_from_arg(args.first())?;
            Ok(Some(Value::Boolean(
                function_metadata::bound_has_own_property(bound, gc_heap, &key),
            )))
        }
        "propertyIsEnumerable" => {
            let key = property_key_from_arg(args.first())?;
            Ok(Some(Value::Boolean(
                function_metadata::bound_own_property_is_enumerable(bound, gc_heap, &key),
            )))
        }
        "isPrototypeOf" => Ok(Some(Value::Boolean(false))),
        _ => Ok(None),
    }
}

fn descriptor_value(desc: &crate::object::PropertyDescriptor) -> Value {
    match &desc.kind {
        crate::object::DescriptorKind::Data { value } => value.clone(),
        crate::object::DescriptorKind::Accessor { .. } => Value::Undefined,
    }
}

pub(crate) fn value_kind_name(value: &Value) -> &'static str {
    match value {
        Value::Undefined | Value::Hole => "undefined",
        Value::Null => "null",
        Value::Boolean(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Symbol(_) => "symbol",
        Value::BigInt(_) => "bigint",
        Value::Object(_) => "object",
        Value::Array(_) => "array",
        Value::Function { .. } | Value::Closure { .. } => "function",
        Value::NativeFunction(_) => "function",
        Value::BoundFunction(_) => "function",
        Value::ClassConstructor(_) => "class constructor",
        Value::RegExp(_) => "regexp",
        Value::Date(_) => "date",
        Value::Promise(_) => "promise",
        Value::Proxy(_) => "proxy",
        Value::Map(_) => "map",
        Value::Set(_) => "set",
        Value::WeakMap(_) => "weakmap",
        Value::WeakSet(_) => "weakset",
        Value::WeakRef(_) => "weakref",
        Value::FinalizationRegistry(_) => "finalization registry",
        Value::Generator(_) => "generator",
        Value::Iterator(_) => "iterator",
        Value::Temporal(_) => "temporal",
        Value::Intl(_) => "intl",
        Value::ArrayBuffer(_) => "arraybuffer",
        Value::DataView(_) => "dataview",
        Value::TypedArray(_) => "typedarray",
    }
}

/// §7.1.19 ToPropertyKey for a single optional argument used by
/// `Object.prototype.hasOwnProperty` / `propertyIsEnumerable`.
fn property_key_from_arg(arg: Option<&Value>) -> Result<String, VmError> {
    match arg {
        Some(Value::String(s)) => Ok(s.to_lossy_string()),
        Some(Value::Number(n)) => Ok(n.to_display_string()),
        Some(Value::Boolean(b)) => Ok((if *b { "true" } else { "false" }).to_string()),
        Some(Value::Null) => Ok("null".to_string()),
        Some(Value::Undefined) | None => Ok("undefined".to_string()),
        _ => Err(VmError::TypeMismatch),
    }
}

fn to_length(value: &Value) -> Result<usize, VmError> {
    match value {
        Value::Symbol(_) | Value::BigInt(_) => return Err(VmError::TypeMismatch),
        _ => {}
    }
    let n = number::to_number_value(value);
    if n.is_nan() || n <= 0.0 {
        return Ok(0);
    }
    if n.is_infinite() {
        return Ok(usize::MAX.min(9_007_199_254_740_991));
    }
    let len = n.trunc().min(9_007_199_254_740_991.0);
    if len > usize::MAX as f64 {
        Ok(usize::MAX)
    } else {
        Ok(len as usize)
    }
}

/// Validate that the first callback argument to an Array method is
/// callable per ECMA-262 §23.1.3 step 3 (CheckObjectCoercible +
/// IsCallable). Returns the callable value cloned out for the
/// dispatch loop.
fn require_callable(arg: Option<&Value>) -> Result<Value, VmError> {
    match arg {
        Some(v) if abstract_ops::is_callable(v) => Ok(v.clone()),
        _ => Err(VmError::NotCallable),
    }
}

/// Build the canonical `(value, index, array)` argument tuple every
/// `Array.prototype` callback expects.
fn build_array_cb_args(value: &Value, index: usize, arr: &Value) -> SmallVec<[Value; 8]> {
    let mut cb_args: SmallVec<[Value; 8]> = SmallVec::new();
    cb_args.push(value.clone());
    cb_args.push(Value::Number(NumberValue::from_i32(index as i32)));
    cb_args.push(arr.clone());
    cb_args
}

fn read_register(frame: &Frame, idx: u16) -> Result<&Value, VmError> {
    frame
        .registers
        .get(idx as usize)
        .ok_or(VmError::InvalidOperand)
}

fn write_register(frame: &mut Frame, idx: u16, value: Value) -> Result<(), VmError> {
    let slot = frame
        .registers
        .get_mut(idx as usize)
        .ok_or(VmError::InvalidOperand)?;
    *slot = value;
    Ok(())
}

/// Build the native callable that `arr[Symbol.iterator]` evaluates
/// to. Invoking the returned function (with any `this`) yields a
/// fresh [`Value::Iterator`] over the captured array — matching the
/// surface of `Array.prototype[@@iterator]` from
/// [ECMA-262 §23.1.5.1](https://tc39.es/ecma262/#sec-array.prototype-@@iterator).
///
/// # Invariants
/// - Capturing the array by handle means the iterator observes
///   subsequent in-place mutations through the same `JsArray`,
///   matching real-engine `Array.prototype[Symbol.iterator]`
///   semantics.
fn make_array_iterator_factory(
    array: JsArray,
    heap: &mut otter_gc::GcHeap,
) -> Result<Value, otter_gc::OutOfMemory> {
    native_value_with_captures(
        heap,
        "Array[Symbol.iterator]",
        smallvec::smallvec![Value::Array(array)],
        array_iterator_factory_call,
    )
}

pub(crate) fn make_array_iterator_factory_runtime_rooted(
    interp: &mut Interpreter,
    array: JsArray,
) -> Result<Value, otter_gc::OutOfMemory> {
    interp.native_value_with_captures_runtime_rooted(
        "Array[Symbol.iterator]",
        smallvec::smallvec![Value::Array(array)],
        &[],
        &[],
        array_iterator_factory_call,
    )
}

fn array_iterator_factory_call(
    ctx: &mut NativeCtx<'_>,
    _: &[Value],
    captures: &[Value],
) -> Result<Value, NativeError> {
    let array = match captures.first() {
        Some(Value::Array(array)) => *array,
        _ => {
            return Err(NativeError::TypeError {
                name: "Array[Symbol.iterator]",
                reason: "missing traced array capture".to_string(),
            });
        }
    };
    let state = IteratorState::Array { array, index: 0 };
    Ok(Value::Iterator(ctx.alloc_iterator_state(
        state,
        &[],
        &[],
    )?))
}

/// Generator resume entry per ECMA-262 §27.5.3.
#[derive(Debug, Clone)]
pub enum GeneratorResumeKind {
    /// `gen.next(arg)`.
    Next(Value),
    /// `gen.return(arg)` — foundation closes the generator without
    /// running additional finally blocks.
    Return(Value),
    /// `gen.throw(reason)` — re-enters the body and unwinds.
    Throw(Value),
}

/// Coerce `take(n)` / `drop(n)` argument to a non-negative integer.
/// Per the iterator-helpers proposal step 3, NaN / non-integer
/// inputs raise a RangeError-equivalent (surfaced here as
/// `TypeMismatch`).
fn take_drop_count(arg: Option<&Value>) -> Result<u64, VmError> {
    let n = match arg {
        None | Some(Value::Undefined) => return Err(VmError::TypeMismatch),
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::Boolean(true)) => 1.0,
        Some(Value::Boolean(false)) | Some(Value::Null) => 0.0,
        _ => return Err(VmError::TypeMismatch),
    };
    if n.is_nan() {
        return Err(VmError::TypeMismatch);
    }
    if n.is_infinite() && n.is_sign_positive() {
        return Ok(u64::MAX);
    }
    if n < 0.0 {
        return Err(VmError::TypeMismatch);
    }
    Ok(n.trunc() as u64)
}

/// Drive an iterator one step. Returns `(value, done)`. Once an
/// iterator hands back `done = true`, its state transitions to
/// `Exhausted` so subsequent calls are stable no-ops (matches the
/// spec rule "an iterator never produces values after it has
/// produced `done: true`"; §7.4.2 step 6).
fn step_iterator(
    iter: IteratorHandle,
    string_heap: &StringHeap,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<(Value, bool), VmError> {
    enum FastIteratorSnapshot {
        Array(JsArray, usize),
        String(JsString, u32),
        Exhausted,
        Slow,
    }

    let snapshot = gc_heap.read_payload(iter, |state| match state {
        IteratorState::Array { array, index } => FastIteratorSnapshot::Array(*array, *index),
        IteratorState::String { string, index } => {
            FastIteratorSnapshot::String(string.clone(), *index)
        }
        IteratorState::Exhausted => FastIteratorSnapshot::Exhausted,
        IteratorState::User { .. }
        | IteratorState::Generator { .. }
        | IteratorState::Map { .. }
        | IteratorState::Filter { .. }
        | IteratorState::Take { .. }
        | IteratorState::Drop { .. }
        | IteratorState::FlatMap { .. } => FastIteratorSnapshot::Slow,
    });

    let outcome = match snapshot {
        FastIteratorSnapshot::Array(array, index) => {
            if index >= crate::array::len(array, gc_heap) {
                None
            } else {
                let v = crate::array::get(array, gc_heap, index);
                gc_heap.with_payload(iter, |state| {
                    if let IteratorState::Array { index, .. } = state {
                        *index += 1;
                    }
                });
                Some(v)
            }
        }
        FastIteratorSnapshot::String(string, index) => {
            // §22.1.5.1 `%StringIteratorPrototype%.next`.
            if let Some(unit) = string.char_code_at(index) {
                let next_unit = string.char_code_at(index + 1);
                let is_pair = (0xD800..=0xDBFF).contains(&unit)
                    && matches!(next_unit, Some(low) if (0xDC00..=0xDFFF).contains(&low));
                let (s, advance) = if is_pair {
                    let pair = [unit, next_unit.unwrap()];
                    (JsString::from_utf16_units(&pair, string_heap)?, 2)
                } else {
                    (JsString::from_utf16_units(&[unit], string_heap)?, 1)
                };
                gc_heap.with_payload(iter, |state| {
                    if let IteratorState::String { index, .. } = state {
                        *index += advance;
                    }
                });
                Some(Value::String(s))
            } else {
                None
            }
        }
        FastIteratorSnapshot::Exhausted => None,
        FastIteratorSnapshot::Slow => return Err(VmError::TypeMismatch),
    };
    match outcome {
        Some(value) => Ok((value, false)),
        None => {
            gc_heap.with_payload(iter, |state| *state = IteratorState::Exhausted);
            Ok((Value::Undefined, true))
        }
    }
}

/// Whether a constructor return value is an ECMAScript object and
/// therefore replaces the freshly-created receiver.
fn constructor_return_is_object(value: &Value) -> bool {
    matches!(
        value,
        Value::Object(_)
            | Value::Array(_)
            | Value::Function { .. }
            | Value::Closure { .. }
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
            | Value::Date(_)
            | Value::Intl(_)
            | Value::ArrayBuffer(_)
            | Value::DataView(_)
            | Value::TypedArray(_)
            | Value::Generator(_)
            | Value::Proxy(_)
    )
}

/// `true` when `value` is a `JsObject` whose internal native
/// call slot carries a `Value::NativeFunction`, i.e. it is
/// callable even though it is not a plain function value.
fn object_has_call_slot(value: &Value, heap: &otter_gc::GcHeap) -> bool {
    let Value::Object(obj) = value else {
        return false;
    };
    matches!(
        crate::object::call_native(*obj, heap),
        Some(Value::NativeFunction(_))
    )
}

/// `true` when `value` is a VM constructor. This is intentionally
/// stricter than `IsCallable`: callable ordinary objects such as
/// `Function.prototype` must reject `new`.
fn is_constructor_runtime(
    value: &Value,
    context: &ExecutionContext,
    heap: &otter_gc::GcHeap,
) -> bool {
    match value {
        Value::BoundFunction(bound) => {
            let (target, _, _) = bound.parts(heap);
            is_constructor_runtime(&target, context, heap)
        }
        _ => {
            abstract_ops::is_constructor(value, context, heap)
                || object_has_construct_slot(value, heap)
        }
    }
}

/// `true` when `value` is a `JsObject` whose internal native
/// constructor slot carries a `Value::NativeFunction`, i.e. it is
/// admissible as a `new` callee even though it is not a plain
/// function value.
fn object_has_construct_slot(value: &Value, heap: &otter_gc::GcHeap) -> bool {
    let Value::Object(obj) = value else {
        return false;
    };
    matches!(
        crate::object::constructor_native(*obj, heap),
        Some(Value::NativeFunction(_))
    )
}

fn is_restricted_function_property(name: &str) -> bool {
    matches!(name, "caller" | "arguments")
}

/// Pick the property name for the current
/// [`ToPrimitiveStage`] under ECMA-262 §7.1.1.1
/// `OrdinaryToPrimitive`.
///
/// - `Default` / `Number` → first slot is `"valueOf"`, second is
///   `"toString"`.
/// - `String` → first slot is `"toString"`, second is `"valueOf"`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-ordinarytoprimitive>
fn ordinary_method_for(
    hint: abstract_ops::ToPrimitiveHint,
    stage: ToPrimitiveStage,
) -> &'static str {
    let (first, second) = match hint {
        abstract_ops::ToPrimitiveHint::String => ("toString", "valueOf"),
        abstract_ops::ToPrimitiveHint::Default | abstract_ops::ToPrimitiveHint::Number => {
            ("valueOf", "toString")
        }
    };
    match stage {
        ToPrimitiveStage::OrdinaryFirst => first,
        ToPrimitiveStage::OrdinarySecond => second,
        ToPrimitiveStage::SymbolToPrim | ToPrimitiveStage::Exhausted => "",
    }
}

/// `true` when `value` is one of the call-site shapes the dispatcher
/// can invoke. Thin wrapper over [`abstract_ops::is_callable`]
/// (ECMA-262 §7.2.3) — kept under the same name so existing call
/// sites do not change.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-iscallable>
pub(crate) fn is_callable(value: &Value) -> bool {
    abstract_ops::is_callable(value)
}

/// Public re-export of [`is_callable`] for crate-external dispatch
/// helpers (e.g. [`crate::promise_dispatch`]).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-iscallable>
#[must_use]
pub fn is_callable_value(value: &Value) -> bool {
    abstract_ops::is_callable(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::{
        Constant, Function, Instruction, Op, Operand, SourceKind as BcSourceKind, SpanEntry,
    };

    fn spans_for(code: &[Instruction]) -> Vec<SpanEntry> {
        code.iter()
            .map(|i| SpanEntry {
                pc: i.pc,
                span: (0, 0),
            })
            .collect()
    }

    fn test_function(
        id: u32,
        name: &str,
        param_count: u16,
        scratch: u16,
        code: Vec<Instruction>,
    ) -> Function {
        let spans = spans_for(&code);
        Function {
            id,
            name: name.to_string(),
            span: (0, 0),
            locals: 0,
            scratch,
            param_count,
            own_upvalue_count: 0,
            is_strict: false,
            is_arrow: false,
            has_rest: false,
            is_async: false,
            is_generator: false,
            is_async_generator: false,
            is_module: false,
            needs_arguments: false,
            arguments_object_kind: ArgumentsObjectKind::Unmapped,
            mapped_argument_bindings: Vec::new(),
            module_url: String::new(),
            code,
            spans,
        }
    }

    fn module_with(code: Vec<Instruction>, scratch: u16) -> BytecodeModule {
        BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, scratch, code)],
            constants: vec![],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        }
    }

    #[test]
    fn returns_undefined_for_load_then_return() {
        let module = module_with(
            vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadUndefined,
                    operands: vec![Operand::Register(0)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)].into(),
                },
            ],
            1,
        );
        let mut interp = Interpreter::new();
        let context = ExecutionContext::from_module(module);
        assert_eq!(interp.run(&context).unwrap(), Value::Undefined);
    }

    #[test]
    fn direct_bytecode_call_binds_arguments_from_register_window() {
        let callee = test_function(
            1,
            "callee",
            3,
            2,
            vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadInt32,
                    operands: vec![Operand::Register(3), Operand::Imm32(100)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::Mul,
                    operands: vec![
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::Register(3),
                    ]
                    .into(),
                },
                Instruction {
                    pc: 2,
                    op: Op::LoadInt32,
                    operands: vec![Operand::Register(4), Operand::Imm32(10)].into(),
                },
                Instruction {
                    pc: 3,
                    op: Op::Mul,
                    operands: vec![
                        Operand::Register(4),
                        Operand::Register(1),
                        Operand::Register(4),
                    ]
                    .into(),
                },
                Instruction {
                    pc: 4,
                    op: Op::Add,
                    operands: vec![
                        Operand::Register(3),
                        Operand::Register(3),
                        Operand::Register(4),
                    ]
                    .into(),
                },
                Instruction {
                    pc: 5,
                    op: Op::Add,
                    operands: vec![
                        Operand::Register(3),
                        Operand::Register(3),
                        Operand::Register(2),
                    ]
                    .into(),
                },
                Instruction {
                    pc: 6,
                    op: Op::Return,
                    operands: vec![Operand::Register(3)].into(),
                },
            ],
        );
        let main_code = vec![
            Instruction {
                pc: 0,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(1), Operand::Imm32(1)].into(),
            },
            Instruction {
                pc: 1,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(2), Operand::Imm32(2)].into(),
            },
            Instruction {
                pc: 2,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(3), Operand::Imm32(3)].into(),
            },
            Instruction {
                pc: 3,
                op: Op::MakeFunction,
                operands: vec![Operand::Register(4), Operand::ConstIndex(0)].into(),
            },
            Instruction {
                pc: 4,
                op: Op::Call,
                operands: vec![
                    Operand::Register(0),
                    Operand::Register(4),
                    Operand::ConstIndex(3),
                    Operand::Register(3),
                    Operand::Register(1),
                    Operand::Register(2),
                ]
                .into(),
            },
            Instruction {
                pc: 5,
                op: Op::Return,
                operands: vec![Operand::Register(0)].into(),
            },
        ];
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 5, main_code), callee],
            constants: vec![Constant::FunctionId { index: 1 }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let context = ExecutionContext::from_module(module);
        assert_eq!(
            interp.run(&context).unwrap(),
            Value::Number(NumberValue::Smi(312))
        );
    }

    #[test]
    fn direct_bytecode_call_window_populates_arguments_object() {
        let mut callee = test_function(
            1,
            "callee",
            0,
            1,
            vec![
                Instruction {
                    pc: 0,
                    op: Op::CollectArguments,
                    operands: vec![Operand::Register(0)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)].into(),
                },
            ],
        );
        callee.needs_arguments = true;
        let main_code = vec![
            Instruction {
                pc: 0,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(1), Operand::Imm32(21)].into(),
            },
            Instruction {
                pc: 1,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(2), Operand::Imm32(34)].into(),
            },
            Instruction {
                pc: 2,
                op: Op::MakeFunction,
                operands: vec![Operand::Register(3), Operand::ConstIndex(0)].into(),
            },
            Instruction {
                pc: 3,
                op: Op::Call,
                operands: vec![
                    Operand::Register(0),
                    Operand::Register(3),
                    Operand::ConstIndex(2),
                    Operand::Register(2),
                    Operand::Register(1),
                ]
                .into(),
            },
            Instruction {
                pc: 4,
                op: Op::Return,
                operands: vec![Operand::Register(0)].into(),
            },
        ];
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 4, main_code), callee],
            constants: vec![Constant::FunctionId { index: 1 }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let context = ExecutionContext::from_module(module);
        let Value::Object(args) = interp.run(&context).unwrap() else {
            panic!("expected arguments object");
        };
        assert_eq!(
            object::get(args, interp.gc_heap(), "0"),
            Some(Value::Number(NumberValue::Smi(34)))
        );
        assert_eq!(
            object::get(args, interp.gc_heap(), "1"),
            Some(Value::Number(NumberValue::Smi(21)))
        );
        assert_eq!(
            object::get(args, interp.gc_heap(), "length"),
            Some(Value::Number(NumberValue::Smi(2)))
        );
    }

    #[test]
    fn direct_bytecode_call_window_populates_rest_arguments() {
        let mut callee = test_function(
            1,
            "callee",
            1,
            1,
            vec![
                Instruction {
                    pc: 0,
                    op: Op::CollectRest,
                    operands: vec![Operand::Register(1)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::Return,
                    operands: vec![Operand::Register(1)].into(),
                },
            ],
        );
        callee.has_rest = true;
        let main_code = vec![
            Instruction {
                pc: 0,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(1), Operand::Imm32(5)].into(),
            },
            Instruction {
                pc: 1,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(2), Operand::Imm32(8)].into(),
            },
            Instruction {
                pc: 2,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(3), Operand::Imm32(13)].into(),
            },
            Instruction {
                pc: 3,
                op: Op::MakeFunction,
                operands: vec![Operand::Register(4), Operand::ConstIndex(0)].into(),
            },
            Instruction {
                pc: 4,
                op: Op::Call,
                operands: vec![
                    Operand::Register(0),
                    Operand::Register(4),
                    Operand::ConstIndex(3),
                    Operand::Register(1),
                    Operand::Register(3),
                    Operand::Register(2),
                ]
                .into(),
            },
            Instruction {
                pc: 5,
                op: Op::Return,
                operands: vec![Operand::Register(0)].into(),
            },
        ];
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 5, main_code), callee],
            constants: vec![Constant::FunctionId { index: 1 }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let context = ExecutionContext::from_module(module);
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let Value::Array(rest) = interp.run(&context).unwrap() else {
            panic!("expected rest array");
        };
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        let elements = array::with_elements(rest, interp.gc_heap(), |elements| elements.to_vec());
        assert_eq!(
            elements,
            vec![
                Value::Number(NumberValue::Smi(13)),
                Value::Number(NumberValue::Smi(8))
            ]
        );
        assert!(
            after > before,
            "CollectRest should allocate the rest array in young space"
        );
    }

    #[test]
    fn bytecode_store_property_function_bag_uses_young_allocation_with_frame_roots() {
        let callee = test_function(1, "callee", 0, 0, Vec::new());
        let main_code = vec![
            Instruction {
                pc: 0,
                op: Op::MakeFunction,
                operands: vec![Operand::Register(0), Operand::ConstIndex(0)].into(),
            },
            Instruction {
                pc: 1,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(1), Operand::Imm32(42)].into(),
            },
            Instruction {
                pc: 2,
                op: Op::StoreProperty,
                operands: vec![
                    Operand::Register(0),
                    Operand::ConstIndex(1),
                    Operand::Register(1),
                    Operand::Register(2),
                ]
                .into(),
            },
            Instruction {
                pc: 3,
                op: Op::Return,
                operands: vec![Operand::Register(0)].into(),
            },
        ];
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 3, main_code), callee],
            constants: vec![
                Constant::FunctionId { index: 1 },
                Constant::String {
                    utf16: "custom".encode_utf16().collect(),
                },
            ],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let context = ExecutionContext::from_module(module);
        assert!(matches!(
            interp.run(&context).unwrap(),
            Value::Function { .. }
        ));
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "StoreProperty should allocate function user props in young space"
        );
        let desc = interp
            .ordinary_function_own_property_descriptor(Some(&context), 1, "custom")
            .unwrap()
            .expect("custom property descriptor");
        assert_eq!(
            descriptor_value(&desc),
            Value::Number(NumberValue::from_i32(42))
        );
    }

    #[test]
    fn bytecode_function_prototype_uses_young_allocation_with_frame_roots() {
        let module = module_with(Vec::new(), 4);
        let context = ExecutionContext::from_module(module.clone());
        let mut interp = Interpreter::new();
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::Function { function_id: 1 };
        stack.push(frame);

        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let prototype = interp
            .function_property_get_stack_rooted(&context, &stack, 1, "prototype")
            .expect("prototype");
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Function .prototype should allocate user bag and prototype object in young space"
        );

        let Value::Object(proto) = prototype else {
            panic!("function prototype should be an object");
        };
        assert_eq!(
            object::get(proto, interp.gc_heap(), "constructor"),
            Some(Value::Function { function_id: 1 })
        );
    }

    #[test]
    fn runtime_function_prototype_uses_young_allocation_with_explicit_roots() {
        let module = module_with(Vec::new(), 4);
        let context = ExecutionContext::from_module(module);
        let mut interp = Interpreter::new();
        let target = Value::Function { function_id: 1 };
        let arg = Value::String(JsString::from_str("rooted-arg", &interp.string_heap).unwrap());
        let args = [arg.clone()];

        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let prototype = interp
            .function_property_get_runtime_rooted(&context, 1, "prototype", &[&target], &[&args])
            .expect("prototype");
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Function .prototype should allocate through runtime roots when no VM frame is active"
        );

        let Value::Object(proto) = prototype else {
            panic!("function prototype should be an object");
        };
        assert_eq!(
            object::get(proto, interp.gc_heap(), "constructor"),
            Some(target)
        );
    }

    #[test]
    fn bytecode_instanceof_function_prototype_uses_stack_roots() {
        let module = module_with(Vec::new(), 4);
        let context = ExecutionContext::from_module(module.clone());
        let mut interp = Interpreter::new();
        let lhs = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).expect("lhs");
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[1] = Value::Object(lhs);
        frame.registers[2] = Value::Function { function_id: 1 };
        stack.push(frame);
        let operands = vec![
            Operand::Register(0),
            Operand::Register(1),
            Operand::Register(2),
        ];

        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            interp
                .drive_instanceof(&mut stack, &context, operands.as_slice())
                .expect("instanceof")
        );
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "instanceof should lazily allocate function .prototype through stack roots"
        );
        assert_eq!(stack[0].registers[0], Value::Boolean(false));
        let desc = interp
            .ordinary_function_own_property_descriptor(Some(&context), 1, "prototype")
            .unwrap()
            .expect("prototype descriptor");
        assert!(matches!(descriptor_value(&desc), Value::Object(_)));
    }

    #[test]
    fn new_function_wrapper_uses_rooted_prototype_and_native_allocation() {
        let compiled_main = vec![
            Instruction {
                pc: 0,
                op: Op::MakeFunction,
                operands: vec![Operand::Register(0), Operand::ConstIndex(0)].into(),
            },
            Instruction {
                pc: 1,
                op: Op::Return,
                operands: vec![Operand::Register(0)].into(),
            },
        ];
        let inner = test_function(1, "anonymous", 0, 1, Vec::new());
        let compiled = BytecodeModule {
            module: "eval.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 1, compiled_main), inner],
            constants: vec![Constant::FunctionId { index: 1 }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let outer = module_with(Vec::new(), 4);
        let context = ExecutionContext::from_module(outer.clone());
        let mut interp = Interpreter::new();
        interp.set_eval_hook(Some(std::rc::Rc::new(move |_, _| Ok(compiled.clone()))));
        let arg = Value::String(JsString::from_str("", &interp.string_heap).unwrap());
        let args = [arg];
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&outer.functions[0]);
        frame.registers[0] = args[0].clone();
        stack.push(frame);

        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let wrapper = interp
            .build_function_constructor_with_roots(
                &context,
                args.as_slice(),
                Some(&stack),
                &[],
                &[args.as_slice()],
            )
            .expect("Function constructor");
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "new Function wrapper should allocate prototype and native metadata through roots"
        );

        let Value::NativeFunction(native) = wrapper else {
            panic!("new Function should return a native wrapper");
        };
        let desc = native
            .own_property_descriptor(interp.gc_heap(), &interp.string_heap, "prototype")
            .unwrap()
            .expect("prototype descriptor");
        let Value::Object(proto) = descriptor_value(&desc) else {
            panic!("prototype should be an object");
        };
        assert_eq!(
            object::get(proto, interp.gc_heap(), "constructor"),
            Some(Value::NativeFunction(native))
        );
    }

    #[test]
    fn get_iterator_map_snapshot_uses_young_allocation_with_frame_roots() {
        let module = module_with(Vec::new(), 5);
        let mut interp = Interpreter::new();
        let map = crate::collections::alloc_map(interp.gc_heap_mut()).unwrap();
        crate::collections::map_set(
            map,
            interp.gc_heap_mut(),
            Value::Number(NumberValue::from_i32(1)),
            Value::Number(NumberValue::from_i32(10)),
        )
        .unwrap();
        crate::collections::map_set(
            map,
            interp.gc_heap_mut(),
            Value::Number(NumberValue::from_i32(2)),
            Value::Number(NumberValue::from_i32(20)),
        )
        .unwrap();

        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::Map(map);
        stack.push(frame);

        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        interp.run_get_iterator_regs(&mut stack, 0, 1, 0).unwrap();
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "GetIterator over Map should allocate snapshot arrays and iterator state in young space"
        );

        interp
            .run_iterator_next_regs(&mut stack[0], 2, 3, 1)
            .unwrap();
        assert_eq!(stack[0].registers[3], Value::Boolean(false));
        let Value::Array(pair) = stack[0].registers[2] else {
            panic!("Map iterator should yield entry arrays");
        };
        let values =
            crate::array::with_elements(pair, interp.gc_heap(), |elements| elements.to_vec());
        assert_eq!(
            values,
            vec![
                Value::Number(NumberValue::from_i32(1)),
                Value::Number(NumberValue::from_i32(10)),
            ]
        );
    }

    #[test]
    fn iterator_from_map_uses_stack_rooted_snapshot_and_state_allocation() {
        let module = module_with(Vec::new(), 5);
        let mut interp = Interpreter::new();
        let map = crate::collections::alloc_map(interp.gc_heap_mut()).unwrap();
        crate::collections::map_set(
            map,
            interp.gc_heap_mut(),
            Value::Number(NumberValue::from_i32(4)),
            Value::Number(NumberValue::from_i32(40)),
        )
        .unwrap();

        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::Map(map);
        stack.push(frame);
        let operands = vec![
            Operand::Register(1),
            Operand::ConstIndex(1),
            Operand::ConstIndex(1),
            Operand::Register(0),
        ];
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        interp
            .run_iterator_static_call_operands(&mut stack, &operands)
            .unwrap();

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Iterator.from(Map) should allocate pair arrays, snapshot array, and iterator state through stack roots"
        );
        interp
            .run_iterator_next_regs(&mut stack[0], 2, 3, 1)
            .unwrap();
        assert_eq!(stack[0].registers[3], Value::Boolean(false));
        let Value::Array(pair) = stack[0].registers[2] else {
            panic!("Iterator.from(Map) should yield entry arrays");
        };
        assert_eq!(
            crate::array::get(pair, interp.gc_heap(), 0),
            Value::Number(NumberValue::from_i32(4))
        );
        assert_eq!(
            crate::array::get(pair, interp.gc_heap(), 1),
            Value::Number(NumberValue::from_i32(40))
        );
    }

    #[test]
    fn get_iterator_user_resume_uses_young_allocation_with_frame_roots() {
        let module = module_with(Vec::new(), 4);
        let mut interp = Interpreter::new();
        let iterator_obj = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();

        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.pc = 0;
        frame.pending_get_iterator = Some(PendingGetIterator { pc: 0, dst: 1 });
        frame.registers[1] = Value::Object(iterator_obj);
        stack.push(frame);
        let context = ExecutionContext::from_module(module);
        let operands = vec![Operand::Register(1), Operand::Register(0)];

        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            interp
                .drive_get_iterator(&mut stack, &context, &operands)
                .unwrap()
        );
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;

        assert!(
            after > before,
            "GetIterator resume should allocate user iterator state in young space"
        );
        assert!(matches!(stack[0].registers[1], Value::Iterator(_)));
        assert!(stack[0].pending_get_iterator.is_none());
        assert_eq!(stack[0].pc, 1);
    }

    #[test]
    fn iterator_helper_next_uses_young_allocation_with_frame_roots() {
        let module = module_with(Vec::new(), 4);
        let mut interp = Interpreter::new();
        let source = crate::array::from_elements_old_for_fixture(
            interp.gc_heap_mut(),
            [Value::Number(NumberValue::from_i32(7))],
        )
        .unwrap();

        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::Array(source);
        stack.push(frame);
        let context = ExecutionContext::from_module(module);

        interp.run_get_iterator_regs(&mut stack, 0, 1, 0).unwrap();
        let iter = match stack[0].registers[1].clone() {
            Value::Iterator(iter) => iter,
            _ => panic!("GetIterator should produce an iterator handle"),
        };
        let args: SmallVec<[Value; 8]> = SmallVec::new();

        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            interp
                .iterator_helper_dispatch(&mut stack, &context, &iter, "next", &args, 2)
                .unwrap()
        );
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Iterator helper next() should allocate its result object in young space"
        );

        let Value::Object(record) = stack[0].registers[2] else {
            panic!("Iterator helper next() should write a result object");
        };
        assert_eq!(
            object::get(record, interp.gc_heap(), "value"),
            Some(Value::Number(NumberValue::from_i32(7)))
        );
        assert_eq!(
            object::get(record, interp.gc_heap(), "done"),
            Some(Value::Boolean(false))
        );
    }

    #[test]
    fn iterator_helper_map_uses_young_allocation_with_frame_roots() {
        fn identity_mapper(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            Ok(args.first().cloned().unwrap_or(Value::Undefined))
        }

        let module = module_with(Vec::new(), 4);
        let mut interp = Interpreter::new();
        let source = crate::array::from_elements_old_for_fixture(
            interp.gc_heap_mut(),
            [Value::Number(NumberValue::from_i32(5))],
        )
        .unwrap();
        let mapper =
            native_value_static(interp.gc_heap_mut(), "identityMapper", 1, identity_mapper)
                .unwrap();

        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::Array(source);
        stack.push(frame);
        let context = ExecutionContext::from_module(module);

        interp.run_get_iterator_regs(&mut stack, 0, 1, 0).unwrap();
        let iter = match stack[0].registers[1].clone() {
            Value::Iterator(iter) => iter,
            _ => panic!("GetIterator should produce an iterator handle"),
        };
        let args: SmallVec<[Value; 8]> = smallvec::smallvec![mapper];

        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            interp
                .iterator_helper_dispatch(&mut stack, &context, &iter, "map", &args, 2)
                .unwrap()
        );
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Iterator helper map() should allocate its wrapper state in young space"
        );
        assert!(matches!(stack[0].registers[2], Value::Iterator(_)));
    }

    #[test]
    fn iterator_flat_map_inner_array_uses_runtime_rooted_iterator_allocation() {
        let module = module_with(Vec::new(), 4);
        let mut interp = Interpreter::new();
        let source = crate::array::from_elements_old_for_fixture(
            interp.gc_heap_mut(),
            [Value::Number(NumberValue::from_i32(1))],
        )
        .unwrap();
        let mapped = crate::array::from_elements_old_for_fixture(
            interp.gc_heap_mut(),
            [Value::Number(NumberValue::from_i32(99))],
        )
        .unwrap();
        let mapper = native_value_with_captures(
            interp.gc_heap_mut(),
            "returnCapturedArray",
            smallvec::smallvec![Value::Array(mapped)],
            |_ctx, _args, captures| Ok(captures.first().cloned().unwrap_or(Value::Undefined)),
        )
        .unwrap();

        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::Array(source);
        stack.push(frame);
        let context = ExecutionContext::from_module(module);

        interp.run_get_iterator_regs(&mut stack, 0, 1, 0).unwrap();
        let iter = match stack[0].registers[1].clone() {
            Value::Iterator(iter) => iter,
            _ => panic!("GetIterator should produce an iterator handle"),
        };
        let args: SmallVec<[Value; 8]> = smallvec::smallvec![mapper];
        assert!(
            interp
                .iterator_helper_dispatch(&mut stack, &context, &iter, "flatMap", &args, 2)
                .unwrap()
        );
        let flat_iter = match stack[0].registers[2].clone() {
            Value::Iterator(iter) => iter,
            _ => panic!("flatMap should return an iterator"),
        };
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        let (value, done) = interp
            .iterator_next_full(&context, &flat_iter)
            .expect("flatMap next");

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "flatMap should allocate adopted inner array iterator state through runtime roots"
        );
        assert_eq!(value, Value::Number(NumberValue::from_i32(99)));
        assert!(!done);
    }

    #[test]
    fn array_callback_map_uses_stack_rooted_result_allocation() {
        fn identity_mapper(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            Ok(args.first().cloned().unwrap_or(Value::Undefined))
        }

        let module = BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 3, Vec::new())],
            constants: vec![Constant::String {
                utf16: "map".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let source = crate::array::from_elements_old_for_fixture(
            interp.gc_heap_mut(),
            [Value::Number(NumberValue::from_i32(12))],
        )
        .unwrap();
        let mapper =
            native_value_static(interp.gc_heap_mut(), "identityMapper", 1, identity_mapper)
                .unwrap();
        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::Array(source);
        frame.registers[1] = mapper;
        stack.push(frame);
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(1),
                    Operand::Register(1),
                ],
            )
            .expect("array map");

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Array.prototype.map should allocate its result through stack roots"
        );
        let Value::Array(result) = stack[0].registers[2] else {
            panic!("map should return an array");
        };
        assert_eq!(
            crate::array::get(result, interp.gc_heap(), 0),
            Value::Number(NumberValue::from_i32(12))
        );
    }

    #[test]
    fn iterator_helper_to_array_uses_stack_rooted_result_allocation() {
        let module = module_with(Vec::new(), 4);
        let mut interp = Interpreter::new();
        let source = crate::array::from_elements_old_for_fixture(
            interp.gc_heap_mut(),
            [Value::Number(NumberValue::from_i32(21))],
        )
        .unwrap();

        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::Array(source);
        stack.push(frame);
        let context = ExecutionContext::from_module(module);

        interp.run_get_iterator_regs(&mut stack, 0, 1, 0).unwrap();
        let iter = match stack[0].registers[1].clone() {
            Value::Iterator(iter) => iter,
            _ => panic!("GetIterator should produce an iterator handle"),
        };
        let args: SmallVec<[Value; 8]> = SmallVec::new();
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        assert!(
            interp
                .iterator_helper_dispatch(&mut stack, &context, &iter, "toArray", &args, 2)
                .unwrap()
        );

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Iterator helper toArray() should allocate its result through stack roots"
        );
        let Value::Array(result) = stack[0].registers[2] else {
            panic!("toArray should return an array");
        };
        assert_eq!(
            crate::array::get(result, interp.gc_heap(), 0),
            Value::Number(NumberValue::from_i32(21))
        );
    }

    #[test]
    fn array_symbol_iterator_factory_uses_native_rooted_iterator_allocation() {
        let module = module_with(Vec::new(), 2);
        let context = ExecutionContext::from_module(module);
        let mut interp = Interpreter::new();
        let source = crate::array::from_elements_old_for_fixture(
            interp.gc_heap_mut(),
            [Value::Number(NumberValue::from_i32(21))],
        )
        .unwrap();
        let factory = make_array_iterator_factory(source, interp.gc_heap_mut()).unwrap();
        let Value::NativeFunction(native) = factory else {
            panic!("Array iterator factory should be native");
        };
        let call = native.call_target(interp.gc_heap());
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let call_info = NativeCallInfo::call(Value::Undefined);
        let mut ctx =
            NativeCtx::new_with_call_info_and_context(&mut interp, call_info, Some(context));

        let result = call.invoke(&mut ctx, &[]).expect("invoke iterator factory");

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Array[Symbol.iterator] factory should allocate iterator state through native roots"
        );
        let Value::Iterator(iter) = result else {
            panic!("factory should return an iterator");
        };
        let (array, index) = interp.gc_heap().read_payload(iter, |state| match state {
            IteratorState::Array { array, index } => (*array, *index),
            _ => panic!("factory should create an array iterator"),
        });
        assert_eq!(array, source);
        assert_eq!(index, 0);
    }

    #[test]
    fn iterator_to_list_map_pairs_use_runtime_rooted_array_allocation() {
        let module = module_with(Vec::new(), 4);
        let context = ExecutionContext::from_module(module);
        let mut interp = Interpreter::new();
        let map = crate::collections::alloc_map(interp.gc_heap_mut()).unwrap();
        crate::collections::map_set(
            map,
            interp.gc_heap_mut(),
            Value::Number(NumberValue::from_i32(3)),
            Value::Number(NumberValue::from_i32(30)),
        )
        .unwrap();
        let map_value = Value::Map(map);
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        let entries = interp
            .iterator_to_list_sync(&context, &map_value)
            .expect("map entries");

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "iterator_to_list_sync Map fast path should allocate pair arrays through runtime roots"
        );
        let Some(Value::Array(pair)) = entries.first() else {
            panic!("expected pair array");
        };
        assert_eq!(
            crate::array::get(*pair, interp.gc_heap(), 0),
            Value::Number(NumberValue::from_i32(3))
        );
        assert_eq!(
            crate::array::get(*pair, interp.gc_heap(), 1),
            Value::Number(NumberValue::from_i32(30))
        );
    }

    #[test]
    fn synthesized_iterator_next_uses_runtime_rooted_young_allocation() {
        let module = module_with(Vec::new(), 4);
        let mut interp = Interpreter::new();
        let source = crate::array::from_elements_old_for_fixture(
            interp.gc_heap_mut(),
            [Value::Number(NumberValue::from_i32(17))],
        )
        .unwrap();

        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::Array(source);
        stack.push(frame);
        let context = ExecutionContext::from_module(module);

        interp.run_get_iterator_regs(&mut stack, 0, 1, 0).unwrap();
        let iterator_value = stack[0].registers[1].clone();
        let method = interp
            .synthesize_iterator_method(&stack, "next", iterator_value)
            .unwrap();

        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let result = interp
            .run_callable_sync(&context, &method, Value::Undefined, SmallVec::new())
            .unwrap();
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "synthesized iterator next() should allocate its result object in young space"
        );

        let Value::Object(record) = result else {
            panic!("synthesized iterator next() should return a result object");
        };
        assert_eq!(
            object::get(record, interp.gc_heap(), "value"),
            Some(Value::Number(NumberValue::from_i32(17)))
        );
        assert_eq!(
            object::get(record, interp.gc_heap(), "done"),
            Some(Value::Boolean(false))
        );
    }

    #[test]
    fn iterator_result_record_uses_runtime_rooted_young_allocation() {
        let mut interp = Interpreter::new();
        let value = Value::Number(NumberValue::from_i32(44));
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        let result = interp
            .make_runtime_rooted_iter_result(value.clone(), true, &[], &[])
            .unwrap();

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "IteratorResult records should allocate through runtime roots"
        );
        let Value::Object(record) = result else {
            panic!("IteratorResult should be an object");
        };
        assert_eq!(object::get(record, interp.gc_heap(), "value"), Some(value));
        assert_eq!(
            object::get(record, interp.gc_heap(), "done"),
            Some(Value::Boolean(true))
        );
    }

    #[test]
    fn new_collection_map_uses_root_aware_allocation_with_frame_roots() {
        let mut interp = Interpreter::new();
        let pair = crate::array::from_elements_old_for_fixture(
            interp.gc_heap_mut(),
            [
                Value::Number(NumberValue::from_i32(1)),
                Value::Number(NumberValue::from_i32(10)),
            ],
        )
        .unwrap();
        let seed =
            crate::array::from_elements_old_for_fixture(interp.gc_heap_mut(), [Value::Array(pair)])
                .unwrap();
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 3, Vec::new())],
            constants: vec![Constant::String {
                utf16: "Map".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[1] = Value::Array(seed);
        stack.push(frame);

        let before_alloc = interp.gc_heap_mut().stats().new_allocated_bytes;
        let before_reserved = interp.gc_heap_mut().stats().reserved_bytes;
        interp
            .run_new_collection_regs(&context, &mut stack, 0, 0, 0, 1)
            .unwrap();
        let after_alloc = interp.gc_heap_mut().stats().new_allocated_bytes;
        let after_reserved = interp.gc_heap_mut().stats().reserved_bytes;

        assert!(
            after_alloc > before_alloc,
            "NewCollection Map should allocate the map body in young space"
        );
        assert!(
            after_reserved > before_reserved,
            "NewCollection Map should reserve backing storage through the root-aware path"
        );
        let Value::Map(map) = stack[0].registers[0] else {
            panic!("NewCollection Map should write a Map");
        };
        assert_eq!(
            crate::collections::map_get(
                map,
                interp.gc_heap(),
                &Value::Number(NumberValue::from_i32(1))
            ),
            Some(Value::Number(NumberValue::from_i32(10)))
        );
    }

    #[test]
    fn bytecode_new_error_uses_young_allocation_with_frame_roots() {
        let module = module_with(
            vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadUndefined,
                    operands: vec![Operand::Register(1)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::NewError,
                    operands: vec![Operand::Register(0), Operand::Register(1)].into(),
                },
                Instruction {
                    pc: 2,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)].into(),
                },
            ],
            2,
        );
        let mut interp = Interpreter::new();
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let context = ExecutionContext::from_module(module);
        let Value::Object(obj) = interp.run(&context).unwrap() else {
            panic!("NewError should return an object");
        };
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "NewError should allocate the error instance in young space"
        );
        assert!(crate::object::get_own_descriptor(obj, interp.gc_heap(), "message").is_none());
    }

    #[test]
    fn vm_error_throwable_uses_stack_rooted_allocation() {
        let module = module_with(Vec::new(), 1);
        let mut interp = Interpreter::new();
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(Frame::for_function(&module.functions[0]));
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        let Some(Value::Object(error)) =
            interp.vm_error_to_throwable_with_stack_roots(&stack, &VmError::TypeMismatch)
        else {
            panic!("TypeMismatch should convert to a throwable object");
        };

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "VM error throwable conversion should allocate through stack roots"
        );
        assert!(matches!(
            object::get(error, interp.gc_heap(), "message"),
            Some(Value::String(message)) if message
                .to_lossy_string()
                .contains("type mismatch")
        ));
    }

    #[test]
    fn host_rooted_object_and_array_helpers_use_young_allocation() {
        let mut interp = Interpreter::new();
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        let host = interp
            .alloc_host_object_with_roots(&[], &[])
            .expect("host object allocation");
        let host_root = Value::Object(host);
        let elements = [Value::Number(NumberValue::from_i32(1))];
        let array = interp
            .array_from_elements_host_rooted(
                elements.iter().cloned(),
                &[&host_root],
                &[elements.as_slice()],
            )
            .expect("host array allocation");
        object::set(host, interp.gc_heap_mut(), "items", Value::Array(array));

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "host-rooted object and array helpers should allocate in young space"
        );
        assert!(matches!(
            object::get(host, interp.gc_heap(), "items"),
            Some(Value::Array(_))
        ));
    }

    #[test]
    fn json_parse_uses_stack_rooted_container_allocation() {
        let module = module_with(Vec::new(), 3);
        let mut interp = Interpreter::new();
        let input = Value::String(
            JsString::from_str("{\"items\":[1,{\"nested\":2}]}", &interp.string_heap).unwrap(),
        );
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = input;
        stack.push(frame);
        let operands = vec![
            Operand::Register(1),
            Operand::ConstIndex(0),
            Operand::ConstIndex(1),
            Operand::Register(0),
        ];
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        interp
            .run_json_static_call_operands(&mut stack, operands.as_slice())
            .expect("JSON.parse");

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "JSON.parse should allocate result containers through stack roots"
        );
        let Value::Object(obj) = stack[0].registers[1] else {
            panic!("JSON.parse should return an object");
        };
        let Some(Value::Array(items)) = object::get(obj, interp.gc_heap(), "items") else {
            panic!("parsed object should contain items array");
        };
        assert_eq!(
            array::get(items, interp.gc_heap(), 0),
            Value::Number(NumberValue::from_i32(1))
        );
    }

    #[test]
    fn bytecode_new_weak_ref_uses_young_allocation_with_frame_roots() {
        let module = module_with(
            vec![
                Instruction {
                    pc: 0,
                    op: Op::NewObject,
                    operands: vec![Operand::Register(1)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::NewWeakRef,
                    operands: vec![Operand::Register(0), Operand::Register(1)].into(),
                },
                Instruction {
                    pc: 2,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)].into(),
                },
            ],
            2,
        );
        let mut interp = Interpreter::new();
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let context = ExecutionContext::from_module(module);
        assert!(matches!(interp.run(&context).unwrap(), Value::WeakRef(_)));
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "NewWeakRef should allocate the weak-ref body in young space"
        );
    }

    #[test]
    fn bytecode_new_finalization_registry_uses_young_allocation_with_frame_roots() {
        let cleanup = test_function(1, "cleanup", 1, 1, Vec::new());
        let main_code = vec![
            Instruction {
                pc: 0,
                op: Op::MakeFunction,
                operands: vec![Operand::Register(1), Operand::ConstIndex(0)].into(),
            },
            Instruction {
                pc: 1,
                op: Op::NewFinalizationRegistry,
                operands: vec![Operand::Register(0), Operand::Register(1)].into(),
            },
            Instruction {
                pc: 2,
                op: Op::Return,
                operands: vec![Operand::Register(0)].into(),
            },
        ];
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, main_code), cleanup],
            constants: vec![Constant::FunctionId { index: 1 }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let context = ExecutionContext::from_module(module);
        assert!(matches!(
            interp.run(&context).unwrap(),
            Value::FinalizationRegistry(_)
        ));
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "NewFinalizationRegistry should allocate the registry body in young space"
        );
    }

    #[test]
    fn direct_bytecode_async_call_window_populates_parameters() {
        let mut callee = test_function(
            1,
            "async_callee",
            1,
            1,
            vec![Instruction {
                pc: 0,
                op: Op::Return,
                operands: vec![Operand::Register(0)].into(),
            }],
        );
        callee.is_async = true;
        let main_code = vec![
            Instruction {
                pc: 0,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(1), Operand::Imm32(144)].into(),
            },
            Instruction {
                pc: 1,
                op: Op::MakeFunction,
                operands: vec![Operand::Register(2), Operand::ConstIndex(0)].into(),
            },
            Instruction {
                pc: 2,
                op: Op::Call,
                operands: vec![
                    Operand::Register(0),
                    Operand::Register(2),
                    Operand::ConstIndex(1),
                    Operand::Register(1),
                ]
                .into(),
            },
            Instruction {
                pc: 3,
                op: Op::Return,
                operands: vec![Operand::Register(0)].into(),
            },
        ];
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 3, main_code), callee],
            constants: vec![Constant::FunctionId { index: 1 }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let context = ExecutionContext::from_module(module);
        let Value::Promise(promise) = interp.run(&context).unwrap() else {
            panic!("expected async function call to return a promise");
        };
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "async bytecode calls should allocate their result promise in young space"
        );
        assert_eq!(
            promise.state(interp.gc_heap()),
            crate::promise::PromiseState::Fulfilled(Value::Number(NumberValue::Smi(144)))
        );
    }

    #[test]
    fn async_generator_method_uses_stack_rooted_capability_allocation() {
        let main = test_function(0, "<main>", 0, 1, Vec::new());
        let generator_body = test_function(
            1,
            "async_generator_body",
            0,
            1,
            vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadInt32,
                    operands: vec![Operand::Register(0), Operand::Imm32(91)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)].into(),
                },
            ],
        );
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![main.clone(), generator_body.clone()],
            constants: vec![Constant::String {
                utf16: "next".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let context = ExecutionContext::from_module(module);
        let mut interp = Interpreter::new();
        let body_frame = Frame::for_function(&generator_body);
        let generator =
            crate::generator::JsGenerator::new(interp.gc_heap_mut(), body_frame).expect("gen");
        generator.set_async(interp.gc_heap_mut(), true);
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&main);
        frame.registers[0] = Value::Generator(generator);
        stack.push(frame);

        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(0),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect("async generator next");
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;

        assert!(
            after > before,
            "async generator method should allocate its pending capability through stack roots"
        );
        assert!(matches!(stack[0].registers[0], Value::Promise(_)));
    }

    #[test]
    fn primitive_wrapper_boxing_uses_stack_rooted_young_allocation() {
        let main = test_function(0, "<main>", 0, 1, Vec::new());
        let callee = test_function(1, "sloppy_callee", 0, 1, Vec::new());
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![main.clone(), callee],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let context = ExecutionContext::from_module(module);
        let mut interp = Interpreter::new();
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(Frame::for_function(&main));
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        let boxed_this = interp
            .this_for_bytecode_call_stack_rooted(
                context.exec_function(1).expect("callee"),
                &stack,
                Value::Number(NumberValue::from_i32(33)),
                &[],
            )
            .expect("boxed this");
        let primitive_string =
            Value::String(crate::JsString::from_str("abc", &interp.string_heap).unwrap());
        let property_base = interp
            .object_for_primitive_property_base_stack_rooted(&stack, &primitive_string)
            .expect("property base")
            .expect("primitive base");

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "primitive wrapper boxing should allocate through stack-rooted young allocation"
        );
        assert!(matches!(boxed_this, Value::Object(_)));
        assert!(matches!(Value::Object(property_base), Value::Object(_)));
    }

    #[test]
    fn top_level_async_entry_uses_stack_rooted_result_promise_allocation() {
        let mut main = test_function(
            0,
            "<main>",
            0,
            1,
            vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadInt32,
                    operands: vec![Operand::Register(0), Operand::Imm32(512)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)].into(),
                },
            ],
        );
        main.is_async = true;
        main.is_module = true;
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![main],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let context = ExecutionContext::from_module(module);
        assert_eq!(
            interp.run(&context).unwrap(),
            Value::Number(NumberValue::Smi(512))
        );
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "async entry promise should allocate through stack-rooted young allocation"
        );
    }

    #[test]
    fn promise_fulfilled_of_uses_young_allocation_with_frame_roots() {
        let main_code = vec![
            Instruction {
                pc: 0,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(1), Operand::Imm32(211)].into(),
            },
            Instruction {
                pc: 1,
                op: Op::PromiseFulfilledOf,
                operands: vec![Operand::Register(0), Operand::Register(1)].into(),
            },
            Instruction {
                pc: 2,
                op: Op::Return,
                operands: vec![Operand::Register(0)].into(),
            },
        ];
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, main_code)],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let context = ExecutionContext::from_module(module);
        let Value::Promise(promise) = interp.run(&context).unwrap() else {
            panic!("expected promise");
        };
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "PromiseFulfilledOf should allocate the promise body in young space"
        );
        assert_eq!(
            promise.state(interp.gc_heap()),
            crate::promise::PromiseState::Fulfilled(Value::Number(NumberValue::Smi(211)))
        );
    }

    #[test]
    fn await_non_promise_uses_stack_rooted_wrapper_allocation() {
        let mut function = test_function(0, "async_body", 0, 1, Vec::new());
        function.is_async = true;
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![function],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let result_promise = {
            let mut external_visit = |_visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {};
            JsPromiseHandle::pending_with_roots(interp.gc_heap_mut(), &mut external_visit)
                .expect("result promise")
        };
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.async_state = Some(AsyncFrameState { result_promise });
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(frame);
        let context = ExecutionContext::from_module(module);
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        interp
            .do_await(
                &mut stack,
                &context,
                0,
                Value::Number(NumberValue::Smi(307)),
            )
            .expect("await");

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Await of a non-promise should wrap through stack-rooted young allocation"
        );
        assert!(stack.is_empty(), "await should park the active frame");
    }

    #[test]
    fn promise_new_uses_stack_rooted_capability_allocation() {
        fn executor(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
            Ok(Value::Undefined)
        }

        let module = module_with(Vec::new(), 3);
        let context = ExecutionContext::from_module(module.clone());
        let mut interp = Interpreter::new();
        let executor_value =
            native_value_static(interp.gc_heap_mut(), "executor", 2, executor).expect("executor");
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[1] = executor_value;
        stack.push(frame);
        let operands = vec![
            Operand::Register(0),
            Operand::Register(1),
            Operand::Register(2),
        ];
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        interp
            .run_promise_new_operands(&context, &mut stack, operands.as_slice())
            .expect("PromiseNew");

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "PromiseNew should allocate its promise/capability through stack-rooted young allocation"
        );
        assert!(matches!(stack[0].registers[0], Value::Promise(_)));
    }

    #[test]
    fn dynamic_import_rejection_uses_stack_rooted_promise_allocation() {
        let module = module_with(Vec::new(), 2);
        let context = ExecutionContext::from_module(module.clone());
        let mut interp = Interpreter::new();
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[1] = Value::Number(NumberValue::Smi(12));
        stack.push(frame);
        let operands = vec![Operand::Register(0), Operand::Register(1)];
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        interp
            .run_import_namespace_dynamic_operands(&context, &mut stack, 0, operands.as_slice())
            .expect("dynamic import");

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "dynamic import rejection should allocate the TypeError and promise body through stack roots"
        );
        let Value::Promise(promise) = stack[0].registers[0] else {
            panic!("expected promise");
        };
        let crate::promise::PromiseState::Rejected(Value::Object(reason)) =
            promise.state(interp.gc_heap())
        else {
            panic!("expected TypeError rejection object");
        };
        assert!(matches!(
            object::get(reason, interp.gc_heap(), "message"),
            Some(Value::String(message)) if message
                .to_lossy_string()
                .contains("specifier must be a string")
        ));
    }

    #[test]
    fn direct_bytecode_construct_window_populates_arguments_object() {
        let mut ctor = test_function(
            1,
            "Ctor",
            0,
            1,
            vec![
                Instruction {
                    pc: 0,
                    op: Op::CollectArguments,
                    operands: vec![Operand::Register(0)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)].into(),
                },
            ],
        );
        ctor.needs_arguments = true;
        let main_code = vec![
            Instruction {
                pc: 0,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(1), Operand::Imm32(55)].into(),
            },
            Instruction {
                pc: 1,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(2), Operand::Imm32(89)].into(),
            },
            Instruction {
                pc: 2,
                op: Op::MakeFunction,
                operands: vec![Operand::Register(3), Operand::ConstIndex(0)].into(),
            },
            Instruction {
                pc: 3,
                op: Op::New,
                operands: vec![
                    Operand::Register(0),
                    Operand::Register(3),
                    Operand::ConstIndex(2),
                    Operand::Register(2),
                    Operand::Register(1),
                ]
                .into(),
            },
            Instruction {
                pc: 4,
                op: Op::Return,
                operands: vec![Operand::Register(0)].into(),
            },
        ];
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 4, main_code), ctor],
            constants: vec![Constant::FunctionId { index: 1 }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let context = ExecutionContext::from_module(module);
        let Value::Object(args) = interp.run(&context).unwrap() else {
            panic!("expected constructor-returned arguments object");
        };
        assert_eq!(
            object::get(args, interp.gc_heap(), "0"),
            Some(Value::Number(NumberValue::Smi(89)))
        );
        assert_eq!(
            object::get(args, interp.gc_heap(), "1"),
            Some(Value::Number(NumberValue::Smi(55)))
        );
    }

    #[test]
    fn direct_bytecode_construct_receiver_uses_young_allocation_with_frame_roots() {
        let ctor = test_function(
            1,
            "Ctor",
            0,
            1,
            vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadThis,
                    operands: vec![Operand::Register(0)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)].into(),
                },
            ],
        );
        let main_code = vec![
            Instruction {
                pc: 0,
                op: Op::MakeFunction,
                operands: vec![Operand::Register(1), Operand::ConstIndex(0)].into(),
            },
            Instruction {
                pc: 1,
                op: Op::New,
                operands: vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(0),
                ]
                .into(),
            },
            Instruction {
                pc: 2,
                op: Op::Return,
                operands: vec![Operand::Register(0)].into(),
            },
        ];
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, main_code), ctor],
            constants: vec![Constant::FunctionId { index: 1 }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let context = ExecutionContext::from_module(module);
        assert!(matches!(interp.run(&context).unwrap(), Value::Object(_)));
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "ordinary bytecode constructor receiver should allocate in young space"
        );
    }

    #[test]
    fn bound_bytecode_construct_receiver_uses_young_allocation_with_frame_roots() {
        let ctor = test_function(
            1,
            "Ctor",
            0,
            1,
            vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadThis,
                    operands: vec![Operand::Register(0)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)].into(),
                },
            ],
        );
        let main_code = vec![
            Instruction {
                pc: 0,
                op: Op::MakeFunction,
                operands: vec![Operand::Register(1), Operand::ConstIndex(0)].into(),
            },
            Instruction {
                pc: 1,
                op: Op::LoadUndefined,
                operands: vec![Operand::Register(2)].into(),
            },
            Instruction {
                pc: 2,
                op: Op::BindFunction,
                operands: vec![
                    Operand::Register(3),
                    Operand::Register(1),
                    Operand::Register(2),
                    Operand::ConstIndex(0),
                ]
                .into(),
            },
            Instruction {
                pc: 3,
                op: Op::New,
                operands: vec![
                    Operand::Register(0),
                    Operand::Register(3),
                    Operand::ConstIndex(0),
                ]
                .into(),
            },
            Instruction {
                pc: 4,
                op: Op::Return,
                operands: vec![Operand::Register(0)].into(),
            },
        ];
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 4, main_code), ctor],
            constants: vec![Constant::FunctionId { index: 1 }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let context = ExecutionContext::from_module(module);
        assert!(matches!(interp.run(&context).unwrap(), Value::Object(_)));
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "bound bytecode constructor receiver should allocate in young space"
        );
    }

    #[test]
    fn runtime_budget_stats_record_reductions_and_budget_observations() {
        let module = module_with(
            vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadUndefined,
                    operands: vec![Operand::Register(0)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)].into(),
                },
            ],
            1,
        );
        let mut interp = Interpreter::new();
        interp.set_runtime_budget(RuntimeBudget {
            max_reductions_per_turn: Some(1),
            ..RuntimeBudget::default()
        });
        let context = ExecutionContext::from_module(module);
        assert_eq!(interp.run(&context).unwrap(), Value::Undefined);
        let stats = interp.runtime_budget_stats();
        assert_eq!(stats.turns_started, 1);
        assert_eq!(stats.turns_finished, 1);
        assert!(stats.reductions_executed >= 2);
        assert!(stats.max_turn_reductions >= 2);
        assert_eq!(stats.budget_limit_observations, 1);
        assert_eq!(stats.max_stack_depth_observed, 1);
    }

    #[test]
    fn runtime_budget_can_reject_on_reduction_limit() {
        let module = module_with(
            vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadUndefined,
                    operands: vec![Operand::Register(0)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)].into(),
                },
            ],
            1,
        );
        let mut interp = Interpreter::new();
        interp.set_runtime_budget(RuntimeBudget {
            on_exceeded: RuntimeBudgetExceededAction::Reject,
            max_reductions_per_turn: Some(0),
            ..RuntimeBudget::default()
        });
        let context = ExecutionContext::from_module(module);
        let err = interp.run(&context).unwrap_err();
        assert!(matches!(err.error, VmError::BudgetExceeded { .. }));
        let stats = interp.runtime_budget_stats();
        assert_eq!(stats.budget_rejections, 1);
        assert_eq!(stats.budget_limit_observations, 1);
    }

    #[test]
    fn runtime_budget_stats_record_heap_allocations() {
        let module = module_with(
            vec![
                Instruction {
                    pc: 0,
                    op: Op::NewObject,
                    operands: vec![Operand::Register(0)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)].into(),
                },
            ],
            1,
        );
        let mut interp = Interpreter::new();
        let context = ExecutionContext::from_module(module);
        assert!(matches!(interp.run(&context).unwrap(), Value::Object(_)));
        let stats = interp.runtime_budget_stats();
        assert!(stats.allocated_objects_observed >= 1);
        assert!(stats.allocated_bytes_observed > 0);
        assert!(stats.max_turn_allocated_bytes > 0);
        assert!(stats.max_live_heap_bytes > 0);
    }

    #[test]
    fn bytecode_new_object_uses_young_allocation_with_frame_roots() {
        let module = module_with(
            vec![
                Instruction {
                    pc: 0,
                    op: Op::NewObject,
                    operands: vec![Operand::Register(0)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)].into(),
                },
            ],
            1,
        );
        let mut interp = Interpreter::new();
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let context = ExecutionContext::from_module(module);
        assert!(matches!(interp.run(&context).unwrap(), Value::Object(_)));
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "NewObject should allocate the object body in young space"
        );
    }

    #[test]
    fn bytecode_new_array_uses_young_allocation_with_frame_roots() {
        let module = module_with(
            vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadInt32,
                    operands: vec![Operand::Register(0), Operand::Imm32(42)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::NewArray,
                    operands: vec![
                        Operand::Register(1),
                        Operand::ConstIndex(1),
                        Operand::Register(0),
                    ]
                    .into(),
                },
                Instruction {
                    pc: 2,
                    op: Op::Return,
                    operands: vec![Operand::Register(1)].into(),
                },
            ],
            2,
        );
        let mut interp = Interpreter::new();
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let context = ExecutionContext::from_module(module);
        assert!(matches!(interp.run(&context).unwrap(), Value::Array(_)));
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "NewArray should allocate the array body in young space"
        );
    }

    #[test]
    fn bytecode_array_push_uses_root_aware_growth_with_frame_roots() {
        let module = module_with(
            vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadInt32,
                    operands: vec![Operand::Register(0), Operand::Imm32(1)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::LoadInt32,
                    operands: vec![Operand::Register(1), Operand::Imm32(2)].into(),
                },
                Instruction {
                    pc: 2,
                    op: Op::LoadInt32,
                    operands: vec![Operand::Register(2), Operand::Imm32(3)].into(),
                },
                Instruction {
                    pc: 3,
                    op: Op::LoadInt32,
                    operands: vec![Operand::Register(3), Operand::Imm32(4)].into(),
                },
                Instruction {
                    pc: 4,
                    op: Op::NewArray,
                    operands: vec![
                        Operand::Register(4),
                        Operand::ConstIndex(4),
                        Operand::Register(0),
                        Operand::Register(1),
                        Operand::Register(2),
                        Operand::Register(3),
                    ]
                    .into(),
                },
                Instruction {
                    pc: 5,
                    op: Op::LoadInt32,
                    operands: vec![Operand::Register(5), Operand::Imm32(5)].into(),
                },
                Instruction {
                    pc: 6,
                    op: Op::ArrayPush,
                    operands: vec![Operand::Register(4), Operand::Register(5)].into(),
                },
                Instruction {
                    pc: 7,
                    op: Op::Return,
                    operands: vec![Operand::Register(4)].into(),
                },
            ],
            6,
        );
        let mut interp = Interpreter::new();
        let before = interp.gc_heap_mut().stats().reserved_bytes;
        let context = ExecutionContext::from_module(module);
        let result = interp.run(&context).unwrap();
        let Value::Array(array) = result else {
            panic!("ArrayPush program should return the grown array");
        };
        let values =
            crate::array::with_elements(array, interp.gc_heap_mut(), |elements| elements.to_vec());
        assert_eq!(values.len(), 5);
        assert_eq!(values[4], Value::Number(NumberValue::from_i32(5)));
        let after = interp.gc_heap_mut().stats().reserved_bytes;
        assert!(
            after > before,
            "ArrayPush should reserve dense backing storage through the root-aware path"
        );
    }

    #[test]
    fn bytecode_store_element_uses_root_aware_growth_with_frame_roots() {
        let module = module_with(
            vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadInt32,
                    operands: vec![Operand::Register(0), Operand::Imm32(1)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::LoadInt32,
                    operands: vec![Operand::Register(1), Operand::Imm32(2)].into(),
                },
                Instruction {
                    pc: 2,
                    op: Op::LoadInt32,
                    operands: vec![Operand::Register(2), Operand::Imm32(3)].into(),
                },
                Instruction {
                    pc: 3,
                    op: Op::LoadInt32,
                    operands: vec![Operand::Register(3), Operand::Imm32(4)].into(),
                },
                Instruction {
                    pc: 4,
                    op: Op::NewArray,
                    operands: vec![
                        Operand::Register(4),
                        Operand::ConstIndex(4),
                        Operand::Register(0),
                        Operand::Register(1),
                        Operand::Register(2),
                        Operand::Register(3),
                    ]
                    .into(),
                },
                Instruction {
                    pc: 5,
                    op: Op::LoadInt32,
                    operands: vec![Operand::Register(5), Operand::Imm32(4)].into(),
                },
                Instruction {
                    pc: 6,
                    op: Op::LoadInt32,
                    operands: vec![Operand::Register(6), Operand::Imm32(99)].into(),
                },
                Instruction {
                    pc: 7,
                    op: Op::StoreElement,
                    operands: vec![
                        Operand::Register(4),
                        Operand::Register(5),
                        Operand::Register(6),
                        Operand::Register(7),
                    ]
                    .into(),
                },
                Instruction {
                    pc: 8,
                    op: Op::Return,
                    operands: vec![Operand::Register(4)].into(),
                },
            ],
            8,
        );
        let mut interp = Interpreter::new();
        let before = interp.gc_heap_mut().stats().reserved_bytes;
        let context = ExecutionContext::from_module(module);
        let result = interp.run(&context).unwrap();
        let Value::Array(array) = result else {
            panic!("StoreElement program should return the grown array");
        };
        let values =
            crate::array::with_elements(array, interp.gc_heap_mut(), |elements| elements.to_vec());
        assert_eq!(values.len(), 5);
        assert_eq!(values[4], Value::Number(NumberValue::from_i32(99)));
        let after = interp.gc_heap_mut().stats().reserved_bytes;
        assert!(
            after > before,
            "StoreElement should reserve dense backing storage through the root-aware path"
        );
    }

    #[test]
    fn runtime_budget_stats_record_host_ops_and_external_bytes() {
        let mut interp = Interpreter::new();
        interp.set_runtime_budget(RuntimeBudget {
            max_host_ops_per_turn: Some(0),
            max_external_bytes: Some(0),
            ..RuntimeBudget::default()
        });

        interp.begin_runtime_budget_turn();
        interp.record_runtime_host_op_enqueued();
        let external = interp.gc_heap_mut().reserve_external(64).unwrap();
        interp.finish_runtime_budget_turn();

        let stats = interp.runtime_budget_stats();
        assert_eq!(stats.host_ops_enqueued, 1);
        assert_eq!(stats.max_turn_host_ops, 1);
        assert!(stats.max_external_bytes_observed >= 64);
        assert!(stats.budget_limit_observations >= 1);
        drop(external);
    }

    #[test]
    fn missing_return_errors() {
        let module = module_with(
            vec![Instruction {
                pc: 0,
                op: Op::Nop,
                operands: vec![].into(),
            }],
            0,
        );
        let mut interp = Interpreter::new();
        let context = ExecutionContext::from_module(module);
        assert_eq!(
            interp.run(&context).unwrap_err().error,
            VmError::MissingReturn
        );
    }

    #[test]
    fn unwind_throw_pops_frames_until_handler_or_uncaught() {
        // No handlers anywhere in the stack: the throw escapes as
        // VmError::Uncaught carrying the rendered value.
        let main = Function {
            id: 0,
            name: "<main>".to_string(),
            span: (0, 0),
            locals: 0,
            scratch: 1,
            param_count: 0,
            own_upvalue_count: 0,
            is_strict: false,
            is_arrow: false,
            has_rest: false,
            is_async: false,
            is_generator: false,
            is_async_generator: false,
            is_module: false,
            needs_arguments: false,
            arguments_object_kind: ArgumentsObjectKind::Unmapped,
            mapped_argument_bindings: Vec::new(),
            module_url: String::new(),
            code: vec![Instruction {
                pc: 0,
                op: Op::ReturnUndefined,
                operands: vec![].into(),
            }],
            spans: vec![SpanEntry {
                pc: 0,
                span: (0, 0),
            }],
        };
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(Frame::for_function(&main));
        // Push a second frame on top — should be popped during
        // unwinding and not absorb the throw.
        stack.push(Frame::for_function(&main));
        let mut interp = Interpreter::new();
        let err = interp
            .unwind_throw(&mut stack, Value::Boolean(true))
            .unwrap_err();
        match err {
            VmError::Uncaught { value } => assert_eq!(value, "true"),
            other => panic!("expected Uncaught, got {other:?}"),
        }
        assert!(stack.is_empty(), "frames should be drained on uncaught");
    }

    #[test]
    fn unwind_throw_lands_in_catch_handler() {
        let main = Function {
            id: 0,
            name: "<main>".to_string(),
            span: (0, 0),
            locals: 0,
            scratch: 2,
            param_count: 0,
            own_upvalue_count: 0,
            is_strict: false,
            is_arrow: false,
            has_rest: false,
            is_async: false,
            is_generator: false,
            is_async_generator: false,
            is_module: false,
            needs_arguments: false,
            arguments_object_kind: ArgumentsObjectKind::Unmapped,
            mapped_argument_bindings: Vec::new(),
            module_url: String::new(),
            code: vec![Instruction {
                pc: 0,
                op: Op::ReturnUndefined,
                operands: vec![].into(),
            }],
            spans: vec![SpanEntry {
                pc: 0,
                span: (0, 0),
            }],
        };
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&main);
        frame.handlers.push(TryHandler {
            catch_pc: Some(42),
            finally_pc: None,
            exc_register: 1,
        });
        stack.push(frame);
        let mut interp = Interpreter::new();
        interp
            .unwind_throw(&mut stack, Value::Boolean(true))
            .unwrap();
        assert_eq!(stack[0].pc, 42);
        assert_eq!(stack[0].registers[1], Value::Boolean(true));
        assert!(stack[0].handlers.is_empty());
    }

    #[test]
    fn is_callable_recognises_call_shapes() {
        assert!(is_callable(&Value::Function { function_id: 7 }));
        assert!(is_callable(&Value::Closure {
            function_id: 7,
            upvalues: Frame::empty_upvalues(),
            bound_this: None,
        }));
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let bound = BoundFunction::new(
            &mut heap,
            Value::Function { function_id: 7 },
            Value::Undefined,
            SmallVec::new(),
        )
        .expect("bound");
        assert!(is_callable(&Value::BoundFunction(bound)));
        assert!(!is_callable(&Value::Number(NumberValue::Smi(1))));
        assert!(!is_callable(&Value::Object(
            crate::object::alloc_object_old_for_fixture(&mut heap).unwrap()
        )));
    }

    #[test]
    fn native_call_context_receives_method_receiver() {
        fn return_this(ctx: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
            Ok(ctx.this_value().clone())
        }

        let module = module_with(vec![], 1);
        let mut interp = Interpreter::new();
        let callee = native_value_static(interp.gc_heap_mut(), "returnThis", 0, return_this)
            .expect("native");
        let receiver = Value::Object(
            crate::object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap(),
        );
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(Frame::for_function(&module.functions[0]));
        let context = ExecutionContext::from_module(module.clone());

        interp
            .invoke(
                &mut stack,
                &context,
                &callee,
                receiver.clone(),
                SmallVec::new(),
                0,
            )
            .unwrap();

        assert_eq!(stack[0].registers[0], receiver);
    }

    #[test]
    fn direct_native_call_uses_contiguous_argument_window() {
        fn sum_smi_args(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            let mut sum = 0;
            for arg in args {
                match arg {
                    Value::Number(NumberValue::Smi(n)) => sum += *n,
                    _ => {
                        return Err(NativeError::TypeError {
                            name: "sum",
                            reason: "expected smi".to_string(),
                        });
                    }
                }
            }
            Ok(Value::Number(NumberValue::Smi(sum)))
        }

        let module = module_with(vec![], 4);
        let mut interp = Interpreter::new();
        let callee =
            native_value_static(interp.gc_heap_mut(), "sum", 2, sum_smi_args).expect("native");
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = callee;
        frame.registers[1] = Value::Number(NumberValue::Smi(8));
        frame.registers[2] = Value::Number(NumberValue::Smi(13));
        stack.push(frame);
        let context = ExecutionContext::from_module(module.clone());
        let operands = vec![
            Operand::Register(3),
            Operand::Register(0),
            Operand::ConstIndex(2),
            Operand::Register(1),
            Operand::Register(2),
        ];

        interp.do_call(&mut stack, &context, &operands).unwrap();

        assert_eq!(stack[0].registers[3], Value::Number(NumberValue::Smi(21)));
    }

    #[test]
    fn proxy_revocable_uses_stack_rooted_result_allocation() {
        let module = module_with(vec![], 4);
        let mut interp = Interpreter::new();
        let target = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();
        let handler = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();

        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::Object(target);
        frame.registers[1] = Value::Object(handler);
        stack.push(frame);
        let operands = vec![
            Operand::Register(2),
            Operand::ConstIndex(1),
            Operand::ConstIndex(2),
            Operand::Register(0),
            Operand::Register(1),
        ];
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        interp
            .run_proxy_static_call_operands(&mut stack, &operands)
            .unwrap();

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Proxy.revocable should allocate revoke function and result object through stack roots"
        );
        let Value::Object(record) = stack[0].registers[2] else {
            panic!("Proxy.revocable should return an object");
        };
        assert!(matches!(
            object::get(record, interp.gc_heap(), "proxy"),
            Some(Value::Proxy(_))
        ));
        assert!(matches!(
            object::get(record, interp.gc_heap(), "revoke"),
            Some(Value::NativeFunction(_))
        ));
    }

    #[test]
    fn object_entries_uses_stack_rooted_result_allocation() {
        let module = module_with(vec![], 5);
        let mut interp = Interpreter::new();
        let target = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();
        object::set(
            target,
            interp.gc_heap_mut(),
            "answer",
            Value::Number(NumberValue::Smi(42)),
        );

        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::Object(target);
        stack.push(frame);
        let context = ExecutionContext::from_module(module);
        let operands = vec![
            Operand::Register(1),
            Operand::ConstIndex(4),
            Operand::ConstIndex(1),
            Operand::Register(0),
        ];
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        interp
            .run_object_static_call_operands(&context, &mut stack, &operands)
            .unwrap();

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Object.entries should allocate pair and result arrays through stack roots"
        );
        let Value::Array(entries) = stack[0].registers[1] else {
            panic!("Object.entries should return an array");
        };
        let Value::Array(pair) = array::get(entries, interp.gc_heap(), 0) else {
            panic!("Object.entries should contain pair arrays");
        };
        assert!(matches!(
            array::get(pair, interp.gc_heap(), 0),
            Value::String(name) if name.to_lossy_string() == "answer"
        ));
        assert_eq!(
            array::get(pair, interp.gc_heap(), 1),
            Value::Number(NumberValue::Smi(42))
        );
    }

    #[test]
    fn object_get_own_property_descriptors_uses_stack_rooted_allocation() {
        let module = module_with(vec![], 5);
        let mut interp = Interpreter::new();
        let target = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();
        let descriptor =
            object::PropertyDescriptor::data(Value::Number(NumberValue::Smi(7)), true, false, true);
        assert!(object::define_own_property(
            target,
            interp.gc_heap_mut(),
            "answer",
            descriptor,
        ));

        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::Object(target);
        stack.push(frame);
        let context = ExecutionContext::from_module(module);
        let operands = vec![
            Operand::Register(1),
            Operand::ConstIndex(8),
            Operand::ConstIndex(1),
            Operand::Register(0),
        ];
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        interp
            .run_object_static_call_operands(&context, &mut stack, &operands)
            .unwrap();

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Object.getOwnPropertyDescriptors should allocate result and descriptor objects through stack roots"
        );
        let Value::Object(result) = stack[0].registers[1] else {
            panic!("Object.getOwnPropertyDescriptors should return an object");
        };
        let Some(Value::Object(desc_obj)) = object::get(result, interp.gc_heap(), "answer") else {
            panic!("Object.getOwnPropertyDescriptors should expose a descriptor object");
        };
        assert_eq!(
            object::get(desc_obj, interp.gc_heap(), "value"),
            Some(Value::Number(NumberValue::Smi(7)))
        );
        assert_eq!(
            object::get(desc_obj, interp.gc_heap(), "enumerable"),
            Some(Value::Boolean(false))
        );
    }

    #[test]
    fn object_from_entries_uses_stack_rooted_result_allocation() {
        let module = module_with(vec![], 5);
        let mut interp = Interpreter::new();
        let key = Value::String(JsString::from_str("answer", &interp.string_heap).unwrap());
        let pair = array::from_elements_old_for_fixture(
            interp.gc_heap_mut(),
            vec![key, Value::Number(NumberValue::Smi(9))],
        )
        .unwrap();
        let entries =
            array::from_elements_old_for_fixture(interp.gc_heap_mut(), vec![Value::Array(pair)])
                .unwrap();

        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::Array(entries);
        stack.push(frame);
        let context = ExecutionContext::from_module(module);
        let operands = vec![
            Operand::Register(1),
            Operand::ConstIndex(6),
            Operand::ConstIndex(1),
            Operand::Register(0),
        ];
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        interp
            .run_object_static_call_operands(&context, &mut stack, &operands)
            .unwrap();

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Object.fromEntries should allocate the result object through stack roots"
        );
        let Value::Object(result) = stack[0].registers[1] else {
            panic!("Object.fromEntries should return an object");
        };
        assert_eq!(
            object::get(result, interp.gc_heap(), "answer"),
            Some(Value::Number(NumberValue::Smi(9)))
        );
    }

    #[test]
    fn object_create_with_properties_uses_stack_rooted_result_allocation() {
        let module = module_with(vec![], 6);
        let mut interp = Interpreter::new();
        let proto = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();
        object::set(
            proto,
            interp.gc_heap_mut(),
            "inherited",
            Value::Boolean(true),
        );
        let descriptor = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();
        object::set(
            descriptor,
            interp.gc_heap_mut(),
            "value",
            Value::Number(NumberValue::Smi(11)),
        );
        let props = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();
        object::set(
            props,
            interp.gc_heap_mut(),
            "answer",
            Value::Object(descriptor),
        );

        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::Object(proto);
        frame.registers[1] = Value::Object(props);
        stack.push(frame);
        let context = ExecutionContext::from_module(module);
        let operands = vec![
            Operand::Register(2),
            Operand::ConstIndex(1),
            Operand::ConstIndex(2),
            Operand::Register(0),
            Operand::Register(1),
        ];
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        interp
            .run_object_static_call_operands(&context, &mut stack, &operands)
            .unwrap();

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Object.create with descriptors should allocate the result object through stack roots"
        );
        let Value::Object(created) = stack[0].registers[2] else {
            panic!("Object.create should return an object");
        };
        assert_eq!(
            object::prototype_value(created, interp.gc_heap()),
            Some(Value::Object(proto))
        );
        assert_eq!(
            object::get(created, interp.gc_heap(), "answer"),
            Some(Value::Number(NumberValue::Smi(11)))
        );
    }

    #[test]
    fn object_function_descriptor_uses_stack_rooted_result_allocation() {
        let module = module_with(vec![], 5);
        let mut interp = Interpreter::new();
        let key = Value::String(JsString::from_str("name", &interp.string_heap).unwrap());

        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::Function { function_id: 0 };
        frame.registers[1] = key;
        stack.push(frame);
        let context = ExecutionContext::from_module(module);
        let operands = vec![
            Operand::Register(2),
            Operand::ConstIndex(7),
            Operand::ConstIndex(2),
            Operand::Register(0),
            Operand::Register(1),
        ];
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        interp
            .run_object_static_call_operands(&context, &mut stack, &operands)
            .unwrap();

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Object.getOwnPropertyDescriptor(function, key) should allocate the descriptor object through stack roots"
        );
        let Value::Object(desc) = stack[0].registers[2] else {
            panic!("Object.getOwnPropertyDescriptor should return a descriptor object");
        };
        assert!(matches!(
            object::get(desc, interp.gc_heap(), "value"),
            Some(Value::String(_))
        ));
    }

    #[test]
    fn object_define_property_function_bag_uses_stack_rooted_allocation() {
        let module = module_with(vec![], 6);
        let mut interp = Interpreter::new();
        let key = Value::String(JsString::from_str("custom", &interp.string_heap).unwrap());
        let desc = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).expect("descriptor");
        object::set(
            desc,
            interp.gc_heap_mut(),
            "value",
            Value::Number(NumberValue::from_i32(7)),
        );

        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::Function { function_id: 0 };
        frame.registers[1] = key;
        frame.registers[2] = Value::Object(desc);
        stack.push(frame);
        let context = ExecutionContext::from_module(module);
        let operands = vec![
            Operand::Register(3),
            Operand::ConstIndex(2),
            Operand::ConstIndex(3),
            Operand::Register(0),
            Operand::Register(1),
            Operand::Register(2),
        ];
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        interp
            .run_object_static_call_operands(&context, &mut stack, &operands)
            .unwrap();

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Object.defineProperty(function, key, desc) should allocate the function bag through stack roots"
        );
        assert_eq!(stack[0].registers[3], Value::Function { function_id: 0 });
        let desc = interp
            .ordinary_function_own_property_descriptor(Some(&context), 0, "custom")
            .unwrap()
            .expect("custom descriptor");
        assert_eq!(
            descriptor_value(&desc),
            Value::Number(NumberValue::from_i32(7))
        );
    }

    #[test]
    fn object_proxy_property_names_use_stack_rooted_result_allocation() {
        let module = module_with(vec![], 5);
        let mut interp = Interpreter::new();
        let target = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();
        object::set(
            target,
            interp.gc_heap_mut(),
            "answer",
            Value::Number(NumberValue::Smi(42)),
        );
        let handler = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();
        let proxy = Value::Proxy(crate::proxy::JsProxy::new(Value::Object(target), handler));

        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = proxy;
        stack.push(frame);
        let context = ExecutionContext::from_module(module);
        let operands = vec![
            Operand::Register(1),
            Operand::ConstIndex(9),
            Operand::ConstIndex(1),
            Operand::Register(0),
        ];
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        interp
            .run_object_static_call_operands(&context, &mut stack, &operands)
            .unwrap();

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Object.getOwnPropertyNames(proxy) should allocate the result array through stack roots"
        );
        let Value::Array(names) = stack[0].registers[1] else {
            panic!("Object.getOwnPropertyNames(proxy) should return an array");
        };
        assert!(matches!(
            array::get(names, interp.gc_heap(), 0),
            Value::String(name) if name.to_lossy_string() == "answer"
        ));
    }

    #[test]
    fn proxy_call_argv_array_uses_young_allocation_with_frame_roots() {
        fn return_argv_array(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            Ok(args.get(2).cloned().unwrap_or(Value::Undefined))
        }

        fn target_noop(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
            Ok(Value::Undefined)
        }

        let module = module_with(vec![], 4);
        let mut interp = Interpreter::new();
        let apply =
            native_value_static(interp.gc_heap_mut(), "apply", 3, return_argv_array).unwrap();
        let target = native_value_static(interp.gc_heap_mut(), "target", 0, target_noop).unwrap();
        let handler = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();
        object::set(handler, interp.gc_heap_mut(), "apply", apply);
        let proxy = Value::Proxy(crate::proxy::JsProxy::new(target, handler));

        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = proxy;
        frame.registers[1] = Value::Number(NumberValue::Smi(7));
        frame.registers[2] = Value::Number(NumberValue::Smi(11));
        stack.push(frame);
        let context = ExecutionContext::from_module(module.clone());
        let operands = vec![
            Operand::Register(3),
            Operand::Register(0),
            Operand::ConstIndex(2),
            Operand::Register(1),
            Operand::Register(2),
        ];

        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        interp.do_call(&mut stack, &context, &operands).unwrap();
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;

        let Value::Array(argv) = stack[0].registers[3] else {
            panic!("expected proxy apply argv array");
        };
        let elements = array::with_elements(argv, interp.gc_heap(), |elements| elements.to_vec());
        assert_eq!(
            elements,
            vec![
                Value::Number(NumberValue::Smi(7)),
                Value::Number(NumberValue::Smi(11)),
            ]
        );
        assert!(
            after > before,
            "proxy apply argv array should allocate in young space"
        );
    }

    #[test]
    fn proxy_construct_argv_array_uses_young_allocation_with_frame_roots() {
        fn return_proxy_arg(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            Ok(args.get(2).cloned().unwrap_or(Value::Undefined))
        }

        let ctor = test_function(
            1,
            "Ctor",
            0,
            1,
            vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadThis,
                    operands: vec![Operand::Register(0)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)].into(),
                },
            ],
        );
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, vec![]), ctor],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let construct =
            native_value_static(interp.gc_heap_mut(), "construct", 3, return_proxy_arg).unwrap();
        let handler = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();
        object::set(handler, interp.gc_heap_mut(), "construct", construct);
        let proxy = Value::Proxy(crate::proxy::JsProxy::new(
            Value::Function { function_id: 1 },
            handler,
        ));

        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[1] = proxy;
        stack.push(frame);
        let context = ExecutionContext::from_module(module.clone());
        let operands = vec![
            Operand::Register(0),
            Operand::Register(1),
            Operand::ConstIndex(0),
        ];

        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        interp
            .do_construct(&mut stack, &context, &operands)
            .unwrap();
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;

        assert!(matches!(stack[0].registers[0], Value::Proxy(_)));
        assert!(
            after > before,
            "proxy construct argv array should allocate in young space"
        );
    }

    #[test]
    fn run_callable_sync_proxy_argv_array_uses_runtime_rooted_young_allocation() {
        fn return_argv_array(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            Ok(args.get(2).cloned().unwrap_or(Value::Undefined))
        }

        fn target_noop(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
            Ok(Value::Undefined)
        }

        let module = module_with(Vec::new(), 1);
        let context = ExecutionContext::from_module(module);
        let mut interp = Interpreter::new();
        let apply =
            native_value_static(interp.gc_heap_mut(), "apply", 3, return_argv_array).unwrap();
        let target = native_value_static(interp.gc_heap_mut(), "target", 0, target_noop).unwrap();
        let handler = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();
        object::set(handler, interp.gc_heap_mut(), "apply", apply);
        let proxy = Value::Proxy(crate::proxy::JsProxy::new(target, handler));
        let args: SmallVec<[Value; 8]> = smallvec::smallvec![
            Value::Number(NumberValue::Smi(3)),
            Value::Number(NumberValue::Smi(5)),
        ];

        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let result = interp
            .run_callable_sync(&context, &proxy, Value::Undefined, args)
            .unwrap();
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;

        let Value::Array(argv) = result else {
            panic!("proxy apply trap should return the synthesized argv array");
        };
        let elements = array::with_elements(argv, interp.gc_heap(), |elements| elements.to_vec());
        assert_eq!(
            elements,
            vec![
                Value::Number(NumberValue::Smi(3)),
                Value::Number(NumberValue::Smi(5)),
            ]
        );
        assert!(
            after > before,
            "run_callable_sync proxy argv array should allocate in young space"
        );
    }

    #[test]
    fn run_construct_sync_receiver_uses_runtime_rooted_young_allocation() {
        let ctor = test_function(
            1,
            "Ctor",
            0,
            1,
            vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadThis,
                    operands: vec![Operand::Register(0)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)].into(),
                },
            ],
        );
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 1, Vec::new()), ctor],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let context = ExecutionContext::from_module(module);
        let mut interp = Interpreter::new();
        let target = Value::Function { function_id: 1 };

        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let result = interp
            .run_construct_sync(&context, &target, target.clone(), SmallVec::new())
            .unwrap();
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;

        assert!(matches!(result, Value::Object(_)));
        assert!(
            after > before,
            "run_construct_sync should allocate the receiver in young space"
        );
    }

    #[test]
    fn run_construct_sync_proxy_argv_array_uses_runtime_rooted_young_allocation() {
        fn return_argv_array(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            Ok(args.get(1).cloned().unwrap_or(Value::Undefined))
        }

        let ctor = test_function(
            1,
            "Ctor",
            0,
            1,
            vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadThis,
                    operands: vec![Operand::Register(0)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)].into(),
                },
            ],
        );
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 1, Vec::new()), ctor],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let context = ExecutionContext::from_module(module);
        let mut interp = Interpreter::new();
        let construct =
            native_value_static(interp.gc_heap_mut(), "construct", 3, return_argv_array).unwrap();
        let handler = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();
        object::set(handler, interp.gc_heap_mut(), "construct", construct);
        let proxy = Value::Proxy(crate::proxy::JsProxy::new(
            Value::Function { function_id: 1 },
            handler,
        ));
        let args: SmallVec<[Value; 8]> = smallvec::smallvec![Value::Number(NumberValue::Smi(13))];

        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let result = interp
            .run_construct_sync(&context, &proxy, proxy.clone(), args)
            .unwrap();
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;

        let Value::Array(argv) = result else {
            panic!("proxy construct trap should return the synthesized argv array");
        };
        let elements = array::with_elements(argv, interp.gc_heap(), |elements| elements.to_vec());
        assert_eq!(elements, vec![Value::Number(NumberValue::Smi(13))]);
        assert!(
            after > before,
            "run_construct_sync proxy argv array should allocate in young space"
        );
    }

    #[test]
    fn arrow_closure_overrides_call_site_this() {
        // <main>: r0 = LoadThis; Return r0
        // The arrow closure wraps function id 1 with `is_arrow=true`
        // and a `bound_this = Some({tag: "outer"})`. We sneak the
        // bound `this` in by hand-building the closure value rather
        // than going through the full call sequence — the unit test
        // is proving that the arrow's lexical receiver wins, not
        // that the compiler emits the right opcode (the engine
        // suite's `arrow-this.ts` covers the latter).
        let main = Function {
            id: 0,
            name: "<main>".to_string(),
            span: (0, 0),
            locals: 0,
            scratch: 1,
            param_count: 0,
            own_upvalue_count: 0,
            is_strict: false,
            is_arrow: false,
            has_rest: false,
            is_async: false,
            is_generator: false,
            is_async_generator: false,
            is_module: false,
            needs_arguments: false,
            arguments_object_kind: ArgumentsObjectKind::Unmapped,
            mapped_argument_bindings: Vec::new(),
            module_url: String::new(),
            code: vec![Instruction {
                pc: 0,
                op: Op::ReturnUndefined,
                operands: vec![].into(),
            }],
            spans: vec![SpanEntry {
                pc: 0,
                span: (0, 0),
            }],
        };
        let arrow = Function {
            id: 1,
            name: "<arrow>".to_string(),
            span: (0, 0),
            locals: 0,
            scratch: 1,
            param_count: 0,
            own_upvalue_count: 0,
            is_strict: false,
            is_arrow: true,
            has_rest: false,
            is_async: false,
            is_generator: false,
            is_async_generator: false,
            is_module: false,
            needs_arguments: false,
            arguments_object_kind: ArgumentsObjectKind::Unmapped,
            mapped_argument_bindings: Vec::new(),
            module_url: String::new(),
            code: vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadThis,
                    operands: vec![Operand::Register(0)].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::ReturnValue,
                    operands: vec![Operand::Register(0)].into(),
                },
            ],
            spans: vec![SpanEntry {
                pc: 0,
                span: (0, 0),
            }],
        };
        let module = BytecodeModule {
            module: "arrow.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![main, arrow],
            constants: vec![],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        // Build the closure by hand and dispatch via `invoke`. The
        // bound_this is a marker string — if `LoadThis` returns it,
        // the lexical override is working.
        let mut interp = Interpreter::new();
        let bound = JsString::from_str("outer", interp.string_heap()).unwrap();
        let closure = Value::Closure {
            function_id: 1,
            upvalues: Frame::empty_upvalues(),
            bound_this: Some(Box::new(Value::String(bound.clone()))),
        };
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(Frame::for_function(&module.functions[0]));
        let context = ExecutionContext::from_module(module.clone());
        // Reserve a scratch slot in <main> to receive the result.
        stack[0].registers.push(Value::Undefined);
        // Caller-supplied this is `Null` — the closure must override.
        interp
            .invoke(
                &mut stack,
                &context,
                &closure,
                Value::Null,
                SmallVec::new(),
                /* dst */ 0,
            )
            .unwrap();
        // Drive the arrow's body to completion, then read r0 of <main>.
        loop {
            let top = stack.len() - 1;
            let f = module
                .functions
                .get(stack[top].function_id as usize)
                .unwrap();
            let pc = stack[top].pc as usize;
            let instr = &f.code[pc];
            if matches!(instr.op, Op::ReturnValue) {
                let value = stack[top].registers[0].clone();
                stack.pop();
                let caller = stack.last_mut().unwrap();
                let dst = caller.return_register.unwrap_or(0) as usize;
                caller.registers[dst] = value;
                break;
            }
            if matches!(instr.op, Op::LoadThis) {
                let dst = match instr.operands[0] {
                    Operand::Register(r) => r,
                    _ => unreachable!(),
                };
                let value = stack[top].this_value.clone();
                stack[top].registers[dst as usize] = value;
                stack[top].pc += 1;
                continue;
            }
            unreachable!();
        }
        assert_eq!(stack[0].registers[0], Value::String(bound));
    }

    #[test]
    fn interrupt_handle_breaks_loop() {
        let module = module_with(
            vec![
                Instruction {
                    pc: 0,
                    op: Op::Nop,
                    operands: vec![].into(),
                },
                Instruction {
                    pc: 1,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)].into(),
                },
            ],
            1,
        );
        let mut interp = Interpreter::new();
        let handle = interp.interrupt_handle();
        handle.interrupt();
        let context = ExecutionContext::from_module(module);
        assert_eq!(
            interp.run(&context).unwrap_err().error,
            VmError::Interrupted
        );
    }
}
