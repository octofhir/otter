//! Shared AArch64 emission primitives used by every native JIT tier.
//!
//! # Contents
//! - [`CallTrampoline`] — the owned compiled-to-compiled call lifecycle.
//! - [`emit_prepared_call`] — the tier-neutral call-site bridge.
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

mod calls;

pub(crate) use calls::{CallTrampoline, emit_prepared_call};
