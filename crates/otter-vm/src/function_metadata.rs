//! Function object metadata helpers.
//!
//! This module centralizes the observable `name` / `length` surface shared by
//! ordinary functions, native functions, and bound functions. It keeps
//! `Function.prototype.bind` metadata composition out of the interpreter's
//! opcode arms while still routing descriptor reads, redefinition, and deletion
//! through the existing descriptor core.
//!
//! # Contents
//! - [`FunctionMetadataContext`] — read-only metadata lookup inputs.
//! - [`callable_intrinsic_property`] — `f.name` / `f.length` value reads.
//! - Bound-function own-property helpers for descriptor APIs.
//!
//! # Invariants
//! - Bound-function `name` / `length` are own data properties:
//!   non-writable, non-enumerable, configurable.
//! - Bound-function metadata composes by reading the target callable's own
//!   `name` / `length` value first, then applying the `bound ` prefix and
//!   argument-count subtraction.
//! - Descriptor updates use `object::validate_descriptor_update`; this module
//!   does not bypass ordinary descriptor compatibility rules.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-function.prototype.bind>
//! - <https://tc39.es/ecma262/#sec-setfunctionname>
//! - <https://tc39.es/ecma262/#sec-setfunctionlength>

use std::collections::HashMap;

use otter_bytecode::BytecodeModule;

use crate::number::NumberValue;
use crate::object::{self, DescriptorKind, JsObject, PropertyDescriptor};
use crate::string::{JsString, StringHeap};
use crate::{BoundFunction, BoundFunctionMetadataProperty, Value, VmError};

/// Read-only inputs needed to resolve callable metadata.
pub(crate) struct FunctionMetadataContext<'a> {
    module: &'a BytecodeModule,
    gc_heap: &'a otter_gc::GcHeap,
    string_heap: &'a StringHeap,
    function_user_props: &'a HashMap<u32, JsObject>,
}

/// Builtin metadata captured when `Function.prototype.bind` creates a wrapper.
pub(crate) struct BoundFunctionCreateMetadata {
    /// Computed `name` value.
    pub(crate) name: String,
    /// Computed `length` value.
    pub(crate) length: NumberValue,
}

impl<'a> FunctionMetadataContext<'a> {
    /// Build a metadata lookup context.
    #[must_use]
    pub(crate) fn new(
        module: &'a BytecodeModule,
        gc_heap: &'a otter_gc::GcHeap,
        string_heap: &'a StringHeap,
        function_user_props: &'a HashMap<u32, JsObject>,
    ) -> Self {
        Self {
            module,
            gc_heap,
            string_heap,
            function_user_props,
        }
    }
}

/// Read `name` or `length` from any callable metadata shape.
pub(crate) fn callable_intrinsic_property(
    ctx: &FunctionMetadataContext<'_>,
    callee: &Value,
    key: &str,
) -> Result<Value, VmError> {
    match callable_metadata_value(ctx, callee, key)? {
        Some(value) => Ok(value),
        None => Ok(Value::Undefined),
    }
}

/// Read `name` or `length` from an ordinary function record.
pub(crate) fn ordinary_function_intrinsic_property(
    ctx: &FunctionMetadataContext<'_>,
    function_id: u32,
    key: &str,
) -> Result<Value, VmError> {
    callable_intrinsic_property(ctx, &Value::Function { function_id }, key)
}

/// Return the own descriptor for a bound function metadata property.
pub(crate) fn bound_own_property_descriptor(
    bound: &BoundFunction,
    gc_heap: &otter_gc::GcHeap,
    string_heap: &StringHeap,
    key: &str,
) -> Result<Option<PropertyDescriptor>, VmError> {
    gc_heap.read_payload(bound.inner, |body| {
        let property = match key {
            "name" => &body.name_property,
            "length" => &body.length_property,
            _ => return Ok(None),
        };
        match property {
            BoundFunctionMetadataProperty::Builtin => {
                bound_builtin_descriptor(body, string_heap, key).map(Some)
            }
            BoundFunctionMetadataProperty::Deleted => Ok(None),
            BoundFunctionMetadataProperty::Overridden(desc) => Ok(Some(desc.clone())),
        }
    })
}

/// Return own string property keys in built-in creation order.
#[must_use]
pub(crate) fn bound_own_property_keys(
    bound: &BoundFunction,
    gc_heap: &otter_gc::GcHeap,
) -> Vec<&'static str> {
    gc_heap.read_payload(bound.inner, |body| {
        let mut keys = Vec::new();
        if !matches!(body.length_property, BoundFunctionMetadataProperty::Deleted) {
            keys.push("length");
        }
        if !matches!(body.name_property, BoundFunctionMetadataProperty::Deleted) {
            keys.push("name");
        }
        keys
    })
}

/// Test whether a bound function still owns a metadata property.
#[must_use]
pub(crate) fn bound_has_own_property(
    bound: &BoundFunction,
    gc_heap: &otter_gc::GcHeap,
    key: &str,
) -> bool {
    gc_heap.read_payload(bound.inner, |body| match key {
        "name" => !matches!(body.name_property, BoundFunctionMetadataProperty::Deleted),
        "length" => !matches!(body.length_property, BoundFunctionMetadataProperty::Deleted),
        _ => false,
    })
}

/// Define or redefine a bound function metadata property.
pub(crate) fn bound_define_own_property(
    bound: &BoundFunction,
    heap: &mut otter_gc::GcHeap,
    string_heap: &StringHeap,
    key: &str,
    descriptor: PropertyDescriptor,
) -> bool {
    let existing = match bound_own_property_descriptor(bound, heap, string_heap, key) {
        Ok(existing) => existing,
        Err(_) => return false,
    };
    let descriptor = match existing {
        Some(existing) => match object::validate_descriptor_update(&existing, &descriptor) {
            Some(merged) => merged,
            None => return false,
        },
        None if key == "name" || key == "length" => descriptor,
        None => return false,
    };
    let barrier_descriptor = descriptor.clone();
    let success = heap.with_payload(bound.inner, |body| {
        let slot = match key {
            "name" => &mut body.name_property,
            "length" => &mut body.length_property,
            _ => return false,
        };
        *slot = BoundFunctionMetadataProperty::Overridden(descriptor);
        true
    });
    if success {
        heap.record_write(bound.inner, &barrier_descriptor);
    }
    success
}

/// Delete a configurable bound function metadata property.
pub(crate) fn bound_delete_own_property(
    bound: &BoundFunction,
    heap: &mut otter_gc::GcHeap,
    key: &str,
) -> bool {
    heap.with_payload(bound.inner, |body| {
        let slot = match key {
            "name" => &mut body.name_property,
            "length" => &mut body.length_property,
            _ => return true,
        };
        let configurable = match slot {
            BoundFunctionMetadataProperty::Builtin => true,
            BoundFunctionMetadataProperty::Deleted => return true,
            BoundFunctionMetadataProperty::Overridden(desc) => desc.configurable(),
        };
        if !configurable {
            return false;
        }
        *slot = BoundFunctionMetadataProperty::Deleted;
        true
    })
}

/// Render a callable through the current foundation `toString` placeholder.
#[must_use]
pub(crate) fn callable_to_string(ctx: &FunctionMetadataContext<'_>, callee: &Value) -> String {
    let display = callable_name(ctx, callee).unwrap_or_default();
    format!("function {display}() {{ [native code] }}")
}

/// Compute builtin metadata for a newly created bound function.
pub(crate) fn bound_create_metadata(
    ctx: &FunctionMetadataContext<'_>,
    target: &Value,
    bound_arg_count: usize,
) -> Result<BoundFunctionCreateMetadata, VmError> {
    let target_name = callable_name(ctx, target)?;
    let target_len = callable_length(ctx, target)?;
    let length = (target_len - bound_arg_count as f64).max(0.0);
    Ok(BoundFunctionCreateMetadata {
        name: format!("bound {target_name}"),
        length: number_from_length_value(length),
    })
}

fn callable_metadata_value(
    ctx: &FunctionMetadataContext<'_>,
    callee: &Value,
    key: &str,
) -> Result<Option<Value>, VmError> {
    match key {
        "name" => callable_name(ctx, callee).and_then(|name| {
            JsString::from_str(&name, ctx.string_heap)
                .map(Value::String)
                .map(Some)
                .map_err(VmError::from)
        }),
        "length" => callable_length(ctx, callee)
            .map(|value| Some(Value::Number(number_from_length_value(value)))),
        _ => Ok(None),
    }
}

fn callable_name(ctx: &FunctionMetadataContext<'_>, callee: &Value) -> Result<String, VmError> {
    match callee {
        Value::Function { function_id } | Value::Closure { function_id, .. } => {
            if let Some(value) = ordinary_function_user_property(ctx, *function_id, "name") {
                return Ok(match value {
                    Value::String(s) => s.to_lossy_string(),
                    _ => String::new(),
                });
            }
            let function = ctx
                .module
                .functions
                .get(*function_id as usize)
                .ok_or(VmError::InvalidOperand)?;
            Ok(function.name.clone())
        }
        Value::NativeFunction(native) => {
            match native.own_property_descriptor(ctx.gc_heap, ctx.string_heap, "name")? {
                Some(desc) => Ok(match descriptor_value(&desc) {
                    Value::String(s) => s.to_lossy_string(),
                    _ => String::new(),
                }),
                None => Ok(String::new()),
            }
        }
        Value::BoundFunction(bound) => {
            match bound_own_property_descriptor(bound, ctx.gc_heap, ctx.string_heap, "name")? {
                Some(desc) => Ok(match descriptor_value(&desc) {
                    Value::String(s) => s.to_lossy_string(),
                    _ => String::new(),
                }),
                None => Ok(String::new()),
            }
        }
        Value::ClassConstructor(class) => callable_name(ctx, &class.ctor(ctx.gc_heap)),
        _ => Ok(String::new()),
    }
}

fn callable_length(ctx: &FunctionMetadataContext<'_>, callee: &Value) -> Result<f64, VmError> {
    match callee {
        Value::Function { function_id } | Value::Closure { function_id, .. } => {
            if let Some(value) = ordinary_function_user_property(ctx, *function_id, "length") {
                return Ok(match value {
                    Value::Number(n) => to_integer_or_infinity(n.as_f64()),
                    _ => 0.0,
                });
            }
            let function = ctx
                .module
                .functions
                .get(*function_id as usize)
                .ok_or(VmError::InvalidOperand)?;
            Ok(f64::from(function.param_count))
        }
        Value::NativeFunction(native) => {
            match native.own_property_descriptor(ctx.gc_heap, ctx.string_heap, "length")? {
                Some(desc) => Ok(match descriptor_value(&desc) {
                    Value::Number(n) => to_integer_or_infinity(n.as_f64()),
                    _ => 0.0,
                }),
                None => Ok(0.0),
            }
        }
        Value::BoundFunction(bound) => {
            match bound_own_property_descriptor(bound, ctx.gc_heap, ctx.string_heap, "length")? {
                Some(desc) => Ok(match descriptor_value(&desc) {
                    Value::Number(n) => to_integer_or_infinity(n.as_f64()),
                    _ => 0.0,
                }),
                None => Ok(0.0),
            }
        }
        Value::ClassConstructor(class) => callable_length(ctx, &class.ctor(ctx.gc_heap)),
        _ => Ok(0.0),
    }
}

fn ordinary_function_user_property(
    ctx: &FunctionMetadataContext<'_>,
    function_id: u32,
    key: &str,
) -> Option<Value> {
    let bag = ctx.function_user_props.get(&function_id).copied()?;
    object::get_own(bag, ctx.gc_heap, key)
}

fn bound_builtin_descriptor(
    body: &crate::BoundFunctionBody,
    string_heap: &StringHeap,
    key: &str,
) -> Result<PropertyDescriptor, VmError> {
    let value = match key {
        "name" => Value::String(JsString::from_str(&body.builtin_name, string_heap)?),
        "length" => Value::Number(body.builtin_length),
        _ => Value::Undefined,
    };
    Ok(PropertyDescriptor::data(value, false, false, true))
}

fn descriptor_value(desc: &PropertyDescriptor) -> Value {
    match &desc.kind {
        DescriptorKind::Data { value } => value.clone(),
        DescriptorKind::Accessor { .. } => Value::Undefined,
    }
}

fn number_from_length_value(value: f64) -> NumberValue {
    NumberValue::from_f64(value)
}

fn to_integer_or_infinity(value: f64) -> f64 {
    if value.is_nan() || value == 0.0 {
        0.0
    } else if value.is_infinite() {
        value
    } else {
        value.trunc()
    }
}
