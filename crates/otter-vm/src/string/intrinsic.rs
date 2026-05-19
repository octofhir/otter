//! `String` built-in installer.
//!
//! Owns the full installation of the global `String` constructor:
//! the constructor object, its `[[Prototype]]` chain to
//! `Object.prototype`, the `prototype` object with the hidden
//! `[[StringData]]` slot, the JS-visible static method specs from
//! [`super::statics`], the prototype methods registered through the
//! `intrinsics!` table in [`super::prototype`], and the
//! callable+constructable bridge wired through the dispatch path.
//!
//! # Contents
//! - [`Intrinsic`] — zero-sized type implementing
//!   [`crate::intrinsic_install::BuiltinIntrinsic`] for `String`.
//!
//! # Invariants
//! - `Object` is installed before `String` (see
//!   [`crate::bootstrap::BOOTSTRAP_ENTRIES`] ordering); the installer
//!   reads `globalThis.Object.prototype` directly to wire the
//!   prototype chain. Reordering the bootstrap table would break
//!   this — call sites that move `String` earlier must drop the
//!   `Object.prototype` lookup.
//! - The constructor object's reserved `[[ConstructorNative]]` slot
//!   carries the `String(...)` native and stays distinct from the
//!   ordinary own property surface; static methods land on the
//!   constructor object as ordinary own properties.
//! - The `prototype` carries an empty `[[StringData]]` so
//!   `Object.prototype.toString.call(String.prototype)` reports the
//!   `String` brand and prototype methods recover a string receiver
//!   when invoked through `Reflect.get` on the prototype.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-string-constructor>
//! - <https://tc39.es/ecma262/#sec-string>
//! - <https://tc39.es/ecma262/#sec-properties-of-the-string-prototype-object>
//! - <https://tc39.es/ecma262/#sec-properties-of-the-string-constructor>

use crate::bootstrap::{
    BootstrapFeatures, alloc_object_with_value_roots, native_static_with_value_roots,
};
use crate::intrinsic_install::BuiltinIntrinsic;
use crate::js_surface::{JsSurfaceError, ObjectBuilder};
use crate::object::{self, JsObject};
use crate::{NativeCtx, NativeError, Value};

/// Zero-sized marker type used to install the global `String`
/// constructor through [`BuiltinIntrinsic`]. The actual installer
/// body lives in [`install`].
pub struct Intrinsic;

impl BuiltinIntrinsic for Intrinsic {
    const NAME: &'static str = "String";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;

    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install(heap, global)
    }
}

/// Materialise the global `String` surface.
///
/// # Algorithm
/// 1. Allocate the constructor and prototype objects with explicit
///    GC roots so a GC during allocation can find them.
/// 2. Chain both `[[Prototype]]` links to `Object.prototype` so
///    `Object.prototype.hasOwnProperty` etc. resolve through ordinary
///    property lookup on both objects.
/// 3. Seed `String.prototype` with an empty `[[StringData]]` so
///    prototype methods that fall through to the prototype recover a
///    string receiver and brand checks observe the spec invariant.
/// 4. Install the `String(...)` / `new String(...)` native into the
///    constructor's reserved bridge slot. The native coerces its
///    argument through `ToString` (§7.1.17) for the call form and
///    additionally wraps the result in a `[[StringData]]` object for
///    the construct form, matching §22.1.1.
/// 5. Install `String.prototype` as an own property on the
///    constructor object.
/// 6. Install the static methods declared by [`super::statics`]
///    (`fromCharCode`, `fromCodePoint`) so user code reading
///    `String.fromCharCode` resolves to a real callable rather than
///    `undefined`.
/// 7. Cross-link `String.prototype.constructor` and register the
///    `String` global binding on `globalThis`.
///
/// # Errors
/// - [`JsSurfaceError::OutOfMemory`] — heap exhausted while
///   allocating the constructor object, prototype object, or native
///   function metadata.
/// - [`JsSurfaceError`] propagated from `ObjectBuilder` when
///   installing static method specs.
fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
    let global_root = Value::Object(global);
    let constructor = alloc_object_with_value_roots(heap, &[&global_root])?;
    let constructor_root = Value::Object(constructor);
    let prototype = alloc_object_with_value_roots(heap, &[&global_root, &constructor_root])?;
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(constructor, heap, Some(object_proto));
        object::set_prototype(prototype, heap, Some(object_proto));
    }
    crate::object::set_string_data(
        prototype,
        heap,
        crate::string::JsString::from_str("", &crate::string::StringHeap::default())
            .map_err(|_| JsSurfaceError::OutOfMemory)?,
    );

    let prototype_root = Value::Object(prototype);
    let ctor_native = native_static_with_value_roots(
        heap,
        "String",
        1,
        string_ctor_call,
        &[&global_root, &constructor_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    object::set_constructor_native(constructor, heap, Value::NativeFunction(ctor_native));
    // §22.1.2.3 — `String.prototype` is a non-writable, non-enumerable,
    // non-configurable data property.
    let _ = object::define_own_property(
        constructor,
        heap,
        "prototype",
        crate::object::PropertyDescriptor::data(Value::Object(prototype), false, false, false),
    );
    // §22.1.2 — `String.length` is a non-writable, non-enumerable,
    // configurable data property whose value is 1.
    let _ = object::define_own_property(
        constructor,
        heap,
        "length",
        crate::object::PropertyDescriptor::data(
            Value::Number(crate::number::NumberValue::from_i32(1)),
            false,
            false,
            true,
        ),
    );
    // §22.1.2 — `String.name` is `"String"`, non-writable,
    // non-enumerable, configurable.
    let string_name_value = Value::String(
        crate::JsString::from_str("String", &crate::string::StringHeap::default())
            .map_err(|_| JsSurfaceError::OutOfMemory)?,
    );
    let _ = object::define_own_property(
        constructor,
        heap,
        "name",
        crate::object::PropertyDescriptor::data(string_name_value, false, false, true),
    );

    // §22.1.2 Properties of the String Constructor — install
    // JS-visible static method specs as ordinary own properties.
    {
        let mut builder = ObjectBuilder::from_object_with_value_roots(
            heap,
            constructor,
            vec![global_root.clone(), prototype_root.clone()],
        );
        for spec in super::statics::STRING_STATIC_METHODS {
            builder.method_from_spec(spec)?;
        }
    }

    // §22.1.3 Properties of the String Prototype Object — install
    // JS-visible prototype method specs so `"abc".split` and
    // `Reflect.get(String.prototype, "trim")` resolve to real
    // callables. The compile-time `CallString` opcode keeps using
    // the prototype intrinsic table directly for the hot path.
    {
        let mut builder = ObjectBuilder::from_object_with_value_roots(
            heap,
            prototype,
            vec![global_root.clone(), constructor_root.clone()],
        );
        for spec in super::prototype::STRING_PROTOTYPE_METHODS {
            builder.method_from_spec(spec)?;
        }
    }

    let string_value = Value::Object(constructor);
    object::set(prototype, heap, "constructor", string_value.clone());
    crate::bootstrap::define_global_value(
        global,
        heap,
        <Intrinsic as BuiltinIntrinsic>::NAME,
        string_value,
    );
    Ok(())
}

/// `String(...)` / `new String(...)` native — ECMA-262 §22.1.1.
///
/// # Algorithm
/// 1. Take the first argument or default to `undefined`.
/// 2. If the value is already a primitive (`undefined`, `null`,
///    `Boolean`, `Number`, `BigInt`, `String`, `Symbol`), pass it
///    through unchanged. Otherwise call `ToPrimitive(value, "string")`
///    via the interpreter so `@@toPrimitive` / `toString` / `valueOf`
///    are observable.
/// 3. Dispatch through [`crate::string::dispatch::call`] with
///    [`otter_bytecode::method_id::StringMethod::Construct`]. The
///    dispatcher performs `ToString` and returns a JS string value.
/// 4. For the construct form (`new String(...)`), unwrap the string
///    primitive and install it as the receiver's `[[StringData]]`,
///    yielding a `String` wrapper object.
///
/// # Errors
/// - [`NativeError::TypeError`] — `ToPrimitive` rejected an exotic
///   receiver, the dispatcher could not coerce, or the construct form
///   ran without an object receiver.
/// - [`NativeError::Thrown`] — `ToPrimitive` raised an uncaught JS
///   exception (rethrown without wrapping).
fn string_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let raw = args.first().cloned().unwrap_or(Value::Undefined);
    let string_heap = ctx.interp_mut().string_heap_clone();
    let primitive = match &raw {
        Value::Undefined
        | Value::Null
        | Value::Boolean(_)
        | Value::Number(_)
        | Value::BigInt(_)
        | Value::String(_)
        | Value::Symbol(_) => raw.clone(),
        _ => {
            let (interp, exec) = ctx.interp_mut_and_context();
            let exec = exec.ok_or_else(|| NativeError::TypeError {
                name: "String",
                reason: "missing execution context".to_string(),
            })?;
            interp
                .evaluate_to_primitive(&exec, &raw, crate::abstract_ops::ToPrimitiveHint::String)
                .map_err(|e| match e {
                    crate::VmError::Uncaught { value } => NativeError::Thrown {
                        name: "String",
                        message: value,
                    },
                    other => NativeError::TypeError {
                        name: "String",
                        reason: other.to_string(),
                    },
                })?
        }
    };
    let value = crate::string::dispatch::call(
        otter_bytecode::method_id::StringMethod::Construct,
        std::slice::from_ref(&primitive),
        &string_heap,
    )
    .map_err(|err| NativeError::TypeError {
        name: "String",
        reason: err.to_string(),
    })?;
    if ctx.is_construct_call() {
        let Value::String(string) = value else {
            return Err(NativeError::TypeError {
                name: "String",
                reason: "constructor did not return a string primitive".to_string(),
            });
        };
        let this = ctx.this_value().clone();
        if let Value::Object(obj) = this {
            crate::object::set_string_data(obj, ctx.heap_mut(), string);
            Ok(Value::Object(obj))
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
