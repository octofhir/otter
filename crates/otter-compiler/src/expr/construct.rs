//! Constructor expression lowering.
//!
//! # Contents
//! - [`compile_new`] — evaluates the constructor reference and its arguments.
//! - Dense construction enforces the shared encoded argument bound.
//!
//! # Invariants
//! - Constructor references are resolved before arguments; callable identity
//!   and intrinsic specialization belong to runtime feedback, never spelling.
//!
//! # See also
//! - [`super`] — expression dispatch and shared helpers.

use crate::*;
use oxc_ast::ast::NewExpression;

pub(crate) fn compile_new(
    cx: &mut Compiler,
    new_expr: &NewExpression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let _ = span;
    let new_span = (new_expr.span.start, new_expr.span.end);
    // Callee + argument temps die once the construct opcode reads them
    // into the callee frame (`dst` is written on return), so recycle the
    // whole range into `dst`. See `FunctionContext::reset_scratch`.
    let mark = cx.scratch;
    let callee = unwrap_ts_expr(&new_expr.callee);
    // §13.3.5 — a `new C(...args)` whose arguments include a SpreadElement
    // must build its argument list dynamically. Route it straight to the
    // `Op::NewSpread` path, preserving the resolved constructor reference.
    if new_expr
        .arguments
        .iter()
        .any(|arg| matches!(arg, oxc_ast::ast::Argument::SpreadElement(_)))
    {
        let callee_reg = compile_expr(cx, callee, new_span)?;
        let args_reg = compile_spread_call_args(cx, &new_expr.arguments, new_span)?;
        cx.reset_scratch(mark);
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::NewSpread,
            vec![
                Operand::Register(dst),
                Operand::Register(callee_reg),
                Operand::Register(args_reg),
            ],
            new_span,
        );
        return Ok(dst);
    }
    // §13.3.5 NewExpression — positional arguments (spread is handled by the
    // early-return path above).
    let callee_reg = compile_expr(cx, callee, new_span)?;
    let arg_regs = compile_call_args(cx, &new_expr.arguments, new_span)?;
    check_call_arity(arg_regs.len(), "Op::New", new_span)?;
    cx.reset_scratch(mark);
    let dst = cx.alloc_scratch();
    let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
    operands.push(Operand::Register(dst));
    operands.push(Operand::Register(callee_reg));
    operands.push(Operand::ConstIndex(arg_regs.len() as u32));
    operands.extend(arg_regs.into_iter().map(Operand::Register));
    cx.emit(Op::New, operands, new_span);
    Ok(dst)
}

#[cfg(test)]
mod tests {
    use crate::{CompileError, SyntaxSourceKind, compile_script_source};

    #[test]
    fn dense_construction_checks_its_wire_argument_bound() {
        for count in [crate::MAX_DENSE_CALL_ARGS, crate::MAX_DENSE_CALL_ARGS + 1] {
            let arguments = vec!["0"; count].join(",");
            let source = format!("new C({arguments});");
            let result =
                compile_script_source(&source, SyntaxSourceKind::JavaScript, "wide-new.js");
            if count == crate::MAX_DENSE_CALL_ARGS {
                crate::context_tests::assert_verified(&result.expect("encoded construction"));
            } else {
                assert!(
                    matches!(result, Err(CompileError::Unsupported { node, .. }) if node.contains("Op::New") && node.contains("arg limit"))
                );
            }
        }
    }
}
