//! Finalized template code objects and VM entry publication.
//!
//! # Contents
//! - [`TemplateCode`] — ownership of one finalized template compilation.
//! - The [`JitFunctionCode`] implementation entering through the shared
//!   compiled-entry ABI.
//!
//! # Invariants
//! - The executable mapping and the entry pointer share one owner; entries run
//!   only through the frozen `JitCtx`/`NativeResultPair` contract.
//! - Allocating runtime calls name a concrete code-object-owned safepoint;
//!   the sorted record table resolves ids for the moving collector.
//! - Installed code uses the single in-process VM layout.
//!
//! # See also
//! - [`crate::entry`] — owner of the shared entry context and epilogue
//!   status contract.

use crate::CompiledCode;
use otter_vm::JitFunctionCode;
use otter_vm::native_abi::{CodeDependency, CodeObjectMetadata, SafepointRecord};

/// Finalized template machine code for one function.
pub struct TemplateCode {
    code: CompiledCode,
    /// Frozen VM-owned metadata validated before every entry selection.
    metadata: CodeObjectMetadata,
    /// Exact installed callee generations entered by emitted direct edges.
    dependencies: Box<[CodeDependency]>,
    /// Stable decoded register buffer shared by variadic operation sites.
    /// Emitted code passes pointers into this boxed slice to runtime
    /// transitions, so the allocation must live exactly as long as the code.
    #[allow(dead_code)]
    register_operands: Box<[u16]>,
    /// Stable backing store for the self-patching `LoadProperty` IC cells;
    /// emitted code holds raw addresses into this slice.
    #[allow(dead_code)]
    load_ic_cells: Box<[crate::entry::PropertySourceCell]>,
    /// Stable backing store for the self-patching `StoreProperty` IC cells.
    #[allow(dead_code)]
    store_ic_cells: Box<[crate::entry::PropertySourceCell]>,
    /// Code-object-owned allocating safepoints, sorted by id.
    safepoint_records: Box<[SafepointRecord]>,
    /// Loop-header logical PC → assembler offset of its OSR-entry trampoline.
    osr_entries: std::collections::BTreeMap<u32, usize>,
    /// Offset of the JavaScript call ABI entry, which builds this function's
    /// own frame; absent for a body entered only over an interpreter frame.
    call_entry: Option<usize>,
    /// `true` when unsupported opcodes were lowered to exact side exits;
    /// entry selection skips such code and only loop OSR uses it.
    osr_only: bool,
}

impl TemplateCode {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn from_emission(
        code: CompiledCode,
        code_object_id: u64,
        function_id: u32,
        dependencies: Box<[CodeDependency]>,
        register_operands: Box<[u16]>,
        load_ic_cells: Box<[crate::entry::PropertySourceCell]>,
        store_ic_cells: Box<[crate::entry::PropertySourceCell]>,
        safepoint_records: Box<[SafepointRecord]>,
        osr_entries: std::collections::BTreeMap<u32, usize>,
        call_entry: Option<usize>,
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
            dependency_count: dependencies.len() as u32,
        };
        Self {
            code,
            metadata,
            dependencies,
            register_operands,
            load_ic_cells,
            store_ic_cells,
            safepoint_records,
            osr_entries,
            call_entry,
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

    fn retained_bytes(&self) -> u64 {
        retained_bytes_sum(
            self.code.len(),
            &[
                std::mem::size_of_val::<[u16]>(&self.register_operands),
                std::mem::size_of_val::<[crate::entry::PropertySourceCell]>(&self.load_ic_cells),
                std::mem::size_of_val::<[crate::entry::PropertySourceCell]>(&self.store_ic_cells),
                std::mem::size_of_val::<[SafepointRecord]>(&self.safepoint_records),
                std::mem::size_of_val::<[CodeDependency]>(&self.dependencies),
                self.osr_entries.len() * std::mem::size_of::<(u32, usize)>(),
            ],
        )
    }

    fn call_entry_addr(&self) -> Option<usize> {
        // SAFETY: the assembler recorded this entry in the live mapping.
        self.call_entry
            .map(|offset| unsafe { self.code.ptr_at(offset) as usize })
    }

    fn dependencies(&self) -> &[CodeDependency] {
        &self.dependencies
    }

    fn osr_only(&self) -> bool {
        self.osr_only
    }

    fn entry_addr(&self) -> Option<usize> {
        // SAFETY: the mapping is live for `self`; callers must keep the owning
        // code object installed while using this address. Generated call edges
        // acquire an exact entry-cell lease before branching here.
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

    fn osr_entry_addr(&self, logical_pc: u32) -> Option<usize> {
        let offset = *self.osr_entries.get(&logical_pc)?;
        // SAFETY: the assembler recorded this OSR prologue in the live mapping.
        Some(unsafe { self.code.ptr_at(offset) as usize })
    }
}

/// Saturating sum of the executable mapping and its owned side tables.
/// Saturation is acceptable here: a saturated total still exceeds any real
/// budget and fails admission closed.
pub(crate) fn retained_bytes_sum(code_len: usize, parts: &[usize]) -> u64 {
    let mut total = code_len as u64;
    for part in parts {
        total = total.saturating_add(*part as u64);
    }
    total
}
