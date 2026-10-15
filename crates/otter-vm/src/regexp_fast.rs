//! The unmodified-RegExp protocol.
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
//! - [`exec_direct`] — `RegExpBuiltinExec` of a directly called original
//!   `exec` or `test`, without a frame, allocating nothing before a hit.
//! - [`split`] — `@@split` with the receiver's own matcher as the splitter.
//! - [`replace()`] — `@@replace` over match records, substituting by range.
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
//! - Replacement string borrows end before callbacks or VM allocation.
//!
//! # See also
//! - [`crate::regexp_prototype`] — the general spec ladders.

mod replace;

pub(crate) use replace::replace;

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
        let species = self
            .well_known_symbols
            .get(crate::symbol::WellKnown::Species);
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
        let Some(validity) = object::prototype_validity::chain_validity(prototype, &self.gc_heap)
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
    // Flatten a rope once so this and every later match of a `/g` loop read
    // the subject in place at its stored width; a failure only costs speed.
    let _ = text.flatten_in_place(scope.context().heap_mut());
    let regexp = scope
        .raw(receiver)
        .as_regexp()
        .expect("pristine receiver stays a RegExp");
    let text = scope
        .raw(input)
        .as_string(scope.context().heap())
        .expect("subject root stays a string");
    let context = scope.context();
    let search = context.regex_search();
    let heap = context.heap();
    let execution =
        text.with_code_units(heap, |units| regexp.find_one(heap, units, start, &search));
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
            interp.regexp_legacy.record_match(
                subject,
                matched.range.clone(),
                matched.captures().iter(),
            );
        });
    }
    Ok(true)
}

/// What a direct `exec` or `test` produced without a native frame.
pub(crate) enum DirectExec {
    /// The call completed with this value.
    Done(Value),
    /// `exec` matched: the match array remains to be built from the record.
    Matched(crate::regexp::engine::MatchRecord),
}

/// `RegExpBuiltinExec(R, S)` for a RegExp `receiver` whose `lastIndex` is a
/// writable non-negative int32 (so reading, coercing and writing it run no
/// user code) and a string `input`, as far as it goes without allocating.
/// A miss and every `test` complete; an `exec` hit returns its record after
/// the `lastIndex` write and the legacy statics. `None` when either operand
/// needs the general ladder. The match neither allocates nor collects.
pub(crate) fn exec_direct(
    interp: &mut Interpreter,
    receiver: Value,
    input: Value,
    test: bool,
) -> Option<Result<DirectExec, crate::VmError>> {
    let regexp = receiver.as_regexp()?;
    let heap = &interp.gc_heap;
    let text = input.as_string(heap)?;
    let last_index = regexp.last_index_value(heap).as_i32()?;
    if last_index < 0 || !regexp.last_index_writable(heap) {
        return None;
    }
    let flags = regexp.flags(heap);
    let tracks_last_index = flags.global || flags.sticky;
    let start = if tracks_last_index {
        last_index as usize
    } else {
        0
    };
    let missed = if test {
        Value::boolean(false)
    } else {
        Value::NULL
    };
    if start > text.len() as usize {
        regexp.set_last_index(heap, 0);
        return Some(Ok(DirectExec::Done(missed)));
    }
    let search = interp.regex_search();
    let execution =
        text.with_code_units(heap, |units| regexp.find_one(heap, units, start, &search));
    if let Err(error) = interp.charge_regex_backtrack_steps(execution.steps) {
        return Some(Err(error));
    }
    let Ok(matched) = execution.result else {
        return Some(Err(interp.err_budget(
            "work budget exceeded in a regular expression search".into(),
        )));
    };
    let heap = &interp.gc_heap;
    let Some(matched) = matched.filter(|m| !flags.sticky || m.range.start == start) else {
        if tracks_last_index {
            regexp.set_last_index(heap, 0);
        }
        return Some(Ok(DirectExec::Done(missed)));
    };
    if tracks_last_index {
        regexp.set_last_index(heap, matched.range.end as u32);
    }
    if regexp.legacy_features_enabled(heap) {
        interp
            .regexp_legacy
            .record_match(input, matched.range.clone(), matched.captures().iter());
    }
    Some(Ok(if test {
        DirectExec::Done(Value::boolean(true))
    } else {
        DirectExec::Matched(matched)
    }))
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
            let regexp = scope.raw(receiver).as_regexp().expect("pristine RegExp");
            let context = scope.context();
            let search = context.regex_search();
            let execution = regexp.find_one(
                context.heap(),
                crate::string::CodeUnits::Utf16(&units),
                at,
                &search,
            );
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
                for capture in matched.captures().iter() {
                    let piece = match capture {
                        Some(range) => slice(&mut scope, input, range.clone())?,
                        None => scope.undefined(),
                    };
                    pieces.push(piece);
                    if pieces.len() as u32 == limit {
                        break;
                    }
                }
                last_match = Some(matched);
                if pieces.len() as u32 == limit {
                    break 'pieces;
                }
                q = p;
            }
            if (pieces.len() as u32) < limit {
                pieces.push(slice(&mut scope, input, p..size)?);
            }
        }
        if let Some(matched) = last_match {
            let subject = scope.raw(input);
            scope.with_turn_parts(|interp, _| {
                interp.regexp_legacy.record_match(
                    subject,
                    matched.range.clone(),
                    matched.captures().iter(),
                );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::string::JsString;

    fn regexp(vm: &mut Interpreter, pattern: &str, flags: &str) -> Value {
        let units: Vec<u16> = pattern.encode_utf16().collect();
        let regexp = crate::regexp::JsRegExp::compile_cached(
            &mut vm.gc_heap,
            &mut vm.regex_compile_cache,
            &units,
            flags,
        )
        .expect("fixture pattern");
        Value::regexp(regexp)
    }

    fn done(result: Option<Result<DirectExec, crate::VmError>>) -> Option<Value> {
        match result {
            Some(Ok(DirectExec::Done(value))) => Some(value),
            _ => None,
        }
    }

    #[test]
    fn direct_exec_completes_misses_tests_and_global_last_index_without_allocating() {
        let mut vm = Interpreter::new().expect("fixture interpreter");
        vm.gc_heap.set_gc_stress(0, false);
        let subject = Value::string(JsString::from_str("xbaxba", &mut vm.gc_heap).unwrap());
        let plain = regexp(&mut vm, "^ba", "");
        assert_eq!(
            done(exec_direct(&mut vm, plain, subject, false)),
            Some(Value::NULL)
        );
        assert_eq!(
            done(exec_direct(&mut vm, plain, subject, true)),
            Some(Value::boolean(false))
        );
        let global = regexp(&mut vm, "ba", "g");
        assert_eq!(
            done(exec_direct(&mut vm, global, subject, true)),
            Some(Value::boolean(true))
        );
        assert_eq!(global.as_regexp().unwrap().last_index(&vm.gc_heap), 3);
        // An exec hit leaves the array to its caller, after the lastIndex write.
        match exec_direct(&mut vm, global, subject, false) {
            Some(Ok(DirectExec::Matched(matched))) => {
                assert_eq!(matched.range, 4..6)
            }
            _ => panic!("a hit returns its record"),
        }
        assert_eq!(global.as_regexp().unwrap().last_index(&vm.gc_heap), 6);
        assert_eq!(
            done(exec_direct(&mut vm, global, subject, false)),
            Some(Value::NULL)
        );
        assert_eq!(global.as_regexp().unwrap().last_index(&vm.gc_heap), 0);
    }

    #[test]
    fn direct_exec_declines_operands_that_need_the_general_ladder() {
        let mut vm = Interpreter::new().expect("fixture interpreter");
        vm.gc_heap.set_gc_stress(0, false);
        let subject = Value::string(JsString::from_str("aaa", &mut vm.gc_heap).unwrap());
        let re = regexp(&mut vm, "a", "g");
        // A non-string subject and a non-RegExp receiver.
        assert!(exec_direct(&mut vm, re, Value::number_i32(1), false).is_none());
        assert!(exec_direct(&mut vm, subject, subject, false).is_none());
        // A lastIndex that is not a non-negative int32 needs its coercion.
        re.as_regexp()
            .unwrap()
            .set_last_index_value(&mut vm.gc_heap, Value::number_f64(1.5));
        assert!(exec_direct(&mut vm, re, subject, false).is_none());
        re.as_regexp()
            .unwrap()
            .set_last_index_value(&mut vm.gc_heap, Value::number_i32(-1));
        assert!(exec_direct(&mut vm, re, subject, false).is_none());
    }
}
