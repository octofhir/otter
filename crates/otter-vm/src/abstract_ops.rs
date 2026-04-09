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
/// <https://tc39.es/ecma262/#sec-isstrictlyequal>
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

    // §7.2.14 step 2: If Type(x) is BigInt, return BigInt::equal(x, y).
    if let Some(result) = compare_bigint_primitives(heap, lhs, rhs)? {
        return Ok(result);
    }

    compare_string_primitives(heap, lhs, rhs).map(|result| result.unwrap_or(false))
}

/// ES2024 §7.2.9 SameValue(x, y).
/// <https://tc39.es/ecma262/#sec-samevalue>
pub fn same_value(
    heap: &ObjectHeap,
    lhs: RegisterValue,
    rhs: RegisterValue,
) -> Result<bool, ObjectError> {
    if let (Some(lhs_num), Some(rhs_num)) = (lhs.as_number(), rhs.as_number()) {
        return Ok(number_same_value(lhs_num, rhs_num));
    }

    if let Some(result) = compare_bigint_primitives(heap, lhs, rhs)? {
        return Ok(result);
    }

    if let Some(result) = compare_string_primitives(heap, lhs, rhs)? {
        return Ok(result);
    }

    Ok(lhs == rhs)
}

/// ES2024 §7.2.10 SameValueZero(x, y).
/// <https://tc39.es/ecma262/#sec-samevaluezero>
pub fn same_value_zero(
    heap: &ObjectHeap,
    lhs: RegisterValue,
    rhs: RegisterValue,
) -> Result<bool, ObjectError> {
    if let (Some(lhs_num), Some(rhs_num)) = (lhs.as_number(), rhs.as_number()) {
        return Ok(number_same_value_zero(lhs_num, rhs_num));
    }

    if let Some(result) = compare_bigint_primitives(heap, lhs, rhs)? {
        return Ok(result);
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
    let primitive = runtime
        .js_to_primitive_with_hint(value, crate::interpreter::ToPrimitiveHint::String)
        .map_err(|error| interpreter_error_to_thrown(runtime, error, "ToPropertyKey"))?;

    if let Some(symbol_id) = primitive.as_symbol_id() {
        return Ok(runtime.intern_symbol_property_name(symbol_id));
    }

    let key = to_property_key_string(runtime, primitive)?;
    Ok(runtime.intern_property_name(&key))
}

fn to_property_key_string(
    runtime: &mut RuntimeState,
    value: RegisterValue,
) -> Result<Box<str>, VmNativeCallError> {
    runtime
        .js_to_string(value)
        .map_err(|error| interpreter_error_to_thrown(runtime, error, "ToPropertyKey"))
}

/// Translate an `InterpreterError` produced during a ToPrimitive/ToString
/// step into a JS-visible `VmNativeCallError::Thrown`. Spec abstract ops
/// (ToPropertyKey, ToString, ToPrimitive) can raise a caller-catchable
/// TypeError; preserving that error as `Internal` would instead surface it
/// as an internal "native host call failed" message that cannot be caught
/// from user code.
fn interpreter_error_to_thrown(
    runtime: &mut crate::interpreter::RuntimeState,
    error: crate::interpreter::InterpreterError,
    op: &str,
) -> VmNativeCallError {
    use crate::interpreter::InterpreterError;
    match error {
        InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
        InterpreterError::TypeError(message) => match runtime.alloc_type_error(&message) {
            Ok(handle) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0)),
            Err(_) => VmNativeCallError::Internal(
                format!("{op}: failed to allocate TypeError: {message}").into(),
            ),
        },
        other => VmNativeCallError::Internal(format!("{op}: {other}").into()),
    }
}

/// Compares two BigInt values by their decimal string representation.
///
/// Returns `Some(true/false)` when both operands are BigInt, `None` otherwise.
/// §6.1.6.2.13 BigInt::equal(x, y)
/// <https://tc39.es/ecma262/#sec-numeric-types-bigint-equal>
fn compare_bigint_primitives(
    heap: &ObjectHeap,
    lhs: RegisterValue,
    rhs: RegisterValue,
) -> Result<Option<bool>, ObjectError> {
    let Some(lhs_handle) = lhs.as_bigint_handle() else {
        return Ok(None);
    };
    let Some(rhs_handle) = rhs.as_bigint_handle() else {
        return Ok(None);
    };

    let Some(lhs_value) = heap.bigint_value(ObjectHandle(lhs_handle))? else {
        return Ok(None);
    };
    let Some(rhs_value) = heap.bigint_value(ObjectHandle(rhs_handle))? else {
        return Ok(None);
    };

    Ok(Some(lhs_value == rhs_value))
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
