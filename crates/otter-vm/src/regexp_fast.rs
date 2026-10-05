//! The unmodified-RegExp protocol — V8's `IsFastRegExp`.
//!
//! RegExp built-ins observe their receiver through `exec`, the flag getters,
//! `constructor`, `@@species` and `lastIndex`. While a receiver and its
//! realm's `%RegExp.prototype%` are exactly as bootstrap left them, those
//! reads have known answers, so the built-ins run the matcher directly
//! instead of building and re-reading match arrays.
//!
//! # Contents
//! - [`RegExpProtocolProof`] — per-realm record that the prototype is built-in.
//! - [`Interpreter::is_pristine_regexp`] — a receiver qualifies.
//! - [`Interpreter::regexp_species_is_builtin`] — `@@split`'s species read.
//! - [`exec_without_result`] — `RegExpBuiltinExec` reporting only success.
//! - [`split`] — `@@split` with the receiver's own matcher as the splitter.
//! - [`replace`] — `@@replace` over match records, substituting by range.
//!
//! # Invariants
//! - The proof is the prototype-chain validity cell of `%RegExp.prototype%`,
//!   taken after verifying `exec`, the flag getters, `constructor` and the
//!   symbol methods are the built-ins. Any write to the prototype (a value,
//!   an accessor, its key set or `[[Prototype]]`) retires the cell, and the
//!   next use verifies again.
//! - A receiver qualifies with no own property beside `lastIndex`, no
//!   `[[Prototype]]` override, and a writable `lastIndex` holding a
//!   non-negative int32: reading and writing it can then run no user code.
//! - Fast bodies are observably the spec algorithms for such a receiver,
//!   including `lastIndex` writes and the legacy RegExp statics.
//!
//! # See also
//! - [`crate::regexp_prototype`] — the general spec ladders.

use std::sync::Arc;

use crate::object::prototype_validity::PrototypeValidity;
use crate::object::{self, ShapeState};
use crate::runtime_cx::{NativeCtx, NativeScope};
use crate::{Interpreter, Local, NativeError, Value};

/// Per-realm proof about `%RegExp.prototype%`, valid while its cell is.
#[derive(Debug, Clone)]
pub(crate) struct RegExpProtocolProof {
    validity: Arc<PrototypeValidity>,
    builtin: bool,
}

impl Interpreter {
    /// Whether `receiver` is a RegExp whose observable protocol is the
    /// built-in one. May allocate the prototype's instance root once.
    pub(crate) fn is_pristine_regexp(&mut self, receiver: Value) -> bool {
        let Some(regexp) = receiver.as_regexp() else {
            return false;
        };
        let heap = &self.gc_heap;
        let plain = regexp.expando(heap).is_none()
            && regexp.prototype_override(heap).is_none()
            && regexp.last_index_writable(heap)
            && regexp
                .last_index_value(heap)
                .as_i32()
                .is_some_and(|index| index >= 0);
        plain && self.regexp_prototype_is_builtin()
    }

    /// Whether `%RegExp%[@@species]` is still the built-in accessor, so
    /// `SpeciesConstructor(rx, %RegExp%)` of a pristine receiver is `%RegExp%`.
    pub(crate) fn regexp_species_is_builtin(&self) -> bool {
        let Some(prototype) = self.realm_intrinsics.regexp_prototype() else {
            return false;
        };
        let heap = &self.gc_heap;
        let object::PropertyLookup::Data { value, .. } =
            object::lookup_own(prototype, heap, "constructor")
        else {
            return false;
        };
        let species = self.well_known_symbols.get(crate::symbol::WellKnown::Species);
        value
            .as_native_function()
            .and_then(|constructor| constructor.own_symbol_property_descriptor(heap, species))
            .is_some_and(|descriptor| {
                matches!(
                    descriptor.kind,
                    object::DescriptorKind::Accessor { getter: Some(getter), .. }
                        if getter.as_native_function().is_some_and(|getter| {
                            getter.is_static_fn(
                                heap,
                                crate::intrinsics::symbol::constructor_species_get,
                            )
                        })
                )
            })
    }

    fn regexp_prototype_is_builtin(&mut self) -> bool {
        if let Some(proof) = &self.realm_intrinsics.regexp_protocol
            && proof.validity.is_valid()
        {
            return proof.builtin;
        }
        let Some(prototype) = self.realm_intrinsics.regexp_prototype() else {
            return false;
        };
        // An instance root gives the prototype its prototype role, whose
        // writes retire the proofs taken over it.
        if self
            .object_root(
                Some(prototype),
                object::DEFAULT_INLINE_CAPACITY,
                ShapeState::ORDINARY,
            )
            .is_err()
        {
            return false;
        }
        let Some(prototype) = self.realm_intrinsics.regexp_prototype() else {
            return false;
        };
        let Some(validity) =
            object::prototype_validity::chain_validity(prototype, &self.gc_heap)
        else {
            return false;
        };
        let builtin = crate::bootstrap_regexp::prototype_is_builtin(
            prototype,
            &self.gc_heap,
            &self.well_known_symbols,
        );
        self.realm_intrinsics.regexp_protocol = Some(RegExpProtocolProof { validity, builtin });
        builtin
    }
}

/// `RegExpBuiltinExec(R, S)` for a pristine `receiver`, reporting only
/// whether it matched: the `lastIndex` protocol and the legacy statics are
/// exact, and no match array is built.
pub(crate) fn exec_without_result(
    scope: &mut NativeScope<'_, '_>,
    receiver: Local<'_>,
    input: Local<'_>,
) -> Result<bool, NativeError> {
    let receiver_value = scope.raw(receiver);
    let input_value = scope.raw(input);
    let heap = scope.context().heap();
    let (Some(regexp), Some(text)) = (receiver_value.as_regexp(), input_value.as_string(heap))
    else {
        return Err(NativeError::TypeError {
            name: "RegExpBuiltinExec",
            reason: "pristine receiver and subject roots".to_string(),
        });
    };
    let flags = regexp.flags(heap);
    let tracks_last_index = flags.global || flags.sticky;
    let start = if tracks_last_index {
        regexp.last_index(heap) as usize
    } else {
        0
    };
    if start > text.len() as usize {
        regexp.set_last_index(heap, 0);
        return Ok(false);
    }
    let step_limit = scope.context().regex_step_limit();
    let heap = scope.context().heap();
    let execution = text.with_utf16(heap, |units| {
        regexp.find_one_from_utf16(heap, units, start, step_limit)
    });
    let matched = crate::regexp::finish_execution(scope.context(), execution)?;
    // Charging the work cannot run JavaScript but re-reads the roots anyway.
    let regexp = scope
        .raw(receiver)
        .as_regexp()
        .expect("pristine receiver stays a RegExp");
    let heap = scope.context().heap();
    let Some(matched) = matched.filter(|m| !flags.sticky || m.range.start == start) else {
        if tracks_last_index {
            regexp.set_last_index(heap, 0);
        }
        return Ok(false);
    };
    if tracks_last_index {
        regexp.set_last_index(heap, matched.range.end as u32);
    }
    if regexp.legacy_features_enabled(heap) {
        let subject = scope.raw(input);
        scope.with_turn_parts(|interp, _| {
            interp
                .regexp_legacy
                .record_match(subject, matched.range.clone(), &matched.captures);
        });
    }
    Ok(true)
}

/// `ToUint32(limit)` for a limit whose conversion runs no user code.
pub(crate) fn plain_split_limit(limit: Value) -> Option<u32> {
    if limit.is_undefined() {
        return Some(u32::MAX);
    }
    let limit = limit.as_number()?.as_f64();
    Some(if limit.is_finite() {
        limit.trunc().rem_euclid(4_294_967_296.0) as u32
    } else {
        0
    })
}

/// `RegExp.prototype[@@split]` for a pristine receiver whose species is
/// `%RegExp%`. The spec's splitter is a sticky copy of the receiver probed at
/// every position; the leftmost match at or after a position is exactly the
/// first position where that probe succeeds, so one search finds each piece.
/// The legacy statics end at the last successful probe, as the splitter's
/// own `exec`s leave them.
pub(crate) fn split(
    ctx: &mut NativeCtx<'_>,
    receiver: Value,
    input: Value,
    limit: u32,
) -> Result<Value, NativeError> {
    ctx.scope(|mut scope| {
        let receiver = scope.value(receiver);
        let input = scope.value(input);
        let regexp = scope.raw(receiver).as_regexp().expect("pristine RegExp");
        let subject = scope.raw(input);
        let heap = scope.context().heap();
        let flags = regexp.flags(heap);
        let unicode = flags.unicode || flags.unicode_sets;
        let units = subject
            .as_string(heap)
            .expect("split subject is a string")
            .to_utf16_vec(heap);
        let size = units.len();
        let mut pieces: Vec<Local<'_>> = Vec::new();
        let mut last_match = None;
        let search = |scope: &mut NativeScope<'_, '_>, at: usize| {
            let step_limit = scope.context().regex_step_limit();
            let regexp = scope.raw(receiver).as_regexp().expect("pristine RegExp");
            let heap = scope.context().heap();
            let execution = regexp.find_one_from_utf16(heap, &units, at, step_limit);
            crate::regexp::finish_execution(scope.context(), execution)
        };
        if limit > 0 && size == 0 {
            let matched = search(&mut scope, 0)?.filter(|m| m.range.start == 0);
            if matched.is_none() {
                pieces.push(input);
            }
            last_match = matched;
        } else if limit > 0 {
            let mut p = 0;
            let mut q = 0;
            'pieces: while q < size {
                let Some(matched) = search(&mut scope, q)? else {
                    if flags.sticky {
                        q = crate::regexp_prototype::advance_string_index(&units, q, unicode);
                        continue;
                    }
                    break;
                };
                if matched.range.start >= size {
                    break;
                }
                q = matched.range.start;
                let end = matched.range.end.min(size);
                if end == p {
                    last_match = Some(matched);
                    q = crate::regexp_prototype::advance_string_index(&units, q, unicode);
                    continue;
                }
                pieces.push(slice(&mut scope, input, p..q)?);
                if pieces.len() as u32 == limit {
                    last_match = Some(matched);
                    break 'pieces;
                }
                p = end;
                for capture in &matched.captures {
                    let piece = match capture {
                        Some(range) => slice(&mut scope, input, range.clone())?,
                        None => scope.undefined(),
                    };
                    pieces.push(piece);
                    if pieces.len() as u32 == limit {
                        last_match = Some(matched);
                        break 'pieces;
                    }
                }
                last_match = Some(matched);
                q = p;
            }
            if (pieces.len() as u32) < limit {
                pieces.push(slice(&mut scope, input, p..size)?);
            }
        }
        if let Some(matched) = last_match {
            let subject = scope.raw(input);
            scope.with_turn_parts(|interp, _| {
                interp
                    .regexp_legacy
                    .record_match(subject, matched.range.clone(), &matched.captures);
            });
        }
        let array = scope.array(pieces.len())?;
        for (index, piece) in pieces.into_iter().enumerate() {
            scope.set_index(array, index, piece)?;
        }
        Ok(scope.finish(array))
    })
}

fn slice<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    input: Local<'_>,
    range: std::ops::Range<usize>,
) -> Result<Local<'scope>, NativeError> {
    let string = scope
        .raw(input)
        .as_string(scope.context().heap())
        .expect("split subject is a string");
    let start = u32::try_from(range.start).expect("string offsets fit u32");
    let length = u32::try_from(range.len()).expect("string offsets fit u32");
    let piece = string.slice(start, length, scope.context().heap_mut())?;
    Ok(scope.value(Value::string(piece)))
}

/// `RegExp.prototype[@@replace]` for a pristine, non-sticky receiver, from
/// the point where `S` and the replacement (a callable or its string) are
/// known. The spec collects every `exec` result before calling a replacer,
/// so one engine pass yields the same matches; results are fresh arrays, so
/// reading them back is unobservable, and substitution works on ranges.
pub(crate) fn replace(
    ctx: &mut NativeCtx<'_>,
    receiver: Value,
    input: Value,
    replacement: Value,
    functional: bool,
) -> Result<Value, NativeError> {
    const NAME: &str = "RegExp.prototype[@@replace]";
    ctx.scope(|mut scope| {
        let receiver = scope.value(receiver);
        let input = scope.value(input);
        let replacement = scope.value(replacement);
        let regexp = scope.raw(receiver).as_regexp().expect("pristine RegExp");
        let subject = scope.raw(input);
        let heap = scope.context().heap();
        let flags = regexp.flags(heap);
        let units = subject
            .as_string(heap)
            .expect("replace subject is a string")
            .to_utf16_vec(heap);
        let template = (!functional).then(|| {
            scope
                .raw(replacement)
                .as_string(scope.context().heap())
                .expect("replace template is a string")
                .to_utf16_vec(scope.context().heap())
        });
        let step_limit = scope.context().regex_step_limit();
        let heap = scope.context().heap();
        let matches = if flags.global {
            regexp.set_last_index(heap, 0);
            let execution = regexp.find_from_utf16(heap, &units, 0, step_limit);
            crate::regexp::finish_execution(scope.context(), execution)?
        } else {
            let execution = regexp.find_one_from_utf16(heap, &units, 0, step_limit);
            crate::regexp::finish_execution(scope.context(), execution)?
                .into_iter()
                .collect()
        };
        if let Some(last) = matches.last() {
            let subject = scope.raw(input);
            scope.with_turn_parts(|interp, _| {
                interp
                    .regexp_legacy
                    .record_match(subject, last.range.clone(), &last.captures);
            });
        }
        let named = matches.first().is_some_and(|m| m.named_groups().next().is_some());
        let mut out: Vec<u16> = Vec::with_capacity(units.len());
        let mut next = 0;
        for matched in &matches {
            let position = matched.range.start;
            let mut replaced = Vec::new();
            match &template {
                Some(template) => {
                    if !expand_template(&mut replaced, template, &units, matched, named) {
                        return Err(NativeError::RangeError {
                            name: NAME,
                            reason: "Invalid string length".to_string(),
                        });
                    }
                }
                None => {
                    let result =
                        call_replacer(&mut scope, replacement, input, matched, named)?;
                    let result = scope.raw(result);
                    let result = crate::regexp_prototype::coerce_to_jsstring_runtime(
                        scope.context(),
                        &result,
                        NAME,
                    )?;
                    replaced = result.to_utf16_vec(scope.context().heap());
                }
            }
            if position >= next {
                out.extend_from_slice(&units[next..position]);
                out.extend_from_slice(&replaced);
                next = matched.range.end;
            }
            if out.len() > MAX_STRING_UNITS {
                return Err(NativeError::RangeError {
                    name: NAME,
                    reason: "Invalid string length".to_string(),
                });
            }
        }
        out.extend_from_slice(&units[next.min(units.len())..]);
        if out.len() > MAX_STRING_UNITS {
            return Err(NativeError::RangeError {
                name: NAME,
                reason: "Invalid string length".to_string(),
            });
        }
        let result = crate::string::JsString::from_utf16_units(&out, scope.context().heap_mut())?;
        let result = scope.value(Value::string(result));
        Ok(scope.finish(result))
    })
}

/// Longest string `@@replace` builds before reporting a RangeError.
const MAX_STRING_UNITS: usize = 1 << 29;

/// `Call(replaceValue, undefined, « matched, ...captures, position, S [,
/// groups] »)` with every argument rooted.
fn call_replacer<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    replacer: Local<'_>,
    input: Local<'_>,
    matched: &crate::regexp::engine::Match,
    named: bool,
) -> Result<Local<'scope>, NativeError> {
    let mut args = Vec::with_capacity(matched.captures.len() + 4);
    args.push(slice(scope, input, matched.range.clone())?);
    for capture in &matched.captures {
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
    out: &mut Vec<u16>,
    template: &[u16],
    subject: &[u16],
    matched: &crate::regexp::engine::Match,
    named: bool,
) -> bool {
    const DOLLAR: u16 = b'$' as u16;
    let digit = |unit: u16| (b'0' as u16..=b'9' as u16).contains(&unit).then(|| usize::from(unit - b'0' as u16));
    let groups = matched.captures.len();
    let tail = matched.range.end.min(subject.len());
    let mut i = 0;
    while i < template.len() {
        if out.len() > MAX_STRING_UNITS {
            return false;
        }
        let unit = template[i];
        let Some(&next) = template.get(i + 1).filter(|_| unit == DOLLAR) else {
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
                out.extend_from_slice(&subject[matched.range.clone()]);
                i += 2;
            }
            0x60 => {
                out.extend_from_slice(&subject[..matched.range.start]);
                i += 2;
            }
            0x27 => {
                out.extend_from_slice(&subject[tail..]);
                i += 2;
            }
            _ if digit(next).is_some() => {
                let first = digit(next).expect("digit");
                let second = template.get(i + 2).copied().and_then(digit);
                let group = |index: usize| (index > 0 && index <= groups).then_some(index);
                let (index, consumed) = match second.map(|second| first * 10 + second) {
                    Some(two) if group(two).is_some() => (Some(two), 3),
                    _ => (group(first), 2),
                };
                match index {
                    Some(index) => {
                        if let Some(range) = &matched.captures[index - 1] {
                            out.extend_from_slice(&subject[range.clone()]);
                        }
                    }
                    None => out.extend_from_slice(&template[i..i + consumed]),
                }
                i += consumed;
            }
            0x3c if named => {
                let Some(close) = template[i + 2..].iter().position(|&u| u == b'>' as u16) else {
                    out.push(DOLLAR);
                    i += 1;
                    continue;
                };
                let name = String::from_utf16_lossy(&template[i + 2..i + 2 + close]);
                if let Some((_, Some(range))) = matched.named_groups().find(|(group, _)| *group == name) {
                    out.extend_from_slice(&subject[range]);
                }
                i += close + 3;
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
