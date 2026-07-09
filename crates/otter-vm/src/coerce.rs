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
//! - [`to_js_string_or_throw`] — §7.1.17 `ToString(argument)` returning
//!   a [`crate::string::JsString`] without losing WTF-16 code units when
//!   the primitive result is already a JavaScript string.
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
use crate::string::JsString;
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
    primitive_to_string_lossy(interp, &primitive)
}

/// §7.1.17 `ToString` restricted to primitive operands — the re-entry-free
/// tail of [`to_string_or_throw`]. Symbol operands surface as
/// `VmError::TypeError`; the returned `String` is the lossy Rust rendering
/// (lone surrogates become U+FFFD).
pub(crate) fn primitive_to_string_lossy(
    interp: &Interpreter,
    primitive: &Value,
) -> Result<String, VmError> {
    if primitive.is_symbol() {
        return Err(
            interp.err_type(("Cannot convert a Symbol value to a string".to_string()).into())
        );
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

/// §7.1.17 `ToString` returning WTF-16 code units (lone surrogates
/// preserved). Non-primitive operands require the execution context for
/// the `ToPrimitive` re-entry; a context-free call on an object reports
/// a TypeError instead of guessing.
pub(crate) fn to_js_string_units(
    interp: &mut Interpreter,
    context: Option<&ExecutionContext>,
    input: &Value,
) -> Result<Vec<u16>, VmError> {
    let primitive = if abstract_ops::is_primitive(input) {
        *input
    } else if let Some(context) = context {
        interp.evaluate_to_primitive(context, input, ToPrimitiveHint::String)?
    } else {
        return Err(interp.err_type(
            ("cannot coerce an object to a string without an execution context".to_string()).into(),
        ));
    };
    if primitive.is_symbol() {
        return Err(
            interp.err_type(("Cannot convert a Symbol value to a string".to_string()).into())
        );
    }
    if let Some(s) = primitive.as_string(&interp.gc_heap) {
        return Ok(s.with_utf16(&interp.gc_heap, <[u16]>::to_vec));
    }
    let rendered = primitive_to_string_lossy(interp, &primitive)?;
    Ok(rendered.encode_utf16().collect())
}

/// §7.1.17 `ToString(argument)` returning a GC-backed JavaScript
/// string. Unlike [`to_string_or_throw`], this preserves WTF-16 code
/// units when the primitive result is already a string, including lone
/// surrogates.
pub(crate) fn to_js_string_or_throw(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    input: &Value,
) -> Result<JsString, VmError> {
    let primitive = if abstract_ops::is_primitive(input) {
        *input
    } else {
        interp.evaluate_to_primitive(context, input, ToPrimitiveHint::String)?
    };
    if primitive.is_symbol() {
        return Err(
            interp.err_type(("Cannot convert a Symbol value to a string".to_string()).into())
        );
    }
    if let Some(s) = primitive.as_string(&interp.gc_heap) {
        return Ok(s);
    }
    let rendered = if primitive.is_undefined() {
        "undefined".to_string()
    } else if primitive.is_null() {
        "null".to_string()
    } else if let Some(b) = primitive.as_boolean() {
        if b { "true" } else { "false" }.to_string()
    } else if let Some(n) = primitive.as_number() {
        n.to_display_string()
    } else if let Some(b) = primitive.as_big_int() {
        b.to_decimal_string(&interp.gc_heap)
    } else {
        primitive.display_string(&interp.gc_heap)
    };
    JsString::from_str(&rendered, interp.gc_heap_mut()).map_err(Into::into)
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
    primitive_to_number(interp, &primitive)
}

/// §7.1.4 `ToNumber` restricted to primitive operands — the
/// re-entry-free tail of [`to_number_or_throw`]. Symbol and BigInt
/// operands surface as `VmError::TypeError`.
pub(crate) fn primitive_to_number(
    interp: &Interpreter,
    primitive: &Value,
) -> Result<NumberValue, VmError> {
    if primitive.is_symbol() {
        return Err(
            interp.err_type(("Cannot convert a Symbol value to a number".to_string()).into())
        );
    }
    if primitive.is_big_int() {
        return Err(
            interp.err_type(("Cannot convert a BigInt value to a number".to_string()).into())
        );
    }
    Ok(NumberValue::from_f64(
        crate::number::parse::to_number_value(primitive, &interp.gc_heap),
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
        return Err(
            interp.err_type(("Cannot convert a Symbol value to a number".to_string()).into())
        );
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
        return Err(
            interp.err_type(("Cannot convert a Symbol value to a number".to_string()).into())
        );
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
/// §7.1.20 ToLength — ToIntegerOrInfinity clamped to
/// [0, 2^53 - 1], with full user coercion (valueOf / @@toPrimitive)
/// and abrupt completions propagated.
pub(crate) fn to_length_or_throw(
    interp: &mut crate::Interpreter,
    context: &crate::ExecutionContext,
    value: &crate::Value,
) -> Result<usize, crate::VmError> {
    let number = to_number_or_throw(interp, context, value)?;
    let n = number.as_f64();
    if n.is_nan() || n <= 0.0 {
        return Ok(0);
    }
    Ok(n.trunc().min(9_007_199_254_740_991.0) as usize)
}

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
        let parsed = abstract_ops::string_to_big_int(&text).ok_or_else(|| {
            interp.err_syntax((format!("Cannot convert {text:?} to a BigInt")).into())
        })?;
        return BigIntValue::from_inner(&mut interp.gc_heap, parsed).map_err(crate::oom_to_vm);
    }
    if primitive.is_number() {
        return Err(interp.err_type(("Cannot convert a Number to a BigInt".to_string()).into()));
    }
    if primitive.is_symbol() {
        return Err(
            interp.err_type(("Cannot convert a Symbol value to a BigInt".to_string()).into())
        );
    }
    Err(interp.err_type(("Cannot convert value to a BigInt".to_string()).into()))
}
