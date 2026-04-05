//! Intl.Collator implementation.
//!
//! Spec: <https://tc39.es/ecma402/#sec-intl-collator-constructor>

use icu_collator::options::CollatorOptions;
use icu_collator::options::{CaseLevel, Strength};
use icu_collator::{CollatorBorrowed, CollatorPreferences};
use icu_locale::Locale;
use std::cmp::Ordering;
use std::str::FromStr;

use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::{ObjectHandle, PropertyAttributes, PropertyValue};
use crate::value::RegisterValue;

use super::locale_utils;
use super::options_utils::{get_option_bool, get_option_string};
use super::payload::{
    self, CollatorCaseFirst, CollatorData, CollatorSensitivity, CollatorUsage, IntlPayload,
};

// ═══════════════════════════════════════════════════════════════════
//  Class descriptor
// ═══════════════════════════════════════════════════════════════════

pub fn collator_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("Collator")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "Collator",
            0,
            collator_constructor,
        ))
        // §11.3.3 — `compare` is a getter that returns a bound function.
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::getter("compare", collator_compare_getter),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("resolvedOptions", 0, collator_resolved_options),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method(
                "supportedLocalesOf",
                1,
                collator_supported_locales_of,
            ),
        ))
}

// ═══════════════════════════════════════════════════════════════════
//  §11.1.1 Intl.Collator(locales, options)
// ═══════════════════════════════════════════════════════════════════

fn collator_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let locales_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let options_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let locale = super::resolve_locale(locales_arg, runtime)?;
    let data = resolve_collator_options(&locale, options_arg, runtime)?;

    let prototype = runtime.intrinsics().intl_collator_prototype();
    let handle = payload::construct_intl(IntlPayload::Collator(data), prototype, runtime);

    Ok(RegisterValue::from_object_handle(handle.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §11.3.3 get Intl.Collator.prototype.compare
// ═══════════════════════════════════════════════════════════════════

/// §11.3.3 get Intl.Collator.prototype.compare
///
/// Returns a bound compare function. The function is cached on the instance
/// as `__boundCompare` after first access.
///
/// Spec: <https://tc39.es/ecma402/#sec-intl.collator.prototype.compare>
fn collator_compare_getter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Collator.compare getter: expected object".into())
    })?;

    // Check cache.
    let cache_prop = runtime.intern_property_name("__boundCompare");
    let cached = runtime
        .own_property_value(handle, cache_prop)
        .map_err(interp_err)?;
    if cached != RegisterValue::undefined() && cached.as_object_handle().is_some() {
        return Ok(cached);
    }

    // Create the bound function and store collator handle on it.
    let desc = NativeFunctionDescriptor::method("compare", 2, bound_collator_compare);
    let fn_id = runtime.register_native_function(desc);
    let fn_proto = runtime.intrinsics().function_prototype();
    let bound_fn = runtime.objects_mut().alloc_host_function(fn_id);
    let _ = runtime
        .objects_mut()
        .set_prototype(bound_fn, Some(fn_proto));

    // Store the collator handle on the bound function as __collator__.
    let collator_prop = runtime.intern_property_name("__collator__");
    runtime
        .objects_mut()
        .define_own_property(
            bound_fn,
            collator_prop,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(handle.0),
                PropertyAttributes::from_flags(false, false, false),
            ),
        )
        .map_err(interp_err)?;

    // Cache on the instance.
    let bound_val = RegisterValue::from_object_handle(bound_fn.0);
    runtime
        .objects_mut()
        .define_own_property(
            handle,
            cache_prop,
            PropertyValue::data_with_attrs(
                bound_val,
                PropertyAttributes::from_flags(false, false, false),
            ),
        )
        .map_err(interp_err)?;

    Ok(bound_val)
}

/// The bound compare function returned by the `compare` getter.
/// Reads its collator from `__collator__` on the function object itself.
fn bound_collator_compare(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let fn_handle = runtime
        .current_native_callee()
        .ok_or_else(|| VmNativeCallError::Internal("bound compare: no callee".into()))?;
    let collator_prop = runtime.intern_property_name("__collator__");
    let collator_val = runtime
        .own_property_value(fn_handle, collator_prop)
        .map_err(interp_err)?;
    let collator_rv =
        RegisterValue::from_object_handle(collator_val.as_object_handle().ok_or_else(|| {
            VmNativeCallError::Internal("bound compare: missing __collator__".into())
        })?);

    let data = require_collator_data(&collator_rv, runtime)?.clone();

    let x_val = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let y_val = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let x_str = runtime
        .js_to_string(x_val)
        .map_err(|e| VmNativeCallError::Internal(format!("Collator.compare: {e}").into()))?;
    let y_str = runtime
        .js_to_string(y_val)
        .map_err(|e| VmNativeCallError::Internal(format!("Collator.compare: {e}").into()))?;

    Ok(RegisterValue::from_i32(perform_compare(
        &x_str, &y_str, &data,
    )))
}

fn interp_err(e: impl std::fmt::Debug) -> VmNativeCallError {
    VmNativeCallError::Internal(format!("{e:?}").into())
}

fn perform_compare(x: &str, y: &str, data: &CollatorData) -> i32 {
    let mut options = CollatorOptions::default();

    // Map sensitivity → ICU4X Strength + CaseLevel.
    match data.sensitivity {
        CollatorSensitivity::Base => {
            options.strength = Some(Strength::Primary);
        }
        CollatorSensitivity::Accent => {
            options.strength = Some(Strength::Secondary);
        }
        CollatorSensitivity::Case => {
            options.strength = Some(Strength::Primary);
            options.case_level = Some(CaseLevel::On);
        }
        CollatorSensitivity::Variant => {
            options.strength = Some(Strength::Tertiary);
        }
    }

    let locale = Locale::from_str(&data.locale).unwrap_or_else(|_| {
        Locale::from_str(locale_utils::DEFAULT_LOCALE).expect("default locale should parse")
    });

    let prefs = CollatorPreferences::from(&locale);
    let collator = match CollatorBorrowed::try_new(prefs, options) {
        Ok(c) => c,
        Err(_) => {
            // Fallback to byte comparison if ICU fails.
            return match x.cmp(y) {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            };
        }
    };

    match collator.compare(x, y) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

// ═══════════════════════════════════════════════════════════════════
//  §11.3.5 Intl.Collator.prototype.resolvedOptions()
// ═══════════════════════════════════════════════════════════════════

fn collator_resolved_options(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_collator_data(this, runtime)?.clone();

    let obj = runtime.alloc_object();
    set_string_prop(runtime, obj, "locale", &data.locale);
    set_string_prop(runtime, obj, "usage", data.usage.as_str());
    set_string_prop(runtime, obj, "sensitivity", data.sensitivity.as_str());
    set_bool_prop(runtime, obj, "ignorePunctuation", data.ignore_punctuation);
    set_string_prop(runtime, obj, "collation", &data.collation);
    set_bool_prop(runtime, obj, "numeric", data.numeric);
    set_string_prop(runtime, obj, "caseFirst", data.case_first.as_str());

    Ok(RegisterValue::from_object_handle(obj.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §11.2.2 Intl.Collator.supportedLocalesOf(locales)
// ═══════════════════════════════════════════════════════════════════

fn collator_supported_locales_of(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let locales_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let locale_list = super::canonicalize_locale_list_from_value(locales_arg, runtime)?;
    let arr = runtime.alloc_array();
    for locale in &locale_list {
        let s = runtime.alloc_string(locale.as_str());
        runtime
            .objects_mut()
            .push_element(arr, RegisterValue::from_object_handle(s.0))
            .map_err(|e| {
                VmNativeCallError::Internal(format!("supportedLocalesOf: {e:?}").into())
            })?;
    }
    Ok(RegisterValue::from_object_handle(arr.0))
}

// ═══════════════════════════════════════════════════════════════════
//  Options resolution
// ═══════════════════════════════════════════════════════════════════

fn resolve_collator_options(
    locale: &str,
    options: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<CollatorData, VmNativeCallError> {
    let usage = parse_enum(
        get_option_string(options, "usage", runtime)?,
        CollatorUsage::from_str_opt,
        CollatorUsage::Sort,
        "usage",
        runtime,
    )?;

    let sensitivity = parse_enum(
        get_option_string(options, "sensitivity", runtime)?,
        CollatorSensitivity::from_str_opt,
        CollatorSensitivity::Variant,
        "sensitivity",
        runtime,
    )?;

    let ignore_punctuation =
        get_option_bool(options, "ignorePunctuation", runtime)?.unwrap_or(false);

    let collation =
        get_option_string(options, "collation", runtime)?.unwrap_or_else(|| "default".to_string());

    let numeric = get_option_bool(options, "numeric", runtime)?.unwrap_or(false);

    let case_first = parse_enum(
        get_option_string(options, "caseFirst", runtime)?,
        CollatorCaseFirst::from_str_opt,
        CollatorCaseFirst::False,
        "caseFirst",
        runtime,
    )?;

    Ok(CollatorData {
        locale: locale.to_string(),
        usage,
        sensitivity,
        ignore_punctuation,
        collation,
        numeric,
        case_first,
    })
}

// ═══════════════════════════════════════════════════════════════════
//  Enum as_str / from_str_opt implementations
// ═══════════════════════════════════════════════════════════════════

impl CollatorUsage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sort => "sort",
            Self::Search => "search",
        }
    }
    fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "sort" => Some(Self::Sort),
            "search" => Some(Self::Search),
            _ => None,
        }
    }
}

impl CollatorSensitivity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Accent => "accent",
            Self::Case => "case",
            Self::Variant => "variant",
        }
    }
    fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "base" => Some(Self::Base),
            "accent" => Some(Self::Accent),
            "case" => Some(Self::Case),
            "variant" => Some(Self::Variant),
            _ => None,
        }
    }
}

impl CollatorCaseFirst {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Upper => "upper",
            Self::Lower => "lower",
            Self::False => "false",
        }
    }
    fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "upper" => Some(Self::Upper),
            "lower" => Some(Self::Lower),
            "false" => Some(Self::False),
            _ => None,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Internal helpers
// ═══════════════════════════════════════════════════════════════════

fn require_collator_data<'a>(
    this: &RegisterValue,
    runtime: &'a crate::interpreter::RuntimeState,
) -> Result<&'a CollatorData, VmNativeCallError> {
    let payload = payload::require_intl_payload(this, runtime)
        .map_err(|e| VmNativeCallError::Internal(format!("Collator: {e}").into()))?;
    payload.as_collator().ok_or_else(|| {
        VmNativeCallError::Internal("called on incompatible Intl receiver (not Collator)".into())
    })
}

fn set_string_prop(
    runtime: &mut crate::interpreter::RuntimeState,
    obj: crate::object::ObjectHandle,
    name: &str,
    value: &str,
) {
    let prop = runtime.intern_property_name(name);
    let s = runtime.alloc_string(value);
    let _ = runtime
        .objects_mut()
        .set_property(obj, prop, RegisterValue::from_object_handle(s.0));
}

fn set_bool_prop(
    runtime: &mut crate::interpreter::RuntimeState,
    obj: crate::object::ObjectHandle,
    name: &str,
    value: bool,
) {
    let prop = runtime.intern_property_name(name);
    let _ = runtime
        .objects_mut()
        .set_property(obj, prop, RegisterValue::from_bool(value));
}

fn parse_enum<T>(
    value: Option<String>,
    from_str: fn(&str) -> Option<T>,
    default: T,
    name: &str,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<T, VmNativeCallError> {
    match value {
        None => Ok(default),
        Some(s) => {
            from_str(&s).ok_or_else(|| range_error(runtime, &format!("Invalid {name} option")))
        }
    }
}

fn range_error(runtime: &mut crate::interpreter::RuntimeState, message: &str) -> VmNativeCallError {
    match runtime.alloc_range_error(message) {
        Ok(err) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(err.0)),
        Err(e) => VmNativeCallError::Internal(format!("RangeError alloc: {e}").into()),
    }
}
