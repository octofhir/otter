//! Baseline `Intl` implementation.
//!
//! This installs `Intl` globally with broad API surface so locale-aware
//! builtins can run and Test262 can exercise shape/branding checks.

use std::collections::HashSet;
use std::sync::Arc;
use unicode_normalization::char::canonical_combining_class;

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;

const DEFAULT_LOCALE: &str = "en-US";
const INTL_LOCALE_KEY: &str = "__intl_locale";
const INTL_COLLATOR_BRAND_KEY: &str = "__intl_collator_brand";
const INTL_DATETIMEFORMAT_BRAND_KEY: &str = "__intl_datetimeformat_brand";
const INTL_NUMBERFORMAT_BRAND_KEY: &str = "__intl_numberformat_brand";
const INTL_PLURALRULES_BRAND_KEY: &str = "__intl_pluralrules_brand";
const INTL_RELATIVETIMEFORMAT_BRAND_KEY: &str = "__intl_relativetimeformat_brand";
const INTL_LISTFORMAT_BRAND_KEY: &str = "__intl_listformat_brand";
const INTL_DISPLAYNAMES_BRAND_KEY: &str = "__intl_displaynames_brand";
const INTL_SEGMENTER_BRAND_KEY: &str = "__intl_segmenter_brand";
const INTL_DURATIONFORMAT_BRAND_KEY: &str = "__intl_durationformat_brand";
const INTL_LOCALE_BRAND_KEY: &str = "__intl_locale_brand";

fn is_turkic_locale(locale: &str) -> bool {
    let lower = locale.to_ascii_lowercase();
    lower == "tr"
        || lower == "az"
        || lower.starts_with("tr-")
        || lower.starts_with("az-")
        || lower.starts_with("tr_")
        || lower.starts_with("az_")
}

fn is_lithuanian_locale(locale: &str) -> bool {
    let lower = locale.to_ascii_lowercase();
    lower == "lt" || lower.starts_with("lt-")
}

fn is_soft_dotted(ch: char) -> bool {
    matches!(
        ch as u32,
        0x0069 | 0x006A | 0x012F | 0x0249 | 0x0268 | 0x029D | 0x02B2 | 0x03F3
            | 0x0456 | 0x0458 | 0x1D62 | 0x1D96 | 0x1DA4 | 0x1DA8 | 0x1E2D | 0x1ECB
            | 0x2071 | 0x2148 | 0x2149 | 0x2C7C | 0x1D422 | 0x1D423 | 0x1D456
            | 0x1D457 | 0x1D48A | 0x1D48B | 0x1D4BE | 0x1D4BF | 0x1D4F2 | 0x1D4F3
            | 0x1D526 | 0x1D527 | 0x1D55A | 0x1D55B | 0x1D58E | 0x1D58F | 0x1D5C2
            | 0x1D5C3 | 0x1D5F6 | 0x1D5F7 | 0x1D62A | 0x1D62B | 0x1D65E | 0x1D65F
            | 0x1D692 | 0x1D693
    )
}

fn turkic_to_lowercase(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == 'I' {
            i += 1;
            let mut marks = Vec::new();
            while i < chars.len() && canonical_combining_class(chars[i]) != 0 {
                marks.push(chars[i]);
                i += 1;
            }
            let mut removable_dot = false;
            let mut seen_above = false;
            for mark in &marks {
                if *mark == '\u{0307}' && !seen_above {
                    removable_dot = true;
                    break;
                }
                if canonical_combining_class(*mark) == 230 {
                    seen_above = true;
                }
            }
            out.push(if removable_dot { 'i' } else { 'ı' });
            let mut seen_above = false;
            for mark in marks {
                if mark == '\u{0307}' && !seen_above {
                    continue;
                }
                if canonical_combining_class(mark) == 230 {
                    seen_above = true;
                }
                out.push(mark);
            }
            continue;
        }
        if ch == 'İ' {
            out.push('i');
            i += 1;
            continue;
        }
        out.extend(ch.to_lowercase());
        i += 1;
    }
    out
}

fn turkic_to_uppercase(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            'i' => out.push('İ'),
            'ı' => out.push('I'),
            _ => out.extend(ch.to_uppercase()),
        }
    }
    out
}

fn lithuanian_to_lowercase(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 8);
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        match ch {
            '\u{00CC}' => {
                out.push('i');
                out.push('\u{0307}');
                out.push('\u{0300}');
                i += 1;
                continue;
            }
            '\u{00CD}' => {
                out.push('i');
                out.push('\u{0307}');
                out.push('\u{0301}');
                i += 1;
                continue;
            }
            '\u{0128}' => {
                out.push('i');
                out.push('\u{0307}');
                out.push('\u{0303}');
                i += 1;
                continue;
            }
            'I' | 'J' | '\u{012E}' => {
                let base = match ch {
                    'I' => 'i',
                    'J' => 'j',
                    _ => '\u{012F}',
                };
                i += 1;
                let mut marks = Vec::new();
                while i < chars.len() && canonical_combining_class(chars[i]) != 0 {
                    marks.push(chars[i]);
                    i += 1;
                }
                let has_above = marks.iter().any(|m| canonical_combining_class(*m) == 230);
                out.push(base);
                if has_above {
                    out.push('\u{0307}');
                }
                for mark in marks {
                    out.push(mark);
                }
                continue;
            }
            _ => {}
        }
        out.extend(ch.to_lowercase());
        i += 1;
    }
    out
}

fn lithuanian_to_uppercase(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if is_soft_dotted(ch) {
            out.extend(ch.to_uppercase());
            i += 1;
            while i < chars.len() && canonical_combining_class(chars[i]) != 0 {
                let mark = chars[i];
                if mark != '\u{0307}' {
                    out.push(mark);
                }
                i += 1;
            }
            continue;
        }
        out.extend(ch.to_uppercase());
        i += 1;
    }
    out
}

fn canonicalize_locale_tag(raw: &str) -> Result<String, VmError> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(VmError::range_error("Invalid language tag"));
    }
    if s.contains('_') {
        return Err(VmError::range_error("Invalid language tag"));
    }
    let mut out = Vec::new();
    for (i, part) in s.split('-').enumerate() {
        if part.is_empty() || part.len() > 8 || !part.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(VmError::range_error("Invalid language tag"));
        }
        if i == 0 && part.len() < 2 {
            return Err(VmError::range_error("Invalid language tag"));
        }
        let canon = if i == 0 {
            part.to_ascii_lowercase()
        } else if part.len() == 4 && part.chars().all(|c| c.is_ascii_alphabetic()) {
            let mut chars = part.chars();
            let first = chars
                .next()
                .map(|c| c.to_ascii_uppercase())
                .unwrap_or_default();
            let rest = chars.as_str().to_ascii_lowercase();
            format!("{first}{rest}")
        } else if (part.len() == 2 && part.chars().all(|c| c.is_ascii_alphabetic()))
            || (part.len() == 3 && part.chars().all(|c| c.is_ascii_digit()))
        {
            part.to_ascii_uppercase()
        } else {
            part.to_ascii_lowercase()
        };
        out.push(canon);
    }
    Ok(out.join("-"))
}

fn canonicalize_locale_list(
    locales: Option<&Value>,
    ncx: &mut NativeContext<'_>,
) -> Result<Vec<String>, VmError> {
    let Some(locales) = locales else {
        return Ok(Vec::new());
    };
    if locales.is_undefined() {
        return Ok(Vec::new());
    }
    if locales.is_null() {
        return Err(VmError::type_error("Invalid locale list"));
    }

    let mut seen = HashSet::new();
    let mut out = Vec::new();

    let mut push_locale = |raw: String| -> Result<(), VmError> {
        let tag = canonicalize_locale_tag(&raw)?;
        if seen.insert(tag.clone()) {
            out.push(tag);
        }
        Ok(())
    };

    if let Some(s) = locales.as_string() {
        push_locale(s.as_str().to_string())?;
        return Ok(out);
    }

    if locales.as_object().is_some() || locales.as_proxy().is_some() {
        let get_prop = |ncx: &mut NativeContext<'_>, key: PropertyKey| -> Result<Value, VmError> {
            if let Some(proxy) = locales.as_proxy() {
                return crate::proxy_operations::proxy_get(
                    ncx,
                    proxy,
                    &key,
                    crate::proxy_operations::property_key_to_value_pub(&key),
                    locales.clone(),
                );
            }
            if let Some(obj) = locales.as_object() {
                return Ok(obj.get(&key).unwrap_or_else(Value::undefined));
            }
            Ok(Value::undefined())
        };

        let has_prop =
            |ncx: &mut NativeContext<'_>, key: PropertyKey| -> Result<bool, VmError> {
                if let Some(proxy) = locales.as_proxy() {
                    return crate::proxy_operations::proxy_has(
                        ncx,
                        proxy,
                        &key,
                        crate::proxy_operations::property_key_to_value_pub(&key),
                    );
                }
                if let Some(obj) = locales.as_object() {
                    return Ok(obj.has(&key));
                }
                Ok(false)
            };

        let length_val = get_prop(ncx, PropertyKey::string("length"))?;
        let len = if length_val.is_undefined() {
            0usize
        } else {
            ncx.to_number_value(&length_val)?.max(0.0).min(64.0).floor() as usize
        };

        for i in 0..len {
            let key = PropertyKey::Index(i as u32);
            if !has_prop(ncx, key)? {
                continue;
            }
            let v = get_prop(ncx, key)?;
            if !v.is_string() && !v.is_object() && !v.is_proxy() {
                return Err(VmError::type_error(
                    "Locale list element must be String or Object",
                ));
            }
            push_locale(ncx.to_string_value(&v)?)?;
        }
        return Ok(out);
    }

    push_locale(ncx.to_string_value(locales)?)?;
    Ok(out)
}

fn first_requested_locale(
    locales: Option<&Value>,
    ncx: &mut NativeContext<'_>,
) -> Result<Option<String>, VmError> {
    let list = canonicalize_locale_list(locales, ncx)?;
    Ok(list.first().cloned())
}

fn normalize_locale_for_ops(locale: Option<String>) -> String {
    locale.unwrap_or_else(|| DEFAULT_LOCALE.to_string())
}

fn create_array(ncx: &mut NativeContext<'_>, length: usize) -> GcRef<JsObject> {
    let arr = GcRef::new(JsObject::array(length, ncx.memory_manager().clone()));
    if let Some(array_ctor) = ncx.ctx.get_global("Array").and_then(|v| v.as_object())
        && let Some(proto) = array_ctor.get(&PropertyKey::string("prototype"))
    {
        arr.set_prototype(proto);
    }
    arr
}

fn create_plain_object(ncx: &mut NativeContext<'_>) -> GcRef<JsObject> {
    if let Some(object_ctor) = ncx.ctx.get_global("Object").and_then(|v| v.as_object())
        && let Some(proto) = object_ctor.get(&PropertyKey::string("prototype"))
    {
        return GcRef::new(JsObject::new(proto, ncx.memory_manager().clone()));
    }
    GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()))
}

fn locale_from_receiver(
    this_val: &Value,
    brand_key: &str,
    method_name: &str,
) -> Result<String, VmError> {
    let this_obj = this_val
        .as_object()
        .ok_or_else(|| VmError::type_error(format!("{method_name} called on non-object")))?;
    if this_obj.get(&PropertyKey::string(brand_key)).is_none() {
        return Err(VmError::type_error(format!(
            "{method_name} called on incompatible receiver"
        )));
    }
    Ok(this_obj
        .get(&PropertyKey::string(INTL_LOCALE_KEY))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
        .unwrap_or_else(|| DEFAULT_LOCALE.to_string()))
}

fn resolved_options(this_val: &Value, brand_key: &str, method_name: &str, ncx: &mut NativeContext<'_>) -> Result<Value, VmError> {
    let locale = locale_from_receiver(this_val, brand_key, method_name)?;
    let obj = create_plain_object(ncx);
    obj.define_property(
        PropertyKey::string("locale"),
        PropertyDescriptor::builtin_data(Value::string(JsString::intern(&locale))),
    );
    Ok(Value::object(obj))
}

fn validate_collator_options(
    options: Option<&Value>,
    ncx: &mut NativeContext<'_>,
) -> Result<(), VmError> {
    let Some(options) = options else {
        return Ok(());
    };
    if options.is_undefined() {
        return Ok(());
    }
    let obj = options
        .as_object()
        .ok_or_else(|| VmError::type_error("Options must be an object"))?;

    if let Some(v) = obj.get(&PropertyKey::string("localeMatcher")) && !v.is_undefined() {
        let s = ncx.to_string_value(&v)?;
        if s != "lookup" && s != "best fit" {
            return Err(VmError::range_error("Invalid localeMatcher option"));
        }
    }
    if let Some(v) = obj.get(&PropertyKey::string("usage")) && !v.is_undefined() {
        let s = ncx.to_string_value(&v)?;
        if s != "sort" && s != "search" {
            return Err(VmError::range_error("Invalid usage option"));
        }
    }
    if let Some(v) = obj.get(&PropertyKey::string("sensitivity")) && !v.is_undefined() {
        let s = ncx.to_string_value(&v)?;
        if s != "base" && s != "accent" && s != "case" && s != "variant" {
            return Err(VmError::range_error("Invalid sensitivity option"));
        }
    }
    Ok(())
}

fn supported_locales_of_impl(
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let locales = canonicalize_locale_list(args.first(), ncx)?;
    let arr = create_array(ncx, locales.len());
    for (i, locale) in locales.iter().enumerate() {
        let _ = arr.set(PropertyKey::Index(i as u32), Value::string(JsString::intern(locale)));
    }
    Ok(Value::array(arr))
}

fn install_supported_locales_of(
    ctor: &GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    fn_proto: GcRef<JsObject>,
) {
    let supported_locales_of = Value::native_function_with_proto_named(
        |_this, args, ncx| supported_locales_of_impl(args, ncx),
        mm.clone(),
        fn_proto,
        "supportedLocalesOf",
        1,
    );
    ctor.define_property(
        PropertyKey::string("supportedLocalesOf"),
        PropertyDescriptor::builtin_method(supported_locales_of),
    );
}

fn install_common_constructor_bits(
    ctor_obj: &GcRef<JsObject>,
    ctor_name: &str,
    ctor_len: u32,
    prototype_obj: GcRef<JsObject>,
) {
    ctor_obj.define_property(
        PropertyKey::string("prototype"),
        PropertyDescriptor::data_with_attrs(
            Value::object(prototype_obj),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: false,
            },
        ),
    );
    ctor_obj.define_property(
        PropertyKey::string("name"),
        PropertyDescriptor::function_length(Value::string(JsString::intern(ctor_name))),
    );
    ctor_obj.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::function_length(Value::number(ctor_len as f64)),
    );
    let _ = ctor_obj.set(
        PropertyKey::string("__non_constructor"),
        Value::boolean(false),
    );
}

fn install_basic_intl_constructor(
    intl: &GcRef<JsObject>,
    ctor_name: &str,
    brand_key: &'static str,
    ctor_len: u32,
    prototype_methods: &[(&str, u32, Value)],
    mm: &Arc<MemoryManager>,
    object_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
) -> Value {
    let proto_obj = GcRef::new(JsObject::new(Value::object(object_proto), mm.clone()));
    for (name, _length, value) in prototype_methods {
        proto_obj.define_property(
            PropertyKey::string(*name),
            PropertyDescriptor::builtin_method(value.clone()),
        );
    }

    let ctor_obj = GcRef::new(JsObject::new(Value::object(fn_proto.clone()), mm.clone()));
    install_common_constructor_bits(&ctor_obj, ctor_name, ctor_len, proto_obj);

    let ctor = Value::native_function_with_proto_and_object(
        Arc::new({
            let proto_obj = proto_obj;
            move |this, args, ncx| {
                let locale = normalize_locale_for_ops(first_requested_locale(args.first(), ncx)?);
                if brand_key == INTL_COLLATOR_BRAND_KEY {
                    validate_collator_options(args.get(1), ncx)?;
                }
                let target = if ncx.is_construct() {
                    this.as_object()
                        .ok_or_else(|| VmError::type_error("Intl constructor requires object receiver"))?
                } else {
                    GcRef::new(JsObject::new(
                        Value::object(proto_obj),
                        ncx.memory_manager().clone(),
                    ))
                };
                target.define_property(
                    PropertyKey::string(brand_key),
                    PropertyDescriptor::data_with_attrs(
                        Value::boolean(true),
                        PropertyAttributes::permanent(),
                    ),
                );
                target.define_property(
                    PropertyKey::string(INTL_LOCALE_KEY),
                    PropertyDescriptor::data_with_attrs(
                        Value::string(JsString::intern(&locale)),
                        PropertyAttributes::permanent(),
                    ),
                );
                Ok(Value::object(target))
            }
        }),
        mm.clone(),
        fn_proto.clone(),
        ctor_obj.clone(),
    );

    proto_obj.define_property(
        PropertyKey::string("constructor"),
        PropertyDescriptor::data_with_attrs(ctor.clone(), PropertyAttributes::constructor_link()),
    );

    install_supported_locales_of(&ctor_obj, mm, fn_proto);

    intl.define_property(
        PropertyKey::string(ctor_name),
        PropertyDescriptor::data_with_attrs(ctor.clone(), PropertyAttributes::builtin_method()),
    );

    ctor
}

pub fn to_locale_lowercase(
    input: &str,
    locales: Option<&Value>,
    ncx: &mut NativeContext<'_>,
) -> Result<String, VmError> {
    let locale = normalize_locale_for_ops(first_requested_locale(locales, ncx)?);
    if is_turkic_locale(&locale) {
        return Ok(turkic_to_lowercase(input));
    }
    if is_lithuanian_locale(&locale) {
        return Ok(lithuanian_to_lowercase(input));
    }
    Ok(input.to_lowercase())
}

pub fn to_locale_uppercase(
    input: &str,
    locales: Option<&Value>,
    ncx: &mut NativeContext<'_>,
) -> Result<String, VmError> {
    let locale = normalize_locale_for_ops(first_requested_locale(locales, ncx)?);
    if is_turkic_locale(&locale) {
        return Ok(turkic_to_uppercase(input));
    }
    if is_lithuanian_locale(&locale) {
        return Ok(lithuanian_to_uppercase(input));
    }
    Ok(input.to_uppercase())
}

pub fn locale_compare(
    left: &str,
    right: &str,
    locales: Option<&Value>,
    options: Option<&Value>,
    ncx: &mut NativeContext<'_>,
) -> Result<f64, VmError> {
    use unicode_normalization::UnicodeNormalization;

    validate_collator_options(options, ncx)?;
    let locale = normalize_locale_for_ops(first_requested_locale(locales, ncx)?);
    let left_norm = left.nfc().collect::<String>();
    let right_norm = right.nfc().collect::<String>();

    let ord = if is_turkic_locale(&locale) {
        turkic_to_lowercase(&left_norm).cmp(&turkic_to_lowercase(&right_norm))
    } else {
        let left_fold = left_norm.to_lowercase();
        let right_fold = right_norm.to_lowercase();
        let fold_ord = left_fold.cmp(&right_fold);
        if fold_ord != std::cmp::Ordering::Equal {
            fold_ord
        } else if left_norm == right_norm {
            std::cmp::Ordering::Equal
        } else {
            let mut tie = std::cmp::Ordering::Equal;
            for (lch, rch) in left_norm.chars().zip(right_norm.chars()) {
                if lch == rch {
                    continue;
                }
                let ll = lch.to_lowercase().to_string();
                let rl = rch.to_lowercase().to_string();
                if ll == rl {
                    let l_lower = lch.is_lowercase();
                    let r_lower = rch.is_lowercase();
                    if l_lower != r_lower {
                        tie = if l_lower {
                            std::cmp::Ordering::Less
                        } else {
                            std::cmp::Ordering::Greater
                        };
                        break;
                    }
                }
                tie = lch.cmp(&rch);
                break;
            }
            if tie == std::cmp::Ordering::Equal {
                left_norm.len().cmp(&right_norm.len())
            } else {
                tie
            }
        }
    };
    Ok(match ord {
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Equal => 0.0,
        std::cmp::Ordering::Greater => 1.0,
    })
}

pub fn install_intl(
    global: GcRef<JsObject>,
    object_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    let intl = GcRef::new(JsObject::new(Value::object(object_proto), mm.clone()));

    let get_canonical_locales = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let locales = canonicalize_locale_list(args.first(), ncx)?;
            let arr = create_array(ncx, locales.len());
            for (i, locale) in locales.iter().enumerate() {
                let _ = arr.set(PropertyKey::Index(i as u32), Value::string(JsString::intern(locale)));
            }
            Ok(Value::array(arr))
        },
        mm.clone(),
        fn_proto.clone(),
        "getCanonicalLocales",
        1,
    );
    intl.define_property(
        PropertyKey::string("getCanonicalLocales"),
        PropertyDescriptor::builtin_method(get_canonical_locales),
    );

    let supported_values_of = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let key = ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
            let values: &[&str] = match key.as_str() {
                "calendar" => &["gregory", "iso8601"],
                "collation" => &["default"],
                "currency" => &["USD", "EUR"],
                "numberingSystem" => &["latn"],
                "timeZone" => &["UTC"],
                "unit" => &["meter", "second"],
                _ => return Err(VmError::range_error("Invalid key for Intl.supportedValuesOf")),
            };
            let arr = create_array(ncx, values.len());
            for (i, value) in values.iter().enumerate() {
                let _ = arr.set(
                    PropertyKey::Index(i as u32),
                    Value::string(JsString::intern(value)),
                );
            }
            Ok(Value::array(arr))
        },
        mm.clone(),
        fn_proto.clone(),
        "supportedValuesOf",
        1,
    );
    intl.define_property(
        PropertyKey::string("supportedValuesOf"),
        PropertyDescriptor::builtin_method(supported_values_of),
    );

    let collator_compare_getter = Value::native_function_with_proto_named(
        |this_val, _args, ncx| {
            let locale = locale_from_receiver(
                this_val,
                INTL_COLLATOR_BRAND_KEY,
                "Intl.Collator.prototype.compare",
            )?;
            let fn_proto = ncx
                .ctx
                .function_prototype()
                .ok_or_else(|| VmError::type_error("Function.prototype is not available"))?;
            let compare = Value::native_function_with_proto_named(
                move |_this, args, ncx| {
                    let left = args.first().cloned().unwrap_or(Value::undefined());
                    let right = args.get(1).cloned().unwrap_or(Value::undefined());
                    let l = ncx.to_string_value(&left)?;
                    let r = ncx.to_string_value(&right)?;
                    let locale_value = Value::string(JsString::intern(&locale));
                    Ok(Value::number(locale_compare(
                        &l,
                        &r,
                        Some(&locale_value),
                        None,
                        ncx,
                    )?))
                },
                ncx.memory_manager().clone(),
                fn_proto,
                "",
                2,
            );
            Ok(compare)
        },
        mm.clone(),
        fn_proto.clone(),
        "get compare",
        0,
    );
    let collator_resolved_options = Value::native_function_with_proto_named(
        |this_val, _args, ncx| {
            resolved_options(
                this_val,
                INTL_COLLATOR_BRAND_KEY,
                "Intl.Collator.prototype.resolvedOptions",
                ncx,
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "resolvedOptions",
        0,
    );
    let collator_ctor = install_basic_intl_constructor(
        &intl,
        "Collator",
        INTL_COLLATOR_BRAND_KEY,
        0,
        &[("resolvedOptions", 0, collator_resolved_options)],
        mm,
        object_proto,
        fn_proto.clone(),
    );
    if let Some(collator_ctor_obj) = collator_ctor.as_object()
        && let Some(collator_proto_val) = collator_ctor_obj.get(&PropertyKey::string("prototype"))
        && let Some(collator_proto_obj) = collator_proto_val.as_object()
    {
        collator_proto_obj.define_property(
            PropertyKey::string("compare"),
            PropertyDescriptor::getter(collator_compare_getter),
        );
    }

    let datetime_format = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let value = args.first().cloned().unwrap_or(Value::undefined());
            let s = ncx.to_string_value(&value)?;
            Ok(Value::string(JsString::intern(&s)))
        },
        mm.clone(),
        fn_proto.clone(),
        "format",
        1,
    );
    let datetime_resolved_options = Value::native_function_with_proto_named(
        |this_val, _args, ncx| {
            resolved_options(
                this_val,
                INTL_DATETIMEFORMAT_BRAND_KEY,
                "Intl.DateTimeFormat.prototype.resolvedOptions",
                ncx,
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "resolvedOptions",
        0,
    );
    install_basic_intl_constructor(
        &intl,
        "DateTimeFormat",
        INTL_DATETIMEFORMAT_BRAND_KEY,
        0,
        &[("format", 1, datetime_format), ("resolvedOptions", 0, datetime_resolved_options)],
        mm,
        object_proto,
        fn_proto.clone(),
    );

    let number_format = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let value = args.first().cloned().unwrap_or(Value::number(0.0));
            let number = ncx.to_number_value(&value)?;
            Ok(Value::string(JsString::intern(&number.to_string())))
        },
        mm.clone(),
        fn_proto.clone(),
        "format",
        1,
    );
    let number_format_to_parts = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let value = args.first().cloned().unwrap_or(Value::number(0.0));
            let number = ncx.to_number_value(&value)?;
            let part = create_plain_object(ncx);
            part.define_property(
                PropertyKey::string("type"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern("integer"))),
            );
            part.define_property(
                PropertyKey::string("value"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern(&number.to_string()))),
            );
            let arr = create_array(ncx, 1);
            let _ = arr.set(PropertyKey::Index(0), Value::object(part));
            Ok(Value::array(arr))
        },
        mm.clone(),
        fn_proto.clone(),
        "formatToParts",
        1,
    );
    let number_resolved_options = Value::native_function_with_proto_named(
        |this_val, _args, ncx| {
            resolved_options(
                this_val,
                INTL_NUMBERFORMAT_BRAND_KEY,
                "Intl.NumberFormat.prototype.resolvedOptions",
                ncx,
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "resolvedOptions",
        0,
    );
    install_basic_intl_constructor(
        &intl,
        "NumberFormat",
        INTL_NUMBERFORMAT_BRAND_KEY,
        0,
        &[
            ("format", 1, number_format),
            ("formatToParts", 1, number_format_to_parts),
            ("resolvedOptions", 0, number_resolved_options),
        ],
        mm,
        object_proto,
        fn_proto.clone(),
    );

    let plural_select = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let value = args.first().cloned().unwrap_or(Value::number(0.0));
            let n = ncx.to_number_value(&value)?;
            let sel = if n == 1.0 { "one" } else { "other" };
            Ok(Value::string(JsString::intern(sel)))
        },
        mm.clone(),
        fn_proto.clone(),
        "select",
        1,
    );
    let plural_select_range = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let start = ncx.to_number_value(args.first().unwrap_or(&Value::number(0.0)))?;
            let end = ncx.to_number_value(args.get(1).unwrap_or(&Value::number(0.0)))?;
            let sel = if start == 1.0 && end == 1.0 {
                "one"
            } else {
                "other"
            };
            Ok(Value::string(JsString::intern(sel)))
        },
        mm.clone(),
        fn_proto.clone(),
        "selectRange",
        2,
    );
    let plural_resolved_options = Value::native_function_with_proto_named(
        |this_val, _args, ncx| {
            resolved_options(
                this_val,
                INTL_PLURALRULES_BRAND_KEY,
                "Intl.PluralRules.prototype.resolvedOptions",
                ncx,
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "resolvedOptions",
        0,
    );
    install_basic_intl_constructor(
        &intl,
        "PluralRules",
        INTL_PLURALRULES_BRAND_KEY,
        0,
        &[
            ("select", 1, plural_select),
            ("selectRange", 2, plural_select_range),
            ("resolvedOptions", 0, plural_resolved_options),
        ],
        mm,
        object_proto,
        fn_proto.clone(),
    );

    let rtf_format = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let value = args.first().cloned().unwrap_or(Value::number(0.0));
            let unit = args.get(1).cloned().unwrap_or(Value::string(JsString::intern("second")));
            let v = ncx.to_string_value(&value)?;
            let u = ncx.to_string_value(&unit)?;
            Ok(Value::string(JsString::intern(&format!("{v} {u}"))))
        },
        mm.clone(),
        fn_proto.clone(),
        "format",
        2,
    );
    let rtf_format_to_parts = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let formatted = {
                let value = args.first().cloned().unwrap_or(Value::number(0.0));
                let unit = args.get(1).cloned().unwrap_or(Value::string(JsString::intern("second")));
                let v = ncx.to_string_value(&value)?;
                let u = ncx.to_string_value(&unit)?;
                format!("{v} {u}")
            };
            let part = create_plain_object(ncx);
            part.define_property(
                PropertyKey::string("type"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern("literal"))),
            );
            part.define_property(
                PropertyKey::string("value"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern(&formatted))),
            );
            let arr = create_array(ncx, 1);
            let _ = arr.set(PropertyKey::Index(0), Value::object(part));
            Ok(Value::array(arr))
        },
        mm.clone(),
        fn_proto.clone(),
        "formatToParts",
        2,
    );
    let rtf_resolved_options = Value::native_function_with_proto_named(
        |this_val, _args, ncx| {
            resolved_options(
                this_val,
                INTL_RELATIVETIMEFORMAT_BRAND_KEY,
                "Intl.RelativeTimeFormat.prototype.resolvedOptions",
                ncx,
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "resolvedOptions",
        0,
    );
    install_basic_intl_constructor(
        &intl,
        "RelativeTimeFormat",
        INTL_RELATIVETIMEFORMAT_BRAND_KEY,
        0,
        &[
            ("format", 2, rtf_format),
            ("formatToParts", 2, rtf_format_to_parts),
            ("resolvedOptions", 0, rtf_resolved_options),
        ],
        mm,
        object_proto,
        fn_proto.clone(),
    );

    let list_format = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            if let Some(v) = args.first() {
                if let Some(obj) = v.as_object() {
                    let length = obj
                        .get(&PropertyKey::string("length"))
                        .map(|v| ncx.to_number_value(&v).unwrap_or(0.0).max(0.0).min(64.0).floor() as usize)
                        .unwrap_or(0);
                    let mut parts = Vec::new();
                    for i in 0..length {
                        if let Some(item) = obj.get(&PropertyKey::Index(i as u32)) {
                            parts.push(ncx.to_string_value(&item)?);
                        }
                    }
                    return Ok(Value::string(JsString::intern(&parts.join(", "))));
                }
            }
            let s = ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
            Ok(Value::string(JsString::intern(&s)))
        },
        mm.clone(),
        fn_proto.clone(),
        "format",
        1,
    );
    let list_format_to_parts = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let formatted = if let Some(v) = args.first() {
                ncx.to_string_value(v)?
            } else {
                String::new()
            };
            let part = create_plain_object(ncx);
            part.define_property(
                PropertyKey::string("type"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern("element"))),
            );
            part.define_property(
                PropertyKey::string("value"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern(&formatted))),
            );
            let arr = create_array(ncx, 1);
            let _ = arr.set(PropertyKey::Index(0), Value::object(part));
            Ok(Value::array(arr))
        },
        mm.clone(),
        fn_proto.clone(),
        "formatToParts",
        1,
    );
    let list_resolved_options = Value::native_function_with_proto_named(
        |this_val, _args, ncx| {
            resolved_options(
                this_val,
                INTL_LISTFORMAT_BRAND_KEY,
                "Intl.ListFormat.prototype.resolvedOptions",
                ncx,
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "resolvedOptions",
        0,
    );
    install_basic_intl_constructor(
        &intl,
        "ListFormat",
        INTL_LISTFORMAT_BRAND_KEY,
        0,
        &[
            ("format", 1, list_format),
            ("formatToParts", 1, list_format_to_parts),
            ("resolvedOptions", 0, list_resolved_options),
        ],
        mm,
        object_proto,
        fn_proto.clone(),
    );

    let display_names_of = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let code = ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
            Ok(Value::string(JsString::intern(&code)))
        },
        mm.clone(),
        fn_proto.clone(),
        "of",
        1,
    );
    let display_names_resolved_options = Value::native_function_with_proto_named(
        |this_val, _args, ncx| {
            resolved_options(
                this_val,
                INTL_DISPLAYNAMES_BRAND_KEY,
                "Intl.DisplayNames.prototype.resolvedOptions",
                ncx,
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "resolvedOptions",
        0,
    );
    install_basic_intl_constructor(
        &intl,
        "DisplayNames",
        INTL_DISPLAYNAMES_BRAND_KEY,
        0,
        &[
            ("of", 1, display_names_of),
            ("resolvedOptions", 0, display_names_resolved_options),
        ],
        mm,
        object_proto,
        fn_proto.clone(),
    );

    let segmenter_segment = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let input = ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
            let result = create_plain_object(ncx);
            result.define_property(
                PropertyKey::string("input"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern(&input))),
            );
            let segments = create_array(ncx, 1);
            let part = create_plain_object(ncx);
            part.define_property(
                PropertyKey::string("segment"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern(&input))),
            );
            let _ = segments.set(PropertyKey::Index(0), Value::object(part));
            result.define_property(
                PropertyKey::string("segments"),
                PropertyDescriptor::builtin_data(Value::array(segments)),
            );
            Ok(Value::object(result))
        },
        mm.clone(),
        fn_proto.clone(),
        "segment",
        1,
    );
    let segmenter_resolved_options = Value::native_function_with_proto_named(
        |this_val, _args, ncx| {
            resolved_options(
                this_val,
                INTL_SEGMENTER_BRAND_KEY,
                "Intl.Segmenter.prototype.resolvedOptions",
                ncx,
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "resolvedOptions",
        0,
    );
    install_basic_intl_constructor(
        &intl,
        "Segmenter",
        INTL_SEGMENTER_BRAND_KEY,
        0,
        &[
            ("segment", 1, segmenter_segment),
            ("resolvedOptions", 0, segmenter_resolved_options),
        ],
        mm,
        object_proto,
        fn_proto.clone(),
    );

    let duration_format = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let value = args.first().cloned().unwrap_or(Value::undefined());
            let s = ncx.to_string_value(&value)?;
            Ok(Value::string(JsString::intern(&s)))
        },
        mm.clone(),
        fn_proto.clone(),
        "format",
        1,
    );
    let duration_format_to_parts = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let formatted = ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
            let part = create_plain_object(ncx);
            part.define_property(
                PropertyKey::string("type"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern("literal"))),
            );
            part.define_property(
                PropertyKey::string("value"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern(&formatted))),
            );
            let arr = create_array(ncx, 1);
            let _ = arr.set(PropertyKey::Index(0), Value::object(part));
            Ok(Value::array(arr))
        },
        mm.clone(),
        fn_proto.clone(),
        "formatToParts",
        1,
    );
    let duration_resolved_options = Value::native_function_with_proto_named(
        |this_val, _args, ncx| {
            resolved_options(
                this_val,
                INTL_DURATIONFORMAT_BRAND_KEY,
                "Intl.DurationFormat.prototype.resolvedOptions",
                ncx,
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "resolvedOptions",
        0,
    );
    install_basic_intl_constructor(
        &intl,
        "DurationFormat",
        INTL_DURATIONFORMAT_BRAND_KEY,
        0,
        &[
            ("format", 1, duration_format),
            ("formatToParts", 1, duration_format_to_parts),
            ("resolvedOptions", 0, duration_resolved_options),
        ],
        mm,
        object_proto,
        fn_proto.clone(),
    );

    let locale_to_string = Value::native_function_with_proto_named(
        |this_val, _args, _ncx| {
            let locale = locale_from_receiver(
                this_val,
                INTL_LOCALE_BRAND_KEY,
                "Intl.Locale.prototype.toString",
            )?;
            Ok(Value::string(JsString::intern(&locale)))
        },
        mm.clone(),
        fn_proto.clone(),
        "toString",
        0,
    );
    let locale_maximize = Value::native_function_with_proto_named(
        |this_val, _args, _ncx| {
            let _ = locale_from_receiver(
                this_val,
                INTL_LOCALE_BRAND_KEY,
                "Intl.Locale.prototype.maximize",
            )?;
            Ok(this_val.clone())
        },
        mm.clone(),
        fn_proto.clone(),
        "maximize",
        0,
    );
    let locale_minimize = Value::native_function_with_proto_named(
        |this_val, _args, _ncx| {
            let _ = locale_from_receiver(
                this_val,
                INTL_LOCALE_BRAND_KEY,
                "Intl.Locale.prototype.minimize",
            )?;
            Ok(this_val.clone())
        },
        mm.clone(),
        fn_proto.clone(),
        "minimize",
        0,
    );
    install_basic_intl_constructor(
        &intl,
        "Locale",
        INTL_LOCALE_BRAND_KEY,
        1,
        &[
            ("toString", 0, locale_to_string),
            ("maximize", 0, locale_maximize),
            ("minimize", 0, locale_minimize),
        ],
        mm,
        object_proto,
        fn_proto,
    );

    if let Some(symbol_ctor) = global
        .get(&PropertyKey::string("Symbol"))
        .and_then(|v| v.as_object())
        && let Some(to_string_tag) = symbol_ctor
            .get(&PropertyKey::string("toStringTag"))
            .and_then(|v| v.as_symbol())
    {
        intl.define_property(
            PropertyKey::Symbol(to_string_tag),
            PropertyDescriptor::data_with_attrs(
                Value::string(JsString::intern("Intl")),
                PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: true,
                },
            ),
        );
    } else {
        intl.define_property(
            PropertyKey::string("@@toStringTag"),
            PropertyDescriptor::data_with_attrs(
                Value::string(JsString::intern("Intl")),
                PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: true,
                },
            ),
        );
    }

    global.define_property(
        PropertyKey::string("Intl"),
        PropertyDescriptor::data_with_attrs(
            Value::object(intl),
            PropertyAttributes::builtin_method(),
        ),
    );
    let _ = global.set(PropertyKey::string("__Intl_Collator"), collator_ctor);
}
