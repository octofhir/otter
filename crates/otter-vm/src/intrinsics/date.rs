//! `%Date%` constructor installer.
//!
//! Implements ECMA-262 §21.4 Date Objects: the `Date()` constructor and
//! its prototype wiring.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-date-objects>

use crate::bootstrap::{
    BootstrapFeatures, alloc_object_with_value_roots, define_global,
    native_static_with_value_roots,
};
use crate::intrinsic_install::BuiltinIntrinsic;
use crate::js_surface::{Attr, JsSurfaceError, ObjectBuilder};
use crate::object::{self, JsObject, PropertyDescriptor};
use crate::Value;

fn install_date(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
    use crate::js_surface::MethodSpec;
    use crate::native_function::NativeCall;
    use crate::{JsString, NativeCtx, NativeError};

    // §21.4.3 Date statics — trampolines that route to the typed
    // dispatcher with no `this`. The constructor's
    // `[[Construct]]` / `[[Call]]` slot still handles the
    // `Date(...)` and `new Date(...)` shapes via `date_ctor_call`.
    fn date_now_call(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
        crate::date::dispatch::call_static(
            otter_bytecode::method_id::DateMethod::Now,
            &[],
            ctx.heap(),
        )
        .map_err(|err| NativeError::TypeError {
            name: "Date.now",
            reason: err.to_string(),
        })
    }
    fn date_parse_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        crate::date::dispatch::call_static(
            otter_bytecode::method_id::DateMethod::Parse,
            args,
            ctx.heap(),
        )
        .map_err(|err| NativeError::TypeError {
            name: "Date.parse",
            reason: err.to_string(),
        })
    }
    fn date_utc_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        crate::date::dispatch::call_static(
            otter_bytecode::method_id::DateMethod::UTC,
            args,
            ctx.heap(),
        )
        .map_err(|err| NativeError::TypeError {
            name: "Date.UTC",
            reason: err.to_string(),
        })
    }

    fn date_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        let time = {
            let heap = ctx.heap_mut();
            crate::date::dispatch::construct_time_value(args, heap)
        };
        if ctx.is_construct_call() {
            // §21.4.2.1 — `new Date(...)`. The construct receiver
            // is already a freshly allocated JsObject (via
            // OrdinaryCreateFromConstructor on `Date`). Install
            // the `[[DateValue]]` internal slot and return it.
            if let Some(obj) = ctx.this_value().as_object() {
                crate::object::set_date_data(obj, ctx.heap_mut(), time);
                return Ok(Value::object(obj));
            }
            return Err(NativeError::TypeError {
                name: "Date",
                reason: "expected object receiver in `new Date(...)`".to_string(),
            });
        }
        // §21.4.2.2 — `Date()` without `new` returns the current
        // time rendered as an ISO string.
        let text = crate::date::to_iso_string(time).unwrap_or_else(|| "Invalid Date".to_string());

        let value =
            JsString::from_str(&text, ctx.heap_mut()).map_err(|err| NativeError::TypeError {
                name: "Date",
                reason: err.to_string(),
            })?;
        Ok(Value::string(value))
    }

    let global_root = Value::object(global);
    let constructor = alloc_object_with_value_roots(heap, &[&global_root])?;
    let constructor_root = Value::object(constructor);
    let prototype = alloc_object_with_value_roots(heap, &[&global_root, &constructor_root])?;
    if let Some(object_ctor) = object::get(global, heap, "Object").and_then(|v| v.as_object())
        && let Some(object_proto) =
            object::get(object_ctor, heap, "prototype").and_then(|v| v.as_object())
    {
        object::set_prototype(constructor, heap, Some(object_proto));
        object::set_prototype(prototype, heap, Some(object_proto));
    }
    let prototype_root = Value::object(prototype);
    let ctor_native = native_static_with_value_roots(
        heap,
        "Date",
        7,
        date_ctor_call,
        &[&global_root, &constructor_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    object::set_constructor_native(constructor, heap, Value::native_function(ctor_native));
    let _ = object::define_own_property(
        constructor,
        heap,
        "prototype",
        PropertyDescriptor::data(Value::object(prototype), false, false, false),
    );

    // §21.4.4 Properties of the Date Prototype Object — install
    // JS-visible prototype method specs so `(new Date()).getTime`
    // resolves to a callable. The compile-time `CallDate` opcode
    // keeps using the prototype intrinsic table directly.
    {
        let mut builder = ObjectBuilder::from_object_with_value_roots(
            heap,
            prototype,
            vec![global_root, constructor_root],
        );
        for spec in crate::date::prototype::DATE_PROTOTYPE_METHODS
            .iter()
            .chain(crate::date::prototype::DATE_PROTOTYPE_EXTRA_METHODS)
        {
            builder.method_from_spec(spec)?;
        }
    }

    // §21.4.3 statics — `Date.now()`, `Date.parse(str)`, `Date.UTC(...)`.
    {
        let mut builder = ObjectBuilder::from_object_with_value_roots(
            heap,
            constructor,
            vec![global_root, prototype_root],
        );
        builder.method_from_spec(&MethodSpec {
            name: "now",
            length: 0,
            attrs: Attr::builtin_function(),
            call: NativeCall::Static(date_now_call),
        })?;
        builder.method_from_spec(&MethodSpec {
            name: "parse",
            length: 1,
            attrs: Attr::builtin_function(),
            call: NativeCall::Static(date_parse_call),
        })?;
        builder.method_from_spec(&MethodSpec {
            name: "UTC",
            length: 7,
            attrs: Attr::builtin_function(),
            call: NativeCall::Static(date_utc_call),
        })?;
    }

    let date_value = Value::object(constructor);
    let _ = object::define_own_property(
        prototype,
        heap,
        "constructor",
        PropertyDescriptor::data(date_value, true, false, true),
    );
    define_global(global, heap, "Date", date_value);
    Ok(())
}

/// `BuiltinIntrinsic` adapter for the global `Date` constructor.
pub struct Intrinsic;

impl BuiltinIntrinsic for Intrinsic {
    const NAME: &'static str = "Date";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_date(heap, global)
    }
}
