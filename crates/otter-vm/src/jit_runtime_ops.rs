//! Typed runtime operations used by baseline JIT slow paths.
//!
//! # Contents
//! Narrow entry points for fixed-operand operations whose full ECMAScript
//! semantics still belong to the VM: coercing arithmetic, captured-binding
//! checks, constant materialization, descriptor writes, and loose equality.
//!
//! # Invariants
//! - Every operand is decoded by the compiler and passed explicitly. These
//!   functions never receive a byte PC or decode a `CodeBlockInstruction`.
//! - Frame slots remain the canonical moving-GC roots across allocating or
//!   throwing operations.
//! - The compiled frame's instruction PC is preserved; advancing dispatch is
//!   the interpreter caller's responsibility, not the JIT ABI's.
//!
//! # See also
//! - `crate::property_dispatch` for typed property and element slow paths.
//! - `otter-jit::baseline` for the machine-code stubs calling these operations.

use crate::{ExecutionContext, Interpreter, VmError, holt_stack::HoltStack, write_register};

impl Interpreter {
    /// Execute generic ECMAScript addition from decoded register operands.
    pub fn jit_runtime_add(
        &mut self,
        stack: &mut HoltStack,
        frame_index: usize,
        dst: u16,
        lhs: u16,
        rhs: u16,
    ) -> Result<(), VmError> {
        let saved_pc = stack[frame_index].pc;
        let result = self.run_add_regs(&mut stack[frame_index], dst, lhs, rhs);
        stack[frame_index].pc = saved_pc;
        result
    }

    /// Store a captured binding after enforcing its TDZ check.
    pub fn jit_runtime_store_upvalue_checked(
        &mut self,
        stack: &mut HoltStack,
        frame_index: usize,
        src: u16,
        idx: i32,
    ) -> Result<(), VmError> {
        let saved_pc = stack[frame_index].pc;
        let result = self.run_store_upvalue_checked_reg(&mut stack[frame_index], src, idx);
        stack[frame_index].pc = saved_pc;
        result
    }

    /// Materialize a string constant from the owning function's constant pool.
    pub fn jit_runtime_load_string(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        function_id: u32,
        dst: u16,
        constant_index: u32,
    ) -> Result<(), VmError> {
        let resolved = context
            .for_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let value = self.load_string_constant_value(&resolved, constant_index)?;
        write_register(&mut stack[frame_index], dst, value)
    }

    /// Define one object-literal data property from decoded registers.
    pub fn jit_runtime_define_data_property(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        object: u16,
        key: u16,
        value: u16,
    ) -> Result<(), VmError> {
        self.run_define_data_property_regs(context, stack, frame_index, object, key, value)
    }

    /// Replace one loop-captured upvalue cell with a fresh TDZ cell.
    pub fn jit_runtime_fresh_upvalue(
        &mut self,
        stack: &mut HoltStack,
        frame_index: usize,
        idx: i32,
    ) -> Result<(), VmError> {
        let saved_pc = stack[frame_index].pc;
        let result = self.run_fresh_upvalue_reg(&mut stack[frame_index], idx);
        stack[frame_index].pc = saved_pc;
        result
    }

    /// Load one realm builtin error constructor from a decoded constant index.
    pub fn jit_runtime_load_builtin_error(
        &self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        dst: u16,
        kind_index: u32,
    ) -> Result<(), VmError> {
        let saved_pc = stack[frame_index].pc;
        let result =
            self.run_load_builtin_error_reg(context, &mut stack[frame_index], dst, kind_index);
        stack[frame_index].pc = saved_pc;
        result
    }

    /// Execute generic ECMAScript unary negation from decoded registers.
    pub fn jit_runtime_neg(
        &mut self,
        stack: &mut HoltStack,
        frame_index: usize,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let saved_pc = stack[frame_index].pc;
        let result = self.run_neg_regs(&mut stack[frame_index], dst, src);
        stack[frame_index].pc = saved_pc;
        result
    }

    /// Apply a descriptor object through `OrdinaryDefineOwnProperty`.
    pub fn jit_runtime_define_own_property(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        target: u16,
        key: u16,
        descriptor: u16,
    ) -> Result<(), VmError> {
        let saved_pc = stack[frame_index].pc;
        let result =
            self.run_define_own_property_regs(context, stack, frame_index, target, key, descriptor);
        stack[frame_index].pc = saved_pc;
        result
    }

    /// Allocate a closure from decoded function and parent-upvalue indices.
    pub fn jit_runtime_make_closure(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        function_id: u32,
        dst: u16,
        function_index: u32,
        parent_indices: &[u32],
    ) -> Result<(), VmError> {
        let resolved = context
            .for_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let saved_pc = stack[frame_index].pc;
        let result = self.run_make_closure_regs(
            &resolved,
            &mut stack[frame_index],
            dst,
            function_index,
            parent_indices,
        );
        stack[frame_index].pc = saved_pc;
        result
    }

    /// Execute a guarded `Math` call from decoded argument registers.
    #[allow(clippy::too_many_arguments)]
    pub fn jit_runtime_math_call(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        dst: u16,
        method_id: u32,
        argument_regs: &[u16],
    ) -> Result<(), VmError> {
        let saved_pc = stack[frame_index].pc;
        let result = self.do_math_call_regs(
            stack,
            context,
            frame_index,
            dst,
            method_id,
            argument_regs,
            false,
        );
        stack[frame_index].pc = saved_pc;
        result
    }
}
