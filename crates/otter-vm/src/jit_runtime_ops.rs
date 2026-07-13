//! Typed runtime operations used by baseline JIT slow paths.
//!
//! # Contents
//! Narrow entry points for fixed-operand operations whose full ECMAScript
//! semantics still belong to the VM: the complete numeric family, coercing
//! arithmetic, captured-binding checks, constant materialization, descriptor
//! writes, and loose equality.
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
//! - `otter-jit::template` for the machine-code stubs calling these operations.

use crate::{ExecutionContext, Interpreter, VmError, holt_stack::HoltStack, write_register};
use otter_bytecode::Op;

impl Interpreter {
    /// Complete a numeric, bitwise, update, or relational opcode from decoded
    /// operands. This is the single compiled slow transition for the numeric
    /// family; it calls the same register helpers as interpreter dispatch and
    /// restores the interpreter PC because the native frame owns canonical
    /// compiled progress.
    #[allow(clippy::too_many_arguments)]
    pub fn jit_runtime_numeric_op(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        dst: u16,
        lhs: u16,
        rhs_or_delta: u64,
        opcode: u8,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        let saved_pc = stack[frame_index].pc;
        let result = {
            let frame = &mut stack[frame_index];
            match opcode {
                x if x == Op::Sub as u8 => self.run_numeric_regs(
                    frame,
                    dst,
                    lhs,
                    rhs_or_delta as u16,
                    crate::number::sub,
                    crate::arithmetic_dispatch::bigint_sub_op,
                    None,
                ),
                x if x == Op::Mul as u8 => self.run_numeric_regs(
                    frame,
                    dst,
                    lhs,
                    rhs_or_delta as u16,
                    crate::number::mul,
                    crate::arithmetic_dispatch::bigint_mul_op,
                    None,
                ),
                x if x == Op::Div as u8 => self.run_numeric_regs(
                    frame,
                    dst,
                    lhs,
                    rhs_or_delta as u16,
                    crate::number::div,
                    crate::bigint::ops::div,
                    None,
                ),
                x if x == Op::Rem as u8 => self.run_numeric_regs(
                    frame,
                    dst,
                    lhs,
                    rhs_or_delta as u16,
                    crate::number::rem,
                    crate::bigint::ops::rem,
                    None,
                ),
                x if x == Op::Pow as u8 => self.run_numeric_regs(
                    frame,
                    dst,
                    lhs,
                    rhs_or_delta as u16,
                    crate::number::pow,
                    crate::bigint::ops::pow,
                    None,
                ),
                x if x == Op::BitwiseAnd as u8 => self.run_numeric_regs(
                    frame,
                    dst,
                    lhs,
                    rhs_or_delta as u16,
                    crate::number::bitwise_and,
                    crate::arithmetic_dispatch::bigint_and_op,
                    None,
                ),
                x if x == Op::BitwiseOr as u8 => self.run_numeric_regs(
                    frame,
                    dst,
                    lhs,
                    rhs_or_delta as u16,
                    crate::number::bitwise_or,
                    crate::arithmetic_dispatch::bigint_or_op,
                    None,
                ),
                x if x == Op::BitwiseXor as u8 => self.run_numeric_regs(
                    frame,
                    dst,
                    lhs,
                    rhs_or_delta as u16,
                    crate::number::bitwise_xor,
                    crate::arithmetic_dispatch::bigint_xor_op,
                    None,
                ),
                x if x == Op::Shl as u8 => self.run_numeric_regs(
                    frame,
                    dst,
                    lhs,
                    rhs_or_delta as u16,
                    crate::number::shl,
                    crate::bigint::ops::shl,
                    None,
                ),
                x if x == Op::Shr as u8 => self.run_numeric_regs(
                    frame,
                    dst,
                    lhs,
                    rhs_or_delta as u16,
                    crate::number::shr_arith,
                    crate::bigint::ops::shr,
                    None,
                ),
                x if x == Op::Ushr as u8 => {
                    self.run_ushr_regs(frame, dst, lhs, rhs_or_delta as u16, None)
                }
                x if x == Op::LessThan as u8 => {
                    self.run_compare_regs(frame, dst, lhs, rhs_or_delta as u16, Op::LessThan, None)
                }
                x if x == Op::LessEq as u8 => {
                    self.run_compare_regs(frame, dst, lhs, rhs_or_delta as u16, Op::LessEq, None)
                }
                x if x == Op::GreaterThan as u8 => self.run_compare_regs(
                    frame,
                    dst,
                    lhs,
                    rhs_or_delta as u16,
                    Op::GreaterThan,
                    None,
                ),
                x if x == Op::GreaterEq as u8 => {
                    self.run_compare_regs(frame, dst, lhs, rhs_or_delta as u16, Op::GreaterEq, None)
                }
                x if x == Op::Neg as u8 => self.run_neg_regs(frame, dst, lhs),
                x if x == Op::BitwiseNot as u8 => self.run_bitwise_not_regs(frame, dst, lhs),
                x if x == Op::Increment as u8 => self.run_increment_regs(
                    context,
                    frame,
                    dst,
                    lhs,
                    rhs_or_delta as u32 as i32,
                    None,
                ),
                _ => Err(VmError::InvalidOperand),
            }
        };
        stack[frame_index].pc = saved_pc;
        result
    }

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
        let result = self.run_add_regs(&mut stack[frame_index], dst, lhs, rhs, None);
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
        // `kind_index` is a constant-pool index of the COMPILED function's
        // chunk; in a multi-script runtime the ambient context may belong to
        // a different chunk, so resolve the owner before decoding.
        let function_id = stack[frame_index].function_id;
        let resolved = context
            .for_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let saved_pc = stack[frame_index].pc;
        let result =
            self.run_load_builtin_error_reg(&resolved, &mut stack[frame_index], dst, kind_index);
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
