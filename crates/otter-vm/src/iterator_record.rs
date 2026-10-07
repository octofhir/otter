//! Synchronous iterator records held in a register pair.
//!
//! `Op::GetIterator` writes an iterator record into two consecutive
//! registers: the iterator, then its cursor. GetIterator over an ordinary
//! Array whose iteration nothing can observe keeps the array itself and an
//! int32 cursor, with no iterator object (JSC's fast-array iteration mode):
//! `%ArrayIteratorPrototype%.next` reads the array's length and the element at
//! the cursor on every step, so the record needs no other state. Every other
//! iterable keeps an iterator value and an `undefined` cursor.
//!
//! # Contents
//! - [`RecordCursor`] — a record's mode, read from its cursor.
//! - `Interpreter::open_fast_array_record`, `step_fast_array_record` and
//!   `close_fast_array_record` drive fast records;
//!   `write_iterator_record` publishes a generic one.
//!
//! # Invariants
//! - The cursor is an int32 `n >= 0` for a live fast record, `true` once the
//!   record is exhausted or a step threw, and `undefined` for a generic one.
//! - A fast record opens only while GetIterator's `@@iterator` and the
//!   iterator's `next` are the built-ins (the realm's iteration proof, or
//!   runtime-internal code). GetIterator caches `next`, so a later patch of
//!   `%ArrayIteratorPrototype%.next` never reaches an open record.
//! - Closing reads `return` at the close: while the proof keeps it absent,
//!   or for runtime-internal code, nothing runs; otherwise the record first
//!   becomes an ArrayIterator object at its cursor, so a user `return`
//!   observes an ordinary iterator.
//! - A cursor past `i32::MAX` turns the record into that ArrayIterator too.
//!
//! # See also
//! - [`crate::iteration_protocol`] for the iteration proofs.
//! - [`crate::iterator_ops`] for the generic record's steps.

use crate::{
    ActivationStack, BuiltinIteratorOrigin, CommittedValueError, ExecutionContext, Interpreter,
    IteratorState, Value, VmError, array, read_register, write_register,
};

/// A synchronous iterator record's mode, read from its cursor register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordCursor {
    /// A fast Array record about to read this index.
    Fast(u32),
    /// A fast Array record that finished, or whose step threw.
    Exhausted,
    /// An iterator value in the record's first register.
    Generic,
}

impl RecordCursor {
    /// The mode of the record whose cursor is `cursor`.
    pub(crate) fn of(cursor: Value) -> Self {
        match cursor.as_i32() {
            Some(index) => Self::Fast(index as u32),
            None if cursor.as_boolean() == Some(true) => Self::Exhausted,
            None => Self::Generic,
        }
    }

    /// The mode of the record at `record` in `frame`.
    pub(crate) fn read(frame: &crate::Frame, record: u16) -> Result<Self, VmError> {
        Ok(Self::of(*read_register(frame, record + 1)?))
    }
}

impl Interpreter {
    /// Publish a generic record: `iterator` and an `undefined` cursor.
    pub(crate) fn write_iterator_record(
        frame: &mut crate::Frame,
        record: u16,
        iterator: Value,
    ) -> Result<(), VmError> {
        write_register(frame, record, iterator)?;
        write_register(frame, record + 1, Value::undefined())
    }

    /// Open a fast Array record at `record` over the value in `source` when
    /// nothing can observe its iteration; `false` leaves GetIterator to the
    /// generic path.
    pub(crate) fn open_fast_array_record(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        record: u16,
        source: u16,
    ) -> Result<bool, VmError> {
        let value = *read_register(&stack[frame_index], source)?;
        if !value.is_array() {
            return Ok(false);
        }
        let primordial = self.frame_iterates_primordially(context, stack, frame_index);
        if !self.fast_array_iterable(value, primordial) {
            return Ok(false);
        }
        // Proving the chain may allocate; the source register kept the array.
        let frame = &mut stack[frame_index];
        let array = *read_register(frame, source)?;
        write_register(frame, record, array)?;
        write_register(frame, record + 1, Value::number_i32(0))?;
        Ok(true)
    }

    /// One `%ArrayIteratorPrototype%.next` step of the fast record at
    /// `record`: the element at the cursor, or done past the array's length.
    /// A throw out of the element's `[[Get]]` exhausts the record.
    pub(crate) fn step_fast_array_record(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        record: u16,
    ) -> Result<(Value, bool), CommittedValueError> {
        let fatal = |error: VmError| CommittedValueError::Fatal(error);
        let RecordCursor::Fast(index) =
            RecordCursor::read(&stack[frame_index], record).map_err(fatal)?
        else {
            return Ok((Value::undefined(), true));
        };
        let array = read_register(&stack[frame_index], record)
            .map_err(fatal)?
            .as_array()
            .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
        let exhausted = Value::boolean(true);
        if index as usize >= array::len(array, &self.gc_heap) {
            write_register(&mut stack[frame_index], record + 1, exhausted).map_err(fatal)?;
            return Ok((Value::undefined(), true));
        }
        let value = match array::plain_dense_element(array, &self.gc_heap, index as usize) {
            Some(value) => value,
            // A hole, an accessor or a sparse element runs the observable
            // `[[Get]]` through the prototype chain.
            None => {
                let loaded = self.load_property_value(
                    context,
                    stack,
                    Value::array(array),
                    &index.to_string(),
                );
                match loaded {
                    Ok(value) => value,
                    Err(error) => {
                        write_register(&mut stack[frame_index], record + 1, exhausted)
                            .map_err(fatal)?;
                        return Err(error);
                    }
                }
            }
        };
        let next = index + 1;
        match i32::try_from(next) {
            Ok(next) => {
                write_register(&mut stack[frame_index], record + 1, Value::number_i32(next))
                    .map_err(fatal)?;
            }
            Err(_) => self
                .materialize_array_record(stack, frame_index, record, next)
                .map_err(CommittedValueError::JavaScript)?,
        }
        Ok((value, false))
    }

    /// §7.4.11 IteratorClose of the fast record at `record`. An exhausted
    /// record is done; a live one runs `return` only when it may exist.
    pub(crate) fn close_fast_array_record(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        record: u16,
        throw_completion: bool,
    ) -> Result<(), CommittedValueError> {
        let RecordCursor::Fast(index) =
            RecordCursor::read(&stack[frame_index], record).map_err(CommittedValueError::Fatal)?
        else {
            return Ok(());
        };
        if self.frame_iterates_primordially(context, stack, frame_index)
            || self.array_iterator_close_proven()
        {
            return Ok(());
        }
        self.materialize_array_record(stack, frame_index, record, index)
            .map_err(CommittedValueError::JavaScript)?;
        let iterator =
            *read_register(&stack[frame_index], record).map_err(CommittedValueError::Fatal)?;
        self.iterator_close_op(context, stack, frame_index, iterator, throw_completion)
    }

    /// §7.4.11 IteratorClose of the record at `record`, in either mode.
    pub(crate) fn close_iterator_record(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        record: u16,
        throw_completion: bool,
    ) -> Result<(), CommittedValueError> {
        let cursor =
            RecordCursor::read(&stack[frame_index], record).map_err(CommittedValueError::Fatal)?;
        if cursor != RecordCursor::Generic {
            return self.close_fast_array_record(
                context,
                stack,
                frame_index,
                record,
                throw_completion,
            );
        }
        let iterator =
            *read_register(&stack[frame_index], record).map_err(CommittedValueError::Fatal)?;
        self.iterator_close_op(context, stack, frame_index, iterator, throw_completion)
    }

    /// Turn the fast record at `record` into an ArrayIterator object at
    /// `index`, held as a generic record.
    fn materialize_array_record(
        &mut self,
        stack: &mut ActivationStack,
        frame_index: usize,
        record: u16,
        index: u32,
    ) -> Result<(), VmError> {
        let source = *read_register(&stack[frame_index], record)?;
        let array = source.as_array().ok_or(VmError::InvalidOperand)?;
        let iterator = self.alloc_stack_rooted_iterator_state(
            stack,
            IteratorState::Array {
                array,
                index: index as usize,
                origin: BuiltinIteratorOrigin::Array,
            },
            &[&source],
            &[],
        )?;
        Self::write_iterator_record(&mut stack[frame_index], record, Value::iterator(iterator))
    }
}
