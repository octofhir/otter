//! Binary-family expression lowering.
//!
//! # Contents
//! - [`compile_binary`] — lowers ordinary binary expressions.
//! - [`compile_logical`] — lowers logical short-circuit expressions.
//! - [`compile_private_in`] — lowers private-name membership probes.
//! - Destination-aware variants reuse a caller-owned result register.
//!
//! # Invariants
//! - Left-to-right evaluation and observable coercion order are preserved.
//! - An explicit destination changes only the final result location; operand
//!   values remain independent snapshots.
//! - Logical branches converge on one reserved result register.
//!
//! # See also
//! - [`super`] — expression dispatch and shared helpers.

use crate::*;
use oxc_ast::ast::{BinaryExpression, Expression, LogicalExpression, PrivateInExpression};

/// `true` when `expr` provably evaluates to a primitive, so the
/// `ToPrimitive` step a binary-op lowering would emit ahead of it is
/// redundant. Conservative — only AST shapes whose result is *always* a
/// primitive (number / string / boolean / bigint / undefined / null) qualify.
/// A primitive's `ToPrimitive` has no observable side effect (no `valueOf` /
/// `toString` / `[Symbol.toPrimitive]` call), so eliding the op is
/// behavior-preserving; it only removes redundant coercion instructions.
pub(crate) fn expr_is_primitive(expr: &Expression<'_>) -> bool {
    // `(expr)` is transparent — recurse through the parens.
    if let Expression::ParenthesizedExpression(p) = expr {
        return expr_is_primitive(&p.expression);
    }
    matches!(
        expr,
        // Literals that are primitives (object literals/regexp excluded).
        Expression::NumericLiteral(_)
            | Expression::StringLiteral(_)
            | Expression::BooleanLiteral(_)
            | Expression::NullLiteral(_)
            | Expression::BigIntLiteral(_)
            // A template literal always coerces its parts to a String.
            | Expression::TemplateLiteral(_)
            // Every binary operator yields a primitive: arith/bitwise/shift →
            // number|bigint, `+` → string|number, compare/equality/`in`/
            // `instanceof` → boolean.
            | Expression::BinaryExpression(_)
            // typeof→string, void→undefined, `!`→boolean, `-`/`+`/`~`→number|
            // bigint, delete→boolean.
            | Expression::UnaryExpression(_)
            // `++`/`--` → number|bigint.
            | Expression::UpdateExpression(_)
    )
}

/// `true` for opcodes whose optimizing-tier lowering is selected from the
/// arithmetic feedback cell, and which therefore already emit a
/// representation guard with a deoptimization exit. Only these may consume an
/// annotation-derived seed.
const fn op_takes_number_hint(op: Op) -> bool {
    matches!(
        op,
        Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Rem
            | Op::Pow
            | Op::BitwiseAnd
            | Op::BitwiseOr
            | Op::BitwiseXor
            | Op::Shl
            | Op::Shr
            | Op::LessThan
            | Op::LessEq
            | Op::GreaterThan
            | Op::GreaterEq
            | Op::Equal
            | Op::NotEqual
    )
}

pub(crate) fn compile_logical(
    cx: &mut Compiler,
    l: &LogicalExpression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let destination = cx.alloc_scratch();
    compile_logical_into(cx, l, span, destination)
}

pub(crate) fn compile_logical_into(
    cx: &mut Compiler,
    l: &LogicalExpression<'_>,
    span: (u32, u32),
    destination: u16,
) -> Result<u16, CompileError> {
    let _ = span;
    let span = (l.span.start, l.span.end);
    // Lower `a && b`, `a || b`, `a ?? b` with short-circuit
    // semantics. Both branches store into the same caller-owned slot.
    let left = compile_expr(cx, &l.left, span)?;
    cx.emit(
        Op::StoreLocal,
        [Operand::Register(left), Operand::Imm32(destination as i32)],
        span,
    );
    // Note: locals and scratch share the same register
    // window. We use STORE_LOCAL into the freshly-allocated
    // scratch index so the JUMP target reads back through
    // LOAD_LOCAL — preserves register liveness across the
    // branch without a phi.
    let short_circuit = match l.operator {
        LogicalOperator::And => cx.emit_branch_placeholder(Op::JumpIfFalse, Some(left), span),
        LogicalOperator::Or => cx.emit_branch_placeholder(Op::JumpIfTrue, Some(left), span),
        LogicalOperator::Coalesce => {
            // `a ?? b`: if `a` is **not** nullish, short-
            // circuit. JumpIfNullish jumps when nullish, so
            // we want the **inverse**: emit a normal branch
            // into "evaluate b" path when nullish, and let
            // fall-through skip past `b`. Implement via two
            // jumps for clarity.
            let to_b = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(left), span);
            let skip = cx.emit_branch_placeholder(Op::Jump, None, span);
            cx.patch_branch_to_here(to_b);
            let right = compile_expr(cx, &l.right, span)?;
            cx.emit(
                Op::StoreLocal,
                [Operand::Register(right), Operand::Imm32(destination as i32)],
                span,
            );
            cx.patch_branch_to_here(skip);
            return Ok(destination);
        }
    };
    // Falling here for `&&` / `||`: evaluate `right` and
    // store; patch short-circuit at end.
    let right = compile_expr(cx, &l.right, span)?;
    cx.emit(
        Op::StoreLocal,
        [Operand::Register(right), Operand::Imm32(destination as i32)],
        span,
    );
    cx.patch_branch_to_here(short_circuit);
    Ok(destination)
}

pub(crate) fn compile_private_in(
    cx: &mut Compiler,
    p: &PrivateInExpression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let _ = span;
    let pspan = (p.span.start, p.span.end);
    let key_reg = crate::class::load_private_key(cx, p.left.name.as_str(), pspan)?;
    let obj_reg = compile_expr(cx, &p.right, pspan)?;
    let dst = cx.alloc_scratch();
    cx.emit(
        Op::HasProperty,
        vec![
            Operand::Register(dst),
            Operand::Register(key_reg),
            Operand::Register(obj_reg),
        ],
        pspan,
    );
    Ok(dst)
}

pub(crate) fn compile_binary(
    cx: &mut Compiler,
    b: &BinaryExpression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    compile_binary_to(cx, b, span, None)
}

pub(crate) fn compile_binary_into(
    cx: &mut Compiler,
    b: &BinaryExpression<'_>,
    span: (u32, u32),
    destination: u16,
) -> Result<u16, CompileError> {
    compile_binary_to(cx, b, span, Some(destination))
}

fn compile_binary_to(
    cx: &mut Compiler,
    b: &BinaryExpression<'_>,
    span: (u32, u32),
    destination: Option<u16>,
) -> Result<u16, CompileError> {
    let _ = span;
    let span = (b.span.start, b.span.end);
    // Read the annotation-derived hint before lowering the operands: the
    // bindings it consults are the ones in scope at the source position.
    let number_typed_operands = expr_number_typed(cx, &b.left) && expr_number_typed(cx, &b.right);
    // Operand temps (and their coercion temps) are dead once the
    // result opcode below has read them, so recycle the whole range
    // into the destination register. See `FunctionContext::reset_scratch`.
    let mark = cx.scratch;
    let lhs = compile_expr(cx, &b.left, span)?;
    let rhs = compile_expr(cx, &b.right, span)?;
    let op = match b.operator {
        BinaryOperator::Addition => Op::Add,
        BinaryOperator::Subtraction => Op::Sub,
        BinaryOperator::Multiplication => Op::Mul,
        BinaryOperator::Division => Op::Div,
        BinaryOperator::Remainder => Op::Rem,
        BinaryOperator::Exponential => Op::Pow,
        BinaryOperator::BitwiseAnd => Op::BitwiseAnd,
        BinaryOperator::BitwiseOR => Op::BitwiseOr,
        BinaryOperator::BitwiseXOR => Op::BitwiseXor,
        BinaryOperator::ShiftLeft => Op::Shl,
        BinaryOperator::ShiftRight => Op::Shr,
        BinaryOperator::ShiftRightZeroFill => Op::Ushr,
        BinaryOperator::StrictEquality => Op::Equal,
        BinaryOperator::StrictInequality => Op::NotEqual,
        BinaryOperator::Equality => Op::LooseEqual,
        BinaryOperator::Inequality => Op::LooseNotEqual,
        BinaryOperator::LessThan => Op::LessThan,
        BinaryOperator::LessEqualThan => Op::LessEq,
        BinaryOperator::GreaterThan => Op::GreaterThan,
        BinaryOperator::GreaterEqualThan => Op::GreaterEq,
        BinaryOperator::Instanceof => Op::Instanceof,
        // §13.10.1 `RelationalExpression in ShiftExpression`.
        // <https://tc39.es/ecma262/#sec-relational-operators-runtime-semantics-evaluation>
        BinaryOperator::In => Op::HasProperty,
    };
    // Coercion is the operator's own job. `Op::Add`, the relational
    // comparisons, and the non-additive numeric / bitwise / shift opcodes each
    // run their operands through the ToPrimitive / ToNumeric ladder in the
    // order §13.15.3, §7.2.13, and §7.2.14 prescribe, so the lowering emits the
    // operator alone. Emitting the conversions as separate opcodes cost four
    // extra dispatched instructions per non-additive arithmetic operation and
    // two per addition or comparison, all of which reduce to identity the
    // moment the operand is already a Number.
    let (lhs_in, rhs_in) = (lhs, rhs);
    cx.reset_scratch(mark);
    // Recycling the operand temporaries would otherwise hand the result the
    // register an operand still occupies. The opcode reads both operands before
    // it writes, so an aliased destination is semantically fine, but the
    // optimizing tier side-exits on it, which pins the whole function to the
    // template tier. Take the first recycled register the operands do not hold.
    let dst = destination.unwrap_or_else(|| {
        let mut candidate = cx.alloc_scratch();
        while candidate == lhs_in || candidate == rhs_in {
            candidate = cx.alloc_scratch();
        }
        candidate
    });
    // Seeds the optimizing tier's representation choice for a site that has
    // not warmed up yet. Restricted to opcodes that carry a numeric guard and
    // a deopt exit, so a wrong annotation costs one deoptimization.
    if number_typed_operands && op_takes_number_hint(op) {
        cx.mark_number_hint_site();
    }
    cx.emit(
        op,
        vec![
            Operand::Register(dst),
            Operand::Register(lhs_in),
            Operand::Register(rhs_in),
        ],
        span,
    );
    Ok(dst)
}
