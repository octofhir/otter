//! `String.<static>` JS-visible method specs.
//!
//! The `String` constructor's static surface is split between two
//! paths:
//!
//! 1. The compiler's `StringCall` opcode (see
//!    [`crate::string::dispatch`]) recognises calls of the form
//!    `String.fromCharCode(...)` / `String.fromCodePoint(...)` at
//!    compile time and routes them through a typed enum dispatch.
//!    This path is used when the receiver is provably the global
//!    `String` constructor and is the runtime hot path.
//!
//! 2. The specs in this module install the same methods as ordinary
//!    own properties on the `String` constructor object so that
//!    indirect access (`const f = String.fromCharCode; f(72)`),
//!    `Reflect.get`, `Object.getOwnPropertyNames`, and similar
//!    spec-observable lookups see real `Function` values rather than
//!    `undefined`.
//!
//! Keeping the implementations here (instead of inlining them into
//! `bootstrap.rs`) bounds the size of the installer and gives the
//! per-class module the only home for String constructor behaviour.
//!
//! # Contents
//! - [`STRING_STATIC_METHODS`] — slice consumed by
//!   `bootstrap::install_string` via `ObjectBuilder::method_from_spec`.
//! - One private `string_<method>` native per spec entry.
//! - [`to_uint16`] — shared §7.1.21 ToUint16 coercion helper used
//!   by `String.fromCharCode`.
//!
//! # Invariants
//! - Each native coerces its arguments through the spec's abstract
//!   operations: `String.fromCharCode` uses ? ToUint16 on every
//!   argument; `String.fromCodePoint` validates integer code points
//!   in `[0, 0x10FFFF]` and rejects everything else with a
//!   `RangeError`. The runtime never silently widens / truncates
//!   beyond what the spec mandates.
//! - Supplementary code points are encoded as UTF-16 surrogate pairs
//!   per §11.1.4 so the resulting `JsString` round-trips through the
//!   engine's UTF-16 representation without re-validation.
//! - The natives must not allocate JS objects on the GC heap. They
//!   only allocate `JsString` instances through the runtime string
//!   heap, which has its own (non-GC) lifecycle.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-string-constructor>
//! - <https://tc39.es/ecma262/#sec-string.fromcharcode>
//! - <https://tc39.es/ecma262/#sec-string.fromcodepoint>
//! - <https://tc39.es/ecma262/#sec-touint16>

use crate::js_surface::{Attr, MethodSpec};
use crate::native_function::NativeCall;
use crate::string::JsString;
use crate::{NativeCtx, NativeError, Value};

/// Static methods installed on the `String` constructor.
///
/// Consumed by [`crate::bootstrap`] through
/// `ObjectBuilder::method_from_spec` so the surface stays declarative
/// and the installer body stays small.
pub static STRING_STATIC_METHODS: &[MethodSpec] = &[
    MethodSpec {
        name: "fromCharCode",
        length: 1,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(string_from_char_code),
    },
    MethodSpec {
        name: "fromCodePoint",
        length: 1,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(string_from_code_point),
    },
    MethodSpec {
        name: "raw",
        length: 1,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(string_raw),
    },
];

/// `String.raw(template, ...substitutions)` — ECMA-262 §22.1.2.4.
///
/// # Algorithm
/// 1. `cooked = ? ToObject(template)`; `raw = ? ToObject(? Get(cooked,
///    "raw"))`.
/// 2. `literalSegments = ? LengthOfArrayLike(raw)`; return `""` when
///    it is `<= 0`.
/// 3. Walk `nextIndex` in `[0, literalSegments)`, appending
///    `? ToString(? Get(raw, ToString(nextIndex)))` and — for every
///    index but the last — `? ToString(substitutions[nextIndex])`.
///
/// Each `Get` flows through the interpreter's `[[Get]]` ladder so
/// accessor `raw` segments observe user getters, and every `ToString`
/// re-enters `@@toPrimitive` / `toString` / `valueOf` on object
/// operands.
fn string_raw(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let context = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "String.raw",
            reason: "no execution context available".to_string(),
        })?;
    ctx.scope(|mut scope| {
        let template = scope.argument(args, 0);
        let template_value = scope.raw(template);
        let raw_result = scope.with_turn_parts(|interp, stack| {
            interp.get_property_value_for_call(stack, &context, template_value, "raw")
        });
        let raw_value = match raw_result {
            Ok(value) => value,
            Err(error) => {
                return Err(crate::native_function::vm_to_native_error(
                    scope.context().interp_mut(),
                    error,
                    "String.raw",
                ));
            }
        };
        let raw = scope.value(raw_value);

        let raw_value = scope.raw(raw);
        let length_result = scope.with_turn_parts(|interp, stack| {
            interp.get_property_value_for_call(stack, &context, raw_value, "length")
        });
        let length_value = match length_result {
            Ok(value) => value,
            Err(error) => {
                return Err(crate::native_function::vm_to_native_error(
                    scope.context().interp_mut(),
                    error,
                    "String.raw",
                ));
            }
        };
        let length = scope.value(length_value);
        let length_value = scope.raw(length);
        let literal_segments_result = scope.with_turn_parts(|interp, stack| {
            crate::coerce::to_length_or_throw(interp, stack, &context, &length_value)
        });
        let literal_segments = match literal_segments_result {
            Ok(length) => length,
            Err(error) => {
                return Err(crate::native_function::vm_to_native_error(
                    scope.context().interp_mut(),
                    error,
                    "String.raw",
                ));
            }
        };
        if literal_segments == 0 {
            let empty = scope.string("")?;
            return Ok(scope.finish(empty));
        }

        let mut out = String::new();
        let substitutions = args.get(1..).unwrap_or(&[]);
        for next_index in 0..literal_segments {
            let key = next_index.to_string();
            let raw_value = scope.raw(raw);
            let segment_result = scope.with_turn_parts(|interp, stack| {
                interp.get_property_value_for_call(stack, &context, raw_value, &key)
            });
            let segment_value = match segment_result {
                Ok(value) => value,
                Err(error) => {
                    return Err(crate::native_function::vm_to_native_error(
                        scope.context().interp_mut(),
                        error,
                        "String.raw",
                    ));
                }
            };
            let segment = scope.value(segment_value);
            let segment_value = scope.raw(segment);
            let text_result = scope.with_turn_parts(|interp, stack| {
                interp.coerce_to_string(stack, &context, &segment_value)
            });
            let text = match text_result {
                Ok(text) => text,
                Err(error) => {
                    return Err(crate::native_function::vm_to_native_error(
                        scope.context().interp_mut(),
                        error,
                        "String.raw",
                    ));
                }
            };
            out.push_str(&text);
            if next_index + 1 == literal_segments {
                break;
            }
            if let Some(substitution) = substitutions.get(next_index) {
                let text_result = scope.with_turn_parts(|interp, stack| {
                    interp.coerce_to_string(stack, &context, substitution)
                });
                let text = match text_result {
                    Ok(text) => text,
                    Err(error) => {
                        return Err(crate::native_function::vm_to_native_error(
                            scope.context().interp_mut(),
                            error,
                            "String.raw",
                        ));
                    }
                };
                out.push_str(&text);
            }
        }
        let result = scope.string(&out)?;
        Ok(scope.finish(result))
    })
}

/// `String.fromCharCode(...codeUnits)` — ECMA-262 §22.1.2.1.
///
/// # Algorithm
/// 1. Allocate a `Vec<u16>` sized to the argument count.
/// 2. For each `arg`, compute `unit = ToUint16(? ToNumber(arg))` via
///    [`to_uint16`] (the helper also collapses NaN / ±Infinity / ±0
///    to `0`, matching the spec's reduction in step 2).
/// 3. Build a UTF-16 [`JsString`] from the accumulated units through
///    the runtime's string heap.
///
/// # Coercion
/// Accepts any JavaScript value that survives `? ToNumber`:
/// `Number`, primitive `String`, `Boolean`, `null`, `undefined`,
/// `BigInt` (which throws — see [`crate::number::parse::to_number_value`]
/// for the exact contract), or a wrapper object via
/// `@@toPrimitive` → `valueOf` → `toString`.
fn string_from_char_code(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    // §22.1.2.1 — each argument is `ToUint16(? ToNumber(next))`, so an
    // object's `valueOf` runs and a Symbol / BigInt argument throws,
    // rather than being silently coerced to 0.
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "String.fromCharCode",
            reason: "no execution context available".to_string(),
        })?;
    let mut units: Vec<u16> = Vec::with_capacity(args.len());
    for arg in args {
        let number = ctx.with_turn_parts(|interp, stack| {
            crate::coerce::to_number_or_throw(interp, stack, &exec, arg)
        });
        let n = number
            .map_err(|error| {
                crate::native_function::vm_to_native_error(
                    ctx.interp_mut(),
                    error,
                    "String.fromCharCode",
                )
            })?
            .as_f64();
        units.push(to_uint16(n));
    }
    JsString::from_utf16_units(&units, ctx.heap_mut())
        .map(Value::string)
        .map_err(|_| NativeError::TypeError {
            name: "String.fromCharCode",
            reason: "string allocation failed".to_string(),
        })
}

/// `String.fromCodePoint(...codePoints)` — ECMA-262 §22.1.2.2.
///
/// # Algorithm
/// 1. Allocate a `Vec<u16>` sized to `args.len() * 2` to cover the
///    worst case of every code point landing in a supplementary
///    plane (each one expands to a surrogate pair).
/// 2. For each `arg`, run `nextCP ← ? ToNumber(arg)`. Reject
///    non-finite values, negative values, values above `0x10FFFF`,
///    and non-integer values with a `RangeError`.
/// 3. Encode BMP code points (`<= 0xFFFF`) as a single `u16` and
///    supplementary code points as a `0xD800`/`0xDC00` surrogate
///    pair per §11.1.4 UTF-16 Encoding of Code Points.
/// 4. Build a [`JsString`] from the units.
///
/// # Errors
/// - [`NativeError::RangeError`] — argument is `NaN`, negative,
///   greater than `0x10FFFF`, or has a non-zero fractional part.
/// - [`NativeError::TypeError`] — string heap exhausted while
///   materialising the final `JsString`.
fn string_from_code_point(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    // §22.1.2.2 — each argument is `? ToNumber(next)` (an object's
    // `valueOf` runs, a Symbol / BigInt throws), then validated as an
    // integer code point in `[0, 0x10FFFF]` (otherwise RangeError).
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "String.fromCodePoint",
            reason: "no execution context available".to_string(),
        })?;
    let mut units: Vec<u16> = Vec::with_capacity(args.len() * 2);
    for arg in args {
        let number = ctx.with_turn_parts(|interp, stack| {
            crate::coerce::to_number_or_throw(interp, stack, &exec, arg)
        });
        let n = number
            .map_err(|error| {
                crate::native_function::vm_to_native_error(
                    ctx.interp_mut(),
                    error,
                    "String.fromCodePoint",
                )
            })?
            .as_f64();
        if !n.is_finite() || n < 0.0 || n > 0x10FFFF as f64 || n.fract() != 0.0 {
            return Err(NativeError::RangeError {
                name: "String.fromCodePoint",
                reason: format!("invalid code point {n}"),
            });
        }
        let cp = n as u32;
        if cp <= 0xFFFF {
            units.push(cp as u16);
        } else {
            let v = cp - 0x10000;
            units.push(0xD800 | (v >> 10) as u16);
            units.push(0xDC00 | (v & 0x3FF) as u16);
        }
    }
    JsString::from_utf16_units(&units, ctx.heap_mut())
        .map(Value::string)
        .map_err(|_| NativeError::TypeError {
            name: "String.fromCodePoint",
            reason: "string allocation failed".to_string(),
        })
}

/// ECMA-262 §7.1.21 ToUint16.
///
/// 1. Let `number` be `? ToNumber(argument)` (assumed already
///    performed by the caller; this helper operates on the resulting
///    `f64`).
/// 2. If `number` is `NaN`, `+0`, `-0`, or non-finite, return `0`.
/// 3. Otherwise let `int` be `sign(number) * floor(abs(number))` and
///    return `int modulo 2^16`.
///
/// The `rem_euclid` step keeps the result non-negative without an
/// explicit sign branch and avoids the saturating-cast trap when
/// converting a very large `f64` directly to `u16`.
pub(crate) fn to_uint16(n: f64) -> u16 {
    if !n.is_finite() {
        return 0;
    }
    let truncated = n.trunc();
    (truncated.rem_euclid(65536.0) as u32) as u16
}

#[cfg(test)]
mod tests {
    use super::to_uint16;

    #[test]
    fn to_uint16_collapses_non_finite_to_zero() {
        assert_eq!(to_uint16(f64::NAN), 0);
        assert_eq!(to_uint16(f64::INFINITY), 0);
        assert_eq!(to_uint16(f64::NEG_INFINITY), 0);
    }

    #[test]
    fn to_uint16_truncates_toward_zero_then_mods() {
        assert_eq!(to_uint16(0.0), 0);
        assert_eq!(to_uint16(-0.0), 0);
        assert_eq!(to_uint16(65.999), 65);
        assert_eq!(to_uint16(-1.5), u16::MAX); // (-1).rem_euclid(2^16) == 65535
        assert_eq!(to_uint16(65536.0), 0);
        assert_eq!(to_uint16(65537.0), 1);
        assert_eq!(to_uint16(0x1_0000_0001u64 as f64), 1);
    }

    #[test]
    fn to_uint16_handles_string_like_input_via_caller_coercion() {
        // Caller passes ToNumber("72") == 72.0 down here.
        assert_eq!(to_uint16(72.0), 72);
    }
}
