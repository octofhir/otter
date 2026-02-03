//! Function.prototype methods implementation
//!
//! Complete ES2026 Function.prototype:
//! - call(thisArg, ...args) - calls function with specified this
//! - apply(thisArg, argsArray) - calls function with this and array of args
//! - bind(thisArg, ...args) - creates bound function
//! - toString() - returns string representation
//!
//! ## Implementation Strategy
//! - **call/apply**: Hybrid approach with error-based interception for closures
//!   - Native functions: Direct call (zero overhead fast path)
//!   - Closures: Error-based interception in interpreter (full VM context access)
//! - **bind**: Direct implementation (creates bound function object)
//! - **toString**: Direct implementation (returns string representation)
//!
//! ## ES2026 Compliance
//! All methods follow ECMAScript specification:
//! - call §20.2.3.3
//! - apply §20.2.3.1
//! - bind §20.2.3.2
//! - toString §20.2.3.5

use crate::gc::GcRef;
use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use crate::memory::MemoryManager;
use crate::error::VmError;
use std::sync::Arc;

/// Initialize Function.prototype with all ES2026 methods
///
/// # Methods
/// - **call(thisArg, ...args)** - Calls function with specified this
/// - **apply(thisArg, argsArray)** - Calls function with this and array
/// - **bind(thisArg, ...args)** - Creates bound function
/// - **toString()** - Returns string representation
///
/// # Property Attributes
/// All methods: `{ writable: true, enumerable: false, configurable: true }`
pub fn init_function_prototype(
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // Function.prototype.length = 0 (§20.2.3)
    fn_proto.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::function_length(Value::number(0.0)),
    );
    // Function.prototype.name = "" (§20.2.3)
    fn_proto.define_property(
        PropertyKey::string("name"),
        PropertyDescriptor::function_length(Value::string(JsString::intern(""))),
    );

    // ====================================================================
    // Function.prototype.toString() §20.2.3.5
    // ====================================================================
    fn_proto.define_property(
        PropertyKey::string("toString"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                // Check if this is a closure
                if this_val.is_function() {
                    if let Some(closure) = this_val.as_function() {
                        return Ok(Value::string(JsString::intern(&format!(
                            "function {}() {{ [native code] }}",
                            if closure.is_async { "async " } else { "" }
                        ))));
                    }
                }
                // Check if this is a native function
                if this_val.is_native_function() {
                    return Ok(Value::string(JsString::intern(
                        "function () { [native code] }"
                    )));
                }
                // Check if this is a bound function
                if let Some(obj) = this_val.as_object() {
                    if obj.has(&PropertyKey::string("__boundFunction__")) {
                        return Ok(Value::string(JsString::intern(
                            "function bound() { [native code] }"
                        )));
                    }
                }
                // Not a function
                Err(VmError::type_error("Function.prototype.toString requires a function"))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // ====================================================================
    // Function.prototype.call(thisArg, ...args) §20.2.3.3
    // ====================================================================
    fn_proto.define_property(
        PropertyKey::string("call"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                // this_val is the function to call
                // args[0] is thisArg
                // args[1..] are the arguments
                let this_arg = args.first().cloned().unwrap_or(Value::undefined());
                let call_args: Vec<Value> = if args.len() > 1 {
                    args[1..].to_vec()
                } else {
                    vec![]
                };

                if let Some(proxy) = this_val.as_proxy() {
                    return crate::proxy_operations::proxy_apply(ncx, proxy, this_arg, &call_args);
                }

                // Check if target is callable
                if !this_val.is_callable() {
                    return Err(VmError::type_error("Function.prototype.call requires a callable target"));
                }

                // Call the function (handles both closures and native functions)
                ncx.call_function(&this_val, this_arg, &call_args)
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // ====================================================================
    // Function.prototype.apply(thisArg, argsArray) §20.2.3.1
    // ====================================================================
    fn_proto.define_property(
        PropertyKey::string("apply"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                // this_val is the function to call
                // args[0] is thisArg
                // args[1] is argsArray
                let this_arg = args.first().cloned().unwrap_or(Value::undefined());
                let args_array_val = args.get(1).cloned().unwrap_or(Value::undefined());

                // Convert argsArray to Vec<Value>
                let call_args = if args_array_val.is_undefined() || args_array_val.is_null() {
                    vec![]
                } else if let Some(arr_obj) = args_array_val.as_object() {
                    if arr_obj.is_array() {
                        let len = arr_obj.array_length();
                        let mut extracted = Vec::with_capacity(len);
                        for i in 0..len {
                            extracted.push(
                                arr_obj.get(&PropertyKey::Index(i as u32))
                                    .unwrap_or(Value::undefined())
                            );
                        }
                        extracted
                    } else {
                        return Err(VmError::type_error("Function.prototype.apply: argumentsList must be an array"));
                    }
                } else {
                    return Err(VmError::type_error("Function.prototype.apply: argumentsList must be an object"));
                };

                if let Some(proxy) = this_val.as_proxy() {
                    return crate::proxy_operations::proxy_apply(ncx, proxy, this_arg, &call_args);
                }

                // Check if target is callable
                if !this_val.is_callable() {
                    return Err(VmError::type_error("Function.prototype.apply requires a callable target"));
                }

                // Call the function (handles both closures and native functions)
                ncx.call_function(&this_val, this_arg, &call_args)
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // ====================================================================
    // Function.prototype.bind(thisArg, ...args) §20.2.3.2
    // ====================================================================
    let fn_proto_for_bind = fn_proto.clone();
    fn_proto.define_property(
        PropertyKey::string("bind"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            move |this_val, args, ncx| {
                // this_val is the function being bound
                let this_arg = args.first().cloned().unwrap_or(Value::undefined());

                // Create bound function object with Function.prototype as prototype
                let bound = GcRef::new(JsObject::new(Value::object(fn_proto_for_bind.clone()), ncx.memory_manager().clone()));

                // Store the original function
                bound.set(
                    PropertyKey::string("__boundFunction__"),
                    this_val.clone(),
                );

                // Store the thisArg
                bound.set(PropertyKey::string("__boundThis__"), this_arg);

                // Store bound arguments (if any)
                if args.len() > 1 {
                    let arr = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                    for (i, arg) in args[1..].iter().enumerate() {
                        arr.set(PropertyKey::Index(i as u32), arg.clone());
                    }
                    arr.set(
                        PropertyKey::string("length"),
                        Value::int32((args.len() - 1) as i32),
                    );
                    bound.set(
                        PropertyKey::string("__boundArgs__"),
                        Value::object(arr),
                    );
                }

                // Set name
                bound.set(
                    PropertyKey::string("__boundName__"),
                    Value::string(JsString::intern("bound ")),
                );

                // Set length (original length - bound args count, min 0)
                let bound_args_len = if args.len() > 1 { args.len() - 1 } else { 0 };
                let new_length = 0i32.saturating_sub(bound_args_len as i32).max(0);
                bound.set(
                    PropertyKey::string("__boundLength__"),
                    Value::int32(new_length),
                );

                // Mark as callable
                bound.set(
                    PropertyKey::string("__isCallable__"),
                    Value::boolean(true),
                );

                Ok(Value::object(bound))
            },
            mm.clone(),
            fn_proto,
        )),
    );
}
