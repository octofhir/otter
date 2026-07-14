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
/// Machine-visible context shared by every compiled tier.
///
/// Generated code reads `regs` (offset 0) and `self_closure` (offset 8)
/// directly, so those fields stay first. The full struct is
/// machine-constructible: nested direct calls copy plain pointers/scalars and
/// share the caller's initialized `error` slot.
#[repr(C)]
pub(crate) struct JitCtx {
    /// Base of the executing frame's register window (`*mut u64` over Values).
    pub(crate) regs: *mut u64,
    /// Boxed `Value` bits of this frame's SELF closure (the named-function self
    /// binding). Read directly by a `MakeFunction`-of-self at offset 8.
    pub(crate) self_closure: u64,
    /// Boxed `Value` bits of this frame's `this` binding, read once at entry.
    /// A `LoadThis` reads it directly at offset 16 (and bails on a hole).
    pub(crate) this_value: u64,
    /// Sole machine-visible VM state pointer.
    pub(crate) thread: *mut VmThread,
    /// Published authoritative activation.
    pub(crate) native_frame: *mut NativeFrame,
    /// Index of the executing frame within `stack`.
    pub(crate) frame_index: usize,
    /// Base of this frame's upvalue spine (`Box<[UpvalueCell]>` data; each a
    /// 4-byte compressed cell handle), or `0` when the frame captures nothing
    /// or the function captures nothing. Inline `LoadUpvalue` /
    /// `StoreUpvalue` read `[upvalues_ptr + idx*4]`.
    pub(crate) upvalues_ptr: usize,
    /// Error slot shared by direct callees and bridge stubs when a re-entered
    /// operation throws. Pointer form keeps `JitCtx` constructible by emitted
    /// code; assembly never initializes a Rust enum in place.
    pub(crate) error: *mut Option<VmError>,
    /// Prepared direct-call callee entry address.
    pub(crate) direct_entry_addr: usize,
    /// Prepared direct-call callee register base.
    pub(crate) direct_regs: *mut u64,
    /// Prepared direct-call callee SELF bits.
    pub(crate) direct_self_closure: u64,
    /// Prepared direct-call callee `this` bits.
    pub(crate) direct_this_value: u64,
    /// Prepared direct-call callee frame index.
    pub(crate) direct_frame_index: usize,
    /// Prepared direct-call callee upvalue-spine base (staged from
    /// [`otter_vm::JitPreparedDirectCall::upvalues_ptr`]); the dispatch tail
    /// copies it into the callee `JitCtx.upvalues_ptr`.
    pub(crate) direct_upvalues_ptr: usize,
    /// Prepared callee native-frame identity word (`function_id |
    /// code_block_id << 32`).
    pub(crate) direct_frame_ids: u64,
    /// Prepared callee native-frame header word at byte 8 with `pc = 0`
    /// (`register_count << 32 | kind << 48 | flags << 56`).
    pub(crate) direct_frame_meta: u64,
    /// Prepared callee installed code-object identity.
    pub(crate) direct_code_object_id: u64,
    /// Base of the interpreter's flat JIT register stack
    /// (`reg_stack[0]`). Compiled code builds a self-recursive callee window at
    /// `reg_stack_base + reg_top*8` without a Rust frame-build bridge.
    pub(crate) reg_stack_base: *mut u64,
    /// Address of the interpreter's `reg_top` (live extent of the flat register
    /// stack, in slots). Compiled code loads it, reserves a callee window by
    /// adding the callee register count, and stores it back; the matching pop on
    /// return restores it.
    pub(crate) reg_top_ptr: *mut usize,
}

impl JitCtx {
    /// VM-owned activation published through the sole machine-visible thread
    /// pointer. Runtime stubs use this explicitly; emitted code never observes
    /// its Rust pointers or container types.
    pub(crate) fn activation(&self) -> &VmRuntimeActivation {
        // SAFETY: runtime-capable contexts point at the VmThread built for the
        // current entry, whose runtime_context retains VmRuntimeActivation.
        unsafe { &*((*self.thread).runtime_context as *const VmRuntimeActivation) }
    }

    /// Published activation, or `None` when this entry carries no runtime
    /// context (fixture entries drive pure compiled code with no interpreter).
    /// The cooperative poll boundary must stay sound for such entries instead
    /// of dereferencing an absent activation.
    pub(crate) fn checked_activation(&self) -> Option<&VmRuntimeActivation> {
        if self.thread.is_null() {
            return None;
        }
        // SAFETY: a non-null thread points at the VmThread built for the
        // current entry.
        let runtime_context = unsafe { (*self.thread).runtime_context };
        if runtime_context == 0 {
            return None;
        }
        // SAFETY: a nonzero runtime_context retains VmRuntimeActivation for
        // this entry's dynamic extent.
        Some(unsafe { &*(runtime_context as *const VmRuntimeActivation) })
    }
}

/// Two-word return of compiled code (`x0`/`x1` on arm64).
#[repr(C)]
pub(crate) struct JitRet {
    pub(crate) value: u64,
    pub(crate) status: u64,
}

/// `status` discriminants in [`JitRet`].
pub(crate) const STATUS_RETURNED: u64 = 0;
pub(crate) const STATUS_BAILED: u64 = 1;
pub(crate) const STATUS_THREW: u64 = 2;
/// Internal runtime-transition result: the committed opcode completed and the
/// current machine-code fallthrough remains authoritative.
pub(crate) const STATUS_CONTINUE: u64 = 3;

/// Byte offset of [`JitCtx::error`] for nested direct-call context construction.
#[allow(dead_code)]
pub(crate) const ERROR_SLOT_OFFSET: u32 = std::mem::offset_of!(JitCtx, error) as u32;
pub(crate) const THREAD_OFFSET: u32 = std::mem::offset_of!(JitCtx, thread) as u32;
/// Byte offset of the SELF-closure bits in [`JitCtx`], for inline
/// named-function self bindings.
pub(crate) const SELF_CLOSURE_OFFSET: u32 = std::mem::offset_of!(JitCtx, self_closure) as u32;
pub(crate) const NATIVE_FRAME_OFFSET: u32 = std::mem::offset_of!(JitCtx, native_frame) as u32;
/// Byte offset of the canonical instruction-index PC in the published native
/// frame. Generated code updates this together with its nested-call exit
/// payload before any opcode can observe or mutate JavaScript state.
pub(crate) const NATIVE_FRAME_PC_OFFSET: u32 = (std::mem::offset_of!(NativeFrame, header)
    + std::mem::offset_of!(otter_vm::native_abi::VmFrameHeader, pc))
    as u32;
pub(crate) const NATIVE_FRAME_CODE_OBJECT_ID_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, code_object_id) as u32;
pub(crate) const FRAME_INDEX_OFFSET: u32 = std::mem::offset_of!(JitCtx, frame_index) as u32;
/// Byte offsets of the isolate-published cells on [`VmThread`] read by
/// emitted code: interrupt poll byte, back-edge fuel counter, and the
/// leaf-stub heap pointer.
pub(crate) const VM_THREAD_INTERRUPT_CELL_OFFSET: u32 =
    std::mem::offset_of!(VmThread, interrupt_cell) as u32;
pub(crate) const VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET: u32 =
    std::mem::offset_of!(VmThread, backedge_fuel_cell) as u32;
pub(crate) const VM_THREAD_GC_HEAP_OFFSET: u32 = std::mem::offset_of!(VmThread, gc_heap) as u32;
/// Byte offset of [`JitCtx::upvalues_ptr`] for inline upvalue access.
pub(crate) const UPVALUES_PTR_OFFSET: u32 = std::mem::offset_of!(JitCtx, upvalues_ptr) as u32;
/// Byte offset of [`JitCtx::reg_stack_base`] — the flat JIT register stack base
/// used to build a self-recursive callee window inline.
pub(crate) const REG_STACK_BASE_OFFSET: u32 = std::mem::offset_of!(JitCtx, reg_stack_base) as u32;
/// Byte offset of [`JitCtx::reg_top_ptr`] — the address of the interpreter's
/// `reg_top`, bumped to reserve a callee window and restored on return.
pub(crate) const REG_TOP_PTR_OFFSET: u32 = std::mem::offset_of!(JitCtx, reg_top_ptr) as u32;
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
pub(crate) const DIRECT_FRAME_IDS_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, direct_frame_ids) as u32;
pub(crate) const DIRECT_FRAME_META_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, direct_frame_meta) as u32;
pub(crate) const DIRECT_CODE_OBJECT_ID_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, direct_code_object_id) as u32;
pub(crate) const JIT_CTX_STACK_SIZE: u32 = ((std::mem::size_of::<JitCtx>() + 15) & !15) as u32;
/// 16-aligned machine-stack reservation for a nested callee's own published
/// [`NativeFrame`], placed immediately above its `JitCtx`.
pub(crate) const NATIVE_FRAME_STACK_SIZE: u32 =
    ((std::mem::size_of::<NativeFrame>() + 15) & !15) as u32;
/// Combined nested-call reservation: callee `JitCtx` at `sp`, callee
/// `NativeFrame` at `sp + JIT_CTX_STACK_SIZE`.
pub(crate) const CTX_PLUS_FRAME_STACK_SIZE: u32 = JIT_CTX_STACK_SIZE + NATIVE_FRAME_STACK_SIZE;
/// Byte offsets of the callee-frame fields emitted nested-call sequences fill.
pub(crate) const NATIVE_FRAME_PREVIOUS_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, previous_frame) as u32;
pub(crate) const NATIVE_FRAME_REGISTER_BASE_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, register_base) as u32;
pub(crate) const NATIVE_FRAME_ARGUMENT_BASE_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, argument_base) as u32;
pub(crate) const NATIVE_FRAME_FEEDBACK_BASE_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, feedback_base) as u32;
pub(crate) const NATIVE_FRAME_THIS_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, this_value_bits) as u32;
pub(crate) const NATIVE_FRAME_NEW_TARGET_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, new_target_bits) as u32;
pub(crate) const NATIVE_FRAME_RETURN_REGISTER_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, return_register) as u32;
pub(crate) const NATIVE_FRAME_TAIL_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, argument_count) as u32;

/// Compiled-code entry signature.
pub(crate) type JitEntry = extern "C" fn(*mut JitCtx) -> JitRet;
