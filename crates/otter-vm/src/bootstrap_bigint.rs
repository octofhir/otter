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
use crate::js_surface::{Attr, JsSurfaceError, ObjectBuilder};
use crate::native_function::{NativeCall, NativeFunction};
use crate::object::{self, JsObject, PartialPropertyDescriptor, PropertyDescriptor};
use crate::{NativeCtx, NativeError, Value, VmError};

/// `BuiltinIntrinsic` adapter for the global `BigInt` constructor.
pub struct Intrinsic;

impl crate::intrinsic_install::BuiltinIntrinsic for Intrinsic {
    const NAME: &'static str = "BigInt";
    const FEATURE: crate::bootstrap::BootstrapFeatures = crate::bootstrap::BootstrapFeatures::CORE;

    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install(heap, global)
    }
}

/// §21.2 BigInt — installer body, called through [`Intrinsic`].
fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
    let global_root = Value::Object(global);
    let prototype = crate::bootstrap::alloc_object_with_value_roots(heap, &[&global_root])?;
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(prototype, heap, Some(object_proto));
    }
    {
        let mut builder =
            ObjectBuilder::from_object_with_value_roots(heap, prototype, vec![global_root.clone()]);
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
    let prototype_root = Value::Object(prototype);
    let ctor = crate::bootstrap::native_static_with_value_roots(
        heap,
        "BigInt",
        1,
        bigint_ctor_call,
        &[&global_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let proto_desc = PropertyDescriptor::data(Value::Object(prototype), false, false, false);
    if !ctor.define_own_property(heap, "prototype", proto_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("prototype"));
    }
    let ctor_roots = vec![global_root.clone(), Value::Object(prototype)];
    define_static(heap, ctor, "asIntN", 2, bigint_static_as_int_n, &ctor_roots)?;
    define_static(
        heap,
        ctor,
        "asUintN",
        2,
        bigint_static_as_uint_n,
        &ctor_roots,
    )?;
    object::define_own_property(
        prototype,
        heap,
        "constructor",
        PropertyDescriptor::data(Value::NativeFunction(ctor), true, false, true),
    );
    crate::bootstrap::define_global_value(
        global,
        heap,
        <Intrinsic as crate::intrinsic_install::BuiltinIntrinsic>::NAME,
        Value::NativeFunction(ctor),
    );
    Ok(())
}

/// Install `BigInt.prototype[@@toStringTag] = "BigInt"`.
pub fn install_bigint_well_knowns_post_bootstrap(
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    use crate::symbol::WellKnown;

    let Some(Value::NativeFunction(ctor)) = object::get(global, heap, "BigInt") else {
        return Ok(());
    };
    let descriptor = ctor
        .own_property_descriptor(heap, "prototype")
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
    let tag = crate::string::JsString::from_str("BigInt", heap)
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
    let coerced = coerce_bigint_call_args(ctx, args, "BigInt")?;
    bigint::dispatch::call(
        ctx.interp_mut().gc_heap_mut(),
        BigIntMethod::Construct,
        &coerced,
    )
    .map_err(|e| vm_to_native(e, "BigInt"))
}

fn bigint_static_as_int_n(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let coerced = coerce_bigint_call_args(ctx, args, "BigInt.asIntN")?;
    bigint::dispatch::call(
        ctx.interp_mut().gc_heap_mut(),
        BigIntMethod::AsIntN,
        &coerced,
    )
    .map_err(|e| vm_to_native(e, "BigInt.asIntN"))
}

fn bigint_static_as_uint_n(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let coerced = coerce_bigint_call_args(ctx, args, "BigInt.asUintN")?;
    bigint::dispatch::call(
        ctx.interp_mut().gc_heap_mut(),
        BigIntMethod::AsUintN,
        &coerced,
    )
    .map_err(|e| vm_to_native(e, "BigInt.asUintN"))
}

/// §7.1.13 ToBigInt step 4 — Array operands flow through
/// `ToPrimitive(hint: number)` which routes through
/// `Array.prototype.toString` = `.join(",")`. The free
/// `bigint::dispatch::call` has no GC access, so we pre-coerce
/// Array arguments to their joined-string form before dispatch.
fn coerce_bigint_call_args(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    name: &'static str,
) -> Result<smallvec::SmallVec<[Value; 4]>, NativeError> {
    let mut out: smallvec::SmallVec<[Value; 4]> = args.iter().cloned().collect();
    for slot in out.iter_mut() {
        match slot {
            Value::Array(arr) => {
                let parts: Vec<String> = {
                    let heap = ctx.heap();
                    crate::array::with_elements(*arr, heap, |elements| {
                        elements
                            .iter()
                            .map(|v| match v {
                                Value::Undefined | Value::Null | Value::Hole => String::new(),
                                other => other.display_string(heap),
                            })
                            .collect()
                    })
                };
                let joined = parts.join(",");
                *slot = Value::String(
                    crate::string::JsString::from_str(&joined, ctx.heap_mut()).map_err(|_| {
                        NativeError::TypeError {
                            name,
                            reason: "out of memory".to_string(),
                        }
                    })?,
                );
            }
            // §7.1.1 ToPrimitive — object / function / proxy operands
            // run through the spec ladder so user `Symbol.toPrimitive` /
            // `valueOf` / `toString` is observable. The result is then
            // re-coerced by the BigInt dispatcher.
            Value::Object(_)
            | Value::Function { .. }
            | Value::Closure(_)
            | Value::NativeFunction(_)
            | Value::BoundFunction(_)
            | Value::ClassConstructor(_)
            | Value::Proxy(_)
            | Value::RegExp(_)
            | Value::Map(_)
            | Value::Set(_) => {
                let (interp, exec) = ctx.interp_mut_and_context();
                let exec = exec.ok_or_else(|| NativeError::TypeError {
                    name,
                    reason: "missing execution context".to_string(),
                })?;
                let primitive = interp
                    .evaluate_to_primitive(
                        &exec,
                        slot,
                        crate::abstract_ops::ToPrimitiveHint::Number,
                    )
                    .map_err(|e| vm_to_native(e, name))?;
                *slot = primitive;
            }
            _ => {}
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------
// Prototype methods
// ---------------------------------------------------------------

fn bigint_proto_to_string(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let b = match ctx.this_value() {
        Value::BigInt(b) => *b,
        Value::Object(obj) => {
            crate::object::bigint_data(*obj, ctx.heap()).ok_or_else(|| NativeError::TypeError {
                name: "BigInt.prototype.toString",
                reason: "this is not a BigInt".to_string(),
            })?
        }
        _ => {
            return Err(NativeError::TypeError {
                name: "BigInt.prototype.toString",
                reason: "this is not a BigInt".to_string(),
            });
        }
    };
    // §21.2.3.4 step 2 — `radix` defaults to 10 when `undefined`,
    // otherwise routes through `ToIntegerOrInfinity`. The spec
    // raises RangeError for `< 2` / `> 36`; non-coercible operands
    // raise TypeError.
    let radix = match args.first() {
        None | Some(Value::Undefined) => 10u32,
        Some(Value::Symbol(_)) => {
            return Err(NativeError::TypeError {
                name: "BigInt.prototype.toString",
                reason: "Cannot convert a Symbol value to a number".to_string(),
            });
        }
        Some(Value::BigInt(_)) => {
            return Err(NativeError::TypeError {
                name: "BigInt.prototype.toString",
                reason: "Cannot convert a BigInt value to a number".to_string(),
            });
        }
        Some(other) => {
            let f = match other {
                Value::Number(n) => n.as_f64(),
                Value::Boolean(true) => 1.0,
                Value::Boolean(false) => 0.0,
                Value::Null => 0.0,
                Value::String(s) => {
                    crate::number::parse::to_number_from_string(&s.to_lossy_string(ctx.heap()))
                        .as_f64()
                }
                _ => f64::NAN,
            };
            let trunc = if f.is_nan() { 0.0 } else { f.trunc() };
            if !trunc.is_finite() || !(2.0..=36.0).contains(&trunc) {
                return Err(NativeError::RangeError {
                    name: "BigInt.prototype.toString",
                    reason: "radix must be an integer in [2, 36]".to_string(),
                });
            }
            trunc as u32
        }
    };
    let rendered = b.with_inner(ctx.heap(), |bi| bi.to_str_radix(radix));

    let s = crate::string::JsString::from_str(&rendered, ctx.heap_mut())
        .map_err(|_| oom("BigInt.prototype.toString"))?;
    Ok(Value::String(s))
}

fn bigint_proto_value_of(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    match ctx.this_value() {
        Value::BigInt(b) => Ok(Value::BigInt(*b)),
        Value::Object(obj) => crate::object::bigint_data(*obj, ctx.heap())
            .map(Value::BigInt)
            .ok_or_else(|| NativeError::TypeError {
                name: "BigInt.prototype.valueOf",
                reason: "this is not a BigInt".to_string(),
            }),
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
    value_roots: &[Value],
) -> Result<(), JsSurfaceError> {
    let ctor_root = Value::NativeFunction(ctor);
    let mut roots = Vec::with_capacity(value_roots.len() + 1);
    roots.push(&ctor_root);
    roots.extend(value_roots.iter());
    let func = crate::bootstrap::native_static_with_value_roots(
        heap,
        name,
        length,
        call,
        roots.as_slice(),
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let attrs = Attr::builtin_function();
    let desc = PropertyDescriptor::data(
        Value::NativeFunction(func),
        attrs.writable,
        attrs.enumerable,
        attrs.configurable,
    );
    if !ctor.define_own_property(heap, name, desc) {
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
        VmError::TypeError { message } => NativeError::TypeError {
            name,
            reason: message,
        },
        VmError::TypeMismatch => NativeError::TypeError {
            name,
            reason: "type mismatch".to_string(),
        },
        VmError::RangeError { message } => NativeError::RangeError {
            name,
            reason: message,
        },
        VmError::SyntaxError { message } => NativeError::SyntaxError {
            name,
            reason: message,
        },
        VmError::Uncaught { value } => NativeError::Thrown {
            name,
            message: value,
        },
        other => NativeError::TypeError {
            name,
            reason: other.to_string(),
        },
    }
}
