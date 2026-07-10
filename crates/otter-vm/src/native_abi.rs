//! Native execution ABI metadata shared by interpreter, JIT, runtime stubs, and
//! GC safepoints.
//!
//! This module is the passive source of truth for the VM-native ABI target. It
//! deliberately carries no execution policy yet: current interpreter/JIT paths
//! can adopt these records incrementally while keeping the existing correctness
//! surface intact.
//!
//! # Contents
//! - [`VmThread`], [`NativeFrame`], and [`NativeFrameHeader`] describe the
//!   machine-observed execution state every tier must converge on.
//! - [`RuntimeStubClass`], [`RuntimeStubStatus`], and [`RuntimeStubResult`]
//!   describe machine-callable runtime stubs without the generic `NativeCtx`
//!   boundary.
//! - [`SafepointEntry`], [`FrameMap`], [`SpillMap`], [`SafepointRecord`], and
//!   [`TaggedLocation`] describe tagged `Value` liveness for moving-GC
//!   safepoints and deopt exits.
//! - [`CodeDependency`] and the ABI/build versions make code validity explicit.
//!
//! # Invariants
//! - Machine-observed records are C-layout and contain only fixed-width fields;
//!   addresses are encoded as `u64` and must be zero-extended canonical target
//!   addresses on supported 64-bit hosts.
//! - Allocating or re-entrant stubs must name a safepoint record; leaf stubs must
//!   not allocate, throw, re-enter JS, or trigger GC.
//! - Safepoint locations describe only tagged GC-visible values. Unboxed machine
//!   values belong to deopt frame-state metadata, not GC root maps.
//! - Installed code is invalidated by epoch/version mismatch, unlinked before it
//!   is retired, and retained until no active frame/code anchor can return to it.
//!
//! # See also
//! - [`crate::jit`] for the current type-erased JIT entry surface.
//! - [`crate::frame_state`] for the current interpreter frame.
//! - `JIT_REFACTOR_PLAN.md` for the refactor roadmap.

/// Native VM ABI layout version. Increment for any machine-observed field,
/// offset, enum discriminant, or calling-convention change.
pub const VM_LAYOUT_VERSION: u32 = 1;

/// Runtime-stub table version. Increment when stable stub ids, signatures,
/// classes, or effect contracts change.
pub const RUNTIME_STUB_TABLE_VERSION: u32 = 1;

/// Build identity folded into transient compiled-code/cache validity.
///
/// This is intentionally numeric and reproducible. Persistent CodeBlocks will
/// replace it with a content/build hash in Phase 2.
pub const VM_BUILD_VERSION: u64 = 0x4f54_5445_525f_0001;

/// Dense identifier for one native ABI frame-state snapshot.
pub type FrameStateId = u32;

/// Dense identifier for one GC/deopt safepoint record.
pub type SafepointId = u32;

/// Dense identifier for one runtime stub descriptor.
pub type RuntimeStubId = u32;

/// Sentinel for call sites that cannot allocate and therefore have no safepoint.
pub const NO_SAFEPOINT: SafepointId = u32::MAX;

/// Sentinel for guards/calls that cannot deopt.
pub const NO_FRAME_STATE: FrameStateId = u32::MAX;

/// Runtime stub descriptor argument count for variadic call shapes.
pub const VARIADIC_STUB_ARGUMENTS: u8 = u8::MAX;

/// Stable VM-thread fields visible to native code.
///
/// This is a passive ABI shell, not the current Rust [`crate::Interpreter`].
/// Phase 2 will make the interpreter own/populate one shell rather than expose
/// its implementation layout to generated code.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmThread {
    /// Address of the currently published [`NativeFrame`], or zero.
    pub current_frame: u64,
    /// Opaque stable heap/isolate context address.
    pub heap_context: u64,
    /// Address of the process-local runtime stub entry table.
    pub runtime_stub_table: u64,
    /// Address of the interrupt/budget cell.
    pub interrupt_cell: u64,
    /// Rooted pending exception as boxed `Value` bits, or zero when absent.
    pub pending_exception_bits: u64,
    /// Current code invalidation epoch.
    pub code_epoch: u64,
    /// [`VM_LAYOUT_VERSION`] observed by entered code.
    pub layout_version: u32,
    /// [`RUNTIME_STUB_TABLE_VERSION`] observed by entered code.
    pub stub_table_version: u32,
}

impl VmThread {
    /// Empty passive shell with current ABI versions.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            current_frame: 0,
            heap_context: 0,
            runtime_stub_table: 0,
            interrupt_cell: 0,
            pending_exception_bits: 0,
            code_epoch: 0,
            layout_version: VM_LAYOUT_VERSION,
            stub_table_version: RUNTIME_STUB_TABLE_VERSION,
        }
    }
}

/// Execution tier that owns a frame.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeFrameKind {
    /// Current bytecode interpreter frame.
    Interpreter = 0,
    /// Low-latency baseline compiled frame.
    Baseline = 1,
    /// Speculative optimizing-tier frame.
    Optimized = 2,
    /// Runtime stub frame that is visible to GC/profiling while active.
    RuntimeStub = 3,
}

/// Bitflags attached to a native JS frame header.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeFrameFlags(u32);

impl NativeFrameFlags {
    /// Frame is an OSR entry rather than a function-entry frame.
    pub const OSR_ENTRY: u32 = 1 << 0;
    /// Frame has exact deopt metadata.
    pub const HAS_DEOPT: u32 = 1 << 1;
    /// Frame has precise safepoint maps for tagged machine locations.
    pub const HAS_SAFEPOINTS: u32 = 1 << 2;
    /// Frame may call back into JS from a runtime stub.
    pub const MAY_REENTER_JS: u32 = 1 << 3;

    /// Empty flag set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Build from raw bits.
    #[must_use]
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// Raw flag bits.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether all `mask` bits are present.
    #[must_use]
    pub const fn contains(self, mask: u32) -> bool {
        (self.0 & mask) == mask
    }
}

/// Fixed header shape every JS execution tier should become able to describe.
///
/// This is metadata, not yet the in-memory interpreter frame. It is intentionally
/// C-layout-compatible so generated code, runtime stubs, and diagnostic tooling
/// can share offsets once the active frame layout migrates.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeFrameHeader {
    /// Previous frame link or `0` at the stack root.
    pub previous_frame: u64,
    /// Global VM function id.
    pub function_id: u32,
    /// Linked code-block/chunk id.
    pub code_block_id: u32,
    /// Resume bytecode PC or instruction-index token, depending on frame flags.
    pub resume_pc: u32,
    /// Execution tier owning the frame.
    pub kind: NativeFrameKind,
    /// Frame flags.
    pub flags: NativeFrameFlags,
    /// Number of tagged interpreter-visible register slots.
    pub register_count: u16,
    /// Number of argument registers/slots.
    pub argument_count: u16,
    /// Feedback vector base/index for IC and type-feedback metadata.
    pub feedback_index: u32,
}

/// Authoritative machine-observed synchronous frame shell.
///
/// Register and feedback storage are referenced by stable base addresses. No
/// pointer into either storage may survive an allocating/reentrant safepoint;
/// generated code retains the tagged slot and recomputes derived pointers.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeFrame {
    /// Common tier-independent header.
    pub header: NativeFrameHeader,
    /// Base address of initialized tagged register slots.
    pub register_base: u64,
    /// Base address of the call's overflow argument area, or zero.
    pub argument_base: u64,
    /// Isolate-local feedback-vector base, or zero while not migrated.
    pub feedback_base: u64,
    /// Installed code-object identity/anchor, or zero for interpreter frames.
    pub code_object_id: u64,
    /// Boxed `this` value.
    pub this_value_bits: u64,
    /// Boxed `new.target` value.
    pub new_target_bits: u64,
    /// Caller destination register; `u32::MAX` at the stack root.
    pub return_register: u32,
    /// Index into cold async/generator/protocol state, or `u32::MAX`.
    pub cold_state_index: u32,
}

/// Result of interpreter/compiled dispatch.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchStatus {
    /// Function returned; `value_bits` is the completion value.
    Return = 0,
    /// Resume the interpreter at `payload` logical PC.
    SideExit = 1,
    /// A rooted pending exception is stored on [`VmThread`].
    Throw = 2,
    /// Interrupt/budget handling is required at `payload` logical PC.
    Interrupt = 3,
    /// Fatal allocation failure.
    OutOfMemory = 4,
}

/// Fixed two-word dispatch result shared by interpreter and compiled entries.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchResult {
    /// Dispatch action.
    pub status: DispatchStatus,
    /// Logical PC, reason id, or zero depending on [`Self::status`].
    pub payload: u32,
    /// Boxed completion value bits for [`DispatchStatus::Return`].
    pub value_bits: u64,
}

impl DispatchResult {
    /// Normal return.
    #[must_use]
    pub const fn returned(value_bits: u64) -> Self {
        Self {
            status: DispatchStatus::Return,
            payload: 0,
            value_bits,
        }
    }

    /// Exact logical-PC side exit.
    #[must_use]
    pub const fn side_exit(logical_pc: u32) -> Self {
        Self {
            status: DispatchStatus::SideExit,
            payload: logical_pc,
            value_bits: 0,
        }
    }

    /// Throw with the exception rooted in [`VmThread::pending_exception_bits`].
    #[must_use]
    pub const fn thrown() -> Self {
        Self {
            status: DispatchStatus::Throw,
            payload: 0,
            value_bits: 0,
        }
    }
}

/// Runtime-stub semantic class.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStubClass {
    /// Cannot allocate, cannot call JS, cannot trigger GC.
    LeafNoAlloc = 0,
    /// May allocate and must provide a precise safepoint.
    Alloc = 1,
    /// May call JS/proxies/accessors and must support full reentry/deopt state.
    Reentrant = 2,
}

impl RuntimeStubClass {
    /// Whether this stub class can allocate and therefore needs a safepoint.
    #[must_use]
    pub const fn can_allocate(self) -> bool {
        matches!(self, Self::Alloc | Self::Reentrant)
    }

    /// Whether this stub class can re-enter JS.
    #[must_use]
    pub const fn can_reenter_js(self) -> bool {
        matches!(self, Self::Reentrant)
    }
}

/// Runtime-stub observable effects. The descriptor verifier cross-checks these
/// bits against [`RuntimeStubClass`] so leaf stubs cannot silently acquire a
/// throwing, allocating, collecting, or reentrant implementation.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeStubEffects(u16);

impl RuntimeStubEffects {
    /// Stub may allocate managed or externally-accounted memory.
    pub const MAY_ALLOCATE: u16 = 1 << 0;
    /// Stub may trigger a moving collection.
    pub const MAY_TRIGGER_GC: u16 = 1 << 1;
    /// Stub may produce a JavaScript exception.
    pub const MAY_THROW: u16 = 1 << 2;
    /// Stub may call JavaScript, proxies, accessors, or coercion hooks.
    pub const MAY_REENTER_JS: u16 = 1 << 3;
    /// Stub may mutate a GC-managed object and must perform the write barrier.
    pub const MAY_MUTATE_GC: u16 = 1 << 4;

    /// No observable effects beyond reading passive state and returning a value.
    #[must_use]
    pub const fn none() -> Self {
        Self(0)
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

/// Machine-callable runtime stub descriptor.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeStubDescriptor {
    /// Stable descriptor id.
    pub id: RuntimeStubId,
    /// Stub semantic class.
    pub class: RuntimeStubClass,
    /// Fixed argument count for the fast ABI entry, or
    /// [`VARIADIC_STUB_ARGUMENTS`] when the call shape is argument-vector based.
    pub argument_count: u8,
    /// Declared observable effects.
    pub effects: RuntimeStubEffects,
}

/// VM-native allocation/rooting context passed to allocating runtime stubs.
///
/// This is the fixed machine-call packet for `AllocStub` entries. It is not a
/// generic native-call context: it contains only the active VM reentry pointers
/// the current frame register window that a safepoint map can trace/update, and
/// the safepoint table for the current compiled function. Generated code builds
/// this packet from its `JitCtx` immediately before an allocating stub call and
/// must not retain it after the call returns.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeStubAllocContext {
    /// Erased `*mut Interpreter`.
    pub vm: *mut std::ffi::c_void,
    /// Erased `*mut JitFrameStack`.
    pub stack: *mut std::ffi::c_void,
    /// Erased `*const ExecutionContext`.
    pub context: *const std::ffi::c_void,
    /// Base of the active function's safepoint records.
    pub safepoint_records: *const SafepointRecord,
    /// Number of records starting at [`Self::safepoint_records`].
    pub safepoint_count: u32,
    /// Reserved for flags without changing the ABI shape.
    pub reserved0: u32,
    /// Current frame index within `stack`.
    pub frame_index: usize,
    /// Base of the current frame's tagged register window as raw `Value` bits.
    pub frame_slots: *mut u64,
    /// Number of tagged frame slots starting at [`Self::frame_slots`].
    pub frame_slot_count: u16,
    /// Number of native spill slots starting at [`Self::spill_slots`].
    pub spill_slot_count: u16,
    /// Reserved for future safepoint/deopt metadata width.
    pub reserved1: u32,
    /// Absolute base of the compiled frame's native spill/save area as raw
    /// `Value` bits (the `[sp]`-relative area a register-map safepoint saves
    /// call-crossing values into). A [`TaggedLocation::spill_slot`] root at index
    /// `i` names `spill_slots[i]`. Null when the call site publishes no native
    /// spill roots (every safepoint location is a frame slot).
    pub spill_slots: *mut u64,
}

impl RuntimeStubAllocContext {
    /// Build a context packet for an allocating runtime-stub call.
    #[must_use]
    pub const fn new(
        vm: *mut std::ffi::c_void,
        stack: *mut std::ffi::c_void,
        context: *const std::ffi::c_void,
        safepoint_records: *const SafepointRecord,
        safepoint_count: u32,
        frame_index: usize,
        frame_slots: *mut u64,
        frame_slot_count: u16,
    ) -> Self {
        Self {
            vm,
            stack,
            context,
            safepoint_records,
            safepoint_count,
            reserved0: 0,
            frame_index,
            frame_slots,
            frame_slot_count,
            spill_slot_count: 0,
            reserved1: 0,
            spill_slots: std::ptr::null_mut(),
        }
    }

    /// Attach a native spill/save-area root window to this packet. The base must
    /// point at `count` live, writable `Value` ABI slots that stay pinned while a
    /// GC may trace and update them.
    #[must_use]
    pub const fn with_spill_area(mut self, spill_slots: *mut u64, count: u16) -> Self {
        self.spill_slots = spill_slots;
        self.spill_slot_count = count;
        self
    }

    /// Whether this packet names a concrete frame-slot root window.
    #[must_use]
    pub const fn has_frame_slots(self) -> bool {
        !self.frame_slots.is_null() && self.frame_slot_count != 0
    }

    /// Whether this packet names a concrete native spill/save-area root window.
    #[must_use]
    pub const fn has_spill_slots(self) -> bool {
        !self.spill_slots.is_null() && self.spill_slot_count != 0
    }

    /// Whether this packet names a concrete safepoint-record table.
    #[must_use]
    pub const fn has_safepoint_records(self) -> bool {
        !self.safepoint_records.is_null() && self.safepoint_count != 0
    }
}

/// Current compiled `Op::Call` bridge. Re-entrant because it can invoke
/// arbitrary JS or native callables.
pub const STUB_JIT_RUNTIME_CALL: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 1,
    class: RuntimeStubClass::Reentrant,
    argument_count: VARIADIC_STUB_ARGUMENTS,
    effects: RuntimeStubEffects::reentrant(true),
};

/// Current compiled `CallMethodValue` bridge. Re-entrant because method
/// resolution and invocation can run user code.
pub const STUB_JIT_RUNTIME_CALL_METHOD: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 2,
    class: RuntimeStubClass::Reentrant,
    argument_count: VARIADIC_STUB_ARGUMENTS,
    effects: RuntimeStubEffects::reentrant(true),
};

/// Direct compiled-call frame preparation. It does not intentionally re-enter
/// JS, but it can allocate upvalue/frame-side state and therefore needs an
/// allocating-stub safepoint in the target ABI.
pub const STUB_JIT_PREPARE_DIRECT_CALL: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 3,
    class: RuntimeStubClass::Alloc,
    argument_count: VARIADIC_STUB_ARGUMENTS,
    effects: RuntimeStubEffects::allocating(true, true),
};

/// Direct compiled method-call frame preparation. Same allocation contract as
/// [`STUB_JIT_PREPARE_DIRECT_CALL`].
pub const STUB_JIT_PREPARE_DIRECT_METHOD_CALL: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 4,
    class: RuntimeStubClass::Alloc,
    argument_count: VARIADIC_STUB_ARGUMENTS,
    effects: RuntimeStubEffects::allocating(true, true),
};

/// Current compiled property/method runtime fallback bucket. Re-entrant until
/// individual property operations are split into leaf/allocating stubs.
pub const STUB_JIT_PROPERTY_FALLBACK: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 5,
    class: RuntimeStubClass::Reentrant,
    argument_count: VARIADIC_STUB_ARGUMENTS,
    effects: RuntimeStubEffects::reentrant(true),
};

/// Compiled-loop backedge poll for interrupts and runtime-budget enforcement.
/// This is leaf/no-alloc: it charges reductions, checks the VM interrupt flag,
/// and may report an already-constructed structural VM error, but it must not
/// allocate, trigger GC, or re-enter JS.
pub const STUB_JIT_BACKEDGE_POLL: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 6,
    class: RuntimeStubClass::LeafNoAlloc,
    argument_count: 1,
    effects: RuntimeStubEffects::none(),
};

/// Leaf `Map.prototype.get` probe used after method/prototype guards have
/// proven the receiver and builtin identity. The key must already be in a
/// representation that does not require flattening/materialisation.
pub const STUB_COLLECTION_MAP_GET_LEAF: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 7,
    class: RuntimeStubClass::LeafNoAlloc,
    argument_count: 2,
    effects: RuntimeStubEffects::none(),
};

/// Leaf `Map.prototype.has` probe with the same no-flatten/no-GC contract as
/// [`STUB_COLLECTION_MAP_GET_LEAF`].
pub const STUB_COLLECTION_MAP_HAS_LEAF: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 8,
    class: RuntimeStubClass::LeafNoAlloc,
    argument_count: 2,
    effects: RuntimeStubEffects::none(),
};

/// Leaf `Set.prototype.has` probe with the same no-flatten/no-GC contract as
/// [`STUB_COLLECTION_MAP_GET_LEAF`].
pub const STUB_COLLECTION_SET_HAS_LEAF: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 9,
    class: RuntimeStubClass::LeafNoAlloc,
    argument_count: 2,
    effects: RuntimeStubEffects::none(),
};

/// Allocating `Map.prototype.set` mutation stub.
///
/// The fixed value-argument shape is `(receiver, key, value)`. Machine callers
/// must also pass the current VM-native frame/context pointer and a non-sentinel
/// safepoint id so a moving GC can trace and rewrite all live tagged values.
pub const STUB_COLLECTION_MAP_SET_ALLOC: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 10,
    class: RuntimeStubClass::Alloc,
    argument_count: 3,
    effects: RuntimeStubEffects::allocating(true, true),
};

/// Allocating `Set.prototype.add` mutation stub.
///
/// Uses the same three-value ABI shape as [`STUB_COLLECTION_MAP_SET_ALLOC`]:
/// `(receiver, value, unused)`. Keeping the collection mutation stubs uniform
/// lets generated method-call code pass receiver plus two argument slots without
/// a per-builtin bridge shape.
pub const STUB_COLLECTION_SET_ADD_ALLOC: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 11,
    class: RuntimeStubClass::Alloc,
    argument_count: 3,
    effects: RuntimeStubEffects::allocating(true, true),
};

/// Allocating `Map.prototype.get` lookup stub.
///
/// Uses the uniform three-value ABI shape `(receiver, key, unused)`. This stub
/// may materialize or flatten string keys before probing the map, so callers
/// must publish the same safepoint/root packet as mutation stubs.
pub const STUB_COLLECTION_MAP_GET_ALLOC: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 12,
    class: RuntimeStubClass::Alloc,
    argument_count: 3,
    effects: RuntimeStubEffects::allocating(true, false),
};

/// Allocating `Map.prototype.has` lookup stub with the same materializing key
/// contract as [`STUB_COLLECTION_MAP_GET_ALLOC`].
pub const STUB_COLLECTION_MAP_HAS_ALLOC: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 13,
    class: RuntimeStubClass::Alloc,
    argument_count: 3,
    effects: RuntimeStubEffects::allocating(true, false),
};

/// Allocating `Set.prototype.has` lookup stub with the same materializing key
/// contract as [`STUB_COLLECTION_MAP_GET_ALLOC`].
pub const STUB_COLLECTION_SET_HAS_ALLOC: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 14,
    class: RuntimeStubClass::Alloc,
    argument_count: 3,
    effects: RuntimeStubEffects::allocating(true, false),
};

/// Allocating `Map.prototype.delete` mutation stub with the same materializing
/// key contract as [`STUB_COLLECTION_MAP_GET_ALLOC`].
pub const STUB_COLLECTION_MAP_DELETE_ALLOC: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 15,
    class: RuntimeStubClass::Alloc,
    argument_count: 3,
    effects: RuntimeStubEffects::allocating(true, true),
};

/// Allocating `Set.prototype.delete` mutation stub with the same materializing
/// key contract as [`STUB_COLLECTION_MAP_GET_ALLOC`].
pub const STUB_COLLECTION_SET_DELETE_ALLOC: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 16,
    class: RuntimeStubClass::Alloc,
    argument_count: 3,
    effects: RuntimeStubEffects::allocating(true, true),
};

/// Allocating primitive string-concat stub for `+`.
///
/// Uses the uniform three-value ABI shape `(lhs, rhs, unused)`. It is only
/// valid when at least one operand is already a primitive string and no
/// observable `ToPrimitive` work is needed; other cases miss to the full
/// bytecode delegate.
pub const STUB_STRING_CONCAT_ALLOC: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 17,
    class: RuntimeStubClass::Alloc,
    argument_count: 3,
    effects: RuntimeStubEffects::allocating(true, false),
};

/// Human-readable symbol for a stable runtime-stub id.
#[must_use]
pub const fn runtime_stub_name(id: RuntimeStubId) -> &'static str {
    match id {
        1 => "jit_runtime_call",
        2 => "jit_runtime_call_method",
        3 => "jit_prepare_direct_call",
        4 => "jit_prepare_direct_method_call",
        5 => "jit_property_fallback",
        6 => "jit_backedge_poll",
        7 => "collection_map_get_leaf",
        8 => "collection_map_has_leaf",
        9 => "collection_set_has_leaf",
        10 => "collection_map_set_alloc",
        11 => "collection_set_add_alloc",
        12 => "collection_map_get_alloc",
        13 => "collection_map_has_alloc",
        14 => "collection_set_has_alloc",
        15 => "collection_map_delete_alloc",
        16 => "collection_set_delete_alloc",
        17 => "string_concat_alloc",
        _ => "unknown_runtime_stub",
    }
}

/// Checked inventory of every current machine-callable runtime-stub contract.
pub const RUNTIME_STUB_DESCRIPTORS: &[RuntimeStubDescriptor] = &[
    STUB_JIT_RUNTIME_CALL,
    STUB_JIT_RUNTIME_CALL_METHOD,
    STUB_JIT_PREPARE_DIRECT_CALL,
    STUB_JIT_PREPARE_DIRECT_METHOD_CALL,
    STUB_JIT_PROPERTY_FALLBACK,
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
];

/// Status code returned by a runtime stub.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStubStatus {
    /// Stub completed and `value_bits` carries the JS result.
    Ok = 0,
    /// Guarded fast path was not applicable; caller should use the next slower
    /// ABI path with equivalent semantics.
    Miss = 1,
    /// Stub threw; `payload` identifies the parked VM error payload.
    Throw = 2,
    /// Stub requests deopt; `payload` identifies the target frame state.
    Deopt = 3,
    /// Allocation failed.
    OutOfMemory = 4,
    /// Runtime interrupt or budget stop.
    Interrupt = 5,
}

/// Fixed-width result returned by machine-callable runtime stubs.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeStubResult {
    /// Result status.
    pub status: RuntimeStubStatus,
    /// Raw boxed [`crate::Value`] bits when `status == Ok`.
    pub value_bits: u64,
    /// Status-specific payload: error id, frame-state id, or zero.
    pub payload: u64,
}

impl RuntimeStubResult {
    /// Successful stub result from raw boxed value bits.
    #[must_use]
    pub const fn ok_bits(value_bits: u64) -> Self {
        Self {
            status: RuntimeStubStatus::Ok,
            value_bits,
            payload: 0,
        }
    }

    /// Successful stub result from a boxed VM [`crate::Value`].
    #[must_use]
    pub(crate) const fn ok_value(value: crate::Value) -> Self {
        Self::ok_bits(value.to_abi_bits())
    }

    /// Guard miss: the caller should run the next slower ABI-compatible path.
    #[must_use]
    pub const fn miss() -> Self {
        Self {
            status: RuntimeStubStatus::Miss,
            value_bits: 0,
            payload: 0,
        }
    }

    /// Deopt request targeting `frame_state`.
    #[must_use]
    pub const fn deopt(frame_state: FrameStateId) -> Self {
        Self {
            status: RuntimeStubStatus::Deopt,
            value_bits: 0,
            payload: frame_state as u64,
        }
    }

    /// Allocation failed while running the stub.
    #[must_use]
    pub const fn out_of_memory() -> Self {
        Self {
            status: RuntimeStubStatus::OutOfMemory,
            value_bits: 0,
            payload: 0,
        }
    }

    /// Extract the successful boxed VM value.
    #[must_use]
    pub(crate) const fn into_value(self) -> Option<crate::Value> {
        match self.status {
            RuntimeStubStatus::Ok => Some(crate::Value::from_abi_bits(self.value_bits)),
            RuntimeStubStatus::Miss
            | RuntimeStubStatus::Throw
            | RuntimeStubStatus::Deopt
            | RuntimeStubStatus::OutOfMemory
            | RuntimeStubStatus::Interrupt => None,
        }
    }
}

/// Two-register runtime-stub result ABI.
///
/// This is the machine-code-friendly form for leaf stubs on AArch64/x86_64:
/// `value_bits` occupies the first result register and `status_payload` the
/// second. The low 8 bits of `status_payload` are [`RuntimeStubStatus`]; the
/// remaining high bits carry the payload. General re-entrant stubs can keep
/// using [`RuntimeStubResult`] until they need the same direct-call ABI.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeStubResultPair {
    /// Raw boxed [`crate::Value`] bits when status is `Ok`.
    pub value_bits: u64,
    /// Low 8 bits: status. High 56 bits: payload.
    pub status_payload: u64,
}

impl RuntimeStubResultPair {
    /// Pack a full runtime-stub result into the two-register ABI.
    #[must_use]
    pub const fn from_result(result: RuntimeStubResult) -> Self {
        Self {
            value_bits: result.value_bits,
            status_payload: ((result.payload & 0x00ff_ffff_ffff_ffff) << 8) | result.status as u64,
        }
    }

    /// Extract the status byte.
    #[must_use]
    pub const fn status(self) -> RuntimeStubStatus {
        match (self.status_payload & 0xff) as u8 {
            0 => RuntimeStubStatus::Ok,
            1 => RuntimeStubStatus::Miss,
            2 => RuntimeStubStatus::Throw,
            3 => RuntimeStubStatus::Deopt,
            4 => RuntimeStubStatus::OutOfMemory,
            _ => RuntimeStubStatus::Interrupt,
        }
    }

    /// Extract the packed payload.
    #[must_use]
    pub const fn payload(self) -> u64 {
        self.status_payload >> 8
    }

    /// Convert back to the full Rust-facing result record.
    #[must_use]
    pub const fn into_result(self) -> RuntimeStubResult {
        RuntimeStubResult {
            status: self.status(),
            value_bits: self.value_bits,
            payload: self.payload(),
        }
    }
}

/// Storage class for one tagged value location at a safepoint.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaggedLocationKind {
    /// Interpreter-visible register-window slot.
    FrameSlot = 0,
    /// Machine register in the platform ABI register numbering.
    MachineRegister = 1,
    /// Native spill slot relative to the frame's spill area base.
    SpillSlot = 2,
}

/// One tagged `Value` location live at a safepoint.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaggedLocation {
    /// Storage class.
    pub kind: TaggedLocationKind,
    /// Register number, frame slot index, or spill slot index.
    pub index: u16,
}

impl TaggedLocation {
    /// Tagged interpreter-visible frame slot.
    #[must_use]
    pub const fn frame_slot(index: u16) -> Self {
        Self {
            kind: TaggedLocationKind::FrameSlot,
            index,
        }
    }

    /// Tagged machine register.
    #[must_use]
    pub const fn machine_register(index: u16) -> Self {
        Self {
            kind: TaggedLocationKind::MachineRegister,
            index,
        }
    }

    /// Tagged native spill slot.
    #[must_use]
    pub const fn spill_slot(index: u16) -> Self {
        Self {
            kind: TaggedLocationKind::SpillSlot,
            index,
        }
    }
}

/// Compact immutable frame-root map descriptor.
///
/// `bitmap_offset` and `bitmap_word_count` index a CodeObject-owned `u64`
/// bitmap table. Bit `n` names tagged register slot `n`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMap {
    /// Dense map id local to the owning CodeObject.
    pub id: u32,
    /// First bitmap word in the CodeObject frame-map table.
    pub bitmap_offset: u32,
    /// Number of bitmap words.
    pub bitmap_word_count: u16,
    /// Number of initialized frame slots covered by the map.
    pub slot_count: u16,
}

/// Compact immutable native spill-root map descriptor.
///
/// Spill locations are signed byte offsets from the native spill-area base and
/// use the CodeObject-owned `i32` location table.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpillMap {
    /// Dense map id local to the owning CodeObject.
    pub id: u32,
    /// First spill offset in the CodeObject spill-location table.
    pub location_offset: u32,
    /// Number of tagged spill locations.
    pub location_count: u16,
    /// Reserved; must be zero in layout version 1.
    pub reserved: u16,
}

/// Machine-code return-PC safepoint entry.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafepointEntry {
    /// Native return offset within the owning CodeObject.
    pub native_return_offset: u32,
    /// Canonical logical bytecode PC published before the call.
    pub logical_pc: u32,
    /// [`FrameMap::id`].
    pub frame_map_id: u32,
    /// [`SpillMap::id`].
    pub spill_map_id: u32,
    /// Deopt/side-exit frame state or [`NO_FRAME_STATE`].
    pub frame_state_id: FrameStateId,
    /// Runtime stub invoked at this return PC.
    pub stub_id: RuntimeStubId,
}

/// Kind of assumption that can invalidate installed native code.
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeDependencyKind {
    /// VM field layout/build compatibility.
    VmLayout = 0,
    /// Runtime-stub table ids/signatures compatibility.
    RuntimeStubTable = 1,
    /// Realm identity.
    Realm = 2,
    /// Global/prototype/array protector epoch.
    Protector = 3,
    /// Builtin identity epoch.
    BuiltinIdentity = 4,
    /// Shape/prototype dependency not represented by a live feedback guard.
    ShapeEpoch = 5,
}

/// Explicit validity dependency owned by a CodeObject.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeDependency {
    /// Dependency family.
    pub kind: CodeDependencyKind,
    /// Reserved flags; must be zero in layout version 1.
    pub flags: u16,
    /// Isolate-local stable identity.
    pub identity: u32,
    /// Expected version/value at code entry.
    pub expected: u64,
}

/// Lifecycle state for installed code. Invalidation first transitions
/// `Installed -> Invalid`, unlinks entry points, then transitions to `Retired`;
/// executable memory can be reclaimed only after all active anchors are gone.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeLifetimeState {
    /// Entry is valid and may be selected.
    Installed = 0,
    /// Entry is unlinked; active frames may still return through the code.
    Invalid = 1,
    /// No new or active entry; awaiting/finalizing reclamation.
    Retired = 2,
}

/// Passive GC/deopt safepoint metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafepointRecord {
    /// Stable safepoint id.
    pub id: SafepointId,
    /// Frame-state snapshot to use for deopt, or [`NO_FRAME_STATE`].
    pub frame_state: FrameStateId,
    /// Tagged values visible to the moving collector.
    pub tagged_locations: Vec<TaggedLocation>,
}

impl SafepointRecord {
    /// Build a safepoint that treats the interpreter-visible register window as
    /// the tagged root set.
    ///
    /// Baseline v1 keeps GC-bearing values in frame slots at allocation
    /// boundaries, so this is the first machine-stub-safe map before finer
    /// liveness and register/spill maps are available.
    #[must_use]
    pub fn frame_slot_window(
        id: SafepointId,
        frame_state: FrameStateId,
        register_count: u16,
    ) -> Self {
        Self {
            id,
            frame_state,
            tagged_locations: (0..register_count)
                .map(TaggedLocation::frame_slot)
                .collect(),
        }
    }

    /// Whether this safepoint can reconstruct an interpreter-visible frame state.
    #[must_use]
    pub fn has_deopt_state(&self) -> bool {
        self.frame_state != NO_FRAME_STATE
    }
}

/// Validate that a runtime stub descriptor is internally consistent.
#[must_use]
pub const fn validate_stub_descriptor(desc: RuntimeStubDescriptor, safepoint: SafepointId) -> bool {
    match desc.class {
        RuntimeStubClass::LeafNoAlloc => safepoint == NO_SAFEPOINT && desc.effects.bits() == 0,
        RuntimeStubClass::Alloc => {
            safepoint != NO_SAFEPOINT
                && desc
                    .effects
                    .contains(RuntimeStubEffects::MAY_ALLOCATE | RuntimeStubEffects::MAY_TRIGGER_GC)
                && !desc.effects.contains(RuntimeStubEffects::MAY_REENTER_JS)
        }
        RuntimeStubClass::Reentrant => {
            safepoint != NO_SAFEPOINT
                && desc.effects.contains(
                    RuntimeStubEffects::MAY_ALLOCATE
                        | RuntimeStubEffects::MAY_TRIGGER_GC
                        | RuntimeStubEffects::MAY_THROW
                        | RuntimeStubEffects::MAY_REENTER_JS,
                )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_stub_must_not_name_safepoint() {
        let desc = RuntimeStubDescriptor {
            id: 1,
            class: RuntimeStubClass::LeafNoAlloc,
            argument_count: 2,
            effects: RuntimeStubEffects::none(),
        };
        assert!(validate_stub_descriptor(desc, NO_SAFEPOINT));
        assert!(!validate_stub_descriptor(desc, 7));
    }

    #[test]
    fn allocating_stub_must_name_safepoint() {
        assert!(!validate_stub_descriptor(
            STUB_COLLECTION_MAP_SET_ALLOC,
            NO_SAFEPOINT
        ));
        assert!(validate_stub_descriptor(STUB_COLLECTION_MAP_SET_ALLOC, 9));
        assert!(!validate_stub_descriptor(
            STUB_COLLECTION_SET_ADD_ALLOC,
            NO_SAFEPOINT
        ));
        assert!(validate_stub_descriptor(STUB_COLLECTION_SET_ADD_ALLOC, 9));
        assert!(!validate_stub_descriptor(
            STUB_COLLECTION_MAP_GET_ALLOC,
            NO_SAFEPOINT
        ));
        assert!(validate_stub_descriptor(STUB_COLLECTION_MAP_GET_ALLOC, 9));
        assert!(!validate_stub_descriptor(
            STUB_COLLECTION_MAP_HAS_ALLOC,
            NO_SAFEPOINT
        ));
        assert!(validate_stub_descriptor(STUB_COLLECTION_MAP_HAS_ALLOC, 9));
        assert!(!validate_stub_descriptor(
            STUB_COLLECTION_SET_HAS_ALLOC,
            NO_SAFEPOINT
        ));
        assert!(validate_stub_descriptor(STUB_COLLECTION_SET_HAS_ALLOC, 9));
        assert!(!validate_stub_descriptor(
            STUB_COLLECTION_MAP_DELETE_ALLOC,
            NO_SAFEPOINT
        ));
        assert!(validate_stub_descriptor(
            STUB_COLLECTION_MAP_DELETE_ALLOC,
            9
        ));
        assert!(!validate_stub_descriptor(
            STUB_COLLECTION_SET_DELETE_ALLOC,
            NO_SAFEPOINT
        ));
        assert!(validate_stub_descriptor(
            STUB_COLLECTION_SET_DELETE_ALLOC,
            9
        ));
        assert!(!validate_stub_descriptor(
            STUB_STRING_CONCAT_ALLOC,
            NO_SAFEPOINT
        ));
        assert!(validate_stub_descriptor(STUB_STRING_CONCAT_ALLOC, 9));
    }

    #[test]
    fn abi_records_stay_small() {
        assert_eq!(std::mem::size_of::<VmThread>(), 56);
        assert_eq!(std::mem::size_of::<NativeFrameHeader>(), 40);
        assert_eq!(std::mem::size_of::<NativeFrame>(), 96);
        assert_eq!(std::mem::size_of::<DispatchResult>(), 16);
        assert_eq!(std::mem::size_of::<RuntimeStubDescriptor>(), 8);
        assert!(std::mem::size_of::<RuntimeStubAllocContext>() <= 72);
        assert!(std::mem::size_of::<RuntimeStubResult>() <= 24);
        assert_eq!(std::mem::size_of::<RuntimeStubResultPair>(), 16);
        assert!(std::mem::size_of::<TaggedLocation>() <= 4);
        assert_eq!(std::mem::size_of::<FrameMap>(), 12);
        assert_eq!(std::mem::size_of::<SpillMap>(), 12);
        assert_eq!(std::mem::size_of::<SafepointEntry>(), 24);
        assert_eq!(std::mem::size_of::<CodeDependency>(), 16);
    }

    #[test]
    fn host_abi_offsets_are_golden() {
        assert_eq!(std::mem::offset_of!(VmThread, current_frame), 0);
        assert_eq!(std::mem::offset_of!(VmThread, pending_exception_bits), 32);
        assert_eq!(std::mem::offset_of!(VmThread, layout_version), 48);
        assert_eq!(std::mem::offset_of!(NativeFrameHeader, previous_frame), 0);
        assert_eq!(std::mem::offset_of!(NativeFrameHeader, resume_pc), 16);
        assert_eq!(std::mem::offset_of!(NativeFrameHeader, flags), 24);
        assert_eq!(std::mem::offset_of!(NativeFrameHeader, feedback_index), 32);
        assert_eq!(std::mem::offset_of!(NativeFrame, register_base), 40);
        assert_eq!(std::mem::offset_of!(NativeFrame, code_object_id), 64);
        assert_eq!(std::mem::offset_of!(NativeFrame, return_register), 88);
    }

    #[test]
    fn runtime_stub_inventory_is_dense_and_classified() {
        for (index, descriptor) in RUNTIME_STUB_DESCRIPTORS.iter().enumerate() {
            assert_eq!(descriptor.id as usize, index + 1);
            assert_ne!(runtime_stub_name(descriptor.id), "unknown_runtime_stub");
            let safepoint = if descriptor.class == RuntimeStubClass::LeafNoAlloc {
                NO_SAFEPOINT
            } else {
                0
            };
            assert!(
                validate_stub_descriptor(*descriptor, safepoint),
                "invalid descriptor {}",
                runtime_stub_name(descriptor.id)
            );
        }
    }

    #[test]
    fn alloc_context_layout_is_machine_readable() {
        assert_eq!(std::mem::offset_of!(RuntimeStubAllocContext, vm), 0);
        assert_eq!(
            std::mem::offset_of!(RuntimeStubAllocContext, stack),
            std::mem::size_of::<usize>()
        );
        assert!(
            std::mem::offset_of!(RuntimeStubAllocContext, safepoint_records)
                > std::mem::offset_of!(RuntimeStubAllocContext, context)
        );
        assert!(
            std::mem::offset_of!(RuntimeStubAllocContext, frame_slots)
                > std::mem::offset_of!(RuntimeStubAllocContext, frame_index)
        );
        let mut slots = [0_u64; 2];
        let safepoints = [SafepointRecord::frame_slot_window(3, NO_FRAME_STATE, 2)];
        let ctx = RuntimeStubAllocContext::new(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
            safepoints.as_ptr(),
            safepoints.len() as u32,
            7,
            slots.as_mut_ptr(),
            slots.len() as u16,
        );
        assert_eq!(ctx.frame_index, 7);
        assert!(ctx.has_frame_slots());
        assert!(ctx.has_safepoint_records());
        // A fresh packet publishes no native spill window until a register-map
        // safepoint attaches one.
        assert!(!ctx.has_spill_slots());
        let mut spill = [0_u64; 1];
        let with_spill = ctx.with_spill_area(spill.as_mut_ptr(), 1);
        assert!(with_spill.has_spill_slots());
    }

    #[test]
    fn stub_result_round_trips_values() {
        let value = crate::Value::number_i32(42);
        let result = RuntimeStubResult::ok_value(value);
        assert_eq!(result.status, RuntimeStubStatus::Ok);
        assert_eq!(result.into_value(), Some(value));
    }

    #[test]
    fn frame_slot_window_safepoint_covers_registers() {
        let record = SafepointRecord::frame_slot_window(3, NO_FRAME_STATE, 4);
        assert_eq!(record.id, 3);
        assert_eq!(record.frame_state, NO_FRAME_STATE);
        assert!(!record.has_deopt_state());
        assert_eq!(
            record.tagged_locations,
            vec![
                TaggedLocation::frame_slot(0),
                TaggedLocation::frame_slot(1),
                TaggedLocation::frame_slot(2),
                TaggedLocation::frame_slot(3),
            ]
        );
    }

    #[test]
    fn stub_result_miss_has_no_value() {
        let result = RuntimeStubResult::miss();
        assert_eq!(result.status, RuntimeStubStatus::Miss);
        assert_eq!(result.into_value(), None);
    }

    #[test]
    fn stub_result_pair_round_trips_result() {
        let result = RuntimeStubResult::deopt(17);
        let pair = RuntimeStubResultPair::from_result(result);
        assert_eq!(pair.status(), RuntimeStubStatus::Deopt);
        assert_eq!(pair.payload(), 17);
        assert_eq!(pair.into_result(), result);
    }
}
