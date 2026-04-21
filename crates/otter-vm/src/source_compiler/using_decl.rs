use super::*;
use crate::bytecode::{BytecodeBuilder, Opcode, Operand};
use oxc_ast::ast::{
    BindingPattern, Expression, ForStatement, Statement, VariableDeclaration,
    VariableDeclarationKind,
};

use super::try_finally::{
    lower_synthetic_try_finally, lower_synthetic_try_finally_with_internal_jumps,
};

pub(super) fn lower_function_top_statement_list<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    statements: &'a [Statement<'a>],
) -> Result<(), SourceLoweringError> {
    lower_statement_list_slice(builder, ctx, statements, lower_top_statement)
}

pub(super) fn lower_nested_statement_list<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    statements: &'a [Statement<'a>],
) -> Result<(), SourceLoweringError> {
    lower_statement_list_slice(builder, ctx, statements, lower_nested_block_statement)
}

pub(super) fn lower_top_level_statement_list<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    statements: &[&'a Statement<'a>],
) -> Result<(), SourceLoweringError> {
    lower_statement_list_ref_slice(builder, ctx, statements, lower_top_statement)
}

pub(super) fn lower_classic_for_using_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    for_stmt: &'a ForStatement<'a>,
    decl: &'a VariableDeclaration<'a>,
) -> Result<(), SourceLoweringError> {
    let scope = ctx.snapshot_scope();
    let loop_header = builder.new_label();
    let loop_exit = builder.new_label();
    let loop_continue = builder.new_label();

    let result = (|| -> Result<(), SourceLoweringError> {
        builder.emit(Opcode::PushUsingScope, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode PushUsingScope (for using): {err:?}"))
        })?;
        lower_synthetic_try_finally_with_internal_jumps(
            builder,
            ctx,
            vec![loop_continue, loop_exit],
            |builder, ctx| {
                lower_using_decl(builder, ctx, decl)?;
                lower_classic_for_after_using_init(
                    builder,
                    ctx,
                    for_stmt,
                    loop_header,
                    loop_continue,
                    loop_exit,
                )
            },
            |builder, _ctx| {
                builder
                    .emit(Opcode::DisposeUsingScope, &[])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode DisposeUsingScope (for using): {err:?}"
                        ))
                    })?;
                Ok(())
            },
        )
    })();
    ctx.restore_scope(scope);
    result
}

fn lower_classic_for_after_using_init<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    for_stmt: &'a ForStatement<'a>,
    loop_header: Label,
    loop_continue: Label,
    loop_exit: Label,
) -> Result<(), SourceLoweringError> {
    builder
        .bind_label(loop_header)
        .map_err(|err| SourceLoweringError::Internal(format!("bind for using header: {err:?}")))?;

    if let Some(test) = &for_stmt.test {
        lower_return_expression(builder, ctx, test)?;
    } else {
        builder.emit(Opcode::LdaTrue, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode LdaTrue (for using): {err:?}"))
        })?;
    }
    builder
        .emit_jump_to(Opcode::JumpIfToBooleanFalse, loop_exit)
        .map_err(|err| {
            SourceLoweringError::Internal(format!(
                "encode JumpIfToBooleanFalse (for using): {err:?}"
            ))
        })?;

    ctx.enter_loop(LoopLabels {
        break_label: loop_exit,
        continue_label: Some(loop_continue),
        label: ctx.take_pending_loop_label(),
    });
    let body_result = lower_nested_statement(builder, ctx, &for_stmt.body);
    ctx.exit_loop();
    body_result?;

    builder.bind_label(loop_continue).map_err(|err| {
        SourceLoweringError::Internal(format!("bind for using continue: {err:?}"))
    })?;
    lower_classic_for_update(builder, ctx, for_stmt.update.as_ref())?;

    builder
        .emit_jump_to(Opcode::Jump, loop_header)
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Jump (for using back): {err:?}"))
        })?;
    builder
        .bind_label(loop_exit)
        .map_err(|err| SourceLoweringError::Internal(format!("bind for using exit: {err:?}")))?;
    Ok(())
}

fn lower_classic_for_update<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    update: Option<&'a Expression<'a>>,
) -> Result<(), SourceLoweringError> {
    let Some(update) = update else {
        return Ok(());
    };
    match update {
        Expression::AssignmentExpression(assign) => {
            lower_assignment_expression(builder, ctx, assign)
        }
        Expression::UpdateExpression(update_expr) => {
            lower_update_expression(builder, ctx, update_expr)
        }
        Expression::CallExpression(call) => lower_call_expression(builder, ctx, call),
        other => lower_return_expression(builder, ctx, other),
    }
}

fn lower_statement_list_slice<'a, Lower>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    statements: &'a [Statement<'a>],
    lower_non_using: Lower,
) -> Result<(), SourceLoweringError>
where
    Lower: Copy
        + Fn(
            &mut BytecodeBuilder,
            &mut LoweringContext<'a>,
            &'a Statement<'a>,
        ) -> Result<(), SourceLoweringError>,
{
    let mut index = 0usize;
    while index < statements.len() {
        let stmt = &statements[index];
        if let Statement::VariableDeclaration(decl) = stmt
            && matches!(
                decl.kind,
                VariableDeclarationKind::Using | VariableDeclarationKind::AwaitUsing
            )
        {
            builder.emit(Opcode::PushUsingScope, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode PushUsingScope: {err:?}"))
            })?;
            let rest = &statements[index + 1..];
            return lower_synthetic_try_finally(
                builder,
                ctx,
                |builder, ctx| {
                    lower_using_decl(builder, ctx, decl)?;
                    lower_statement_list_slice(builder, ctx, rest, lower_non_using)
                },
                |builder, _ctx| {
                    builder
                        .emit(Opcode::DisposeUsingScope, &[])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode DisposeUsingScope: {err:?}"
                            ))
                        })?;
                    Ok(())
                },
            );
        }
        lower_non_using(builder, ctx, stmt)?;
        index += 1;
    }
    Ok(())
}

fn lower_statement_list_ref_slice<'a, Lower>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    statements: &[&'a Statement<'a>],
    lower_non_using: Lower,
) -> Result<(), SourceLoweringError>
where
    Lower: Copy
        + Fn(
            &mut BytecodeBuilder,
            &mut LoweringContext<'a>,
            &'a Statement<'a>,
        ) -> Result<(), SourceLoweringError>,
{
    let mut index = 0usize;
    while index < statements.len() {
        let stmt = statements[index];
        if let Statement::VariableDeclaration(decl) = stmt
            && matches!(
                decl.kind,
                VariableDeclarationKind::Using | VariableDeclarationKind::AwaitUsing
            )
        {
            builder.emit(Opcode::PushUsingScope, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode PushUsingScope: {err:?}"))
            })?;
            let rest = &statements[index + 1..];
            return lower_synthetic_try_finally(
                builder,
                ctx,
                |builder, ctx| {
                    lower_using_decl(builder, ctx, decl)?;
                    lower_statement_list_ref_slice(builder, ctx, rest, lower_non_using)
                },
                |builder, _ctx| {
                    builder
                        .emit(Opcode::DisposeUsingScope, &[])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode DisposeUsingScope: {err:?}"
                            ))
                        })?;
                    Ok(())
                },
            );
        }
        lower_non_using(builder, ctx, stmt)?;
        index += 1;
    }
    Ok(())
}

fn lower_nested_block_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    stmt: &'a Statement<'a>,
) -> Result<(), SourceLoweringError> {
    match stmt {
        Statement::VariableDeclaration(decl) => lower_let_const_declaration(builder, ctx, decl),
        _ => lower_nested_statement(builder, ctx, stmt),
    }
}

fn lower_using_decl<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    decl: &'a VariableDeclaration<'a>,
) -> Result<(), SourceLoweringError> {
    if decl.declarations.is_empty() {
        return Err(SourceLoweringError::unsupported(
            "empty_variable_declaration",
            decl.span,
        ));
    }
    let await_dispose = decl.kind == VariableDeclarationKind::AwaitUsing;
    for declarator in &decl.declarations {
        let init = declarator.init.as_ref().ok_or_else(|| {
            SourceLoweringError::unsupported("uninitialized_binding", declarator.span)
        })?;
        let BindingPattern::BindingIdentifier(ident) = &declarator.id else {
            return Err(SourceLoweringError::unsupported(
                "parser_recovery_using_binding_pattern",
                declarator.id.span(),
            ));
        };
        let name = ident.name.as_str();
        let slot = ctx.allocate_local(name, true, declarator.span)?;
        lower_return_expression(builder, ctx, init)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (using init): {err:?}"))
            })?;
        ctx.mark_initialized(name)?;
        emit_add_disposable_resource(builder, slot, await_dispose)?;
    }
    Ok(())
}

pub(super) fn lower_loop_using_iteration<'a, LowerBody>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    resource_reg: u16,
    await_dispose: bool,
    lower_body: LowerBody,
) -> Result<(), SourceLoweringError>
where
    LowerBody:
        FnOnce(&mut BytecodeBuilder, &mut LoweringContext<'a>) -> Result<(), SourceLoweringError>,
{
    builder.emit(Opcode::PushUsingScope, &[]).map_err(|err| {
        SourceLoweringError::Internal(format!("encode PushUsingScope (loop using): {err:?}"))
    })?;
    lower_synthetic_try_finally(
        builder,
        ctx,
        |builder, ctx| {
            emit_add_disposable_resource(builder, resource_reg, await_dispose)?;
            lower_body(builder, ctx)
        },
        |builder, _ctx| {
            builder
                .emit(Opcode::DisposeUsingScope, &[])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode DisposeUsingScope (loop using): {err:?}"
                    ))
                })?;
            Ok(())
        },
    )
}

fn emit_add_disposable_resource(
    builder: &mut BytecodeBuilder,
    resource_reg: u16,
    await_dispose: bool,
) -> Result<(), SourceLoweringError> {
    builder
        .emit(
            Opcode::AddDisposableResource,
            &[
                Operand::Reg(u32::from(resource_reg)),
                Operand::Imm(if await_dispose { 1 } else { 0 }),
            ],
        )
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode AddDisposableResource: {err:?}"))
        })?;
    Ok(())
}
