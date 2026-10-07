//! Stack-owned iterator and spread-collection operations.
//!
//! # Contents
//! - Fast Array iterator records opened and stepped for generated callees.
//! - Dense spread-result appends through the published native root window.
//! - Exact pre-effect refusal for observable/custom iterator protocols.
//!
//! # Invariants
//! - A fast record opens only while the realm proves the Array's GetIterator
//!   and `next` built-in, and steps only over plain dense elements.
//! - A refused operation has run no callback and written no iterator or
//!   destination register, so generated code may side-exit at the original
//!   opcode.
//! - Every input remains rooted through the published runtime activation
//!   during collection.
//! - No materialized interpreter frame or raw register window escapes this
//!   typed boundary.
//!
//! # See also
//! - [`crate::Interpreter::jit_runtime_iterator_op`]
//! - [`crate::array_prototype::ArrayMethodTag::Values`]

use otter_bytecode::Op;

use crate::iterator_record::RecordCursor;
use crate::{BuiltinIteratorOrigin, IteratorState, Value, VmError, array, step_iterator};

use super::RuntimeCall;

/// Result of a representation-neutral iterator operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IteratorRuntimeOutcome {
    /// The opcode completed and generated code may fall through.
    Completed,
    /// The source requires observable/custom semantics owned by the
    /// materialized interpreter path. No effect has occurred.
    Bail,
}

impl RuntimeCall<'_> {
    /// Complete the stack-owned subset of one iterator opcode.
    pub fn iterator_op(
        &mut self,
        opcode: u8,
        arg0: u16,
        arg1: u16,
        arg2: u16,
    ) -> Result<IteratorRuntimeOutcome, VmError> {
        match opcode {
            value if value == Op::GetIterator as u8 => self.get_builtin_array_iterator(arg0, arg1),
            value if value == Op::IteratorNext as u8 => {
                self.step_builtin_iterator(arg0, arg1, arg2)
            }
            value if value == Op::IteratorClose as u8 || value == Op::IteratorCloseThrow as u8 => {
                self.close_fast_record(arg0)
            }
            _ => Ok(IteratorRuntimeOutcome::Bail),
        }
    }

    /// Append one value while the receiver/value registers remain published
    /// as precise roots in either physical frame representation.
    pub fn spread_array_push(
        &mut self,
        array_register: u16,
        value_register: u16,
    ) -> Result<(), VmError> {
        let array_value = self.read(array_register)?;
        let value = self.read(value_register)?;
        let array = array_value.as_array().ok_or(VmError::TypeMismatch)?;
        // SAFETY: RuntimeCall owns exclusive mutator access for this operation.
        let vm = unsafe { &mut *self.vm.as_ptr() };
        vm.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Alloc);
        let _roots = vm.scope_runtime_roots_guard();
        let mut no_extra_roots = |_visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {};
        array::push_with_roots(array, &mut vm.gc_heap, value, &mut no_extra_roots)?;
        Ok(())
    }

    /// Open a fast Array record (see [`crate::iterator_record`]) while the
    /// realm proves GetIterator and `next` built-in. Without a frame the
    /// caller's primordial iteration is unknown, so only the proof opens one.
    fn get_builtin_array_iterator(
        &mut self,
        destination: u16,
        source: u16,
    ) -> Result<IteratorRuntimeOutcome, VmError> {
        let source_value = self.read(source)?;
        if !source_value.is_array() {
            return Ok(IteratorRuntimeOutcome::Bail);
        }
        // SAFETY: RuntimeCall owns exclusive mutator access for this operation.
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let proven = {
            // Proving the chain caches its prototype roles once.
            let _roots = vm.scope_runtime_roots_guard();
            vm.fast_array_iterable(source_value, false)
        };
        if !proven {
            return Ok(IteratorRuntimeOutcome::Bail);
        }
        let array = self.read(source)?;
        self.write(destination, array)?;
        self.write(destination + 1, Value::number_i32(0))?;
        Ok(IteratorRuntimeOutcome::Completed)
    }

    /// Close a fast Array record that is done, or whose `return` the realm
    /// proves absent; anything that may run `return` is refused first.
    fn close_fast_record(&mut self, record: u16) -> Result<IteratorRuntimeOutcome, VmError> {
        // SAFETY: RuntimeCall owns exclusive mutator access for this operation.
        let vm = unsafe { &mut *self.vm.as_ptr() };
        Ok(match RecordCursor::of(self.read(record + 1)?) {
            RecordCursor::Exhausted => IteratorRuntimeOutcome::Completed,
            RecordCursor::Fast(_) => {
                let proven = {
                    let _roots = vm.scope_runtime_roots_guard();
                    vm.array_iterator_close_proven()
                };
                if proven {
                    IteratorRuntimeOutcome::Completed
                } else {
                    IteratorRuntimeOutcome::Bail
                }
            }
            RecordCursor::Generic => IteratorRuntimeOutcome::Bail,
        })
    }

    /// Step a fast Array record whose element is one plain dense value, or a
    /// built-in Array iterator value; anything observable is refused first.
    fn step_builtin_iterator(
        &mut self,
        value_destination: u16,
        done_destination: u16,
        record: u16,
    ) -> Result<IteratorRuntimeOutcome, VmError> {
        // SAFETY: RuntimeCall owns exclusive mutator access for this operation.
        let vm = unsafe { &mut *self.vm.as_ptr() };
        match RecordCursor::of(self.read(record + 1)?) {
            RecordCursor::Exhausted => {
                self.write(value_destination, Value::undefined())?;
                self.write(done_destination, Value::boolean(true))?;
                return Ok(IteratorRuntimeOutcome::Completed);
            }
            RecordCursor::Fast(index) => {
                let array = self
                    .read(record)?
                    .as_array()
                    .ok_or(VmError::InvalidOperand)?;
                if index as usize >= array::len(array, &vm.gc_heap) {
                    self.write(record + 1, Value::boolean(true))?;
                    self.write(value_destination, Value::undefined())?;
                    self.write(done_destination, Value::boolean(true))?;
                    return Ok(IteratorRuntimeOutcome::Completed);
                }
                let (Some(value), Ok(next)) = (
                    array::plain_dense_element(array, &vm.gc_heap, index as usize),
                    i32::try_from(index + 1),
                ) else {
                    return Ok(IteratorRuntimeOutcome::Bail);
                };
                self.write(record + 1, Value::number_i32(next))?;
                self.write(value_destination, value)?;
                self.write(done_destination, Value::boolean(false))?;
                return Ok(IteratorRuntimeOutcome::Completed);
            }
            RecordCursor::Generic => {}
        }
        let Some(iterator) = self.read(record)?.as_iterator() else {
            return Ok(IteratorRuntimeOutcome::Bail);
        };
        let position = vm.gc_heap.read_payload(iterator, |state| match state {
            IteratorState::Array {
                array,
                index,
                origin: BuiltinIteratorOrigin::Array,
            } => Some((*array, *index)),
            _ => None,
        });
        // A hole, an accessor or a sparse element needs the observable
        // `[[Get]]` the materialized step runs.
        let Some((array, index)) = position else {
            return Ok(IteratorRuntimeOutcome::Bail);
        };
        if index < array::len(array, &vm.gc_heap)
            && array::plain_dense_element(array, &vm.gc_heap, index).is_none()
        {
            return Ok(IteratorRuntimeOutcome::Bail);
        }
        let (value, done) = step_iterator(iterator, &mut vm.gc_heap)?;
        self.write(value_destination, value)?;
        self.write(done_destination, Value::boolean(done))?;
        Ok(IteratorRuntimeOutcome::Completed)
    }
}
