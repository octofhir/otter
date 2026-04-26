//! `String.prototype.*` intrinsic implementations.
//!
//! Slice 10. Every method dispatches through the
//! [`crate::intrinsics`] table so primitive string receivers reach
//! these implementations without allocating a wrapper object
//! (foundation plan rule #2).
//!
//! # Contents
//! - [`STRING_PROTOTYPE_TABLE`] — declarative table built with the
//!   [`crate::intrinsics!`] macro.
//! - One private `impl_*` function per method.
//!
//! # Invariants
//! - Every method validates the receiver as `Value::String`; a non-
//!   string raises [`crate::intrinsics::IntrinsicError::BadReceiver`].
//! - Numeric arguments are accepted as `Value::String` for now (this
//!   slice predates `Value::Number`); the parsing rule mirrors a
//!   tiny subset of `ToInteger`.
//! - `indexOf` polls the runtime interrupt flag every
//!   [`crate::string::INDEX_OF_INTERRUPT_BUDGET`] iterations.
//!
//! # See also
//! - [`docs/new-engine/tasks/10-string-methods-slice.md`](
//!     ../../../docs/new-engine/tasks/10-string-methods-slice.md
//!   )

use crate::Value;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::number::NumberValue;
use crate::string::Interrupted;
use crate::string::JsString;

fn receiver_string<'a>(args: &'a IntrinsicArgs<'_>) -> Result<&'a JsString, IntrinsicError> {
    match args.receiver {
        Value::String(s) => Ok(s),
        _ => Err(IntrinsicError::BadReceiver { expected: "string" }),
    }
}

fn arg_string<'a>(args: &'a IntrinsicArgs<'_>, index: u16) -> Result<&'a JsString, IntrinsicError> {
    match args.args.get(index as usize) {
        Some(Value::String(s)) => Ok(s),
        Some(_) => Err(IntrinsicError::BadArgument {
            index,
            reason: "must be a string",
        }),
        None => Err(IntrinsicError::BadArgument {
            index,
            reason: "is required",
        }),
    }
}

/// Pull a u32 index from arg `index`. Accepts `Value::Number`
/// (clamped to `[0, u32::MAX]`) or, for foundation-era ergonomics,
/// `Value::String` whose body parses as a non-negative decimal
/// integer. Missing arguments collapse to `default`.
fn arg_u32_or(args: &IntrinsicArgs<'_>, index: u16, default: u32) -> Result<u32, IntrinsicError> {
    match args.args.get(index as usize) {
        None => Ok(default),
        Some(Value::Number(n)) => Ok(number_to_u32(*n)),
        Some(Value::String(s)) => parse_index(s).ok_or(IntrinsicError::BadArgument {
            index,
            reason: "must be a non-negative integer",
        }),
        Some(_) => Err(IntrinsicError::BadArgument {
            index,
            reason: "must be a non-negative integer",
        }),
    }
}

fn number_to_u32(n: NumberValue) -> u32 {
    match n.as_smi() {
        Some(v) if v >= 0 => v as u32,
        Some(_) => 0,
        None => {
            let f = n.as_f64();
            if f.is_nan() || f.is_sign_negative() {
                0
            } else if f >= u32::MAX as f64 {
                u32::MAX
            } else {
                f as u32
            }
        }
    }
}

fn parse_index(s: &JsString) -> Option<u32> {
    let text = s.to_lossy_string();
    text.trim().parse::<u32>().ok()
}

fn impl_length(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    Ok(Value::Number(NumberValue::from_i32(recv.len() as i32)))
}

fn impl_char_code_at(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let idx = arg_u32_or(args, 0, 0)?;
    let value = match recv.char_code_at(idx) {
        Some(unit) => NumberValue::from_i32(i32::from(unit)),
        None => NumberValue::Double(f64::NAN),
    };
    Ok(Value::Number(value))
}

fn impl_char_at(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let idx = arg_u32_or(args, 0, 0)?;
    let unit = recv.char_code_at(idx);
    match unit {
        Some(u) => {
            let s = JsString::from_utf16_units(&[u], args.string_heap)?;
            Ok(Value::String(s))
        }
        None => Ok(Value::String(JsString::empty(args.string_heap)?)),
    }
}

fn impl_slice(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let total = recv.len();
    let start = arg_u32_or(args, 0, 0)?.min(total);
    let end = match args.args.get(1) {
        Some(_) => arg_u32_or(args, 1, total)?.min(total),
        None => total,
    };
    let length = end.saturating_sub(start);
    let out = recv.slice(start, length, args.string_heap)?;
    Ok(Value::String(out))
}

fn impl_substring(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let total = recv.len();
    let mut start = arg_u32_or(args, 0, 0)?.min(total);
    let mut end = match args.args.get(1) {
        Some(_) => arg_u32_or(args, 1, total)?.min(total),
        None => total,
    };
    // Spec: if start > end, swap.
    if start > end {
        std::mem::swap(&mut start, &mut end);
    }
    let length = end - start;
    let out = recv.slice(start, length, args.string_heap)?;
    Ok(Value::String(out))
}

fn impl_index_of(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let needle = arg_string(args, 0)?;
    let from = arg_u32_or(args, 1, 0)?;
    let pos =
        recv.index_of(needle, from, None)
            .map_err(|Interrupted| IntrinsicError::BadArgument {
                index: 0,
                reason: "interrupted",
            })?;
    let value = match pos {
        Some(p) => NumberValue::from_i32(p as i32),
        None => NumberValue::from_i32(-1),
    };
    Ok(Value::Number(value))
}

fn impl_starts_with(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let needle = arg_string(args, 0)?;
    let from = arg_u32_or(args, 1, 0)?;
    Ok(Value::Boolean(recv.starts_with(needle, from)))
}

fn impl_ends_with(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let needle = arg_string(args, 0)?;
    let end_pos = arg_u32_or(args, 1, recv.len())?;
    Ok(Value::Boolean(recv.ends_with(needle, end_pos)))
}

/// Declarative `String.prototype` table.
///
/// Slice 10 covers the foundation-spec subset. Slice 11 will retire
/// the string-encoded boolean / numeric outputs once
/// `Value::Boolean` and `Value::Number` exist.
pub static STRING_PROTOTYPE_TABLE: std::sync::LazyLock<IntrinsicTable> =
    std::sync::LazyLock::new(|| {
        crate::intrinsics!(
            String,
            "length"      / 0 => impl_length,
            "charCodeAt"  / 1 => impl_char_code_at,
            "charAt"      / 1 => impl_char_at,
            "slice"       / 2 => impl_slice,
            "substring"   / 2 => impl_substring,
            "indexOf"     / 2 => impl_index_of,
            "startsWith"  / 2 => impl_starts_with,
            "endsWith"    / 2 => impl_ends_with,
        )
    });

/// Convenience accessor used by the dispatcher.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    STRING_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::String, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::string::StringHeap;

    /// Drive an intrinsic with a string receiver. Argument inputs
    /// can be either decimal-integer strings (turned into
    /// `Value::Number`) or quoted forms — the helper auto-detects
    /// to keep the existing test cases readable.
    fn call(method: &str, recv: &str, args: &[&str]) -> String {
        let heap = StringHeap::default();
        let recv_v = Value::String(JsString::from_str(recv, &heap).unwrap());
        let arg_vs: Vec<Value> = args
            .iter()
            .map(|s| match s.parse::<i32>() {
                Ok(n) => Value::Number(NumberValue::from_i32(n)),
                Err(_) => Value::String(JsString::from_str(s, &heap).unwrap()),
            })
            .collect();
        let entry = lookup(method).unwrap();
        let result = (entry.impl_fn)(&IntrinsicArgs {
            receiver: &recv_v,
            args: &arg_vs,
            string_heap: &heap,
        })
        .unwrap();
        result.display_string()
    }

    #[test]
    fn length() {
        assert_eq!(call("length", "abc", &[]), "3");
    }

    #[test]
    fn char_code_at_basic() {
        assert_eq!(call("charCodeAt", "abc", &["1"]), "98");
        assert_eq!(call("charCodeAt", "abc", &["10"]), "NaN");
    }

    #[test]
    fn char_at_basic() {
        assert_eq!(call("charAt", "abc", &["1"]), "b");
        assert_eq!(call("charAt", "abc", &["10"]), "");
    }

    #[test]
    fn slice_basic() {
        assert_eq!(call("slice", "abcdef", &["1", "4"]), "bcd");
        assert_eq!(call("slice", "abcdef", &["2"]), "cdef");
    }

    #[test]
    fn substring_swaps_when_reversed() {
        assert_eq!(call("substring", "abcdef", &["4", "1"]), "bcd");
    }

    #[test]
    fn index_of() {
        assert_eq!(call("indexOf", "abcabc", &["bc"]), "1");
        assert_eq!(call("indexOf", "abcabc", &["bc", "2"]), "4");
        assert_eq!(call("indexOf", "abcabc", &["zz"]), "-1");
    }

    #[test]
    fn starts_ends_with() {
        assert_eq!(call("startsWith", "hello", &["he"]), "true");
        assert_eq!(call("startsWith", "hello", &["lo"]), "false");
        assert_eq!(call("endsWith", "hello", &["lo"]), "true");
        assert_eq!(call("endsWith", "hello", &["he"]), "false");
    }

    #[test]
    fn bad_receiver_rejects() {
        let heap = StringHeap::default();
        let entry = lookup("length").unwrap();
        let err = (entry.impl_fn)(&IntrinsicArgs {
            receiver: &Value::Undefined,
            args: &[],
            string_heap: &heap,
        })
        .unwrap_err();
        assert!(matches!(err, IntrinsicError::BadReceiver { .. }));
    }
}
