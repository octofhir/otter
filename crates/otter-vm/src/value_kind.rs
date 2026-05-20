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
    matches!(
        value,
        Value::Object(_)
            | Value::Array(_)
            | Value::Function { .. }
            | Value::Closure { .. }
            | Value::NativeFunction(_)
            | Value::BoundFunction(_)
            | Value::ClassConstructor(_)
            | Value::Promise(_)
            | Value::Iterator(_)
            | Value::RegExp(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::WeakMap(_)
            | Value::WeakSet(_)
            | Value::WeakRef(_)
            | Value::FinalizationRegistry(_)
            | Value::Temporal(_)
            | Value::Intl(_)
            | Value::ArrayBuffer(_)
            | Value::DataView(_)
            | Value::TypedArray(_)
            | Value::Generator(_)
            | Value::Proxy(_)
    )
}
