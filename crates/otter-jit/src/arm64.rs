//! Shared AArch64 emission primitives used by every native JIT tier.
//!
//! # Contents
//! - [`emit_direct_call`] — compiler-generated monomorphic plain-call linkage.
//! - Guarded static-native leaves for extracted builtins.
//! - [`allocation`] — nursery carves shared by both tiers.
//! - [`property_actions`] — one live property-action probe shared by both tiers.
//! - [`binding`] — source-owned global reads and guards shared by both tiers.
//! - Shared generated-code policy constants used by multiple native tiers.
//!
//! # Invariants
//! - Common emitters depend only on the crate-wide entry ABI and VM runtime
//!   descriptors; they never depend on template- or optimizing-tier internals.
//! - Tier-specific code establishes the shared compiled-entry register
//!   convention before invoking an emitter from this module.
//!
//! # See also
//! - [`crate::entry`] — the shared compiled-entry ABI and transition table.
//! - [`crate::template`] — the baseline tier consuming these primitives.
//! - [`crate::optimizing`] — the optimizing tier consuming these primitives.

// dynasm 5 normalizes dynamic AArch64 register operands through `Into<u8>`;
// register ids in shared emitters are already `u8`, so the macro-generated
// conversion is intentionally redundant.
#![allow(clippy::useless_conversion)]

pub(crate) mod activation;
pub(crate) mod allocation;
pub(crate) mod binding;
pub(crate) mod frame;
pub(crate) mod inline_guard;
pub(crate) mod js_call;
mod method_guard;
pub(crate) mod property_actions;
pub(crate) mod property_ic;
mod receiver_allocation;

/// Whether this CPU implements the ARMv8.3 JavaScript conversion
/// (`FJCVTZS`), which computes ECMAScript ToInt32 of a double in one
/// instruction. V8 selects the same instruction under `JSCVT`.
pub(crate) fn has_javascript_conversion() -> bool {
    std::arch::is_aarch64_feature_detected!("jsconv")
}

/// `W(destination) = ToInt32(D(source))` through `FJCVTZS`: NaN and the
/// infinities give zero, every finite value its low 32 bits modulo 2^32.
/// Writing the W register clears the upper half. Requires
/// [`has_javascript_conversion`].
pub(crate) fn emit_fjcvtzs(ops: &mut dynasmrt::aarch64::Assembler, source: u8, destination: u8) {
    use dynasmrt::DynasmApi;
    debug_assert!(source < 32 && destination < 32);
    ops.push_u32(0x1E7E_0000 | (u32::from(source) << 5) | u32::from(destination));
}

pub(crate) use method_guard::{MethodGuardSite, emit_method_guard};

/// Largest bytecode body whose code loads symbols from a literal pool: its
/// code, about twelve bytes per bytecode byte plus inlined bodies, stays
/// well within the ±1 MiB reach of `LDR (literal)`.
const LITERAL_POOL_MAX_BYTECODE_BYTES: u32 = 32 * 1024;

/// Relocation capture for one code object of `view`, with the literal pool
/// enabled when the body is small enough for the pool to stay in range.
pub(crate) fn literal_pool_capture(
    capture: bool,
    view: &otter_vm::JitCompileSnapshot,
) -> crate::artifact::relocation::RelocationCapture {
    let relocations = crate::artifact::relocation::RelocationCapture::new(capture);
    if view.code_block.bytecode_byte_len() <= LITERAL_POOL_MAX_BYTECODE_BYTES {
        relocations.with_literal_pool()
    } else {
        relocations
    }
}
pub(crate) use receiver_allocation::{
    emit_receiver_bump, emit_receiver_candidate_probe, emit_receiver_fit, emit_receiver_guards,
    emit_receiver_publication_effect,
};

pub(crate) mod arguments;
