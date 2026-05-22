//! ECMA-262 §28.1 `Reflect` object — full static surface.
//!
//! Two entry points share one implementation:
//!
//! - [`call`] — typed-dispatch fast path reached from
//!   [`crate::otter_bytecode::Op::ReflectCall`] when the compiler can
//!   prove the receiver is the global `Reflect`. Skips property lookup
//!   and argument-vector allocation.
//! - [`REFLECT_SPEC`] — static [`crate::js_surface::NamespaceSpec`]
//!   installed at bootstrap. Exposes every method as a JS-visible
//!   own property on the `Reflect` namespace object so user code that
//!   extracts a builtin (`const get = Reflect.get`), enumerates with
//!   `Object.getOwnPropertyNames(Reflect)`, or checks
//!   `Reflect.get.length` observes the spec-required descriptor shape.
//!   Each method delegates straight back to [`call`].
//!
//! Internal-method dispatch routes through the shared
//! `Interpreter::ordinary_*_value` helpers so the same `[[Get]]`,
//! `[[Set]]`, `[[HasProperty]]`, `[[GetOwnProperty]]`, `[[Delete]]`,
//! and `[[GetPrototypeOf]]` paths cover ordinary objects, arrays,
//! callables, and `Value::Proxy` (which walks handler traps).
//!
//! # Contents
//! - [`call`] — typed dispatcher keyed by [`ReflectMethod`].
//! - [`REFLECT_SPEC`] — namespace spec consumed by bootstrap.
//!
//! # Invariants
//! - Type-of-Object checks accept any heap-bound value
//!   ([`is_type_object`]) so Reflect mirrors the spec's `Object`
//!   type, not just `Value::Object`.
//! - Property-key coercion (§7.1.19 ToPropertyKey) is shallow at
//!   this slice: primitive keys are stringified, symbols pass
//!   through, and non-primitive keys raise `TypeMismatch`. A future
//!   slice will invoke the full `[[ToPrimitive]]` ladder.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-reflect-object>

use smallvec::SmallVec;

use crate::ExecutionContext;
use crate::js_surface::{Attr, MethodSpec, NamespaceSpec};
use crate::native_function::NativeCall;
use crate::{Interpreter, NativeCtx, NativeError, Value, VmError, VmGetOutcome, VmPropertyKey};

/// Dispatch `Reflect.<name>(args...)`.
///
/// `string_heap` is the runtime heap used for the rare cases
/// where this dispatcher needs to allocate (e.g. `ownKeys` returning
/// a string array). Most paths only forward existing `Value`s and
/// avoid allocation.
///
/// # Errors
/// - [`VmError::TypeMismatch`] for receiver / argument shape errors.
pub fn call(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    method: otter_bytecode::method_id::ReflectMethod,
    args: &[Value],
) -> Result<Value, VmError> {
    use otter_bytecode::method_id::ReflectMethod as M;
    match method {
        // §28.1.2 Reflect.apply(target, thisArgument, argumentsList)
        // <https://tc39.es/ecma262/#sec-reflect.apply>
        M::Apply => {
            let target = args.first().cloned().unwrap_or(Value::undefined());
            if !is_callable(&target, interp.gc_heap()) {
                return Err(VmError::NotCallable);
            }
            let this_value = args.get(1).cloned().unwrap_or(Value::undefined());
            let argv = create_list_from_array_like(interp, context, args.get(2))?;
            interp.run_callable_sync(context, &target, this_value, argv)
        }
        // §28.1.3 Reflect.construct(target, argumentsList[, newTarget])
        // <https://tc39.es/ecma262/#sec-reflect.construct>
        M::Construct => {
            let target = args.first().cloned().unwrap_or(Value::undefined());
            if !is_constructor(&target, context, interp.gc_heap()) {
                return Err(VmError::NotCallable);
            }
            let new_target = args.get(2).cloned().unwrap_or(target);
            if !is_constructor(&new_target, context, interp.gc_heap()) {
                return Err(VmError::NotCallable);
            }
            let argv = create_list_from_array_like(interp, context, args.get(1))?;
            interp.run_construct_sync(context, &target, new_target, argv)
        }
        // §28.1.4 Reflect.defineProperty(target, propertyKey, attributes)
        // <https://tc39.es/ecma262/#sec-reflect.defineproperty>
        M::DefineProperty => {
            let target = expect_object_value(args.first())?;
            let key = coerce_property_key(interp, context, args.get(1))?;
            let attributes = args.get(2).cloned().unwrap_or(Value::undefined());
            let descriptor = interp.evaluate_to_property_descriptor(context, &attributes)?;
            let ok = interp.define_own_property_value(context, &target, &key, descriptor)?;
            Ok(Value::boolean(ok))
        }
        // §28.1.5 Reflect.deleteProperty(target, propertyKey)
        // <https://tc39.es/ecma262/#sec-reflect.deleteproperty>
        M::DeleteProperty => {
            let target = expect_object_value(args.first())?;
            let key = coerce_property_key(interp, context, args.get(1))?;
            let removed = interp.ordinary_delete_value(context, target, &key, 0)?;
            Ok(Value::boolean(removed))
        }
        // §28.1.6 Reflect.get(target, propertyKey[, receiver])
        // <https://tc39.es/ecma262/#sec-reflect.get>
        M::Get => {
            let target = expect_object_value(args.first())?;
            let key = coerce_property_key(interp, context, args.get(1))?;
            let receiver = args.get(2).cloned().unwrap_or(target);
            match interp.ordinary_get_value(context, target, receiver, &key, 0)? {
                VmGetOutcome::Value(v) => Ok(v),
                VmGetOutcome::InvokeGetter { getter } => {
                    let argv: SmallVec<[Value; 8]> = SmallVec::new();
                    interp.run_callable_sync(context, &getter, receiver, argv)
                }
            }
        }
        // §28.1.7 Reflect.getOwnPropertyDescriptor(target, propertyKey)
        // <https://tc39.es/ecma262/#sec-reflect.getownpropertydescriptor>
        M::GetOwnPropertyDescriptor => {
            let target = expect_object_value(args.first())?;
            let key = coerce_property_key(interp, context, args.get(1))?;
            match interp.ordinary_get_own_property_descriptor_value_runtime_rooted(
                context,
                target,
                &key,
                0,
                &[&target],
                &[args],
            )? {
                None => Ok(Value::undefined()),
                Some(desc) => {
                    let flags = desc.flags;
                    let descriptor_roots: Vec<Value> = match &desc.kind {
                        crate::object::DescriptorKind::Data { value } => vec![*value],
                        crate::object::DescriptorKind::Accessor { getter, setter } => vec![
                            (*getter).unwrap_or(Value::undefined()),
                            (*setter).unwrap_or(Value::undefined()),
                        ],
                    };
                    let obj =
                        interp.alloc_runtime_rooted_object_with_roots(&[], &[&descriptor_roots])?;
                    match &desc.kind {
                        crate::object::DescriptorKind::Data { .. } => {
                            let mut external_visit =
                                |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                                    for value in &descriptor_roots {
                                        value.trace_value_slots(visitor);
                                    }
                                };
                            interp.set_property_with_extra_roots(
                                obj,
                                "value",
                                descriptor_roots[0],
                                &mut external_visit,
                            )?;
                            interp.set_property(
                                obj,
                                "writable",
                                Value::Boolean(flags.writable()),
                            )?;
                        }
                        crate::object::DescriptorKind::Accessor { .. } => {
                            let mut external_visit =
                                |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                                    for value in &descriptor_roots {
                                        value.trace_value_slots(visitor);
                                    }
                                };
                            interp.set_property_with_extra_roots(
                                obj,
                                "get",
                                descriptor_roots[0],
                                &mut external_visit,
                            )?;
                            let mut external_visit =
                                |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                                    for value in &descriptor_roots {
                                        value.trace_value_slots(visitor);
                                    }
                                };
                            interp.set_property_with_extra_roots(
                                obj,
                                "set",
                                descriptor_roots[1],
                                &mut external_visit,
                            )?;
                        }
                    }
                    interp.set_property(obj, "enumerable", Value::Boolean(flags.enumerable()))?;
                    interp.set_property(
                        obj,
                        "configurable",
                        Value::Boolean(flags.configurable()),
                    )?;
                    Ok(Value::object(obj))
                }
            }
        }
        // §28.1.8 Reflect.getPrototypeOf(target)
        // <https://tc39.es/ecma262/#sec-reflect.getprototypeof>
        M::GetPrototypeOf => {
            let target = expect_object_value(args.first())?;
            interp.ordinary_get_prototype_value(context, target, 0)
        }
        // §28.1.9 Reflect.has(target, propertyKey)
        // <https://tc39.es/ecma262/#sec-reflect.has>
        M::Has => {
            let target = expect_object_value(args.first())?;
            let key = coerce_property_key(interp, context, args.get(1))?;
            let present = interp.ordinary_has_property_value(context, target, &key, 0)?;
            Ok(Value::boolean(present))
        }
        // §28.1.10 Reflect.isExtensible(target)
        // <https://tc39.es/ecma262/#sec-reflect.isextensible>
        M::IsExtensible => {
            let target = expect_object_value(args.first())?;
            let ext = interp.is_extensible_value(context, &target)?;
            Ok(Value::boolean(ext))
        }
        // §28.1.11 Reflect.ownKeys(target)
        // <https://tc39.es/ecma262/#sec-reflect.ownkeys>
        M::OwnKeys => {
            let target = expect_object_value(args.first())?;
            let keys = interp.own_property_keys_value(context, &target)?;
            Ok(Value::array(
                interp.alloc_runtime_rooted_array_from_values(keys, &[&target], &[])?,
            ))
        }
        // §28.1.12 Reflect.preventExtensions(target)
        // <https://tc39.es/ecma262/#sec-reflect.preventextensions>
        M::PreventExtensions => {
            let target = expect_object_value(args.first())?;
            let ok = interp.prevent_extensions_value(context, &target)?;
            Ok(Value::boolean(ok))
        }
        // §28.1.13 Reflect.set(target, propertyKey, V[, receiver])
        // <https://tc39.es/ecma262/#sec-reflect.set>
        M::Set => {
            let target = expect_object_value(args.first())?;
            let key = coerce_property_key(interp, context, args.get(1))?;
            let value = args.get(2).cloned().unwrap_or(Value::undefined());
            let receiver = args.get(3).cloned().unwrap_or(target);
            // §10.1.9 OrdinarySet with receiver semantics for ordinary
            // object targets. Proxy / non-ordinary targets route
            // through `ordinary_set_data_value` (which dispatches the
            // `set` trap + falls through).
            if let Value::Object(obj) = &target {
                let outcome = {
                    let heap = interp.gc_heap();
                    match &key {
                        VmPropertyKey::Symbol(sym) => {
                            crate::object::resolve_symbol_set(*obj, heap, sym)
                        }
                        _ => crate::object::resolve_set(
                            *obj,
                            heap,
                            key.string_name()
                                .expect("non-symbol key has string spelling"),
                        ),
                    }
                };
                match outcome {
                    crate::object::SetOutcome::InvokeSetter { setter } => {
                        if !is_callable(&setter, interp.gc_heap()) {
                            return Ok(Value::boolean(false));
                        }
                        let argv: SmallVec<[Value; 8]> = smallvec::smallvec![value];
                        interp.run_callable_sync(context, &setter, receiver, argv)?;
                        return Ok(Value::boolean(true));
                    }
                    crate::object::SetOutcome::Reject { .. } => {
                        return Ok(Value::boolean(false));
                    }
                    crate::object::SetOutcome::AssignData => {
                        // §10.1.9 step 4 — data path. Honour receiver:
                        // when target ≠ receiver, the data write lands
                        // on receiver, not target.
                        return Ok(Value::boolean(set_data_on_receiver(
                            interp, context, &target, &key, value, &receiver,
                        )?));
                    }
                }
            }
            let ok = interp.ordinary_set_data_value(context, target, &key, value, receiver, 0)?;
            Ok(Value::boolean(ok))
        }
        // §28.1.14 Reflect.setPrototypeOf(target, prototype)
        // <https://tc39.es/ecma262/#sec-reflect.setprototypeof>
        M::SetPrototypeOf => {
            let target = expect_object_value(args.first())?;
            let proto = match args.get(1) {
                Some(Value::Object(_)) | Some(Value::Proxy(_)) | Some(Value::Null) => {
                    args.get(1).cloned().unwrap_or(Value::null())
                }
                None => Value::null(),
                _ => return Err(VmError::TypeMismatch),
            };
            let ok = interp.set_prototype_value_proxy_aware(context, &target, &proto)?;
            Ok(Value::boolean(ok))
        }
    }
}

/// §10.1.9 OrdinarySet step 5 — data-property write that honours the
/// `receiver` argument. When `receiver` is not an Object, the write
/// is rejected. When the receiver already owns the property, the
/// write goes through `[[DefineOwnProperty]]` with `{value: V}` only
/// (preserving its existing attributes). Otherwise the write creates
/// a fresh data property on the receiver with the spec-default
/// `{writable: true, enumerable: true, configurable: true}`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-ordinarysetwithowndescriptor>
fn set_data_on_receiver(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    _target: &Value,
    key: &VmPropertyKey,
    value: Value,
    receiver: &Value,
) -> Result<bool, VmError> {
    let Value::Object(recv_obj) = receiver else {
        // §10.1.9 step 5.b — non-object receiver rejects.
        return Ok(false);
    };
    let recv_obj = *recv_obj;
    let existing = {
        let heap = interp.gc_heap();
        match key {
            VmPropertyKey::Symbol(sym) => crate::object::lookup_own_symbol(recv_obj, heap, sym),
            _ => crate::object::lookup_own(
                recv_obj,
                heap,
                key.string_name()
                    .expect("non-symbol key has string spelling"),
            ),
        }
    };
    match existing {
        crate::object::PropertyLookup::Accessor { .. } => Ok(false),
        crate::object::PropertyLookup::Data { flags, .. } => {
            if !flags.writable() {
                return Ok(false);
            }
            // §10.1.9 step 5.e.iii — write only `{value: V}`,
            // preserving existing attributes.
            let partial = crate::object::PartialPropertyDescriptor {
                value: Some(value),
                ..Default::default()
            };
            interp.define_own_property_value(context, &Value::Object(recv_obj), key, partial)
        }
        crate::object::PropertyLookup::Absent => {
            // §10.1.9 step 5.f — CreateDataProperty on receiver.
            let descriptor = crate::object::PartialPropertyDescriptor {
                value: Some(value),
                writable: Some(true),
                enumerable: Some(true),
                configurable: Some(true),
                ..Default::default()
            };
            interp.define_own_property_value(context, &Value::Object(recv_obj), key, descriptor)
        }
    }
}

/// `true` when `value` is a member of the spec type Object — anything
/// allocated on the heap. Mirrors §6.1.7. Primitives (Undefined, Null,
/// Boolean, Number, BigInt, String, Symbol, Hole) return `false`.
fn is_type_object(value: &Value) -> bool {
    !matches!(
        value,
        Value::Undefined
            | Value::Null
            | Value::Boolean(_)
            | Value::Number(_)
            | Value::BigInt(_)
            | Value::String(_)
            | Value::Symbol(_)
            | Value::Hole
    )
}

/// Accept any value of spec type Object (§6.1.7). Used by every
/// Reflect entry point whose step 1 is "If Type(target) is not Object,
/// throw a TypeError exception."
fn expect_object_value(arg: Option<&Value>) -> Result<Value, VmError> {
    match arg {
        Some(v) if is_type_object(v) => Ok(*v),
        _ => Err(VmError::TypeMismatch),
    }
}

/// §7.1.19 ToPropertyKey, invoked observably for non-primitive
/// argument values. Primitive inputs short-circuit to their canonical
/// string form (no JS observable coercion), matching the dispatch
/// ladder's behaviour on simple keys.
fn coerce_property_key(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    arg: Option<&Value>,
) -> Result<VmPropertyKey<'static>, VmError> {
    match arg {
        Some(Value::String(s)) => Ok(VmPropertyKey::OwnedString(
            s.to_lossy_string(interp.gc_heap()),
        )),
        Some(Value::Number(n)) => Ok(VmPropertyKey::OwnedString(n.to_display_string())),
        Some(Value::Boolean(b)) => Ok(VmPropertyKey::String(if *b { "true" } else { "false" })),
        Some(Value::Null) => Ok(VmPropertyKey::String("null")),
        Some(Value::Undefined) | None => Ok(VmPropertyKey::String("undefined")),
        Some(Value::Symbol(sym)) => Ok(VmPropertyKey::Symbol(*sym)),
        Some(v) => interp.evaluate_to_property_key(context, v),
    }
}

/// §7.3.18 CreateListFromArrayLike for the array-like arguments that
/// `Reflect.apply` / `Reflect.construct` pass through. Accepts:
/// `undefined`, `null` (empty), `Value::Array`, and ordinary array-likes
/// with `length` + indexed properties via the shared interpreter helper.
fn create_list_from_array_like(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    arg: Option<&Value>,
) -> Result<SmallVec<[Value; 8]>, VmError> {
    match arg {
        // §7.3.18 step 4–5 — when the source is a real Array the
        // dense fast path bypasses ordinary Get walks, but it must
        // still substitute holes (the internal `Value::Hole`
        // sentinel) with the spec-mandated `undefined` from
        // ArrayPrototype's `[[Get]]` fall-through. Without this,
        // `Reflect.apply(fn, null, ['a', , null])` leaks `Hole`
        // through `arguments` and trips later equality / typeof
        // ladders.
        Some(Value::Array(arr)) => Ok(crate::array::with_elements(
            *arr,
            interp.gc_heap(),
            |elements| {
                elements
                    .iter()
                    .map(|v| match v {
                        Value::Hole => Value::undefined(),
                        other => *other,
                    })
                    .collect()
            },
        )),
        Some(v) if is_type_object(v) => {
            // §7.3.18 CreateListFromArrayLike: probe `length` then
            // walk indexed properties.
            interp.create_list_from_array_like(context, *v)
        }
        // §7.3.18 step 1 — non-Object argumentsList throws TypeError.
        _ => Err(VmError::TypeError {
            message: "argumentsList must be an object".to_string(),
        }),
    }
}

/// Static namespace spec installed by bootstrap.
///
/// Every method is configurable, writable, non-enumerable per §28.1
/// (the same descriptor shape used by `Object` statics). `length`
/// values come from the algorithm headers in the spec.
pub static REFLECT_SPEC: NamespaceSpec = NamespaceSpec {
    name: "Reflect",
    methods: REFLECT_METHODS,
    accessors: &[],
    constants: &[],
    attrs: Attr::global_binding(),
};

/// `BuiltinIntrinsic` adapter for the global `Reflect` namespace.
/// Mirrors §28.1: ordinary namespace object with own data
/// properties for each spec method, `[[Prototype]]` linked to
/// `%Object.prototype%`.
pub struct Intrinsic;

impl crate::intrinsic_install::BuiltinIntrinsic for Intrinsic {
    const NAME: &'static str = REFLECT_SPEC.name;
    const FEATURE: crate::bootstrap::BootstrapFeatures = crate::bootstrap::BootstrapFeatures::CORE;

    fn install(
        heap: &mut otter_gc::GcHeap,
        global: crate::object::JsObject,
    ) -> Result<(), crate::js_surface::JsSurfaceError> {
        let global_root = crate::Value::Object(global);
        let namespace = crate::js_surface::NamespaceBuilder::from_spec_with_value_roots(
            heap,
            &REFLECT_SPEC,
            vec![global_root],
        )?
        .build()?;
        if let Some(crate::Value::Object(object_ctor)) = crate::object::get(global, heap, "Object")
            && let Some(crate::Value::Object(object_proto)) =
                crate::object::get(object_ctor, heap, "prototype")
        {
            crate::object::set_prototype(namespace, heap, Some(object_proto));
        }
        crate::bootstrap::define_global_value(
            global,
            heap,
            <Self as crate::intrinsic_install::BuiltinIntrinsic>::NAME,
            crate::Value::Object(namespace),
        );
        Ok(())
    }
}

const REFLECT_METHODS: &[MethodSpec] = &[
    method("apply", 3, native_apply),
    method("construct", 2, native_construct),
    method("defineProperty", 3, native_define_property),
    method("deleteProperty", 2, native_delete_property),
    method("get", 2, native_get),
    method(
        "getOwnPropertyDescriptor",
        2,
        native_get_own_property_descriptor,
    ),
    method("getPrototypeOf", 1, native_get_prototype_of),
    method("has", 2, native_has),
    method("isExtensible", 1, native_is_extensible),
    method("ownKeys", 1, native_own_keys),
    method("preventExtensions", 1, native_prevent_extensions),
    method("set", 3, native_set),
    method("setPrototypeOf", 2, native_set_prototype_of),
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

fn native_apply(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(ctx, otter_bytecode::method_id::ReflectMethod::Apply, args)
}

fn native_construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(
        ctx,
        otter_bytecode::method_id::ReflectMethod::Construct,
        args,
    )
}

fn native_define_property(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(
        ctx,
        otter_bytecode::method_id::ReflectMethod::DefineProperty,
        args,
    )
}

fn native_delete_property(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(
        ctx,
        otter_bytecode::method_id::ReflectMethod::DeleteProperty,
        args,
    )
}

fn native_get(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(ctx, otter_bytecode::method_id::ReflectMethod::Get, args)
}

fn native_get_own_property_descriptor(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    invoke(
        ctx,
        otter_bytecode::method_id::ReflectMethod::GetOwnPropertyDescriptor,
        args,
    )
}

fn native_get_prototype_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(
        ctx,
        otter_bytecode::method_id::ReflectMethod::GetPrototypeOf,
        args,
    )
}

fn native_has(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(ctx, otter_bytecode::method_id::ReflectMethod::Has, args)
}

fn native_is_extensible(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(
        ctx,
        otter_bytecode::method_id::ReflectMethod::IsExtensible,
        args,
    )
}

fn native_own_keys(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(ctx, otter_bytecode::method_id::ReflectMethod::OwnKeys, args)
}

fn native_prevent_extensions(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    invoke(
        ctx,
        otter_bytecode::method_id::ReflectMethod::PreventExtensions,
        args,
    )
}

fn native_set(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(ctx, otter_bytecode::method_id::ReflectMethod::Set, args)
}

fn native_set_prototype_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(
        ctx,
        otter_bytecode::method_id::ReflectMethod::SetPrototypeOf,
        args,
    )
}

fn invoke(
    ctx: &mut NativeCtx<'_>,
    method: otter_bytecode::method_id::ReflectMethod,
    args: &[Value],
) -> Result<Value, NativeError> {
    let (interp, context) = ctx.interp_mut_and_context();
    let context = context.ok_or(NativeError::TypeError {
        name: "Reflect",
        reason: "no active execution context".to_string(),
    })?;
    call(interp, &context, method, args).map_err(vm_to_native)
}

fn vm_to_native(err: VmError) -> NativeError {
    match err {
        VmError::TypeMismatch => NativeError::TypeError {
            name: "Reflect",
            reason: "type mismatch".to_string(),
        },
        VmError::TypeError { message } => NativeError::TypeError {
            name: "Reflect",
            reason: message,
        },
        VmError::SyntaxError { message } => NativeError::SyntaxError {
            name: "Reflect",
            reason: message,
        },
        VmError::RangeError { message } => NativeError::RangeError {
            name: "Reflect",
            reason: message,
        },
        VmError::NotCallable => NativeError::TypeError {
            name: "Reflect",
            reason: "value is not a function".to_string(),
        },
        VmError::Uncaught { value } => NativeError::Thrown {
            name: "Reflect",
            message: value,
        },
        VmError::OutOfMemory { .. } => NativeError::TypeError {
            name: "Reflect",
            reason: "out of memory".to_string(),
        },
        VmError::Exit { code } => NativeError::Exit { code },
        other => NativeError::TypeError {
            name: "Reflect",
            reason: other.to_string(),
        },
    }
}

fn is_callable(value: &Value, heap: &otter_gc::GcHeap) -> bool {
    match value {
        Value::Object(obj) => matches!(
            crate::object::call_native(*obj, heap),
            Some(Value::NativeFunction(_))
        ),
        _ => crate::abstract_ops::is_callable(value),
    }
}

fn is_constructor(value: &Value, context: &ExecutionContext, heap: &otter_gc::GcHeap) -> bool {
    match value {
        Value::Object(obj) => matches!(
            crate::object::constructor_native(*obj, heap),
            Some(Value::NativeFunction(_))
        ),
        _ => crate::abstract_ops::is_constructor(value, context, heap),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::{
        BytecodeModule, Function, Instruction, SourceKind as BcSourceKind, SpanEntry,
        method_id::ReflectMethod,
    };

    fn empty_context() -> ExecutionContext {
        ExecutionContext::from_module(BytecodeModule {
            module: "reflect-test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![Function {
                id: 0,
                name: "<main>".to_string(),
                span: (0, 0),
                locals: 0,
                scratch: 0,
                param_count: 0,
                own_upvalue_count: 0,
                is_strict: false,
                is_arrow: false,
                has_rest: false,
                is_async: false,
                is_generator: false,
                is_async_generator: false,
                is_module: false,
                needs_arguments: false,
                arguments_object_kind: crate::ArgumentsObjectKind::Unmapped,
                mapped_argument_bindings: Vec::new(),
                module_url: String::new(),
                code: Vec::<Instruction>::new(),
                spans: Vec::<SpanEntry>::new(),
            }],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        })
    }

    #[test]
    fn get_own_property_descriptor_uses_runtime_rooted_object_allocation() {
        let mut interp = Interpreter::new();
        let target =
            crate::object::alloc_object_old_for_fixture(interp.gc_heap_mut()).expect("target");
        crate::object::set(
            target,
            interp.gc_heap_mut(),
            "answer",
            Value::Number(crate::NumberValue::from_i32(42)),
        );
        let context = empty_context();
        let key =
            Value::String(crate::JsString::from_str("answer", interp.gc_heap_mut()).expect("key"));
        let before = interp.gc_heap().stats().new_allocated_bytes;

        let result = call(
            &mut interp,
            &context,
            ReflectMethod::GetOwnPropertyDescriptor,
            &[Value::Object(target), key],
        )
        .expect("descriptor");

        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Reflect.getOwnPropertyDescriptor should allocate descriptor objects in young space"
        );
        assert!(matches!(result, Value::Object(_)));
    }

    #[test]
    fn own_keys_uses_runtime_rooted_array_allocation() {
        let mut interp = Interpreter::new();
        let target =
            crate::object::alloc_object_old_for_fixture(interp.gc_heap_mut()).expect("target");
        crate::object::set(target, interp.gc_heap_mut(), "a", Value::Boolean(true));
        let context = empty_context();
        let before = interp.gc_heap().stats().new_allocated_bytes;

        let result = call(
            &mut interp,
            &context,
            ReflectMethod::OwnKeys,
            &[Value::Object(target)],
        )
        .expect("ownKeys");

        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Reflect.ownKeys should allocate the key array in young space"
        );
        assert!(matches!(result, Value::Array(_)));
    }
}
