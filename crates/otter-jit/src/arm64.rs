//! Shared AArch64 emission primitives used by every native JIT tier.
//!
//! # Contents
//! - [`emit_direct_call`] — compiler-generated monomorphic plain-call linkage.
//! - Guarded static-native leaves for extracted builtins.
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

mod direct_call;
mod method_guard;

/// Backedges between shared interrupt/fuel-cell probes in generated code.
///
/// `x29` holds the activation-local countdown in optimizing and scalar Machine
/// IR bodies. Both tiers subtract this exact batch from shared VM fuel when the
/// countdown expires, keeping accounting and interrupt latency aligned.
pub(crate) const GENERATED_POLL_BATCH: u32 = 16;

pub(crate) use direct_call::{
    DirectCallArguments, DirectCallForm, DirectCallSite, direct_call_artifact, emit_direct_call,
    emit_direct_call_with_access, emit_runtime_forward,
    target_is_supported as direct_call_target_is_supported,
};
pub(crate) use method_guard::{
    MethodGuardSite, emit_method_guard, emit_method_guard_from_tagged_register,
};
