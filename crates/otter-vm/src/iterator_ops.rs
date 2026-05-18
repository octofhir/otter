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
    JsString, NativeError, PendingGetIterator, PendingIteratorNext, Value, VmError, VmGetOutcome,
    VmPropertyKey, array, is_callable, operand_decode::register_operand, read_register,
    require_callable, step_iterator, symbol, take_drop_count, value_kind_name, write_register,
};

/// Cloned snapshot of an [`IteratorState`] taken before driving a
/// helper callback so the GC body borrow does not span dispatch.
enum IteratorStateSnapshot {
    User(Value),
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
        let value = read_register(frame, src)?.clone();
        let state = match &value {
            Value::Array(array) => IteratorState::Array {
                array: *array,
                index: 0,
            },
            Value::String(string) => IteratorState::String {
                string: string.clone(),
                index: 0,
            },
            // `for…of` over a `Map` yields `[key, value]` pairs (Spec
            // §24.1.3.12 — `@@iterator` aliases `entries`); over a `Set`
            // yields values (Spec §24.2.3.11). Snapshot at iteration start by
            // building a synthetic backing array.
            Value::Map(m) => {
                let entries = crate::collections::map_entries(*m, &self.gc_heap);
                let entries_len = entries.len();
                let mut entry_values: Vec<Value> = Vec::with_capacity(entries_len * 2);
                for (k, v) in entries {
                    entry_values.push(k);
                    entry_values.push(v);
                }
                let mut snap: SmallVec<[Value; 4]> = SmallVec::with_capacity(entries_len);
                for pair in entry_values.chunks_exact(2) {
                    let pair_array = self.alloc_stack_rooted_array_from_values_with_root_slices(
                        stack,
                        pair.iter().cloned(),
                        &[&value],
                        &[entry_values.as_slice(), snap.as_slice()],
                    )?;
                    snap.push(Value::Array(pair_array));
                }
                IteratorState::Array {
                    array: self.alloc_stack_rooted_array_from_values_with_root_slices(
                        stack,
                        snap.iter().cloned(),
                        &[&value],
                        &[entry_values.as_slice(), snap.as_slice()],
                    )?,
                    index: 0,
                }
            }
            Value::Set(s) => {
                let snap: SmallVec<[Value; 4]> = crate::collections::set_values(*s, &self.gc_heap)
                    .into_iter()
                    .collect();
                IteratorState::Array {
                    array: self.alloc_stack_rooted_array_from_values(
                        stack,
                        snap.iter().cloned(),
                        &[&value],
                        snap.as_slice(),
                    )?,
                    index: 0,
                }
            }
            // §27.5 — generator objects are iterable; `[@@iterator]()` returns
            // the generator itself, and `next()` drives the suspended body.
            Value::Generator(handle) => IteratorState::Generator { handle: *handle },
            // Already-an-iterator should pass through unchanged.
            Value::Iterator(rc) => {
                let frame = &mut stack[top_idx];
                write_register(frame, dst, Value::Iterator(*rc))?;
                frame.pc += 1;
                return Ok(());
            }
            _ => return Err(VmError::TypeMismatch),
        };
        let iter = self.alloc_stack_rooted_iterator_state(stack, state, &[&value], &[])?;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, Value::Iterator(iter))?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_iterator_next_regs(
        &mut self,
        frame: &mut Frame,
        value_dst: u16,
        done_dst: u16,
        iter_reg: u16,
    ) -> Result<(), VmError> {
        let iter = match read_register(frame, iter_reg)? {
            Value::Iterator(rc) => *rc,
            _ => return Err(VmError::TypeMismatch),
        };
        let (value, done) = step_iterator(iter, &self.string_heap, &mut self.gc_heap)?;
        write_register(frame, value_dst, value)?;
        write_register(frame, done_dst, Value::Boolean(done))?;
        frame.pc += 1;
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
        match step_iterator(*iter, &self.string_heap, &mut self.gc_heap) {
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
                IteratorState::User { iterator } => {
                    Some(IteratorStateSnapshot::User(iterator.clone()))
                }
                IteratorState::Generator { handle } => {
                    Some(IteratorStateSnapshot::Generator(*handle))
                }
                IteratorState::Map { source, mapper } => Some(IteratorStateSnapshot::Map {
                    source: *source,
                    mapper: mapper.clone(),
                }),
                IteratorState::Filter { source, predicate } => {
                    Some(IteratorStateSnapshot::Filter {
                        source: *source,
                        predicate: predicate.clone(),
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
                    mapper: mapper.clone(),
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
                    GeneratorResumeKind::Next(Value::Undefined),
                )?;
                let Value::Object(record) = &result else {
                    return Err(VmError::TypeMismatch);
                };
                let value =
                    crate::object::get(*record, &self.gc_heap, "value").unwrap_or(Value::Undefined);
                let done = crate::object::get(*record, &self.gc_heap, "done")
                    .unwrap_or(Value::Undefined)
                    .to_boolean();
                if done {
                    self.gc_heap
                        .with_payload(*iter, |state| *state = IteratorState::Exhausted);
                }
                Ok((value, done))
            }
            IteratorStateSnapshot::User(iter_value) => {
                let Value::Object(iter_obj) = &iter_value else {
                    return Err(VmError::TypeMismatch);
                };
                let next_fn = crate::object::get(*iter_obj, &self.gc_heap, "next")
                    .ok_or(VmError::TypeMismatch)?;
                if !self.is_callable_runtime(&next_fn) {
                    return Err(VmError::TypeMismatch);
                }
                let result =
                    self.run_callable_sync(context, &next_fn, iter_value.clone(), SmallVec::new())?;
                let Value::Object(record) = &result else {
                    return Err(VmError::TypeMismatch);
                };
                let value =
                    crate::object::get(*record, &self.gc_heap, "value").unwrap_or(Value::Undefined);
                let done = crate::object::get(*record, &self.gc_heap, "done")
                    .unwrap_or(Value::Undefined)
                    .to_boolean();
                if done {
                    self.gc_heap
                        .with_payload(*iter, |state| *state = IteratorState::Exhausted);
                }
                Ok((value, done))
            }
            IteratorStateSnapshot::Map { source, mapper } => {
                let (v, done) = self.iterator_next_full(context, &source)?;
                if done {
                    self.gc_heap
                        .with_payload(*iter, |state| *state = IteratorState::Exhausted);
                    return Ok((Value::Undefined, true));
                }
                let mapped = self.run_callable_sync(
                    context,
                    &mapper,
                    Value::Undefined,
                    smallvec::smallvec![v],
                )?;
                Ok((mapped, false))
            }
            IteratorStateSnapshot::Filter { source, predicate } => loop {
                let (v, done) = self.iterator_next_full(context, &source)?;
                if done {
                    self.gc_heap
                        .with_payload(*iter, |state| *state = IteratorState::Exhausted);
                    return Ok((Value::Undefined, true));
                }
                let kept = self.run_callable_sync(
                    context,
                    &predicate,
                    Value::Undefined,
                    smallvec::smallvec![v.clone()],
                )?;
                if kept.to_boolean() {
                    return Ok((v, false));
                }
            },
            IteratorStateSnapshot::Take { source, remaining } => {
                if remaining == 0 {
                    self.gc_heap
                        .with_payload(*iter, |state| *state = IteratorState::Exhausted);
                    return Ok((Value::Undefined, true));
                }
                let (v, done) = self.iterator_next_full(context, &source)?;
                if done {
                    self.gc_heap
                        .with_payload(*iter, |state| *state = IteratorState::Exhausted);
                    return Ok((Value::Undefined, true));
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
                        return Ok((Value::Undefined, true));
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
                    return Ok((Value::Undefined, true));
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
                    return Ok((Value::Undefined, true));
                }
                let mapped = self.run_callable_sync(
                    context,
                    &mapper,
                    Value::Undefined,
                    smallvec::smallvec![v],
                )?;
                // §27.5.1.10 step 7.b.iv — `GetIteratorFlattenable(mapped)`
                // accepts any iterable (Array / Set / Map / String /
                // Generator / Object with `@@iterator`) and any
                // existing iterator. Non-iterable primitives throw
                // TypeError. The Iterator-helpers spec rejects raw
                // values that aren't iterables.
                let inner_state = match mapped {
                    Value::Array(arr) => IteratorState::Array {
                        array: arr,
                        index: 0,
                    },
                    Value::Iterator(rc) => {
                        let new_inner = rc;
                        self.gc_heap.with_payload(*iter, |state| {
                            if let IteratorState::FlatMap { inner: slot, .. } = state {
                                *slot = Some(new_inner);
                            }
                        });
                        inner = Some(new_inner);
                        continue;
                    }
                    Value::Generator(g) => IteratorState::Generator { handle: g },
                    Value::String(s) => IteratorState::String { string: s, index: 0 },
                    Value::Set(_) | Value::Map(_) | Value::Object(_) => {
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
                        let outcome = self.ordinary_get_value(
                            context,
                            mapped.clone(),
                            mapped.clone(),
                            &key,
                            0,
                        )?;
                        let iter_method = match outcome {
                            crate::VmGetOutcome::Value(v) => v,
                            crate::VmGetOutcome::InvokeGetter { getter } => self.run_callable_sync(
                                context,
                                &getter,
                                mapped.clone(),
                                SmallVec::new(),
                            )?,
                        };
                        let iter_value = if matches!(iter_method, Value::Undefined | Value::Null) {
                            // Iterator-without-`@@iterator` shape —
                            // wrap the mapped object directly so
                            // subsequent `IteratorNext` calls invoke
                            // its own `.next`.
                            mapped.clone()
                        } else if self.is_callable_runtime(&iter_method) {
                            self.run_callable_sync(
                                context,
                                &iter_method,
                                mapped.clone(),
                                SmallVec::new(),
                            )?
                        } else {
                            return Err(VmError::TypeError {
                                message: "Iterator.prototype.flatMap mapper return must be iterable"
                                    .to_string(),
                            });
                        };
                        if let Value::Iterator(rc) = iter_value {
                            let new_inner = rc;
                            self.gc_heap.with_payload(*iter, |state| {
                                if let IteratorState::FlatMap { inner: slot, .. } = state {
                                    *slot = Some(new_inner);
                                }
                            });
                            inner = Some(new_inner);
                            continue;
                        }
                        if let Value::Generator(g) = iter_value {
                            IteratorState::Generator { handle: g }
                        } else {
                            IteratorState::User {
                                iterator: iter_value,
                            }
                        }
                    }
                    _ => {
                        return Err(VmError::TypeError {
                            message: "Iterator.prototype.flatMap mapper return must be iterable"
                                .to_string(),
                        });
                    }
                };
                let iter_root = Value::Iterator(*iter);
                let source_root = Value::Iterator(source);
                let mapper_root = mapper.clone();
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

    /// Build a `Value::NativeFunction` that, when called, re-dispatches the
    /// corresponding iterator method on the captured receiver. Used by
    /// `Op::LoadProperty` so `typeof it.next === "function"` and detached
    /// invocation (`const f = it.next; f();`) both work.
    pub(crate) fn synthesize_iterator_method(
        &mut self,
        stack: &SmallVec<[Frame; 8]>,
        name: &str,
        receiver: Value,
    ) -> Result<Value, VmError> {
        let method_name: &'static str = match name {
            "next" => "next",
            "return" => "return",
            "throw" => "throw",
            _ => return Ok(Value::Undefined),
        };
        let captures: smallvec::SmallVec<[Value; 4]> = smallvec::smallvec![receiver];
        let value = self.native_value_with_captures_stack_rooted(
            stack,
            method_name,
            captures,
            &[],
            &[],
            move |ctx, args, captures| {
                let receiver = captures.first().cloned().unwrap_or(Value::Undefined);
                let Value::Iterator(iter_rc) = receiver.clone() else {
                    return Err(NativeError::TypeError {
                        name: method_name,
                        reason: "receiver is not an Iterator".to_string(),
                    });
                };
                let (interp, exec) = ctx.interp_mut_and_context();
                let exec = exec.ok_or_else(|| NativeError::TypeError {
                    name: method_name,
                    reason: "missing execution context".to_string(),
                })?;
                let small_args: smallvec::SmallVec<[Value; 8]> = args.iter().cloned().collect();
                match method_name {
                    "next" => {
                        let (v, done) =
                            interp.iterator_next_full(&exec, &iter_rc).map_err(|e| {
                                NativeError::TypeError {
                                    name: method_name,
                                    reason: e.to_string(),
                                }
                            })?;
                        let obj = interp
                            .alloc_runtime_rooted_object_with_roots(
                                &[&receiver, &v],
                                &[small_args.as_slice()],
                            )
                            .map_err(|e| NativeError::TypeError {
                                name: method_name,
                                reason: e.to_string(),
                            })?;
                        crate::object::set(obj, &mut interp.gc_heap, "value", v);
                        crate::object::set(obj, &mut interp.gc_heap, "done", Value::Boolean(done));
                        Ok(Value::Object(obj))
                    }
                    "return" => {
                        let arg = small_args.first().cloned().unwrap_or(Value::Undefined);
                        let obj = interp
                            .alloc_runtime_rooted_object_with_roots(
                                &[&receiver, &arg],
                                &[small_args.as_slice()],
                            )
                            .map_err(|e| NativeError::TypeError {
                                name: method_name,
                                reason: e.to_string(),
                            })?;
                        crate::object::set(obj, &mut interp.gc_heap, "value", arg);
                        crate::object::set(obj, &mut interp.gc_heap, "done", Value::Boolean(true));
                        Ok(Value::Object(obj))
                    }
                    _ => Err(NativeError::Thrown {
                        name: method_name,
                        message: value_kind_name(
                            &small_args.first().cloned().unwrap_or(Value::Undefined),
                        )
                        .to_string(),
                    }),
                }
            },
        )?;
        Ok(value)
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
        let iter_value = Value::Iterator(*iter_rc);
        let result = match name {
            "map" => {
                let mapper = require_callable(args.first())?;
                let mapper_root = mapper.clone();
                let state = IteratorState::Map {
                    source: *iter_rc,
                    mapper,
                };
                Value::Iterator(self.alloc_stack_rooted_iterator_state(
                    stack,
                    state,
                    &[&iter_value, &mapper_root],
                    &[],
                )?)
            }
            "filter" => {
                let predicate = require_callable(args.first())?;
                let predicate_root = predicate.clone();
                let state = IteratorState::Filter {
                    source: *iter_rc,
                    predicate,
                };
                Value::Iterator(self.alloc_stack_rooted_iterator_state(
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
                Value::Iterator(self.alloc_stack_rooted_iterator_state(
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
                Value::Iterator(self.alloc_stack_rooted_iterator_state(
                    stack,
                    state,
                    &[&iter_value],
                    &[],
                )?)
            }
            "flatMap" => {
                let mapper = require_callable(args.first())?;
                let mapper_root = mapper.clone();
                let state = IteratorState::FlatMap {
                    source: *iter_rc,
                    mapper,
                    inner: None,
                };
                Value::Iterator(self.alloc_stack_rooted_iterator_state(
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
                Value::Array(result)
            }
            "forEach" => {
                let callback = require_callable(args.first())?;
                let collected = self.drain_iterator(context, iter_rc)?;
                for v in collected {
                    self.run_callable_sync(
                        context,
                        &callback,
                        Value::Undefined,
                        smallvec::smallvec![v],
                    )?;
                }
                Value::Undefined
            }
            "reduce" => {
                let reducer = require_callable(args.first())?;
                let has_initial = args.len() >= 2;
                let mut acc = if has_initial {
                    args[1].clone()
                } else {
                    Value::Undefined
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
                        Value::Undefined,
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
                crate::object::set(obj, &mut self.gc_heap, "value", v);
                crate::object::set(obj, &mut self.gc_heap, "done", Value::Boolean(done));
                Value::Object(obj)
            }
            // §27.1.3 / §27.1.4 — `return` / `throw` on plain
            // array-backed iterators are no-ops that fold the
            // iterator to its completion state. Generator-style
            // iterators are handled by the dedicated
            // `Value::Generator` dispatch above.
            "return" => {
                let arg = args.first().cloned().unwrap_or(Value::Undefined);
                let obj =
                    self.alloc_stack_rooted_object_with_extra_roots(stack, &[&iter_value, &arg])?;
                crate::object::set(obj, &mut self.gc_heap, "value", arg);
                crate::object::set(obj, &mut self.gc_heap, "done", Value::Boolean(true));
                Value::Object(obj)
            }
            "throw" => {
                let arg = args.first().cloned().unwrap_or(Value::Undefined);
                return Err(VmError::Uncaught {
                    value: value_kind_name(&arg).to_string(),
                });
            }
            _ => return Ok(false),
        };
        let top_idx = stack.len() - 1;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, result)?;
        frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
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
            iterable.clone(),
            iterable.clone(),
            &VmPropertyKey::Symbol(iterator_sym),
            0,
        )? {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync(context, &getter, iterable.clone(), SmallVec::new())?
            }
        };
        if matches!(method, Value::Undefined | Value::Null) {
            return Err(VmError::TypeError {
                message: "iterator method is not callable".to_string(),
            });
        }
        if !self.is_callable_runtime(&method) {
            return Err(VmError::TypeError {
                message: "iterator method is not callable".to_string(),
            });
        }
        let iterator =
            self.run_callable_sync(context, &method, iterable.clone(), SmallVec::new())?;
        if !matches!(
            iterator,
            Value::Object(_)
                | Value::Proxy(_)
                | Value::Array(_)
                | Value::Iterator(_)
                | Value::Map(_)
                | Value::Set(_)
                | Value::Generator(_)
        ) {
            return Err(VmError::TypeError {
                message: "iterator method did not return an object".to_string(),
            });
        }
        let next_method = match self.ordinary_get_value(
            context,
            iterator.clone(),
            iterator.clone(),
            &VmPropertyKey::String("next"),
            0,
        )? {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync(context, &getter, iterator.clone(), SmallVec::new())?
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
        let result =
            self.run_callable_sync(context, next_method, iterator.clone(), SmallVec::new())?;
        if !matches!(result, Value::Object(_) | Value::Proxy(_)) {
            return Err(VmError::TypeError {
                message: "iterator result is not an object".to_string(),
            });
        }
        let done_value = match self.ordinary_get_value(
            context,
            result.clone(),
            result.clone(),
            &VmPropertyKey::String("done"),
            0,
        )? {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync(context, &getter, result.clone(), SmallVec::new())?
            }
        };
        if done_value.to_boolean() {
            return Ok(None);
        }
        let value = match self.ordinary_get_value(
            context,
            result.clone(),
            result.clone(),
            &VmPropertyKey::String("value"),
            0,
        )? {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync(context, &getter, result.clone(), SmallVec::new())?
            }
        };
        Ok(Some(value))
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
            iterator.clone(),
            iterator.clone(),
            &VmPropertyKey::String("return"),
            0,
        )? {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync(context, &getter, iterator.clone(), SmallVec::new())?
            }
        };
        if matches!(return_method, Value::Undefined | Value::Null) {
            return Ok(());
        }
        if !self.is_callable_runtime(&return_method) {
            return Err(VmError::TypeError {
                message: "iterator `return` is not callable".to_string(),
            });
        }
        let result =
            self.run_callable_sync(context, &return_method, iterator.clone(), SmallVec::new())?;
        if !matches!(result, Value::Object(_) | Value::Proxy(_)) {
            return Err(VmError::TypeError {
                message: "iterator `return` did not yield an object".to_string(),
            });
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
        match iterable {
            Value::Array(arr) => {
                let elements =
                    array::with_elements(*arr, &self.gc_heap, |elements| elements.to_vec());
                return Ok(elements);
            }
            Value::String(s) => {
                let len = s.len() as usize;
                let mut out = Vec::with_capacity(len);
                for i in 0..s.len() {
                    let unit = s.char_code_at(i).unwrap_or(0);
                    let unit_str = JsString::from_utf16_units(&[unit], &self.string_heap)?;
                    out.push(Value::String(unit_str));
                }
                return Ok(out);
            }
            Value::Set(s) => return Ok(crate::collections::set_values(*s, &mut self.gc_heap)),
            Value::Map(m) => {
                let pairs = crate::collections::map_entries(*m, &mut self.gc_heap);
                let mut out = Vec::with_capacity(pairs.len());
                for (k, v) in pairs {
                    let entry = self.alloc_runtime_rooted_array_from_values(
                        [k.clone(), v.clone()],
                        &[iterable, &k, &v],
                        &[out.as_slice()],
                    )?;
                    out.push(Value::Array(entry));
                }
                return Ok(out);
            }
            Value::Generator(handle) => {
                let handle = *handle;
                let mut out: Vec<Value> = Vec::new();
                loop {
                    let result = self.resume_generator(
                        context,
                        &handle,
                        GeneratorResumeKind::Next(Value::Undefined),
                    )?;
                    let Value::Object(record) = &result else {
                        return Err(VmError::TypeError {
                            message: "generator next did not return an object".to_string(),
                        });
                    };
                    let done = crate::object::get(*record, &self.gc_heap, "done")
                        .unwrap_or(Value::Undefined)
                        .to_boolean();
                    if done {
                        return Ok(out);
                    }
                    let value = crate::object::get(*record, &self.gc_heap, "value")
                        .unwrap_or(Value::Undefined);
                    out.push(value);
                }
            }
            // §27.5 IteratorRecord drain — `Value::Iterator` wraps a
            // foundation `IteratorState`. Drive it through
            // `iterator_next_full` so lazy combinators (Map / Filter
            // / Take / Drop / FlatMap) and user iterators all share
            // the same termination contract.
            Value::Iterator(handle) => {
                let handle = *handle;
                let mut out: Vec<Value> = Vec::new();
                loop {
                    let (v, done) = self.iterator_next_full(context, &handle)?;
                    if done {
                        return Ok(out);
                    }
                    out.push(v);
                }
            }
            _ => {}
        }

        let (iterator, next_method) = self.get_iterator_sync(context, iterable)?;
        let mut values: Vec<Value> = Vec::new();
        loop {
            match self.iterator_step_sync(context, &iterator, &next_method) {
                Ok(Some(value)) => values.push(value),
                Ok(None) => return Ok(values),
                Err(err) => {
                    // Best-effort close; original error wins.
                    let _ = self.iterator_close_sync(context, &iterator);
                    return Err(err);
                }
            }
        }
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
            return self.make_runtime_rooted_iter_result(Value::Undefined, true, &[], &[]);
        }
        // Pull the frame out of the gen body so we can mutate it.
        let mut frame = match handle.take_frame(&mut self.gc_heap) {
            Some(f) => f,
            None => return self.make_runtime_rooted_iter_result(Value::Undefined, true, &[], &[]),
        };
        // Apply the resume operation to the frame before re-entering
        // dispatch.
        let mut throw_value: Option<Value> = None;
        match &kind {
            GeneratorResumeKind::Next(arg) => {
                if frame.pc != 0
                    && let Some(slot) = frame.registers.get_mut(resume_dst as usize)
                {
                    *slot = arg.clone();
                }
            }
            GeneratorResumeKind::Return(arg) => {
                // Foundation: mark done and surface arg without
                // running the body further.
                handle.mark_done(&mut self.gc_heap);
                return self.make_runtime_rooted_iter_result(arg.clone(), true, &[], &[]);
            }
            GeneratorResumeKind::Throw(reason) => {
                throw_value = Some(reason.clone());
            }
        }
        let mut sub_stack: SmallVec<[Frame; 8]> = SmallVec::new();
        sub_stack.push(*frame);
        if let Some(reason) = throw_value {
            // Preserve the original throw value so the caller can
            // re-raise it on the outer stack when the gen body
            // does not catch it (the unwind_throw machinery
            // converts the value to a string when it surfaces as
            // VmError::Uncaught, losing the payload).
            self.pending_generator_throw = Some(reason.clone());
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
        let outcome = self.dispatch_loop(context, &mut sub_stack);
        match outcome {
            Ok(value) => {
                // If a Yield fired, the gen body has the paused
                // frame back; surface yielded_value as the result.
                let yielded = handle.take_yielded(&mut self.gc_heap);
                if let Some(v) = yielded {
                    // Sync generators surface the iter result
                    // through the return value; async generators
                    // already settled `pending_request` from inside
                    // `Op::Yield`.
                    if is_async {
                        return Ok(Value::Undefined);
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
                    // that case by checking if pending_request is
                    // still set — if so, it's awaiting.
                    handle.has_pending_request(&self.gc_heap)
                };
                let _ = frame_taken_by_await;
                if parked {
                    // Body suspended on `Op::Await`; the resume
                    // microtask will eventually settle
                    // `pending_request`.
                    return Ok(Value::Undefined);
                }
                // Body completed.
                handle.mark_done(&mut self.gc_heap);
                if is_async {
                    if let Some(req) = handle.take_pending_request(&mut self.gc_heap) {
                        let record = self.make_runtime_rooted_iter_result(
                            value,
                            true,
                            &[&req.resolve],
                            &[],
                        )?;
                        self.run_callable_sync(
                            context,
                            &req.resolve,
                            Value::Undefined,
                            smallvec::smallvec![record],
                        )?;
                    }
                    return Ok(Value::Undefined);
                }
                self.make_runtime_rooted_iter_result(value, true, &[], &[])
            }
            Err(err) => {
                handle.mark_done(&mut self.gc_heap);
                if is_async {
                    // Pending request stays alive — the caller
                    // (do_call_method_value) settles it on the
                    // pending_generator_throw side-channel.
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
        let resume = stack[top_idx]
            .pending_get_iterator
            .as_ref()
            .filter(|s| s.pc == pc && s.dst == dst)
            .cloned();
        if let Some(_state) = resume {
            let produced = read_register(&stack[top_idx], dst)?.clone();
            // §7.4.3 step 2 — `[@@iterator]()` must return an
            // Object. Anything else is a TypeError.
            if !matches!(produced, Value::Object(_)) {
                stack[top_idx].pending_get_iterator = None;
                return Err(VmError::TypeMismatch);
            }
            let iter_state = IteratorState::User {
                iterator: produced.clone(),
            };
            let iter =
                self.alloc_stack_rooted_iterator_state(stack, iter_state, &[&produced], &[])?;
            write_register(&mut stack[top_idx], dst, Value::Iterator(iter))?;
            stack[top_idx].pending_get_iterator = None;
            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            return Ok(true);
        }

        // 2 + 3. Fresh entry — only intercept user objects. The
        // built-in fast path is the existing in-frame match arm.
        let value = read_register(&stack[top_idx], src)?.clone();
        let Value::Object(obj) = &value else {
            return Ok(false);
        };
        let iter_sym = self.well_known_symbols.get(symbol::WellKnown::Iterator);
        let Some(callee) = crate::object::get_symbol(*obj, &self.gc_heap, &iter_sym) else {
            // No `[Symbol.iterator]` — §7.4.3 step 2 throws.
            return Err(VmError::TypeMismatch);
        };
        if !is_callable(&callee) {
            return Err(VmError::TypeMismatch);
        }
        stack[top_idx].pending_get_iterator = Some(PendingGetIterator { pc, dst });
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
        let resume = stack[top_idx]
            .pending_iterator_next
            .as_ref()
            .filter(|s| s.pc == pc && s.value_dst == value_dst && s.done_dst == done_dst)
            .cloned();
        if let Some(state) = resume {
            let result = read_register(&stack[top_idx], state.result_reg)?.clone();
            let Value::Object(obj) = &result else {
                stack[top_idx].pending_iterator_next = None;
                return Err(VmError::TypeMismatch);
            };
            let value =
                crate::object::get(*obj, &self.gc_heap, "value").unwrap_or(Value::Undefined);
            let done_value =
                crate::object::get(*obj, &self.gc_heap, "done").unwrap_or(Value::Undefined);
            let done = done_value.to_boolean();
            if done && let Value::Iterator(rc) = &state.iterator {
                self.gc_heap
                    .with_payload(*rc, |state| *state = IteratorState::Exhausted);
            }
            write_register(&mut stack[top_idx], value_dst, value)?;
            write_register(&mut stack[top_idx], done_dst, Value::Boolean(done))?;
            stack[top_idx].pending_iterator_next = None;
            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            return Ok(true);
        }

        // 2 + 3. Fresh entry. Inspect the iterator's inner state.
        let iter_value = read_register(&stack[top_idx], iter_reg)?.clone();
        let Value::Iterator(iter_rc) = &iter_value else {
            return Err(VmError::TypeMismatch);
        };
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
                GeneratorResumeKind::Next(Value::Undefined),
            )?;
            let Value::Object(obj) = &result else {
                return Err(VmError::TypeMismatch);
            };
            let value =
                crate::object::get(*obj, &self.gc_heap, "value").unwrap_or(Value::Undefined);
            let done = crate::object::get(*obj, &self.gc_heap, "done")
                .unwrap_or(Value::Undefined)
                .to_boolean();
            if done {
                self.gc_heap
                    .with_payload(*iter_rc, |state| *state = IteratorState::Exhausted);
            }
            write_register(&mut stack[top_idx], value_dst, value)?;
            write_register(&mut stack[top_idx], done_dst, Value::Boolean(done))?;
            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            return Ok(true);
        }
        // Helper-wrapper iterator states drive through the
        // interpreter-aware step path so callbacks can run.
        let needs_full_step = self.gc_heap.read_payload(*iter_rc, |state| {
            matches!(
                state,
                IteratorState::Map { .. }
                    | IteratorState::Filter { .. }
                    | IteratorState::Take { .. }
                    | IteratorState::Drop { .. }
                    | IteratorState::FlatMap { .. }
            )
        });
        if needs_full_step {
            let (value, done) = self.iterator_next_full(context, iter_rc)?;
            write_register(&mut stack[top_idx], value_dst, value)?;
            write_register(&mut stack[top_idx], done_dst, Value::Boolean(done))?;
            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            return Ok(true);
        }
        // Snapshot the user iterator object out of the inner
        // state so the borrow does not span the `invoke` call
        // below.
        let user_iter = self.gc_heap.read_payload(*iter_rc, |state| match state {
            IteratorState::User { iterator } => Some(iterator.clone()),
            _ => None,
        });
        let Some(user_iter_value) = user_iter else {
            // Built-in iterator — let the synchronous in-frame
            // path drive it.
            return Ok(false);
        };
        // Already-exhausted user iterators short-circuit per
        // §7.4.2 step 6.
        let Value::Object(iter_obj) = &user_iter_value else {
            return Err(VmError::TypeMismatch);
        };
        let next_fn =
            crate::object::get(*iter_obj, &self.gc_heap, "next").ok_or(VmError::TypeMismatch)?;
        if !is_callable(&next_fn) {
            return Err(VmError::TypeMismatch);
        }
        // Park the state and push a call. `result_reg` reuses the
        // `value_dst` slot — the resume step overwrites it with
        // the unpacked value before the user code observes it.
        stack[top_idx].pending_iterator_next = Some(PendingIteratorNext {
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
