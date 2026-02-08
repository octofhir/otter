//! Date.prototype methods implementation
//!
//! All Date object methods for ES2026 standard.
//! Date objects store timestamp in `__timestamp__` property (milliseconds since epoch).

use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use std::sync::Arc;

/// Helper to extract timestamp from Date object
fn get_timestamp_value(this_val: &Value) -> Result<f64, String> {
    let obj = this_val
        .as_object()
        .ok_or("Date method requires a Date object")?;
    let ts_val = obj
        .get(&PropertyKey::string("__timestamp__"))
        .ok_or("Date object missing __timestamp__")?;
    if let Some(n) = ts_val.as_number() {
        Ok(n)
    } else if let Some(i) = ts_val.as_int32() {
        Ok(i as f64)
    } else {
        Err("Invalid timestamp".to_string())
    }
}

fn get_timestamp(this_val: &Value) -> Result<i64, String> {
    let ts = get_timestamp_value(this_val)?;
    if ts.is_nan() || ts.is_infinite() {
        return Err("Invalid timestamp".to_string());
    }
    Ok(ts as i64)
}

/// Wire all Date.prototype methods to the prototype object
pub fn init_date_prototype(
    date_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    to_string_tag_symbol: crate::gc::GcRef<crate::value::Symbol>,
) {
    // Date.prototype[@@toStringTag] = "Date"
    date_proto.define_property(
        PropertyKey::Symbol(to_string_tag_symbol),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Date")),
            crate::object::PropertyAttributes::builtin_method(),
        ),
    );

    // Date.prototype.getTime()
    date_proto.define_property(
        PropertyKey::string("getTime"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                let ts = get_timestamp_value(this_val)?;
                Ok(Value::number(ts))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.valueOf() - same as getTime()
    date_proto.define_property(
        PropertyKey::string("valueOf"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                let ts = get_timestamp_value(this_val)?;
                Ok(Value::number(ts))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.toISOString()
    date_proto.define_property(
        PropertyKey::string("toISOString"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::DateTime;
                let ts = get_timestamp_value(this_val)?;
                if ts.is_nan() || ts.is_infinite() {
                    return Err(crate::error::VmError::range_error(
                        "Invalid time value",
                    ));
                }
                let dt = DateTime::from_timestamp(
                    (ts / 1000.0) as i64,
                    ((ts % 1000.0) * 1_000_000.0) as u32,
                )
                .ok_or("Invalid timestamp")?;
                Ok(Value::string(JsString::intern(&dt.to_rfc3339())))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.toString()
    date_proto.define_property(
        PropertyKey::string("toString"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::{DateTime, Local};
                let ts = get_timestamp_value(this_val)?;
                if ts.is_nan() || ts.is_infinite() {
                    return Ok(Value::string(JsString::intern("Invalid Date")));
                }
                let dt = DateTime::from_timestamp(
                    (ts / 1000.0) as i64,
                    ((ts % 1000.0) * 1_000_000.0) as u32,
                )
                .ok_or("Invalid timestamp")?;
                let local_dt: DateTime<Local> = dt.into();
                let str = format!("{}", local_dt.format("%a %b %d %Y %H:%M:%S GMT%z"));
                Ok(Value::string(JsString::intern(&str)))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.toDateString()
    date_proto.define_property(
        PropertyKey::string("toDateString"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::{DateTime, Local};
                let ts = get_timestamp_value(this_val)?;
                if ts.is_nan() || ts.is_infinite() {
                    return Ok(Value::string(JsString::intern("Invalid Date")));
                }
                let dt = DateTime::from_timestamp(
                    (ts / 1000.0) as i64,
                    ((ts % 1000.0) * 1_000_000.0) as u32,
                )
                .ok_or("Invalid timestamp")?;
                let local_dt: DateTime<Local> = dt.into();
                let str = format!("{}", local_dt.format("%a %b %d %Y"));
                Ok(Value::string(JsString::intern(&str)))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.toTimeString()
    date_proto.define_property(
        PropertyKey::string("toTimeString"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::{DateTime, Local};
                let ts = get_timestamp_value(this_val)?;
                if ts.is_nan() || ts.is_infinite() {
                    return Ok(Value::string(JsString::intern("Invalid Date")));
                }
                let dt = DateTime::from_timestamp(
                    (ts / 1000.0) as i64,
                    ((ts % 1000.0) * 1_000_000.0) as u32,
                )
                .ok_or("Invalid timestamp")?;
                let local_dt: DateTime<Local> = dt.into();
                let str = format!("{}", local_dt.format("%H:%M:%S GMT%z"));
                Ok(Value::string(JsString::intern(&str)))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.getFullYear()
    date_proto.define_property(
        PropertyKey::string("getFullYear"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::{DateTime, Datelike, Local};
                let ts = get_timestamp(this_val)?;
                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let local_dt: DateTime<Local> = dt.into();
                Ok(Value::int32(local_dt.year()))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.getMonth()
    date_proto.define_property(
        PropertyKey::string("getMonth"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::{DateTime, Datelike, Local};
                let ts = get_timestamp(this_val)?;
                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let local_dt: DateTime<Local> = dt.into();
                Ok(Value::int32(local_dt.month() as i32 - 1)) // JS months are 0-indexed
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.getDate()
    date_proto.define_property(
        PropertyKey::string("getDate"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::{DateTime, Datelike, Local};
                let ts = get_timestamp(this_val)?;
                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let local_dt: DateTime<Local> = dt.into();
                Ok(Value::int32(local_dt.day() as i32))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.getDay()
    date_proto.define_property(
        PropertyKey::string("getDay"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::{DateTime, Datelike, Local};
                let ts = get_timestamp(this_val)?;
                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let local_dt: DateTime<Local> = DateTime::from(dt);
                // Sunday = 0, Monday = 1, ..., Saturday = 6
                let day = local_dt.weekday().num_days_from_sunday();
                Ok(Value::int32(day as i32))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.getHours()
    date_proto.define_property(
        PropertyKey::string("getHours"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::{DateTime, Local, Timelike};
                let ts = get_timestamp(this_val)?;
                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let local_dt: DateTime<Local> = dt.into();
                Ok(Value::int32(local_dt.hour() as i32))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.getMinutes()
    date_proto.define_property(
        PropertyKey::string("getMinutes"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::{DateTime, Local, Timelike};
                let ts = get_timestamp(this_val)?;
                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let local_dt: DateTime<Local> = dt.into();
                Ok(Value::int32(local_dt.minute() as i32))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.getSeconds()
    date_proto.define_property(
        PropertyKey::string("getSeconds"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::{DateTime, Local, Timelike};
                let ts = get_timestamp(this_val)?;
                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let local_dt: DateTime<Local> = dt.into();
                Ok(Value::int32(local_dt.second() as i32))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.getMilliseconds()
    date_proto.define_property(
        PropertyKey::string("getMilliseconds"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                let ts = get_timestamp(this_val)?;
                Ok(Value::int32((ts % 1000) as i32))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.getTimezoneOffset()
    date_proto.define_property(
        PropertyKey::string("getTimezoneOffset"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::{DateTime, Local};
                let ts = get_timestamp(this_val)?;
                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let local_dt: DateTime<Local> = DateTime::from(dt);
                // Returns difference in minutes from UTC (negative for ahead of UTC)
                let offset_seconds = local_dt.offset().local_minus_utc();
                let offset_minutes = -(offset_seconds / 60);
                Ok(Value::int32(offset_minutes))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.toJSON()
    date_proto.define_property(
        PropertyKey::string("toJSON"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::DateTime;
                let ts = get_timestamp(this_val)?;
                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                Ok(Value::string(JsString::intern(&dt.to_rfc3339())))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.toUTCString()
    date_proto.define_property(
        PropertyKey::string("toUTCString"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::DateTime;
                let ts = get_timestamp(this_val)?;
                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                // RFC 2822 format: "Fri, 31 Jan 2026 09:30:00 GMT"
                Ok(Value::string(JsString::intern(
                    &dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
                )))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.toLocaleDateString()
    date_proto.define_property(
        PropertyKey::string("toLocaleDateString"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::{DateTime, Datelike, Local};
                let ts = get_timestamp(this_val)?;
                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let local_dt: DateTime<Local> = DateTime::from(dt);
                // Simple US format: "1/31/2026"
                Ok(Value::string(JsString::intern(&format!(
                    "{}/{}/{}",
                    local_dt.month(),
                    local_dt.day(),
                    local_dt.year()
                ))))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.toLocaleTimeString()
    date_proto.define_property(
        PropertyKey::string("toLocaleTimeString"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::{DateTime, Local, Timelike};
                let ts = get_timestamp(this_val)?;
                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let local_dt: DateTime<Local> = DateTime::from(dt);
                // Simple 12-hour format: "9:30:00 AM"
                let hour12 = if local_dt.hour() == 0 {
                    12
                } else if local_dt.hour() > 12 {
                    local_dt.hour() - 12
                } else {
                    local_dt.hour()
                };
                let ampm = if local_dt.hour() < 12 { "AM" } else { "PM" };
                Ok(Value::string(JsString::intern(&format!(
                    "{}:{:02}:{:02} {}",
                    hour12,
                    local_dt.minute(),
                    local_dt.second(),
                    ampm
                ))))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.toLocaleString()
    date_proto.define_property(
        PropertyKey::string("toLocaleString"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::{DateTime, Datelike, Local, Timelike};
                let ts = get_timestamp(this_val)?;
                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let local_dt: DateTime<Local> = DateTime::from(dt);
                // Combined: "1/31/2026, 9:30:00 AM"
                let hour12 = if local_dt.hour() == 0 {
                    12
                } else if local_dt.hour() > 12 {
                    local_dt.hour() - 12
                } else {
                    local_dt.hour()
                };
                let ampm = if local_dt.hour() < 12 { "AM" } else { "PM" };
                Ok(Value::string(JsString::intern(&format!(
                    "{}/{}/{}, {}:{:02}:{:02} {}",
                    local_dt.month(),
                    local_dt.day(),
                    local_dt.year(),
                    hour12,
                    local_dt.minute(),
                    local_dt.second(),
                    ampm
                ))))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // ====================================================================
    // UTC getters
    // ====================================================================

    // Date.prototype.getUTCFullYear()
    date_proto.define_property(
        PropertyKey::string("getUTCFullYear"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::{DateTime, Datelike, Utc};
                let ts = get_timestamp(this_val)?;
                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let utc_dt: DateTime<Utc> = dt.into();
                Ok(Value::int32(utc_dt.year()))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.getUTCMonth()
    date_proto.define_property(
        PropertyKey::string("getUTCMonth"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::{DateTime, Datelike, Utc};
                let ts = get_timestamp(this_val)?;
                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let utc_dt: DateTime<Utc> = dt.into();
                Ok(Value::int32(utc_dt.month() as i32 - 1)) // JS months are 0-indexed
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.getUTCDate()
    date_proto.define_property(
        PropertyKey::string("getUTCDate"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::{DateTime, Datelike, Utc};
                let ts = get_timestamp(this_val)?;
                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let utc_dt: DateTime<Utc> = dt.into();
                Ok(Value::int32(utc_dt.day() as i32))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.getUTCDay()
    date_proto.define_property(
        PropertyKey::string("getUTCDay"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::{DateTime, Datelike, Utc};
                let ts = get_timestamp(this_val)?;
                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let utc_dt: DateTime<Utc> = dt.into();
                let day = utc_dt.weekday().num_days_from_sunday();
                Ok(Value::int32(day as i32))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.getUTCHours()
    date_proto.define_property(
        PropertyKey::string("getUTCHours"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::{DateTime, Timelike, Utc};
                let ts = get_timestamp(this_val)?;
                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let utc_dt: DateTime<Utc> = dt.into();
                Ok(Value::int32(utc_dt.hour() as i32))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.getUTCMinutes()
    date_proto.define_property(
        PropertyKey::string("getUTCMinutes"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::{DateTime, Timelike, Utc};
                let ts = get_timestamp(this_val)?;
                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let utc_dt: DateTime<Utc> = dt.into();
                Ok(Value::int32(utc_dt.minute() as i32))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.getUTCSeconds()
    date_proto.define_property(
        PropertyKey::string("getUTCSeconds"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                use chrono::{DateTime, Timelike, Utc};
                let ts = get_timestamp(this_val)?;
                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let utc_dt: DateTime<Utc> = dt.into();
                Ok(Value::int32(utc_dt.second() as i32))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.getUTCMilliseconds()
    date_proto.define_property(
        PropertyKey::string("getUTCMilliseconds"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                let ts = get_timestamp(this_val)?;
                Ok(Value::int32((ts % 1000) as i32))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // ====================================================================
    // Local setters
    // ====================================================================

    // Date.prototype.setTime(ms)
    date_proto.define_property(
        PropertyKey::string("setTime"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or("Date method requires a Date object")?;
                let new_time = args
                    .first()
                    .and_then(|v| v.as_number())
                    .ok_or("setTime requires a number argument")?;
                let _ = obj.set(
                    PropertyKey::string("__timestamp__"),
                    Value::number(new_time),
                );
                Ok(Value::number(new_time))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.setMilliseconds(ms)
    date_proto.define_property(
        PropertyKey::string("setMilliseconds"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                use chrono::{DateTime, Local};
                let obj = this_val
                    .as_object()
                    .ok_or("Date method requires a Date object")?;
                let ts = get_timestamp(this_val)?;
                let ms = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i64;

                let dt = DateTime::from_timestamp(ts / 1000, 0).ok_or("Invalid timestamp")?;
                let local_dt: DateTime<Local> = dt.into();
                let new_ts = (local_dt.timestamp() * 1000) + ms;

                let _ = obj.set(
                    PropertyKey::string("__timestamp__"),
                    Value::number(new_ts as f64),
                );
                Ok(Value::number(new_ts as f64))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.setSeconds(sec [, ms])
    date_proto.define_property(
        PropertyKey::string("setSeconds"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                use chrono::{DateTime, Local, Timelike};
                let obj = this_val
                    .as_object()
                    .ok_or("Date method requires a Date object")?;
                let ts = get_timestamp(this_val)?;
                let sec = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                let ms = args.get(1).and_then(|v| v.as_number()).map(|v| v as i64);

                let dt = DateTime::from_timestamp(ts / 1000, 0).ok_or("Invalid timestamp")?;
                let local_dt: DateTime<Local> = dt.into();

                let new_dt = local_dt.with_second(sec).ok_or("Invalid second")?;
                let mut new_ts = new_dt.timestamp() * 1000;
                if let Some(ms_val) = ms {
                    new_ts += ms_val;
                } else {
                    new_ts += ts % 1000;
                }

                let _ = obj.set(
                    PropertyKey::string("__timestamp__"),
                    Value::number(new_ts as f64),
                );
                Ok(Value::number(new_ts as f64))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.setMinutes(min [, sec [, ms]])
    date_proto.define_property(
        PropertyKey::string("setMinutes"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                use chrono::{DateTime, Local, Timelike};
                let obj = this_val
                    .as_object()
                    .ok_or("Date method requires a Date object")?;
                let ts = get_timestamp(this_val)?;
                let min = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                let sec = args.get(1).and_then(|v| v.as_number()).map(|v| v as u32);
                let ms = args.get(2).and_then(|v| v.as_number()).map(|v| v as i64);

                let dt = DateTime::from_timestamp(ts / 1000, 0).ok_or("Invalid timestamp")?;
                let mut local_dt: DateTime<Local> = dt.into();

                local_dt = local_dt.with_minute(min).ok_or("Invalid minute")?;
                if let Some(s) = sec {
                    local_dt = local_dt.with_second(s).ok_or("Invalid second")?;
                }

                let mut new_ts = local_dt.timestamp() * 1000;
                if let Some(ms_val) = ms {
                    new_ts += ms_val;
                } else if sec.is_none() {
                    new_ts += ts % 1000;
                }

                let _ = obj.set(
                    PropertyKey::string("__timestamp__"),
                    Value::number(new_ts as f64),
                );
                Ok(Value::number(new_ts as f64))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.setHours(hours [, min [, sec [, ms]]])
    date_proto.define_property(
        PropertyKey::string("setHours"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                use chrono::{DateTime, Local, Timelike};
                let obj = this_val
                    .as_object()
                    .ok_or("Date method requires a Date object")?;
                let ts = get_timestamp(this_val)?;
                let hours = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                let min = args.get(1).and_then(|v| v.as_number()).map(|v| v as u32);
                let sec = args.get(2).and_then(|v| v.as_number()).map(|v| v as u32);
                let ms = args.get(3).and_then(|v| v.as_number()).map(|v| v as i64);

                let dt = DateTime::from_timestamp(ts / 1000, 0).ok_or("Invalid timestamp")?;
                let mut local_dt: DateTime<Local> = dt.into();

                local_dt = local_dt.with_hour(hours).ok_or("Invalid hour")?;
                if let Some(m) = min {
                    local_dt = local_dt.with_minute(m).ok_or("Invalid minute")?;
                }
                if let Some(s) = sec {
                    local_dt = local_dt.with_second(s).ok_or("Invalid second")?;
                }

                let mut new_ts = local_dt.timestamp() * 1000;
                if let Some(ms_val) = ms {
                    new_ts += ms_val;
                } else if min.is_none() {
                    new_ts += ts % 1000;
                }

                let _ = obj.set(
                    PropertyKey::string("__timestamp__"),
                    Value::number(new_ts as f64),
                );
                Ok(Value::number(new_ts as f64))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.setDate(date)
    date_proto.define_property(
        PropertyKey::string("setDate"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                use chrono::{DateTime, Datelike, Local};
                let obj = this_val
                    .as_object()
                    .ok_or("Date method requires a Date object")?;
                let ts = get_timestamp(this_val)?;
                let date = args.first().and_then(|v| v.as_number()).unwrap_or(1.0) as u32;

                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let local_dt: DateTime<Local> = dt.into();

                let new_dt = local_dt.with_day(date).ok_or("Invalid date")?;
                let new_ts = new_dt.timestamp() * 1000 + (ts % 1000);

                let _ = obj.set(
                    PropertyKey::string("__timestamp__"),
                    Value::number(new_ts as f64),
                );
                Ok(Value::number(new_ts as f64))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.setMonth(month [, date])
    date_proto.define_property(
        PropertyKey::string("setMonth"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                use chrono::{DateTime, Datelike, Local};
                let obj = this_val
                    .as_object()
                    .ok_or("Date method requires a Date object")?;
                let ts = get_timestamp(this_val)?;
                let month = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as u32 + 1; // JS months are 0-indexed
                let date = args.get(1).and_then(|v| v.as_number()).map(|v| v as u32);

                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let mut local_dt: DateTime<Local> = dt.into();

                local_dt = local_dt.with_month(month).ok_or("Invalid month")?;
                if let Some(d) = date {
                    local_dt = local_dt.with_day(d).ok_or("Invalid date")?;
                }

                let new_ts = local_dt.timestamp() * 1000 + (ts % 1000);

                let _ = obj.set(
                    PropertyKey::string("__timestamp__"),
                    Value::number(new_ts as f64),
                );
                Ok(Value::number(new_ts as f64))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.setFullYear(year [, month [, date]])
    date_proto.define_property(
        PropertyKey::string("setFullYear"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                use chrono::{DateTime, Datelike, Local};
                let obj = this_val
                    .as_object()
                    .ok_or("Date method requires a Date object")?;
                let ts = get_timestamp(this_val)?;
                let year = args.first().and_then(|v| v.as_number()).unwrap_or(1970.0) as i32;
                let month = args
                    .get(1)
                    .and_then(|v| v.as_number())
                    .map(|v| v as u32 + 1);
                let date = args.get(2).and_then(|v| v.as_number()).map(|v| v as u32);

                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let mut local_dt: DateTime<Local> = dt.into();

                local_dt = local_dt.with_year(year).ok_or("Invalid year")?;
                if let Some(m) = month {
                    local_dt = local_dt.with_month(m).ok_or("Invalid month")?;
                }
                if let Some(d) = date {
                    local_dt = local_dt.with_day(d).ok_or("Invalid date")?;
                }

                let new_ts = local_dt.timestamp() * 1000 + (ts % 1000);

                let _ = obj.set(
                    PropertyKey::string("__timestamp__"),
                    Value::number(new_ts as f64),
                );
                Ok(Value::number(new_ts as f64))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // ====================================================================
    // UTC setters
    // ====================================================================

    // Date.prototype.setUTCMilliseconds(ms)
    date_proto.define_property(
        PropertyKey::string("setUTCMilliseconds"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                use chrono::{DateTime, Utc};
                let obj = this_val
                    .as_object()
                    .ok_or("Date method requires a Date object")?;
                let ts = get_timestamp(this_val)?;
                let ms = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i64;

                let dt = DateTime::from_timestamp(ts / 1000, 0).ok_or("Invalid timestamp")?;
                let utc_dt: DateTime<Utc> = dt.into();
                let new_ts = (utc_dt.timestamp() * 1000) + ms;

                let _ = obj.set(
                    PropertyKey::string("__timestamp__"),
                    Value::number(new_ts as f64),
                );
                Ok(Value::number(new_ts as f64))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.setUTCSeconds(sec [, ms])
    date_proto.define_property(
        PropertyKey::string("setUTCSeconds"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                use chrono::{DateTime, Timelike, Utc};
                let obj = this_val
                    .as_object()
                    .ok_or("Date method requires a Date object")?;
                let ts = get_timestamp(this_val)?;
                let sec = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                let ms = args.get(1).and_then(|v| v.as_number()).map(|v| v as i64);

                let dt = DateTime::from_timestamp(ts / 1000, 0).ok_or("Invalid timestamp")?;
                let utc_dt: DateTime<Utc> = dt.into();

                let new_dt = utc_dt.with_second(sec).ok_or("Invalid second")?;
                let mut new_ts = new_dt.timestamp() * 1000;
                if let Some(ms_val) = ms {
                    new_ts += ms_val;
                } else {
                    new_ts += ts % 1000;
                }

                let _ = obj.set(
                    PropertyKey::string("__timestamp__"),
                    Value::number(new_ts as f64),
                );
                Ok(Value::number(new_ts as f64))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.setUTCMinutes(min [, sec [, ms]])
    date_proto.define_property(
        PropertyKey::string("setUTCMinutes"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                use chrono::{DateTime, Timelike, Utc};
                let obj = this_val
                    .as_object()
                    .ok_or("Date method requires a Date object")?;
                let ts = get_timestamp(this_val)?;
                let min = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                let sec = args.get(1).and_then(|v| v.as_number()).map(|v| v as u32);
                let ms = args.get(2).and_then(|v| v.as_number()).map(|v| v as i64);

                let dt = DateTime::from_timestamp(ts / 1000, 0).ok_or("Invalid timestamp")?;
                let mut utc_dt: DateTime<Utc> = dt.into();

                utc_dt = utc_dt.with_minute(min).ok_or("Invalid minute")?;
                if let Some(s) = sec {
                    utc_dt = utc_dt.with_second(s).ok_or("Invalid second")?;
                }

                let mut new_ts = utc_dt.timestamp() * 1000;
                if let Some(ms_val) = ms {
                    new_ts += ms_val;
                } else if sec.is_none() {
                    new_ts += ts % 1000;
                }

                let _ = obj.set(
                    PropertyKey::string("__timestamp__"),
                    Value::number(new_ts as f64),
                );
                Ok(Value::number(new_ts as f64))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.setUTCHours(hours [, min [, sec [, ms]]])
    date_proto.define_property(
        PropertyKey::string("setUTCHours"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                use chrono::{DateTime, Timelike, Utc};
                let obj = this_val
                    .as_object()
                    .ok_or("Date method requires a Date object")?;
                let ts = get_timestamp(this_val)?;
                let hours = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                let min = args.get(1).and_then(|v| v.as_number()).map(|v| v as u32);
                let sec = args.get(2).and_then(|v| v.as_number()).map(|v| v as u32);
                let ms = args.get(3).and_then(|v| v.as_number()).map(|v| v as i64);

                let dt = DateTime::from_timestamp(ts / 1000, 0).ok_or("Invalid timestamp")?;
                let mut utc_dt: DateTime<Utc> = dt.into();

                utc_dt = utc_dt.with_hour(hours).ok_or("Invalid hour")?;
                if let Some(m) = min {
                    utc_dt = utc_dt.with_minute(m).ok_or("Invalid minute")?;
                }
                if let Some(s) = sec {
                    utc_dt = utc_dt.with_second(s).ok_or("Invalid second")?;
                }

                let mut new_ts = utc_dt.timestamp() * 1000;
                if let Some(ms_val) = ms {
                    new_ts += ms_val;
                } else if min.is_none() {
                    new_ts += ts % 1000;
                }

                let _ = obj.set(
                    PropertyKey::string("__timestamp__"),
                    Value::number(new_ts as f64),
                );
                Ok(Value::number(new_ts as f64))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.setUTCDate(date)
    date_proto.define_property(
        PropertyKey::string("setUTCDate"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                use chrono::{DateTime, Datelike, Utc};
                let obj = this_val
                    .as_object()
                    .ok_or("Date method requires a Date object")?;
                let ts = get_timestamp(this_val)?;
                let date = args.first().and_then(|v| v.as_number()).unwrap_or(1.0) as u32;

                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let utc_dt: DateTime<Utc> = dt.into();

                let new_dt = utc_dt.with_day(date).ok_or("Invalid date")?;
                let new_ts = new_dt.timestamp() * 1000 + (ts % 1000);

                let _ = obj.set(
                    PropertyKey::string("__timestamp__"),
                    Value::number(new_ts as f64),
                );
                Ok(Value::number(new_ts as f64))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.setUTCMonth(month [, date])
    date_proto.define_property(
        PropertyKey::string("setUTCMonth"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                use chrono::{DateTime, Datelike, Utc};
                let obj = this_val
                    .as_object()
                    .ok_or("Date method requires a Date object")?;
                let ts = get_timestamp(this_val)?;
                let month = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as u32 + 1;
                let date = args.get(1).and_then(|v| v.as_number()).map(|v| v as u32);

                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let mut utc_dt: DateTime<Utc> = dt.into();

                utc_dt = utc_dt.with_month(month).ok_or("Invalid month")?;
                if let Some(d) = date {
                    utc_dt = utc_dt.with_day(d).ok_or("Invalid date")?;
                }

                let new_ts = utc_dt.timestamp() * 1000 + (ts % 1000);

                let _ = obj.set(
                    PropertyKey::string("__timestamp__"),
                    Value::number(new_ts as f64),
                );
                Ok(Value::number(new_ts as f64))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Date.prototype.setUTCFullYear(year [, month [, date]])
    date_proto.define_property(
        PropertyKey::string("setUTCFullYear"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                use chrono::{DateTime, Datelike, Utc};
                let obj = this_val
                    .as_object()
                    .ok_or("Date method requires a Date object")?;
                let ts = get_timestamp(this_val)?;
                let year = args.first().and_then(|v| v.as_number()).unwrap_or(1970.0) as i32;
                let month = args
                    .get(1)
                    .and_then(|v| v.as_number())
                    .map(|v| v as u32 + 1);
                let date = args.get(2).and_then(|v| v.as_number()).map(|v| v as u32);

                let dt = DateTime::from_timestamp(ts / 1000, ((ts % 1000) * 1_000_000) as u32)
                    .ok_or("Invalid timestamp")?;
                let mut utc_dt: DateTime<Utc> = dt.into();

                utc_dt = utc_dt.with_year(year).ok_or("Invalid year")?;
                if let Some(m) = month {
                    utc_dt = utc_dt.with_month(m).ok_or("Invalid month")?;
                }
                if let Some(d) = date {
                    utc_dt = utc_dt.with_day(d).ok_or("Invalid date")?;
                }

                let new_ts = utc_dt.timestamp() * 1000 + (ts % 1000);

                let _ = obj.set(
                    PropertyKey::string("__timestamp__"),
                    Value::number(new_ts as f64),
                );
                Ok(Value::number(new_ts as f64))
            },
            mm.clone(),
            fn_proto,
        )),
    );
}
