//! `BigInt.prototype` method metadata.
//!
//! The executable method bodies live in
//! [`crate::bootstrap_bigint`], where the `BigInt` `couch!`
//! surface installs real `NativeCtx` functions on
//! `BigInt.prototype`.
//!
//! # Contents
//! - [`is_builtin_method`] — direct-call guard used by
//!   `CallMethodValue` before it falls through to `GetMethod + Call`.
//!
//! # Invariants
//! - Prototype behavior is installed through the bootstrap native
//!   surface.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-bigint-prototype-object>

/// Whether `name` is an installed `BigInt.prototype` method.
#[must_use]
pub fn is_builtin_method(name: &str) -> bool {
    matches!(name, "toString" | "valueOf")
}
