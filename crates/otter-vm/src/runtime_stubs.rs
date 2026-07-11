//! VM-native runtime stub entrypoints.
//!
//! These functions are the reusable implementation layer behind
//! [`crate::native_abi`] descriptors. The current interpreter can call them
//! directly, and generated code can later call the same entrypoints instead of
//! reimplementing equivalent fast paths.
//!
//! # Contents
//! - Leaf/no-allocation collection probes for `Map.get`, `Map.has`, and
//!   `Set.has`.
//! - Allocating collection mutation ABI descriptors for `Map.set` and
//!   `Set.add`.
//!
//! # Invariants
//! - Arguments are boxed [`crate::Value`] raw ABI bits.
//! - Results are returned as [`crate::native_abi::RuntimeStubResult`].
//! - `LeafNoAlloc` stubs must not allocate, trigger GC, call JS, flatten
//!   strings, or mutate heap state.
//! - `Alloc` stubs must publish their current safepoint roots before any
//!   allocation and must not hold untracked raw `Value` bits across GC.
//!
//! # See also
//! - [`crate::native_abi`]
//! - [`crate::method_ops`]

use crate::native_abi::{
    CodeRegistryView, NO_SAFEPOINT, RuntimeStubAllocContext, RuntimeStubDescriptor, RuntimeStubId,
    RuntimeStubResult, RuntimeStubResultPair, STUB_COLLECTION_MAP_DELETE_ALLOC,
    STUB_COLLECTION_MAP_GET_ALLOC, STUB_COLLECTION_MAP_GET_LEAF, STUB_COLLECTION_MAP_HAS_ALLOC,
    STUB_COLLECTION_MAP_HAS_LEAF, STUB_COLLECTION_MAP_SET_ALLOC, STUB_COLLECTION_SET_ADD_ALLOC,
    STUB_COLLECTION_SET_DELETE_ALLOC, STUB_COLLECTION_SET_HAS_ALLOC, STUB_COLLECTION_SET_HAS_LEAF,
    STUB_STRING_CONCAT_ALLOC, SafepointId, SafepointRecord, TaggedLocationKind,
    validate_stub_descriptor,
};
use crate::{Interpreter, Value, collections};
use std::cell::UnsafeCell;

/// Two-argument leaf/no-allocation runtime stub ABI.
///
/// The heap pointer is opaque to generated code. It must name the current
/// isolate heap and must remain valid for the duration of the call. The callee
/// must not allocate, trigger GC, or retain the pointer.
pub type LeafNoAllocStub2Fn = extern "C" fn(*const otter_gc::GcHeap, u64, u64) -> RuntimeStubResult;

/// Callable leaf/no-allocation stub entry with its ABI descriptor.
#[derive(Clone, Copy)]
pub struct LeafNoAllocStub2 {
    /// Passive descriptor shared with profiler/JIT metadata.
    pub descriptor: RuntimeStubDescriptor,
    /// Machine-callable Rust entrypoint with the descriptor's fixed ABI shape.
    pub entry: LeafNoAllocStub2Fn,
}

impl LeafNoAllocStub2 {
    /// `true` when descriptor metadata matches this callable ABI shape.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        validate_stub_descriptor(self.descriptor, NO_SAFEPOINT)
            && self.descriptor.argument_count == 2
    }

    /// Raw native entry address for generated code.
    #[must_use]
    pub fn entry_addr(self) -> usize {
        self.entry as usize
    }

    /// Invoke this entry with raw ABI bits.
    #[must_use]
    pub fn invoke_raw(
        self,
        heap: *const otter_gc::GcHeap,
        a0_bits: u64,
        a1_bits: u64,
    ) -> RuntimeStubResult {
        (self.entry)(heap, a0_bits, a1_bits)
    }
}

/// Machine-callable fixed-value allocating runtime stub entry shape.
///
/// Generated code supplies the VM-native allocation/rooting context separately
/// from the raw `Value` arguments:
/// `(alloc_ctx, safepoint_id, receiver_bits, arg0_bits, arg1_bits)`.
/// `safepoint_id` must identify a precise map for the current call site.
pub type AllocValueStubFn = extern "C" fn(
    *mut RuntimeStubAllocContext,
    SafepointId,
    u64,
    u64,
    u64,
) -> RuntimeStubResultPair;

/// Fixed-value allocating runtime stub ABI record.
///
/// Generated code supplies the VM-native allocation/rooting context separately
/// from the raw `Value` arguments:
/// `(alloc_ctx, safepoint_id, receiver_bits, arg0_bits, arg1_bits)`.
/// `safepoint_id` must identify a precise map for the current call site.
#[derive(Clone, Copy)]
pub struct AllocValueStub {
    /// Passive descriptor shared with profiler/JIT metadata.
    pub descriptor: RuntimeStubDescriptor,
    /// Machine-callable Rust entrypoint once a concrete stub has a proven
    /// safepoint/rooting implementation.
    pub entry: Option<AllocValueStubFn>,
}

impl AllocValueStub {
    /// `true` when descriptor metadata matches this callable ABI shape for a
    /// concrete allocating call site.
    #[must_use]
    pub const fn is_valid_for_safepoint(self, safepoint: SafepointId) -> bool {
        validate_stub_descriptor(self.descriptor, safepoint) && self.descriptor.argument_count == 3
    }

    /// Whether this ABI record currently has executable machine-call code.
    #[must_use]
    pub const fn has_entry(self) -> bool {
        self.entry.is_some()
    }

    /// Raw native entry address for generated code.
    #[must_use]
    pub fn entry_addr(self) -> Option<usize> {
        self.entry.map(|entry| entry as usize)
    }

    /// Invoke this entry with raw ABI bits when executable code is installed.
    #[must_use]
    pub fn invoke_raw(
        self,
        ctx: *mut RuntimeStubAllocContext,
        safepoint: SafepointId,
        recv_bits: u64,
        arg0_bits: u64,
        arg1_bits: u64,
    ) -> Option<RuntimeStubResultPair> {
        self.entry
            .map(|entry| entry(ctx, safepoint, recv_bits, arg0_bits, arg1_bits))
    }
}

/// Validation failure for publishing an allocating-stub safepoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocSafepointRootError {
    /// Allocating stubs must name a concrete safepoint.
    NoSafepoint,
    /// The context packet does not include a safepoint-record table.
    MissingSafepointRecords,
    /// The supplied safepoint id is not present in the context table.
    UnknownSafepoint {
        /// Requested safepoint id.
        id: SafepointId,
    },
    /// The context packet does not include a frame-slot root window.
    MissingFrameSlots,
    /// The safepoint names a root class this frame-window publisher cannot
    /// trace yet.
    UnsupportedLocation {
        /// Unsupported location class.
        kind: TaggedLocationKind,
        /// Location index from the safepoint map.
        index: u16,
    },
    /// A frame-slot root points outside the context packet's slot window.
    FrameSlotOutOfBounds {
        /// Safepoint frame-slot index.
        index: u16,
        /// Slot count supplied by the context packet.
        frame_slot_count: u16,
    },
    /// A safepoint names a native spill-slot root but the packet exposes no
    /// spill/save-area window.
    MissingSpillSlots,
    /// A spill-slot root points outside the context packet's spill window.
    SpillSlotOutOfBounds {
        /// Safepoint spill-slot index.
        index: u16,
        /// Spill-slot count supplied by the context packet.
        spill_slot_count: u16,
    },
}

/// Resolve an allocating-stub safepoint through the active code registry.
///
/// # Safety
///
/// The thread, code-registry view, resolver context, and returned record must
/// remain alive for the duration of the allocating stub call.
pub unsafe fn alloc_safepoint_record(
    ctx: &RuntimeStubAllocContext,
    safepoint: SafepointId,
) -> Result<&SafepointRecord, AllocSafepointRootError> {
    if safepoint == NO_SAFEPOINT || safepoint != ctx.safepoint_id {
        return Err(AllocSafepointRootError::NoSafepoint);
    }
    if !ctx.has_safepoint_records() {
        return Err(AllocSafepointRootError::MissingSafepointRecords);
    }
    // SAFETY: guaranteed by the caller's published-thread contract.
    let thread = unsafe { &*ctx.thread };
    if thread.code_registry == 0 {
        return Err(AllocSafepointRootError::MissingSafepointRecords);
    }
    // SAFETY: the thread publishes a live CodeRegistryView for this entry.
    let registry = unsafe { *(thread.code_registry as *const CodeRegistryView) };
    // SAFETY: the registry publisher retains its resolver and records for the
    // native entry's dynamic extent.
    let Some(record) = (unsafe { registry.resolve(ctx.code_object_id, safepoint) }) else {
        return Err(AllocSafepointRootError::UnknownSafepoint { id: safepoint });
    };
    // SAFETY: resolver contract above.
    Ok(unsafe { &*record })
}

/// Validate that `safepoint` can be published from `ctx`'s frame-slot window.
///
/// Baseline v1 spills every tagged value live at an allocating collection call
/// into the interpreter-visible register window. Register and native-spill
/// stack maps are intentionally rejected until the machine frame layout can
/// publish those locations directly.
pub fn validate_alloc_safepoint_frame_roots(
    ctx: &RuntimeStubAllocContext,
    safepoint: &SafepointRecord,
) -> Result<(), AllocSafepointRootError> {
    if safepoint.id == NO_SAFEPOINT {
        return Err(AllocSafepointRootError::NoSafepoint);
    }
    if !ctx.has_frame_slots() {
        return Err(AllocSafepointRootError::MissingFrameSlots);
    }
    // SAFETY: `has_frame_slots` verified the published frame pointer/window.
    let frame = unsafe { &*ctx.frame };
    let frame_slot_count = frame.header.register_count;
    for location in &safepoint.tagged_locations {
        match location.kind {
            TaggedLocationKind::FrameSlot => {
                if location.index >= frame_slot_count {
                    return Err(AllocSafepointRootError::FrameSlotOutOfBounds {
                        index: location.index,
                        frame_slot_count,
                    });
                }
            }
            TaggedLocationKind::SpillSlot => {
                if !ctx.has_spill_slots() {
                    return Err(AllocSafepointRootError::MissingSpillSlots);
                }
                if location.index >= ctx.spill_slot_count {
                    return Err(AllocSafepointRootError::SpillSlotOutOfBounds {
                        index: location.index,
                        spill_slot_count: ctx.spill_slot_count,
                    });
                }
            }
            kind => {
                return Err(AllocSafepointRootError::UnsupportedLocation {
                    kind,
                    index: location.index,
                });
            }
        }
    }
    Ok(())
}

/// Root publisher for an allocating runtime-stub safepoint backed by frame slots.
///
/// This type is the VM-native equivalent of the ad hoc native-call root scopes:
/// it exposes the active frame-window slots named by a [`SafepointRecord`] to
/// the moving collector, so a GC can both trace and rewrite those slots while an
/// `Alloc` stub is executing.
pub struct AllocSafepointFrameRoots<'a> {
    ctx: &'a RuntimeStubAllocContext,
    safepoint: &'a SafepointRecord,
}

impl<'a> AllocSafepointFrameRoots<'a> {
    /// Build a frame-slot root publisher for a validated safepoint.
    ///
    /// # Safety
    ///
    /// `ctx.frame` must publish a live, writable tagged register window for the
    /// duration of any heap registration created from this value.
    pub unsafe fn new(
        ctx: &'a RuntimeStubAllocContext,
        safepoint: &'a SafepointRecord,
    ) -> Result<Self, AllocSafepointRootError> {
        // Every record location is bounds-consistent by construction — both tiers
        // build the table from `frame_slot_window(register_count)` and the running
        // frame's slot window is exactly that register count — so the bounds walk
        // is redundant on the per-allocation path. Keep it as a debug assertion;
        // the release path trusts the compiler-emitted table.
        debug_assert!(validate_alloc_safepoint_frame_roots(ctx, safepoint).is_ok());
        Ok(Self { ctx, safepoint })
    }

    /// Safepoint id being published.
    #[must_use]
    pub fn safepoint_id(&self) -> SafepointId {
        self.safepoint.id
    }
}

impl otter_gc::ExtraRootSource for AllocSafepointFrameRoots<'_> {
    fn visit_extra_roots(&self, visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)) {
        for location in &self.safepoint.tagged_locations {
            // SAFETY: construction validated a live published frame.
            let frame = unsafe { &*self.ctx.frame };
            // SAFETY: construction validated every location's storage class and
            // bounds and requires callers to keep the writable frame and spill
            // windows alive while this root source is registered. A moving
            // collector both traces and rewrites the pointer in place through the
            // `&mut Value`, so a call-crossing pointer saved in the native spill
            // area is updated exactly like one held in the interpreter window.
            let base = match location.kind {
                TaggedLocationKind::FrameSlot => {
                    debug_assert!(location.index < frame.header.register_count);
                    frame.register_base as *mut u64
                }
                TaggedLocationKind::SpillSlot => {
                    debug_assert!(location.index < self.ctx.spill_slot_count);
                    self.ctx.spill_slots
                }
                // A machine-register root class is rejected at validation; the
                // register-map safepoint saves the value to a spill slot first.
                TaggedLocationKind::MachineRegister => unreachable!("validated away"),
            };
            let value = unsafe { &mut *(base.add(location.index as usize) as *mut Value) };
            value.trace_value_slot_mut(visitor);
        }
    }
}

/// Root publisher for values passed in ABI registers to an allocating stub.
///
/// The safepoint map publishes the caller's frame slots. This publisher also
/// roots the value copies held by the stub itself, so receiver/arguments remain
/// valid if the stub allocates before it reloads from its local ABI variables.
struct AllocValueStubCallRoots<'a> {
    frame_roots: AllocSafepointFrameRoots<'a>,
    values: [UnsafeCell<Value>; 3],
}

impl<'a> AllocValueStubCallRoots<'a> {
    fn new(frame_roots: AllocSafepointFrameRoots<'a>, values: [Value; 3]) -> Self {
        Self {
            frame_roots,
            values: [
                UnsafeCell::new(values[0]),
                UnsafeCell::new(values[1]),
                UnsafeCell::new(values[2]),
            ],
        }
    }

    fn value(&self, index: usize) -> Value {
        // SAFETY: values are only rewritten by the stop-the-world collector
        // while this root source is synchronously visiting roots.
        unsafe { *self.values[index].get() }
    }
}

impl otter_gc::ExtraRootSource for AllocValueStubCallRoots<'_> {
    fn visit_extra_roots(&self, visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)) {
        self.frame_roots.visit_extra_roots(visitor);
        for value in &self.values {
            // SAFETY: `UnsafeCell` makes these stub-local ABI value copies
            // legitimate mutable root slots for a moving collection.
            unsafe { (&mut *value.get()).trace_value_slot_mut(visitor) };
        }
    }
}

/// Callable ABI entry for `Map.prototype.get`.
pub const COLLECTION_MAP_GET_LEAF: LeafNoAllocStub2 = LeafNoAllocStub2 {
    descriptor: STUB_COLLECTION_MAP_GET_LEAF,
    entry: collection_map_get_leaf,
};

/// Callable ABI entry for `Map.prototype.has`.
pub const COLLECTION_MAP_HAS_LEAF: LeafNoAllocStub2 = LeafNoAllocStub2 {
    descriptor: STUB_COLLECTION_MAP_HAS_LEAF,
    entry: collection_map_has_leaf,
};

/// Callable ABI entry for `Set.prototype.has`.
pub const COLLECTION_SET_HAS_LEAF: LeafNoAllocStub2 = LeafNoAllocStub2 {
    descriptor: STUB_COLLECTION_SET_HAS_LEAF,
    entry: collection_set_has_leaf,
};

/// ABI descriptor for `Map.prototype.set` collection mutation.
pub const COLLECTION_MAP_SET_ALLOC: AllocValueStub = AllocValueStub {
    descriptor: STUB_COLLECTION_MAP_SET_ALLOC,
    entry: Some(collection_map_set_alloc),
};

/// ABI descriptor for `Set.prototype.add` collection mutation.
pub const COLLECTION_SET_ADD_ALLOC: AllocValueStub = AllocValueStub {
    descriptor: STUB_COLLECTION_SET_ADD_ALLOC,
    entry: Some(collection_set_add_alloc),
};

/// ABI descriptor for materializing `Map.prototype.get` collection lookup.
pub const COLLECTION_MAP_GET_ALLOC: AllocValueStub = AllocValueStub {
    descriptor: STUB_COLLECTION_MAP_GET_ALLOC,
    entry: Some(collection_map_get_alloc),
};

/// ABI descriptor for materializing `Map.prototype.has` collection lookup.
pub const COLLECTION_MAP_HAS_ALLOC: AllocValueStub = AllocValueStub {
    descriptor: STUB_COLLECTION_MAP_HAS_ALLOC,
    entry: Some(collection_map_has_alloc),
};

/// ABI descriptor for materializing `Set.prototype.has` collection lookup.
pub const COLLECTION_SET_HAS_ALLOC: AllocValueStub = AllocValueStub {
    descriptor: STUB_COLLECTION_SET_HAS_ALLOC,
    entry: Some(collection_set_has_alloc),
};

/// ABI descriptor for materializing `Map.prototype.delete`.
pub const COLLECTION_MAP_DELETE_ALLOC: AllocValueStub = AllocValueStub {
    descriptor: STUB_COLLECTION_MAP_DELETE_ALLOC,
    entry: Some(collection_map_delete_alloc),
};

/// ABI descriptor for materializing `Set.prototype.delete`.
pub const COLLECTION_SET_DELETE_ALLOC: AllocValueStub = AllocValueStub {
    descriptor: STUB_COLLECTION_SET_DELETE_ALLOC,
    entry: Some(collection_set_delete_alloc),
};

/// ABI descriptor for primitive string concatenation.
pub const STRING_CONCAT_ALLOC: AllocValueStub = AllocValueStub {
    descriptor: STUB_STRING_CONCAT_ALLOC,
    entry: Some(string_concat_alloc),
};

/// Resolve a fixed two-argument leaf/no-allocation stub by ABI descriptor id.
#[must_use]
pub const fn leaf_no_alloc_stub2_by_id(id: RuntimeStubId) -> Option<LeafNoAllocStub2> {
    match id {
        id if id == STUB_COLLECTION_MAP_GET_LEAF.id => Some(COLLECTION_MAP_GET_LEAF),
        id if id == STUB_COLLECTION_MAP_HAS_LEAF.id => Some(COLLECTION_MAP_HAS_LEAF),
        id if id == STUB_COLLECTION_SET_HAS_LEAF.id => Some(COLLECTION_SET_HAS_LEAF),
        _ => None,
    }
}

/// Resolve a fixed-value allocating stub descriptor by ABI descriptor id.
#[must_use]
pub const fn alloc_value_stub_by_id(id: RuntimeStubId) -> Option<AllocValueStub> {
    match id {
        id if id == STUB_COLLECTION_MAP_SET_ALLOC.id => Some(COLLECTION_MAP_SET_ALLOC),
        id if id == STUB_COLLECTION_SET_ADD_ALLOC.id => Some(COLLECTION_SET_ADD_ALLOC),
        id if id == STUB_COLLECTION_MAP_GET_ALLOC.id => Some(COLLECTION_MAP_GET_ALLOC),
        id if id == STUB_COLLECTION_MAP_HAS_ALLOC.id => Some(COLLECTION_MAP_HAS_ALLOC),
        id if id == STUB_COLLECTION_SET_HAS_ALLOC.id => Some(COLLECTION_SET_HAS_ALLOC),
        id if id == STUB_COLLECTION_MAP_DELETE_ALLOC.id => Some(COLLECTION_MAP_DELETE_ALLOC),
        id if id == STUB_COLLECTION_SET_DELETE_ALLOC.id => Some(COLLECTION_SET_DELETE_ALLOC),
        id if id == STUB_STRING_CONCAT_ALLOC.id => Some(STRING_CONCAT_ALLOC),
        _ => None,
    }
}

/// Invoke a fixed two-argument leaf/no-allocation stub by ABI descriptor id.
///
/// This is the reusable VM-side equivalent of the machine-code call sequence a
/// native tier will eventually emit directly: resolve descriptor id, pass raw
/// boxed value bits, receive a fixed [`RuntimeStubResult`]. It intentionally
/// takes no root scope or safepoint because the descriptor class is
/// `LeafNoAlloc`.
#[must_use]
pub fn invoke_leaf_no_alloc_stub2(
    heap: &otter_gc::GcHeap,
    id: RuntimeStubId,
    a0: Value,
    a1: Value,
) -> RuntimeStubResult {
    leaf_no_alloc_stub2_trampoline(
        heap as *const otter_gc::GcHeap,
        id,
        a0.to_abi_bits(),
        a1.to_abi_bits(),
    )
}

/// Generic native trampoline for fixed two-argument leaf/no-allocation stubs.
///
/// Generated code can call this while it still carries a dynamic
/// [`RuntimeStubId`] in feedback. A later codegen slice can resolve the id at
/// compile time and call [`LeafNoAllocStub2::entry_addr`] directly.
#[must_use]
pub extern "C" fn leaf_no_alloc_stub2_trampoline(
    heap: *const otter_gc::GcHeap,
    id: RuntimeStubId,
    a0_bits: u64,
    a1_bits: u64,
) -> RuntimeStubResult {
    let Some(stub) = leaf_no_alloc_stub2_by_id(id) else {
        return RuntimeStubResult::miss();
    };
    stub.invoke_raw(heap, a0_bits, a1_bits)
}

/// Two-register result variant of [`leaf_no_alloc_stub2_trampoline`].
#[must_use]
pub extern "C" fn leaf_no_alloc_stub2_trampoline_pair(
    heap: *const otter_gc::GcHeap,
    id: RuntimeStubId,
    a0_bits: u64,
    a1_bits: u64,
) -> RuntimeStubResultPair {
    RuntimeStubResultPair::from_result(leaf_no_alloc_stub2_trampoline(heap, id, a0_bits, a1_bits))
}

/// Dynamic trampoline for fixed-value allocating runtime stubs.
#[must_use]
pub extern "C" fn alloc_value_stub_trampoline_pair(
    ctx: *mut RuntimeStubAllocContext,
    id: RuntimeStubId,
    safepoint: SafepointId,
    recv_bits: u64,
    arg0_bits: u64,
    arg1_bits: u64,
) -> RuntimeStubResultPair {
    let Some(stub) = alloc_value_stub_by_id(id) else {
        return RuntimeStubResultPair::from_result(RuntimeStubResult::miss());
    };
    stub.invoke_raw(ctx, safepoint, recv_bits, arg0_bits, arg1_bits)
        .unwrap_or_else(|| RuntimeStubResultPair::from_result(RuntimeStubResult::miss()))
}

fn alloc_value_stub_result_pair(
    ctx: *mut RuntimeStubAllocContext,
    result: RuntimeStubResult,
) -> RuntimeStubResultPair {
    if let Some(ctx) = alloc_context_mut(ctx)
        && let Some(interp) = alloc_interpreter_mut(ctx)
    {
        interp.record_jit_alloc_value_stub_status(result.status);
    }
    RuntimeStubResultPair::from_result(result)
}

/// Allocating `Map.prototype.set` mutation stub.
///
/// This entry roots the caller frame through `safepoint` and roots its own ABI
/// value copies before flattening string keys or mutating the collection.
#[must_use]
pub extern "C" fn collection_map_set_alloc(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    recv_bits: u64,
    key_bits: u64,
    value_bits: u64,
) -> RuntimeStubResultPair {
    alloc_value_stub_result_pair(
        ctx,
        collection_map_set_alloc_inner(ctx, safepoint, recv_bits, key_bits, value_bits),
    )
}

/// Allocating `Set.prototype.add` mutation stub.
#[must_use]
pub extern "C" fn collection_set_add_alloc(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    recv_bits: u64,
    value_bits: u64,
    unused_bits: u64,
) -> RuntimeStubResultPair {
    alloc_value_stub_result_pair(
        ctx,
        collection_set_add_alloc_inner(ctx, safepoint, recv_bits, value_bits, unused_bits),
    )
}

/// Allocating `Map.prototype.get` lookup stub.
#[must_use]
pub extern "C" fn collection_map_get_alloc(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    recv_bits: u64,
    key_bits: u64,
    unused_bits: u64,
) -> RuntimeStubResultPair {
    alloc_value_stub_result_pair(
        ctx,
        collection_map_get_alloc_inner(ctx, safepoint, recv_bits, key_bits, unused_bits),
    )
}

/// Allocating `Map.prototype.has` lookup stub.
#[must_use]
pub extern "C" fn collection_map_has_alloc(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    recv_bits: u64,
    key_bits: u64,
    unused_bits: u64,
) -> RuntimeStubResultPair {
    alloc_value_stub_result_pair(
        ctx,
        collection_map_has_alloc_inner(ctx, safepoint, recv_bits, key_bits, unused_bits),
    )
}

/// Allocating `Set.prototype.has` lookup stub.
#[must_use]
pub extern "C" fn collection_set_has_alloc(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    recv_bits: u64,
    value_bits: u64,
    unused_bits: u64,
) -> RuntimeStubResultPair {
    alloc_value_stub_result_pair(
        ctx,
        collection_set_has_alloc_inner(ctx, safepoint, recv_bits, value_bits, unused_bits),
    )
}

/// Allocating `Map.prototype.delete` mutation stub.
#[must_use]
pub extern "C" fn collection_map_delete_alloc(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    recv_bits: u64,
    key_bits: u64,
    unused_bits: u64,
) -> RuntimeStubResultPair {
    alloc_value_stub_result_pair(
        ctx,
        collection_map_delete_alloc_inner(ctx, safepoint, recv_bits, key_bits, unused_bits),
    )
}

/// Allocating `Set.prototype.delete` mutation stub.
#[must_use]
pub extern "C" fn collection_set_delete_alloc(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    recv_bits: u64,
    value_bits: u64,
    unused_bits: u64,
) -> RuntimeStubResultPair {
    alloc_value_stub_result_pair(
        ctx,
        collection_set_delete_alloc_inner(ctx, safepoint, recv_bits, value_bits, unused_bits),
    )
}

/// Allocating primitive string-concat stub for `+`.
#[must_use]
pub extern "C" fn string_concat_alloc(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    lhs_bits: u64,
    rhs_bits: u64,
    unused_bits: u64,
) -> RuntimeStubResultPair {
    alloc_value_stub_result_pair(
        ctx,
        string_concat_alloc_inner(ctx, safepoint, lhs_bits, rhs_bits, unused_bits),
    )
}

/// Leaf `Map.prototype.get` probe.
///
/// Returns `Miss` when the receiver is not a Map or the key would need string
/// materialisation/flattening before a no-GC lookup is safe.
#[must_use]
pub extern "C" fn collection_map_get_leaf(
    heap: *const otter_gc::GcHeap,
    recv_bits: u64,
    key_bits: u64,
) -> RuntimeStubResult {
    let Some(heap) = heap_ref(heap) else {
        return RuntimeStubResult::miss();
    };
    let recv = Value::from_abi_bits(recv_bits);
    let key = Value::from_abi_bits(key_bits);
    if !leaf_key_is_materialized(heap, key) {
        return RuntimeStubResult::miss();
    }
    let Some(map) = recv.as_map() else {
        return RuntimeStubResult::miss();
    };
    RuntimeStubResult::ok_value(
        collections::map_get(map, heap, &key).unwrap_or_else(Value::undefined),
    )
}

/// Leaf `Map.prototype.has` probe.
///
/// Returns `Miss` when the receiver is not a Map or the key would need string
/// materialisation/flattening before a no-GC lookup is safe.
#[must_use]
pub extern "C" fn collection_map_has_leaf(
    heap: *const otter_gc::GcHeap,
    recv_bits: u64,
    key_bits: u64,
) -> RuntimeStubResult {
    let Some(heap) = heap_ref(heap) else {
        return RuntimeStubResult::miss();
    };
    let recv = Value::from_abi_bits(recv_bits);
    let key = Value::from_abi_bits(key_bits);
    if !leaf_key_is_materialized(heap, key) {
        return RuntimeStubResult::miss();
    }
    let Some(map) = recv.as_map() else {
        return RuntimeStubResult::miss();
    };
    RuntimeStubResult::ok_value(Value::boolean(collections::map_has(map, heap, &key)))
}

/// Leaf `Set.prototype.has` probe.
///
/// Returns `Miss` when the receiver is not a Set or the key would need string
/// materialisation/flattening before a no-GC lookup is safe.
#[must_use]
pub extern "C" fn collection_set_has_leaf(
    heap: *const otter_gc::GcHeap,
    recv_bits: u64,
    key_bits: u64,
) -> RuntimeStubResult {
    let Some(heap) = heap_ref(heap) else {
        return RuntimeStubResult::miss();
    };
    let recv = Value::from_abi_bits(recv_bits);
    let key = Value::from_abi_bits(key_bits);
    if !leaf_key_is_materialized(heap, key) {
        return RuntimeStubResult::miss();
    }
    let Some(set) = recv.as_set() else {
        return RuntimeStubResult::miss();
    };
    RuntimeStubResult::ok_value(Value::boolean(collections::set_has(set, heap, &key)))
}

fn collection_map_set_alloc_inner(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    recv_bits: u64,
    key_bits: u64,
    value_bits: u64,
) -> RuntimeStubResult {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return RuntimeStubResult::miss();
    };
    let Some(interp) = alloc_interpreter_mut(ctx) else {
        return RuntimeStubResult::miss();
    };
    // SAFETY: `ctx` is the current allocating-stub call packet. Its safepoint
    // table and frame-slot window must remain live for this call.
    let Ok(roots) = (unsafe {
        alloc_value_stub_call_roots(
            ctx,
            safepoint,
            [
                Value::from_abi_bits(recv_bits),
                Value::from_abi_bits(key_bits),
                Value::from_abi_bits(value_bits),
            ],
        )
    }) else {
        return RuntimeStubResult::miss();
    };
    let _roots_guard = interp
        .gc_heap
        .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
    (|| {
        let key = roots.value(1);
        if let Some(string) = key.as_string(&interp.gc_heap) {
            let _ = string.flatten_in_place(&mut interp.gc_heap);
        }
        let recv = roots.value(0);
        let key = roots.value(1);
        let value = roots.value(2);
        let Some(map) = recv.as_map() else {
            return RuntimeStubResult::miss();
        };
        match collections::map_set(map, &mut interp.gc_heap, key, value) {
            Ok(()) => RuntimeStubResult::ok_value(roots.value(0)),
            Err(_) => RuntimeStubResult::out_of_memory(),
        }
    })()
}

fn collection_set_add_alloc_inner(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    recv_bits: u64,
    value_bits: u64,
    unused_bits: u64,
) -> RuntimeStubResult {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return RuntimeStubResult::miss();
    };
    let Some(interp) = alloc_interpreter_mut(ctx) else {
        return RuntimeStubResult::miss();
    };
    // SAFETY: `ctx` is the current allocating-stub call packet. Its safepoint
    // table and frame-slot window must remain live for this call.
    let Ok(roots) = (unsafe {
        alloc_value_stub_call_roots(
            ctx,
            safepoint,
            [
                Value::from_abi_bits(recv_bits),
                Value::from_abi_bits(value_bits),
                Value::from_abi_bits(unused_bits),
            ],
        )
    }) else {
        return RuntimeStubResult::miss();
    };
    let _roots_guard = interp
        .gc_heap
        .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
    (|| {
        let value = roots.value(1);
        if let Some(string) = value.as_string(&interp.gc_heap) {
            let _ = string.flatten_in_place(&mut interp.gc_heap);
        }
        let recv = roots.value(0);
        let value = roots.value(1);
        let Some(set) = recv.as_set() else {
            return RuntimeStubResult::miss();
        };
        match collections::set_add(set, &mut interp.gc_heap, value) {
            Ok(()) => RuntimeStubResult::ok_value(roots.value(0)),
            Err(_) => RuntimeStubResult::out_of_memory(),
        }
    })()
}

fn collection_map_get_alloc_inner(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    recv_bits: u64,
    key_bits: u64,
    unused_bits: u64,
) -> RuntimeStubResult {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return RuntimeStubResult::miss();
    };
    let Some(interp) = alloc_interpreter_mut(ctx) else {
        return RuntimeStubResult::miss();
    };
    // SAFETY: `ctx` is the current allocating-stub call packet. Its safepoint
    // table and frame-slot window must remain live for this call.
    let Ok(roots) = (unsafe {
        alloc_value_stub_call_roots(
            ctx,
            safepoint,
            [
                Value::from_abi_bits(recv_bits),
                Value::from_abi_bits(key_bits),
                Value::from_abi_bits(unused_bits),
            ],
        )
    }) else {
        return RuntimeStubResult::miss();
    };
    let _roots_guard = interp
        .gc_heap
        .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
    (|| {
        let key = roots.value(1);
        if let Some(string) = key.as_string(&interp.gc_heap) {
            let _ = string.flatten_in_place(&mut interp.gc_heap);
        }
        let recv = roots.value(0);
        let key = roots.value(1);
        let Some(map) = recv.as_map() else {
            return RuntimeStubResult::miss();
        };
        RuntimeStubResult::ok_value(
            collections::map_get(map, &interp.gc_heap, &key).unwrap_or_else(Value::undefined),
        )
    })()
}

fn collection_map_has_alloc_inner(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    recv_bits: u64,
    key_bits: u64,
    unused_bits: u64,
) -> RuntimeStubResult {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return RuntimeStubResult::miss();
    };
    let Some(interp) = alloc_interpreter_mut(ctx) else {
        return RuntimeStubResult::miss();
    };
    // SAFETY: `ctx` is the current allocating-stub call packet. Its safepoint
    // table and frame-slot window must remain live for this call.
    let Ok(roots) = (unsafe {
        alloc_value_stub_call_roots(
            ctx,
            safepoint,
            [
                Value::from_abi_bits(recv_bits),
                Value::from_abi_bits(key_bits),
                Value::from_abi_bits(unused_bits),
            ],
        )
    }) else {
        return RuntimeStubResult::miss();
    };
    let _roots_guard = interp
        .gc_heap
        .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
    (|| {
        let key = roots.value(1);
        if let Some(string) = key.as_string(&interp.gc_heap) {
            let _ = string.flatten_in_place(&mut interp.gc_heap);
        }
        let recv = roots.value(0);
        let key = roots.value(1);
        let Some(map) = recv.as_map() else {
            return RuntimeStubResult::miss();
        };
        RuntimeStubResult::ok_value(Value::boolean(collections::map_has(
            map,
            &interp.gc_heap,
            &key,
        )))
    })()
}

fn collection_set_has_alloc_inner(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    recv_bits: u64,
    value_bits: u64,
    unused_bits: u64,
) -> RuntimeStubResult {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return RuntimeStubResult::miss();
    };
    let Some(interp) = alloc_interpreter_mut(ctx) else {
        return RuntimeStubResult::miss();
    };
    // SAFETY: `ctx` is the current allocating-stub call packet. Its safepoint
    // table and frame-slot window must remain live for this call.
    let Ok(roots) = (unsafe {
        alloc_value_stub_call_roots(
            ctx,
            safepoint,
            [
                Value::from_abi_bits(recv_bits),
                Value::from_abi_bits(value_bits),
                Value::from_abi_bits(unused_bits),
            ],
        )
    }) else {
        return RuntimeStubResult::miss();
    };
    let _roots_guard = interp
        .gc_heap
        .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
    (|| {
        let value = roots.value(1);
        if let Some(string) = value.as_string(&interp.gc_heap) {
            let _ = string.flatten_in_place(&mut interp.gc_heap);
        }
        let recv = roots.value(0);
        let value = roots.value(1);
        let Some(set) = recv.as_set() else {
            return RuntimeStubResult::miss();
        };
        RuntimeStubResult::ok_value(Value::boolean(collections::set_has(
            set,
            &interp.gc_heap,
            &value,
        )))
    })()
}

fn collection_map_delete_alloc_inner(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    recv_bits: u64,
    key_bits: u64,
    unused_bits: u64,
) -> RuntimeStubResult {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return RuntimeStubResult::miss();
    };
    let Some(interp) = alloc_interpreter_mut(ctx) else {
        return RuntimeStubResult::miss();
    };
    // SAFETY: `ctx` is the current allocating-stub call packet. Its safepoint
    // table and frame-slot window must remain live for this call.
    let Ok(roots) = (unsafe {
        alloc_value_stub_call_roots(
            ctx,
            safepoint,
            [
                Value::from_abi_bits(recv_bits),
                Value::from_abi_bits(key_bits),
                Value::from_abi_bits(unused_bits),
            ],
        )
    }) else {
        return RuntimeStubResult::miss();
    };
    let _roots_guard = interp
        .gc_heap
        .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
    (|| {
        let key = roots.value(1);
        if let Some(string) = key.as_string(&interp.gc_heap) {
            let _ = string.flatten_in_place(&mut interp.gc_heap);
        }
        let recv = roots.value(0);
        let key = roots.value(1);
        let Some(map) = recv.as_map() else {
            return RuntimeStubResult::miss();
        };
        RuntimeStubResult::ok_value(Value::boolean(collections::map_delete(
            map,
            &mut interp.gc_heap,
            &key,
        )))
    })()
}

fn collection_set_delete_alloc_inner(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    recv_bits: u64,
    value_bits: u64,
    unused_bits: u64,
) -> RuntimeStubResult {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return RuntimeStubResult::miss();
    };
    let Some(interp) = alloc_interpreter_mut(ctx) else {
        return RuntimeStubResult::miss();
    };
    // SAFETY: `ctx` is the current allocating-stub call packet. Its safepoint
    // table and frame-slot window must remain live for this call.
    let Ok(roots) = (unsafe {
        alloc_value_stub_call_roots(
            ctx,
            safepoint,
            [
                Value::from_abi_bits(recv_bits),
                Value::from_abi_bits(value_bits),
                Value::from_abi_bits(unused_bits),
            ],
        )
    }) else {
        return RuntimeStubResult::miss();
    };
    let _roots_guard = interp
        .gc_heap
        .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
    (|| {
        let value = roots.value(1);
        if let Some(string) = value.as_string(&interp.gc_heap) {
            let _ = string.flatten_in_place(&mut interp.gc_heap);
        }
        let recv = roots.value(0);
        let value = roots.value(1);
        let Some(set) = recv.as_set() else {
            return RuntimeStubResult::miss();
        };
        RuntimeStubResult::ok_value(Value::boolean(collections::set_delete(
            set,
            &mut interp.gc_heap,
            &value,
        )))
    })()
}

fn string_concat_alloc_inner(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    lhs_bits: u64,
    rhs_bits: u64,
    unused_bits: u64,
) -> RuntimeStubResult {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return RuntimeStubResult::miss();
    };
    let Some(interp) = alloc_interpreter_mut(ctx) else {
        return RuntimeStubResult::miss();
    };
    // One-allocation fast path for `<short flat latin1 string> + <int32>` and
    // its mirror — the common key-building shape (`"k" + n`), skipping the
    // general path's number-string / cons-rope / flatten allocations. Shared
    // with the interpreter's `Op::Add` string path.
    if let Some(fast) = interp.try_concat_string_int32(
        Value::from_abi_bits(lhs_bits),
        Value::from_abi_bits(rhs_bits),
    ) {
        return match fast {
            Ok(value) => RuntimeStubResult::ok_value(value),
            Err(_) => RuntimeStubResult::out_of_memory(),
        };
    }
    // SAFETY: `ctx` is the current allocating-stub call packet. Its safepoint
    // table and frame-slot window must remain live for this call.
    let Ok(roots) = (unsafe {
        alloc_value_stub_call_roots(
            ctx,
            safepoint,
            [
                Value::from_abi_bits(lhs_bits),
                Value::from_abi_bits(rhs_bits),
                Value::from_abi_bits(unused_bits),
            ],
        )
    }) else {
        return RuntimeStubResult::miss();
    };
    let _roots_guard = interp
        .gc_heap
        .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
    (|| {
        let lhs = roots.value(0);
        let rhs = roots.value(1);
        if lhs.as_string(&interp.gc_heap).is_none() && rhs.as_string(&interp.gc_heap).is_none() {
            return RuntimeStubResult::miss();
        }
        let Ok(lhs_string) = (if let Some(string) = lhs.as_string(&interp.gc_heap) {
            Ok(string)
        } else {
            interp.js_string_for_concat(lhs)
        }) else {
            return RuntimeStubResult::miss();
        };
        let Ok(rhs_string) = (if let Some(string) = rhs.as_string(&interp.gc_heap) {
            Ok(string)
        } else {
            interp.js_string_for_concat(rhs)
        }) else {
            return RuntimeStubResult::miss();
        };
        match crate::string::JsString::concat(lhs_string, rhs_string, &mut interp.gc_heap) {
            Ok(result) => RuntimeStubResult::ok_value(Value::string(result)),
            Err(_) => RuntimeStubResult::out_of_memory(),
        }
    })()
}

fn heap_ref(heap: *const otter_gc::GcHeap) -> Option<&'static otter_gc::GcHeap> {
    if heap.is_null() {
        return None;
    }
    // SAFETY: runtime stub callers pass the current isolate heap pointer and
    // leaf stubs neither allocate nor retain it. The returned reference is used
    // only for this call.
    Some(unsafe { &*heap })
}

fn alloc_context_mut(
    ctx: *mut RuntimeStubAllocContext,
) -> Option<&'static mut RuntimeStubAllocContext> {
    if ctx.is_null() {
        return None;
    }
    // SAFETY: allocating-stub callers pass a live context packet for the
    // duration of the call and the stub never retains this reference.
    Some(unsafe { &mut *ctx })
}

fn interpreter_mut(vm: *mut std::ffi::c_void) -> Option<&'static mut Interpreter> {
    if vm.is_null() {
        return None;
    }
    // SAFETY: `RuntimeStubAllocContext.vm` is the current isolate
    // `Interpreter` pointer. The stub executes synchronously on the mutator
    // thread and does not retain the reference.
    Some(unsafe { &mut *(vm as *mut Interpreter) })
}

fn alloc_interpreter_mut(ctx: &RuntimeStubAllocContext) -> Option<&'static mut Interpreter> {
    if ctx.thread.is_null() {
        return None;
    }
    // SAFETY: allocating stubs execute synchronously while the JIT entry keeps
    // both the VmThread and its VM-owned reentry record live.
    let thread = unsafe { &*ctx.thread };
    if thread.runtime_context == 0 {
        return None;
    }
    // SAFETY: `runtime_context` is published from a live JitReentryPtrs value.
    let reentry = unsafe { &*(thread.runtime_context as *const crate::jit::JitReentryPtrs) };
    interpreter_mut(reentry.vm.cast())
}

unsafe fn alloc_value_stub_call_roots<'a>(
    ctx: &'a RuntimeStubAllocContext,
    safepoint: SafepointId,
    values: [Value; 3],
) -> Result<AllocValueStubCallRoots<'a>, AllocSafepointRootError> {
    // SAFETY: forwarded from this helper's caller.
    let record = unsafe { alloc_safepoint_record(ctx, safepoint)? };
    // SAFETY: forwarded from this helper's caller.
    let frame_roots = unsafe { AllocSafepointFrameRoots::new(ctx, record)? };
    Ok(AllocValueStubCallRoots::new(frame_roots, values))
}

fn leaf_key_is_materialized(heap: &otter_gc::GcHeap, key: Value) -> bool {
    key.as_string(heap)
        .is_none_or(|string| string.is_flat_or_latin1(heap))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_abi::{
        NO_FRAME_STATE, NativeFrame, NativeFrameFlags, NativeFrameHeader, NativeFrameKind,
        RuntimeStubStatus, TaggedLocation, TaggedLocationKind, VmThread,
    };
    use otter_gc::ExtraRootSource;

    fn n(i: i32) -> Value {
        Value::number_i32(i)
    }

    fn young_object_value(heap: &mut otter_gc::GcHeap) -> Value {
        let mut no_roots = |_visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {};
        Value::object(crate::object::alloc_object_with_roots(heap, &mut no_roots).unwrap())
    }

    #[repr(C)]
    struct TestSafepoints {
        records: *const SafepointRecord,
        count: u32,
    }

    unsafe extern "C" fn resolve_test_safepoint(
        context: u64,
        code_object_id: u64,
        safepoint_id: SafepointId,
    ) -> *const SafepointRecord {
        if context == 0 || code_object_id != 1 {
            return std::ptr::null();
        }
        // SAFETY: test_alloc_context leaks this bounded fixture for the test process.
        let active = unsafe { &*(context as *const TestSafepoints) };
        let records = unsafe { std::slice::from_raw_parts(active.records, active.count as usize) };
        records
            .iter()
            .find(|record| record.id == safepoint_id)
            .map_or(std::ptr::null(), std::ptr::from_ref)
    }

    fn test_alloc_context(
        vm: *mut Interpreter,
        slots: &mut [u64],
        records: &[SafepointRecord],
        safepoint_id: SafepointId,
    ) -> RuntimeStubAllocContext {
        let records: &'static [SafepointRecord] = Box::leak(records.to_vec().into_boxed_slice());
        let active = Box::leak(Box::new(TestSafepoints {
            records: records.as_ptr(),
            count: records.len() as u32,
        }));
        let registry = Box::leak(Box::new(CodeRegistryView {
            context: std::ptr::from_ref(active) as u64,
            resolve_safepoint: resolve_test_safepoint as *const () as u64,
        }));
        let reentry = Box::leak(Box::new(crate::jit::JitReentryPtrs {
            vm,
            stack: std::ptr::null_mut(),
            context: std::ptr::null(),
            frame_index: 0,
        }));
        let frame = Box::leak(Box::new(NativeFrame {
            header: NativeFrameHeader {
                previous_frame: 0,
                function_id: 0,
                code_block_id: 0,
                resume_pc: 0,
                kind: NativeFrameKind::Baseline,
                reserved0: [0; 3],
                flags: NativeFrameFlags::from_bits(NativeFrameFlags::HAS_SAFEPOINTS),
                register_count: slots.len() as u16,
                argument_count: 0,
                feedback_id: 0,
            },
            register_base: slots.as_mut_ptr() as u64,
            argument_base: 0,
            feedback_base: 0,
            code_object_id: 1,
            this_value_bits: Value::undefined().to_abi_bits(),
            new_target_bits: Value::undefined().to_abi_bits(),
            return_register: u32::MAX,
            cold_state_index: u32::MAX,
        }));
        let mut thread = VmThread::empty();
        thread.current_frame = std::ptr::from_mut(frame) as u64;
        thread.runtime_context = std::ptr::from_ref(reentry) as u64;
        thread.code_registry = std::ptr::from_ref(registry) as u64;
        let thread = Box::leak(Box::new(thread));
        RuntimeStubAllocContext::new(thread, frame, 1, safepoint_id)
    }

    #[test]
    fn leaf_stub_entries_match_descriptors() {
        assert!(COLLECTION_MAP_GET_LEAF.is_valid());
        assert!(COLLECTION_MAP_HAS_LEAF.is_valid());
        assert!(COLLECTION_SET_HAS_LEAF.is_valid());
        assert_eq!(
            leaf_no_alloc_stub2_by_id(STUB_COLLECTION_MAP_GET_LEAF.id).map(|stub| stub.descriptor),
            Some(STUB_COLLECTION_MAP_GET_LEAF)
        );
        assert!(leaf_no_alloc_stub2_by_id(u32::MAX).is_none());
    }

    #[test]
    fn alloc_stub_descriptors_require_safepoints() {
        assert!(!COLLECTION_MAP_SET_ALLOC.is_valid_for_safepoint(NO_SAFEPOINT));
        assert!(COLLECTION_MAP_SET_ALLOC.is_valid_for_safepoint(1));
        assert!(COLLECTION_MAP_SET_ALLOC.has_entry());
        assert!(COLLECTION_MAP_SET_ALLOC.entry_addr().is_some());
        assert!(!COLLECTION_SET_ADD_ALLOC.is_valid_for_safepoint(NO_SAFEPOINT));
        assert!(COLLECTION_SET_ADD_ALLOC.is_valid_for_safepoint(1));
        assert!(COLLECTION_SET_ADD_ALLOC.has_entry());
        assert!(COLLECTION_SET_ADD_ALLOC.entry_addr().is_some());
        assert!(!COLLECTION_MAP_GET_ALLOC.is_valid_for_safepoint(NO_SAFEPOINT));
        assert!(COLLECTION_MAP_GET_ALLOC.is_valid_for_safepoint(1));
        assert!(COLLECTION_MAP_GET_ALLOC.has_entry());
        assert!(COLLECTION_MAP_GET_ALLOC.entry_addr().is_some());
        assert!(!COLLECTION_MAP_HAS_ALLOC.is_valid_for_safepoint(NO_SAFEPOINT));
        assert!(COLLECTION_MAP_HAS_ALLOC.is_valid_for_safepoint(1));
        assert!(COLLECTION_MAP_HAS_ALLOC.has_entry());
        assert!(COLLECTION_MAP_HAS_ALLOC.entry_addr().is_some());
        assert!(!COLLECTION_SET_HAS_ALLOC.is_valid_for_safepoint(NO_SAFEPOINT));
        assert!(COLLECTION_SET_HAS_ALLOC.is_valid_for_safepoint(1));
        assert!(COLLECTION_SET_HAS_ALLOC.has_entry());
        assert!(COLLECTION_SET_HAS_ALLOC.entry_addr().is_some());
        assert!(!COLLECTION_MAP_DELETE_ALLOC.is_valid_for_safepoint(NO_SAFEPOINT));
        assert!(COLLECTION_MAP_DELETE_ALLOC.is_valid_for_safepoint(1));
        assert!(COLLECTION_MAP_DELETE_ALLOC.has_entry());
        assert!(COLLECTION_MAP_DELETE_ALLOC.entry_addr().is_some());
        assert!(!COLLECTION_SET_DELETE_ALLOC.is_valid_for_safepoint(NO_SAFEPOINT));
        assert!(COLLECTION_SET_DELETE_ALLOC.is_valid_for_safepoint(1));
        assert!(COLLECTION_SET_DELETE_ALLOC.has_entry());
        assert!(COLLECTION_SET_DELETE_ALLOC.entry_addr().is_some());
        assert!(!STRING_CONCAT_ALLOC.is_valid_for_safepoint(NO_SAFEPOINT));
        assert!(STRING_CONCAT_ALLOC.is_valid_for_safepoint(1));
        assert!(STRING_CONCAT_ALLOC.has_entry());
        assert!(STRING_CONCAT_ALLOC.entry_addr().is_some());
        assert_eq!(
            alloc_value_stub_by_id(STUB_COLLECTION_MAP_SET_ALLOC.id).map(|stub| stub.descriptor),
            Some(STUB_COLLECTION_MAP_SET_ALLOC)
        );
        assert_eq!(
            alloc_value_stub_by_id(STUB_COLLECTION_SET_ADD_ALLOC.id).map(|stub| stub.descriptor),
            Some(STUB_COLLECTION_SET_ADD_ALLOC)
        );
        assert_eq!(
            alloc_value_stub_by_id(STUB_COLLECTION_MAP_GET_ALLOC.id).map(|stub| stub.descriptor),
            Some(STUB_COLLECTION_MAP_GET_ALLOC)
        );
        assert_eq!(
            alloc_value_stub_by_id(STUB_COLLECTION_MAP_HAS_ALLOC.id).map(|stub| stub.descriptor),
            Some(STUB_COLLECTION_MAP_HAS_ALLOC)
        );
        assert_eq!(
            alloc_value_stub_by_id(STUB_COLLECTION_SET_HAS_ALLOC.id).map(|stub| stub.descriptor),
            Some(STUB_COLLECTION_SET_HAS_ALLOC)
        );
        assert_eq!(
            alloc_value_stub_by_id(STUB_COLLECTION_MAP_DELETE_ALLOC.id).map(|stub| stub.descriptor),
            Some(STUB_COLLECTION_MAP_DELETE_ALLOC)
        );
        assert_eq!(
            alloc_value_stub_by_id(STUB_COLLECTION_SET_DELETE_ALLOC.id).map(|stub| stub.descriptor),
            Some(STUB_COLLECTION_SET_DELETE_ALLOC)
        );
        assert_eq!(
            alloc_value_stub_by_id(STUB_STRING_CONCAT_ALLOC.id).map(|stub| stub.descriptor),
            Some(STUB_STRING_CONCAT_ALLOC)
        );
        assert!(alloc_value_stub_by_id(u32::MAX).is_none());
    }

    #[test]
    fn alloc_value_stub_fn_uses_alloc_context_and_pair_result() {
        extern "C" fn probe(
            ctx: *mut RuntimeStubAllocContext,
            safepoint: SafepointId,
            recv_bits: u64,
            arg0_bits: u64,
            arg1_bits: u64,
        ) -> RuntimeStubResultPair {
            if ctx.is_null() || safepoint != 9 || recv_bits != 1 || arg0_bits != 2 || arg1_bits != 3
            {
                return RuntimeStubResultPair::from_result(RuntimeStubResult::miss());
            }
            RuntimeStubResultPair::from_result(RuntimeStubResult::ok_bits(recv_bits))
        }

        let entry: AllocValueStubFn = probe;
        let stub = AllocValueStub {
            descriptor: STUB_COLLECTION_MAP_SET_ALLOC,
            entry: Some(entry),
        };
        let mut slots = [Value::undefined().to_abi_bits()];
        let mut ctx = test_alloc_context(std::ptr::null_mut(), &mut slots, &[], 9);
        assert!(stub.has_entry());
        assert!(stub.entry_addr().is_some());
        let result = stub
            .invoke_raw(&mut ctx, 9, 1, 2, 3)
            .expect("executable alloc stub");
        assert_eq!(result.status(), RuntimeStubStatus::Ok);
        assert_eq!(result.value_bits, 1);
    }

    #[test]
    fn alloc_safepoint_frame_roots_publish_value_slots() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let map = collections::alloc_map(&mut heap).expect("map");
        let mut slots = [Value::map(map).to_abi_bits(), n(7).to_abi_bits()];
        let safepoint = SafepointRecord {
            id: 12,
            frame_state: NO_FRAME_STATE,
            tagged_locations: vec![TaggedLocation::frame_slot(0), TaggedLocation::frame_slot(1)],
        };
        let safepoints = [safepoint.clone()];
        let ctx = test_alloc_context(std::ptr::null_mut(), &mut slots, &safepoints, 12);

        assert_eq!(
            validate_alloc_safepoint_frame_roots(&ctx, &safepoint),
            Ok(())
        );
        // SAFETY: `safepoints` is alive for the lookup.
        assert_eq!(
            unsafe { alloc_safepoint_record(&ctx, 12) },
            Ok(&safepoints[0])
        );
        // SAFETY: `slots` is a live writable `Value` bit window for the root
        // publisher's full lifetime.
        let roots = unsafe { AllocSafepointFrameRoots::new(&ctx, &safepoint) }.expect("roots");
        assert_eq!(roots.safepoint_id(), 12);

        let mut seen = Vec::new();
        roots.visit_extra_roots(&mut |slot| seen.push(slot));
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0], slots.as_mut_ptr().cast::<otter_gc::raw::RawGc>());
    }

    #[test]
    fn alloc_value_stub_roots_survive_minor_relocation() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let frame_value = young_object_value(&mut heap);
        let arg_value = young_object_value(&mut heap);
        let frame_before = frame_value.as_raw_gc().expect("frame raw");
        let arg_before = arg_value.as_raw_gc().expect("arg raw");

        let safepoint = SafepointRecord::frame_slot_window(31, NO_FRAME_STATE, 1);
        let safepoints = [safepoint.clone()];
        let mut slots = [frame_value.to_abi_bits()];
        let ctx = test_alloc_context(std::ptr::null_mut(), &mut slots, &safepoints, 31);
        // SAFETY: `slots` and `safepoints` remain alive while roots are used.
        let frame_roots = unsafe { AllocSafepointFrameRoots::new(&ctx, &safepoint) }.unwrap();
        let roots = AllocValueStubCallRoots::new(
            frame_roots,
            [arg_value, Value::undefined(), Value::undefined()],
        );
        heap.collect_minor_with_roots(&mut |visitor| roots.visit_extra_roots(visitor))
            .expect("minor GC");

        let frame_after = Value::from_abi_bits(slots[0])
            .as_raw_gc()
            .expect("moved frame raw");
        let arg_after = roots.value(0).as_raw_gc().expect("moved arg raw");
        assert_ne!(frame_after, frame_before);
        assert_ne!(arg_after, arg_before);
    }

    #[test]
    fn alloc_safepoint_frame_roots_reject_invalid_maps() {
        let mut slots = [Value::undefined().to_abi_bits()];
        let ctx = test_alloc_context(std::ptr::null_mut(), &mut slots, &[], 1);
        let no_safepoint = SafepointRecord {
            id: NO_SAFEPOINT,
            frame_state: NO_FRAME_STATE,
            tagged_locations: vec![TaggedLocation::frame_slot(0)],
        };
        assert_eq!(
            validate_alloc_safepoint_frame_roots(&ctx, &no_safepoint),
            Err(AllocSafepointRootError::NoSafepoint)
        );
        // SAFETY: the context intentionally names no table, so no pointer is
        // dereferenced.
        assert_eq!(
            unsafe { alloc_safepoint_record(&ctx, 1) },
            Err(AllocSafepointRootError::UnknownSafepoint { id: 1 })
        );

        let safepoints = [SafepointRecord::frame_slot_window(7, NO_FRAME_STATE, 1)];
        let table_ctx = test_alloc_context(std::ptr::null_mut(), &mut slots, &safepoints, 9);
        // SAFETY: `safepoints` is alive for the lookup.
        assert_eq!(
            unsafe { alloc_safepoint_record(&table_ctx, NO_SAFEPOINT) },
            Err(AllocSafepointRootError::NoSafepoint)
        );
        // SAFETY: `safepoints` is alive for the lookup.
        assert_eq!(
            unsafe { alloc_safepoint_record(&table_ctx, 9) },
            Err(AllocSafepointRootError::UnknownSafepoint { id: 9 })
        );

        let missing_slots_ctx =
            RuntimeStubAllocContext::new(std::ptr::null_mut(), std::ptr::null_mut(), 1, 1);
        let valid_safepoint = SafepointRecord::frame_slot_window(1, NO_FRAME_STATE, 1);
        assert_eq!(
            validate_alloc_safepoint_frame_roots(&missing_slots_ctx, &valid_safepoint),
            Err(AllocSafepointRootError::MissingFrameSlots)
        );

        let out_of_bounds = SafepointRecord::frame_slot_window(2, NO_FRAME_STATE, 2);
        assert_eq!(
            validate_alloc_safepoint_frame_roots(&ctx, &out_of_bounds),
            Err(AllocSafepointRootError::FrameSlotOutOfBounds {
                index: 1,
                frame_slot_count: 1,
            })
        );

        let unsupported = SafepointRecord {
            id: 3,
            frame_state: NO_FRAME_STATE,
            tagged_locations: vec![TaggedLocation::machine_register(0)],
        };
        assert_eq!(
            validate_alloc_safepoint_frame_roots(&ctx, &unsupported),
            Err(AllocSafepointRootError::UnsupportedLocation {
                kind: TaggedLocationKind::MachineRegister,
                index: 0,
            })
        );
    }

    #[test]
    fn map_set_alloc_entry_mutates_and_returns_receiver() {
        let mut interp = Interpreter::new();
        let map = collections::alloc_map(interp.gc_heap_mut()).expect("map");
        let key = crate::string::JsString::from_str("k", interp.gc_heap_mut()).expect("key");
        let safepoints = [SafepointRecord::frame_slot_window(21, NO_FRAME_STATE, 3)];
        let mut slots = [
            Value::map(map).to_abi_bits(),
            Value::string(key).to_abi_bits(),
            n(99).to_abi_bits(),
        ];
        let mut ctx = test_alloc_context(&mut interp, &mut slots, &safepoints, 21);

        let pair = COLLECTION_MAP_SET_ALLOC
            .invoke_raw(&mut ctx, 21, slots[0], slots[1], slots[2])
            .expect("entry");
        assert_eq!(pair.status(), RuntimeStubStatus::Ok);
        let result = pair.into_result().into_value().expect("receiver");
        let map = result.as_map().expect("map receiver");
        assert_eq!(
            collections::map_get(map, interp.gc_heap_mut(), &Value::string(key)),
            Some(n(99))
        );
    }

    #[test]
    fn set_add_alloc_entry_mutates_and_returns_receiver() {
        let mut interp = Interpreter::new();
        let set = collections::alloc_set(interp.gc_heap_mut()).expect("set");
        let value = crate::string::JsString::from_str("v", interp.gc_heap_mut()).expect("value");
        let safepoints = [SafepointRecord::frame_slot_window(22, NO_FRAME_STATE, 3)];
        let mut slots = [
            Value::set(set).to_abi_bits(),
            Value::string(value).to_abi_bits(),
            Value::undefined().to_abi_bits(),
        ];
        let mut ctx = test_alloc_context(&mut interp, &mut slots, &safepoints, 22);

        let pair = COLLECTION_SET_ADD_ALLOC
            .invoke_raw(&mut ctx, 22, slots[0], slots[1], slots[2])
            .expect("entry");
        assert_eq!(pair.status(), RuntimeStubStatus::Ok);
        let result = pair.into_result().into_value().expect("receiver");
        let set = result.as_set().expect("set receiver");
        assert!(collections::set_has(
            set,
            interp.gc_heap_mut(),
            &Value::string(value)
        ));
    }

    #[test]
    fn string_concat_alloc_entry_concats_primitive_string_operands() {
        let mut interp = Interpreter::new();
        let lhs = crate::string::JsString::from_str("k", interp.gc_heap_mut()).expect("lhs");
        let safepoints = [SafepointRecord::frame_slot_window(24, NO_FRAME_STATE, 3)];
        let mut slots = [
            Value::string(lhs).to_abi_bits(),
            n(7).to_abi_bits(),
            Value::undefined().to_abi_bits(),
        ];
        let mut ctx = test_alloc_context(&mut interp, &mut slots, &safepoints, 24);

        let pair = STRING_CONCAT_ALLOC
            .invoke_raw(&mut ctx, 24, slots[0], slots[1], slots[2])
            .expect("entry");
        assert_eq!(pair.status(), RuntimeStubStatus::Ok);
        let value = pair.into_result().into_value().expect("string");
        let string = value.as_string(interp.gc_heap()).expect("string value");
        assert_eq!(string.to_lossy_string(interp.gc_heap()), "k7");

        let pair = STRING_CONCAT_ALLOC
            .invoke_raw(
                &mut ctx,
                24,
                n(1).to_abi_bits(),
                n(2).to_abi_bits(),
                slots[2],
            )
            .expect("entry");
        assert_eq!(pair.status(), RuntimeStubStatus::Miss);
    }

    #[test]
    fn spill_slot_safepoint_root_is_traced_and_validated() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let obj = young_object_value(&mut heap);
        // Frame window holds a non-pointer; the tagged pointer lives only in the
        // native spill/save area, named by a spill-slot safepoint location.
        let mut frame = [n(3).to_abi_bits()];
        let mut spill = [obj.to_abi_bits()];
        let record = SafepointRecord {
            id: 1,
            frame_state: NO_FRAME_STATE,
            tagged_locations: vec![TaggedLocation::spill_slot(0)],
        };
        let ctx = test_alloc_context(
            std::ptr::null_mut(),
            &mut frame,
            std::slice::from_ref(&record),
            1,
        )
        .with_spill_area(spill.as_mut_ptr(), spill.len() as u16);

        validate_alloc_safepoint_frame_roots(&ctx, &record).expect("spill root validates");
        let roots = unsafe { AllocSafepointFrameRoots::new(&ctx, &record) }.expect("publisher");
        let mut visited = 0usize;
        roots.visit_extra_roots(&mut |_p| visited += 1);
        assert_eq!(visited, 1, "the spill-slot pointer is traced exactly once");

        // A spill-slot location without a published spill window is rejected, and
        // a machine-register location remains unsupported (spilled first).
        let no_spill = test_alloc_context(
            std::ptr::null_mut(),
            &mut frame,
            std::slice::from_ref(&record),
            1,
        );
        assert_eq!(
            validate_alloc_safepoint_frame_roots(&no_spill, &record),
            Err(AllocSafepointRootError::MissingSpillSlots)
        );
        let reg_record = SafepointRecord {
            id: 1,
            frame_state: NO_FRAME_STATE,
            tagged_locations: vec![TaggedLocation::machine_register(0)],
        };
        assert_eq!(
            validate_alloc_safepoint_frame_roots(&ctx, &reg_record),
            Err(AllocSafepointRootError::UnsupportedLocation {
                kind: TaggedLocationKind::MachineRegister,
                index: 0,
            })
        );
    }

    #[test]
    fn collection_alloc_entries_miss_invalid_context() {
        let pair = collection_map_set_alloc(
            std::ptr::null_mut(),
            1,
            Value::undefined().to_abi_bits(),
            Value::undefined().to_abi_bits(),
            Value::undefined().to_abi_bits(),
        );
        assert_eq!(pair.status(), RuntimeStubStatus::Miss);

        let mut interp = Interpreter::new();
        let safepoints = [SafepointRecord::frame_slot_window(1, NO_FRAME_STATE, 1)];
        let mut slots = [Value::undefined().to_abi_bits()];
        let mut ctx = test_alloc_context(&mut interp, &mut slots, &safepoints, 1);
        let pair = collection_set_add_alloc(
            &mut ctx,
            99,
            Value::undefined().to_abi_bits(),
            Value::undefined().to_abi_bits(),
            Value::undefined().to_abi_bits(),
        );
        assert_eq!(pair.status(), RuntimeStubStatus::Miss);
    }

    #[test]
    fn map_get_leaf_hits_flat_key() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let map = collections::alloc_map(&mut heap).expect("map");
        let key = crate::string::JsString::from_str("k", &mut heap).expect("key");
        collections::map_set(map, &mut heap, Value::string(key), n(42)).expect("set");

        let result = collection_map_get_leaf(
            &heap as *const otter_gc::GcHeap,
            Value::map(map).to_abi_bits(),
            Value::string(key).to_abi_bits(),
        );
        assert_eq!(result.status, RuntimeStubStatus::Ok);
        assert_eq!(result.into_value(), Some(n(42)));

        let result = invoke_leaf_no_alloc_stub2(
            &heap,
            STUB_COLLECTION_MAP_GET_LEAF.id,
            Value::map(map),
            Value::string(key),
        );
        assert_eq!(result.status, RuntimeStubStatus::Ok);
        assert_eq!(result.into_value(), Some(n(42)));

        let result = leaf_no_alloc_stub2_trampoline(
            &heap as *const otter_gc::GcHeap,
            STUB_COLLECTION_MAP_GET_LEAF.id,
            Value::map(map).to_abi_bits(),
            Value::string(key).to_abi_bits(),
        );
        assert_eq!(result.status, RuntimeStubStatus::Ok);
        assert_eq!(result.into_value(), Some(n(42)));

        let pair = leaf_no_alloc_stub2_trampoline_pair(
            &heap as *const otter_gc::GcHeap,
            STUB_COLLECTION_MAP_GET_LEAF.id,
            Value::map(map).to_abi_bits(),
            Value::string(key).to_abi_bits(),
        );
        assert_eq!(pair.status(), RuntimeStubStatus::Ok);
        assert_eq!(pair.into_result().into_value(), Some(n(42)));
    }

    #[test]
    fn map_has_leaf_misses_rope_key() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let map = collections::alloc_map(&mut heap).expect("map");
        // Short concatenations flatten in place; long operands keep the key an
        // unflattened rope so the leaf path exercises its rope miss.
        let left = crate::string::JsString::from_str("kkkkkkkkkkkkkkkk", &mut heap).expect("left");
        let right =
            crate::string::JsString::from_str("1111111111111111", &mut heap).expect("right");
        let rope = crate::string::JsString::concat(left, right, &mut heap).expect("rope");

        let result = collection_map_has_leaf(
            &heap as *const otter_gc::GcHeap,
            Value::map(map).to_abi_bits(),
            Value::string(rope).to_abi_bits(),
        );
        assert_eq!(result.status, RuntimeStubStatus::Miss);
        assert_eq!(result.into_value(), None);
    }

    #[test]
    fn collection_lookup_alloc_entries_materialize_rope_keys() {
        let mut interp = Interpreter::new();
        let map = collections::alloc_map(interp.gc_heap_mut()).expect("map");
        let set = collections::alloc_set(interp.gc_heap_mut()).expect("set");
        // Short concatenations flatten in place; long operands keep the keys
        // unflattened ropes so the leaf path misses and the alloc path has to
        // materialize them.
        let insert_left =
            crate::string::JsString::from_str("kkkkkkkkkkkkkkkk", interp.gc_heap_mut())
                .expect("insert left");
        let insert_right =
            crate::string::JsString::from_str("1111111111111111", interp.gc_heap_mut())
                .expect("insert right");
        let insert_rope =
            crate::string::JsString::concat(insert_left, insert_right, interp.gc_heap_mut())
                .expect("insert rope");
        let lookup_left =
            crate::string::JsString::from_str("kkkkkkkkkkkkkkkk", interp.gc_heap_mut())
                .expect("lookup left");
        let lookup_right =
            crate::string::JsString::from_str("1111111111111111", interp.gc_heap_mut())
                .expect("lookup right");
        let lookup_rope =
            crate::string::JsString::concat(lookup_left, lookup_right, interp.gc_heap_mut())
                .expect("lookup rope");
        let safepoints = [SafepointRecord::frame_slot_window(23, NO_FRAME_STATE, 3)];

        let mut insert_map_slots = [
            Value::map(map).to_abi_bits(),
            Value::string(insert_rope).to_abi_bits(),
            n(77).to_abi_bits(),
        ];
        let mut insert_map_ctx =
            test_alloc_context(&mut interp, &mut insert_map_slots, &safepoints, 23);
        let inserted = COLLECTION_MAP_SET_ALLOC
            .invoke_raw(
                &mut insert_map_ctx,
                23,
                insert_map_slots[0],
                insert_map_slots[1],
                insert_map_slots[2],
            )
            .expect("map set entry");
        assert_eq!(inserted.status(), RuntimeStubStatus::Ok);

        let mut insert_set_slots = [
            Value::set(set).to_abi_bits(),
            Value::string(insert_rope).to_abi_bits(),
            Value::undefined().to_abi_bits(),
        ];
        let mut insert_set_ctx =
            test_alloc_context(&mut interp, &mut insert_set_slots, &safepoints, 23);
        let inserted = COLLECTION_SET_ADD_ALLOC
            .invoke_raw(
                &mut insert_set_ctx,
                23,
                insert_set_slots[0],
                insert_set_slots[1],
                insert_set_slots[2],
            )
            .expect("set add entry");
        assert_eq!(inserted.status(), RuntimeStubStatus::Ok);

        let leaf = collection_map_has_leaf(
            &interp.gc_heap as *const otter_gc::GcHeap,
            Value::map(map).to_abi_bits(),
            Value::string(lookup_rope).to_abi_bits(),
        );
        assert_eq!(leaf.status, RuntimeStubStatus::Miss);

        let mut map_slots = [
            Value::map(map).to_abi_bits(),
            Value::string(lookup_rope).to_abi_bits(),
            Value::undefined().to_abi_bits(),
        ];
        let mut map_ctx = test_alloc_context(&mut interp, &mut map_slots, &safepoints, 23);
        let get = COLLECTION_MAP_GET_ALLOC
            .invoke_raw(&mut map_ctx, 23, map_slots[0], map_slots[1], map_slots[2])
            .expect("map get entry");
        assert_eq!(get.status(), RuntimeStubStatus::Ok);
        assert_eq!(get.into_result().into_value(), Some(n(77)));

        let has = COLLECTION_MAP_HAS_ALLOC
            .invoke_raw(&mut map_ctx, 23, map_slots[0], map_slots[1], map_slots[2])
            .expect("map has entry");
        assert_eq!(has.status(), RuntimeStubStatus::Ok);
        assert_eq!(has.into_result().into_value(), Some(Value::boolean(true)));

        let deleted = COLLECTION_MAP_DELETE_ALLOC
            .invoke_raw(&mut map_ctx, 23, map_slots[0], map_slots[1], map_slots[2])
            .expect("map delete entry");
        assert_eq!(deleted.status(), RuntimeStubStatus::Ok);
        assert_eq!(
            deleted.into_result().into_value(),
            Some(Value::boolean(true))
        );

        let mut set_slots = [
            Value::set(set).to_abi_bits(),
            Value::string(lookup_rope).to_abi_bits(),
            Value::undefined().to_abi_bits(),
        ];
        let mut set_ctx = test_alloc_context(&mut interp, &mut set_slots, &safepoints, 23);
        let has = COLLECTION_SET_HAS_ALLOC
            .invoke_raw(&mut set_ctx, 23, set_slots[0], set_slots[1], set_slots[2])
            .expect("set has entry");
        assert_eq!(has.status(), RuntimeStubStatus::Ok);
        assert_eq!(has.into_result().into_value(), Some(Value::boolean(true)));

        let deleted = COLLECTION_SET_DELETE_ALLOC
            .invoke_raw(&mut set_ctx, 23, set_slots[0], set_slots[1], set_slots[2])
            .expect("set delete entry");
        assert_eq!(deleted.status(), RuntimeStubStatus::Ok);
        assert_eq!(
            deleted.into_result().into_value(),
            Some(Value::boolean(true))
        );
    }

    #[test]
    fn set_has_leaf_hits_flat_key() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let set = collections::alloc_set(&mut heap).expect("set");
        collections::set_add(set, &mut heap, n(7)).expect("add");

        let result = collection_set_has_leaf(
            &heap as *const otter_gc::GcHeap,
            Value::set(set).to_abi_bits(),
            n(7).to_abi_bits(),
        );
        assert_eq!(result.status, RuntimeStubStatus::Ok);
        assert_eq!(result.into_value(), Some(Value::boolean(true)));
    }

    #[test]
    fn leaf_stub_entries_miss_null_heap() {
        let result = collection_map_get_leaf(
            std::ptr::null(),
            Value::undefined().to_abi_bits(),
            Value::undefined().to_abi_bits(),
        );
        assert_eq!(result.status, RuntimeStubStatus::Miss);
        assert_eq!(result.into_value(), None);
    }
}
