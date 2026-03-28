//! ECMAScript abstract operations shared by the new VM.
//!
//! The goal of this module is to keep spec-level helpers centralized instead of
//! re-implementing small semantic fragments inside the interpreter and builtin
//! modules.

mod property_descriptor;

use crate::descriptors::VmNativeCallError;
use crate::interpreter::RuntimeState;
use crate::object::{ObjectError, ObjectHandle, ObjectHeap};
use crate::property::PropertyNameId;
use crate::value::RegisterValue;

pub(crate) use property_descriptor::{
    collect_define_properties, from_property_descriptor, to_property_descriptor,
};

/// ES2024 §7.2.14 IsStrictlyEqual(x, y).
pub fn is_strictly_equal(
    heap: &ObjectHeap,
    lhs: RegisterValue,
    rhs: RegisterValue,
) -> Result<bool, ObjectError> {
    if let (Some(lhs_num), Some(rhs_num)) = (lhs.as_number(), rhs.as_number()) {
        if lhs_num.is_nan() || rhs_num.is_nan() {
            return Ok(false);
        }
        return Ok(lhs_num == rhs_num);
    }

    if lhs == rhs {
        return Ok(true);
    }

    compare_string_primitives(heap, lhs, rhs).map(|result| result.unwrap_or(false))
}

/// ES2024 §7.2.9 SameValue(x, y).
pub fn same_value(
    heap: &ObjectHeap,
    lhs: RegisterValue,
    rhs: RegisterValue,
) -> Result<bool, ObjectError> {
    if let (Some(lhs_num), Some(rhs_num)) = (lhs.as_number(), rhs.as_number()) {
        return Ok(number_same_value(lhs_num, rhs_num));
    }

    if let Some(result) = compare_string_primitives(heap, lhs, rhs)? {
        return Ok(result);
    }

    Ok(lhs == rhs)
}

/// ES2024 §7.2.10 SameValueZero(x, y).
pub fn same_value_zero(
    heap: &ObjectHeap,
    lhs: RegisterValue,
    rhs: RegisterValue,
) -> Result<bool, ObjectError> {
    if let (Some(lhs_num), Some(rhs_num)) = (lhs.as_number(), rhs.as_number()) {
        return Ok(number_same_value_zero(lhs_num, rhs_num));
    }

    if let Some(result) = compare_string_primitives(heap, lhs, rhs)? {
        return Ok(result);
    }

    Ok(lhs == rhs)
}

/// ES2024 §7.1.19 ToPropertyKey(argument), string-only subset for the new VM.
pub fn to_property_key(
    runtime: &mut RuntimeState,
    value: RegisterValue,
) -> Result<PropertyNameId, VmNativeCallError> {
    let key = to_property_key_string(runtime, value)?;
    Ok(runtime.intern_property_name(&key))
}

fn to_property_key_string(
    runtime: &mut RuntimeState,
    value: RegisterValue,
) -> Result<Box<str>, VmNativeCallError> {
    if let Some(handle) = value.as_object_handle().map(ObjectHandle)
        && let Some(primitive) = runtime.boxed_primitive_value(handle).map_err(|error| {
            VmNativeCallError::Internal(
                format!("ToPropertyKey boxed primitive lookup failed: {error}").into(),
            )
        })?
    {
        return to_property_key_string(runtime, primitive);
    }

    runtime.js_to_string(value).map_err(|error| {
        VmNativeCallError::Internal(format!("ToPropertyKey string coercion failed: {error}").into())
    })
}

fn compare_string_primitives(
    heap: &ObjectHeap,
    lhs: RegisterValue,
    rhs: RegisterValue,
) -> Result<Option<bool>, ObjectError> {
    let Some(lhs_handle) = lhs.as_object_handle().map(ObjectHandle) else {
        return Ok(None);
    };
    let Some(rhs_handle) = rhs.as_object_handle().map(ObjectHandle) else {
        return Ok(None);
    };

    let Some(lhs_string) = heap.string_value(lhs_handle)? else {
        return Ok(None);
    };
    let Some(rhs_string) = heap.string_value(rhs_handle)? else {
        return Ok(None);
    };

    Ok(Some(lhs_string == rhs_string))
}

fn number_same_value(lhs: f64, rhs: f64) -> bool {
    if lhs.is_nan() && rhs.is_nan() {
        return true;
    }
    if lhs == 0.0 && rhs == 0.0 {
        return lhs.is_sign_positive() == rhs.is_sign_positive();
    }
    lhs == rhs
}

fn number_same_value_zero(lhs: f64, rhs: f64) -> bool {
    if lhs.is_nan() && rhs.is_nan() {
        return true;
    }
    lhs == rhs
}

#[cfg(test)]
mod tests {
    use crate::object::ObjectHeap;
    use crate::value::RegisterValue;

    use super::{is_strictly_equal, same_value, same_value_zero};

    #[test]
    fn same_value_treats_nan_as_equal() {
        let heap = ObjectHeap::new();
        assert_eq!(
            same_value(
                &heap,
                RegisterValue::from_number(f64::NAN),
                RegisterValue::from_number(f64::NAN),
            ),
            Ok(true)
        );
    }

    #[test]
    fn same_value_distinguishes_signed_zero() {
        let heap = ObjectHeap::new();
        assert_eq!(
            same_value(
                &heap,
                RegisterValue::from_number(0.0),
                RegisterValue::from_number(-0.0),
            ),
            Ok(false)
        );
    }

    #[test]
    fn same_value_zero_merges_signed_zero() {
        let heap = ObjectHeap::new();
        assert_eq!(
            same_value_zero(
                &heap,
                RegisterValue::from_number(0.0),
                RegisterValue::from_number(-0.0),
            ),
            Ok(true)
        );
    }

    #[test]
    fn same_value_compares_string_primitive_contents() {
        let mut heap = ObjectHeap::new();
        let lhs = RegisterValue::from_object_handle(heap.alloc_string("otter").0);
        let rhs = RegisterValue::from_object_handle(heap.alloc_string("otter").0);
        let other = RegisterValue::from_object_handle(heap.alloc_string("vm").0);

        assert_eq!(same_value(&heap, lhs, rhs), Ok(true));
        assert_eq!(same_value(&heap, lhs, other), Ok(false));
    }

    #[test]
    fn strict_equality_keeps_nan_unequal() {
        let heap = ObjectHeap::new();
        assert_eq!(
            is_strictly_equal(
                &heap,
                RegisterValue::from_number(f64::NAN),
                RegisterValue::from_number(f64::NAN),
            ),
            Ok(false)
        );
    }
}
