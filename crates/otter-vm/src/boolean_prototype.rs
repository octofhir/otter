//! `Boolean.prototype.*` intrinsic implementations.
//!
//! Boolean methods support both primitive booleans and Boolean
//! wrapper objects. Wrapper objects carry their `[[BooleanData]]`
//! internal slot inside the object payload, not as a JS-visible own
//! property.
//!
//! # Contents
//! - [`BOOLEAN_PROTOTYPE_TABLE`] — declarative table built with
//!   the [`crate::intrinsics!`] macro.
//! - [`BOOLEAN_PROTOTYPE_METHODS`] — native method specs installed
//!   on the global `Boolean.prototype`.
//! - [`lookup`] — convenience accessor used by the dispatcher.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-boolean-prototype-object>
use crate::Value;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::js_surface::{Attr, MethodSpec};
use crate::string::JsString;
use crate::{NativeCall, NativeCtx, NativeError};

fn receiver_bool(args: &IntrinsicArgs<'_>) -> Result<bool, IntrinsicError> {
    match args.receiver {
        Value::Boolean(b) => Ok(*b),
        Value::Object(obj) => {
            let gc = args.gc_heap.borrow();
            crate::object::boolean_data(*obj, &gc).ok_or(IntrinsicError::BadReceiver {
                expected: "boolean",
            })
        }
        _ => Err(IntrinsicError::BadReceiver {
            expected: "boolean",
        }),
    }
}

/// §20.3.3.2 Boolean.prototype.toString — `"true"` / `"false"`.
fn impl_to_string(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = if receiver_bool(args)? {
        "true"
    } else {
        "false"
    };
    Ok(Value::String(JsString::from_str(s, args.string_heap)?))
}

/// §20.3.3.3 Boolean.prototype.valueOf — returns the receiver.
fn impl_value_of(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(Value::Boolean(receiver_bool(args)?))
}

/// Declarative `Boolean.prototype` table.
pub static BOOLEAN_PROTOTYPE_TABLE: std::sync::LazyLock<IntrinsicTable> =
    std::sync::LazyLock::new(|| {
        crate::intrinsics!(
            Boolean,
            "toString" / 0 => impl_to_string,
            "valueOf"  / 0 => impl_value_of,
        )
    });

/// Convenience accessor used by the dispatcher.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    BOOLEAN_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Boolean, name)
}

/// `MethodSpec` list installed on `Boolean.prototype` by
/// `bootstrap::install_boolean`. Both primitive dispatch and
/// object-property calls route through [`BOOLEAN_PROTOTYPE_TABLE`].
pub static BOOLEAN_PROTOTYPE_METHODS: &[MethodSpec] = &[
    method("toString", 0, native_to_string),
    method("valueOf", 0, native_value_of),
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

fn native_boolean_method(
    name: &'static str,
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let receiver = ctx.this_value().clone();
    let string_heap = ctx.interp_mut().string_heap_clone();
    let entry = lookup(name).ok_or_else(|| NativeError::TypeError {
        name,
        reason: "unknown Boolean.prototype method".to_string(),
    })?;
    (entry.impl_fn)(&IntrinsicArgs {
        receiver: &receiver,
        args,
        string_heap: &string_heap,
        gc_heap: std::cell::RefCell::new(ctx.heap_mut()),
    })
    .map_err(|err| NativeError::TypeError {
        name,
        reason: err.to_string(),
    })
}

fn native_to_string(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    native_boolean_method("toString", ctx, args)
}

fn native_value_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    native_boolean_method("valueOf", ctx, args)
}
