//! AST-to-bytecode lowering for the Ignition-style ISA.
//!
//! [`ModuleCompiler`] is the single entry point the rest of the VM uses
//! to turn a JavaScript/TypeScript source string into a
//! [`crate::module::Module`]. It owns the oxc `Allocator` for the
//! current compilation and drives the staged lowering: parse → AST
//! shape check → bytecode emit → `Module`.
//!
//! # Current state (M1)
//!
//! The compiler accepts a **single** top-level `FunctionDeclaration`
//! and lowers a narrow slice of its body. Supported surface:
//!
//! - Program with exactly one statement, and that statement is a
//!   `FunctionDeclaration`.
//! - Function: named (Identifier), not async, not a generator, 0 or 1
//!   parameters. The parameter must be a plain identifier — no
//!   destructuring, no default, no rest, no type annotation.
//! - Body: a `BlockStatement` with exactly one `ReturnStatement`.
//! - Return expression: one of
//!   - `Identifier` (must reference the declared parameter);
//!   - int32-safe `NumericLiteral` (integral, in `i32` range);
//!   - `BinaryExpression` with operator `+`, where each operand is
//!     itself int32-safe (identifier or int32-safe literal).
//!
//! Anything outside that shape surfaces as a
//! [`SourceLoweringError::Unsupported`] with a `construct: &'static
//! str` tag pointing at the offending node. Unsupported is the
//! **expected** result for every milestone gap during the staged
//! rollout (see `V2_MIGRATION.md`), not a bug.
//!
//! The bytecode shape is fixed:
//!
//! ```text
//!   <return-expr lowering>   // leaves the value in the accumulator
//!   Return                    // acc is the callee's return value
//! ```
//!
//! For `function f(n) { return n + 1 }` this is:
//!
//! ```text
//!   Ldar r0      ; acc = n
//!   AddSmi 1     ; acc = n + 1
//!   Return
//! ```

mod error;

#[cfg(test)]
mod tests;

pub use error::SourceLoweringError;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    BinaryExpression, BinaryOperator, BindingPattern, Expression, FormalParameter,
    FormalParameters, Function, FunctionBody, NumericLiteral, Program, Statement,
};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType, Span};

use crate::bytecode::{Bytecode, BytecodeBuilder, Opcode, Operand};
use crate::frame::{FrameLayout, RegisterIndex};
use crate::module::{Function as VmFunction, FunctionIndex, Module};

/// Staged AST-to-bytecode compiler for a single source file.
///
/// Construct one `ModuleCompiler` per source file. The compiler walks
/// the parsed AST and, when a construct is recognised, emits the
/// corresponding Ignition bytecode; unrecognised constructs produce a
/// [`SourceLoweringError::Unsupported`].
#[derive(Debug, Default)]
pub struct ModuleCompiler;

impl ModuleCompiler {
    /// Creates a new, empty compiler.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Parse and lower `source` into a [`Module`].
    ///
    /// `source_url` is used for diagnostics only — it is not fetched or
    /// resolved. `source_type` controls whether the parser treats the
    /// input as a script, module, or `.ts`/`.tsx` file; the value is
    /// forwarded verbatim to `oxc_parser`.
    ///
    /// # Errors
    ///
    /// - [`SourceLoweringError::Parse`] on parse-phase syntax errors.
    /// - [`SourceLoweringError::Unsupported`] when the AST falls outside
    ///   the currently supported M1 slice.
    pub fn compile(
        &self,
        source: &str,
        source_url: &str,
        source_type: SourceType,
    ) -> Result<Module, SourceLoweringError> {
        let _ = source_url;
        let allocator = Allocator::default();
        let parser_return = Parser::new(&allocator, source, source_type).parse();

        if !parser_return.errors.is_empty() {
            let diag = &parser_return.errors[0];
            let label_span = diag
                .labels
                .as_ref()
                .and_then(|labels| labels.first())
                .map(|label| {
                    let start = u32::try_from(label.offset()).unwrap_or(0);
                    let length = u32::try_from(label.len()).unwrap_or(0);
                    Span::new(start, start.saturating_add(length))
                })
                .unwrap_or_else(|| Span::new(0, 0));
            return Err(SourceLoweringError::Parse {
                message: diag.message.to_string(),
                span: label_span,
            });
        }

        lower_program(&parser_return.program)
    }
}

// ---------------------------------------------------------------------------
// Lowering
// ---------------------------------------------------------------------------

fn lower_program(program: &Program<'_>) -> Result<Module, SourceLoweringError> {
    // M1 accepts exactly one top-level statement, and it must be a
    // `FunctionDeclaration`. Everything else — empty bodies, multiple
    // statements, `class`/`var`/`import`/bare expressions — surfaces as
    // an `Unsupported` pointing at the offending (or missing) node so
    // later milestones can widen coverage one construct at a time.
    let only = match program.body.as_slice() {
        [single] => single,
        [] => return Err(SourceLoweringError::unsupported("program", program.span)),
        [_first, second, ..] => {
            return Err(SourceLoweringError::unsupported(
                "multiple_top_level_statements",
                second.span(),
            ));
        }
    };

    let function = match only {
        Statement::FunctionDeclaration(func) => func.as_ref(),
        Statement::ClassDeclaration(class) => {
            return Err(SourceLoweringError::unsupported(
                "class_declaration",
                class.span,
            ));
        }
        other => {
            return Err(SourceLoweringError::unsupported(
                statement_construct_tag(other),
                other.span(),
            ));
        }
    };

    let lowered = lower_function_declaration(function)?;
    let module = Module::new(None::<&str>, vec![lowered], FunctionIndex(0)).map_err(|err| {
        SourceLoweringError::Internal(format!("module construction failed: {err}"))
    })?;
    Ok(module)
}

/// Maps the residual `Statement` variants we explicitly don't handle at
/// M1 back to a stable `construct` tag. Later milestones can move a row
/// from this catch-all into a real lowering arm without touching call
/// sites in tests.
fn statement_construct_tag(stmt: &Statement<'_>) -> &'static str {
    match stmt {
        Statement::VariableDeclaration(_) => "variable_declaration",
        Statement::ExpressionStatement(_) => "expression_statement",
        Statement::IfStatement(_) => "if_statement",
        Statement::WhileStatement(_) => "while_statement",
        Statement::ForStatement(_) => "for_statement",
        Statement::BlockStatement(_) => "block_statement",
        Statement::ReturnStatement(_) => "return_statement",
        Statement::ImportDeclaration(_) | Statement::ExportAllDeclaration(_) => {
            "module_declaration"
        }
        Statement::ExportDefaultDeclaration(_) | Statement::ExportNamedDeclaration(_) => {
            "export_declaration"
        }
        _ => "statement",
    }
}

fn lower_function_declaration(func: &Function<'_>) -> Result<VmFunction, SourceLoweringError> {
    if func.r#async {
        return Err(SourceLoweringError::unsupported(
            "async_function",
            func.span,
        ));
    }
    if func.generator {
        return Err(SourceLoweringError::unsupported("generator", func.span));
    }

    let name = func
        .id
        .as_ref()
        .map(|ident| ident.name.as_str().to_owned())
        .ok_or_else(|| SourceLoweringError::unsupported("anonymous_function", func.span))?;

    let param_count = count_simple_params(&func.params)?;

    let body = func
        .body
        .as_ref()
        .ok_or_else(|| SourceLoweringError::unsupported("declared_only_function", func.span))?;

    // FrameLayout: 1 hidden slot for `this`, plus `param_count` parameter
    // slots, zero locals / temporaries at M1. The v2 interpreter maps
    // `Ldar r0` through `FrameLayout::resolve_user_visible(0)`, which
    // points at the first parameter (absolute index 1), so parameter
    // access stays symmetric with v1's register semantics.
    let layout = FrameLayout::new(1, param_count, 0, 0)
        .map_err(|err| SourceLoweringError::Internal(format!("frame layout invalid: {err:?}")))?;

    let bytecode = lower_function_body(body, &func.params)?;

    Ok(VmFunction::with_empty_tables(Some(name), layout, bytecode).with_strict(func.id.is_some()))
}

fn count_simple_params(
    params: &FormalParameters<'_>,
) -> Result<RegisterIndex, SourceLoweringError> {
    if params.rest.is_some() {
        return Err(SourceLoweringError::unsupported(
            "rest_parameter",
            params.span,
        ));
    }
    match params.items.as_slice() {
        [] => Ok(0),
        [single] => {
            validate_simple_param(single)?;
            Ok(1)
        }
        [_, second, ..] => Err(SourceLoweringError::unsupported(
            "multiple_parameters",
            second.span,
        )),
    }
}

fn validate_simple_param(param: &FormalParameter<'_>) -> Result<(), SourceLoweringError> {
    if param.initializer.is_some() {
        return Err(SourceLoweringError::unsupported(
            "default_parameter",
            param.span,
        ));
    }
    if !matches!(param.pattern, BindingPattern::BindingIdentifier(_)) {
        return Err(SourceLoweringError::unsupported(
            "destructuring_parameter",
            param.span,
        ));
    }
    Ok(())
}

fn lower_function_body(
    body: &FunctionBody<'_>,
    params: &FormalParameters<'_>,
) -> Result<Bytecode, SourceLoweringError> {
    if !body.directives.is_empty() {
        return Err(SourceLoweringError::unsupported(
            "directive_prologue",
            body.directives[0].span,
        ));
    }
    let only = match body.statements.as_slice() {
        [single] => single,
        [] => return Err(SourceLoweringError::unsupported("empty_body", body.span)),
        [_first, second, ..] => {
            return Err(SourceLoweringError::unsupported(
                "multi_statement_body",
                second.span(),
            ));
        }
    };
    let ret = match only {
        Statement::ReturnStatement(ret) => ret,
        other => {
            return Err(SourceLoweringError::unsupported(
                statement_construct_tag(other),
                other.span(),
            ));
        }
    };
    let argument = ret
        .argument
        .as_ref()
        .ok_or_else(|| SourceLoweringError::unsupported("return_without_value", ret.span))?;

    let mut builder = BytecodeBuilder::new();
    let ctx = LoweringContext::new(params);
    lower_return_expression(&mut builder, &ctx, argument)?;
    builder
        .emit(Opcode::Return, &[])
        .map_err(|err| SourceLoweringError::Internal(format!("encode Return: {err:?}")))?;
    builder
        .finish()
        .map_err(|err| SourceLoweringError::Internal(format!("finalise bytecode: {err:?}")))
}

/// Per-function lowering context: enough to resolve a parameter
/// identifier into a user-visible register index.
struct LoweringContext<'a> {
    param_name: Option<&'a str>,
}

impl<'a> LoweringContext<'a> {
    fn new(params: &'a FormalParameters<'a>) -> Self {
        let param_name = match params.items.as_slice() {
            [single] => match &single.pattern {
                BindingPattern::BindingIdentifier(ident) => Some(ident.name.as_str()),
                _ => None,
            },
            _ => None,
        };
        Self { param_name }
    }

    /// Resolves a JS identifier reference into a bytecode-visible
    /// register. At M1 only the sole parameter is accessible; globals,
    /// closures, and locals land in later milestones.
    fn resolve_identifier(&self, name: &str) -> Option<u16> {
        match self.param_name {
            Some(param) if param == name => Some(0),
            _ => None,
        }
    }
}

fn lower_return_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &Expression<'_>,
) -> Result<(), SourceLoweringError> {
    match expr {
        Expression::Identifier(ident) => {
            let reg = ctx.resolve_identifier(ident.name.as_str()).ok_or_else(|| {
                SourceLoweringError::unsupported("unbound_identifier", ident.span)
            })?;
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(reg))])
                .map_err(|err| SourceLoweringError::Internal(format!("encode Ldar: {err:?}")))?;
            Ok(())
        }
        Expression::NumericLiteral(literal) => {
            let value = int32_from_literal(literal)?;
            builder
                .emit(Opcode::LdaSmi, &[Operand::Imm(value)])
                .map_err(|err| SourceLoweringError::Internal(format!("encode LdaSmi: {err:?}")))?;
            Ok(())
        }
        Expression::BinaryExpression(binary) => lower_binary_expression(builder, ctx, binary),
        Expression::ParenthesizedExpression(inner) => {
            lower_return_expression(builder, ctx, &inner.expression)
        }
        other => Err(SourceLoweringError::unsupported(
            expression_construct_tag(other),
            other.span(),
        )),
    }
}

/// Lowers `lhs + rhs` where both operands must be int32-safe. Picks
/// `AddSmi imm` whenever the RHS is a literal that fits in `i8`; falls
/// back to `Add reg` otherwise (with the RHS evaluated into a
/// temporary slot only if it isn't already an identifier).
fn lower_binary_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &BinaryExpression<'_>,
) -> Result<(), SourceLoweringError> {
    if expr.operator != BinaryOperator::Addition {
        return Err(SourceLoweringError::unsupported(
            binary_operator_tag(expr.operator),
            expr.span,
        ));
    }

    // LHS must evaluate into the accumulator. Only identifier /
    // int32-safe literal / parenthesised variants of those are allowed
    // at M1 — nested binary expressions require a scratch slot we don't
    // allocate yet.
    lower_accumulator_operand(builder, ctx, &expr.left)?;

    // RHS: prefer the `AddSmi` fast path if the right operand is an
    // int32-safe literal that fits into the signed-8-bit immediate the
    // narrow operand width accepts.
    match &expr.right {
        Expression::NumericLiteral(literal) => {
            let value = int32_from_literal(literal)?;
            if (i32::from(i8::MIN)..=i32::from(i8::MAX)).contains(&value) {
                builder
                    .emit(Opcode::AddSmi, &[Operand::Imm(value)])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!("encode AddSmi: {err:?}"))
                    })?;
            } else {
                // Wider immediate — no scratch slot in M1's frame layout
                // to materialise it into, so reject cleanly until M4
                // introduces locals.
                return Err(SourceLoweringError::unsupported(
                    "wide_integer_literal_on_rhs",
                    literal.span,
                ));
            }
        }
        Expression::Identifier(ident) => {
            let reg = ctx.resolve_identifier(ident.name.as_str()).ok_or_else(|| {
                SourceLoweringError::unsupported("unbound_identifier", ident.span)
            })?;
            builder
                .emit(Opcode::Add, &[Operand::Reg(u32::from(reg))])
                .map_err(|err| SourceLoweringError::Internal(format!("encode Add: {err:?}")))?;
        }
        Expression::ParenthesizedExpression(inner) => {
            return Err(SourceLoweringError::unsupported(
                "parenthesised_rhs",
                inner.span,
            ));
        }
        other => {
            return Err(SourceLoweringError::unsupported(
                expression_construct_tag(other),
                other.span(),
            ));
        }
    }
    Ok(())
}

fn lower_accumulator_operand(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &Expression<'_>,
) -> Result<(), SourceLoweringError> {
    match expr {
        Expression::Identifier(ident) => {
            let reg = ctx.resolve_identifier(ident.name.as_str()).ok_or_else(|| {
                SourceLoweringError::unsupported("unbound_identifier", ident.span)
            })?;
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(reg))])
                .map_err(|err| SourceLoweringError::Internal(format!("encode Ldar: {err:?}")))?;
            Ok(())
        }
        Expression::NumericLiteral(literal) => {
            let value = int32_from_literal(literal)?;
            builder
                .emit(Opcode::LdaSmi, &[Operand::Imm(value)])
                .map_err(|err| SourceLoweringError::Internal(format!("encode LdaSmi: {err:?}")))?;
            Ok(())
        }
        Expression::ParenthesizedExpression(inner) => {
            lower_accumulator_operand(builder, ctx, &inner.expression)
        }
        other => Err(SourceLoweringError::unsupported(
            expression_construct_tag(other),
            other.span(),
        )),
    }
}

/// Convert a parsed `NumericLiteral` into an int32. Rejects fractional
/// parts and values outside `i32` range — those surface as
/// `Unsupported { construct: "non_int32_literal" }` because the
/// widening path (`LoadF64` / `LoadBigInt`) lands in a later milestone.
fn int32_from_literal(literal: &NumericLiteral<'_>) -> Result<i32, SourceLoweringError> {
    let value = literal.value;
    if !value.is_finite() || value.fract() != 0.0 {
        return Err(SourceLoweringError::unsupported(
            "non_int32_literal",
            literal.span,
        ));
    }
    if !(f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&value) {
        return Err(SourceLoweringError::unsupported(
            "non_int32_literal",
            literal.span,
        ));
    }
    // Safe because `value` is finite, integral, and within i32 range.
    Ok(value as i32)
}

fn expression_construct_tag(expr: &Expression<'_>) -> &'static str {
    match expr {
        Expression::BooleanLiteral(_) => "boolean_literal",
        Expression::NullLiteral(_) => "null_literal",
        Expression::StringLiteral(_) => "string_literal",
        Expression::BigIntLiteral(_) => "bigint_literal",
        Expression::TemplateLiteral(_) => "template_literal",
        Expression::CallExpression(_) => "call_expression",
        Expression::NewExpression(_) => "new_expression",
        Expression::StaticMemberExpression(_) => "member_expression",
        Expression::ComputedMemberExpression(_) => "member_expression",
        Expression::PrivateFieldExpression(_) => "private_field_expression",
        Expression::ArrayExpression(_) => "array_expression",
        Expression::ObjectExpression(_) => "object_expression",
        Expression::FunctionExpression(_) => "function_expression",
        Expression::ArrowFunctionExpression(_) => "arrow_function_expression",
        Expression::ClassExpression(_) => "class_expression",
        Expression::UnaryExpression(_) => "unary_expression",
        Expression::UpdateExpression(_) => "update_expression",
        Expression::LogicalExpression(_) => "logical_expression",
        Expression::ConditionalExpression(_) => "conditional_expression",
        Expression::AssignmentExpression(_) => "assignment_expression",
        Expression::ThisExpression(_) => "this_expression",
        Expression::Super(_) => "super_expression",
        _ => "expression",
    }
}

fn binary_operator_tag(op: BinaryOperator) -> &'static str {
    match op {
        BinaryOperator::Addition => "addition",
        BinaryOperator::Subtraction => "subtraction",
        BinaryOperator::Multiplication => "multiplication",
        BinaryOperator::Division => "division",
        BinaryOperator::Remainder => "remainder",
        BinaryOperator::Exponential => "exponent",
        BinaryOperator::ShiftLeft => "shift_left",
        BinaryOperator::ShiftRight => "shift_right",
        BinaryOperator::ShiftRightZeroFill => "unsigned_shift_right",
        BinaryOperator::BitwiseOR => "bitwise_or",
        BinaryOperator::BitwiseXOR => "bitwise_xor",
        BinaryOperator::BitwiseAnd => "bitwise_and",
        BinaryOperator::Equality
        | BinaryOperator::Inequality
        | BinaryOperator::StrictEquality
        | BinaryOperator::StrictInequality
        | BinaryOperator::LessThan
        | BinaryOperator::LessEqualThan
        | BinaryOperator::GreaterThan
        | BinaryOperator::GreaterEqualThan => "comparison",
        BinaryOperator::In | BinaryOperator::Instanceof => "membership",
    }
}
