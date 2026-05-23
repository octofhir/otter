//! `%Array%` constructor installer.
//!
//! Implements ECMA-262 §23.1 Array Objects: the `Array()` constructor,
//! `Array.from`, `Array.of`, `Array.isArray`, and `Array[Symbol.species]`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-array-constructor>

use crate::bootstrap::{
    BootstrapFeatures, alloc_object_with_value_roots, define_global,
    native_static_with_value_roots,
};
use crate::intrinsic_install::BuiltinIntrinsic;
use crate::js_surface::{JsSurfaceError, ObjectBuilder};
use crate::object::{self, JsObject};
use crate::{Value, array, array_prototype, array_statics, descriptor_value};

fn install_array(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
    use crate::{NativeCtx, NativeError};

    let global_root = Value::object(global);
    let array = alloc_object_with_value_roots(heap, &[&global_root])?;
    let array_root = Value::object(array);
    let prototype = alloc_object_with_value_roots(heap, &[&global_root, &array_root])?;
    // §23.1 — `Array.prototype` is itself an Array exotic object whose
    // `[[Prototype]]` is `%Object.prototype%`. Bootstrap order installs
    // `Object` first, so the realm's Object.prototype is reachable at
    // this point. Linking the chain here keeps the §7.1.1 / §7.1.1.1
    // `ToPrimitive` / `OrdinaryToPrimitive` lookup path working for
    // `Value::Array` operands — without it, `[1,2,3] + ""` walks an
    // empty proto chain and reaches the foundation TypeError ladder.
    // <https://tc39.es/ecma262/#sec-properties-of-the-array-prototype-object>
    if let Some(object_ctor) = object::get(global, heap, "Object").and_then(|v| v.as_object())
        && let Some(object_proto) =
            object::get(object_ctor, heap, "prototype").and_then(|v| v.as_object())
    {
        object::set_prototype(array, heap, Some(object_proto));
        object::set_prototype(prototype, heap, Some(object_proto));
    }
    let _ = object::define_own_property(
        array,
        heap,
        "prototype",
        crate::object::PropertyDescriptor::data(Value::object(prototype), false, false, false),
    );
    {
        let mut builder = ObjectBuilder::from_object_with_value_roots(
            heap,
            array,
            vec![global_root, Value::object(prototype)],
        );
        for method in array_statics::ARRAY_STATIC_METHODS {
            builder.method_from_spec(method)?;
        }
    }
    {
        let mut builder = ObjectBuilder::from_object_with_value_roots(
            heap,
            prototype,
            vec![global_root, array_root],
        );
        for method in array_prototype::ARRAY_PROTOTYPE_METHODS {
            builder.method_from_spec(method)?;
        }
    }

    // §23.1.1.1 Array(...values) — both `Array(…)` and
    // `new Array(…)` reach this callback. Single numeric argument
    // means "pre-sized sparse array of length n"; anything else
    // collects values verbatim.
    // <https://tc39.es/ecma262/#sec-array>
    fn array_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        if !(args.len() == 1 && args.first().is_some_and(|v| v.is_number())) {
            let arr = ctx.array_from_elements(args.iter().cloned()).map_err(|_| {
                NativeError::TypeError {
                    name: "Array",
                    reason: "out of memory while allocating array".to_string(),
                }
            })?;
            apply_array_new_target_prototype(ctx, arr)?;
            return Ok(Value::array(arr));
        }
        let arr =
            ctx.array_from_elements(std::iter::empty())
                .map_err(|_| NativeError::TypeError {
                    name: "Array",
                    reason: "out of memory while allocating array".to_string(),
                })?;
        if let Some(n) = args[0].as_number() {
            let raw = n.as_f64();
            let len = raw as u32;
            if !raw.is_finite() || raw < 0.0 || raw != f64::from(len) {
                return Err(NativeError::RangeError {
                    name: "Array",
                    reason: "Invalid array length".to_string(),
                });
            }
            if len > 0 {
                // `array::set` gap-fills with `Value::Hole`, so
                // writing the trailing slot also fills every index
                // in `[0, len-1)` with a hole.
                let last = (len - 1) as usize;
                ctx.array_set(arr, last, Value::hole())
                    .map_err(|_| NativeError::TypeError {
                        name: "Array",
                        reason: "out of memory while sizing array".to_string(),
                    })?;
            }
            apply_array_new_target_prototype(ctx, arr)?;
            return Ok(Value::array(arr));
        }
        unreachable!("non-numeric Array(...) arguments returned above")
    }

    fn apply_array_new_target_prototype(
        ctx: &mut NativeCtx<'_>,
        arr: array::JsArray,
    ) -> Result<(), NativeError> {
        let Some(new_target) = ctx.new_target().cloned() else {
            return Ok(());
        };
        let proto = if let Some(class) = new_target.as_class_constructor() {
            Some(Value::object(class.prototype(ctx.heap())))
        } else if let Some(obj) = new_target.as_object() {
            object::get(obj, ctx.heap(), "prototype")
                .filter(|value| value.is_object_type() || value.is_proxy())
        } else if let Some(native) = new_target.as_native_function() {
            native
                .own_property_descriptor(ctx.heap_mut(), "prototype")
                .map_err(|err| NativeError::TypeError {
                    name: "Array",
                    reason: err.to_string(),
                })?
                .map(|descriptor| descriptor_value(&descriptor))
                .filter(|value| value.is_object_type() || value.is_proxy())
        } else {
            None
        };
        if let Some(proto) = proto {
            array::set_prototype_override(arr, ctx.heap_mut(), Some(proto));
        }
        Ok(())
    }

    let ctor_native = native_static_with_value_roots(
        heap,
        "Array",
        1,
        array_ctor_call,
        &[&global_root, &array_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    // Wire the callable+constructable bridge as an internal object
    // slot. This must not appear in JS own-property reflection.
    object::set_constructor_native(array, heap, Value::native_function(ctor_native));

    // §23.1.3.1 — `Array.prototype.constructor = Array`, writable,
    // non-enumerable, configurable.
    object::define_own_property(
        prototype,
        heap,
        "constructor",
        crate::object::PropertyDescriptor::data(Value::object(array), true, false, true),
    );

    define_global(global, heap, "Array", Value::object(array));
    Ok(())
}


/// `BuiltinIntrinsic` adapter for the global `Array` constructor.
pub struct Intrinsic;

impl BuiltinIntrinsic for Intrinsic {
    const NAME: &'static str = "Array";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_array(heap, global)
    }
}
