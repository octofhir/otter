//! Temporal.Instant - fixed point in time (nanosecond precision)
//!
//! Backed by `temporal_rs::Instant` for spec-compliant behavior.

use otter_vm_core::error::VmError;
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::{Op, op_native};
use temporal_rs::Instant;
use temporal_rs::options::{
    DisplayCalendar, DisplayOffset, DisplayTimeZone, ToStringRoundingOptions,
};
use temporal_rs::provider::COMPILED_TZ_PROVIDER;

pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Temporal_Instant_from", instant_from),
        op_native(
            "__Temporal_Instant_fromEpochSeconds",
            instant_from_epoch_seconds,
        ),
        op_native(
            "__Temporal_Instant_fromEpochMilliseconds",
            instant_from_epoch_milliseconds,
        ),
        op_native(
            "__Temporal_Instant_fromEpochNanoseconds",
            instant_from_epoch_nanoseconds,
        ),
        op_native("__Temporal_Instant_epochSeconds", instant_epoch_seconds),
        op_native(
            "__Temporal_Instant_epochMilliseconds",
            instant_epoch_milliseconds,
        ),
        op_native(
            "__Temporal_Instant_epochNanoseconds",
            instant_epoch_nanoseconds,
        ),
        op_native("__Temporal_Instant_add", instant_add),
        op_native("__Temporal_Instant_subtract", instant_subtract),
        op_native("__Temporal_Instant_until", instant_until),
        op_native("__Temporal_Instant_since", instant_since),
        op_native("__Temporal_Instant_round", instant_round),
        op_native("__Temporal_Instant_equals", instant_equals),
        op_native("__Temporal_Instant_toString", instant_to_string),
        op_native("__Temporal_Instant_toJSON", instant_to_json),
        op_native(
            "__Temporal_Instant_toZonedDateTimeISO",
            instant_to_zoned_date_time_iso,
        ),
    ]
}

fn parse_instant(args: &[Value]) -> Result<Instant, VmError> {
    let s = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or_else(|| VmError::type_error("Invalid Instant"))?;

    // Try parsing as nanoseconds number first
    if let Ok(nanos) = s.as_str().parse::<i128>() {
        return Instant::try_new(nanos)
            .map_err(|e| VmError::type_error(format!("Invalid Instant: {e}")));
    }

    // Try parsing as ISO string
    Instant::from_utf8(s.as_str().as_bytes())
        .map_err(|e| VmError::type_error(format!("Invalid Instant string: {e}")))
}

fn instant_from(args: &[Value]) -> Result<Value, VmError> {
    let instant = parse_instant(args)?;
    Ok(Value::string(JsString::intern(
        &instant.as_i128().to_string(),
    )))
}

fn instant_from_epoch_seconds(args: &[Value]) -> Result<Value, VmError> {
    let secs = args
        .first()
        .and_then(|v| v.as_number())
        .ok_or_else(|| VmError::type_error("fromEpochSeconds requires a number"))?;
    let nanos = (secs * 1_000_000_000.0) as i128;
    let instant = Instant::try_new(nanos)
        .map_err(|e| VmError::type_error(format!("Invalid epoch seconds: {e}")))?;
    Ok(Value::string(JsString::intern(
        &instant.as_i128().to_string(),
    )))
}

fn instant_from_epoch_milliseconds(args: &[Value]) -> Result<Value, VmError> {
    let ms = args
        .first()
        .and_then(|v| v.as_number())
        .ok_or_else(|| VmError::type_error("fromEpochMilliseconds requires a number"))?;
    let nanos = (ms * 1_000_000.0) as i128;
    let instant = Instant::try_new(nanos)
        .map_err(|e| VmError::type_error(format!("Invalid epoch milliseconds: {e}")))?;
    Ok(Value::string(JsString::intern(
        &instant.as_i128().to_string(),
    )))
}

fn instant_from_epoch_nanoseconds(args: &[Value]) -> Result<Value, VmError> {
    let s = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or_else(|| VmError::type_error("fromEpochNanoseconds requires a string"))?;
    let nanos: i128 = s
        .as_str()
        .parse()
        .map_err(|_| VmError::type_error("Invalid nanoseconds string"))?;
    let instant = Instant::try_new(nanos)
        .map_err(|e| VmError::type_error(format!("Invalid epoch nanoseconds: {e}")))?;
    Ok(Value::string(JsString::intern(
        &instant.as_i128().to_string(),
    )))
}

fn instant_epoch_seconds(args: &[Value]) -> Result<Value, VmError> {
    let instant = parse_instant(args)?;
    Ok(Value::number((instant.as_i128() / 1_000_000_000) as f64))
}

fn instant_epoch_milliseconds(args: &[Value]) -> Result<Value, VmError> {
    let instant = parse_instant(args)?;
    Ok(Value::number((instant.as_i128() / 1_000_000) as f64))
}

fn instant_epoch_nanoseconds(args: &[Value]) -> Result<Value, VmError> {
    let instant = parse_instant(args)?;
    Ok(Value::string(JsString::intern(
        &instant.as_i128().to_string(),
    )))
}

fn instant_add(args: &[Value]) -> Result<Value, VmError> {
    let instant = parse_instant(args)?;
    let duration = if let Some(ns_str) = args.get(1).and_then(|v| v.as_string()) {
        if let Ok(ns) = ns_str.as_str().parse::<i128>() {
            // Raw nanoseconds
            let new_ns = instant.as_i128() + ns;
            return Instant::try_new(new_ns)
                .map(|i| Value::string(JsString::intern(&i.as_i128().to_string())))
                .map_err(|e| VmError::type_error(format!("Instant add error: {e}")));
        }
        temporal_rs::Duration::from_utf8(ns_str.as_str().as_bytes())
            .map_err(|e| VmError::type_error(format!("Invalid duration: {e}")))?
    } else if let Some(ns) = args.get(1).and_then(|v| v.as_number()) {
        let new_ns = instant.as_i128() + ns as i128;
        return Instant::try_new(new_ns)
            .map(|i| Value::string(JsString::intern(&i.as_i128().to_string())))
            .map_err(|e| VmError::type_error(format!("Instant add error: {e}")));
    } else {
        return Err(VmError::type_error("Missing duration"));
    };

    let result = instant
        .add(&duration)
        .map_err(|e| VmError::type_error(format!("Instant add error: {e}")))?;
    Ok(Value::string(JsString::intern(
        &result.as_i128().to_string(),
    )))
}

fn instant_subtract(args: &[Value]) -> Result<Value, VmError> {
    let instant = parse_instant(args)?;
    let duration = if let Some(ns_str) = args.get(1).and_then(|v| v.as_string()) {
        if let Ok(ns) = ns_str.as_str().parse::<i128>() {
            let new_ns = instant.as_i128() - ns;
            return Instant::try_new(new_ns)
                .map(|i| Value::string(JsString::intern(&i.as_i128().to_string())))
                .map_err(|e| VmError::type_error(format!("Instant subtract error: {e}")));
        }
        temporal_rs::Duration::from_utf8(ns_str.as_str().as_bytes())
            .map_err(|e| VmError::type_error(format!("Invalid duration: {e}")))?
    } else if let Some(ns) = args.get(1).and_then(|v| v.as_number()) {
        let new_ns = instant.as_i128() - ns as i128;
        return Instant::try_new(new_ns)
            .map(|i| Value::string(JsString::intern(&i.as_i128().to_string())))
            .map_err(|e| VmError::type_error(format!("Instant subtract error: {e}")));
    } else {
        return Err(VmError::type_error("Missing duration"));
    };

    let result = instant
        .subtract(&duration)
        .map_err(|e| VmError::type_error(format!("Instant subtract error: {e}")))?;
    Ok(Value::string(JsString::intern(
        &result.as_i128().to_string(),
    )))
}

fn instant_until(args: &[Value]) -> Result<Value, VmError> {
    let i1 = parse_instant(args)?;
    let i2 = args
        .get(1)
        .ok_or_else(|| VmError::type_error("Missing target instant"))
        .and_then(|v| parse_instant(&[v.clone()]))?;

    let diff_ns = i2.as_i128() - i1.as_i128();
    Ok(Value::string(JsString::intern(&diff_ns.to_string())))
}

fn instant_since(args: &[Value]) -> Result<Value, VmError> {
    let i1 = parse_instant(args)?;
    let i2 = args
        .get(1)
        .ok_or_else(|| VmError::type_error("Missing target instant"))
        .and_then(|v| parse_instant(&[v.clone()]))?;

    let diff_ns = i1.as_i128() - i2.as_i128();
    Ok(Value::string(JsString::intern(&diff_ns.to_string())))
}

fn instant_round(args: &[Value]) -> Result<Value, VmError> {
    let instant = parse_instant(args)?;
    let unit_str = args.get(1).and_then(|v| v.as_string());

    let smallest_unit = match unit_str.as_ref().map(|s| s.as_str()) {
        Some("hour") => temporal_rs::options::Unit::Hour,
        Some("minute") => temporal_rs::options::Unit::Minute,
        Some("second") => temporal_rs::options::Unit::Second,
        Some("millisecond") => temporal_rs::options::Unit::Millisecond,
        Some("microsecond") => temporal_rs::options::Unit::Microsecond,
        _ => temporal_rs::options::Unit::Nanosecond,
    };

    let mut options = temporal_rs::options::RoundingOptions::default();
    options.smallest_unit = Some(smallest_unit);

    let result = instant
        .round(options)
        .map_err(|e| VmError::type_error(format!("Instant round error: {e}")))?;
    Ok(Value::string(JsString::intern(
        &result.as_i128().to_string(),
    )))
}

fn instant_equals(args: &[Value]) -> Result<Value, VmError> {
    let ns1 = parse_instant(args).ok().map(|i| i.as_i128());
    let ns2 = args
        .get(1)
        .and_then(|v| parse_instant(&[v.clone()]).ok())
        .map(|i| i.as_i128());
    match (ns1, ns2) {
        (Some(a), Some(b)) => Ok(Value::boolean(a == b)),
        _ => Ok(Value::boolean(false)),
    }
}

fn instant_to_string(args: &[Value]) -> Result<Value, VmError> {
    let instant = parse_instant(args)?;
    let s = instant
        .to_ixdtf_string_with_provider(
            None,
            ToStringRoundingOptions::default(),
            &*COMPILED_TZ_PROVIDER,
        )
        .map_err(|e| VmError::type_error(format!("Instant toString error: {e}")))?;
    Ok(Value::string(JsString::intern(&s)))
}

fn instant_to_json(args: &[Value]) -> Result<Value, VmError> {
    instant_to_string(args)
}

fn instant_to_zoned_date_time_iso(args: &[Value]) -> Result<Value, VmError> {
    let instant = parse_instant(args)?;
    let tz = if let Some(tz_str) = args.get(1).and_then(|v| v.as_string()) {
        temporal_rs::TimeZone::try_from_identifier_str_with_provider(
            tz_str.as_str(),
            &*COMPILED_TZ_PROVIDER,
        )
        .map_err(|e| VmError::type_error(format!("Invalid timezone: {e}")))?
    } else {
        temporal_rs::Temporal::now()
            .time_zone_with_provider(&*COMPILED_TZ_PROVIDER)
            .map_err(|e| VmError::type_error(format!("System timezone error: {e}")))?
    };

    let zdt = instant
        .to_zoned_date_time_iso_with_provider(tz, &*COMPILED_TZ_PROVIDER)
        .map_err(|e| VmError::type_error(format!("toZonedDateTimeISO error: {e}")))?;

    let s = zdt
        .to_ixdtf_string_with_provider(
            DisplayOffset::Auto,
            DisplayTimeZone::Auto,
            DisplayCalendar::Auto,
            ToStringRoundingOptions::default(),
            &*COMPILED_TZ_PROVIDER,
        )
        .map_err(|e| VmError::type_error(format!("ZonedDateTime toString error: {e}")))?;
    Ok(Value::string(JsString::intern(&s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_instant_from_nanoseconds() {
        let args = vec![Value::string(JsString::intern("0"))];
        let result = instant_from(&args).unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "0");
    }

    #[test]
    fn test_instant_epoch_seconds() {
        let args = vec![Value::string(JsString::intern("1000000000000000000"))];
        let result = instant_epoch_seconds(&args).unwrap();
        assert_eq!(result.as_number(), Some(1000000000.0));
    }

    #[test]
    fn test_instant_add() {
        let args = vec![
            Value::string(JsString::intern("1000")),
            Value::string(JsString::intern("500")),
        ];
        let result = instant_add(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert_eq!(s, "1500");
    }
}
