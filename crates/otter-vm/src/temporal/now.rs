//! `Temporal.Now` — read-only views of the host clock.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-now-object>

#![allow(missing_docs)]

use crate::ValueWindow;
use crate::temporal::helpers::{
    arg_or_undef, js_string_value, make_temporal, parse_time_zone, temporal_err,
};
use crate::temporal::payload::TemporalPayload;
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "Temporal.Now";

pub fn instant(ctx: &mut NativeCtx<'_>, _args: ValueWindow<'_>) -> Result<Value, NativeError> {
    let inst = crate::temporal::host::now()
        .instant()
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Instant(inst))
}

pub fn time_zone_id(ctx: &mut NativeCtx<'_>, _args: ValueWindow<'_>) -> Result<Value, NativeError> {
    let tz = crate::temporal::host::now()
        .time_zone()
        .map_err(|e| temporal_err(e, CLASS))?;
    let id = tz.identifier().map_err(|e| temporal_err(e, CLASS))?;
    js_string_value(id, ctx)
}

pub fn zoned_date_time_iso(
    ctx: &mut NativeCtx<'_>,
    args: ValueWindow<'_>,
) -> Result<Value, NativeError> {
    let tz = optional_time_zone(ctx, args)?;
    let zdt = crate::temporal::host::now()
        .zoned_date_time_iso(tz)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::ZonedDateTime(zdt))
}

/// Parse the optional `temporalTimeZoneLike` first argument shared by the
/// `Temporal.Now.plain*ISO` methods. `undefined` selects the system zone;
/// any other value is validated through [`parse_time_zone`], so a
/// wrong-type argument throws `TypeError` and a malformed identifier throws
/// `RangeError` before the host clock is read.
fn optional_time_zone(
    ctx: &mut NativeCtx<'_>,
    args: ValueWindow<'_>,
) -> Result<Option<temporal_rs::TimeZone>, NativeError> {
    let arg = arg_or_undef(args, 0);
    if arg.is_undefined() {
        Ok(None)
    } else {
        Ok(Some(parse_time_zone(&arg, ctx.heap(), CLASS)?))
    }
}

pub fn plain_date_time_iso(
    ctx: &mut NativeCtx<'_>,
    args: ValueWindow<'_>,
) -> Result<Value, NativeError> {
    let tz = optional_time_zone(ctx, args)?;
    let pdt = crate::temporal::host::now()
        .plain_date_time_iso(tz)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDateTime(pdt))
}

pub fn plain_date_iso(
    ctx: &mut NativeCtx<'_>,
    args: ValueWindow<'_>,
) -> Result<Value, NativeError> {
    let tz = optional_time_zone(ctx, args)?;
    let pd = crate::temporal::host::now()
        .plain_date_iso(tz)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDate(pd))
}

pub fn plain_time_iso(
    ctx: &mut NativeCtx<'_>,
    args: ValueWindow<'_>,
) -> Result<Value, NativeError> {
    let tz = optional_time_zone(ctx, args)?;
    let pt = crate::temporal::host::now()
        .plain_time_iso(tz)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainTime(pt))
}
