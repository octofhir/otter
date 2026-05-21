//! `String(...)` constructor + `String.<static>` dispatcher.
//! Routed through [`crate::otter_bytecode::Op::StringCall`].
//!
//! # Contents
//! - [`call`] — single entry point used by the dispatch loop.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-string-constructor>
//! - <https://tc39.es/ecma262/#sec-string.fromcharcode>
//! - <https://tc39.es/ecma262/#sec-string.fromcodepoint>

use crate::string::{JsString, StringHeap};
use crate::{Value, VmError};

/// Dispatch `String(...)` ([`StringMethod::Construct`]) /
/// `String.<method>(...)`. Routes the typed [`StringMethod`]
/// emitted by the compiler.
///
/// # Errors
/// - [`VmError::TypeMismatch`] for malformed inputs.
pub fn call(
    method: otter_bytecode::method_id::StringMethod,
    args: &[Value],
    heap: &StringHeap,
    gc_heap: &otter_gc::GcHeap,
) -> Result<Value, VmError> {
    use otter_bytecode::method_id::StringMethod as M;
    match method {
        // §22.1.1 String(value) — coerce via §7.1.17 ToString.
        M::Construct => {
            let s = crate::conversion::string_constructor_js_string(args.first(), heap, gc_heap)?;
            Ok(Value::String(s))
        }
        // §22.1.2.1 String.fromCharCode(...codeUnits).
        M::FromCharCode => {
            let mut units: Vec<u16> = Vec::with_capacity(args.len());
            for arg in args {
                let n = match arg {
                    Value::Number(n) => n.as_f64(),
                    _ => return Err(VmError::TypeMismatch),
                };
                let truncated = if n.is_finite() {
                    (n as i64) as u16
                } else {
                    0u16
                };
                units.push(truncated);
            }
            Ok(Value::String(
                JsString::from_utf16_units(&units, heap).map_err(|_| VmError::TypeMismatch)?,
            ))
        }
        // §22.1.2.2 String.fromCodePoint(...codePoints).
        M::FromCodePoint => {
            let mut units: Vec<u16> = Vec::with_capacity(args.len());
            for arg in args {
                let n = match arg {
                    Value::Number(n) => n.as_f64(),
                    _ => return Err(VmError::TypeMismatch),
                };
                if !n.is_finite() || n < 0.0 || n > 0x10FFFF as f64 || n.fract() != 0.0 {
                    return Err(VmError::TypeMismatch);
                }
                let cp = n as u32;
                if cp <= 0xFFFF {
                    units.push(cp as u16);
                } else {
                    // Surrogate pair encoding for supplementary
                    // planes.
                    let v = cp - 0x10000;
                    units.push(0xD800 | (v >> 10) as u16);
                    units.push(0xDC00 | (v & 0x3FF) as u16);
                }
            }
            Ok(Value::String(
                JsString::from_utf16_units(&units, heap).map_err(|_| VmError::TypeMismatch)?,
            ))
        }
    }
}
