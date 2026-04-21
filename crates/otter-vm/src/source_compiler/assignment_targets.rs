use super::*;
use oxc_ast::ast::{
    ArrayAssignmentTarget, AssignmentTarget, ComputedMemberExpression, Expression,
    IdentifierReference, ObjectAssignmentTarget, PrivateFieldExpression, SimpleAssignmentTarget,
    StaticMemberExpression,
};

pub(super) enum AssignmentTargetRef<'a> {
    Identifier(&'a IdentifierReference<'a>),
    StaticMember(&'a StaticMemberExpression<'a>),
    ComputedMember(&'a ComputedMemberExpression<'a>),
    PrivateField(&'a PrivateFieldExpression<'a>),
    Array(&'a ArrayAssignmentTarget<'a>),
    Object(&'a ObjectAssignmentTarget<'a>),
}

pub(super) enum SimpleAssignmentTargetRef<'a> {
    Identifier(&'a IdentifierReference<'a>),
    StaticMember(&'a StaticMemberExpression<'a>),
    ComputedMember(&'a ComputedMemberExpression<'a>),
    PrivateField(&'a PrivateFieldExpression<'a>),
}

pub(super) fn unwrap_assignment_target<'a>(
    target: &'a AssignmentTarget<'a>,
) -> Result<AssignmentTargetRef<'a>, SourceLoweringError> {
    match target {
        AssignmentTarget::AssignmentTargetIdentifier(ident) => {
            Ok(AssignmentTargetRef::Identifier(ident.as_ref()))
        }
        AssignmentTarget::StaticMemberExpression(member) => {
            Ok(AssignmentTargetRef::StaticMember(member))
        }
        AssignmentTarget::ComputedMemberExpression(member) => {
            Ok(AssignmentTargetRef::ComputedMember(member))
        }
        AssignmentTarget::PrivateFieldExpression(member) => {
            Ok(AssignmentTargetRef::PrivateField(member))
        }
        AssignmentTarget::ArrayAssignmentTarget(pattern) => Ok(AssignmentTargetRef::Array(pattern)),
        AssignmentTarget::ObjectAssignmentTarget(pattern) => {
            Ok(AssignmentTargetRef::Object(pattern))
        }
        AssignmentTarget::TSAsExpression(expr) => {
            unwrap_assignment_target_expression(&expr.expression, target.span())
        }
        AssignmentTarget::TSSatisfiesExpression(expr) => {
            unwrap_assignment_target_expression(&expr.expression, target.span())
        }
        AssignmentTarget::TSNonNullExpression(expr) => {
            unwrap_assignment_target_expression(&expr.expression, target.span())
        }
        AssignmentTarget::TSTypeAssertion(expr) => {
            unwrap_assignment_target_expression(&expr.expression, target.span())
        }
    }
}

pub(super) fn unwrap_simple_assignment_target<'a>(
    target: &'a SimpleAssignmentTarget<'a>,
) -> Result<SimpleAssignmentTargetRef<'a>, SourceLoweringError> {
    match target {
        SimpleAssignmentTarget::AssignmentTargetIdentifier(ident) => {
            Ok(SimpleAssignmentTargetRef::Identifier(ident.as_ref()))
        }
        SimpleAssignmentTarget::StaticMemberExpression(member) => {
            Ok(SimpleAssignmentTargetRef::StaticMember(member))
        }
        SimpleAssignmentTarget::ComputedMemberExpression(member) => {
            Ok(SimpleAssignmentTargetRef::ComputedMember(member))
        }
        SimpleAssignmentTarget::PrivateFieldExpression(member) => {
            Ok(SimpleAssignmentTargetRef::PrivateField(member))
        }
        SimpleAssignmentTarget::TSAsExpression(expr) => {
            unwrap_simple_assignment_target_expression(&expr.expression, target.span())
        }
        SimpleAssignmentTarget::TSSatisfiesExpression(expr) => {
            unwrap_simple_assignment_target_expression(&expr.expression, target.span())
        }
        SimpleAssignmentTarget::TSNonNullExpression(expr) => {
            unwrap_simple_assignment_target_expression(&expr.expression, target.span())
        }
        SimpleAssignmentTarget::TSTypeAssertion(expr) => {
            unwrap_simple_assignment_target_expression(&expr.expression, target.span())
        }
    }
}

fn unwrap_assignment_target_expression<'a>(
    expr: &'a Expression<'a>,
    target_span: Span,
) -> Result<AssignmentTargetRef<'a>, SourceLoweringError> {
    match expr {
        Expression::Identifier(ident) => Ok(AssignmentTargetRef::Identifier(ident)),
        Expression::StaticMemberExpression(member) => Ok(AssignmentTargetRef::StaticMember(member)),
        Expression::ComputedMemberExpression(member) => {
            Ok(AssignmentTargetRef::ComputedMember(member))
        }
        Expression::PrivateFieldExpression(member) => Ok(AssignmentTargetRef::PrivateField(member)),
        Expression::ParenthesizedExpression(paren) => {
            unwrap_assignment_target_expression(&paren.expression, target_span)
        }
        Expression::TSAsExpression(expr) => {
            unwrap_assignment_target_expression(&expr.expression, target_span)
        }
        Expression::TSSatisfiesExpression(expr) => {
            unwrap_assignment_target_expression(&expr.expression, target_span)
        }
        Expression::TSNonNullExpression(expr) => {
            unwrap_assignment_target_expression(&expr.expression, target_span)
        }
        Expression::TSTypeAssertion(expr) => {
            unwrap_assignment_target_expression(&expr.expression, target_span)
        }
        _ => Err(SourceLoweringError::unsupported(
            "parser_recovery_ts_lhs_wrapper",
            target_span,
        )),
    }
}

fn unwrap_simple_assignment_target_expression<'a>(
    expr: &'a Expression<'a>,
    target_span: Span,
) -> Result<SimpleAssignmentTargetRef<'a>, SourceLoweringError> {
    match unwrap_assignment_target_expression(expr, target_span)? {
        AssignmentTargetRef::Identifier(ident) => Ok(SimpleAssignmentTargetRef::Identifier(ident)),
        AssignmentTargetRef::StaticMember(member) => {
            Ok(SimpleAssignmentTargetRef::StaticMember(member))
        }
        AssignmentTargetRef::ComputedMember(member) => {
            Ok(SimpleAssignmentTargetRef::ComputedMember(member))
        }
        AssignmentTargetRef::PrivateField(member) => {
            Ok(SimpleAssignmentTargetRef::PrivateField(member))
        }
        AssignmentTargetRef::Array(_) | AssignmentTargetRef::Object(_) => Err(
            SourceLoweringError::unsupported("parser_recovery_ts_lhs_wrapper", target_span),
        ),
    }
}
