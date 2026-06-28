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
    NO_SAFEPOINT, RuntimeStubAllocContext, RuntimeStubDescriptor, RuntimeStubId, RuntimeStubResult,
    RuntimeStubResultPair, STUB_COLLECTION_MAP_GET_LEAF, STUB_COLLECTION_MAP_HAS_LEAF,
    STUB_COLLECTION_MAP_SET_ALLOC, STUB_COLLECTION_SET_ADD_ALLOC, STUB_COLLECTION_SET_HAS_LEAF,
    SafepointId, SafepointRecord, TaggedLocationKind, validate_stub_descriptor,
};
use crate::{Interpreter, Value, collections};

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
}

/// Resolve an allocating-stub safepoint id through the context packet table.
///
/// # Safety
///
/// `ctx.safepoint_records` must point at `ctx.safepoint_count` initialized
/// [`SafepointRecord`] values that remain alive for the duration of the
/// allocating stub call.
pub unsafe fn alloc_safepoint_record(
    ctx: &RuntimeStubAllocContext,
    safepoint: SafepointId,
) -> Result<&SafepointRecord, AllocSafepointRootError> {
    if safepoint == NO_SAFEPOINT {
        return Err(AllocSafepointRootError::NoSafepoint);
    }
    if !ctx.has_safepoint_records() {
        return Err(AllocSafepointRootError::MissingSafepointRecords);
    }
    // SAFETY: guaranteed by the caller; generated code builds this pointer
    // from the active function metadata and the stub must not retain it.
    let records =
        unsafe { std::slice::from_raw_parts(ctx.safepoint_records, ctx.safepoint_count as usize) };
    records
        .iter()
        .find(|record| record.id == safepoint)
        .ok_or(AllocSafepointRootError::UnknownSafepoint { id: safepoint })
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
    for location in &safepoint.tagged_locations {
        match location.kind {
            TaggedLocationKind::FrameSlot => {
                if location.index >= ctx.frame_slot_count {
                    return Err(AllocSafepointRootError::FrameSlotOutOfBounds {
                        index: location.index,
                        frame_slot_count: ctx.frame_slot_count,
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
    /// `ctx.frame_slots` must point at `ctx.frame_slot_count` live, writable
    /// `Value` ABI slots for the duration of any heap registration created from
    /// this value. The slots must remain pinned in memory while a GC may trace
    /// and update them.
    pub unsafe fn new(
        ctx: &'a RuntimeStubAllocContext,
        safepoint: &'a SafepointRecord,
    ) -> Result<Self, AllocSafepointRootError> {
        validate_alloc_safepoint_frame_roots(ctx, safepoint)?;
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
            debug_assert_eq!(location.kind, TaggedLocationKind::FrameSlot);
            debug_assert!(location.index < self.ctx.frame_slot_count);
            // SAFETY: construction validates non-null frame slots, rejects
            // out-of-bounds locations, and requires callers to keep the
            // writable frame window alive while this root source is registered.
            let value =
                unsafe { &*(self.ctx.frame_slots.add(location.index as usize) as *const Value) };
            value.trace_value_slots(visitor);
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
    values: [Value; 3],
}

impl<'a> AllocValueStubCallRoots<'a> {
    fn new(frame_roots: AllocSafepointFrameRoots<'a>, values: [Value; 3]) -> Self {
        Self {
            frame_roots,
            values,
        }
    }

    fn value(&self, index: usize) -> Value {
        self.values[index]
    }
}

impl otter_gc::ExtraRootSource for AllocValueStubCallRoots<'_> {
    fn visit_extra_roots(&self, visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)) {
        self.frame_roots.visit_extra_roots(visitor);
        for value in &self.values {
            value.trace_value_slots(visitor);
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
    RuntimeStubResultPair::from_result(collection_map_set_alloc_inner(
        ctx, safepoint, recv_bits, key_bits, value_bits,
    ))
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
    RuntimeStubResultPair::from_result(collection_set_add_alloc_inner(
        ctx,
        safepoint,
        recv_bits,
        value_bits,
        unused_bits,
    ))
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
    let Some(interp) = interpreter_mut(ctx.vm) else {
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
    let depth = interp
        .gc_heap
        .push_extra_roots(otter_gc::ExtraRoots::new(&roots));
    let result = (|| {
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
    })();
    interp.gc_heap.pop_extra_roots_to(depth - 1);
    result
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
    let Some(interp) = interpreter_mut(ctx.vm) else {
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
    let depth = interp
        .gc_heap
        .push_extra_roots(otter_gc::ExtraRoots::new(&roots));
    let result = (|| {
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
    })();
    interp.gc_heap.pop_extra_roots_to(depth - 1);
    result
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
        NO_FRAME_STATE, RuntimeStubStatus, TaggedLocation, TaggedLocationKind,
    };
    use otter_gc::ExtraRootSource;

    fn n(i: i32) -> Value {
        Value::number_i32(i)
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
        assert_eq!(
            alloc_value_stub_by_id(STUB_COLLECTION_MAP_SET_ALLOC.id).map(|stub| stub.descriptor),
            Some(STUB_COLLECTION_MAP_SET_ALLOC)
        );
        assert_eq!(
            alloc_value_stub_by_id(STUB_COLLECTION_SET_ADD_ALLOC.id).map(|stub| stub.descriptor),
            Some(STUB_COLLECTION_SET_ADD_ALLOC)
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
        let mut ctx = RuntimeStubAllocContext::new(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            0,
            slots.as_mut_ptr(),
            slots.len() as u16,
        );
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
        let ctx = RuntimeStubAllocContext::new(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
            safepoints.as_ptr(),
            safepoints.len() as u32,
            0,
            slots.as_mut_ptr(),
            slots.len() as u16,
        );

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
    fn alloc_safepoint_frame_roots_reject_invalid_maps() {
        let mut slots = [Value::undefined().to_abi_bits()];
        let ctx = RuntimeStubAllocContext::new(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            0,
            slots.as_mut_ptr(),
            slots.len() as u16,
        );
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
            Err(AllocSafepointRootError::MissingSafepointRecords)
        );

        let safepoints = [SafepointRecord::frame_slot_window(7, NO_FRAME_STATE, 1)];
        let table_ctx = RuntimeStubAllocContext::new(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
            safepoints.as_ptr(),
            safepoints.len() as u32,
            0,
            slots.as_mut_ptr(),
            slots.len() as u16,
        );
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

        let missing_slots_ctx = RuntimeStubAllocContext::new(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
            safepoints.as_ptr(),
            safepoints.len() as u32,
            0,
            std::ptr::null_mut(),
            0,
        );
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
        let mut ctx = RuntimeStubAllocContext::new(
            (&mut interp as *mut Interpreter).cast(),
            std::ptr::null_mut(),
            std::ptr::null(),
            safepoints.as_ptr(),
            safepoints.len() as u32,
            0,
            slots.as_mut_ptr(),
            slots.len() as u16,
        );

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
        let mut ctx = RuntimeStubAllocContext::new(
            (&mut interp as *mut Interpreter).cast(),
            std::ptr::null_mut(),
            std::ptr::null(),
            safepoints.as_ptr(),
            safepoints.len() as u32,
            0,
            slots.as_mut_ptr(),
            slots.len() as u16,
        );

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
        let mut ctx = RuntimeStubAllocContext::new(
            (&mut interp as *mut Interpreter).cast(),
            std::ptr::null_mut(),
            std::ptr::null(),
            safepoints.as_ptr(),
            safepoints.len() as u32,
            0,
            slots.as_mut_ptr(),
            slots.len() as u16,
        );
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
        let left = crate::string::JsString::from_str("k", &mut heap).expect("left");
        let right = crate::string::JsString::from_str("1", &mut heap).expect("right");
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
