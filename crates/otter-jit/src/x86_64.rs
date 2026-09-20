//! Shared System V x86-64 emission primitives used by every native JIT tier.
//!
//! # Contents
//! - [`emit_runtime_forward`] — runtime-selected generated argument forwarding.
//!
//! # Invariants
//! - Shared emitters consume the target-neutral VM descriptors and native-frame
//!   contract; Template and Machine provide only their value-home accessors.
//! - A forwarding miss occurs before call effects. Once a callee frame is
//!   published, completion restores the caller and never replays the call.
//! - Dynamic frames initialize every traced word before allocation or publication.
//!
//! # See also
//! - `crate::arm64::direct_call::runtime_forward` — peer target implementation.
//! - `otter_vm::runtime_activation::forward_arguments` — admission and copy rules.

mod runtime_forward;

pub(crate) use runtime_forward::emit_runtime_forward;
