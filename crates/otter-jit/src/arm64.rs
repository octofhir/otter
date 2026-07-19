//! Shared AArch64 emission primitives used by every native JIT tier.
//!
//! # Contents
//! - [`emit_direct_call`] — compiler-generated monomorphic plain-call linkage.
//! - Guarded static-native leaves for extracted builtins.
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
mod static_native;

pub(crate) use direct_call::{
    DirectCallForm, DirectCallSite, direct_call_artifact, emit_direct_call,
    target_is_supported as direct_call_target_is_supported,
};
pub(crate) use method_guard::{MethodGuardSite, emit_method_guard};
pub(crate) use static_native::{
    StaticNativeCallSite, emit_static_native_call,
    target_is_supported as static_native_target_is_supported,
};
