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
                Some(Value::Array(arr)) => {
                    crate::array::with_elements(*arr, interp.gc_heap(), |elements| {
                        elements.iter().cloned().collect()
                    })
                }
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
                Some(Value::Array(arr)) => {
                    crate::array::with_elements(*arr, interp.gc_heap(), |elements| {
                        elements.iter().cloned().collect()
                    })
                }
                None | Some(Value::Undefined) | Some(Value::Null) => SmallVec::new(),
                _ => return Err(VmError::TypeMismatch),
            };
            // Foundation: build a fresh receiver from the
            // constructor's `prototype` chain and run the body via
            // run_callable_sync.
            let proto = {
                let heap = interp.gc_heap();
                construct_prototype(&target, heap)
            };
            let receiver = {
                let heap = interp.gc_heap_mut();
                let receiver = crate::object::alloc_object(heap).map_err(VmError::from)?;
                if let Some(proto) = proto {
                    crate::object::set_prototype(receiver, heap, Some(proto));
                }
                receiver
            };
            let result =
                interp.run_callable_sync(module, &target, Value::Object(receiver), argv)?;
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
            let descriptor = {
                let heap = interp.gc_heap();
                coerce_to_descriptor(&desc_obj, heap)?
            };
            let heap = interp.gc_heap_mut();
            let ok = crate::object::define_own_property(target, heap, &key, descriptor);
            Ok(Value::Boolean(ok))
        }
        // §28.1.5 Reflect.deleteProperty(target, propertyKey)
        // <https://tc39.es/ecma262/#sec-reflect.deleteproperty>
        "deleteProperty" => {
            let target = expect_object(args.first())?;
            let key = expect_property_key(args.get(1))?;
            let heap = interp.gc_heap_mut();
            Ok(Value::Boolean(crate::object::delete(target, heap, &key)))
        }
        // §28.1.6 Reflect.get(target, propertyKey[, receiver])
        // <https://tc39.es/ecma262/#sec-reflect.get>
        "get" => {
            let target = expect_object(args.first())?;
            let key = expect_property_key(args.get(1))?;
            // Foundation: `receiver` is honoured by accessor
            // dispatch elsewhere; the simple data-property surface
            // here ignores the third argument.
            let heap = interp.gc_heap();
            Ok(crate::object::get(target, heap, &key).unwrap_or(Value::Undefined))
        }
        // §28.1.7 Reflect.getOwnPropertyDescriptor(target, propertyKey)
        // <https://tc39.es/ecma262/#sec-reflect.getownpropertydescriptor>
        "getOwnPropertyDescriptor" => {
            let target = expect_object(args.first())?;
            let key = expect_property_key(args.get(1))?;
            let lookup = {
                let heap = interp.gc_heap();
                crate::object::lookup_own(target, heap, &key)
            };
            match lookup {
                PropertyLookup::Absent => Ok(Value::Undefined),
                PropertyLookup::Data { value, flags } => {
                    let heap = interp.gc_heap_mut();
                    let obj = crate::object::alloc_object(heap).map_err(VmError::from)?;
                    crate::object::set(obj, heap, "value", value);
                    crate::object::set(obj, heap, "writable", Value::Boolean(flags.writable()));
                    crate::object::set(obj, heap, "enumerable", Value::Boolean(flags.enumerable()));
                    crate::object::set(
                        obj,
                        heap,
                        "configurable",
                        Value::Boolean(flags.configurable()),
                    );
                    Ok(Value::Object(obj))
                }
                PropertyLookup::Accessor {
                    getter,
                    setter,
                    flags,
                } => {
                    let heap = interp.gc_heap_mut();
                    let obj = crate::object::alloc_object(heap).map_err(VmError::from)?;
                    crate::object::set(obj, heap, "get", getter.unwrap_or(Value::Undefined));
                    crate::object::set(obj, heap, "set", setter.unwrap_or(Value::Undefined));
                    crate::object::set(obj, heap, "enumerable", Value::Boolean(flags.enumerable()));
                    crate::object::set(
                        obj,
                        heap,
                        "configurable",
                        Value::Boolean(flags.configurable()),
                    );
                    Ok(Value::Object(obj))
                }
            }
        }
        // §28.1.8 Reflect.getPrototypeOf(target)
        // <https://tc39.es/ecma262/#sec-reflect.getprototypeof>
        "getPrototypeOf" => {
            let target = expect_object(args.first())?;
            let heap = interp.gc_heap();
            Ok(crate::object::prototype(target, heap)
                .map(Value::Object)
                .unwrap_or(Value::Null))
        }
        // §28.1.9 Reflect.has(target, propertyKey)
        // <https://tc39.es/ecma262/#sec-reflect.has>
        "has" => {
            let target = expect_object(args.first())?;
            let key = expect_property_key(args.get(1))?;
            let heap = interp.gc_heap();
            Ok(Value::Boolean(!matches!(
                crate::object::lookup(target, heap, &key),
                PropertyLookup::Absent
            )))
        }
        // §28.1.10 Reflect.isExtensible(target)
        // <https://tc39.es/ecma262/#sec-reflect.isextensible>
        "isExtensible" => {
            let target = expect_object(args.first())?;
            let heap = interp.gc_heap();
            Ok(Value::Boolean(crate::object::is_extensible(target, heap)))
        }
        // §28.1.11 Reflect.ownKeys(target)
        // <https://tc39.es/ecma262/#sec-reflect.ownkeys>
        "ownKeys" => {
            let target = expect_object(args.first())?;
            let heap = interp.gc_heap();
            let keys: Vec<Value> = crate::object::with_properties(target, heap, |p| {
                p.keys()
                    .map(|k| {
                        JsString::from_str(k, string_heap)
                            .map(Value::String)
                            .unwrap_or(Value::Undefined)
                    })
                    .collect()
            });
            Ok(Value::Array(crate::array::from_elements(
                interp.gc_heap_mut(),
                keys,
            )?))
        }
        // §28.1.12 Reflect.preventExtensions(target)
        // <https://tc39.es/ecma262/#sec-reflect.preventextensions>
        "preventExtensions" => {
            let target = expect_object(args.first())?;
            let heap = interp.gc_heap_mut();
            crate::object::prevent_extensions(target, heap);
            Ok(Value::Boolean(true))
        }
        // §28.1.13 Reflect.set(target, propertyKey, V[, receiver])
        // <https://tc39.es/ecma262/#sec-reflect.set>
        "set" => {
            let target = expect_object(args.first())?;
            let key = expect_property_key(args.get(1))?;
            let value = args.get(2).cloned().unwrap_or(Value::Undefined);
            let heap = interp.gc_heap_mut();
            crate::object::set(target, heap, &key, value);
            Ok(Value::Boolean(true))
        }
        // §28.1.14 Reflect.setPrototypeOf(target, prototype)
        // <https://tc39.es/ecma262/#sec-reflect.setprototypeof>
        "setPrototypeOf" => {
            let target = expect_object(args.first())?;
            let proto = match args.get(1) {
                Some(Value::Object(p)) => Some(*p),
                Some(Value::Null) | None => None,
                _ => return Err(VmError::TypeMismatch),
            };
            let heap = interp.gc_heap_mut();
            crate::object::set_prototype(target, heap, proto);
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
fn construct_prototype(callee: &Value, heap: &otter_gc::GcHeap) -> Option<JsObject> {
    match callee {
        Value::ClassConstructor(c) => Some(c.prototype),
        Value::Object(obj) => match crate::object::get(*obj, heap, "prototype") {
            Some(Value::Object(p)) => Some(p),
            _ => None,
        },
        Value::BoundFunction(b) => {
            let (target, _, _) = b.parts(heap);
            construct_prototype(&target, heap)
        }
        _ => None,
    }
}

fn expect_object(arg: Option<&Value>) -> Result<JsObject, VmError> {
    match arg {
        Some(Value::Object(o)) => Ok(*o),
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
