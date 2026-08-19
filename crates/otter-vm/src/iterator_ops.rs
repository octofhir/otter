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
//! - Iterator records are reloaded from traced anchors after every property
//!   lookup or callback that may move the young generation. The `chunks` /
//!   `windows` element buffer follows the same rule: it is read back from the
//!   state slot at every use and never carried across a step.
//! - A freshly allocated child stored into an existing iterator state (the
//!   `flatMap` inner iterator, the `chunks` replacement buffer) is followed by
//!   an explicit write barrier — the parent state may already be tenured.
//! - IteratorClose holds its iterator and discovered `return` method in
//!   canonical handles, reloading the receiver after accessor/call re-entry.
//!
//! # See also
//! - [`crate::executable`]
//! - [`crate::IteratorState`]

use crate::activation_stack::ActivationStack;
use smallvec::SmallVec;

use crate::{
    ExecutionContext, Frame, GeneratorResumeKind, Interpreter, IteratorHandle, IteratorState,
    JsPromise, JsString, PendingGetIterator, PendingIteratorNext, Value, VmError, VmGetOutcome,
    VmPropertyKey, array, generator::AsyncGeneratorState, is_callable,
    operand_decode::register_operand, promise::PromiseCapability, read_register, step_iterator,
    symbol, write_register,
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

/// Which of a `Zip` helper's traced lists an append targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ZipList {
    /// Per-input iterator records.
    Inputs,
    /// `"longest"` padding values.
    Padding,
    /// `zipKeyed` result keys.
    Keys,
    /// Scratch values for the round being assembled.
    Results,
}

/// Cloned snapshot of an [`IteratorState`] taken before driving a
/// helper callback so the GC body borrow does not span dispatch.
enum IteratorStateSnapshot {
    User(Value, Option<Value>),
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
        running: bool,
        counter: u64,
    },
    Filter {
        source: IteratorHandle,
        predicate: Value,
        running: bool,
        counter: u64,
    },
    Take {
        source: IteratorHandle,
        remaining: u64,
        running: bool,
    },
    Drop {
        source: IteratorHandle,
        to_drop: u64,
        running: bool,
    },
    FlatMap {
        source: IteratorHandle,
        mapper: Value,
        running: bool,
        inner: Option<IteratorHandle>,
        counter: u64,
    },
    // The retained element buffer is deliberately absent: it is a
    // nursery array that any user callback can move, so it is re-read
    // from the (traced) state slot at every use instead of snapshotted.
    Chunks {
        source: IteratorHandle,
        chunk_size: u32,
        running: bool,
    },
    Windows {
        source: IteratorHandle,
        window_size: u32,
        allow_partial: bool,
        running: bool,
    },
    Concat {
        inner: Option<IteratorHandle>,
        index: usize,
        running: bool,
    },
    Zip {
        mode: crate::iterator_state::ZipMode,
        keyed: bool,
        running: bool,
    },
}

impl Interpreter {
    pub(crate) fn run_get_iterator_regs(
        &mut self,
        stack: &mut ActivationStack,
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
            frame.advance_pc()?;
            return Ok(());
        } else {
            return Err(VmError::TypeMismatch);
        };
        let iter = self.alloc_stack_rooted_iterator_state(stack, state, &[&value], &[])?;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, Value::iterator(iter))?;
        frame.advance_pc()?;
        Ok(())
    }

    pub(crate) fn run_get_async_iterator_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let value = *read_register(&stack[top_idx], src)?;
        if let Some(handle) = value.as_generator()
            && handle.is_async(&self.gc_heap)
        {
            write_register(&mut stack[top_idx], dst, value)?;
            stack[top_idx].advance_pc()?;
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
                stack,
                context,
                value,
                value,
                &VmPropertyKey::Symbol(async_iter_sym),
                0,
            )? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync_rooted(stack, context, &getter, value, SmallVec::new())?
                }
            };
            if !method.is_nullish() {
                if !is_callable(&method) {
                    return Err(VmError::TypeMismatch);
                }
                let produced =
                    self.run_callable_sync_rooted(stack, context, &method, value, SmallVec::new())?;
                if produced.as_object().is_none()
                    && produced.as_generator().is_none()
                    && produced.as_iterator().is_none()
                    && produced.as_array().is_none()
                    && !produced.is_proxy()
                {
                    return Err(VmError::TypeMismatch);
                }
                write_register(&mut stack[top_idx], dst, produced)?;
                stack[top_idx].advance_pc()?;
                return Ok(());
            }
        }

        self.run_get_iterator_regs(stack, top_idx, dst, src)
    }

    /// §7.4.2 GetIteratorDirect — wrap the object returned by a user
    /// `[@@iterator]()` into an iterator `Value`, reading `next` exactly
    /// once and caching it in the record. Shared by the interpreter's
    /// frame-push resume path and the synchronous [`Self::get_iterator_full`]
    /// reentrant transition, so both tiers observe identical accessor and
    /// prototype effects.
    pub(crate) fn wrap_iterator_method_result(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        produced: Value,
    ) -> Result<Value, VmError> {
        // §7.4.3 step 2 — `[@@iterator]()` must return an Object.
        if let Some(iter) = produced.as_iterator() {
            return Ok(Value::iterator(iter));
        }
        let iter_state = if let Some(handle) = produced.as_generator() {
            IteratorState::Generator { handle }
        } else if produced.is_object()
            || produced.is_proxy()
            || produced.is_array()
            || produced.is_map()
            || produced.is_set()
        {
            // `next` is read ONCE here; later `IteratorNext` ticks must not
            // re-read it (observable via an accessor-defined `next`).
            let next_method = match self.ordinary_get_value(
                stack,
                context,
                produced,
                produced,
                &VmPropertyKey::String("next"),
                0,
            )? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => self.run_callable_sync_rooted(
                    stack,
                    context,
                    &getter,
                    produced,
                    SmallVec::new(),
                )?,
            };
            IteratorState::User {
                iterator: produced,
                next_method: Some(next_method),
            }
        } else {
            return Err(VmError::TypeMismatch);
        };
        let iter = self.alloc_stack_rooted_iterator_state(stack, iter_state, &[&produced], &[])?;
        Ok(Value::iterator(iter))
    }

    /// Complete one `Op::GetIterator` synchronously for a compiled frame.
    ///
    /// This is the reentrant sibling of the interpreter's frame-push
    /// [`Self::drive_get_iterator`]: user `[Symbol.iterator]()` methods run
    /// through [`Self::run_callable_sync`] instead of suspending the opcode on
    /// a parked continuation, so the JIT never resumes a partially observed
    /// GetIterator. Built-in iterables fall through to the shared
    /// [`Self::run_get_iterator_regs`] fast path. Every observable accessor,
    /// `@@iterator` call, and GetIteratorDirect `next` read is committed before
    /// the destination register is written; there is no post-effect side exit.
    pub(crate) fn get_iterator_full(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let value = *read_register(&stack[top_idx], src)?;
        let iter_sym = self.well_known_symbols.get(symbol::WellKnown::Iterator);

        // Arrays with an own or prototype `@@iterator` run the user method;
        // the plain built-in Array iterator falls through to the fast path.
        if let Some(arr) = value.as_array() {
            let own_method = array::get_symbol_property(arr, &self.gc_heap, iter_sym);
            let proto = self.constructor_prototype_value("Array")?;
            let proto_has = if own_method.is_none() {
                self.ordinary_has_property_value(
                    stack,
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
                        stack,
                        context,
                        proto,
                        value,
                        &VmPropertyKey::Symbol(iter_sym),
                        0,
                    )? {
                        VmGetOutcome::Value(v) => v,
                        VmGetOutcome::InvokeGetter { getter } => self.run_callable_sync_rooted(
                            stack,
                            context,
                            &getter,
                            value,
                            SmallVec::new(),
                        )?,
                    }
                };
                if callee.is_undefined() || callee.is_null() || !is_callable(&callee) {
                    return Err(VmError::TypeMismatch);
                }
                let receiver = *read_register(&stack[top_idx], src)?;
                let produced = self.run_callable_sync_rooted(
                    stack,
                    context,
                    &callee,
                    receiver,
                    SmallVec::new(),
                )?;
                let wrapped = self.wrap_iterator_method_result(context, stack, produced)?;
                write_register(&mut stack[top_idx], dst, wrapped)?;
                return Ok(());
            }
            return Err(VmError::TypeMismatch);
        }

        // §23.2.3.32 %TypedArray%.prototype[@@iterator] returns a live array
        // iterator; a TypedArray is not an ordinary object, so always route it
        // through its prototype's `@@iterator`.
        if value.as_typed_array(&self.gc_heap).is_some() {
            let callee = match self.ordinary_get_value(
                stack,
                context,
                value,
                value,
                &VmPropertyKey::Symbol(iter_sym),
                0,
            )? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync_rooted(stack, context, &getter, value, SmallVec::new())?
                }
            };
            if !is_callable(&callee) {
                return Err(VmError::TypeMismatch);
            }
            let receiver = *read_register(&stack[top_idx], src)?;
            let produced =
                self.run_callable_sync_rooted(stack, context, &callee, receiver, SmallVec::new())?;
            let wrapped = self.wrap_iterator_method_result(context, stack, produced)?;
            write_register(&mut stack[top_idx], dst, wrapped)?;
            return Ok(());
        }

        // Non-object, non-proxy sources are the built-in fast path (arrays and
        // strings handled there); everything callable-iterable goes through
        // the ordinary `[[Get]]` ladder so an accessor `@@iterator` fires.
        if value.as_object().is_none() && value.as_proxy().is_none() {
            return self.run_get_iterator_regs(stack, top_idx, dst, src);
        }

        let callee = match self.ordinary_get_value(
            stack,
            context,
            value,
            value,
            &VmPropertyKey::Symbol(iter_sym),
            0,
        )? {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync_rooted(stack, context, &getter, value, SmallVec::new())?
            }
        };
        if callee.is_undefined() || callee.is_null() || !is_callable(&callee) {
            // No `[Symbol.iterator]` — §7.4.3 step 2 throws.
            return Err(VmError::TypeMismatch);
        }
        let receiver = *read_register(&stack[top_idx], src)?;
        let produced =
            self.run_callable_sync_rooted(stack, context, &callee, receiver, SmallVec::new())?;
        let wrapped = self.wrap_iterator_method_result(context, stack, produced)?;
        write_register(&mut stack[top_idx], dst, wrapped)?;
        Ok(())
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
        if done {
            // §7.4.9 — an exhausted iterator is `[[Done]]`; drop it from
            // the closer registry so a later throw-unwind treats
            // IteratorClose as the spec no-op rather than re-running
            // `[[return]]`.
            let iterator = *read_register(frame, iter_reg)?;
            self.deregister_frame_iterator_closer(frame, iterator);
        }
        frame.advance_pc()?;
        Ok(())
    }

    /// Read a property off an iterator result record through the
    /// ordinary `[[Get]]`, so an accessor getter runs and propagates
    /// its abrupt completion. Spec: IteratorComplete / IteratorValue.
    fn iter_result_get(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        record: Value,
        name: &'static str,
    ) -> Result<Value, VmError> {
        let key = crate::VmPropertyKey::String(name);
        match self.ordinary_get_value(stack, context, record, record, &key, 0)? {
            crate::VmGetOutcome::Value(v) => Ok(v),
            crate::VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync_rooted(stack, context, &getter, record, SmallVec::new())
            }
        }
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
        stack: &mut ActivationStack,
        iter: &IteratorHandle,
    ) -> Result<(Value, bool), VmError> {
        match step_iterator(*iter, &mut self.gc_heap) {
            Ok((value, done)) => Ok((value, done)),
            Err(_) => self.iterator_next_full_slow(context, stack, iter),
        }
    }

    fn iterator_next_full_slow(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        iter: &IteratorHandle,
    ) -> Result<(Value, bool), VmError> {
        let snapshot: Option<IteratorStateSnapshot> =
            self.gc_heap.read_payload(*iter, |state| match state {
                IteratorState::User {
                    iterator,
                    next_method,
                } => Some(IteratorStateSnapshot::User(*iterator, *next_method)),
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
                IteratorState::Map {
                    source,
                    mapper,
                    running,
                    counter,
                } => Some(IteratorStateSnapshot::Map {
                    source: *source,
                    mapper: *mapper,
                    running: *running,
                    counter: *counter,
                }),
                IteratorState::Filter {
                    source,
                    predicate,
                    running,
                    counter,
                } => Some(IteratorStateSnapshot::Filter {
                    source: *source,
                    predicate: *predicate,
                    running: *running,
                    counter: *counter,
                }),
                IteratorState::Take {
                    source,
                    remaining,
                    running,
                } => Some(IteratorStateSnapshot::Take {
                    source: *source,
                    remaining: *remaining,
                    running: *running,
                }),
                IteratorState::Drop {
                    source,
                    to_drop,
                    running,
                } => Some(IteratorStateSnapshot::Drop {
                    source: *source,
                    to_drop: *to_drop,
                    running: *running,
                }),
                IteratorState::FlatMap {
                    source,
                    mapper,
                    running,
                    inner,
                    counter,
                } => Some(IteratorStateSnapshot::FlatMap {
                    source: *source,
                    mapper: *mapper,
                    running: *running,
                    inner: *inner,
                    counter: *counter,
                }),
                IteratorState::Chunks {
                    source,
                    chunk_size,
                    running,
                    ..
                } => Some(IteratorStateSnapshot::Chunks {
                    source: *source,
                    chunk_size: *chunk_size,
                    running: *running,
                }),
                IteratorState::Windows {
                    source,
                    window_size,
                    allow_partial,
                    running,
                    ..
                } => Some(IteratorStateSnapshot::Windows {
                    source: *source,
                    window_size: *window_size,
                    allow_partial: *allow_partial,
                    running: *running,
                }),
                IteratorState::Concat {
                    inner,
                    index,
                    running,
                    ..
                } => Some(IteratorStateSnapshot::Concat {
                    inner: *inner,
                    index: *index,
                    running: *running,
                }),
                IteratorState::Zip {
                    mode,
                    keyed,
                    running,
                    ..
                } => Some(IteratorStateSnapshot::Zip {
                    mode: *mode,
                    keyed: *keyed,
                    running: *running,
                }),
                _ => None,
            });
        let snapshot = snapshot.ok_or(VmError::TypeMismatch)?;
        match snapshot {
            IteratorStateSnapshot::Generator(handle) => {
                let result = self.resume_generator(
                    stack,
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
                    self.gc_heap.with_payload(*iter, |state| state.exhaust());
                }
                Ok((value, done))
            }
            IteratorStateSnapshot::User(iter_value, next_method) => {
                // §7.4.4 GetIteratorDirect — helpers cache `next` once
                // at iterator-record creation. Paths that defer the
                // lookup (for-of `GetIterator`) read it through the
                // ordinary `[[Get]]` so accessor `next` fires.
                let next_fn = match next_method {
                    Some(next_fn) => next_fn,
                    None => {
                        let key = crate::VmPropertyKey::String("next");
                        match self
                            .ordinary_get_value(stack, context, iter_value, iter_value, &key, 0)?
                        {
                            crate::VmGetOutcome::Value(v) => v,
                            crate::VmGetOutcome::InvokeGetter { getter } => self
                                .run_callable_sync_rooted(
                                    stack,
                                    context,
                                    &getter,
                                    iter_value,
                                    SmallVec::new(),
                                )?,
                        }
                    }
                };
                if !self.is_callable_runtime(&next_fn) {
                    return Err(VmError::TypeMismatch);
                }
                let result = self.run_callable_sync_rooted(
                    stack,
                    context,
                    &next_fn,
                    iter_value,
                    SmallVec::new(),
                )?;
                if !crate::reflect::is_type_object_value(&result) {
                    return Err(
                        self.err_type(("iterator result is not an object".to_string()).into())
                    );
                }
                // §7.4.5 IteratorComplete / §7.4.6 IteratorValue read
                // `done` then (when not done) `value` through the
                // ordinary `[[Get]]`, so an accessor result object fires
                // its getters and an abrupt completion propagates rather
                // than silently reading `undefined` (which would never
                // terminate a `done`-less iterator).
                let done = self
                    .iter_result_get(stack, context, result, "done")?
                    .to_boolean(&self.gc_heap);
                if done {
                    self.gc_heap.with_payload(*iter, |state| state.exhaust());
                    return Ok((Value::undefined(), true));
                }
                let value = self.iter_result_get(stack, context, result, "value")?;
                Ok((value, false))
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
                    stack,
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
            IteratorStateSnapshot::Map {
                source,
                mapper,
                running,
                counter,
            } => {
                if running {
                    return Err(
                        self.err_type(("Iterator helper is already running".to_string()).into())
                    );
                }
                let (v, done) = self.iterator_next_full(context, stack, &source)?;
                if done {
                    self.gc_heap.with_payload(*iter, |state| state.exhaust());
                    return Ok((Value::undefined(), true));
                }
                let counter_value =
                    Value::number(crate::number::NumberValue::from_f64(counter as f64));
                // §27.1.4.7 step 5.b.v — IfAbruptCloseIterator: a throw
                // from the mapper closes the underlying iterator before
                // propagating.
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::Map { running, .. } = state {
                        *running = true;
                    }
                });
                let mapped = match self.run_callable_sync_rooted(
                    stack,
                    context,
                    &mapper,
                    Value::undefined(),
                    smallvec::smallvec![v, counter_value],
                ) {
                    Ok(mapped) => {
                        self.gc_heap.with_payload(*iter, |state| {
                            if let IteratorState::Map { running, .. } = state {
                                *running = false;
                            }
                        });
                        mapped
                    }
                    Err(err) => {
                        self.gc_heap.with_payload(*iter, |state| state.exhaust());
                        self.close_iterator_preserving_throw(stack, context, &source);
                        return Err(err);
                    }
                };
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::Map { counter, .. } = state {
                        *counter += 1;
                    }
                });
                Ok((mapped, false))
            }
            IteratorStateSnapshot::Filter {
                source,
                predicate,
                running,
                counter,
            } => {
                if running {
                    return Err(
                        self.err_type(("Iterator helper is already running".to_string()).into())
                    );
                }
                let mut counter = counter;
                loop {
                    let (v, done) = self.iterator_next_full(context, stack, &source)?;
                    if done {
                        self.gc_heap.with_payload(*iter, |state| state.exhaust());
                        return Ok((Value::undefined(), true));
                    }
                    let counter_value =
                        Value::number(crate::number::NumberValue::from_f64(counter as f64));
                    // §27.1.4.6 step 5.b.v — IfAbruptCloseIterator on a
                    // throwing predicate.
                    self.gc_heap.with_payload(*iter, |state| {
                        if let IteratorState::Filter { running, .. } = state {
                            *running = true;
                        }
                    });
                    let kept = match self.run_callable_sync_rooted(
                        stack,
                        context,
                        &predicate,
                        Value::undefined(),
                        smallvec::smallvec![v, counter_value],
                    ) {
                        Ok(kept) => {
                            self.gc_heap.with_payload(*iter, |state| {
                                if let IteratorState::Filter { running, .. } = state {
                                    *running = false;
                                }
                            });
                            kept
                        }
                        Err(err) => {
                            self.gc_heap.with_payload(*iter, |state| state.exhaust());
                            self.close_iterator_preserving_throw(stack, context, &source);
                            return Err(err);
                        }
                    };
                    counter += 1;
                    self.gc_heap.with_payload(*iter, |state| {
                        if let IteratorState::Filter { counter: slot, .. } = state {
                            *slot = counter;
                        }
                    });
                    if kept.to_boolean(&self.gc_heap) {
                        return Ok((v, false));
                    }
                }
            }
            IteratorStateSnapshot::Take {
                source,
                remaining,
                running,
            } => {
                // §27.5.3.2 GeneratorValidate step 6 — the helper is
                // "executing" for the whole step, including while the
                // underlying iterator's `next` runs; re-entry throws.
                if running {
                    return Err(
                        self.err_type(("Iterator helper is already running".to_string()).into())
                    );
                }
                if remaining == 0 {
                    self.gc_heap.with_payload(*iter, |state| state.exhaust());
                    // §27.1.4.9 step 5.b.ii — the limit being reached
                    // closes the underlying iterator with a normal
                    // completion.
                    self.iterator_close_value_sync(stack, context, Value::iterator(source))?;
                    return Ok((Value::undefined(), true));
                }
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::Take { running, .. } = state {
                        *running = true;
                    }
                });
                let step = self.iterator_next_full(context, stack, &source);
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::Take { running, .. } = state {
                        *running = false;
                    }
                });
                let (v, done) = match step {
                    Ok(step) => step,
                    Err(err) => {
                        // Abrupt completion completes the helper
                        // generator (§27.5.3.3 GeneratorResume).
                        self.gc_heap.with_payload(*iter, |state| state.exhaust());
                        return Err(err);
                    }
                };
                if done {
                    self.gc_heap.with_payload(*iter, |state| state.exhaust());
                    return Ok((Value::undefined(), true));
                }
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::Take { remaining, .. } = state {
                        *remaining = remaining.saturating_sub(1);
                    }
                });
                Ok((v, false))
            }
            IteratorStateSnapshot::Drop {
                source,
                to_drop,
                running,
            } => {
                // §27.5.3.2 GeneratorValidate step 6 — see Take above.
                if running {
                    return Err(
                        self.err_type(("Iterator helper is already running".to_string()).into())
                    );
                }
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::Drop { running, .. } = state {
                        *running = true;
                    }
                });
                let step = (|| {
                    for _ in 0..to_drop {
                        let (_, done) = self.iterator_next_full(context, stack, &source)?;
                        if done {
                            return Ok(None);
                        }
                    }
                    self.gc_heap.with_payload(*iter, |state| {
                        if let IteratorState::Drop { to_drop, .. } = state {
                            *to_drop = 0;
                        }
                    });
                    let (v, done) = self.iterator_next_full(context, stack, &source)?;
                    Ok(if done { None } else { Some(v) })
                })();
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::Drop { running, .. } = state {
                        *running = false;
                    }
                });
                match step {
                    Ok(Some(v)) => Ok((v, false)),
                    Ok(None) => {
                        self.gc_heap.with_payload(*iter, |state| state.exhaust());
                        Ok((Value::undefined(), true))
                    }
                    Err(err) => {
                        // Abrupt completion completes the helper
                        // generator (§27.5.3.3 GeneratorResume).
                        self.gc_heap.with_payload(*iter, |state| state.exhaust());
                        Err(err)
                    }
                }
            }
            IteratorStateSnapshot::FlatMap {
                source,
                mapper,
                running,
                mut inner,
                mut counter,
            } => loop {
                if running {
                    return Err(
                        self.err_type(("Iterator helper is already running".to_string()).into())
                    );
                }
                if let Some(inner_iter) = inner.take() {
                    let (v, done) = match self.iterator_next_full(context, stack, &inner_iter) {
                        Ok(next) => next,
                        Err(err) => {
                            self.gc_heap.with_payload(*iter, |state| state.exhaust());
                            self.close_iterator_preserving_throw(stack, context, &source);
                            return Err(err);
                        }
                    };
                    if !done {
                        return Ok((v, false));
                    }
                    self.gc_heap.with_payload(*iter, |state| {
                        if let IteratorState::FlatMap { inner: slot, .. } = state {
                            *slot = None;
                        }
                    });
                }
                let (v, done) = self.iterator_next_full(context, stack, &source)?;
                if done {
                    self.gc_heap.with_payload(*iter, |state| state.exhaust());
                    return Ok((Value::undefined(), true));
                }
                let counter_value =
                    Value::number(crate::number::NumberValue::from_f64(counter as f64));
                // §27.1.4.5 step 5.b.iv — IfAbruptCloseIterator on a
                // throwing mapper.
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::FlatMap { running, .. } = state {
                        *running = true;
                    }
                });
                let mapped = match self.run_callable_sync_rooted(
                    stack,
                    context,
                    &mapper,
                    Value::undefined(),
                    smallvec::smallvec![v, counter_value],
                ) {
                    Ok(mapped) => {
                        self.gc_heap.with_payload(*iter, |state| {
                            if let IteratorState::FlatMap { running, .. } = state {
                                *running = false;
                            }
                        });
                        mapped
                    }
                    Err(err) => {
                        self.gc_heap.with_payload(*iter, |state| state.exhaust());
                        self.close_iterator_preserving_throw(stack, context, &source);
                        return Err(err);
                    }
                };
                counter += 1;
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::FlatMap { counter: slot, .. } = state {
                        *slot = counter;
                    }
                });
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
                    let outcome =
                        match self.ordinary_get_value(stack, context, mapped, mapped, &key, 0) {
                            Ok(outcome) => outcome,
                            Err(err) => {
                                self.gc_heap.with_payload(*iter, |state| state.exhaust());
                                self.close_iterator_preserving_throw(stack, context, &source);
                                return Err(err);
                            }
                        };
                    let iter_method = match outcome {
                        crate::VmGetOutcome::Value(v) => v,
                        crate::VmGetOutcome::InvokeGetter { getter } => match self
                            .run_callable_sync_rooted(
                                stack,
                                context,
                                &getter,
                                mapped,
                                SmallVec::new(),
                            ) {
                            Ok(value) => value,
                            Err(err) => {
                                self.gc_heap.with_payload(*iter, |state| state.exhaust());
                                self.close_iterator_preserving_throw(stack, context, &source);
                                return Err(err);
                            }
                        },
                    };
                    let iter_value = if iter_method.is_undefined() || iter_method.is_null() {
                        // Iterator-without-`@@iterator` shape —
                        // wrap the mapped object directly so
                        // subsequent `IteratorNext` calls invoke
                        // its own `.next`.
                        mapped
                    } else if self.is_callable_runtime(&iter_method) {
                        match self.run_callable_sync_rooted(
                            stack,
                            context,
                            &iter_method,
                            mapped,
                            SmallVec::new(),
                        ) {
                            Ok(value) => value,
                            Err(err) => {
                                self.gc_heap.with_payload(*iter, |state| state.exhaust());
                                self.close_iterator_preserving_throw(stack, context, &source);
                                return Err(err);
                            }
                        }
                    } else {
                        self.gc_heap.with_payload(*iter, |state| state.exhaust());
                        self.close_iterator_preserving_throw(stack, context, &source);
                        return Err(self.err_type(
                            ("Iterator.prototype.flatMap mapper return must be iterable"
                                .to_string())
                            .into(),
                        ));
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
                        // §7.4.2 GetIteratorFlattenable step 3 —
                        // GetIteratorDirect caches `next` once.
                        let key = crate::VmPropertyKey::String("next");
                        let next_method = match match self
                            .ordinary_get_value(stack, context, iter_value, iter_value, &key, 0)
                        {
                            Ok(outcome) => outcome,
                            Err(err) => {
                                self.gc_heap.with_payload(*iter, |state| state.exhaust());
                                self.close_iterator_preserving_throw(stack, context, &source);
                                return Err(err);
                            }
                        } {
                            crate::VmGetOutcome::Value(v) => v,
                            crate::VmGetOutcome::InvokeGetter { getter } => match self
                                .run_callable_sync_rooted(
                                    stack,
                                    context,
                                    &getter,
                                    iter_value,
                                    SmallVec::new(),
                                ) {
                                Ok(value) => value,
                                Err(err) => {
                                    self.gc_heap.with_payload(*iter, |state| state.exhaust());
                                    self.close_iterator_preserving_throw(stack, context, &source);
                                    return Err(err);
                                }
                            },
                        };
                        if !self.is_callable_runtime(&next_method) {
                            self.gc_heap.with_payload(*iter, |state| state.exhaust());
                            self.close_iterator_preserving_throw(stack, context, &source);
                            return Err(self.err_type(
                                ("Iterator.prototype.flatMap mapper return must be iterable"
                                    .to_string())
                                .into(),
                            ));
                        }
                        IteratorState::User {
                            iterator: iter_value,
                            next_method: Some(next_method),
                        }
                    }
                } else {
                    self.gc_heap.with_payload(*iter, |state| state.exhaust());
                    self.close_iterator_preserving_throw(stack, context, &source);
                    return Err(self.err_type(
                        ("Iterator.prototype.flatMap mapper return must be iterable".to_string())
                            .into(),
                    ));
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
                // A freshly allocated inner iterator is young; the helper
                // holding it may already be old.
                self.gc_heap.write_barrier(*iter, new_inner);
                inner = Some(new_inner);
            },
            IteratorStateSnapshot::Chunks {
                source,
                chunk_size,
                running,
            } => {
                // §27.5.3.2 GeneratorValidate step 6 — see Take above.
                if running {
                    return Err(
                        self.err_type(("Iterator helper is already running".to_string()).into())
                    );
                }
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::Chunks { running, .. } = state {
                        *running = true;
                    }
                });
                // Pull until the buffer holds a full chunk, or the
                // source runs dry.
                let step = (|| {
                    loop {
                        let (v, done) = self.iterator_next_full(context, stack, &source)?;
                        if done {
                            return Ok(false);
                        }
                        self.append_to_iterator_buffer(*iter, source, v)?;
                        let Some(buffer) = self.iterator_helper_buffer(*iter) else {
                            // Re-entrant close folded the helper away.
                            return Ok(false);
                        };
                        if array::len(buffer, &self.gc_heap) as u64 >= u64::from(chunk_size) {
                            return Ok(true);
                        }
                    }
                })();
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::Chunks { running, .. } = state {
                        *running = false;
                    }
                });
                let full = match step {
                    Ok(full) => full,
                    Err(err) => {
                        // Abrupt completion completes the helper
                        // generator (§27.5.3.3 GeneratorResume).
                        self.gc_heap.with_payload(*iter, |state| state.exhaust());
                        return Err(err);
                    }
                };
                let buffer = self.iterator_helper_buffer(*iter);
                if !full {
                    // Source exhausted: a non-empty buffer is emitted as
                    // the trailing partial chunk, and the helper reports
                    // `done` on the following step.
                    let pending = buffer.filter(|b| array::len(*b, &self.gc_heap) > 0);
                    self.gc_heap.with_payload(*iter, |state| state.exhaust());
                    return Ok(match pending {
                        Some(chunk) => (Value::array(chunk), false),
                        None => (Value::undefined(), true),
                    });
                }
                let Some(buffer) = buffer else {
                    self.gc_heap.with_payload(*iter, |state| state.exhaust());
                    return Ok((Value::undefined(), true));
                };
                // Hand the buffer out as-is and start collecting into a
                // fresh array, so every yielded chunk is distinct.
                let yielded = Value::array(buffer);
                let fresh = self.alloc_runtime_rooted_array_from_values(
                    std::iter::empty(),
                    &[&yielded],
                    &[],
                )?;
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::Chunks { buffer: slot, .. } = state {
                        *slot = fresh;
                    }
                });
                // The helper state may already have been promoted, so the
                // fresh nursery buffer is an old→young edge the scavenger
                // has to know about.
                self.gc_heap.write_barrier(*iter, fresh);
                Ok((yielded, false))
            }
            IteratorStateSnapshot::Windows {
                source,
                window_size,
                allow_partial,
                running,
            } => {
                // §27.5.3.2 GeneratorValidate step 6 — see Take above.
                if running {
                    return Err(
                        self.err_type(("Iterator helper is already running".to_string()).into())
                    );
                }
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::Windows { running, .. } = state {
                        *running = true;
                    }
                });
                let step = (|| {
                    loop {
                        let (v, done) = self.iterator_next_full(context, stack, &source)?;
                        if done {
                            return Ok(false);
                        }
                        let Some(buffer) = self.iterator_helper_buffer(*iter) else {
                            return Ok(false);
                        };
                        // The window slides: once full, the oldest
                        // element leaves before the new one is appended.
                        if array::len(buffer, &self.gc_heap) as u64 >= u64::from(window_size) {
                            let _evicted = array::dense_shift(buffer, &mut self.gc_heap);
                        }
                        self.append_to_iterator_buffer(*iter, source, v)?;
                        let Some(buffer) = self.iterator_helper_buffer(*iter) else {
                            return Ok(false);
                        };
                        if array::len(buffer, &self.gc_heap) as u64 >= u64::from(window_size) {
                            return Ok(true);
                        }
                    }
                })();
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::Windows { running, .. } = state {
                        *running = false;
                    }
                });
                let full = match step {
                    Ok(full) => full,
                    Err(err) => {
                        self.gc_heap.with_payload(*iter, |state| state.exhaust());
                        return Err(err);
                    }
                };
                let buffer = self.iterator_helper_buffer(*iter);
                if !full {
                    // The window never filled. `"allow-partial"` emits
                    // what was collected; `"only-full"` emits nothing.
                    // `!full` already implies the buffer is shorter than
                    // `window_size`, so only emptiness is left to check.
                    let short =
                        buffer.filter(|b| allow_partial && array::len(*b, &self.gc_heap) > 0);
                    let partial = match short {
                        Some(buffer) => Some(self.copy_iterator_buffer(buffer)?),
                        None => None,
                    };
                    self.gc_heap.with_payload(*iter, |state| state.exhaust());
                    return Ok(match partial {
                        Some(window) => (window, false),
                        None => (Value::undefined(), true),
                    });
                }
                let Some(buffer) = buffer else {
                    self.gc_heap.with_payload(*iter, |state| state.exhaust());
                    return Ok((Value::undefined(), true));
                };
                // The retained buffer keeps sliding, so each window is
                // handed out as a copy.
                let window = self.copy_iterator_buffer(buffer)?;
                Ok((window, false))
            }
            IteratorStateSnapshot::Concat {
                mut inner,
                mut index,
                running,
            } => {
                // §27.5.3.2 GeneratorValidate step 6 — see Take above.
                if running {
                    return Err(
                        self.err_type(("Iterator helper is already running".to_string()).into())
                    );
                }
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::Concat { running, .. } = state {
                        *running = true;
                    }
                });
                let step = (|| {
                    loop {
                        if let Some(open) = inner {
                            let (v, done) = self.iterator_next_full(context, stack, &open)?;
                            if !done {
                                return Ok(Some(v));
                            }
                            self.gc_heap.with_payload(*iter, |state| {
                                if let IteratorState::Concat { inner: slot, .. } = state {
                                    *slot = None;
                                }
                            });
                            inner = None;
                        }
                        // Open the next captured `(iterable, method)`
                        // pair. The sources array is re-read every time:
                        // driving the previous inner iterator can have
                        // moved it.
                        let Some(sources) = self.iterator_concat_sources(*iter) else {
                            return Ok(None);
                        };
                        let slot = index.saturating_mul(2);
                        if slot >= array::len(sources, &self.gc_heap) {
                            return Ok(None);
                        }
                        let iterable = array::get(sources, &self.gc_heap, slot);
                        let method = array::get(sources, &self.gc_heap, slot + 1);
                        index += 1;
                        self.gc_heap.with_payload(*iter, |state| {
                            if let IteratorState::Concat { index: slot, .. } = state {
                                *slot = index;
                            }
                        });
                        let opened = self.run_callable_sync_rooted(
                            stack,
                            context,
                            &method,
                            iterable,
                            SmallVec::new(),
                        )?;
                        // Built-in iterators are their own value family,
                        // so "is an Object" is the primitive test, not
                        // `as_object`.
                        if crate::abstract_ops::is_primitive(&opened) {
                            return Err(self.err_type(
                                ("Iterator.concat: iterator method did not return an object"
                                    .to_string())
                                .into(),
                            ));
                        }
                        // §7.4.4 GetIteratorDirect — `next` is cached
                        // once per opened iterator.
                        let key = VmPropertyKey::String("next");
                        let next_method = match self
                            .ordinary_get_value(stack, context, opened, opened, &key, 0)?
                        {
                            VmGetOutcome::Value(v) => v,
                            VmGetOutcome::InvokeGetter { getter } => self
                                .run_callable_sync_rooted(
                                    stack,
                                    context,
                                    &getter,
                                    opened,
                                    SmallVec::new(),
                                )?,
                        };
                        let iter_root = Value::iterator(*iter);
                        let new_inner = self.alloc_runtime_rooted_iterator_state(
                            IteratorState::User {
                                iterator: opened,
                                next_method: Some(next_method),
                            },
                            &[&iter_root, &opened, &next_method],
                            &[],
                        )?;
                        self.gc_heap.with_payload(*iter, |state| {
                            if let IteratorState::Concat { inner: slot, .. } = state {
                                *slot = Some(new_inner);
                            }
                        });
                        // Freshly allocated child into a possibly
                        // tenured parent.
                        self.gc_heap.write_barrier(*iter, new_inner);
                        inner = Some(new_inner);
                    }
                })();
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::Concat { running, .. } = state {
                        *running = false;
                    }
                });
                match step {
                    Ok(Some(v)) => Ok((v, false)),
                    Ok(None) => {
                        self.gc_heap.with_payload(*iter, |state| state.exhaust());
                        Ok((Value::undefined(), true))
                    }
                    Err(err) => {
                        // Abrupt completion completes the helper
                        // generator (§27.5.3.3 GeneratorResume).
                        self.gc_heap.with_payload(*iter, |state| state.exhaust());
                        Err(err)
                    }
                }
            }
            IteratorStateSnapshot::Zip {
                mode,
                keyed,
                running,
            } => {
                // §27.5.3.2 GeneratorValidate step 6 — see Take above.
                if running {
                    return Err(
                        self.err_type(("Iterator helper is already running".to_string()).into())
                    );
                }
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::Zip {
                        running, started, ..
                    } = state
                    {
                        *running = true;
                        *started = true;
                    }
                });
                let step = self.iterator_zip_step(stack, context, *iter, mode);
                self.gc_heap.with_payload(*iter, |state| {
                    if let IteratorState::Zip { running, .. } = state {
                        *running = false;
                    }
                });
                match step {
                    Ok(Some(())) => {}
                    Ok(None) => {
                        self.gc_heap.with_payload(*iter, |state| state.exhaust());
                        return Ok((Value::undefined(), true));
                    }
                    Err(err) => {
                        self.gc_heap.with_payload(*iter, |state| state.exhaust());
                        return Err(err);
                    }
                };
                let finished = self.iterator_zip_finish(*iter, keyed)?;
                Ok((finished, false))
            }
        }
    }

    /// Captured `(iterable, method)` pairs of an `Iterator.concat`
    /// sequence. Re-read from the traced state slot at every use for the
    /// same reason as [`Self::iterator_helper_buffer`].
    fn iterator_concat_sources(&self, iter: IteratorHandle) -> Option<array::JsArray> {
        self.gc_heap.read_payload(iter, |state| match state {
            IteratorState::Concat { sources, .. } => Some(*sources),
            _ => None,
        })
    }

    /// The `iters` / `padding` / `keys` arrays of a `Zip` helper,
    /// re-read from the traced state slots.
    fn iterator_zip_arrays(
        &self,
        iter: IteratorHandle,
    ) -> Option<(array::JsArray, array::JsArray, array::JsArray)> {
        self.gc_heap.read_payload(iter, |state| match state {
            IteratorState::Zip {
                iters,
                padding,
                keys,
                ..
            } => Some((*iters, *padding, *keys)),
            _ => None,
        })
    }

    /// The `results` scratch array of a `Zip` helper.
    fn iterator_zip_results(&self, iter: IteratorHandle) -> Option<array::JsArray> {
        self.gc_heap.read_payload(iter, |state| match state {
            IteratorState::Zip { results, .. } => Some(*results),
            _ => None,
        })
    }

    /// Which list a `Zip` helper is being filled into while it is built.
    pub(crate) fn zip_append(
        &mut self,
        iter: IteratorHandle,
        list: ZipList,
        value: Value,
    ) -> Result<(), VmError> {
        let Some(target) = self.zip_list(iter, list) else {
            return Ok(());
        };
        let target_root = Value::array(target);
        let iter_root = Value::iterator(iter);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            target_root.trace_value_slots(visitor);
            iter_root.trace_value_slots(visitor);
            value.trace_value_slots(visitor);
        };
        array::push_with_roots(target, &mut self.gc_heap, value, &mut external_visit)
            .map_err(VmError::from)?;
        Ok(())
    }

    /// Re-point a freshly allocated `Zip` helper at its lists.
    ///
    /// The state body is filled in *after* the allocation that creates
    /// it, so a collection driven by that allocation updates only the
    /// caller's rooted `Value` locals — the handles copied into the body
    /// beforehand can be stale. Rewriting them here from the rooted
    /// values is the fixup, and each store needs a barrier because the
    /// helper may already have been tenured.
    pub(crate) fn zip_relink_lists(&mut self, iter: IteratorHandle, lists: [Value; 4]) {
        let arrays: [array::JsArray; 4] = match [
            lists[0].as_array(),
            lists[1].as_array(),
            lists[2].as_array(),
            lists[3].as_array(),
        ] {
            [Some(iters), Some(padding), Some(keys), Some(results)] => {
                [iters, padding, keys, results]
            }
            _ => return,
        };
        self.gc_heap.with_payload(iter, |state| {
            if let IteratorState::Zip {
                iters,
                padding,
                keys,
                results,
                ..
            } = state
            {
                *iters = arrays[0];
                *padding = arrays[1];
                *keys = arrays[2];
                *results = arrays[3];
            }
        });
        for array in arrays {
            self.gc_heap.write_barrier(iter, array);
        }
    }

    /// Snapshot the still-open inputs of a `Zip` helper.
    ///
    /// Taken before anything that can fold the helper to `Exhausted`
    /// (a close from suspended-start does exactly that). Iterator
    /// records live in old space, so the snapshot cannot go stale.
    pub(crate) fn zip_open_inputs(&self, iter: IteratorHandle) -> Vec<Value> {
        (0..self.zip_list_len(iter, ZipList::Inputs))
            .map(|index| self.zip_list_entry(iter, ZipList::Inputs, index))
            .collect()
    }

    /// Length of one of a `Zip` helper's traced lists.
    pub(crate) fn zip_list_len(&self, iter: IteratorHandle, list: ZipList) -> usize {
        self.zip_list(iter, list)
            .map_or(0, |array| array::len(array, &self.gc_heap))
    }

    /// One entry of a `Zip` helper's traced list.
    pub(crate) fn zip_list_entry(
        &self,
        iter: IteratorHandle,
        list: ZipList,
        index: usize,
    ) -> Value {
        self.zip_list(iter, list)
            .map_or(Value::undefined(), |array| {
                array::get(array, &self.gc_heap, index)
            })
    }

    fn zip_list(&self, iter: IteratorHandle, list: ZipList) -> Option<array::JsArray> {
        self.gc_heap.read_payload(iter, |state| match state {
            IteratorState::Zip {
                iters,
                padding,
                keys,
                results,
                ..
            } => Some(match list {
                ZipList::Inputs => *iters,
                ZipList::Padding => *padding,
                ZipList::Keys => *keys,
                ZipList::Results => *results,
            }),
            _ => None,
        })
    }

    /// Close every input a partially built `Zip` helper has collected.
    pub(crate) fn zip_close_inputs(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        iter: IteratorHandle,
    ) {
        let open = self.zip_open_inputs(iter);
        let saved = self.take_pending_uncaught_throw();
        let _ = self.iterator_zip_close_all(stack, context, open, false);
        let _ = self.take_pending_uncaught_throw();
        if let Some(value) = saved {
            self.set_pending_uncaught_throw(value);
        }
    }

    /// IteratorCloseAll — close every still-open input in reverse list
    /// order.
    ///
    /// §7.4.10 IteratorClose step 5: once the running completion is a
    /// throw it wins over anything a later `return` raises. `throwing`
    /// says the caller already holds such a completion, in which case
    /// every `return` still runs but none of them can win. A losing
    /// throw is also lifted off the interpreter, or its value would
    /// overwrite the winner's on the way out.
    fn iterator_zip_close_all(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        open: Vec<Value>,
        throwing: bool,
    ) -> Result<(), VmError> {
        let incoming = if throwing {
            self.take_pending_uncaught_throw()
        } else {
            None
        };
        let mut outcome = Ok(());
        let mut thrown: Option<Value> = None;
        for entry in open.into_iter().rev() {
            if entry.is_null() || entry.is_undefined() {
                continue;
            }
            let closed = self.iterator_close_value_sync(stack, context, entry);
            if !throwing && outcome.is_ok() {
                if closed.is_err() {
                    thrown = self.take_pending_uncaught_throw();
                }
                outcome = closed;
            } else {
                let _ = self.take_pending_uncaught_throw();
            }
        }
        if let Some(value) = incoming.or(thrown) {
            self.set_pending_uncaught_throw(value);
        }
        outcome
    }

    /// One `IteratorZip` round: step every still-open input once and
    /// collect the per-input results. `Ok(None)` means the join is over.
    fn iterator_zip_step(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        iter: IteratorHandle,
        mode: crate::iterator_state::ZipMode,
    ) -> Result<Option<()>, VmError> {
        use crate::iterator_state::ZipMode;
        let Some((iters, _, _)) = self.iterator_zip_arrays(iter) else {
            return Ok(None);
        };
        let count = array::len(iters, &self.gc_heap);
        if count == 0 {
            return Ok(None);
        }
        // The round's values are staged in the state's traced scratch
        // array; a Rust-side `Vec` would go stale across the `next`
        // calls that fill it.
        if let Some(results) = self.iterator_zip_results(iter) {
            array::set_length(results, &mut self.gc_heap, 0).map_err(VmError::from)?;
        }
        for index in 0..count {
            let Some((iters, padding, _)) = self.iterator_zip_arrays(iter) else {
                return Ok(None);
            };
            let entry = array::get(iters, &self.gc_heap, index);
            if entry.is_null() {
                // Already exhausted; only `"longest"` gets this far.
                let pad = array::get(padding, &self.gc_heap, index);
                self.zip_append(iter, ZipList::Results, pad)?;
                continue;
            }
            let Some(handle) = entry.as_iterator() else {
                return Ok(None);
            };
            let stepped = self.iterator_next_full(context, stack, &handle);
            let Some((_, padding, _)) = self.iterator_zip_arrays(iter) else {
                return Ok(None);
            };
            let (value, done) = match stepped {
                Ok(step) => step,
                Err(err) => {
                    self.zip_forget_input(iter, index);
                    let _ = self.iterator_zip_close_all(
                        stack,
                        context,
                        self.zip_open_inputs(iter),
                        true,
                    );
                    return Err(err);
                }
            };
            if !done {
                self.zip_append(iter, ZipList::Results, value)?;
                continue;
            }
            // This input is finished: drop it from the open set first so
            // the close-all below never re-enters it.
            self.zip_forget_input(iter, index);
            match mode {
                ZipMode::Shortest => {
                    self.iterator_zip_close_all(stack, context, self.zip_open_inputs(iter), false)?;
                    return Ok(None);
                }
                ZipMode::Strict => {
                    if index != 0 {
                        // The completion is the strict-mode TypeError, so
                        // the closes below cannot override it.
                        let error = self.err_type(
                            ("Iterator.zip: inputs of unequal length in strict mode".to_string())
                                .into(),
                        );
                        let _ = self.iterator_zip_close_all(
                            stack,
                            context,
                            self.zip_open_inputs(iter),
                            true,
                        );
                        return Err(error);
                    }
                    // The first input finished, so every other one must
                    // finish on this same step.
                    for other in 1..count {
                        let Some((iters, _, _)) = self.iterator_zip_arrays(iter) else {
                            return Ok(None);
                        };
                        let entry = array::get(iters, &self.gc_heap, other);
                        let Some(handle) = entry.as_iterator() else {
                            continue;
                        };
                        let stepped = self.iterator_next_full(context, stack, &handle);
                        if self.iterator_zip_arrays(iter).is_none() {
                            return Ok(None);
                        }
                        match stepped {
                            Ok((_, true)) => self.zip_forget_input(iter, other),
                            Ok((_, false)) => {
                                let error = self.err_type(
                                    ("Iterator.zip: inputs of unequal length in strict mode"
                                        .to_string())
                                    .into(),
                                );
                                let _ = self.iterator_zip_close_all(
                                    stack,
                                    context,
                                    self.zip_open_inputs(iter),
                                    true,
                                );
                                return Err(error);
                            }
                            Err(err) => {
                                self.zip_forget_input(iter, other);
                                let _ = self.iterator_zip_close_all(
                                    stack,
                                    context,
                                    self.zip_open_inputs(iter),
                                    true,
                                );
                                return Err(err);
                            }
                        }
                    }
                    return Ok(None);
                }
                ZipMode::Longest => {
                    if self.zip_all_inputs_done(iter) {
                        return Ok(None);
                    }
                    let pad = array::get(padding, &self.gc_heap, index);
                    self.zip_append(iter, ZipList::Results, pad)?;
                }
            }
        }
        Ok(Some(()))
    }

    /// Replace a finished input with `null` so it leaves the open set.
    /// Storing `null` writes no pointer, so no barrier is needed.
    fn zip_forget_input(&mut self, iter: IteratorHandle, index: usize) {
        let Some((iters, _, _)) = self.iterator_zip_arrays(iter) else {
            return;
        };
        let _stored = array::set(iters, &mut self.gc_heap, index, Value::null());
    }

    /// Whether every `Zip` input has reported done.
    fn zip_all_inputs_done(&self, iter: IteratorHandle) -> bool {
        let Some((iters, _, _)) = self.iterator_zip_arrays(iter) else {
            return true;
        };
        (0..array::len(iters, &self.gc_heap))
            .all(|index| array::get(iters, &self.gc_heap, index).is_null())
    }

    /// `finishResults` — an Array for `Iterator.zip`, an object keyed by
    /// the captured property keys for `Iterator.zipKeyed`.
    fn iterator_zip_finish(&mut self, iter: IteratorHandle, keyed: bool) -> Result<Value, VmError> {
        let Some(scratch) = self.iterator_zip_results(iter) else {
            return Ok(Value::undefined());
        };
        let results: Vec<Value> = (0..array::len(scratch, &self.gc_heap))
            .map(|index| array::get(scratch, &self.gc_heap, index))
            .collect();
        let results = results.as_slice();
        if !keyed {
            let array = self.alloc_runtime_rooted_array_from_values(
                results.iter().copied(),
                &[],
                &[results],
            )?;
            return Ok(Value::array(array));
        }
        let Some((_, _, keys)) = self.iterator_zip_arrays(iter) else {
            return Ok(Value::undefined());
        };
        let keys: Vec<Value> = (0..array::len(keys, &self.gc_heap))
            .map(|index| array::get(keys, &self.gc_heap, index))
            .collect();
        let object = self.alloc_runtime_rooted_object_with_roots(&[], &[results, &keys])?;
        // Each define can relocate the receiver; the in-place form
        // writes the new location back into `object`.
        let mut object = object;
        for (key, value) in keys.iter().zip(results.iter()) {
            let descriptor = crate::object::PropertyDescriptor::data(*value, true, true, true);
            let heap = &mut self.gc_heap;
            if let Some(text) = key.as_string(heap) {
                let text = text.to_lossy_string(heap);
                crate::object::define_own_property_in_place(&mut object, heap, &text, descriptor);
            } else if let Some(symbol) = key.as_symbol(heap) {
                crate::object::define_own_symbol_property(object, heap, symbol, descriptor);
            }
        }
        Ok(Value::object(object))
    }

    /// Current element buffer of a lazy `chunks` / `windows` helper.
    ///
    /// The buffer is a nursery array: any user callback driven between
    /// two uses can move it, and only the traced state slot is updated.
    /// Every use therefore re-reads it here instead of caching a handle.
    /// `None` means the helper is no longer collecting (a re-entrant
    /// close folded it to `Exhausted`).
    fn iterator_helper_buffer(&self, iter: IteratorHandle) -> Option<array::JsArray> {
        self.gc_heap.read_payload(iter, |state| match state {
            IteratorState::Chunks { buffer, .. } | IteratorState::Windows { buffer, .. } => {
                Some(*buffer)
            }
            _ => None,
        })
    }

    /// Append `value` to a lazy helper's retained buffer, keeping the
    /// buffer, its owning source iterator, and the pending value rooted
    /// across the dense-storage growth.
    fn append_to_iterator_buffer(
        &mut self,
        iter: IteratorHandle,
        source: IteratorHandle,
        value: Value,
    ) -> Result<(), VmError> {
        let Some(buffer) = self.iterator_helper_buffer(iter) else {
            return Ok(());
        };
        let buffer_root = Value::array(buffer);
        let source_root = Value::iterator(source);
        let iter_root = Value::iterator(iter);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            buffer_root.trace_value_slots(visitor);
            source_root.trace_value_slots(visitor);
            iter_root.trace_value_slots(visitor);
            value.trace_value_slots(visitor);
        };
        array::push_with_roots(buffer, &mut self.gc_heap, value, &mut external_visit)
            .map_err(VmError::from)?;
        Ok(())
    }

    /// CreateArrayFromList over a helper's retained buffer — the copy is
    /// what the helper yields, so later slides cannot mutate a window a
    /// consumer already holds.
    fn copy_iterator_buffer(&mut self, buffer: array::JsArray) -> Result<Value, VmError> {
        let len = array::len(buffer, &self.gc_heap);
        let elements: Vec<Value> = (0..len)
            .map(|index| array::get(buffer, &self.gc_heap, index))
            .collect();
        let buffer_root = Value::array(buffer);
        let copy = self.alloc_runtime_rooted_array_from_values(
            elements.iter().copied(),
            &[&buffer_root],
            &[&elements],
        )?;
        Ok(Value::array(copy))
    }

    pub(crate) fn get_iterator_sync(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        iterable: &Value,
    ) -> Result<(Value, Value), VmError> {
        let iterable_anchor = self.push_iteration_anchor(*iterable) - 1;
        let anchor_base = iterable_anchor;
        let result = (|interp: &mut Self| -> Result<(Value, Value), VmError> {
            let iterator_sym = interp.well_known_symbols.get(symbol::WellKnown::Iterator);
            let iterable = interp.iteration_anchor(iterable_anchor);
            let method = match interp.ordinary_get_value(
                stack,
                context,
                iterable,
                iterable,
                &VmPropertyKey::Symbol(iterator_sym),
                0,
            )? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => {
                    let getter_anchor = interp.push_iteration_anchor(getter) - 1;
                    let getter = interp.iteration_anchor(getter_anchor);
                    let iterable = interp.iteration_anchor(iterable_anchor);
                    interp.run_callable_sync_rooted(
                        stack,
                        context,
                        &getter,
                        iterable,
                        SmallVec::new(),
                    )?
                }
            };
            if method.is_undefined() || method.is_null() || !interp.is_callable_runtime(&method) {
                return Err(interp.err_type(("iterator method is not callable".to_string()).into()));
            }
            let method_anchor = interp.push_iteration_anchor(method) - 1;
            let method = interp.iteration_anchor(method_anchor);
            let iterable = interp.iteration_anchor(iterable_anchor);
            let iterator = interp.run_callable_sync_rooted(
                stack,
                context,
                &method,
                iterable,
                SmallVec::new(),
            )?;
            if !(iterator.is_object()
                || iterator.is_proxy()
                || iterator.is_array()
                || iterator.is_iterator()
                || iterator.is_map()
                || iterator.is_set()
                || iterator.is_generator())
            {
                return Err(interp
                    .err_type(("iterator method did not return an object".to_string()).into()));
            }
            let iterator_anchor = interp.push_iteration_anchor(iterator) - 1;
            let iterator = interp.iteration_anchor(iterator_anchor);
            let next_method = match interp.ordinary_get_value(
                stack,
                context,
                iterator,
                iterator,
                &VmPropertyKey::String("next"),
                0,
            )? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => {
                    let getter_anchor = interp.push_iteration_anchor(getter) - 1;
                    let getter = interp.iteration_anchor(getter_anchor);
                    let iterator = interp.iteration_anchor(iterator_anchor);
                    interp.run_callable_sync_rooted(
                        stack,
                        context,
                        &getter,
                        iterator,
                        SmallVec::new(),
                    )?
                }
            };
            Ok((interp.iteration_anchor(iterator_anchor), next_method))
        })(self);
        self.pop_iteration_anchors_to(anchor_base);
        result
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
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        iterator: &Value,
        next_method: &Value,
    ) -> Result<Option<Value>, VmError> {
        let result =
            self.run_callable_sync_rooted(stack, context, next_method, *iterator, SmallVec::new())?;
        if !result.is_object() && !result.is_proxy() {
            return Err(self.err_type(("iterator result is not an object".to_string()).into()));
        }
        // §7.4.6 IteratorStep — anchor the result on the GC root
        // stack across the subsequent `done` / `value` property
        // reads. Without this, a GC triggered inside an accessor
        // getter (or by allocations on the way to the slot lookup)
        // could reclaim the IterResult — its shape/keys would then
        // dangle when the second read walks the same shape chain.
        let anchor_depth = self.push_iteration_anchor(result);
        let outcome = iterator_step_read(self, stack, context, &result);
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
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        iterator: &Value,
    ) -> Result<(), VmError> {
        self.with_handle_scope(|interp, scope| {
            let iterator = interp.scoped_value(scope, *iterator);
            let current = interp.escape_scoped(iterator);
            let return_method = match interp.ordinary_get_value(
                stack,
                context,
                current,
                current,
                &VmPropertyKey::String("return"),
                0,
            )? {
                VmGetOutcome::Value(value) => interp.scoped_value(scope, value),
                VmGetOutcome::InvokeGetter { getter } => {
                    let getter = interp.scoped_value(scope, getter);
                    let value = interp.run_callable_sync_rooted(
                        stack,
                        context,
                        &interp.escape_scoped(getter),
                        interp.escape_scoped(iterator),
                        SmallVec::new(),
                    )?;
                    interp.scoped_value(scope, value)
                }
            };
            let return_value = interp.escape_scoped(return_method);
            if return_value.is_undefined() || return_value.is_null() {
                return Ok(());
            }
            if !interp.is_callable_runtime(&return_value) {
                return Err(
                    interp.err_type(("iterator `return` is not callable".to_string()).into())
                );
            }
            let result = interp.run_callable_sync_rooted(
                stack,
                context,
                &interp.escape_scoped(return_method),
                interp.escape_scoped(iterator),
                SmallVec::new(),
            )?;
            if !result.is_object() && !result.is_proxy() {
                return Err(interp
                    .err_type(("iterator `return` did not yield an object".to_string()).into()));
            }
            Ok(())
        })
    }

    /// §7.4.11 AsyncIteratorClose steps 3-5: read `iterator.return` and
    /// call it. `None` means the iterator has no `return`, which leaves
    /// nothing to await and nothing to check.
    ///
    /// The awaiting and the "result is an Object" check belong to the
    /// caller: an async close awaits the result before inspecting it,
    /// which a synchronous close cannot do.
    pub(crate) fn async_iterator_return_call(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        iterator: Value,
    ) -> Result<Option<Value>, VmError> {
        self.with_handle_scope(|interp, scope| {
            let iterator = interp.scoped_value(scope, iterator);
            let current = interp.escape_scoped(iterator);
            let return_method = match interp.ordinary_get_value(
                stack,
                context,
                current,
                current,
                &VmPropertyKey::String("return"),
                0,
            )? {
                VmGetOutcome::Value(value) => interp.scoped_value(scope, value),
                VmGetOutcome::InvokeGetter { getter } => {
                    let getter = interp.scoped_value(scope, getter);
                    let value = interp.run_callable_sync_rooted(
                        stack,
                        context,
                        &interp.escape_scoped(getter),
                        interp.escape_scoped(iterator),
                        SmallVec::new(),
                    )?;
                    interp.scoped_value(scope, value)
                }
            };
            let return_value = interp.escape_scoped(return_method);
            if return_value.is_undefined() || return_value.is_null() {
                return Ok(None);
            }
            if !interp.is_callable_runtime(&return_value) {
                return Err(
                    interp.err_type(("iterator `return` is not callable".to_string()).into())
                );
            }
            let result = interp.run_callable_sync_rooted(
                stack,
                context,
                &interp.escape_scoped(return_method),
                interp.escape_scoped(iterator),
                SmallVec::new(),
            )?;
            Ok(Some(result))
        })
    }

    pub(crate) fn iterator_close_value_sync(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        iterator: Value,
    ) -> Result<(), VmError> {
        enum CloseAction {
            User(Value),
            Generator(crate::generator::JsGenerator),
            /// Iterator-helper wrapper — closing it forwards the close
            /// to the (optional) inner iterator, then to the source.
            Helper {
                source: IteratorHandle,
                inner: Option<IteratorHandle>,
            },
            /// `Iterator.concat` — only the currently open iterable is
            /// closed; the not-yet-opened ones were never started.
            ConcatInner(Option<IteratorHandle>),
            /// `Iterator.zip` — close every still-open input.
            ZipAll {
                started: bool,
            },
            /// A close re-entered the helper while its own close was
            /// still running.
            HelperRunning,
            Builtin,
            None,
        }
        let action = if let Some(handle) = iterator.as_iterator() {
            self.gc_heap.read_payload(handle, |state| match state {
                IteratorState::User { iterator, .. } => CloseAction::User(*iterator),
                // §7.4.9 — a generator's `return` resumes the suspended
                // body with a return completion so its `finally` blocks
                // run and `[[GeneratorState]]` becomes completed.
                IteratorState::Generator { handle } => CloseAction::Generator(*handle),
                // §27.1.2.1 — the helper wrappers behave like the spec's
                // implicit generators: a close runs their underlying
                // IteratorClose before completing.
                IteratorState::Map { source, .. }
                | IteratorState::Filter { source, .. }
                | IteratorState::Take { source, .. }
                | IteratorState::Drop { source, .. }
                | IteratorState::Chunks { source, .. }
                | IteratorState::Windows { source, .. } => CloseAction::Helper {
                    source: *source,
                    inner: None,
                },
                IteratorState::FlatMap { source, inner, .. } => CloseAction::Helper {
                    source: *source,
                    inner: *inner,
                },
                // §27.1.4.x — `Iterator.concat` owns no single source:
                // a close forwards only to whichever iterable is open.
                // §27.5.3.2 GeneratorValidate step 6 — a close that
                // re-enters from the forwarded `return` throws.
                IteratorState::Concat { inner, running, .. } => {
                    if *running {
                        CloseAction::HelperRunning
                    } else {
                        CloseAction::ConcatInner(*inner)
                    }
                }
                // A zip close forwards to every still-open input, in
                // reverse order (IteratorCloseAll).
                IteratorState::Zip {
                    running, started, ..
                } => {
                    if *running {
                        CloseAction::HelperRunning
                    } else {
                        CloseAction::ZipAll { started: *started }
                    }
                }
                // Array / TypedArray / String / Map / Set iterators
                // expose no `return`, so IteratorClose is a no-op.
                IteratorState::Exhausted { .. } => CloseAction::None,
                _ => CloseAction::Builtin,
            })
        } else {
            CloseAction::User(iterator)
        };
        match action {
            CloseAction::User(close_target) => {
                self.iterator_close_sync(stack, context, &close_target)?;
            }
            CloseAction::Generator(handle) => {
                self.resume_generator(
                    stack,
                    context,
                    &handle,
                    GeneratorResumeKind::Return(Value::undefined()),
                )?;
            }
            CloseAction::Helper { source, inner } => {
                // Mark the wrapper exhausted FIRST so a re-entrant or
                // repeated close does not forward twice.
                if let Some(handle) = iterator.as_iterator() {
                    self.gc_heap.with_payload(handle, |state| state.exhaust());
                }
                let inner_result = match inner {
                    Some(inner) => {
                        self.iterator_close_value_sync(stack, context, Value::iterator(inner))
                    }
                    None => Ok(()),
                };
                self.iterator_close_value_sync(stack, context, Value::iterator(source))?;
                inner_result?;
            }
            CloseAction::ConcatInner(inner) => {
                // Mark running (rather than exhausted) so a `return`
                // that re-enters this same helper is rejected instead of
                // silently becoming a no-op.
                if let Some(handle) = iterator.as_iterator() {
                    self.gc_heap.with_payload(handle, |state| {
                        if let IteratorState::Concat { running, .. } = state {
                            *running = true;
                        }
                    });
                }
                let inner_result = match inner {
                    Some(inner) => {
                        self.iterator_close_value_sync(stack, context, Value::iterator(inner))
                    }
                    None => Ok(()),
                };
                if let Some(handle) = iterator.as_iterator() {
                    self.gc_heap.with_payload(handle, |state| state.exhaust());
                }
                inner_result?;
            }
            CloseAction::ZipAll { started } => {
                // §27.1.2.1.2 step 4 — from suspended-start the helper
                // is completed *before* the inputs are closed, so a
                // re-entrant close returns normally. From suspended-yield
                // it is marked running, so a re-entrant close throws.
                let handle = iterator.as_iterator();
                let Some(handle_for_close) = handle else {
                    return Ok(());
                };
                // Snapshot before the state is folded away below.
                let open = self.zip_open_inputs(handle_for_close);
                if let Some(handle) = handle {
                    if started {
                        self.gc_heap.with_payload(handle, |state| {
                            if let IteratorState::Zip { running, .. } = state {
                                *running = true;
                            }
                        });
                    } else {
                        self.gc_heap.with_payload(handle, |state| state.exhaust());
                    }
                }
                let closed = self.iterator_zip_close_all(stack, context, open, false);
                if let Some(handle) = handle {
                    self.gc_heap.with_payload(handle, |state| state.exhaust());
                }
                closed?;
            }
            CloseAction::HelperRunning => {
                return Err(
                    self.err_type(("Iterator helper is already running".to_string()).into())
                );
            }
            CloseAction::Builtin | CloseAction::None => {}
        }
        Ok(())
    }

    /// §7.4.8 IfAbruptCloseIterator — close `handle` while preserving
    /// the original pending thrown value; the close result (normal or
    /// abrupt) is swallowed in favour of the original completion.
    pub(crate) fn close_iterator_preserving_throw(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        handle: &IteratorHandle,
    ) {
        let original_throw = self.take_pending_uncaught_throw();
        let _ = self.iterator_close_value_sync(stack, context, Value::iterator(*handle));
        if let Some(value) = original_throw {
            self.set_pending_uncaught_throw(value);
        }
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
        stack: &mut ActivationStack,
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
                    stack,
                    context,
                    &handle,
                    GeneratorResumeKind::Next(Value::undefined()),
                )?;
                let Some(record) = result.as_object() else {
                    return Err(self
                        .err_type(("generator next did not return an object".to_string()).into()));
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
                let (v, done) = self.iterator_next_full(context, stack, &handle)?;
                if done {
                    return Ok(out);
                }
                out.push(v);
            }
        }

        let (iterator, next_method) = self.get_iterator_sync(stack, context, iterable)?;
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
            match self.iterator_step_sync(stack, context, &iterator, &next_method) {
                Ok(Some(value)) => values.push(value),
                Ok(None) => break Ok(values),
                Err(err) => {
                    // Best-effort close; original error wins.
                    let _ = self.iterator_close_sync(stack, context, &iterator);
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
        self.note_settle_rejection(&jobs);
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

    /// Resume a generator object above a floor on the current activation stack
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
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        handle: &crate::generator::JsGenerator,
        kind: GeneratorResumeKind,
    ) -> Result<Value, VmError> {
        let _window_rollback = self.register_window_rollback();
        let (frame_opt, resume_dst) = (
            handle.has_frame(&self.gc_heap),
            handle.resume_dst(&self.gc_heap),
        );
        if !frame_opt {
            // §27.5.3.2 GeneratorValidate — a generator whose frame is
            // checked out but not done is mid-dispatch: state
            // "executing" throws TypeError.
            if !handle.is_done(&self.gc_heap) {
                handle.mark_done(&mut self.gc_heap);
                return Err(self.err_type(("Generator is already running".to_string()).into()));
            }
            // Completed (§27.5.3.3/.4): `next` yields {undefined, true},
            // `return` echoes its argument, `throw` re-raises.
            return match kind {
                GeneratorResumeKind::Next(_) => {
                    self.make_runtime_rooted_iter_result(Value::undefined(), true, &[], &[])
                }
                GeneratorResumeKind::Return(arg) => {
                    self.make_runtime_rooted_iter_result(arg, true, &[], &[])
                }
                GeneratorResumeKind::Throw(reason) => {
                    self.set_pending_uncaught_throw(reason);
                    Err(self.err_uncaught((self.render_thrown(&reason)).into()))
                }
            };
        }
        // Pull the frame out of the gen body so we can mutate it.
        let (frame, cold) = match handle.take_frame(&mut self.gc_heap) {
            Some(pair) => pair,
            None => {
                return self.make_runtime_rooted_iter_result(Value::undefined(), true, &[], &[]);
            }
        };
        let mut frame = Box::new(self.resume_parked_frame(*frame)?);
        if let Some(c) = cold {
            self.frame_attach_cold(&mut frame, c);
        }
        // §27.5.3.7 — a frame parked on `Op::YieldDelegate` receives
        // every resume kind as data (kind code + argument) so the
        // compiled `yield*` loop can forward it to the inner
        // iterator's next / throw / return method instead of the
        // generator unwinding or completing.
        let delegating = handle.is_delegating(&self.gc_heap);
        if delegating {
            handle.clear_delegating(&mut self.gc_heap);
            let kind_dst = handle.resume_kind_dst(&self.gc_heap);
            let (code, arg) = match &kind {
                GeneratorResumeKind::Next(v) => (0, *v),
                GeneratorResumeKind::Throw(v) => (1, *v),
                GeneratorResumeKind::Return(v) => (2, *v),
            };
            if let Some(slot) = frame.registers.get_mut(kind_dst as usize) {
                *slot = Value::number_i32(code);
            }
            if let Some(slot) = frame.registers.get_mut(resume_dst as usize) {
                *slot = arg;
            }
            let is_async = handle.is_async(&self.gc_heap);
            if is_async {
                handle.set_async_state(&mut self.gc_heap, AsyncGeneratorState::Executing);
            }
            let floor = stack.floor();
            stack.push(*frame);
            return self.finish_generator_dispatch(stack, floor, context, handle, is_async);
        }
        // Apply the resume operation to the frame before re-entering
        // dispatch.
        let mut throw_value: Option<Value> = None;
        let mut return_value: Option<Value> = None;
        match &kind {
            GeneratorResumeKind::Next(arg) => {
                // §27.5.3.3 — the value passed to the first `next()`
                // (frame parked at GeneratorStart) is discarded.
                if frame.pc != 0
                    && resume_dst != crate::generator::JsGenerator::RESUME_DST_NONE
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
                for (iterator, _) in closers.iter().rev() {
                    self.iterator_close_value_sync(stack, context, *iterator)?;
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
        let floor = stack.floor();
        stack.push(*frame);
        if let Some(arg) = return_value {
            // Drive the parked frame's `finally` blocks via the abrupt
            // `return` path; `EndFinally` resumes the completion.
            match self.return_running_finally_above(stack, floor, arg) {
                Ok(Some(v)) => {
                    handle.mark_done(&mut self.gc_heap);
                    self.release_frames_above(stack, floor);
                    return self.make_runtime_rooted_iter_result(v, true, &[], &[]);
                }
                Ok(None) => { /* finally parked; dispatch below runs it */ }
                Err(err) => {
                    handle.mark_done(&mut self.gc_heap);
                    self.release_frames_above(stack, floor);
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
            match self.unwind_throw_above(context, stack, floor, reason) {
                Ok(_) => {}
                Err(err) => {
                    handle.mark_done(&mut self.gc_heap);
                    self.release_frames_above(stack, floor);
                    return Err(err);
                }
            }
            if stack.is_at_floor(floor) {
                handle.mark_done(&mut self.gc_heap);
                return Err(self.err_uncaught(("generator-throw".to_string()).into()));
            }
            // A handler caught the throw — clear the side channel.
            self.pending_generator_throw = None;
        }
        let is_async = handle.is_async(&self.gc_heap);
        if is_async {
            handle.set_async_state(&mut self.gc_heap, AsyncGeneratorState::Executing);
        }
        self.finish_generator_dispatch(stack, floor, context, handle, is_async)
    }

    /// Run the resumed generator frame to its next suspension or
    /// completion and shape the `.next()` result. Shared by the
    /// ordinary resume path and the §27.5.3.7 delegating resume.
    fn finish_generator_dispatch(
        &mut self,
        stack: &mut ActivationStack,
        floor: crate::ActivationFloor,
        context: &ExecutionContext,
        handle: &crate::generator::JsGenerator,
        is_async: bool,
    ) -> Result<Value, VmError> {
        let outcome = self.dispatch_loop_above_rooted(context, stack, floor);
        let result = (|| match outcome {
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
                    // §27.5.3.7 — a delegating suspension surfaces
                    // the inner iterator result object verbatim.
                    if handle.is_delegating(&self.gc_heap) {
                        return Ok(v);
                    }
                    return self.make_runtime_rooted_iter_result(v, false, &[], &[]);
                }
                // Body ran to completion or `Op::Await` parked the
                // frame. Distinguish by whether the gen still owns
                // the frame: a parked await leaves the slot empty
                // (the await microtask owns it) AND `sub_stack` is
                // empty.
                // An `Op::Await` parking stores the frame in the resume
                // microtask's closure, not on the gen handle, and stamps
                // `Awaiting` — the request queue cannot answer this (the
                // caller's own `.next()` request is still queued either way).
                let parked = is_async
                    && !handle.has_frame(&self.gc_heap)
                    && handle.async_state(&self.gc_heap) == AsyncGeneratorState::Awaiting;
                if parked {
                    // The resume microtask will eventually settle the queued
                    // request.
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
                        self.vm_error_to_throwable_with_stack_roots(Some(context), stack, &err)
                    };
                    if let Some(reason) = rejection {
                        self.async_generator_complete_step(context, handle, Err(reason), true)?;
                        self.async_generator_drain_done(context, handle)?;
                        return Ok(Value::undefined());
                    }
                }
                Err(err)
            }
        })();
        self.release_frames_above(stack, floor);
        result
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
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        operands: impl crate::executable::OperandSource,
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
            // Object; GetIteratorDirect then reads `next` once. Both
            // are owned by the shared wrap helper. A failure here must
            // still clear the parked continuation before propagating.
            let produced_value = match self.wrap_iterator_method_result(context, stack, produced) {
                Ok(value) => value,
                Err(e) => {
                    if let Some(cold) = self.frame_cold_mut(&mut stack[top_idx]) {
                        cold.pending_get_iterator = None;
                    }
                    return Err(e);
                }
            };
            write_register(&mut stack[top_idx], dst, produced_value)?;
            if let Some(cold) = self.frame_cold_mut(&mut stack[top_idx]) {
                cold.pending_get_iterator = None;
            }
            stack[top_idx].advance_pc()?;
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
                    stack,
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
                        stack,
                        context,
                        proto,
                        value,
                        &VmPropertyKey::Symbol(iter_sym),
                        0,
                    )? {
                        VmGetOutcome::Value(v) => v,
                        VmGetOutcome::InvokeGetter { getter } => self.run_callable_sync_rooted(
                            stack,
                            context,
                            &getter,
                            value,
                            SmallVec::new(),
                        )?,
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
                stack,
                context,
                value,
                value,
                &VmPropertyKey::Symbol(iter_sym),
                0,
            )? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync_rooted(stack, context, &getter, value, SmallVec::new())?
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
        if value.as_object().is_none() && value.as_proxy().is_none() {
            return Ok(false);
        }
        // §7.4.3 GetIterator step 1 — GetMethod(obj, @@iterator) runs
        // the ordinary [[Get]] ladder, so an accessor-defined
        // @@iterator fires its getter (and the getter's abrupt
        // completion propagates) instead of reading a data slot.
        let callee = match self.ordinary_get_value(
            stack,
            context,
            value,
            value,
            &VmPropertyKey::Symbol(iter_sym),
            0,
        )? {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync_rooted(stack, context, &getter, value, SmallVec::new())?
            }
        };
        if callee.is_undefined() || callee.is_null() || !is_callable(&callee) {
            // No `[Symbol.iterator]` — §7.4.3 step 2 throws.
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
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        operands: impl crate::executable::OperandSource,
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
            // §7.4.5 step 3 — the result record must be Type Object,
            // which includes Proxy and other exotic shapes, not just
            // plain JsObject.
            if !crate::reflect::is_type_object_value(&result) {
                if let Some(cold) = self.frame_cold_mut(&mut stack[top_idx]) {
                    cold.pending_iterator_next = None;
                }
                return Err(VmError::TypeMismatch);
            }
            // A throw out of the `done` / `value` getters also sets
            // [[Done]] (IteratorStepValue); drop the parked state so a
            // later IteratorNext at this pc starts fresh.
            let step = match iterator_step_read(self, stack, context, &result) {
                Ok(step) => step,
                Err(e) => {
                    if let Some(cold) = self.frame_cold_mut(&mut stack[top_idx]) {
                        cold.pending_iterator_next = None;
                    }
                    return Err(e);
                }
            };
            let (value, done) = match step {
                Some(value) => (value, false),
                None => (Value::undefined(), true),
            };
            if done && let Some(rc) = state.iterator.as_iterator() {
                self.gc_heap.with_payload(rc, |state| state.exhaust());
            }
            if !done {
                // §7.4.9 — `next` produced a value without throwing, so
                // the iterator is live again: re-arm its closer (cleared
                // before the call) so a body abrupt completion runs
                // `[[return]]`.
                self.register_frame_iterator_closer(&mut stack[top_idx], state.iterator);
            }
            write_register(&mut stack[top_idx], value_dst, value)?;
            write_register(&mut stack[top_idx], done_dst, Value::boolean(done))?;
            if let Some(cold) = self.frame_cold_mut(&mut stack[top_idx]) {
                cold.pending_iterator_next = None;
            }
            stack[top_idx].advance_pc()?;
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
                stack,
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
                self.gc_heap.with_payload(*iter_rc, |state| state.exhaust());
            }
            write_register(&mut stack[top_idx], value_dst, value)?;
            write_register(&mut stack[top_idx], done_dst, Value::boolean(done))?;
            stack[top_idx].advance_pc()?;
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
                    | IteratorState::Chunks { .. }
                    | IteratorState::Windows { .. }
                    | IteratorState::Concat { .. }
                    | IteratorState::Zip { .. }
                    | IteratorState::RegExpString { .. }
            )
        });
        if needs_full_step {
            let (value, done) = self.iterator_next_full(context, stack, iter_rc)?;
            write_register(&mut stack[top_idx], value_dst, value)?;
            write_register(&mut stack[top_idx], done_dst, Value::boolean(done))?;
            stack[top_idx].advance_pc()?;
            return Ok(true);
        }
        // §23.1.5.1 ArrayIterator `next` performs Get(array, index) —
        // an element backed by an accessor must run its getter through
        // the interpreter (and propagate its abrupt completion); the
        // synchronous fast path below reads raw element storage only.
        let array_step = self.gc_heap.read_payload(*iter_rc, |state| match state {
            IteratorState::Array { array, index, .. } => Some((*array, *index)),
            _ => None,
        });
        if let Some((array, index)) = array_step
            && crate::array::has_accessors(array, &self.gc_heap)
            && index < crate::array::len(array, &self.gc_heap)
            && let Some((getter, _)) =
                crate::array::get_accessor(array, &self.gc_heap, &index.to_string())
        {
            // Advance before the getter runs so a re-entrant `next`
            // from inside it observes the post-step index.
            self.gc_heap.with_payload(*iter_rc, |state| {
                if let IteratorState::Array { index, .. } = state {
                    *index += 1;
                }
            });
            let value = match getter {
                Some(g) => self.run_callable_sync_rooted(
                    stack,
                    context,
                    &g,
                    Value::array(array),
                    SmallVec::new(),
                )?,
                None => Value::undefined(),
            };
            write_register(&mut stack[top_idx], value_dst, value)?;
            write_register(&mut stack[top_idx], done_dst, Value::boolean(false))?;
            stack[top_idx].advance_pc()?;
            return Ok(true);
        }
        // Snapshot the user iterator object out of the inner
        // state so the borrow does not span the `invoke` call
        // below.
        let user_iter = self.gc_heap.read_payload(*iter_rc, |state| match state {
            IteratorState::User {
                iterator,
                next_method,
            } => Some((*iterator, *next_method)),
            _ => None,
        });
        let Some((user_iter_value, cached_next)) = user_iter else {
            // Built-in iterator — let the synchronous in-frame
            // path drive it.
            return Ok(false);
        };
        // §7.4.2 — the iterator record caches [[NextMethod]] at
        // GetIterator time; use it when present. Legacy User states
        // without a cache resolve through the ordinary [[Get]]
        // ladder so a Proxy iterator (or one exposing `next` via an
        // accessor) is handled, not just plain objects.
        let next_fn = match cached_next {
            Some(cached) => cached,
            None => match self.ordinary_get_value(
                stack,
                context,
                user_iter_value,
                user_iter_value,
                &VmPropertyKey::String("next"),
                0,
            )? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => self.run_callable_sync_rooted(
                    stack,
                    context,
                    &getter,
                    user_iter_value,
                    SmallVec::new(),
                )?,
            },
        };
        if !is_callable(&next_fn) {
            return Err(VmError::TypeMismatch);
        }
        // §7.4.9 / IteratorStepValue — a throw out of `next` (or out
        // of reading the result record) sets [[Done]] and skips
        // IteratorClose. The call runs in a pushed frame, so its
        // throw unwinds without ever returning `Err` to this opcode:
        // disarm the closer for the span of the call; the resume path
        // re-arms it once `next` yields `done: false`.
        self.deregister_frame_iterator_closer(&mut stack[top_idx], iter_value);
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
    stack: &mut ActivationStack,
    context: &ExecutionContext,
    result: &Value,
) -> Result<Option<Value>, VmError> {
    let done_value = match interp.ordinary_get_value(
        stack,
        context,
        *result,
        *result,
        &VmPropertyKey::String("done"),
        0,
    )? {
        VmGetOutcome::Value(v) => v,
        VmGetOutcome::InvokeGetter { getter } => {
            interp.run_callable_sync_rooted(stack, context, &getter, *result, SmallVec::new())?
        }
    };
    if done_value.to_boolean(interp.gc_heap()) {
        return Ok(None);
    }
    let value = match interp.ordinary_get_value(
        stack,
        context,
        *result,
        *result,
        &VmPropertyKey::String("value"),
        0,
    )? {
        VmGetOutcome::Value(v) => v,
        VmGetOutcome::InvokeGetter { getter } => {
            interp.run_callable_sync_rooted(stack, context, &getter, *result, SmallVec::new())?
        }
    };
    Ok(Some(value))
}
