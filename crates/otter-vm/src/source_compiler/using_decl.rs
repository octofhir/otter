use super::*;
use crate::bytecode::{BytecodeBuilder, Opcode, Operand};
use oxc_ast::ast::{BindingPattern, Statement, VariableDeclaration, VariableDeclarationKind};

use super::try_finally::lower_synthetic_try_finally;

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
                    lower_using_declaration(builder, ctx, decl)?;
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
                    lower_using_declaration(builder, ctx, decl)?;
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

fn lower_using_declaration<'a>(
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
                "using_declaration",
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
