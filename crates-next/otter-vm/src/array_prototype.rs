//! `Array.prototype.*` non-callback intrinsic implementations.
//!
//! Slice 21 ships methods that do **not** invoke a JS callback:
//! `push`, `pop`, `shift`, `unshift`, `slice`, `concat`, `join`,
//! `includes`, `indexOf`. The callback-driven family
//! (`forEach`, `map`, `filter`, `reduce`) lowers to inline bytecode
//! loops in the compiler so we don't need a host-callable bridge
//! for the runtime.
//!
//! # Contents
//! - [`ARRAY_PROTOTYPE_TABLE`] — declarative registry built with
//!   the `intrinsics!` macro.
//! - One private `impl_*` function per method.
//!
//! # Invariants
//! - Receivers must be `Value::Array`; non-arrays raise
//!   `IntrinsicError::BadReceiver`.
//! - Spec-mandated argument coercion (e.g., `slice` clamping
//!   negatives) follows the foundation subset; rare edge cases
//!   are documented inline.

use crate::Value;
use crate::array::JsArray;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::number::NumberValue;
use crate::string::JsString;

fn receiver_array<'a>(args: &'a IntrinsicArgs<'_>) -> Result<&'a JsArray, IntrinsicError> {
    match args.receiver {
        Value::Array(a) => Ok(a),
        _ => Err(IntrinsicError::BadReceiver { expected: "array" }),
    }
}

/// Convert a possibly-negative numeric index into an absolute
/// element index, clamped to `[0, len]`. Mirrors the spec's
/// `ToIntegerOrInfinity` + clamping rule for `slice` / `indexOf`.
fn clamp_index(raw: i64, len: usize) -> usize {
    if raw < 0 {
        let from_end = len as i64 + raw;
        if from_end < 0 { 0 } else { from_end as usize }
    } else if (raw as usize) > len {
        len
    } else {
        raw as usize
    }
}

fn arg_signed_index(
    args: &IntrinsicArgs<'_>,
    index: u16,
    default: i64,
) -> Result<i64, IntrinsicError> {
    match args.args.get(index as usize) {
        None => Ok(default),
        Some(Value::Number(n)) => match n.as_smi() {
            Some(v) => Ok(v as i64),
            None => Ok(n.as_f64() as i64),
        },
        Some(_) => Err(IntrinsicError::BadArgument {
            index,
            reason: "must be a number",
        }),
    }
}

fn impl_push(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let mut new_len = arr.len();
    for v in args.args {
        new_len = arr.push(v.clone());
    }
    Ok(Value::Number(NumberValue::from_i32(new_len as i32)))
}

fn impl_pop(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    Ok(arr.pop())
}

fn impl_shift(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let mut body = arr.borrow_body_mut();
    if body.elements.is_empty() {
        return Ok(Value::Undefined);
    }
    Ok(body.elements.remove(0))
}

fn impl_unshift(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let mut body = arr.borrow_body_mut();
    for (i, v) in args.args.iter().enumerate() {
        body.elements.insert(i, v.clone());
    }
    Ok(Value::Number(NumberValue::from_i32(
        body.elements.len() as i32
    )))
}

fn impl_slice(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let len = arr.len();
    let start = clamp_index(arg_signed_index(args, 0, 0)?, len);
    let end_default = len as i64;
    let end_raw = arg_signed_index(args, 1, end_default)?;
    let end = clamp_index(end_raw, len);
    let body = arr.borrow_body();
    let slice: Vec<Value> = if start >= end {
        Vec::new()
    } else {
        body.elements[start..end].to_vec()
    };
    Ok(Value::Array(JsArray::from_elements(slice)))
}

fn impl_concat(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    // Spec: result starts as a copy of the receiver; for each
    // argument, if it's an array, append its elements; otherwise
    // append the value itself.
    let mut combined: Vec<Value> = arr.borrow_body().iter().cloned().collect();
    for v in args.args {
        match v {
            Value::Array(other) => {
                for el in other.borrow_body().iter() {
                    combined.push(el.clone());
                }
            }
            other => combined.push(other.clone()),
        }
    }
    Ok(Value::Array(JsArray::from_elements(combined)))
}

fn impl_join(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let separator = match args.args.first() {
        None | Some(Value::Undefined) => ",".to_string(),
        Some(Value::String(s)) => s.to_lossy_string(),
        Some(other) => other.display_string(),
    };
    let body = arr.borrow_body();
    let parts: Vec<String> = body
        .iter()
        .map(|v| match v {
            Value::Undefined | Value::Null => String::new(),
            other => other.display_string(),
        })
        .collect();
    let joined = parts.join(&separator);
    Ok(Value::String(JsString::from_str(
        &joined,
        args.string_heap,
    )?))
}

fn impl_includes(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let needle = args.args.first().cloned().unwrap_or(Value::Undefined);
    let body = arr.borrow_body();
    let found = body.iter().any(|v| v == &needle);
    Ok(Value::Boolean(found))
}

fn impl_index_of(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let needle = args.args.first().cloned().unwrap_or(Value::Undefined);
    let from_raw = arg_signed_index(args, 1, 0)?;
    let len = arr.len();
    let from = clamp_index(from_raw, len);
    let body = arr.borrow_body();
    for (i, v) in body.iter().enumerate().skip(from) {
        if v == &needle {
            return Ok(Value::Number(NumberValue::from_i32(i as i32)));
        }
    }
    Ok(Value::Number(NumberValue::from_i32(-1)))
}

/// Declarative `Array.prototype` table.
pub static ARRAY_PROTOTYPE_TABLE: std::sync::LazyLock<IntrinsicTable> =
    std::sync::LazyLock::new(|| {
        crate::intrinsics!(
            Array,
            "push"     / 1 => impl_push,
            "pop"      / 0 => impl_pop,
            "shift"    / 0 => impl_shift,
            "unshift"  / 1 => impl_unshift,
            "slice"    / 2 => impl_slice,
            "concat"   / 1 => impl_concat,
            "join"     / 1 => impl_join,
            "includes" / 1 => impl_includes,
            "indexOf"  / 1 => impl_index_of,
        )
    });

/// Convenience accessor used by the dispatcher.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    ARRAY_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Array, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::string::StringHeap;

    fn make_arr(values: &[i32]) -> Value {
        let arr = JsArray::from_elements(
            values
                .iter()
                .map(|&n| Value::Number(NumberValue::from_i32(n))),
        );
        Value::Array(arr)
    }

    fn call(method: &str, recv: Value, args: &[Value]) -> Value {
        let heap = StringHeap::default();
        let entry = lookup(method).unwrap();
        (entry.impl_fn)(&IntrinsicArgs {
            receiver: &recv,
            args,
            string_heap: &heap,
        })
        .unwrap()
    }

    #[test]
    fn push_returns_new_length() {
        let arr = make_arr(&[1, 2]);
        let r = call(
            "push",
            arr.clone(),
            &[Value::Number(NumberValue::from_i32(3))],
        );
        assert_eq!(r.display_string(), "3");
        assert_eq!(arr.display_string(), "1,2,3");
    }

    #[test]
    fn pop_yields_tail() {
        let arr = make_arr(&[1, 2, 3]);
        let r = call("pop", arr.clone(), &[]);
        assert_eq!(r.display_string(), "3");
        assert_eq!(arr.display_string(), "1,2");
    }

    #[test]
    fn shift_yields_head() {
        let arr = make_arr(&[10, 20, 30]);
        let r = call("shift", arr.clone(), &[]);
        assert_eq!(r.display_string(), "10");
        assert_eq!(arr.display_string(), "20,30");
    }

    #[test]
    fn slice_handles_negative_end() {
        let arr = make_arr(&[1, 2, 3, 4, 5]);
        let r = call(
            "slice",
            arr,
            &[
                Value::Number(NumberValue::from_i32(1)),
                Value::Number(NumberValue::from_i32(-1)),
            ],
        );
        assert_eq!(r.display_string(), "2,3,4");
    }

    #[test]
    fn concat_flattens_one_level() {
        let arr = make_arr(&[1, 2]);
        let other = make_arr(&[3, 4]);
        let r = call(
            "concat",
            arr,
            &[other, Value::Number(NumberValue::from_i32(5))],
        );
        assert_eq!(r.display_string(), "1,2,3,4,5");
    }

    #[test]
    fn join_with_default_separator() {
        let arr = make_arr(&[1, 2, 3]);
        let r = call("join", arr, &[]);
        assert_eq!(r.display_string(), "1,2,3");
    }

    #[test]
    fn includes_and_index_of() {
        let arr = make_arr(&[10, 20, 30]);
        let yes = call(
            "includes",
            arr.clone(),
            &[Value::Number(NumberValue::from_i32(20))],
        );
        let no = call(
            "includes",
            arr.clone(),
            &[Value::Number(NumberValue::from_i32(99))],
        );
        assert_eq!(yes, Value::Boolean(true));
        assert_eq!(no, Value::Boolean(false));
        let idx = call("indexOf", arr, &[Value::Number(NumberValue::from_i32(30))]);
        assert_eq!(idx.display_string(), "2");
    }
}
