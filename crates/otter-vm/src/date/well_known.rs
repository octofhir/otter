//! ECMA-262 §21.4.4.45 — `Date.prototype[@@toPrimitive]` install.
//!
//! Bootstrap runs before [`crate::symbol::WellKnownSymbols`] exists,
//! so the realm-local `@@toPrimitive` binding is wired by this
//! post-bootstrap hook. The native dispatches to
//! [`Interpreter::evaluate_ordinary_to_primitive`] so the algorithm
//! matches §7.1.1.1 step 6 without re-entering `[Symbol.toPrimitive]`
//! (which would recurse forever, since the receiver's own
//! `@@toPrimitive` resolves back to this function).
//!
//! # Contents
//! - [`install_date_well_knowns_post_bootstrap`] — entry hook.
//!
//! # Invariants
//! - `Date.prototype[@@toPrimitive]` lives at
//!   `{ value: <native>, writable: false, enumerable: false, configurable: true }`
//!   per §21.4.4.45 the property table.
//! - The hint argument MUST be `"string"`, `"default"`, or
//!   `"number"`; any other value throws `TypeError`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-date.prototype-@@toprimitive>
//! - <https://tc39.es/ecma262/#sec-ordinarytoprimitive>

use crate::abstract_ops::ToPrimitiveHint;
use crate::bootstrap::native_static_with_value_roots;
use crate::js_surface::JsSurfaceError;
use crate::object::{self, JsObject, PartialPropertyDescriptor};
use crate::symbol::WellKnown;
use crate::{NativeCtx, NativeError, Value, VmError};

/// Install `Date.prototype[@@toPrimitive]` per §21.4.4.45.
pub fn install_date_well_knowns_post_bootstrap(
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    let Some(date_ctor_value) = object::get(global, heap, "Date") else {
        return Ok(());
    };
    let prototype = if let Some(date_ctor) = date_ctor_value.as_native_function() {
        date_ctor
            .own_property_descriptor(heap, "prototype")
            .ok()
            .flatten()
            .and_then(|desc| match desc.kind {
                object::DescriptorKind::Data { value } => value.as_object(),
                _ => None,
            })
    } else if let Some(date_ctor) = date_ctor_value.as_object() {
        object::get(date_ctor, heap, "prototype").and_then(|v| v.as_object())
    } else {
        None
    };
    let Some(prototype) = prototype else {
        return Ok(());
    };

    if let Some(to_utc_string) = object::get(prototype, heap, "toUTCString") {
        object::define_own_property_partial(
            prototype,
            heap,
            "toGMTString",
            PartialPropertyDescriptor {
                value: Some(to_utc_string),
                writable: Some(true),
                enumerable: Some(false),
                configurable: Some(true),
                ..Default::default()
            },
        );
    }

    let global_root = Value::object(global);
    let date_ctor_root = date_ctor_value;
    let prototype_root = Value::object(prototype);
    let to_prim_fn = native_static_with_value_roots(
        heap,
        "[Symbol.toPrimitive]",
        1,
        date_proto_to_primitive,
        &[&global_root, &date_ctor_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;

    let to_primitive_sym = well_known.get(WellKnown::ToPrimitive);
    object::define_own_symbol_property_partial(
        prototype,
        heap,
        to_primitive_sym,
        PartialPropertyDescriptor {
            value: Some(Value::native_function(to_prim_fn)),
            writable: Some(false),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    );
    Ok(())
}

/// §21.4.4.45 body.
///
/// 1. Let `O` be the **this** value.
/// 2. If `O` is not an Object, throw **TypeError**.
/// 3. If `hint` is `"string"` or `"default"`, let `tryFirst` be
///    `"string"`.
/// 4. Else if `hint` is `"number"`, let `tryFirst` be `"number"`.
/// 5. Else throw **TypeError**.
/// 6. Return `? OrdinaryToPrimitive(O, tryFirst)`.
fn date_proto_to_primitive(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    const NAME: &str = "Date.prototype[Symbol.toPrimitive]";
    let receiver = *ctx.this_value();
    if !receiver.is_object() {
        return Err(NativeError::TypeError {
            name: NAME,
            reason: "this value must be an Object".to_string(),
        });
    }
    let hint_value = args.first().cloned().unwrap_or(Value::undefined());
    let Some(js) = hint_value.as_string(ctx.heap()) else {
        return Err(NativeError::TypeError {
            name: NAME,
            reason: "hint must be a string".to_string(),
        });
    };
    let token = js.to_lossy_string(ctx.heap());
    let try_first = match token.as_str() {
        "string" | "default" => ToPrimitiveHint::String,
        "number" => ToPrimitiveHint::Number,
        _ => {
            return Err(NativeError::TypeError {
                name: NAME,
                reason: format!("invalid hint {token:?}"),
            });
        }
    };
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: NAME,
            reason: "missing execution context".to_string(),
        })?;
    let result = ctx.with_turn_parts(|interp, stack| {
        interp.evaluate_ordinary_to_primitive(stack, &exec, &receiver, try_first)
    });
    let interp = ctx.interp_mut();
    match result {
        Ok(v) => Ok(v),
        Err(VmError::Uncaught) => {
            let value = match interp.take_error_detail() {
                Some(crate::run_control::ErrorDetail::Uncaught(m)) => m,
                _ => Default::default(),
            };
            Err(NativeError::Thrown {
                name: NAME,
                message: value.into(),
            })
        }
        Err(VmError::TypeError) => {
            let message = match interp.take_error_detail() {
                Some(crate::run_control::ErrorDetail::Message(m)) => m,
                _ => Default::default(),
            };
            Err(NativeError::TypeError {
                name: NAME,
                reason: message.into(),
            })
        }
        Err(VmError::RangeError) => {
            let message = match interp.take_error_detail() {
                Some(crate::run_control::ErrorDetail::Message(m)) => m,
                _ => Default::default(),
            };
            Err(NativeError::RangeError {
                name: NAME,
                reason: message.into(),
            })
        }
        Err(other) => Err(NativeError::TypeError {
            name: NAME,
            reason: other.to_string(),
        }),
    }
}
