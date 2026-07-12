//! Native entry context and machine-visible layout constants.
//!
//! # Contents
//! - The C-layout context and return pair used by compiled entries.
//! - Offset constants baked by architecture-specific templates.
//! - Compile-time layout derivation from VM-owned ABI records.
//!
//! # Invariants
//! Every offset is derived with `offset_of!`; emitted code must not duplicate
//! Rust layout knowledge outside this module. Context pointers remain valid for
//! the dynamic extent of one compiled activation.
//!
//! # See also
//! - `otter_vm::native_abi` — authoritative VM frame and thread records.

use otter_vm::{
    RuntimeStubAllocContext, VmError, VmRuntimeActivation,
    native_abi::{NativeFrame, VmThread},
};
/// (offset 0) and `self_closure` (offset 8) directly by offset — keep those two
/// first. The full struct is machine-constructible: nested direct calls copy
/// plain pointers/scalars and share the caller's initialized `error` slot.
#[repr(C)]
pub(crate) struct JitCtx {
    /// Base of the executing frame's register window (`*mut u64` over Values).
    pub(super) regs: *mut u64,
    /// Boxed `Value` bits of this frame's SELF closure (the named-function self
    /// binding). Read directly by a `MakeFunction`-of-self at offset 8.
    pub(super) self_closure: u64,
    /// Boxed `Value` bits of this frame's `this` binding, read once at entry.
    /// A `LoadThis` reads it directly at offset 16 (and bails on a hole).
    pub(super) this_value: u64,
    /// Logical PC of the instruction currently executing, written by compiled
    /// code before each op (offset [`RESUME_PC_OFFSET`]). On a guard bail the
    /// interpreter resumes here — the exact instruction, not the entry/loop
    /// header — which is what makes bailing out of a loop body that has
    /// already committed side effects (or out of an unsupported opcode)
    /// correct. Read by `enter_at` on `STATUS_BAILED`.
    pub(super) resume_pc: u32,
    /// Sole machine-visible VM state pointer.
    pub(super) thread: *mut VmThread,
    /// Published authoritative activation.
    pub(super) native_frame: *mut NativeFrame,
    /// Index of the executing frame within `stack`.
    pub(super) frame_index: usize,
    /// Base of this frame's upvalue spine (`Box<[UpvalueCell]>` data; each a
    /// 4-byte compressed cell handle), or `0` when the frame captures nothing
    /// or the function captures nothing. Inline `LoadUpvalue` /
    /// `StoreUpvalue` read `[upvalues_ptr + idx*4]`.
    pub(super) upvalues_ptr: usize,
    /// Error slot shared by direct callees and bridge stubs when a re-entered
    /// operation throws. Pointer form keeps `JitCtx` constructible by emitted
    /// code; assembly never initializes a Rust enum in place.
    pub(super) error: *mut Option<VmError>,
    /// Prepared direct-call callee entry address.
    pub(super) direct_entry_addr: usize,
    /// Prepared direct-call callee register base.
    pub(super) direct_regs: *mut u64,
    /// Prepared direct-call callee SELF bits.
    pub(super) direct_self_closure: u64,
    /// Prepared direct-call callee `this` bits.
    pub(super) direct_this_value: u64,
    /// Prepared direct-call callee frame index.
    pub(super) direct_frame_index: usize,
    /// Prepared direct-call callee upvalue-spine base (staged from
    /// [`otter_vm::JitPreparedDirectCall::upvalues_ptr`]); the dispatch tail
    /// copies it into the callee `JitCtx.upvalues_ptr`.
    pub(super) direct_upvalues_ptr: usize,
    /// Base of the interpreter's flat JIT register stack
    /// (`reg_stack[0]`). Compiled code builds a self-recursive callee window at
    /// `reg_stack_base + reg_top*8` without a Rust frame-build bridge.
    pub(super) reg_stack_base: *mut u64,
    /// Address of the interpreter's `reg_top` (live extent of the flat register
    /// stack, in slots). Compiled code loads it, reserves a callee window by
    /// adding the callee register count, and stores it back; the matching pop on
    /// return restores it.
    pub(super) reg_top_ptr: *mut usize,
    /// Shared synchronous native-reentry depth counter.
    pub(super) sync_reentry_depth_ptr: *mut u32,
    /// Effective limit checked before a frameless native call mutates state.
    pub(super) sync_reentry_limit: u32,
    /// Address of the live array-index accessor protector. Dense array stores
    /// read through this pointer at the store site, not at entry, because a
    /// re-entered VM call can invalidate the protector before later stores.
    pub(super) array_index_accessor_protector_ptr: *const bool,
    /// Opaque heap pointer for native leaf runtime stubs.
    pub(super) gc_heap: *const std::ffi::c_void,
    /// Address of the cooperative interrupt flag's backing byte. Compiled code
    /// polls this inline at every back-edge and re-enters only when it is set.
    pub(super) interrupt_flag: *const u8,
    /// Address of the VM's back-edge fuel counter. Compiled code decrements it
    /// inline per back-edge and re-enters the poll stub when it reaches zero,
    /// batching the budget checkpoint across the whole run of iterations.
    pub(super) backedge_fuel: *mut u64,
}

impl JitCtx {
    /// VM-owned activation published through the sole machine-visible thread
    /// pointer. Runtime stubs use this explicitly; emitted code never observes
    /// its Rust pointers or container types.
    pub(super) fn activation(&self) -> &VmRuntimeActivation {
        // SAFETY: runtime-capable contexts point at the VmThread built for the
        // current entry, whose runtime_context retains VmRuntimeActivation.
        unsafe { &*((*self.thread).runtime_context as *const VmRuntimeActivation) }
    }
}

/// Two-word return of compiled code (`x0`/`x1` on arm64).
#[repr(C)]
pub(crate) struct JitRet {
    pub(super) value: u64,
    pub(super) status: u64,
}

/// `status` discriminants in [`JitRet`].
pub(crate) const STATUS_RETURNED: u64 = 0;
pub(crate) const STATUS_BAILED: u64 = 1;
pub(crate) const STATUS_THREW: u64 = 2;

/// Byte offset of [`JitCtx::resume_pc`] — where compiled code stamps the
/// current logical PC before each op so a bail resumes at the exact site.
pub(crate) const RESUME_PC_OFFSET: u32 = std::mem::offset_of!(JitCtx, resume_pc) as u32;
/// Byte offset of [`JitCtx::error`] for nested direct-call context construction.
#[allow(dead_code)]
pub(crate) const ERROR_SLOT_OFFSET: u32 = std::mem::offset_of!(JitCtx, error) as u32;
pub(crate) const THREAD_OFFSET: u32 = std::mem::offset_of!(JitCtx, thread) as u32;
pub(crate) const NATIVE_FRAME_OFFSET: u32 = std::mem::offset_of!(JitCtx, native_frame) as u32;
pub(crate) const NATIVE_FRAME_CODE_OBJECT_ID_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, code_object_id) as u32;
/// Byte offset of [`JitCtx::interrupt_flag`] — the inline back-edge interrupt poll.
pub(crate) const INTERRUPT_FLAG_OFFSET: u32 = std::mem::offset_of!(JitCtx, interrupt_flag) as u32;
/// Byte offset of [`JitCtx::backedge_fuel`] — the inline back-edge fuel counter.
pub(crate) const BACKEDGE_FUEL_OFFSET: u32 = std::mem::offset_of!(JitCtx, backedge_fuel) as u32;
pub(crate) const FRAME_INDEX_OFFSET: u32 = std::mem::offset_of!(JitCtx, frame_index) as u32;
/// Byte offset of [`JitCtx::upvalues_ptr`] for inline upvalue access.
pub(crate) const UPVALUES_PTR_OFFSET: u32 = std::mem::offset_of!(JitCtx, upvalues_ptr) as u32;
/// Byte offset of [`JitCtx::reg_stack_base`] — the flat JIT register stack base
/// used to build a self-recursive callee window inline.
pub(crate) const REG_STACK_BASE_OFFSET: u32 = std::mem::offset_of!(JitCtx, reg_stack_base) as u32;
/// Byte offset of [`JitCtx::reg_top_ptr`] — the address of the interpreter's
/// `reg_top`, bumped to reserve a callee window and restored on return.
pub(crate) const REG_TOP_PTR_OFFSET: u32 = std::mem::offset_of!(JitCtx, reg_top_ptr) as u32;
pub(crate) const SYNC_REENTRY_DEPTH_PTR_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, sync_reentry_depth_ptr) as u32;
pub(crate) const SYNC_REENTRY_LIMIT_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, sync_reentry_limit) as u32;
pub(crate) const ARRAY_INDEX_ACCESSOR_PROTECTOR_PTR_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, array_index_accessor_protector_ptr) as u32;
#[allow(dead_code)]
pub(crate) const GC_HEAP_OFFSET: u32 = std::mem::offset_of!(JitCtx, gc_heap) as u32;
pub(crate) const ALLOC_CTX_THREAD_OFFSET: u32 =
    std::mem::offset_of!(RuntimeStubAllocContext, thread) as u32;
pub(crate) const ALLOC_CTX_FRAME_OFFSET: u32 =
    std::mem::offset_of!(RuntimeStubAllocContext, frame) as u32;
pub(crate) const ALLOC_CTX_CODE_OBJECT_ID_OFFSET: u32 =
    std::mem::offset_of!(RuntimeStubAllocContext, code_object_id) as u32;
pub(crate) const ALLOC_CTX_SAFEPOINT_ID_OFFSET: u32 =
    std::mem::offset_of!(RuntimeStubAllocContext, safepoint_id) as u32;
pub(crate) const ALLOC_CTX_RESERVED0_OFFSET: u32 =
    std::mem::offset_of!(RuntimeStubAllocContext, reserved0) as u32;
pub(crate) const ALLOC_CTX_RESERVED1_OFFSET: u32 =
    std::mem::offset_of!(RuntimeStubAllocContext, reserved1) as u32;
pub(crate) const ALLOC_CTX_SPILL_SLOTS_OFFSET: u32 =
    std::mem::offset_of!(RuntimeStubAllocContext, spill_slots) as u32;
pub(crate) const ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET: u32 =
    std::mem::offset_of!(RuntimeStubAllocContext, spill_slot_count) as u32;
pub(crate) const ALLOC_CTX_STACK_SIZE: u32 =
    ((std::mem::size_of::<RuntimeStubAllocContext>() + 15) & !15) as u32;
/// Size of one `UpvalueCell` (a 4-byte compressed `Gc<UpvalueCellBody>`).
pub(crate) const UPVALUE_CELL_SIZE: u32 = 4;
/// Byte offset of the single `Value` inside an `UpvalueCellBody` from its
/// decompressed pointer (just past the 8-byte `GcHeader`).
pub(crate) const UPVALUE_VALUE_OFFSET: u32 = 8;
pub(crate) const DIRECT_ENTRY_OFFSET: u32 = std::mem::offset_of!(JitCtx, direct_entry_addr) as u32;
pub(crate) const DIRECT_REGS_OFFSET: u32 = std::mem::offset_of!(JitCtx, direct_regs) as u32;
pub(crate) const DIRECT_SELF_OFFSET: u32 = std::mem::offset_of!(JitCtx, direct_self_closure) as u32;
pub(crate) const DIRECT_THIS_OFFSET: u32 = std::mem::offset_of!(JitCtx, direct_this_value) as u32;
/// Byte offset of the precomputed `this` bits in [`JitCtx`], for inline
/// `LoadThis` in baseline entries.
pub(crate) const THIS_VALUE_OFFSET: u32 = std::mem::offset_of!(JitCtx, this_value) as u32;
pub(crate) const DIRECT_FRAME_INDEX_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, direct_frame_index) as u32;
pub(crate) const DIRECT_UPVALUES_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, direct_upvalues_ptr) as u32;
pub(crate) const JIT_CTX_STACK_SIZE: u32 = ((std::mem::size_of::<JitCtx>() + 15) & !15) as u32;

/// Compiled-code entry signature.
pub(super) type JitEntry = extern "C" fn(*mut JitCtx) -> JitRet;
