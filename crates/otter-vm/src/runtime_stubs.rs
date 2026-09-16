//! VM-native runtime stub entrypoints and the isolate-owned entry table.
//!
//! These functions are the machine-callable implementation layer behind
//! [`crate::native_abi`] descriptors. The interpreter calls them directly and
//! generated code calls the same entrypoints, resolved by descriptor id at
//! compile time.
//!
//! # Contents
//! - Leaf/no-allocation collection probes for `Map.get`, `Map.has`, and
//!   `Set.has`.
//! - Guarded boxed-value leaves for numeric bootstrap natives, including the
//!   exact-int32 `parseInt` identity case.
//! - Unboxed binary64 math and scalar-conversion leaves for typed numeric
//!   machine code.
//! - Allocating collection, string-concat, and length-array entries.
//! - JIT transition-binding validation against the static descriptor inventory.
//!
//! # Invariants
//! - Value-family arguments are boxed [`crate::Value`] raw ABI bits; typed
//!   numeric leaves use the platform's unboxed floating-point ABI.
//! - Machine and Rust-facing helpers use the same two-register
//!   [`crate::native_abi::NativeResultPair`]; no unpacked result DTO exists.
//! - Signature families are never mixed behind one untyped address array.
//! - `LeafNoAlloc` stubs must not allocate, trigger GC, call JS, flatten
//!   strings, or mutate heap state.
//! - `Alloc` stubs must publish their current safepoint roots before any
//!   allocation and must not hold untracked raw `Value` bits across GC.
//!
//! # See also
//! - [`crate::native_abi`]
//! - [`crate::method_ops`]

use crate::native_abi::{
    CodeRegistryView, NO_SAFEPOINT, NativeResultDomain, NativeResultPair, RuntimeStubAllocContext,
    RuntimeStubDescriptor, RuntimeStubId, STUB_ARRAY_CONSTRUCT_ALLOC, STUB_ARRAY_POP_LEAF,
    STUB_ARRAY_PUSH_ALLOC, STUB_ARRAY_SHIFT_LEAF, STUB_ARRAY_UNSHIFT_ALLOC,
    STUB_COLLECTION_MAP_DELETE_ALLOC, STUB_COLLECTION_MAP_GET_ALLOC, STUB_COLLECTION_MAP_GET_LEAF,
    STUB_COLLECTION_MAP_HAS_ALLOC, STUB_COLLECTION_MAP_HAS_LEAF, STUB_COLLECTION_MAP_SET_ALLOC,
    STUB_COLLECTION_MAP_SET_MUTATING, STUB_COLLECTION_SET_ADD_ALLOC,
    STUB_COLLECTION_SET_DELETE_ALLOC, STUB_COLLECTION_SET_HAS_ALLOC, STUB_COLLECTION_SET_HAS_LEAF,
    STUB_MATH_ABS_LEAF, STUB_MATH_FLOOR_LEAF, STUB_MATH_MAX_LEAF, STUB_MATH_MIN_LEAF,
    STUB_MATH_SQRT_LEAF, STUB_NUMBER_POW_F64_LEAF, STUB_NUMBER_REM_F64_LEAF, STUB_NUMBER_REM_LEAF,
    STUB_NUMBER_TO_INT32_F64_LEAF, STUB_PARSE_INT_I32_LEAF, STUB_STRICT_EQ_LEAF,
    STUB_STRING_CHAR_CODE_AT_LEAF, STUB_STRING_CODE_POINT_AT_LEAF, STUB_STRING_CONCAT_ALLOC,
    STUB_STRING_ENDS_WITH_LEAF, STUB_STRING_INCLUDES_LEAF, STUB_STRING_INDEX_OF_LEAF,
    STUB_STRING_STARTS_WITH_LEAF, STUB_TO_BOOLEAN_LEAF, SafepointId, SafepointRecord,
    TaggedLocationKind, validate_stub_descriptor,
};
use crate::rooting::RootScopeExt;
use crate::{Interpreter, Value, collections};
use std::cell::UnsafeCell;

/// Two-argument leaf/no-allocation runtime stub ABI.
///
/// The heap pointer is opaque to generated code. It must name the current
/// isolate heap and must remain valid for the duration of the call. The callee
/// must not allocate, trigger GC, or retain the pointer. The result is the
/// two-register [`NativeResultPair`] encoding, so generated code never
/// needs a memory-returned record for a leaf probe.
pub type LeafNoAllocStub2Fn = extern "C" fn(*const otter_gc::GcHeap, u64, u64) -> NativeResultPair;

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
    ) -> NativeResultPair {
        (self.entry)(heap, a0_bits, a1_bits)
    }
}

/// Pure two-argument unboxed binary64 runtime stub ABI.
pub type Float64LeafStub2Fn = extern "C" fn(f64, f64) -> f64;

/// Callable unboxed binary64 leaf with its shared ABI descriptor.
#[derive(Clone, Copy)]
pub struct Float64LeafStub2 {
    /// Passive descriptor shared with profiler/JIT metadata.
    pub descriptor: RuntimeStubDescriptor,
    /// Machine-callable entrypoint using FP argument and result registers.
    pub entry: Float64LeafStub2Fn,
}

impl Float64LeafStub2 {
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

    /// Invoke the typed leaf without boxing or a status channel.
    #[must_use]
    pub fn invoke(self, left: f64, right: f64) -> f64 {
        (self.entry)(left, right)
    }
}

/// Pure one-argument binary64-to-word runtime stub ABI.
pub type Float64ToWordLeafStub1Fn = extern "C" fn(f64) -> u64;

/// Callable binary64-to-word leaf with its shared ABI descriptor.
#[derive(Clone, Copy)]
pub struct Float64ToWordLeafStub1 {
    /// Passive descriptor shared with profiler/JIT metadata.
    pub descriptor: RuntimeStubDescriptor,
    /// Machine-callable entrypoint using one FP argument and one word result.
    pub entry: Float64ToWordLeafStub1Fn,
}

impl Float64ToWordLeafStub1 {
    /// `true` when descriptor metadata matches this callable ABI shape.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        validate_stub_descriptor(self.descriptor, NO_SAFEPOINT)
            && self.descriptor.argument_count == 1
    }

    /// Raw native entry address for generated code.
    #[must_use]
    pub fn entry_addr(self) -> usize {
        self.entry as usize
    }

    /// Invoke the typed conversion without boxing or a status channel.
    #[must_use]
    pub fn invoke(self, value: f64) -> u64 {
        (self.entry)(value)
    }
}

/// Two-argument mutating leaf runtime stub ABI.
///
/// Identical to [`LeafNoAllocStub2Fn`] with a mutable heap: the entry rewrites
/// GC-managed state in place and runs any required write barrier, but still
/// must not allocate, trigger collection, or re-enter JS, so the call site
/// publishes no safepoint and no rooting packet.
pub type MutatingLeafStub2Fn = extern "C" fn(*mut otter_gc::GcHeap, u64, u64) -> NativeResultPair;

/// Callable mutating-leaf stub entry with its ABI descriptor.
#[derive(Clone, Copy)]
pub struct MutatingLeafStub2 {
    /// Passive descriptor shared with profiler/JIT metadata.
    pub descriptor: RuntimeStubDescriptor,
    /// Machine-callable Rust entrypoint with the descriptor's fixed ABI shape.
    pub entry: MutatingLeafStub2Fn,
}

impl MutatingLeafStub2 {
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
        heap: *mut otter_gc::GcHeap,
        a0_bits: u64,
        a1_bits: u64,
    ) -> NativeResultPair {
        (self.entry)(heap, a0_bits, a1_bits)
    }
}

/// Three-argument mutating leaf runtime stub ABI.
///
/// [`MutatingLeafStub2Fn`] with one more operand word, for an in-place write
/// whose receiver and two arguments do not fit two words. The same rules
/// apply: rewrite GC-managed state in place, run every required write barrier,
/// and never allocate, collect, or re-enter JS, so the call site publishes no
/// safepoint and no rooting packet.
pub type MutatingLeafStub3Fn =
    extern "C" fn(*mut otter_gc::GcHeap, u64, u64, u64) -> NativeResultPair;

/// Callable three-argument mutating-leaf stub entry with its ABI descriptor.
#[derive(Clone, Copy)]
pub struct MutatingLeafStub3 {
    /// Passive descriptor shared with profiler/JIT metadata.
    pub descriptor: RuntimeStubDescriptor,
    /// Machine-callable Rust entrypoint with the descriptor's fixed ABI shape.
    pub entry: MutatingLeafStub3Fn,
}

impl MutatingLeafStub3 {
    /// `true` when descriptor metadata matches this callable ABI shape.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        validate_stub_descriptor(self.descriptor, NO_SAFEPOINT)
            && self.descriptor.argument_count == 3
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
        heap: *mut otter_gc::GcHeap,
        a0_bits: u64,
        a1_bits: u64,
        a2_bits: u64,
    ) -> NativeResultPair {
        (self.entry)(heap, a0_bits, a1_bits, a2_bits)
    }
}

/// Machine-callable fixed-value allocating runtime stub entry shape.
///
/// Generated code supplies the VM-native allocation/rooting context separately
/// from the raw `Value` arguments:
/// `(alloc_ctx, safepoint_id, receiver_bits, arg0_bits, arg1_bits)`.
/// `safepoint_id` must identify a precise map for the current call site.
pub type AllocValueStubFn =
    extern "C" fn(*mut RuntimeStubAllocContext, SafepointId, u64, u64, u64) -> NativeResultPair;

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
    ) -> Option<NativeResultPair> {
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
    let Some(record) = (unsafe { registry.resolve(ctx.code_object_id(), safepoint) }) else {
        return Err(AllocSafepointRootError::UnknownSafepoint { id: safepoint });
    };
    // SAFETY: resolver contract above.
    Ok(unsafe { &*record })
}

/// Validate that `safepoint` can be published from `ctx`'s frame and native
/// spill windows.
///
/// Baseline names interpreter-visible frame slots. Machine IR names only its
/// allocator-driven native save homes after copying register/spill roots into
/// the packet's rewriteable spill window. Raw machine-register roots remain
/// invalid because the collector cannot rewrite a live register directly.
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
    // SAFETY: `has_frame_slots` verified the thread-published frame/window.
    let frame = unsafe { &*ctx.current_frame() };
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

/// Root publisher for an allocating runtime-stub safepoint backed by frame or
/// native spill slots.
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
    /// Build a root publisher for a validated safepoint.
    ///
    /// # Safety
    ///
    /// `ctx.thread.current_frame` must publish a live, writable tagged window for the
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
            let frame = unsafe { &*self.ctx.current_frame() };
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

/// Callable ABI entry for the generational/insertion write barrier.
pub const WRITE_BARRIER_MUTATING: MutatingLeafStub2 = MutatingLeafStub2 {
    descriptor: crate::native_abi::STUB_WRITE_BARRIER,
    entry: write_barrier_mutating,
};

/// Callable ABI entry for in-place `Map.prototype.set`.
pub const COLLECTION_MAP_SET_MUTATING: MutatingLeafStub3 = MutatingLeafStub3 {
    descriptor: STUB_COLLECTION_MAP_SET_MUTATING,
    entry: collection_map_set_mutating,
};

/// Callable ABI entry for `Math.abs`.
pub const MATH_ABS_LEAF: LeafNoAllocStub2 = LeafNoAllocStub2 {
    descriptor: STUB_MATH_ABS_LEAF,
    entry: math_abs_leaf,
};

/// Callable ABI entry for `Math.floor`.
pub const MATH_FLOOR_LEAF: LeafNoAllocStub2 = LeafNoAllocStub2 {
    descriptor: STUB_MATH_FLOOR_LEAF,
    entry: math_floor_leaf,
};

/// Callable ABI entry for `Math.sqrt`.
pub const MATH_SQRT_LEAF: LeafNoAllocStub2 = LeafNoAllocStub2 {
    descriptor: STUB_MATH_SQRT_LEAF,
    entry: math_sqrt_leaf,
};

/// Callable ABI entry for two-argument `Math.max`.
pub const MATH_MAX_LEAF: LeafNoAllocStub2 = LeafNoAllocStub2 {
    descriptor: STUB_MATH_MAX_LEAF,
    entry: math_max_leaf,
};

/// Callable ABI entry for two-argument `Math.min`.
pub const MATH_MIN_LEAF: LeafNoAllocStub2 = LeafNoAllocStub2 {
    descriptor: STUB_MATH_MIN_LEAF,
    entry: math_min_leaf,
};

/// Callable ABI entry for exact-one-argument `parseInt(Int32)`.
pub const PARSE_INT_I32_LEAF: LeafNoAllocStub2 = LeafNoAllocStub2 {
    descriptor: STUB_PARSE_INT_I32_LEAF,
    entry: parse_int_i32_leaf,
};

/// Callable ABI entry for `String.prototype.charCodeAt`.
pub const STRING_CHAR_CODE_AT_LEAF: LeafNoAllocStub2 = LeafNoAllocStub2 {
    descriptor: STUB_STRING_CHAR_CODE_AT_LEAF,
    entry: string_char_code_at_leaf,
};

/// Callable ABI entry for `String.prototype.codePointAt`.
pub const STRING_CODE_POINT_AT_LEAF: LeafNoAllocStub2 = LeafNoAllocStub2 {
    descriptor: STUB_STRING_CODE_POINT_AT_LEAF,
    entry: string_code_point_at_leaf,
};

/// Callable ABI entry for `String.prototype.indexOf`.
pub const STRING_INDEX_OF_LEAF: LeafNoAllocStub2 = LeafNoAllocStub2 {
    descriptor: STUB_STRING_INDEX_OF_LEAF,
    entry: string_index_of_leaf,
};

/// Callable ABI entry for `String.prototype.includes`.
pub const STRING_INCLUDES_LEAF: LeafNoAllocStub2 = LeafNoAllocStub2 {
    descriptor: STUB_STRING_INCLUDES_LEAF,
    entry: string_includes_leaf,
};

/// Callable ABI entry for `String.prototype.startsWith`.
pub const STRING_STARTS_WITH_LEAF: LeafNoAllocStub2 = LeafNoAllocStub2 {
    descriptor: STUB_STRING_STARTS_WITH_LEAF,
    entry: string_starts_with_leaf,
};

/// Callable ABI entry for `String.prototype.endsWith`.
pub const STRING_ENDS_WITH_LEAF: LeafNoAllocStub2 = LeafNoAllocStub2 {
    descriptor: STUB_STRING_ENDS_WITH_LEAF,
    entry: string_ends_with_leaf,
};

/// Callable ABI entry for the strict-equality probe.
pub const STRICT_EQ_LEAF: LeafNoAllocStub2 = LeafNoAllocStub2 {
    descriptor: STUB_STRICT_EQ_LEAF,
    entry: strict_eq_leaf,
};

/// Callable ABI entry for the ToBoolean probe.
pub const TO_BOOLEAN_LEAF: LeafNoAllocStub2 = LeafNoAllocStub2 {
    descriptor: STUB_TO_BOOLEAN_LEAF,
    entry: to_boolean_leaf,
};

/// Callable ABI entry for the numeric-remainder probe.
pub const NUMBER_REM_LEAF: LeafNoAllocStub2 = LeafNoAllocStub2 {
    descriptor: STUB_NUMBER_REM_LEAF,
    entry: number_rem_leaf,
};

/// Callable typed ABI entry for Number remainder.
pub const NUMBER_REM_F64_LEAF: Float64LeafStub2 = Float64LeafStub2 {
    descriptor: STUB_NUMBER_REM_F64_LEAF,
    entry: number_rem_f64_leaf,
};

/// Callable typed ABI entry for Number exponentiation.
pub const NUMBER_POW_F64_LEAF: Float64LeafStub2 = Float64LeafStub2 {
    descriptor: STUB_NUMBER_POW_F64_LEAF,
    entry: number_pow_f64_leaf,
};

/// Callable typed ABI entry for ECMAScript ToInt32.
pub const NUMBER_TO_INT32_F64_LEAF: Float64ToWordLeafStub1 = Float64ToWordLeafStub1 {
    descriptor: STUB_NUMBER_TO_INT32_F64_LEAF,
    entry: number_to_int32_f64_leaf,
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

/// ABI descriptor for guarded `Array(length)` allocation.
pub const ARRAY_CONSTRUCT_ALLOC: AllocValueStub = AllocValueStub {
    descriptor: STUB_ARRAY_CONSTRUCT_ALLOC,
    entry: Some(array_construct_alloc),
};

/// Callable ABI entry for `Array.prototype.pop` over a dense array.
pub const ARRAY_POP_LEAF: MutatingLeafStub2 = MutatingLeafStub2 {
    descriptor: STUB_ARRAY_POP_LEAF,
    entry: array_pop_leaf,
};

/// ABI descriptor for `Array.prototype.push` over a dense array.
pub const ARRAY_PUSH_ALLOC: AllocValueStub = AllocValueStub {
    descriptor: STUB_ARRAY_PUSH_ALLOC,
    entry: Some(array_push_alloc),
};

/// Callable ABI entry for `Array.prototype.shift` over a dense array.
pub const ARRAY_SHIFT_LEAF: MutatingLeafStub2 = MutatingLeafStub2 {
    descriptor: STUB_ARRAY_SHIFT_LEAF,
    entry: array_shift_leaf,
};

/// ABI descriptor for `Array.prototype.unshift` over a dense array.
pub const ARRAY_UNSHIFT_ALLOC: AllocValueStub = AllocValueStub {
    descriptor: STUB_ARRAY_UNSHIFT_ALLOC,
    entry: Some(array_unshift_alloc),
};

/// Resolve a two-argument mutating leaf stub by ABI descriptor id.
#[must_use]
pub const fn mutating_leaf_stub2_by_id(id: RuntimeStubId) -> Option<MutatingLeafStub2> {
    match id {
        id if id == STUB_ARRAY_POP_LEAF.id => Some(ARRAY_POP_LEAF),
        id if id == STUB_ARRAY_SHIFT_LEAF.id => Some(ARRAY_SHIFT_LEAF),
        id if id == crate::native_abi::STUB_WRITE_BARRIER.id => Some(WRITE_BARRIER_MUTATING),
        _ => None,
    }
}

/// Resolve a three-argument mutating-leaf entry by descriptor id.
#[must_use]
pub const fn mutating_leaf_stub3_by_id(id: RuntimeStubId) -> Option<MutatingLeafStub3> {
    if id == STUB_COLLECTION_MAP_SET_MUTATING.id {
        return Some(COLLECTION_MAP_SET_MUTATING);
    }
    None
}

/// Resolve a fixed two-argument leaf/no-allocation stub by ABI descriptor id.
#[must_use]
pub const fn leaf_no_alloc_stub2_by_id(id: RuntimeStubId) -> Option<LeafNoAllocStub2> {
    match id {
        id if id == STUB_COLLECTION_MAP_GET_LEAF.id => Some(COLLECTION_MAP_GET_LEAF),
        id if id == STUB_MATH_ABS_LEAF.id => Some(MATH_ABS_LEAF),
        id if id == STUB_MATH_FLOOR_LEAF.id => Some(MATH_FLOOR_LEAF),
        id if id == STUB_MATH_SQRT_LEAF.id => Some(MATH_SQRT_LEAF),
        id if id == STUB_MATH_MAX_LEAF.id => Some(MATH_MAX_LEAF),
        id if id == STUB_MATH_MIN_LEAF.id => Some(MATH_MIN_LEAF),
        id if id == STUB_PARSE_INT_I32_LEAF.id => Some(PARSE_INT_I32_LEAF),
        id if id == STUB_COLLECTION_MAP_HAS_LEAF.id => Some(COLLECTION_MAP_HAS_LEAF),
        id if id == STUB_COLLECTION_SET_HAS_LEAF.id => Some(COLLECTION_SET_HAS_LEAF),
        id if id == STUB_STRICT_EQ_LEAF.id => Some(STRICT_EQ_LEAF),
        id if id == STUB_TO_BOOLEAN_LEAF.id => Some(TO_BOOLEAN_LEAF),
        id if id == STUB_NUMBER_REM_LEAF.id => Some(NUMBER_REM_LEAF),
        id if id == STUB_STRING_CHAR_CODE_AT_LEAF.id => Some(STRING_CHAR_CODE_AT_LEAF),
        id if id == STUB_STRING_CODE_POINT_AT_LEAF.id => Some(STRING_CODE_POINT_AT_LEAF),
        id if id == STUB_STRING_INDEX_OF_LEAF.id => Some(STRING_INDEX_OF_LEAF),
        id if id == STUB_STRING_INCLUDES_LEAF.id => Some(STRING_INCLUDES_LEAF),
        id if id == STUB_STRING_STARTS_WITH_LEAF.id => Some(STRING_STARTS_WITH_LEAF),
        id if id == STUB_STRING_ENDS_WITH_LEAF.id => Some(STRING_ENDS_WITH_LEAF),
        _ => None,
    }
}

/// Resolve a pure unboxed binary64 leaf by ABI descriptor id.
#[must_use]
pub const fn float64_leaf_stub2_by_id(id: RuntimeStubId) -> Option<Float64LeafStub2> {
    match id {
        id if id == STUB_NUMBER_REM_F64_LEAF.id => Some(NUMBER_REM_F64_LEAF),
        id if id == STUB_NUMBER_POW_F64_LEAF.id => Some(NUMBER_POW_F64_LEAF),
        _ => None,
    }
}

/// Resolve a pure binary64-to-word leaf by ABI descriptor id.
#[must_use]
pub const fn float64_to_word_leaf_stub1_by_id(id: RuntimeStubId) -> Option<Float64ToWordLeafStub1> {
    if id == STUB_NUMBER_TO_INT32_F64_LEAF.id {
        return Some(NUMBER_TO_INT32_F64_LEAF);
    }
    None
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
        id if id == STUB_ARRAY_CONSTRUCT_ALLOC.id => Some(ARRAY_CONSTRUCT_ALLOC),
        id if id == STUB_ARRAY_PUSH_ALLOC.id => Some(ARRAY_PUSH_ALLOC),
        id if id == STUB_ARRAY_UNSHIFT_ALLOC.id => Some(ARRAY_UNSHIFT_ALLOC),
        _ => None,
    }
}

/// Whether a descriptor is implemented by a statically typed VM entry.
#[must_use]
pub(crate) fn is_vm_owned_runtime_stub(id: RuntimeStubId) -> bool {
    leaf_no_alloc_stub2_by_id(id).is_some()
        || float64_leaf_stub2_by_id(id).is_some()
        || float64_to_word_leaf_stub1_by_id(id).is_some()
        || mutating_leaf_stub2_by_id(id).is_some()
        || mutating_leaf_stub3_by_id(id).is_some()
        || alloc_value_stub_by_id(id)
            .and_then(|stub| stub.entry)
            .is_some()
}

/// Validate the compiler-owned transition inventory once at hook install.
///
/// Runtime calls use their statically typed VM/JIT entrypoints directly; this
/// check exists only to prove that the installed compiler's transition table
/// covers every active non-VM descriptor exactly once with the declared
/// signature.
pub(crate) fn validate_jit_runtime_stub_bindings(bindings: &[crate::jit::JitRuntimeStubBinding]) {
    let mut seen = [false; crate::native_abi::RUNTIME_STUB_DESCRIPTORS.len()];

    for binding in bindings {
        let index = binding
            .id
            .checked_sub(1)
            .map(|index| index as usize)
            .expect("JIT runtime-stub id is 1-based");
        let descriptor = crate::native_abi::RUNTIME_STUB_DESCRIPTORS
            .get(index)
            .expect("JIT runtime-stub id names a VM descriptor");
        assert_eq!(descriptor.id, binding.id);
        assert_eq!(
            descriptor.signature, binding.signature,
            "JIT runtime-stub binding {} declares the descriptor signature family",
            binding.id
        );
        assert_ne!(binding.entry_addr, 0);
        assert!(
            !is_vm_owned_runtime_stub(binding.id),
            "JIT runtime-stub binding {} may only fill a JIT-owned slot",
            binding.id
        );
        assert!(
            !std::mem::replace(&mut seen[index], true),
            "JIT runtime-stub binding {} is duplicated",
            binding.id
        );
    }

    for (index, descriptor) in crate::native_abi::RUNTIME_STUB_DESCRIPTORS
        .iter()
        .enumerate()
    {
        if !is_vm_owned_runtime_stub(descriptor.id) {
            assert!(
                seen[index],
                "runtime stub {} left vacant after JIT installation",
                index + 1
            );
        }
    }
}

/// Invoke a fixed two-argument leaf/no-allocation stub by ABI descriptor id.
///
/// Rust-facing dynamic-id dispatch over the static typed inventory, used by
/// interpreter fast paths that carry a stub id in feedback. Generated code
/// resolves the id at compile time instead and calls the typed entry directly.
/// It intentionally takes no root scope or safepoint because the descriptor
/// class is `LeafNoAlloc`.
#[must_use]
pub fn invoke_leaf_no_alloc_stub2(
    heap: &otter_gc::GcHeap,
    id: RuntimeStubId,
    a0: Value,
    a1: Value,
) -> NativeResultPair {
    let Some(stub) = leaf_no_alloc_stub2_by_id(id) else {
        return NativeResultPair::miss();
    };
    stub.invoke_raw(
        heap as *const otter_gc::GcHeap,
        a0.to_abi_bits(),
        a1.to_abi_bits(),
    )
}

/// Invoke a three-argument mutating-leaf entry by descriptor id.
///
/// The interpreter reaches the same entry generated code calls, so an in-place
/// write has one implementation and one set of preconditions.
#[must_use]
pub fn invoke_mutating_leaf_stub3(
    heap: &mut otter_gc::GcHeap,
    id: RuntimeStubId,
    a0: Value,
    a1: Value,
    a2: Value,
) -> NativeResultPair {
    let Some(stub) = mutating_leaf_stub3_by_id(id) else {
        return NativeResultPair::miss();
    };
    stub.invoke_raw(
        heap as *mut otter_gc::GcHeap,
        a0.to_abi_bits(),
        a1.to_abi_bits(),
        a2.to_abi_bits(),
    )
}

fn record_alloc_value_stub_result(
    ctx: *mut RuntimeStubAllocContext,
    result: NativeResultPair,
) -> NativeResultPair {
    let status = match result.validate(NativeResultDomain::Probe) {
        Some(status) => status,
        None => return NativeResultPair::fatal_internal(),
    };
    if let Some(ctx) = alloc_context_mut(ctx)
        && let Some(interp) = alloc_interpreter_mut(ctx)
    {
        interp.record_jit_alloc_value_stub_status(status);
    }
    result
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
) -> NativeResultPair {
    record_alloc_value_stub_result(
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
) -> NativeResultPair {
    record_alloc_value_stub_result(
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
) -> NativeResultPair {
    record_alloc_value_stub_result(
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
) -> NativeResultPair {
    record_alloc_value_stub_result(
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
) -> NativeResultPair {
    record_alloc_value_stub_result(
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
) -> NativeResultPair {
    record_alloc_value_stub_result(
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
) -> NativeResultPair {
    record_alloc_value_stub_result(
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
) -> NativeResultPair {
    record_alloc_value_stub_result(
        ctx,
        string_concat_alloc_inner(ctx, safepoint, lhs_bits, rhs_bits, unused_bits),
    )
}

/// Allocating `Array(length)` stub for an exact nonnegative int32 length.
#[must_use]
pub extern "C" fn array_construct_alloc(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    length_bits: u64,
    padding0_bits: u64,
    padding1_bits: u64,
) -> NativeResultPair {
    record_alloc_value_stub_result(
        ctx,
        array_construct_alloc_inner(ctx, safepoint, length_bits, padding0_bits, padding1_bits),
    )
}

/// Debug guard proving a `LeafNoAlloc` entry neither allocated nor triggered
/// a collection: the young/old allocation extents must be byte-identical on
/// exit. Release builds compile it away.
struct LeafNoAllocGuard {
    #[cfg(debug_assertions)]
    heap: *const otter_gc::GcHeap,
    #[cfg(debug_assertions)]
    extents: Option<(u64, u64)>,
}

impl LeafNoAllocGuard {
    #[cfg_attr(not(debug_assertions), allow(unused_variables))]
    fn new(heap: *const otter_gc::GcHeap) -> Self {
        Self {
            #[cfg(debug_assertions)]
            heap,
            #[cfg(debug_assertions)]
            extents: heap_ref(heap).map(Self::extents),
        }
    }

    #[cfg(debug_assertions)]
    fn extents(heap: &otter_gc::GcHeap) -> (u64, u64) {
        let stats = heap.stats();
        (
            stats.new_allocated_bytes as u64,
            stats.old_allocated_bytes as u64,
        )
    }
}

impl Drop for LeafNoAllocGuard {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        {
            let after = heap_ref(self.heap).map(Self::extents);
            debug_assert_eq!(
                self.extents, after,
                "LeafNoAlloc stub allocated or triggered a collection"
            );
        }
    }
}

/// §7.1.2 ToBoolean over one raw operand word (`rhs_bits` is ignored).
/// Total for every value including heap cells; the only miss is a null heap
/// pointer (probe harnesses without a live isolate).
#[must_use]
pub extern "C" fn to_boolean_leaf(
    heap: *const otter_gc::GcHeap,
    value_bits: u64,
    _ignored: u64,
) -> NativeResultPair {
    let _guard = LeafNoAllocGuard::new(heap);
    let Some(heap) = heap_ref(heap) else {
        return NativeResultPair::miss();
    };
    let value = Value::from_abi_bits(value_bits);
    let truthy = value.to_boolean(heap);
    NativeResultPair::success(Value::boolean(truthy))
}

/// Full f64 remainder over two operand words already guarded to be numbers
/// (the emitted int32 fast path handles the representable cases; this owns
/// doubles, zero divisors, and the `-0` results int32 cannot express).
/// A non-number operand misses so the interpreter owns coercion.
#[must_use]
pub extern "C" fn number_rem_leaf(
    heap: *const otter_gc::GcHeap,
    lhs_bits: u64,
    rhs_bits: u64,
) -> NativeResultPair {
    let _guard = LeafNoAllocGuard::new(heap);
    let lhs = Value::from_abi_bits(lhs_bits);
    let rhs = Value::from_abi_bits(rhs_bits);
    let (Some(a), Some(b)) = (lhs.as_number(), rhs.as_number()) else {
        return NativeResultPair::miss();
    };
    let rem = a.as_f64() % b.as_f64();
    NativeResultPair::success(Value::number(crate::NumberValue::Double(rem)))
}

/// Pure unboxed Number remainder for typed numeric machine code.
#[must_use]
pub extern "C" fn number_rem_f64_leaf(left: f64, right: f64) -> f64 {
    left % right
}

/// Pure unboxed ECMAScript Number exponentiation for typed machine code.
#[must_use]
pub extern "C" fn number_pow_f64_leaf(base: f64, exponent: f64) -> f64 {
    crate::number::pow(
        crate::NumberValue::Double(base),
        crate::NumberValue::Double(exponent),
    )
    .as_f64()
}

/// Pure unboxed ECMAScript ToInt32 conversion for typed machine code.
#[must_use]
pub extern "C" fn number_to_int32_f64_leaf(value: f64) -> u64 {
    u64::from(crate::number::to_int32(crate::NumberValue::Double(value)) as u32)
}

/// `String.prototype.charCodeAt` over a string receiver and an integral index.
/// Walks the body to the requested code unit without allocating; a non-string
/// receiver, a non-integral index, or an index outside `[0, len)` misses so the
/// general method path owns coercion and the NaN result.
#[must_use]
pub extern "C" fn string_char_code_at_leaf(
    heap: *const otter_gc::GcHeap,
    receiver_bits: u64,
    index_bits: u64,
) -> NativeResultPair {
    let _guard = LeafNoAllocGuard::new(heap);
    let Some(heap) = heap_ref(heap) else {
        return NativeResultPair::miss();
    };
    let receiver = Value::from_abi_bits(receiver_bits);
    let index = Value::from_abi_bits(index_bits);
    let (Some(string), Some(index)) = (receiver.as_string(heap), index.as_i32()) else {
        return NativeResultPair::miss();
    };
    if index < 0 {
        return NativeResultPair::miss();
    }
    let Some(unit) = string.char_code_at(index as u32, heap) else {
        return NativeResultPair::miss();
    };
    NativeResultPair::success(Value::number(crate::NumberValue::from_i32(i32::from(unit))))
}

/// Resolve two operand words to a string receiver and a string argument, the
/// shape every two-string leaf probe below shares.
fn string_pair(
    heap: &otter_gc::GcHeap,
    receiver_bits: u64,
    arg_bits: u64,
) -> Option<(crate::JsString, crate::JsString)> {
    let receiver = Value::from_abi_bits(receiver_bits).as_string(heap)?;
    let argument = Value::from_abi_bits(arg_bits).as_string(heap)?;
    Some((receiver, argument))
}

/// `String.prototype.codePointAt` over a string receiver and an integral
/// index, resolving a surrogate pair to its scalar value. A non-string
/// receiver, a non-integral index, or an index outside `[0, len)` misses so
/// the general path owns coercion and the `undefined` result.
#[must_use]
pub extern "C" fn string_code_point_at_leaf(
    heap: *const otter_gc::GcHeap,
    receiver_bits: u64,
    index_bits: u64,
) -> NativeResultPair {
    let _guard = LeafNoAllocGuard::new(heap);
    let Some(heap) = heap_ref(heap) else {
        return NativeResultPair::miss();
    };
    let (Some(string), Some(index)) = (
        Value::from_abi_bits(receiver_bits).as_string(heap),
        Value::from_abi_bits(index_bits).as_i32(),
    ) else {
        return NativeResultPair::miss();
    };
    if index < 0 {
        return NativeResultPair::miss();
    }
    let index = index as u32;
    let Some(first) = string.char_code_at(index, heap) else {
        return NativeResultPair::miss();
    };
    // §22.1.3.4 CodePointAt: a leading surrogate followed by a trailing one
    // yields the combined scalar value; anything else yields the unit itself.
    let code_point = match string.char_code_at(index + 1, heap) {
        Some(second) if (0xD800..0xDC00).contains(&first) && (0xDC00..0xE000).contains(&second) => {
            0x1_0000 + ((u32::from(first) - 0xD800) << 10) + (u32::from(second) - 0xDC00)
        }
        _ => u32::from(first),
    };
    NativeResultPair::success(Value::number(crate::NumberValue::from_i32(
        code_point as i32,
    )))
}

/// `String.prototype.indexOf` over two string operands, searching from index
/// zero. A non-string operand misses so the general path owns coercion.
#[must_use]
pub extern "C" fn string_index_of_leaf(
    heap: *const otter_gc::GcHeap,
    receiver_bits: u64,
    needle_bits: u64,
) -> NativeResultPair {
    let _guard = LeafNoAllocGuard::new(heap);
    let Some(heap) = heap_ref(heap) else {
        return NativeResultPair::miss();
    };
    let Some((haystack, needle)) = string_pair(heap, receiver_bits, needle_bits) else {
        return NativeResultPair::miss();
    };
    let Ok(found) = haystack.index_of(needle, 0, None, heap) else {
        return NativeResultPair::miss();
    };
    let position = found.map_or(-1, |index| index as i32);
    NativeResultPair::success(Value::number(crate::NumberValue::from_i32(position)))
}

/// `String.prototype.includes` over two string operands, searching from index
/// zero. A non-string operand misses so the general path owns coercion and the
/// RegExp-argument rejection.
#[must_use]
pub extern "C" fn string_includes_leaf(
    heap: *const otter_gc::GcHeap,
    receiver_bits: u64,
    needle_bits: u64,
) -> NativeResultPair {
    let _guard = LeafNoAllocGuard::new(heap);
    let Some(heap) = heap_ref(heap) else {
        return NativeResultPair::miss();
    };
    let Some((haystack, needle)) = string_pair(heap, receiver_bits, needle_bits) else {
        return NativeResultPair::miss();
    };
    let Ok(found) = haystack.index_of(needle, 0, None, heap) else {
        return NativeResultPair::miss();
    };
    NativeResultPair::success(Value::boolean(found.is_some()))
}

/// `String.prototype.startsWith` over two string operands, anchored at index
/// zero. A non-string operand misses so the general path owns coercion and the
/// RegExp-argument rejection.
#[must_use]
pub extern "C" fn string_starts_with_leaf(
    heap: *const otter_gc::GcHeap,
    receiver_bits: u64,
    prefix_bits: u64,
) -> NativeResultPair {
    let _guard = LeafNoAllocGuard::new(heap);
    let Some(heap) = heap_ref(heap) else {
        return NativeResultPair::miss();
    };
    let Some((string, prefix)) = string_pair(heap, receiver_bits, prefix_bits) else {
        return NativeResultPair::miss();
    };
    NativeResultPair::success(Value::boolean(string.starts_with(prefix, 0, heap)))
}

/// `String.prototype.endsWith` over two string operands, anchored at the
/// receiver's end. A non-string operand misses so the general path owns
/// coercion and the RegExp-argument rejection.
#[must_use]
pub extern "C" fn string_ends_with_leaf(
    heap: *const otter_gc::GcHeap,
    receiver_bits: u64,
    suffix_bits: u64,
) -> NativeResultPair {
    let _guard = LeafNoAllocGuard::new(heap);
    let Some(heap) = heap_ref(heap) else {
        return NativeResultPair::miss();
    };
    let Some((string, suffix)) = string_pair(heap, receiver_bits, suffix_bits) else {
        return NativeResultPair::miss();
    };
    let end = string.len();
    NativeResultPair::success(Value::boolean(string.ends_with(suffix, end, heap)))
}

/// §7.2.15 IsStrictlyEqual over two raw operand words. Total for every value
/// pair — numbers, strings (code-unit content), BigInts, and identity for
/// the remaining heap shapes — so the only miss is a null heap pointer
/// (probe harnesses without a live isolate).
#[must_use]
pub extern "C" fn strict_eq_leaf(
    heap: *const otter_gc::GcHeap,
    lhs_bits: u64,
    rhs_bits: u64,
) -> NativeResultPair {
    let _guard = LeafNoAllocGuard::new(heap);
    let Some(heap) = heap_ref(heap) else {
        return NativeResultPair::miss();
    };
    let lhs = Value::from_abi_bits(lhs_bits);
    let rhs = Value::from_abi_bits(rhs_bits);
    let eq = crate::abstract_ops::is_strictly_equal(&lhs, &rhs, heap);
    NativeResultPair::success(Value::boolean(eq))
}

/// One numeric unary builtin, reached through the declared leaf ABI.
///
/// The argument arrives already boxed; a non-numeric argument misses so the
/// call site falls through to ordinary dispatch rather than performing
/// coercion here, which could observe user code and is not a leaf operation.
fn math_unary_leaf(arg_bits: u64, op: fn(f64) -> f64) -> NativeResultPair {
    let Some(value) = Value::from_abi_bits(arg_bits).as_f64() else {
        return NativeResultPair::miss();
    };
    NativeResultPair::success(Value::number_f64(op(value)))
}

/// One numeric binary builtin, reached through the declared leaf ABI.
///
/// Both arguments arrive already boxed; either one non-numeric misses, for the
/// same reason the unary entry does — coercion can observe user code.
fn math_binary_leaf(lhs_bits: u64, rhs_bits: u64, op: fn(f64, f64) -> f64) -> NativeResultPair {
    let (Some(lhs), Some(rhs)) = (
        Value::from_abi_bits(lhs_bits).as_f64(),
        Value::from_abi_bits(rhs_bits).as_f64(),
    ) else {
        return NativeResultPair::miss();
    };
    NativeResultPair::success(Value::number_f64(op(lhs, rhs)))
}

/// §21.3.2.24 `Math.max` over two numbers.
///
/// `f64::max` is IEEE `maxNum`: it swallows a NaN operand and picks either
/// zero when the operands compare equal. JavaScript propagates NaN and orders
/// `-0` below `+0`, so the comparison is written out rather than delegated.
fn js_math_max(lhs: f64, rhs: f64) -> f64 {
    if lhs.is_nan() || rhs.is_nan() {
        return f64::NAN;
    }
    if rhs > lhs || (rhs == 0.0 && lhs == 0.0 && lhs.is_sign_negative()) {
        rhs
    } else {
        lhs
    }
}

/// §21.3.2.25 `Math.min` over two numbers, mirroring [`js_math_max`].
fn js_math_min(lhs: f64, rhs: f64) -> f64 {
    if lhs.is_nan() || rhs.is_nan() {
        return f64::NAN;
    }
    if rhs < lhs || (rhs == 0.0 && lhs == 0.0 && rhs.is_sign_negative()) {
        rhs
    } else {
        lhs
    }
}

/// Leaf ABI entry for `Math.abs`.
pub extern "C" fn math_abs_leaf(
    heap: *const otter_gc::GcHeap,
    arg_bits: u64,
    _unused: u64,
) -> NativeResultPair {
    let _guard = LeafNoAllocGuard::new(heap);
    math_unary_leaf(arg_bits, f64::abs)
}

/// Leaf ABI entry for `Math.floor`.
pub extern "C" fn math_floor_leaf(
    heap: *const otter_gc::GcHeap,
    arg_bits: u64,
    _unused: u64,
) -> NativeResultPair {
    let _guard = LeafNoAllocGuard::new(heap);
    math_unary_leaf(arg_bits, f64::floor)
}

/// Leaf ABI entry for `Math.sqrt`.
pub extern "C" fn math_sqrt_leaf(
    heap: *const otter_gc::GcHeap,
    arg_bits: u64,
    _unused: u64,
) -> NativeResultPair {
    let _guard = LeafNoAllocGuard::new(heap);
    math_unary_leaf(arg_bits, f64::sqrt)
}

/// Leaf ABI entry for two-argument `Math.max`.
pub extern "C" fn math_max_leaf(
    heap: *const otter_gc::GcHeap,
    lhs_bits: u64,
    rhs_bits: u64,
) -> NativeResultPair {
    let _guard = LeafNoAllocGuard::new(heap);
    math_binary_leaf(lhs_bits, rhs_bits, js_math_max)
}

/// Leaf ABI entry for two-argument `Math.min`.
pub extern "C" fn math_min_leaf(
    heap: *const otter_gc::GcHeap,
    lhs_bits: u64,
    rhs_bits: u64,
) -> NativeResultPair {
    let _guard = LeafNoAllocGuard::new(heap);
    math_binary_leaf(lhs_bits, rhs_bits, js_math_min)
}

/// Leaf ABI entry for exact-one-argument `parseInt(Int32)`.
///
/// Int32-to-decimal-string-to-integer is the identity over the full int32
/// domain. Every other boxed representation misses before coercion so the
/// ordinary native performs the canonical observable `ToString` and radix
/// handling.
pub extern "C" fn parse_int_i32_leaf(
    heap: *const otter_gc::GcHeap,
    arg_bits: u64,
    _unused: u64,
) -> NativeResultPair {
    let _guard = LeafNoAllocGuard::new(heap);
    let value = Value::from_abi_bits(arg_bits);
    if value.is_int32() {
        NativeResultPair::success_bits(arg_bits)
    } else {
        NativeResultPair::miss()
    }
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
) -> NativeResultPair {
    let _guard = LeafNoAllocGuard::new(heap);
    collection_map_get_leaf_inner(heap, recv_bits, key_bits)
}

fn collection_map_get_leaf_inner(
    heap: *const otter_gc::GcHeap,
    recv_bits: u64,
    key_bits: u64,
) -> NativeResultPair {
    let Some(heap) = heap_ref(heap) else {
        return NativeResultPair::miss();
    };
    let recv = Value::from_abi_bits(recv_bits);
    let key = Value::from_abi_bits(key_bits);
    if !leaf_key_is_materialized(heap, key) {
        return NativeResultPair::miss();
    }
    let Some(map) = recv.as_map() else {
        return NativeResultPair::miss();
    };
    NativeResultPair::success(
        collections::map_get(map, heap, &key).unwrap_or_else(Value::undefined),
    )
}

/// Leaf in-place `Map.prototype.set`.
///
/// Runs only the case that cannot allocate: a key the map already holds, whose
/// value slot is rewritten in place with its write barrier. An absent key, an
/// unmaterialized key, or a non-Map receiver misses, and the allocating
/// sibling completes the call.
#[must_use]
/// Record one pointer store's write barrier from the parent's header address.
///
/// Generated code has already proven that the store needs the runtime: either
/// a marking cycle is in progress or the parent is an old, unrecorded object
/// with a nursery child. Nothing here allocates or re-enters, so the call site
/// publishes no safepoint.
pub extern "C" fn write_barrier_mutating(
    heap: *mut otter_gc::GcHeap,
    parent_header: u64,
    child_bits: u64,
) -> NativeResultPair {
    let Some(heap) = heap_mut(heap) else {
        return NativeResultPair::miss();
    };
    let child = Value::from_abi_bits(child_bits);
    // SAFETY: the emitted barrier derived this header from the receiver it is
    // storing into, under the same guard that proved the receiver's class.
    unsafe {
        heap.record_write_at(parent_header as *mut otter_gc::header::GcHeader, &child);
    }
    NativeResultPair::success(Value::undefined())
}

/// Insert or overwrite one `Map` entry in place.
pub extern "C" fn collection_map_set_mutating(
    heap: *mut otter_gc::GcHeap,
    recv_bits: u64,
    key_bits: u64,
    value_bits: u64,
) -> NativeResultPair {
    collection_map_set_mutating_inner(heap, recv_bits, key_bits, value_bits)
}

fn collection_map_set_mutating_inner(
    heap: *mut otter_gc::GcHeap,
    recv_bits: u64,
    key_bits: u64,
    value_bits: u64,
) -> NativeResultPair {
    let Some(heap) = heap_mut(heap) else {
        return NativeResultPair::miss();
    };
    let recv = Value::from_abi_bits(recv_bits);
    let key = Value::from_abi_bits(key_bits);
    // A key needing materialization for SameValueZero would flatten a string,
    // which a leaf entry may not do.
    if !leaf_key_is_materialized(heap, key) {
        return NativeResultPair::miss();
    }
    let Some(map) = recv.as_map() else {
        return NativeResultPair::miss();
    };
    if collections::map_set_existing(map, heap, &key, Value::from_abi_bits(value_bits)) {
        NativeResultPair::success(recv)
    } else {
        NativeResultPair::miss()
    }
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
) -> NativeResultPair {
    let _guard = LeafNoAllocGuard::new(heap);
    collection_map_has_leaf_inner(heap, recv_bits, key_bits)
}

fn collection_map_has_leaf_inner(
    heap: *const otter_gc::GcHeap,
    recv_bits: u64,
    key_bits: u64,
) -> NativeResultPair {
    let Some(heap) = heap_ref(heap) else {
        return NativeResultPair::miss();
    };
    let recv = Value::from_abi_bits(recv_bits);
    let key = Value::from_abi_bits(key_bits);
    if !leaf_key_is_materialized(heap, key) {
        return NativeResultPair::miss();
    }
    let Some(map) = recv.as_map() else {
        return NativeResultPair::miss();
    };
    NativeResultPair::success(Value::boolean(collections::map_has(map, heap, &key)))
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
) -> NativeResultPair {
    let _guard = LeafNoAllocGuard::new(heap);
    collection_set_has_leaf_inner(heap, recv_bits, key_bits)
}

fn collection_set_has_leaf_inner(
    heap: *const otter_gc::GcHeap,
    recv_bits: u64,
    key_bits: u64,
) -> NativeResultPair {
    let Some(heap) = heap_ref(heap) else {
        return NativeResultPair::miss();
    };
    let recv = Value::from_abi_bits(recv_bits);
    let key = Value::from_abi_bits(key_bits);
    if !leaf_key_is_materialized(heap, key) {
        return NativeResultPair::miss();
    }
    let Some(set) = recv.as_set() else {
        return NativeResultPair::miss();
    };
    NativeResultPair::success(Value::boolean(collections::set_has(set, heap, &key)))
}

fn collection_map_set_alloc_inner(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    recv_bits: u64,
    key_bits: u64,
    value_bits: u64,
) -> NativeResultPair {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return NativeResultPair::miss();
    };
    let Some(interp) = alloc_interpreter_mut(ctx) else {
        return NativeResultPair::miss();
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
        return NativeResultPair::miss();
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
            return NativeResultPair::miss();
        };
        match collections::map_set(map, &mut interp.gc_heap, key, value) {
            Ok(()) => NativeResultPair::success(roots.value(0)),
            Err(_) => NativeResultPair::out_of_memory(),
        }
    })()
}

fn collection_set_add_alloc_inner(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    recv_bits: u64,
    value_bits: u64,
    unused_bits: u64,
) -> NativeResultPair {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return NativeResultPair::miss();
    };
    let Some(interp) = alloc_interpreter_mut(ctx) else {
        return NativeResultPair::miss();
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
        return NativeResultPair::miss();
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
            return NativeResultPair::miss();
        };
        match collections::set_add(set, &mut interp.gc_heap, value) {
            Ok(()) => NativeResultPair::success(roots.value(0)),
            Err(_) => NativeResultPair::out_of_memory(),
        }
    })()
}

fn collection_map_get_alloc_inner(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    recv_bits: u64,
    key_bits: u64,
    unused_bits: u64,
) -> NativeResultPair {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return NativeResultPair::miss();
    };
    let Some(interp) = alloc_interpreter_mut(ctx) else {
        return NativeResultPair::miss();
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
        return NativeResultPair::miss();
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
            return NativeResultPair::miss();
        };
        NativeResultPair::success(
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
) -> NativeResultPair {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return NativeResultPair::miss();
    };
    let Some(interp) = alloc_interpreter_mut(ctx) else {
        return NativeResultPair::miss();
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
        return NativeResultPair::miss();
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
            return NativeResultPair::miss();
        };
        NativeResultPair::success(Value::boolean(collections::map_has(
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
) -> NativeResultPair {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return NativeResultPair::miss();
    };
    let Some(interp) = alloc_interpreter_mut(ctx) else {
        return NativeResultPair::miss();
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
        return NativeResultPair::miss();
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
            return NativeResultPair::miss();
        };
        NativeResultPair::success(Value::boolean(collections::set_has(
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
) -> NativeResultPair {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return NativeResultPair::miss();
    };
    let Some(interp) = alloc_interpreter_mut(ctx) else {
        return NativeResultPair::miss();
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
        return NativeResultPair::miss();
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
            return NativeResultPair::miss();
        };
        NativeResultPair::success(Value::boolean(collections::map_delete(
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
) -> NativeResultPair {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return NativeResultPair::miss();
    };
    let Some(interp) = alloc_interpreter_mut(ctx) else {
        return NativeResultPair::miss();
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
        return NativeResultPair::miss();
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
            return NativeResultPair::miss();
        };
        NativeResultPair::success(Value::boolean(collections::set_delete(
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
) -> NativeResultPair {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return NativeResultPair::miss();
    };
    let Some(interp) = alloc_interpreter_mut(ctx) else {
        return NativeResultPair::miss();
    };
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
        return NativeResultPair::miss();
    };
    let _roots_guard = interp
        .gc_heap
        .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
    (|| {
        // One-allocation fast path for `<short flat latin1 string> + <int32>`
        // and its mirror. It must run only after the generated frame slots are
        // published: the result allocation may scavenge either string operand.
        let lhs = roots.value(0);
        let rhs = roots.value(1);
        if let Some(fast) = interp.try_concat_string_int32(lhs, rhs) {
            return match fast {
                Ok(value) => NativeResultPair::success(value),
                Err(_) => NativeResultPair::out_of_memory(),
            };
        }
        let mut lhs_string_root = Value::undefined();
        let mut rhs_string_root = Value::undefined();
        let mut string_roots = otter_gc::RootScope::new(&mut interp.gc_heap);
        // SAFETY: both slots precede the scope and remain stationary through
        // coercion, which can allocate, and the final rope allocation.
        unsafe {
            string_roots.add_value(&mut lhs_string_root);
            string_roots.add_value(&mut rhs_string_root);
        }
        let lhs = roots.value(0);
        let rhs = roots.value(1);
        if lhs.as_string(&interp.gc_heap).is_none() && rhs.as_string(&interp.gc_heap).is_none() {
            return NativeResultPair::miss();
        }
        let Ok(lhs_string) = (if let Some(string) = lhs.as_string(&interp.gc_heap) {
            Ok(string)
        } else {
            interp.js_string_for_concat(lhs)
        }) else {
            return NativeResultPair::miss();
        };
        lhs_string_root = Value::string(lhs_string);
        // The left conversion may scavenge. Reload the right ABI operand from
        // its published safepoint slot before inspecting or converting it.
        let rhs = roots.value(1);
        let Ok(rhs_string) = (if let Some(string) = rhs.as_string(&interp.gc_heap) {
            Ok(string)
        } else {
            interp.js_string_for_concat(rhs)
        }) else {
            return NativeResultPair::miss();
        };
        rhs_string_root = Value::string(rhs_string);
        let lhs_string = lhs_string_root
            .as_string(&interp.gc_heap)
            .expect("rooted concat lhs is a string");
        let rhs_string = rhs_string_root
            .as_string(&interp.gc_heap)
            .expect("rooted concat rhs is a string");
        match crate::string::JsString::concat(lhs_string, rhs_string, &mut interp.gc_heap) {
            Ok(result) => NativeResultPair::success(Value::string(result)),
            // The generated caller owns the exact source FrameState. A logical
            // length overflow is therefore a pre-effect miss: deopt/replay lets
            // the canonical interpreter raise one catchable RangeError. It is
            // not heap exhaustion and must never increment the OOM outcome.
            Err(crate::string::StringConcatError::StringTooLong { .. }) => NativeResultPair::miss(),
            Err(crate::string::StringConcatError::OutOfMemory(_)) => {
                NativeResultPair::out_of_memory()
            }
        }
    })()
}

fn array_construct_alloc_inner(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    length_bits: u64,
    padding0_bits: u64,
    padding1_bits: u64,
) -> NativeResultPair {
    let Some(length) = Value::from_abi_bits(length_bits).as_i32() else {
        return NativeResultPair::miss();
    };
    let Ok(length) = u32::try_from(length) else {
        return NativeResultPair::miss();
    };
    let Some(ctx) = alloc_context_mut(ctx) else {
        return NativeResultPair::miss();
    };
    let Some(interp) = alloc_interpreter_mut(ctx) else {
        return NativeResultPair::miss();
    };
    // SAFETY: `ctx` is the current allocating-stub call packet. Its safepoint
    // table and frame/spill windows stay published for this synchronous call.
    let Ok(roots) = (unsafe {
        alloc_value_stub_call_roots(
            ctx,
            safepoint,
            [
                Value::from_abi_bits(length_bits),
                Value::from_abi_bits(padding0_bits),
                Value::from_abi_bits(padding1_bits),
            ],
        )
    }) else {
        return NativeResultPair::miss();
    };
    let _call_roots_guard = interp
        .gc_heap
        .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
    match interp.array_construct_length_runtime_rooted(length) {
        Ok(array) => NativeResultPair::success(array),
        Err(_) => NativeResultPair::out_of_memory(),
    }
}

/// `true` when a dense-array receiver still satisfies the `push` / `pop` fast
/// path over `[start, end)`.
///
/// Generated code proves the receiver is an ordinary dense array with the
/// original `%Array.prototype%` builtin in its slot. It cannot see the
/// remaining spec preconditions — a writable `length`, extensibility, an
/// accessor or attribute override in range, or the dense-size cap — so the
/// stub re-checks them and misses instead of falling back internally.
fn dense_range_is_fast(
    arr: crate::array::JsArray,
    heap: &otter_gc::GcHeap,
    start: usize,
    end: usize,
) -> bool {
    crate::array::is_ordinary_dense(arr, heap)
        && crate::array::length_writable(arr, heap)
        && crate::array::can_fast_fill_dense_range(arr, heap, start, end)
}

/// Leaf `Array.prototype.pop` over a dense array.
///
/// Truncation drops one reference and refreshes the cached element base and
/// length; it never allocates, so no safepoint or rooting packet is needed.
#[must_use]
pub extern "C" fn array_pop_leaf(
    heap: *mut otter_gc::GcHeap,
    recv_bits: u64,
    _unused_bits: u64,
) -> NativeResultPair {
    array_pop_leaf_inner(heap, recv_bits)
}

fn array_pop_leaf_inner(heap: *mut otter_gc::GcHeap, recv_bits: u64) -> NativeResultPair {
    let Some(heap) = heap_mut(heap) else {
        return NativeResultPair::miss();
    };
    let Some(arr) = Value::from_abi_bits(recv_bits).as_array() else {
        return NativeResultPair::miss();
    };
    let len = crate::array::len(arr, heap);
    // The last index must be a present own element: a hole would make the
    // spec's `Get` read an inherited value off the prototype chain.
    if len == 0
        || !crate::array::has_own_element(arr, heap, len - 1)
        || !dense_range_is_fast(arr, heap, len - 1, len)
    {
        return NativeResultPair::miss();
    }
    NativeResultPair::success(crate::array::pop(arr, heap))
}

/// Allocating `Array.prototype.push` over a dense array.
///
/// Appending may grow the dense buffer, which can move the receiver, so the
/// receiver is re-read from the rooted packet and the growth path threads the
/// caller roots through any emergency collection.
#[must_use]
pub extern "C" fn array_push_alloc(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    recv_bits: u64,
    value_bits: u64,
    unused_bits: u64,
) -> NativeResultPair {
    record_alloc_value_stub_result(
        ctx,
        array_push_alloc_inner(ctx, safepoint, recv_bits, value_bits, unused_bits),
    )
}

fn array_push_alloc_inner(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    recv_bits: u64,
    value_bits: u64,
    unused_bits: u64,
) -> NativeResultPair {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return NativeResultPair::miss();
    };
    let Some(interp) = alloc_interpreter_mut(ctx) else {
        return NativeResultPair::miss();
    };
    // `push` creates a *new* index, so the spec consults the prototype chain
    // for an inherited indexed setter there. The realm protector trips as soon
    // as any indexed accessor is installed anywhere, and generated code cannot
    // read it, so it is part of the stub's precondition set.
    if interp.array_index_accessor_protector {
        return NativeResultPair::miss();
    }
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
        return NativeResultPair::miss();
    };
    let _roots_guard = interp
        .gc_heap
        .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
    let Some(arr) = roots.value(0).as_array() else {
        return NativeResultPair::miss();
    };
    let len = crate::array::len(arr, &interp.gc_heap);
    if !dense_range_is_fast(arr, &interp.gc_heap, len, len + 1) {
        return NativeResultPair::miss();
    }
    let value = roots.value(1);
    // Growth may collect; the rooted receiver and pending value are traced so
    // the handle survives and the helper republishes the moved array.
    let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        roots.value(0).trace_value_slots(visitor);
        value.trace_value_slots(visitor);
    };
    match crate::array::push_with_roots(arr, &mut interp.gc_heap, value, &mut visit) {
        Ok(new_len) => {
            NativeResultPair::success(Value::number(crate::NumberValue::from_f64(new_len as f64)))
        }
        Err(_) => NativeResultPair::out_of_memory(),
    }
}

/// Leaf `Array.prototype.shift` over a dense array.
#[must_use]
pub extern "C" fn array_shift_leaf(
    heap: *mut otter_gc::GcHeap,
    recv_bits: u64,
    _unused_bits: u64,
) -> NativeResultPair {
    array_shift_leaf_inner(heap, recv_bits)
}

fn array_shift_leaf_inner(heap: *mut otter_gc::GcHeap, recv_bits: u64) -> NativeResultPair {
    let Some(heap) = heap_mut(heap) else {
        return NativeResultPair::miss();
    };
    let Some(arr) = Value::from_abi_bits(recv_bits).as_array() else {
        return NativeResultPair::miss();
    };
    let len = crate::array::len(arr, heap);
    // Every moved index must be a present own element: a hole anywhere in the
    // range would make the spec's per-index `Get` read an inherited value.
    if len == 0
        || !crate::array::is_fully_dense(arr, heap)
        || !dense_range_is_fast(arr, heap, 0, len)
    {
        return NativeResultPair::miss();
    }
    NativeResultPair::success(crate::array::dense_shift(arr, heap))
}

/// Allocating `Array.prototype.unshift` over a dense array.
#[must_use]
pub extern "C" fn array_unshift_alloc(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    recv_bits: u64,
    value_bits: u64,
    unused_bits: u64,
) -> NativeResultPair {
    record_alloc_value_stub_result(
        ctx,
        array_unshift_alloc_inner(ctx, safepoint, recv_bits, value_bits, unused_bits),
    )
}

fn array_unshift_alloc_inner(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    recv_bits: u64,
    value_bits: u64,
    unused_bits: u64,
) -> NativeResultPair {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return NativeResultPair::miss();
    };
    let Some(interp) = alloc_interpreter_mut(ctx) else {
        return NativeResultPair::miss();
    };
    // Head insertion creates a fresh trailing index, so the realm
    // indexed-accessor protector gates it exactly as it gates `push`.
    if interp.array_index_accessor_protector {
        return NativeResultPair::miss();
    }
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
        return NativeResultPair::miss();
    };
    let _roots_guard = interp
        .gc_heap
        .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
    let Some(arr) = roots.value(0).as_array() else {
        return NativeResultPair::miss();
    };
    let len = crate::array::len(arr, &interp.gc_heap);
    if !crate::array::is_fully_dense(arr, &interp.gc_heap)
        || !dense_range_is_fast(arr, &interp.gc_heap, len, len + 1)
    {
        return NativeResultPair::miss();
    }
    let value = roots.value(1);
    let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        roots.value(0).trace_value_slots(visitor);
        value.trace_value_slots(visitor);
    };
    match crate::array::dense_unshift_with_roots(arr, &mut interp.gc_heap, value, &mut visit) {
        Ok(new_len) => {
            NativeResultPair::success(Value::number(crate::NumberValue::from_f64(new_len as f64)))
        }
        Err(_) => NativeResultPair::out_of_memory(),
    }
}

fn heap_mut(heap: *mut otter_gc::GcHeap) -> Option<&'static mut otter_gc::GcHeap> {
    if heap.is_null() {
        return None;
    }
    // SAFETY: runtime stub callers pass the current isolate heap pointer.
    // Mutating leaf stubs neither allocate nor retain it, so the exclusive
    // borrow lasts only for this call.
    Some(unsafe { &mut *heap })
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
    // SAFETY: `runtime_context` is published from a live VmRuntimeActivation value.
    let reentry = unsafe { &*(thread.runtime_context as *const crate::jit::VmRuntimeActivation) };
    interpreter_mut(reentry.vm_ptr().cast())
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
        NO_FRAME_STATE, NativeFrame, NativeFrameFlags, NativeFrameKind, NativeResultStatus,
        TaggedLocation, TaggedLocationKind, VmFrameHeader, VmThread,
    };
    use otter_gc::ExtraRootSource;

    fn n(i: i32) -> Value {
        Value::number_i32(i)
    }

    fn probe_status(result: NativeResultPair) -> NativeResultStatus {
        result
            .validate(NativeResultDomain::Probe)
            .expect("valid probe result")
    }

    fn probe_value(result: NativeResultPair) -> Option<Value> {
        (probe_status(result) == NativeResultStatus::Success).then(|| result.payload_value())
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
            hot_function: 0,
        }));
        let reentry = Box::leak(Box::new(crate::jit::VmRuntimeActivation::for_test(vm)));
        let native_frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 0,
                pc: 0,
                register_count: slots.len() as u16,
                kind: NativeFrameKind::Baseline,
                flags: NativeFrameFlags::from_bits(NativeFrameFlags::HAS_SAFEPOINTS),
            },
            slots.as_mut_ptr() as u64,
            Value::function(0),
            Value::undefined(),
        );
        let frame = Box::leak(Box::new(native_frame));
        let mut thread = VmThread::empty();
        thread.current_frame = std::ptr::from_mut(frame) as u64;
        thread.current_code_object_id = 1;
        thread.runtime_context = std::ptr::from_ref(reentry) as u64;
        thread.code_registry = std::ptr::from_ref(registry) as u64;
        let thread = Box::leak(Box::new(thread));
        RuntimeStubAllocContext::new(thread, safepoint_id)
    }

    #[test]
    fn leaf_stub_entries_match_descriptors() {
        assert!(COLLECTION_MAP_GET_LEAF.is_valid());
        assert!(COLLECTION_MAP_HAS_LEAF.is_valid());
        assert!(COLLECTION_SET_HAS_LEAF.is_valid());
        assert!(NUMBER_REM_F64_LEAF.is_valid());
        assert!(NUMBER_POW_F64_LEAF.is_valid());
        assert!(NUMBER_TO_INT32_F64_LEAF.is_valid());
        assert!(PARSE_INT_I32_LEAF.is_valid());
        assert_eq!(
            leaf_no_alloc_stub2_by_id(STUB_COLLECTION_MAP_GET_LEAF.id).map(|stub| stub.descriptor),
            Some(STUB_COLLECTION_MAP_GET_LEAF)
        );
        assert_eq!(
            float64_leaf_stub2_by_id(STUB_NUMBER_REM_F64_LEAF.id).map(|stub| stub.invoke(5.5, 2.0)),
            Some(1.5)
        );
        assert_eq!(NUMBER_POW_F64_LEAF.invoke(f64::NAN, 0.0), 1.0);
        assert!(NUMBER_POW_F64_LEAF.invoke(-1.0, f64::INFINITY).is_nan());
        assert_eq!(NUMBER_TO_INT32_F64_LEAF.invoke(f64::NAN), 0);
        assert_eq!(NUMBER_TO_INT32_F64_LEAF.invoke(f64::INFINITY), 0);
        assert_eq!(NUMBER_TO_INT32_F64_LEAF.invoke(-1.9), u64::from(u32::MAX));
        assert_eq!(NUMBER_TO_INT32_F64_LEAF.invoke(4_294_967_297.0), 1);
        assert!(leaf_no_alloc_stub2_by_id(u32::MAX).is_none());
        assert!(float64_leaf_stub2_by_id(u32::MAX).is_none());
        assert!(float64_to_word_leaf_stub1_by_id(u32::MAX).is_none());
    }

    #[test]
    fn parse_int_leaf_is_exact_int32_identity_and_misses_other_tags() {
        for value in [i32::MIN, -1, 0, 1, i32::MAX] {
            let boxed = Value::number_i32(value).to_abi_bits();
            let pair = parse_int_i32_leaf(std::ptr::null(), boxed, 0);
            assert_eq!(probe_status(pair), NativeResultStatus::Success);
            assert_eq!(pair.payload_bits(), boxed);
        }

        for value in [
            Value::number_f64(7.0),
            Value::undefined(),
            Value::boolean(true),
        ] {
            let pair = parse_int_i32_leaf(std::ptr::null(), value.to_abi_bits(), 0);
            assert_eq!(probe_status(pair), NativeResultStatus::SideExit);
        }
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
        assert!(!ARRAY_CONSTRUCT_ALLOC.is_valid_for_safepoint(NO_SAFEPOINT));
        assert!(ARRAY_CONSTRUCT_ALLOC.is_valid_for_safepoint(1));
        assert!(ARRAY_CONSTRUCT_ALLOC.has_entry());
        assert!(ARRAY_CONSTRUCT_ALLOC.entry_addr().is_some());
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
        assert_eq!(
            alloc_value_stub_by_id(STUB_ARRAY_CONSTRUCT_ALLOC.id).map(|stub| stub.descriptor),
            Some(STUB_ARRAY_CONSTRUCT_ALLOC)
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
        ) -> NativeResultPair {
            if ctx.is_null() || safepoint != 9 || recv_bits != 1 || arg0_bits != 2 || arg1_bits != 3
            {
                return NativeResultPair::miss();
            }
            NativeResultPair::success_bits(recv_bits)
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
        assert_eq!(probe_status(result), NativeResultStatus::Success);
        assert_eq!(result.payload_bits(), 1);
    }

    #[test]
    fn alloc_safepoint_frame_roots_publish_value_slots() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let map = collections::alloc_map(&mut heap).expect("map");
        let mut slots = [Value::map(map).to_abi_bits(), n(7).to_abi_bits()];
        let safepoint = SafepointRecord {
            inline_frames: Box::default(),
            inline_frames_published: false,
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
            inline_frames: Box::default(),
            inline_frames_published: false,
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

        let missing_slots_ctx = RuntimeStubAllocContext::new(std::ptr::null_mut(), 1);
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
            inline_frames: Box::default(),
            inline_frames_published: false,
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
        assert_eq!(probe_status(pair), NativeResultStatus::Success);
        let result = probe_value(pair).expect("receiver");
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
        assert_eq!(probe_status(pair), NativeResultStatus::Success);
        let result = probe_value(pair).expect("receiver");
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
        assert_eq!(probe_status(pair), NativeResultStatus::Success);
        let value = probe_value(pair).expect("string");
        let string = value.as_string(interp.gc_heap()).expect("string value");
        assert_eq!(string.to_lossy_string(interp.gc_heap()), "k7");

        slots[1] = Value::boolean(true).to_abi_bits();
        let pair = STRING_CONCAT_ALLOC
            .invoke_raw(&mut ctx, 24, slots[0], slots[1], slots[2])
            .expect("entry");
        assert_eq!(probe_status(pair), NativeResultStatus::Success);
        let string = probe_value(pair)
            .and_then(|value| value.as_string(interp.gc_heap()))
            .expect("string value");
        assert_eq!(string.to_lossy_string(interp.gc_heap()), "ktrue");

        // The left conversion allocates before the generated-stub boundary
        // reads the right string. The right operand must be reloaded from the
        // safepoint slot after that scavenge.
        let lhs_bits = slots[0];
        slots[0] = Value::boolean(false).to_abi_bits();
        slots[1] = lhs_bits;
        let pair = STRING_CONCAT_ALLOC
            .invoke_raw(&mut ctx, 24, slots[0], slots[1], slots[2])
            .expect("entry");
        assert_eq!(probe_status(pair), NativeResultStatus::Success);
        let string = probe_value(pair)
            .and_then(|value| value.as_string(interp.gc_heap()))
            .expect("string value");
        assert_eq!(string.to_lossy_string(interp.gc_heap()), "falsek");

        let pair = STRING_CONCAT_ALLOC
            .invoke_raw(
                &mut ctx,
                24,
                n(1).to_abi_bits(),
                n(2).to_abi_bits(),
                slots[2],
            )
            .expect("entry");
        assert_eq!(probe_status(pair), NativeResultStatus::SideExit);
    }

    #[test]
    fn array_construct_alloc_entry_builds_empty_and_dense_hole_arrays() {
        let mut interp = Interpreter::new();
        let safepoints = [SafepointRecord::frame_slot_window(25, NO_FRAME_STATE, 3)];
        let mut slots = [
            n(0).to_abi_bits(),
            Value::undefined().to_abi_bits(),
            Value::undefined().to_abi_bits(),
        ];
        let mut ctx = test_alloc_context(&mut interp, &mut slots, &safepoints, 25);

        let pair = ARRAY_CONSTRUCT_ALLOC
            .invoke_raw(&mut ctx, 25, slots[0], slots[1], slots[2])
            .expect("entry");
        assert_eq!(probe_status(pair), NativeResultStatus::Success);
        let empty = probe_value(pair)
            .and_then(Value::as_array)
            .expect("empty array");
        assert_eq!(crate::array::len(empty, interp.gc_heap()), 0);

        let pair = ARRAY_CONSTRUCT_ALLOC
            .invoke_raw(&mut ctx, 25, n(8).to_abi_bits(), slots[1], slots[2])
            .expect("entry");
        assert_eq!(probe_status(pair), NativeResultStatus::Success);
        let array = probe_value(pair)
            .and_then(Value::as_array)
            .expect("length array");
        assert_eq!(crate::array::len(array, interp.gc_heap()), 8);
        assert!(!crate::array::has_own_element(array, interp.gc_heap(), 0));
        assert!(!crate::array::has_own_element(array, interp.gc_heap(), 7));
    }

    #[test]
    fn array_construct_alloc_entry_misses_invalid_length_before_allocation() {
        let mut interp = Interpreter::new();
        let safepoints = [SafepointRecord::frame_slot_window(26, NO_FRAME_STATE, 3)];
        let mut slots = [
            n(0).to_abi_bits(),
            Value::undefined().to_abi_bits(),
            Value::undefined().to_abi_bits(),
        ];
        let mut ctx = test_alloc_context(&mut interp, &mut slots, &safepoints, 26);
        let before = interp.gc_heap().stats();

        for invalid in [
            n(-1),
            Value::number_f64(7.0),
            Value::number_f64(1.5),
            Value::undefined(),
        ] {
            let pair = ARRAY_CONSTRUCT_ALLOC
                .invoke_raw(&mut ctx, 26, invalid.to_abi_bits(), slots[1], slots[2])
                .expect("entry");
            assert_eq!(probe_status(pair), NativeResultStatus::SideExit);
        }

        let after = interp.gc_heap().stats();
        assert_eq!(after.allocated_bytes, before.allocated_bytes);
        assert_eq!(after.new_allocated_bytes, before.new_allocated_bytes);
        assert_eq!(after.old_allocated_bytes, before.old_allocated_bytes);
    }

    #[test]
    fn array_construct_alloc_entry_reports_dense_backing_oom() {
        let mut interp = Interpreter::with_string_heap_cap(2 * 1024 * 1024);
        let safepoints = [SafepointRecord::frame_slot_window(27, NO_FRAME_STATE, 3)];
        let mut slots = [
            n(1_048_576).to_abi_bits(),
            Value::undefined().to_abi_bits(),
            Value::undefined().to_abi_bits(),
        ];
        let mut ctx = test_alloc_context(&mut interp, &mut slots, &safepoints, 27);

        let pair = ARRAY_CONSTRUCT_ALLOC
            .invoke_raw(&mut ctx, 27, slots[0], slots[1], slots[2])
            .expect("entry");
        assert_eq!(probe_status(pair), NativeResultStatus::OutOfMemory);
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
            inline_frames: Box::default(),
            inline_frames_published: false,
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
            inline_frames: Box::default(),
            inline_frames_published: false,
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
        assert_eq!(probe_status(pair), NativeResultStatus::SideExit);

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
        assert_eq!(probe_status(pair), NativeResultStatus::SideExit);
    }

    #[test]
    fn map_get_leaf_hits_flat_key() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let map = collections::alloc_map(&mut heap).expect("map");
        let key = crate::string::JsString::from_str("k", &mut heap).expect("key");
        collections::map_set(map, &mut heap, Value::string(key), n(42)).expect("set");

        let pair = collection_map_get_leaf(
            &heap as *const otter_gc::GcHeap,
            Value::map(map).to_abi_bits(),
            Value::string(key).to_abi_bits(),
        );
        assert_eq!(probe_status(pair), NativeResultStatus::Success);
        assert_eq!(probe_value(pair), Some(n(42)));

        let result = invoke_leaf_no_alloc_stub2(
            &heap,
            STUB_COLLECTION_MAP_GET_LEAF.id,
            Value::map(map),
            Value::string(key),
        );
        assert_eq!(probe_status(result), NativeResultStatus::Success);
        assert_eq!(probe_value(result), Some(n(42)));
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

        let pair = collection_map_has_leaf(
            &heap as *const otter_gc::GcHeap,
            Value::map(map).to_abi_bits(),
            Value::string(rope).to_abi_bits(),
        );
        assert_eq!(probe_status(pair), NativeResultStatus::SideExit);
        assert_eq!(probe_value(pair), None);
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
        assert_eq!(probe_status(inserted), NativeResultStatus::Success);

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
        assert_eq!(probe_status(inserted), NativeResultStatus::Success);

        let leaf = collection_map_has_leaf(
            &interp.gc_heap as *const otter_gc::GcHeap,
            Value::map(map).to_abi_bits(),
            Value::string(lookup_rope).to_abi_bits(),
        );
        assert_eq!(probe_status(leaf), NativeResultStatus::SideExit);

        let mut map_slots = [
            Value::map(map).to_abi_bits(),
            Value::string(lookup_rope).to_abi_bits(),
            Value::undefined().to_abi_bits(),
        ];
        let mut map_ctx = test_alloc_context(&mut interp, &mut map_slots, &safepoints, 23);
        let get = COLLECTION_MAP_GET_ALLOC
            .invoke_raw(&mut map_ctx, 23, map_slots[0], map_slots[1], map_slots[2])
            .expect("map get entry");
        assert_eq!(probe_status(get), NativeResultStatus::Success);
        assert_eq!(probe_value(get), Some(n(77)));

        let has = COLLECTION_MAP_HAS_ALLOC
            .invoke_raw(&mut map_ctx, 23, map_slots[0], map_slots[1], map_slots[2])
            .expect("map has entry");
        assert_eq!(probe_status(has), NativeResultStatus::Success);
        assert_eq!(probe_value(has), Some(Value::boolean(true)));

        let deleted = COLLECTION_MAP_DELETE_ALLOC
            .invoke_raw(&mut map_ctx, 23, map_slots[0], map_slots[1], map_slots[2])
            .expect("map delete entry");
        assert_eq!(probe_status(deleted), NativeResultStatus::Success);
        assert_eq!(probe_value(deleted), Some(Value::boolean(true)));

        let mut set_slots = [
            Value::set(set).to_abi_bits(),
            Value::string(lookup_rope).to_abi_bits(),
            Value::undefined().to_abi_bits(),
        ];
        let mut set_ctx = test_alloc_context(&mut interp, &mut set_slots, &safepoints, 23);
        let has = COLLECTION_SET_HAS_ALLOC
            .invoke_raw(&mut set_ctx, 23, set_slots[0], set_slots[1], set_slots[2])
            .expect("set has entry");
        assert_eq!(probe_status(has), NativeResultStatus::Success);
        assert_eq!(probe_value(has), Some(Value::boolean(true)));

        let deleted = COLLECTION_SET_DELETE_ALLOC
            .invoke_raw(&mut set_ctx, 23, set_slots[0], set_slots[1], set_slots[2])
            .expect("set delete entry");
        assert_eq!(probe_status(deleted), NativeResultStatus::Success);
        assert_eq!(probe_value(deleted), Some(Value::boolean(true)));
    }

    #[test]
    fn set_has_leaf_hits_flat_key() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let set = collections::alloc_set(&mut heap).expect("set");
        collections::set_add(set, &mut heap, n(7)).expect("add");

        let pair = collection_set_has_leaf(
            &heap as *const otter_gc::GcHeap,
            Value::set(set).to_abi_bits(),
            n(7).to_abi_bits(),
        );
        assert_eq!(probe_status(pair), NativeResultStatus::Success);
        assert_eq!(probe_value(pair), Some(Value::boolean(true)));
    }

    #[test]
    fn leaf_stub_entries_miss_null_heap() {
        let pair = collection_map_get_leaf(
            std::ptr::null(),
            Value::undefined().to_abi_bits(),
            Value::undefined().to_abi_bits(),
        );
        assert_eq!(probe_status(pair), NativeResultStatus::SideExit);
        assert_eq!(probe_value(pair), None);
    }

    #[test]
    fn static_entries_type_every_vm_owned_descriptor() {
        for descriptor in crate::native_abi::RUNTIME_STUB_DESCRIPTORS {
            if is_vm_owned_runtime_stub(descriptor.id) {
                match descriptor.signature {
                    crate::native_abi::RuntimeStubSignature::LeafValue2 => {
                        assert!(leaf_no_alloc_stub2_by_id(descriptor.id).is_some());
                    }
                    crate::native_abi::RuntimeStubSignature::Float64Leaf2 => {
                        assert!(
                            float64_leaf_stub2_by_id(descriptor.id)
                                .is_some_and(Float64LeafStub2::is_valid)
                        );
                    }
                    crate::native_abi::RuntimeStubSignature::Float64ToWordLeaf1 => {
                        assert!(
                            float64_to_word_leaf_stub1_by_id(descriptor.id)
                                .is_some_and(Float64ToWordLeafStub1::is_valid)
                        );
                    }
                    crate::native_abi::RuntimeStubSignature::MutatingLeafValue2 => {
                        assert!(
                            mutating_leaf_stub2_by_id(descriptor.id)
                                .is_some_and(MutatingLeafStub2::is_valid)
                        );
                    }
                    crate::native_abi::RuntimeStubSignature::MutatingLeafValue3 => {
                        assert!(
                            mutating_leaf_stub3_by_id(descriptor.id)
                                .is_some_and(MutatingLeafStub3::is_valid)
                        );
                    }
                    crate::native_abi::RuntimeStubSignature::AllocValue3 => {
                        assert!(
                            alloc_value_stub_by_id(descriptor.id)
                                .and_then(|stub| stub.entry)
                                .is_some()
                        );
                    }
                    signature => panic!("VM-owned descriptor has unexpected {signature:?}"),
                }
            } else {
                assert!(matches!(
                    descriptor.signature,
                    crate::native_abi::RuntimeStubSignature::Poll1
                        | crate::native_abi::RuntimeStubSignature::Variadic
                        | crate::native_abi::RuntimeStubSignature::ContextWords
                        | crate::native_abi::RuntimeStubSignature::ReentrantValue2
                        | crate::native_abi::RuntimeStubSignature::ReentrantValue3
                        | crate::native_abi::RuntimeStubSignature::ReentrantNamedLoad
                        | crate::native_abi::RuntimeStubSignature::ReentrantNamedStore
                        | crate::native_abi::RuntimeStubSignature::ReentrantValueSpan
                        | crate::native_abi::RuntimeStubSignature::CommittedValue2
                        | crate::native_abi::RuntimeStubSignature::RouteThrow1
                        | crate::native_abi::RuntimeStubSignature::AcknowledgeCaughtThrow0
                ));
            }
        }
    }

    struct BindingHook(Vec<crate::jit::JitRuntimeStubBinding>);

    impl crate::jit::JitCompilerHook for BindingHook {
        fn runtime_stub_bindings(&self) -> Vec<crate::jit::JitRuntimeStubBinding> {
            self.0.clone()
        }

        fn compile_function(
            &self,
            _request: crate::jit::JitCompileRequest,
        ) -> Result<crate::jit::JitCompileStatus, crate::jit::JitCompileError> {
            Ok(crate::jit::JitCompileStatus::Unavailable)
        }
    }

    fn poll_binding() -> crate::jit::JitRuntimeStubBinding {
        let descriptor = crate::native_abi::STUB_JIT_BACKEDGE_POLL;
        crate::jit::JitRuntimeStubBinding {
            id: descriptor.id,
            signature: descriptor.signature,
            entry_addr: 0x1000,
        }
    }

    /// One fake binding per compiler-owned inventory slot.
    fn all_jit_bindings() -> Vec<crate::jit::JitRuntimeStubBinding> {
        crate::native_abi::RUNTIME_STUB_DESCRIPTORS
            .iter()
            .filter(|descriptor| !is_vm_owned_runtime_stub(descriptor.id))
            .map(|descriptor| crate::jit::JitRuntimeStubBinding {
                id: descriptor.id,
                signature: descriptor.signature,
                entry_addr: 0x1000 + descriptor.id as usize,
            })
            .collect()
    }

    #[test]
    fn jit_binding_installation_validates_inventory_without_persistent_table() {
        let mut interp = Interpreter::new();
        assert!(!interp.jit_compiler_installed());
        interp.set_jit_compiler(Some(std::sync::Arc::new(BindingHook(all_jit_bindings()))));
        assert!(interp.jit_compiler_installed());
        interp.set_jit_compiler(None);
        assert!(!interp.jit_compiler_installed());
    }

    #[test]
    #[should_panic(expected = "signature family")]
    fn jit_binding_with_wrong_signature_family_panics() {
        let mut binding = poll_binding();
        binding.signature = crate::native_abi::RuntimeStubSignature::LeafValue2;
        Interpreter::new().set_jit_compiler(Some(std::sync::Arc::new(BindingHook(vec![binding]))));
    }

    #[test]
    #[should_panic(expected = "JIT-owned slot")]
    fn jit_binding_cannot_overwrite_vm_owned_slot() {
        let descriptor = crate::native_abi::STUB_COLLECTION_MAP_GET_LEAF;
        let binding = crate::jit::JitRuntimeStubBinding {
            id: descriptor.id,
            signature: descriptor.signature,
            entry_addr: 0x1000,
        };
        Interpreter::new().set_jit_compiler(Some(std::sync::Arc::new(BindingHook(vec![binding]))));
    }

    #[test]
    #[should_panic(expected = "left vacant")]
    fn jit_installation_requires_complete_inventory() {
        Interpreter::new().set_jit_compiler(Some(std::sync::Arc::new(BindingHook(Vec::new()))));
    }

    #[test]
    #[should_panic(expected = "duplicated")]
    fn jit_installation_rejects_duplicate_bindings() {
        let mut bindings = all_jit_bindings();
        bindings.push(bindings[0]);
        Interpreter::new().set_jit_compiler(Some(std::sync::Arc::new(BindingHook(bindings))));
    }
}
