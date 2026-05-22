//! Runtime value-kind predicates shared by VM object operations.
//!
//! The `Value` enum lives in `lib.rs`, but spec algorithms often need the
//! broader ECMA-262 `Type(value) is Object` classification. Keeping that
//! classifier here prevents each builtin path from growing its own partial
//! variant list.
//!
//! # Contents
//! - [`is_object_like_value`] — `true` for every VM value variant that has
//!   object identity and internal methods.
//!
//! # Invariants
//! - Primitive variants (`undefined`, `null`, booleans, numbers, strings,
//!   symbols, bigint, and internal holes) are never object-like.
//! - Object-like variants must stay in sync with `Value` variants that expose
//!   ECMAScript object internal methods.
//!
//! # See also
//! - [`crate::Value`]
//! - <https://tc39.es/ecma262/#sec-ecmascript-data-types-and-values>

use crate::Value;

#[must_use]
pub(crate) fn is_object_like_value(value: &Value) -> bool {
    value.is_object_like()
}
