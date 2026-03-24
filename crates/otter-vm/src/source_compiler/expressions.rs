use super::ast::{
    extract_function_params, is_test262_assert_same_value_call, non_computed_property_key_name,
};
use super::module_compiler::{FunctionIdentity, ModuleCompiler};
use super::shared::{Binding, FunctionCompiler, FunctionKind, ValueLocation};
use super::*;

impl<'a> FunctionCompiler<'a> {
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
            Expression::UpdateExpression(update) => self.compile_update_expression(update),
            Expression::CallExpression(call) => self.compile_call_expression(call, module),
            Expression::FunctionExpression(function) => {
                self.compile_function_expression(function, module)
            }
            Expression::ObjectExpression(object) => self.compile_object_expression(object, module),
            Expression::StaticMemberExpression(member) => {
                self.compile_static_member_expression(member, module)
            }
            Expression::ComputedMemberExpression(member) => {
                self.compile_computed_member_expression(member, module)
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
        if !value.is_finite()
            || value.fract() != 0.0
            || value < i32::MIN as f64
            || value > i32::MAX as f64
        {
            return Err(SourceLoweringError::Unsupported(format!(
                "numeric literal {value}"
            )));
        }

        self.load_i32(value as i32)
    }

    fn compile_string_literal(
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
        let register = self.alloc_temp();
        self.instructions.push(Instruction::load_this(register));
        Ok(ValueLocation::temp(register))
    }

    fn compile_identifier(&mut self, name: &str) -> Result<ValueLocation, SourceLoweringError> {
        if name == "undefined" && !self.env.bindings.contains_key(name) {
            return self.load_undefined();
        }
        if self.mode == LoweringMode::Test262Basic
            && name == "NaN"
            && !self.env.bindings.contains_key(name)
        {
            return self.compile_bool(false);
        }

        match self.resolve_binding(name)? {
            Binding::Register(register) => Ok(ValueLocation::local(register)),
            Binding::Function {
                closure_register, ..
            } => Ok(ValueLocation::local(closure_register)),
            Binding::Upvalue(upvalue) => {
                let register = self.alloc_temp();
                self.instructions
                    .push(Instruction::get_upvalue(register, upvalue));
                Ok(ValueLocation::temp(register))
            }
        }
    }

    fn compile_assignment_expression(
        &mut self,
        assignment: &oxc_ast::ast::AssignmentExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        match assignment.operator {
            AssignmentOperator::Assign => match &assignment.left {
                AssignmentTarget::AssignmentTargetIdentifier(identifier) => {
                    let value = self.compile_expression(&assignment.right, module)?;
                    self.assign_to_name(identifier.name.as_str(), value)
                }
                AssignmentTarget::ComputedMemberExpression(member) => {
                    let object = self.compile_expression(&member.object, module)?;
                    let value = self.compile_expression(&assignment.right, module)?;
                    self.store_computed_member(object, module, member, value)?;
                    Ok(value)
                }
                AssignmentTarget::StaticMemberExpression(member) => {
                    let object = self.compile_expression(&member.object, module)?;
                    let value = self.compile_expression(&assignment.right, module)?;
                    let property = self.intern_property_name(member.property.name.as_str())?;
                    self.instructions.push(Instruction::set_property(
                        object.register,
                        value.register,
                        property,
                    ));
                    self.release(object);
                    Ok(value)
                }
                _ => Err(SourceLoweringError::Unsupported(
                    "unsupported assignment target".to_string(),
                )),
            },
            AssignmentOperator::Addition => match &assignment.left {
                AssignmentTarget::AssignmentTargetIdentifier(identifier) => {
                    let current = self.compile_identifier(identifier.name.as_str())?;
                    let current = self.materialize_value(current);
                    let rhs = self.compile_expression(&assignment.right, module)?;
                    let result = if rhs.is_temp {
                        rhs
                    } else if current.is_temp {
                        current
                    } else {
                        ValueLocation::temp(self.alloc_temp())
                    };

                    self.instructions.push(Instruction::add(
                        result.register,
                        current.register,
                        rhs.register,
                    ));

                    if result.register != current.register {
                        self.release(current);
                    }
                    if result.register != rhs.register {
                        self.release(rhs);
                    }

                    self.assign_to_name(identifier.name.as_str(), result)
                }
                _ => Err(SourceLoweringError::Unsupported(
                    "compound assignment target".to_string(),
                )),
            },
            _ => Err(SourceLoweringError::Unsupported(format!(
                "assignment operator {:?}",
                assignment.operator
            ))),
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

        let result = if rhs.is_temp {
            rhs
        } else if lhs.is_temp {
            lhs
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
            BinaryOperator::Equality | BinaryOperator::StrictEquality => {
                self.instructions.push(Instruction::eq(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::Inequality | BinaryOperator::StrictInequality => {
                self.instructions.push(Instruction::eq(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
                self.instructions
                    .push(Instruction::not(result.register, result.register));
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
            _ => {
                return Err(SourceLoweringError::Unsupported(format!(
                    "logical operator {:?}",
                    logical.operator
                )));
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
            UnaryOperator::UnaryPlus => self.compile_expression(&unary.argument, module),
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
            _ => Err(SourceLoweringError::Unsupported(format!(
                "unary operator {:?}",
                unary.operator
            ))),
        }
    }

    fn compile_update_expression(
        &mut self,
        update: &oxc_ast::ast::UpdateExpression<'_>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let SimpleAssignmentTarget::AssignmentTargetIdentifier(identifier) = &update.argument
        else {
            return Err(SourceLoweringError::Unsupported(
                "non-identifier update target".to_string(),
            ));
        };

        let current = self.compile_identifier(identifier.name.as_str())?;
        let delta = match update.operator {
            UpdateOperator::Increment => self.load_i32(1)?,
            UpdateOperator::Decrement => self.load_i32(-1)?,
        };
        let result = if current.is_temp {
            current
        } else {
            ValueLocation::temp(self.alloc_temp())
        };
        self.instructions.push(Instruction::add(
            result.register,
            current.register,
            delta.register,
        ));
        self.release(delta);
        let _ = self.assign_to_name(identifier.name.as_str(), result)?;
        Ok(result)
    }

    fn compile_call_expression(
        &mut self,
        call: &oxc_ast::ast::CallExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        if self.mode == LoweringMode::Test262Basic && is_test262_assert_same_value_call(call) {
            return self.compile_test262_assert_same_value(call, module);
        }

        let (callee, receiver) = self.compile_call_target(&call.callee, module)?;

        let argument_count = RegisterIndex::try_from(call.arguments.len())
            .map_err(|_| SourceLoweringError::TooManyLocals)?;
        let arg_start = if argument_count == 0 {
            BytecodeRegister::new(self.next_local + self.next_temp)
        } else {
            self.reserve_temp_window(argument_count)?
        };

        for (offset, argument) in call.arguments.iter().enumerate() {
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
            let destination = BytecodeRegister::new(arg_start.index() + offset as u16);
            if value.register != destination {
                self.instructions
                    .push(Instruction::move_(destination, value.register));
                self.release(value);
            }
        }

        let result = if receiver.is_some_and(|receiver| receiver.is_temp) {
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
            self.release_temp_window(argument_count);
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

    fn compile_call_target(
        &mut self,
        callee: &Expression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(ValueLocation, Option<ValueLocation>), SourceLoweringError> {
        match callee {
            Expression::Identifier(identifier) => {
                match self.resolve_binding(identifier.name.as_str())? {
                    Binding::Function {
                        closure_register, ..
                    } => Ok((ValueLocation::local(closure_register), None)),
                    _ => {
                        let callee = self.compile_expression(callee, module)?;
                        Ok((self.materialize_value(callee), None))
                    }
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
                let receiver = self.compile_expression(&member.object, module)?;
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

        let comparison = if actual.is_temp {
            actual
        } else if expected.is_temp {
            expected
        } else {
            ValueLocation::temp(self.alloc_temp())
        };
        self.instructions.push(Instruction::eq(
            comparison.register,
            actual.register,
            expected.register,
        ));
        let jump_to_end =
            self.emit_conditional_placeholder(Opcode::JumpIfTrue, comparison.register);

        let failure = self.load_i32(1)?;
        self.instructions.push(Instruction::ret(failure.register));
        self.release(failure);

        self.patch_jump(jump_to_end, self.instructions.len())?;
        if comparison.register != actual.register {
            self.release(actual);
        }
        if comparison.register != expected.register {
            self.release(expected);
        }
        if comparison.is_temp {
            self.release(comparison);
        }

        self.load_undefined()
    }

    fn compile_function_expression(
        &mut self,
        function: &Function<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        if function.id.is_some() {
            return Err(SourceLoweringError::Unsupported(
                "named function expressions".to_string(),
            ));
        }

        let reserved = module.reserve_function();
        let params = extract_function_params(function)?;
        let compiled = module.compile_function_from_statements(
            reserved,
            FunctionIdentity {
                debug_name: self
                    .function_name
                    .as_ref()
                    .map(|name| format!("{name}::<anonymous>")),
                self_binding_name: None,
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
            FunctionKind::Ordinary,
            Some(self.env.clone()),
        )?;
        module.set_function(reserved, compiled.function);

        let destination = self.alloc_temp();
        self.emit_new_closure(destination, reserved, &compiled.captures)?;
        Ok(ValueLocation::temp(destination))
    }

    fn compile_object_expression(
        &mut self,
        object: &oxc_ast::ast::ObjectExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let destination = self.alloc_temp();
        self.instructions.push(Instruction::new_object(destination));

        for property in &object.properties {
            let ObjectPropertyKind::ObjectProperty(property) = property else {
                return Err(SourceLoweringError::Unsupported(
                    "object spread properties".to_string(),
                ));
            };
            if property.kind != PropertyKind::Init {
                return Err(SourceLoweringError::Unsupported(
                    "object getters/setters/methods".to_string(),
                ));
            }
            let name = non_computed_property_key_name(&property.key).ok_or_else(|| {
                SourceLoweringError::Unsupported("computed object property names".to_string())
            })?;
            let property_id = self.intern_property_name(&name)?;
            let value = self.compile_expression(&property.value, module)?;
            self.instructions.push(Instruction::set_property(
                destination,
                value.register,
                property_id,
            ));
            self.release(value);
        }

        Ok(ValueLocation::temp(destination))
    }

    fn compile_array_expression(
        &mut self,
        array: &oxc_ast::ast::ArrayExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let destination = self.alloc_temp();
        self.instructions.push(Instruction::new_array(destination));

        for (index, element) in array.elements.iter().enumerate() {
            let value = match element {
                oxc_ast::ast::ArrayExpressionElement::SpreadElement(_) => {
                    return Err(SourceLoweringError::Unsupported(
                        "array spread elements".to_string(),
                    ));
                }
                oxc_ast::ast::ArrayExpressionElement::Elision(_) => self.load_undefined()?,
                expr => self.compile_expression(expr.to_expression(), module)?,
            };
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
        let object = self.compile_expression(&member.object, module)?;
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

    pub(super) fn store_computed_member(
        &mut self,
        object: ValueLocation,
        module: &mut ModuleCompiler<'a>,
        member: &ComputedMemberExpression<'_>,
        value: ValueLocation,
    ) -> Result<(), SourceLoweringError> {
        match &member.expression {
            Expression::StringLiteral(literal) => {
                let property = self.intern_property_name(literal.value.as_str())?;
                self.instructions.push(Instruction::set_property(
                    object.register,
                    value.register,
                    property,
                ));
            }
            _ => {
                let index = self.compile_expression(&member.expression, module)?;
                self.instructions.push(Instruction::set_index(
                    object.register,
                    index.register,
                    value.register,
                ));
                self.release(index);
            }
        }
        self.release(object);
        Ok(())
    }
}
