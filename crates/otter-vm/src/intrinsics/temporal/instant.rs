//! Temporal.Instant constructor and prototype intrinsics.
//!
//! §8.2 Temporal.Instant
//! <https://tc39.es/proposal-temporal/#sec-temporal-instant-objects>

use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::{ObjectHandle, PropertyValue};
use crate::value::RegisterValue;

use super::helpers::{self, temporal_err, to_bigint_i128, to_string_arg};
use super::payload::{TemporalPayload, construct_temporal, require_temporal_payload};

fn tz_provider() -> &'static impl temporal_rs::provider::TimeZoneProvider {
    &*temporal_rs::provider::COMPILED_TZ_PROVIDER
}

// ── Descriptor ──────────────────────────────────────────────────────

pub fn instant_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("Instant")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "Instant",
            1,
            instant_constructor,
        ))
        .with_binding(stat("from", 1, instant_from))
        .with_binding(stat("compare", 2, instant_compare))
        .with_binding(stat("fromEpochMilliseconds", 1, instant_from_epoch_ms))
        .with_binding(stat("fromEpochNanoseconds", 1, instant_from_epoch_ns))
        .with_binding(proto("epochMilliseconds", 0, instant_epoch_ms))
        .with_binding(proto("epochNanoseconds", 0, instant_epoch_ns))
        .with_binding(proto("add", 1, instant_add))
        .with_binding(proto("subtract", 1, instant_subtract))
        .with_binding(proto("equals", 1, instant_equals))
        .with_binding(proto("toString", 0, instant_to_string))
        .with_binding(proto("toJSON", 0, instant_to_json))
        .with_binding(proto("valueOf", 0, helpers::temporal_value_of))
}

fn proto(
    name: &str,
    arity: u16,
    f: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
) -> NativeBindingDescriptor {
    NativeBindingDescriptor::new(
        NativeBindingTarget::Prototype,
        NativeFunctionDescriptor::method(name, arity, f),
    )
}

fn stat(
    name: &str,
    arity: u16,
    f: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
) -> NativeBindingDescriptor {
    NativeBindingDescriptor::new(
        NativeBindingTarget::Constructor,
        NativeFunctionDescriptor::method(name, arity, f),
    )
}

// ── Helpers ─────────────────────────────────────────────────────────

fn require_instant(
    this: &RegisterValue,
    runtime: &crate::interpreter::RuntimeState,
) -> Result<temporal_rs::Instant, VmNativeCallError> {
    let payload = require_temporal_payload(this, runtime)
        .map_err(|_| VmNativeCallError::Internal("expected Temporal.Instant".into()))?;
    payload
        .as_instant()
        .copied()
        .ok_or_else(|| VmNativeCallError::Internal("expected Temporal.Instant".into()))
}

fn wrap_instant(
    instant: temporal_rs::Instant,
    runtime: &mut crate::interpreter::RuntimeState,
) -> RegisterValue {
    let proto = runtime.intrinsics().temporal_instant_prototype();
    let handle = construct_temporal(TemporalPayload::Instant(instant), proto, runtime);
    RegisterValue::from_object_handle(handle.0)
}

/// Extracts an Instant from an argument — accepts Instant objects or ISO strings.
fn to_instant(
    val: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<temporal_rs::Instant, VmNativeCallError> {
    // Try as Temporal.Instant payload first.
    if let Some(handle) = val.as_object_handle().map(ObjectHandle)
        && let Ok(payload) = runtime.native_payload::<TemporalPayload>(handle)
        && let Some(instant) = payload.as_instant()
    {
        return Ok(*instant);
    }
    // Fall back to ISO string parsing.
    let s = runtime
        .js_to_string(val)
        .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?;
    temporal_rs::Instant::from_utf8(s.as_bytes()).map_err(|e| temporal_err(e, runtime))
}

// ── Constructor ─────────────────────────────────────────────────────

/// §8.2.1 new Temporal.Instant ( epochNanoseconds )
/// <https://tc39.es/proposal-temporal/#sec-temporal.instant>
fn instant_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ns = to_bigint_i128(args, 0, runtime)?;
    let instant = temporal_rs::Instant::try_new(ns).map_err(|e| temporal_err(e, runtime))?;
    Ok(wrap_instant(instant, runtime))
}

// ── Static methods ──────────────────────────────────────────────────

/// §8.2.2.1 Temporal.Instant.from ( item )
/// <https://tc39.es/proposal-temporal/#sec-temporal.instant.from>
fn instant_from(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let instant = to_instant(val, runtime)?;
    Ok(wrap_instant(instant, runtime))
}

/// §8.2.2.2 Temporal.Instant.compare ( one, two )
/// <https://tc39.es/proposal-temporal/#sec-temporal.instant.compare>
fn instant_compare(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let one = args.first().copied().unwrap_or(RegisterValue::undefined());
    let two = args.get(1).copied().unwrap_or(RegisterValue::undefined());
    let a = to_instant(one, runtime)?;
    let b = to_instant(two, runtime)?;
    let cmp = a.epoch_nanoseconds().0.cmp(&b.epoch_nanoseconds().0);
    Ok(RegisterValue::from_i32(cmp as i32))
}

/// §8.2.2.3 Temporal.Instant.fromEpochMilliseconds ( epochMilliseconds )
/// <https://tc39.es/proposal-temporal/#sec-temporal.instant.fromepochmilliseconds>
fn instant_from_epoch_ms(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let ms = runtime
        .js_to_number(val)
        .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?;
    let ns = (ms as i128) * 1_000_000;
    let instant = temporal_rs::Instant::try_new(ns).map_err(|e| temporal_err(e, runtime))?;
    Ok(wrap_instant(instant, runtime))
}

/// §8.2.2.4 Temporal.Instant.fromEpochNanoseconds ( epochNanoseconds )
/// <https://tc39.es/proposal-temporal/#sec-temporal.instant.fromepochnanoseconds>
fn instant_from_epoch_ns(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ns = to_bigint_i128(args, 0, runtime)?;
    let instant = temporal_rs::Instant::try_new(ns).map_err(|e| temporal_err(e, runtime))?;
    Ok(wrap_instant(instant, runtime))
}

// ── Prototype methods ───────────────────────────────────────────────

/// §8.2.3.2 get Temporal.Instant.prototype.epochMilliseconds
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.instant.prototype.epochmilliseconds>
fn instant_epoch_ms(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let instant = require_instant(this, runtime)?;
    Ok(RegisterValue::from_number(
        instant.epoch_milliseconds() as f64
    ))
}

/// §8.2.3.3 get Temporal.Instant.prototype.epochNanoseconds
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.instant.prototype.epochnanoseconds>
fn instant_epoch_ns(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let instant = require_instant(this, runtime)?;
    let ns = instant.epoch_nanoseconds().0;
    let handle = runtime.alloc_bigint(&ns.to_string());
    Ok(RegisterValue::from_bigint_handle(handle.0))
}

/// §8.2.3.5 Temporal.Instant.prototype.add ( duration )
/// <https://tc39.es/proposal-temporal/#sec-temporal.instant.prototype.add>
fn instant_add(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let instant = require_instant(this, runtime)?;
    let dur_val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let dur = to_duration(dur_val, runtime)?;
    let result = instant.add(&dur).map_err(|e| temporal_err(e, runtime))?;
    Ok(wrap_instant(result, runtime))
}

/// §8.2.3.6 Temporal.Instant.prototype.subtract ( duration )
/// <https://tc39.es/proposal-temporal/#sec-temporal.instant.prototype.subtract>
fn instant_subtract(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let instant = require_instant(this, runtime)?;
    let dur_val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let dur = to_duration(dur_val, runtime)?;
    let result = instant
        .subtract(&dur)
        .map_err(|e| temporal_err(e, runtime))?;
    Ok(wrap_instant(result, runtime))
}

/// §8.2.3.10 Temporal.Instant.prototype.equals ( other )
/// <https://tc39.es/proposal-temporal/#sec-temporal.instant.prototype.equals>
fn instant_equals(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let instant = require_instant(this, runtime)?;
    let other_val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let other = to_instant(other_val, runtime)?;
    Ok(RegisterValue::from_bool(
        instant.epoch_nanoseconds() == other.epoch_nanoseconds(),
    ))
}

/// §8.2.3.11 Temporal.Instant.prototype.toString ( [ options ] )
/// <https://tc39.es/proposal-temporal/#sec-temporal.instant.prototype.tostring>
fn instant_to_string(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let instant = require_instant(this, runtime)?;
    // Parse optional timeZone from options.
    let tz_str = extract_timezone_option(args, runtime)?;

    let tz = if let Some(ref tz_id) = tz_str {
        Some(
            temporal_rs::TimeZone::try_from_identifier_str_with_provider(tz_id, tz_provider())
                .map_err(|e| temporal_err(e, runtime))?,
        )
    } else {
        None
    };

    let text = instant
        .to_ixdtf_string_with_provider(
            tz,
            temporal_rs::options::ToStringRoundingOptions::default(),
            tz_provider(),
        )
        .map_err(|e| temporal_err(e, runtime))?;

    let handle = runtime.alloc_string(text);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// Extracts an optional `timeZone` string from an options bag argument.
fn extract_timezone_option(
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Option<String>, VmNativeCallError> {
    let opts = match args.first().copied() {
        Some(v) if v != RegisterValue::undefined() => v,
        _ => return Ok(None),
    };
    let handle = match opts.as_object_handle().map(ObjectHandle) {
        Some(h) => h,
        None => return Ok(None),
    };
    let tz_prop = runtime.intern_property_name("timeZone");
    let lookup = runtime
        .objects()
        .get_property(handle, tz_prop)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let tz_val = match lookup {
        Some(pl) => match pl.value() {
            PropertyValue::Data { value, .. } => value,
            _ => return Ok(None),
        },
        None => return Ok(None),
    };
    if tz_val == RegisterValue::undefined() {
        return Ok(None);
    }
    let s = runtime
        .js_to_string(tz_val)
        .map(|s| s.into_string())
        .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?;
    Ok(Some(s))
}

/// §8.2.3.12 Temporal.Instant.prototype.toJSON ( )
/// <https://tc39.es/proposal-temporal/#sec-temporal.instant.prototype.tojson>
fn instant_to_json(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let instant = require_instant(this, runtime)?;
    let text = instant
        .to_ixdtf_string_with_provider(
            None,
            temporal_rs::options::ToStringRoundingOptions::default(),
            tz_provider(),
        )
        .map_err(|e| temporal_err(e, runtime))?;
    let handle = runtime.alloc_string(text);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ── Duration extraction helper ──────────────────────────────────────

/// Extracts a Duration from an argument — accepts Duration objects or ISO strings.
fn to_duration(
    val: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<temporal_rs::Duration, VmNativeCallError> {
    if let Some(handle) = val.as_object_handle().map(ObjectHandle)
        && let Ok(payload) = runtime.native_payload::<TemporalPayload>(handle)
        && let Some(dur) = payload.as_duration()
    {
        return Ok(*dur);
    }
    let s = to_string_arg(&[val], 0, runtime)?;
    temporal_rs::Duration::from_utf8(s.as_bytes()).map_err(|e| temporal_err(e, runtime))
}
