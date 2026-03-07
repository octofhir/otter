//! Runtime-owned registry for intrinsic objects and well-known symbols.
//!
//! `Intrinsics` is allocated once per runtime, then reused by every context.
//! Initialization is split into allocation, prototype wiring, prototype/constructor
//! population, and optional global installation.

use std::sync::Arc;

use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::JsObject;

use crate::value::{Symbol, Value};

/// Stable IDs for well-known symbols.
///
/// Symbol identity is derived from these IDs, so they must remain stable across
/// the runtime.
pub mod well_known {
    pub const ITERATOR: u64 = 1;
    pub const ASYNC_ITERATOR: u64 = 2;
    pub const TO_STRING_TAG: u64 = 3;
    pub const HAS_INSTANCE: u64 = 4;
    pub const TO_PRIMITIVE: u64 = 5;
    pub const IS_CONCAT_SPREADABLE: u64 = 6;
    pub const MATCH: u64 = 7;
    pub const MATCH_ALL: u64 = 8;
    pub const REPLACE: u64 = 9;
    pub const SEARCH: u64 = 10;
    pub const SPLIT: u64 = 11;
    pub const SPECIES: u64 = 12;
    pub const UNSCOPABLES: u64 = 13;

    /// Creates a symbol ref with the given well-known ID.
    pub fn symbol_ref(id: u64, desc: &'static str) -> crate::gc::GcRef<crate::value::Symbol> {
        crate::gc::GcRef::new(crate::value::Symbol {
            description: Some(desc.to_string()),
            id,
        })
    }

    pub fn iterator_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(ITERATOR, "Symbol.iterator")
    }

    pub fn async_iterator_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(ASYNC_ITERATOR, "Symbol.asyncIterator")
    }

    pub fn to_string_tag_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(TO_STRING_TAG, "Symbol.toStringTag")
    }

    pub fn has_instance_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(HAS_INSTANCE, "Symbol.hasInstance")
    }

    pub fn to_primitive_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(TO_PRIMITIVE, "Symbol.toPrimitive")
    }

    pub fn is_concat_spreadable_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(IS_CONCAT_SPREADABLE, "Symbol.isConcatSpreadable")
    }

    pub fn match_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(MATCH, "Symbol.match")
    }

    pub fn match_all_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(MATCH_ALL, "Symbol.matchAll")
    }

    pub fn replace_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(REPLACE, "Symbol.replace")
    }

    pub fn search_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(SEARCH, "Symbol.search")
    }

    pub fn split_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(SPLIT, "Symbol.split")
    }

    pub fn species_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(SPECIES, "Symbol.species")
    }

    pub fn unscopables_symbol() -> crate::gc::GcRef<crate::value::Symbol> {
        symbol_ref(UNSCOPABLES, "Symbol.unscopables")
    }
}

/// Runtime-owned handles to intrinsic prototypes, constructors, and symbols.
#[derive(Clone)]
pub struct Intrinsics {
    // Core prototypes
    pub object_prototype: GcRef<JsObject>,
    pub function_prototype: GcRef<JsObject>,
    pub generator_function_prototype: GcRef<JsObject>,
    pub async_function_prototype: GcRef<JsObject>,
    pub async_generator_function_prototype: GcRef<JsObject>,

    // Core constructors
    pub object_constructor: GcRef<JsObject>,
    pub function_constructor: GcRef<JsObject>,
    pub abort_controller_constructor: GcRef<JsObject>,
    pub abort_signal_constructor: GcRef<JsObject>,
    pub regexp_constructor: GcRef<JsObject>,

    // Primitive wrapper prototypes
    pub string_prototype: GcRef<JsObject>,
    pub number_prototype: GcRef<JsObject>,
    pub boolean_prototype: GcRef<JsObject>,
    pub symbol_prototype: GcRef<JsObject>,
    pub bigint_prototype: GcRef<JsObject>,

    // Collection prototypes
    pub array_prototype: GcRef<JsObject>,
    pub map_prototype: GcRef<JsObject>,
    pub set_prototype: GcRef<JsObject>,
    pub weak_map_prototype: GcRef<JsObject>,
    pub weak_set_prototype: GcRef<JsObject>,

    // Error prototypes
    pub error_prototype: GcRef<JsObject>,
    pub type_error_prototype: GcRef<JsObject>,
    pub range_error_prototype: GcRef<JsObject>,
    pub reference_error_prototype: GcRef<JsObject>,
    pub syntax_error_prototype: GcRef<JsObject>,
    pub uri_error_prototype: GcRef<JsObject>,
    pub eval_error_prototype: GcRef<JsObject>,
    pub aggregate_error_prototype: GcRef<JsObject>,

    // Async/Promise
    pub promise_prototype: GcRef<JsObject>,

    // Other built-in prototypes
    pub regexp_prototype: GcRef<JsObject>,
    pub date_prototype: GcRef<JsObject>,
    pub array_buffer_prototype: GcRef<JsObject>,
    pub data_view_prototype: GcRef<JsObject>,
    pub abort_controller_prototype: GcRef<JsObject>,
    pub abort_signal_prototype: GcRef<JsObject>,

    // Iterator and iterator-adjacent prototypes
    pub iterator_prototype: GcRef<JsObject>,
    pub string_iterator_prototype: GcRef<JsObject>,
    pub array_iterator_prototype: GcRef<JsObject>,
    pub async_iterator_prototype: GcRef<JsObject>,
    pub iterator_helper_prototype: GcRef<JsObject>,
    pub wrap_for_valid_iterator_prototype: GcRef<JsObject>,
    pub weak_ref_prototype: GcRef<JsObject>,
    pub finalization_registry_prototype: GcRef<JsObject>,

    // Generator prototypes
    pub generator_prototype: GcRef<JsObject>,
    pub async_generator_prototype: GcRef<JsObject>,

    // TypedArray prototypes
    pub typed_array_prototype: GcRef<JsObject>,
    pub int8_array_prototype: GcRef<JsObject>,
    pub uint8_array_prototype: GcRef<JsObject>,
    pub uint8_clamped_array_prototype: GcRef<JsObject>,
    pub int16_array_prototype: GcRef<JsObject>,
    pub uint16_array_prototype: GcRef<JsObject>,
    pub int32_array_prototype: GcRef<JsObject>,
    pub uint32_array_prototype: GcRef<JsObject>,
    pub float32_array_prototype: GcRef<JsObject>,
    pub float64_array_prototype: GcRef<JsObject>,
    pub bigint64_array_prototype: GcRef<JsObject>,
    pub biguint64_array_prototype: GcRef<JsObject>,

    // Well-known symbols
    pub symbol_iterator: Value,
    pub symbol_async_iterator: Value,
    pub symbol_to_string_tag: Value,
    pub symbol_has_instance: Value,
    pub symbol_to_primitive: Value,
    pub symbol_is_concat_spreadable: Value,
    pub symbol_match: Value,
    pub symbol_match_all: Value,
    pub symbol_replace: Value,
    pub symbol_search: Value,
    pub symbol_split: Value,
    pub symbol_species: Value,
    pub symbol_unscopables: Value,
}

impl Intrinsics {
    /// Trace all GC roots referenced by this intrinsics table.
    pub fn trace_roots(&self, tracer: &mut dyn FnMut(*const crate::gc::GcHeader)) {
        for obj in [
            self.object_prototype,
            self.function_prototype,
            self.generator_function_prototype,
            self.async_function_prototype,
            self.async_generator_function_prototype,
            self.object_constructor,
            self.function_constructor,
            self.abort_controller_constructor,
            self.abort_signal_constructor,
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
            self.error_prototype,
            self.type_error_prototype,
            self.range_error_prototype,
            self.reference_error_prototype,
            self.syntax_error_prototype,
            self.uri_error_prototype,
            self.eval_error_prototype,
            self.aggregate_error_prototype,
            self.promise_prototype,
            self.regexp_prototype,
            self.regexp_constructor,
            self.date_prototype,
            self.array_buffer_prototype,
            self.data_view_prototype,
            self.abort_controller_prototype,
            self.abort_signal_prototype,
            self.iterator_prototype,
            self.string_iterator_prototype,
            self.array_iterator_prototype,
            self.async_iterator_prototype,
            self.iterator_helper_prototype,
            self.wrap_for_valid_iterator_prototype,
            self.weak_ref_prototype,
            self.finalization_registry_prototype,
            self.generator_prototype,
            self.async_generator_prototype,
            self.typed_array_prototype,
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
        ] {
            tracer(obj.header() as *const _);
        }

        for symbol in [
            &self.symbol_iterator,
            &self.symbol_async_iterator,
            &self.symbol_to_string_tag,
            &self.symbol_has_instance,
            &self.symbol_to_primitive,
            &self.symbol_is_concat_spreadable,
            &self.symbol_match,
            &self.symbol_match_all,
            &self.symbol_replace,
            &self.symbol_search,
            &self.symbol_split,
            &self.symbol_species,
            &self.symbol_unscopables,
        ] {
            symbol.trace(tracer);
        }
    }

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
            "AbortController" => Some(self.abort_controller_prototype),
            "AbortSignal" => Some(self.abort_signal_prototype),
            "Error" => Some(self.error_prototype),
            "TypeError" => Some(self.type_error_prototype),
            "RangeError" => Some(self.range_error_prototype),
            "ReferenceError" => Some(self.reference_error_prototype),
            "SyntaxError" => Some(self.syntax_error_prototype),
            "URIError" => Some(self.uri_error_prototype),
            "EvalError" => Some(self.eval_error_prototype),
            "AggregateError" => Some(self.aggregate_error_prototype),
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

    /// Stage 1: allocate the intrinsic object graph and well-known symbols.
    ///
    /// The returned table still needs `wire_prototype_chains()`, `init_core()`,
    /// and optionally `install_on_global()`.
    pub fn allocate(fn_proto: GcRef<JsObject>) -> Self {
        let alloc = || GcRef::new(JsObject::new(Value::null()));

        let make_symbol = |id: u64, desc: &str| -> Value {
            Value::symbol(GcRef::new(Symbol {
                description: Some(desc.to_string()),
                id,
            }))
        };

        let result = Self {
            object_prototype: alloc(),
            function_prototype: fn_proto,
            generator_function_prototype: alloc(),
            async_function_prototype: alloc(),
            async_generator_function_prototype: alloc(),
            object_constructor: alloc(),
            function_constructor: alloc(),
            abort_controller_constructor: alloc(),
            abort_signal_constructor: alloc(),
            string_prototype: alloc(),
            number_prototype: alloc(),
            boolean_prototype: alloc(),
            symbol_prototype: alloc(),
            bigint_prototype: alloc(),
            array_prototype: alloc(),
            map_prototype: alloc(),
            set_prototype: alloc(),
            weak_map_prototype: alloc(),
            weak_set_prototype: alloc(),
            error_prototype: alloc(),
            type_error_prototype: alloc(),
            range_error_prototype: alloc(),
            reference_error_prototype: alloc(),
            syntax_error_prototype: alloc(),
            uri_error_prototype: alloc(),
            eval_error_prototype: alloc(),
            aggregate_error_prototype: alloc(),
            promise_prototype: alloc(),
            regexp_prototype: alloc(),
            date_prototype: alloc(),
            array_buffer_prototype: alloc(),
            data_view_prototype: alloc(),
            abort_controller_prototype: alloc(),
            abort_signal_prototype: alloc(),
            regexp_constructor: alloc(),
            iterator_prototype: alloc(),
            string_iterator_prototype: alloc(),
            array_iterator_prototype: alloc(),
            async_iterator_prototype: alloc(),
            iterator_helper_prototype: alloc(),
            wrap_for_valid_iterator_prototype: alloc(),
            weak_ref_prototype: alloc(),
            finalization_registry_prototype: alloc(),
            generator_prototype: alloc(),
            async_generator_prototype: alloc(),
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

        // Shared intrinsics must survive per-context teardown.
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
            result.aggregate_error_prototype,
            result.promise_prototype,
            result.regexp_prototype,
            result.regexp_constructor,
            result.date_prototype,
            result.array_buffer_prototype,
            result.data_view_prototype,
            result.abort_controller_prototype,
            result.abort_signal_prototype,
            result.iterator_prototype,
            result.string_iterator_prototype,
            result.array_iterator_prototype,
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
            result.abort_controller_constructor,
            result.abort_signal_constructor,
        ];
        for obj in all_intrinsic_objects {
            (*obj).mark_as_intrinsic();
        }

        result
    }

    /// Stage 2: assign the fixed intrinsic `[[Prototype]]` relationships.
    pub fn wire_prototype_chains(&self) {
        self.function_prototype
            .set_prototype(Value::object(self.object_prototype));

        let function_like_protos = [
            self.generator_function_prototype,
            self.async_function_prototype,
            self.async_generator_function_prototype,
        ];
        for proto in &function_like_protos {
            proto.set_prototype(Value::object(self.function_prototype));
        }

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
            self.abort_controller_prototype,
            self.abort_signal_prototype,
            self.iterator_prototype,
            self.weak_ref_prototype,
            self.finalization_registry_prototype,
        ];
        for proto in &protos_to_obj {
            proto.set_prototype(Value::object(self.object_prototype));
        }

        self.error_prototype
            .set_prototype(Value::object(self.object_prototype));

        let error_protos = [
            self.type_error_prototype,
            self.range_error_prototype,
            self.reference_error_prototype,
            self.syntax_error_prototype,
            self.uri_error_prototype,
            self.eval_error_prototype,
            self.aggregate_error_prototype,
        ];
        for proto in &error_protos {
            proto.set_prototype(Value::object(self.error_prototype));
        }

        self.async_iterator_prototype
            .set_prototype(Value::object(self.object_prototype));

        self.generator_prototype
            .set_prototype(Value::object(self.iterator_prototype));

        self.async_generator_prototype
            .set_prototype(Value::object(self.async_iterator_prototype));

        self.typed_array_prototype
            .set_prototype(Value::object(self.object_prototype));

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

        let ctors = [
            self.object_constructor,
            self.function_constructor,
            self.abort_controller_constructor,
            self.abort_signal_constructor,
        ];
        for ctor in &ctors {
            ctor.set_prototype(Value::object(self.function_prototype));
        }
    }

    /// Stage 3: populate intrinsic prototypes and constructors through wrapper types.
    pub fn init_core(&self, mm: &Arc<MemoryManager>) {
        use crate::builtin_builder::{IntrinsicContext, IntrinsicObject};

        let ctx = IntrinsicContext::for_init_core(mm.clone(), self.clone());

        // Keep this order aligned with wrapper dependencies.
        crate::intrinsics_impl::object::ObjectIntrinsic::init(&ctx);
        crate::intrinsics_impl::function::FunctionIntrinsic::init(&ctx);
        crate::intrinsics_impl::error::ErrorIntrinsic::init(&ctx);
        crate::intrinsics_impl::string::StringIntrinsic::init(&ctx);
        crate::intrinsics_impl::number::NumberIntrinsic::init(&ctx);
        crate::intrinsics_impl::boolean::BooleanIntrinsic::init(&ctx);
        crate::intrinsics_impl::symbol::SymbolIntrinsic::init(&ctx);
        crate::intrinsics_impl::bigint::BigIntIntrinsic::init(&ctx);
        crate::intrinsics_impl::date::DateIntrinsic::init(&ctx);
        crate::intrinsics_impl::iterator_helpers::IteratorIntrinsic::init(&ctx);
        crate::intrinsics_impl::array::ArrayIntrinsic::init(&ctx);
        crate::intrinsics_impl::map_set::MapSetIntrinsic::init(&ctx);
        crate::intrinsics_impl::promise::PromiseIntrinsic::init(&ctx);
        crate::intrinsics_impl::regexp::RegExpIntrinsic::init(&ctx);
        crate::intrinsics_impl::weak_ref::WeakRefIntrinsic::init(&ctx);
        crate::intrinsics_impl::generator::GeneratorIntrinsic::init(&ctx);
        crate::intrinsics_impl::typed_array::TypedArrayIntrinsic::init(&ctx);
        crate::web_api::abort_controller::AbortIntrinsic::init(&ctx);
    }

    /// Stage 4: expose selected intrinsic constructors and namespaces on `global`.
    pub fn install_on_global(&self, global: GcRef<JsObject>, mm: &Arc<MemoryManager>) {
        use crate::builtin_builder::{IntrinsicContext, IntrinsicObject};

        let ctx = IntrinsicContext::new(mm.clone(), global, self.clone());

        crate::intrinsics_impl::object::ObjectIntrinsic::init(&ctx);
        crate::intrinsics_impl::function::FunctionIntrinsic::init(&ctx);
        crate::intrinsics_impl::error::ErrorIntrinsic::init(&ctx);
        crate::intrinsics_impl::string::StringIntrinsic::init(&ctx);
        crate::intrinsics_impl::number::NumberIntrinsic::init(&ctx);
        crate::intrinsics_impl::boolean::BooleanIntrinsic::init(&ctx);
        crate::intrinsics_impl::symbol::SymbolIntrinsic::init(&ctx);
        crate::intrinsics_impl::bigint::BigIntIntrinsic::init(&ctx);
        crate::intrinsics_impl::date::DateIntrinsic::init(&ctx);
        crate::intrinsics_impl::iterator_helpers::IteratorIntrinsic::init(&ctx);
        crate::intrinsics_impl::generator::GeneratorIntrinsic::init(&ctx);
        crate::intrinsics_impl::array::ArrayIntrinsic::init(&ctx);
        crate::intrinsics_impl::map_set::MapSetIntrinsic::init(&ctx);
        crate::intrinsics_impl::promise::PromiseIntrinsic::init(&ctx);
        crate::intrinsics_impl::regexp::RegExpIntrinsic::init(&ctx);
        crate::intrinsics_impl::proxy::ProxyIntrinsic::init(&ctx);
        crate::intrinsics_impl::typed_array::TypedArrayIntrinsic::init(&ctx);
        crate::intrinsics_impl::weak_ref::WeakRefIntrinsic::init(&ctx);
        crate::web_api::abort_controller::AbortIntrinsic::init(&ctx);
        crate::intrinsics_impl::math::MathNamespace::init(&ctx);
        crate::intrinsics_impl::reflect::ReflectNamespace::init(&ctx);
        crate::intrinsics_impl::json::JsonNamespace::init(&ctx);
        crate::web_api::intl::IntlNamespace::init(&ctx);
        crate::intrinsics_impl::temporal::TemporalNamespace::init(&ctx);
    }
}
