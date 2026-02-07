//! Boolean constructor and prototype implementation
//!
//! Complete ES2026 Boolean implementation:
//! - Boolean(value) - converts to primitive boolean
//! - new Boolean(value) - creates Boolean object
//! - Boolean.prototype.valueOf() - returns primitive value
//! - Boolean.prototype.toString() - returns "true" or "false"
//!
//! All methods use inline implementations for optimal performance.

use crate::gc::GcRef;
use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use crate::memory::MemoryManager;
use std::sync::Arc;

/// Convert a value to boolean (ToBoolean abstract operation ES2026 §7.1.2)
fn to_boolean(val: &Value) -> bool {
    if val.is_undefined() || val.is_null() {
        false
    } else if let Some(b) = val.as_boolean() {
        b
    } else if let Some(n) = val.as_number() {
        // false for +0, -0, NaN; true otherwise
        !n.is_nan() && n != 0.0
    } else if let Some(n) = val.as_int32() {
        n != 0
    } else if let Some(s) = val.as_string() {
        !s.as_str().is_empty()
    } else {
        // Objects are always truthy
        true
    }
}

/// Initialize Boolean.prototype with valueOf and toString methods
///
/// # ES2026 Methods
/// - **valueOf()** - Returns the primitive boolean value
/// - **toString()** - Returns "true" or "false"
///
/// # Property Attributes
/// All methods use `{ writable: true, enumerable: false, configurable: true }`
pub fn init_boolean_prototype(
    boolean_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // ====================================================================
    // Boolean.prototype.valueOf()
    // ====================================================================
    boolean_proto.define_property(
        PropertyKey::string("valueOf"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                // Return primitive boolean value
                if let Some(b) = this_val.as_boolean() {
                    Ok(Value::boolean(b))
                } else if let Some(obj) = this_val.as_object() {
                    // Boolean object - extract internal [[BooleanData]]
                    if let Some(val) = obj.get(&PropertyKey::string("__value__")) {
                        if let Some(b) = val.as_boolean() {
                            return Ok(Value::boolean(b));
                        }
                    }
                    // Fallback: coerce to boolean
                    Ok(Value::boolean(to_boolean(this_val)))
                } else {
                    // Primitive - coerce to boolean
                    Ok(Value::boolean(to_boolean(this_val)))
                }
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // ====================================================================
    // Boolean.prototype.toString()
    // ====================================================================
    boolean_proto.define_property(
        PropertyKey::string("toString"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                let b = if let Some(b) = this_val.as_boolean() {
                    b
                } else if let Some(obj) = this_val.as_object() {
                    // Boolean object - extract internal [[BooleanData]]
                    if let Some(val) = obj.get(&PropertyKey::string("__value__")) {
                        if let Some(b) = val.as_boolean() {
                            b
                        } else {
                            to_boolean(this_val)
                        }
                    } else {
                        to_boolean(this_val)
                    }
                } else {
                    to_boolean(this_val)
                };
                Ok(Value::string(JsString::intern(if b { "true" } else { "false" })))
            },
            mm.clone(),
            fn_proto,
        )),
    );
}

/// Create Boolean constructor function
///
/// The Boolean constructor supports both call and construct forms:
/// - **Boolean(value)** - Returns primitive boolean (ToBoolean conversion)
/// - **new Boolean(value)** - Returns Boolean object wrapper
///
/// # ES2026 Behavior
/// - Call form: Returns primitive boolean (§21.3.1.1)
/// - Construct form: Returns new Boolean object with [[BooleanData]] internal slot (§21.3.1.2)
///
/// # Implementation
/// The constructor checks the `this` value to determine call vs construct form:
/// - If `this` is undefined (call form), return primitive boolean
/// - If `this` is object (construct form), set internal [[BooleanData]] and return object
pub fn create_boolean_constructor() -> Box<dyn Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, crate::error::VmError> + Send + Sync> {
    Box::new(|this_val, args, _ncx| {
        let value = args.first().cloned().unwrap_or(Value::undefined());
        let bool_val = Value::boolean(to_boolean(&value));

        // Check if called as constructor (new Boolean(...))
        if this_val.is_undefined() {
            // Call form: Boolean(value) → primitive boolean
            Ok(bool_val)
        } else if let Some(obj) = this_val.as_object() {
            // Construct form: new Boolean(value) → Boolean object
            // Store primitive value in internal [[BooleanData]] slot
            let _ = obj.set(PropertyKey::string("__value__"), bool_val);
            Ok(this_val.clone())
        } else {
            // Call form fallback
            Ok(bool_val)
        }
    })
}
