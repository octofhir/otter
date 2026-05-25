//! Async control expression lowering.
//!
//! # Contents
//! - [`compile_await`] — lowers await expressions.
//! - [`compile_yield`] — lowers yield expressions.
//!
//! # See also
//! - [`super`] — expression dispatch and shared helpers.

use crate::*;
use oxc_ast::ast::{AwaitExpression, YieldExpression};

pub(crate) fn compile_await(
    cx: &mut Compiler,
    a: &AwaitExpression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let _ = span;
    let span = (a.span.start, a.span.end);
    let src = compile_expr(cx, &a.argument, span)?;
    let dst = cx.alloc_scratch();
    cx.emit(
        Op::Await,
        [Operand::Register(dst), Operand::Register(src)],
        span,
    );
    Ok(dst)
}

pub(crate) fn compile_yield(
    cx: &mut Compiler,
    y: &YieldExpression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let _ = span;
    let span = (y.span.start, y.span.end);
    // §15.5.5 `yield*` — delegate to an inner iterable. The
    // foundation lowers it as the canonical for-of-style
    // pump:
    //
    //   const iter = GetIterator(arg);
    //   while (true) {
    //     const { value, done } = iter.next();
    //     if (done) { break; }       // value of yield* is `undefined`
    //     yield value;
    //   }
    //
    // Spec demands threading the resume value into iter.next
    // and forwarding `.return` / `.throw` through; both are
    // filed for a follow-up.
    // <https://tc39.es/ecma262/#sec-generator-function-definitions-runtime-semantics-evaluation>
    if y.delegate {
        let arg = match &y.argument {
            Some(a) => a,
            None => {
                return Err(CompileError::Unsupported {
                    node: "yield*: missing argument".to_string(),
                    span,
                });
            }
        };
        let arg_reg = compile_expr(cx, arg, span)?;
        if cx.is_async_generator {
            let iter_reg = cx.alloc_scratch();
            cx.emit(
                Op::GetAsyncIterator,
                [Operand::Register(iter_reg), Operand::Register(arg_reg)],
                span,
            );
            let result_reg = cx.alloc_scratch();
            let awaited_reg = cx.alloc_scratch();
            let done_reg = cx.alloc_scratch();
            let value_reg = cx.alloc_scratch();
            let next_name = cx.intern_string_constant("next");
            let loop_top = cx.next_pc;
            cx.emit(
                Op::CallMethodValue,
                vec![
                    Operand::Register(result_reg),
                    Operand::Register(iter_reg),
                    Operand::ConstIndex(next_name),
                    Operand::ConstIndex(0),
                ],
                span,
            );
            cx.emit(
                Op::Await,
                [
                    Operand::Register(awaited_reg),
                    Operand::Register(result_reg),
                ],
                span,
            );
            cx.emit_load_property(done_reg, awaited_reg, "done", span);
            let exit_jmp = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(done_reg), span);
            cx.emit_load_property(value_reg, awaited_reg, "value", span);
            let yield_dst = cx.alloc_scratch();
            cx.emit(
                Op::Yield,
                [Operand::Register(yield_dst), Operand::Register(value_reg)],
                span,
            );
            let back_jmp = cx.emit_branch_placeholder(Op::Jump, None, span);
            cx.patch_branch(back_jmp, loop_top);
            cx.patch_branch_to_here(exit_jmp);
            let dst = cx.alloc_scratch();
            cx.emit_load_property(dst, awaited_reg, "value", span);
            return Ok(dst);
        }
        let iter_reg = cx.alloc_scratch();
        cx.emit(
            Op::GetIterator,
            [Operand::Register(iter_reg), Operand::Register(arg_reg)],
            span,
        );
        let value_reg = cx.alloc_scratch();
        let done_reg = cx.alloc_scratch();
        let loop_top = cx.next_pc;
        cx.emit(
            Op::IteratorNext,
            vec![
                Operand::Register(value_reg),
                Operand::Register(done_reg),
                Operand::Register(iter_reg),
            ],
            span,
        );
        let exit_jmp = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(done_reg), span);
        let yield_dst = cx.alloc_scratch();
        cx.emit(
            Op::Yield,
            [Operand::Register(yield_dst), Operand::Register(value_reg)],
            span,
        );
        let back_jmp = cx.emit_branch_placeholder(Op::Jump, None, span);
        cx.patch_branch(back_jmp, loop_top);
        cx.patch_branch_to_here(exit_jmp);
        let dst = cx.alloc_scratch();
        cx.emit(Op::LoadUndefined, [Operand::Register(dst)], span);
        return Ok(dst);
    }
    let src = match &y.argument {
        Some(arg) => compile_expr(cx, arg, span)?,
        None => {
            let r = cx.alloc_scratch();
            cx.emit(Op::LoadUndefined, [Operand::Register(r)], span);
            r
        }
    };
    let dst = cx.alloc_scratch();
    cx.emit(
        Op::Yield,
        [Operand::Register(dst), Operand::Register(src)],
        span,
    );
    Ok(dst)
}
