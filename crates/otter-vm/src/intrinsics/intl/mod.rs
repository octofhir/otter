//! Intl namespace and sub-type intrinsics (ECMA-402).
//!
//! The `Intl` global is a plain namespace object (like `Math` / `JSON`).
//! Each Intl type (Collator, NumberFormat, PluralRules, Locale, DateTimeFormat)
//! is installed as a property of the `Intl` namespace, not as a direct global.
//!
//! Spec: <https://tc39.es/ecma402/>

pub mod collator;
pub mod date_time_format;
pub mod locale;
pub mod locale_utils;
pub mod number_format;
pub mod options_utils;
#[allow(dead_code)]
pub mod payload;
pub mod plural_rules;

use crate::builders::ClassBuilder;
use crate::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
use crate::object::{ObjectHandle, PropertyAttributes, PropertyValue};
use crate::value::RegisterValue;

use super::install::{
    IntrinsicInstallContext, IntrinsicInstaller, install_class_plan, install_function_length_name,
};
use super::{IntrinsicsError, VmIntrinsics, WellKnownSymbol};

pub(super) static INTL_INTRINSIC: IntlIntrinsic = IntlIntrinsic;

pub(super) struct IntlIntrinsic;

impl IntrinsicInstaller for IntlIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        // @@toStringTag on Intl namespace
        let tag_symbol = cx
            .property_names
            .intern_symbol(WellKnownSymbol::ToStringTag.stable_id());
        let tag_str = cx.heap.alloc_string("Intl");
        cx.heap.define_own_property(
            intrinsics.intl_namespace,
            tag_symbol,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(tag_str.0),
                PropertyAttributes::from_flags(false, false, true),
            ),
        )?;

        // Intl.Collator (ECMA-402 &sect;11)
        install_intl_class(
            intrinsics.intl_collator_prototype,
            &mut intrinsics.intl_collator_constructor,
            &collator::collator_class_descriptor(),
            intrinsics.function_prototype,
            cx,
        )?;

        // Intl.NumberFormat (ECMA-402 &sect;15)
        install_intl_class(
            intrinsics.intl_number_format_prototype,
            &mut intrinsics.intl_number_format_constructor,
            &number_format::number_format_class_descriptor(),
            intrinsics.function_prototype,
            cx,
        )?;

        // Intl.PluralRules (ECMA-402 &sect;13)
        install_intl_class(
            intrinsics.intl_plural_rules_prototype,
            &mut intrinsics.intl_plural_rules_constructor,
            &plural_rules::plural_rules_class_descriptor(),
            intrinsics.function_prototype,
            cx,
        )?;

        // Intl.Locale (ECMA-402 &sect;14)
        install_intl_class(
            intrinsics.intl_locale_prototype,
            &mut intrinsics.intl_locale_constructor,
            &locale::locale_class_descriptor(),
            intrinsics.function_prototype,
            cx,
        )?;

        // Intl.DateTimeFormat (ECMA-402 &sect;12)
        install_intl_class(
            intrinsics.intl_date_time_format_prototype,
            &mut intrinsics.intl_date_time_format_constructor,
            &date_time_format::date_time_format_class_descriptor(),
            intrinsics.function_prototype,
            cx,
        )?;

        // Intl.getCanonicalLocales (&sect;8.3.1)
        install_namespace_method(
            intrinsics.intl_namespace,
            NativeFunctionDescriptor::method("getCanonicalLocales", 1, intl_get_canonical_locales),
            intrinsics.function_prototype,
            cx,
        )?;

        // Intl.supportedValuesOf (&sect;8.3.2)
        install_namespace_method(
            intrinsics.intl_namespace,
            NativeFunctionDescriptor::method("supportedValuesOf", 1, intl_supported_values_of),
            intrinsics.function_prototype,
            cx,
        )?;

        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        cx.install_global_value(
            intrinsics,
            "Intl",
            RegisterValue::from_object_handle(intrinsics.intl_namespace.0),
        )?;

        install_on_namespace(intrinsics.intl_namespace, "Collator", intrinsics.intl_collator_constructor, cx)?;
        install_on_namespace(intrinsics.intl_namespace, "NumberFormat", intrinsics.intl_number_format_constructor, cx)?;
        install_on_namespace(intrinsics.intl_namespace, "PluralRules", intrinsics.intl_plural_rules_constructor, cx)?;
        install_on_namespace(intrinsics.intl_namespace, "Locale", intrinsics.intl_locale_constructor, cx)?;
        install_on_namespace(intrinsics.intl_namespace, "DateTimeFormat", intrinsics.intl_date_time_format_constructor, cx)?;

        Ok(())
    }
}

// ── Namespace method installation helper ───────────────────────────

fn install_namespace_method(
    namespace: ObjectHandle,
    descriptor: NativeFunctionDescriptor,
    function_prototype: ObjectHandle,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let name = descriptor.js_name().to_string();
    let length = descriptor.length();
    let host_id = cx.native_functions.register(descriptor);
    let handle = cx.alloc_intrinsic_host_function(host_id, function_prototype)?;
    install_function_length_name(handle, length, &name, cx)?;
    let prop = cx.property_names.intern(&name);
    cx.heap.define_own_property(
        namespace,
        prop,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(handle.0),
            PropertyAttributes::from_flags(true, false, true),
        ),
    )?;
    Ok(())
}

// ── Intl class installation helper ─────────────────────────────────

/// Installs an Intl class following the same pattern as Temporal types.
fn install_intl_class(
    prototype: ObjectHandle,
    constructor: &mut ObjectHandle,
    descriptor: &crate::descriptors::JsClassDescriptor,
    function_prototype: ObjectHandle,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let plan = ClassBuilder::from_descriptor(descriptor)
        .expect("Intl class descriptor should normalize")
        .build();

    if let Some(ctor_desc) = plan.constructor() {
        let host_id = cx.native_functions.register(ctor_desc.clone());
        *constructor = cx.alloc_intrinsic_host_function(host_id, function_prototype)?;
    }

    install_class_plan(prototype, *constructor, &plan, function_prototype, cx)?;
    Ok(())
}

/// Installs a constructor as a property of a namespace object.
fn install_on_namespace(
    namespace: ObjectHandle,
    name: &str,
    constructor: ObjectHandle,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let prop = cx.property_names.intern(name);
    cx.heap.define_own_property(
        namespace,
        prop,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(constructor.0),
            PropertyAttributes::from_flags(true, false, true),
        ),
    )?;
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════
//  §8.3.1 Intl.getCanonicalLocales(locales)
// ═══════════════════════════════════════════════════════════════════

/// §8.3.1 Intl.getCanonicalLocales(locales)
///
/// Spec: <https://tc39.es/ecma402/#sec-intl.getcanonicallocales>
fn intl_get_canonical_locales(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let locales_arg = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let locale_strings = canonicalize_locale_list_from_value(locales_arg, runtime)?;
    let arr = runtime.alloc_array();
    for locale in &locale_strings {
        let s = runtime.alloc_string(locale.as_str());
        runtime
            .objects_mut()
            .push_element(arr, RegisterValue::from_object_handle(s.0))
            .map_err(|e| {
                VmNativeCallError::Internal(format!("getCanonicalLocales: {e:?}").into())
            })?;
    }
    Ok(RegisterValue::from_object_handle(arr.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §8.3.2 Intl.supportedValuesOf(key)
// ═══════════════════════════════════════════════════════════════════

/// §8.3.2 Intl.supportedValuesOf(key)
///
/// Spec: <https://tc39.es/ecma402/#sec-intl.supportedvaluesof>
fn intl_supported_values_of(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let key_arg = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let key = runtime
        .js_to_string(key_arg)
        .map_err(|e| VmNativeCallError::Internal(format!("supportedValuesOf: {e}").into()))?;

    let mut values: Vec<&str> = match key.as_ref() {
        "calendar" => locale_utils::SUPPORTED_CALENDARS.to_vec(),
        "collation" => locale_utils::SUPPORTED_COLLATIONS.to_vec(),
        "currency" => locale_utils::SUPPORTED_CURRENCIES.to_vec(),
        "numberingSystem" => locale_utils::SUPPORTED_NUMBERING_SYSTEMS.to_vec(),
        "timeZone" => locale_utils::SUPPORTED_TIME_ZONES.to_vec(),
        "unit" => locale_utils::SANCTIONED_SIMPLE_UNITS.to_vec(),
        _ => {
            let err = runtime
                .alloc_range_error("Invalid key for Intl.supportedValuesOf")
                .map_err(|e| {
                    VmNativeCallError::Internal(format!("RangeError alloc: {e}").into())
                })?;
            return Err(VmNativeCallError::Thrown(
                RegisterValue::from_object_handle(err.0),
            ));
        }
    };
    values.sort_unstable();
    values.dedup();

    let arr = runtime.alloc_array();
    for value in &values {
        let s = runtime.alloc_string(*value);
        runtime
            .objects_mut()
            .push_element(arr, RegisterValue::from_object_handle(s.0))
            .map_err(|e| {
                VmNativeCallError::Internal(format!("supportedValuesOf: {e:?}").into())
            })?;
    }
    Ok(RegisterValue::from_object_handle(arr.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §9.2.1 CanonicalizeLocaleList — shared helpers
// ═══════════════════════════════════════════════════════════════════

/// Resolves a locale from a JS locales argument (first of the list, or default).
pub(crate) fn resolve_locale(
    locales_arg: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<String, VmNativeCallError> {
    let list = canonicalize_locale_list_from_value(locales_arg, runtime)?;
    Ok(list.into_iter().next().unwrap_or_else(|| locale_utils::DEFAULT_LOCALE.to_string()))
}

/// Simplified CanonicalizeLocaleList that handles string and array-of-strings.
///
/// Spec: <https://tc39.es/ecma402/#sec-canonicalizelocalelist>
pub(crate) fn canonicalize_locale_list_from_value(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Vec<String>, VmNativeCallError> {
    if value == RegisterValue::undefined() {
        return Ok(Vec::new());
    }

    // Single string argument.
    if let Some(handle) = value.as_object_handle() {
        let h = crate::object::ObjectHandle(handle);
        if let Ok(Some(s)) = runtime.objects().string_value(h) {
            let tag = locale_utils::canonicalize_locale_tag(s).map_err(|_| {
                alloc_range_error_thrown(runtime, "Invalid language tag")
            })?;
            return Ok(vec![tag]);
        }
    }

    // Array-like argument.
    if let Some(handle) = value.as_object_handle() {
        let h = crate::object::ObjectHandle(handle);
        let elements = runtime.list_from_array_like(h)?;
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for elem in elements {
            if elem == RegisterValue::undefined() {
                continue;
            }
            let raw = runtime.js_to_string(elem).map_err(|e| {
                VmNativeCallError::Internal(format!("CanonicalizeLocaleList: {e}").into())
            })?;
            let tag = locale_utils::canonicalize_locale_tag(&raw)
                .map_err(|_| alloc_range_error_thrown(runtime, "Invalid language tag"))?;
            if seen.insert(tag.clone()) {
                out.push(tag);
            }
        }
        return Ok(out);
    }

    Ok(Vec::new())
}

/// Helper to throw a RangeError.
fn alloc_range_error_thrown(
    runtime: &mut crate::interpreter::RuntimeState,
    message: &str,
) -> VmNativeCallError {
    match runtime.alloc_range_error(message) {
        Ok(err) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(err.0)),
        Err(e) => VmNativeCallError::Internal(format!("RangeError alloc: {e}").into()),
    }
}
