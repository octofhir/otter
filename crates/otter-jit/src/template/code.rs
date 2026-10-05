//! Finalized template code objects and VM entry publication.
//!
//! # Contents
//! - [`TemplateCode`] — ownership of one finalized template compilation.
//! - The [`JitFunctionCode`] implementation entering through the shared
//!   compiled-entry ABI.
//! - [`retained_source_work`] — the source-work cells a mapping keeps alive.
//!
//! # Invariants
//! - The executable mapping and the entry pointer share one owner; entries run
//!   only through the frozen `JitCtx`/`NativeResultPair` contract.
//! - Allocating runtime calls name a concrete code-object-owned safepoint;
//!   the sorted record table resolves ids for the moving collector.
//! - Exact physical return offsets and source records have one retained owner;
//!   resource charging includes both tables independently of artifact capture.
//! - Installed code uses the single in-process VM layout.
//! - Resource admission charges reserved mapping bytes and actual owned table
//!   payloads, including nested safepoints. Logical code length remains separate.
//! - Spliced source-function metadata describes accepted emitted bodies,
//!   independently of optional artifacts, exits, and safepoints.
//! - The code object owns its function's source-work cell, whose address the
//!   entry prologue bakes, for as long as the mapping lives.
//!
//! # See also
//! - [`crate::entry`] — owner of the shared entry context and epilogue
//!   status contract.

use crate::CompiledCode;
use otter_vm::JitFunctionCode;
use otter_vm::native_abi::{CodeDependency, CodeObjectMetadata, SafepointEntry, SafepointRecord};

/// Finalized template machine code for one function.
pub struct TemplateCode {
    code: CompiledCode,
    /// Frozen VM-owned metadata validated before every entry selection.
    metadata: CodeObjectMetadata,
    /// Exact installed callee generations entered by emitted direct edges.
    dependencies: Box<[CodeDependency]>,
    spliced_functions: Box<[u32]>,
    source_work: Box<[std::sync::Arc<otter_vm::native_abi::SourceWork>]>,
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
    /// Exact CALL/BLR return offsets, sorted and retained with their generation.
    return_sites: Box<[SafepointEntry]>,
    /// Loop-header logical PC → assembler offset of its OSR-entry trampoline.
    osr_entries: Box<[(u32, usize)]>,
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
        spliced_functions: Box<[u32]>,
        source_work: Box<[std::sync::Arc<otter_vm::native_abi::SourceWork>]>,
        register_operands: Box<[u16]>,
        load_ic_cells: Box<[crate::entry::PropertySourceCell]>,
        store_ic_cells: Box<[crate::entry::PropertySourceCell]>,
        safepoint_records: Box<[SafepointRecord]>,
        return_sites: Box<[SafepointEntry]>,
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
            spliced_functions,
            source_work,
            register_operands,
            load_ic_cells,
            store_ic_cells,
            safepoint_records,
            return_sites,
            osr_entries: osr_entries
                .into_iter()
                .collect::<Vec<_>>()
                .into_boxed_slice(),
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
    pub(super) fn osr_entries_for_test(&self) -> &[(u32, usize)] {
        &self.osr_entries
    }
}

/// The canonical source-work cells a finalized Template mapping retains: its
/// own function's, whose address the entry prologue charges, and those of the
/// accepted leaf splices. Candidate snapshots that did not splice contribute
/// no ownership.
pub(crate) fn retained_source_work(
    view: &otter_vm::JitCompileSnapshot,
    spliced: &std::collections::BTreeSet<u32>,
) -> Box<[std::sync::Arc<otter_vm::native_abi::SourceWork>]> {
    let mut cells = std::collections::BTreeMap::new();
    cells.insert(view.code_block.id, view.code_block.source_work().clone());
    for body in view
        .inline_callees
        .values()
        .map(|callee| &callee.body)
        .chain(view.inline_methods.values().map(|method| &method.body))
        .chain(
            view.inline_poly_methods
                .values()
                .flatten()
                .map(|method| &method.body),
        )
    {
        if spliced.contains(&body.code_block.id) {
            cells.insert(body.code_block.id, body.code_block.source_work().clone());
        }
    }
    assert!(
        spliced.iter().all(|fid| cells.contains_key(fid)),
        "accepted leaf must retain its canonical source cell"
    );
    cells.into_values().collect::<Vec<_>>().into_boxed_slice()
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
        let nested_safepoints = self.safepoint_records.iter().fold(0u64, |bytes, record| {
            bytes.saturating_add(record.retained_bytes())
        });
        retained_bytes_sum(
            self.code.retained_mapping_bytes(),
            &[
                std::mem::size_of::<Self>(),
                std::mem::size_of_val::<[u16]>(&self.register_operands),
                std::mem::size_of_val::<[crate::entry::PropertySourceCell]>(&self.load_ic_cells),
                std::mem::size_of_val::<[crate::entry::PropertySourceCell]>(&self.store_ic_cells),
                std::mem::size_of_val::<[SafepointRecord]>(&self.safepoint_records),
                std::mem::size_of_val::<[SafepointEntry]>(&self.return_sites),
                std::mem::size_of_val::<[CodeDependency]>(&self.dependencies),
                std::mem::size_of_val::<[u32]>(&self.spliced_functions),
                std::mem::size_of_val(self.source_work.as_ref()),
                self.osr_entries.len() * std::mem::size_of::<(u32, usize)>(),
            ],
        )
        .saturating_add(nested_safepoints)
    }

    fn call_entry_addr(&self) -> Option<usize> {
        // SAFETY: the assembler recorded this entry in the live mapping.
        self.call_entry
            .map(|offset| unsafe { self.code.ptr_at(offset) as usize })
    }

    fn dependencies(&self) -> &[CodeDependency] {
        &self.dependencies
    }

    fn spliced_functions(&self) -> &[u32] {
        &self.spliced_functions
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

    fn native_code_address(&self) -> Option<u64> {
        // SAFETY: offset zero belongs to this still-owned executable mapping.
        Some(unsafe { self.code.ptr_at(0) as u64 })
    }

    fn return_sites(&self) -> &[SafepointEntry] {
        &self.return_sites
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
        let index = self
            .osr_entries
            .binary_search_by_key(&logical_pc, |entry| entry.0)
            .ok()?;
        let offset = self.osr_entries[index].1;
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

#[cfg(test)]
mod tests {
    use super::*;
    use dynasmrt::DynasmApi;
    use otter_vm::native_abi::NO_FRAME_STATE;

    #[test]
    fn retained_bytes_include_template_nested_safepoint_capacity_and_boxed_osr_entries() {
        // Inspect a real allocated mapping; these bytes are never executed.
        let mut ops = dynasmrt::x64::Assembler::new_with_capacity(32_768).unwrap();
        let entry = ops.offset();
        ops.push(0xc3);
        let mapping = CompiledCode::new(ops.finalize().unwrap(), entry);
        let reserved = mapping.retained_mapping_bytes();
        let mut code = TemplateCode::from_emission(
            mapping,
            1,
            7,
            Box::new([]),
            Box::new([]),
            Box::new([]),
            Box::new([1, 2]),
            Box::new([]),
            Box::new([]),
            Box::new([SafepointRecord::window(1, NO_FRAME_STATE)]),
            Box::new([]),
            [(2, 0), (7, 0)].into_iter().collect(),
            None,
            false,
        );
        assert_eq!(code.code_len(), 1);
        assert!(reserved > code.code_len());
        assert_eq!(code.osr_entries.as_ref(), [(2, 0), (7, 0)]);
        assert!(code.osr_entry_addr(2).is_some());
        assert!(code.osr_entry_addr(3).is_none());
        // Metadata-only retention proof over the real mapping. Native CALL
        // production and runtime lookup are tested separately by emitted edges.
        let before_sites = code.retained_bytes();
        code.return_sites = vec![SafepointEntry {
            native_return_offset: 1,
            safepoint_id: 1,
        }]
        .into_boxed_slice();
        assert_eq!(code.return_sites().len(), 1);
        assert_eq!(
            code.retained_bytes() - before_sites,
            std::mem::size_of::<SafepointEntry>() as u64
        );
        assert_eq!(
            code.native_code_address(),
            Some(unsafe { code.code.ptr_at(0) as u64 })
        );
        let before = code.retained_bytes();
        code.safepoint_records[0].spill_roots = otter_vm::native_abi::SpillRoots::from_slots([255]);
        assert_eq!(
            code.retained_bytes() - before,
            4 * std::mem::size_of::<u64>() as u64,
            "the spill bitmap is owned by the code object"
        );
        assert!(
            code.retained_bytes() >= reserved as u64 + std::mem::size_of::<TemplateCode>() as u64
        );
    }
}
