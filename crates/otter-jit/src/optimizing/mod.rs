//! First machine-code slice of the feedback-guided optimizing tier.
//!
//! This module compiles only one-basic-block int32 arithmetic leaves. It runs
//! the complete CFG, dominance, SSA, liveness, register-allocation,
//! representation, frame-state, and deopt-lowering pipeline before the arm64
//! backend checks the deliberately narrow eligibility contract. It is not
//! installed in runtime tier selection.
//!
//! # Contents
//! - [`compile_optimized`] — whole-pipeline compilation entry point.
//! - [`OptimizedCode`] — executable code plus deopt and allocation metadata.
//! - [`OptimizedLeafEntry`] and [`OptimizedLeafRet`] — standalone test ABI.
//!
//! # Invariants
//! - Entries are leaves: they never allocate, call, safepoint, or re-enter the VM.
//! - ABI argument `x0` points to tagged `u64` parameters. A two-word return
//!   uses `x0` for a boxed result or deopt byte PC and `x1` for status.
//! - Tagged int32 values are `(0xfffe << 48) | payload_u32`.
//! - This compiler is additive and no runtime path calls it.
//!
//! # See also
//! - [`crate::ir`] — the reusable optimizing analyses consumed here.
//! - [`crate::template`] — the runtime-wired baseline compiler.

use otter_vm::{JitCompileSnapshot, deopt::DeoptTable};

use crate::{CompiledCode, Unsupported};

#[cfg(target_arch = "aarch64")]
mod arm64;

/// Successful standalone optimizing-leaf status.
pub const OPTIMIZED_STATUS_RETURNED: u64 = crate::entry::STATUS_RETURNED;
/// Speculation-failure standalone optimizing-leaf status.
pub const OPTIMIZED_STATUS_DEOPT: u64 = crate::entry::STATUS_DEOPT;

/// Two-word return of an [`OptimizedLeafEntry`].
///
/// On [`OPTIMIZED_STATUS_RETURNED`], `value` is a boxed int32. On
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

/// Standalone optimizing-leaf calling convention.
///
/// The pointer names a contiguous tagged-`u64` parameter array. On arm64 it
/// arrives in `x0`; [`OptimizedLeafRet`] returns in `x0`/`x1` per AAPCS64.
pub type OptimizedLeafEntry = extern "C" fn(*const u64) -> OptimizedLeafRet;

/// Deterministic metadata for one optimized leaf compilation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptimizedMetadata {
    /// Isolate-assigned identity supplied to [`compile_optimized`].
    pub code_object_id: u64,
    /// Source bytecode function identity.
    pub function_id: u32,
    /// Number of allocatable machine registers used by linear scan.
    pub machine_register_count: u8,
    /// Spill slots forced by linear scan before deopt-location legalization.
    pub linear_scan_spill_slot_count: u32,
    /// Number of eight-byte stack spill slots reserved by the emitter.
    pub spill_slot_count: u32,
}

/// Finalized optimizing-leaf code and its exact-PC deoptimization metadata.
pub struct OptimizedCode {
    code: CompiledCode,
    deopt_table: DeoptTable,
    metadata: OptimizedMetadata,
}

impl OptimizedCode {
    pub(super) fn new(
        code: CompiledCode,
        deopt_table: DeoptTable,
        metadata: OptimizedMetadata,
    ) -> Self {
        Self {
            code,
            deopt_table,
            metadata,
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

/// Compile the minimal int32 optimizing subset after running every existing IR
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
