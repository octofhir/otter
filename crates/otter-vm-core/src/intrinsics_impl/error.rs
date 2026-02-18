//! Error.prototype methods and Error constructor implementation
//!
//! All Error object methods for ES2026 standard, including stack trace support.

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use otter_macros::dive;
use std::sync::Arc;

fn native_from_decl_with_function_proto(
    native_fn: crate::value::NativeFn,
    mm: &Arc<MemoryManager>,
    fn_proto: GcRef<JsObject>,
) -> Value {
    // Preserve historical function object shape from native_function_with_proto:
    // [[Prototype]] = %Function.prototype%, name="", length=0.
    let object = GcRef::new(JsObject::new(Value::object(fn_proto), mm.clone()));
    object.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::function_length(Value::int32(0)),
    );
    object.define_property(
        PropertyKey::string("name"),
        PropertyDescriptor::function_length(Value::string(JsString::intern(""))),
    );
    Value::native_function_with_proto_and_object(native_fn, mm.clone(), fn_proto, object)
}

#[dive(name = "toString", length = 0)]
fn error_to_string(
    this_val: &Value,
    _args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    if let Some(obj) = this_val.as_object() {
        let name = obj
            .get(&PropertyKey::string("name"))
            .and_then(|v| v.as_string())
            .map(|s| s.as_str().to_string())
            .unwrap_or_else(|| "Error".to_string());
        let msg = obj
            .get(&PropertyKey::string("message"))
            .and_then(|v| v.as_string())
            .map(|s| s.as_str().to_string())
            .unwrap_or_default();
        if msg.is_empty() {
            Ok(Value::string(JsString::intern(&name)))
        } else {
            Ok(Value::string(JsString::intern(&format!(
                "{}: {}",
                name, msg
            ))))
        }
    } else {
        Ok(Value::string(JsString::intern("Error")))
    }
}

#[dive(name = "stack", length = 0)]
fn error_stack_getter(
    this_val: &Value,
    _args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    if let Some(obj) = this_val.as_object() {
        let name = obj
            .get(&PropertyKey::string("name"))
            .and_then(|v| v.as_string())
            .map(|s| s.as_str().to_string())
            .unwrap_or_else(|| "Error".to_string());
        let message = obj
            .get(&PropertyKey::string("message"))
            .and_then(|v| v.as_string())
            .map(|s| s.as_str().to_string())
            .unwrap_or_default();

        let frames = obj.get(&PropertyKey::string("__stack_frames__"));

        let mut stack = if message.is_empty() {
            name.clone()
        } else {
            format!("{}: {}", name, message)
        };

        if let Some(frames_val) = frames {
            if let Some(frames_arr) = frames_val.as_object() {
                if let Some(len_val) = frames_arr.get(&PropertyKey::string("length")) {
                    if let Some(len) = len_val.as_number() {
                        for i in 0..(len as u32) {
                            if let Some(frame_val) = frames_arr.get(&PropertyKey::Index(i)) {
                                if let Some(frame) = frame_val.as_object() {
                                    let func = frame
                                        .get(&PropertyKey::string("function"))
                                        .and_then(|v| v.as_string())
                                        .map(|s| s.as_str().to_string())
                                        .unwrap_or_else(|| "<anonymous>".to_string());
                                    let file = frame
                                        .get(&PropertyKey::string("file"))
                                        .and_then(|v| v.as_string())
                                        .map(|s| s.as_str().to_string());
                                    let line = frame
                                        .get(&PropertyKey::string("line"))
                                        .and_then(|v| v.as_number())
                                        .map(|n| n as u32);
                                    let column = frame
                                        .get(&PropertyKey::string("column"))
                                        .and_then(|v| v.as_number())
                                        .map(|n| n as u32);

                                    stack.push_str("\n    at ");
                                    stack.push_str(&func);
                                    if let Some(file_str) = file {
                                        stack.push_str(" (");
                                        stack.push_str(&file_str);
                                        if let Some(l) = line {
                                            stack.push_str(&format!(":{}", l));
                                            if let Some(c) = column {
                                                stack.push_str(&format!(":{}", c));
                                            }
                                        }
                                        stack.push(')');
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(Value::string(JsString::intern(&stack)))
    } else {
        Ok(Value::undefined())
    }
}

/// Initialize Error.prototype and all error type prototypes
pub fn init_error_prototypes(
    error_proto: GcRef<JsObject>,
    type_error_proto: GcRef<JsObject>,
    range_error_proto: GcRef<JsObject>,
    reference_error_proto: GcRef<JsObject>,
    syntax_error_proto: GcRef<JsObject>,
    uri_error_proto: GcRef<JsObject>,
    eval_error_proto: GcRef<JsObject>,
    aggregate_error_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // Error.prototype properties
    error_proto.define_property(
        PropertyKey::string("name"),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Error")),
            PropertyAttributes::builtin_method(),
        ),
    );
    error_proto.define_property(
        PropertyKey::string("message"),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("")),
            PropertyAttributes::builtin_method(),
        ),
    );

    // Mark as Error prototype for stack trace capture
    let _ = error_proto.set(PropertyKey::string("__is_error__"), Value::boolean(true));

    let (_to_string_name, to_string_native, _to_string_length) = error_to_string_decl();
    let to_string_fn = native_from_decl_with_function_proto(to_string_native, mm, fn_proto.clone());

    // Error.prototype.toString
    error_proto.define_property(
        PropertyKey::string("toString"),
        PropertyDescriptor::builtin_method(to_string_fn),
    );

    // Error.prototype.stack getter
    // This is a lazy getter that formats the __stack_frames__ property
    // The actual stack frames are captured in interpreter.rs during Error construction
    let (_stack_name, stack_native, _stack_length) = error_stack_getter_decl();
    let stack_getter = native_from_decl_with_function_proto(stack_native, mm, fn_proto.clone());
    error_proto.define_property(
        PropertyKey::string("stack"),
        PropertyDescriptor::Accessor {
            get: Some(stack_getter),
            set: None,
            attributes: PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        },
    );

    // Error type-specific names
    let error_names = [
        (type_error_proto, "TypeError"),
        (range_error_proto, "RangeError"),
        (reference_error_proto, "ReferenceError"),
        (syntax_error_proto, "SyntaxError"),
        (uri_error_proto, "URIError"),
        (eval_error_proto, "EvalError"),
        (aggregate_error_proto, "AggregateError"),
    ];
    for (proto, name) in &error_names {
        proto.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::data_with_attrs(
                Value::string(JsString::intern(name)),
                PropertyAttributes::builtin_method(),
            ),
        );
        proto.define_property(
            PropertyKey::string("message"),
            PropertyDescriptor::data_with_attrs(
                Value::string(JsString::intern("")),
                PropertyAttributes::builtin_method(),
            ),
        );
        // Mark as Error prototype for stack trace capture
        let _ = proto.set(PropertyKey::string("__is_error__"), Value::boolean(true));
    }
}

/// Create Error constructor function
pub fn create_error_constructor(
    error_name: &'static str,
) -> Box<
    dyn Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, VmError>
        + Send
        + Sync,
> {
    Box::new(move |this, args, _ncx_inner| {
        // Set properties on `this` (the new object created by Construct
        // which already has the correct ErrorType.prototype)
        if let Some(obj) = this.as_object() {
            // §20.5.1.1: Only set message if first arg is not undefined
            if let Some(msg) = args.first() {
                if !msg.is_undefined() {
                    // message: { writable: true, enumerable: false, configurable: true }
                    obj.define_property(
                        PropertyKey::string("message"),
                        PropertyDescriptor::data_with_attrs(
                            Value::string(JsString::intern(&crate::globals::to_string(msg))),
                            PropertyAttributes::builtin_method(),
                        ),
                    );
                }
            }
            // §20.5.1.1 step 5: InstallErrorCause(O, options)
            if let Some(options) = args.get(1) {
                if let Some(opts_obj) = options.as_object() {
                    if opts_obj.has(&PropertyKey::string("cause")) {
                        if let Some(cause) = opts_obj.get(&PropertyKey::string("cause")) {
                            obj.define_property(
                                PropertyKey::string("cause"),
                                PropertyDescriptor::data_with_attrs(
                                    cause,
                                    PropertyAttributes::builtin_method(),
                                ),
                            );
                        }
                    }
                }
            }
            // Note: `name` is inherited from prototype, NOT set on instances
        }
        // Return undefined so Construct uses new_obj_value with correct prototype
        Ok(Value::undefined())
    })
}

/// Create AggregateError constructor: `new AggregateError(errors, message, options)`
///
/// Per §20.5.7.1, the first argument is an iterable of errors stored in `.errors`.
pub fn create_aggregate_error_constructor() -> Box<
    dyn Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, VmError>
        + Send
        + Sync,
> {
    Box::new(|this, args, ncx| {
        if let Some(obj) = this.as_object() {
            // arg0: errors (iterable) — iterate via iterator protocol
            let errors_arg = args.first().cloned().unwrap_or(Value::undefined());
            let mm = obj.memory_manager().clone();
            let errors_array = if let Some(arr_obj) = errors_arg.as_object() {
                // Try to use Symbol.iterator for proper iteration
                let iter_sym = crate::intrinsics::well_known::iterator_symbol();
                let iter_fn = arr_obj.get(&PropertyKey::Symbol(iter_sym));
                if let Some(iter_method) = iter_fn {
                    if iter_method.is_callable() {
                        // Use iterator protocol
                        let iterator =
                            ncx.call_function(&iter_method, Value::object(arr_obj.clone()), &[])?;
                        let result_arr = GcRef::new(JsObject::array(0, mm.clone()));
                        let mut idx = 0u32;
                        if let Some(iter_obj) = iterator.as_object() {
                            let next_fn = iter_obj
                                .get(&PropertyKey::string("next"))
                                .unwrap_or(Value::undefined());
                            loop {
                                let step = ncx.call_function(
                                    &next_fn,
                                    Value::object(iter_obj.clone()),
                                    &[],
                                )?;
                                if let Some(step_obj) = step.as_object() {
                                    let done = step_obj
                                        .get(&PropertyKey::string("done"))
                                        .map(|v| v.to_boolean())
                                        .unwrap_or(false);
                                    if done {
                                        break;
                                    }
                                    let value = step_obj
                                        .get(&PropertyKey::string("value"))
                                        .unwrap_or(Value::undefined());
                                    let _ = result_arr.set(PropertyKey::Index(idx), value);
                                    idx += 1;
                                } else {
                                    break;
                                }
                            }
                        }
                        let _ = result_arr
                            .set(PropertyKey::string("length"), Value::number(idx as f64));
                        Value::array(result_arr)
                    } else {
                        // No iterator, fall back to array-like
                        let len = arr_obj
                            .get(&PropertyKey::string("length"))
                            .and_then(|v| v.as_number())
                            .unwrap_or(0.0) as u32;
                        let result_arr = GcRef::new(JsObject::array(len as usize, mm.clone()));
                        for i in 0..len {
                            if let Some(val) = arr_obj.get(&PropertyKey::Index(i)) {
                                let _ = result_arr.set(PropertyKey::Index(i), val);
                            }
                        }
                        Value::array(result_arr)
                    }
                } else {
                    // No Symbol.iterator, try array-like
                    let len = arr_obj
                        .get(&PropertyKey::string("length"))
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as u32;
                    let result_arr = GcRef::new(JsObject::array(len as usize, mm.clone()));
                    for i in 0..len {
                        if let Some(val) = arr_obj.get(&PropertyKey::Index(i)) {
                            let _ = result_arr.set(PropertyKey::Index(i), val);
                        }
                    }
                    Value::array(result_arr)
                }
            } else {
                // Non-object: spec says TypeError for non-iterable
                return Err(VmError::type_error(
                    "AggregateError: errors argument is not iterable",
                ));
            };

            // Store .errors as a data property { writable: true, enumerable: false, configurable: true }
            obj.define_property(
                PropertyKey::string("errors"),
                PropertyDescriptor::data_with_attrs(
                    errors_array,
                    PropertyAttributes::builtin_method(),
                ),
            );

            // arg1: message (optional)
            if let Some(msg) = args.get(1) {
                if !msg.is_undefined() {
                    let msg_str = ncx.to_string_value(msg)?;
                    obj.define_property(
                        PropertyKey::string("message"),
                        PropertyDescriptor::data_with_attrs(
                            Value::string(JsString::intern(&msg_str)),
                            PropertyAttributes::builtin_method(),
                        ),
                    );
                }
            }

            // arg2: options (optional) — extract .cause if present
            if let Some(options) = args.get(2) {
                if let Some(opts_obj) = options.as_object() {
                    if opts_obj.has(&PropertyKey::string("cause")) {
                        if let Some(cause) = opts_obj.get(&PropertyKey::string("cause")) {
                            obj.define_property(
                                PropertyKey::string("cause"),
                                PropertyDescriptor::data_with_attrs(
                                    cause,
                                    PropertyAttributes::builtin_method(),
                                ),
                            );
                        }
                    }
                }
            }

            // Note: `name` is inherited from AggregateError.prototype, NOT set on instances
        }
        Ok(Value::undefined())
    })
}
