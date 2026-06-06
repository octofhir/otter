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
    // §27.5.3.7 `yield*` — sync generators lower the full
    // delegation state machine (resume values thread into
    // `next(v)`, `.throw()` / `.return()` forward to the inner
    // iterator, inner results surface verbatim). Async generators
    // keep the simpler await-pump below.
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
            let resume_arg_reg = cx.alloc_scratch();
            cx.emit(Op::LoadUndefined, [Operand::Register(resume_arg_reg)], span);
            let next_name = cx.intern_string_constant("next");
            let loop_top = cx.next_pc;
            cx.emit(
                Op::CallMethodValue,
                vec![
                    Operand::Register(result_reg),
                    Operand::Register(iter_reg),
                    Operand::ConstIndex(next_name),
                    Operand::ConstIndex(1),
                    Operand::Register(resume_arg_reg),
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
            cx.emit(
                Op::Yield,
                [
                    Operand::Register(resume_arg_reg),
                    Operand::Register(value_reg),
                ],
                span,
            );
            let back_jmp = cx.emit_branch_placeholder(Op::Jump, None, span);
            cx.patch_branch(back_jmp, loop_top);
            cx.patch_branch_to_here(exit_jmp);
            let dst = cx.alloc_scratch();
            cx.emit_load_property(dst, awaited_reg, "value", span);
            return Ok(dst);
        }
        // §27.5.3.7 sync `yield*` — full delegation state machine.
        // The resume kind code (0 = next, 1 = throw, 2 = return) is
        // delivered by `Op::YieldDelegate` so abrupt resumes forward
        // to the inner iterator's `throw` / `return` method instead
        // of unwinding the generator body. The inner iterator result
        // object surfaces from the outer `.next()` verbatim.
        // §7.4.3 GetIterator — resolve the RAW iterator object via
        // GetMethod(obj, @@iterator) + Call, instead of
        // `Op::GetIterator` (whose internal IteratorState wrapper
        // hides the iterator's own `next` / `throw` / `return`
        // methods from the delegation loop).
        let iter_sym = cx.alloc_scratch();
        let iter_sym_idx = cx.intern_string_constant("iterator");
        cx.emit(
            Op::SymbolLoad,
            [
                Operand::Register(iter_sym),
                Operand::ConstIndex(iter_sym_idx),
            ],
            span,
        );
        let iter_method = cx.alloc_scratch();
        cx.emit(
            Op::LoadElement,
            vec![
                Operand::Register(iter_method),
                Operand::Register(arg_reg),
                Operand::Register(iter_sym),
            ],
            span,
        );
        let no_iter = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(iter_method), span);
        let have_iter = cx.emit_branch_placeholder(Op::Jump, None, span);
        cx.patch_branch_to_here(no_iter);
        emit_yield_star_type_error(cx, "yield* argument is not iterable", span);
        cx.patch_branch_to_here(have_iter);
        let iter_reg = cx.alloc_scratch();
        cx.emit(
            Op::CallWithThis,
            vec![
                Operand::Register(iter_reg),
                Operand::Register(iter_method),
                Operand::Register(arg_reg),
                Operand::ConstIndex(0),
            ],
            span,
        );
        // §7.4.3 GetIterator step 3 — the `next` method is read once
        // and cached in the iterator record.
        let next_m = cx.alloc_scratch();
        cx.emit_load_property(next_m, iter_reg, "next", span);
        let kind_reg = cx.alloc_scratch();
        cx.emit(
            Op::LoadInt32,
            [Operand::Register(kind_reg), Operand::Imm32(0)],
            span,
        );
        let recv_reg = cx.alloc_scratch();
        cx.emit(Op::LoadUndefined, [Operand::Register(recv_reg)], span);
        let inner_reg = cx.alloc_scratch();

        let loop_top = cx.next_pc;
        // Dispatch on the resume kind.
        let one_reg = cx.alloc_scratch();
        cx.emit(
            Op::LoadInt32,
            [Operand::Register(one_reg), Operand::Imm32(1)],
            span,
        );
        let is_throw = cx.alloc_scratch();
        cx.emit(
            Op::Equal,
            [
                Operand::Register(is_throw),
                Operand::Register(kind_reg),
                Operand::Register(one_reg),
            ],
            span,
        );
        let to_throw = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(is_throw), span);
        let two_reg = cx.alloc_scratch();
        cx.emit(
            Op::LoadInt32,
            [Operand::Register(two_reg), Operand::Imm32(2)],
            span,
        );
        let is_return = cx.alloc_scratch();
        cx.emit(
            Op::Equal,
            [
                Operand::Register(is_return),
                Operand::Register(kind_reg),
                Operand::Register(two_reg),
            ],
            span,
        );
        let to_return = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(is_return), span);
        // kind == next: innerResult = Call(nextMethod, iterator, «received»).
        cx.emit(
            Op::CallWithThis,
            vec![
                Operand::Register(inner_reg),
                Operand::Register(next_m),
                Operand::Register(iter_reg),
                Operand::ConstIndex(1),
                Operand::Register(recv_reg),
            ],
            span,
        );
        let next_to_check = cx.emit_branch_placeholder(Op::Jump, None, span);

        // kind == throw (§27.5.3.7 step 7.b).
        cx.patch_branch_to_here(to_throw);
        let throw_m = cx.alloc_scratch();
        cx.emit_load_property(throw_m, iter_reg, "throw", span);
        let throw_absent = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(throw_m), span);
        cx.emit(
            Op::CallWithThis,
            vec![
                Operand::Register(inner_reg),
                Operand::Register(throw_m),
                Operand::Register(iter_reg),
                Operand::ConstIndex(1),
                Operand::Register(recv_reg),
            ],
            span,
        );
        let throw_to_check = cx.emit_branch_placeholder(Op::Jump, None, span);
        // No `throw` method: close the inner iterator, then raise
        // TypeError (protocol violation).
        cx.patch_branch_to_here(throw_absent);
        cx.emit(Op::IteratorClose, [Operand::Register(iter_reg)], span);
        emit_yield_star_type_error(cx, "The iterator does not provide a 'throw' method", span);

        // kind == return (§27.5.3.7 step 7.c).
        cx.patch_branch_to_here(to_return);
        let return_m = cx.alloc_scratch();
        cx.emit_load_property(return_m, iter_reg, "return", span);
        let return_absent = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(return_m), span);
        cx.emit(
            Op::CallWithThis,
            vec![
                Operand::Register(inner_reg),
                Operand::Register(return_m),
                Operand::Register(iter_reg),
                Operand::ConstIndex(1),
                Operand::Register(recv_reg),
            ],
            span,
        );
        let return_to_check = cx.emit_branch_placeholder(Op::Jump, None, span);
        // No `return` method: GeneratorReturn(received.value).
        cx.patch_branch_to_here(return_absent);
        cx.emit(Op::ReturnValue, [Operand::Register(recv_reg)], span);

        // Common: innerResult must be an Object.
        cx.patch_branch_to_here(next_to_check);
        cx.patch_branch_to_here(throw_to_check);
        cx.patch_branch_to_here(return_to_check);
        let non_object = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(inner_reg), span);
        let type_reg = cx.alloc_scratch();
        cx.emit(
            Op::TypeOf,
            [Operand::Register(type_reg), Operand::Register(inner_reg)],
            span,
        );
        let object_str = cx.alloc_scratch();
        let object_idx = cx.intern_string_constant("object");
        cx.emit(
            Op::LoadString,
            [
                Operand::Register(object_str),
                Operand::ConstIndex(object_idx),
            ],
            span,
        );
        let is_object = cx.alloc_scratch();
        cx.emit(
            Op::Equal,
            [
                Operand::Register(is_object),
                Operand::Register(type_reg),
                Operand::Register(object_str),
            ],
            span,
        );
        let obj_ok = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(is_object), span);
        let function_str = cx.alloc_scratch();
        let function_idx = cx.intern_string_constant("function");
        cx.emit(
            Op::LoadString,
            [
                Operand::Register(function_str),
                Operand::ConstIndex(function_idx),
            ],
            span,
        );
        let is_function = cx.alloc_scratch();
        cx.emit(
            Op::Equal,
            [
                Operand::Register(is_function),
                Operand::Register(type_reg),
                Operand::Register(function_str),
            ],
            span,
        );
        let fn_ok = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(is_function), span);
        cx.patch_branch_to_here(non_object);
        emit_yield_star_type_error(cx, "Iterator result is not an object", span);
        cx.patch_branch_to_here(obj_ok);
        cx.patch_branch_to_here(fn_ok);

        let done_reg = cx.alloc_scratch();
        cx.emit_load_property(done_reg, inner_reg, "done", span);
        let exit_jmp = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(done_reg), span);
        cx.emit(
            Op::YieldDelegate,
            [
                Operand::Register(kind_reg),
                Operand::Register(recv_reg),
                Operand::Register(inner_reg),
            ],
            span,
        );
        let back_jmp = cx.emit_branch_placeholder(Op::Jump, None, span);
        cx.patch_branch(back_jmp, loop_top);

        // Done: value = innerResult.value. A `return` resume
        // completes the generator with that value; `next` / `throw`
        // complete the yield* expression normally.
        cx.patch_branch_to_here(exit_jmp);
        let dst = cx.alloc_scratch();
        cx.emit_load_property(dst, inner_reg, "value", span);
        let final_two = cx.alloc_scratch();
        cx.emit(
            Op::LoadInt32,
            [Operand::Register(final_two), Operand::Imm32(2)],
            span,
        );
        let final_is_return = cx.alloc_scratch();
        cx.emit(
            Op::Equal,
            [
                Operand::Register(final_is_return),
                Operand::Register(kind_reg),
                Operand::Register(final_two),
            ],
            span,
        );
        let not_return = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(final_is_return), span);
        cx.emit(Op::ReturnValue, [Operand::Register(dst)], span);
        cx.patch_branch_to_here(not_return);
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

/// Throw a TypeError with `message` from the `yield*` delegation
/// loop (§27.5.3.7 protocol violations).
fn emit_yield_star_type_error(cx: &mut Compiler, message: &str, span: (u32, u32)) {
    let message_reg = cx.alloc_scratch();
    let message_idx = cx.intern_string_constant(message);
    cx.emit(
        Op::LoadString,
        [
            Operand::Register(message_reg),
            Operand::ConstIndex(message_idx),
        ],
        span,
    );
    let error_reg = cx.alloc_scratch();
    let kind_idx = cx.intern_string_constant("TypeError");
    cx.emit(
        Op::NewBuiltinError,
        [
            Operand::Register(error_reg),
            Operand::ConstIndex(kind_idx),
            Operand::Register(message_reg),
        ],
        span,
    );
    cx.emit(Op::Throw, [Operand::Register(error_reg)], span);
}
