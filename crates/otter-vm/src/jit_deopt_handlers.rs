//! Static catch-handler reconstruction for exact JIT deoptimization.
//!
//! # Contents
//! - [`StaticCatchHandlerPlan`] — validated outer-to-inner catch-only state for
//!   one function/resume PC.
//! - [`Interpreter::jit_rebuild_materialized_catch_handlers`] — the public JIT
//!   boundary for an existing interpreter-owned activation.
//! - Internal prepare/install helpers shared by generated stack-call and
//!   spliced-frame materialization.
//!
//! # Invariants
//! - Reconstruction replaces only [`crate::cold_frame::ColdFrame::handlers`];
//!   constructor, argument, eval, and derived-`this` state remains untouched.
//! - A `finally`, catchless region, suspension owner, pending protocol ladder,
//!   parked completion, or iterator-close region is a structural
//!   [`crate::VmError::InvalidOperand`], never a JavaScript throw.
//! - Handler order is outermost to innermost, exactly matching interpreter
//!   `EnterTry` push order. The resume PC is the next instruction to execute.
//! - Catch PCs and exception registers come only from the schema-verified
//!   [`crate::CodeBlock`], never from deopt metadata or generated operands.
//! - Plans are fully validated before a handler stack is mutated.
//!
//! # See also
//! - [`crate::CodeBlockControlFlowView::active_catch_regions`]
//! - `interp/jit_call.rs`

use smallvec::SmallVec;

use crate::{ActivationStack, ExecutionContext, Frame, Interpreter, TryHandler, VmError};

/// Validated catch-only state for one exact deopt resume point.
pub(crate) struct StaticCatchHandlerPlan {
    function_id: u32,
    handlers: SmallVec<[TryHandler; 4]>,
}

impl Interpreter {
    /// Validate the static catch stack for `function_id` at `resume_pc` without
    /// touching a frame or allocating a cold sidecar.
    pub(crate) fn jit_prepare_static_catch_handlers(
        context: &ExecutionContext,
        function_id: u32,
        resume_pc: u32,
    ) -> Result<StaticCatchHandlerPlan, VmError> {
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        if function.id != function_id || function.instr_at_index(resume_pc as usize).is_none() {
            return Err(VmError::InvalidOperand);
        }

        let mut handlers = SmallVec::new();
        let regions = function
            .control_flow()
            .active_catch_regions(resume_pc)
            .map_err(|_| VmError::InvalidOperand)?;
        for region in regions {
            let catch_pc = region.catch_pc.ok_or(VmError::InvalidOperand)?;
            debug_assert!(function.instr_at_index(catch_pc as usize).is_some());
            debug_assert!(region.exception_register < function.register_count);
            if function.instr_at_index(catch_pc as usize).is_none()
                || region.exception_register >= function.register_count
            {
                return Err(VmError::InvalidOperand);
            }
            handlers.push(TryHandler {
                catch_pc: Some(catch_pc),
                finally_pc: None,
                exc_register: region.exception_register,
            });
        }

        Ok(StaticCatchHandlerPlan {
            function_id,
            handlers,
        })
    }

    /// Install a fully validated plan into `frame`, changing no other cold
    /// state. This consumes the plan so no partial prefix can be installed.
    pub(crate) fn jit_install_static_catch_handlers(
        &mut self,
        frame: &mut Frame,
        plan: StaticCatchHandlerPlan,
    ) -> Result<(), VmError> {
        if frame.function_id != plan.function_id {
            return Err(VmError::InvalidOperand);
        }
        if self
            .frame_cold(frame)
            .is_some_and(|cold| !cold.supports_static_catch_rebuild())
        {
            return Err(VmError::InvalidOperand);
        }

        if let Some(cold) = self.frame_cold_mut(frame) {
            cold.handlers = plan.handlers;
        } else if !plan.handlers.is_empty() {
            self.frame_ensure_cold(frame).handlers = plan.handlers;
        }
        Ok(())
    }

    /// Rebuild the exact catch-only handler stack for one existing
    /// interpreter-owned activation about to resume after Machine deopt.
    ///
    /// The caller still owns register and PC writeback. This boundary only
    /// validates static control flow and replaces `cold.handlers`, preserving
    /// every supported sibling field verbatim.
    pub fn jit_rebuild_materialized_catch_handlers(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        resume_pc: u32,
    ) -> Result<(), VmError> {
        let function_id = stack
            .get(frame_index)
            .ok_or(VmError::InvalidOperand)?
            .function_id;
        let plan = Self::jit_prepare_static_catch_handlers(context, function_id, resume_pc)?;
        let frame = stack.get_mut(frame_index).ok_or(VmError::InvalidOperand)?;
        self.jit_install_static_catch_handlers(frame, plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BytecodeModule, Value, cold_frame::ParkedFinally};
    use otter_bytecode::{Function, Instruction, Op, Operand, SourceKind};

    fn catch_function(id: u32) -> Function {
        Function {
            id,
            name: format!("catch_{id}"),
            scratch: 2,
            code: vec![
                Instruction {
                    pc: 0,
                    op: Op::EnterTry,
                    operands: vec![
                        Operand::Imm32(3),
                        Operand::Imm32(otter_bytecode::NO_HANDLER_OFFSET),
                        Operand::Register(1),
                    ],
                },
                Instruction {
                    pc: 1,
                    op: Op::Throw,
                    operands: vec![Operand::Register(0)],
                },
                Instruction {
                    pc: 2,
                    op: Op::LeaveTry,
                    operands: Vec::new(),
                },
                Instruction {
                    pc: 3,
                    op: Op::Jump,
                    operands: vec![Operand::Imm32(1)],
                },
                Instruction {
                    pc: 4,
                    op: Op::Return,
                    operands: vec![Operand::Register(1)],
                },
                Instruction {
                    pc: 5,
                    op: Op::ReturnUndefined,
                    operands: Vec::new(),
                },
            ]
            .into(),
            ..Function::default()
        }
    }

    fn context(functions: Vec<Function>) -> ExecutionContext {
        ExecutionContext::from_module(BytecodeModule {
            module: "deopt-handler-test.js".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::JavaScript,
            functions,
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        })
    }

    #[test]
    fn materialized_rebuild_replaces_only_handlers() {
        let function = catch_function(0);
        let context = context(vec![function.clone()]);
        let mut interpreter = Interpreter::new();
        let mut stack = ActivationStack::new();
        let mut frame = interpreter
            .test_frame_for_function(&function)
            .expect("frame");
        {
            let cold = interpreter.frame_ensure_cold(&mut frame);
            cold.new_target = Some(Value::function(19));
            cold.rest_args.push(Value::number_i32(23));
            cold.incoming_args.push(Value::number_i32(29));
            cold.is_derived_constructor = true;
            cold.handlers.push(TryHandler {
                catch_pc: Some(99),
                finally_pc: None,
                exc_register: 0,
            });
        }
        stack.push(frame);

        interpreter
            .jit_rebuild_materialized_catch_handlers(&context, &mut stack, 0, 1)
            .expect("catch reconstruction");
        let cold = interpreter.frame_cold(&stack[0]).expect("preserved cold");
        assert_eq!(cold.new_target, Some(Value::function(19)));
        assert_eq!(cold.rest_args.as_slice(), &[Value::number_i32(23)]);
        assert_eq!(cold.incoming_args.as_slice(), &[Value::number_i32(29)]);
        assert!(cold.is_derived_constructor);
        assert_eq!(cold.handlers.len(), 1);
        assert_eq!(cold.handlers[0].catch_pc, Some(4));
        assert_eq!(cold.handlers[0].finally_pc, None);
        assert_eq!(cold.handlers[0].exc_register, 1);
        assert_eq!(stack[0].pc, 0, "handler-only helper must not own PC");

        interpreter
            .jit_rebuild_materialized_catch_handlers(&context, &mut stack, 0, 4)
            .expect("catch body has no active handler");
        let cold = interpreter.frame_cold(&stack[0]).expect("preserved cold");
        assert!(cold.handlers.is_empty());
        assert_eq!(cold.new_target, Some(Value::function(19)));
        assert_eq!(cold.rest_args.as_slice(), &[Value::number_i32(23)]);
    }

    #[test]
    fn materialized_rebuild_rejects_parked_control_state_without_mutation() {
        let function = catch_function(0);
        let context = context(vec![function.clone()]);
        let mut interpreter = Interpreter::new();
        let mut stack = ActivationStack::new();
        let mut frame = interpreter
            .test_frame_for_function(&function)
            .expect("frame");
        {
            let cold = interpreter.frame_ensure_cold(&mut frame);
            cold.handlers.push(TryHandler {
                catch_pc: Some(99),
                finally_pc: None,
                exc_register: 0,
            });
            cold.parked_finally.push((ParkedFinally::Normal, 0));
        }
        stack.push(frame);

        assert!(matches!(
            interpreter.jit_rebuild_materialized_catch_handlers(&context, &mut stack, 0, 1),
            Err(VmError::InvalidOperand)
        ));
        let cold = interpreter.frame_cold(&stack[0]).expect("cold retained");
        assert_eq!(cold.handlers.len(), 1);
        assert_eq!(cold.handlers[0].catch_pc, Some(99));
        assert_eq!(cold.parked_finally.len(), 1);
    }
}
