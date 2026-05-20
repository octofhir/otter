//! ECMA-262 §25.3 DataView bootstrap installer.
//!
//! Real callable + constructible `NativeFunction` for `DataView`
//! plus a prototype carrying every spec-listed method and the
//! `buffer` / `byteLength` / `byteOffset` accessors.
//!
//! # Contents
//! - [`install_data_view`] — bootstrap entry.
//! - [`install_data_view_well_knowns_post_bootstrap`] — `@@toStringTag`.
//!
//! # Invariants
//! - `new DataView(buffer, byteOffset?, byteLength?)` runs §25.3.2.1.
//! - Bare `DataView(...)` (without `new`) throws `TypeError`.
//! - 20 prototype methods + 3 read-only accessors live as own
//!   properties on `DataView.prototype`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-dataview-constructor>
//! - <https://tc39.es/ecma262/#sec-properties-of-the-dataview-prototype-object>

use otter_bytecode::method_id::DataViewMethod;
use smallvec::SmallVec;

use crate::binary::data_view_prototype;
use crate::intrinsics::IntrinsicArgs;
use crate::js_surface::{Attr, JsSurfaceError, ObjectBuilder};
use crate::native_function::NativeCall;
use crate::object::{self, JsObject, PartialPropertyDescriptor, PropertyDescriptor};
use crate::{NativeCtx, NativeError, Value, VmError};

const DATA_VIEW_METHODS: &[(&str, u8, crate::native_function::NativeFastFn)] = &[
    ("getInt8", 1, dv_get_int8),
    ("getUint8", 1, dv_get_uint8),
    ("getInt16", 1, dv_get_int16),
    ("getUint16", 1, dv_get_uint16),
    ("getInt32", 1, dv_get_int32),
    ("getUint32", 1, dv_get_uint32),
    ("getFloat32", 1, dv_get_float32),
    ("getFloat64", 1, dv_get_float64),
    ("getBigInt64", 1, dv_get_bigint64),
    ("getBigUint64", 1, dv_get_biguint64),
    ("setInt8", 2, dv_set_int8),
    ("setUint8", 2, dv_set_uint8),
    ("setInt16", 2, dv_set_int16),
    ("setUint16", 2, dv_set_uint16),
    ("setInt32", 2, dv_set_int32),
    ("setUint32", 2, dv_set_uint32),
    ("setFloat32", 2, dv_set_float32),
    ("setFloat64", 2, dv_set_float64),
    ("setBigInt64", 2, dv_set_bigint64),
    ("setBigUint64", 2, dv_set_biguint64),
];

/// `BuiltinIntrinsic` adapter for the global `DataView` constructor.
pub struct Intrinsic;

impl crate::intrinsic_install::BuiltinIntrinsic for Intrinsic {
    const NAME: &'static str = "DataView";
    const FEATURE: crate::bootstrap::BootstrapFeatures = crate::bootstrap::BootstrapFeatures::CORE;

    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install(heap, global)
    }
}

/// §25.3 DataView — installer body, called through [`Intrinsic`].
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
        for (name, length, call) in DATA_VIEW_METHODS {
            builder.method(
                name,
                *length,
                NativeCall::Static(*call),
                Attr::builtin_function(),
            )?;
        }
    }
    // Accessor properties: buffer / byteLength / byteOffset.
    let accessor_roots = vec![global_root.clone()];
    install_accessor(heap, prototype, "buffer", dv_get_buffer, &accessor_roots)?;
    install_accessor(
        heap,
        prototype,
        "byteLength",
        dv_get_byte_length,
        &accessor_roots,
    )?;
    install_accessor(
        heap,
        prototype,
        "byteOffset",
        dv_get_byte_offset,
        &accessor_roots,
    )?;

    let prototype_root = Value::Object(prototype);
    let ctor = crate::bootstrap::native_constructor_static_with_value_roots(
        heap,
        "DataView",
        1,
        data_view_ctor_call,
        &[&global_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let string_heap = crate::string::StringHeap::default();
    let proto_desc = PropertyDescriptor::data(Value::Object(prototype), false, false, false);
    if !ctor.define_own_property(heap, &string_heap, "prototype", proto_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("prototype"));
    }
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

/// Install `DataView.prototype[@@toStringTag] = "DataView"`.
pub fn install_data_view_well_knowns_post_bootstrap(
    heap: &mut otter_gc::GcHeap,
    string_heap: &crate::string::StringHeap,
    global: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    use crate::symbol::WellKnown;

    let Some(Value::NativeFunction(ctor)) = object::get(global, heap, "DataView") else {
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
    let tag = crate::string::JsString::from_str("DataView", string_heap)
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
// Constructor
// ---------------------------------------------------------------

fn data_view_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    if !ctx.is_construct_call() {
        return Err(NativeError::TypeError {
            name: "DataView",
            reason: "constructor requires 'new'".to_string(),
        });
    }
    let value = crate::binary::dispatch::data_view_call(DataViewMethod::Construct, args)
        .map_err(|e| vm_to_native(e, "DataView"))?;
    // §10.1.13 GetPrototypeFromConstructor — derived `super()`
    // construction forwards `new.target`, so the allocated exotic
    // receives `Subclass.prototype` as its observable [[Prototype]].
    // <https://tc39.es/ecma262/#sec-getprototypefromconstructor>
    let needs_proto_override = !matches!(ctx.new_target(), Some(Value::NativeFunction(_)));
    if needs_proto_override
        && let Some(proto) = crate::bootstrap::native_new_target_prototype(ctx, "DataView")?
    {
        ctx.interp_mut()
            .set_non_gc_exotic_prototype_override(&value, Some(proto));
    }
    Ok(value)
}

// ---------------------------------------------------------------
// Prototype methods (thin wrappers over the intrinsic table)
// ---------------------------------------------------------------

fn dv_get_int8(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "getInt8")
}
fn dv_get_uint8(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "getUint8")
}
fn dv_get_int16(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "getInt16")
}
fn dv_get_uint16(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "getUint16")
}
fn dv_get_int32(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "getInt32")
}
fn dv_get_uint32(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "getUint32")
}
fn dv_get_float32(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "getFloat32")
}
fn dv_get_float64(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "getFloat64")
}
fn dv_get_bigint64(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "getBigInt64")
}
fn dv_get_biguint64(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "getBigUint64")
}
fn dv_set_int8(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "setInt8")
}
fn dv_set_uint8(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "setUint8")
}
fn dv_set_int16(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "setInt16")
}
fn dv_set_uint16(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "setUint16")
}
fn dv_set_int32(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "setInt32")
}
fn dv_set_uint32(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "setUint32")
}
fn dv_set_float32(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "setFloat32")
}
fn dv_set_float64(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "setFloat64")
}
fn dv_set_bigint64(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "setBigInt64")
}
fn dv_set_biguint64(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "setBigUint64")
}

fn dispatch_method(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    method_name: &str,
) -> Result<Value, NativeError> {
    let entry = data_view_prototype::lookup(method_name).ok_or_else(|| NativeError::TypeError {
        name: "DataView.prototype",
        reason: format!("method {method_name} missing"),
    })?;
    let receiver = ctx.this_value().clone();
    // §24.3.1.1 GetViewValue / §24.3.1.2 SetViewValue both start with
    // `ToIndex(byteOffset)`, which runs `ToPrimitive(Number)` →
    // `ToIntegerOrInfinity` per spec. For setters the third arg
    // (value) needs ToNumber / ToBigInt before the intrinsic's
    // strict guard runs. Pre-coerce non-primitive args here so user
    // `@@toPrimitive` / `valueOf` / `toString` hooks fire.
    let exec = ctx.execution_context().cloned();
    let small_args: SmallVec<[Value; 4]> = if let Some(exec) = &exec {
        let mut out: SmallVec<[Value; 4]> = args.iter().cloned().collect();
        let coerce_indices: &[usize] = if method_name.starts_with("get") {
            &[0]
        } else if method_name.starts_with("set") {
            &[0, 1]
        } else {
            &[]
        };
        for &idx in coerce_indices {
            let Some(slot) = out.get_mut(idx) else {
                continue;
            };
            if !matches!(
                slot,
                Value::Object(_)
                    | Value::Array(_)
                    | Value::Function { .. }
                    | Value::Closure { .. }
                    | Value::NativeFunction(_)
                    | Value::BoundFunction(_)
                    | Value::ClassConstructor(_)
                    | Value::Proxy(_)
                    | Value::RegExp(_)
            ) {
                continue;
            }
            let interp = ctx.interp_mut();
            let primitive = interp
                .evaluate_to_primitive(exec, slot, crate::abstract_ops::ToPrimitiveHint::Number)
                .map_err(|e| NativeError::TypeError {
                    name: "DataView.prototype",
                    reason: e.to_string(),
                })?;
            *slot = primitive;
        }
        out
    } else {
        args.iter().cloned().collect()
    };
    let (string_heap, allocation_roots) = {
        let interp = ctx.interp_mut();
        (interp.string_heap_clone(), interp.collect_runtime_roots())
    };
    let gc_heap = ctx.heap_mut();
    let mut intrinsic_args = IntrinsicArgs {
        receiver: &receiver,
        args: &small_args,
        string_heap: &string_heap,
        gc_heap,
        allocation_roots: allocation_roots.as_slice(),
    };
    (entry.impl_fn)(&mut intrinsic_args).map_err(|e| intrinsic_to_native(e, method_name))
}

// ---------------------------------------------------------------
// Accessors
// ---------------------------------------------------------------

fn install_accessor(
    heap: &mut otter_gc::GcHeap,
    prototype: JsObject,
    name: &'static str,
    call: crate::native_function::NativeFastFn,
    value_roots: &[Value],
) -> Result<(), JsSurfaceError> {
    let prototype_root = Value::Object(prototype);
    let mut roots = Vec::with_capacity(value_roots.len() + 1);
    roots.push(&prototype_root);
    roots.extend(value_roots.iter());
    let getter =
        crate::bootstrap::native_static_with_value_roots(heap, name, 0, call, roots.as_slice())
            .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let desc = PropertyDescriptor::accessor(Some(Value::NativeFunction(getter)), None, false, true);
    if !object::define_own_property(prototype, heap, name, desc) {
        return Err(JsSurfaceError::DefinePropertyFailed(name));
    }
    Ok(())
}

fn dv_get_buffer(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let view = receiver_dv(ctx, "get DataView.prototype.buffer")?;
    Ok(Value::ArrayBuffer(view.buffer().clone()))
}

fn dv_get_byte_length(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let view = receiver_dv(ctx, "get DataView.prototype.byteLength")?;
    if view.buffer().is_detached() {
        return Ok(Value::Number(crate::number::NumberValue::from_i32(0)));
    }
    Ok(Value::Number(crate::number::NumberValue::from_i32(
        view.byte_length() as i32,
    )))
}

fn dv_get_byte_offset(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let view = receiver_dv(ctx, "get DataView.prototype.byteOffset")?;
    if view.buffer().is_detached() {
        return Ok(Value::Number(crate::number::NumberValue::from_i32(0)));
    }
    Ok(Value::Number(crate::number::NumberValue::from_i32(
        view.byte_offset() as i32,
    )))
}

fn receiver_dv(
    ctx: &NativeCtx<'_>,
    name: &'static str,
) -> Result<crate::binary::data_view::JsDataView, NativeError> {
    match ctx.this_value() {
        Value::DataView(v) => Ok(v.clone()),
        _ => Err(NativeError::TypeError {
            name,
            reason: "this is not a DataView".to_string(),
        }),
    }
}

// ---------------------------------------------------------------
// Error coercion
// ---------------------------------------------------------------

fn intrinsic_to_native(err: crate::intrinsics::IntrinsicError, name: &str) -> NativeError {
    let _ = name;
    NativeError::TypeError {
        name: "DataView.prototype",
        reason: err.to_string(),
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
        other => NativeError::TypeError {
            name,
            reason: other.to_string(),
        },
    }
}
