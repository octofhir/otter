use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
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
    // Getter with branding check: ensures SLOT_TEMPORAL_TYPE == "PlainDateTime"
    let make_branding_getter = |slot: &'static str, name: &'static str, mm: &Arc<MemoryManager>, fn_proto: &GcRef<JsObject>| -> Value {
        Value::native_function_with_proto(
            move |this, _args, _ncx| {
                let obj = this.as_object().ok_or_else(|| {
                    VmError::type_error(&format!("{} called on non-object", name))
                })?;
                // Branding check
                let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                if ty.as_deref() != Some("PlainDateTime") {
                    return Err(VmError::type_error(&format!("{} called on non-PlainDateTime", name)));
                }
                obj.get(&PropertyKey::string(slot))
                    .filter(|v| !v.is_undefined())
                    .ok_or_else(|| VmError::type_error(&format!("{} called on non-PlainDateTime", name)))
            },
            mm.clone(),
            fn_proto.clone(),
        )
    };

    // Time getter with branding: checks brand, defaults to 0 if time slot missing
    let make_time_getter = |slot: &'static str, name: &'static str, mm: &Arc<MemoryManager>, fn_proto: &GcRef<JsObject>| -> Value {
        Value::native_function_with_proto(
            move |this, _args, _ncx| {
                let obj = this.as_object().ok_or_else(|| {
                    VmError::type_error(&format!("{} called on non-object", name))
                })?;
                // Branding check
                let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                if ty.as_deref() != Some("PlainDateTime") {
                    return Err(VmError::type_error(&format!("{} called on non-PlainDateTime", name)));
                }
                Ok(obj.get(&PropertyKey::string(slot))
                    .filter(|v| !v.is_undefined())
                    .unwrap_or(Value::int32(0)))
            },
            mm.clone(),
            fn_proto.clone(),
        )
    };

    // Date getters (branded)
    for (slot, name) in &[
        (SLOT_ISO_YEAR, "year"),
        (SLOT_ISO_MONTH, "month"),
        (SLOT_ISO_DAY, "day"),
    ] {
        proto.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::Accessor {
                get: Some(make_branding_getter(slot, name, mm, &fn_proto)),
                set: None,
                attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
            },
        );
    }

    // Time getters (branded, default to 0)
    for (slot, name) in &[
        (SLOT_ISO_HOUR, "hour"),
        (SLOT_ISO_MINUTE, "minute"),
        (SLOT_ISO_SECOND, "second"),
        (SLOT_ISO_MILLISECOND, "millisecond"),
        (SLOT_ISO_MICROSECOND, "microsecond"),
        (SLOT_ISO_NANOSECOND, "nanosecond"),
    ] {
        proto.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::Accessor {
                get: Some(make_time_getter(slot, name, mm, &fn_proto)),
                set: None,
                attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
            },
        );
    }

    // monthCode
    proto.define_property(
        PropertyKey::string("monthCode"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("monthCode"))?;
                    let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32())
                        .ok_or_else(|| VmError::type_error("monthCode on non-PlainDateTime"))?;
                    Ok(Value::string(JsString::intern(&format_month_code(m as u32))))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    // calendarId
    proto.define_property(
        PropertyKey::string("calendarId"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("calendarId"))?;
                    let _ = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32())
                        .ok_or_else(|| VmError::type_error("calendarId on non-PlainDateTime"))?;
                    Ok(Value::string(JsString::intern("iso8601")))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
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
                        let _ = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32())
                            .ok_or_else(|| VmError::type_error(&format!("{} on non-PlainDateTime", n)))?;
                        Ok(Value::undefined())
                    },
                    mm.clone(),
                    fn_proto.clone(),
                )),
                set: None,
                attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
            },
        );
    }

    // dayOfWeek, dayOfYear, daysInWeek, daysInMonth, daysInYear, monthsInYear, inLeapYear — via temporal_rs::PlainDate
    for (prop, getter_fn) in &[
        ("dayOfWeek", (|pd: &temporal_rs::PlainDate| pd.day_of_week() as i32) as fn(&temporal_rs::PlainDate) -> i32),
        ("dayOfYear", |pd: &temporal_rs::PlainDate| pd.day_of_year() as i32),
        ("daysInWeek", |pd: &temporal_rs::PlainDate| pd.days_in_week() as i32),
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
                    mm.clone(), fn_proto.clone(),
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
                mm.clone(), fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    // toString
    let to_string_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toString"))?;
            let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("toString"))?;
            let mo = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            let h = obj.get(&PropertyKey::string(SLOT_ISO_HOUR)).and_then(|v| v.as_int32()).unwrap_or(0);
            let mi = obj.get(&PropertyKey::string(SLOT_ISO_MINUTE)).and_then(|v| v.as_int32()).unwrap_or(0);
            let s = obj.get(&PropertyKey::string(SLOT_ISO_SECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let ms = obj.get(&PropertyKey::string(SLOT_ISO_MILLISECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let us = obj.get(&PropertyKey::string(SLOT_ISO_MICROSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let ns = obj.get(&PropertyKey::string(SLOT_ISO_NANOSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);

            let date_part = if y < 0 || y > 9999 {
                format!("{:+07}-{:02}-{:02}", y, mo, d)
            } else {
                format!("{:04}-{:02}-{:02}", y, mo, d)
            };

            let sub = ns + us * 1000 + ms * 1_000_000;
            let time_part = if sub != 0 {
                let frac = format!("{:09}", sub);
                let trimmed = frac.trim_end_matches('0');
                format!("T{:02}:{:02}:{:02}.{}", h, mi, s, trimmed)
            } else if s != 0 {
                format!("T{:02}:{:02}:{:02}", h, mi, s)
            } else if mi != 0 || h != 0 {
                format!("T{:02}:{:02}:{:02}", h, mi, s)
            } else {
                "T00:00:00".to_string()
            };

            Ok(Value::string(JsString::intern(&format!("{}{}", date_part, time_part))))
        },
        mm.clone(),
        fn_proto.clone(),
        "toString",
        0,
    );
    proto.define_property(PropertyKey::string("toString"), PropertyDescriptor::builtin_method(to_string_fn));

    // toJSON
    let to_json_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toJSON called on non-PlainDateTime"))?;
            // Branding check
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainDateTime") {
                return Err(VmError::type_error("toJSON called on non-PlainDateTime"));
            }
            // Delegate to toString
            if let Some(ts) = obj.get(&PropertyKey::string("toString")) {
                return ncx.call_function(&ts, this.clone(), &[]);
            }
            Err(VmError::type_error("toJSON called on non-PlainDateTime"))
        },
        mm.clone(), fn_proto.clone(), "toJSON", 0,
    );
    proto.define_property(PropertyKey::string("toJSON"), PropertyDescriptor::builtin_method(to_json_fn));

    // valueOf — throws
    let value_of_fn = Value::native_function_with_proto_named(
        |_this, _args, _ncx| {
            Err(VmError::type_error("use compare() or toString() to compare Temporal.PlainDateTime"))
        },
        mm.clone(), fn_proto.clone(), "valueOf", 0,
    );
    proto.define_property(PropertyKey::string("valueOf"), PropertyDescriptor::builtin_method(value_of_fn));

    // toLocaleString
    let to_locale_string_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            if let Some(obj) = this.as_object() {
                if let Some(ts) = obj.get(&PropertyKey::string("toString")) {
                    return ncx.call_function(&ts, this.clone(), &[]);
                }
            }
            Err(VmError::type_error("toLocaleString"))
        },
        mm.clone(), fn_proto.clone(), "toLocaleString", 0,
    );
    proto.define_property(PropertyKey::string("toLocaleString"), PropertyDescriptor::builtin_method(to_locale_string_fn));

    // toPlainDate
    let to_plain_date_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toPlainDate"))?;
            let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("toPlainDate"))?;
            let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            let temporal_ns = ncx.ctx.get_global("Temporal")
                .ok_or_else(|| VmError::type_error("Temporal not found"))?;
            let temporal_obj = temporal_ns.as_object()
                .ok_or_else(|| VmError::type_error("Temporal not found"))?;
            let pd_ctor = temporal_obj.get(&PropertyKey::string("PlainDate")).ok_or_else(|| VmError::type_error("PlainDate not found"))?;
            ncx.call_function_construct(&pd_ctor, Value::undefined(), &[Value::int32(y), Value::int32(m), Value::int32(d)])
        },
        mm.clone(), fn_proto.clone(), "toPlainDate", 0,
    );
    proto.define_property(PropertyKey::string("toPlainDate"), PropertyDescriptor::builtin_method(to_plain_date_fn));

    // with — PlainDateTime.prototype.with(temporalDateTimeLike [, options])
    let with_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("with called on non-PlainDateTime"))?;
            // Branding check
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainDateTime") {
                return Err(VmError::type_error("with called on non-PlainDateTime"));
            }

            // Get current values
            let cur_y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
            let cur_m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let cur_d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            let cur_h = obj.get(&PropertyKey::string(SLOT_ISO_HOUR)).and_then(|v| v.as_int32()).unwrap_or(0);
            let cur_mi = obj.get(&PropertyKey::string(SLOT_ISO_MINUTE)).and_then(|v| v.as_int32()).unwrap_or(0);
            let cur_s = obj.get(&PropertyKey::string(SLOT_ISO_SECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let cur_ms = obj.get(&PropertyKey::string(SLOT_ISO_MILLISECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let cur_us = obj.get(&PropertyKey::string(SLOT_ISO_MICROSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let cur_ns = obj.get(&PropertyKey::string(SLOT_ISO_NANOSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);

            let item = args.first().cloned().unwrap_or(Value::undefined());

            // Helper: get property from object or proxy
            let get_prop = |ncx: &mut NativeContext<'_>, item: &Value, name: &str| -> Result<Value, VmError> {
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
                if n.is_infinite() { return Err(VmError::range_error("day property cannot be Infinity")); }
                n as i32
            } else { cur_d };

            let hour_raw = get_prop(ncx, &item, "hour")?;
            let hour = if !hour_raw.is_undefined() {
                let n = ncx.to_number_value(&hour_raw)?;
                if n.is_infinite() { return Err(VmError::range_error("hour property cannot be Infinity")); }
                n as i32
            } else { cur_h };

            let microsecond_raw = get_prop(ncx, &item, "microsecond")?;
            let microsecond = if !microsecond_raw.is_undefined() {
                let n = ncx.to_number_value(&microsecond_raw)?;
                if n.is_infinite() { return Err(VmError::range_error("microsecond property cannot be Infinity")); }
                n as i32
            } else { cur_us };

            let millisecond_raw = get_prop(ncx, &item, "millisecond")?;
            let millisecond = if !millisecond_raw.is_undefined() {
                let n = ncx.to_number_value(&millisecond_raw)?;
                if n.is_infinite() { return Err(VmError::range_error("millisecond property cannot be Infinity")); }
                n as i32
            } else { cur_ms };

            let minute_raw = get_prop(ncx, &item, "minute")?;
            let minute = if !minute_raw.is_undefined() {
                let n = ncx.to_number_value(&minute_raw)?;
                if n.is_infinite() { return Err(VmError::range_error("minute property cannot be Infinity")); }
                n as i32
            } else { cur_mi };

            let month_raw = get_prop(ncx, &item, "month")?;
            let month_n = if !month_raw.is_undefined() {
                let n = ncx.to_number_value(&month_raw)?;
                if n.is_infinite() { return Err(VmError::range_error("month property cannot be Infinity")); }
                Some(n as i32)
            } else { None };

            let month_code_raw = get_prop(ncx, &item, "monthCode")?;
            // Only read and convert monthCode here; validation happens AFTER options
            let mc_str = if !month_code_raw.is_undefined() {
                Some(ncx.to_string_value(&month_code_raw)?)
            } else { None };
            // Temporary month for basic below-min validation (monthCode validation deferred)
            let month_pre = if let Some(mn) = month_n { mn } else { cur_m };

            let nanosecond_raw = get_prop(ncx, &item, "nanosecond")?;
            let nanosecond = if !nanosecond_raw.is_undefined() {
                let n = ncx.to_number_value(&nanosecond_raw)?;
                if n.is_infinite() { return Err(VmError::range_error("nanosecond property cannot be Infinity")); }
                n as i32
            } else { cur_ns };

            let second_raw = get_prop(ncx, &item, "second")?;
            let second = if !second_raw.is_undefined() {
                let n = ncx.to_number_value(&second_raw)?;
                if n.is_infinite() { return Err(VmError::range_error("second property cannot be Infinity")); }
                n as i32
            } else { cur_s };

            let year_raw = get_prop(ncx, &item, "year")?;
            let year = if !year_raw.is_undefined() {
                let n = ncx.to_number_value(&year_raw)?;
                if n.is_infinite() { return Err(VmError::range_error("year property cannot be Infinity")); }
                n as i32
            } else { cur_y };

            // Check at least one field is defined
            let has_any = [&day_raw, &hour_raw, &microsecond_raw, &millisecond_raw, &minute_raw,
                &month_raw, &month_code_raw, &nanosecond_raw, &second_raw, &year_raw]
                .iter().any(|v| !v.is_undefined());
            if !has_any {
                return Err(VmError::type_error("with argument must have at least one recognized temporal property"));
            }

            // CalendarResolveFields: reject below-minimum values BEFORE options
            // (above-maximum values are handled by overflow constrain/reject after options)
            if month_pre < 1 { return Err(VmError::range_error(format!("month must be >= 1, got {}", month_pre))); }
            if day < 1 { return Err(VmError::range_error(format!("day must be >= 1, got {}", day))); }
            if hour < 0 { return Err(VmError::range_error(format!("hour must be >= 0, got {}", hour))); }
            if minute < 0 { return Err(VmError::range_error(format!("minute must be >= 0, got {}", minute))); }
            if second < 0 { return Err(VmError::range_error(format!("second must be >= 0, got {}", second))); }
            if millisecond < 0 { return Err(VmError::range_error(format!("millisecond must be >= 0, got {}", millisecond))); }
            if microsecond < 0 { return Err(VmError::range_error(format!("microsecond must be >= 0, got {}", microsecond))); }
            if nanosecond < 0 { return Err(VmError::range_error(format!("nanosecond must be >= 0, got {}", nanosecond))); }

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
            } else { month_pre };

            // Use temporal_rs for full validation including calendar-specific checks
            let ov = overflow;
            let pdt = temporal_rs::PlainDateTime::new_with_overflow(
                year, month as u8, day as u8,
                hour.clamp(0, 255) as u8, minute.clamp(0, 255) as u8, second.clamp(0, 255) as u8,
                millisecond.clamp(0, 65535) as u16, microsecond.clamp(0, 65535) as u16, nanosecond.clamp(0, 65535) as u16,
                temporal_rs::Calendar::default(), ov,
            ).map_err(temporal_err)?;

            // Subclassing ignored — always use Temporal.PlainDateTime constructor prototype
            let temporal_ns = ncx.ctx.get_global("Temporal")
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let temporal_obj_ns = temporal_ns.as_object()
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let pdt_ctor = temporal_obj_ns.get(&PropertyKey::string("PlainDateTime"))
                .ok_or_else(|| VmError::type_error("PlainDateTime constructor not found"))?;
            let pdt_ctor_obj = pdt_ctor.as_object()
                .ok_or_else(|| VmError::type_error("PlainDateTime is not a function"))?;
            let pdt_proto = pdt_ctor_obj.get(&PropertyKey::string("prototype"))
                .unwrap_or(Value::undefined());

            let result_obj = GcRef::new(JsObject::new(
                pdt_proto,
                ncx.ctx.memory_manager().clone(),
            ));
            result_obj.define_property(PropertyKey::string(SLOT_TEMPORAL_TYPE), PropertyDescriptor::data(Value::string(JsString::intern("PlainDateTime"))));
            result_obj.define_property(PropertyKey::string(SLOT_ISO_YEAR), PropertyDescriptor::data(Value::int32(pdt.year())));
            result_obj.define_property(PropertyKey::string(SLOT_ISO_MONTH), PropertyDescriptor::data(Value::int32(pdt.month() as i32)));
            result_obj.define_property(PropertyKey::string(SLOT_ISO_DAY), PropertyDescriptor::data(Value::int32(pdt.day() as i32)));
            result_obj.define_property(PropertyKey::string(SLOT_ISO_HOUR), PropertyDescriptor::data(Value::int32(pdt.hour() as i32)));
            result_obj.define_property(PropertyKey::string(SLOT_ISO_MINUTE), PropertyDescriptor::data(Value::int32(pdt.minute() as i32)));
            result_obj.define_property(PropertyKey::string(SLOT_ISO_SECOND), PropertyDescriptor::data(Value::int32(pdt.second() as i32)));
            result_obj.define_property(PropertyKey::string(SLOT_ISO_MILLISECOND), PropertyDescriptor::data(Value::int32(pdt.millisecond() as i32)));
            result_obj.define_property(PropertyKey::string(SLOT_ISO_MICROSECOND), PropertyDescriptor::data(Value::int32(pdt.microsecond() as i32)));
            result_obj.define_property(PropertyKey::string(SLOT_ISO_NANOSECOND), PropertyDescriptor::data(Value::int32(pdt.nanosecond() as i32)));
            Ok(Value::object(result_obj))
        },
        mm.clone(), fn_proto.clone(), "with", 1,
    );
    proto.define_property(PropertyKey::string("with"), PropertyDescriptor::builtin_method(with_fn));

    // toZonedDateTime — implementation for UTC and fixed-offset timezones
    let to_zoned_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toZonedDateTime called on non-PlainDateTime"))?;
            // Branding check
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainDateTime") {
                return Err(VmError::type_error("toZonedDateTime called on non-PlainDateTime"));
            }

            // Get timeZone argument
            let tz_arg = args.first().cloned().unwrap_or(Value::undefined());

            // Type check: primitives → TypeError (except string)
            if tz_arg.is_undefined() || tz_arg.is_null() || tz_arg.is_boolean()
                || tz_arg.is_number() || tz_arg.is_bigint() {
                return Err(VmError::type_error(format!(
                    "{} is not a valid time zone",
                    if tz_arg.is_null() { "null" } else if tz_arg.is_undefined() { "undefined" } else { tz_arg.type_of() }
                )));
            }
            if tz_arg.as_symbol().is_some() {
                return Err(VmError::type_error("Cannot convert a Symbol value to a string"));
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

            // Use temporal_rs for spec-compliant timezone parsing
            // This handles: "UTC", "+01:00", "2021-08-19T17:30Z", "2021-08-19T17:30-07:00[+01:46]", etc.
            let tz = temporal_rs::TimeZone::try_from_str(tz_s).map_err(temporal_err)?;
            let tz_identifier = tz.identifier().map_err(temporal_err)?;

            // Read disambiguation option
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let disambiguation_str = if !options_val.is_undefined() {
                if options_val.is_null() || options_val.is_boolean() || options_val.is_number()
                    || options_val.is_string() || options_val.is_bigint() || options_val.as_symbol().is_some() {
                    return Err(VmError::type_error("options must be an object or undefined"));
                }
                if let Some(opts_obj) = options_val.as_object() {
                    let dis_val = ncx.get_property(&opts_obj, &PropertyKey::string("disambiguation"))?;
                    if !dis_val.is_undefined() {
                        let dis_str = ncx.to_string_value(&dis_val)?;
                        match dis_str.as_str() {
                            "compatible" | "earlier" | "later" | "reject" => dis_str.as_str().to_string(),
                            _ => return Err(VmError::range_error(format!("{} is not a valid value for disambiguation", dis_str))),
                        }
                    } else { "compatible".to_string() }
                } else if let Some(proxy) = options_val.as_proxy() {
                    let key = PropertyKey::string("disambiguation");
                    let key_value = crate::proxy_operations::property_key_to_value_pub(&key);
                    let dis_val = crate::proxy_operations::proxy_get(ncx, proxy, &key, key_value, options_val.clone())?;
                    if !dis_val.is_undefined() {
                        let dis_str = ncx.to_string_value(&dis_val)?;
                        match dis_str.as_str() {
                            "compatible" | "earlier" | "later" | "reject" => dis_str.as_str().to_string(),
                            _ => return Err(VmError::range_error(format!("{} is not a valid value for disambiguation", dis_str))),
                        }
                    } else { "compatible".to_string() }
                } else { "compatible".to_string() }
            } else { "compatible".to_string() };

            // Get the PlainDateTime components
            let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
            let mo = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            let h = obj.get(&PropertyKey::string(SLOT_ISO_HOUR)).and_then(|v| v.as_int32()).unwrap_or(0);
            let mi = obj.get(&PropertyKey::string(SLOT_ISO_MINUTE)).and_then(|v| v.as_int32()).unwrap_or(0);
            let s = obj.get(&PropertyKey::string(SLOT_ISO_SECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let ms_val = obj.get(&PropertyKey::string(SLOT_ISO_MILLISECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let us_val = obj.get(&PropertyKey::string(SLOT_ISO_MICROSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let ns_val = obj.get(&PropertyKey::string(SLOT_ISO_NANOSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);

            // Compute epoch nanoseconds from ISO date/time components
            // Epoch is 1970-01-01T00:00:00Z
            let days_from_epoch = iso_date_to_epoch_days(y, mo, d);
            let time_ns = (h as i128) * 3_600_000_000_000
                + (mi as i128) * 60_000_000_000
                + (s as i128) * 1_000_000_000
                + (ms_val as i128) * 1_000_000
                + (us_val as i128) * 1_000
                + (ns_val as i128);
            let local_epoch_ns = (days_from_epoch as i128) * 86_400_000_000_000 + time_ns;

            // Parse offset from timezone identifier
            let offset_ns = parse_tz_offset_ns(&tz_identifier)?;

            // For fixed-offset/UTC timezones, epoch_ns = local_epoch_ns - offset_ns
            let epoch_ns = local_epoch_ns - offset_ns;

            // Validate Instant range: ±10^8 days = ±8.64 × 10^21 nanoseconds
            let max_instant_ns: i128 = 8_640_000_000_000_000_000_000;
            if epoch_ns < -max_instant_ns || epoch_ns > max_instant_ns {
                return Err(VmError::range_error("resulting Instant is outside the allowed range"));
            }

            let epoch_ns_str = epoch_ns.to_string();

            // Create a ZonedDateTime object with proper prototype
            let temporal_ns_val = ncx.ctx.get_global("Temporal")
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let temporal_obj = temporal_ns_val.as_object()
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;

            let zdt_proto = temporal_obj.get(&PropertyKey::string("ZonedDateTime"))
                .and_then(|ctor| ctor.as_object())
                .and_then(|ctor_obj| ctor_obj.get(&PropertyKey::string("prototype")))
                .unwrap_or(Value::undefined());

            let result = GcRef::new(JsObject::new(zdt_proto, ncx.ctx.memory_manager().clone()));
            result.define_property(PropertyKey::string(SLOT_TEMPORAL_TYPE),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern("ZonedDateTime"))));
            result.define_property(PropertyKey::string("epochNanoseconds"),
                PropertyDescriptor::data(Value::bigint(epoch_ns_str.clone())));
            result.define_property(PropertyKey::string("calendarId"),
                PropertyDescriptor::data(Value::string(JsString::intern("iso8601"))));
            result.define_property(PropertyKey::string("timeZoneId"),
                PropertyDescriptor::data(Value::string(JsString::intern(&tz_identifier))));
            // Store the offset for this timezone
            result.define_property(PropertyKey::string("__tz_offset_ns__"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern(&offset_ns.to_string()))));
            Ok(Value::object(result))
        },
        mm.clone(), fn_proto.clone(), "toZonedDateTime", 1,
    );
    proto.define_property(PropertyKey::string("toZonedDateTime"), PropertyDescriptor::builtin_method(to_zoned_fn));

    // .equals(other) method
    let equals_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("equals called on non-object"))?;
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainDateTime") {
                return Err(VmError::type_error("equals called on non-PlainDateTime"));
            }
            let this_pdt = extract_pdt(&obj)?;

            let other = args.first().cloned().unwrap_or(Value::undefined());
            let other_pdt = to_temporal_datetime(ncx, &other)?;

            Ok(Value::boolean(this_pdt.compare_iso(&other_pdt) == std::cmp::Ordering::Equal))
        },
        mm.clone(), fn_proto.clone(), "equals", 1,
    );
    proto.define_property(PropertyKey::string("equals"), PropertyDescriptor::builtin_method(equals_fn));

    // Helper: extract temporal_rs::PlainDateTime from a JsObject with ISO slots
    fn extract_pdt(obj: &GcRef<JsObject>) -> Result<temporal_rs::PlainDateTime, VmError> {
        let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
        let mo = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1) as u8;
        let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1) as u8;
        let h = obj.get(&PropertyKey::string(SLOT_ISO_HOUR)).and_then(|v| v.as_int32()).unwrap_or(0) as u8;
        let mi = obj.get(&PropertyKey::string(SLOT_ISO_MINUTE)).and_then(|v| v.as_int32()).unwrap_or(0) as u8;
        let sec = obj.get(&PropertyKey::string(SLOT_ISO_SECOND)).and_then(|v| v.as_int32()).unwrap_or(0) as u8;
        let ms = obj.get(&PropertyKey::string(SLOT_ISO_MILLISECOND)).and_then(|v| v.as_int32()).unwrap_or(0) as u16;
        let us = obj.get(&PropertyKey::string(SLOT_ISO_MICROSECOND)).and_then(|v| v.as_int32()).unwrap_or(0) as u16;
        let ns = obj.get(&PropertyKey::string(SLOT_ISO_NANOSECOND)).and_then(|v| v.as_int32()).unwrap_or(0) as u16;
        temporal_rs::PlainDateTime::try_new(y, mo, d, h, mi, sec, ms, us, ns, temporal_rs::Calendar::default())
            .map_err(temporal_err)
    }

    // Helper: read a property from a Value (handles both JsObject and Proxy)
    fn get_val_property(ncx: &mut NativeContext<'_>, val: &Value, key: &str) -> Result<Value, VmError> {
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
            _ => Err(VmError::range_error(format!("{} is not a valid rounding mode", s))),
        }
    }

    // Helper: validate calendar argument (ToTemporalCalendarIdentifier)
    fn validate_calendar_arg(ncx: &mut NativeContext<'_>, cal: &Value) -> Result<String, VmError> {
        if cal.is_undefined() {
            return Ok("iso8601".to_string());
        }
        // Symbol → TypeError
        if cal.as_symbol().is_some() {
            return Err(VmError::type_error("Cannot convert a Symbol value to a string"));
        }
        // Temporal objects with calendar → use internal calendar
        // Only PlainDate, PlainDateTime, PlainMonthDay, PlainYearMonth, ZonedDateTime have calendars
        // Duration and Instant do NOT have calendars → TypeError
        if let Some(obj) = cal.as_object() {
            let tt = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            match tt.as_deref() {
                Some("PlainDate") | Some("PlainDateTime") | Some("PlainMonthDay") |
                Some("PlainYearMonth") | Some("ZonedDateTime") => {
                    return Ok("iso8601".to_string());
                }
                Some("Duration") | Some("Instant") => {
                    return Err(VmError::type_error(format!("{} instance is not a valid calendar", tt.unwrap())));
                }
                _ => {}
            }
        }
        // Non-string types → TypeError (per ToTemporalCalendarIdentifier)
        if !cal.is_string() {
            if cal.is_null() || cal.is_boolean() || cal.is_number() || cal.is_bigint() || cal.as_object().is_some() {
                return Err(VmError::type_error(format!("{} is not a valid calendar", ncx.to_string_value(cal).unwrap_or_default())));
            }
            return Err(VmError::type_error("calendar must be a string"));
        }
        let s = cal.as_string().unwrap().as_str().to_string();
        if s.is_empty() {
            return Err(VmError::range_error("empty string is not a valid calendar ID"));
        }
        // Validate calendar string: must be "iso8601" or a valid ISO string
        let lower = s.to_ascii_lowercase();
        if lower == "iso8601" {
            return Ok("iso8601".to_string());
        }
        // Try to parse as ISO date/datetime/time string
        if s.chars().any(|c| c.is_ascii_digit()) {
            if s.starts_with("-000000") || s.contains("-000000") {
                return Err(VmError::range_error("reject minus zero as extended year"));
            }
            if temporal_rs::PlainDateTime::from_utf8(s.as_bytes()).is_ok() {
                return Ok("iso8601".to_string());
            }
            if temporal_rs::PlainDate::from_utf8(s.as_bytes()).is_ok() {
                return Ok("iso8601".to_string());
            }
            if temporal_rs::PlainTime::from_utf8(s.as_bytes()).is_ok() {
                return Ok("iso8601".to_string());
            }
            if temporal_rs::PlainMonthDay::from_utf8(s.as_bytes()).is_ok() {
                return Ok("iso8601".to_string());
            }
            if temporal_rs::PlainYearMonth::from_utf8(s.as_bytes()).is_ok() {
                return Ok("iso8601".to_string());
            }
            return Err(VmError::range_error(format!("{} is not a valid calendar ID", s)));
        }
        Err(VmError::range_error(format!("{} is not a valid calendar ID", s)))
    }

    // Helper: read and validate a numeric temporal field, checking Infinity
    fn read_temporal_number(ncx: &mut NativeContext<'_>, val: &Value, field: &str) -> Result<f64, VmError> {
        let n = ncx.to_number_value(val)?;
        if n.is_infinite() {
            return Err(VmError::range_error(format!("{} property cannot be Infinity", field)));
        }
        Ok(n)
    }

    // Helper: ToTemporalDateTime — convert a Value to a temporal_rs::PlainDateTime
    // Handles: PlainDateTime objects, ZonedDateTime objects, property bags {year, month, day}, strings
    fn to_temporal_datetime(ncx: &mut NativeContext<'_>, item: &Value) -> Result<temporal_rs::PlainDateTime, VmError> {
        if item.is_string() {
            let s = ncx.to_string_value(item)?;
            reject_utc_designator_for_plain(s.as_str())?;
            return temporal_rs::PlainDateTime::from_utf8(s.as_bytes()).map_err(temporal_err);
        }

        if item.is_undefined() || item.is_null() || item.is_boolean() || item.is_number() || item.is_bigint() {
            return Err(VmError::type_error(format!("cannot convert {} to a PlainDateTime", item.type_of())));
        }

        if item.as_symbol().is_some() {
            return Err(VmError::type_error("Cannot convert a Symbol value to a string"));
        }

        // Handle both JsObject and Proxy
        if item.as_object().is_none() && item.as_proxy().is_none() {
            return Err(VmError::type_error("Expected an object or string"));
        }

        // Check if it's a Temporal type (only on real objects, not proxies)
        if let Some(obj) = item.as_object() {
            let temporal_type = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));

            if temporal_type.as_deref() == Some("PlainDateTime") {
                return extract_pdt(&obj);
            }

            if temporal_type.as_deref() == Some("PlainDate") {
                let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
                let mo = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
                let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
                return temporal_rs::PlainDateTime::try_new(
                    y, mo as u8, d as u8, 0, 0, 0, 0, 0, 0,
                    temporal_rs::Calendar::default(),
                ).map_err(temporal_err);
            }

            if temporal_type.as_deref() == Some("ZonedDateTime") {
                let epoch_ns_val = obj.get(&PropertyKey::string("epochNanoseconds")).unwrap_or(Value::int32(0));
                let tz_id_val = obj.get(&PropertyKey::string("timeZoneId")).unwrap_or(Value::string(JsString::intern("UTC")));
                let tz_id = if let Some(s) = tz_id_val.as_string() { s.as_str().to_string() } else { "UTC".to_string() };

                let epoch_ns: i128 = if epoch_ns_val.is_bigint() {
                    let s = ncx.to_string_value(&epoch_ns_val)?;
                    let s = s.trim_end_matches('n');
                    s.parse::<i128>().unwrap_or(0)
                } else if let Some(n) = epoch_ns_val.as_number() { n as i128 } else { 0 };

                let offset_ns: i128 = parse_timezone_offset_ns(&tz_id);
                let wall_ns = epoch_ns + offset_ns;

                let ns_per_ms: i128 = 1_000_000;
                let ms_per_s: i128 = 1_000;

                let epoch_ms = wall_ns.div_euclid(ns_per_ms);
                let remainder_ns = wall_ns.rem_euclid(ns_per_ms);
                let us_part = (remainder_ns / 1000) as u16;
                let ns_part = (remainder_ns % 1000) as u16;

                let epoch_secs = epoch_ms.div_euclid(ms_per_s);
                let ms_rem = epoch_ms.rem_euclid(ms_per_s) as u16;

                let ndt = chrono::DateTime::from_timestamp(epoch_secs as i64, (ms_rem as u32) * 1_000_000)
                    .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).unwrap())
                    .naive_utc();

                return temporal_rs::PlainDateTime::try_new(
                    ndt.year(), ndt.month() as u8, ndt.day() as u8,
                    ndt.hour() as u8, ndt.minute() as u8, ndt.second() as u8,
                    ms_rem, us_part, ns_part,
                    temporal_rs::Calendar::default(),
                ).map_err(temporal_err);
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
        let d = if !day_val.is_undefined() { read_temporal_number(ncx, &day_val, "day")? as i32 } else { -1 }; // -1 = missing

        // hour
        let hour_val = get_val_property(ncx, item, "hour")?;
        let h = if !hour_val.is_undefined() { read_temporal_number(ncx, &hour_val, "hour")? as i32 } else { 0 };

        // microsecond
        let us_val = get_val_property(ncx, item, "microsecond")?;
        let us = if !us_val.is_undefined() { read_temporal_number(ncx, &us_val, "microsecond")? as i32 } else { 0 };

        // millisecond
        let ms_val = get_val_property(ncx, item, "millisecond")?;
        let ms = if !ms_val.is_undefined() { read_temporal_number(ncx, &ms_val, "millisecond")? as i32 } else { 0 };

        // minute
        let minute_val = get_val_property(ncx, item, "minute")?;
        let mi = if !minute_val.is_undefined() { read_temporal_number(ncx, &minute_val, "minute")? as i32 } else { 0 };

        // month
        let month_val = get_val_property(ncx, item, "month")?;
        let month_num = if !month_val.is_undefined() { Some(read_temporal_number(ncx, &month_val, "month")? as i32) } else { None };

        // monthCode
        let month_code_val = get_val_property(ncx, item, "monthCode")?;
        let month_from_code = if !month_code_val.is_undefined() {
            let mc_str = ncx.to_string_value(&month_code_val)?;
            validate_month_code_syntax(&mc_str)?;
            Some(validate_month_code_iso_suitability(&mc_str)? as i32)
        } else { None };

        // nanosecond
        let ns_val = get_val_property(ncx, item, "nanosecond")?;
        let ns = if !ns_val.is_undefined() { read_temporal_number(ncx, &ns_val, "nanosecond")? as i32 } else { 0 };

        // second
        let second_val = get_val_property(ncx, item, "second")?;
        let sec = if !second_val.is_undefined() {
            let sv = read_temporal_number(ncx, &second_val, "second")? as i32;
            if sv == 60 { 59 } else { sv }
        } else { 0 };

        // year
        let year_val = get_val_property(ncx, item, "year")?;
        let y = if !year_val.is_undefined() { Some(read_temporal_number(ncx, &year_val, "year")? as i32) } else { None };

        // Check for required fields
        let y = match y {
            Some(y) => y,
            None => {
                if month_num.is_none() && month_from_code.is_none() && d == -1 {
                    return Err(VmError::type_error("plain object is not a valid property bag and does not convert to a string"));
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
            y, month as u8, d as u8,
            h as u8, mi as u8, sec as u8,
            ms as u16, us as u16, ns as u16,
            temporal_rs::Calendar::default(),
        ).map_err(temporal_err)
    }

    // Helper: ToTemporalDuration — convert a Value to a temporal_rs::Duration
    fn to_temporal_duration(ncx: &mut NativeContext<'_>, item: &Value) -> Result<temporal_rs::Duration, VmError> {
        if item.is_string() {
            let s = ncx.to_string_value(item)?;
            return temporal_rs::Duration::from_utf8(s.as_bytes()).map_err(temporal_err);
        }
        if item.is_undefined() || item.is_null() || item.is_boolean() || item.is_number() || item.is_bigint() {
            return Err(VmError::type_error(format!("cannot convert {} to a Duration", item.type_of())));
        }
        if item.as_symbol().is_some() {
            return Err(VmError::type_error("Cannot convert a Symbol value to a string"));
        }
        // Handle both JsObject and Proxy
        if item.as_object().is_none() && item.as_proxy().is_none() {
            return Err(VmError::type_error("Expected an object or string for duration"));
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

        // If it's a Duration Temporal object, extract fields with 0 defaults (blank duration is valid)
        if let Some(obj) = item.as_object() {
            let temporal_type = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if temporal_type.as_deref() == Some("Duration") {
                // Duration object — read fields with 0 defaults, allowing all-zero
                let y = obj.get(&PropertyKey::string("years")).and_then(|v| v.as_number()).unwrap_or(0.0);
                let mo = obj.get(&PropertyKey::string("months")).and_then(|v| v.as_number()).unwrap_or(0.0);
                let w = obj.get(&PropertyKey::string("weeks")).and_then(|v| v.as_number()).unwrap_or(0.0);
                let d = obj.get(&PropertyKey::string("days")).and_then(|v| v.as_number()).unwrap_or(0.0);
                let h = obj.get(&PropertyKey::string("hours")).and_then(|v| v.as_number()).unwrap_or(0.0);
                let mi = obj.get(&PropertyKey::string("minutes")).and_then(|v| v.as_number()).unwrap_or(0.0);
                let s = obj.get(&PropertyKey::string("seconds")).and_then(|v| v.as_number()).unwrap_or(0.0);
                let ms = obj.get(&PropertyKey::string("milliseconds")).and_then(|v| v.as_number()).unwrap_or(0.0);
                let us = obj.get(&PropertyKey::string("microseconds")).and_then(|v| v.as_number()).unwrap_or(0.0);
                let ns = obj.get(&PropertyKey::string("nanoseconds")).and_then(|v| v.as_number()).unwrap_or(0.0);
                return temporal_rs::Duration::new(
                    y as i64, mo as i64, w as i64, d as i64,
                    h as i64, mi as i64, s as i64, ms as i64,
                    us as i128, ns as i128,
                ).map_err(temporal_err);
            }
        }

        // Read AND convert each duration field IMMEDIATELY in ALPHABETICAL order per spec:
        // days, hours, microseconds, milliseconds, minutes, months, nanoseconds, seconds, weeks, years
        // Each get is immediately followed by valueOf conversion for observable ordering
        fn read_dur_field(ncx: &mut NativeContext<'_>, item: &Value, field: &str) -> Result<(bool, f64), VmError> {
            let v = get_val_property(ncx, item, field)?;
            if v.is_undefined() {
                return Ok((false, 0.0));
            }
            let n = ncx.to_number_value(&v)?;
            if n.is_infinite() {
                return Err(VmError::range_error(format!("{} property cannot be Infinity", field)));
            }
            if n.is_nan() {
                return Err(VmError::range_error(format!("{} property cannot be NaN", field)));
            }
            if n != n.trunc() {
                return Err(VmError::range_error(format!("{} property must be an integer", field)));
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

        let has_any = has_days || has_hours || has_us || has_ms || has_min || has_mo || has_ns || has_sec || has_wk || has_yr;

        if !has_any {
            return Err(VmError::type_error("duration object must have at least one temporal property"));
        }

        temporal_rs::Duration::new(
            years as i64, months as i64, weeks as i64, days as i64,
            hours as i64, minutes as i64, seconds as i64, milliseconds as i64,
            microseconds as i128, nanoseconds as i128,
        ).map_err(temporal_err)
    }

    // Helper: parse difference options in ALPHABETICAL order per spec:
    // largestUnit, roundingIncrement, roundingMode, smallestUnit
    fn parse_difference_settings(ncx: &mut NativeContext<'_>, options_val: &Value) -> Result<temporal_rs::options::DifferenceSettings, VmError> {
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
        } else { None };

        let ri_val = get_val_property(ncx, options_val, "roundingIncrement")?;
        let ri_parsed = if !ri_val.is_undefined() {
            let ri_num = ncx.to_number_value(&ri_val)?;
            Some(temporal_rs::options::RoundingIncrement::try_from(ri_num).map_err(temporal_err)?)
        } else { None };

        let rm_val = get_val_property(ncx, options_val, "roundingMode")?;
        let rm_parsed = if !rm_val.is_undefined() {
            let rm_str = ncx.to_string_value(&rm_val)?;
            Some(parse_rounding_mode(&rm_str)?)
        } else { None };

        let su_val = get_val_property(ncx, options_val, "smallestUnit")?;
        let su_parsed = if !su_val.is_undefined() {
            let su_str = ncx.to_string_value(&su_val)?;
            Some(parse_temporal_unit(&su_str)?)
        } else { None };

        settings.largest_unit = lu_parsed;
        settings.smallest_unit = su_parsed;
        settings.rounding_mode = rm_parsed;
        settings.increment = ri_parsed;
        Ok(settings)
    }

    // .since(other, options) method
    let since_fn = Value::native_function_with_proto_named(
        move |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("since called on non-object"))?;
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainDateTime") {
                return Err(VmError::type_error("since called on non-PlainDateTime"));
            }
            let this_pdt = extract_pdt(&obj)?;

            let other_arg = args.first().cloned().unwrap_or(Value::undefined());
            let other_pdt = to_temporal_datetime(ncx, &other_arg)?;

            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let settings = parse_difference_settings(ncx, &options_val)?;

            let duration = this_pdt.since(&other_pdt, settings).map_err(temporal_err)?;

            // Create a Duration object via Temporal.Duration constructor
            let global = ncx.global();
            let dur_ctor = global.get(&PropertyKey::string("Temporal"))
                .and_then(|v| v.as_object())
                .and_then(|t| t.get(&PropertyKey::string("Duration")))
                .ok_or_else(|| VmError::type_error("Temporal.Duration not found"))?;

            ncx.call_function_construct(&dur_ctor, Value::undefined(), &[
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
            ])
        },
        mm.clone(), fn_proto.clone(), "since", 1,
    );
    proto.define_property(PropertyKey::string("since"), PropertyDescriptor::builtin_method(since_fn));

    // .until(other, options) method
    let until_fn = Value::native_function_with_proto_named(
        move |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("until called on non-object"))?;
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainDateTime") {
                return Err(VmError::type_error("until called on non-PlainDateTime"));
            }
            let this_pdt = extract_pdt(&obj)?;

            let other_arg = args.first().cloned().unwrap_or(Value::undefined());
            let other_pdt = to_temporal_datetime(ncx, &other_arg)?;

            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let settings = parse_difference_settings(ncx, &options_val)?;

            let duration = this_pdt.until(&other_pdt, settings).map_err(temporal_err)?;

            let global = ncx.global();
            let dur_ctor = global.get(&PropertyKey::string("Temporal"))
                .and_then(|v| v.as_object())
                .and_then(|t| t.get(&PropertyKey::string("Duration")))
                .ok_or_else(|| VmError::type_error("Temporal.Duration not found"))?;

            ncx.call_function_construct(&dur_ctor, Value::undefined(), &[
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
            ])
        },
        mm.clone(), fn_proto.clone(), "until", 1,
    );
    proto.define_property(PropertyKey::string("until"), PropertyDescriptor::builtin_method(until_fn));

    // .add(duration, options) method
    let add_fn = Value::native_function_with_proto_named(
        move |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("add called on non-object"))?;
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainDateTime") {
                return Err(VmError::type_error("add called on non-PlainDateTime"));
            }
            let this_pdt = extract_pdt(&obj)?;

            let dur_arg = args.first().cloned().unwrap_or(Value::undefined());
            let duration = to_temporal_duration(ncx, &dur_arg)?;

            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = parse_overflow_option(ncx, &options_val)?;

            let result = this_pdt.add(&duration, Some(overflow)).map_err(temporal_err)?;

            let global = ncx.global();
            let pdt_ctor = global.get(&PropertyKey::string("Temporal"))
                .and_then(|v| v.as_object())
                .and_then(|t| t.get(&PropertyKey::string("PlainDateTime")))
                .ok_or_else(|| VmError::type_error("Temporal.PlainDateTime not found"))?;
            ncx.call_function_construct(&pdt_ctor, Value::undefined(), &[
                Value::int32(result.year()),
                Value::int32(result.month() as i32),
                Value::int32(result.day() as i32),
                Value::int32(result.hour() as i32),
                Value::int32(result.minute() as i32),
                Value::int32(result.second() as i32),
                Value::int32(result.millisecond() as i32),
                Value::int32(result.microsecond() as i32),
                Value::int32(result.nanosecond() as i32),
            ])
        },
        mm.clone(), fn_proto.clone(), "add", 1,
    );
    proto.define_property(PropertyKey::string("add"), PropertyDescriptor::builtin_method(add_fn));

    // .subtract(duration, options) method
    let subtract_fn = Value::native_function_with_proto_named(
        move |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("subtract called on non-object"))?;
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainDateTime") {
                return Err(VmError::type_error("subtract called on non-PlainDateTime"));
            }
            let this_pdt = extract_pdt(&obj)?;

            let dur_arg = args.first().cloned().unwrap_or(Value::undefined());
            let duration = to_temporal_duration(ncx, &dur_arg)?;

            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = parse_overflow_option(ncx, &options_val)?;

            let result = this_pdt.subtract(&duration, Some(overflow)).map_err(temporal_err)?;

            let global = ncx.global();
            let pdt_ctor = global.get(&PropertyKey::string("Temporal"))
                .and_then(|v| v.as_object())
                .and_then(|t| t.get(&PropertyKey::string("PlainDateTime")))
                .ok_or_else(|| VmError::type_error("Temporal.PlainDateTime not found"))?;
            ncx.call_function_construct(&pdt_ctor, Value::undefined(), &[
                Value::int32(result.year()),
                Value::int32(result.month() as i32),
                Value::int32(result.day() as i32),
                Value::int32(result.hour() as i32),
                Value::int32(result.minute() as i32),
                Value::int32(result.second() as i32),
                Value::int32(result.millisecond() as i32),
                Value::int32(result.microsecond() as i32),
                Value::int32(result.nanosecond() as i32),
            ])
        },
        mm.clone(), fn_proto.clone(), "subtract", 1,
    );
    proto.define_property(PropertyKey::string("subtract"), PropertyDescriptor::builtin_method(subtract_fn));

    // .withCalendar(calendar) method
    let withcal_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("withCalendar called on non-object"))?;
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainDateTime") {
                return Err(VmError::type_error("withCalendar called on non-PlainDateTime"));
            }

            let cal_arg = args.first().cloned().unwrap_or(Value::undefined());
            if cal_arg.is_undefined() {
                return Err(VmError::type_error("missing calendar argument"));
            }
            // ToTemporalCalendarIdentifier
            validate_calendar_arg(ncx, &cal_arg)?;

            // Return new PlainDateTime with same ISO fields
            let global = ncx.global();
            let pdt_ctor = global.get(&PropertyKey::string("Temporal"))
                .and_then(|v| v.as_object())
                .and_then(|t| t.get(&PropertyKey::string("PlainDateTime")))
                .ok_or_else(|| VmError::type_error("Temporal.PlainDateTime not found"))?;
            let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
            let mo = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            let h = obj.get(&PropertyKey::string(SLOT_ISO_HOUR)).and_then(|v| v.as_int32()).unwrap_or(0);
            let mi = obj.get(&PropertyKey::string(SLOT_ISO_MINUTE)).and_then(|v| v.as_int32()).unwrap_or(0);
            let sec = obj.get(&PropertyKey::string(SLOT_ISO_SECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let ms = obj.get(&PropertyKey::string(SLOT_ISO_MILLISECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let us = obj.get(&PropertyKey::string(SLOT_ISO_MICROSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let ns = obj.get(&PropertyKey::string(SLOT_ISO_NANOSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            ncx.call_function_construct(&pdt_ctor, Value::undefined(), &[
                Value::int32(y), Value::int32(mo), Value::int32(d),
                Value::int32(h), Value::int32(mi), Value::int32(sec),
                Value::int32(ms), Value::int32(us), Value::int32(ns),
            ])
        },
        mm.clone(), fn_proto.clone(), "withCalendar", 1,
    );
    proto.define_property(PropertyKey::string("withCalendar"), PropertyDescriptor::builtin_method(withcal_fn));

    // @@toStringTag
    proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Temporal.PlainDateTime")),
            PropertyAttributes { writable: false, enumerable: false, configurable: true },
        ),
    );
}
