//! Iterator opcode helpers.
//!
//! Built-in iterator operations can run synchronously after the dispatch loop
//! gives user-defined iterator hooks a chance to push call frames.
//!
//! # Contents
//! - Built-in iterable wrapping for `GetIterator`.
//! - Synchronous stepping for VM iterator handles.
//! - Full iterator stepping for user iterators and iterator-helper wrappers.
//!
//! # Invariants
//! - User-object `@@iterator` and `next()` call paths are driven before these
//!   helpers are called.
//! - Inputs are decoded from executable operands.
//! - Helpers advance the current frame PC exactly once on success.
//! - Iterator helper callbacks never hold a GC payload borrow across VM
//!   dispatch; state is snapshotted first.
//!
//! # See also
//! - [`crate::executable`]
//! - [`crate::IteratorState`]

use smallvec::SmallVec;

use otter_bytecode::Operand;

use crate::{
    ExecutionContext, Frame, GeneratorResumeKind, Interpreter, IteratorHandle, IteratorState,
    JsPromise, JsString, PendingGetIterator, PendingIteratorNext, Value, VmError, VmGetOutcome,
    VmPropertyKey, array, generator::AsyncGeneratorState, is_callable,
    operand_decode::register_operand, promise::PromiseCapability, read_register, require_callable,
    step_iterator, symbol, take_drop_count, value_kind_name, write_register,
};

fn string_iterator_values(s: JsString, heap: &mut otter_gc::GcHeap) -> Result<Vec<Value>, VmError> {
    let mut out = Vec::new();
    let mut index = 0;
    while let Some(unit) = s.char_code_at(index, heap) {
        let next_unit = s.char_code_at(index + 1, heap);
        let is_pair = (0xD800..=0xDBFF).contains(&unit)
            && matches!(next_unit, Some(low) if (0xDC00..=0xDFFF).contains(&low));
        let units: smallvec::SmallVec<[u16; 2]> = if is_pair {
            smallvec::smallvec![unit, next_unit.expect("checked above")]
        } else {
            smallvec::smallvec![unit]
        };
        let advance = units.len() as u32;
        let value = JsString::from_utf16_units(&units, heap)?;
        out.push(Value::string(value));
        index += advance;
    }
    Ok(out)
}

/// Cloned snapshot of an [`IteratorState`] taken before driving a
/// helper callback so the GC body borrow does not span dispatch.
enum IteratorStateSnapshot {
    User(Value),
    RegExpString {
        matcher: Value,
        input: JsString,
        global: bool,
        full_unicode: bool,
        done: bool,
    },
    Generator(crate::generator::JsGenerator),
    Map {
        source: IteratorHandle,
        mapper: Value,
    },
    Filter {
        source: IteratorHandle,
        predicate: Value,
    },
    Take {
        source: IteratorHandle,
        remaining: u64,
    },
    Drop {
        source: IteratorHandle,
        to_drop: u64,
    },
    FlatMap {
        source: IteratorHandle,
        mapper: Value,
        inner: Option<IteratorHandle>,
    },
}

impl Interpreter {
    pub(crate) fn run_get_iterator_regs(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        top_idx: usize,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let frame = &stack[top_idx];
        let value = *read_register(frame, src)?;
        let state = if let Some(array) = value.as_array() {
            IteratorState::Array {
                array,
                index: 0,
                origin: crate::BuiltinIteratorOrigin::Array,
            }
        } else if let Some(string) = value.as_string(&self.gc_heap) {
            IteratorState::String { string, index: 0 }
        } else if let Some(m) = value.as_map() {
            // `for…of` over a `Map` yields `[key, value]` pairs (Spec
            // §24.1.3.12 — `@@iterator` aliases `entries`). A live
            // `MapCollection` iterator walks the backing entry table by
            // index so additions / deletions during iteration are
            // observed per §24.1.5.1 CreateMapIterator.
            IteratorState::MapCollection {
                map: m,
                index: 0,
                kind: crate::MapIteratorKind::Entry,
            }
        } else if let Some(s) = value.as_set() {
            // §24.2.3.11 — `for…of` over a `Set` yields values via a
            // live `SetCollection` iterator (§24.2.5.1).
            IteratorState::SetCollection {
                set: s,
                index: 0,
                kind: crate::SetIteratorKind::Value,
            }
        } else if let Some(handle) = value.as_generator() {
            // §27.5 — generator objects are iterable; `[@@iterator]()` returns
            // the generator itself, and `next()` drives the suspended body.
            IteratorState::Generator { handle }
        } else if let Some(rc) = value.as_iterator() {
            // Already-an-iterator should pass through unchanged.
            let frame = &mut stack[top_idx];
            write_register(frame, dst, Value::iterator(rc))?;
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        } else {
            return Err(VmError::TypeMismatch);
        };
        let iter = self.alloc_stack_rooted_iterator_state(stack, state, &[&value], &[])?;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, Value::iterator(iter))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_get_async_iterator_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
        top_idx: usize,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let value = *read_register(&stack[top_idx], src)?;
        if let Some(handle) = value.as_generator()
            && handle.is_async(&self.gc_heap)
        {
            write_register(&mut stack[top_idx], dst, value)?;
            stack[top_idx].advance_pc(self.current_byte_len)?;
            return Ok(());
        }

        let async_iter_sym = self
            .well_known_symbols
            .get(symbol::WellKnown::AsyncIterator);
        let can_have_method = value.as_object().is_some()
            || value.as_array().is_some()
            || value.as_map().is_some()
            || value.as_set().is_some()
            || value.is_proxy();
        if can_have_method {
            let method = match self.ordinary_get_value(
                context,
                value,
                value,
                &VmPropertyKey::Symbol(async_iter_sym),
                0,
            )? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync(context, &getter, value, SmallVec::new())?
                }
            };
            if !method.is_nullish() {
                if !is_callable(&method) {
                    return Err(VmError::TypeMismatch);
                }
                let produced = self.run_callable_sync(context, &method, value, SmallVec::new())?;
                if produced.as_object().is_none()
                    && produced.as_generator().is_none()
                    && produced.as_iterator().is_none()
                    && produced.as_array().is_none()
                    && !produced.is_proxy()
                {
                    return Err(VmError::TypeMismatch);
                }
                write_register(&mut stack[top_idx], dst, produced)?;
                stack[top_idx].advance_pc(self.current_byte_len)?;
                return Ok(());
            }
        }

        self.run_get_iterator_regs(stack, top_idx, dst, src)
    }

    pub(crate) fn run_iterator_next_regs(
        &mut self,
        frame: &mut Frame,
        value_dst: u16,
        done_dst: u16,
        iter_reg: u16,
    ) -> Result<(), VmError> {
        let Some(iter) = read_register(frame, iter_reg)?.as_iterator() else {
            return Err(VmError::TypeMismatch);
        };
        let (value, done) = step_iterator(iter, &mut self.gc_heap)?;
        write_register(frame, value_dst, value)?;
        write_register(frame, done_dst, Value::boolean(done))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    /// Synchronously advance an iterator one step, with full
    /// interpreter access so user-iterator `next()` calls and
    /// helper-wrapper callbacks can run inline. Mirrors the
    /// fast-path [`step_iterator`] helper but also handles the
    /// `User` / `Map` / `Filter` / `Take` / `Drop` / `FlatMap`
    /// variants by driving callbacks through
    /// [`Self::run_callable_sync`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-iteratornext>
    /// - <https://tc39.es/proposal-iterator-helpers/>
    pub(crate) fn iterator_next_full(
        &mut self,
        context: &ExecutionContext,
        iter: &IteratorHandle,
    ) -> Result<(Value, bool), VmError> {
        match step_iterator(*iter, &mut self.gc_heap) {
            Ok((value, done)) => Ok((value, done)),
            Err(_) => self.iterator_next_full_slow(context, iter),
        }
    }

    fn iterator_next_full_slow(
        &mut self,
        context: &ExecutionContext,
        iter: &IteratorHandle,
    ) -> Result<(Value, bool), VmError> {
        let snapshot: Option<IteratorStateSnapshot> =
            self.gc_heap.read_payload(*iter, |state| match state {
                IteratorState::User { iterator } => Some(IteratorStateSnapshot::User(*iterator)),
                IteratorState::RegExpString {
                    matcher,
                    input,
                    global,
                    full_unicode,
                    done,
                } => Some(IteratorStateSnapshot::RegExpString {
                    matcher: *matcher,
                    input: *input,
                    global: *global,
                    full_unicode: *full_unicode,
                    done: *done,
                }),
                IteratorState::Generator { handle } => {
                    Some(IteratorStateSnapshot::Generator(*handle))
                }
                IteratorState::Map { source, mapper } => Some(IteratorStateSnapshot::Map {
                    source: *source,
                    mapper: *mapper,
                }),
                IteratorState::Filter { source, predicate } => {
                    Some(IteratorStateSnapshot::Filter {
                        source: *source,
                        predicate: *predicate,
                    })
                }
                IteratorState::Take { source, remaining } => Some(IteratorStateSnapshot::Take {
                    source: *source,
                    remaining: *remaining,
                }),
                IteratorState::Drop { source, to_drop } => Some(IteratorStateSnapshot::Drop {
                    source: *source,
                    to_drop: *to_drop,
                }),
                IteratorState::FlatMap {
                    source,
                    mapper,
                    inner,
                } => Some(IteratorStateSnapshot::FlatMap {
                    source: *source,
                    mapper: *mapper,
                    inner: *inner,
                }),
                _ => None,
            });
        let snapshot = snapshot.ok_or(VmError::TypeMismatch)?;
        match snapshot {
            IteratorStateSnapshot::Generator(handle) => {
                let result = self.resume_generator(
                    context,
                    &handle,
                    GeneratorResumeKind::Next(Value::undefined()),
                )?;
                let Some(record) = result.as_object() else {
                    return Err(VmError::TypeMismatch);
                };
                let value = crate::object::get(record, &self.gc_heap, "value")
                    .unwrap_or(Value::undefined());
                let done = crate::object::get(record, &self.gc_heap, "done")
                    .unwrap_or(Value::undefined())
                    .to_boolean(&self.gc_heap);
                if done {
                    self.gc_heap
                        .with_payload(*iter, |state| *state = IteratorState::Exhausted);
                }
                Ok((value, done))
            }
            IteratorStateSnapshot::User(iter_value) => {
                let Some(iter_obj) = iter_value.as_object() else {
                    return Err(VmError::TypeMismatch);
                };
                let next_fn = crate::object::get(iter_obj, &self.gc_heap, "next")
                    .ok_or(VmError::TypeMismatch)?;
                if !self.is_callable_runtime(&next_fn) {
                    return Err(VmError::TypeMismatch);
                }
                let result =
                    self.run_callable_sync(context, &next_fn, iter_value, SmallVec::new())?;
                let Some(record) = result.as_object() else {
                    return Err(VmError::TypeMismatch);
                };
                let value = crate::object::get(record, &self.gc_heap, "value")
                    .unwrap_or(Value::undefined());
                let done = crate::object::get(record, &self.gc_heap, "done")
                    .unwrap_or(Value::undefined())
                    .to_boolean(&self.gc_heap);
                if done {
                    self.gc_heap
                        .with_payload(*iter, |state| *state = IteratorState::Exhausted);
                }
                Ok((value, done))
            }
            IteratorStateSnapshot::RegExpString {
                matcher,
                input,
                global,
                full_unicode,
                done,
            } => {
                if done {
                    return Ok((Value::undefined(), true));
                }
                let result = crate::regexp_prototype::regexp_string_iterator_next_runtime(
                    self,
                    context,
                    &matcher,
                    input,
                    global,
                    full_unicode,
                )?;
                let Some(match_value) = result else {
                    self.gc_heap.with_payload(*iter, |state| {
                        if let IteratorState::RegExpString { done, .. } = state {
                            *done = true;
                        }
                    });
                    return Ok((Value::undefined(), true));
                };
                if !global {
                    self.gc_heap.with_payload(*iter, |state| {
                        if let IteratorState::RegExpString { done, .. } = state {
                            *done = true;
                        }
                    });
                }
                Ok((match_value, false))
            }
            IteratorStateSnapshot::Map { source, mapper } => {
                let (v, done) = self.iterator_next_full(context, &source)?;
                if done {
                    self.gc_heap
                        .with_payload(*iter, |state| *state = IteratorState::Exhausted);
                    return Ok((Value::undefined(), true));
                }
                let mapped = self.run_callable_sync(
                    context,
                    &mapper,
                    Value::undefined(),
                    smallvec::smallvec![v],
                )?;
                Ok((mapped, false))
            }
            IteratorStateSnapshot::Filter { source, predicate } => loop {
                let (v, done) = self.iterator_next_full(context, &source)?;
                if done {
                    self.gc_heap
                        .with_payload(*iter, |state| *state = IteratorState::Exhausted);
                    return Ok((Value::undefined(), true));
                }
                let kept = self.run_callable_sync(
                    context,
                    &predicate,
                    Value::undefined(),
                    smallvec::smallvec![v],
                )?;
                if kept.to_boolean(&self.gc_heap) {
                    return Ok((v, false));
                }
            },
            IteratorStateSnapshot::Take { source, remaining } => {
                if remaining == 0 {
                    self.gc_heap
                        .with_payload(*iter, |state| *state = IteratorState::Exhausted);
                    return Ok((Value::undefined(), true));
                }
                let (v, done) = self.iterator_next_full(context, &source)?;
                if done {
                    self.gc_heap
                        .with_payload(*iter, |state| *state = IteratorState::Exhausted);
                    return Ok((Value::undefined(), true));
                }
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::Take { remaining, .. } = state {
                        *remaining = remaining.saturating_sub(1);
                    }
                });
                Ok((v, false))
            }
            IteratorStateSnapshot::Drop { source, to_drop } => {
                for _ in 0..to_drop {
                    let (_, done) = self.iterator_next_full(context, &source)?;
                    if done {
                        self.gc_heap
                            .with_payload(*iter, |state| *state = IteratorState::Exhausted);
                        return Ok((Value::undefined(), true));
                    }
                }
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::Drop { to_drop, .. } = state {
                        *to_drop = 0;
                    }
                });
                let (v, done) = self.iterator_next_full(context, &source)?;
                if done {
                    self.gc_heap
                        .with_payload(*iter, |state| *state = IteratorState::Exhausted);
                    return Ok((Value::undefined(), true));
                }
                Ok((v, false))
            }
            IteratorStateSnapshot::FlatMap {
                source,
                mapper,
                mut inner,
            } => loop {
                if let Some(inner_iter) = inner.take() {
                    let (v, done) = self.iterator_next_full(context, &inner_iter)?;
                    if !done {
                        return Ok((v, false));
                    }
                    self.gc_heap.with_payload(*iter, |state| {
                        if let IteratorState::FlatMap { inner: slot, .. } = state {
                            *slot = None;
                        }
                    });
                }
                let (v, done) = self.iterator_next_full(context, &source)?;
                if done {
                    self.gc_heap
                        .with_payload(*iter, |state| *state = IteratorState::Exhausted);
                    return Ok((Value::undefined(), true));
                }
                let mapped = self.run_callable_sync(
                    context,
                    &mapper,
                    Value::undefined(),
                    smallvec::smallvec![v],
                )?;
                // §27.5.1.10 step 7.b.iv — `GetIteratorFlattenable(mapped)`
                // accepts any iterable (Array / Set / Map / String /
                // Generator / Object with `@@iterator`) and any
                // existing iterator. Non-iterable primitives throw
                // TypeError. The Iterator-helpers spec rejects raw
                // values that aren't iterables.
                let inner_state = if let Some(arr) = mapped.as_array() {
                    IteratorState::Array {
                        array: arr,
                        index: 0,
                        origin: crate::BuiltinIteratorOrigin::Array,
                    }
                } else if let Some(rc) = mapped.as_iterator() {
                    let new_inner = rc;
                    self.gc_heap.with_payload(*iter, |state| {
                        if let IteratorState::FlatMap { inner: slot, .. } = state {
                            *slot = Some(new_inner);
                        }
                    });
                    inner = Some(new_inner);
                    continue;
                } else if let Some(g) = mapped.as_generator() {
                    IteratorState::Generator { handle: g }
                } else if let Some(s) = mapped.as_string(&self.gc_heap) {
                    IteratorState::String {
                        string: s,
                        index: 0,
                    }
                } else if mapped.is_set() || mapped.is_map() || mapped.is_object() {
                    // §7.4.2 GetIteratorFlattenable — look up
                    // `@@iterator`. If present, call it to obtain
                    // the real iterator. If missing / null, the
                    // value is already an iterator (has `.next`
                    // directly) and routes through
                    // `IteratorState::User` unchanged.
                    let iterator_sym = self
                        .well_known_symbols
                        .get(crate::symbol::WellKnown::Iterator);
                    let key = crate::VmPropertyKey::Symbol(iterator_sym);
                    let outcome = self.ordinary_get_value(context, mapped, mapped, &key, 0)?;
                    let iter_method = match outcome {
                        crate::VmGetOutcome::Value(v) => v,
                        crate::VmGetOutcome::InvokeGetter { getter } => {
                            self.run_callable_sync(context, &getter, mapped, SmallVec::new())?
                        }
                    };
                    let iter_value = if iter_method.is_undefined() || iter_method.is_null() {
                        // Iterator-without-`@@iterator` shape —
                        // wrap the mapped object directly so
                        // subsequent `IteratorNext` calls invoke
                        // its own `.next`.
                        mapped
                    } else if self.is_callable_runtime(&iter_method) {
                        self.run_callable_sync(context, &iter_method, mapped, SmallVec::new())?
                    } else {
                        return Err(VmError::TypeError {
                            message: "Iterator.prototype.flatMap mapper return must be iterable"
                                .to_string(),
                        });
                    };
                    if let Some(rc) = iter_value.as_iterator() {
                        let new_inner = rc;
                        self.gc_heap.with_payload(*iter, |state| {
                            if let IteratorState::FlatMap { inner: slot, .. } = state {
                                *slot = Some(new_inner);
                            }
                        });
                        inner = Some(new_inner);
                        continue;
                    }
                    if let Some(g) = iter_value.as_generator() {
                        IteratorState::Generator { handle: g }
                    } else {
                        IteratorState::User {
                            iterator: iter_value,
                        }
                    }
                } else {
                    return Err(VmError::TypeError {
                        message: "Iterator.prototype.flatMap mapper return must be iterable"
                            .to_string(),
                    });
                };
                let iter_root = Value::iterator(*iter);
                let source_root = Value::iterator(source);
                let mapper_root = mapper;
                let new_inner = self.alloc_runtime_rooted_iterator_state(
                    inner_state,
                    &[&iter_root, &source_root, &mapper_root],
                    &[],
                )?;
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::FlatMap { inner: slot, .. } = state {
                        *slot = Some(new_inner);
                    }
                });
                inner = Some(new_inner);
            },
        }
    }

    /// Dispatch one of the §27.5 / iterator-helper-proposal methods against a
    /// [`Value::Iterator`] receiver. Returns `Ok(true)` when the call was
    /// handled (`dst` written, pc advanced) and `Ok(false)` when the receiver
    /// does not expose `name`.
    ///
    /// # See also
    /// - <https://tc39.es/proposal-iterator-helpers/>
    pub(crate) fn iterator_helper_dispatch(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        iter_rc: &IteratorHandle,
        name: &str,
        args: &SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<bool, VmError> {
        // Lazy helpers wrap the source in a new IteratorState; the
        // eager terminals drain via `iterator_next_full`.
        let iter_value = Value::iterator(*iter_rc);
        let result = match name {
            "map" => {
                let mapper = require_callable(args.first())?;
                let mapper_root = mapper;
                let state = IteratorState::Map {
                    source: *iter_rc,
                    mapper,
                };
                Value::iterator(self.alloc_stack_rooted_iterator_state(
                    stack,
                    state,
                    &[&iter_value, &mapper_root],
                    &[],
                )?)
            }
            "filter" => {
                let predicate = require_callable(args.first())?;
                let predicate_root = predicate;
                let state = IteratorState::Filter {
                    source: *iter_rc,
                    predicate,
                };
                Value::iterator(self.alloc_stack_rooted_iterator_state(
                    stack,
                    state,
                    &[&iter_value, &predicate_root],
                    &[],
                )?)
            }
            "take" => {
                let n = take_drop_count(args.first())?;
                let state = IteratorState::Take {
                    source: *iter_rc,
                    remaining: n,
                };
                Value::iterator(self.alloc_stack_rooted_iterator_state(
                    stack,
                    state,
                    &[&iter_value],
                    &[],
                )?)
            }
            "drop" => {
                let n = take_drop_count(args.first())?;
                let state = IteratorState::Drop {
                    source: *iter_rc,
                    to_drop: n,
                };
                Value::iterator(self.alloc_stack_rooted_iterator_state(
                    stack,
                    state,
                    &[&iter_value],
                    &[],
                )?)
            }
            "flatMap" => {
                let mapper = require_callable(args.first())?;
                let mapper_root = mapper;
                let state = IteratorState::FlatMap {
                    source: *iter_rc,
                    mapper,
                    inner: None,
                };
                Value::iterator(self.alloc_stack_rooted_iterator_state(
                    stack,
                    state,
                    &[&iter_value, &mapper_root],
                    &[],
                )?)
            }
            "toArray" => {
                let collected = self.drain_iterator(context, iter_rc)?;
                let result = self.alloc_stack_rooted_array_from_values_with_root_slices(
                    stack,
                    collected.iter().cloned(),
                    &[&iter_value],
                    &[args.as_slice(), collected.as_slice()],
                )?;
                Value::array(result)
            }
            "forEach" => {
                let callback = require_callable(args.first())?;
                let collected = self.drain_iterator(context, iter_rc)?;
                for v in collected {
                    self.run_callable_sync(
                        context,
                        &callback,
                        Value::undefined(),
                        smallvec::smallvec![v],
                    )?;
                }
                Value::undefined()
            }
            "reduce" => {
                let reducer = require_callable(args.first())?;
                let has_initial = args.len() >= 2;
                let mut acc = if has_initial {
                    args[1]
                } else {
                    Value::undefined()
                };
                let collected = self.drain_iterator(context, iter_rc)?;
                let mut iter = collected.into_iter();
                if !has_initial {
                    acc = match iter.next() {
                        Some(v) => v,
                        None => {
                            // Spec §27.5.x — empty + no initial → TypeError.
                            return Err(VmError::TypeMismatch);
                        }
                    };
                }
                for v in iter {
                    acc = self.run_callable_sync(
                        context,
                        &reducer,
                        Value::undefined(),
                        smallvec::smallvec![acc, v],
                    )?;
                }
                acc
            }
            // §27.1.2 %IteratorPrototype%.next — pull one step from
            // the wrapped state and surface the spec-shaped result
            // object `{ value, done }`.
            // <https://tc39.es/ecma262/#sec-iteratorprototype>
            "next" => {
                let (v, done) = self.iterator_next_full(context, iter_rc)?;
                let obj =
                    self.alloc_stack_rooted_object_with_extra_roots(stack, &[&iter_value, &v])?;
                self.set_property(obj, "value", v)?;
                self.set_property(obj, "done", Value::boolean(done))?;
                Value::object(obj)
            }
            // §27.1.3 / §27.1.4 — `return` / `throw` on plain
            // array-backed iterators are no-ops that fold the
            // iterator to its completion state. Generator-style
            // iterators are handled by the dedicated
            // `Value::Generator` dispatch above.
            "return" => {
                let arg = args.first().cloned().unwrap_or(Value::undefined());
                let obj =
                    self.alloc_stack_rooted_object_with_extra_roots(stack, &[&iter_value, &arg])?;
                self.set_property(obj, "value", arg)?;
                self.set_property(obj, "done", Value::boolean(true))?;
                Value::object(obj)
            }
            "throw" => {
                let arg = args.first().cloned().unwrap_or(Value::undefined());
                return Err(VmError::Uncaught {
                    value: value_kind_name(&arg).to_string(),
                });
            }
            _ => return Ok(false),
        };
        let top_idx = stack.len() - 1;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, result)?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(true)
    }

    fn drain_iterator(
        &mut self,
        context: &ExecutionContext,
        iter_rc: &IteratorHandle,
    ) -> Result<Vec<Value>, VmError> {
        let mut out = Vec::new();
        loop {
            let (v, done) = self.iterator_next_full(context, iter_rc)?;
            if done {
                return Ok(out);
            }
            out.push(v);
        }
    }

    /// §7.4.1 GetIterator(obj, hint=sync) sync helper.
    ///
    /// Returns the spec's `IteratorRecord` as `(iterator, nextMethod)`
    /// — the `[[Done]]` slot lives on the caller side as a local
    /// `bool` because step / close paths short-circuit through `?`.
    ///
    /// # Errors
    /// - `TypeError` if `@@iterator` lookup or the result of calling
    ///   it is not an Object.
    /// - Any abrupt completion from the user `@@iterator` / `Get`
    ///   ladder propagates verbatim.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-getiterator>
    pub(crate) fn get_iterator_sync(
        &mut self,
        context: &ExecutionContext,
        iterable: &Value,
    ) -> Result<(Value, Value), VmError> {
        let iterator_sym = self.well_known_symbols.get(symbol::WellKnown::Iterator);
        let method = match self.ordinary_get_value(
            context,
            *iterable,
            *iterable,
            &VmPropertyKey::Symbol(iterator_sym),
            0,
        )? {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync(context, &getter, *iterable, SmallVec::new())?
            }
        };
        if method.is_undefined() || method.is_null() {
            return Err(VmError::TypeError {
                message: "iterator method is not callable".to_string(),
            });
        }
        if !self.is_callable_runtime(&method) {
            return Err(VmError::TypeError {
                message: "iterator method is not callable".to_string(),
            });
        }
        let iterator = self.run_callable_sync(context, &method, *iterable, SmallVec::new())?;
        if !(iterator.is_object()
            || iterator.is_proxy()
            || iterator.is_array()
            || iterator.is_iterator()
            || iterator.is_map()
            || iterator.is_set()
            || iterator.is_generator())
        {
            return Err(VmError::TypeError {
                message: "iterator method did not return an object".to_string(),
            });
        }
        let next_method = match self.ordinary_get_value(
            context,
            iterator,
            iterator,
            &VmPropertyKey::String("next"),
            0,
        )? {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync(context, &getter, iterator, SmallVec::new())?
            }
        };
        Ok((iterator, next_method))
    }

    /// §7.4.6 IteratorStep — invoke `next` and read the result.
    ///
    /// Returns `Some(value)` when the iterator yielded a value,
    /// `None` when it signalled completion. Caller is responsible
    /// for tracking the IteratorRecord `[[Done]]` bit (it should
    /// flip to `true` on `None` or on any abrupt completion).
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-iteratorstep>
    /// - <https://tc39.es/ecma262/#sec-iteratornext>
    /// - <https://tc39.es/ecma262/#sec-iteratorvalue>
    pub(crate) fn iterator_step_sync(
        &mut self,
        context: &ExecutionContext,
        iterator: &Value,
        next_method: &Value,
    ) -> Result<Option<Value>, VmError> {
        let result = self.run_callable_sync(context, next_method, *iterator, SmallVec::new())?;
        if !result.is_object() && !result.is_proxy() {
            return Err(VmError::TypeError {
                message: "iterator result is not an object".to_string(),
            });
        }
        // §7.4.6 IteratorStep — anchor the result on the GC root
        // stack across the subsequent `done` / `value` property
        // reads. Without this, a GC triggered inside an accessor
        // getter (or by allocations on the way to the slot lookup)
        // could reclaim the IterResult — its shape/keys would then
        // dangle when the second read walks the same shape chain.
        let anchor_depth = self.push_iteration_anchor(result);
        let outcome = iterator_step_read(self, context, &result);
        self.pop_iteration_anchors_to(anchor_depth - 1);
        outcome
    }

    /// §7.4.8 IteratorClose — invoke `return` if present.
    ///
    /// The `completion` semantics are caller-owned: pass `Ok(())` to
    /// run the close because the surrounding loop finished
    /// successfully; on an abrupt completion the caller should
    /// invoke close and then propagate the original completion
    /// regardless of close's result.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-iteratorclose>
    pub(crate) fn iterator_close_sync(
        &mut self,
        context: &ExecutionContext,
        iterator: &Value,
    ) -> Result<(), VmError> {
        let return_method = match self.ordinary_get_value(
            context,
            *iterator,
            *iterator,
            &VmPropertyKey::String("return"),
            0,
        )? {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync(context, &getter, *iterator, SmallVec::new())?
            }
        };
        if return_method.is_undefined() || return_method.is_null() {
            return Ok(());
        }
        if !self.is_callable_runtime(&return_method) {
            return Err(VmError::TypeError {
                message: "iterator `return` is not callable".to_string(),
            });
        }
        let result = self.run_callable_sync(context, &return_method, *iterator, SmallVec::new())?;
        if !result.is_object() && !result.is_proxy() {
            return Err(VmError::TypeError {
                message: "iterator `return` did not yield an object".to_string(),
            });
        }
        Ok(())
    }

    pub(crate) fn iterator_close_value_sync(
        &mut self,
        context: &ExecutionContext,
        iterator: Value,
    ) -> Result<(), VmError> {
        enum CloseAction {
            User(Value),
            Generator(crate::generator::JsGenerator),
            Builtin,
            None,
        }
        let action = if let Some(handle) = iterator.as_iterator() {
            self.gc_heap.read_payload(handle, |state| match state {
                IteratorState::User { iterator } => CloseAction::User(*iterator),
                // §7.4.9 — a generator's `return` resumes the suspended
                // body with a return completion so its `finally` blocks
                // run and `[[GeneratorState]]` becomes completed.
                IteratorState::Generator { handle } => CloseAction::Generator(*handle),
                // Array / TypedArray / String / Map / Set iterators
                // expose no `return`, so IteratorClose is a no-op.
                IteratorState::Exhausted => CloseAction::None,
                _ => CloseAction::Builtin,
            })
        } else {
            CloseAction::User(iterator)
        };
        match action {
            CloseAction::User(close_target) => {
                self.iterator_close_sync(context, &close_target)?;
            }
            CloseAction::Generator(handle) => {
                self.resume_generator(
                    context,
                    &handle,
                    GeneratorResumeKind::Return(Value::undefined()),
                )?;
            }
            CloseAction::Builtin | CloseAction::None => {}
        }
        Ok(())
    }

    /// §7.4.13 IteratorToList synchronous helper.
    ///
    /// Drives the iterator to exhaustion and returns the collected
    /// values. Built-in iterables (`Array`, `String`, `Map`, `Set`,
    /// `Generator`) take a fast path that bypasses the user-visible
    /// `@@iterator` round-trip; everything else routes through
    /// `GetIterator` + `IteratorStep`. On abrupt completion mid-walk
    /// the iterator's `return` method is invoked best-effort.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-iteratortolist>
    pub(crate) fn iterator_to_list_sync(
        &mut self,
        context: &ExecutionContext,
        iterable: &Value,
    ) -> Result<Vec<Value>, VmError> {
        // Built-in iterable fast paths — §22.1.5.1 ArrayIterator,
        // §22.1.3.36 String[@@iterator], §24.1.5.1 SetIterator,
        // §24.3.5.1 MapIterator, §27.5.1.2 Generator step.
        if let Some(arr) = iterable.as_array() {
            let elements = array::with_elements(arr, &self.gc_heap, |elements| elements.to_vec());
            return Ok(elements);
        }
        if let Some(s) = iterable.as_string(&self.gc_heap) {
            return string_iterator_values(s, &mut self.gc_heap);
        }
        if let Some(s) = iterable.as_set() {
            return Ok(crate::collections::set_values(s, &self.gc_heap));
        }
        if let Some(m) = iterable.as_map() {
            let pairs = crate::collections::map_entries(m, &self.gc_heap);
            let mut out = Vec::with_capacity(pairs.len());
            for (k, v) in pairs {
                let entry = self.alloc_runtime_rooted_array_from_values(
                    [k, v],
                    &[iterable, &k, &v],
                    &[out.as_slice()],
                )?;
                out.push(Value::array(entry));
            }
            return Ok(out);
        }
        if let Some(handle) = iterable.as_generator() {
            let mut out: Vec<Value> = Vec::new();
            loop {
                let result = self.resume_generator(
                    context,
                    &handle,
                    GeneratorResumeKind::Next(Value::undefined()),
                )?;
                let Some(record) = result.as_object() else {
                    return Err(VmError::TypeError {
                        message: "generator next did not return an object".to_string(),
                    });
                };
                let done = crate::object::get(record, &self.gc_heap, "done")
                    .unwrap_or(Value::undefined())
                    .to_boolean(&self.gc_heap);
                if done {
                    return Ok(out);
                }
                let value = crate::object::get(record, &self.gc_heap, "value")
                    .unwrap_or(Value::undefined());
                out.push(value);
            }
        }
        // §27.5 IteratorRecord drain — `Value::Iterator` wraps a
        // foundation `IteratorState`. Drive it through
        // `iterator_next_full` so lazy combinators (Map / Filter
        // / Take / Drop / FlatMap) and user iterators all share
        // the same termination contract.
        if let Some(handle) = iterable.as_iterator() {
            let mut out: Vec<Value> = Vec::new();
            loop {
                let (v, done) = self.iterator_next_full(context, &handle)?;
                if done {
                    return Ok(out);
                }
                out.push(v);
            }
        }

        let (iterator, next_method) = self.get_iterator_sync(context, iterable)?;
        // §7.4.13 — drive `IteratorStep` through the user iterator.
        // Each step calls into JS (the user's `next`), which can
        // trigger GC. Park the iterator + next-method handles on
        // the GC-traced anchor stack so a collection inside the
        // user code cannot reclaim them. The pop-to depth captured
        // here matches the LIFO push order even when the inner
        // body recurses into another `iterator_to_list_sync`.
        let anchor_depth = self.push_iteration_anchor(iterator);
        self.push_iteration_anchor(next_method);
        let mut values: Vec<Value> = Vec::new();
        let result = loop {
            match self.iterator_step_sync(context, &iterator, &next_method) {
                Ok(Some(value)) => values.push(value),
                Ok(None) => break Ok(values),
                Err(err) => {
                    // Best-effort close; original error wins.
                    let _ = self.iterator_close_sync(context, &iterator);
                    break Err(err);
                }
            }
        };
        self.pop_iteration_anchors_to(anchor_depth - 1);
        result
    }

    /// Complete the front async-generator request.
    pub(crate) fn async_generator_complete_step(
        &mut self,
        context: &ExecutionContext,
        handle: &crate::generator::JsGenerator,
        completion: Result<Value, Value>,
        done: bool,
    ) -> Result<(), VmError> {
        let Some(req) = handle.pop_async_request(&mut self.gc_heap) else {
            return Ok(());
        };
        self.async_generator_settle_capability(context, &req.capability, completion, done)
    }

    /// Settle an async-generator request capability without re-entering JS.
    pub(crate) fn async_generator_settle_capability(
        &mut self,
        _context: &ExecutionContext,
        capability: &PromiseCapability,
        completion: Result<Value, Value>,
        done: bool,
    ) -> Result<(), VmError> {
        let Some(promise) = capability.promise.as_promise() else {
            return Err(VmError::InvalidOperand);
        };
        let jobs = match completion {
            Ok(value) => {
                let record =
                    self.make_runtime_rooted_iter_result(value, done, &[&capability.promise], &[])?;
                promise.fulfill(&mut self.gc_heap, record)
            }
            Err(reason) => promise.reject(&mut self.gc_heap, reason),
        };
        for job in jobs.jobs {
            self.microtasks.enqueue(job);
        }
        Ok(())
    }

    /// Drain queued async-generator requests after the body is done.
    pub(crate) fn async_generator_drain_done(
        &mut self,
        context: &ExecutionContext,
        handle: &crate::generator::JsGenerator,
    ) -> Result<(), VmError> {
        handle.set_async_state(&mut self.gc_heap, AsyncGeneratorState::Draining);
        while let Some(resume) = handle.front_async_resume(&self.gc_heap) {
            match resume {
                GeneratorResumeKind::Throw(reason) => {
                    self.async_generator_complete_step(context, handle, Err(reason), true)?;
                }
                GeneratorResumeKind::Next(_) => {
                    self.async_generator_complete_step(
                        context,
                        handle,
                        Ok(Value::undefined()),
                        true,
                    )?;
                }
                GeneratorResumeKind::Return(value) => {
                    self.async_generator_complete_step(context, handle, Ok(value), true)?;
                }
            }
        }
        handle.set_async_state(&mut self.gc_heap, AsyncGeneratorState::Completed);
        Ok(())
    }

    /// Resume a generator object — drives the saved frame on a fresh sub-stack
    /// until either an [`otter_bytecode::Op::Yield`] pauses it (returning
    /// `{value, done: false}`) or the body runs to completion (returning
    /// `{value: returnValue, done: true}`).
    ///
    /// `kind` selects the entry behaviour per §27.5.3:
    /// - `Next(arg)`: write `arg` into the previous yield's dst and continue.
    /// - `Return(arg)`: act as if the body executed `return arg;` from the
    ///   current pc — foundation simplification: mark the generator done and
    ///   surface `{value: arg, done: true}` without running additional finally
    ///   blocks.
    /// - `Throw(reason)`: re-enter the body and immediately throw `reason`
    ///   from the current pc; finally / catch handlers take over per the
    ///   unwind machinery.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-generator.prototype.next>
    /// - <https://tc39.es/ecma262/#sec-generator.prototype.return>
    /// - <https://tc39.es/ecma262/#sec-generator.prototype.throw>
    pub fn resume_generator(
        &mut self,
        context: &ExecutionContext,
        handle: &crate::generator::JsGenerator,
        kind: GeneratorResumeKind,
    ) -> Result<Value, VmError> {
        // Already-done generators short-circuit per §27.5.1.2.
        let (frame_opt, resume_dst) = (
            handle.has_frame(&self.gc_heap),
            handle.resume_dst(&self.gc_heap),
        );
        if !frame_opt {
            return self.make_runtime_rooted_iter_result(Value::undefined(), true, &[], &[]);
        }
        // Pull the frame out of the gen body so we can mutate it.
        let (mut frame, cold) = match handle.take_frame(&mut self.gc_heap) {
            Some(pair) => pair,
            None => {
                return self.make_runtime_rooted_iter_result(Value::undefined(), true, &[], &[]);
            }
        };
        if let Some(c) = cold {
            self.frame_attach_cold(&mut frame, c);
        }
        // Apply the resume operation to the frame before re-entering
        // dispatch.
        let mut throw_value: Option<Value> = None;
        let mut return_value: Option<Value> = None;
        match &kind {
            GeneratorResumeKind::Next(arg) => {
                if frame.pc != 0
                    && let Some(slot) = frame.registers.get_mut(resume_dst as usize)
                {
                    *slot = *arg;
                }
            }
            GeneratorResumeKind::Return(arg) => {
                let closers = self
                    .frame_cold(&frame)
                    .map(|cold| cold.active_iterator_closers.clone())
                    .unwrap_or_default();
                for iterator in closers.iter().rev() {
                    self.iterator_close_value_sync(context, *iterator)?;
                }
                // §27.5.3.4 GeneratorResumeAbrupt(return) — if the body
                // is suspended inside a `try` with a `finally`, resume
                // it so those blocks run (a finally may even override
                // the completion). With no active finally, complete
                // immediately.
                let has_finally = self
                    .frame_cold(&frame)
                    .is_some_and(|c| c.handlers.iter().any(|h| h.finally_pc.is_some()));
                if !has_finally {
                    handle.mark_done(&mut self.gc_heap);
                    return self.make_runtime_rooted_iter_result(*arg, true, &[], &[]);
                }
                return_value = Some(*arg);
            }
            GeneratorResumeKind::Throw(reason) => {
                throw_value = Some(*reason);
            }
        }
        let mut sub_stack: SmallVec<[Frame; 8]> = SmallVec::new();
        sub_stack.push(*frame);
        if let Some(arg) = return_value {
            // Drive the parked frame's `finally` blocks via the abrupt
            // `return` path; `EndFinally` resumes the completion.
            match self.return_running_finally(&mut sub_stack, arg) {
                Ok(Some(v)) => {
                    handle.mark_done(&mut self.gc_heap);
                    return self.make_runtime_rooted_iter_result(v, true, &[], &[]);
                }
                Ok(None) => { /* finally parked; dispatch below runs it */ }
                Err(err) => {
                    handle.mark_done(&mut self.gc_heap);
                    return Err(err);
                }
            }
        }
        if let Some(reason) = throw_value {
            // Preserve the original throw value so the caller can
            // re-raise it on the outer stack when the gen body
            // does not catch it (the unwind_throw machinery
            // converts the value to a string when it surfaces as
            // VmError::Uncaught, losing the payload).
            self.pending_generator_throw = Some(reason);
            match self.unwind_throw(&mut sub_stack, reason) {
                Ok(_) => {}
                Err(err) => {
                    handle.mark_done(&mut self.gc_heap);
                    return Err(err);
                }
            }
            if sub_stack.is_empty() {
                handle.mark_done(&mut self.gc_heap);
                return Err(VmError::Uncaught {
                    value: "generator-throw".to_string(),
                });
            }
            // A handler caught the throw — clear the side channel.
            self.pending_generator_throw = None;
        }
        let is_async = handle.is_async(&self.gc_heap);
        if is_async {
            handle.set_async_state(&mut self.gc_heap, AsyncGeneratorState::Executing);
        }
        let outcome = self.dispatch_loop(context, &mut sub_stack);
        match outcome {
            Ok(value) => {
                // If a Yield fired, the gen body has the paused
                // frame back; surface yielded_value as the result.
                let yielded = handle.take_yielded(&mut self.gc_heap);
                if let Some(v) = yielded {
                    // Sync generators surface the iter result
                    // through the return value; async generators
                    // already settled their front request from inside
                    // `Op::Yield`.
                    if is_async {
                        return Ok(Value::undefined());
                    }
                    return self.make_runtime_rooted_iter_result(v, false, &[], &[]);
                }
                // Body ran to completion or `Op::Await` parked the
                // frame. Distinguish by whether the gen still owns
                // the frame: a parked await leaves the slot empty
                // (the await microtask owns it) AND `sub_stack` is
                // empty.
                let frame_taken_by_await =
                    handle.has_frame(&self.gc_heap) || sub_stack.is_empty() && is_async;
                let parked = is_async && !handle.has_frame(&self.gc_heap) && {
                    // The await machinery stored the parked frame
                    // in its closure, not on the gen handle. Detect
                    // that case by checking if queued requests still
                    // exist — if so, it is awaiting.
                    handle.has_async_requests(&self.gc_heap)
                };
                let _ = frame_taken_by_await;
                if parked {
                    // Body suspended on `Op::Await`; the resume
                    // microtask will eventually settle
                    // the queued request.
                    handle.set_async_state(&mut self.gc_heap, AsyncGeneratorState::Awaiting);
                    return Ok(Value::undefined());
                }
                // Body completed.
                handle.mark_done(&mut self.gc_heap);
                if is_async {
                    self.async_generator_complete_step(context, handle, Ok(value), true)?;
                    self.async_generator_drain_done(context, handle)?;
                    return Ok(Value::undefined());
                }
                self.make_runtime_rooted_iter_result(value, true, &[], &[])
            }
            Err(err) => {
                handle.mark_done(&mut self.gc_heap);
                if is_async {
                    if matches!(err, VmError::MissingReturn) {
                        self.async_generator_drain_done(context, handle)?;
                        return Ok(Value::undefined());
                    }
                    let rejection = if let Some(thrown) = self.pending_generator_throw.take() {
                        Some(thrown)
                    } else if let Some(thrown) = self.pending_uncaught_throw.take() {
                        Some(thrown)
                    } else {
                        self.vm_error_to_throwable_with_stack_roots(&sub_stack, &err)
                    };
                    if let Some(reason) = rejection {
                        self.async_generator_complete_step(context, handle, Err(reason), true)?;
                        self.async_generator_drain_done(context, handle)?;
                        return Ok(Value::undefined());
                    }
                }
                Err(err)
            }
        }
    }
    /// Drive one tick of [`Op::GetIterator`] for user objects.
    ///
    /// Returns `Ok(true)` when the dispatcher must restart the
    /// outer loop (frame pushed or pc advanced synchronously),
    /// `Ok(false)` when the source operand is a built-in iterable
    /// and the in-frame fast path should run instead.
    ///
    /// # Algorithm (§7.4.3 `GetIterator`)
    /// 1. **Resume** — when the running frame's
    ///    [`Frame::pending_get_iterator`] matches the current pc,
    ///    read the called function's result from `dst`. The result
    ///    must be an Object (the iterator). On non-Object, raise
    ///    `TypeMismatch` (foundation surface for §7.4.3 step 2's
    ///    TypeError; task 25 upgrades to a real Error).
    /// 2. **Fresh entry, built-in** — `Value::Array` / `String` /
    ///    `Map` / `Set` flow through the existing fast path.
    /// 3. **Fresh entry, user object** — look up
    ///    `[Symbol.iterator]`; if callable, push a frame to invoke
    ///    it with `this = obj`, no arguments. Pc stays on the
    ///    `Op::GetIterator` so resume can wrap the returned
    ///    iterator object as [`IteratorState::User`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-getiterator>
    pub(crate) fn drive_get_iterator(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let src = register_operand(operands.get(1))?;
        let top_idx = stack.len() - 1;
        let pc = stack[top_idx].pc;

        // 1. Resume path.
        let resume = self
            .frame_cold(&stack[top_idx])
            .and_then(|c| c.pending_get_iterator.as_ref())
            .filter(|s| s.pc == pc && s.dst == dst)
            .cloned();
        if let Some(_state) = resume {
            let produced = *read_register(&stack[top_idx], dst)?;
            // §7.4.3 step 2 — `[@@iterator]()` must return an
            // Object. Anything else is a TypeError.
            let produced_value = if let Some(iter) = produced.as_iterator() {
                Value::iterator(iter)
            } else {
                let iter_state = if let Some(handle) = produced.as_generator() {
                    IteratorState::Generator { handle }
                } else if produced.is_object()
                    || produced.is_proxy()
                    || produced.is_array()
                    || produced.is_map()
                    || produced.is_set()
                {
                    IteratorState::User { iterator: produced }
                } else {
                    if let Some(cold) = self.frame_cold_mut(&mut stack[top_idx]) {
                        cold.pending_get_iterator = None;
                    }
                    return Err(VmError::TypeMismatch);
                };
                let iter =
                    self.alloc_stack_rooted_iterator_state(stack, iter_state, &[&produced], &[])?;
                Value::iterator(iter)
            };
            write_register(&mut stack[top_idx], dst, produced_value)?;
            if let Some(cold) = self.frame_cold_mut(&mut stack[top_idx]) {
                cold.pending_get_iterator = None;
            }
            stack[top_idx].advance_pc(self.current_byte_len)?;
            return Ok(true);
        }

        // 2 + 3. Fresh entry — only intercept user objects. The
        // built-in fast path is the existing in-frame match arm.
        let value = *read_register(&stack[top_idx], src)?;
        let iter_sym = self.well_known_symbols.get(symbol::WellKnown::Iterator);
        if let Some(arr) = value.as_array() {
            let own_method = array::get_symbol_property(arr, &self.gc_heap, iter_sym);
            let proto = self.constructor_prototype_value("Array")?;
            let proto_has = if own_method.is_none() {
                self.ordinary_has_property_value(
                    context,
                    proto,
                    &VmPropertyKey::Symbol(iter_sym),
                    0,
                )?
            } else {
                false
            };
            if own_method.is_some() || proto_has {
                let callee = if let Some(method) = own_method {
                    method
                } else {
                    match self.ordinary_get_value(
                        context,
                        proto,
                        value,
                        &VmPropertyKey::Symbol(iter_sym),
                        0,
                    )? {
                        VmGetOutcome::Value(v) => v,
                        VmGetOutcome::InvokeGetter { getter } => {
                            self.run_callable_sync(context, &getter, value, SmallVec::new())?
                        }
                    }
                };
                if callee.is_undefined() || callee.is_null() || !is_callable(&callee) {
                    return Err(VmError::TypeMismatch);
                }
                self.frame_ensure_cold(&mut stack[top_idx])
                    .pending_get_iterator = Some(PendingGetIterator { pc, dst });
                self.invoke(stack, context, &callee, value, SmallVec::new(), dst)?;
                return Ok(true);
            }
            return Err(VmError::TypeMismatch);
        }
        // §23.2.3.32 %TypedArray%.prototype[@@iterator] — a TypedArray
        // is not an ordinary object, so route it through its
        // prototype's `@@iterator` (which returns a *live* array
        // iterator that observes element mutations during `for…of`).
        if value.as_typed_array(&self.gc_heap).is_some() {
            let callee = match self.ordinary_get_value(
                context,
                value,
                value,
                &VmPropertyKey::Symbol(iter_sym),
                0,
            )? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync(context, &getter, value, SmallVec::new())?
                }
            };
            if !is_callable(&callee) {
                return Err(VmError::TypeMismatch);
            }
            self.frame_ensure_cold(&mut stack[top_idx])
                .pending_get_iterator = Some(PendingGetIterator { pc, dst });
            self.invoke(stack, context, &callee, value, SmallVec::new(), dst)?;
            return Ok(true);
        }
        let Some(obj) = value.as_object() else {
            return Ok(false);
        };
        let Some(callee) = crate::object::get_symbol(obj, &self.gc_heap, iter_sym) else {
            // No `[Symbol.iterator]` — §7.4.3 step 2 throws.
            return Err(VmError::TypeMismatch);
        };
        if !is_callable(&callee) {
            return Err(VmError::TypeMismatch);
        }
        self.frame_ensure_cold(&mut stack[top_idx])
            .pending_get_iterator = Some(PendingGetIterator { pc, dst });
        let args: SmallVec<[Value; 8]> = SmallVec::new();
        // pc stays on Op::GetIterator; the called frame's result
        // lands in `dst` and the resume guard above wraps it.
        self.invoke(stack, context, &callee, value, args, dst)?;
        Ok(true)
    }

    /// Drive one tick of [`Op::IteratorNext`] for user iterators.
    ///
    /// Returns `Ok(true)` when the dispatcher must restart (frame
    /// pushed or pc advanced synchronously), `Ok(false)` when the
    /// iterator is a built-in synchronous shape and the in-frame
    /// fast path should run.
    ///
    /// # Algorithm (§7.4.5 `IteratorNext`)
    /// 1. **Resume** — read the result record from the scratch
    ///    register; pull `value` and `done`; truthy `done`
    ///    transitions the iterator to `Exhausted` per §7.4.2 step 6.
    /// 2. **Fresh entry, built-in iterator** — fall through.
    /// 3. **Fresh entry, user iterator** — look up `iterator.next`,
    ///    push a frame to invoke it with `this = iterator`, no
    ///    arguments. Result lands in a scratch slot adjacent to
    ///    the `value` / `done` destinations.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-iteratornext>
    /// - <https://tc39.es/ecma262/#sec-iteratorcomplete>
    /// - <https://tc39.es/ecma262/#sec-iteratorvalue>
    pub(crate) fn drive_iterator_next(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let value_dst = register_operand(operands.first())?;
        let done_dst = register_operand(operands.get(1))?;
        let iter_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let pc = stack[top_idx].pc;

        // 1. Resume path — read the parked record.
        let resume = self
            .frame_cold(&stack[top_idx])
            .and_then(|c| c.pending_iterator_next.as_ref())
            .filter(|s| s.pc == pc && s.value_dst == value_dst && s.done_dst == done_dst)
            .cloned();
        if let Some(state) = resume {
            let result = *read_register(&stack[top_idx], state.result_reg)?;
            let Some(obj) = result.as_object() else {
                if let Some(cold) = self.frame_cold_mut(&mut stack[top_idx]) {
                    cold.pending_iterator_next = None;
                }
                return Err(VmError::TypeMismatch);
            };
            let step = iterator_step_read(self, context, &Value::object(obj))?;
            let (value, done) = match step {
                Some(value) => (value, false),
                None => (Value::undefined(), true),
            };
            if done && let Some(rc) = state.iterator.as_iterator() {
                self.gc_heap
                    .with_payload(rc, |state| *state = IteratorState::Exhausted);
            }
            write_register(&mut stack[top_idx], value_dst, value)?;
            write_register(&mut stack[top_idx], done_dst, Value::boolean(done))?;
            if let Some(cold) = self.frame_cold_mut(&mut stack[top_idx]) {
                cold.pending_iterator_next = None;
            }
            stack[top_idx].advance_pc(self.current_byte_len)?;
            return Ok(true);
        }

        // 2 + 3. Fresh entry. Inspect the iterator's inner state.
        let iter_value = *read_register(&stack[top_idx], iter_reg)?;
        let Some(iter_rc_handle) = iter_value.as_iterator() else {
            return Err(VmError::TypeMismatch);
        };
        let iter_rc = &iter_rc_handle;
        // §27.5 generator-state path — drive the suspended body
        // synchronously and write the unpacked `value` / `done`
        // pair into the caller's destination registers.
        let gen_handle = self.gc_heap.read_payload(*iter_rc, |state| match state {
            IteratorState::Generator { handle } => Some(*handle),
            _ => None,
        });
        if let Some(handle) = gen_handle {
            let result = self.resume_generator(
                context,
                &handle,
                GeneratorResumeKind::Next(Value::undefined()),
            )?;
            let Some(obj) = result.as_object() else {
                return Err(VmError::TypeMismatch);
            };
            let value =
                crate::object::get(obj, &self.gc_heap, "value").unwrap_or(Value::undefined());
            let done = crate::object::get(obj, &self.gc_heap, "done")
                .unwrap_or(Value::undefined())
                .to_boolean(&self.gc_heap);
            if done {
                self.gc_heap
                    .with_payload(*iter_rc, |state| *state = IteratorState::Exhausted);
            }
            write_register(&mut stack[top_idx], value_dst, value)?;
            write_register(&mut stack[top_idx], done_dst, Value::boolean(done))?;
            stack[top_idx].advance_pc(self.current_byte_len)?;
            return Ok(true);
        }
        // Helper-wrapper and RegExp-String iterator states drive
        // through the interpreter-aware step path: the former need to
        // run user callbacks, the latter re-enters `RegExpExec` (a JS
        // `exec` that the synchronous `step_iterator` cannot call).
        let needs_full_step = self.gc_heap.read_payload(*iter_rc, |state| {
            matches!(
                state,
                IteratorState::Map { .. }
                    | IteratorState::Filter { .. }
                    | IteratorState::Take { .. }
                    | IteratorState::Drop { .. }
                    | IteratorState::FlatMap { .. }
                    | IteratorState::RegExpString { .. }
            )
        });
        if needs_full_step {
            let (value, done) = self.iterator_next_full(context, iter_rc)?;
            write_register(&mut stack[top_idx], value_dst, value)?;
            write_register(&mut stack[top_idx], done_dst, Value::boolean(done))?;
            stack[top_idx].advance_pc(self.current_byte_len)?;
            return Ok(true);
        }
        // Snapshot the user iterator object out of the inner
        // state so the borrow does not span the `invoke` call
        // below.
        let user_iter = self.gc_heap.read_payload(*iter_rc, |state| match state {
            IteratorState::User { iterator } => Some(*iterator),
            _ => None,
        });
        let Some(user_iter_value) = user_iter else {
            // Built-in iterator — let the synchronous in-frame
            // path drive it.
            return Ok(false);
        };
        // Already-exhausted user iterators short-circuit per
        // §7.4.2 step 6.
        let Some(iter_obj) = user_iter_value.as_object() else {
            return Err(VmError::TypeMismatch);
        };
        let next_fn =
            crate::object::get(iter_obj, &self.gc_heap, "next").ok_or(VmError::TypeMismatch)?;
        if !is_callable(&next_fn) {
            return Err(VmError::TypeMismatch);
        }
        // Park the state and push a call. `result_reg` reuses the
        // `value_dst` slot — the resume step overwrites it with
        // the unpacked value before the user code observes it.
        self.frame_ensure_cold(&mut stack[top_idx])
            .pending_iterator_next = Some(PendingIteratorNext {
            pc,
            value_dst,
            done_dst,
            result_reg: value_dst,
            iterator: iter_value,
        });
        let args: SmallVec<[Value; 8]> = SmallVec::new();
        self.invoke(stack, context, &next_fn, user_iter_value, args, value_dst)?;
        Ok(true)
    }
}

fn iterator_step_read(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    result: &Value,
) -> Result<Option<Value>, VmError> {
    let done_value = match interp.ordinary_get_value(
        context,
        *result,
        *result,
        &VmPropertyKey::String("done"),
        0,
    )? {
        VmGetOutcome::Value(v) => v,
        VmGetOutcome::InvokeGetter { getter } => {
            interp.run_callable_sync(context, &getter, *result, SmallVec::new())?
        }
    };
    if done_value.to_boolean(interp.gc_heap()) {
        return Ok(None);
    }
    let value = match interp.ordinary_get_value(
        context,
        *result,
        *result,
        &VmPropertyKey::String("value"),
        0,
    )? {
        VmGetOutcome::Value(v) => v,
        VmGetOutcome::InvokeGetter { getter } => {
            interp.run_callable_sync(context, &getter, *result, SmallVec::new())?
        }
    };
    Ok(Some(value))
}
