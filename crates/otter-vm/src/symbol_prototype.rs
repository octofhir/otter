//! `Symbol.prototype.*` intrinsic implementations.
//!
//! The methods exposed here are invoked through
//! [`Op::CallMethodValue`](otter_bytecode::Op::CallMethodValue) when
//! the receiver is a [`Value::Symbol`]; see
//! [`crate::intrinsics::IntrinsicReceiver::Symbol`].
//!
//! # Contents
//! - [`SYMBOL_PROTOTYPE_TABLE`] — declarative method registry.
//! - [`load_property`] — accessor reads (`description`).
//! - [`lookup`] — convenience for the dispatcher.
//!
//! # Invariants
//! - Receivers must be [`Value::Symbol`]; the foundation does not
//!   wrap symbols in a Symbol object yet, so all methods reject
//!   non-symbol receivers up-front.
//! - Methods return primitive [`Value`]s; nothing here allocates a
//!   wrapper object.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-symbol-prototype-object>

use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::string::JsString;
use crate::{JsSymbol, Value};

fn receiver_symbol<'a>(args: &'a IntrinsicArgs<'_>) -> Result<&'a JsSymbol, IntrinsicError> {
    match args.receiver {
        Value::Symbol(s) => Ok(s),
        _ => Err(IntrinsicError::BadReceiver { expected: "symbol" }),
    }
}

/// `Symbol.prototype.toString` — Spec §20.4.3.3. Returns
/// `"Symbol(<desc>)"`, with no description rendered as `"Symbol()"`.
fn impl_to_string(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let sym = receiver_symbol(args)?;
    let s = JsString::from_str(&sym.descriptive_string(args.gc_heap), args.gc_heap)?;
    Ok(Value::String(s))
}

/// `Symbol.prototype.valueOf` — Spec §20.4.3.4. Returns the
/// receiver symbol primitive.
fn impl_value_of(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let sym = receiver_symbol(args)?;
    Ok(Value::Symbol(*sym))
}

/// `Symbol.prototype[@@toPrimitive]` — Spec §20.4.3.5. The hint is
/// ignored; the symbol primitive is returned for every hint.
fn impl_to_primitive(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let sym = receiver_symbol(args)?;
    Ok(Value::Symbol(*sym))
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
pub fn load_property(sym: &JsSymbol, name: &str) -> Value {
    if name == "description" {
        match sym.description() {
            Some(s) => Value::String(*s),
            None => Value::undefined(),
        }
    } else {
        Value::Undefined
    }
}

/// Declarative `Symbol.prototype` table.
pub static SYMBOL_PROTOTYPE_TABLE: std::sync::LazyLock<IntrinsicTable> =
    std::sync::LazyLock::new(|| {
        crate::intrinsics!(
            Symbol,
            "toString" / 0 => impl_to_string,
            "valueOf"  / 0 => impl_value_of,
            // The well-known method exposes the Symbol body through
            // both the primitive name `[Symbol.toPrimitive]` and the
            // descriptive string (which a literal `"Symbol(@@toPrimitive)"`
            // member access would yield) — see test fixtures.
            "@@toPrimitive" / 1 => impl_to_primitive,
        )
    });

/// Convenience accessor used by the dispatcher.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    SYMBOL_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Symbol, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_string_renders_descriptive_form() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let desc = JsString::from_str("ok", &mut gc_heap).unwrap();
        let sym = JsSymbol::new(&mut gc_heap, Some(desc)).unwrap();
        let entry = lookup("toString").unwrap();
        let result = (entry.impl_fn)(&mut IntrinsicArgs {
            receiver: &Value::Symbol(sym),
            args: &[],
            gc_heap: &mut gc_heap,
            allocation_roots: &[],
        })
        .unwrap();
        assert_eq!(result.display_string(&gc_heap), "Symbol(ok)");
    }

    #[test]
    fn description_accessor() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let s = JsString::from_str("ok", &mut gc_heap).unwrap();
        let sym = JsSymbol::new(&mut gc_heap, Some(s)).unwrap();
        let value = load_property(&sym, "description");
        assert_eq!(value.display_string(&gc_heap), "ok");
        let no_desc = JsSymbol::new(&mut gc_heap, None).unwrap();
        assert!(matches!(
            load_property(&no_desc, "description"),
            Value::Undefined
        ));
    }
}
