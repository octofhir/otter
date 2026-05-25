//! `%Array%` constructor installer.
//!
//! Routes through `couch!`. Static methods come from
//! `ARRAY_STATIC_METHODS` (also consumed by the `Op::CallMethod`
//! intrinsic dispatch fast path), prototype methods from
//! `ARRAY_PROTOTYPE_METHODS`. The constructor body handles the
//! §23.1.1.1 "single numeric arg = pre-sized sparse array" / "n+
//! args = collected values" split internally, plus the
//! `apply_array_new_target_prototype` subclassing fixup.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-array-constructor>

use crate::object;
use crate::{NativeCtx, NativeError, Value, array, descriptor_value};

otter_macros::couch! {
    name = "Array",
    feature = CORE,
    constructor = (length = 1, call = array_ctor_call),
    static_method_specs = [crate::array_statics::ARRAY_STATIC_METHODS],
    prototype = {
        method_specs = [crate::array_prototype::ARRAY_PROTOTYPE_METHODS],
    },
}

/// §23.1.1.1 Array(...values) — both `Array(…)` and `new Array(…)`
/// reach this callback. Single numeric argument means "pre-sized
/// sparse array of length n"; anything else collects values
/// verbatim.
///
/// <https://tc39.es/ecma262/#sec-array>
fn array_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    if !(args.len() == 1 && args.first().is_some_and(|v| v.is_number())) {
        let arr =
            ctx.array_from_elements(args.iter().cloned())
                .map_err(|_| NativeError::TypeError {
                    name: "Array",
                    reason: "out of memory while allocating array".to_string(),
                })?;
        apply_array_new_target_prototype(ctx, arr)?;
        return Ok(Value::array(arr));
    }
    let arr = ctx
        .array_from_elements(std::iter::empty())
        .map_err(|_| NativeError::TypeError {
            name: "Array",
            reason: "out of memory while allocating array".to_string(),
        })?;
    if let Some(n) = args[0].as_number() {
        let raw = n.as_f64();
        let len = raw as u32;
        if !raw.is_finite() || raw < 0.0 || raw != f64::from(len) {
            return Err(NativeError::RangeError {
                name: "Array",
                reason: "Invalid array length".to_string(),
            });
        }
        if len > 0 {
            // `array::set` gap-fills with `Value::Hole`, so writing
            // the trailing slot also fills every index in `[0, len-1)`
            // with a hole.
            let last = (len - 1) as usize;
            ctx.array_set(arr, last, Value::hole())
                .map_err(|_| NativeError::TypeError {
                    name: "Array",
                    reason: "out of memory while sizing array".to_string(),
                })?;
        }
        apply_array_new_target_prototype(ctx, arr)?;
        return Ok(Value::array(arr));
    }
    unreachable!("non-numeric Array(...) arguments returned above")
}

fn apply_array_new_target_prototype(
    ctx: &mut NativeCtx<'_>,
    arr: array::JsArray,
) -> Result<(), NativeError> {
    let Some(new_target) = ctx.new_target().cloned() else {
        return Ok(());
    };
    let proto = if let Some(class) = new_target.as_class_constructor() {
        Some(Value::object(class.prototype(ctx.heap())))
    } else if let Some(obj) = new_target.as_object() {
        object::get(obj, ctx.heap(), "prototype")
            .filter(|value| value.is_object_type() || value.is_proxy())
    } else if let Some(native) = new_target.as_native_function() {
        native
            .own_property_descriptor(ctx.heap_mut(), "prototype")
            .map_err(|err| NativeError::TypeError {
                name: "Array",
                reason: err.to_string(),
            })?
            .map(|descriptor| descriptor_value(&descriptor))
            .filter(|value| value.is_object_type() || value.is_proxy())
    } else {
        None
    };
    if let Some(proto) = proto {
        array::set_prototype_override(arr, ctx.heap_mut(), Some(proto));
    }
    Ok(())
}
