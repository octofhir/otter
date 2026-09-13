//! Classified machine-callable runtime-stub contracts.
//!
//! # Contents
//! - [`RuntimeStubDescriptor`] declares signature, effects, safepoint,
//!   exception, and result ABI for every dense [`RuntimeStubId`].
//! - [`RuntimeStubAllocContext`] is the rooted allocation packet passed by
//!   every allocating entry.
//! - Typed scalar leaves keep unboxed numeric values in their machine ABI.
//!
//! # Invariants
//! - The inventory is dense and unique; descriptor `id == index + 1`.
//! - Leaf stubs cannot allocate, trigger GC, reenter JS, or name a safepoint.
//! - Allocating and reentrant stubs require a precise safepoint at every call.
//! - Throwing behavior and result-status encoding are explicit descriptor data.
//!
//! # See also
//! - [`crate::runtime_stubs`] for semantic entrypoints.
//! - [`super::safepoints`] for root maps.

use super::{NO_SAFEPOINT, NativeFrame, NativeResultDomain, SafepointId, VmThread};

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
    /// `(jit_ctx, receiver, property_ic_cell) -> NativeResultPair`
    /// reentrant named-property read.
    ///
    /// The receiver is a boxed JavaScript value. `property_ic_cell` is stable
    /// compiler-owned metadata rather than a GC value; the published native
    /// frame supplies the exact function and logical-PC identity from which
    /// the VM decodes the property name and feedback site.
    ReentrantNamedLoad = 11,
    /// `(jit_ctx, receiver, value, property_ic_cell) -> NativeResultPair`
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
    /// `(jit_ctx) -> status-word` acknowledgement executed only when a
    /// Machine local catch absorbs a pure exception value.
    ///
    /// The leaf clears diagnostic provenance that must survive propagation but
    /// must not leak past the catch body. It cannot allocate, collect, reenter,
    /// or throw.
    AcknowledgeCaughtThrow0 = 16,
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
    pub const fn has_frame_slots(self) -> bool {
        let frame = self.current_frame();
        if frame.is_null() {
            return false;
        }
        // SAFETY: callers uphold the live published-frame contract.
        let frame = unsafe { &*frame };
        frame.register_base != 0 && frame.header.register_count != 0
    }

    /// Currently published activation, or null outside compiled execution.
    #[must_use]
    pub const fn current_frame(self) -> *mut NativeFrame {
        if self.thread.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: callers uphold the live VM-thread contract.
        unsafe { (*self.thread).current_frame as *mut NativeFrame }
    }

    /// Installed code generation owning the current activation.
    #[must_use]
    pub const fn code_object_id(self) -> u64 {
        if self.thread.is_null() {
            return 0;
        }
        // SAFETY: callers uphold the live VM-thread contract.
        unsafe { (*self.thread).current_code_object_id }
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
/// Named-property read over one boxed receiver and stable IC cell.
///
/// Accessors, proxies, and exotic receivers may re-enter JavaScript. The
/// published native frame identifies the exact `LoadProperty`; success returns
/// its value and may patch the supplied cell, while failure returns one pure
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
/// Named-property write over boxed receiver/value operands and a stable IC
/// cell.
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
/// `MakeFunction` closure construction.
pub const STUB_JIT_MAKE_FN: RuntimeStubDescriptor = descriptor(
    21,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);
/// `MakeClosure` construction with captured parent upvalues.
pub const STUB_JIT_MAKE_CLOSURE: RuntimeStubDescriptor = descriptor(
    22,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
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
/// Fresh loop-iteration upvalue cell allocation.
pub const STUB_JIT_FRESH_UPVALUE: RuntimeStubDescriptor = descriptor(
    25,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);
/// Registers a native activation's scalar root slots.
pub const STUB_JIT_PUSH_NATIVE_ACTIVATION: RuntimeStubDescriptor = descriptor(
    26,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::leaf(false, false),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);
/// Releases the topmost native activation registration.
pub const STUB_JIT_POP_NATIVE_ACTIVATION: RuntimeStubDescriptor = descriptor(
    27,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::leaf(false, false),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);

/// Generational and insertion write barrier for one pointer store.
///
/// Generated code runs both halves inline and reaches this only when a marking
/// cycle is in progress or the store really creates an unrecorded old->young
/// edge. It therefore takes the parent's header address and the stored value
/// directly: no register window, no published frame, no reentry.
pub const STUB_WRITE_BARRIER: RuntimeStubDescriptor = descriptor(
    28,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::MutatingLeafValue2,
    2,
    RuntimeStubEffects::leaf(false, true),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);
/// Validates an inline-call closure and returns its upvalue base.
pub const STUB_JIT_INLINE_CLOSURE_UPVALUES: RuntimeStubDescriptor = descriptor(
    29,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::leaf(false, false),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::ValueWord,
    NativeResultDomain::None,
);
/// Leaf §7.2.15 IsStrictlyEqual probe over two raw operand words: never
/// throws, never allocates; a null heap reports a miss so probe harnesses
/// without a live isolate fall back to normal dispatch.
pub const STUB_STRICT_EQ_LEAF: RuntimeStubDescriptor = descriptor(
    30,
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
    31,
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
    34,
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
    32,
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
    33,
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
    59,
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
    60,
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
    61,
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
    62,
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
    63,
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
    64,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Completes one full fixed-arity `Op::New` or `Op::SuperConstruct` outside
/// the compiled subset; the constructor body may run arbitrary JS.
pub const STUB_JIT_CONSTRUCT: RuntimeStubDescriptor = descriptor(
    35,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);

/// Completes one coercive `ToPrimitive` or `ToNumeric` opcode; user conversion
/// hooks may allocate, throw, and re-enter arbitrary JS.
pub const STUB_JIT_COERCE_UNARY: RuntimeStubDescriptor = descriptor(
    36,
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
    37,
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
    38,
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
    39,
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
    40,
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
    41,
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
    42,
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
    43,
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
    44,
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
    45,
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
    46,
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
    47,
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
    48,
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
    49,
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
    50,
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
    51,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);

/// Completes spread calls/constructions, explicit-receiver calls, generic
/// method-call misses, and `CollectArguments` through the VM's synchronous
/// call helpers. `TailCall` is excluded: its interpreter path reuses the
/// caller frame for true tail recursion, so it stays an exact side exit rather
/// than a nested call.
pub const STUB_JIT_SPREAD_CALL_OP: RuntimeStubDescriptor = descriptor(
    52,
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
    53,
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
    54,
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
    55,
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
/// A generated exit site passes its site index; the shared handler dumps the
/// allocatable machine registers and calls this with the code object's baked
/// [`crate::deopt::DeoptRuntime`], the dump, the frame's stack pointer and its
/// register window. The stub reconstitutes every slot of every owed frame, so
/// exit sites carry no reconstruction code at all.
///
/// An exit owing only the compiled function's own frame writes it into the
/// published window and reports a bail. An exit owing a chain of spliced
/// frames *constructs* them all in owned storage and runs the chain to
/// completion, reporting the outermost frame's return value — nothing about
/// that depends on how the compiled function was entered.
pub const STUB_JIT_DEOPT_WRITEBACK: RuntimeStubDescriptor = descriptor(
    56,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Compiled,
);

/// Resume one already-started compiler-generated stack call after its native
/// callee side-exits.
///
/// The complete `NativeFrame` and tagged stack window remain published for
/// this cold transition. It materializes the callee once, dispatches from the
/// exact native PC, and returns the final value/status pair to generated
/// linkage. This is deoptimization support, never normal call preparation.
pub const STUB_JIT_DEOPT_STACK_CALL: RuntimeStubDescriptor = descriptor(
    57,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Compiled,
);

/// Cold no-allocation repair for a stable generated-call function entry.
///
/// The normal path reads the published generation cell entirely in machine
/// code. A zero target calls this resolver once to republish any already
/// installed fallback generation, returning its generation-cell address or
/// zero when the caller must take an exact pre-effect side exit.
pub const STUB_JIT_RESOLVE_DIRECT_ENTRY: RuntimeStubDescriptor = descriptor(
    58,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::ValueWord,
    NativeResultDomain::None,
);

/// Leaf dense-array `Array.prototype.pop` mutation.
///
/// Truncating the dense buffer drops a reference and rewrites the cached
/// length pair; neither allocates. The entry re-checks the dense
/// preconditions the inline guard cannot see (writable `length`, a present
/// own last element, no accessor override in range) and reports a miss when
/// they fail, so the call site falls through to ordinary dispatch.
pub const STUB_ARRAY_POP_LEAF: RuntimeStubDescriptor = descriptor(
    65,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::MutatingLeafValue2,
    2,
    RuntimeStubEffects::leaf(false, true),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);
/// Allocating dense-array `Array.prototype.push` mutation.
///
/// Appending may grow the dense buffer, so the site publishes a precise
/// safepoint. Like the `pop` entry it re-checks the dense preconditions and
/// misses instead of falling back internally.
pub const STUB_ARRAY_PUSH_ALLOC: RuntimeStubDescriptor = descriptor(
    66,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(false, true),
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
    67,
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
    68,
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
    74,
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
    75,
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
    76,
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
    77,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Float64ToWordLeaf1,
    1,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::ValueWord,
    NativeResultDomain::None,
);

/// Reentrant `OrdinaryCreateFromConstructor` preparation for one generated
/// base-constructor call. Returns the rooted receiver through a status pair;
/// the constructor body has not started yet.
pub const STUB_JIT_PREPARE_BASE_CONSTRUCT: RuntimeStubDescriptor = descriptor(
    78,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::ContextWords,
    3,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);

/// Derived-constructor return validation over `(result, bound_this)`.
pub const STUB_JIT_DERIVED_CONSTRUCT_RESULT: RuntimeStubDescriptor = descriptor(
    79,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::ContextWords,
    2,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);

/// Read the live superclass constructor from an exact class wrapper.
pub const STUB_JIT_CLASS_SUPER_CONSTRUCTOR: RuntimeStubDescriptor = descriptor(
    80,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Variadic,
    1,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::ValueWord,
    NativeResultDomain::None,
);

/// Copy the dense values collected for one spread construct into the
/// parameter prefix of an unpublished stack-owned callee frame.
///
/// Eligible generated callees neither observe `arguments` nor own a rest
/// parameter, so values beyond the declared parameter count are intentionally
/// ignored. The source is the compiler-created dense argument array; a
/// different value reports a pre-entry guard miss.
pub const STUB_JIT_COPY_SPREAD_ARGUMENTS: RuntimeStubDescriptor = descriptor(
    81,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Variadic,
    3,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::ValueWord,
    NativeResultDomain::None,
);
/// Allocate fresh capture cells and complete an unpublished generated frame's
/// stack-owned upvalue spine.
pub const STUB_JIT_INITIALIZE_UPVALUES: RuntimeStubDescriptor = descriptor(
    82,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::ContextWords,
    3,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Allocating, non-reentrant `OrdinaryCreateFromConstructor` fast path for a
/// generated base constructor. An uncertain or observable `prototype` lookup
/// reports a pre-effect miss through the status pair. The third operand names
/// the exact generated target so conservative straight-line initializers can
/// allocate their receiver with the final hidden class and undefined slots.
pub const STUB_JIT_TRY_PREPARE_BASE_CONSTRUCT: RuntimeStubDescriptor = descriptor(
    83,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::ContextWords,
    4,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Leaf `Math.abs`.
///
/// A numeric builtin reached through a declared entry rather than a
/// per-builtin machine-code body. Adding a sibling is a declaration plus its
/// entry; it costs no generated code.
pub const STUB_MATH_ABS_LEAF: RuntimeStubDescriptor = descriptor(
    69,
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
    70,
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
    71,
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
    72,
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
    73,
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
    84,
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
    85,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(false, false),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Probe,
);

/// Complete the exact published `CallMethodValue` from one boxed-value span.
///
/// The span contains the receiver followed by every actual argument. Function
/// and logical-PC identity come from the published native frame; the VM
/// decodes the immutable method name and declared argument count from that
/// instruction before any observable operation. Success or throw commits
/// exactly once and never asks generated code to replay the call.
pub const STUB_JIT_CALL_METHOD_VALUE: RuntimeStubDescriptor = descriptor(
    86,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::ReentrantValueSpan,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);

/// Complete the exact published `CallWithThis` (or an attempted plain `Call`
/// whose receiver is `undefined`) from an owned boxed-value span: the callee,
/// the receiver, then every argument. The published frame supplies exact
/// function/PC identity and precise roots; the call commits exactly once and
/// never asks generated code to replay it.
pub const STUB_JIT_CALL_WITH_THIS_VALUE: RuntimeStubDescriptor = descriptor(
    91,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::ReentrantValueSpan,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);

/// Complete the exact published `New` from an owned boxed-value span: the
/// constructor, then every argument. The published frame supplies exact
/// function/PC identity and precise roots; the construct commits exactly once
/// and never asks generated code to replay it.
pub const STUB_JIT_CONSTRUCT_VALUE: RuntimeStubDescriptor = descriptor(
    92,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::ReentrantValueSpan,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);

/// Build the activation's `arguments` object into one destination register.
/// A stack-owned frame supplies the actual arguments its generated caller
/// published after the register window; a materialized activation supplies
/// them from its cold record. Allocating, never reentrant.
pub const STUB_JIT_COLLECT_ARGUMENTS: RuntimeStubDescriptor = descriptor(
    93,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
);

/// Complete an admitted forwarding source from method/callee/receiver and
/// current register-alias values. The separate source probe rejects required
/// stack-owned caller materialization before effects. This entry commits once
/// and returns a pure value/exception, never a destination or replay status.
pub const STUB_JIT_CALL_FORWARD_ARGUMENTS: RuntimeStubDescriptor = descriptor(
    94,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::ReentrantValueSpan,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Committed,
);

/// Probe the already-resolved apply and count an elided live argument window.
/// Returns the full Uint32 count, or `u64::MAX` for a pre-effect miss.
pub const STUB_JIT_FORWARD_ARGUMENT_COUNT: RuntimeStubDescriptor = descriptor(
    95,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Variadic,
    1,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::ValueWord,
    NativeResultDomain::None,
);

/// Copy incoming actuals and captured aliases into an initialized private callee.
/// Returns the actual count, or `u64::MAX` on a pre-entry miss. Generated code
/// patches register aliases from current rooted homes before publication.
pub const STUB_JIT_COPY_FORWARDED_ARGUMENTS: RuntimeStubDescriptor = descriptor(
    96,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Variadic,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::ValueWord,
    NativeResultDomain::None,
);

/// Resolve current ordinary-call metadata into native scratch and count live
/// forwarded actuals. Returns the count or u64::MAX before any call effects.
pub const STUB_JIT_FORWARD_CALL_PLAN: RuntimeStubDescriptor = descriptor(
    97,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Variadic,
    3,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::ValueWord,
    NativeResultDomain::None,
);

/// Probe whether a forwarding source can complete without materializing a
/// stack-owned caller first. Returns one for admitted sources, zero otherwise.
pub const STUB_JIT_FORWARD_SOURCE_READY: RuntimeStubDescriptor = descriptor(
    98,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Variadic,
    1,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::ValueWord,
    NativeResultDomain::None,
);

/// Complete the exact published schema-owned binding operation from two boxed
/// values. Function/PC identity selects the semantic family and operand roles
/// through `otter_bytecode::opcode_schema::BindingSemantics`; a result
/// register is committed by generated code only after the returned `Ok`.
pub const STUB_JIT_BINDING_VALUE: RuntimeStubDescriptor = descriptor(
    89,
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
    90,
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
/// This entry is never used by a Machine local landing.
pub const STUB_JIT_ROUTE_THROW: RuntimeStubDescriptor = descriptor(
    87,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::RouteThrow1,
    1,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::NativePair,
    NativeResultDomain::Compiled,
);

/// Acknowledge a pure exception absorbed by a Machine local catch landing.
///
/// This exceptional-only leaf clears preserved uncaught-frame provenance and
/// stale rendered detail before the catch body can throw again. Propagating
/// paths never call it.
pub const STUB_JIT_ACKNOWLEDGE_CAUGHT_THROW: RuntimeStubDescriptor = descriptor(
    88,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::AcknowledgeCaughtThrow0,
    0,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::StatusWord,
    NativeResultDomain::None,
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
        21 => "jit_make_fn",
        22 => "jit_make_closure",
        23 => "jit_new_object",
        24 => "jit_new_array",
        25 => "jit_fresh_upvalue",
        26 => "jit_push_native_activation",
        27 => "jit_pop_native_activation",
        28 => "write_barrier",
        29 => "jit_inline_closure_upvalues",
        30 => "strict_eq_leaf",
        31 => "jit_loose_eq",
        32 => "to_boolean_leaf",
        33 => "number_rem_leaf",
        34 => "jit_load_regexp",
        35 => "jit_construct",
        36 => "jit_coerce_unary",
        37 => "jit_numeric_op",
        38 => "jit_exception_op",
        39 => "jit_iterator_op",
        40 => "jit_bind_function",
        41 => "jit_object_protocol_value",
        42 => "jit_delete_op",
        43 => "jit_scalar_value",
        44 => "jit_super_op",
        45 => "jit_private_op",
        46 => "jit_value_load_op",
        47 => "jit_construct_op",
        48 => "jit_structural_op",
        49 => "jit_class_op",
        50 => "jit_variadic_op",
        51 => "jit_static_call_op",
        52 => "jit_spread_call_op",
        53 => "jit_class_value_op",
        54 => "jit_module_op",
        55 => "jit_finish_error",
        56 => "jit_deopt_rebuild_frames",
        57 => "jit_deopt_stack_call",
        58 => "jit_resolve_direct_entry",
        59 => "string_char_code_at_leaf",
        60 => "string_code_point_at_leaf",
        61 => "string_index_of_leaf",
        62 => "string_includes_leaf",
        63 => "string_starts_with_leaf",
        64 => "string_ends_with_leaf",
        65 => "array_pop_leaf",
        66 => "array_push_alloc",
        67 => "array_shift_leaf",
        68 => "array_unshift_alloc",
        69 => "math_abs_leaf",
        70 => "math_floor_leaf",
        71 => "math_sqrt_leaf",
        72 => "math_max_leaf",
        73 => "math_min_leaf",
        74 => "collection_map_set_mutating",
        75 => "number_rem_f64_leaf",
        76 => "number_pow_f64_leaf",
        77 => "number_to_int32_f64_leaf",
        78 => "jit_prepare_base_construct",
        79 => "jit_derived_construct_result",
        80 => "jit_class_super_constructor",
        81 => "jit_copy_spread_arguments",
        82 => "jit_initialize_upvalues",
        83 => "jit_try_prepare_base_construct",
        84 => "parse_int_i32_leaf",
        85 => "array_construct_alloc",
        86 => "jit_call_method_value",
        87 => "jit_route_throw",
        88 => "jit_acknowledge_caught_throw",
        89 => "jit_binding_value",
        90 => "jit_global_declaration_value",
        91 => "jit_call_with_this_value",
        92 => "jit_construct_value",
        93 => "jit_collect_arguments",
        94 => "jit_call_forward_arguments",
        95 => "jit_forward_argument_count",
        96 => "jit_copy_forwarded_arguments",
        97 => "jit_forward_call_plan",
        98 => "jit_forward_source_ready",
        _ => "unknown_runtime_stub",
    }
}

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
    STUB_JIT_FRESH_UPVALUE,
    STUB_JIT_PUSH_NATIVE_ACTIVATION,
    STUB_JIT_POP_NATIVE_ACTIVATION,
    STUB_WRITE_BARRIER,
    STUB_JIT_INLINE_CLOSURE_UPVALUES,
    STUB_STRICT_EQ_LEAF,
    STUB_JIT_LOOSE_EQ,
    STUB_TO_BOOLEAN_LEAF,
    STUB_NUMBER_REM_LEAF,
    STUB_JIT_LOAD_REGEXP,
    STUB_JIT_CONSTRUCT,
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
    STUB_JIT_SPREAD_CALL_OP,
    STUB_JIT_CLASS_VALUE_OP,
    STUB_JIT_MODULE_OP,
    STUB_JIT_FINISH_ERROR,
    STUB_JIT_DEOPT_WRITEBACK,
    STUB_JIT_DEOPT_STACK_CALL,
    STUB_JIT_RESOLVE_DIRECT_ENTRY,
    STUB_STRING_CHAR_CODE_AT_LEAF,
    STUB_STRING_CODE_POINT_AT_LEAF,
    STUB_STRING_INDEX_OF_LEAF,
    STUB_STRING_INCLUDES_LEAF,
    STUB_STRING_STARTS_WITH_LEAF,
    STUB_STRING_ENDS_WITH_LEAF,
    STUB_ARRAY_POP_LEAF,
    STUB_ARRAY_PUSH_ALLOC,
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
    STUB_JIT_PREPARE_BASE_CONSTRUCT,
    STUB_JIT_DERIVED_CONSTRUCT_RESULT,
    STUB_JIT_CLASS_SUPER_CONSTRUCTOR,
    STUB_JIT_COPY_SPREAD_ARGUMENTS,
    STUB_JIT_INITIALIZE_UPVALUES,
    STUB_JIT_TRY_PREPARE_BASE_CONSTRUCT,
    STUB_PARSE_INT_I32_LEAF,
    STUB_ARRAY_CONSTRUCT_ALLOC,
    STUB_JIT_CALL_METHOD_VALUE,
    STUB_JIT_ROUTE_THROW,
    STUB_JIT_ACKNOWLEDGE_CAUGHT_THROW,
    STUB_JIT_BINDING_VALUE,
    STUB_JIT_GLOBAL_DECLARATION_VALUE,
    STUB_JIT_CALL_WITH_THIS_VALUE,
    STUB_JIT_CONSTRUCT_VALUE,
    STUB_JIT_COLLECT_ARGUMENTS,
    STUB_JIT_CALL_FORWARD_ARGUMENTS,
    STUB_JIT_FORWARD_ARGUMENT_COUNT,
    STUB_JIT_COPY_FORWARDED_ARGUMENTS,
    STUB_JIT_FORWARD_CALL_PLAN,
    STUB_JIT_FORWARD_SOURCE_READY,
];

/// Validate a descriptor and one concrete call-site safepoint id.
#[must_use]
pub const fn validate_stub_descriptor(
    desc: RuntimeStubDescriptor,
    safepoint_id: SafepointId,
) -> bool {
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
        RuntimeStubSignature::AcknowledgeCaughtThrow0 | RuntimeStubSignature::Poll1 => matches!(
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
        }
        RuntimeStubSignature::Variadic => match (desc.result_abi, desc.result_domain) {
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
const _: [(); 9] = [(); std::mem::offset_of!(RuntimeStubDescriptor, result_abi)];
const _: [(); 10] = [(); std::mem::offset_of!(RuntimeStubDescriptor, result_domain)];
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
                STUB_JIT_MAKE_FN,
                STUB_JIT_MAKE_CLOSURE,
                STUB_JIT_FRESH_UPVALUE,
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
                STUB_JIT_CONSTRUCT,
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
                STUB_JIT_SPREAD_CALL_OP,
                STUB_JIT_CLASS_VALUE_OP,
                STUB_JIT_MODULE_OP,
            ],
            RuntimeStubException::Status,
        );
    }

    #[test]
    fn status_word_success_fatal_subset_is_explicit() {
        assert_status_words(
            &[
                STUB_JIT_PUSH_NATIVE_ACTIVATION,
                STUB_JIT_ACKNOWLEDGE_CAUGHT_THROW,
            ],
            RuntimeStubException::Never,
        );
    }

    #[test]
    fn status_word_success_only_subset_is_explicit() {
        assert_status_words(
            &[STUB_JIT_POP_NATIVE_ACTIVATION],
            RuntimeStubException::Never,
        );
    }

    #[test]
    fn context_word_descriptors_have_exact_fixed_arities() {
        for (descriptor, argument_count) in [
            (STUB_JIT_PREPARE_BASE_CONSTRUCT, 3),
            (STUB_JIT_DERIVED_CONSTRUCT_RESULT, 2),
            (STUB_JIT_INITIALIZE_UPVALUES, 3),
            (STUB_JIT_TRY_PREPARE_BASE_CONSTRUCT, 4),
        ] {
            assert_eq!(descriptor.signature, RuntimeStubSignature::ContextWords);
            assert_eq!(descriptor.argument_count, argument_count);
        }
    }

    #[test]
    fn element_entries_are_fixed_value_reentrant_committed_pairs() {
        assert_eq!(STUB_JIT_LOAD_ELEMENT.id, 14);
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

        assert_eq!(STUB_JIT_STORE_ELEMENT.id, 15);
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
        assert_eq!(STUB_JIT_LOAD_PROPERTY.id, 17);
        assert_eq!(
            STUB_JIT_LOAD_PROPERTY.signature,
            RuntimeStubSignature::ReentrantNamedLoad
        );
        assert_eq!(STUB_JIT_LOAD_PROPERTY.argument_count, 1);
        assert_eq!(runtime_stub_name(17), "jit_load_property_value");

        assert_eq!(STUB_JIT_STORE_PROPERTY.id, 18);
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
        assert_eq!(STUB_JIT_OBJECT_PROTOCOL_VALUE.id, 41);
        assert_eq!(
            runtime_stub_name(STUB_JIT_OBJECT_PROTOCOL_VALUE.id),
            "jit_object_protocol_value"
        );
        assert_eq!(STUB_JIT_SCALAR_VALUE.id, 43);
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
        assert_eq!(STUB_JIT_ROUTE_THROW.id, 87);
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
    fn caught_throw_acknowledgement_is_an_exceptional_only_leaf() {
        assert_eq!(STUB_JIT_ACKNOWLEDGE_CAUGHT_THROW.id, 88);
        assert_eq!(
            STUB_JIT_ACKNOWLEDGE_CAUGHT_THROW.signature,
            RuntimeStubSignature::AcknowledgeCaughtThrow0
        );
        assert_eq!(STUB_JIT_ACKNOWLEDGE_CAUGHT_THROW.argument_count, 0);
        assert_eq!(
            STUB_JIT_ACKNOWLEDGE_CAUGHT_THROW.exception,
            RuntimeStubException::Never
        );
        assert!(validate_stub_descriptor(
            STUB_JIT_ACKNOWLEDGE_CAUGHT_THROW,
            NO_SAFEPOINT
        ));
    }

    #[test]
    fn collect_arguments_entry_is_an_allocating_status_word_transition() {
        assert_eq!(STUB_JIT_COLLECT_ARGUMENTS.id, 93);
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
    fn call_forward_arguments_entry_is_a_committed_value_span() {
        assert_eq!(STUB_JIT_CALL_FORWARD_ARGUMENTS.id, 94);
        assert_eq!(
            STUB_JIT_CALL_FORWARD_ARGUMENTS.class,
            RuntimeStubClass::Reentrant
        );
        assert_eq!(
            STUB_JIT_CALL_FORWARD_ARGUMENTS.result_abi,
            RuntimeStubResultAbi::NativePair
        );
        assert_eq!(
            STUB_JIT_CALL_FORWARD_ARGUMENTS.signature,
            RuntimeStubSignature::ReentrantValueSpan
        );
        assert_eq!(
            STUB_JIT_CALL_FORWARD_ARGUMENTS.result_domain,
            NativeResultDomain::Committed
        );
        assert_eq!(
            runtime_stub_name(STUB_JIT_CALL_FORWARD_ARGUMENTS.id),
            "jit_call_forward_arguments"
        );
    }

    #[test]
    fn construct_entry_is_reentrant_value_span_committed_pair() {
        assert_eq!(STUB_JIT_CONSTRUCT_VALUE.id, 92);
        assert_eq!(
            STUB_JIT_CONSTRUCT_VALUE.signature,
            RuntimeStubSignature::ReentrantValueSpan
        );
        assert_eq!(
            runtime_stub_name(STUB_JIT_CONSTRUCT_VALUE.id),
            "jit_construct_value"
        );
        assert_eq!(
            STUB_JIT_CONSTRUCT_VALUE.result_domain,
            NativeResultDomain::Committed
        );
    }

    #[test]
    fn call_with_this_entry_is_reentrant_value_span_committed_pair() {
        assert_eq!(STUB_JIT_CALL_WITH_THIS_VALUE.id, 91);
        assert_eq!(
            STUB_JIT_CALL_WITH_THIS_VALUE.signature,
            RuntimeStubSignature::ReentrantValueSpan
        );
        assert_eq!(
            runtime_stub_name(STUB_JIT_CALL_WITH_THIS_VALUE.id),
            "jit_call_with_this_value"
        );
        assert_eq!(
            STUB_JIT_CALL_WITH_THIS_VALUE.result_abi,
            RuntimeStubResultAbi::NativePair
        );
        assert_eq!(
            STUB_JIT_CALL_WITH_THIS_VALUE.result_domain,
            NativeResultDomain::Committed
        );
    }

    #[test]
    fn method_call_entry_is_reentrant_value_span_committed_pair() {
        assert_eq!(STUB_JIT_CALL_METHOD_VALUE.id, 86);
        assert_eq!(
            STUB_JIT_CALL_METHOD_VALUE.signature,
            RuntimeStubSignature::ReentrantValueSpan
        );
        assert_eq!(
            STUB_JIT_CALL_METHOD_VALUE.argument_count,
            VARIADIC_STUB_ARGUMENTS
        );
        assert_eq!(
            runtime_stub_name(STUB_JIT_CALL_METHOD_VALUE.id),
            "jit_call_method_value"
        );
        assert_eq!(
            STUB_JIT_CALL_METHOD_VALUE.class,
            RuntimeStubClass::Reentrant
        );
        assert_eq!(
            STUB_JIT_CALL_METHOD_VALUE.safepoint,
            RuntimeStubSafepoint::Required
        );
        assert_eq!(
            STUB_JIT_CALL_METHOD_VALUE.exception,
            RuntimeStubException::Status
        );
        assert_eq!(
            STUB_JIT_CALL_METHOD_VALUE.result_abi,
            RuntimeStubResultAbi::NativePair
        );
        assert_eq!(
            STUB_JIT_CALL_METHOD_VALUE.result_domain,
            NativeResultDomain::Committed
        );
        assert!(STUB_JIT_CALL_METHOD_VALUE.effects.contains(
            RuntimeStubEffects::MAY_ALLOCATE
                | RuntimeStubEffects::MAY_TRIGGER_GC
                | RuntimeStubEffects::MAY_THROW
                | RuntimeStubEffects::MAY_REENTER_JS
                | RuntimeStubEffects::MAY_MUTATE_GC
        ));
        assert!(validate_stub_descriptor(STUB_JIT_CALL_METHOD_VALUE, 0));
        assert!(!validate_stub_descriptor(
            STUB_JIT_CALL_METHOD_VALUE,
            NO_SAFEPOINT
        ));
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
