//! Static TypeScript type hints carried from annotations into bytecode.
//!
//! # Contents
//! - [`TypeHint`] — the hint lattice attached to a lexical binding.
//! - [`annotation_hint`] — read a hint out of a `TSTypeAnnotation`.
//! - [`expr_number_typed`] — decide whether an expression is statically
//!   `number` under the current binding table.
//! - [`mark_class_receiver`] — mark a property site whose receiver is a
//!   class-annotated binding.
//! - [`resolve_class_hint_sites`] — turn interned annotation names into the
//!   declaring class's function id once the whole module is compiled.
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
//! - A [`TypeHint::Class`] survives to bytecode only when its name resolves to
//!   exactly one `class` declaration in the module. Interfaces, type aliases,
//!   and repeated class names have no single runtime identity to seed with.
//!
//! # See also
//! - `otter_bytecode::Function::number_hint_sites` — the emitted numeric sites.
//! - `otter_bytecode::Function::class_hint_sites` — the emitted class sites.

use crate::*;
use oxc_ast::ast::{TSType, TSTypeAnnotation, TSTypeName};

/// Static type known for a binding from its TypeScript annotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum TypeHint {
    /// No annotation, or one this compiler does not model.
    #[default]
    Unknown,
    /// Annotated `number` — an IEEE-754 double, never a BigInt.
    Number,
    /// Annotated with a bare type reference, held as an interned annotation
    /// name. The declaring class is resolved after the module is compiled: a
    /// class may be declared textually after the function that takes it.
    Class(u32),
}

/// Hint implied by a type annotation, if any.
pub(crate) fn annotation_hint(
    cx: &mut Compiler,
    annotation: Option<&TSTypeAnnotation<'_>>,
) -> TypeHint {
    match annotation.map(|annotation| &annotation.type_annotation) {
        Some(TSType::TSNumberKeyword(_)) => TypeHint::Number,
        // A generic reference (`Box<T>`) says nothing about the instance shape
        // of the value, so only bare `C` is carried.
        Some(TSType::TSTypeReference(reference)) if reference.type_arguments.is_none() => {
            match &reference.type_name {
                TSTypeName::IdentifierReference(id) => {
                    TypeHint::Class(cx.intern_class_hint_name(id.name.as_str()))
                }
                _ => TypeHint::Unknown,
            }
        }
        _ => TypeHint::Unknown,
    }
}

/// Mark the next emitted property instruction when `object` is an identifier
/// whose binding carries a class annotation. Call immediately before the
/// `LoadProperty` / `StoreProperty` emit it describes.
pub(crate) fn mark_class_receiver(cx: &mut Compiler, object: &Expression<'_>) {
    if let Expression::Identifier(id) = object
        && let Some(TypeHint::Class(name)) = cx
            .lookup_binding(id.name.as_str())
            .map(|info| info.type_hint)
    {
        cx.mark_class_hint_site(name);
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
            let hint = annotation_hint(cx, param.type_annotation.as_deref());
            cx.annotate_binding(id.name.as_str(), hint);
        }
    }
}

/// Resolve every recorded class-annotated property site against the module's
/// class declarations and publish the survivors onto their functions.
///
/// A name that never named a `class`, or that named more than one, is dropped:
/// there is no single constructor whose instance shape the site could be
/// seeded with.
pub(crate) fn resolve_class_hint_sites(cx: &Compiler, functions: &mut [otter_bytecode::Function]) {
    for site in &cx.pending_class_hint_sites {
        let Some(name) = cx.class_hint_names.get(site.name as usize) else {
            continue;
        };
        let Some(&Some(class_function_id)) = cx.declared_classes.get(name.as_str()) else {
            continue;
        };
        let Some(function) = functions.get_mut(site.function_id as usize) else {
            continue;
        };
        function
            .class_hint_sites
            .push(otter_bytecode::ClassHintSite {
                pc: site.pc,
                class_function_id,
            });
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

    /// `(pc, class function id)` pairs recorded in the named function.
    fn class_sites(source: &str, function_name: &str) -> Vec<(u32, u32)> {
        let module = compile_script_source(source, SyntaxSourceKind::TypeScript, "file:///t.ts")
            .expect("test source compiles");
        module
            .functions
            .iter()
            .find(|function| function.name == function_name)
            .unwrap_or_else(|| panic!("function `{function_name}` is compiled"))
            .class_hint_sites
            .iter()
            .map(|site| (site.pc, site.class_function_id))
            .collect()
    }

    /// Function id of the named class's constructor.
    fn class_ctor_id(source: &str, class_name: &str) -> u32 {
        let module = compile_script_source(source, SyntaxSourceKind::TypeScript, "file:///t.ts")
            .expect("test source compiles");
        module
            .functions
            .iter()
            .find(|function| function.name == class_name)
            .expect("class constructor is compiled")
            .id
    }

    #[test]
    fn class_annotated_receivers_mark_their_property_sites() {
        let source = "class C { constructor(v) { this.v = v; } }\n\
                      function f(c: C) { return c.v; }";
        let sites = class_sites(source, "f");
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].1, class_ctor_id(source, "C"));
        // A store through the same annotation is marked too.
        let store = "class C { constructor(v) { this.v = v; } }\n\
                     function f(c: C) { c.v = 1; }";
        assert_eq!(class_sites(store, "f").len(), 1);
    }

    #[test]
    fn class_hints_need_one_unambiguous_local_class() {
        // A name that never declared a class has no runtime identity.
        assert!(
            class_sites(
                "interface I { v: number }\nfunction f(i: I) { return i.v; }",
                "f"
            )
            .is_empty()
        );
        // Two classes of the same name: no single instance shape to seed with.
        assert!(
            class_sites(
                "function f(c: C) { return c.v; }\n\
                 { class C { constructor(v) { this.v = v; } } }\n\
                 { class C { constructor(w) { this.w = w; } } }",
                "f"
            )
            .is_empty()
        );
        // A generic reference says nothing about the instance shape.
        assert!(
            class_sites(
                "class C<T> { constructor(v) { this.v = v; } }\n\
                 function f(c: C<number>) { return c.v; }",
                "f"
            )
            .is_empty()
        );
    }

    #[test]
    fn class_declared_after_its_use_still_resolves() {
        let source = "function f(c: C) { return c.v; }\n\
                      class C { constructor(v) { this.v = v; } }";
        assert_eq!(class_sites(source, "f").len(), 1);
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
