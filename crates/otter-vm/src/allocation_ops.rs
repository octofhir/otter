//! Allocation opcode helpers.
//!
//! These helpers allocate VM heap objects for bytecode instructions that do not
//! alter the call stack. They sit behind the dense executable operand format so
//! the main dispatch loop can keep allocation tails out of `lib.rs`.
//!
//! # Contents
//! - Object literal allocation.
//! - Array literal allocation from variadic register operands.
//! - Array push helper used by spread/rest lowering.
//! - WeakRef and FinalizationRegistry allocation.
//!
//! # Invariants
//! - Inputs are decoded from executable operands.
//! - Helpers advance the current frame PC exactly once on success.
//!
//! # See also
//! - [`crate::array`]
//! - [`crate::object`]
//! - [`crate::executable`]

use otter_bytecode::Operand;
use otter_gc::raw::RawGc;
use smallvec::SmallVec;

use crate::{
    ExecutionContext, Frame, Interpreter, Value, VmError,
    operand_decode::{const_operand, register_operand},
    read_register, regexp,
    runtime_state::RuntimeState,
    write_register,
};

impl Interpreter {
    fn collect_allocation_roots(&self, stack: &SmallVec<[Frame; 8]>) -> Vec<*mut RawGc> {
        let mut roots = Vec::new();
        RuntimeState::new(self).trace_roots(&mut |slot| roots.push(slot));
        for frame in stack {
            frame.trace_frame_slots(&mut |slot| roots.push(slot));
        }
        roots
    }

    pub(crate) fn alloc_stack_rooted_object(
        &mut self,
        stack: &SmallVec<[Frame; 8]>,
    ) -> Result<crate::object::JsObject, VmError> {
        self.alloc_stack_rooted_object_with_extra_roots(stack, &[])
    }

    pub(crate) fn alloc_stack_rooted_object_with_extra_roots(
        &mut self,
        stack: &SmallVec<[Frame; 8]>,
        extra_roots: &[&Value],
    ) -> Result<crate::object::JsObject, VmError> {
        let roots = self.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            for value in extra_roots {
                value.trace_value_slots(visitor);
            }
        };
        crate::object::alloc_object_with_roots(&mut self.gc_heap, &mut external_visit)
            .map_err(VmError::from)
    }

    fn alloc_stack_rooted_array_from_elements(
        &mut self,
        stack: &SmallVec<[Frame; 8]>,
        elements: SmallVec<[Value; 4]>,
    ) -> Result<crate::array::JsArray, VmError> {
        let roots = self.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
        };
        crate::array::from_elements_with_roots(&mut self.gc_heap, elements, &mut external_visit)
            .map_err(VmError::from)
    }

    pub(crate) fn run_new_object_reg(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        top_idx: usize,
        dst: u16,
    ) -> Result<(), VmError> {
        let proto = self.object_prototype_object_opt();
        let obj = self.alloc_stack_rooted_object(stack)?;
        if let Some(proto) = proto {
            crate::object::set_prototype(obj, &mut self.gc_heap, Some(proto));
        }
        let frame = &mut stack[top_idx];
        write_register(frame, dst, Value::Object(obj))?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_new_array_operands(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        top_idx: usize,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let count = const_operand(operands.get(1))? as usize;
        let mut elements: SmallVec<[Value; 4]> = SmallVec::with_capacity(count);
        {
            let frame = &stack[top_idx];
            for i in 0..count {
                let r = register_operand(operands.get(2 + i))?;
                elements.push(read_register(frame, r)?.clone());
            }
        }
        let array = self.alloc_stack_rooted_array_from_elements(stack, elements)?;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, Value::Array(array))?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_load_regexp_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        idx: u32,
    ) -> Result<(), VmError> {
        let (pattern_utf16, flags) = context
            .regexp_constant(idx)
            .ok_or(VmError::InvalidOperand)?;
        let regex =
            regexp::JsRegExp::compile(&mut self.gc_heap, pattern_utf16, flags).map_err(|e| {
                VmError::InvalidRegExp {
                    message: e.to_string(),
                }
            })?;
        write_register(frame, dst, Value::RegExp(regex))?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_array_push_regs(
        &mut self,
        frame: &mut Frame,
        arr_reg: u16,
        value_reg: u16,
    ) -> Result<(), VmError> {
        let value = read_register(frame, value_reg)?.clone();
        let array = match read_register(frame, arr_reg)? {
            Value::Array(a) => *a,
            _ => return Err(VmError::TypeMismatch),
        };
        crate::array::push(array, &mut self.gc_heap, value)?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_new_weak_ref_regs(
        &mut self,
        frame: &mut Frame,
        dst: u16,
        target_reg: u16,
    ) -> Result<(), VmError> {
        let target = read_register(frame, target_reg)?.clone();
        let weak_ref = crate::weak_refs::alloc_weak_ref(&mut self.gc_heap, &target)?;
        write_register(frame, dst, Value::WeakRef(weak_ref))?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_new_finalization_registry_regs(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        callback_reg: u16,
    ) -> Result<(), VmError> {
        let callback = read_register(frame, callback_reg)?.clone();
        let registry = crate::weak_refs::alloc_finalization_registry_with_context(
            &mut self.gc_heap,
            callback,
            Some(context.clone()),
        )?;
        write_register(frame, dst, Value::FinalizationRegistry(registry))?;
        frame.pc += 1;
        Ok(())
    }
}
