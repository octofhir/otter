//! Rooted Math argument conversion across user-defined primitive coercion.
//!
//! # Contents
//! - Each method selects only the arguments its specification consumes.
//! - Primitive-only arguments avoid the handle scope and JavaScript reentry.
//! - Pending object arguments live in one native handle scope.
//!
//! # Invariants
//! - Every pending argument is rooted before the first JavaScript reentry.
//! - Each primitive becomes a non-GC Number before the next argument is coerced.
//! - A Symbol/BigInt TypeError prevents all later argument conversions.
//! - User-thrown values preserve their identity through native error conversion.
//!
//! # See also
//! - `super::native_call` and `super::native_f16round`.
//! - `crate::NativeCtx::scope` — the shared moving-GC handle contract.

use super::MathError;
use crate::{Local, NativeCtx, NativeError, Value, number::NumberValue};
use otter_bytecode::method_id::MathMethod;
use smallvec::SmallVec;

pub(super) fn arguments_for(method: MathMethod, args: &[Value]) -> &[Value] {
    use MathMethod::*;
    let count = match method {
        Max | Min | Hypot => args.len(),
        Atan2 | Pow | Imul => 2,
        Random => 0,
        Abs | Acos | Acosh | Asin | Asinh | Atan | Atanh | Cbrt | Ceil | Clz32 | Cos | Cosh
        | Exp | Expm1 | Floor | Fround | Log | Log10 | Log1p | Log2 | Round | Sign | Sin | Sinh
        | Sqrt | Tan | Tanh | Trunc => 1,
    };
    &args[..args.len().min(count)]
}

pub(super) fn coerce_math_args(
    ctx: &mut NativeCtx<'_>,
    name: &'static str,
    args: &[Value],
) -> Result<Vec<NumberValue>, NativeError> {
    if !args.iter().any(needs_to_primitive) {
        return coerce_all(name, args, ctx.heap()).map_err(native_error);
    }
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "Math",
            reason: "missing execution context".to_string(),
        })?;
    ctx.scope(|mut scope| {
        let rooted: SmallVec<[Local<'_>; 4]> =
            args.iter().map(|&value| scope.value(value)).collect();
        let mut numbers = Vec::with_capacity(args.len());
        for (index, argument) in rooted.into_iter().enumerate() {
            let value = scope.raw(argument);
            let primitive = if needs_to_primitive(&value) {
                let result = scope.with_turn_parts(|interp, stack| {
                    interp.evaluate_to_primitive(
                        stack,
                        &exec,
                        &value,
                        crate::abstract_ops::ToPrimitiveHint::Number,
                    )
                });
                match result {
                    Ok(primitive) => primitive,
                    Err(error) => {
                        return Err(crate::native_function::vm_to_native_error(
                            scope.context().interp_mut(),
                            error,
                            "Math",
                        ));
                    }
                }
            } else {
                value
            };
            numbers.push(
                number_argument(name, index, primitive, scope.context().heap())
                    .map_err(native_error)?,
            );
        }
        Ok(numbers)
    })
}

pub(super) fn coerce_all(
    name: &'static str,
    args: &[Value],
    heap: &otter_gc::GcHeap,
) -> Result<Vec<NumberValue>, MathError> {
    args.iter()
        .enumerate()
        .map(|(index, &value)| number_argument(name, index, value, heap))
        .collect()
}

fn number_argument(
    name: &'static str,
    index: usize,
    value: Value,
    heap: &otter_gc::GcHeap,
) -> Result<NumberValue, MathError> {
    if let Some(number) = value.as_number() {
        Ok(number)
    } else if let Some(boolean) = value.as_boolean() {
        Ok(NumberValue::Smi(i32::from(boolean)))
    } else if value.is_null() {
        Ok(NumberValue::Smi(0))
    } else if value.is_undefined() {
        Ok(NumberValue::Double(f64::NAN))
    } else if let Some(string) = value.as_string(heap) {
        Ok(crate::number::parse::to_number_from_string(
            &string.to_lossy_string(heap),
        ))
    } else if value.is_big_int() || value.is_symbol() {
        Err(MathError::BadArgument {
            name,
            index: index as u16,
            reason: if value.is_big_int() {
                "cannot convert a BigInt to a number"
            } else {
                "cannot convert a Symbol to a number"
            },
        })
    } else {
        // The non-reentrant numeric dispatcher also accepts unprepared values;
        // native entry performs object ToPrimitive before reaching this case.
        Ok(NumberValue::Double(f64::NAN))
    }
}

fn native_error(error: MathError) -> NativeError {
    match error {
        MathError::BadArgument { name, reason, .. } => NativeError::TypeError {
            name,
            reason: reason.to_string(),
        },
        MathError::UnknownMember(member) => NativeError::TypeError {
            name: "Math",
            reason: format!("unknown Math member {member}"),
        },
    }
}

fn needs_to_primitive(value: &Value) -> bool {
    !crate::abstract_ops::is_primitive(value)
}
