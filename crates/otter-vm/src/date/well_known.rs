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
    let Some(Value::Object(date_ctor)) = object::get(global, heap, "Date") else {
        return Ok(());
    };
    let Some(Value::Object(prototype)) = object::get(date_ctor, heap, "prototype") else {
        return Ok(());
    };

    let global_root = Value::Object(global);
    let date_ctor_root = Value::Object(date_ctor);
    let prototype_root = Value::Object(prototype);
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
        &to_primitive_sym,
        PartialPropertyDescriptor {
            value: Some(Value::NativeFunction(to_prim_fn)),
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
    let receiver = ctx.this_value().clone();
    if !matches!(receiver, Value::Object(_)) {
        return Err(NativeError::TypeError {
            name: NAME,
            reason: "this value must be an Object".to_string(),
        });
    }
    let hint_value = args.first().cloned().unwrap_or(Value::Undefined);
    let try_first = match &hint_value {
        Value::String(js) => {
            let token = js.to_lossy_string();
            match token.as_str() {
                "string" | "default" => ToPrimitiveHint::String,
                "number" => ToPrimitiveHint::Number,
                _ => {
                    return Err(NativeError::TypeError {
                        name: NAME,
                        reason: format!("invalid hint {token:?}"),
                    });
                }
            }
        }
        _ => {
            return Err(NativeError::TypeError {
                name: NAME,
                reason: "hint must be a string".to_string(),
            });
        }
    };
    let (interp, exec) = ctx.interp_mut_and_context();
    let exec = exec.ok_or_else(|| NativeError::TypeError {
        name: NAME,
        reason: "missing execution context".to_string(),
    })?;
    interp
        .evaluate_ordinary_to_primitive(&exec, &receiver, try_first)
        .map_err(|err| match err {
            VmError::Uncaught { value } => NativeError::Thrown {
                name: NAME,
                message: value,
            },
            VmError::TypeError { message } => NativeError::TypeError {
                name: NAME,
                reason: message,
            },
            VmError::RangeError { message } => NativeError::RangeError {
                name: NAME,
                reason: message,
            },
            other => NativeError::TypeError {
                name: NAME,
                reason: other.to_string(),
            },
        })
}
