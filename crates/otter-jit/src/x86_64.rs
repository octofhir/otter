//! Shared x86-64 emission primitives used by every native JIT tier.
//!
//! # Contents
//! - [`js_call`] — generated JavaScript calls.
//! - [`activation`] — call-entry pieces shared by both tiers.
//! - [`frame`] — shared call and tier-entry frame publication and retirement.
//! - [`call_abi`] — native C entry and runtime-call adaptation.
//! - [`values`] — immediate, symbolic and canonical boxed-number encoding.
//! - [`binding`] — live source-owned global lexical/object binding reads.
//! - [`fields`] — persistent inline prefix and live suffix address encoding.
//! - [`property_actions`] — one shared VM action-table probe after property PICs.
//! - [`allocation`] — nursery carves shared by both tiers.
//! - [`method_guard`] — live receiver, prototype and callable identity proofs.
//!
//! # Invariants
//! - Shared emitters consume the target-neutral VM descriptors and native-frame
//!   contract; each tier provides only its value-home accessors.
//!
//! # See also
//! - `crate::arm64` — peer target implementation.

// dynasm 5 normalizes dynamic x86-64 register operands through `Into<u8>`;
// register ids in shared emitters are already `u8`, so the macro-generated
// conversion is intentionally redundant.
#![allow(clippy::useless_conversion)]

pub(crate) mod activation;
pub(crate) mod allocation;
pub(crate) mod binding;
pub(crate) mod call_abi;
pub(crate) mod fields;
pub(crate) mod frame;
pub(crate) mod js_call;
pub(crate) mod method_guard;
pub(crate) mod property_actions;
pub(crate) mod property_ic;
pub(crate) mod values;

pub(crate) mod arguments;
