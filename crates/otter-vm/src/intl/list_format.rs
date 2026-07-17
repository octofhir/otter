//! `Intl.ListFormat` — locale-aware list joining.
//!
//! The rendered list and `formatToParts` layout come from ICU4X list
//! formatter data, which implements CLDR list patterns for the selected
//! locale, type, and style.
//!
//! # Contents
//! - Locale and option resolution for `Intl.ListFormat`.
//! - ICU-backed `format` and `formatToParts`.
//! - Rooted `resolvedOptions` result construction.
//!
//! # Invariants
//! - Every JS value retained across an allocation lives in one native handle
//!   scope; list-part arrays never rely on copied raw-value root snapshots.
//! - Part objects are installed into their result array as they are built, so
//!   construction is linear in the number of formatted parts.
//!
//! # See also
//! - <https://tc39.es/ecma402/#listformat-objects>

use std::fmt;
use std::str::FromStr;

use icu_list::options::{ListFormatterOptions, ListLength};
use icu_list::{ListFormatter, ListFormatterPreferences};
use icu_locale::Locale;
use writeable::{Part, PartsWrite, Writeable};

use crate::intl::helpers::{DEFAULT_LOCALE, get_string_option, require_options_object};
use crate::intl::payload::{IntlPayload, ListFormatPayload};
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
    ctx.scope(|mut scope| {
        let rendered = scope.string(&rendered)?;
        Ok(scope.finish(rendered))
    })
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

    ctx.scope(|mut scope| {
        let result = scope.array(parts.len())?;
        for (index, (ty, value)) in parts.iter().enumerate() {
            let part = scope.object()?;
            let ty = scope.string(ty)?;
            scope.set(part, "type", ty)?;
            let value = scope.string(value)?;
            scope.set(part, "value", value)?;
            scope.set_index(result, index, part)?;
        }
        Ok(scope.finish(result))
    })
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
    ctx.scope(|mut scope| {
        let options = scope.object()?;
        let locale = scope.string(&payload.locale)?;
        scope.set(options, "locale", locale)?;
        let kind = scope.string(&payload.kind)?;
        scope.set(options, "type", kind)?;
        let style = scope.string(&payload.style)?;
        scope.set(options, "style", style)?;
        Ok(scope.finish(options))
    })
}
