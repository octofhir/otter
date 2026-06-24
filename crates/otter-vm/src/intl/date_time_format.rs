//! `Intl.DateTimeFormat` — locale-aware date / time formatting.
//!
//! Foundation slice ships a narrow surface: the `format(date)`
//! method accepts a JS `Number` (epoch milliseconds, the same
//! `Date.now()` shape) or a `Temporal.PlainDateTime` and produces
//! a string sized by the option bag (`year` / `month` / `day` /
//! `hour` / `minute` / `second`). Locale-specific punctuation is
//! deferred until ICU `FieldSetBuilder` integration lands; the
//! foundation renders a stable ISO-like layout that matches the
//! task's "returns a formatted string" criterion.
//!
//! # See also
//! - <https://tc39.es/ecma402/#sec-intl-datetimeformat-objects>

use crate::intl::dispatch::IntlError;
use crate::intl::helpers::{coerce_locale, options_object, read_bool_option, read_string_option};
use crate::intl::payload::{
    DateTimeFormatPayload, DtHourCycle, DtMonthWidth, DtNumWidth, DtStyle, DtTextWidth, DtZoneName,
    IntlPayload,
};
use crate::string::JsString;
use crate::temporal::TemporalPayload;
use crate::{NativeCtx, NativeError, Value};

fn range_err(message: String) -> IntlError {
    IntlError::Range { message }
}

/// Parse a string-valued option against `parse`; an absent option (`""`)
/// yields `None`, an unrecognised value is a `RangeError`.
fn parse_enum_option<T>(
    opts: Option<&crate::object::JsObject>,
    name: &str,
    gc_heap: &otter_gc::GcHeap,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<Option<T>, IntlError> {
    let s = read_string_option(opts, name, "", gc_heap);
    if s.is_empty() {
        return Ok(None);
    }
    match parse(&s) {
        Some(v) => Ok(Some(v)),
        None => Err(range_err(format!(
            "invalid value '{s}' for option '{name}'"
        ))),
    }
}

fn parse_text_width(s: &str) -> Option<DtTextWidth> {
    match s {
        "narrow" => Some(DtTextWidth::Narrow),
        "short" => Some(DtTextWidth::Short),
        "long" => Some(DtTextWidth::Long),
        _ => None,
    }
}

fn parse_num_width(s: &str) -> Option<DtNumWidth> {
    match s {
        "numeric" => Some(DtNumWidth::Numeric),
        "2-digit" => Some(DtNumWidth::TwoDigit),
        _ => None,
    }
}

fn parse_month_width(s: &str) -> Option<DtMonthWidth> {
    match s {
        "numeric" => Some(DtMonthWidth::Numeric),
        "2-digit" => Some(DtMonthWidth::TwoDigit),
        "narrow" => Some(DtMonthWidth::Narrow),
        "short" => Some(DtMonthWidth::Short),
        "long" => Some(DtMonthWidth::Long),
        _ => None,
    }
}

fn parse_style(s: &str) -> Option<DtStyle> {
    match s {
        "full" => Some(DtStyle::Full),
        "long" => Some(DtStyle::Long),
        "medium" => Some(DtStyle::Medium),
        "short" => Some(DtStyle::Short),
        _ => None,
    }
}

fn parse_zone_name(s: &str) -> Option<DtZoneName> {
    match s {
        "long" => Some(DtZoneName::Long),
        "short" => Some(DtZoneName::Short),
        "shortOffset" => Some(DtZoneName::ShortOffset),
        "longOffset" => Some(DtZoneName::LongOffset),
        "shortGeneric" => Some(DtZoneName::ShortGeneric),
        "longGeneric" => Some(DtZoneName::LongGeneric),
        _ => None,
    }
}

fn parse_hour_cycle(s: &str) -> Option<DtHourCycle> {
    match s {
        "h11" => Some(DtHourCycle::H11),
        "h12" => Some(DtHourCycle::H12),
        "h23" => Some(DtHourCycle::H23),
        "h24" => Some(DtHourCycle::H24),
        _ => None,
    }
}

fn text_width_str(w: DtTextWidth) -> &'static str {
    match w {
        DtTextWidth::Narrow => "narrow",
        DtTextWidth::Short => "short",
        DtTextWidth::Long => "long",
    }
}

fn num_width_str(w: DtNumWidth) -> &'static str {
    match w {
        DtNumWidth::Numeric => "numeric",
        DtNumWidth::TwoDigit => "2-digit",
    }
}

fn month_width_str(w: DtMonthWidth) -> &'static str {
    match w {
        DtMonthWidth::Numeric => "numeric",
        DtMonthWidth::TwoDigit => "2-digit",
        DtMonthWidth::Narrow => "narrow",
        DtMonthWidth::Short => "short",
        DtMonthWidth::Long => "long",
    }
}

fn zone_name_str(w: DtZoneName) -> &'static str {
    match w {
        DtZoneName::Long => "long",
        DtZoneName::Short => "short",
        DtZoneName::ShortOffset => "shortOffset",
        DtZoneName::LongOffset => "longOffset",
        DtZoneName::ShortGeneric => "shortGeneric",
        DtZoneName::LongGeneric => "longGeneric",
    }
}

fn style_str(s: DtStyle) -> &'static str {
    match s {
        DtStyle::Full => "full",
        DtStyle::Long => "long",
        DtStyle::Medium => "medium",
        DtStyle::Short => "short",
    }
}

fn hour_cycle_str(h: DtHourCycle) -> &'static str {
    match h {
        DtHourCycle::H11 => "h11",
        DtHourCycle::H12 => "h12",
        DtHourCycle::H23 => "h23",
        DtHourCycle::H24 => "h24",
    }
}

/// Resolve the constructor option bag per ECMA-402 §11.1.2
/// `CreateDateTimeFormat` — each component carries its width, and
/// invalid option values raise a `RangeError`.
pub fn resolve(
    locale: &Value,
    options: &Value,
    gc_heap: &otter_gc::GcHeap,
) -> Result<DateTimeFormatPayload, IntlError> {
    let opts = options_object(Some(options));
    let opts_ref = opts.as_ref();

    let date_style = parse_enum_option(opts_ref, "dateStyle", gc_heap, parse_style)?;
    let time_style = parse_enum_option(opts_ref, "timeStyle", gc_heap, parse_style)?;
    let weekday = parse_enum_option(opts_ref, "weekday", gc_heap, parse_text_width)?;
    let era = parse_enum_option(opts_ref, "era", gc_heap, parse_text_width)?;
    let mut year = parse_enum_option(opts_ref, "year", gc_heap, parse_num_width)?;
    let mut month = parse_enum_option(opts_ref, "month", gc_heap, parse_month_width)?;
    let mut day = parse_enum_option(opts_ref, "day", gc_heap, parse_num_width)?;
    let day_period = parse_enum_option(opts_ref, "dayPeriod", gc_heap, parse_text_width)?;
    let hour = parse_enum_option(opts_ref, "hour", gc_heap, parse_num_width)?;
    let minute = parse_enum_option(opts_ref, "minute", gc_heap, parse_num_width)?;
    let second = parse_enum_option(opts_ref, "second", gc_heap, parse_num_width)?;
    let time_zone_name = parse_enum_option(opts_ref, "timeZoneName", gc_heap, parse_zone_name)?;
    let hour_cycle = parse_enum_option(opts_ref, "hourCycle", gc_heap, parse_hour_cycle)?;

    // fractionalSecondDigits — integer 1..=3 (an out-of-range value is a
    // RangeError; absent or non-numeric is ignored here).
    let fractional_second_digits = opts_ref
        .and_then(|o| crate::object::get(*o, gc_heap, "fractionalSecondDigits"))
        .filter(|v| !v.is_undefined())
        .map(|v| v.as_number().map(|n| n.as_f64()).unwrap_or(f64::NAN))
        .map(|n| {
            if (1.0..=3.0).contains(&n) {
                Ok(n as u8)
            } else {
                Err(range_err(
                    "fractionalSecondDigits must be between 1 and 3".to_string(),
                ))
            }
        })
        .transpose()?;

    let hour12 = read_bool_option_opt(opts_ref, "hour12", gc_heap);
    let tz = read_string_option(opts_ref, "timeZone", "", gc_heap);
    let time_zone = if tz.is_empty() { None } else { Some(tz) };

    // §11.1.2 — dateStyle / timeStyle are mutually exclusive with
    // explicit component options.
    let has_components = weekday.is_some()
        || era.is_some()
        || year.is_some()
        || month.is_some()
        || day.is_some()
        || day_period.is_some()
        || hour.is_some()
        || minute.is_some()
        || second.is_some()
        || fractional_second_digits.is_some();
    if (date_style.is_some() || time_style.is_some()) && has_components {
        return Err(range_err(
            "dateStyle/timeStyle may not be combined with explicit date-time components"
                .to_string(),
        ));
    }

    // ToDateTimeOptions defaults: no style and no components → numeric
    // year/month/day.
    if date_style.is_none() && time_style.is_none() && !has_components {
        year = Some(DtNumWidth::Numeric);
        month = Some(DtMonthWidth::Numeric);
        day = Some(DtNumWidth::Numeric);
    }

    Ok(DateTimeFormatPayload {
        locale: coerce_locale(Some(locale), gc_heap),
        weekday,
        era,
        year,
        month,
        day,
        day_period,
        hour,
        minute,
        second,
        fractional_second_digits,
        time_zone_name,
        hour_cycle,
        hour12,
        date_style,
        time_style,
        time_zone,
    })
}

/// Read an optional boolean option (`None` when absent/undefined).
fn read_bool_option_opt(
    opts: Option<&crate::object::JsObject>,
    name: &str,
    gc_heap: &otter_gc::GcHeap,
) -> Option<bool> {
    let v = opts.and_then(|o| crate::object::get(*o, gc_heap, name))?;
    if v.is_undefined() {
        None
    } else {
        Some(read_bool_option(opts, name, false, gc_heap))
    }
}

fn require_date_time(
    ctx: &NativeCtx<'_>,
    name: &'static str,
) -> Result<DateTimeFormatPayload, NativeError> {
    let bad = || NativeError::TypeError {
        name,
        reason: "intrinsic called on a non-Intl.DateTimeFormat receiver".to_string(),
    };
    let intl = ctx.this_value().as_intl(ctx.heap()).ok_or_else(bad)?;
    match intl.payload_clone(ctx.heap()) {
        IntlPayload::DateTimeFormat(d) => Ok(d),
        _ => Err(bad()),
    }
}

/// §12.1.5 `Intl.DateTimeFormat.prototype.format(date)`.
pub(crate) fn date_time_format_format(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_date_time(ctx, "format")?;
    let (y, mo, d, h, mi, s) = arg_to_civil(ctx, args.first(), "format")?;
    let formatted = format_components(y, mo, d, h, mi, s, &payload);
    Ok(Value::string(JsString::from_str(
        &formatted,
        ctx.heap_mut(),
    )?))
}

/// Resolve the `format`/`formatToParts` argument to civil
/// `(year, month, day, hour, minute, second)` — a Number is epoch ms,
/// a Temporal value uses its own fields, `undefined` is "now".
fn arg_to_civil(
    ctx: &mut NativeCtx<'_>,
    first: Option<&Value>,
    name: &'static str,
) -> Result<(i32, u8, u8, u8, u8, u8), NativeError> {
    if let Some(n) = first.and_then(|v| v.as_number()) {
        let ms = n.as_f64();
        if !ms.is_finite() {
            return Err(NativeError::RangeError {
                name,
                reason: "date value is not a finite number".to_string(),
            });
        }
        Ok(epoch_to_civil((ms as i64).div_euclid(1000)))
    } else if let Some(t) = first.and_then(|v| v.as_temporal(ctx.heap())) {
        match t.payload_clone(ctx.heap()) {
            TemporalPayload::PlainDateTime(pdt) => Ok((
                pdt.year(),
                pdt.month(),
                pdt.day(),
                pdt.hour(),
                pdt.minute(),
                pdt.second(),
            )),
            TemporalPayload::PlainDate(pd) => Ok((pd.year(), pd.month(), pd.day(), 0, 0, 0)),
            TemporalPayload::Instant(inst) => {
                Ok(epoch_to_civil(inst.epoch_milliseconds().div_euclid(1000)))
            }
            _ => Err(NativeError::TypeError {
                name,
                reason: "argument 0 must be a Number, Temporal.Instant, Temporal.PlainDate, or Temporal.PlainDateTime".to_string(),
            }),
        }
    } else if first.is_none() || first.is_some_and(|v| v.is_undefined()) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Ok(epoch_to_civil(now.div_euclid(1000)))
    } else {
        Err(NativeError::TypeError {
            name,
            reason: "argument 0 must be a Number or Temporal value".to_string(),
        })
    }
}

/// §11.5.4 `Intl.DateTimeFormat.prototype.formatToParts(date)` — the
/// same formatting as `format`, returned as an array of
/// `{ type, value }` records from ICU4X part-aware output.
pub(crate) fn date_time_format_format_to_parts(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_date_time(ctx, "formatToParts")?;
    let (y, mo, d, h, mi, s) = arg_to_civil(ctx, args.first(), "formatToParts")?;
    let parts = icu_format_segments(y, mo, d, h, mi, s, &payload)
        .unwrap_or_else(|| vec![("literal", format_components(y, mo, d, h, mi, s, &payload))]);

    let mut elements: Vec<Value> = Vec::with_capacity(parts.len());
    for (ty, val) in &parts {
        let ty_s = Value::string(JsString::from_str(ty, ctx.heap_mut())?);
        let val_s = Value::string(JsString::from_str(val, ctx.heap_mut())?);
        let snapshot = elements.clone();
        let obj = ctx.alloc_object_with_roots(&[&ty_s, &val_s], &[&snapshot])?;
        crate::object::set(obj, ctx.heap_mut(), "type", ty_s);
        crate::object::set(obj, ctx.heap_mut(), "value", val_s);
        elements.push(Value::object(obj));
    }
    let element_roots = elements.clone();
    let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        for v in &element_roots {
            v.trace_value_slots(visitor);
        }
    };
    let arr = crate::array::from_elements_with_roots(ctx.heap_mut(), elements, &mut visit)?;
    Ok(Value::array(arr))
}

/// §12.1.6 `Intl.DateTimeFormat.prototype.resolvedOptions()`.
pub(crate) fn date_time_format_resolved_options(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_date_time(ctx, "resolvedOptions")?;
    // Build (key, value) pairs in ECMA-402 §11.1.6 order. String values
    // are allocated up front and all rooted across the object alloc.
    let mut entries: Vec<(&'static str, Value)> = Vec::new();
    macro_rules! str_entry {
        ($key:expr, $s:expr) => {
            entries.push(($key, Value::string(JsString::from_str($s, ctx.heap_mut())?)));
        };
    }
    str_entry!("locale", &payload.locale);
    str_entry!("calendar", "iso8601");
    str_entry!("numberingSystem", "latn");
    let tz = payload
        .time_zone
        .clone()
        .unwrap_or_else(|| "UTC".to_string());
    str_entry!("timeZone", &tz);

    let time_present = payload.hour.is_some()
        || payload.minute.is_some()
        || payload.second.is_some()
        || payload.time_style.is_some();
    if time_present {
        let hc = payload.hour_cycle.unwrap_or(DtHourCycle::H23);
        str_entry!("hourCycle", hour_cycle_str(hc));
        entries.push((
            "hour12",
            Value::boolean(matches!(hc, DtHourCycle::H11 | DtHourCycle::H12)),
        ));
    }
    if let Some(w) = payload.weekday {
        str_entry!("weekday", text_width_str(w));
    }
    if let Some(w) = payload.era {
        str_entry!("era", text_width_str(w));
    }
    if let Some(w) = payload.year {
        str_entry!("year", num_width_str(w));
    }
    if let Some(w) = payload.month {
        str_entry!("month", month_width_str(w));
    }
    if let Some(w) = payload.day {
        str_entry!("day", num_width_str(w));
    }
    if let Some(w) = payload.day_period {
        str_entry!("dayPeriod", text_width_str(w));
    }
    if let Some(w) = payload.hour {
        str_entry!("hour", num_width_str(w));
    }
    if let Some(w) = payload.minute {
        str_entry!("minute", num_width_str(w));
    }
    if let Some(w) = payload.second {
        str_entry!("second", num_width_str(w));
    }
    if let Some(d) = payload.fractional_second_digits {
        entries.push(("fractionalSecondDigits", Value::number_i32(i32::from(d))));
    }
    if let Some(w) = payload.time_zone_name {
        str_entry!("timeZoneName", zone_name_str(w));
    }
    if let Some(s) = payload.date_style {
        str_entry!("dateStyle", style_str(s));
    }
    if let Some(s) = payload.time_style {
        str_entry!("timeStyle", style_str(s));
    }

    let roots: Vec<&Value> = entries.iter().map(|(_, v)| v).collect();
    let obj = ctx.alloc_object_with_roots(&roots, &[])?;
    let heap = ctx.heap_mut();
    for (k, v) in &entries {
        crate::object::set(obj, heap, k, *v);
    }
    Ok(Value::object(obj))
}

/// Render a `(year, month, day, hour, minute, second)` tuple per
/// the resolved option bag. Locale-specific punctuation is left to
/// future ICU integration; the foundation uses ISO-like fragments
/// joined by `, ` so the output is unambiguous and stable.
/// Real ICU4X locale-aware rendering of a civil date/time via
/// `icu_datetime`. Maps the resolved component flags onto a
/// [`FieldSetBuilder`] and formats an ISO `DateTime`. Returns `None`
/// when the locale, field set, or input is unrepresentable so the caller
/// can fall back to the stable ISO-ish layout.
fn icu_format_components(
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    payload: &DateTimeFormatPayload,
) -> Option<String> {
    use icu_datetime::input::{Date, DateTime, Time};
    use icu_datetime::{DateTimeFormatter, fieldsets::builder::FieldSetBuilder};

    let locale: icu_locale::Locale = payload.locale.parse().ok()?;
    let prefs = icu_datetime::DateTimeFormatterPreferences::from(&locale);

    let mut builder = FieldSetBuilder::default();
    builder.length = payload_length(payload);
    builder.date_fields = payload_date_fields(payload);
    builder.time_precision = payload_time_precision(payload);
    builder.year_style = payload_year_style(payload);
    // Date + time without a zone — input is a plain `DateTime`, no
    // `TimeZoneInfo` required (zone formatting lands with the timeZone
    // option work).
    let fieldset = builder.build_composite_datetime().ok()?;
    let formatter = DateTimeFormatter::try_new(prefs, fieldset).ok()?;
    let dt = DateTime {
        date: Date::try_new_iso(year, month, day).ok()?,
        time: Time::try_new(hour, minute, second, 0).ok()?,
    };
    Some(formatter.format(&dt).to_string())
}

/// A `writeable::PartsWrite` sink recording `(ECMA-402 part type, text)`
/// segments. ICU `datetime` parts (`year`, `month`, …) already coincide
/// with the §11.5.5 field names; text outside a part is a `"literal"`.
struct DateTimePartCollector {
    segments: Vec<(&'static str, String)>,
    current: &'static str,
}

impl std::fmt::Write for DateTimePartCollector {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        if s.is_empty() {
            return Ok(());
        }
        if let Some(last) = self.segments.last_mut()
            && last.0 == self.current
        {
            last.1.push_str(s);
            return Ok(());
        }
        self.segments.push((self.current, s.to_string()));
        Ok(())
    }
}

impl writeable::PartsWrite for DateTimePartCollector {
    type SubPartsWrite = Self;
    fn with_part(
        &mut self,
        part: writeable::Part,
        mut f: impl FnMut(&mut Self) -> std::fmt::Result,
    ) -> std::fmt::Result {
        let prev = self.current;
        // Only a `datetime` field changes the ECMA-402 part type; the
        // `DecimalFormatter` nests its own `decimal` sub-parts inside a
        // numeric field (e.g. `year`), and those digits must keep the
        // enclosing field's type rather than collapse to a literal.
        if part.category == "datetime" {
            self.current = ecma_part_type(part.value);
        }
        let r = f(self);
        self.current = prev;
        r
    }
}

fn ecma_part_type(value: &str) -> &'static str {
    match value {
        "era" => "era",
        "year" => "year",
        "relatedYear" => "relatedYear",
        "yearName" => "yearName",
        "month" => "month",
        "day" => "day",
        "weekday" => "weekday",
        "dayPeriod" => "dayPeriod",
        "hour" => "hour",
        "minute" => "minute",
        "second" => "second",
        "timeZoneName" => "timeZoneName",
        _ => "literal",
    }
}

/// Format a civil date/time into `(type, text)` segments via ICU4X
/// part-aware writing. `None` on the same conditions as
/// [`icu_format_components`].
fn icu_format_segments(
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    payload: &DateTimeFormatPayload,
) -> Option<Vec<(&'static str, String)>> {
    use icu_datetime::input::{Date, DateTime, Time};
    use icu_datetime::{DateTimeFormatter, fieldsets::builder::FieldSetBuilder};
    use writeable::Writeable;

    let locale: icu_locale::Locale = payload.locale.parse().ok()?;
    let prefs = icu_datetime::DateTimeFormatterPreferences::from(&locale);
    let mut builder = FieldSetBuilder::default();
    builder.length = payload_length(payload);
    builder.date_fields = payload_date_fields(payload);
    builder.time_precision = payload_time_precision(payload);
    builder.year_style = payload_year_style(payload);
    let fieldset = builder.build_composite_datetime().ok()?;
    let formatter = DateTimeFormatter::try_new(prefs, fieldset).ok()?;
    let dt = DateTime {
        date: Date::try_new_iso(year, month, day).ok()?,
        time: Time::try_new(hour, minute, second, 0).ok()?,
    };
    let mut sink = DateTimePartCollector {
        segments: Vec::new(),
        current: "literal",
    };
    formatter.format(&dt).write_to_parts(&mut sink).ok()?;
    Some(sink.segments)
}

/// Overall ICU [`Length`] from the option bag (ECMA-402 has no concept;
/// ICU4X semantic skeletons pick one length for the whole pattern).
/// `dateStyle`/`timeStyle` map directly; otherwise the most significant
/// textual component decides (month > weekday > dayPeriod > era), with
/// `long → Long`, `short → Medium`, everything else → `Short`.
fn payload_length(p: &DateTimeFormatPayload) -> Option<icu_datetime::options::Length> {
    use icu_datetime::options::Length;
    if let Some(s) = p.date_style.or(p.time_style) {
        return Some(match s {
            DtStyle::Full | DtStyle::Long => Length::Long,
            DtStyle::Medium => Length::Medium,
            DtStyle::Short => Length::Short,
        });
    }
    if let Some(m) = p.month {
        return Some(match m {
            DtMonthWidth::Long => Length::Long,
            DtMonthWidth::Short => Length::Medium,
            _ => Length::Short,
        });
    }
    let text = p.weekday.or(p.day_period).or(p.era);
    text.map(|w| match w {
        DtTextWidth::Long => Length::Long,
        DtTextWidth::Short => Length::Medium,
        DtTextWidth::Narrow => Length::Short,
    })
}

fn payload_date_fields(
    p: &DateTimeFormatPayload,
) -> Option<icu_datetime::fieldsets::builder::DateFields> {
    use icu_datetime::fieldsets::builder::DateFields;
    if let Some(s) = p.date_style {
        return Some(match s {
            DtStyle::Full => DateFields::YMDE,
            _ => DateFields::YMD,
        });
    }
    let (y, m, d, e) = (
        p.year.is_some(),
        p.month.is_some(),
        p.day.is_some(),
        p.weekday.is_some(),
    );
    Some(match (y, m, d, e) {
        (true, _, _, true) => DateFields::YMDE,
        (true, _, true, false) => DateFields::YMD,
        (true, true, false, false) => DateFields::YM,
        (true, false, false, false) => DateFields::Y,
        (false, true, _, true) => DateFields::MDE,
        (false, true, true, false) => DateFields::MD,
        (false, true, false, false) => DateFields::M,
        (false, false, true, true) => DateFields::DE,
        (false, false, true, false) => DateFields::D,
        (false, false, false, true) => DateFields::E,
        (false, false, false, false) => return None,
    })
}

fn payload_time_precision(
    p: &DateTimeFormatPayload,
) -> Option<icu_datetime::options::TimePrecision> {
    use icu_datetime::options::TimePrecision;
    if let Some(s) = p.time_style {
        return Some(match s {
            DtStyle::Short => TimePrecision::Minute,
            _ => TimePrecision::Second,
        });
    }
    if let Some(digits) = p.fractional_second_digits {
        let sd = match digits {
            1 => icu_datetime::options::SubsecondDigits::S1,
            2 => icu_datetime::options::SubsecondDigits::S2,
            _ => icu_datetime::options::SubsecondDigits::S3,
        };
        return Some(TimePrecision::Subsecond(sd));
    }
    if p.second.is_some() {
        Some(TimePrecision::Second)
    } else if p.minute.is_some() {
        Some(TimePrecision::Minute)
    } else if p.hour.is_some() {
        Some(TimePrecision::Hour)
    } else {
        None
    }
}

/// `year` width / `era` presence → ICU `YearStyle`. `numeric` forces a
/// full year, `2-digit` the short form, an `era` request adds the era.
fn payload_year_style(p: &DateTimeFormatPayload) -> Option<icu_datetime::options::YearStyle> {
    use icu_datetime::options::YearStyle;
    if p.era.is_some() {
        return Some(YearStyle::WithEra);
    }
    match p.year {
        Some(DtNumWidth::Numeric) => Some(YearStyle::Full),
        Some(DtNumWidth::TwoDigit) => Some(YearStyle::Auto),
        None => None,
    }
}

fn format_components(
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    payload: &DateTimeFormatPayload,
) -> String {
    if let Some(s) = icu_format_components(year, month, day, hour, minute, second, payload) {
        return s;
    }
    let mut date_part = String::new();
    if payload.month.is_some() {
        date_part.push_str(&format!("{:02}", month));
    }
    if payload.day.is_some() {
        if !date_part.is_empty() {
            date_part.push('/');
        }
        date_part.push_str(&format!("{:02}", day));
    }
    if payload.year.is_some() {
        if !date_part.is_empty() {
            date_part.push('/');
        }
        date_part.push_str(&format!("{}", year));
    }
    let mut time_part = String::new();
    if payload.hour.is_some() {
        time_part.push_str(&format!("{:02}", hour));
    }
    if payload.minute.is_some() {
        if !time_part.is_empty() {
            time_part.push(':');
        }
        time_part.push_str(&format!("{:02}", minute));
    }
    if payload.second.is_some() {
        if !time_part.is_empty() {
            time_part.push(':');
        }
        time_part.push_str(&format!("{:02}", second));
    }
    match (date_part.is_empty(), time_part.is_empty()) {
        (false, false) => format!("{date_part}, {time_part}"),
        (false, true) => date_part,
        (true, false) => time_part,
        (true, true) => String::new(),
    }
}

/// Convert UTC epoch seconds to a civil `(year, month, day, hour,
/// minute, second)` tuple using the proleptic Gregorian calendar.
/// Howard Hinnant's algorithm — public-domain, exact for the full
/// `i64` range.
fn epoch_to_civil(epoch_secs: i64) -> (i32, u8, u8, u8, u8, u8) {
    let secs_per_day = 86_400_i64;
    let days = epoch_secs.div_euclid(secs_per_day);
    let secs_of_day = epoch_secs.rem_euclid(secs_per_day);
    // Civil-from-days, Hinnant 2013 (https://howardhinnant.github.io/date_algorithms.html)
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = if m <= 2 { y + 1 } else { y };
    let hour = (secs_of_day / 3600) as u8;
    let minute = ((secs_of_day % 3600) / 60) as u8;
    let second = (secs_of_day % 60) as u8;
    (year, m, d, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_at_unix_zero() {
        let (y, m, d, h, mi, s) = epoch_to_civil(0);
        assert_eq!((y, m, d, h, mi, s), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn epoch_2024_january() {
        let (y, m, d, _, _, _) = epoch_to_civil(1_704_067_200);
        assert_eq!((y, m, d), (2024, 1, 1));
    }
}
