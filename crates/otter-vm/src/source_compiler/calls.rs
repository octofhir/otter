//! Call, super-call, and `new` expression compilation: CallStatic/CallSpread
//! argument marshalling, super-call forwarding with `this` bind-back and
//! field-initializer triggering, optional-chaining short-circuit integration,
//! member-update operators (`obj.p++`, `obj[k]--`, `obj.#p += …`), and the
//! shared `compile_call_target` / `compile_construct_target` LHS evaluators
//! used by ordinary calls, tail calls, tagged templates, optional chains,
//! and `new`.
//!
//! Spec: ECMA-262 §13.3 (LeftHandSideExpressions), §13.4 (Update Expressions),
//! §12.3.7 (SuperCall), §13.3.5 (`new` Operator), §7.3.32 (PrivateGet),
//! §19.2.1.1 (PerformEval).

use super::ast::is_test262_assert_same_value_call;
use super::shared::{Binding, FunctionCompiler, FunctionKind, ValueLocation};
use super::*;

impl<'a> FunctionCompiler<'a> {
    /// §13.4 Update Expressions (`x++`, `--x`, `obj.x++`, `arr[i]--`)
    /// Spec: <https://tc39.es/ecma262/#sec-update-expressions>
    pub(super) fn compile_update_expression(
        &mut self,
        update: &oxc_ast::ast::UpdateExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        match &update.argument {
            SimpleAssignmentTarget::AssignmentTargetIdentifier(identifier) => {
                self.compile_identifier_update(identifier.name.as_str(), update)
            }
            SimpleAssignmentTarget::StaticMemberExpression(member) => {
                self.compile_member_update_static(member, update, module)
            }
            SimpleAssignmentTarget::ComputedMemberExpression(member) => {
                self.compile_member_update_computed(member, update, module)
            }
            SimpleAssignmentTarget::PrivateFieldExpression(member) => {
                self.compile_member_update_private(member, update, module)
            }
            _ => Err(SourceLoweringError::Unsupported(
                "unsupported update target".to_string(),
            )),
        }
    }

    fn compile_identifier_update(
        &mut self,
        name: &str,
        update: &oxc_ast::ast::UpdateExpression<'_>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let current = self.compile_identifier(name)?;
        // §13.4.2 Postfix Increment: save old value BEFORE assignment,
        // because assign_to_name may mutate the same register (for locals)
        // and temp LIFO ordering can cause register reuse for upvalues.
        // Use allocate_local() for old_val so it persists safely.
        // Spec: <https://tc39.es/ecma262/#sec-postfix-increment-operator>
        let old_val = if !update.prefix {
            let reg = self.allocate_local()?;
            self.instructions
                .push(Instruction::to_number(reg, current.register));
            Some(ValueLocation::local(reg))
        } else {
            None
        };
        let result = self.alloc_temp();
        let delta = match update.operator {
            UpdateOperator::Increment => self.load_i32(1)?,
            UpdateOperator::Decrement => self.load_i32(-1)?,
        };
        self.instructions
            .push(Instruction::add(result, current.register, delta.register));
        self.release(delta);
        self.release(current);
        let new_val = ValueLocation::temp(result);
        let _ = self.assign_to_name(name, new_val)?;
        if update.prefix {
            Ok(ValueLocation::temp(result))
        } else {
            Ok(old_val.unwrap())
        }
    }

    /// `obj.prop++` / `--obj.prop`
    fn compile_member_update_static(
        &mut self,
        member: &oxc_ast::ast::StaticMemberExpression<'_>,
        update: &oxc_ast::ast::UpdateExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let object = self.compile_expression(&member.object, module)?;
        let object = self.stabilize_binding_value(object)?;
        let prop = self.intern_property_name(member.property.name.as_str())?;

        // Load current value.
        let old_val = ValueLocation::temp(self.alloc_temp());
        self.instructions.push(Instruction::get_property(
            old_val.register,
            object.register,
            prop,
        ));

        // Compute new value.
        let delta = match update.operator {
            UpdateOperator::Increment => self.load_i32(1)?,
            UpdateOperator::Decrement => self.load_i32(-1)?,
        };
        let new_val = ValueLocation::temp(self.alloc_temp());
        self.instructions.push(Instruction::add(
            new_val.register,
            old_val.register,
            delta.register,
        ));
        self.release(delta);

        // Store new value back.
        self.instructions.push(Instruction::set_property(
            object.register,
            new_val.register,
            prop,
        ));

        if update.prefix {
            self.release(old_val);
            Ok(new_val)
        } else {
            self.release(new_val);
            Ok(old_val)
        }
    }

    /// `arr[i]++` / `--obj[key]`
    fn compile_member_update_computed(
        &mut self,
        member: &oxc_ast::ast::ComputedMemberExpression<'_>,
        update: &oxc_ast::ast::UpdateExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let object = self.compile_expression(&member.object, module)?;
        let object = self.stabilize_binding_value(object)?;
        let key = self.compile_expression(&member.expression, module)?;
        let key = self.stabilize_binding_value(key)?;

        // Load current value.
        let old_val = ValueLocation::temp(self.alloc_temp());
        self.instructions.push(Instruction::get_index(
            old_val.register,
            object.register,
            key.register,
        ));

        // Compute new value.
        let delta = match update.operator {
            UpdateOperator::Increment => self.load_i32(1)?,
            UpdateOperator::Decrement => self.load_i32(-1)?,
        };
        let new_val = ValueLocation::temp(self.alloc_temp());
        self.instructions.push(Instruction::add(
            new_val.register,
            old_val.register,
            delta.register,
        ));
        self.release(delta);

        // Store new value back.
        self.instructions.push(Instruction::set_index(
            object.register,
            key.register,
            new_val.register,
        ));

        if update.prefix {
            self.release(old_val);
            Ok(new_val)
        } else {
            self.release(new_val);
            Ok(old_val)
        }
    }

    /// `obj.#field++` / `--obj.#field`
    fn compile_member_update_private(
        &mut self,
        member: &oxc_ast::ast::PrivateFieldExpression<'_>,
        update: &oxc_ast::ast::UpdateExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let object = self.compile_expression(&member.object, module)?;
        let object = self.stabilize_binding_value(object)?;
        let prop_id = self.intern_property_name(member.field.name.as_str())?;

        let old_val = ValueLocation::temp(self.alloc_temp());
        self.instructions.push(Instruction::get_private_field(
            old_val.register,
            object.register,
            prop_id,
        ));

        let delta = match update.operator {
            UpdateOperator::Increment => self.load_i32(1)?,
            UpdateOperator::Decrement => self.load_i32(-1)?,
        };
        let new_val = ValueLocation::temp(self.alloc_temp());
        self.instructions.push(Instruction::add(
            new_val.register,
            old_val.register,
            delta.register,
        ));
        self.release(delta);

        self.instructions.push(Instruction::set_private_field(
            object.register,
            new_val.register,
            prop_id,
        ));

        if update.prefix {
            self.release(old_val);
            Ok(new_val)
        } else {
            self.release(new_val);
            Ok(old_val)
        }
    }

    /// §7.3.32 PrivateGet — `obj.#field`
    /// Spec: <https://tc39.es/ecma262/#sec-privatemethods-specification-type>
    pub(super) fn compile_private_field_get(
        &mut self,
        member: &oxc_ast::ast::PrivateFieldExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let object = self.compile_expression(&member.object, module)?;
        let prop_id = self.intern_property_name(member.field.name.as_str())?;
        let dst = self.alloc_temp();
        self.instructions.push(Instruction::get_private_field(
            dst,
            object.register,
            prop_id,
        ));
        self.release(object);
        Ok(ValueLocation::temp(dst))
    }

    /// §13.10.1 `#field in obj` brand check.
    /// Spec: <https://tc39.es/ecma262/#sec-relational-operators-runtime-semantics-evaluation>
    pub(super) fn compile_private_in_expression(
        &mut self,
        expr: &oxc_ast::ast::PrivateInExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let object = self.compile_expression(&expr.right, module)?;
        let prop_id = self.intern_property_name(expr.left.name.as_str())?;
        let dst = self.alloc_temp();
        self.instructions
            .push(Instruction::in_private(dst, object.register, prop_id));
        self.release(object);
        Ok(ValueLocation::temp(dst))
    }

    pub(super) fn compile_call_expression(
        &mut self,
        call: &oxc_ast::ast::CallExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        if self.mode == LoweringMode::Test262Basic && is_test262_assert_same_value_call(call) {
            return self.compile_test262_assert_same_value(call, module);
        }
        if matches!(&call.callee, Expression::Super(_)) {
            return self.compile_super_call_expression(call, module);
        }

        // §19.2.1.1 Detect direct eval: `eval(code)`.
        // A call where the callee is the bare identifier `eval` is a *candidate*
        // for direct eval — but only when the reference would resolve to the
        // intrinsic %eval% (i.e. the global `eval`). If `eval` has been locally
        // rebound (e.g. `var eval = f` hoisted in an enclosing function), then
        // SameValue(func, %eval%) is false at runtime and the call must use
        // normal call semantics (so tail-call optimization can apply, etc.).
        // JSC performs the same compile-time check: if `eval` is in any local
        // scope, it is *not* the intrinsic, so emit a regular call.
        // Spec: <https://tc39.es/ecma262/#sec-function-calls-runtime-semantics-evaluation>
        if let Expression::Identifier(ident) = &call.callee
            && ident.name == "eval"
            && !self.is_name_locally_visible("eval")
        {
            return self.compile_direct_eval_call(call, module);
        }

        let (callee, receiver) = self.compile_call_target(&call.callee, module)?;
        // Spill older temporaries before newer ones. `allocate_local()` grows the
        // local prefix from the bottom, so stabilizing a newer temp first can
        // overlap and clobber an older live temp register.
        let receiver = match receiver {
            Some(receiver) if receiver.is_temp => Some(self.stabilize_binding_value(receiver)?),
            other => other,
        };
        let callee = if callee.is_temp {
            self.stabilize_binding_value(callee)?
        } else {
            callee
        };

        let has_spread = call
            .arguments
            .iter()
            .any(|arg| matches!(arg, Argument::SpreadElement(_)));

        // Stash the call expression's own span on the compiler. The call
        // helpers below compile the argument list (which will overwrite
        // the active span via per-sub-expression record_location calls),
        // then read this stash and re-record it RIGHT BEFORE emitting the
        // CallClosure / CallSpread opcode. That way the underline lands
        // on the call site itself when the callee throws — not on the
        // last argument.
        let saved = self.pending_site_span.replace(call.span);
        let result = if has_spread {
            self.compile_call_with_spread(&call.arguments, callee, receiver, false, module)
        } else {
            self.compile_call_static_args(&call.arguments, callee, receiver, false, module)
        };
        self.pending_site_span = saved;
        result
    }

    /// §19.2.1.1 Compile a direct eval call: `eval(code)`.
    ///
    /// Emits a `CallEval dst, code` instruction instead of a normal function call.
    /// The interpreter will compile and execute the source code in the caller's
    /// context, inheriting strict mode.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-performeval>
    fn compile_direct_eval_call(
        &mut self,
        call: &oxc_ast::ast::CallExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let code = if let Some(arg) = call.arguments.first() {
            let expr = arg.as_expression().ok_or_else(|| {
                SourceLoweringError::Unsupported("spread in eval() not supported".to_string())
            })?;
            let val = self.compile_expression(expr, module)?;
            if val.is_temp {
                self.stabilize_binding_value(val)?
            } else {
                val
            }
        } else {
            let reg = self.alloc_temp();
            self.instructions.push(Instruction::load_undefined(reg));
            ValueLocation::temp(reg)
        };

        let dst = self.alloc_temp();
        self.instructions
            .push(Instruction::call_eval(dst, code.register));
        self.release(code);
        Ok(ValueLocation::temp(dst))
    }

    /// §13.3.8.1 ArgumentListEvaluation — no spread, register-window path.
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-argumentlistevaluation>
    pub(super) fn compile_call_static_args(
        &mut self,
        arguments: &[Argument<'_>],
        callee: ValueLocation,
        receiver: Option<ValueLocation>,
        is_construct: bool,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let argument_count = RegisterIndex::try_from(arguments.len())
            .map_err(|_| SourceLoweringError::TooManyLocals)?;
        let mut argument_values = Vec::with_capacity(usize::from(argument_count));

        for argument in arguments {
            let value = self.compile_expression(
                argument.as_expression().ok_or_else(|| {
                    SourceLoweringError::Unsupported("unsupported call argument".to_string())
                })?,
                module,
            )?;
            argument_values.push(if value.is_temp {
                self.stabilize_binding_value(value)?
            } else {
                value
            });
        }

        let arg_start = if argument_count == 0 {
            BytecodeRegister::new(self.next_local + self.next_temp)
        } else {
            self.reserve_temp_window(argument_count)?
        };

        for (offset, value) in argument_values.into_iter().enumerate() {
            let destination = BytecodeRegister::new(arg_start.index() + offset as u16);
            if value.register != destination {
                self.instructions
                    .push(Instruction::move_(destination, value.register));
                self.release(value);
            }
        }

        let mut result = if receiver.is_some_and(|receiver| receiver.is_temp) {
            receiver.expect("receiver must exist when reusing receiver temp")
        } else if callee.is_temp {
            callee
        } else {
            ValueLocation::temp(self.alloc_temp())
        };
        // Re-attribute the next opcode to the parent call/new expression.
        if let Some(site_span) = self.pending_site_span {
            self.record_location(site_span);
        }
        let pc = self.instructions.len();
        self.instructions.push(Instruction::call_closure(
            result.register,
            callee.register,
            arg_start,
        ));
        let call_site = match receiver {
            Some(receiver) => CallSite::Closure(ClosureCall::new_with_receiver(
                argument_count,
                FrameFlags::new(is_construct, true, false),
                receiver.register,
            )),
            None => CallSite::Closure(ClosureCall::new(
                argument_count,
                FrameFlags::new(is_construct, !is_construct, false),
            )),
        };
        self.record_call_site(pc, call_site);

        if argument_count != 0 {
            let stable_register =
                BytecodeRegister::new(arg_start.index() + argument_count.saturating_sub(1));
            if result.register != stable_register {
                self.instructions
                    .push(Instruction::move_(stable_register, result.register));
                result = ValueLocation::temp(stable_register);
            }
            self.release_temp_window(argument_count.saturating_sub(1));
        }
        if callee.register != result.register {
            self.release(callee);
        }
        if let Some(receiver) = receiver
            && receiver.register != result.register
        {
            self.release(receiver);
        }
        // The bumper allocator releases are not strictly LIFO: the arg window
        // shrink and the callee release may leave `next_temp` pointing *below*
        // the slot that actually holds the live result. Subsequent `alloc_temp`
        // calls would then hand out that live register to a new temp and clobber
        // it (reproducer: `arr.findIndex(inline-fn)` inside `assert.sameValue`
        // where `-1` is compiled into the allocator holes that overlap the
        // result). Force `next_temp` to cover the stable result register.
        self.ensure_temp_region_covers(result.register);
        Ok(result)
    }

    /// §13.3.8.1 ArgumentListEvaluation — spread present, array-based path.
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-argumentlistevaluation>
    ///
    /// Builds all arguments (spread and non-spread) into a temporary array,
    /// then emits CallSpread which extracts the elements at runtime.
    pub(super) fn compile_call_with_spread(
        &mut self,
        arguments: &[Argument<'_>],
        callee: ValueLocation,
        receiver: Option<ValueLocation>,
        is_construct: bool,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        // Build argument array: NewArray(0) + ArrayPush/SpreadIntoArray.
        let args_array = ValueLocation::temp(self.alloc_temp());
        self.instructions
            .push(Instruction::new_array(args_array.register, 0));
        let args_array = self.stabilize_binding_value(args_array)?;

        for argument in arguments {
            match argument {
                Argument::SpreadElement(spread) => {
                    let iterable = self.compile_expression(&spread.argument, module)?;
                    let iterable = if iterable.is_temp {
                        self.stabilize_binding_value(iterable)?
                    } else {
                        iterable
                    };
                    self.instructions.push(Instruction::spread_into_array(
                        args_array.register,
                        iterable.register,
                    ));
                    self.release(iterable);
                }
                _ => {
                    let value = self.compile_expression(
                        argument.as_expression().ok_or_else(|| {
                            SourceLoweringError::Unsupported(
                                "unsupported call argument".to_string(),
                            )
                        })?,
                        module,
                    )?;
                    let value = if value.is_temp {
                        self.stabilize_binding_value(value)?
                    } else {
                        value
                    };
                    self.instructions
                        .push(Instruction::array_push(args_array.register, value.register));
                    self.release(value);
                }
            }
        }

        // Emit CallSpread dst, callee, args_array.
        let result = if callee.is_temp {
            callee
        } else {
            ValueLocation::temp(self.alloc_temp())
        };
        // Re-attribute the next opcode to the parent call/new expression.
        if let Some(site_span) = self.pending_site_span {
            self.record_location(site_span);
        }
        let pc = self.instructions.len();
        self.instructions.push(Instruction::call_spread(
            result.register,
            callee.register,
            args_array.register,
        ));
        let call_site = match receiver {
            Some(receiver) => CallSite::Closure(ClosureCall::new_with_receiver(
                0, // argument_count unused for CallSpread — args come from the array
                FrameFlags::new(is_construct, true, false),
                receiver.register,
            )),
            None => CallSite::Closure(ClosureCall::new(
                0,
                FrameFlags::new(is_construct, !is_construct, false),
            )),
        };
        self.record_call_site(pc, call_site);

        self.release(args_array);
        if callee.register != result.register {
            self.release(callee);
        }
        if let Some(receiver) = receiver
            && receiver.register != result.register
        {
            self.release(receiver);
        }
        Ok(result)
    }

    /// §12.3.7.1 SuperCall: `super(Arguments)`
    /// Spec: <https://tc39.es/ecma262/#sec-super-keyword-runtime-semantics-evaluation>
    fn compile_super_call_expression(
        &mut self,
        call: &oxc_ast::ast::CallExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        // `super(...)` is valid in derived-class constructors AND in arrow
        // functions lexically nested inside them (§15.3 — arrows inherit
        // `super` from their enclosing non-arrow function).
        let allowed_by_arrow = matches!(self.kind, FunctionKind::Arrow | FunctionKind::AsyncArrow);
        if !self.is_derived_constructor && !allowed_by_arrow {
            return Err(SourceLoweringError::Unsupported(
                "super() is only supported inside derived class constructors".to_string(),
            ));
        }

        let has_spread = call
            .arguments
            .iter()
            .any(|arg| matches!(arg, Argument::SpreadElement(_)));

        if has_spread {
            self.compile_super_call_with_spread(call, module)
        } else {
            self.compile_super_call_static(call, module)
        }
    }

    /// super() with no spread — uses CallSuper opcode with contiguous register window.
    fn compile_super_call_static(
        &mut self,
        call: &oxc_ast::ast::CallExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let argument_count = RegisterIndex::try_from(call.arguments.len())
            .map_err(|_| SourceLoweringError::TooManyLocals)?;
        let mut argument_values = Vec::with_capacity(usize::from(argument_count));
        for argument in &call.arguments {
            let value = self.compile_expression(
                argument.as_expression().ok_or_else(|| {
                    SourceLoweringError::Unsupported("unsupported super() argument".to_string())
                })?,
                module,
            )?;
            argument_values.push(if value.is_temp {
                self.stabilize_binding_value(value)?
            } else {
                value
            });
        }

        let arg_start = if argument_count == 0 {
            BytecodeRegister::new(self.next_local + self.next_temp)
        } else {
            self.reserve_temp_window(argument_count)?
        };
        for (offset, value) in argument_values.into_iter().enumerate() {
            let destination = BytecodeRegister::new(arg_start.index() + offset as u16);
            if value.register != destination {
                self.instructions
                    .push(Instruction::move_(destination, value.register));
                self.release(value);
            }
        }

        let mut result = ValueLocation::temp(self.alloc_temp());
        self.instructions.push(Instruction::call_super(
            result.register,
            arg_start,
            argument_count,
        ));
        // §9.1.1.3.1 BindThisValue — write back the newly-constructed
        // `this` to the lexical "this" binding. For direct ctor use this
        // is a local register Move. For arrows inside derived ctors this
        // is a SetUpvalue that propagates through the UpvalueCell to the
        // enclosing constructor's local "this" register.
        // Use resolve_binding (not scope lookup) so arrows capture from
        // the parent scope chain correctly.
        match self.resolve_binding("this") {
            Ok(Binding::ThisRegister(this_register)) if this_register != result.register => {
                self.instructions
                    .push(Instruction::move_(this_register, result.register));
            }
            Ok(Binding::ThisUpvalue(upvalue)) => {
                self.instructions
                    .push(Instruction::set_upvalue(result.register, upvalue));
            }
            _ => {}
        }

        // §15.7.14 — Run instance field initializers after super() binds `this`.
        if self.has_instance_fields {
            self.instructions
                .push(Instruction::run_class_field_initializer());
        }

        if argument_count != 0 {
            let stable_register =
                BytecodeRegister::new(arg_start.index() + argument_count.saturating_sub(1));
            if result.register != stable_register {
                self.instructions
                    .push(Instruction::move_(stable_register, result.register));
                result = ValueLocation::temp(stable_register);
            }
            self.release_temp_window(argument_count.saturating_sub(1));
        }

        Ok(result)
    }

    /// super(...spread) — uses CallSuperSpread opcode with args from array.
    fn compile_super_call_with_spread(
        &mut self,
        call: &oxc_ast::ast::CallExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        // Build arguments array: NewArray(0) + ArrayPush/SpreadIntoArray.
        let args_array = ValueLocation::temp(self.alloc_temp());
        self.instructions
            .push(Instruction::new_array(args_array.register, 0));
        let args_array = self.stabilize_binding_value(args_array)?;

        for argument in &call.arguments {
            match argument {
                Argument::SpreadElement(spread) => {
                    let iterable = self.compile_expression(&spread.argument, module)?;
                    let iterable = if iterable.is_temp {
                        self.stabilize_binding_value(iterable)?
                    } else {
                        iterable
                    };
                    self.instructions.push(Instruction::spread_into_array(
                        args_array.register,
                        iterable.register,
                    ));
                    self.release(iterable);
                }
                _ => {
                    let value = self.compile_expression(
                        argument.as_expression().ok_or_else(|| {
                            SourceLoweringError::Unsupported(
                                "unsupported super() argument".to_string(),
                            )
                        })?,
                        module,
                    )?;
                    let value = if value.is_temp {
                        self.stabilize_binding_value(value)?
                    } else {
                        value
                    };
                    self.instructions
                        .push(Instruction::array_push(args_array.register, value.register));
                    self.release(value);
                }
            }
        }

        let result = ValueLocation::temp(self.alloc_temp());
        self.instructions.push(Instruction::call_super_spread(
            result.register,
            args_array.register,
        ));
        self.release(args_array);

        // §9.1.1.3.1 BindThisValue write-back (same as static path).
        match self.resolve_binding("this") {
            Ok(Binding::ThisRegister(this_register)) if this_register != result.register => {
                self.instructions
                    .push(Instruction::move_(this_register, result.register));
            }
            Ok(Binding::ThisUpvalue(upvalue)) => {
                self.instructions
                    .push(Instruction::set_upvalue(result.register, upvalue));
            }
            _ => {}
        }

        // §15.7.14 — Run instance field initializers after super() binds `this`.
        if self.has_instance_fields {
            self.instructions
                .push(Instruction::run_class_field_initializer());
        }

        Ok(result)
    }

    /// §13.3.5 The `new` Operator — `new MemberExpression Arguments`
    /// Spec: <https://tc39.es/ecma262/#sec-new-operator>
    pub(super) fn compile_new_expression(
        &mut self,
        new_expression: &oxc_ast::ast::NewExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let (callee, cleanup) = self.compile_construct_target(&new_expression.callee, module)?;
        // Same spill ordering rule as ordinary calls: stabilize older temps first.
        let cleanup = match cleanup {
            Some(value) if value.is_temp => Some(self.stabilize_binding_value(value)?),
            other => other,
        };
        let callee = if callee.is_temp {
            self.stabilize_binding_value(callee)?
        } else {
            callee
        };

        let arguments = &new_expression.arguments;
        let has_spread = arguments
            .iter()
            .any(|arg| matches!(arg, Argument::SpreadElement(_)));

        // Stash `new`'s span; the call helper re-records it right before
        // emitting the Construct opcode so error frames captured at
        // constructor invocation (Error / TypeError / etc.) underline the
        // whole `new Foo(...)` expression rather than the last argument.
        let saved = self.pending_site_span.replace(new_expression.span);
        let result = if has_spread {
            self.compile_call_with_spread(arguments, callee, None, true, module)?
        } else {
            self.compile_call_static_args(arguments, callee, None, true, module)?
        };
        self.pending_site_span = saved;

        if let Some(value) = cleanup
            && value.register != result.register
        {
            self.release(value);
        }
        Ok(result)
    }

    pub(super) fn compile_call_target(
        &mut self,
        callee: &Expression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(ValueLocation, Option<ValueLocation>), SourceLoweringError> {
        match callee {
            Expression::Identifier(identifier) => {
                match self.resolve_binding(identifier.name.as_str()) {
                    Ok(Binding::Function {
                        closure_register, ..
                    }) => Ok((ValueLocation::local(closure_register), None)),
                    Ok(_) | Err(SourceLoweringError::UnknownBinding(_)) => {
                        // Known non-function binding or undeclared global →
                        // compile as expression (which handles GetGlobal).
                        let callee = self.compile_expression(callee, module)?;
                        Ok((self.materialize_value(callee), None))
                    }
                    Err(e) => Err(e),
                }
            }
            Expression::StaticMemberExpression(member) => {
                // §13.3.7 SuperProperty call target — `super.foo(args)`.
                // Looks up `foo` via GetSuperProperty and calls it with
                // `this` (not `base`) as the receiver.
                if matches!(&member.object, Expression::Super(_)) {
                    let this_value = self.compile_this_expression()?;
                    let this_value = if this_value.is_temp {
                        self.stabilize_binding_value(this_value)?
                    } else {
                        this_value
                    };
                    let callee_register = self.alloc_temp();
                    let property = self.intern_property_name(member.property.name.as_str())?;
                    self.record_location(member.span);
                    self.instructions
                        .push(Instruction::get_super_property(callee_register, property));
                    return Ok((ValueLocation::temp(callee_register), Some(this_value)));
                }
                let receiver = self.compile_expression(&member.object, module)?;
                // Stabilize a temp receiver before allocating the callee
                // register. The bumper's `release` does not enforce LIFO, so
                // the temp count after compiling `member.object` may
                // underestimate the highest live slot, causing `alloc_temp()`
                // to return the receiver's own register and clobber it via
                // GetProperty.
                let receiver = if receiver.is_temp {
                    self.stabilize_binding_value(receiver)?
                } else {
                    receiver
                };
                let callee_register = self.alloc_temp();
                let property = self.intern_property_name(member.property.name.as_str())?;
                self.record_location(member.span);
                self.instructions.push(Instruction::get_property(
                    callee_register,
                    receiver.register,
                    property,
                ));
                Ok((ValueLocation::temp(callee_register), Some(receiver)))
            }
            Expression::ComputedMemberExpression(member) => {
                // §13.3.7 SuperProperty call target — `super[key](args)`.
                if matches!(&member.object, Expression::Super(_)) {
                    let key = self.compile_expression(&member.expression, module)?;
                    let key = if key.is_temp {
                        self.stabilize_binding_value(key)?
                    } else {
                        key
                    };
                    let this_value = self.compile_this_expression()?;
                    let this_value = if this_value.is_temp {
                        self.stabilize_binding_value(this_value)?
                    } else {
                        this_value
                    };
                    let callee_register = self.alloc_temp();
                    self.record_location(member.span);
                    self.instructions
                        .push(Instruction::get_super_property_computed(
                            callee_register,
                            key.register,
                        ));
                    self.release(key);
                    return Ok((ValueLocation::temp(callee_register), Some(this_value)));
                }
                let mut receiver = self.compile_expression(&member.object, module)?;
                if receiver.is_temp {
                    receiver = self.stabilize_binding_value(receiver)?;
                }
                let callee_register = self.alloc_temp();

                match &member.expression {
                    Expression::StringLiteral(literal) => {
                        let property = self.intern_property_name(literal.value.as_str())?;
                        self.record_location(member.span);
                        self.instructions.push(Instruction::get_property(
                            callee_register,
                            receiver.register,
                            property,
                        ));
                    }
                    _ => {
                        let index = self.compile_expression(&member.expression, module)?;
                        self.record_location(member.span);
                        self.instructions.push(Instruction::get_index(
                            callee_register,
                            receiver.register,
                            index.register,
                        ));
                        self.release(index);
                    }
                }

                Ok((ValueLocation::temp(callee_register), Some(receiver)))
            }
            Expression::PrivateFieldExpression(member) => {
                let receiver = self.compile_expression(&member.object, module)?;
                let receiver = if receiver.is_temp {
                    self.stabilize_binding_value(receiver)?
                } else {
                    receiver
                };
                let callee_register = self.alloc_temp();
                let prop_id = self.intern_property_name(member.field.name.as_str())?;
                self.record_location(member.span);
                self.instructions.push(Instruction::get_private_field(
                    callee_register,
                    receiver.register,
                    prop_id,
                ));
                Ok((ValueLocation::temp(callee_register), Some(receiver)))
            }
            _ => {
                let callee = self.compile_expression(callee, module)?;
                Ok((self.materialize_value(callee), None))
            }
        }
    }

    fn compile_construct_target(
        &mut self,
        callee: &Expression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(ValueLocation, Option<ValueLocation>), SourceLoweringError> {
        match callee {
            Expression::StaticMemberExpression(member) => {
                let object = self.compile_expression(&member.object, module)?;
                let callee_register = self.alloc_temp();
                let property = self.intern_property_name(member.property.name.as_str())?;
                self.record_location(member.span);
                self.instructions.push(Instruction::get_property(
                    callee_register,
                    object.register,
                    property,
                ));
                Ok((ValueLocation::temp(callee_register), Some(object)))
            }
            Expression::ComputedMemberExpression(member) => {
                let mut object = self.compile_expression(&member.object, module)?;
                if object.is_temp {
                    object = self.stabilize_binding_value(object)?;
                }
                let callee_register = self.alloc_temp();

                match &member.expression {
                    Expression::StringLiteral(literal) => {
                        let property = self.intern_property_name(literal.value.as_str())?;
                        self.record_location(member.span);
                        self.instructions.push(Instruction::get_property(
                            callee_register,
                            object.register,
                            property,
                        ));
                    }
                    _ => {
                        let index = self.compile_expression(&member.expression, module)?;
                        self.record_location(member.span);
                        self.instructions.push(Instruction::get_index(
                            callee_register,
                            object.register,
                            index.register,
                        ));
                        self.release(index);
                    }
                }

                Ok((ValueLocation::temp(callee_register), Some(object)))
            }
            _ => Ok((self.compile_expression(callee, module)?, None)),
        }
    }

    fn compile_test262_assert_same_value(
        &mut self,
        call: &oxc_ast::ast::CallExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let mut actual = None;
        let mut expected = None;

        for (index, argument) in call.arguments.iter().enumerate() {
            if index > 1 {
                continue;
            }
            let value = match argument {
                Argument::SpreadElement(_) => {
                    return Err(SourceLoweringError::Unsupported(
                        "spread arguments".to_string(),
                    ));
                }
                _ => self.compile_expression(
                    argument.as_expression().ok_or_else(|| {
                        SourceLoweringError::Unsupported("unsupported call argument".to_string())
                    })?,
                    module,
                )?,
            };

            match index {
                0 => actual = Some(value),
                1 => expected = Some(value),
                _ => unreachable!("additional assert.sameValue args are skipped"),
            }
        }

        let actual = actual.ok_or_else(|| {
            SourceLoweringError::Unsupported(
                "assert.sameValue requires actual and expected arguments".to_string(),
            )
        })?;
        let expected = expected.ok_or_else(|| {
            SourceLoweringError::Unsupported(
                "assert.sameValue requires actual and expected arguments".to_string(),
            )
        })?;

        let comparison = ValueLocation::temp(self.alloc_temp());
        self.instructions.push(Instruction::eq(
            comparison.register,
            actual.register,
            expected.register,
        ));
        let jump_to_end =
            self.emit_conditional_placeholder(Opcode::JumpIfTrue, comparison.register);

        let actual_is_nan =
            if comparison.register != actual.register && comparison.register != expected.register {
                comparison
            } else {
                ValueLocation::temp(self.alloc_temp())
            };
        self.instructions.push(Instruction::eq(
            actual_is_nan.register,
            actual.register,
            actual.register,
        ));
        self.instructions.push(Instruction::not(
            actual_is_nan.register,
            actual_is_nan.register,
        ));
        let jump_to_failure =
            self.emit_conditional_placeholder(Opcode::JumpIfFalse, actual_is_nan.register);

        let expected_is_nan = if actual_is_nan.register != expected.register {
            actual_is_nan
        } else {
            ValueLocation::temp(self.alloc_temp())
        };
        self.instructions.push(Instruction::eq(
            expected_is_nan.register,
            expected.register,
            expected.register,
        ));
        self.instructions.push(Instruction::not(
            expected_is_nan.register,
            expected_is_nan.register,
        ));
        let jump_past_failure =
            self.emit_conditional_placeholder(Opcode::JumpIfTrue, expected_is_nan.register);

        let failure_pc = self.instructions.len();
        let failure = self.load_i32(1)?;
        self.instructions.push(Instruction::ret(failure.register));
        self.release(failure);

        let success_pc = self.instructions.len();
        self.patch_jump(jump_to_failure, failure_pc)?;
        self.patch_jump(jump_to_end, success_pc)?;
        self.patch_jump(jump_past_failure, success_pc)?;
        if comparison.register != actual.register {
            self.release(actual);
        }
        if comparison.register != expected.register {
            self.release(expected);
        }
        if comparison.is_temp {
            self.release(comparison);
        }
        if actual_is_nan.register != comparison.register && actual_is_nan.is_temp {
            self.release(actual_is_nan);
        }
        if expected_is_nan.register != actual_is_nan.register && expected_is_nan.is_temp {
            self.release(expected_is_nan);
        }

        self.load_undefined()
    }
}
