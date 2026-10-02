//! Shared System V x86-64 emission primitives used by every native JIT tier.
//!
//! # Contents
//! - [`js_call`] — generated JavaScript calls.
//! - [`activation`] — call-entry pieces shared by both tiers.
//! - [`allocation`] — nursery carves shared by both tiers.
//!
//! # Invariants
//! - Shared emitters consume the target-neutral VM descriptors and native-frame
//!   contract; Template and Machine provide only their value-home accessors.
//!
//! # See also
//! - `crate::arm64` — peer target implementation.

pub(crate) mod activation;
pub(crate) mod allocation;
pub(crate) mod js_call;

pub(crate) mod arguments;
