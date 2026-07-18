//! Finalized template code objects and VM entry publication.
//!
//! # Contents
//! - [`TemplateCode`] — ownership of one finalized template compilation.
//! - The [`JitFunctionCode`] implementation entering through the shared
//!   compiled-entry ABI.
//!
//! # Invariants
//! - The executable mapping and the entry pointer share one owner; entries run
//!   only through the frozen `JitCtx`/`JitRet` contract.
//! - Allocating runtime calls name a concrete code-object-owned safepoint;
//!   the sorted record table resolves ids for the moving collector.
//! - Installed code uses the single in-process VM layout.
//!
//! # See also
//! - [`crate::entry`] — owner of the shared entry context and epilogue
//!   status contract.

use crate::CompiledCode;
#[cfg(target_arch = "aarch64")]
use crate::arm64::CallTrampoline;
use crate::entry::enter_compiled;
use otter_vm::native_abi::CodeObjectMetadata;
use otter_vm::{JitExecOutcome, JitFunctionCode, SafepointRecord, VmRuntimeActivation};

/// Finalized template machine code for one function.
pub struct TemplateCode {
    code: CompiledCode,
    /// Shared executable call lifecycle whose entry address is baked into this
    /// mapping. Installed code can outlive the compiler hook, so ownership is
    /// retained here as well as by [`crate::OtterJitCompiler`].
    #[cfg(target_arch = "aarch64")]
    _call_trampoline: std::sync::Arc<CallTrampoline>,
    /// Frozen VM-owned metadata validated before every entry selection.
    metadata: CodeObjectMetadata,
    /// Installed code-object identity published in native frames.
    code_object_id: u64,
    /// Source function id published in this code's native frames.
    function_id: u32,
    /// Tagged register-window width published in the native frame.
    register_count: u16,
    /// Stable decoded register buffer shared by variadic operation sites.
    /// Emitted code passes pointers into this boxed slice to runtime
    /// transitions, so the allocation must live exactly as long as the code.
    #[allow(dead_code)]
    register_operands: Box<[u16]>,
    /// Stable decoded parent-upvalue index buffer for closure sites; same
    /// ownership contract as [`Self::register_operands`].
    #[allow(dead_code)]
    index_operands: Box<[u32]>,
    /// Stable backing store for the self-patching `LoadProperty` IC cells;
    /// emitted code holds raw addresses into this slice.
    #[allow(dead_code)]
    load_ic_cells: Box<[crate::entry::WhiskerIcCell]>,
    /// Stable backing store for the self-patching `StoreProperty` IC cells.
    #[allow(dead_code)]
    store_ic_cells: Box<[crate::entry::WhiskerIcCell]>,
    /// Code-object-owned allocating safepoints, sorted by id.
    safepoint_records: Box<[SafepointRecord]>,
    /// Loop-header logical PC → assembler offset of its OSR-entry trampoline.
    osr_entries: std::collections::BTreeMap<u32, usize>,
    /// `true` when unsupported opcodes were lowered to exact side exits;
    /// entry selection skips such code and only loop OSR uses it.
    osr_only: bool,
}

impl TemplateCode {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn from_emission(
        code: CompiledCode,
        #[cfg(target_arch = "aarch64")] call_trampoline: std::sync::Arc<CallTrampoline>,
        code_object_id: u64,
        function_id: u32,
        register_count: u16,
        register_operands: Box<[u16]>,
        index_operands: Box<[u32]>,
        load_ic_cells: Box<[crate::entry::WhiskerIcCell]>,
        store_ic_cells: Box<[crate::entry::WhiskerIcCell]>,
        safepoint_records: Box<[SafepointRecord]>,
        osr_entries: std::collections::BTreeMap<u32, usize>,
        osr_only: bool,
    ) -> Self {
        let metadata = CodeObjectMetadata {
            id: code_object_id,
            code_block_id: function_id,
            entry_offset: code.entry_offset() as u32,
            code_size: code.len() as u32,
            safepoint_count: safepoint_records.len() as u32,
            frame_map_count: safepoint_records.len() as u32,
            spill_map_count: 0,
            dependency_count: 0,
        };
        Self {
            code,
            #[cfg(target_arch = "aarch64")]
            _call_trampoline: call_trampoline,
            metadata,
            code_object_id,
            function_id,
            register_count,
            register_operands,
            index_operands,
            load_ic_cells,
            store_ic_cells,
            safepoint_records,
            osr_entries,
            osr_only,
        }
    }

    #[cfg(test)]
    pub(super) unsafe fn entry_ptr_for_test(&self) -> *const u8 {
        // SAFETY: tests keep `self` alive for the complete native call.
        unsafe { self.code.entry_ptr() }
    }

    #[cfg(test)]
    pub(super) fn exact_bytes_for_test(&self) -> &[u8] {
        self.code.bytes()
    }

    #[cfg(test)]
    pub(super) fn osr_entries_for_test(&self) -> &std::collections::BTreeMap<u32, usize> {
        &self.osr_entries
    }
}

impl std::fmt::Debug for TemplateCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TemplateCode")
            .field("code_len", &self.code.len())
            .field("osr_only", &self.osr_only)
            .finish()
    }
}

impl JitFunctionCode for TemplateCode {
    fn metadata(&self) -> CodeObjectMetadata {
        self.metadata
    }

    fn code_len(&self) -> usize {
        self.code.len()
    }

    fn osr_only(&self) -> bool {
        self.osr_only
    }

    fn entry_addr(&self) -> Option<usize> {
        // SAFETY: the mapping is live for `self`; callers must keep the owning
        // code object installed while using this address (the direct-call
        // prepare path anchors it through the code registry).
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

    fn osr_entry(
        &self,
        activation: VmRuntimeActivation,
        logical_pc: u32,
    ) -> Option<JitExecOutcome> {
        let offset = *self.osr_entries.get(&logical_pc)?;
        // SAFETY: `offset` is an assembler offset recorded for this buffer and
        // points at a prologue trampoline emitted with the shared entry ABI.
        let entry = unsafe { self.code.ptr_at(offset) };
        // SAFETY: same reentry contract as `run_entry`.
        Some(unsafe {
            enter_compiled(
                activation,
                entry,
                self.code_object_id,
                self.function_id,
                self.register_count,
                otter_vm::native_abi::NativeFrameKind::Baseline,
                !self.safepoint_records.is_empty(),
            )
        })
    }

    fn run_entry(&self, activation: VmRuntimeActivation) -> JitExecOutcome {
        // SAFETY: the mapping is live and the main entry was emitted with the
        // shared compiled-entry ABI.
        let entry = unsafe { self.code.entry_ptr() };
        // SAFETY: `entry` points into the live mapping; `activation` upholds
        // the reentry contract (valid, non-aliased for the call).
        unsafe {
            enter_compiled(
                activation,
                entry,
                self.code_object_id,
                self.function_id,
                self.register_count,
                otter_vm::native_abi::NativeFrameKind::Baseline,
                !self.safepoint_records.is_empty(),
            )
        }
    }
}
