//! `@@match` and `@@search` of a pristine receiver, matched in place.
//!
//! # Contents
//! - [`match_pristine`] — `@@match` by the receiver's own flags.
//! - [`match_global`] — every match of a global, non-sticky receiver as an
//!   array of substrings, the ranges found before any substring exists.
//! - [`search`] — the first match's index, `lastIndex` left as it was.
//!
//! # Invariants
//! - The matcher borrows the subject only while no VM allocation runs.
//! - `lastIndex` and the legacy statics end exactly as the spec ladder's
//!   `RegExpExec` calls leave them: `@@match` leaves `lastIndex` zero,
//!   `@@search` restores it, and both record the last successful match.
//!
//! # See also
//! - `super` owns the pristine receiver proof.
//! - `crate::regexp_prototype` owns the general ladders.

use smallvec::SmallVec;

use crate::runtime_cx::NativeCtx;
use crate::{NativeError, Value};

use super::slice;

/// `RegExp.prototype[@@match]` of a pristine `receiver` over the string
/// `input`, or `None` for a global sticky receiver, whose chained sticky
/// matches the general ladder completes.
pub(crate) fn match_pristine(
    ctx: &mut NativeCtx<'_>,
    receiver: Value,
    input: Value,
) -> Result<Option<Value>, NativeError> {
    let flags = receiver
        .as_regexp()
        .expect("pristine RegExp")
        .flags(ctx.heap());
    if !flags.global {
        let input = input
            .as_string(ctx.heap())
            .expect("match subject is a string");
        return crate::regexp_prototype::exec_once_native(&receiver, input, ctx).map(Some);
    }
    if flags.sticky {
        return Ok(None);
    }
    match_global(ctx, receiver, input).map(Some)
}

/// `RegExp.prototype[@@match]` of a pristine global, non-sticky `receiver`
/// over the string `input`: `null` without a match.
fn match_global(
    ctx: &mut NativeCtx<'_>,
    receiver: Value,
    input: Value,
) -> Result<Value, NativeError> {
    ctx.scope(|mut scope| {
        let receiver = scope.value(receiver);
        let input = scope.value(input);
        let _ = scope
            .raw(input)
            .as_string(scope.context().heap())
            .expect("match subject is a string")
            .flatten_in_place(scope.context().heap_mut());
        let mut last = std::mem::take(&mut scope.context().interp_mut().regexp_offsets);
        let mut ranges: SmallVec<[(u32, u32); 16]> = SmallVec::new();
        let regexp = scope.raw(receiver).as_regexp().expect("pristine RegExp");
        let subject = scope
            .raw(input)
            .as_string(scope.context().heap())
            .expect("match subject is a string");
        let context = scope.context();
        let search = context.regex_search();
        let heap = context.heap();
        regexp.set_last_index(heap, 0);
        let execution = subject.with_code_units(heap, |units| {
            regexp.for_each_match(heap, units, 0, usize::MAX, &search, |matched| {
                ranges.push((matched.range.start as u32, matched.range.end as u32));
                record_offsets(&mut last, matched);
                true
            })
        });
        let found = crate::regexp::finish_execution(scope.context(), execution);
        if !matches!(found, Ok(count) if count > 0) {
            scope.context().interp_mut().regexp_offsets = last;
            return found.map(|_| Value::null());
        }
        let last = record_legacy(&mut scope, receiver, input, last);
        scope.context().interp_mut().regexp_offsets = last;
        let mut pieces = SmallVec::<[crate::Local<'_>; 16]>::with_capacity(ranges.len());
        for &(start, end) in &ranges {
            pieces.push(slice(&mut scope, input, start as usize..end as usize)?);
        }
        let array = scope.array_from(&pieces)?;
        Ok(scope.finish(array))
    })
}

/// `RegExp.prototype[@@search]` of a pristine `receiver` over the string
/// `input`: the first match's start from offset zero, or `-1`.
pub(crate) fn search(
    ctx: &mut NativeCtx<'_>,
    receiver: Value,
    input: Value,
) -> Result<Value, NativeError> {
    ctx.scope(|mut scope| {
        let receiver = scope.value(receiver);
        let input = scope.value(input);
        let _ = scope
            .raw(input)
            .as_string(scope.context().heap())
            .expect("search subject is a string")
            .flatten_in_place(scope.context().heap_mut());
        let mut offsets = std::mem::take(&mut scope.context().interp_mut().regexp_offsets);
        let regexp = scope.raw(receiver).as_regexp().expect("pristine RegExp");
        let subject = scope
            .raw(input)
            .as_string(scope.context().heap())
            .expect("search subject is a string");
        let context = scope.context();
        let search = context.regex_search();
        let heap = context.heap();
        let execution = subject.with_code_units(heap, |units| {
            regexp.find_first_offsets(heap, units, 0, &search, &mut offsets)
        });
        let found = crate::regexp::finish_execution(scope.context(), execution);
        if !matches!(found, Ok(true)) {
            scope.context().interp_mut().regexp_offsets = offsets;
            return found.map(|_| Value::number_i32(-1));
        }
        let index = offsets[0];
        let offsets = record_legacy(&mut scope, receiver, input, offsets);
        scope.context().interp_mut().regexp_offsets = offsets;
        Ok(Value::number_f64(f64::from(index)))
    })
}

/// Overwrite `out` with `matched`'s group offsets, group zero first.
pub(super) fn record_offsets(out: &mut Vec<u32>, matched: &crate::regexp::engine::Match<'_>) {
    use crate::regexp::engine::NO_OFFSET;
    out.clear();
    out.extend_from_slice(&[matched.range.start as u32, matched.range.end as u32]);
    for capture in matched.captures.iter() {
        let (start, end) = capture.map_or((NO_OFFSET, NO_OFFSET), |range| {
            (range.start as u32, range.end as u32)
        });
        out.extend_from_slice(&[start, end]);
    }
}

/// Record the last successful match's `offsets` in the legacy statics when
/// the receiver keeps them, handing the buffer back.
pub(super) fn record_legacy(
    scope: &mut crate::runtime_cx::NativeScope<'_, '_>,
    receiver: crate::Local<'_>,
    input: crate::Local<'_>,
    offsets: Vec<u32>,
) -> Vec<u32> {
    let regexp = scope.raw(receiver).as_regexp().expect("pristine RegExp");
    if !regexp.legacy_features_enabled(scope.context().heap()) {
        return offsets;
    }
    let subject = scope.raw(input);
    let offsets = crate::regexp::engine::MatchOffsets(offsets);
    scope.with_turn_parts(|interp, _| interp.regexp_legacy.record_offsets(subject, &offsets));
    offsets.0
}
