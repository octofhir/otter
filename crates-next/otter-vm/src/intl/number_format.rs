//! `Intl.NumberFormat` — locale-aware number formatting.
//!
//! Backed by [`icu_decimal::DecimalFormatter`] for the integer +
//! fractional part. Currency / percent are layered on top via a
//! small symbol table — the foundation prioritises the common
//! `en-US` shape that tests target. Locales outside the table fall
//! back to the bare ISO currency code (e.g. `"USD 1,234.50"`).
//!
//! # See also
//! - <https://tc39.es/ecma402/#sec-intl-numberformat-objects>

use std::str::FromStr;
use std::sync::LazyLock;

use fixed_decimal::Decimal;
use icu_decimal::DecimalFormatter;
use icu_decimal::options::{DecimalFormatterOptions, GroupingStrategy};
use icu_locale::Locale;

use crate::Value;
use crate::intl::dispatch::IntlError;
use crate::intl::helpers::{
    DEFAULT_LOCALE, coerce_locale, js_string, options_object, read_bool_option, read_string_option,
    read_u8_option,
};
use crate::intl::payload::{IntlPayload, NumberFormatPayload};
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::number::NumberValue;

/// Resolve the `(locale, options)` argument pair to a payload.
///
/// # Errors
/// - `BadArgument` when `style == "currency"` is requested without a
///   `currency` option.
pub fn resolve(
    locale: &Value,
    options: &Value,
    gc_heap: &otter_gc::GcHeap,
) -> Result<NumberFormatPayload, IntlError> {
    let opts = options_object(Some(options));
    let opts_ref = opts.as_ref();
    let style = read_string_option(opts_ref, "style", "decimal", gc_heap);
    let currency = match style.as_str() {
        "currency" => {
            match opts_ref.and_then(|o| match crate::object::get(*o, gc_heap, "currency") {
                Some(Value::String(s)) => Some(s.to_lossy_string().to_uppercase()),
                _ => None,
            }) {
                Some(c) => Some(c),
                None => {
                    return Err(IntlError::BadArgument {
                        class: "NumberFormat",
                        method: "constructor",
                        index: 1,
                        reason: "currency style requires a `currency` option",
                    });
                }
            }
        }
        _ => None,
    };
    let (default_min, default_max) = match style.as_str() {
        "currency" => (2, 2),
        "percent" => (0, 0),
        _ => (0, 3),
    };
    let minimum_fraction_digits = read_u8_option(
        opts_ref,
        "minimumFractionDigits",
        default_min,
        0,
        20,
        gc_heap,
    );
    let maximum_fraction_digits = read_u8_option(
        opts_ref,
        "maximumFractionDigits",
        default_max.max(minimum_fraction_digits),
        minimum_fraction_digits,
        20,
        gc_heap,
    );
    let use_grouping = read_bool_option(opts_ref, "useGrouping", true, gc_heap);
    Ok(NumberFormatPayload {
        locale: coerce_locale(Some(locale)),
        style,
        currency,
        minimum_fraction_digits,
        maximum_fraction_digits,
        use_grouping,
    })
}

fn require_number_format<'a>(
    args: &'a IntrinsicArgs<'_>,
) -> Result<&'a NumberFormatPayload, IntrinsicError> {
    match args.receiver {
        Value::Intl(intl) => match intl.payload() {
            IntlPayload::NumberFormat(n) => Ok(n),
            _ => Err(IntrinsicError::BadReceiver {
                expected: "Intl.NumberFormat",
            }),
        },
        _ => Err(IntrinsicError::BadReceiver {
            expected: "Intl.NumberFormat",
        }),
    }
}

fn impl_format(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let payload = require_number_format(args)?;
    let n = match args.args.first() {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.to_lossy_string().parse::<f64>().unwrap_or(f64::NAN),
        Some(Value::Boolean(b)) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Some(Value::Null) => 0.0,
        _ => f64::NAN,
    };
    let rendered = format_number(n, payload);
    js_string(&rendered, args.string_heap).map_err(intl_to_intrinsic)
}

fn impl_resolved_options(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let payload = require_number_format(args)?;
    let locale = js_string_value(&payload.locale, args)?;
    let style = js_string_value(&payload.style, args)?;
    let currency_val = match &payload.currency {
        Some(c) => Some(js_string_value(c, args)?),
        None => None,
    };
    let min_fd = payload.minimum_fraction_digits as i32;
    let max_fd = payload.maximum_fraction_digits as i32;
    let use_grouping = payload.use_grouping;
    let mut heap = args.gc_heap.borrow_mut();
    let obj = crate::object::alloc_object(*heap)?;
    crate::object::set(obj, *heap, "locale", locale);
    crate::object::set(obj, *heap, "style", style);
    if let Some(c) = currency_val {
        crate::object::set(obj, *heap, "currency", c);
    }
    crate::object::set(
        obj,
        *heap,
        "minimumFractionDigits",
        Value::Number(NumberValue::from_i32(min_fd)),
    );
    crate::object::set(
        obj,
        *heap,
        "maximumFractionDigits",
        Value::Number(NumberValue::from_i32(max_fd)),
    );
    crate::object::set(obj, *heap, "useGrouping", Value::Boolean(use_grouping));
    Ok(Value::Object(obj))
}

fn js_string_value(s: &str, args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(Value::String(crate::string::JsString::from_str(
        s,
        args.string_heap,
    )?))
}

fn intl_to_intrinsic(err: IntlError) -> IntrinsicError {
    let _ = err;
    IntrinsicError::BadArgument {
        index: 0,
        reason: "format failed",
    }
}

/// Render `n` per the resolved option bag.
fn format_number(n: f64, payload: &NumberFormatPayload) -> String {
    if n.is_nan() {
        return "NaN".to_string();
    }
    if n.is_infinite() {
        let sign = if n.is_sign_negative() { "-" } else { "" };
        return format!("{sign}∞");
    }
    let value = match payload.style.as_str() {
        "percent" => n * 100.0,
        _ => n,
    };
    let core = format_decimal(value, payload);
    match payload.style.as_str() {
        "currency" => format_currency(&core, payload, n.is_sign_negative()),
        "percent" => format!("{core}%"),
        _ => core,
    }
}

/// Format a number through ICU's `DecimalFormatter`. Falls back to
/// the Rust-side `format!` rendering when ICU instantiation fails.
fn format_decimal(n: f64, payload: &NumberFormatPayload) -> String {
    let locale = Locale::from_str(&payload.locale)
        .or_else(|_| Locale::from_str(DEFAULT_LOCALE))
        .expect("default locale parses");
    let mut options = DecimalFormatterOptions::default();
    options.grouping_strategy = Some(if payload.use_grouping {
        GroupingStrategy::Auto
    } else {
        GroupingStrategy::Never
    });
    let formatter = match DecimalFormatter::try_new((&locale).into(), options) {
        Ok(f) => f,
        Err(_) => return rust_fallback_format(n, payload),
    };
    // Render to the precise number of fraction digits we want:
    // start with `minimumFractionDigits`, round to
    // `maximumFractionDigits`, and trim any trailing zeros above
    // the minimum so `1234567` doesn't surface as `1,234,567.000`.
    let max = payload.maximum_fraction_digits as usize;
    let formatted = format!("{:.max$}", n.abs(), max = max);
    let trimmed = trim_trailing_zero_fraction(&formatted, payload.minimum_fraction_digits as usize);
    let mut decimal = match Decimal::from_str(&trimmed) {
        Ok(d) => d,
        Err(_) => return rust_fallback_format(n, payload),
    };
    decimal.pad_end(-(payload.minimum_fraction_digits as i16));
    let mut out = String::new();
    let _ = writeable::Writeable::write_to(&formatter.format(&decimal), &mut out);
    if n.is_sign_negative() {
        out = format!("-{out}");
    }
    out
}

/// Trim trailing fractional zeros above `min_frac` digits.
fn trim_trailing_zero_fraction(s: &str, min_frac: usize) -> String {
    let Some(dot) = s.find('.') else {
        return s.to_string();
    };
    let allowed_min = dot + 1 + min_frac;
    let mut out = s.to_string();
    while out.len() > allowed_min && out.ends_with('0') {
        out.pop();
    }
    if out.ends_with('.') {
        out.pop();
    }
    out
}

/// Last-resort formatter when ICU rejects the locale: plain Rust
/// `format!` with manual grouping.
fn rust_fallback_format(n: f64, payload: &NumberFormatPayload) -> String {
    let max = payload.maximum_fraction_digits as usize;
    let mut s = format!("{:.max$}", n.abs(), max = max);
    // Trim trailing zeros down to `minimumFractionDigits`.
    if max > payload.minimum_fraction_digits as usize {
        if let Some(dot) = s.find('.') {
            let allowed_min = dot + 1 + payload.minimum_fraction_digits as usize;
            while s.len() > allowed_min && s.ends_with('0') {
                s.pop();
            }
            if s.ends_with('.') {
                s.pop();
            }
        }
    }
    if payload.use_grouping {
        s = group_thousands(&s);
    }
    if n.is_sign_negative() {
        s = format!("-{s}");
    }
    s
}

fn group_thousands(s: &str) -> String {
    let (int_part, frac_part) = s.split_once('.').unwrap_or((s, ""));
    let mut out = String::with_capacity(int_part.len() + int_part.len() / 3);
    let chars: Vec<char> = int_part.chars().collect();
    for (i, ch) in chars.iter().enumerate() {
        if i > 0 && (chars.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*ch);
    }
    if !frac_part.is_empty() {
        out.push('.');
        out.push_str(frac_part);
    }
    out
}

/// Tack a currency symbol / code onto a formatted decimal core.
/// Picks the symbol from a small built-in table (USD, EUR, GBP,
/// JPY, RUB, CNY, INR) and falls back to the ISO code prefix.
fn format_currency(core: &str, payload: &NumberFormatPayload, is_negative: bool) -> String {
    let code = payload.currency.as_deref().unwrap_or("USD");
    let symbol = match code {
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" => "¥",
        "CNY" => "¥",
        "RUB" => "₽",
        "INR" => "₹",
        other => {
            return format!(
                "{}{} {}",
                if is_negative { "-" } else { "" },
                other,
                core.trim_start_matches('-')
            );
        }
    };
    let core_unsigned = core.trim_start_matches('-');
    if is_negative {
        format!("-{symbol}{core_unsigned}")
    } else {
        format!("{symbol}{core_unsigned}")
    }
}

/// `Intl.NumberFormat.prototype` table.
pub static NUMBER_FORMAT_PROTOTYPE_TABLE: LazyLock<IntrinsicTable> = LazyLock::new(|| {
    crate::intrinsics!(
        Intl,
        "format"          / 1 => impl_format,
        "resolvedOptions" / 0 => impl_resolved_options,
    )
});

/// Convenience accessor used by [`super::lookup_prototype`].
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    NUMBER_FORMAT_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Intl, name)
}

/// Static-side dispatch (none today).
pub fn dispatch_static(method: &str, _args: &[Value]) -> Result<Value, IntlError> {
    Err(IntlError::UnknownMember {
        class: "NumberFormat",
        method: method.to_string(),
    })
}
