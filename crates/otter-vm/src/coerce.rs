//! Spec-aware operand coercion helpers.
//!
//! This module is the single home for the §7.1.x ToPrimitive /
//! ToString / ToNumber ladders that every constructor / intrinsic
//! invokes when its argument is not already a primitive. The plain
//! primitive-only counterparts live in `crate::conversion` for the
//! hot opcode-dispatch paths that cannot reach into the
//! interpreter's `ExecutionContext` (and therefore cannot fire user
//! `@@toPrimitive` / `valueOf` / `toString` overrides).
//!
//! # Contents
//! - [`to_primitive_or_throw`] — §7.1.1 `ToPrimitive(input, hint)`.
//! - [`to_string_or_throw`] — §7.1.17 `ToString(argument)` returning
//!   the lossy Rust `String` (every caller pushes it through
//!   `JsString::from_str` anyway, so the boxed step happens at the
//!   call site once the appropriate `StringHeap` is in scope).
//! - [`to_number_or_throw`] — §7.1.4 `ToNumber(argument)`.
//! - [`to_index_or_throw`] — §7.1.22 `ToIndex(value)`.
//!
//! # Invariants
//! - Symbol operands surface as `VmError::TypeError` per §7.1.17
//!   step 2 / §7.1.4 step 2 / §7.1.22 step 2 — exactly the spec
//!   error class so callers do not have to remap.
//! - BigInt operands going through `ToNumber` raise `TypeError`
//!   (§7.1.4 step 4). Number-constructor callers that need the
//!   `Number(bigint)` exception path of §21.1.1.1 step 5 handle
//!   the BigInt case before invoking these helpers.
//! - Object operands re-enter `Interpreter::evaluate_to_primitive`
//!   so `@@toPrimitive` / `valueOf` / `toString` overrides fire
//!   exactly once.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-toprimitive>
//! - <https://tc39.es/ecma262/#sec-tostring>
//! - <https://tc39.es/ecma262/#sec-tonumber>
//! - <https://tc39.es/ecma262/#sec-toindex>

use crate::abstract_ops::{self, ToPrimitiveHint};
use crate::bigint::BigIntValue;
use crate::number::NumberValue;
use crate::{ExecutionContext, Interpreter, Value, VmError};

impl Interpreter {
    /// §7.1.17 ToString shortcut — see [`to_string_or_throw`].
    pub(crate) fn coerce_to_string(
        &mut self,
        context: &ExecutionContext,
        input: &Value,
    ) -> Result<String, VmError> {
        to_string_or_throw(self, context, input)
    }

    /// §7.1.4 ToNumber shortcut — see [`to_number_or_throw`].
    #[allow(dead_code)]
    pub(crate) fn coerce_to_number(
        &mut self,
        context: &ExecutionContext,
        input: &Value,
    ) -> Result<NumberValue, VmError> {
        to_number_or_throw(self, context, input)
    }

    /// §21.1.1.1 `Number(value)` coercion (BigInt → f64, Symbol →
    /// TypeError, Object → ToPrimitive(number) ladder).
    pub(crate) fn number_for_number_ctor(
        &mut self,
        context: &ExecutionContext,
        input: &Value,
    ) -> Result<NumberValue, VmError> {
        to_number_for_number_ctor(self, context, input)
    }

    /// §7.1.1 ToPrimitive shortcut — see [`to_primitive_or_throw`].
    #[allow(dead_code)]
    pub(crate) fn coerce_to_primitive(
        &mut self,
        context: &ExecutionContext,
        input: &Value,
        hint: ToPrimitiveHint,
    ) -> Result<Value, VmError> {
        to_primitive_or_throw(self, context, input, hint)
    }
}

/// §7.1.1 `ToPrimitive(input, hint)`. Returns `input` unchanged when
/// it is already a primitive; otherwise dispatches through the
/// `@@toPrimitive` / `OrdinaryToPrimitive` ladder.
pub(crate) fn to_primitive_or_throw(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    input: &Value,
    hint: ToPrimitiveHint,
) -> Result<Value, VmError> {
    if abstract_ops::is_primitive(input) {
        return Ok(*input);
    }
    interp.evaluate_to_primitive(context, input, hint)
}

/// §7.1.17 `ToString(argument)`. Symbol operands surface as
/// `VmError::TypeError`; non-primitive operands flow through
/// `ToPrimitive(argument, "string")` first.
///
/// The returned `String` is the lossy Rust rendering — callers that
/// need a [`crate::JsString`] should call `JsString::from_str(&s,
/// string_heap)` after this returns, with the appropriate
/// `StringHeap` in scope.
pub(crate) fn to_string_or_throw(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    input: &Value,
) -> Result<String, VmError> {
    let primitive = if abstract_ops::is_primitive(input) {
        *input
    } else {
        interp.evaluate_to_primitive(context, input, ToPrimitiveHint::String)?
    };
    if primitive.is_symbol() {
        return Err(VmError::TypeError {
            message: "Cannot convert a Symbol value to a string".to_string(),
        });
    }
    if let Some(s) = primitive.as_string(&interp.gc_heap) {
        return Ok(s.to_lossy_string(&interp.gc_heap));
    }
    if primitive.is_undefined() {
        return Ok("undefined".to_string());
    }
    if primitive.is_null() {
        return Ok("null".to_string());
    }
    if let Some(b) = primitive.as_boolean() {
        return Ok(if b { "true" } else { "false" }.to_string());
    }
    if let Some(n) = primitive.as_number() {
        return Ok(n.to_display_string());
    }
    if let Some(b) = primitive.as_big_int() {
        return Ok(b.to_decimal_string(&interp.gc_heap));
    }
    Ok(primitive.display_string(&interp.gc_heap))
}

/// §7.1.4 `ToNumber(argument)`. Symbol and BigInt operands surface
/// as `VmError::TypeError`; non-primitive operands flow through
/// `ToPrimitive(argument, "number")` first.
pub(crate) fn to_number_or_throw(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    input: &Value,
) -> Result<NumberValue, VmError> {
    let primitive = if abstract_ops::is_primitive(input) {
        *input
    } else {
        interp.evaluate_to_primitive(context, input, ToPrimitiveHint::Number)?
    };
    if primitive.is_symbol() {
        return Err(VmError::TypeError {
            message: "Cannot convert a Symbol value to a number".to_string(),
        });
    }
    if primitive.is_big_int() {
        return Err(VmError::TypeError {
            message: "Cannot convert a BigInt value to a number".to_string(),
        });
    }
    Ok(NumberValue::from_f64(
        crate::number::parse::to_number_value(&primitive, &interp.gc_heap),
    ))
}

/// §7.1.4 variant used by the `Number(value)` constructor — diverges
/// from generic ToNumber for BigInt operands (§21.1.1.1 step 5
/// converts `Number(bigint)` to the nearest `f64` instead of
/// throwing). Symbol arguments still raise TypeError.
pub(crate) fn to_number_for_number_ctor(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    input: &Value,
) -> Result<NumberValue, VmError> {
    if input.is_symbol() {
        return Err(VmError::TypeError {
            message: "Cannot convert a Symbol value to a number".to_string(),
        });
    }
    if let Some(b) = input.as_big_int() {
        let f = b
            .to_decimal_string(&interp.gc_heap)
            .parse::<f64>()
            .unwrap_or(f64::NAN);
        return Ok(NumberValue::from_f64(f));
    }
    let primitive = if abstract_ops::is_primitive(input) {
        *input
    } else {
        interp.evaluate_to_primitive(context, input, ToPrimitiveHint::Number)?
    };
    if primitive.is_symbol() {
        return Err(VmError::TypeError {
            message: "Cannot convert a Symbol value to a number".to_string(),
        });
    }
    if let Some(b) = primitive.as_big_int() {
        let f = b
            .to_decimal_string(&interp.gc_heap)
            .parse::<f64>()
            .unwrap_or(f64::NAN);
        return Ok(NumberValue::from_f64(f));
    }
    Ok(NumberValue::from_f64(
        crate::number::parse::to_number_value(&primitive, &interp.gc_heap),
    ))
}

/// §7.1.13 `StringToBigInt` — accessor-aware variant. Object operands
/// flow through `ToPrimitive(argument, "number")` first per
/// §7.1.13.1; the resulting primitive then follows the spec
/// StringToBigInt grammar (`abstract_ops::string_to_big_int`).
///
/// Returns `None` when the string fails the grammar — the spec
/// `BigInt(value)` constructor surfaces that as a SyntaxError /
/// TypeError depending on the source kind, and callers map the
/// outcome accordingly.
#[allow(dead_code)]
pub(crate) fn to_big_int_or_throw(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    input: &Value,
) -> Result<BigIntValue, VmError> {
    let primitive = if abstract_ops::is_primitive(input) {
        *input
    } else {
        interp.evaluate_to_primitive(context, input, ToPrimitiveHint::Number)?
    };
    if let Some(b) = primitive.as_big_int() {
        return Ok(b);
    }
    if let Some(b) = primitive.as_boolean() {
        return BigIntValue::from_i32(&mut interp.gc_heap, if b { 1 } else { 0 })
            .map_err(crate::oom_to_vm);
    }
    if let Some(s) = primitive.as_string(&interp.gc_heap) {
        let text = s.to_lossy_string(&interp.gc_heap);
        let parsed = abstract_ops::string_to_big_int(&text).ok_or(VmError::SyntaxError {
            message: format!("Cannot convert {text:?} to a BigInt"),
        })?;
        return BigIntValue::from_inner(&mut interp.gc_heap, parsed).map_err(crate::oom_to_vm);
    }
    if primitive.is_number() {
        return Err(VmError::TypeError {
            message: "Cannot convert a Number to a BigInt".to_string(),
        });
    }
    if primitive.is_symbol() {
        return Err(VmError::TypeError {
            message: "Cannot convert a Symbol value to a BigInt".to_string(),
        });
    }
    Err(VmError::TypeError {
        message: "Cannot convert value to a BigInt".to_string(),
    })
}
