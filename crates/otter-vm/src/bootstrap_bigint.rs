//! ECMA-262 §21.2 BigInt bootstrap installer.
//!
//! `BigInt` is a callable-only NativeFunction (no `[[Construct]]`).
//! The prototype carries `toString` / `valueOf` as own data
//! properties; the constructor carries `asIntN` / `asUintN`
//! statics. `@@toStringTag = "BigInt"` is installed in the
//! post-bootstrap hook.
//!
//! # Contents
//! - [`install_bigint`] — bootstrap entry.
//! - [`install_bigint_well_knowns_post_bootstrap`] — `@@toStringTag`.
//!
//! # Invariants
//! - `new BigInt(x)` throws `TypeError` per §21.2.1.1 step 1.
//! - `BigInt(x)` callable returns a `Value::BigInt` via
//!   [`crate::bigint::dispatch::call`].
//! - `asIntN` / `asUintN` flow through the same dispatcher.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-bigint-constructor>

use otter_bytecode::method_id::BigIntMethod;

use crate::bigint;
use crate::bootstrap::BootstrapEntry;
use crate::js_surface::{Attr, JsSurfaceError, ObjectBuilder};
use crate::native_function::{NativeCall, NativeFunction};
use crate::object::{self, JsObject, PartialPropertyDescriptor, PropertyDescriptor};
use crate::{NativeCtx, NativeError, Value, VmError};

/// §21.2 BigInt — bootstrap install.
pub(crate) fn install_bigint(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    let prototype = object::alloc_object(heap)?;
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(prototype, heap, Some(object_proto));
    }
    {
        let mut builder = ObjectBuilder::from_object(heap, prototype);
        builder.method(
            "toString",
            0,
            NativeCall::Static(bigint_proto_to_string),
            Attr::builtin_function(),
        )?;
        builder.method(
            "valueOf",
            0,
            NativeCall::Static(bigint_proto_value_of),
            Attr::builtin_function(),
        )?;
    }
    // BigInt is callable-only — use `new_static`, not
    // `new_constructor_static`, so `new BigInt(x)` triggers the
    // §10.1.10 [[Construct]]-missing path (TypeError).
    let ctor = NativeFunction::new_static(heap, "BigInt", 1, bigint_ctor_call)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let string_heap = crate::string::StringHeap::default();
    let proto_desc = PropertyDescriptor::data(Value::Object(prototype), false, false, false);
    if !ctor.define_own_property(heap, &string_heap, "prototype", proto_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("prototype"));
    }
    define_static(heap, ctor, "asIntN", 2, bigint_static_as_int_n)?;
    define_static(heap, ctor, "asUintN", 2, bigint_static_as_uint_n)?;
    object::define_own_property(
        prototype,
        heap,
        "constructor",
        PropertyDescriptor::data(Value::NativeFunction(ctor), true, false, true),
    );
    crate::bootstrap::define_global_value(global, heap, entry.name, Value::NativeFunction(ctor));
    Ok(())
}

/// Install `BigInt.prototype[@@toStringTag] = "BigInt"`.
pub fn install_bigint_well_knowns_post_bootstrap(
    heap: &mut otter_gc::GcHeap,
    string_heap: &crate::string::StringHeap,
    global: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    use crate::symbol::WellKnown;

    let Some(Value::NativeFunction(ctor)) = object::get(global, heap, "BigInt") else {
        return Ok(());
    };
    let descriptor = ctor
        .own_property_descriptor(heap, string_heap, "prototype")
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let prototype = match descriptor.and_then(|d| match d.kind {
        crate::object::DescriptorKind::Data {
            value: Value::Object(p),
        } => Some(p),
        _ => None,
    }) {
        Some(p) => p,
        None => return Ok(()),
    };
    let tag = crate::string::JsString::from_str("BigInt", string_heap)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    object::define_own_symbol_property_partial(
        prototype,
        heap,
        &well_known.get(WellKnown::ToStringTag),
        PartialPropertyDescriptor {
            value: Some(Value::String(tag)),
            writable: Some(false),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    );
    Ok(())
}

// ---------------------------------------------------------------
// Constructor + statics
// ---------------------------------------------------------------

fn bigint_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    if ctx.is_construct_call() {
        return Err(NativeError::TypeError {
            name: "BigInt",
            reason: "BigInt is not a constructor".to_string(),
        });
    }
    bigint::dispatch::call(BigIntMethod::Construct, args)
        .map_err(|e| vm_to_native(e, "BigInt"))
}

fn bigint_static_as_int_n(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let _ = ctx;
    bigint::dispatch::call(BigIntMethod::AsIntN, args)
        .map_err(|e| vm_to_native(e, "BigInt.asIntN"))
}

fn bigint_static_as_uint_n(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let _ = ctx;
    bigint::dispatch::call(BigIntMethod::AsUintN, args)
        .map_err(|e| vm_to_native(e, "BigInt.asUintN"))
}

// ---------------------------------------------------------------
// Prototype methods
// ---------------------------------------------------------------

fn bigint_proto_to_string(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let b = match ctx.this_value() {
        Value::BigInt(b) => b.clone(),
        _ => {
            return Err(NativeError::TypeError {
                name: "BigInt.prototype.toString",
                reason: "this is not a BigInt".to_string(),
            });
        }
    };
    let radix = match args.first() {
        None | Some(Value::Undefined) => 10u32,
        Some(Value::Number(n)) => {
            let f = n.as_f64();
            if !f.is_finite() || !(2.0..=36.0).contains(&f) || f.fract() != 0.0 {
                return Err(NativeError::RangeError {
                    name: "BigInt.prototype.toString",
                    reason: "radix must be an integer in [2, 36]".to_string(),
                });
            }
            f as u32
        }
        _ => {
            return Err(NativeError::TypeError {
                name: "BigInt.prototype.toString",
                reason: "radix must be a number".to_string(),
            });
        }
    };
    let rendered = b.as_inner().to_str_radix(radix);
    let string_heap = ctx.interp_mut().string_heap_clone();
    let s = crate::string::JsString::from_str(&rendered, &string_heap)
        .map_err(|_| oom("BigInt.prototype.toString"))?;
    Ok(Value::String(s))
}

fn bigint_proto_value_of(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    match ctx.this_value() {
        Value::BigInt(b) => Ok(Value::BigInt(b.clone())),
        _ => Err(NativeError::TypeError {
            name: "BigInt.prototype.valueOf",
            reason: "this is not a BigInt".to_string(),
        }),
    }
}

// ---------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------

fn define_static(
    heap: &mut otter_gc::GcHeap,
    ctor: NativeFunction,
    name: &'static str,
    length: u8,
    call: crate::native_function::NativeFastFn,
) -> Result<(), JsSurfaceError> {
    let func = NativeFunction::new_static(heap, name, length, call)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let string_heap = crate::string::StringHeap::default();
    let attrs = Attr::builtin_function();
    let desc = PropertyDescriptor::data(
        Value::NativeFunction(func),
        attrs.writable,
        attrs.enumerable,
        attrs.configurable,
    );
    if !ctor.define_own_property(heap, &string_heap, name, desc) {
        return Err(JsSurfaceError::DefinePropertyFailed(name));
    }
    Ok(())
}

fn oom(name: &'static str) -> NativeError {
    NativeError::TypeError {
        name,
        reason: "out of memory".to_string(),
    }
}

fn vm_to_native(err: VmError, name: &'static str) -> NativeError {
    match err {
        VmError::TypeError { message } => NativeError::TypeError { name, reason: message },
        VmError::TypeMismatch => NativeError::TypeError {
            name,
            reason: "type mismatch".to_string(),
        },
        VmError::RangeError { message } => NativeError::RangeError { name, reason: message },
        VmError::SyntaxError { message } => NativeError::SyntaxError { name, reason: message },
        VmError::Uncaught { value } => NativeError::Thrown { name, message: value },
        other => NativeError::TypeError {
            name,
            reason: other.to_string(),
        },
    }
}
