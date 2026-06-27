//! Native execution ABI metadata shared by interpreter, JIT, runtime stubs, and
//! GC safepoints.
//!
//! This module is the passive source of truth for the VM-native ABI target. It
//! deliberately carries no execution policy yet: current interpreter/JIT paths
//! can adopt these records incrementally while keeping the existing correctness
//! surface intact.
//!
//! # Contents
//! - [`NativeFrameKind`] and [`NativeFrameHeader`] describe the frame shape every
//!   execution tier should converge on.
//! - [`RuntimeStubClass`], [`RuntimeStubStatus`], and [`RuntimeStubResult`]
//!   describe machine-callable runtime stubs without the generic `NativeCtx`
//!   boundary.
//! - [`SafepointRecord`] and [`TaggedLocation`] describe tagged `Value` liveness
//!   for moving-GC safepoints and deopt exits.
//!
//! # Invariants
//! - ABI-facing values are fixed-width integers or raw boxed `Value` bits.
//! - Allocating or re-entrant stubs must name a safepoint record; leaf stubs must
//!   not allocate, re-enter JS, or trigger GC.
//! - Safepoint locations describe only tagged GC-visible values. Unboxed machine
//!   values belong to deopt frame-state metadata, not GC root maps.
//!
//! # See also
//! - [`crate::jit`] for the current type-erased JIT entry surface.
//! - [`crate::frame_state`] for the current interpreter frame.
//! - `NATIVE_VM_ABI_PLAN.md` for the refactor roadmap.

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
    pub previous_frame: usize,
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

/// Machine-callable runtime stub descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeStubDescriptor {
    /// Stable descriptor id.
    pub id: RuntimeStubId,
    /// Human-readable symbol name for diagnostics and profiles.
    pub name: &'static str,
    /// Stub semantic class.
    pub class: RuntimeStubClass,
    /// Fixed argument count for the fast ABI entry, or
    /// [`VARIADIC_STUB_ARGUMENTS`] when the call shape is argument-vector based.
    pub argument_count: u8,
}

/// Current compiled `Op::Call` bridge. Re-entrant because it can invoke
/// arbitrary JS or native callables.
pub const STUB_JIT_RUNTIME_CALL: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 1,
    name: "jit_runtime_call",
    class: RuntimeStubClass::Reentrant,
    argument_count: VARIADIC_STUB_ARGUMENTS,
};

/// Current compiled `CallMethodValue` bridge. Re-entrant because method
/// resolution and invocation can run user code.
pub const STUB_JIT_RUNTIME_CALL_METHOD: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 2,
    name: "jit_runtime_call_method",
    class: RuntimeStubClass::Reentrant,
    argument_count: VARIADIC_STUB_ARGUMENTS,
};

/// Direct compiled-call frame preparation. It does not intentionally re-enter
/// JS, but it can allocate upvalue/frame-side state and therefore needs an
/// allocating-stub safepoint in the target ABI.
pub const STUB_JIT_PREPARE_DIRECT_CALL: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 3,
    name: "jit_prepare_direct_call",
    class: RuntimeStubClass::Alloc,
    argument_count: VARIADIC_STUB_ARGUMENTS,
};

/// Direct compiled method-call frame preparation. Same allocation contract as
/// [`STUB_JIT_PREPARE_DIRECT_CALL`].
pub const STUB_JIT_PREPARE_DIRECT_METHOD_CALL: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 4,
    name: "jit_prepare_direct_method_call",
    class: RuntimeStubClass::Alloc,
    argument_count: VARIADIC_STUB_ARGUMENTS,
};

/// Current compiled property/method runtime fallback bucket. Re-entrant until
/// individual property operations are split into leaf/allocating stubs.
pub const STUB_JIT_PROPERTY_FALLBACK: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 5,
    name: "jit_property_fallback",
    class: RuntimeStubClass::Reentrant,
    argument_count: VARIADIC_STUB_ARGUMENTS,
};

/// Compiled-loop backedge poll for interrupts and runtime-budget enforcement.
/// This is leaf/no-alloc: it charges reductions, checks the VM interrupt flag,
/// and may report an already-constructed structural VM error, but it must not
/// allocate, trigger GC, or re-enter JS.
pub const STUB_JIT_BACKEDGE_POLL: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 6,
    name: "jit_backedge_poll",
    class: RuntimeStubClass::LeafNoAlloc,
    argument_count: 1,
};

/// Leaf `Map.prototype.get` probe used after method/prototype guards have
/// proven the receiver and builtin identity. The key must already be in a
/// representation that does not require flattening/materialisation.
pub const STUB_COLLECTION_MAP_GET_LEAF: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 7,
    name: "collection_map_get_leaf",
    class: RuntimeStubClass::LeafNoAlloc,
    argument_count: 2,
};

/// Leaf `Map.prototype.has` probe with the same no-flatten/no-GC contract as
/// [`STUB_COLLECTION_MAP_GET_LEAF`].
pub const STUB_COLLECTION_MAP_HAS_LEAF: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 8,
    name: "collection_map_has_leaf",
    class: RuntimeStubClass::LeafNoAlloc,
    argument_count: 2,
};

/// Leaf `Set.prototype.has` probe with the same no-flatten/no-GC contract as
/// [`STUB_COLLECTION_MAP_GET_LEAF`].
pub const STUB_COLLECTION_SET_HAS_LEAF: RuntimeStubDescriptor = RuntimeStubDescriptor {
    id: 9,
    name: "collection_set_has_leaf",
    class: RuntimeStubClass::LeafNoAlloc,
    argument_count: 2,
};

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
        RuntimeStubClass::LeafNoAlloc => safepoint == NO_SAFEPOINT,
        RuntimeStubClass::Alloc | RuntimeStubClass::Reentrant => safepoint != NO_SAFEPOINT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_stub_must_not_name_safepoint() {
        let desc = RuntimeStubDescriptor {
            id: 1,
            name: "map_has_leaf",
            class: RuntimeStubClass::LeafNoAlloc,
            argument_count: 2,
        };
        assert!(validate_stub_descriptor(desc, NO_SAFEPOINT));
        assert!(!validate_stub_descriptor(desc, 7));
    }

    #[test]
    fn allocating_stub_must_name_safepoint() {
        let desc = RuntimeStubDescriptor {
            id: 2,
            name: "map_set_alloc",
            class: RuntimeStubClass::Alloc,
            argument_count: 3,
        };
        assert!(!validate_stub_descriptor(desc, NO_SAFEPOINT));
        assert!(validate_stub_descriptor(desc, 9));
    }

    #[test]
    fn abi_records_stay_small() {
        assert!(std::mem::size_of::<NativeFrameHeader>() <= 40);
        assert!(std::mem::size_of::<RuntimeStubResult>() <= 24);
        assert_eq!(std::mem::size_of::<RuntimeStubResultPair>(), 16);
        assert!(std::mem::size_of::<TaggedLocation>() <= 4);
    }

    #[test]
    fn stub_result_round_trips_values() {
        let value = crate::Value::number_i32(42);
        let result = RuntimeStubResult::ok_value(value);
        assert_eq!(result.status, RuntimeStubStatus::Ok);
        assert_eq!(result.into_value(), Some(value));
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
