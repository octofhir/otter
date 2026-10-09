//! Exact interpreter resumption after generated side exits.
//!
//! # Contents
//! - Physical deopt prepares the existing frame for an assembly continuation.
//! - Inline deopt restores the outer frame and prepares missing descendants.
//!
//! # Invariants
//! The physical outer frame and its windows stay published throughout deopt.
//! Its tier changes in place, preserving actuals, SELF, constructor bindings
//! and arguments identity. Only inlined descendants require new native extents;
//! their owned inputs transfer through the common trampoline. A restored frame
//! waiting on a restored callee stands at its call instruction and moves past
//! it when the callee returns. A committed source operation is never replayed,
//! and dispatch stays above its caller floor.
//!
//! # See also
//! - [`crate::active_frame`] for checked frame access.
//! - [`crate::deopt::DeoptFrame`] for inline reconstruction recipes.
//! - [`crate::interp::call_dispatch`] for native resumption.

use crate::{
    ActivationStack, ActiveFrameRef, ExecutionContext, Frame, Interpreter, Value, VmError,
    native_abi::{NativeFrameFlags, SideExit},
};

impl Interpreter {
    /// Restore interpreter ownership without executing JS.
    /// The native deopt entry resumes this frame after this method returns.
    pub(crate) fn prepare_deoptimized_frame(
        &mut self,
        context: &ExecutionContext,
        native: &mut Frame,
    ) -> Result<(), VmError> {
        // SAFETY: generated code keeps the original Frame and both
        // windows initialized, published, and stable across this cold call.
        let active =
            unsafe { ActiveFrameRef::from_ptr(native) }.map_err(|_| VmError::InvalidOperand)?;
        let function = context
            .exec_function(native.header.function_id)
            .ok_or(VmError::InvalidOperand)?;
        let register_count = active.register_count();
        if register_count != usize::from(function.register_count) {
            return Err(VmError::InvalidOperand);
        }

        if !native.enter_interpreter() {
            return Err(VmError::InvalidOperand);
        }
        Ok(())
    }

    /// Prepare restored inline descendants for assembly-owned continuation.
    ///
    /// `frames` is ordered outermost first. The outermost completion returns to
    /// compiled code; every younger frame returns through its recorded parent
    /// destination. This owned reconstruction is reserved for inlined
    /// optimized exits that cannot resume one canonical native activation.
    pub fn prepare_inline_deopt_frames(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        native: &mut Frame,
        frames: &[crate::deopt::DeoptFrame<Value>],
        exit: SideExit,
    ) -> Result<(), VmError> {
        // Assembly will complete this restored chain without another bail, so
        // this preparation is the only place that charges the optimizing tier's
        // bounded reoptimization budget. An installed generation that keeps
        // exiting is discarded on the same rule as every other entry path.
        let outermost = frames.first().ok_or(VmError::InvalidOperand)?;
        let owner = context
            .for_function(outermost.function_id)
            .map_err(|_| VmError::InvalidOperand)?;
        let context = &*owner;
        if native.header.function_id != outermost.function_id {
            return Err(VmError::InvalidOperand);
        }
        let resume_pcs = frames
            .iter()
            .enumerate()
            .map(|(index, frame)| {
                let owner = context
                    .for_function(frame.function_id)
                    .map_err(|_| VmError::InvalidOperand)?;
                let function = owner
                    .exec_function(frame.function_id)
                    .ok_or(VmError::InvalidOperand)?;
                if frame.entry.is_some() != (index != 0)
                    || frame.register_count != function.register_count
                    || frame
                        .slots
                        .iter()
                        .any(|&(register, _)| register >= frame.register_count)
                    || frame.entry.is_some_and(|entry| {
                        entry.return_register >= frames[index - 1].register_count
                    })
                {
                    return Err(VmError::InvalidOperand);
                }
                if frame.entry.is_some_and(|entry| {
                    !entry.new_target.is_undefined()
                        && !function.is_arrow
                        && !function.is_derived_constructor
                        && entry.this.as_object().is_none()
                }) {
                    return Err(VmError::InvalidOperand);
                }
                (0..function.code.len())
                    .find(|&pc| function.instruction_byte_pc(pc) == Some(frame.byte_pc))
                    .and_then(|pc| u32::try_from(pc).ok())
                    .ok_or(VmError::InvalidOperand)
            })
            .collect::<Result<Vec<_>, _>>()?;
        // SAFETY: the entry remains published until this continuation returns;
        // copying its windows below performs only host allocation, never GC.
        // The speculation that exited lives in the innermost spliced body, so
        // its site is the one an arithmetic exit widens.
        let (site_fid, site_pc) = match (frames.last(), resume_pcs.last()) {
            (Some(innermost), Some(&pc)) => (innermost.function_id, pc),
            _ => (outermost.function_id, exit.logical_pc()),
        };
        self.note_jit_optimized_bail_at(
            context,
            outermost.function_id,
            u64::from(native.code_object_id),
            exit,
            (site_fid, site_pc),
            false,
        );
        // The outer recipe writes back into its existing physical activation.
        // Descendants are owned entry inputs until assembly creates their extents.
        // Every frame but the innermost waits on its restored callee: its
        // recipe resumes after the call instruction, and it stands at that
        // instruction until the callee returns.
        let waiting = |index: usize| index + 1 < frames.len();
        let standing_pc = |index: usize| -> Result<u32, VmError> {
            if waiting(index) {
                resume_pcs[index]
                    .checked_sub(1)
                    .ok_or(VmError::InvalidOperand)
            } else {
                Ok(resume_pcs[index])
            }
        };
        native
            .registers
            .copy_from_slice(&outermost.dense(Value::undefined()));
        native.header.pc = standing_pc(0)?;
        if waiting(0) {
            native.header.flags = native
                .header
                .flags
                .with(NativeFrameFlags::ADVANCE_ON_RESUME);
        }
        if !native.enter_interpreter() {
            return Err(VmError::InvalidOperand);
        }
        let mut descendants = Vec::with_capacity(frames.len().saturating_sub(1));
        for index in 0..frames.len().saturating_sub(1) {
            // Plans exclude the physical outer activation.
            let deopt = &frames[index + 1];
            let entry = deopt.entry.ok_or(VmError::InvalidOperand)?;
            let owner = context
                .for_function(deopt.function_id)
                .map_err(|_| VmError::InvalidOperand)?;
            let function = owner
                .exec_function(deopt.function_id)
                .ok_or(VmError::InvalidOperand)?;
            let mut call = crate::PreparedCall::for_code_block(
                function,
                Some(entry.return_register),
                entry.closure,
                entry.this,
            );
            call.pc = standing_pc(index + 1)?;
            if waiting(index + 1) {
                call.header.flags = call.header.flags.with(NativeFrameFlags::ADVANCE_ON_RESUME);
            }
            let window = deopt.dense(Value::undefined());
            // The recorded actuals are the activation's incoming arguments;
            // a body whose arguments nothing reads rebuilds them from its
            // parameter registers.
            match &deopt.arguments {
                Some(arguments) => call.arguments.extend(arguments.iter().copied()),
                None => call.arguments.extend(
                    window
                        .iter()
                        .copied()
                        .take(usize::from(function.param_count)),
                ),
            }
            call.initial_registers.extend_from_slice(&window);
            call.set_new_target(entry.new_target);
            if !entry.new_target.is_undefined() {
                if function.is_derived_constructor {
                    call.set_derived_constructor();
                } else if !function.is_arrow {
                    call.set_construct();
                }
            }
            self.record_jit_debug_event(|| crate::JitDebugEvent::InlineDeoptFrame {
                index: (index + 1) as u32,
                total: frames.len() as u32,
                function_id: deopt.function_id,
                resume_pc: resume_pcs[index + 1],
            });
            descendants.push(call);
        }
        let mut child = None;
        for mut call in descendants.into_iter().rev() {
            call.child = child;
            child = Some(Box::new(call));
        }
        if let Some(child) = child {
            stack.push(*child);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BytecodeModule;
    use otter_bytecode::{Function, Instruction, Op, Operand, SourceKind};

    /// `catch (e) { return e }` around `throw r0`, after a call site at pc 0
    /// a waiting caller stands on.
    fn catch_function(id: u32) -> Function {
        Function {
            id,
            name: format!("deopt_catch_{id}"),
            scratch: 2,
            code: vec![
                Instruction {
                    pc: 0,
                    op: Op::Nop,
                    operands: Vec::new(),
                },
                Instruction {
                    pc: 1,
                    op: Op::Throw,
                    operands: vec![Operand::Register(0)],
                },
                Instruction {
                    pc: 2,
                    op: Op::ReturnUndefined,
                    operands: Vec::new(),
                },
                Instruction {
                    pc: 3,
                    op: Op::Return,
                    operands: vec![Operand::Register(1)],
                },
            ]
            .into(),
            handlers: vec![otter_bytecode::ExceptionHandler {
                start: 1,
                end: 2,
                target: 3,
                exception: 1,
            }],
            ..Function::default()
        }
    }

    fn context(functions: Vec<Function>) -> ExecutionContext {
        ExecutionContext::from_module(
            BytecodeModule {
                module: "deopt-materialization-test.js".to_string(),
                template_sites: Vec::new(),
                source_kind: SourceKind::JavaScript,
                functions,
                constants: Vec::new(),
                module_resolutions: Vec::new(),
                module_inits: Vec::new(),
                function_source: None,
            },
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("valid bytecode fixture")
    }

    #[test]
    fn every_spliced_frame_routes_throws_through_its_handler_table() {
        let context = context(vec![catch_function(0), catch_function(1)]);
        let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
        let mut stack = crate::test_support::FrameChainFixture::new();
        let thrown = Value::number_i32(73);
        let mut native = Frame::new(
            crate::native_abi::VmFrameHeader {
                function_id: 0,
                pc: 1,
                register_count: 2,
                kind: crate::native_abi::NativeFrameKind::Optimizing,
                flags: NativeFrameFlags::empty(),
            },
            0,
            Value::function(0),
            Value::undefined(),
        );
        let mut native_registers = [Value::undefined(); 2];
        native.registers = crate::RegisterWindow::attached(native_registers.as_mut_ptr(), 2);
        // SAFETY: the frame and initialized window remain stationary through resumption.
        unsafe { interpreter.jit_push_native_frame(&mut native).unwrap() };
        let frames = [
            crate::deopt::DeoptFrame::with_window(
                0,
                context
                    .exec_function(0)
                    .unwrap()
                    .instruction_byte_pc(1)
                    .unwrap(),
                None,
                [Value::undefined(), Value::undefined()],
            ),
            crate::deopt::DeoptFrame::with_window(
                1,
                context
                    .exec_function(1)
                    .unwrap()
                    .instruction_byte_pc(1)
                    .unwrap(),
                Some(crate::deopt::DeoptFrameEntry {
                    return_register: 0,
                    this: Value::undefined(),
                    closure: Value::function(1),
                    new_target: Value::undefined(),
                }),
                [thrown, Value::undefined()],
            ),
        ];

        let result = interpreter
            .with_runtime_turn(&mut stack, |turn| {
                let (interpreter, stack) = turn.into_parts();
                interpreter.prepare_inline_deopt_frames(
                    &context,
                    stack,
                    &mut native,
                    &frames,
                    SideExit::new(
                        1,
                        crate::native_abi::ExitReason::UnsupportedOperation,
                        crate::native_abi::ExitAction::Recompile,
                    ),
                )?;
                crate::test_support::resume_prepared_frame(
                    interpreter,
                    stack,
                    &context,
                    &mut native,
                )
            })
            .expect("both nested catches resume");
        interpreter.jit_pop_native_frame();
        assert_eq!(result, thrown);
        assert!(stack.is_empty());
    }

    #[test]
    fn inline_constructor_restores_new_target_and_receiver_result() {
        for returns_target in [false, true] {
            let context = context(
                (0..3)
                    .map(|id| Function {
                        id,
                        scratch: 1,
                        code: if id == 1 {
                            vec![
                                Instruction {
                                    pc: 0,
                                    op: Op::LoadNewTarget,
                                    operands: vec![Operand::Register(0)],
                                },
                                Instruction {
                                    pc: 1,
                                    op: if returns_target {
                                        Op::ReturnValue
                                    } else {
                                        Op::ReturnUndefined
                                    },
                                    operands: if returns_target {
                                        vec![Operand::Register(0)]
                                    } else {
                                        vec![]
                                    },
                                },
                            ]
                            .into()
                        } else {
                            // The waiting caller stands on its call site at
                            // pc 0 and returns the callee's result.
                            vec![
                                Instruction {
                                    pc: 0,
                                    op: Op::Nop,
                                    operands: Vec::new(),
                                },
                                Instruction {
                                    pc: 1,
                                    op: Op::ReturnValue,
                                    operands: vec![Operand::Register(0)],
                                },
                            ]
                            .into()
                        },
                        ..Function::default()
                    })
                    .collect(),
            );
            let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
            let mut stack = crate::test_support::FrameChainFixture::new();
            let mut registers = [Value::undefined()];
            let mut native = Frame::new(
                crate::native_abi::VmFrameHeader::interpreter(0, 1),
                registers.as_mut_ptr() as u64,
                Value::function(0),
                Value::undefined(),
            );

            // SAFETY: the frame and initialized window remain stationary through resumption.
            unsafe { vm.jit_push_native_frame(&mut native).unwrap() };
            vm.with_runtime_turn(&mut stack, |turn| {
                let (vm, stack) = turn.into_parts();
                let receiver =
                    Value::object(vm.alloc_runtime_rooted_object_with_roots(&[], &[]).unwrap());
                let frames = [
                    crate::deopt::DeoptFrame::with_window(
                        0,
                        context
                            .exec_function(0)
                            .unwrap()
                            .instruction_byte_pc(1)
                            .unwrap(),
                        None,
                        [Value::undefined()],
                    ),
                    crate::deopt::DeoptFrame::with_window(
                        1,
                        0,
                        Some(crate::deopt::DeoptFrameEntry {
                            return_register: 0,
                            this: receiver,
                            closure: Value::function(1),
                            new_target: Value::function(2),
                        }),
                        [Value::undefined()],
                    ),
                ];
                vm.prepare_inline_deopt_frames(
                    &context,
                    stack,
                    &mut native,
                    &frames,
                    SideExit::new(
                        0,
                        crate::native_abi::ExitReason::UnsupportedOperation,
                        crate::native_abi::ExitAction::Recompile,
                    ),
                )
                .unwrap();
                let result =
                    crate::test_support::resume_prepared_frame(vm, stack, &context, &mut native)
                        .unwrap();
                assert_eq!(
                    result,
                    if returns_target {
                        Value::function(2)
                    } else {
                        receiver
                    }
                );
            });
            vm.jit_pop_native_frame();
            assert!(stack.is_empty());
        }
    }

    #[test]
    fn generated_stack_frame_resumes_into_its_handler_table() {
        let context = context(vec![catch_function(0)]);
        let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
        let mut stack = crate::test_support::FrameChainFixture::new();
        let thrown = Value::number_i32(91);
        let mut registers = [thrown, Value::undefined()];
        let mut native = Frame::new(
            crate::native_abi::VmFrameHeader {
                function_id: 0,
                pc: 1,
                register_count: 2,
                kind: crate::native_abi::NativeFrameKind::Optimizing,
                flags: NativeFrameFlags::empty(),
            },
            registers.as_mut_ptr() as u64,
            Value::function(0),
            Value::undefined(),
        );

        // SAFETY: this fixture keeps `native` and its register window live and
        // stationary through the complete rooted deopt transaction below.
        unsafe {
            interpreter
                .jit_push_native_frame(&mut native)
                .expect("publish generated frame");
        }

        let result = interpreter
            .with_runtime_turn(&mut stack, |turn| {
                let (interpreter, stack) = turn.into_parts();
                interpreter.prepare_deoptimized_frame(&context, &mut native)?;
                crate::test_support::resume_prepared_frame(
                    interpreter,
                    stack,
                    &context,
                    &mut native,
                )
            })
            .expect("stack-call catch resumes");
        interpreter.jit_pop_native_frame();
        assert_eq!(result, thrown);
        assert!(stack.is_empty());
    }
}
