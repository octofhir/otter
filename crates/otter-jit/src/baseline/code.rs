//! Finalized baseline code objects and VM entry publication.
//!
//! # Contents
//! - Ownership of executable code and code-address-stable emission artifacts.
//! - Function and OSR entry lookup through the VM JIT interface.
//! - Native frame, safepoint registry, and JIT context publication.
//!
//! # Invariants
//! - Executable mappings and every embedded artifact pointer share one owner.
//! - Native frames publish exact register windows and sorted safepoint records.
//! - Entry pointers are called only through the frozen baseline ABI.
//!
//! # See also
//! - [`super::artifacts`] owns data addresses embedded by the backend.
//! - [`super::abi`] defines the entry and context layouts.

use super::{
    EmissionArtifacts, JitCtx, JitEntry, STATUS_BAILED, STATUS_RETURNED, WhiskerIcCell,
    jit_pop_native_activation_stub, jit_push_native_activation_stub,
};
use crate::CompiledCode;
use otter_vm::{
    HoltStack, Interpreter, JitExecOutcome, JitFunctionCode, SafepointRecord, Value, VmError,
    VmRuntimeActivation,
    native_abi::{
        BuildVersionRecord, CodeObjectMetadata, LayoutVersionRecord, NativeFrame, NativeFrameFlags,
        NativeFrameKind, VM_BUILD_VERSION, VmFrameHeader, VmThread,
    },
};

/// Finalized baseline machine code for one function.
pub struct BaselineCode {
    code: CompiledCode,
    /// Frozen VM-owned metadata validated before every entry selection.
    metadata: CodeObjectMetadata,
    /// Installed code-object identity used for safepoint lookup.
    code_object_id: u64,
    /// Source function id published in this code's native frames.
    function_id: u32,
    /// Tagged register-window width published in the native frame.
    register_count: u16,
    /// Loop-header logical PC → assembler offset of its OSR-entry trampoline.
    /// Each trampoline runs the standard prologue then branches to the header's
    /// body label, so the VM can enter mid-loop with the live frame registers.
    osr_entries: std::collections::BTreeMap<u32, usize>,
    /// `true` when at least one opcode outside the supported subset was emitted
    /// as a bail-to-interpreter (not a hard compile failure). Such code is only
    /// sound to enter at a supported loop header via OSR — entering at function
    /// entry would just bail immediately. The function-entry path skips it; only
    /// loop OSR uses it.
    osr_only: bool,
    /// Stable backing store for the WhiskerIC `LoadProperty` cells — one per
    /// `LoadProperty` op, self-patched by [`jit_load_prop_window_stub`]. Emitted code
    /// holds raw addresses into this slice, so it must never be moved out or
    /// cloned after `compile` returns (the code object is only ever shared by
    /// `Arc`, never cloned by value). Boxed so the buffer address is fixed.
    #[allow(dead_code)]
    load_ic_cells: Box<[WhiskerIcCell]>,
    /// Stable backing store for the WhiskerIC `StoreProperty` cells — one per
    /// `StoreProperty` op, self-patched by [`jit_store_prop_window_stub`]. Same
    /// ownership / stability contract as [`Self::load_ic_cells`].
    #[allow(dead_code)]
    store_ic_cells: Box<[WhiskerIcCell]>,
    /// Stable decoded register buffer shared by variadic operation sites.
    /// Emitted code passes pointers into this boxed slice to runtime stubs.
    #[allow(dead_code)]
    register_operands: Box<[u16]>,
    /// Stable decoded parent-upvalue index buffer for `MakeClosure` sites.
    #[allow(dead_code)]
    index_operands: Box<[u32]>,
    /// Stable backing store for code-object-owned allocating safepoints.
    safepoint_records: Box<[SafepointRecord]>,
    /// Every op in the body addresses registers through the window
    /// (`JitCtx.regs`), so the body is sound to enter frameless (see
    /// [`JitFunctionCode::frameless_entry_safe`]).
    frameless_entry_safe: bool,
}

impl BaselineCode {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn from_emission(
        code: CompiledCode,
        code_object_id: u64,
        function_id: u32,
        register_count: u16,
        osr_entries: std::collections::BTreeMap<u32, usize>,
        osr_only: bool,
        artifacts: EmissionArtifacts,
        safepoint_records: Box<[SafepointRecord]>,
        frameless_entry_safe: bool,
    ) -> Self {
        const AARCH64_BASELINE_ABI: u64 = 0x4136_3442_4c4e_0001;
        let metadata = CodeObjectMetadata {
            id: code_object_id,
            code_block_id: function_id,
            entry_offset: 0,
            code_size: code.len() as u32,
            safepoint_count: safepoint_records.len() as u32,
            frame_map_count: safepoint_records.len() as u32,
            spill_map_count: 0,
            dependency_count: 0,
            reserved: 0,
            layout: LayoutVersionRecord::CURRENT,
            build: BuildVersionRecord {
                vm_build: VM_BUILD_VERSION,
                target_abi: AARCH64_BASELINE_ABI,
            },
        };
        Self {
            code,
            metadata,
            code_object_id,
            function_id,
            register_count,
            osr_entries,
            osr_only,
            load_ic_cells: artifacts.load_ic_cells,
            store_ic_cells: artifacts.store_ic_cells,
            register_operands: artifacts.register_operands,
            index_operands: artifacts.index_operands,
            safepoint_records,
            frameless_entry_safe,
        }
    }

    #[cfg(test)]
    pub(super) unsafe fn entry_ptr_for_test(&self) -> *const u8 {
        // SAFETY: tests keep `self` alive for the complete native call.
        unsafe { self.code.entry_ptr() }
    }
}

impl std::fmt::Debug for BaselineCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BaselineCode")
            .field("code_len", &self.code.len())
            .finish()
    }
}

impl JitFunctionCode for BaselineCode {
    fn metadata(&self) -> CodeObjectMetadata {
        self.metadata
    }

    fn code_len(&self) -> usize {
        self.code.len()
    }

    fn osr_only(&self) -> bool {
        self.osr_only
    }

    fn frameless_entry_safe(&self) -> bool {
        self.frameless_entry_safe
    }

    fn entry_addr(&self) -> Option<usize> {
        // SAFETY: the mapping is live for `self`; callers must keep the owning
        // code object installed while using this address.
        Some(unsafe { self.code.entry_ptr() as usize })
    }

    fn safepoint_count(&self) -> u32 {
        self.safepoint_records.len() as u32
    }

    fn safepoint_record(&self, safepoint_id: u32) -> Option<&SafepointRecord> {
        self.safepoint_records
            .binary_search_by_key(&safepoint_id, |record| record.id)
            .ok()
            .map(|index| &self.safepoint_records[index])
    }

    fn run_entry(&self, activation: VmRuntimeActivation) -> JitExecOutcome {
        assert!(
            self.metadata.is_compatible_with_current_vm(),
            "incompatible native code reached entry"
        );
        // SAFETY: the mapping is live and the main entry was emitted with the
        // `JitEntry` ABI.
        let entry = unsafe { self.code.entry_ptr() };
        // SAFETY: `entry` points into the live mapping; `activation` upholds the
        // reentry contract (valid, non-aliased for the call).
        unsafe {
            enter_compiled(
                activation,
                entry,
                self.code_object_id,
                self.function_id,
                self.register_count,
                !self.safepoint_records.is_empty(),
            )
        }
    }

    fn osr_entry(
        &self,
        activation: VmRuntimeActivation,
        logical_pc: u32,
    ) -> Option<JitExecOutcome> {
        if !self.metadata.is_compatible_with_current_vm() {
            return None;
        }
        let offset = *self.osr_entries.get(&logical_pc)?;
        // SAFETY: `offset` is an assembler offset recorded for this buffer and
        // points at a prologue trampoline emitted with the `JitEntry` ABI.
        let entry = unsafe { self.code.ptr_at(offset) };
        // SAFETY: same reentry contract as `run_entry`.
        Some(unsafe {
            enter_compiled(
                activation,
                entry,
                self.code_object_id,
                self.function_id,
                self.register_count,
                !self.safepoint_records.is_empty(),
            )
        })
    }
}

/// Build the `JitCtx` for `activation` and invoke compiled code at `entry`, mapping
/// the returned status to a [`JitExecOutcome`].
///
/// Shared across compiled tiers and entry kinds: the baseline function-entry
/// and loop-header OSR paths, and the optimizing tier — every compiled entry
/// uses the identical [`JitEntry`] ABI (`extern "C" fn(*mut JitCtx) -> JitRet`)
/// and the same `JitCtx` construction, differing only in which instruction the
/// prologue branches to. Lives free (it uses no compiled-code state) so any
/// [`JitFunctionCode`] implementation can reuse it.
///
/// # Safety
/// `entry` must point at a prologue emitted with the [`JitEntry`] ABI inside a
/// live executable mapping that outlives the call, and `activation` must uphold the
/// [`VmRuntimeActivation`](otter_vm::VmRuntimeActivation) contract.
pub(crate) unsafe fn enter_compiled(
    activation: VmRuntimeActivation,
    entry: *const u8,
    code_object_id: u64,
    function_id: u32,
    register_count: u16,
    has_safepoints: bool,
) -> JitExecOutcome {
    {
        let stack = activation.stack_ptr().cast::<HoltStack>();
        let vm = activation.vm_ptr().cast::<Interpreter>();
        // SAFETY: `activation.stack_ptr()` is a valid `*mut HoltStack` for this call.
        let regs =
            Interpreter::jit_frame_regs_ptr(unsafe { &mut *stack }, activation.frame_index());
        // SAFETY: `activation.vm_ptr()`/`activation.stack_ptr()` are valid for this call and not aliased
        // by a live `&mut` (the VM froze its borrows); read the self closure up
        // front so a `MakeFunction`-of-self needs no Rust round-trip.
        let self_closure =
            unsafe { (*vm).jit_frame_self_closure_bits(&*stack, activation.frame_index()) };
        // SAFETY: same validity/aliasing contract as `self_closure` above.
        let this_value = unsafe { (*vm).jit_frame_this_bits(&*stack, activation.frame_index()) };
        // SAFETY: same validity/aliasing contract; the spine `Box` outlives this
        // entry (frame-owned), and the cells it holds are old-space (immobile).
        let upvalues_ptr =
            Interpreter::jit_frame_upvalues_ptr(unsafe { &*stack }, activation.frame_index());
        // SAFETY: `vm` is a valid `*mut Interpreter` for this entry and not
        // aliased by a live `&mut` (the VM froze its borrows); these return the
        // stable base / `reg_top` address of the flat JIT register stack.
        let reg_stack_base = unsafe { (*vm).jit_reg_stack_base() };
        let reg_top_ptr = unsafe { (*vm).jit_reg_top_ptr() };
        let sync_reentry_depth_ptr = unsafe { (*vm).jit_sync_reentry_depth_ptr() };
        let sync_reentry_limit = unsafe { (*vm).jit_sync_reentry_limit() };
        let gc_heap = unsafe { (*vm).jit_gc_heap_ptr() };
        let interrupt_flag = unsafe { (*vm).jit_interrupt_flag_ptr() };
        let backedge_fuel = unsafe { (*vm).jit_backedge_fuel_ptr() };
        let flags = if has_safepoints {
            NativeFrameFlags::from_bits(NativeFrameFlags::HAS_SAFEPOINTS)
        } else {
            NativeFrameFlags::empty()
        };
        let mut native_frame = NativeFrame {
            header: VmFrameHeader {
                function_id,
                code_block_id: function_id,
                pc: 0,
                register_count,
                kind: NativeFrameKind::Baseline,
                flags,
            },
            previous_frame: 0,
            register_base: regs as u64,
            argument_base: 0,
            feedback_base: 0,
            code_object_id,
            this_value_bits: this_value,
            new_target_bits: Value::undefined().to_bits(),
            return_register: u32::MAX,
            cold_state_index: u32::MAX,
            argument_count: 0,
            reserved0: 0,
            feedback_id: 0,
        };
        let mut thread = VmThread::empty();
        thread.current_frame = std::ptr::addr_of_mut!(native_frame) as u64;
        thread.runtime_context = std::ptr::addr_of!(activation) as u64;
        // SAFETY: `vm` is a valid `*mut Interpreter` for this entry; the header
        // and entry columns are isolate-owned and outlive every activation.
        thread.runtime_stub_table = unsafe { (*vm).jit_runtime_stub_table_addr() };
        // SAFETY: the boxed registry cell is isolate-owned and address-stable;
        // its view resolves safepoints for any installed code object, so a
        // nested compiled callee's allocating stubs root through real maps.
        thread.code_registry = unsafe { (*vm).jit_code_registry_view_addr() };
        thread.interrupt_cell = interrupt_flag as u64;
        thread.gc_heap = gc_heap as u64;
        thread.backedge_fuel_cell = backedge_fuel as u64;
        thread.sync_reentry_depth_cell = sync_reentry_depth_ptr as u64;
        thread.sync_reentry_limit = sync_reentry_limit;
        let mut error = None;
        let mut ctx = JitCtx {
            regs,
            self_closure,
            this_value,
            thread: std::ptr::addr_of_mut!(thread),
            native_frame: std::ptr::addr_of_mut!(native_frame),
            frame_index: activation.frame_index(),
            upvalues_ptr,
            error: &mut error,
            direct_entry_addr: 0,
            direct_regs: std::ptr::null_mut(),
            direct_self_closure: 0,
            direct_this_value: 0,
            direct_frame_index: 0,
            direct_upvalues_ptr: 0,
            direct_frame_ids: 0,
            direct_frame_meta: 0,
            direct_code_object_id: 0,
            reg_stack_base,
            reg_top_ptr,
        };
        // SAFETY: the mapping is live and `entry` was emitted with the
        // `JitEntry` ABI.
        let entry: JitEntry = unsafe { std::mem::transmute(entry) };
        let activation_status = jit_push_native_activation_stub(&mut ctx);
        if activation_status != 0 {
            return JitExecOutcome::Threw(error.take().unwrap_or(VmError::InvalidOperand));
        }
        let ret = entry(&mut ctx);
        let _ = jit_pop_native_activation_stub(&mut ctx);
        match ret.status {
            STATUS_RETURNED => JitExecOutcome::Returned(Value::from_bits(ret.value)),
            STATUS_BAILED => JitExecOutcome::Bailed(native_frame.header.pc),
            _ => JitExecOutcome::Threw(error.take().unwrap_or(VmError::InvalidOperand)),
        }
    }
}
