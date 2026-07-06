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
//! - Bound-function own-property helpers for descriptor and enumeration APIs.
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

use std::collections::HashSet;

use crate::number::NumberValue;
use crate::object::{self, DescriptorKind, JsObject, PropertyDescriptor};
use crate::string::JsString;
use crate::{BoundFunction, BoundFunctionMetadataProperty, ExecutionContext, Value, VmError};

/// Read-only inputs needed to resolve callable metadata.
pub(crate) struct FunctionMetadataContext<'a> {
    context: &'a ExecutionContext,
    pub(crate) gc_heap: &'a mut otter_gc::GcHeap,
    /// This callable instance's own-property bag (per-instance for
    /// closures, the template side-table bag for bare functions), or
    /// `None` when no expandos exist. Used to honour user overrides of
    /// the intrinsic `name` / `length` properties.
    owner_bag: Option<JsObject>,
    function_deleted_metadata: &'a HashSet<(u32, &'static str)>,
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
        context: &'a ExecutionContext,
        gc_heap: &'a mut otter_gc::GcHeap,
        owner_bag: Option<JsObject>,
        function_deleted_metadata: &'a HashSet<(u32, &'static str)>,
    ) -> Self {
        Self {
            context,
            gc_heap,
            owner_bag,
            function_deleted_metadata,
        }
    }

    /// Shared borrow of the heap. Use for reader-only paths.
    pub(crate) fn heap(&self) -> &otter_gc::GcHeap {
        &*self.gc_heap
    }
}

/// Read `name` or `length` from any callable metadata shape.
pub(crate) fn callable_intrinsic_property(
    ctx: &mut FunctionMetadataContext<'_>,
    callee: &Value,
    key: &str,
) -> Result<Value, VmError> {
    match callable_metadata_value(ctx, callee, key)? {
        Some(value) => Ok(value),
        None => Ok(Value::undefined()),
    }
}

/// Read `name` or `length` from an ordinary function record.
pub(crate) fn ordinary_function_intrinsic_property(
    ctx: &mut FunctionMetadataContext<'_>,
    function_id: u32,
    key: &str,
) -> Result<Value, VmError> {
    callable_intrinsic_property(ctx, &Value::function(function_id), key)
}

/// Return the own descriptor for a bound function metadata property.
pub(crate) fn bound_own_property_descriptor(
    bound: &BoundFunction,
    gc_heap: &mut otter_gc::GcHeap,
    key: &str,
) -> Result<Option<PropertyDescriptor>, VmError> {
    enum Slot {
        Builtin { name: String, length: NumberValue },
        Deleted,
        Overridden(PropertyDescriptor),
        Ordinary(JsObject),
    }
    let slot = gc_heap.read_payload(bound.inner, |body| {
        let property = match key {
            "name" => &body.name_property,
            "length" => &body.length_property,
            _ => return Slot::Ordinary(body.own_properties),
        };
        match property {
            BoundFunctionMetadataProperty::Builtin => Slot::Builtin {
                name: body.builtin_name.clone(),
                length: body.builtin_length,
            },
            BoundFunctionMetadataProperty::Deleted => Slot::Deleted,
            BoundFunctionMetadataProperty::Overridden(desc) => Slot::Overridden(desc.clone()),
        }
    });
    match slot {
        Slot::Ordinary(own_properties) => {
            Ok(object::get_own_descriptor(own_properties, gc_heap, key))
        }
        Slot::Deleted => Ok(None),
        Slot::Overridden(desc) => Ok(Some(desc)),
        Slot::Builtin { name, length } => {
            let value = match key {
                "name" => Value::string(JsString::from_str(&name, gc_heap)?),
                "length" => Value::number(length),
                _ => Value::undefined(),
            };
            Ok(Some(PropertyDescriptor::data(value, false, false, true)))
        }
    }
}

/// Return own string property keys in built-in creation order.
#[must_use]
pub(crate) fn bound_own_property_keys(
    bound: &BoundFunction,
    gc_heap: &otter_gc::GcHeap,
) -> Vec<String> {
    gc_heap.read_payload(bound.inner, |body| {
        let mut keys = Vec::new();
        if !matches!(body.length_property, BoundFunctionMetadataProperty::Deleted) {
            keys.push("length".to_string());
        }
        if !matches!(body.name_property, BoundFunctionMetadataProperty::Deleted) {
            keys.push("name".to_string());
        }
        keys.extend(object::with_properties(body.own_properties, gc_heap, |p| {
            p.keys()
                .filter(|key| *key != "name" && *key != "length")
                .map(|key| key.to_string())
                .collect::<Vec<_>>()
        }));
        keys
    })
}

/// Return enumerable own string property keys in built-in creation order.
#[must_use]
pub(crate) fn bound_enumerable_own_property_keys(
    bound: &BoundFunction,
    gc_heap: &otter_gc::GcHeap,
) -> Vec<String> {
    gc_heap.read_payload(bound.inner, |body| {
        let mut keys = Vec::new();
        if bound_metadata_property_is_enumerable(&body.length_property, false) {
            keys.push("length".to_string());
        }
        if bound_metadata_property_is_enumerable(&body.name_property, false) {
            keys.push("name".to_string());
        }
        keys.extend(object::with_properties(body.own_properties, gc_heap, |p| {
            p.enumerable_keys()
                .filter(|key| *key != "name" && *key != "length")
                .map(|key| key.to_string())
                .collect::<Vec<_>>()
        }));
        keys
    })
}

/// Test whether a bound function own metadata property is enumerable.
#[must_use]
pub(crate) fn bound_own_property_is_enumerable(
    bound: &BoundFunction,
    gc_heap: &otter_gc::GcHeap,
    key: &str,
) -> bool {
    gc_heap.read_payload(bound.inner, |body| match key {
        "name" => bound_metadata_property_is_enumerable(&body.name_property, false),
        "length" => bound_metadata_property_is_enumerable(&body.length_property, false),
        _ => match object::lookup_own(body.own_properties, gc_heap, key) {
            object::PropertyLookup::Data { flags, .. }
            | object::PropertyLookup::Accessor { flags, .. } => flags.enumerable(),
            object::PropertyLookup::Absent => false,
        },
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
        _ => !matches!(
            object::lookup_own(body.own_properties, gc_heap, key),
            object::PropertyLookup::Absent
        ),
    })
}

/// Define or redefine a bound function metadata property.
pub(crate) fn bound_define_own_property(
    bound: &BoundFunction,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    descriptor: PropertyDescriptor,
) -> bool {
    let existing = match bound_own_property_descriptor(bound, heap, key) {
        Ok(existing) => existing,
        Err(_) => return false,
    };
    let descriptor = match existing {
        Some(existing) => match object::validate_descriptor_update(&existing, &descriptor, heap) {
            Some(merged) => merged,
            None => return false,
        },
        None if key == "name" || key == "length" => descriptor,
        None => descriptor,
    };
    if key != "name" && key != "length" {
        let own_properties = heap.read_payload(bound.inner, |body| body.own_properties);
        return object::define_own_property(own_properties, heap, key, descriptor);
    }
    let barrier_descriptor = descriptor.clone();
    let success = heap.with_payload(bound.inner, |body| {
        let slot = match key {
            "name" => &mut body.name_property,
            "length" => &mut body.length_property,
            _ => unreachable!("ordinary bound properties return before metadata update"),
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
    if key != "name" && key != "length" {
        let own_properties = heap.read_payload(bound.inner, |body| body.own_properties);
        return object::delete(own_properties, heap, key);
    }
    heap.with_payload(bound.inner, |body| {
        let slot = match key {
            "name" => &mut body.name_property,
            "length" => &mut body.length_property,
            _ => unreachable!("ordinary bound properties return before metadata delete"),
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

/// `true` when `name` is a syntactically valid `IdentifierName`
/// (§12.7) usable in the `NativeFunction` grammar's optional name slot.
/// Internal display placeholders (`<anonymous>`, `<arrow>`), computed /
/// symbol method names (`[Symbol.iterator]`), and `bound `-prefixed
/// names contain characters that are not `IdentifierPart`, so they must
/// be omitted rather than emitted into an otherwise-parseable native
/// function source.
/// `true` for the compiler's internal anonymous-callable display
/// placeholders, whose observable `name` property is the empty string.
fn is_anon_name_placeholder(name: &str) -> bool {
    matches!(name, "<anonymous>" | "<arrow>" | "<class>")
}

fn is_identifier_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c == '$' || c == '_' || c.is_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '$' || c == '_' || c.is_alphanumeric())
}

/// Render a callable's `Function.prototype.toString` value (§20.2.3.5).
/// A user function / class carries its verbatim [[SourceText]], so its
/// definition source is returned exactly. Native functions, bound
/// functions, and any synthesized callable without source fall back to
/// the `NativeFunction` form; the optional name is emitted only when it
/// is a valid `IdentifierName`, keeping the result parseable.
#[must_use]
pub(crate) fn callable_to_string(ctx: &mut FunctionMetadataContext<'_>, callee: &Value) -> String {
    // A class binding is a class-constructor wrapper; its [[SourceText]]
    // (the whole `class … {}`) lives on the inner constructor function.
    let resolved = match callee.as_class_constructor() {
        Some(class) => class.ctor(ctx.heap()),
        None => *callee,
    };
    if let Some(fid) = resolved.as_function().or_else(|| {
        resolved
            .as_closure(ctx.heap())
            .map(|c| c.cached_function_id)
    }) && let Some(function) = ctx.context.function(fid)
        && let Some(source) = &function.source_text
    {
        return source.clone();
    }
    let display = callable_name(ctx, callee).unwrap_or_default();
    if is_identifier_name(&display) {
        format!("function {display}() {{ [native code] }}")
    } else {
        "function () { [native code] }".to_string()
    }
}

/// Compute bound-function metadata from spec-observable
/// `Get(target, "name")` / `Get(target, "length")` results.
#[must_use]
pub(crate) fn bound_create_metadata_from_values(
    target_name: &Value,
    target_length: &Value,
    bound_arg_count: usize,
    heap: &otter_gc::GcHeap,
) -> BoundFunctionCreateMetadata {
    let target_name = target_name
        .as_string(heap)
        .map(|s| s.to_lossy_string(heap))
        .unwrap_or_default();
    let target_len = target_length
        .as_number()
        .map(|n| to_integer_or_infinity(n.as_f64()))
        .unwrap_or(0.0);
    let length = (target_len - bound_arg_count as f64).max(0.0);
    BoundFunctionCreateMetadata {
        name: format!("bound {target_name}"),
        length: number_from_length_value(length),
    }
}

fn callable_metadata_value(
    ctx: &mut FunctionMetadataContext<'_>,
    callee: &Value,
    key: &str,
) -> Result<Option<Value>, VmError> {
    match key {
        "name" => callable_name(ctx, callee).and_then(|name| {
            JsString::from_str(&name, ctx.gc_heap)
                .map(Value::string)
                .map(Some)
                .map_err(VmError::from)
        }),
        "length" => callable_length(ctx, callee)
            .map(|value| Some(Value::number(number_from_length_value(value)))),
        _ => Ok(None),
    }
}

fn callable_name(ctx: &mut FunctionMetadataContext<'_>, callee: &Value) -> Result<String, VmError> {
    if let Some(fid) = callee
        .as_function()
        .or_else(|| callee.as_closure(ctx.heap()).map(|c| c.cached_function_id))
    {
        if let Some(value) = ordinary_function_user_property(ctx, fid, "name") {
            return Ok(value
                .as_string(ctx.heap())
                .map(|s| s.to_lossy_string(ctx.heap()))
                .unwrap_or_default());
        }
        if ordinary_function_metadata_deleted(ctx, fid, "name") {
            return Ok(String::new());
        }
        let function = ctx.context.function(fid).ok_or(VmError::InvalidOperand)?;
        // §15.2.5 / §15.4.5 — an anonymous function / arrow / class with
        // no NamedEvaluation context has a `name` of `""`. The compiler
        // records an internal `<…>` placeholder for diagnostics; surface
        // it as the empty string for the observable `name` property.
        return Ok(if is_anon_name_placeholder(&function.name) {
            String::new()
        } else {
            function.name.clone()
        });
    }
    if let Some(native) = callee.as_native_function() {
        return match native.own_property_descriptor(ctx.gc_heap, "name")? {
            Some(desc) => Ok(descriptor_value(&desc)
                .as_string(ctx.heap())
                .map(|s| s.to_lossy_string(ctx.heap()))
                .unwrap_or_default()),
            None => Ok(String::new()),
        };
    }
    if let Some(bound) = callee.as_bound_function() {
        return match bound_own_property_descriptor(&bound, ctx.gc_heap, "name")? {
            Some(desc) => Ok(descriptor_value(&desc)
                .as_string(ctx.heap())
                .map(|s| s.to_lossy_string(ctx.heap()))
                .unwrap_or_default()),
            None => Ok(String::new()),
        };
    }
    if let Some(class) = callee.as_class_constructor() {
        return callable_name(ctx, &class.ctor(ctx.gc_heap));
    }
    if let Some(obj) = callee.as_object() {
        if let Some(native) =
            object::constructor_native(obj, ctx.gc_heap).and_then(|v| v.as_native_function())
        {
            return callable_name(ctx, &Value::native_function(native));
        }
        return Ok(String::new());
    }
    Ok(String::new())
}

fn callable_length(ctx: &mut FunctionMetadataContext<'_>, callee: &Value) -> Result<f64, VmError> {
    if let Some(fid) = callee
        .as_function()
        .or_else(|| callee.as_closure(ctx.heap()).map(|c| c.cached_function_id))
    {
        if let Some(value) = ordinary_function_user_property(ctx, fid, "length") {
            return Ok(value
                .as_number()
                .map(|n| to_integer_or_infinity(n.as_f64()))
                .unwrap_or(0.0));
        }
        if ordinary_function_metadata_deleted(ctx, fid, "length") {
            return Ok(0.0);
        }
        let function = ctx.context.function(fid).ok_or(VmError::InvalidOperand)?;
        return Ok(f64::from(function.length));
    }
    if let Some(native) = callee.as_native_function() {
        return match native.own_property_descriptor(ctx.gc_heap, "length")? {
            Some(desc) => Ok(descriptor_value(&desc)
                .as_number()
                .map(|n| to_integer_or_infinity(n.as_f64()))
                .unwrap_or(0.0)),
            None => Ok(0.0),
        };
    }
    if let Some(bound) = callee.as_bound_function() {
        return match bound_own_property_descriptor(&bound, ctx.gc_heap, "length")? {
            Some(desc) => Ok(descriptor_value(&desc)
                .as_number()
                .map(|n| to_integer_or_infinity(n.as_f64()))
                .unwrap_or(0.0)),
            None => Ok(0.0),
        };
    }
    if let Some(class) = callee.as_class_constructor() {
        return callable_length(ctx, &class.ctor(ctx.gc_heap));
    }
    if let Some(obj) = callee.as_object() {
        if let Some(native) =
            object::constructor_native(obj, ctx.gc_heap).and_then(|v| v.as_native_function())
        {
            return callable_length(ctx, &Value::native_function(native));
        }
        return Ok(0.0);
    }
    Ok(0.0)
}

fn ordinary_function_user_property(
    ctx: &mut FunctionMetadataContext<'_>,
    function_id: u32,
    key: &str,
) -> Option<Value> {
    if ordinary_function_metadata_deleted(ctx, function_id, key) {
        return None;
    }
    let bag = ctx.owner_bag?;
    object::get_own(bag, ctx.gc_heap, key)
}

fn ordinary_function_metadata_deleted(
    ctx: &mut FunctionMetadataContext<'_>,
    function_id: u32,
    key: &str,
) -> bool {
    let Some(key) = ordinary_function_metadata_key(key) else {
        return false;
    };
    ctx.function_deleted_metadata.contains(&(function_id, key))
}

pub(crate) fn ordinary_function_metadata_key(key: &str) -> Option<&'static str> {
    match key {
        "name" => Some("name"),
        "length" => Some("length"),
        _ => None,
    }
}

fn descriptor_value(desc: &PropertyDescriptor) -> Value {
    match &desc.kind {
        DescriptorKind::Data { value } => *value,
        DescriptorKind::Accessor { .. } => Value::undefined(),
    }
}

fn bound_metadata_property_is_enumerable(
    property: &BoundFunctionMetadataProperty,
    builtin_default: bool,
) -> bool {
    match property {
        BoundFunctionMetadataProperty::Builtin => builtin_default,
        BoundFunctionMetadataProperty::Deleted => false,
        BoundFunctionMetadataProperty::Overridden(desc) => desc.flags.enumerable(),
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
