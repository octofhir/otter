//! RegExp replacement from rooted, borrowed string code units.
//!
//! # Contents
//! - Collect match ranges before running any functional replacer.
//! - Assemble string templates while borrowing subject and template together.
//! - Copy only selected subject spans around collecting callbacks.
//! - Build the final result in an owned, width-preserving Rust buffer.
//!
//! # Invariants
//! - A string borrow never spans JavaScript, VM allocation or result publication.
//! - Match records and the output buffer own their data across callbacks.
//! - Every callback argument and subject is held in a native handle scope.
//! - Substitution order, legacy statics and abrupt completion remain observable.
//!
//! # See also
//! - `super` owns the pristine receiver proof and shared substring builder.
//! - `crate::regexp_prototype` owns the general replacement protocol.

use crate::runtime_cx::{NativeCtx, NativeScope};
use crate::string::CodeUnits;
use crate::{Local, NativeError, Value};

use super::slice;

#[cfg(test)]
#[path = "replace_tests.rs"]
mod tests;

const NAME: &str = "RegExp.prototype[@@replace]";

/// `RegExp.prototype[@@replace]` for a pristine, non-sticky receiver after
/// subject and replacement coercion. Matches precede every replacer call.
pub(crate) fn replace(
    ctx: &mut NativeCtx<'_>,
    receiver: Value,
    input: Value,
    replacement: Value,
    functional: bool,
) -> Result<Value, NativeError> {
    ctx.scope(|mut scope| {
        let receiver = scope.value(receiver);
        let input = scope.value(input);
        let replacement = scope.value(replacement);
        // Flatten once; failure leaves a correct, less direct code-unit view.
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
        let out = if functional {
            build_functional(&mut scope, replacement, input, &matches, named)?
        } else {
            let subject = scope.raw(input);
            let template = scope.raw(replacement);
            let heap = scope.context().heap();
            let subject = subject
                .as_string(heap)
                .expect("replace subject is a string");
            let template = template
                .as_string(heap)
                .expect("replace template is a string");
            subject.with_code_units(heap, |subject| {
                template.with_code_units(heap, |template| {
                    build_template(subject, template, &matches, named)
                })
            })?
        };
        let result = out.finish(scope.context().heap_mut())?;
        let result = scope.value(Value::string(result));
        Ok(scope.finish(result))
    })
}

/// Build from immutable borrowed inputs; only Rust-owned output may escape.
fn build_template(
    subject: CodeUnits<'_>,
    template: CodeUnits<'_>,
    matches: &crate::regexp::engine::MatchBatch,
    named: bool,
) -> Result<UnitSink, NativeError> {
    let mut out = UnitSink::with_capacity(subject.len());
    let mut next = 0;
    for matched in matches.iter() {
        let position = matched.range.start;
        if position >= next {
            out.extend(subject.slice(next..position));
        }
        let start = out.len();
        if !expand_template(&mut out, template, subject, &matched, named) {
            return Err(invalid_length());
        }
        if position >= next {
            next = matched.range.end;
        } else {
            out.truncate(start);
        }
        if out.len() > MAX_STRING_UNITS {
            return Err(invalid_length());
        }
    }
    out.extend(subject.slice(next.min(subject.len())..subject.len()));
    if out.len() > MAX_STRING_UNITS {
        return Err(invalid_length());
    }
    Ok(out)
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

fn invalid_length() -> NativeError {
    NativeError::RangeError {
        name: NAME,
        reason: "Invalid string length".to_string(),
    }
}

/// A result string assembled one byte per unit until a unit past Latin-1
/// arrives, then widened once.
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

    fn push(&mut self, unit: u16) {
        match self {
            Self::Latin1(bytes) if unit <= 0xff => bytes.push(unit as u8),
            _ => self.widen().push(unit),
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

/// Longest string `@@replace` builds before reporting a RangeError.
const MAX_STRING_UNITS: usize = 1 << 29;

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

/// §22.2.6.11.1 `GetSubstitution` over code-unit ranges of `subject`;
/// `false` once the expansion passes the longest string a result can be.
fn expand_template(
    out: &mut UnitSink,
    template: CodeUnits<'_>,
    subject: CodeUnits<'_>,
    matched: &crate::regexp::engine::Match<'_>,
    named: bool,
) -> bool {
    const DOLLAR: u16 = b'$' as u16;
    let digit = |unit: u16| {
        (b'0' as u16..=b'9' as u16)
            .contains(&unit)
            .then(|| usize::from(unit - b'0' as u16))
    };
    let groups = matched.captures.len();
    let tail = matched.range.end.min(subject.len());
    let mut i = 0;
    while let Some(unit) = template.get(i) {
        if out.len() > MAX_STRING_UNITS {
            return false;
        }
        let Some(next) = template.get(i + 1).filter(|_| unit == DOLLAR) else {
            out.push(unit);
            i += 1;
            continue;
        };
        match next {
            DOLLAR => {
                out.push(DOLLAR);
                i += 2;
            }
            0x26 => {
                out.extend(subject.slice(matched.range.clone()));
                i += 2;
            }
            0x60 => {
                out.extend(subject.slice(0..matched.range.start));
                i += 2;
            }
            0x27 => {
                out.extend(subject.slice(tail..subject.len()));
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
                match index {
                    Some(index) => {
                        if let Some(range) = matched.captures.get(index - 1) {
                            out.extend(subject.slice(range.clone()));
                        }
                    }
                    None => out.extend(template.slice(i..i + consumed)),
                }
                i += consumed;
            }
            0x3c if named => {
                let close = (i + 2..template.len()).find(|&j| template.get(j) == Some(b'>' as u16));
                let Some(close) = close else {
                    out.push(DOLLAR);
                    i += 1;
                    continue;
                };
                let name: String =
                    char::decode_utf16((i + 2..close).filter_map(|j| template.get(j)))
                        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
                        .collect();
                if let Some((_, Some(range))) =
                    matched.named_groups().find(|(group, _)| *group == name)
                {
                    out.extend(subject.slice(range));
                }
                i = close + 1;
            }
            _ => {
                out.push(DOLLAR);
                out.push(next);
                i += 2;
            }
        }
    }
    out.len() <= MAX_STRING_UNITS
}
