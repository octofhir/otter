//! `%Proxy%` constructor installer.
//!
//! Implements ECMA-262 §28 Proxy Objects: the `Proxy` constructor
//! itself, the `Proxy.revocable(target, handler)` factory, and the
//! revocation closure attached to its `revoke` slot.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-proxy-objects>

use crate::bootstrap::{
    BootstrapFeatures, define_global, native_constructor_static_with_value_roots,
    native_static_with_value_roots,
};
use crate::intrinsic_install::BuiltinIntrinsic;
use crate::js_surface::JsSurfaceError;
use crate::object::{self, JsObject, PropertyDescriptor};
use crate::{NativeCtx, NativeError, Value};

fn proxy_target_is_object(value: &Value) -> bool {
    value.is_object_like()
}

fn proxy_target_arg(args: &[Value]) -> Result<Value, NativeError> {
    match args.first() {
        Some(value) if proxy_target_is_object(value) => Ok(*value),
        _ => Err(NativeError::TypeError {
            name: "Proxy",
            reason: "target must be an object".to_string(),
        }),
    }
}

fn proxy_handler_arg(args: &[Value]) -> Result<Value, NativeError> {
    match args.get(1) {
        Some(value) if proxy_target_is_object(value) => Ok(*value),
        _ => Err(NativeError::TypeError {
            name: "Proxy",
            reason: "handler must be an object".to_string(),
        }),
    }
}

fn proxy_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    if !ctx.is_construct_call() {
        return Err(NativeError::TypeError {
            name: "Proxy",
            reason: "constructor requires new".to_string(),
        });
    }
    let target = proxy_target_arg(args)?;
    let handler = proxy_handler_arg(args)?;
    let proxy = crate::proxy::JsProxy::new(ctx.heap_mut(), target, handler).map_err(|_| {
        NativeError::TypeError {
            name: "Proxy",
            reason: "out of memory while allocating proxy".to_string(),
        }
    })?;
    Ok(Value::proxy(proxy))
}

fn proxy_revocable_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let target = proxy_target_arg(args)?;
    let handler = proxy_handler_arg(args)?;
    let proxy = crate::proxy::JsProxy::new(ctx.heap_mut(), target, handler).map_err(|_| {
        NativeError::TypeError {
            name: "Proxy.revocable",
            reason: "out of memory while allocating proxy".to_string(),
        }
    })?;
    let proxy_value = Value::proxy(proxy);
    let revoke = ctx
        .native_value_with_captures(
            "revoke",
            smallvec::smallvec![proxy_value],
            &[],
            &[args],
            move |ctx, _, captures| {
                if let Some(proxy) = captures.first().and_then(|v| v.as_proxy()) {
                    proxy.revoke(ctx.heap_mut());
                }
                Ok(Value::undefined())
            },
        )
        .map_err(|_| NativeError::TypeError {
            name: "Proxy.revocable",
            reason: "out of memory while creating revoke function".to_string(),
        })?;
    let obj = ctx
        .alloc_object_with_roots(&[&proxy_value, &revoke], &[args])
        .map_err(|_| NativeError::TypeError {
            name: "Proxy.revocable",
            reason: "out of memory while creating result object".to_string(),
        })?;
    object::set(obj, ctx.heap_mut(), "proxy", proxy_value);
    object::set(obj, ctx.heap_mut(), "revoke", revoke);
    Ok(Value::object(obj))
}

fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
    let global_root = Value::object(global);
    let proxy_ctor = native_constructor_static_with_value_roots(
        heap,
        "Proxy",
        2,
        proxy_ctor_call,
        &[&global_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let proxy_ctor_root = Value::native_function(proxy_ctor);
    let revocable = native_static_with_value_roots(
        heap,
        "revocable",
        2,
        proxy_revocable_call,
        &[&global_root, &proxy_ctor_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let revocable_desc =
        PropertyDescriptor::data(Value::native_function(revocable), true, false, true);
    if !proxy_ctor.define_own_property(heap, "revocable", revocable_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("revocable"));
    }
    define_global(global, heap, "Proxy", Value::native_function(proxy_ctor));
    Ok(())
}

/// `BuiltinIntrinsic` adapter for the global `Proxy` constructor.
pub struct Intrinsic;

impl BuiltinIntrinsic for Intrinsic {
    const NAME: &'static str = "Proxy";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install(heap, global)
    }
}
