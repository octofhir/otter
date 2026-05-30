//! `Symbol.prototype.*` native implementations.
//!
//! The methods exposed here are installed on the realm
//! `Symbol.prototype` as NativeCtx natives. Primitive symbol method
//! calls front-run through `method_ops` only to decide whether the
//! prototype lookup should be attempted; invocation still uses
//! §7.3.11 `GetMethod` + §7.3.14 `Call`.
//!
//! # Contents
//! - [`SYMBOL_PROTOTYPE_METHODS`] — native method specs installed on
//!   the global `Symbol.prototype`.
//! - [`load_property`] — accessor reads (`description`).
//!
//! # Invariants
//! - Receivers must be primitive symbols or Symbol wrapper objects.
//! - Methods return primitive [`Value`]s.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-symbol-prototype-object>

use crate::js_surface::{Attr, MethodSpec};
use crate::string::JsString;
use crate::{JsSymbol, NativeCall, NativeCtx, NativeError, Value};

/// Native `Symbol.prototype` method specs.
pub static SYMBOL_PROTOTYPE_METHODS: &[MethodSpec] = &[
    method("toString", 0, symbol_to_string),
    method("valueOf", 0, symbol_value_of),
];

const fn method(
    name: &'static str,
    length: u8,
    call: for<'rt> fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>,
) -> MethodSpec {
    MethodSpec {
        name,
        length,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(call),
    }
}

/// `true` when `name` is installed on `Symbol.prototype`.
#[must_use]
pub fn is_builtin_method(name: &str) -> bool {
    SYMBOL_PROTOTYPE_METHODS
        .iter()
        .any(|method| method.name == name)
}

fn this_symbol_value(ctx: &NativeCtx<'_>, name: &'static str) -> Result<JsSymbol, NativeError> {
    let this = *ctx.this_value();
    if let Some(sym) = this.as_symbol(ctx.heap()) {
        return Ok(sym);
    }
    if let Some(obj) = this.as_object()
        && let Some(sym) = crate::object::symbol_data(obj, ctx.heap())
    {
        return Ok(sym);
    }
    Err(NativeError::TypeError {
        name,
        reason: "this is not a Symbol".to_string(),
    })
}

/// §20.4.3.3 `Symbol.prototype.toString`.
fn symbol_to_string(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let sym = this_symbol_value(ctx, "Symbol.prototype.toString")?;
    let s =
        JsString::from_str(&sym.descriptive_string(ctx.heap()), ctx.heap_mut()).map_err(|_| {
            NativeError::TypeError {
                name: "Symbol.prototype.toString",
                reason: "out of memory".to_string(),
            }
        })?;
    Ok(Value::string(s))
}

/// §20.4.3.4 `Symbol.prototype.valueOf`.
fn symbol_value_of(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let sym = this_symbol_value(ctx, "Symbol.prototype.valueOf")?;
    Ok(Value::symbol(sym))
}

/// Read a non-method property off `Symbol.prototype`-bearing
/// receivers. Foundation exposes only the `description` accessor;
/// other names fall back to `Value::Undefined` per the spec's
/// `[[Get]]` semantics for primitives without ordinary properties.
///
/// Reads come from the wrapper-side description cache; no heap touch.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-symbol.prototype.description>
#[must_use]
pub fn load_property(sym: JsSymbol, name: &str) -> Value {
    if name == "description" {
        match sym.description() {
            Some(s) => Value::string(*s),
            None => Value::undefined(),
        }
    } else {
        Value::undefined()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn description_accessor() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let s = JsString::from_str("ok", &mut gc_heap).unwrap();
        let sym = JsSymbol::new(&mut gc_heap, Some(s)).unwrap();
        let value = load_property(sym, "description");
        assert_eq!(value.display_string(&gc_heap), "ok");
        let no_desc = JsSymbol::new(&mut gc_heap, None).unwrap();
        assert!(load_property(no_desc, "description").is_undefined());
    }
}
