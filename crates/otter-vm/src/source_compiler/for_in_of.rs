use super::*;
use oxc_ast::ast::{
    ArrayAssignmentTarget, ComputedMemberExpression, Expression, ForStatementLeft,
    IdentifierReference, ObjectAssignmentTarget, PrivateFieldExpression, StaticMemberExpression,
};

pub(super) enum ForInOfLeft<'a> {
    Identifier(&'a IdentifierReference<'a>),
    AssignmentTarget(ForInOfAssignmentTarget<'a>),
}

pub(super) enum ForInOfAssignmentTarget<'a> {
    Array(&'a ArrayAssignmentTarget<'a>),
    Object(&'a ObjectAssignmentTarget<'a>),
    StaticMember(&'a StaticMemberExpression<'a>),
    ComputedMember(&'a ComputedMemberExpression<'a>),
    PrivateField(&'a PrivateFieldExpression<'a>),
}

pub(super) fn classify_for_in_of_left<'a>(
    left: &'a ForStatementLeft<'a>,
    unsupported_tag: &'static str,
) -> Result<ForInOfLeft<'a>, SourceLoweringError> {
    match left {
        ForStatementLeft::VariableDeclaration(_) => Err(SourceLoweringError::Internal(
            "classify_for_in_of_left called with VariableDeclaration".into(),
        )),
        ForStatementLeft::AssignmentTargetIdentifier(ident) => Ok(ForInOfLeft::Identifier(ident)),
        ForStatementLeft::ArrayAssignmentTarget(pattern) => Ok(ForInOfLeft::AssignmentTarget(
            ForInOfAssignmentTarget::Array(pattern),
        )),
        ForStatementLeft::ObjectAssignmentTarget(pattern) => Ok(ForInOfLeft::AssignmentTarget(
            ForInOfAssignmentTarget::Object(pattern),
        )),
        ForStatementLeft::StaticMemberExpression(member) => Ok(ForInOfLeft::AssignmentTarget(
            ForInOfAssignmentTarget::StaticMember(member),
        )),
        ForStatementLeft::ComputedMemberExpression(member) => Ok(ForInOfLeft::AssignmentTarget(
            ForInOfAssignmentTarget::ComputedMember(member),
        )),
        ForStatementLeft::PrivateFieldExpression(member) => Ok(ForInOfLeft::AssignmentTarget(
            ForInOfAssignmentTarget::PrivateField(member),
        )),
        ForStatementLeft::TSAsExpression(expr) => {
            classify_for_in_of_target_expression(&expr.expression, unsupported_tag)
        }
        ForStatementLeft::TSSatisfiesExpression(expr) => {
            classify_for_in_of_target_expression(&expr.expression, unsupported_tag)
        }
        ForStatementLeft::TSNonNullExpression(expr) => {
            classify_for_in_of_target_expression(&expr.expression, unsupported_tag)
        }
        ForStatementLeft::TSTypeAssertion(expr) => {
            classify_for_in_of_target_expression(&expr.expression, unsupported_tag)
        }
    }
}

fn classify_for_in_of_target_expression<'a>(
    expr: &'a Expression<'a>,
    unsupported_tag: &'static str,
) -> Result<ForInOfLeft<'a>, SourceLoweringError> {
    match expr {
        Expression::Identifier(ident) => Ok(ForInOfLeft::Identifier(ident)),
        Expression::ArrayExpression(_) | Expression::ObjectExpression(_) => Err(
            SourceLoweringError::unsupported(unsupported_tag, expr.span()),
        ),
        Expression::StaticMemberExpression(member) => Ok(ForInOfLeft::AssignmentTarget(
            ForInOfAssignmentTarget::StaticMember(member),
        )),
        Expression::ComputedMemberExpression(member) => Ok(ForInOfLeft::AssignmentTarget(
            ForInOfAssignmentTarget::ComputedMember(member),
        )),
        Expression::PrivateFieldExpression(member) => Ok(ForInOfLeft::AssignmentTarget(
            ForInOfAssignmentTarget::PrivateField(member),
        )),
        Expression::ParenthesizedExpression(paren) => {
            classify_for_in_of_target_expression(&paren.expression, unsupported_tag)
        }
        Expression::TSAsExpression(expr) => {
            classify_for_in_of_target_expression(&expr.expression, unsupported_tag)
        }
        Expression::TSSatisfiesExpression(expr) => {
            classify_for_in_of_target_expression(&expr.expression, unsupported_tag)
        }
        Expression::TSNonNullExpression(expr) => {
            classify_for_in_of_target_expression(&expr.expression, unsupported_tag)
        }
        Expression::TSTypeAssertion(expr) => {
            classify_for_in_of_target_expression(&expr.expression, unsupported_tag)
        }
        _ => Err(SourceLoweringError::unsupported(
            unsupported_tag,
            expr.span(),
        )),
    }
}

pub(super) fn lower_for_in_of_assignment_target<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    target: ForInOfAssignmentTarget<'a>,
    iter_value_reg: u16,
) -> Result<(), SourceLoweringError> {
    match target {
        ForInOfAssignmentTarget::Array(pattern) => {
            destructure_array_assignment_from_temp(builder, ctx, pattern, iter_value_reg)
        }
        ForInOfAssignmentTarget::Object(pattern) => {
            destructure_object_assignment_from_temp(builder, ctx, pattern, iter_value_reg)
        }
        ForInOfAssignmentTarget::StaticMember(member) => {
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(iter_value_reg))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Ldar (for-in/of static target): {err:?}"
                    ))
                })?;
            assign_static_member(builder, ctx, member)
        }
        ForInOfAssignmentTarget::ComputedMember(member) => {
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(iter_value_reg))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Ldar (for-in/of computed target): {err:?}"
                    ))
                })?;
            assign_computed_member(builder, ctx, member)
        }
        ForInOfAssignmentTarget::PrivateField(member) => {
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(iter_value_reg))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Ldar (for-in/of private target): {err:?}"
                    ))
                })?;
            assign_private_field(builder, ctx, member)
        }
    }
}

fn assign_private_field<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    member: &'a PrivateFieldExpression<'a>,
) -> Result<(), SourceLoweringError> {
    if member.optional {
        return Err(SourceLoweringError::unsupported(
            "parser_recovery_optional_member_assignment",
            member.span,
        ));
    }

    let name = member.field.name.as_str();
    enforce_private_name_declared(ctx, name, member.span)?;
    let idx = ctx.intern_property_name(name)?;
    let value_temp = ctx.acquire_temps(1)?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(value_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (destruct private val): {err:?}"
                ))
            })?;
        let base = materialize_member_base(builder, ctx, &member.object)?;
        let write = (|| -> Result<(), SourceLoweringError> {
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(value_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Ldar (destruct private reload): {err:?}"
                    ))
                })?;
            builder
                .emit(
                    Opcode::SetPrivateField,
                    &[Operand::Reg(u32::from(base.reg)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode SetPrivateField (destruct private): {err:?}"
                    ))
                })?;
            Ok(())
        })();
        if base.temp_count != 0 {
            ctx.release_temps(base.temp_count);
        }
        write
    })();
    ctx.release_temps(1);
    lower
}
