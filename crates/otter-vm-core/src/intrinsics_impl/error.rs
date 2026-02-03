//! Error.prototype methods and Error constructor implementation
//!
//! All Error object methods for ES2026 standard, including stack trace support.

use crate::error::VmError;
use crate::gc::GcRef;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use crate::memory::MemoryManager;
use std::sync::Arc;

/// Initialize Error.prototype and all error type prototypes
pub fn init_error_prototypes(
    error_proto: GcRef<JsObject>,
    type_error_proto: GcRef<JsObject>,
    range_error_proto: GcRef<JsObject>,
    reference_error_proto: GcRef<JsObject>,
    syntax_error_proto: GcRef<JsObject>,
    uri_error_proto: GcRef<JsObject>,
    eval_error_proto: GcRef<JsObject>,
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
    error_proto.set(
        PropertyKey::string("__is_error__"),
        Value::boolean(true),
    );

    // Error.prototype.toString
    error_proto.define_property(
        PropertyKey::string("toString"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
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
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Error.prototype.stack getter
    // This is a lazy getter that formats the __stack_frames__ property
    // The actual stack frames are captured in interpreter.rs during Error construction
    error_proto.define_property(
        PropertyKey::string("stack"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this_val, _args, _ncx| {
                    if let Some(obj) = this_val.as_object() {
                        // Get error name and message
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

                        // Get stack frames (if captured)
                        let frames = obj.get(&PropertyKey::string("__stack_frames__"));

                        // Format stack trace
                        let mut stack = if message.is_empty() {
                            name.clone()
                        } else {
                            format!("{}: {}", name, message)
                        };

                        if let Some(frames_val) = frames {
                            if let Some(frames_arr) = frames_val.as_object() {
                                // Get array length
                                if let Some(len_val) = frames_arr.get(&PropertyKey::string("length")) {
                                    if let Some(len) = len_val.as_number() {
                                        for i in 0..(len as u32) {
                                            if let Some(frame_val) = frames_arr.get(&PropertyKey::Index(i)) {
                                                if let Some(frame) = frame_val.as_object() {
                                                    // Extract frame details
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

                                                    // Format: "\n    at func (file:line:col)"
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
                },
                mm.clone(),
                fn_proto.clone(),
            )),
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
        proto.set(
            PropertyKey::string("__is_error__"),
            Value::boolean(true),
        );
    }
}

/// Create Error constructor function
pub fn create_error_constructor(
    error_name: &'static str,
) -> Box<dyn Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, VmError> + Send + Sync> {
    Box::new(move |this, args, _ncx_inner| {
        // Set properties on `this` (the new object created by Construct
        // which already has the correct ErrorType.prototype)
        if let Some(obj) = this.as_object() {
            if let Some(msg) = args.first() {
                if !msg.is_undefined() {
                    obj.set(
                        PropertyKey::string("message"),
                        Value::string(JsString::intern(&crate::globals::to_string(msg))),
                    );
                }
            }
            obj.set(
                PropertyKey::string("name"),
                Value::string(JsString::intern(error_name)),
            );
        }
        // Return undefined so Construct uses new_obj_value with correct prototype
        Ok(Value::undefined())
    })
}
