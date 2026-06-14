//! Interpreter and value model for the Otter engine.
//!
//! # Contents
//! - [`Value`] — opaque NaN-boxed runtime value.
//! - [`Frame`] — compact call frame.
//! - [`Interpreter`] — match-based dispatch loop over the frozen
//!   executable view inside [`ExecutionContext`].
//! - [`InterruptFlag`] — atomic flag observed at back-edges; cheap.
//! - [`VmError`] — runtime errors the interpreter can raise.
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

// The otter-themed macros (`holt!`, `couch!`, `lodge!`) emit code
// against the absolute `::otter_vm::*` path so consumers outside
// this crate can call them without remembering to import every
// type. Inside `otter-vm` itself, that path normally fails to
// resolve; this self-alias makes `::otter_vm::Foo` mean the same
// thing as `crate::Foo` so the macro-generated install bodies
// compile in both contexts.
extern crate self as otter_vm;

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
pub mod boolean;
mod call_ops;
pub mod closure;
mod code_space;
mod coerce;
pub mod cold_frame;
mod collection_ops;
pub mod collections;
pub mod collections_prototype;
pub mod console;
mod constant_ops;
mod conversion;
pub mod date;
pub mod eval_env;
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
pub mod bound_function;
pub mod class_constructor;
pub mod dynamic_import;
pub mod error_classes;
mod error_ops;
mod eval_ops;
mod executable;
pub mod execution_context;
mod frame_ops;
mod frame_state;
mod function_kind;
pub mod function_metadata;
mod function_ops;
pub mod function_prototype;
pub mod gc_trace;
pub mod generator;
pub mod global_functions;
mod global_ops;
pub mod groom;
pub mod inspect;
pub mod intl;
mod intl_ops;
pub mod intrinsic_install;
pub mod intrinsics;
mod iterator_ops;
pub mod iterator_state;
pub mod jit;
pub mod js_surface;
pub mod json;
pub mod math;
mod method_ops;
pub mod microtask;
mod module_ops;
mod module_records;
pub mod native_function;
pub mod number;
pub mod object;
mod object_internal_ops;
pub mod object_statics;
mod operand_decode;
pub mod pelt;
pub mod promise;
pub mod promise_dispatch;
mod promise_ops;
mod property_atom;
mod property_dispatch;
mod property_ic;
pub mod proxy;
pub mod realm_intrinsics;
pub mod reflect;
pub mod regexp;
pub mod regexp_prototype;
pub mod run_control;
pub mod runtime_budget;
pub mod runtime_cx;
pub mod runtime_state;
mod static_call_ops;
mod static_load_ops;
pub mod string;
pub mod swar;
pub mod symbol;
pub mod symbol_dispatch;
pub mod symbol_prototype;
pub mod temporal;
pub mod timers;
pub mod uint8_base64;
pub mod upvalue;
pub mod value;
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
    DEFAULT_MAX_STACK_DEPTH, DEFAULT_MAX_SYNC_REENTRY_DEPTH, InterruptFlag, NO_HANDLER_OFFSET,
    RunError, StackFrameSnapshot, VmError,
};

use otter_bytecode::{ArgumentBindingStorage, ArgumentsObjectKind, BytecodeModule, Op};
use smallvec::SmallVec;

use arithmetic_dispatch::{
    bigint_and_op, bigint_mul_op, bigint_or_op, bigint_sub_op, bigint_xor_op,
};
pub(crate) use error_ops::{
    native_to_vm_error, snapshot_frames, symbol_to_vm_error, vm_err_to_value,
};
use executable::ExecutableFunction;
use operand_decode::{apply_branch, register_operand};

pub use array::JsArray;
pub use closure::{
    JS_CLOSURE_BODY_TYPE_TAG, JsClosure, JsClosureBody, alloc_closure, alloc_closure_with_roots,
};
pub use collections::{CollectionError, JsMap, JsSet, JsWeakMap, JsWeakSet, MapKey};
pub use console::{ConsoleLevel, ConsoleSink, ConsoleSinkHandle, StdConsoleSink};
pub use dynamic_import::{DynamicImportLoader, DynamicImportLoaderHandle, DynamicImportRegistry};
pub use error_classes::{ErrorClassRegistry, ErrorKind};
pub use intl::{IntlKind, IntlPayload, JsIntl};
pub use jit::{
    JitCompileError, JitCompileRequest, JitCompileStatus, JitCompilerHook, JitExecOutcome,
    JitFrameStack, JitFunctionCode, JitFunctionView, JitInstrView, JitReentryPtrs,
};
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
pub use string::{JsString, MAX_ROPE_DEPTH};
pub use symbol::{JsSymbol, SymbolBody, SymbolRegistry, WellKnown, WellKnownSymbols};
pub use temporal::{JsTemporal, TemporalKind, TemporalPayload};
pub use timers::{TimerCallbacks, TimerEntry, TimerScheduler, TimerSchedulerHandle};
pub use weak_refs::{JsFinalizationRegistry, JsWeakRef};

// Eight-byte tagged value. Canonical `Value` export.
pub use value::{Value, ValueKind};

pub(crate) use bound_function::BoundFunctionMetadataProperty;
pub use bound_function::{BOUND_FUNCTION_BODY_TYPE_TAG, BoundFunction, BoundFunctionBody};
pub use class_constructor::{
    CLASS_CONSTRUCTOR_BODY_TYPE_TAG, ClassConstructor, ClassConstructorBody,
};
pub use iterator_state::{
    ArrayIterKind, BuiltinIteratorOrigin, ITERATOR_STATE_TYPE_TAG, IteratorHandle, IteratorState,
    MapIteratorKind, SetIteratorKind,
};
pub use upvalue::{
    UPVALUE_CELL_TYPE_TAG, UpvalueCell, UpvalueCellBody, alloc_upvalue, read_upvalue, store_upvalue,
};

pub use runtime_budget::{RuntimeBudget, RuntimeBudgetExceededAction, RuntimeBudgetStats};
pub use runtime_cx::{NativeCallInfo, NativeCtx};

use runtime_budget::RuntimeHeapSnapshot;

use otter_gc::raw::RawGc;

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

/// Key form for a `super.name` / `super[expr]` read. `Resolved`
/// carries an already-interned named key; `Computed` carries the raw
/// key value so `ToPropertyKey` runs *after* `GetSuperBase` per
/// §13.3.7.1.
pub(crate) enum SuperReadKey<'a> {
    Resolved(VmPropertyKey<'a>),
    Computed(Value),
}

/// Map an [`otter_gc::OutOfMemory`] from a GC body allocation into the
/// runtime-shaped [`VmError::OutOfMemory`]. Used by every value-model
/// helper that surfaces an allocation failure to the dispatcher.
#[must_use]
pub fn oom_to_vm(err: otter_gc::OutOfMemory) -> VmError {
    VmError::OutOfMemory {
        requested_bytes: err.requested_bytes(),
        heap_limit_bytes: err.heap_limit_bytes(),
    }
}

/// Match-based dispatch loop. The harness baseline; slice tasks may
/// later switch to threaded dispatch after benchmark-driven review
/// (foundation plan §"Interpreter requirements").
pub struct Interpreter {
    /// §13.2.8.4 GetTemplateObject realm cache — one frozen
    /// template-strings object per tagged-template site, keyed by
    /// `(chunk function_base, site index)`.
    template_objects: rustc_hash::FxHashMap<(u32, u32), Value>,
    /// Scratch GC-root stack for native recursive algorithms (today:
    /// `JSON.stringify`'s spec serializer) that must hold live `Value`s
    /// across calls which allocate — e.g. key-name `JsString`s minted by
    /// `[[OwnPropertyKeys]]`, user getters, `toJSON`, or a replacer. Each
    /// entry is a stable slot the collector rewrites on a move, so the
    /// serializer re-reads its container from the slot after every
    /// allocating sub-call instead of dereferencing a stale copy. Traced
    /// in [`crate::runtime_state::RuntimeState::trace_roots`]; pushed/read
    /// back/popped as a strict stack so it is empty between top-level
    /// native calls.
    json_root_stack: Vec<Value>,
    /// Protector for the array element store fast path: flips to
    /// `true` once any accessor descriptor lands on an array-index
    /// key anywhere (e.g. `Array.prototype[1] = {set}`); array index
    /// writes then re-check the prototype chain per OrdinarySet
    /// before creating an own element. Stays `false` for the
    /// overwhelmingly common unpolluted heap, keeping appends cheap.
    array_index_accessor_protector: bool,
    interrupt: InterruptFlag,
    /// Byte length of the instruction currently being dispatched. Set
    /// by `dispatch_loop_inner` right after each fetch and consumed by
    /// every `frame.advance_pc(self.current_byte_len)?` call along
    /// the dispatch path. Centralises the PC advance so opcode helpers
    /// stay byte-length agnostic.
    current_byte_len: u32,
    /// Per-isolate GC heap. Owned here so allocator-bearing
    /// opcodes (e.g. `Op::MakeClosure`'s upvalue alloc since
    /// task 76) reach it through `&mut self`. The `Runtime`
    /// layer delegates `gc_heap` / `heap_stats` /
    /// `heap_snapshot` / `force_gc` accessors here.
    gc_heap: otter_gc::GcHeap,
    /// Registry of every linked code chunk (entry scripts, module
    /// graphs, `eval` / `new Function` bodies, dynamic-import
    /// fragments). Function ids are global across chunks, so a
    /// function value escaping its chunk stays callable from any
    /// frame; every linked [`ExecutionContext`] resolves foreign ids
    /// through this shared registry.
    code_space: std::sync::Arc<code_space::CodeSpace>,
    /// Interpreter-owned hidden-class side tables for GC-managed shapes.
    /// Runtime object storage uses the root, interned shape keys, and
    /// transition/cache tables here.
    shape_runtime: object::ShapeRuntime,
    max_stack_depth: u32,
    sync_reentry_depth: u32,
    allow_blocking_atomics_wait: bool,
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
    module_environments: std::collections::HashMap<std::sync::Arc<str>, JsObject>,
    /// Per-module persistent `<module-init>` own-upvalue cells — the
    /// engine's module environment record. The link-phase (hoist) and
    /// evaluation-phase invocations of one module's init share these
    /// cells so closures instantiated at link time observe the
    /// bindings the body later initialises.
    module_init_upvalues: std::collections::HashMap<std::sync::Arc<str>, Box<[crate::UpvalueCell]>>,
    /// §9.1.1.4 GlobalEnvironmentRecord declarative record — script
    /// top-level `let` / `const` / `class` bindings, shared across
    /// every script and eval chunk in the realm and *not* reflected
    /// as global object properties. Cells start as the TDZ hole.
    pub(crate) global_lexicals:
        rustc_hash::FxHashMap<Box<str>, (crate::UpvalueCell, /* is_const */ bool)>,
    /// Depth of active §16.2.1.4 Evaluate calls. Dynamic imports that
    /// land while this is non-zero defer their target's evaluation to
    /// a host job so they cannot preempt the running DFS.
    pub(crate) module_evaluation_depth: u32,
    /// Modules whose link-phase (function-hoisting) init pass already
    /// ran. Cleared with the rest of the module state.
    module_hoisted: std::collections::HashSet<std::sync::Arc<str>>,
    /// Cached `(referrer, specifier) → target` lookup, built
    /// lazily from [`otter_bytecode::BytecodeModule::module_resolutions`]
    /// the first time the running module is observed. Cleared
    /// alongside `module_environments`.
    module_resolution_cache:
        std::collections::HashMap<(std::sync::Arc<str>, String), std::sync::Arc<str>>,
    /// Per-module Cyclic Module Record evaluation state (§16.2.1.4):
    /// `[[Status]]`, `[[EvaluationError]]`, and the
    /// `[[TopLevelCapability]]`-shaped promise gate. Promise and error
    /// values are traced as GC roots. Cleared with
    /// `module_environments`.
    module_records:
        std::collections::HashMap<std::sync::Arc<str>, module_records::ModuleRecordState>,
    /// Monotonic `[[AsyncEvaluationOrder]]` source (§16.2.1.4); next
    /// value handed to a module entering async evaluation.
    next_module_async_order: u64,
    /// Cache of deferred module namespace exotic objects, keyed by
    /// target module URL, so two `import defer * as` of the same module
    /// yield the identical object (§16.2.1). Cleared with
    /// `module_environments`.
    deferred_namespaces: std::collections::HashMap<std::sync::Arc<str>, JsObject>,
    /// Cache of eager Module Namespace Exotic Objects (§10.4.6), keyed
    /// by target module URL, so every `import * as ns` / `export * as
    /// ns` of the same module yields the identical object. Cleared with
    /// `module_environments`.
    module_namespaces: std::collections::HashMap<std::sync::Arc<str>, JsObject>,
    /// Per-run §16.2.1.6 ResolveExport tables: importing module URL →
    /// (exported name → `(defining_module, binding)`). Populated by the
    /// runtime from each module's
    /// [`otter_compiler::CompiledModuleMetadata::resolved_exports`].
    /// The Module Namespace Exotic Object reads
    /// ([`Op::LoadImportBinding`], `[[Get]]`, `[[OwnPropertyKeys]]`, …)
    /// consult this so re-exported and star-exported names read the
    /// defining module's live environment. `binding == "*namespace*"`
    /// resolves to `defining_module`'s namespace object. Cleared with
    /// `module_environments`.
    module_resolved_exports: std::collections::HashMap<
        std::sync::Arc<str>,
        std::collections::BTreeMap<String, (std::sync::Arc<str>, String)>,
    >,
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
    /// Runtime-installed baseline JIT compiler hook. The hook lives behind a VM
    /// trait object so `otter-vm` never depends on executable-memory code.
    /// `Some` is also the tier-up gate: with no hook installed, all tier-up
    /// bookkeeping below stays untouched and execution is interpreter-only.
    jit_hook: Option<std::sync::Arc<dyn jit::JitCompilerHook>>,
    /// Per-function call counter driving function-entry tier-up. Only mutated
    /// when a JIT hook is installed.
    jit_call_counts: rustc_hash::FxHashMap<u32, u32>,
    /// Compiled-code cache keyed by global function id. `Some(code)` is an
    /// installed baseline body; `None` records a function the emitter could not
    /// compile (outside the supported subset), so it is never retried.
    jit_code: std::collections::BTreeMap<u32, Option<std::sync::Arc<dyn jit::JitFunctionCode>>>,
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
    /// Iteration-anchor stack: GC roots for in-flight iterator
    /// drains. `iterator_to_list_sync` and similar helpers push the
    /// iterator + next-method handles here before each
    /// `IteratorStep` call so a GC triggered inside the user's
    /// `next` body cannot reclaim them. Frames pop their entries on
    /// the way out, matching the LIFO call shape. Traced by
    /// [`RuntimeState::trace_roots`].
    iteration_anchors: Vec<Value>,
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
    /// Per ordinary-function `[[Prototype]]` overrides. Used by
    /// `CreateDynamicFunction` when subclassing `%Function%`:
    /// `new Subclass extends Function` returns a fresh ordinary
    /// function whose internal prototype is `new.target.prototype`.
    function_prototype_overrides: std::collections::HashMap<u32, Value>,
    /// Function ids whose ordinary function object has had
    /// `[[PreventExtensions]]` applied. Kept separate from the
    /// lazy user bag so materialising spec-existing virtual
    /// properties such as `prototype` remains valid after
    /// `preventExtensions`.
    function_non_extensible: std::collections::HashSet<u32>,
    /// Deleted virtual `name` / `length` own properties for ordinary
    /// bytecode functions. Stored separately from the user bag so
    /// deleting built-in function metadata does not resurrect the
    /// intrinsic fallback on later reads.
    function_deleted_metadata: std::collections::HashSet<(u32, &'static str)>,
    /// Per-instance `[[Prototype]]` overrides for object-shaped
    /// exotics whose payloads are still `Rc` / `Arc` backed rather
    /// than GC-managed. Values stored here are traced from the
    /// interpreter root set, which keeps subclass prototypes safe
    /// without embedding untraced `Value` slots in non-GC bodies.
    non_gc_exotic_prototype_overrides: std::collections::HashMap<usize, Value>,
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
    /// Per-kind iterator prototypes — `%ArrayIteratorPrototype%`,
    /// `%MapIteratorPrototype%`, `%SetIteratorPrototype%`,
    /// `%StringIteratorPrototype%`, and
    /// `%RegExpStringIteratorPrototype%`. Each inherits from
    /// `%IteratorPrototype%` and carries its own `@@toStringTag`.
    /// Populated by bootstrap; consulted by
    /// `intrinsic_prototype_object_for` (iterator family) to
    /// route `[[GetPrototypeOf]]` per ECMA-262
    /// §22.1.5 / §23.1.5 / §24.1.5 / §24.2.5.
    array_iterator_prototype: Option<JsObject>,
    map_iterator_prototype: Option<JsObject>,
    set_iterator_prototype: Option<JsObject>,
    string_iterator_prototype: Option<JsObject>,
    regexp_string_iterator_prototype: Option<JsObject>,
    iterator_helper_prototype: Option<JsObject>,
    wrap_for_valid_iterator_prototype: Option<JsObject>,
    function_kind_prototypes: function_kind::FunctionKindPrototypes,
    /// Pool of cold-frame side records (try handlers, async parking,
    /// in-flight ToPrimitive/bind/iterator ladders, module URL, …).
    /// Hot [`crate::Frame`] carries an `Option<ColdFrameIdx>` and
    /// acquires a slot the first time an opcode needs cold state.
    /// See [`crate::cold_frame`] for the contract.
    cold_frames: cold_frame::ColdFramePool,
    /// Resolved per-realm intrinsic handles. Populated at the end of
    /// `build_global_this_impl`; runtime lookups consult these before
    /// falling back to the string-name path. See
    /// [`crate::realm_intrinsics::RealmIntrinsics`].
    realm_intrinsics: realm_intrinsics::RealmIntrinsics,
    /// Per-isolate cache of compiled regex programs, keyed by pattern +
    /// engine-relevant flags. Re-evaluating the same regex literal (or
    /// `new RegExp` over a repeated pattern) reuses the lowered program
    /// instead of re-parsing it. See [`crate::regexp::RegexCompileCache`].
    regex_compile_cache: regexp::RegexCompileCache,
    /// Optional step-trace observer. When `Some`, the dispatch loop
    /// emits one [`inspect::StepEvent`] per instruction. When `None`,
    /// the hot path pays a single `Option` discriminant check and
    /// branches around the observer with no further work. See
    /// [`crate::inspect`] for the format contract.
    tracer: Option<Box<dyn inspect::StepTracer>>,
}

impl std::fmt::Debug for Interpreter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Interpreter")
            .field("max_stack_depth", &self.max_stack_depth)
            .field("eval_hook_installed", &self.eval_hook.is_some())
            .field("jit_hook_installed", &self.jit_hook.is_some())
            .finish_non_exhaustive()
    }
}

impl Drop for Interpreter {
    fn drop(&mut self) {
        // Shape side tables store raw compressed GC handles outside the heap.
        // Clear them before `gc_heap` drops so stale offsets cannot outlive the
        // collector they came from.
        self.shape_runtime.clear();
    }
}

impl Interpreter {
    /// Root-tracing view of the template-object cache.
    pub(crate) fn template_objects_for_trace(&self) -> impl Iterator<Item = &Value> {
        self.template_objects.values()
    }

    /// Root-tracing view of the native serializer scratch root stack.
    pub(crate) fn json_root_stack_for_trace(&self) -> impl Iterator<Item = &Value> {
        self.json_root_stack.iter()
    }

    /// Push `value` onto the native serializer scratch root stack,
    /// returning its stable index. The collector traces and rewrites
    /// the slot on a move, so the caller re-reads the relocated value
    /// via [`Self::json_root_get`] after any allocating sub-call.
    pub(crate) fn json_root_push(&mut self, value: Value) -> usize {
        let idx = self.json_root_stack.len();
        self.json_root_stack.push(value);
        idx
    }

    /// Read the (possibly relocated) value parked at `idx`.
    pub(crate) fn json_root_get(&self, idx: usize) -> Value {
        self.json_root_stack[idx]
    }

    /// Overwrite the parked value at `idx` (the serializer reassigns
    /// `value` as `toJSON` / the replacer / wrapper unwrapping run).
    pub(crate) fn json_root_set(&mut self, idx: usize, value: Value) {
        self.json_root_stack[idx] = value;
    }

    /// Pop the scratch root stack back down to `idx` (the value that
    /// [`Self::json_root_push`] returned), restoring the strict-stack
    /// discipline after a serializer frame returns.
    pub(crate) fn json_root_pop_to(&mut self, idx: usize) {
        self.json_root_stack.truncate(idx);
    }

    /// §13.2.8.4 GetTemplateObject steps 7-15 — build the frozen
    /// template-strings array with its frozen, non-enumerable `.raw`
    /// companion.
    fn build_template_object(
        &mut self,
        context: &ExecutionContext,
        stack: &SmallVec<[Frame; 8]>,
        site_idx: u32,
    ) -> Result<Value, VmError> {
        let site = context
            .template_site(site_idx)
            .ok_or(VmError::InvalidOperand)?
            .clone();
        let mut cooked: Vec<Value> = Vec::with_capacity(site.cooked.len());
        for entry in &site.cooked {
            match entry {
                Some(text) => {
                    let s = JsString::from_str(text, &mut self.gc_heap)?;
                    cooked.push(Value::string(s));
                }
                None => cooked.push(Value::undefined()),
            }
        }
        let mut raw: Vec<Value> = Vec::with_capacity(site.raw.len());
        for text in &site.raw {
            let s = JsString::from_str(text, &mut self.gc_heap)?;
            raw.push(Value::string(s));
        }
        let roots = self.collect_allocation_roots(stack);
        let raw_value;
        {
            let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                for &slot in &roots {
                    visitor(slot);
                }
                for v in cooked.iter().chain(raw.iter()) {
                    v.trace_value_slots(visitor);
                }
            };
            let raw_arr = crate::array::from_elements_with_roots(
                &mut self.gc_heap,
                raw.iter().copied(),
                &mut visit,
            )
            .map_err(crate::oom_to_vm)?;
            raw_value = Value::array(raw_arr);
        }
        let strings_arr;
        {
            let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                for &slot in &roots {
                    visitor(slot);
                }
                for v in cooked.iter().chain(std::iter::once(&raw_value)) {
                    v.trace_value_slots(visitor);
                }
            };
            strings_arr = crate::array::from_elements_with_roots(
                &mut self.gc_heap,
                cooked.iter().copied(),
                &mut visit,
            )
            .map_err(crate::oom_to_vm)?;
        }
        crate::array::set_named_property(strings_arr, &mut self.gc_heap, "raw", raw_value)
            .map_err(|_| VmError::TypeMismatch)?;
        // §13.2.8.4 steps 10-14 — `.raw` is non-enumerable and both
        // arrays are frozen.
        crate::array::set_named_property_flags(
            strings_arr,
            &mut self.gc_heap,
            "raw",
            object::PropertyFlags::new(false, false, false),
        );
        if let Some(raw_arr) = raw_value.as_array() {
            crate::array::set_integrity_level(raw_arr, &mut self.gc_heap, true);
        }
        crate::array::set_integrity_level(strings_arr, &mut self.gc_heap, true);
        Ok(Value::array(strings_arr))
    }
}

impl otter_gc::ExtraRootSource for Interpreter {
    fn visit_extra_roots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        crate::runtime_state::RuntimeState::new(self).trace_roots(visitor);
    }
}

pub(crate) fn trace_active_frame_roots(
    stack: &SmallVec<[Frame; 8]>,
    pool: &cold_frame::ColdFramePool,
    visitor: &mut dyn FnMut(*mut RawGc),
) {
    for frame in stack {
        frame.trace_frame_slots(visitor);
        if let Some(idx) = frame.cold {
            pool.get(idx).trace_cold_slots(visitor);
        }
    }
}

/// Compile-time options for dynamic source text.
#[derive(Debug, Clone, Default)]
pub struct EvalCompileOptions {
    /// `true` for direct eval executed from strict code. The
    /// compiler stores the resulting strict bit on `<main>` so
    /// nested functions inherit it normally.
    pub force_strict: bool,
    /// `true` for a direct eval whose calling variable environment
    /// binds `arguments` (ordinary functions always; arrows only when
    /// they bind the name themselves). §19.2.1.3 then makes a sloppy
    /// body var-declaring `arguments` an early SyntaxError.
    pub forbid_var_arguments: bool,
    /// §19.2.1.3 EvalDeclarationInstantiation — the caller variable
    /// environment of a direct eval running inside a function. Entry
    /// `i` corresponds to the upvalue cell the runtime splices into
    /// slot `i` of the compiled `<main>`'s frame; the compiler binds
    /// each name to that slot. `None` for indirect eval and for
    /// direct eval at script top level (global environment).
    pub caller_scope: Option<Vec<EvalCallerBinding>>,
    /// `true` to compile the source as *script global code*
    /// (§16.1.7 GlobalDeclarationInstantiation — non-configurable
    /// global var bindings) instead of eval code. Used by host hooks
    /// such as `$262.evalScript` that execute a full Script in the
    /// current realm.
    pub script_goal: bool,
    /// §19.2.1.1 step 5 — `true` when the direct-eval call site sits
    /// inside non-arrow function code (or a class field
    /// initializer), making `new.target` legal in the eval body.
    pub new_target_allowed: bool,
    /// §15.7.1 ContainsArguments through §19.2.1.1 — `true` when the
    /// direct-eval call site sits inside a class field initializer:
    /// a free `arguments` reference in the eval body is an early
    /// SyntaxError.
    pub in_class_field_initializer: bool,
    /// §19.2.1.1 step 5 — the direct-eval call site carries a
    /// [[HomeObject]] (method / field initializer), making
    /// `super.x` legal in the eval body.
    pub super_property_allowed: bool,
}

/// One caller-environment binding visible to a direct eval body.
#[derive(Debug, Clone)]
pub struct EvalCallerBinding {
    /// Source-level binding name.
    pub name: String,
    /// `true` for `let` / `const` / `class` caller bindings — a
    /// sloppy eval body var-declaring the same name is a runtime
    /// `SyntaxError` (§19.2.1.3 step 5).
    pub lexical: bool,
    /// Passthrough capture from a function enclosing the caller —
    /// readable, but a `var` of the same name in the eval body
    /// declares a fresh caller binding (§19.2.1.3).
    pub captured: bool,
    /// `true` for a `const` / `class` caller binding — an eval-body
    /// assignment throws `TypeError` in every mode (§13.3.1).
    pub is_const: bool,
    /// `true` for a named function expression's self-name binding —
    /// an eval-body assignment throws `TypeError` in strict mode only
    /// (§10.2.11, §9.1.1.1.5).
    pub fn_self_name: bool,
}

/// Embedder-supplied parse + compile callback used by
/// [`Op::Eval`] / [`Op::NewFunction`]. Returns a freshly linked
/// [`BytecodeModule`] whose `<main>` completion value becomes the
/// dispatch result.
pub type EvalHook = std::sync::Arc<
    dyn Fn(&str, EvalCompileOptions) -> Result<BytecodeModule, String> + Send + Sync,
>;

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

/// Read `globalThis.<name>.prototype` regardless of whether the
/// global binding stores the constructor as a plain [`JsObject`] (the
/// shape used by hand-rolled bootstrap installers like
/// [`error_classes`]) or as a `Value::NativeFunction` (the shape
/// produced by `couch!` for Function / Object / Array / etc.).
fn resolve_ctor_prototype(
    heap: &mut otter_gc::GcHeap,
    global_this: JsObject,
    name: &str,
) -> Option<JsObject> {
    let ctor_value = object::get(global_this, heap, name)?;
    if let Some(ctor_obj) = ctor_value.as_object() {
        return object::get(ctor_obj, heap, "prototype").and_then(|v| v.as_object());
    }
    if let Some(ctor) = ctor_value.as_native_function() {
        return ctor
            .own_property_descriptor(heap, "prototype")
            .ok()
            .flatten()
            .and_then(|d| match d.kind {
                object::DescriptorKind::Data { value } => value.as_object(),
                _ => None,
            });
    }
    None
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
        let mut gc_heap = otter_gc::GcHeap::with_max_heap_bytes(cap_bytes)
            .expect("GcHeap construction never fails on the default cage");
        startup_timer.mark("vm_gc_heap");
        let well_known_symbols = WellKnownSymbols::new(&mut gc_heap)
            .expect("well-known symbol descriptions + bodies fit within any positive cap");
        startup_timer.mark("vm_well_known_symbols");
        let error_classes = ErrorClassRegistry::new(&mut gc_heap)
            .expect("error class prototypes fit within any positive cap");
        startup_timer.mark("vm_error_classes");
        let global_this = bootstrap::build_global_this(&mut gc_heap, &well_known_symbols)
            .expect("global_this fits within any positive cap");
        startup_timer.mark("vm_global_this");
        // §20.4.2 — install well-known symbols on the realm's
        // `Symbol` constructor + `Symbol.prototype[@@toPrimitive]`.
        // Bootstrap allocates the ctor + prototype objects; this
        // hook attaches the per-realm singleton symbols once
        // `WellKnownSymbols` exists.
        crate::intrinsics::symbol::install_symbol_well_knowns_post_bootstrap(
            &mut gc_heap,
            global_this,
            &well_known_symbols,
        )
        .expect("Symbol well-known properties fit within any positive cap");
        // §20.2.3.6 — install `Function.prototype[@@hasInstance]`.
        // Bootstrap can't see `WellKnownSymbols`, so we wire the
        // realm-local @@hasInstance after both Function.prototype
        // and the symbol table exist.
        let function_prototype_handle = if let Some(function_proto) =
            resolve_ctor_prototype(&mut gc_heap, global_this, "Function")
        {
            let has_instance = well_known_symbols.get(symbol::WellKnown::HasInstance);
            let global_root = Value::object(global_this);
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
            && let Some(object_prototype) =
                resolve_ctor_prototype(&mut gc_heap, global_this, "Object")
        {
            error_classes.finalize_after_bootstrap(
                &mut gc_heap,
                function_prototype,
                object_prototype,
                global_this,
            );
        }
        let shape_runtime = object::ShapeRuntime::new(&mut gc_heap)
            .expect("shape root fits within any positive cap");
        startup_timer.mark("vm_shape_runtime");
        let mut interp = Self {
            template_objects: rustc_hash::FxHashMap::default(),
            json_root_stack: Vec::new(),
            array_index_accessor_protector: false,
            interrupt: InterruptFlag::new(),
            current_byte_len: 1,
            gc_heap,
            code_space: std::sync::Arc::new(code_space::CodeSpace::default()),
            shape_runtime,
            max_stack_depth: DEFAULT_MAX_STACK_DEPTH,
            sync_reentry_depth: 0,
            allow_blocking_atomics_wait: false,
            microtasks: MicrotaskQueue::new(),
            module_environments: std::collections::HashMap::new(),
            module_init_upvalues: std::collections::HashMap::new(),
            global_lexicals: rustc_hash::FxHashMap::default(),
            module_hoisted: std::collections::HashSet::new(),
            module_evaluation_depth: 0,
            module_resolution_cache: std::collections::HashMap::new(),
            module_records: std::collections::HashMap::new(),
            next_module_async_order: 0,
            deferred_namespaces: std::collections::HashMap::new(),
            module_namespaces: std::collections::HashMap::new(),
            module_resolved_exports: std::collections::HashMap::new(),
            load_property_ics: Vec::new(),
            store_property_ics: Vec::new(),
            has_property_ics: Vec::new(),
            property_ic_stats: property_ic::PropertyIcStats::default(),
            jit_hook: None,
            jit_call_counts: rustc_hash::FxHashMap::default(),
            jit_code: std::collections::BTreeMap::new(),
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
            iteration_anchors: Vec::new(),
            pending_uncaught_frames: None,
            function_user_props: std::collections::HashMap::new(),
            function_prototype_overrides: std::collections::HashMap::new(),
            function_non_extensible: std::collections::HashSet::new(),
            function_deleted_metadata: std::collections::HashSet::new(),
            non_gc_exotic_prototype_overrides: std::collections::HashMap::new(),
            console_sink: console::default_console_sink(),
            timer_scheduler: None,
            timer_callbacks: timers::TimerCallbacks::new(),
            dynamic_import_loader: None,
            dynamic_import_registry: dynamic_import::DynamicImportRegistry::new(),
            array_iterator_prototype: None,
            map_iterator_prototype: None,
            set_iterator_prototype: None,
            string_iterator_prototype: None,
            regexp_string_iterator_prototype: None,
            iterator_helper_prototype: None,
            wrap_for_valid_iterator_prototype: None,
            function_kind_prototypes: function_kind::FunctionKindPrototypes::default(),
            cold_frames: cold_frame::ColdFramePool::new(),
            realm_intrinsics: realm_intrinsics::RealmIntrinsics::default(),
            regex_compile_cache: regexp::RegexCompileCache::default(),
            tracer: None,
        };
        // Cache typed handles for the well-known constructors and
        // prototypes. Subsequent runtime lookups read the slots and
        // skip the global → ctor → prototype string walk.
        interp
            .realm_intrinsics
            .populate(&mut interp.gc_heap, global_this);
        let extra_roots = otter_gc::ExtraRoots::new(&interp);
        let extra_root_depth = interp.gc_heap.push_extra_roots(extra_roots);
        // §22.1.5 / §23.1.5 / §24.1.5 / §24.2.5 — build the per-kind
        // iterator prototypes once `%Iterator.prototype%` is wired
        // into the global. The bootstrap helper owns the install
        // logic; this site only caches the resulting handles so
        // `intrinsic_prototype_object_for` (iterator family) can
        // route without a global lookup per access.
        if let Ok(iter_proto_value) = interp.constructor_prototype_value("Iterator")
            && let Some(iter_proto) = iter_proto_value.as_object()
        {
            let shape_root = interp.shape_runtime.root();
            let protos =
                crate::intrinsics::iterator::build_builtin_iterator_prototypes_post_bootstrap(
                    &mut interp.gc_heap,
                    shape_root,
                    iter_proto,
                    &interp.well_known_symbols,
                )
                .expect("per-kind iterator prototypes fit within any positive cap");
            interp.array_iterator_prototype = Some(protos.array);
            interp.map_iterator_prototype = Some(protos.map);
            interp.set_iterator_prototype = Some(protos.set);
            interp.string_iterator_prototype = Some(protos.string);
            interp.regexp_string_iterator_prototype = Some(protos.regexp_string);
            interp.iterator_helper_prototype = Some(protos.helper);
            interp.wrap_for_valid_iterator_prototype = Some(protos.wrap_for_valid_iterator);
        }
        interp.install_function_kind_prototypes_post_bootstrap();
        interp.gc_heap.pop_extra_roots_to(extra_root_depth - 1);
        interp
    }

    /// Look up `%<Kind>IteratorPrototype%` by origin.
    #[must_use]
    pub(crate) fn builtin_iterator_prototype_for(
        &self,
        origin: BuiltinIteratorOrigin,
    ) -> Option<JsObject> {
        match origin {
            BuiltinIteratorOrigin::Array => self.array_iterator_prototype,
            BuiltinIteratorOrigin::Map => self.map_iterator_prototype,
            BuiltinIteratorOrigin::Set => self.set_iterator_prototype,
            BuiltinIteratorOrigin::String => self.string_iterator_prototype,
            BuiltinIteratorOrigin::RegExpString => self.regexp_string_iterator_prototype,
            BuiltinIteratorOrigin::Helper => self.iterator_helper_prototype,
            BuiltinIteratorOrigin::WrapForValidIterator => self.wrap_for_valid_iterator_prototype,
        }
    }

    #[cfg(test)]
    pub(crate) fn load_property_ic_count(&self) -> usize {
        self.load_property_ics
            .iter()
            .filter(|entry| entry.is_polymorphic())
            .count()
    }

    #[cfg(test)]
    pub(crate) fn store_property_ic_count(&self) -> usize {
        self.store_property_ics
            .iter()
            .filter(|entry| entry.is_polymorphic())
            .count()
    }

    /// Return aggregate property inline-cache counters.
    #[must_use]
    pub fn property_ic_stats(&self) -> property_ic::PropertyIcStats {
        self.property_ic_stats
    }

    /// Install or remove the runtime-owned JIT compiler hook.
    ///
    /// `None` keeps interpreter-only behavior. A hook returning
    /// [`JitCompileStatus::Unavailable`] or [`JitCompileStatus::Unsupported`]
    /// must also leave execution on the interpreter fallback path.
    pub fn set_jit_compiler(&mut self, hook: Option<std::sync::Arc<dyn jit::JitCompilerHook>>) {
        self.jit_hook = hook;
    }

    /// `true` when a JIT compiler hook has been installed.
    #[must_use]
    pub fn jit_compiler_installed(&self) -> bool {
        self.jit_hook.is_some()
    }

    /// Call-count at which a function body is offered to the JIT. Low enough
    /// that genuinely hot functions tier up early, high enough that one-shot
    /// calls never pay compile latency.
    const JIT_TIER_UP_THRESHOLD: u32 = 50;

    /// After a call pushed a fresh bytecode callee frame as the new top of
    /// `stack`, try to run it as compiled baseline code instead of interpreting.
    ///
    /// Only invoked when a JIT hook is installed and a frame was actually
    /// pushed (the caller checks `stack` grew). Returns `Ok(None)` to interpret
    /// normally; `Ok(Some(popped))` when the JIT ran and the callee returned,
    /// where `popped` mirrors [`Self::return_running_finally`] (`Some(v)` means
    /// the return unwound the dispatch entry and the loop should yield `v`).
    fn maybe_dispatch_jit(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
    ) -> Result<Option<Option<Value>>, VmError> {
        let top_idx = stack.len() - 1;
        let Some(code) = self.resolve_jit_code(stack, context, top_idx) else {
            return Ok(None);
        };
        match self.run_compiled_frame(stack, context, top_idx, &code) {
            jit::JitExecOutcome::Bailed => Ok(None),
            jit::JitExecOutcome::Returned(value) => {
                let popped = self.return_running_finally(stack, value)?;
                Ok(Some(popped))
            }
            jit::JitExecOutcome::Threw(err) => Err(err),
        }
    }

    /// Tier-up entry point for a synchronously-entered call frame (the
    /// [`Self::run_callable_sync`] path), where the callee frame is the sole
    /// entry on its own `stack`. Mirrors [`Self::maybe_dispatch_jit`] but, on a
    /// successful compiled run, the completion *is* the call result (there is no
    /// caller frame to unwind into).
    ///
    /// Returns `Ok(Some(v))` when compiled code ran the frame to completion, or
    /// `Ok(None)` to interpret it normally.
    pub(crate) fn dispatch_jit_sync_entry(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
    ) -> Result<Option<Value>, VmError> {
        if self.jit_hook.is_none() {
            return Ok(None);
        }
        let top_idx = stack.len() - 1;
        let Some(code) = self.resolve_jit_code(stack, context, top_idx) else {
            return Ok(None);
        };
        match self.run_compiled_frame(stack, context, top_idx, &code) {
            jit::JitExecOutcome::Bailed => Ok(None),
            jit::JitExecOutcome::Returned(value) => Ok(Some(value)),
            jit::JitExecOutcome::Threw(err) => Err(err),
        }
    }

    /// Resolve installed compiled code for the bytecode frame at `top_idx`,
    /// compiling once at the tier-up threshold. Returns `None` when the frame is
    /// ineligible (not a fresh ordinary bytecode entry), still cold, or known to
    /// be outside the compilable subset.
    fn resolve_jit_code(
        &mut self,
        stack: &[Frame],
        context: &ExecutionContext,
        top_idx: usize,
    ) -> Option<std::sync::Arc<dyn jit::JitFunctionCode>> {
        // Only fresh, ordinary bytecode frames: at entry (pc == 0), not async,
        // not a generator body.
        let frame = &stack[top_idx];
        if frame.pc != 0 || frame.async_state.is_some() || frame.generator_owner.is_some() {
            return None;
        }
        let fid = frame.function_id;
        if let Some(slot) = self.jit_code.get(&fid) {
            return slot.clone();
        }
        let count = {
            let counter = self.jit_call_counts.entry(fid).or_insert(0);
            *counter = counter.saturating_add(1);
            *counter
        };
        if count < Self::JIT_TIER_UP_THRESHOLD {
            return None;
        }
        let compiled = self.compile_jit_function(context, fid);
        self.jit_code.insert(fid, compiled.clone());
        compiled
    }

    /// Run compiled `code` over the rooted register window of frame `top_idx`.
    ///
    /// The window stays rooted on `stack` for the call, so closure allocation
    /// and recursive calls inside the body are GC-safe.
    fn run_compiled_frame(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        top_idx: usize,
        code: &std::sync::Arc<dyn jit::JitFunctionCode>,
    ) -> jit::JitExecOutcome {
        // SAFETY: the raw pointers are formed from this method's own live
        // borrows (`self`, `stack`, `context`) and are valid for the duration
        // of `run_entry`; the JIT does not retain them, and we do not touch
        // those borrows again until `run_entry` returns.
        let ptrs = jit::JitReentryPtrs {
            vm: <*mut Interpreter>::cast(self),
            stack: <*mut jit::JitFrameStack>::cast(stack),
            context: <*const ExecutionContext>::cast(context),
            frame_index: top_idx,
        };
        code.run_entry(ptrs)
    }

    /// JIT bridge — perform a `Call` from compiled code. Reads the callee and
    /// argument Values from frame `frame_index`'s register window, runs the
    /// callee synchronously (which may itself tier up), and writes the
    /// completion into register `dst`. Safe: all raw-pointer handling stays in
    /// the JIT crate; this side sees only ordinary references.
    ///
    /// # Errors
    /// Propagates any error the callee raises, and `InvalidOperand` for an
    /// out-of-range frame or register index.
    pub fn jit_runtime_call(
        &mut self,
        context: &ExecutionContext,
        stack: &mut jit::JitFrameStack,
        frame_index: usize,
        dst: u16,
        callee_reg: u16,
        arg_regs: &[u16],
    ) -> Result<(), VmError> {
        let frame = stack.get(frame_index).ok_or(VmError::InvalidOperand)?;
        let callee = *frame
            .registers
            .get(callee_reg as usize)
            .ok_or(VmError::InvalidOperand)?;
        let mut args: SmallVec<[Value; 8]> = SmallVec::with_capacity(arg_regs.len());
        for &r in arg_regs {
            args.push(
                *frame
                    .registers
                    .get(r as usize)
                    .ok_or(VmError::InvalidOperand)?,
            );
        }
        let result = self.run_callable_sync(context, &callee, Value::undefined(), args)?;
        let frame = stack.get_mut(frame_index).ok_or(VmError::InvalidOperand)?;
        *frame
            .registers
            .get_mut(dst as usize)
            .ok_or(VmError::InvalidOperand)? = result;
        Ok(())
    }

    /// JIT bridge — build the closure for a `MakeFunction` from compiled code,
    /// writing it into register `dst` of frame `frame_index` (self-reference
    /// capture and upvalue binding go through the normal interpreter path).
    ///
    /// # Errors
    /// Propagates closure-construction errors and `InvalidOperand` for an
    /// out-of-range frame index.
    pub fn jit_runtime_make_function(
        &mut self,
        context: &ExecutionContext,
        stack: &mut jit::JitFrameStack,
        frame_index: usize,
        dst: u16,
        idx: u32,
    ) -> Result<(), VmError> {
        // `self` and `stack` are disjoint, so the two `&mut` are non-aliasing.
        let frame = stack.get_mut(frame_index).ok_or(VmError::InvalidOperand)?;
        self.run_make_function_reg(context, frame, dst, idx)
    }

    /// JIT bridge — base pointer of frame `frame_index`'s register window, for
    /// the compiled entry to address registers. The window is rooted on
    /// `stack`, so the pointer is stable for the compiled call's duration
    /// (recursive calls run on a separate internal stack and never grow this
    /// one).
    #[must_use]
    pub fn jit_frame_regs_ptr(stack: &mut jit::JitFrameStack, frame_index: usize) -> *mut u64 {
        stack[frame_index].registers.as_mut_ptr().cast::<u64>()
    }

    /// Build a compile request for `fid` and run the installed hook. Returns the
    /// installed code, or `None` when the hook declines (unsupported subset or
    /// executable memory unavailable) — either way execution stays correct on
    /// the interpreter.
    fn compile_jit_function(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
    ) -> Option<std::sync::Arc<dyn jit::JitFunctionCode>> {
        let view = context.jit_function_view(fid)?;
        let trace = std::env::var_os("OTTER_JIT_TRACE").is_some();
        let (regs, params) = (view.register_count, view.param_count);
        let hook = self.jit_hook.as_ref()?.clone();
        let status = hook.compile_function(jit::JitCompileRequest { function: view });
        if trace {
            eprintln!("[jit] compile fid={fid} regs={regs} params={params} -> {status:?}");
        }
        match status {
            Ok(jit::JitCompileStatus::Compiled { code }) => Some(code),
            _ => None,
        }
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
        let site_count = context.property_ic_site_end();
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
        // Host-settlement entry point: runs outside any rooted VM
        // scope, and settling can allocate reaction records — root
        // the runtime state for the duration.
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let extra_root_depth = self.gc_heap.push_extra_roots(extra_roots);
        let settled = self.settle_dynamic_import_inner(token, outcome);
        self.gc_heap.pop_extra_roots_to(extra_root_depth - 1);
        settled
    }

    fn settle_dynamic_import_inner(
        &mut self,
        token: u64,
        outcome: Result<Value, Value>,
    ) -> Option<ExecutionContext> {
        let entry = self.dynamic_import_registry.take(token)?;
        let jobs = match outcome {
            Ok(value) => crate::JsPromise::fulfill(&entry.promise, &mut self.gc_heap, value),
            Err(reason) => crate::JsPromise::reject(&entry.promise, &mut self.gc_heap, reason),
        };
        for j in jobs.jobs {
            self.microtasks.enqueue(j);
        }
        Some(entry.context)
    }

    /// Run one dynamically loaded module init to completion or first
    /// suspension. Mirrors `run_inner`'s entry wiring: a top-level-await
    /// module body compiles to an async `<main>`, which needs an async
    /// result promise before `Op::Await` can park the frame (§16.2.1.9
    /// ExecuteAsyncModule). Returns that promise for async inits so the
    /// dynamic-import machinery can defer settlement until the module
    /// body actually finishes; sync inits return `None` after running
    /// to completion.
    ///
    /// # Errors
    /// Propagates any `VmError` thrown synchronously by the init body.
    pub fn run_module_init(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        env: Value,
        import_meta: Value,
    ) -> Result<Option<crate::promise::JsPromiseHandle>, VmError> {
        self.run_module_init_phase(context, function_id, env, import_meta, false)
    }

    /// Link-phase init invocation — runs only the §16.2.1.7
    /// InitializeEnvironment prologue (export TDZ slots + hoisted
    /// function instantiation) and returns before any body statement.
    pub fn run_module_init_hoist(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        env: Value,
        import_meta: Value,
    ) -> Result<(), VmError> {
        self.run_module_init_phase(context, function_id, env, import_meta, true)
            .map(|_| ())
    }

    fn run_module_init_phase(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        env: Value,
        import_meta: Value,
        hoist_phase: bool,
    ) -> Result<Option<crate::promise::JsPromiseHandle>, VmError> {
        self.enter_sync_reentry()?;
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let extra_root_depth = self.gc_heap.push_extra_roots(extra_roots);
        let result =
            self.run_module_init_inner(context, function_id, env, import_meta, hoist_phase);
        self.gc_heap.pop_extra_roots_to(extra_root_depth - 1);
        self.leave_sync_reentry();
        result
    }

    fn run_module_init_inner(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        env: Value,
        import_meta: Value,
        hoist_phase: bool,
    ) -> Result<Option<crate::promise::JsPromiseHandle>, VmError> {
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        // The module environment record: link-phase and
        // evaluation-phase invocations share one persistent set of
        // own-upvalue cells so hoisted closures and the body bind the
        // same module-scope storage.
        let module_url: std::sync::Arc<str> = std::sync::Arc::from(function.module_url.as_ref());
        let upvalues = if let Some(cells) = self.module_init_upvalues.get(&module_url) {
            cells.clone()
        } else {
            let built = Frame::build_upvalues_for_exec(
                &mut self.gc_heap,
                function,
                Frame::empty_upvalues(),
            )?;
            self.module_init_upvalues.insert(module_url, built.clone());
            built
        };
        let mut frame =
            Frame::with_exec_return_upvalues_and_this(function, None, upvalues, Value::undefined());
        let args: SmallVec<[Value; 8]> =
            smallvec::smallvec![env, import_meta, Value::boolean(hoist_phase)];
        self.bind_bytecode_call_arguments(function, &mut frame, args)?;
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(frame);
        let init_promise = if function.is_async {
            let result = promise_dispatch::PromiseBuilder::with_context(context.clone())
                .pending_stack_rooted(self, &stack, &[&env, &import_meta], &[])?;
            stack
                .last_mut()
                .expect("init frame was just pushed")
                .async_state = Some(AsyncFrameState {
                result_promise: result,
            });
            Some(result)
        } else {
            None
        };
        self.dispatch_loop(context, &mut stack)?;
        Ok(init_promise)
    }

    /// Defer settlement of dynamic-import `token` until the gating
    /// async module-init promise settles — the target's init promise,
    /// pushed last by the evaluation DFS (spec `[[TopLevelCapability]]`
    /// shape, §16.2.1.9). A rejection rejects the import; fulfilment
    /// resolves it with the namespace registered for `namespace_url`.
    ///
    /// # Errors
    /// Returns `VmError` only for allocation failure while building
    /// the reaction callables.
    pub fn settle_dynamic_import_on_async_inits(
        &mut self,
        context: &ExecutionContext,
        token: u64,
        promises: Vec<crate::promise::JsPromiseHandle>,
        namespace_url: std::sync::Arc<str>,
    ) -> Result<(), VmError> {
        debug_assert!(!promises.is_empty());
        let Some(gate) = promises.last().copied() else {
            return Ok(());
        };
        let url = namespace_url;
        let on_fulfilled = crate::native_function::native_value_with_captures_unchecked_with_roots(
            &mut self.gc_heap,
            "dynamicImportInitFulfilled",
            SmallVec::new(),
            &mut |_visitor| {},
            move |ncx, _args, _captures| {
                let interp = ncx.interp_mut();
                let namespace = interp
                    .module_env(&url)
                    .map(Value::object)
                    .unwrap_or_else(Value::undefined);
                let _ = interp.settle_dynamic_import(token, Ok(namespace));
                Ok(Value::undefined())
            },
        )?;
        let on_rejected = crate::native_function::native_value_with_captures_unchecked_with_roots(
            &mut self.gc_heap,
            "dynamicImportInitRejected",
            SmallVec::new(),
            &mut |visitor| on_fulfilled.trace_value_slots(visitor),
            move |ncx, args, _captures| {
                let reason = args.first().copied().unwrap_or_else(Value::undefined);
                let _ = ncx.interp_mut().settle_dynamic_import(token, Err(reason));
                Ok(Value::undefined())
            },
        )?;
        let capability = promise_dispatch::PromiseBuilder::with_context(context.clone())
            .capability_runtime_rooted(self, &[&on_fulfilled, &on_rejected], &[])?;
        let outcome = crate::JsPromise::perform_then_with_context(
            &gate,
            &mut self.gc_heap,
            Some(on_fulfilled),
            Some(on_rejected),
            capability,
            Some(context.clone()),
        );
        if let Some(job) = outcome.immediate_job {
            self.microtasks.enqueue(job);
        }
        Ok(())
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

    fn non_gc_exotic_prototype_override_key(
        value: &Value,
        heap: &otter_gc::GcHeap,
    ) -> Option<usize> {
        if let Some(buffer) = value.as_array_buffer() {
            return Some(buffer.identity_addr() as usize);
        }
        if let Some(view) = value.as_data_view() {
            return Some(view.identity_addr() as usize);
        }
        value
            .as_typed_array(heap)
            .map(|array| array.identity_addr() as usize)
    }

    /// Store the allocation-time `[[Prototype]]` selected by
    /// ECMA-262 `GetPrototypeFromConstructor` for exotics whose
    /// bodies are not GC-managed yet.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-getprototypefromconstructor>
    pub(crate) fn set_non_gc_exotic_prototype_override(
        &mut self,
        value: &Value,
        proto: Option<Value>,
    ) {
        let Some(key) = Self::non_gc_exotic_prototype_override_key(value, &self.gc_heap) else {
            return;
        };
        match proto {
            Some(proto) => {
                self.non_gc_exotic_prototype_overrides.insert(key, proto);
            }
            None => {
                self.non_gc_exotic_prototype_overrides.remove(&key);
            }
        }
    }

    pub(crate) fn non_gc_exotic_prototype_override(&self, value: &Value) -> Option<Value> {
        let key = Self::non_gc_exotic_prototype_override_key(value, &self.gc_heap)?;
        self.non_gc_exotic_prototype_overrides.get(&key).cloned()
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
    pub(crate) fn get_prototype_for_op(&mut self, value: &Value) -> Result<Value, VmError> {
        // §15.7.14 step 6.b — a class constructor's [[Prototype]] is
        // the parent class value (identity preserved in the
        // ctor_proto slot), %Function.prototype% for a base class,
        // or null for `extends null` / a later setPrototypeOf.
        if let Some(c) = value.as_class_constructor() {
            let stored = c.ctor_proto(&self.gc_heap);
            if !stored.is_undefined() {
                return Ok(stored);
            }
            return Ok(Value::object(self.function_prototype_object()?));
        }
        let intrinsic_or_null =
            |this: &mut Self, v: &Value| match this.intrinsic_prototype_object_for(v) {
                Some(o) => Value::object(o),
                None => Value::null(),
            };
        if let Some(obj) = value.as_object() {
            let stored = object::prototype_value(obj, &self.gc_heap);
            let has_construct = object_has_construct_slot(&Value::object(obj), &self.gc_heap);
            if has_construct {
                let function_proto = self.function_prototype_object().ok();
                let object_proto = self.object_prototype_object_opt();
                match &stored {
                    None => {
                        if let Some(fp) = function_proto {
                            return Ok(Value::object(fp));
                        }
                    }
                    Some(p_val) if p_val.as_object().is_some_and(|p| object_proto == Some(p)) => {
                        if let Some(fp) = function_proto {
                            return Ok(Value::object(fp));
                        }
                    }
                    _ => {}
                }
            }
            return Ok(stored.unwrap_or(Value::null()));
        }
        if let Some(t) = value.as_typed_array(&self.gc_heap) {
            if let Some(over) = t.custom_proto(&self.gc_heap) {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if let Some(nf) = value.as_native_function() {
            if let Some(over) = nf.prototype_override(&self.gc_heap) {
                return Ok(over);
            }
            return Ok(Value::object(self.function_prototype_object()?));
        }
        if let Some(arr) = value.as_array() {
            if let Some(over) = array::prototype_override(arr, &self.gc_heap) {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if let Some(map) = value.as_map() {
            if let Some(over) = crate::collections::map_prototype_override(map, &self.gc_heap) {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if let Some(set) = value.as_set() {
            if let Some(over) = crate::collections::set_prototype_override(set, &self.gc_heap) {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if let Some(map) = value.as_weak_map() {
            if let Some(over) = crate::collections::weak_map_prototype_override(map, &self.gc_heap)
            {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if let Some(set) = value.as_weak_set() {
            if let Some(over) = crate::collections::weak_set_prototype_override(set, &self.gc_heap)
            {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if let Some(promise) = value.as_promise() {
            if let Some(over) = promise.prototype_override(&self.gc_heap) {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if let Some(regexp) = value.as_regexp() {
            if let Some(over) = regexp.prototype_override(&self.gc_heap) {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if let Some(weak_ref) = value.as_weak_ref() {
            if let Some(over) =
                crate::weak_refs::weak_ref_prototype_override(weak_ref, &self.gc_heap)
            {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if let Some(registry) = value.as_finalization_registry() {
            if let Some(over) =
                crate::weak_refs::finalization_registry_prototype_override(registry, &self.gc_heap)
            {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if value.is_function()
            || value.is_closure()
            || value.is_bound_function()
            || value.is_class_constructor()
        {
            // §10.2 ordinary bytecode functions: the kind prototype
            // (%GeneratorFunction.prototype% et al.) for generator /
            // async flavours — resolved context-free through the
            // shared code space so proto-chain walks (`instanceof`,
            // `Reflect.getPrototypeOf`) see the same graph as
            // property reads — else `%Function.prototype%`.
            if let Some(function_id) = value.as_function().or_else(|| {
                value
                    .as_closure(&self.gc_heap)
                    .map(|c| c.cached_function_id)
            }) {
                if let Some(over) = self.function_prototype_overrides.get(&function_id).copied() {
                    return Ok(over);
                }
                if let Some(chunk) = self.code_space.chunk_for(function_id)
                    && let Some(local) = function_id.checked_sub(chunk.function_base)
                    && let Some(function) = chunk.module.functions.get(local as usize)
                    && let Some(proto) = self.function_kind_prototypes.kind_prototype_for_flags(
                        function.is_generator,
                        function.is_async || function.is_async_generator,
                    )
                {
                    return Ok(Value::object(proto));
                }
            }
            return Ok(Value::object(self.function_prototype_object()?));
        }
        // §10.4 exotic objects (ArrayBuffer / SharedArrayBuffer /
        // DataView / TypedArray) — per-class realm prototype.
        // <https://tc39.es/ecma262/#sec-ordinarygetprototypeof>
        if value.is_array_buffer() || value.is_data_view() || value.is_typed_array() {
            if let Some(over) = self.non_gc_exotic_prototype_override(value) {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if let Some(t) = value.as_temporal(&self.gc_heap) {
            return Ok(self
                .temporal_prototype_object(t.kind())
                .map(Value::object)
                .unwrap_or(Value::null()));
        }
        if let Some(intl) = value.as_intl(&self.gc_heap) {
            return Ok(self.intl_kind_prototype_value(intl.kind().class_name()));
        }
        if let Some(generator) = value.as_generator() {
            if let Some(proto) = generator.prototype_override(&self.gc_heap) {
                return Ok(proto);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if value.is_iterator() {
            return Ok(intrinsic_or_null(self, value));
        }
        // §20.1.2.10 / §7.1.18 — primitives ToObject then walk
        // wrapper's [[Prototype]].
        if value.is_symbol()
            || value.is_string()
            || value.is_number()
            || value.is_boolean()
            || value.is_big_int()
        {
            return Ok(intrinsic_or_null(self, value));
        }
        Err(VmError::TypeMismatchAt {
            op: "Object.getPrototypeOf",
            kind: value_kind_name(value),
        })
    }

    pub(crate) fn object_prototype_object_opt(&self) -> Option<JsObject> {
        // Fast path: typed slot populated by RealmIntrinsics::populate.
        if let Some(proto) = self.realm_intrinsics.object_prototype {
            return Some(proto);
        }
        // Fallback for embedders that build a non-default global
        // (e.g. feature-gated bootstrap that omits Object).
        let ctor =
            object::get(self.global_this, &self.gc_heap, "Object").and_then(|v| v.as_object())?;
        object::get(ctor, &self.gc_heap, "prototype").and_then(|v| v.as_object())
    }

    pub(crate) fn function_prototype_object(&self) -> Result<JsObject, VmError> {
        // Fast path: typed slot.
        if let Some(proto) = self.realm_intrinsics.function_prototype {
            return Ok(proto);
        }
        let function_ctor = object::get(self.global_this, &self.gc_heap, "Function")
            .and_then(|v| v.as_object())
            .ok_or(VmError::TypeMismatch)?;
        object::get(function_ctor, &self.gc_heap, "prototype")
            .and_then(|v| v.as_object())
            .ok_or(VmError::TypeMismatch)
    }

    fn is_callable_runtime(&self, value: &Value) -> bool {
        // §10.5.15 — a Proxy is callable only when its target was
        // callable at creation (the heap-blind `is_callable` assumes
        // every proxy is callable). Resolve the real [[Call]] slot here.
        if let Some(proxy) = value.as_proxy() {
            return proxy.is_callable(&self.gc_heap);
        }
        is_callable(value) || object_has_call_slot(value, &self.gc_heap)
    }

    /// Resolve property read on function / closure. Honours user
    /// props via `function_user_props`, lazily allocates
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

    /// Look up or register a symbol for `key`. Splits borrows over the
    /// registry, the GC heap, and the string heap so callers do not
    /// need to juggle them manually.
    ///
    /// # Errors
    /// Surfaces [`crate::symbol::SymbolRegistryError`] (string or GC
    /// out-of-memory).
    pub fn symbol_for_key(
        &mut self,
        key: &str,
    ) -> Result<JsSymbol, crate::symbol::SymbolRegistryError> {
        self.symbol_registry.for_key(&mut self.gc_heap, key)
    }

    /// Register or overwrite a module's `module_env` object so
    /// later [`Op::ImportNamespace`] dispatches can resolve
    /// references to it.
    ///
    /// Called by the runtime's module-graph driver as it walks
    /// the topological order — once a module's `<module-init>`
    /// has run and populated its env, the driver records it
    /// here keyed by canonical URL.
    pub fn register_module_env(&mut self, url: std::sync::Arc<str>, env: JsObject) {
        self.module_environments.insert(url, env);
    }

    /// Register a module's §16.2.1.6 ResolveExport table (exported name
    /// → `(defining_module, binding)`), computed by the linker. Read by
    /// the Module Namespace Exotic Object MOP forks and
    /// [`Op::LoadImportBinding`] so re-exported / star-exported names
    /// resolve to the defining module's live binding. Overwrites any
    /// prior table for `url`; cleared by [`Self::reset_module_state`].
    pub fn register_module_resolved_exports(
        &mut self,
        url: std::sync::Arc<str>,
        table: std::collections::BTreeMap<String, (std::sync::Arc<str>, String)>,
    ) {
        self.module_resolved_exports.insert(url, table);
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
        self.module_init_upvalues.clear();
        self.module_hoisted.clear();
        self.module_resolution_cache.clear();
        self.module_records.clear();
        self.next_module_async_order = 0;
        self.deferred_namespaces.clear();
        self.module_namespaces.clear();
        self.module_resolved_exports.clear();
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
        let referrer_rc: std::sync::Arc<str> = std::sync::Arc::from(referrer);
        let key = (referrer_rc.clone(), specifier.to_string());
        let target_url = if let Some(hit) = self.module_resolution_cache.get(&key) {
            hit.clone()
        } else {
            let target = context.module_resolution_target(referrer, specifier)?;
            let target_rc: std::sync::Arc<str> = std::sync::Arc::from(target);
            self.module_resolution_cache.insert(key, target_rc.clone());
            target_rc
        };
        self.module_environments.get(target_url.as_ref()).cloned()
    }

    /// Resolve `(referrer, specifier)` to the eager Module Namespace
    /// Exotic Object (§10.4.6) — used by the user-visible `import * as
    /// ns` binding and `export * as ns`, distinct from the raw module
    /// environment used for named-import indirection.
    pub(crate) fn resolve_module_namespace_object(
        &mut self,
        context: &ExecutionContext,
        referrer: &str,
        specifier: &str,
    ) -> Option<JsObject> {
        let referrer_rc: std::sync::Arc<str> = std::sync::Arc::from(referrer);
        let key = (referrer_rc, specifier.to_string());
        let target_url = if let Some(hit) = self.module_resolution_cache.get(&key) {
            hit.clone()
        } else {
            let target = context.module_resolution_target(referrer, specifier)?;
            let target_rc: std::sync::Arc<str> = std::sync::Arc::from(target);
            self.module_resolution_cache.insert(key, target_rc.clone());
            target_rc
        };
        self.get_or_create_module_namespace(target_url.as_ref())
    }

    /// Eager Module Namespace Exotic Object (§10.4.6) wrapping the
    /// environment of `target_url`, created on first use and cached so
    /// every `import * as ns` / re-export of the same module yields the
    /// identical object.
    fn get_or_create_module_namespace(&mut self, target_url: &str) -> Option<JsObject> {
        let target_rc: std::sync::Arc<str> = std::sync::Arc::from(target_url);
        if let Some(ns) = self.module_namespaces.get(&target_rc) {
            return Some(*ns);
        }
        let env = *self.module_environments.get(&target_rc)?;
        let ns = self
            .alloc_module_namespace_object(env, target_rc.clone())
            .ok()?;
        self.module_namespaces.insert(target_rc, ns);
        Some(ns)
    }

    /// §10.4.6 namespace string-key resolution. Resolves `name` through
    /// `ns_obj`'s module §16.2.1.6 ResolveExport table to the live
    /// binding value. Returns `Some(value)` when `name` is an exported
    /// binding — the value may be the TDZ hole, which the caller maps to
    /// a `ReferenceError` (§10.4.6.8 step 9). Returns `None` when `name`
    /// is not exported. A re-exported / star-exported name resolves to
    /// the *defining* module's live environment, not a snapshot. The
    /// `"*namespace*"` binding (`export * as ns`) resolves to the
    /// defining module's namespace object. Unmodeled (host) modules with
    /// no table fall back to reading the wrapped environment directly.
    pub(crate) fn module_namespace_get_binding(
        &mut self,
        ns_obj: JsObject,
        name: &str,
    ) -> Option<Value> {
        let url = crate::object::module_namespace_url(ns_obj, &self.gc_heap)?;
        self.resolve_module_binding(&url, name)
    }

    /// §16.2.1.6 ResolveExport + §9.1.1.5 GetBindingValue for one
    /// `(module_url, exported name)` pair. Returns the defining module's
    /// live binding value (possibly the TDZ hole), the defining module's
    /// namespace object for the `"*namespace*"` sentinel, or `None` when
    /// the name is not exported. Backs both the namespace MOP forks and
    /// [`Op::LoadImportBinding`]. Unmodeled (host) modules with no table
    /// read their environment directly by name.
    pub(crate) fn resolve_module_binding(&mut self, module_url: &str, name: &str) -> Option<Value> {
        if let Some(table) = self.module_resolved_exports.get(module_url) {
            let (defmod, binding) = table.get(name)?.clone();
            if binding == "*namespace*" {
                return self
                    .get_or_create_module_namespace(&defmod)
                    .map(Value::object);
            }
            if binding == "*deferred-namespace*" {
                return self
                    .get_or_create_deferred_namespace(defmod)
                    .ok()
                    .map(Value::object);
            }
            let env = *self.module_environments.get(&defmod)?;
            return Some(
                crate::object::get(env, &self.gc_heap, &binding).unwrap_or_else(Value::hole),
            );
        }
        let env = *self.module_environments.get(module_url)?;
        crate::object::get(env, &self.gc_heap, name)
    }

    /// Exported string names a namespace exposes — its ResolveExport
    /// table keys (already ascending), or the wrapped env keys for
    /// unmodeled (host) modules. Used by the namespace `[[HasProperty]]`
    /// and `[[OwnPropertyKeys]]` MOP forks.
    pub(crate) fn module_namespace_export_names(&self, ns_obj: JsObject) -> Vec<String> {
        let Some(url) = crate::object::module_namespace_url(ns_obj, &self.gc_heap) else {
            return Vec::new();
        };
        if let Some(table) = self.module_resolved_exports.get(&url) {
            return table.keys().cloned().collect();
        }
        match crate::object::module_namespace_env(ns_obj, &self.gc_heap) {
            Some(env) => crate::object::module_namespace_sorted_string_keys(env, &self.gc_heap),
            None => Vec::new(),
        }
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

    fn enter_sync_reentry(&mut self) -> Result<(), VmError> {
        let limit = self.max_stack_depth.min(DEFAULT_MAX_SYNC_REENTRY_DEPTH);
        if self.sync_reentry_depth >= limit {
            return Err(VmError::StackOverflow { limit });
        }
        self.sync_reentry_depth += 1;
        Ok(())
    }

    fn leave_sync_reentry(&mut self) {
        debug_assert!(self.sync_reentry_depth > 0);
        self.sync_reentry_depth = self.sync_reentry_depth.saturating_sub(1);
    }

    /// Install the parse + compile callback used by `Op::Eval` and
    /// `Op::NewFunction`. The runtime layer hooks the otter-compiler
    /// in here at construction time. Pass `None` (the default) to
    /// disable dynamic code; both opcodes will raise SyntaxError
    /// when invoked without a hook.
    pub fn set_eval_hook(&mut self, hook: Option<EvalHook>) {
        self.eval_hook = hook;
    }

    /// Install (or clear) the per-instruction step tracer.
    ///
    /// When `Some`, every dispatched instruction routes through the
    /// observer. When `None` (the default), the dispatch loop pays a
    /// single `Option` discriminant check per instruction and never
    /// touches the tracer slot. The trace format is documented at
    /// [`crate::inspect`] and `docs/book/src/engine/step-trace.md`.
    pub fn set_tracer(&mut self, tracer: Option<Box<dyn inspect::StepTracer>>) {
        self.tracer = tracer;
    }

    /// Whether a step tracer is installed.
    #[must_use]
    pub fn has_tracer(&self) -> bool {
        self.tracer.is_some()
    }

    /// Install (or clear) the shape-transition observer. The
    /// observer fires on every hidden-class transition the VM
    /// takes — both fresh allocations and cached lookups. See
    /// [`inspect::ShapeTransitionEvent`].
    pub fn set_shape_transition_observer(
        &mut self,
        observer: Option<Box<dyn inspect::ShapeTransitionObserver>>,
    ) {
        self.shape_runtime.set_observer(observer);
    }

    /// Snapshot every property inline-cache site in dense site-id
    /// order. The snapshot is built without disturbing the live IC
    /// state and can be called from anywhere with a `&self`
    /// borrow.
    #[must_use]
    pub fn ic_snapshot(&self) -> Vec<inspect::IcSiteSnapshot> {
        let mut out = Vec::with_capacity(
            self.load_property_ics.len()
                + self.store_property_ics.len()
                + self.has_property_ics.len(),
        );
        for (index, entry) in self.load_property_ics.iter().enumerate() {
            out.push(inspect::IcSiteSnapshot {
                site_index: index as u32,
                kind: inspect::IcSiteKind::Load,
                state: inspect::snapshot_load_state(entry),
            });
        }
        for (index, entry) in self.store_property_ics.iter().enumerate() {
            out.push(inspect::IcSiteSnapshot {
                site_index: index as u32,
                kind: inspect::IcSiteKind::Store,
                state: inspect::snapshot_store_state(entry),
            });
        }
        for (index, entry) in self.has_property_ics.iter().enumerate() {
            out.push(inspect::IcSiteSnapshot {
                site_index: index as u32,
                kind: inspect::IcSiteKind::Has,
                state: inspect::snapshot_has_state(entry),
            });
        }
        out
    }

    /// Snapshot the active hidden-class transition tree. Nodes
    /// appear in deterministic order: root first, then transitions
    /// sorted by `(parent_shape_id, transition_key)`.
    #[must_use]
    pub fn shape_transition_snapshot(&self) -> inspect::ShapeTransitionSnapshot {
        inspect::build_shape_transition_snapshot(&self.shape_runtime, &self.gc_heap)
    }

    /// Type-count summary of every live GC body. Walks the heap
    /// without holding allocator paths open — safe to call from
    /// any mutator-turn boundary.
    #[must_use]
    pub fn heap_snapshot_summary(&self) -> inspect::HeapSnapshotSummary {
        let raw = self.gc_heap.snapshot(&[]);
        inspect::HeapSnapshotSummary::from_snapshot(&raw)
    }

    /// Write a Chrome DevTools `.heapsnapshot` JSON document for the
    /// current heap state. The output matches the format documented
    /// at
    /// <https://developer.chrome.com/docs/devtools/memory-problems/heap-snapshots>
    /// and can be loaded straight into the DevTools "Memory" panel.
    ///
    /// # Errors
    /// Propagates I/O errors from `writer`.
    pub fn write_chrome_heap_snapshot<W: std::io::Write>(
        &self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        // Single-mutator model: `&self` while no allocator path
        // runs is the documented STW-equivalent for the safe
        // `chrome_heap_snapshot` wrapper.
        let payload = otter_gc::devtools_snapshot::chrome_heap_snapshot(&self.gc_heap);
        serde_json::to_writer(&mut *writer, &payload.0).map_err(std::io::Error::other)?;
        writer.write_all(b"\n")?;
        Ok(())
    }

    /// Cloneable handle for cooperative cancellation.
    #[must_use]
    pub fn interrupt_handle(&self) -> InterruptFlag {
        self.interrupt.clone()
    }

    /// Configure whether this isolate may block in `Atomics.wait`.
    ///
    /// Main/direct runtimes keep this disabled so an infinite wait cannot
    /// stall the host thread. Worker runtimes enable it because their owning
    /// host can interrupt and terminate the isolate thread.
    pub fn set_allow_blocking_atomics_wait(&mut self, allow: bool) {
        self.allow_blocking_atomics_wait = allow;
    }

    /// Whether this isolate may block in `Atomics.wait`.
    #[must_use]
    pub fn allow_blocking_atomics_wait(&self) -> bool {
        self.allow_blocking_atomics_wait
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

    fn primitive_wrapper_prototype(&mut self, constructor_name: &str) -> Result<JsObject, VmError> {
        let constructor = object::get(self.global_this, &self.gc_heap, constructor_name)
            .ok_or(VmError::InvalidOperand)?;
        let prototype = if let Some(ctor) = constructor.as_object() {
            object::get(ctor, &self.gc_heap, "prototype")
        } else if let Some(native) = constructor.as_native_function() {
            let desc = native
                .own_property_descriptor(&mut self.gc_heap, "prototype")
                .map_err(|_| VmError::InvalidOperand)?;
            desc.and_then(|d| match d.kind {
                object::DescriptorKind::Data { value } => Some(value),
                _ => None,
            })
        } else {
            None
        };
        prototype
            .and_then(|v| v.as_object())
            .ok_or(VmError::InvalidOperand)
    }

    fn box_sloppy_this_primitive_runtime_rooted(
        &mut self,
        this_value: Value,
        slice_roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let object = if let Some(value) = this_value.as_boolean() {
            let proto = self.primitive_wrapper_prototype("Boolean")?;
            let obj =
                self.alloc_runtime_rooted_object_with_proto(proto, &[&this_value], slice_roots)?;
            object::set_boolean_data(obj, &mut self.gc_heap, value);
            obj
        } else if let Some(value) = this_value.as_number() {
            let proto = self.primitive_wrapper_prototype("Number")?;
            let obj =
                self.alloc_runtime_rooted_object_with_proto(proto, &[&this_value], slice_roots)?;
            object::set_number_data(obj, &mut self.gc_heap, value);
            obj
        } else if let Some(value) = this_value.as_string(&self.gc_heap) {
            let proto = self.primitive_wrapper_prototype("String")?;
            let obj =
                self.alloc_runtime_rooted_object_with_proto(proto, &[&this_value], slice_roots)?;
            object::set_string_data(obj, &mut self.gc_heap, value);
            obj
        } else if let Some(sym) = this_value.as_symbol(&self.gc_heap) {
            let proto = self.primitive_wrapper_prototype("Symbol")?;
            let obj =
                self.alloc_runtime_rooted_object_with_proto(proto, &[&this_value], slice_roots)?;
            object::set_symbol_data(obj, &mut self.gc_heap, sym);
            obj
        } else if let Some(value) = this_value.as_big_int() {
            let proto = self.primitive_wrapper_prototype("BigInt")?;
            let obj =
                self.alloc_runtime_rooted_object_with_proto(proto, &[&this_value], slice_roots)?;
            object::set_bigint_data(obj, &mut self.gc_heap, value);
            obj
        } else {
            return Ok(this_value);
        };
        Ok(Value::object(object))
    }

    fn box_sloppy_this_primitive_stack_rooted(
        &mut self,
        stack: &SmallVec<[Frame; 8]>,
        this_value: Value,
        slice_roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let object = if let Some(value) = this_value.as_boolean() {
            let proto = self.primitive_wrapper_prototype("Boolean")?;
            let obj = self.alloc_stack_rooted_object_with_proto(
                stack,
                proto,
                &[&this_value],
                slice_roots,
            )?;
            object::set_boolean_data(obj, &mut self.gc_heap, value);
            obj
        } else if let Some(value) = this_value.as_number() {
            let proto = self.primitive_wrapper_prototype("Number")?;
            let obj = self.alloc_stack_rooted_object_with_proto(
                stack,
                proto,
                &[&this_value],
                slice_roots,
            )?;
            object::set_number_data(obj, &mut self.gc_heap, value);
            obj
        } else if let Some(value) = this_value.as_string(&self.gc_heap) {
            let proto = self.primitive_wrapper_prototype("String")?;
            let obj = self.alloc_stack_rooted_object_with_proto(
                stack,
                proto,
                &[&this_value],
                slice_roots,
            )?;
            object::set_string_data(obj, &mut self.gc_heap, value);
            obj
        } else if let Some(sym) = this_value.as_symbol(&self.gc_heap) {
            let proto = self.primitive_wrapper_prototype("Symbol")?;
            let obj = self.alloc_stack_rooted_object_with_proto(
                stack,
                proto,
                &[&this_value],
                slice_roots,
            )?;
            object::set_symbol_data(obj, &mut self.gc_heap, sym);
            obj
        } else if let Some(value) = this_value.as_big_int() {
            let proto = self.primitive_wrapper_prototype("BigInt")?;
            let obj = self.alloc_stack_rooted_object_with_proto(
                stack,
                proto,
                &[&this_value],
                slice_roots,
            )?;
            object::set_bigint_data(obj, &mut self.gc_heap, value);
            obj
        } else {
            return Ok(this_value);
        };
        Ok(Value::object(object))
    }

    fn object_for_primitive_property_base_stack_rooted(
        &mut self,
        stack: &SmallVec<[Frame; 8]>,
        value: &Value,
    ) -> Result<Option<JsObject>, VmError> {
        let object = if let Some(v) = value.as_boolean() {
            let proto = self.primitive_wrapper_prototype("Boolean")?;
            let obj = self.alloc_stack_rooted_object_with_proto(stack, proto, &[value], &[])?;
            object::set_boolean_data(obj, &mut self.gc_heap, v);
            obj
        } else if let Some(v) = value.as_number() {
            let proto = self.primitive_wrapper_prototype("Number")?;
            let obj = self.alloc_stack_rooted_object_with_proto(stack, proto, &[value], &[])?;
            object::set_number_data(obj, &mut self.gc_heap, v);
            obj
        } else if let Some(v) = value.as_string(&self.gc_heap) {
            let proto = self.primitive_wrapper_prototype("String")?;
            let obj = self.alloc_stack_rooted_object_with_proto(stack, proto, &[value], &[])?;
            object::set_string_data(obj, &mut self.gc_heap, v);
            obj
        } else if value.is_symbol() {
            let proto = self.primitive_wrapper_prototype("Symbol")?;
            self.alloc_stack_rooted_object_with_proto(stack, proto, &[value], &[])?
        } else if value.is_big_int() {
            let proto = self.primitive_wrapper_prototype("BigInt")?;
            self.alloc_stack_rooted_object_with_proto(stack, proto, &[value], &[])?
        } else {
            return Ok(None);
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
            v if v.is_undefined() || v.is_null() => Ok(Value::object(self.global_this)),
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
        if this_value.is_undefined() || this_value.is_null() {
            Ok(Value::object(self.global_this))
        } else {
            self.box_sloppy_this_primitive_stack_rooted(stack, this_value, slice_roots)
        }
    }

    /// Install a class-shaped global from a static JS surface spec.
    ///
    /// Product crates use this for centralized bootstrap wiring:
    /// specs stay static, while the actual object allocation and
    /// global mutation happen during one mutator turn.
    pub fn install_global_class(&mut self, spec: &'static ClassSpec) -> Result<(), JsSurfaceError> {
        let raw_roots = self.collect_runtime_roots();
        let global_root = Value::object(self.global_this);
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

    /// Borrow the persistent module-init upvalue spines for GC root
    /// tracing. The cells back module-scope bindings shared between
    /// the link-phase and evaluation-phase init invocations.
    pub(crate) fn module_init_upvalues_for_trace(
        &self,
    ) -> impl Iterator<Item = &Box<[crate::UpvalueCell]>> {
        self.module_init_upvalues.values()
    }

    /// Global declarative-record cells for the GC root walk.
    pub(crate) fn global_lexicals_for_trace(&self) -> impl Iterator<Item = &crate::UpvalueCell> {
        self.global_lexicals.values().map(|(cell, _)| cell)
    }

    /// Borrow cached eager + deferred module namespace exotic objects
    /// for GC root tracing. They are reachable from JS via `import * as
    /// ns`, so they must survive collection even when no live register
    /// currently holds them.
    pub fn module_namespaces_for_trace(&self) -> impl Iterator<Item = &JsObject> {
        self.module_namespaces
            .values()
            .chain(self.deferred_namespaces.values())
    }

    /// Borrow cached module-evaluation thrown values for GC root tracing.
    pub fn module_errors_for_trace(&self) -> impl Iterator<Item = &Value> {
        self.module_records
            .values()
            .filter_map(|record| record.evaluation_error.as_ref())
    }

    /// Borrow per-module evaluation gate promises for GC root tracing.
    pub(crate) fn module_async_init_promises_for_trace(
        &self,
    ) -> impl Iterator<Item = &crate::promise::JsPromiseHandle> {
        self.module_records
            .values()
            .filter_map(|record| record.evaluation_promise.as_ref())
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

    /// Iterator over ordinary-function `[[Prototype]]` override
    /// values. Used by the GC root walker because subclassed
    /// dynamic functions can retain user-created prototype objects.
    pub fn function_prototype_overrides_for_trace(&self) -> impl Iterator<Item = &Value> {
        self.function_prototype_overrides.values()
    }

    pub(crate) fn set_function_prototype_override(&mut self, value: &Value, proto: Option<Value>) {
        let function_id = value.as_function().or_else(|| {
            value
                .as_closure(&self.gc_heap)
                .map(|closure| closure.cached_function_id)
        });
        let Some(function_id) = function_id else {
            return;
        };
        if let Some(proto) = proto {
            self.function_prototype_overrides.insert(function_id, proto);
        } else {
            self.function_prototype_overrides.remove(&function_id);
        }
    }

    /// Iterator over cached per-kind iterator prototypes.
    pub fn iterator_prototypes_for_trace(&self) -> impl Iterator<Item = &JsObject> {
        [
            self.array_iterator_prototype.as_ref(),
            self.map_iterator_prototype.as_ref(),
            self.set_iterator_prototype.as_ref(),
            self.string_iterator_prototype.as_ref(),
            self.regexp_string_iterator_prototype.as_ref(),
            self.iterator_helper_prototype.as_ref(),
            self.wrap_for_valid_iterator_prototype.as_ref(),
        ]
        .into_iter()
        .flatten()
    }

    /// Iterator over non-GC exotic prototype override values.
    /// Used by the GC root walker because the side table can retain
    /// subclass prototype objects for `ArrayBuffer`, `DataView`, and
    /// `TypedArray` instances.
    pub fn non_gc_exotic_prototype_overrides_for_trace(&self) -> impl Iterator<Item = &Value> {
        self.non_gc_exotic_prototype_overrides.values()
    }

    /// Borrow the GC-managed shape side tables for root tracing.
    #[must_use]
    pub(crate) fn shape_runtime_for_trace(&self) -> &object::ShapeRuntime {
        &self.shape_runtime
    }

    /// Borrow store-property ICs for root tracing of cached GC shape handles.
    pub(crate) fn store_property_ics_for_trace(
        &self,
    ) -> &[property_ic::PropertyIcEntry<property_ic::StorePropertyIc>] {
        &self.store_property_ics
    }

    /// Empty GC-managed hidden-class root.
    #[must_use]
    pub(crate) fn shape_root(&self) -> object::ShapeHandle {
        self.shape_runtime.root()
    }

    /// Return the GC-managed child shape for appending `key` to `parent`.
    #[cfg(test)]
    pub(crate) fn shape_child(
        &mut self,
        parent: object::ShapeHandle,
        key: &str,
    ) -> Result<object::ShapeHandle, VmError> {
        let roots = self.collect_runtime_roots_without_shape_runtime();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
        };
        self.shape_runtime
            .child_with_roots(&mut self.gc_heap, parent, key, &mut external_visit)
            .map_err(VmError::from)
    }

    fn shape_child_rooting_object_value(
        &mut self,
        parent: object::ShapeHandle,
        key: &str,
        obj: &mut object::JsObject,
        value: &Value,
    ) -> Result<object::ShapeHandle, VmError> {
        let mut no_extra_roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        self.shape_child_rooting_object_value_with_extra_roots(
            parent,
            key,
            obj,
            value,
            &mut no_extra_roots,
        )
    }

    fn shape_child_rooting_object_value_with_extra_roots(
        &mut self,
        parent: object::ShapeHandle,
        key: &str,
        obj: &mut object::JsObject,
        value: &Value,
        extra_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
    ) -> Result<object::ShapeHandle, VmError> {
        let roots = self.collect_runtime_roots_without_shape_runtime();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            extra_visit(visitor);
            for &slot in &roots {
                visitor(slot);
            }
            let p = obj as *mut object::JsObject as *mut RawGc;
            visitor(p);
            value.trace_value_slots(visitor);
        };
        self.shape_runtime
            .child_with_roots(&mut self.gc_heap, parent, key, &mut external_visit)
            .map_err(VmError::from)
    }

    fn shape_child_rooting_object_descriptor(
        &mut self,
        parent: object::ShapeHandle,
        key: &str,
        obj: &mut object::JsObject,
        descriptor: &object::PropertyDescriptor,
    ) -> Result<object::ShapeHandle, VmError> {
        let roots = self.collect_runtime_roots_without_shape_runtime();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            let p = obj as *mut object::JsObject as *mut RawGc;
            visitor(p);
            match &descriptor.kind {
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
        };
        self.shape_runtime
            .child_with_roots(&mut self.gc_heap, parent, key, &mut external_visit)
            .map_err(VmError::from)
    }

    fn should_add_property(&mut self, obj: object::JsObject, key: &str) -> bool {
        let shape = object::shape(obj, &self.gc_heap);
        !shape.is_null()
            && object::is_extensible(obj, &self.gc_heap)
            && matches!(
                object::lookup_own(obj, &self.gc_heap, key),
                object::PropertyLookup::Absent
            )
            && self.shape_offset_of(shape, key).is_none()
    }

    fn update_array_prototype_length_after_index_store(
        &mut self,
        obj: object::JsObject,
        key: &str,
    ) {
        if self.realm_intrinsics.array_prototype != Some(obj) {
            return;
        }
        let Some(index) = object::array_index_property_name(key) else {
            return;
        };
        let new_len = f64::from(index) + 1.0;
        let current = object::get(obj, &self.gc_heap, "length")
            .and_then(|value| value.as_number())
            .map(|number| number.as_f64())
            .unwrap_or(0.0);
        if new_len > current {
            object::set(
                obj,
                &mut self.gc_heap,
                "length",
                Value::number(NumberValue::from_f64(new_len)),
            );
        }
    }

    /// Descriptor-aware data assignment that advances the object's GC-managed
    /// hidden class when a new own data property is created.
    pub(crate) fn ordinary_set_data_property(
        &mut self,
        mut obj: object::JsObject,
        key: &str,
        value: Value,
    ) -> Result<bool, VmError> {
        let shape = object::shape(obj, &self.gc_heap);
        // Past the fast-property cap, stop extending the transition
        // chain and let `object::ordinary_set_data_property` normalize
        // the object to dictionary storage (shape → null). Otherwise a
        // growing chain makes every lookup O(n) and bulk addition
        // O(n²).
        let should_add_shape = self.should_add_property(obj, key)
            && object::shape_property_count(shape, &self.gc_heap) < object::MAX_FAST_PROPERTIES;
        let next_shape = if should_add_shape {
            Some(self.shape_child_rooting_object_value(shape, key, &mut obj, &value)?)
        } else {
            None
        };

        let ok = if let Some(next_shape) = next_shape {
            object::ordinary_set_data_property_with_shape(
                obj,
                &mut self.gc_heap,
                key,
                value,
                next_shape,
            )
        } else {
            object::ordinary_set_data_property(obj, &mut self.gc_heap, key, value)
        };
        if ok {
            self.update_array_prototype_length_after_index_store(obj, key);
        }
        Ok(ok)
    }

    /// Construction-time data store that advances the object's GC-managed
    /// hidden class when a new own data property is created.
    pub(crate) fn set_property(
        &mut self,
        mut obj: object::JsObject,
        key: &str,
        value: Value,
    ) -> Result<(), VmError> {
        let shape = object::shape(obj, &self.gc_heap);
        let should_add_shape = self.should_add_property(obj, key)
            && object::shape_property_count(shape, &self.gc_heap) < object::MAX_FAST_PROPERTIES;
        let next_shape = if should_add_shape {
            Some(self.shape_child_rooting_object_value(shape, key, &mut obj, &value)?)
        } else {
            None
        };

        if let Some(next_shape) = next_shape {
            object::set_with_shape(obj, &mut self.gc_heap, key, value, next_shape);
        } else {
            object::set(obj, &mut self.gc_heap, key, value);
        }
        self.update_array_prototype_length_after_index_store(obj, key);
        Ok(())
    }

    /// Construction-time data store with caller-supplied roots for native
    /// binding contexts that hold live values outside VM frames.
    pub(crate) fn set_property_with_extra_roots(
        &mut self,
        mut obj: object::JsObject,
        key: &str,
        value: Value,
        extra_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
    ) -> Result<(), VmError> {
        let shape = object::shape(obj, &self.gc_heap);
        let should_add_shape = self.should_add_property(obj, key)
            && object::shape_property_count(shape, &self.gc_heap) < object::MAX_FAST_PROPERTIES;
        let next_shape = if should_add_shape {
            Some(self.shape_child_rooting_object_value_with_extra_roots(
                shape,
                key,
                &mut obj,
                &value,
                extra_visit,
            )?)
        } else {
            None
        };

        if let Some(next_shape) = next_shape {
            object::set_with_shape(obj, &mut self.gc_heap, key, value, next_shape);
        } else {
            object::set(obj, &mut self.gc_heap, key, value);
        }
        self.update_array_prototype_length_after_index_store(obj, key);
        Ok(())
    }

    /// Field-presence-aware defineProperty path that advances the object's
    /// GC-managed hidden class when a new own property is created.
    pub(crate) fn define_own_property_partial(
        &mut self,
        mut obj: object::JsObject,
        key: &str,
        descriptor: object::PartialPropertyDescriptor,
    ) -> Result<bool, VmError> {
        let completed = descriptor.complete_for_new_property();
        let shape = object::shape(obj, &self.gc_heap);
        let should_add_shape = self.should_add_property(obj, key)
            && object::shape_property_count(shape, &self.gc_heap) < object::MAX_FAST_PROPERTIES;
        let next_shape = if should_add_shape {
            Some(self.shape_child_rooting_object_descriptor(shape, key, &mut obj, &completed)?)
        } else {
            None
        };

        let ok = if let Some(next_shape) = next_shape {
            object::define_own_property_partial_with_shape(
                obj,
                &mut self.gc_heap,
                key,
                descriptor,
                next_shape,
            )
        } else {
            object::define_own_property_partial(obj, &mut self.gc_heap, key, descriptor)
        };
        Ok(ok)
    }

    /// Look up a property slot in a GC-managed hidden-class shape.
    #[must_use]
    pub(crate) fn shape_offset_of(&mut self, shape: object::ShapeHandle, key: &str) -> Option<u32> {
        self.shape_runtime.offset_of(&self.gc_heap, shape, key)
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

    /// Borrow the iteration-anchor stack for GC root tracing.
    #[must_use]
    pub(crate) fn iteration_anchors_for_trace(&self) -> &[Value] {
        &self.iteration_anchors
    }

    /// Push a value onto the iteration-anchor stack. Returns the
    /// new stack depth so the matching pop can sanity-check.
    pub(crate) fn push_iteration_anchor(&mut self, value: Value) -> usize {
        self.iteration_anchors.push(value);
        self.iteration_anchors.len()
    }

    /// Pop entries back down to the depth captured at push time.
    pub(crate) fn pop_iteration_anchors_to(&mut self, depth: usize) {
        self.iteration_anchors.truncate(depth);
    }

    /// Overwrite an existing iteration-anchor slot. Used by loops that
    /// carry a *mutating* rooted value (an accumulator, the current
    /// element) across a reentrant callback: the slot is refreshed
    /// before each callback so a moving scavenge rewrites the live
    /// value, and read back afterwards via [`Self::iteration_anchor`].
    pub(crate) fn set_iteration_anchor(&mut self, index: usize, value: Value) {
        self.iteration_anchors[index] = value;
    }

    /// Read an iteration-anchor slot back after a reentrant callback —
    /// a moving scavenge rewrites the slot in place, so this returns the
    /// relocated handle.
    #[must_use]
    pub(crate) fn iteration_anchor(&self, index: usize) -> Value {
        self.iteration_anchors[index]
    }

    /// Root a value for the duration of an out-of-crate builder (e.g. the
    /// runtime's module `ModuleScope`). Backed by the iteration-anchor stack,
    /// which the GC traces and rewrites in place, so the rooted value survives a
    /// moving scavenge triggered by a later allocation. Returns the new depth;
    /// pass it (minus one) to read the value back via [`Self::module_root`].
    ///
    /// Because the GC moves, a `Value` copy held across an allocation is stale —
    /// re-read it with [`Self::module_root`] after any allocation, and balance
    /// every push with [`Self::pop_module_roots_to`].
    pub fn push_module_root(&mut self, value: Value) -> usize {
        self.push_iteration_anchor(value)
    }

    /// Current module-root stack depth. Capture before a build and pass to
    /// [`Self::pop_module_roots_to`] to release everything pushed since.
    #[must_use]
    pub fn module_root_depth(&self) -> usize {
        self.iteration_anchors.len()
    }

    /// Pop module roots back to a depth previously returned by
    /// [`Self::push_module_root`] / [`Self::module_root_depth`]. Must be called
    /// to balance the pushes.
    pub fn pop_module_roots_to(&mut self, depth: usize) {
        self.pop_iteration_anchors_to(depth);
    }

    /// Read a module-root slot back after an allocation/reentry — the moving GC
    /// rewrites the slot in place, so this returns the relocated handle.
    #[must_use]
    pub fn module_root(&self, index: usize) -> Value {
        self.iteration_anchor(index)
    }

    /// Overwrite a module-root slot (for a value mutated across allocations).
    pub fn set_module_root(&mut self, index: usize, value: Value) {
        self.set_iteration_anchor(index, value);
    }

    /// Consume the pending uncaught-throw payload, if any. Embedder
    /// callers that catch a `VmError::Uncaught` at a sync entry
    /// point use this to recover the original thrown
    /// [`Value`] (an `Error` instance, a string, etc.) instead of
    /// the lossy `Display` rendering carried by the `VmError`.
    pub fn take_pending_uncaught_throw(&mut self) -> Option<Value> {
        self.pending_uncaught_throw.take()
    }

    /// Stash a [`Value`] on the pending-uncaught-throw side channel
    /// so the surrounding microtask drain / sync entry point can
    /// surface the original [[Value]] verbatim after the native
    /// returns [`NativeError::Thrown`]. The pairing with
    /// `NativeError::Thrown` (which carries only a display rendering)
    /// preserves identity per §27.2.1.3.2 step 1.f.iii for natives
    /// that need to re-throw a JS value verbatim — such as the
    /// `thrower` function CreateCatchFinally(C, onFinally) installs.
    pub(crate) fn set_pending_uncaught_throw(&mut self, value: Value) {
        self.pending_uncaught_throw = Some(value);
    }

    /// Borrow the cold record attached to `frame`, if any.
    #[inline]
    #[must_use]
    pub(crate) fn frame_cold(&self, frame: &Frame) -> Option<&cold_frame::ColdFrame> {
        frame.cold.map(|idx| self.cold_frames.get(idx))
    }

    /// Mutable borrow of the cold record attached to `frame`, if any.
    #[inline]
    #[must_use]
    pub(crate) fn frame_cold_mut(
        &mut self,
        frame: &mut Frame,
    ) -> Option<&mut cold_frame::ColdFrame> {
        frame.cold.map(|idx| self.cold_frames.get_mut(idx))
    }

    /// Acquire a cold record for `frame` if it doesn't have one yet,
    /// then return a mutable borrow.
    #[inline]
    pub(crate) fn frame_ensure_cold(&mut self, frame: &mut Frame) -> &mut cold_frame::ColdFrame {
        let idx = match frame.cold {
            Some(idx) => idx,
            None => {
                let idx = self.cold_frames.acquire();
                frame.cold = Some(idx);
                idx
            }
        };
        self.cold_frames.get_mut(idx)
    }

    /// Release `frame`'s cold record back to the pool if it holds one.
    /// Called when a frame is popped off the dispatcher stack.
    #[inline]
    pub(crate) fn frame_release_cold(&mut self, frame: &mut Frame) {
        if let Some(idx) = frame.cold.take() {
            self.cold_frames.release(idx);
        }
    }

    /// Detach `frame`'s cold record out of the pool, returning it as
    /// an owned [`Box`] so the caller can store it alongside the
    /// parked frame (async await, generator yield). Returns `None`
    /// when the frame had no cold state.
    #[inline]
    pub(crate) fn frame_detach_cold(
        &mut self,
        frame: &mut Frame,
    ) -> Option<Box<cold_frame::ColdFrame>> {
        let idx = frame.cold.take()?;
        Some(Box::new(self.cold_frames.detach(idx)))
    }

    /// Re-attach an owned cold record into the pool and bind it to
    /// `frame`. Matches [`Self::frame_detach_cold`] on the resume path.
    #[inline]
    pub(crate) fn frame_attach_cold(
        &mut self,
        frame: &mut Frame,
        cold: Box<cold_frame::ColdFrame>,
    ) {
        let idx = self.cold_frames.attach(*cold);
        frame.cold = Some(idx);
    }

    /// Borrow the per-interpreter cold-frame pool.
    #[inline]
    #[must_use]
    pub(crate) fn cold_frames(&self) -> &cold_frame::ColdFramePool {
        &self.cold_frames
    }

    /// Borrow the per-realm typed intrinsic slots.
    #[inline]
    #[must_use]
    pub(crate) fn realm_intrinsics(&self) -> &realm_intrinsics::RealmIntrinsics {
        &self.realm_intrinsics
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

    /// Force a full GC cycle. Runtime-owned roots are supplied through the
    /// heap's [`otter_gc::ExtraRoots`] callback so explicit GC and
    /// allocation-triggered GC use the same root walk.
    ///
    /// **Debug / test only** — production embedders let the GC
    /// trigger itself.
    pub fn force_gc(&mut self) {
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let extra_root_depth = self.gc_heap.push_extra_roots(extra_roots);
        let mut noop = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        self.gc_heap.mark_phase(&mut noop);
        crate::collections::run_ephemeron_fixpoint(&mut self.gc_heap);
        let finalization_jobs =
            crate::weak_refs::process_weak_refs_and_finalizers(&mut self.gc_heap);
        for job in finalization_jobs {
            let mut args = SmallVec::new();
            args.push(job.held_value);
            self.microtasks.enqueue(Microtask {
                callee: job.cleanup_callback,
                this_value: Value::undefined(),
                args,
                context: job.context,
                result_capability: None,
                kind: MicrotaskKind::FinalizationCallback,
            });
        }
        self.gc_heap.sweep_phase();
        self.gc_heap.pop_extra_roots_to(extra_root_depth - 1);
    }

    /// Link a freshly compiled module into this interpreter's code
    /// space. Rebases the module's function ids onto the global id
    /// space so function values created by this chunk stay callable
    /// after they escape to frames executing other chunks (the
    /// `eval` / `new Function` / dynamic-import escape paths).
    pub fn link_module(&mut self, module: otter_bytecode::BytecodeModule) -> ExecutionContext {
        code_space::CodeSpace::link(&self.code_space, module)
    }

    /// Execute `<main>` of `module` and return its completion value.
    ///
    /// # Errors
    /// Returns [`RunError`] (a `VmError` plus a stack-frame
    /// snapshot) on bytecode malformation, type mismatch, OOM,
    /// interrupt, or stack overflow.
    pub fn run(&mut self, context: &ExecutionContext) -> Result<Value, RunError> {
        // Adopt the entry chunk's code space so chunks linked during
        // this run (eval / new Function bodies) land in the same
        // function-id space as the running script. No-op for contexts
        // produced by `link_module`.
        if !std::sync::Arc::ptr_eq(&self.code_space, context.space()) {
            self.code_space = std::sync::Arc::clone(context.space());
        }
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let extra_root_depth = self.gc_heap.push_extra_roots(extra_roots);
        self.pending_uncaught_throw = None;
        self.pending_uncaught_frames = None;
        self.ensure_property_ic_capacity(context);
        let result = match self.run_inner(context) {
            Ok(v) => Ok(v),
            Err((error, frames)) => Err(RunError { error, frames }),
        };
        self.gc_heap.pop_extra_roots_to(extra_root_depth - 1);
        result
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
        // The drain runs outside `Interpreter::run`'s rooted scope
        // (the runtime layer drains after `run` returns), so register
        // the interpreter's runtime roots here. Without this, a
        // scavenge triggered by any allocation in a microtask body —
        // including async-resume parked frames and queued reaction
        // values — would miss every root enumerated by
        // [`crate::runtime_state::RuntimeState`] (shape side tables,
        // the microtask queue itself, globalThis, module envs) and
        // free or move objects still reachable through them.
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let extra_root_depth = self.gc_heap.push_extra_roots(extra_roots);
        let result = self.drain_microtasks_with_default_inner(default_context);
        self.gc_heap.pop_extra_roots_to(extra_root_depth - 1);
        result
    }

    fn drain_microtasks_with_default_inner(
        &mut self,
        default_context: Option<ExecutionContext>,
    ) -> Result<(), RunError> {
        self.record_runtime_microtask_drain_started();
        let mut iters: u32 = 0;
        let mut observed_microtask_budget = false;
        loop {
            let Some(batch_len) = self.microtasks.begin_drain() else {
                return Ok(());
            };
            if batch_len == 0 {
                self.microtasks.end_drain();
                return Ok(());
            }
            // Tasks stay queue-owned (`next_in_flight`) rather than
            // being moved into a driver-local batch, so the ones
            // waiting behind the executing task remain visible to
            // the GC root walk — parked async frames in the queue
            // hold raw register slots a scavenge must rewrite.
            while let Some(task) = self.microtasks.next_in_flight() {
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
            cold,
            await_dst,
            fulfilled,
        } = task.kind
        {
            let value = task.args.into_iter().next().unwrap_or(Value::undefined());
            return self.run_async_resume(context, frame, cold, await_dst, fulfilled, value);
        }
        if let MicrotaskKind::AsyncGenResume {
            frame,
            cold,
            await_dst,
            fulfilled,
            owner,
        } = task.kind
        {
            let value = task.args.into_iter().next().unwrap_or(Value::undefined());
            return self
                .run_async_gen_resume(context, frame, cold, await_dst, fulfilled, value, owner);
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
            if let Some(bound) = current.as_bound_function() {
                hops += 1;
                let (target, bound_this, bound_args) = bound.parts(&self.gc_heap);
                let mut combined: SmallVec<[Value; 8]> =
                    SmallVec::with_capacity(bound_args.len() + effective_args.len());
                combined.extend(bound_args);
                combined.extend(effective_args);
                effective_this = bound_this;
                effective_args = combined;
                current = target;
            } else if let Some(cc) = current.as_class_constructor() {
                hops += 1;
                current = cc.ctor(&self.gc_heap);
            } else {
                break;
            }
        }
        // Native callables run inline at the drain site: no frame
        // push, no return register. Errors propagate as RunError.
        if let Some(native) = current.as_native_function() {
            let native = &native;
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
                            let reason = vm_err_to_value(&vm_err, &mut self.gc_heap);
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
            let call_info = NativeCallInfo::call(effective_this);
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
                        // than aborting the drain. If a sub-dispatch
                        // (e.g. `run_callable_sync` from within the
                        // native body) caught a user `throw`, the
                        // original `Value` was stashed on
                        // `pending_uncaught_throw` — prefer it over a
                        // stringified `vm_err_to_value` rendering so
                        // identity is preserved per §27.2.1.3.2 step
                        // 1.f.iii.
                        let reason = self
                            .pending_uncaught_throw
                            .take()
                            .unwrap_or_else(|| vm_err_to_value(&vm_err, &mut self.gc_heap));
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
        let (
            function_id,
            parent_upvalues,
            this_for_callee,
            _new_target_for_callee,
            _derived_this_cell,
            _callee_env,
        ) = match Self::bytecode_call_target_parts(current, effective_this, &self.gc_heap) {
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
        self.bind_bytecode_call_arguments(function, &mut new_frame, effective_args)
            .map_err(|error| RunError {
                error,
                frames: Vec::new(),
            })?;
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
                        .unwrap_or_else(|| vm_err_to_value(&error, &mut self.gc_heap));
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
            this_value: Value::undefined(),
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
            Value::undefined()
        } else {
            Value::object(self.global_this)
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
                                    value: self.render_thrown(&reason),
                                },
                                Vec::new(),
                            ));
                        }
                        crate::promise::PromiseState::Pending => return Ok(Value::undefined()),
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
        let frame_roots = otter_gc::RawFrameRoots::new(
            stack as *const SmallVec<[Frame; 8]>,
            &self.cold_frames as *const cold_frame::ColdFramePool,
            trace_active_frame_roots,
        );
        let frame_root_provider: &dyn otter_gc::FrameRoots = &frame_roots;
        let frame_root_depth = self
            .gc_heap
            .push_frame_roots(frame_root_provider as *const dyn otter_gc::FrameRoots);
        // Catch-all runtime-roots registration: every bytecode tick
        // can allocate, and some dispatch entries (generator
        // prologues spawned from host-driven drains, future embedder
        // entry points) reach here without an enclosing rooted scope.
        // The heap dedupes same-source stack entries, so re-pushing
        // under `run` / `run_callable_sync` costs one Vec slot.
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let extra_root_depth = self.gc_heap.push_extra_roots(extra_roots);
        // Nested dispatch must not leak its last-instruction byte length
        // into the caller's PC advance: helpers like Op::Eval invoke
        // dispatch_loop on a sub-stack and then expect
        // self.current_byte_len to still describe the *outer* opcode
        // when they call frame.advance_pc(self.current_byte_len).
        let saved_byte_len = self.current_byte_len;
        let result = (|| -> Result<Value, VmError> {
            loop {
                match self.dispatch_loop_inner(context, stack) {
                    Ok(value) => break Ok(value),
                    Err(err) => {
                        if matches!(err, VmError::Uncaught { .. })
                            && !stack.is_empty()
                            && let Some(thrown) = self.pending_uncaught_throw.take()
                        {
                            self.pending_uncaught_frames = Some(snapshot_frames(context, stack));
                            let unwind = self.unwind_throw(context, stack, thrown);
                            if unwind.is_ok() {
                                self.pending_uncaught_frames = None;
                            } else {
                                // No handler in THIS dispatch stack —
                                // restore the original thrown value so
                                // an outer dispatch loop (across a
                                // native boundary) can still unwind
                                // with identity intact instead of the
                                // rendered string.
                                self.pending_uncaught_throw = Some(thrown);
                            }
                            unwind?;
                            if stack.is_empty() {
                                break Ok(Value::undefined());
                            }
                            continue;
                        }
                        if let Some(thrown) =
                            self.vm_error_to_throwable_with_stack_roots(stack, &err)
                        {
                            let uncaught = if matches!(
                                err,
                                VmError::OutOfMemory { .. } | VmError::JsonError { .. }
                            ) {
                                Some(err.clone())
                            } else {
                                None
                            };
                            self.pending_uncaught_frames = Some(snapshot_frames(context, stack));
                            let unwind =
                                self.unwind_throw_with_uncaught(context, stack, thrown, uncaught);
                            if unwind.is_ok() {
                                self.pending_uncaught_frames = None;
                            }
                            unwind?;
                            if stack.is_empty() {
                                break Ok(Value::undefined());
                            }
                            continue;
                        }
                        break Err(err);
                    }
                }
            }
        })();
        self.gc_heap.pop_extra_roots_to(extra_root_depth - 1);
        self.gc_heap.pop_frame_roots_to(frame_root_depth - 1);
        self.finish_runtime_budget_turn();
        self.current_byte_len = saved_byte_len;
        result
    }

    fn dispatch_loop_inner(
        &mut self,
        entry_context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
    ) -> Result<Value, VmError> {
        // One stack can interleave frames from several code chunks
        // (closures escaped from `eval` / `new Function` / sibling
        // scripts), so each iteration dispatches against the chunk
        // owning the *top frame*: constants, atoms, and module
        // resolutions are chunk-local. The owned slot caches the last
        // foreign chunk so repeated foreign-frame ticks don't re-lock
        // the code-space registry.
        let mut foreign_context: Option<ExecutionContext> = None;
        // Hoisted once per turn: the budget config does not change mid-turn,
        // so the per-op checkpoint only needs to run when enforcement is on.
        // In the default Observe mode this collapses to a not-taken branch.
        let enforce_budget = self.runtime_budget.rejects_on_exceedance();
        loop {
            if self.interrupt.is_set() {
                return Err(VmError::Interrupted);
            }
            if stack.is_empty() {
                // Defensive: unwind paths (throw / finally) can
                // pop the last frame without writing back to a
                // caller register. Surface `undefined` so
                // the dispatch loop terminates cleanly instead of
                // panicking on the next `stack.len() - 1`. Tests
                // that rely on the throw escape will already have
                // flowed through `unwind_throw` and surfaced as
                // `VmError::Uncaught`; this guard catches the
                // residual "fell off the bottom" path and treats
                // it as completion.
                return Ok(Value::undefined());
            }
            let top_idx = stack.len() - 1;
            let function_id = stack[top_idx].function_id;
            let context: &ExecutionContext = if entry_context.covers_function(function_id) {
                entry_context
            } else {
                let cached_covers = foreign_context
                    .as_ref()
                    .is_some_and(|c| c.covers_function(function_id));
                if !cached_covers {
                    foreign_context = match entry_context.for_function(function_id) {
                        Some(code_space::ResolvedCtx::Owned(owned)) => {
                            // Foreign chunks linked after this loop
                            // started (eval during this turn) carry
                            // IC sites past the entry chunk's range.
                            self.ensure_property_ic_capacity(&owned);
                            Some(owned)
                        }
                        _ => None,
                    };
                }
                foreign_context.as_ref().ok_or(VmError::InvalidOperand)?
            };
            let function = context
                .exec_function(function_id)
                .ok_or(VmError::InvalidOperand)?;
            let pc = stack[top_idx].pc;
            let instr = function
                .instr_at_byte_pc(pc)
                .ok_or(VmError::MissingReturn)?;
            let op = instr.op();
            self.current_byte_len = instr.byte_len();
            // Inlined runtime metering on the dispatch hot path. The three
            // former per-op method calls collapse into `record_reductions`
            // (`#[inline]`), an inlined monotonic stack-depth max, and a
            // budget checkpoint gated on `enforce_budget`. Semantics are
            // exact — reductions accumulate identically and max stack depth
            // is unchanged; the checkpoint is skipped only in Observe mode
            // (where it was already a cheap early return).
            {
                let units = runtime_budget::opcode_reductions(op);
                let stats = &mut self.runtime_budget_stats;
                stats.record_reductions(units);
                let depth = u32::try_from(stack.len()).unwrap_or(u32::MAX);
                if depth > stats.max_stack_depth_observed {
                    stats.max_stack_depth_observed = depth;
                }
            }
            if enforce_budget {
                self.enforce_runtime_budget_checkpoint()?;
            }

            // Step-trace hook. The hot path checks one `Option` slot
            // per instruction; the body only runs when an embedder
            // installed a tracer through `Interpreter::set_tracer`.
            if self.tracer.is_some() {
                let function_name = context
                    .function(function_id)
                    .map(|f| f.name.as_str())
                    .unwrap_or("<unknown>");
                let operands = context.exec_operands(instr);
                let register_window = stack[top_idx].registers.as_slice();
                let event = inspect::StepEvent {
                    frame_depth: stack.len(),
                    function_id,
                    function_name,
                    byte_pc: pc,
                    op,
                    operands,
                    register_window,
                };
                if let Some(tracer) = self.tracer.as_deref_mut() {
                    tracer.on_step(&event);
                }
            }

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
                    if let Some(popped) = self.return_running_finally(stack, value)? {
                        return Ok(popped);
                    }
                    continue;
                }
                Op::ReturnUndefined => {
                    if let Some(popped) = self.return_running_finally(stack, Value::undefined())? {
                        return Ok(popped);
                    }
                    continue;
                }
                Op::Call => {
                    let operands = context.exec_operands(instr);
                    let depth_before = stack.len();
                    self.do_call(stack, context, operands)?;
                    // Tier-up hook: only when a bytecode callee frame was just
                    // pushed and a JIT is installed. Cheap (one bool) when off.
                    if self.jit_hook.is_some()
                        && stack.len() > depth_before
                        && let Some(Some(value)) = self.maybe_dispatch_jit(stack, context)?
                    {
                        return Ok(value);
                    }
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
                Op::SuperConstructSpread => {
                    let operands = context.exec_operands(instr);
                    self.do_super_construct_spread(stack, context, operands)?;
                    continue;
                }
                Op::BindThisValue => {
                    let src = register_operand(context.exec_operand(instr, 0))?;
                    let value = *read_register(&stack[top_idx], src)?;
                    // §13.3.7.2 — super() may execute inside an arrow
                    // (its this/super are lexical); the binding
                    // belongs to the nearest derived-constructor
                    // environment, i.e. the closest derived-ctor
                    // frame below the call site.
                    let target = (0..=top_idx).rev().find(|&i| {
                        self.frame_cold(&stack[i])
                            .is_some_and(|c| c.is_derived_constructor)
                    });
                    if let Some(ti) = target {
                        if !stack[ti].this_value.is_hole() {
                            return Err(VmError::ThisUninitialized {
                                message: "super constructor may only be called once".to_string(),
                            });
                        }
                        stack[ti].this_value = value;
                        let frame = &mut stack[ti];
                        let derived_this_cell = self
                            .frame_cold(frame)
                            .and_then(|cold| cold.derived_this_cell);
                        if let Some(cell) = derived_this_cell {
                            crate::store_upvalue(&mut self.gc_heap, cell, value);
                        }
                        if let Some(obj) = value.as_object() {
                            let cold = self.frame_ensure_cold(frame);
                            cold.construct_target = Some(obj);
                        }
                    } else {
                        let derived_this_cell = self
                            .frame_cold(&stack[top_idx])
                            .and_then(|cold| cold.derived_this_cell);
                        let Some(cell) = derived_this_cell else {
                            return Err(VmError::ThisUninitialized {
                                message: "super called outside a derived constructor".to_string(),
                            });
                        };
                        if !crate::read_upvalue(&self.gc_heap, cell).is_hole() {
                            return Err(VmError::ThisUninitialized {
                                message: "super constructor may only be called once".to_string(),
                            });
                        }
                        crate::store_upvalue(&mut self.gc_heap, cell, value);
                    }
                    stack[top_idx].advance_pc(self.current_byte_len)?;
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
                    let unwind = self.unwind_throw(context, stack, value);
                    if unwind.is_ok() {
                        self.pending_uncaught_frames = None;
                    } else {
                        // No handler in this dispatch stack — stash
                        // the thrown VALUE so outer loops / native
                        // boundaries keep identity instead of the
                        // rendered string.
                        self.pending_uncaught_throw = Some(value);
                    }
                    unwind?;
                    continue;
                }
                Op::EndFinally => {
                    let parked = self
                        .frame_cold_mut(&mut stack[top_idx])
                        .and_then(|c| c.parked_finally.pop());
                    match parked {
                        Some((crate::cold_frame::ParkedFinally::Throw(value), _)) => {
                            self.pending_uncaught_frames = Some(snapshot_frames(context, stack));
                            let unwind = self.unwind_throw(context, stack, value);
                            if unwind.is_ok() {
                                self.pending_uncaught_frames = None;
                            } else {
                                self.pending_uncaught_throw = Some(value);
                            }
                            unwind?;
                        }
                        Some((crate::cold_frame::ParkedFinally::Abrupt(completion, floor), _)) => {
                            // Resume the parked `return`/`break`/`continue`:
                            // run the next enclosing `finally`, or perform
                            // the completion when none remain.
                            if let Some(popped) = self.unwind_abrupt(stack, completion, floor)? {
                                return Ok(popped);
                            }
                        }
                        Some((crate::cold_frame::ParkedFinally::Normal, _)) | None => {
                            stack[top_idx].advance_pc(self.current_byte_len)?;
                        }
                    }
                    continue;
                }
                Op::Await => {
                    let dst = register_operand(context.exec_operand(instr, 0))?;
                    let src = register_operand(context.exec_operand(instr, 1))?;
                    let awaited = *read_register(&stack[top_idx], src)?;
                    self.do_await(stack, context, dst, awaited)?;
                    if stack.is_empty() {
                        return Ok(Value::undefined());
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
                // §27.5.3.7 `yield*` delegating suspension — parks
                // the frame with the inner iterator result surfaced
                // verbatim; resume delivers (kind, value) into the
                // two destination registers without unwinding.
                Op::YieldDelegate => {
                    let (kind_dst, value_dst, src) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let yielded = *read_register(&stack[top_idx], src)?;
                    let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                    let owner = frame.generator_owner.ok_or(VmError::TypeMismatch)?;
                    frame.advance_pc(self.current_byte_len)?;
                    let mut popped = stack.pop().expect("frame present");
                    let detached_cold = self.frame_detach_cold(&mut popped);
                    owner.park_after_yield_delegate(
                        &mut self.gc_heap,
                        popped,
                        detached_cold,
                        kind_dst,
                        value_dst,
                        yielded,
                    );
                    return Ok(Value::undefined());
                }
                Op::Yield => {
                    let dst = register_operand(context.exec_operand(instr, 0))?;
                    let src = register_operand(context.exec_operand(instr, 1))?;
                    let yielded = *read_register(&stack[top_idx], src)?;
                    let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                    let owner = frame.generator_owner.ok_or(VmError::TypeMismatch)?;
                    frame.advance_pc(self.current_byte_len)?;
                    let mut popped = stack.pop().expect("frame present");
                    let detached_cold = self.frame_detach_cold(&mut popped);
                    owner.park_after_yield(&mut self.gc_heap, popped, detached_cold, dst, yielded);
                    // §27.6 — async-generator yield settles the
                    // outer `.next()` promise immediately with
                    // `{value, done: false}`. Sync generators bubble
                    // the yielded value out so the `resume_generator`
                    // caller can shape it.
                    if owner.is_async(&self.gc_heap) {
                        owner.set_async_state(
                            &mut self.gc_heap,
                            crate::generator::AsyncGeneratorState::SuspendedYield,
                        );
                        self.async_generator_complete_step(context, &owner, Ok(yielded), false)?;
                    }
                    return Ok(yielded);
                }
                Op::GeneratorStart => {
                    let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                    let owner = frame.generator_owner.ok_or(VmError::TypeMismatch)?;
                    frame.advance_pc(self.current_byte_len)?;
                    let mut popped = stack.pop().expect("frame present");
                    let detached_cold = self.frame_detach_cold(&mut popped);
                    owner.park_frame(&mut self.gc_heap, popped, detached_cold);
                    return Ok(Value::undefined());
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
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    self.run_get_iterator_regs(&mut *stack, top_idx, dst, src)?;
                    continue;
                }
                Op::GetAsyncIterator => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    self.run_get_async_iterator_regs(context, &mut *stack, top_idx, dst, src)?;
                    continue;
                }
                // §7.4.5 `IteratorNext`. Built-in iterators step
                // synchronously; user iterators push a call to
                // `iter.next()` and resume to extract `value` /
                // `done`.
                // <https://tc39.es/ecma262/#sec-iteratornext>
                Op::IteratorNext => {
                    // §7.4.8 IteratorStep — if `next` throws, the
                    // iterator record is set `[[done]]` and IteratorClose
                    // is *not* run for it. Deregister the iterator from
                    // the §7.4.9 closer set before propagating so the
                    // throw-unwind does not invoke `[[return]]`.
                    let iter_reg = context
                        .exec_register(instr, 2)
                        .ok_or(VmError::InvalidOperand)?;
                    let iterator = *read_register(&stack[top_idx], iter_reg)?;
                    let operands = context.exec_operands(instr);
                    match self.drive_iterator_next(stack, context, operands) {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(e) => {
                            self.deregister_frame_iterator_closer(&mut stack[top_idx], iterator);
                            return Err(e);
                        }
                    }
                    let value_dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let done_dst = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    if let Err(e) =
                        self.run_iterator_next_regs(frame, value_dst, done_dst, iter_reg)
                    {
                        self.deregister_frame_iterator_closer(&mut stack[top_idx], iterator);
                        return Err(e);
                    }
                    continue;
                }
                Op::IteratorClose => {
                    let iter_reg = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let iterator = *read_register(&stack[top_idx], iter_reg)?;
                    // §7.4.9 — mark the iterator done *before* running its
                    // `[[return]]`: if `return` throws, the unwind must
                    // not close it again (it is already closing).
                    self.deregister_frame_iterator_closer(&mut stack[top_idx], iterator);
                    self.iterator_close_value_sync(context, iterator)?;
                    stack[top_idx].advance_pc(self.current_byte_len)?;
                    continue;
                }
                Op::IteratorCloseStart => {
                    let iter_reg = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let iterator = *read_register(&stack[top_idx], iter_reg)?;
                    // §7.4.9 — record the handler depth so throw-unwind
                    // can tell whether a catching handler sits inside or
                    // outside this iterator's region.
                    let handler_depth = self
                        .frame_cold(&stack[top_idx])
                        .map_or(0, |c| c.handlers.len() as u32);
                    self.frame_ensure_cold(&mut stack[top_idx])
                        .active_iterator_closers
                        .push((iterator, handler_depth));
                    stack[top_idx].advance_pc(self.current_byte_len)?;
                    continue;
                }
                Op::IteratorCloseEnd => {
                    let iter_reg = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let iterator = *read_register(&stack[top_idx], iter_reg)?;
                    if let Some(cold) = self.frame_cold_mut(&mut stack[top_idx])
                        && let Some(pos) = cold
                            .active_iterator_closers
                            .iter()
                            .rposition(|(value, _)| *value == iterator)
                    {
                        cold.active_iterator_closers.remove(pos);
                    }
                    stack[top_idx].advance_pc(self.current_byte_len)?;
                    continue;
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
                Op::LoadElement => {
                    let operands = context.exec_operands(instr);
                    if self.drive_load_element(stack, context, operands)? {
                        continue;
                    }
                    let (dst, recv_reg, idx_reg) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_load_element_regs(context, frame, dst, recv_reg, idx_reg)?;
                    continue;
                }
                Op::LoadSuperProperty => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let home_reg = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 2)
                        .ok_or(VmError::InvalidOperand)?;
                    let name = context
                        .property_atom(name_idx)
                        .ok_or(VmError::InvalidOperand)?
                        .name();
                    let home = *read_register(&stack[top_idx], home_reg)?;
                    self.run_load_super_property(
                        context,
                        stack,
                        top_idx,
                        dst,
                        home,
                        SuperReadKey::Resolved(VmPropertyKey::String(name)),
                    )?;
                    continue;
                }
                Op::LoadSuperElement => {
                    let (dst, home_reg, key_reg) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let home = *read_register(&stack[top_idx], home_reg)?;
                    let key_raw = *read_register(&stack[top_idx], key_reg)?;
                    self.run_load_super_property(
                        context,
                        stack,
                        top_idx,
                        dst,
                        home,
                        SuperReadKey::Computed(key_raw),
                    )?;
                    continue;
                }
                Op::SetSuperProperty => {
                    let home_reg = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let value_reg = context
                        .exec_register(instr, 2)
                        .ok_or(VmError::InvalidOperand)?;
                    let name = context
                        .property_atom(name_idx)
                        .ok_or(VmError::InvalidOperand)?
                        .name();
                    let home = *read_register(&stack[top_idx], home_reg)?;
                    let value = *read_register(&stack[top_idx], value_reg)?;
                    let strict = context.function_is_strict(stack[top_idx].function_id);
                    self.run_store_super_property(
                        context,
                        stack,
                        top_idx,
                        home,
                        SuperReadKey::Resolved(VmPropertyKey::String(name)),
                        value,
                        strict,
                    )?;
                    continue;
                }
                Op::SetSuperElement => {
                    let (home_reg, key_reg, value_reg) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let home = *read_register(&stack[top_idx], home_reg)?;
                    let key_raw = *read_register(&stack[top_idx], key_reg)?;
                    let value = *read_register(&stack[top_idx], value_reg)?;
                    let strict = context.function_is_strict(stack[top_idx].function_id);
                    self.run_store_super_property(
                        context,
                        stack,
                        top_idx,
                        home,
                        SuperReadKey::Computed(key_raw),
                        value,
                        strict,
                    )?;
                    continue;
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
                Op::StoreElement => {
                    let operands = context.exec_operands(instr);
                    if self.drive_store_element(stack, context, operands)? {
                        continue;
                    }
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
                Op::Instanceof => {
                    let operands = context.exec_operands(instr);
                    if self.drive_instanceof(stack, context, operands)? {
                        continue;
                    }
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_instanceof_legacy_regs(frame, dst, lhs, rhs)?;
                    continue;
                }
                // §28.2.4.7 / .10 Proxy.[[HasProperty]] /
                // [[Delete]] — invoke `has` / `deleteProperty`
                // traps when the receiver is a Proxy.
                Op::HasProperty => {
                    let operands = context.exec_operands(instr);
                    if self.drive_has_property_proxy(stack, context, operands)? {
                        continue;
                    }
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_has_property_regs(frame, context, dst, lhs, rhs)?;
                    continue;
                }
                Op::DeleteProperty => {
                    let operands = context.exec_operands(instr);
                    if self.drive_delete_property_proxy(stack, context, operands)? {
                        continue;
                    }
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
                    let strict = context.function_is_strict(stack[top_idx].function_id);
                    // `delete` has an object fast path that bypasses the
                    // §28.3 MOP funnel; trigger deferred-namespace
                    // evaluation here (named delete is never symbol-like
                    // unless the key is "then").
                    let receiver = *read_register(&stack[top_idx], obj_reg)?;
                    if receiver.as_object().is_some_and(|o| {
                        crate::object::deferred_namespace_target(o, &self.gc_heap).is_some()
                    }) {
                        self.ensure_deferred_namespace_ready(
                            context,
                            &receiver,
                            key.name() != "then",
                        )?;
                    }
                    let frame = &mut stack[top_idx];
                    self.run_delete_property_reg(frame, dst, obj_reg, key, strict)?;
                    continue;
                }
                Op::DeleteElement => {
                    let operands = context.exec_operands(instr);
                    if self.drive_delete_element_proxy(stack, context, operands)? {
                        continue;
                    }
                    let (dst, obj_reg, idx_reg) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let strict = context.function_is_strict(stack[top_idx].function_id);
                    let receiver = *read_register(&stack[top_idx], obj_reg)?;
                    if receiver.as_object().is_some_and(|o| {
                        crate::object::deferred_namespace_target(o, &self.gc_heap).is_some()
                    }) {
                        let key_val = *read_register(&stack[top_idx], idx_reg)?;
                        let symbol_like = key_val.as_symbol(&self.gc_heap).is_some()
                            || key_val
                                .as_string(&self.gc_heap)
                                .is_some_and(|s| s.to_lossy_string(&self.gc_heap) == "then");
                        self.ensure_deferred_namespace_ready(context, &receiver, !symbol_like)?;
                    }
                    let frame = &mut stack[top_idx];
                    self.run_delete_element_regs(frame, dst, obj_reg, idx_reg, strict)?;
                    continue;
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
                    let operands = context.exec_operands(instr);
                    if self.drive_set_prototype_proxy(stack, context, operands)? {
                        continue;
                    }
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
                        let function_id = stack.last().ok_or(VmError::InvalidOperand)?.function_id;
                        let function = context
                            .exec_function(function_id)
                            .ok_or(VmError::InvalidOperand)?;
                        let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                        let elements: SmallVec<[Value; 4]> = self
                            .frame_cold_mut(frame)
                            .map(|c| std::mem::take(&mut c.incoming_args))
                            .unwrap_or_default();
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
                        let callee = Value::function(frame.function_id);
                        (
                            elements,
                            function.arguments_object_kind,
                            mapped_entries,
                            callee,
                        )
                    };
                    let iterator_method =
                        crate::object::get(self.global_this, &self.gc_heap, "Array")
                            .and_then(|v| {
                                if let Some(ctor) = v.as_object() {
                                    crate::object::get(ctor, &self.gc_heap, "prototype")
                                } else if let Some(native) = v.as_native_function() {
                                    native
                                        .own_property_descriptor(&mut self.gc_heap, "prototype")
                                        .ok()
                                        .flatten()
                                        .and_then(|d| match d.kind {
                                            crate::object::DescriptorKind::Data { value } => {
                                                Some(value)
                                            }
                                            _ => None,
                                        })
                                } else {
                                    None
                                }
                            })
                            .and_then(|v| v.as_object())
                            .and_then(|prototype| {
                                crate::object::get(prototype, &self.gc_heap, "values")
                            });
                    let iterator_symbol = self
                        .well_known_symbols
                        .get(crate::symbol::WellKnown::Iterator);
                    let iterator_root = iterator_method.unwrap_or(Value::undefined());
                    let iterator_descriptor =
                        iterator_method.map(|method| (iterator_symbol, method));
                    let obj = if kind == ArgumentsObjectKind::Mapped {
                        let obj = self.alloc_stack_rooted_object_with_value_roots(
                            stack,
                            &[&callee, &iterator_root],
                            &elements,
                        )?;
                        if let Some(proto) = self.object_prototype_object_opt() {
                            object::set_prototype(obj, &mut self.gc_heap, Some(proto));
                        }
                        crate::arguments_object::initialize_mapped(
                            obj,
                            &mut self.gc_heap,
                            elements,
                            callee,
                            mapped_entries,
                            iterator_descriptor,
                        );
                        obj
                    } else {
                        let thrower = self.restricted_throw_type_error()?;
                        let obj = self.alloc_stack_rooted_object_with_value_roots(
                            stack,
                            &[&thrower, &iterator_root],
                            &elements,
                        )?;
                        if let Some(proto) = self.object_prototype_object_opt() {
                            object::set_prototype(obj, &mut self.gc_heap, Some(proto));
                        }
                        crate::arguments_object::initialize_unmapped(
                            obj,
                            &mut self.gc_heap,
                            elements,
                            thrower,
                            iterator_descriptor,
                        );
                        obj
                    };
                    let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                    write_register(frame, dst, Value::object(obj))?;
                    frame.advance_pc(self.current_byte_len)?;
                    continue;
                }
                Op::Nop => {
                    stack[top_idx].advance_pc(self.current_byte_len)?;
                    continue;
                }
                Op::LoadUndefined => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::undefined())?;
                    frame.advance_pc(self.current_byte_len)?;
                    continue;
                }
                Op::LoadHole => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::hole())?;
                    frame.advance_pc(self.current_byte_len)?;
                    continue;
                }
                Op::LoadTrue => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::boolean(true))?;
                    frame.advance_pc(self.current_byte_len)?;
                    continue;
                }
                Op::LoadFalse => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::boolean(false))?;
                    frame.advance_pc(self.current_byte_len)?;
                    continue;
                }
                Op::LoadNull => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::null())?;
                    frame.advance_pc(self.current_byte_len)?;
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
                    write_register(frame, dst, Value::number(NumberValue::Smi(imm)))?;
                    frame.advance_pc(self.current_byte_len)?;
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
                    write_register(frame, dst, Value::number(value))?;
                    frame.advance_pc(self.current_byte_len)?;
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
                    let s = JsString::from_utf16_units(units, self.gc_heap_mut())?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::string(s))?;
                    frame.advance_pc(self.current_byte_len)?;
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
                        .as_string(&self.gc_heap)
                        .ok_or(VmError::TypeMismatch)?;
                    let len = NumberValue::from_i32(s.len() as i32);
                    write_register(frame, dst, Value::number(len))?;
                    frame.advance_pc(self.current_byte_len)?;
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
                    let truthy = read_register(frame, src)?.to_boolean(&self.gc_heap);
                    write_register(frame, dst, Value::boolean(!truthy))?;
                    frame.advance_pc(self.current_byte_len)?;
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
                    let truthy = read_register(frame, src)?.to_boolean(&self.gc_heap);
                    write_register(frame, dst, Value::boolean(truthy))?;
                    frame.advance_pc(self.current_byte_len)?;
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
                    let mut value = stack[top_idx].this_value;
                    if value.is_hole() {
                        // §13.3.7.3 — an arrow's lexical `this` in a
                        // derived constructor: the hole snapshot
                        // resolves through the nearest derived-ctor
                        // frame (bound there by a super() that may
                        // itself have run inside an arrow).
                        for i in (0..=top_idx).rev() {
                            if self
                                .frame_cold(&stack[i])
                                .is_some_and(|c| c.is_derived_constructor)
                            {
                                value = stack[i].this_value;
                                break;
                            }
                        }
                    }
                    if value.is_hole() {
                        return Err(VmError::ThisUninitialized {
                            message: "must call super constructor in derived class before accessing 'this' or returning from derived constructor".to_string(),
                        });
                    }
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, value)?;
                    frame.advance_pc(self.current_byte_len)?;
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
                Op::FreshUpvalue => {
                    let idx = context
                        .exec_imm32(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_fresh_upvalue_reg(frame, idx)?;
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
                Op::StoreUpvalueChecked => {
                    let src = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let idx = context
                        .exec_imm32(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_store_upvalue_checked_reg(frame, src, idx)?;
                    continue;
                }
                Op::CollectRest => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    self.run_collect_rest_reg(&mut *stack, top_idx, dst)?;
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
                    // Operand 4 (parent class value) — absent in
                    // pre-existing bytecode; `undefined` = base class.
                    let parent_reg = context.exec_register(instr, 4);
                    self.run_make_class_regs(
                        &mut *stack,
                        top_idx,
                        dst,
                        ctor_reg,
                        proto_reg,
                        statics_reg,
                        parent_reg,
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
                    self.run_new_error_regs(context, &mut *stack, top_idx, dst, msg_reg)?;
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
                Op::DeclareGlobalVar => {
                    let name_idx = context
                        .exec_const_index(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let configurable = context.exec_imm32(instr, 1).unwrap_or(0) != 0;
                    let frame = &mut stack[top_idx];
                    self.run_declare_global_var_reg(context, frame, name_idx, configurable)?;
                    continue;
                }
                // §13.2.8.4 GetTemplateObject — realm-cached frozen
                // template-strings object per tagged-template site.
                Op::GetTemplateObject => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let site_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let key = (context.function_base(), site_idx);
                    let value = match self.template_objects.get(&key) {
                        Some(v) => *v,
                        None => {
                            let built = self.build_template_object(context, &*stack, site_idx)?;
                            self.template_objects.insert(key, built);
                            built
                        }
                    };
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, value)?;
                    frame.advance_pc(self.current_byte_len)?;
                    continue;
                }
                // §9.1 — captured-binding read in a frame whose
                // function contains a direct eval: an
                // eval-introduced var of the same name shadows the
                // capture.
                Op::LoadShadowedUpvalue => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let uv_idx = context.exec_imm32(instr, 2).unwrap_or(0) as usize;
                    let frame = &mut stack[top_idx];
                    if let Some(name) = context.string_constant_str(name_idx)
                        && let Some(cell) = self
                            .frame_cold(frame)
                            .and_then(|cold| cold.eval_vars.as_ref())
                            .and_then(|map| map.get(name))
                            .copied()
                    {
                        let value = crate::read_upvalue(&self.gc_heap, cell);
                        write_register(frame, dst, value)?;
                        frame.advance_pc(self.current_byte_len)?;
                        continue;
                    }
                    let cell = frame
                        .upvalues
                        .get(uv_idx)
                        .copied()
                        .ok_or(VmError::InvalidOperand)?;
                    let value = crate::read_upvalue(&self.gc_heap, cell);
                    write_register(frame, dst, value)?;
                    frame.advance_pc(self.current_byte_len)?;
                    continue;
                }
                Op::LoadDynamic => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_load_dynamic_reg(context, frame, dst, name_idx)?;
                    continue;
                }
                Op::StoreDynamic => {
                    let value_reg = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_store_dynamic_reg(context, frame, value_reg, name_idx)?;
                    continue;
                }
                Op::TypeofDynamic => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_typeof_dynamic_reg(context, frame, dst, name_idx)?;
                    continue;
                }
                Op::DeleteDynamic => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_delete_dynamic_reg(context, frame, dst, name_idx)?;
                    continue;
                }
                // §6.2.12 — mint a Private Name carrier; the marker
                // keeps it out of Proxy traps and arms the §7.3.28
                // extensibility check on adds.
                Op::NewPrivateName => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let desc_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let desc = context
                        .string_constant_str(desc_idx)
                        .ok_or(VmError::InvalidOperand)?
                        .to_string();
                    let desc_str = JsString::from_str(&desc, &mut self.gc_heap)?;
                    let sym =
                        crate::symbol::JsSymbol::new_private(&mut self.gc_heap, Some(desc_str))?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::symbol(sym))?;
                    frame.advance_pc(self.current_byte_len)?;
                    continue;
                }
                Op::DefineGlobalFunction => {
                    let name_idx = context
                        .exec_const_index(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let value_reg = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let deletable = context.exec_imm32(instr, 2).unwrap_or(0) != 0;
                    let frame = &mut stack[top_idx];
                    self.run_define_global_function_reg(
                        context, frame, name_idx, value_reg, deletable,
                    )?;
                    continue;
                }
                Op::DeclareGlobalLex => {
                    let name_idx = context
                        .exec_const_index(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let is_const = context.exec_imm32(instr, 1).unwrap_or(0) != 0;
                    let frame = &mut stack[top_idx];
                    self.run_declare_global_lex_reg(context, frame, name_idx, is_const)?;
                    continue;
                }
                Op::StoreGlobalBinding => {
                    let value_reg = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let strict = context.exec_imm32(instr, 2).unwrap_or(0) != 0;
                    let frame = &mut stack[top_idx];
                    self.run_store_global_binding_reg(context, frame, value_reg, name_idx, strict)?;
                    continue;
                }
                Op::InitGlobalLex => {
                    let value_reg = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_init_global_lex_reg(context, frame, value_reg, name_idx)?;
                    continue;
                }
                // §15.7.14 class-definition validation: heritage
                // IsConstructor / static computed key != "prototype".
                Op::ClassCheck => {
                    let kind = context.exec_imm32(instr, 0).unwrap_or(0);
                    let reg = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let value = *read_register(&stack[top_idx], reg)?;
                    match kind {
                        0 => {
                            if !value.is_null()
                                && !abstract_ops::is_constructor(&value, context, &self.gc_heap)
                            {
                                return Err(VmError::TypeError {
                                    message: "Class extends value is not a constructor or null"
                                        .to_string(),
                                });
                            }
                        }
                        _ => {
                            if value
                                .as_string(&self.gc_heap)
                                .is_some_and(|s| s.to_lossy_string(&self.gc_heap) == "prototype")
                            {
                                return Err(VmError::TypeError {
                                    message:
                                        "Classes may not have a static property named 'prototype'"
                                            .to_string(),
                                });
                            }
                        }
                    }
                    let frame = &mut stack[top_idx];
                    frame.advance_pc(self.current_byte_len)?;
                    continue;
                }
                // §7.3.7 CreateDataPropertyOrThrow — object literal
                // property definition; never consults inherited
                // setters (unlike StoreProperty's Set semantics).
                Op::DefineDataProperty => {
                    let (obj_reg, key_reg, value_reg) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let target = *read_register(&stack[top_idx], obj_reg)?;
                    let key_value = *read_register(&stack[top_idx], key_reg)?;
                    let value = *read_register(&stack[top_idx], value_reg)?;
                    let key = self.to_property_key_sync(context, key_value)?;
                    // Fast path: a plain object receiver takes the
                    // shape-friendly construction-time store (no
                    // prototype consult — define semantics).
                    if let Some(obj) = target.as_object() {
                        match &key {
                            VmPropertyKey::Symbol(sym) => {
                                object::set_symbol(obj, &mut self.gc_heap, *sym, value);
                            }
                            _ => {
                                let name = key
                                    .string_name()
                                    .expect("non-symbol key has string spelling")
                                    .to_string();
                                self.set_property(obj, &name, value)?;
                            }
                        }
                    } else {
                        let descriptor = object::PartialPropertyDescriptor {
                            value: Some(value),
                            writable: Some(true),
                            enumerable: Some(true),
                            configurable: Some(true),
                            ..Default::default()
                        };
                        if !self.define_own_property_value(context, &target, &key, descriptor)? {
                            return Err(VmError::TypeError {
                                message: "Cannot define property on object literal".to_string(),
                            });
                        }
                    }
                    let frame = &mut stack[top_idx];
                    frame.advance_pc(self.current_byte_len)?;
                    continue;
                }
                // §10.2.10 SetFunctionName — names an anonymous
                // function from a run-time property key.
                Op::SetFunctionName => {
                    let fn_reg = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let key_reg = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let prefix_idx = context
                        .exec_const_index(instr, 2)
                        .ok_or(VmError::InvalidOperand)?;
                    let callee = *read_register(&stack[top_idx], fn_reg)?;
                    let key_value = *read_register(&stack[top_idx], key_reg)?;
                    let prefix = context
                        .property_atom(prefix_idx)
                        .map(|atom| atom.name().to_string())
                        .unwrap_or_default();
                    let mut name = if let Some(sym) = key_value.as_symbol(&self.gc_heap) {
                        match sym.description() {
                            Some(desc) => format!("[{}]", desc.to_lossy_string(&self.gc_heap)),
                            None => String::new(),
                        }
                    } else {
                        key_value.display_string(&self.gc_heap)
                    };
                    if !prefix.is_empty() {
                        name = format!("{prefix} {name}");
                    }
                    // A class expression's `name` delegates to the
                    // inner callable's metadata, so naming the ctor
                    // names the class.
                    let callee = match callee.as_class_constructor() {
                        Some(c) => c.ctor(&self.gc_heap),
                        None => callee,
                    };
                    if let Some(fid) = callee.as_function().or_else(|| {
                        callee
                            .as_closure(&self.gc_heap)
                            .map(|c| c.cached_function_id)
                    }) {
                        let owner = callee.as_closure(&self.gc_heap);
                        let name_str = JsString::from_str(&name, &mut self.gc_heap)?;
                        let descriptor = object::PropertyDescriptor {
                            kind: object::DescriptorKind::Data {
                                value: Value::string(name_str),
                            },
                            flags: object::PropertyFlags::new(false, false, true),
                        };
                        self.ordinary_function_define_own_property(
                            Some(context),
                            owner,
                            fid,
                            "name",
                            None,
                            descriptor,
                        )?;
                    }
                    let frame = &mut stack[top_idx];
                    frame.advance_pc(self.current_byte_len)?;
                    continue;
                }
                // §7.3.31 PrivateGet — brand check (absent name
                // throws), accessor-without-getter throws, accessor
                // invokes its getter with the receiver as `this`.
                Op::PrivateGet => {
                    let (dst, obj_reg, key_reg) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let receiver = *read_register(&stack[top_idx], obj_reg)?;
                    let key = *read_register(&stack[top_idx], key_reg)?;
                    // A non-symbol key means the private-name binding
                    // failed to resolve through the capture chain —
                    // surface the spec's brand-check TypeError rather
                    // than a VM invariant crash.
                    let Some(sym) = key.as_symbol(&self.gc_heap) else {
                        return Err(VmError::TypeError {
                            message:
                                "Cannot read private member from an object whose class did not declare it"
                                    .to_string(),
                        });
                    };
                    let found = self.private_element_lookup(context, &receiver, sym)?;
                    let result = match found {
                        None => {
                            return Err(VmError::TypeError {
                                message:
                                    "Cannot read private member from an object whose class did not declare it"
                                        .to_string(),
                            });
                        }
                        Some((_, desc)) => match desc.kind {
                            object::DescriptorKind::Data { value } => value,
                            object::DescriptorKind::Accessor { getter, .. } => match getter {
                                Some(getter) => self.run_callable_sync(
                                    context,
                                    &getter,
                                    receiver,
                                    smallvec::SmallVec::new(),
                                )?,
                                None => {
                                    return Err(VmError::TypeError {
                                        message: "'#x' was defined without a getter".to_string(),
                                    });
                                }
                            },
                        },
                    };
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, result)?;
                    frame.advance_pc(self.current_byte_len)?;
                    continue;
                }
                // §7.3.32 PrivateSet — brand check, private methods
                // are not writable, accessor-without-setter throws,
                // an own field writes in place preserving attributes.
                Op::PrivateSet => {
                    let (obj_reg, key_reg, value_reg) = context
                        .exec_register3(instr)
                        .ok_or(VmError::InvalidOperand)?;
                    let receiver = *read_register(&stack[top_idx], obj_reg)?;
                    let key = *read_register(&stack[top_idx], key_reg)?;
                    let value = *read_register(&stack[top_idx], value_reg)?;
                    let Some(sym) = key.as_symbol(&self.gc_heap) else {
                        return Err(VmError::TypeError {
                            message:
                                "Cannot write private member to an object whose class did not declare it"
                                    .to_string(),
                        });
                    };
                    let found = self.private_element_lookup(context, &receiver, sym)?;
                    match found {
                        None => {
                            return Err(VmError::TypeError {
                                message:
                                    "Cannot write private member to an object whose class did not declare it"
                                        .to_string(),
                            });
                        }
                        Some((holder, desc)) => match desc.kind {
                            object::DescriptorKind::Accessor { setter, .. } => match setter {
                                Some(setter) => {
                                    let argv: smallvec::SmallVec<[Value; 8]> =
                                        smallvec::smallvec![value];
                                    self.run_callable_sync(context, &setter, receiver, argv)?;
                                }
                                None => {
                                    return Err(VmError::TypeError {
                                        message: "'#x' was defined without a setter".to_string(),
                                    });
                                }
                            },
                            object::DescriptorKind::Data { .. } => {
                                if holder != receiver || !desc.flags.writable() {
                                    // Prototype side or a non-writable
                                    // own slot — a private method.
                                    return Err(VmError::TypeError {
                                        message: "Private method is not writable".to_string(),
                                    });
                                }
                                let descriptor = object::PartialPropertyDescriptor {
                                    value: Some(value),
                                    ..Default::default()
                                };
                                let vm_key = VmPropertyKey::Symbol(sym);
                                self.define_own_property_value(
                                    context, &receiver, &vm_key, descriptor,
                                )?;
                            }
                        },
                    }
                    let frame = &mut stack[top_idx];
                    frame.advance_pc(self.current_byte_len)?;
                    continue;
                }
                // §7.1.3 ToNumeric on an already-primitive operand:
                // Number / BigInt pass through, Symbol throws, the
                // rest convert via ToNumber. Emitted between the two
                // ToPrimitive coercions of a numeric binary operator
                // so ToNumeric(lhs) throws before rhs `valueOf` runs.
                Op::ToNumeric => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let value = *read_register(&stack[top_idx], src)?;
                    let result = if value.is_number() || value.is_big_int() {
                        value
                    } else if value.is_symbol() {
                        return Err(VmError::TypeError {
                            message: "Cannot convert a Symbol value to a number".to_string(),
                        });
                    } else {
                        Value::number(crate::number::NumberValue::from_f64(
                            crate::number::parse::to_number_value(&value, &self.gc_heap),
                        ))
                    };
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, result)?;
                    frame.advance_pc(self.current_byte_len)?;
                    continue;
                }
                // §7.1.18 ToObject — wrap a primitive in its
                // `%X.prototype%` body; objects pass through;
                // `null` / `undefined` throw a TypeError. Emitted by
                // the `with` statement lowering (§14.11.2 step 2).
                Op::ToObject => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let value = *read_register(&stack[top_idx], src)?;
                    if value.is_nullish() {
                        return Err(VmError::TypeMismatch);
                    }
                    let boxed = self.box_sloppy_this_primitive_stack_rooted(stack, value, &[])?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, boxed)?;
                    frame.advance_pc(self.current_byte_len)?;
                    continue;
                }
                // §7.1.19 ToPropertyKey with full user coercion —
                // class field definitions canonicalize their
                // computed names at class-definition time.
                Op::ToPropertyKey => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let value = *read_register(&stack[top_idx], src)?;
                    let primitive = self.evaluate_to_primitive(
                        context,
                        &value,
                        abstract_ops::ToPrimitiveHint::String,
                    )?;
                    let key = if primitive.as_symbol(&self.gc_heap).is_some()
                        || primitive.as_string(&self.gc_heap).is_some()
                    {
                        primitive
                    } else {
                        let text = primitive.display_string(&self.gc_heap);
                        Value::string(JsString::from_str(&text, &mut self.gc_heap)?)
                    };
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, key)?;
                    frame.advance_pc(self.current_byte_len)?;
                    continue;
                }
                // §7.3.31 PrivateElementFind own-only — private
                // methods / accessors require the class brand marker
                // as an OWN property of the receiver (installed after
                // super() returns); the prototype-side method lookup
                // alone must not satisfy access before that.
                Op::PrivateBrandCheck => {
                    let obj_reg = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let brand_reg = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let receiver = *read_register(&stack[top_idx], obj_reg)?;
                    let brand = *read_register(&stack[top_idx], brand_reg)?;
                    let Some(sym) = brand.as_symbol(&self.gc_heap) else {
                        return Err(VmError::TypeError {
                            message:
                                "Cannot read private member from an object whose class did not declare it"
                                    .to_string(),
                        });
                    };
                    let key = VmPropertyKey::Symbol(sym);
                    // §6.2.12 — a Proxy answers brand checks from its
                    // own [[PrivateElements]] bag, never from traps.
                    let found = if let Some(p) = receiver.as_proxy() {
                        self.proxy_private_find(&p, sym).is_some()
                    } else {
                        self.ordinary_get_own_property_descriptor_value_runtime_rooted(
                            context,
                            receiver,
                            &key,
                            0,
                            &[&receiver, &brand],
                            &[],
                        )?
                        .is_some()
                    };
                    if !found {
                        return Err(VmError::TypeError {
                            message:
                                "Cannot read private member from an object whose class did not declare it"
                                    .to_string(),
                        });
                    }
                    let frame = &mut stack[top_idx];
                    frame.advance_pc(self.current_byte_len)?;
                    continue;
                }
                // §13.4.2 UpdateExpression numeric step — ToNumeric
                // then ±1, preserving the BigInt type (§6.1.6.2.7).
                Op::Increment => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let delta = context.exec_imm32(instr, 2).unwrap_or(1);
                    let value = *read_register(&stack[top_idx], src)?;
                    let primitive = self.evaluate_to_primitive(
                        context,
                        &value,
                        abstract_ops::ToPrimitiveHint::Number,
                    )?;
                    let kind = abstract_ops::to_numeric_kind(&primitive, &self.gc_heap)
                        .ok_or(VmError::TypeMismatch)?;
                    let next = match kind {
                        abstract_ops::NumericKind::Num(n) => Value::number(
                            crate::number::NumberValue::from_f64(n.as_f64() + f64::from(delta)),
                        ),
                        abstract_ops::NumericKind::Big(b) => {
                            let delta_big = num_bigint::BigInt::from(delta);
                            let sum = bigint::ops::add(&b, &delta_big);
                            let handle = bigint::BigIntValue::from_inner(&mut self.gc_heap, sum)
                                .map_err(|_| VmError::TypeMismatch)?;
                            Value::big_int(handle)
                        }
                    };
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, next)?;
                    frame.advance_pc(self.current_byte_len)?;
                    continue;
                }
                Op::ValidateGlobalDecl => {
                    let name_idx = context
                        .exec_const_index(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let kind = context.exec_imm32(instr, 1).unwrap_or(0);
                    let frame = &mut stack[top_idx];
                    self.run_validate_global_decl_reg(context, frame, name_idx, kind)?;
                    continue;
                }
                Op::DefineGlobalVar => {
                    let name_idx = context
                        .exec_const_index(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let value_reg = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_define_global_var_reg(context, frame, name_idx, value_reg)?;
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
                Op::ImportNamespaceDeferred => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let spec_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_import_namespace_deferred_reg(context, frame, dst, spec_idx)?;
                    continue;
                }
                Op::ModuleNamespaceObject => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let spec_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_module_namespace_object_reg(context, frame, dst, spec_idx)?;
                    continue;
                }
                Op::LoadImportBinding => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let url_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 2)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_load_import_binding_reg(context, frame, dst, url_idx, name_idx)?;
                    continue;
                }
                Op::EvaluateModule => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let url_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_evaluate_module_const(context, frame, dst, url_idx)?;
                    continue;
                }
                Op::MarkModuleEvaluated => {
                    let url_idx = context
                        .exec_const_index(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    if let Some(url) = context.string_constant_str(url_idx) {
                        let url_arc: std::sync::Arc<str> = std::sync::Arc::from(url);
                        self.module_record_mut(&url_arc).status =
                            module_records::ModuleStatus::Evaluated;
                    }
                    stack[top_idx].advance_pc(self.current_byte_len)?;
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
                    self.run_import_meta_resolve_regs(context, frame, dst, spec_reg)?;
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
                Op::JumpViaFinally => {
                    // §14.15.3 — `break`/`continue` crossing `finally`
                    // blocks: run them (down to `floor`), then jump.
                    let offset = context
                        .exec_imm32(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let floor = context
                        .exec_imm32(instr, 1)
                        .ok_or(VmError::InvalidOperand)? as u32;
                    let next_pc = (stack[top_idx].pc as i64 + 1).saturating_add(offset as i64);
                    if !(0..=u32::MAX as i64).contains(&next_pc) {
                        return Err(VmError::InvalidOperand);
                    }
                    if let Some(popped) = self.unwind_abrupt(
                        stack,
                        crate::cold_frame::AbruptKind::Jump(next_pc as u32),
                        floor,
                    )? {
                        return Ok(popped);
                    }
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
                    if read_register(frame, cond)?.to_boolean(&self.gc_heap) {
                        apply_branch(frame, offset, &self.interrupt)?;
                    } else {
                        frame.advance_pc(self.current_byte_len)?;
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
                    if !read_register(frame, cond)?.to_boolean(&self.gc_heap) {
                        apply_branch(frame, offset, &self.interrupt)?;
                    } else {
                        frame.advance_pc(self.current_byte_len)?;
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
                        frame.advance_pc(self.current_byte_len)?;
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
                    let value = *read_register(frame, idx as u16)?;
                    write_register(frame, dst, value)?;
                    frame.advance_pc(self.current_byte_len)?;
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
                    let value = *read_register(frame, src)?;
                    write_register(frame, idx as u16, value)?;
                    frame.advance_pc(self.current_byte_len)?;
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
                        Op::LooseEqual => {
                            self.run_loose_equal_regs(context, frame, dst, lhs, rhs, false)?;
                        }
                        Op::LooseNotEqual => {
                            self.run_loose_equal_regs(context, frame, dst, lhs, rhs, true)?;
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
                    let arr = read_register(frame, src)?
                        .as_array()
                        .ok_or(VmError::TypeMismatch)?;
                    let n = NumberValue::from_f64(crate::array::len(arr, &self.gc_heap) as f64);
                    write_register(frame, dst, Value::number(n))?;
                    frame.advance_pc(self.current_byte_len)?;
                    continue;
                }
                Op::IsArray => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    let value = *read_register(&stack[top_idx], src)?;
                    let mut result = abstract_ops::is_array(&self.gc_heap, &value)?;
                    if !result
                        && let Some(obj) = value.as_object()
                        && self.realm_intrinsics.array_prototype == Some(obj)
                    {
                        result = true;
                    }
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::boolean(result))?;
                    frame.advance_pc(self.current_byte_len)?;
                    continue;
                }
                Op::MakeClosure => {
                    let operands = context.exec_operands(instr);
                    let frame = &mut stack[top_idx];
                    self.run_make_closure_operands(context, frame, operands)?;
                    continue;
                }
                Op::ArrayBufferCall => {
                    let operands = context.exec_operands(instr);
                    self.run_array_buffer_static_call_operands(stack, operands)?;
                    continue;
                }
                Op::SharedArrayBufferCall => {
                    let operands = context.exec_operands(instr);
                    self.run_shared_array_buffer_static_call_operands(stack, operands)?;
                    continue;
                }
                Op::BigIntCall | Op::DataViewCall => {
                    let operands = context.exec_operands(instr);
                    let frame = &mut stack[top_idx];
                    self.run_static_call_operands(op, context, frame, operands)?;
                    continue;
                }
                Op::ArrayConstruct | Op::ArrayFrom | Op::ArrayOf => {
                    let operands = context.exec_operands(instr);
                    self.run_array_static_operands(op, context, stack, operands)?;
                    continue;
                }
                Op::ForInKeys => {
                    let operands = context.exec_operands(instr);
                    self.run_for_in_keys_operands(context, stack, operands)?;
                    continue;
                }
                Op::CopyDataProperties => {
                    let operands = context.exec_operands(instr);
                    self.run_copy_data_properties_operands(context, stack, operands)?;
                    continue;
                }
                Op::StarReexport => {
                    let operands = context.exec_operands(instr);
                    self.run_star_reexport_operands(context, stack, operands)?;
                    continue;
                }
                Op::DefineOwnProperty => {
                    let operands = context.exec_operands(instr);
                    self.run_define_own_property_operands(context, stack, operands)?;
                    continue;
                }
                Op::QueueMicrotask => {
                    let operands = context.exec_operands(instr);
                    let frame = &mut stack[top_idx];
                    self.run_queue_microtask_operands(context, frame, operands)?;
                    continue;
                }
                Op::PromiseNew => {
                    let operands = context.exec_operands(instr);
                    self.run_promise_new_operands(context, stack, operands)?;
                    continue;
                }
                Op::PromiseCall => {
                    let operands = context.exec_operands(instr);
                    self.run_promise_call_operands(context, stack, operands)?;
                    continue;
                }
                Op::ImportNamespaceDynamic => {
                    let operands = context.exec_operands(instr);
                    self.run_import_namespace_dynamic_operands(context, stack, top_idx, operands)?;
                    continue;
                }
                Op::BindFunction => {
                    let operands = context.exec_operands(instr);
                    self.drive_bind_function(stack, context, operands)?;
                    continue;
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
        let mut popped = stack.pop().ok_or(VmError::InvalidOperand)?;
        let construct_target = self.frame_cold(&popped).and_then(|c| c.construct_target);
        let is_derived_ctor = self
            .frame_cold(&popped)
            .is_some_and(|c| c.is_derived_constructor);
        let mut derived_this = popped.this_value;
        if derived_this.is_hole()
            && let Some(cell) = self.frame_cold(&popped).and_then(|c| c.derived_this_cell)
        {
            derived_this = crate::read_upvalue(&self.gc_heap, cell);
        }
        // Release the cold slot now so the pool can reuse it; the
        // remaining cold-record reads above already happened.
        self.frame_release_cold(&mut popped);
        let resolved = if is_derived_ctor {
            // §10.2.2 derived-constructor return semantics. An object
            // return overrides the bound `this`; `undefined` yields
            // the `super(...)`-bound `this` (ReferenceError if
            // `super` never ran); any other primitive is a TypeError.
            if value.is_object_type() {
                value
            } else if value.is_undefined() {
                if derived_this.is_hole() {
                    return Err(VmError::ThisUninitialized {
                        message: "must call super constructor in derived class before accessing 'this' or returning from derived constructor".to_string(),
                    });
                }
                derived_this
            } else {
                return Err(VmError::TypeError {
                    message: "derived constructors may only return an object or undefined"
                        .to_string(),
                });
            }
        } else {
            match construct_target {
                Some(_) if value.is_object_type() => value,
                Some(target) => Value::object(target),
                None => value,
            }
        };
        if let Some(state) = popped.async_state {
            let jobs = state.result_promise.fulfill(&mut self.gc_heap, resolved);
            for j in jobs.jobs {
                self.microtasks.enqueue(j);
            }
            if stack.is_empty() {
                return Ok(Some(Value::undefined()));
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

    /// §14.15.3 — run the `finally` blocks between an abrupt `return` /
    /// `break` / `continue` and its target, then perform the
    /// completion. Pops handlers off the top frame until the handler
    /// stack reaches `floor`; the first `finally` found parks the
    /// completion (`pending_abrupt`) and jumps to the finally body —
    /// `Op::EndFinally` resumes this walk. With no remaining `finally`,
    /// a `Jump` sets the target pc and a `Return` pops the frame.
    fn unwind_abrupt(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        completion: crate::cold_frame::AbruptKind,
        floor: u32,
    ) -> Result<Option<Value>, VmError> {
        use crate::cold_frame::AbruptKind;
        loop {
            let top_idx = stack.len() - 1;
            let handler_count = self
                .frame_cold(&stack[top_idx])
                .map(|c| c.handlers.len() as u32)
                .unwrap_or(0);
            if handler_count <= floor {
                return match completion {
                    AbruptKind::Jump(pc) => {
                        stack[top_idx].pc = pc;
                        Ok(None)
                    }
                    AbruptKind::Return(v) => self.pop_frame(stack, v),
                };
            }
            let handler = self
                .frame_cold_mut(&mut stack[top_idx])
                .and_then(|c| c.handlers.pop());
            // §14.15.3 — discard completions parked by `finally`
            // blocks this completion abandons (depth above the
            // remaining handler stack).
            if let Some(cold) = self.frame_cold_mut(&mut stack[top_idx]) {
                let len = cold.handlers.len() as u32;
                cold.parked_finally.retain(|(_, depth)| *depth <= len);
            }
            match handler {
                Some(h) if h.finally_pc.is_some() => {
                    let finally_pc = h.finally_pc.expect("finally_pc checked");
                    let cold = self.frame_ensure_cold(&mut stack[top_idx]);
                    let depth = cold.handlers.len() as u32;
                    cold.parked_finally.push((
                        crate::cold_frame::ParkedFinally::Abrupt(completion, floor),
                        depth,
                    ));
                    stack[top_idx].pc = finally_pc;
                    return Ok(None);
                }
                // Catch-only handler crossed by the abrupt completion:
                // pop it (cleanup) and keep walking.
                Some(_) => continue,
                None => {
                    return match completion {
                        AbruptKind::Jump(pc) => {
                            stack[top_idx].pc = pc;
                            Ok(None)
                        }
                        AbruptKind::Return(v) => self.pop_frame(stack, v),
                    };
                }
            }
        }
    }

    /// Return `value` from the top frame, first running any enclosing
    /// `finally` blocks (§14.15.3). Equivalent to [`Self::pop_frame`]
    /// when no `finally` handler is active.
    fn return_running_finally(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        value: Value,
    ) -> Result<Option<Value>, VmError> {
        let top_idx = stack.len() - 1;
        let has_finally = self
            .frame_cold(&stack[top_idx])
            .is_some_and(|c| c.handlers.iter().any(|h| h.finally_pc.is_some()));
        if has_finally {
            self.unwind_abrupt(stack, crate::cold_frame::AbruptKind::Return(value), 0)
        } else {
            self.pop_frame(stack, value)
        }
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
    gc_heap: &mut otter_gc::GcHeap,
    function_prototype: Option<object::JsObject>,
) -> Result<Option<Value>, VmError> {
    match name {
        // §20.1.3.2 Object.prototype.hasOwnProperty(V)
        // <https://tc39.es/ecma262/#sec-object.prototype.hasownproperty>
        "hasOwnProperty" => {
            let key = property_key_from_arg(args.first(), gc_heap)?;
            let present = !matches!(
                object::lookup_own(*obj, gc_heap, &key),
                object::PropertyLookup::Absent
            );
            Ok(Some(Value::boolean(present)))
        }
        // §20.1.3.4 Object.prototype.propertyIsEnumerable(V)
        // <https://tc39.es/ecma262/#sec-object.prototype.propertyisenumerable>
        "propertyIsEnumerable" => {
            let key = property_key_from_arg(args.first(), gc_heap)?;
            let result = match object::lookup_own(*obj, gc_heap, &key) {
                object::PropertyLookup::Data { flags, .. } => flags.enumerable(),
                object::PropertyLookup::Accessor { flags, .. } => flags.enumerable(),
                object::PropertyLookup::Absent => false,
            };
            Ok(Some(Value::boolean(result)))
        }
        // §20.1.3.3 Object.prototype.isPrototypeOf(V)
        // <https://tc39.es/ecma262/#sec-object.prototype.isprototypeof>
        "isPrototypeOf" => {
            let result = args.first().is_some_and(|value| {
                value_has_prototype_in_chain(value, *obj, gc_heap, function_prototype)
            });
            Ok(Some(Value::boolean(result)))
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
            let recv_value = Value::object(*obj);
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
            let s = JsString::from_str(&display, gc_heap).map_err(|_| VmError::TypeMismatch)?;
            Ok(Some(Value::string(s)))
        }
        // §20.1.3.7 Object.prototype.valueOf() — returns the receiver.
        // <https://tc39.es/ecma262/#sec-object.prototype.valueof>
        "valueOf" => Ok(Some(Value::object(*obj))),
        _ => Ok(None),
    }
}

fn value_has_prototype_in_chain(
    value: &Value,
    target: object::JsObject,
    gc_heap: &otter_gc::GcHeap,
    function_prototype: Option<object::JsObject>,
) -> bool {
    if let Some(obj) = value.as_object() {
        if object_has_construct_slot(value, gc_heap) {
            function_value_has_prototype_in_chain(target, gc_heap, function_prototype)
        } else {
            object::has_in_proto_chain(obj, gc_heap, target)
        }
    } else if value.is_function()
        || value.is_closure()
        || value.is_bound_function()
        || value.is_native_function()
        || value.is_class_constructor()
    {
        function_value_has_prototype_in_chain(target, gc_heap, function_prototype)
    } else {
        false
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

fn descriptor_value(desc: &crate::object::PropertyDescriptor) -> Value {
    match &desc.kind {
        crate::object::DescriptorKind::Data { value } => *value,
        crate::object::DescriptorKind::Accessor { .. } => Value::undefined(),
    }
}

pub(crate) fn value_kind_name(value: &Value) -> &'static str {
    if value.is_undefined() || value.is_hole() {
        "undefined"
    } else if value.is_null() {
        "null"
    } else if value.is_boolean() {
        "boolean"
    } else if value.is_number() {
        "number"
    } else if value.is_string() {
        "string"
    } else if value.is_symbol() {
        "symbol"
    } else if value.is_big_int() {
        "bigint"
    } else if value.is_object() {
        "object"
    } else if value.is_array() {
        "array"
    } else if value.is_function()
        || value.is_closure()
        || value.is_native_function()
        || value.is_bound_function()
    {
        "function"
    } else if value.is_class_constructor() {
        "class constructor"
    } else if value.is_regexp() {
        "regexp"
    } else if value.is_promise() {
        "promise"
    } else if value.is_proxy() {
        "proxy"
    } else if value.is_map() {
        "map"
    } else if value.is_set() {
        "set"
    } else if value.is_weak_map() {
        "weakmap"
    } else if value.is_weak_set() {
        "weakset"
    } else if value.is_weak_ref() {
        "weakref"
    } else if value.is_finalization_registry() {
        "finalization registry"
    } else if value.is_generator() {
        "generator"
    } else if value.is_iterator() {
        "iterator"
    } else if value.is_temporal() {
        "temporal"
    } else if value.is_intl() {
        "intl"
    } else if value.is_array_buffer() {
        "arraybuffer"
    } else if value.is_data_view() {
        "dataview"
    } else if value.is_typed_array() {
        "typedarray"
    } else {
        "unknown"
    }
}

/// §7.1.19 ToPropertyKey for a single optional argument used by
/// `Object.prototype.hasOwnProperty` / `propertyIsEnumerable`.
fn property_key_from_arg(arg: Option<&Value>, heap: &otter_gc::GcHeap) -> Result<String, VmError> {
    let Some(v) = arg else {
        return Ok("undefined".to_string());
    };
    if let Some(s) = v.as_string(heap) {
        Ok(s.to_lossy_string(heap))
    } else if let Some(n) = v.as_number() {
        Ok(n.to_display_string())
    } else if let Some(b) = v.as_boolean() {
        Ok((if b { "true" } else { "false" }).to_string())
    } else if v.is_null() {
        Ok("null".to_string())
    } else if v.is_undefined() {
        Ok("undefined".to_string())
    } else {
        Err(VmError::TypeMismatch)
    }
}

fn to_length(value: &Value, heap: &otter_gc::GcHeap) -> Result<usize, VmError> {
    if value.is_symbol() || value.is_big_int() {
        return Err(VmError::TypeMismatch);
    }
    let n = number::to_number_value(value, heap);
    if n.is_nan() || n <= 0.0 {
        return Ok(0);
    }
    if n.is_infinite() {
        return Ok(9_007_199_254_740_991);
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
        Some(v) if abstract_ops::is_callable(v) => Ok(*v),
        _ => Err(VmError::NotCallable),
    }
}

/// Build the canonical `(value, index, array)` argument tuple every
/// `Array.prototype` callback expects.
fn build_array_cb_args(value: &Value, index: usize, arr: &Value) -> SmallVec<[Value; 8]> {
    let mut cb_args: SmallVec<[Value; 8]> = SmallVec::new();
    cb_args.push(*value);
    cb_args.push(Value::number(NumberValue::from_i32(index as i32)));
    cb_args.push(*arr);
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
/// fresh iterator over the captured array — matching the
/// surface of `Array.prototype[@@iterator]` from
/// [ECMA-262 §23.1.5.1](https://tc39.es/ecma262/#sec-array.prototype-@@iterator).
///
/// # Invariants
/// - Capturing the array by handle means the iterator observes
///   subsequent in-place mutations through the same `JsArray`,
///   matching real-engine `Array.prototype[Symbol.iterator]`
///   semantics.
///
/// `String.prototype[Symbol.iterator]()` — receiver-dispatched
/// shim that materialises a string iterator from the calling
/// `this` value. Installed as the realm's iterator method per
/// §22.1.3.34.
fn string_proto_iterator(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    const NAME: &str = "String.prototype[Symbol.iterator]";
    let this = *ctx.this_value();
    // §22.1.3.34 — RequireObjectCoercible(this), then `S = ?
    // ToString(this)`: the method is generic, so a plain-object
    // receiver runs its own `toString` / `valueOf` / `@@toPrimitive`
    // (and an abrupt completion from there propagates).
    if this.is_nullish() {
        return Err(NativeError::TypeError {
            name: NAME,
            reason: "called on null or undefined".to_string(),
        });
    }
    let string = if let Some(s) = this.as_string(ctx.heap()) {
        s
    } else if let Some(obj) = this.as_object()
        && let Some(s) = crate::object::string_data(obj, ctx.heap())
    {
        s
    } else {
        let (interp, exec) = ctx.interp_mut_and_context();
        let exec = exec.ok_or_else(|| NativeError::TypeError {
            name: NAME,
            reason: "missing execution context".to_string(),
        })?;
        let text = interp
            .coerce_to_string(&exec, &this)
            .map_err(|e| crate::native_function::vm_to_native_error(e, NAME))?;
        JsString::from_str(&text, ctx.heap_mut()).map_err(|_| NativeError::TypeError {
            name: NAME,
            reason: "out of memory".to_string(),
        })?
    };
    let state = IteratorState::String { string, index: 0 };
    Ok(Value::iterator(ctx.alloc_iterator_state(
        state,
        &[],
        &[],
    )?))
}

/// Install `String.prototype[Symbol.iterator]` per §22.1.3.34.
pub(crate) fn install_string_iterator_post_bootstrap(
    heap: &mut otter_gc::GcHeap,
    global: crate::object::JsObject,
    well_known: &symbol::WellKnownSymbols,
) -> Result<(), crate::js_surface::JsSurfaceError> {
    let Some(string_ctor) = crate::object::get(global, heap, "String") else {
        return Ok(());
    };
    let prototype = if let Some(string_ctor) = string_ctor.as_object() {
        crate::object::get(string_ctor, heap, "prototype").and_then(|v| v.as_object())
    } else if let Some(string_ctor) = string_ctor.as_native_function() {
        string_ctor
            .own_property_descriptor(heap, "prototype")
            .ok()
            .flatten()
            .and_then(|desc| match desc.kind {
                crate::object::DescriptorKind::Data { value } => value.as_object(),
                crate::object::DescriptorKind::Accessor { .. } => None,
            })
    } else {
        None
    };
    let Some(prototype) = prototype else {
        return Ok(());
    };
    let global_root = Value::object(global);
    let prototype_root = Value::object(prototype);
    let getter = crate::bootstrap::native_static_with_value_roots(
        heap,
        "[Symbol.iterator]",
        0,
        string_proto_iterator,
        &[&global_root, &prototype_root],
    )
    .map_err(|_| crate::js_surface::JsSurfaceError::OutOfMemory)?;
    let sym = well_known.get(symbol::WellKnown::Iterator);
    crate::object::define_own_symbol_property_partial(
        prototype,
        heap,
        sym,
        crate::object::PartialPropertyDescriptor {
            value: Some(Value::native_function(getter)),
            writable: Some(true),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    );
    Ok(())
}

#[cfg(test)]
fn make_array_iterator_factory(
    array: JsArray,
    heap: &mut otter_gc::GcHeap,
) -> Result<Value, otter_gc::OutOfMemory> {
    native_value_with_captures(
        heap,
        "Array[Symbol.iterator]",
        smallvec::smallvec![Value::array(array)],
        array_iterator_factory_call,
    )
}

#[cfg(test)]
fn array_iterator_factory_call(
    ctx: &mut NativeCtx<'_>,
    _: &[Value],
    captures: &[Value],
) -> Result<Value, NativeError> {
    let Some(array) = captures.first().and_then(|v| v.as_array()) else {
        return Err(NativeError::TypeError {
            name: "Array[Symbol.iterator]",
            reason: "missing traced array capture".to_string(),
        });
    };
    let state = IteratorState::Array {
        array,
        index: 0,
        origin: BuiltinIteratorOrigin::Array,
    };
    Ok(Value::iterator(ctx.alloc_iterator_state(
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

/// Drive an iterator one step. Returns `(value, done)`. Once an
/// iterator hands back `done = true`, its state transitions to
/// `Exhausted` so subsequent calls are stable no-ops (matches the
/// spec rule "an iterator never produces values after it has
/// produced `done: true`"; §7.4.2 step 6).
fn step_iterator(
    iter: IteratorHandle,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<(Value, bool), VmError> {
    enum FastIteratorSnapshot {
        Array(JsArray, usize),
        ArrayKey(JsArray, usize),
        ArrayEntry(JsArray, usize),
        ArrayLike(Value, usize, crate::iterator_state::ArrayIterKind),
        TypedArray(
            crate::binary::typed_array::JsTypedArray,
            usize,
            crate::iterator_state::ArrayIterKind,
        ),
        String(JsString, u32),
        MapCollection(JsMap, usize, MapIteratorKind),
        SetCollection(JsSet, usize, SetIteratorKind),
        Exhausted,
        Slow,
    }

    let snapshot = gc_heap.read_payload(iter, |state| match state {
        IteratorState::Array { array, index, .. } => FastIteratorSnapshot::Array(*array, *index),
        IteratorState::ArrayKey { array, index } => FastIteratorSnapshot::ArrayKey(*array, *index),
        IteratorState::ArrayEntry { array, index } => {
            FastIteratorSnapshot::ArrayEntry(*array, *index)
        }
        IteratorState::TypedArray {
            typed_array,
            index,
            kind,
        } => FastIteratorSnapshot::TypedArray(*typed_array, *index, *kind),
        IteratorState::String { string, index } => FastIteratorSnapshot::String(*string, *index),
        IteratorState::MapCollection { map, index, kind } => {
            FastIteratorSnapshot::MapCollection(*map, *index, *kind)
        }
        IteratorState::SetCollection { set, index, kind } => {
            FastIteratorSnapshot::SetCollection(*set, *index, *kind)
        }
        IteratorState::ArrayLike {
            object,
            index,
            kind,
        } => FastIteratorSnapshot::ArrayLike(*object, *index, *kind),
        IteratorState::Exhausted { .. } => FastIteratorSnapshot::Exhausted,
        IteratorState::User { .. }
        | IteratorState::RegExpString { .. }
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
        FastIteratorSnapshot::ArrayKey(array, index) => {
            if index >= crate::array::len(array, gc_heap) {
                None
            } else {
                gc_heap.with_payload(iter, |state| {
                    if let IteratorState::ArrayKey { index, .. } = state {
                        *index += 1;
                    }
                });
                Some(Value::number(crate::number::NumberValue::from_f64(
                    index as f64,
                )))
            }
        }
        FastIteratorSnapshot::ArrayEntry(array, index) => {
            if index >= crate::array::len(array, gc_heap) {
                None
            } else {
                let v = crate::array::get(array, gc_heap, index);
                let index_val = Value::number(crate::number::NumberValue::from_f64(index as f64));
                // Materialise [index, value] dense array. Roots both
                // operands via the visitor so a GC during allocation
                // sees them.
                let pair = {
                    let array_root = Value::array(array);
                    let mut visitor = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                        array_root.trace_value_slots(visit);
                        index_val.trace_value_slots(visit);
                        v.trace_value_slots(visit);
                    };
                    crate::array::alloc_array_with_roots(gc_heap, &mut visitor)
                        .map_err(|_| VmError::TypeMismatch)?
                };
                crate::array::with_elements_mut(pair, gc_heap, |elements| {
                    elements.push(index_val);
                    elements.push(v);
                });
                gc_heap.with_payload(iter, |state| {
                    if let IteratorState::ArrayEntry { index, .. } = state {
                        *index += 1;
                    }
                });
                Some(Value::array(pair))
            }
        }
        FastIteratorSnapshot::ArrayLike(object, index, kind) => {
            // §23.1.5.2.1 %ArrayIteratorPrototype%.next over a generic
            // array-like object: re-read `length` and the element each
            // step so a mutation between calls is observed. Reads go
            // through the object's own data slots (`arguments`-style
            // array-likes), matching the other heap-only fast iterators.
            let len = match object
                .as_object()
                .and_then(|obj| crate::object::get(obj, gc_heap, "length"))
            {
                Some(v) => to_length(&v, gc_heap)?,
                None => 0,
            };
            if index >= len {
                None
            } else {
                let advance = |gc_heap: &mut otter_gc::GcHeap| {
                    gc_heap.with_payload(iter, |state| {
                        if let IteratorState::ArrayLike { index, .. } = state {
                            *index += 1;
                        }
                    });
                };
                match kind {
                    ArrayIterKind::Key => {
                        advance(gc_heap);
                        Some(Value::number(crate::number::NumberValue::from_f64(
                            index as f64,
                        )))
                    }
                    ArrayIterKind::Value => {
                        let v = object
                            .as_object()
                            .and_then(|obj| crate::object::get(obj, gc_heap, &index.to_string()))
                            .unwrap_or_else(Value::undefined);
                        advance(gc_heap);
                        Some(v)
                    }
                    ArrayIterKind::Entry => {
                        let element = object
                            .as_object()
                            .and_then(|obj| crate::object::get(obj, gc_heap, &index.to_string()))
                            .unwrap_or_else(Value::undefined);
                        let index_val =
                            Value::number(crate::number::NumberValue::from_f64(index as f64));
                        let pair = {
                            let mut visitor = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                                index_val.trace_value_slots(visit);
                                element.trace_value_slots(visit);
                            };
                            crate::array::alloc_array_with_roots(gc_heap, &mut visitor)
                                .map_err(|_| VmError::TypeMismatch)?
                        };
                        crate::array::with_elements_mut(pair, gc_heap, |elements| {
                            elements.push(index_val);
                            elements.push(element);
                        });
                        advance(gc_heap);
                        Some(Value::array(pair))
                    }
                }
            }
        }
        FastIteratorSnapshot::TypedArray(typed_array, index, kind) => {
            // §23.1.5.1 CreateArrayIterator step — for a typed array the
            // closure rebuilds a buffer-witness record each step and
            // throws a TypeError when the array is out of bounds (a
            // shrunk resizable buffer or a detached one); otherwise it
            // reads the live element and terminates at the live length.
            if typed_array.is_out_of_bounds(gc_heap) {
                return Err(VmError::TypeError {
                    message: "typed array is out of bounds".to_string(),
                });
            }
            if index >= typed_array.length(gc_heap) {
                None
            } else {
                let element = typed_array
                    .get(gc_heap, index)
                    .map_err(|_| VmError::TypeMismatch)?;
                let advance = |gc_heap: &mut otter_gc::GcHeap| {
                    gc_heap.with_payload(iter, |state| {
                        if let IteratorState::TypedArray { index, .. } = state {
                            *index += 1;
                        }
                    });
                };
                match kind {
                    ArrayIterKind::Key => {
                        advance(gc_heap);
                        Some(Value::number(crate::number::NumberValue::from_f64(
                            index as f64,
                        )))
                    }
                    ArrayIterKind::Value => {
                        advance(gc_heap);
                        Some(element)
                    }
                    ArrayIterKind::Entry => {
                        let index_val =
                            Value::number(crate::number::NumberValue::from_f64(index as f64));
                        let pair = {
                            let mut visitor = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                                index_val.trace_value_slots(visit);
                                element.trace_value_slots(visit);
                            };
                            crate::array::alloc_array_with_roots(gc_heap, &mut visitor)
                                .map_err(|_| VmError::TypeMismatch)?
                        };
                        crate::array::with_elements_mut(pair, gc_heap, |elements| {
                            elements.push(index_val);
                            elements.push(element);
                        });
                        advance(gc_heap);
                        Some(Value::array(pair))
                    }
                }
            }
        }
        FastIteratorSnapshot::String(string, index) => {
            // §22.1.5.1 `%StringIteratorPrototype%.next`.
            if let Some(unit) = string.char_code_at(index, gc_heap) {
                let next_unit = string.char_code_at(index + 1, gc_heap);
                let is_pair = (0xD800..=0xDBFF).contains(&unit)
                    && matches!(next_unit, Some(low) if (0xDC00..=0xDFFF).contains(&low));
                let (s, advance) = if is_pair {
                    let pair = [unit, next_unit.unwrap()];
                    (JsString::from_utf16_units(&pair, gc_heap)?, 2)
                } else {
                    (JsString::from_utf16_units(&[unit], gc_heap)?, 1)
                };
                gc_heap.with_payload(iter, |state| {
                    if let IteratorState::String { index, .. } = state {
                        *index += advance;
                    }
                });
                Some(Value::string(s))
            } else {
                None
            }
        }
        FastIteratorSnapshot::MapCollection(map, index, kind) => {
            let raw_len = crate::collections::map_raw_len(map, gc_heap);
            let mut next_index = index;
            let mut next_entry = None;
            while next_index < raw_len {
                let probe_index = next_index;
                next_index += 1;
                if let Some(entry) = crate::collections::map_entry_at(map, gc_heap, probe_index) {
                    next_entry = Some(entry);
                    break;
                }
            }
            if let Some((key, value)) = next_entry {
                gc_heap.with_payload(iter, |state| {
                    if let IteratorState::MapCollection { index, .. } = state {
                        *index = next_index;
                    }
                });
                Some(match kind {
                    MapIteratorKind::Key => key,
                    MapIteratorKind::Value => value,
                    MapIteratorKind::Entry => {
                        let pair = {
                            let map_root = Value::map(map);
                            let mut visitor = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                                map_root.trace_value_slots(visit);
                                key.trace_value_slots(visit);
                                value.trace_value_slots(visit);
                            };
                            crate::array::alloc_array_with_roots(gc_heap, &mut visitor)
                                .map_err(|_| VmError::TypeMismatch)?
                        };
                        crate::array::with_elements_mut(pair, gc_heap, |elements| {
                            elements.push(key);
                            elements.push(value);
                        });
                        Value::array(pair)
                    }
                })
            } else {
                None
            }
        }
        FastIteratorSnapshot::SetCollection(set, index, kind) => {
            let raw_len = crate::collections::set_raw_len(set, gc_heap);
            let mut next_index = index;
            let mut next_value = None;
            while next_index < raw_len {
                let probe_index = next_index;
                next_index += 1;
                if let Some(value) = crate::collections::set_value_at(set, gc_heap, probe_index) {
                    next_value = Some(value);
                    break;
                }
            }
            if let Some(value) = next_value {
                gc_heap.with_payload(iter, |state| {
                    if let IteratorState::SetCollection { index, .. } = state {
                        *index = next_index;
                    }
                });
                Some(match kind {
                    SetIteratorKind::Value => value,
                    SetIteratorKind::Entry => {
                        let pair = {
                            let set_root = Value::set(set);
                            let mut visitor = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                                set_root.trace_value_slots(visit);
                                value.trace_value_slots(visit);
                            };
                            crate::array::alloc_array_with_roots(gc_heap, &mut visitor)
                                .map_err(|_| VmError::TypeMismatch)?
                        };
                        crate::array::with_elements_mut(pair, gc_heap, |elements| {
                            elements.push(value);
                            elements.push(value);
                        });
                        Value::array(pair)
                    }
                })
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
            gc_heap.with_payload(iter, |state| state.exhaust());
            Ok((Value::undefined(), true))
        }
    }
}

/// `true` when `value` is a `JsObject` whose internal native
/// call slot carries a native function, i.e. it is
/// callable even though it is not a plain function value.
fn object_has_call_slot(value: &Value, heap: &otter_gc::GcHeap) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    crate::object::call_native(obj, heap).is_some_and(|v| v.is_native_function())
}

/// `true` when `value` is a VM constructor. This is intentionally
/// stricter than `IsCallable`: callable ordinary objects such as
/// `Function.prototype` must reject `new`.
fn is_constructor_runtime(
    value: &Value,
    context: &ExecutionContext,
    heap: &otter_gc::GcHeap,
) -> bool {
    if let Some(bound) = value.as_bound_function() {
        let (target, _, _) = bound.parts(heap);
        is_constructor_runtime(&target, context, heap)
    } else {
        abstract_ops::is_constructor(value, context, heap) || object_has_construct_slot(value, heap)
    }
}

/// `true` when `value` is a `JsObject` whose internal native
/// constructor slot carries a native function, i.e. it is
/// admissible as a `new` callee even though it is not a plain
/// function value.
fn object_has_construct_slot(value: &Value, heap: &otter_gc::GcHeap) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    crate::object::constructor_native(obj, heap).is_some_and(|v| v.is_native_function())
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
        ToPrimitiveStage::SymbolToPrim
        | ToPrimitiveStage::SymbolResult
        | ToPrimitiveStage::Exhausted => "",
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
            length: param_count,
            own_upvalue_count: 0,
            is_strict: false,
            is_arrow: false,
            is_method: false,
            has_rest: false,
            is_async: false,
            is_generator: false,
            is_async_generator: false,
            is_derived_constructor: false,
            is_module: false,
            needs_arguments: false,
            arguments_object_kind: ArgumentsObjectKind::Unmapped,
            mapped_argument_bindings: Vec::new(),
            source_text: None,
            source_text_span: None,
            module_url: String::new(),
            direct_eval_bindings: Vec::new(),
            contains_direct_eval: false,
            code,
            spans,
        }
    }

    fn module_with(code: Vec<Instruction>, scratch: u16) -> BytecodeModule {
        BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
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
        assert_eq!(interp.run(&context).unwrap(), Value::undefined());
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
            template_sites: Vec::new(),
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
            Value::number(NumberValue::Smi(312))
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
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 4, main_code), callee],
            constants: vec![Constant::FunctionId { index: 1 }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let context = ExecutionContext::from_module(module);
        let Some(args) = (interp.run(&context).unwrap()).as_object() else {
            panic!("expected arguments object");
        };
        assert_eq!(
            object::get(args, interp.gc_heap(), "0"),
            Some(Value::number(NumberValue::Smi(34)))
        );
        assert_eq!(
            object::get(args, interp.gc_heap(), "1"),
            Some(Value::number(NumberValue::Smi(21)))
        );
        assert_eq!(
            object::get(args, interp.gc_heap(), "length"),
            Some(Value::number(NumberValue::Smi(2)))
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
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 5, main_code), callee],
            constants: vec![Constant::FunctionId { index: 1 }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let context = ExecutionContext::from_module(module);
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let Some(rest) = (interp.run(&context).unwrap()).as_array() else {
            panic!("expected rest array");
        };
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        let elements = array::with_elements(rest, interp.gc_heap(), |elements| elements.to_vec());
        assert_eq!(
            elements,
            vec![
                Value::number(NumberValue::Smi(13)),
                Value::number(NumberValue::Smi(8))
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
            template_sites: Vec::new(),
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
        assert!(interp.run(&context).unwrap().is_function());
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "StoreProperty should allocate function user props in young space"
        );
        let desc = interp
            .ordinary_function_own_property_descriptor(Some(&context), None, 1, "custom")
            .unwrap()
            .expect("custom property descriptor");
        assert_eq!(
            descriptor_value(&desc),
            Value::number(NumberValue::from_i32(42))
        );
    }

    #[test]
    fn bytecode_function_prototype_uses_young_allocation_with_frame_roots() {
        let module = module_with(Vec::new(), 4);
        let context = ExecutionContext::from_module(module.clone());
        let mut interp = Interpreter::new();
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::function(1);
        stack.push(frame);

        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let prototype = interp
            .function_property_get_stack_rooted(&context, &stack, None, 1, "prototype")
            .expect("prototype");
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Function .prototype should allocate user bag and prototype object in young space"
        );

        let Some(proto) = (prototype).as_object() else {
            panic!("function prototype should be an object");
        };
        assert_eq!(
            object::get(proto, interp.gc_heap(), "constructor"),
            Some(Value::function(1))
        );
    }

    #[test]
    fn runtime_function_prototype_uses_young_allocation_with_explicit_roots() {
        let module = module_with(Vec::new(), 4);
        let context = ExecutionContext::from_module(module);
        let mut interp = Interpreter::new();
        let target = Value::function(1);
        let arg = Value::string(JsString::from_str("rooted-arg", interp.gc_heap_mut()).unwrap());
        let args = [arg];

        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let prototype = interp
            .function_property_get_runtime_rooted(
                &context,
                None,
                1,
                "prototype",
                &[&target],
                &[&args],
            )
            .expect("prototype");
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Function .prototype should allocate through runtime roots when no VM frame is active"
        );

        let Some(proto) = (prototype).as_object() else {
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
        frame.registers[1] = Value::object(lhs);
        frame.registers[2] = Value::function(1);
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
        assert_eq!(stack[0].registers[0], Value::boolean(false));
        let desc = interp
            .ordinary_function_own_property_descriptor(Some(&context), None, 1, "prototype")
            .unwrap()
            .expect("prototype descriptor");
        assert!(descriptor_value(&desc).is_object());
    }

    #[test]
    fn new_function_links_eval_chunk_into_shared_code_space() {
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
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 1, compiled_main), inner],
            constants: vec![Constant::FunctionId { index: 1 }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let outer = module_with(Vec::new(), 4);
        let mut interp = Interpreter::new();
        let context = interp.link_module(outer);
        interp.set_eval_hook(Some(std::sync::Arc::new(move |_, _| Ok(compiled.clone()))));
        let arg = Value::string(JsString::from_str("", interp.gc_heap_mut()).unwrap());
        let args = [arg];

        let result = interp
            .build_dynamic_function(
                &context,
                args.as_slice(),
                crate::eval_ops::DynamicFunctionKind::Normal,
            )
            .expect("Function constructor");

        let fid = result.as_function().expect("plain function value");
        assert_eq!(
            fid, 2,
            "eval chunk ids rebase past the outer chunk's single function"
        );
        let function = context
            .function(fid)
            .expect("foreign id resolves through the shared code space");
        assert_eq!(function.name, "anonymous");
    }

    #[test]
    fn get_iterator_map_snapshot_uses_old_iterator_state_allocation_with_frame_roots() {
        let module = module_with(Vec::new(), 5);
        let mut interp = Interpreter::new();
        let map = crate::collections::alloc_map(interp.gc_heap_mut()).unwrap();
        crate::collections::map_set(
            map,
            interp.gc_heap_mut(),
            Value::number(NumberValue::from_i32(1)),
            Value::number(NumberValue::from_i32(10)),
        )
        .unwrap();
        crate::collections::map_set(
            map,
            interp.gc_heap_mut(),
            Value::number(NumberValue::from_i32(2)),
            Value::number(NumberValue::from_i32(20)),
        )
        .unwrap();

        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::map(map);
        stack.push(frame);

        let before = interp.gc_heap_mut().stats().old_allocated_bytes;
        interp.run_get_iterator_regs(&mut stack, 0, 1, 0).unwrap();
        let after = interp.gc_heap_mut().stats().old_allocated_bytes;
        assert!(
            after > before,
            "GetIterator over Map should allocate its iterator state in non-moving old space"
        );

        interp
            .run_iterator_next_regs(&mut stack[0], 2, 3, 1)
            .unwrap();
        assert_eq!(stack[0].registers[3], Value::boolean(false));
        let Some(pair) = (stack[0].registers[2]).as_array() else {
            panic!("Map iterator should yield entry arrays");
        };
        let values =
            crate::array::with_elements(pair, interp.gc_heap(), |elements| elements.to_vec());
        assert_eq!(
            values,
            vec![
                Value::number(NumberValue::from_i32(1)),
                Value::number(NumberValue::from_i32(10)),
            ]
        );
    }

    #[test]
    fn get_iterator_user_resume_uses_old_iterator_state_allocation_with_frame_roots() {
        let module = module_with(Vec::new(), 4);
        let mut interp = Interpreter::new();
        let iterator_obj = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();

        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.pc = 0;
        interp.frame_ensure_cold(&mut frame).pending_get_iterator =
            Some(PendingGetIterator { pc: 0, dst: 1 });
        frame.registers[1] = Value::object(iterator_obj);
        stack.push(frame);
        let context = ExecutionContext::from_module(module);
        let operands = vec![Operand::Register(1), Operand::Register(0)];

        let before = interp.gc_heap_mut().stats().old_allocated_bytes;
        assert!(
            interp
                .drive_get_iterator(&mut stack, &context, &operands)
                .unwrap()
        );
        let after = interp.gc_heap_mut().stats().old_allocated_bytes;

        assert!(
            after > before,
            "GetIterator resume should allocate user iterator state in non-moving old space"
        );
        assert!(stack[0].registers[1].is_iterator());
        assert!(
            interp
                .frame_cold(&stack[0])
                .is_none_or(|c| c.pending_get_iterator.is_none())
        );
        assert_eq!(stack[0].pc, 1);
    }

    #[test]
    fn array_callback_map_uses_stack_rooted_result_allocation() {
        fn identity_mapper(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            Ok(args.first().cloned().unwrap_or(Value::undefined()))
        }

        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
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
            [Value::number(NumberValue::from_i32(12))],
        )
        .unwrap();
        let mapper =
            native_value_static(interp.gc_heap_mut(), "identityMapper", 1, identity_mapper)
                .unwrap();
        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::array(source);
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
        let Some(result) = (stack[0].registers[2]).as_array() else {
            panic!("map should return an array");
        };
        assert_eq!(
            crate::array::get(result, interp.gc_heap(), 0),
            Value::number(NumberValue::from_i32(12))
        );
    }

    #[test]
    fn call_method_on_nullish_receiver_reports_type_error() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "foo".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::undefined();
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("nullish method call should reject before intrinsic fallback");

        assert!(matches!(
            err,
            VmError::TypeError { message } if message == "Cannot read properties of undefined"
        ));
    }

    #[test]
    fn call_method_on_missing_primitive_method_reports_not_callable() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "missing".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::number_i32(1);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("missing primitive method should reject as non-callable");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_string_prototype_non_callable_shadows_builtin() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "slice".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("String")
            .expect("String.prototype")
            .as_object()
            .expect("String.prototype object");
        object::set(proto, interp.gc_heap_mut(), "slice", Value::number_i32(1));
        let recv = Value::string(JsString::from_str("abc", interp.gc_heap_mut()).unwrap());

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = recv;
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable String.prototype.slice should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_number_prototype_non_callable_shadows_builtin() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "toString".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("Number")
            .expect("Number.prototype")
            .as_object()
            .expect("Number.prototype object");
        object::set(
            proto,
            interp.gc_heap_mut(),
            "toString",
            Value::number_i32(1),
        );

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::number_i32(7);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable Number.prototype.toString should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_boolean_prototype_non_callable_shadows_builtin() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "valueOf".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("Boolean")
            .expect("Boolean.prototype")
            .as_object()
            .expect("Boolean.prototype object");
        object::set(proto, interp.gc_heap_mut(), "valueOf", Value::number_i32(1));

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::boolean(true);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable Boolean.prototype.valueOf should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_bigint_prototype_non_callable_shadows_builtin() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "toString".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("BigInt")
            .expect("BigInt.prototype")
            .as_object()
            .expect("BigInt.prototype object");
        object::set(
            proto,
            interp.gc_heap_mut(),
            "toString",
            Value::number_i32(1),
        );
        let bigint = crate::bigint::BigIntValue::from_i32(interp.gc_heap_mut(), 7)
            .expect("bigint allocation");

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::big_int(bigint);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable BigInt.prototype.toString should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_symbol_prototype_non_callable_shadows_builtin() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "valueOf".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("Symbol")
            .expect("Symbol.prototype")
            .as_object()
            .expect("Symbol.prototype object");
        object::set(proto, interp.gc_heap_mut(), "valueOf", Value::number_i32(1));
        let symbol = JsSymbol::new(interp.gc_heap_mut(), None).expect("symbol allocation");

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::symbol(symbol);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable Symbol.prototype.valueOf should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_weak_ref_prototype_non_callable_shadows_builtin() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "deref".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("WeakRef")
            .expect("WeakRef.prototype")
            .as_object()
            .expect("WeakRef.prototype object");
        object::set(proto, interp.gc_heap_mut(), "deref", Value::number_i32(1));
        let target = Value::object(
            crate::object::alloc_object_old_for_fixture(interp.gc_heap_mut())
                .expect("target object"),
        );
        let weak_ref =
            crate::test_support::alloc_weak_ref(interp.gc_heap_mut(), &target).expect("weak ref");

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::weak_ref(weak_ref);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable WeakRef.prototype.deref should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_finalization_registry_prototype_non_callable_shadows_builtin() {
        fn cleanup(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
            Ok(Value::undefined())
        }

        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "unregister".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("FinalizationRegistry")
            .expect("FinalizationRegistry.prototype")
            .as_object()
            .expect("FinalizationRegistry.prototype object");
        object::set(
            proto,
            interp.gc_heap_mut(),
            "unregister",
            Value::number_i32(1),
        );
        let cleanup = native_value_static(interp.gc_heap_mut(), "cleanup", 0, cleanup)
            .expect("cleanup function");
        let registry =
            crate::test_support::alloc_finalization_registry(interp.gc_heap_mut(), cleanup)
                .expect("registry");

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::finalization_registry(registry);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err(
                "non-callable FinalizationRegistry.prototype.unregister should shadow builtin",
            );

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_promise_expando_non_callable_shadows_builtin() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "then".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let promise = promise_dispatch::pending_runtime_rooted(&mut interp, &[], &[]).unwrap();
        let bag = property_dispatch::promise_ensure_expando_pub(interp.gc_heap_mut(), &promise)
            .expect("promise expando");
        object::set(bag, interp.gc_heap_mut(), "then", Value::number_i32(1));

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::promise(promise);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable own promise method should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_promise_prototype_non_callable_shadows_builtin() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "then".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let promise = promise_dispatch::pending_runtime_rooted(&mut interp, &[], &[]).unwrap();
        let proto = interp
            .constructor_prototype_value("Promise")
            .expect("Promise.prototype")
            .as_object()
            .expect("Promise.prototype object");
        object::set(proto, interp.gc_heap_mut(), "then", Value::number_i32(1));

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::promise(promise);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable Promise.prototype.then should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_array_own_non_callable_shadows_builtin() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "map".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let array = crate::array::from_elements_old_for_fixture(
            interp.gc_heap_mut(),
            [Value::number_i32(1)],
        )
        .expect("array allocation");
        crate::array::set_named_property(array, interp.gc_heap_mut(), "map", Value::number_i32(1))
            .expect("array expando property");

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::array(array);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable own array method should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_regexp_own_non_callable_shadows_builtin() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "exec".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let units: Vec<u16> = "x".encode_utf16().collect();
        let regexp = JsRegExp::compile(interp.gc_heap_mut(), &units, "").expect("regexp");
        let bag = property_dispatch::regexp_ensure_expando_pub(interp.gc_heap_mut(), &regexp)
            .expect("regexp expando");
        object::set(bag, interp.gc_heap_mut(), "exec", Value::number_i32(1));

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::regexp(regexp);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable own regexp method should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_regexp_prototype_non_callable_shadows_builtin() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "exec".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("RegExp")
            .expect("RegExp.prototype")
            .as_object()
            .expect("RegExp.prototype object");
        object::set(proto, interp.gc_heap_mut(), "exec", Value::number_i32(1));
        let units: Vec<u16> = "x".encode_utf16().collect();
        let regexp = JsRegExp::compile(interp.gc_heap_mut(), &units, "").expect("regexp");

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::regexp(regexp);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable RegExp.prototype.exec should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_date_prototype_non_callable_shadows_builtin() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "getTime".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("Date")
            .expect("Date.prototype")
            .as_object()
            .expect("Date.prototype object");
        object::set(proto, interp.gc_heap_mut(), "getTime", Value::number_i32(1));
        let date =
            crate::object::alloc_object_old_for_fixture(interp.gc_heap_mut()).expect("date object");
        object::set_prototype(date, interp.gc_heap_mut(), Some(proto));
        object::set_date_data(date, interp.gc_heap_mut(), 0.0);

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::object(date);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable Date.prototype.getTime should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_date_setter_prototype_non_callable_shadows_builtin() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "setTime".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("Date")
            .expect("Date.prototype")
            .as_object()
            .expect("Date.prototype object");
        object::set(proto, interp.gc_heap_mut(), "setTime", Value::number_i32(1));
        let date =
            crate::object::alloc_object_old_for_fixture(interp.gc_heap_mut()).expect("date object");
        object::set_prototype(date, interp.gc_heap_mut(), Some(proto));
        object::set_date_data(date, interp.gc_heap_mut(), 0.0);

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::object(date);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable Date.prototype.setTime should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_typed_array_own_non_callable_shadows_builtin() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "map".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let buffer =
            crate::binary::alloc_local_array_buffer(interp.gc_heap_mut(), vec![0], None, None)
                .expect("array buffer");
        let buffer = crate::binary::JsArrayBuffer::from_local_handle(buffer);
        let typed_array = crate::binary::JsTypedArray::new(
            interp.gc_heap_mut(),
            buffer,
            crate::binary::TypedArrayKind::Int8,
            0,
            1,
        )
        .expect("typed array");
        let bag =
            property_dispatch::typed_array_ensure_expando_pub(interp.gc_heap_mut(), &typed_array)
                .expect("typed array expando");
        object::set(bag, interp.gc_heap_mut(), "map", Value::number_i32(1));

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::typed_array(typed_array);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable own typed array method should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_typed_array_callback_prototype_non_callable_shadows_builtin() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "map".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("Int8Array")
            .expect("Int8Array.prototype")
            .as_object()
            .expect("Int8Array.prototype object");
        object::set(proto, interp.gc_heap_mut(), "map", Value::number_i32(1));
        let buffer =
            crate::binary::alloc_local_array_buffer(interp.gc_heap_mut(), vec![0], None, None)
                .expect("array buffer");
        let buffer = crate::binary::JsArrayBuffer::from_local_handle(buffer);
        let typed_array = crate::binary::JsTypedArray::new(
            interp.gc_heap_mut(),
            buffer,
            crate::binary::TypedArrayKind::Int8,
            0,
            1,
        )
        .expect("typed array");

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::typed_array(typed_array);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable Int8Array.prototype.map should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_typed_array_slice_prototype_non_callable_shadows_builtin() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "slice".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("Int8Array")
            .expect("Int8Array.prototype")
            .as_object()
            .expect("Int8Array.prototype object");
        object::set(proto, interp.gc_heap_mut(), "slice", Value::number_i32(1));
        let buffer =
            crate::binary::alloc_local_array_buffer(interp.gc_heap_mut(), vec![0], None, None)
                .expect("array buffer");
        let buffer = crate::binary::JsArrayBuffer::from_local_handle(buffer);
        let typed_array = crate::binary::JsTypedArray::new(
            interp.gc_heap_mut(),
            buffer,
            crate::binary::TypedArrayKind::Int8,
            0,
            1,
        )
        .expect("typed array");

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::typed_array(typed_array);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable Int8Array.prototype.slice should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_iterator_prototype_non_callable_shadows_helper() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 3, Vec::new())],
            constants: vec![Constant::String {
                utf16: "toArray".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("Iterator")
            .expect("Iterator.prototype")
            .as_object()
            .expect("Iterator.prototype object");
        object::set(proto, interp.gc_heap_mut(), "toArray", Value::number_i32(1));
        let source = crate::array::from_elements_old_for_fixture(
            interp.gc_heap_mut(),
            [Value::number(NumberValue::from_i32(1))],
        )
        .expect("source array");

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::array(source);
        stack.push(frame);
        interp
            .run_get_iterator_regs(&mut stack, 0, 1, 0)
            .expect("array iterator");
        stack[0].registers[0] = stack[0].registers[1];

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable Iterator.prototype.toArray should shadow helper");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_map_prototype_non_callable_shadows_builtin_for_each() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "forEach".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("Map")
            .expect("Map.prototype")
            .as_object()
            .expect("Map.prototype object");
        object::set(proto, interp.gc_heap_mut(), "forEach", Value::number_i32(1));
        let map = crate::collections::alloc_map(interp.gc_heap_mut()).expect("map");

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::map(map);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable Map.prototype.forEach should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_set_prototype_non_callable_shadows_builtin_for_each() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "forEach".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("Set")
            .expect("Set.prototype")
            .as_object()
            .expect("Set.prototype object");
        object::set(proto, interp.gc_heap_mut(), "forEach", Value::number_i32(1));
        let set = crate::collections::alloc_set(interp.gc_heap_mut()).expect("set");

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::set(set);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable Set.prototype.forEach should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_map_prototype_non_callable_shadows_map_method() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "get".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("Map")
            .expect("Map.prototype")
            .as_object()
            .expect("Map.prototype object");
        object::set(proto, interp.gc_heap_mut(), "get", Value::number_i32(1));
        let map = crate::collections::alloc_map(interp.gc_heap_mut()).expect("map");

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::map(map);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable Map.prototype.get should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_set_prototype_non_callable_shadows_set_add() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "add".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("Set")
            .expect("Set.prototype")
            .as_object()
            .expect("Set.prototype object");
        object::set(proto, interp.gc_heap_mut(), "add", Value::number_i32(1));
        let set = crate::collections::alloc_set(interp.gc_heap_mut()).expect("set");

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::set(set);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable Set.prototype.add should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_weak_map_prototype_non_callable_shadows_weak_map_method() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "get".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("WeakMap")
            .expect("WeakMap.prototype")
            .as_object()
            .expect("WeakMap.prototype object");
        object::set(proto, interp.gc_heap_mut(), "get", Value::number_i32(1));
        let weak_map = crate::collections::alloc_weak_map(interp.gc_heap_mut()).expect("weak map");

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::weak_map(weak_map);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable WeakMap.prototype.get should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_weak_set_prototype_non_callable_shadows_weak_set_method() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "add".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("WeakSet")
            .expect("WeakSet.prototype")
            .as_object()
            .expect("WeakSet.prototype object");
        object::set(proto, interp.gc_heap_mut(), "add", Value::number_i32(1));
        let weak_set = crate::collections::alloc_weak_set(interp.gc_heap_mut()).expect("weak set");

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::weak_set(weak_set);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable WeakSet.prototype.add should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_array_buffer_prototype_non_callable_shadows_builtin() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "slice".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("ArrayBuffer")
            .expect("ArrayBuffer.prototype")
            .as_object()
            .expect("ArrayBuffer.prototype object");
        object::set(proto, interp.gc_heap_mut(), "slice", Value::number_i32(1));
        let buffer =
            crate::binary::alloc_local_array_buffer(interp.gc_heap_mut(), vec![0], None, None)
                .expect("array buffer");
        let buffer = crate::binary::JsArrayBuffer::from_local_handle(buffer);

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::array_buffer(buffer);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable ArrayBuffer.prototype.slice should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_data_view_prototype_non_callable_shadows_builtin() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "getUint8".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("DataView")
            .expect("DataView.prototype")
            .as_object()
            .expect("DataView.prototype object");
        object::set(
            proto,
            interp.gc_heap_mut(),
            "getUint8",
            Value::number_i32(1),
        );
        let buffer =
            crate::binary::alloc_local_array_buffer(interp.gc_heap_mut(), vec![0], None, None)
                .expect("array buffer");
        let buffer = crate::binary::JsArrayBuffer::from_local_handle(buffer);
        let view =
            crate::binary::JsDataView::new(interp.gc_heap_mut(), buffer, 0, 1).expect("data view");

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::data_view(view);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable DataView.prototype.getUint8 should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_set_prototype_non_callable_shadows_es_set_method() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "union".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("Set")
            .expect("Set.prototype")
            .as_object()
            .expect("Set.prototype object");
        object::set(proto, interp.gc_heap_mut(), "union", Value::number_i32(1));
        let set = crate::collections::alloc_set(interp.gc_heap_mut()).expect("set");

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::set(set);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable Set.prototype.union should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_function_own_non_callable_shadows_call() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(1, "target", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "call".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::function(1);
        stack.push(frame);
        let function_value = Value::function(1);
        let bag = interp
            .function_user_bag_stack_rooted(&stack, None, 1, &[&function_value])
            .expect("function user bag");
        object::set(bag, interp.gc_heap_mut(), "call", Value::number_i32(1));

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable own function call should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_function_own_non_callable_shadows_object_method() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(1, "target", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "hasOwnProperty".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::function(1);
        stack.push(frame);
        let function_value = Value::function(1);
        let bag = interp
            .function_user_bag_stack_rooted(&stack, None, 1, &[&function_value])
            .expect("function user bag");
        object::set(
            bag,
            interp.gc_heap_mut(),
            "hasOwnProperty",
            Value::number_i32(1),
        );

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("non-callable own hasOwnProperty should shadow Object.prototype");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_null_proto_object_missing_object_method_is_not_callable() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "hasOwnProperty".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let obj = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).expect("object");
        object::set_prototype(obj, interp.gc_heap_mut(), None);

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::object(obj);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err("null-prototype object should not inherit Object.prototype methods");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_native_function_object_prototype_non_callable_shadows_builtin() {
        fn noop(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
            Ok(Value::undefined())
        }

        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "hasOwnProperty".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("Object")
            .expect("Object.prototype")
            .as_object()
            .expect("Object.prototype object");
        object::set(
            proto,
            interp.gc_heap_mut(),
            "hasOwnProperty",
            Value::number_i32(1),
        );
        let native = native_value_static(interp.gc_heap_mut(), "target", 0, noop).expect("native");

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = native;
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err(
                "non-callable Object.prototype.hasOwnProperty should shadow native intercept",
            );

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_primitive_object_prototype_non_callable_shadows_builtin() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
            constants: vec![Constant::String {
                utf16: "hasOwnProperty".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("Object")
            .expect("Object.prototype")
            .as_object()
            .expect("Object.prototype object");
        object::set(
            proto,
            interp.gc_heap_mut(),
            "hasOwnProperty",
            Value::number_i32(1),
        );

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::number_i32(1);
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            )
            .expect_err(
                "non-callable Object.prototype.hasOwnProperty should shadow primitive intercept",
            );

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn call_method_string_wrapper_replace_own_non_callable_shadows_builtin() {
        fn replacement(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
            Ok(Value::undefined())
        }

        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 4, Vec::new())],
            constants: vec![Constant::String {
                utf16: "replace".encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let proto = interp
            .constructor_prototype_value("String")
            .expect("String.prototype")
            .as_object()
            .expect("String.prototype object");
        let obj =
            object::alloc_object_old_for_fixture(interp.gc_heap_mut()).expect("string wrapper");
        object::set_prototype(obj, interp.gc_heap_mut(), Some(proto));
        let data = JsString::from_str("abc", interp.gc_heap_mut()).expect("string data");
        object::set_string_data(obj, interp.gc_heap_mut(), data);
        object::set(obj, interp.gc_heap_mut(), "replace", Value::number_i32(1));
        let search = Value::string(JsString::from_str("a", interp.gc_heap_mut()).expect("search"));
        let repl =
            native_value_static(interp.gc_heap_mut(), "replacement", 1, replacement).expect("repl");

        let context = ExecutionContext::from_module(module.clone());
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = Value::object(obj);
        frame.registers[1] = search;
        frame.registers[2] = repl;
        stack.push(frame);

        let err = interp
            .do_call_method_value(
                &mut stack,
                &context,
                &[
                    Operand::Register(3),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(2),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            )
            .expect_err("non-callable own String wrapper replace should shadow builtin");

        assert!(matches!(err, VmError::NotCallable));
    }

    #[test]
    fn array_symbol_iterator_factory_uses_native_rooted_iterator_allocation() {
        let module = module_with(Vec::new(), 2);
        let context = ExecutionContext::from_module(module);
        let mut interp = Interpreter::new();
        let source = crate::array::from_elements_old_for_fixture(
            interp.gc_heap_mut(),
            [Value::number(NumberValue::from_i32(21))],
        )
        .unwrap();
        let factory = make_array_iterator_factory(source, interp.gc_heap_mut()).unwrap();
        let Some(native) = (factory).as_native_function() else {
            panic!("Array iterator factory should be native");
        };
        let call = native.call_target(interp.gc_heap());
        let before = interp.gc_heap_mut().stats().old_allocated_bytes;
        let call_info = NativeCallInfo::call(Value::undefined());
        let mut ctx =
            NativeCtx::new_with_call_info_and_context(&mut interp, call_info, Some(context));

        let result = call.invoke(&mut ctx, &[]).expect("invoke iterator factory");

        let after = interp.gc_heap_mut().stats().old_allocated_bytes;
        assert!(
            after > before,
            "Array[Symbol.iterator] factory should allocate iterator state in non-moving old space"
        );
        let Some(iter) = (result).as_iterator() else {
            panic!("factory should return an iterator");
        };
        let (array, index) = interp.gc_heap().read_payload(iter, |state| match state {
            IteratorState::Array { array, index, .. } => (*array, *index),
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
            Value::number(NumberValue::from_i32(3)),
            Value::number(NumberValue::from_i32(30)),
        )
        .unwrap();
        let map_value = Value::map(map);
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        let entries = interp
            .iterator_to_list_sync(&context, &map_value)
            .expect("map entries");

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "iterator_to_list_sync Map fast path should allocate pair arrays through runtime roots"
        );
        let Some(pair) = entries.first().and_then(|v| v.as_array()) else {
            panic!("expected pair array");
        };
        assert_eq!(
            crate::array::get(pair, interp.gc_heap(), 0),
            Value::number(NumberValue::from_i32(3))
        );
        assert_eq!(
            crate::array::get(pair, interp.gc_heap(), 1),
            Value::number(NumberValue::from_i32(30))
        );
    }

    #[test]
    fn iterator_result_record_uses_runtime_rooted_young_allocation() {
        let mut interp = Interpreter::new();
        let value = Value::number(NumberValue::from_i32(44));
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        let result = interp
            .make_runtime_rooted_iter_result(value, true, &[], &[])
            .unwrap();

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "IteratorResult records should allocate through runtime roots"
        );
        let Some(record) = (result).as_object() else {
            panic!("IteratorResult should be an object");
        };
        assert_eq!(object::get(record, interp.gc_heap(), "value"), Some(value));
        assert_eq!(
            object::get(record, interp.gc_heap(), "done"),
            Some(Value::boolean(true))
        );
    }

    #[test]
    fn new_collection_map_uses_root_aware_allocation_with_frame_roots() {
        let mut interp = Interpreter::new();
        let pair = crate::array::from_elements_old_for_fixture(
            interp.gc_heap_mut(),
            [
                Value::number(NumberValue::from_i32(1)),
                Value::number(NumberValue::from_i32(10)),
            ],
        )
        .unwrap();
        let seed =
            crate::array::from_elements_old_for_fixture(interp.gc_heap_mut(), [Value::array(pair)])
                .unwrap();
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
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
        frame.registers[1] = Value::array(seed);
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
        let Some(map) = (stack[0].registers[0]).as_map() else {
            panic!("NewCollection Map should write a Map");
        };
        assert_eq!(
            crate::collections::map_get(
                map,
                interp.gc_heap(),
                &Value::number(NumberValue::from_i32(1))
            ),
            Some(Value::number(NumberValue::from_i32(10)))
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
        let Some(obj) = (interp.run(&context).unwrap()).as_object() else {
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

        let error = interp
            .vm_error_to_throwable_with_stack_roots(&stack, &VmError::TypeMismatch)
            .and_then(|v| v.as_object())
            .expect("TypeMismatch should convert to a throwable object");

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "VM error throwable conversion should allocate through stack roots"
        );
        let message_value = object::get(error, interp.gc_heap(), "message");
        let heap_ref = interp.gc_heap();
        let message = message_value
            .as_ref()
            .and_then(|v| v.as_string(heap_ref))
            .expect("message string");
        assert!(message.to_lossy_string(heap_ref).contains("type mismatch"));
    }

    #[test]
    fn oom_throwable_uses_range_error_prototype() {
        let module = module_with(Vec::new(), 1);
        let mut interp = Interpreter::new();
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(Frame::for_function(&module.functions[0]));

        let error = interp
            .vm_error_to_throwable_with_stack_roots(
                &stack,
                &VmError::OutOfMemory {
                    requested_bytes: 160,
                    heap_limit_bytes: 2 * 1024 * 1024,
                },
            )
            .and_then(|v| v.as_object())
            .expect("OutOfMemory should convert to a throwable object");

        assert!(object::has_in_proto_chain(
            error,
            interp.gc_heap(),
            interp.error_classes.prototype(ErrorKind::RangeError)
        ));
    }

    #[test]
    fn host_rooted_object_and_array_helpers_use_young_allocation() {
        let mut interp = Interpreter::new();
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;

        let host = interp
            .alloc_host_object_with_roots(&[], &[])
            .expect("host object allocation");
        let host_root = Value::object(host);
        let elements = [Value::number(NumberValue::from_i32(1))];
        let array = interp
            .array_from_elements_host_rooted(
                elements.iter().cloned(),
                &[&host_root],
                &[elements.as_slice()],
            )
            .expect("host array allocation");
        object::set(host, interp.gc_heap_mut(), "items", Value::array(array));

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "host-rooted object and array helpers should allocate in young space"
        );
        assert!(object::get(host, interp.gc_heap(), "items").is_some_and(|v| v.is_array()));
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
        assert!(interp.run(&context).unwrap().is_weak_ref());
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
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, main_code), cleanup],
            constants: vec![Constant::FunctionId { index: 1 }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let context = ExecutionContext::from_module(module);
        assert!(interp.run(&context).unwrap().is_finalization_registry());
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
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 3, main_code), callee],
            constants: vec![Constant::FunctionId { index: 1 }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let context = ExecutionContext::from_module(module);
        let Some(promise) = (interp.run(&context).unwrap()).as_promise() else {
            panic!("expected async function call to return a promise");
        };
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "async bytecode calls should allocate their result promise in young space"
        );
        assert_eq!(
            promise.state(interp.gc_heap()),
            crate::promise::PromiseState::Fulfilled(Value::number(NumberValue::Smi(144)))
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
            template_sites: Vec::new(),
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
        frame.registers[0] = Value::generator(generator);
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
        assert!(stack[0].registers[0].is_promise());
    }

    #[test]
    fn primitive_wrapper_boxing_uses_stack_rooted_young_allocation() {
        let main = test_function(0, "<main>", 0, 1, Vec::new());
        let callee = test_function(1, "sloppy_callee", 0, 1, Vec::new());
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
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
                Value::number(NumberValue::from_i32(33)),
                &[],
            )
            .expect("boxed this");
        let primitive_string =
            Value::string(crate::JsString::from_str("abc", interp.gc_heap_mut()).unwrap());
        let property_base = interp
            .object_for_primitive_property_base_stack_rooted(&stack, &primitive_string)
            .expect("property base")
            .expect("primitive base");

        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "primitive wrapper boxing should allocate through stack-rooted young allocation"
        );
        assert!(boxed_this.is_object());
        assert!(Value::object(property_base).is_object());
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
            template_sites: Vec::new(),
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
            Value::number(NumberValue::Smi(512))
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
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, main_code)],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let context = ExecutionContext::from_module(module);
        let Some(promise) = (interp.run(&context).unwrap()).as_promise() else {
            panic!("expected promise");
        };
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;
        assert!(
            after > before,
            "PromiseFulfilledOf should allocate the promise body in young space"
        );
        assert_eq!(
            promise.state(interp.gc_heap()),
            crate::promise::PromiseState::Fulfilled(Value::number(NumberValue::Smi(211)))
        );
    }

    #[test]
    fn await_non_promise_uses_stack_rooted_wrapper_allocation() {
        let mut function = test_function(0, "async_body", 0, 1, Vec::new());
        function.is_async = true;
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
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
                Value::number(NumberValue::Smi(307)),
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
            Ok(Value::undefined())
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
        assert!(stack[0].registers[0].is_promise());
    }

    #[test]
    fn dynamic_import_rejection_uses_stack_rooted_promise_allocation() {
        let module = module_with(Vec::new(), 2);
        let context = ExecutionContext::from_module(module.clone());
        let mut interp = Interpreter::new();
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[1] = Value::number(NumberValue::Smi(12));
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
        let Some(promise) = (stack[0].registers[0]).as_promise() else {
            panic!("expected promise");
        };
        let crate::promise::PromiseState::Rejected(reason_value) = promise.state(interp.gc_heap())
        else {
            panic!("expected TypeError rejection object");
        };
        let reason = reason_value
            .as_object()
            .expect("expected TypeError rejection object");
        let msg = object::get(reason, interp.gc_heap(), "message");
        let heap_ref = interp.gc_heap();
        let message = msg
            .as_ref()
            .and_then(|v| v.as_string(heap_ref))
            .expect("message string");
        // §13.3.10: the numeric specifier is coerced via ToString to
        // "12", then rejected because no module resolves under that
        // name (no loader is installed in this test).
        assert!(
            message
                .to_lossy_string(heap_ref)
                .contains("module not resolvable")
        );
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
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 4, main_code), ctor],
            constants: vec![Constant::FunctionId { index: 1 }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let context = ExecutionContext::from_module(module);
        let Some(args) = (interp.run(&context).unwrap()).as_object() else {
            panic!("expected constructor-returned arguments object");
        };
        assert_eq!(
            object::get(args, interp.gc_heap(), "0"),
            Some(Value::number(NumberValue::Smi(89)))
        );
        assert_eq!(
            object::get(args, interp.gc_heap(), "1"),
            Some(Value::number(NumberValue::Smi(55)))
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
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 2, main_code), ctor],
            constants: vec![Constant::FunctionId { index: 1 }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let context = ExecutionContext::from_module(module);
        assert!(interp.run(&context).unwrap().is_object());
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
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 4, main_code), ctor],
            constants: vec![Constant::FunctionId { index: 1 }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let mut interp = Interpreter::new();
        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let context = ExecutionContext::from_module(module);
        assert!(interp.run(&context).unwrap().is_object());
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
        assert_eq!(interp.run(&context).unwrap(), Value::undefined());
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
        assert!(interp.run(&context).unwrap().is_object());
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
        assert!(interp.run(&context).unwrap().is_object());
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
        assert!(interp.run(&context).unwrap().is_array());
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
        let Some(array) = (result).as_array() else {
            panic!("ArrayPush program should return the grown array");
        };
        let values =
            crate::array::with_elements(array, interp.gc_heap_mut(), |elements| elements.to_vec());
        assert_eq!(values.len(), 5);
        assert_eq!(values[4], Value::number(NumberValue::from_i32(5)));
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
        let Some(array) = (result).as_array() else {
            panic!("StoreElement program should return the grown array");
        };
        let values =
            crate::array::with_elements(array, interp.gc_heap_mut(), |elements| elements.to_vec());
        assert_eq!(values.len(), 5);
        assert_eq!(values[4], Value::number(NumberValue::from_i32(99)));
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
            length: 0,
            own_upvalue_count: 0,
            is_strict: false,
            is_arrow: false,
            is_method: false,
            has_rest: false,
            is_async: false,
            is_generator: false,
            is_async_generator: false,
            is_derived_constructor: false,
            is_module: false,
            needs_arguments: false,
            arguments_object_kind: ArgumentsObjectKind::Unmapped,
            mapped_argument_bindings: Vec::new(),
            source_text: None,
            source_text_span: None,
            module_url: String::new(),
            direct_eval_bindings: Vec::new(),
            contains_direct_eval: false,
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
        let context = ExecutionContext::from_module(module_with(
            vec![Instruction {
                pc: 0,
                op: Op::ReturnUndefined,
                operands: vec![].into(),
            }],
            1,
        ));
        let mut interp = Interpreter::new();
        let err = interp
            .unwind_throw(&context, &mut stack, Value::boolean(true))
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
            length: 0,
            own_upvalue_count: 0,
            is_strict: false,
            is_arrow: false,
            is_method: false,
            has_rest: false,
            is_async: false,
            is_generator: false,
            is_async_generator: false,
            is_derived_constructor: false,
            is_module: false,
            needs_arguments: false,
            arguments_object_kind: ArgumentsObjectKind::Unmapped,
            mapped_argument_bindings: Vec::new(),
            source_text: None,
            source_text_span: None,
            module_url: String::new(),
            direct_eval_bindings: Vec::new(),
            contains_direct_eval: false,
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
        let mut interp = Interpreter::new();
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&main);
        interp
            .frame_ensure_cold(&mut frame)
            .handlers
            .push(TryHandler {
                catch_pc: Some(42),
                finally_pc: None,
                exc_register: 1,
            });
        stack.push(frame);
        let context = ExecutionContext::from_module(module_with(
            vec![Instruction {
                pc: 0,
                op: Op::ReturnUndefined,
                operands: vec![].into(),
            }],
            2,
        ));
        interp
            .unwind_throw(&context, &mut stack, Value::boolean(true))
            .unwrap();
        assert_eq!(stack[0].pc, 42);
        assert_eq!(stack[0].registers[1], Value::boolean(true));
        assert!(
            interp
                .frame_cold(&stack[0])
                .is_none_or(|c| c.handlers.is_empty())
        );
    }

    #[test]
    fn is_callable_recognises_call_shapes() {
        assert!(is_callable(&Value::function(7)));
        let mut closure_heap = otter_gc::GcHeap::new().expect("closure heap");
        let closure_handle =
            crate::closure::alloc_closure(&mut closure_heap, 7, Vec::new(), None, None, None, None)
                .expect("closure");
        assert!(is_callable(&Value::closure(closure_handle)));
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let bound = BoundFunction::new(
            &mut heap,
            Value::function(7),
            Value::undefined(),
            SmallVec::new(),
        )
        .expect("bound");
        assert!(is_callable(&Value::bound_function(bound)));
        assert!(!is_callable(&Value::number(NumberValue::Smi(1))));
        assert!(!is_callable(&Value::object(
            crate::object::alloc_object_old_for_fixture(&mut heap).unwrap()
        )));
    }

    #[test]
    fn native_call_context_receives_method_receiver() {
        fn return_this(ctx: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
            Ok(*ctx.this_value())
        }

        let module = module_with(vec![], 1);
        let mut interp = Interpreter::new();
        let callee = native_value_static(interp.gc_heap_mut(), "returnThis", 0, return_this)
            .expect("native");
        let receiver = Value::object(
            crate::object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap(),
        );
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(Frame::for_function(&module.functions[0]));
        let context = ExecutionContext::from_module(module.clone());

        interp
            .invoke(&mut stack, &context, &callee, receiver, SmallVec::new(), 0)
            .unwrap();

        assert_eq!(stack[0].registers[0], receiver);
    }

    #[test]
    fn direct_native_call_uses_contiguous_argument_window() {
        fn sum_smi_args(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            let mut sum = 0;
            for arg in args {
                match arg.as_number().and_then(|n| n.as_smi()) {
                    Some(n) => sum += n,
                    None => {
                        return Err(NativeError::TypeError {
                            name: "sum",
                            reason: "expected smi".to_string(),
                        });
                    }
                }
            }
            Ok(Value::number(NumberValue::Smi(sum)))
        }

        let module = module_with(vec![], 4);
        let mut interp = Interpreter::new();
        let callee =
            native_value_static(interp.gc_heap_mut(), "sum", 2, sum_smi_args).expect("native");
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = callee;
        frame.registers[1] = Value::number(NumberValue::Smi(8));
        frame.registers[2] = Value::number(NumberValue::Smi(13));
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

        assert_eq!(stack[0].registers[3], Value::number(NumberValue::Smi(21)));
    }

    #[test]
    fn proxy_call_argv_array_uses_young_allocation_with_frame_roots() {
        fn return_argv_array(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            Ok(args.get(2).cloned().unwrap_or(Value::undefined()))
        }

        fn target_noop(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
            Ok(Value::undefined())
        }

        let module = module_with(vec![], 4);
        let mut interp = Interpreter::new();
        let apply =
            native_value_static(interp.gc_heap_mut(), "apply", 3, return_argv_array).unwrap();
        let target = native_value_static(interp.gc_heap_mut(), "target", 0, target_noop).unwrap();
        let handler = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();
        object::set(handler, interp.gc_heap_mut(), "apply", apply);
        let proxy = Value::proxy(
            crate::proxy::JsProxy::new(interp.gc_heap_mut(), target, Value::object(handler))
                .unwrap(),
        );

        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&module.functions[0]);
        frame.registers[0] = proxy;
        frame.registers[1] = Value::number(NumberValue::Smi(7));
        frame.registers[2] = Value::number(NumberValue::Smi(11));
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

        let Some(argv) = (stack[0].registers[3]).as_array() else {
            panic!("expected proxy apply argv array");
        };
        let elements = array::with_elements(argv, interp.gc_heap(), |elements| elements.to_vec());
        assert_eq!(
            elements,
            vec![
                Value::number(NumberValue::Smi(7)),
                Value::number(NumberValue::Smi(11)),
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
            Ok(args.get(2).cloned().unwrap_or(Value::undefined()))
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
            template_sites: Vec::new(),
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
        let proxy = Value::proxy(
            crate::proxy::JsProxy::new(
                interp.gc_heap_mut(),
                Value::function(1),
                Value::object(handler),
            )
            .unwrap(),
        );

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

        assert!(stack[0].registers[0].is_proxy());
        assert!(
            after > before,
            "proxy construct argv array should allocate in young space"
        );
    }

    #[test]
    fn run_callable_sync_proxy_argv_array_uses_runtime_rooted_young_allocation() {
        fn return_argv_array(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            Ok(args.get(2).cloned().unwrap_or(Value::undefined()))
        }

        fn target_noop(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
            Ok(Value::undefined())
        }

        let module = module_with(Vec::new(), 1);
        let context = ExecutionContext::from_module(module);
        let mut interp = Interpreter::new();
        let apply =
            native_value_static(interp.gc_heap_mut(), "apply", 3, return_argv_array).unwrap();
        let target = native_value_static(interp.gc_heap_mut(), "target", 0, target_noop).unwrap();
        let handler = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();
        object::set(handler, interp.gc_heap_mut(), "apply", apply);
        let proxy = Value::proxy(
            crate::proxy::JsProxy::new(interp.gc_heap_mut(), target, Value::object(handler))
                .unwrap(),
        );
        let args: SmallVec<[Value; 8]> = smallvec::smallvec![
            Value::number(NumberValue::Smi(3)),
            Value::number(NumberValue::Smi(5)),
        ];

        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let result = interp
            .run_callable_sync(&context, &proxy, Value::undefined(), args)
            .unwrap();
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;

        let Some(argv) = (result).as_array() else {
            panic!("proxy apply trap should return the synthesized argv array");
        };
        let elements = array::with_elements(argv, interp.gc_heap(), |elements| elements.to_vec());
        assert_eq!(
            elements,
            vec![
                Value::number(NumberValue::Smi(3)),
                Value::number(NumberValue::Smi(5)),
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
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(0, "<main>", 0, 1, Vec::new()), ctor],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let context = ExecutionContext::from_module(module);
        let mut interp = Interpreter::new();
        let target = Value::function(1);

        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let result = interp
            .run_construct_sync(&context, &target, target, SmallVec::new())
            .unwrap();
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;

        assert!(result.is_object());
        assert!(
            after > before,
            "run_construct_sync should allocate the receiver in young space"
        );
    }

    #[test]
    fn run_construct_sync_proxy_argv_array_uses_runtime_rooted_young_allocation() {
        fn return_argv_array(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            Ok(args.get(1).cloned().unwrap_or(Value::undefined()))
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
            template_sites: Vec::new(),
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
        let proxy = Value::proxy(
            crate::proxy::JsProxy::new(
                interp.gc_heap_mut(),
                Value::function(1),
                Value::object(handler),
            )
            .unwrap(),
        );
        let args: SmallVec<[Value; 8]> = smallvec::smallvec![Value::number(NumberValue::Smi(13))];

        let before = interp.gc_heap_mut().stats().new_allocated_bytes;
        let result = interp
            .run_construct_sync(&context, &proxy, proxy, args)
            .unwrap();
        let after = interp.gc_heap_mut().stats().new_allocated_bytes;

        let Some(argv) = (result).as_array() else {
            panic!("proxy construct trap should return the synthesized argv array");
        };
        let elements = array::with_elements(argv, interp.gc_heap(), |elements| elements.to_vec());
        assert_eq!(elements, vec![Value::number(NumberValue::Smi(13))]);
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
            length: 0,
            own_upvalue_count: 0,
            is_strict: false,
            is_arrow: false,
            is_method: false,
            has_rest: false,
            is_async: false,
            is_generator: false,
            is_async_generator: false,
            is_derived_constructor: false,
            is_module: false,
            needs_arguments: false,
            arguments_object_kind: ArgumentsObjectKind::Unmapped,
            mapped_argument_bindings: Vec::new(),
            source_text: None,
            source_text_span: None,
            module_url: String::new(),
            direct_eval_bindings: Vec::new(),
            contains_direct_eval: false,
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
            length: 0,
            own_upvalue_count: 0,
            is_strict: false,
            is_arrow: true,
            is_method: false,
            has_rest: false,
            is_async: false,
            is_generator: false,
            is_async_generator: false,
            is_derived_constructor: false,
            is_module: false,
            needs_arguments: false,
            arguments_object_kind: ArgumentsObjectKind::Unmapped,
            mapped_argument_bindings: Vec::new(),
            source_text: None,
            source_text_span: None,
            module_url: String::new(),
            direct_eval_bindings: Vec::new(),
            contains_direct_eval: false,
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
            template_sites: Vec::new(),
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
        let bound = JsString::from_str("outer", interp.gc_heap_mut()).unwrap();
        let closure_handle = crate::closure::alloc_closure(
            interp.gc_heap_mut(),
            1,
            Vec::new(),
            Some(Value::string(bound)),
            None,
            None,
            None,
        )
        .expect("closure alloc");
        let closure = Value::closure(closure_handle);
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(Frame::for_function(&module.functions[0]));
        let context = ExecutionContext::from_module(module.clone());
        // Reserve a scratch slot in <main> to receive the result.
        stack[0].registers.push(Value::undefined());
        // Caller-supplied this is `Null` — the closure must override.
        interp
            .invoke(
                &mut stack,
                &context,
                &closure,
                Value::null(),
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
                let value = stack[top].registers[0];
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
                let value = stack[top].this_value;
                stack[top].registers[dst as usize] = value;
                stack[top].pc += 1;
                continue;
            }
            unreachable!();
        }
        assert_eq!(stack[0].registers[0], Value::string(bound));
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
