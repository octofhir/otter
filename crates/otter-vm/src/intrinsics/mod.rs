//! Intrinsics module.
//!
//! Contains two distinct kinds of code:
//!
//! 1. [`dispatch`] — declarative scaffolding for primitive-receiver
//!    intrinsics (`String.prototype` / `Number.prototype` method
//!    routing). Pre-existing; re-exported at this module root for
//!    backward-compatible call paths (`crate::intrinsics::Foo`).
//! 2. Per-constructor installer modules (one per
//!    [`crate::intrinsic_install::BuiltinIntrinsic`]). Each carries
//!    one `Intrinsic` adapter referenced from
//!    [`crate::bootstrap::BOOTSTRAP_ENTRIES`]. Phase 3.1 of the
//!    refactor plan migrates these out of `bootstrap.rs`.
//!
//! # See also
//! - [`crate::bootstrap`]
//! - `docs/architecture-refactor-plan-2026-05.md` Task 3.1

pub mod dispatch;

// Bootstrap installer modules — one per intrinsic.
pub mod array;
pub mod object;
pub mod proxy;
pub mod symbol;

// Backward-compatible re-exports so existing `crate::intrinsics::Foo`
// call sites continue to resolve.
pub use dispatch::*;
