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

    /// Create a GcRef<crate::value::Symbol> for a well-known symbol.
    /// These are compared by ID, so multiple GcRef instances with the same ID are equal.
    pub fn symbol_ref(id: u64, desc: &'static str) -> crate::gc::GcRef<crate::value::Symbol> {
        crate::gc::GcRef::new(crate::value::Symbol {
            description: Some(desc.to_string()),
            id,
        })
    }

    /// Get Symbol.iterator as GcRef<crate::value::Symbol>
    pub fn iterator_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(ITERATOR, "Symbol.iterator")
    }

    /// Get Symbol.asyncIterator as GcRef<crate::value::Symbol>
    pub fn async_iterator_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(ASYNC_ITERATOR, "Symbol.asyncIterator")
    }

    /// Get Symbol.toStringTag as GcRef<crate::value::Symbol>
    pub fn to_string_tag_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(TO_STRING_TAG, "Symbol.toStringTag")
    }

    /// Get Symbol.hasInstance as GcRef<crate::value::Symbol>
    pub fn has_instance_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(HAS_INSTANCE, "Symbol.hasInstance")
    }

    /// Get Symbol.toPrimitive as GcRef<crate::value::Symbol>
    pub fn to_primitive_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(TO_PRIMITIVE, "Symbol.toPrimitive")
    }

    /// Get Symbol.isConcatSpreadable as GcRef<crate::value::Symbol>
    pub fn is_concat_spreadable_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(IS_CONCAT_SPREADABLE, "Symbol.isConcatSpreadable")
    }

    /// Get Symbol.match as GcRef<crate::value::Symbol>
    pub fn match_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(MATCH, "Symbol.match")
    }

    /// Get Symbol.matchAll as GcRef<crate::value::Symbol>
    pub fn match_all_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(MATCH_ALL, "Symbol.matchAll")
    }

    /// Get Symbol.replace as GcRef<crate::value::Symbol>
    pub fn replace_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(REPLACE, "Symbol.replace")
    }

    /// Get Symbol.search as GcRef<crate::value::Symbol>
    pub fn search_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(SEARCH, "Symbol.search")
    }

    /// Get Symbol.split as GcRef<crate::value::Symbol>
    pub fn split_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(SPLIT, "Symbol.split")
    }

    /// Get Symbol.species as GcRef<crate::value::Symbol>
    pub fn species_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(SPECIES, "Symbol.species")
    }

    /// Get Symbol.unscopables as GcRef<crate::value::Symbol>
    pub fn unscopables_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(UNSCOPABLES, "Symbol.unscopables")
    }
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
    /// `%GeneratorFunction.prototype%` — prototype of generator functions
    pub generator_function_prototype: GcRef<JsObject>,
    /// `%AsyncFunction.prototype%` — prototype of async functions
    pub async_function_prototype: GcRef<JsObject>,
    /// `%AsyncGeneratorFunction.prototype%` — prototype of async generator functions
    pub async_generator_function_prototype: GcRef<JsObject>,

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
    // Generator prototypes
    // ========================================================================
    /// `%GeneratorPrototype%` — ES2026 §27.5.1
    pub generator_prototype: GcRef<JsObject>,
    /// `%AsyncGeneratorPrototype%` — ES2026 §27.6.1
    pub async_generator_prototype: GcRef<JsObject>,

    // ========================================================================
    // TypedArray prototypes
    // ========================================================================
    /// `%TypedArray%.prototype` — ES2026 §22.2.3 (common prototype for all typed arrays)
    pub typed_array_prototype: GcRef<JsObject>,
    /// `Int8Array.prototype`
    pub int8_array_prototype: GcRef<JsObject>,
    /// `Uint8Array.prototype`
    pub uint8_array_prototype: GcRef<JsObject>,
    /// `Uint8ClampedArray.prototype`
    pub uint8_clamped_array_prototype: GcRef<JsObject>,
    /// `Int16Array.prototype`
    pub int16_array_prototype: GcRef<JsObject>,
    /// `Uint16Array.prototype`
    pub uint16_array_prototype: GcRef<JsObject>,
    /// `Int32Array.prototype`
    pub int32_array_prototype: GcRef<JsObject>,
    /// `Uint32Array.prototype`
    pub uint32_array_prototype: GcRef<JsObject>,
    /// `Float32Array.prototype`
    pub float32_array_prototype: GcRef<JsObject>,
    /// `Float64Array.prototype`
    pub float64_array_prototype: GcRef<JsObject>,
    /// `BigInt64Array.prototype`
    pub bigint64_array_prototype: GcRef<JsObject>,
    /// `BigUint64Array.prototype`
    pub biguint64_array_prototype: GcRef<JsObject>,

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
    /// Resolve the intrinsic prototype for a builtin tag.
    pub fn prototype_for_builtin_tag(&self, tag: &str) -> Option<GcRef<JsObject>> {
        match tag {
            "Object" => Some(self.object_prototype),
            "Function" => Some(self.function_prototype),
            "GeneratorFunction" => Some(self.generator_function_prototype),
            "AsyncFunction" => Some(self.async_function_prototype),
            "AsyncGeneratorFunction" => Some(self.async_generator_function_prototype),
            "Array" => Some(self.array_prototype),
            "Map" => Some(self.map_prototype),
            "Set" => Some(self.set_prototype),
            "WeakMap" => Some(self.weak_map_prototype),
            "WeakSet" => Some(self.weak_set_prototype),
            "Promise" => Some(self.promise_prototype),
            "RegExp" => Some(self.regexp_prototype),
            "Date" => Some(self.date_prototype),
            "ArrayBuffer" => Some(self.array_buffer_prototype),
            "DataView" => Some(self.data_view_prototype),
            "Error" => Some(self.error_prototype),
            "TypeError" => Some(self.type_error_prototype),
            "RangeError" => Some(self.range_error_prototype),
            "ReferenceError" => Some(self.reference_error_prototype),
            "SyntaxError" => Some(self.syntax_error_prototype),
            "URIError" => Some(self.uri_error_prototype),
            "EvalError" => Some(self.eval_error_prototype),
            "String" => Some(self.string_prototype),
            "Number" => Some(self.number_prototype),
            "Boolean" => Some(self.boolean_prototype),
            "Symbol" => Some(self.symbol_prototype),
            "BigInt" => Some(self.bigint_prototype),
            "Int8Array" => Some(self.int8_array_prototype),
            "Uint8Array" => Some(self.uint8_array_prototype),
            "Uint8ClampedArray" => Some(self.uint8_clamped_array_prototype),
            "Int16Array" => Some(self.int16_array_prototype),
            "Uint16Array" => Some(self.uint16_array_prototype),
            "Int32Array" => Some(self.int32_array_prototype),
            "Uint32Array" => Some(self.uint32_array_prototype),
            "Float32Array" => Some(self.float32_array_prototype),
            "Float64Array" => Some(self.float64_array_prototype),
            "BigInt64Array" => Some(self.bigint64_array_prototype),
            "BigUint64Array" => Some(self.biguint64_array_prototype),
            _ => None,
        }
    }

    /// Create a new `Intrinsics` with all objects allocated but NOT yet initialized.
    ///
    /// This is Stage 1 of the two-stage initialization. Call `init()` after
    /// this to populate properties and wire prototype chains (Stage 2).
    ///
    /// `fn_proto` is the pre-existing intrinsic `%Function.prototype%` created
    /// by `VmRuntime` before this call.
    pub fn allocate(mm: &Arc<MemoryManager>, fn_proto: GcRef<JsObject>) -> Self {
        // Helper to allocate an empty object with no prototype
        let alloc = || GcRef::new(JsObject::new(Value::null(), mm.clone()));

        // Create well-known symbols
        let make_symbol = |id: u64, desc: &str| -> Value {
            Value::symbol(GcRef::new(Symbol {
                description: Some(desc.to_string()),
                id,
            }))
        };

        let result = Self {
            // Core prototypes
            object_prototype: alloc(),
            function_prototype: fn_proto, // Reuse existing intrinsic
            generator_function_prototype: alloc(),
            async_function_prototype: alloc(),
            async_generator_function_prototype: alloc(),
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
            // Generators
            generator_prototype: alloc(),
            async_generator_prototype: alloc(),
            // TypedArrays
            typed_array_prototype: alloc(),
            int8_array_prototype: alloc(),
            uint8_array_prototype: alloc(),
            uint8_clamped_array_prototype: alloc(),
            int16_array_prototype: alloc(),
            uint16_array_prototype: alloc(),
            int32_array_prototype: alloc(),
            uint32_array_prototype: alloc(),
            float32_array_prototype: alloc(),
            float64_array_prototype: alloc(),
            bigint64_array_prototype: alloc(),
            biguint64_array_prototype: alloc(),
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
            result.generator_prototype,
            result.async_generator_prototype,
            result.typed_array_prototype,
            result.int8_array_prototype,
            result.uint8_array_prototype,
            result.uint8_clamped_array_prototype,
            result.int16_array_prototype,
            result.uint16_array_prototype,
            result.int32_array_prototype,
            result.uint32_array_prototype,
            result.float32_array_prototype,
            result.float64_array_prototype,
            result.bigint64_array_prototype,
            result.biguint64_array_prototype,
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
            .set_prototype(Value::object(self.object_prototype));

        // Generator/Async function prototypes inherit from Function.prototype
        let function_like_protos = [
            self.generator_function_prototype,
            self.async_function_prototype,
            self.async_generator_function_prototype,
        ];
        for proto in &function_like_protos {
            proto.set_prototype(Value::object(self.function_prototype));
        }

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
            proto.set_prototype(Value::object(self.object_prototype));
        }

        // Error.prototype.[[Prototype]] = Object.prototype
        self.error_prototype
            .set_prototype(Value::object(self.object_prototype));

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
            proto.set_prototype(Value::object(self.error_prototype));
        }

        // AsyncIteratorPrototype.[[Prototype]] = Object.prototype
        self.async_iterator_prototype
            .set_prototype(Value::object(self.object_prototype));

        // Generator.prototype.[[Prototype]] = Iterator.prototype (ES2026 §27.5.1)
        self.generator_prototype
            .set_prototype(Value::object(self.iterator_prototype));

        // AsyncGenerator.prototype.[[Prototype]] = AsyncIterator.prototype
        self.async_generator_prototype
            .set_prototype(Value::object(self.async_iterator_prototype));

        // %TypedArray%.prototype.[[Prototype]] = Object.prototype (ES2026 §22.2.3)
        self.typed_array_prototype
            .set_prototype(Value::object(self.object_prototype));

        // All specific TypedArray prototypes chain to %TypedArray%.prototype
        let typed_array_protos = [
            self.int8_array_prototype,
            self.uint8_array_prototype,
            self.uint8_clamped_array_prototype,
            self.int16_array_prototype,
            self.uint16_array_prototype,
            self.int32_array_prototype,
            self.uint32_array_prototype,
            self.float32_array_prototype,
            self.float64_array_prototype,
            self.bigint64_array_prototype,
            self.biguint64_array_prototype,
        ];
        for proto in &typed_array_protos {
            proto.set_prototype(Value::object(self.typed_array_prototype));
        }

        // Constructor objects: [[Prototype]] = Function.prototype
        let ctors = [self.object_constructor, self.function_constructor];
        for ctor in &ctors {
            ctor.set_prototype(Value::object(self.function_prototype));
        }
    }

    /// Stage 3: Initialize core intrinsic properties using `BuiltInBuilder`.
    ///
    /// This populates Object.prototype, Function.prototype, and Error prototypes
    /// with their spec-required methods and properties. Must be called after
    /// `wire_prototype_chains()`.
    pub fn init_core(&self, mm: &Arc<MemoryManager>) {
        use crate::object::{PropertyDescriptor, PropertyKey};

        // ====================================================================
        // Object.prototype methods (extracted to intrinsics_impl/object.rs)
        // ====================================================================
        let fn_proto = self.function_prototype;
        crate::intrinsics_impl::object::init_object_prototype(self.object_prototype, fn_proto, mm);

        // ===================================================================
        // Function.prototype methods (extracted to intrinsics_impl/function.rs)
        // ===================================================================
        crate::intrinsics_impl::function::init_function_prototype(fn_proto, mm);

        // ====================================================================
        // Error.prototype properties (extracted to intrinsics_impl/error.rs)
        // ====================================================================
        crate::intrinsics_impl::error::init_error_prototypes(
            self.error_prototype,
            self.type_error_prototype,
            self.range_error_prototype,
            self.reference_error_prototype,
            self.syntax_error_prototype,
            self.uri_error_prototype,
            self.eval_error_prototype,
            fn_proto,
            mm,
        );

        // ====================================================================
        // Object static methods (extracted to intrinsics_impl/object.rs)
        // ====================================================================
        crate::intrinsics_impl::object::init_object_constructor(
            self.object_constructor,
            fn_proto,
            mm,
        );

        // ====================================================================

        // ===================================================================
        // String.prototype methods (extracted to intrinsics_impl/string.rs)
        // ===================================================================
        crate::intrinsics_impl::string::init_string_prototype(
            self.string_prototype,
            fn_proto,
            mm,
            self.iterator_prototype,
            well_known::iterator_symbol(),
        );

        // ====================================================================

        // ===================================================================
        // Number.prototype methods (extracted to intrinsics_impl/number.rs)
        // ===================================================================
        crate::intrinsics_impl::number::init_number_prototype(self.number_prototype, fn_proto, mm);

        // ===================================================================
        // Boolean.prototype methods (extracted to intrinsics_impl/boolean.rs)
        // ===================================================================
        crate::intrinsics_impl::boolean::init_boolean_prototype(
            self.boolean_prototype,
            fn_proto,
            mm,
        );

        // ===================================================================
        // Symbol.prototype methods (extracted to intrinsics_impl/symbol.rs)
        // ===================================================================
        crate::intrinsics_impl::symbol::init_symbol_prototype(self.symbol_prototype, fn_proto, mm);

        // ===================================================================
        // BigInt.prototype methods (extracted to intrinsics_impl/bigint.rs)
        // ===================================================================
        crate::intrinsics_impl::bigint::init_bigint_prototype(self.bigint_prototype, fn_proto, mm);

        // ===================================================================
        // Date.prototype methods (extracted to intrinsics_impl/date.rs)
        // ===================================================================
        crate::intrinsics_impl::date::init_date_prototype(
            self.date_prototype,
            fn_proto,
            mm,
            well_known::to_string_tag_symbol(),
        );

        // ====================================================================
        // Iterator prototype: [Symbol.iterator]() { return this; }
        // ====================================================================
        if let Some(sym) = self.symbol_iterator.as_symbol() {
            self.iterator_prototype.define_property(
                PropertyKey::Symbol(sym),
                PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                    |this_val, _args, _ncx| Ok(this_val.clone()),
                    mm.clone(),
                    fn_proto,
                )),
            );
        }

        // ====================================================================

        // ===================================================================
        // Array.prototype methods (extracted to intrinsics_impl/array.rs)
        // ===================================================================
        // ES2026 §23.1.3: Array.prototype is itself an Array exotic object
        // Its [[DefineOwnProperty]] is as specified in §10.4.2.1, length is 0
        self.array_prototype.mark_as_array();
        crate::intrinsics_impl::array::init_array_prototype(
            self.array_prototype,
            fn_proto,
            mm,
            self.iterator_prototype,
            well_known::iterator_symbol(),
        );

        // ===================================================================
        // Map/Set/WeakMap/WeakSet prototype methods (extracted to intrinsics_impl/map_set.rs)
        // ===================================================================
        crate::intrinsics_impl::map_set::init_map_prototype(
            self.map_prototype,
            fn_proto,
            mm,
            self.iterator_prototype,
            well_known::iterator_symbol(),
        );
        crate::intrinsics_impl::map_set::init_set_prototype(
            self.set_prototype,
            fn_proto,
            mm,
            self.iterator_prototype,
            well_known::iterator_symbol(),
        );
        crate::intrinsics_impl::map_set::init_weak_map_prototype(
            self.weak_map_prototype,
            fn_proto,
            mm,
        );
        crate::intrinsics_impl::map_set::init_weak_set_prototype(
            self.weak_set_prototype,
            fn_proto,
            mm,
        );

        // ===================================================================
        // RegExp.prototype methods (extracted to intrinsics_impl/regexp.rs)
        // ===================================================================
        crate::intrinsics_impl::regexp::init_regexp_prototype(self.regexp_prototype, fn_proto, mm);

        // ===================================================================
        // Promise.prototype methods (extracted to intrinsics_impl/promise.rs)
        // ===================================================================
        crate::intrinsics_impl::promise::init_promise_prototype(
            self.promise_prototype,
            fn_proto,
            mm,
        );

        // ===================================================================
        // Generator.prototype and AsyncGenerator.prototype methods
        // ===================================================================
        crate::intrinsics_impl::generator::init_generator_prototype(
            self.generator_prototype,
            fn_proto,
            mm,
            well_known::iterator_symbol(),
            well_known::to_string_tag_symbol(),
        );

        crate::intrinsics_impl::generator::init_async_generator_prototype(
            self.async_generator_prototype,
            fn_proto,
            mm,
            well_known::async_iterator_symbol(),
            well_known::to_string_tag_symbol(),
        );

        // ===================================================================
        // %TypedArray%.prototype and all specific TypedArray prototypes
        // ===================================================================
        crate::intrinsics_impl::typed_array::init_typed_array_prototype(
            self.typed_array_prototype,
            fn_proto,
            mm,
            well_known::iterator_symbol(),
            well_known::to_string_tag_symbol(),
        );

        // Initialize each specific typed array prototype
        use crate::typed_array::TypedArrayKind;
        crate::intrinsics_impl::typed_array::init_specific_typed_array_prototype(
            self.int8_array_prototype,
            TypedArrayKind::Int8,
            well_known::to_string_tag_symbol(),
        );
        crate::intrinsics_impl::typed_array::init_specific_typed_array_prototype(
            self.uint8_array_prototype,
            TypedArrayKind::Uint8,
            well_known::to_string_tag_symbol(),
        );
        crate::intrinsics_impl::typed_array::init_specific_typed_array_prototype(
            self.uint8_clamped_array_prototype,
            TypedArrayKind::Uint8Clamped,
            well_known::to_string_tag_symbol(),
        );
        crate::intrinsics_impl::typed_array::init_specific_typed_array_prototype(
            self.int16_array_prototype,
            TypedArrayKind::Int16,
            well_known::to_string_tag_symbol(),
        );
        crate::intrinsics_impl::typed_array::init_specific_typed_array_prototype(
            self.uint16_array_prototype,
            TypedArrayKind::Uint16,
            well_known::to_string_tag_symbol(),
        );
        crate::intrinsics_impl::typed_array::init_specific_typed_array_prototype(
            self.int32_array_prototype,
            TypedArrayKind::Int32,
            well_known::to_string_tag_symbol(),
        );
        crate::intrinsics_impl::typed_array::init_specific_typed_array_prototype(
            self.uint32_array_prototype,
            TypedArrayKind::Uint32,
            well_known::to_string_tag_symbol(),
        );
        crate::intrinsics_impl::typed_array::init_specific_typed_array_prototype(
            self.float32_array_prototype,
            TypedArrayKind::Float32,
            well_known::to_string_tag_symbol(),
        );
        crate::intrinsics_impl::typed_array::init_specific_typed_array_prototype(
            self.float64_array_prototype,
            TypedArrayKind::Float64,
            well_known::to_string_tag_symbol(),
        );
        crate::intrinsics_impl::typed_array::init_specific_typed_array_prototype(
            self.bigint64_array_prototype,
            TypedArrayKind::BigInt64,
            well_known::to_string_tag_symbol(),
        );
        crate::intrinsics_impl::typed_array::init_specific_typed_array_prototype(
            self.biguint64_array_prototype,
            TypedArrayKind::BigUint64,
            well_known::to_string_tag_symbol(),
        );
    }

    /// Install intrinsic constructors on the global object.
    ///
    /// This creates constructor Values (native functions) backed by the intrinsic
    /// objects and installs them as global properties. Call after `init_core()`.
    pub fn install_on_global(&self, global: GcRef<JsObject>, mm: &Arc<MemoryManager>) {
        use crate::object::{PropertyAttributes, PropertyDescriptor, PropertyKey};
        use crate::string::JsString;

        let fn_proto = self.function_prototype;
        let realm_id = fn_proto
            .get(&PropertyKey::string("__realm_id__"))
            .and_then(|v| v.as_int32())
            .map(|id| id as u32)
            .unwrap_or(0);
        let alloc_ctor = || GcRef::new(JsObject::new(Value::object(fn_proto), mm.clone()));

        // Helper: install a constructor+prototype pair on the global
        let install = |name: &str,
                       ctor_obj: GcRef<JsObject>,
                       proto: GcRef<JsObject>,
                       ctor_fn: Option<
            Box<
                dyn Fn(
                        &Value,
                        &[Value],
                        &mut crate::context::NativeContext<'_>,
                    ) -> Result<Value, VmError>
                    + Send
                    + Sync,
            >,
        >| {
            // Tag constructor to enable realm-aware GetPrototypeFromConstructor defaults.
            ctor_obj.define_property(
                PropertyKey::string("__builtin_tag__"),
                PropertyDescriptor::data_with_attrs(
                    Value::string(JsString::intern(name)),
                    PropertyAttributes::permanent(),
                ),
            );

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
                PropertyDescriptor::data_with_attrs(
                    ctor_value,
                    PropertyAttributes::builtin_method(),
                ),
            );
        };

        // ====================================================================
        // Core constructors
        // ====================================================================
        install(
            "Object",
            self.object_constructor,
            self.object_prototype,
            Some(crate::intrinsics_impl::object::create_object_constructor()),
        );
        install(
            "Function",
            self.function_constructor,
            self.function_prototype,
            Some(crate::intrinsics_impl::function::create_function_constructor(realm_id)),
        );
        let gen_fn_ctor = alloc_ctor();
        install(
            "GeneratorFunction",
            gen_fn_ctor,
            self.generator_function_prototype,
            Some(crate::intrinsics_impl::function::create_generator_function_constructor(realm_id)),
        );
        let async_fn_ctor = alloc_ctor();
        install(
            "AsyncFunction",
            async_fn_ctor,
            self.async_function_prototype,
            Some(crate::intrinsics_impl::function::create_async_function_constructor(realm_id)),
        );
        let async_gen_fn_ctor = alloc_ctor();
        install(
            "AsyncGeneratorFunction",
            async_gen_fn_ctor,
            self.async_generator_function_prototype,
            Some(
                crate::intrinsics_impl::function::create_async_generator_function_constructor(
                    realm_id,
                ),
            ),
        );

        // Internal globals used by the interpreter for function prototype lookups.
        global.define_property(
            PropertyKey::string("GeneratorFunctionPrototype"),
            PropertyDescriptor::data_with_attrs(
                Value::object(self.generator_function_prototype),
                PropertyAttributes::permanent(),
            ),
        );
        global.define_property(
            PropertyKey::string("AsyncFunctionPrototype"),
            PropertyDescriptor::data_with_attrs(
                Value::object(self.async_function_prototype),
                PropertyAttributes::permanent(),
            ),
        );
        global.define_property(
            PropertyKey::string("AsyncGeneratorFunctionPrototype"),
            PropertyDescriptor::data_with_attrs(
                Value::object(self.async_generator_function_prototype),
                PropertyAttributes::permanent(),
            ),
        );

        // Register global aliases for interpreter interception
        // The interpreter checks for these globals to detect and intercept
        // Function.prototype.call/apply (see interpreter.rs:5647, 5651)
        if let Some(call_fn) = self.function_prototype.get(&PropertyKey::string("call")) {
            let _ = global.set(PropertyKey::string("__Function_call"), call_fn);
        }
        if let Some(apply_fn) = self.function_prototype.get(&PropertyKey::string("apply")) {
            let _ = global.set(PropertyKey::string("__Function_apply"), apply_fn);
        }

        // ====================================================================
        // Primitive wrapper constructors
        // ====================================================================

        // For constructors that need actual implementations, we allocate fresh
        // constructor objects (since intrinsics only pre-allocated prototypes).
        // The prototype still comes from intrinsics with correct [[Prototype]] chain.

        // String
        let string_ctor = alloc_ctor();
        let string_ctor_fn: Box<
            dyn Fn(
                    &Value,
                    &[Value],
                    &mut crate::context::NativeContext<'_>,
                ) -> Result<Value, VmError>
                + Send
                + Sync,
        > = Box::new(|this, args, ncx| {
            let s = if let Some(arg) = args.first() {
                let symbol_to_string = |sym: &crate::value::Symbol| {
                    if let Some(desc) = sym.description.as_deref() {
                        format!("Symbol({})", desc)
                    } else {
                        "Symbol()".to_string()
                    }
                };

                if let Some(sym) = arg.as_symbol() {
                    symbol_to_string(&sym)
                } else if arg.is_object() {
                    let prim = ncx.to_primitive(arg, crate::interpreter::PreferredType::String)?;
                    if let Some(sym) = prim.as_symbol() {
                        symbol_to_string(&sym)
                    } else {
                        ncx.to_string_value(&prim)?
                    }
                } else {
                    ncx.to_string_value(arg)?
                }
            } else {
                String::new()
            };
            let str_val = Value::string(JsString::intern(&s));
            // When called as constructor (new String("...")), `this` is an object.
            // Store the primitive value so String.prototype methods can retrieve it.
            if let Some(obj) = this.as_object() {
                let _ = obj.set(PropertyKey::string("__primitiveValue__"), str_val.clone());
            }
            Ok(str_val)
        });
        install(
            "String",
            string_ctor,
            self.string_prototype,
            Some(string_ctor_fn),
        );

        // String.fromCharCode(...codeUnits)
        string_ctor.define_property(
            PropertyKey::string("fromCharCode"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _ncx| {
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
                |_this, args, _ncx| {
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
                            return Err(VmError::type_error(format!(
                                "Invalid code point: {}",
                                code
                            )));
                        }
                        if let Some(ch) = char::from_u32(code) {
                            result.push(ch);
                        } else {
                            return Err(VmError::type_error(format!(
                                "Invalid code point: {}",
                                code
                            )));
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
            dyn Fn(
                    &Value,
                    &[Value],
                    &mut crate::context::NativeContext<'_>,
                ) -> Result<Value, VmError>
                + Send
                + Sync,
        > = Box::new(|this, args, ncx| {
            let n = if let Some(arg) = args.first() {
                ncx.to_number_value(arg)?
            } else {
                0.0
            };
            if let Some(obj) = this.as_object() {
                let _ = obj.set(PropertyKey::string("__value__"), Value::number(n));
                Ok(this.clone())
            } else {
                Ok(Value::number(n))
            }
        });
        install(
            "Number",
            number_ctor,
            self.number_prototype,
            Some(number_ctor_fn),
        );
        crate::intrinsics_impl::number::install_number_statics(number_ctor, fn_proto, mm);

        // Boolean
        let boolean_ctor = alloc_ctor();
        let boolean_ctor_fn = crate::intrinsics_impl::boolean::create_boolean_constructor();
        install(
            "Boolean",
            boolean_ctor,
            self.boolean_prototype,
            Some(boolean_ctor_fn),
        );

        // Symbol
        let symbol_ctor = alloc_ctor();
        let symbol_ctor_fn = crate::intrinsics_impl::symbol::create_symbol_constructor();
        install(
            "Symbol",
            symbol_ctor,
            self.symbol_prototype,
            Some(symbol_ctor_fn),
        );
        crate::intrinsics_impl::symbol::install_symbol_statics(symbol_ctor, fn_proto, mm);

        // BigInt
        let bigint_ctor = alloc_ctor();
        let bigint_ctor_fn = crate::intrinsics_impl::bigint::create_bigint_constructor();
        install(
            "BigInt",
            bigint_ctor,
            self.bigint_prototype,
            Some(bigint_ctor_fn),
        );
        crate::intrinsics_impl::bigint::install_bigint_statics(bigint_ctor, fn_proto, mm);

        // ====================================================================
        // Collection constructors
        // ====================================================================
        let array_ctor = alloc_ctor();
        let array_ctor_fn: Box<
            dyn Fn(
                    &Value,
                    &[Value],
                    &mut crate::context::NativeContext<'_>,
                ) -> Result<Value, VmError>
                + Send
                + Sync,
        > = Box::new(|_this, args, ncx| {
            if args.len() == 1 {
                if let Some(n) = args[0].as_number() {
                    let len = n as u32;
                    if (len as f64) != n || n < 0.0 {
                        return Err(VmError::type_error("Invalid array length"));
                    }
                    let arr =
                        GcRef::new(JsObject::array(len as usize, ncx.memory_manager().clone()));
                    return Ok(Value::object(arr));
                }
            }
            // Array(...items) — populate the array
            let arr = GcRef::new(JsObject::array(args.len(), ncx.memory_manager().clone()));
            for (i, arg) in args.iter().enumerate() {
                let _ = arr.set(PropertyKey::index(i as u32), arg.clone());
            }
            Ok(Value::object(arr))
        });
        install(
            "Array",
            array_ctor,
            self.array_prototype,
            Some(array_ctor_fn),
        );
        crate::intrinsics_impl::array::install_array_statics(array_ctor, fn_proto, mm);
        crate::intrinsics_impl::helpers::define_species_getter(array_ctor, fn_proto, mm);

        let map_ctor = alloc_ctor();
        let map_ctor_fn = crate::intrinsics_impl::map_set::create_map_constructor();
        install("Map", map_ctor, self.map_prototype, Some(map_ctor_fn));
        crate::intrinsics_impl::helpers::define_species_getter(map_ctor, fn_proto, mm);

        let set_ctor = alloc_ctor();
        let set_ctor_fn = crate::intrinsics_impl::map_set::create_set_constructor();
        install("Set", set_ctor, self.set_prototype, Some(set_ctor_fn));
        crate::intrinsics_impl::helpers::define_species_getter(set_ctor, fn_proto, mm);

        let weak_map_ctor = alloc_ctor();
        let weak_map_ctor_fn = crate::intrinsics_impl::map_set::create_weak_map_constructor();
        install(
            "WeakMap",
            weak_map_ctor,
            self.weak_map_prototype,
            Some(weak_map_ctor_fn),
        );
        // WeakMap constructor.length should be 0 (iterable parameter is optional)
        weak_map_ctor.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::number(0.0)),
        );

        let weak_set_ctor = alloc_ctor();
        let weak_set_ctor_fn = crate::intrinsics_impl::map_set::create_weak_set_constructor();
        install(
            "WeakSet",
            weak_set_ctor,
            self.weak_set_prototype,
            Some(weak_set_ctor_fn),
        );
        // WeakSet constructor.length should be 0 (iterable parameter is optional)
        weak_set_ctor.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::number(0.0)),
        );

        // ====================================================================
        // Error constructors (extracted to intrinsics_impl/error.rs)
        // ====================================================================
        let error_ctor = alloc_ctor();
        install(
            "Error",
            error_ctor,
            self.error_prototype,
            Some(crate::intrinsics_impl::error::create_error_constructor(
                "Error",
            )),
        );

        let type_error_ctor = alloc_ctor();
        install(
            "TypeError",
            type_error_ctor,
            self.type_error_prototype,
            Some(crate::intrinsics_impl::error::create_error_constructor(
                "TypeError",
            )),
        );

        let range_error_ctor = alloc_ctor();
        install(
            "RangeError",
            range_error_ctor,
            self.range_error_prototype,
            Some(crate::intrinsics_impl::error::create_error_constructor(
                "RangeError",
            )),
        );

        let reference_error_ctor = alloc_ctor();
        install(
            "ReferenceError",
            reference_error_ctor,
            self.reference_error_prototype,
            Some(crate::intrinsics_impl::error::create_error_constructor(
                "ReferenceError",
            )),
        );

        let syntax_error_ctor = alloc_ctor();
        install(
            "SyntaxError",
            syntax_error_ctor,
            self.syntax_error_prototype,
            Some(crate::intrinsics_impl::error::create_error_constructor(
                "SyntaxError",
            )),
        );

        let uri_error_ctor = alloc_ctor();
        install(
            "URIError",
            uri_error_ctor,
            self.uri_error_prototype,
            Some(crate::intrinsics_impl::error::create_error_constructor(
                "URIError",
            )),
        );

        let eval_error_ctor = alloc_ctor();
        install(
            "EvalError",
            eval_error_ctor,
            self.eval_error_prototype,
            Some(crate::intrinsics_impl::error::create_error_constructor(
                "EvalError",
            )),
        );

        // ====================================================================
        // Other builtins
        // ====================================================================
        let promise_ctor = alloc_ctor();
        install(
            "Promise",
            promise_ctor,
            self.promise_prototype,
            Some(crate::intrinsics_impl::promise::create_promise_constructor()),
        );
        crate::intrinsics_impl::promise::install_promise_statics(promise_ctor, fn_proto, mm);
        crate::intrinsics_impl::helpers::define_species_getter(promise_ctor, fn_proto, mm);

        let regexp_ctor = alloc_ctor();
        let regexp_ctor_fn =
            crate::intrinsics_impl::regexp::create_regexp_constructor(self.regexp_prototype);
        install(
            "RegExp",
            regexp_ctor,
            self.regexp_prototype,
            Some(regexp_ctor_fn),
        );
        crate::intrinsics_impl::helpers::define_species_getter(regexp_ctor, fn_proto, mm);

        // RegExp.escape (ES2026 §22.2.4.1)
        regexp_ctor.define_property(
            PropertyKey::string("escape"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, args, _ncx| {
                    let s = args.first().and_then(|v| v.as_string()).ok_or_else(|| {
                        VmError::type_error("RegExp.escape requires a string argument")
                    })?;
                    Ok(Value::string(JsString::intern(&regress::escape(
                        s.as_str(),
                    ))))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        let date_ctor = alloc_ctor();
        let date_ctor_fn: Box<
            dyn Fn(
                    &Value,
                    &[Value],
                    &mut crate::context::NativeContext<'_>,
                ) -> Result<Value, VmError>
                + Send
                + Sync,
        > = Box::new(|this, args, ncx| {
            use chrono::{Local, NaiveDate, TimeZone};
            use std::time::{SystemTime, UNIX_EPOCH};

            // Per ES spec: Date() called without `new` returns a string
            // When called with new, `this` is a new object with Date.prototype
            // When called without new, `this` is undefined or global
            let is_constructor_call = this.as_object().map_or(false, |obj| {
                let proto = obj.prototype();
                !proto.is_null() && !proto.is_undefined()
            });

            if !is_constructor_call {
                // Return current date as string per ES spec 20.3.2.2
                let now = Local::now();
                let date_str = now.format("%a %b %d %Y %H:%M:%S GMT%z").to_string();
                return Ok(Value::string(JsString::intern(&date_str)));
            }

            let timestamp = if args.is_empty() {
                // No args: current time
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as f64)
                    .unwrap_or(0.0)
            } else if args.len() == 1 {
                // Single arg: string or timestamp
                // Per ES spec, use "default" hint for ToPrimitive, not "number"
                let prim =
                    ncx.to_primitive(&args[0], crate::interpreter::PreferredType::Default)?;
                if let Some(date_str) = prim.as_string() {
                    // Parse date string
                    parse_date_string(date_str.as_str())
                } else {
                    ncx.to_number_value(&prim)?
                }
            } else {
                // Multi-arg: Date(year, month, day?, hours?, minutes?, seconds?, ms?)
                // Per ES spec 20.3.1.1, ToNumber each argument in order
                let year = ncx.to_number_value(&args[0])? as i32;
                let month = args
                    .get(1)
                    .map(|v| ncx.to_number_value(v))
                    .transpose()?
                    .unwrap_or(0.0) as u32
                    + 1; // JS months are 0-indexed
                let day = args
                    .get(2)
                    .map(|v| ncx.to_number_value(v))
                    .transpose()?
                    .unwrap_or(1.0) as u32;
                let hour = args
                    .get(3)
                    .map(|v| ncx.to_number_value(v))
                    .transpose()?
                    .unwrap_or(0.0) as u32;
                let min = args
                    .get(4)
                    .map(|v| ncx.to_number_value(v))
                    .transpose()?
                    .unwrap_or(0.0) as u32;
                let sec = args
                    .get(5)
                    .map(|v| ncx.to_number_value(v))
                    .transpose()?
                    .unwrap_or(0.0) as u32;
                let ms = args
                    .get(6)
                    .map(|v| ncx.to_number_value(v))
                    .transpose()?
                    .unwrap_or(0.0) as u32;

                // 2-digit years (0-99) → 1900-1999 per ES spec
                let adjusted_year = if year >= 0 && year <= 99 {
                    1900 + year
                } else {
                    year
                };

                NaiveDate::from_ymd_opt(adjusted_year, month, day)
                    .and_then(|d| d.and_hms_milli_opt(hour, min, sec, ms))
                    .and_then(|dt| Local.from_local_datetime(&dt).single())
                    .map(|ldt| ldt.timestamp_millis() as f64)
                    .unwrap_or(f64::NAN)
            };

            // Set timestamp on `this` (created by Construct with Date.prototype)
            if let Some(obj) = this.as_object() {
                let _ = obj.set(
                    PropertyKey::string("__timestamp__"),
                    Value::number(timestamp),
                );
            }
            Ok(Value::undefined())
        });

        // Helper for date string parsing
        fn parse_date_string(s: &str) -> f64 {
            use chrono::{NaiveDate, NaiveDateTime};
            let s = s.trim();

            // Try RFC3339/ISO8601
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                return dt.timestamp_millis() as f64;
            }

            // Try date-only (YYYY-MM-DD) - interpreted as UTC per ES spec
            if s.len() == 10 && s.chars().nth(4) == Some('-') && s.chars().nth(7) == Some('-') {
                if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                    if let Some(dt) = d.and_hms_opt(0, 0, 0) {
                        return dt.and_utc().timestamp_millis() as f64;
                    }
                }
            }

            // Try datetime without timezone
            if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
                return dt.and_utc().timestamp_millis() as f64;
            }
            if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
                return dt.and_utc().timestamp_millis() as f64;
            }

            f64::NAN
        }

        install("Date", date_ctor, self.date_prototype, Some(date_ctor_fn));

        // Date.length = 7 (per ECMAScript spec - Date constructor takes 7 args max)
        date_ctor.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::data_with_attrs(
                Value::int32(7),
                PropertyAttributes::function_length(),
            ),
        );

        // Date.now() - returns current timestamp in milliseconds
        date_ctor.define_property(
            PropertyKey::string("now"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |_this, _args, _ncx| {
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
                |_this, args, _ncx| {
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
                                .or_else(|_| {
                                    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
                                })
                                .map(|dt| dt.and_utc().timestamp_millis() as f64)
                        })
                        .or_else(|_| {
                            // Try parsing as date only
                            NaiveDate::parse_from_str(s, "%Y-%m-%d").map(|d| {
                                d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis() as f64
                            })
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
                |_this, args, _ncx| {
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

                    let date =
                        NaiveDate::from_ymd_opt(year, month as u32, day as u32).and_then(|d| {
                            d.and_hms_milli_opt(
                                hour as u32,
                                minute as u32,
                                second as u32,
                                ms as u32,
                            )
                        });

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
        install(
            "ArrayBuffer",
            array_buffer_ctor,
            self.array_buffer_prototype,
            None,
        );

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

        // ====================================================================
        // JSON namespace (extracted to intrinsics_impl/json.rs)
        // Implements JSON.parse and JSON.stringify using serde_json
        // ====================================================================
        crate::intrinsics_impl::json::install_json_namespace(global, mm, self.function_prototype);
    }
}
