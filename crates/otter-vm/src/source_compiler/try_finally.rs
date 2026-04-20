use super::*;
use oxc_ast::ast::{
    BindingPattern, BreakStatement, ContinueStatement, ReturnStatement, TryStatement,
};

enum AbruptCompletionTarget {
    Return,
    Jump(Label),
}

pub(super) fn lower_return_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    ret: &'a ReturnStatement<'a>,
) -> Result<(), SourceLoweringError> {
    match ret.argument.as_ref() {
        Some(argument) => {
            lower_return_expression(builder, ctx, argument)?;
        }
        None => {
            builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaUndefined (bare return): {err:?}"))
            })?;
        }
    }
    emit_abrupt_completion(builder, ctx, AbruptCompletionTarget::Return)
}

pub(super) fn lower_break_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    break_stmt: &'a BreakStatement<'a>,
) -> Result<(), SourceLoweringError> {
    let target = if let Some(label) = &break_stmt.label {
        ctx.find_break_label_by_name(label.name.as_str())
            .ok_or_else(|| SourceLoweringError::unsupported("undeclared_label", break_stmt.span))?
    } else {
        ctx.innermost_break_label().ok_or_else(|| {
            SourceLoweringError::unsupported("break_outside_loop", break_stmt.span)
        })?
    };
    emit_abrupt_completion(builder, ctx, AbruptCompletionTarget::Jump(target))
}

pub(super) fn lower_continue_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    cont_stmt: &'a ContinueStatement<'a>,
) -> Result<(), SourceLoweringError> {
    let target = if let Some(label) = &cont_stmt.label {
        ctx.find_continue_label_by_name(label.name.as_str())
            .ok_or_else(|| SourceLoweringError::unsupported("undeclared_label", cont_stmt.span))?
    } else {
        ctx.innermost_continue_label().ok_or_else(|| {
            SourceLoweringError::unsupported("continue_outside_loop", cont_stmt.span)
        })?
    };
    emit_abrupt_completion(builder, ctx, AbruptCompletionTarget::Jump(target))
}

fn emit_abrupt_completion<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    completion: AbruptCompletionTarget,
) -> Result<(), SourceLoweringError> {
    let finally_targets = ctx.active_finally_targets();
    if finally_targets.is_empty() {
        return match completion {
            AbruptCompletionTarget::Return => builder
                .emit(Opcode::Return, &[])
                .map(|_| ())
                .map_err(|err| SourceLoweringError::Internal(format!("encode Return: {err:?}"))),
            AbruptCompletionTarget::Jump(target) => builder
                .emit_jump_to(Opcode::Jump, target)
                .map(|_| ())
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Jump (abrupt): {err:?}"))
                }),
        };
    }

    for label in &finally_targets[..finally_targets.len() - 1] {
        builder
            .emit_label_immediate(Opcode::PushPendingFinally, *label)
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode PushPendingFinally (abrupt completion): {err:?}"
                ))
            })?;
    }

    match completion {
        AbruptCompletionTarget::Return => {
            builder.emit(Opcode::SetPendingReturn, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode SetPendingReturn (abrupt completion): {err:?}"
                ))
            })?;
        }
        AbruptCompletionTarget::Jump(target) => {
            builder
                .emit_label_immediate(Opcode::SetPendingJump, target)
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode SetPendingJump (abrupt completion): {err:?}"
                    ))
                })?;
        }
    }

    builder
        .emit_jump_to(
            Opcode::Jump,
            *finally_targets.last().expect("non-empty finally stack"),
        )
        .map(|_| ())
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Jump (abrupt to finally): {err:?}"))
        })
}

pub(super) fn lower_try_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    try_stmt: &'a TryStatement<'a>,
) -> Result<(), SourceLoweringError> {
    if try_stmt.handler.is_none() && try_stmt.finalizer.is_none() {
        return Err(SourceLoweringError::unsupported(
            "try_without_catch_or_finally",
            try_stmt.span,
        ));
    }

    let try_start = builder.new_label();
    let try_end = builder.new_label();
    let after_try = builder.new_label();
    let catch_start = try_stmt.handler.as_ref().map(|_| builder.new_label());
    let catch_end = try_stmt.handler.as_ref().map(|_| builder.new_label());
    let finally_handler = try_stmt.finalizer.as_ref().map(|_| builder.new_label());
    let finally_normal = try_stmt.finalizer.as_ref().map(|_| builder.new_label());

    if let Some(normal_entry) = finally_normal {
        ctx.enter_finally_frame(FinallyFrame { normal_entry });
    }

    let try_catch_result = (|| -> Result<(), SourceLoweringError> {
        builder
            .bind_label(try_start)
            .map_err(|err| SourceLoweringError::Internal(format!("bind try_start: {err:?}")))?;
        lower_block_statement(builder, ctx, &try_stmt.block)?;
        builder
            .bind_label(try_end)
            .map_err(|err| SourceLoweringError::Internal(format!("bind try_end: {err:?}")))?;

        let try_normal_target = finally_normal.unwrap_or(after_try);
        builder
            .emit_jump_to(Opcode::Jump, try_normal_target)
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Jump (try normal exit): {err:?}"))
            })?;

        if let (Some(handler), Some(catch_start), Some(catch_end)) =
            (try_stmt.handler.as_deref(), catch_start, catch_end)
        {
            builder.bind_label(catch_start).map_err(|err| {
                SourceLoweringError::Internal(format!("bind catch_start: {err:?}"))
            })?;
            ctx.record_exception_handler(try_start, try_end, catch_start);

            let scope = ctx.snapshot_scope();
            let lower_catch = (|| -> Result<(), SourceLoweringError> {
                if let Some(param) = handler.param.as_ref() {
                    builder.emit(Opcode::LdaException, &[]).map_err(|err| {
                        SourceLoweringError::Internal(format!("encode LdaException: {err:?}"))
                    })?;
                    match &param.pattern {
                        BindingPattern::BindingIdentifier(ident) => {
                            let name = ident.name.as_str();
                            let slot = ctx.allocate_local(name, false, ident.span)?;
                            builder
                                .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
                                .map_err(|err| {
                                    SourceLoweringError::Internal(format!(
                                        "encode Star (catch param): {err:?}"
                                    ))
                                })?;
                            ctx.mark_initialized(name)?;
                        }
                        BindingPattern::ArrayPattern(_)
                        | BindingPattern::ObjectPattern(_)
                        | BindingPattern::AssignmentPattern(_) => {
                            let exc_slot = ctx.allocate_anonymous_local()?;
                            builder
                                .emit(Opcode::Star, &[Operand::Reg(u32::from(exc_slot))])
                                .map_err(|err| {
                                    SourceLoweringError::Internal(format!(
                                        "encode Star (destructured catch): {err:?}"
                                    ))
                                })?;
                            lower_pattern_bind(builder, ctx, &param.pattern, exc_slot, false)?;
                        }
                    }
                } else {
                    builder.emit(Opcode::LdaException, &[]).map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode LdaException (bindingless): {err:?}"
                        ))
                    })?;
                }
                lower_block_statement(builder, ctx, &handler.body)?;
                Ok(())
            })();
            ctx.restore_scope(scope);
            lower_catch?;

            builder
                .bind_label(catch_end)
                .map_err(|err| SourceLoweringError::Internal(format!("bind catch_end: {err:?}")))?;
            builder
                .emit_jump_to(Opcode::Jump, try_normal_target)
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Jump (catch normal exit): {err:?}"
                    ))
                })?;
        }

        Ok(())
    })();

    if finally_normal.is_some() {
        ctx.exit_finally_frame();
    }
    try_catch_result?;

    if let (Some(finalizer), Some(finally_handler), Some(finally_normal)) = (
        try_stmt.finalizer.as_deref(),
        finally_handler,
        finally_normal,
    ) {
        match (catch_start, catch_end) {
            (Some(catch_start), Some(catch_end)) => {
                ctx.record_exception_handler(catch_start, catch_end, finally_handler);
            }
            _ => ctx.record_exception_handler(try_start, try_end, finally_handler),
        }

        builder.bind_label(finally_handler).map_err(|err| {
            SourceLoweringError::Internal(format!("bind finally_handler: {err:?}"))
        })?;
        lower_block_statement(builder, ctx, finalizer)?;
        builder.emit(Opcode::ReThrow, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode ReThrow (finally): {err:?}"))
        })?;

        builder.bind_label(finally_normal).map_err(|err| {
            SourceLoweringError::Internal(format!("bind finally_normal: {err:?}"))
        })?;
        lower_block_statement(builder, ctx, finalizer)?;
        builder.emit(Opcode::ResumeAbrupt, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode ResumeAbrupt (finally): {err:?}"))
        })?;
    }

    builder
        .bind_label(after_try)
        .map_err(|err| SourceLoweringError::Internal(format!("bind after_try: {err:?}")))?;

    Ok(())
}
