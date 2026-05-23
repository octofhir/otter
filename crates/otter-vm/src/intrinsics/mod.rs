//! Intrinsics module.
//!
//! Two kinds of code live here:
//!
//! 1. [`dispatch`] — primitive-receiver dispatch tables for
//!    `String.prototype` / `Number.prototype` method routing.
//! 2. Per-constructor installer modules — one per
//!    [`crate::intrinsic_install::BuiltinIntrinsic`]. Each exports an
//!    `Intrinsic` adapter referenced from
//!    [`crate::bootstrap::BOOTSTRAP_ENTRIES`].
//!
//! # See also
//! - [`crate::bootstrap`]

pub mod dispatch;
pub(crate) mod shared;

// Bootstrap installer modules — one per intrinsic.
pub mod array;
pub mod date;
pub mod function;
pub mod iterator;
pub mod number;
pub mod object;
pub mod placeholders;
pub mod proxy;
pub mod symbol;

// Backward-compatible re-exports so existing `crate::intrinsics::Foo`
// call sites continue to resolve.
pub use dispatch::*;
