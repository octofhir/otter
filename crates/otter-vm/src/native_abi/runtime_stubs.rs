//! Classified machine-callable runtime-stub contracts.
//!
//! # Contents
//! - [`RuntimeStubDescriptor`] declares signature, effects, safepoint,
//!   exception, and result ABI for every dense [`RuntimeStubId`].
//! - [`RuntimeStubTable`] is the fixed C-layout table header seen by code.
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

use super::{NO_SAFEPOINT, NativeFrame, RUNTIME_STUB_TABLE_VERSION, SafepointId, VmThread};

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
    /// Runtime-owned variadic call packet.
    Variadic = 3,
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
    /// [`super::RuntimeStubResult`] C-layout record.
    Full = 0,
    /// [`super::RuntimeStubResultPair`] two-register encoding.
    StatusPair = 1,
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
    /// Reserved; zero in stub-table version 2.
    pub reserved: u8,
    /// Declared observable effects.
    pub effects: RuntimeStubEffects,
    /// Reserved; zero in stub-table version 2.
    pub reserved2: u16,
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
        reserved: 0,
        effects,
        reserved2: 0,
    }
}

/// Fixed C-layout header for the process-local runtime-stub table.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeStubTable {
    /// Address of `count` machine entry addresses indexed by `id - 1`.
    pub entries_address: u64,
    /// Address of `count` [`RuntimeStubDescriptor`] records.
    pub descriptors_address: u64,
    /// Dense descriptor/entry count.
    pub count: u32,
    /// [`RUNTIME_STUB_TABLE_VERSION`].
    pub version: u32,
}

impl RuntimeStubTable {
    /// Construct a table header for already-owned process-local arrays.
    #[must_use]
    pub const fn new(entries_address: u64, descriptors_address: u64, count: u32) -> Self {
        Self {
            entries_address,
            descriptors_address,
            count,
            version: RUNTIME_STUB_TABLE_VERSION,
        }
    }
}

/// VM-native allocation/rooting packet used by allocating entries.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeStubAllocContext {
    /// Active VM thread record.
    pub thread: *mut VmThread,
    /// Published activation containing the tagged frame window.
    pub frame: *mut NativeFrame,
    /// Installed code-object identity.
    pub code_object_id: u64,
    /// Dense safepoint id within the code object.
    pub safepoint_id: SafepointId,
    /// Reserved; zero in layout version 2.
    pub reserved0: u32,
    /// Number of native spill slots.
    pub spill_slot_count: u16,
    /// Reserved; zero in layout version 2.
    pub reserved1: u16,
    /// Base of tagged native spill slots.
    pub spill_slots: *mut u64,
}

impl RuntimeStubAllocContext {
    /// Build an allocating call packet.
    #[must_use]
    pub const fn new(
        thread: *mut VmThread,
        frame: *mut NativeFrame,
        code_object_id: u64,
        safepoint_id: SafepointId,
    ) -> Self {
        Self {
            thread,
            frame,
            code_object_id,
            safepoint_id,
            reserved0: 0,
            spill_slot_count: 0,
            reserved1: 0,
            spill_slots: std::ptr::null_mut(),
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
        if self.frame.is_null() {
            return false;
        }
        // SAFETY: callers uphold the live published-frame contract.
        let frame = unsafe { &*self.frame };
        frame.register_base != 0 && frame.header.register_count != 0
    }

    /// Whether a spill-slot window is present.
    #[must_use]
    pub const fn has_spill_slots(self) -> bool {
        !self.spill_slots.is_null() && self.spill_slot_count != 0
    }

    /// Whether code-object/safepoint identity is publishable.
    #[must_use]
    pub const fn has_safepoint_records(self) -> bool {
        !self.thread.is_null() && self.code_object_id != 0 && self.safepoint_id != NO_SAFEPOINT
    }
}

/// VM-published collection/method inline-cache probe and refresh operation.
pub const STUB_JIT_COLLECTION_METHOD_IC: RuntimeStubDescriptor = descriptor(
    1,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::Full,
);
/// Direct compiled method-call frame preparation.
pub const STUB_JIT_PREPARE_DIRECT_METHOD_CALL: RuntimeStubDescriptor = descriptor(
    2,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::Full,
);
/// Compiled property runtime fallback bucket.
pub const STUB_JIT_PROPERTY_FALLBACK: RuntimeStubDescriptor = descriptor(
    3,
    RuntimeStubClass::Reentrant,
    RuntimeStubSignature::Variadic,
    VARIADIC_STUB_ARGUMENTS,
    RuntimeStubEffects::reentrant(true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::Full,
);
/// Leaf compiled-loop backedge poll.
pub const STUB_JIT_BACKEDGE_POLL: RuntimeStubDescriptor = descriptor(
    4,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::Poll1,
    1,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::Full,
);
/// Leaf `Map.prototype.get` probe.
pub const STUB_COLLECTION_MAP_GET_LEAF: RuntimeStubDescriptor = descriptor(
    5,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::Full,
);
/// Leaf `Map.prototype.has` probe.
pub const STUB_COLLECTION_MAP_HAS_LEAF: RuntimeStubDescriptor = descriptor(
    6,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::Full,
);
/// Leaf `Set.prototype.has` probe.
pub const STUB_COLLECTION_SET_HAS_LEAF: RuntimeStubDescriptor = descriptor(
    7,
    RuntimeStubClass::LeafNoAlloc,
    RuntimeStubSignature::LeafValue2,
    2,
    RuntimeStubEffects::none(),
    RuntimeStubException::Never,
    RuntimeStubResultAbi::Full,
);
/// Allocating `Map.prototype.set` mutation.
pub const STUB_COLLECTION_MAP_SET_ALLOC: RuntimeStubDescriptor = descriptor(
    8,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusPair,
);
/// Allocating `Set.prototype.add` mutation.
pub const STUB_COLLECTION_SET_ADD_ALLOC: RuntimeStubDescriptor = descriptor(
    9,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusPair,
);
/// Allocating `Map.prototype.get` lookup.
pub const STUB_COLLECTION_MAP_GET_ALLOC: RuntimeStubDescriptor = descriptor(
    10,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusPair,
);
/// Allocating `Map.prototype.has` lookup.
pub const STUB_COLLECTION_MAP_HAS_ALLOC: RuntimeStubDescriptor = descriptor(
    11,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusPair,
);
/// Allocating `Set.prototype.has` lookup.
pub const STUB_COLLECTION_SET_HAS_ALLOC: RuntimeStubDescriptor = descriptor(
    12,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusPair,
);
/// Allocating `Map.prototype.delete` mutation.
pub const STUB_COLLECTION_MAP_DELETE_ALLOC: RuntimeStubDescriptor = descriptor(
    13,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusPair,
);
/// Allocating `Set.prototype.delete` mutation.
pub const STUB_COLLECTION_SET_DELETE_ALLOC: RuntimeStubDescriptor = descriptor(
    14,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, true),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusPair,
);
/// Allocating primitive string-concat operation.
pub const STUB_STRING_CONCAT_ALLOC: RuntimeStubDescriptor = descriptor(
    15,
    RuntimeStubClass::Alloc,
    RuntimeStubSignature::AllocValue3,
    3,
    RuntimeStubEffects::allocating(true, false),
    RuntimeStubException::Status,
    RuntimeStubResultAbi::StatusPair,
);

/// Human-readable symbol for a stable runtime-stub id.
#[must_use]
pub const fn runtime_stub_name(id: super::RuntimeStubId) -> &'static str {
    match id {
        1 => "jit_collection_method_ic",
        2 => "jit_prepare_direct_method_call",
        3 => "jit_property_fallback",
        4 => "jit_backedge_poll",
        5 => "collection_map_get_leaf",
        6 => "collection_map_has_leaf",
        7 => "collection_set_has_leaf",
        8 => "collection_map_set_alloc",
        9 => "collection_set_add_alloc",
        10 => "collection_map_get_alloc",
        11 => "collection_map_has_alloc",
        12 => "collection_set_has_alloc",
        13 => "collection_map_delete_alloc",
        14 => "collection_set_delete_alloc",
        15 => "string_concat_alloc",
        _ => "unknown_runtime_stub",
    }
}

/// Dense inventory of every current machine-callable runtime-stub contract.
pub const RUNTIME_STUB_DESCRIPTORS: &[RuntimeStubDescriptor] = &[
    STUB_JIT_COLLECTION_METHOD_IC,
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
        RuntimeStubSignature::AllocValue3 => {
            matches!(desc.result_abi, RuntimeStubResultAbi::StatusPair)
        }
        _ => matches!(desc.result_abi, RuntimeStubResultAbi::Full),
    };
    if !throwing_matches || !result_matches || desc.reserved != 0 || desc.reserved2 != 0 {
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
const _: [(); 24] = [(); std::mem::size_of::<RuntimeStubTable>()];
const _: [(); 8] = [(); std::mem::align_of::<RuntimeStubTable>()];
const _: [(); 0] = [(); std::mem::offset_of!(RuntimeStubDescriptor, id)];
const _: [(); 12] = [(); std::mem::offset_of!(RuntimeStubDescriptor, effects)];
const _: [(); 16] = [(); std::mem::offset_of!(RuntimeStubTable, count)];
const _: [(); 48] = [(); std::mem::size_of::<RuntimeStubAllocContext>()];
const _: [(); 16] = [(); std::mem::offset_of!(RuntimeStubAllocContext, code_object_id)];
const _: [(); 24] = [(); std::mem::offset_of!(RuntimeStubAllocContext, safepoint_id)];

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
        assert!(!validate_stub_descriptor(
            STUB_JIT_COLLECTION_METHOD_IC,
            NO_SAFEPOINT
        ));
        assert!(validate_stub_descriptor(STUB_JIT_COLLECTION_METHOD_IC, 7));
    }

    #[test]
    fn table_header_carries_authoritative_version() {
        let table = RuntimeStubTable::new(8, 16, RUNTIME_STUB_DESCRIPTORS.len() as u32);
        assert_eq!(table.version, RUNTIME_STUB_TABLE_VERSION);
        assert_eq!(table.count as usize, RUNTIME_STUB_DESCRIPTORS.len());
    }
}
