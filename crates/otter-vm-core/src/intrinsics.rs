//! Intrinsics registry for ECMAScript built-in objects and well-known symbols.
//!
//! This module provides the `Intrinsics` struct which holds references to all
//! intrinsic objects (constructors, prototypes) and well-known symbols.
//! It is created once per `VmRuntime` and shared across contexts.
//!
//! The initialization follows a two-stage pattern (inspired by Boa):
//! 1. **Stage 1**: Allocate empty prototype/constructor objects to break circular deps
//! 2. **Stage 2**: Initialize properties in dependency order using `BuiltInBuilder`

use std::sync::Arc;


use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::JsObject;
use crate::intrinsics_impl::helpers::{same_value_zero, strict_equal};

use crate::value::{Symbol, Value};



/// Well-known symbol IDs (fixed, pre-defined).
/// These must match the IDs in `otter-vm-builtins/src/symbol.rs`.
pub mod well_known {
    /// `Symbol.iterator`
    pub const ITERATOR: u64 = 1;
    /// `Symbol.asyncIterator`
    pub const ASYNC_ITERATOR: u64 = 2;
    /// `Symbol.toStringTag`
    pub const TO_STRING_TAG: u64 = 3;
    /// `Symbol.hasInstance`
    pub const HAS_INSTANCE: u64 = 4;
    /// `Symbol.toPrimitive`
    pub const TO_PRIMITIVE: u64 = 5;
    /// `Symbol.isConcatSpreadable`
    pub const IS_CONCAT_SPREADABLE: u64 = 6;
    /// `Symbol.match`
    pub const MATCH: u64 = 7;
    /// `Symbol.matchAll`
    pub const MATCH_ALL: u64 = 8;
    /// `Symbol.replace`
    pub const REPLACE: u64 = 9;
    /// `Symbol.search`
    pub const SEARCH: u64 = 10;
    /// `Symbol.split`
    pub const SPLIT: u64 = 11;
    /// `Symbol.species`
    pub const SPECIES: u64 = 12;
    /// `Symbol.unscopables`
    pub const UNSCOPABLES: u64 = 13;
}

/// Registry of all ECMAScript intrinsic objects and well-known symbols.
///
/// Created once per `VmRuntime`, shared across all contexts.
/// Provides direct Rust access to intrinsics without JS global lookups.
#[derive(Clone)]
pub struct Intrinsics {
    // ========================================================================
    // Core prototypes
    // ========================================================================
    /// `Object.prototype` — `[[Prototype]]` is `null`
    pub object_prototype: GcRef<JsObject>,
    /// `Function.prototype` — `[[Prototype]]` is `Object.prototype`
    pub function_prototype: GcRef<JsObject>,

    // ========================================================================
    // Core constructors
    // ========================================================================
    /// `Object` constructor
    pub object_constructor: GcRef<JsObject>,
    /// `Function` constructor
    pub function_constructor: GcRef<JsObject>,

    // ========================================================================
    // Primitive wrapper prototypes
    // ========================================================================
    /// `String.prototype`
    pub string_prototype: GcRef<JsObject>,
    /// `Number.prototype`
    pub number_prototype: GcRef<JsObject>,
    /// `Boolean.prototype`
    pub boolean_prototype: GcRef<JsObject>,
    /// `Symbol.prototype`
    pub symbol_prototype: GcRef<JsObject>,
    /// `BigInt.prototype`
    pub bigint_prototype: GcRef<JsObject>,

    // ========================================================================
    // Collection prototypes
    // ========================================================================
    /// `Array.prototype`
    pub array_prototype: GcRef<JsObject>,
    /// `Map.prototype`
    pub map_prototype: GcRef<JsObject>,
    /// `Set.prototype`
    pub set_prototype: GcRef<JsObject>,
    /// `WeakMap.prototype`
    pub weak_map_prototype: GcRef<JsObject>,
    /// `WeakSet.prototype`
    pub weak_set_prototype: GcRef<JsObject>,

    // ========================================================================
    // Error prototypes
    // ========================================================================
    /// `Error.prototype`
    pub error_prototype: GcRef<JsObject>,
    /// `TypeError.prototype`
    pub type_error_prototype: GcRef<JsObject>,
    /// `RangeError.prototype`
    pub range_error_prototype: GcRef<JsObject>,
    /// `ReferenceError.prototype`
    pub reference_error_prototype: GcRef<JsObject>,
    /// `SyntaxError.prototype`
    pub syntax_error_prototype: GcRef<JsObject>,
    /// `URIError.prototype`
    pub uri_error_prototype: GcRef<JsObject>,
    /// `EvalError.prototype`
    pub eval_error_prototype: GcRef<JsObject>,

    // ========================================================================
    // Async/Promise
    // ========================================================================
    /// `Promise.prototype`
    pub promise_prototype: GcRef<JsObject>,

    // ========================================================================
    // Other built-in prototypes
    // ========================================================================
    /// `RegExp.prototype`
    pub regexp_prototype: GcRef<JsObject>,
    /// `Date.prototype`
    pub date_prototype: GcRef<JsObject>,
    /// `ArrayBuffer.prototype`
    pub array_buffer_prototype: GcRef<JsObject>,
    /// `DataView.prototype`
    pub data_view_prototype: GcRef<JsObject>,

    // ========================================================================
    // Iterator prototypes
    // ========================================================================
    /// `%IteratorPrototype%` — base for all iterator prototypes
    pub iterator_prototype: GcRef<JsObject>,
    /// `%AsyncIteratorPrototype%`
    pub async_iterator_prototype: GcRef<JsObject>,

    // ========================================================================
    // Well-known symbols (Value::symbol)
    // ========================================================================
    /// `Symbol.iterator`
    pub symbol_iterator: Value,
    /// `Symbol.asyncIterator`
    pub symbol_async_iterator: Value,
    /// `Symbol.toStringTag`
    pub symbol_to_string_tag: Value,
    /// `Symbol.hasInstance`
    pub symbol_has_instance: Value,
    /// `Symbol.toPrimitive`
    pub symbol_to_primitive: Value,
    /// `Symbol.isConcatSpreadable`
    pub symbol_is_concat_spreadable: Value,
    /// `Symbol.match`
    pub symbol_match: Value,
    /// `Symbol.matchAll`
    pub symbol_match_all: Value,
    /// `Symbol.replace`
    pub symbol_replace: Value,
    /// `Symbol.search`
    pub symbol_search: Value,
    /// `Symbol.split`
    pub symbol_split: Value,
    /// `Symbol.species`
    pub symbol_species: Value,
    /// `Symbol.unscopables`
    pub symbol_unscopables: Value,
}

impl Intrinsics {
    /// Create a new `Intrinsics` with all objects allocated but NOT yet initialized.
    ///
    /// This is Stage 1 of the two-stage initialization. Call `init()` after
    /// this to populate properties and wire prototype chains (Stage 2).
    ///
    /// `fn_proto` is the pre-existing intrinsic `%Function.prototype%` created
    /// by `VmRuntime` before this call.
    pub fn allocate(mm: &Arc<MemoryManager>, fn_proto: GcRef<JsObject>) -> Self {
        // Helper to allocate an empty object with no prototype
        let alloc = || GcRef::new(JsObject::new(None, mm.clone()));

        // Create well-known symbols
        let make_symbol = |id: u64, desc: &str| -> Value {
            Value::symbol(Arc::new(Symbol {
                description: Some(desc.to_string()),
                id,
            }))
        };

        let result = Self {
            // Core prototypes
            object_prototype: alloc(),
            function_prototype: fn_proto, // Reuse existing intrinsic
            // Core constructors
            object_constructor: alloc(),
            function_constructor: alloc(),
            // Primitive wrappers
            string_prototype: alloc(),
            number_prototype: alloc(),
            boolean_prototype: alloc(),
            symbol_prototype: alloc(),
            bigint_prototype: alloc(),
            // Collections
            array_prototype: alloc(),
            map_prototype: alloc(),
            set_prototype: alloc(),
            weak_map_prototype: alloc(),
            weak_set_prototype: alloc(),
            // Errors
            error_prototype: alloc(),
            type_error_prototype: alloc(),
            range_error_prototype: alloc(),
            reference_error_prototype: alloc(),
            syntax_error_prototype: alloc(),
            uri_error_prototype: alloc(),
            eval_error_prototype: alloc(),
            // Promise
            promise_prototype: alloc(),
            // Other
            regexp_prototype: alloc(),
            date_prototype: alloc(),
            array_buffer_prototype: alloc(),
            data_view_prototype: alloc(),
            // Iterators
            iterator_prototype: alloc(),
            async_iterator_prototype: alloc(),
            // Well-known symbols
            symbol_iterator: make_symbol(well_known::ITERATOR, "Symbol.iterator"),
            symbol_async_iterator: make_symbol(well_known::ASYNC_ITERATOR, "Symbol.asyncIterator"),
            symbol_to_string_tag: make_symbol(well_known::TO_STRING_TAG, "Symbol.toStringTag"),
            symbol_has_instance: make_symbol(well_known::HAS_INSTANCE, "Symbol.hasInstance"),
            symbol_to_primitive: make_symbol(well_known::TO_PRIMITIVE, "Symbol.toPrimitive"),
            symbol_is_concat_spreadable: make_symbol(
                well_known::IS_CONCAT_SPREADABLE,
                "Symbol.isConcatSpreadable",
            ),
            symbol_match: make_symbol(well_known::MATCH, "Symbol.match"),
            symbol_match_all: make_symbol(well_known::MATCH_ALL, "Symbol.matchAll"),
            symbol_replace: make_symbol(well_known::REPLACE, "Symbol.replace"),
            symbol_search: make_symbol(well_known::SEARCH, "Symbol.search"),
            symbol_split: make_symbol(well_known::SPLIT, "Symbol.split"),
            symbol_species: make_symbol(well_known::SPECIES, "Symbol.species"),
            symbol_unscopables: make_symbol(well_known::UNSCOPABLES, "Symbol.unscopables"),
        };

        // Mark all intrinsic objects so they are protected from teardown clearing.
        // When a VmContext is torn down, DropGuard calls clear_and_extract_values()
        // on reachable objects; intrinsics are shared across contexts and must survive.
        let all_intrinsic_objects: &[GcRef<JsObject>] = &[
            result.object_prototype,
            result.function_prototype,
            result.object_constructor,
            result.function_constructor,
            result.string_prototype,
            result.number_prototype,
            result.boolean_prototype,
            result.symbol_prototype,
            result.bigint_prototype,
            result.array_prototype,
            result.map_prototype,
            result.set_prototype,
            result.weak_map_prototype,
            result.weak_set_prototype,
            result.error_prototype,
            result.type_error_prototype,
            result.range_error_prototype,
            result.reference_error_prototype,
            result.syntax_error_prototype,
            result.uri_error_prototype,
            result.eval_error_prototype,
            result.promise_prototype,
            result.regexp_prototype,
            result.date_prototype,
            result.array_buffer_prototype,
            result.data_view_prototype,
            result.iterator_prototype,
            result.async_iterator_prototype,
        ];
        for obj in all_intrinsic_objects {
            (*obj).mark_as_intrinsic();
        }

        result
    }

    /// Stage 2: Wire up prototype chains for all intrinsic objects.
    ///
    /// This sets the `[[Prototype]]` of each intrinsic object according to
    /// the ECMAScript specification. Must be called after `allocate()`.
    pub fn wire_prototype_chains(&self) {
        // Object.prototype.[[Prototype]] = null (already null from allocate)

        // Function.prototype.[[Prototype]] = Object.prototype
        self.function_prototype
            .set_prototype(Some(self.object_prototype));

        // All other prototypes chain to Object.prototype
        let protos_to_obj = [
            self.string_prototype,
            self.number_prototype,
            self.boolean_prototype,
            self.symbol_prototype,
            self.bigint_prototype,
            self.array_prototype,
            self.map_prototype,
            self.set_prototype,
            self.weak_map_prototype,
            self.weak_set_prototype,
            self.promise_prototype,
            self.regexp_prototype,
            self.date_prototype,
            self.array_buffer_prototype,
            self.data_view_prototype,
            self.iterator_prototype,
        ];
        for proto in &protos_to_obj {
            proto.set_prototype(Some(self.object_prototype));
        }

        // Error.prototype.[[Prototype]] = Object.prototype
        self.error_prototype
            .set_prototype(Some(self.object_prototype));

        // All specific error prototypes chain to Error.prototype
        let error_protos = [
            self.type_error_prototype,
            self.range_error_prototype,
            self.reference_error_prototype,
            self.syntax_error_prototype,
            self.uri_error_prototype,
            self.eval_error_prototype,
        ];
        for proto in &error_protos {
            proto.set_prototype(Some(self.error_prototype));
        }

        // AsyncIteratorPrototype.[[Prototype]] = Object.prototype
        self.async_iterator_prototype
            .set_prototype(Some(self.object_prototype));

        // Constructor objects: [[Prototype]] = Function.prototype
        let ctors = [self.object_constructor, self.function_constructor];
        for ctor in &ctors {
            ctor.set_prototype(Some(self.function_prototype));
        }
    }

    /// Stage 3: Initialize core intrinsic properties using `BuiltInBuilder`.
    ///
    /// This populates Object.prototype, Function.prototype, and Error prototypes
    /// with their spec-required methods and properties. Must be called after
    /// `wire_prototype_chains()`.
    pub fn init_core(&self, mm: &Arc<MemoryManager>) {
        use crate::builtin_builder::BuiltInBuilder;
        use crate::object::{PropertyAttributes, PropertyDescriptor, PropertyKey};
        use crate::string::JsString;

        // ====================================================================
        // Object.prototype methods (non-enumerable)
        // ====================================================================
        let obj_proto = self.object_prototype;
        let fn_proto = self.function_prototype;

        // Object.prototype.toString
        obj_proto.define_property(
            PropertyKey::string("toString"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, _args, _mm| Ok(Value::string(JsString::intern("[object Object]"))),
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.prototype.valueOf
        obj_proto.define_property(
            PropertyKey::string("valueOf"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, _args, _mm| Ok(this_val.clone()),
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.prototype.hasOwnProperty
        obj_proto.define_property(
            PropertyKey::string("hasOwnProperty"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _mm| {
                    if let Some(obj) = this_val.as_object() {
                        if let Some(key) = args.first() {
                            if let Some(s) = key.as_string() {
                                return Ok(Value::boolean(
                                    obj.has_own(&PropertyKey::string(s.as_str())),
                                ));
                            }
                        }
                    }
                    Ok(Value::boolean(false))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.prototype.isPrototypeOf
        obj_proto.define_property(
            PropertyKey::string("isPrototypeOf"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _mm| {
                    if let Some(target) = args.first().and_then(|v| v.as_object()) {
                        if let Some(this_obj) = this_val.as_object() {
                            let mut current = target.prototype();
                            while let Some(proto) = current {
                                if std::ptr::eq(
                                    proto.as_ptr() as *const _,
                                    this_obj.as_ptr() as *const _,
                                ) {
                                    return Ok(Value::boolean(true));
                                }
                                current = proto.prototype();
                            }
                        }
                    }
                    Ok(Value::boolean(false))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.prototype.propertyIsEnumerable
        obj_proto.define_property(
            PropertyKey::string("propertyIsEnumerable"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _mm| {
                    if let Some(obj) = this_val.as_object() {
                        if let Some(key) = args.first() {
                            if let Some(s) = key.as_string() {
                                let pk = PropertyKey::string(s.as_str());
                                if let Some(desc) = obj.get_own_property_descriptor(&pk) {
                                    return Ok(Value::boolean(desc.enumerable()));
                                }
                            }
                        }
                    }
                    Ok(Value::boolean(false))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // ===================================================================
        // Function.prototype methods (extracted to intrinsics_impl/function.rs)
        // ===================================================================
        crate::intrinsics_impl::function::init_function_prototype(fn_proto, mm);

        // ====================================================================
        // Error.prototype properties
        // ====================================================================
        self.error_prototype.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::data_with_attrs(
                Value::string(JsString::intern("Error")),
                PropertyAttributes::builtin_method(),
            ),
        );
        self.error_prototype.define_property(
            PropertyKey::string("message"),
            PropertyDescriptor::data_with_attrs(
                Value::string(JsString::intern("")),
                PropertyAttributes::builtin_method(),
            ),
        );

        // Error.prototype.toString
        self.error_prototype.define_property(
            PropertyKey::string("toString"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, _args, _mm| {
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
                fn_proto,
            )),
        );

        // Error type-specific names
        let error_names = [
            (self.type_error_prototype, "TypeError"),
            (self.range_error_prototype, "RangeError"),
            (self.reference_error_prototype, "ReferenceError"),
            (self.syntax_error_prototype, "SyntaxError"),
            (self.uri_error_prototype, "URIError"),
            (self.eval_error_prototype, "EvalError"),
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
        }

        // ====================================================================
        // Object static methods (on Object constructor, non-enumerable)
        // ====================================================================
        let obj_ctor = self.object_constructor;

        // Object.getPrototypeOf
        obj_ctor.define_property(
            PropertyKey::string("getPrototypeOf"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _mm| {
                    if let Some(obj) = args.first().and_then(|v| v.as_object()) {
                        match obj.prototype() {
                            Some(proto) => Ok(Value::object(proto)),
                            None => Ok(Value::null()),
                        }
                    } else {
                        Ok(Value::null())
                    }
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.setPrototypeOf
        obj_ctor.define_property(
            PropertyKey::string("setPrototypeOf"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _mm| {
                    let target = args.first().cloned().unwrap_or(Value::undefined());
                    if let Some(obj) = target.as_object() {
                        let proto_val = args.get(1).cloned().unwrap_or(Value::undefined());
                        let proto = if proto_val.is_null() {
                            None
                        } else {
                            proto_val.as_object()
                        };
                        if !obj.set_prototype(proto) {
                            return Err(
                                VmError::type_error("Object.setPrototypeOf failed")
                            );
                        }
                    }
                    Ok(target)
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.getOwnPropertyDescriptor
        obj_ctor.define_property(
            PropertyKey::string("getOwnPropertyDescriptor"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, mm_inner| {
                    let target = args.first().and_then(|v| v.as_object());
                    let key = args.get(1).and_then(|v| v.as_string());
                    if let (Some(obj), Some(key_str)) = (target, key) {
                        let pk = PropertyKey::string(key_str.as_str());
                        if let Some(desc) = obj.get_own_property_descriptor(&pk) {
                            // Build descriptor object
                            let desc_obj =
                                GcRef::new(JsObject::new(None, mm_inner));
                            match &desc {
                                PropertyDescriptor::Data { value, attributes } => {
                                    desc_obj.set(
                                        PropertyKey::string("value"),
                                        value.clone(),
                                    );
                                    desc_obj.set(
                                        PropertyKey::string("writable"),
                                        Value::boolean(attributes.writable),
                                    );
                                    desc_obj.set(
                                        PropertyKey::string("enumerable"),
                                        Value::boolean(attributes.enumerable),
                                    );
                                    desc_obj.set(
                                        PropertyKey::string("configurable"),
                                        Value::boolean(attributes.configurable),
                                    );
                                }
                                PropertyDescriptor::Accessor {
                                    get,
                                    set,
                                    attributes,
                                } => {
                                    desc_obj.set(
                                        PropertyKey::string("get"),
                                        get.clone().unwrap_or(Value::undefined()),
                                    );
                                    desc_obj.set(
                                        PropertyKey::string("set"),
                                        set.clone().unwrap_or(Value::undefined()),
                                    );
                                    desc_obj.set(
                                        PropertyKey::string("enumerable"),
                                        Value::boolean(attributes.enumerable),
                                    );
                                    desc_obj.set(
                                        PropertyKey::string("configurable"),
                                        Value::boolean(attributes.configurable),
                                    );
                                }
                                PropertyDescriptor::Deleted => {}
                            }
                            return Ok(Value::object(desc_obj));
                        }
                    }
                    Ok(Value::undefined())
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.keys
        obj_ctor.define_property(
            PropertyKey::string("keys"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _mm| {
                    let obj = args
                        .first()
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| "Object.keys requires an object".to_string())?;
                    let keys = obj.own_keys();
                    let mut names = Vec::new();
                    for key in keys {
                        match &key {
                            PropertyKey::String(s) => {
                                if let Some(desc) = obj.get_own_property_descriptor(&key) {
                                    if desc.enumerable() {
                                        names.push(Value::string(s.clone()));
                                    }
                                }
                            }
                            PropertyKey::Index(i) => {
                                if let Some(desc) = obj.get_own_property_descriptor(&key) {
                                    if desc.enumerable() {
                                        names.push(Value::string(JsString::intern(
                                            &i.to_string(),
                                        )));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    let result = GcRef::new(JsObject::array(names.len(), _mm));
                    for (i, name) in names.into_iter().enumerate() {
                        result.set(PropertyKey::Index(i as u32), name);
                    }
                    Ok(Value::array(result))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.values
        obj_ctor.define_property(
            PropertyKey::string("values"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _mm| {
                    let obj = args
                        .first()
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| "Object.values requires an object".to_string())?;
                    let keys = obj.own_keys();
                    let mut values = Vec::new();
                    for key in keys {
                        if let Some(desc) = obj.get_own_property_descriptor(&key) {
                            if desc.enumerable() {
                                if let Some(value) = obj.get(&key) {
                                    values.push(value);
                                }
                            }
                        }
                    }
                    let result = GcRef::new(JsObject::array(values.len(), _mm));
                    for (i, value) in values.into_iter().enumerate() {
                        result.set(PropertyKey::Index(i as u32), value);
                    }
                    Ok(Value::array(result))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.entries
        obj_ctor.define_property(
            PropertyKey::string("entries"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, mm_inner| {
                    let obj = args
                        .first()
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| "Object.entries requires an object".to_string())?;
                    let keys = obj.own_keys();
                    let mut entries = Vec::new();
                    for key in keys {
                        if let Some(desc) = obj.get_own_property_descriptor(&key) {
                            if desc.enumerable() {
                                if let Some(value) = obj.get(&key) {
                                    let key_str = match &key {
                                        PropertyKey::String(s) => Value::string(s.clone()),
                                        PropertyKey::Index(i) => {
                                            Value::string(JsString::intern(&i.to_string()))
                                        }
                                        _ => continue,
                                    };
                                    let entry = GcRef::new(JsObject::array(
                                        2,
                                        mm_inner.clone(),
                                    ));
                                    entry.set(PropertyKey::Index(0), key_str);
                                    entry.set(PropertyKey::Index(1), value);
                                    entries.push(Value::array(entry));
                                }
                            }
                        }
                    }
                    let result =
                        GcRef::new(JsObject::array(entries.len(), mm_inner));
                    for (i, entry) in entries.into_iter().enumerate() {
                        result.set(PropertyKey::Index(i as u32), entry);
                    }
                    Ok(Value::array(result))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.assign
        obj_ctor.define_property(
            PropertyKey::string("assign"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _mm| {
                    let target_val = args
                        .first()
                        .ok_or_else(|| {
                            "Object.assign requires at least one argument".to_string()
                        })?;
                    let target = target_val
                        .as_object()
                        .ok_or_else(|| "Object.assign target must be an object".to_string())?;
                    for source_val in &args[1..] {
                        if source_val.is_null() || source_val.is_undefined() {
                            continue;
                        }
                        if let Some(source) = source_val.as_object() {
                            for key in source.own_keys() {
                                if let Some(desc) = source.get_own_property_descriptor(&key) {
                                    if desc.enumerable() {
                                        if let Some(value) = source.get(&key) {
                                            target.set(key, value);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Ok(target_val.clone())
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.hasOwn
        obj_ctor.define_property(
            PropertyKey::string("hasOwn"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _mm| {
                    let obj = args
                        .first()
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| "Object.hasOwn requires an object".to_string())?;
                    let prop = args.get(1).ok_or_else(|| {
                        "Object.hasOwn requires a property key".to_string()
                    })?;
                    let key = if let Some(s) = prop.as_string() {
                        PropertyKey::String(s)
                    } else if let Some(sym) = prop.as_symbol() {
                        PropertyKey::Symbol(sym.id)
                    } else if let Some(n) = prop.as_number() {
                        if n.fract() == 0.0 && n >= 0.0 && n <= u32::MAX as f64 {
                            PropertyKey::Index(n as u32)
                        } else {
                            PropertyKey::String(JsString::intern(&n.to_string()))
                        }
                    } else {
                        PropertyKey::String(JsString::intern("undefined"))
                    };
                    Ok(Value::boolean(obj.has_own(&key)))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.freeze
        obj_ctor.define_property(
            PropertyKey::string("freeze"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _mm| {
                    let obj_val = args.first().cloned().unwrap_or(Value::undefined());
                    if let Some(obj) = obj_val.as_object() {
                        obj.freeze();
                    }
                    Ok(obj_val)
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.isFrozen
        obj_ctor.define_property(
            PropertyKey::string("isFrozen"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _mm| {
                    let is_frozen = args
                        .first()
                        .and_then(|v| v.as_object())
                        .map(|o| o.is_frozen())
                        .unwrap_or(true);
                    Ok(Value::boolean(is_frozen))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.seal
        obj_ctor.define_property(
            PropertyKey::string("seal"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _mm| {
                    let obj_val = args.first().cloned().unwrap_or(Value::undefined());
                    if let Some(obj) = obj_val.as_object() {
                        obj.seal();
                    }
                    Ok(obj_val)
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.isSealed
        obj_ctor.define_property(
            PropertyKey::string("isSealed"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _mm| {
                    let is_sealed = args
                        .first()
                        .and_then(|v| v.as_object())
                        .map(|o| o.is_sealed())
                        .unwrap_or(true);
                    Ok(Value::boolean(is_sealed))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.preventExtensions
        obj_ctor.define_property(
            PropertyKey::string("preventExtensions"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _mm| {
                    let obj_val = args.first().cloned().unwrap_or(Value::undefined());
                    if let Some(obj) = obj_val.as_object() {
                        obj.prevent_extensions();
                    }
                    Ok(obj_val)
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.isExtensible
        obj_ctor.define_property(
            PropertyKey::string("isExtensible"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _mm| {
                    let is_extensible = args
                        .first()
                        .and_then(|v| v.as_object())
                        .map(|o| o.is_extensible())
                        .unwrap_or(false);
                    Ok(Value::boolean(is_extensible))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.defineProperty
        obj_ctor.define_property(
            PropertyKey::string("defineProperty"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _mm| {
                    let obj_val = args
                        .first()
                        .ok_or_else(|| "Object.defineProperty requires an object".to_string())?;
                    let obj = obj_val.as_object().ok_or_else(|| {
                        "Object.defineProperty first argument must be an object".to_string()
                    })?;
                    let key_val = args
                        .get(1)
                        .ok_or_else(|| {
                            "Object.defineProperty requires a property key".to_string()
                        })?;
                    let descriptor = args
                        .get(2)
                        .ok_or_else(|| {
                            "Object.defineProperty requires a descriptor".to_string()
                        })?;

                    // Convert key
                    let key = if let Some(s) = key_val.as_string() {
                        PropertyKey::String(s)
                    } else if let Some(sym) = key_val.as_symbol() {
                        PropertyKey::Symbol(sym.id)
                    } else if let Some(n) = key_val.as_number() {
                        if n.fract() == 0.0 && n >= 0.0 && n <= u32::MAX as f64 {
                            PropertyKey::Index(n as u32)
                        } else {
                            PropertyKey::String(JsString::intern(&n.to_string()))
                        }
                    } else {
                        PropertyKey::String(JsString::intern("undefined"))
                    };

                    let attr_obj = descriptor.as_object().ok_or_else(|| {
                        "Property descriptor must be an object".to_string()
                    })?;

                    let read_bool = |name: &str, default: bool| -> bool {
                        attr_obj
                            .get(&PropertyKey::from(name))
                            .and_then(|v| v.as_boolean())
                            .unwrap_or(default)
                    };

                    let get = attr_obj.get(&PropertyKey::from("get"));
                    let set = attr_obj.get(&PropertyKey::from("set"));

                    if get.is_some() || set.is_some() {
                        let enumerable = read_bool("enumerable", false);
                        let configurable = read_bool("configurable", false);

                        let existing = obj.get_own_property_descriptor(&key);
                        let (mut existing_get, mut existing_set) = match existing {
                            Some(PropertyDescriptor::Accessor { get, set, .. }) => {
                                (get, set)
                            }
                            _ => (None, None),
                        };
                        let get = get
                            .filter(|v| !v.is_undefined())
                            .or_else(|| existing_get.take());
                        let set = set
                            .filter(|v| !v.is_undefined())
                            .or_else(|| existing_set.take());

                        obj.define_property(
                            key,
                            PropertyDescriptor::Accessor {
                                get,
                                set,
                                attributes: PropertyAttributes {
                                    writable: false,
                                    enumerable,
                                    configurable,
                                },
                            },
                        );
                    } else {
                        let value = attr_obj
                            .get(&PropertyKey::from("value"))
                            .unwrap_or(Value::undefined());
                        let writable = read_bool("writable", false);
                        let enumerable = read_bool("enumerable", false);
                        let configurable = read_bool("configurable", false);

                        obj.define_property(
                            key,
                            PropertyDescriptor::data_with_attrs(
                                value,
                                PropertyAttributes {
                                    writable,
                                    enumerable,
                                    configurable,
                                },
                            ),
                        );
                    }
                    Ok(obj_val.clone())
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.create
        obj_ctor.define_property(
            PropertyKey::string("create"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, mm_inner| {
                    let proto_val = args.first().ok_or_else(|| {
                        "Object.create requires a prototype argument".to_string()
                    })?;
                    let prototype = if proto_val.is_null() {
                        None
                    } else if let Some(proto_obj) = proto_val.as_object() {
                        Some(proto_obj)
                    } else {
                        return Err(
                            VmError::type_error("Object prototype may only be an Object or null")
                        );
                    };
                    let new_obj = GcRef::new(JsObject::new(prototype, mm_inner.clone()));

                    // Handle optional properties object (second argument)
                    if let Some(props_val) = args.get(1) {
                        if !props_val.is_undefined() {
                            let props = props_val.as_object().ok_or_else(|| {
                                "Properties argument must be an object".to_string()
                            })?;
                            for key in props.own_keys() {
                                if let Some(descriptor) = props.get(&key) {
                                    if let Some(attr_obj) = descriptor.as_object() {
                                        let read_bool =
                                            |name: &str, default: bool| -> bool {
                                                attr_obj
                                                    .get(&PropertyKey::from(name))
                                                    .and_then(|v| v.as_boolean())
                                                    .unwrap_or(default)
                                            };
                                        let get =
                                            attr_obj.get(&PropertyKey::from("get"));
                                        let set =
                                            attr_obj.get(&PropertyKey::from("set"));
                                        if get.is_some() || set.is_some() {
                                            let enumerable =
                                                read_bool("enumerable", false);
                                            let configurable =
                                                read_bool("configurable", false);
                                            new_obj.define_property(
                                                key,
                                                PropertyDescriptor::Accessor {
                                                    get: get
                                                        .filter(|v| !v.is_undefined()),
                                                    set: set
                                                        .filter(|v| !v.is_undefined()),
                                                    attributes: PropertyAttributes {
                                                        writable: false,
                                                        enumerable,
                                                        configurable,
                                                    },
                                                },
                                            );
                                        } else {
                                            let value = attr_obj
                                                .get(&PropertyKey::from("value"))
                                                .unwrap_or(Value::undefined());
                                            let writable =
                                                read_bool("writable", false);
                                            let enumerable =
                                                read_bool("enumerable", false);
                                            let configurable =
                                                read_bool("configurable", false);
                                            new_obj.define_property(
                                                key,
                                                PropertyDescriptor::data_with_attrs(
                                                    value,
                                                    PropertyAttributes {
                                                        writable,
                                                        enumerable,
                                                        configurable,
                                                    },
                                                ),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }

                    Ok(Value::object(new_obj))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.is (SameValue algorithm)
        obj_ctor.define_property(
            PropertyKey::string("is"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _mm| {
                    let v1 = args.first().cloned().unwrap_or(Value::undefined());
                    let v2 = args.get(1).cloned().unwrap_or(Value::undefined());
                    let result =
                        if let (Some(n1), Some(n2)) = (v1.as_number(), v2.as_number()) {
                            if n1.is_nan() && n2.is_nan() {
                                true
                            } else if n1 == 0.0 && n2 == 0.0 {
                                (1.0_f64 / n1).is_sign_positive()
                                    == (1.0_f64 / n2).is_sign_positive()
                            } else {
                                n1 == n2
                            }
                        } else if v1.is_undefined() && v2.is_undefined() {
                            true
                        } else if v1.is_null() && v2.is_null() {
                            true
                        } else if let (Some(b1), Some(b2)) =
                            (v1.as_boolean(), v2.as_boolean())
                        {
                            b1 == b2
                        } else if let (Some(s1), Some(s2)) =
                            (v1.as_string(), v2.as_string())
                        {
                            s1.as_str() == s2.as_str()
                        } else if let (Some(sym1), Some(sym2)) =
                            (v1.as_symbol(), v2.as_symbol())
                        {
                            sym1.id == sym2.id
                        } else if let (Some(o1), Some(o2)) =
                            (v1.as_object(), v2.as_object())
                        {
                            o1.as_ptr() == o2.as_ptr()
                        } else {
                            false
                        };
                    Ok(Value::boolean(result))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.getOwnPropertyNames
        obj_ctor.define_property(
            PropertyKey::string("getOwnPropertyNames"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, mm_inner| {
                    let obj = match args.first().and_then(|v| v.as_object()) {
                        Some(o) => o,
                        None => {
                            return Ok(Value::array(GcRef::new(JsObject::array(
                                0, mm_inner,
                            ))));
                        }
                    };
                    let keys = obj.own_keys();
                    let mut names = Vec::new();
                    for key in keys {
                        match key {
                            PropertyKey::String(s) => names.push(Value::string(s)),
                            PropertyKey::Index(i) => {
                                names.push(Value::string(JsString::intern(
                                    &i.to_string(),
                                )));
                            }
                            _ => {} // skip symbols
                        }
                    }
                    let result =
                        GcRef::new(JsObject::array(names.len(), mm_inner));
                    for (i, name) in names.into_iter().enumerate() {
                        result.set(PropertyKey::Index(i as u32), name);
                    }
                    Ok(Value::array(result))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.getOwnPropertyDescriptors
        obj_ctor.define_property(
            PropertyKey::string("getOwnPropertyDescriptors"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, mm_inner| {
                    let obj = match args.first().and_then(|v| v.as_object()) {
                        Some(o) => o,
                        None => {
                            return Ok(Value::object(GcRef::new(JsObject::new(
                                None, mm_inner,
                            ))));
                        }
                    };
                    let result = GcRef::new(JsObject::new(None, mm_inner.clone()));
                    for key in obj.own_keys() {
                        if let Some(desc) = obj.get_own_property_descriptor(&key) {
                            let desc_obj =
                                GcRef::new(JsObject::new(None, mm_inner.clone()));
                            match &desc {
                                PropertyDescriptor::Data { value, attributes } => {
                                    desc_obj.set(
                                        PropertyKey::string("value"),
                                        value.clone(),
                                    );
                                    desc_obj.set(
                                        PropertyKey::string("writable"),
                                        Value::boolean(attributes.writable),
                                    );
                                    desc_obj.set(
                                        PropertyKey::string("enumerable"),
                                        Value::boolean(attributes.enumerable),
                                    );
                                    desc_obj.set(
                                        PropertyKey::string("configurable"),
                                        Value::boolean(attributes.configurable),
                                    );
                                }
                                PropertyDescriptor::Accessor {
                                    get,
                                    set,
                                    attributes,
                                } => {
                                    desc_obj.set(
                                        PropertyKey::string("get"),
                                        get.clone()
                                            .unwrap_or(Value::undefined()),
                                    );
                                    desc_obj.set(
                                        PropertyKey::string("set"),
                                        set.clone()
                                            .unwrap_or(Value::undefined()),
                                    );
                                    desc_obj.set(
                                        PropertyKey::string("enumerable"),
                                        Value::boolean(attributes.enumerable),
                                    );
                                    desc_obj.set(
                                        PropertyKey::string("configurable"),
                                        Value::boolean(attributes.configurable),
                                    );
                                }
                                PropertyDescriptor::Deleted => {}
                            }
                            result.set(key, Value::object(desc_obj));
                        }
                    }
                    Ok(Value::object(result))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.defineProperties
        obj_ctor.define_property(
            PropertyKey::string("defineProperties"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _mm| {
                    let obj_val = args
                        .first()
                        .ok_or_else(|| {
                            "Object.defineProperties requires an object".to_string()
                        })?;
                    let obj = obj_val.as_object().ok_or_else(|| {
                        "Object.defineProperties first argument must be an object"
                            .to_string()
                    })?;
                    let props_val = args.get(1).ok_or_else(|| {
                        "Object.defineProperties requires properties".to_string()
                    })?;
                    let props = props_val.as_object().ok_or_else(|| {
                        "Object.defineProperties second argument must be an object"
                            .to_string()
                    })?;

                    for key in props.own_keys() {
                        if let Some(descriptor) = props.get(&key) {
                            if let Some(attr_obj) = descriptor.as_object() {
                                let read_bool =
                                    |name: &str, default: bool| -> bool {
                                        attr_obj
                                            .get(&PropertyKey::from(name))
                                            .and_then(|v| v.as_boolean())
                                            .unwrap_or(default)
                                    };
                                let get = attr_obj.get(&PropertyKey::from("get"));
                                let set = attr_obj.get(&PropertyKey::from("set"));
                                if get.is_some() || set.is_some() {
                                    let enumerable =
                                        read_bool("enumerable", false);
                                    let configurable =
                                        read_bool("configurable", false);
                                    obj.define_property(
                                        key,
                                        PropertyDescriptor::Accessor {
                                            get: get
                                                .filter(|v| !v.is_undefined()),
                                            set: set
                                                .filter(|v| !v.is_undefined()),
                                            attributes: PropertyAttributes {
                                                writable: false,
                                                enumerable,
                                                configurable,
                                            },
                                        },
                                    );
                                } else {
                                    let value = attr_obj
                                        .get(&PropertyKey::from("value"))
                                        .unwrap_or(Value::undefined());
                                    let writable = read_bool("writable", false);
                                    let enumerable =
                                        read_bool("enumerable", false);
                                    let configurable =
                                        read_bool("configurable", false);
                                    obj.define_property(
                                        key,
                                        PropertyDescriptor::data_with_attrs(
                                            value,
                                            PropertyAttributes {
                                                writable,
                                                enumerable,
                                                configurable,
                                            },
                                        ),
                                    );
                                }
                            }
                        }
                    }
                    Ok(obj_val.clone())
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Object.fromEntries
        obj_ctor.define_property(
            PropertyKey::string("fromEntries"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, mm_inner| {
                    let iterable = args.first().ok_or_else(|| {
                        "Object.fromEntries requires an iterable".to_string()
                    })?;
                    let iter_obj = iterable.as_object().ok_or_else(|| {
                        "Object.fromEntries argument must be iterable".to_string()
                    })?;
                    let result = GcRef::new(JsObject::new(None, mm_inner));

                    // Support array-like iterables (check length property)
                    if let Some(len_val) =
                        iter_obj.get(&PropertyKey::String(JsString::intern("length")))
                    {
                        if let Some(len) = len_val.as_number() {
                            for i in 0..(len as u32) {
                                if let Some(entry) =
                                    iter_obj.get(&PropertyKey::Index(i))
                                {
                                    if let Some(entry_obj) = entry.as_object() {
                                        let key = entry_obj
                                            .get(&PropertyKey::Index(0))
                                            .unwrap_or(Value::undefined());
                                        let value = entry_obj
                                            .get(&PropertyKey::Index(1))
                                            .unwrap_or(Value::undefined());
                                        let pk = if let Some(s) = key.as_string() {
                                            PropertyKey::String(s)
                                        } else if let Some(n) = key.as_number() {
                                            PropertyKey::String(JsString::intern(
                                                &n.to_string(),
                                            ))
                                        } else {
                                            PropertyKey::String(JsString::intern(
                                                "undefined",
                                            ))
                                        };
                                        result.set(pk, value);
                                    }
                                }
                            }
                        }
                    }
                    Ok(Value::object(result))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // ====================================================================

        // ===================================================================
        // String.prototype methods (extracted to intrinsics_impl/string.rs)
        // ===================================================================
        crate::intrinsics_impl::string::init_string_prototype(self.string_prototype, fn_proto, mm);


        // ====================================================================

        // ===================================================================
        // Number.prototype methods (extracted to intrinsics_impl/number.rs)
        // ===================================================================
        crate::intrinsics_impl::number::init_number_prototype(self.number_prototype, fn_proto, mm);

        // ===================================================================
        // Boolean.prototype methods (extracted to intrinsics_impl/boolean.rs)
        // ===================================================================
        crate::intrinsics_impl::boolean::init_boolean_prototype(self.boolean_prototype, fn_proto, mm);


        // ===================================================================
        // Date.prototype methods (extracted to intrinsics_impl/date.rs)
        // ===================================================================
        crate::intrinsics_impl::date::init_date_prototype(self.date_prototype, fn_proto, mm);

        // ====================================================================
        // Iterator prototype: [Symbol.iterator]() { return this; }
        // ====================================================================
        if let Some(sym) = self.symbol_iterator.as_symbol() {
            self.iterator_prototype.define_property(
                PropertyKey::Symbol(sym.id),
                PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                    |this_val, _args, _mm| Ok(this_val.clone()),
                    mm.clone(),
                    fn_proto,
                )),
            );
        }

        // ====================================================================

        // ===================================================================
        // Array.prototype methods (extracted to intrinsics_impl/array.rs)
        // ===================================================================
        crate::intrinsics_impl::array::init_array_prototype(
            self.array_prototype,
            fn_proto,
            mm,
            self.iterator_prototype,
            well_known::ITERATOR,
        );

        // ===================================================================
        // Map/Set/WeakMap/WeakSet prototype methods (extracted to intrinsics_impl/map_set.rs)
        // ===================================================================
        crate::intrinsics_impl::map_set::init_map_prototype(self.map_prototype, fn_proto, mm);
        crate::intrinsics_impl::map_set::init_set_prototype(self.set_prototype, fn_proto, mm);
        crate::intrinsics_impl::map_set::init_weak_map_prototype(self.weak_map_prototype, fn_proto, mm);
        crate::intrinsics_impl::map_set::init_weak_set_prototype(self.weak_set_prototype, fn_proto, mm);

        // ===================================================================
        // RegExp.prototype methods (extracted to intrinsics_impl/regexp.rs)
        // ===================================================================
        crate::intrinsics_impl::regexp::init_regexp_prototype(self.regexp_prototype, fn_proto, mm);

        // ===================================================================
        // Promise.prototype methods (extracted to intrinsics_impl/promise.rs)
        // ===================================================================
        crate::intrinsics_impl::promise::init_promise_prototype(self.promise_prototype, fn_proto, mm);
    }

    /// Install intrinsic constructors on the global object.
    ///
    /// This creates constructor Values (native functions) backed by the intrinsic
    /// objects and installs them as global properties. Call after `init_core()`.
    pub fn install_on_global(&self, global: GcRef<JsObject>, mm: &Arc<MemoryManager>) {
        use crate::object::{PropertyAttributes, PropertyDescriptor, PropertyKey};
        use crate::string::JsString;

        let fn_proto = self.function_prototype;

        // Helper: install a constructor+prototype pair on the global
        let install = |name: &str,
                       ctor_obj: GcRef<JsObject>,
                       proto: GcRef<JsObject>,
                       ctor_fn: Option<
            Box<
                dyn Fn(&Value, &[Value], Arc<MemoryManager>) -> Result<Value, VmError>
                    + Send
                    + Sync,
            >,
        >| {
            // Wire constructor.prototype = prototype
            ctor_obj.define_property(
                PropertyKey::string("prototype"),
                PropertyDescriptor::data_with_attrs(
                    Value::object(proto),
                    PropertyAttributes {
                        writable: false,
                        enumerable: false,
                        configurable: false,
                    },
                ),
            );

            // Create constructor Value
            let ctor_value = if let Some(f) = ctor_fn {
                Value::native_function_with_proto_and_object(
                    Arc::from(f),
                    mm.clone(),
                    fn_proto,
                    ctor_obj,
                )
            } else {
                Value::object(ctor_obj)
            };

            // Wire prototype.constructor = ctor
            proto.define_property(
                PropertyKey::string("constructor"),
                PropertyDescriptor::data_with_attrs(
                    ctor_value.clone(),
                    PropertyAttributes::constructor_link(),
                ),
            );

            // Set name and length on constructor
            if let Some(obj) = ctor_value.as_object() {
                obj.define_property(
                    PropertyKey::string("name"),
                    PropertyDescriptor::function_length(Value::string(JsString::intern(name))),
                );
                obj.define_property(
                    PropertyKey::string("length"),
                    PropertyDescriptor::function_length(Value::number(1.0)),
                );
            }

            // Install on global as non-enumerable (spec behavior)
            global.define_property(
                PropertyKey::string(name),
                PropertyDescriptor::data_with_attrs(ctor_value, PropertyAttributes::builtin_method()),
            );
        };

        // ====================================================================
        // Core constructors
        // ====================================================================
        let object_ctor_fn: Box<
            dyn Fn(&Value, &[Value], Arc<MemoryManager>) -> Result<Value, VmError> + Send + Sync,
        > = Box::new(|_this, args, _mm_inner| {
            // When called with an object argument, return it directly
            if let Some(arg) = args.first() {
                if arg.is_object() {
                    return Ok(arg.clone());
                }
            }
            // Return undefined so Construct handler uses new_obj_value
            // (which has Object.prototype as [[Prototype]])
            Ok(Value::undefined())
        });
        install(
            "Object",
            self.object_constructor,
            self.object_prototype,
            Some(object_ctor_fn),
        );
        install("Function", self.function_constructor, self.function_prototype, None);

        // Register global aliases for interpreter interception
        // The interpreter checks for these globals to detect and intercept
        // Function.prototype.call/apply (see interpreter.rs:5647, 5651)
        if let Some(call_fn) = self.function_prototype.get(&PropertyKey::string("call")) {
            global.set(PropertyKey::string("__Function_call"), call_fn);
        }
        if let Some(apply_fn) = self.function_prototype.get(&PropertyKey::string("apply")) {
            global.set(PropertyKey::string("__Function_apply"), apply_fn);
        }

        // ====================================================================
        // Primitive wrapper constructors
        // ====================================================================

        // For constructors that need actual implementations, we allocate fresh
        // constructor objects (since intrinsics only pre-allocated prototypes).
        // The prototype still comes from intrinsics with correct [[Prototype]] chain.
        let alloc_ctor = || GcRef::new(JsObject::new(Some(fn_proto), mm.clone()));

        // String
        let string_ctor = alloc_ctor();
        let string_ctor_fn: Box<
            dyn Fn(&Value, &[Value], Arc<MemoryManager>) -> Result<Value, VmError> + Send + Sync,
        > = Box::new(|this, args, _mm| {
            let s = if let Some(arg) = args.first() {
                crate::globals::to_string(arg)
            } else {
                String::new()
            };
            let str_val = Value::string(JsString::intern(&s));
            // When called as constructor (new String("...")), `this` is an object.
            // Store the primitive value so String.prototype methods can retrieve it.
            if let Some(obj) = this.as_object() {
                obj.set(
                    PropertyKey::string("__primitiveValue__"),
                    str_val.clone(),
                );
            }
            Ok(str_val)
        });
        install("String", string_ctor, self.string_prototype, Some(string_ctor_fn));

        // String.fromCharCode(...codeUnits)
        string_ctor.define_property(
            PropertyKey::string("fromCharCode"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _mm| {
                    let mut result = String::new();
                    for arg in args {
                        // Per ES2023 §22.1.2.1: ToUint16(ToNumber(arg))
                        let n = if let Some(n) = arg.as_number() {
                            n
                        } else if let Some(i) = arg.as_int32() {
                            i as f64
                        } else if let Some(s) = arg.as_string() {
                            let trimmed = s.as_str().trim();
                            if trimmed.is_empty() {
                                0.0
                            } else {
                                trimmed.parse::<f64>().unwrap_or(f64::NAN)
                            }
                        } else if let Some(b) = arg.as_boolean() {
                            if b { 1.0 } else { 0.0 }
                        } else if arg.is_null() {
                            0.0
                        } else {
                            f64::NAN
                        };
                        let code = if n.is_nan() || n.is_infinite() {
                            0u16
                        } else {
                            (n.trunc() as i64 as u32 & 0xFFFF) as u16
                        };
                        if let Some(ch) = char::from_u32(code as u32) {
                            result.push(ch);
                        }
                    }
                    Ok(Value::string(JsString::intern(&result)))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.fromCodePoint(...codePoints)
        string_ctor.define_property(
            PropertyKey::string("fromCodePoint"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _mm| {
                    let mut result = String::new();
                    for arg in args {
                        let code = if let Some(n) = arg.as_number() {
                            n as u32
                        } else if let Some(i) = arg.as_int32() {
                            i as u32
                        } else {
                            0
                        };
                        if code > 0x10FFFF {
                            return Err(VmError::type_error(format!("Invalid code point: {}", code)));
                        }
                        if let Some(ch) = char::from_u32(code) {
                            result.push(ch);
                        } else {
                            return Err(VmError::type_error(format!("Invalid code point: {}", code)));
                        }
                    }
                    Ok(Value::string(JsString::intern(&result)))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Number
        let number_ctor = alloc_ctor();
        let number_ctor_fn: Box<
            dyn Fn(&Value, &[Value], Arc<MemoryManager>) -> Result<Value, VmError> + Send + Sync,
        > = Box::new(|_this, args, _mm| {
            let n = if let Some(arg) = args.first() {
                crate::globals::to_number(arg)
            } else {
                0.0
            };
            Ok(Value::number(n))
        });
        install("Number", number_ctor, self.number_prototype, Some(number_ctor_fn));
        crate::intrinsics_impl::number::install_number_statics(number_ctor, fn_proto, mm);

        // Boolean
        let boolean_ctor = alloc_ctor();
        let boolean_ctor_fn = crate::intrinsics_impl::boolean::create_boolean_constructor();
        install("Boolean", boolean_ctor, self.boolean_prototype, Some(boolean_ctor_fn));

        // Symbol
        let symbol_ctor = alloc_ctor();
        install("Symbol", symbol_ctor, self.symbol_prototype, None);

        // BigInt
        let bigint_ctor = alloc_ctor();
        install("BigInt", bigint_ctor, self.bigint_prototype, None);

        // ====================================================================
        // Collection constructors
        // ====================================================================
        let array_ctor = alloc_ctor();
        let array_ctor_fn: Box<
            dyn Fn(&Value, &[Value], Arc<MemoryManager>) -> Result<Value, VmError> + Send + Sync,
        > = Box::new(|_this, args, mm_inner| {
            if args.len() == 1 {
                if let Some(n) = args[0].as_number() {
                    let len = n as u32;
                    if (len as f64) != n || n < 0.0 {
                        return Err(VmError::type_error("Invalid array length"));
                    }
                    let arr = GcRef::new(JsObject::array(len as usize, mm_inner));
                    return Ok(Value::object(arr));
                }
            }
            // Array(...items) — populate the array
            let arr = GcRef::new(JsObject::array(args.len(), mm_inner));
            for (i, arg) in args.iter().enumerate() {
                arr.set(PropertyKey::index(i as u32), arg.clone());
            }
            Ok(Value::object(arr))
        });
        install("Array", array_ctor, self.array_prototype, Some(array_ctor_fn));
        crate::intrinsics_impl::array::install_array_statics(array_ctor, fn_proto, mm);

        let map_ctor = alloc_ctor();
        let map_ctor_fn = crate::intrinsics_impl::map_set::create_map_constructor();
        install("Map", map_ctor, self.map_prototype, Some(map_ctor_fn));

        let set_ctor = alloc_ctor();
        let set_ctor_fn = crate::intrinsics_impl::map_set::create_set_constructor();
        install("Set", set_ctor, self.set_prototype, Some(set_ctor_fn));

        let weak_map_ctor = alloc_ctor();
        let weak_map_ctor_fn = crate::intrinsics_impl::map_set::create_weak_map_constructor();
        install("WeakMap", weak_map_ctor, self.weak_map_prototype, Some(weak_map_ctor_fn));

        let weak_set_ctor = alloc_ctor();
        let weak_set_ctor_fn = crate::intrinsics_impl::map_set::create_weak_set_constructor();
        install("WeakSet", weak_set_ctor, self.weak_set_prototype, Some(weak_set_ctor_fn));

        // ====================================================================
        // Error constructors
        // ====================================================================
        // Helper to create error constructor functions
        let make_error_ctor = |error_name: &'static str| -> Box<
            dyn Fn(&Value, &[Value], Arc<MemoryManager>) -> Result<Value, VmError> + Send + Sync,
        > {
            Box::new(move |this, args, _mm_inner| {
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
        };

        let error_ctor = alloc_ctor();
        install("Error", error_ctor, self.error_prototype, Some(make_error_ctor("Error")));

        let type_error_ctor = alloc_ctor();
        install("TypeError", type_error_ctor, self.type_error_prototype, Some(make_error_ctor("TypeError")));

        let range_error_ctor = alloc_ctor();
        install("RangeError", range_error_ctor, self.range_error_prototype, Some(make_error_ctor("RangeError")));

        let reference_error_ctor = alloc_ctor();
        install("ReferenceError", reference_error_ctor, self.reference_error_prototype, Some(make_error_ctor("ReferenceError")));

        let syntax_error_ctor = alloc_ctor();
        install("SyntaxError", syntax_error_ctor, self.syntax_error_prototype, Some(make_error_ctor("SyntaxError")));

        let uri_error_ctor = alloc_ctor();
        install("URIError", uri_error_ctor, self.uri_error_prototype, Some(make_error_ctor("URIError")));

        let eval_error_ctor = alloc_ctor();
        install("EvalError", eval_error_ctor, self.eval_error_prototype, Some(make_error_ctor("EvalError")));

        // ====================================================================
        // Other builtins
        // ====================================================================
        let promise_ctor = alloc_ctor();
        install("Promise", promise_ctor, self.promise_prototype, None);
        crate::intrinsics_impl::promise::install_promise_statics(promise_ctor, fn_proto, mm);

        let regexp_ctor = alloc_ctor();
        let regexp_ctor_fn = crate::intrinsics_impl::regexp::create_regexp_constructor(self.regexp_prototype);
        install("RegExp", regexp_ctor, self.regexp_prototype, Some(regexp_ctor_fn));

        // RegExp.escape (ES2026 §22.2.4.1)
        regexp_ctor.define_property(
            PropertyKey::string("escape"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _mm| {
                    let s = args
                        .first()
                        .and_then(|v| v.as_string())
                        .ok_or_else(|| VmError::type_error("RegExp.escape requires a string argument"))?;
                    Ok(Value::string(JsString::intern(&regress::escape(s.as_str()))))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        let date_ctor = alloc_ctor();
        let date_ctor_fn: Box<
            dyn Fn(&Value, &[Value], Arc<MemoryManager>) -> Result<Value, VmError> + Send + Sync,
        > = Box::new(|this, args, _mm_inner| {
            use std::time::{SystemTime, UNIX_EPOCH};
            let timestamp = if args.is_empty() {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as f64)
                    .unwrap_or(0.0)
            } else if let Some(n) = args[0].as_number() {
                n
            } else if args[0].as_string().is_some() {
                f64::NAN // TODO: proper date parsing
            } else {
                f64::NAN
            };
            // Set timestamp on `this` (created by Construct with Date.prototype)
            if let Some(obj) = this.as_object() {
                obj.set(
                    PropertyKey::string("__timestamp"),
                    Value::number(timestamp),
                );
            }
            Ok(Value::undefined())
        });
        install("Date", date_ctor, self.date_prototype, Some(date_ctor_fn));

        // Date.now() - returns current timestamp in milliseconds
        date_ctor.define_property(
            PropertyKey::string("now"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, _args, _mm| {
                    use std::time::{SystemTime, UNIX_EPOCH};
                    let timestamp = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_millis() as f64)
                        .unwrap_or(0.0);
                    Ok(Value::number(timestamp))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Date.parse(dateString) - parses ISO 8601 date strings
        date_ctor.define_property(
            PropertyKey::string("parse"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _mm| {
                    use chrono::{DateTime, NaiveDate, NaiveDateTime};

                    let date_str = args
                        .first()
                        .and_then(|v| v.as_string())
                        .ok_or("Date.parse requires a string argument")?;

                    let s = date_str.as_str();

                    // Try parsing as RFC3339/ISO8601 with timezone
                    let parsed = DateTime::parse_from_rfc3339(s)
                        .map(|dt| dt.timestamp_millis() as f64)
                        .or_else(|_| {
                            // Try parsing as naive datetime
                            NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
                                .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f"))
                                .map(|dt| dt.and_utc().timestamp_millis() as f64)
                        })
                        .or_else(|_| {
                            // Try parsing as date only
                            NaiveDate::parse_from_str(s, "%Y-%m-%d")
                                .map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis() as f64)
                        });

                    match parsed {
                        Ok(ts) => Ok(Value::number(ts)),
                        Err(_) => Ok(Value::number(f64::NAN)),
                    }
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Date.UTC(year, month, day, hour, min, sec, ms) - constructs UTC timestamp
        date_ctor.define_property(
            PropertyKey::string("UTC"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _mm| {
                    use chrono::NaiveDate;

                    if args.is_empty() {
                        return Ok(Value::number(f64::NAN));
                    }

                    let year = args.get(0).and_then(|v| v.as_int32()).unwrap_or(1970);
                    let month = args.get(1).and_then(|v| v.as_int32()).unwrap_or(0) + 1; // JS months are 0-indexed
                    let day = args.get(2).and_then(|v| v.as_int32()).unwrap_or(1);
                    let hour = args.get(3).and_then(|v| v.as_int32()).unwrap_or(0);
                    let minute = args.get(4).and_then(|v| v.as_int32()).unwrap_or(0);
                    let second = args.get(5).and_then(|v| v.as_int32()).unwrap_or(0);
                    let ms = args.get(6).and_then(|v| v.as_int32()).unwrap_or(0);

                    let date = NaiveDate::from_ymd_opt(year, month as u32, day as u32)
                        .and_then(|d| d.and_hms_milli_opt(hour as u32, minute as u32, second as u32, ms as u32));

                    match date {
                        Some(dt) => {
                            let timestamp = dt.and_utc().timestamp_millis() as f64;
                            Ok(Value::number(timestamp))
                        }
                        None => Ok(Value::number(f64::NAN)),
                    }
                },
                mm.clone(),
                fn_proto,
            )),
        );

        let array_buffer_ctor = alloc_ctor();
        install("ArrayBuffer", array_buffer_ctor, self.array_buffer_prototype, None);

        let data_view_ctor = alloc_ctor();
        install("DataView", data_view_ctor, self.data_view_prototype, None);

        // ====================================================================
        // Non-constructor namespace objects
        // Math: extracted to intrinsics_impl/math.rs
        // Reflect, JSON: TODO - still need to be extracted from builtins.js
        // ====================================================================

        // Install well-known symbols on Symbol constructor
        if let Some(sym_ctor_obj) = global
            .get(&PropertyKey::string("Symbol"))
            .and_then(|v| v.as_object())
        {
            let sym_attrs = PropertyAttributes::permanent();
            let install_sym = |name: &str, sym_val: &Value| {
                sym_ctor_obj.define_property(
                    PropertyKey::string(name),
                    PropertyDescriptor::data_with_attrs(sym_val.clone(), sym_attrs),
                );
            };
            install_sym("iterator", &self.symbol_iterator);
            install_sym("asyncIterator", &self.symbol_async_iterator);
            install_sym("toStringTag", &self.symbol_to_string_tag);
            install_sym("hasInstance", &self.symbol_has_instance);
            install_sym("toPrimitive", &self.symbol_to_primitive);
            install_sym("isConcatSpreadable", &self.symbol_is_concat_spreadable);
            install_sym("match", &self.symbol_match);
            install_sym("matchAll", &self.symbol_match_all);
            install_sym("replace", &self.symbol_replace);
            install_sym("search", &self.symbol_search);
            install_sym("split", &self.symbol_split);
            install_sym("species", &self.symbol_species);
            install_sym("unscopables", &self.symbol_unscopables);
        }

        // ====================================================================
        // Temporal namespace (extracted to intrinsics_impl/temporal.rs)
        // ====================================================================
        crate::intrinsics_impl::temporal::install_temporal_namespace(global, mm);

        // ====================================================================
        // Math namespace (extracted to intrinsics_impl/math.rs)
        // All Math methods are implemented natively in Rust using std::f64
        // ====================================================================
        crate::intrinsics_impl::math::install_math_namespace(global, mm);

        // ====================================================================
        // Reflect namespace (extracted to intrinsics_impl/reflect.rs)
        // All Reflect methods are implemented natively as __Reflect_* ops
        // and registered as globals. This module creates the Reflect namespace.
        //
        // NOTE: Reflect.apply and Reflect.construct require function invocation
        // support and will be added in a future update.
        // ====================================================================
        crate::intrinsics_impl::reflect::install_reflect_namespace(global, mm);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{PropertyDescriptor, PropertyKey};

    #[test]
    fn test_intrinsics_allocate() {
        let mm = Arc::new(MemoryManager::test());
        let fn_proto = GcRef::new(JsObject::new(None, mm.clone()));
        let intrinsics = Intrinsics::allocate(&mm, fn_proto);

        // All well-known symbols should be symbols
        assert!(intrinsics.symbol_iterator.is_symbol());
        assert!(intrinsics.symbol_async_iterator.is_symbol());
        assert!(intrinsics.symbol_to_string_tag.is_symbol());
        assert!(intrinsics.symbol_has_instance.is_symbol());
        assert!(intrinsics.symbol_to_primitive.is_symbol());
        assert!(intrinsics.symbol_species.is_symbol());
    }

    #[test]
    fn test_prototype_chain_wiring() {
        let mm = Arc::new(MemoryManager::test());
        let fn_proto = GcRef::new(JsObject::new(None, mm.clone()));
        let intrinsics = Intrinsics::allocate(&mm, fn_proto);
        intrinsics.wire_prototype_chains();

        // Object.prototype.__proto__ === null
        assert!(intrinsics.object_prototype.prototype().is_none());

        // Function.prototype.__proto__ === Object.prototype
        let fp_proto = intrinsics.function_prototype.prototype();
        assert!(fp_proto.is_some());
        assert_eq!(fp_proto.unwrap().as_ptr(), intrinsics.object_prototype.as_ptr());

        // Array.prototype.__proto__ === Object.prototype
        let ap_proto = intrinsics.array_prototype.prototype();
        assert!(ap_proto.is_some());
        assert_eq!(ap_proto.unwrap().as_ptr(), intrinsics.object_prototype.as_ptr());

        // TypeError.prototype.__proto__ === Error.prototype
        let tep_proto = intrinsics.type_error_prototype.prototype();
        assert!(tep_proto.is_some());
        assert_eq!(tep_proto.unwrap().as_ptr(), intrinsics.error_prototype.as_ptr());

        // RangeError.prototype.__proto__ === Error.prototype
        let rep_proto = intrinsics.range_error_prototype.prototype();
        assert!(rep_proto.is_some());
        assert_eq!(rep_proto.unwrap().as_ptr(), intrinsics.error_prototype.as_ptr());

        // Error.prototype.__proto__ === Object.prototype
        let ep_proto = intrinsics.error_prototype.prototype();
        assert!(ep_proto.is_some());
        assert_eq!(ep_proto.unwrap().as_ptr(), intrinsics.object_prototype.as_ptr());
    }

    #[test]
    fn test_init_core_builtin_methods() {
        let mm = Arc::new(MemoryManager::test());
        let fn_proto = GcRef::new(JsObject::new(None, mm.clone()));
        let intrinsics = Intrinsics::allocate(&mm, fn_proto);
        intrinsics.wire_prototype_chains();
        intrinsics.init_core(&mm);

        // Array.prototype should have map, filter, forEach, etc.
        assert!(intrinsics.array_prototype.has(&PropertyKey::string("map")));
        assert!(intrinsics.array_prototype.has(&PropertyKey::string("filter")));
        assert!(intrinsics.array_prototype.has(&PropertyKey::string("forEach")));
        assert!(intrinsics.array_prototype.has(&PropertyKey::string("find")));
        assert!(intrinsics.array_prototype.has(&PropertyKey::string("reduce")));
        assert!(intrinsics.array_prototype.has(&PropertyKey::string("values")));
        assert!(intrinsics.array_prototype.has(&PropertyKey::string("keys")));
        assert!(intrinsics.array_prototype.has(&PropertyKey::string("entries")));
        assert!(intrinsics.array_prototype.has(&PropertyKey::string("sort")));

        // Array.prototype[Symbol.iterator] should exist
        assert!(intrinsics.array_prototype.has(&PropertyKey::Symbol(well_known::ITERATOR)));

        // Builtin methods should be non-enumerable
        let map_desc = intrinsics.array_prototype.get_own_property_descriptor(&PropertyKey::string("map"));
        assert!(map_desc.is_some(), "Array.prototype.map descriptor should exist");
        if let Some(PropertyDescriptor::Data { attributes, .. }) = map_desc {
            assert!(!attributes.enumerable, "Array.prototype.map should be non-enumerable");
            assert!(attributes.writable, "Array.prototype.map should be writable");
            assert!(attributes.configurable, "Array.prototype.map should be configurable");
        }

        // String.prototype should have methods
        assert!(intrinsics.string_prototype.has(&PropertyKey::string("charAt")));
        assert!(intrinsics.string_prototype.has(&PropertyKey::string("slice")));

        // Number.prototype should have methods
        assert!(intrinsics.number_prototype.has(&PropertyKey::string("toFixed")));
        assert!(intrinsics.number_prototype.has(&PropertyKey::string("toString")));
    }
}
