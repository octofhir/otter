//! Classified machine-callable runtime-stub contracts.
//!
//! # Contents
//! - [`RuntimeStubDescriptor`] declares signature, effects, safepoint,
//!   exception, result ABI, and static work charge for every dense
//!   [`RuntimeStubId`].
//! - [`RuntimeStubAllocContext`] is the rooted allocation packet passed by
//!   every allocating entry.
//! - Typed scalar leaves keep unboxed numeric values in their machine ABI.
//!
//! # Invariants
//! - The inventory is dense and unique; descriptor `id == index + 1`.
//! - Leaf stubs cannot allocate, trigger GC, reenter JS, or name a safepoint.
//! - Allocating and reentrant stubs require a precise safepoint at every call.
//! - Throwing behavior and result-status encoding are explicit descriptor data.
//! - Every descriptor has a non-zero static work charge.
//!
//! # See also
//! - [`crate::runtime_stubs`] for semantic entrypoints.
//! - [`super::safepoints`] for root maps.

use super::{Frame, NO_SAFEPOINT, NativeResultDomain, SafepointId, VmThread};

/// Descriptor argument count for variadic call shapes.
pub const VARIADIC_STUB_ARGUMENTS: u8 = u8::MAX;

/// Runtime-stub semantic class.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStubClass {
    /// Cannot allocate, trigger GC, or call JS.
    LeafNoAlloc = 0,
    /// May allocate and must provide a precise safepoint.
    Alloc = 1,
    /// May call JS/proxies/accessors and requires full reentry state.
    Reentrant = 2,
}

impl RuntimeStubClass {
    /// Whether this class can allocate.
    #[must_use]
    pub const fn can_allocate(self) -> bool {
        matches!(self, Self::Alloc | Self::Reentrant)
    }

    /// Whether this class can reenter JS.
    #[must_use]
    pub const fn can_reenter_js(self) -> bool {
        matches!(self, Self::Reentrant)
    }

    /// Static base charge for crossing this runtime boundary.
    #[must_use]
    pub const fn work_units(self) -> u8 {
        match self {
            Self::LeafNoAlloc => 2,
            Self::Alloc => 4,
            Self::Reentrant => 8,
        }
    }
}

/// Runtime-stub observable effects.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeStubEffects(u16);

impl RuntimeStubEffects {
    /// Stub may allocate managed or externally-accounted memory.
    pub const MAY_ALLOCATE: u16 = 1 << 0;
    /// Stub may trigger moving collection.
    pub const MAY_TRIGGER_GC: u16 = 1 << 1;
    /// Stub may produce a JavaScript exception.
    pub const MAY_THROW: u16 = 1 << 2;
    /// Stub may invoke JavaScript, proxies, accessors, or coercion hooks.
    pub const MAY_REENTER_JS: u16 = 1 << 3;
    /// Stub may mutate GC-managed state and must perform barriers.
    pub const MAY_MUTATE_GC: u16 = 1 << 4;

    /// No observable effects beyond passive reads and a result.
    #[must_use]
    pub const fn none() -> Self {
        Self(0)
    }

    /// Effects for a non-allocating leaf stub that may still throw through a
    /// status and/or run write barriers on GC-managed state.
    #[must_use]
    pub const fn leaf(may_throw: bool, may_mutate_gc: bool) -> Self {
        let mut bits = 0;
        if may_throw {
            bits |= Self::MAY_THROW;
        }
        if may_mutate_gc {
            bits |= Self::MAY_MUTATE_GC;
        }
        Self(bits)
    }

    /// Effects for an allocating, non-reentrant stub.
    #[must_use]
    pub const fn allocating(may_throw: bool, may_mutate_gc: bool) -> Self {
        let mut bits = Self::MAY_ALLOCATE | Self::MAY_TRIGGER_GC;
        if may_throw {
            bits |= Self::MAY_THROW;
        }
        if may_mutate_gc {
            bits |= Self::MAY_MUTATE_GC;
        }
        Self(bits)
    }

    /// Effects for a reentrant stub.
    #[must_use]
    pub const fn reentrant(may_mutate_gc: bool) -> Self {
        let mut bits =
            Self::MAY_ALLOCATE | Self::MAY_TRIGGER_GC | Self::MAY_THROW | Self::MAY_REENTER_JS;
        if may_mutate_gc {
            bits |= Self::MAY_MUTATE_GC;
        }
        Self(bits)
    }

    /// Raw effect bits.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Whether all `mask` bits are present.
    #[must_use]
    pub const fn contains(self, mask: u16) -> bool {
        self.0 & mask == mask
    }
}

/// Machine entry signature family.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStubSignature {
    /// `(heap, value0, value1)` leaf probe.
    LeafValue2 = 0,
    /// `(alloc_ctx, safepoint_id, receiver, arg0, arg1)`.
    AllocValue3 = 1,
    /// One integer argument poll.
    Poll1 = 2,
    /// JIT-owned transition: the JIT entry context plus up to six scalar
    /// operand words whose meaning the installing compiler owns together with
    /// every call site. Precise roots are published through the VM frame the
    /// context names, not through a numeric safepoint id.
    Variadic = 3,
    /// `(jit_ctx, word0, ..) -> NativeResultPair` with one to four fixed
    /// machine-word operands.
    ///
    /// The descriptor's exact [`RuntimeStubDescriptor::argument_count`] is
    /// part of the ABI and every binding is checked against a statically typed
    /// function pointer. Precise roots remain owned by the published native
    /// frame named by the context.
    ContextWords = 4,
    /// `(heap_mut, value0, value1)` leaf mutation.
    ///
    /// Same shape as [`Self::LeafValue2`] with a mutable heap: the entry may
    /// rewrite GC-managed state in place (and must run the matching write
    /// barriers) but still cannot allocate, trigger collection, or re-enter
    /// JS, so the call site publishes no safepoint.
    MutatingLeafValue2 = 5,
    /// `(heap_mut, value0, value1, value2)` leaf mutation.
    ///
    /// [`Self::MutatingLeafValue2`] with one more operand word, for an
    /// in-place write whose receiver and two arguments do not fit two words.
    /// The same rules apply: rewrite in place, run the matching write
    /// barriers, never allocate, collect, or re-enter JS.
    MutatingLeafValue3 = 6,
    /// `(left: f64, right: f64) -> f64` pure numeric leaf.
    Float64Leaf2 = 7,
    /// `(value: f64) -> word` pure numeric conversion leaf.
    Float64ToWordLeaf1 = 8,
    /// `(jit_ctx, receiver, key) -> NativeResultPair` reentrant value call.
    ///
    /// Unlike [`Self::Variadic`], both operands are boxed JavaScript values,
    /// never register indices into an interpreter-compatible window. The call
    /// site publishes precise roots independently of this fixed value ABI.
    ReentrantValue2 = 9,
    /// `(jit_ctx, receiver, key, value) -> NativeResultPair` reentrant
    /// value call.
    ///
    /// This is the three-value form of [`Self::ReentrantValue2`].
    ReentrantValue3 = 10,
    /// `(jit_ctx, receiver, property_ic_slot) -> NativeResultPair`
    /// reentrant named-property read.
    ///
    /// The receiver is a boxed JavaScript value. `property_ic_slot` is the
    /// site's CodeBlock-owned feedback slot rather than a GC value; its bound
    /// function and logical-PC identity let the VM decode the property name.
    ReentrantNamedLoad = 11,
    /// `(jit_ctx, receiver, value, property_ic_slot) -> NativeResultPair`
    /// reentrant named-property write.
    ///
    /// This is the two-value store counterpart of
    /// [`Self::ReentrantNamedLoad`].
    ReentrantNamedStore = 12,
    /// `(jit_ctx, values, count) -> NativeResultPair` reentrant boxed-value
    /// span call.
    ///
    /// `values` points at `count` contiguous boxed JavaScript values. The
    /// generated caller publishes every value as a precise root and the entry
    /// copies the complete span before allocation or JavaScript reentry.
    ReentrantValueSpan = 13,
    /// `(jit_ctx, value0, value1) -> NativeResultPair` reentrant
    /// semantic completion.
    ///
    /// Both operands are boxed JavaScript values. The published function/PC
    /// selects a typed semantic operation; no opcode, destination, register
    /// index, or materialized-frame identity crosses this ABI. Once entered,
    /// only normal completion or a JavaScript throw may be returned.
    CommittedValue2 = 14,
    /// `(jit_ctx, exception) -> NativeResultPair` propagation router for a pure
    /// exception value returned by [`Self::CommittedValue2`].
    ///
    /// A generated local landing never calls this entry. Propagating code uses
    /// it to deliver a materialized handler or publish the value as uncaught;
    /// the router may run observable IteratorClose during unwind.
    RouteThrow1 = 15,
    /// `(jit_ctx) -> NativeResultPair` entry of the common call trampoline.
    ///
    /// The caller has written a classify request into the context. The result
    /// is in the execution domain: success, a pure JavaScript throw, or a fatal
    /// failure with its error parked in the context.
    ExecutionEntry0 = 16,
    /// The JavaScript call ABI: `(jit_ctx, callee, receiver, new_target,
    /// argc)` with the actual span at the caller's stack pointer.
    ///
    /// The result is in the execution domain: success, a pure JavaScript
    /// throw, or a fatal failure with its error parked in the context.
    JsCall = 17,
}

/// Safepoint requirement encoded in the descriptor.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStubSafepoint {
    /// Call site must not publish a safepoint.
    Forbidden = 0,
    /// Call site must publish a concrete safepoint id.
    Required = 1,
}

/// JavaScript exception behavior.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStubException {
    /// Stub cannot produce a JavaScript exception.
    Never = 0,
    /// Throw is reported through an explicit result status.
    Status = 1,
}

/// Machine result representation.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStubResultAbi {
    /// [`super::NativeResultPair`] two-register encoding. The descriptor's
    /// [`RuntimeStubDescriptor::result_domain`] owns its semantic state
    /// machine.
    NativePair = 0,
    /// One whole-word [`super::NativeResultStatus`]; any value result is
    /// written through the call packet or published frame before return.
    StatusWord = 1,
    /// Single raw value word with no status channel.
    ValueWord = 2,
    /// One unboxed IEEE-754 binary64 value in the platform FP result register.
    Float64 = 3,
}

/// Machine-callable runtime-stub descriptor.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeStubDescriptor {
    /// Dense descriptor id in the current runtime contract.
    pub id: super::RuntimeStubId,
    /// Semantic class.
    pub class: RuntimeStubClass,
    /// Machine signature family.
    pub signature: RuntimeStubSignature,
    /// Fixed value argument count, or [`VARIADIC_STUB_ARGUMENTS`].
    pub argument_count: u8,
    /// Static base charge debited on each transition.
    pub work_units: u8,
    /// Safepoint requirement.
    pub safepoint: RuntimeStubSafepoint,
    /// Exception behavior.
    pub exception: RuntimeStubException,
    /// Result encoding.
    pub result_abi: RuntimeStubResultAbi,
    /// Exact semantic domain when `result_abi` is [`RuntimeStubResultAbi::NativePair`],
    /// or [`NativeResultDomain::None`] for every other physical result ABI.
    pub result_domain: NativeResultDomain,
    /// Declared observable effects.
    pub effects: RuntimeStubEffects,
}

const fn descriptor(
    id: super::RuntimeStubId,
    class: RuntimeStubClass,
    signature: RuntimeStubSignature,
    argument_count: u8,
    effects: RuntimeStubEffects,
    exception: RuntimeStubException,
    result_abi: RuntimeStubResultAbi,
    result_domain: NativeResultDomain,
) -> RuntimeStubDescriptor {
    RuntimeStubDescriptor {
        id,
        class,
        signature,
        argument_count,
        work_units: class.work_units(),
        safepoint: if class.can_allocate() {
            RuntimeStubSafepoint::Required
        } else {
            RuntimeStubSafepoint::Forbidden
        },
        exception,
        result_abi,
        result_domain,
        effects,
    }
}

/// VM-native allocation/rooting packet used by allocating entries.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeStubAllocContext {
    /// Active VM thread record.
    pub thread: *mut VmThread,
    /// Base of tagged native spill slots.
    pub spill_slots: *mut u64,
    /// Dense safepoint id within the code object.
    pub safepoint_id: SafepointId,
    /// Number of native spill slots.
    pub spill_slot_count: u16,
}

impl RuntimeStubAllocContext {
    /// Build an allocating call packet.
    #[must_use]
    pub const fn new(thread: *mut VmThread, safepoint_id: SafepointId) -> Self {
        Self {
            thread,
            spill_slots: std::ptr::null_mut(),
            safepoint_id,
            spill_slot_count: 0,
        }
    }

    /// Attach a tagged native spill window.
    #[must_use]
    pub const fn with_spill_area(mut self, spill_slots: *mut u64, count: u16) -> Self {
        self.spill_slots = spill_slots;
        self.spill_slot_count = count;
        self
    }

    /// Whether a frame-slot window is present.
    #[must_use]
    pub fn has_frame_slots(self) -> bool {
        let frame = self.current_frame();
        if frame.is_null() {
            return false;
        }
        // SAFETY: callers uphold the live published-frame contract.
        let frame = unsafe { &*frame };
        frame.register_base() != 0 && frame.header.register_count != 0
    }

    /// Innermost published frame of the compiled entry, or null outside
    /// compiled execution.
    #[must_use]
    pub const fn current_frame(self) -> *mut Frame {
        if self.thread.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: callers uphold the live VM-thread contract; a nonzero frame
        // cell is the live entry's innermost-frame cell.
        unsafe {
            let cell = (*self.thread).frame_cell as *const u64;
            if cell.is_null() {
                return std::ptr::null_mut();
            }
            *cell as *mut Frame
        }
    }

    /// Installed code generation executing the current frame.
    #[must_use]
    pub const fn code_object_id(self) -> u64 {
        let frame = self.current_frame();
        if frame.is_null() {
            return 0;
        }
        // SAFETY: the innermost published frame is live.
        unsafe { (*frame).code_object_id as u64 }
    }

    /// Whether a spill-slot window is present.
    #[must_use]
    pub const fn has_spill_slots(self) -> bool {
        !self.spill_slots.is_null() && self.spill_slot_count != 0
    }

    /// Whether code-object/safepoint identity is publishable.
    #[must_use]
    pub const fn has_safepoint_records(self) -> bool {
        self.code_object_id() != 0 && self.safepoint_id != NO_SAFEPOINT
    }
}

/// Leaf compiled-loop backedge poll; reports interrupt/budget stops through
/// its status word.
pub const STUB_JIT_BACKEDGE_POLL: RuntimeStubDescriptor = descriptor(
    1,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Poll1,
    1,
    RuntimeStubEffects::leaf(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);
/// Leaf `Map.prototype.get` probe.
pub const STUB_COLLECTION_MAP_GET_LEAF: RuntimeStubDescriptor = descriptor(
    2,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);
/// Leaf `Map.prototype.has` probe.
pub const STUB_COLLECTION_MAP_HAS_LEAF: RuntimeStubDescriptor = descriptor(
    3,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);
/// Leaf `Set.prototype.has` probe.
pub const STUB_COLLECTION_SET_HAS_LEAF: RuntimeStubDescriptor = descriptor(
    4,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);
/// Allocating `Map.prototype.set` mutation.
pub const STUB_COLLECTION_MAP_SET_ALLOC: RuntimeStubDescriptor = descriptor(
    5,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);
/// Allocating `Set.prototype.add` mutation.
pub const STUB_COLLECTION_SET_ADD_ALLOC: RuntimeStubDescriptor = descriptor(
    6,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);
/// Allocating `Map.prototype.get` lookup.
pub const STUB_COLLECTION_MAP_GET_ALLOC: RuntimeStubDescriptor = descriptor(
    7,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);
/// Allocating `Map.prototype.has` lookup.
pub const STUB_COLLECTION_MAP_HAS_ALLOC: RuntimeStubDescriptor = descriptor(
    8,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);
/// Allocating `Set.prototype.has` lookup.
pub const STUB_COLLECTION_SET_HAS_ALLOC: RuntimeStubDescriptor = descriptor(
    9,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);
/// Allocating `Map.prototype.delete` mutation.
pub const STUB_COLLECTION_MAP_DELETE_ALLOC: RuntimeStubDescriptor = descriptor(
    10,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);
/// Allocating `Set.prototype.delete` mutation.
pub const STUB_COLLECTION_SET_DELETE_ALLOC: RuntimeStubDescriptor = descriptor(
    11,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);
/// Allocating primitive string-concat operation.
pub const STUB_STRING_CONCAT_ALLOC: RuntimeStubDescriptor = descriptor(
    12,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Generic `+` slow path; operand coercion may re-enter JS.
pub const STUB_JIT_ADD: RuntimeStubDescriptor = descriptor(
    13,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);

/// Computed element read over boxed value operands.
///
/// `ToPropertyKey`, proxies, accessors, and prototype lookup may all re-enter
/// JavaScript. The fixed entry completes the operation or returns a pure
/// exception value; it never asks generated code to replay the access.
pub const STUB_JIT_LOAD_ELEMENT: RuntimeStubDescriptor = descriptor(
    14,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::ReentrantValue2,
    2,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);
/// Computed element write over boxed value operands.
///
/// The store may mutate arbitrary GC-managed state and invoke proxy traps,
/// setters, or property-key coercion hooks. Once entered it either completes
/// exactly once or returns a pure exception value.
pub const STUB_JIT_STORE_ELEMENT: RuntimeStubDescriptor = descriptor(
    15,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::ReentrantValue3,
    3,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);
/// Descriptor-driven define; descriptor reads may re-enter JS.
pub const STUB_JIT_DEFINE_OWN_PROPERTY: RuntimeStubDescriptor = descriptor(
    16,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);
/// Named-property read over one boxed receiver and the site's native IC slot
/// (V8 `LoadIC_Miss`).
///
/// Accessors, proxies, and exotic receivers may re-enter JavaScript. The site
/// identity bound in the slot selects `LoadProperty`; success returns its value
/// and may install a handler in the slot, while failure returns one pure
/// exception value without replay.
pub const STUB_JIT_LOAD_PROPERTY: RuntimeStubDescriptor = descriptor(
    17,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::ReentrantNamedLoad,
    1,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);
/// Named-property write over boxed receiver/value operands and the site's
/// native IC slot (V8 `StoreIC_Miss`).
///
/// Shape transitions may allocate and setters or proxy traps may re-enter
/// JavaScript. Entry commits the complete store exactly once or returns one
/// pure exception value; it never asks generated code to replay the operation.
pub const STUB_JIT_STORE_PROPERTY: RuntimeStubDescriptor = descriptor(
    18,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::ReentrantNamedStore,
    2,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);
/// Plain data-property define.
pub const STUB_JIT_DEFINE_DATA_PROPERTY: RuntimeStubDescriptor = descriptor(
    19,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);
/// Builtin error-constructor load.
pub const STUB_JIT_LOAD_BUILTIN_ERROR: RuntimeStubDescriptor = descriptor(
    20,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);
/// Capture-free function allocation. The published semantic source owns the
/// function constant; the three boxed operands are `undefined`. The entry
/// returns a fresh fully initialized closure or a pre-effect Probe refusal.
pub const STUB_JIT_MAKE_FN: RuntimeStubDescriptor = descriptor(
    21,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(false, false),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);
/// Closure allocation from `(context, lexical this, lexical new.target)`.
/// The published innermost source owns the function constant. No operand is
/// an interpreter register index and no physical-frame binding is consulted.
pub const STUB_JIT_MAKE_CLOSURE: RuntimeStubDescriptor = descriptor(
    22,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(false, false),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);
/// Ordinary object allocation from an empty boxed-value span. The native
/// frame publishes roots; success returns the object without a destination.
pub const STUB_JIT_NEW_OBJECT: RuntimeStubDescriptor = descriptor(
    23,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::ReentrantValueSpan,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);
/// Array literal allocation from boxed values, copied before collection.
/// The span preserves holes and never denotes interpreter register indices.
pub const STUB_JIT_NEW_ARRAY: RuntimeStubDescriptor = descriptor(
    24,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::ReentrantValueSpan,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);
/// Static-key object literal allocation (`Op::NewObjectLiteral`) from boxed
/// values copied before collection; the published instruction names the keys.
pub const STUB_JIT_NEW_OBJECT_LITERAL: RuntimeStubDescriptor = descriptor(
    83,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::ReentrantValueSpan,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);
/// `CreateContext` allocation: `(parent, function id, scope index)` as boxed
/// words (the ids as int32 values). Fully initializes the context — scope
/// identity, parent, and every slot as the hole or `undefined` per its scope
/// descriptor — before returning it; the parent rides as a rooted stub
/// argument. Heap refusal is reported through the status pair; the entry
/// never raises a JavaScript exception.
pub const STUB_CREATE_CONTEXT_ALLOC: RuntimeStubDescriptor = descriptor(
    25,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(false, false),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Generational and insertion write barrier for one pointer store.
///
/// Generated code runs both halves inline and reaches this only when a marking
/// cycle is in progress or the store really creates an unrecorded old->young
/// edge. It therefore takes the parent's header address and the stored value
/// directly: no register window, no published frame, no reentry.
pub const STUB_WRITE_BARRIER: RuntimeStubDescriptor = descriptor(
    26,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::MutatingLeafValue2,
    2,
    RuntimeStubEffects::leaf(false, true),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);
/// `CopyContext` allocation (§14.7.4.4 CreatePerIterationEnvironment):
/// `(source, _, _)` as boxed words. Returns a context with the source's scope
/// identity, parent, and slot values; the source rides as a rooted stub
/// argument and is re-read after the allocation. Heap refusal is reported
/// through the status pair.
pub const STUB_COPY_CONTEXT_ALLOC: RuntimeStubDescriptor = descriptor(
    27,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(false, false),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);
/// Leaf §7.2.15 IsStrictlyEqual probe over two raw operand words: never
/// throws, never allocates; a null heap reports a miss so probe harnesses
/// without a live isolate fall back to normal dispatch.
pub const STUB_STRICT_EQ_LEAF: RuntimeStubDescriptor = descriptor(
    28,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);
/// Completes one full loose-equality opcode in the VM; object-to-primitive
/// coercion may re-enter JS.
pub const STUB_JIT_LOOSE_EQ: RuntimeStubDescriptor = descriptor(
    29,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);
/// Materializes a regex literal from the constant pool; allocates the
/// RegExp body and may compile the pattern.
pub const STUB_JIT_LOAD_REGEXP: RuntimeStubDescriptor = descriptor(
    32,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);
/// Leaf §7.1.2 ToBoolean probe over one raw operand word (the second
/// argument is ignored): never throws, never allocates; total for every
/// value including heap cells, so it never misses on a live isolate.
pub const STUB_TO_BOOLEAN_LEAF: RuntimeStubDescriptor = descriptor(
    30,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);
/// Leaf `typeof x === kind` test over one raw operand word and an
/// `Op::TestTypeOf` immediate: never throws, never allocates; total for
/// every value, so it never misses on a live isolate.
pub const STUB_TYPEOF_TEST_LEAF: RuntimeStubDescriptor = descriptor(
    84,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);
/// Leaf numeric remainder over two raw operand words already known to be
/// numbers: full f64 remainder semantics (sign of the dividend, NaN for a
/// zero divisor), boxed without allocation.
pub const STUB_NUMBER_REM_LEAF: RuntimeStubDescriptor = descriptor(
    31,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Leaf `String.prototype.charCodeAt` over a string receiver and an integral
/// index: walks the body to one code unit, boxed without allocation. Misses
/// (non-string receiver, non-integral or out-of-range index) report
/// `SideExit` in the shared native pair so the caller falls back to the
/// general method path.
pub const STUB_STRING_CHAR_CODE_AT_LEAF: RuntimeStubDescriptor = descriptor(
    54,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Leaf `String.prototype.codePointAt` over a string receiver and an integral
/// index. Misses on a non-string receiver, a non-integral or out-of-range
/// index, so the general path owns coercion and the `undefined` result.
pub const STUB_STRING_CODE_POINT_AT_LEAF: RuntimeStubDescriptor = descriptor(
    55,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Leaf `String.prototype.indexOf` over two string operands, searching from
/// index zero. Misses when either operand is not a string.
pub const STUB_STRING_INDEX_OF_LEAF: RuntimeStubDescriptor = descriptor(
    56,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Leaf `String.prototype.includes` over two string operands, searching from
/// index zero. Misses when either operand is not a string.
pub const STUB_STRING_INCLUDES_LEAF: RuntimeStubDescriptor = descriptor(
    57,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Leaf `String.prototype.startsWith` over two string operands, anchored at
/// index zero. Misses when either operand is not a string.
pub const STUB_STRING_STARTS_WITH_LEAF: RuntimeStubDescriptor = descriptor(
    58,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Leaf `String.prototype.endsWith` over two string operands, anchored at the
/// receiver's end. Misses when either operand is not a string.
pub const STUB_STRING_ENDS_WITH_LEAF: RuntimeStubDescriptor = descriptor(
    59,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Completes one coercive `ToPrimitive` or `ToNumeric` opcode; user conversion
/// hooks may allocate, throw, and re-enter arbitrary JS.
pub const STUB_JIT_COERCE_UNARY: RuntimeStubDescriptor = descriptor(
    33,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);

/// Completes one numeric, bitwise, update, or relational opcode in the VM.
/// The shared family is conservatively reentrant because `Increment` may run
/// user conversion hooks and BigInt results may allocate.
pub const STUB_JIT_NUMERIC_OP: RuntimeStubDescriptor = descriptor(
    34,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);

/// Completes structured exception-region state changes, abrupt unwinds, and
/// TDZ `ReferenceError` materialization.
pub const STUB_JIT_EXCEPTION_OP: RuntimeStubDescriptor = descriptor(
    35,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::ExceptionTransition,
);

/// Completes iterator stepping, iterator close, and closer-registry state
/// through the VM's full iterator semantics.
pub const STUB_JIT_ITERATOR_OP: RuntimeStubDescriptor = descriptor(
    36,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);

/// Completes `Function.prototype.bind` — accessor `name`/`length` getters and
/// bound-function allocation — through the VM's full bind semantics.
pub const STUB_JIT_BIND_FUNCTION: RuntimeStubDescriptor = descriptor(
    37,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);

/// Complete the exact published object property-protocol operation from two
/// boxed values. Function/PC identity selects `instanceof`, `in`,
/// `[[GetPrototypeOf]]`, or `[[SetPrototypeOf]]`; Proxy traps and
/// `@@hasInstance` commit exactly once.
pub const STUB_JIT_OBJECT_PROTOCOL_VALUE: RuntimeStubDescriptor = descriptor(
    38,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::CommittedValue2,
    2,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);

/// Completes `delete` (`DeleteProperty`, `DeleteElement`, `DeleteDynamic`) —
/// including the Proxy `deleteProperty` trap and unqualified delete — through
/// the VM's delete drivers and fast paths.
pub const STUB_JIT_DELETE_OP: RuntimeStubDescriptor = descriptor(
    39,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);

/// Complete the exact published scalar value operation from two boxed values.
/// Function/PC identity selects the typed coercion/query or derived-`this`
/// bind; a result register is committed by generated code only after the
/// returned `Ok(value)`.
pub const STUB_JIT_SCALAR_VALUE: RuntimeStubDescriptor = descriptor(
    40,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::CommittedValue2,
    2,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);

/// Completes `super` property reads and writes (`LoadSuperProperty`,
/// `LoadSuperElement`, `SetSuperProperty`, `SetSuperElement`) — including home-
/// prototype accessor getters/setters — through the VM's super helpers.
pub const STUB_JIT_SUPER_OP: RuntimeStubDescriptor = descriptor(
    41,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);

/// Completes private-member access (`PrivateGet`, `PrivateSet`,
/// `PrivateBrandCheck`) — including private accessor getters/setters — through
/// the VM's private-element helpers.
pub const STUB_JIT_PRIVATE_OP: RuntimeStubDescriptor = descriptor(
    42,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);

/// Completes static value loads (`MathLoad`, `SymbolLoad`, `TemporalLoad`,
/// `LoadBigInt`, `GetStringIndex`) through the VM's load helpers.
pub const STUB_JIT_VALUE_LOAD_OP: RuntimeStubDescriptor = descriptor(
    43,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);

/// Completes allocating construction opcodes (`CollectRest`, `ArrayPush`) through the VM's construction helpers.
pub const STUB_JIT_CONSTRUCT_OP: RuntimeStubDescriptor = descriptor(
    44,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);

/// Completes structural object opcodes (`ForInKeys`, `CopyDataProperties`)
/// through the VM's structural helpers.
pub const STUB_JIT_STRUCTURAL_OP: RuntimeStubDescriptor = descriptor(
    45,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);

/// Completes class-construction opcodes (`ClassCheck`, `SetFunctionName`)
/// through the VM's class helpers.
pub const STUB_JIT_CLASS_OP: RuntimeStubDescriptor = descriptor(
    46,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);

/// Completes variadic construction opcodes (`ArrayConstruct`, `ArrayFrom`,
/// `ArrayOf`, `QueueMicrotask`) through the VM's variadic helpers.
pub const STUB_JIT_VARIADIC_OP: RuntimeStubDescriptor = descriptor(
    47,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);

/// Completes static intrinsic-call opcodes (`ArrayBufferCall`,
/// `SharedArrayBufferCall`, `BigIntCall`, `DataViewCall`) through the VM's
/// static-call helpers, rebuilding their method-id operand layout.
pub const STUB_JIT_STATIC_CALL_OP: RuntimeStubDescriptor = descriptor(
    48,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);

/// Completes class creation, dynamic source evaluation, private-name/template
/// materialization, eval identity, and full `ToNumber` coercion through shared
/// VM helpers.
pub const STUB_JIT_CLASS_VALUE_OP: RuntimeStubDescriptor = descriptor(
    49,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);

/// Completes synchronous static-module namespace/binding operations,
/// star re-export, module-record marking, and `import.meta.resolve` through
/// shared VM helpers. Promise-producing module operations remain side exits.
pub const STUB_JIT_MODULE_OP: RuntimeStubDescriptor = descriptor(
    50,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);

/// Normalize one parked runtime error at the compiled frame's canonical final
/// abrupt-completion boundary. Catchable failures become pure exception values; a
/// materialized local handler produces an exact-PC Bail; structural failures
/// remain parked and produce Fatal. This boundary is never called between two
/// compiled frames.
pub const STUB_JIT_FINISH_ERROR: RuntimeStubDescriptor = descriptor(
    51,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Compiled,
);

/// Rebuild every interpreter frame a deopt exit owes, from deopt metadata.
///
/// A generated exit site selects a code-owned recipe and materializes its
/// canonical homes before calling this with the baked
/// [`crate::deopt::DeoptRuntime`], the frame's stack pointer and its register
/// window. Physical recipes read homes or constants; the entry accepts context,
/// exit index, recipe address, canonical slot base and destination window.
/// The stub reconstitutes every slot of every owed frame from that recipe.
///
/// An exit owing only the compiled function's own frame writes it into the
/// published window and reports a bail. An exit owing a chain of spliced
/// frames restores the physical outer frame and prepares owned descendant
/// inputs, then switches the physical frame to interpreter ownership, dropping
/// its machine-root publication. Its generated entry resumes that chain only
/// after Rust preparation returns and reports the outermost completion.
pub const STUB_JIT_DEOPT_WRITEBACK: RuntimeStubDescriptor = descriptor(
    52,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Compiled,
);

/// Generated deopt continuation over the context's published callee.
///
/// One typed side-exit word selects the exact resume PC. Rust preparation
/// returns before assembly resumes JavaScript on the same Frame. Sources,
/// generations and constructor mode come from the published chain and recipes.
pub const STUB_JIT_DEOPT_CALL: RuntimeStubDescriptor = descriptor(
    53,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::ContextWords,
    1,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);

/// Leaf dense-array `Array.prototype.pop` mutation.
///
/// Truncating the dense buffer drops a reference and rewrites the cached
/// length pair; neither allocates. The entry re-checks the dense
/// preconditions the inline guard cannot see (writable `length`, a present
/// own last element, no accessor override in range) and reports a miss when
/// they fail, so the call site falls through to ordinary dispatch.
pub const STUB_ARRAY_POP_LEAF: RuntimeStubDescriptor = descriptor(
    60,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::MutatingLeafValue2,
    2,
    RuntimeStubEffects::leaf(false, true),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);
/// Leaf dense-array `Array.prototype.push` mutation.
///
/// Appends into the dense buffer's existing capacity and never grows it, so
/// it neither allocates nor collects. `push` creates a new index, which the
/// spec resolves through the prototype chain: the caller must prove the
/// array-index accessor protector intact before the call (see
/// [`crate::runtime_stubs::LeafEntryShape::array_index_protector`]). The entry
/// re-checks the dense preconditions and misses on a full buffer.
pub const STUB_ARRAY_PUSH_LEAF: RuntimeStubDescriptor = descriptor(
    61,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::MutatingLeafValue2,
    2,
    RuntimeStubEffects::leaf(false, true),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Leaf dense-array `Array.prototype.shift` mutation.
///
/// Sliding the element buffer down drops one reference and rewrites the cached
/// length pair without allocating. Like the `pop` entry it re-checks the dense
/// preconditions and misses instead of falling back internally.
pub const STUB_ARRAY_SHIFT_LEAF: RuntimeStubDescriptor = descriptor(
    62,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::MutatingLeafValue2,
    2,
    RuntimeStubEffects::leaf(false, true),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);
/// Allocating dense-array `Array.prototype.unshift` mutation.
///
/// Inserting at the head may grow the dense buffer, so the site publishes a
/// precise safepoint exactly like the `push` entry.
pub const STUB_ARRAY_UNSHIFT_ALLOC: RuntimeStubDescriptor = descriptor(
    63,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(false, true),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Leaf in-place `Map.prototype.set` over a key the map already holds.
///
/// Overwriting an existing entry rewrites one slot and runs its write
/// barrier; nothing allocates, so the site owes no safepoint and no rooting
/// packet. A key the map does not hold appends and may grow the table, so the
/// entry misses and the allocating sibling completes the call.
pub const STUB_COLLECTION_MAP_SET_MUTATING: RuntimeStubDescriptor = descriptor(
    69,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::MutatingLeafValue3,
    3,
    RuntimeStubEffects::leaf(false, true),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Pure unboxed Number remainder for typed numeric machine code.
pub const STUB_NUMBER_REM_F64_LEAF: RuntimeStubDescriptor = descriptor(
    70,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Float64Leaf2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::Float64,
    NativeResultDomain::None,
);

/// Pure unboxed ECMAScript Number exponentiation for typed numeric machine code.
pub const STUB_NUMBER_POW_F64_LEAF: RuntimeStubDescriptor = descriptor(
    71,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Float64Leaf2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::Float64,
    NativeResultDomain::None,
);

/// Pure ECMAScript ToInt32 conversion for typed numeric machine code.
pub const STUB_NUMBER_TO_INT32_F64_LEAF: RuntimeStubDescriptor = descriptor(
    72,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Float64ToWordLeaf1,
    1,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::ValueWord,
    NativeResultDomain::None,
);

/// Bind the published record's receiver before its body runs: convert a
/// sloppy receiver, or create a base constructor's receiver
/// (`OrdinaryCreateFromConstructor` over the record's `new.target`).
pub const STUB_JIT_PREPARE_ACTIVATION: RuntimeStubDescriptor = descriptor(
    73,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::ExecutionEntry0,
    0,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Execution,
);

/// Derived-constructor completion of a non-object `result` against the
/// published record's `this` binding.
pub const STUB_JIT_DERIVED_CONSTRUCT_RESULT: RuntimeStubDescriptor = descriptor(
    74,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::ContextWords,
    1,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);

/// Read the live superclass constructor from an exact class wrapper.
pub const STUB_JIT_CLASS_SUPER_CONSTRUCTOR: RuntimeStubDescriptor = descriptor(
    75,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Variadic,
    1,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::ValueWord,
    NativeResultDomain::None,
);

/// Ask the cold optimizing policy to promote the function of the published
/// record, whose canonical source-work target was reached before this entry.
/// Compilation runs no JavaScript; the record keeps its generation.
pub const STUB_JIT_PROMOTE_ENTERED: RuntimeStubDescriptor = descriptor(
    76,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::ExecutionEntry0,
    0,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Execution,
);

/// Leaf `Math.abs`.
///
/// A numeric builtin reached through a declared entry rather than a
/// per-builtin machine-code body. Adding a sibling is a declaration plus its
/// entry; it costs no generated code.
pub const STUB_MATH_ABS_LEAF: RuntimeStubDescriptor = descriptor(
    64,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Leaf `Math.floor`.
pub const STUB_MATH_FLOOR_LEAF: RuntimeStubDescriptor = descriptor(
    65,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Leaf `Math.sqrt`.
pub const STUB_MATH_SQRT_LEAF: RuntimeStubDescriptor = descriptor(
    66,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Leaf `Math.max` over exactly two arguments.
///
/// The variadic and zero/one-argument forms keep the ordinary call path; the
/// declared arity is what makes a site eligible for this entry.
pub const STUB_MATH_MAX_LEAF: RuntimeStubDescriptor = descriptor(
    67,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Leaf `Math.min` over exactly two arguments.
pub const STUB_MATH_MIN_LEAF: RuntimeStubDescriptor = descriptor(
    68,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Leaf `parseInt(value)` for one exact int32-tagged argument.
///
/// Other argument representations and all non-one-argument JavaScript call
/// shapes retain the canonical coercing parser before any observable effect.
pub const STUB_PARSE_INT_I32_LEAF: RuntimeStubDescriptor = descriptor(
    77,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Allocating `Array(length)` specialization for one exact nonnegative int32.
///
/// Invalid tags and negative lengths miss before allocation so the generated
/// caller can resume the canonical constructor at its exact pre-operation
/// frame. Heap refusal is reported separately through the status pair; this
/// entry never constructs or parks a JavaScript exception.
pub const STUB_ARRAY_CONSTRUCT_ALLOC: RuntimeStubDescriptor = descriptor(
    78,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(false, false),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Build the activation's `arguments` object into one destination register.
/// A stack-owned frame supplies the actual arguments its generated caller
/// published after the register window; a materialized activation supplies
/// them from its cold record. Allocating, never reentrant.
pub const STUB_JIT_COLLECT_ARGUMENTS: RuntimeStubDescriptor = descriptor(
    82,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);

/// Complete the exact published schema-owned binding operation from two boxed
/// values. Function/PC identity selects the semantic family and operand roles
/// through `otter_bytecode::opcode_schema::BindingSemantics`; a result
/// register is committed by generated code only after the returned `Ok`.
pub const STUB_JIT_BINDING_VALUE: RuntimeStubDescriptor = descriptor(
    80,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::CommittedValue2,
    2,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);

/// Complete the exact published schema-owned global declaration or
/// initialization from two boxed values. Declarations remain a separate
/// semantic family even though they share the committed physical ABI.
pub const STUB_JIT_GLOBAL_DECLARATION_VALUE: RuntimeStubDescriptor = descriptor(
    81,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::CommittedValue2,
    2,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);

/// Route one pure exception value into the current activation.
///
/// A handled materialized throw publishes the catch/finally PC and reports a
/// same-frame side exit. An unhandled or stack-owned throw returns the same
/// exception payload with `Throw`; it is never staged between compiled frames.
pub const STUB_JIT_ROUTE_THROW: RuntimeStubDescriptor = descriptor(
    79,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::RouteThrow1,
    1,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Compiled,
);

/// Enter the common call trampoline for the request the generated caller
/// wrote into its context: callee, receiver, `new.target`, flags and the actual
/// span. The trampoline classifies the callee, owns the callee activation and
/// returns its completion.
pub const STUB_JIT_CALL: RuntimeStubDescriptor = descriptor(
    85,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::ExecutionEntry0,
    0,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Execution,
);

/// Stage a compiler-created dense spread array's elements as the actual span
/// of the context's pending call request. Reads only; the span stays valid
/// until the trampoline copies it.
pub const STUB_JIT_STAGE_SPREAD: RuntimeStubDescriptor = descriptor(
    86,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::ContextWords,
    1,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);

/// Stage the complete pending call request of a forwarding site from one
/// boxed-value span: the resolved `apply` value, the callee, the receiver and
/// the current argument-binding values. The intrinsic `apply` forwards the
/// activation's actual arguments; any other method receives the arguments
/// object. No JavaScript runs except materialization accessors.
pub const STUB_JIT_STAGE_FORWARD: RuntimeStubDescriptor = descriptor(
    87,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::ReentrantValueSpan,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);

/// Resolve the callable of the exact published `CallMethodValue` from its
/// receiver span, through the shared method caches, and record call feedback.
/// The generated caller then enters the call trampoline with that callable.
pub const STUB_JIT_RESOLVE_METHOD: RuntimeStubDescriptor = descriptor(
    88,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::ReentrantValueSpan,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);

/// Generic entry of the JavaScript call ABI: classify any callee through the
/// call trampoline and complete the call. Also the entry of every
/// interpreter destination.
pub const STUB_JIT_CALL_GENERIC: RuntimeStubDescriptor = descriptor(
    89,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::JsCall,
    4,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Execution,
);

/// Park the stack-overflow `RangeError` for a call entry that found no room
/// for its frame, before publishing anything.
pub const STUB_JIT_CALL_OVERFLOW: RuntimeStubDescriptor = descriptor(
    90,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::ExecutionEntry0,
    0,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Execution,
);

/// Stage a §15.10.3 tail call from one boxed-value span — the callee, then
/// its actual arguments — as the context's pending request. The staging
/// activation then retires its record and returns `Continue`; its caller
/// enters the request in its place. Reads only.
pub const STUB_JIT_STAGE_TAIL_CALL: RuntimeStubDescriptor = descriptor(
    91,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::ReentrantValueSpan,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);

/// Barrier for one tagged element store generated code wrote into an
/// ordinary array's slab: `(element base, index, value)`. Reached only for a
/// heap-cell value while marking or when the value is young; marks the slot
/// dirty and remembers the slab.
pub const STUB_ELEMENT_WRITE_BARRIER: RuntimeStubDescriptor = descriptor(
    92,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::MutatingLeafValue3,
    3,
    RuntimeStubEffects::leaf(false, true),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Human-readable symbol for a runtime-stub id in the current contract.
#[must_use]
pub const fn runtime_stub_name(id: super::RuntimeStubId) -> &'static str {
    match id {
        1 => "jit_backedge_poll",
        2 => "collection_map_get_leaf",
        3 => "collection_map_has_leaf",
        4 => "collection_set_has_leaf",
        5 => "collection_map_set_alloc",
        6 => "collection_set_add_alloc",
        7 => "collection_map_get_alloc",
        8 => "collection_map_has_alloc",
        9 => "collection_set_has_alloc",
        10 => "collection_map_delete_alloc",
        11 => "collection_set_delete_alloc",
        12 => "string_concat_alloc",
        13 => "jit_add",
        14 => "jit_load_element",
        15 => "jit_store_element",
        16 => "jit_define_own_property",
        17 => "jit_load_property_value",
        18 => "jit_store_property_value",
        19 => "jit_define_data_property",
        20 => "jit_load_builtin_error",
        21 => "make_function_alloc",
        22 => "make_closure_alloc",
        23 => "jit_new_object",
        24 => "jit_new_array",
        25 => "create_context_alloc",
        26 => "write_barrier",
        27 => "copy_context_alloc",
        28 => "strict_eq_leaf",
        29 => "jit_loose_eq",
        30 => "to_boolean_leaf",
        31 => "number_rem_leaf",
        32 => "jit_load_regexp",
        33 => "jit_coerce_unary",
        34 => "jit_numeric_op",
        35 => "jit_exception_op",
        36 => "jit_iterator_op",
        37 => "jit_bind_function",
        38 => "jit_object_protocol_value",
        39 => "jit_delete_op",
        40 => "jit_scalar_value",
        41 => "jit_super_op",
        42 => "jit_private_op",
        43 => "jit_value_load_op",
        44 => "jit_construct_op",
        45 => "jit_structural_op",
        46 => "jit_class_op",
        47 => "jit_variadic_op",
        48 => "jit_static_call_op",
        49 => "jit_class_value_op",
        50 => "jit_module_op",
        51 => "jit_finish_error",
        52 => "jit_deopt_rebuild_frames",
        53 => "jit_deopt_call",
        54 => "string_char_code_at_leaf",
        55 => "string_code_point_at_leaf",
        56 => "string_index_of_leaf",
        57 => "string_includes_leaf",
        58 => "string_starts_with_leaf",
        59 => "string_ends_with_leaf",
        60 => "array_pop_leaf",
        61 => "array_push_leaf",
        62 => "array_shift_leaf",
        63 => "array_unshift_alloc",
        64 => "math_abs_leaf",
        65 => "math_floor_leaf",
        66 => "math_sqrt_leaf",
        67 => "math_max_leaf",
        68 => "math_min_leaf",
        69 => "collection_map_set_mutating",
        70 => "number_rem_f64_leaf",
        71 => "number_pow_f64_leaf",
        72 => "number_to_int32_f64_leaf",
        73 => "jit_prepare_activation",
        74 => "jit_derived_construct_result",
        75 => "jit_class_super_constructor",
        76 => "jit_promote_entered",
        77 => "parse_int_i32_leaf",
        78 => "array_construct_alloc",
        79 => "jit_route_throw",
        80 => "jit_binding_value",
        81 => "jit_global_declaration_value",
        82 => "jit_collect_arguments",
        83 => "jit_new_object_literal",
        84 => "typeof_test_leaf",
        85 => "jit_call",
        86 => "jit_stage_spread",
        87 => "jit_stage_forward",
        88 => "jit_resolve_method",
        89 => "jit_call_generic",
        90 => "jit_call_overflow",
        91 => "jit_stage_tail_call",
        92 => "element_write_barrier",
        93 => "jit_constructor_terminal",
        94 => "alloc_group_ensure",
        95 => "primitive_string_order",
        96 => "jit_call_native",
        97 => "constructor_receiver_probe",
        98 => "constructor_receiver_commit",
        _ => "unknown_runtime_stub",
    }
}

/// Terminal construction sampling/transfer over the canonical frame roots.
/// `(jit_ctx, pair_ptr)` copies and returns the sole parked compiled pair;
/// no allocation, JavaScript, shape preparation or status reconstruction.
pub const STUB_JIT_CONSTRUCTOR_TERMINAL: RuntimeStubDescriptor = descriptor(
    93,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::ContextWords,
    1,
    RuntimeStubEffects::leaf(false, true),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Compiled,
);

/// Ensure bytes for a fixed allocation group in the canonical LAB.
/// The boxed int32 byte count and two undefined padding operands accompany the
/// actual current safepoint. Success carries undefined; Miss/OOM restore homes
/// and resume the first original allocation. No source effect or OOM latch is
/// committed by this speculative reservation boundary.
pub const STUB_ALLOC_GROUP_ENSURE: RuntimeStubDescriptor = descriptor(
    94,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(false, false),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Pure ordering of Number/String primitive operands. Two Strings compare
/// UTF-16 units; a mixed pair uses canonical StringNumericLiteral conversion.
/// Success carries -1/0/1 or 2 (unordered NaN). Other types miss before effects.
pub const STUB_PRIMITIVE_STRING_ORDER: RuntimeStubDescriptor = descriptor(
    95,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Private JS entry for a proved NativeFunction kind. The canonical pending
/// request and Host Native kernel own actual roots, live policy and completion.
pub const STUB_JIT_CALL_NATIVE: RuntimeStubDescriptor = descriptor(
    96,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::JsCall,
    4,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Execution,
);

/// Pure actual-new.target family admission: `(heap, new.target, boxed baseFID)`.
/// Success is a boxed int32 layout offset; unsupported/unprepared owners Miss.
pub const STUB_CONSTRUCTOR_RECEIVER_PROBE: RuntimeStubDescriptor = descriptor(
    97,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Complete source feedback after a generated receiver/ticket is published.
/// `(jit_ctx, receiver)` is the existing two-word ContextWords physical ABI.
/// Only Success/Fatal can leave this noalloc committed extent.
pub const STUB_CONSTRUCTOR_RECEIVER_COMMIT: RuntimeStubDescriptor = descriptor(
    98,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::ContextWords,
    1,
    RuntimeStubEffects::leaf(false, true),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);

/// Dense inventory of every current machine-callable runtime-stub contract.
pub const RUNTIME_STUB_DESCRIPTORS: &[RuntimeStubDescriptor] = &[
    STUB_JIT_BACKEDGE_POLL,
    STUB_COLLECTION_MAP_GET_LEAF,
    STUB_COLLECTION_MAP_HAS_LEAF,
    STUB_COLLECTION_SET_HAS_LEAF,
    STUB_COLLECTION_MAP_SET_ALLOC,
    STUB_COLLECTION_SET_ADD_ALLOC,
    STUB_COLLECTION_MAP_GET_ALLOC,
    STUB_COLLECTION_MAP_HAS_ALLOC,
    STUB_COLLECTION_SET_HAS_ALLOC,
    STUB_COLLECTION_MAP_DELETE_ALLOC,
    STUB_COLLECTION_SET_DELETE_ALLOC,
    STUB_STRING_CONCAT_ALLOC,
    STUB_JIT_ADD,
    STUB_JIT_LOAD_ELEMENT,
    STUB_JIT_STORE_ELEMENT,
    STUB_JIT_DEFINE_OWN_PROPERTY,
    STUB_JIT_LOAD_PROPERTY,
    STUB_JIT_STORE_PROPERTY,
    STUB_JIT_DEFINE_DATA_PROPERTY,
    STUB_JIT_LOAD_BUILTIN_ERROR,
    STUB_JIT_MAKE_FN,
    STUB_JIT_MAKE_CLOSURE,
    STUB_JIT_NEW_OBJECT,
    STUB_JIT_NEW_ARRAY,
    STUB_CREATE_CONTEXT_ALLOC,
    STUB_WRITE_BARRIER,
    STUB_COPY_CONTEXT_ALLOC,
    STUB_STRICT_EQ_LEAF,
    STUB_JIT_LOOSE_EQ,
    STUB_TO_BOOLEAN_LEAF,
    STUB_NUMBER_REM_LEAF,
    STUB_JIT_LOAD_REGEXP,
    STUB_JIT_COERCE_UNARY,
    STUB_JIT_NUMERIC_OP,
    STUB_JIT_EXCEPTION_OP,
    STUB_JIT_ITERATOR_OP,
    STUB_JIT_BIND_FUNCTION,
    STUB_JIT_OBJECT_PROTOCOL_VALUE,
    STUB_JIT_DELETE_OP,
    STUB_JIT_SCALAR_VALUE,
    STUB_JIT_SUPER_OP,
    STUB_JIT_PRIVATE_OP,
    STUB_JIT_VALUE_LOAD_OP,
    STUB_JIT_CONSTRUCT_OP,
    STUB_JIT_STRUCTURAL_OP,
    STUB_JIT_CLASS_OP,
    STUB_JIT_VARIADIC_OP,
    STUB_JIT_STATIC_CALL_OP,
    STUB_JIT_CLASS_VALUE_OP,
    STUB_JIT_MODULE_OP,
    STUB_JIT_FINISH_ERROR,
    STUB_JIT_DEOPT_WRITEBACK,
    STUB_JIT_DEOPT_CALL,
    STUB_STRING_CHAR_CODE_AT_LEAF,
    STUB_STRING_CODE_POINT_AT_LEAF,
    STUB_STRING_INDEX_OF_LEAF,
    STUB_STRING_INCLUDES_LEAF,
    STUB_STRING_STARTS_WITH_LEAF,
    STUB_STRING_ENDS_WITH_LEAF,
    STUB_ARRAY_POP_LEAF,
    STUB_ARRAY_PUSH_LEAF,
    STUB_ARRAY_SHIFT_LEAF,
    STUB_ARRAY_UNSHIFT_ALLOC,
    STUB_MATH_ABS_LEAF,
    STUB_MATH_FLOOR_LEAF,
    STUB_MATH_SQRT_LEAF,
    STUB_MATH_MAX_LEAF,
    STUB_MATH_MIN_LEAF,
    STUB_COLLECTION_MAP_SET_MUTATING,
    STUB_NUMBER_REM_F64_LEAF,
    STUB_NUMBER_POW_F64_LEAF,
    STUB_NUMBER_TO_INT32_F64_LEAF,
    STUB_JIT_PREPARE_ACTIVATION,
    STUB_JIT_DERIVED_CONSTRUCT_RESULT,
    STUB_JIT_CLASS_SUPER_CONSTRUCTOR,
    STUB_JIT_PROMOTE_ENTERED,
    STUB_PARSE_INT_I32_LEAF,
    STUB_ARRAY_CONSTRUCT_ALLOC,
    STUB_JIT_ROUTE_THROW,
    STUB_JIT_BINDING_VALUE,
    STUB_JIT_GLOBAL_DECLARATION_VALUE,
    STUB_JIT_COLLECT_ARGUMENTS,
    STUB_JIT_NEW_OBJECT_LITERAL,
    STUB_TYPEOF_TEST_LEAF,
    STUB_JIT_CALL,
    STUB_JIT_STAGE_SPREAD,
    STUB_JIT_STAGE_FORWARD,
    STUB_JIT_RESOLVE_METHOD,
    STUB_JIT_CALL_GENERIC,
    STUB_JIT_CALL_OVERFLOW,
    STUB_JIT_STAGE_TAIL_CALL,
    STUB_ELEMENT_WRITE_BARRIER,
    STUB_JIT_CONSTRUCTOR_TERMINAL,
    STUB_ALLOC_GROUP_ENSURE,
    STUB_PRIMITIVE_STRING_ORDER,
    STUB_JIT_CALL_NATIVE,
    STUB_CONSTRUCTOR_RECEIVER_PROBE,
    STUB_CONSTRUCTOR_RECEIVER_COMMIT,
];

/// Validate a descriptor and one concrete call-site safepoint id.
#[must_use]
pub const fn validate_stub_descriptor(
    desc: RuntimeStubDescriptor,
    safepoint_id: SafepointId,
) -> bool {
    if desc.work_units == 0 || desc.work_units != desc.class.work_units() {
        return false;
    }
    let alloc_gc = RuntimeStubEffects::MAY_ALLOCATE | RuntimeStubEffects::MAY_TRIGGER_GC;
    let throwing_matches = desc.effects.contains(RuntimeStubEffects::MAY_THROW)
        == matches!(desc.exception, RuntimeStubException::Status);
    let physical_domain_matches = match desc.result_abi {
        RuntimeStubResultAbi::NativePair => !matches!(desc.result_domain, NativeResultDomain::None),
        RuntimeStubResultAbi::StatusWord
        | RuntimeStubResultAbi::ValueWord
        | RuntimeStubResultAbi::Float64 => {
            matches!(desc.result_domain, NativeResultDomain::None)
        }
    };
    let result_matches = match desc.signature {
        RuntimeStubSignature::LeafValue2
        | RuntimeStubSignature::MutatingLeafValue2
        | RuntimeStubSignature::MutatingLeafValue3
        | RuntimeStubSignature::AllocValue3 => matches!(
            (desc.result_abi, desc.result_domain),
            (RuntimeStubResultAbi::NativePair, NativeResultDomain::Probe)
        ),
        RuntimeStubSignature::ReentrantValue2
        | RuntimeStubSignature::ReentrantValue3
        | RuntimeStubSignature::ReentrantNamedLoad
        | RuntimeStubSignature::ReentrantNamedStore
        | RuntimeStubSignature::ReentrantValueSpan
        | RuntimeStubSignature::CommittedValue2 => matches!(
            (desc.result_abi, desc.result_domain),
            (
                RuntimeStubResultAbi::NativePair,
                NativeResultDomain::Committed
            )
        ),
        RuntimeStubSignature::RouteThrow1 => matches!(
            (desc.result_abi, desc.result_domain),
            (
                RuntimeStubResultAbi::NativePair,
                NativeResultDomain::Compiled
            )
        ),
        RuntimeStubSignature::ExecutionEntry0 | RuntimeStubSignature::JsCall => {
            desc.argument_count
                == if matches!(desc.signature, RuntimeStubSignature::JsCall) {
                    4
                } else {
                    0
                }
                && matches!(
                    (desc.result_abi, desc.result_domain),
                    (
                        RuntimeStubResultAbi::NativePair,
                        NativeResultDomain::Execution
                    )
                )
        }
        RuntimeStubSignature::Poll1 => matches!(
            (desc.result_abi, desc.result_domain),
            (RuntimeStubResultAbi::StatusWord, NativeResultDomain::None)
        ),
        RuntimeStubSignature::Float64Leaf2 => matches!(
            (desc.result_abi, desc.result_domain),
            (RuntimeStubResultAbi::Float64, NativeResultDomain::None)
        ),
        RuntimeStubSignature::Float64ToWordLeaf1 => matches!(
            (desc.result_abi, desc.result_domain),
            (RuntimeStubResultAbi::ValueWord, NativeResultDomain::None)
        ),
        RuntimeStubSignature::ContextWords => {
            desc.argument_count >= 1
                && desc.argument_count <= 4
                && matches!(
                    (desc.result_abi, desc.result_domain),
                    (
                        RuntimeStubResultAbi::NativePair,
                        NativeResultDomain::Committed | NativeResultDomain::Probe
                    )
                )
                || (desc.id == STUB_JIT_CONSTRUCTOR_TERMINAL.id
                    && desc.argument_count == 1
                    && matches!(desc.class, RuntimeStubClass::LeafNoAlloc)
                    && matches!(
                        (desc.result_abi, desc.result_domain),
                        (
                            RuntimeStubResultAbi::NativePair,
                            NativeResultDomain::Compiled
                        )
                    ))
        }
        RuntimeStubSignature::Variadic => match (desc.result_abi, desc.result_domain) {
            (_, NativeResultDomain::Execution) => false,
            (
                RuntimeStubResultAbi::NativePair,
                NativeResultDomain::Compiled | NativeResultDomain::ExceptionTransition,
            ) => true,
            (RuntimeStubResultAbi::StatusWord, NativeResultDomain::None) => true,
            (RuntimeStubResultAbi::ValueWord, NativeResultDomain::None) => {
                matches!(desc.exception, RuntimeStubException::Never)
            }
            (
                RuntimeStubResultAbi::Float64 | RuntimeStubResultAbi::NativePair,
                NativeResultDomain::None
                | NativeResultDomain::Committed
                | NativeResultDomain::Probe,
            )
            | (
                RuntimeStubResultAbi::StatusWord
                | RuntimeStubResultAbi::ValueWord
                | RuntimeStubResultAbi::Float64,
                NativeResultDomain::Compiled
                | NativeResultDomain::ExceptionTransition
                | NativeResultDomain::Committed
                | NativeResultDomain::Probe,
            ) => false,
        },
    };
    if !throwing_matches || !physical_domain_matches || !result_matches {
        return false;
    }
    match desc.class {
        RuntimeStubClass::LeafNoAlloc => {
            matches!(desc.safepoint, RuntimeStubSafepoint::Forbidden)
                && safepoint_id == NO_SAFEPOINT
                && desc.effects.bits()
                    & (RuntimeStubEffects::MAY_ALLOCATE
                        | RuntimeStubEffects::MAY_TRIGGER_GC
                        | RuntimeStubEffects::MAY_REENTER_JS)
                    == 0
        }
        RuntimeStubClass::Alloc => {
            matches!(desc.safepoint, RuntimeStubSafepoint::Required)
                && safepoint_id != NO_SAFEPOINT
                && desc.effects.contains(alloc_gc)
                && !desc.effects.contains(RuntimeStubEffects::MAY_REENTER_JS)
        }
        RuntimeStubClass::Reentrant => {
            matches!(desc.safepoint, RuntimeStubSafepoint::Required)
                && safepoint_id != NO_SAFEPOINT
                && desc.effects.contains(
                    alloc_gc | RuntimeStubEffects::MAY_THROW | RuntimeStubEffects::MAY_REENTER_JS,
                )
        }
    }
}

const _: [(); 16] = [(); std::mem::size_of::<RuntimeStubDescriptor>()];
const _: [(); 4] = [(); std::mem::align_of::<RuntimeStubDescriptor>()];
const _: [(); 0] = [(); std::mem::offset_of!(RuntimeStubDescriptor, id)];
const _: [(); 10] = [(); std::mem::offset_of!(RuntimeStubDescriptor, result_abi)];
const _: [(); 11] = [(); std::mem::offset_of!(RuntimeStubDescriptor, result_domain)];
const _: [(); 12] = [(); std::mem::offset_of!(RuntimeStubDescriptor, effects)];
const _: [(); 24] = [(); std::mem::size_of::<RuntimeStubAllocContext>()];
const _: [(); 8] = [(); std::mem::offset_of!(RuntimeStubAllocContext, spill_slots)];
const _: [(); 16] = [(); std::mem::offset_of!(RuntimeStubAllocContext, safepoint_id)];

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_status_words(descriptors: &[RuntimeStubDescriptor], exception: RuntimeStubException) {
        for descriptor in descriptors {
            assert_eq!(descriptor.result_abi, RuntimeStubResultAbi::StatusWord);
            assert_eq!(descriptor.result_domain, NativeResultDomain::None);
            assert_eq!(descriptor.exception, exception);
        }
    }

    #[test]
    fn inventory_is_dense_unique_and_fully_classified() {
        for (index, descriptor) in RUNTIME_STUB_DESCRIPTORS.iter().enumerate() {
            assert_eq!(descriptor.id as usize, index + 1);
            assert_ne!(runtime_stub_name(descriptor.id), "unknown_runtime_stub");
            let safepoint = if descriptor.class == RuntimeStubClass::LeafNoAlloc {
                NO_SAFEPOINT
            } else {
                0
            };
            assert!(validate_stub_descriptor(*descriptor, safepoint));
        }
    }

    #[test]
    fn leaf_forbids_allocation_reentry_and_safepoint() {
        assert!(validate_stub_descriptor(
            STUB_COLLECTION_MAP_GET_LEAF,
            NO_SAFEPOINT
        ));
        assert!(!validate_stub_descriptor(STUB_COLLECTION_MAP_GET_LEAF, 0));
        let mut invalid = STUB_COLLECTION_MAP_GET_LEAF;
        invalid.effects = RuntimeStubEffects::allocating(false, false);
        assert!(!validate_stub_descriptor(invalid, NO_SAFEPOINT));
    }

    #[test]
    fn allocating_and_reentrant_require_safepoints() {
        assert!(!validate_stub_descriptor(
            STUB_COLLECTION_MAP_SET_ALLOC,
            NO_SAFEPOINT
        ));
        assert!(validate_stub_descriptor(STUB_COLLECTION_MAP_SET_ALLOC, 7));
    }

    #[test]
    fn status_word_success_throw_subset_is_explicit() {
        assert_status_words(
            &[
                STUB_JIT_BACKEDGE_POLL,
                STUB_JIT_ADD,
                STUB_JIT_DEFINE_OWN_PROPERTY,
                STUB_JIT_DEFINE_DATA_PROPERTY,
                STUB_JIT_LOAD_BUILTIN_ERROR,
                STUB_JIT_LOAD_REGEXP,
                STUB_JIT_COERCE_UNARY,
                STUB_JIT_NUMERIC_OP,
            ],
            RuntimeStubException::Status,
        );
    }

    #[test]
    fn status_word_success_side_exit_throw_subset_is_explicit() {
        assert_status_words(
            &[
                STUB_JIT_LOOSE_EQ,
                STUB_JIT_ITERATOR_OP,
                STUB_JIT_BIND_FUNCTION,
                STUB_JIT_DELETE_OP,
                STUB_JIT_SUPER_OP,
                STUB_JIT_PRIVATE_OP,
                STUB_JIT_VALUE_LOAD_OP,
                STUB_JIT_CONSTRUCT_OP,
                STUB_JIT_STRUCTURAL_OP,
                STUB_JIT_CLASS_OP,
                STUB_JIT_VARIADIC_OP,
                STUB_JIT_STATIC_CALL_OP,
                STUB_JIT_CLASS_VALUE_OP,
                STUB_JIT_MODULE_OP,
            ],
            RuntimeStubException::Status,
        );
    }

    #[test]
    fn context_word_descriptors_have_exact_fixed_arities() {
        assert_eq!(
            STUB_JIT_DERIVED_CONSTRUCT_RESULT.signature,
            RuntimeStubSignature::ContextWords
        );
        assert_eq!(STUB_JIT_DERIVED_CONSTRUCT_RESULT.argument_count, 1);
    }

    #[test]
    fn lexical_allocation_entries_are_fixed_value_alloc_probes() {
        for (descriptor, id) in [
            (STUB_JIT_MAKE_FN, 21),
            (STUB_JIT_MAKE_CLOSURE, 22),
            (STUB_CREATE_CONTEXT_ALLOC, 25),
            (STUB_COPY_CONTEXT_ALLOC, 27),
        ] {
            assert_eq!(descriptor.id, id);
            assert_eq!(descriptor.class, RuntimeStubClass::Alloc);
            assert_eq!(descriptor.signature, RuntimeStubSignature::AllocValue3);
            assert_eq!(descriptor.argument_count, 3);
            assert_eq!(descriptor.exception, RuntimeStubException::Never);
            assert_eq!(descriptor.result_abi, RuntimeStubResultAbi::NativePair);
            assert_eq!(descriptor.result_domain, NativeResultDomain::Probe);
            assert_eq!(
                descriptor,
                RuntimeStubDescriptor {
                    id,
                    ..STUB_ARRAY_CONSTRUCT_ALLOC
                }
            );
        }
    }

    #[test]
    fn element_entries_are_fixed_value_reentrant_committed_pairs() {
        assert_eq!(
            STUB_JIT_LOAD_ELEMENT.signature,
            RuntimeStubSignature::ReentrantValue2
        );
        assert_eq!(STUB_JIT_LOAD_ELEMENT.argument_count, 2);
        assert_eq!(
            STUB_JIT_LOAD_ELEMENT.result_abi,
            RuntimeStubResultAbi::NativePair
        );
        assert_eq!(
            STUB_JIT_LOAD_ELEMENT.result_domain,
            NativeResultDomain::Committed
        );
        assert_eq!(
            STUB_JIT_STORE_ELEMENT.signature,
            RuntimeStubSignature::ReentrantValue3
        );
        assert_eq!(STUB_JIT_STORE_ELEMENT.argument_count, 3);
        assert_eq!(
            STUB_JIT_STORE_ELEMENT.result_abi,
            RuntimeStubResultAbi::NativePair
        );
        assert_eq!(
            STUB_JIT_STORE_ELEMENT.result_domain,
            NativeResultDomain::Committed
        );

        for descriptor in [STUB_JIT_LOAD_ELEMENT, STUB_JIT_STORE_ELEMENT] {
            assert_eq!(descriptor.class, RuntimeStubClass::Reentrant);
            assert_eq!(descriptor.safepoint, RuntimeStubSafepoint::Required);
            assert_eq!(descriptor.exception, RuntimeStubException::Status);
            assert!(descriptor.effects.contains(
                RuntimeStubEffects::MAY_ALLOCATE
                    | RuntimeStubEffects::MAY_TRIGGER_GC
                    | RuntimeStubEffects::MAY_THROW
                    | RuntimeStubEffects::MAY_REENTER_JS
                    | RuntimeStubEffects::MAY_MUTATE_GC
            ));
            assert!(validate_stub_descriptor(descriptor, 0));
            assert!(!validate_stub_descriptor(descriptor, NO_SAFEPOINT));
        }
    }

    #[test]
    fn named_property_entries_are_fixed_value_reentrant_committed_pairs() {
        assert_eq!(
            STUB_JIT_LOAD_PROPERTY.signature,
            RuntimeStubSignature::ReentrantNamedLoad
        );
        assert_eq!(STUB_JIT_LOAD_PROPERTY.argument_count, 1);
        assert_eq!(runtime_stub_name(17), "jit_load_property_value");
        assert_eq!(
            STUB_JIT_STORE_PROPERTY.signature,
            RuntimeStubSignature::ReentrantNamedStore
        );
        assert_eq!(STUB_JIT_STORE_PROPERTY.argument_count, 2);
        assert_eq!(runtime_stub_name(18), "jit_store_property_value");

        for descriptor in [STUB_JIT_LOAD_PROPERTY, STUB_JIT_STORE_PROPERTY] {
            assert_eq!(descriptor.class, RuntimeStubClass::Reentrant);
            assert_eq!(descriptor.safepoint, RuntimeStubSafepoint::Required);
            assert_eq!(descriptor.exception, RuntimeStubException::Status);
            assert_eq!(descriptor.result_abi, RuntimeStubResultAbi::NativePair);
            assert_eq!(descriptor.result_domain, NativeResultDomain::Committed);
            assert!(descriptor.effects.contains(
                RuntimeStubEffects::MAY_ALLOCATE
                    | RuntimeStubEffects::MAY_TRIGGER_GC
                    | RuntimeStubEffects::MAY_THROW
                    | RuntimeStubEffects::MAY_REENTER_JS
                    | RuntimeStubEffects::MAY_MUTATE_GC
            ));
            assert!(validate_stub_descriptor(descriptor, 0));
            assert!(!validate_stub_descriptor(descriptor, NO_SAFEPOINT));
        }
    }

    #[test]
    fn committed_value_families_share_one_fixed_physical_abi() {
        assert_eq!(
            runtime_stub_name(STUB_JIT_OBJECT_PROTOCOL_VALUE.id),
            "jit_object_protocol_value"
        );
        assert_eq!(
            runtime_stub_name(STUB_JIT_SCALAR_VALUE.id),
            "jit_scalar_value"
        );

        for descriptor in [STUB_JIT_OBJECT_PROTOCOL_VALUE, STUB_JIT_SCALAR_VALUE] {
            assert_eq!(descriptor.signature, RuntimeStubSignature::CommittedValue2);
            assert_eq!(descriptor.argument_count, 2);
            assert_eq!(descriptor.class, RuntimeStubClass::Reentrant);
            assert_eq!(descriptor.safepoint, RuntimeStubSafepoint::Required);
            assert_eq!(descriptor.exception, RuntimeStubException::Status);
            assert_eq!(descriptor.result_abi, RuntimeStubResultAbi::NativePair);
            assert_eq!(descriptor.result_domain, NativeResultDomain::Committed);
            assert!(validate_stub_descriptor(descriptor, 0));
            assert!(!validate_stub_descriptor(descriptor, NO_SAFEPOINT));
        }
    }

    #[test]
    fn pure_throw_router_is_one_value_reentrant_compiled_pair() {
        assert_eq!(
            runtime_stub_name(STUB_JIT_ROUTE_THROW.id),
            "jit_route_throw"
        );
        assert_eq!(
            STUB_JIT_ROUTE_THROW.signature,
            RuntimeStubSignature::RouteThrow1
        );
        assert_eq!(STUB_JIT_ROUTE_THROW.argument_count, 1);
        assert_eq!(
            STUB_JIT_ROUTE_THROW.result_abi,
            RuntimeStubResultAbi::NativePair
        );
        assert_eq!(
            STUB_JIT_ROUTE_THROW.result_domain,
            NativeResultDomain::Compiled
        );
        assert!(validate_stub_descriptor(STUB_JIT_ROUTE_THROW, 0));
        assert!(!validate_stub_descriptor(
            STUB_JIT_ROUTE_THROW,
            NO_SAFEPOINT
        ));
    }

    #[test]
    fn collect_arguments_entry_is_an_allocating_status_word_transition() {
        assert_eq!(
            STUB_JIT_COLLECT_ARGUMENTS.signature,
            RuntimeStubSignature::Variadic
        );
        assert_eq!(STUB_JIT_COLLECT_ARGUMENTS.class, RuntimeStubClass::Alloc);
        assert_eq!(
            runtime_stub_name(STUB_JIT_COLLECT_ARGUMENTS.id),
            "jit_collect_arguments"
        );
        assert_eq!(
            STUB_JIT_COLLECT_ARGUMENTS.result_abi,
            RuntimeStubResultAbi::StatusWord
        );
    }

    #[test]
    fn native_pair_descriptors_reject_wrong_or_missing_domains() {
        let mut wrong = STUB_JIT_LOAD_ELEMENT;
        wrong.result_domain = NativeResultDomain::Probe;
        assert!(!validate_stub_descriptor(wrong, 0));

        wrong.result_domain = NativeResultDomain::None;
        assert!(!validate_stub_descriptor(wrong, 0));

        let mut non_pair = STUB_JIT_BACKEDGE_POLL;
        non_pair.result_domain = NativeResultDomain::Compiled;
        assert!(!validate_stub_descriptor(non_pair, NO_SAFEPOINT));
    }
}
