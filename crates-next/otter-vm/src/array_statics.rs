//! `Array.<static>` dispatcher — `Array.from` and `Array.of` per
//! ECMA-262 §23.1.2. Routed through [`crate::otter_bytecode::Op::ArrayCall`]
//! by the compiler; `Array.isArray` keeps its dedicated fast-path
//! opcode and is handled outside this module.
//!
//! # Contents
//! - [`call`] — single entry point used by the dispatch loop.
//!
//! # Invariants
//! - The callback-driven shape of `Array.from(iter, mapFn)` is
//!   filed as a follow-up; this module accepts an optional `mapFn`
//!   only when it is `undefined`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-array-constructor>
//! - <https://tc39.es/ecma262/#sec-array.from>
//! - <https://tc39.es/ecma262/#sec-array.of>

use crate::array::JsArray;
use crate::{Value, VmError};

/// Dispatch `Array.<name>(args...)`. Returns the call's completion
/// value or surfaces a [`VmError`].
///
/// # Errors
/// - [`VmError::UnknownIntrinsic`] when `name` is not recognised.
/// - [`VmError::TypeMismatch`] when the iterable argument has the
///   wrong shape.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-array.from>
/// - <https://tc39.es/ecma262/#sec-array.of>
pub fn call(name: &str, args: &[Value]) -> Result<Value, VmError> {
    match name {
        // §23.1.2.1 Array.from(items[, mapFn[, thisArg]]).
        // <https://tc39.es/ecma262/#sec-array.from>
        "from" => {
            // Reject a present mapFn — its callback dispatch is
            // filed as a follow-up.
            if !matches!(args.get(1), None | Some(Value::Undefined)) {
                return Err(VmError::UnknownIntrinsic {
                    name: "Array.from(iter, mapFn)".to_string(),
                });
            }
            let source = args.first().cloned().unwrap_or(Value::Undefined);
            let elements = match source {
                Value::Array(a) => a.borrow_body().iter().cloned().collect::<Vec<_>>(),
                Value::String(s) => {
                    // §23.1.2.1 step 5 — string iterator yields
                    // 16-bit code units (foundation simplification;
                    // full code-point iteration is filed against
                    // task 71).
                    let mut units: Vec<Value> = Vec::with_capacity(s.len() as usize);
                    let len = s.len();
                    for i in 0..len {
                        units.push(Value::String(crate::JsString::from_utf16_units(
                            &[s.char_code_at(i).unwrap_or(0)],
                            // No string heap available in this path:
                            // re-use the input's allocator via a
                            // temporary shared heap is not ideal.
                            // Fallback: empty string when allocation
                            // fails. The interpreter's heap-aware
                            // path handles this cleanly through the
                            // intrinsic table; for now we copy via
                            // each unit.
                            &Default::default(),
                        )?));
                    }
                    units
                }
                Value::Set(s) => s.values(),
                Value::Map(m) => m
                    .entries()
                    .into_iter()
                    .map(|(k, v)| Value::Array(JsArray::from_elements(vec![k, v])))
                    .collect(),
                _ => return Err(VmError::TypeMismatch),
            };
            Ok(Value::Array(JsArray::from_elements(elements)))
        }
        // §23.1.2.3 Array.of(...items).
        // <https://tc39.es/ecma262/#sec-array.of>
        "of" => Ok(Value::Array(JsArray::from_elements(args.to_vec()))),
        _ => Err(VmError::UnknownIntrinsic {
            name: format!("Array.{name}"),
        }),
    }
}
