//! RegExp replacement as a splice of subject and template runs.
//!
//! # Contents
//! - [`replace()`] — `@@replace` of a pristine receiver: matches visited in
//!   place, the result described as runs of the subject and the template and
//!   built with one exact-size allocation.
//! - [`replace_direct`] — the same core with no native frame or handle
//!   scope, for generated code and the String method's own fast path.
//! - `expand_template` — `GetSubstitution` as runs, never as copied units.
//! - The functional replacer path: matches collected before any callback,
//!   callback results assembled in an owned width-preserving buffer.
//!
//! # Invariants
//! - A string borrow never spans JavaScript, VM allocation or result publication.
//! - No match leaves the subject itself as the result: nothing is allocated.
//! - Runs are offsets into the rooted subject and template, so a collection
//!   during the result allocation moves nothing they describe.
//! - Match records and the output buffer own their data across callbacks.
//! - Every callback argument and subject is held in a native handle scope.
//! - Substitution order, `lastIndex`, legacy statics and abrupt completion
//!   remain observable exactly as the spec ladder leaves them.
//!
//! # See also
//! - `super` owns the pristine receiver proof and shared substring builder.
//! - `crate::string::gc_body::splice_string_body` builds the result.
//! - `crate::regexp_prototype` owns the general replacement protocol.

use smallvec::SmallVec;

use crate::regexp::engine::MatchStream;
use crate::runtime_cx::{NativeCtx, NativeScope};
use crate::string::CodeUnits;
use crate::string::gc_body::SpliceRun;
use crate::{Local, NativeError, Value};

use super::slice;

#[cfg(test)]
#[path = "replace_tests.rs"]
mod tests;

const NAME: &str = "RegExp.prototype[@@replace]";

/// Splice source index of the subject.
const SUBJECT: u8 = 0;
/// Splice source index of the replacement template.
const TEMPLATE: u8 = 1;

/// Longest string `@@replace` builds before reporting a RangeError.
const MAX_STRING_UNITS: usize = 1 << 29;

/// `RegExp.prototype[@@replace]` for a pristine, non-sticky receiver after
/// subject and replacement coercion. Matches precede every replacer call.
pub(crate) fn replace(
    ctx: &mut NativeCtx<'_>,
    receiver: Value,
    input: Value,
    replacement: Value,
    functional: bool,
) -> Result<Value, NativeError> {
    if functional {
        return replace_functional(ctx, receiver, input, replacement);
    }
    ctx.scope(|mut scope| {
        let receiver = scope.value(receiver);
        let input = scope.value(input);
        let template = scope.value(replacement);
        for string in [input, template] {
            scope
                .raw(string)
                .as_string(scope.context().heap())
                .expect("replace operand is a string")
                .flatten_in_place(scope.context().heap_mut())?;
        }
        let (receiver_value, input_value, template_value) =
            (scope.raw(receiver), scope.raw(input), scope.raw(template));
        let interp = scope.context().interp_mut();
        let result = match replace_direct(interp, receiver_value, input_value, template_value) {
            Some(Ok(result)) => result,
            Some(Err(error)) => {
                return Err(crate::native_function::vm_to_native_error(
                    interp, error, NAME,
                ));
            }
            // Contiguous operands decline only a result past the longest string.
            None => return Err(invalid_length()),
        };
        let result = scope.value(result);
        Ok(scope.finish(result))
    })
}

/// `RegExp.prototype[@@replace]` with the string `template` of a `receiver`
/// the caller proved pristine and non-sticky, over the string `input`,
/// without a native frame or handle scope. `None`, before any observable
/// effect, unless the receiver is a non-sticky RegExp and both strings read
/// contiguously, or when the result would pass the longest string. Only the
/// result allocation can collect, and it roots both strings itself.
pub(crate) fn replace_direct(
    interp: &mut crate::Interpreter,
    receiver: Value,
    input: Value,
    template: Value,
) -> Option<Result<Value, crate::VmError>> {
    let regexp = receiver.as_regexp()?;
    let heap = &interp.gc_heap;
    let (subject, replacement) = (input.as_string(heap)?, template.as_string(heap)?);
    let flags = regexp.flags(heap);
    if flags.sticky || !subject.is_contiguous(heap) || !replacement.is_contiguous(heap) {
        return None;
    }
    let step_limit = interp.regex_step_limit();
    // SAFETY: the pattern record is shared, never moves, and the body keeps
    // it alive; nothing collects before the stream has ended.
    let regex = unsafe { &*regexp.regex_ptr(heap) };
    let names = regex.group_names();
    let scratch = std::mem::take(interp.regex_scratch.get_mut());
    let mut runs: SmallVec<[SpliceRun; 16]> = SmallVec::new();
    if flags.global {
        regexp.set_last_index(heap, 0);
    }
    let size = subject.len() as usize;
    let max_matches = if flags.global { usize::MAX } else { 1 };
    let mut next = 0usize;
    let mut found = 0usize;
    let (scratch, steps, outcome) = subject.with_code_units(heap, |units| {
        replacement.with_code_units(heap, |template| {
            let plain = !template.contains(u16::from(b'$'));
            let mut stream = MatchStream::new(regex, units, 0, false, step_limit, scratch);
            let mut outcome = Ok(());
            while found < max_matches {
                let slots = match stream.next() {
                    None => break,
                    Some(Err(error)) => {
                        outcome = Err(error);
                        break;
                    }
                    Some(Ok(slots)) => slots,
                };
                found += 1;
                let range = slots.range();
                let position = range.start;
                if position > next {
                    runs.push(run(SUBJECT, next..position));
                }
                let mark = runs.len();
                if plain {
                    if !template.is_empty() {
                        runs.push(run(TEMPLATE, 0..template.len()));
                    }
                } else {
                    expand_template(&mut runs, template, size, slots, names);
                }
                if position >= next {
                    next = range.end;
                } else {
                    runs.truncate(mark);
                }
            }
            let (scratch, steps) = stream.finish();
            (scratch, steps, outcome)
        })
    });
    *interp.regex_scratch.get_mut() = scratch;
    if let Err(error) = interp.charge_regex_backtrack_steps(steps) {
        return Some(Err(error));
    }
    if outcome.is_err() {
        return Some(Err(interp.err_budget(
            "work budget exceeded in a regular expression search".into(),
        )));
    }
    if found == 0 {
        return Some(Ok(input));
    }
    if next < size {
        runs.push(run(SUBJECT, next..size));
    }
    let total: usize = runs.iter().map(|run| run.len as usize).sum();
    if total > MAX_STRING_UNITS {
        return None;
    }
    if regexp.legacy_features_enabled(&interp.gc_heap) {
        let slots = interp.regex_scratch.get_mut().last_match(regex);
        interp.regexp_legacy.record_slots(input, slots);
    }
    interp.regex_scratch.get_mut().release_oversized();
    let mut sources = [subject.handle(), replacement.handle()];
    let heap = &mut interp.gc_heap;
    Some(
        crate::string::gc_body::splice_string_body(heap, &mut sources, &runs, &mut |_| {})
            .map(|result| Value::string(crate::string::JsString::from_handle(result, heap)))
            .map_err(crate::VmError::from),
    )
}

fn run(source: u8, range: std::ops::Range<usize>) -> SpliceRun {
    SpliceRun {
        source,
        start: range.start as u32,
        len: (range.end - range.start) as u32,
    }
}

/// §22.2.6.11.1 `GetSubstitution` for one match as runs: literal template
/// spans stay template runs and every substitution is a subject run.
fn expand_template(
    runs: &mut SmallVec<[SpliceRun; 16]>,
    template: CodeUnits<'_>,
    subject_len: usize,
    matched: otter_regex::MatchSlots<'_>,
    names: &[String],
) {
    const DOLLAR: u16 = b'$' as u16;
    let digit = |unit: u16| {
        (u16::from(b'0')..=u16::from(b'9'))
            .contains(&unit)
            .then(|| usize::from(unit - u16::from(b'0')))
    };
    let groups = matched.groups() - 1;
    let range = matched.range();
    let tail = range.end.min(subject_len);
    let named = names.iter().any(|name| !name.is_empty());
    // The literal template span not yet emitted starts here.
    let mut literal = 0;
    let mut i = 0;
    let substitute = |runs: &mut SmallVec<[SpliceRun; 16]>,
                      literal: &mut usize,
                      at: usize,
                      resume: usize,
                      piece: Option<std::ops::Range<usize>>| {
        if at > *literal {
            runs.push(run(TEMPLATE, *literal..at));
        }
        if let Some(piece) = piece.filter(|piece| !piece.is_empty()) {
            runs.push(run(SUBJECT, piece));
        }
        *literal = resume;
    };
    while let Some(unit) = template.get(i) {
        let Some(next) = template.get(i + 1).filter(|_| unit == DOLLAR) else {
            i += 1;
            continue;
        };
        match next {
            // `$$`: the second dollar is the literal kept.
            DOLLAR => {
                substitute(runs, &mut literal, i, i + 1, None);
                i += 2;
            }
            0x26 => {
                substitute(runs, &mut literal, i, i + 2, Some(range.clone()));
                i += 2;
            }
            0x60 => {
                substitute(runs, &mut literal, i, i + 2, Some(0..range.start));
                i += 2;
            }
            0x27 => {
                substitute(runs, &mut literal, i, i + 2, Some(tail..subject_len));
                i += 2;
            }
            _ if digit(next).is_some() => {
                let first = digit(next).expect("digit");
                let second = template.get(i + 2).and_then(digit);
                let group = |index: usize| (index > 0 && index <= groups).then_some(index);
                let (index, consumed) = match second.map(|second| first * 10 + second) {
                    Some(two) if group(two).is_some() => (Some(two), 3),
                    _ => (group(first), 2),
                };
                if let Some(index) = index {
                    substitute(runs, &mut literal, i, i + consumed, matched.group(index));
                }
                i += consumed;
            }
            0x3c if named => {
                let close =
                    (i + 2..template.len()).find(|&j| template.get(j) == Some(u16::from(b'>')));
                let Some(close) = close else {
                    i += 1;
                    continue;
                };
                let name: String =
                    char::decode_utf16((i + 2..close).filter_map(|j| template.get(j)))
                        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
                        .collect();
                let piece = named_group(matched, names, &name);
                substitute(runs, &mut literal, i, close + 1, piece);
                i = close + 1;
            }
            _ => i += 2,
        }
    }
    if template.len() > literal {
        runs.push(run(TEMPLATE, literal..template.len()));
    }
}

/// The range of the group named `name` in `matched`, preferring an
/// alternative that participated; `None` for no such name or one that did
/// not participate.
fn named_group(
    matched: otter_regex::MatchSlots<'_>,
    names: &[String],
    name: &str,
) -> Option<std::ops::Range<usize>> {
    let mut first = None;
    for (index, candidate) in names.iter().enumerate() {
        if candidate == name {
            if let Some(range) = matched.group(index + 1) {
                return Some(range);
            }
            first.get_or_insert(index + 1);
        }
    }
    first.and_then(|index| matched.group(index))
}

fn invalid_length() -> NativeError {
    NativeError::RangeError {
        name: NAME,
        reason: "Invalid string length".to_string(),
    }
}

/// `@@replace` with a callable replacer: every match is found first, then
/// each callback runs with the subject re-read after it.
fn replace_functional(
    ctx: &mut NativeCtx<'_>,
    receiver: Value,
    input: Value,
    replacement: Value,
) -> Result<Value, NativeError> {
    ctx.scope(|mut scope| {
        let receiver = scope.value(receiver);
        let input = scope.value(input);
        let replacement = scope.value(replacement);
        let _ = scope
            .raw(input)
            .as_string(scope.context().heap())
            .expect("replace subject is a string")
            .flatten_in_place(scope.context().heap_mut());
        let regexp = scope.raw(receiver).as_regexp().expect("pristine RegExp");
        let subject = scope
            .raw(input)
            .as_string(scope.context().heap())
            .expect("replace subject is a string");
        let flags = regexp.flags(scope.context().heap());
        let context = scope.context();
        let search = context.regex_search();
        let heap = context.heap();
        if flags.global {
            regexp.set_last_index(heap, 0);
        }
        let execution = subject.with_code_units(heap, |units| {
            regexp.find_batch(
                heap,
                units,
                0,
                if flags.global { usize::MAX } else { 1 },
                &search,
            )
        });
        let matches = crate::regexp::finish_execution(scope.context(), execution)?;
        if matches.is_empty() {
            return Ok(scope.finish(input));
        }
        if let Some(last) = matches.last() {
            let subject = scope.raw(input);
            scope.with_turn_parts(|interp, _| {
                interp.regexp_legacy.record_match(
                    subject,
                    last.range.clone(),
                    last.captures.iter(),
                );
            });
        }
        let named = matches
            .first()
            .is_some_and(|m| m.named_groups().next().is_some());
        let out = build_functional(&mut scope, replacement, input, &matches, named)?;
        let result = out.finish(scope.context().heap_mut())?;
        let result = scope.value(Value::string(result));
        Ok(scope.finish(result))
    })
}

/// Re-read each rooted subject span after callbacks may have moved its cell.
fn build_functional(
    scope: &mut NativeScope<'_, '_>,
    replacement: Local<'_>,
    input: Local<'_>,
    matches: &crate::regexp::engine::MatchBatch,
    named: bool,
) -> Result<UnitSink, NativeError> {
    let size = scope
        .raw(input)
        .as_string(scope.context().heap())
        .expect("replace subject is a string")
        .len() as usize;
    let mut out = UnitSink::with_capacity(size);
    let mut next = 0;
    for matched in matches.iter() {
        let position = matched.range.start;
        if position >= next {
            extend_subject(scope, input, next..position, &mut out);
        }
        let start = out.len();
        let result = call_replacer(scope, replacement, input, &matched, named)?;
        let result = scope.raw(result);
        let result =
            crate::regexp_prototype::coerce_to_jsstring_runtime(scope.context(), &result, NAME)?;
        result.with_code_units(scope.context().heap(), |units| out.extend(units));
        if position >= next {
            next = matched.range.end;
        } else {
            out.truncate(start);
        }
        if out.len() > MAX_STRING_UNITS {
            return Err(invalid_length());
        }
    }
    extend_subject(scope, input, next.min(size)..size, &mut out);
    if out.len() > MAX_STRING_UNITS {
        return Err(invalid_length());
    }
    Ok(out)
}

fn extend_subject(
    scope: &mut NativeScope<'_, '_>,
    input: Local<'_>,
    range: std::ops::Range<usize>,
    out: &mut UnitSink,
) {
    let input = scope.raw(input);
    let heap = scope.context().heap();
    input
        .as_string(heap)
        .expect("replace subject is a string")
        .with_code_units(heap, |units| out.extend(units.slice(range)));
}

/// A callback-built result assembled one byte per unit until a unit past
/// Latin-1 arrives, then widened once.
enum UnitSink {
    Latin1(Vec<u8>),
    Utf16(Vec<u16>),
}

impl UnitSink {
    fn with_capacity(capacity: usize) -> Self {
        Self::Latin1(Vec::with_capacity(capacity))
    }

    fn len(&self) -> usize {
        match self {
            Self::Latin1(bytes) => bytes.len(),
            Self::Utf16(units) => units.len(),
        }
    }

    fn truncate(&mut self, len: usize) {
        match self {
            Self::Latin1(bytes) => bytes.truncate(len),
            Self::Utf16(units) => units.truncate(len),
        }
    }

    fn widen(&mut self) -> &mut Vec<u16> {
        if let Self::Latin1(bytes) = self {
            let mut units = Vec::with_capacity(bytes.capacity().max(bytes.len() * 2));
            units.extend(bytes.iter().map(|&byte| u16::from(byte)));
            *self = Self::Utf16(units);
        }
        match self {
            Self::Utf16(units) => units,
            Self::Latin1(_) => unreachable!("widened above"),
        }
    }

    fn extend(&mut self, units: CodeUnits<'_>) {
        match (&mut *self, units) {
            (Self::Latin1(bytes), CodeUnits::Latin1(more)) => bytes.extend_from_slice(more),
            (Self::Utf16(units), CodeUnits::Latin1(more)) => {
                units.extend(more.iter().map(|&byte| u16::from(byte)));
            }
            (Self::Latin1(bytes), CodeUnits::Utf16(more)) if more.iter().all(|&u| u <= 0xff) => {
                bytes.extend(more.iter().map(|&unit| unit as u8));
            }
            (_, CodeUnits::Utf16(more)) => self.widen().extend_from_slice(more),
        }
    }

    fn finish(
        self,
        heap: &mut otter_gc::GcHeap,
    ) -> Result<crate::string::JsString, otter_gc::OutOfMemory> {
        match self {
            Self::Latin1(bytes) => crate::string::JsString::from_latin1(&bytes, heap),
            Self::Utf16(units) => crate::string::JsString::from_utf16_units(&units, heap),
        }
    }
}

/// `Call(replaceValue, undefined, « matched, ...captures, position, S [,
/// groups] »)` with every argument rooted.
fn call_replacer<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    replacer: Local<'_>,
    input: Local<'_>,
    matched: &crate::regexp::engine::Match<'_>,
    named: bool,
) -> Result<Local<'scope>, NativeError> {
    let mut args = Vec::with_capacity(matched.captures.len() + 4);
    args.push(slice(scope, input, matched.range.clone())?);
    for capture in matched.captures.iter() {
        let value = match capture {
            Some(range) => slice(scope, input, range.clone())?,
            None => scope.undefined(),
        };
        args.push(value);
    }
    args.push(scope.number(matched.range.start as f64));
    args.push(input);
    if named {
        let groups = scope.bare_object()?;
        for (name, range) in matched.named_groups() {
            let value = match range {
                Some(range) => slice(scope, input, range)?,
                None => scope.undefined(),
            };
            scope.set(groups, name, value)?;
        }
        args.push(groups);
    }
    let undefined = scope.undefined();
    scope.call(replacer, undefined, &args)
}
