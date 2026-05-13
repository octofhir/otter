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
use smallvec::SmallVec;

use crate::{
    ExecutionContext, Frame, Interpreter, Value, VmError,
    operand_decode::{const_operand, register_operand},
    read_register, regexp, write_register,
};

impl Interpreter {
    pub(crate) fn run_new_object_reg(
        &mut self,
        frame: &mut Frame,
        dst: u16,
    ) -> Result<(), VmError> {
        let proto = self.object_prototype_object_opt();
        let obj = crate::object::alloc_object(&mut self.gc_heap)?;
        if let Some(proto) = proto {
            crate::object::set_prototype(obj, &mut self.gc_heap, Some(proto));
        }
        write_register(frame, dst, Value::Object(obj))?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_new_array_operands(
        &mut self,
        frame: &mut Frame,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let count = const_operand(operands.get(1))? as usize;
        let mut elements: SmallVec<[Value; 4]> = SmallVec::with_capacity(count);
        for i in 0..count {
            let r = register_operand(operands.get(2 + i))?;
            elements.push(read_register(frame, r)?.clone());
        }
        let array = crate::array::from_elements(&mut self.gc_heap, elements)?;
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
