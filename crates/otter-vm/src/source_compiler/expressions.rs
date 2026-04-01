use super::ast::{
    expected_function_length, extract_function_params, extract_function_params_from_formal,
    is_test262_assert_same_value_call, non_computed_property_key_name,
};
use super::module_compiler::{FunctionIdentity, ModuleCompiler};
use super::shared::{Binding, FunctionCompiler, FunctionKind, ValueLocation};
use super::*;

impl<'a> FunctionCompiler<'a> {
    pub(super) fn compile_expression_with_inferred_name(
        &mut self,
        expression: &Expression<'_>,
        inferred_name: Option<&str>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        match expression {
            Expression::FunctionExpression(function) => {
                self.compile_function_expression(function, inferred_name, module)
            }
            Expression::ArrowFunctionExpression(arrow) => {
                self.compile_arrow_function_expression(arrow, inferred_name, module)
            }
            _ => self.compile_expression(expression, module),
        }
    }

    pub(super) fn compile_expression(
        &mut self,
        expression: &Expression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        match expression {
            Expression::NumericLiteral(literal) => self.compile_numeric_literal(literal.value),
            Expression::BooleanLiteral(literal) => self.compile_bool(literal.value),
            Expression::NullLiteral(_) => self.load_null(),
            Expression::StringLiteral(literal) => {
                self.compile_string_literal(literal.value.as_str())
            }
            Expression::ThisExpression(_) => self.compile_this_expression(),
            Expression::ArrayExpression(array) => self.compile_array_expression(array, module),
            Expression::Identifier(identifier) => self.compile_identifier(identifier.name.as_str()),
            Expression::ParenthesizedExpression(parenthesized) => {
                self.compile_expression(&parenthesized.expression, module)
            }
            Expression::AssignmentExpression(assignment) => {
                self.compile_assignment_expression(assignment, module)
            }
            Expression::BinaryExpression(binary) => self.compile_binary_expression(binary, module),
            Expression::LogicalExpression(logical) => {
                self.compile_logical_expression(logical, module)
            }
            Expression::UnaryExpression(unary) => self.compile_unary_expression(unary, module),
            Expression::UpdateExpression(update) => self.compile_update_expression(update, module),
            Expression::CallExpression(call) => self.compile_call_expression(call, module),
            Expression::NewExpression(new_expression) => {
                self.compile_new_expression(new_expression, module)
            }
            Expression::FunctionExpression(function) => {
                self.compile_function_expression(function, None, module)
            }
            Expression::ObjectExpression(object) => self.compile_object_expression(object, module),
            Expression::StaticMemberExpression(member) => {
                self.compile_static_member_expression(member, module)
            }
            Expression::ComputedMemberExpression(member) => {
                self.compile_computed_member_expression(member, module)
            }
            Expression::ConditionalExpression(conditional) => {
                self.compile_conditional_expression(conditional, module)
            }
            Expression::ArrowFunctionExpression(arrow) => {
                self.compile_arrow_function_expression(arrow, None, module)
            }
            Expression::TemplateLiteral(template) => {
                self.compile_template_literal(template, module)
            }
            Expression::SequenceExpression(sequence) => {
                self.compile_sequence_expression(sequence, module)
            }
            // §15.7 ClassExpression
            // Spec: <https://tc39.es/ecma262/#sec-class-definitions-runtime-semantics-evaluation>
            Expression::ClassExpression(class) => self.compile_class_expression(class, module),
            // §13.3.7 Optional Chaining (`?.`)
            // Spec: <https://tc39.es/ecma262/#sec-optional-chaining>
            Expression::ChainExpression(chain) => self.compile_chain_expression(chain, module),
            // §14.4 Yield — `yield expr` / `yield`
            // Spec: <https://tc39.es/ecma262/#sec-yield>
            Expression::YieldExpression(yield_expr) => {
                self.compile_yield_expression(yield_expr, module)
            }
            _ => Err(SourceLoweringError::Unsupported(format!(
                "expression {:?}",
                expression
            ))),
        }
    }

    fn compile_numeric_literal(
        &mut self,
        value: f64,
    ) -> Result<ValueLocation, SourceLoweringError> {
        // NaN is handled by a dedicated opcode.
        if value.is_nan() {
            let register = self.alloc_temp();
            self.instructions.push(Instruction::load_nan(register));
            return Ok(ValueLocation::temp(register));
        }
        // Integers that fit in i32 use the compact LoadI32 encoding.
        if value.is_finite()
            && value.fract() == 0.0
            && value >= i32::MIN as f64
            && value <= i32::MAX as f64
        {
            return self.load_i32(value as i32);
        }
        // General float64 values go through the float constant table.
        self.load_f64(value)
    }

    fn load_f64(&mut self, value: f64) -> Result<ValueLocation, SourceLoweringError> {
        let id = if let Some(pos) = self
            .float_constants
            .iter()
            .position(|v| v.to_bits() == value.to_bits())
        {
            FloatId(pos as u16)
        } else {
            let id = FloatId(self.float_constants.len() as u16);
            self.float_constants.push(value);
            id
        };
        let register = self.alloc_temp();
        self.instructions.push(Instruction::load_f64(register, id));
        Ok(ValueLocation::temp(register))
    }

    pub(super) fn compile_string_literal(
        &mut self,
        value: &str,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let register = self.alloc_temp();
        let string_id = self.intern_string(value)?;
        self.instructions
            .push(Instruction::load_string(register, string_id));
        Ok(ValueLocation::temp(register))
    }

    fn compile_this_expression(&mut self) -> Result<ValueLocation, SourceLoweringError> {
        // Arrow functions capture `this` lexically — resolve via the "this" binding.
        if self.kind == FunctionKind::Arrow {
            return self.compile_identifier("this");
        }
        let register = self.alloc_temp();
        self.instructions.push(Instruction::load_this(register));
        Ok(ValueLocation::temp(register))
    }

    pub(super) fn compile_identifier(
        &mut self,
        name: &str,
    ) -> Result<ValueLocation, SourceLoweringError> {
        // Global value identifiers — always available, not bindings.
        if !self.env.bindings.contains_key(name) {
            match name {
                "undefined" => return self.load_undefined(),
                "NaN" => return self.load_nan(),
                "Infinity" => return self.load_f64(f64::INFINITY),
                _ => {}
            }
        }

        match self.resolve_binding(name) {
            Ok(Binding::Register(register)) => {
                if self.parameter_tdz_active {
                    self.emit_assert_not_hole(register);
                }
                Ok(ValueLocation::local(register))
            }
            Ok(Binding::ThisRegister(register)) => {
                self.emit_assert_not_hole(register);
                Ok(ValueLocation::local(register))
            }
            Ok(Binding::Function {
                closure_register, ..
            }) => Ok(ValueLocation::local(closure_register)),
            Ok(Binding::Upvalue(upvalue)) => {
                let register = self.alloc_temp();
                self.instructions
                    .push(Instruction::get_upvalue(register, upvalue));
                Ok(ValueLocation::temp(register))
            }
            Ok(Binding::ThisUpvalue(upvalue)) => {
                let register = self.alloc_temp();
                self.instructions
                    .push(Instruction::get_upvalue(register, upvalue));
                self.emit_assert_not_hole(register);
                Ok(ValueLocation::temp(register))
            }
            Err(SourceLoweringError::UnknownBinding(_)) => {
                // Undeclared variable → runtime global lookup (V8's LdaGlobal).
                let property = self.intern_property_name(name)?;
                let register = self.alloc_temp();
                self.instructions
                    .push(Instruction::get_global(register, property));
                Ok(ValueLocation::temp(register))
            }
            Err(e) => Err(e),
        }
    }

    fn compile_binary_expression(
        &mut self,
        binary: &oxc_ast::ast::BinaryExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let lhs = self.compile_expression(&binary.left, module)?;
        let lhs = self.materialize_value(lhs);
        let rhs = self.compile_expression(&binary.right, module)?;

        let result = if lhs.is_temp {
            lhs
        } else if rhs.is_temp {
            rhs
        } else {
            ValueLocation::temp(self.alloc_temp())
        };

        match binary.operator {
            BinaryOperator::Addition => {
                self.instructions.push(Instruction::add(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::Subtraction => {
                self.instructions.push(Instruction::sub(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::Multiplication => {
                self.instructions.push(Instruction::mul(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::Division => {
                self.instructions.push(Instruction::div(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::LessThan => {
                self.instructions.push(Instruction::lt(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::GreaterThan => {
                self.instructions.push(Instruction::gt(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::GreaterEqualThan => {
                self.instructions.push(Instruction::gte(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::LessEqualThan => {
                self.instructions.push(Instruction::lte(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::Remainder => {
                self.instructions.push(Instruction::mod_(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::Equality => {
                self.instructions.push(Instruction::loose_eq(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::StrictEquality => {
                self.instructions.push(Instruction::eq(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::Inequality => {
                self.instructions.push(Instruction::loose_eq(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
                self.instructions
                    .push(Instruction::not(result.register, result.register));
            }
            BinaryOperator::StrictInequality => {
                self.instructions.push(Instruction::eq(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
                self.instructions
                    .push(Instruction::not(result.register, result.register));
            }
            BinaryOperator::Instanceof => {
                self.instructions.push(Instruction::instance_of(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::In => {
                self.instructions.push(Instruction::has_property(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::BitwiseAnd => {
                self.instructions.push(Instruction::bit_and(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::BitwiseOR => {
                self.instructions.push(Instruction::bit_or(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::BitwiseXOR => {
                self.instructions.push(Instruction::bit_xor(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::ShiftLeft => {
                self.instructions.push(Instruction::shl(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::ShiftRight => {
                self.instructions.push(Instruction::shr(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::ShiftRightZeroFill => {
                self.instructions.push(Instruction::ushr(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            _ => {
                return Err(SourceLoweringError::Unsupported(format!(
                    "binary operator {:?}",
                    binary.operator
                )));
            }
        }

        if result.register == rhs.register {
            self.release(lhs);
        } else if result.register == lhs.register {
            self.release(rhs);
        } else {
            self.release(rhs);
            self.release(lhs);
        }

        Ok(result)
    }

    fn compile_logical_expression(
        &mut self,
        logical: &oxc_ast::ast::LogicalExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let left = self.compile_expression(&logical.left, module)?;
        let result = if left.is_temp {
            left
        } else {
            let register = self.alloc_temp();
            self.instructions
                .push(Instruction::move_(register, left.register));
            ValueLocation::temp(register)
        };

        let short_circuit = match logical.operator {
            LogicalOperator::And => {
                self.emit_conditional_placeholder(Opcode::JumpIfFalse, result.register)
            }
            LogicalOperator::Or => {
                self.emit_conditional_placeholder(Opcode::JumpIfTrue, result.register)
            }
            LogicalOperator::Coalesce => {
                // ?? : short-circuit if LHS is not null/undefined.
                let null_val = self.load_null()?;
                let cmp = ValueLocation::temp(self.alloc_temp());
                self.instructions.push(Instruction::eq(
                    cmp.register,
                    result.register,
                    null_val.register,
                ));
                self.release(null_val);
                let jump_if_null =
                    self.emit_conditional_placeholder(Opcode::JumpIfTrue, cmp.register);

                let undef_val = self.load_undefined()?;
                self.instructions.push(Instruction::eq(
                    cmp.register,
                    result.register,
                    undef_val.register,
                ));
                self.release(undef_val);
                let jump_if_undef =
                    self.emit_conditional_placeholder(Opcode::JumpIfTrue, cmp.register);
                self.release(cmp);

                // Not nullish — skip RHS.
                let skip_rhs = self.emit_jump_placeholder();

                let rhs_start = self.instructions.len();
                self.patch_jump(jump_if_null, rhs_start)?;
                self.patch_jump(jump_if_undef, rhs_start)?;

                let right = self.compile_expression(&logical.right, module)?;
                if right.register != result.register {
                    self.instructions
                        .push(Instruction::move_(result.register, right.register));
                    self.release(right);
                }

                self.patch_jump(skip_rhs, self.instructions.len())?;
                return Ok(result);
            }
        };

        let right = self.compile_expression(&logical.right, module)?;
        if right.register != result.register {
            self.instructions
                .push(Instruction::move_(result.register, right.register));
            self.release(right);
        }
        self.patch_jump(short_circuit, self.instructions.len())?;
        Ok(result)
    }

    fn compile_unary_expression(
        &mut self,
        unary: &oxc_ast::ast::UnaryExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        match unary.operator {
            UnaryOperator::UnaryNegation => {
                let zero = self.load_i32(0)?;
                let argument = self.compile_expression(&unary.argument, module)?;
                let result = if argument.is_temp {
                    argument
                } else {
                    ValueLocation::temp(self.alloc_temp())
                };
                self.instructions.push(Instruction::sub(
                    result.register,
                    zero.register,
                    argument.register,
                ));
                self.release(zero);
                if result.register != argument.register {
                    self.release(argument);
                }
                Ok(result)
            }
            UnaryOperator::UnaryPlus => {
                let argument = self.compile_expression(&unary.argument, module)?;
                let result = if argument.is_temp {
                    argument
                } else {
                    ValueLocation::temp(self.alloc_temp())
                };
                self.instructions
                    .push(Instruction::to_number(result.register, argument.register));
                if result.register != argument.register {
                    self.release(argument);
                }
                Ok(result)
            }
            UnaryOperator::Typeof => {
                // ES2024 §13.5.1: typeof on an unresolvable global reference
                // must return "undefined", not throw ReferenceError.
                if let oxc_ast::ast::Expression::Identifier(ident) = &unary.argument {
                    if !self.env.bindings.contains_key(ident.name.as_str()) {
                        // Global variable — use TypeOfGlobal which doesn't throw.
                        let result = ValueLocation::temp(self.alloc_temp());
                        let prop = self.intern_property_name(ident.name.as_str())?;
                        self.instructions
                            .push(Instruction::type_of_global(result.register, prop));
                        return Ok(result);
                    }
                }
                let value = self.compile_expression(&unary.argument, module)?;
                let result = if value.is_temp {
                    value
                } else {
                    ValueLocation::temp(self.alloc_temp())
                };
                self.instructions
                    .push(Instruction::type_of(result.register, value.register));
                Ok(result)
            }
            UnaryOperator::LogicalNot => {
                let value = self.compile_expression(&unary.argument, module)?;
                let result = if value.is_temp {
                    value
                } else {
                    ValueLocation::temp(self.alloc_temp())
                };
                self.instructions
                    .push(Instruction::not(result.register, value.register));
                Ok(result)
            }
            UnaryOperator::BitwiseNot => {
                let value = self.compile_expression(&unary.argument, module)?;
                let value = self.materialize_value(value);
                let minus_one = self.load_i32(-1)?;
                let result = if value.is_temp {
                    value
                } else {
                    ValueLocation::temp(self.alloc_temp())
                };
                // ~x === x ^ -1
                self.instructions.push(Instruction::bit_xor(
                    result.register,
                    value.register,
                    minus_one.register,
                ));
                self.release(minus_one);
                if result.register != value.register {
                    self.release(value);
                }
                Ok(result)
            }
            UnaryOperator::Void => {
                let value = self.compile_expression(&unary.argument, module)?;
                self.release(value);
                self.load_undefined()
            }
            UnaryOperator::Delete => self.compile_delete_expression(&unary.argument, module),
        }
    }

    /// §13.4 Update Expressions (`x++`, `--x`, `obj.x++`, `arr[i]--`)
    /// Spec: <https://tc39.es/ecma262/#sec-update-expressions>
    fn compile_update_expression(
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
        let delta = match update.operator {
            UpdateOperator::Increment => self.load_i32(1)?,
            UpdateOperator::Decrement => self.load_i32(-1)?,
        };
        let new_val = ValueLocation::temp(self.alloc_temp());
        self.instructions.push(Instruction::add(
            new_val.register,
            current.register,
            delta.register,
        ));
        self.release(delta);
        let _ = self.assign_to_name(name, new_val)?;
        if update.prefix {
            self.release(current);
            Ok(new_val)
        } else {
            self.release(new_val);
            Ok(current)
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

    fn compile_call_expression(
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

        let argument_count = RegisterIndex::try_from(call.arguments.len())
            .map_err(|_| SourceLoweringError::TooManyLocals)?;
        let mut argument_values = Vec::with_capacity(usize::from(argument_count));

        for argument in &call.arguments {
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
            argument_values.push(value);
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
        let pc = self.instructions.len();
        self.instructions.push(Instruction::call_closure(
            result.register,
            callee.register,
            arg_start,
        ));
        let call_site = match receiver {
            Some(receiver) => CallSite::Closure(ClosureCall::new_with_receiver(
                argument_count,
                FrameFlags::new(false, true, false),
                receiver.register,
            )),
            None => CallSite::Closure(ClosureCall::new(argument_count, FrameFlags::empty())),
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
        Ok(result)
    }

    fn compile_super_call_expression(
        &mut self,
        call: &oxc_ast::ast::CallExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        if !self.is_derived_constructor {
            return Err(SourceLoweringError::Unsupported(
                "super() is only supported inside derived class constructors".to_string(),
            ));
        }

        for argument in &call.arguments {
            if matches!(argument, Argument::SpreadElement(_)) {
                return Err(SourceLoweringError::Unsupported(
                    "super(...spread) is not implemented yet on the new VM path".to_string(),
                ));
            }
        }

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
            argument_values.push(value);
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
        if let Some(Binding::ThisRegister(this_register)) = self.env.bindings.get("this").copied()
            && this_register != result.register
        {
            self.instructions
                .push(Instruction::move_(this_register, result.register));
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

    fn compile_new_expression(
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
        let argument_count = RegisterIndex::try_from(arguments.len())
            .map_err(|_| SourceLoweringError::TooManyLocals)?;
        let mut argument_values = Vec::with_capacity(usize::from(argument_count));

        for argument in arguments {
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
            argument_values.push(value);
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

        let mut result = if callee.is_temp {
            callee
        } else {
            ValueLocation::temp(self.alloc_temp())
        };
        let pc = self.instructions.len();
        self.instructions.push(Instruction::call_closure(
            result.register,
            callee.register,
            arg_start,
        ));
        self.record_call_site(
            pc,
            CallSite::Closure(ClosureCall::new(
                argument_count,
                FrameFlags::new(true, true, false),
            )),
        );

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
        if let Some(value) = cleanup
            && value.register != result.register
        {
            self.release(value);
        }
        Ok(result)
    }

    fn compile_call_target(
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
                let receiver = self.compile_expression(&member.object, module)?;
                let callee_register = self.alloc_temp();
                let property = self.intern_property_name(member.property.name.as_str())?;
                self.instructions.push(Instruction::get_property(
                    callee_register,
                    receiver.register,
                    property,
                ));
                Ok((ValueLocation::temp(callee_register), Some(receiver)))
            }
            Expression::ComputedMemberExpression(member) => {
                let mut receiver = self.compile_expression(&member.object, module)?;
                if receiver.is_temp {
                    receiver = self.stabilize_binding_value(receiver)?;
                }
                let callee_register = self.alloc_temp();

                match &member.expression {
                    Expression::StringLiteral(literal) => {
                        let property = self.intern_property_name(literal.value.as_str())?;
                        self.instructions.push(Instruction::get_property(
                            callee_register,
                            receiver.register,
                            property,
                        ));
                    }
                    _ => {
                        let index = self.compile_expression(&member.expression, module)?;
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
                        self.instructions.push(Instruction::get_property(
                            callee_register,
                            object.register,
                            property,
                        ));
                    }
                    _ => {
                        let index = self.compile_expression(&member.expression, module)?;
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

    fn compile_function_expression(
        &mut self,
        function: &Function<'_>,
        inferred_name: Option<&str>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let fn_name = function.id.as_ref().map(|id| id.name.to_string());
        let public_name = fn_name
            .clone()
            .or_else(|| inferred_name.map(ToOwned::to_owned));

        let reserved = module.reserve_function();
        let params = extract_function_params(function)?;
        let compiled = module.compile_function_from_statements(
            reserved,
            FunctionIdentity {
                debug_name: public_name.clone().or_else(|| {
                    self.function_name
                        .as_ref()
                        .map(|name| format!("{name}::<anonymous>"))
                }),
                self_binding_name: fn_name,
                length: expected_function_length(&params),
            },
            function
                .body
                .as_ref()
                .map(|body| body.statements.as_slice())
                .ok_or_else(|| {
                    SourceLoweringError::Unsupported(
                        "function expressions without bodies".to_string(),
                    )
                })?,
            &params,
            if function.generator {
                FunctionKind::Generator
            } else {
                FunctionKind::Ordinary
            },
            Some(self.env.clone()),
            self.strict_mode
                || super::ast::has_use_strict_directive(
                    function
                        .body
                        .as_ref()
                        .map(|body| body.directives.as_slice())
                        .unwrap_or(&[]),
                ),
        )?;
        module.set_function(reserved, compiled.function);

        let destination = self.alloc_temp();
        if function.generator {
            self.emit_new_closure_generator(destination, reserved, &compiled.captures)?;
        } else {
            self.emit_new_closure(destination, reserved, &compiled.captures)?;
        }
        Ok(ValueLocation::temp(destination))
    }

    fn compile_object_expression(
        &mut self,
        object: &oxc_ast::ast::ObjectExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        // Computed property keys may contain call expressions (e.g. IIFEs)
        // whose `stabilize_binding_value` would clobber a temp destination.
        // Pre-allocate a local in that case so it sits below the temp region.
        let has_computed = object.properties.iter().any(|p| {
            matches!(p, ObjectPropertyKind::ObjectProperty(p) if p.computed)
                || matches!(p, ObjectPropertyKind::SpreadProperty(_))
        });
        let (destination, is_local) = if has_computed {
            (self.allocate_local()?, true)
        } else {
            (self.alloc_temp(), false)
        };
        self.instructions.push(Instruction::new_object(destination));

        for property in &object.properties {
            let ObjectPropertyKind::ObjectProperty(property) = property else {
                let ObjectPropertyKind::SpreadProperty(spread) = property else {
                    unreachable!("object literal property kind should be exhaustively handled");
                };
                let source = self.compile_expression(&spread.argument, module)?;
                self.instructions.push(Instruction::copy_data_properties(
                    destination,
                    source.register,
                ));
                self.release(source);
                continue;
            };
            if property.kind != PropertyKind::Init
                && !matches!(property.kind, PropertyKind::Get | PropertyKind::Set)
            {
                return Err(SourceLoweringError::Unsupported(
                    "object getters/setters/methods".to_string(),
                ));
            }
            if matches!(property.kind, PropertyKind::Get | PropertyKind::Set) {
                let inferred_name = if property.computed {
                    None
                } else {
                    let name = non_computed_property_key_name(&property.key).ok_or_else(|| {
                        SourceLoweringError::Unsupported("object accessor property key".to_string())
                    })?;
                    Some(match property.kind {
                        PropertyKind::Get => format!("get {name}"),
                        PropertyKind::Set => format!("set {name}"),
                        _ => unreachable!(),
                    })
                };
                let accessor = self.compile_expression_with_inferred_name(
                    &property.value,
                    inferred_name.as_deref(),
                    module,
                )?;
                if property.computed {
                    let key = self.compile_expression(property.key.to_expression(), module)?;
                    match property.kind {
                        PropertyKind::Get => {
                            self.instructions.push(Instruction::define_computed_getter(
                                destination,
                                key.register,
                                accessor.register,
                            ))
                        }
                        PropertyKind::Set => {
                            self.instructions.push(Instruction::define_computed_setter(
                                destination,
                                key.register,
                                accessor.register,
                            ))
                        }
                        _ => unreachable!(),
                    }
                    self.release(key);
                } else {
                    let name = non_computed_property_key_name(&property.key).ok_or_else(|| {
                        SourceLoweringError::Unsupported("object accessor property key".to_string())
                    })?;
                    let property_id = self.intern_property_name(&name)?;
                    match property.kind {
                        PropertyKind::Get => {
                            self.instructions.push(Instruction::define_named_getter(
                                destination,
                                accessor.register,
                                property_id,
                            ))
                        }
                        PropertyKind::Set => {
                            self.instructions.push(Instruction::define_named_setter(
                                destination,
                                accessor.register,
                                property_id,
                            ))
                        }
                        _ => unreachable!(),
                    }
                }
                self.release(accessor);
                continue;
            }
            if property.computed {
                let key = self.compile_expression(property.key.to_expression(), module)?;
                let value = self.compile_expression(&property.value, module)?;
                self.instructions.push(Instruction::set_index(
                    destination,
                    key.register,
                    value.register,
                ));
                self.release(key);
                self.release(value);
            } else {
                let name = non_computed_property_key_name(&property.key).ok_or_else(|| {
                    SourceLoweringError::Unsupported("object property key".to_string())
                })?;
                let property_id = self.intern_property_name(&name)?;
                let value = self.compile_expression_with_inferred_name(
                    &property.value,
                    Some(&name),
                    module,
                )?;
                self.instructions.push(Instruction::set_property(
                    destination,
                    value.register,
                    property_id,
                ));
                self.release(value);
            }
        }

        if is_local {
            Ok(ValueLocation::local(destination))
        } else {
            Ok(ValueLocation::temp(destination))
        }
    }

    fn compile_array_expression(
        &mut self,
        array: &oxc_ast::ast::ArrayExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let destination = self.alloc_temp();
        let len =
            u16::try_from(array.elements.len()).map_err(|_| SourceLoweringError::TooManyLocals)?;
        self.instructions
            .push(Instruction::new_array(destination, len));

        for (index, element) in array.elements.iter().enumerate() {
            let expr = match element {
                oxc_ast::ast::ArrayExpressionElement::SpreadElement(_) => {
                    return Err(SourceLoweringError::Unsupported(
                        "array spread elements".to_string(),
                    ));
                }
                oxc_ast::ast::ArrayExpressionElement::Elision(_) => continue,
                expr => expr.to_expression(),
            };

            let value = self.compile_expression(expr, module)?;
            let index_value = self
                .load_i32(i32::try_from(index).map_err(|_| SourceLoweringError::TooManyLocals)?)?;
            self.instructions.push(Instruction::set_index(
                destination,
                index_value.register,
                value.register,
            ));
            self.release(index_value);
            self.release(value);
        }

        Ok(ValueLocation::temp(destination))
    }

    fn compile_static_member_expression(
        &mut self,
        member: &StaticMemberExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let object = self.compile_expression(&member.object, module)?;
        let result = if object.is_temp {
            object
        } else {
            ValueLocation::temp(self.alloc_temp())
        };
        let property = self.intern_property_name(member.property.name.as_str())?;
        self.instructions.push(Instruction::get_property(
            result.register,
            object.register,
            property,
        ));
        if result.register != object.register {
            self.release(object);
        }
        Ok(result)
    }

    fn compile_computed_member_expression(
        &mut self,
        member: &ComputedMemberExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let mut object = self.compile_expression(&member.object, module)?;
        if object.is_temp {
            object = self.stabilize_binding_value(object)?;
        }
        let result = if object.is_temp {
            object
        } else {
            ValueLocation::temp(self.alloc_temp())
        };

        match &member.expression {
            Expression::StringLiteral(literal) => {
                let property = self.intern_property_name(literal.value.as_str())?;
                self.instructions.push(Instruction::get_property(
                    result.register,
                    object.register,
                    property,
                ));
            }
            _ => {
                let index = self.compile_expression(&member.expression, module)?;
                self.instructions.push(Instruction::get_index(
                    result.register,
                    object.register,
                    index.register,
                ));
                self.release(index);
            }
        }

        if result.register != object.register {
            self.release(object);
        }
        Ok(result)
    }

    fn compile_delete_expression(
        &mut self,
        argument: &Expression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        match argument {
            Expression::StaticMemberExpression(member) => {
                let object = self.compile_expression(&member.object, module)?;
                let result = if object.is_temp {
                    object
                } else {
                    ValueLocation::temp(self.alloc_temp())
                };
                let property = self.intern_property_name(member.property.name.as_str())?;
                self.instructions.push(Instruction::delete_property(
                    result.register,
                    object.register,
                    property,
                ));
                if result.register != object.register {
                    self.release(object);
                }
                Ok(result)
            }
            Expression::ComputedMemberExpression(member) => {
                // Optimise: if the key is a string literal, use the named delete path.
                if let Expression::StringLiteral(literal) = &member.expression {
                    let object = self.compile_expression(&member.object, module)?;
                    let result = if object.is_temp {
                        object
                    } else {
                        ValueLocation::temp(self.alloc_temp())
                    };
                    let property = self.intern_property_name(literal.value.as_str())?;
                    self.instructions.push(Instruction::delete_property(
                        result.register,
                        object.register,
                        property,
                    ));
                    if result.register != object.register {
                        self.release(object);
                    }
                    return Ok(result);
                }

                // General case: dynamic key — emit DeleteComputed.
                let mut object = self.compile_expression(&member.object, module)?;
                if object.is_temp {
                    object = self.stabilize_binding_value(object)?;
                }
                let key = self.compile_expression(&member.expression, module)?;
                let result = if object.is_temp {
                    object
                } else {
                    ValueLocation::temp(self.alloc_temp())
                };
                self.instructions.push(Instruction::delete_computed(
                    result.register,
                    object.register,
                    key.register,
                ));
                self.release(key);
                if result.register != object.register {
                    self.release(object);
                }
                Ok(result)
            }
            _ => Err(SourceLoweringError::Unsupported(
                "delete target".to_string(),
            )),
        }
    }

    fn compile_arrow_function_expression(
        &mut self,
        arrow: &oxc_ast::ast::ArrowFunctionExpression<'_>,
        inferred_name: Option<&str>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let public_name = inferred_name.map(ToOwned::to_owned);
        let reserved = module.reserve_function();
        let params = extract_function_params_from_formal(&arrow.params)?;

        let compiled = if arrow.expression {
            let body_statements = &arrow.body.statements;
            let expression = match body_statements.first() {
                Some(AstStatement::ExpressionStatement(expr_stmt)) => &expr_stmt.expression,
                _ => {
                    return Err(SourceLoweringError::Unsupported(
                        "arrow expression body without expression statement".to_string(),
                    ));
                }
            };
            module.compile_function_from_expression(
                reserved,
                FunctionIdentity {
                    debug_name: public_name.clone().or_else(|| {
                        self.function_name
                            .as_ref()
                            .map(|name| format!("{name}::<arrow>"))
                    }),
                    self_binding_name: None,
                    length: expected_function_length(&params),
                },
                expression,
                &params,
                FunctionKind::Arrow,
                Some(self.env.clone()),
                self.strict_mode,
            )?
        } else {
            module.compile_function_from_statements(
                reserved,
                FunctionIdentity {
                    debug_name: public_name.clone().or_else(|| {
                        self.function_name
                            .as_ref()
                            .map(|name| format!("{name}::<arrow>"))
                    }),
                    self_binding_name: None,
                    length: expected_function_length(&params),
                },
                &arrow.body.statements,
                &params,
                FunctionKind::Arrow,
                Some(self.env.clone()),
                self.strict_mode
                    || super::ast::has_use_strict_directive(arrow.body.directives.as_slice()),
            )?
        };
        module.set_function(reserved, compiled.function);

        let destination = self.alloc_temp();
        self.emit_new_closure_arrow(destination, reserved, &compiled.captures)?;
        Ok(ValueLocation::temp(destination))
    }

    fn compile_conditional_expression(
        &mut self,
        conditional: &oxc_ast::ast::ConditionalExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let test = self.compile_expression(&conditional.test, module)?;
        let jump_to_alternate =
            self.emit_conditional_placeholder(Opcode::JumpIfFalse, test.register);
        self.release(test);

        let consequent = self.compile_expression(&conditional.consequent, module)?;
        let result = if consequent.is_temp {
            consequent
        } else {
            let result = ValueLocation::temp(self.alloc_temp());
            self.instructions
                .push(Instruction::move_(result.register, consequent.register));
            self.release(consequent);
            result
        };
        let jump_to_end = self.emit_jump_placeholder();

        self.patch_jump(jump_to_alternate, self.instructions.len())?;

        let alternate = self.compile_expression(&conditional.alternate, module)?;
        self.instructions
            .push(Instruction::move_(result.register, alternate.register));
        if alternate.register != result.register {
            self.release(alternate);
        }

        self.patch_jump(jump_to_end, self.instructions.len())?;

        Ok(result)
    }

    /// Sequence expression: `(a, b, c)` — evaluates all, returns last.
    fn compile_sequence_expression(
        &mut self,
        sequence: &oxc_ast::ast::SequenceExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let mut result = None;
        for expression in &sequence.expressions {
            if let Some(prev) = result {
                self.release(prev);
            }
            result = Some(self.compile_expression(expression, module)?);
        }
        result.ok_or_else(|| {
            SourceLoweringError::Unsupported("empty sequence expression".to_string())
        })
    }

    /// §14.4 Yield — `yield expr` or bare `yield` (produces undefined).
    /// Spec: <https://tc39.es/ecma262/#sec-yield>
    ///
    /// Emits `Yield dst, value` which suspends the generator.
    /// On resume, the sent value (from `.next(v)`) is written to `dst`.
    fn compile_yield_expression(
        &mut self,
        yield_expr: &oxc_ast::ast::YieldExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        if yield_expr.delegate {
            // yield* — delegate to sub-iterator. Not yet implemented.
            return Err(SourceLoweringError::Unsupported(
                "yield* (delegating yield) is not implemented yet".to_string(),
            ));
        }

        let value = if let Some(argument) = &yield_expr.argument {
            self.compile_expression(argument, module)?
        } else {
            self.load_undefined()?
        };

        // Yield suspends execution and returns `value` to the caller.
        // The register `dst` will receive the sent value when resumed.
        let dst = self.allocate_local()?;
        self.instructions
            .push(Instruction::yield_(dst, value.register));
        self.release(value);
        Ok(ValueLocation::local(dst))
    }

    /// §13.3.7 Optional Chaining — `obj?.prop`, `obj?.[key]`, `obj?.method()`
    /// Spec: <https://tc39.es/ecma262/#sec-optional-chaining>
    ///
    /// Strategy: extract the base object, check if nullish, short-circuit to
    /// undefined if so. Otherwise perform the member access / call normally.
    fn compile_chain_expression(
        &mut self,
        chain: &oxc_ast::ast::ChainExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        use oxc_ast::ast::ChainElement;

        // Pre-allocate result with `undefined` for the short-circuit path.
        let result = self.allocate_local()?;
        self.instructions.push(Instruction::load_undefined(result));

        match &chain.expression {
            ChainElement::StaticMemberExpression(member) => {
                let base = self.compile_expression(&member.object, module)?;
                let base = self.stabilize_binding_value(base)?;

                // if (base == null) jump to end
                let null_val = self.load_null()?;
                let is_nullish = ValueLocation::temp(self.alloc_temp());
                self.instructions.push(Instruction::loose_eq(
                    is_nullish.register,
                    base.register,
                    null_val.register,
                ));
                self.release(null_val);
                let jump_end =
                    self.emit_conditional_placeholder(Opcode::JumpIfTrue, is_nullish.register);
                self.release(is_nullish);

                let prop = self.intern_property_name(member.property.name.as_str())?;
                let val = ValueLocation::temp(self.alloc_temp());
                self.instructions.push(Instruction::get_property(
                    val.register,
                    base.register,
                    prop,
                ));
                self.instructions
                    .push(Instruction::move_(result, val.register));
                self.release(val);
                let end = self.instructions.len();
                self.patch_jump(jump_end, end)?;
            }
            ChainElement::ComputedMemberExpression(member) => {
                let base = self.compile_expression(&member.object, module)?;
                let base = self.stabilize_binding_value(base)?;

                let null_val = self.load_null()?;
                let is_nullish = ValueLocation::temp(self.alloc_temp());
                self.instructions.push(Instruction::loose_eq(
                    is_nullish.register,
                    base.register,
                    null_val.register,
                ));
                self.release(null_val);
                let jump_end =
                    self.emit_conditional_placeholder(Opcode::JumpIfTrue, is_nullish.register);
                self.release(is_nullish);

                let key = self.compile_expression(&member.expression, module)?;
                let val = ValueLocation::temp(self.alloc_temp());
                self.instructions.push(Instruction::get_index(
                    val.register,
                    base.register,
                    key.register,
                ));
                self.release(key);
                self.instructions
                    .push(Instruction::move_(result, val.register));
                self.release(val);
                let end = self.instructions.len();
                self.patch_jump(jump_end, end)?;
            }
            ChainElement::CallExpression(_call) => {
                // TODO: optional call `obj?.method()` — fall back for now.
                return Err(SourceLoweringError::Unsupported(
                    "optional call expressions (?.) are not yet implemented".to_string(),
                ));
            }
            _ => {
                return Err(SourceLoweringError::Unsupported(
                    "unsupported chain element type".to_string(),
                ));
            }
        }
        Ok(ValueLocation::local(result))
    }

    /// §15.7 ClassExpression — `let x = class [Name] { ... }`
    /// Spec: <https://tc39.es/ecma262/#sec-class-definitions-runtime-semantics-evaluation>
    fn compile_class_expression(
        &mut self,
        class: &oxc_ast::ast::Class<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        use oxc_ast::ast::{ClassElement, MethodDefinitionKind};
        let class_name = class
            .id
            .as_ref()
            .map(|id| id.name.as_str())
            .unwrap_or("anonymous");

        // First pass: extract constructor.
        let mut constructor = None;
        for element in &class.body.body {
            match element {
                ClassElement::MethodDefinition(method)
                    if matches!(method.kind, MethodDefinitionKind::Constructor) =>
                {
                    constructor = Some(&method.value);
                }
                ClassElement::MethodDefinition(_) => {}
                _ => {
                    return Err(SourceLoweringError::Unsupported(
                        "unsupported class expression element".to_string(),
                    ));
                }
            }
        }

        let super_class = if let Some(super_class) = class.super_class.as_ref() {
            let super_value = self.compile_expression(super_class, module)?;
            Some(self.stabilize_binding_value(super_value)?)
        } else {
            None
        };

        let constructor_value = if let Some(ctor) = constructor {
            self.compile_class_constructor(class_name, ctor, super_class.is_some(), module)?
        } else if super_class.is_some() {
            self.compile_default_derived_class_constructor(class_name, module)?
        } else {
            self.compile_default_base_class_constructor(class_name, module)?
        };
        let constructor_value = if constructor_value.is_temp {
            self.stabilize_binding_value(constructor_value)?
        } else {
            constructor_value
        };

        if let Some(super_class) = super_class {
            self.emit_object_method_call(
                "setPrototypeOf",
                constructor_value,
                &[super_class],
                module,
            )?;
        }

        let prototype = self.emit_named_property_load(constructor_value, "prototype")?;
        let prototype = self.stabilize_binding_value(prototype)?;
        let prototype_parent = if let Some(super_class) = super_class {
            self.emit_named_property_load(super_class, "prototype")?
        } else {
            let object_ctor = self.compile_identifier("Object")?;
            let object_ctor = if object_ctor.is_temp {
                self.stabilize_binding_value(object_ctor)?
            } else {
                object_ctor
            };
            self.emit_named_property_load(object_ctor, "prototype")?
        };
        self.emit_object_method_call("setPrototypeOf", prototype, &[prototype_parent], module)?;

        // Install methods.
        for element in &class.body.body {
            if let ClassElement::MethodDefinition(method) = element {
                if !matches!(method.kind, MethodDefinitionKind::Constructor) {
                    let target = if method.r#static {
                        constructor_value
                    } else {
                        prototype
                    };
                    self.compile_class_method(method, target, module)?;
                }
            }
        }

        self.emit_make_class_prototype_non_writable(constructor_value, module)?;
        Ok(constructor_value)
    }

    /// Template literal: `` `prefix${expr}mid${expr}suffix` ``
    /// Compiles to a chain of string concatenations via Add.
    fn compile_template_literal(
        &mut self,
        template: &oxc_ast::ast::TemplateLiteral<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        // quasis[0] expr[0] quasis[1] expr[1] ... quasis[N]
        // quasis always has one more element than expressions.
        let first_quasi = &template.quasis[0];
        let cooked = first_quasi
            .value
            .cooked
            .as_ref()
            .map(|s| s.as_str())
            .unwrap_or("");
        let mut result = self.compile_string_literal(cooked)?;

        for (i, expression) in template.expressions.iter().enumerate() {
            let expr_val = self.compile_expression(expression, module)?;
            let expr_string = ValueLocation::temp(self.alloc_temp());
            self.instructions.push(Instruction::to_string(
                expr_string.register,
                expr_val.register,
            ));
            self.release(expr_val);
            let dst = if result.is_temp {
                result
            } else {
                ValueLocation::temp(self.alloc_temp())
            };
            self.instructions.push(Instruction::add(
                dst.register,
                result.register,
                expr_string.register,
            ));
            if dst.register != result.register {
                self.release(result);
            }
            self.release(expr_string);
            result = dst;

            // Append the next quasi (string part after the expression).
            let quasi = &template.quasis[i + 1];
            let quasi_str = quasi
                .value
                .cooked
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or("");
            if !quasi_str.is_empty() {
                let str_val = self.compile_string_literal(quasi_str)?;
                let dst = if result.is_temp {
                    result
                } else {
                    ValueLocation::temp(self.alloc_temp())
                };
                self.instructions.push(Instruction::add(
                    dst.register,
                    result.register,
                    str_val.register,
                ));
                if dst.register != result.register {
                    self.release(result);
                }
                self.release(str_val);
                result = dst;
            }
        }

        Ok(result)
    }
}
