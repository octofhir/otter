//! Machine-visible offsets derived from the VM-owned execution ABI.
//!
//! # Contents
//! - Shared VM entry types consumed by compiled entries.
//! - Offset constants baked by architecture-specific templates.
//! - Compile-time layout derivation from VM-owned ABI records.
//! - The collector-published nursery window and exact generated-allocation
//!   counters carried by each compiled activation.
//!
//! # Invariants
//! `JitCtx` offsets are derived with `offset_of!` here. Offsets for VM-private
//! fields are imported from `otter_vm::native_abi`; the JIT never re-derives
//! them from private Rust fields. Context pointers remain valid for the dynamic
//! extent of one compiled activation. Nursery-window pointers are consumed
//! only by the no-safepoint allocation sequence and refreshed by its rooted
//! cold sibling.
//!
//! # See also
//! - `otter_vm::native_abi` — authoritative VM frame and thread records.

pub(crate) use otter_vm::native_abi::JitCtx;
#[cfg(test)]
pub(crate) use otter_vm::native_abi::JitEntry;
use otter_vm::native_abi::{CodeEntryCell, Frame, RuntimeStubAllocContext, VmThread};

pub(crate) const THREAD_OFFSET: u32 = std::mem::offset_of!(JitCtx, thread) as u32;
pub(crate) const NATIVE_FRAME_OFFSET: u32 = std::mem::offset_of!(JitCtx, native_frame) as u32;
/// Offset of the call request a generated caller writes before entering the
/// common call trampoline.
pub(crate) const PENDING_CALL_OFFSET: u32 = std::mem::offset_of!(JitCtx, pending_call) as u32;
/// Request field offsets relative to [`PENDING_CALL_OFFSET`].
pub(crate) const REQUEST_ENTRY_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::native_abi::CallRequest, entry) as u32;
/// Request frame-flag byte, carrying `CONSTRUCT` for `[[Construct]]`.
pub(crate) const REQUEST_FLAGS_OFFSET: u32 = (std::mem::offset_of!(
    otter_vm::native_abi::CallRequest,
    header
) + std::mem::offset_of!(otter_vm::native_abi::VmFrameHeader, flags))
    as u32;
/// Request callee value.
pub(crate) const REQUEST_CALLEE_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::native_abi::CallRequest, callee) as u32;
/// Request receiver value.
pub(crate) const REQUEST_RECEIVER_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::native_abi::CallRequest, receiver) as u32;
/// Request `new.target` value.
pub(crate) const REQUEST_NEW_TARGET_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::native_abi::CallRequest, new_target) as u32;
/// Request word holding the restored-register count and, above it, the
/// return destination.
pub(crate) const REQUEST_REGISTER_SEED_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::native_abi::CallRequest, initial_register_count) as u32;
/// Request word holding the cold record and arguments identity.
pub(crate) const REQUEST_COLD_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::native_abi::CallRequest, cold) as u32;
const _: () = assert!(
    REQUEST_REGISTER_SEED_OFFSET + 4
        == std::mem::offset_of!(otter_vm::native_abi::CallRequest, return_destination) as u32
);
const _: () = assert!(
    REQUEST_COLD_OFFSET + 4
        == std::mem::offset_of!(otter_vm::native_abi::CallRequest, arguments_object) as u32
);
/// Byte offset of the canonical instruction-index PC in the published native
/// frame. Generated code updates this together with its nested-call exit
/// payload before any opcode can observe or mutate JavaScript state.
pub(crate) const NATIVE_FRAME_PC_OFFSET: u32 = (std::mem::offset_of!(Frame, header)
    + std::mem::offset_of!(otter_vm::native_abi::VmFrameHeader, pc))
    as u32;
/// Byte offsets of the isolate-published cells on [`VmThread`] read by
/// emitted code: interrupt poll byte, back-edge fuel counter, and the
/// leaf-stub heap pointer.
pub(crate) const VM_THREAD_INTERRUPT_CELL_OFFSET: u32 =
    std::mem::offset_of!(VmThread, interrupt_cell) as u32;
pub(crate) const VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET: u32 =
    std::mem::offset_of!(VmThread, backedge_fuel_cell) as u32;
pub(crate) const VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET: u32 =
    std::mem::offset_of!(VmThread, global_lexical_epoch_cell) as u32;
pub(crate) const VM_THREAD_GC_HEAP_OFFSET: u32 = std::mem::offset_of!(VmThread, gc_heap) as u32;
#[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
pub(crate) const VM_THREAD_MARKING_FLAG_CELL_OFFSET: u32 =
    std::mem::offset_of!(VmThread, marking_flag_cell) as u32;
pub(crate) const VM_THREAD_ARRAY_INDEX_PROTECTOR_CELL_OFFSET: u32 =
    std::mem::offset_of!(VmThread, array_index_protector_cell) as u32;
#[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
pub(crate) const VM_THREAD_ARRAY_BUFFER_DETACH_PROTECTOR_CELL_OFFSET: u32 =
    std::mem::offset_of!(VmThread, array_buffer_detach_protector_cell) as u32;
pub(crate) const VM_THREAD_ACTIVE_REALM_CELL_OFFSET: u32 =
    std::mem::offset_of!(VmThread, active_realm_cell) as u32;
pub(crate) const GLOBAL_THIS_OFFSET_PTR_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, global_this_offset) as u32;
pub(crate) const NATIVE_STACK_LIMIT_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, native_stack_limit) as u32;
pub(crate) const GENERATED_FEEDBACK_CLEAN_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, generated_feedback_clean) as u32;
pub(crate) const ALLOC_WINDOW_LAB_OFFSET: u32 = (std::mem::offset_of!(JitCtx, alloc_window)
    + std::mem::offset_of!(otter_vm::jit::JitMachineAllocationWindow, lab))
    as u32;
pub(crate) const ALLOC_WINDOW_TYPE_STATS_OFFSET: u32 = (std::mem::offset_of!(JitCtx, alloc_window)
    + std::mem::offset_of!(otter_vm::jit::JitMachineAllocationWindow, type_stats))
    as u32;
pub(crate) const RUNTIME_STATS_OFFSET: u32 = std::mem::offset_of!(JitCtx, runtime_stats) as u32;
pub(crate) const RECEIVER_ALLOC_ATTEMPTS_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::JitRuntimeStats, receiver_alloc_attempts) as u32;
pub(crate) const RECEIVER_ALLOC_GENERATED_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::JitRuntimeStats, receiver_alloc_generated) as u32;
pub(crate) const RECEIVER_ALLOC_GUARD_MISSES_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::JitRuntimeStats, receiver_alloc_guard_misses) as u32;
pub(crate) const RECEIVER_ALLOC_SPACE_MISSES_OFFSET: u32 =
    std::mem::offset_of!(otter_vm::JitRuntimeStats, receiver_alloc_space_misses) as u32;
pub(crate) const LAB_TOP_OFFSET: u32 = otter_vm::jit::JIT_LAB_TOP_OFFSET;
pub(crate) const LAB_LIMIT_OFFSET: u32 = otter_vm::jit::JIT_LAB_LIMIT_OFFSET;
pub(crate) const ALLOC_CTX_THREAD_OFFSET: u32 =
    std::mem::offset_of!(RuntimeStubAllocContext, thread) as u32;
pub(crate) const ALLOC_CTX_SAFEPOINT_ID_OFFSET: u32 =
    std::mem::offset_of!(RuntimeStubAllocContext, safepoint_id) as u32;
pub(crate) const ALLOC_CTX_SPILL_SLOTS_OFFSET: u32 =
    std::mem::offset_of!(RuntimeStubAllocContext, spill_slots) as u32;
pub(crate) const ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET: u32 =
    std::mem::offset_of!(RuntimeStubAllocContext, spill_slot_count) as u32;
pub(crate) const ALLOC_CTX_STACK_SIZE: u32 =
    ((std::mem::size_of::<RuntimeStubAllocContext>() + 15) & !15) as u32;
/// Fixed-layout fields consumed by native call linkage. Generation cells are
/// boxed by the isolate registry and never reused.
pub(crate) const CODE_ENTRY_GENERATED_ENTRIES_OFFSET: u32 =
    std::mem::offset_of!(CodeEntryCell, generated_entries) as u32;
pub(crate) const CODE_ENTRY_TIERING_BREAK_EVEN_OFFSET: u32 =
    std::mem::offset_of!(CodeEntryCell, generated_tiering_break_even) as u32;
pub(crate) const CODE_ENTRY_TIERING_ENABLED_OFFSET: u32 =
    std::mem::offset_of!(CodeEntryCell, generated_tiering_enabled) as u32;
/// Byte offsets of the callee-frame fields emitted nested-call sequences fill,
/// re-exported from the VM-owned [`Frame`] layout.
pub(crate) use otter_vm::native_abi::{
    NATIVE_FRAME_CALL_SITE_OFFSET,
    NATIVE_FRAME_MACHINE_ROOTS_OFFSET, NATIVE_FRAME_NEW_TARGET_OFFSET,
    NATIVE_FRAME_REGISTER_BASE_OFFSET, NATIVE_FRAME_SELF_OFFSET, NATIVE_FRAME_THIS_OFFSET,
};
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub(crate) const NATIVE_FRAME_FLAGS_OFFSET: u32 = (std::mem::offset_of!(Frame, header)
    + std::mem::offset_of!(otter_vm::native_abi::VmFrameHeader, flags))
    as u32;
