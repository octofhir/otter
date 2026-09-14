//! Cold materialization for JIT side exits.
//!
//! Native execution normally keeps the canonical [`crate::native_abi::NativeFrame`] and its
//! register window intact. This module is the narrow exception: a generated
//! stack-owned call or nested inline exit copies/materializes an
//! [`crate::ActivationStack`] frame only after the side exit has fired.
//!
//! # Contents
//! - [`Interpreter::jit_deopt_materialize_stack_call`] — standard generated
//!   stack-call deopt, including exact-generation diagnostics and policy.
//! - [`Interpreter::jit_deopt_materialize_inline_frames`] — nested inline deopt.
//!
//! # Invariants
//! - These APIs are cold side-exit operations, never normal call entry or a
//!   native-to-interpreter tier transition.
//! - Register values are attached/copied exactly once after the side exit.
//! - The direct-eval environment is moved exactly once from the published
//!   native frame into the fully published outer materialized frame.
//! - Inline chains return a complete result for materialized and stack-owned
//!   native entries alike; their published outer frame retains its GC lifetime.
//! - Every rebuilt frame receives the exact static catch-only handler stack for
//!   its resume PC before interpreter dispatch can observe it.
//! - The outer inline continuation retains the live activation's actuals,
//!   captured cells, arguments object and constructor bindings. Descendants
//!   use the same decoded frame schema, including their own or lexical new.target.
//!   A lexical arrow binding never enables constructor result substitution.
//! - Every temporary materialized frame and register window is removed before
//!   returning to compiled code.
//! - Nested dispatch stops at the caller's activation floor and reuses the
//!   already-published runtime-turn root provider.
//! - A stack-owned frame stays published while copying and dispatching, so its
//!   machine-stack values remain precise roots until generated code unpublishes
//!   the activation after this API returns.
//! - Generated code owns synchronous-depth and native-stack-byte counters.
//!   Stack-call deopt neither enters nor leaves synchronous re-entry. Its
//!   logical-depth slot is transferred temporarily to the materialized frame
//!   so interpreter stack checks count native outer frames exactly once.
//! - New interpreter/baseline/optimizer transitions must use
//!   [`crate::ActiveFrameMut`] over the existing [`NativeFrame`] instead.
//!
//! # See also
//! - [`crate::active_frame`] — canonical tier-neutral activation access.
//! - [`crate::deopt::DeoptFrame`] — owned inline-deopt reconstruction input.
//! - [`crate::native_abi::NativeFrame`] — canonical activation reconstructed only after a
//!   cold side exit.

use crate::{
    ActivationStack, ActiveFrameRef, ExecutionContext, Frame, Interpreter, Value, VmError, jit,
    native_abi::{NativeFrame, NativeFrameFlags, SideExit},
};

impl Interpreter {
    /// Materialize one generated stack-owned activation in interpreter storage
    /// and run it from its exact published resume PC.
    ///
    /// `native` is the still-published stack frame. Generated code retains
    /// ownership of its synchronous-depth and native-stack-byte reservations
    /// across this call and releases them only after the interpreter
    /// continuation returns.
    pub fn jit_deopt_materialize_stack_call(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        native: &mut NativeFrame,
        caller_function_id: u32,
        caller_call_pc: u32,
        callee_code_object_id: u64,
        caller_code_object_id: u64,
        call_kind: jit::JitDirectCallKind,
        exit: SideExit,
    ) -> Result<Value, VmError> {
        if !native
            .header
            .flags
            .contains(NativeFrameFlags::STACK_REGISTERS)
        {
            return Err(VmError::InvalidOperand);
        }
        self.note_generated_call_deopt(
            context,
            caller_function_id,
            caller_call_pc,
            caller_code_object_id,
            callee_code_object_id,
            call_kind,
            native,
            exit,
        )?;
        self.jit_deopt_resume_stack_call(context, stack, native, call_kind)
    }

    /// Materialize and resume one already-validated generated stack frame.
    /// Diagnostics/generation policy stay at the public boundary above; this
    /// helper owns the representation transition and is independently tested.
    fn jit_deopt_resume_stack_call(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        native: &mut NativeFrame,
        call_kind: jit::JitDirectCallKind,
    ) -> Result<Value, VmError> {
        // SAFETY: generated code keeps the original NativeFrame and both
        // windows initialized, published, and stable across this cold call.
        let active = unsafe { ActiveFrameRef::from_native_ptr(native) }
            .map_err(|_| VmError::InvalidOperand)?;
        let function = context
            .exec_function(native.header.function_id)
            .ok_or(VmError::InvalidOperand)?;
        let handler_plan = Self::jit_prepare_static_catch_handlers(
            context,
            native.header.function_id,
            native.header.pc,
        )?;
        let register_count = active.register_count();
        if register_count != usize::from(function.register_count)
            || active.upvalue_count() < usize::from(function.own_upvalue_count)
        {
            return Err(VmError::InvalidOperand);
        }

        // Copy all scalar and tagged inputs before building the materialized
        // frame. Host Vec/register-arena growth cannot collect; once pushed,
        // ordinary runtime-turn tracing owns the new interpreter storage.
        let self_value = active.self_value();
        let this_value = active.this_value();
        let mut upvalues = Vec::with_capacity(active.upvalue_count());
        for index in 0..active.upvalue_count() {
            let index = u32::try_from(index).map_err(|_| VmError::InvalidOperand)?;
            upvalues.push(active.upvalue(index)?);
        }

        let _window_rollback = self.register_window_rollback();
        let mut window = self.alloc_reg_window(register_count)?;
        for index in 0..register_count {
            let register = u16::try_from(index).map_err(|_| VmError::InvalidOperand)?;
            window[index] = active.read(register)?;
        }
        let mut frame = Frame::with_exec_return_upvalues_and_this(
            function,
            None,
            upvalues.into_boxed_slice(),
            this_value,
            window,
        );
        frame.self_value = self_value;
        frame.pc = native.header.pc;
        // The generated caller's actual-argument window dies with its stack
        // frame; the interpreter continuation reads the same list from the
        // cold record, exactly as an interpreter-entered call would.
        if let Some(count) = active.incoming_argument_count() {
            let incoming = (0..count)
                .map(|index| active.incoming_argument(index))
                .collect::<Result<smallvec::SmallVec<[Value; 4]>, _>>()?;
            self.frame_ensure_cold(&mut frame).incoming_args = incoming;
        }
        if matches!(
            call_kind,
            jit::JitDirectCallKind::Construct | jit::JitDirectCallKind::SuperConstruct
        ) {
            let receiver = this_value.as_object().ok_or(VmError::InvalidOperand)?;
            let cold = self.frame_ensure_cold(&mut frame);
            cold.construct_target = Some(receiver);
            cold.new_target = Some(active.new_target_value());
        } else if matches!(
            call_kind,
            jit::JitDirectCallKind::DerivedConstruct
                | jit::JitDirectCallKind::DerivedSuperConstruct
        ) {
            let cold = self.frame_ensure_cold(&mut frame);
            cold.is_derived_constructor = true;
            cold.new_target = Some(active.new_target_value());
        }
        if let Err(error) = self.jit_install_static_catch_handlers(&mut frame, handler_plan) {
            self.frame_release_cold(&mut frame);
            return Err(error);
        }
        self.with_materialized_native_frame(
            std::ptr::from_mut(native) as usize,
            stack.len(),
            |interp| {
                let floor = stack.floor();
                stack.push(frame);
                let materialized = stack
                    .last_mut()
                    .expect("generated deopt frame was just published");
                debug_assert!(materialized.eval_env.is_null());
                std::mem::swap(&mut materialized.eval_env, &mut native.eval_env);
                let result = interp.dispatch_loop_above_rooted(context, stack, floor);
                interp.release_frames_above(stack, floor);
                result
            },
        )?
    }

    /// Materialize a nested inline deopt chain and run it to completion.
    ///
    /// `frames` is ordered outermost first. The outermost completion returns to
    /// compiled code; every younger frame returns through its recorded parent
    /// destination. This owned reconstruction is reserved for inlined
    /// optimized exits that cannot resume one canonical native activation.
    /// `materialized_index` identifies the existing owner for an arena-backed
    /// entry; generated stack entries carry their actuals in the native window.
    pub fn jit_deopt_materialize_inline_frames(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        native: &mut NativeFrame,
        materialized_index: Option<usize>,
        frames: &[crate::deopt::DeoptFrame<Value>],
        exit: SideExit,
    ) -> Result<Value, VmError> {
        // The chain runs to completion here rather than reporting a bail, so
        // this is the only place the exit can charge the optimizing tier's
        // bounded reoptimization budget. An installed generation that keeps
        // exiting is discarded on the same rule as every other entry path.
        let outermost = frames.first().ok_or(VmError::InvalidOperand)?;
        if native.header.function_id != outermost.function_id {
            return Err(VmError::InvalidOperand);
        }
        let resume_pcs = frames
            .iter()
            .enumerate()
            .map(|(index, frame)| {
                let function = context
                    .exec_function(frame.function_id)
                    .ok_or(VmError::InvalidOperand)?;
                if frame.entry.is_some() != (index != 0)
                    || frame.slots.len() != usize::from(function.register_count)
                    || frame.entry.is_some_and(|entry| {
                        usize::from(entry.return_register) >= frames[index - 1].slots.len()
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
        let active = unsafe { ActiveFrameRef::from_native_ptr(native) }
            .map_err(|_| VmError::InvalidOperand)?;
        let source = materialized_index
            .map(|index| {
                let frame = stack.get(index).ok_or(VmError::InvalidOperand)?;
                if frame.header.function_id != native.header.function_id
                    || frame.registers.as_ptr() as u64 != native.register_base
                {
                    return Err(VmError::InvalidOperand);
                }
                Ok(frame)
            })
            .transpose()?;
        let source_cold = source.and_then(|frame| self.frame_cold(frame)).map(|cold| {
            (
                cold.incoming_args.clone(),
                cold.rest_args.clone(),
                cold.arguments_object,
                cold.construct_target,
            )
        });
        let incoming = active
            .incoming_argument_count()
            .map(|count| {
                (0..count)
                    .map(|index| active.incoming_argument(index))
                    .collect::<Result<smallvec::SmallVec<[Value; 4]>, _>>()
            })
            .transpose()?;
        self.note_jit_optimized_bail(context, outermost.function_id, exit);
        // The speculation that exited lives in the innermost spliced body; its
        // own exit profile must learn the PC so the next bake of that body,
        // inline or standalone, widens the site.
        if let Some(innermost) = frames.last()
            && frames.len() > 1
        {
            self.jit_optimized_exit_profiles
                .entry((
                    innermost.function_id,
                    *resume_pcs.last().ok_or(VmError::InvalidOperand)?,
                    exit.reason(),
                ))
                .and_modify(|profile| profile.action = profile.action.max(exit.action()))
                .or_insert(crate::jit::JitExitProfile {
                    action: exit.action(),
                    count: 0,
                });
        }
        let handler_plans = frames
            .iter()
            .enumerate()
            .map(|(index, frame)| {
                Self::jit_prepare_static_catch_handlers(
                    context,
                    frame.function_id,
                    resume_pcs[index],
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let _window_rollback = self.register_window_rollback();
        let floor = stack.floor();
        let mut materialized: smallvec::SmallVec<[Frame; 4]> = smallvec::SmallVec::new();

        for (index, deopt) in frames.iter().enumerate() {
            let entry = deopt.entry.unwrap_or(crate::deopt::DeoptFrameEntry {
                return_register: 0,
                this: native.this_value(),
                closure: native.self_value(),
                new_target: native.new_target(),
            });
            let function = context
                .exec_function(deopt.function_id)
                .ok_or(VmError::InvalidOperand)?;
            let upvalues: crate::frame_state::UpvalueSpine = if index == 0 {
                (0..active.upvalue_count())
                    .map(|index| active.upvalue(index as u32))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_boxed_slice()
            } else {
                match entry.closure.as_closure(&self.gc_heap) {
                    Some(closure) => closure.upvalues_snapshot(&self.gc_heap).into_boxed_slice(),
                    None => Vec::new().into_boxed_slice(),
                }
            };
            let mut window = self.alloc_reg_window(deopt.slots.len())?;
            window.copy_from_slice(&deopt.slots);
            let return_register = (index != 0).then_some(entry.return_register);
            let mut frame = Frame::with_exec_return_upvalues_and_this(
                function,
                return_register,
                upvalues,
                entry.this,
                window,
            );
            frame.self_value = entry.closure;
            frame.pc = resume_pcs[index];
            materialized.push(frame);
            self.record_jit_debug_event(|| crate::JitDebugEvent::InlineDeoptFrame {
                index: u32::try_from(index).unwrap_or(u32::MAX),
                total: u32::try_from(frames.len()).unwrap_or(u32::MAX),
                function_id: deopt.function_id,
                resume_pc: resume_pcs[index],
            });
        }

        // All fallible frame/window construction is complete. Attach call
        // state before any handler installation or dispatch can collect.
        let outer = &mut materialized[0];
        let new_target = active.new_target_value();
        if source_cold.is_some() || incoming.is_some() || !new_target.is_undefined() {
            let cold = self.frame_ensure_cold(outer);
            if let Some((args, rest, object, receiver)) = source_cold {
                cold.incoming_args = args;
                cold.rest_args = rest;
                cold.arguments_object = object;
                cold.construct_target = receiver;
            }
            if let Some(args) = incoming {
                cold.incoming_args = args;
            }
            cold.new_target = (!new_target.is_undefined()).then_some(new_target);
            cold.is_derived_constructor = native.is_derived_constructor();
            if !new_target.is_undefined()
                && !cold.is_derived_constructor
                && !context
                    .exec_function(outermost.function_id)
                    .expect("validated function")
                    .is_arrow
            {
                cold.construct_target = native.this_value().as_object();
            }
        }

        for (deopt, frame) in frames.iter().zip(materialized.iter_mut()).skip(1) {
            let entry = deopt.entry.expect("validated descendant entry");
            if !entry.new_target.is_undefined() {
                let function = context
                    .exec_function(deopt.function_id)
                    .expect("validated function");
                let derived = function.is_derived_constructor;
                let cold = self.frame_ensure_cold(frame);
                cold.new_target = Some(entry.new_target);
                cold.is_derived_constructor = derived;
                if !derived && !function.is_arrow {
                    cold.construct_target = entry.this.as_object();
                }
            }
        }

        for (index, plan) in handler_plans.into_iter().enumerate() {
            if let Err(error) =
                self.jit_install_static_catch_handlers(&mut materialized[index], plan)
            {
                for frame in &mut materialized {
                    self.frame_release_cold(frame);
                }
                return Err(error);
            }
        }

        if let Err(error) = self.enter_sync_reentry() {
            for frame in &mut materialized {
                self.frame_release_cold(frame);
            }
            return Err(error);
        }
        for frame in materialized {
            stack.push(frame);
        }
        let outer = stack
            .get_mut(floor.depth())
            .expect("inline deopt outer frame was just published");
        debug_assert!(outer.eval_env.is_null());
        std::mem::swap(&mut outer.eval_env, &mut native.eval_env);
        let result = self.dispatch_loop_above_rooted(context, stack, floor);
        self.leave_sync_reentry();
        self.release_frames_above(stack, floor);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BytecodeModule;
    use otter_bytecode::{Function, Instruction, Op, Operand, SourceKind};

    fn catch_function(id: u32) -> Function {
        Function {
            id,
            name: format!("deopt_catch_{id}"),
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
            module: "deopt-materialization-test.js".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::JavaScript,
            functions,
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
            function_source: None,
        })
        .expect("valid bytecode fixture")
    }

    #[test]
    fn every_spliced_frame_rebuilds_its_own_catch_stack() {
        for stack_owned in [false, true] {
            let context = context(vec![catch_function(0), catch_function(1)]);
            let mut interpreter = Interpreter::new();
            let mut stack = ActivationStack::new();
            let thrown = Value::number_i32(73);
            let eval_env = crate::eval_env::alloc_eval_env(&mut interpreter.gc_heap, None)
                .expect("inline eval env");
            let mut native = NativeFrame::new(
                crate::native_abi::VmFrameHeader {
                    function_id: 0,
                    pc: 1,
                    register_count: 0,
                    kind: crate::native_abi::NativeFrameKind::Optimizing,
                    flags: NativeFrameFlags::empty(),
                },
                0,
                Value::function(0),
                Value::undefined(),
            );
            let mut native_registers = [Value::undefined(); 2];
            if stack_owned {
                native.register_base = native_registers.as_mut_ptr() as u64;
                native.header.register_count = 2;
                native.set_stack_registers();
            }
            native.set_eval_env(Some(eval_env));
            let frames = [
                crate::deopt::DeoptFrame {
                    function_id: 0,
                    byte_pc: context
                        .exec_function(0)
                        .unwrap()
                        .instruction_byte_pc(1)
                        .unwrap(),
                    entry: None,
                    slots: Box::new([Value::undefined(), Value::undefined()]),
                },
                crate::deopt::DeoptFrame {
                    function_id: 1,
                    byte_pc: context
                        .exec_function(1)
                        .unwrap()
                        .instruction_byte_pc(1)
                        .unwrap(),
                    entry: Some(crate::deopt::DeoptFrameEntry {
                        return_register: 0,
                        this: Value::undefined(),
                        closure: Value::function(1),
                        new_target: Value::undefined(),
                    }),
                    slots: Box::new([thrown, Value::undefined()]),
                },
            ];

            let result = interpreter
                .with_runtime_turn(&mut stack, |turn| {
                    let (interpreter, stack) = turn.into_parts();
                    interpreter.jit_deopt_materialize_inline_frames(
                        &context,
                        stack,
                        &mut native,
                        None,
                        &frames,
                        SideExit::new(
                            1,
                            crate::native_abi::ExitReason::UnsupportedOperation,
                            crate::native_abi::ExitAction::Recompile,
                        ),
                    )
                })
                .expect("both nested catches resume");
            assert_eq!(result, thrown);
            assert!(native.eval_env().is_none());
            assert!(stack.is_empty());
        }
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
                            vec![Instruction {
                                pc: 0,
                                op: Op::ReturnValue,
                                operands: vec![Operand::Register(0)],
                            }]
                            .into()
                        },
                        ..Function::default()
                    })
                    .collect(),
            );
            let mut vm = Interpreter::new();
            let mut stack = ActivationStack::new();
            let mut registers = [Value::undefined()];
            let mut native = NativeFrame::new(
                crate::native_abi::VmFrameHeader::interpreter(0, 1),
                registers.as_mut_ptr() as u64,
                Value::function(0),
                Value::undefined(),
            );
            native.set_stack_registers();
            vm.with_runtime_turn(&mut stack, |turn| {
                let (vm, stack) = turn.into_parts();
                let receiver =
                    Value::object(vm.alloc_runtime_rooted_object_with_roots(&[], &[]).unwrap());
                let frames = [
                    crate::deopt::DeoptFrame {
                        function_id: 0,
                        byte_pc: 0,
                        entry: None,
                        slots: Box::new([Value::undefined()]),
                    },
                    crate::deopt::DeoptFrame {
                        function_id: 1,
                        byte_pc: 0,
                        entry: Some(crate::deopt::DeoptFrameEntry {
                            return_register: 0,
                            this: receiver,
                            closure: Value::function(1),
                            new_target: Value::function(2),
                        }),
                        slots: Box::new([Value::undefined()]),
                    },
                ];
                let result = vm
                    .jit_deopt_materialize_inline_frames(
                        &context,
                        stack,
                        &mut native,
                        None,
                        &frames,
                        SideExit::new(
                            0,
                            crate::native_abi::ExitReason::UnsupportedOperation,
                            crate::native_abi::ExitAction::Recompile,
                        ),
                    )
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
            assert!(stack.is_empty());
        }
    }

    #[test]
    fn generated_stack_frame_rebuilds_catch_before_dispatch() {
        let context = context(vec![catch_function(0)]);
        let mut interpreter = Interpreter::new();
        let mut stack = ActivationStack::new();
        let thrown = Value::number_i32(91);
        let eval_env = crate::eval_env::alloc_eval_env(&mut interpreter.gc_heap, None)
            .expect("generated eval env");
        let mut registers = [thrown, Value::undefined()];
        let mut native = NativeFrame::new(
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
        native.set_stack_registers();
        native.set_eval_env(Some(eval_env));
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
                interpreter.jit_deopt_resume_stack_call(
                    &context,
                    stack,
                    &mut native,
                    jit::JitDirectCallKind::Plain,
                )
            })
            .expect("stack-call catch resumes");
        interpreter.jit_pop_native_activation();
        assert_eq!(result, thrown);
        assert!(native.eval_env().is_none());
        assert!(stack.is_empty());
    }
}
