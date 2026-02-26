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
    _error_name: &'static str,
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
/// Per §20.5.7.1 AggregateError ( errors, message [ , options ] ):
/// 1. If NewTarget is undefined, let newTarget be the active function object
/// 2. Let O = OrdinaryCreateFromConstructor(newTarget, "%AggregateError.prototype%")
/// 3. If message is not undefined, let msg = ToString(message), set O.[[message]]
/// 4. InstallErrorCause(O, options)
/// 5. Let errorsList = IterableToList(errors)   ← AFTER message
/// 6. Perform ! DefinePropertyOrThrow(O, "errors", { [[Value]]: errorsList, ... })
/// 7. Return O
pub fn create_aggregate_error_constructor() -> Box<
    dyn Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, VmError>
        + Send
        + Sync,
> {
    Box::new(|this, args, ncx| {
        if let Some(obj) = this.as_object() {
            let errors_arg = args.first().cloned().unwrap_or(Value::undefined());
            let mm = obj.memory_manager().clone();

            // Step 3: message (BEFORE errors per spec)
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

            // Step 4: InstallErrorCause(O, options)
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

            // Step 5: IterableToList(errors)
            let errors_array = iterable_to_list(ncx, &errors_arg, &mm)?;

            // Step 6: Define .errors property
            obj.define_property(
                PropertyKey::string("errors"),
                PropertyDescriptor::data_with_attrs(
                    errors_array,
                    PropertyAttributes::builtin_method(),
                ),
            );
        }
        Ok(Value::undefined())
    })
}

/// Implements IterableToList (§7.4.7) — iterate an iterable and collect values into an array.
///
/// Uses `get_value_full` to properly invoke accessor getters on iterator protocol objects.
fn iterable_to_list(
    ncx: &mut NativeContext<'_>,
    iterable: &Value,
    mm: &Arc<MemoryManager>,
) -> Result<Value, VmError> {
    use crate::object::get_value_full;

    // GetMethod(obj, @@iterator) — §7.3.10
    let iter_sym = crate::intrinsics::well_known::iterator_symbol();
    let iter_method = if let Some(obj) = iterable.as_object() {
        // Use get_value_full to invoke getter if @@iterator is an accessor
        let val = get_value_full(&obj, &PropertyKey::Symbol(iter_sym), ncx)?;
        if val.is_undefined() || val.is_null() {
            None
        } else {
            Some(val)
        }
    } else {
        None
    };

    let iter_method = match iter_method {
        Some(m) if m.is_callable() => m,
        Some(_) => {
            return Err(VmError::type_error(
                "Result of the Symbol.iterator method is not callable",
            ));
        }
        None => {
            return Err(VmError::type_error("object is not iterable"));
        }
    };

    // Call(method, obj) — get iterator
    let iterator = ncx.call_function(&iter_method, iterable.clone(), &[])?;
    let iter_obj = iterator.as_object().ok_or_else(|| {
        VmError::type_error("Result of the Symbol.iterator method is not an object")
    })?;

    // GetV(iterator, "next")
    let next_fn = get_value_full(&iter_obj, &PropertyKey::string("next"), ncx)?;
    if !next_fn.is_callable() {
        return Err(VmError::type_error("iterator.next is not a function"));
    }

    let result_arr = GcRef::new(JsObject::array(0, mm.clone()));
    // Root the result array across call_function GC points
    ncx.ctx.push_root_slot(Value::array(result_arr));
    let mut idx = 0u32;

    let loop_result: Result<(), VmError> = (|| {
        loop {
            // IteratorStep: call next()
            let step = ncx.call_function(&next_fn, Value::object(iter_obj.clone()), &[])?;
            let step_obj = step
                .as_object()
                .ok_or_else(|| VmError::type_error("Iterator result is not an object"))?;
            // IteratorComplete: Get(result, "done")
            let done = get_value_full(&step_obj, &PropertyKey::string("done"), ncx)?;
            if done.to_boolean() {
                break;
            }
            // IteratorValue: Get(result, "value")
            let value = get_value_full(&step_obj, &PropertyKey::string("value"), ncx)?;
            let _ = result_arr.set(PropertyKey::Index(idx), value);
            idx += 1;
        }
        Ok(())
    })();

    ncx.ctx.pop_root_slots(1);
    loop_result?;

    let _ = result_arr.set(PropertyKey::string("length"), Value::number(idx as f64));
    Ok(Value::array(result_arr))
}
