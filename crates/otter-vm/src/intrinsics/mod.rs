//! Intrinsics module.
//!
//! Per-constructor installer modules live here — one per
//! [`crate::intrinsic_install::BuiltinIntrinsic`]. Each exports an
//! `Intrinsic` adapter referenced from
//! [`crate::bootstrap::BOOTSTRAP_ENTRIES`].
//!
//! # See also
//! - [`crate::bootstrap`]

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
