//! ECMAScript primitive conversion helpers.
//!
//! Keep VM conversion semantics in one place instead of scattering local
//! `ToNumber` / `ToString` fragments through opcode dispatch, builtins, and
//! arithmetic helpers.
//!
//! # Contents
//! - Primitive `ToNumber` for opcode and builtin tails.
//! - Primitive `ToString` helpers for string concatenation and `String(...)`.
//! - Register entrypoints used by dense VM dispatch.
//!
//! # Invariants
//! - Object `ToPrimitive` dispatch is driven before these primitive tails.
//! - `ToString(Symbol)` is an error, while bare `String(symbol)` returns the
//!   symbol descriptive form per §22.1.1.1.
//!
//! # See also
//! - [`crate::abstract_ops`]
//! - [`crate::number`]
//! - [`crate::string_dispatch`]

use crate::{
    Frame, Interpreter, JsString, NumberValue, Value, VmError, number, read_register,
    string::StringHeap, write_register,
};

pub(crate) fn to_number_primitive(value: &Value) -> Result<NumberValue, VmError> {
    let number = match value {
        Value::Number(n) => *n,
        Value::Boolean(true) => NumberValue::Smi(1),
        Value::Boolean(false) | Value::Null => NumberValue::Smi(0),
        Value::BigInt(_) | Value::Symbol(_) => return Err(VmError::TypeMismatch),
        Value::Undefined
        | Value::Hole
        | Value::Function { .. }
        | Value::Closure { .. }
        | Value::BoundFunction(_)
        | Value::NativeFunction(_)
        | Value::Object(_)
        | Value::Array(_)
        | Value::Iterator(_)
        | Value::RegExp(_)
        | Value::Promise(_)
        | Value::ClassConstructor(_)
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
        | Value::Proxy(_) => NumberValue::Double(f64::NAN),
        Value::Date(d) => NumberValue::from_f64(d.time()),
        Value::String(s) => number::to_number_from_string(&s.to_lossy_string()),
    };
    Ok(number)
}

pub(crate) fn to_string_primitive(value: &Value) -> Result<String, VmError> {
    match value {
        Value::String(s) => Ok(s.to_lossy_string()),
        Value::Number(n) => Ok(n.to_display_string()),
        Value::BigInt(b) => Ok(b.to_decimal_string()),
        Value::Boolean(true) => Ok("true".to_string()),
        Value::Boolean(false) => Ok("false".to_string()),
        Value::Null => Ok("null".to_string()),
        Value::Undefined | Value::Hole => Ok("undefined".to_string()),
        Value::Symbol(_) => Err(VmError::TypeMismatch),
        _ => Err(VmError::TypeMismatch),
    }
}

pub(crate) fn to_js_string_primitive(
    value: &Value,
    heap: &StringHeap,
) -> Result<JsString, VmError> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => {
            number::ecma::number_to_string(n.as_f64(), heap).map_err(|_| VmError::TypeMismatch)
        }
        _ => JsString::from_str(&to_string_primitive(value)?, heap)
            .map_err(|_| VmError::TypeMismatch),
    }
}

pub(crate) fn string_constructor_js_string(
    value: Option<&Value>,
    heap: &StringHeap,
) -> Result<JsString, VmError> {
    match value {
        Some(Value::Symbol(s)) => {
            JsString::from_str(&s.descriptive_string(), heap).map_err(|_| VmError::TypeMismatch)
        }
        Some(value) => match to_js_string_primitive(value, heap) {
            Ok(value) => Ok(value),
            Err(VmError::TypeMismatch) => {
                JsString::from_str(&value.display_string(), heap).map_err(|_| VmError::TypeMismatch)
            }
            Err(err) => Err(err),
        },
        None => JsString::empty(heap).map_err(|_| VmError::TypeMismatch),
    }
}

impl Interpreter {
    pub(crate) fn run_to_number_regs(
        &self,
        frame: &mut Frame,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let value = to_number_primitive(read_register(frame, src)?)?;
        write_register(frame, dst, Value::Number(value))?;
        frame.pc += 1;
        Ok(())
    }
}
