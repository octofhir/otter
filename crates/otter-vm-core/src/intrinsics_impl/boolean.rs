//! Boolean constructor and prototype implementation
//!
//! Complete ES2026 Boolean implementation:
//! - Boolean(value) - converts to primitive boolean
//! - new Boolean(value) - creates Boolean object
//! - Boolean.prototype.valueOf() - returns primitive value
//! - Boolean.prototype.toString() - returns "true" or "false"

use crate::builtin_builder::{BuiltInBuilder, IntrinsicContext, IntrinsicObject};
use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use otter_macros::dive;
use std::sync::Arc;

pub struct BooleanIntrinsic;

impl IntrinsicObject for BooleanIntrinsic {
    fn init(ctx: &IntrinsicContext) {
        let mm = ctx.mm();
        init_boolean_prototype(ctx.intrinsics().boolean_prototype, ctx.fn_proto(), &mm);

        if let Some(global) = ctx.global_opt() {
            BuiltInBuilder::new(
                mm.clone(),
                ctx.fn_proto(),
                ctx.alloc_constructor(),
                ctx.intrinsics().boolean_prototype,
                "Boolean",
            )
            .inherits(ctx.obj_proto())
            .constructor_fn(create_boolean_constructor(), 1)
            .build_and_install(&global);
        }
    }
}

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

/// thisBooleanValue(value) — ES2026 §21.3.3
/// Returns the boolean value if value is a boolean primitive or a Boolean object.
/// Throws TypeError otherwise.
fn this_boolean_value(this_val: &Value) -> Result<bool, VmError> {
    // 1. If Type(value) is Boolean, return value.
    if let Some(b) = this_val.as_boolean() {
        return Ok(b);
    }
    // 2. If Type(value) is Object and value has a [[BooleanData]] internal slot...
    if let Some(obj) = this_val.as_object() {
        if let Some(val) = obj.get(&PropertyKey::string("__primitiveValue__")) {
            if let Some(b) = val.as_boolean() {
                return Ok(b);
            }
        }
    }
    // 3. Throw a TypeError exception.
    Err(VmError::type_error(
        "Boolean.prototype.valueOf requires that 'this' be a Boolean",
    ))
}

#[dive(name = "valueOf", length = 0)]
fn boolean_value_of(
    this_val: &Value,
    _args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    Ok(Value::boolean(this_boolean_value(this_val)?))
}

#[dive(name = "toString", length = 0)]
fn boolean_to_string(
    this_val: &Value,
    _args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let b = this_boolean_value(this_val)?;
    Ok(Value::string(JsString::intern(if b {
        "true"
    } else {
        "false"
    })))
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
    _fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // Boolean.prototype has [[BooleanData]] = false (ES2026 §21.3.3)
    let _ = boolean_proto.set(
        PropertyKey::string("__primitiveValue__"),
        Value::boolean(false),
    );

    let methods: &[(&str, crate::value::NativeFn, u32)] =
        &[boolean_value_of_decl(), boolean_to_string_decl()];

    for (name, native_fn, length) in methods {
        let fn_val = Value::native_function_from_decl(name, native_fn.clone(), *length, mm.clone());
        boolean_proto.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::builtin_method(fn_val),
        );
    }
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
pub fn create_boolean_constructor() -> Box<
    dyn Fn(
            &Value,
            &[Value],
            &mut crate::context::NativeContext<'_>,
        ) -> Result<Value, crate::error::VmError>
        + Send
        + Sync,
> {
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
            let _ = obj.set(PropertyKey::string("__primitiveValue__"), bool_val);
            Ok(*this_val)
        } else {
            // Call form fallback
            Ok(bool_val)
        }
    })
}
