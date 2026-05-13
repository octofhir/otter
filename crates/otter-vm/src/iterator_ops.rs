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

use crate::{
    ExecutionContext, Frame, GeneratorResumeKind, Interpreter, IteratorHandle, IteratorState,
    Value, VmError, alloc_iterator_state, read_register, step_iterator, write_register,
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
        frame: &mut Frame,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let value = read_register(frame, src)?.clone();
        let state = match value {
            Value::Array(array) => IteratorState::Array { array, index: 0 },
            Value::String(string) => IteratorState::String { string, index: 0 },
            // `for…of` over a `Map` yields `[key, value]` pairs (Spec
            // §24.1.3.12 — `@@iterator` aliases `entries`); over a `Set`
            // yields values (Spec §24.2.3.11). Snapshot at iteration start by
            // building a synthetic backing array.
            Value::Map(m) => {
                let entries = crate::collections::map_entries(m, &self.gc_heap);
                let mut snap: SmallVec<[Value; 4]> = SmallVec::with_capacity(entries.len());
                for (k, v) in entries {
                    let mut pair: SmallVec<[Value; 4]> = SmallVec::new();
                    pair.push(k);
                    pair.push(v);
                    let pair_array = crate::array::from_elements(&mut self.gc_heap, pair)?;
                    snap.push(Value::Array(pair_array));
                }
                IteratorState::Array {
                    array: crate::array::from_elements(&mut self.gc_heap, snap)?,
                    index: 0,
                }
            }
            Value::Set(s) => {
                let snap: SmallVec<[Value; 4]> = crate::collections::set_values(s, &self.gc_heap)
                    .into_iter()
                    .collect();
                IteratorState::Array {
                    array: crate::array::from_elements(&mut self.gc_heap, snap)?,
                    index: 0,
                }
            }
            // §27.5 — generator objects are iterable; `[@@iterator]()` returns
            // the generator itself, and `next()` drives the suspended body.
            Value::Generator(handle) => IteratorState::Generator { handle },
            // Already-an-iterator should pass through unchanged.
            Value::Iterator(rc) => {
                write_register(frame, dst, Value::Iterator(rc))?;
                frame.pc += 1;
                return Ok(());
            }
            _ => return Err(VmError::TypeMismatch),
        };
        let iter = alloc_iterator_state(&mut self.gc_heap, state)?;
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
                    other => return Ok((other, false)),
                };
                let new_inner = alloc_iterator_state(&mut self.gc_heap, inner_state)?;
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::FlatMap { inner: slot, .. } = state {
                        *slot = Some(new_inner);
                    }
                });
                inner = Some(new_inner);
            },
        }
    }
}
