//! `String` built-in installer.
//!
//! Routes through `couch!`. Static methods come from the pre-built
//! `STRING_STATIC_METHODS` slice via `static_method_specs`; prototype
//! methods come from `STRING_PROTOTYPE_METHODS` via the prototype
//! `method_specs` field. The constructor itself handles call vs
//! construct internally (§22.1.1). The `[[StringData]] = ""` slot on
//! the prototype + the §B.2.3 trimLeft/trimRight identity-sharing
//! ride the `post_install` hook.
//!
//! # Invariants
//! - `Object` is installed before `String` (see
//!   [`crate::bootstrap::BOOTSTRAP_ENTRIES`] ordering); `couch!` reads
//!   `globalThis.Object.prototype` to wire the prototype chain.
//! - The prototype carries an empty `[[StringData]]` so
//!   `Object.prototype.toString.call(String.prototype)` reports the
//!   `String` brand and prototype methods recover a string receiver
//!   when invoked through `Reflect.get` on the prototype.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-string-constructor>
//! - <https://tc39.es/ecma262/#sec-properties-of-the-string-prototype-object>

use crate::js_surface::JsSurfaceError;
use crate::object::{self, JsObject};
use crate::{NativeCtx, NativeError, Value};

otter_macros::couch! {
    name = "String",
    feature = CORE,
    constructor = (length = 1, call = string_ctor_call),
    static_method_specs = [super::statics::STRING_STATIC_METHODS],
    prototype = {
        method_specs = [super::prototype::STRING_PROTOTYPE_METHODS],
    },
    post_install = pin_string_data_and_aliases,
}

/// Post-bootstrap fixup:
/// - §22.1.3 — set `[[StringData]] = ""` on the prototype so brand
///   checks and prototype-method receivers behave per spec.
/// - §B.2.3.{2,3} — `String.prototype.trimLeft` is the SAME function
///   object as `String.prototype.trimStart` (and `trimRight` ===
///   `trimEnd`). Replace the independently-installed copies with
///   shared references so identity holds.
fn pin_string_data_and_aliases(
    heap: &mut otter_gc::GcHeap,
    _global: JsObject,
    ctor: crate::native_function::NativeFunction,
) -> Result<(), JsSurfaceError> {
    let descriptor = ctor
        .own_property_descriptor(heap, "prototype")
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let mut prototype = match descriptor.and_then(|d| match d.kind {
        crate::object::DescriptorKind::Data { value } => value.as_object(),
        _ => None,
    }) {
        Some(p) => p,
        None => return Ok(()),
    };
    let empty_str =
        crate::string::JsString::from_str("", heap).map_err(|_| JsSurfaceError::OutOfMemory)?;
    crate::object::set_string_data(prototype, heap, empty_str);

    if let Some(start_fn) = object::get(prototype, heap, "trimStart") {
        object::set(&mut prototype, heap, "trimLeft", start_fn);
    }
    if let Some(end_fn) = object::get(prototype, heap, "trimEnd") {
        object::set(&mut prototype, heap, "trimRight", end_fn);
    }
    Ok(())
}

/// `String(...)` / `new String(...)` native — ECMA-262 §22.1.1.
fn string_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let raw = match args.first() {
        Some(value) => *value,
        None => {
            let empty = crate::string::JsString::from_str("", ctx.heap_mut()).map_err(|_| {
                NativeError::TypeError {
                    name: "String",
                    reason: "string allocation failed".to_string(),
                }
            })?;
            Value::string(empty)
        }
    };
    // §22.1.1.1 step 2.a — `String(sym)` (NewTarget undefined) returns
    // SymbolDescriptiveString; `new String(sym)` falls through to
    // ToString(Symbol), a TypeError. Only the non-construct call gets
    // the descriptive-string shortcut.
    if let Some(sym) = raw.as_symbol(ctx.heap()) {
        if ctx.is_construct_call() {
            return Err(NativeError::TypeError {
                name: "String",
                reason: "Cannot convert a Symbol value to a string".to_string(),
            });
        }
        let descriptive = sym.descriptive_string(ctx.heap());
        let s = crate::string::JsString::from_str(&descriptive, ctx.heap_mut()).map_err(|_| {
            NativeError::TypeError {
                name: "String",
                reason: "string allocation failed".to_string(),
            }
        })?;
        return Ok(Value::string(s));
    }
    let is_primitive = raw.is_undefined()
        || raw.is_null()
        || raw.is_boolean()
        || raw.is_number()
        || raw.is_big_int()
        || raw.is_string()
        || raw.is_symbol();
    let primitive = if is_primitive {
        raw
    } else {
        let (interp, exec) = ctx.interp_mut_and_context();
        let exec = exec.ok_or_else(|| NativeError::TypeError {
            name: "String",
            reason: "missing execution context".to_string(),
        })?;
        match interp.evaluate_to_primitive(
            &exec,
            &raw,
            crate::abstract_ops::ToPrimitiveHint::String,
        ) {
            Ok(p) => p,
            Err(crate::VmError::Uncaught) => {
                let value = match interp.take_error_detail() {
                    Some(crate::run_control::ErrorDetail::Uncaught(m)) => m,
                    _ => Default::default(),
                };
                return Err(NativeError::Thrown {
                    name: "String",
                    message: value.into(),
                });
            }
            Err(other) => {
                return Err(NativeError::TypeError {
                    name: "String",
                    reason: other.to_string(),
                });
            }
        }
    };
    let value = crate::string::dispatch::call(
        otter_bytecode::method_id::StringMethod::Construct,
        std::slice::from_ref(&primitive),
        ctx.heap_mut(),
    )
    .map_err(|err| NativeError::TypeError {
        name: "String",
        reason: err.to_string(),
    })?;
    if ctx.is_construct_call() {
        let Some(string) = value.as_string(ctx.heap()) else {
            return Err(NativeError::TypeError {
                name: "String",
                reason: "constructor did not return a string primitive".to_string(),
            });
        };
        let this = *ctx.this_value();
        if let Some(obj) = this.as_object() {
            crate::object::set_string_data(obj, ctx.heap_mut(), string);
            Ok(Value::object(obj))
        } else {
            Err(NativeError::TypeError {
                name: "String",
                reason: "expected object receiver in `new String(...)`".to_string(),
            })
        }
    } else {
        Ok(value)
    }
}
