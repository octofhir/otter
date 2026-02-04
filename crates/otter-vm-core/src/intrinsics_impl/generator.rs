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
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;

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
    symbol_iterator_id: u64,
    symbol_to_string_tag_id: u64,
) {
    // Generator.prototype.next(value) — §27.5.1.2
    proto.define_property(
        PropertyKey::string("next"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, ncx| {
                // Extract the generator from `this`
                let generator = this_val
                    .as_generator()
                    .ok_or_else(|| VmError::type_error("Generator.prototype.next called on non-generator"))?;

                // Ensure it's not an async generator
                if generator.is_async() {
                    return Err(VmError::type_error(
                        "Generator.prototype.next called on async generator",
                    ));
                }

                // This is a placeholder that returns an iterator result object.
                // The actual generator execution is handled by the interpreter's
                // special case for __Generator_next. This method exists on the
                // prototype to satisfy ES2026 spec requirements.

                // For now, return a placeholder iterator result
                // In production, this would delegate to the interpreter
                let result = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                result.set(PropertyKey::string("value"), Value::undefined());
                result.set(PropertyKey::string("done"), Value::boolean(false));
                Ok(Value::object(result))
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

                // Placeholder - actual execution handled by interpreter
                let result = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                let return_value = args.first().cloned().unwrap_or_else(Value::undefined);
                result.set(PropertyKey::string("value"), return_value);
                result.set(PropertyKey::string("done"), Value::boolean(true));
                Ok(Value::object(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Generator.prototype.throw(exception) — §27.5.1.4
    proto.define_property(
        PropertyKey::string("throw"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let generator = this_val
                    .as_generator()
                    .ok_or_else(|| VmError::type_error("Generator.prototype.throw called on non-generator"))?;

                if generator.is_async() {
                    return Err(VmError::type_error(
                        "Generator.prototype.throw called on async generator",
                    ));
                }

                // Placeholder - actual execution handled by interpreter
                let exception = args.first().cloned().unwrap_or_else(Value::undefined);
                Err(VmError::exception(exception))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Generator.prototype[Symbol.iterator] — returns `this`
    proto.define_property(
        PropertyKey::Symbol(symbol_iterator_id),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                // Generators are iterable - Symbol.iterator returns the generator itself
                Ok(this_val.clone())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Generator.prototype[Symbol.toStringTag] = "Generator"
    proto.define_property(
        PropertyKey::Symbol(symbol_to_string_tag_id),
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
///
/// Wires the following to the prototype:
/// - `next(value)` - Resumes async generator execution
/// - `return(value)` - Forces async generator to return
/// - `throw(exception)` - Throws an exception into the async generator
/// - `[Symbol.asyncIterator]` - Returns `this` (makes async generator async-iterable)
/// - `[Symbol.toStringTag]` - "AsyncGenerator" (non-enumerable, configurable)
pub fn init_async_generator_prototype(
    proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    symbol_async_iterator_id: u64,
    symbol_to_string_tag_id: u64,
) {
    // AsyncGenerator.prototype.next(value) — §27.6.1.2
    proto.define_property(
        PropertyKey::string("next"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, ncx| {
                let generator = this_val
                    .as_generator()
                    .ok_or_else(|| {
                        VmError::type_error("AsyncGenerator.prototype.next called on non-generator")
                    })?;

                // Ensure it's an async generator
                if !generator.is_async() {
                    return Err(VmError::type_error(
                        "AsyncGenerator.prototype.next called on sync generator",
                    ));
                }

                // Placeholder - actual execution handled by interpreter
                // Async generators return promises
                let result = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                result.set(PropertyKey::string("value"), Value::undefined());
                result.set(PropertyKey::string("done"), Value::boolean(false));
                Ok(Value::object(result))
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

                // Placeholder - actual execution handled by interpreter
                let result = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                let return_value = args.first().cloned().unwrap_or_else(Value::undefined);
                result.set(PropertyKey::string("value"), return_value);
                result.set(PropertyKey::string("done"), Value::boolean(true));
                Ok(Value::object(result))
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

                // Placeholder - actual execution handled by interpreter
                let exception = args.first().cloned().unwrap_or_else(Value::undefined);
                Err(VmError::exception(exception))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // AsyncGenerator.prototype[Symbol.asyncIterator] — returns `this`
    proto.define_property(
        PropertyKey::Symbol(symbol_async_iterator_id),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                // Async generators are async-iterable - Symbol.asyncIterator returns the generator itself
                Ok(this_val.clone())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // AsyncGenerator.prototype[Symbol.toStringTag] = "AsyncGenerator"
    proto.define_property(
        PropertyKey::Symbol(symbol_to_string_tag_id),
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
