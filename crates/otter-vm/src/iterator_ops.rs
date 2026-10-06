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
//! - Full iterator steps distinguish source VM errors from completed native
//!   failures through the existing CommittedValueError owner. Explicitly
//!   completed child calls retain that disposition before recursive cleanup.
//! - Missing generator source, disposed creation realm, and invalid resume
//!   register/site metadata are structural terminal failures. They never restore
//!   an incoming IteratorClose throw or run subordinate JavaScript cleanup.
//! - Generator body completion is classified after its published dispatch
//!   finishes. Fresh iterator-result allocation retains the current JavaScript
//!   operation's local error domain.
//! - IteratorClose holds its iterator and discovered `return` method in
//!   canonical handles, reloading the receiver after accessor/call re-entry.
//!
//! # See also
//! - [`crate::executable`]
//! - [`crate::IteratorState`]

mod close_completion;

use crate::activation_stack::ActivationStack;
use smallvec::SmallVec;

use crate::{
    CommittedValueError, ExecutionContext, Frame, GeneratorResumeKind, Interpreter, IteratorHandle,
    IteratorState, JsPromise, JsString, PendingGetIterator, PendingIteratorNext, Value, VmError,
    VmGetOutcome, VmPropertyKey, array, generator::AsyncGeneratorState, is_callable,
    iterator_state::ArrayIterKind, operand_decode::register_operand, promise::PromiseCapability,
    read_register, step_iterator, symbol, write_register,
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
    /// `GetIterator` for a value whose record needs no observable read (see
    /// [`Self::unobservable_iterator_record`]); any other value is a
    /// `TypeMismatch` here.
    #[cfg(test)]
    pub(crate) fn run_get_iterator_regs(
        &mut self,
        stack: &mut ActivationStack,
        top_idx: usize,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let value = *read_register(&stack[top_idx], src)?;
        let record = self
            .unobservable_iterator_record(stack, value, false)?
            .ok_or(VmError::TypeMismatch)?;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, record)?;
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
    ) -> Result<(), CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            let value = *read_register(&stack[top_idx], src)
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            let value_root = interp.scoped_value(scope, value);
            if let Some(handle) = value.as_generator()
                && handle.is_async(&interp.gc_heap)
            {
                write_register(&mut stack[top_idx], dst, value)
                    .map_err(|error| CommittedValueError::Fatal(error.into()))?;
                stack[top_idx]
                    .advance_pc()
                    .map_err(|error| CommittedValueError::Fatal(error.into()))?;
                return Ok(());
            }

            let async_iter_sym = interp
                .well_known_symbols
                .get(symbol::WellKnown::AsyncIterator);
            let can_have_method = value.as_object().is_some()
                || value.as_array().is_some()
                || value.as_map().is_some()
                || value.as_set().is_some()
                || value.is_proxy();
            if can_have_method {
                let method = match interp.ordinary_get_value(
                    stack,
                    Some(context),
                    value,
                    value,
                    &VmPropertyKey::Symbol(async_iter_sym),
                    0,
                )? {
                    VmGetOutcome::Value(v) => v,
                    VmGetOutcome::InvokeGetter { getter } => interp
                        .run_callable_sync_rooted(
                            stack,
                            Some(context),
                            &getter,
                            interp.escape_scoped(value_root),
                            SmallVec::new(),
                        )
                        .map_err(CommittedValueError::completed_call)?,
                };
                if !method.is_nullish() {
                    if !is_callable(&method) {
                        return Err(CommittedValueError::JavaScript(VmError::TypeMismatch));
                    }
                    let produced = interp
                        .run_callable_sync_rooted(
                            stack,
                            Some(context),
                            &method,
                            interp.escape_scoped(value_root),
                            SmallVec::new(),
                        )
                        .map_err(CommittedValueError::completed_call)?;
                    if produced.as_object().is_none()
                        && produced.as_generator().is_none()
                        && produced.as_iterator().is_none()
                        && produced.as_array().is_none()
                        && !produced.is_proxy()
                    {
                        return Err(CommittedValueError::JavaScript(VmError::TypeMismatch));
                    }
                    write_register(&mut stack[top_idx], dst, produced)
                        .map_err(|error| CommittedValueError::Fatal(error.into()))?;
                    stack[top_idx]
                        .advance_pc()
                        .map_err(|error| CommittedValueError::Fatal(error.into()))?;
                    return Ok(());
                }
            }

            // §27.1.4 — an iterable with no `@@asyncIterator` is driven through
            // an adapter, so each step's *value* is awaited and not just the
            // result record. Without it `for await` over an array of promises
            // hands the body the promises themselves.
            let iterator = interp.get_sync_iterator_object(
                stack,
                context,
                interp.escape_scoped(value_root),
            )?;
            let wrapped = interp.create_async_from_sync_iterator(stack, context, iterator)?;
            write_register(&mut stack[top_idx], dst, wrapped)
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            stack[top_idx]
                .advance_pc()
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            Ok(())
        })
    }

    /// §7.4.2 GetIterator(obj, sync) — the object `@@iterator` produced,
    /// kept whole so the async adapter can call `next` / `return` on it.
    fn get_sync_iterator_object(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        value: Value,
    ) -> Result<Value, CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            let value_root = interp.scoped_value(scope, value);

            let iterator_sym = interp.well_known_symbols.get(symbol::WellKnown::Iterator);
            let method = match interp.ordinary_get_value(
                stack,
                Some(context),
                value,
                value,
                &VmPropertyKey::Symbol(iterator_sym),
                0,
            )? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => interp
                    .run_callable_sync_rooted(
                        stack,
                        Some(context),
                        &getter,
                        interp.escape_scoped(value_root),
                        SmallVec::new(),
                    )
                    .map_err(CommittedValueError::completed_call)?,
            };
            if !is_callable(&method) {
                return Err(CommittedValueError::JavaScript(VmError::TypeMismatch));
            }
            let produced = interp
                .run_callable_sync_rooted(
                    stack,
                    Some(context),
                    &method,
                    interp.escape_scoped(value_root),
                    SmallVec::new(),
                )
                .map_err(CommittedValueError::completed_call)?;
            if !produced.is_object_type() && !produced.is_proxy() {
                return Err(CommittedValueError::JavaScript(VmError::TypeMismatch));
            }
            Ok(produced)
        })
    }

    /// §7.4.2 GetIteratorDirect — wrap the object returned by
    /// `[@@iterator]()` into an iterator record, reading `next` exactly
    /// once and caching it unless it is the built-in step of a built-in
    /// iterator. Shared by the interpreter's
    /// frame-push resume path and the synchronous [`Self::get_iterator_full`]
    /// reentrant transition, so both tiers observe identical accessor and
    /// prototype effects.
    pub(crate) fn wrap_iterator_method_result(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        produced: Value,
        primordial: bool,
    ) -> Result<Value, CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            let produced_root = interp.scoped_value(scope, produced);

            // A built-in iterator whose `next` is proven built-in (or that
            // runtime-internal code steps intrinsically) is its own record.
            if produced.is_iterator()
                && (primordial && interp.plain_iterator_origin(produced).is_some()
                    || interp.builtin_next_proven(produced))
            {
                return Ok(interp.escape_scoped(produced_root));
            }
            if produced.is_generator()
                && (primordial || interp.proven_generator_next(interp.escape_scoped(produced_root)))
            {
                let produced = interp.escape_scoped(produced_root);
                let handle = produced
                    .as_generator()
                    .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
                let iter = interp
                    .alloc_stack_rooted_iterator_state(
                        stack,
                        IteratorState::Generator { handle },
                        &[&produced],
                        &[],
                    )
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                return Ok(Value::iterator(iter));
            }
            let produced = interp.escape_scoped(produced_root);
            // §7.4.3 step 2 — `[@@iterator]()` must return an Object.
            let iter_state = if produced.is_object()
                || produced.is_proxy()
                || produced.is_array()
                || produced.is_map()
                || produced.is_set()
                || produced.is_iterator()
                || produced.is_generator()
            {
                // `next` is read ONCE here; later `IteratorNext` ticks must not
                // re-read it (observable via an accessor-defined `next`).
                let next_method = match interp.ordinary_get_value(
                    stack,
                    Some(context),
                    produced,
                    produced,
                    &VmPropertyKey::String("next"),
                    0,
                )? {
                    VmGetOutcome::Value(v) => v,
                    VmGetOutcome::InvokeGetter { getter } => interp
                        .run_callable_sync_rooted(
                            stack,
                            Some(context),
                            &getter,
                            interp.escape_scoped(produced_root),
                            SmallVec::new(),
                        )
                        .map_err(CommittedValueError::completed_call)?,
                };
                if interp.is_builtin_next(interp.escape_scoped(produced_root), next_method) {
                    return Ok(interp.escape_scoped(produced_root));
                }
                if let Some(handle) = interp.escape_scoped(produced_root).as_generator()
                    && interp.is_generator_next(next_method)
                {
                    IteratorState::Generator { handle }
                } else {
                    IteratorState::User {
                        iterator: interp.escape_scoped(produced_root),
                        next_method: Some(next_method),
                    }
                }
            } else {
                return Err(CommittedValueError::JavaScript(VmError::TypeMismatch));
            };
            let iter = interp
                .alloc_stack_rooted_iterator_state(
                    stack,
                    iter_state,
                    &[&interp.escape_scoped(produced_root)],
                    &[],
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            Ok(Value::iterator(iter))
        })
    }

    /// Complete one `Op::GetIterator` synchronously for a compiled frame.
    ///
    /// This is the reentrant sibling of the interpreter's frame-push
    /// [`Self::drive_get_iterator`]: a user `[Symbol.iterator]()` method runs
    /// through [`Self::run_callable_sync`] instead of suspending the opcode on
    /// a parked continuation, so the JIT never resumes a partially observed
    /// GetIterator. Every observable accessor, `@@iterator` call and
    /// GetIteratorDirect `next` read is committed before the destination
    /// register is written; there is no post-effect side exit.
    pub(crate) fn get_iterator_full(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        dst: u16,
        src: u16,
    ) -> Result<(), CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            let value = *read_register(&stack[top_idx], src)
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            let value_root = interp.scoped_value(scope, value);
            let primordial = interp.frame_iterates_primordially(context, stack, top_idx);
            if let Some(record) = interp
                .unobservable_iterator_record(stack, value, primordial)
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?
            {
                write_register(&mut stack[top_idx], dst, record)
                    .map_err(|error| CommittedValueError::Fatal(error.into()))?;
                return Ok(());
            }
            // §7.4.3 GetIterator step 1 — GetMethod(obj, @@iterator) runs the
            // ordinary [[Get]], so an accessor `@@iterator` fires its getter.
            let callee =
                interp.get_iterator_method(stack, context, interp.escape_scoped(value_root))?;
            let produced = interp
                .run_callable_sync_rooted(
                    stack,
                    Some(context),
                    &callee,
                    interp.escape_scoped(value_root),
                    SmallVec::new(),
                )
                .map_err(CommittedValueError::completed_call)?;
            let wrapped =
                interp.wrap_iterator_method_result(context, stack, produced, primordial)?;
            write_register(&mut stack[top_idx], dst, wrapped)
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            Ok(())
        })
    }

    /// §7.4.3 GetIterator steps 1-2 — `GetMethod(value, @@iterator)`, which
    /// must be callable.
    fn get_iterator_method(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        value: Value,
    ) -> Result<Value, CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            let value_root = interp.scoped_value(scope, value);
            let iter_sym = interp.well_known_symbols.get(symbol::WellKnown::Iterator);
            let method = match interp.ordinary_get_value(
                stack,
                Some(context),
                value,
                value,
                &VmPropertyKey::Symbol(iter_sym),
                0,
            )? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => interp
                    .run_callable_sync_rooted(
                        stack,
                        Some(context),
                        &getter,
                        interp.escape_scoped(value_root),
                        SmallVec::new(),
                    )
                    .map_err(CommittedValueError::completed_call)?,
            };
            if !is_callable(&method) {
                return Err(CommittedValueError::JavaScript(VmError::TypeMismatch));
            }
            Ok(method)
        })
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
    ) -> Result<Value, CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            let record = interp.scoped_value(scope, record);
            let key = crate::VmPropertyKey::String(name);
            match interp.ordinary_get_value(
                stack,
                Some(context),
                interp.escape_scoped(record),
                interp.escape_scoped(record),
                &key,
                0,
            )? {
                crate::VmGetOutcome::Value(v) => Ok(v),
                crate::VmGetOutcome::InvokeGetter { getter } => interp
                    .run_callable_sync_rooted(
                        stack,
                        Some(context),
                        &getter,
                        interp.escape_scoped(record),
                        SmallVec::new(),
                    )
                    .map_err(CommittedValueError::completed_call),
            }
        })
    }

    /// The callback a `map` / `filter` / `flatMap` helper holds, read from
    /// its state cell after the step's last allocation. The state cell is
    /// allocated old and never moves; the callback is an ordinary movable
    /// value, so a snapshot copy taken before the source's `next` ran is
    /// stale by the time the helper calls it.
    fn iterator_helper_callback(&self, iter: IteratorHandle, fallback: Value) -> Value {
        self.gc_heap.read_payload(iter, |state| match state {
            IteratorState::Map { mapper, .. } | IteratorState::FlatMap { mapper, .. } => *mapper,
            IteratorState::Filter { predicate, .. } => *predicate,
            _ => fallback,
        })
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
    ) -> Result<(Value, bool), CommittedValueError> {
        // §23.1.5.1 ArrayIterator `next` performs Get(array, index): an
        // element that is not one plain dense value (an accessor, a
        // sparse slot, a hole answered by the prototype chain) must run
        // the observable [[Get]] instead of the raw element read the
        // synchronous fast step performs.
        enum ArrayObservable {
            Value(crate::array::JsArray, usize),
            Entry(crate::array::JsArray, usize),
        }
        let observable = self.gc_heap.read_payload(*iter, |state| match state {
            IteratorState::Array { array, index, .. } => {
                Some(ArrayObservable::Value(*array, *index))
            }
            IteratorState::ArrayEntry { array, index } => {
                Some(ArrayObservable::Entry(*array, *index))
            }
            _ => None,
        });
        // §23.1.5.1 over a generic array-like (an `arguments` object, a
        // Proxy, anything with a `length`): every step re-reads `length`
        // and the element through the observable [[Get]], so an accessor
        // or a proxy trap runs and its abrupt completion propagates.
        let array_like = self.gc_heap.read_payload(*iter, |state| match state {
            IteratorState::ArrayLike {
                object,
                index,
                kind,
            } => Some((*object, *index, *kind)),
            _ => None,
        });
        if let Some((object, index, kind)) = array_like {
            return self.array_like_iterator_step(stack, context, iter, object, index, kind);
        }
        if let Some(step) = observable {
            let (array, index, entry) = match step {
                ArrayObservable::Value(a, i) => (a, i, false),
                ArrayObservable::Entry(a, i) => (a, i, true),
            };
            if index < array::len(array, &self.gc_heap)
                && array::plain_dense_element(array, &self.gc_heap, index).is_none()
            {
                // Advance before the getter runs so a re-entrant `next`
                // from inside it observes the post-step index.
                self.gc_heap.with_payload(*iter, |state| match state {
                    IteratorState::Array { index, .. }
                    | IteratorState::ArrayEntry { index, .. } => *index += 1,
                    _ => {}
                });
                let value = self.load_property_value(
                    context,
                    stack,
                    Value::array(array),
                    &index.to_string(),
                )?;
                if entry {
                    let idx_value = Value::number_f64(index as f64);
                    let pair = self
                        .alloc_runtime_rooted_array_from_values([idx_value, value], &[&value], &[])
                        .map_err(CommittedValueError::JavaScript)?;
                    return Ok((Value::array(pair), false));
                }
                return Ok((value, false));
            }
        }
        match step_iterator(*iter, &mut self.gc_heap) {
            Ok((value, done)) => Ok((value, done)),
            Err(_) => self.iterator_next_full_slow(context, stack, iter),
        }
    }

    /// One §23.1.5.1 CreateArrayIterator step over a generic array-like.
    ///
    /// The receiver is anchored across every observable read: both the
    /// `length` coercion and the element [[Get]] can run user code and
    /// move the heap.
    fn array_like_iterator_step(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        iter: &IteratorHandle,
        object: Value,
        index: usize,
        kind: ArrayIterKind,
    ) -> Result<(Value, bool), CommittedValueError> {
        let anchor = self.push_iteration_anchor(object) - 1;
        let result = (|interp: &mut Self| -> Result<(Value, bool), CommittedValueError> {
            let receiver = interp.iteration_anchor(anchor);
            let length = interp.load_property_value(context, stack, receiver, "length")?;
            let len = crate::coerce::to_length_or_throw(interp, stack, context, &length)?;
            if index >= len {
                interp.gc_heap.with_payload(*iter, |state| state.exhaust());
                return Ok((Value::undefined(), true));
            }
            // Advance before the element read so a re-entrant `next`
            // from inside a getter observes the post-step index.
            interp.gc_heap.with_payload(*iter, |state| {
                if let IteratorState::ArrayLike { index, .. } = state {
                    *index += 1;
                }
            });
            let index_value = Value::number_f64(index as f64);
            if matches!(kind, ArrayIterKind::Key) {
                return Ok((index_value, false));
            }
            let receiver = interp.iteration_anchor(anchor);
            let value = interp.load_property_value(context, stack, receiver, &index.to_string())?;
            if matches!(kind, ArrayIterKind::Entry) {
                let pair = interp
                    .alloc_runtime_rooted_array_from_values([index_value, value], &[&value], &[])
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                return Ok((Value::array(pair), false));
            }
            Ok((value, false))
        })(self);
        self.pop_iteration_anchors_to(anchor);
        result
    }

    fn iterator_next_full_slow(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        iter: &IteratorHandle,
    ) -> Result<(Value, bool), CommittedValueError> {
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
        let snapshot = snapshot
            .ok_or(VmError::TypeMismatch)
            .map_err(CommittedValueError::JavaScript)?;
        match snapshot {
            IteratorStateSnapshot::Generator(handle) => {
                let result = self.resume_generator(
                    stack,
                    Some(context),
                    &handle,
                    GeneratorResumeKind::Next(Value::undefined()),
                )?;
                let Some(record) = result.as_object() else {
                    return Err(CommittedValueError::JavaScript(VmError::TypeMismatch));
                };
                let (value, done) = self.iterator_result_parts(record);
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
                let (next_fn, iter_value) = match next_method {
                    Some(next_fn) => (next_fn, iter_value),
                    None => self.with_handle_scope(|interp, scope| {
                        let receiver = interp.scoped_value(scope, iter_value);
                        let key = crate::VmPropertyKey::String("next");
                        let outcome = interp.ordinary_get_value(
                            stack,
                            Some(context),
                            interp.escape_scoped(receiver),
                            interp.escape_scoped(receiver),
                            &key,
                            0,
                        )?;
                        let next_fn = match outcome {
                            crate::VmGetOutcome::Value(value) => value,
                            crate::VmGetOutcome::InvokeGetter { getter } => interp
                                .run_callable_sync_rooted(
                                    stack,
                                    Some(context),
                                    &getter,
                                    interp.escape_scoped(receiver),
                                    SmallVec::new(),
                                )
                                .map_err(CommittedValueError::completed_call)?,
                        };
                        Ok::<_, CommittedValueError>((next_fn, interp.escape_scoped(receiver)))
                    })?,
                };
                if !self.is_callable_runtime(&next_fn) {
                    return Err(CommittedValueError::JavaScript(VmError::TypeMismatch));
                }
                // A `next` getter can have moved the iterator object; the
                // state cell is old and traced, so read it from there.
                let iter_value = self.gc_heap.read_payload(*iter, |state| match state {
                    IteratorState::User { iterator, .. } => *iterator,
                    _ => iter_value,
                });
                let result = self
                    .run_callable_sync_rooted(
                        stack,
                        Some(context),
                        &next_fn,
                        iter_value,
                        SmallVec::new(),
                    )
                    .map_err(CommittedValueError::completed_call)?;
                if !crate::reflect::is_type_object_value(&result) {
                    return Err(CommittedValueError::JavaScript(
                        self.err_type(("iterator result is not an object".to_string()).into()),
                    ));
                }
                // §7.4.5 IteratorComplete / §7.4.6 IteratorValue read
                // `done` then (when not done) `value` through the
                // ordinary `[[Get]]`, so an accessor result object fires
                // its getters and an abrupt completion propagates rather
                // than silently reading `undefined` (which would never
                // terminate a `done`-less iterator).
                // A `done` getter allocates; the result object rides the
                // anchor stack to the `value` read.
                let result_slot = self.push_iteration_anchor(result) - 1;
                let done = match self.iter_result_get(stack, context, result, "done") {
                    Ok(done) => done.to_boolean(&self.gc_heap),
                    Err(err) => {
                        self.pop_iteration_anchors_to(result_slot);
                        return Err(err);
                    }
                };
                let result = self.iteration_anchor(result_slot);
                self.pop_iteration_anchors_to(result_slot);
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
                    return Err(CommittedValueError::JavaScript(self.err_type(
                        ("Iterator helper is already running".to_string()).into(),
                    )));
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
                let mapper = self.iterator_helper_callback(*iter, mapper);
                let mapped = match self.run_callable_sync_rooted(
                    stack,
                    Some(context),
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
                        let err = CommittedValueError::completed_call(err);
                        if matches!(&err, CommittedValueError::Fatal(_)) {
                            return Err(err);
                        }
                        self.close_iterator_preserving_throw(stack, Some(context), &source)?;
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
                    return Err(CommittedValueError::JavaScript(self.err_type(
                        ("Iterator helper is already running".to_string()).into(),
                    )));
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
                    let predicate = self.iterator_helper_callback(*iter, predicate);
                    // The kept value is yielded after the predicate ran and
                    // possibly moved it.
                    let value_slot = self.push_iteration_anchor(v) - 1;
                    let called = self.run_callable_sync_rooted(
                        stack,
                        Some(context),
                        &predicate,
                        Value::undefined(),
                        smallvec::smallvec![v, counter_value],
                    );
                    let v = self.iteration_anchor(value_slot);
                    self.pop_iteration_anchors_to(value_slot);
                    let kept = match called {
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
                            let err = CommittedValueError::completed_call(err);
                            if matches!(&err, CommittedValueError::Fatal(_)) {
                                return Err(err);
                            }
                            self.close_iterator_preserving_throw(stack, Some(context), &source)?;
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
                    return Err(CommittedValueError::JavaScript(self.err_type(
                        ("Iterator helper is already running".to_string()).into(),
                    )));
                }
                if remaining == 0 {
                    self.gc_heap.with_payload(*iter, |state| state.exhaust());
                    // §27.1.4.9 step 5.b.ii — the limit being reached
                    // closes the underlying iterator with a normal
                    // completion.
                    self.iterator_close_value_sync(stack, Some(context), Value::iterator(source))?;
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
                    return Err(CommittedValueError::JavaScript(self.err_type(
                        ("Iterator helper is already running".to_string()).into(),
                    )));
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
            } => {
                loop {
                    if running {
                        return Err(CommittedValueError::JavaScript(self.err_type(
                            ("Iterator helper is already running".to_string()).into(),
                        )));
                    }
                    if let Some(inner_iter) = inner.take() {
                        let (v, done) = match self.iterator_next_full(context, stack, &inner_iter) {
                            Ok(next) => next,
                            Err(err) => {
                                self.gc_heap.with_payload(*iter, |state| state.exhaust());
                                if matches!(&err, CommittedValueError::Fatal(_)) {
                                    return Err(err);
                                }
                                self.close_iterator_preserving_throw(
                                    stack,
                                    Some(context),
                                    &source,
                                )?;
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
                    let mapper = self.iterator_helper_callback(*iter, mapper);
                    let mapped = match self.run_callable_sync_rooted(
                        stack,
                        Some(context),
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
                            let err = CommittedValueError::completed_call(err);
                            if matches!(&err, CommittedValueError::Fatal(_)) {
                                return Err(err);
                            }
                            self.close_iterator_preserving_throw(stack, Some(context), &source)?;
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
                        // The mapper's fresh result is not in the helper state
                        // yet. Hold it in the canonical arena while Get/getter and
                        // the iterable method can move it.
                        let opened = self.with_handle_scope(|interp, scope| {
                        let mapped = interp.scoped_value(scope, mapped);
                        let outcome = interp
                            .ordinary_get_value(
                                stack,
                                Some(context),
                                interp.escape_scoped(mapped),
                                interp.escape_scoped(mapped),
                                &key,
                                0,
                            )
                            ?;
                        let iter_method = match outcome {
                            crate::VmGetOutcome::Value(value) => value,
                            crate::VmGetOutcome::InvokeGetter { getter } => interp
                                .run_callable_sync_rooted(
                                    stack,
                                    Some(context),
                                    &getter,
                                    interp.escape_scoped(mapped),
                                    SmallVec::new(),
                                )
                                .map_err(CommittedValueError::completed_call)?,
                        };
                        if iter_method.is_nullish() {
                            Ok(interp.escape_scoped(mapped))
                        } else if interp.is_callable_runtime(&iter_method) {
                            interp
                                .run_callable_sync_rooted(
                                    stack,
                                    Some(context),
                                    &iter_method,
                                    interp.escape_scoped(mapped),
                                    SmallVec::new(),
                                )
                                .map_err(CommittedValueError::completed_call)
                        } else {
                            Err(CommittedValueError::JavaScript(interp.err_type(
                                ("Iterator.prototype.flatMap mapper return must be iterable".to_string()).into(),
                            )))
                        }
                    });
                        let iter_value = match opened {
                            Ok(value) => value,
                            Err(err) => {
                                self.gc_heap.with_payload(*iter, |state| state.exhaust());
                                if matches!(&err, CommittedValueError::Fatal(_)) {
                                    return Err(err);
                                }
                                self.close_iterator_preserving_throw(
                                    stack,
                                    Some(context),
                                    &source,
                                )?;
                                return Err(err);
                            }
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
                            let next = self.with_handle_scope(|interp, scope| {
                                let iterator = interp.scoped_value(scope, iter_value);
                                let outcome = interp.ordinary_get_value(
                                    stack,
                                    Some(context),
                                    interp.escape_scoped(iterator),
                                    interp.escape_scoped(iterator),
                                    &key,
                                    0,
                                )?;
                                let next_method = match outcome {
                                    crate::VmGetOutcome::Value(value) => value,
                                    crate::VmGetOutcome::InvokeGetter { getter } => interp
                                        .run_callable_sync_rooted(
                                            stack,
                                            Some(context),
                                            &getter,
                                            interp.escape_scoped(iterator),
                                            SmallVec::new(),
                                        )
                                        .map_err(CommittedValueError::completed_call)?,
                                };
                                Ok::<_, CommittedValueError>((
                                    interp.escape_scoped(iterator),
                                    next_method,
                                ))
                            });
                            let (iter_value, next_method) = match next {
                                Ok(next) => next,
                                Err(err) => {
                                    self.gc_heap.with_payload(*iter, |state| state.exhaust());
                                    if matches!(&err, CommittedValueError::Fatal(_)) {
                                        return Err(err);
                                    }
                                    self.close_iterator_preserving_throw(
                                        stack,
                                        Some(context),
                                        &source,
                                    )?;
                                    return Err(err);
                                }
                            };
                            if !self.is_callable_runtime(&next_method) {
                                self.gc_heap.with_payload(*iter, |state| state.exhaust());
                                self.close_iterator_preserving_throw(
                                    stack,
                                    Some(context),
                                    &source,
                                )?;
                                return Err(CommittedValueError::JavaScript(
                                self.err_type(
                                    ("Iterator.prototype.flatMap mapper return must be iterable"
                                        .to_string())
                                    .into(),
                                ),
                            ));
                            }
                            IteratorState::User {
                                iterator: iter_value,
                                next_method: Some(next_method),
                            }
                        }
                    } else {
                        self.gc_heap.with_payload(*iter, |state| state.exhaust());
                        self.close_iterator_preserving_throw(stack, Some(context), &source)?;
                        return Err(CommittedValueError::JavaScript(
                            self.err_type(
                                ("Iterator.prototype.flatMap mapper return must be iterable"
                                    .to_string())
                                .into(),
                            ),
                        ));
                    };
                    let iter_root = Value::iterator(*iter);
                    let source_root = Value::iterator(source);
                    let mapper_root = self.gc_heap.read_payload(*iter, |state| match state {
                        IteratorState::FlatMap { mapper, .. } => *mapper,
                        _ => Value::undefined(),
                    });
                    let new_inner = self
                        .alloc_runtime_rooted_iterator_state(
                            inner_state,
                            &[&iter_root, &source_root, &mapper_root],
                            &[],
                        )
                        .map_err(CommittedValueError::JavaScript)?;
                    self.gc_heap.with_payload(*iter, |state| {
                        if let IteratorState::FlatMap { inner: slot, .. } = state {
                            *slot = Some(new_inner);
                        }
                    });
                    // A freshly allocated inner iterator is young; the helper
                    // holding it may already be old.
                    self.gc_heap.write_barrier(*iter, new_inner);
                    inner = Some(new_inner);
                }
            }
            IteratorStateSnapshot::Chunks {
                source,
                chunk_size,
                running,
            } => {
                // §27.5.3.2 GeneratorValidate step 6 — see Take above.
                if running {
                    return Err(CommittedValueError::JavaScript(self.err_type(
                        ("Iterator helper is already running".to_string()).into(),
                    )));
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
                        self.append_to_iterator_buffer(*iter, source, v)
                            .map_err(CommittedValueError::JavaScript)?;
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
                let fresh = self
                    .alloc_runtime_rooted_array_from_values(std::iter::empty(), &[&yielded], &[])
                    .map_err(CommittedValueError::JavaScript)?;
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
                    return Err(CommittedValueError::JavaScript(self.err_type(
                        ("Iterator helper is already running".to_string()).into(),
                    )));
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
                        self.append_to_iterator_buffer(*iter, source, v)
                            .map_err(CommittedValueError::JavaScript)?;
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
                        Some(buffer) => Some(
                            self.copy_iterator_buffer(buffer)
                                .map_err(CommittedValueError::JavaScript)?,
                        ),
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
                let window = self
                    .copy_iterator_buffer(buffer)
                    .map_err(CommittedValueError::JavaScript)?;
                Ok((window, false))
            }
            IteratorStateSnapshot::Concat {
                mut inner,
                mut index,
                running,
            } => {
                // §27.5.3.2 GeneratorValidate step 6 — see Take above.
                if running {
                    return Err(CommittedValueError::JavaScript(self.err_type(
                        ("Iterator helper is already running".to_string()).into(),
                    )));
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
                        let opened = self
                            .run_callable_sync_rooted(
                                stack,
                                Some(context),
                                &method,
                                iterable,
                                SmallVec::new(),
                            )
                            .map_err(CommittedValueError::completed_call)?;
                        // Built-in iterators are their own value family,
                        // so "is an Object" is the primitive test, not
                        // `as_object`.
                        if crate::abstract_ops::is_primitive(&opened) {
                            return Err(CommittedValueError::JavaScript(
                                self.err_type(
                                    ("Iterator.concat: iterator method did not return an object"
                                        .to_string())
                                    .into(),
                                ),
                            ));
                        }
                        // §7.4.4 GetIteratorDirect — `next` is cached
                        // once per opened iterator.
                        let key = VmPropertyKey::String("next");
                        let (opened, next_method) = self.with_handle_scope(
                            |interp, scope| -> Result<_, CommittedValueError> {
                                let opened = interp.scoped_value(scope, opened);
                                let outcome = interp.ordinary_get_value(
                                    stack,
                                    Some(context),
                                    interp.escape_scoped(opened),
                                    interp.escape_scoped(opened),
                                    &key,
                                    0,
                                )?;
                                let next_method = match outcome {
                                    VmGetOutcome::Value(value) => value,
                                    VmGetOutcome::InvokeGetter { getter } => interp
                                        .run_callable_sync_rooted(
                                            stack,
                                            Some(context),
                                            &getter,
                                            interp.escape_scoped(opened),
                                            SmallVec::new(),
                                        )
                                        .map_err(CommittedValueError::completed_call)?,
                                };
                                Ok::<_, CommittedValueError>((
                                    interp.escape_scoped(opened),
                                    next_method,
                                ))
                            },
                        )?;
                        let iter_root = Value::iterator(*iter);
                        let new_inner = self
                            .alloc_runtime_rooted_iterator_state(
                                IteratorState::User {
                                    iterator: opened,
                                    next_method: Some(next_method),
                                },
                                &[&iter_root, &opened, &next_method],
                                &[],
                            )
                            .map_err(CommittedValueError::JavaScript)?;
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
                    return Err(CommittedValueError::JavaScript(self.err_type(
                        ("Iterator helper is already running".to_string()).into(),
                    )));
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
                let finished = self
                    .iterator_zip_finish(*iter, keyed)
                    .map_err(CommittedValueError::JavaScript)?;
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
        context: Option<&ExecutionContext>,
        iter: IteratorHandle,
    ) -> Result<(), CommittedValueError> {
        let open = self.zip_open_inputs(iter);
        self.iterator_zip_close_all(stack, context, open, true)
    }

    /// IteratorCloseAll in reverse input order. A normal close retains the
    /// first catchable failure while later closes run. Throwing callers use
    /// the sole scoped completion extent; fatal/control and escaping OOM stop
    /// further JavaScript and leave with their current cause and provenance.
    fn iterator_zip_close_all(
        &mut self,
        stack: &mut ActivationStack,
        context: Option<&ExecutionContext>,
        open: Vec<Value>,
        throwing: bool,
    ) -> Result<(), CommittedValueError> {
        if throwing {
            return self.preserving_iterator_throw_completion(|interp| {
                interp.iterator_zip_close_all(stack, context, open, false)
            });
        }
        self.with_handle_scope(|interp, scope| {
            let open: Vec<_> = open
                .into_iter()
                .map(|value| interp.scoped_value(scope, value))
                .collect();
            let mut outcome = Ok(());
            for entry in open.into_iter().rev() {
                if interp.escape_scoped(entry).is_null()
                    || interp.escape_scoped(entry).is_undefined()
                {
                    continue;
                }
                if outcome.is_err() {
                    // Later catchable cleanup preserves the first throw and
                    // its exact provenance; fatal/control/OOM stops all calls.
                    interp.preserving_iterator_throw_completion(|interp| {
                        interp.iterator_close_value_sync(
                            stack,
                            context,
                            interp.escape_scoped(entry),
                        )
                    })?;
                } else {
                    outcome = interp.iterator_close_value_sync(
                        stack,
                        context,
                        interp.escape_scoped(entry),
                    );
                    if let Err(CommittedValueError::Fatal(error)) = &outcome {
                        return Err(CommittedValueError::Fatal(*error));
                    }
                }
            }
            outcome
        })
    }

    /// One `IteratorZip` round: step every still-open input once and
    /// collect the per-input results. `Ok(None)` means the join is over.
    fn iterator_zip_step(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        iter: IteratorHandle,
        mode: crate::iterator_state::ZipMode,
    ) -> Result<Option<()>, CommittedValueError> {
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
            array::set_length(results, &mut self.gc_heap, 0)
                .map_err(VmError::from)
                .map_err(CommittedValueError::JavaScript)?;
        }
        for index in 0..count {
            let Some((iters, padding, _)) = self.iterator_zip_arrays(iter) else {
                return Ok(None);
            };
            let entry = array::get(iters, &self.gc_heap, index);
            if entry.is_null() {
                // Already exhausted; only `"longest"` gets this far.
                let pad = array::get(padding, &self.gc_heap, index);
                self.zip_append(iter, ZipList::Results, pad)
                    .map_err(CommittedValueError::JavaScript)?;
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
                    if matches!(&err, CommittedValueError::Fatal(_)) {
                        return Err(err);
                    }
                    self.iterator_zip_close_all(
                        stack,
                        Some(context),
                        self.zip_open_inputs(iter),
                        true,
                    )?;
                    return Err(err);
                }
            };
            if !done {
                self.zip_append(iter, ZipList::Results, value)
                    .map_err(CommittedValueError::JavaScript)?;
                continue;
            }
            // This input is finished: drop it from the open set first so
            // the close-all below never re-enters it.
            self.zip_forget_input(iter, index);
            match mode {
                ZipMode::Shortest => {
                    self.iterator_zip_close_all(
                        stack,
                        Some(context),
                        self.zip_open_inputs(iter),
                        false,
                    )?;
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
                        self.iterator_zip_close_all(
                            stack,
                            Some(context),
                            self.zip_open_inputs(iter),
                            true,
                        )?;
                        return Err(CommittedValueError::JavaScript(error));
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
                                self.iterator_zip_close_all(
                                    stack,
                                    Some(context),
                                    self.zip_open_inputs(iter),
                                    true,
                                )?;
                                return Err(CommittedValueError::JavaScript(error));
                            }
                            Err(err) => {
                                self.zip_forget_input(iter, other);
                                if matches!(&err, CommittedValueError::Fatal(_)) {
                                    return Err(err);
                                }
                                self.iterator_zip_close_all(
                                    stack,
                                    Some(context),
                                    self.zip_open_inputs(iter),
                                    true,
                                )?;
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
                    self.zip_append(iter, ZipList::Results, pad)
                        .map_err(CommittedValueError::JavaScript)?;
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
        // Every define below can allocate (a shape transition, a wider slot
        // slab) and move the receiver, the keys and the values. All of them
        // live in the handle scope and are re-read for each property.
        self.with_handle_scope(|interp, scope| {
            let keys = keys
                .iter()
                .map(|key| interp.scoped_value(scope, *key))
                .collect::<Vec<_>>();
            let results = results
                .iter()
                .map(|value| interp.scoped_value(scope, *value))
                .collect::<Vec<_>>();
            let object = interp.alloc_runtime_rooted_object_with_roots(&[], &[])?;
            let object = interp.scoped_value(scope, Value::object(object));
            for (key, value) in keys.iter().zip(&results) {
                let key = interp.escape_scoped(*key);
                let value = interp.escape_scoped(*value);
                let mut target = interp
                    .escape_scoped(object)
                    .as_object()
                    .ok_or(VmError::TypeMismatch)?;
                let descriptor = crate::object::PropertyDescriptor::data(value, true, true, true);
                let heap = &mut interp.gc_heap;
                if let Some(text) = key.as_string(heap) {
                    let text = text.to_lossy_string(heap);
                    crate::object::define_own_property_in_place(
                        &mut target,
                        heap,
                        &text,
                        descriptor,
                    )?;
                } else if let Some(symbol) = key.as_symbol(heap) {
                    crate::object::define_own_symbol_property(target, heap, symbol, descriptor)?;
                }
            }
            Ok(interp.escape_scoped(object))
        })
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
    ) -> Result<(Value, Value), CommittedValueError> {
        let iterable_anchor = self.push_iteration_anchor(*iterable) - 1;
        let anchor_base = iterable_anchor;
        let result = (|interp: &mut Self| -> Result<(Value, Value), CommittedValueError> {
            let iterator_sym = interp.well_known_symbols.get(symbol::WellKnown::Iterator);
            let iterable = interp.iteration_anchor(iterable_anchor);
            let method = match interp.ordinary_get_value(
                stack,
                Some(context),
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
                    interp
                        .run_callable_sync_rooted(
                            stack,
                            Some(context),
                            &getter,
                            iterable,
                            SmallVec::new(),
                        )
                        .map_err(CommittedValueError::completed_call)?
                }
            };
            interp.get_iterator_from_method_sync(
                stack,
                context,
                &interp.iteration_anchor(iterable_anchor),
                &method,
            )
        })(self);
        self.pop_iteration_anchors_to(anchor_base);
        result
    }

    /// §7.4.3 GetIteratorFromMethod — the caller already performed
    /// `GetMethod(obj, @@iterator)`, which the spec runs exactly once.
    pub(crate) fn get_iterator_from_method_sync(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        iterable: &Value,
        method: &Value,
    ) -> Result<(Value, Value), CommittedValueError> {
        let iterable_anchor = self.push_iteration_anchor(*iterable) - 1;
        let anchor_base = iterable_anchor;
        let result = (|interp: &mut Self| -> Result<(Value, Value), CommittedValueError> {
            let method = *method;
            if method.is_undefined() || method.is_null() || !interp.is_callable_runtime(&method) {
                return Err(CommittedValueError::JavaScript(
                    interp.err_type(("iterator method is not callable".to_string()).into()),
                ));
            }
            let method_anchor = interp.push_iteration_anchor(method) - 1;
            let method = interp.iteration_anchor(method_anchor);
            let iterable = interp.iteration_anchor(iterable_anchor);
            let iterator = interp
                .run_callable_sync_rooted(stack, Some(context), &method, iterable, SmallVec::new())
                .map_err(CommittedValueError::completed_call)?;
            if !(iterator.is_object()
                || iterator.is_proxy()
                || iterator.is_array()
                || iterator.is_iterator()
                || iterator.is_map()
                || iterator.is_set()
                || iterator.is_generator())
            {
                return Err(CommittedValueError::JavaScript(interp.err_type(
                    ("iterator method did not return an object".to_string()).into(),
                )));
            }
            let iterator_anchor = interp.push_iteration_anchor(iterator) - 1;
            let iterator = interp.iteration_anchor(iterator_anchor);
            let next_method = match interp.ordinary_get_value(
                stack,
                Some(context),
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
                    interp
                        .run_callable_sync_rooted(
                            stack,
                            Some(context),
                            &getter,
                            iterator,
                            SmallVec::new(),
                        )
                        .map_err(CommittedValueError::completed_call)?
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
    ) -> Result<Option<Value>, CommittedValueError> {
        let result = self
            .run_callable_sync_rooted(
                stack,
                Some(context),
                next_method,
                *iterator,
                SmallVec::new(),
            )
            .map_err(CommittedValueError::completed_call)?;
        if !result.is_object() && !result.is_proxy() {
            return Err(CommittedValueError::JavaScript(
                self.err_type(("iterator result is not an object".to_string()).into()),
            ));
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

    /// IteratorClose while preserving an incoming abrupt completion.
    /// Catchable cleanup failures are suppressed according to the caller's
    /// throw-completion domain. Structural/control or actual allocation
    /// failure escapes with its own provenance. The incoming exception and
    /// iterator are scoped roots throughout observable Get/Call.
    pub(crate) fn iterator_close_discarding_completion(
        &mut self,
        stack: &mut ActivationStack,
        context: Option<&ExecutionContext>,
        iterator: &Value,
    ) -> Result<(), CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            let iterator = interp.scoped_value(scope, *iterator);
            interp.preserving_iterator_throw_completion(|interp| {
                interp.iterator_close_value_sync(stack, context, interp.escape_scoped(iterator))
            })
        })
    }

    /// §7.4.8 IteratorClose — invoke `return` if present.
    ///
    /// The `completion` semantics are caller-owned: pass `Ok(())` to
    /// run the close because the surrounding loop finished
    /// successfully; on an abrupt completion the caller should
    /// use `iterator_close_discarding_completion`: catchable cleanup is
    /// suppressed while a completed terminal failure propagates with its own provenance.
    /// Local allocation/validation errors keep their source disposition.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-iteratorclose>
    pub(crate) fn iterator_close_sync(
        &mut self,
        stack: &mut ActivationStack,
        context: Option<&ExecutionContext>,
        iterator: &Value,
    ) -> Result<(), CommittedValueError> {
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
                    let value = interp
                        .run_callable_sync_rooted(
                            stack,
                            context,
                            &interp.escape_scoped(getter),
                            interp.escape_scoped(iterator),
                            SmallVec::new(),
                        )
                        .map_err(CommittedValueError::completed_call)?;
                    interp.scoped_value(scope, value)
                }
            };
            let return_value = interp.escape_scoped(return_method);
            if return_value.is_undefined() || return_value.is_null() {
                return Ok(());
            }
            if !interp.is_callable_runtime(&return_value) {
                return Err(CommittedValueError::JavaScript(interp.err_type(
                    ("iterator `return` is not callable".to_string()).into(),
                )));
            }
            let result = interp
                .run_callable_sync_rooted(
                    stack,
                    context,
                    &interp.escape_scoped(return_method),
                    interp.escape_scoped(iterator),
                    SmallVec::new(),
                )
                .map_err(CommittedValueError::completed_call)?;
            if !result.is_object() && !result.is_proxy() {
                return Err(CommittedValueError::JavaScript(interp.err_type(
                    ("iterator `return` did not yield an object".to_string()).into(),
                )));
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
    ) -> Result<Option<Value>, CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            let iterator = interp.scoped_value(scope, iterator);
            let current = interp.escape_scoped(iterator);
            let return_method = match interp.ordinary_get_value(
                stack,
                Some(context),
                current,
                current,
                &VmPropertyKey::String("return"),
                0,
            )? {
                VmGetOutcome::Value(value) => interp.scoped_value(scope, value),
                VmGetOutcome::InvokeGetter { getter } => {
                    let getter = interp.scoped_value(scope, getter);
                    let value = interp
                        .run_callable_sync_rooted(
                            stack,
                            Some(context),
                            &interp.escape_scoped(getter),
                            interp.escape_scoped(iterator),
                            SmallVec::new(),
                        )
                        .map_err(CommittedValueError::completed_call)?;
                    interp.scoped_value(scope, value)
                }
            };
            let return_value = interp.escape_scoped(return_method);
            if return_value.is_undefined() || return_value.is_null() {
                return Ok(None);
            }
            if !interp.is_callable_runtime(&return_value) {
                return Err(CommittedValueError::JavaScript(interp.err_type(
                    ("iterator `return` is not callable".to_string()).into(),
                )));
            }
            let result = interp
                .run_callable_sync_rooted(
                    stack,
                    Some(context),
                    &interp.escape_scoped(return_method),
                    interp.escape_scoped(iterator),
                    SmallVec::new(),
                )
                .map_err(CommittedValueError::completed_call)?;
            Ok(Some(result))
        })
    }

    /// Set an iterator record's `[[Done]]`: a later close is a no-op.
    pub(crate) fn iterator_mark_done(&mut self, iterator: Value) {
        if let Some(handle) = iterator.as_iterator() {
            self.gc_heap.with_payload(handle, |state| state.exhaust());
        }
    }

    /// §7.4.11 IteratorClose(iterator, normal completion).
    pub(crate) fn iterator_close_value_sync(
        &mut self,
        stack: &mut ActivationStack,
        context: Option<&ExecutionContext>,
        iterator: Value,
    ) -> Result<(), CommittedValueError> {
        self.iterator_close_record(stack, context, iterator, false)
    }

    /// `Op::IteratorClose` / `Op::IteratorCloseThrow` for the activation at
    /// `frame_index`: a throw completion keeps its own value over the
    /// close's catchable failures.
    pub(crate) fn iterator_close_op(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        iterator: Value,
        throw: bool,
    ) -> Result<(), CommittedValueError> {
        let primordial = self.frame_iterates_primordially(context, stack, frame_index);
        if !throw {
            return self.iterator_close_record(stack, Some(context), iterator, primordial);
        }
        self.with_handle_scope(|interp, scope| {
            let iterator = interp.scoped_value(scope, iterator);
            interp.preserving_iterator_throw_completion(|interp| {
                interp.iterator_close_record(
                    stack,
                    Some(context),
                    interp.escape_scoped(iterator),
                    primordial,
                )
            })
        })
    }

    /// IteratorClose of one record. A built-in iterator whose `return` is
    /// not proven built-in runs the observable `GetMethod(iterator,
    /// "return")` protocol, unless runtime-internal (`primordial`) code
    /// closes it.
    fn iterator_close_record(
        &mut self,
        stack: &mut ActivationStack,
        context: Option<&ExecutionContext>,
        iterator: Value,
        primordial: bool,
    ) -> Result<(), CommittedValueError> {
        enum Record {
            Builtin,
            Generator(crate::generator::JsGenerator),
            Other,
        }
        let record = match iterator.as_iterator() {
            Some(handle) if !primordial => self.gc_heap.read_payload(handle, |state| match state {
                IteratorState::User { .. } | IteratorState::Exhausted { .. } => Record::Other,
                IteratorState::Generator { handle } => Record::Generator(*handle),
                _ => Record::Builtin,
            }),
            _ => Record::Other,
        };
        self.with_handle_scope(|interp, scope| {
            let root = interp.scoped_value(scope, iterator);
            match record {
                Record::Builtin if !interp.builtin_return_proven(iterator) => {
                    let iterator = interp.escape_scoped(root);
                    interp.iterator_close_sync(stack, context, &iterator)
                }
                Record::Generator(handle) => {
                    let generator = interp.scoped_value(scope, Value::generator(handle));
                    if interp.proven_generator_return(Value::generator(handle)) {
                        return interp.close_iterator_state(stack, context, interp.escape_scoped(root));
                    }
                    // The record is done; the generator's own `return` runs.
                    interp.iterator_mark_done(interp.escape_scoped(root));
                    let generator = interp.escape_scoped(generator);
                    interp.iterator_close_sync(stack, context, &generator)
                }
                _ => interp.close_iterator_state(stack, context, interp.escape_scoped(root)),
            }
        })
    }

    /// The close a built-in iterator's own `return` performs: no
    /// protocol reads, only the state's semantics.
    pub(crate) fn close_iterator_state(
        &mut self,
        stack: &mut ActivationStack,
        context: Option<&ExecutionContext>,
        iterator: Value,
    ) -> Result<(), CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            let iterator = interp.scoped_value(scope, iterator);
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
            let action = if let Some(handle) = interp.escape_scoped(iterator).as_iterator() {
                let action = interp.gc_heap.read_payload(handle, |state| match state {
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
                });
                // A closing record is done: a close it triggers again, or a
                // throw out of its `return`, finds nothing left to close.
                if matches!(action, CloseAction::User(_) | CloseAction::Generator(_)) {
                    interp.gc_heap.with_payload(handle, |state| state.exhaust());
                }
                action
            } else {
                CloseAction::User(interp.escape_scoped(iterator))
            };
            match action {
                CloseAction::User(close_target) => {
                    interp.iterator_close_sync(stack, context, &close_target)?;
                }
                CloseAction::Generator(handle) => {
                    let (done, function_id) = handle.with_body(&interp.gc_heap, |body| {
                        (
                            body.done,
                            body.frame.as_ref().map(|frame| frame.header.function_id),
                        )
                    });
                    if !done {
                        let owner = match function_id {
                            Some(function_id) => interp
                                .function_context(context, function_id)
                                .map_err(CommittedValueError::Fatal)?,
                            None => context
                                .cloned()
                                .ok_or(VmError::InvalidOperand)
                                .map_err(CommittedValueError::Fatal)?,
                        };
                        interp.resume_generator(
                            stack,
                            Some(&owner),
                            &handle,
                            GeneratorResumeKind::Return(Value::undefined()),
                        )?;
                    }
                }
                CloseAction::Helper { source, inner } => {
                    // Mark the wrapper exhausted FIRST so a re-entrant or
                    // repeated close does not forward twice.
                    if let Some(handle) = interp.escape_scoped(iterator).as_iterator() {
                        interp.gc_heap.with_payload(handle, |state| state.exhaust());
                    }
                    let source = interp.scoped_value(scope, Value::iterator(source));
                    let inner =
                        inner.map(|inner| interp.scoped_value(scope, Value::iterator(inner)));
                    let inner_result = match inner {
                        Some(inner) => interp.iterator_close_value_sync(
                            stack,
                            context,
                            interp.escape_scoped(inner),
                        ),
                        None => Ok(()),
                    };
                    if let Err(CommittedValueError::Fatal(error)) = &inner_result {
                        return Err(CommittedValueError::Fatal(*error));
                    }
                    let source_result = if inner_result.is_err() {
                        interp.preserving_iterator_throw_completion(|interp| {
                            interp.iterator_close_value_sync(
                                stack,
                                context,
                                interp.escape_scoped(source),
                            )
                        })
                    } else {
                        interp.iterator_close_value_sync(
                            stack,
                            context,
                            interp.escape_scoped(source),
                        )
                    };
                    source_result?;
                    inner_result?;
                }
                CloseAction::ConcatInner(inner) => {
                    // Mark running (rather than exhausted) so a `return`
                    // that re-enters this same helper is rejected instead of
                    // silently becoming a no-op.
                    if let Some(handle) = interp.escape_scoped(iterator).as_iterator() {
                        interp.gc_heap.with_payload(handle, |state| {
                            if let IteratorState::Concat { running, .. } = state {
                                *running = true;
                            }
                        });
                    }
                    let inner_result = match inner {
                        Some(inner) => {
                            interp.iterator_close_value_sync(stack, context, Value::iterator(inner))
                        }
                        None => Ok(()),
                    };
                    if let Some(handle) = interp.escape_scoped(iterator).as_iterator() {
                        interp.gc_heap.with_payload(handle, |state| state.exhaust());
                    }
                    inner_result?;
                }
                CloseAction::ZipAll { started } => {
                    // §27.1.2.1.2 step 4 — from suspended-start the helper
                    // is completed *before* the inputs are closed, so a
                    // re-entrant close returns normally. From suspended-yield
                    // it is marked running, so a re-entrant close throws.
                    let handle = interp.escape_scoped(iterator).as_iterator();
                    let Some(handle_for_close) = handle else {
                        return Ok(());
                    };
                    // Snapshot before the state is folded away below.
                    let open = interp.zip_open_inputs(handle_for_close);
                    if let Some(handle) = handle {
                        if started {
                            interp.gc_heap.with_payload(handle, |state| {
                                if let IteratorState::Zip { running, .. } = state {
                                    *running = true;
                                }
                            });
                        } else {
                            interp.gc_heap.with_payload(handle, |state| state.exhaust());
                        }
                    }
                    let closed = interp.iterator_zip_close_all(stack, context, open, false);
                    if let Some(handle) = interp.escape_scoped(iterator).as_iterator() {
                        interp.gc_heap.with_payload(handle, |state| state.exhaust());
                    }
                    closed?;
                }
                CloseAction::HelperRunning => {
                    return Err(CommittedValueError::JavaScript(interp.err_type(
                        ("Iterator helper is already running".to_string()).into(),
                    )));
                }
                CloseAction::Builtin | CloseAction::None => {}
            }
            Ok(())
        })
    }

    /// Close a built-in iterator record for an incoming throw through the
    /// same scoped completion owner as ordinary iterators.
    pub(crate) fn close_iterator_preserving_throw(
        &mut self,
        stack: &mut ActivationStack,
        context: Option<&ExecutionContext>,
        handle: &IteratorHandle,
    ) -> Result<(), CommittedValueError> {
        self.iterator_close_discarding_completion(stack, context, &Value::iterator(*handle))
    }

    /// `Op::SpreadAppend` — §13.2.4.1 ArrayAccumulation for one
    /// SpreadElement: append IteratorToList(GetIterator(iterable)) to the
    /// array in `array_reg`. A spread never closes its iterator.
    pub(crate) fn spread_append(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        array_reg: u16,
        iterable_reg: u16,
    ) -> Result<(), CommittedValueError> {
        let iterable = *read_register(&stack[frame_index], iterable_reg)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let operands = |stack: &ActivationStack| -> Result<(Value, Value), CommittedValueError> {
            let frame = &stack[frame_index];
            Ok((
                *read_register(frame, array_reg)
                    .map_err(|error| CommittedValueError::Fatal(error.into()))?,
                *read_register(frame, iterable_reg)
                    .map_err(|error| CommittedValueError::Fatal(error.into()))?,
            ))
        };
        // A proven plain dense array copies straight across. Proving may
        // move both operands; their registers are roots.
        if iterable.is_array() && self.intrinsic_iterable(context, stack, iterable) {
            let (array, source) = operands(stack)?;
            let (Some(array), Some(source)) = (array.as_array(), source.as_array()) else {
                return Err(CommittedValueError::Fatal(VmError::InvalidOperand));
            };
            let roots = self.collect_allocation_roots(stack);
            let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                for &slot in &roots {
                    visitor(slot);
                }
            };
            if array::append_plain_dense(array, source, &mut self.gc_heap, &mut external_visit)
                .map_err(|error| CommittedValueError::JavaScript(VmError::from(error)))?
            {
                return Ok(());
            }
        }
        let (_, iterable) = operands(stack)?;
        let values = self.iterator_to_list_sync(context, stack, &iterable)?;
        let array = read_register(&stack[frame_index], array_reg)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?
            .as_array()
            .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
        let roots = self.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
        };
        array::extend_with_roots(array, &mut self.gc_heap, &values, &mut external_visit)
            .map_err(|error| CommittedValueError::JavaScript(VmError::from(error)))?;
        Ok(())
    }

    /// §7.4.13 IteratorToList(GetIterator(iterable, sync)).
    ///
    /// A built-in iterable or iterator whose protocol is proven unmodified
    /// (see [`crate::iteration_protocol`]), or a built-in iterable that
    /// runtime-internal code passed, is read directly; a generator and a
    /// built-in iterator stepped by its built-in `next` run their states.
    /// Everything else runs `GetIterator` + `IteratorStepValue`. Values
    /// collected across steps that can run code are parked on the anchor
    /// stack. As the spec's IteratorToList, a throwing step propagates
    /// without closing the iterator.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-iteratortolist>
    pub(crate) fn iterator_to_list_sync(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        iterable: &Value,
    ) -> Result<Vec<Value>, CommittedValueError> {
        let (iterable, proven, self_iterator) = self.with_handle_scope(|interp, scope| {
            let root = interp.scoped_value(scope, *iterable);
            let proven = interp.intrinsic_iterable(context, stack, *iterable);
            let self_iterator = !proven && interp.proven_self_iterator(interp.escape_scoped(root));
            (interp.escape_scoped(root), proven, self_iterator)
        });
        if proven {
            if let Some(arr) = iterable.as_array() {
                // Bulk reads are exact only for a plain dense array: holes
                // resolve through the prototype and accessors run, so
                // anything else steps the built-in iterator below.
                let len = array::len(arr, &self.gc_heap);
                if let Some(values) = array::plain_dense_prefix_values(arr, &self.gc_heap, len) {
                    return Ok(values);
                }
            }
            if let Some(s) = iterable.as_string(&self.gc_heap) {
                return string_iterator_values(s, &mut self.gc_heap)
                    .map_err(CommittedValueError::JavaScript);
            }
            if let Some(s) = iterable.as_set() {
                return Ok(crate::collections::set_values(s, &self.gc_heap));
            }
            if let Some(m) = iterable.as_map() {
                // Every pending key and value stays traced while the entry
                // arrays allocate.
                let pending: Vec<Value> = crate::collections::map_entries(m, &self.gc_heap)
                    .into_iter()
                    .flat_map(|(key, value)| [key, value])
                    .collect();
                let mut out = Vec::with_capacity(pending.len() / 2);
                for index in 0..pending.len() / 2 {
                    let (key, value) = (pending[2 * index], pending[2 * index + 1]);
                    let entry = self
                        .alloc_runtime_rooted_array_from_values(
                            [key, value],
                            &[&iterable],
                            &[out.as_slice(), &pending[2 * index..]],
                        )
                        .map_err(CommittedValueError::JavaScript)?;
                    out.push(Value::array(entry));
                }
                return Ok(out);
            }
        }
        let base = self.push_iteration_anchor(iterable) - 1;
        let result = self
            .drain_iterable(context, stack, base, proven, self_iterator)
            .map(|first| self.iteration_anchors[first..].to_vec());
        self.pop_iteration_anchors_to(base);
        result
    }

    /// Step the iterable anchored at `base` to exhaustion. The record it
    /// steps is anchored right above it and every value above that; returns
    /// the anchor index of the first value. Each step re-reads the record
    /// from its anchor, since a step can run code and move it.
    fn drain_iterable(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        base: usize,
        proven: bool,
        self_iterator: bool,
    ) -> Result<usize, CommittedValueError> {
        let iterable = self.iteration_anchor(base);
        if iterable.is_generator()
            && (self.caller_iterates_primordially(context, stack)
                || self.proven_generator_iterable(iterable))
        {
            let first = base + 1;
            loop {
                let handle = self
                    .iteration_anchor(base)
                    .as_generator()
                    .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
                let result = self.resume_generator(
                    stack,
                    Some(context),
                    &handle,
                    GeneratorResumeKind::Next(Value::undefined()),
                )?;
                let Some(record) = result.as_object() else {
                    return Err(CommittedValueError::JavaScript(self.err_type(
                        ("generator next did not return an object".to_string()).into(),
                    )));
                };
                let (value, done) = self.iterator_result_parts(record);
                if done {
                    return Ok(first);
                }
                self.push_iteration_anchor(value);
            }
        }
        // Proving the generator above may have moved the iterable.
        let iterable = self.iteration_anchor(base);
        // A built-in state whose stepping is the built-in `next` runs
        // directly: a proven iterable's fresh state, a proven iterator itself,
        // or the record GetIterator produced with the built-in `next`.
        let builtin = if proven {
            let state = crate::iteration_protocol::proven_iterator_state(iterable, &self.gc_heap)
                .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
            let handle = self
                .alloc_stack_rooted_iterator_state(stack, state, &[&iterable], &[])
                .map_err(CommittedValueError::JavaScript)?;
            self.push_iteration_anchor(Value::iterator(handle));
            true
        } else if self_iterator {
            self.push_iteration_anchor(iterable);
            true
        } else {
            let (iterator, next_method) = self.get_iterator_sync(stack, context, &iterable)?;
            let builtin = self.is_builtin_next(iterator, next_method);
            self.push_iteration_anchor(iterator);
            if !builtin {
                self.push_iteration_anchor(next_method);
            }
            builtin
        };
        let record = base + 1;
        if builtin {
            let first = record + 1;
            loop {
                let handle = self
                    .iteration_anchor(record)
                    .as_iterator()
                    .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
                let (value, done) = self.iterator_next_full(context, stack, &handle)?;
                if done {
                    return Ok(first);
                }
                self.push_iteration_anchor(value);
            }
        }
        let first = record + 2;
        loop {
            let iterator = self.iteration_anchor(record);
            let next_method = self.iteration_anchor(record + 1);
            match self.iterator_step_sync(stack, context, &iterator, &next_method)? {
                Some(value) => {
                    self.push_iteration_anchor(value);
                }
                None => return Ok(first),
            }
        }
    }

    /// §27.6.3.8 AsyncGeneratorYield step 5 — `Set value to ? Await(value)`.
    ///
    /// A yielded thenable is adopted before the request settles, so
    /// `yield somePromise` hands the consumer what it resolves to; a plain
    /// value still costs the one tick the await takes. A rejection is
    /// delivered back into the body as a throw at the `yield`, which is
    /// where the `?` in that step leads.
    ///
    /// # Errors
    /// Returns the failure behind allocating the promise or its reactions.
    pub(crate) fn async_generator_yield_awaited(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        owner: &crate::generator::JsGenerator,
        value: Value,
    ) -> Result<(), CommittedValueError> {
        self.with_handle_scope(|vm, scope| {
            let owner_root = vm.scoped_value(scope, Value::generator(*owner));
            // Values carried across the two handler allocations are parked in
            // the handle arena and re-read afterwards — a stack-local `Value`
            // "updated in place" through a shared reference is not a root the
            // optimizer has to honor.
            let inner_value = vm.promise_resolve_value(stack, Some(context), value)?;
            let inner_root = vm.scoped_value(scope, inner_value);

            let owner_value = vm.escape_scoped(owner_root);
            let on_fulfilled =
                crate::native_function::native_value_with_captures_unchecked_with_roots(
                    &mut vm.gc_heap,
                    "AsyncGeneratorYield",
                    smallvec::smallvec![owner_value],
                    &mut |_visitor| {},
                    move |ctx, args, captures| {
                        let context = ctx.execution_context().cloned().ok_or_else(|| {
                            crate::native_function::vm_to_native_error(
                                ctx.interp_mut(),
                                VmError::InvalidOperand,
                                "AsyncGeneratorYield",
                            )
                        })?;
                        ctx.scope(|mut scope| {
                            let owner = captures
                                .first()
                                .copied()
                                .ok_or(crate::NativeError::InvalidOperand)?;
                            let owner = scope.value(owner);
                            let value =
                                scope.value(args.first().copied().unwrap_or_else(Value::undefined));
                            let current_owner = scope
                                .raw(owner)
                                .as_generator()
                                .ok_or(crate::NativeError::InvalidOperand)?;
                            let current_value = scope.raw(value);
                            scope.with_turn_parts(|interp, _| {
                                interp
                                    .async_generator_complete_step(
                                        Some(&context),
                                        &current_owner,
                                        Ok(current_value),
                                        false,
                                    )
                                    .map_err(|error| {
                                        crate::native_function::vm_to_native_error(
                                            interp,
                                            error,
                                            "AsyncGeneratorYield",
                                        )
                                    })
                            })?;
                            let current_owner = scope
                                .raw(owner)
                                .as_generator()
                                .ok_or(crate::NativeError::InvalidOperand)?;
                            scope.with_turn_parts(|interp, stack| {
                                interp
                                    .async_generator_resume_next(
                                        stack,
                                        Some(&context),
                                        &current_owner,
                                    )
                                    .map_err(|error| {
                                        error.into_native(interp, "AsyncGeneratorYield")
                                    })
                            })?;
                            Ok(Value::undefined())
                        })
                    },
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            let on_fulfilled = vm.stamp_native_creation_realm(on_fulfilled);
            let fulfilled_root = vm.scoped_value(scope, on_fulfilled);
            let owner_value = vm.escape_scoped(owner_root);
            let on_rejected =
                crate::native_function::native_value_with_captures_unchecked_with_roots(
                    &mut vm.gc_heap,
                    "AsyncGeneratorYieldRejected",
                    smallvec::smallvec![owner_value],
                    &mut |_visitor| {},
                    move |ctx, args, captures| {
                        let context = ctx.execution_context().cloned().ok_or_else(|| {
                            crate::native_function::vm_to_native_error(
                                ctx.interp_mut(),
                                VmError::InvalidOperand,
                                "AsyncGeneratorYieldRejected",
                            )
                        })?;
                        ctx.scope(|mut scope| {
                            let owner = captures
                                .first()
                                .copied()
                                .ok_or(crate::NativeError::InvalidOperand)?;
                            let owner = scope.value(owner);
                            let value =
                                scope.value(args.first().copied().unwrap_or_else(Value::undefined));
                            let current_owner = scope
                                .raw(owner)
                                .as_generator()
                                .ok_or(crate::NativeError::InvalidOperand)?;
                            let current_value = scope.raw(value);
                            let reason =
                                scope.with_turn_parts(|interp, stack| {
                                    match interp.resume_generator(
                                        stack,
                                        Some(&context),
                                        &current_owner,
                                        crate::GeneratorResumeKind::Throw(current_value),
                                    ) {
                                        Ok(_) => Ok(None),
                                        Err(error @ CommittedValueError::Fatal(_)) => Err(error
                                            .into_native(interp, "AsyncGeneratorYieldRejected")),
                                        Err(CommittedValueError::JavaScript(error)) => interp
                                            .vm_error_to_throwable_with_stack_roots(
                                                Some(&context),
                                                stack,
                                                &error,
                                            )
                                            .map(Some)
                                            .map_err(|error| {
                                                crate::native_function::vm_to_native_error(
                                                    interp,
                                                    error,
                                                    "AsyncGeneratorYieldRejected",
                                                )
                                            }),
                                    }
                                })?;
                            if let Some(reason) = reason {
                                let reason = scope.value(reason);
                                let current_owner = scope
                                    .raw(owner)
                                    .as_generator()
                                    .ok_or(crate::NativeError::InvalidOperand)?;
                                let reason = scope.raw(reason);
                                scope.with_turn_parts(|interp, _| {
                                    interp
                                        .async_generator_complete_step(
                                            Some(&context),
                                            &current_owner,
                                            Err(reason),
                                            true,
                                        )
                                        .map_err(|error| {
                                            crate::native_function::vm_to_native_error(
                                                interp,
                                                error,
                                                "AsyncGeneratorYieldRejected",
                                            )
                                        })
                                })?;
                                let current_owner = scope
                                    .raw(owner)
                                    .as_generator()
                                    .ok_or(crate::NativeError::InvalidOperand)?;
                                scope.with_turn_parts(|interp, stack| {
                                    interp
                                        .async_generator_drain_done(
                                            stack,
                                            Some(&context),
                                            &current_owner,
                                        )
                                        .map_err(|error| {
                                            error.into_native(interp, "AsyncGeneratorYieldRejected")
                                        })
                                })?;
                            }
                            Ok(Value::undefined())
                        })
                    },
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            let on_rejected = vm.stamp_native_creation_realm(on_rejected);
            let rejected_root = vm.scoped_value(scope, on_rejected);
            let capability =
                crate::promise_dispatch::PromiseBuilder::with_context(Some(context.clone()))
                    .capability_stack_rooted(vm, stack, &[], &[])
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            let inner = vm
                .escape_scoped(inner_root)
                .as_promise()
                .ok_or(VmError::InvalidOperand)
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            let on_fulfilled = vm.escape_scoped(fulfilled_root);
            let on_rejected = vm.escape_scoped(rejected_root);
            let outcome = vm
                .register_promise_reactions(
                    inner,
                    Some(on_fulfilled),
                    Some(on_rejected),
                    capability,
                    Some(context.clone()),
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            if let Some(job) = outcome.immediate_job {
                vm.microtasks.enqueue(job);
            }
            Ok(())
        })
    }

    /// §27.6.3.5.2 AsyncGeneratorResumeNext — drive the front queued
    /// request when the generator is not executing or awaiting. `next` and
    /// `throw` resume the suspended body directly (a `throw` at
    /// suspended-start closes without resuming, and a done body answers
    /// from the queue); `return` routes through
    /// [`Self::async_generator_await_return`] so its value is awaited
    /// before anything resumes (§27.6.3.5.1).
    pub(crate) fn async_generator_resume_next(
        &mut self,
        stack: &mut ActivationStack,
        context: Option<&ExecutionContext>,
        handle: &crate::generator::JsGenerator,
    ) -> Result<(), CommittedValueError> {
        loop {
            let state = handle.async_state(&self.gc_heap);
            if matches!(
                state,
                AsyncGeneratorState::Executing
                    | AsyncGeneratorState::Awaiting
                    | AsyncGeneratorState::AwaitingReturn
            ) {
                return Ok(());
            }
            let Some(resume) = handle.front_async_resume(&self.gc_heap) else {
                return Ok(());
            };
            let done = handle.is_done(&self.gc_heap) || state == AsyncGeneratorState::Completed;
            match resume {
                crate::GeneratorResumeKind::Next(value) => {
                    if done {
                        self.async_generator_complete_step(
                            context,
                            handle,
                            Ok(Value::undefined()),
                            true,
                        )
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                        continue;
                    }
                    self.resume_generator(
                        stack,
                        context,
                        handle,
                        crate::GeneratorResumeKind::Next(value),
                    )?;
                    return Ok(());
                }
                crate::GeneratorResumeKind::Throw(reason) => {
                    if done || state == AsyncGeneratorState::SuspendedStart {
                        // §27.6.3.5.2 — a throw completion delivered while
                        // the body never ran closes the generator.
                        handle.mark_done(&mut self.gc_heap);
                        handle.set_async_state(&mut self.gc_heap, AsyncGeneratorState::Completed);
                        self.async_generator_complete_step(context, handle, Err(reason), true)
                            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                        continue;
                    }
                    self.resume_generator(
                        stack,
                        context,
                        handle,
                        crate::GeneratorResumeKind::Throw(reason),
                    )?;
                    return Ok(());
                }
                crate::GeneratorResumeKind::Return(value) => {
                    return self.async_generator_await_return(stack, context, handle, value);
                }
            }
        }
    }

    /// §27.6.3.5.1 AsyncGeneratorAwaitReturn — await the front `return`
    /// request's value before delivering it. A fulfilled await resumes a
    /// suspended-yield body with a return completion (or settles the
    /// request when the body never ran / already finished); a rejection —
    /// including an abrupt `PromiseResolve` on a poisoned thenable —
    /// delivers a throw completion the same way.
    pub(crate) fn async_generator_await_return(
        &mut self,
        stack: &mut ActivationStack,
        context: Option<&ExecutionContext>,
        handle: &crate::generator::JsGenerator,
        value: Value,
    ) -> Result<(), CommittedValueError> {
        self.with_handle_scope(|vm, scope| {
            let owner_root = vm.scoped_value(scope, Value::generator(*handle));
            // A body parked at a yield receives the awaited completion as its
            // resumption; a body that never ran (suspended-start) or already
            // finished settles the request without resuming.
            let resume_body =
                handle.async_state(&vm.gc_heap) == AsyncGeneratorState::SuspendedYield;
            handle.set_async_state(&mut vm.gc_heap, AsyncGeneratorState::AwaitingReturn);
            let inner_value = match vm.promise_resolve_value(stack, context, value) {
                Ok(inner) => inner,
                Err(CommittedValueError::Fatal(error)) => {
                    return Err(CommittedValueError::Fatal(error));
                }
                Err(CommittedValueError::JavaScript(error)) => {
                    let reason = vm
                        .vm_error_to_throwable_with_stack_roots(context, stack, &error)
                        .map_err(CommittedValueError::Fatal)?;
                    return vm.async_generator_deliver_return_completion(
                        stack,
                        context,
                        &vm.escape_scoped(owner_root)
                            .as_generator()
                            .ok_or(VmError::InvalidOperand)
                            .map_err(CommittedValueError::Fatal)?,
                        resume_body,
                        Err(reason),
                    );
                }
            };
            let resume_body_value = Value::boolean(resume_body);
            // Values carried across the two handler allocations are parked in
            // the handle arena and re-read afterwards — a stack-local `Value`
            // "updated in place" through a shared reference is not a root the
            // optimizer has to honor.
            let inner_root = vm.scoped_value(scope, inner_value);
            let owner_value = vm.escape_scoped(owner_root);
            let on_fulfilled =
                crate::native_function::native_value_with_captures_unchecked_with_roots(
                    &mut vm.gc_heap,
                    "AsyncGeneratorAwaitReturnFulfilled",
                    smallvec::smallvec![owner_value, resume_body_value],
                    &mut |_visitor| {},
                    move |ctx, args, captures| {
                        let context = ctx.execution_context().cloned();
                        ctx.scope(|mut scope| {
                            let owner = captures
                                .first()
                                .copied()
                                .ok_or(crate::NativeError::InvalidOperand)?;
                            let owner = scope.value(owner);
                            let value =
                                scope.value(args.first().copied().unwrap_or_else(Value::undefined));
                            let resume_body =
                                captures.get(1).and_then(|value| value.as_boolean()) == Some(true);
                            let current_owner = scope
                                .raw(owner)
                                .as_generator()
                                .ok_or(crate::NativeError::InvalidOperand)?;
                            let value = scope.raw(value);
                            scope.with_turn_parts(|interp, stack| {
                                interp
                                    .async_generator_deliver_return_completion(
                                        stack,
                                        context.as_ref(),
                                        &current_owner,
                                        resume_body,
                                        Ok(value),
                                    )
                                    .map_err(|error| {
                                        error.into_native(
                                            interp,
                                            "AsyncGeneratorAwaitReturnFulfilled",
                                        )
                                    })
                            })?;
                            Ok(Value::undefined())
                        })
                    },
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            let on_fulfilled = vm.stamp_native_creation_realm(on_fulfilled);
            let fulfilled_root = vm.scoped_value(scope, on_fulfilled);
            let owner_value = vm.escape_scoped(owner_root);
            let on_rejected =
                crate::native_function::native_value_with_captures_unchecked_with_roots(
                    &mut vm.gc_heap,
                    "AsyncGeneratorAwaitReturnRejected",
                    smallvec::smallvec![owner_value, resume_body_value],
                    &mut |_visitor| {},
                    move |ctx, args, captures| {
                        let context = ctx.execution_context().cloned();
                        ctx.scope(|mut scope| {
                            let owner = captures
                                .first()
                                .copied()
                                .ok_or(crate::NativeError::InvalidOperand)?;
                            let owner = scope.value(owner);
                            let value =
                                scope.value(args.first().copied().unwrap_or_else(Value::undefined));
                            let resume_body =
                                captures.get(1).and_then(|value| value.as_boolean()) == Some(true);
                            let current_owner = scope
                                .raw(owner)
                                .as_generator()
                                .ok_or(crate::NativeError::InvalidOperand)?;
                            let value = scope.raw(value);
                            scope.with_turn_parts(|interp, stack| {
                                interp
                                    .async_generator_deliver_return_completion(
                                        stack,
                                        context.as_ref(),
                                        &current_owner,
                                        resume_body,
                                        Err(value),
                                    )
                                    .map_err(|error| {
                                        error.into_native(
                                            interp,
                                            "AsyncGeneratorAwaitReturnRejected",
                                        )
                                    })
                            })?;
                            Ok(Value::undefined())
                        })
                    },
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            let on_rejected = vm.stamp_native_creation_realm(on_rejected);
            let rejected_root = vm.scoped_value(scope, on_rejected);
            let capability =
                crate::promise_dispatch::PromiseBuilder::with_context(context.cloned())
                    .capability_stack_rooted(vm, stack, &[], &[])
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            let inner = vm
                .escape_scoped(inner_root)
                .as_promise()
                .ok_or(VmError::InvalidOperand)
                .map_err(CommittedValueError::Fatal)?;
            let on_fulfilled = vm.escape_scoped(fulfilled_root);
            let on_rejected = vm.escape_scoped(rejected_root);
            let outcome = vm
                .register_promise_reactions(
                    inner,
                    Some(on_fulfilled),
                    Some(on_rejected),
                    capability,
                    context.cloned(),
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            if let Some(job) = outcome.immediate_job {
                vm.microtasks.enqueue(job);
            }
            Ok(())
        })
    }

    /// Deliver an awaited `return` completion: a suspended-yield body is
    /// resumed with it (finally blocks run and may override); a body that
    /// never ran or already finished settles the front request directly.
    fn async_generator_deliver_return_completion(
        &mut self,
        stack: &mut ActivationStack,
        context: Option<&ExecutionContext>,
        handle: &crate::generator::JsGenerator,
        resume_body: bool,
        completion: Result<Value, Value>,
    ) -> Result<(), CommittedValueError> {
        if resume_body && handle.has_frame(&self.gc_heap) && !handle.is_done(&self.gc_heap) {
            handle.set_async_state(&mut self.gc_heap, AsyncGeneratorState::SuspendedYield);
            let kind = match completion {
                Ok(value) => crate::GeneratorResumeKind::Return(value),
                Err(reason) => crate::GeneratorResumeKind::Throw(reason),
            };
            return self
                .resume_generator(stack, context, handle, kind)
                .map(|_| ());
        }
        handle.mark_done(&mut self.gc_heap);
        handle.set_async_state(&mut self.gc_heap, AsyncGeneratorState::Completed);
        self.async_generator_complete_step(context, handle, completion, true)
            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
        self.async_generator_resume_next(stack, context, handle)
    }

    /// Complete the front async-generator request.
    pub(crate) fn async_generator_complete_step(
        &mut self,
        context: Option<&ExecutionContext>,
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
        context: Option<&ExecutionContext>,
        capability: &PromiseCapability,
        completion: Result<Value, Value>,
        done: bool,
    ) -> Result<(), VmError> {
        self.with_handle_scope(|interp, scope| {
            let promise = interp.scoped_value(scope, capability.promise);
            if interp.escape_scoped(promise).as_promise().is_none() {
                return Err(VmError::InvalidOperand);
            }
            let jobs = match completion {
                Ok(value) => {
                    let record = interp.make_runtime_rooted_iter_result(value, done, &[], &[])?;
                    interp
                        .escape_scoped(promise)
                        .as_promise()
                        .ok_or(VmError::InvalidOperand)?
                        .fulfill(&mut interp.gc_heap, record)
                }
                Err(reason) => interp
                    .escape_scoped(promise)
                    .as_promise()
                    .ok_or(VmError::InvalidOperand)?
                    .reject(&mut interp.gc_heap, reason),
            };
            interp.note_settle_rejection(&jobs, context);
            for job in jobs.jobs {
                interp.microtasks.enqueue(job);
            }
            Ok(())
        })
    }

    /// Drain queued async-generator requests after the body is done.
    /// `next` answers `{undefined, true}`, `throw` rejects, and `return`
    /// awaits its value first (§27.6.3.5.1) — all through the shared
    /// resume-next walk.
    pub(crate) fn async_generator_drain_done(
        &mut self,
        stack: &mut ActivationStack,
        context: Option<&ExecutionContext>,
        handle: &crate::generator::JsGenerator,
    ) -> Result<(), CommittedValueError> {
        handle.set_async_state(&mut self.gc_heap, AsyncGeneratorState::Completed);
        self.async_generator_resume_next(stack, context, handle)
    }

    /// Resume a generator object above a floor on the current activation stack
    /// until either an [`otter_bytecode::Op::Yield`] pauses it (returning
    /// `{value, done: false}`) or the body runs to completion (returning
    /// `{value: returnValue, done: true}`).
    ///
    /// `kind` selects the entry behaviour per §27.5.3. A body suspended at a
    /// yield receives the kind code and argument in the yield's registers
    /// and its compiled code continues, throws, or returns. A body that has
    /// not started discards a `next` argument, completes on `return`, and
    /// throws a `throw` reason without running.
    ///
    /// A completed body failure preserves the terminal disposition of its
    /// existing dispatch result. Source/realm and resume-register admission are
    /// structural terminal errors. Fresh `{value, done}` allocation remains a
    /// failure of the current JavaScript operation.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-generator.prototype.next>
    /// - <https://tc39.es/ecma262/#sec-generator.prototype.return>
    /// - <https://tc39.es/ecma262/#sec-generator.prototype.throw>
    pub fn resume_generator(
        &mut self,
        stack: &mut ActivationStack,
        context: Option<&ExecutionContext>,
        handle: &crate::generator::JsGenerator,
        kind: GeneratorResumeKind,
    ) -> Result<Value, CommittedValueError> {
        let source_function = handle.with_body(&self.gc_heap, |body| {
            body.frame.as_ref().map(|frame| frame.header.function_id)
        });
        let source = match source_function {
            Some(function_id) => Some(
                self.function_context(context, function_id)
                    .map_err(CommittedValueError::Fatal)?,
            ),
            None => context.cloned(),
        };
        if let Some(function_id) = source_function {
            let realm = self.function_realm_id(function_id);
            if !self.job_realm_is_live(realm) {
                return Err(CommittedValueError::Fatal(VmError::InvalidOperand));
            }
            if realm != self.active_realm_id {
                return self
                    .with_host_realm_id(realm, |interp| {
                        Ok(interp.resume_generator(stack, source.as_ref(), handle, kind))
                    })
                    // The live-id admission above and this lookup have no
                    // intervening callback/collection. The closure always
                    // returns Ok(typed body result); an outer failure is malformed
                    // realm metadata, not a source TypeError from the body.
                    .map_err(|_| CommittedValueError::Fatal(VmError::InvalidOperand))?;
            }
        }
        let context = source.as_ref();
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
                return Err(CommittedValueError::JavaScript(
                    self.err_type(("Generator is already running".to_string()).into()),
                ));
            }
            // Completed (§27.5.3.3/.4): `next` yields {undefined, true},
            // `return` echoes its argument, `throw` re-raises.
            return match kind {
                GeneratorResumeKind::Next(_) => self
                    .make_runtime_rooted_iter_result(Value::undefined(), true, &[], &[])
                    .map_err(CommittedValueError::JavaScript),
                GeneratorResumeKind::Return(arg) => self
                    .make_runtime_rooted_iter_result(arg, true, &[], &[])
                    .map_err(CommittedValueError::JavaScript),
                GeneratorResumeKind::Throw(reason) => {
                    self.set_pending_uncaught_throw(reason);
                    Err(CommittedValueError::JavaScript(
                        self.err_uncaught((self.render_thrown(&reason)).into()),
                    ))
                }
            };
        }
        let context = context
            .ok_or(VmError::InvalidOperand)
            .map_err(CommittedValueError::Fatal)?;
        // Pull the frame out of the gen body so we can mutate it.
        let (frame, cold) = match handle.take_frame(&mut self.gc_heap) {
            Some(pair) => pair,
            None => {
                return self
                    .make_runtime_rooted_iter_result(Value::undefined(), true, &[], &[])
                    .map_err(CommittedValueError::JavaScript);
            }
        };
        let mut frame = Box::new(
            self.resume_parked_frame(*frame)
                .map_err(CommittedValueError::Fatal)?,
        );
        let input_roots = self
            .gc_heap
            .register_extra_roots(otter_gc::ExtraRoots::new(frame.as_ref()));
        if let Some(c) = cold {
            self.prepared_attach_cold(&mut frame, *c);
        }
        // §27.5.3.7 — a frame parked on a yield receives every resume kind
        // as data (kind code + argument): the compiled code after the
        // suspension continues, throws the argument, or returns it through
        // its finally blocks and open iterators, and a `yield*` forwards it
        // to the inner iterator.
        if resume_dst != crate::generator::JsGenerator::RESUME_DST_NONE {
            handle.clear_delegating(&mut self.gc_heap);
            let kind_dst = handle.resume_kind_dst(&self.gc_heap);
            let (code, arg) = match &kind {
                GeneratorResumeKind::Next(v) => (0, *v),
                GeneratorResumeKind::Throw(v) => (1, *v),
                GeneratorResumeKind::Return(v) => (2, *v),
            };
            frame
                .seed_register(kind_dst, Value::number_i32(code))
                .map_err(CommittedValueError::Fatal)?;
            frame
                .seed_register(resume_dst, arg)
                .map_err(CommittedValueError::Fatal)?;
            let is_async = handle.is_async(&self.gc_heap);
            if is_async {
                handle.set_async_state(&mut self.gc_heap, AsyncGeneratorState::Executing);
            }
            let floor = stack.floor();
            drop(input_roots);
            stack.push(*frame);
            return self.finish_generator_dispatch(stack, floor, context, handle, is_async);
        }
        // Suspended at the start (§27.5.3.3/.4): the body has not run, so
        // the value of a first `next()` is discarded, a `return` completes
        // without resuming, and a `throw` leaves the body at once.
        match &kind {
            GeneratorResumeKind::Next(_) => {}
            GeneratorResumeKind::Return(arg) => {
                handle.mark_done(&mut self.gc_heap);
                // An async generator answers through the request its caller
                // queued, not through this return value — the caller was
                // handed that request's promise and has already gone. The
                // whole queue completes the way a finished body completes
                // it: the front request with the value `return` carried,
                // and every request behind it as done.
                if handle.is_async(&self.gc_heap) {
                    self.async_generator_complete_step(Some(context), handle, Ok(*arg), true)
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                    self.async_generator_drain_done(stack, Some(context), handle)?;
                    return Ok(Value::undefined());
                }
                return self
                    .make_runtime_rooted_iter_result(*arg, true, &[], &[])
                    .map_err(CommittedValueError::JavaScript);
            }
            GeneratorResumeKind::Throw(reason) => {
                frame.resume = crate::prepared_call::ResumeInput::Throw(*reason);
            }
        }
        let floor = stack.floor();
        drop(input_roots);
        stack.push(*frame);
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
    ) -> Result<Value, CommittedValueError> {
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
                    return self
                        .make_runtime_rooted_iter_result(v, false, &[], &[])
                        .map_err(CommittedValueError::JavaScript);
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
                    self.async_generator_complete_step(Some(context), handle, Ok(value), true)
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                    self.async_generator_drain_done(stack, Some(context), handle)?;
                    return Ok(Value::undefined());
                }
                self.make_runtime_rooted_iter_result(value, true, &[], &[])
                    .map_err(CommittedValueError::JavaScript)
            }
            Err(err) => {
                handle.mark_done(&mut self.gc_heap);
                let error = CommittedValueError::completed_call(err);
                let CommittedValueError::JavaScript(err) = error else {
                    return Err(error);
                };
                if is_async {
                    let reason = self
                        .vm_error_to_throwable_with_stack_roots(Some(context), stack, &err)
                        .map_err(CommittedValueError::Fatal)?;
                    {
                        self.async_generator_complete_step(
                            Some(context),
                            handle,
                            Err(reason),
                            true,
                        )
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                        self.async_generator_drain_done(stack, Some(context), handle)?;
                        return Ok(Value::undefined());
                    }
                }
                Err(CommittedValueError::JavaScript(err))
            }
        })();
        self.release_frames_above(stack, floor);
        result
    }
    /// Drive one tick of [`Op::GetIterator`] in the interpreter: either the
    /// pc advances with the record in `dst`, or a frame for `@@iterator` is
    /// pushed and the opcode resumes when it returns.
    ///
    /// # Algorithm (§7.4.3 `GetIterator`)
    /// 1. **Resume** — when the running frame's pending GetIterator matches
    ///    the current pc, wrap the called function's result from `dst`
    ///    (GetIteratorDirect reads `next` once).
    /// 2. **Fresh entry, unobservable** — a proven built-in iterable or
    ///    iterator, or a generator, gets its record directly.
    /// 3. **Fresh entry, observable** — `GetMethod(obj, @@iterator)`, then a
    ///    pushed frame invokes it with `this = obj`; pc stays on the opcode
    ///    so the resume can wrap the returned iterator.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-getiterator>
    pub(crate) fn drive_get_iterator(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        operands: impl crate::executable::OperandSource,
    ) -> Result<(), CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            let dst = register_operand(operands.first())
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            let src = register_operand(operands.get(1))
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            let top_idx = stack.len() - 1;
            let pc = stack[top_idx].pc;

            // 1. Resume path.
            let resume = interp
                .frame_cold(&stack[top_idx])
                .and_then(|c| c.pending_get_iterator.as_ref())
                .filter(|s| s.pc == pc && s.dst == dst)
                .cloned();
            if let Some(_state) = resume {
                let produced = *read_register(&stack[top_idx], dst)
                    .map_err(|error| CommittedValueError::Fatal(error.into()))?;
                // §7.4.3 step 2 — `[@@iterator]()` must return an
                // Object; GetIteratorDirect then reads `next` once. Both
                // are owned by the shared wrap helper. A failure here must
                // still clear the parked continuation before propagating.
                let primordial = interp.frame_iterates_primordially(context, stack, top_idx);
                let produced_value =
                    match interp.wrap_iterator_method_result(context, stack, produced, primordial) {
                        Ok(value) => value,
                        Err(e) => {
                            if let Some(cold) = interp.frame_cold_mut(&mut stack[top_idx]) {
                                cold.pending_get_iterator = None;
                            }
                            return Err(e);
                        }
                    };
                write_register(&mut stack[top_idx], dst, produced_value)
                    .map_err(|error| CommittedValueError::Fatal(error.into()))?;
                if let Some(cold) = interp.frame_cold_mut(&mut stack[top_idx]) {
                    cold.pending_get_iterator = None;
                }
                stack[top_idx]
                    .advance_pc()
                    .map_err(|error| CommittedValueError::Fatal(error.into()))?;
                return Ok(());
            }

            // 2. Fresh entry whose record needs no observable read.
            let value = *read_register(&stack[top_idx], src)
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            let value_root = interp.scoped_value(scope, value);
            let primordial = interp.frame_iterates_primordially(context, stack, top_idx);
            if let Some(record) = interp
                .unobservable_iterator_record(stack, value, primordial)
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?
            {
                write_register(&mut stack[top_idx], dst, record)
                    .map_err(|error| CommittedValueError::Fatal(error.into()))?;
                stack[top_idx]
                    .advance_pc()
                    .map_err(|error| CommittedValueError::Fatal(error.into()))?;
                return Ok(());
            }
            // 3. Call `@@iterator` in a pushed frame; pc stays on
            // `Op::GetIterator`, the result lands in `dst` and the resume
            // guard above wraps it.
            let callee =
                interp.get_iterator_method(stack, context, interp.escape_scoped(value_root))?;
            interp
                .frame_ensure_cold(&mut stack[top_idx])
                .pending_get_iterator = Some(PendingGetIterator { pc, dst });
            interp
                .invoke(
                    stack,
                    context,
                    &callee,
                    interp.escape_scoped(value_root),
                    SmallVec::new(),
                    dst,
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            Ok(())
        })
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
    ) -> Result<bool, CommittedValueError> {
        let value_dst = register_operand(operands.first()).map_err(CommittedValueError::Fatal)?;
        let done_dst = register_operand(operands.get(1)).map_err(CommittedValueError::Fatal)?;
        let iter_reg = register_operand(operands.get(2)).map_err(CommittedValueError::Fatal)?;
        let top_idx = stack.len() - 1;
        let pc = stack[top_idx].pc;

        // 1. Resume path — read the parked record.
        let resume = self
            .frame_cold(&stack[top_idx])
            .and_then(|c| c.pending_iterator_next.as_ref())
            .filter(|s| s.pc == pc && s.value_dst == value_dst && s.done_dst == done_dst)
            .cloned();
        if let Some(state) = resume {
            let result = *read_register(&stack[top_idx], state.result_reg)
                .map_err(CommittedValueError::Fatal)?;
            // §7.4.5 step 3 — the result record must be Type Object,
            // which includes Proxy and other exotic shapes, not just
            // plain JsObject.
            if !crate::reflect::is_type_object_value(&result) {
                if let Some(cold) = self.frame_cold_mut(&mut stack[top_idx]) {
                    cold.pending_iterator_next = None;
                }
                return Err(CommittedValueError::JavaScript(VmError::TypeMismatch));
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
            write_register(&mut stack[top_idx], value_dst, value)
                .map_err(CommittedValueError::Fatal)?;
            write_register(&mut stack[top_idx], done_dst, Value::boolean(done))
                .map_err(CommittedValueError::Fatal)?;
            if let Some(cold) = self.frame_cold_mut(&mut stack[top_idx]) {
                cold.pending_iterator_next = None;
            }
            stack[top_idx]
                .advance_pc()
                .map_err(CommittedValueError::Fatal)?;
            return Ok(true);
        }

        // 2 + 3. Fresh entry. Inspect the iterator's inner state.
        let iter_value =
            *read_register(&stack[top_idx], iter_reg).map_err(CommittedValueError::Fatal)?;
        let Some(iter_rc_handle) = iter_value.as_iterator() else {
            return Err(CommittedValueError::JavaScript(VmError::TypeMismatch));
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
                Some(context),
                &handle,
                GeneratorResumeKind::Next(Value::undefined()),
            )?;
            let Some(obj) = result.as_object() else {
                return Err(CommittedValueError::JavaScript(VmError::TypeMismatch));
            };
            let value =
                crate::object::get(obj, &self.gc_heap, "value").unwrap_or(Value::undefined());
            let done = crate::object::get(obj, &self.gc_heap, "done")
                .unwrap_or(Value::undefined())
                .to_boolean(&self.gc_heap);
            if done {
                self.gc_heap.with_payload(*iter_rc, |state| state.exhaust());
            }
            write_register(&mut stack[top_idx], value_dst, value)
                .map_err(CommittedValueError::Fatal)?;
            write_register(&mut stack[top_idx], done_dst, Value::boolean(done))
                .map_err(CommittedValueError::Fatal)?;
            stack[top_idx]
                .advance_pc()
                .map_err(CommittedValueError::Fatal)?;
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
                    | IteratorState::ArrayLike { .. }
            )
        });
        if needs_full_step {
            let (value, done) = self.iterator_next_full(context, stack, iter_rc)?;
            write_register(&mut stack[top_idx], value_dst, value)
                .map_err(CommittedValueError::Fatal)?;
            write_register(&mut stack[top_idx], done_dst, Value::boolean(done))
                .map_err(CommittedValueError::Fatal)?;
            stack[top_idx]
                .advance_pc()
                .map_err(CommittedValueError::Fatal)?;
            return Ok(true);
        }
        // §23.1.5.1 ArrayIterator `next` performs Get(array, index).
        // Anything that is not one plain dense element — an accessor, a
        // sparse slot, a hole the prototype chain answers — must run the
        // observable [[Get]] through the interpreter; the synchronous
        // fast path below reads raw element storage only.
        let array_step = self.gc_heap.read_payload(*iter_rc, |state| match state {
            IteratorState::Array { array, index, .. }
            | IteratorState::ArrayEntry { array, index } => Some((*array, *index)),
            _ => None,
        });
        if let Some((array, index)) = array_step
            && index < crate::array::len(array, &self.gc_heap)
            && crate::array::plain_dense_element(array, &self.gc_heap, index).is_none()
        {
            let (value, done) = self.iterator_next_full(context, stack, iter_rc)?;
            write_register(&mut stack[top_idx], value_dst, value)
                .map_err(CommittedValueError::Fatal)?;
            write_register(&mut stack[top_idx], done_dst, Value::boolean(done))
                .map_err(CommittedValueError::Fatal)?;
            stack[top_idx]
                .advance_pc()
                .map_err(CommittedValueError::Fatal)?;
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
        let (next_fn, user_iter_value) = match cached_next {
            Some(cached) => (cached, user_iter_value),
            None => self.with_handle_scope(|interp, scope| {
                let receiver = interp.scoped_value(scope, user_iter_value);
                let outcome = interp.ordinary_get_value(
                    stack,
                    Some(context),
                    interp.escape_scoped(receiver),
                    interp.escape_scoped(receiver),
                    &VmPropertyKey::String("next"),
                    0,
                )?;
                let next_fn = match outcome {
                    VmGetOutcome::Value(value) => value,
                    VmGetOutcome::InvokeGetter { getter } => interp
                        .run_callable_sync_rooted(
                            stack,
                            Some(context),
                            &getter,
                            interp.escape_scoped(receiver),
                            SmallVec::new(),
                        )
                        .map_err(CommittedValueError::completed_call)?,
                };
                Ok::<_, CommittedValueError>((next_fn, interp.escape_scoped(receiver)))
            })?,
        };
        if !is_callable(&next_fn) {
            return Err(CommittedValueError::JavaScript(VmError::TypeMismatch));
        }
        // §7.4.9 / IteratorStepValue — a throw out of `next` sets
        // [[Done]]. The call runs in a pushed frame, so its throw reaches
        // this activation as an abandoned ladder, which marks the parked
        // iterator done (`abandon_pending_ladders`).
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
        let user_iter_value = self.gc_heap.read_payload(*iter_rc, |state| match state {
            IteratorState::User { iterator, .. } => *iterator,
            _ => user_iter_value,
        });
        self.invoke(stack, context, &next_fn, user_iter_value, args, value_dst)
            .map_err(CommittedValueError::JavaScript)?;
        Ok(true)
    }
}

fn iterator_step_read(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: &ExecutionContext,
    result: &Value,
) -> Result<Option<Value>, CommittedValueError> {
    interp.with_handle_scope(|interp, scope| {
        let result = interp.scoped_value(scope, *result);
        let done_value =
            interp.iter_result_get(stack, context, interp.escape_scoped(result), "done")?;
        if done_value.to_boolean(interp.gc_heap()) {
            return Ok(None);
        }
        let value =
            interp.iter_result_get(stack, context, interp.escape_scoped(result), "value")?;
        Ok(Some(value))
    })
}
