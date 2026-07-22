//! Static TypeScript type hints carried from annotations into bytecode.
//!
//! # Contents
//! - [`TypeHint`] — the hint lattice attached to a lexical binding.
//! - [`annotation_hint`] — read a hint out of a `TSTypeAnnotation`.
//! - [`expr_number_typed`] — decide whether an expression is statically
//!   `number` under the current binding table.
//!
//! # Invariants
//! - Hints are **advisory and unsound**: TypeScript annotations are not
//!   checked at runtime, and unannotated code carries no hint at all. Nothing
//!   here may be used as a proof — only as a seed for a site that already
//!   carries a representation guard and a deoptimization exit.
//! - The lattice is deliberately narrow. Only the exact `number` keyword
//!   produces [`TypeHint::Number`]; `any`, unions, aliases, and `bigint` all
//!   stay [`TypeHint::Unknown`], because a wrong hint is paid for with a
//!   deoptimization and there is no gain in guessing.
//!
//! # See also
//! - `otter_bytecode::Function::number_hint_sites` — the emitted site list.

use crate::*;
use oxc_ast::ast::{TSType, TSTypeAnnotation};

/// Static type known for a binding from its TypeScript annotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum TypeHint {
    /// No annotation, or one this compiler does not model.
    #[default]
    Unknown,
    /// Annotated `number` — an IEEE-754 double, never a BigInt.
    Number,
}

/// Hint implied by a type annotation, if any.
pub(crate) fn annotation_hint(annotation: Option<&TSTypeAnnotation<'_>>) -> TypeHint {
    match annotation.map(|annotation| &annotation.type_annotation) {
        Some(TSType::TSNumberKeyword(_)) => TypeHint::Number,
        _ => TypeHint::Unknown,
    }
}

/// Attach annotation hints to the simple formals of a parameter list.
///
/// Destructuring patterns are skipped: their annotation describes the object
/// being destructured, not the names bound out of it.
pub(crate) fn annotate_formal_parameters(
    cx: &mut Compiler,
    params: &oxc_ast::ast::FormalParameters<'_>,
) {
    for param in &params.items {
        if let oxc_ast::ast::BindingPattern::BindingIdentifier(id) = &param.pattern {
            let hint = annotation_hint(param.type_annotation.as_deref());
            cx.annotate_binding(id.name.as_str(), hint);
        }
    }
}

/// `true` when `expr` is statically a Number under the annotations in scope.
///
/// Conservative in both directions: an unannotated identifier is unknown, and
/// so is anything whose result could be a BigInt or a String. `number`
/// annotations on the leaves propagate through arithmetic because every
/// modelled operator is closed over Number when both operands are Numbers.
pub(crate) fn expr_number_typed(cx: &Compiler, expr: &Expression<'_>) -> bool {
    match expr {
        Expression::ParenthesizedExpression(inner) => expr_number_typed(cx, &inner.expression),
        // `x as number` is an assertion, not a check — same unsound-hint
        // status as an annotation, and guarded the same way.
        Expression::TSAsExpression(inner) => {
            matches!(&inner.type_annotation, TSType::TSNumberKeyword(_))
                || expr_number_typed(cx, &inner.expression)
        }
        Expression::TSSatisfiesExpression(inner) => expr_number_typed(cx, &inner.expression),
        Expression::TSNonNullExpression(inner) => expr_number_typed(cx, &inner.expression),
        Expression::NumericLiteral(_) => true,
        Expression::Identifier(id) => cx
            .lookup_binding(id.name.as_str())
            .is_some_and(|binding| binding.type_hint == TypeHint::Number),
        // `++x` / `x--` yield a Number whenever the operand is one; over a
        // BigInt they yield a BigInt, hence the operand check.
        Expression::UpdateExpression(update) => match &update.argument {
            oxc_ast::ast::SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => cx
                .lookup_binding(id.name.as_str())
                .is_some_and(|binding| binding.type_hint == TypeHint::Number),
            _ => false,
        },
        Expression::UnaryExpression(unary) => match unary.operator {
            // `+x` is ToNumber, which throws on a BigInt rather than
            // producing one, so its result is always a Number.
            UnaryOperator::UnaryPlus => true,
            UnaryOperator::UnaryNegation | UnaryOperator::BitwiseNot => {
                expr_number_typed(cx, &unary.argument)
            }
            _ => false,
        },
        // Memoized: an operand tree is re-inspected by every enclosing
        // operator, which would be quadratic over a long chain.
        Expression::BinaryExpression(binary) => {
            let key = std::ptr::from_ref(binary.as_ref()) as usize;
            if let Some(&cached) = cx.number_typed_cache.borrow().get(&key) {
                return cached;
            }
            let typed = binary_is_number_closed(binary.operator)
                && expr_number_typed(cx, &binary.left)
                && expr_number_typed(cx, &binary.right);
            cx.number_typed_cache.borrow_mut().insert(key, typed);
            typed
        }
        _ => false,
    }
}

/// `true` for operators whose result is a Number whenever both operands are.
///
/// `>>>` is included because ToUint32 rejects BigInt outright; the comparison
/// and equality operators are excluded because they yield Booleans.
const fn binary_is_number_closed(operator: BinaryOperator) -> bool {
    matches!(
        operator,
        BinaryOperator::Addition
            | BinaryOperator::Subtraction
            | BinaryOperator::Multiplication
            | BinaryOperator::Division
            | BinaryOperator::Remainder
            | BinaryOperator::Exponential
            | BinaryOperator::BitwiseAnd
            | BinaryOperator::BitwiseOR
            | BinaryOperator::BitwiseXOR
            | BinaryOperator::ShiftLeft
            | BinaryOperator::ShiftRight
            | BinaryOperator::ShiftRightZeroFill
    )
}

#[cfg(test)]
mod tests {
    use crate::{SyntaxSourceKind, compile_script_source};

    /// Instruction PCs marked as statically `number` in the named function.
    fn hint_sites(source: &str, function_name: &str) -> Vec<u32> {
        let module = compile_script_source(source, SyntaxSourceKind::TypeScript, "file:///t.ts")
            .expect("test source compiles");
        module
            .functions
            .iter()
            .find(|function| function.name == function_name)
            .unwrap_or_else(|| panic!("function `{function_name}` is compiled"))
            .number_hint_sites
            .clone()
    }

    #[test]
    fn annotated_parameters_mark_their_arithmetic_sites() {
        assert_eq!(
            hint_sites("function f(a: number, b: number) { return a * b; }", "f").len(),
            1
        );
        // Chained arithmetic marks every operator in the tree.
        assert_eq!(
            hint_sites(
                "function f(a: number, b: number) { return a * b - a / b; }",
                "f"
            )
            .len(),
            3
        );
        // Comparisons consume the same feedback cell as arithmetic.
        assert_eq!(
            hint_sites("function f(a: number, b: number) { return a < b; }", "f").len(),
            1
        );
    }

    #[test]
    fn annotated_locals_and_literals_mark_their_sites() {
        assert_eq!(
            hint_sites("function f() { let x: number = 1; return x + 2; }", "f").len(),
            1
        );
        assert_eq!(
            hint_sites("function f(a: number) { return -a * 2; }", "f").len(),
            1
        );
    }

    #[test]
    fn unannotated_and_non_number_operands_stay_unmarked() {
        assert!(hint_sites("function f(a, b) { return a * b; }", "f").is_empty());
        assert!(hint_sites("function f(a: string, b: number) { return a + b; }", "f").is_empty());
        assert!(hint_sites("function f(a: bigint, b: bigint) { return a * b; }", "f").is_empty());
        // `any` is not a number claim.
        assert!(hint_sites("function f(a: any, b: any) { return a * b; }", "f").is_empty());
        // A `number` operand mixed with an unknown one proves nothing.
        assert!(hint_sites("function f(a: number, b) { return a * b; }", "f").is_empty());
        // Shadowing drops the hint: the inner `x` carries no annotation.
        assert!(
            hint_sites(
                "function f(x: number) { { let x = g(); return x * 2; } }",
                "f"
            )
            .is_empty()
        );
    }
}
