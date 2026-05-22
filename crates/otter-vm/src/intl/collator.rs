//! `Intl.Collator` — locale-aware string comparison.
//!
//! Backed by [`icu_collator::Collator`]. Collator instances are
//! constructed lazily inside [`compare`] from the resolved options
//! cached on the [`crate::intl::payload::CollatorPayload`].
//!
//! # See also
//! - <https://tc39.es/ecma402/#sec-intl-collator-objects>

use std::cmp::Ordering;
use std::str::FromStr;
use std::sync::LazyLock;

use icu_collator::options::CollatorOptions;
use icu_collator::{Collator, CollatorPreferences};
use icu_locale::Locale;

use crate::Value;
use crate::intl::dispatch::IntlError;
use crate::intl::helpers::{
    DEFAULT_LOCALE, coerce_locale, options_object, read_bool_option, read_string_option,
};
use crate::intl::payload::{CollatorPayload, IntlPayload};
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};

/// Resolve the constructor option bag.
pub fn resolve(locale: &Value, options: &Value, gc_heap: &otter_gc::GcHeap) -> CollatorPayload {
    let opts = options_object(Some(options));
    let opts_ref = opts.as_ref();
    CollatorPayload {
        locale: coerce_locale(Some(locale), gc_heap),
        usage: read_string_option(opts_ref, "usage", "sort", gc_heap),
        sensitivity: read_string_option(opts_ref, "sensitivity", "variant", gc_heap),
        ignore_punctuation: read_bool_option(opts_ref, "ignorePunctuation", false, gc_heap),
        numeric: read_bool_option(opts_ref, "numeric", false, gc_heap),
        case_first: read_string_option(opts_ref, "caseFirst", "false", gc_heap),
    }
}

fn require_collator(args: &IntrinsicArgs<'_>) -> Result<CollatorPayload, IntrinsicError> {
    let bad = || IntrinsicError::BadReceiver {
        expected: "Intl.Collator",
    };
    let intl = args.receiver.as_intl(args.gc_heap).ok_or_else(bad)?;
    match intl.payload_clone(args.gc_heap) {
        IntlPayload::Collator(c) => Ok(c),
        _ => Err(bad()),
    }
}

fn coerce_compare_arg(value: Option<&Value>, heap: &otter_gc::GcHeap) -> Option<String> {
    let v = value?;
    if let Some(s) = v.as_string(heap) {
        return Some(s.to_lossy_string(heap));
    }
    if let Some(n) = v.as_number() {
        return Some(n.to_display_string());
    }
    if let Some(b) = v.as_boolean() {
        return Some((if b { "true" } else { "false" }).to_string());
    }
    None
}

fn impl_compare(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let payload = require_collator(args)?;
    let Some(x) = coerce_compare_arg(args.args.first(), args.gc_heap) else {
        return Ok(Value::number_i32(0));
    };
    let Some(y) = coerce_compare_arg(args.args.get(1), args.gc_heap) else {
        return Ok(Value::number_i32(0));
    };
    let n = compare_with_payload(&x, &y, &payload);
    Ok(Value::number_i32(n))
}

fn impl_resolved_options(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let payload = require_collator(args)?;
    let payload_locale = payload.locale.clone();
    let payload_usage = payload.usage.clone();
    let payload_sensitivity = payload.sensitivity.clone();
    let payload_case_first = payload.case_first.clone();
    let locale = js_string_value(&payload_locale, args)?;
    let usage = js_string_value(&payload_usage, args)?;
    let sensitivity = js_string_value(&payload_sensitivity, args)?;
    let case_first = js_string_value(&payload_case_first, args)?;
    let ignore_punctuation = payload.ignore_punctuation;
    let numeric = payload.numeric;
    let obj = args.alloc_object_rooted(&[&locale, &usage, &sensitivity, &case_first], &[])?;
    let heap = &mut *args.gc_heap;
    crate::object::set(obj, heap, "locale", locale);
    crate::object::set(obj, heap, "usage", usage);
    crate::object::set(obj, heap, "sensitivity", sensitivity);
    crate::object::set(
        obj,
        heap,
        "ignorePunctuation",
        Value::boolean(ignore_punctuation),
    );
    crate::object::set(obj, heap, "numeric", Value::boolean(numeric));
    crate::object::set(obj, heap, "caseFirst", case_first);
    Ok(Value::object(obj))
}

fn js_string_value(s: &str, args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(Value::string(crate::string::JsString::from_str(
        s,
        args.gc_heap,
    )?))
}

/// Run an ICU comparison with the resolved options. Falls back to
/// byte-wise comparison if ICU instantiation fails (the spec
/// requires a stable result; defaulting to byte comparison keeps
/// the surface usable even on exotic tags).
fn compare_with_payload(x: &str, y: &str, payload: &CollatorPayload) -> i32 {
    let locale = Locale::from_str(&payload.locale)
        .or_else(|_| Locale::from_str(DEFAULT_LOCALE))
        .expect("default locale parses");
    let prefs = CollatorPreferences::from(&locale);
    let mut options = CollatorOptions::default();
    let (strength, case_level) = payload.icu_strength();
    options.strength = strength;
    options.case_level = case_level;
    match Collator::try_new(prefs, options) {
        Ok(collator) => match collator.compare(x, y) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        },
        Err(_) => match x.cmp(y) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        },
    }
}

/// `Intl.Collator.prototype` table.
pub static COLLATOR_PROTOTYPE_TABLE: LazyLock<IntrinsicTable> = LazyLock::new(|| {
    crate::intrinsics!(
        Intl,
        "compare"          / 2 => impl_compare,
        "resolvedOptions"  / 0 => impl_resolved_options,
    )
});

/// Convenience accessor used by [`super::lookup_prototype`].
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    COLLATOR_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Intl, name)
}

/// Static side: `Intl.Collator.<member>`. No statics today; reserved
/// for `supportedLocalesOf` once a locale-list helper lands.
pub fn dispatch_static(method: &str, _args: &[Value]) -> Result<Value, IntlError> {
    Err(IntlError::UnknownMember {
        class: "Collator",
        method: method.to_string(),
    })
}
