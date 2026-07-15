//! Feedback-guided optimizing tier for reducible numeric and element-access functions.
//!
//! This module compiles multi-block int32 and float64 arithmetic functions,
//! including reducible loops entered at function entry or by on-stack
//! replacement at a hot loop header. It runs
//! the complete CFG, dominance, SSA, liveness, register-allocation,
//! representation, frame-state, and deopt-lowering pipeline before the arm64
//! backend checks the deliberately narrow eligibility contract. CFG edges
//! carry sequentialized phi moves, while every back-edge polls the VM thread's
//! interrupt and fuel cells before returning to its dominating header.
//! Installed code enters through the shared reentrant `JitCtx` ABI and
//! reconstructs the interpreter register window in place before every bail and
//! publishes only precise tagged roots around allocating element transitions.
//!
//! # Contents
//! - [`compile_optimized`] — whole-pipeline compilation entry point.
//! - [`OptimizedCode`] — executable code plus deopt and allocation metadata.
//!
//! # Invariants
//! - Every `LoadElement` / `StoreElement` materializes its operands plus tagged
//!   SSA values live across the call, publishes a code-object-owned precise
//!   frame bitmap, and reloads only locations that moving GC can rewrite.
//!   Store scratch slots remain non-roots and may be clobbered by the runtime.
//! - Every backwards bytecode edge targets a header that dominates its predecessor;
//!   irreducible loops and exception edges are rejected.
//! - The sole ABI argument is a dynamically valid `JitCtx`; parameters, OSR
//!   materialization, and deopt writeback use its rooted interpreter window.
//! - A two-word `JitRet` uses `x0` for a boxed returned value and `x1` for
//!   `RETURNED`, `BAILED`, or `THREW` status.
//! - Phi moves execute before a back-edge poll. Interrupt or exhausted fuel
//!   bails at the target header so the interpreter owns cancellation/refill.
//! - Tagged int32 values are `(0xfffe << 48) | payload_u32`; boxed doubles use
//!   the VM's frozen NaN-box encoding and canonical NaN representation.
//! - Bail writeback is generated from the same [`DeoptTable`] published with
//!   the code object; every interpreter register and the exact logical resume
//!   PC are published before return.
//!
//! # See also
//! - [`crate::ir`] — the reusable optimizing analyses consumed here.
//! - [`crate::template`] — the runtime-wired baseline compiler.

use std::collections::BTreeMap;

use otter_vm::{
    JitCompileSnapshot, JitExecOutcome, JitFunctionCode, VmRuntimeActivation,
    deopt::DeoptTable,
    native_abi::{
        BuildVersionRecord, CodeObjectMetadata, FrameMap, LayoutVersionRecord, SafepointRecord,
        VM_BUILD_VERSION,
    },
};

use crate::{
    CompiledCode, Unsupported,
    entry::{TransitionTable, enter_compiled},
};

#[cfg(target_arch = "aarch64")]
mod arm64;

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
    /// Total number of allocatable GPR and FP registers used by linear scan.
    pub machine_register_count: u8,
    /// GPR and FP spill slots forced by linear scan before deopt legalization.
    pub linear_scan_spill_slot_count: u32,
    /// Number of eight-byte stack spill slots reserved by the emitter.
    pub spill_slot_count: u32,
}

/// Finalized optimizing code and its exact-PC deoptimization metadata.
pub struct OptimizedCode {
    code: CompiledCode,
    deopt_table: DeoptTable,
    safepoint_records: Box<[SafepointRecord]>,
    frame_maps: Box<[FrameMap]>,
    frame_map_bitmap_words: Box<[u64]>,
    /// Loop-header logical PC → assembler offset of its OSR trampoline.
    osr_entries: BTreeMap<u32, usize>,
    /// Per-`MathCall`-site argument window registers. The emitted calls carry
    /// interior pointers into these boxed slices, so they must live exactly as
    /// long as the code.
    _math_call_arguments: BTreeMap<u32, Box<[u16]>>,
    /// Per-`LoadProperty`-site inline caches. Their addresses are baked into
    /// the emitted probes and self-patched by the miss transition, so the
    /// allocation must live exactly as long as the code.
    _load_ic_cells: Box<[crate::entry::WhiskerIcCell]>,
    /// Per-`StoreProperty`-site inline caches, same ownership contract.
    _store_ic_cells: Box<[crate::entry::WhiskerIcCell]>,
    metadata: OptimizedMetadata,
    code_metadata: CodeObjectMetadata,
}

impl OptimizedCode {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        code: CompiledCode,
        deopt_table: DeoptTable,
        safepoint_records: Box<[SafepointRecord]>,
        frame_maps: Box<[FrameMap]>,
        frame_map_bitmap_words: Box<[u64]>,
        osr_entries: BTreeMap<u32, usize>,
        math_call_arguments: BTreeMap<u32, Box<[u16]>>,
        load_ic_cells: Box<[crate::entry::WhiskerIcCell]>,
        store_ic_cells: Box<[crate::entry::WhiskerIcCell]>,
        metadata: OptimizedMetadata,
    ) -> Self {
        const AARCH64_OPTIMIZED_JIT_CTX_ABI: u64 = 0x4136_344f_5054_0005;
        let code_metadata = CodeObjectMetadata {
            id: metadata.code_object_id,
            code_block_id: metadata.function_id,
            entry_offset: 0,
            code_size: code.len() as u32,
            safepoint_count: safepoint_records.len() as u32,
            frame_map_count: frame_maps.len() as u32,
            spill_map_count: 0,
            dependency_count: 0,
            reserved: 0,
            layout: LayoutVersionRecord::CURRENT,
            build: BuildVersionRecord {
                vm_build: VM_BUILD_VERSION,
                target_abi: AARCH64_OPTIMIZED_JIT_CTX_ABI,
            },
        };
        Self {
            code,
            deopt_table,
            safepoint_records,
            frame_maps,
            frame_map_bitmap_words,
            osr_entries,
            _math_call_arguments: math_call_arguments,
            _load_ic_cells: load_ic_cells,
            _store_ic_cells: store_ic_cells,
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
        &self.deopt_table
    }

    /// Return deterministic allocation and source identity metadata.
    #[must_use]
    pub const fn metadata(&self) -> OptimizedMetadata {
        self.metadata
    }

    #[cfg(test)]
    fn frame_map(&self, id: u32) -> Option<&FrameMap> {
        self.frame_maps
            .binary_search_by_key(&id, |frame_map| frame_map.id)
            .ok()
            .map(|index| &self.frame_maps[index])
    }

    #[cfg(test)]
    fn frame_map_bitmap_words(&self) -> &[u64] {
        &self.frame_map_bitmap_words
    }

    #[cfg(test)]
    unsafe fn osr_entry_ptr_for_test(&self, logical_pc: u32) -> Option<*const u8> {
        let offset = *self.osr_entries.get(&logical_pc)?;
        // SAFETY: tests keep this code object alive through the native call.
        Some(unsafe { self.code.ptr_at(offset) })
    }
}

impl std::fmt::Debug for OptimizedCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OptimizedCode")
            .field("code_len", &self.code.len())
            .field("deopt_points", &self.deopt_table.len())
            .field("safepoints", &self.safepoint_records.len())
            .field("frame_maps", &self.frame_maps.len())
            .field("osr_entries", &self.osr_entries.len())
            .field("frame_map_bitmap_words", &self.frame_map_bitmap_words.len())
            .field("metadata", &self.metadata)
            .finish()
    }
}

impl JitFunctionCode for OptimizedCode {
    fn metadata(&self) -> CodeObjectMetadata {
        self.code_metadata
    }

    fn code_len(&self) -> usize {
        self.code.len()
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

    fn run_entry(&self, _activation: otter_vm::VmRuntimeActivation) -> JitExecOutcome {
        JitExecOutcome::Threw(otter_vm::VmError::InvalidOperand)
    }

    fn run_optimized_entry(&self, activation: VmRuntimeActivation) -> Option<JitExecOutcome> {
        if !self.code_metadata.is_compatible_with_current_vm() {
            return None;
        }
        // SAFETY: this object owns the live executable mapping, whose entry was
        // emitted with the shared `JitCtx` ABI. `activation` carries the VM's
        // frozen-borrow contract for the dynamic call.
        let entry = unsafe { self.code.entry_ptr() };
        Some(unsafe {
            enter_compiled(
                activation,
                entry,
                self.metadata.code_object_id,
                self.metadata.function_id,
                self.metadata.register_count,
                !self.safepoint_records.is_empty(),
            )
        })
    }

    fn run_optimized_osr_entry(
        &self,
        activation: VmRuntimeActivation,
        logical_pc: u32,
    ) -> Option<JitExecOutcome> {
        if !self.code_metadata.is_compatible_with_current_vm() {
            return None;
        }
        let offset = *self.osr_entries.get(&logical_pc)?;
        // SAFETY: the recorded offset belongs to this live executable mapping
        // and names a trampoline emitted with the shared `JitCtx` ABI.
        let entry = unsafe { self.code.ptr_at(offset) };
        Some(unsafe {
            enter_compiled(
                activation,
                entry,
                self.metadata.code_object_id,
                self.metadata.function_id,
                self.metadata.register_count,
                !self.safepoint_records.is_empty(),
            )
        })
    }
}

/// Compile the minimal numeric optimizing subset after running every existing IR
/// analysis, or return [`Unsupported`] without producing executable code.
#[cfg(target_arch = "aarch64")]
pub fn compile_optimized(
    view: &JitCompileSnapshot,
    code_object_id: u64,
) -> Result<OptimizedCode, Unsupported> {
    let transitions = TransitionTable::resolve();
    arm64::compile_with_transitions(view, code_object_id, &transitions)
}

#[cfg(target_arch = "aarch64")]
pub(crate) fn compile_optimized_with_transitions(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &TransitionTable,
) -> Result<OptimizedCode, Unsupported> {
    arm64::compile_with_transitions(view, code_object_id, transitions)
}

/// Non-arm64 stub: the first optimizing backend is arm64-only.
#[cfg(not(target_arch = "aarch64"))]
pub fn compile_optimized(
    view: &JitCompileSnapshot,
    code_object_id: u64,
) -> Result<OptimizedCode, Unsupported> {
    let _ = (view, code_object_id);
    Err(Unsupported::OperandShape(
        "optimizing compiler is arm64-only",
    ))
}

#[cfg(not(target_arch = "aarch64"))]
pub(crate) fn compile_optimized_with_transitions(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &TransitionTable,
) -> Result<OptimizedCode, Unsupported> {
    let _ = transitions;
    compile_optimized(view, code_object_id)
}

#[cfg(test)]
mod tests {
    use otter_bytecode::{Op, Operand};
    use otter_vm::{JitCompileSnapshot, jit::JitTestInstruction};

    use super::compile_optimized;

    #[test]
    fn refuses_out_of_subset_on_every_host() {
        let instructions = vec![
            JitTestInstruction::new(
                Op::TypeOf,
                0,
                11,
                vec![Operand::Register(1), Operand::Register(0)],
            ),
            JitTestInstruction::new(Op::ReturnValue, 1, 29, vec![Operand::Register(1)]),
        ];
        let view = JitCompileSnapshot::without_feedback(17, 1, 2, instructions);
        assert!(compile_optimized(&view, 91).is_err());
    }
}
