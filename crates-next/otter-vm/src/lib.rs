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
//! - [`Interpreter`] — match-based dispatch loop over
//!   [`otter_bytecode::BytecodeModule`].
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
//! - [`docs/new-engine/adr/0003-public-api-and-cli.md`](
//!     ../../../docs/new-engine/adr/0003-public-api-and-cli.md
//!   )
//! - [`docs/new-engine/specs/bytecode-dump-disasm-trace.md`](
//!     ../../../docs/new-engine/specs/bytecode-dump-disasm-trace.md
//!   )

pub mod abstract_ops;
pub mod array;
pub mod array_prototype;
pub mod array_statics;
pub mod atomics;
pub mod bigint;
pub mod binary;
pub mod boolean_prototype;
pub mod collections;
pub mod collections_prototype;
pub mod date;
// `date` is a directory module — see `date/mod.rs`.
pub mod error_classes;
pub mod gc_trace;
pub mod generator;
pub mod global_functions;
pub mod intl;
pub mod intrinsics;
pub mod json;
pub mod math;
pub mod microtask;
pub mod native_function;
pub mod number;
pub mod object;
pub mod object_statics;
pub mod promise;
pub mod promise_dispatch;
pub mod proxy;
pub mod reflect;
pub mod regexp;
pub mod regexp_prototype;
pub mod runtime_cx;
pub mod runtime_state;
pub mod string;
pub mod string_dispatch;
pub mod string_prototype;
pub mod symbol;
pub mod symbol_dispatch;
pub mod symbol_prototype;
pub mod temporal;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use otter_bytecode::{BytecodeModule, Constant, Function, Op, Operand};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::intrinsics::{IntrinsicArgs, IntrinsicError};

pub use array::JsArray;
pub use collections::{CollectionError, JsMap, JsSet, JsWeakMap, JsWeakSet, MapKey};
pub use error_classes::{ErrorClassRegistry, ErrorKind};
pub use intl::{IntlKind, IntlPayload, JsIntl};
pub use microtask::{AsyncRuntime, Microtask, MicrotaskError, MicrotaskKind, MicrotaskQueue};
pub use native_function::{NativeError, NativeFn, NativeFunction, native_value};
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

pub use runtime_cx::NativeCtx;

// ---------------------------------------------------------------------------
// `!Send + !Sync` static assertions for the new-engine VM.
//
// Per ADR-0005 §3 ("VM and GC stay explicit-context and
// single-mutator") and task 76A, the interpreter, every GC handle,
// and every borrowed-context type must be `!Send + !Sync` so the
// compile-fail tests under `tests/compile_fail/` reject any future
// edit that accidentally moves a VM handle into `tokio::spawn` or
// holds a `&mut RuntimeCx` across `.await`.
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
    /// the wrapper is `Rc`-shared.
    BoundFunction(std::rc::Rc<BoundFunction>),
    /// Host-implemented callable. Used by `Promise` resolve/reject
    /// closures, the `Promise.all` aggregator-functions, and any
    /// other native shape that needs to be JS-callable without
    /// going through bytecode. See [`crate::NativeFunction`].
    NativeFunction(std::rc::Rc<NativeFunction>),
    /// Internal iterator state, produced by [`otter_bytecode::Op::GetIterator`]
    /// and driven by [`otter_bytecode::Op::IteratorNext`]. Until
    /// task 37 adds real `Symbol.iterator` lookup, the foundation
    /// models iterators out-of-band as a dedicated value variant
    /// — they are not addressable via `o[@@iterator]` from user
    /// code.
    Iterator(std::rc::Rc<std::cell::RefCell<IteratorState>>),
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
    /// JS `WeakMap` — object-keyed map. Foundation keeps strong
    /// refs until task 57 lands the tracing GC.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-weakmap-objects>
    WeakMap(JsWeakMap),
    /// JS `WeakSet` — object-keyed set. Same foundation gap as
    /// [`Self::WeakMap`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-weakset-objects>
    WeakSet(JsWeakSet),
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
    ClassConstructor(std::rc::Rc<ClassConstructor>),
}

/// Storage for [`Value::ClassConstructor`]. Cloned by handle so
/// passing a class through registers stays cheap; the wrapper is
/// `Rc`-shared and the inner objects are themselves heap-shared.
#[derive(Debug)]
pub struct ClassConstructor {
    /// The actual callable (a `Value::Function` or
    /// `Value::Closure`) the runtime invokes for `new C(...)` or
    /// `super(...)`. Constructed by the compiler's class-lowering
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

/// Foundation iterator-state machine. Each variant carries the
/// minimum information needed to advance one step at a time. Once
/// the iterator reports `done`, subsequent calls keep returning
/// `done = true` with `value = undefined` (per spec §7.4.2 step 6).
#[derive(Debug)]
pub enum IteratorState {
    /// Walks `array`'s dense storage in insertion order.
    Array {
        /// Backing array — held by `JsArray`'s internal `Rc` so
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
        source: std::rc::Rc<std::cell::RefCell<IteratorState>>,
        /// Per-element mapper. Must be callable.
        mapper: Value,
    },
    /// Lazy `Iterator.prototype.filter(predicate)` wrapper.
    /// Skips elements for which `predicate` returns falsey.
    Filter {
        /// Underlying iterator handle.
        source: std::rc::Rc<std::cell::RefCell<IteratorState>>,
        /// Per-element predicate. Must be callable.
        predicate: Value,
    },
    /// Lazy `Iterator.prototype.take(n)` wrapper. Yields at most
    /// `remaining` more elements from `source`.
    Take {
        /// Underlying iterator handle.
        source: std::rc::Rc<std::cell::RefCell<IteratorState>>,
        /// Steps still allowed before the wrapper reports `done`.
        remaining: u64,
    },
    /// Lazy `Iterator.prototype.drop(n)` wrapper. Discards the
    /// first `to_drop` elements of `source` then forwards the rest.
    Drop {
        /// Underlying iterator handle.
        source: std::rc::Rc<std::cell::RefCell<IteratorState>>,
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
        source: std::rc::Rc<std::cell::RefCell<IteratorState>>,
        /// Per-element mapper. Must be callable.
        mapper: Value,
        /// Inner iterator currently being drained, when the last
        /// `mapper` call produced an iterable.
        inner: Option<std::rc::Rc<std::cell::RefCell<IteratorState>>>,
    },
}

/// Storage for `Value::BoundFunction`. Constructed by the
/// `Op::BindFunction` opcode and consumed by every call dispatch
/// path (`Op::Call`, `Op::CallWithThis`, `Op::CallMethodValue`).
#[derive(Debug, Clone)]
pub struct BoundFunction {
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
    pub bound_args: SmallVec<[Value; 4]>,
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
/// the previous `Rc<RefCell<Value>>` (8-byte pointer +
/// allocation overhead).
///
/// # Spec
///
/// Captured-binding semantics — ECMA-262
/// §9.1.1.1.4 (CreateMutableBinding) + §9.1.1.1.5
/// (InitializeBinding); the closure spine that holds these
/// cells is built by `Op::MakeClosure` per §15.2.5
/// (FunctionDeclarationInstantiation). Upvalue migration
/// rationale lives in
/// `docs/new-engine/gc-architecture.md` §4.1, §6.3.
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
    fn trace_slots_safe(&self, v: &mut otter_gc::SlotVisitor<'_>) {
        self.value.trace_value_slots(v);
    }
}

/// Compressed handle to an [`UpvalueCellBody`] — replaces the
/// pre-task-76 `Rc<RefCell<Value>>`. `Copy + Eq + Hash`
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
    let child_raw = value.as_gc_raw();
    heap.with_payload(cell, |body| {
        body.value = value;
    });
    if let Some(child) = child_raw {
        // Slot pointer = base of payload; payload's first (and
        // only) field is `value`. Pointer arithmetic is safe
        // (raw casts), provenance flows from `as_header_ptr`.
        let body_base = cell.as_header_ptr() as *mut u8;
        let value_slot = body_base.wrapping_add(std::mem::size_of::<otter_gc::GcHeader>())
            as *mut otter_gc::RawGc;
        heap.write_barrier_raw(cell, value_slot, child);
    }
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
    pub fn as_gc_raw(&self) -> Option<otter_gc::RawGc> {
        // Phase-1 stub. Migrations 77+ pattern-match the new
        // variants and return `Some(handle.raw())`.
        None
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
    pub fn trace_value_slots(&self, visitor: &mut otter_gc::SlotVisitor<'_>) {
        if let Value::Closure { upvalues, .. } = self {
            for slot in upvalues.iter() {
                let p = slot as *const UpvalueCell as *mut otter_gc::RawGc;
                visitor(p);
            }
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
            Value::Undefined => "undefined".to_string(),
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
            Value::BoundFunction(b) => format!("[BoundFunction → {}]", b.target.display_string()),
            Value::NativeFunction(f) => format!("[NativeFunction {}]", f.name),
            Value::Iterator(_) => "[object Iterator]".to_string(),
            Value::RegExp(r) => format!("/{}/{}", r.source(), r.flags().to_js_string()),
            Value::Promise(_) => "[object Promise]".to_string(),
            Value::ClassConstructor(_) => "[class]".to_string(),
            Value::Map(_) => "[object Map]".to_string(),
            Value::Set(_) => "[object Set]".to_string(),
            Value::WeakMap(_) => "[object WeakMap]".to_string(),
            Value::WeakSet(_) => "[object WeakSet]".to_string(),
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
            Value::Array(a) => {
                let body = a.borrow_body();
                let parts: Vec<String> = body.iter().map(Value::display_string).collect();
                parts.join(",")
            }
        }
    }

    /// Spec [`ToBoolean`](https://tc39.es/ecma262/#sec-toboolean)
    /// for the foundation subset.
    #[must_use]
    pub fn to_boolean(&self) -> bool {
        match self {
            Value::Undefined | Value::Null => false,
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
            Value::Undefined => "undefined",
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

    /// Construct a string value from in-memory text. Convenience
    /// for tests and the compiler's literal table.
    ///
    /// # Errors
    /// See [`JsString::from_str`].
    pub fn from_str(s: &str, heap: &StringHeap) -> Result<Self, StringError> {
        Ok(Self::String(JsString::from_str(s, heap)?))
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Undefined, Value::Undefined) => true,
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
            (Value::Object(a), Value::Object(b)) => a.ptr_eq(b),
            (Value::Array(a), Value::Array(b)) => a.ptr_eq(b),
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
            (Value::BoundFunction(a), Value::BoundFunction(b)) => std::rc::Rc::ptr_eq(a, b),
            (Value::NativeFunction(a), Value::NativeFunction(b)) => std::rc::Rc::ptr_eq(a, b),
            (Value::Promise(a), Value::Promise(b)) => a.ptr_eq(b as &dyn JsPromise),
            (Value::Iterator(a), Value::Iterator(b)) => std::rc::Rc::ptr_eq(a, b),
            (Value::RegExp(a), Value::RegExp(b)) => a.ptr_eq(b),
            (Value::ClassConstructor(a), Value::ClassConstructor(b)) => std::rc::Rc::ptr_eq(a, b),
            (Value::Map(a), Value::Map(b)) => a.ptr_eq(b),
            (Value::Set(a), Value::Set(b)) => a.ptr_eq(b),
            (Value::WeakMap(a), Value::WeakMap(b)) => a.ptr_eq(b),
            (Value::WeakSet(a), Value::WeakSet(b)) => a.ptr_eq(b),
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

/// Cooperative cancellation flag.
///
/// Cheap, cloneable, `Send + Sync`. The interpreter polls this flag
/// before each instruction. An interrupt request converts into
/// [`VmError::Interrupted`] at the next checkpoint.
#[derive(Debug, Default, Clone)]
pub struct InterruptFlag(Arc<AtomicBool>);

impl InterruptFlag {
    /// Construct a fresh, un-tripped flag.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Trip the flag from any thread.
    pub fn interrupt(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Check the flag without resetting it.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Reset the flag.
    pub fn reset(&self) {
        self.0.store(false, Ordering::Release);
    }
}

/// One call frame. Compact and cache-conscious per foundation
/// plan §M7. Slice 13 promotes the interpreter to a real frame
/// stack (`SmallVec<[Frame; 8]>` inside the dispatcher) so
/// function calls push and pop without per-call `Vec` allocation.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Index into the bytecode container's function table.
    pub function_id: u32,
    /// Current program counter (instruction index, not byte offset).
    pub pc: u32,
    /// Register window for this frame.
    pub registers: SmallVec<[Value; 8]>,
    /// When `Some(reg)`, returning from this frame writes the
    /// completion value into the **caller's** register `reg` and
    /// resumes at the caller's next pc. `<main>` carries `None`
    /// and propagates the value out as the script's completion.
    pub return_register: Option<u16>,
    /// Captured upvalues for this call. Empty for non-closure
    /// frames. Indexed by `Op::LoadUpvalue` / `Op::StoreUpvalue`
    /// operands.
    pub upvalues: std::rc::Rc<[UpvalueCell]>,
    /// `this` value visible inside the body. `<main>` and free
    /// `Op::Call` invocations both bind `Value::Undefined`
    /// (foundation strict default). Method calls set the receiver,
    /// `Op::CallWithThis` and `Op::CallMethodValue` thread a caller-
    /// provided value, and arrow closures override with their
    /// lexically-captured `this` regardless of the call site.
    pub this_value: Value,
    /// Active try-handler stack. Pushed by [`Op::EnterTry`], popped
    /// by [`Op::LeaveTry`] or by an exception unwind landing on a
    /// matching catch / finally. Innermost handler is on top.
    pub handlers: SmallVec<[TryHandler; 4]>,
    /// In-flight exception parked when a throw routed into a
    /// `finally` block. [`Op::EndFinally`] consumes it: `Some` →
    /// re-throw, `None` → fall through. The compiler always emits
    /// `EndFinally` at the close of every finally body, so the
    /// re-throw protocol stays bytecode-visible.
    pub pending_throw: Option<Value>,
    /// Newly-allocated receiver when this frame was entered via
    /// [`Op::New`] (`new C(args)`). On return, [`Interpreter::pop_frame`]
    /// substitutes this object for any non-object return value, so
    /// constructors that don't `return` a replacement still hand the
    /// caller the freshly-built instance.
    pub construct_target: Option<JsObject>,
    /// Trailing arguments past the declared `param_count`. Populated
    /// by the call dispatcher only when the callee declares a rest
    /// parameter (`function f(...rest) { … }`); consumed by
    /// [`otter_bytecode::Op::CollectRest`] which packs them into a
    /// fresh `JsArray`. Always empty for non-rest callees so the
    /// allocation cost is paid only when needed.
    pub rest_args: SmallVec<[Value; 4]>,
    /// Full incoming-argument list captured at call entry. Used by
    /// [`otter_bytecode::Op::CollectArguments`] to materialise an
    /// `arguments`-style array containing every value the caller
    /// supplied — including the named parameters. Populated only
    /// when the callee was compiled with `needs_arguments = true`
    /// so non-arguments-using functions pay no allocation cost.
    pub incoming_args: SmallVec<[Value; 4]>,
    /// Async-call state: `Some` when this frame belongs to an
    /// `async` function. The result promise was created at call
    /// entry and written into the caller's destination register
    /// **then**; on return / unhandled throw, the dispatcher
    /// settles this promise instead of writing a value to the
    /// caller. `Op::Await` parks the frame off the stack and
    /// re-pushes it from a microtask once the awaited promise
    /// settles. `None` for ordinary (non-async) frames.
    pub async_state: Option<AsyncFrameState>,
    /// Source-module URL the running function was compiled from.
    /// Snapshot of [`otter_bytecode::Function::module_url`] at
    /// frame-push time. Read by [`Op::ImportNamespace`] to look
    /// up specifier resolutions in the linker's pre-built
    /// `module_resolutions` table — the caller frame's URL is
    /// the referrer for the import-resolution algorithm.
    ///
    /// Empty string for non-module functions (e.g. the linker's
    /// synthesised `<entry>` driver) — those frames inherit the
    /// caller's URL when invoking module-init functions, but
    /// `Op::ImportNamespace` itself never executes from a
    /// non-module frame in well-formed bytecode.
    pub module_url: std::rc::Rc<str>,
    /// State machine for the in-flight ECMA-262 §7.1.1 `ToPrimitive`
    /// ladder. `Some` while the dispatcher is mid-way through the
    /// `[Symbol.toPrimitive]` / `valueOf` / `toString` chain on a
    /// specific `Op::ToPrimitive` instruction; `None` otherwise.
    /// Set by [`Interpreter::drive_to_primitive`] before pushing a
    /// call frame, cleared once the ladder hands back a primitive
    /// (or exhausts every stage and the dispatcher raises a
    /// `TypeMismatch`).
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-toprimitive>
    pub pending_to_primitive: Option<PendingToPrimitive>,
    /// In-flight ECMA-262 §7.4.3 `GetIterator` over a user object.
    /// `Some` while the dispatcher is awaiting the result of
    /// `obj[@@iterator]()`; the resume step wraps that return
    /// value as [`IteratorState::User`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-getiterator>
    pub pending_get_iterator: Option<PendingGetIterator>,
    /// In-flight ECMA-262 §7.4.5 `IteratorNext` over a user
    /// iterator. `Some` while the dispatcher is awaiting the
    /// result of `iter.next()`; the resume step extracts
    /// `value` / `done` from the returned record.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-iteratornext>
    pub pending_iterator_next: Option<PendingIteratorNext>,
    /// `Some(gen)` when this frame is the suspended body of an
    /// active generator object. [`otter_bytecode::Op::Yield`]
    /// inspects this slot: if set, the running frame is unspooled
    /// onto the generator's saved-state slot and the dispatcher
    /// returns to the calling `.next()` resume site. `None` for
    /// every other call shape.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-generator-objects>
    pub generator_owner: Option<crate::generator::JsGenerator>,
}

/// In-flight state for [`Op::GetIterator`] when the source operand
/// is a user object. Carries the originating `pc` (so the resume
/// guard can verify) and the destination register that should
/// receive the [`Value::Iterator`] handle on completion.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-getiterator>
#[derive(Debug, Clone)]
pub struct PendingGetIterator {
    /// pc of the originating `Op::GetIterator`.
    pub pc: u32,
    /// Destination register the iterator handle must land in.
    pub dst: u16,
}

/// In-flight state for [`Op::IteratorNext`] over a user iterator.
/// The dispatcher calls `iter.next()` and parks this record with
/// the destination registers for `value` and `done` plus the
/// scratch register that received the call's result record.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-iteratornext>
#[derive(Debug, Clone)]
pub struct PendingIteratorNext {
    /// pc of the originating `Op::IteratorNext`.
    pub pc: u32,
    /// Destination register for the unpacked `value`.
    pub value_dst: u16,
    /// Destination register for the unpacked `done` flag.
    pub done_dst: u16,
    /// Scratch register that receives the `iter.next()` result
    /// record. The resume step reads `value` / `done` off this
    /// register and clears the slot.
    pub result_reg: u16,
    /// The iterator value itself. Cloned onto the parked record
    /// so the resume step can transition the inner state to
    /// [`IteratorState::Exhausted`] once `done` becomes truthy.
    pub iterator: Value,
}

/// In-flight state for an [`Op::ToPrimitive`] dispatch.
///
/// Carries the original object operand, the resolved hint, the
/// destination register the ladder writes its final result into,
/// and the next stage to run when the dispatcher resumes. Cloning
/// is cheap: every payload is either a small enum variant or a
/// `Value` clone.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-toprimitive>
/// - <https://tc39.es/ecma262/#sec-ordinarytoprimitive>
#[derive(Debug, Clone)]
pub struct PendingToPrimitive {
    /// pc of the originating `Op::ToPrimitive` — so the resume
    /// hook can verify the dispatcher is back on the same
    /// instruction.
    pub pc: u32,
    /// Destination register for the final primitive value.
    pub dst: u16,
    /// Original (object) operand.
    pub obj: Value,
    /// Caller's preferred-type hint.
    pub hint: abstract_ops::ToPrimitiveHint,
    /// Next stage to attempt.
    pub stage: ToPrimitiveStage,
}

/// Stages of the §7.1.1 / §7.1.1.1 ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToPrimitiveStage {
    /// About to look up `[Symbol.toPrimitive]` and (if callable)
    /// invoke it. On resume, validate the result is primitive;
    /// otherwise fall through to [`Self::OrdinaryFirst`].
    SymbolToPrim,
    /// First slot of the OrdinaryToPrimitive chain — `valueOf` for
    /// `Default` / `Number` hints, `toString` for `String` hint.
    OrdinaryFirst,
    /// Second slot — `toString` after `valueOf`, or `valueOf` after
    /// `toString`.
    OrdinarySecond,
    /// Both ordinary slots have run and returned non-primitive
    /// values. The next dispatch tick raises `TypeMismatch` per
    /// §7.1.1.1 step 6.
    Exhausted,
}

/// Per-frame bookkeeping for an async-function call. Constructed
/// by the entry path in [`Interpreter::invoke`] when the callee's
/// [`otter_bytecode::Function::is_async`] flag is true; consumed by
/// [`Interpreter::pop_frame`] (fulfilment) and the throw-unwinder
/// (rejection).
#[derive(Debug, Clone)]
pub struct AsyncFrameState {
    /// The promise the call-site received synchronously. Settles
    /// when the async body returns (fulfil) or throws an
    /// unhandled error (reject).
    pub result_promise: JsPromiseHandle,
}

/// One active try-handler descriptor — the runtime counterpart to
/// the compiler's `TRY_BEGIN … TRY_END` block. Each
/// [`Op::EnterTry`] dispatch pushes one of these onto the
/// owning frame; throw unwinding pops back to the innermost match.
#[derive(Debug, Clone, Copy)]
pub struct TryHandler {
    /// Catch clause entry pc, or `None` for `try { … } finally { … }`
    /// without a catch.
    pub catch_pc: Option<u32>,
    /// Finally clause entry pc, or `None` when there is no
    /// finally. The unwinder routes the in-flight exception
    /// through finally even when a catch is present, so the
    /// compiler emits the catch body first and chains its
    /// completion through finally.
    pub finally_pc: Option<u32>,
    /// Register that the catch clause expects the thrown value in.
    /// Ignored when `catch_pc` is `None`.
    pub exc_register: u16,
}

impl Frame {
    /// Allocate a frame for `function`. Registers are pre-filled
    /// with `Value::Undefined`. Used for test-side construction
    /// of trivial functions.
    ///
    /// **Precondition (since task 76):** `function.own_upvalue_count
    /// == 0`. Functions with own upvalues route through
    /// [`Self::for_function_with_heap`] (production path) or
    /// [`Self::build_upvalues`] + [`Self::with_return_upvalues_and_this`].
    #[must_use]
    pub fn for_function(function: &Function) -> Self {
        debug_assert_eq!(
            function.own_upvalue_count, 0,
            "Frame::for_function requires zero own upvalues — use for_function_with_heap or build_upvalues + with_return_upvalues_and_this"
        );
        Self::with_return(function, None)
    }

    /// Allocate a frame for `function`, allocating
    /// `function.own_upvalue_count` cells on the GC heap.
    /// The production entry path uses this for the `<main>`
    /// frame so any top-level `let n = 0; () => n` style upvalue
    /// has a backing cell from the moment dispatch starts.
    ///
    /// # Errors
    ///
    /// Surfaces [`otter_gc::OutOfMemory`] verbatim.
    pub fn for_function_with_heap(
        function: &Function,
        heap: &mut otter_gc::GcHeap,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let upvalues = Self::build_upvalues(heap, function, std::rc::Rc::from(Vec::new()))?;
        Ok(Self::with_return_upvalues_and_this(
            function,
            None,
            upvalues,
            Value::Undefined,
        ))
    }

    /// Allocate a frame whose return value should land in the
    /// caller's register `return_register`. Same precondition as
    /// [`Self::for_function`] — zero own upvalues.
    #[must_use]
    pub fn with_return(function: &Function, return_register: Option<u16>) -> Self {
        Self::with_return_upvalues_and_this(
            function,
            return_register,
            std::rc::Rc::from(Vec::new()),
            Value::Undefined,
        )
    }

    /// Build the captured-upvalue spine for `function`, allocating
    /// `function.own_upvalue_count` fresh
    /// [`UpvalueCellBody`] cells on the GC heap and prepending them
    /// to `parent_upvalues` (per the §15.2.5 capture layout).
    ///
    /// # Errors
    ///
    /// Surfaces [`otter_gc::OutOfMemory`] verbatim.
    pub fn build_upvalues(
        heap: &mut otter_gc::GcHeap,
        function: &Function,
        parent_upvalues: std::rc::Rc<[UpvalueCell]>,
    ) -> Result<std::rc::Rc<[UpvalueCell]>, otter_gc::OutOfMemory> {
        let own = function.own_upvalue_count as usize;
        if own == 0 {
            return Ok(parent_upvalues);
        }
        let mut cells: Vec<UpvalueCell> = Vec::with_capacity(own + parent_upvalues.len());
        for _ in 0..own {
            cells.push(alloc_upvalue(heap, Value::Undefined)?);
        }
        cells.extend(parent_upvalues.iter().copied());
        Ok(std::rc::Rc::from(cells))
    }

    /// Full constructor used by call sites that need to bind a
    /// non-default `this`. The caller is responsible for
    /// pre-building `upvalues` via [`Self::build_upvalues`] (or
    /// passing `Rc::from(Vec::new())` when the function has none).
    /// See [`Op::MakeClosure`](otter_bytecode::Op::MakeClosure)
    /// for the layout.
    #[must_use]
    pub fn with_return_upvalues_and_this(
        function: &Function,
        return_register: Option<u16>,
        upvalues: std::rc::Rc<[UpvalueCell]>,
        this_value: Value,
    ) -> Self {
        let total = function
            .param_count
            .saturating_add(function.locals)
            .saturating_add(function.scratch) as usize;
        let mut registers: SmallVec<[Value; 8]> = SmallVec::with_capacity(total);
        registers.resize(total, Value::Undefined);
        debug_assert!(
            upvalues.len() >= function.own_upvalue_count as usize,
            "frame upvalues must include the function's own cells"
        );
        Self {
            function_id: function.id,
            pc: 0,
            registers,
            return_register,
            upvalues,
            this_value,
            handlers: SmallVec::new(),
            pending_throw: None,
            construct_target: None,
            rest_args: SmallVec::new(),
            incoming_args: SmallVec::new(),
            async_state: None,
            module_url: std::rc::Rc::from(function.module_url.as_str()),
            pending_to_primitive: None,
            pending_get_iterator: None,
            pending_iterator_next: None,
            generator_owner: None,
        }
    }
}

/// Runtime errors raised by the interpreter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum VmError {
    /// The program counter walked off the end of `code` without a
    /// `RETURN`. Indicates a compiler bug.
    MissingReturn,
    /// An operand index was out of range. Indicates a compiler bug
    /// or a malformed bytecode dump.
    InvalidOperand,
    /// An operand had the wrong type for its opcode (e.g.,
    /// `STRING_CONCAT` on a non-string register). Indicates a
    /// compiler bug at this slice.
    TypeMismatch,
    /// String allocation failed because the heap cap was hit.
    OutOfMemory {
        /// Bytes the allocation requested.
        requested_bytes: u64,
        /// Heap cap (`0` = unlimited).
        heap_limit_bytes: u64,
    },
    /// `InterruptFlag` was tripped before the next checkpoint.
    Interrupted,
    /// `CALL_STRING_METHOD` referenced a method name not in
    /// [`string_prototype::STRING_PROTOTYPE_TABLE`].
    UnknownIntrinsic {
        /// Method name as it appeared in the constant pool.
        name: String,
    },
    /// A `let`/`const` binding was read before its initializer ran
    /// (Temporal Dead Zone).
    TemporalDeadZone {
        /// Compiler-assigned local index.
        local_index: u32,
    },
    /// JS call-stack depth exceeded the configured limit. Catchable
    /// per foundation plan §M7 ("stack-depth limit returns a
    /// catchable JS error").
    StackOverflow {
        /// Maximum depth that was about to be exceeded.
        limit: u32,
    },
    /// Tried to call a value that is not callable.
    NotCallable,
    /// `LoadGlobalOrThrow` (or another lookup site) hit an
    /// unbound free identifier in strict mode. Convertible to a
    /// real `ReferenceError` instance via `vm_error_to_throwable`.
    UndefinedIdentifier {
        /// Name of the unbound identifier.
        name: String,
    },
    /// A user `throw` (or a re-throw from `finally`) walked the
    /// entire frame stack without finding a matching handler. The
    /// payload is the JS value that was thrown, rendered for
    /// diagnostics through [`Value::display_string`]; the runtime
    /// surfaces this as `OtterError::Runtime { code = "UNCAUGHT" }`.
    Uncaught {
        /// Display rendering of the thrown value.
        value: String,
    },
    /// `Op::LoadRegExp` produced a pattern that the regex backend
    /// could not compile. Catchable as `SyntaxError` once a real
    /// error model lands; for now it surfaces through the standard
    /// runtime-error code.
    InvalidRegExp {
        /// Backend diagnostic — pattern + flags + reason.
        message: String,
    },
    /// `JSON.stringify` / `JSON.parse` rejected its input. The
    /// `code` discriminates the failure family so the runtime can
    /// surface a precise diagnostic (`JSON.stringify cannot
    /// serialize cyclic structures.`, `JSON Parse error: <reason>
    /// at byte N`, …) instead of the generic `TYPE_MISMATCH`.
    JsonError {
        /// Stable identifier (e.g. `"JSON_CYCLIC"`).
        code: &'static str,
        /// Human-readable diagnostic. Includes the byte position
        /// for `JSON_PARSE`.
        message: String,
    },
}

impl std::fmt::Display for VmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmError::MissingReturn => write!(f, "function did not RETURN"),
            VmError::InvalidOperand => write!(f, "invalid operand"),
            VmError::TypeMismatch => write!(f, "operand type mismatch"),
            VmError::OutOfMemory {
                requested_bytes,
                heap_limit_bytes,
            } => write!(
                f,
                "out of memory: requested {requested_bytes} bytes, heap limit {heap_limit_bytes}"
            ),
            VmError::Interrupted => write!(f, "interrupted"),
            VmError::UnknownIntrinsic { name } => write!(f, "unknown intrinsic method `{name}`"),
            VmError::TemporalDeadZone { local_index } => {
                write!(f, "cannot access local {local_index} before initialization")
            }
            VmError::StackOverflow { limit } => {
                write!(f, "maximum call stack size exceeded (limit {limit})")
            }
            VmError::NotCallable => write!(f, "value is not a function"),
            VmError::UndefinedIdentifier { name } => write!(f, "{name} is not defined"),
            VmError::Uncaught { value } => write!(f, "uncaught exception: {value}"),
            VmError::InvalidRegExp { message } => write!(f, "{message}"),
            VmError::JsonError { message, .. } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for VmError {}

impl From<StringError> for VmError {
    fn from(err: StringError) -> Self {
        match err {
            StringError::OutOfMemory {
                requested_bytes,
                heap_limit_bytes,
            } => VmError::OutOfMemory {
                requested_bytes,
                heap_limit_bytes,
            },
        }
    }
}

impl From<otter_gc::OutOfMemory> for VmError {
    fn from(err: otter_gc::OutOfMemory) -> Self {
        VmError::OutOfMemory {
            requested_bytes: err.requested_bytes(),
            heap_limit_bytes: err.heap_limit_bytes(),
        }
    }
}

/// Default JS call-stack depth limit. Catchable via
/// [`VmError::StackOverflow`].
pub const DEFAULT_MAX_STACK_DEPTH: u32 = 1024;

/// Re-export of the bytecode-defined sentinel for "this try block
/// has no catch / finally clause". Kept on the VM surface so
/// embedders that want to hand-build EnterTry operands have one
/// import path for the runtime semantics.
pub use otter_bytecode::NO_HANDLER_OFFSET;

/// One stack-frame snapshot captured at the moment an error is
/// raised. Foundation slice 16 ships this — task 24 (exceptions)
/// reuses it for catchable error frames.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackFrameSnapshot {
    /// Function name; `<main>` for the script entry,
    /// `<arrow>`/`<anonymous>` for function expressions.
    pub function_name: String,
    /// Module specifier the function was compiled from.
    pub module: String,
    /// Source span of the failing instruction (byte offsets).
    pub span: (u32, u32),
}

/// Result type returned by [`Interpreter::run`] on failure: the
/// underlying [`VmError`] plus a snapshot of the live frame stack
/// at the moment the error was raised. Caller-level translation
/// (e.g., `otter-runtime::map_vm_error`) propagates `frames` into
/// `Diagnostic.frames`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunError {
    /// Underlying error.
    pub error: VmError,
    /// Top-of-stack first; element zero is the failing function.
    pub frames: Vec<StackFrameSnapshot>,
}

impl RunError {
    /// Convenience constructor for the no-frames case (e.g., setup
    /// errors before any frame exists).
    #[must_use]
    pub fn bare(error: VmError) -> Self {
        Self {
            error,
            frames: Vec::new(),
        }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}

impl std::error::Error for RunError {}

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
    /// Per-function user-property bag (§20.2.4 Function-instance
    /// properties + ordinary [[Set]] semantics for callables).
    /// `function_id` → `JsObject` carrying anything the user wrote
    /// via `f.foo = bar` / `Ctor.prototype.x = …` / etc. Lazily
    /// allocated on first write. Closures share the bag with their
    /// underlying function so writes through any closure handle
    /// land on the same place.
    function_user_props: std::collections::HashMap<u32, JsObject>,
}

impl std::fmt::Debug for Interpreter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Interpreter")
            .field("max_stack_depth", &self.max_stack_depth)
            .field("eval_hook_installed", &self.eval_hook.is_some())
            .finish_non_exhaustive()
    }
}

/// Embedder-supplied parse + compile callback used by
/// [`Op::Eval`] / [`Op::NewFunction`]. Returns a freshly linked
/// [`BytecodeModule`] whose `<main>` completion value becomes the
/// dispatch result.
pub type EvalHook = std::rc::Rc<dyn Fn(&str) -> Result<BytecodeModule, String>>;

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
        let string_heap = Arc::new(StringHeap::with_cap(cap_bytes));
        let well_known_symbols = WellKnownSymbols::new(&string_heap)
            .expect("well-known symbol descriptions fit within any positive cap");
        let error_classes = ErrorClassRegistry::new(&string_heap)
            .expect("error class prototypes fit within any positive cap");
        let gc_heap = otter_gc::GcHeap::with_max_heap_bytes(cap_bytes)
            .expect("GcHeap construction never fails on the default cage");
        Self {
            interrupt: InterruptFlag::new(),
            string_heap,
            gc_heap,
            max_stack_depth: DEFAULT_MAX_STACK_DEPTH,
            microtasks: MicrotaskQueue::new(),
            module_environments: std::collections::HashMap::new(),
            module_resolution_cache: std::collections::HashMap::new(),
            well_known_symbols,
            symbol_registry: SymbolRegistry::new(),
            error_classes,
            global_this: build_global_this(),
            eval_hook: None,
            pending_generator_throw: None,
            function_user_props: std::collections::HashMap::new(),
        }
    }

    /// Resolve a property read on a `Value::Function` /
    /// `Value::Closure`. Honours user-installed properties via the
    /// `function_user_props` side table, lazily allocates
    /// `Function.prototype` on first access (§9.2.10
    /// MakeConstructor), and falls back to `name` / `length`
    /// intrinsics. Unknown names return `undefined` per §10.1.8
    /// OrdinaryGet step 4.
    fn function_property_get(
        &mut self,
        module: &BytecodeModule,
        function_id: u32,
        name: &str,
    ) -> Result<Value, VmError> {
        if let Some(bag) = self.function_user_props.get(&function_id)
            && let Some(v) = bag.get(name)
        {
            return Ok(v);
        }
        if name == "prototype" {
            // §9.2.10 — function instances expose a writable,
            // non-configurable `.prototype` that auto-allocates as
            // a fresh ordinary object on first access. We don't
            // wire `prototype.constructor` back to the function
            // here (foundation gap; matches the §41 audit closeout
            // priorities).
            let bag = self.function_user_props.entry(function_id).or_default();
            if let Some(existing) = bag.get("prototype") {
                return Ok(existing);
            }
            let proto = JsObject::new();
            let proto_value = Value::Object(proto);
            bag.set("prototype", proto_value.clone());
            return Ok(proto_value);
        }
        if name == "name" || name == "length" {
            return function_intrinsic_property(module, function_id, name, &self.string_heap);
        }
        Ok(Value::Undefined)
    }

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
        module: &BytecodeModule,
        referrer: &str,
        specifier: &str,
    ) -> Option<JsObject> {
        let referrer_rc: std::rc::Rc<str> = std::rc::Rc::from(referrer);
        let key = (referrer_rc.clone(), specifier.to_string());
        let target_url = if let Some(hit) = self.module_resolution_cache.get(&key) {
            hit.clone()
        } else {
            let target = module
                .module_resolutions
                .iter()
                .find(|r| r.referrer == referrer && r.specifier == specifier)?
                .target
                .clone();
            let target_rc: std::rc::Rc<str> = std::rc::Rc::from(target.as_str());
            self.module_resolution_cache.insert(key, target_rc.clone());
            target_rc
        };
        self.module_environments.get(target_url.as_ref()).cloned()
    }

    /// Mutable handle to the microtask queue. Embedders use this
    /// to wire an [`AsyncRuntime`] inbox or to enqueue host-side
    /// callbacks before a script runs.
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
        let mut roots: Vec<*mut otter_gc::RawGc> = Vec::new();
        {
            let state = crate::runtime_state::RuntimeState::new(self);
            state.trace_roots(&mut |slot| roots.push(slot));
        }
        let mut visit = |sv: &mut dyn FnMut(*mut otter_gc::RawGc)| {
            for &p in &roots {
                sv(p);
            }
        };
        self.gc_heap.collect_full(&mut visit);
    }

    /// Execute `<main>` of `module` and return its completion value.
    ///
    /// # Errors
    /// Returns [`RunError`] (a `VmError` plus a stack-frame
    /// snapshot) on bytecode malformation, type mismatch, OOM,
    /// interrupt, or stack overflow.
    pub fn run(&mut self, module: &BytecodeModule) -> Result<Value, RunError> {
        match self.run_inner(module) {
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
    pub fn drain_microtasks(&mut self, module: &BytecodeModule) -> Result<(), RunError> {
        let mut iters: u32 = 0;
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
                if let Err(err) = self.invoke_microtask(module, task) {
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
        module: &BytecodeModule,
        task: Microtask,
    ) -> Result<(), RunError> {
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
            return self.run_async_resume(module, frame, await_dst, fulfilled, value);
        }
        if let MicrotaskKind::AsyncGenResume {
            frame,
            await_dst,
            fulfilled,
            owner,
        } = task.kind
        {
            let value = task.args.into_iter().next().unwrap_or(Value::Undefined);
            return self.run_async_gen_resume(module, frame, await_dst, fulfilled, value, owner);
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
                    let mut combined: SmallVec<[Value; 8]> =
                        SmallVec::with_capacity(bound.bound_args.len() + effective_args.len());
                    combined.extend(bound.bound_args.iter().cloned());
                    combined.extend(effective_args);
                    effective_this = bound.bound_this.clone();
                    effective_args = combined;
                    current = bound.target.clone();
                }
                Value::ClassConstructor(cc) => {
                    hops += 1;
                    current = cc.ctor.clone();
                }
                _ => break,
            }
        }
        // Native callables run inline at the drain site: no frame
        // push, no return register. Errors propagate as RunError.
        if let Value::NativeFunction(native) = &current {
            let argv: Vec<Value> = effective_args.into_iter().collect();
            let native = native.clone();
            return match (native.call)(self, &argv) {
                Ok(value) => {
                    self.settle_microtask_capability(result_capability, Ok(value));
                    Ok(())
                }
                Err(err) => {
                    let vm_err = native_to_vm_error(err);
                    if result_capability.is_some() {
                        // Reaction-mode: route the error into the
                        // downstream promise as a rejection rather
                        // than aborting the drain.
                        let reason = vm_err_to_value(&vm_err);
                        self.settle_microtask_capability(result_capability, Err(reason));
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
        let (function_id, parent_upvalues, this_for_callee) = match current {
            Value::Function { function_id } => {
                (function_id, std::rc::Rc::from(Vec::new()), effective_this)
            }
            Value::Closure {
                function_id,
                upvalues,
                bound_this,
            } => {
                let this_value = match bound_this {
                    Some(t) => *t,
                    None => effective_this,
                };
                (function_id, upvalues, this_value)
            }
            _ => {
                return Err(RunError {
                    error: VmError::NotCallable,
                    frames: Vec::new(),
                });
            }
        };
        let function = match module.functions.get(function_id as usize) {
            Some(f) => f,
            None => {
                return Err(RunError {
                    error: VmError::InvalidOperand,
                    frames: Vec::new(),
                });
            }
        };
        let upvalues = match Frame::build_upvalues(&mut self.gc_heap, function, parent_upvalues) {
            Ok(u) => u,
            Err(oom) => {
                return Err(RunError {
                    error: VmError::from(oom),
                    frames: Vec::new(),
                });
            }
        };
        let mut new_frame = Frame::with_return_upvalues_and_this(
            function,
            None, // top-level — no return register
            upvalues,
            this_for_callee,
        );
        let bind_count = (function.param_count as usize).min(effective_args.len());
        let total_args = effective_args.len();
        let mut iter = effective_args.into_iter();
        for i in 0..bind_count {
            let value = iter.next().expect("bind_count <= len");
            if let Some(slot) = new_frame.registers.get_mut(i) {
                *slot = value;
            }
        }
        if function.has_rest && total_args > function.param_count as usize {
            new_frame.rest_args = iter.collect();
        }
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(new_frame);
        match self.dispatch_loop(module, &mut stack) {
            Ok(value) => {
                // Reaction job: settle the downstream promise with
                // the handler's return value (spec §27.2.5.4).
                self.settle_microtask_capability(result_capability, Ok(value));
                Ok(())
            }
            Err(error) => {
                if result_capability.is_some() {
                    let reason = vm_err_to_value(&error);
                    self.settle_microtask_capability(result_capability, Err(reason));
                    Ok(())
                } else {
                    let frames = snapshot_frames(module, &stack);
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
            result_capability: None,
            kind: microtask::MicrotaskKind::Call,
        });
    }

    /// Internal driver. Pulls the snapshot capture out of the
    /// dispatch loop so the hot path remains allocation-free; the
    /// snapshot is built only when a `VmError` actually escapes.
    fn run_inner(
        &mut self,
        module: &BytecodeModule,
    ) -> Result<Value, (VmError, Vec<StackFrameSnapshot>)> {
        let main = module.main();
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut entry = Frame::for_function_with_heap(main, &mut self.gc_heap)
            .map_err(|oom| (VmError::from(oom), Vec::new()))?;
        // §16.2.1.7 ModuleDeclarationInstantiation step 5 — when the
        // entry function carries top-level await, wire up an async
        // result promise so `Op::Await` can park / resume normally.
        // The dispatch loop's exit returns the result promise's
        // resolved value once microtasks drain.
        let entry_promise = if main.is_async {
            let result = JsPromiseHandle::pending();
            entry.async_state = Some(AsyncFrameState {
                result_promise: result.clone(),
            });
            Some(result)
        } else {
            None
        };
        stack.push(entry);

        let dispatch_result = self.dispatch_loop(module, &mut stack);
        match dispatch_result {
            Ok(value) => {
                if let Some(promise) = entry_promise {
                    // Drain microtasks until the entry promise
                    // settles. The settled value (or rejection)
                    // becomes the program's completion value.
                    if let Err(err) = self.drain_microtasks(module) {
                        return Err((err.error, err.frames));
                    }
                    match promise.state() {
                        crate::promise::PromiseState::Fulfilled(v) => return Ok(v),
                        crate::promise::PromiseState::Rejected(reason) => {
                            return Err((
                                VmError::Uncaught {
                                    value: reason.display_string(),
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
                let frames = snapshot_frames(module, &stack);
                Err((err, frames))
            }
        }
    }

    /// Drive the dispatch loop, converting convertible `VmError`
    /// variants (TypeMismatch, NotCallable, TemporalDeadZone, etc.)
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
        module: &BytecodeModule,
        stack: &mut SmallVec<[Frame; 8]>,
    ) -> Result<Value, VmError> {
        loop {
            match self.dispatch_loop_inner(module, stack) {
                Ok(value) => return Ok(value),
                Err(err) => {
                    if let Some(thrown) = self.vm_error_to_throwable(&err) {
                        self.unwind_throw(stack, thrown)?;
                        if stack.is_empty() {
                            return Ok(Value::Undefined);
                        }
                        continue;
                    }
                    return Err(err);
                }
            }
        }
    }

    /// Convert a `VmError` raised by a dispatch step into a thrown
    /// `Error` instance. Returns `None` for variants that should
    /// keep propagating as host errors (StackOverflow, etc.).
    fn vm_error_to_throwable(&self, err: &VmError) -> Option<Value> {
        let dynamic_message: String;
        let (kind, message) = match err {
            VmError::TypeMismatch => (error_classes::ErrorKind::TypeError, "operand type mismatch"),
            VmError::NotCallable => (
                error_classes::ErrorKind::TypeError,
                "value is not a function",
            ),
            VmError::TemporalDeadZone { .. } => (
                error_classes::ErrorKind::ReferenceError,
                "cannot access binding before initialization",
            ),
            VmError::UndefinedIdentifier { name } => {
                dynamic_message = format!("{name} is not defined");
                (
                    error_classes::ErrorKind::ReferenceError,
                    dynamic_message.as_str(),
                )
            }
            VmError::UnknownIntrinsic { .. } => (
                error_classes::ErrorKind::TypeError,
                "unknown intrinsic method",
            ),
            // Hard / structural errors stay as host failures so the
            // caller surfaces them through `RunError` rather than
            // catching them as `try { ... } catch`.
            _ => return None,
        };
        let proto = self.error_classes.prototype(kind);
        let obj = JsObject::new();
        obj.set_prototype(Some(proto));
        let message_str = JsString::from_str(message, &self.string_heap).ok()?;
        obj.set("message", Value::String(message_str));
        Some(Value::Object(obj))
    }

    fn dispatch_loop_inner(
        &mut self,
        module: &BytecodeModule,
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
            let function = module
                .functions
                .get(function_id as usize)
                .ok_or(VmError::InvalidOperand)?;
            let pc = stack[top_idx].pc;
            let instr = function
                .code
                .get(pc as usize)
                .ok_or(VmError::MissingReturn)?;
            let op = instr.op;
            let operands = instr.operands.clone();

            // Stack-modifying opcodes go first so we don't hold a
            // `&mut Frame` borrow while pushing / popping.
            match op {
                Op::ReturnValue | Op::Return => {
                    let src = register_operand(operands.first())?;
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
                    self.do_call(stack, module, &operands)?;
                    continue;
                }
                Op::CallWithThis => {
                    self.do_call_with_this(stack, module, &operands)?;
                    continue;
                }
                Op::CallMethodValue => {
                    self.do_call_method_value(stack, module, &operands)?;
                    continue;
                }
                Op::CallSpread => {
                    self.do_call_spread(stack, module, &operands)?;
                    continue;
                }
                Op::New => {
                    self.do_construct(stack, module, &operands)?;
                    continue;
                }
                Op::NewSpread => {
                    self.do_construct_spread(stack, module, &operands)?;
                    continue;
                }
                Op::Throw => {
                    let src = register_operand(operands.first())?;
                    let value = stack[top_idx]
                        .registers
                        .get(src as usize)
                        .cloned()
                        .ok_or(VmError::InvalidOperand)?;
                    self.unwind_throw(stack, value)?;
                    continue;
                }
                Op::EndFinally => {
                    if let Some(value) = stack[top_idx].pending_throw.take() {
                        self.unwind_throw(stack, value)?;
                    } else {
                        stack[top_idx].pc = stack[top_idx]
                            .pc
                            .checked_add(1)
                            .ok_or(VmError::InvalidOperand)?;
                    }
                    continue;
                }
                Op::Await => {
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let awaited = read_register(&stack[top_idx], src)?.clone();
                    self.do_await(stack, dst, awaited)?;
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
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let yielded = read_register(&stack[top_idx], src)?.clone();
                    let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                    let owner = frame.generator_owner.clone().ok_or(VmError::TypeMismatch)?;
                    frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                    let popped = stack.pop().expect("frame present");
                    let pending_request_resolve = {
                        let mut body = owner.borrow_mut();
                        body.frame = Some(popped);
                        body.resume_dst = dst;
                        body.yielded = Some(yielded.clone());
                        if body.is_async {
                            body.pending_request.take().map(|cap| cap.resolve)
                        } else {
                            None
                        }
                    };
                    // §27.6 — async-generator yield settles the
                    // outer `.next()` promise immediately with
                    // `{value, done: false}`. Sync generators bubble
                    // the yielded value out so the `resume_generator`
                    // caller can shape it.
                    if let Some(resolve) = pending_request_resolve {
                        let record = make_iter_result(yielded.clone(), false);
                        self.run_callable_sync(
                            module,
                            &resolve,
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
                    if let Some(()) = self.try_to_primitive_dispatch(stack, module, &operands)? {
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
                    self.drive_to_primitive(stack, module, &operands)?;
                    continue;
                }
                // §7.4.3 `GetIterator`. Built-in iterables fall
                // through to the in-frame fast path; user objects
                // route through the call-frame ladder.
                // <https://tc39.es/ecma262/#sec-getiterator>
                Op::GetIterator => {
                    if self.drive_get_iterator(stack, module, &operands)? {
                        continue;
                    }
                }
                // §7.4.5 `IteratorNext`. Built-in iterators step
                // synchronously; user iterators push a call to
                // `iter.next()` and resume to extract `value` /
                // `done`.
                // <https://tc39.es/ecma262/#sec-iteratornext>
                Op::IteratorNext => {
                    if self.drive_iterator_next(stack, module, &operands)? {
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
                    if self.drive_load_property(stack, module, &operands)? {
                        continue;
                    }
                }
                // §10.1.9 [[Set]] — accessor setter dispatch follows
                // the same pattern as `LoadProperty`. Non-writable
                // and non-extensible rejections surface here too.
                // <https://tc39.es/ecma262/#sec-ordinaryset>
                Op::StoreProperty => {
                    if self.drive_store_property(stack, module, &operands)? {
                        continue;
                    }
                }
                // §28.2.4.7 / .10 Proxy.[[HasProperty]] /
                // [[Delete]] — invoke `has` / `deleteProperty`
                // traps when the receiver is a Proxy.
                Op::HasProperty => {
                    if self.drive_has_property_proxy(stack, module, &operands)? {
                        continue;
                    }
                }
                Op::DeleteProperty => {
                    if self.drive_delete_property_proxy(stack, module, &operands)? {
                        continue;
                    }
                }
                // §28.2.4.1 / .2 Proxy.[[GetPrototypeOf]] /
                // [[SetPrototypeOf]] — invoke `getPrototypeOf` /
                // `setPrototypeOf` traps when the receiver is a
                // Proxy.
                Op::GetPrototype => {
                    if self.drive_get_prototype_proxy(stack, module, &operands)? {
                        continue;
                    }
                }
                Op::SetPrototype => {
                    if self.drive_set_prototype_proxy(stack, module, &operands)? {
                        continue;
                    }
                }
                // §19.4.1 indirect eval — recursively dispatches a
                // freshly compiled module on a sub-stack, then
                // writes the completion value into `dst`. Stack-
                // modifying so it has to run before the in-frame
                // borrow below.
                Op::Eval => {
                    let dst = register_operand(operands.first())?;
                    let src_reg = register_operand(operands.get(1))?;
                    let top_idx = stack.len() - 1;
                    let value = read_register(&stack[top_idx], src_reg)?.clone();
                    let result = self.run_eval(&value)?;
                    let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                    write_register(frame, dst, result)?;
                    frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                    continue;
                }
                // §20.2.1.1 — `new Function(args, body)` recurses
                // into the eval hook with a synthesised wrapper.
                Op::NewFunction => {
                    let dst = register_operand(operands.first())?;
                    let argc = match operands.get(1) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let top_idx = stack.len() - 1;
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(2 + i))?;
                        args.push(read_register(&stack[top_idx], r)?.clone());
                    }
                    let result = self.build_function_constructor(&args)?;
                    let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                    write_register(frame, dst, result)?;
                    frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                    continue;
                }
                _ => {}
            }

            let frame = &mut stack[top_idx];
            match op {
                Op::Nop => {
                    frame.pc += 1;
                }
                Op::LoadUndefined => {
                    let dst = register_operand(operands.first())?;
                    write_register(frame, dst, Value::Undefined)?;
                    frame.pc += 1;
                }
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
                Op::MakeFunction => {
                    let dst = register_operand(operands.first())?;
                    let idx = const_operand(operands.get(1))?;
                    let function_id = match module.constants.get(idx as usize) {
                        Some(Constant::FunctionId { index }) => *index,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    write_register(frame, dst, Value::Function { function_id })?;
                    frame.pc += 1;
                }
                Op::MakeClosure => {
                    let dst = register_operand(operands.first())?;
                    let idx = const_operand(operands.get(1))?;
                    let function_id = match module.constants.get(idx as usize) {
                        Some(Constant::FunctionId { index }) => *index,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let count = match operands.get(2) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let mut cells: Vec<UpvalueCell> = Vec::with_capacity(count);
                    for i in 0..count {
                        let parent_idx = match operands.get(3 + i) {
                            Some(&Operand::Imm32(n)) if n >= 0 => n as usize,
                            _ => return Err(VmError::InvalidOperand),
                        };
                        let cell = *frame
                            .upvalues
                            .get(parent_idx)
                            .ok_or(VmError::InvalidOperand)?;
                        cells.push(cell);
                    }
                    let upvalues: std::rc::Rc<[UpvalueCell]> = std::rc::Rc::from(cells);
                    // Arrow-closure receivers are bound lexically:
                    // every later invocation ignores the call site
                    // and uses the enclosing frame's `this`.
                    let is_arrow = module
                        .functions
                        .get(function_id as usize)
                        .map(|f| f.is_arrow)
                        .unwrap_or(false);
                    let bound_this = if is_arrow {
                        Some(Box::new(frame.this_value.clone()))
                    } else {
                        None
                    };
                    write_register(
                        frame,
                        dst,
                        Value::Closure {
                            function_id,
                            upvalues,
                            bound_this,
                        },
                    )?;
                    frame.pc += 1;
                }
                Op::LoadUpvalue => {
                    let dst = register_operand(operands.first())?;
                    let idx = imm32_operand(operands.get(1))?;
                    if idx < 0 {
                        return Err(VmError::InvalidOperand);
                    }
                    let cell = *frame
                        .upvalues
                        .get(idx as usize)
                        .ok_or(VmError::InvalidOperand)?;
                    let value = read_upvalue(&self.gc_heap, cell);
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
                }
                Op::StoreUpvalue => {
                    let src = register_operand(operands.first())?;
                    let idx = imm32_operand(operands.get(1))?;
                    if idx < 0 {
                        return Err(VmError::InvalidOperand);
                    }
                    let value = read_register(frame, src)?.clone();
                    let cell = *frame
                        .upvalues
                        .get(idx as usize)
                        .ok_or(VmError::InvalidOperand)?;
                    store_upvalue(&mut self.gc_heap, cell, value);
                    frame.pc += 1;
                }
                Op::LoadString => {
                    let dst = register_operand(operands.first())?;
                    let idx = const_operand(operands.get(1))?;
                    let units = match module.constants.get(idx as usize) {
                        Some(otter_bytecode::Constant::String { utf16 }) => utf16.as_slice(),
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let s = JsString::from_utf16_units(units, &self.string_heap)?;
                    write_register(frame, dst, Value::String(s))?;
                    frame.pc += 1;
                }
                Op::LoadLength => {
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let s = read_register(frame, src)?
                        .as_string()
                        .ok_or(VmError::TypeMismatch)?;
                    let len = NumberValue::from_i32(s.len() as i32);
                    write_register(frame, dst, Value::Number(len))?;
                    frame.pc += 1;
                }
                Op::LoadNumber => {
                    let dst = register_operand(operands.first())?;
                    let idx = const_operand(operands.get(1))?;
                    let value = match module.constants.get(idx as usize) {
                        Some(Constant::Number { bits }) => {
                            NumberValue::from_f64(f64::from_bits(*bits))
                        }
                        _ => return Err(VmError::InvalidOperand),
                    };
                    write_register(frame, dst, Value::Number(value))?;
                    frame.pc += 1;
                }
                Op::LoadInt32 => {
                    let dst = register_operand(operands.first())?;
                    let imm = match operands.get(1) {
                        Some(&Operand::Imm32(v)) => v,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    write_register(frame, dst, Value::Number(NumberValue::Smi(imm)))?;
                    frame.pc += 1;
                }
                Op::LoadBigInt => {
                    let dst = register_operand(operands.first())?;
                    let idx = const_operand(operands.get(1))?;
                    let value = match module.constants.get(idx as usize) {
                        Some(Constant::BigInt { decimal }) => {
                            bigint::BigIntValue::from_decimal(decimal)
                                .ok_or(VmError::InvalidOperand)?
                        }
                        _ => return Err(VmError::InvalidOperand),
                    };
                    write_register(frame, dst, Value::BigInt(value))?;
                    frame.pc += 1;
                }
                Op::LoadRegExp => {
                    // Foundation path: compile once per load. Per-
                    // literal caching is task 31's explicit non-goal.
                    let dst = register_operand(operands.first())?;
                    let idx = const_operand(operands.get(1))?;
                    let regex = match module.constants.get(idx as usize) {
                        Some(Constant::RegExp {
                            pattern_utf16,
                            flags,
                        }) => regexp::JsRegExp::compile(pattern_utf16, flags).map_err(|e| {
                            VmError::InvalidRegExp {
                                message: e.to_string(),
                            }
                        })?,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    write_register(frame, dst, Value::RegExp(regex))?;
                    frame.pc += 1;
                }
                Op::LoadTrue => {
                    let dst = register_operand(operands.first())?;
                    write_register(frame, dst, Value::Boolean(true))?;
                    frame.pc += 1;
                }
                Op::LoadFalse => {
                    let dst = register_operand(operands.first())?;
                    write_register(frame, dst, Value::Boolean(false))?;
                    frame.pc += 1;
                }
                Op::LoadNull => {
                    let dst = register_operand(operands.first())?;
                    write_register(frame, dst, Value::Null)?;
                    frame.pc += 1;
                }
                Op::LogicalNot => {
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let truthy = read_register(frame, src)?.to_boolean();
                    write_register(frame, dst, Value::Boolean(!truthy))?;
                    frame.pc += 1;
                }
                Op::ToBoolean => {
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let truthy = read_register(frame, src)?.to_boolean();
                    write_register(frame, dst, Value::Boolean(truthy))?;
                    frame.pc += 1;
                }
                Op::Jump => {
                    let offset = imm32_operand(operands.first())?;
                    apply_branch(frame, offset, &self.interrupt)?;
                }
                Op::JumpIfTrue => {
                    let offset = imm32_operand(operands.first())?;
                    let cond = register_operand(operands.get(1))?;
                    if read_register(frame, cond)?.to_boolean() {
                        apply_branch(frame, offset, &self.interrupt)?;
                    } else {
                        frame.pc += 1;
                    }
                }
                Op::JumpIfFalse => {
                    let offset = imm32_operand(operands.first())?;
                    let cond = register_operand(operands.get(1))?;
                    if !read_register(frame, cond)?.to_boolean() {
                        apply_branch(frame, offset, &self.interrupt)?;
                    } else {
                        frame.pc += 1;
                    }
                }
                Op::JumpIfNullish => {
                    let offset = imm32_operand(operands.first())?;
                    let cond = register_operand(operands.get(1))?;
                    if read_register(frame, cond)?.is_nullish() {
                        apply_branch(frame, offset, &self.interrupt)?;
                    } else {
                        frame.pc += 1;
                    }
                }
                Op::LoadLocal => {
                    let dst = register_operand(operands.first())?;
                    let idx = imm32_operand(operands.get(1))?;
                    let value = read_register(frame, idx as u16)?.clone();
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
                }
                Op::StoreLocal => {
                    let src = register_operand(operands.first())?;
                    let idx = imm32_operand(operands.get(1))?;
                    let value = read_register(frame, src)?.clone();
                    write_register(frame, idx as u16, value)?;
                    frame.pc += 1;
                }
                Op::TdzError => {
                    return Err(VmError::TemporalDeadZone {
                        local_index: imm32_operand(operands.first())? as u32,
                    });
                }
                Op::NewObject => {
                    let dst = register_operand(operands.first())?;
                    write_register(frame, dst, Value::Object(JsObject::new()))?;
                    frame.pc += 1;
                }
                Op::LoadProperty => {
                    let dst = register_operand(operands.first())?;
                    let obj_reg = register_operand(operands.get(1))?;
                    let name_idx = const_operand(operands.get(2))?;
                    let name = lookup_string_constant(module, name_idx)?;
                    let value = match read_register(frame, obj_reg)? {
                        Value::Object(o) => o.get(&name).unwrap_or(Value::Undefined),
                        Value::ClassConstructor(c) => {
                            if name == "prototype" {
                                Value::Object(c.prototype.clone())
                            } else {
                                match c.statics.get(&name) {
                                    Some(v) => v,
                                    None if name == "name" || name == "length" => {
                                        // Fall back to the underlying
                                        // ctor's intrinsic property
                                        // when the user hasn't shadowed
                                        // it via a static field.
                                        match &c.ctor {
                                            Value::Function { function_id }
                                            | Value::Closure { function_id, .. } => {
                                                function_intrinsic_property(
                                                    module,
                                                    *function_id,
                                                    &name,
                                                    &self.string_heap,
                                                )?
                                            }
                                            _ => Value::Undefined,
                                        }
                                    }
                                    None => Value::Undefined,
                                }
                            }
                        }
                        Value::String(s) if name == "length" => {
                            Value::Number(NumberValue::from_i32(s.len() as i32))
                        }
                        Value::Array(a) => a.get_named_property(&name).unwrap_or(Value::Undefined),
                        // §20.2.4 Function-instance properties — every
                        // callable carries `.name` / `.length` /
                        // `.prototype`; user writes through the side
                        // table take precedence per ordinary [[Get]]
                        // semantics, and `.prototype` auto-allocates
                        // on first access (§9.2.10 MakeConstructor).
                        // <https://tc39.es/ecma262/#sec-function-instances>
                        Value::Function { function_id } => {
                            let fid = *function_id;
                            self.function_property_get(module, fid, &name)?
                        }
                        Value::Closure { function_id, .. } => {
                            let fid = *function_id;
                            self.function_property_get(module, fid, &name)?
                        }
                        Value::NativeFunction(native) if name == "name" || name == "length" => {
                            if name == "name" {
                                let s = JsString::from_str(native.name, &self.string_heap)
                                    .map_err(|_| VmError::TypeMismatch)?;
                                Value::String(s)
                            } else {
                                Value::Number(NumberValue::from_i32(0))
                            }
                        }
                        Value::BoundFunction(bound) if name == "name" || name == "length" => {
                            bound_function_intrinsic_property(
                                module,
                                bound,
                                &name,
                                &self.string_heap,
                            )?
                        }
                        Value::RegExp(r) => {
                            regexp_prototype::load_property(r, &name, &self.string_heap)
                        }
                        Value::Symbol(s) => symbol_prototype::load_property(s, &name),
                        v @ (Value::Map(_)
                        | Value::Set(_)
                        | Value::WeakMap(_)
                        | Value::WeakSet(_)) => collections_prototype::load_property(v, &name),
                        Value::Temporal(t) => temporal::load_property(t, &name),
                        Value::ArrayBuffer(b) => {
                            binary::array_buffer_prototype::load_property(b, &name)
                        }
                        Value::DataView(v) => binary::data_view_prototype::load_property(v, &name),
                        Value::TypedArray(t) => {
                            binary::typed_array_prototype::load_property(t, &name)
                        }
                        _ => return Err(VmError::TypeMismatch),
                    };
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
                }
                Op::StoreProperty => {
                    let obj_reg = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let src = register_operand(operands.get(2))?;
                    let name = lookup_string_constant(module, name_idx)?;
                    let value = read_register(frame, src)?.clone();
                    let target = match read_register(frame, obj_reg)? {
                        Value::Object(o) => Some(o.clone()),
                        Value::ClassConstructor(c) => Some(c.statics.clone()),
                        Value::RegExp(r) => {
                            regexp_prototype::store_property(r, &name, &value);
                            None
                        }
                        Value::Array(a) => {
                            // §10.4.2.1 [[DefineOwnProperty]] for
                            // arrays: indexed names route to the
                            // dense element table; non-index names
                            // land in the optional named-property
                            // bag. `length` writes are filed.
                            a.set_named_property(&name, value.clone());
                            None
                        }
                        // §9.2 Function-instance ordinary [[Set]]:
                        // `f.foo = 1`, `Ctor.prototype = obj`, etc.
                        // The function-property side table (keyed
                        // by function_id) is shared across closure
                        // handles for the same compiled function so
                        // every closure observes the same bag.
                        Value::Function { function_id } | Value::Closure { function_id, .. } => {
                            let fid = *function_id;
                            Some(self.function_user_props.entry(fid).or_default().clone())
                        }
                        _ => return Err(VmError::TypeMismatch),
                    };
                    if let Some(target) = target {
                        target.set(&name, value);
                    }
                    frame.pc += 1;
                }
                Op::DeleteProperty => {
                    let dst = register_operand(operands.first())?;
                    let obj_reg = register_operand(operands.get(1))?;
                    let name_idx = const_operand(operands.get(2))?;
                    let name = lookup_string_constant(module, name_idx)?;
                    let obj = match read_register(frame, obj_reg)? {
                        Value::Object(o) => o.clone(),
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let removed = obj.delete(&name);
                    write_register(frame, dst, Value::Boolean(removed))?;
                    frame.pc += 1;
                }
                Op::GetPrototype => {
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let result = match read_register(frame, src)? {
                        Value::Object(o) => match o.prototype() {
                            Some(p) => Value::Object(p),
                            None => Value::Null,
                        },
                        _ => return Err(VmError::TypeMismatch),
                    };
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                Op::SetPrototype => {
                    let obj_reg = register_operand(operands.first())?;
                    let proto_reg = register_operand(operands.get(1))?;
                    let obj = match read_register(frame, obj_reg)? {
                        Value::Object(o) => o.clone(),
                        _ => return Err(VmError::TypeMismatch),
                    };
                    // Class values chain through their statics
                    // object — `class D extends C` sets
                    // `D.statics.[[Prototype]] = C.statics` so
                    // `D.staticMethod` walks up to `C.staticMethod`
                    // through the existing prototype lookup.
                    let proto = match read_register(frame, proto_reg)? {
                        Value::Object(p) => Some(p.clone()),
                        Value::ClassConstructor(c) => Some(c.statics.clone()),
                        Value::Null => None,
                        _ => return Err(VmError::TypeMismatch),
                    };
                    obj.set_prototype(proto);
                    frame.pc += 1;
                }
                Op::NewArray => {
                    let dst = register_operand(operands.first())?;
                    let count = match operands.get(1) {
                        Some(&Operand::ConstIndex(n)) => n,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let mut elements: SmallVec<[Value; 4]> =
                        SmallVec::with_capacity(count as usize);
                    for i in 0..count as usize {
                        let r = register_operand(operands.get(2 + i))?;
                        elements.push(read_register(frame, r)?.clone());
                    }
                    write_register(frame, dst, Value::Array(JsArray::from_elements(elements)))?;
                    frame.pc += 1;
                }
                Op::LoadElement => {
                    let dst = register_operand(operands.first())?;
                    let recv_reg = register_operand(operands.get(1))?;
                    let idx_reg = register_operand(operands.get(2))?;
                    let recv = read_register(frame, recv_reg)?.clone();
                    let idx_value = read_register(frame, idx_reg)?.clone();
                    let value = match (&recv, &idx_value) {
                        // Symbol-keyed property access on objects —
                        // foundation §7.4 (well-known symbols) +
                        // §10.1 (ordinary objects). Arrays delegate
                        // through their `JsObject`-style symbol
                        // store too once the well-known iterator
                        // exposes a callable (see below).
                        (Value::Object(obj), Value::Symbol(sym)) => {
                            obj.get_symbol(sym).unwrap_or(Value::Undefined)
                        }
                        // String-keyed access on objects with
                        // computed names: `obj["foo"]` — falls back
                        // to the string property table.
                        (Value::Object(obj), Value::String(key)) => {
                            obj.get(&key.to_lossy_string()).unwrap_or(Value::Undefined)
                        }
                        // `arr[Symbol.iterator]` — return a native
                        // callable producing the foundation
                        // iterator state for the array.
                        (Value::Array(arr), Value::Symbol(sym))
                            if sym
                                .well_known_tag()
                                .is_some_and(|t| t == symbol::WellKnown::Iterator) =>
                        {
                            make_array_iterator_factory(arr.clone())
                        }
                        // `map[Symbol.iterator]` aliases `entries` per
                        // Spec §24.1.3.12; `set[Symbol.iterator]`
                        // aliases `values` per §24.2.3.11.
                        (Value::Map(m), Value::Symbol(sym))
                            if sym
                                .well_known_tag()
                                .is_some_and(|t| t == symbol::WellKnown::Iterator) =>
                        {
                            collections_prototype::make_map_iterator_factory(m.clone())
                        }
                        (Value::Set(s), Value::Symbol(sym))
                            if sym
                                .well_known_tag()
                                .is_some_and(|t| t == symbol::WellKnown::Iterator) =>
                        {
                            collections_prototype::make_set_iterator_factory(s.clone())
                        }
                        // Numeric-indexed array / string element
                        // reads.
                        _ => {
                            let idx = match &idx_value {
                                Value::Number(n) => match n.as_smi() {
                                    Some(v) if v >= 0 => v as usize,
                                    _ => return Err(VmError::TypeMismatch),
                                },
                                _ => return Err(VmError::TypeMismatch),
                            };
                            match recv {
                                Value::Array(a) => a.get(idx),
                                Value::String(s) => match s.char_code_at(idx as u32) {
                                    Some(unit) => Value::String(crate::JsString::from_utf16_units(
                                        &[unit],
                                        &self.string_heap,
                                    )?),
                                    None => {
                                        Value::String(crate::JsString::empty(&self.string_heap)?)
                                    }
                                },
                                // §10.4.5.13 IntegerIndexedElementGet
                                // <https://tc39.es/ecma262/#sec-integerindexedelementget>
                                Value::TypedArray(t) => t.get(idx),
                                _ => return Err(VmError::TypeMismatch),
                            }
                        }
                    };
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
                }
                Op::StoreElement => {
                    let recv_reg = register_operand(operands.first())?;
                    let idx_reg = register_operand(operands.get(1))?;
                    let src_reg = register_operand(operands.get(2))?;
                    let recv = read_register(frame, recv_reg)?.clone();
                    let idx_value = read_register(frame, idx_reg)?.clone();
                    let value = read_register(frame, src_reg)?.clone();
                    match (&recv, &idx_value) {
                        // Symbol-keyed write on an object.
                        (Value::Object(obj), Value::Symbol(sym)) => {
                            obj.set_symbol(sym.clone(), value);
                        }
                        // Computed string-key write (`obj["k"] = …`).
                        (Value::Object(obj), Value::String(key)) => {
                            obj.set(&key.to_lossy_string(), value);
                        }
                        // Numeric-indexed array write.
                        (Value::Array(arr), Value::Number(n)) => match n.as_smi() {
                            Some(v) if v >= 0 => arr.set(v as usize, value),
                            _ => return Err(VmError::TypeMismatch),
                        },
                        // §10.4.5.14 IntegerIndexedElementSet — out-of-
                        // range indices silently no-op; element-type /
                        // value-type mismatches raise TypeError.
                        // <https://tc39.es/ecma262/#sec-integerindexedelementset>
                        (Value::TypedArray(t), Value::Number(n)) => match n.as_smi() {
                            Some(v) if v >= 0 => {
                                let coerced =
                                    binary::dispatch::coerce_element_for_store(t.kind(), &value)?;
                                t.set(v as usize, &coerced);
                            }
                            _ => return Err(VmError::TypeMismatch),
                        },
                        _ => return Err(VmError::TypeMismatch),
                    }
                    frame.pc += 1;
                }
                Op::ArrayLength => {
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let arr = match read_register(frame, src)? {
                        Value::Array(a) => a.clone(),
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let n = NumberValue::from_i32(arr.len() as i32);
                    write_register(frame, dst, Value::Number(n))?;
                    frame.pc += 1;
                }
                Op::Instanceof => {
                    // ECMA-262 §13.10.2 InstanceofOperator —
                    // OrdinaryHasInstance fallback: walk
                    // `lhs.[[Prototype]]` looking for `rhs.prototype`
                    // (or just `rhs` itself, kept as a
                    // backwards-compatible foundation shape so
                    // `obj instanceof proto` still works for tests
                    // that pass a raw prototype object).
                    //
                    // <https://tc39.es/ecma262/#sec-ordinaryhasinstance>
                    let (dst, lhs, rhs) = self.binop_regs(&operands, frame)?;
                    let result = match (&lhs, &rhs) {
                        (Value::Object(a), Value::Object(target)) => {
                            // Spec path: `target.prototype` is the
                            // proto object instances inherit from.
                            // When `target.prototype` resolves to an
                            // object, walk the chain against it.
                            // Otherwise fall back to walking the
                            // chain against `target` directly so
                            // older fixtures that pass a prototype
                            // object as rhs still work.
                            match target.get("prototype") {
                                Some(Value::Object(proto)) => a.has_in_proto_chain(&proto),
                                _ => a.has_in_proto_chain(target),
                            }
                        }
                        // §13.10.2 — for class values, walk the
                        // proto chain against `class.prototype`.
                        (Value::Object(a), Value::ClassConstructor(c)) => {
                            a.has_in_proto_chain(&c.prototype)
                        }
                        _ => false,
                    };
                    write_register(frame, dst, Value::Boolean(result))?;
                    frame.pc += 1;
                }
                // §13.10.1 / §7.3.10 HasProperty — `key in obj`.
                // Right operand must be an Object. The left operand
                // is coerced via §7.1.19 ToPropertyKey: strings stay
                // as-is, symbols stay as-is, anything else coerces
                // to its display string.
                // <https://tc39.es/ecma262/#sec-hasproperty>
                Op::HasProperty => {
                    let (dst, lhs, rhs) = self.binop_regs(&operands, frame)?;
                    let present = match &rhs {
                        Value::Object(obj) => match &lhs {
                            Value::Symbol(s) => obj.get_symbol(s).is_some(),
                            Value::String(s) => {
                                let key = s.to_lossy_string();
                                !matches!(obj.lookup(&key), object::PropertyLookup::Absent)
                            }
                            Value::Number(n) => {
                                let key = n.to_display_string();
                                !matches!(obj.lookup(&key), object::PropertyLookup::Absent)
                            }
                            other => {
                                let key = other.display_string();
                                !matches!(obj.lookup(&key), object::PropertyLookup::Absent)
                            }
                        },
                        Value::Array(arr) => match &lhs {
                            // §10.4.2 ArrayExoticObject: indexed
                            // properties are present iff index is
                            // in `[0, len)`. The string `"length"`
                            // is also always present.
                            Value::Number(n) => match n.as_smi() {
                                Some(i) if i >= 0 => (i as usize) < arr.len(),
                                _ => false,
                            },
                            Value::String(s) => {
                                let key = s.to_lossy_string();
                                if key == "length" {
                                    true
                                } else if let Ok(i) = key.parse::<usize>() {
                                    i < arr.len()
                                } else {
                                    false
                                }
                            }
                            _ => false,
                        },
                        Value::ClassConstructor(c) => {
                            // Static side: "prototype" plus whatever
                            // the statics object carries.
                            match &lhs {
                                Value::String(s) if s.to_lossy_string() == "prototype" => true,
                                Value::String(s) => !matches!(
                                    c.statics.lookup(&s.to_lossy_string()),
                                    object::PropertyLookup::Absent
                                ),
                                _ => false,
                            }
                        }
                        _ => return Err(VmError::TypeMismatch),
                    };
                    write_register(frame, dst, Value::Boolean(present))?;
                    frame.pc += 1;
                }
                Op::SameValue => {
                    // `Object.is(x, y)` — ECMA-262 §7.2.11.
                    // <https://tc39.es/ecma262/#sec-samevalue>
                    let (dst, lhs, rhs) = self.binop_regs(&operands, frame)?;
                    let result = abstract_ops::same_value(&lhs, &rhs);
                    write_register(frame, dst, Value::Boolean(result))?;
                    frame.pc += 1;
                }
                Op::IsArray => {
                    // `Array.isArray(v)` — ECMA-262 §7.2.2.
                    // <https://tc39.es/ecma262/#sec-isarray>
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let value = read_register(frame, src)?.clone();
                    let result = abstract_ops::is_array(&value);
                    write_register(frame, dst, Value::Boolean(result))?;
                    frame.pc += 1;
                }
                Op::Add => {
                    self.run_add(module, &operands, frame)?;
                }
                Op::Sub => {
                    self.run_numeric(&operands, frame, number::sub, bigint_sub_op)?;
                }
                Op::Mul => {
                    self.run_numeric(&operands, frame, number::mul, bigint_mul_op)?;
                }
                Op::Div => {
                    self.run_numeric(&operands, frame, number::div, bigint::ops::div)?;
                }
                Op::Rem => {
                    self.run_numeric(&operands, frame, number::rem, bigint::ops::rem)?;
                }
                Op::Pow => {
                    self.run_numeric(&operands, frame, number::pow, bigint::ops::pow)?;
                }
                Op::BitwiseAnd => {
                    self.run_numeric(&operands, frame, number::bitwise_and, bigint_and_op)?;
                }
                Op::BitwiseOr => {
                    self.run_numeric(&operands, frame, number::bitwise_or, bigint_or_op)?;
                }
                Op::BitwiseXor => {
                    self.run_numeric(&operands, frame, number::bitwise_xor, bigint_xor_op)?;
                }
                Op::Shl => {
                    self.run_numeric(&operands, frame, number::shl, bigint::ops::shl)?;
                }
                Op::Shr => {
                    self.run_numeric(&operands, frame, number::shr_arith, bigint::ops::shr)?;
                }
                Op::Ushr => {
                    // §13.10 `>>>` — BigInt operands raise TypeError;
                    // every other primitive is coerced via
                    // ToNumber. Compiler ToPrimitive(number) ahead.
                    // <https://tc39.es/ecma262/#sec-unsigned-right-shift-operator>
                    let (dst, lhs, rhs) = self.binop_regs(&operands, frame)?;
                    let lk = abstract_ops::to_numeric_kind(&lhs).ok_or(VmError::TypeMismatch)?;
                    let rk = abstract_ops::to_numeric_kind(&rhs).ok_or(VmError::TypeMismatch)?;
                    let result = match (lk, rk) {
                        (abstract_ops::NumericKind::Num(a), abstract_ops::NumericKind::Num(b)) => {
                            Value::Number(number::shr_logical(a, b))
                        }
                        _ => return Err(VmError::TypeMismatch),
                    };
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                Op::Neg => {
                    // §13.5.6 unary `-`: ToNumeric, then negate.
                    // Compiler emits ToPrimitive(number) ahead of
                    // this op so we only see primitives.
                    // <https://tc39.es/ecma262/#sec-unary-minus-operator>
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let v = read_register(frame, src)?.clone();
                    let value = match abstract_ops::to_numeric_kind(&v)
                        .ok_or(VmError::TypeMismatch)?
                    {
                        abstract_ops::NumericKind::Num(n) => Value::Number(number::neg(n)),
                        abstract_ops::NumericKind::Big(b) => Value::BigInt(bigint::ops::neg(&b)),
                    };
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
                }
                Op::BitwiseNot => {
                    // §13.5.7 unary `~`: ToNumeric, then bitwise
                    // not. BigInt stays BigInt; otherwise Number.
                    // <https://tc39.es/ecma262/#sec-bitwise-not-operator>
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let v = read_register(frame, src)?.clone();
                    let value = match abstract_ops::to_numeric_kind(&v)
                        .ok_or(VmError::TypeMismatch)?
                    {
                        abstract_ops::NumericKind::Num(n) => Value::Number(number::bitwise_not(n)),
                        abstract_ops::NumericKind::Big(b) => {
                            Value::BigInt(bigint::ops::bitwise_not(&b))
                        }
                    };
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
                }
                Op::ToNumber => {
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let value = match read_register(frame, src)? {
                        Value::Number(n) => *n,
                        Value::Boolean(true) => NumberValue::Smi(1),
                        Value::Boolean(false) | Value::Null => NumberValue::Smi(0),
                        // Spec ToNumber(BigInt) is a TypeError; we
                        // surface it here so the unary `+` operator
                        // doesn't silently coerce.
                        Value::BigInt(_) => return Err(VmError::TypeMismatch),
                        // Spec ToNumber(Symbol) is a TypeError per
                        // §7.1.4 step 4.
                        Value::Symbol(_) => return Err(VmError::TypeMismatch),
                        Value::Undefined
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
                        | Value::Temporal(_)
                        | Value::Intl(_)
                        | Value::ArrayBuffer(_)
                        | Value::DataView(_)
                        | Value::TypedArray(_)
                        | Value::Generator(_)
                        | Value::Proxy(_) => NumberValue::Double(f64::NAN),
                        // §21.4.4.45 Date.prototype[@@toPrimitive] —
                        // ToNumber on a Date returns its time value.
                        Value::Date(d) => NumberValue::from_f64(d.time()),
                        Value::String(s) => number::to_number_from_string(&s.to_lossy_string()),
                    };
                    write_register(frame, dst, Value::Number(value))?;
                    frame.pc += 1;
                }
                Op::Equal => {
                    let (dst, lhs, rhs) = self.binop_regs(&operands, frame)?;
                    let eq = lhs == rhs;
                    write_register(frame, dst, Value::Boolean(eq))?;
                    frame.pc += 1;
                }
                Op::NotEqual => {
                    let (dst, lhs, rhs) = self.binop_regs(&operands, frame)?;
                    let eq = lhs == rhs;
                    write_register(frame, dst, Value::Boolean(!eq))?;
                    frame.pc += 1;
                }
                Op::LooseEqual => {
                    // ECMA-262 §7.2.13. The compiler has already
                    // coerced both operands through
                    // `Op::ToPrimitive(default)`, so the runtime
                    // sees primitives only.
                    // <https://tc39.es/ecma262/#sec-islooselyequal>
                    let (dst, lhs, rhs) = self.binop_regs(&operands, frame)?;
                    let eq = abstract_ops::is_loosely_equal(&lhs, &rhs);
                    write_register(frame, dst, Value::Boolean(eq))?;
                    frame.pc += 1;
                }
                Op::LooseNotEqual => {
                    let (dst, lhs, rhs) = self.binop_regs(&operands, frame)?;
                    let eq = abstract_ops::is_loosely_equal(&lhs, &rhs);
                    write_register(frame, dst, Value::Boolean(!eq))?;
                    frame.pc += 1;
                }
                Op::LessThan | Op::LessEq | Op::GreaterThan | Op::GreaterEq => {
                    self.run_compare(&operands, frame, op)?;
                }
                Op::GetStringIndex => {
                    let dst = register_operand(operands.first())?;
                    let recv = register_operand(operands.get(1))?;
                    let idx_reg = register_operand(operands.get(2))?;
                    let recv_s = read_register(frame, recv)?
                        .as_string()
                        .ok_or(VmError::TypeMismatch)?
                        .clone();
                    let idx = match read_register(frame, idx_reg)? {
                        Value::Number(n) => match n.as_smi() {
                            Some(v) if v >= 0 => v as u32,
                            _ => recv_s.len(), // out of range → empty
                        },
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let result_str = match recv_s.char_code_at(idx) {
                        Some(unit) => JsString::from_utf16_units(&[unit], &self.string_heap)?,
                        None => JsString::empty(&self.string_heap)?,
                    };
                    write_register(frame, dst, Value::String(result_str))?;
                    frame.pc += 1;
                }
                Op::LoadThis => {
                    let dst = register_operand(operands.first())?;
                    let value = frame.this_value.clone();
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
                }
                Op::NewError => {
                    // Foundation `new Error(arg)` shape — preserved
                    // alongside the wider [`Op::NewBuiltinError`]
                    // opcode for backwards compatibility with already-
                    // shipped fixtures. Both routes consult the per-
                    // interpreter [`ErrorClassRegistry`] so prototype
                    // identity matches `instanceof Error`.
                    //
                    // <https://tc39.es/ecma262/#sec-error-constructor>
                    let dst = register_operand(operands.first())?;
                    let msg_reg = register_operand(operands.get(1))?;
                    let value = read_register(frame, msg_reg)?.clone();
                    let owned_message: Option<String> = match value {
                        Value::Undefined => None,
                        Value::String(s) => Some(s.to_lossy_string()),
                        other => Some(other.display_string()),
                    };
                    let obj = self.error_classes.make_instance(
                        ErrorKind::Error,
                        owned_message.as_deref(),
                        &self.string_heap,
                    )?;
                    write_register(frame, dst, Value::Object(obj))?;
                    frame.pc += 1;
                }
                Op::NewBuiltinError => {
                    // ECMA-262 §19.3 / §20.5 native error
                    // constructors. The compiler resolves the
                    // identifier to an [`ErrorKind`] before emitting
                    // this opcode, so a missing variant in the
                    // constant pool is a compiler bug surfaced as
                    // `InvalidOperand`.
                    //
                    // <https://tc39.es/ecma262/#sec-error-objects>
                    let dst = register_operand(operands.first())?;
                    let kind_idx = const_operand(operands.get(1))?;
                    let msg_reg = register_operand(operands.get(2))?;
                    let kind_name = lookup_string_constant(module, kind_idx)?;
                    let kind =
                        ErrorKind::from_class_name(&kind_name).ok_or(VmError::InvalidOperand)?;
                    let value = read_register(frame, msg_reg)?.clone();
                    let owned_message: Option<String> = match value {
                        Value::Undefined => None,
                        Value::String(s) => Some(s.to_lossy_string()),
                        other => Some(other.display_string()),
                    };
                    let obj = self.error_classes.make_instance(
                        kind,
                        owned_message.as_deref(),
                        &self.string_heap,
                    )?;
                    write_register(frame, dst, Value::Object(obj))?;
                    frame.pc += 1;
                }
                Op::LoadBuiltinError => {
                    // Resolve a bare identifier reference (e.g.
                    // `e instanceof TypeError`) to the matching
                    // constructor object out of
                    // [`ErrorClassRegistry`].
                    //
                    // <https://tc39.es/ecma262/#sec-error-objects>
                    let dst = register_operand(operands.first())?;
                    let kind_idx = const_operand(operands.get(1))?;
                    let kind_name = lookup_string_constant(module, kind_idx)?;
                    let kind =
                        ErrorKind::from_class_name(&kind_name).ok_or(VmError::InvalidOperand)?;
                    let ctor = self.error_classes.constructor(kind);
                    write_register(frame, dst, Value::Object(ctor))?;
                    frame.pc += 1;
                }
                Op::MathLoad => {
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let name = lookup_string_constant(module, name_idx)?;
                    let value =
                        math::load_constant(&name).ok_or_else(|| VmError::UnknownIntrinsic {
                            name: format!("Math.{name}"),
                        })?;
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
                }
                Op::MathCall => {
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let argc = match operands.get(2) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let name = lookup_string_constant(module, name_idx)?;
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(3 + i))?;
                        args.push(read_register(frame, r)?.clone());
                    }
                    let result = math::call(&name, &args).map_err(math_to_vm_error)?;
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                Op::JsonCall => {
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let argc = match operands.get(2) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let name = lookup_string_constant(module, name_idx)?;
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(3 + i))?;
                        args.push(read_register(frame, r)?.clone());
                    }
                    let result =
                        json::call(&name, &args, &self.string_heap).map_err(json_to_vm_error)?;
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                // §22.1.1 / §22.1.2 String constructor + statics.
                // <https://tc39.es/ecma262/#sec-string-constructor>
                Op::StringCall => {
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let argc = match operands.get(2) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let name = lookup_string_constant(module, name_idx)?;
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(3 + i))?;
                        args.push(read_register(frame, r)?.clone());
                    }
                    let result = string_dispatch::call(&name, &args, &self.string_heap)?;
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                // §21.4 Date constructor + statics.
                // <https://tc39.es/ecma262/#sec-date-objects>
                Op::DateCall => {
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let argc = match operands.get(2) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let name = lookup_string_constant(module, name_idx)?;
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(3 + i))?;
                        args.push(read_register(frame, r)?.clone());
                    }
                    let result = date::dispatch::call(&name, &args)?;
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                // §21.2.1 / §21.2.2 BigInt static dispatch.
                // <https://tc39.es/ecma262/#sec-bigint-constructor>
                Op::BigIntCall => {
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let argc = match operands.get(2) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let name = lookup_string_constant(module, name_idx)?;
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(3 + i))?;
                        args.push(read_register(frame, r)?.clone());
                    }
                    let result = bigint::dispatch::call(&name, &args)?;
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                // §25.1.4 ArrayBuffer constructor + isView static.
                // <https://tc39.es/ecma262/#sec-arraybuffer-constructor>
                Op::ArrayBufferCall => {
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let argc = match operands.get(2) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let name = lookup_string_constant(module, name_idx)?;
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(3 + i))?;
                        args.push(read_register(frame, r)?.clone());
                    }
                    let result = binary::dispatch::array_buffer_call(&name, &args)?;
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                // §25.3 DataView constructor.
                // <https://tc39.es/ecma262/#sec-dataview-constructor>
                Op::DataViewCall => {
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let argc = match operands.get(2) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let name = lookup_string_constant(module, name_idx)?;
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(3 + i))?;
                        args.push(read_register(frame, r)?.clone());
                    }
                    let result = binary::dispatch::data_view_call(&name, &args)?;
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                // §23.2 TypedArray constructor + statics.
                // <https://tc39.es/ecma262/#sec-typedarray-constructors>
                Op::TypedArrayCall => {
                    let dst = register_operand(operands.first())?;
                    let kind_idx = const_operand(operands.get(1))?;
                    let name_idx = const_operand(operands.get(2))?;
                    let argc = match operands.get(3) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let kind_name = lookup_string_constant(module, kind_idx)?;
                    let kind = binary::TypedArrayKind::from_name(&kind_name)
                        .ok_or(VmError::TypeMismatch)?;
                    let name = lookup_string_constant(module, name_idx)?;
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(4 + i))?;
                        args.push(read_register(frame, r)?.clone());
                    }
                    let result = binary::dispatch::typed_array_call(kind, &name, &args)?;
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                // Iterator-helpers proposal — `Iterator.from(iter)`
                // and friends.
                // <https://tc39.es/proposal-iterator-helpers/>
                Op::IteratorCall => {
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let argc = match operands.get(2) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let name = lookup_string_constant(module, name_idx)?;
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(3 + i))?;
                        args.push(read_register(frame, r)?.clone());
                    }
                    let result = iterator_static_call(&name, &args)?;
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                // §25.2 SharedArrayBuffer constructor.
                // <https://tc39.es/ecma262/#sec-sharedarraybuffer-constructor>
                Op::SharedArrayBufferCall => {
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let argc = match operands.get(2) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let name = lookup_string_constant(module, name_idx)?;
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(3 + i))?;
                        args.push(read_register(frame, r)?.clone());
                    }
                    let result = binary::dispatch::shared_array_buffer_call(&name, &args)?;
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                // §25.4 Atomics namespace dispatcher.
                // <https://tc39.es/ecma262/#sec-atomics-object>
                Op::AtomicsCall => {
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let argc = match operands.get(2) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let name = lookup_string_constant(module, name_idx)?;
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(3 + i))?;
                        args.push(read_register(frame, r)?.clone());
                    }
                    let result = atomics::call(&name, &args, &self.string_heap)?;
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                // §28.2 Proxy constructor + statics — `new Proxy`
                // and `Proxy.revocable`.
                // <https://tc39.es/ecma262/#sec-proxy-constructor>
                Op::ProxyCall => {
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let argc = match operands.get(2) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let name = lookup_string_constant(module, name_idx)?;
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(3 + i))?;
                        args.push(read_register(frame, r)?.clone());
                    }
                    let result = proxy_static_call(&name, &args)?;
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                // §28.1 Reflect static surface — single dispatcher
                // covering every spec method.
                // <https://tc39.es/ecma262/#sec-reflect-object>
                Op::ReflectCall => {
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let argc = match operands.get(2) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let name = lookup_string_constant(module, name_idx)?;
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(3 + i))?;
                        args.push(read_register(&stack[top_idx], r)?.clone());
                    }
                    // Apply / construct need interp access; advance pc
                    // first so the sub-dispatch returns to the next op.
                    let pc = stack[top_idx].pc;
                    stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                    let heap = self.string_heap.clone();
                    let result = reflect::call(self, module, &name, &args, &heap)?;
                    write_register(&mut stack[top_idx], dst, result)?;
                    continue;
                }
                // §23.1.2 Array static dispatch. Routes
                // `Array.from` / `Array.of` through one entry point.
                // <https://tc39.es/ecma262/#sec-properties-of-the-array-constructor>
                Op::ArrayCall => {
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let argc = match operands.get(2) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let name = lookup_string_constant(module, name_idx)?;
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(3 + i))?;
                        args.push(read_register(frame, r)?.clone());
                    }
                    // Callback-driven `Array.from(iter, mapFn)` would
                    // need the interpreter; the foundation slice
                    // routes the no-callback shape through
                    // `array_statics::call`. With a callback we
                    // dispatch on the surrounding stack instead —
                    // filed as a follow-up.
                    let result = array_statics::call(&name, &args)?;
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                // §20.1.2 / §10.1.6 — Object static dispatch.
                // Routes through `object_statics::call` which honours
                // ECMA-262 ValidateAndApplyPropertyDescriptor and the
                // freeze/seal/preventExtensions integrity ladder.
                // <https://tc39.es/ecma262/#sec-properties-of-the-object-constructor>
                Op::ObjectCall => {
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let argc = match operands.get(2) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let name = lookup_string_constant(module, name_idx)?;
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(3 + i))?;
                        args.push(read_register(frame, r)?.clone());
                    }
                    let result = object_statics::call(&name, &args, &self.string_heap)?;
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                Op::QueueMicrotask => {
                    // Operands: callee, argc, args... — no dst.
                    let callee_reg = register_operand(operands.first())?;
                    let argc = match operands.get(1) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let callee = read_register(frame, callee_reg)?.clone();
                    if !is_callable(&callee) {
                        return Err(VmError::NotCallable);
                    }
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(2 + i))?;
                        args.push(read_register(frame, r)?.clone());
                    }
                    // Advance pc *before* mutating self.microtasks
                    // — the per-frame `frame: &mut Frame` borrow
                    // ends at the next statement, so the disjoint
                    // `&mut self.microtasks` borrow is legal.
                    frame.pc += 1;
                    self.microtasks.enqueue(Microtask {
                        callee,
                        this_value: Value::Undefined,
                        args,
                        result_capability: None,
                        kind: microtask::MicrotaskKind::Call,
                    });
                }
                Op::PromiseNew => {
                    // Operands: dst, executor_reg, scratch_dst.
                    let dst = register_operand(operands.first())?;
                    let executor_reg = register_operand(operands.get(1))?;
                    let scratch_dst = register_operand(operands.get(2))?;
                    let executor = read_register(frame, executor_reg)?.clone();
                    if !is_callable(&executor) {
                        return Err(VmError::NotCallable);
                    }
                    let (handle, resolve, reject) = promise_dispatch::construct();
                    let promise_value = Value::Promise(handle);
                    write_register(frame, dst, promise_value)?;
                    // Advance pc, then invoke executor with [resolve, reject].
                    frame.pc += 1;
                    let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                    args.push(resolve);
                    args.push(reject);
                    self.invoke(
                        stack,
                        module,
                        &executor,
                        Value::Undefined,
                        args,
                        scratch_dst,
                    )?;
                }
                Op::PromiseCall => {
                    // Operands: dst, name_const, argc, args...
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let argc = match operands.get(2) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let name = lookup_string_constant(module, name_idx)?;
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(3 + i))?;
                        args.push(read_register(frame, r)?.clone());
                    }
                    let argv: Vec<Value> = args.into_iter().collect();
                    frame.pc += 1;
                    let result = promise_dispatch::statics_call(self, &name, &argv)
                        .map_err(native_to_vm_error)?;
                    let top_idx = stack.len() - 1;
                    write_register(&mut stack[top_idx], dst, result)?;
                }
                Op::CollectRest => {
                    let dst = register_operand(operands.first())?;
                    // Drain rather than clone — the rest array is
                    // built once per call and CollectRest is the
                    // single consumer, so freeing the backing
                    // storage promptly keeps frame sizes small.
                    let elements: SmallVec<[Value; 4]> = std::mem::take(&mut frame.rest_args);
                    write_register(frame, dst, Value::Array(JsArray::from_elements(elements)))?;
                    frame.pc += 1;
                }
                Op::CollectArguments => {
                    // §10.4.4 Arguments Exotic Objects (foundation
                    // lowers the unmapped variant — strict mode is
                    // ADR-0001's default). Wrap the captured argv
                    // as a fresh JsArray. Drain so the frame's
                    // copy is released after the (typically
                    // single) materialisation.
                    let dst = register_operand(operands.first())?;
                    let elements: SmallVec<[Value; 4]> = std::mem::take(&mut frame.incoming_args);
                    write_register(frame, dst, Value::Array(JsArray::from_elements(elements)))?;
                    frame.pc += 1;
                }
                Op::ImportNamespace => {
                    let dst = register_operand(operands.first())?;
                    let spec_idx = const_operand(operands.get(1))?;
                    let specifier = lookup_string_constant(module, spec_idx)?;
                    let referrer = frame.module_url.clone();
                    let namespace = self
                        .resolve_module_namespace(module, referrer.as_ref(), &specifier)
                        .ok_or(VmError::UnknownIntrinsic {
                            name: format!("import \"{specifier}\""),
                        })?;
                    write_register(frame, dst, Value::Object(namespace))?;
                    frame.pc += 1;
                }
                Op::LoadGlobalThis => {
                    let dst = register_operand(operands.first())?;
                    let value = Value::Object(self.global_this.clone());
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
                }
                Op::LoadGlobalOrThrow => {
                    // §10.2.4.1 ResolveBinding + §10.2.4.5 GetValue:
                    // when the global env has no binding for `name`,
                    // throw a `ReferenceError`. Foundation lowers
                    // free-identifier reads to this opcode.
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let name = lookup_string_constant(module, name_idx)?;
                    if let Some(value) = self.global_this.get(&name) {
                        write_register(frame, dst, value)?;
                        frame.pc += 1;
                    } else {
                        // Throw a real `ReferenceError` instance so
                        // `e instanceof ReferenceError` checks
                        // observe the spec-correct shape.
                        return Err(VmError::UndefinedIdentifier { name });
                    }
                }
                Op::LoadGlobalOrUndefined => {
                    // §13.5.3 typeof — IsUnresolvableReference path:
                    // a free identifier with no global binding
                    // resolves to `undefined` rather than throwing.
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let name = lookup_string_constant(module, name_idx)?;
                    let value = self.global_this.get(&name).unwrap_or(Value::Undefined);
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
                }
                Op::GlobalCall => {
                    // §19.2 global functions — parseInt / parseFloat /
                    // isNaN / isFinite / encodeURI* / decodeURI*.
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let argc = match operands.get(2) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let name = lookup_string_constant(module, name_idx)?;
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(3 + i))?;
                        args.push(read_register(frame, r)?.clone());
                    }
                    let result = global_functions::call(&name, &args, &self.string_heap)?;
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                Op::ImportMetaResolve => {
                    // Resolve `specifier` against frame.module_url
                    // and write the resulting absolute URL string.
                    let dst = register_operand(operands.first())?;
                    let spec_reg = register_operand(operands.get(1))?;
                    let spec_value = read_register(frame, spec_reg)?.clone();
                    let specifier = match spec_value {
                        Value::String(s) => s.to_lossy_string(),
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let referrer_str: &str = &frame.module_url;
                    let resolved = resolve_relative_url(Some(referrer_str), &specifier);
                    let resolved_str = JsString::from_str(&resolved, &self.string_heap)
                        .map_err(|_| VmError::TypeMismatch)?;
                    write_register(frame, dst, Value::String(resolved_str))?;
                    frame.pc += 1;
                }
                Op::ImportNamespaceDynamic => {
                    // Runtime-resolved `import(spec)` per §16.2.1.7.
                    // The specifier is whatever string the user
                    // expression evaluated to. Fall back to a
                    // TypeError when the linker hasn't pre-resolved
                    // it — re-entrant compile is filed.
                    let dst = register_operand(operands.first())?;
                    let spec_reg = register_operand(operands.get(1))?;
                    let spec_value = read_register(frame, spec_reg)?.clone();
                    let specifier = match spec_value {
                        Value::String(s) => s.to_lossy_string(),
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let referrer = frame.module_url.clone();
                    let namespace = self
                        .resolve_module_namespace(module, referrer.as_ref(), &specifier)
                        .ok_or(VmError::UnknownIntrinsic {
                            name: format!("import \"{specifier}\""),
                        })?;
                    write_register(frame, dst, Value::Object(namespace))?;
                    frame.pc += 1;
                }
                Op::PromiseFulfilledOf => {
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let value = read_register(frame, src)?.clone();
                    let promise = JsPromiseHandle::fulfilled(value);
                    write_register(frame, dst, Value::Promise(promise))?;
                    frame.pc += 1;
                }
                Op::MakeClass => {
                    let dst = register_operand(operands.first())?;
                    let ctor_reg = register_operand(operands.get(1))?;
                    let proto_reg = register_operand(operands.get(2))?;
                    let statics_reg = register_operand(operands.get(3))?;
                    let ctor = read_register(frame, ctor_reg)?.clone();
                    if !is_callable(&ctor) {
                        return Err(VmError::NotCallable);
                    }
                    let prototype = match read_register(frame, proto_reg)? {
                        Value::Object(o) => o.clone(),
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let statics = match read_register(frame, statics_reg)? {
                        Value::Object(o) => o.clone(),
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let class = std::rc::Rc::new(ClassConstructor {
                        ctor,
                        prototype,
                        statics,
                    });
                    write_register(frame, dst, Value::ClassConstructor(class))?;
                    frame.pc += 1;
                }
                Op::EnterTry => {
                    let catch_off = imm32_operand(operands.first())?;
                    let finally_off = imm32_operand(operands.get(1))?;
                    let exc_register = register_operand(operands.get(2))?;
                    let next_pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)? as i64;
                    let resolve = |off: i32| -> Result<Option<u32>, VmError> {
                        if off == NO_HANDLER_OFFSET {
                            return Ok(None);
                        }
                        let target = next_pc + off as i64;
                        if target < 0 || target > u32::MAX as i64 {
                            return Err(VmError::InvalidOperand);
                        }
                        Ok(Some(target as u32))
                    };
                    let catch_pc = resolve(catch_off)?;
                    let finally_pc = resolve(finally_off)?;
                    if catch_pc.is_none() && finally_pc.is_none() {
                        return Err(VmError::InvalidOperand);
                    }
                    frame.handlers.push(TryHandler {
                        catch_pc,
                        finally_pc,
                        exc_register,
                    });
                    frame.pc += 1;
                }
                Op::LeaveTry => {
                    if frame.handlers.pop().is_none() {
                        return Err(VmError::InvalidOperand);
                    }
                    frame.pc += 1;
                }
                Op::BindFunction => {
                    let dst = register_operand(operands.first())?;
                    let callee_reg = register_operand(operands.get(1))?;
                    let this_reg = register_operand(operands.get(2))?;
                    let argc = match operands.get(3) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let target = read_register(frame, callee_reg)?.clone();
                    if !is_callable(&target) {
                        return Err(VmError::NotCallable);
                    }
                    let bound_this = read_register(frame, this_reg)?.clone();
                    let mut bound_args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(4 + i))?;
                        bound_args.push(read_register(frame, r)?.clone());
                    }
                    let bound = std::rc::Rc::new(BoundFunction {
                        target,
                        bound_this,
                        bound_args,
                    });
                    write_register(frame, dst, Value::BoundFunction(bound))?;
                    frame.pc += 1;
                }
                Op::GetIterator => {
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let value = read_register(frame, src)?.clone();
                    let state = match value {
                        Value::Array(array) => IteratorState::Array { array, index: 0 },
                        Value::String(string) => IteratorState::String { string, index: 0 },
                        // `for…of` over a `Map` yields `[key, value]`
                        // pairs (Spec §24.1.3.12 — `@@iterator` aliases
                        // `entries`); over a `Set` yields values
                        // (Spec §24.2.3.11). We snapshot at iteration
                        // start, building a synthetic backing array.
                        Value::Map(m) => {
                            let entries = m.entries();
                            let mut snap: SmallVec<[Value; 4]> =
                                SmallVec::with_capacity(entries.len());
                            for (k, v) in entries {
                                let mut pair: SmallVec<[Value; 4]> = SmallVec::new();
                                pair.push(k);
                                pair.push(v);
                                snap.push(Value::Array(JsArray::from_elements(pair)));
                            }
                            IteratorState::Array {
                                array: JsArray::from_elements(snap),
                                index: 0,
                            }
                        }
                        Value::Set(s) => {
                            let snap: SmallVec<[Value; 4]> = s.values().into_iter().collect();
                            IteratorState::Array {
                                array: JsArray::from_elements(snap),
                                index: 0,
                            }
                        }
                        // §27.5 — generator objects are iterable;
                        // `[@@iterator]()` returns the generator
                        // itself, and `next()` drives the suspended
                        // body.
                        Value::Generator(handle) => IteratorState::Generator { handle },
                        // Already-an-iterator (from
                        // `Iterator.from(...)` / a helper wrapper)
                        // should pass through unchanged.
                        Value::Iterator(rc) => {
                            write_register(frame, dst, Value::Iterator(rc))?;
                            frame.pc += 1;
                            continue;
                        }
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let iter = std::rc::Rc::new(std::cell::RefCell::new(state));
                    write_register(frame, dst, Value::Iterator(iter))?;
                    frame.pc += 1;
                }
                Op::IteratorNext => {
                    let value_dst = register_operand(operands.first())?;
                    let done_dst = register_operand(operands.get(1))?;
                    let iter_reg = register_operand(operands.get(2))?;
                    let iter = match read_register(frame, iter_reg)? {
                        Value::Iterator(rc) => rc.clone(),
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let (value, done) = step_iterator(&iter, &self.string_heap)?;
                    write_register(frame, value_dst, value)?;
                    write_register(frame, done_dst, Value::Boolean(done))?;
                    frame.pc += 1;
                }
                Op::ArrayPush => {
                    let arr_reg = register_operand(operands.first())?;
                    let value_reg = register_operand(operands.get(1))?;
                    let value = read_register(frame, value_reg)?.clone();
                    let array = match read_register(frame, arr_reg)? {
                        Value::Array(a) => a.clone(),
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let next_idx = array.len();
                    array.set(next_idx, value);
                    frame.pc += 1;
                }
                Op::SymbolLoad => {
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let name = lookup_string_constant(module, name_idx)?;
                    let value =
                        symbol_dispatch::load_static(self, &name).map_err(symbol_to_vm_error)?;
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
                }
                Op::SymbolCall => {
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let argc = match operands.get(2) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let name = lookup_string_constant(module, name_idx)?;
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(3 + i))?;
                        args.push(read_register(frame, r)?.clone());
                    }
                    let result =
                        symbol_dispatch::call(self, &name, &args).map_err(symbol_to_vm_error)?;
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                Op::TypeOf => {
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let tag = read_register(frame, src)?.typeof_string();
                    let s = JsString::from_str(tag, &self.string_heap)?;
                    write_register(frame, dst, Value::String(s))?;
                    frame.pc += 1;
                }
                Op::DeleteElement => {
                    let dst = register_operand(operands.first())?;
                    let obj_reg = register_operand(operands.get(1))?;
                    let idx_reg = register_operand(operands.get(2))?;
                    let obj = match read_register(frame, obj_reg)? {
                        Value::Object(o) => o.clone(),
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let removed = match read_register(frame, idx_reg)? {
                        Value::Symbol(sym) => obj.delete_symbol(sym),
                        Value::String(s) => obj.delete(&s.to_lossy_string()),
                        Value::Number(n) => match n.as_smi() {
                            Some(v) if v >= 0 => obj.delete(&v.to_string()),
                            _ => false,
                        },
                        _ => return Err(VmError::TypeMismatch),
                    };
                    write_register(frame, dst, Value::Boolean(removed))?;
                    frame.pc += 1;
                }
                Op::NewCollection => {
                    let dst = register_operand(operands.first())?;
                    let kind_idx = const_operand(operands.get(1))?;
                    let iter_reg = register_operand(operands.get(2))?;
                    let kind = lookup_string_constant(module, kind_idx)?;
                    let seed = read_register(frame, iter_reg)?.clone();
                    let value = build_collection(&kind, &seed)?;
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
                }
                Op::TemporalCall => {
                    let dst = register_operand(operands.first())?;
                    let class_idx = const_operand(operands.get(1))?;
                    let method_idx = const_operand(operands.get(2))?;
                    let argc = match operands.get(3) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let class = lookup_string_constant(module, class_idx)?;
                    let method = lookup_string_constant(module, method_idx)?;
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(4 + i))?;
                        args.push(read_register(frame, r)?.clone());
                    }
                    let result = temporal::call_static(&self.string_heap, &class, &method, &args)
                        .map_err(temporal_to_vm_error)?;
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                Op::TemporalLoad => {
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let name = lookup_string_constant(module, name_idx)?;
                    let value = temporal::load_static(&name).map_err(temporal_to_vm_error)?;
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
                }
                Op::NewIntl => {
                    let dst = register_operand(operands.first())?;
                    let class_idx = const_operand(operands.get(1))?;
                    let locale_reg = register_operand(operands.get(2))?;
                    let options_reg = register_operand(operands.get(3))?;
                    let class = lookup_string_constant(module, class_idx)?;
                    let locale = read_register(frame, locale_reg)?.clone();
                    let options = read_register(frame, options_reg)?.clone();
                    let value =
                        intl::construct(&class, &locale, &options).map_err(intl_to_vm_error)?;
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
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
            Some(target) => match value {
                Value::Object(_) | Value::Array(_) => value,
                _ => Value::Object(target),
            },
            None => value,
        };
        if let Some(state) = popped.async_state {
            let jobs = state.result_promise.fulfill(resolved);
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

    /// Handle `Op::Call`: push a new frame for the callee with
    /// arguments copied into the parameter slots and `this` bound
    /// to `Value::Undefined` (foundation strict default).
    fn do_call(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let argc = match operands.get(2) {
            Some(&Operand::ConstIndex(n)) => n,
            _ => return Err(VmError::InvalidOperand),
        };

        let top_idx = stack.len() - 1;
        let callee = read_register(&stack[top_idx], callee_reg)?.clone();
        let mut args: SmallVec<[Value; 8]> = SmallVec::with_capacity(argc as usize);
        for i in 0..argc as usize {
            let r = register_operand(operands.get(3 + i))?;
            args.push(read_register(&stack[top_idx], r)?.clone());
        }
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        self.invoke(stack, module, &callee, Value::Undefined, args, dst)
    }

    /// Invoke `callee` with the explicit receiver `this_value` and
    /// the given argument list. Centralizes the BoundFunction
    /// unwrapping, closure `bound_this` override, and frame push so
    /// every call opcode (`Op::Call`, `Op::CallWithThis`,
    /// `Op::CallMethodValue`) shares one path.
    ///
    /// `dst` is the **caller's** register that should receive the
    /// completion value when the callee returns. `caller_pc` must
    /// already be advanced before this call so the post-pop
    /// dispatch resumes after the originating instruction.
    fn invoke(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        callee: &Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        // Walk through any number of `bind` layers, accumulating
        // their bound arguments and overriding `this_value` with
        // the innermost `bound_this`. The loop bound matches the
        // JS-call stack-depth limit so a pathological self-bound
        // chain still surfaces as `StackOverflow` rather than
        // unbounded recursion.
        let mut current = callee.clone();
        let mut effective_this = this_value;
        let mut effective_args = args;
        let mut hops: u32 = 0;
        loop {
            if hops >= self.max_stack_depth {
                return Err(VmError::StackOverflow {
                    limit: self.max_stack_depth,
                });
            }
            match current {
                Value::BoundFunction(bound) => {
                    hops += 1;
                    let mut combined: SmallVec<[Value; 8]> =
                        SmallVec::with_capacity(bound.bound_args.len() + effective_args.len());
                    combined.extend(bound.bound_args.iter().cloned());
                    combined.extend(effective_args);
                    effective_this = bound.bound_this.clone();
                    effective_args = combined;
                    current = bound.target.clone();
                }
                Value::ClassConstructor(cc) => {
                    hops += 1;
                    current = cc.ctor.clone();
                }
                _ => break,
            }
        }
        // Native callables short-circuit the frame push: invoke
        // the closure inline, write the result into the caller's
        // dst, and advance pc on the caller frame. No stack frame
        // is created — the closure cannot itself push frames.
        if let Value::NativeFunction(native) = &current {
            let argv: Vec<Value> = effective_args.into_iter().collect();
            // Clone the Rc out of `current` so the native body can
            // freely take `&mut self` without holding a borrow on
            // `current`.
            let native = native.clone();
            let result = (native.call)(self, &argv).map_err(native_to_vm_error)?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(());
        }
        // §28.2.4.13 Proxy.[[Call]] — delegate to the `apply`
        // trap when present; otherwise call through to the
        // target as a function.
        if let Value::Proxy(p) = &current {
            let proxy = p.clone();
            let argv_array = JsArray::from_elements(effective_args.iter().cloned());
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                proxy.target(),
                effective_this.clone(),
                Value::Array(argv_array),
            ];
            let result = match self.invoke_proxy_trap(module, &proxy, "apply", trap_args)? {
                Some(v) => v,
                None => {
                    // Fall through to the target's [[Call]] —
                    // `proxy.target()` returns the original Value,
                    // which may be a callable directly.
                    let underlying = proxy.target();
                    self.run_callable_sync(module, &underlying, effective_this, effective_args)?
                }
            };
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(());
        }
        let (function_id, parent_upvalues, this_for_callee) = match current {
            Value::Function { function_id } => {
                (function_id, std::rc::Rc::from(Vec::new()), effective_this)
            }
            Value::Closure {
                function_id,
                upvalues,
                bound_this,
            } => {
                let this_value = match bound_this {
                    Some(t) => *t,
                    None => effective_this,
                };
                (function_id, upvalues, this_value)
            }
            _ => return Err(VmError::NotCallable),
        };

        if stack.len() as u32 >= self.max_stack_depth {
            return Err(VmError::StackOverflow {
                limit: self.max_stack_depth,
            });
        }
        let function = module
            .functions
            .get(function_id as usize)
            .ok_or(VmError::InvalidOperand)?;
        // Async-call entry path (spec §27.7.5.1): synthesise a
        // fresh pending result promise, write it into the caller's
        // `dst` register *now* so the call expression's value is
        // visible synchronously, and park the new frame with
        // `return_register = None` so its eventual completion
        // settles the promise instead of writing back.
        let (return_register, async_state) = if function.is_async {
            let result_promise = JsPromiseHandle::pending();
            let promise_value = Value::Promise(result_promise.clone());
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, promise_value)?;
            (None, Some(AsyncFrameState { result_promise }))
        } else {
            (Some(dst), None)
        };
        let upvalues = Frame::build_upvalues(&mut self.gc_heap, function, parent_upvalues)?;
        let mut new_frame = Frame::with_return_upvalues_and_this(
            function,
            return_register,
            upvalues,
            this_for_callee,
        );
        new_frame.async_state = async_state;
        // Bind parameters: extra args are dropped, missing args
        // stay `Value::Undefined` (matches JS semantics).
        let bind_count = (function.param_count as usize).min(effective_args.len());
        let total_args = effective_args.len();
        // Snapshot the full argv when the callee body references
        // `arguments`. Cloning is cheap because effective_args is a
        // SmallVec; the snapshot is consumed exactly once by
        // `Op::CollectArguments`.
        if function.needs_arguments {
            new_frame.incoming_args = effective_args.iter().cloned().collect();
        }
        let mut iter = effective_args.into_iter();
        for i in 0..bind_count {
            let value = iter.next().expect("bind_count <= len");
            let slot = new_frame
                .registers
                .get_mut(i)
                .ok_or(VmError::InvalidOperand)?;
            *slot = value;
        }
        // Stash the trailing args for `Op::CollectRest`. Only the
        // rest-aware callees pay the allocation; everyone else
        // leaves `rest_args` empty as initialised.
        if function.has_rest && total_args > function.param_count as usize {
            new_frame.rest_args = iter.collect();
        }
        // §27.5 Generator-call entry: instead of pushing the frame
        // onto the dispatch stack, hand the caller a paused
        // [`Value::Generator`] handle that owns the prepared frame.
        // The body only runs when `.next()` resumes it.
        if function.is_generator {
            new_frame.return_register = None;
            let async_gen = function.is_async_generator;
            let gen_handle = crate::generator::JsGenerator::new(new_frame);
            gen_handle.borrow_mut().is_async = async_gen;
            // Backlink the generator into the frame so `Op::Yield`
            // can find its owner once execution starts.
            {
                let mut body = gen_handle.borrow_mut();
                if let Some(frame) = body.frame.as_mut() {
                    frame.generator_owner = Some(gen_handle.clone());
                }
            }
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, Value::Generator(gen_handle))?;
            return Ok(());
        }
        stack.push(new_frame);
        Ok(())
    }

    /// Handle [`otter_bytecode::Op::Await`]: park the current
    /// async frame off the active stack and attach resume / reject
    /// reactions to the awaited promise.
    ///
    /// # Algorithm
    /// 1. Wrap a non-promise value with `Promise.resolve(v)` per
    ///    spec §27.7.5.3 step 1.b (an `Await` of a non-thenable
    ///    settles immediately on the next microtask tick).
    /// 2. Advance the parked frame's pc past the `Await`
    ///    instruction so resumption continues with the next op.
    /// 3. Pop the frame off the active stack and box it; share the
    ///    box between the resume / reject closures via an
    ///    `Rc<Cell<Option<_>>>` so whichever reaction fires first
    ///    consumes the parked frame and the other reaction falls
    ///    through as a no-op (matching spec idempotency for
    ///    `then`'s twin reactions).
    /// 4. Build native `resume_fulfill` / `resume_reject` closures
    ///    that enqueue a [`MicrotaskKind::AsyncResume`] microtask
    ///    when invoked. Attach them with `perform_then` so the
    ///    drain delivers the awaited value into the parked frame's
    ///    `dst` register on resume.
    ///
    /// # Invariants
    /// - The frame at the top of `stack` MUST be an async frame
    ///   (its `async_state.is_some()`); the compiler enforces
    ///   this. Violating it is a bytecode-malformation error and
    ///   surfaces as `VmError::InvalidOperand`.
    /// - On return, `stack` no longer contains the parked frame.
    ///   Callers that need to know whether the dispatch loop should
    ///   exit (because the parked frame was at the bottom) read
    ///   `stack.is_empty()` after this call.
    ///
    /// # Errors
    /// - [`VmError::InvalidOperand`] when called on a non-async
    ///   frame.
    fn do_await(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        dst: u16,
        awaited: Value,
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        // §27.6 Async-generator body — the running frame has no
        // `async_state` (it isn't a regular async-function frame),
        // but it carries a `generator_owner` whose body was flagged
        // async. Park the frame on a dedicated resume native that
        // re-enters the generator body and either settles the
        // outer `pending_request` from a subsequent `Op::Yield` /
        // completion, or chains another `Op::Await`.
        if stack[top_idx].async_state.is_none() {
            if let Some(owner) = stack[top_idx].generator_owner.clone() {
                if owner.borrow().is_async {
                    return self.do_await_async_gen(stack, dst, awaited, owner);
                }
            }
            return Err(VmError::InvalidOperand);
        }
        // Advance past the Await before parking so resumption
        // continues at the next instruction.
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        let parked = stack.pop().expect("top frame existed");
        let promise = match awaited {
            Value::Promise(p) => p,
            other => JsPromiseHandle::fulfilled(other),
        };

        // Share the parked frame between the two reaction
        // closures. Whichever reaction the runtime invokes first
        // takes the box; the other observes `None` and short-circuits.
        let parked_slot: std::rc::Rc<std::cell::Cell<Option<Box<Frame>>>> =
            std::rc::Rc::new(std::cell::Cell::new(Some(Box::new(parked))));

        let resume_native = make_async_resume_native(parked_slot.clone(), dst, true);
        let reject_native = make_async_resume_native(parked_slot, dst, false);
        let capability = promise_dispatch::make_capability();
        let outcome = promise.perform_then(Some(resume_native), Some(reject_native), capability);
        if let Some(job) = outcome.immediate_job {
            self.microtasks.enqueue(job);
        }
        Ok(())
    }

    /// §27.6.3 — `Op::Await` inside an async-generator body. Parks
    /// the running frame and attaches resume / reject reactions
    /// that re-enter the body when the awaited promise settles. On
    /// resume, the generator's `pending_request` is settled by a
    /// subsequent `Op::Yield`, completion, or further `Op::Await`.
    fn do_await_async_gen(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        dst: u16,
        awaited: Value,
        owner: crate::generator::JsGenerator,
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        let parked = stack.pop().expect("top frame existed");
        let promise = match awaited {
            Value::Promise(p) => p,
            other => JsPromiseHandle::fulfilled(other),
        };
        let parked_slot: std::rc::Rc<std::cell::Cell<Option<Box<Frame>>>> =
            std::rc::Rc::new(std::cell::Cell::new(Some(Box::new(parked))));
        let resume_native =
            make_async_gen_resume_native(parked_slot.clone(), dst, owner.clone(), true);
        let reject_native = make_async_gen_resume_native(parked_slot, dst, owner, false);
        let capability = promise_dispatch::make_capability();
        let outcome = promise.perform_then(Some(resume_native), Some(reject_native), capability);
        if let Some(job) = outcome.immediate_job {
            self.microtasks.enqueue(job);
        }
        Ok(())
    }

    /// Resume an async-generator body whose `Op::Await` parked
    /// `frame`. Mirrors [`Self::run_async_resume`] but settles the
    /// generator's `pending_request` on completion / unhandled
    /// throw rather than the frame's `async_state` promise.
    fn run_async_gen_resume(
        &mut self,
        module: &BytecodeModule,
        mut frame: Box<Frame>,
        await_dst: u16,
        fulfilled: bool,
        value: Value,
        owner: crate::generator::JsGenerator,
    ) -> Result<(), RunError> {
        if fulfilled {
            if let Some(slot) = frame.registers.get_mut(await_dst as usize) {
                *slot = value.clone();
            } else {
                return Err(RunError {
                    error: VmError::InvalidOperand,
                    frames: Vec::new(),
                });
            }
        }
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(*frame);
        if !fulfilled {
            if let Err(error) = self.unwind_throw(&mut stack, value.clone()) {
                let frames = snapshot_frames(module, &stack);
                return Err(RunError { error, frames });
            }
            if stack.is_empty() {
                // Throw drained out of the gen body; settle the
                // pending request as rejected.
                let req = owner.borrow_mut().pending_request.take();
                if let Some(req) = req {
                    if let Err(error) = self.run_callable_sync(
                        module,
                        &req.reject,
                        Value::Undefined,
                        smallvec::smallvec![value],
                    ) {
                        return Err(RunError {
                            error,
                            frames: Vec::new(),
                        });
                    }
                }
                owner.borrow_mut().done = true;
                return Ok(());
            }
        }
        match self.dispatch_loop(module, &mut stack) {
            Ok(value) => {
                let yielded_already = owner.borrow().yielded.is_some();
                if yielded_already {
                    // Op::Yield already settled the request and
                    // saved the frame back to the gen.
                    owner.borrow_mut().yielded.take();
                    return Ok(());
                }
                // Body completed: settle the pending request with
                // the final return value as `done: true`.
                let req = owner.borrow_mut().pending_request.take();
                if let Some(req) = req {
                    let record = make_iter_result(value, true);
                    if let Err(error) = self.run_callable_sync(
                        module,
                        &req.resolve,
                        Value::Undefined,
                        smallvec::smallvec![record],
                    ) {
                        return Err(RunError {
                            error,
                            frames: Vec::new(),
                        });
                    }
                }
                owner.borrow_mut().done = true;
                owner.borrow_mut().frame = None;
                Ok(())
            }
            Err(error) => {
                let frames = snapshot_frames(module, &stack);
                Err(RunError { error, frames })
            }
        }
    }

    /// Drive a [`MicrotaskKind::AsyncResume`] task: re-push the
    /// parked async frame onto a fresh stack and run
    /// [`Self::dispatch_loop`] until it settles.
    ///
    /// # Algorithm
    /// 1. On the fulfillment path, write the resolved value into
    ///    the await's destination register and run dispatch.
    /// 2. On the rejection path, push the frame, then enter
    ///    dispatch by injecting an immediate throw via
    ///    [`Self::unwind_throw`]. If unwind eats the throw via an
    ///    in-frame handler, dispatch continues normally; if no
    ///    handler exists, unwind settles the result promise as
    ///    rejected and the stack is empty so the loop never starts.
    ///
    /// # Errors
    /// - Propagates any `VmError` raised inside the resumed body.
    ///   Async frames absorb their own throws via `async_state`,
    ///   so the only errors that escape are runtime-level (OOM,
    ///   stack overflow, interrupt).
    fn run_async_resume(
        &mut self,
        module: &BytecodeModule,
        mut frame: Box<Frame>,
        await_dst: u16,
        fulfilled: bool,
        value: Value,
    ) -> Result<(), RunError> {
        if fulfilled {
            if let Some(slot) = frame.registers.get_mut(await_dst as usize) {
                *slot = value.clone();
            } else {
                return Err(RunError {
                    error: VmError::InvalidOperand,
                    frames: Vec::new(),
                });
            }
        }
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(*frame);
        if !fulfilled {
            // Inject the rejection as a throw so the parked frame
            // observes it through its `try`/`catch`/`finally`
            // structure exactly as a synchronous throw would.
            if let Err(error) = self.unwind_throw(&mut stack, value) {
                let frames = snapshot_frames(module, &stack);
                return Err(RunError { error, frames });
            }
            if stack.is_empty() {
                // The rejection drained through the async frame's
                // result promise — nothing left to dispatch.
                return Ok(());
            }
        }
        match self.dispatch_loop(module, &mut stack) {
            Ok(_) => Ok(()),
            Err(error) => {
                let frames = snapshot_frames(module, &stack);
                Err(RunError { error, frames })
            }
        }
    }

    /// Walk the live frame stack looking for a try-handler that
    /// can absorb an in-flight throw.
    ///
    /// # Algorithm
    /// 1. Inspect the top frame:
    ///    - **Catch handler hit** — write the thrown value into
    ///      the handler's `exc_register`, jump pc to the catch
    ///      entry, pop the handler, return `Ok(())` so dispatch
    ///      resumes in that frame.
    ///    - **Finally-only handler hit** — park the value on
    ///      `frame.pending_throw`, jump pc to the finally entry,
    ///      pop the handler, return `Ok(())`.
    ///      [`otter_bytecode::Op::EndFinally`] re-throws.
    ///    - **No handler in this frame** — if the frame is async
    ///      (`async_state.is_some()`), settle its result promise
    ///      as rejected, drain the resulting jobs into the
    ///      microtask queue, pop the frame, and stop unwinding.
    ///      The caller is in a different "logical thread" — its pc
    ///      was advanced past the call site at entry and the
    ///      result promise was already in its register.
    ///    - **Otherwise** — pop the frame and continue.
    ///
    /// # Errors
    /// - [`VmError::Uncaught`] when the frame stack empties without
    ///   a handler and no async-frame absorbed the throw.
    fn unwind_throw(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        value: Value,
    ) -> Result<(), VmError> {
        let display = render_thrown_value(&value);
        let payload = value;
        loop {
            let Some(frame) = stack.last_mut() else {
                return Err(VmError::Uncaught { value: display });
            };
            let Some(handler) = frame.handlers.pop() else {
                // No in-frame try-handler. Async frames absorb
                // their own unhandled throws into the result
                // promise as a rejection — synthesised in spec
                // §27.7.5.3 step 1.h.iii.
                if frame.async_state.is_some() {
                    let popped = stack.pop().expect("frame existed at last_mut");
                    let result_promise = popped
                        .async_state
                        .expect("async_state checked just above")
                        .result_promise;
                    let jobs = result_promise.reject(payload);
                    for j in jobs.jobs {
                        self.microtasks.enqueue(j);
                    }
                    return Ok(());
                }
                stack.pop();
                continue;
            };
            if let Some(catch_pc) = handler.catch_pc {
                frame.pc = catch_pc;
                let slot = frame
                    .registers
                    .get_mut(handler.exc_register as usize)
                    .ok_or(VmError::InvalidOperand)?;
                *slot = payload;
                return Ok(());
            }
            let finally_pc = handler.finally_pc.ok_or(VmError::InvalidOperand)?;
            frame.pc = finally_pc;
            frame.pending_throw = Some(payload);
            return Ok(());
        }
    }

    /// Handle `Op::New`: allocate a fresh receiver, set its
    /// `[[Prototype]]` to `callee.prototype` (when present), and
    /// invoke the callee with `this = receiver`. The caller's `dst`
    /// register receives either the constructor's returned object
    /// or the freshly allocated receiver — `pop_frame` performs
    /// that swap so the unwind path is uniform across call shapes.
    fn do_construct(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let argc = match operands.get(2) {
            Some(&Operand::ConstIndex(n)) => n,
            _ => return Err(VmError::InvalidOperand),
        };
        let top_idx = stack.len() - 1;
        let callee = read_register(&stack[top_idx], callee_reg)?.clone();
        if !is_callable(&callee) {
            return Err(VmError::NotCallable);
        }
        let mut args: SmallVec<[Value; 8]> = SmallVec::with_capacity(argc as usize);
        for i in 0..argc as usize {
            let r = register_operand(operands.get(3 + i))?;
            args.push(read_register(&stack[top_idx], r)?.clone());
        }
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        self.dispatch_construct(stack, module, callee, args, dst)
    }

    fn do_construct_spread(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let args_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let callee = read_register(&stack[top_idx], callee_reg)?.clone();
        if !is_callable(&callee) {
            return Err(VmError::NotCallable);
        }
        let args_value = read_register(&stack[top_idx], args_reg)?.clone();
        let arr = match args_value {
            Value::Array(a) => a,
            _ => return Err(VmError::TypeMismatch),
        };
        let args: SmallVec<[Value; 8]> = arr.borrow_body().iter().cloned().collect();
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        self.dispatch_construct(stack, module, callee, args, dst)
    }

    fn dispatch_construct(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        callee: Value,
        args: SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        // §28.2.4.14 Proxy.[[Construct]] — `new <proxy>(args)`
        // routes through the `construct` trap when present;
        // otherwise delegates to the target.
        if let Value::Proxy(p) = &callee {
            let proxy = p.clone();
            let argv_array = JsArray::from_elements(args.iter().cloned());
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                proxy.target(),
                Value::Array(argv_array),
                Value::Proxy(proxy.clone()),
            ];
            let result = match self.invoke_proxy_trap(module, &proxy, "construct", trap_args)? {
                Some(v) => v,
                None => {
                    // Fall through to constructing the underlying
                    // target.
                    let underlying = proxy.target();
                    let receiver = JsObject::new();
                    if let Some(proto) = construct_prototype(&underlying) {
                        receiver.set_prototype(Some(proto));
                    }
                    let result = self.run_callable_sync(
                        module,
                        &underlying,
                        Value::Object(receiver.clone()),
                        args,
                    )?;
                    match result {
                        Value::Object(_) => result,
                        _ => Value::Object(receiver),
                    }
                }
            };
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(());
        }
        // Allocate receiver and link its prototype before pushing
        // the new frame. The constructor might mutate the receiver
        // immediately, so the prototype link must already be in
        // place.
        let receiver = JsObject::new();
        if let Some(proto) = construct_prototype(&callee) {
            receiver.set_prototype(Some(proto));
        }
        let this_value = Value::Object(receiver.clone());
        self.invoke(stack, module, &callee, this_value, args, dst)?;
        // The pushed frame is now on top; mark it so `pop_frame`
        // can substitute the receiver for any non-object return.
        if let Some(top) = stack.last_mut() {
            top.construct_target = Some(receiver);
        }
        Ok(())
    }

    /// Handle `Op::CallSpread`: read the args array, fan it out
    /// into the standard call path. The receiver register holds
    /// the explicit `this` value (foundation lowers free spread
    /// calls with `this = undefined`).
    fn do_call_spread(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let this_reg = register_operand(operands.get(2))?;
        let args_reg = register_operand(operands.get(3))?;
        let top_idx = stack.len() - 1;
        let callee = read_register(&stack[top_idx], callee_reg)?.clone();
        let this_value = read_register(&stack[top_idx], this_reg)?.clone();
        let args_array = match read_register(&stack[top_idx], args_reg)? {
            Value::Array(a) => a.clone(),
            _ => return Err(VmError::TypeMismatch),
        };
        let args: SmallVec<[Value; 8]> = args_array.borrow_body().iter().cloned().collect();
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        self.invoke(stack, module, &callee, this_value, args, dst)
    }

    /// Handle `Op::CallWithThis`: same as `do_call` but the call
    /// site supplies an explicit `this` register. Used by
    /// `Function.prototype.call` lowering and the array-literal
    /// path of `Function.prototype.apply`.
    fn do_call_with_this(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let this_reg = register_operand(operands.get(2))?;
        let argc = match operands.get(3) {
            Some(&Operand::ConstIndex(n)) => n,
            _ => return Err(VmError::InvalidOperand),
        };
        let top_idx = stack.len() - 1;
        let callee = read_register(&stack[top_idx], callee_reg)?.clone();
        let this_value = read_register(&stack[top_idx], this_reg)?.clone();
        let mut args: SmallVec<[Value; 8]> = SmallVec::with_capacity(argc as usize);
        for i in 0..argc as usize {
            let r = register_operand(operands.get(4 + i))?;
            args.push(read_register(&stack[top_idx], r)?.clone());
        }
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        self.invoke(stack, module, &callee, this_value, args, dst)
    }

    /// Handle `Op::CallMethodValue`: the universal method-call op.
    /// Branches by receiver kind:
    /// - `String` / `Array` — synchronous intrinsic-table dispatch.
    ///   Result lands in the destination register without pushing
    ///   a frame.
    /// - `Object` — load the property; raise `NotCallable` if the
    ///   resolved value is not a function; otherwise call it with
    ///   `this = receiver`.
    /// - `Function` / `Closure` / `BoundFunction` — only the
    ///   `call`, `apply`, and `bind` shapes are recognised; anything
    ///   else surfaces as `UnknownIntrinsic`.
    fn do_call_method_value(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let recv_reg = register_operand(operands.get(1))?;
        let name_idx = const_operand(operands.get(2))?;
        let argc = match operands.get(3) {
            Some(&Operand::ConstIndex(n)) => n as usize,
            _ => return Err(VmError::InvalidOperand),
        };
        let name = match module.constants.get(name_idx as usize) {
            Some(Constant::String { utf16 }) => String::from_utf16_lossy(utf16),
            _ => return Err(VmError::InvalidOperand),
        };
        let top_idx = stack.len() - 1;
        let recv_value = read_register(&stack[top_idx], recv_reg)?.clone();
        let mut arg_values: SmallVec<[Value; 8]> = SmallVec::with_capacity(argc);
        for i in 0..argc {
            let r = register_operand(operands.get(4 + i))?;
            arg_values.push(read_register(&stack[top_idx], r)?.clone());
        }

        // Promise.prototype dispatches separately because it
        // needs `&mut self` to enqueue microtasks.
        if let Value::Promise(p) = &recv_value {
            let promise = p.clone();
            let argv: Vec<Value> = arg_values.iter().cloned().collect();
            let result = promise_dispatch::prototype_call(self, &promise, &name, &argv)
                .map_err(native_to_vm_error)?;
            let top_idx = stack.len() - 1;
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            return Ok(());
        }

        // `forEach` on a collection requires a callback dispatch
        // that pushes a frame; lives outside the static intrinsic
        // table so it can drive `self.invoke`.
        if name == "forEach" && matches!(&recv_value, Value::Map(_) | Value::Set(_)) {
            return self.do_collection_for_each(stack, module, &recv_value, &arg_values, dst);
        }

        // Iterator-helpers proposal — when receiver is an iterator
        // value, route through the dedicated dispatcher that builds
        // lazy wrappers / drains for terminals.
        // <https://tc39.es/proposal-iterator-helpers/>
        if let Value::Iterator(rc) = &recv_value {
            let iter_rc = rc.clone();
            if self.iterator_helper_dispatch(stack, module, &iter_rc, &name, &arg_values, dst)? {
                return Ok(());
            }
        }

        // §27.5.3 Generator.prototype methods — `.next` / `.return`
        // / `.throw`. The receiver carries the suspended frame; the
        // resume helper drives a sub-dispatch until the next Yield
        // or completion.
        // <https://tc39.es/ecma262/#sec-generator-objects>
        if let Value::Generator(g) = &recv_value {
            let kind = match name.as_str() {
                "next" => Some(GeneratorResumeKind::Next(
                    arg_values.first().cloned().unwrap_or(Value::Undefined),
                )),
                "return" => Some(GeneratorResumeKind::Return(
                    arg_values.first().cloned().unwrap_or(Value::Undefined),
                )),
                "throw" => Some(GeneratorResumeKind::Throw(
                    arg_values.first().cloned().unwrap_or(Value::Undefined),
                )),
                _ => None,
            };
            if let Some(kind) = kind {
                let g = g.clone();
                let is_async_gen = g.borrow().is_async;
                if is_async_gen {
                    // §27.6.3 — async-generator method calls always
                    // return a Promise. Allocate the outer
                    // capability up front and stash it on
                    // `pending_request` so `Op::Yield` /
                    // `resume_generator` / the await-resume native
                    // can settle it from inside the dispatch loop.
                    let cap = promise_dispatch::make_capability();
                    let promise = cap.promise.clone();
                    g.borrow_mut().pending_request = Some(cap.clone());
                    let outcome = self.resume_generator(module, &g, kind);
                    match outcome {
                        Ok(_) => {
                            // resume_generator drained the request
                            // — either by Op::Yield, by completion,
                            // or it left the request pending while
                            // an `Op::Await` parked the body. In
                            // any case, the outer promise is the
                            // user-visible handle.
                        }
                        Err(err) => {
                            if let Some(thrown) = self.pending_generator_throw.take() {
                                if let Some(req) = g.borrow_mut().pending_request.take() {
                                    self.run_callable_sync(
                                        module,
                                        &req.reject,
                                        Value::Undefined,
                                        smallvec::smallvec![thrown],
                                    )?;
                                }
                            } else {
                                g.borrow_mut().pending_request = None;
                                return Err(err);
                            }
                        }
                    }
                    let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                    write_register(frame, dst, promise)?;
                    frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                    return Ok(());
                }
                match self.resume_generator(module, &g, kind) {
                    Ok(result) => {
                        let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                        write_register(frame, dst, result)?;
                        frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                        return Ok(());
                    }
                    Err(err) => {
                        // If the generator body unwound an
                        // uncaught throw, re-raise the *original*
                        // value on the caller's frame stack so a
                        // surrounding `try { gen.throw(x) } catch`
                        // observes the right payload.
                        if let Some(thrown) = self.pending_generator_throw.take() {
                            self.unwind_throw(stack, thrown)?;
                            return Ok(());
                        }
                        return Err(err);
                    }
                }
            }
        }

        // §23.1.3 callback-driven Array.prototype methods. The
        // intrinsic table can't drive callbacks, so the foundation
        // dispatches them here via `run_callable_sync`. Each method
        // matches its ECMA-262 algorithm with sloppy edge handling
        // (sparse holes, throwing comparators, length mutation
        // mid-walk) deferred to follow-ups.
        if let Value::Array(arr) = &recv_value {
            if matches!(
                name.as_str(),
                "forEach"
                    | "map"
                    | "filter"
                    | "reduce"
                    | "reduceRight"
                    | "find"
                    | "findIndex"
                    | "every"
                    | "some"
                    | "flatMap"
                    | "sort"
            ) && self.array_callback_dispatch(stack, module, arr, &name, &arg_values, dst)?
            {
                return Ok(());
            }
        }
        // Primitive prototypes go through the intrinsic table —
        // synchronous, no frame push, advance pc and write directly.
        let intrinsic = match &recv_value {
            Value::String(_) => string_prototype::lookup(&name),
            Value::Array(_) => array_prototype::lookup(&name),
            Value::Number(_) => number::prototype_lookup(&name),
            Value::Boolean(_) => boolean_prototype::lookup(&name),
            Value::BigInt(_) => bigint::prototype::lookup(&name),
            Value::Date(_) => date::prototype::lookup(&name),
            Value::RegExp(_) => regexp_prototype::lookup(&name),
            Value::Symbol(_) => symbol_prototype::lookup(&name),
            Value::Map(_) => collections_prototype::lookup_map(&name),
            Value::Set(_) => collections_prototype::lookup_set(&name),
            Value::WeakMap(_) => collections_prototype::lookup_weak_map(&name),
            Value::WeakSet(_) => collections_prototype::lookup_weak_set(&name),
            Value::Temporal(_) => temporal::lookup_prototype(&recv_value, &name),
            Value::Intl(_) => intl::lookup_prototype(&recv_value, &name),
            Value::ArrayBuffer(_) => binary::array_buffer_prototype::lookup(&name),
            Value::DataView(_) => binary::data_view_prototype::lookup(&name),
            Value::TypedArray(_) => binary::typed_array_prototype::lookup(&name),
            _ => None,
        };
        if let Some(entry) = intrinsic {
            let small_args: SmallVec<[Value; 4]> = arg_values.iter().cloned().collect();
            let result = (entry.impl_fn)(&IntrinsicArgs {
                receiver: &recv_value,
                args: &small_args,
                string_heap: &self.string_heap,
            })
            .map_err(intrinsic_to_vm_error)?;
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            return Ok(());
        }

        // §20.1.3 Object.prototype methods that ordinary objects
        // inherit. Foundation has no installed Object.prototype yet,
        // so the runtime intercepts the canonical names directly when
        // the receiver is an ordinary `JsObject`. Once the prototype
        // tree is real (task 61 follow-up) these route through the
        // standard property lookup below.
        // <https://tc39.es/ecma262/#sec-properties-of-the-object-prototype-object>
        if let Value::Object(obj) = &recv_value {
            // Only intercept when the user hasn't overridden the
            // method via an own / inherited data property. This
            // keeps `Object.create({hasOwnProperty: () => 'shadow'})`
            // observable.
            if matches!(obj.lookup(&name), crate::object::PropertyLookup::Absent) {
                if let Some(result) =
                    object_prototype_intercept(obj, &name, &arg_values, &self.string_heap)?
                {
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, result)?;
                    frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                    return Ok(());
                }
            }
        }
        // Functions / closures inherit Object.prototype-style
        // methods. Foundation routes the call through the user-
        // properties bag attached to the compiled function.
        if let Value::Function { function_id } | Value::Closure { function_id, .. } = &recv_value
            && matches!(
                name.as_str(),
                "hasOwnProperty" | "propertyIsEnumerable" | "isPrototypeOf"
            )
        {
            let bag = self
                .function_user_props
                .entry(*function_id)
                .or_default()
                .clone();
            if let Some(result) =
                object_prototype_intercept(&bag, &name, &arg_values, &self.string_heap)?
            {
                let frame = &mut stack[top_idx];
                write_register(frame, dst, result)?;
                frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                return Ok(());
            }
        }

        // §20.2.3 Function.prototype canonical methods —
        // `call` / `apply` / `bind` / `toString`. They are
        // unconditionally available on any callable, even when the
        // receiver is a ClassConstructor whose statics object
        // hasn't installed them. The intercept runs before the
        // property-lookup so user-installed shadows take precedence
        // only when the receiver is a plain Object. Callable
        // receivers go straight here.
        // <https://tc39.es/ecma262/#sec-properties-of-the-function-prototype-object>
        if matches!(name.as_str(), "call" | "apply" | "bind" | "toString")
            && is_callable(&recv_value)
        {
            return self.dispatch_function_method(
                stack,
                module,
                &recv_value,
                &name,
                arg_values,
                dst,
            );
        }

        // Property-bearing receivers — load the property first.
        // For class constructors, `prototype` resolves to the
        // instance prototype object (mirroring `Op::LoadProperty`'s
        // class shape) and other names walk the static side. Only
        // when the property lookup hands back a callable do we
        // dispatch with `this = recv`; missing or non-callable
        // properties surface as `NotCallable` so callers see the
        // same error as `obj.notFn()`.
        let lookup_via_property = match &recv_value {
            Value::Object(obj) => Some(obj.get(&name).unwrap_or(Value::Undefined)),
            Value::ClassConstructor(c) => Some(if name == "prototype" {
                Value::Object(c.prototype.clone())
            } else {
                c.statics.get(&name).unwrap_or(Value::Undefined)
            }),
            // §10.1.8 OrdinaryGet on a callable receiver — user
            // properties (e.g. `assert.sameValue = function(){}`)
            // resolve via the function-properties side table; the
            // fallback to `Function.prototype.{call,apply,bind}`
            // happens below if we hand back `Undefined`.
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
                let fid = *function_id;
                Some(self.function_property_get(module, fid, &name)?)
            }
            _ => None,
        };
        if let Some(method) = lookup_via_property {
            if !is_callable(&method) {
                return Err(VmError::NotCallable);
            }
            stack[top_idx].pc = stack[top_idx]
                .pc
                .checked_add(1)
                .ok_or(VmError::InvalidOperand)?;
            return self.invoke(stack, module, &method, recv_value.clone(), arg_values, dst);
        }

        // `Function.prototype.{call, apply, bind, toString}` on a
        // callable receiver that doesn't expose the method as a
        // property — fallback path.
        if is_callable(&recv_value) {
            return self.dispatch_function_method(
                stack,
                module,
                &recv_value,
                &name,
                arg_values,
                dst,
            );
        }

        Err(VmError::UnknownIntrinsic { name })
    }

    /// Dispatch `call` / `apply` / `bind` on a callable receiver.
    /// Foundation handles only the literal-array shape of `apply`
    /// — non-array second arguments raise `TypeMismatch` so callers
    /// learn quickly that the foundation slice rejects dynamic
    /// argument arrays.
    /// Drive `Map.prototype.forEach` / `Set.prototype.forEach` —
    /// invoke the callback on each entry in insertion order.
    ///
    /// # Algorithm
    /// 1. Snapshot the entry list at call time (matches Spec
    ///    §24.1.3.5 / §24.2.3.6 — observable mutation during the
    ///    walk is captured by re-reading the live receiver, but the
    ///    snapshot still gates `index < snapshot.len()`).
    /// 2. For each entry, enqueue an inline call: every callback is
    ///    invoked synchronously through `self.invoke`. Because each
    ///    invoke pushes a frame and returns through the dispatch
    ///    loop, the foundation chains them by stashing the iteration
    ///    state in a tiny native closure that re-enters this helper.
    /// 3. Foundation simplification: rather than a re-entrant
    ///    chain, walk the snapshot here and synchronously invoke
    ///    each callback via a fresh dispatch_loop run on a new
    ///    stack. This matches the synchronous-callback model the
    ///    rest of the foundation already uses (see
    ///    [`Interpreter::run_callable_sync`]).
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-map.prototype.foreach>
    /// - <https://tc39.es/ecma262/#sec-set.prototype.foreach>
    fn do_collection_for_each(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        recv: &Value,
        args: &SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        let callee = match args.first() {
            Some(c) if is_callable(c) => c.clone(),
            _ => return Err(VmError::NotCallable),
        };
        let entries: Vec<(Value, Value)> = match recv {
            Value::Map(m) => m.entries(),
            Value::Set(s) => s.values().into_iter().map(|v| (v.clone(), v)).collect(),
            _ => return Err(VmError::TypeMismatch),
        };
        // Advance pc *before* invoking the callbacks so each
        // callback returns to the next instruction in the caller
        // frame.
        let top_idx = stack.len() - 1;
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        // Write `undefined` into the dst slot — `forEach` returns
        // `undefined` synchronously, even if the callback chain
        // produces values.
        write_register(&mut stack[top_idx], dst, Value::Undefined)?;
        let recv_for_callback = recv.clone();
        for (key, value) in entries {
            let mut cb_args: SmallVec<[Value; 8]> = SmallVec::new();
            cb_args.push(value);
            cb_args.push(key);
            cb_args.push(recv_for_callback.clone());
            self.run_callable_sync(module, &callee, Value::Undefined, cb_args)?;
        }
        Ok(())
    }

    /// Dispatch the §23.1.3 callback-driven Array prototype methods.
    /// Returns `Ok(true)` when the call was handled here (the
    /// dispatcher should fall through to the post-dispatch return),
    /// `Ok(false)` when the method is `sort` with no comparator
    /// (intrinsic-table path takes over).
    ///
    /// All callbacks run synchronously through
    /// [`Self::run_callable_sync`] — the foundation walks the array
    /// snapshot at call time, matching spec semantics for arrays
    /// whose length doesn't change mid-iteration.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-array.prototype.foreach>
    /// - <https://tc39.es/ecma262/#sec-array.prototype.map>
    /// - <https://tc39.es/ecma262/#sec-array.prototype.filter>
    /// - <https://tc39.es/ecma262/#sec-array.prototype.reduce>
    /// - <https://tc39.es/ecma262/#sec-array.prototype.find>
    /// - <https://tc39.es/ecma262/#sec-array.prototype.findindex>
    /// - <https://tc39.es/ecma262/#sec-array.prototype.every>
    /// - <https://tc39.es/ecma262/#sec-array.prototype.some>
    /// - <https://tc39.es/ecma262/#sec-array.prototype.flatmap>
    /// - <https://tc39.es/ecma262/#sec-array.prototype.sort>
    #[allow(clippy::too_many_arguments)]
    fn array_callback_dispatch(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        arr: &JsArray,
        name: &str,
        args: &SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<bool, VmError> {
        // `sort` without a comparator falls through to the intrinsic
        // table's lexicographic path. Comparator-driven sort is
        // handled here.
        if name == "sort" && matches!(args.first(), None | Some(Value::Undefined)) {
            return Ok(false);
        }

        let arr_value = Value::Array(arr.clone());
        // Snapshot the elements so callback-driven mutation of the
        // receiver does not corrupt iteration. Foundation matches
        // ECMA-262's "single-pass over indices 0..len" by capturing
        // length at entry; growing the array inside the callback
        // does not extend the walk (spec-compliant for `forEach` /
        // `map` / `filter`).
        let elements: Vec<Value> = arr.borrow_body().elements.iter().cloned().collect();
        let len = elements.len();

        let top_idx = stack.len() - 1;
        // Advance pc up front so each synchronous callback returns to
        // the next caller instruction.
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;

        let result = match name {
            "forEach" => {
                let callee = require_callable(args.first())?;
                for (i, value) in elements.into_iter().enumerate() {
                    let cb_args = build_array_cb_args(&value, i, &arr_value);
                    self.run_callable_sync(module, &callee, Value::Undefined, cb_args)?;
                }
                Value::Undefined
            }
            "map" => {
                let callee = require_callable(args.first())?;
                let mut out: Vec<Value> = Vec::with_capacity(len);
                for (i, value) in elements.into_iter().enumerate() {
                    let cb_args = build_array_cb_args(&value, i, &arr_value);
                    out.push(self.run_callable_sync(module, &callee, Value::Undefined, cb_args)?);
                }
                Value::Array(JsArray::from_elements(out))
            }
            "filter" => {
                let callee = require_callable(args.first())?;
                let mut out: Vec<Value> = Vec::new();
                for (i, value) in elements.into_iter().enumerate() {
                    let cb_args = build_array_cb_args(&value, i, &arr_value);
                    let kept =
                        self.run_callable_sync(module, &callee, Value::Undefined, cb_args)?;
                    if kept.to_boolean() {
                        out.push(arr.get(i));
                    }
                }
                Value::Array(JsArray::from_elements(out))
            }
            "reduce" | "reduceRight" => {
                let callee = require_callable(args.first())?;
                let has_init = args.len() >= 2;
                let initial = if has_init {
                    args[1].clone()
                } else {
                    Value::Undefined
                };
                let reverse = name == "reduceRight";
                if !has_init && elements.is_empty() {
                    return Err(VmError::TypeMismatch);
                }
                let mut acc = if has_init {
                    initial
                } else if reverse {
                    elements[len - 1].clone()
                } else {
                    elements[0].clone()
                };
                let start_idx = if has_init {
                    if reverse {
                        len.saturating_sub(1) as i64
                    } else {
                        0
                    }
                } else if reverse {
                    (len as i64) - 2
                } else {
                    1
                };
                let step: i64 = if reverse { -1 } else { 1 };
                let mut i = start_idx;
                while i >= 0 && (i as usize) < len {
                    let mut cb_args: SmallVec<[Value; 8]> = SmallVec::new();
                    cb_args.push(acc.clone());
                    cb_args.push(elements[i as usize].clone());
                    cb_args.push(Value::Number(NumberValue::from_i32(i as i32)));
                    cb_args.push(arr_value.clone());
                    acc = self.run_callable_sync(module, &callee, Value::Undefined, cb_args)?;
                    i += step;
                }
                acc
            }
            "find" => {
                let callee = require_callable(args.first())?;
                let mut found = Value::Undefined;
                for (i, value) in elements.into_iter().enumerate() {
                    let cb_args = build_array_cb_args(&value, i, &arr_value);
                    let hit = self.run_callable_sync(module, &callee, Value::Undefined, cb_args)?;
                    if hit.to_boolean() {
                        found = arr.get(i);
                        break;
                    }
                }
                found
            }
            "findIndex" => {
                let callee = require_callable(args.first())?;
                let mut idx: i32 = -1;
                for (i, value) in elements.into_iter().enumerate() {
                    let cb_args = build_array_cb_args(&value, i, &arr_value);
                    let hit = self.run_callable_sync(module, &callee, Value::Undefined, cb_args)?;
                    if hit.to_boolean() {
                        idx = i as i32;
                        break;
                    }
                }
                Value::Number(NumberValue::from_i32(idx))
            }
            "every" => {
                let callee = require_callable(args.first())?;
                let mut all = true;
                for (i, value) in elements.into_iter().enumerate() {
                    let cb_args = build_array_cb_args(&value, i, &arr_value);
                    let hit = self.run_callable_sync(module, &callee, Value::Undefined, cb_args)?;
                    if !hit.to_boolean() {
                        all = false;
                        break;
                    }
                }
                Value::Boolean(all)
            }
            "some" => {
                let callee = require_callable(args.first())?;
                let mut any = false;
                for (i, value) in elements.into_iter().enumerate() {
                    let cb_args = build_array_cb_args(&value, i, &arr_value);
                    let hit = self.run_callable_sync(module, &callee, Value::Undefined, cb_args)?;
                    if hit.to_boolean() {
                        any = true;
                        break;
                    }
                }
                Value::Boolean(any)
            }
            "flatMap" => {
                let callee = require_callable(args.first())?;
                let mut out: Vec<Value> = Vec::with_capacity(len);
                for (i, value) in elements.into_iter().enumerate() {
                    let cb_args = build_array_cb_args(&value, i, &arr_value);
                    let mapped =
                        self.run_callable_sync(module, &callee, Value::Undefined, cb_args)?;
                    match mapped {
                        Value::Array(inner) => {
                            for el in inner.borrow_body().iter() {
                                out.push(el.clone());
                            }
                        }
                        other => out.push(other),
                    }
                }
                Value::Array(JsArray::from_elements(out))
            }
            "sort" => {
                let callee = require_callable(args.first())?;
                // Snapshot, sort by user comparator, write back.
                let mut buffer: Vec<Value> = elements;
                // `sort_by` requires a closure that doesn't itself
                // mutate the engine — we can't recurse the
                // interpreter from inside `Ord::cmp`. Use a manual
                // insertion sort over the snapshot to stay
                // interpreter-friendly. O(n²) but matches the
                // foundation's "correctness over speed" stance.
                let n = buffer.len();
                for i in 1..n {
                    let mut j = i;
                    while j > 0 {
                        let mut cmp_args: SmallVec<[Value; 8]> = SmallVec::new();
                        cmp_args.push(buffer[j - 1].clone());
                        cmp_args.push(buffer[j].clone());
                        let outcome =
                            self.run_callable_sync(module, &callee, Value::Undefined, cmp_args)?;
                        let order = match outcome {
                            Value::Number(n) => n.as_f64(),
                            _ => 0.0,
                        };
                        if order > 0.0 {
                            buffer.swap(j - 1, j);
                            j -= 1;
                        } else {
                            break;
                        }
                    }
                }
                {
                    let mut body = arr.borrow_body_mut();
                    body.elements.clear();
                    for v in buffer {
                        body.elements.push(v);
                    }
                }
                arr_value.clone()
            }
            _ => return Ok(false),
        };

        let frame_top = stack.last_mut().ok_or(VmError::InvalidOperand)?;
        write_register(frame_top, dst, result)?;
        Ok(true)
    }

    /// Synchronously invoke `callee(args)` with the given `this` and
    /// return the completion value.
    ///
    /// # Algorithm
    /// 1. NativeFunction callees run inline — the foundation native
    ///    surface is `Fn`, so calling them here is just a function
    ///    pointer hop with `&mut self` access.
    /// 2. BoundFunction layers are unwrapped iteratively, prepending
    ///    bound args and replacing `this_value` with `bound_this`.
    /// 3. Bytecode / closure callees push a frame whose
    ///    `return_register` is `None`, which makes
    ///    [`Self::dispatch_loop`] return the completion value when
    ///    the frame pops.
    ///
    /// Used by collection `forEach` and other host-driven iteration
    /// helpers.
    pub fn run_callable_sync(
        &mut self,
        module: &BytecodeModule,
        callee: &Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        let mut current = callee.clone();
        let mut effective_this = this_value;
        let mut effective_args = args;
        let mut hops: u32 = 0;
        loop {
            if hops >= self.max_stack_depth {
                return Err(VmError::StackOverflow {
                    limit: self.max_stack_depth,
                });
            }
            match current {
                Value::BoundFunction(bound) => {
                    hops += 1;
                    let mut combined: SmallVec<[Value; 8]> =
                        SmallVec::with_capacity(bound.bound_args.len() + effective_args.len());
                    combined.extend(bound.bound_args.iter().cloned());
                    combined.extend(effective_args);
                    effective_this = bound.bound_this.clone();
                    effective_args = combined;
                    current = bound.target.clone();
                }
                Value::ClassConstructor(cc) => {
                    hops += 1;
                    current = cc.ctor.clone();
                }
                _ => break,
            }
        }
        if let Value::NativeFunction(native) = &current {
            let argv: Vec<Value> = effective_args.into_iter().collect();
            return (native.call)(self, &argv).map_err(native_to_vm_error);
        }
        let (function_id, parent_upvalues, this_for_callee) = match current {
            Value::Function { function_id } => {
                (function_id, std::rc::Rc::from(Vec::new()), effective_this)
            }
            Value::Closure {
                function_id,
                upvalues,
                bound_this,
            } => {
                let this_value = match bound_this {
                    Some(t) => *t,
                    None => effective_this,
                };
                (function_id, upvalues, this_value)
            }
            _ => return Err(VmError::NotCallable),
        };
        let function = module
            .functions
            .get(function_id as usize)
            .ok_or(VmError::InvalidOperand)?;
        let upvalues = Frame::build_upvalues(&mut self.gc_heap, function, parent_upvalues)?;
        let mut inner: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut new_frame =
            Frame::with_return_upvalues_and_this(function, None, upvalues, this_for_callee);
        let bind_count = (function.param_count as usize).min(effective_args.len());
        let total_args = effective_args.len();
        let mut arg_iter = effective_args.into_iter();
        for i in 0..bind_count {
            let v = arg_iter.next().expect("bind_count <= len");
            let slot = new_frame
                .registers
                .get_mut(i)
                .ok_or(VmError::InvalidOperand)?;
            *slot = v;
        }
        if function.has_rest && total_args > function.param_count as usize {
            new_frame.rest_args = arg_iter.collect();
        }
        inner.push(new_frame);
        self.dispatch_loop(module, &mut inner)
    }

    /// Synchronously advance an iterator one step, with full
    /// interpreter access so user-iterator `next()` calls and
    /// helper-wrapper callbacks can run inline. Mirrors the
    /// fast-path [`step_iterator`] helper but also handles the
    /// `User` / `Map` / `Filter` / `Take` / `Drop` / `FlatMap`
    /// variants by driving callbacks through
    /// [`Self::run_callable_sync`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-iteratornext>
    /// - <https://tc39.es/proposal-iterator-helpers/>
    fn iterator_next_full(
        &mut self,
        module: &BytecodeModule,
        iter: &std::rc::Rc<std::cell::RefCell<IteratorState>>,
    ) -> Result<(Value, bool), VmError> {
        // First try the fast path; falls through to the
        // interpreter-aware branch on `User` / wrapper variants.
        match step_iterator(iter, &self.string_heap) {
            Ok((value, done)) => Ok((value, done)),
            Err(_) => self.iterator_next_full_slow(module, iter),
        }
    }

    fn iterator_next_full_slow(
        &mut self,
        module: &BytecodeModule,
        iter: &std::rc::Rc<std::cell::RefCell<IteratorState>>,
    ) -> Result<(Value, bool), VmError> {
        // Snapshot the current state to avoid holding the borrow
        // across user-callback dispatch.
        let snapshot: IteratorStateSnapshot = {
            let state = iter.borrow();
            match &*state {
                IteratorState::User { iterator } => IteratorStateSnapshot::User(iterator.clone()),
                IteratorState::Generator { handle } => {
                    IteratorStateSnapshot::Generator(handle.clone())
                }
                IteratorState::Map { source, mapper } => IteratorStateSnapshot::Map {
                    source: source.clone(),
                    mapper: mapper.clone(),
                },
                IteratorState::Filter { source, predicate } => IteratorStateSnapshot::Filter {
                    source: source.clone(),
                    predicate: predicate.clone(),
                },
                IteratorState::Take { source, remaining } => IteratorStateSnapshot::Take {
                    source: source.clone(),
                    remaining: *remaining,
                },
                IteratorState::Drop { source, to_drop } => IteratorStateSnapshot::Drop {
                    source: source.clone(),
                    to_drop: *to_drop,
                },
                IteratorState::FlatMap {
                    source,
                    mapper,
                    inner,
                } => IteratorStateSnapshot::FlatMap {
                    source: source.clone(),
                    mapper: mapper.clone(),
                    inner: inner.clone(),
                },
                _ => return Err(VmError::TypeMismatch),
            }
        };
        match snapshot {
            IteratorStateSnapshot::Generator(handle) => {
                let result = self.resume_generator(
                    module,
                    &handle,
                    GeneratorResumeKind::Next(Value::Undefined),
                )?;
                let Value::Object(record) = &result else {
                    return Err(VmError::TypeMismatch);
                };
                let value = record.get("value").unwrap_or(Value::Undefined);
                let done = record.get("done").unwrap_or(Value::Undefined).to_boolean();
                if done {
                    *iter.borrow_mut() = IteratorState::Exhausted;
                }
                Ok((value, done))
            }
            IteratorStateSnapshot::User(iter_value) => {
                let Value::Object(iter_obj) = &iter_value else {
                    return Err(VmError::TypeMismatch);
                };
                let next_fn = iter_obj.get("next").ok_or(VmError::TypeMismatch)?;
                if !is_callable(&next_fn) {
                    return Err(VmError::TypeMismatch);
                }
                let result =
                    self.run_callable_sync(module, &next_fn, iter_value.clone(), SmallVec::new())?;
                let Value::Object(record) = &result else {
                    return Err(VmError::TypeMismatch);
                };
                let value = record.get("value").unwrap_or(Value::Undefined);
                let done = record.get("done").unwrap_or(Value::Undefined).to_boolean();
                if done {
                    *iter.borrow_mut() = IteratorState::Exhausted;
                }
                Ok((value, done))
            }
            IteratorStateSnapshot::Map { source, mapper } => {
                let (v, done) = self.iterator_next_full(module, &source)?;
                if done {
                    *iter.borrow_mut() = IteratorState::Exhausted;
                    return Ok((Value::Undefined, true));
                }
                let mapped = self.run_callable_sync(
                    module,
                    &mapper,
                    Value::Undefined,
                    smallvec::smallvec![v],
                )?;
                Ok((mapped, false))
            }
            IteratorStateSnapshot::Filter { source, predicate } => loop {
                let (v, done) = self.iterator_next_full(module, &source)?;
                if done {
                    *iter.borrow_mut() = IteratorState::Exhausted;
                    return Ok((Value::Undefined, true));
                }
                let kept = self.run_callable_sync(
                    module,
                    &predicate,
                    Value::Undefined,
                    smallvec::smallvec![v.clone()],
                )?;
                if kept.to_boolean() {
                    return Ok((v, false));
                }
            },
            IteratorStateSnapshot::Take { source, remaining } => {
                if remaining == 0 {
                    *iter.borrow_mut() = IteratorState::Exhausted;
                    return Ok((Value::Undefined, true));
                }
                let (v, done) = self.iterator_next_full(module, &source)?;
                if done {
                    *iter.borrow_mut() = IteratorState::Exhausted;
                    return Ok((Value::Undefined, true));
                }
                if let IteratorState::Take { remaining, .. } = &mut *iter.borrow_mut() {
                    *remaining = remaining.saturating_sub(1);
                }
                Ok((v, false))
            }
            IteratorStateSnapshot::Drop { source, to_drop } => {
                for _ in 0..to_drop {
                    let (_, done) = self.iterator_next_full(module, &source)?;
                    if done {
                        *iter.borrow_mut() = IteratorState::Exhausted;
                        return Ok((Value::Undefined, true));
                    }
                }
                if let IteratorState::Drop { to_drop, .. } = &mut *iter.borrow_mut() {
                    *to_drop = 0;
                }
                let (v, done) = self.iterator_next_full(module, &source)?;
                if done {
                    *iter.borrow_mut() = IteratorState::Exhausted;
                    return Ok((Value::Undefined, true));
                }
                Ok((v, false))
            }
            IteratorStateSnapshot::FlatMap {
                source,
                mapper,
                mut inner,
            } => {
                loop {
                    if let Some(inner_iter) = inner.take() {
                        let (v, done) = self.iterator_next_full(module, &inner_iter)?;
                        if !done {
                            // `inner_iter` remains the active inner
                            // iterator for the next call; the FlatMap
                            // slot still holds it.
                            return Ok((v, false));
                        }
                        if let IteratorState::FlatMap { inner: slot, .. } = &mut *iter.borrow_mut()
                        {
                            *slot = None;
                        }
                    }
                    let (v, done) = self.iterator_next_full(module, &source)?;
                    if done {
                        *iter.borrow_mut() = IteratorState::Exhausted;
                        return Ok((Value::Undefined, true));
                    }
                    let mapped = self.run_callable_sync(
                        module,
                        &mapper,
                        Value::Undefined,
                        smallvec::smallvec![v],
                    )?;
                    let inner_state = match mapped {
                        Value::Array(arr) => IteratorState::Array {
                            array: arr,
                            index: 0,
                        },
                        Value::Iterator(rc) => {
                            let new_inner = rc.clone();
                            if let IteratorState::FlatMap { inner: slot, .. } =
                                &mut *iter.borrow_mut()
                            {
                                *slot = Some(new_inner.clone());
                            }
                            inner = Some(new_inner);
                            continue;
                        }
                        other => return Ok((other, false)),
                    };
                    let new_inner = std::rc::Rc::new(std::cell::RefCell::new(inner_state));
                    if let IteratorState::FlatMap { inner: slot, .. } = &mut *iter.borrow_mut() {
                        *slot = Some(new_inner.clone());
                    }
                    inner = Some(new_inner);
                }
            }
        }
    }

    /// Dispatch one of the §27.5 / iterator-helper-proposal methods
    /// against a [`Value::Iterator`] receiver. Returns `Ok(true)`
    /// when the call was handled (`dst` written, pc advanced) and
    /// `Ok(false)` when the receiver does not expose `name`.
    ///
    /// # See also
    /// - <https://tc39.es/proposal-iterator-helpers/>
    fn iterator_helper_dispatch(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        iter_rc: &std::rc::Rc<std::cell::RefCell<IteratorState>>,
        name: &str,
        args: &SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<bool, VmError> {
        // Lazy helpers wrap the source in a new IteratorState; the
        // eager terminals drain via `iterator_next_full`.
        let result = match name {
            "map" => {
                let mapper = require_callable(args.first())?;
                Value::Iterator(std::rc::Rc::new(std::cell::RefCell::new(
                    IteratorState::Map {
                        source: iter_rc.clone(),
                        mapper,
                    },
                )))
            }
            "filter" => {
                let predicate = require_callable(args.first())?;
                Value::Iterator(std::rc::Rc::new(std::cell::RefCell::new(
                    IteratorState::Filter {
                        source: iter_rc.clone(),
                        predicate,
                    },
                )))
            }
            "take" => {
                let n = take_drop_count(args.first())?;
                Value::Iterator(std::rc::Rc::new(std::cell::RefCell::new(
                    IteratorState::Take {
                        source: iter_rc.clone(),
                        remaining: n,
                    },
                )))
            }
            "drop" => {
                let n = take_drop_count(args.first())?;
                Value::Iterator(std::rc::Rc::new(std::cell::RefCell::new(
                    IteratorState::Drop {
                        source: iter_rc.clone(),
                        to_drop: n,
                    },
                )))
            }
            "flatMap" => {
                let mapper = require_callable(args.first())?;
                Value::Iterator(std::rc::Rc::new(std::cell::RefCell::new(
                    IteratorState::FlatMap {
                        source: iter_rc.clone(),
                        mapper,
                        inner: None,
                    },
                )))
            }
            "toArray" => {
                let collected = self.drain_iterator(module, iter_rc)?;
                Value::Array(JsArray::from_elements(collected))
            }
            "forEach" => {
                let callback = require_callable(args.first())?;
                let collected = self.drain_iterator(module, iter_rc)?;
                for v in collected {
                    self.run_callable_sync(
                        module,
                        &callback,
                        Value::Undefined,
                        smallvec::smallvec![v],
                    )?;
                }
                Value::Undefined
            }
            "reduce" => {
                let reducer = require_callable(args.first())?;
                let has_initial = args.len() >= 2;
                let mut acc = if has_initial {
                    args[1].clone()
                } else {
                    Value::Undefined
                };
                let collected = self.drain_iterator(module, iter_rc)?;
                let mut iter = collected.into_iter();
                if !has_initial {
                    acc = match iter.next() {
                        Some(v) => v,
                        None => {
                            // Spec §27.5.x — empty + no initial → TypeError.
                            return Err(VmError::TypeMismatch);
                        }
                    };
                }
                for v in iter {
                    acc = self.run_callable_sync(
                        module,
                        &reducer,
                        Value::Undefined,
                        smallvec::smallvec![acc, v],
                    )?;
                }
                acc
            }
            _ => return Ok(false),
        };
        let top_idx = stack.len() - 1;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, result)?;
        frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
        Ok(true)
    }

    fn drain_iterator(
        &mut self,
        module: &BytecodeModule,
        iter_rc: &std::rc::Rc<std::cell::RefCell<IteratorState>>,
    ) -> Result<Vec<Value>, VmError> {
        let mut out = Vec::new();
        loop {
            let (v, done) = self.iterator_next_full(module, iter_rc)?;
            if done {
                return Ok(out);
            }
            out.push(v);
        }
    }

    /// Resume a generator object — drives the saved frame on a
    /// fresh sub-stack until either an [`otter_bytecode::Op::Yield`]
    /// pauses it (returning `{value, done: false}`) or the body
    /// runs to completion (returning `{value: returnValue,
    /// done: true}`).
    ///
    /// `kind` selects the entry behaviour per §27.5.3:
    /// - `Next(arg)`: write `arg` into the previous yield's dst
    ///   and continue.
    /// - `Return(arg)`: act as if the body executed `return arg;`
    ///   from the current pc — foundation simplification: mark the
    ///   generator done and surface `{value: arg, done: true}`
    ///   without running additional finally blocks.
    /// - `Throw(reason)`: re-enter the body and immediately throw
    ///   `reason` from the current pc; finally / catch handlers
    ///   take over per the unwind machinery.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-generator.prototype.next>
    /// - <https://tc39.es/ecma262/#sec-generator.prototype.return>
    /// - <https://tc39.es/ecma262/#sec-generator.prototype.throw>
    pub fn resume_generator(
        &mut self,
        module: &BytecodeModule,
        handle: &crate::generator::JsGenerator,
        kind: GeneratorResumeKind,
    ) -> Result<Value, VmError> {
        // Already-done generators short-circuit per §27.5.1.2.
        let (frame_opt, resume_dst) = {
            let body = handle.borrow();
            (body.frame.is_some(), body.resume_dst)
        };
        if !frame_opt {
            return Ok(make_iter_result(Value::Undefined, true));
        }
        // Pull the frame out of the gen body so we can mutate it.
        let mut frame = match handle.borrow_mut().frame.take() {
            Some(f) => f,
            None => return Ok(make_iter_result(Value::Undefined, true)),
        };
        // Apply the resume operation to the frame before re-entering
        // dispatch.
        let mut throw_value: Option<Value> = None;
        match &kind {
            GeneratorResumeKind::Next(arg) => {
                if frame.pc != 0 {
                    if let Some(slot) = frame.registers.get_mut(resume_dst as usize) {
                        *slot = arg.clone();
                    }
                }
            }
            GeneratorResumeKind::Return(arg) => {
                // Foundation: mark done and surface arg without
                // running the body further.
                handle.borrow_mut().done = true;
                handle.borrow_mut().frame = None;
                return Ok(make_iter_result(arg.clone(), true));
            }
            GeneratorResumeKind::Throw(reason) => {
                throw_value = Some(reason.clone());
            }
        }
        let mut sub_stack: SmallVec<[Frame; 8]> = SmallVec::new();
        sub_stack.push(frame);
        if let Some(reason) = throw_value {
            // Preserve the original throw value so the caller can
            // re-raise it on the outer stack when the gen body
            // does not catch it (the unwind_throw machinery
            // converts the value to a string when it surfaces as
            // VmError::Uncaught, losing the payload).
            self.pending_generator_throw = Some(reason.clone());
            match self.unwind_throw(&mut sub_stack, reason) {
                Ok(_) => {}
                Err(err) => {
                    handle.borrow_mut().done = true;
                    handle.borrow_mut().frame = None;
                    return Err(err);
                }
            }
            if sub_stack.is_empty() {
                handle.borrow_mut().done = true;
                handle.borrow_mut().frame = None;
                return Err(VmError::Uncaught {
                    value: "generator-throw".to_string(),
                });
            }
            // A handler caught the throw — clear the side channel.
            self.pending_generator_throw = None;
        }
        let is_async = handle.borrow().is_async;
        let outcome = self.dispatch_loop(module, &mut sub_stack);
        match outcome {
            Ok(value) => {
                // If a Yield fired, the gen body has the paused
                // frame back; surface yielded_value as the result.
                let yielded = handle.borrow_mut().yielded.take();
                if let Some(v) = yielded {
                    // Sync generators surface the iter result
                    // through the return value; async generators
                    // already settled `pending_request` from inside
                    // `Op::Yield`.
                    if is_async {
                        return Ok(Value::Undefined);
                    }
                    return Ok(make_iter_result(v, false));
                }
                // Body ran to completion or `Op::Await` parked the
                // frame. Distinguish by whether the gen still owns
                // the frame: a parked await leaves the slot empty
                // (the await microtask owns it) AND `sub_stack` is
                // empty.
                let frame_taken_by_await =
                    handle.borrow().frame.is_some() || sub_stack.is_empty() && is_async;
                let parked = is_async && handle.borrow().frame.is_none() && {
                    // The await machinery stored the parked frame
                    // in its closure, not on the gen handle. Detect
                    // that case by checking if pending_request is
                    // still set — if so, it's awaiting.
                    handle.borrow().pending_request.is_some()
                };
                let _ = frame_taken_by_await;
                if parked {
                    // Body suspended on `Op::Await`; the resume
                    // microtask will eventually settle
                    // `pending_request`.
                    return Ok(Value::Undefined);
                }
                // Body completed.
                handle.borrow_mut().done = true;
                handle.borrow_mut().frame = None;
                if is_async {
                    if let Some(req) = handle.borrow_mut().pending_request.take() {
                        let record = make_iter_result(value, true);
                        self.run_callable_sync(
                            module,
                            &req.resolve,
                            Value::Undefined,
                            smallvec::smallvec![record],
                        )?;
                    }
                    return Ok(Value::Undefined);
                }
                Ok(make_iter_result(value, true))
            }
            Err(err) => {
                handle.borrow_mut().done = true;
                handle.borrow_mut().frame = None;
                if is_async {
                    // Pending request stays alive — the caller
                    // (do_call_method_value) settles it on the
                    // pending_generator_throw side-channel.
                }
                Err(err)
            }
        }
    }

    /// §28.2 — call a Proxy handler trap. When the trap is missing,
    /// returns `Ok(None)` so the caller can fall through to the
    /// target's behaviour. When the trap exists, invokes it with
    /// `(target, ...trap_args)` (per spec each trap takes the
    /// target as its first explicit argument; subsequent ones come
    /// from `args`) and returns the result.
    pub fn invoke_proxy_trap(
        &mut self,
        module: &BytecodeModule,
        proxy: &crate::proxy::JsProxy,
        trap: &str,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Option<Value>, VmError> {
        if proxy.is_revoked() {
            return Err(VmError::TypeMismatch);
        }
        let handler = proxy.handler();
        let trap_fn = match handler.get(trap) {
            Some(v) if abstract_ops::is_callable(&v) => v,
            Some(Value::Undefined) | Some(Value::Null) | None => return Ok(None),
            _ => return Err(VmError::TypeMismatch),
        };
        let result = self.run_callable_sync(module, &trap_fn, Value::Object(handler), args)?;
        Ok(Some(result))
    }

    fn dispatch_function_method(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        callee: &Value,
        name: &str,
        args: SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        match name {
            "call" => {
                let mut iter = args.into_iter();
                let this_value = iter.next().unwrap_or(Value::Undefined);
                let forwarded: SmallVec<[Value; 8]> = iter.collect();
                stack[top_idx].pc = stack[top_idx]
                    .pc
                    .checked_add(1)
                    .ok_or(VmError::InvalidOperand)?;
                self.invoke(stack, module, callee, this_value, forwarded, dst)
            }
            "apply" => {
                let mut iter = args.into_iter();
                let this_value = iter.next().unwrap_or(Value::Undefined);
                let forwarded: SmallVec<[Value; 8]> = match iter.next() {
                    None | Some(Value::Undefined) | Some(Value::Null) => SmallVec::new(),
                    Some(Value::Array(arr)) => arr.borrow_body().iter().cloned().collect(),
                    _ => return Err(VmError::TypeMismatch),
                };
                stack[top_idx].pc = stack[top_idx]
                    .pc
                    .checked_add(1)
                    .ok_or(VmError::InvalidOperand)?;
                self.invoke(stack, module, callee, this_value, forwarded, dst)
            }
            "bind" => {
                let mut iter = args.into_iter();
                let this_value = iter.next().unwrap_or(Value::Undefined);
                let bound_args: SmallVec<[Value; 4]> = iter.collect();
                let bound = std::rc::Rc::new(BoundFunction {
                    target: callee.clone(),
                    bound_this: this_value,
                    bound_args,
                });
                let frame = &mut stack[top_idx];
                write_register(frame, dst, Value::BoundFunction(bound))?;
                frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                Ok(())
            }
            // §20.2.3.5 Function.prototype.toString — foundation
            // returns the canonical `function <name>() { [native
            // code] }` placeholder. Spec mandates a source-faithful
            // representation when source is available; the
            // foundation defers source preservation to a follow-up.
            // <https://tc39.es/ecma262/#sec-function.prototype.tostring>
            "toString" => {
                let display = function_to_string(module, callee);
                let s = JsString::from_str(&display, &self.string_heap)
                    .map_err(|_| VmError::TypeMismatch)?;
                let frame = &mut stack[top_idx];
                write_register(frame, dst, Value::String(s))?;
                frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                Ok(())
            }
            _ => Err(VmError::UnknownIntrinsic {
                name: name.to_string(),
            }),
        }
    }

    /// Pre-dispatch hook for [`Op::ToNumber`] that consults
    /// `[Symbol.toPrimitive]` on object operands.
    ///
    /// # Algorithm
    /// 1. If the source register holds a [`Value::Object`] whose
    ///    `[Symbol.toPrimitive]` symbol-keyed property is callable,
    ///    advance pc past the `ToNumber` instruction and invoke
    ///    the hook with `this = obj` and `args = ["number"]`.
    /// 2. The hook's return value lands in the `ToNumber`'s
    ///    destination register on frame pop. The foundation does
    ///    not re-coerce; tests targeting this slice return a
    ///    Number directly.
    /// 3. Return `Ok(Some(()))` when the hook fired (caller
    ///    `continue`s the dispatch loop), `Ok(None)` otherwise so
    ///    the in-frame fast path runs.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-toprimitive>
    /// - <https://tc39.es/ecma262/#sec-ordinarytoprimitive>
    fn try_to_primitive_dispatch(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        operands: &[Operand],
    ) -> Result<Option<()>, VmError> {
        let dst = register_operand(operands.first())?;
        let src = register_operand(operands.get(1))?;
        let top_idx = stack.len() - 1;
        let recv = read_register(&stack[top_idx], src)?.clone();
        let Value::Object(obj) = &recv else {
            return Ok(None);
        };
        let to_primitive_sym = self.well_known_symbols.get(symbol::WellKnown::ToPrimitive);
        let Some(callee) = obj.get_symbol(&to_primitive_sym) else {
            return Ok(None);
        };
        if !is_callable(&callee) {
            return Ok(None);
        }
        let hint = JsString::from_str("number", &self.string_heap)?;
        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
        args.push(Value::String(hint));
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        self.invoke(stack, module, &callee, recv.clone(), args, dst)?;
        Ok(Some(()))
    }

    /// Drive one tick of the [`Op::ToPrimitive`] ladder.
    ///
    /// # Algorithm
    /// Implements ECMA-262 §7.1.1 `ToPrimitive` plus §7.1.1.1
    /// `OrdinaryToPrimitive`:
    ///
    /// 1. **Already primitive** — write `src` to `dst`, advance pc.
    /// 2. **Resume from prior stage** — read the result the called
    ///    function wrote into `dst`. If primitive, advance pc and
    ///    clear the parked state. Otherwise advance the stage.
    /// 3. **`SymbolToPrim`** — look up `[Symbol.toPrimitive]`. If
    ///    callable, push a frame with `[hint]` and `this = obj`,
    ///    park state with `stage = OrdinaryFirst` (set so a
    ///    non-primitive result falls through to the ordinary
    ///    chain). Otherwise fall through to `OrdinaryFirst`
    ///    immediately.
    /// 4. **`OrdinaryFirst` / `OrdinarySecond`** — pick `valueOf`
    ///    (default / number) or `toString` (string) for the first
    ///    slot; the other method for the second. If callable, push
    ///    a frame with no arguments. If neither slot returns a
    ///    primitive, raise `VmError::TypeMismatch` (task 25 will
    ///    upgrade this to a real `TypeError` Error object).
    ///
    /// Returns `Ok(true)` when the ladder pushed a frame (the
    /// dispatch loop must `continue` to the new top frame),
    /// `Ok(false)` when the ladder finished synchronously and pc
    /// advanced.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-toprimitive>
    /// - <https://tc39.es/ecma262/#sec-ordinarytoprimitive>
    fn drive_to_primitive(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let src = register_operand(operands.get(1))?;
        let hint_idx = const_operand(operands.get(2))?;
        let hint_token = lookup_string_constant(module, hint_idx)?;
        let hint = abstract_ops::ToPrimitiveHint::from_token(&hint_token)
            .ok_or(VmError::InvalidOperand)?;

        let top_idx = stack.len() - 1;
        let pc = stack[top_idx].pc;

        // 1. Resume path — only when the parked state matches this
        //    instruction. Read the result the called function wrote
        //    to `dst`; if primitive, finish.
        let resume = stack[top_idx]
            .pending_to_primitive
            .as_ref()
            .filter(|s| s.pc == pc && s.dst == dst)
            .cloned();
        if let Some(state) = resume {
            let produced = read_register(&stack[top_idx], dst)?.clone();
            if abstract_ops::is_primitive(&produced) {
                stack[top_idx].pending_to_primitive = None;
                stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                return Ok(false);
            }
            // Non-primitive — advance to the next stage.
            return self.drive_to_primitive_stage(stack, module, dst, state.obj, hint, state.stage);
        }

        // 2. Fresh entry — primitive fast path.
        let recv = read_register(&stack[top_idx], src)?.clone();
        if abstract_ops::is_primitive(&recv) {
            write_register(&mut stack[top_idx], dst, recv)?;
            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            return Ok(false);
        }

        // 3. Object operand — start the ladder at SymbolToPrim.
        self.drive_to_primitive_stage(
            stack,
            module,
            dst,
            recv,
            hint,
            ToPrimitiveStage::SymbolToPrim,
        )
    }

    /// Run a single stage of the §7.1.1 / §7.1.1.1 ladder, falling
    /// through synchronously when the chosen method is missing or
    /// non-callable until we either push a frame, throw, or write
    /// a primitive result.
    fn drive_to_primitive_stage(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        dst: u16,
        obj: Value,
        hint: abstract_ops::ToPrimitiveHint,
        mut stage: ToPrimitiveStage,
    ) -> Result<bool, VmError> {
        loop {
            match stage {
                ToPrimitiveStage::SymbolToPrim => {
                    let to_prim_sym = self.well_known_symbols.get(symbol::WellKnown::ToPrimitive);
                    let callee = match &obj {
                        Value::Object(o) => o.get_symbol(&to_prim_sym),
                        _ => None,
                    };
                    if let Some(callee) = callee
                        && is_callable(&callee)
                    {
                        let hint_str = JsString::from_str(hint.as_token(), &self.string_heap)?;
                        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                        args.push(Value::String(hint_str));
                        // §7.1.1 step 5.d. The resume guard
                        // upstream validates the result is a
                        // primitive — if not, that branch lands
                        // on `OrdinaryFirst` which is **wrong**
                        // per spec (a non-primitive return from
                        // `[Symbol.toPrimitive]` is supposed to
                        // throw TypeError directly). The runtime
                        // currently routes that case through the
                        // ordinary chain rather than throwing, to
                        // mirror the existing `Op::ToNumber` hook
                        // behaviour. Task 25 + a follow-up will
                        // tighten this branch to spec.
                        return self.push_to_primitive_call(
                            stack,
                            module,
                            dst,
                            obj.clone(),
                            hint,
                            ToPrimitiveStage::OrdinaryFirst,
                            &callee,
                            obj.clone(),
                            args,
                        );
                    }
                    stage = ToPrimitiveStage::OrdinaryFirst;
                }
                ToPrimitiveStage::OrdinaryFirst => {
                    let method = ordinary_method_for(hint, stage);
                    let callee = match &obj {
                        Value::Object(o) => o.get(method),
                        _ => None,
                    };
                    if let Some(callee) = callee
                        && is_callable(&callee)
                    {
                        // OrdinaryToPrimitive calls valueOf /
                        // toString with `this = obj` and no args.
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        return self.push_to_primitive_call(
                            stack,
                            module,
                            dst,
                            obj.clone(),
                            hint,
                            ToPrimitiveStage::OrdinarySecond,
                            &callee,
                            obj.clone(),
                            args,
                        );
                    }
                    // Fallback: when the prototype chain has no
                    // own / inherited callable for `method`, fall
                    // back to the synthetic Object.prototype
                    // intercept (the same one the call dispatcher
                    // routes plain `obj.valueOf()` / `obj.toString()`
                    // through). This keeps behaviour consistent
                    // for plain object literals which never receive
                    // a real Object.prototype linkage.
                    if let Value::Object(o) = &obj {
                        let no_args: SmallVec<[Value; 8]> = SmallVec::new();
                        if let Some(v) =
                            object_prototype_intercept(o, method, &no_args, &self.string_heap)?
                            && abstract_ops::is_primitive(&v)
                        {
                            let top_idx = stack.len() - 1;
                            stack[top_idx].pending_to_primitive = None;
                            write_register(&mut stack[top_idx], dst, v)?;
                            stack[top_idx].pc = stack[top_idx]
                                .pc
                                .checked_add(1)
                                .ok_or(VmError::InvalidOperand)?;
                            return Ok(false);
                        }
                    }
                    stage = ToPrimitiveStage::OrdinarySecond;
                }
                ToPrimitiveStage::OrdinarySecond => {
                    let method = ordinary_method_for(hint, stage);
                    let callee = match &obj {
                        Value::Object(o) => o.get(method),
                        _ => None,
                    };
                    if let Some(callee) = callee
                        && is_callable(&callee)
                    {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        // After OrdinarySecond the only spec-legal
                        // outcomes are: primitive result (resume
                        // path writes it) or non-primitive →
                        // throw. Park the stage as `Exhausted` so
                        // the resume re-entry can't loop back into
                        // this slot.
                        return self.push_to_primitive_call(
                            stack,
                            module,
                            dst,
                            obj.clone(),
                            hint,
                            ToPrimitiveStage::Exhausted,
                            &callee,
                            obj.clone(),
                            args,
                        );
                    }
                    // Same prototype-intercept fallback as
                    // OrdinaryFirst above — runs the second method
                    // (`toString` for hint=number, `valueOf` for
                    // hint=string) when the chain has nothing
                    // callable.
                    if let Value::Object(o) = &obj {
                        let no_args: SmallVec<[Value; 8]> = SmallVec::new();
                        if let Some(v) =
                            object_prototype_intercept(o, method, &no_args, &self.string_heap)?
                            && abstract_ops::is_primitive(&v)
                        {
                            let top_idx = stack.len() - 1;
                            stack[top_idx].pending_to_primitive = None;
                            write_register(&mut stack[top_idx], dst, v)?;
                            stack[top_idx].pc = stack[top_idx]
                                .pc
                                .checked_add(1)
                                .ok_or(VmError::InvalidOperand)?;
                            return Ok(false);
                        }
                    }
                    stage = ToPrimitiveStage::Exhausted;
                }
                ToPrimitiveStage::Exhausted => {
                    // §7.1.1.1 step 6 — TypeError. Task 25 will
                    // upgrade `VmError::TypeMismatch` to a real
                    // `TypeError` Error object.
                    let top_idx = stack.len() - 1;
                    stack[top_idx].pending_to_primitive = None;
                    return Err(VmError::TypeMismatch);
                }
            }
        }
    }

    /// Park `Op::ToPrimitive` ladder state on the running frame and
    /// invoke `callee`. The dispatcher re-enters the same opcode
    /// after the call returns; the resume path validates the
    /// result.
    #[allow(clippy::too_many_arguments)]
    fn push_to_primitive_call(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        dst: u16,
        obj: Value,
        hint: abstract_ops::ToPrimitiveHint,
        next_stage: ToPrimitiveStage,
        callee: &Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<bool, VmError> {
        let top_idx = stack.len() - 1;
        let pc = stack[top_idx].pc;
        stack[top_idx].pending_to_primitive = Some(PendingToPrimitive {
            pc,
            dst,
            obj,
            hint,
            stage: next_stage,
        });
        // pc stays on the Op::ToPrimitive instruction so the
        // dispatcher re-enters the resume path after the called
        // function returns.
        self.invoke(stack, module, callee, this_value, args, dst)?;
        Ok(true)
    }

    /// Execute `eval(source)` per §19.4.1.1 indirect-eval semantics:
    /// parse + compile via the embedder hook, then run `<main>`
    /// on a sub-stack. The current dispatch loop's stack stays
    /// untouched.
    ///
    /// # Errors
    /// - [`VmError::TypeMismatch`] when no eval hook is installed.
    /// - [`VmError::JsonError`] (mapped to `SyntaxError` by the
    ///   throwable-conversion path) when parsing / compilation fail.
    fn run_eval(&mut self, value: &Value) -> Result<Value, VmError> {
        let source = match value {
            Value::String(s) => s.to_lossy_string(),
            // Per §19.4.1.1 step 4, eval'd non-strings are returned
            // unchanged — `eval(42) === 42`.
            _ => return Ok(value.clone()),
        };
        let module = self.compile_eval_source(&source)?;
        let main = module.main();
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut entry = Frame::for_function_with_heap(main, &mut self.gc_heap)?;
        let entry_promise = if main.is_async {
            let result = JsPromiseHandle::pending();
            entry.async_state = Some(AsyncFrameState {
                result_promise: result.clone(),
            });
            Some(result)
        } else {
            None
        };
        stack.push(entry);
        let value = self.dispatch_loop(&module, &mut stack)?;
        if let Some(promise) = entry_promise {
            // Drain microtasks attached to top-level await so the
            // entry promise settles before we read its value.
            self.drain_microtasks(&module).map_err(|e| e.error)?;
            return Ok(match promise.state() {
                crate::promise::PromiseState::Fulfilled(v) => v,
                crate::promise::PromiseState::Rejected(reason) => {
                    return Err(VmError::Uncaught {
                        value: reason.display_string(),
                    });
                }
                crate::promise::PromiseState::Pending => Value::Undefined,
            });
        }
        Ok(value)
    }

    /// Build a `Function(args, body)` callable per §20.2.1.1. The
    /// result is a [`NativeFunction`] that holds the freshly
    /// compiled inner module and dispatches it on every call;
    /// inner-module function IDs aren't valid against the outer
    /// running module, so wrapping in a native rather than
    /// returning the inner closure handle directly keeps the call
    /// surface correct.
    fn build_function_constructor(&mut self, args: &[Value]) -> Result<Value, VmError> {
        // Coerce every argument to a string per §20.2.1.1 step 1.
        let parts: Vec<String> = args
            .iter()
            .map(|v| match v {
                Value::String(s) => s.to_lossy_string(),
                other => other.display_string(),
            })
            .collect();
        let (params, body): (Vec<&str>, &str) = if parts.is_empty() {
            (Vec::new(), "")
        } else {
            let body = parts.last().expect("non-empty checked above").as_str();
            let params: Vec<&str> = parts[..parts.len() - 1]
                .iter()
                .map(String::as_str)
                .collect();
            (params, body)
        };
        let params_joined = params.join(",");
        let source = format!("(function anonymous({params_joined}) {{\n{body}\n}})");
        let module = self.compile_eval_source(&source)?;
        // Running the synthesised module's `<main>` returns the
        // function value (the parenthesised expression is the
        // program's completion). We capture that value's
        // `function_id` together with the inner module so the
        // returned native can replay calls against the right
        // bytecode.
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(Frame::for_function_with_heap(
            module.main(),
            &mut self.gc_heap,
        )?);
        let value = self.dispatch_loop(&module, &mut stack)?;
        let function_id = match &value {
            Value::Function { function_id } => *function_id,
            Value::Closure { function_id, .. } => *function_id,
            _ => {
                return Err(VmError::JsonError {
                    code: "EVAL_NOT_FUNCTION",
                    message: "Function constructor body did not produce a function value"
                        .to_string(),
                });
            }
        };
        let module_rc = std::rc::Rc::new(module);
        let module_clone = std::rc::Rc::clone(&module_rc);
        let native = std::rc::Rc::new(NativeFunction::new(
            "anonymous",
            move |interp: &mut Interpreter, call_args: &[Value]| {
                interp
                    .invoke_eval_function(&module_clone, function_id, call_args)
                    .map_err(|err| crate::native_function::NativeError::TypeError {
                        name: "anonymous",
                        reason: format!("{err}"),
                    })
            },
        ));
        Ok(Value::NativeFunction(native))
    }

    /// Invoke a function from a previously-eval'd module on a
    /// fresh sub-stack, binding `args` to its formal parameters.
    /// Used by [`Self::build_function_constructor`] so that
    /// `new Function(...)` results stay callable across the outer
    /// module's lifetime.
    fn invoke_eval_function(
        &mut self,
        module: &BytecodeModule,
        function_id: u32,
        args: &[Value],
    ) -> Result<Value, VmError> {
        let function = module
            .functions
            .get(function_id as usize)
            .ok_or(VmError::InvalidOperand)?;
        let upvalues =
            Frame::build_upvalues(&mut self.gc_heap, function, std::rc::Rc::from(Vec::new()))?;
        let mut frame =
            Frame::with_return_upvalues_and_this(function, None, upvalues, Value::Undefined);
        // Bind args to parameter slots; extras drop, missing stay
        // `Value::Undefined` (matches §10.2.1.4 step 22.b).
        let bind_count = (function.param_count as usize).min(args.len());
        for (i, arg) in args.iter().take(bind_count).enumerate() {
            if let Some(slot) = frame.registers.get_mut(i) {
                *slot = arg.clone();
            }
        }
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(frame);
        self.dispatch_loop(module, &mut stack)
    }

    /// Helper — invoke the eval hook, mapping its error to a
    /// VmError that the throwable-conversion path will surface as
    /// `SyntaxError`.
    fn compile_eval_source(&self, source: &str) -> Result<BytecodeModule, VmError> {
        let hook = self.eval_hook.as_ref().ok_or_else(|| VmError::JsonError {
            code: "EVAL_DISABLED",
            message: "eval / new Function are disabled (no compiler hook installed)".to_string(),
        })?;
        hook(source).map_err(|message| VmError::JsonError {
            code: "EVAL_PARSE",
            message,
        })
    }

    /// Drive one tick of [`Op::LoadProperty`] when the receiver is
    /// an object and the resolved property is an accessor descriptor.
    /// Returns `Ok(true)` when an accessor was dispatched (frame
    /// pushed or undefined written) and the outer loop should
    /// `continue`; `Ok(false)` when the in-frame fast path should
    /// run (data slot, non-object receiver, or absent property).
    ///
    /// # Algorithm — §10.1.8 OrdinaryGet
    /// 1. Decode the operands and read the receiver register.
    /// 2. Probe the receiver's own + prototype chain.
    ///    - Absent / data slot: hand off to the in-frame fast path.
    ///    - Accessor with no getter: write `undefined` to `dst`,
    ///      advance pc, signal handled.
    ///    - Accessor with a getter: advance pc, push a call to the
    ///      getter with `this = receiver` and dst = `dst`.
    /// 3. Class constructors and other special receiver kinds skip
    ///    accessor handling: their property tables are plain data
    ///    today, so the in-frame match is authoritative.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-ordinaryget>
    fn drive_load_property(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let obj_reg = register_operand(operands.get(1))?;
        let name_idx = const_operand(operands.get(2))?;
        let name = lookup_string_constant(module, name_idx)?;
        let top_idx = stack.len() - 1;
        let receiver = read_register(&stack[top_idx], obj_reg)?.clone();
        // §28.2.4.4 Proxy.[[Get]] — invoke the `get` trap when
        // present; otherwise fall through to target.
        if let Value::Proxy(p) = &receiver {
            let proxy = p.clone();
            let key_str = JsString::from_str(&name, &self.string_heap)?;
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                proxy.target(),
                Value::String(key_str),
                Value::Proxy(proxy.clone()),
            ];
            let pc = stack[top_idx].pc;
            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            let result = match self.invoke_proxy_trap(module, &proxy, "get", trap_args)? {
                Some(v) => v,
                None => proxy.target_object().get(&name).unwrap_or(Value::Undefined),
            };
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(true);
        }
        let obj = match &receiver {
            Value::Object(o) => o.clone(),
            _ => return Ok(false),
        };
        match obj.lookup(&name) {
            object::PropertyLookup::Accessor { getter, .. } => {
                let pc = stack[top_idx].pc;
                stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                match getter {
                    Some(callee) if abstract_ops::is_callable(&callee) => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, module, &callee, receiver, args, dst)?;
                    }
                    _ => {
                        // No getter (or non-callable) — §10.1.8.1
                        // step 4.b returns undefined.
                        write_register(&mut stack[top_idx], dst, Value::Undefined)?;
                    }
                }
                Ok(true)
            }
            // Data or absent — fall through to the in-frame fast path.
            _ => Ok(false),
        }
    }

    /// Drive one tick of [`Op::StoreProperty`] when §10.1.9
    /// OrdinarySet routes through an accessor setter, hits a
    /// non-writable shadow, or hits a non-extensible receiver.
    /// Returns `Ok(true)` when the dispatch path took over,
    /// `Ok(false)` when the in-frame data-write fast path should run.
    ///
    /// Non-writable / accessor-without-setter / non-extensible
    /// rejections surface as [`VmError::TypeMismatch`] today —
    /// follow-up [task 25](../docs/new-engine/tasks/25-internal-error-throwability.md)
    /// upgrades these to real `TypeError` instances. Sloppy-mode
    /// silent rejection is filed against the same task.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-ordinaryset>
    /// - <https://tc39.es/ecma262/#sec-ordinarysetwithowndescriptor>
    fn drive_store_property(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let obj_reg = register_operand(operands.first())?;
        let name_idx = const_operand(operands.get(1))?;
        let src_reg = register_operand(operands.get(2))?;
        let scratch_reg = register_operand(operands.get(3))?;
        let name = lookup_string_constant(module, name_idx)?;
        let top_idx = stack.len() - 1;
        let receiver = read_register(&stack[top_idx], obj_reg)?.clone();
        let value = read_register(&stack[top_idx], src_reg)?.clone();
        // §28.2.4.5 Proxy.[[Set]] — invoke the `set` trap when
        // present; otherwise delegate to the target.
        if let Value::Proxy(p) = &receiver {
            let proxy = p.clone();
            let key_str = JsString::from_str(&name, &self.string_heap)?;
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                proxy.target(),
                Value::String(key_str),
                value.clone(),
                Value::Proxy(proxy.clone()),
            ];
            let pc = stack[top_idx].pc;
            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            match self.invoke_proxy_trap(module, &proxy, "set", trap_args)? {
                Some(_) => { /* trap handled the write; spec ignores return value
                    except for boolean-rejection — foundation accepts. */
                }
                None => {
                    proxy.target_object().set(&name, value);
                }
            }
            let _ = scratch_reg;
            return Ok(true);
        }
        let obj = match &receiver {
            Value::Object(o) => o.clone(),
            _ => return Ok(false),
        };
        let outcome = obj.resolve_set(&name);
        match outcome {
            object::SetOutcome::AssignData => {
                // Fall through to the in-frame data-write path so
                // the existing semantics keep applying.
                Ok(false)
            }
            object::SetOutcome::InvokeSetter { setter } => {
                if !abstract_ops::is_callable(&setter) {
                    // Spec §10.1.9 step 5.b — accessor with non-
                    // callable setter rejects.
                    return Err(VmError::TypeMismatch);
                }
                let pc = stack[top_idx].pc;
                stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                args.push(value);
                self.invoke(stack, module, &setter, receiver, args, scratch_reg)?;
                Ok(true)
            }
            object::SetOutcome::Reject { .. } => {
                // Spec routes this to a strict-mode TypeError. The
                // foundation has no strict flag yet — surface the
                // failure unconditionally so test fixtures observe
                // the rejection. Task 25 wires the real `TypeError`.
                Err(VmError::TypeMismatch)
            }
        }
    }

    /// §28.2.4.7 Proxy.[[HasProperty]] — invoke the `has` trap
    /// when the rhs of `key in obj` is a Proxy. Returns
    /// `Ok(true)` when the proxy path handled the op.
    fn drive_has_property_proxy(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let lhs_reg = register_operand(operands.get(1))?;
        let rhs_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let lhs = read_register(&stack[top_idx], lhs_reg)?.clone();
        let rhs = read_register(&stack[top_idx], rhs_reg)?.clone();
        let Value::Proxy(proxy) = rhs else {
            return Ok(false);
        };
        let key_str = match &lhs {
            Value::String(s) => s.clone(),
            other => JsString::from_str(&other.display_string(), &self.string_heap)?,
        };
        let trap_args: SmallVec<[Value; 8]> =
            smallvec::smallvec![proxy.target(), Value::String(key_str),];
        let pc = stack[top_idx].pc;
        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
        let present = match self.invoke_proxy_trap(module, &proxy, "has", trap_args)? {
            Some(v) => v.to_boolean(),
            None => {
                let key = match &lhs {
                    Value::String(s) => s.to_lossy_string(),
                    other => other.display_string(),
                };
                !matches!(
                    proxy.target_object().lookup(&key),
                    object::PropertyLookup::Absent
                )
            }
        };
        write_register(&mut stack[top_idx], dst, Value::Boolean(present))?;
        Ok(true)
    }

    /// §28.2.4.10 Proxy.[[Delete]] — invoke the `deleteProperty`
    /// trap when the receiver of `delete obj.x` is a Proxy.
    fn drive_delete_property_proxy(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let obj_reg = register_operand(operands.get(1))?;
        let name_idx = const_operand(operands.get(2))?;
        let name = lookup_string_constant(module, name_idx)?;
        let top_idx = stack.len() - 1;
        let receiver = read_register(&stack[top_idx], obj_reg)?.clone();
        let Value::Proxy(proxy) = receiver else {
            return Ok(false);
        };
        let key_str = JsString::from_str(&name, &self.string_heap)?;
        let trap_args: SmallVec<[Value; 8]> =
            smallvec::smallvec![proxy.target(), Value::String(key_str),];
        let pc = stack[top_idx].pc;
        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
        let removed = match self.invoke_proxy_trap(module, &proxy, "deleteProperty", trap_args)? {
            Some(v) => v.to_boolean(),
            None => proxy.target_object().delete(&name),
        };
        write_register(&mut stack[top_idx], dst, Value::Boolean(removed))?;
        Ok(true)
    }

    /// §28.2.4.1 Proxy.[[GetPrototypeOf]] — invoke the
    /// `getPrototypeOf` trap when the source is a Proxy.
    fn drive_get_prototype_proxy(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let src = register_operand(operands.get(1))?;
        let top_idx = stack.len() - 1;
        let value = read_register(&stack[top_idx], src)?.clone();
        let Value::Proxy(proxy) = value else {
            return Ok(false);
        };
        let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![proxy.target()];
        let pc = stack[top_idx].pc;
        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
        let result = match self.invoke_proxy_trap(module, &proxy, "getPrototypeOf", trap_args)? {
            Some(v) => v,
            None => match proxy.target_object().prototype() {
                Some(p) => Value::Object(p),
                None => Value::Null,
            },
        };
        write_register(&mut stack[top_idx], dst, result)?;
        Ok(true)
    }

    /// §28.2.4.2 Proxy.[[SetPrototypeOf]] — invoke the
    /// `setPrototypeOf` trap when the receiver is a Proxy.
    fn drive_set_prototype_proxy(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let obj_reg = register_operand(operands.first())?;
        let proto_reg = register_operand(operands.get(1))?;
        let top_idx = stack.len() - 1;
        let recv = read_register(&stack[top_idx], obj_reg)?.clone();
        let Value::Proxy(proxy) = recv else {
            return Ok(false);
        };
        let proto_val = read_register(&stack[top_idx], proto_reg)?.clone();
        let proto_obj = match &proto_val {
            Value::Object(_) | Value::Null => proto_val.clone(),
            Value::ClassConstructor(c) => Value::Object(c.statics.clone()),
            _ => return Err(VmError::TypeMismatch),
        };
        let trap_args: SmallVec<[Value; 8]> =
            smallvec::smallvec![proxy.target(), proto_obj.clone()];
        let pc = stack[top_idx].pc;
        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
        match self.invoke_proxy_trap(module, &proxy, "setPrototypeOf", trap_args)? {
            Some(_) => {}
            None => {
                let proto = match proto_obj {
                    Value::Object(p) => Some(p),
                    Value::Null => None,
                    _ => return Err(VmError::TypeMismatch),
                };
                proxy.target_object().set_prototype(proto);
            }
        }
        Ok(true)
    }

    /// Drive one tick of [`Op::GetIterator`] for user objects.
    ///
    /// Returns `Ok(true)` when the dispatcher must restart the
    /// outer loop (frame pushed or pc advanced synchronously),
    /// `Ok(false)` when the source operand is a built-in iterable
    /// and the in-frame fast path should run instead.
    ///
    /// # Algorithm (§7.4.3 `GetIterator`)
    /// 1. **Resume** — when the running frame's
    ///    [`Frame::pending_get_iterator`] matches the current pc,
    ///    read the called function's result from `dst`. The result
    ///    must be an Object (the iterator). On non-Object, raise
    ///    `TypeMismatch` (foundation surface for §7.4.3 step 2's
    ///    TypeError; task 25 upgrades to a real Error).
    /// 2. **Fresh entry, built-in** — `Value::Array` / `String` /
    ///    `Map` / `Set` flow through the existing fast path.
    /// 3. **Fresh entry, user object** — look up
    ///    `[Symbol.iterator]`; if callable, push a frame to invoke
    ///    it with `this = obj`, no arguments. Pc stays on the
    ///    `Op::GetIterator` so resume can wrap the returned
    ///    iterator object as [`IteratorState::User`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-getiterator>
    fn drive_get_iterator(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let src = register_operand(operands.get(1))?;
        let top_idx = stack.len() - 1;
        let pc = stack[top_idx].pc;

        // 1. Resume path.
        let resume = stack[top_idx]
            .pending_get_iterator
            .as_ref()
            .filter(|s| s.pc == pc && s.dst == dst)
            .cloned();
        if let Some(_state) = resume {
            let produced = read_register(&stack[top_idx], dst)?.clone();
            // §7.4.3 step 2 — `[@@iterator]()` must return an
            // Object. Anything else is a TypeError.
            if !matches!(produced, Value::Object(_)) {
                stack[top_idx].pending_get_iterator = None;
                return Err(VmError::TypeMismatch);
            }
            let iter_state = IteratorState::User { iterator: produced };
            let iter = std::rc::Rc::new(std::cell::RefCell::new(iter_state));
            write_register(&mut stack[top_idx], dst, Value::Iterator(iter))?;
            stack[top_idx].pending_get_iterator = None;
            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            return Ok(true);
        }

        // 2 + 3. Fresh entry — only intercept user objects. The
        // built-in fast path is the existing in-frame match arm.
        let value = read_register(&stack[top_idx], src)?.clone();
        let Value::Object(obj) = &value else {
            return Ok(false);
        };
        let iter_sym = self.well_known_symbols.get(symbol::WellKnown::Iterator);
        let Some(callee) = obj.get_symbol(&iter_sym) else {
            // No `[Symbol.iterator]` — §7.4.3 step 2 throws.
            return Err(VmError::TypeMismatch);
        };
        if !is_callable(&callee) {
            return Err(VmError::TypeMismatch);
        }
        stack[top_idx].pending_get_iterator = Some(PendingGetIterator { pc, dst });
        let args: SmallVec<[Value; 8]> = SmallVec::new();
        // pc stays on Op::GetIterator; the called frame's result
        // lands in `dst` and the resume guard above wraps it.
        self.invoke(stack, module, &callee, value, args, dst)?;
        Ok(true)
    }

    /// Drive one tick of [`Op::IteratorNext`] for user iterators.
    ///
    /// Returns `Ok(true)` when the dispatcher must restart (frame
    /// pushed or pc advanced synchronously), `Ok(false)` when the
    /// iterator is a built-in synchronous shape and the in-frame
    /// fast path should run.
    ///
    /// # Algorithm (§7.4.5 `IteratorNext`)
    /// 1. **Resume** — read the result record from the scratch
    ///    register; pull `value` and `done`; truthy `done`
    ///    transitions the iterator to `Exhausted` per §7.4.2 step 6.
    /// 2. **Fresh entry, built-in iterator** — fall through.
    /// 3. **Fresh entry, user iterator** — look up `iterator.next`,
    ///    push a frame to invoke it with `this = iterator`, no
    ///    arguments. Result lands in a scratch slot adjacent to
    ///    the `value` / `done` destinations.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-iteratornext>
    /// - <https://tc39.es/ecma262/#sec-iteratorcomplete>
    /// - <https://tc39.es/ecma262/#sec-iteratorvalue>
    fn drive_iterator_next(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let value_dst = register_operand(operands.first())?;
        let done_dst = register_operand(operands.get(1))?;
        let iter_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let pc = stack[top_idx].pc;

        // 1. Resume path — read the parked record.
        let resume = stack[top_idx]
            .pending_iterator_next
            .as_ref()
            .filter(|s| s.pc == pc && s.value_dst == value_dst && s.done_dst == done_dst)
            .cloned();
        if let Some(state) = resume {
            let result = read_register(&stack[top_idx], state.result_reg)?.clone();
            let Value::Object(obj) = &result else {
                stack[top_idx].pending_iterator_next = None;
                return Err(VmError::TypeMismatch);
            };
            let value = obj.get("value").unwrap_or(Value::Undefined);
            let done_value = obj.get("done").unwrap_or(Value::Undefined);
            let done = done_value.to_boolean();
            if done {
                if let Value::Iterator(rc) = &state.iterator {
                    *rc.borrow_mut() = IteratorState::Exhausted;
                }
            }
            write_register(&mut stack[top_idx], value_dst, value)?;
            write_register(&mut stack[top_idx], done_dst, Value::Boolean(done))?;
            stack[top_idx].pending_iterator_next = None;
            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            return Ok(true);
        }

        // 2 + 3. Fresh entry. Inspect the iterator's inner state.
        let iter_value = read_register(&stack[top_idx], iter_reg)?.clone();
        let Value::Iterator(iter_rc) = &iter_value else {
            return Err(VmError::TypeMismatch);
        };
        // §27.5 generator-state path — drive the suspended body
        // synchronously and write the unpacked `value` / `done`
        // pair into the caller's destination registers.
        let gen_handle = match &*iter_rc.borrow() {
            IteratorState::Generator { handle } => Some(handle.clone()),
            _ => None,
        };
        if let Some(handle) = gen_handle {
            let result = self.resume_generator(
                module,
                &handle,
                GeneratorResumeKind::Next(Value::Undefined),
            )?;
            let Value::Object(obj) = &result else {
                return Err(VmError::TypeMismatch);
            };
            let value = obj.get("value").unwrap_or(Value::Undefined);
            let done = obj.get("done").unwrap_or(Value::Undefined).to_boolean();
            if done {
                *iter_rc.borrow_mut() = IteratorState::Exhausted;
            }
            write_register(&mut stack[top_idx], value_dst, value)?;
            write_register(&mut stack[top_idx], done_dst, Value::Boolean(done))?;
            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            return Ok(true);
        }
        // Helper-wrapper iterator states drive through the
        // interpreter-aware step path so callbacks can run.
        let needs_full_step = matches!(
            &*iter_rc.borrow(),
            IteratorState::Map { .. }
                | IteratorState::Filter { .. }
                | IteratorState::Take { .. }
                | IteratorState::Drop { .. }
                | IteratorState::FlatMap { .. }
        );
        if needs_full_step {
            let (value, done) = self.iterator_next_full(module, iter_rc)?;
            write_register(&mut stack[top_idx], value_dst, value)?;
            write_register(&mut stack[top_idx], done_dst, Value::Boolean(done))?;
            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            return Ok(true);
        }
        // Snapshot the user iterator object out of the inner
        // state so the borrow does not span the `invoke` call
        // below.
        let user_iter = match &*iter_rc.borrow() {
            IteratorState::User { iterator } => Some(iterator.clone()),
            _ => None,
        };
        let Some(user_iter_value) = user_iter else {
            // Built-in iterator — let the synchronous in-frame
            // path drive it.
            return Ok(false);
        };
        // Already-exhausted user iterators short-circuit per
        // §7.4.2 step 6.
        let Value::Object(iter_obj) = &user_iter_value else {
            return Err(VmError::TypeMismatch);
        };
        let next_fn = iter_obj.get("next").ok_or(VmError::TypeMismatch)?;
        if !is_callable(&next_fn) {
            return Err(VmError::TypeMismatch);
        }
        // Park the state and push a call. `result_reg` reuses the
        // `value_dst` slot — the resume step overwrites it with
        // the unpacked value before the user code observes it.
        stack[top_idx].pending_iterator_next = Some(PendingIteratorNext {
            pc,
            value_dst,
            done_dst,
            result_reg: value_dst,
            iterator: iter_value,
        });
        let args: SmallVec<[Value; 8]> = SmallVec::new();
        self.invoke(stack, module, &next_fn, user_iter_value, args, value_dst)?;
        Ok(true)
    }

    fn binop_regs(
        &self,
        operands: &[Operand],
        frame: &Frame,
    ) -> Result<(u16, Value, Value), VmError> {
        let dst = register_operand(operands.first())?;
        let lhs = register_operand(operands.get(1))?;
        let rhs = register_operand(operands.get(2))?;
        let l = read_register(frame, lhs)?.clone();
        let r = read_register(frame, rhs)?.clone();
        Ok((dst, l, r))
    }

    fn run_numeric(
        &self,
        operands: &[Operand],
        frame: &mut Frame,
        op: fn(NumberValue, NumberValue) -> NumberValue,
        bigint_op: BigIntBinop,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = self.binop_regs(operands, frame)?;
        // §13.15.3 ApplyStringOrNumericBinaryOperator step 5/6:
        // non-additive numeric ops apply ToNumeric to each operand
        // (the compiler emits ToPrimitive(number) ahead of these
        // ops so by the time we get here only primitives remain).
        // A non-bigint operand becomes Number; bigint stays BigInt.
        // <https://tc39.es/ecma262/#sec-applystringornumericbinaryoperator>
        let lnum = abstract_ops::to_numeric_kind(&lhs).ok_or(VmError::TypeMismatch)?;
        let rnum = abstract_ops::to_numeric_kind(&rhs).ok_or(VmError::TypeMismatch)?;
        let result = match (lnum, rnum) {
            (abstract_ops::NumericKind::Num(a), abstract_ops::NumericKind::Num(b)) => {
                Value::Number(op(a, b))
            }
            (abstract_ops::NumericKind::Big(a), abstract_ops::NumericKind::Big(b)) => {
                Value::BigInt(bigint_op(&a, &b).map_err(bigint_to_vm_error)?)
            }
            // Mixed Number/BigInt is a spec TypeError.
            _ => return Err(VmError::TypeMismatch),
        };
        write_register(frame, dst, result)?;
        frame.pc += 1;
        Ok(())
    }

    /// Implements ECMA-262 §13.15.4
    /// `ApplyStringOrNumericBinaryOperator` for the `+` operator
    /// after the compiler has already coerced both operands through
    /// `Op::ToPrimitive(default)`.
    ///
    /// # Algorithm
    /// 1. If either operand is a `String`, ToString the other
    ///    operand and return the concatenation.
    /// 2. Otherwise apply spec-faithful numeric add — `Number +
    ///    Number` and `BigInt + BigInt` keep their kind; mixed
    ///    `Number` / `BigInt` is a `TypeError`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-applystringornumericbinaryoperator>
    fn run_add(
        &self,
        _module: &BytecodeModule,
        operands: &[Operand],
        frame: &mut Frame,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = self.binop_regs(operands, frame)?;
        let result = if matches!(lhs, Value::String(_)) || matches!(rhs, Value::String(_)) {
            // §13.15.4 step 1.c.ii — string concat path. The
            // operand that is already a String stays as-is; the
            // other goes through ToString.
            let l_str = self.to_display_string(&lhs)?;
            let r_str = self.to_display_string(&rhs)?;
            Value::String(JsString::concat(&l_str, &r_str, &self.string_heap)?)
        } else {
            match (&lhs, &rhs) {
                (Value::Number(a), Value::Number(b)) => Value::Number(number::add(*a, *b)),
                (Value::BigInt(a), Value::BigInt(b)) => Value::BigInt(bigint::ops::add(a, b)),
                (Value::Number(_), Value::BigInt(_)) | (Value::BigInt(_), Value::Number(_)) => {
                    return Err(VmError::TypeMismatch);
                }
                _ => return Err(VmError::TypeMismatch),
            }
        };
        write_register(frame, dst, result)?;
        frame.pc += 1;
        Ok(())
    }

    /// Display-form `ToString` over already-primitive `Value`s.
    ///
    /// Used by [`Self::run_add`]'s string-concat path — the
    /// compiler has already inserted `Op::ToPrimitive(default)`
    /// before the `+` so any object operand has been collapsed.
    /// Symbol operands raise a `TypeError` per §7.1.17 step 4.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-tostring>
    fn to_display_string(&self, value: &Value) -> Result<JsString, VmError> {
        match value {
            Value::String(s) => Ok(s.clone()),
            Value::Number(n) => Ok(JsString::from_str(
                &n.to_display_string(),
                &self.string_heap,
            )?),
            Value::BigInt(b) => Ok(JsString::from_str(
                &b.to_decimal_string(),
                &self.string_heap,
            )?),
            Value::Boolean(true) => Ok(JsString::from_str("true", &self.string_heap)?),
            Value::Boolean(false) => Ok(JsString::from_str("false", &self.string_heap)?),
            Value::Null => Ok(JsString::from_str("null", &self.string_heap)?),
            Value::Undefined => Ok(JsString::from_str("undefined", &self.string_heap)?),
            // §7.1.17 step 4 — Symbol → TypeError.
            Value::Symbol(_) => Err(VmError::TypeMismatch),
            // Object-shaped values would normally come through
            // ToPrimitive(string) first; reaching here means an
            // object slipped through (e.g. ToPrimitive(default)
            // returned an object via [Symbol.toPrimitive], in
            // which case the resume path already raised
            // TypeMismatch).
            _ => Err(VmError::TypeMismatch),
        }
    }

    /// Implements ECMA-262 §7.2.14 `AbstractRelationalComparison`
    /// for the four relational operators `<`, `<=`, `>`, `>=`.
    /// The compiler has already coerced both operands through
    /// `Op::ToPrimitive(number)`, so the runtime sees primitives.
    ///
    /// # Algorithm
    /// 1. Delegate to [`abstract_ops::abstract_relational_comparison`]
    ///    with the operands in the canonical order — `lhs < rhs`
    ///    for `LessThan` / `LessEq`, swapped for `GreaterThan` /
    ///    `GreaterEq`.
    /// 2. Translate the [`abstract_ops::RelationalOutcome`] into
    ///    the boolean each opcode reports:
    ///    - `<`  / `>`  → `LessThan` only.
    ///    - `<=` / `>=` → spec `r === undefined ? false : !r` (i.e.
    ///      `NotLessThan` of the swapped operands).
    ///    - `Undefined` → always `false`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-abstract-relational-comparison>
    fn run_compare(&self, operands: &[Operand], frame: &mut Frame, op: Op) -> Result<(), VmError> {
        let (dst, lhs, rhs) = self.binop_regs(operands, frame)?;
        let truthy = match op {
            Op::LessThan => {
                matches!(
                    abstract_ops::abstract_relational_comparison(&lhs, &rhs),
                    abstract_ops::RelationalOutcome::LessThan
                )
            }
            Op::GreaterThan => {
                matches!(
                    abstract_ops::abstract_relational_comparison(&rhs, &lhs),
                    abstract_ops::RelationalOutcome::LessThan
                )
            }
            Op::LessEq => matches!(
                abstract_ops::abstract_relational_comparison(&rhs, &lhs),
                abstract_ops::RelationalOutcome::NotLessThan
            ),
            Op::GreaterEq => matches!(
                abstract_ops::abstract_relational_comparison(&lhs, &rhs),
                abstract_ops::RelationalOutcome::NotLessThan
            ),
            _ => unreachable!("run_compare called with non-relational op"),
        };
        write_register(frame, dst, Value::Boolean(truthy))?;
        frame.pc += 1;
        Ok(())
    }
}

/// Function-pointer alias for the BigInt sibling of the
/// `NumberValue` arithmetic helpers. A few `BigInt` ops can fail
/// (division by zero, negative exponent, oversized shift); the
/// VM dispatcher maps each error variant to the matching
/// `VmError`.
type BigIntBinop = fn(
    &bigint::BigIntValue,
    &bigint::BigIntValue,
) -> Result<bigint::BigIntValue, bigint::ops::OpError>;

fn bigint_sub_op(
    a: &bigint::BigIntValue,
    b: &bigint::BigIntValue,
) -> Result<bigint::BigIntValue, bigint::ops::OpError> {
    Ok(bigint::ops::sub(a, b))
}

fn bigint_mul_op(
    a: &bigint::BigIntValue,
    b: &bigint::BigIntValue,
) -> Result<bigint::BigIntValue, bigint::ops::OpError> {
    Ok(bigint::ops::mul(a, b))
}

fn bigint_and_op(
    a: &bigint::BigIntValue,
    b: &bigint::BigIntValue,
) -> Result<bigint::BigIntValue, bigint::ops::OpError> {
    Ok(bigint::ops::bitwise_and(a, b))
}

fn bigint_or_op(
    a: &bigint::BigIntValue,
    b: &bigint::BigIntValue,
) -> Result<bigint::BigIntValue, bigint::ops::OpError> {
    Ok(bigint::ops::bitwise_or(a, b))
}

fn bigint_xor_op(
    a: &bigint::BigIntValue,
    b: &bigint::BigIntValue,
) -> Result<bigint::BigIntValue, bigint::ops::OpError> {
    Ok(bigint::ops::bitwise_xor(a, b))
}

/// Map [`bigint::ops::OpError`] into the surrounding [`VmError`].
fn bigint_to_vm_error(err: bigint::ops::OpError) -> VmError {
    match err {
        bigint::ops::OpError::DivisionByZero
        | bigint::ops::OpError::NegativeExponent
        | bigint::ops::OpError::ShiftOutOfRange => VmError::TypeMismatch,
    }
}

/// Walk a live frame stack top-down and build a snapshot the
/// runtime / CLI can render. Top-of-stack first.
fn snapshot_frames(module: &BytecodeModule, stack: &[Frame]) -> Vec<StackFrameSnapshot> {
    stack
        .iter()
        .rev()
        .map(|f| {
            let function = module.functions.get(f.function_id as usize);
            let function_name = function
                .map(|fun| fun.name.clone())
                .unwrap_or_else(|| "<unknown>".to_string());
            let span = function
                .and_then(|fun| fun.spans.iter().find(|s| s.pc == f.pc).map(|s| s.span))
                .or_else(|| function.map(|fun| fun.span))
                .unwrap_or((0, 0));
            StackFrameSnapshot {
                function_name,
                module: module.module.clone(),
                span,
            }
        })
        .collect()
}

fn math_to_vm_error(err: math::MathError) -> VmError {
    match err {
        math::MathError::UnknownMember(name) => VmError::UnknownIntrinsic {
            name: format!("Math.{name}"),
        },
        math::MathError::BadArgument { .. } => VmError::TypeMismatch,
    }
}

fn symbol_to_vm_error(err: symbol_dispatch::SymbolError) -> VmError {
    match err {
        symbol_dispatch::SymbolError::UnknownMember(name) => VmError::UnknownIntrinsic {
            name: format!("Symbol.{name}"),
        },
        symbol_dispatch::SymbolError::BadArgument { .. } => VmError::TypeMismatch,
        symbol_dispatch::SymbolError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => VmError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
    }
}

fn intl_to_vm_error(err: intl::IntlError) -> VmError {
    match err {
        intl::IntlError::UnknownClass(name) => VmError::UnknownIntrinsic {
            name: format!("Intl.{name}"),
        },
        intl::IntlError::UnknownMember { class, method } => VmError::UnknownIntrinsic {
            name: format!("Intl.{class}.prototype.{method}"),
        },
        intl::IntlError::BadArgument { .. } => VmError::TypeMismatch,
        intl::IntlError::Engine { message, .. } => VmError::Uncaught { value: message },
        intl::IntlError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => VmError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
    }
}

fn temporal_to_vm_error(err: temporal::TemporalError) -> VmError {
    match err {
        temporal::TemporalError::UnknownMember { class, method } => VmError::UnknownIntrinsic {
            name: format!("Temporal.{class}.{method}"),
        },
        temporal::TemporalError::BadArgument { .. } => VmError::TypeMismatch,
        temporal::TemporalError::Engine { message, .. } => VmError::Uncaught { value: message },
        temporal::TemporalError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => VmError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
    }
}

fn native_to_vm_error(err: NativeError) -> VmError {
    match err {
        NativeError::Thrown { name: _, message } => VmError::Uncaught { value: message },
        NativeError::TypeError { .. } => VmError::TypeMismatch,
    }
}

/// Convert a `VmError` into a JS `Value` used as a rejection
/// reason for promise reactions. Foundation: a plain string is
/// fine; once the full Error hierarchy is in we'll synthesize a
/// real `TypeError` / `RangeError` instance.
fn vm_err_to_value(err: &VmError) -> Value {
    Value::String(
        crate::JsString::from_str(&err.to_string(), &crate::StringHeap::default()).unwrap_or_else(
            |_| {
                // Allocator failure here is exceptional; substitute
                // an empty string rather than panicking.
                crate::JsString::from_str("", &crate::StringHeap::default())
                    .expect("empty string allocates")
            },
        ),
    )
}

fn json_to_vm_error(err: json::JsonError) -> VmError {
    // Diagnostic strings stay short and spec-faithful (no cycle
    // path-walk) to match the identity-pointer visit set. Parse
    // errors additionally carry the byte position so users can
    // locate the offending token.
    match err {
        json::JsonError::UnknownMember(name) => VmError::UnknownIntrinsic {
            name: format!("JSON.{name}"),
        },
        json::JsonError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => VmError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
        json::JsonError::Cyclic => VmError::JsonError {
            code: "JSON_CYCLIC",
            message: "JSON.stringify cannot serialize cyclic structures.".to_string(),
        },
        json::JsonError::BigInt => VmError::JsonError {
            code: "JSON_BIGINT",
            message: "JSON.stringify cannot serialize BigInt values.".to_string(),
        },
        json::JsonError::TooDeep { limit } => VmError::JsonError {
            code: "JSON_DEPTH",
            message: format!("JSON nesting exceeded {limit} levels."),
        },
        json::JsonError::ParseFailed { message, position } => VmError::JsonError {
            code: "JSON_PARSE",
            message: format!("JSON Parse error: {message} at byte {position}"),
        },
        json::JsonError::BadArgument {
            name,
            index,
            reason,
        } => VmError::JsonError {
            code: "JSON_BAD_ARG",
            message: format!("JSON.{name} argument {index} {reason}"),
        },
    }
}

fn intrinsic_to_vm_error(err: IntrinsicError) -> VmError {
    match err {
        IntrinsicError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => VmError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
        IntrinsicError::BadReceiver { .. } | IntrinsicError::BadArgument { .. } => {
            VmError::TypeMismatch
        }
        IntrinsicError::UnknownMethod { name } => VmError::UnknownIntrinsic { name },
    }
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}

fn register_operand(operand: Option<&Operand>) -> Result<u16, VmError> {
    match operand {
        Some(Operand::Register(r)) => Ok(*r),
        _ => Err(VmError::InvalidOperand),
    }
}

fn const_operand(operand: Option<&Operand>) -> Result<u32, VmError> {
    match operand {
        Some(Operand::ConstIndex(k)) => Ok(*k),
        _ => Err(VmError::InvalidOperand),
    }
}

/// Resolve a string constant referenced by index. Returned as a
/// Rust `String` because `JsObject` keys are stored UTF-8 in this
/// slice; task 18 (shapes) revisits the key representation.
fn lookup_string_constant(module: &BytecodeModule, idx: u32) -> Result<String, VmError> {
    match module.constants.get(idx as usize) {
        Some(Constant::String { utf16 }) => Ok(String::from_utf16_lossy(utf16)),
        _ => Err(VmError::InvalidOperand),
    }
}

fn imm32_operand(operand: Option<&Operand>) -> Result<i32, VmError> {
    match operand {
        Some(Operand::Imm32(v)) => Ok(*v),
        _ => Err(VmError::InvalidOperand),
    }
}

/// Apply a relative branch. Negative offsets are back-edges and
/// poll the interrupt flag — that's the foundation plan's
/// `every back-edge polls the runtime checkpoint` rule.
fn apply_branch(frame: &mut Frame, offset: i32, interrupt: &InterruptFlag) -> Result<(), VmError> {
    let next_pc = (frame.pc as i64 + 1).saturating_add(offset as i64);
    if next_pc < 0 || next_pc > u32::MAX as i64 {
        return Err(VmError::InvalidOperand);
    }
    if offset < 0 && interrupt.is_set() {
        return Err(VmError::Interrupted);
    }
    frame.pc = next_pc as u32;
    Ok(())
}

/// Render an uncaught JS value for diagnostic output. Routes
/// Error-shaped objects through [`error_classes::render_error_to_string`]
/// so the unwind printout matches what `e.toString()` returns at
/// the JS surface (§20.5.3.4).
fn render_thrown_value(value: &Value) -> String {
    if let Value::Object(obj) = value {
        // Treat anything with both `name` and `message` data slots
        // as an Error instance. Plain objects fall through to
        // `[object Object]` via `display_string`.
        let has_name = obj.get("name").is_some();
        let has_message = obj.get("message").is_some();
        if has_name || has_message {
            let rendered = error_classes::render_error_to_string(value);
            if !rendered.is_empty() {
                return rendered;
            }
        }
    }
    value.display_string()
}

/// §20.2.3.5 — render a callable as a string. Foundation returns
/// the canonical placeholder `function <name>() { [native code] }`
/// for every callable shape; source-faithful output is filed.
fn function_to_string(module: &BytecodeModule, callee: &Value) -> String {
    let display = match callee {
        Value::Function { function_id } | Value::Closure { function_id, .. } => module
            .functions
            .get(*function_id as usize)
            .map(|f| f.name.as_str())
            .unwrap_or("anonymous"),
        Value::NativeFunction(n) => n.name,
        Value::ClassConstructor(c) => match &c.ctor {
            Value::Function { function_id } | Value::Closure { function_id, .. } => module
                .functions
                .get(*function_id as usize)
                .map(|f| f.name.as_str())
                .unwrap_or("anonymous"),
            _ => "anonymous",
        },
        Value::BoundFunction(_) => "bound",
        _ => "anonymous",
    };
    format!("function {display}() {{ [native code] }}")
}

/// §20.2.4 — read `name` or `length` off a function record. `name`
/// returns the recorded display name (`<anonymous>` for unnamed
/// expressions); `length` returns `param_count` minus the rest
/// parameter (excluded per spec §10.2.4).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-function-instances-name>
/// - <https://tc39.es/ecma262/#sec-function-instances-length>
fn function_intrinsic_property(
    module: &BytecodeModule,
    function_id: u32,
    name: &str,
    heap: &StringHeap,
) -> Result<Value, VmError> {
    let function = module
        .functions
        .get(function_id as usize)
        .ok_or(VmError::InvalidOperand)?;
    Ok(match name {
        "name" => {
            let display = function.name.as_str();
            // Synthetic class names like "<anonymous>" / "<class>"
            // round-trip as their literal text — matching V8's
            // `(function () {}).name === ""` behaviour requires
            // empty-string canonicalisation, which is filed.
            let s = JsString::from_str(display, heap).map_err(|_| VmError::TypeMismatch)?;
            Value::String(s)
        }
        "length" => {
            // `param_count` already excludes the rest parameter
            // (it lives separately in `Function::has_rest`), so it
            // matches §10.2.4 ExpectedArgumentCount directly.
            // Default-valued parameters would also reduce the
            // count; the foundation tracks them as ordinary params
            // for now — filed in the param-defaults follow-up.
            Value::Number(NumberValue::from_i32(function.param_count as i32))
        }
        _ => Value::Undefined,
    })
}

/// §20.2.3.2 — derive `.name` / `.length` for a bound function.
/// The spec prepends `"bound "` to the target's name; foundation
/// matches that. `length` is `target.length - bound_args.len()`,
/// clamped at zero.
fn bound_function_intrinsic_property(
    module: &BytecodeModule,
    bound: &BoundFunction,
    name: &str,
    heap: &StringHeap,
) -> Result<Value, VmError> {
    let inner = match &bound.target {
        Value::Function { function_id } | Value::Closure { function_id, .. } => Some(
            module
                .functions
                .get(*function_id as usize)
                .ok_or(VmError::InvalidOperand)?,
        ),
        _ => None,
    };
    Ok(match name {
        "name" => {
            let inner_name = inner.map(|f| f.name.as_str()).unwrap_or("");
            let s = JsString::from_str(&format!("bound {inner_name}"), heap)
                .map_err(|_| VmError::TypeMismatch)?;
            Value::String(s)
        }
        "length" => {
            let raw = inner.map(|f| f.param_count as i32).unwrap_or(0);
            let bound_count = bound.bound_args.len() as i32;
            Value::Number(NumberValue::from_i32(
                raw.saturating_sub(bound_count).max(0),
            ))
        }
        _ => Value::Undefined,
    })
}

/// Build the per-Interpreter shared `globalThis` JsObject. Foundation
/// seeds the self-reference so `globalThis.globalThis === globalThis`
/// and leaves the rest of the global namespace empty — the §19.2
/// global functions are reached through `Op::GlobalCall`, not
/// `globalThis.parseInt`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-globalthis>
fn build_global_this() -> JsObject {
    let obj = JsObject::new();
    obj.set("globalThis", Value::Object(obj.clone()));
    // §19.1 standard globals — install placeholder constructor
    // sentinels so identifier-as-value reads (e.g. `Array.prototype`,
    // `Object.keys`) resolve to *something* rather than throwing
    // ReferenceError. Each placeholder has a `prototype` own
    // property pointing at a fresh object; the engine's existing
    // intrinsic interceptors (`Op::ArrayCall` / `Op::ObjectCall` /
    // …) handle the call sites that matter, and reads of `.length`
    // / `.name` arrive at the per-function intrinsic helper.
    //
    // This is intentionally minimal — full constructor semantics
    // (e.g. `new Array(n)` returning an array of length `n`) await
    // the dedicated §22 / §23 engine track. The placeholder lets
    // tests that *don't* rely on the constructor identity compile
    // and run.
    for name in [
        "Array",
        "Object",
        "JSON",
        "String",
        "Number",
        "Boolean",
        "BigInt",
        "Symbol",
        "Math",
        "Date",
        "RegExp",
        "Map",
        "Set",
        "WeakMap",
        "WeakSet",
        "WeakRef",
        "Promise",
        "Proxy",
        "Reflect",
        "Function",
        "ArrayBuffer",
        "SharedArrayBuffer",
        "DataView",
        "Int8Array",
        "Uint8Array",
        "Uint8ClampedArray",
        "Int16Array",
        "Uint16Array",
        "Int32Array",
        "Uint32Array",
        "Float32Array",
        "Float64Array",
        "BigInt64Array",
        "BigUint64Array",
        "Atomics",
        "Intl",
        "Temporal",
        "AggregateError",
        "FinalizationRegistry",
        "Iterator",
    ] {
        let placeholder = JsObject::new();
        placeholder.set("prototype", Value::Object(JsObject::new()));
        obj.set(name, Value::Object(placeholder));
    }
    obj
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
) -> Result<Option<Value>, VmError> {
    match name {
        // §20.1.3.2 Object.prototype.hasOwnProperty(V)
        // <https://tc39.es/ecma262/#sec-object.prototype.hasownproperty>
        "hasOwnProperty" => {
            let key = property_key_from_arg(args.first())?;
            let present = !matches!(obj.lookup_own(&key), object::PropertyLookup::Absent);
            Ok(Some(Value::Boolean(present)))
        }
        // §20.1.3.4 Object.prototype.propertyIsEnumerable(V)
        // <https://tc39.es/ecma262/#sec-object.prototype.propertyisenumerable>
        "propertyIsEnumerable" => {
            let key = property_key_from_arg(args.first())?;
            let result = match obj.lookup_own(&key) {
                object::PropertyLookup::Data { flags, .. } => flags.enumerable(),
                object::PropertyLookup::Accessor { flags, .. } => flags.enumerable(),
                object::PropertyLookup::Absent => false,
            };
            Ok(Some(Value::Boolean(result)))
        }
        // §20.1.3.3 Object.prototype.isPrototypeOf(V)
        // <https://tc39.es/ecma262/#sec-object.prototype.isprototypeof>
        "isPrototypeOf" => {
            let result = match args.first() {
                Some(Value::Object(other)) => other.has_in_proto_chain(obj),
                _ => false,
            };
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
            let recv_value = Value::Object(obj.clone());
            let has_error_shape = obj.get("name").is_some() || obj.get("message").is_some();
            let display = if has_error_shape {
                let rendered = error_classes::render_error_to_string(&recv_value);
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
        "valueOf" => Ok(Some(Value::Object(obj.clone()))),
        _ => Ok(None),
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

/// Drive an iterator one step. Returns `(value, done)`. Once an
/// iterator hands back `done = true`, its state transitions to
/// `Exhausted` so subsequent calls are stable no-ops (matches the
/// spec rule "an iterator never produces values after it has
/// produced `done: true`"; §7.4.2 step 6).
/// Build a fresh `Map` / `Set` / `WeakMap` / `WeakSet`, optionally
/// seeded from an iterable.
///
/// # Algorithm
/// 1. Match `kind` against the four collection names and allocate
///    the corresponding handle.
/// 2. If `seed` is `Value::Undefined` or `Value::Null`, return the
///    fresh empty handle (Spec §24.1.1.1 / §24.2.1.1 step 5 et al.).
/// 3. Otherwise the seed must be a `Value::Array` (foundation
///    relaxation: a real iterable protocol consultation lands when
///    user-defined iterables are wired); for `Map` / `WeakMap`
///    each element is a 2-element `[key, value]` array; for
///    `Set` / `WeakSet` each element is added directly.
///
/// # Errors
/// - [`VmError::TypeMismatch`] when the seed is non-iterable, when a
///   `Map` / `WeakMap` seed element is not a 2-array, or when a
///   `WeakMap` / `WeakSet` seed key is a primitive (the underlying
///   [`crate::collections::CollectionError::NonObjectKey`] surfaces
///   through this arm).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-map-constructor>
/// - <https://tc39.es/ecma262/#sec-set-constructor>
/// - <https://tc39.es/ecma262/#sec-weakmap-constructor>
/// - <https://tc39.es/ecma262/#sec-weakset-constructor>
fn build_collection(kind: &str, seed: &Value) -> Result<Value, VmError> {
    match kind {
        "Map" => {
            let m = JsMap::new();
            if seed_is_present(seed) {
                let entries = seed_array(seed)?;
                for entry in entries {
                    let pair = match entry {
                        Value::Array(a) => a,
                        _ => return Err(VmError::TypeMismatch),
                    };
                    if pair.len() < 2 {
                        return Err(VmError::TypeMismatch);
                    }
                    m.set(pair.get(0), pair.get(1));
                }
            }
            Ok(Value::Map(m))
        }
        "Set" => {
            let s = JsSet::new();
            if seed_is_present(seed) {
                for v in seed_array(seed)? {
                    s.add(v);
                }
            }
            Ok(Value::Set(s))
        }
        "WeakMap" => {
            let m = JsWeakMap::new();
            if seed_is_present(seed) {
                for entry in seed_array(seed)? {
                    let pair = match entry {
                        Value::Array(a) => a,
                        _ => return Err(VmError::TypeMismatch),
                    };
                    if pair.len() < 2 {
                        return Err(VmError::TypeMismatch);
                    }
                    m.set(pair.get(0), pair.get(1))
                        .map_err(|_| VmError::TypeMismatch)?;
                }
            }
            Ok(Value::WeakMap(m))
        }
        "WeakSet" => {
            let s = JsWeakSet::new();
            if seed_is_present(seed) {
                for v in seed_array(seed)? {
                    s.add(v).map_err(|_| VmError::TypeMismatch)?;
                }
            }
            Ok(Value::WeakSet(s))
        }
        _ => Err(VmError::UnknownIntrinsic {
            name: format!("new {kind}"),
        }),
    }
}

fn seed_is_present(v: &Value) -> bool {
    !matches!(v, Value::Undefined | Value::Null)
}

fn seed_array(seed: &Value) -> Result<Vec<Value>, VmError> {
    match seed {
        Value::Array(a) => Ok(a.borrow_body().iter().cloned().collect()),
        _ => Err(VmError::TypeMismatch),
    }
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
fn make_array_iterator_factory(array: JsArray) -> Value {
    native_value("Array[Symbol.iterator]", move |_, _| {
        let state = IteratorState::Array {
            array: array.clone(),
            index: 0,
        };
        Ok(Value::Iterator(std::rc::Rc::new(std::cell::RefCell::new(
            state,
        ))))
    })
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

/// Build an `IteratorResult { value, done }` plain object per
/// §7.4.6 `CreateIterResultObject`.
fn make_iter_result(value: Value, done: bool) -> Value {
    let obj = JsObject::new();
    obj.set("value", value);
    obj.set("done", Value::Boolean(done));
    Value::Object(obj)
}

/// Coerce a `new Proxy(target, ...)` first argument to a
/// [`JsObject`]. Plain objects pass through; callables (Function /
/// Closure / NativeFunction / BoundFunction / ClassConstructor)
/// are wrapped in a fresh JsObject that stashes the callable in a
/// hidden `__callable` slot so the apply / construct trap fallback
/// can re-invoke it through `run_callable_sync`.
fn coerce_proxy_target(arg: Option<&Value>) -> Result<Value, VmError> {
    match arg {
        Some(v @ Value::Object(_)) => Ok(v.clone()),
        Some(v) if abstract_ops::is_callable(v) => Ok(v.clone()),
        _ => Err(VmError::TypeMismatch),
    }
}

/// §28.2 Proxy static dispatcher. Empty name = `new Proxy(target,
/// handler)`; `"revocable"` = `Proxy.revocable(target, handler)`.
fn proxy_static_call(name: &str, args: &[Value]) -> Result<Value, VmError> {
    match name {
        // §28.2.1.1 — `new Proxy(target, handler)`. Target may be
        // any object — including callables — wrapped here in a
        // synthetic JsObject that carries the original value as
        // `[[ProxyTarget]]`. Foundation simplification: use a
        // dedicated `__callable` slot when the target is a
        // function so the apply trap's fallback can re-invoke it.
        "" => {
            let target = coerce_proxy_target(args.first())?;
            let handler = match args.get(1) {
                Some(Value::Object(o)) => o.clone(),
                _ => return Err(VmError::TypeMismatch),
            };
            Ok(Value::Proxy(crate::proxy::JsProxy::new(target, handler)))
        }
        // §28.2.2.1 — `Proxy.revocable(target, handler)` returns
        // `{proxy, revoke}`.
        "revocable" => {
            let target = coerce_proxy_target(args.first())?;
            let handler = match args.get(1) {
                Some(Value::Object(o)) => o.clone(),
                _ => return Err(VmError::TypeMismatch),
            };
            let proxy = crate::proxy::JsProxy::new(target, handler);
            let proxy_handle = proxy.clone();
            let revoke = native_function::native_value("revoke", move |_, _| {
                proxy_handle.revoke();
                Ok(Value::Undefined)
            });
            let obj = JsObject::new();
            obj.set("proxy", Value::Proxy(proxy));
            obj.set("revoke", revoke);
            Ok(Value::Object(obj))
        }
        other => Err(VmError::UnknownIntrinsic {
            name: format!("Proxy.{other}"),
        }),
    }
}

/// Iterator-helpers proposal §sec-iterator.from — coerce any
/// iterable / iterator-like value into a [`Value::Iterator`].
///
/// Foundation surface accepts `Array` / `String` / `Set` / `Map`
/// (via their dense iteration form) and existing
/// [`Value::Iterator`] handles directly. Non-iterable inputs raise
/// a `TypeMismatch` (surfaced upstream as a `TypeError`).
///
/// # See also
/// - <https://tc39.es/proposal-iterator-helpers/#sec-iterator.from>
fn iterator_static_call(name: &str, args: &[Value]) -> Result<Value, VmError> {
    match name {
        // Reserved spec form — the constructor itself isn't
        // user-callable.
        "" => Err(VmError::TypeMismatch),
        "from" => {
            let value = args.first().cloned().unwrap_or(Value::Undefined);
            let state = match value {
                Value::Iterator(rc) => return Ok(Value::Iterator(rc)),
                Value::Generator(handle) => IteratorState::Generator { handle },
                Value::Array(arr) => IteratorState::Array {
                    array: arr,
                    index: 0,
                },
                Value::String(s) => IteratorState::String {
                    string: s,
                    index: 0,
                },
                Value::Set(s) => {
                    let snap: SmallVec<[Value; 4]> = s.values().into_iter().collect();
                    IteratorState::Array {
                        array: JsArray::from_elements(snap),
                        index: 0,
                    }
                }
                Value::Map(m) => {
                    let entries: Vec<Value> = m
                        .entries()
                        .into_iter()
                        .map(|(k, v)| Value::Array(JsArray::from_elements([k, v])))
                        .collect();
                    IteratorState::Array {
                        array: JsArray::from_elements(entries),
                        index: 0,
                    }
                }
                Value::Object(_) => {
                    // Foundation: object-shaped iterables go through
                    // the user-iterator protocol; the value handed in
                    // is treated as the iterator object itself.
                    IteratorState::User { iterator: value }
                }
                _ => return Err(VmError::TypeMismatch),
            };
            Ok(Value::Iterator(std::rc::Rc::new(std::cell::RefCell::new(
                state,
            ))))
        }
        other => Err(VmError::UnknownIntrinsic {
            name: format!("Iterator.{other}"),
        }),
    }
}

/// Cloned snapshot of an [`IteratorState`] taken before driving a
/// helper callback so the borrow on `Rc<RefCell<IteratorState>>`
/// does not span the dispatch.
enum IteratorStateSnapshot {
    User(Value),
    Generator(crate::generator::JsGenerator),
    Map {
        source: std::rc::Rc<std::cell::RefCell<IteratorState>>,
        mapper: Value,
    },
    Filter {
        source: std::rc::Rc<std::cell::RefCell<IteratorState>>,
        predicate: Value,
    },
    Take {
        source: std::rc::Rc<std::cell::RefCell<IteratorState>>,
        remaining: u64,
    },
    Drop {
        source: std::rc::Rc<std::cell::RefCell<IteratorState>>,
        to_drop: u64,
    },
    FlatMap {
        source: std::rc::Rc<std::cell::RefCell<IteratorState>>,
        mapper: Value,
        inner: Option<std::rc::Rc<std::cell::RefCell<IteratorState>>>,
    },
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

fn step_iterator(
    iter: &std::rc::Rc<std::cell::RefCell<IteratorState>>,
    string_heap: &StringHeap,
) -> Result<(Value, bool), VmError> {
    let mut state = iter.borrow_mut();
    let outcome = match &mut *state {
        IteratorState::Array { array, index } => {
            if *index >= array.len() {
                None
            } else {
                let v = array.get(*index);
                *index += 1;
                Some(v)
            }
        }
        IteratorState::String { string, index } => {
            // §22.1.5.1 [`%StringIteratorPrototype%.next`](
            // https://tc39.es/ecma262/#sec-%25stringiteratorprototype%25.next
            // ) — yield code points. A valid surrogate pair (high +
            // low) combines into a single two-unit string; lone
            // surrogates are yielded as-is.
            if let Some(unit) = string.char_code_at(*index) {
                let next_unit = string.char_code_at(*index + 1);
                let is_pair = (0xD800..=0xDBFF).contains(&unit)
                    && matches!(next_unit, Some(low) if (0xDC00..=0xDFFF).contains(&low));
                let s = if is_pair {
                    let pair = [unit, next_unit.unwrap()];
                    *index += 2;
                    JsString::from_utf16_units(&pair, string_heap)?
                } else {
                    *index += 1;
                    JsString::from_utf16_units(&[unit], string_heap)?
                };
                Some(Value::String(s))
            } else {
                None
            }
        }
        IteratorState::User { .. }
        | IteratorState::Generator { .. }
        | IteratorState::Map { .. }
        | IteratorState::Filter { .. }
        | IteratorState::Take { .. }
        | IteratorState::Drop { .. }
        | IteratorState::FlatMap { .. } => {
            // The user-iterator + helper-wrapper protocols require
            // invoking JS callbacks on each step, which mutates the
            // dispatch stack. The synchronous helper cannot push
            // frames; the dispatcher must take the interpreter-aware
            // path (`Interpreter::iterator_next_full`) instead.
            return Err(VmError::TypeMismatch);
        }
        IteratorState::Exhausted => None,
    };
    match outcome {
        Some(value) => Ok((value, false)),
        None => {
            *state = IteratorState::Exhausted;
            Ok((Value::Undefined, true))
        }
    }
}

/// Look up the `prototype` own property of a callable so the
/// `Op::New` path can link the freshly allocated receiver. The
/// foundation supports only object-shaped prototypes: anything
/// else (or a missing `prototype`) leaves the receiver's chain
/// unset, matching `Object.create(null)` semantics. For
/// `Value::Function` (no own properties yet) we always fall back
/// to no prototype; closures created by the class lowering carry
/// `prototype` on the constructor's *own* property table by way
/// of the `BoundFunction` / object surface — but native bytecode
/// `Value::Function` values have no per-id property store, so
/// proto-linking only fires for closures whose function table id
/// has been augmented at class-build time. For the foundation
/// slice that distinction is invisible because the compiler always
/// installs `prototype` through a separate `StoreProperty` on a
/// constructor object reference (the constructor itself is held in
/// a register, with `prototype` set via `obj.prototype = …` style
/// dispatch only on the rare path).
fn construct_prototype(callee: &Value) -> Option<JsObject> {
    match callee {
        Value::ClassConstructor(c) => Some(c.prototype.clone()),
        Value::Object(obj) => match obj.get("prototype") {
            Some(Value::Object(p)) => Some(p),
            _ => None,
        },
        Value::BoundFunction(b) => construct_prototype(&b.target),
        _ => None,
    }
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
fn is_callable(value: &Value) -> bool {
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

/// Build a native callable that resumes a parked async frame when
/// invoked as a `then` reaction.
///
/// # Algorithm
/// 1. Take the parked frame out of the shared cell. If a sibling
///    reaction already consumed it (the spec lets only one of
///    `then`'s twin handlers fire), return `undefined` and exit.
/// 2. Enqueue a [`MicrotaskKind::AsyncResume`] microtask carrying
///    the boxed frame, the await's destination register, and the
///    fulfilled / rejected branch tag. The drain re-pushes the
///    frame onto a fresh stack and runs `dispatch_loop` from the
///    next pc on the next generation.
///
/// # Invariants
/// - The native handler MUST be idempotent. The shared cell
///   guarantees this: once the parked frame is taken, subsequent
///   invocations are no-ops.
fn make_async_resume_native(
    parked_slot: std::rc::Rc<std::cell::Cell<Option<Box<Frame>>>>,
    await_dst: u16,
    fulfilled: bool,
) -> Value {
    let label = if fulfilled {
        "async resume fulfill"
    } else {
        "async resume reject"
    };
    native_function::native_value(label, move |interp, args| {
        let Some(frame) = parked_slot.take() else {
            return Ok(Value::Undefined);
        };
        let value = args.first().cloned().unwrap_or(Value::Undefined);
        let mut task_args: SmallVec<[Value; 4]> = SmallVec::new();
        task_args.push(value);
        interp.microtasks.enqueue(Microtask {
            callee: Value::Undefined,
            this_value: Value::Undefined,
            args: task_args,
            result_capability: None,
            kind: MicrotaskKind::AsyncResume {
                frame,
                await_dst,
                fulfilled,
            },
        });
        Ok(Value::Undefined)
    })
}

/// Build a resume / reject native for `Op::Await` inside an
/// async-generator body. The closure runs as a promise reaction;
/// it enqueues a [`MicrotaskKind::AsyncGenResume`] task that the
/// drain rebinds back into the gen body with module access.
fn make_async_gen_resume_native(
    parked_slot: std::rc::Rc<std::cell::Cell<Option<Box<Frame>>>>,
    await_dst: u16,
    owner: crate::generator::JsGenerator,
    fulfilled: bool,
) -> Value {
    let label = if fulfilled {
        "async-gen await fulfill"
    } else {
        "async-gen await reject"
    };
    native_function::native_value(label, move |interp, args| {
        let Some(frame) = parked_slot.take() else {
            return Ok(Value::Undefined);
        };
        let value = args.first().cloned().unwrap_or(Value::Undefined);
        let mut task_args: SmallVec<[Value; 4]> = SmallVec::new();
        task_args.push(value);
        interp.microtasks.enqueue(Microtask {
            callee: Value::Undefined,
            this_value: Value::Undefined,
            args: task_args,
            result_capability: None,
            kind: MicrotaskKind::AsyncGenResume {
                frame,
                await_dst,
                fulfilled,
                owner: owner.clone(),
            },
        });
        Ok(Value::Undefined)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::{
        Function, Instruction, Op, Operand, SourceKind as BcSourceKind, SpanEntry,
    };

    fn module_with(code: Vec<Instruction>, scratch: u16) -> BytecodeModule {
        let spans: Vec<SpanEntry> = code
            .iter()
            .map(|i| SpanEntry {
                pc: i.pc,
                span: (0, 0),
            })
            .collect();
        BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![Function {
                id: 0,
                name: "<main>".to_string(),
                span: (0, 0),
                locals: 0,
                scratch,
                param_count: 0,
                own_upvalue_count: 0,
                is_arrow: false,
                has_rest: false,
                is_async: false,
                is_generator: false,
                is_async_generator: false,
                is_module: false,
                needs_arguments: false,
                module_url: String::new(),
                code,
                spans,
            }],
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
                    operands: vec![Operand::Register(0)],
                },
                Instruction {
                    pc: 1,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)],
                },
            ],
            1,
        );
        let mut interp = Interpreter::new();
        assert_eq!(interp.run(&module).unwrap(), Value::Undefined);
    }

    #[test]
    fn missing_return_errors() {
        let module = module_with(
            vec![Instruction {
                pc: 0,
                op: Op::Nop,
                operands: vec![],
            }],
            0,
        );
        let mut interp = Interpreter::new();
        assert_eq!(
            interp.run(&module).unwrap_err().error,
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
            is_arrow: false,
            has_rest: false,
            is_async: false,
            is_generator: false,
            is_async_generator: false,
            is_module: false,
            needs_arguments: false,
            module_url: String::new(),
            code: vec![Instruction {
                pc: 0,
                op: Op::ReturnUndefined,
                operands: vec![],
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
            is_arrow: false,
            has_rest: false,
            is_async: false,
            is_generator: false,
            is_async_generator: false,
            is_module: false,
            needs_arguments: false,
            module_url: String::new(),
            code: vec![Instruction {
                pc: 0,
                op: Op::ReturnUndefined,
                operands: vec![],
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
            upvalues: std::rc::Rc::from(Vec::new()),
            bound_this: None,
        }));
        let bound = std::rc::Rc::new(BoundFunction {
            target: Value::Function { function_id: 7 },
            bound_this: Value::Undefined,
            bound_args: SmallVec::new(),
        });
        assert!(is_callable(&Value::BoundFunction(bound)));
        assert!(!is_callable(&Value::Number(NumberValue::Smi(1))));
        assert!(!is_callable(&Value::Object(JsObject::new())));
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
        use std::rc::Rc;
        let main = Function {
            id: 0,
            name: "<main>".to_string(),
            span: (0, 0),
            locals: 0,
            scratch: 1,
            param_count: 0,
            own_upvalue_count: 0,
            is_arrow: false,
            has_rest: false,
            is_async: false,
            is_generator: false,
            is_async_generator: false,
            is_module: false,
            needs_arguments: false,
            module_url: String::new(),
            code: vec![Instruction {
                pc: 0,
                op: Op::ReturnUndefined,
                operands: vec![],
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
            is_arrow: true,
            has_rest: false,
            is_async: false,
            is_generator: false,
            is_async_generator: false,
            is_module: false,
            needs_arguments: false,
            module_url: String::new(),
            code: vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadThis,
                    operands: vec![Operand::Register(0)],
                },
                Instruction {
                    pc: 1,
                    op: Op::ReturnValue,
                    operands: vec![Operand::Register(0)],
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
            upvalues: Rc::from(Vec::new()),
            bound_this: Some(Box::new(Value::String(bound.clone()))),
        };
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(Frame::for_function(&module.functions[0]));
        // Reserve a scratch slot in <main> to receive the result.
        stack[0].registers.push(Value::Undefined);
        // Caller-supplied this is `Null` — the closure must override.
        interp
            .invoke(
                &mut stack,
                &module,
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
                    operands: vec![],
                },
                Instruction {
                    pc: 1,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)],
                },
            ],
            1,
        );
        let mut interp = Interpreter::new();
        let handle = interp.interrupt_handle();
        handle.interrupt();
        assert_eq!(interp.run(&module).unwrap_err().error, VmError::Interrupted);
    }
}
