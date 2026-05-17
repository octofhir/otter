//! `Boolean` built-in surface.
//!
//! # Submodules
//! - [`intrinsic`] — `BuiltinIntrinsic` impl that materialises the
//!   global `Boolean` constructor + prototype + the
//!   call/construct bridge.
//! - [`prototype`] — `Boolean.prototype.*` intrinsic table consumed
//!   by `Op::CallBoolean` and by the JS-visible methods installed
//!   on the prototype object during bootstrap.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-boolean-objects>

pub mod intrinsic;
pub mod prototype;
