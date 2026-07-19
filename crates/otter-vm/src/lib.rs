//! Interpreter and value model for the Otter engine.
//!
//! # Contents
//! - [`Value`] — opaque NaN-boxed runtime value.
//! - [`Frame`] — compact call frame.
//! - [`Interpreter`] — match-based dispatch loop over the frozen
//!   executable view inside [`ExecutionContext`].
//! - [`tier_policy::OptimizingDecision`] — additive optimizing-tier promotion
//!   classification over hotness and feedback stability.
//! - [`InterruptFlag`] — atomic flag observed at back-edges; cheap.
//! - [`VmError`] — runtime errors the interpreter can raise.
//!
//! # Invariants
//! - One thread, one [`Interpreter`]. `Send`/`Sync` are not
//!   implemented.
//! - Protector and shape epochs are isolate-local monotonic counters; they are
//!   plain non-GC state and never enter the root graph.
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

pub(crate) mod abstract_ops;
pub mod active_frame;
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
mod call_feedback;
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
mod constructor_fast_path;
mod conversion;
mod cpu_profile;
pub mod date;
pub mod eval_env;
// `date` is a directory module — see `date/mod.rs`.
mod activation_stack;
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
pub mod cache_ir;
pub mod class_constructor;
pub mod deopt;
pub mod dynamic_import;
pub mod error_classes;
mod error_ops;
mod eval_ops;
mod executable;
pub mod execution_context;
#[path = "jit_feedback.rs"]
pub mod feedback;
pub mod field_repr;
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
pub mod handles;
pub mod heap_number;
pub mod host_completion;
pub mod inspect;
pub mod intl;
pub mod intrinsic_install;
pub mod intrinsics;
mod iterator_ops;
pub mod iterator_state;
pub mod jit;
pub mod jit_artifact;
mod jit_class_ops;
mod jit_class_value_ops;
mod jit_construct_ops;
mod jit_control_ops;
pub mod jit_debug;
mod jit_delete_ops;
mod jit_exception_ops;
/// Compatibility path for JIT consumers while feedback ownership migrates to
/// the tier-neutral [`feedback`] API.
#[doc(hidden)]
pub use feedback as jit_feedback;
mod jit_global_ops;
mod jit_iterator_ops;
mod jit_module_ops;
mod jit_object_protocol_ops;
mod jit_private_ops;
pub mod jit_registry;
mod jit_runtime_ops;
mod jit_scalar_ops;
mod jit_spread_call_ops;
mod jit_static_call_ops;
mod jit_structural_ops;
mod jit_super_ops;
mod jit_value_load_ops;
mod jit_variadic_ops;
pub mod js_surface;
pub mod json;
pub mod marshal;
pub mod math;
mod method_ops;
pub mod microtask;
mod module_ops;
mod module_records;
pub mod native_abi;
pub mod native_function;
pub mod number;
pub mod object;
mod object_internal_ops;
pub mod object_statics;
mod operand_decode;
pub mod pelt;
pub mod persistent_roots;
pub mod promise;
pub mod promise_dispatch;
mod promise_ops;
mod promise_rejection;
mod property_atom;
mod property_dispatch;
mod property_ic;
pub mod proxy;
pub mod realm_intrinsics;
pub mod reflect;
pub mod regexp;
pub mod regexp_prototype;
mod register_stack;
#[doc(hidden)]
pub mod rooting;
pub mod run_control;
mod runtime_activation;
pub mod runtime_budget;
pub mod runtime_cx;
pub mod runtime_state;
pub mod runtime_stubs;
pub mod source_registry;
mod static_call_ops;
mod static_load_ops;
pub mod string;
pub mod swar;
pub mod symbol;
pub mod symbol_dispatch;
pub mod symbol_prototype;
pub mod temporal;
pub mod tier_policy;
pub mod timers;
pub mod uint8_base64;
pub mod upvalue;
mod upvalue_source;
pub mod value;
pub mod weak_refs;

#[cfg(test)]
mod gc_invariants;
#[cfg(test)]
mod test_support;

pub use active_frame::{ActiveFrameError, ActiveFrameMut, ActiveFrameRef, ActiveFrameStorage};
pub use arithmetic_dispatch::NumericRuntimeOp;
pub use cpu_profile::CpuProfile;
pub use execution_context::ExecutionContext;
pub use frame_state::{
    AsyncFrameState, Frame, PendingBindFunction, PendingBindStage, PendingGetIterator,
    PendingIteratorNext, PendingToPrimitive, ToPrimitiveStage, TryHandler,
};
pub use jit_exception_ops::JitExceptionOutcome;
pub use jit_runtime_ops::{UnaryCoercionOp, UnaryPrimitiveHint};
pub use property_ic::PropertyIcStats;
pub use run_control::{
    DEFAULT_MAX_STACK_DEPTH, DEFAULT_MAX_SYNC_REENTRY_DEPTH, ErrorDetail, InterruptFlag,
    NO_HANDLER_OFFSET, RunError, StackFrameSnapshot, VmError,
};
pub use runtime_activation::RuntimeCall;

#[cfg(test)]
use otter_bytecode::ArgumentsObjectKind;
use otter_bytecode::{BytecodeModule, Op};
use smallvec::SmallVec;

use arithmetic_dispatch::{
    bigint_and_op, bigint_mul_op, bigint_or_op, bigint_sub_op, bigint_xor_op,
};
pub(crate) use error_ops::{
    native_to_vm_error, native_to_vm_error_with_stack, snapshot_frames, symbol_to_vm_error,
    vm_err_to_value,
};
pub use executable::code_block_cfg::{CodeBlockControlFlowView, CodeBlockExceptionRegion};
pub use executable::{CodeBlock, CodeBlockInstruction, OperandView};
use operand_decode::{apply_branch, const_operand, register_operand};

pub use activation_stack::{ActivationFloor, ActivationStack};
pub use array::JsArray;
pub use closure::{
    JS_CLOSURE_BODY_TYPE_TAG, JsClosure, JsClosureBody, alloc_closure, alloc_closure_with_roots,
};
pub use collections::{CollectionError, JsMap, JsSet, JsWeakMap, JsWeakSet, MapKey};
pub use console::{ConsoleLevel, ConsoleSink, ConsoleSinkHandle, StdConsoleSink};
pub use dynamic_import::{DynamicImportLoader, DynamicImportLoaderHandle, DynamicImportRegistry};
pub use error_classes::{ErrorClassRegistry, ErrorKind};
pub use handles::{HandleArena, Local};
pub use intl::{IntlKind, IntlPayload, JsIntl};
pub use jit::{
    JitArrayLayout, JitArrayMethod, JitArrayMethodKind, JitClosureCallLayout,
    JitCodeGenerationSnapshot, JitCodeResidency, JitCollectionAllocMethod, JitCollectionLayout,
    JitCollectionLeafMethod, JitCompileError, JitCompileRequest, JitCompileSnapshot,
    JitCompileStatus, JitCompilerHook, JitDirectCallKind, JitDirectCallThisMode, JitDirectCallee,
    JitExecOutcome, JitFunctionCode, JitInlineCallee, JitInlineMethod, JitInstructionMetadata,
    JitPrimitiveMethodGuard, JitRuntimeStubBinding, JitStaticNativeCall, JitStaticNativeCallKind,
    JitStringLayout, VmRuntimeActivation,
};
pub use jit_artifact::{
    JIT_ARTIFACT_BUNDLE_LIMIT, JIT_ARTIFACT_BYTE_LIMIT, JitArtifactBatch, JitArtifactBuildError,
    JitArtifactBundle, JitArtifactFile, JitArtifactFileName, JitArtifactIdentity,
    JitArtifactManifest, JitArtifactMetadata,
};
pub use jit_debug::{
    JIT_DEBUG_EVENT_LIMIT, JitCompilerDiagnostic, JitDebugCompileOutcome, JitDebugEvent,
    JitDebugReport, JitDebugRequest, JitDebugTarget, JitDebugTier, JitDirectCallLoweringOutcome,
    JitDirectCallLoweringRejectionReason, JitDirectCallPlanOutcome, JitDirectCallRejectionReason,
    JitInlineRejectionReason, JitStaticNativeCallLoweringOutcome,
    JitStaticNativeCallLoweringRejectionReason,
};
pub use js_surface::{
    AccessorSpec, Attr, ClassBuilder, ClassSpec, ConstSpec, ConstValue, ConstructorBuilder,
    ConstructorSpec, JsSurfaceError, MethodSpec, NamespaceBuilder, NamespaceSpec, ObjectBuilder,
    PropertySpec,
};
pub use microtask::{Microtask, MicrotaskError, MicrotaskKind, MicrotaskQueue};
pub use native_abi::{
    FrameStateId, NO_FRAME_STATE, NO_SAFEPOINT, NativeFrame, NativeFrameFlags, NativeFrameKind,
    RuntimeStubAllocContext, RuntimeStubClass, RuntimeStubDescriptor, RuntimeStubId,
    RuntimeStubResult, RuntimeStubResultPair, RuntimeStubStatus, STUB_COLLECTION_MAP_DELETE_ALLOC,
    STUB_COLLECTION_MAP_GET_ALLOC, STUB_COLLECTION_MAP_GET_LEAF, STUB_COLLECTION_MAP_HAS_ALLOC,
    STUB_COLLECTION_MAP_HAS_LEAF, STUB_COLLECTION_MAP_SET_ALLOC, STUB_COLLECTION_SET_ADD_ALLOC,
    STUB_COLLECTION_SET_DELETE_ALLOC, STUB_COLLECTION_SET_HAS_ALLOC, STUB_COLLECTION_SET_HAS_LEAF,
    STUB_JIT_BACKEDGE_POLL, STUB_STRING_CONCAT_ALLOC, SafepointId, SafepointRecord, TaggedLocation,
    TaggedLocationKind, VARIADIC_STUB_ARGUMENTS, VmFrameHeader, validate_stub_descriptor,
};
pub use native_function::{
    NativeCall, NativeError, NativeFastFn, NativeFn, NativeFunction, VmIntrinsicFunction,
    native_value, native_value_static, native_value_with_captures,
};
pub use number::{NumberValue, NumericOrdering};
pub use object::JsObject;
pub use persistent_roots::{PersistentRootId, PersistentRoots};
pub use promise::{
    JsPromise, JsPromiseHandle, PromiseCapability, PromiseReaction, PromiseSettleJobs,
    PromiseState, PromiseThenOutcome, PurePromise, ReactionKind,
};
pub use regexp::{JsRegExp, RegExpError, RegExpFlags};
pub use register_stack::RegisterWindow;
pub use string::{JsString, MAX_ROPE_DEPTH};
pub use symbol::{JsSymbol, SymbolBody, SymbolRegistry, WellKnown, WellKnownSymbols};
pub use temporal::{JsTemporal, TemporalKind, TemporalPayload};
pub use timers::{TimerCallbacks, TimerEntry, TimerScheduler, TimerSchedulerHandle};
pub use weak_refs::{JsFinalizationRegistry, JsWeakRef};

// Eight-byte tagged value. Canonical `Value` export.
pub use value::{Value, ValueKind};

/// Active-realm state that must switch together for cross-realm calls.
///
/// This keeps the first real realm slice compact: the global object, error
/// constructors, and intrinsic prototype caches whose identity is observable
/// through Test262 cross-realm checks.
#[derive(Clone)]
pub(crate) struct RealmState {
    /// Stable scalar identity. Function-to-realm metadata stores this id rather
    /// than a GC handle, so the metadata needs no separate root protocol.
    pub(crate) id: u32,
    pub(crate) global_this: JsObject,
    pub(crate) error_classes: ErrorClassRegistry,
    pub(crate) realm_intrinsics: realm_intrinsics::RealmIntrinsics,
    pub(crate) array_iterator_prototype: Option<JsObject>,
    pub(crate) map_iterator_prototype: Option<JsObject>,
    pub(crate) set_iterator_prototype: Option<JsObject>,
    pub(crate) string_iterator_prototype: Option<JsObject>,
    pub(crate) regexp_string_iterator_prototype: Option<JsObject>,
    pub(crate) iterator_helper_prototype: Option<JsObject>,
    pub(crate) wrap_for_valid_iterator_prototype: Option<JsObject>,
}

impl RealmState {
    pub(crate) fn trace_roots(&self, visitor: &mut gc_trace::GcRootVisitor<'_>) {
        use crate::gc_trace::GcTrace;

        self.global_this.trace_gc_roots(visitor);
        self.error_classes.trace_gc_roots(visitor);
        self.realm_intrinsics.trace_roots(visitor);
        for object in [
            &self.array_iterator_prototype,
            &self.map_iterator_prototype,
            &self.set_iterator_prototype,
            &self.string_iterator_prototype,
            &self.regexp_string_iterator_prototype,
            &self.iterator_helper_prototype,
            &self.wrap_for_valid_iterator_prototype,
        ]
        .into_iter()
        .filter_map(Option::as_ref)
        {
            object.trace_gc_roots(visitor);
        }
    }
}

impl otter_gc::ExtraRootSource for RealmState {
    fn visit_extra_roots(&self, visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)) {
        self.trace_roots(visitor);
    }
}

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
    UPVALUE_CELL_TYPE_TAG, UpvalueCell, UpvalueCellBody, alloc_upvalue, alloc_upvalue_with_roots,
    read_upvalue, store_upvalue,
};

pub use runtime_budget::{RuntimeBudget, RuntimeBudgetExceededAction, RuntimeBudgetStats};
pub use runtime_cx::{NativeCallInfo, NativeCtx, NativeScope};

use runtime_budget::RuntimeHeapSnapshot;

use otter_gc::raw::RawGc;

// ---------------------------------------------------------------------------
// `!Send + !Sync` static assertions for the new-engine VM.
//
// The VM and GC stay explicit-context and single-mutator: the
// interpreter, every GC handle, and every borrowed-context type must
// be `!Send + !Sync` so compile-fail tests reject any future edit
// that accidentally moves a VM handle into `tokio::spawn` or holds a
// `&mut RuntimeTurn` across `.await`.
//
// Spec:
// - <https://tc39.es/ecma262/#sec-agents>
// ---------------------------------------------------------------------------
static_assertions::assert_not_impl_any!(Interpreter: Send, Sync);
static_assertions::assert_not_impl_any!(crate::runtime_cx::NativeCtx<'static>: Send, Sync);
// `RuntimeTurn<'_>` is `pub(crate)` so we cannot name it directly in
// a `pub`-visible macro. The bound is enforced transitively because
// `RuntimeTurn<'rt>` holds `&'rt mut Interpreter`, and `Interpreter`
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

/// Aggregate native-tier runtime counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitRuntimeStats {
    /// Optimizing-tier function and OSR entries.
    pub optimized_entries: u64,
    /// Optimizing-tier entries materialized at a hot loop header.
    pub optimized_osr_entries: u64,
    /// Optimizing-tier exits that reconstructed and resumed an interpreter frame.
    pub optimized_deopts: u64,
    /// Compiled closure-inline validation transitions.
    pub runtime_calls: u64,
    /// Compiled `Op::New` in-place construct transitions.
    pub runtime_constructs: u64,
    /// Compiler-generated stack-frame calls that entered native callee code.
    pub generated_calls: u64,
    /// Compiler-generated stack-frame calls that cold-deoptimized and resumed
    /// through the interpreter.
    pub generated_call_deopts: u64,
    /// Generated template-tier callee entries.
    pub generated_template_entries: u64,
    /// Generated template-tier callees that returned normally.
    pub generated_template_returns: u64,
    /// Generated template-tier callees that cold-deoptimized.
    pub generated_template_deopts: u64,
    /// Generated template-tier callees that propagated a throw.
    pub generated_template_throws: u64,
    /// Generated optimizing-tier callee entries.
    pub generated_optimizing_entries: u64,
    /// Generated optimizing-tier callees that returned normally.
    pub generated_optimizing_returns: u64,
    /// Generated optimizing-tier callees that cold-deoptimized.
    pub generated_optimizing_deopts: u64,
    /// Generated optimizing-tier callees that propagated a throw.
    pub generated_optimizing_throws: u64,
    /// Function-entry compile attempts across native tiers.
    pub compile_attempts: u64,
    /// Loop-OSR compile/entry attempts at threshold crossings.
    pub osr_attempts: u64,
    /// JIT property/method/element/global/upvalue runtime stub calls.
    pub runtime_property_stubs: u64,
    /// ABI-classified runtime stub transitions from compiled code.
    pub runtime_stub_transitions: u64,
    /// ABI-classified leaf runtime stubs. These are the desired hot-path shape.
    pub leaf_stub_transitions: u64,
    /// ABI-classified allocating runtime stubs that require a safepoint.
    pub alloc_stub_transitions: u64,
    /// ABI-classified re-entrant runtime stubs that may call JS/native code.
    pub reentrant_stub_transitions: u64,
    /// Executed `AllocValueStub` entries that returned `Ok`.
    pub alloc_value_stub_ok: u64,
    /// Executed `AllocValueStub` entries that returned `Miss`.
    pub alloc_value_stub_miss: u64,
    /// Executed `AllocValueStub` entries that returned `OutOfMemory`.
    pub alloc_value_stub_out_of_memory: u64,
    /// Executed `AllocValueStub` entries that returned another non-`Ok` status.
    pub alloc_value_stub_other: u64,
}

/// Snapshot of VM-published collection method IC mirror slots.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitCollectionMethodIcStats {
    /// Total mirror slots allocated for method call IC sites.
    pub slots: u64,
    /// Empty/uninitialized mirror slots.
    pub empty_slots: u64,
    /// Slots currently holding collection method feedback.
    pub collection_slots: u64,
    /// Collection slots with a leaf/no-allocation stub id.
    pub leaf_stub_slots: u64,
    /// Collection slots with an allocating stub id.
    pub alloc_stub_slots: u64,
}

/// Observed state for one `Op::CallMethodValue` site, the feedback the baseline
/// reads to decide whether to inline a tiny method body. A method site is
/// inlinable only when every observed call had the same receiver shape *and*
/// resolved to the same method function — so the emitter can guard the receiver
/// shape and method identity, then splice the body.
///
/// `recv_shape` is the receiver's stable VM-local hidden-class identity, so the
/// isolate-owned distribution never retains a movable GC handle. Absence from
/// the directory = unobserved.
/// Maximum number of distinct `(receiver shape, method)` targets one
/// polymorphic method-call site keeps for inline guard-chain baking. Mirrors
/// the V8/JSC polymorphic IC width: enough to cover real OO dispatch (a family
/// of sibling classes sharing a method name through one call site) while
/// bounding both the emitted guard-chain length and the number of
/// reoptimization evictions a single site can trigger as its target set grows.
///
/// A site with more shapes than this bakes none and side-exits at the method
/// call, which is far costlier than a few extra shape-guard compares. This
/// covers common wide-but-not-megamorphic dispatch (for example, six sibling
/// classes) without bloating every smaller guard chain.
/// The cap never lengthens the guard chain for a site with fewer shapes (only
/// baked shapes are walked), so raising it only helps wider sites.
pub(crate) const MAX_POLY_METHOD_TARGETS: usize = 8;

/// Longest prototype chain a method-call site's inline identity guard walks
/// from the receiver to the object holding the method slot. Deeper
/// resolutions stay uninlined (a chain this long is already rare; each hop
/// costs one flat-prototype chase plus shape compare per call).
pub(crate) const MAX_METHOD_PROTO_CHAIN: usize = 4;

/// Stable shape identities for each prototype hopped from the receiver to the method holder,
/// in hop order — the last entry is the holder's shape. Empty when the
/// method slot is an own property of the receiver. Each level's shape check
/// both pins that object's layout (an own-property insertion that would
/// shadow the method changes the shape) and validates the slot offset baked
/// for the holder.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MethodProtoChain {
    len: u8,
    shapes: [object::ShapeId; MAX_METHOD_PROTO_CHAIN],
}

impl MethodProtoChain {
    /// Chain for a method living directly on the receiver.
    pub(crate) fn own() -> Self {
        Self {
            len: 0,
            shapes: [object::ShapeId::UNASSIGNED; MAX_METHOD_PROTO_CHAIN],
        }
    }

    /// Append one hopped prototype's shape; `false` when the chain is full
    /// (the site must stay uninlined).
    pub(crate) fn push(&mut self, shape: object::ShapeId) -> bool {
        if (self.len as usize) == MAX_METHOD_PROTO_CHAIN {
            return false;
        }
        self.shapes[self.len as usize] = shape;
        self.len += 1;
        true
    }

    pub(crate) fn as_slice(&self) -> &[object::ShapeId] {
        &self.shapes[..self.len as usize]
    }

    /// Whether both chains walk the same stable shape identities.
    pub(crate) fn same(&self, other: &Self) -> bool {
        self.len == other.len
            && self
                .as_slice()
                .iter()
                .zip(other.as_slice())
                .all(|(a, b)| a == b)
    }
}

/// One observed `(receiver shape, resolved method)` target at a polymorphic
/// method-call site. Same layout data the baseline bakes for a monomorphic
/// inline guard ([`MethodCallFeedback::Mono`]), plus a `hits` counter so the
/// emitter can order the guard chain most-frequent-first.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PolyMethodTarget {
    pub(crate) method_fid: u32,
    pub(crate) recv_shape: object::ShapeId,
    pub(crate) proto_chain: MethodProtoChain,
    pub(crate) method_value_byte: u32,
    /// Observations that resolved to exactly this target. Used only to order
    /// the emitted guard chain; not a correctness input.
    pub(crate) hits: u32,
}

impl PolyMethodTarget {
    /// Whether `site`/`method_fid` describe the same layout+identity as this
    /// target (the inline guard for one cannot serve the other).
    fn matches(&self, method_fid: u32, site: &MethodSite) -> bool {
        self.method_fid == method_fid
            && self.recv_shape == site.recv_shape
            && self.proto_chain.same(&site.proto_chain)
            && self.method_value_byte == site.method_value_byte
    }
}

#[derive(Debug, Clone)]
pub(crate) enum MethodCallFeedback {
    /// One method function id and one receiver shape observed so far.
    ///
    /// `proto_chain` holds the shape of each prototype hopped to reach the
    /// object holding the method slot, and `method_value_byte` is that slot's
    /// byte offset within the holder's value slab — both captured at record
    /// time from the live receiver so the baseline can bake a fully-inline
    /// identity guard (chase the flat prototype per hop, guard each shape,
    /// load the slot, compare the closure's `function_id`) with no per-call
    /// runtime resolution.
    Mono {
        method_fid: u32,
        recv_shape: object::ShapeId,
        proto_chain: MethodProtoChain,
        method_value_byte: u32,
    },
    /// Two-to-[`MAX_POLY_METHOD_TARGETS`] distinct inlinable targets observed
    /// at this site. The baseline bakes one inline guard+body per target into a
    /// most-frequent-first chain; a receiver matching none of the guards falls
    /// to the exact method-call side exit. Still GC-safe and Send/Sync: each
    /// target only holds stable shape ids and a function id.
    ///
    /// Boxed: the inline target array dwarfs the `Mono` payload, and most
    /// sites stay monomorphic, so keeping it out of line keeps every
    /// feedback-map entry `Mono`-sized.
    Poly(Box<SmallVec<[PolyMethodTarget; MAX_POLY_METHOD_TARGETS]>>),
    /// More than [`MAX_POLY_METHOD_TARGETS`] distinct targets observed; the
    /// site is too polymorphic to inline profitably and always side-exits.
    Megamorphic,
}

/// Receiver/prototype layout snapshot for one observed `Op::CallMethodValue`,
/// captured from the live receiver *before* the call (the receiver handle may
/// move during the call, so its prototype shape and method slot offset are
/// resolved while it is still valid). Folded into [`MethodCallFeedback`] once
/// the resolved method id is known.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MethodSite {
    /// Stable receiver hidden-class identity.
    recv_shape: object::ShapeId,
    /// Stable identities of prototypes hopped to the method holder;
    /// empty when the method slot lives directly on the receiver.
    proto_chain: MethodProtoChain,
    /// Byte offset of the method slot within the holder's value slab.
    method_value_byte: u32,
}

/// Match-based dispatch loop. The harness baseline; slice tasks may
/// later switch to threaded dispatch after benchmark-driven review
/// (foundation plan §"Interpreter requirements").
pub struct Interpreter {
    /// §13.2.8.4 GetTemplateObject realm cache — one frozen
    /// template-strings object per tagged-template site, keyed by
    /// `(chunk function_base, site index)`.
    template_objects: rustc_hash::FxHashMap<(u32, u32), Value>,
    /// Per-context string constant cache. `LoadString` materializes immutable
    /// primitive string literals once per linked chunk identity and
    /// constant-pool index, then reuses the GC string handle on later
    /// executions. Values are traced from [`RuntimeState::trace_roots`] so a
    /// moving collection can rewrite cached handles in place.
    string_constant_cache: rustc_hash::FxHashMap<(usize, u32), Value>,
    /// Decimal strings for small non-negative integers, served on demand
    /// (JSC `SmallStrings` / V8 number-string-cache idea, adapted). Integer →
    /// string is one of the most repeated allocations in real code (`"" + n`,
    /// `(n).toString()`, integer keys); caching `0..SMALL_INT_STRING_CACHE`
    /// returns one immutable shared handle instead of allocating a fresh string
    /// body every conversion. Lazily filled; entries are traced from
    /// [`RuntimeState::trace_roots`] so a moving collection rewrites them.
    small_int_string_cache: Box<[Option<Value>]>,
    /// Per-context BigInt constant cache. BigInt primitives are immutable and
    /// have numeric, not object-identity, semantics, so `LoadBigInt` can parse
    /// and allocate each bytecode literal once per linked chunk identity and
    /// constant-pool index. Cached handles are traced with other runtime roots.
    bigint_constant_cache: rustc_hash::FxHashMap<(usize, u32), Value>,
    /// Prepared bytecode callback metadata for native loops that repeatedly
    /// call the same JS function. Entries are a strict stack: acquisition
    /// pushes the exact callable and scalar closure state, release pops it.
    /// Runtime root tracing reaches inherited cells through the callable itself;
    /// no second upvalue spine is cloned into this stack.
    lean_callback_roots: Vec<call_ops::LeanCallbackRoot>,
    /// Owned dynamic payload for the most recently raised [`VmError`]. The
    /// error itself is `Copy` (no drop glue on the hot `Result` chain); its
    /// message / name / structured payload is stashed here by the raising
    /// helpers (`type_error`, `range_error`, …) and read back at the surfacing
    /// boundary. Only one error is in flight per isolate at a time, so a single
    /// slot is sound; the next raise overwrites it. Not GC-traced — holds only
    /// owned strings / plain data. Interior-mutable so the `err_*` raising
    /// helpers take `&self` and stay callable from `&self` methods and from
    /// `ok_or_else` / `map_err` closures without borrow conflicts.
    pending_error_detail: std::cell::RefCell<Option<run_control::ErrorDetail>>,
    /// Scope-based GC handle storage for native value building. Handles minted
    /// through [`Self::with_handle_scope`] park their `Value` here; the runtime
    /// root walk traces every live slot and the collector rewrites it in place,
    /// so a handle read is never stale across an allocation. Native recursive
    /// algorithms such as JSON use strict-stack ranges in this same arena rather
    /// than maintaining parallel root stores. Truncated back to the opening
    /// length when each scope returns. See [`crate::handles`].
    handle_arena: handles::HandleArena,
    /// Byte length of the most recent `JSON.stringify` output, used to
    /// pre-size the next call's scratch buffer. Repeated stringify of
    /// similarly-shaped data then never re-grows the buffer from empty.
    json_stringify_capacity_hint: usize,
    /// Cumulative host-reported memory retained outside GC cells. The RAII
    /// token is declared before `gc_heap` so Rust drops it first, while its
    /// owning heap is still live.
    external_memory_adjustment: Option<otter_gc::ExternalMemory>,
    /// Protector for the array element store fast path: flips to
    /// `true` once any accessor descriptor lands on an array-index
    /// key anywhere (e.g. `Array.prototype[1] = {set}`); array index
    /// writes then re-check the prototype chain per OrdinarySet
    /// before creating an own element. Stays `false` for the
    /// overwhelmingly common unpolluted heap, keeping appends cheap.
    array_index_accessor_protector: bool,
    /// Monotonic version of `array_index_accessor_protector`. Advances exactly
    /// once, on the latch's sole `false -> true` transition.
    array_index_accessor_protector_epoch: u64,
    interrupt: InterruptFlag,
    /// Countdown of remaining compiled back-edges before the next cooperative
    /// budget checkpoint. Compiled code decrements this inline at every
    /// back-edge and re-enters [`Self::jit_backedge_poll`] only when it reaches
    /// zero (or the interrupt flag is set, polled inline every back-edge), which
    /// batches the reduction accounting the checkpoint would otherwise record one
    /// unit at a time per iteration. Reset to [`JIT_BACKEDGE_POLL_BATCH`] by the
    /// checkpoint.
    jit_backedge_fuel: u64,
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
    /// The most recent top-level [`ExecutionContext`] this interpreter ran.
    /// Every chunk links into the shared [`code_space`], so this context
    /// resolves function ids for any closure reachable in the realm — it is the
    /// universal fallback the microtask drain uses when a queued job carries no
    /// origin context (async-resume continuations, host-settled reactions),
    /// guaranteeing a drain never strands a deep chain for want of a context.
    realm_context: Option<ExecutionContext>,
    /// Interpreter-owned hidden-class side tables for GC-managed shapes.
    /// Runtime object storage uses the root, interned shape keys, and
    /// transition/cache tables here.
    shape_runtime: object::ShapeRuntime,
    /// Monotonic epoch for successful ordinary-object prototype changes made
    /// through the proxy-aware `[[SetPrototypeOf]]` funnel.
    shape_epoch: u64,
    /// Per-function cache for conservative base-class constructor initializers
    /// recognized by `constructor_fast_path`. Entries hold owned strings and
    /// source descriptors only; GC shape handles stay in `shape_runtime`.
    simple_constructor_init_cache:
        rustc_hash::FxHashMap<u32, Option<constructor_fast_path::SimpleConstructorInit>>,
    /// Final hidden class reached by a cached simple constructor initializer.
    /// These handles are traced explicitly because a moving GC must rewrite the
    /// cache slot, not only the owning `shape_runtime` transition table.
    simple_constructor_shape_cache: rustc_hash::FxHashMap<u32, object::ShapeHandle>,
    max_stack_depth: u32,
    /// Active synchronous JavaScript re-entry depth. Generated call sequences
    /// update this same counter inline through
    /// [`Self::jit_sync_reentry_depth_addr`].
    sync_reentry_depth: u32,
    /// Active compiler-generated JavaScript call frames that still live only
    /// on the native stack. Cold deoptimization temporarily transfers its
    /// current frame out of this count while the interpreter owns a
    /// materialized copy.
    jit_generated_call_depth: u32,
    /// Native-stack bytes reserved by active compiler-generated JS calls.
    /// Generated prologues update this inline and compare against
    /// [`Self::jit_native_stack_bytes_limit`].
    jit_native_stack_bytes: usize,
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
    /// Host-installed builtin module namespaces (e.g. `otter:kv`), keyed by
    /// specifier. Unlike [`Self::module_environments`] this cache survives
    /// [`Self::reset_module_state`] and is shared by the ESM and CommonJS
    /// loaders, so one runtime observes exactly one namespace object — and
    /// runs its installer's side effects exactly once — per builtin
    /// specifier, regardless of import style or how many programs run on
    /// the isolate. Traced as GC roots.
    host_module_env_cache: std::collections::HashMap<std::sync::Arc<str>, JsObject>,
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
    /// Per-load-site cache of resolved global declarative-record cells, keyed by
    /// `(function_id, name_constant_index)`. A global lexical binding's cell is
    /// stable for the realm's lifetime (lexical bindings are never deleted), so
    /// once a hot `LoadGlobalOrThrow` site resolves to one it can read the cell
    /// directly on every later hit — skipping the name-string hash, the const
    /// table lookup, and the object-record `[[Get]]` ladder. The cells are GC
    /// roots traced alongside [`Self::global_lexicals`]; only lexical hits are
    /// cached, so object-record globals never produce a stale entry.
    pub(crate) global_lexical_load_ic: rustc_hash::FxHashMap<(u32, u32), crate::UpvalueCell>,
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
    /// Isolate-owned high-level facade over CodeBlock lock-free feedback,
    /// executable property/method IC banks, and method target distributions.
    /// GC-bearing recipes stay behind this boundary and never enter CodeBlock
    /// feedback DTOs.
    feedback_directory: interp::FeedbackDirectory,
    /// Runtime-installed native-tier compiler hook. The hook lives behind a VM
    /// trait object so `otter-vm` never depends on executable-memory code.
    /// `Some` is also the tier-up gate: with no hook installed, all tier-up
    /// bookkeeping below stays untouched and execution is interpreter-only.
    jit_hook: Option<std::sync::Arc<dyn jit::JitCompilerHook>>,
    /// Owned, default-off JIT diagnostics. Enabled state retains only plain
    /// serializable data and never participates in the GC root graph.
    jit_debug: jit_debug::JitDebugState,
    /// Owned, bounded compile artifacts kept separate from hot installed code.
    /// Disabled state owns no bundle buffer.
    jit_artifacts: jit_artifact::JitArtifactState,
    /// Per-function call counter driving function-entry tier-up. Only mutated
    /// when a JIT hook is installed.
    jit_call_counts: rustc_hash::FxHashMap<u32, u32>,
    /// Isolate-local optimizing-tier feedback-stability telemetry. Entry
    /// selection samples it only after the shared call counter is hot.
    optimizing_tier_policy: tier_policy::TierPolicy,
    /// Per-function count of *entry* bails out of an installed compiled body
    /// (function-entry, sync-entry, and direct-call callee entries alike). A
    /// body that bails on every call — typically compiled early against
    /// feedback that later turned polymorphic — is worse than the interpreter:
    /// each call pays the compiled prologue, the failing guard, and the frame
    /// hand-off, then interprets anyway. At
    /// [`Self::JIT_ENTRY_BAIL_REOPT_THRESHOLD`] the body is evicted so the next
    /// resolve recompiles it against the richer feedback those interpreter
    /// completions recorded.
    jit_entry_bail_counts: rustc_hash::FxHashMap<u32, u32>,
    /// How many times a function's body has been evicted for recompilation by
    /// [`Self::note_jit_entry_bail`]. Bounded by
    /// [`Self::JIT_MAX_ENTRY_BAIL_REOPTS`]: a body still bail-looping after
    /// that many fresh-feedback recompiles is stuck on something feedback
    /// cannot express, and is pinned to the interpreter instead of thrashing
    /// the compiler.
    jit_entry_reopt_counts: rustc_hash::FxHashMap<u32, u32>,
    /// OSR targets that bailed, had no trampoline, or whose function is
    /// uncompilable; OSR is not retried for them. Keyed by `(function_id,
    /// loop_header_pc)` so a bail in one loop disables only *that* loop header,
    /// not the whole function — a later, genuinely-hot loop in the same function
    /// can still tier up. A `(fid, u32::MAX)` entry disables the whole function
    /// (its body did not compile at all). Consulted only at a threshold crossing
    /// (rare), so it adds no per-iteration cost.
    jit_osr_disabled: rustc_hash::FxHashSet<(u32, u32)>,
    /// Per-`(function_id, loop_header_pc)` back-edge counters driving loop-OSR
    /// tier-up. A single shared counter let a frequently-back-edging callee
    /// (e.g. a hot builtin loop) monopolize the count and starve a hot script
    /// loop that calls out — that loop then never tiered up. An independent
    /// counter per loop header lets every hot loop reach the threshold on its
    /// own. Only mutated when a JIT hook is installed; an entry is removed once
    /// its header tiers up (or is recorded disabled), so the map holds only the
    /// handful of loop headers currently warming up.
    jit_osr_counts: rustc_hash::FxHashMap<(u32, u32), u32>,
    /// Back-edge count at which a hot loop tiers up via OSR. Defaults to
    /// [`Self::JIT_OSR_THRESHOLD`]; embedders can override it explicitly through
    /// [`Self::set_jit_osr_threshold`].
    jit_osr_threshold: u32,
    /// Compiled-code cache keyed by global function id. `Some(code)` is an
    /// installed baseline body; `None` records a function the emitter could not
    /// compile (outside the supported subset), so it is never retried.
    jit_code: rustc_hash::FxHashMap<u32, Option<std::sync::Arc<dyn jit::JitFunctionCode>>>,
    /// Separately installed optimizing-tier bodies keyed by function id.
    /// `None` records a hot function outside the deliberately narrow subset.
    jit_optimized_code:
        rustc_hash::FxHashMap<u32, Option<std::sync::Arc<dyn jit::JitFunctionCode>>>,
    /// Single-entry cache over [`Self::jit_optimized_code`] for hot leaf calls.
    jit_optimized_code_cache: Option<(u32, std::sync::Arc<dyn jit::JitFunctionCode>)>,
    /// Feedback epoch at which a hot function last failed optimizing compilation.
    /// A back-edge only re-attempts the whole-body optimizer when the epoch has
    /// advanced, so a structurally-ineligible body is not recompiled on every hot
    /// loop iteration.
    jit_optimized_declined_epoch: rustc_hash::FxHashMap<u32, Option<u32>>,
    /// OSR-target compiled-code cache keyed by `(function_id, loop_header_pc)`.
    /// A target compile is not interchangeable with another header in the same
    /// function: its synthetic entry edge and captured OSR reload set are rooted
    /// at one loop header.
    jit_osr_code:
        rustc_hash::FxHashMap<(u32, u32), Option<std::sync::Arc<dyn jit::JitFunctionCode>>>,
    /// Single-entry monomorphic cache over [`Self::jit_code`] for repeated
    /// synchronous function entries. Records the last function id whose
    /// installed body is a non-OSR baseline body, so repeated resolution skips
    /// the map probe. Cleared whenever a new entry is inserted into `jit_code`.
    jit_code_cache: Option<(u32, std::sync::Arc<dyn jit::JitFunctionCode>)>,
    /// Function ids whose installed body compiled `osr_only` (unsupported opcodes
    /// emitted as bails), so the function-entry path can never run it — only a
    /// loop OSR enters at a supported header. Once compiled, a body's `osr_only`
    /// status is fixed, so caching it here lets the hot entry path short-circuit
    /// to the interpreter with one set probe, skipping the `jit_code` map lookup,
    /// the `Arc` clone (an atomic refcount round-trip), and the `osr_only()`
    /// virtual call on every invocation of an interpreter-resident method body.
    jit_entry_osr_only: rustc_hash::FxHashSet<u32>,
    /// Lightweight native-tier counters for OtterLab diagnostics.
    jit_runtime_stats: JitRuntimeStats,
    /// Address-stable registry of installed code objects behind the one
    /// published safepoint-resolver view; entries are registered at compile
    /// install and retained while any native frame can name them.
    jit_code_registry: Box<jit_registry::JitCodeRegistry>,
    /// Whether compiler-generated entry-cell feedback awaits one cold,
    /// outermost-activation reconciliation pass.
    jit_generated_feedback_pending: bool,
    /// Next unique code-object identity handed to a compile request.
    jit_next_code_object_id: u64,
    /// Flat register stack for JIT-built callee windows. Compiled code sets up a
    /// direct call by bumping `reg_top`, writing the callee's window into
    /// `reg_stack[reg_top..reg_top+regcount]`, and running the callee — no Rust
    /// VM-owned native register arena. Its published prefix is a precise GC
    /// root set for in-flight compiled call windows.
    register_stack: register_stack::RegisterStack,
    /// Fixed VM-owned descriptors for native JIT contexts whose scalar binding
    /// slots must stay visible to a moving collection across safepoints.
    jit_native_activations: Vec<jit::JitNativeActivation>,
    /// Live prefix of [`Self::jit_native_activations`].
    jit_native_activation_top: usize,
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
    /// Additional realm states created by host shells such as
    /// `$262.createRealm`. The first/default realm lives in the
    /// top-level interpreter fields for hot-path compatibility.
    extra_realms: Vec<RealmState>,
    /// Stable identity of the currently swapped-in realm. Realm zero is the
    /// default interpreter realm.
    active_realm_id: u32,
    /// Monotonic allocator for additional realm identities.
    next_realm_id: u32,
    /// ECMAScript function `[[Realm]]` metadata for linked bytecode chunks.
    /// Both key and value are scalars; all moving-GC state remains owned and
    /// traced by [`RealmState`].
    function_realm_ids: rustc_hash::FxHashMap<u32, u32>,
    /// `true` while an extra realm (not the first/default realm) is the
    /// active one. Array allocation stamps a per-instance prototype
    /// override only in that window: a default-realm array resolves its
    /// `[[Prototype]]` through the active realm intrinsics anyway, and
    /// stamping every array would materialize the exotic sidecar that
    /// disqualifies it from the dense fast paths.
    active_realm_is_extra: bool,
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
    /// HTML HostPromiseRejectionTracker state: rejected promises awaiting the
    /// post-drain `unhandledrejection`/`rejectionhandled` checkpoint. Both
    /// internal lists are GC roots. See [`crate::promise_rejection`].
    rejection_tracker: crate::promise_rejection::RejectionTracker,
    /// Stack-frame snapshot captured at the moment of the
    /// originating `Op::Throw` (before [`Self::unwind_throw`]
    /// pops handler-less frames). Surfaces as [`RunError::frames`]
    /// for [`VmError::Uncaught`] so embedders see the call site,
    /// not the empty post-unwind stack. Cleared at every `run_*`
    /// entry and at every successful catch.
    pending_uncaught_frames: Option<Vec<StackFrameSnapshot>>,
    /// Per-interpreter map of `module_url → source text` (+ line index),
    /// populated by the runtime module loader. Lets the VM resolve a
    /// frame's byte span to a `(line, column)` position for
    /// `Error.prototype.stack` and `util.getCallSites` without reaching
    /// back into the runtime layer.
    module_sources: source_registry::SourceRegistry,
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
    /// Per-instance own-property bags for object-shaped exotics whose
    /// payloads are not GC-managed objects yet. `Intl.*` instances are
    /// ordinary objects for user-visible own properties even though
    /// their internal slots live in compact non-object payloads.
    non_gc_exotic_user_props: std::collections::HashMap<usize, JsObject>,
    /// Generic persistent roots owned by host resources. Host data stores root
    /// ids from this table rather than raw JS values.
    persistent_roots: persistent_roots::PersistentRoots,
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
    /// Host completion sink backing async native methods — installed
    /// by the runtime layer like the timer scheduler; `None` in
    /// host-less embeddings (unit tests, sync-only hosts).
    host_completion_sink: Option<std::sync::Arc<dyn host_completion::HostCompletionSink>>,
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
    array_iterator_prototype: gc_trace::RootCell<Option<JsObject>>,
    map_iterator_prototype: gc_trace::RootCell<Option<JsObject>>,
    set_iterator_prototype: gc_trace::RootCell<Option<JsObject>>,
    string_iterator_prototype: gc_trace::RootCell<Option<JsObject>>,
    regexp_string_iterator_prototype: gc_trace::RootCell<Option<JsObject>>,
    iterator_helper_prototype: gc_trace::RootCell<Option<JsObject>>,
    wrap_for_valid_iterator_prototype: gc_trace::RootCell<Option<JsObject>>,
    /// Default-realm copies of the per-kind iterator prototypes above,
    /// captured at bootstrap and NEVER swapped on a realm switch. A
    /// builtin iterator with no stored prototype override belongs to
    /// the default realm (extra-realm iterators are stamped at
    /// creation), so its dynamic `[[GetPrototypeOf]]` must resolve
    /// here even while an extra realm is active — otherwise a
    /// default-realm iterator observed from `$262.createRealm()` code
    /// would claim the foreign realm's prototypes.
    /// Indexed by [`iterator_state::BuiltinIteratorOrigin`] discriminant
    /// order: array, map, set, string, regexp-string, helper,
    /// wrap-for-valid-iterator.
    default_realm_iterator_prototypes: [gc_trace::RootCell<Option<JsObject>>; 7],
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
    /// Optional VM stack sampler used by CLI/debug tooling to emit Chrome
    /// `.cpuprofile` and folded-stack artifacts.
    cpu_profiler: Option<cpu_profile::CpuProfiler>,
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

    /// Root-tracing view of cached string constants.
    pub(crate) fn string_constants_for_trace(&self) -> impl Iterator<Item = &Value> {
        self.string_constant_cache.values()
    }

    /// Upper bound (exclusive) of the cached small-integer decimal strings.
    pub(crate) const SMALL_INT_STRING_CACHE: i32 = 1024;

    /// GC-traced cached small-integer decimal strings.
    pub(crate) fn small_int_strings_for_trace(&self) -> impl Iterator<Item = &Value> {
        self.small_int_string_cache.iter().flatten()
    }

    /// Decimal string for a small non-negative integer, served from the
    /// `SmallStrings`-style cache. Allocates and caches on first use; returns
    /// the shared immutable handle thereafter. `None` for inputs outside
    /// `0..SMALL_INT_STRING_CACHE` (the caller falls back to `number_to_string`).
    pub(crate) fn small_int_string(&mut self, i: i32) -> Result<Option<JsString>, VmError> {
        if !(0..Self::SMALL_INT_STRING_CACHE).contains(&i) {
            return Ok(None);
        }
        if let Some(cached) = self.small_int_string_cache[i as usize] {
            return Ok(cached.as_string(&self.gc_heap));
        }
        let s = number::ecma::number_to_string(f64::from(i), &mut self.gc_heap)
            .map_err(VmError::from)?;
        self.small_int_string_cache[i as usize] = Some(Value::string(s));
        Ok(Some(s))
    }

    /// `ToString` of a primitive operand for string concatenation, routing small
    /// non-negative integers through the [`Self::small_int_string`] cache to
    /// avoid re-allocating their decimal text on every concatenation.
    pub(crate) fn js_string_for_concat(&mut self, value: Value) -> Result<JsString, VmError> {
        if let Some(n) = value.as_number() {
            let f = n.as_f64();
            if f >= 0.0
                && f < Self::SMALL_INT_STRING_CACHE as f64
                && f.fract() == 0.0
                && let Some(s) = self.small_int_string(f as i32)?
            {
                return Ok(s);
            }
        }
        conversion::to_js_string_primitive(&value, self.gc_heap_mut())
    }

    /// One-allocation concat for `<short flat latin1 string> + <int32>` and its
    /// mirror — the common key-building shape (`"k" + n`). Formats the integer's
    /// ASCII digits straight into a single flat latin1 result, skipping the
    /// throwaway number string, the cons rope, and the flatten the general path
    /// would build. Returns `None` when the operands are not that shape (the
    /// caller takes the general concat path). Only exact int32-tagged operands
    /// qualify, so `ToString` semantics are unchanged. No rooting is needed: the
    /// string's bytes are copied before the result allocation and the integer is
    /// not a heap value.
    pub(crate) fn try_concat_string_int32(
        &mut self,
        lhs: Value,
        rhs: Value,
    ) -> Option<Result<Value, otter_gc::OutOfMemory>> {
        let (handle, n, number_first) = match (
            lhs.as_string(&self.gc_heap),
            rhs.as_i32(),
            lhs.as_i32(),
            rhs.as_string(&self.gc_heap),
        ) {
            (Some(string), Some(n), _, _) => (string.handle(), n, false),
            (_, _, Some(n), Some(string)) => (string.handle(), n, true),
            _ => return None,
        };
        let mut string_bytes = [0u8; 32];
        let string_len = crate::string::gc_body::read_short_flat_latin1(
            &self.gc_heap,
            handle,
            &mut string_bytes,
        )?;
        let mut digits = [0u8; crate::number::integer_fast::I32_BUF_LEN];
        let digit_len = crate::number::integer_fast::format_i32(n, &mut digits);
        let mut out = [0u8; 32 + crate::number::integer_fast::I32_BUF_LEN];
        let (first, second): (&[u8], &[u8]) = if number_first {
            (&digits[..digit_len], &string_bytes[..string_len])
        } else {
            (&string_bytes[..string_len], &digits[..digit_len])
        };
        out[..first.len()].copy_from_slice(first);
        out[first.len()..first.len() + second.len()].copy_from_slice(second);
        let total = first.len() + second.len();
        Some(
            crate::string::JsString::from_latin1(&out[..total], &mut self.gc_heap)
                .map(Value::string),
        )
    }

    /// Root-tracing view of cached BigInt constants.
    pub(crate) fn bigint_constants_for_trace(&self) -> impl Iterator<Item = &Value> {
        self.bigint_constant_cache.values()
    }

    #[cfg(test)]
    fn string_constant_cache_len_for_test(&self) -> usize {
        self.string_constant_cache.len()
    }

    #[cfg(test)]
    fn bigint_constant_cache_len_for_test(&self) -> usize {
        self.bigint_constant_cache.len()
    }

    /// Root-tracing view of prepared lean callback state.
    pub(crate) fn lean_callback_roots_for_trace(
        &self,
    ) -> impl Iterator<Item = &call_ops::LeanCallbackRoot> {
        self.lean_callback_roots.iter()
    }

    /// Trace every live scope-handle slot as a GC root. Called from the runtime
    /// root walk so the collector rewrites parked handles on a move.
    pub(crate) fn handle_arena_trace(&self, visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)) {
        self.handle_arena.trace(visitor);
    }

    pub(crate) fn load_string_constant_value(
        &mut self,
        context: &ExecutionContext,
        idx: u32,
    ) -> Result<Value, VmError> {
        let key = context.constant_cache_key(idx);
        if let Some(value) = self.string_constant_cache.get(&key) {
            return Ok(*value);
        }
        let units = context
            .string_constant_units(idx)
            .ok_or_else(|| VmError::InvalidOperand)?;
        let string = JsString::from_utf16_units(units, self.gc_heap_mut())?;
        let value = Value::string(string);
        self.string_constant_cache.insert(key, value);
        Ok(value)
    }

    /// Push `value` into JSON's strict-stack range in the shared handle arena,
    /// returning its stable index. The collector traces and rewrites the slot on
    /// a move, so the caller re-reads the relocated value after an allocating
    /// sub-call without a second root store.
    pub(crate) fn json_root_push(&mut self, value: Value) -> usize {
        let idx = self.handle_arena.len();
        self.handle_arena.push(value);
        idx
    }

    /// Read the (possibly relocated) value parked at `idx`.
    pub(crate) fn json_root_get(&self, idx: usize) -> Value {
        self.handle_arena.get(idx as u32)
    }

    /// Overwrite the parked value at `idx` (the serializer reassigns
    /// `value` as `toJSON` / the replacer / wrapper unwrapping run).
    pub(crate) fn json_root_set(&mut self, idx: usize, value: Value) {
        self.handle_arena.set(idx as u32, value);
    }

    /// Pop JSON's handle range back down to `idx`, preserving handles owned by
    /// an enclosing native scope.
    pub(crate) fn json_root_pop_to(&mut self, idx: usize) {
        self.handle_arena.truncate(idx);
    }

    /// §13.2.8.4 GetTemplateObject steps 7-15 — build the frozen
    /// template-strings array with its frozen, non-enumerable `.raw`
    /// companion.
    pub(crate) fn build_template_object(
        &mut self,
        context: &ExecutionContext,
        stack: &ActivationStack,
        function_id: u32,
        site_idx: u32,
    ) -> Result<Value, VmError> {
        let site = context
            .template_site_for_function(function_id, site_idx)
            .ok_or_else(|| VmError::InvalidOperand)?
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

/// One call-site record for `util.getCallSites`, serialized to JSON and
/// reconstituted as a plain object on the JS side. Field names match
/// Node's `CallSite` property shape.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CallSiteInfo {
    /// Frame function name (`<anonymous>` / `<main>` for unnamed frames).
    #[serde(rename = "functionName")]
    pub function_name: String,
    /// Module URL / file path the frame was compiled from.
    #[serde(rename = "scriptName")]
    pub script_name: String,
    /// 1-based source line of the frame's current instruction.
    #[serde(rename = "lineNumber")]
    pub line_number: u32,
    /// 1-based source column (UTF-16 units); Node's `columnNumber`.
    #[serde(rename = "columnNumber")]
    pub column_number: u32,
    /// Alias of [`Self::column_number`] for Node's `column` accessor.
    pub column: u32,
    /// Source line text for diagnostics that need expression snippets.
    #[serde(rename = "sourceLine", skip_serializing_if = "Option::is_none")]
    pub source_line: Option<String>,
    /// Previous source line text; useful when the current byte span sits in
    /// a multi-line call argument rather than on the call expression line.
    #[serde(rename = "sourceLineBefore", skip_serializing_if = "Option::is_none")]
    pub source_line_before: Option<String>,
    /// Next source line text for frames whose byte span points at an
    /// enclosing callback/function header.
    #[serde(rename = "sourceLineAfter", skip_serializing_if = "Option::is_none")]
    pub source_line_after: Option<String>,
    /// A small forward source window after the current frame line. Diagnostic
    /// code uses this when bytecode spans point at a callback header but the
    /// relevant expression lives inside the callback body.
    #[serde(rename = "sourceLinesAfter", skip_serializing_if = "Vec::is_empty")]
    pub source_lines_after: Vec<String>,
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

mod interp;
#[allow(unused_imports)]
pub(crate) use interp::helpers::*;
pub use interp::helpers::{GeneratorResumeKind, is_callable_value};

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}
