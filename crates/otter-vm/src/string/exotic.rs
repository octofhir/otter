//! String exotic object internal-method helpers.
//!
//! Owns the spec-visible virtual own properties exposed by String
//! wrapper objects and primitive-string `ToObject` views: indexed
//! code-unit data properties and the non-writable `length` property.
//!
//! # Contents
//! - [`descriptor_for_key`] — `[[GetOwnProperty]]` for VM property keys.
//! - [`descriptor_for_name`] — string-name variant for static/native
//!   dispatchers that already performed `ToPropertyKey`.
//!
//! # Invariants
//! - Indexed code-unit descriptors are non-writable, enumerable, and
//!   non-configurable.
//! - `length` is non-writable, non-enumerable, and non-configurable.
//! - No object state is mutated here; callers that apply
//!   `[[DefineOwnProperty]]` compare proposed descriptors against
//!   these virtual descriptors.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-string-exotic-objects>
//! - [`crate::object_internal_ops`]

use crate::object::PropertyDescriptor;
use crate::string::JsString;
use crate::{Value, VmError, VmPropertyKey};

pub(crate) fn descriptor_for_key(
    value: JsString,
    key: &VmPropertyKey,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<Option<PropertyDescriptor>, VmError> {
    let Some(key) = key.string_name() else {
        return Ok(None);
    };
    descriptor_for_name(value, key, gc_heap)
}

pub(crate) fn descriptor_for_name(
    value: JsString,
    key: &str,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<Option<PropertyDescriptor>, VmError> {
    if key == "length" {
        return Ok(Some(PropertyDescriptor::data(
            Value::number_i32(value.len() as i32),
            false,
            false,
            false,
        )));
    }
    let Ok(index) = key.parse::<u32>() else {
        return Ok(None);
    };
    let Some(unit) = value.char_code_at(index, gc_heap) else {
        return Ok(None);
    };
    Ok(Some(PropertyDescriptor::data(
        Value::string(JsString::from_utf16_units(&[unit], gc_heap)?),
        false,
        true,
        false,
    )))
}
