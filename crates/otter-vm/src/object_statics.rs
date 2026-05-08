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
use crate::object::{DescriptorKind, JsObject, PropertyDescriptor, PropertyLookup};
use crate::string::{JsString, StringHeap};
use crate::symbol::JsSymbol;
use crate::{NativeCtx, NativeError, Value, VmError};

enum PropertyKey {
    String(String),
    Symbol(JsSymbol),
}

impl PropertyKey {
    fn label(&self) -> String {
        match self {
            Self::String(key) => key.clone(),
            Self::Symbol(sym) => sym.descriptive_string(),
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
];

/// Static methods installed on `Object.prototype`.
pub static OBJECT_PROTOTYPE_METHODS: &[MethodSpec] = &[
    method("hasOwnProperty", 1, native_prototype_has_own_property),
    method(
        "propertyIsEnumerable",
        1,
        native_prototype_property_is_enumerable,
    ),
    method("isPrototypeOf", 1, native_prototype_is_prototype_of),
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
    name: &'static str,
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let string_heap = ctx.cx.interp.string_heap_clone();
    call(name, args, &string_heap, ctx.heap_mut()).map_err(|err| object_native_error(name, err))
}

fn object_native_error(name: &'static str, err: VmError) -> NativeError {
    NativeError::TypeError {
        name,
        reason: err.to_string(),
    }
}

macro_rules! native_object_static {
    ($fn_name:ident, $js_name:literal) => {
        fn $fn_name(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            native_call($js_name, ctx, args)
        }
    };
}

native_object_static!(native_create, "create");
native_object_static!(native_define_property, "defineProperty");
native_object_static!(native_define_properties, "defineProperties");
native_object_static!(
    native_get_own_property_descriptor,
    "getOwnPropertyDescriptor"
);
native_object_static!(
    native_get_own_property_descriptors,
    "getOwnPropertyDescriptors"
);
native_object_static!(native_freeze, "freeze");
native_object_static!(native_is_frozen, "isFrozen");
native_object_static!(native_seal, "seal");
native_object_static!(native_is_sealed, "isSealed");
native_object_static!(native_prevent_extensions, "preventExtensions");
native_object_static!(native_is_extensible, "isExtensible");
native_object_static!(native_keys, "keys");
native_object_static!(native_values, "values");
native_object_static!(native_entries, "entries");
native_object_static!(native_assign, "assign");
native_object_static!(native_from_entries, "fromEntries");
native_object_static!(native_has_own, "hasOwn");
native_object_static!(native_get_own_property_names, "getOwnPropertyNames");
native_object_static!(native_get_own_property_symbols, "getOwnPropertySymbols");

fn native_prototype_has_own_property(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let present = match ctx.this_value() {
        Value::Object(obj) => has_own_property(*obj, ctx.heap(), args.first())
            .map_err(|err| object_native_error("hasOwnProperty", err))?,
        Value::NativeFunction(native) => native_function_has_own(native, ctx.heap(), args.first()),
        _ => false,
    };
    Ok(Value::Boolean(present))
}

fn native_prototype_property_is_enumerable(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let enumerable = match ctx.this_value() {
        Value::Object(obj) => {
            let key = expect_property_key(args.first())
                .map_err(|err| object_native_error("propertyIsEnumerable", err))?;
            match key {
                PropertyKey::String(key) => {
                    match crate::object::lookup_own(*obj, ctx.heap(), &key) {
                        PropertyLookup::Data { flags, .. }
                        | PropertyLookup::Accessor { flags, .. } => flags.enumerable(),
                        PropertyLookup::Absent => false,
                    }
                }
                PropertyKey::Symbol(sym) => {
                    match crate::object::lookup_own_symbol(*obj, ctx.heap(), &sym) {
                        PropertyLookup::Data { flags, .. }
                        | PropertyLookup::Accessor { flags, .. } => flags.enumerable(),
                        PropertyLookup::Absent => false,
                    }
                }
            }
        }
        Value::NativeFunction(native) => {
            let _ = native;
            false
        }
        _ => false,
    };
    Ok(Value::Boolean(enumerable))
}

fn native_prototype_is_prototype_of(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let result = match (ctx.this_value(), args.first()) {
        (Value::Object(proto), Some(Value::Object(other))) => {
            crate::object::has_in_proto_chain(*other, ctx.heap(), *proto)
        }
        _ => false,
    };
    Ok(Value::Boolean(result))
}

fn native_function_has_own(
    native: &crate::NativeFunction,
    gc_heap: &otter_gc::GcHeap,
    key: Option<&Value>,
) -> bool {
    match expect_property_key(key) {
        Ok(PropertyKey::String(key)) => native
            .own_property_descriptor(gc_heap, &StringHeap::default(), &key)
            .ok()
            .flatten()
            .is_some(),
        Ok(PropertyKey::Symbol(_)) | Err(_) => false,
    }
}

/// Single entry point for `Object.<name>(args...)` dispatch.
///
/// Returns the call's completion value or surfaces a [`VmError`].
///
/// # Errors
/// - [`VmError::UnknownIntrinsic`] when `name` is not recognised.
/// - [`VmError::TypeMismatch`] when an argument has the wrong shape
///   (e.g., the receiver of `defineProperty` is not an Object).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-properties-of-the-object-constructor>
pub fn call(
    name: &str,
    args: &[Value],
    string_heap: &StringHeap,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<Value, VmError> {
    match name {
        // §20.1.2.2 Object.create(O, Properties)
        // <https://tc39.es/ecma262/#sec-object.create>
        "create" => {
            let proto = args.first().cloned().unwrap_or(Value::Undefined);
            let proto_obj = match proto {
                Value::Object(o) => Some(o),
                Value::Null => None,
                _ => return Err(VmError::TypeMismatch),
            };
            let obj = crate::object::alloc_object(gc_heap)?;
            crate::object::set_prototype(obj, gc_heap, proto_obj);
            if let Some(props_arg) = args.get(1)
                && !matches!(props_arg, Value::Undefined)
            {
                let props = match props_arg {
                    Value::Object(o) => *o,
                    _ => return Err(VmError::TypeMismatch),
                };
                let entries: Vec<(String, Value)> =
                    crate::object::with_properties(props, gc_heap, |p| {
                        p.enumerable_data_iter()
                            .map(|(k, v)| (k.to_string(), v))
                            .collect()
                    });
                for (key, desc_value) in entries {
                    let desc_obj = match desc_value {
                        Value::Object(o) => o,
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let descriptor = coerce_to_descriptor(&desc_obj, gc_heap)?;
                    if !crate::object::define_own_property(obj, gc_heap, &key, descriptor) {
                        return Err(VmError::TypeMismatch);
                    }
                }
            }
            Ok(Value::Object(obj))
        }
        // §20.1.2.4 Object.defineProperty(O, P, Attributes)
        // <https://tc39.es/ecma262/#sec-object.defineproperty>
        "defineProperty" => {
            let key = expect_property_key(args.get(1))?;
            let desc_obj = expect_object(args.get(2))?;
            let descriptor = coerce_to_descriptor(&desc_obj, gc_heap)?;
            match args.first() {
                Some(Value::Object(target)) => {
                    let ok = match &key {
                        PropertyKey::String(key) => {
                            crate::object::define_own_property(*target, gc_heap, key, descriptor)
                        }
                        PropertyKey::Symbol(sym) => crate::object::define_own_symbol_property(
                            *target, gc_heap, sym, descriptor,
                        ),
                    };
                    if !ok {
                        return Err(VmError::TypeError {
                            message: format!("Cannot define property '{}'", key.label()),
                        });
                    }
                    Ok(Value::Object(*target))
                }
                Some(Value::ClassConstructor(class)) => {
                    let ok = match &key {
                        PropertyKey::String(key) => crate::object::define_own_property(
                            class.statics,
                            gc_heap,
                            key,
                            descriptor,
                        ),
                        PropertyKey::Symbol(sym) => crate::object::define_own_symbol_property(
                            class.statics,
                            gc_heap,
                            sym,
                            descriptor,
                        ),
                    };
                    if !ok {
                        return Err(VmError::TypeError {
                            message: format!("Cannot define property '{}'", key.label()),
                        });
                    }
                    Ok(Value::ClassConstructor(class.clone()))
                }
                Some(Value::NativeFunction(native)) => {
                    let PropertyKey::String(key) = &key else {
                        return Err(VmError::TypeError {
                            message: format!(
                                "Cannot define property '{}' on function {}",
                                key.label(),
                                native.name(gc_heap)
                            ),
                        });
                    };
                    if !native.define_own_property(gc_heap, key, descriptor) {
                        return Err(VmError::TypeError {
                            message: format!(
                                "Cannot define property '{key}' on function {}",
                                native.name(gc_heap)
                            ),
                        });
                    }
                    Ok(Value::NativeFunction(*native))
                }
                _ => Err(VmError::TypeError {
                    message: "Object.defineProperty target must be an object".to_string(),
                }),
            }
        }
        // §20.1.2.5 Object.defineProperties(O, Properties)
        // <https://tc39.es/ecma262/#sec-object.defineproperties>
        "defineProperties" => {
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
                let desc_obj = match desc_value {
                    Value::Object(o) => o,
                    _ => return Err(VmError::TypeMismatch),
                };
                let descriptor = coerce_to_descriptor(&desc_obj, gc_heap)?;
                if !crate::object::define_own_property(target, gc_heap, &key, descriptor) {
                    return Err(VmError::TypeMismatch);
                }
            }
            Ok(Value::Object(target))
        }
        // §20.1.2.10 Object.getOwnPropertyDescriptor(O, P)
        // <https://tc39.es/ecma262/#sec-object.getownpropertydescriptor>
        "getOwnPropertyDescriptor" => {
            let key = expect_property_key(args.get(1))?;
            match args.first() {
                Some(Value::Object(target)) => match &key {
                    PropertyKey::String(key) => {
                        match crate::object::get_own_descriptor(*target, gc_heap, key) {
                            Some(desc) => Ok(Value::Object(descriptor_to_object(&desc, gc_heap)?)),
                            None => Ok(Value::Undefined),
                        }
                    }
                    PropertyKey::Symbol(sym) => {
                        match crate::object::get_own_symbol_descriptor(*target, gc_heap, sym) {
                            Some(desc) => Ok(Value::Object(descriptor_to_object(&desc, gc_heap)?)),
                            None => Ok(Value::Undefined),
                        }
                    }
                },
                Some(Value::ClassConstructor(class)) => match &key {
                    PropertyKey::String(key) => {
                        match crate::object::get_own_descriptor(class.statics, gc_heap, key) {
                            Some(desc) => Ok(Value::Object(descriptor_to_object(&desc, gc_heap)?)),
                            None => Ok(Value::Undefined),
                        }
                    }
                    PropertyKey::Symbol(sym) => {
                        match crate::object::get_own_symbol_descriptor(class.statics, gc_heap, sym)
                        {
                            Some(desc) => Ok(Value::Object(descriptor_to_object(&desc, gc_heap)?)),
                            None => Ok(Value::Undefined),
                        }
                    }
                },
                Some(Value::NativeFunction(native)) => {
                    let PropertyKey::String(key) = &key else {
                        return Ok(Value::Undefined);
                    };
                    match native.own_property_descriptor(gc_heap, string_heap, key)? {
                        Some(desc) => Ok(Value::Object(descriptor_to_object(&desc, gc_heap)?)),
                        None => Ok(Value::Undefined),
                    }
                }
                _ => Err(VmError::TypeError {
                    message: "Object.getOwnPropertyDescriptor target must be an object".to_string(),
                }),
            }
        }
        // §20.1.2.11 Object.getOwnPropertyDescriptors(O)
        // <https://tc39.es/ecma262/#sec-object.getownpropertydescriptors>
        "getOwnPropertyDescriptors" => {
            let target = expect_object(args.first())?;
            let result = crate::object::alloc_object(gc_heap)?;
            let (keys, symbols): (Vec<String>, Vec<JsSymbol>) =
                crate::object::with_properties(target, gc_heap, |p| {
                    (
                        p.keys().map(|s| s.to_string()).collect(),
                        p.symbol_keys().collect(),
                    )
                });
            for key in keys {
                if let Some(desc) = crate::object::get_own_descriptor(target, gc_heap, &key) {
                    let value = Value::Object(descriptor_to_object(&desc, gc_heap)?);
                    crate::object::set(result, gc_heap, &key, value);
                }
            }
            for sym in symbols {
                if let Some(desc) = crate::object::get_own_symbol_descriptor(target, gc_heap, &sym)
                {
                    let value = Value::Object(descriptor_to_object(&desc, gc_heap)?);
                    if !crate::object::set_symbol(result, gc_heap, sym, value) {
                        return Err(VmError::TypeMismatch);
                    }
                }
            }
            Ok(Value::Object(result))
        }
        // §20.1.2.6 Object.freeze(O)
        // <https://tc39.es/ecma262/#sec-object.freeze>
        "freeze" => {
            let arg = args.first().cloned().unwrap_or(Value::Undefined);
            if let Value::Object(o) = &arg {
                crate::object::freeze(*o, gc_heap);
            }
            // Spec: returns the argument unchanged (non-objects pass
            // through).
            Ok(arg)
        }
        // §20.1.2.20 Object.seal(O)
        "seal" => {
            let arg = args.first().cloned().unwrap_or(Value::Undefined);
            if let Value::Object(o) = &arg {
                crate::object::seal(*o, gc_heap);
            }
            Ok(arg)
        }
        // §20.1.2.18 Object.preventExtensions(O)
        "preventExtensions" => {
            let arg = args.first().cloned().unwrap_or(Value::Undefined);
            if let Value::Object(o) = &arg {
                crate::object::prevent_extensions(*o, gc_heap);
            }
            Ok(arg)
        }
        // §20.1.2.15 Object.isFrozen(O)
        "isFrozen" => {
            let arg = args.first().cloned().unwrap_or(Value::Undefined);
            // Per spec, `Object.isFrozen(non_object) === true`.
            let result = match arg {
                Value::Object(o) => crate::object::is_frozen(o, gc_heap),
                _ => true,
            };
            Ok(Value::Boolean(result))
        }
        // §20.1.2.16 Object.isSealed(O)
        "isSealed" => {
            let arg = args.first().cloned().unwrap_or(Value::Undefined);
            let result = match arg {
                Value::Object(o) => crate::object::is_sealed(o, gc_heap),
                _ => true,
            };
            Ok(Value::Boolean(result))
        }
        // §20.1.2.14 Object.isExtensible(O)
        "isExtensible" => {
            let arg = args.first().cloned().unwrap_or(Value::Undefined);
            // Spec: `Object.isExtensible(non_object) === false`.
            let result = match arg {
                Value::Object(o) => crate::object::is_extensible(o, gc_heap),
                _ => false,
            };
            Ok(Value::Boolean(result))
        }
        // §20.1.2.17 Object.keys(O) — enumerable own string keys.
        // <https://tc39.es/ecma262/#sec-object.keys>
        "keys" => {
            let owned: Vec<String> = match args.first() {
                Some(Value::Object(target)) => {
                    crate::object::with_properties(*target, gc_heap, |p| {
                        p.enumerable_keys().map(|k| k.to_string()).collect()
                    })
                }
                Some(Value::NativeFunction(native)) => native
                    .enumerable_own_property_keys(gc_heap)
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                _ => return Err(VmError::TypeMismatch),
            };
            let mut names = Vec::with_capacity(owned.len());
            for k in owned {
                names.push(string_value(&k, string_heap)?);
            }
            Ok(Value::Array(crate::array::from_elements(gc_heap, names)?))
        }
        // §20.1.2.22 Object.values(O) — enumerable own data values.
        // <https://tc39.es/ecma262/#sec-object.values>
        "values" => {
            let target = expect_object(args.first())?;
            let values: Vec<Value> = crate::object::with_properties(target, gc_heap, |p| {
                p.enumerable_data_iter().map(|(_, v)| v).collect()
            });
            Ok(Value::Array(crate::array::from_elements(gc_heap, values)?))
        }
        // §20.1.2.5 Object.entries(O) — `[key, value]` pairs in
        // insertion order.
        // <https://tc39.es/ecma262/#sec-object.entries>
        "entries" => {
            let target = expect_object(args.first())?;
            let raw: Vec<(String, Value)> = crate::object::with_properties(target, gc_heap, |p| {
                p.enumerable_data_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect()
            });
            let mut pairs: Vec<Value> = Vec::with_capacity(raw.len());
            for (k, v) in raw {
                let key = string_value(&k, string_heap)?;
                let pair: smallvec::SmallVec<[Value; 4]> = smallvec::smallvec![key, v];
                pairs.push(Value::Array(crate::array::from_elements(gc_heap, pair)?));
            }
            Ok(Value::Array(crate::array::from_elements(gc_heap, pairs)?))
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
        "assign" => {
            let target = expect_object(args.first())?;
            for src in args.iter().skip(1) {
                match src {
                    // Per spec, `null` / `undefined` sources are
                    // skipped silently.
                    Value::Undefined | Value::Null => continue,
                    Value::Object(o) => {
                        let entries: Vec<(String, Value)> =
                            crate::object::with_properties(*o, gc_heap, |p| {
                                p.enumerable_data_iter()
                                    .map(|(k, v)| (k.to_string(), v))
                                    .collect()
                            });
                        for (k, v) in entries {
                            crate::object::set(target, gc_heap, &k, v);
                        }
                    }
                    _ => return Err(VmError::TypeMismatch),
                }
            }
            Ok(Value::Object(target))
        }
        // §20.1.2.7 Object.fromEntries(iterable). Foundation accepts
        // an array of `[k, v]` pairs (the most common shape) and a
        // `Value::Map`; arbitrary iterables route through the user
        // iterator protocol once it lands here too — filed.
        // <https://tc39.es/ecma262/#sec-object.fromentries>
        "fromEntries" => {
            let iter = args.first().cloned().unwrap_or(Value::Undefined);
            let result = crate::object::alloc_object(gc_heap)?;
            match iter {
                Value::Array(arr) => {
                    // Snapshot to avoid holding the array's RefCell
                    // borrow while we recurse into per-pair work.
                    let snapshot: Vec<Value> =
                        crate::array::with_elements(arr, gc_heap, |elements| elements.to_vec());
                    for entry in snapshot {
                        match entry {
                            Value::Array(pair) => {
                                let key = crate::array::get(pair, gc_heap, 0);
                                let value = crate::array::get(pair, gc_heap, 1);
                                let key_str = property_key_from_value(&key)?;
                                crate::object::set(result, gc_heap, &key_str, value);
                            }
                            _ => return Err(VmError::TypeMismatch),
                        }
                    }
                }
                Value::Map(m) => {
                    for (key, value) in crate::collections::map_entries(m, gc_heap) {
                        let key_str = property_key_from_value(&key)?;
                        crate::object::set(result, gc_heap, &key_str, value);
                    }
                }
                _ => return Err(VmError::TypeMismatch),
            }
            Ok(Value::Object(result))
        }
        // §20.1.2.13 Object.hasOwn(O, P) — Stage 4 ergonomic
        // alternative to `Object.prototype.hasOwnProperty.call`.
        // <https://tc39.es/ecma262/#sec-object.hasown>
        "hasOwn" => {
            let target = match args.first() {
                Some(Value::Object(target)) => *target,
                Some(Value::ClassConstructor(class)) => class.statics,
                _ => return Err(VmError::TypeMismatch),
            };
            let present = has_own_property(target, gc_heap, args.get(1))?;
            Ok(Value::Boolean(present))
        }
        // §20.1.2.12 Object.getOwnPropertyNames(O) — every own
        // string-keyed property, regardless of enumerability.
        // <https://tc39.es/ecma262/#sec-object.getownpropertynames>
        "getOwnPropertyNames" => {
            let owned: Vec<String> = match args.first() {
                Some(Value::Object(target)) => {
                    crate::object::with_properties(*target, gc_heap, |p| {
                        p.keys().map(|k| k.to_string()).collect()
                    })
                }
                Some(Value::NativeFunction(native)) => native
                    .own_property_keys(gc_heap)
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                Some(Value::Boolean(_) | Value::Number(_) | Value::Symbol(_)) => Vec::new(),
                Some(Value::String(s)) => {
                    let mut keys: Vec<String> = (0..s.len()).map(|idx| idx.to_string()).collect();
                    keys.push("length".to_string());
                    keys
                }
                _ => return Err(VmError::TypeMismatch),
            };
            let mut names: Vec<Value> = Vec::with_capacity(owned.len());
            for k in owned {
                names.push(string_value(&k, string_heap)?);
            }
            Ok(Value::Array(crate::array::from_elements(gc_heap, names)?))
        }
        // §20.1.2.13 Object.getOwnPropertySymbols(O) — every own
        // symbol-keyed property. Foundation property bag is
        // string-keyed today; symbol keys are tracked in a parallel
        // table inside JsObject.
        // <https://tc39.es/ecma262/#sec-object.getownpropertysymbols>
        "getOwnPropertySymbols" => {
            let target = expect_object(args.first())?;
            let syms: Vec<Value> = crate::object::with_properties(target, gc_heap, |p| {
                p.symbol_keys().map(Value::Symbol).collect()
            });
            Ok(Value::Array(crate::array::from_elements(gc_heap, syms)?))
        }
        _ => Err(VmError::UnknownIntrinsic {
            name: format!("Object.{name}"),
        }),
    }
}

fn string_value(s: &str, heap: &StringHeap) -> Result<Value, VmError> {
    Ok(Value::String(
        JsString::from_str(s, heap).map_err(|_| VmError::TypeMismatch)?,
    ))
}

/// Implement §6.2.5.5 ToPropertyDescriptor against `desc_obj`.
///
/// # Algorithm
/// - Read `value`, `writable`, `enumerable`, `configurable`, `get`,
///   `set` from the descriptor object as own data properties.
/// - If `get` or `set` is present, build an [`DescriptorKind::Accessor`].
///   Mixing accessor + data fields rejects with `TypeMismatch` per
///   step 17 of the spec.
/// - If neither accessor field is present, build a
///   [`DescriptorKind::Data`] using `value` (defaulting to
///   `undefined`) and the writable bit (defaulting to `false`).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-topropertydescriptor>
pub fn coerce_to_descriptor(
    desc_obj: &JsObject,
    gc_heap: &otter_gc::GcHeap,
) -> Result<PropertyDescriptor, VmError> {
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

    let enumerable_bit = lookup_to_optional_bool(&enumerable);
    let configurable_bit = lookup_to_optional_bool(&configurable);

    if has_get || has_set {
        let getter_value = lookup_to_optional_value(&getter)?;
        let setter_value = lookup_to_optional_value(&setter)?;
        // Spec: `get` and `set` must be undefined or callable. The
        // callable check happens at install time inside
        // `define_own_property` (a non-callable getter is preserved
        // and would be invoked later, which the dispatcher rejects).
        return Ok(PropertyDescriptor::accessor(
            getter_value,
            setter_value,
            enumerable_bit.unwrap_or(false),
            configurable_bit.unwrap_or(false),
        ));
    }

    let data_value = match value {
        PropertyLookup::Absent => Value::Undefined,
        PropertyLookup::Data { value, .. } => value,
        PropertyLookup::Accessor { .. } => Value::Undefined,
    };
    let writable_bit = lookup_to_optional_bool(&writable).unwrap_or(false);
    Ok(PropertyDescriptor::data(
        data_value,
        writable_bit,
        enumerable_bit.unwrap_or(false),
        configurable_bit.unwrap_or(false),
    ))
}

/// Inverse of [`coerce_to_descriptor`] — returns a fresh
/// `{ value / writable / enumerable / configurable }` or
/// `{ get / set / enumerable / configurable }` object.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-frompropertydescriptor>
fn descriptor_to_object(
    desc: &PropertyDescriptor,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<JsObject, VmError> {
    let result = crate::object::alloc_object(gc_heap)?;
    match &desc.kind {
        DescriptorKind::Data { value } => {
            crate::object::set(result, gc_heap, "value", value.clone());
            crate::object::set(result, gc_heap, "writable", Value::Boolean(desc.writable()));
        }
        DescriptorKind::Accessor { getter, setter } => {
            crate::object::set(
                result,
                gc_heap,
                "get",
                getter.clone().unwrap_or(Value::Undefined),
            );
            crate::object::set(
                result,
                gc_heap,
                "set",
                setter.clone().unwrap_or(Value::Undefined),
            );
        }
    }
    crate::object::set(
        result,
        gc_heap,
        "enumerable",
        Value::Boolean(desc.enumerable()),
    );
    crate::object::set(
        result,
        gc_heap,
        "configurable",
        Value::Boolean(desc.configurable()),
    );
    Ok(result)
}

fn lookup_to_optional_bool(lookup: &PropertyLookup) -> Option<bool> {
    match lookup {
        PropertyLookup::Absent => None,
        PropertyLookup::Data { value, .. } => Some(value.to_boolean()),
        // An accessor on the descriptor object would fire its getter
        // per spec; we treat as absent in the slice.
        PropertyLookup::Accessor { .. } => None,
    }
}

fn lookup_to_optional_value(lookup: &PropertyLookup) -> Result<Option<Value>, VmError> {
    match lookup {
        PropertyLookup::Absent => Ok(None),
        PropertyLookup::Data { value, .. } => match value {
            Value::Undefined => Ok(None),
            v => Ok(Some(v.clone())),
        },
        PropertyLookup::Accessor { .. } => Ok(None),
    }
}

fn expect_object(arg: Option<&Value>) -> Result<JsObject, VmError> {
    match arg {
        Some(Value::Object(o)) => Ok(*o),
        _ => Err(VmError::TypeMismatch),
    }
}

fn expect_property_key(arg: Option<&Value>) -> Result<PropertyKey, VmError> {
    match arg {
        Some(Value::String(s)) => Ok(PropertyKey::String(s.to_lossy_string())),
        Some(Value::Number(n)) => Ok(PropertyKey::String(n.to_display_string())),
        Some(Value::Boolean(b)) => Ok(PropertyKey::String(
            (if *b { "true" } else { "false" }).to_string(),
        )),
        Some(Value::Null) => Ok(PropertyKey::String("null".to_string())),
        Some(Value::Undefined) | None => Ok(PropertyKey::String("undefined".to_string())),
        Some(Value::Symbol(sym)) => Ok(PropertyKey::Symbol(sym.clone())),
        _ => Err(VmError::TypeMismatch),
    }
}

fn has_own_property(
    target: JsObject,
    gc_heap: &otter_gc::GcHeap,
    key: Option<&Value>,
) -> Result<bool, VmError> {
    match expect_property_key(key)? {
        PropertyKey::Symbol(sym) => Ok(crate::object::has_own_symbol(target, gc_heap, &sym)),
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
fn property_key_from_value(value: &Value) -> Result<String, VmError> {
    match value {
        Value::String(s) => Ok(s.to_lossy_string()),
        Value::Number(n) => Ok(n.to_display_string()),
        Value::Boolean(b) => Ok((if *b { "true" } else { "false" }).to_string()),
        Value::Null => Ok("null".to_string()),
        Value::Undefined => Ok("undefined".to_string()),
        _ => Err(VmError::TypeMismatch),
    }
}
