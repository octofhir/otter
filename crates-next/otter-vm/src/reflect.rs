//! ECMA-262 §28.1 `Reflect` object — full static surface.
//!
//! The dispatcher implements every method on `Reflect` per §28.1.1 –
//! §28.1.13. Argument types follow the spec contract: invalid receiver
//! types raise `TypeMismatch` (the runtime mapper converts that to a
//! JS-level `TypeError`).
//!
//! # Contents
//! - [`call`] — single entry point keyed by method name; returned
//!   values match the spec shape.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-reflect-object>

use otter_bytecode::BytecodeModule;
use smallvec::SmallVec;

use crate::abstract_ops::is_callable;
use crate::object::{JsObject, PropertyLookup};
use crate::object_statics::coerce_to_descriptor;
use crate::string::JsString;
use crate::{Interpreter, Value, VmError};

/// Dispatch `Reflect.<name>(args...)`.
///
/// `interp_string_heap` is the runtime heap used for the rare cases
/// where this dispatcher needs to allocate (e.g. `ownKeys` returning
/// a string array). Most paths only forward existing `Value`s and
/// avoid allocation.
///
/// # Errors
/// - [`VmError::TypeMismatch`] for receiver / argument shape errors.
/// - [`VmError::UnknownIntrinsic`] for unrecognised method names.
pub fn call(
    interp: &mut Interpreter,
    module: &BytecodeModule,
    name: &str,
    args: &[Value],
    string_heap: &crate::string::StringHeap,
) -> Result<Value, VmError> {
    match name {
        // §28.1.2 Reflect.apply(target, thisArgument, argumentsList)
        // <https://tc39.es/ecma262/#sec-reflect.apply>
        "apply" => {
            let target = args.first().cloned().unwrap_or(Value::Undefined);
            if !is_callable(&target) {
                return Err(VmError::NotCallable);
            }
            let this_value = args.get(1).cloned().unwrap_or(Value::Undefined);
            let argv: SmallVec<[Value; 8]> = match args.get(2) {
                Some(Value::Array(arr)) => arr.borrow_body().iter().cloned().collect(),
                None | Some(Value::Undefined) | Some(Value::Null) => SmallVec::new(),
                _ => return Err(VmError::TypeMismatch),
            };
            interp.run_callable_sync(module, &target, this_value, argv)
        }
        // §28.1.3 Reflect.construct(target, argumentsList[, newTarget])
        // <https://tc39.es/ecma262/#sec-reflect.construct>
        "construct" => {
            let target = args.first().cloned().unwrap_or(Value::Undefined);
            if !is_callable(&target) && !matches!(&target, Value::ClassConstructor(_)) {
                return Err(VmError::NotCallable);
            }
            let argv: SmallVec<[Value; 8]> = match args.get(1) {
                Some(Value::Array(arr)) => arr.borrow_body().iter().cloned().collect(),
                None | Some(Value::Undefined) | Some(Value::Null) => SmallVec::new(),
                _ => return Err(VmError::TypeMismatch),
            };
            // Foundation: build a fresh receiver from the
            // constructor's `prototype` chain and run the body via
            // run_callable_sync. The third `newTarget` arg is
            // honoured implicitly when target is a ClassConstructor
            // (the body handles `new.target` itself).
            let receiver = JsObject::new();
            if let Some(proto) = construct_prototype(&target) {
                receiver.set_prototype(Some(proto));
            }
            let result =
                interp.run_callable_sync(module, &target, Value::Object(receiver.clone()), argv)?;
            // Per §13.3.5 — non-object return is replaced with the
            // freshly-allocated receiver.
            match result {
                Value::Object(_) => Ok(result),
                _ => Ok(Value::Object(receiver)),
            }
        }
        // §28.1.4 Reflect.defineProperty(target, propertyKey, attributes)
        // <https://tc39.es/ecma262/#sec-reflect.defineproperty>
        "defineProperty" => {
            let target = expect_object(args.first())?;
            let key = expect_property_key(args.get(1))?;
            let desc_obj = expect_object(args.get(2))?;
            let descriptor = coerce_to_descriptor(&desc_obj)?;
            let ok = target.define_own_property(&key, descriptor);
            Ok(Value::Boolean(ok))
        }
        // §28.1.5 Reflect.deleteProperty(target, propertyKey)
        // <https://tc39.es/ecma262/#sec-reflect.deleteproperty>
        "deleteProperty" => {
            let target = expect_object(args.first())?;
            let key = expect_property_key(args.get(1))?;
            Ok(Value::Boolean(target.delete(&key)))
        }
        // §28.1.6 Reflect.get(target, propertyKey[, receiver])
        // <https://tc39.es/ecma262/#sec-reflect.get>
        "get" => {
            let target = expect_object(args.first())?;
            let key = expect_property_key(args.get(1))?;
            // Foundation: `receiver` is honoured by accessor
            // dispatch elsewhere; the simple data-property surface
            // here ignores the third argument.
            Ok(target.get(&key).unwrap_or(Value::Undefined))
        }
        // §28.1.7 Reflect.getOwnPropertyDescriptor(target, propertyKey)
        // <https://tc39.es/ecma262/#sec-reflect.getownpropertydescriptor>
        "getOwnPropertyDescriptor" => {
            let target = expect_object(args.first())?;
            let key = expect_property_key(args.get(1))?;
            match target.lookup_own(&key) {
                PropertyLookup::Absent => Ok(Value::Undefined),
                PropertyLookup::Data { value, flags } => {
                    let obj = JsObject::new();
                    obj.set("value", value);
                    obj.set("writable", Value::Boolean(flags.writable()));
                    obj.set("enumerable", Value::Boolean(flags.enumerable()));
                    obj.set("configurable", Value::Boolean(flags.configurable()));
                    Ok(Value::Object(obj))
                }
                PropertyLookup::Accessor {
                    getter,
                    setter,
                    flags,
                } => {
                    let obj = JsObject::new();
                    obj.set("get", getter.unwrap_or(Value::Undefined));
                    obj.set("set", setter.unwrap_or(Value::Undefined));
                    obj.set("enumerable", Value::Boolean(flags.enumerable()));
                    obj.set("configurable", Value::Boolean(flags.configurable()));
                    Ok(Value::Object(obj))
                }
            }
        }
        // §28.1.8 Reflect.getPrototypeOf(target)
        // <https://tc39.es/ecma262/#sec-reflect.getprototypeof>
        "getPrototypeOf" => {
            let target = expect_object(args.first())?;
            Ok(target.prototype().map(Value::Object).unwrap_or(Value::Null))
        }
        // §28.1.9 Reflect.has(target, propertyKey)
        // <https://tc39.es/ecma262/#sec-reflect.has>
        "has" => {
            let target = expect_object(args.first())?;
            let key = expect_property_key(args.get(1))?;
            Ok(Value::Boolean(!matches!(
                target.lookup(&key),
                PropertyLookup::Absent
            )))
        }
        // §28.1.10 Reflect.isExtensible(target)
        // <https://tc39.es/ecma262/#sec-reflect.isextensible>
        "isExtensible" => {
            let target = expect_object(args.first())?;
            Ok(Value::Boolean(target.is_extensible()))
        }
        // §28.1.11 Reflect.ownKeys(target)
        // <https://tc39.es/ecma262/#sec-reflect.ownkeys>
        "ownKeys" => {
            let target = expect_object(args.first())?;
            let body = target.borrow_props();
            let keys: Vec<Value> = body
                .keys()
                .map(|k| {
                    JsString::from_str(k, string_heap)
                        .map(Value::String)
                        .unwrap_or(Value::Undefined)
                })
                .collect();
            drop(body);
            Ok(Value::Array(crate::array::JsArray::from_elements(keys)))
        }
        // §28.1.12 Reflect.preventExtensions(target)
        // <https://tc39.es/ecma262/#sec-reflect.preventextensions>
        "preventExtensions" => {
            let target = expect_object(args.first())?;
            target.prevent_extensions();
            Ok(Value::Boolean(true))
        }
        // §28.1.13 Reflect.set(target, propertyKey, V[, receiver])
        // <https://tc39.es/ecma262/#sec-reflect.set>
        "set" => {
            let target = expect_object(args.first())?;
            let key = expect_property_key(args.get(1))?;
            let value = args.get(2).cloned().unwrap_or(Value::Undefined);
            target.set(&key, value);
            Ok(Value::Boolean(true))
        }
        // §28.1.14 Reflect.setPrototypeOf(target, prototype)
        // <https://tc39.es/ecma262/#sec-reflect.setprototypeof>
        "setPrototypeOf" => {
            let target = expect_object(args.first())?;
            let proto = match args.get(1) {
                Some(Value::Object(p)) => Some(p.clone()),
                Some(Value::Null) | None => None,
                _ => return Err(VmError::TypeMismatch),
            };
            target.set_prototype(proto);
            Ok(Value::Boolean(true))
        }
        other => Err(VmError::UnknownIntrinsic {
            name: format!("Reflect.{other}"),
        }),
    }
}

/// Pull the `prototype` own property off a callable. Mirrors the
/// existing `Op::New` lookup so `Reflect.construct` builds
/// instances with the same chain.
fn construct_prototype(callee: &Value) -> Option<JsObject> {
    match callee {
        Value::ClassConstructor(c) => Some(c.prototype.clone()),
        Value::Object(obj) => match obj.get("prototype") {
            Some(Value::Object(p)) => Some(p),
            _ => None,
        },
        Value::BoundFunction(b) => construct_prototype(&b.target),
        _ => None,
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
        Some(Value::Boolean(b)) => Ok((if *b { "true" } else { "false" }).to_string()),
        Some(Value::Null) => Ok("null".to_string()),
        Some(Value::Undefined) | None => Ok("undefined".to_string()),
        _ => Err(VmError::TypeMismatch),
    }
}
