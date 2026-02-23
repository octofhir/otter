use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use std::sync::Arc;
use temporal_rs::options::Overflow;

use super::common::*;

// ============================================================================
// Helper: resolve a value to (month, day, refYear) for PlainMonthDay comparison
// ============================================================================

/// Given a value (PlainMonthDay object, string, or property bag), extract (month, day, refYear).
fn resolve_plain_month_day_fields(
    ncx: &mut NativeContext<'_>,
    val: &Value,
) -> Result<(i32, i32, i32), VmError> {
    // Case 1: PlainMonthDay object (has temporal type)
    if let Some(obj) = val.as_object() {
        let temporal_type = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
        if temporal_type.as_deref() == Some("PlainMonthDay") {
            let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(1972);
            return Ok((m, d, y));
        }

        // Property bag: validate calendar first
        let calendar_val = ncx.get_property(&obj, &PropertyKey::string("calendar"))?;
        if !calendar_val.is_undefined() {
            resolve_calendar_from_property(ncx, &calendar_val)?;
        }

        // Read fields via observable get (alphabetical order)
        let day_val = ncx.get_property(&obj, &PropertyKey::string("day"))?;
        let month_val = ncx.get_property(&obj, &PropertyKey::string("month"))?;
        let month_code_val = ncx.get_property(&obj, &PropertyKey::string("monthCode"))?;
        let year_val = ncx.get_property(&obj, &PropertyKey::string("year"))?;

        let has_day = !day_val.is_undefined();
        let has_month = !month_val.is_undefined();
        let has_month_code = !month_code_val.is_undefined();

        if !has_day {
            return Err(VmError::type_error("day is required"));
        }
        if !has_month && !has_month_code {
            return Err(VmError::type_error("either month or monthCode is required"));
        }

        let day_num = ncx.to_number_value(&day_val)?;
        if day_num.is_infinite() {
            return Err(VmError::range_error("day property cannot be Infinity"));
        }
        let day = day_num as i32;

        let month = if has_month_code {
            let mc = ncx.to_string_value(&month_code_val)?;
            validate_month_code_syntax(mc.as_str())?;
            validate_month_code_iso_suitability(mc.as_str())? as i32
        } else {
            let m_num = ncx.to_number_value(&month_val)?;
            if m_num.is_infinite() {
                return Err(VmError::range_error("month property cannot be Infinity"));
            }
            m_num as i32
        };

        // Year from property bag is NOT used as reference year for PlainMonthDay.
        // Per spec, ISO calendar always uses 1972 as the reference year.
        // We still read the year for observable side effects.
        if !year_val.is_undefined() {
            let y_num = ncx.to_number_value(&year_val)?;
            if y_num.is_infinite() {
                return Err(VmError::range_error("year property cannot be Infinity"));
            }
        }
        let year = 1972; // ISO reference year

        return Ok((month, day, year));
    }

    // Case 2: String — try month-day, then date, then datetime
    if val.is_string() {
        let s = ncx.to_string_value(val)?;
        // Try PlainMonthDay first
        if let Ok((ref_year, month, day)) = parse_temporal_month_day_string(s.as_str()) {
            return Ok((month as i32, day as i32, ref_year));
        }
        // Try PlainDate
        if let Ok(pd) = temporal_rs::PlainDate::from_utf8(s.as_str().as_bytes()) {
            return Ok((pd.month() as i32, pd.day() as i32, pd.year()));
        }
        // Try PlainDateTime (handles leap seconds — second:60 is clamped)
        if let Ok(pdt) = temporal_rs::PlainDateTime::from_utf8(s.as_str().as_bytes()) {
            return Ok((pdt.month() as i32, pdt.day() as i32, pdt.year()));
        }
        // If none parse, fall through to generic error
        let _ = parse_temporal_month_day_string(s.as_str())?;
    }

    Err(VmError::type_error("invalid argument for PlainMonthDay comparison"))
}

// ============================================================================
// PlainMonthDay constructor
// ============================================================================

pub(super) fn create_plain_month_day_constructor(
    prototype: GcRef<JsObject>,
) -> Box<dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync>
{
    Box::new(move |this, args, ncx| {
        // Step 1: If NewTarget is undefined, throw TypeError
        // When called with `new`, `this` is a new object with prototype === PlainMonthDay.prototype
        // When called without `new`, `this` is the receiver (Temporal namespace or undefined)
        let is_new_target = if let Some(obj) = this.as_object() {
            // Check if this was created by `new` by verifying prototype chain
            obj.prototype().as_object().map_or(false, |p| p.as_ptr() == prototype.as_ptr())
        } else {
            false
        };
        if !is_new_target {
            return Err(VmError::type_error("Temporal.PlainMonthDay constructor requires 'new'"));
        }

        // new Temporal.PlainMonthDay(isoMonth, isoDay [, calendar [, referenceISOYear]])
        let iso_month_val = args.first().cloned().unwrap_or(Value::undefined());
        let iso_day_val = args.get(1).cloned().unwrap_or(Value::undefined());
        let calendar_val = args.get(2).cloned().unwrap_or(Value::undefined());
        let ref_year_val = args.get(3).cloned().unwrap_or(Value::undefined());

        // ToIntegerWithTruncation for month
        let iso_month = to_integer_with_truncation(ncx, &iso_month_val)? as i32;

        // ToIntegerWithTruncation for day
        let iso_day = to_integer_with_truncation(ncx, &iso_day_val)? as i32;

        // Calendar validation: ToTemporalCalendarIdentifier requires a String type
        if !calendar_val.is_undefined() {
            if !calendar_val.is_string() {
                return Err(VmError::type_error(format!(
                    "{} is not a valid calendar",
                    if calendar_val.is_null() { "null".to_string() } else { calendar_val.type_of().to_string() }
                )));
            }
            let cal_str = calendar_val.as_string().unwrap().as_str().to_ascii_lowercase();
            if cal_str != "iso8601" {
                return Err(VmError::range_error(format!("Unknown calendar: {}", cal_str)));
            }
        }

        // Reference year (default 1972)
        let reference_year = if ref_year_val.is_undefined() {
            1972
        } else {
            to_integer_with_truncation(ncx, &ref_year_val)? as i32
        };

        // Validate
        let (month, day, year) = validate_iso_month_day(iso_month, iso_day, reference_year)?;

        // Validate reference year is within ISO date range
        temporal_rs::PlainDate::try_new_iso(year, month as u8, day as u8).map_err(temporal_err)?;

        // Store internal slots on `this`
        if let Some(obj) = this.as_object() {
            obj.define_property(
                PropertyKey::string(SLOT_ISO_MONTH),
                PropertyDescriptor::builtin_data(Value::int32(month as i32)),
            );
            obj.define_property(
                PropertyKey::string(SLOT_ISO_DAY),
                PropertyDescriptor::builtin_data(Value::int32(day as i32)),
            );
            obj.define_property(
                PropertyKey::string(SLOT_ISO_YEAR),
                PropertyDescriptor::builtin_data(Value::int32(year)),
            );
            obj.define_property(
                PropertyKey::string(SLOT_TEMPORAL_TYPE),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern("PlainMonthDay"))),
            );
        }

        Ok(Value::undefined())
    })
}

// ============================================================================
// PlainMonthDay.from()
// ============================================================================

pub(super) fn plain_month_day_from(
    pmd_ctor_value: Value,
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let item = args.first().cloned().unwrap_or(Value::undefined());
    let options_val = args.get(1).cloned().unwrap_or(Value::undefined());

    // For string arguments: parse string first, then validate options (per spec order)
    if item.is_string() {
        let result = plain_month_day_from_string(ncx, &pmd_ctor_value, &item)?;
        // Read overflow option (for observable side effects) but string parsing ignores it
        let _overflow = parse_overflow_option(ncx, &options_val)?;
        return Ok(result);
    }

    if item.is_undefined() || item.is_null() {
        return Err(VmError::type_error(
            "Cannot convert undefined or null to a Temporal.PlainMonthDay",
        ));
    }

    if item.is_number() || item.is_boolean() {
        return Err(VmError::type_error(format!(
            "invalid type for Temporal.PlainMonthDay.from: {}",
            if item.is_number() { "a number" } else { "a boolean" }
        )));
    }

    if item.as_symbol().is_some() {
        return Err(VmError::type_error(
            "invalid type for Temporal.PlainMonthDay.from: a Symbol",
        ));
    }

    // Check if it's already a PlainMonthDay or another Temporal type
    if let Some(obj) = item.as_object() {
        if let Some(type_val) = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE)) {
            let type_str = type_val.as_string().map(|s| s.as_str().to_string());
            if type_str.as_deref() == Some("PlainMonthDay") {
                // Read overflow option (for observable side effects)
                let _overflow = parse_overflow_option(ncx, &options_val)?;
                // Return a copy preserving the reference year
                let month = obj
                    .get(&PropertyKey::string(SLOT_ISO_MONTH))
                    .and_then(|v| v.as_int32())
                    .unwrap_or(1);
                let day = obj
                    .get(&PropertyKey::string(SLOT_ISO_DAY))
                    .and_then(|v| v.as_int32())
                    .unwrap_or(1);
                let year = obj
                    .get(&PropertyKey::string(SLOT_ISO_YEAR))
                    .and_then(|v| v.as_int32())
                    .unwrap_or(1972);
                return create_plain_month_day_value(ncx, &pmd_ctor_value, month, day, year);
            }
            if type_str.as_deref() == Some("PlainDate") {
                // Extract month and day from PlainDate, use 1972 as reference year
                let _overflow = parse_overflow_option(ncx, &options_val)?;
                let month = obj
                    .get(&PropertyKey::string(SLOT_ISO_MONTH))
                    .and_then(|v| v.as_int32())
                    .unwrap_or(1);
                let day = obj
                    .get(&PropertyKey::string(SLOT_ISO_DAY))
                    .and_then(|v| v.as_int32())
                    .unwrap_or(1);
                return create_plain_month_day_value(ncx, &pmd_ctor_value, month, day, 1972);
            }
        }

        // It's a property bag — per spec, fields are read first, then options
        return plain_month_day_from_fields(ncx, &pmd_ctor_value, &obj, &options_val);
    }

    // Handle Proxy as a property bag
    if let Some(proxy) = item.as_proxy() {
        return plain_month_day_from_proxy(ncx, &pmd_ctor_value, proxy, &item, &options_val);
    }

    Err(VmError::type_error(
        "invalid type for Temporal.PlainMonthDay.from",
    ))
}

fn plain_month_day_from_proxy(
    ncx: &mut NativeContext<'_>,
    pmd_ctor_value: &Value,
    proxy: GcRef<crate::proxy::JsProxy>,
    receiver: &Value,
    options_val: &Value,
) -> Result<Value, VmError> {
    // Read fields through proxy traps with interleaved conversion
    // (per spec PrepareTemporalFields: get + convert each field in order)

    // 1. get calendar (string, no valueOf)
    let calendar_val = proxy_get_property(ncx, proxy.clone(), receiver, "calendar")?;
    if !calendar_val.is_undefined() {
        resolve_calendar_from_property(ncx, &calendar_val)?;
    }

    // 2. get day + convert to number
    let day_val = proxy_get_property(ncx, proxy.clone(), receiver, "day")?;
    let day_raw = if !day_val.is_undefined() {
        let n = ncx.to_number_value(&day_val)?;
        if n.is_infinite() {
            return Err(VmError::range_error("day property cannot be Infinity"));
        }
        n as i32
    } else {
        return Err(VmError::type_error("day is required"));
    };

    // 3. get month + convert to number
    let month_val = proxy_get_property(ncx, proxy.clone(), receiver, "month")?;
    let month_num = if !month_val.is_undefined() {
        let n = ncx.to_number_value(&month_val)?;
        if n.is_infinite() {
            return Err(VmError::range_error("month property cannot be Infinity"));
        }
        Some(n as i32)
    } else {
        None
    };

    // 4. get monthCode + convert to string
    let month_code_val = proxy_get_property(ncx, proxy.clone(), receiver, "monthCode")?;
    let mc_str = if !month_code_val.is_undefined() {
        let mc = ncx.to_string_value(&month_code_val)?;
        validate_month_code_syntax(mc.as_str())?;
        Some(mc)
    } else {
        None
    };

    // 5. get year + convert to number
    let year_val = proxy_get_property(ncx, proxy.clone(), receiver, "year")?;
    let validation_year = if !year_val.is_undefined() {
        let n = ncx.to_number_value(&year_val)?;
        if n.is_infinite() {
            return Err(VmError::range_error("year property cannot be Infinity"));
        }
        Some(n as i32)
    } else {
        None
    };

    // Require either month or monthCode
    if month_num.is_none() && mc_str.is_none() {
        return Err(VmError::type_error("either month or monthCode is required"));
    }

    // Parse overflow option AFTER reading fields but BEFORE algorithmic validation
    let overflow = parse_overflow_option(ncx, options_val)?;

    let month = if let Some(ref mc) = mc_str {
        let mc_month = validate_month_code_iso_suitability(mc.as_str())?;
        if let Some(m_int) = month_num {
            if m_int != mc_month as i32 {
                return Err(VmError::range_error(format!(
                    "monthCode {} and month {} conflict",
                    mc, m_int
                )));
            }
        }
        mc_month as i32
    } else {
        month_num.unwrap()
    };

    let reference_year = 1972;
    let year_for_validation = validation_year.unwrap_or(reference_year);

    match overflow {
        Overflow::Reject => {
            if month < 1 || month > 12 {
                return Err(VmError::range_error(format!(
                    "month must be between 1 and 12, got {}",
                    month
                )));
            }
            let max_day = days_in_month(month as u32, year_for_validation);
            if day_raw < 1 || day_raw as u32 > max_day {
                return Err(VmError::range_error(format!(
                    "day must be between 1 and {}, got {}",
                    max_day, day_raw
                )));
            }
            create_plain_month_day_value(ncx, pmd_ctor_value, month, day_raw, reference_year)
        }
        Overflow::Constrain => {
            if month < 1 {
                return Err(VmError::range_error(format!(
                    "month must be >= 1, got {}",
                    month
                )));
            }
            let clamped_month = month.min(12);
            let max_day = days_in_month(clamped_month as u32, year_for_validation);
            if day_raw < 1 {
                return Err(VmError::range_error(format!(
                    "day must be >= 1, got {}",
                    day_raw
                )));
            }
            let clamped_day = (day_raw as u32).min(max_day) as i32;
            create_plain_month_day_value(
                ncx,
                pmd_ctor_value,
                clamped_month,
                clamped_day,
                reference_year,
            )
        }
    }
}

fn plain_month_day_from_string(
    ncx: &mut NativeContext<'_>,
    pmd_ctor_value: &Value,
    item: &Value,
) -> Result<Value, VmError> {
    let s = ncx.to_string_value(item)?;
    // temporal_rs handles all validation: UTC designator rejection,
    // non-ASCII minus, fractional hours/minutes, annotations, etc.
    let (ref_year, month, day) = parse_temporal_month_day_string(s.as_str())?;
    create_plain_month_day_value(ncx, pmd_ctor_value, month as i32, day as i32, ref_year)
}

fn plain_month_day_from_fields(
    ncx: &mut NativeContext<'_>,
    pmd_ctor_value: &Value,
    fields: &GcRef<JsObject>,
    options_val: &Value,
) -> Result<Value, VmError> {
    // Get calendar first (per spec order)
    let calendar_val = fields.get(&PropertyKey::string("calendar"));
    if let Some(ref cv) = calendar_val {
        if !cv.is_undefined() {
            resolve_calendar_from_property(ncx, cv)?;
        }
    }

    // Get month/monthCode, day, year
    let day_val = fields.get(&PropertyKey::string("day"));
    let month_val = fields.get(&PropertyKey::string("month"));
    let month_code_val = fields.get(&PropertyKey::string("monthCode"));
    let year_val = fields.get(&PropertyKey::string("year"));

    // day is always required
    let day_raw = match day_val {
        Some(ref dv) if !dv.is_undefined() => {
            let n = ncx.to_number_value(dv)?;
            if n.is_infinite() {
                return Err(VmError::range_error("day property cannot be Infinity"));
            }
            if n.is_nan() {
                return Err(VmError::range_error("day property cannot be NaN"));
            }
            n as i32
        }
        _ => {
            return Err(VmError::type_error("day is required"));
        }
    };

    // Need either month or monthCode
    let has_month = month_val.as_ref().map_or(false, |v| !v.is_undefined());
    let has_month_code = month_code_val.as_ref().map_or(false, |v| !v.is_undefined());

    if !has_month && !has_month_code {
        return Err(VmError::type_error("either month or monthCode is required"));
    }

    // Step 1: Validate monthCode SYNTAX (before year type validation)
    let mc_str = if has_month_code {
        let mc = ncx.to_string_value(&month_code_val.clone().unwrap())?;
        validate_month_code_syntax(mc.as_str())?;
        Some(mc)
    } else {
        None
    };

    // Step 2: Convert year to number (TypeError for Symbol, etc.)
    let validation_year = if let Some(ref yv) = year_val {
        if !yv.is_undefined() {
            let n = ncx.to_number_value(yv)?;
            if n.is_infinite() {
                return Err(VmError::range_error("year property cannot be Infinity"));
            }
            if n.is_nan() {
                return Err(VmError::range_error("year property cannot be NaN"));
            }
            Some(n as i32)
        } else {
            None
        }
    } else {
        None
    };

    // Parse overflow option AFTER reading fields but BEFORE algorithmic validation
    // (per spec order of operations: read fields → read options → validate)
    let overflow = parse_overflow_option(ncx, options_val)?;

    // Step 3: Validate monthCode SUITABILITY for ISO calendar (after options read)
    let month = if let Some(ref mc) = mc_str {
        let mc_month = validate_month_code_iso_suitability(mc.as_str())?;

        // Check for month/monthCode conflict
        if has_month {
            let m_num = ncx.to_number_value(&month_val.clone().unwrap())?;
            if m_num.is_infinite() {
                return Err(VmError::range_error("month property cannot be Infinity"));
            }
            let m_int = m_num as i32;
            if m_int != mc_month as i32 {
                return Err(VmError::range_error(format!(
                    "monthCode {} and month {} conflict",
                    mc, m_int
                )));
            }
        }
        mc_month as i32
    } else {
        // has_month only
        let n = ncx.to_number_value(&month_val.clone().unwrap())?;
        if n.is_infinite() {
            return Err(VmError::range_error("month property cannot be Infinity"));
        }
        if n.is_nan() {
            return Err(VmError::range_error("month property cannot be NaN"));
        }
        n as i32
    };

    // Per spec: PlainMonthDay.from({...}) ALWAYS uses 1972 as reference ISO year,
    // regardless of whether a year was provided. The year field is only used for
    // day-of-month validation (e.g., Feb 29 in a leap year).
    let reference_year = 1972;
    // Use validation year if provided, for day-of-month bounds checking
    let year_for_validation = validation_year.unwrap_or(reference_year);

    // Validate/constrain
    match overflow {
        Overflow::Reject => {
            if month < 1 || month > 12 {
                return Err(VmError::range_error(format!(
                    "month must be between 1 and 12, got {}",
                    month
                )));
            }
            let max_day = days_in_month(month as u32, year_for_validation);
            if day_raw < 1 || day_raw as u32 > max_day {
                return Err(VmError::range_error(format!(
                    "day must be between 1 and {}, got {}",
                    max_day, day_raw
                )));
            }
            create_plain_month_day_value(ncx, pmd_ctor_value, month, day_raw, reference_year)
        }
        Overflow::Constrain => {
            // Negative month or zero month always error even in constrain mode
            if month < 1 {
                return Err(VmError::range_error(format!(
                    "month must be >= 1, got {}",
                    month
                )));
            }
            let clamped_month = month.min(12);
            let max_day = days_in_month(clamped_month as u32, year_for_validation);
            // Negative day or zero day always error even in constrain mode
            if day_raw < 1 {
                return Err(VmError::range_error(format!(
                    "day must be >= 1, got {}",
                    day_raw
                )));
            }
            let clamped_day = (day_raw as u32).min(max_day) as i32;
            create_plain_month_day_value(
                ncx,
                pmd_ctor_value,
                clamped_month,
                clamped_day,
                reference_year,
            )
        }
    }
}

/// Create a new PlainMonthDay value by calling the constructor
fn create_plain_month_day_value(
    ncx: &mut NativeContext<'_>,
    ctor: &Value,
    month: i32,
    day: i32,
    reference_year: i32,
) -> Result<Value, VmError> {
    ncx.call_function_construct(
        ctor,
        Value::undefined(),
        &[
            Value::int32(month),
            Value::int32(day),
            Value::string(JsString::intern("iso8601")),
            Value::int32(reference_year),
        ],
    )
}

// ============================================================================
// PlainMonthDay prototype methods
// ============================================================================

pub(super) fn install_plain_month_day_prototype(
    proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // .monthCode getter
    proto.define_property(
        PropertyKey::string("monthCode"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| {
                        VmError::type_error("monthCode called on non-object")
                    })?;
                    let month = obj
                        .get(&PropertyKey::string(SLOT_ISO_MONTH))
                        .and_then(|v| v.as_int32())
                        .ok_or_else(|| {
                            VmError::type_error(
                                "monthCode called on non-PlainMonthDay",
                            )
                        })?;
                    Ok(Value::string(JsString::intern(&format_month_code(
                        month as u32,
                    ))))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        },
    );

    // .month getter — per spec, PlainMonthDay does NOT have a month property
    // Only monthCode is available. month should return undefined (no getter installed).

    // .day getter
    proto.define_property(
        PropertyKey::string("day"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| {
                        VmError::type_error("day called on non-object")
                    })?;
                    let day = obj
                        .get(&PropertyKey::string(SLOT_ISO_DAY))
                        .and_then(|v| v.as_int32())
                        .ok_or_else(|| {
                            VmError::type_error("day called on non-PlainMonthDay")
                        })?;
                    Ok(Value::int32(day))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        },
    );

    // .calendarId getter — must check branding
    proto.define_property(
        PropertyKey::string("calendarId"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| {
                        VmError::type_error("calendarId called on non-PlainMonthDay")
                    })?;
                    let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                    if ty.as_deref() != Some("PlainMonthDay") {
                        return Err(VmError::type_error("calendarId called on non-PlainMonthDay"));
                    }
                    Ok(Value::string(JsString::intern("iso8601")))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        },
    );

    // .toString(options) method
    let to_string_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toString called on non-object"))?;

            // Branding check
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainMonthDay") {
                return Err(VmError::type_error("toString called on non-PlainMonthDay"));
            }

            let month = obj
                .get(&PropertyKey::string(SLOT_ISO_MONTH))
                .and_then(|v| v.as_int32())
                .unwrap_or(1);
            let day = obj
                .get(&PropertyKey::string(SLOT_ISO_DAY))
                .and_then(|v| v.as_int32())
                .unwrap_or(1);
            let year = obj
                .get(&PropertyKey::string(SLOT_ISO_YEAR))
                .and_then(|v| v.as_int32())
                .unwrap_or(1972);

            // Parse calendarName option from options argument
            let options_val = args.first().cloned().unwrap_or(Value::undefined());
            let calendar_name = if options_val.is_undefined() {
                "auto".to_string()
            } else {
                // GetOptionsObject: must be an object
                if options_val.is_null() || options_val.is_boolean() || options_val.is_number()
                    || options_val.is_string() || options_val.is_bigint() || options_val.as_symbol().is_some() {
                    return Err(VmError::type_error("options must be an object or undefined"));
                }
                // Handle Proxy first (as_object() returns None for proxies)
                if let Some(proxy) = options_val.as_proxy() {
                    let key = PropertyKey::string("calendarName");
                    let key_value = crate::proxy_operations::property_key_to_value_pub(&key);
                    let cn_val = crate::proxy_operations::proxy_get(ncx, proxy, &key, key_value, options_val.clone())?;
                    if cn_val.is_undefined() {
                        "auto".to_string()
                    } else {
                        let cn_str = ncx.to_string_value(&cn_val)?;
                        cn_str.as_str().to_string()
                    }
                } else if let Some(oo) = options_val.as_object() {
                    let cn_val = ncx.get_property(&oo, &PropertyKey::string("calendarName"))?;
                    if cn_val.is_undefined() {
                        "auto".to_string()
                    } else {
                        let cn_str = ncx.to_string_value(&cn_val)?;
                        cn_str.as_str().to_string()
                    }
                } else {
                    // Function or other callable — no calendarName property
                    "auto".to_string()
                }
            };

            // Validate calendarName option
            match calendar_name.as_str() {
                "auto" | "always" | "never" | "critical" => {}
                _ => return Err(VmError::range_error(format!("{} is not a valid value for calendarName", calendar_name))),
            }

            let result = match calendar_name.as_str() {
                "always" => format!("{:04}-{:02}-{:02}[u-ca=iso8601]", year, month, day),
                "critical" => format!("{:04}-{:02}-{:02}[!u-ca=iso8601]", year, month, day),
                "never" => format!("{:02}-{:02}", month, day),
                _ /* auto */ => format!("{:02}-{:02}", month, day),
            };

            Ok(Value::string(JsString::intern(&result)))
        },
        mm.clone(),
        fn_proto.clone(),
        "toString",
        0,
    );
    proto.define_property(
        PropertyKey::string("toString"),
        PropertyDescriptor::builtin_method(to_string_fn),
    );

    // .toJSON() method
    let to_json_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toJSON called on non-object"))?;

            let month = obj
                .get(&PropertyKey::string(SLOT_ISO_MONTH))
                .and_then(|v| v.as_int32())
                .ok_or_else(|| {
                    VmError::type_error("toJSON called on non-PlainMonthDay")
                })?;
            let day = obj
                .get(&PropertyKey::string(SLOT_ISO_DAY))
                .and_then(|v| v.as_int32())
                .ok_or_else(|| {
                    VmError::type_error("toJSON called on non-PlainMonthDay")
                })?;

            Ok(Value::string(JsString::intern(&format!(
                "{:02}-{:02}",
                month, day
            ))))
        },
        mm.clone(),
        fn_proto.clone(),
        "toJSON",
        0,
    );
    proto.define_property(
        PropertyKey::string("toJSON"),
        PropertyDescriptor::builtin_method(to_json_fn),
    );

    // .valueOf() - always throws TypeError per Temporal spec
    let value_of_fn = Value::native_function_with_proto_named(
        |_this, _args, _ncx| {
            Err(VmError::type_error(
                "use compare() or toString() to compare Temporal.PlainMonthDay",
            ))
        },
        mm.clone(),
        fn_proto.clone(),
        "valueOf",
        0,
    );
    proto.define_property(
        PropertyKey::string("valueOf"),
        PropertyDescriptor::builtin_method(value_of_fn),
    );

    // .equals(other) method — accepts PlainMonthDay, string, or property bag
    let equals_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("equals called on non-object"))?;
            // Verify receiver is a PlainMonthDay
            let _ = obj
                .get(&PropertyKey::string(SLOT_ISO_MONTH))
                .and_then(|v| v.as_int32())
                .ok_or_else(|| {
                    VmError::type_error("equals called on non-PlainMonthDay")
                })?;

            let other_arg = args.first().cloned().unwrap_or(Value::undefined());

            // Resolve the other argument to a PlainMonthDay-like object
            let (m2, d2, y2) = resolve_plain_month_day_fields(ncx, &other_arg)?;

            let m1 = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap();
            let d1 = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap();
            let y1 = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(1972);

            Ok(Value::boolean(m1 == m2 && d1 == d2 && y1 == y2))
        },
        mm.clone(),
        fn_proto.clone(),
        "equals",
        1,
    );
    proto.define_property(
        PropertyKey::string("equals"),
        PropertyDescriptor::builtin_method(equals_fn),
    );

    // .with(temporalMonthDayLike, options) method
    let with_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("with called on non-object"))?;
            // Branding: verify it's a PlainMonthDay
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainMonthDay") {
                return Err(VmError::type_error("with called on non-PlainMonthDay"));
            }
            let cur_month = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let cur_day = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            let cur_year = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(1972);

            let item = args.first().cloned().unwrap_or(Value::undefined());
            // Argument must be an object (including Proxy)
            if item.is_undefined() || item.is_null() || item.is_boolean() || item.is_number()
                || item.is_string() || item.is_bigint() || item.as_symbol().is_some() {
                return Err(VmError::type_error("with argument must be an object"));
            }

            // Helper to get property from item (supports both Object and Proxy)
            let get_prop = |ncx: &mut NativeContext<'_>, item: &Value, key: &str| -> Result<Value, VmError> {
                if let Some(proxy) = item.as_proxy() {
                    proxy_get_property(ncx, proxy, item, key)
                } else if let Some(item_obj) = item.as_object() {
                    ncx.get_property(&item_obj, &PropertyKey::string(key))
                } else {
                    Ok(Value::undefined())
                }
            };

            // Reject if item is a Temporal type (PlainDate, PlainMonthDay, etc.)
            if let Some(item_obj) = item.as_object() {
                if let Some(item_ty) = item_obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string())) {
                    if !item_ty.is_empty() {
                        return Err(VmError::type_error("with argument must be a partial object, not a Temporal type"));
                    }
                }
            }

            // Step 1: RejectObjectWithCalendarOrTimeZone — BEFORE field reads
            let cal_v = get_prop(ncx, &item, "calendar")?;
            if !cal_v.is_undefined() {
                return Err(VmError::type_error("calendar not allowed in PlainMonthDay.prototype.with"));
            }
            let tz_v = get_prop(ncx, &item, "timeZone")?;
            if !tz_v.is_undefined() {
                return Err(VmError::type_error("timeZone not allowed in PlainMonthDay.prototype.with"));
            }

            // Step 2: PrepareTemporalFields — get + IMMEDIATELY convert each field (alphabetical order)
            let day_v = get_prop(ncx, &item, "day")?;
            let has_day = !day_v.is_undefined();
            let day_num = if has_day {
                let n = ncx.to_number_value(&day_v)?;
                if n.is_infinite() { return Err(VmError::range_error("day property cannot be Infinity")); }
                Some(n as i32)
            } else { None };

            let month_v = get_prop(ncx, &item, "month")?;
            let has_month = !month_v.is_undefined();
            let month_num = if has_month {
                let n = ncx.to_number_value(&month_v)?;
                if n.is_infinite() { return Err(VmError::range_error("month property cannot be Infinity")); }
                Some(n as i32)
            } else { None };

            let month_code_v = get_prop(ncx, &item, "monthCode")?;
            let has_month_code = !month_code_v.is_undefined();
            let mc_str = if has_month_code {
                let mc = ncx.to_string_value(&month_code_v)?;
                Some(mc)
            } else { None };

            let year_v = get_prop(ncx, &item, "year")?;
            let has_year = !year_v.is_undefined();
            let year_num = if has_year {
                let n = ncx.to_number_value(&year_v)?;
                if n.is_infinite() { return Err(VmError::range_error("year property cannot be Infinity")); }
                Some(n as i32)
            } else { None };

            // Must have at least one known temporal field
            if !has_day && !has_month && !has_month_code && !has_year {
                return Err(VmError::type_error(
                    "with argument must have at least one recognized temporal property",
                ));
            }

            // Merge with current values
            let day = day_num.unwrap_or(cur_day);

            // CalendarResolveFields: reject below-minimum values BEFORE options
            if day < 1 { return Err(VmError::range_error(format!("day must be >= 1, got {}", day))); }
            if let Some(m) = month_num { if m < 1 { return Err(VmError::range_error(format!("month must be >= 1, got {}", m))); } }

            // Step 3: Read overflow from options — AFTER fields and basic below-min validation
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = parse_overflow_option(ncx, &options_val)?;

            // monthCode validation AFTER options (per spec: options read before algorithmic validation)
            let month = if let Some(ref mc) = mc_str {
                validate_month_code_syntax(mc.as_str())?;
                let mc_month = validate_month_code_iso_suitability(mc.as_str())? as i32;
                if let Some(m) = month_num {
                    if m != mc_month {
                        return Err(VmError::range_error(format!("monthCode {} and month {} conflict", mc, m)));
                    }
                }
                mc_month
            } else if let Some(m) = month_num {
                m
            } else { cur_month };

            let year = year_num.unwrap_or(cur_year);

            // Build result using temporal_rs for validation with the user's year
            let ov = overflow;
            if month < 0 || month > 255 { return Err(VmError::range_error(format!("month out of range: {}", month))); }
            if day < 0 || day > 255 { return Err(VmError::range_error(format!("day out of range: {}", day))); }
            // Validate with user's year to check day validity
            let pmd = temporal_rs::PlainMonthDay::new_with_overflow(
                month as u8, day as u8, temporal_rs::Calendar::default(), ov, Some(year),
            ).map_err(temporal_err)?;

            // Per spec, the result's reference year is always 1972 (ISO reference year)
            let ref_year = 1972;

            // Subclassing ignored — always use Temporal.PlainMonthDay constructor
            let temporal_ns = ncx.ctx.get_global("Temporal")
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let temporal_obj = temporal_ns.as_object()
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let pmd_ctor = temporal_obj.get(&PropertyKey::string("PlainMonthDay"))
                .ok_or_else(|| VmError::type_error("PlainMonthDay constructor not found"))?;
            create_plain_month_day_value(ncx, &pmd_ctor,
                pmd.month_code().to_month_integer() as i32,
                pmd.day() as i32,
                ref_year)
        },
        mm.clone(),
        fn_proto.clone(),
        "with",
        1,
    );
    proto.define_property(
        PropertyKey::string("with"),
        PropertyDescriptor::builtin_method(with_fn),
    );

    // .toPlainDate(yearLike) method
    let to_plain_date_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toPlainDate called on non-object"))?;
            // Branding check
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainMonthDay") {
                return Err(VmError::type_error("toPlainDate called on non-PlainMonthDay"));
            }
            let month = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let day = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);

            let year_like = args.first().cloned().unwrap_or(Value::undefined());
            if year_like.is_undefined() || year_like.is_null() || year_like.is_boolean()
                || year_like.is_number() || year_like.is_string() || year_like.is_bigint()
                || year_like.as_symbol().is_some() {
                return Err(VmError::type_error("toPlainDate requires an object argument with year"));
            }
            // Use observable get for year (supports both Object and Proxy)
            let year_val = if let Some(proxy) = year_like.as_proxy() {
                proxy_get_property(ncx, proxy, &year_like, "year")?
            } else if let Some(year_obj) = year_like.as_object() {
                ncx.get_property(&year_obj, &PropertyKey::string("year"))?
            } else {
                return Err(VmError::type_error("toPlainDate requires an object argument with year"));
            };
            if year_val.is_undefined() {
                return Err(VmError::type_error("year is required"));
            }
            let year_num = ncx.to_number_value(&year_val)?;
            if year_num.is_infinite() {
                return Err(VmError::range_error("year property cannot be Infinity"));
            }
            let year = year_num as i32;

            // Use temporal_rs with constrain overflow (spec default for toPlainDate)
            let pd = temporal_rs::PlainDate::new_with_overflow(
                year, month as u8, day as u8,
                temporal_rs::Calendar::default(),
                temporal_rs::options::Overflow::Constrain,
            ).map_err(temporal_err)?;

            // Create a PlainDate via Temporal.PlainDate constructor
            let temporal_ns = ncx.ctx.get_global("Temporal")
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let temporal_obj = temporal_ns.as_object()
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let pd_ctor = temporal_obj.get(&PropertyKey::string("PlainDate"))
                .ok_or_else(|| VmError::type_error("PlainDate constructor not found"))?;

            ncx.call_function_construct(
                &pd_ctor,
                Value::undefined(),
                &[Value::int32(pd.year()), Value::int32(pd.month() as i32), Value::int32(pd.day() as i32)],
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "toPlainDate",
        1,
    );
    proto.define_property(
        PropertyKey::string("toPlainDate"),
        PropertyDescriptor::builtin_method(to_plain_date_fn),
    );

    // .toLocaleString() method
    let to_locale_string_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toLocaleString called on non-object"))?;
            let month = obj
                .get(&PropertyKey::string(SLOT_ISO_MONTH))
                .and_then(|v| v.as_int32())
                .ok_or_else(|| VmError::type_error("toLocaleString called on non-PlainMonthDay"))?;
            let day = obj
                .get(&PropertyKey::string(SLOT_ISO_DAY))
                .and_then(|v| v.as_int32())
                .ok_or_else(|| VmError::type_error("toLocaleString called on non-PlainMonthDay"))?;
            Ok(Value::string(JsString::intern(&format!("{:02}-{:02}", month, day))))
        },
        mm.clone(),
        fn_proto.clone(),
        "toLocaleString",
        0,
    );
    proto.define_property(
        PropertyKey::string("toLocaleString"),
        PropertyDescriptor::builtin_method(to_locale_string_fn),
    );

    // @@toStringTag
    proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Temporal.PlainMonthDay")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
    );
}
