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

use crate::object::{JsObject, PropertyLookup};
use crate::object_statics::coerce_to_descriptor;
use crate::string::JsString;
use crate::symbol::JsSymbol;
use crate::{Interpreter, Value, VmError};

enum PropertyKey {
    String(String),
    Symbol(JsSymbol),
}

/// Dispatch `Reflect.<name>(args...)`.
///
/// `interp_string_heap` is the runtime heap used for the rare cases
/// where this dispatcher needs to allocate (e.g. `ownKeys` returning
/// a string array). Most paths only forward existing `Value`s and
/// avoid allocation.
///
/// # Errors
/// - [`VmError::TypeMismatch`] for receiver / argument shape errors.
pub fn call(
    interp: &mut Interpreter,
    module: &BytecodeModule,
    method: otter_bytecode::method_id::ReflectMethod,
    args: &[Value],
    string_heap: &crate::string::StringHeap,
) -> Result<Value, VmError> {
    use otter_bytecode::method_id::ReflectMethod as M;
    match method {
        // §28.1.2 Reflect.apply(target, thisArgument, argumentsList)
        // <https://tc39.es/ecma262/#sec-reflect.apply>
        M::Apply => {
            let target = args.first().cloned().unwrap_or(Value::Undefined);
            if !is_callable(&target, interp.gc_heap()) {
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
        M::Construct => {
            let target = args.first().cloned().unwrap_or(Value::Undefined);
            if !is_constructor(&target, module, interp.gc_heap()) {
                return Err(VmError::NotCallable);
            }
            let new_target = args.get(2).cloned().unwrap_or_else(|| target.clone());
            if !is_constructor(&new_target, module, interp.gc_heap()) {
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
            interp.run_construct_sync(module, &target, new_target, argv)
        }
        // §28.1.4 Reflect.defineProperty(target, propertyKey, attributes)
        // <https://tc39.es/ecma262/#sec-reflect.defineproperty>
        M::DefineProperty => {
            let target = expect_object(args.first())?;
            let key = expect_property_key(args.get(1))?;
            let desc_obj = expect_object(args.get(2))?;
            let descriptor = {
                let heap = interp.gc_heap();
                coerce_to_descriptor(&desc_obj, heap)?
            };
            let heap = interp.gc_heap_mut();
            let ok = match &key {
                PropertyKey::String(key) => {
                    crate::object::define_own_property(target, heap, key, descriptor)
                }
                PropertyKey::Symbol(sym) => {
                    crate::object::define_own_symbol_property(target, heap, sym, descriptor)
                }
            };
            Ok(Value::Boolean(ok))
        }
        // §28.1.5 Reflect.deleteProperty(target, propertyKey)
        // <https://tc39.es/ecma262/#sec-reflect.deleteproperty>
        M::DeleteProperty => {
            let target = expect_object(args.first())?;
            let key = expect_property_key(args.get(1))?;
            let heap = interp.gc_heap_mut();
            let removed = match &key {
                PropertyKey::String(key) => crate::object::delete(target, heap, key),
                PropertyKey::Symbol(sym) => crate::object::delete_symbol(target, heap, sym),
            };
            Ok(Value::Boolean(removed))
        }
        // §28.1.6 Reflect.get(target, propertyKey[, receiver])
        // <https://tc39.es/ecma262/#sec-reflect.get>
        M::Get => {
            let target = expect_object(args.first())?;
            let key = expect_property_key(args.get(1))?;
            // Foundation: `receiver` is honoured by accessor
            // dispatch elsewhere; the simple data-property surface
            // here ignores the third argument.
            let heap = interp.gc_heap();
            let value = match &key {
                PropertyKey::String(key) => {
                    crate::object::get(target, heap, key).unwrap_or(Value::Undefined)
                }
                PropertyKey::Symbol(sym) => {
                    crate::object::get_symbol(target, heap, sym).unwrap_or(Value::Undefined)
                }
            };
            Ok(value)
        }
        // §28.1.7 Reflect.getOwnPropertyDescriptor(target, propertyKey)
        // <https://tc39.es/ecma262/#sec-reflect.getownpropertydescriptor>
        M::GetOwnPropertyDescriptor => {
            let target = expect_object(args.first())?;
            let key = expect_property_key(args.get(1))?;
            let lookup = {
                let heap = interp.gc_heap();
                match &key {
                    PropertyKey::String(key) => crate::object::lookup_own(target, heap, key),
                    PropertyKey::Symbol(sym) => crate::object::lookup_own_symbol(target, heap, sym),
                }
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
        M::GetPrototypeOf => {
            let target = expect_object(args.first())?;
            let heap = interp.gc_heap();
            Ok(crate::object::prototype(target, heap)
                .map(Value::Object)
                .unwrap_or(Value::Null))
        }
        // §28.1.9 Reflect.has(target, propertyKey)
        // <https://tc39.es/ecma262/#sec-reflect.has>
        M::Has => {
            let target = expect_object(args.first())?;
            let key = expect_property_key(args.get(1))?;
            let heap = interp.gc_heap();
            let present = match &key {
                PropertyKey::String(key) => !matches!(
                    crate::object::lookup(target, heap, key),
                    PropertyLookup::Absent
                ),
                PropertyKey::Symbol(sym) => !matches!(
                    crate::object::lookup_symbol(target, heap, sym),
                    PropertyLookup::Absent
                ),
            };
            Ok(Value::Boolean(present))
        }
        // §28.1.10 Reflect.isExtensible(target)
        // <https://tc39.es/ecma262/#sec-reflect.isextensible>
        M::IsExtensible => {
            let target = expect_object(args.first())?;
            let heap = interp.gc_heap();
            Ok(Value::Boolean(crate::object::is_extensible(target, heap)))
        }
        // §28.1.11 Reflect.ownKeys(target)
        // <https://tc39.es/ecma262/#sec-reflect.ownkeys>
        M::OwnKeys => {
            let target = expect_object(args.first())?;
            let heap = interp.gc_heap();
            let keys: Vec<Value> = crate::object::with_properties(target, heap, |p| {
                let mut keys: Vec<Value> = p
                    .keys()
                    .map(|k| {
                        JsString::from_str(k, string_heap)
                            .map(Value::String)
                            .unwrap_or(Value::Undefined)
                    })
                    .collect();
                keys.extend(p.symbol_keys().map(Value::Symbol));
                keys
            });
            Ok(Value::Array(crate::array::from_elements(
                interp.gc_heap_mut(),
                keys,
            )?))
        }
        // §28.1.12 Reflect.preventExtensions(target)
        // <https://tc39.es/ecma262/#sec-reflect.preventextensions>
        M::PreventExtensions => {
            let target = expect_object(args.first())?;
            let heap = interp.gc_heap_mut();
            crate::object::prevent_extensions(target, heap);
            Ok(Value::Boolean(true))
        }
        // §28.1.13 Reflect.set(target, propertyKey, V[, receiver])
        // <https://tc39.es/ecma262/#sec-reflect.set>
        M::Set => {
            let target = expect_object(args.first())?;
            let key = expect_property_key(args.get(1))?;
            let value = args.get(2).cloned().unwrap_or(Value::Undefined);
            let receiver = args.get(3).cloned().unwrap_or(Value::Object(target));
            let outcome = {
                let heap = interp.gc_heap();
                match &key {
                    PropertyKey::String(key) => crate::object::resolve_set(target, heap, key),
                    PropertyKey::Symbol(sym) => {
                        crate::object::resolve_symbol_set(target, heap, sym)
                    }
                }
            };
            let ok = match outcome {
                crate::object::SetOutcome::AssignData => {
                    let heap = interp.gc_heap_mut();
                    match &key {
                        PropertyKey::String(key) => {
                            crate::object::ordinary_set_data_property(target, heap, key, value)
                        }
                        PropertyKey::Symbol(sym) => {
                            crate::object::set_symbol(target, heap, sym.clone(), value)
                        }
                    }
                }
                crate::object::SetOutcome::InvokeSetter { setter } => {
                    if !is_callable(&setter, interp.gc_heap()) {
                        false
                    } else {
                        let argv: SmallVec<[Value; 8]> = smallvec::smallvec![value];
                        interp.run_callable_sync(module, &setter, receiver, argv)?;
                        true
                    }
                }
                crate::object::SetOutcome::Reject { .. } => false,
            };
            Ok(Value::Boolean(ok))
        }
        // §28.1.14 Reflect.setPrototypeOf(target, prototype)
        // <https://tc39.es/ecma262/#sec-reflect.setprototypeof>
        M::SetPrototypeOf => {
            let target = expect_object(args.first())?;
            let proto = match args.get(1) {
                Some(Value::Object(_)) | Some(Value::Proxy(_)) | Some(Value::Null) => {
                    args.get(1).cloned()
                }
                None => Some(Value::Null),
                _ => return Err(VmError::TypeMismatch),
            };
            let heap = interp.gc_heap_mut();
            if !crate::object::set_prototype_value(target, heap, proto) {
                return Err(VmError::TypeMismatch);
            }
            Ok(Value::Boolean(true))
        }
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

fn is_constructor(value: &Value, module: &BytecodeModule, heap: &otter_gc::GcHeap) -> bool {
    match value {
        Value::Object(obj) => matches!(
            crate::object::constructor_native(*obj, heap),
            Some(Value::NativeFunction(_))
        ),
        _ => crate::abstract_ops::is_constructor(value, module, heap),
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
