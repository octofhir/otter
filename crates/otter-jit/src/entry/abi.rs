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
    ActiveFrameMut, ActiveFrameRef, RuntimeCall, RuntimeStubAllocContext, Value, VmError,
    VmRuntimeActivation,
    native_abi::{
        CodeEntryCell, FunctionEntryCell, NativeFrame, NativeFrameFlags, VmFrameHeader, VmThread,
    },
};
/// Machine-visible context shared by every compiled tier.
///
/// The context contains execution services, not duplicated JavaScript frame
/// state. Registers, SELF, `this`, upvalues, PC, and tier state live only in
/// [`NativeFrame`]; every compiled tier resolves them through that canonical
/// activation. Nested calls reuse this context and swap only its active-frame
/// pointer for the dynamic extent of the callee.
#[repr(C)]
pub(crate) struct JitCtx {
    /// Sole machine-visible VM state pointer.
    pub(crate) thread: *mut VmThread,
    /// Published authoritative activation.
    pub(crate) native_frame: *mut NativeFrame,
    /// Error slot shared by direct callees and runtime stubs when a re-entered
    /// operation throws. Pointer form keeps the slot stable while the shared
    /// context swaps its active native frame for a nested callee.
    pub(crate) error: *mut Option<VmError>,
    /// Base of the interpreter's native activation array (one pointer-sized
    /// `JitNativeActivation` per entry). Compiled call sequences publish and
    /// unpublish the complete canonical frame through it.
    pub(crate) activation_base: *mut u8,
    /// Address of the interpreter's native activation cursor.
    pub(crate) activation_top_ptr: *mut usize,
    /// Capacity of the activation array — the inline publish overflow bound.
    pub(crate) activation_limit: usize,
    /// Address of the active realm's GC-rooted `globalThis` compressed offset.
    pub(crate) global_this_offset: *const u32,
    /// Address of the isolate's synchronous JavaScript re-entry depth.
    pub(crate) sync_reentry_depth: *mut u32,
    /// Shared interpreter/generated-code recursion bound.
    pub(crate) sync_reentry_limit: u32,
    /// Address of native-stack bytes reserved by generated JavaScript calls.
    pub(crate) native_stack_bytes: *mut usize,
    /// Conservative aggregate generated-call native-stack bound.
    pub(crate) native_stack_bytes_limit: usize,
    /// Address of the logical depth owned by generated frames that remain
    /// native-only.
    pub(crate) generated_call_depth: *mut u32,
    /// Compiler-generated native calls entered during this outer activation.
    pub(crate) generated_calls: u64,
    /// Started generated callees resumed through cold deoptimization.
    pub(crate) generated_call_deopts: u64,
}

impl JitCtx {
    /// Bind the current machine-published frame to the VM-owned typed runtime
    /// boundary. This is the sole unsafe reconstruction point used by semantic
    /// stubs; [`RuntimeCall`] exposes no raw VM, stack, context, or frame handles.
    pub(crate) fn runtime_call(&mut self) -> Result<RuntimeCall<'_>, VmError> {
        let thread = unsafe { self.thread.as_ref() }.ok_or(VmError::InvalidOperand)?;
        let runtime_context = thread.runtime_context;
        if runtime_context == 0 {
            return Err(VmError::InvalidOperand);
        }
        // SAFETY: enter_compiled publishes this exact activation and native
        // frame for the dynamic extent of the shared JitCtx. `&mut self`
        // prevents a second RuntimeCall from being bound concurrently.
        let activation = std::ptr::NonNull::new(runtime_context as *mut VmRuntimeActivation)
            .ok_or(VmError::InvalidOperand)?;
        let frame = std::ptr::NonNull::new(self.native_frame).ok_or(VmError::InvalidOperand)?;
        // SAFETY: the shared entry ABI publishes both records and their
        // windows for the complete runtime-stub call.
        unsafe { RuntimeCall::bind(activation, frame) }
    }

    /// Try the typed boundary for pure-code fixture entries that deliberately
    /// publish no runtime context.
    pub(crate) fn try_runtime_call(&mut self) -> Result<Option<RuntimeCall<'_>>, VmError> {
        let thread = unsafe { self.thread.as_ref() }.ok_or(VmError::InvalidOperand)?;
        if thread.runtime_context == 0 {
            return Ok(None);
        }
        self.runtime_call().map(Some)
    }

    /// Representation-neutral shared view of the canonical activation.
    pub(crate) fn active_frame(&self) -> Result<ActiveFrameRef<'_>, VmError> {
        // SAFETY: the JIT entry contract publishes this frame and its windows
        // for the complete dynamic extent of `self`. A shared context borrow
        // cannot mutate either descriptor while the view is live.
        unsafe { ActiveFrameRef::from_native_ptr(self.native_frame) }
            .map_err(|_| VmError::InvalidOperand)
    }

    /// Representation-neutral mutable view of the canonical activation.
    pub(crate) fn active_frame_mut(&mut self) -> Result<ActiveFrameMut<'_>, VmError> {
        // SAFETY: the JIT entry contract publishes this frame for the complete
        // dynamic extent of `self`; `&mut self` provides exclusive logical
        // access to its header and window descriptors. ActiveFrame keeps those
        // windows raw so no slice borrow spans reentrant/allocating VM work.
        unsafe { ActiveFrameMut::from_native_ptr(self.native_frame) }
            .map_err(|_| VmError::InvalidOperand)
    }

    /// Stable register-window base derived from the canonical frame.
    pub(crate) fn register_base(&mut self) -> Result<*mut Value, VmError> {
        Ok(self.active_frame_mut()?.register_base_ptr())
    }

    /// Interpreter activation index for an entry that originated in the
    /// interpreter. Stack-register callees deliberately return an error so
    /// operations requiring interpreter-only state can side-exit pre-effect.
    pub(crate) fn materialized_frame_index(&self) -> Result<usize, VmError> {
        // SAFETY: every live JIT context publishes an aligned `NativeFrame`
        // for its complete dynamic extent. Direct-call linkage swaps this
        // pointer only after the callee record is fully initialized.
        let frame = unsafe { self.native_frame.as_ref() }.ok_or(VmError::InvalidOperand)?;
        let Some(index) = frame.materialized_frame_index() else {
            debug_assert!(
                frame
                    .header
                    .flags
                    .contains(NativeFrameFlags::STACK_REGISTERS)
            );
            return Err(VmError::InvalidOperand);
        };
        Ok(index as usize)
    }

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

pub(crate) const THREAD_OFFSET: u32 = std::mem::offset_of!(JitCtx, thread) as u32;
pub(crate) const NATIVE_FRAME_OFFSET: u32 = std::mem::offset_of!(JitCtx, native_frame) as u32;
/// Byte offset of the canonical instruction-index PC in the published native
/// frame. Generated code updates this together with its nested-call exit
/// payload before any opcode can observe or mutate JavaScript state.
pub(crate) const NATIVE_FRAME_PC_OFFSET: u32 = (std::mem::offset_of!(NativeFrame, header)
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
pub(crate) const VM_THREAD_CODE_OBJECT_ID_OFFSET: u32 =
    std::mem::offset_of!(VmThread, current_code_object_id) as u32;
pub(crate) const VM_THREAD_CURRENT_FRAME_OFFSET: u32 =
    std::mem::offset_of!(VmThread, current_frame) as u32;
/// Byte offsets of the native-activation publish fields in [`JitCtx`], used by
/// inline direct-call activation push/pop sequences.
pub(crate) const ACTIVATION_BASE_OFFSET: u32 = std::mem::offset_of!(JitCtx, activation_base) as u32;
pub(crate) const ACTIVATION_TOP_PTR_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, activation_top_ptr) as u32;
pub(crate) const ACTIVATION_LIMIT_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, activation_limit) as u32;
pub(crate) const GLOBAL_THIS_OFFSET_PTR_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, global_this_offset) as u32;
pub(crate) const SYNC_REENTRY_DEPTH_PTR_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, sync_reentry_depth) as u32;
pub(crate) const SYNC_REENTRY_LIMIT_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, sync_reentry_limit) as u32;
pub(crate) const NATIVE_STACK_BYTES_PTR_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, native_stack_bytes) as u32;
pub(crate) const NATIVE_STACK_BYTES_LIMIT_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, native_stack_bytes_limit) as u32;
pub(crate) const GENERATED_CALL_DEPTH_PTR_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, generated_call_depth) as u32;
pub(crate) const GENERATED_CALLS_OFFSET: u32 = std::mem::offset_of!(JitCtx, generated_calls) as u32;
pub(crate) const GENERATED_CALL_DEOPTS_OFFSET: u32 =
    std::mem::offset_of!(JitCtx, generated_call_deopts) as u32;
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
/// Fixed-layout stable function dispatch selected by generated call linkage.
pub(crate) const FUNCTION_ENTRY_GENERATION_CELL_OFFSET: u32 =
    std::mem::offset_of!(FunctionEntryCell, generation_cell) as u32;
/// Fixed-layout fields consumed by native call linkage. Generation cells are
/// boxed by the isolate registry and never reused.
pub(crate) const CODE_ENTRY_ACTIVE_COUNT_OFFSET: u32 =
    std::mem::offset_of!(CodeEntryCell, active_count) as u32;
pub(crate) const CODE_ENTRY_GENERATED_ENTRIES_OFFSET: u32 =
    std::mem::offset_of!(CodeEntryCell, generated_entries) as u32;
pub(crate) const CODE_ENTRY_GENERATED_RETURNS_OFFSET: u32 =
    std::mem::offset_of!(CodeEntryCell, generated_returns) as u32;
pub(crate) const CODE_ENTRY_GENERATED_DEOPTS_OFFSET: u32 =
    std::mem::offset_of!(CodeEntryCell, generated_deopts) as u32;
pub(crate) const CODE_ENTRY_GENERATED_THROWS_OFFSET: u32 =
    std::mem::offset_of!(CodeEntryCell, generated_throws) as u32;
pub(crate) const CODE_ENTRY_GENERATED_BAIL_STREAK_OFFSET: u32 =
    std::mem::offset_of!(CodeEntryCell, generated_bail_streak) as u32;
pub(crate) const CODE_ENTRY_CODE_OBJECT_ID_OFFSET: u32 =
    std::mem::offset_of!(CodeEntryCell, code_object_id) as u32;
pub(crate) const CODE_ENTRY_FUNCTION_ID_OFFSET: u32 =
    std::mem::offset_of!(CodeEntryCell, function_id) as u32;
pub(crate) const CODE_ENTRY_REGISTER_COUNT_OFFSET: u32 =
    std::mem::offset_of!(CodeEntryCell, register_count) as u32;
pub(crate) const CODE_ENTRY_FLAGS_OFFSET: u32 = std::mem::offset_of!(CodeEntryCell, flags) as u32;
pub(crate) const CODE_ENTRY_GENERATED_STACK_FRAME_BYTES_OFFSET: u32 =
    std::mem::offset_of!(CodeEntryCell, generated_stack_frame_bytes) as u32;
/// 16-aligned machine-stack reservation for a nested callee's compact frame.
pub(crate) const NATIVE_FRAME_STACK_SIZE: u32 =
    ((std::mem::size_of::<NativeFrame>() + 15) & !15) as u32;
/// Byte offsets of the callee-frame fields emitted nested-call sequences fill.
pub(crate) const NATIVE_FRAME_REGISTER_BASE_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, register_base) as u32;
pub(crate) const NATIVE_FRAME_FUNCTION_ID_OFFSET: u32 = (std::mem::offset_of!(NativeFrame, header)
    + std::mem::offset_of!(VmFrameHeader, function_id))
    as u32;
pub(crate) const NATIVE_FRAME_CODE_BLOCK_ID_OFFSET: u32 = (std::mem::offset_of!(
    NativeFrame,
    header
) + std::mem::offset_of!(
    VmFrameHeader,
    code_block_id
)) as u32;
pub(crate) const NATIVE_FRAME_REGISTER_COUNT_OFFSET: u32 =
    (std::mem::offset_of!(NativeFrame, header)
        + std::mem::offset_of!(VmFrameHeader, register_count)) as u32;
pub(crate) const NATIVE_FRAME_KIND_OFFSET: u32 =
    (std::mem::offset_of!(NativeFrame, header) + std::mem::offset_of!(VmFrameHeader, kind)) as u32;
pub(crate) const NATIVE_FRAME_FLAGS_OFFSET: u32 =
    (std::mem::offset_of!(NativeFrame, header) + std::mem::offset_of!(VmFrameHeader, flags)) as u32;
pub(crate) const NATIVE_FRAME_THIS_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, this_value_bits) as u32;
pub(crate) const NATIVE_FRAME_NEW_TARGET_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, new_target_bits) as u32;
pub(crate) const NATIVE_FRAME_SELF_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, self_value_bits) as u32;
pub(crate) const NATIVE_FRAME_UPVALUE_BASE_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, upvalue_base) as u32;
pub(crate) const NATIVE_FRAME_UPVALUE_COUNT_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, upvalue_count) as u32;
pub(crate) const NATIVE_FRAME_ACTIVATION_ID_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, activation_id) as u32;

// The native entry ABI targets 64-bit engines. These assertions describe the
// one current VM/JIT layout generated code consumes directly.
#[cfg(target_pointer_width = "64")]
const _: [(); 112] = [(); std::mem::size_of::<JitCtx>()];

/// Compiled-code entry signature.
pub(crate) type JitEntry = extern "C" fn(*mut JitCtx) -> JitRet;
