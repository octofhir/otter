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
];

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
    let mut units: Vec<u16> = Vec::with_capacity(args.len());
    for arg in args {
        let n = crate::number::parse::to_number_value(arg, ctx.heap());
        units.push(to_uint16(n));
    }
    JsString::from_utf16_units(&units, ctx.heap_mut())
        .map(Value::String)
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
    let mut units: Vec<u16> = Vec::with_capacity(args.len() * 2);
    for arg in args {
        let n = crate::number::parse::to_number_value(arg, ctx.heap());
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
        .map(Value::String)
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
