//! Classified machine-callable runtime-stub contracts.
//!
//! # Contents
//! - [`RuntimeStubDescriptor`] declares signature, effects, safepoint,
//!   exception, and result ABI for every dense [`RuntimeStubId`].
//! - [`RuntimeStubAllocContext`] is the temporary rooted call packet used by
//!   the current allocating entries while frame publication converges.
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

use super::{NO_SAFEPOINT, NativeFrame, SafepointId, VmThread};

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
    /// Nullary value producer (`fn() -> value bits`).
    NullaryValue = 4,
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
    /// [`super::RuntimeStubResultPair`] two-register encoding.
    StatusPair = 0,
    /// Single status word; any value result is written through the call
    /// packet or the published frame before the stub returns.
    StatusWord = 1,
    /// Single raw value word with no status channel.
    ValueWord = 2,
}

/// Machine-callable runtime-stub descriptor.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeStubDescriptor {
    /// Dense stable descriptor id.
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
);
/// Leaf `Map.prototype.get` probe.
pub const STUB_COLLECTION_MAP_GET_LEAF: RuntimeStubDescriptor = descriptor(
    2,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::StatusPair,
);
/// Leaf `Map.prototype.has` probe.
pub const STUB_COLLECTION_MAP_HAS_LEAF: RuntimeStubDescriptor = descriptor(
    3,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::StatusPair,
);
/// Leaf `Set.prototype.has` probe.
pub const STUB_COLLECTION_SET_HAS_LEAF: RuntimeStubDescriptor = descriptor(
    4,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::StatusPair,
);
/// Allocating `Map.prototype.set` mutation.
pub const STUB_COLLECTION_MAP_SET_ALLOC: RuntimeStubDescriptor = descriptor(
    5,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusPair,
);
/// Allocating `Set.prototype.add` mutation.
pub const STUB_COLLECTION_SET_ADD_ALLOC: RuntimeStubDescriptor = descriptor(
    6,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusPair,
);
/// Allocating `Map.prototype.get` lookup.
pub const STUB_COLLECTION_MAP_GET_ALLOC: RuntimeStubDescriptor = descriptor(
    7,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusPair,
);
/// Allocating `Map.prototype.has` lookup.
pub const STUB_COLLECTION_MAP_HAS_ALLOC: RuntimeStubDescriptor = descriptor(
    8,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusPair,
);
/// Allocating `Set.prototype.has` lookup.
pub const STUB_COLLECTION_SET_HAS_ALLOC: RuntimeStubDescriptor = descriptor(
    9,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusPair,
);
/// Allocating `Map.prototype.delete` mutation.
pub const STUB_COLLECTION_MAP_DELETE_ALLOC: RuntimeStubDescriptor = descriptor(
    10,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusPair,
);
/// Allocating `Set.prototype.delete` mutation.
pub const STUB_COLLECTION_SET_DELETE_ALLOC: RuntimeStubDescriptor = descriptor(
    11,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusPair,
);
/// Allocating primitive string-concat operation.
pub const STUB_STRING_CONCAT_ALLOC: RuntimeStubDescriptor = descriptor(
    12,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusPair,
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
);
/// Generic negate slow path; `ToNumber` may re-enter JS.
pub const STUB_JIT_NEG: RuntimeStubDescriptor = descriptor(
    14,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// `Math.*` builtin call; argument coercion may re-enter JS.
pub const STUB_JIT_MATH_CALL: RuntimeStubDescriptor = descriptor(
    15,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Global read; global accessors may re-enter JS.
pub const STUB_JIT_LOAD_GLOBAL: RuntimeStubDescriptor = descriptor(
    16,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Computed element read; getters/proxies may re-enter JS.
pub const STUB_JIT_LOAD_ELEMENT: RuntimeStubDescriptor = descriptor(
    17,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Computed element write; setters/proxies may re-enter JS.
pub const STUB_JIT_STORE_ELEMENT: RuntimeStubDescriptor = descriptor(
    18,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Descriptor-driven define; descriptor reads may re-enter JS.
pub const STUB_JIT_DEFINE_OWN_PROPERTY: RuntimeStubDescriptor = descriptor(
    19,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Collection method IC probe and refresh.
pub const STUB_JIT_COLLECTION_METHOD_IC: RuntimeStubDescriptor = descriptor(
    20,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Copies a live bailed native frame, adopts its owner, and runs it to completion.
pub const STUB_JIT_FINISH_DIRECT_CALL_BAILED: RuntimeStubDescriptor = descriptor(
    21,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Cold-materializes a frameless self-recursive deopt and runs it to
/// completion. Normal tier switches retain the canonical native activation.
pub const STUB_JIT_DEOPT_MATERIALIZE_SELF_CALL: RuntimeStubDescriptor = descriptor(
    22,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusPair,
);
/// Named-property IC miss handler over the canonical activation.
pub const STUB_JIT_LOAD_PROPERTY: RuntimeStubDescriptor = descriptor(
    23,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Named-property store miss handler; shape transitions allocate.
pub const STUB_JIT_STORE_PROPERTY: RuntimeStubDescriptor = descriptor(
    24,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Plain data-property define.
pub const STUB_JIT_DEFINE_DATA_PROPERTY: RuntimeStubDescriptor = descriptor(
    25,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// String-constant materialization.
pub const STUB_JIT_LOAD_STRING: RuntimeStubDescriptor = descriptor(
    26,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Builtin error-constructor load.
pub const STUB_JIT_LOAD_BUILTIN_ERROR: RuntimeStubDescriptor = descriptor(
    27,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// `MakeFunction` closure construction.
pub const STUB_JIT_MAKE_FN: RuntimeStubDescriptor = descriptor(
    28,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// `MakeClosure` construction with captured parent upvalues.
pub const STUB_JIT_MAKE_CLOSURE: RuntimeStubDescriptor = descriptor(
    29,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Ordinary object allocation.
pub const STUB_JIT_NEW_OBJECT: RuntimeStubDescriptor = descriptor(
    30,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Array literal allocation.
pub const STUB_JIT_NEW_ARRAY: RuntimeStubDescriptor = descriptor(
    31,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Fresh loop-iteration upvalue cell allocation.
pub const STUB_JIT_FRESH_UPVALUE: RuntimeStubDescriptor = descriptor(
    32,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Publishes an eligible plain-call callee frame.
pub const STUB_JIT_PREPARE_DIRECT_CALL: RuntimeStubDescriptor = descriptor(
    33,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Publishes an eligible method-call callee frame.
pub const STUB_JIT_PREPARE_DIRECT_METHOD_CALL: RuntimeStubDescriptor = descriptor(
    34,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Registers a native activation's scalar root slots.
pub const STUB_JIT_PUSH_NATIVE_ACTIVATION: RuntimeStubDescriptor = descriptor(
    35,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::leaf(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Releases the topmost native activation registration.
pub const STUB_JIT_POP_NATIVE_ACTIVATION: RuntimeStubDescriptor = descriptor(
    36,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::leaf(false, false),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::StatusWord,
);
/// Releases a prepared direct-call owner after throw or rejected native entry.
pub const STUB_JIT_ABORT_DIRECT_CALL: RuntimeStubDescriptor = descriptor(
    37,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::leaf(false, false),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::StatusWord,
);
/// Releases a returned direct-call owner and stores its value in the caller.
pub const STUB_JIT_FINISH_DIRECT_CALL_RETURNED: RuntimeStubDescriptor = descriptor(
    38,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::leaf(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Captured-binding read; TDZ reads throw.
pub const STUB_JIT_LOAD_UPVALUE: RuntimeStubDescriptor = descriptor(
    39,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::leaf(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Captured-binding write with barrier.
pub const STUB_JIT_STORE_UPVALUE: RuntimeStubDescriptor = descriptor(
    40,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::leaf(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// TDZ-checked captured-binding write with barrier.
pub const STUB_JIT_STORE_UPVALUE_CHECKED: RuntimeStubDescriptor = descriptor(
    41,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::leaf(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Generational write barrier for an inline pointer store.
pub const STUB_JIT_WRITE_BARRIER: RuntimeStubDescriptor = descriptor(
    42,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::leaf(false, true),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::StatusWord,
);
/// Frameless write barrier over the register window.
pub const STUB_JIT_WRITE_BARRIER_WINDOW: RuntimeStubDescriptor = descriptor(
    43,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::leaf(false, true),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::StatusWord,
);
/// Validates an inline-call closure and returns its upvalue base.
pub const STUB_JIT_INLINE_CLOSURE_UPVALUES: RuntimeStubDescriptor = descriptor(
    44,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::leaf(false, false),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::ValueWord,
);
/// `Math.random` value producer.
pub const STUB_JIT_MATH_RANDOM: RuntimeStubDescriptor = descriptor(
    45,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::NullaryValue,
    0,
    RuntimeStubEffects::leaf(false, false),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::ValueWord,
);
/// Completes one full `CallMethodValue` in the VM for site resolutions the
/// direct-call prepare cannot stage (polymorphic, native, accessor, or cold
/// methods); the resolved callee may be arbitrary JS, and post-resolution
/// missing/non-callable results throw without replaying observable `[[Get]]`.
pub const STUB_JIT_CALL_METHOD_GENERIC: RuntimeStubDescriptor = descriptor(
    46,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Completes one full plain `Call` in the VM for callee resolutions the
/// direct-call prepare cannot stage (native, bound, or ineligible bytecode
/// callables); the callee may be arbitrary JS.
pub const STUB_JIT_CALL_GENERIC: RuntimeStubDescriptor = descriptor(
    47,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Leaf §7.2.15 IsStrictlyEqual probe over two raw operand words: never
/// throws, never allocates; a null heap reports a miss so probe harnesses
/// without a live isolate fall back to normal dispatch.
pub const STUB_STRICT_EQ_LEAF: RuntimeStubDescriptor = descriptor(
    48,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::StatusPair,
);
/// Completes one full loose-equality opcode in the VM; object-to-primitive
/// coercion may re-enter JS.
pub const STUB_JIT_LOOSE_EQ: RuntimeStubDescriptor = descriptor(
    49,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Materializes a regex literal from the constant pool; allocates the
/// RegExp body and may compile the pattern.
pub const STUB_JIT_LOAD_REGEXP: RuntimeStubDescriptor = descriptor(
    52,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);
/// Leaf §7.1.2 ToBoolean probe over one raw operand word (the second
/// argument is ignored): never throws, never allocates; total for every
/// value including heap cells, so it never misses on a live isolate.
pub const STUB_TO_BOOLEAN_LEAF: RuntimeStubDescriptor = descriptor(
    50,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::StatusPair,
);
/// Leaf numeric remainder over two raw operand words already known to be
/// numbers: full f64 remainder semantics (sign of the dividend, NaN for a
/// zero divisor), boxed without allocation.
pub const STUB_NUMBER_REM_LEAF: RuntimeStubDescriptor = descriptor(
    51,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::StatusPair,
);

/// Completes one full `Op::New` construct in the VM for a New site outside
/// the compiled subset; the constructor body may run arbitrary JS.
pub const STUB_JIT_CONSTRUCT: RuntimeStubDescriptor = descriptor(
    53,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Completes one coercive `ToPrimitive` or `ToNumeric` opcode; user conversion
/// hooks may allocate, throw, and re-enter arbitrary JS.
pub const STUB_JIT_COERCE_UNARY: RuntimeStubDescriptor = descriptor(
    54,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Completes one numeric, bitwise, update, or relational opcode in the VM.
/// The shared family is conservatively reentrant because `Increment` may run
/// user conversion hooks and BigInt results may allocate.
pub const STUB_JIT_NUMERIC_OP: RuntimeStubDescriptor = descriptor(
    55,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Completes structured exception-region state changes, abrupt unwinds, and
/// TDZ `ReferenceError` materialization.
pub const STUB_JIT_EXCEPTION_OP: RuntimeStubDescriptor = descriptor(
    56,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusPair,
);

/// Completes iterator stepping, iterator close, and closer-registry state
/// through the VM's full iterator semantics.
pub const STUB_JIT_ITERATOR_OP: RuntimeStubDescriptor = descriptor(
    57,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Completes `Function.prototype.bind` — accessor `name`/`length` getters and
/// bound-function allocation — through the VM's full bind semantics.
pub const STUB_JIT_BIND_FUNCTION: RuntimeStubDescriptor = descriptor(
    58,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Completes global-variable reads and writes — including accessor globals —
/// through the VM's global environment-record helpers.
pub const STUB_JIT_GLOBAL_OP: RuntimeStubDescriptor = descriptor(
    59,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Completes object property-protocol queries (`instanceof`, `in`,
/// `[[GetPrototypeOf]]`, `[[SetPrototypeOf]]`) — including Proxy traps —
/// through the VM's Proxy-aware drivers and fast paths.
pub const STUB_JIT_OBJECT_PROTOCOL_OP: RuntimeStubDescriptor = descriptor(
    60,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Completes `delete` (`DeleteProperty`, `DeleteElement`, `DeleteDynamic`) —
/// including the Proxy `deleteProperty` trap and unqualified delete — through
/// the VM's delete drivers and fast paths.
pub const STUB_JIT_DELETE_OP: RuntimeStubDescriptor = descriptor(
    61,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Completes scalar value-query and coercion opcodes (`ToObject`,
/// `ToPropertyKey`, `TypeOf`, `LoadNewTarget`, `SameValue`, `IsArray`,
/// `ArrayLength`, `LoadLength`) through the VM's register helpers.
pub const STUB_JIT_SCALAR_OP: RuntimeStubDescriptor = descriptor(
    62,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Completes `super` property reads and writes (`LoadSuperProperty`,
/// `LoadSuperElement`, `SetSuperProperty`, `SetSuperElement`) — including home-
/// prototype accessor getters/setters — through the VM's super helpers.
pub const STUB_JIT_SUPER_OP: RuntimeStubDescriptor = descriptor(
    63,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Completes private-member access (`PrivateGet`, `PrivateSet`,
/// `PrivateBrandCheck`) — including private accessor getters/setters — through
/// the VM's private-element helpers.
pub const STUB_JIT_PRIVATE_OP: RuntimeStubDescriptor = descriptor(
    64,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Completes static value loads (`MathLoad`, `SymbolLoad`, `TemporalLoad`,
/// `LoadBigInt`, `GetStringIndex`) through the VM's load helpers.
pub const STUB_JIT_VALUE_LOAD_OP: RuntimeStubDescriptor = descriptor(
    65,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Completes allocating construction opcodes (`CollectRest`, `NewError`,
/// `NewBuiltinError`, `ArrayPush`) through the VM's construction helpers.
pub const STUB_JIT_CONSTRUCT_OP: RuntimeStubDescriptor = descriptor(
    66,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Completes structural object opcodes (`ForInKeys`, `CopyDataProperties`)
/// through the VM's structural helpers.
pub const STUB_JIT_STRUCTURAL_OP: RuntimeStubDescriptor = descriptor(
    67,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Completes class-construction opcodes (`BindThisValue`, `ClassCheck`,
/// `SetFunctionName`) through the VM's class helpers.
pub const STUB_JIT_CLASS_OP: RuntimeStubDescriptor = descriptor(
    68,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Completes variadic construction opcodes (`ArrayConstruct`, `ArrayFrom`,
/// `ArrayOf`, `QueueMicrotask`) through the VM's variadic helpers.
pub const STUB_JIT_VARIADIC_OP: RuntimeStubDescriptor = descriptor(
    69,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Completes static intrinsic-call opcodes (`ArrayBufferCall`,
/// `SharedArrayBufferCall`, `BigIntCall`, `DataViewCall`) through the VM's
/// static-call helpers, rebuilding their method-id operand layout.
pub const STUB_JIT_STATIC_CALL_OP: RuntimeStubDescriptor = descriptor(
    70,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Completes dynamic control-family reads (`LoadShadowedUpvalue`) through the
/// VM's shared dynamic-environment/upvalue helper.
pub const STUB_JIT_CONTROL_OP: RuntimeStubDescriptor = descriptor(
    71,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Completes spread calls/constructions, explicit-receiver calls, and
/// `CollectArguments` through the VM's synchronous call helpers. `TailCall`
/// is excluded: its interpreter path reuses the caller frame for true tail
/// recursion, so it stays an exact side exit rather than a nested call.
pub const STUB_JIT_SPREAD_CALL_OP: RuntimeStubDescriptor = descriptor(
    72,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Completes class creation, dynamic source evaluation, private-name/template
/// materialization, eval identity, and full `ToNumber` coercion through shared
/// VM helpers.
pub const STUB_JIT_CLASS_VALUE_OP: RuntimeStubDescriptor = descriptor(
    73,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Completes synchronous static-module namespace/binding operations,
/// star re-export, module-record marking, and `import.meta.resolve` through
/// shared VM helpers. Promise-producing module operations remain side exits.
pub const STUB_JIT_MODULE_OP: RuntimeStubDescriptor = descriptor(
    74,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Shared throw-epilogue resolver. Delivers a value transition's parked error to
/// the current compiled frame's own structured-exception handlers before the
/// throw propagates, so a `try` in the same compiled function catches a
/// property/element/global/loose-equality/coercion throw. Reports
/// [`crate::native_abi::runtime_stubs`] status `1` (bailed to the published
/// catch/finally PC) or `2` (re-parked, propagate).
pub const STUB_JIT_RESOLVE_THREW: RuntimeStubDescriptor = descriptor(
    75,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Rebuild the interpreter frame of an inlined callee at a deopt exit.
///
/// Optimized code that splices a callee body owes the interpreter that callee's
/// frame when it exits inside it. Rather than reproduce the call's frame setup
/// — upvalue spine, `this`, argument binding — the stub rewinds the already
/// written-back caller to its call and lets the interpreter's own call path
/// build the frame, so the result is exactly the frame a real call would have
/// produced. The emitted code then fast-forwards that frame's registers and PC
/// to where the optimized code actually was.
///
/// Returns the new frame's register-window pointer, or `0` when the call path
/// raised (a stack overflow the interpreter would also have raised).
pub const STUB_JIT_DEOPT_REIFY_FRAME: RuntimeStubDescriptor = descriptor(
    76,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusWord,
);

/// Human-readable symbol for a stable runtime-stub id.
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
        14 => "jit_neg",
        15 => "jit_math_call",
        16 => "jit_load_global",
        17 => "jit_load_element",
        18 => "jit_store_element",
        19 => "jit_define_own_property",
        20 => "jit_call_collection_method_ic",
        21 => "jit_finish_direct_call_bailed",
        22 => "jit_self_call_bail",
        23 => "jit_load_prop_window",
        24 => "jit_store_prop_window",
        25 => "jit_define_data_property",
        26 => "jit_load_string",
        27 => "jit_load_builtin_error",
        28 => "jit_make_fn",
        29 => "jit_make_closure",
        30 => "jit_new_object",
        31 => "jit_new_array",
        32 => "jit_fresh_upvalue",
        33 => "jit_prepare_direct_call",
        34 => "jit_prepare_direct_method_call",
        35 => "jit_push_native_activation",
        36 => "jit_pop_native_activation",
        37 => "jit_abort_direct_call",
        38 => "jit_finish_direct_call_returned",
        39 => "jit_load_upvalue",
        40 => "jit_store_upvalue",
        41 => "jit_store_upvalue_checked",
        42 => "jit_write_barrier",
        43 => "jit_write_barrier_window",
        44 => "jit_inline_closure_upvalues",
        45 => "jit_math_random",
        46 => "jit_call_method_generic",
        47 => "jit_call_generic",
        48 => "strict_eq_leaf",
        49 => "jit_loose_eq",
        50 => "to_boolean_leaf",
        51 => "number_rem_leaf",
        52 => "jit_load_regexp",
        53 => "jit_construct",
        54 => "jit_coerce_unary",
        55 => "jit_numeric_op",
        56 => "jit_exception_op",
        57 => "jit_iterator_op",
        58 => "jit_bind_function",
        59 => "jit_global_op",
        60 => "jit_object_protocol_op",
        61 => "jit_delete_op",
        62 => "jit_scalar_op",
        63 => "jit_super_op",
        64 => "jit_private_op",
        65 => "jit_value_load_op",
        66 => "jit_construct_op",
        67 => "jit_structural_op",
        68 => "jit_class_op",
        69 => "jit_variadic_op",
        70 => "jit_static_call_op",
        71 => "jit_control_op",
        72 => "jit_spread_call_op",
        73 => "jit_class_value_op",
        74 => "jit_module_op",
        75 => "jit_resolve_threw",
        76 => "jit_deopt_reify_frame",
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
    STUB_JIT_NEG,
    STUB_JIT_MATH_CALL,
    STUB_JIT_LOAD_GLOBAL,
    STUB_JIT_LOAD_ELEMENT,
    STUB_JIT_STORE_ELEMENT,
    STUB_JIT_DEFINE_OWN_PROPERTY,
    STUB_JIT_COLLECTION_METHOD_IC,
    STUB_JIT_FINISH_DIRECT_CALL_BAILED,
    STUB_JIT_DEOPT_MATERIALIZE_SELF_CALL,
    STUB_JIT_LOAD_PROPERTY,
    STUB_JIT_STORE_PROPERTY,
    STUB_JIT_DEFINE_DATA_PROPERTY,
    STUB_JIT_LOAD_STRING,
    STUB_JIT_LOAD_BUILTIN_ERROR,
    STUB_JIT_MAKE_FN,
    STUB_JIT_MAKE_CLOSURE,
    STUB_JIT_NEW_OBJECT,
    STUB_JIT_NEW_ARRAY,
    STUB_JIT_FRESH_UPVALUE,
    STUB_JIT_PREPARE_DIRECT_CALL,
    STUB_JIT_PREPARE_DIRECT_METHOD_CALL,
    STUB_JIT_PUSH_NATIVE_ACTIVATION,
    STUB_JIT_POP_NATIVE_ACTIVATION,
    STUB_JIT_ABORT_DIRECT_CALL,
    STUB_JIT_FINISH_DIRECT_CALL_RETURNED,
    STUB_JIT_LOAD_UPVALUE,
    STUB_JIT_STORE_UPVALUE,
    STUB_JIT_STORE_UPVALUE_CHECKED,
    STUB_JIT_WRITE_BARRIER,
    STUB_JIT_WRITE_BARRIER_WINDOW,
    STUB_JIT_INLINE_CLOSURE_UPVALUES,
    STUB_JIT_MATH_RANDOM,
    STUB_JIT_CALL_METHOD_GENERIC,
    STUB_JIT_CALL_GENERIC,
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
    STUB_JIT_GLOBAL_OP,
    STUB_JIT_OBJECT_PROTOCOL_OP,
    STUB_JIT_DELETE_OP,
    STUB_JIT_SCALAR_OP,
    STUB_JIT_SUPER_OP,
    STUB_JIT_PRIVATE_OP,
    STUB_JIT_VALUE_LOAD_OP,
    STUB_JIT_CONSTRUCT_OP,
    STUB_JIT_STRUCTURAL_OP,
    STUB_JIT_CLASS_OP,
    STUB_JIT_VARIADIC_OP,
    STUB_JIT_STATIC_CALL_OP,
    STUB_JIT_CONTROL_OP,
    STUB_JIT_SPREAD_CALL_OP,
    STUB_JIT_CLASS_VALUE_OP,
    STUB_JIT_MODULE_OP,
    STUB_JIT_RESOLVE_THREW,
    STUB_JIT_DEOPT_REIFY_FRAME,
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
    let result_matches = match desc.signature {
        RuntimeStubSignature::LeafValue2 | RuntimeStubSignature::AllocValue3 => {
            matches!(desc.result_abi, RuntimeStubResultAbi::StatusPair)
        }
        RuntimeStubSignature::Poll1 => {
            matches!(desc.result_abi, RuntimeStubResultAbi::StatusWord)
        }
        RuntimeStubSignature::Variadic => !matches!(
            (desc.exception, desc.result_abi),
            (
                RuntimeStubException::Status,
                RuntimeStubResultAbi::ValueWord
            )
        ),
        RuntimeStubSignature::NullaryValue => {
            matches!(desc.result_abi, RuntimeStubResultAbi::ValueWord)
                && matches!(desc.exception, RuntimeStubException::Never)
        }
    };
    if !throwing_matches || !result_matches {
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

const _: [(); 12] = [(); std::mem::size_of::<RuntimeStubDescriptor>()];
const _: [(); 4] = [(); std::mem::align_of::<RuntimeStubDescriptor>()];
const _: [(); 0] = [(); std::mem::offset_of!(RuntimeStubDescriptor, id)];
const _: [(); 10] = [(); std::mem::offset_of!(RuntimeStubDescriptor, effects)];
const _: [(); 24] = [(); std::mem::size_of::<RuntimeStubAllocContext>()];
const _: [(); 8] = [(); std::mem::offset_of!(RuntimeStubAllocContext, spill_slots)];
const _: [(); 16] = [(); std::mem::offset_of!(RuntimeStubAllocContext, safepoint_id)];

#[cfg(test)]
mod tests {
    use super::*;

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
}
