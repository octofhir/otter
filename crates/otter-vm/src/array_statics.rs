//! `Array.<static>` dispatchers and JS-visible static method specs.
//!
//! Each Array static surface has its own typed entry point —
//! [`construct`] for `Array(...)` / `new Array(...)`, [`from`] for
//! `Array.from`, [`of`] for `Array.of`. The compiler emits a
//! dedicated opcode per shape ([`crate::otter_bytecode::Op::ArrayConstruct`],
//! [`crate::otter_bytecode::Op::ArrayFrom`],
//! [`crate::otter_bytecode::Op::ArrayOf`]) so the dispatch loop
//! never compares strings to route the call. `Array.isArray` keeps
//! its [`crate::otter_bytecode::Op::IsArray`] fast path and is
//! also installed as a real builtin function property so captured
//! calls like `const f = Array.isArray; f(x)` observe the spec
//! surface.
//!
//! # Contents
//! - [`ARRAY_STATIC_METHODS`] — methods installed on the `Array`
//!   constructor during bootstrap.
//! - [`construct`] / [`from`] / [`of`] — entry points used by the
//!   dispatch loop.
//!
//! # Invariants
//! - The callback-driven shape of `Array.from(iter, mapFn)` is
//!   filed as a follow-up; this module accepts an optional `mapFn`
//!   only when it is `undefined`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-array-constructor>
//! - <https://tc39.es/ecma262/#sec-array>
//! - <https://tc39.es/ecma262/#sec-array.from>
//! - <https://tc39.es/ecma262/#sec-array.of>

use crate::array;
use crate::js_surface::{Attr, MethodSpec};
use crate::native_function::NativeCall;
use crate::{NativeCtx, NativeError, Value, VmError};

/// Static methods installed on the `Array` constructor.
pub static ARRAY_STATIC_METHODS: &[MethodSpec] = &[MethodSpec {
    name: "isArray",
    length: 1,
    attrs: Attr::builtin_function(),
    call: NativeCall::Static(native_is_array),
}];

fn native_is_array(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::Boolean(matches!(
        args.first(),
        Some(Value::Array(_))
    )))
}

/// §23.1.1.1 Array(...values) — both `Array(...)` and
/// `new Array(...)` share this body. Single numeric argument
/// reserves a sparse array of that length; any other shape
/// collects values verbatim.
///
/// # Errors
/// - [`VmError::TypeError`] with `"Invalid array length"` when the
///   sole numeric argument is non-finite, negative, or non-integral.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-array>
pub fn construct(args: &[Value], gc_heap: &mut otter_gc::GcHeap) -> Result<Value, VmError> {
    if args.len() == 1
        && let Value::Number(n) = &args[0]
    {
        let raw = n.as_f64();
        let len = raw as u32;
        if !raw.is_finite() || raw < 0.0 || raw != f64::from(len) {
            return Err(VmError::TypeError {
                message: "Invalid array length".to_string(),
            });
        }
        let arr = array::alloc_array(gc_heap)?;
        if len > 0 {
            // `array::set` gap-fills [0, len-1) with `Value::Hole`,
            // then writes `Hole` at `last` — so the whole range
            // becomes sparse holes.
            let last = (len - 1) as usize;
            array::set(arr, gc_heap, last, Value::Hole)?;
        }
        return Ok(Value::Array(arr));
    }
    Ok(Value::Array(array::from_elements(gc_heap, args.to_vec())?))
}

/// §23.1.2.1 Array.from(items[, mapFn[, thisArg]]).
///
/// Foundation simplification: rejects a present `mapFn` (callback
/// dispatch is filed as a follow-up).
///
/// # Errors
/// - [`VmError::UnknownIntrinsic`] when `mapFn` is supplied and not
///   `undefined`.
/// - [`VmError::TypeMismatch`] when the iterable argument has the
///   wrong shape.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-array.from>
pub fn from(args: &[Value], gc_heap: &mut otter_gc::GcHeap) -> Result<Value, VmError> {
    if !matches!(args.get(1), None | Some(Value::Undefined)) {
        return Err(VmError::UnknownIntrinsic {
            name: "Array.from(iter, mapFn)".to_string(),
        });
    }
    let source = args.first().cloned().unwrap_or(Value::Undefined);
    let elements = match source {
        Value::Array(a) => array::with_elements(a, gc_heap, |elements| elements.to_vec()),
        Value::String(s) => {
            // §23.1.2.1 step 5 — string iterator yields 16-bit code
            // units (foundation simplification; full code-point
            // iteration is filed against task 71).
            let mut units: Vec<Value> = Vec::with_capacity(s.len() as usize);
            let len = s.len();
            for i in 0..len {
                units.push(Value::String(crate::JsString::from_utf16_units(
                    &[s.char_code_at(i).unwrap_or(0)],
                    &Default::default(),
                )?));
            }
            units
        }
        Value::Set(s) => crate::collections::set_values(s, gc_heap),
        Value::Map(m) => crate::collections::map_entries(m, gc_heap)
            .into_iter()
            .map(|(k, v)| array::from_elements(gc_heap, vec![k, v]).map(Value::Array))
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err(VmError::TypeMismatch),
    };
    Ok(Value::Array(array::from_elements(gc_heap, elements)?))
}

/// §23.1.2.3 Array.of(...items) — collect the arguments into a
/// dense array.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-array.of>
pub fn of(args: &[Value], gc_heap: &mut otter_gc::GcHeap) -> Result<Value, VmError> {
    Ok(Value::Array(array::from_elements(gc_heap, args.to_vec())?))
}
