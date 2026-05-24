//! `Temporal.Now` — read-only views of the host clock.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-now-object>

#![allow(missing_docs)]


use crate::temporal::helpers::{make_temporal, temporal_err};
use crate::temporal::payload::TemporalPayload;
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "Temporal.Now";

pub fn instant(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let inst = temporal_rs::sys::Temporal::local_now()
        .instant()
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Instant(inst))
}

pub fn plain_date_time_iso(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let pdt = temporal_rs::sys::Temporal::local_now()
        .plain_date_time_iso(None)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDateTime(pdt))
}

pub fn plain_date_iso(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let pd = temporal_rs::sys::Temporal::local_now()
        .plain_date_iso(None)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDate(pd))
}

pub fn plain_time_iso(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let pt = temporal_rs::sys::Temporal::local_now()
        .plain_time_iso(None)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainTime(pt))
}
