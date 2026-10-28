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
        let mut ranges: SmallVec<[(u32, u32); 16]> = SmallVec::new();
        let regexp = scope.raw(receiver).as_regexp().expect("pristine RegExp");
        let subject = scope
            .raw(input)
            .as_string(scope.context().heap())
            .expect("match subject is a string");
        let input_value = scope.raw(input);
        let interp = scope.context().interp_mut();
        let step_limit = interp.regex_step_limit();
        // SAFETY: the pattern record is shared, never moves, and the body
        // keeps it alive; nothing collects before the stream has ended.
        let regex = unsafe { &*regexp.regex_ptr(&interp.gc_heap) };
        let scratch = std::mem::take(interp.regex_scratch.get_mut());
        regexp.set_last_index(&interp.gc_heap, 0);
        let (scratch, steps, outcome) = subject.with_code_units(&interp.gc_heap, |units| {
            let mut stream = crate::regexp::engine::MatchStream::new(
                regex, units, 0, false, step_limit, scratch,
            );
            let mut outcome = Ok(());
            loop {
                match stream.next() {
                    None => break,
                    Some(Err(error)) => {
                        outcome = Err(error);
                        break;
                    }
                    Some(Ok(slots)) => {
                        let range = slots.range();
                        ranges.push((range.start as u32, range.end as u32));
                    }
                }
            }
            let (scratch, steps) = stream.finish();
            (scratch, steps, outcome)
        });
        *interp.regex_scratch.get_mut() = scratch;
        let found = crate::regexp::finish_execution(
            scope.context(),
            crate::regexp::engine::Execution {
                result: outcome.map(|()| ranges.len()),
                steps,
            },
        )?;
        if found == 0 {
            scope
                .context()
                .interp_mut()
                .regex_scratch
                .get_mut()
                .release_oversized();
            return Ok(Value::null());
        }
        let interp = scope.context().interp_mut();
        if regexp.legacy_features_enabled(&interp.gc_heap) {
            let slots = interp.regex_scratch.get_mut().last_match(regex);
            interp.regexp_legacy.record_slots(input_value, slots);
        }
        interp.regex_scratch.get_mut().release_oversized();
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
