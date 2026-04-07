//! Runtime-owned intrinsic registry for the new VM.
//!
//! This module defines the lifecycle and root ownership model for intrinsic
//! objects. The implementation is intentionally small: it allocates the first
//! stable root set, tracks lifecycle staging, and exposes explicit root
//! enumeration for future GC integration and builder-driven bootstrap.

mod array_class;
mod arraybuffer_class;
pub(crate) mod async_generator_class;
mod atomics;
mod bigint_class;
mod boolean_class;
mod dataview_class;
mod date_class;
mod error_class;
mod eval;
mod function_class;
mod generator_class;
mod install;
mod intl;
mod iterator_class;
mod json;
mod map_set_class;
mod math;
mod number_class;
mod object_class;
mod promise_class;
mod proxy_class;
mod reflect;
mod regexp_class;
mod sharedarraybuffer_class;
mod species_support;
mod string_class;
mod symbol_class;
mod temporal;
pub(crate) mod timer_globals;
pub(crate) mod typedarray_class;
mod weakmap_weakset_class;
mod weakref_class;

pub(crate) use boolean_class::box_boolean_object;
pub(crate) use generator_class::GeneratorResumeKind;
pub(crate) use iterator_class::create_iter_result_object;
pub(crate) use number_class::box_number_object;
pub(crate) use symbol_class::{box_symbol_object, symbol_descriptive_string};

use crate::host::NativeFunctionRegistry;
use crate::object::{ObjectError, ObjectHandle, ObjectHeap};
use crate::property::PropertyNameRegistry;
use crate::value::RegisterValue;
use install::{IntrinsicInstallContext, IntrinsicInstaller};

/// Stable well-known symbol identifiers owned by the intrinsic registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WellKnownSymbol {
    Iterator,
    AsyncIterator,
    ToStringTag,
    Species,
    Dispose,
    AsyncDispose,
    HasInstance,
    IsConcatSpreadable,
    Match,
    MatchAll,
    Replace,
    Search,
    Split,
    ToPrimitive,
    Unscopables,
}

impl WellKnownSymbol {
    /// Returns the stable numeric identifier of the symbol.
    #[must_use]
    pub const fn stable_id(self) -> u32 {
        match self {
            Self::Iterator => 1,
            Self::AsyncIterator => 2,
            Self::ToStringTag => 3,
            Self::Species => 4,
            Self::Dispose => 5,
            Self::AsyncDispose => 6,
            Self::HasInstance => 7,
            Self::IsConcatSpreadable => 8,
            Self::Match => 9,
            Self::MatchAll => 10,
            Self::Replace => 11,
            Self::Search => 12,
            Self::Split => 13,
            Self::ToPrimitive => 14,
            Self::Unscopables => 15,
        }
    }

    /// Returns the spec-visible description of the symbol.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Iterator => "Symbol.iterator",
            Self::AsyncIterator => "Symbol.asyncIterator",
            Self::ToStringTag => "Symbol.toStringTag",
            Self::Species => "Symbol.species",
            Self::Dispose => "Symbol.dispose",
            Self::AsyncDispose => "Symbol.asyncDispose",
            Self::HasInstance => "Symbol.hasInstance",
            Self::IsConcatSpreadable => "Symbol.isConcatSpreadable",
            Self::Match => "Symbol.match",
            Self::MatchAll => "Symbol.matchAll",
            Self::Replace => "Symbol.replace",
            Self::Search => "Symbol.search",
            Self::Split => "Symbol.split",
            Self::ToPrimitive => "Symbol.toPrimitive",
            Self::Unscopables => "Symbol.unscopables",
        }
    }
}

/// One root entry owned by [`VmIntrinsics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntrinsicRoot {
    Object(ObjectHandle),
    Symbol(WellKnownSymbol),
}

/// Shared list of ECMAScript globals installed by the new-VM intrinsic bootstrap.
pub const CORE_INTRINSIC_GLOBAL_NAMES: &[&str] = &[
    "Object",
    "Function",
    "Array",
    "ArrayBuffer",
    "Atomics",
    "BigInt",
    "SharedArrayBuffer",
    "String",
    "Number",
    "Boolean",
    "Math",
    "Reflect",
    "JSON",
    "Error",
    "TypeError",
    "RangeError",
    "ReferenceError",
    "SyntaxError",
    "URIError",
    "EvalError",
    "AggregateError",
    "Symbol",
    "Date",
    "RegExp",
    "Map",
    "Set",
    "WeakMap",
    "WeakSet",
    "WeakRef",
    "FinalizationRegistry",
    "Promise",
    "Proxy",
    "Temporal",
    "Int8Array",
    "Uint8Array",
    "Uint8ClampedArray",
    "Int16Array",
    "Uint16Array",
    "Int32Array",
    "Uint32Array",
    "Float32Array",
    "Float64Array",
    "BigInt64Array",
    "BigUint64Array",
    "isNaN",
    "isFinite",
    "parseInt",
    "parseFloat",
    "eval",
    "encodeURI",
    "encodeURIComponent",
    "decodeURI",
    "decodeURIComponent",
    "globalThis",
    "console",
    "$262",
];

/// Lifecycle stage of the intrinsic registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntrinsicsStage {
    Allocated,
    Wired,
    Initialized,
    Installed,
}

/// Errors produced while advancing the intrinsic lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IntrinsicsError {
    InvalidLifecycleStage,
    Heap(ObjectError),
    UnsupportedAccessorInstallation { js_name: Box<str> },
}

impl core::fmt::Display for IntrinsicsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidLifecycleStage => {
                f.write_str("intrinsics lifecycle advanced in an invalid order")
            }
            Self::Heap(error) => write!(f, "intrinsics heap operation failed: {error:?}"),
            Self::UnsupportedAccessorInstallation { js_name } => write!(
                f,
                "intrinsics bootstrap does not yet install accessor member '{js_name}'"
            ),
        }
    }
}

impl std::error::Error for IntrinsicsError {}

impl From<ObjectError> for IntrinsicsError {
    fn from(value: ObjectError) -> Self {
        Self::Heap(value)
    }
}

/// Runtime-owned handles to intrinsic root objects and well-known symbols.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmIntrinsics {
    stage: IntrinsicsStage,
    global_object: ObjectHandle,
    math_namespace: Option<ObjectHandle>,
    object_prototype: ObjectHandle,
    function_prototype: ObjectHandle,
    string_constructor: ObjectHandle,
    symbol_constructor: ObjectHandle,
    number_constructor: ObjectHandle,
    boolean_constructor: ObjectHandle,
    date_constructor: ObjectHandle,
    object_constructor: ObjectHandle,
    function_constructor: ObjectHandle,
    array_constructor: ObjectHandle,
    array_prototype: ObjectHandle,
    array_buffer_constructor: ObjectHandle,
    array_buffer_prototype: ObjectHandle,
    bigint_constructor: ObjectHandle,
    bigint_prototype: ObjectHandle,
    shared_array_buffer_constructor: ObjectHandle,
    shared_array_buffer_prototype: ObjectHandle,
    data_view_constructor: ObjectHandle,
    data_view_prototype: ObjectHandle,
    // %TypedArray% (§23.2)
    pub(crate) typed_array_base_constructor: ObjectHandle,
    pub(crate) typed_array_base_prototype: ObjectHandle,
    // Concrete TypedArray constructors + prototypes (11 pairs)
    pub(crate) int8_array_constructor: ObjectHandle,
    pub(crate) int8_array_prototype: ObjectHandle,
    pub(crate) uint8_array_constructor: ObjectHandle,
    pub(crate) uint8_array_prototype: ObjectHandle,
    pub(crate) uint8_clamped_array_constructor: ObjectHandle,
    pub(crate) uint8_clamped_array_prototype: ObjectHandle,
    pub(crate) int16_array_constructor: ObjectHandle,
    pub(crate) int16_array_prototype: ObjectHandle,
    pub(crate) uint16_array_constructor: ObjectHandle,
    pub(crate) uint16_array_prototype: ObjectHandle,
    pub(crate) int32_array_constructor: ObjectHandle,
    pub(crate) int32_array_prototype: ObjectHandle,
    pub(crate) uint32_array_constructor: ObjectHandle,
    pub(crate) uint32_array_prototype: ObjectHandle,
    pub(crate) float32_array_constructor: ObjectHandle,
    pub(crate) float32_array_prototype: ObjectHandle,
    pub(crate) float64_array_constructor: ObjectHandle,
    pub(crate) float64_array_prototype: ObjectHandle,
    pub(crate) bigint64_array_constructor: ObjectHandle,
    pub(crate) bigint64_array_prototype: ObjectHandle,
    pub(crate) biguint64_array_constructor: ObjectHandle,
    pub(crate) biguint64_array_prototype: ObjectHandle,
    string_prototype: ObjectHandle,
    symbol_prototype: ObjectHandle,
    number_prototype: ObjectHandle,
    boolean_prototype: ObjectHandle,
    date_prototype: ObjectHandle,
    proxy_constructor: ObjectHandle,
    namespace_roots: Vec<ObjectHandle>,
    reflect_namespace: Option<ObjectHandle>,
    json_namespace: Option<ObjectHandle>,
    atomics_namespace: Option<ObjectHandle>,
    well_known_symbols: [WellKnownSymbol; 15],
    // Error hierarchy
    pub(crate) error_prototype: ObjectHandle,
    pub(crate) error_constructor: ObjectHandle,
    pub(crate) type_error_prototype: ObjectHandle,
    pub(crate) type_error_constructor: ObjectHandle,
    pub(crate) reference_error_prototype: ObjectHandle,
    pub(crate) reference_error_constructor: ObjectHandle,
    pub(crate) range_error_prototype: ObjectHandle,
    pub(crate) range_error_constructor: ObjectHandle,
    pub(crate) syntax_error_prototype: ObjectHandle,
    pub(crate) syntax_error_constructor: ObjectHandle,
    pub(crate) uri_error_prototype: ObjectHandle,
    pub(crate) uri_error_constructor: ObjectHandle,
    pub(crate) eval_error_prototype: ObjectHandle,
    pub(crate) eval_error_constructor: ObjectHandle,
    pub(crate) aggregate_error_prototype: ObjectHandle,
    pub(crate) aggregate_error_constructor: ObjectHandle,
    // Map / Set
    pub(crate) map_constructor: ObjectHandle,
    pub(crate) map_prototype: ObjectHandle,
    pub(crate) set_constructor: ObjectHandle,
    pub(crate) set_prototype: ObjectHandle,
    // Promise
    pub(crate) promise_constructor: ObjectHandle,
    pub(crate) promise_prototype: ObjectHandle,
    // WeakMap / WeakSet (§24.3, §24.4)
    pub(crate) weakmap_constructor: ObjectHandle,
    pub(crate) weakmap_prototype: ObjectHandle,
    pub(crate) weakset_constructor: ObjectHandle,
    pub(crate) weakset_prototype: ObjectHandle,
    // WeakRef / FinalizationRegistry (§26.1, §26.2)
    pub(crate) weakref_constructor: ObjectHandle,
    pub(crate) weakref_prototype: ObjectHandle,
    pub(crate) finalization_registry_constructor: ObjectHandle,
    pub(crate) finalization_registry_prototype: ObjectHandle,
    // Iterator prototypes (§27.1.2, §23.1.5, §22.1.5, §24.1.5, §24.2.5)
    pub(crate) iterator_constructor: Option<ObjectHandle>,
    pub(crate) iterator_prototype: ObjectHandle,
    pub(crate) async_iterator_prototype: ObjectHandle,
    pub(crate) array_iterator_prototype: ObjectHandle,
    pub(crate) string_iterator_prototype: ObjectHandle,
    pub(crate) map_iterator_prototype: ObjectHandle,
    pub(crate) set_iterator_prototype: ObjectHandle,
    // Generator (§27.3, §27.5)
    pub(crate) generator_function_prototype: ObjectHandle,
    pub(crate) generator_prototype: ObjectHandle,
    // Async Generator (§27.4, §27.6)
    pub(crate) async_generator_function_prototype: ObjectHandle,
    pub(crate) async_generator_prototype: ObjectHandle,
    // RegExp (§22.2)
    pub(crate) regexp_constructor: ObjectHandle,
    pub(crate) regexp_prototype: ObjectHandle,
    // Intl (ECMA-402)
    pub(crate) intl_namespace: ObjectHandle,
    pub(crate) intl_collator_constructor: ObjectHandle,
    pub(crate) intl_collator_prototype: ObjectHandle,
    pub(crate) intl_number_format_constructor: ObjectHandle,
    pub(crate) intl_number_format_prototype: ObjectHandle,
    pub(crate) intl_plural_rules_constructor: ObjectHandle,
    pub(crate) intl_plural_rules_prototype: ObjectHandle,
    pub(crate) intl_locale_constructor: ObjectHandle,
    pub(crate) intl_locale_prototype: ObjectHandle,
    pub(crate) intl_date_time_format_constructor: ObjectHandle,
    pub(crate) intl_date_time_format_prototype: ObjectHandle,
    pub(crate) intl_list_format_constructor: ObjectHandle,
    pub(crate) intl_list_format_prototype: ObjectHandle,
    pub(crate) intl_segmenter_constructor: ObjectHandle,
    pub(crate) intl_segmenter_prototype: ObjectHandle,
    pub(crate) intl_segments_prototype: ObjectHandle,
    pub(crate) intl_segment_iterator_prototype: ObjectHandle,
    pub(crate) intl_display_names_constructor: ObjectHandle,
    pub(crate) intl_display_names_prototype: ObjectHandle,
    pub(crate) intl_relative_time_format_constructor: ObjectHandle,
    pub(crate) intl_relative_time_format_prototype: ObjectHandle,
    // §10.2.4 %ThrowTypeError% — shared accessor function for strict arguments
    pub(crate) throw_type_error_function: Option<ObjectHandle>,
    // Temporal (proposal-temporal, Stage 4)
    pub(crate) temporal_namespace: ObjectHandle,
    pub(crate) temporal_instant_constructor: ObjectHandle,
    pub(crate) temporal_instant_prototype: ObjectHandle,
    pub(crate) temporal_duration_constructor: ObjectHandle,
    pub(crate) temporal_duration_prototype: ObjectHandle,
    pub(crate) temporal_plain_date_constructor: ObjectHandle,
    pub(crate) temporal_plain_date_prototype: ObjectHandle,
    pub(crate) temporal_plain_time_constructor: ObjectHandle,
    pub(crate) temporal_plain_time_prototype: ObjectHandle,
    pub(crate) temporal_plain_date_time_constructor: ObjectHandle,
    pub(crate) temporal_plain_date_time_prototype: ObjectHandle,
    pub(crate) temporal_plain_year_month_constructor: ObjectHandle,
    pub(crate) temporal_plain_year_month_prototype: ObjectHandle,
    pub(crate) temporal_plain_month_day_constructor: ObjectHandle,
    pub(crate) temporal_plain_month_day_prototype: ObjectHandle,
    pub(crate) temporal_zoned_date_time_constructor: ObjectHandle,
    pub(crate) temporal_zoned_date_time_prototype: ObjectHandle,
}

/// §10.2.4 %ThrowTypeError% native implementation.
/// Always throws a TypeError, used for strict arguments callee/caller.
/// Spec: <https://tc39.es/ecma262/#sec-%throwtypeerror%>
fn throw_type_error_native(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, crate::descriptors::VmNativeCallError> {
    let error_handle = runtime.alloc_type_error(
        "'caller', 'callee', and 'arguments' properties may not be accessed on strict mode functions or the arguments objects for calls to them",
    ).map_err(|e| crate::descriptors::VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Err(crate::descriptors::VmNativeCallError::Thrown(
        RegisterValue::from_object_handle(error_handle.0),
    ))
}

impl VmIntrinsics {
    /// Allocates the minimal intrinsic root set.
    pub fn allocate(heap: &mut ObjectHeap) -> Self {
        let global_object = heap.alloc_object();
        let object_prototype = heap.alloc_object();
        let function_prototype = heap.alloc_object();
        let string_constructor = heap.alloc_object();
        let symbol_constructor = heap.alloc_object();
        let number_constructor = heap.alloc_object();
        let boolean_constructor = heap.alloc_object();
        let date_constructor = heap.alloc_object();
        let object_constructor = heap.alloc_object();
        let function_constructor = heap.alloc_object();
        let array_constructor = heap.alloc_object();
        let array_prototype = heap.alloc_object();
        let array_buffer_constructor = heap.alloc_object();
        let array_buffer_prototype = heap.alloc_object();
        let bigint_constructor = heap.alloc_object();
        let bigint_prototype = heap.alloc_object();
        let shared_array_buffer_constructor = heap.alloc_object();
        let shared_array_buffer_prototype = heap.alloc_object();
        let data_view_constructor = heap.alloc_object();
        let data_view_prototype = heap.alloc_object();
        let typed_array_base_constructor = heap.alloc_object();
        let typed_array_base_prototype = heap.alloc_object();
        let int8_array_constructor = heap.alloc_object();
        let int8_array_prototype = heap.alloc_object();
        let uint8_array_constructor = heap.alloc_object();
        let uint8_array_prototype = heap.alloc_object();
        let uint8_clamped_array_constructor = heap.alloc_object();
        let uint8_clamped_array_prototype = heap.alloc_object();
        let int16_array_constructor = heap.alloc_object();
        let int16_array_prototype = heap.alloc_object();
        let uint16_array_constructor = heap.alloc_object();
        let uint16_array_prototype = heap.alloc_object();
        let int32_array_constructor = heap.alloc_object();
        let int32_array_prototype = heap.alloc_object();
        let uint32_array_constructor = heap.alloc_object();
        let uint32_array_prototype = heap.alloc_object();
        let float32_array_constructor = heap.alloc_object();
        let float32_array_prototype = heap.alloc_object();
        let float64_array_constructor = heap.alloc_object();
        let float64_array_prototype = heap.alloc_object();
        let bigint64_array_constructor = heap.alloc_object();
        let bigint64_array_prototype = heap.alloc_object();
        let biguint64_array_constructor = heap.alloc_object();
        let biguint64_array_prototype = heap.alloc_object();
        let string_prototype = heap.alloc_object();
        let symbol_prototype = heap.alloc_object();
        let number_prototype = heap.alloc_object();
        let boolean_prototype = heap.alloc_object();
        let date_prototype = heap.alloc_object();
        let proxy_constructor = heap.alloc_object();
        let error_prototype = heap.alloc_object();
        let error_constructor = heap.alloc_object();
        let type_error_prototype = heap.alloc_object();
        let type_error_constructor = heap.alloc_object();
        let reference_error_prototype = heap.alloc_object();
        let reference_error_constructor = heap.alloc_object();
        let range_error_prototype = heap.alloc_object();
        let range_error_constructor = heap.alloc_object();
        let syntax_error_prototype = heap.alloc_object();
        let syntax_error_constructor = heap.alloc_object();
        let uri_error_prototype = heap.alloc_object();
        let uri_error_constructor = heap.alloc_object();
        let eval_error_prototype = heap.alloc_object();
        let eval_error_constructor = heap.alloc_object();
        let aggregate_error_prototype = heap.alloc_object();
        let aggregate_error_constructor = heap.alloc_object();
        let map_constructor = heap.alloc_object();
        let map_prototype = heap.alloc_object();
        let set_constructor = heap.alloc_object();
        let set_prototype = heap.alloc_object();
        let promise_constructor = heap.alloc_object();
        let promise_prototype = heap.alloc_object();
        let weakmap_constructor = heap.alloc_object();
        let weakmap_prototype = heap.alloc_object();
        let weakset_constructor = heap.alloc_object();
        let weakset_prototype = heap.alloc_object();
        let weakref_constructor = heap.alloc_object();
        let weakref_prototype = heap.alloc_object();
        let finalization_registry_constructor = heap.alloc_object();
        let finalization_registry_prototype = heap.alloc_object();
        let iterator_prototype = heap.alloc_object();
        let async_iterator_prototype = heap.alloc_object();
        let array_iterator_prototype = heap.alloc_object();
        let string_iterator_prototype = heap.alloc_object();
        let map_iterator_prototype = heap.alloc_object();
        let set_iterator_prototype = heap.alloc_object();
        let generator_function_prototype = heap.alloc_object();
        let generator_prototype = heap.alloc_object();
        let async_generator_function_prototype = heap.alloc_object();
        let async_generator_prototype = heap.alloc_object();
        let regexp_constructor = heap.alloc_object();
        let regexp_prototype = heap.alloc_object();
        // Intl (ECMA-402)
        let intl_namespace = heap.alloc_object();
        let intl_collator_constructor = heap.alloc_object();
        let intl_collator_prototype = heap.alloc_object();
        let intl_number_format_constructor = heap.alloc_object();
        let intl_number_format_prototype = heap.alloc_object();
        let intl_plural_rules_constructor = heap.alloc_object();
        let intl_plural_rules_prototype = heap.alloc_object();
        let intl_locale_constructor = heap.alloc_object();
        let intl_locale_prototype = heap.alloc_object();
        let intl_date_time_format_constructor = heap.alloc_object();
        let intl_date_time_format_prototype = heap.alloc_object();
        let intl_list_format_constructor = heap.alloc_object();
        let intl_list_format_prototype = heap.alloc_object();
        let intl_segmenter_constructor = heap.alloc_object();
        let intl_segmenter_prototype = heap.alloc_object();
        let intl_segments_prototype = heap.alloc_object();
        let intl_segment_iterator_prototype = heap.alloc_object();
        let intl_display_names_constructor = heap.alloc_object();
        let intl_display_names_prototype = heap.alloc_object();
        let intl_relative_time_format_constructor = heap.alloc_object();
        let intl_relative_time_format_prototype = heap.alloc_object();
        // Temporal (proposal-temporal)
        let temporal_namespace = heap.alloc_object();
        let temporal_instant_constructor = heap.alloc_object();
        let temporal_instant_prototype = heap.alloc_object();
        let temporal_duration_constructor = heap.alloc_object();
        let temporal_duration_prototype = heap.alloc_object();
        let temporal_plain_date_constructor = heap.alloc_object();
        let temporal_plain_date_prototype = heap.alloc_object();
        let temporal_plain_time_constructor = heap.alloc_object();
        let temporal_plain_time_prototype = heap.alloc_object();
        let temporal_plain_date_time_constructor = heap.alloc_object();
        let temporal_plain_date_time_prototype = heap.alloc_object();
        let temporal_plain_year_month_constructor = heap.alloc_object();
        let temporal_plain_year_month_prototype = heap.alloc_object();
        let temporal_plain_month_day_constructor = heap.alloc_object();
        let temporal_plain_month_day_prototype = heap.alloc_object();
        let temporal_zoned_date_time_constructor = heap.alloc_object();
        let temporal_zoned_date_time_prototype = heap.alloc_object();

        Self {
            stage: IntrinsicsStage::Allocated,
            global_object,
            math_namespace: None,
            object_prototype,
            function_prototype,
            string_constructor,
            symbol_constructor,
            number_constructor,
            boolean_constructor,
            date_constructor,
            object_constructor,
            function_constructor,
            array_constructor,
            array_prototype,
            array_buffer_constructor,
            array_buffer_prototype,
            bigint_constructor,
            bigint_prototype,
            shared_array_buffer_constructor,
            shared_array_buffer_prototype,
            data_view_constructor,
            data_view_prototype,
            typed_array_base_constructor,
            typed_array_base_prototype,
            int8_array_constructor,
            int8_array_prototype,
            uint8_array_constructor,
            uint8_array_prototype,
            uint8_clamped_array_constructor,
            uint8_clamped_array_prototype,
            int16_array_constructor,
            int16_array_prototype,
            uint16_array_constructor,
            uint16_array_prototype,
            int32_array_constructor,
            int32_array_prototype,
            uint32_array_constructor,
            uint32_array_prototype,
            float32_array_constructor,
            float32_array_prototype,
            float64_array_constructor,
            float64_array_prototype,
            bigint64_array_constructor,
            bigint64_array_prototype,
            biguint64_array_constructor,
            biguint64_array_prototype,
            string_prototype,
            symbol_prototype,
            number_prototype,
            boolean_prototype,
            date_prototype,
            proxy_constructor,
            namespace_roots: Vec::new(),
            reflect_namespace: None,
            json_namespace: None,
            atomics_namespace: None,
            well_known_symbols: [
                WellKnownSymbol::Iterator,
                WellKnownSymbol::AsyncIterator,
                WellKnownSymbol::ToStringTag,
                WellKnownSymbol::Species,
                WellKnownSymbol::Dispose,
                WellKnownSymbol::AsyncDispose,
                WellKnownSymbol::HasInstance,
                WellKnownSymbol::IsConcatSpreadable,
                WellKnownSymbol::Match,
                WellKnownSymbol::MatchAll,
                WellKnownSymbol::Replace,
                WellKnownSymbol::Search,
                WellKnownSymbol::Split,
                WellKnownSymbol::ToPrimitive,
                WellKnownSymbol::Unscopables,
            ],
            error_prototype,
            error_constructor,
            type_error_prototype,
            type_error_constructor,
            reference_error_prototype,
            reference_error_constructor,
            range_error_prototype,
            range_error_constructor,
            syntax_error_prototype,
            syntax_error_constructor,
            uri_error_prototype,
            uri_error_constructor,
            eval_error_prototype,
            eval_error_constructor,
            aggregate_error_prototype,
            aggregate_error_constructor,
            map_constructor,
            map_prototype,
            set_constructor,
            set_prototype,
            promise_constructor,
            promise_prototype,
            weakmap_constructor,
            weakmap_prototype,
            weakset_constructor,
            weakset_prototype,
            weakref_constructor,
            weakref_prototype,
            finalization_registry_constructor,
            finalization_registry_prototype,
            iterator_constructor: None,
            iterator_prototype,
            async_iterator_prototype,
            array_iterator_prototype,
            string_iterator_prototype,
            map_iterator_prototype,
            set_iterator_prototype,
            generator_function_prototype,
            generator_prototype,
            async_generator_function_prototype,
            async_generator_prototype,
            regexp_constructor,
            regexp_prototype,
            intl_namespace,
            intl_collator_constructor,
            intl_collator_prototype,
            intl_number_format_constructor,
            intl_number_format_prototype,
            intl_plural_rules_constructor,
            intl_plural_rules_prototype,
            intl_locale_constructor,
            intl_locale_prototype,
            intl_date_time_format_constructor,
            intl_date_time_format_prototype,
            intl_list_format_constructor,
            intl_list_format_prototype,
            intl_segmenter_constructor,
            intl_segmenter_prototype,
            intl_segments_prototype,
            intl_segment_iterator_prototype,
            intl_display_names_constructor,
            intl_display_names_prototype,
            intl_relative_time_format_constructor,
            intl_relative_time_format_prototype,
            throw_type_error_function: None,
            temporal_namespace,
            temporal_instant_constructor,
            temporal_instant_prototype,
            temporal_duration_constructor,
            temporal_duration_prototype,
            temporal_plain_date_constructor,
            temporal_plain_date_prototype,
            temporal_plain_time_constructor,
            temporal_plain_time_prototype,
            temporal_plain_date_time_constructor,
            temporal_plain_date_time_prototype,
            temporal_plain_year_month_constructor,
            temporal_plain_year_month_prototype,
            temporal_plain_month_day_constructor,
            temporal_plain_month_day_prototype,
            temporal_zoned_date_time_constructor,
            temporal_zoned_date_time_prototype,
        }
    }

    /// Performs prototype-chain wiring for the allocated intrinsic objects.
    pub fn wire_prototype_chains(&mut self, heap: &mut ObjectHeap) -> Result<(), IntrinsicsError> {
        if self.stage != IntrinsicsStage::Allocated {
            return Err(IntrinsicsError::InvalidLifecycleStage);
        }
        heap.set_prototype(self.global_object, Some(self.object_prototype))?;
        heap.set_prototype(self.object_prototype, None)?;
        heap.set_prototype(self.function_prototype, Some(self.object_prototype))?;
        heap.set_prototype(self.string_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.symbol_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.number_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.boolean_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.date_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.object_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.function_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.array_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.array_prototype, Some(self.object_prototype))?;
        heap.set_prototype(self.array_buffer_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.array_buffer_prototype, Some(self.object_prototype))?;
        heap.set_prototype(self.bigint_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.bigint_prototype, Some(self.object_prototype))?;
        heap.set_prototype(
            self.shared_array_buffer_constructor,
            Some(self.function_prototype),
        )?;
        heap.set_prototype(
            self.shared_array_buffer_prototype,
            Some(self.object_prototype),
        )?;
        heap.set_prototype(self.data_view_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.data_view_prototype, Some(self.object_prototype))?;
        // %TypedArray% (§23.2): base constructor → Function.prototype, base prototype → Object.prototype
        heap.set_prototype(
            self.typed_array_base_constructor,
            Some(self.function_prototype),
        )?;
        heap.set_prototype(self.typed_array_base_prototype, Some(self.object_prototype))?;
        // Concrete TypedArray prototypes are wired in the installer (→ %TypedArray%.prototype).
        heap.set_prototype(self.string_prototype, Some(self.object_prototype))?;
        heap.set_prototype(self.symbol_prototype, Some(self.object_prototype))?;
        heap.set_prototype(self.number_prototype, Some(self.object_prototype))?;
        heap.set_prototype(self.boolean_prototype, Some(self.object_prototype))?;
        heap.set_prototype(self.date_prototype, Some(self.object_prototype))?;
        heap.set_prototype(self.proxy_constructor, Some(self.function_prototype))?;
        // Error hierarchy: Error.prototype → Object.prototype,
        // NativeError.prototype → Error.prototype
        heap.set_prototype(self.error_prototype, Some(self.object_prototype))?;
        heap.set_prototype(self.error_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.type_error_prototype, Some(self.error_prototype))?;
        heap.set_prototype(self.type_error_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.reference_error_prototype, Some(self.error_prototype))?;
        heap.set_prototype(
            self.reference_error_constructor,
            Some(self.function_prototype),
        )?;
        heap.set_prototype(self.range_error_prototype, Some(self.error_prototype))?;
        heap.set_prototype(self.range_error_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.syntax_error_prototype, Some(self.error_prototype))?;
        heap.set_prototype(self.syntax_error_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.uri_error_prototype, Some(self.error_prototype))?;
        heap.set_prototype(self.uri_error_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.eval_error_prototype, Some(self.error_prototype))?;
        heap.set_prototype(self.eval_error_constructor, Some(self.function_prototype))?;
        // AggregateError (§20.5.7)
        heap.set_prototype(self.aggregate_error_prototype, Some(self.error_prototype))?;
        heap.set_prototype(
            self.aggregate_error_constructor,
            Some(self.function_prototype),
        )?;
        // Promise: constructor → Function.prototype, prototype → Object.prototype
        heap.set_prototype(self.promise_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.promise_prototype, Some(self.object_prototype))?;
        // WeakMap/WeakSet (§24.3, §24.4)
        heap.set_prototype(self.weakmap_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.weakmap_prototype, Some(self.object_prototype))?;
        heap.set_prototype(self.weakset_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.weakset_prototype, Some(self.object_prototype))?;
        // WeakRef (§26.1): WeakRef → Function.prototype, WeakRef.prototype → Object.prototype
        heap.set_prototype(self.weakref_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.weakref_prototype, Some(self.object_prototype))?;
        // FinalizationRegistry (§26.2)
        heap.set_prototype(
            self.finalization_registry_constructor,
            Some(self.function_prototype),
        )?;
        heap.set_prototype(
            self.finalization_registry_prototype,
            Some(self.object_prototype),
        )?;
        // Iterator prototypes (§27.1.2): %IteratorPrototype% → %Object.prototype%
        // Concrete iterator prototypes → %IteratorPrototype%
        heap.set_prototype(self.iterator_prototype, Some(self.object_prototype))?;
        // §27.1.4 %AsyncIteratorPrototype% → %Object.prototype%
        heap.set_prototype(self.async_iterator_prototype, Some(self.object_prototype))?;
        heap.set_prototype(self.array_iterator_prototype, Some(self.iterator_prototype))?;
        heap.set_prototype(
            self.string_iterator_prototype,
            Some(self.iterator_prototype),
        )?;
        heap.set_prototype(self.map_iterator_prototype, Some(self.iterator_prototype))?;
        heap.set_prototype(self.set_iterator_prototype, Some(self.iterator_prototype))?;
        // Generator (§27.3, §27.5):
        // %GeneratorFunction.prototype% → %Function.prototype%
        // %GeneratorPrototype% → %IteratorPrototype% (generators are iterators!)
        heap.set_prototype(
            self.generator_function_prototype,
            Some(self.function_prototype),
        )?;
        heap.set_prototype(self.generator_prototype, Some(self.iterator_prototype))?;
        // Async Generator (§27.4, §27.6):
        // %AsyncGeneratorFunction.prototype% → %Function.prototype%
        // %AsyncGeneratorPrototype% → %AsyncIteratorPrototype%
        // Spec: <https://tc39.es/ecma262/#sec-asyncgeneratorfunction-objects>
        heap.set_prototype(
            self.async_generator_function_prototype,
            Some(self.function_prototype),
        )?;
        heap.set_prototype(
            self.async_generator_prototype,
            Some(self.async_iterator_prototype),
        )?;
        // RegExp (§22.2): RegExp.prototype → Object.prototype
        heap.set_prototype(self.regexp_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.regexp_prototype, Some(self.object_prototype))?;
        // Intl (ECMA-402): namespace → Object.prototype, constructors → Function.prototype, prototypes → Object.prototype.
        heap.set_prototype(self.intl_namespace, Some(self.object_prototype))?;
        for ctor in [
            self.intl_collator_constructor,
            self.intl_number_format_constructor,
            self.intl_plural_rules_constructor,
            self.intl_locale_constructor,
            self.intl_date_time_format_constructor,
            self.intl_list_format_constructor,
            self.intl_segmenter_constructor,
            self.intl_display_names_constructor,
            self.intl_relative_time_format_constructor,
        ] {
            heap.set_prototype(ctor, Some(self.function_prototype))?;
        }
        for proto in [
            self.intl_collator_prototype,
            self.intl_number_format_prototype,
            self.intl_plural_rules_prototype,
            self.intl_locale_prototype,
            self.intl_date_time_format_prototype,
            self.intl_list_format_prototype,
            self.intl_segmenter_prototype,
            self.intl_segments_prototype,
            self.intl_segment_iterator_prototype,
            self.intl_display_names_prototype,
            self.intl_relative_time_format_prototype,
        ] {
            heap.set_prototype(proto, Some(self.object_prototype))?;
        }
        // Temporal (proposal-temporal): namespace → Object.prototype,
        // each constructor → Function.prototype, each prototype → Object.prototype.
        heap.set_prototype(self.temporal_namespace, Some(self.object_prototype))?;
        heap.set_prototype(
            self.temporal_instant_constructor,
            Some(self.function_prototype),
        )?;
        heap.set_prototype(self.temporal_instant_prototype, Some(self.object_prototype))?;
        heap.set_prototype(
            self.temporal_duration_constructor,
            Some(self.function_prototype),
        )?;
        heap.set_prototype(
            self.temporal_duration_prototype,
            Some(self.object_prototype),
        )?;
        heap.set_prototype(
            self.temporal_plain_date_constructor,
            Some(self.function_prototype),
        )?;
        heap.set_prototype(
            self.temporal_plain_date_prototype,
            Some(self.object_prototype),
        )?;
        heap.set_prototype(
            self.temporal_plain_time_constructor,
            Some(self.function_prototype),
        )?;
        heap.set_prototype(
            self.temporal_plain_time_prototype,
            Some(self.object_prototype),
        )?;
        heap.set_prototype(
            self.temporal_plain_date_time_constructor,
            Some(self.function_prototype),
        )?;
        heap.set_prototype(
            self.temporal_plain_date_time_prototype,
            Some(self.object_prototype),
        )?;
        heap.set_prototype(
            self.temporal_plain_year_month_constructor,
            Some(self.function_prototype),
        )?;
        heap.set_prototype(
            self.temporal_plain_year_month_prototype,
            Some(self.object_prototype),
        )?;
        heap.set_prototype(
            self.temporal_plain_month_day_constructor,
            Some(self.function_prototype),
        )?;
        heap.set_prototype(
            self.temporal_plain_month_day_prototype,
            Some(self.object_prototype),
        )?;
        heap.set_prototype(
            self.temporal_zoned_date_time_constructor,
            Some(self.function_prototype),
        )?;
        heap.set_prototype(
            self.temporal_zoned_date_time_prototype,
            Some(self.object_prototype),
        )?;
        self.stage = IntrinsicsStage::Wired;
        Ok(())
    }

    /// Populates intrinsic objects with core methods/properties.
    pub fn init_core(
        &mut self,
        heap: &mut ObjectHeap,
        property_names: &mut PropertyNameRegistry,
        native_functions: &mut NativeFunctionRegistry,
    ) -> Result<(), IntrinsicsError> {
        if self.stage != IntrinsicsStage::Wired {
            return Err(IntrinsicsError::InvalidLifecycleStage);
        }

        let mut cx = IntrinsicInstallContext::new(heap, property_names, native_functions);
        for installer in core_installers() {
            installer.init(self, &mut cx)?;
        }

        self.stage = IntrinsicsStage::Initialized;
        Ok(())
    }

    /// Installs initialized intrinsics on the global object.
    pub fn install_on_global(
        &mut self,
        heap: &mut ObjectHeap,
        property_names: &mut PropertyNameRegistry,
        native_functions: &mut NativeFunctionRegistry,
    ) -> Result<(), IntrinsicsError> {
        if self.stage != IntrinsicsStage::Initialized {
            return Err(IntrinsicsError::InvalidLifecycleStage);
        }

        let mut cx = IntrinsicInstallContext::new(heap, property_names, native_functions);
        for installer in core_installers() {
            installer.install_on_global(self, &mut cx)?;
        }

        // Install console object with log/warn/error/info/debug methods.
        {
            let console_obj = cx.alloc_intrinsic_object(Some(self.object_prototype))?;
            for binding in crate::console::console_bindings() {
                let function_desc = binding.function().clone();
                let host_fn = cx.native_functions.register(function_desc.clone());
                let handle = cx.alloc_intrinsic_host_function(host_fn, self.function_prototype)?;
                let prop = cx.property_names.intern(function_desc.js_name());
                cx.heap.set_property(
                    console_obj,
                    prop,
                    RegisterValue::from_object_handle(handle.0),
                )?;
            }
            let console_prop = cx.property_names.intern("console");
            cx.heap.set_property(
                self.global_object,
                console_prop,
                RegisterValue::from_object_handle(console_obj.0),
            )?;
        }

        // Install timer/microtask globals (setTimeout, setInterval, etc.)
        for binding in timer_globals::timer_global_bindings() {
            let function_desc = binding.function().clone();
            let host_fn = cx.native_functions.register(function_desc.clone());
            let handle = cx.alloc_intrinsic_host_function(host_fn, self.function_prototype)?;
            let prop = cx.property_names.intern(function_desc.js_name());
            cx.heap.set_property(
                self.global_object,
                prop,
                RegisterValue::from_object_handle(handle.0),
            )?;
        }

        // §19.1 globalThis — self-reference to the global object.
        // Spec: <https://tc39.es/ecma262/#sec-globalthis>
        {
            let global_this_prop = cx.property_names.intern("globalThis");
            cx.heap.set_property(
                self.global_object,
                global_this_prop,
                RegisterValue::from_object_handle(self.global_object.0),
            )?;
        }

        // Install global value properties (§19.1).
        {
            let nan_prop = cx.property_names.intern("NaN");
            cx.heap.set_property(
                self.global_object,
                nan_prop,
                RegisterValue::from_number(f64::NAN),
            )?;
            let inf_prop = cx.property_names.intern("Infinity");
            cx.heap.set_property(
                self.global_object,
                inf_prop,
                RegisterValue::from_number(f64::INFINITY),
            )?;
            let undef_prop = cx.property_names.intern("undefined");
            cx.heap
                .set_property(self.global_object, undef_prop, RegisterValue::undefined())?;
        }

        // Install global functions (§19.2): isNaN, isFinite, parseFloat, parseInt.
        for binding in number_class::global_number_function_bindings() {
            let length = binding.length();
            let name = binding.js_name().to_string();
            let host_fn = cx.native_functions.register(binding);
            let handle = cx.alloc_intrinsic_host_function(host_fn, self.function_prototype)?;
            install::install_function_length_name(handle, length, &name, &mut cx)?;
            let prop = cx.property_names.intern(&name);
            cx.heap.set_property(
                self.global_object,
                prop,
                RegisterValue::from_object_handle(handle.0),
            )?;
        }

        // Install global functions (§19.2): eval, encodeURI, encodeURIComponent,
        // decodeURI, decodeURIComponent.
        for binding in eval::global_eval_and_uri_bindings() {
            let length = binding.length();
            let name = binding.js_name().to_string();
            let host_fn = cx.native_functions.register(binding);
            let handle = cx.alloc_intrinsic_host_function(host_fn, self.function_prototype)?;
            install::install_function_length_name(handle, length, &name, &mut cx)?;
            let prop = cx.property_names.intern(&name);
            cx.heap.set_property(
                self.global_object,
                prop,
                RegisterValue::from_object_handle(handle.0),
            )?;
        }

        // §10.2.4 %ThrowTypeError% — shared intrinsic for strict arguments callee/caller.
        // Spec: <https://tc39.es/ecma262/#sec-%throwtypeerror%>
        {
            let desc = crate::descriptors::NativeFunctionDescriptor::method(
                "%ThrowTypeError%",
                0,
                throw_type_error_native,
            );
            let host_fn = cx.native_functions.register(desc);
            let handle = cx.alloc_intrinsic_host_function(host_fn, self.function_prototype)?;
            // §10.2.4 step 3: SetIntegrityLevel(thrower, "frozen")
            cx.heap.freeze(handle)?;
            self.throw_type_error_function = Some(handle);
        }

        self.stage = IntrinsicsStage::Installed;
        Ok(())
    }

    /// Returns the current lifecycle stage.
    #[must_use]
    pub const fn stage(&self) -> IntrinsicsStage {
        self.stage
    }

    /// Returns the global object root.
    #[must_use]
    pub const fn global_object(&self) -> ObjectHandle {
        self.global_object
    }

    /// Returns `%Object.prototype%`.
    #[must_use]
    pub const fn object_prototype(&self) -> ObjectHandle {
        self.object_prototype
    }

    /// Returns `%Function.prototype%`.
    #[must_use]
    pub const fn function_prototype(&self) -> ObjectHandle {
        self.function_prototype
    }

    /// Returns `%ThrowTypeError%` (§10.2.4).
    /// Used for strict arguments callee/caller accessors.
    #[must_use]
    pub fn throw_type_error_function(&self) -> Option<ObjectHandle> {
        self.throw_type_error_function
    }

    /// Returns `%String%`.
    #[must_use]
    pub const fn string_constructor(&self) -> ObjectHandle {
        self.string_constructor
    }

    /// Returns `%Symbol%`.
    #[must_use]
    pub const fn symbol_constructor(&self) -> ObjectHandle {
        self.symbol_constructor
    }

    /// Returns `%Number%`.
    #[must_use]
    pub const fn number_constructor(&self) -> ObjectHandle {
        self.number_constructor
    }

    /// Returns `%Boolean%`.
    #[must_use]
    pub const fn boolean_constructor(&self) -> ObjectHandle {
        self.boolean_constructor
    }

    /// Returns `%Date%`.
    #[must_use]
    pub const fn date_constructor(&self) -> ObjectHandle {
        self.date_constructor
    }

    /// Returns `%Object%`.
    #[must_use]
    pub const fn object_constructor(&self) -> ObjectHandle {
        self.object_constructor
    }

    /// Returns `%Function%`.
    #[must_use]
    pub const fn function_constructor(&self) -> ObjectHandle {
        self.function_constructor
    }

    /// Returns `%Array%`.
    #[must_use]
    pub const fn array_constructor(&self) -> ObjectHandle {
        self.array_constructor
    }

    /// Returns `%Array.prototype%`.
    #[must_use]
    pub const fn array_prototype(&self) -> ObjectHandle {
        self.array_prototype
    }

    /// Returns `%ArrayBuffer%`.
    #[must_use]
    pub const fn array_buffer_constructor(&self) -> ObjectHandle {
        self.array_buffer_constructor
    }

    /// Returns `%ArrayBuffer.prototype%`.
    #[must_use]
    pub const fn array_buffer_prototype(&self) -> ObjectHandle {
        self.array_buffer_prototype
    }

    /// Returns `%BigInt%` (the constructor function).
    #[must_use]
    pub const fn bigint_constructor(&self) -> ObjectHandle {
        self.bigint_constructor
    }

    /// Returns `%BigInt.prototype%`.
    #[must_use]
    pub const fn bigint_prototype(&self) -> ObjectHandle {
        self.bigint_prototype
    }

    /// Returns `%SharedArrayBuffer%`.
    #[must_use]
    pub const fn shared_array_buffer_constructor(&self) -> ObjectHandle {
        self.shared_array_buffer_constructor
    }

    /// Returns `%SharedArrayBuffer.prototype%`.
    #[must_use]
    pub const fn shared_array_buffer_prototype(&self) -> ObjectHandle {
        self.shared_array_buffer_prototype
    }

    /// Returns `%DataView%`.
    #[must_use]
    pub const fn data_view_constructor(&self) -> ObjectHandle {
        self.data_view_constructor
    }

    /// Returns `%DataView.prototype%`.
    #[must_use]
    pub const fn data_view_prototype(&self) -> ObjectHandle {
        self.data_view_prototype
    }

    /// Returns the (constructor, prototype) pair for a concrete TypedArray kind.
    #[must_use]
    pub fn typed_array_constructor_prototype(
        &self,
        kind: crate::object::TypedArrayKind,
    ) -> (ObjectHandle, ObjectHandle) {
        use crate::object::TypedArrayKind;
        match kind {
            TypedArrayKind::Int8 => (self.int8_array_constructor, self.int8_array_prototype),
            TypedArrayKind::Uint8 => (self.uint8_array_constructor, self.uint8_array_prototype),
            TypedArrayKind::Uint8Clamped => (
                self.uint8_clamped_array_constructor,
                self.uint8_clamped_array_prototype,
            ),
            TypedArrayKind::Int16 => (self.int16_array_constructor, self.int16_array_prototype),
            TypedArrayKind::Uint16 => (self.uint16_array_constructor, self.uint16_array_prototype),
            TypedArrayKind::Int32 => (self.int32_array_constructor, self.int32_array_prototype),
            TypedArrayKind::Uint32 => (self.uint32_array_constructor, self.uint32_array_prototype),
            TypedArrayKind::Float32 => {
                (self.float32_array_constructor, self.float32_array_prototype)
            }
            TypedArrayKind::Float64 => {
                (self.float64_array_constructor, self.float64_array_prototype)
            }
            TypedArrayKind::BigInt64 => (
                self.bigint64_array_constructor,
                self.bigint64_array_prototype,
            ),
            TypedArrayKind::BigUint64 => (
                self.biguint64_array_constructor,
                self.biguint64_array_prototype,
            ),
        }
    }

    /// Replaces a concrete TypedArray constructor handle (used during init).
    pub(crate) fn set_typed_array_constructor(
        &mut self,
        kind: crate::object::TypedArrayKind,
        handle: ObjectHandle,
    ) {
        use crate::object::TypedArrayKind;
        match kind {
            TypedArrayKind::Int8 => self.int8_array_constructor = handle,
            TypedArrayKind::Uint8 => self.uint8_array_constructor = handle,
            TypedArrayKind::Uint8Clamped => self.uint8_clamped_array_constructor = handle,
            TypedArrayKind::Int16 => self.int16_array_constructor = handle,
            TypedArrayKind::Uint16 => self.uint16_array_constructor = handle,
            TypedArrayKind::Int32 => self.int32_array_constructor = handle,
            TypedArrayKind::Uint32 => self.uint32_array_constructor = handle,
            TypedArrayKind::Float32 => self.float32_array_constructor = handle,
            TypedArrayKind::Float64 => self.float64_array_constructor = handle,
            TypedArrayKind::BigInt64 => self.bigint64_array_constructor = handle,
            TypedArrayKind::BigUint64 => self.biguint64_array_constructor = handle,
        }
    }

    /// Returns `%String.prototype%`.
    #[must_use]
    pub const fn string_prototype(&self) -> ObjectHandle {
        self.string_prototype
    }

    /// Returns `%Symbol.prototype%`.
    #[must_use]
    pub const fn symbol_prototype(&self) -> ObjectHandle {
        self.symbol_prototype
    }

    /// Returns `%Number.prototype%`.
    #[must_use]
    pub const fn number_prototype(&self) -> ObjectHandle {
        self.number_prototype
    }

    /// Returns `%Boolean.prototype%`.
    #[must_use]
    pub const fn boolean_prototype(&self) -> ObjectHandle {
        self.boolean_prototype
    }

    /// Returns `%Date.prototype%`.
    #[must_use]
    pub const fn date_prototype(&self) -> ObjectHandle {
        self.date_prototype
    }

    /// Returns `%Proxy%`.
    #[must_use]
    pub const fn proxy_constructor(&self) -> ObjectHandle {
        self.proxy_constructor
    }

    /// Returns `%Promise%`.
    #[must_use]
    pub const fn promise_constructor(&self) -> ObjectHandle {
        self.promise_constructor
    }

    /// Returns `%Promise.prototype%`.
    #[must_use]
    pub const fn promise_prototype(&self) -> ObjectHandle {
        self.promise_prototype
    }

    /// Returns `%IteratorPrototype%` (§27.1.2).
    #[must_use]
    pub const fn iterator_prototype(&self) -> ObjectHandle {
        self.iterator_prototype
    }

    /// Returns `%AsyncIteratorPrototype%` (§27.1.4).
    #[must_use]
    pub const fn async_iterator_prototype(&self) -> ObjectHandle {
        self.async_iterator_prototype
    }

    /// Returns `%ArrayIteratorPrototype%` (§23.1.5).
    #[must_use]
    pub const fn array_iterator_prototype(&self) -> ObjectHandle {
        self.array_iterator_prototype
    }

    /// Returns `%StringIteratorPrototype%` (§22.1.5).
    #[must_use]
    pub const fn string_iterator_prototype(&self) -> ObjectHandle {
        self.string_iterator_prototype
    }

    /// Returns `%MapIteratorPrototype%` (§24.1.5).
    #[must_use]
    pub const fn map_iterator_prototype(&self) -> ObjectHandle {
        self.map_iterator_prototype
    }

    /// Returns `%SetIteratorPrototype%` (§24.2.5).
    #[must_use]
    pub const fn set_iterator_prototype(&self) -> ObjectHandle {
        self.set_iterator_prototype
    }

    /// Returns `%GeneratorFunction.prototype%` (§27.3).
    #[must_use]
    pub const fn generator_function_prototype(&self) -> ObjectHandle {
        self.generator_function_prototype
    }

    /// Returns `%Generator.prototype%` (§27.5).
    #[must_use]
    pub const fn generator_prototype(&self) -> ObjectHandle {
        self.generator_prototype
    }

    /// Returns `%AsyncGeneratorFunction.prototype%` (§27.4).
    /// Spec: <https://tc39.es/ecma262/#sec-properties-of-the-asyncgeneratorfunction-prototype-object>
    #[must_use]
    pub const fn async_generator_function_prototype(&self) -> ObjectHandle {
        self.async_generator_function_prototype
    }

    /// Returns `%AsyncGenerator.prototype%` (§27.6).
    /// Spec: <https://tc39.es/ecma262/#sec-asyncgenerator-objects>
    #[must_use]
    pub const fn async_generator_prototype(&self) -> ObjectHandle {
        self.async_generator_prototype
    }

    /// Returns `%RegExp%` (§22.2).
    #[must_use]
    pub const fn regexp_constructor(&self) -> ObjectHandle {
        self.regexp_constructor
    }

    /// Returns `%RegExp.prototype%` (§22.2).
    #[must_use]
    pub const fn regexp_prototype(&self) -> ObjectHandle {
        self.regexp_prototype
    }

    // ── Temporal (proposal-temporal) ────────────────────────────────

    /// Returns the `Temporal` namespace object.
    #[must_use]
    pub const fn temporal_namespace(&self) -> ObjectHandle {
        self.temporal_namespace
    }

    #[must_use]
    pub const fn intl_collator_prototype(&self) -> ObjectHandle {
        self.intl_collator_prototype
    }

    #[must_use]
    pub const fn intl_number_format_prototype(&self) -> ObjectHandle {
        self.intl_number_format_prototype
    }

    #[must_use]
    pub const fn intl_plural_rules_prototype(&self) -> ObjectHandle {
        self.intl_plural_rules_prototype
    }

    #[must_use]
    pub const fn intl_locale_prototype(&self) -> ObjectHandle {
        self.intl_locale_prototype
    }

    #[must_use]
    pub const fn intl_date_time_format_prototype(&self) -> ObjectHandle {
        self.intl_date_time_format_prototype
    }

    pub const fn intl_list_format_prototype(&self) -> ObjectHandle {
        self.intl_list_format_prototype
    }

    pub const fn intl_segmenter_prototype(&self) -> ObjectHandle {
        self.intl_segmenter_prototype
    }

    pub const fn intl_segments_prototype(&self) -> ObjectHandle {
        self.intl_segments_prototype
    }

    pub const fn intl_segment_iterator_prototype(&self) -> ObjectHandle {
        self.intl_segment_iterator_prototype
    }

    pub const fn intl_display_names_prototype(&self) -> ObjectHandle {
        self.intl_display_names_prototype
    }

    pub const fn intl_relative_time_format_prototype(&self) -> ObjectHandle {
        self.intl_relative_time_format_prototype
    }

    /// Returns `%Temporal.Instant.prototype%`.
    #[must_use]
    pub const fn temporal_instant_prototype(&self) -> ObjectHandle {
        self.temporal_instant_prototype
    }

    /// Returns `%Temporal.Duration.prototype%`.
    #[must_use]
    pub const fn temporal_duration_prototype(&self) -> ObjectHandle {
        self.temporal_duration_prototype
    }

    /// Returns `%Temporal.PlainDate.prototype%`.
    #[must_use]
    pub const fn temporal_plain_date_prototype(&self) -> ObjectHandle {
        self.temporal_plain_date_prototype
    }

    /// Returns `%Temporal.PlainTime.prototype%`.
    #[must_use]
    pub const fn temporal_plain_time_prototype(&self) -> ObjectHandle {
        self.temporal_plain_time_prototype
    }

    /// Returns `%Temporal.PlainDateTime.prototype%`.
    #[must_use]
    pub const fn temporal_plain_date_time_prototype(&self) -> ObjectHandle {
        self.temporal_plain_date_time_prototype
    }

    /// Returns `%Temporal.PlainYearMonth.prototype%`.
    #[must_use]
    pub const fn temporal_plain_year_month_prototype(&self) -> ObjectHandle {
        self.temporal_plain_year_month_prototype
    }

    /// Returns `%Temporal.PlainMonthDay.prototype%`.
    #[must_use]
    pub const fn temporal_plain_month_day_prototype(&self) -> ObjectHandle {
        self.temporal_plain_month_day_prototype
    }

    /// Returns `%Temporal.ZonedDateTime.prototype%`.
    #[must_use]
    pub const fn temporal_zoned_date_time_prototype(&self) -> ObjectHandle {
        self.temporal_zoned_date_time_prototype
    }

    /// Registers an additional namespace root owned by the intrinsic registry.
    pub fn register_namespace_root(&mut self, handle: ObjectHandle) {
        self.namespace_roots.push(handle);
    }

    pub(super) fn set_math_namespace(&mut self, handle: ObjectHandle) {
        self.math_namespace = Some(handle);
        self.register_namespace_root(handle);
    }

    pub(super) fn math_namespace(&self) -> Option<ObjectHandle> {
        self.math_namespace
    }

    pub(super) fn set_reflect_namespace(&mut self, handle: ObjectHandle) {
        self.reflect_namespace = Some(handle);
        self.register_namespace_root(handle);
    }

    pub(super) fn reflect_namespace(&self) -> Option<ObjectHandle> {
        self.reflect_namespace
    }

    /// Store/retrieve a named namespace (generic for JSON, Atomics, and future namespaces).
    pub(super) fn set_namespace(&mut self, name: &str, handle: ObjectHandle) {
        match name {
            "JSON" => self.json_namespace = Some(handle),
            "Atomics" => self.atomics_namespace = Some(handle),
            _ => {}
        }
        self.register_namespace_root(handle);
    }

    pub(super) fn namespace(&self, name: &str) -> Option<ObjectHandle> {
        match name {
            "JSON" => self.json_namespace,
            "Atomics" => self.atomics_namespace,
            _ => None,
        }
    }

    /// Returns the additional namespace roots.
    #[must_use]
    pub fn namespace_roots(&self) -> &[ObjectHandle] {
        &self.namespace_roots
    }

    /// Returns the stable well-known symbols owned by the intrinsic registry.
    #[must_use]
    pub fn well_known_symbols(&self) -> &[WellKnownSymbol] {
        &self.well_known_symbols
    }

    /// Returns the immediate register value for a stable well-known symbol.
    #[must_use]
    pub const fn well_known_symbol_value(&self, symbol: WellKnownSymbol) -> RegisterValue {
        RegisterValue::from_symbol_id(symbol.stable_id())
    }

    /// Collects all ObjectHandle roots owned by intrinsics for GC.
    pub fn gc_root_handles(&self) -> Vec<ObjectHandle> {
        let mut roots = Vec::with_capacity(24 + self.namespace_roots.len());
        roots.extend_from_slice(&[
            self.global_object,
            self.object_prototype,
            self.function_prototype,
            self.string_constructor,
            self.symbol_constructor,
            self.number_constructor,
            self.boolean_constructor,
            self.date_constructor,
            self.object_constructor,
            self.function_constructor,
            self.array_constructor,
            self.array_prototype,
            self.array_buffer_constructor,
            self.array_buffer_prototype,
            self.shared_array_buffer_constructor,
            self.shared_array_buffer_prototype,
            self.data_view_constructor,
            self.data_view_prototype,
            self.typed_array_base_constructor,
            self.typed_array_base_prototype,
            self.int8_array_constructor,
            self.int8_array_prototype,
            self.uint8_array_constructor,
            self.uint8_array_prototype,
            self.uint8_clamped_array_constructor,
            self.uint8_clamped_array_prototype,
            self.int16_array_constructor,
            self.int16_array_prototype,
            self.uint16_array_constructor,
            self.uint16_array_prototype,
            self.int32_array_constructor,
            self.int32_array_prototype,
            self.uint32_array_constructor,
            self.uint32_array_prototype,
            self.float32_array_constructor,
            self.float32_array_prototype,
            self.float64_array_constructor,
            self.float64_array_prototype,
            self.bigint64_array_constructor,
            self.bigint64_array_prototype,
            self.biguint64_array_constructor,
            self.biguint64_array_prototype,
            self.string_prototype,
            self.symbol_prototype,
            self.number_prototype,
            self.boolean_prototype,
            self.date_prototype,
            self.proxy_constructor,
            self.error_prototype,
            self.error_constructor,
            self.type_error_prototype,
            self.type_error_constructor,
            self.reference_error_prototype,
            self.reference_error_constructor,
            self.range_error_prototype,
            self.range_error_constructor,
            self.syntax_error_prototype,
            self.syntax_error_constructor,
            self.promise_constructor,
            self.promise_prototype,
            self.weakmap_constructor,
            self.weakmap_prototype,
            self.weakset_constructor,
            self.weakset_prototype,
            self.weakref_constructor,
            self.weakref_prototype,
            self.finalization_registry_constructor,
            self.finalization_registry_prototype,
            self.iterator_prototype,
            self.async_iterator_prototype,
            self.array_iterator_prototype,
            self.string_iterator_prototype,
            self.map_iterator_prototype,
            self.set_iterator_prototype,
            self.generator_function_prototype,
            self.generator_prototype,
            self.regexp_constructor,
            self.regexp_prototype,
            self.temporal_namespace,
            self.temporal_instant_constructor,
            self.temporal_instant_prototype,
            self.temporal_duration_constructor,
            self.temporal_duration_prototype,
            self.temporal_plain_date_constructor,
            self.temporal_plain_date_prototype,
            self.temporal_plain_time_constructor,
            self.temporal_plain_time_prototype,
            self.temporal_plain_date_time_constructor,
            self.temporal_plain_date_time_prototype,
            self.temporal_plain_year_month_constructor,
            self.temporal_plain_year_month_prototype,
            self.temporal_plain_month_day_constructor,
            self.temporal_plain_month_day_prototype,
            self.temporal_zoned_date_time_constructor,
            self.temporal_zoned_date_time_prototype,
        ]);
        if let Some(h) = self.iterator_constructor {
            roots.push(h);
        }
        if let Some(h) = self.math_namespace {
            roots.push(h);
        }
        if let Some(h) = self.reflect_namespace {
            roots.push(h);
        }
        roots.extend_from_slice(&self.namespace_roots);
        roots
    }

    /// Enumerates all roots owned by the intrinsic registry.
    pub fn trace_roots(&self, tracer: &mut dyn FnMut(IntrinsicRoot)) {
        for handle in [
            self.global_object,
            self.object_prototype,
            self.function_prototype,
            self.string_constructor,
            self.symbol_constructor,
            self.number_constructor,
            self.boolean_constructor,
            self.date_constructor,
            self.object_constructor,
            self.function_constructor,
            self.array_constructor,
            self.array_prototype,
            self.array_buffer_constructor,
            self.array_buffer_prototype,
            self.shared_array_buffer_constructor,
            self.shared_array_buffer_prototype,
            self.data_view_constructor,
            self.data_view_prototype,
            self.typed_array_base_constructor,
            self.typed_array_base_prototype,
            self.int8_array_constructor,
            self.int8_array_prototype,
            self.uint8_array_constructor,
            self.uint8_array_prototype,
            self.uint8_clamped_array_constructor,
            self.uint8_clamped_array_prototype,
            self.int16_array_constructor,
            self.int16_array_prototype,
            self.uint16_array_constructor,
            self.uint16_array_prototype,
            self.int32_array_constructor,
            self.int32_array_prototype,
            self.uint32_array_constructor,
            self.uint32_array_prototype,
            self.float32_array_constructor,
            self.float32_array_prototype,
            self.float64_array_constructor,
            self.float64_array_prototype,
            self.bigint64_array_constructor,
            self.bigint64_array_prototype,
            self.biguint64_array_constructor,
            self.biguint64_array_prototype,
            self.string_prototype,
            self.symbol_prototype,
            self.number_prototype,
            self.boolean_prototype,
            self.date_prototype,
            self.proxy_constructor,
            self.generator_function_prototype,
            self.generator_prototype,
            self.intl_namespace,
            self.intl_collator_constructor,
            self.intl_collator_prototype,
            self.intl_number_format_constructor,
            self.intl_number_format_prototype,
            self.intl_plural_rules_constructor,
            self.intl_plural_rules_prototype,
            self.intl_locale_constructor,
            self.intl_locale_prototype,
            self.intl_date_time_format_constructor,
            self.intl_date_time_format_prototype,
            self.intl_list_format_constructor,
            self.intl_list_format_prototype,
            self.intl_segmenter_constructor,
            self.intl_segmenter_prototype,
            self.intl_segments_prototype,
            self.intl_segment_iterator_prototype,
            self.intl_display_names_constructor,
            self.intl_display_names_prototype,
            self.intl_relative_time_format_constructor,
            self.intl_relative_time_format_prototype,
            self.temporal_namespace,
            self.temporal_instant_constructor,
            self.temporal_instant_prototype,
            self.temporal_duration_constructor,
            self.temporal_duration_prototype,
            self.temporal_plain_date_constructor,
            self.temporal_plain_date_prototype,
            self.temporal_plain_time_constructor,
            self.temporal_plain_time_prototype,
            self.temporal_plain_date_time_constructor,
            self.temporal_plain_date_time_prototype,
            self.temporal_plain_year_month_constructor,
            self.temporal_plain_year_month_prototype,
            self.temporal_plain_month_day_constructor,
            self.temporal_plain_month_day_prototype,
            self.temporal_zoned_date_time_constructor,
            self.temporal_zoned_date_time_prototype,
        ] {
            tracer(IntrinsicRoot::Object(handle));
        }

        if let Some(handle) = self.iterator_constructor {
            tracer(IntrinsicRoot::Object(handle));
        }

        if let Some(handle) = self.throw_type_error_function {
            tracer(IntrinsicRoot::Object(handle));
        }

        for handle in &self.namespace_roots {
            tracer(IntrinsicRoot::Object(*handle));
        }

        for symbol in &self.well_known_symbols {
            tracer(IntrinsicRoot::Symbol(*symbol));
        }
    }
}

fn core_installers() -> [&'static dyn IntrinsicInstaller; 30] {
    [
        // Iterator must be first — other installers reference iterator prototypes.
        &iterator_class::ITERATOR_INTRINSIC as &dyn IntrinsicInstaller,
        &array_class::ARRAY_INTRINSIC as &dyn IntrinsicInstaller,
        &arraybuffer_class::ARRAY_BUFFER_INTRINSIC as &dyn IntrinsicInstaller,
        &atomics::ATOMICS_INTRINSIC as &dyn IntrinsicInstaller,
        &bigint_class::BIGINT_INTRINSIC as &dyn IntrinsicInstaller,
        &boolean_class::BOOLEAN_INTRINSIC as &dyn IntrinsicInstaller,
        &dataview_class::DATA_VIEW_INTRINSIC as &dyn IntrinsicInstaller,
        &date_class::DATE_INTRINSIC as &dyn IntrinsicInstaller,
        &error_class::ERROR_INTRINSIC as &dyn IntrinsicInstaller,
        &function_class::FUNCTION_INTRINSIC as &dyn IntrinsicInstaller,
        &generator_class::GENERATOR_INTRINSIC as &dyn IntrinsicInstaller,
        &async_generator_class::ASYNC_GENERATOR_INTRINSIC as &dyn IntrinsicInstaller,
        &json::JSON_INTRINSIC as &dyn IntrinsicInstaller,
        &map_set_class::MAP_SET_INTRINSIC as &dyn IntrinsicInstaller,
        &math::MATH_INTRINSIC as &dyn IntrinsicInstaller,
        &number_class::NUMBER_INTRINSIC as &dyn IntrinsicInstaller,
        &object_class::OBJECT_INTRINSIC as &dyn IntrinsicInstaller,
        &promise_class::PROMISE_INTRINSIC as &dyn IntrinsicInstaller,
        &proxy_class::PROXY_INTRINSIC as &dyn IntrinsicInstaller,
        &reflect::REFLECT_INTRINSIC as &dyn IntrinsicInstaller,
        &regexp_class::REGEXP_INTRINSIC as &dyn IntrinsicInstaller,
        &sharedarraybuffer_class::SHARED_ARRAY_BUFFER_INTRINSIC as &dyn IntrinsicInstaller,
        &species_support::SPECIES_SUPPORT_INTRINSIC as &dyn IntrinsicInstaller,
        &symbol_class::SYMBOL_INTRINSIC as &dyn IntrinsicInstaller,
        &string_class::STRING_INTRINSIC as &dyn IntrinsicInstaller,
        &intl::INTL_INTRINSIC as &dyn IntrinsicInstaller,
        &temporal::TEMPORAL_INTRINSIC as &dyn IntrinsicInstaller,
        &typedarray_class::TYPED_ARRAY_INTRINSIC as &dyn IntrinsicInstaller,
        &weakmap_weakset_class::WEAKMAP_WEAKSET_INTRINSIC as &dyn IntrinsicInstaller,
        &weakref_class::WEAKREF_INTRINSIC as &dyn IntrinsicInstaller,
    ]
}

#[cfg(test)]
mod tests {
    use super::{IntrinsicRoot, IntrinsicsStage, VmIntrinsics};
    use crate::host::NativeFunctionRegistry;
    use crate::object::{HeapValueKind, PropertyValue};
    use crate::property::PropertyNameRegistry;

    #[test]
    fn intrinsics_bootstrap_advances_through_lifecycle() {
        let mut heap = crate::object::ObjectHeap::new();
        let mut intrinsics = VmIntrinsics::allocate(&mut heap);
        let mut property_names = PropertyNameRegistry::new();
        let mut native_functions = NativeFunctionRegistry::new();
        assert_eq!(intrinsics.stage(), IntrinsicsStage::Allocated);

        intrinsics
            .wire_prototype_chains(&mut heap)
            .expect("wiring should succeed");
        assert_eq!(intrinsics.stage(), IntrinsicsStage::Wired);

        intrinsics
            .init_core(&mut heap, &mut property_names, &mut native_functions)
            .expect("init should succeed");
        assert_eq!(intrinsics.stage(), IntrinsicsStage::Initialized);

        intrinsics
            .install_on_global(&mut heap, &mut property_names, &mut native_functions)
            .expect("install should succeed");
        assert_eq!(intrinsics.stage(), IntrinsicsStage::Installed);

        for handle in [
            intrinsics.global_object(),
            intrinsics.object_prototype(),
            intrinsics.function_prototype(),
            intrinsics.array_prototype(),
            intrinsics.array_buffer_prototype(),
            intrinsics.shared_array_buffer_prototype(),
            intrinsics.string_prototype(),
            intrinsics.symbol_prototype(),
            intrinsics.number_prototype(),
            intrinsics.boolean_prototype(),
        ] {
            assert_eq!(heap.kind(handle), Ok(HeapValueKind::Object));
        }
        assert_eq!(
            heap.kind(intrinsics.string_constructor()),
            Ok(HeapValueKind::HostFunction)
        );
        assert_eq!(
            heap.kind(intrinsics.symbol_constructor()),
            Ok(HeapValueKind::HostFunction)
        );
        assert_eq!(
            heap.kind(intrinsics.number_constructor()),
            Ok(HeapValueKind::HostFunction)
        );
        assert_eq!(
            heap.kind(intrinsics.boolean_constructor()),
            Ok(HeapValueKind::HostFunction)
        );
        assert_eq!(
            heap.kind(intrinsics.date_constructor()),
            Ok(HeapValueKind::HostFunction)
        );
        assert_eq!(
            heap.kind(intrinsics.object_constructor()),
            Ok(HeapValueKind::HostFunction)
        );
        assert_eq!(
            heap.kind(intrinsics.function_constructor()),
            Ok(HeapValueKind::HostFunction)
        );
        assert_eq!(
            heap.kind(intrinsics.array_constructor()),
            Ok(HeapValueKind::HostFunction)
        );
        assert_eq!(
            heap.kind(intrinsics.array_buffer_constructor()),
            Ok(HeapValueKind::HostFunction)
        );
        assert_eq!(
            heap.kind(intrinsics.shared_array_buffer_constructor()),
            Ok(HeapValueKind::HostFunction)
        );
        assert_eq!(
            heap.kind(intrinsics.proxy_constructor()),
            Ok(HeapValueKind::HostFunction)
        );

        assert_eq!(intrinsics.namespace_roots().len(), 4);
        assert_eq!(native_functions.len(), 731);
        assert_eq!(
            heap.get_prototype(intrinsics.global_object()),
            Ok(Some(intrinsics.object_prototype()))
        );
        assert_eq!(heap.get_prototype(intrinsics.object_prototype()), Ok(None));
        assert_eq!(
            heap.get_prototype(intrinsics.function_prototype()),
            Ok(Some(intrinsics.object_prototype()))
        );
        assert_eq!(
            heap.get_prototype(intrinsics.array_constructor()),
            Ok(Some(intrinsics.function_prototype()))
        );
        assert_eq!(
            heap.get_prototype(intrinsics.array_buffer_constructor()),
            Ok(Some(intrinsics.function_prototype()))
        );
        assert_eq!(
            heap.get_prototype(intrinsics.shared_array_buffer_constructor()),
            Ok(Some(intrinsics.function_prototype()))
        );
        assert_eq!(
            heap.get_prototype(intrinsics.string_constructor()),
            Ok(Some(intrinsics.function_prototype()))
        );
        assert_eq!(
            heap.get_prototype(intrinsics.symbol_constructor()),
            Ok(Some(intrinsics.function_prototype()))
        );

        let math_property = property_names.intern("Math");
        let math_namespace = heap
            .get_property(intrinsics.global_object(), math_property)
            .expect("global Math lookup should succeed")
            .expect("Math namespace should be installed");
        let math_namespace = math_namespace.value();
        let PropertyValue::Data {
            value: math_namespace,
            ..
        } = math_namespace
        else {
            panic!("expected Math to be a data property");
        };
        let math_namespace = math_namespace
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Math namespace should be an object");
        assert_eq!(heap.kind(math_namespace), Ok(HeapValueKind::Object));

        let abs_property = property_names.intern("abs");
        let abs = heap
            .get_property(math_namespace, abs_property)
            .expect("Math.abs lookup should succeed")
            .expect("Math.abs should be installed");
        let abs = abs.value();
        let PropertyValue::Data { value: abs, .. } = abs else {
            panic!("expected Math.abs to be a data property");
        };
        let abs = abs
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Math.abs should be an object");
        assert_eq!(heap.kind(abs), Ok(HeapValueKind::HostFunction));
        assert_eq!(
            heap.get_prototype(abs),
            Ok(Some(intrinsics.function_prototype()))
        );
        let to_string_property = property_names.intern("toString");
        let to_string = heap
            .get_property(abs, to_string_property)
            .expect("Function.prototype.toString lookup should succeed")
            .expect("Function.prototype.toString should be inherited");
        assert_eq!(to_string.owner(), intrinsics.function_prototype());

        let object_property = property_names.intern("Object");
        let object_constructor = heap
            .get_property(intrinsics.global_object(), object_property)
            .expect("global Object lookup should succeed")
            .expect("Object constructor should be installed");
        let object_constructor = object_constructor.value();
        let PropertyValue::Data {
            value: object_constructor,
            ..
        } = object_constructor
        else {
            panic!("expected Object to be a data property");
        };
        let object_constructor = object_constructor
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Object should be an object");
        assert_eq!(
            object_constructor,
            intrinsics.object_constructor(),
            "global Object should point at the intrinsic constructor handle"
        );

        let prototype_property = property_names.intern("prototype");
        let prototype = heap
            .get_property(object_constructor, prototype_property)
            .expect("Object.prototype lookup should succeed")
            .expect("Object.prototype should be installed");
        let prototype = prototype.value();
        let PropertyValue::Data {
            value: prototype, ..
        } = prototype
        else {
            panic!("expected Object.prototype to be a data property");
        };
        let prototype = prototype
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Object.prototype should be an object");
        assert_eq!(prototype, intrinsics.object_prototype());

        let function_property = property_names.intern("Function");
        let function_constructor = heap
            .get_property(intrinsics.global_object(), function_property)
            .expect("global Function lookup should succeed")
            .expect("Function constructor should be installed");
        let function_constructor = function_constructor.value();
        let PropertyValue::Data {
            value: function_constructor,
            ..
        } = function_constructor
        else {
            panic!("expected Function to be a data property");
        };
        let function_constructor = function_constructor
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Function should be an object");
        assert_eq!(
            function_constructor,
            intrinsics.function_constructor(),
            "global Function should point at the intrinsic constructor handle"
        );

        let function_prototype = heap
            .get_property(function_constructor, prototype_property)
            .expect("Function.prototype lookup should succeed")
            .expect("Function.prototype should be installed");
        let function_prototype = function_prototype.value();
        let PropertyValue::Data {
            value: function_prototype,
            ..
        } = function_prototype
        else {
            panic!("expected Function.prototype to be a data property");
        };
        let function_prototype = function_prototype
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Function.prototype should be an object");
        assert_eq!(function_prototype, intrinsics.function_prototype());

        let array_property = property_names.intern("Array");
        let array_constructor = heap
            .get_property(intrinsics.global_object(), array_property)
            .expect("global Array lookup should succeed")
            .expect("Array constructor should be installed");
        let array_constructor = array_constructor.value();
        let PropertyValue::Data {
            value: array_constructor,
            ..
        } = array_constructor
        else {
            panic!("expected Array to be a data property");
        };
        let array_constructor = array_constructor
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Array should be an object");
        assert_eq!(array_constructor, intrinsics.array_constructor());

        let array_prototype = heap
            .get_property(array_constructor, prototype_property)
            .expect("Array.prototype lookup should succeed")
            .expect("Array.prototype should be installed");
        let array_prototype = array_prototype.value();
        let PropertyValue::Data {
            value: array_prototype,
            ..
        } = array_prototype
        else {
            panic!("expected Array.prototype to be a data property");
        };
        let array_prototype = array_prototype
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Array.prototype should be an object");
        assert_eq!(array_prototype, intrinsics.array_prototype());

        let push_property = property_names.intern("push");
        let push = heap
            .get_property(array_prototype, push_property)
            .expect("Array.prototype.push lookup should succeed")
            .expect("Array.prototype.push should be installed");
        let push = push.value();
        let PropertyValue::Data { value: push, .. } = push else {
            panic!("expected Array.prototype.push to be a data property");
        };
        let push = push
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Array.prototype.push should be an object");
        assert_eq!(heap.kind(push), Ok(HeapValueKind::HostFunction));

        let string_property = property_names.intern("String");
        let string_constructor = heap
            .get_property(intrinsics.global_object(), string_property)
            .expect("global String lookup should succeed")
            .expect("String constructor should be installed");
        let string_constructor = string_constructor.value();
        let PropertyValue::Data {
            value: string_constructor,
            ..
        } = string_constructor
        else {
            panic!("expected String to be a data property");
        };
        let string_constructor = string_constructor
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("String should be an object");
        assert_eq!(string_constructor, intrinsics.string_constructor());

        let string_prototype = heap
            .get_property(string_constructor, prototype_property)
            .expect("String.prototype lookup should succeed")
            .expect("String.prototype should be installed");
        let string_prototype = string_prototype.value();
        let PropertyValue::Data {
            value: string_prototype,
            ..
        } = string_prototype
        else {
            panic!("expected String.prototype to be a data property");
        };
        let string_prototype = string_prototype
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("String.prototype should be an object");
        assert_eq!(string_prototype, intrinsics.string_prototype());

        let string_value_of_property = property_names.intern("valueOf");
        let string_value_of = heap
            .get_property(string_prototype, string_value_of_property)
            .expect("String.prototype.valueOf lookup should succeed")
            .expect("String.prototype.valueOf should be installed");
        assert_eq!(string_value_of.owner(), intrinsics.string_prototype());

        let symbol_property = property_names.intern("Symbol");
        let symbol_constructor = heap
            .get_property(intrinsics.global_object(), symbol_property)
            .expect("global Symbol lookup should succeed")
            .expect("Symbol constructor should be installed");
        let symbol_constructor = symbol_constructor.value();
        let PropertyValue::Data {
            value: symbol_constructor,
            ..
        } = symbol_constructor
        else {
            panic!("expected Symbol to be a data property");
        };
        let symbol_constructor = symbol_constructor
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Symbol should be an object");
        assert_eq!(symbol_constructor, intrinsics.symbol_constructor());

        let symbol_prototype = heap
            .get_property(symbol_constructor, prototype_property)
            .expect("Symbol.prototype lookup should succeed")
            .expect("Symbol.prototype should be installed");
        let symbol_prototype = symbol_prototype.value();
        let PropertyValue::Data {
            value: symbol_prototype,
            ..
        } = symbol_prototype
        else {
            panic!("expected Symbol.prototype to be a data property");
        };
        let symbol_prototype = symbol_prototype
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Symbol.prototype should be an object");
        assert_eq!(symbol_prototype, intrinsics.symbol_prototype());

        for &symbol in intrinsics.well_known_symbols() {
            let property_name = symbol
                .description()
                .strip_prefix("Symbol.")
                .expect("well-known symbol descriptions use Symbol.<name>");
            let property = property_names.intern(property_name);
            let installed = heap
                .get_property(symbol_constructor, property)
                .unwrap_or_else(|_| panic!("{} lookup should succeed", symbol.description()))
                .unwrap_or_else(|| panic!("{} should be installed", symbol.description()));
            let installed = installed.value();
            let PropertyValue::Data {
                value: installed,
                attributes,
            } = installed
            else {
                panic!("expected {} to be a data property", symbol.description());
            };
            assert_eq!(
                installed.as_symbol_id(),
                Some(symbol.stable_id()),
                "{} should expose the stable symbol value",
                symbol.description()
            );
            assert!(
                !attributes.writable(),
                "{} should be non-writable",
                symbol.description()
            );
            assert!(
                !attributes.enumerable(),
                "{} should be non-enumerable",
                symbol.description()
            );
            assert!(
                !attributes.configurable(),
                "{} should be non-configurable",
                symbol.description()
            );
        }

        let number_property = property_names.intern("Number");
        let number_constructor = heap
            .get_property(intrinsics.global_object(), number_property)
            .expect("global Number lookup should succeed")
            .expect("Number constructor should be installed");
        let number_constructor = number_constructor.value();
        let PropertyValue::Data {
            value: number_constructor,
            ..
        } = number_constructor
        else {
            panic!("expected Number to be a data property");
        };
        let number_constructor = number_constructor
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Number should be an object");
        assert_eq!(number_constructor, intrinsics.number_constructor());

        let boolean_property = property_names.intern("Boolean");
        let boolean_constructor = heap
            .get_property(intrinsics.global_object(), boolean_property)
            .expect("global Boolean lookup should succeed")
            .expect("Boolean constructor should be installed");
        let boolean_constructor = boolean_constructor.value();
        let PropertyValue::Data {
            value: boolean_constructor,
            ..
        } = boolean_constructor
        else {
            panic!("expected Boolean to be a data property");
        };
        let boolean_constructor = boolean_constructor
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Boolean should be an object");
        assert_eq!(boolean_constructor, intrinsics.boolean_constructor());

        let reflect_property = property_names.intern("Reflect");
        let reflect_namespace = heap
            .get_property(intrinsics.global_object(), reflect_property)
            .expect("global Reflect lookup should succeed")
            .expect("Reflect namespace should be installed");
        let reflect_namespace = reflect_namespace.value();
        let PropertyValue::Data {
            value: reflect_namespace,
            ..
        } = reflect_namespace
        else {
            panic!("expected Reflect to be a data property");
        };
        let reflect_namespace = reflect_namespace
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Reflect should be an object");
        assert_eq!(heap.kind(reflect_namespace), Ok(HeapValueKind::Object));
    }

    #[test]
    fn intrinsics_trace_roots_covers_objects_and_symbols() {
        let mut heap = crate::object::ObjectHeap::new();
        let mut intrinsics = VmIntrinsics::allocate(&mut heap);
        let namespace = heap.alloc_object();
        intrinsics.register_namespace_root(namespace);

        let mut seen = Vec::new();
        intrinsics.trace_roots(&mut |root| seen.push(root));

        assert!(seen.contains(&IntrinsicRoot::Object(intrinsics.global_object())));
        assert!(seen.contains(&IntrinsicRoot::Object(namespace)));
        for &symbol in intrinsics.well_known_symbols() {
            assert!(seen.contains(&IntrinsicRoot::Symbol(symbol)));
        }
    }
}
