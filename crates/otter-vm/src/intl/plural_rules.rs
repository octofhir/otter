//! `Intl.PluralRules` — locale-aware plural-category selection.
//!
//! Foundation surface ships English cardinal / ordinal rules:
//! - cardinal: `one` for `n === 1`, `other` otherwise.
//! - ordinal: `one`/`two`/`few` for the canonical English suffixes
//!   (1st, 2nd, 3rd), `other` otherwise.
//!
//! Other locales fall back to the same rules — full ICU CLDR plural
//! tables are filed alongside the wider Intl follow-up. The surface
//! returns spec-shape values so user code that switches on the
//! result keeps working under every locale; the foundation just
//! biases toward English categories.
//!
//! # See also
//! - <https://tc39.es/ecma402/#pluralrules-objects>

use std::sync::LazyLock;

use crate::Value;
use crate::intl::dispatch::IntlError;
use crate::intl::helpers::{
    coerce_locale, js_string, options_object, read_string_option, read_u8_option,
};
use crate::intl::payload::{IntlPayload, PluralRulesPayload};
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};

/// §15.2.1 — resolve constructor options.
pub fn resolve(locale: &Value, options: &Value, gc_heap: &otter_gc::GcHeap) -> PluralRulesPayload {
    let opts = options_object(Some(options));
    let opts_ref = opts.as_ref();
    PluralRulesPayload {
        locale: coerce_locale(Some(locale), gc_heap),
        kind: read_string_option(opts_ref, "type", "cardinal", gc_heap),
        minimum_integer_digits: read_u8_option(opts_ref, "minimumIntegerDigits", 1, 1, 21, gc_heap),
        minimum_fraction_digits: read_u8_option(
            opts_ref,
            "minimumFractionDigits",
            0,
            0,
            20,
            gc_heap,
        ),
        maximum_fraction_digits: read_u8_option(
            opts_ref,
            "maximumFractionDigits",
            3,
            0,
            20,
            gc_heap,
        ),
    }
}

fn require_payload(args: &IntrinsicArgs<'_>) -> Result<PluralRulesPayload, IntrinsicError> {
    let bad = || IntrinsicError::BadReceiver {
        expected: "Intl.PluralRules",
    };
    let intl = args.receiver.as_intl().ok_or_else(bad)?;
    match intl.payload_clone(args.gc_heap) {
        IntlPayload::PluralRules(p) => Ok(p),
        _ => Err(bad()),
    }
}

/// §15.5.4 — `Intl.PluralRules.prototype.select(value)`.
fn impl_select(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let payload = require_payload(args)?;
    let first = args.args.first();
    let n = if let Some(n) = first.and_then(|v| v.as_number()) {
        n.as_f64()
    } else if let Some(b) = first.and_then(|v| v.as_boolean()) {
        if b { 1.0 } else { 0.0 }
    } else if first.is_some_and(|v| v.is_null()) {
        0.0
    } else {
        f64::NAN
    };
    Ok(Value::string(crate::string::JsString::from_str(
        plural_category_en(n, &payload.kind),
        args.gc_heap,
    )?))
}

/// §15.5.5 — `Intl.PluralRules.prototype.resolvedOptions()`.
fn impl_resolved_options(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let payload = require_payload(args)?;
    let locale = js_string(&payload.locale, args.gc_heap).map_err(intl_to_intrinsic)?;
    let kind = js_string(&payload.kind, args.gc_heap).map_err(intl_to_intrinsic)?;
    let mid = payload.minimum_integer_digits as i32;
    let mfd = payload.minimum_fraction_digits as i32;
    let xfd = payload.maximum_fraction_digits as i32;
    let obj = args.alloc_object_rooted(&[&locale, &kind], &[])?;
    let heap = &mut *args.gc_heap;
    crate::object::set(obj, heap, "locale", locale);
    crate::object::set(obj, heap, "type", kind);
    crate::object::set(obj, heap, "minimumIntegerDigits", Value::number_i32(mid));
    crate::object::set(obj, heap, "minimumFractionDigits", Value::number_i32(mfd));
    crate::object::set(obj, heap, "maximumFractionDigits", Value::number_i32(xfd));
    Ok(Value::object(obj))
}

/// English plural-category fallback. `kind` is `"cardinal"` or
/// `"ordinal"`. Negative inputs use absolute value.
fn plural_category_en(n: f64, kind: &str) -> &'static str {
    if n.is_nan() {
        return "other";
    }
    let abs = n.abs();
    if kind == "ordinal" {
        let i = abs as i64;
        let mod10 = i % 10;
        let mod100 = i % 100;
        if mod10 == 1 && mod100 != 11 {
            return "one";
        }
        if mod10 == 2 && mod100 != 12 {
            return "two";
        }
        if mod10 == 3 && mod100 != 13 {
            return "few";
        }
        return "other";
    }
    if (abs - 1.0).abs() < f64::EPSILON {
        "one"
    } else {
        "other"
    }
}

fn intl_to_intrinsic(err: IntlError) -> IntrinsicError {
    match err {
        IntlError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => IntrinsicError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
        _ => IntrinsicError::BadReceiver {
            expected: "Intl.PluralRules",
        },
    }
}

/// `Intl.PluralRules.prototype` table.
pub static PLURAL_RULES_PROTOTYPE_TABLE: LazyLock<IntrinsicTable> = LazyLock::new(|| {
    crate::intrinsics!(
        Intl,
        "select"           / 1 => impl_select,
        "resolvedOptions"  / 0 => impl_resolved_options,
    )
});

/// Convenience accessor used by [`super::lookup_prototype`].
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    PLURAL_RULES_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Intl, name)
}
