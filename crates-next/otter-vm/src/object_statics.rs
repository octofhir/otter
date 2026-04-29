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
//!   ([`JsObject::get_own`]). User-installed accessors / inherited
//!   descriptor fields are intentionally ignored in this slice; the
//!   spec uses the full `[[Get]]` ladder, but the ergonomic surface
//!   (`Object.defineProperty(o, 'k', { value: 1 })`) doesn't depend
//!   on it. Filed against task 60 for full faithfulness.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-object-constructor>
//! - <https://tc39.es/ecma262/#sec-topropertydescriptor>
//! - <https://tc39.es/ecma262/#sec-setintegritylevel>

use crate::array::JsArray;
use crate::object::{DescriptorKind, JsObject, PropertyDescriptor, PropertyLookup};
use crate::string::{JsString, StringHeap};
use crate::{Value, VmError};

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
pub fn call(name: &str, args: &[Value], string_heap: &StringHeap) -> Result<Value, VmError> {
    match name {
        // §20.1.2.4 Object.defineProperty(O, P, Attributes)
        // <https://tc39.es/ecma262/#sec-object.defineproperty>
        "defineProperty" => {
            let target = expect_object(args.first())?;
            let key = expect_property_key(args.get(1))?;
            let desc_obj = expect_object(args.get(2))?;
            let descriptor = coerce_to_descriptor(&desc_obj)?;
            if !target.define_own_property(&key, descriptor) {
                return Err(VmError::TypeMismatch);
            }
            Ok(Value::Object(target))
        }
        // §20.1.2.5 Object.defineProperties(O, Properties)
        // <https://tc39.es/ecma262/#sec-object.defineproperties>
        "defineProperties" => {
            let target = expect_object(args.first())?;
            let props = expect_object(args.get(1))?;
            // Walk enumerable own keys of `props`. Each value is a
            // descriptor object that we coerce + apply.
            let entries: Vec<(String, Value)> = props
                .borrow_props()
                .enumerable_data_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
            for (key, desc_value) in entries {
                let desc_obj = match desc_value {
                    Value::Object(o) => o,
                    _ => return Err(VmError::TypeMismatch),
                };
                let descriptor = coerce_to_descriptor(&desc_obj)?;
                if !target.define_own_property(&key, descriptor) {
                    return Err(VmError::TypeMismatch);
                }
            }
            Ok(Value::Object(target))
        }
        // §20.1.2.10 Object.getOwnPropertyDescriptor(O, P)
        // <https://tc39.es/ecma262/#sec-object.getownpropertydescriptor>
        "getOwnPropertyDescriptor" => {
            let target = expect_object(args.first())?;
            let key = expect_property_key(args.get(1))?;
            match target.get_own_descriptor(&key) {
                Some(desc) => Ok(Value::Object(descriptor_to_object(&desc))),
                None => Ok(Value::Undefined),
            }
        }
        // §20.1.2.11 Object.getOwnPropertyDescriptors(O)
        // <https://tc39.es/ecma262/#sec-object.getownpropertydescriptors>
        "getOwnPropertyDescriptors" => {
            let target = expect_object(args.first())?;
            let result = JsObject::new();
            // Walk every own string key (enumerable or not) — spec
            // says all own keys.
            let keys: Vec<String> = target
                .borrow_props()
                .keys()
                .map(|s| s.to_string())
                .collect();
            for key in keys {
                if let Some(desc) = target.get_own_descriptor(&key) {
                    result.set(&key, Value::Object(descriptor_to_object(&desc)));
                }
            }
            Ok(Value::Object(result))
        }
        // §20.1.2.6 Object.freeze(O)
        // <https://tc39.es/ecma262/#sec-object.freeze>
        "freeze" => {
            let arg = args.first().cloned().unwrap_or(Value::Undefined);
            if let Value::Object(o) = &arg {
                o.freeze();
            }
            // Spec: returns the argument unchanged (non-objects pass
            // through).
            Ok(arg)
        }
        // §20.1.2.20 Object.seal(O)
        "seal" => {
            let arg = args.first().cloned().unwrap_or(Value::Undefined);
            if let Value::Object(o) = &arg {
                o.seal();
            }
            Ok(arg)
        }
        // §20.1.2.18 Object.preventExtensions(O)
        "preventExtensions" => {
            let arg = args.first().cloned().unwrap_or(Value::Undefined);
            if let Value::Object(o) = &arg {
                o.prevent_extensions();
            }
            Ok(arg)
        }
        // §20.1.2.15 Object.isFrozen(O)
        "isFrozen" => {
            let arg = args.first().cloned().unwrap_or(Value::Undefined);
            // Per spec, `Object.isFrozen(non_object) === true`.
            let result = match arg {
                Value::Object(o) => o.is_frozen(),
                _ => true,
            };
            Ok(Value::Boolean(result))
        }
        // §20.1.2.16 Object.isSealed(O)
        "isSealed" => {
            let arg = args.first().cloned().unwrap_or(Value::Undefined);
            let result = match arg {
                Value::Object(o) => o.is_sealed(),
                _ => true,
            };
            Ok(Value::Boolean(result))
        }
        // §20.1.2.14 Object.isExtensible(O)
        "isExtensible" => {
            let arg = args.first().cloned().unwrap_or(Value::Undefined);
            // Spec: `Object.isExtensible(non_object) === false`.
            let result = match arg {
                Value::Object(o) => o.is_extensible(),
                _ => false,
            };
            Ok(Value::Boolean(result))
        }
        // §20.1.2.17 Object.keys(O) — enumerable own string keys.
        // <https://tc39.es/ecma262/#sec-object.keys>
        "keys" => {
            let target = expect_object(args.first())?;
            let owned: Vec<String> = target
                .borrow_props()
                .enumerable_keys()
                .map(|k| k.to_string())
                .collect();
            let mut names: Vec<Value> = Vec::with_capacity(owned.len());
            for k in owned {
                names.push(string_value(&k, string_heap)?);
            }
            Ok(Value::Array(JsArray::from_elements(names)))
        }
        // §20.1.2.22 Object.values(O) — enumerable own data values.
        // <https://tc39.es/ecma262/#sec-object.values>
        "values" => {
            let target = expect_object(args.first())?;
            let values: Vec<Value> = target
                .borrow_props()
                .enumerable_data_iter()
                .map(|(_, v)| v)
                .collect();
            Ok(Value::Array(JsArray::from_elements(values)))
        }
        // §20.1.2.5 Object.entries(O) — `[key, value]` pairs in
        // insertion order.
        // <https://tc39.es/ecma262/#sec-object.entries>
        "entries" => {
            let target = expect_object(args.first())?;
            let raw: Vec<(String, Value)> = target
                .borrow_props()
                .enumerable_data_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
            let mut pairs: Vec<Value> = Vec::with_capacity(raw.len());
            for (k, v) in raw {
                let key = string_value(&k, string_heap)?;
                let pair: smallvec::SmallVec<[Value; 4]> = smallvec::smallvec![key, v];
                pairs.push(Value::Array(JsArray::from_elements(pair)));
            }
            Ok(Value::Array(JsArray::from_elements(pairs)))
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
                        let entries: Vec<(String, Value)> = o
                            .borrow_props()
                            .enumerable_data_iter()
                            .map(|(k, v)| (k.to_string(), v))
                            .collect();
                        for (k, v) in entries {
                            target.set(&k, v);
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
            let result = JsObject::new();
            match iter {
                Value::Array(arr) => {
                    // Snapshot to avoid holding the array's RefCell
                    // borrow while we recurse into per-pair work.
                    let snapshot: Vec<Value> = arr.borrow_body().iter().cloned().collect();
                    for entry in snapshot {
                        match entry {
                            Value::Array(pair) => {
                                let key = pair.get(0);
                                let value = pair.get(1);
                                let key_str = property_key_from_value(&key)?;
                                result.set(&key_str, value);
                            }
                            _ => return Err(VmError::TypeMismatch),
                        }
                    }
                }
                Value::Map(m) => {
                    for (key, value) in m.entries() {
                        let key_str = property_key_from_value(&key)?;
                        result.set(&key_str, value);
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
            let target = expect_object(args.first())?;
            let key = expect_property_key(args.get(1))?;
            let present = !matches!(target.lookup_own(&key), PropertyLookup::Absent);
            Ok(Value::Boolean(present))
        }
        // §20.1.2.12 Object.getOwnPropertyNames(O) — every own
        // string-keyed property, regardless of enumerability.
        // <https://tc39.es/ecma262/#sec-object.getownpropertynames>
        "getOwnPropertyNames" => {
            let target = expect_object(args.first())?;
            let owned: Vec<String> = target
                .borrow_props()
                .keys()
                .map(|k| k.to_string())
                .collect();
            let mut names: Vec<Value> = Vec::with_capacity(owned.len());
            for k in owned {
                names.push(string_value(&k, string_heap)?);
            }
            Ok(Value::Array(JsArray::from_elements(names)))
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
pub fn coerce_to_descriptor(desc_obj: &JsObject) -> Result<PropertyDescriptor, VmError> {
    // Direct own-data probes — accessors on the descriptor object
    // itself are ignored for the slice.
    let value = desc_obj.lookup_own("value");
    let writable = desc_obj.lookup_own("writable");
    let enumerable = desc_obj.lookup_own("enumerable");
    let configurable = desc_obj.lookup_own("configurable");
    let getter = desc_obj.lookup_own("get");
    let setter = desc_obj.lookup_own("set");

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
fn descriptor_to_object(desc: &PropertyDescriptor) -> JsObject {
    let result = JsObject::new();
    match &desc.kind {
        DescriptorKind::Data { value } => {
            result.set("value", value.clone());
            result.set("writable", Value::Boolean(desc.writable()));
        }
        DescriptorKind::Accessor { getter, setter } => {
            result.set("get", getter.clone().unwrap_or(Value::Undefined));
            result.set("set", setter.clone().unwrap_or(Value::Undefined));
        }
    }
    result.set("enumerable", Value::Boolean(desc.enumerable()));
    result.set("configurable", Value::Boolean(desc.configurable()));
    result
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
        Some(Value::Object(o)) => Ok(o.clone()),
        _ => Err(VmError::TypeMismatch),
    }
}

fn expect_property_key(arg: Option<&Value>) -> Result<String, VmError> {
    match arg {
        Some(Value::String(s)) => Ok(s.to_lossy_string()),
        Some(Value::Number(n)) => Ok(n.to_display_string()),
        _ => Err(VmError::TypeMismatch),
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
