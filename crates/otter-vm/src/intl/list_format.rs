//! `Intl.ListFormat` — locale-aware list joining.
//!
//! The rendered list and `formatToParts` layout come from ICU4X list
//! formatter data, which implements CLDR list patterns for the selected
//! locale, type, and style.
//!
//! # See also
//! - <https://tc39.es/ecma402/#listformat-objects>

use std::fmt;
use std::str::FromStr;

use icu_list::options::{ListFormatterOptions, ListLength};
use icu_list::{ListFormatter, ListFormatterPreferences};
use icu_locale::Locale;
use otter_gc::raw::RawGc;
use writeable::{Part, PartsWrite, Writeable};

use crate::intl::helpers::{DEFAULT_LOCALE, get_string_option, require_options_object};
use crate::intl::payload::{IntlPayload, ListFormatPayload};
use crate::string::JsString;
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "ListFormat";

/// §13.1.1 InitializeListFormat — spec-faithful construction firing
/// `localeMatcher` / `type` / `style` getters in order with ToString
/// coercion + RangeError validation, and canonicalizing the locale.
pub fn resolve_ctx(
    ctx: &mut NativeCtx<'_>,
    locales: Value,
    options: Value,
) -> Result<ListFormatPayload, NativeError> {
    let requested = crate::intl::supported::canonicalize_locale_list(ctx, locales)?;
    let locale = requested
        .into_iter()
        .next()
        .unwrap_or_else(|| DEFAULT_LOCALE.to_string());
    let options = require_options_object(options, CLASS)?;
    // §step — read in spec order: localeMatcher, type, style.
    let _matcher = get_string_option(
        ctx,
        options,
        "localeMatcher",
        CLASS,
        &["lookup", "best fit"],
        None,
    )?;
    let kind = get_string_option(
        ctx,
        options,
        "type",
        CLASS,
        &["conjunction", "disjunction", "unit"],
        Some("conjunction"),
    )?
    .unwrap_or_else(|| "conjunction".to_string());
    let style = get_string_option(
        ctx,
        options,
        "style",
        CLASS,
        &["long", "short", "narrow"],
        Some("long"),
    )?
    .unwrap_or_else(|| "long".to_string());
    Ok(ListFormatPayload {
        locale,
        kind,
        style,
    })
}

fn require_payload(
    ctx: &NativeCtx<'_>,
    name: &'static str,
) -> Result<ListFormatPayload, NativeError> {
    let bad = || NativeError::TypeError {
        name,
        reason: "intrinsic called on a non-Intl.ListFormat receiver".to_string(),
    };
    let intl = ctx.this_value().as_intl(ctx.heap()).ok_or_else(bad)?;
    match intl.payload_clone(ctx.heap()) {
        IntlPayload::ListFormat(p) => Ok(p),
        _ => Err(bad()),
    }
}

pub(crate) fn join(items: &[String], payload: &ListFormatPayload) -> String {
    let formatter = match formatter_for(payload) {
        Some(formatter) => formatter,
        None => return items.join(", "),
    };
    let mut out = String::new();
    let _ = formatter
        .format(items.iter().map(String::as_str))
        .write_to(&mut out);
    out
}

/// §13.5.3 `Intl.ListFormat.prototype.format(list)`.
pub(crate) fn list_format_format(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_payload(ctx, "format")?;
    let items = crate::intl::helpers::string_list_from_iterable(ctx, args.first(), "format")?;
    let rendered = join(&items, &payload);
    Ok(Value::string(JsString::from_str(
        &rendered,
        ctx.heap_mut(),
    )?))
}

/// §13.5.4 `Intl.ListFormat.prototype.formatToParts(list)`.
pub(crate) fn list_format_format_to_parts(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_payload(ctx, "formatToParts")?;
    let items =
        crate::intl::helpers::string_list_from_iterable(ctx, args.first(), "formatToParts")?;
    let parts = parts_layout(&items, &payload);

    let mut elements: Vec<Value> = Vec::with_capacity(parts.len());
    for (ty, val) in &parts {
        let ty_s = Value::string(JsString::from_str(ty, ctx.heap_mut())?);
        let val_s = Value::string(JsString::from_str(val, ctx.heap_mut())?);
        let snapshot = elements.clone();
        let mut obj = ctx.alloc_object_with_roots(&[&ty_s, &val_s], &[&snapshot])?;
        crate::object::set(&mut obj, ctx.heap_mut(), "type", ty_s);
        crate::object::set(&mut obj, ctx.heap_mut(), "value", val_s);
        elements.push(Value::object(obj));
    }
    let element_roots = elements.clone();
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        for v in &element_roots {
            v.trace_value_slots(visitor);
        }
    };
    let arr = crate::array::from_elements_with_roots(ctx.heap_mut(), elements, &mut visit)?;
    Ok(Value::array(arr))
}

/// Build the `element` / `literal` part layout matching [`join`].
fn parts_layout(items: &[String], payload: &ListFormatPayload) -> Vec<(&'static str, String)> {
    let formatter = match formatter_for(payload) {
        Some(formatter) => formatter,
        None => {
            let joined = items.join(", ");
            return if joined.is_empty() {
                Vec::new()
            } else {
                vec![("literal", joined)]
            };
        }
    };
    let mut collector = ListPartsCollector::default();
    let _ = formatter
        .format(items.iter().map(String::as_str))
        .write_to_parts(&mut collector);
    collector.parts
}

fn formatter_for(payload: &ListFormatPayload) -> Option<ListFormatter> {
    let locale = Locale::from_str(&payload.locale)
        .or_else(|_| Locale::from_str(DEFAULT_LOCALE))
        .ok()?;
    let prefs = ListFormatterPreferences::from(&locale);
    let options = ListFormatterOptions::default().with_length(match payload.style.as_str() {
        "narrow" => ListLength::Narrow,
        "short" => ListLength::Short,
        _ => ListLength::Wide,
    });
    let formatter = match payload.kind.as_str() {
        "disjunction" => ListFormatter::try_new_or(prefs, options),
        "unit" => ListFormatter::try_new_unit(prefs, options),
        _ => ListFormatter::try_new_and(prefs, options),
    };
    formatter.ok()
}

#[derive(Default)]
struct ListPartsCollector {
    current: Option<&'static str>,
    parts: Vec<(&'static str, String)>,
}

impl fmt::Write for ListPartsCollector {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if s.is_empty() {
            return Ok(());
        }
        let ty = self.current.unwrap_or("literal");
        if let Some((last_ty, last_value)) = self.parts.last_mut()
            && *last_ty == ty
        {
            last_value.push_str(s);
            return Ok(());
        }
        self.parts.push((ty, s.to_string()));
        Ok(())
    }
}

impl PartsWrite for ListPartsCollector {
    type SubPartsWrite = Self;

    fn with_part(
        &mut self,
        part: Part,
        mut f: impl FnMut(&mut Self::SubPartsWrite) -> fmt::Result,
    ) -> fmt::Result {
        let previous = self.current;
        self.current = Some(match part.value {
            "element" => "element",
            _ => "literal",
        });
        let result = f(self);
        self.current = previous;
        result
    }
}

/// §13.5.5 `Intl.ListFormat.prototype.resolvedOptions()`.
pub(crate) fn list_format_resolved_options(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_payload(ctx, "resolvedOptions")?;
    let locale = Value::string(JsString::from_str(&payload.locale, ctx.heap_mut())?);
    let kind = Value::string(JsString::from_str(&payload.kind, ctx.heap_mut())?);
    let style = Value::string(JsString::from_str(&payload.style, ctx.heap_mut())?);
    let mut obj = ctx.alloc_object_with_roots(&[&locale, &kind, &style], &[])?;
    if let Some(proto) = ctx.cx.interp.object_prototype_object_opt() {
        crate::object::set_prototype(obj, ctx.heap_mut(), Some(proto));
    }
    let heap = ctx.heap_mut();
    crate::object::set(&mut obj, heap, "locale", locale);
    crate::object::set(&mut obj, heap, "type", kind);
    crate::object::set(&mut obj, heap, "style", style);
    Ok(Value::object(obj))
}
