use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use std::sync::Arc;

use super::common::*;

/// Install PlainTime constructor and prototype onto `temporal_obj`.
pub(super) fn install_plain_time(
    temporal_obj: &GcRef<JsObject>,
    obj_proto: &GcRef<JsObject>,
    fn_proto: &GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    let proto = GcRef::new(JsObject::new(Value::object(obj_proto.clone()), mm.clone()));
    let ctor_obj = GcRef::new(JsObject::new(Value::object(fn_proto.clone()), mm.clone()));

    ctor_obj.define_property(
        PropertyKey::string("prototype"),
        PropertyDescriptor::data_with_attrs(
            Value::object(proto.clone()),
            PropertyAttributes { writable: false, enumerable: false, configurable: false },
        ),
    );
    ctor_obj.define_property(
        PropertyKey::string("name"),
        PropertyDescriptor::function_length(Value::string(JsString::intern("PlainTime"))),
    );
    ctor_obj.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::function_length(Value::number(0.0)),
    );

    // Constructor: new Temporal.PlainTime(hour?, minute?, second?, ms?, us?, ns?)
    let ctor_fn: Box<
        dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync,
    > = Box::new(|this, args, ncx| {
        let obj = this.as_object().ok_or_else(|| VmError::type_error("PlainTime constructor requires new"))?;

        let mut get_arg = |idx: usize| -> Result<i32, VmError> {
            match args.get(idx) {
                None => Ok(0),
                Some(v) if v.is_undefined() => Ok(0),
                Some(v) => {
                    let n = ncx.to_number_value(v)?;
                    if n.is_nan() || n.is_infinite() || n != n.trunc() {
                        return Err(VmError::range_error("PlainTime field must be a finite integer"));
                    }
                    Ok(n as i32)
                }
            }
        };

        let h = get_arg(0)?;
        let mi = get_arg(1)?;
        let sec = get_arg(2)?;
        let ms = get_arg(3)?;
        let us = get_arg(4)?;
        let ns = get_arg(5)?;

        // Clamp leap second
        let sec = if sec == 60 { 59 } else { sec };
        // Validate ranges
        if !(0..=23).contains(&h) { return Err(VmError::range_error("hour must be 0-23")); }
        if !(0..=59).contains(&mi) { return Err(VmError::range_error("minute must be 0-59")); }
        if !(0..=59).contains(&sec) { return Err(VmError::range_error("second must be 0-59")); }
        if !(0..=999).contains(&ms) { return Err(VmError::range_error("millisecond must be 0-999")); }
        if !(0..=999).contains(&us) { return Err(VmError::range_error("microsecond must be 0-999")); }
        if !(0..=999).contains(&ns) { return Err(VmError::range_error("nanosecond must be 0-999")); }

        obj.define_property(PropertyKey::string(SLOT_TEMPORAL_TYPE),
            PropertyDescriptor::builtin_data(Value::string(JsString::intern("PlainTime"))));
        obj.define_property(PropertyKey::string(SLOT_ISO_HOUR),
            PropertyDescriptor::builtin_data(Value::int32(h)));
        obj.define_property(PropertyKey::string(SLOT_ISO_MINUTE),
            PropertyDescriptor::builtin_data(Value::int32(mi)));
        obj.define_property(PropertyKey::string(SLOT_ISO_SECOND),
            PropertyDescriptor::builtin_data(Value::int32(sec)));
        obj.define_property(PropertyKey::string(SLOT_ISO_MILLISECOND),
            PropertyDescriptor::builtin_data(Value::int32(ms)));
        obj.define_property(PropertyKey::string(SLOT_ISO_MICROSECOND),
            PropertyDescriptor::builtin_data(Value::int32(us)));
        obj.define_property(PropertyKey::string(SLOT_ISO_NANOSECOND),
            PropertyDescriptor::builtin_data(Value::int32(ns)));

        Ok(Value::undefined())
    });

    let ctor_value = Value::native_function_with_proto_and_object(
        Arc::from(ctor_fn),
        mm.clone(),
        fn_proto.clone(),
        ctor_obj.clone(),
    );

    // Prototype accessor getters
    for (name, slot) in &[
        ("hour", SLOT_ISO_HOUR),
        ("minute", SLOT_ISO_MINUTE),
        ("second", SLOT_ISO_SECOND),
        ("millisecond", SLOT_ISO_MILLISECOND),
        ("microsecond", SLOT_ISO_MICROSECOND),
        ("nanosecond", SLOT_ISO_NANOSECOND),
    ] {
        let slot_name = *slot;
        let getter_name = *name;
        proto.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::Accessor {
                get: Some(Value::native_function_with_proto(
                    move |this, _args, _ncx| {
                        let obj = this.as_object().ok_or_else(|| VmError::type_error(
                            format!("Temporal.PlainTime.prototype.{} requires a PlainTime receiver", getter_name)
                        ))?;
                        let tt = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                        if tt.as_deref() != Some("PlainTime") {
                            return Err(VmError::type_error(
                                format!("Temporal.PlainTime.prototype.{} requires a PlainTime receiver", getter_name)
                            ));
                        }
                        Ok(obj.get(&PropertyKey::string(slot_name)).unwrap_or(Value::int32(0)))
                    },
                    mm.clone(), fn_proto.clone(),
                )),
                set: None,
                attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
            },
        );
    }

    // toString
    let to_string_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toString"))?;
            let h = obj.get(&PropertyKey::string(SLOT_ISO_HOUR)).and_then(|v| v.as_int32()).unwrap_or(0);
            let mi = obj.get(&PropertyKey::string(SLOT_ISO_MINUTE)).and_then(|v| v.as_int32()).unwrap_or(0);
            let sec = obj.get(&PropertyKey::string(SLOT_ISO_SECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let ms = obj.get(&PropertyKey::string(SLOT_ISO_MILLISECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let us = obj.get(&PropertyKey::string(SLOT_ISO_MICROSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let ns = obj.get(&PropertyKey::string(SLOT_ISO_NANOSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);

            let sub = ms as i64 * 1_000_000 + us as i64 * 1_000 + ns as i64;
            let s = if sub != 0 {
                let frac = format!("{:09}", sub).trim_end_matches('0').to_string();
                format!("{:02}:{:02}:{:02}.{}", h, mi, sec, frac)
            } else if sec != 0 {
                format!("{:02}:{:02}:{:02}", h, mi, sec)
            } else {
                format!("{:02}:{:02}", h, mi)
            };
            Ok(Value::string(JsString::intern(&s)))
        },
        mm.clone(), fn_proto.clone(), "toString", 0,
    );
    proto.define_property(PropertyKey::string("toString"), PropertyDescriptor::builtin_method(to_string_fn.clone()));
    proto.define_property(PropertyKey::string("toJSON"), PropertyDescriptor::builtin_method(to_string_fn));

    // valueOf — always throw TypeError
    let value_of_fn = Value::native_function_with_proto_named(
        |_this, _args, _ncx| {
            Err(VmError::type_error("Temporal.PlainTime cannot be converted to a primitive"))
        },
        mm.clone(), fn_proto.clone(), "valueOf", 0,
    );
    proto.define_property(PropertyKey::string("valueOf"), PropertyDescriptor::builtin_method(value_of_fn));

    // toLocaleString — delegate to toString
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

    // equals
    let equals_fn = Value::native_function_with_proto_named(
        |this, args, _ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("equals"))?;
            let other_val = args.first().cloned().unwrap_or(Value::undefined());
            let other = other_val.as_object().ok_or_else(|| VmError::type_error("equals argument must be a PlainTime"))?;
            let slots = [SLOT_ISO_HOUR, SLOT_ISO_MINUTE, SLOT_ISO_SECOND, SLOT_ISO_MILLISECOND, SLOT_ISO_MICROSECOND, SLOT_ISO_NANOSECOND];
            for s in &slots {
                let a = obj.get(&PropertyKey::string(s)).and_then(|v| v.as_int32()).unwrap_or(0);
                let b = other.get(&PropertyKey::string(s)).and_then(|v| v.as_int32()).unwrap_or(0);
                if a != b { return Ok(Value::boolean(false)); }
            }
            Ok(Value::boolean(true))
        },
        mm.clone(), fn_proto.clone(), "equals", 1,
    );
    proto.define_property(PropertyKey::string("equals"), PropertyDescriptor::builtin_method(equals_fn));

    // @@toStringTag
    proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Temporal.PlainTime")),
            PropertyAttributes { writable: false, enumerable: false, configurable: true },
        ),
    );

    // prototype.constructor
    proto.define_property(
        PropertyKey::string("constructor"),
        PropertyDescriptor::data_with_attrs(ctor_value.clone(), PropertyAttributes::constructor_link()),
    );

    // PlainTime.from(item)
    let from_proto = proto.clone();
    let from_mm = mm.clone();
    let from_fn = Value::native_function_with_proto_named(
        move |_this, args, ncx| {
            let item = args.first().cloned().unwrap_or(Value::undefined());
            let is_object_like = item.as_object().is_some() || item.as_proxy().is_some();

            // If already a PlainTime or PlainDateTime (check internal slots directly, only for real objects)
            if let Some(obj) = item.as_object() {
                let tt = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                if tt.as_deref() == Some("PlainTime") {
                    let h = obj.get(&PropertyKey::string(SLOT_ISO_HOUR)).and_then(|v| v.as_int32()).unwrap_or(0);
                    let mi = obj.get(&PropertyKey::string(SLOT_ISO_MINUTE)).and_then(|v| v.as_int32()).unwrap_or(0);
                    let sec = obj.get(&PropertyKey::string(SLOT_ISO_SECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
                    let ms = obj.get(&PropertyKey::string(SLOT_ISO_MILLISECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
                    let us = obj.get(&PropertyKey::string(SLOT_ISO_MICROSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
                    let ns = obj.get(&PropertyKey::string(SLOT_ISO_NANOSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
                    let result = GcRef::new(JsObject::new(Value::object(from_proto.clone()), from_mm.clone()));
                    result.define_property(PropertyKey::string(SLOT_TEMPORAL_TYPE),
                        PropertyDescriptor::builtin_data(Value::string(JsString::intern("PlainTime"))));
                    result.define_property(PropertyKey::string(SLOT_ISO_HOUR), PropertyDescriptor::builtin_data(Value::int32(h)));
                    result.define_property(PropertyKey::string(SLOT_ISO_MINUTE), PropertyDescriptor::builtin_data(Value::int32(mi)));
                    result.define_property(PropertyKey::string(SLOT_ISO_SECOND), PropertyDescriptor::builtin_data(Value::int32(sec)));
                    result.define_property(PropertyKey::string(SLOT_ISO_MILLISECOND), PropertyDescriptor::builtin_data(Value::int32(ms)));
                    result.define_property(PropertyKey::string(SLOT_ISO_MICROSECOND), PropertyDescriptor::builtin_data(Value::int32(us)));
                    result.define_property(PropertyKey::string(SLOT_ISO_NANOSECOND), PropertyDescriptor::builtin_data(Value::int32(ns)));
                    return Ok(Value::object(result));
                }

                // PlainDateTime → extract time
                if tt.as_deref() == Some("PlainDateTime") {
                    let h = obj.get(&PropertyKey::string(SLOT_ISO_HOUR)).and_then(|v| v.as_int32()).unwrap_or(0);
                    let mi = obj.get(&PropertyKey::string(SLOT_ISO_MINUTE)).and_then(|v| v.as_int32()).unwrap_or(0);
                    let sec = obj.get(&PropertyKey::string(SLOT_ISO_SECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
                    let ms = obj.get(&PropertyKey::string(SLOT_ISO_MILLISECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
                    let us = obj.get(&PropertyKey::string(SLOT_ISO_MICROSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
                    let ns = obj.get(&PropertyKey::string(SLOT_ISO_NANOSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
                    let result = GcRef::new(JsObject::new(Value::object(from_proto.clone()), from_mm.clone()));
                    result.define_property(PropertyKey::string(SLOT_TEMPORAL_TYPE),
                        PropertyDescriptor::builtin_data(Value::string(JsString::intern("PlainTime"))));
                    result.define_property(PropertyKey::string(SLOT_ISO_HOUR), PropertyDescriptor::builtin_data(Value::int32(h)));
                    result.define_property(PropertyKey::string(SLOT_ISO_MINUTE), PropertyDescriptor::builtin_data(Value::int32(mi)));
                    result.define_property(PropertyKey::string(SLOT_ISO_SECOND), PropertyDescriptor::builtin_data(Value::int32(sec)));
                    result.define_property(PropertyKey::string(SLOT_ISO_MILLISECOND), PropertyDescriptor::builtin_data(Value::int32(ms)));
                    result.define_property(PropertyKey::string(SLOT_ISO_MICROSECOND), PropertyDescriptor::builtin_data(Value::int32(us)));
                    result.define_property(PropertyKey::string(SLOT_ISO_NANOSECOND), PropertyDescriptor::builtin_data(Value::int32(ns)));
                    return Ok(Value::object(result));
                }

                // ZonedDateTime → convert epoch nanoseconds to time components
                if tt.as_deref() == Some("ZonedDateTime") {
                    let (_y, _mo, _d, h, mi, sec, ms, us, ns) = zoned_datetime_to_parts(&obj, ncx)?;
                    let global = ncx.global();
                    let pt_ctor = global.get(&PropertyKey::string("Temporal"))
                        .and_then(|v| v.as_object())
                        .and_then(|t| t.get(&PropertyKey::string("PlainTime")))
                        .ok_or_else(|| VmError::type_error("Temporal.PlainTime not found"))?;
                    return ncx.call_function_construct(&pt_ctor, Value::undefined(), &[
                        Value::int32(h), Value::int32(mi), Value::int32(sec),
                        Value::int32(ms), Value::int32(us), Value::int32(ns),
                    ]);
                }
            }

            // Property bag — handles both plain objects and proxies
            // Read fields in alphabetical order per spec (ToTemporalTimeRecord):
            // hour, microsecond, millisecond, minute, nanosecond, second
            if is_object_like {
                let mut has_any = false;
                let mut get_field = |ncx: &mut NativeContext<'_>, name: &str| -> Result<i32, VmError> {
                    let v = ncx.get_property_of_value(&item, &PropertyKey::string(name))?;
                    if v.is_undefined() { return Ok(0); }
                    has_any = true;
                    let n = ncx.to_number_value(&v)?;
                    if n.is_nan() || n.is_infinite() || n != n.trunc() {
                        return Err(VmError::range_error(format!("{} must be a finite integer", name)));
                    }
                    Ok(n as i32)
                };
                // Alphabetical order: hour, microsecond, millisecond, minute, nanosecond, second
                let h = get_field(ncx, "hour")?;
                let us = get_field(ncx, "microsecond")?;
                let ms = get_field(ncx, "millisecond")?;
                let mi = get_field(ncx, "minute")?;
                let ns = get_field(ncx, "nanosecond")?;
                let sec = get_field(ncx, "second")?;
                if !has_any {
                    return Err(VmError::type_error("property bag must have at least one time property"));
                }
                let global = ncx.global();
                let pt_ctor = global.get(&PropertyKey::string("Temporal"))
                    .and_then(|v| v.as_object())
                    .and_then(|t| t.get(&PropertyKey::string("PlainTime")))
                    .ok_or_else(|| VmError::type_error("Temporal.PlainTime not found"))?;
                return ncx.call_function_construct(&pt_ctor, Value::undefined(), &[
                    Value::int32(h), Value::int32(mi), Value::int32(sec),
                    Value::int32(ms), Value::int32(us), Value::int32(ns),
                ]);
            }

            // String
            if item.is_string() {
                let s = ncx.to_string_value(&item)?;
                let (h, mi, sec, ms, us, ns) = parse_plain_time_string(&s)?;
                let global = ncx.global();
                let pt_ctor = global.get(&PropertyKey::string("Temporal"))
                    .and_then(|v| v.as_object())
                    .and_then(|t| t.get(&PropertyKey::string("PlainTime")))
                    .ok_or_else(|| VmError::type_error("Temporal.PlainTime not found"))?;
                return ncx.call_function_construct(&pt_ctor, Value::undefined(), &[
                    Value::int32(h), Value::int32(mi), Value::int32(sec),
                    Value::int32(ms), Value::int32(us), Value::int32(ns),
                ]);
            }

            Err(VmError::type_error("Cannot convert to PlainTime"))
        },
        mm.clone(),
        fn_proto.clone(),
        "from",
        1,
    );
    ctor_obj.define_property(
        PropertyKey::string("from"),
        PropertyDescriptor::data_with_attrs(from_fn, PropertyAttributes::builtin_method()),
    );

    // Register on namespace
    temporal_obj.define_property(
        PropertyKey::string("PlainTime"),
        PropertyDescriptor::data_with_attrs(ctor_value, PropertyAttributes::builtin_method()),
    );
}

/// Parse a time string like "12:34:56.789" into (h, mi, sec, ms, us, ns).
/// Also handles ISO datetime strings by extracting the time portion.
/// Follows Temporal spec for PlainTime string parsing.
pub(super) fn parse_plain_time_string(s: &str) -> Result<(i32, i32, i32, i32, i32, i32), VmError> {
    // Check for annotations and validate them
    validate_annotations(s)?;

    // Reject year -000000 (negative zero)
    if s.starts_with("-000000") || s.starts_with("\u{2212}000000") {
        return Err(VmError::range_error("reject minus zero as extended year"));
    }

    // Reject unicode minus \u{2212} in date/offset portions
    if s.contains('\u{2212}') {
        return Err(VmError::range_error(format!("variant minus sign: {}", s)));
    }

    // Extract the time portion from the string
    let time_part = extract_time_portion(s)?;

    // Check for UTC designator Z/z — reject for PlainTime
    let without_annot = time_part.split('[').next().unwrap_or(time_part);
    if without_annot.ends_with('Z') || without_annot.ends_with('z') {
        return Err(VmError::range_error(
            "String with UTC designator should not be valid as a PlainTime"
        ));
    }

    // Strip annotations and timezone offset
    let time_no_annot = time_part.split('[').next().unwrap_or(time_part);
    let time_clean = strip_tz_offset(time_no_annot);

    // Parse HH:MM:SS.fff or HH:MM:SS or HH:MM
    parse_time_components(time_clean)
}

/// Extract the time portion from a possibly full ISO datetime string.
/// Handles:
/// - Time-only: "12:34:56", "T12:34:56", "t12:34:56"
/// - DateTime with T: "1976-11-18T12:34:56"
/// - DateTime with space: "1976-11-18 12:34:56"
fn extract_time_portion(s: &str) -> Result<&str, VmError> {
    // If starts with T/t, it's a time-only string: strip T and return rest
    if s.starts_with('T') || s.starts_with('t') {
        return Ok(&s[1..]);
    }

    // Only search for T/t/space BEFORE any annotation brackets
    let search_area = s.split('[').next().unwrap_or(s);

    // Look for T/t separator in the string (datetime format)
    if let Some(t_pos) = search_area.find('T').or_else(|| search_area.find('t')) {
        return Ok(&s[t_pos + 1..]);
    }

    // Look for space separator between date and time (e.g., "1976-11-18 12:34:56")
    // A space separator is valid only when preceded by a date-like part (YYYY-MM-DD)
    if let Some(sp_pos) = search_area.find(' ') {
        let date_part = &s[..sp_pos];
        // Check if date part looks like a date (contains dashes and digits)
        if date_part.len() >= 8 && date_part.contains('-') {
            return Ok(&s[sp_pos + 1..]);
        }
    }

    // No date separator (T/t/space) found — the whole string is candidate time
    // Must reject ambiguous strings that could also be dates per Temporal spec.
    // Ambiguous strings require a T prefix for disambiguation.
    let no_annot = s.split('[').next().unwrap_or(s);
    let no_z = no_annot.trim_end_matches('Z').trim_end_matches('z');

    if !no_z.contains(':') {
        if is_ambiguous_date_time(no_z) {
            return Err(VmError::range_error(format!(
                "'{}' is ambiguous and requires T prefix", s
            )));
        }
    }

    Ok(s)
}

/// Parse time components from "HH:MM:SS.fff", "HH:MM:SS", "HH:MM",
/// or compact forms "HHMM", "HHMMSS", "HHMMSS.fff"
fn parse_time_components(time_clean: &str) -> Result<(i32, i32, i32, i32, i32, i32), VmError> {
    let (h, mi, sec, ms, us, ns) = if time_clean.contains(':') {
        // Colon-separated format
        let parts: Vec<&str> = time_clean.split(':').collect();
        if parts.is_empty() || parts[0].is_empty() {
            return Err(VmError::range_error(format!("Invalid time string: {}", time_clean)));
        }
        let h: i32 = parts[0].parse()
            .map_err(|_| VmError::range_error(format!("Invalid time string: {}", time_clean)))?;
        let mi: i32 = if parts.len() > 1 {
            parts[1].parse()
                .map_err(|_| VmError::range_error(format!("Invalid time string: {}", time_clean)))?
        } else { 0 };
        let (sec, ms, us, ns) = if parts.len() > 2 {
            parse_seconds_with_fraction(parts[2])?
        } else {
            (0, 0, 0, 0)
        };
        (h, mi, sec, ms, us, ns)
    } else {
        // Compact format: HHMM, HHMMSS, HHMMSS.fff
        let (digits, frac) = if let Some(dot_pos) = time_clean.find('.') {
            (&time_clean[..dot_pos], Some(&time_clean[dot_pos + 1..]))
        } else {
            (time_clean, None)
        };
        if digits.len() == 4 {
            // HHMM
            let h: i32 = digits[..2].parse().map_err(|_| VmError::range_error("Invalid time"))?;
            let mi: i32 = digits[2..4].parse().map_err(|_| VmError::range_error("Invalid time"))?;
            (h, mi, 0, 0, 0, 0)
        } else if digits.len() == 6 {
            // HHMMSS or HHMMSS.fff
            let h: i32 = digits[..2].parse().map_err(|_| VmError::range_error("Invalid time"))?;
            let mi: i32 = digits[2..4].parse().map_err(|_| VmError::range_error("Invalid time"))?;
            let sec: i32 = digits[4..6].parse().map_err(|_| VmError::range_error("Invalid time"))?;
            let (ms, us, ns) = if let Some(frac_str) = frac {
                let padded = format!("{:0<9}", frac_str);
                let ms: i32 = padded[0..3].parse().unwrap_or(0);
                let us: i32 = padded[3..6].parse().unwrap_or(0);
                let ns: i32 = padded[6..9].parse().unwrap_or(0);
                (ms, us, ns)
            } else {
                (0, 0, 0)
            };
            (h, mi, sec, ms, us, ns)
        } else if digits.len() == 2 {
            // HH only
            let h: i32 = digits.parse().map_err(|_| VmError::range_error("Invalid time"))?;
            (h, 0, 0, 0, 0, 0)
        } else {
            return Err(VmError::range_error(format!("Invalid time string: {}", time_clean)));
        }
    };

    // Leap second: clamp 60 → 59
    let sec = if sec == 60 { 59 } else { sec };

    if !(0..=23).contains(&h) { return Err(VmError::range_error(format!("hour must be 0-23, got {}", h))); }
    if !(0..=59).contains(&mi) { return Err(VmError::range_error("minute must be 0-59")); }
    if !(0..=59).contains(&sec) { return Err(VmError::range_error("second must be 0-59")); }

    Ok((h, mi, sec, ms, us, ns))
}

/// Parse seconds with optional fractional part: "56" or "56.987654321"
fn parse_seconds_with_fraction(sec_part: &str) -> Result<(i32, i32, i32, i32), VmError> {
    if let Some(dot_pos) = sec_part.find('.') {
        let sec: i32 = sec_part[..dot_pos].parse().map_err(|_| VmError::range_error("Invalid second"))?;
        let frac_str = &sec_part[dot_pos + 1..];
        let padded = format!("{:0<9}", frac_str);
        let ms: i32 = padded[0..3].parse().unwrap_or(0);
        let us: i32 = padded[3..6].parse().unwrap_or(0);
        let ns: i32 = padded[6..9].parse().unwrap_or(0);
        Ok((sec, ms, us, ns))
    } else {
        let sec: i32 = sec_part.parse().map_err(|_| VmError::range_error("Invalid second"))?;
        Ok((sec, 0, 0, 0))
    }
}

/// Check if a string without colons is ambiguous between a time and a date.
/// Returns true if the string could be interpreted as a valid date format.
/// The input `s` is already stripped of annotations and Z suffix.
fn is_ambiguous_date_time(s: &str) -> bool {
    // Helper: check max days in a month (leap year safe — use 29 for Feb)
    fn max_day(month: u32) -> u32 {
        match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 => 29,
            _ => 0,
        }
    }

    // Check if the string, possibly with a TZ offset, could be a date format.
    // "2021-12" → YYYY-MM (year 2021, month 12)
    // "2021-12[-12:00]" → annotations already stripped, so this is "2021-12"
    // "12-14" → MM-DD (month 12, day 14) or HH with offset -14
    // "1214" → MMDD (month 12, day 14) or HHMM (12:14)
    // "202112" → YYYYMM (year 2021, month 12) or HHMMSS (20:21:12)

    // Pattern: contains '-' → check for YYYY-MM or MM-DD patterns
    // Find the FIRST dash (the date separator, not the offset)
    if let Some(dash) = s.find('-') {
        let before = &s[..dash];
        let after_dash = &s[dash + 1..];
        // after_dash might have more content like another offset "-12:00"
        // Take only the first 2-3 chars after dash for the date part
        let after = if after_dash.len() >= 2 { &after_dash[..2] } else { after_dash };

        if before.len() == 4 && before.chars().all(|c| c.is_ascii_digit())
            && after.len() == 2 && after.chars().all(|c| c.is_ascii_digit()) {
            // YYYY-MM: ambiguous if MM is 01-12
            if let Ok(mm) = after.parse::<u32>() {
                if (1..=12).contains(&mm) { return true; }
            }
        }
        if before.len() == 2 && before.chars().all(|c| c.is_ascii_digit())
            && after.len() == 2 && after.chars().all(|c| c.is_ascii_digit()) {
            // MM-DD: ambiguous if MM is 01-12 and DD is valid for that month
            if let (Ok(mm), Ok(dd)) = (before.parse::<u32>(), after.parse::<u32>()) {
                if (1..=12).contains(&mm) && dd >= 1 && dd <= max_day(mm) {
                    return true;
                }
            }
        }
    }

    // Pattern: DDDD (MMDD or HHMM) — all digits, no dashes
    if s.len() == 4 && s.chars().all(|c| c.is_ascii_digit()) {
        let mm: u32 = s[..2].parse().unwrap_or(0);
        let dd: u32 = s[2..4].parse().unwrap_or(0);
        if (1..=12).contains(&mm) && dd >= 1 && dd <= max_day(mm) {
            return true;
        }
    }

    // Pattern: DDDDDD (YYYYMM or HHMMSS) — all digits, no dashes
    if s.len() == 6 && s.chars().all(|c| c.is_ascii_digit()) {
        let mm: u32 = s[4..6].parse().unwrap_or(0);
        if (1..=12).contains(&mm) {
            return true;
        }
    }

    false
}

/// Validate annotations in square brackets per Temporal spec.
/// Rejects:
/// - Multiple calendar annotations if any is critical (!)
/// - Unknown critical annotations (! prefix on non-calendar keys)
/// - Invalid annotation key capitalization (keys must match [a-z0-9-])
fn validate_annotations(s: &str) -> Result<(), VmError> {
    let mut calendar_count = 0;
    let mut has_critical_calendar = false;
    let mut tz_annotation_count = 0;
    let mut rest = s;

    while let Some(start) = rest.find('[') {
        let after_bracket = &rest[start + 1..];
        let end = after_bracket.find(']')
            .ok_or_else(|| VmError::range_error("Unclosed annotation bracket"))?;
        let annotation = &after_bracket[..end];
        rest = &after_bracket[end + 1..];

        let (is_critical, content) = if let Some(stripped) = annotation.strip_prefix('!') {
            (true, stripped)
        } else {
            (false, annotation)
        };

        if let Some(eq_pos) = content.find('=') {
            let key = &content[..eq_pos];
            // Check key is valid (lowercase, digits, hyphens)
            let is_valid_key = key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
            if !is_valid_key {
                return Err(VmError::range_error(format!(
                    "annotation keys must be lowercase: {}", annotation
                )));
            }

            if key == "u-ca" {
                calendar_count += 1;
                if is_critical { has_critical_calendar = true; }
            } else if is_critical {
                // Unknown critical annotation
                return Err(VmError::range_error(format!(
                    "reject unknown annotation with critical flag: {}", annotation
                )));
            }
        } else {
            // No '=' means this is a time zone annotation (e.g., [UTC], [America/New_York])
            tz_annotation_count += 1;
            if tz_annotation_count > 1 {
                return Err(VmError::range_error(format!(
                    "reject more than one time zone annotation: {}", s
                )));
            }
        }
    }

    if calendar_count > 1 && has_critical_calendar {
        return Err(VmError::range_error(format!(
            "reject more than one calendar annotation if any critical: {}", s
        )));
    }

    Ok(())
}

/// Strip timezone offset from a time string.
/// Handles +HH:MM, -HH:MM, +HHMM, -HHMM, +HH, -HH, and unicode minus \u{2212}
fn strip_tz_offset(s: &str) -> &str {
    // Look for + or - or \u{2212} that starts an offset
    // The offset comes after the time portion (HH:MM:SS.fff)
    // We need to find the LAST occurrence of +/- that looks like an offset start

    // First handle unicode minus
    if let Some(pos) = s.rfind('\u{2212}') {
        if pos > 0 {
            return &s[..pos];
        }
    }

    // Find the last '+' that's after a digit (not at start)
    if let Some(pos) = s.rfind('+') {
        if pos > 0 {
            return &s[..pos];
        }
    }

    // Find the last '-' that looks like an offset (after digit, and remaining looks like HH or HH:MM)
    // Be careful: the time itself might have no '-', but a date portion might
    if let Some(pos) = s.rfind('-') {
        if pos > 0 {
            let after = &s[pos + 1..];
            // Check if what follows looks like an offset (starts with digit)
            if after.starts_with(|c: char| c.is_ascii_digit()) && after.len() >= 2 {
                return &s[..pos];
            }
        }
    }

    // Strip trailing Z/z
    s.trim_end_matches('Z').trim_end_matches('z')
}
