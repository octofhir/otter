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
use crate::number::NumberValue;

/// Resolve the constructor option bag.
pub fn resolve(locale: &Value, options: &Value) -> CollatorPayload {
    let opts = options_object(Some(options));
    let opts_ref = opts.as_ref();
    CollatorPayload {
        locale: coerce_locale(Some(locale)),
        usage: read_string_option(opts_ref, "usage", "sort"),
        sensitivity: read_string_option(opts_ref, "sensitivity", "variant"),
        ignore_punctuation: read_bool_option(opts_ref, "ignorePunctuation", false),
        numeric: read_bool_option(opts_ref, "numeric", false),
        case_first: read_string_option(opts_ref, "caseFirst", "false"),
    }
}

fn require_collator<'a>(
    args: &'a IntrinsicArgs<'_>,
) -> Result<&'a CollatorPayload, IntrinsicError> {
    match args.receiver {
        Value::Intl(intl) => match intl.payload() {
            IntlPayload::Collator(c) => Ok(c),
            _ => Err(IntrinsicError::BadReceiver {
                expected: "Intl.Collator",
            }),
        },
        _ => Err(IntrinsicError::BadReceiver {
            expected: "Intl.Collator",
        }),
    }
}

fn impl_compare(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let payload = require_collator(args)?;
    let x = match args.args.first() {
        Some(Value::String(s)) => s.to_lossy_string(),
        Some(Value::Number(n)) => n.to_display_string(),
        Some(Value::Boolean(b)) => if *b { "true" } else { "false" }.to_string(),
        _ => return Ok(Value::Number(NumberValue::from_i32(0))),
    };
    let y = match args.args.get(1) {
        Some(Value::String(s)) => s.to_lossy_string(),
        Some(Value::Number(n)) => n.to_display_string(),
        Some(Value::Boolean(b)) => if *b { "true" } else { "false" }.to_string(),
        _ => return Ok(Value::Number(NumberValue::from_i32(0))),
    };
    let n = compare_with_payload(&x, &y, payload);
    Ok(Value::Number(NumberValue::from_i32(n)))
}

fn impl_resolved_options(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let payload = require_collator(args)?;
    let obj = crate::object::JsObject::new();
    obj.set("locale", js_string_value(&payload.locale, args)?);
    obj.set("usage", js_string_value(&payload.usage, args)?);
    obj.set("sensitivity", js_string_value(&payload.sensitivity, args)?);
    obj.set(
        "ignorePunctuation",
        Value::Boolean(payload.ignore_punctuation),
    );
    obj.set("numeric", Value::Boolean(payload.numeric));
    obj.set("caseFirst", js_string_value(&payload.case_first, args)?);
    Ok(Value::Object(obj))
}

fn js_string_value(s: &str, args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(Value::String(crate::string::JsString::from_str(
        s,
        args.string_heap,
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
