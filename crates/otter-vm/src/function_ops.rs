//! Function and closure construction opcode helpers.
//!
//! Keep callable value construction out of the main interpreter file while
//! preserving the compact executable operand path used by dispatch.
//!
//! # Contents
//! - Closure-less function value construction for `MakeFunction`.
//! - Captured-upvalue closure construction for variadic `MakeClosure`.
//! - Class constructor wrapper construction for `MakeClass`.
//! - `Function.prototype.bind` metadata and bound-function construction.
//!
//! # Invariants
//! - `MakeFunction` receives already-decoded executable operands.
//! - `MakeClosure` reads the executable operand slice because its upvalue list
//!   is variadic.
//! - Arrow closures snapshot the enclosing frame's `this` value at construction.
//!
//! # See also
//! - [`crate::executable`]
//! - [`crate::Frame`]

use otter_bytecode::Operand;
use smallvec::SmallVec;

use crate::{
    BindMetadataGet, BoundFunction, ClassConstructor, ExecutionContext, Frame, Interpreter,
    PendingBindFunction, PendingBindStage, UpvalueCell, Value, VmError, function_metadata,
    operand_decode::{const_operand, register_operand},
    read_register, write_register,
};

impl Interpreter {
    pub(crate) fn run_make_function_reg(
        &self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        idx: u32,
    ) -> Result<(), VmError> {
        let function_id = context
            .function_id_constant(idx)
            .ok_or(VmError::InvalidOperand)?;
        write_register(frame, dst, Value::Function { function_id })?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_make_closure_operands(
        &self,
        context: &ExecutionContext,
        frame: &mut Frame,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let idx = const_operand(operands.get(1))?;
        let function_id = context
            .function_id_constant(idx)
            .ok_or(VmError::InvalidOperand)?;
        let count = match operands.get(2) {
            Some(&Operand::ConstIndex(n)) => n as usize,
            _ => return Err(VmError::InvalidOperand),
        };
        let mut cells: Vec<UpvalueCell> = Vec::with_capacity(count);
        for i in 0..count {
            let parent_idx = match operands.get(3 + i) {
                Some(&Operand::Imm32(n)) if n >= 0 => n as usize,
                _ => return Err(VmError::InvalidOperand),
            };
            let cell = *frame
                .upvalues
                .get(parent_idx)
                .ok_or(VmError::InvalidOperand)?;
            cells.push(cell);
        }
        let upvalues: std::rc::Rc<[UpvalueCell]> = std::rc::Rc::from(cells);
        // Arrow-closure receivers are bound lexically: every later invocation
        // ignores the call site and uses the enclosing frame's `this`.
        let bound_this = if context.function_is_arrow(function_id) {
            Some(Box::new(frame.this_value.clone()))
        } else {
            None
        };
        write_register(
            frame,
            dst,
            Value::Closure {
                function_id,
                upvalues,
                bound_this,
            },
        )?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_make_class_regs(
        &mut self,
        frame: &mut Frame,
        dst: u16,
        ctor_reg: u16,
        proto_reg: u16,
        statics_reg: u16,
    ) -> Result<(), VmError> {
        let ctor = read_register(frame, ctor_reg)?.clone();
        if !self.is_callable_runtime(&ctor) {
            return Err(VmError::NotCallable);
        }
        let prototype = match read_register(frame, proto_reg)? {
            Value::Object(o) => *o,
            _ => return Err(VmError::TypeMismatch),
        };
        let statics = match read_register(frame, statics_reg)? {
            Value::Object(o) => *o,
            _ => return Err(VmError::TypeMismatch),
        };
        let class = ClassConstructor::new(&mut self.gc_heap, ctor, prototype, statics)?;
        write_register(frame, dst, Value::ClassConstructor(class))?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn drive_bind_function(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let top_idx = stack.len() - 1;
        let pc = stack[top_idx].pc;
        let pending = stack[top_idx]
            .pending_bind_function
            .as_ref()
            .filter(|state| state.pc == pc && state.dst == dst)
            .cloned();
        if let Some(state) = pending {
            let produced = read_register(&stack[top_idx], dst)?.clone();
            return match state.stage {
                PendingBindStage::Name => self.continue_bind_function_after_name(
                    stack,
                    context,
                    dst,
                    state.target,
                    state.bound_this,
                    state.bound_args,
                    produced,
                ),
                PendingBindStage::Length => {
                    let target_name = state.target_name.ok_or(VmError::InvalidOperand)?;
                    stack[top_idx].pending_bind_function = None;
                    self.finish_bind_function(
                        stack,
                        dst,
                        state.target,
                        state.bound_this,
                        state.bound_args,
                        target_name,
                        produced,
                    )
                }
            };
        }

        let callee_reg = register_operand(operands.get(1))?;
        let this_reg = register_operand(operands.get(2))?;
        let argc = match operands.get(3) {
            Some(&Operand::ConstIndex(n)) => n as usize,
            _ => return Err(VmError::InvalidOperand),
        };
        let target = read_register(&stack[top_idx], callee_reg)?.clone();
        if !self.is_callable_runtime(&target) {
            return Err(VmError::NotCallable);
        }
        let bound_this = read_register(&stack[top_idx], this_reg)?.clone();
        let mut bound_args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
        for i in 0..argc {
            let r = register_operand(operands.get(4 + i))?;
            bound_args.push(read_register(&stack[top_idx], r)?.clone());
        }
        match self.callable_bind_metadata_get(context, &target, "name")? {
            BindMetadataGet::Value(target_name) => self.continue_bind_function_after_name(
                stack,
                context,
                dst,
                target,
                bound_this,
                bound_args,
                target_name,
            ),
            BindMetadataGet::Getter(getter) => {
                stack[top_idx].pending_bind_function = Some(PendingBindFunction {
                    pc,
                    dst,
                    target: target.clone(),
                    bound_this,
                    bound_args,
                    stage: PendingBindStage::Name,
                    target_name: None,
                });
                self.invoke(stack, context, &getter, target, SmallVec::new(), dst)
            }
        }
    }

    fn continue_bind_function_after_name(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        dst: u16,
        target: Value,
        bound_this: Value,
        bound_args: SmallVec<[Value; 4]>,
        target_name: Value,
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        let pc = stack[top_idx].pc;
        match self.callable_bind_metadata_get(context, &target, "length")? {
            BindMetadataGet::Value(target_length) => {
                stack[top_idx].pending_bind_function = None;
                self.finish_bind_function(
                    stack,
                    dst,
                    target,
                    bound_this,
                    bound_args,
                    target_name,
                    target_length,
                )
            }
            BindMetadataGet::Getter(getter) => {
                stack[top_idx].pending_bind_function = Some(PendingBindFunction {
                    pc,
                    dst,
                    target: target.clone(),
                    bound_this,
                    bound_args,
                    stage: PendingBindStage::Length,
                    target_name: Some(target_name),
                });
                self.invoke(stack, context, &getter, target, SmallVec::new(), dst)
            }
        }
    }

    fn finish_bind_function(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        dst: u16,
        target: Value,
        bound_this: Value,
        bound_args: SmallVec<[Value; 4]>,
        target_name: Value,
        target_length: Value,
    ) -> Result<(), VmError> {
        let metadata = function_metadata::bound_create_metadata_from_values(
            &target_name,
            &target_length,
            bound_args.len(),
        );
        let bound = BoundFunction::new_with_metadata(
            &mut self.gc_heap,
            target,
            bound_this,
            bound_args,
            metadata,
        )?;
        let top_idx = stack.len() - 1;
        stack[top_idx].pending_bind_function = None;
        write_register(&mut stack[top_idx], dst, Value::BoundFunction(bound))?;
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        Ok(())
    }
}
