//! Stack-owned iterator and spread-collection operations.
//!
//! # Contents
//! - Built-in Array iterator acquisition and stepping for generated callees.
//! - Dense spread-result appends through the published native root window.
//! - Exact pre-effect refusal for observable/custom iterator protocols.
//!
//! # Invariants
//! - The fast path runs only while `%Array.prototype%[@@iterator]` is the
//!   original `values` builtin and the receiver has no exotic override.
//! - A refused operation has performed no allocation, callback, iterator
//!   mutation, or destination write, so generated code may side-exit at the
//!   original opcode.
//! - Iterator handles are allocated in old space and every input remains
//!   rooted through the published runtime activation during collection.
//! - No materialized interpreter frame or raw register window escapes this
//!   typed boundary.
//!
//! # See also
//! - [`crate::Interpreter::jit_runtime_iterator_op`]
//! - [`crate::array_prototype::ArrayMethodTag::Values`]

use otter_bytecode::Op;

use crate::{BuiltinIteratorOrigin, IteratorState, Value, VmError, array, object, step_iterator};

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

    fn get_builtin_array_iterator(
        &mut self,
        destination: u16,
        source: u16,
    ) -> Result<IteratorRuntimeOutcome, VmError> {
        let source_value = self.read(source)?;
        let Some(array) = source_value.as_array() else {
            return Ok(IteratorRuntimeOutcome::Bail);
        };
        // SAFETY: RuntimeCall owns exclusive mutator access for this operation.
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let iterator_symbol = vm
            .well_known_symbols
            .get(crate::symbol::WellKnown::Iterator);
        if array::prototype_override(array, &vm.gc_heap).is_some()
            || array::get_symbol_property(array, &vm.gc_heap, iterator_symbol).is_some()
            || array::get_symbol_accessor(array, &vm.gc_heap, iterator_symbol).is_some()
        {
            return Ok(IteratorRuntimeOutcome::Bail);
        }
        let Some(prototype) = vm
            .current_array_prototype_override()
            .and_then(Value::as_object)
        else {
            return Ok(IteratorRuntimeOutcome::Bail);
        };
        let object::PropertyLookup::Data { value: method, .. } =
            object::lookup_symbol(prototype, &vm.gc_heap, iterator_symbol)
        else {
            return Ok(IteratorRuntimeOutcome::Bail);
        };
        if !crate::array_prototype::ArrayMethodTag::Values.matches_builtin(method, &vm.gc_heap) {
            return Ok(IteratorRuntimeOutcome::Bail);
        }

        vm.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Alloc);
        let iterator = vm.alloc_runtime_rooted_iterator_state(
            IteratorState::Array {
                array,
                index: 0,
                origin: BuiltinIteratorOrigin::Array,
            },
            &[&source_value],
            &[],
        )?;
        self.write(destination, Value::iterator(iterator))?;
        Ok(IteratorRuntimeOutcome::Completed)
    }

    fn step_builtin_iterator(
        &mut self,
        value_destination: u16,
        done_destination: u16,
        iterator_register: u16,
    ) -> Result<IteratorRuntimeOutcome, VmError> {
        let Some(iterator) = self.read(iterator_register)?.as_iterator() else {
            return Ok(IteratorRuntimeOutcome::Bail);
        };
        // SAFETY: RuntimeCall owns exclusive mutator access for this operation.
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let is_builtin_array = vm.gc_heap.read_payload(iterator, |state| {
            matches!(
                state,
                IteratorState::Array {
                    origin: BuiltinIteratorOrigin::Array,
                    ..
                }
            )
        });
        if !is_builtin_array {
            return Ok(IteratorRuntimeOutcome::Bail);
        }
        let (value, done) = step_iterator(iterator, &mut vm.gc_heap)?;
        self.write(value_destination, value)?;
        self.write(done_destination, Value::boolean(done))?;
        Ok(IteratorRuntimeOutcome::Completed)
    }
}
