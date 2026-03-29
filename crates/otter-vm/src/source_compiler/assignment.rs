use super::ast::inferred_name_for_assignment_target;
use super::module_compiler::ModuleCompiler;
use super::shared::{FunctionCompiler, ValueLocation};
use super::*;

impl<'a> FunctionCompiler<'a> {
    pub(super) fn compile_assignment_expression(
        &mut self,
        assignment: &oxc_ast::ast::AssignmentExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        match assignment.operator {
            AssignmentOperator::Assign => match &assignment.left {
                AssignmentTarget::AssignmentTargetIdentifier(identifier) => {
                    let value = self.compile_expression_with_inferred_name(
                        &assignment.right,
                        Some(identifier.name.as_str()),
                        module,
                    )?;
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
                AssignmentTarget::ArrayAssignmentTarget(_)
                | AssignmentTarget::ObjectAssignmentTarget(_) => {
                    let value = self.compile_expression_with_inferred_name(
                        &assignment.right,
                        inferred_name_for_assignment_target(&assignment.left),
                        module,
                    )?;
                    let value = self.stabilize_binding_value(value)?;
                    self.compile_assignment_target(&assignment.left, value, module)?;
                    Ok(value)
                }
                _ => Err(SourceLoweringError::Unsupported(
                    "unsupported assignment target".to_string(),
                )),
            },
            AssignmentOperator::Addition
            | AssignmentOperator::Subtraction
            | AssignmentOperator::Multiplication
            | AssignmentOperator::Division
            | AssignmentOperator::Remainder
            | AssignmentOperator::BitwiseAnd
            | AssignmentOperator::BitwiseOR
            | AssignmentOperator::BitwiseXOR
            | AssignmentOperator::ShiftLeft
            | AssignmentOperator::ShiftRight
            | AssignmentOperator::ShiftRightZeroFill => {
                self.compile_compound_assignment(assignment, module)
            }
            _ => Err(SourceLoweringError::Unsupported(format!(
                "assignment operator {:?}",
                assignment.operator
            ))),
        }
    }

    fn emit_compound_op(
        &mut self,
        op: AssignmentOperator,
        dst: BytecodeRegister,
        lhs: BytecodeRegister,
        rhs: BytecodeRegister,
    ) {
        let instr = match op {
            AssignmentOperator::Addition => Instruction::add(dst, lhs, rhs),
            AssignmentOperator::Subtraction => Instruction::sub(dst, lhs, rhs),
            AssignmentOperator::Multiplication => Instruction::mul(dst, lhs, rhs),
            AssignmentOperator::Division => Instruction::div(dst, lhs, rhs),
            AssignmentOperator::Remainder => Instruction::mod_(dst, lhs, rhs),
            AssignmentOperator::BitwiseAnd => Instruction::bit_and(dst, lhs, rhs),
            AssignmentOperator::BitwiseOR => Instruction::bit_or(dst, lhs, rhs),
            AssignmentOperator::BitwiseXOR => Instruction::bit_xor(dst, lhs, rhs),
            AssignmentOperator::ShiftLeft => Instruction::shl(dst, lhs, rhs),
            AssignmentOperator::ShiftRight => Instruction::shr(dst, lhs, rhs),
            AssignmentOperator::ShiftRightZeroFill => Instruction::ushr(dst, lhs, rhs),
            _ => unreachable!(),
        };
        self.instructions.push(instr);
    }

    fn compile_compound_assignment(
        &mut self,
        assignment: &oxc_ast::ast::AssignmentExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        match &assignment.left {
            AssignmentTarget::AssignmentTargetIdentifier(identifier) => {
                let current = self.compile_identifier(identifier.name.as_str())?;
                let current = self.materialize_value(current);
                let rhs = self.compile_expression(&assignment.right, module)?;
                let result = if current.is_temp {
                    current
                } else if rhs.is_temp {
                    rhs
                } else {
                    ValueLocation::temp(self.alloc_temp())
                };
                self.emit_compound_op(
                    assignment.operator,
                    result.register,
                    current.register,
                    rhs.register,
                );
                if result.register != current.register {
                    self.release(current);
                }
                if result.register != rhs.register {
                    self.release(rhs);
                }
                self.assign_to_name(identifier.name.as_str(), result)
            }
            AssignmentTarget::StaticMemberExpression(member) => {
                let object = self.compile_expression(&member.object, module)?;
                let object = self.materialize_value(object);
                let property = self.intern_property_name(member.property.name.as_str())?;
                let current = ValueLocation::temp(self.alloc_temp());
                self.instructions.push(Instruction::get_property(
                    current.register,
                    object.register,
                    property,
                ));
                let rhs = self.compile_expression(&assignment.right, module)?;
                self.emit_compound_op(
                    assignment.operator,
                    current.register,
                    current.register,
                    rhs.register,
                );
                self.release(rhs);
                self.instructions.push(Instruction::set_property(
                    object.register,
                    current.register,
                    property,
                ));
                self.release(object);
                Ok(current)
            }
            AssignmentTarget::ComputedMemberExpression(member) => {
                let object = self.compile_expression(&member.object, module)?;
                let object = self.materialize_value(object);
                let index = self.compile_expression(&member.expression, module)?;
                let index = self.materialize_value(index);
                let current = ValueLocation::temp(self.alloc_temp());
                self.instructions.push(Instruction::get_index(
                    current.register,
                    object.register,
                    index.register,
                ));
                let rhs = self.compile_expression(&assignment.right, module)?;
                self.emit_compound_op(
                    assignment.operator,
                    current.register,
                    current.register,
                    rhs.register,
                );
                self.release(rhs);
                self.instructions.push(Instruction::set_index(
                    object.register,
                    index.register,
                    current.register,
                ));
                self.release(index);
                self.release(object);
                Ok(current)
            }
            _ => Err(SourceLoweringError::Unsupported(
                "compound assignment target".to_string(),
            )),
        }
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
