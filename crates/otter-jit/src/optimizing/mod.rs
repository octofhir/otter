//! Finalized code objects and entry points for the Machine optimizing tier.
//!
//! Supported functions lower once through typed scalar HIR, target-neutral
//! Machine IR, regalloc2, exact safepoint/deoptimization metadata, and the
//! selected target encoder. A function outside that pipeline returns [`Unsupported`];
//! optimizing compilation has no second SSA, allocator, emitter, or artifact
//! fallback. The VM retains the independently compiled template body as its
//! baseline for such functions.
//!
//! # Contents
//! - [`compile_optimized`] — whole-pipeline compilation entry point.
//! - [`OptimizedCode`] — executable code plus deopt and allocation metadata.
//! - [`compile_optimized`] — the sole optimizing compilation entry point.
//!
//! # Invariants
//! - Allocating or reentrant Machine calls publish precise tagged roots derived
//!   from final allocator locations and reload only values moving GC can rewrite.
//! - Every backwards bytecode edge targets a header that dominates its
//!   predecessor; irreducible loops and unsupported exception regions reject
//!   the Machine compilation instead of selecting another optimizer.
//! - The sole entry ABI argument is a dynamically valid `JitCtx`; parameters
//!   and OSR inputs enter allocator-owned homes, while cold deoptimization
//!   reconstructs the interpreter window from exact metadata.
//! - The VM-owned two-word `NativeResultPair` uses `x0` for a boxed
//!   Return/Throw value or exact Bail PC and `x1` for status.
//! - Deopt reconstruction is generated from the same [`DeoptTable`] published
//!   with the code object; every live interpreter value and the exact logical
//!   resume PC are committed before returning to the VM.
//! - Every backend publishes bytes through the same [`CompiledCode`], code
//!   registry, native-frame kind, artifact bundle, and W^X lifecycle.
//!
//! # See also
//! - [`crate::machine`] — the sole optimizing instruction and allocation path.
//! - [`crate::template`] — the runtime-wired baseline compiler.

use std::collections::BTreeSet;

use otter_vm::{
    JitCompileSnapshot, JitFunctionCode,
    deopt::DeoptTable,
    native_abi::{CodeDependency, CodeObjectMetadata, SafepointRecord},
};

use crate::{CompiledCode, Unsupported, entry::TransitionTable};

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
    /// Total number of physical GPR and FP registers used by regalloc2.
    pub machine_register_count: u8,
    /// Spill slots selected by regalloc2 before root-save area reservation.
    pub allocator_spill_slot_count: u32,
    /// Number of eight-byte allocator-spill and GC-root-save slots reserved by
    /// the emitter.
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
    /// Loop-header logical PCs the entry dispatch can enter through an OSR
    /// block: the compile's OSR target, or nothing for an entry compile.
    osr_headers: BTreeSet<u32>,
    /// Exact installed callee generations entered by emitted direct edges.
    dependencies: Box<[CodeDependency]>,
    /// Per-`LoadProperty`-site inline caches. Their addresses are baked into
    /// the emitted probes and self-patched by the miss transition, so the
    /// allocation must live exactly as long as the code.
    _load_ic_cells: Box<[crate::entry::PropertySourceCell]>,
    /// Per-`StoreProperty`-site inline caches, same ownership contract.
    _store_ic_cells: Box<[crate::entry::PropertySourceCell]>,
    /// Operand registers of generic baseline operations whose addresses the
    /// code bakes; same ownership contract.
    _register_operands: Box<[u16]>,
    metadata: OptimizedMetadata,
    code_metadata: CodeObjectMetadata,
}

impl OptimizedCode {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        code: CompiledCode,
        call_entry: usize,
        deopt: Box<otter_vm::deopt::DeoptRuntime>,
        safepoint_records: Box<[SafepointRecord]>,
        osr_headers: BTreeSet<u32>,
        dependencies: Box<[CodeDependency]>,
        load_ic_cells: Box<[crate::entry::PropertySourceCell]>,
        store_ic_cells: Box<[crate::entry::PropertySourceCell]>,
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
            osr_headers,
            dependencies,
            _load_ic_cells: load_ic_cells,
            _store_ic_cells: store_ic_cells,
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

    #[cfg(test)]
    pub(crate) fn has_osr_header_for_test(&self, logical_pc: u32) -> bool {
        self.osr_headers.contains(&logical_pc)
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
    fn metadata(&self) -> CodeObjectMetadata {
        self.code_metadata
    }

    fn retained_bytes(&self) -> u64 {
        crate::template::code::retained_bytes_sum(
            self.code.len(),
            &[
                std::mem::size_of_val::<[SafepointRecord]>(&self.safepoint_records),
                std::mem::size_of_val::<[CodeDependency]>(&self.dependencies),
                std::mem::size_of_val::<[crate::entry::PropertySourceCell]>(&self._load_ic_cells),
                std::mem::size_of_val::<[crate::entry::PropertySourceCell]>(&self._store_ic_cells),
                self.osr_headers.len() * std::mem::size_of::<u32>(),
            ],
        )
        .saturating_add(self.deopt.retained_bytes())
        .saturating_add(self.safepoint_records.iter().fold(0u64, |bytes, record| {
            bytes.saturating_add(record.inline_retained_bytes())
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
        self.osr_headers.contains(&logical_pc).then(|| {
            // SAFETY: the live mapping dispatches OSR from the frame bit and PC.
            unsafe { self.code.entry_ptr() as usize }
        })
    }
}

/// Compile through the Machine optimizing backend, or return [`Unsupported`]
/// without producing executable code.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub fn compile_optimized(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    osr_pc: Option<u32>,
) -> Result<OptimizedCode, Unsupported> {
    let transitions = TransitionTable::resolve();
    compile_optimized_with_artifacts(view, code_object_id, &transitions, false, None, osr_pc)
        .map(|output| output.code)
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub(crate) fn compile_optimized_with_artifacts(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &TransitionTable,
    capture_events: bool,
    artifact_request: Option<crate::artifact::ArtifactRequest>,
    osr_pc: Option<u32>,
) -> Result<crate::artifact::NativeCompileOutput<OptimizedCode>, Unsupported> {
    #[cfg(target_arch = "aarch64")]
    let target_spec = crate::machine::TargetSpec::aarch64();
    #[cfg(target_arch = "x86_64")]
    let target_spec = crate::machine::TargetSpec::x86_64();
    crate::machine::numeric::try_compile(
        &target_spec,
        view,
        code_object_id,
        transitions,
        capture_events,
        artifact_request,
        osr_pc,
    )
}

/// Stub for architectures without a native optimizing encoder.
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

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub(crate) fn compile_optimized_with_transitions(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &TransitionTable,
    osr_pc: Option<u32>,
) -> Result<OptimizedCode, Unsupported> {
    let _ = transitions;
    compile_optimized(view, code_object_id, osr_pc)
}

#[cfg(test)]
mod tests {
    use otter_bytecode::{Op, Operand};
    use otter_vm::{JitCompileSnapshot, jit::JitTestInstruction};

    use super::compile_optimized;

    #[test]
    fn machine_only_optimizer_refuses_out_of_subset_on_every_host() {
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
        assert!(result.is_err());
        // The refusal names the opcode the Machine HIR has no lowering for.
        #[cfg(target_arch = "aarch64")]
        assert!(matches!(
            result,
            Err(super::Unsupported::Constraint {
                op: Op::NewWeakRef,
                constraint: "opcode outside the Machine HIR",
            })
        ));
    }
}
