use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use std::sync::Arc;

use super::common::*;

// ============================================================================
// PlainDate prototype methods
// ============================================================================

pub(super) fn install_plain_date_prototype(
    proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // Helper macro-like for creating slot accessor getters
    let make_slot_getter = |slot: &'static str, name: &'static str, mm: &Arc<MemoryManager>, fn_proto: &GcRef<JsObject>| -> Value {
        Value::native_function_with_proto(
            move |this, _args, _ncx| {
                let obj = this.as_object().ok_or_else(|| {
                    VmError::type_error(&format!("{} called on non-object", name))
                })?;
                obj.get(&PropertyKey::string(slot))
                    .filter(|v| !v.is_undefined())
                    .ok_or_else(|| VmError::type_error(&format!("{} called on non-PlainDate", name)))
            },
            mm.clone(),
            fn_proto.clone(),
        )
    };

    // year, month, day getters
    for (slot, name) in &[
        (SLOT_ISO_YEAR, "year"),
        (SLOT_ISO_MONTH, "month"),
        (SLOT_ISO_DAY, "day"),
    ] {
        proto.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::Accessor {
                get: Some(make_slot_getter(slot, name, mm, &fn_proto)),
                set: None,
                attributes: PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: true,
                },
            },
        );
    }

    // monthCode getter
    proto.define_property(
        PropertyKey::string("monthCode"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| {
                        VmError::type_error("monthCode called on non-object")
                    })?;
                    let month = obj.get(&PropertyKey::string(SLOT_ISO_MONTH))
                        .and_then(|v| v.as_int32())
                        .ok_or_else(|| VmError::type_error("monthCode called on non-PlainDate"))?;
                    Ok(Value::string(JsString::intern(&format_month_code(month as u32))))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    // calendarId getter
    proto.define_property(
        PropertyKey::string("calendarId"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    // Branding: check it's a PlainDate
                    let obj = this.as_object().ok_or_else(|| {
                        VmError::type_error("calendarId called on non-object")
                    })?;
                    let _ = obj.get(&PropertyKey::string(SLOT_ISO_YEAR))
                        .and_then(|v| v.as_int32())
                        .ok_or_else(|| VmError::type_error("calendarId called on non-PlainDate"))?;
                    Ok(Value::string(JsString::intern("iso8601")))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    // era getter — always undefined for ISO calendar
    proto.define_property(
        PropertyKey::string("era"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| {
                        VmError::type_error("era called on non-object")
                    })?;
                    let _ = obj.get(&PropertyKey::string(SLOT_ISO_YEAR))
                        .and_then(|v| v.as_int32())
                        .ok_or_else(|| VmError::type_error("era called on non-PlainDate"))?;
                    Ok(Value::undefined())
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    // eraYear getter — always undefined for ISO calendar
    proto.define_property(
        PropertyKey::string("eraYear"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| {
                        VmError::type_error("eraYear called on non-object")
                    })?;
                    let _ = obj.get(&PropertyKey::string(SLOT_ISO_YEAR))
                        .and_then(|v| v.as_int32())
                        .ok_or_else(|| VmError::type_error("eraYear called on non-PlainDate"))?;
                    Ok(Value::undefined())
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    // dayOfWeek, dayOfYear, daysInMonth, daysInYear, monthsInYear, inLeapYear — via temporal_rs::PlainDate
    for (prop, getter_fn) in &[
        ("dayOfWeek", (|pd: &temporal_rs::PlainDate| pd.day_of_week() as i32) as fn(&temporal_rs::PlainDate) -> i32),
        ("dayOfYear", |pd: &temporal_rs::PlainDate| pd.day_of_year() as i32),
        ("daysInMonth", |pd: &temporal_rs::PlainDate| pd.days_in_month() as i32),
        ("daysInYear", |pd: &temporal_rs::PlainDate| pd.days_in_year() as i32),
        ("monthsInYear", |pd: &temporal_rs::PlainDate| pd.months_in_year() as i32),
    ] {
        let f = *getter_fn;
        proto.define_property(
            PropertyKey::string(prop),
            PropertyDescriptor::Accessor {
                get: Some(Value::native_function_with_proto(
                    move |this, _args, _ncx| {
                        let obj = this.as_object().ok_or_else(|| VmError::type_error("getter called on non-object"))?;
                        let pd = extract_iso_date_from_slots(&obj)?;
                        Ok(Value::int32(f(&pd)))
                    },
                    mm.clone(),
                    fn_proto.clone(),
                )),
                set: None,
                attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
            },
        );
    }

    proto.define_property(
        PropertyKey::string("inLeapYear"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("inLeapYear"))?;
                    let pd = extract_iso_date_from_slots(&obj)?;
                    Ok(Value::boolean(pd.in_leap_year()))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    proto.define_property(
        PropertyKey::string("daysInWeek"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("daysInWeek"))?;
                    let _ = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("daysInWeek"))?;
                    Ok(Value::int32(7))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    // ========================================================================
    // .equals(other)
    // ========================================================================
    let equals_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("equals called on non-object"))?;
            let this_pd = extract_iso_date_from_slots(&obj)?;
            let other = args.first().cloned().unwrap_or(Value::undefined());
            let other_pd = to_temporal_plain_date(ncx, &other, None)?;
            Ok(Value::boolean(this_pd.compare_iso(&other_pd) == std::cmp::Ordering::Equal))
        },
        mm.clone(), fn_proto.clone(), "equals", 1,
    );
    proto.define_property(PropertyKey::string("equals"), PropertyDescriptor::builtin_method(equals_fn));

    // ========================================================================
    // .add(duration, options)
    // ========================================================================
    let add_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("add called on non-object"))?;
            let this_pd = extract_iso_date_from_slots(&obj)?;
            let dur_arg = args.first().cloned().unwrap_or(Value::undefined());
            let duration = to_temporal_duration(ncx, &dur_arg)?;
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = parse_overflow_option(ncx, &options_val)?;
            let result = this_pd.add(&duration, Some(overflow)).map_err(temporal_err)?;
            construct_plain_date_value(ncx, result.year(), result.month() as i32, result.day() as i32)
        },
        mm.clone(), fn_proto.clone(), "add", 1,
    );
    proto.define_property(PropertyKey::string("add"), PropertyDescriptor::builtin_method(add_fn));

    // ========================================================================
    // .subtract(duration, options)
    // ========================================================================
    let subtract_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("subtract called on non-object"))?;
            let this_pd = extract_iso_date_from_slots(&obj)?;
            let dur_arg = args.first().cloned().unwrap_or(Value::undefined());
            let duration = to_temporal_duration(ncx, &dur_arg)?;
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = parse_overflow_option(ncx, &options_val)?;
            let result = this_pd.subtract(&duration, Some(overflow)).map_err(temporal_err)?;
            construct_plain_date_value(ncx, result.year(), result.month() as i32, result.day() as i32)
        },
        mm.clone(), fn_proto.clone(), "subtract", 1,
    );
    proto.define_property(PropertyKey::string("subtract"), PropertyDescriptor::builtin_method(subtract_fn));

    // ========================================================================
    // .since(other, options)
    // ========================================================================
    let since_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("since called on non-object"))?;
            let this_pd = extract_iso_date_from_slots(&obj)?;
            let other_arg = args.first().cloned().unwrap_or(Value::undefined());
            let other_pd = to_temporal_plain_date(ncx, &other_arg, None)?;
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let settings = parse_difference_settings_for_date(ncx, &options_val)?;
            let duration = this_pd.since(&other_pd, settings).map_err(temporal_err)?;
            construct_duration_value(ncx, &duration)
        },
        mm.clone(), fn_proto.clone(), "since", 1,
    );
    proto.define_property(PropertyKey::string("since"), PropertyDescriptor::builtin_method(since_fn));

    // ========================================================================
    // .until(other, options)
    // ========================================================================
    let until_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("until called on non-object"))?;
            let this_pd = extract_iso_date_from_slots(&obj)?;
            let other_arg = args.first().cloned().unwrap_or(Value::undefined());
            let other_pd = to_temporal_plain_date(ncx, &other_arg, None)?;
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let settings = parse_difference_settings_for_date(ncx, &options_val)?;
            let duration = this_pd.until(&other_pd, settings).map_err(temporal_err)?;
            construct_duration_value(ncx, &duration)
        },
        mm.clone(), fn_proto.clone(), "until", 1,
    );
    proto.define_property(PropertyKey::string("until"), PropertyDescriptor::builtin_method(until_fn));

    // ========================================================================
    // .with(temporalDateLike, options)
    // ========================================================================
    let with_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("with called on non-object"))?;
            let cur_y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("with called on non-PlainDate"))?;
            let cur_m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let cur_d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);

            let item = args.first().cloned().unwrap_or(Value::undefined());
            if !item.as_object().is_some() && item.as_proxy().is_none() {
                return Err(VmError::type_error("with argument must be an object"));
            }

            // Helper: read property from object or proxy
            fn get_prop(ncx: &mut NativeContext<'_>, val: &Value, key: &str) -> Result<Value, VmError> {
                if let Some(obj) = val.as_object() {
                    ncx.get_property(&obj, &PropertyKey::string(key))
                } else if let Some(proxy) = val.as_proxy() {
                    proxy_get_property(ncx, proxy, val, key)
                } else {
                    Ok(Value::undefined())
                }
            }

            // Reject temporal types (PlainDate, PlainMonthDay etc.)
            if let Some(arg_obj) = item.as_object() {
                let tt = arg_obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                if tt.is_some() {
                    return Err(VmError::type_error("with argument must be a plain object, not a Temporal type"));
                }
            }

            // Read fields in alphabetical order (spec: PrepareTemporalFields)
            let calendar_val = get_prop(ncx, &item, "calendar")?;
            if !calendar_val.is_undefined() {
                return Err(VmError::type_error("calendar not allowed in with()"));
            }
            let time_zone_val = get_prop(ncx, &item, "timeZone")?;
            if !time_zone_val.is_undefined() {
                return Err(VmError::type_error("timeZone not allowed in with()"));
            }

            let day_raw = get_prop(ncx, &item, "day")?;
            let day = if !day_raw.is_undefined() {
                let n = ncx.to_number_value(&day_raw)?;
                if n.is_infinite() { return Err(VmError::range_error("day property cannot be Infinity")); }
                n as i32
            } else { cur_d };

            let month_raw = get_prop(ncx, &item, "month")?;
            let month_n = if !month_raw.is_undefined() {
                let n = ncx.to_number_value(&month_raw)?;
                if n.is_infinite() { return Err(VmError::range_error("month property cannot be Infinity")); }
                Some(n as i32)
            } else { None };

            let month_code_raw = get_prop(ncx, &item, "monthCode")?;
            let mc_str = if !month_code_raw.is_undefined() {
                let mc = ncx.to_string_value(&month_code_raw)?;
                validate_month_code_syntax(mc.as_str())?;
                Some(mc)
            } else { None };

            let year_raw = get_prop(ncx, &item, "year")?;
            let year = if !year_raw.is_undefined() {
                let n = ncx.to_number_value(&year_raw)?;
                if n.is_infinite() { return Err(VmError::range_error("year property cannot be Infinity")); }
                n as i32
            } else { cur_y };

            // Check at least one recognized field
            let has_any = [&day_raw, &month_raw, &month_code_raw, &year_raw]
                .iter().any(|v| !v.is_undefined());
            if !has_any {
                return Err(VmError::type_error("with argument must have at least one recognized temporal property"));
            }

            // Validate minimum before options
            let month_pre = month_n.unwrap_or(cur_m);
            if month_pre < 1 { return Err(VmError::range_error(format!("month must be >= 1, got {}", month_pre))); }
            if day < 1 { return Err(VmError::range_error(format!("day must be >= 1, got {}", day))); }

            // Read options AFTER field reads
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = parse_overflow_option(ncx, &options_val)?;

            // Resolve month from monthCode
            let month = if let Some(ref mc) = mc_str {
                let mc_month = validate_month_code_iso_suitability(mc.as_str())? as i32;
                if let Some(mn) = month_n {
                    if mn != mc_month {
                        return Err(VmError::range_error("month and monthCode must agree"));
                    }
                }
                mc_month
            } else { month_pre };

            let result = temporal_rs::PlainDate::new_with_overflow(
                year, month as u8, day as u8,
                temporal_rs::Calendar::default(), overflow,
            ).map_err(temporal_err)?;

            construct_plain_date_value(ncx, result.year(), result.month() as i32, result.day() as i32)
        },
        mm.clone(), fn_proto.clone(), "with", 1,
    );
    proto.define_property(PropertyKey::string("with"), PropertyDescriptor::builtin_method(with_fn));

    // ========================================================================
    // .toPlainDateTime(plainTimeLike?)
    // ========================================================================
    let to_pdt_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toPlainDateTime called on non-object"))?;
            let this_pd = extract_iso_date_from_slots(&obj)?;

            let time_arg = args.first().cloned().unwrap_or(Value::undefined());
            if time_arg.is_undefined() {
                // No time argument — midnight
                let pdt = this_pd.to_plain_date_time(None).map_err(temporal_err)?;
                return construct_plain_date_time_value(ncx, &pdt);
            }

            // If it's a PlainTime object, extract time slots
            if let Some(time_obj) = time_arg.as_object() {
                let tt = time_obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                if tt.as_deref() == Some("PlainTime") {
                    let h = time_obj.get(&PropertyKey::string(SLOT_ISO_HOUR)).and_then(|v| v.as_int32()).unwrap_or(0);
                    let mi = time_obj.get(&PropertyKey::string(SLOT_ISO_MINUTE)).and_then(|v| v.as_int32()).unwrap_or(0);
                    let s = time_obj.get(&PropertyKey::string(SLOT_ISO_SECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
                    let ms = time_obj.get(&PropertyKey::string(SLOT_ISO_MILLISECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
                    let us = time_obj.get(&PropertyKey::string(SLOT_ISO_MICROSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
                    let ns = time_obj.get(&PropertyKey::string(SLOT_ISO_NANOSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
                    let time = temporal_rs::PlainTime::try_new(
                        h as u8, mi as u8, s as u8, ms as u16, us as u16, ns as u16,
                    ).map_err(temporal_err)?;
                    let pdt = this_pd.to_plain_date_time(Some(time)).map_err(temporal_err)?;
                    return construct_plain_date_time_value(ncx, &pdt);
                }
            }

            // Property bag with time fields
            if let Some(time_obj) = time_arg.as_object() {
                let h_val = ncx.get_property(&time_obj, &PropertyKey::string("hour"))?;
                let h = if !h_val.is_undefined() { ncx.to_number_value(&h_val)? as i32 } else { 0 };
                let mi_val = ncx.get_property(&time_obj, &PropertyKey::string("minute"))?;
                let mi = if !mi_val.is_undefined() { ncx.to_number_value(&mi_val)? as i32 } else { 0 };
                let s_val = ncx.get_property(&time_obj, &PropertyKey::string("second"))?;
                let s = if !s_val.is_undefined() { ncx.to_number_value(&s_val)? as i32 } else { 0 };
                let ms_val = ncx.get_property(&time_obj, &PropertyKey::string("millisecond"))?;
                let ms = if !ms_val.is_undefined() { ncx.to_number_value(&ms_val)? as i32 } else { 0 };
                let us_val = ncx.get_property(&time_obj, &PropertyKey::string("microsecond"))?;
                let us = if !us_val.is_undefined() { ncx.to_number_value(&us_val)? as i32 } else { 0 };
                let ns_val = ncx.get_property(&time_obj, &PropertyKey::string("nanosecond"))?;
                let ns = if !ns_val.is_undefined() { ncx.to_number_value(&ns_val)? as i32 } else { 0 };

                let time = temporal_rs::PlainTime::try_new(
                    h.clamp(0, 255) as u8, mi.clamp(0, 255) as u8, s.clamp(0, 255) as u8,
                    ms.clamp(0, 65535) as u16, us.clamp(0, 65535) as u16, ns.clamp(0, 65535) as u16,
                ).map_err(temporal_err)?;
                let pdt = this_pd.to_plain_date_time(Some(time)).map_err(temporal_err)?;
                return construct_plain_date_time_value(ncx, &pdt);
            }

            // String: parse as time
            if time_arg.is_string() {
                let s = ncx.to_string_value(&time_arg)?;
                // Parse as PlainTime via temporal_rs
                let pt = temporal_rs::PlainTime::from_utf8(s.as_bytes()).map_err(temporal_err)?;
                let pdt = this_pd.to_plain_date_time(Some(pt)).map_err(temporal_err)?;
                return construct_plain_date_time_value(ncx, &pdt);
            }

            Err(VmError::type_error("toPlainDateTime: invalid time argument"))
        },
        mm.clone(), fn_proto.clone(), "toPlainDateTime", 0,
    );
    proto.define_property(PropertyKey::string("toPlainDateTime"), PropertyDescriptor::builtin_method(to_pdt_fn));

    // ========================================================================
    // .toPlainYearMonth()
    // ========================================================================
    let to_pym_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toPlainYearMonth called on non-object"))?;
            let pd = extract_iso_date_from_slots(&obj)?;
            let pym = pd.to_plain_year_month().map_err(temporal_err)?;
            construct_plain_year_month_value(ncx, pym.year(), pym.month() as i32)
        },
        mm.clone(), fn_proto.clone(), "toPlainYearMonth", 0,
    );
    proto.define_property(PropertyKey::string("toPlainYearMonth"), PropertyDescriptor::builtin_method(to_pym_fn));

    // ========================================================================
    // .toPlainMonthDay()
    // ========================================================================
    let to_pmd_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toPlainMonthDay called on non-object"))?;
            let pd = extract_iso_date_from_slots(&obj)?;
            let pmd = pd.to_plain_month_day().map_err(temporal_err)?;
            let month = pmd.month_code().to_month_integer() as i32;
            construct_plain_month_day_value(ncx, month, pmd.day() as i32)
        },
        mm.clone(), fn_proto.clone(), "toPlainMonthDay", 0,
    );
    proto.define_property(PropertyKey::string("toPlainMonthDay"), PropertyDescriptor::builtin_method(to_pmd_fn));

    // ========================================================================
    // .withCalendar(calendar)
    // ========================================================================
    let with_cal_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("withCalendar called on non-object"))?;
            let _ = extract_iso_date_from_slots(&obj)?;
            let cal_arg = args.first().cloned().unwrap_or(Value::undefined());
            if cal_arg.is_undefined() {
                return Err(VmError::type_error("missing calendar argument"));
            }
            validate_calendar_arg_standalone(ncx, &cal_arg)?;
            // ISO calendar only — return copy with same fields
            let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
            let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            construct_plain_date_value(ncx, y, m, d)
        },
        mm.clone(), fn_proto.clone(), "withCalendar", 1,
    );
    proto.define_property(PropertyKey::string("withCalendar"), PropertyDescriptor::builtin_method(with_cal_fn));

    // ========================================================================
    // .getISOFields()
    // ========================================================================
    let get_iso_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("getISOFields called on non-object"))?;
            let _ = extract_iso_date_from_slots(&obj)?;
            let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
            let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            let result = GcRef::new(JsObject::new(Value::undefined(), ncx.ctx.memory_manager().clone()));
            result.define_property(PropertyKey::string("calendar"), PropertyDescriptor::data(Value::string(JsString::intern("iso8601"))));
            result.define_property(PropertyKey::string("isoDay"), PropertyDescriptor::data(Value::int32(d)));
            result.define_property(PropertyKey::string("isoMonth"), PropertyDescriptor::data(Value::int32(m)));
            result.define_property(PropertyKey::string("isoYear"), PropertyDescriptor::data(Value::int32(y)));
            Ok(Value::object(result))
        },
        mm.clone(), fn_proto.clone(), "getISOFields", 0,
    );
    proto.define_property(PropertyKey::string("getISOFields"), PropertyDescriptor::builtin_method(get_iso_fn));

    // toString()
    let to_string_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toString on non-PlainDate"))?;
            let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("toString"))?;
            let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);

            // Check calendarName option
            let options_val = args.first().cloned().unwrap_or(Value::undefined());
            let show_calendar = if let Some(opts_obj) = options_val.as_object() {
                let cn = ncx.get_property(&opts_obj, &PropertyKey::string("calendarName"))?;
                if !cn.is_undefined() {
                    let cn_str = ncx.to_string_value(&cn)?;
                    match cn_str.as_str() {
                        "auto" | "never" => false,
                        "always" | "critical" => true,
                        _ => return Err(VmError::range_error(format!("{} is not a valid calendar display option", cn_str))),
                    }
                } else { false }
            } else { false };

            let date_str = format!("{}-{:02}-{:02}", format_iso_year(y), m, d);
            if show_calendar {
                Ok(Value::string(JsString::intern(&format!("{}[u-ca=iso8601]", date_str))))
            } else {
                Ok(Value::string(JsString::intern(&date_str)))
            }
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

    // toJSON()
    let to_json_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toJSON"))?;
            let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("toJSON"))?;
            let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            Ok(Value::string(JsString::intern(&format!("{}-{:02}-{:02}", format_iso_year(y), m, d))))
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

    // valueOf() — always throws
    let value_of_fn = Value::native_function_with_proto_named(
        |_this, _args, _ncx| {
            Err(VmError::type_error("use compare() or toString() to compare Temporal.PlainDate"))
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

    // toLocaleString()
    let to_locale_string_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toLocaleString"))?;
            let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("toLocaleString"))?;
            let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            Ok(Value::string(JsString::intern(&format!("{}-{:02}-{:02}", format_iso_year(y), m, d))))
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
            Value::string(JsString::intern("Temporal.PlainDate")),
            PropertyAttributes { writable: false, enumerable: false, configurable: true },
        ),
    );
}
