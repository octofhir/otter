//! Finalized code objects and the entry point of the optimizing tier.
//!
//! The graph tier compiles every function it is asked for: an instruction
//! without a specialized form runs its baseline operation, and one that cannot
//! run in optimized code leaves through a deopt. Only a target without the
//! graph tier's code generator returns [`Unsupported`].
//!
//! # Contents
//! - [`compile_optimized`] — the optimizing compilation entry point.
//! - [`OptimizedCode`] — executable code plus deopt and safepoint metadata.
//! - [`OptimizedMetadata`] — deterministic identity and allocation figures.
//!
//! # Invariants
//! - Both entry forms initialize and publish the complete tagged home region;
//!   every safepoint scans that region and excludes unboxed homes. Collecting
//!   slow paths preserve exact-live registers through their canonical homes,
//!   which moving collection rewrites in place.
//! - Deopt recipes read canonical homes and constants. Memory-spilled values
//!   populate homes at definitions; register-only values populate them in
//!   collecting slow paths or cold guard exits before reconstruction.
//! - The VM-owned two-word `NativeResultPair` uses `x0`/`x1` on AArch64
//!   and `rax`/`rdx` on x86-64 for payload/status after native C adaptation.
//! - Deopt reconstruction reads the [`DeoptTable`] published with the code
//!   object; every live interpreter value of every frame, inlined frames
//!   included, and the exact logical resume PCs are committed before
//!   returning to the VM.
//! - Every backend publishes bytes through the same [`CompiledCode`], code
//!   registry, native-frame kind, artifact bundle, and W^X lifecycle.
//! - Actual spliced source functions are retained independently of deopt and
//!   safepoint presence, so source retraining can retire every inlined caller.
//! - Resource admission accounts requested executable backing and every owned
//!   side table, including nested recipe storage and reserved vector capacity;
//!   logical code lengths remain the artifact and native metadata boundary.
//!
//! # See also
//! - `crate::graph` — the optimizing compiler.
//! - [`crate::template`] — the runtime-wired baseline compiler.

use std::collections::BTreeSet;

use otter_vm::{
    JitCompileSnapshot, JitFunctionCode,
    deopt::DeoptTable,
    native_abi::{CodeDependency, CodeObjectMetadata, SafepointRecord},
};

use crate::{CompiledCode, Unsupported};

/// Deterministic metadata for one optimized compilation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptimizedMetadata {
    /// Isolate-assigned identity supplied to [`compile_optimized`].
    pub code_object_id: u64,
    /// Source bytecode function identity.
    pub function_id: u32,
    /// Number of tagged parameters read from the entry register window.
    pub param_count: u16,
    /// Number of writable interpreter registers reconstructed on bail.
    pub register_count: u16,
    /// Spill slots the register allocator selected.
    pub allocator_spill_slot_count: u32,
    /// Number of eight-byte canonical tagged and untagged homes selected by
    /// the allocator.
    pub spill_slot_count: u32,
}

/// Finalized optimizing code and its exact-PC deoptimization metadata.
pub struct OptimizedCode {
    /// The body; its entry continues a published interpreter frame.
    code: CompiledCode,
    /// Offset of the JavaScript call ABI entry, which builds this
    /// function's own frame.
    call_entry: usize,
    /// Deopt metadata whose address is baked into the shared exit handler; the
    /// allocation must live exactly as long as the code.
    deopt: Box<otter_vm::deopt::DeoptRuntime>,
    safepoint_records: Box<[SafepointRecord]>,
    return_sites: Box<[otter_vm::native_abi::SafepointEntry]>,
    /// Loop-header logical PCs the entry dispatch can enter through an OSR
    /// block: the compile's OSR target, or nothing for an entry compile.
    osr_headers: Box<[u32]>,
    /// Exact installed callee generations entered by emitted direct edges.
    dependencies: Box<[CodeDependency]>,
    spliced_functions: Box<[u32]>,
    /// Operand registers of generic baseline operations whose addresses the
    /// code bakes; same ownership contract.
    _register_operands: Box<[u16]>,
    metadata: OptimizedMetadata,
    code_metadata: CodeObjectMetadata,
}

impl OptimizedCode {
    #[allow(clippy::too_many_arguments)]
    #[cfg_attr(
        not(any(target_arch = "aarch64", target_arch = "x86_64")),
        allow(dead_code)
    )]
    pub(crate) fn new(
        code: CompiledCode,
        call_entry: usize,
        deopt: Box<otter_vm::deopt::DeoptRuntime>,
        safepoint_records: Box<[SafepointRecord]>,
        return_sites: Box<[otter_vm::native_abi::SafepointEntry]>,
        osr_headers: BTreeSet<u32>,
        dependencies: Box<[CodeDependency]>,
        spliced_functions: Box<[u32]>,
        register_operands: Box<[u16]>,
        metadata: OptimizedMetadata,
    ) -> Self {
        let code_metadata = CodeObjectMetadata {
            id: metadata.code_object_id,
            code_block_id: metadata.function_id,
            entry_offset: code.entry_offset() as u32,
            code_size: code.len() as u32,
            safepoint_count: safepoint_records.len() as u32,
            frame_map_count: 0,
            spill_map_count: 0,
            dependency_count: dependencies.len() as u32,
        };
        Self {
            code,
            call_entry,
            deopt,
            safepoint_records,
            return_sites,
            osr_headers: osr_headers.into_iter().collect(),
            dependencies,
            spliced_functions,
            _register_operands: register_operands,
            metadata,
            code_metadata,
        }
    }

    /// Borrow the finalized executable mapping.
    #[must_use]
    pub fn compiled_code(&self) -> &CompiledCode {
        &self.code
    }

    /// Borrow the verified exact-byte-PC deoptimization table.
    #[must_use]
    pub fn deopt_table(&self) -> &DeoptTable {
        &self.deopt.table
    }

    /// Return deterministic allocation and source identity metadata.
    #[must_use]
    pub const fn metadata(&self) -> OptimizedMetadata {
        self.metadata
    }
}

impl std::fmt::Debug for OptimizedCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OptimizedCode")
            .field("code_len", &self.code.len())
            .field("call_entry", &self.call_entry)
            .field("deopt_points", &self.deopt.table.len())
            .field("safepoints", &self.safepoint_records.len())
            .field("osr_headers", &self.osr_headers.len())
            .field("metadata", &self.metadata)
            .finish()
    }
}

impl JitFunctionCode for OptimizedCode {
    fn native_code_address(&self) -> Option<u64> {
        Some(unsafe { self.code.ptr_at(0) as u64 })
    }
    fn return_sites(&self) -> &[otter_vm::native_abi::SafepointEntry] {
        &self.return_sites
    }

    fn metadata(&self) -> CodeObjectMetadata {
        self.code_metadata
    }

    fn retained_bytes(&self) -> u64 {
        crate::template::code::retained_bytes_sum(
            self.code.retained_mapping_bytes(),
            &[
                std::mem::size_of::<Self>(),
                std::mem::size_of_val::<[SafepointRecord]>(&self.safepoint_records),
                std::mem::size_of_val(self.return_sites.as_ref()),
                std::mem::size_of_val::<[CodeDependency]>(&self.dependencies),
                std::mem::size_of_val::<[u32]>(&self.spliced_functions),
                std::mem::size_of_val::<[u16]>(&self._register_operands),
                std::mem::size_of_val(self.osr_headers.as_ref()),
            ],
        )
        .saturating_add(self.deopt.retained_bytes())
        .saturating_add(self.safepoint_records.iter().fold(0u64, |bytes, record| {
            bytes.saturating_add(record.retained_bytes())
        }))
    }

    fn native_frame_kind(&self) -> otter_vm::native_abi::NativeFrameKind {
        otter_vm::native_abi::NativeFrameKind::Optimizing
    }

    fn call_entry_addr(&self) -> Option<usize> {
        // SAFETY: the assembler recorded this entry in the live mapping.
        Some(unsafe { self.code.ptr_at(self.call_entry) as usize })
    }

    fn dependencies(&self) -> &[CodeDependency] {
        &self.dependencies
    }

    fn spliced_functions(&self) -> &[u32] {
        &self.spliced_functions
    }

    fn code_len(&self) -> usize {
        self.code.len()
    }

    fn entry_addr(&self) -> Option<usize> {
        // SAFETY: the executable mapping is owned by `self`; the registry and
        // active entry-cell leases retain this code object for every direct
        // branch using the published address.
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
        self.osr_headers
            .binary_search(&logical_pc)
            .is_ok()
            .then(|| {
                // SAFETY: the live mapping dispatches OSR from the frame bit and PC.
                unsafe { self.code.entry_ptr() as usize }
            })
    }
}

/// Compile `view` with the optimizing tier.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub fn compile_optimized(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    osr_pc: Option<u32>,
) -> Result<OptimizedCode, Unsupported> {
    let transitions = crate::entry::TransitionTable::resolve();
    crate::graph::compile_optimized(view, code_object_id, &transitions, osr_pc, None, false)
        .map(|output| output.code)
}

/// A target without the optimizing tier's code generator.
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub fn compile_optimized(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    osr_pc: Option<u32>,
) -> Result<OptimizedCode, Unsupported> {
    let _ = (view, code_object_id, osr_pc);
    Err(Unsupported::OperandShape(
        "optimizing compiler target is unavailable",
    ))
}

#[cfg(test)]
mod tests {
    use dynasmrt::DynasmApi;
    use otter_bytecode::{Op, Operand};
    use otter_vm::{JitCompileSnapshot, JitFunctionCode, jit::JitTestInstruction};

    use super::{OptimizedCode, OptimizedMetadata, compile_optimized};

    #[test]
    fn retained_bytes_count_mapping_nested_roots_operands_and_finalized_osr_headers() {
        let mut assembler = dynasmrt::x64::Assembler::new_with_capacity(32_768).unwrap();
        let entry = assembler.offset();
        assembler.push(0x90);
        let code = crate::CompiledCode::new(assembler.finalize().unwrap(), entry);
        let mapping_bytes = code.retained_mapping_bytes();
        let mut roots =
            otter_vm::native_abi::SafepointRecord::window(3, otter_vm::native_abi::NO_FRAME_STATE);
        roots.spill_roots = otter_vm::native_abi::SpillRoots::from_slots([0, 64]);
        let compiled = OptimizedCode::new(
            code,
            0,
            Box::default(),
            Box::new([roots]),
            Box::new([]),
            [3, 17].into_iter().collect(),
            Box::default(),
            Box::default(),
            Box::new([0; 11]),
            OptimizedMetadata {
                code_object_id: 91,
                function_id: 7,
                param_count: 0,
                register_count: 1,
                allocator_spill_slot_count: 0,
                spill_slot_count: 0,
            },
        );
        let expected = mapping_bytes
            + std::mem::size_of::<OptimizedCode>()
            + std::mem::size_of::<otter_vm::deopt::DeoptRuntime>()
            + std::mem::size_of::<otter_vm::native_abi::SafepointRecord>()
            + 2 * std::mem::size_of::<u64>()
            + 11 * std::mem::size_of::<u16>()
            + 2 * std::mem::size_of::<u32>();
        assert_eq!(compiled.retained_bytes(), expected as u64);
        assert_eq!(compiled.code_len(), 1);
        assert_eq!(compiled.metadata().code_object_id, 91);
        assert_eq!(JitFunctionCode::metadata(&compiled).code_size, 1);
        assert!(compiled.osr_entry_addr(3).is_some());
        assert!(compiled.osr_entry_addr(17).is_some());
        assert!(compiled.osr_entry_addr(9).is_none());
    }

    /// An opcode with no specialized form still compiles: it runs its
    /// baseline operation, or leaves through a deopt.
    #[test]
    fn optimizer_compiles_every_function_on_its_target() {
        let instructions = vec![
            JitTestInstruction::new(
                Op::NewWeakRef,
                0,
                11,
                vec![Operand::Register(1), Operand::Register(0)],
            ),
            JitTestInstruction::new(Op::ReturnValue, 1, 29, vec![Operand::Register(1)]),
        ];
        let view = JitCompileSnapshot::without_feedback(17, 1, 2, instructions);
        let result = compile_optimized(&view, 91, None);
        assert_eq!(
            result.is_ok(),
            cfg!(any(target_arch = "aarch64", target_arch = "x86_64"))
        );
    }
}
