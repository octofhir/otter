//! `Object.<static>` dispatcher — handles the descriptor-shaped
//! surface (`defineProperty`, `getOwnPropertyDescriptor`, `freeze`,
//! `seal`, `preventExtensions`, the `is*` predicates) wired through
//! [`crate::otter_bytecode::Op::ObjectCall`]. Routed by name; unknown
//! names raise [`VmError::UnknownIntrinsic`].
//!
//! Construction-time built-ins (`create`, `getPrototypeOf`,
//! `setPrototypeOf`, `is`) keep their own dedicated opcodes so this
//! file only owns the descriptor / integrity ladder.
//!
//! # Contents
//! - [`call`] — single entry point used by the dispatch loop.
//! - [`coerce_to_descriptor`] — implements §6.2.5.5
//!   `ToPropertyDescriptor` against a JS-side descriptor object.
//!
//! # Invariants
//! - All names match ECMA-262 spelling exactly.
//! - Reads of the descriptor object's `value / writable / enumerable
//!   / configurable / get / set` slots use direct own-data reads
//!   ([`crate::object::lookup_own`]). User-installed accessors / inherited
//!   descriptor fields are intentionally ignored in this slice; the
//!   spec uses the full `[[Get]]` ladder, but the ergonomic surface
//!   (`Object.defineProperty(o, 'k', { value: 1 })`) doesn't depend
//!   on it. Filed against task 60 for full faithfulness.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-object-constructor>
//! - <https://tc39.es/ecma262/#sec-topropertydescriptor>
//! - <https://tc39.es/ecma262/#sec-setintegritylevel>

use crate::js_surface::{Attr, MethodSpec, NamespaceSpec};
use crate::native_function::NativeCall;
use crate::object::{
    DescriptorKind, JsObject, PartialPropertyDescriptor, PropertyDescriptor, PropertyLookup,
};
use crate::string::JsString;
use crate::symbol::JsSymbol;
use crate::{NativeCtx, NativeError, Value, VmError};

enum PropertyKey {
    String(String),
    Symbol(JsSymbol),
}

impl PropertyKey {
    fn label(&self, heap: &otter_gc::GcHeap) -> String {
        match self {
            Self::String(key) => key.clone(),
            Self::Symbol(sym) => sym.descriptive_string(heap),
        }
    }
}

/// Static `Object` constructor-shaped surface installed by bootstrap.
///
/// The active compiler still lowers direct `Object.<name>(...)`
/// calls to [`crate::otter_bytecode::Op::ObjectCall`]. This spec
/// installs the same functions as JS-visible properties so `typeof
/// Object.hasOwn`, descriptor helpers, and extracted calls observe a
/// real builtin function value too.
pub static OBJECT_SPEC: NamespaceSpec = NamespaceSpec {
    name: "Object",
    methods: OBJECT_STATIC_METHODS,
    accessors: &[],
    constants: &[],
    attrs: Attr::global_binding(),
};

const OBJECT_STATIC_METHODS: &[MethodSpec] = &[
    method("is", 2, native_is),
    method("getPrototypeOf", 1, native_get_prototype_of),
    method("setPrototypeOf", 2, native_set_prototype_of),
    method("create", 2, native_create),
    method("defineProperty", 3, native_define_property),
    method("defineProperties", 2, native_define_properties),
    method(
        "getOwnPropertyDescriptor",
        2,
        native_get_own_property_descriptor,
    ),
    method(
        "getOwnPropertyDescriptors",
        1,
        native_get_own_property_descriptors,
    ),
    method("freeze", 1, native_freeze),
    method("isFrozen", 1, native_is_frozen),
    method("seal", 1, native_seal),
    method("isSealed", 1, native_is_sealed),
    method("preventExtensions", 1, native_prevent_extensions),
    method("isExtensible", 1, native_is_extensible),
    method("keys", 1, native_keys),
    method("values", 1, native_values),
    method("entries", 1, native_entries),
    method("assign", 2, native_assign),
    method("fromEntries", 1, native_from_entries),
    method("hasOwn", 2, native_has_own),
    method("getOwnPropertyNames", 1, native_get_own_property_names),
    method("getOwnPropertySymbols", 1, native_get_own_property_symbols),
    method("groupBy", 2, native_group_by),
];

/// Static methods installed on `Object.prototype`.
pub static OBJECT_PROTOTYPE_METHODS: &[MethodSpec] = &[
    method("toString", 0, native_prototype_to_string),
    method("toLocaleString", 0, native_prototype_to_locale_string),
    method("valueOf", 0, native_prototype_value_of),
    method("hasOwnProperty", 1, native_prototype_has_own_property),
    method(
        "propertyIsEnumerable",
        1,
        native_prototype_property_is_enumerable,
    ),
    method("isPrototypeOf", 1, native_prototype_is_prototype_of),
    method("__defineGetter__", 2, native_prototype_define_getter),
    method("__defineSetter__", 2, native_prototype_define_setter),
    method("__lookupGetter__", 1, native_prototype_lookup_getter),
    method("__lookupSetter__", 1, native_prototype_lookup_setter),
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

fn native_call(
    method: otter_bytecode::method_id::ObjectMethod,
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let context = ctx.execution_context().cloned();

    // Accessor-aware spec helpers route through the shared
    // `do_object_*` paths whenever an ExecutionContext is available;
    // the operands dispatcher uses the same helpers.
    let coerced_args: Option<smallvec::SmallVec<[Value; 4]>> = if let Some(context) =
        context.as_ref()
    {
        use otter_bytecode::method_id::ObjectMethod as M;
        match method {
            M::Create => {
                return ctx
                    .cx
                    .interp
                    .do_object_create_with_descriptors(context, None, args)
                    .map_err(|err| object_native_error(method.name(), err));
            }
            M::DefineProperties => {
                return ctx
                    .cx
                    .interp
                    .do_object_define_properties(context, None, args)
                    .map_err(|err| object_native_error(method.name(), err));
            }
            M::Assign => {
                return ctx
                    .cx
                    .interp
                    .do_object_assign(context, None, args)
                    .map_err(|err| object_native_error(method.name(), err));
            }
            M::GetOwnPropertyDescriptor | M::HasOwn => {
                // §20.1.2.10 / §20.1.2.13 step 1 — ToObject(O) throws
                // for null / undefined before any key coercion.
                if args.first().is_none_or(|v| v.is_nullish()) {
                    return Err(NativeError::TypeError {
                        name: method.name(),
                        reason: "Object static method called on null or undefined".to_string(),
                    });
                }
                // §20.1.2.10 / §20.1.2.13 step 2 — accessor-aware
                // ToPropertyKey(P) may invoke user `Symbol.toPrimitive`
                // / `toString` / `valueOf`; route through the context-
                // aware path only for non-trivially-coerced inputs so
                // the spec error ordering matches the operands path.
                let key_arg = args.get(1).cloned().unwrap_or(Value::undefined());
                let needs_coercion = !(key_arg.is_string()
                    || key_arg.is_number()
                    || key_arg.is_boolean()
                    || key_arg.is_null()
                    || key_arg.is_undefined()
                    || key_arg.is_symbol());
                if needs_coercion {
                    let coerced_key = ctx
                        .cx
                        .interp
                        .evaluate_to_property_key(context, &key_arg)
                        .map_err(|err| object_native_error(method.name(), err))?;
                    let coerced_value = match &coerced_key {
                        crate::VmPropertyKey::Symbol(sym) => Value::symbol(*sym),
                        other => Value::string(
                            crate::string::JsString::from_str(
                                other
                                    .string_name()
                                    .expect("non-symbol key has string spelling"),
                                ctx.heap_mut(),
                            )
                            .map_err(|_| NativeError::TypeError {
                                name: method.name(),
                                reason: "out of memory".to_string(),
                            })?,
                        ),
                    };
                    let mut rewritten: smallvec::SmallVec<[Value; 4]> =
                        args.iter().cloned().collect();
                    if rewritten.len() >= 2 {
                        rewritten[1] = coerced_value;
                    } else {
                        rewritten.push(coerced_value);
                    }
                    Some(rewritten)
                } else {
                    None
                }
            }
            _ => None,
        }
    } else {
        None
    };
    let args: &[Value] = coerced_args.as_deref().unwrap_or(args);

    // Function / Proxy spec ladders go first so they observe the
    // canonical descriptor / trap behaviour.
    if let Some(result) = ctx
        .cx
        .interp
        .try_function_object_static_call(context.as_ref(), None, method, args)
        .map_err(|err| object_native_error(method.name(), err))?
    {
        return Ok(result);
    }
    if let Some(context) = context.as_ref()
        && let Some(result) = ctx
            .cx
            .interp
            .try_proxy_object_static_call(context, None, method, args)
            .map_err(|err| object_native_error(method.name(), err))?
    {
        return Ok(result);
    }

    // Full spec-aware dispatch (primitive coercion, accessor
    // enumeration, string-exotic indexed slots, …) lives in the
    // operand-dispatcher helper. Reuse it with an empty frame stack
    // so the alloc helpers fall back to runtime-rooted allocation
    // and `args` survives via slice_roots.
    if let Some(context) = context.as_ref()
        && let Some(result) = ctx
            .cx
            .interp
            .object_static_call_no_stack(context, method, args)
            .map_err(|err| object_native_error(method.name(), err))?
    {
        return Ok(result);
    }

    call(method, args, ctx.heap_mut()).map_err(|err| object_native_error(method.name(), err))
}

fn set_from_entries_key_heap(
    target: crate::object::JsObject,
    key: &Value,
    value: Value,
    heap: &mut otter_gc::GcHeap,
) -> Result<(), VmError> {
    if let Some(sym) = key.as_symbol(heap) {
        crate::object::set_symbol(target, heap, sym, value);
        return Ok(());
    }
    let key_str = property_key_from_value(key, heap)?;
    crate::object::set(target, heap, &key_str, value);
    Ok(())
}

/// §20.1.2.7 step 5.b — read indices `"0"` and `"1"` from an entry
/// candidate via the spec `[[Get]]`. Heap-only variant for the
/// context-less `object_statics::call` path. Accepts Array pairs,
/// ordinary Objects with indexed keys, and String / String-wrapper
/// entries.
fn read_entry_pair_heap(
    entry: &Value,
    heap: &mut otter_gc::GcHeap,
) -> Result<(Value, Value), VmError> {
    if let Some(pair) = entry.as_array() {
        return Ok((
            crate::array::get(pair, heap, 0),
            crate::array::get(pair, heap, 1),
        ));
    }
    if let Some(obj) = entry.as_object() {
        if let Some(s) = crate::object::string_data(obj, heap) {
            let units = s.to_utf16_vec(heap);
            let zero = units.first().copied().map_or(Value::undefined(), |u| {
                crate::string::JsString::from_utf16_units(&[u], heap)
                    .map(Value::string)
                    .unwrap_or(Value::undefined())
            });
            let one = units.get(1).copied().map_or(Value::undefined(), |u| {
                crate::string::JsString::from_utf16_units(&[u], heap)
                    .map(Value::string)
                    .unwrap_or(Value::undefined())
            });
            return Ok((zero, one));
        }
        let key = crate::object::get(obj, heap, "0").unwrap_or(Value::undefined());
        let value = crate::object::get(obj, heap, "1").unwrap_or(Value::undefined());
        return Ok((key, value));
    }
    if let Some(s) = entry.as_string(heap) {
        let units = s.to_utf16_vec(heap);
        let zero = units.first().copied().map_or(Value::undefined(), |u| {
            crate::string::JsString::from_utf16_units(&[u], heap)
                .map(Value::string)
                .unwrap_or(Value::undefined())
        });
        let one = units.get(1).copied().map_or(Value::undefined(), |u| {
            crate::string::JsString::from_utf16_units(&[u], heap)
                .map(Value::string)
                .unwrap_or(Value::undefined())
        });
        return Ok((zero, one));
    }
    Err(VmError::TypeMismatch)
}

fn class_constructor_own_property_keys_without_context(
    class: &crate::ClassConstructor,
    gc_heap: &otter_gc::GcHeap,
) -> Result<Vec<String>, VmError> {
    let ctor = class.ctor(gc_heap);
    let mut keys = if let Some(native) = ctor.as_native_function() {
        native.own_property_keys(gc_heap)
    } else if let Some(bound) = ctor.as_bound_function() {
        crate::function_metadata::bound_own_property_keys(&bound, gc_heap)
    } else if let Some(inner) = ctor.as_class_constructor() {
        class_constructor_own_property_keys_without_context(&inner, gc_heap)?
    } else if ctor.is_function() || ctor.is_closure() {
        return Err(VmError::InvalidOperand);
    } else {
        Vec::new()
    };
    if !keys.iter().any(|key| key == "prototype") {
        keys.push("prototype".to_string());
    }
    for key in crate::object::with_properties(class.statics(gc_heap), gc_heap, |p| {
        p.keys().map(str::to_string).collect::<Vec<_>>()
    }) {
        if !keys.iter().any(|existing| existing == &key) {
            keys.push(key);
        }
    }
    Ok(keys)
}

fn object_native_error(name: &'static str, err: VmError) -> NativeError {
    match err {
        VmError::Uncaught { value } => NativeError::Thrown {
            name,
            message: value,
        },
        VmError::TypeError { message } => NativeError::TypeError {
            name,
            reason: message,
        },
        VmError::RangeError { message } => NativeError::RangeError {
            name,
            reason: message,
        },
        VmError::SyntaxError { message } => NativeError::SyntaxError {
            name,
            reason: message,
        },
        other => NativeError::TypeError {
            name,
            reason: other.to_string(),
        },
    }
}

macro_rules! native_object_static {
    ($fn_name:ident, $variant:ident) => {
        fn $fn_name(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            native_call(otter_bytecode::method_id::ObjectMethod::$variant, ctx, args)
        }
    };
}

native_object_static!(native_create, Create);
native_object_static!(native_define_property, DefineProperty);
native_object_static!(native_define_properties, DefineProperties);
native_object_static!(native_get_own_property_descriptor, GetOwnPropertyDescriptor);
native_object_static!(
    native_get_own_property_descriptors,
    GetOwnPropertyDescriptors
);
native_object_static!(native_freeze, Freeze);
native_object_static!(native_is_frozen, IsFrozen);
native_object_static!(native_seal, Seal);
native_object_static!(native_is_sealed, IsSealed);
native_object_static!(native_prevent_extensions, PreventExtensions);
native_object_static!(native_is_extensible, IsExtensible);
native_object_static!(native_keys, Keys);
native_object_static!(native_values, Values);
native_object_static!(native_entries, Entries);
native_object_static!(native_assign, Assign);
native_object_static!(native_from_entries, FromEntries);
native_object_static!(native_has_own, HasOwn);
native_object_static!(native_get_own_property_names, GetOwnPropertyNames);
native_object_static!(native_get_own_property_symbols, GetOwnPropertySymbols);
native_object_static!(native_group_by, GroupBy);

/// §20.1.2.13 `Object.is(value1, value2)` — direct §7.2.11 SameValue.
///
/// Mirrors the compile-time `Op::SameValue` lowering so callers that
/// read the property as a value (e.g.
/// `Object.getOwnPropertyDescriptor(Object, "is").value`) and then
/// invoke it through `.call` / `Reflect.apply` see the spec result.
fn native_is(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let a = args.first().cloned().unwrap_or(Value::undefined());
    let b = args.get(1).cloned().unwrap_or(Value::undefined());
    Ok(Value::boolean(crate::abstract_ops::same_value(
        &a,
        &b,
        ctx.heap(),
    )))
}

/// §20.1.2.12 `Object.getPrototypeOf(O)` — `[[Prototype]]` of `O`
/// after ToObject coercion. Primitive operands resolve to their
/// respective `%X.prototype%` per §7.1.18.
fn native_get_prototype_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let target = args.first().cloned().unwrap_or(Value::undefined());
    let exec_ctx = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "Object.getPrototypeOf",
            reason: "missing execution context".to_string(),
        })?;
    let closure_id = target.as_closure(ctx.heap()).map(|c| c.cached_function_id);
    let interp = ctx.interp_mut();
    let function_id = target.as_function().or(closure_id);
    if let Some(function_id) = function_id
        && let Some(proto) = interp.function_kind_prototype_for(&exec_ctx, function_id)
    {
        return Ok(Value::object(proto));
    }
    interp
        .get_prototype_for_op(&target)
        .map_err(|err| object_native_error("Object.getPrototypeOf", err))
}

/// §20.1.2.21 `Object.setPrototypeOf(O, proto)` — assigns the
/// `[[Prototype]]` of `O` to `proto` (which must be Object or Null)
/// and returns `O` after ToObject coercion checks.
fn native_set_prototype_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let target = args.first().cloned().unwrap_or(Value::undefined());
    let proto = args.get(1).cloned().unwrap_or(Value::undefined());
    if target.is_nullish() {
        return Err(NativeError::TypeError {
            name: "Object.setPrototypeOf",
            reason: "Object.setPrototypeOf called on null or undefined".to_string(),
        });
    }
    if !proto.is_null() && !proto.is_object_type() {
        return Err(NativeError::TypeError {
            name: "Object.setPrototypeOf",
            reason: "Object.setPrototypeOf prototype must be an Object or null".to_string(),
        });
    }
    if !target.is_object_type() {
        return Ok(target);
    }
    let exec_ctx = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "Object.setPrototypeOf",
            reason: "missing execution context".to_string(),
        })?;
    let ok = ctx
        .cx
        .interp
        .set_prototype_value_proxy_aware(&exec_ctx, &target, &proto)
        .map_err(|err| object_native_error("Object.setPrototypeOf", err))?;
    if !ok {
        return Err(NativeError::TypeError {
            name: "Object.setPrototypeOf",
            reason: "Object.setPrototypeOf failed".to_string(),
        });
    }
    Ok(target)
}

fn native_prototype_to_string(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let exec_ctx = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "toString",
            reason: "missing execution context".to_string(),
        })?;
    // §20.1.3.6 computes `builtinTag` before the observable
    // `Get(O, @@toStringTag)` call, so proxy revocation triggered by
    // the tag getter cannot retroactively change the builtin tag.
    let builtin_tag = builtin_to_string_tag(ctx);
    let explicit_tag = explicit_to_string_tag_with_context(ctx, &exec_ctx)
        .map_err(|err| object_native_error("toString", err))?;
    let tag = match explicit_tag {
        Some(t) => t,
        None => builtin_tag,
    };
    let display = format!("[object {tag}]");

    Ok(Value::string(
        JsString::from_str(&display, ctx.heap_mut()).map_err(|_| NativeError::TypeError {
            name: "toString",
            reason: "out of memory while allocating string".to_string(),
        })?,
    ))
}

fn native_prototype_value_of(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    object_prototype_to_object(ctx, "valueOf")
}

fn object_prototype_to_object(
    ctx: &mut NativeCtx<'_>,
    method_name: &'static str,
) -> Result<Value, NativeError> {
    let this_value = *ctx.this_value();
    let (proto_name, setter): (&str, fn(JsObject, &mut otter_gc::GcHeap, &Value)) =
        if this_value.is_nullish() {
            return Err(NativeError::TypeError {
                name: method_name,
                reason: "cannot convert null or undefined to object".to_string(),
            });
        } else if this_value.is_boolean() {
            ("Boolean", set_primitive_wrapper_data)
        } else if this_value.is_number() {
            ("Number", set_primitive_wrapper_data)
        } else if this_value.is_string() {
            ("String", set_primitive_wrapper_data)
        } else if this_value.is_symbol() {
            ("Symbol", set_primitive_wrapper_data)
        } else if this_value.is_big_int() {
            ("BigInt", set_primitive_wrapper_data)
        } else {
            return Ok(this_value);
        };
    let proto = ctx
        .cx
        .interp
        .constructor_prototype_value(proto_name)
        .ok()
        .and_then(|v| v.as_object())
        .or_else(|| ctx.cx.interp.object_prototype_object_opt());
    let wrapper = ctx.alloc_object_with_roots(&[&this_value], &[])?;
    if let Some(proto) = proto {
        crate::object::set_prototype(wrapper, ctx.heap_mut(), Some(proto));
    }
    setter(wrapper, ctx.heap_mut(), &this_value);
    Ok(Value::object(wrapper))
}

fn set_primitive_wrapper_data(wrapper: JsObject, heap: &mut otter_gc::GcHeap, value: &Value) {
    if let Some(b) = value.as_boolean() {
        crate::object::set_boolean_data(wrapper, heap, b);
    } else if let Some(n) = value.as_number() {
        crate::object::set_number_data(wrapper, heap, n);
    } else if let Some(s) = value.as_string(heap) {
        crate::object::set_string_data(wrapper, heap, s);
    } else if let Some(sym) = value.as_symbol(heap) {
        crate::object::set_symbol_data(wrapper, heap, sym);
    } else if let Some(bi) = value.as_big_int() {
        crate::object::set_bigint_data(wrapper, heap, bi);
    }
}

/// §20.1.3.5 `Object.prototype.toLocaleString ( [ reserved1 [ , reserved2 ] ] )`.
///
/// 1. Let `O` be the this value.
/// 2. Return `? Invoke(O, "toString")`.
///
/// Routes through the `Invoke` ladder so user-installed
/// `Boolean.prototype.toString` / `Number.prototype.toString` /
/// other receiver-side overrides are observable. Falls back to
/// `Object.prototype.toString` when no execution context is wired
/// (sync-only fast path used by some embedders).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-object.prototype.tolocalestring>
fn native_prototype_to_locale_string(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let this_value = *ctx.this_value();
    if let Some(context) = ctx.execution_context().cloned() {
        let callee = ctx
            .cx
            .interp
            .get_property_value_for_call(&context, this_value, "toString")
            .map_err(|err| object_native_error("toLocaleString", err))?;
        if crate::is_callable_value(&callee) {
            let result = ctx
                .cx
                .interp
                .run_callable_sync(&context, &callee, this_value, smallvec::SmallVec::new())
                .map_err(|err| object_native_error("toLocaleString", err))?;
            return Ok(result);
        }
    }
    native_prototype_to_string(ctx, args)
}

fn native_prototype_has_own_property(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let this_value = *ctx.this_value();
    if let Some(context) = ctx.execution_context().cloned() {
        let key = ctx
            .cx
            .interp
            .to_property_key_sync(
                &context,
                args.first().cloned().unwrap_or(Value::undefined()),
            )
            .map_err(|err| object_native_error("hasOwnProperty", err))?;
        if this_value.is_nullish() {
            return Err(NativeError::TypeError {
                name: "hasOwnProperty",
                reason: "cannot convert null or undefined to object".to_string(),
            });
        }
        let desc = ctx
            .cx
            .interp
            .ordinary_get_own_property_descriptor_value_runtime_rooted(
                &context,
                this_value,
                &key,
                0,
                &[&this_value],
                &[],
            )
            .map_err(|err| object_native_error("hasOwnProperty", err))?;
        return Ok(Value::boolean(desc.is_some()));
    }
    if this_value.is_nullish() {
        return Err(NativeError::TypeError {
            name: "hasOwnProperty",
            reason: "cannot convert null or undefined to object".to_string(),
        });
    }
    let this_kind = *ctx.this_value();
    let present = if let Some(obj) = this_kind.as_object() {
        has_own_property(obj, ctx.heap(), args.first())
            .map_err(|err| object_native_error("hasOwnProperty", err))?
    } else if let Some(native) = this_kind.as_native_function() {
        native_function_has_own(&native, ctx.heap_mut(), args.first())
    } else if let Some(bound) = this_kind.as_bound_function() {
        bound_function_has_own(&bound, ctx.heap(), args.first())
    } else if let Some(class) = this_kind.as_class_constructor() {
        let key = args.first();
        let is_prototype_key = key
            .and_then(|v| v.as_string(ctx.heap()))
            .is_some_and(|s| s.to_lossy_string(ctx.heap()) == "prototype");
        if is_prototype_key {
            true
        } else {
            has_own_property(class.statics(ctx.heap()), ctx.heap(), key)
                .map_err(|err| object_native_error("hasOwnProperty", err))?
        }
    } else {
        false
    };
    Ok(Value::boolean(present))
}

fn native_prototype_property_is_enumerable(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let this_value = *ctx.this_value();
    if this_value.is_nullish() {
        return Err(NativeError::TypeError {
            name: "propertyIsEnumerable",
            reason: "cannot convert null or undefined to object".to_string(),
        });
    }
    if let Some(context) = ctx.execution_context().cloned() {
        let desc = ctx
            .cx
            .interp
            .get_own_property_descriptor_for_value(&context, this_value, args.first())
            .map_err(|err| object_native_error("propertyIsEnumerable", err))?;
        return Ok(Value::boolean(
            desc.as_ref().is_some_and(PropertyDescriptor::enumerable),
        ));
    }
    let this_clone = *ctx.this_value();
    let enumerable = if let Some(obj) = this_clone.as_object() {
        let key = expect_property_key(args.first(), ctx.heap())
            .map_err(|err| object_native_error("propertyIsEnumerable", err))?;
        match key {
            PropertyKey::String(key) => match crate::object::lookup_own(obj, ctx.heap(), &key) {
                PropertyLookup::Data { flags, .. } | PropertyLookup::Accessor { flags, .. } => {
                    flags.enumerable()
                }
                PropertyLookup::Absent => false,
            },
            PropertyKey::Symbol(sym) => {
                match crate::object::lookup_own_symbol(obj, ctx.heap(), sym) {
                    PropertyLookup::Data { flags, .. } | PropertyLookup::Accessor { flags, .. } => {
                        flags.enumerable()
                    }
                    PropertyLookup::Absent => false,
                }
            }
        }
    } else if let Some(native) = this_clone.as_native_function() {
        let key_owned = expect_property_key(args.first(), ctx.heap())
            .map_err(|err| object_native_error("propertyIsEnumerable", err))?;
        let desc = match key_owned {
            PropertyKey::String(key) => native
                .own_property_descriptor(ctx.heap_mut(), &key)
                .map_err(|err| object_native_error("propertyIsEnumerable", err.into()))?,
            PropertyKey::Symbol(sym) => native.own_symbol_property_descriptor(ctx.heap(), sym),
        };
        desc.as_ref().is_some_and(PropertyDescriptor::enumerable)
    } else if let Some(bound) = this_clone.as_bound_function() {
        let key = expect_property_key(args.first(), ctx.heap())
            .map_err(|err| object_native_error("propertyIsEnumerable", err))?;
        match key {
            PropertyKey::String(key) => {
                crate::function_metadata::bound_own_property_is_enumerable(&bound, ctx.heap(), &key)
            }
            PropertyKey::Symbol(_) => false,
        }
    } else {
        false
    };
    Ok(Value::boolean(enumerable))
}

fn native_prototype_is_prototype_of(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let this_value = *ctx.this_value();
    let target = args.first().cloned().unwrap_or(Value::undefined());
    if !target.is_object_type() {
        return Ok(Value::boolean(false));
    }
    if this_value.is_nullish() {
        return Err(NativeError::TypeError {
            name: "isPrototypeOf",
            reason: "cannot convert null or undefined to object".to_string(),
        });
    }
    if !this_value.is_object_type() {
        return Ok(Value::boolean(false));
    }
    let exec_ctx = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "isPrototypeOf",
            reason: "missing execution context".to_string(),
        })?;
    let mut current = target;
    for _ in 0..crate::object::PROTO_CHAIN_HARD_CAP {
        let proto = ctx
            .cx
            .interp
            .ordinary_get_prototype_value(&exec_ctx, current, 0)
            .map_err(|err| object_native_error("isPrototypeOf", err))?;
        if proto.is_null() {
            return Ok(Value::boolean(false));
        }
        if crate::abstract_ops::same_value(&this_value, &proto, ctx.heap()) {
            return Ok(Value::boolean(true));
        }
        current = proto;
    }
    Ok(Value::boolean(false))
}

/// §B.2.2.1.1 `get Object.prototype.__proto__` — returns the
/// receiver's `[[Prototype]]`.
///
/// 1. Let `O` be `? ToObject(this value)`.
/// 2. Return `? O.[[GetPrototypeOf]]()`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-get-object.prototype.__proto__>
pub fn native_prototype_proto_get(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let this_value = *ctx.this_value();
    if this_value.is_nullish() {
        return Err(NativeError::TypeError {
            name: "get __proto__",
            reason: "cannot convert null or undefined to object".to_string(),
        });
    }
    // §B.2.2.1.1 step 2 — `Return ? O.[[GetPrototypeOf]]()`. Proxy
    // and other exotic receivers must dispatch through their
    // `[[GetPrototypeOf]]` so user `getPrototypeOf` traps fire
    // observably.
    if let Some(exec_ctx) = ctx.execution_context().cloned() {
        let result = ctx
            .cx
            .interp
            .ordinary_get_prototype_value(&exec_ctx, this_value, 0)
            .map_err(|err| object_native_error("get __proto__", err))?;
        return Ok(result);
    }
    // Context-less fallback (sync embedders without a JS frame).
    if let Some(o) = this_value.as_object() {
        return Ok(crate::object::prototype_value(o, ctx.heap()).unwrap_or(Value::null()));
    }
    let recv = ctx.this_value();
    let name = if recv.is_boolean() {
        "Boolean"
    } else if recv.is_number() {
        "Number"
    } else if recv.is_string() {
        "String"
    } else if recv.is_symbol() {
        "Symbol"
    } else if recv.is_big_int() {
        "BigInt"
    } else {
        return Ok(Value::null());
    };
    Ok(ctx
        .cx
        .interp
        .constructor_prototype_value(name)
        .unwrap_or(Value::null()))
}

/// §B.2.2.1.2 `set Object.prototype.__proto__` — installs a new
/// `[[Prototype]]`.
///
/// 1. Let `O` be `? RequireObjectCoercible(this value)`.
/// 2. If `Type(proto)` is neither Object nor Null, return undefined.
/// 3. If `Type(O)` is not Object, return undefined.
/// 4. Let `status` be `? O.[[SetPrototypeOf]](proto)`.
/// 5. If `status` is false, throw a TypeError.
/// 6. Return undefined.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-set-object.prototype.__proto__>
pub fn native_prototype_proto_set(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let this_value = *ctx.this_value();
    if this_value.is_nullish() {
        return Err(NativeError::TypeError {
            name: "set __proto__",
            reason: "cannot convert null or undefined to object".to_string(),
        });
    }
    let proto_value = args.first().cloned().unwrap_or(Value::undefined());
    // §B.2.2.1.2 step 2 — only Object / Null proto values are
    // honoured; everything else returns undefined without
    // mutating. Proxy-as-prototype is admissible via the broader
    // value lattice.
    if !(proto_value.is_object() || proto_value.is_null() || proto_value.is_proxy()) {
        return Ok(Value::undefined());
    }
    // §B.2.2.1.2 step 3 — non-object receivers silently no-op.
    if !this_value.is_object() || this_value.is_proxy() {
        return Ok(Value::undefined());
    }
    // §20.1.3 — `Object.prototype` is an immutable-prototype
    // exotic. Reject any change that would diverge from its
    // current `[[Prototype]]` so
    // `Object.prototype.__proto__ = X` throws TypeError unless
    // `X` already matches.
    if let Some(obj) = this_value.as_object() {
        let object_proto = ctx.cx.interp.object_prototype_object_opt();
        if object_proto == Some(obj) {
            let current = crate::object::prototype_value(obj, ctx.heap()).unwrap_or(Value::null());
            if !crate::abstract_ops::same_value(&proto_value, &current, ctx.heap()) {
                return Err(NativeError::TypeError {
                    name: "set __proto__",
                    reason: "Immutable prototype object cannot have its prototype changed"
                        .to_string(),
                });
            }
            return Ok(Value::undefined());
        }
    }
    let exec_ctx = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "set __proto__",
            reason: "missing execution context".to_string(),
        })?;
    let ok = ctx
        .cx
        .interp
        .set_prototype_value_proxy_aware(&exec_ctx, &this_value, &proto_value)
        .map_err(|err| object_native_error("set __proto__", err))?;
    if !ok {
        return Err(NativeError::TypeError {
            name: "set __proto__",
            reason: "cyclic or non-extensible prototype chain".to_string(),
        });
    }
    Ok(Value::undefined())
}

/// §B.2.2.2 `Object.prototype.__defineGetter__(P, getter)`.
///
/// 1. Let `O` be `? ToObject(this value)`.
/// 2. If `IsCallable(getter)` is false, throw a TypeError.
/// 3. Let `desc` be `PropertyDescriptor { [[Get]]: getter, [[Enumerable]]: true,
///    [[Configurable]]: true }`.
/// 4. Let `key` be `? ToPropertyKey(P)`.
/// 5. Perform `? DefinePropertyOrThrow(O, key, desc)`.
/// 6. Return undefined.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-object.prototype.__defineGetter__>
fn native_prototype_define_getter(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    define_accessor_helper(ctx, args, /* is_setter */ false, "__defineGetter__")
}

/// §B.2.2.3 `Object.prototype.__defineSetter__(P, setter)`.
///
/// Mirror of [`native_prototype_define_getter`] for `[[Set]]`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-object.prototype.__defineSetter__>
fn native_prototype_define_setter(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    define_accessor_helper(ctx, args, /* is_setter */ true, "__defineSetter__")
}

fn define_accessor_helper(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    is_setter: bool,
    method_name: &'static str,
) -> Result<Value, NativeError> {
    let this_value = *ctx.this_value();
    if this_value.is_nullish() {
        return Err(NativeError::TypeError {
            name: method_name,
            reason: "cannot convert null or undefined to object".to_string(),
        });
    }
    let callable = args.get(1).cloned().unwrap_or(Value::undefined());
    if !crate::is_callable_value(&callable) {
        return Err(NativeError::TypeError {
            name: method_name,
            reason: "argument is not a function".to_string(),
        });
    }
    let key = native_to_property_key(ctx, args.first(), method_name)?;
    let desc = if is_setter {
        PartialPropertyDescriptor {
            set: Some(callable),
            enumerable: Some(true),
            configurable: Some(true),
            ..Default::default()
        }
    } else {
        PartialPropertyDescriptor {
            get: Some(callable),
            enumerable: Some(true),
            configurable: Some(true),
            ..Default::default()
        }
    };
    if let Some(exec_ctx) = ctx.execution_context().cloned()
        && this_value.is_object_type()
    {
        let key = property_key_to_vm_key(&key);
        let ok = ctx
            .cx
            .interp
            .define_own_property_value(&exec_ctx, &this_value, &key, desc)
            .map_err(|err| object_native_error(method_name, err))?;
        if !ok {
            return Err(NativeError::TypeError {
                name: method_name,
                reason: "cannot redefine property".to_string(),
            });
        }
        return Ok(Value::undefined());
    }

    let Some(target) = this_value.as_object() else {
        // §7.1.18 ToObject — primitives wrap. The accessor lands on
        // the transient wrapper which is discarded once the call
        // returns, mirroring V8/JSC.
        return Ok(Value::undefined());
    };
    let ok = match key {
        PropertyKey::String(name) => {
            crate::object::define_own_property_partial(target, ctx.heap_mut(), &name, desc)
        }
        PropertyKey::Symbol(sym) => {
            crate::object::define_own_symbol_property_partial(target, ctx.heap_mut(), sym, desc)
        }
    };
    if !ok {
        return Err(NativeError::TypeError {
            name: method_name,
            reason: "cannot redefine property".to_string(),
        });
    }
    Ok(Value::undefined())
}

/// §B.2.2.4 `Object.prototype.__lookupGetter__(P)`.
///
/// 1. Let `O` be `? ToObject(this value)`.
/// 2. Let `key` be `? ToPropertyKey(P)`.
/// 3. Repeat:
///    a. Let `desc` be `? O.[[GetOwnProperty]](key)`.
///    b. If `desc` is not undefined, return the getter for an accessor
///    descriptor and otherwise return undefined.
///    c. Let `O` be `? O.[[GetPrototypeOf]]()`.
///    d. If `O` is null, return undefined.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-object.prototype.__lookupGetter__>
fn native_prototype_lookup_getter(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    lookup_accessor_helper(
        ctx,
        args,
        /* lookup_setter */ false,
        "__lookupGetter__",
    )
}

/// §B.2.2.5 `Object.prototype.__lookupSetter__(P)`. Mirror for `[[Set]]`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-object.prototype.__lookupSetter__>
fn native_prototype_lookup_setter(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    lookup_accessor_helper(ctx, args, /* lookup_setter */ true, "__lookupSetter__")
}

fn lookup_accessor_helper(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    lookup_setter: bool,
    method_name: &'static str,
) -> Result<Value, NativeError> {
    let this_value = *ctx.this_value();
    if this_value.is_nullish() {
        return Err(NativeError::TypeError {
            name: method_name,
            reason: "cannot convert null or undefined to object".to_string(),
        });
    }
    let key = native_to_property_key(ctx, args.first(), method_name)?;
    if let Some(exec_ctx) = ctx.execution_context().cloned() {
        let key = property_key_to_vm_key(&key);
        let mut current = match this_value {
            value if value.is_object_type() => Some(value),
            _ => return Ok(Value::undefined()),
        };
        while let Some(value) = current {
            let desc = ctx
                .cx
                .interp
                .ordinary_get_own_property_descriptor_value_runtime_rooted(
                    &exec_ctx,
                    value,
                    &key,
                    0,
                    &[&value],
                    &[],
                )
                .map_err(|err| object_native_error(method_name, err))?;
            if let Some(desc) = desc {
                return Ok(match desc.kind {
                    DescriptorKind::Accessor { getter, setter } => {
                        if lookup_setter {
                            setter.unwrap_or(Value::undefined())
                        } else {
                            getter.unwrap_or(Value::undefined())
                        }
                    }
                    DescriptorKind::Data { .. } => Value::undefined(),
                });
            }
            let proto = ctx
                .cx
                .interp
                .ordinary_get_prototype_value(&exec_ctx, value, 0)
                .map_err(|err| object_native_error(method_name, err))?;
            current = if proto.is_null() { None } else { Some(proto) };
        }
        return Ok(Value::undefined());
    }

    let Some(start) = this_value.as_object() else {
        return Ok(Value::undefined());
    };
    let mut current = Some(start);
    while let Some(obj) = current {
        let lookup = match &key {
            PropertyKey::String(name) => crate::object::lookup_own(obj, ctx.heap(), name),
            PropertyKey::Symbol(sym) => crate::object::lookup_own_symbol(obj, ctx.heap(), *sym),
        };
        match lookup {
            PropertyLookup::Accessor { getter, setter, .. } => {
                let value = if lookup_setter { setter } else { getter };
                return Ok(value.unwrap_or(Value::undefined()));
            }
            PropertyLookup::Data { .. } => return Ok(Value::undefined()),
            PropertyLookup::Absent => {
                current = crate::object::prototype(obj, ctx.heap());
            }
        }
    }
    Ok(Value::undefined())
}

fn property_key_to_vm_key(key: &PropertyKey) -> crate::VmPropertyKey<'static> {
    match key {
        PropertyKey::String(name) => crate::VmPropertyKey::OwnedString(name.clone()),
        PropertyKey::Symbol(sym) => crate::VmPropertyKey::Symbol(*sym),
    }
}

/// §20.1.3.6 step 14 — `builtinTag` table. Only the internal-slot
/// driven tags surface here (Array / Arguments / Function / Error /
/// Boolean / Number / String / Date / RegExp); every other kind
/// (Map, Set, Promise, BigInt, Symbol, TypedArray, …) falls back to
/// `"Object"` and relies on a prototype-installed `@@toStringTag`
/// for its kind-specific string.
fn builtin_to_string_tag(ctx: &NativeCtx<'_>) -> String {
    let v = ctx.this_value();
    if v.is_undefined() || v.is_hole() {
        return "Undefined".to_string();
    }
    if v.is_null() {
        return "Null".to_string();
    }
    if v.is_boolean() {
        return "Boolean".to_string();
    }
    if v.is_number() {
        return "Number".to_string();
    }
    if v.is_string() {
        return "String".to_string();
    }
    if v.is_big_int() || v.is_symbol() {
        return "Object".to_string();
    }
    if v.is_function()
        || v.is_closure()
        || v.is_bound_function()
        || v.is_native_function()
        || v.is_class_constructor()
    {
        return "Function".to_string();
    }
    if v.is_array() {
        return "Array".to_string();
    }
    if v.is_regexp() {
        return "RegExp".to_string();
    }
    if v.is_promise()
        || v.is_map()
        || v.is_set()
        || v.is_weak_map()
        || v.is_weak_set()
        || v.is_weak_ref()
        || v.is_finalization_registry()
        || v.is_generator()
        || v.is_iterator()
        || v.is_temporal()
        || v.is_intl()
        || v.is_array_buffer()
        || v.is_data_view()
        || v.is_typed_array()
    {
        return "Object".to_string();
    }
    if v.is_proxy() {
        return proxy_builtin_tag(v, ctx.heap());
    }
    if let Some(obj) = v.as_object() {
        if crate::object::is_arguments_object(obj, ctx.heap()) {
            return "Arguments".to_string();
        }
        if crate::object::date_data(obj, ctx.heap()).is_some() {
            return "Date".to_string();
        }
        if crate::object::call_native(obj, ctx.heap()).is_some() {
            return "Function".to_string();
        }
        if crate::object::boolean_data(obj, ctx.heap()).is_some() {
            return "Boolean".to_string();
        }
        if crate::object::number_data(obj, ctx.heap()).is_some() {
            return "Number".to_string();
        }
        if crate::object::string_data(obj, ctx.heap()).is_some() {
            return "String".to_string();
        }
        if object_has_error_data(ctx, obj) {
            return "Error".to_string();
        }
        return "Object".to_string();
    }
    "Object".to_string()
}

/// §7.2.2 IsArray + §7.2.4 IsCallable for a Proxy target. Walks the
/// `[[ProxyTarget]]` chain until reaching a non-proxy value and
/// returns the builtin tag of that underlying value.
fn proxy_builtin_tag(value: &Value, heap: &otter_gc::GcHeap) -> String {
    let mut current = *value;
    let mut hops = 0_usize;
    loop {
        if hops >= crate::object::PROTO_CHAIN_HARD_CAP {
            return "Object".to_string();
        }
        hops += 1;
        if let Some(p) = current.as_proxy() {
            if p.is_revoked(heap) {
                return "Object".to_string();
            }
            current = p.target(heap);
            continue;
        }
        if current.is_array() {
            return "Array".to_string();
        }
        if current.is_function()
            || current.is_closure()
            || current.is_bound_function()
            || current.is_native_function()
            || current.is_class_constructor()
        {
            return "Function".to_string();
        }
        return "Object".to_string();
    }
}

/// Walk `obj`'s `[[Prototype]]` chain and return `true` when any
/// realm error prototype is reached. Used as a substitute for the
/// spec's `[[ErrorData]]` internal slot, which Otter does not carry
/// on ordinary object instances.
fn object_has_error_data(ctx: &NativeCtx<'_>, obj: crate::object::JsObject) -> bool {
    use crate::ErrorKind;
    let heap = ctx.heap();
    let registry = &ctx.cx.interp.error_classes;
    // §20.5.3 "The Error prototype object does not have an
    // `[[ErrorData]]` internal slot." Treat any of the realm error
    // prototypes as ordinary objects when probed directly — only
    // their (transitive) descendants carry the slot.
    let kinds = [
        ErrorKind::Error,
        ErrorKind::TypeError,
        ErrorKind::RangeError,
        ErrorKind::SyntaxError,
        ErrorKind::ReferenceError,
        ErrorKind::URIError,
        ErrorKind::EvalError,
        ErrorKind::AggregateError,
    ];
    for kind in kinds {
        if registry.prototype(kind) == obj {
            return false;
        }
    }
    let mut current = crate::object::prototype(obj, heap);
    let mut hops = 0;
    while let Some(o) = current {
        if hops >= crate::object::PROTO_CHAIN_HARD_CAP {
            return false;
        }
        hops += 1;
        for kind in kinds {
            if registry.prototype(kind) == o {
                return true;
            }
        }
        current = crate::object::prototype(o, heap);
    }
    false
}

/// §20.1.3.6 step 15 — `Get(O, @@toStringTag)` through the full
/// `[[Get]]` ladder, so accessor getters fire and the realm
/// prototype's tag (`Map.prototype[@@toStringTag]`, etc.) is
/// observed. Non-string results return `None` so the caller falls
/// back to the builtin tag.
fn explicit_to_string_tag_with_context(
    ctx: &mut NativeCtx<'_>,
    exec_ctx: &crate::ExecutionContext,
) -> Result<Option<String>, crate::VmError> {
    // §20.1.3.6 steps 1-2 — `undefined` and `null` resolve to their
    // builtin tags before ToObject and never enter the `[[Get]]`
    // ladder. The `Hole` sentinel never reaches user code, but if it
    // somehow does, behave like `undefined`.
    let this_value = *ctx.this_value();
    if this_value.is_nullish() || this_value.is_hole() {
        return Ok(None);
    }
    let tag_symbol = ctx
        .cx
        .interp
        .well_known_symbols()
        .get(crate::symbol::WellKnown::ToStringTag);
    // string primitive doesn't have its own arm in
    // `ordinary_get_value`; route the lookup through
    // `String.prototype` explicitly so user-installed
    // `String.prototype[@@toStringTag]` overrides surface.
    let base: Value = if this_value.is_string() {
        match ctx.cx.interp.constructor_prototype_value("String").ok() {
            Some(p) => p,
            None => return Ok(None),
        }
    } else {
        this_value
    };
    let outcome = ctx.cx.interp.ordinary_get_value(
        exec_ctx,
        base,
        this_value,
        &crate::VmPropertyKey::Symbol(tag_symbol),
        0,
    )?;
    let value = match outcome {
        crate::VmGetOutcome::Value(v) => v,
        crate::VmGetOutcome::InvokeGetter { getter } => ctx.cx.interp.run_callable_sync(
            exec_ctx,
            &getter,
            this_value,
            smallvec::SmallVec::new(),
        )?,
    };
    Ok(value
        .as_string(ctx.heap())
        .map(|s| s.to_lossy_string(ctx.heap())))
}

fn native_function_has_own(
    native: &crate::NativeFunction,
    gc_heap: &mut otter_gc::GcHeap,
    key: Option<&Value>,
) -> bool {
    match expect_property_key(key, gc_heap) {
        Ok(PropertyKey::String(key)) => native
            .own_property_descriptor(gc_heap, &key)
            .ok()
            .flatten()
            .is_some(),
        Ok(PropertyKey::Symbol(sym)) => native
            .own_symbol_property_descriptor(gc_heap, sym)
            .is_some(),
        Err(_) => false,
    }
}

fn bound_function_has_own(
    bound: &crate::BoundFunction,
    gc_heap: &otter_gc::GcHeap,
    key: Option<&Value>,
) -> bool {
    match expect_property_key(key, gc_heap) {
        Ok(PropertyKey::String(key)) => {
            crate::function_metadata::bound_has_own_property(bound, gc_heap, &key)
        }
        Ok(PropertyKey::Symbol(_)) | Err(_) => false,
    }
}

/// Single entry point for `Object.<method>(args...)` dispatch.
///
/// Routes the typed [`ObjectMethod`] emitted by the compiler — no
/// per-call name match.
///
/// # Errors
/// - [`VmError::TypeMismatch`] when an argument has the wrong shape
///   (e.g., the receiver of `defineProperty` is not an Object).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-properties-of-the-object-constructor>
pub fn call(
    method: otter_bytecode::method_id::ObjectMethod,
    args: &[Value],
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<Value, VmError> {
    use otter_bytecode::method_id::ObjectMethod as M;
    match method {
        // §20.1.2.2 Object.create(O, Properties)
        // <https://tc39.es/ecma262/#sec-object.create>
        M::Create => {
            let proto = args.first().cloned().unwrap_or(Value::undefined());
            let proto_value = if proto.is_object() || proto.is_iterator() {
                Some(proto)
            } else if proto.is_null() {
                None
            } else {
                return Err(VmError::TypeMismatch);
            };
            let obj = rooted_object(gc_heap, &[&proto], &[args])?;
            if !crate::object::set_prototype_value(obj, gc_heap, proto_value) {
                return Err(VmError::TypeMismatch);
            }
            if let Some(props_arg) = args.get(1)
                && !props_arg.is_undefined()
            {
                let props = props_arg.as_object().ok_or(VmError::TypeMismatch)?;
                let entries: Vec<(String, Value)> =
                    crate::object::with_properties(props, gc_heap, |p| {
                        p.enumerable_data_iter()
                            .map(|(k, v)| (k.to_string(), v))
                            .collect()
                    });
                for (key, desc_value) in entries {
                    let desc_obj = desc_value.as_object().ok_or(VmError::TypeMismatch)?;
                    let descriptor = coerce_to_descriptor(&desc_obj, gc_heap)?;
                    if !crate::object::define_own_property_partial(obj, gc_heap, &key, descriptor) {
                        return Err(VmError::TypeMismatch);
                    }
                }
            }
            Ok(Value::object(obj))
        }
        // §20.1.2.4 Object.defineProperty(O, P, Attributes)
        // <https://tc39.es/ecma262/#sec-object.defineproperty>
        M::DefineProperty => {
            let key = expect_property_key(args.get(1), gc_heap)?;
            let desc_obj = expect_object(args.get(2))?;
            let descriptor = coerce_to_descriptor(&desc_obj, gc_heap)?;
            let first = args.first();
            if let Some(target) = first.and_then(|v| v.as_object()) {
                let ok = match &key {
                    PropertyKey::String(key) => {
                        crate::object::define_own_property_partial(target, gc_heap, key, descriptor)
                    }
                    PropertyKey::Symbol(sym) => crate::object::define_own_symbol_property_partial(
                        target, gc_heap, *sym, descriptor,
                    ),
                };
                if !ok {
                    return Err(VmError::TypeError {
                        message: format!("Cannot define property '{}'", key.label(gc_heap)),
                    });
                }
                Ok(Value::object(target))
            } else if let Some(class) = first.and_then(|v| v.as_class_constructor()) {
                let ok = match &key {
                    PropertyKey::String(key) => crate::object::define_own_property_partial(
                        class.statics(gc_heap),
                        gc_heap,
                        key,
                        descriptor,
                    ),
                    PropertyKey::Symbol(sym) => crate::object::define_own_symbol_property_partial(
                        class.statics(gc_heap),
                        gc_heap,
                        *sym,
                        descriptor,
                    ),
                };
                if !ok {
                    return Err(VmError::TypeError {
                        message: format!("Cannot define property '{}'", key.label(gc_heap)),
                    });
                }
                Ok(Value::class_constructor(class))
            } else if let Some(native) = first.and_then(|v| v.as_native_function()) {
                let ok = match &key {
                    PropertyKey::String(key) => native.define_own_property(
                        gc_heap,
                        key,
                        descriptor.complete_for_new_property(),
                    ),
                    PropertyKey::Symbol(sym) => {
                        native.define_own_symbol_property(gc_heap, *sym, descriptor)
                    }
                };
                if !ok {
                    return Err(VmError::TypeError {
                        message: format!(
                            "Cannot define property '{}' on function {}",
                            key.label(gc_heap),
                            native.name(gc_heap)
                        ),
                    });
                }
                Ok(Value::native_function(native))
            } else if let Some(r) = first.and_then(|v| v.as_regexp()) {
                // RegExp instances expose `lastIndex` + expando.
                {
                    let existing = match &key {
                        PropertyKey::String(k) => r.expando(gc_heap).is_some_and(|bag| {
                            crate::object::get_own_descriptor(bag, gc_heap, k).is_some()
                        }),
                        PropertyKey::Symbol(sym) => r.expando(gc_heap).is_some_and(|bag| {
                            crate::object::get_own_symbol_descriptor(bag, gc_heap, *sym).is_some()
                        }),
                    };
                    if !existing && !r.is_extensible(gc_heap) {
                        return Err(VmError::TypeError {
                            message: format!("Cannot define property '{}'", key.label(gc_heap)),
                        });
                    }
                    let bag = crate::property_dispatch::regexp_ensure_expando_pub(gc_heap, &r)?;
                    let ok = match &key {
                        PropertyKey::String(k) => {
                            crate::object::define_own_property_partial(bag, gc_heap, k, descriptor)
                        }
                        PropertyKey::Symbol(sym) => {
                            crate::object::define_own_symbol_property_partial(
                                bag, gc_heap, *sym, descriptor,
                            )
                        }
                    };
                    if !ok {
                        return Err(VmError::TypeError {
                            message: format!("Cannot define property '{}'", key.label(gc_heap)),
                        });
                    }
                    Ok(Value::regexp(r))
                }
            } else if let Some(p) = first.and_then(|v| v.as_promise()) {
                // Promise instances also expose lazy expando.
                let bag = crate::property_dispatch::promise_ensure_expando_pub(gc_heap, &p)?;
                let ok = match &key {
                    PropertyKey::String(k) => {
                        crate::object::define_own_property_partial(bag, gc_heap, k, descriptor)
                    }
                    PropertyKey::Symbol(sym) => crate::object::define_own_symbol_property_partial(
                        bag, gc_heap, *sym, descriptor,
                    ),
                };
                if !ok {
                    return Err(VmError::TypeError {
                        message: format!("Cannot define property '{}'", key.label(gc_heap)),
                    });
                }
                Ok(Value::promise(p))
            } else if let Some(t) = first.and_then(|v| v.as_typed_array(gc_heap)) {
                // §10.4.5.3 IntegerIndexedExoticObject [[DefineOwnProperty]].
                match &key {
                    PropertyKey::String(k) => {
                        if let Some(n) = crate::property_dispatch::canonical_numeric_index_string(k)
                        {
                            if t.buffer(gc_heap).is_detached(gc_heap)
                                || !n.is_finite()
                                || n.fract() != 0.0
                                || n < 0.0
                                || (n as usize) >= t.length(gc_heap)
                                || descriptor.configurable == Some(false)
                                || descriptor.enumerable == Some(false)
                                || descriptor.writable == Some(false)
                                || descriptor.is_accessor()
                            {
                                return Err(VmError::TypeError {
                                    message: format!(
                                        "Cannot define property '{}'",
                                        key.label(gc_heap)
                                    ),
                                });
                            }
                            if let Some(value) = descriptor.value {
                                let coerced = crate::binary::dispatch::coerce_element_for_store(
                                    gc_heap,
                                    t.kind(),
                                    &value,
                                )?;
                                t.set(gc_heap, n as usize, &coerced);
                            }
                        } else {
                            let bag = crate::property_dispatch::typed_array_ensure_expando_pub(
                                gc_heap, &t,
                            )?;
                            if !crate::object::define_own_property_partial(
                                bag, gc_heap, k, descriptor,
                            ) {
                                return Err(VmError::TypeError {
                                    message: format!(
                                        "Cannot define property '{}'",
                                        key.label(gc_heap)
                                    ),
                                });
                            }
                        }
                    }
                    PropertyKey::Symbol(sym) => {
                        let bag =
                            crate::property_dispatch::typed_array_ensure_expando_pub(gc_heap, &t)?;
                        if !crate::object::define_own_symbol_property_partial(
                            bag, gc_heap, *sym, descriptor,
                        ) {
                            return Err(VmError::TypeError {
                                message: format!("Cannot define property '{}'", key.label(gc_heap)),
                            });
                        }
                    }
                }
                Ok(Value::typed_array(t))
            } else {
                Err(VmError::TypeError {
                    message: "Object.defineProperty target must be an object".to_string(),
                })
            }
        }
        // §20.1.2.5 Object.defineProperties(O, Properties)
        // <https://tc39.es/ecma262/#sec-object.defineproperties>
        M::DefineProperties => {
            let target = expect_object(args.first())?;
            let props = expect_object(args.get(1))?;
            // Walk enumerable own keys of `props`. Each value is a
            // descriptor object that we coerce + apply.
            let entries: Vec<(String, Value)> =
                crate::object::with_properties(props, gc_heap, |p| {
                    p.enumerable_data_iter()
                        .map(|(k, v)| (k.to_string(), v))
                        .collect()
                });
            for (key, desc_value) in entries {
                let desc_obj = desc_value.as_object().ok_or(VmError::TypeMismatch)?;
                let descriptor = coerce_to_descriptor(&desc_obj, gc_heap)?;
                if !crate::object::define_own_property_partial(target, gc_heap, &key, descriptor) {
                    return Err(VmError::TypeMismatch);
                }
            }
            Ok(Value::object(target))
        }
        // §20.1.2.10 Object.getOwnPropertyDescriptor(O, P)
        // <https://tc39.es/ecma262/#sec-object.getownpropertydescriptor>
        M::GetOwnPropertyDescriptor => {
            let key = expect_property_key(args.get(1), gc_heap)?;
            let first = args.first();
            if let Some(target) = first.and_then(|v| v.as_object()) {
                match &key {
                    PropertyKey::String(string_key) => {
                        if let Some(value) = crate::object::string_data(target, gc_heap)
                            && let Some(desc) = crate::string::exotic::descriptor_for_name(
                                value, string_key, gc_heap,
                            )?
                        {
                            return Ok(Value::object(descriptor_to_object_with_roots(
                                &desc,
                                gc_heap,
                                &[],
                                &[args],
                            )?));
                        }
                        match crate::object::get_own_descriptor(target, gc_heap, string_key) {
                            Some(desc) => Ok(Value::object(descriptor_to_object_with_roots(
                                &desc,
                                gc_heap,
                                &[],
                                &[args],
                            )?)),
                            None => Ok(Value::undefined()),
                        }
                    }
                    PropertyKey::Symbol(sym) => {
                        match crate::object::get_own_symbol_descriptor(target, gc_heap, *sym) {
                            Some(desc) => Ok(Value::object(descriptor_to_object_with_roots(
                                &desc,
                                gc_heap,
                                &[],
                                &[args],
                            )?)),
                            None => Ok(Value::undefined()),
                        }
                    }
                }
            } else if let Some(class) = first.and_then(|v| v.as_class_constructor()) {
                match &key {
                    PropertyKey::String(key) => {
                        match crate::object::get_own_descriptor(
                            class.statics(gc_heap),
                            gc_heap,
                            key,
                        ) {
                            Some(desc) => Ok(Value::object(descriptor_to_object_with_roots(
                                &desc,
                                gc_heap,
                                &[],
                                &[args],
                            )?)),
                            None => Ok(Value::undefined()),
                        }
                    }
                    PropertyKey::Symbol(sym) => {
                        match crate::object::get_own_symbol_descriptor(
                            class.statics(gc_heap),
                            gc_heap,
                            *sym,
                        ) {
                            Some(desc) => Ok(Value::object(descriptor_to_object_with_roots(
                                &desc,
                                gc_heap,
                                &[],
                                &[args],
                            )?)),
                            None => Ok(Value::undefined()),
                        }
                    }
                }
            } else if let Some(native) = first.and_then(|v| v.as_native_function()) {
                let PropertyKey::String(key) = &key else {
                    return Ok(Value::undefined());
                };
                match native.own_property_descriptor(gc_heap, key)? {
                    Some(desc) => Ok(Value::object(descriptor_to_object_with_roots(
                        &desc,
                        gc_heap,
                        &[],
                        &[args],
                    )?)),
                    None => Ok(Value::undefined()),
                }
            } else if let Some(value) = first.and_then(|v| v.as_string(gc_heap)) {
                let desc = match &key {
                    PropertyKey::String(key) => {
                        crate::string::exotic::descriptor_for_name(value, key, gc_heap)?
                    }
                    PropertyKey::Symbol(_) => None,
                };
                match desc {
                    Some(desc) => Ok(Value::object(descriptor_to_object_with_roots(
                        &desc,
                        gc_heap,
                        &[],
                        &[args],
                    )?)),
                    None => Ok(Value::undefined()),
                }
            } else if first
                .is_some_and(|v| v.is_boolean() || v.is_number() || v.is_symbol() || v.is_big_int())
            {
                // §20.1.2.7 Object.getOwnPropertyDescriptor — primitive
                // wrappers carry no own data props for arbitrary keys.
                Ok(Value::undefined())
            } else if first.is_none_or(|v| v.is_null() || v.is_undefined()) {
                Err(VmError::TypeError {
                    message:
                        "Object.getOwnPropertyDescriptor: cannot convert null/undefined to object"
                            .to_string(),
                })
            } else {
                Err(VmError::TypeError {
                    message: "Object.getOwnPropertyDescriptor target must be an object".to_string(),
                })
            }
        }
        // §20.1.2.11 Object.getOwnPropertyDescriptors(O)
        // <https://tc39.es/ecma262/#sec-object.getownpropertydescriptors>
        M::GetOwnPropertyDescriptors => {
            let target = expect_object(args.first())?;
            let target_root = Value::object(target);
            let result = rooted_object(gc_heap, &[&target_root], &[args])?;
            let result_root = Value::object(result);
            let (keys, symbols): (Vec<String>, Vec<JsSymbol>) =
                crate::object::with_properties(target, gc_heap, |p| {
                    (
                        p.keys().map(|s| s.to_string()).collect(),
                        p.symbol_keys().collect(),
                    )
                });
            for key in keys {
                if let Some(desc) = crate::object::get_own_descriptor(target, gc_heap, &key) {
                    let value = Value::object(descriptor_to_object_with_roots(
                        &desc,
                        gc_heap,
                        &[&target_root, &result_root],
                        &[args],
                    )?);
                    crate::object::set(result, gc_heap, &key, value);
                }
            }
            for sym in symbols {
                if let Some(desc) = crate::object::get_own_symbol_descriptor(target, gc_heap, sym) {
                    let value = Value::object(descriptor_to_object_with_roots(
                        &desc,
                        gc_heap,
                        &[&target_root, &result_root],
                        &[args],
                    )?);
                    if !crate::object::set_symbol(result, gc_heap, sym, value) {
                        return Err(VmError::TypeMismatch);
                    }
                }
            }
            Ok(Value::object(result))
        }
        // §20.1.2.6 Object.freeze(O)
        // <https://tc39.es/ecma262/#sec-object.freeze>
        M::Freeze => {
            let arg = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(o) = arg.as_object() {
                crate::object::freeze(o, gc_heap);
            }
            // Spec: returns the argument unchanged (non-objects pass
            // through).
            Ok(arg)
        }
        // §20.1.2.20 Object.seal(O)
        M::Seal => {
            let arg = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(o) = arg.as_object() {
                crate::object::seal(o, gc_heap);
            }
            Ok(arg)
        }
        // §20.1.2.18 Object.preventExtensions(O)
        M::PreventExtensions => {
            let arg = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(o) = arg.as_object() {
                crate::object::prevent_extensions(o, gc_heap);
            } else if let Some(a) = arg.as_array() {
                crate::array::prevent_extensions(a, gc_heap);
            } else if let Some(r) = arg.as_regexp() {
                r.prevent_extensions(gc_heap);
            }
            Ok(arg)
        }
        // §20.1.2.15 Object.isFrozen(O)
        M::IsFrozen => {
            let arg = args.first().cloned().unwrap_or(Value::undefined());
            // Per spec, `Object.isFrozen(non_object) === true`. Heap
            // exotics default to extensible+configurable so they are
            // not frozen unless the foundation explicitly toggles
            // their `[[Extensible]]` slot.
            let result = if let Some(o) = arg.as_object() {
                crate::object::is_frozen(o, gc_heap)
            } else {
                // §20.1.2.15 step 2 — non-Object returns true. Spec
                // Object covers callables/exotics (`is_object_type`),
                // not just `TAG_PTR_OBJECT`.
                !arg.is_object_type()
            };
            Ok(Value::boolean(result))
        }
        // §20.1.2.16 Object.isSealed(O)
        M::IsSealed => {
            let arg = args.first().cloned().unwrap_or(Value::undefined());
            // §20.1.2.16 — `Object.isSealed(non_object) === true`. For
            // ordinary objects, `is_sealed` walks the property table
            // checking that nothing is configurable and that the
            // object is non-extensible. Heap-allocated exotics that
            // do not yet carry per-instance attribute tracking
            // (Array indexed slots, RegExp expando, …) default to
            // `false` because their elements / lazy expando bags
            // remain configurable until `preventExtensions` is
            // applied through the foundation surface.
            let result = if let Some(o) = arg.as_object() {
                crate::object::is_sealed(o, gc_heap)
            } else {
                // §20.1.2.16 step 2 — non-Object returns true. Same
                // spec-Object widening as `isFrozen` above.
                !arg.is_object_type()
            };
            Ok(Value::boolean(result))
        }
        // §20.1.2.14 Object.isExtensible(O)
        M::IsExtensible => {
            let arg = args.first().cloned().unwrap_or(Value::undefined());
            // §20.1.2.14 — `Object.isExtensible(non_object) === false`.
            // Every heap-allocated value kind is an Object, so they
            // all default to extensible until a `preventExtensions`
            // / `seal` / `freeze` toggle landed. Primitives and the
            // null / undefined sentinels return false.
            let result = if let Some(o) = arg.as_object() {
                crate::object::is_extensible(o, gc_heap)
            } else if let Some(arr) = arg.as_array() {
                crate::array::is_extensible(arr, gc_heap)
            } else if let Some(r) = arg.as_regexp() {
                r.is_extensible(gc_heap)
            } else {
                // §20.1.2.14 step 2 — non-Object returns false; every
                // spec-Object kind reaches here only when none of the
                // dedicated wrappers above matched, in which case
                // `is_object_type` widens to callable / exotic
                // payloads that default to extensible.
                arg.is_object_type()
            };
            Ok(Value::boolean(result))
        }
        // §20.1.2.17 Object.keys(O) — enumerable own string keys.
        // <https://tc39.es/ecma262/#sec-object.keys>
        M::Keys => {
            let first = args.first();
            let owned: Vec<String> = if let Some(target) = first.and_then(|v| v.as_object()) {
                crate::object::with_properties(target, gc_heap, |p| {
                    p.enumerable_keys().map(|k| k.to_string()).collect()
                })
            } else if let Some(native) = first.and_then(|v| v.as_native_function()) {
                native.enumerable_own_property_keys(gc_heap)
            } else if let Some(bound) = first.and_then(|v| v.as_bound_function()) {
                crate::function_metadata::bound_enumerable_own_property_keys(&bound, gc_heap)
            } else {
                return Err(VmError::TypeMismatch);
            };
            let mut names = Vec::with_capacity(owned.len());
            for k in owned {
                names.push(string_value(&k, gc_heap)?);
            }
            Ok(Value::array(rooted_array_from_elements(
                gc_heap,
                names,
                &[],
                &[args],
            )?))
        }
        // §20.1.2.22 Object.values(O) — enumerable own data values.
        // <https://tc39.es/ecma262/#sec-object.values>
        M::Values => {
            let target = expect_object(args.first())?;
            let values: Vec<Value> = crate::object::with_properties(target, gc_heap, |p| {
                p.enumerable_data_iter().map(|(_, v)| v).collect()
            });
            let target_root = Value::object(target);
            Ok(Value::array(rooted_array_from_elements(
                gc_heap,
                values,
                &[&target_root],
                &[args],
            )?))
        }
        // §20.1.2.5 Object.entries(O) — `[key, value]` pairs in
        // insertion order.
        // <https://tc39.es/ecma262/#sec-object.entries>
        M::Entries => {
            let target = expect_object(args.first())?;
            let raw: Vec<(String, Value)> = crate::object::with_properties(target, gc_heap, |p| {
                p.enumerable_data_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect()
            });
            let mut pairs: Vec<Value> = Vec::with_capacity(raw.len());
            for (k, v) in raw {
                let key = string_value(&k, gc_heap)?;
                let pair: smallvec::SmallVec<[Value; 4]> = smallvec::smallvec![key, v];
                let target_root = Value::object(target);
                pairs.push(Value::array(rooted_array_from_elements(
                    gc_heap,
                    pair,
                    &[&target_root],
                    &[args, pairs.as_slice()],
                )?));
            }
            let target_root = Value::object(target);
            Ok(Value::array(rooted_array_from_elements(
                gc_heap,
                pairs,
                &[&target_root],
                &[args],
            )?))
        }
        // §20.1.2.1 Object.assign(target, ...sources). Copies own
        // enumerable string-keyed data properties from each source
        // into `target` using `[[Set]]` (so existing accessors on
        // target invoke their setters). Foundation simplifies the
        // [[Set]] step: we use the `set()` construction helper since
        // the spec's full ladder is filed against the dispatch layer.
        // Symbol-keyed properties + non-enumerable + accessor sources
        // are left to follow-ups.
        // <https://tc39.es/ecma262/#sec-object.assign>
        M::Assign => {
            let target = expect_object(args.first())?;
            for src in args.iter().skip(1) {
                if src.is_undefined() || src.is_null() {
                    // Per spec, null/undefined sources are skipped.
                    continue;
                }
                let o = src.as_object().ok_or(VmError::TypeMismatch)?;
                let entries: Vec<(String, Value)> =
                    crate::object::with_properties(o, gc_heap, |p| {
                        p.enumerable_data_iter()
                            .map(|(k, v)| (k.to_string(), v))
                            .collect()
                    });
                for (k, v) in entries {
                    crate::object::set(target, gc_heap, &k, v);
                }
            }
            Ok(Value::object(target))
        }
        // §20.1.2.7 Object.fromEntries(iterable). Foundation accepts
        // an array of `[k, v]` pairs (the most common shape) and a
        // Map; arbitrary iterables route through the user
        // iterator protocol once it lands here too — filed.
        // <https://tc39.es/ecma262/#sec-object.fromentries>
        M::FromEntries => {
            let iter = args.first().cloned().unwrap_or(Value::undefined());
            let iter_root = iter;
            let result = rooted_object(gc_heap, &[&iter_root], &[args])?;
            if let Some(arr) = iter.as_array() {
                let snapshot: Vec<Value> =
                    crate::array::with_elements(arr, gc_heap, |elements| elements.to_vec());
                for entry in snapshot {
                    let (key, value) = read_entry_pair_heap(&entry, gc_heap)?;
                    set_from_entries_key_heap(result, &key, value, gc_heap)?;
                }
            } else if let Some(m) = iter.as_map() {
                for (key, value) in crate::collections::map_entries(m, gc_heap) {
                    set_from_entries_key_heap(result, &key, value, gc_heap)?;
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
            Ok(Value::object(result))
        }
        // §20.1.2.13 Object.hasOwn(O, P) — Stage 4 ergonomic
        // alternative to `Object.prototype.hasOwnProperty.call`.
        // <https://tc39.es/ecma262/#sec-object.hasown>
        M::HasOwn => {
            let first = args.first();
            let target = if let Some(target) = first.and_then(|v| v.as_object()) {
                target
            } else if let Some(class) = first.and_then(|v| v.as_class_constructor()) {
                class.statics(gc_heap)
            } else {
                return Err(VmError::TypeMismatch);
            };
            let present = has_own_property(target, gc_heap, args.get(1))?;
            Ok(Value::boolean(present))
        }
        // §20.1.2.12 Object.getOwnPropertyNames(O) — every own
        // string-keyed property, regardless of enumerability.
        // <https://tc39.es/ecma262/#sec-object.getownpropertynames>
        M::GetOwnPropertyNames => {
            let first = args.first();
            let owned: Vec<String> = if let Some(target) = first.and_then(|v| v.as_object()) {
                crate::object::with_properties(target, gc_heap, |p| {
                    p.keys().map(|k| k.to_string()).collect()
                })
            } else if let Some(native) = first.and_then(|v| v.as_native_function()) {
                native.own_property_keys(gc_heap)
            } else if let Some(bound) = first.and_then(|v| v.as_bound_function()) {
                crate::function_metadata::bound_own_property_keys(&bound, gc_heap)
            } else if first.is_some_and(|v| v.is_function() || v.is_closure()) {
                // Ordinary function / closure — no context here.
                return Err(VmError::InvalidOperand);
            } else if let Some(class) = first.and_then(|v| v.as_class_constructor()) {
                class_constructor_own_property_keys_without_context(&class, gc_heap)?
            } else if first.is_some_and(|v| v.is_boolean() || v.is_number() || v.is_symbol()) {
                Vec::new()
            } else if let Some(s) = first.and_then(|v| v.as_string(gc_heap)) {
                let mut keys: Vec<String> = (0..s.len()).map(|idx| idx.to_string()).collect();
                keys.push("length".to_string());
                keys
            } else {
                return Err(VmError::TypeMismatch);
            };
            let mut names: Vec<Value> = Vec::with_capacity(owned.len());
            for k in owned {
                names.push(string_value(&k, gc_heap)?);
            }
            Ok(Value::array(rooted_array_from_elements(
                gc_heap,
                names,
                &[],
                &[args],
            )?))
        }
        // §20.1.2.13 Object.getOwnPropertySymbols(O) — every own
        // symbol-keyed property. Foundation property bag is
        // string-keyed today; symbol keys are tracked in a parallel
        // table inside JsObject.
        // <https://tc39.es/ecma262/#sec-object.getownpropertysymbols>
        M::GetOwnPropertySymbols => {
            let target = expect_object(args.first())?;
            let syms: Vec<Value> = crate::object::with_properties(target, gc_heap, |p| {
                p.symbol_keys().map(Value::symbol).collect()
            });
            let target_root = Value::object(target);
            Ok(Value::array(rooted_array_from_elements(
                gc_heap,
                syms,
                &[&target_root],
                &[args],
            )?))
        }
        // §20.1.2.7 `Object.groupBy(items, callbackfn)` — the
        // context-less fallback path can't run the callback, so it
        // routes through the rooted entrypoint above. Reaching this
        // arm means the call site bypassed `native_rooted_call`
        // (e.g. through `Reflect.apply` without a live execution
        // context); surface as a TypeError so the caller learns the
        // method needs a JS frame.
        M::GroupBy => Err(VmError::TypeError {
            message: "Object.groupBy requires an active execution context".to_string(),
        }),
        M::ForInKeys => Err(VmError::TypeError {
            message: "Object.__forInKeys requires an active execution context".to_string(),
        }),
    }
}

fn string_value(s: &str, heap: &mut otter_gc::GcHeap) -> Result<Value, VmError> {
    Ok(Value::string(
        JsString::from_str(s, heap).map_err(|_| VmError::TypeMismatch)?,
    ))
}

/// Implement §6.2.5.5 ToPropertyDescriptor against `desc_obj`.
///
/// Returns a [`PartialPropertyDescriptor`] that tracks which fields
/// were present on the source object, matching the V8 / JSC /
/// SpiderMonkey descriptor-coercion shape so
/// `ValidateAndApplyPropertyDescriptor` can distinguish "absent" from
/// "present with `false`".
///
/// # Algorithm
/// - Read `value`, `writable`, `enumerable`, `configurable`, `get`,
///   `set` from the descriptor object as own data properties.
/// - Mixing accessor + data fields rejects with `TypeMismatch` per
///   step 17.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-topropertydescriptor>
pub fn coerce_to_descriptor(
    desc_obj: &JsObject,
    gc_heap: &otter_gc::GcHeap,
) -> Result<PartialPropertyDescriptor, VmError> {
    // Direct own-data probes — accessors on the descriptor object
    // itself are ignored for the slice.
    let value = crate::object::lookup_own(*desc_obj, gc_heap, "value");
    let writable = crate::object::lookup_own(*desc_obj, gc_heap, "writable");
    let enumerable = crate::object::lookup_own(*desc_obj, gc_heap, "enumerable");
    let configurable = crate::object::lookup_own(*desc_obj, gc_heap, "configurable");
    let getter = crate::object::lookup_own(*desc_obj, gc_heap, "get");
    let setter = crate::object::lookup_own(*desc_obj, gc_heap, "set");

    let has_value = !matches!(value, PropertyLookup::Absent);
    let has_writable = !matches!(writable, PropertyLookup::Absent);
    let has_get = !matches!(getter, PropertyLookup::Absent);
    let has_set = !matches!(setter, PropertyLookup::Absent);

    if (has_get || has_set) && (has_value || has_writable) {
        // §6.2.5.5 step 17 — cannot mix data and accessor fields.
        return Err(VmError::TypeMismatch);
    }

    let mut descriptor = PartialPropertyDescriptor::default();
    if has_value {
        descriptor.value = Some(match value {
            PropertyLookup::Data { value, .. } => value,
            _ => Value::undefined(),
        });
    }
    if has_writable {
        descriptor.writable = lookup_to_optional_bool(&writable, gc_heap);
    }
    descriptor.enumerable = lookup_to_optional_bool(&enumerable, gc_heap);
    descriptor.configurable = lookup_to_optional_bool(&configurable, gc_heap);
    if has_get {
        descriptor.get = Some(lookup_to_optional_value(&getter)?.unwrap_or(Value::undefined()));
    }
    if has_set {
        descriptor.set = Some(lookup_to_optional_value(&setter)?.unwrap_or(Value::undefined()));
    }
    Ok(descriptor)
}

fn rooted_object(
    gc_heap: &mut otter_gc::GcHeap,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<JsObject, VmError> {
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        for value in value_roots {
            value.trace_value_slots(visitor);
        }
        for slice in slice_roots {
            for value in *slice {
                value.trace_value_slots(visitor);
            }
        }
    };
    crate::object::alloc_object_with_roots(gc_heap, &mut external_visit).map_err(VmError::from)
}

fn rooted_array_from_elements<I>(
    gc_heap: &mut otter_gc::GcHeap,
    values: I,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<crate::array::JsArray, VmError>
where
    I: IntoIterator<Item = Value>,
{
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        for value in value_roots {
            value.trace_value_slots(visitor);
        }
        for slice in slice_roots {
            for value in *slice {
                value.trace_value_slots(visitor);
            }
        }
    };
    crate::array::from_elements_with_roots(gc_heap, values, &mut external_visit)
        .map_err(VmError::from)
}

fn descriptor_to_object_with_roots(
    desc: &PropertyDescriptor,
    gc_heap: &mut otter_gc::GcHeap,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<JsObject, VmError> {
    let mut roots = Vec::with_capacity(value_roots.len() + 2);
    roots.extend_from_slice(value_roots);
    match &desc.kind {
        DescriptorKind::Data { value } => roots.push(value),
        DescriptorKind::Accessor { getter, setter } => {
            if let Some(getter) = getter {
                roots.push(getter);
            }
            if let Some(setter) = setter {
                roots.push(setter);
            }
        }
    }
    let result = rooted_object(gc_heap, &roots, slice_roots)?;
    match &desc.kind {
        DescriptorKind::Data { value } => {
            crate::object::set(result, gc_heap, "value", *value);
            crate::object::set(result, gc_heap, "writable", Value::boolean(desc.writable()));
        }
        DescriptorKind::Accessor { getter, setter } => {
            crate::object::set(
                result,
                gc_heap,
                "get",
                (*getter).unwrap_or(Value::undefined()),
            );
            crate::object::set(
                result,
                gc_heap,
                "set",
                (*setter).unwrap_or(Value::undefined()),
            );
        }
    }
    crate::object::set(
        result,
        gc_heap,
        "enumerable",
        Value::boolean(desc.enumerable()),
    );
    crate::object::set(
        result,
        gc_heap,
        "configurable",
        Value::boolean(desc.configurable()),
    );
    Ok(result)
}

fn lookup_to_optional_bool(lookup: &PropertyLookup, heap: &otter_gc::GcHeap) -> Option<bool> {
    match lookup {
        PropertyLookup::Absent => None,
        PropertyLookup::Data { value, .. } => Some(value.to_boolean(heap)),
        // An accessor on the descriptor object would fire its getter
        // per spec; we treat as absent in the slice.
        PropertyLookup::Accessor { .. } => None,
    }
}

fn lookup_to_optional_value(lookup: &PropertyLookup) -> Result<Option<Value>, VmError> {
    match lookup {
        PropertyLookup::Absent => Ok(None),
        PropertyLookup::Data { value, .. } => {
            if value.is_undefined() {
                Ok(None)
            } else {
                Ok(Some(*value))
            }
        }
        PropertyLookup::Accessor { .. } => Ok(None),
    }
}

fn expect_object(arg: Option<&Value>) -> Result<JsObject, VmError> {
    arg.and_then(|v| v.as_object()).ok_or(VmError::TypeMismatch)
}

fn expect_property_key(
    arg: Option<&Value>,
    heap: &otter_gc::GcHeap,
) -> Result<PropertyKey, VmError> {
    let Some(v) = arg else {
        return Ok(PropertyKey::String("undefined".to_string()));
    };
    if let Some(s) = v.as_string(heap) {
        Ok(PropertyKey::String(s.to_lossy_string(heap)))
    } else if let Some(n) = v.as_number() {
        Ok(PropertyKey::String(n.to_display_string()))
    } else if let Some(b) = v.as_boolean() {
        Ok(PropertyKey::String(
            (if b { "true" } else { "false" }).to_string(),
        ))
    } else if v.is_null() {
        Ok(PropertyKey::String("null".to_string()))
    } else if v.is_undefined() {
        Ok(PropertyKey::String("undefined".to_string()))
    } else if let Some(sym) = v.as_symbol(heap) {
        Ok(PropertyKey::Symbol(sym))
    } else {
        Err(VmError::TypeMismatch)
    }
}

fn native_to_property_key(
    ctx: &mut NativeCtx<'_>,
    arg: Option<&Value>,
    method_name: &'static str,
) -> Result<PropertyKey, NativeError> {
    let value = arg.cloned().unwrap_or(Value::undefined());
    let Some(exec_ctx) = ctx.execution_context().cloned() else {
        return expect_property_key(Some(&value), ctx.heap())
            .map_err(|err| object_native_error(method_name, err));
    };
    let key = ctx
        .cx
        .interp
        .to_property_key_sync(&exec_ctx, value)
        .map_err(|err| object_native_error(method_name, err))?;
    match key {
        crate::VmPropertyKey::Symbol(sym) => Ok(PropertyKey::Symbol(sym)),
        crate::VmPropertyKey::Atom(atom) => Ok(PropertyKey::String(atom.name().to_string())),
        crate::VmPropertyKey::String(s) => Ok(PropertyKey::String(s.to_string())),
        crate::VmPropertyKey::OwnedString(s) => Ok(PropertyKey::String(s)),
    }
}

fn has_own_property(
    target: JsObject,
    gc_heap: &otter_gc::GcHeap,
    key: Option<&Value>,
) -> Result<bool, VmError> {
    match expect_property_key(key, gc_heap)? {
        PropertyKey::Symbol(sym) => Ok(crate::object::has_own_symbol(target, gc_heap, sym)),
        PropertyKey::String(key) => Ok(!matches!(
            crate::object::lookup_own(target, gc_heap, &key),
            PropertyLookup::Absent
        )),
    }
}

/// §7.1.19 ToPropertyKey for a free-standing `Value`. Foundation
/// accepts `String` and `Number` operands; symbol keys take a
/// dedicated path (object_statics here is string-key only).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-topropertykey>
fn property_key_from_value(value: &Value, heap: &otter_gc::GcHeap) -> Result<String, VmError> {
    if let Some(s) = value.as_string(heap) {
        Ok(s.to_lossy_string(heap))
    } else if let Some(n) = value.as_number() {
        Ok(n.to_display_string())
    } else if let Some(b) = value.as_boolean() {
        Ok((if b { "true" } else { "false" }).to_string())
    } else if value.is_null() {
        Ok("null".to_string())
    } else if value.is_undefined() {
        Ok("undefined".to_string())
    } else {
        Err(VmError::TypeMismatch)
    }
}
