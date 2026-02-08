//! Generator.prototype and AsyncGenerator.prototype methods (ES2026)
//!
//! ## Generator.prototype methods:
//! - `Generator.prototype.next(value)` — §27.5.1.2
//! - `Generator.prototype.return(value)` — §27.5.1.3
//! - `Generator.prototype.throw(exception)` — §27.5.1.4
//! - `Generator.prototype[Symbol.iterator]()` — returns `this`
//! - `Generator.prototype[Symbol.toStringTag]` — "Generator"
//!
//! ## AsyncGenerator.prototype methods:
//! - `AsyncGenerator.prototype.next(value)` — §27.6.1.2
//! - `AsyncGenerator.prototype.return(value)` — §27.6.1.3
//! - `AsyncGenerator.prototype.throw(exception)` — §27.6.1.4
//! - `AsyncGenerator.prototype[Symbol.asyncIterator]()` — returns `this`
//! - `AsyncGenerator.prototype[Symbol.toStringTag]` — "AsyncGenerator"

use std::sync::Arc;

use crate::error::VmError;
use crate::gc::GcRef;
use crate::interpreter::GeneratorResult;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::promise::JsPromise;
use crate::string::JsString;
use crate::value::Value;

/// Helper: convert a GeneratorResult into an iterator result object for sync generators.
fn sync_generator_result_to_value(
    gen_result: GeneratorResult,
    mm: &Arc<MemoryManager>,
) -> Result<Value, VmError> {
    match gen_result {
        GeneratorResult::Yielded(v) => {
            let result = GcRef::new(JsObject::new(Value::null(), mm.clone()));
            let _ = result.set(PropertyKey::string("value"), v);
            let _ = result.set(PropertyKey::string("done"), Value::boolean(false));
            Ok(Value::object(result))
        }
        GeneratorResult::Returned(v) => {
            let result = GcRef::new(JsObject::new(Value::null(), mm.clone()));
            let _ = result.set(PropertyKey::string("value"), v);
            let _ = result.set(PropertyKey::string("done"), Value::boolean(true));
            Ok(Value::object(result))
        }
        GeneratorResult::Error(e) => Err(e),
        GeneratorResult::Suspended { .. } => {
            Err(VmError::internal("Sync generator cannot suspend"))
        }
    }
}

/// Helper: convert a GeneratorResult into a promise-wrapped iterator result for async generators.
fn async_generator_result_to_promise(
    gen_result: GeneratorResult,
    ncx: &mut crate::context::NativeContext<'_>,
) -> Value {
    let mm = ncx.memory_manager().clone();
    let js_queue = ncx.js_job_queue();
    let promise = JsPromise::new();

    match gen_result {
        GeneratorResult::Yielded(v) => {
            let iter_result = GcRef::new(JsObject::new(Value::null(), mm));
            let _ = iter_result.set(PropertyKey::string("value"), v);
            let _ = iter_result.set(PropertyKey::string("done"), Value::boolean(false));
            let js_queue = js_queue.clone();
            JsPromise::resolve_with_js_jobs(
                promise,
                Value::object(iter_result),
                move |job, args| {
                    if let Some(queue) = &js_queue {
                        queue.enqueue(job, args);
                    }
                },
            );
        }
        GeneratorResult::Returned(v) => {
            let iter_result = GcRef::new(JsObject::new(Value::null(), mm));
            let _ = iter_result.set(PropertyKey::string("value"), v);
            let _ = iter_result.set(PropertyKey::string("done"), Value::boolean(true));
            let js_queue = js_queue.clone();
            JsPromise::resolve_with_js_jobs(
                promise,
                Value::object(iter_result),
                move |job, args| {
                    if let Some(queue) = &js_queue {
                        queue.enqueue(job, args);
                    }
                },
            );
        }
        GeneratorResult::Error(e) => {
            let error_msg = e.to_string();
            let js_queue = js_queue.clone();
            JsPromise::reject_with_js_jobs(
                promise,
                Value::string(JsString::intern(&error_msg)),
                move |job, args| {
                    if let Some(queue) = &js_queue {
                        queue.enqueue(job, args);
                    }
                },
            );
        }
        GeneratorResult::Suspended {
            promise: awaited_promise,
            ..
        } => {
            let result_promise = promise.clone();
            let js_queue = js_queue.clone();
            awaited_promise.then(move |resolved_value| {
                let iter_result =
                    GcRef::new(JsObject::new(Value::null(), mm.clone()));
                let _ = iter_result.set(PropertyKey::string("value"), resolved_value);
                let _ = iter_result.set(PropertyKey::string("done"), Value::boolean(false));
                let js_queue = js_queue.clone();
                JsPromise::resolve_with_js_jobs(
                    result_promise,
                    Value::object(iter_result),
                    move |job, args| {
                        if let Some(queue) = &js_queue {
                            queue.enqueue(job, args);
                        }
                    },
                );
            });
        }
    }

    Value::promise(promise)
}

// ============================================================================
// Generator.prototype initialization
// ============================================================================

/// Initialize `%GeneratorPrototype%` with its methods and properties.
///
/// Wires the following to the prototype:
/// - `next(value)` - Resumes generator execution
/// - `return(value)` - Forces generator to return
/// - `throw(exception)` - Throws an exception into the generator
/// - `[Symbol.iterator]` - Returns `this` (makes generator iterable)
/// - `[Symbol.toStringTag]` - "Generator" (non-enumerable, configurable)
pub fn init_generator_prototype(
    proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    symbol_iterator: crate::gc::GcRef<crate::value::Symbol>,
    symbol_to_string_tag: crate::gc::GcRef<crate::value::Symbol>,
) {
    // Generator.prototype.next(value) — §27.5.1.2
    proto.define_property(
        PropertyKey::string("next"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let generator = this_val
                    .as_generator()
                    .ok_or_else(|| VmError::type_error("Generator.prototype.next called on non-generator"))?;

                if generator.is_async() {
                    return Err(VmError::type_error(
                        "Generator.prototype.next called on async generator",
                    ));
                }

                let sent_value = args.first().cloned();
                let gen_result = ncx.execute_generator(generator, sent_value);
                let mm = ncx.memory_manager();
                sync_generator_result_to_value(gen_result, mm)
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Generator.prototype.return(value) — §27.5.1.3
    proto.define_property(
        PropertyKey::string("return"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let generator = this_val
                    .as_generator()
                    .ok_or_else(|| VmError::type_error("Generator.prototype.return called on non-generator"))?;

                if generator.is_async() {
                    return Err(VmError::type_error(
                        "Generator.prototype.return called on async generator",
                    ));
                }

                let return_value = args.first().cloned().unwrap_or_else(Value::undefined);

                // If completed, just return { value, done: true }
                if generator.is_completed() {
                    let gen_result = GeneratorResult::Returned(return_value);
                    return sync_generator_result_to_value(gen_result, ncx.memory_manager());
                }

                // If no try handlers, complete immediately
                if !generator.has_try_handlers() {
                    generator.complete();
                    let gen_result = GeneratorResult::Returned(return_value);
                    return sync_generator_result_to_value(gen_result, ncx.memory_manager());
                }

                // Has try handlers - need to run finally blocks
                generator.set_pending_return(return_value);
                let gen_result = ncx.execute_generator(generator, None);
                sync_generator_result_to_value(gen_result, ncx.memory_manager())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Generator.prototype.throw(exception) — §27.5.1.4
    proto.define_property(
        PropertyKey::string("throw"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let generator = this_val
                    .as_generator()
                    .ok_or_else(|| VmError::type_error("Generator.prototype.throw called on non-generator"))?;

                if generator.is_async() {
                    return Err(VmError::type_error(
                        "Generator.prototype.throw called on async generator",
                    ));
                }

                let error_value = args.first().cloned().unwrap_or_else(Value::undefined);

                // If completed, just throw
                if generator.is_completed() {
                    return Err(VmError::exception(error_value));
                }

                // Set pending throw and execute
                generator.set_pending_throw(error_value);
                let gen_result = ncx.execute_generator(generator, None);
                sync_generator_result_to_value(gen_result, ncx.memory_manager())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Generator.prototype[Symbol.iterator] — returns `this`
    proto.define_property(
        PropertyKey::Symbol(symbol_iterator),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                Ok(this_val.clone())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Generator.prototype[Symbol.toStringTag] = "Generator"
    proto.define_property(
        PropertyKey::Symbol(symbol_to_string_tag),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Generator")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
    );
}

// ============================================================================
// AsyncGenerator.prototype initialization
// ============================================================================

/// Initialize `%AsyncGeneratorPrototype%` with its methods and properties.
pub fn init_async_generator_prototype(
    proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    symbol_async_iterator: crate::gc::GcRef<crate::value::Symbol>,
    symbol_to_string_tag: crate::gc::GcRef<crate::value::Symbol>,
) {
    // AsyncGenerator.prototype.next(value) — §27.6.1.2
    proto.define_property(
        PropertyKey::string("next"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let generator = this_val
                    .as_generator()
                    .ok_or_else(|| {
                        VmError::type_error("AsyncGenerator.prototype.next called on non-generator")
                    })?;

                if !generator.is_async() {
                    return Err(VmError::type_error(
                        "AsyncGenerator.prototype.next called on sync generator",
                    ));
                }

                let sent_value = args.first().cloned();
                let gen_result = ncx.execute_generator(generator, sent_value);
                Ok(async_generator_result_to_promise(gen_result, ncx))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // AsyncGenerator.prototype.return(value) — §27.6.1.3
    proto.define_property(
        PropertyKey::string("return"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let generator = this_val.as_generator().ok_or_else(|| {
                    VmError::type_error("AsyncGenerator.prototype.return called on non-generator")
                })?;

                if !generator.is_async() {
                    return Err(VmError::type_error(
                        "AsyncGenerator.prototype.return called on sync generator",
                    ));
                }

                let return_value = args.first().cloned().unwrap_or_else(Value::undefined);

                let gen_result = if generator.is_completed() {
                    GeneratorResult::Returned(return_value)
                } else if !generator.has_try_handlers() {
                    generator.complete();
                    GeneratorResult::Returned(return_value)
                } else {
                    generator.set_pending_return(return_value);
                    ncx.execute_generator(generator, None)
                };

                Ok(async_generator_result_to_promise(gen_result, ncx))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // AsyncGenerator.prototype.throw(exception) — §27.6.1.4
    proto.define_property(
        PropertyKey::string("throw"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let generator = this_val.as_generator().ok_or_else(|| {
                    VmError::type_error("AsyncGenerator.prototype.throw called on non-generator")
                })?;

                if !generator.is_async() {
                    return Err(VmError::type_error(
                        "AsyncGenerator.prototype.throw called on sync generator",
                    ));
                }

                let error_value = args.first().cloned().unwrap_or_else(Value::undefined);

                let gen_result = if generator.is_completed() {
                    GeneratorResult::Error(VmError::exception(error_value))
                } else {
                    generator.set_pending_throw(error_value);
                    ncx.execute_generator(generator, None)
                };

                Ok(async_generator_result_to_promise(gen_result, ncx))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // AsyncGenerator.prototype[Symbol.asyncIterator] — returns `this`
    proto.define_property(
        PropertyKey::Symbol(symbol_async_iterator),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                Ok(this_val.clone())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // AsyncGenerator.prototype[Symbol.toStringTag] = "AsyncGenerator"
    proto.define_property(
        PropertyKey::Symbol(symbol_to_string_tag),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("AsyncGenerator")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
    );
}
