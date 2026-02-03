//! Temporal.Now - utilities for getting current time

use otter_vm_core::value::Value;
use otter_vm_core::{VmError, string::JsString};
use otter_vm_runtime::{Op, op_native};
use temporal_rs::Temporal;
use temporal_rs::options::{DisplayCalendar, DisplayOffset, DisplayTimeZone, ToStringRoundingOptions};
use temporal_rs::provider::COMPILED_TZ_PROVIDER;

pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Temporal_Now_instant", now_instant),
        op_native("__Temporal_Now_timeZoneId", now_timezone_id),
        op_native("__Temporal_Now_zonedDateTimeISO", now_zoned_date_time_iso),
        op_native("__Temporal_Now_plainDateTimeISO", now_plain_date_time_iso),
        op_native("__Temporal_Now_plainDateISO", now_plain_date_iso),
        op_native("__Temporal_Now_plainTimeISO", now_plain_time_iso),
    ]
}

fn now_instant(_args: &[Value]) -> Result<Value, VmError> {
    let instant = Temporal::now()
        .instant()
        .map_err(|e| VmError::type_error(format!("Failed to get instant: {:?}", e)))?;

    Ok(Value::string(JsString::intern(
        &instant.epoch_nanoseconds().as_i128().to_string(),
    )))
}

fn now_timezone_id(_args: &[Value]) -> Result<Value, VmError> {
    let tz = Temporal::now()
        .time_zone_with_provider(&*COMPILED_TZ_PROVIDER)
        .map_err(|e| VmError::type_error(format!("Failed to get timezone: {:?}", e)))?;

    let tz_id = tz
        .identifier_with_provider(&*COMPILED_TZ_PROVIDER)
        .map_err(|e| VmError::type_error(format!("Failed to get timezone id: {:?}", e)))?;
    Ok(Value::string(JsString::intern(&tz_id)))
}

fn now_zoned_date_time_iso(_args: &[Value]) -> Result<Value, VmError> {
    let zdt = Temporal::now()
        .zoned_date_time_iso_with_provider(None, &*COMPILED_TZ_PROVIDER)
        .map_err(|e| VmError::type_error(format!("Failed to get zoned datetime: {:?}", e)))?;

    let s = zdt
        .to_ixdtf_string_with_provider(
            DisplayOffset::Auto,
            DisplayTimeZone::Auto,
            DisplayCalendar::Auto,
            ToStringRoundingOptions::default(),
            &*COMPILED_TZ_PROVIDER,
        )
        .map_err(|e| VmError::type_error(format!("Failed to format: {:?}", e)))?;

    Ok(Value::string(JsString::intern(&s)))
}

fn now_plain_date_time_iso(_args: &[Value]) -> Result<Value, VmError> {
    let dt = Temporal::now()
        .plain_date_time_iso_with_provider(None, &*COMPILED_TZ_PROVIDER)
        .map_err(|e| VmError::type_error(format!("Failed to get datetime: {:?}", e)))?;

    let s = dt
        .to_ixdtf_string(ToStringRoundingOptions::default(), DisplayCalendar::Auto)
        .map_err(|e| VmError::type_error(format!("Failed to format: {:?}", e)))?;

    Ok(Value::string(JsString::intern(&s)))
}

fn now_plain_date_iso(_args: &[Value]) -> Result<Value, VmError> {
    let date = Temporal::now()
        .plain_date_iso_with_provider(None, &*COMPILED_TZ_PROVIDER)
        .map_err(|e| VmError::type_error(format!("Failed to get date: {:?}", e)))?;

    let s = date.to_ixdtf_string(DisplayCalendar::Auto);

    Ok(Value::string(JsString::intern(&s)))
}

fn now_plain_time_iso(_args: &[Value]) -> Result<Value, VmError> {
    let time = Temporal::now()
        .plain_time_with_provider(None, &*COMPILED_TZ_PROVIDER)
        .map_err(|e| VmError::type_error(format!("Failed to get time: {:?}", e)))?;

    let s = time.to_ixdtf_string(ToStringRoundingOptions::default())
        .map_err(|e| VmError::type_error(format!("Failed to format: {:?}", e)))?;

    Ok(Value::string(JsString::intern(&s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_now_instant() {
        let result = now_instant(&[]).unwrap();
        let s = result.as_string().unwrap().to_string();
        let nanos: i128 = s.parse().unwrap();
        assert!(nanos > 1_700_000_000_000_000_000);
    }

    #[test]
    fn test_now_timezone_id() {
        let result = now_timezone_id(&[]).unwrap();
        let tz = result.as_string().unwrap().to_string();
        assert!(!tz.is_empty());
    }
}
