use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::temporal_value::TemporalValue;
use crate::value::Value;
use chrono::{Datelike, Timelike};
use std::sync::Arc;

use super::common::*;

// ============================================================================
// PlainDateTime prototype and constructor helpers
// ============================================================================

pub(super) fn install_plain_date_time_prototype(
    proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // Date/time getters via extract_plain_date_time
    for (name, getter_fn) in &[
        (
            "year",
            (|pdt: &temporal_rs::PlainDateTime| Value::int32(pdt.year()))
                as fn(&temporal_rs::PlainDateTime) -> Value,
        ),
        ("month", |pdt: &temporal_rs::PlainDateTime| {
            Value::int32(pdt.month() as i32)
        }),
        ("day", |pdt: &temporal_rs::PlainDateTime| {
            Value::int32(pdt.day() as i32)
        }),
        ("hour", |pdt: &temporal_rs::PlainDateTime| {
            Value::int32(pdt.hour() as i32)
        }),
        ("minute", |pdt: &temporal_rs::PlainDateTime| {
            Value::int32(pdt.minute() as i32)
        }),
        ("second", |pdt: &temporal_rs::PlainDateTime| {
            Value::int32(pdt.second() as i32)
        }),
        ("millisecond", |pdt: &temporal_rs::PlainDateTime| {
            Value::int32(pdt.millisecond() as i32)
        }),
        ("microsecond", |pdt: &temporal_rs::PlainDateTime| {
            Value::int32(pdt.microsecond() as i32)
        }),
        ("nanosecond", |pdt: &temporal_rs::PlainDateTime| {
            Value::int32(pdt.nanosecond() as i32)
        }),
    ] {
        let f = *getter_fn;
        proto.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::Accessor {
                get: Some(Value::native_function_with_proto(
                    move |this, _args, _ncx| {
                        let obj = this
                            .as_object()
                            .ok_or_else(|| VmError::type_error("getter called on non-object"))?;
                        let pdt = extract_plain_date_time(&obj)?;
                        Ok(f(&pdt))
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
    }

    // monthCode
    proto.define_property(
        PropertyKey::string("monthCode"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this
                        .as_object()
                        .ok_or_else(|| VmError::type_error("monthCode called on non-object"))?;
                    let pdt = extract_plain_date_time(&obj)?;
                    Ok(Value::string(JsString::intern(&format_month_code(
                        pdt.month() as u32,
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

    // calendarId
    proto.define_property(
        PropertyKey::string("calendarId"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this
                        .as_object()
                        .ok_or_else(|| VmError::type_error("calendarId called on non-object"))?;
                    let _ = extract_plain_date_time(&obj)?;
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

    // era, eraYear — undefined for ISO
    for name in &["era", "eraYear"] {
        let n = *name;
        proto.define_property(
            PropertyKey::string(n),
            PropertyDescriptor::Accessor {
                get: Some(Value::native_function_with_proto(
                    move |this, _args, _ncx| {
                        let obj = this.as_object().ok_or_else(|| VmError::type_error(n))?;
                        let _ = extract_plain_date_time(&obj)?;
                        Ok(Value::undefined())
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
    }

    // dayOfWeek, dayOfYear, daysInWeek, daysInMonth, daysInYear, monthsInYear, inLeapYear — via temporal_rs::PlainDate derived from PlainDateTime
    for (prop, getter_fn) in &[
        (
            "dayOfWeek",
            (|pd: &temporal_rs::PlainDate| pd.day_of_week() as i32)
                as fn(&temporal_rs::PlainDate) -> i32,
        ),
        ("dayOfYear", |pd: &temporal_rs::PlainDate| {
            pd.day_of_year() as i32
        }),
        ("daysInWeek", |pd: &temporal_rs::PlainDate| {
            pd.days_in_week() as i32
        }),
        ("daysInMonth", |pd: &temporal_rs::PlainDate| {
            pd.days_in_month() as i32
        }),
        ("daysInYear", |pd: &temporal_rs::PlainDate| {
            pd.days_in_year() as i32
        }),
        ("monthsInYear", |pd: &temporal_rs::PlainDate| {
            pd.months_in_year() as i32
        }),
    ] {
        let f = *getter_fn;
        proto.define_property(
            PropertyKey::string(prop),
            PropertyDescriptor::Accessor {
                get: Some(Value::native_function_with_proto(
                    move |this, _args, _ncx| {
                        let obj = this
                            .as_object()
                            .ok_or_else(|| VmError::type_error("getter called on non-object"))?;
                        let pdt = extract_plain_date_time(&obj)?;
                        let pd =
                            temporal_rs::PlainDate::try_new_iso(pdt.year(), pdt.month(), pdt.day())
                                .map_err(temporal_err)?;
                        Ok(Value::int32(f(&pd)))
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
    }

    proto.define_property(
        PropertyKey::string("inLeapYear"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this
                        .as_object()
                        .ok_or_else(|| VmError::type_error("inLeapYear called on non-object"))?;
                    let pdt = extract_plain_date_time(&obj)?;
                    let pd =
                        temporal_rs::PlainDate::try_new_iso(pdt.year(), pdt.month(), pdt.day())
                            .map_err(temporal_err)?;
                    Ok(Value::boolean(pd.in_leap_year()))
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

    // weekOfYear
    proto.define_property(
        PropertyKey::string("weekOfYear"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this
                        .as_object()
                        .ok_or_else(|| VmError::type_error("weekOfYear called on non-object"))?;
                    let pdt = extract_plain_date_time(&obj)?;
                    match pdt.week_of_year() {
                        Some(w) => Ok(Value::int32(w as i32)),
                        None => Ok(Value::undefined()),
                    }
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

    // yearOfWeek
    proto.define_property(
        PropertyKey::string("yearOfWeek"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this
                        .as_object()
                        .ok_or_else(|| VmError::type_error("yearOfWeek called on non-object"))?;
                    let pdt = extract_plain_date_time(&obj)?;
                    match pdt.year_of_week() {
                        Some(y) => Ok(Value::int32(y)),
                        None => Ok(Value::undefined()),
                    }
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

    // Helper: format PlainDateTime to string with options
    fn format_pdt_string(
        obj: &GcRef<JsObject>,
        fractional_digits: Option<i32>, // None = auto, 0-9 = explicit
        smallest_unit: Option<temporal_rs::options::Unit>,
        rounding_mode: temporal_rs::options::RoundingMode,
        calendar_name: &str,
    ) -> Result<String, VmError> {
        let pdt = extract_plain_date_time(obj)?;

        // Determine effective precision from smallestUnit (overrides fractionalSecondDigits)
        let (effective_digits, round_unit) = if let Some(unit) = smallest_unit {
            match unit {
                temporal_rs::options::Unit::Minute => (Some(-1i32), Some(unit)), // special: truncate to minute
                temporal_rs::options::Unit::Second => (Some(0), Some(unit)),
                temporal_rs::options::Unit::Millisecond => (Some(3), Some(unit)),
                temporal_rs::options::Unit::Microsecond => (Some(6), Some(unit)),
                temporal_rs::options::Unit::Nanosecond => (Some(9), Some(unit)),
                _ => {
                    return Err(VmError::range_error(format!(
                        "{:?} is not a valid value for smallest unit",
                        unit
                    )));
                }
            }
        } else {
            (fractional_digits, None)
        };

        // Round the PlainDateTime if needed
        let rounded = if round_unit.is_some()
            || (effective_digits.is_some() && effective_digits != Some(-1))
        {
            let su = round_unit.unwrap_or_else(|| match effective_digits.unwrap_or(9) {
                0 => temporal_rs::options::Unit::Second,
                1..=3 => temporal_rs::options::Unit::Millisecond,
                4..=6 => temporal_rs::options::Unit::Microsecond,
                _ => temporal_rs::options::Unit::Nanosecond,
            });
            let inc_val = match su {
                temporal_rs::options::Unit::Second => 1,
                temporal_rs::options::Unit::Millisecond => {
                    let d = effective_digits.unwrap_or(3);
                    if d <= 0 {
                        1
                    } else {
                        let pow = 10u64.pow((3 - d.min(3)) as u32);
                        if pow == 0 { 1 } else { pow }
                    }
                }
                temporal_rs::options::Unit::Microsecond => {
                    let d = effective_digits.unwrap_or(6);
                    if d <= 3 {
                        1
                    } else {
                        let pow = 10u64.pow((6 - d.min(6)) as u32);
                        if pow == 0 { 1 } else { pow }
                    }
                }
                temporal_rs::options::Unit::Nanosecond => {
                    let d = effective_digits.unwrap_or(9);
                    if d <= 6 {
                        1
                    } else {
                        let pow = 10u64.pow((9 - d.min(9)) as u32);
                        if pow == 0 { 1 } else { pow }
                    }
                }
                temporal_rs::options::Unit::Minute => 1,
                _ => 1,
            };
            let ri = temporal_rs::options::RoundingIncrement::try_from(inc_val as f64)
                .unwrap_or_default();
            let mut opts = temporal_rs::options::RoundingOptions::default();
            opts.largest_unit = None;
            opts.smallest_unit = Some(su);
            opts.rounding_mode = Some(rounding_mode);
            opts.increment = Some(ri);
            pdt.round(opts).map_err(temporal_err)?
        } else {
            pdt
        };

        let y = rounded.iso_year();
        let mo = rounded.iso_month();
        let d = rounded.iso_day();
        let h = rounded.hour();
        let mi = rounded.minute();
        let sec = rounded.second();
        let ms = rounded.millisecond();
        let us = rounded.microsecond();
        let ns = rounded.nanosecond();

        let date_part = if y < 0 || y > 9999 {
            format!("{:+07}-{:02}-{:02}", y, mo, d)
        } else {
            format!("{:04}-{:02}-{:02}", y, mo, d)
        };

        let time_part = if effective_digits == Some(-1) {
            // minute precision
            format!("T{:02}:{:02}", h, mi)
        } else {
            let sub = ns as i64 + us as i64 * 1000 + ms as i64 * 1_000_000;
            match effective_digits {
                Some(0) => format!("T{:02}:{:02}:{:02}", h, mi, sec),
                Some(n) if n > 0 => {
                    let frac = format!("{:09}", sub);
                    let trimmed = &frac[..n as usize];
                    format!("T{:02}:{:02}:{:02}.{}", h, mi, sec, trimmed)
                }
                None => {
                    // auto: trim trailing zeros
                    if sub != 0 {
                        let frac = format!("{:09}", sub);
                        let trimmed = frac.trim_end_matches('0');
                        format!("T{:02}:{:02}:{:02}.{}", h, mi, sec, trimmed)
                    } else if sec != 0 || mi != 0 || h != 0 {
                        format!("T{:02}:{:02}:{:02}", h, mi, sec)
                    } else {
                        "T00:00:00".to_string()
                    }
                }
                _ => format!("T{:02}:{:02}:{:02}", h, mi, sec),
            }
        };

        let cal_suffix = match calendar_name {
            "always" => "[u-ca=iso8601]",
            "critical" => "[!u-ca=iso8601]",
            "auto" | "never" | _ => "",
        };

        Ok(format!("{}{}{}", date_part, time_part, cal_suffix))
    }

    // toString
    let to_string_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toString called on non-PlainDateTime"))?;
            // Branding check via extract
            let _ = extract_plain_date_time(&obj)?;

            let options_val = args.first().cloned().unwrap_or(Value::undefined());
            if options_val.is_undefined() {
                return Ok(Value::string(JsString::intern(&format_pdt_string(
                    &obj,
                    None,
                    None,
                    temporal_rs::options::RoundingMode::Trunc,
                    "auto",
                )?)));
            }
            if !options_val.is_object() && options_val.as_proxy().is_none() {
                return Err(VmError::type_error("options must be an object"));
            }

            // Read options in alphabetical order per spec
            let cal_val = get_val_property(ncx, &options_val, "calendarName")?;
            let calendar_name = if !cal_val.is_undefined() {
                let s = ncx.to_string_value(&cal_val)?;
                match s.as_str() {
                    "auto" | "always" | "never" | "critical" => s,
                    _ => {
                        return Err(VmError::range_error(format!(
                            "{} is not a valid value for calendarName option",
                            s
                        )));
                    }
                }
            } else {
                "auto".to_string()
            };

            let fsd_val = get_val_property(ncx, &options_val, "fractionalSecondDigits")?;
            let fractional_digits = if !fsd_val.is_undefined() {
                // Per spec GetStringOrNumberOption: if typeof is "number", use as number; else ToString
                if fsd_val.is_number() {
                    let n = fsd_val.as_number().unwrap_or(f64::NAN);
                    if n.is_nan() || n.is_infinite() {
                        return Err(VmError::range_error(
                            "fractionalSecondDigits must be auto or 0-9",
                        ));
                    }
                    let n = n.floor() as i32;
                    if !(0..=9).contains(&n) {
                        return Err(VmError::range_error(
                            "fractionalSecondDigits must be auto or 0-9",
                        ));
                    }
                    Some(n)
                } else {
                    // Not a number — convert to string and check for "auto"
                    let s = ncx.to_string_value(&fsd_val)?;
                    if s == "auto" {
                        None
                    } else {
                        return Err(VmError::range_error(format!(
                            "{} is not a valid value for fractionalSecondDigits",
                            s
                        )));
                    }
                }
            } else {
                None
            };

            let rm_val = get_val_property(ncx, &options_val, "roundingMode")?;
            let rounding_mode = if !rm_val.is_undefined() {
                let s = ncx.to_string_value(&rm_val)?;
                parse_rounding_mode(&s)?
            } else {
                temporal_rs::options::RoundingMode::Trunc
            };

            let su_val = get_val_property(ncx, &options_val, "smallestUnit")?;
            let smallest_unit = if !su_val.is_undefined() {
                let s = ncx.to_string_value(&su_val)?;
                let u = parse_temporal_unit(&s)?;
                match u {
                    temporal_rs::options::Unit::Year
                    | temporal_rs::options::Unit::Month
                    | temporal_rs::options::Unit::Week
                    | temporal_rs::options::Unit::Day
                    | temporal_rs::options::Unit::Auto => {
                        return Err(VmError::range_error(format!(
                            "{} is not a valid value for smallest unit",
                            s
                        )));
                    }
                    _ => Some(u),
                }
            } else {
                None
            };

            Ok(Value::string(JsString::intern(&format_pdt_string(
                &obj,
                fractional_digits,
                smallest_unit,
                rounding_mode,
                &calendar_name,
            )?)))
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

    // toJSON
    let to_json_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toJSON called on non-PlainDateTime"))?;
            // Branding check via extract
            let _ = extract_plain_date_time(&obj)?;
            // Delegate to toString
            if let Some(ts) = obj.get(&PropertyKey::string("toString")) {
                return ncx.call_function(&ts, this.clone(), &[]);
            }
            Err(VmError::type_error("toJSON called on non-PlainDateTime"))
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

    // valueOf — throws
    let value_of_fn = Value::native_function_with_proto_named(
        |_this, _args, _ncx| {
            Err(VmError::type_error(
                "use compare() or toString() to compare Temporal.PlainDateTime",
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

    // toLocaleString
    let to_locale_string_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toLocaleString called on non-object"))?;
            let _ = extract_plain_date_time(&obj)?;
            if let Some(ts) = obj.get(&PropertyKey::string("toString")) {
                return ncx.call_function(&ts, this.clone(), &[]);
            }
            Err(VmError::type_error(
                "toLocaleString called on non-PlainDateTime",
            ))
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

    // toPlainDate
    let to_plain_date_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toPlainDate called on non-object"))?;
            let pdt = extract_plain_date_time(&obj)?;
            let temporal_ns = ncx
                .ctx
                .get_global("Temporal")
                .ok_or_else(|| VmError::type_error("Temporal not found"))?;
            let temporal_obj = temporal_ns
                .as_object()
                .ok_or_else(|| VmError::type_error("Temporal not found"))?;
            let pd_ctor = temporal_obj
                .get(&PropertyKey::string("PlainDate"))
                .ok_or_else(|| VmError::type_error("PlainDate not found"))?;
            ncx.call_function_construct(
                &pd_ctor,
                Value::undefined(),
                &[
                    Value::int32(pdt.year()),
                    Value::int32(pdt.month() as i32),
                    Value::int32(pdt.day() as i32),
                ],
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "toPlainDate",
        0,
    );
    proto.define_property(
        PropertyKey::string("toPlainDate"),
        PropertyDescriptor::builtin_method(to_plain_date_fn),
    );

    // with — PlainDateTime.prototype.with(temporalDateTimeLike [, options])
    let with_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("with called on non-PlainDateTime"))?;
            // Extract current PlainDateTime via TemporalValue
            let cur_pdt = extract_plain_date_time(&obj)?;

            // Get current values from the extracted PlainDateTime
            let cur_y = cur_pdt.year();
            let cur_m = cur_pdt.month() as i32;
            let cur_d = cur_pdt.day() as i32;
            let cur_h = cur_pdt.hour() as i32;
            let cur_mi = cur_pdt.minute() as i32;
            let cur_s = cur_pdt.second() as i32;
            let cur_ms = cur_pdt.millisecond() as i32;
            let cur_us = cur_pdt.microsecond() as i32;
            let cur_ns = cur_pdt.nanosecond() as i32;

            let item = args.first().cloned().unwrap_or(Value::undefined());

            // Helper: get property from object or proxy
            let get_prop = |ncx: &mut NativeContext<'_>,
                            item: &Value,
                            name: &str|
             -> Result<Value, VmError> {
                if let Some(proxy) = item.as_proxy() {
                    let key = PropertyKey::string(name);
                    let key_value = crate::proxy_operations::property_key_to_value_pub(&key);
                    crate::proxy_operations::proxy_get(ncx, proxy, &key, key_value, item.clone())
                } else if let Some(obj) = item.as_object() {
                    ncx.get_property(&obj, &PropertyKey::string(name))
                } else {
                    Err(VmError::type_error("with argument must be an object"))
                }
            };

            // Argument must be an object (including Proxy)
            if item.as_object().is_none() && item.as_proxy().is_none() {
                return Err(VmError::type_error("with argument must be an object"));
            }

            // Reject if item is a known Temporal type
            if let Some(item_obj) = item.as_object() {
                if let Some(item_ty) = item_obj
                    .get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
                {
                    if !item_ty.is_empty() {
                        return Err(VmError::type_error(
                            "with argument must be a partial object, not a Temporal type",
                        ));
                    }
                }
            }

            // Step 1: RejectObjectWithCalendarOrTimeZone — BEFORE field reads
            let cal_v = get_prop(ncx, &item, "calendar")?;
            if !cal_v.is_undefined() {
                return Err(VmError::type_error("calendar not allowed in with argument"));
            }
            let tz_v = get_prop(ncx, &item, "timeZone")?;
            if !tz_v.is_undefined() {
                return Err(VmError::type_error("timeZone not allowed in with argument"));
            }

            // Step 2: PrepareTemporalFields — get + IMMEDIATELY convert each field (alphabetical)
            // Each field: get → immediately convert via valueOf/toString
            let day_raw = get_prop(ncx, &item, "day")?;
            let day = if !day_raw.is_undefined() {
                let n = ncx.to_number_value(&day_raw)?;
                if n.is_infinite() {
                    return Err(VmError::range_error("day property cannot be Infinity"));
                }
                n as i32
            } else {
                cur_d
            };

            let hour_raw = get_prop(ncx, &item, "hour")?;
            let hour = if !hour_raw.is_undefined() {
                let n = ncx.to_number_value(&hour_raw)?;
                if n.is_infinite() {
                    return Err(VmError::range_error("hour property cannot be Infinity"));
                }
                n as i32
            } else {
                cur_h
            };

            let microsecond_raw = get_prop(ncx, &item, "microsecond")?;
            let microsecond = if !microsecond_raw.is_undefined() {
                let n = ncx.to_number_value(&microsecond_raw)?;
                if n.is_infinite() {
                    return Err(VmError::range_error(
                        "microsecond property cannot be Infinity",
                    ));
                }
                n as i32
            } else {
                cur_us
            };

            let millisecond_raw = get_prop(ncx, &item, "millisecond")?;
            let millisecond = if !millisecond_raw.is_undefined() {
                let n = ncx.to_number_value(&millisecond_raw)?;
                if n.is_infinite() {
                    return Err(VmError::range_error(
                        "millisecond property cannot be Infinity",
                    ));
                }
                n as i32
            } else {
                cur_ms
            };

            let minute_raw = get_prop(ncx, &item, "minute")?;
            let minute = if !minute_raw.is_undefined() {
                let n = ncx.to_number_value(&minute_raw)?;
                if n.is_infinite() {
                    return Err(VmError::range_error("minute property cannot be Infinity"));
                }
                n as i32
            } else {
                cur_mi
            };

            let month_raw = get_prop(ncx, &item, "month")?;
            let month_n = if !month_raw.is_undefined() {
                let n = ncx.to_number_value(&month_raw)?;
                if n.is_infinite() {
                    return Err(VmError::range_error("month property cannot be Infinity"));
                }
                Some(n as i32)
            } else {
                None
            };

            let month_code_raw = get_prop(ncx, &item, "monthCode")?;
            // Only read and convert monthCode here; validation happens AFTER options
            let mc_str = if !month_code_raw.is_undefined() {
                Some(ncx.to_string_value(&month_code_raw)?)
            } else {
                None
            };
            // Temporary month for basic below-min validation (monthCode validation deferred)
            let month_pre = if let Some(mn) = month_n { mn } else { cur_m };

            let nanosecond_raw = get_prop(ncx, &item, "nanosecond")?;
            let nanosecond = if !nanosecond_raw.is_undefined() {
                let n = ncx.to_number_value(&nanosecond_raw)?;
                if n.is_infinite() {
                    return Err(VmError::range_error(
                        "nanosecond property cannot be Infinity",
                    ));
                }
                n as i32
            } else {
                cur_ns
            };

            let second_raw = get_prop(ncx, &item, "second")?;
            let second = if !second_raw.is_undefined() {
                let n = ncx.to_number_value(&second_raw)?;
                if n.is_infinite() {
                    return Err(VmError::range_error("second property cannot be Infinity"));
                }
                n as i32
            } else {
                cur_s
            };

            let year_raw = get_prop(ncx, &item, "year")?;
            let year = if !year_raw.is_undefined() {
                let n = ncx.to_number_value(&year_raw)?;
                if n.is_infinite() {
                    return Err(VmError::range_error("year property cannot be Infinity"));
                }
                n as i32
            } else {
                cur_y
            };

            // Check at least one field is defined
            let has_any = [
                &day_raw,
                &hour_raw,
                &microsecond_raw,
                &millisecond_raw,
                &minute_raw,
                &month_raw,
                &month_code_raw,
                &nanosecond_raw,
                &second_raw,
                &year_raw,
            ]
            .iter()
            .any(|v| !v.is_undefined());
            if !has_any {
                return Err(VmError::type_error(
                    "with argument must have at least one recognized temporal property",
                ));
            }

            // CalendarResolveFields: reject below-minimum values BEFORE options
            // (above-maximum values are handled by overflow constrain/reject after options)
            if month_pre < 1 {
                return Err(VmError::range_error(format!(
                    "month must be >= 1, got {}",
                    month_pre
                )));
            }
            if day < 1 {
                return Err(VmError::range_error(format!(
                    "day must be >= 1, got {}",
                    day
                )));
            }
            if hour < 0 {
                return Err(VmError::range_error(format!(
                    "hour must be >= 0, got {}",
                    hour
                )));
            }
            if minute < 0 {
                return Err(VmError::range_error(format!(
                    "minute must be >= 0, got {}",
                    minute
                )));
            }
            if second < 0 {
                return Err(VmError::range_error(format!(
                    "second must be >= 0, got {}",
                    second
                )));
            }
            if millisecond < 0 {
                return Err(VmError::range_error(format!(
                    "millisecond must be >= 0, got {}",
                    millisecond
                )));
            }
            if microsecond < 0 {
                return Err(VmError::range_error(format!(
                    "microsecond must be >= 0, got {}",
                    microsecond
                )));
            }
            if nanosecond < 0 {
                return Err(VmError::range_error(format!(
                    "nanosecond must be >= 0, got {}",
                    nanosecond
                )));
            }

            // Step 3: Read options — AFTER field reads and basic validation, BEFORE monthCode validation
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = parse_overflow_option(ncx, &options_val)?;

            // Resolve month from monthCode AFTER options (per spec: options read before algorithmic validation)
            let month = if let Some(ref mc) = mc_str {
                validate_month_code_syntax(mc.as_str())?;
                let mc_month = validate_month_code_iso_suitability(mc.as_str())? as i32;
                if let Some(mn) = month_n {
                    if mn != mc_month {
                        return Err(VmError::range_error("month and monthCode must agree"));
                    }
                }
                mc_month
            } else {
                month_pre
            };

            // Use temporal_rs for full validation including calendar-specific checks
            let ov = overflow;
            let pdt = temporal_rs::PlainDateTime::new_with_overflow(
                year,
                month as u8,
                day as u8,
                hour.clamp(0, 255) as u8,
                minute.clamp(0, 255) as u8,
                second.clamp(0, 255) as u8,
                millisecond.clamp(0, 65535) as u16,
                microsecond.clamp(0, 65535) as u16,
                nanosecond.clamp(0, 65535) as u16,
                temporal_rs::Calendar::default(),
                ov,
            )
            .map_err(temporal_err)?;

            // Subclassing ignored — always use Temporal.PlainDateTime constructor prototype
            let temporal_ns = ncx
                .ctx
                .get_global("Temporal")
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let temporal_obj_ns = temporal_ns
                .as_object()
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let pdt_ctor = temporal_obj_ns
                .get(&PropertyKey::string("PlainDateTime"))
                .ok_or_else(|| VmError::type_error("PlainDateTime constructor not found"))?;
            let pdt_ctor_obj = pdt_ctor
                .as_object()
                .ok_or_else(|| VmError::type_error("PlainDateTime is not a function"))?;
            let pdt_proto = pdt_ctor_obj
                .get(&PropertyKey::string("prototype"))
                .unwrap_or(Value::undefined());

            let result_obj = GcRef::new(JsObject::new(pdt_proto, ncx.ctx.memory_manager().clone()));
            store_temporal_inner(&result_obj, TemporalValue::PlainDateTime(pdt));
            Ok(Value::object(result_obj))
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

    // toZonedDateTime — implementation for UTC and fixed-offset timezones
    let to_zoned_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| {
                VmError::type_error("toZonedDateTime called on non-PlainDateTime")
            })?;
            // Extract PlainDateTime via TemporalValue (also serves as branding check)
            let this_pdt = extract_plain_date_time(&obj)?;

            // Get timeZone argument
            let tz_arg = args.first().cloned().unwrap_or(Value::undefined());

            // Type check: primitives → TypeError (except string)
            if tz_arg.is_undefined()
                || tz_arg.is_null()
                || tz_arg.is_boolean()
                || tz_arg.is_number()
                || tz_arg.is_bigint()
            {
                return Err(VmError::type_error(format!(
                    "{} is not a valid time zone",
                    if tz_arg.is_null() {
                        "null"
                    } else if tz_arg.is_undefined() {
                        "undefined"
                    } else {
                        tz_arg.type_of()
                    }
                )));
            }
            if tz_arg.as_symbol().is_some() {
                return Err(VmError::type_error(
                    "Cannot convert a Symbol value to a string",
                ));
            }
            // Objects (non-string) → TypeError
            if tz_arg.as_object().is_some() || tz_arg.as_proxy().is_some() {
                return Err(VmError::type_error("object is not a valid time zone"));
            }

            let tz_str = ncx.to_string_value(&tz_arg)?;
            let tz_s = tz_str.as_str();

            // If it's an empty string, throw RangeError
            if tz_s.is_empty() {
                return Err(VmError::range_error("time zone string must not be empty"));
            }

            // Use temporal_rs for spec-compliant timezone parsing (with provider for IANA support)
            let tz = temporal_rs::TimeZone::try_from_str_with_provider(tz_s, tz_provider())
                .map_err(temporal_err)?;

            // Read disambiguation option
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let disambiguation = if !options_val.is_undefined() {
                if options_val.is_null()
                    || options_val.is_boolean()
                    || options_val.is_number()
                    || options_val.is_string()
                    || options_val.is_bigint()
                    || options_val.as_symbol().is_some()
                {
                    return Err(VmError::type_error(
                        "options must be an object or undefined",
                    ));
                }
                let dis_val = ncx
                    .get_property_of_value(&options_val, &PropertyKey::string("disambiguation"))?;
                if !dis_val.is_undefined() {
                    let dis_str = ncx.to_string_value(&dis_val)?;
                    match dis_str.as_str() {
                        "compatible" => temporal_rs::options::Disambiguation::Compatible,
                        "earlier" => temporal_rs::options::Disambiguation::Earlier,
                        "later" => temporal_rs::options::Disambiguation::Later,
                        "reject" => temporal_rs::options::Disambiguation::Reject,
                        _ => {
                            return Err(VmError::range_error(format!(
                                "{} is not a valid value for disambiguation",
                                dis_str
                            )));
                        }
                    }
                } else {
                    temporal_rs::options::Disambiguation::Compatible
                }
            } else {
                temporal_rs::options::Disambiguation::Compatible
            };

            // Use temporal_rs to properly handle DST disambiguation for named timezones
            let zdt = this_pdt
                .to_zoned_date_time_with_provider(tz, disambiguation, tz_provider())
                .map_err(temporal_err)?;

            construct_zoned_date_time_value(ncx, &zdt)
        },
        mm.clone(),
        fn_proto.clone(),
        "toZonedDateTime",
        1,
    );
    proto.define_property(
        PropertyKey::string("toZonedDateTime"),
        PropertyDescriptor::builtin_method(to_zoned_fn),
    );

    // .equals(other) method
    let equals_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("equals called on non-object"))?;
            let this_pdt = extract_plain_date_time(&obj)?;

            let other = args.first().cloned().unwrap_or(Value::undefined());
            let other_pdt = to_temporal_datetime(ncx, &other)?;

            Ok(Value::boolean(
                this_pdt.compare_iso(&other_pdt) == std::cmp::Ordering::Equal,
            ))
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

    // Helper: read a property from a Value (handles both JsObject and Proxy)
    fn get_val_property(
        ncx: &mut NativeContext<'_>,
        val: &Value,
        key: &str,
    ) -> Result<Value, VmError> {
        if let Some(obj) = val.as_object() {
            return ncx.get_property(&obj, &PropertyKey::string(key));
        }
        if let Some(proxy) = val.as_proxy() {
            return proxy_get_property(ncx, proxy, val, key);
        }
        Ok(Value::undefined())
    }

    // Helper: parse unit string to temporal_rs::options::Unit
    fn parse_temporal_unit(s: &str) -> Result<temporal_rs::options::Unit, VmError> {
        match s {
            "auto" => Ok(temporal_rs::options::Unit::Auto),
            "year" | "years" => Ok(temporal_rs::options::Unit::Year),
            "month" | "months" => Ok(temporal_rs::options::Unit::Month),
            "week" | "weeks" => Ok(temporal_rs::options::Unit::Week),
            "day" | "days" => Ok(temporal_rs::options::Unit::Day),
            "hour" | "hours" => Ok(temporal_rs::options::Unit::Hour),
            "minute" | "minutes" => Ok(temporal_rs::options::Unit::Minute),
            "second" | "seconds" => Ok(temporal_rs::options::Unit::Second),
            "millisecond" | "milliseconds" => Ok(temporal_rs::options::Unit::Millisecond),
            "microsecond" | "microseconds" => Ok(temporal_rs::options::Unit::Microsecond),
            "nanosecond" | "nanoseconds" => Ok(temporal_rs::options::Unit::Nanosecond),
            _ => Err(VmError::range_error(format!("{} is not a valid unit", s))),
        }
    }

    // Helper: parse rounding mode string to temporal_rs::options::RoundingMode
    fn parse_rounding_mode(s: &str) -> Result<temporal_rs::options::RoundingMode, VmError> {
        match s {
            "ceil" => Ok(temporal_rs::options::RoundingMode::Ceil),
            "floor" => Ok(temporal_rs::options::RoundingMode::Floor),
            "expand" => Ok(temporal_rs::options::RoundingMode::Expand),
            "trunc" => Ok(temporal_rs::options::RoundingMode::Trunc),
            "halfCeil" => Ok(temporal_rs::options::RoundingMode::HalfCeil),
            "halfFloor" => Ok(temporal_rs::options::RoundingMode::HalfFloor),
            "halfExpand" => Ok(temporal_rs::options::RoundingMode::HalfExpand),
            "halfTrunc" => Ok(temporal_rs::options::RoundingMode::HalfTrunc),
            "halfEven" => Ok(temporal_rs::options::RoundingMode::HalfEven),
            _ => Err(VmError::range_error(format!(
                "{} is not a valid rounding mode",
                s
            ))),
        }
    }

    // Helper: validate calendar argument — delegates to shared validator (lenient: accepts ISO strings)
    fn validate_calendar_arg(ncx: &mut NativeContext<'_>, cal: &Value) -> Result<String, VmError> {
        resolve_calendar_from_property(ncx, cal)?;
        Ok("iso8601".to_string())
    }

    // Helper: read and validate a numeric temporal field, checking Infinity
    fn read_temporal_number(
        ncx: &mut NativeContext<'_>,
        val: &Value,
        field: &str,
    ) -> Result<f64, VmError> {
        let n = ncx.to_number_value(val)?;
        if n.is_infinite() {
            return Err(VmError::range_error(format!(
                "{} property cannot be Infinity",
                field
            )));
        }
        Ok(n)
    }

    // Helper: ToTemporalDateTime — convert a Value to a temporal_rs::PlainDateTime
    // Handles: PlainDateTime objects, ZonedDateTime objects, property bags {year, month, day}, strings
    fn to_temporal_datetime(
        ncx: &mut NativeContext<'_>,
        item: &Value,
    ) -> Result<temporal_rs::PlainDateTime, VmError> {
        if item.is_string() {
            let s = ncx.to_string_value(item)?;
            reject_utc_designator_for_plain(s.as_str())?;
            return temporal_rs::PlainDateTime::from_utf8(s.as_bytes()).map_err(temporal_err);
        }

        if item.is_undefined()
            || item.is_null()
            || item.is_boolean()
            || item.is_number()
            || item.is_bigint()
        {
            return Err(VmError::type_error(format!(
                "cannot convert {} to a PlainDateTime",
                item.type_of()
            )));
        }

        if item.as_symbol().is_some() {
            return Err(VmError::type_error(
                "Cannot convert a Symbol value to a string",
            ));
        }

        // Handle both JsObject and Proxy
        if item.as_object().is_none() && item.as_proxy().is_none() {
            return Err(VmError::type_error("Expected an object or string"));
        }

        // Check if it's a Temporal type (only on real objects, not proxies)
        if let Some(obj) = item.as_object() {
            if let Ok(pdt) = extract_plain_date_time(&obj) {
                return Ok(pdt);
            }

            if let Ok(pd) = extract_plain_date(&obj) {
                return temporal_rs::PlainDateTime::try_new(
                    pd.year(),
                    pd.month(),
                    pd.day(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    temporal_rs::Calendar::default(),
                )
                .map_err(temporal_err);
            }

            let temporal_type = obj
                .get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));

            if temporal_type.as_deref() == Some("ZonedDateTime") {
                // Use temporal_rs to convert ZonedDateTime → PlainDateTime correctly
                let zdt = extract_zoned_date_time(&obj)?;
                return Ok(zdt.to_plain_date_time());
            }
        }

        // Property bag — read AND convert each property in ALPHABETICAL order per spec
        // Order: calendar, day, hour, microsecond, millisecond, minute, month, monthCode, nanosecond, second, year
        // Each property is read then immediately converted (get → valueOf/toString) per spec observable ordering
        let calendar_val = get_val_property(ncx, item, "calendar")?;
        if !calendar_val.is_undefined() {
            validate_calendar_arg(ncx, &calendar_val)?;
        }

        // day — read and convert immediately
        let day_val = get_val_property(ncx, item, "day")?;
        let d = if !day_val.is_undefined() {
            read_temporal_number(ncx, &day_val, "day")? as i32
        } else {
            -1
        }; // -1 = missing

        // hour
        let hour_val = get_val_property(ncx, item, "hour")?;
        let h = if !hour_val.is_undefined() {
            read_temporal_number(ncx, &hour_val, "hour")? as i32
        } else {
            0
        };

        // microsecond
        let us_val = get_val_property(ncx, item, "microsecond")?;
        let us = if !us_val.is_undefined() {
            read_temporal_number(ncx, &us_val, "microsecond")? as i32
        } else {
            0
        };

        // millisecond
        let ms_val = get_val_property(ncx, item, "millisecond")?;
        let ms = if !ms_val.is_undefined() {
            read_temporal_number(ncx, &ms_val, "millisecond")? as i32
        } else {
            0
        };

        // minute
        let minute_val = get_val_property(ncx, item, "minute")?;
        let mi = if !minute_val.is_undefined() {
            read_temporal_number(ncx, &minute_val, "minute")? as i32
        } else {
            0
        };

        // month
        let month_val = get_val_property(ncx, item, "month")?;
        let month_num = if !month_val.is_undefined() {
            Some(read_temporal_number(ncx, &month_val, "month")? as i32)
        } else {
            None
        };

        // monthCode
        let month_code_val = get_val_property(ncx, item, "monthCode")?;
        let month_from_code = if !month_code_val.is_undefined() {
            let mc_str = ncx.to_string_value(&month_code_val)?;
            validate_month_code_syntax(&mc_str)?;
            Some(validate_month_code_iso_suitability(&mc_str)? as i32)
        } else {
            None
        };

        // nanosecond
        let ns_val = get_val_property(ncx, item, "nanosecond")?;
        let ns = if !ns_val.is_undefined() {
            read_temporal_number(ncx, &ns_val, "nanosecond")? as i32
        } else {
            0
        };

        // second
        let second_val = get_val_property(ncx, item, "second")?;
        let sec = if !second_val.is_undefined() {
            let sv = read_temporal_number(ncx, &second_val, "second")? as i32;
            if sv == 60 { 59 } else { sv }
        } else {
            0
        };

        // year
        let year_val = get_val_property(ncx, item, "year")?;
        let y = if !year_val.is_undefined() {
            Some(read_temporal_number(ncx, &year_val, "year")? as i32)
        } else {
            None
        };

        // Check for required fields
        let y = match y {
            Some(y) => y,
            None => {
                if month_num.is_none() && month_from_code.is_none() && d == -1 {
                    return Err(VmError::type_error(
                        "plain object is not a valid property bag and does not convert to a string",
                    ));
                }
                return Err(VmError::type_error("year is required"));
            }
        };

        let month = if let Some(mc) = month_from_code {
            mc
        } else if let Some(m) = month_num {
            m
        } else {
            return Err(VmError::type_error("month or monthCode is required"));
        };

        if d == -1 {
            return Err(VmError::type_error("day is required"));
        }

        temporal_rs::PlainDateTime::try_new(
            y,
            month as u8,
            d as u8,
            h as u8,
            mi as u8,
            sec as u8,
            ms as u16,
            us as u16,
            ns as u16,
            temporal_rs::Calendar::default(),
        )
        .map_err(temporal_err)
    }

    // Helper: ToTemporalDuration — convert a Value to a temporal_rs::Duration
    fn to_temporal_duration(
        ncx: &mut NativeContext<'_>,
        item: &Value,
    ) -> Result<temporal_rs::Duration, VmError> {
        if item.is_string() {
            let s = ncx.to_string_value(item)?;
            return temporal_rs::Duration::from_utf8(s.as_bytes()).map_err(temporal_err);
        }
        if item.is_undefined()
            || item.is_null()
            || item.is_boolean()
            || item.is_number()
            || item.is_bigint()
        {
            return Err(VmError::type_error(format!(
                "cannot convert {} to a Duration",
                item.type_of()
            )));
        }
        if item.as_symbol().is_some() {
            return Err(VmError::type_error(
                "Cannot convert a Symbol value to a string",
            ));
        }
        // Handle both JsObject and Proxy
        if item.as_object().is_none() && item.as_proxy().is_none() {
            return Err(VmError::type_error(
                "Expected an object or string for duration",
            ));
        }

        // Check for array or function (not valid duration)
        if let Some(obj) = item.as_object() {
            if obj.is_array() {
                return Err(VmError::type_error("cannot convert array to a Duration"));
            }
        }
        if item.is_callable() {
            return Err(VmError::type_error("cannot convert function to a Duration"));
        }

        // If it's a Duration Temporal object, extract from internal slots
        if let Some(obj) = item.as_object() {
            let temporal_type = obj
                .get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if temporal_type.as_deref() == Some("Duration") {
                return extract_duration_from_slots(&obj);
            }
        }

        // Read AND convert each duration field IMMEDIATELY in ALPHABETICAL order per spec:
        // days, hours, microseconds, milliseconds, minutes, months, nanoseconds, seconds, weeks, years
        // Each get is immediately followed by valueOf conversion for observable ordering
        fn read_dur_field(
            ncx: &mut NativeContext<'_>,
            item: &Value,
            field: &str,
        ) -> Result<(bool, f64), VmError> {
            let v = get_val_property(ncx, item, field)?;
            if v.is_undefined() {
                return Ok((false, 0.0));
            }
            let n = ncx.to_number_value(&v)?;
            if n.is_infinite() {
                return Err(VmError::range_error(format!(
                    "{} property cannot be Infinity",
                    field
                )));
            }
            if n.is_nan() {
                return Err(VmError::range_error(format!(
                    "{} property cannot be NaN",
                    field
                )));
            }
            if n != n.trunc() {
                return Err(VmError::range_error(format!(
                    "{} property must be an integer",
                    field
                )));
            }
            Ok((true, n))
        }

        let (has_days, days) = read_dur_field(ncx, item, "days")?;
        let (has_hours, hours) = read_dur_field(ncx, item, "hours")?;
        let (has_us, microseconds) = read_dur_field(ncx, item, "microseconds")?;
        let (has_ms, milliseconds) = read_dur_field(ncx, item, "milliseconds")?;
        let (has_min, minutes) = read_dur_field(ncx, item, "minutes")?;
        let (has_mo, months) = read_dur_field(ncx, item, "months")?;
        let (has_ns, nanoseconds) = read_dur_field(ncx, item, "nanoseconds")?;
        let (has_sec, seconds) = read_dur_field(ncx, item, "seconds")?;
        let (has_wk, weeks) = read_dur_field(ncx, item, "weeks")?;
        let (has_yr, years) = read_dur_field(ncx, item, "years")?;

        let has_any = has_days
            || has_hours
            || has_us
            || has_ms
            || has_min
            || has_mo
            || has_ns
            || has_sec
            || has_wk
            || has_yr;

        if !has_any {
            return Err(VmError::type_error(
                "duration object must have at least one temporal property",
            ));
        }

        temporal_rs::Duration::new(
            years as i64,
            months as i64,
            weeks as i64,
            days as i64,
            hours as i64,
            minutes as i64,
            seconds as i64,
            milliseconds as i64,
            microseconds as i128,
            nanoseconds as i128,
        )
        .map_err(temporal_err)
    }

    // Helper: parse difference options in ALPHABETICAL order per spec:
    // largestUnit, roundingIncrement, roundingMode, smallestUnit
    fn parse_difference_settings(
        ncx: &mut NativeContext<'_>,
        options_val: &Value,
    ) -> Result<temporal_rs::options::DifferenceSettings, VmError> {
        let mut settings = temporal_rs::options::DifferenceSettings::default();
        if options_val.is_undefined() {
            return Ok(settings);
        }
        // Per GetOptionsObject: only undefined → default, only Object/Proxy → use it, else → TypeError
        if !options_val.is_object() && options_val.as_proxy().is_none() {
            return Err(VmError::type_error("options must be an object"));
        }
        // Read all options first, then validate — alphabetical order
        let lu_val = get_val_property(ncx, options_val, "largestUnit")?;
        let lu_parsed = if !lu_val.is_undefined() {
            let lu_str = ncx.to_string_value(&lu_val)?;
            Some(parse_temporal_unit(&lu_str)?)
        } else {
            None
        };

        let ri_val = get_val_property(ncx, options_val, "roundingIncrement")?;
        let ri_parsed = if !ri_val.is_undefined() {
            let ri_num = ncx.to_number_value(&ri_val)?;
            Some(temporal_rs::options::RoundingIncrement::try_from(ri_num).map_err(temporal_err)?)
        } else {
            None
        };

        let rm_val = get_val_property(ncx, options_val, "roundingMode")?;
        let rm_parsed = if !rm_val.is_undefined() {
            let rm_str = ncx.to_string_value(&rm_val)?;
            Some(parse_rounding_mode(&rm_str)?)
        } else {
            None
        };

        let su_val = get_val_property(ncx, options_val, "smallestUnit")?;
        let su_parsed = if !su_val.is_undefined() {
            let su_str = ncx.to_string_value(&su_val)?;
            Some(parse_temporal_unit(&su_str)?)
        } else {
            None
        };

        settings.largest_unit = lu_parsed;
        settings.smallest_unit = su_parsed;
        settings.rounding_mode = rm_parsed;
        settings.increment = ri_parsed;
        Ok(settings)
    }

    // .since(other, options) method
    let since_fn = Value::native_function_with_proto_named(
        move |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("since called on non-object"))?;
            let this_pdt = extract_plain_date_time(&obj)?;

            let other_arg = args.first().cloned().unwrap_or(Value::undefined());
            let other_pdt = to_temporal_datetime(ncx, &other_arg)?;

            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let settings = parse_difference_settings(ncx, &options_val)?;

            let duration = this_pdt.since(&other_pdt, settings).map_err(temporal_err)?;

            // Create a Duration object via Temporal.Duration constructor
            let global = ncx.global();
            let dur_ctor = global
                .get(&PropertyKey::string("Temporal"))
                .and_then(|v| v.as_object())
                .and_then(|t| t.get(&PropertyKey::string("Duration")))
                .ok_or_else(|| VmError::type_error("Temporal.Duration not found"))?;

            ncx.call_function_construct(
                &dur_ctor,
                Value::undefined(),
                &[
                    Value::number(duration.years() as f64),
                    Value::number(duration.months() as f64),
                    Value::number(duration.weeks() as f64),
                    Value::number(duration.days() as f64),
                    Value::number(duration.hours() as f64),
                    Value::number(duration.minutes() as f64),
                    Value::number(duration.seconds() as f64),
                    Value::number(duration.milliseconds() as f64),
                    Value::number(duration.microseconds() as f64),
                    Value::number(duration.nanoseconds() as f64),
                ],
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "since",
        1,
    );
    proto.define_property(
        PropertyKey::string("since"),
        PropertyDescriptor::builtin_method(since_fn),
    );

    // .until(other, options) method
    let until_fn = Value::native_function_with_proto_named(
        move |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("until called on non-object"))?;
            let this_pdt = extract_plain_date_time(&obj)?;

            let other_arg = args.first().cloned().unwrap_or(Value::undefined());
            let other_pdt = to_temporal_datetime(ncx, &other_arg)?;

            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let settings = parse_difference_settings(ncx, &options_val)?;

            let duration = this_pdt.until(&other_pdt, settings).map_err(temporal_err)?;

            let global = ncx.global();
            let dur_ctor = global
                .get(&PropertyKey::string("Temporal"))
                .and_then(|v| v.as_object())
                .and_then(|t| t.get(&PropertyKey::string("Duration")))
                .ok_or_else(|| VmError::type_error("Temporal.Duration not found"))?;

            ncx.call_function_construct(
                &dur_ctor,
                Value::undefined(),
                &[
                    Value::number(duration.years() as f64),
                    Value::number(duration.months() as f64),
                    Value::number(duration.weeks() as f64),
                    Value::number(duration.days() as f64),
                    Value::number(duration.hours() as f64),
                    Value::number(duration.minutes() as f64),
                    Value::number(duration.seconds() as f64),
                    Value::number(duration.milliseconds() as f64),
                    Value::number(duration.microseconds() as f64),
                    Value::number(duration.nanoseconds() as f64),
                ],
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "until",
        1,
    );
    proto.define_property(
        PropertyKey::string("until"),
        PropertyDescriptor::builtin_method(until_fn),
    );

    // .add(duration, options) method
    let add_fn = Value::native_function_with_proto_named(
        move |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("add called on non-object"))?;
            let this_pdt = extract_plain_date_time(&obj)?;

            let dur_arg = args.first().cloned().unwrap_or(Value::undefined());
            let duration = to_temporal_duration(ncx, &dur_arg)?;

            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = parse_overflow_option(ncx, &options_val)?;

            let result = this_pdt
                .add(&duration, Some(overflow))
                .map_err(temporal_err)?;

            let global = ncx.global();
            let pdt_ctor = global
                .get(&PropertyKey::string("Temporal"))
                .and_then(|v| v.as_object())
                .and_then(|t| t.get(&PropertyKey::string("PlainDateTime")))
                .ok_or_else(|| VmError::type_error("Temporal.PlainDateTime not found"))?;
            ncx.call_function_construct(
                &pdt_ctor,
                Value::undefined(),
                &[
                    Value::int32(result.year()),
                    Value::int32(result.month() as i32),
                    Value::int32(result.day() as i32),
                    Value::int32(result.hour() as i32),
                    Value::int32(result.minute() as i32),
                    Value::int32(result.second() as i32),
                    Value::int32(result.millisecond() as i32),
                    Value::int32(result.microsecond() as i32),
                    Value::int32(result.nanosecond() as i32),
                ],
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "add",
        1,
    );
    proto.define_property(
        PropertyKey::string("add"),
        PropertyDescriptor::builtin_method(add_fn),
    );

    // .subtract(duration, options) method
    let subtract_fn = Value::native_function_with_proto_named(
        move |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("subtract called on non-object"))?;
            let this_pdt = extract_plain_date_time(&obj)?;

            let dur_arg = args.first().cloned().unwrap_or(Value::undefined());
            let duration = to_temporal_duration(ncx, &dur_arg)?;

            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = parse_overflow_option(ncx, &options_val)?;

            let result = this_pdt
                .subtract(&duration, Some(overflow))
                .map_err(temporal_err)?;

            let global = ncx.global();
            let pdt_ctor = global
                .get(&PropertyKey::string("Temporal"))
                .and_then(|v| v.as_object())
                .and_then(|t| t.get(&PropertyKey::string("PlainDateTime")))
                .ok_or_else(|| VmError::type_error("Temporal.PlainDateTime not found"))?;
            ncx.call_function_construct(
                &pdt_ctor,
                Value::undefined(),
                &[
                    Value::int32(result.year()),
                    Value::int32(result.month() as i32),
                    Value::int32(result.day() as i32),
                    Value::int32(result.hour() as i32),
                    Value::int32(result.minute() as i32),
                    Value::int32(result.second() as i32),
                    Value::int32(result.millisecond() as i32),
                    Value::int32(result.microsecond() as i32),
                    Value::int32(result.nanosecond() as i32),
                ],
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "subtract",
        1,
    );
    proto.define_property(
        PropertyKey::string("subtract"),
        PropertyDescriptor::builtin_method(subtract_fn),
    );

    // toPlainTime — extract time portion as Temporal.PlainTime
    let to_plain_time_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toPlainTime called on non-object"))?;
            let pdt = extract_plain_date_time(&obj)?;
            let global = ncx.global();
            let pt_ctor = global
                .get(&PropertyKey::string("Temporal"))
                .and_then(|v| v.as_object())
                .and_then(|t| t.get(&PropertyKey::string("PlainTime")))
                .ok_or_else(|| VmError::type_error("Temporal.PlainTime not found"))?;
            ncx.call_function_construct(
                &pt_ctor,
                Value::undefined(),
                &[
                    Value::int32(pdt.hour() as i32),
                    Value::int32(pdt.minute() as i32),
                    Value::int32(pdt.second() as i32),
                    Value::int32(pdt.millisecond() as i32),
                    Value::int32(pdt.microsecond() as i32),
                    Value::int32(pdt.nanosecond() as i32),
                ],
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "toPlainTime",
        0,
    );
    proto.define_property(
        PropertyKey::string("toPlainTime"),
        PropertyDescriptor::builtin_method(to_plain_time_fn),
    );

    // withPlainTime — replace time portion, keep date
    let with_plain_time_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("withPlainTime called on non-object"))?;
            let this_pdt = extract_plain_date_time(&obj)?;

            // Get time from argument (default to midnight if undefined/absent)
            let time_arg = args.first().cloned().unwrap_or(Value::undefined());
            let (h, mi, sec, ms, us, ns) = if time_arg.is_undefined() {
                (0i32, 0i32, 0i32, 0i32, 0i32, 0i32)
            } else {
                // Convert to PlainTime first via Temporal.PlainTime.from(), then extract via TemporalValue
                let global = ncx.global();
                let pt_ctor = global
                    .get(&PropertyKey::string("Temporal"))
                    .and_then(|v| v.as_object())
                    .and_then(|t| t.get(&PropertyKey::string("PlainTime")))
                    .ok_or_else(|| VmError::type_error("Temporal.PlainTime not found"))?;
                let pt_from = pt_ctor
                    .as_object()
                    .ok_or_else(|| VmError::type_error("Temporal.PlainTime not found"))?
                    .get(&PropertyKey::string("from"))
                    .ok_or_else(|| VmError::type_error("Temporal.PlainTime.from not found"))?;
                let pt = ncx.call_function(&pt_from, pt_ctor, &[time_arg])?;
                let pt_obj = pt.as_object().ok_or_else(|| {
                    VmError::type_error("PlainTime.from did not return an object")
                })?;
                let pt_val = extract_plain_time(&pt_obj)?;
                (
                    pt_val.hour() as i32,
                    pt_val.minute() as i32,
                    pt_val.second() as i32,
                    pt_val.millisecond() as i32,
                    pt_val.microsecond() as i32,
                    pt_val.nanosecond() as i32,
                )
            };

            let global = ncx.global();
            let pdt_ctor = global
                .get(&PropertyKey::string("Temporal"))
                .and_then(|v| v.as_object())
                .and_then(|t| t.get(&PropertyKey::string("PlainDateTime")))
                .ok_or_else(|| VmError::type_error("Temporal.PlainDateTime not found"))?;
            ncx.call_function_construct(
                &pdt_ctor,
                Value::undefined(),
                &[
                    Value::int32(this_pdt.year()),
                    Value::int32(this_pdt.month() as i32),
                    Value::int32(this_pdt.day() as i32),
                    Value::int32(h),
                    Value::int32(mi),
                    Value::int32(sec),
                    Value::int32(ms),
                    Value::int32(us),
                    Value::int32(ns),
                ],
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "withPlainTime",
        0,
    );
    proto.define_property(
        PropertyKey::string("withPlainTime"),
        PropertyDescriptor::builtin_method(with_plain_time_fn),
    );

    // .withCalendar(calendar) method
    let withcal_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("withCalendar called on non-object"))?;
            let this_pdt = extract_plain_date_time(&obj)?;

            let cal_arg = args.first().cloned().unwrap_or(Value::undefined());
            if cal_arg.is_undefined() {
                return Err(VmError::type_error("missing calendar argument"));
            }
            // ToTemporalCalendarIdentifier
            validate_calendar_arg(ncx, &cal_arg)?;

            // Return new PlainDateTime with same ISO fields
            let global = ncx.global();
            let pdt_ctor = global
                .get(&PropertyKey::string("Temporal"))
                .and_then(|v| v.as_object())
                .and_then(|t| t.get(&PropertyKey::string("PlainDateTime")))
                .ok_or_else(|| VmError::type_error("Temporal.PlainDateTime not found"))?;
            ncx.call_function_construct(
                &pdt_ctor,
                Value::undefined(),
                &[
                    Value::int32(this_pdt.year()),
                    Value::int32(this_pdt.month() as i32),
                    Value::int32(this_pdt.day() as i32),
                    Value::int32(this_pdt.hour() as i32),
                    Value::int32(this_pdt.minute() as i32),
                    Value::int32(this_pdt.second() as i32),
                    Value::int32(this_pdt.millisecond() as i32),
                    Value::int32(this_pdt.microsecond() as i32),
                    Value::int32(this_pdt.nanosecond() as i32),
                ],
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "withCalendar",
        1,
    );
    proto.define_property(
        PropertyKey::string("withCalendar"),
        PropertyDescriptor::builtin_method(withcal_fn),
    );

    // round(roundTo)
    let round_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("round called on non-object"))?;
            // Branding check via extract (will be called again below for the actual computation)
            let _ = extract_plain_date_time(&obj)?;

            let round_to = args.first().cloned().unwrap_or(Value::undefined());
            if round_to.is_undefined() {
                return Err(VmError::type_error("options parameter is required"));
            }

            // String shorthand: round("minute") etc.
            let (smallest_unit, rounding_mode, increment) = if round_to.is_string() {
                let s = ncx.to_string_value(&round_to)?;
                let u = parse_temporal_unit(&s)?;
                match u {
                    temporal_rs::options::Unit::Year
                    | temporal_rs::options::Unit::Month
                    | temporal_rs::options::Unit::Week
                    | temporal_rs::options::Unit::Auto => {
                        return Err(VmError::range_error(format!(
                            "{} is not a valid value for smallest unit",
                            s
                        )));
                    }
                    _ => {}
                }
                (Some(u), None, None)
            } else if !round_to.is_object() && round_to.as_proxy().is_none() {
                return Err(VmError::type_error("options must be a string or object"));
            } else {
                // Read options in alphabetical order
                let ri_val = get_val_property(ncx, &round_to, "roundingIncrement")?;
                let ri = if !ri_val.is_undefined() {
                    let n = ncx.to_number_value(&ri_val)?;
                    Some(
                        temporal_rs::options::RoundingIncrement::try_from(n)
                            .map_err(temporal_err)?,
                    )
                } else {
                    None
                };

                let rm_val = get_val_property(ncx, &round_to, "roundingMode")?;
                let rm = if !rm_val.is_undefined() {
                    let s = ncx.to_string_value(&rm_val)?;
                    Some(parse_rounding_mode(&s)?)
                } else {
                    None
                };

                let su_val = get_val_property(ncx, &round_to, "smallestUnit")?;
                let su = if !su_val.is_undefined() {
                    let s = ncx.to_string_value(&su_val)?;
                    let u = parse_temporal_unit(&s)?;
                    match u {
                        temporal_rs::options::Unit::Year
                        | temporal_rs::options::Unit::Month
                        | temporal_rs::options::Unit::Week
                        | temporal_rs::options::Unit::Auto => {
                            return Err(VmError::range_error(format!(
                                "{} is not a valid value for smallest unit",
                                s
                            )));
                        }
                        _ => {}
                    }
                    Some(u)
                } else {
                    None
                };

                if su.is_none() {
                    return Err(VmError::range_error("smallestUnit is required"));
                }

                (su, rm, ri)
            };

            let pdt = extract_plain_date_time(&obj)?;
            let mut opts = temporal_rs::options::RoundingOptions::default();
            opts.largest_unit = None;
            opts.smallest_unit = smallest_unit;
            opts.rounding_mode = rounding_mode;
            opts.increment = increment;
            let rounded = pdt.round(opts).map_err(temporal_err)?;

            // Construct result via constructor
            let global = ncx.global();
            let pdt_ctor = global
                .get(&PropertyKey::string("Temporal"))
                .and_then(|v| v.as_object())
                .and_then(|t| t.get(&PropertyKey::string("PlainDateTime")))
                .ok_or_else(|| VmError::type_error("Temporal.PlainDateTime not found"))?;

            ncx.call_function_construct(
                &pdt_ctor,
                Value::undefined(),
                &[
                    Value::int32(rounded.iso_year()),
                    Value::int32(rounded.iso_month() as i32),
                    Value::int32(rounded.iso_day() as i32),
                    Value::int32(rounded.hour() as i32),
                    Value::int32(rounded.minute() as i32),
                    Value::int32(rounded.second() as i32),
                    Value::int32(rounded.millisecond() as i32),
                    Value::int32(rounded.microsecond() as i32),
                    Value::int32(rounded.nanosecond() as i32),
                ],
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "round",
        1,
    );
    proto.define_property(
        PropertyKey::string("round"),
        PropertyDescriptor::builtin_method(round_fn),
    );

    // @@toStringTag
    proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Temporal.PlainDateTime")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
    );
}
