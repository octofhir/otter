//! Feedback-guided optimizing tier for reducible numeric arithmetic functions.
//!
//! This module compiles multi-block int32 and float64 arithmetic leaves, including
//! reducible loops entered only at function entry. It runs
//! the complete CFG, dominance, SSA, liveness, register-allocation,
//! representation, frame-state, and deopt-lowering pipeline before the arm64
//! backend checks the deliberately narrow eligibility contract. CFG edges
//! carry sequentialized phi moves, while every back-edge polls the VM thread's
//! interrupt and fuel cells before returning to its dominating header.
//! Installed code reconstructs the interpreter register file in place before
//! every deopt.
//!
//! # Contents
//! - [`compile_optimized`] — whole-pipeline compilation entry point.
//! - [`OptimizedCode`] — executable code plus deopt and allocation metadata.
//! - [`OptimizedLeafEntry`] and [`OptimizedLeafRet`] — production leaf ABI.
//!
//! # Invariants
//! - Entries are leaves: they never allocate, call, safepoint, or re-enter the VM.
//! - Every backwards bytecode edge targets a header that dominates its predecessor;
//!   irreducible loops and exception edges are rejected.
//! - ABI arguments `x0`, `x1`, and `x2` point to tagged `u64` parameters, the
//!   interpreter register file, and a dynamically valid [`VmThread`]. A
//!   two-word return uses `x0` for a boxed result or deopt byte PC and `x1` for
//!   status.
//! - Phi moves execute before a back-edge poll. Interrupt or exhausted fuel
//!   deopts at the target header so the interpreter owns cancellation/refill.
//! - Tagged int32 values are `(0xfffe << 48) | payload_u32`; boxed doubles use
//!   the VM's frozen NaN-box encoding and canonical NaN representation.
//! - Deopt writeback is generated from the same [`DeoptTable`] published with
//!   the code object; every interpreter register is materialized before return.
//!
//! # See also
//! - [`crate::ir`] — the reusable optimizing analyses consumed here.
//! - [`crate::template`] — the runtime-wired baseline compiler.

use otter_vm::{
    JitCompileSnapshot, JitExecOutcome, JitFunctionCode, JitOptimizedExecOutcome, Value,
    deopt::DeoptTable,
    native_abi::{
        BuildVersionRecord, CodeObjectMetadata, LayoutVersionRecord, VM_BUILD_VERSION, VmThread,
    },
};

use crate::{CompiledCode, Unsupported};

#[cfg(target_arch = "aarch64")]
mod arm64;

/// Successful standalone optimizing-leaf status.
pub const OPTIMIZED_STATUS_RETURNED: u64 = crate::entry::STATUS_RETURNED;
/// Speculation-failure standalone optimizing-leaf status.
pub const OPTIMIZED_STATUS_DEOPT: u64 = crate::entry::STATUS_DEOPT;

/// Two-word return of an [`OptimizedLeafEntry`].
///
/// On [`OPTIMIZED_STATUS_RETURNED`], `value` is a boxed numeric value. On
/// [`OPTIMIZED_STATUS_DEOPT`], `value` is the exact interpreter byte PC whose
/// speculation failed.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptimizedLeafRet {
    /// Boxed return value or exact deopt byte PC, selected by [`Self::status`].
    pub value: u64,
    /// [`OPTIMIZED_STATUS_RETURNED`] or [`OPTIMIZED_STATUS_DEOPT`].
    pub status: u64,
}

/// Optimizing-leaf calling convention.
///
/// The first pointer names a contiguous tagged-`u64` parameter array, the
/// second names the writable interpreter register file, and the third names
/// the VM thread record whose interrupt and back-edge-fuel cells remain valid
/// for the call's dynamic extent. On arm64 they arrive in `x0`/`x1`/`x2`;
/// [`OptimizedLeafRet`] returns in `x0`/`x1` per AAPCS64.
pub type OptimizedLeafEntry =
    extern "C" fn(*const u64, *mut u64, *mut VmThread) -> OptimizedLeafRet;

/// Deterministic metadata for one optimized leaf compilation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptimizedMetadata {
    /// Isolate-assigned identity supplied to [`compile_optimized`].
    pub code_object_id: u64,
    /// Source bytecode function identity.
    pub function_id: u32,
    /// Number of tagged parameters consumed by the leaf ABI.
    pub param_count: u16,
    /// Number of writable interpreter registers reconstructed on deopt.
    pub register_count: u16,
    /// Total number of allocatable GPR and FP registers used by linear scan.
    pub machine_register_count: u8,
    /// GPR and FP spill slots forced by linear scan before deopt legalization.
    pub linear_scan_spill_slot_count: u32,
    /// Number of eight-byte stack spill slots reserved by the emitter.
    pub spill_slot_count: u32,
}

/// Finalized optimizing-leaf code and its exact-PC deoptimization metadata.
pub struct OptimizedCode {
    code: CompiledCode,
    deopt_table: DeoptTable,
    metadata: OptimizedMetadata,
    code_metadata: CodeObjectMetadata,
}

impl OptimizedCode {
    pub(super) fn new(
        code: CompiledCode,
        deopt_table: DeoptTable,
        metadata: OptimizedMetadata,
    ) -> Self {
        const AARCH64_OPTIMIZED_LEAF_ABI: u64 = 0x4136_344f_5054_0003;
        let code_metadata = CodeObjectMetadata {
            id: metadata.code_object_id,
            code_block_id: metadata.function_id,
            entry_offset: 0,
            code_size: code.len() as u32,
            safepoint_count: 0,
            frame_map_count: 0,
            spill_map_count: 0,
            dependency_count: 0,
            reserved: 0,
            layout: LayoutVersionRecord::CURRENT,
            build: BuildVersionRecord {
                vm_build: VM_BUILD_VERSION,
                target_abi: AARCH64_OPTIMIZED_LEAF_ABI,
            },
        };
        Self {
            code,
            deopt_table,
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
}

impl std::fmt::Debug for OptimizedCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OptimizedCode")
            .field("code_len", &self.code.len())
            .field("deopt_points", &self.deopt_table.len())
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

    fn run_entry(&self, _activation: otter_vm::VmRuntimeActivation) -> JitExecOutcome {
        JitExecOutcome::Threw(otter_vm::VmError::InvalidOperand)
    }

    fn run_optimized_entry(
        &self,
        params: &[u64],
        frame_registers: &mut [Value],
        thread: *mut VmThread,
    ) -> Option<JitOptimizedExecOutcome> {
        if params.len() < usize::from(self.metadata.param_count)
            || frame_registers.len() < usize::from(self.metadata.register_count)
            || thread.is_null()
        {
            return None;
        }
        // SAFETY: the compiler emitted `OptimizedLeafEntry`, this object owns
        // the executable mapping through the call, and both slices satisfy the
        // lengths recorded in immutable compilation metadata. The VM-owned
        // thread record and its poll cells remain live until this call returns.
        let entry: OptimizedLeafEntry = unsafe { std::mem::transmute(self.code.entry_ptr()) };
        let result = entry(
            params.as_ptr(),
            frame_registers.as_mut_ptr().cast::<u64>(),
            thread,
        );
        match result.status {
            OPTIMIZED_STATUS_RETURNED => Some(JitOptimizedExecOutcome::Returned(Value::from_bits(
                result.value,
            ))),
            OPTIMIZED_STATUS_DEOPT if self.deopt_table.lookup(result.value as u32).is_some() => {
                Some(JitOptimizedExecOutcome::Deopt(result.value as u32))
            }
            _ => None,
        }
    }
}

/// Compile the minimal numeric optimizing subset after running every existing IR
/// analysis, or return [`Unsupported`] without producing executable code.
#[cfg(target_arch = "aarch64")]
pub fn compile_optimized(
    view: &JitCompileSnapshot,
    code_object_id: u64,
) -> Result<OptimizedCode, Unsupported> {
    arm64::compile(view, code_object_id)
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

#[cfg(test)]
mod tests {
    use otter_bytecode::{Op, Operand};
    use otter_vm::{JitCompileSnapshot, jit::JitTestInstruction};

    use super::compile_optimized;

    #[test]
    fn refuses_out_of_subset_on_every_host() {
        let instructions = vec![
            JitTestInstruction::new(
                Op::LoadProperty,
                0,
                11,
                vec![
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                ],
            ),
            JitTestInstruction::new(Op::ReturnValue, 1, 29, vec![Operand::Register(1)]),
        ];
        let view = JitCompileSnapshot::without_feedback(17, 1, 2, instructions);
        assert!(compile_optimized(&view, 91).is_err());
    }
}
