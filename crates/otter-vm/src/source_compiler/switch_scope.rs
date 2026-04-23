use super::*;
use oxc_ast::ast::{
    BindingIdentifier, BindingPattern, Expression, Statement, SwitchStatement, VariableDeclaration,
    VariableDeclarationKind, VariableDeclarator,
};

#[derive(Clone, Copy)]
enum SwitchBindingMode {
    Lexical { is_const: bool },
    Var,
}

pub(super) fn hoist_switch_var_declarations<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    sw: &'a SwitchStatement<'a>,
) -> Result<(), SourceLoweringError> {
    let mut seen: Vec<&'a str> = Vec::new();
    for case in &sw.cases {
        for stmt in &case.consequent {
            let Statement::VariableDeclaration(decl) = stmt else {
                continue;
            };
            if decl.kind != VariableDeclarationKind::Var {
                continue;
            }
            for declarator in &decl.declarations {
                let mut bindings = Vec::new();
                collect_switch_binding_identifiers(&declarator.id, &mut bindings);
                for ident in bindings {
                    let name = ident.name.as_str();
                    if seen.contains(&name) {
                        continue;
                    }
                    seen.push(name);
                    let slot = ctx.allocate_initialized_local(name, false, ident.span)?;
                    builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode LdaUndefined (switch var hoist): {err:?}"
                        ))
                    })?;
                    builder
                        .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode Star (switch var hoist): {err:?}"
                            ))
                        })?;
                }
            }
        }
    }
    Ok(())
}

pub(super) fn enter_switch_lexical_scope<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    sw: &'a SwitchStatement<'a>,
) -> Result<ScopeSnapshot, SourceLoweringError> {
    let scope = ctx.snapshot_scope();
    let mut hoisted_regs: Vec<RegisterIndex> = Vec::new();

    for case in &sw.cases {
        for stmt in &case.consequent {
            let Statement::VariableDeclaration(decl) = stmt else {
                continue;
            };
            match decl.kind {
                VariableDeclarationKind::Let | VariableDeclarationKind::Const => {
                    let is_const = decl.kind == VariableDeclarationKind::Const;
                    for declarator in &decl.declarations {
                        let mut bindings = Vec::new();
                        collect_switch_binding_identifiers(&declarator.id, &mut bindings);
                        for ident in bindings {
                            let slot = ctx.allocate_hoisted_local(
                                ident.name.as_str(),
                                is_const,
                                ident.span,
                            )?;
                            hoisted_regs.push(slot);
                        }
                    }
                }
                VariableDeclarationKind::Using | VariableDeclarationKind::AwaitUsing => {
                    return Err(SourceLoweringError::unsupported(
                        "parser_recovery_switch_using_decl",
                        decl.span,
                    ));
                }
                VariableDeclarationKind::Var => {}
            }
        }
    }

    for reg in hoisted_regs {
        builder.emit(Opcode::LdaTheHole, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode LdaTheHole (switch lexical): {err:?}"))
        })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(reg))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (switch lexical hole): {err:?}"))
            })?;
    }

    Ok(scope)
}

pub(super) fn lower_switch_case_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    stmt: &'a Statement<'a>,
) -> Result<(), SourceLoweringError> {
    match stmt {
        Statement::VariableDeclaration(decl) => match decl.kind {
            VariableDeclarationKind::Let | VariableDeclarationKind::Const => {
                lower_switch_lexical_declaration(builder, ctx, decl)
            }
            VariableDeclarationKind::Using | VariableDeclarationKind::AwaitUsing => Err(
                SourceLoweringError::unsupported("parser_recovery_switch_using_decl", decl.span),
            ),
            VariableDeclarationKind::Var => lower_switch_var_declaration(builder, ctx, decl),
        },
        _ => lower_nested_statement(builder, ctx, stmt),
    }
}

fn lower_switch_lexical_declaration<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    decl: &'a VariableDeclaration<'a>,
) -> Result<(), SourceLoweringError> {
    let is_const = decl.kind == VariableDeclarationKind::Const;
    if decl.declarations.is_empty() {
        return Err(SourceLoweringError::unsupported(
            "parser_recovery_empty_switch_lexical_decl",
            decl.span,
        ));
    }

    for declarator in &decl.declarations {
        lower_switch_lexical_declarator(builder, ctx, declarator, is_const)?;
    }
    Ok(())
}

fn lower_switch_lexical_declarator<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    declarator: &'a VariableDeclarator<'a>,
    is_const: bool,
) -> Result<(), SourceLoweringError> {
    let init = declarator.init.as_ref().ok_or_else(|| {
        SourceLoweringError::unsupported("uninitialized_binding", declarator.span)
    })?;

    match &declarator.id {
        BindingPattern::BindingIdentifier(ident) => {
            lower_switch_lexical_identifier_init(builder, ctx, ident, init)
        }
        pattern => {
            let src_slot = ctx.allocate_anonymous_local()?;
            (|| -> Result<(), SourceLoweringError> {
                lower_return_expression(builder, ctx, init)?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(src_slot))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (switch destruct src): {err:?}"
                        ))
                    })?;
                lower_switch_pattern_bind_existing(
                    builder,
                    ctx,
                    pattern,
                    src_slot,
                    SwitchBindingMode::Lexical { is_const },
                )
            })()
        }
    }
}

fn lower_switch_var_declaration<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    decl: &'a VariableDeclaration<'a>,
) -> Result<(), SourceLoweringError> {
    if decl.declarations.is_empty() {
        return Err(SourceLoweringError::unsupported(
            "parser_recovery_empty_switch_var_decl",
            decl.span,
        ));
    }

    for declarator in &decl.declarations {
        let Some(init) = declarator.init.as_ref() else {
            continue;
        };
        match &declarator.id {
            BindingPattern::BindingIdentifier(ident) => {
                lower_return_expression(builder, ctx, init)?;
                lower_switch_binding_identifier_from_acc(
                    builder,
                    ctx,
                    ident,
                    SwitchBindingMode::Var,
                )?;
            }
            pattern => {
                let src_slot = ctx.allocate_anonymous_local()?;
                lower_return_expression(builder, ctx, init)?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(src_slot))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (switch var destruct src): {err:?}"
                        ))
                    })?;
                lower_switch_pattern_bind_existing(
                    builder,
                    ctx,
                    pattern,
                    src_slot,
                    SwitchBindingMode::Var,
                )?;
            }
        }
    }
    Ok(())
}

fn lower_switch_lexical_identifier_init<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    ident: &'a BindingIdentifier<'a>,
    init: &'a Expression<'a>,
) -> Result<(), SourceLoweringError> {
    let binding = ctx
        .resolve_own(ident.name.as_str())
        .ok_or_else(|| SourceLoweringError::Internal("missing hoisted switch binding".into()))?;
    let BindingRef::Local {
        reg,
        initialized: false,
        runtime_tdz: true,
        ..
    } = binding
    else {
        return Err(SourceLoweringError::Internal(format!(
            "switch binding {} was not hoisted as runtime TDZ local",
            ident.name.as_str()
        )));
    };
    lower_return_expression(builder, ctx, init)?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(reg))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (switch lexical init): {err:?}"))
        })?;
    ctx.mark_initialized(ident.name.as_str())
}

fn collect_switch_binding_identifiers<'a>(
    pattern: &'a BindingPattern<'a>,
    out: &mut Vec<&'a BindingIdentifier<'a>>,
) {
    match pattern {
        BindingPattern::BindingIdentifier(ident) => out.push(ident),
        BindingPattern::ArrayPattern(pat) => {
            for element in pat.elements.iter().flatten() {
                collect_switch_binding_identifiers(element, out);
            }
            if let Some(rest) = pat.rest.as_deref() {
                collect_switch_binding_identifiers(&rest.argument, out);
            }
        }
        BindingPattern::ObjectPattern(pat) => {
            for prop in &pat.properties {
                collect_switch_binding_identifiers(&prop.value, out);
            }
            if let Some(rest) = pat.rest.as_deref() {
                collect_switch_binding_identifiers(&rest.argument, out);
            }
        }
        BindingPattern::AssignmentPattern(assign) => {
            collect_switch_binding_identifiers(&assign.left, out);
        }
    }
}

fn lower_switch_pattern_bind_existing<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    pattern: &'a BindingPattern<'a>,
    src_reg: RegisterIndex,
    mode: SwitchBindingMode,
) -> Result<(), SourceLoweringError> {
    match pattern {
        BindingPattern::BindingIdentifier(ident) => {
            lower_switch_binding_identifier_from_acc(builder, ctx, ident, mode)
        }
        BindingPattern::ArrayPattern(pat) => {
            lower_switch_array_pattern(builder, ctx, pat, src_reg, mode)
        }
        BindingPattern::ObjectPattern(pat) => {
            lower_switch_object_pattern(builder, ctx, pat, src_reg, mode)
        }
        BindingPattern::AssignmentPattern(assign) => {
            emit_default_for_destructured_leaf(builder, ctx, Some(&assign.right))?;
            lower_switch_assign_target(builder, ctx, &assign.left, mode)
        }
    }
}

fn lower_switch_binding_identifier_from_acc<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    ident: &'a BindingIdentifier<'a>,
    mode: SwitchBindingMode,
) -> Result<(), SourceLoweringError> {
    let reg = resolve_switch_binding_target(ctx, ident, mode)?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(reg))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (switch lexical leaf): {err:?}"))
        })?;
    finish_switch_binding_write(ctx, ident, mode)
}

fn lower_switch_array_pattern<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    pat: &'a oxc_ast::ast::ArrayPattern<'a>,
    src_reg: RegisterIndex,
    mode: SwitchBindingMode,
) -> Result<(), SourceLoweringError> {
    for (index, element) in pat.elements.iter().enumerate() {
        let Some(element_pat) = element.as_ref() else {
            continue;
        };
        let idx_i32 = i32::try_from(index)
            .map_err(|_| SourceLoweringError::Internal("array pattern index overflow".into()))?;
        builder
            .emit(Opcode::LdaSmi, &[Operand::Imm(idx_i32)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode LdaSmi (switch array pattern): {err:?}"
                ))
            })?;
        builder
            .emit(
                Opcode::LdaKeyedProperty,
                &[Operand::Reg(u32::from(src_reg))],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode LdaKeyedProperty (switch array pattern): {err:?}"
                ))
            })?;
        match element_pat {
            BindingPattern::AssignmentPattern(assign) => {
                emit_default_for_destructured_leaf(builder, ctx, Some(&assign.right))?;
                lower_switch_assign_target(builder, ctx, &assign.left, mode)?;
            }
            BindingPattern::ArrayPattern(_) | BindingPattern::ObjectPattern(_) => {
                let nested_slot = ctx.allocate_anonymous_local()?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(nested_slot))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (switch array nested): {err:?}"
                        ))
                    })?;
                lower_switch_pattern_bind_existing(builder, ctx, element_pat, nested_slot, mode)?;
            }
            _ => {
                lower_switch_pattern_bind_existing(builder, ctx, element_pat, src_reg, mode)?;
            }
        }
    }

    if let Some(rest) = pat.rest.as_deref() {
        match &rest.argument {
            BindingPattern::BindingIdentifier(ident) => {
                let reg = resolve_switch_binding_target(ctx, ident, mode)?;
                emit_array_rest_slice(builder, ctx, src_reg, pat.elements.len(), reg)?;
                finish_switch_binding_write(ctx, ident, mode)?;
            }
            nested @ (BindingPattern::ArrayPattern(_) | BindingPattern::ObjectPattern(_)) => {
                let rest_slot = ctx.allocate_anonymous_local()?;
                emit_array_rest_slice(builder, ctx, src_reg, pat.elements.len(), rest_slot)?;
                lower_switch_pattern_bind_existing(builder, ctx, nested, rest_slot, mode)?;
            }
            _ => {
                return Err(SourceLoweringError::unsupported(
                    "parser_recovery_array_rest_assignment",
                    rest.span,
                ));
            }
        }
    }
    Ok(())
}

fn lower_switch_assign_target<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    target: &'a BindingPattern<'a>,
    mode: SwitchBindingMode,
) -> Result<(), SourceLoweringError> {
    match target {
        BindingPattern::BindingIdentifier(ident) => {
            lower_switch_binding_identifier_from_acc(builder, ctx, ident, mode)
        }
        BindingPattern::ArrayPattern(_) | BindingPattern::ObjectPattern(_) => {
            let nested_slot = ctx.allocate_anonymous_local()?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(nested_slot))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (switch assign-target nested): {err:?}"
                    ))
                })?;
            lower_switch_pattern_bind_existing(builder, ctx, target, nested_slot, mode)
        }
        BindingPattern::AssignmentPattern(nested) => {
            emit_default_for_destructured_leaf(builder, ctx, Some(&nested.right))?;
            lower_switch_assign_target(builder, ctx, &nested.left, mode)
        }
    }
}

fn lower_switch_object_pattern<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    pat: &'a oxc_ast::ast::ObjectPattern<'a>,
    src_reg: RegisterIndex,
    mode: SwitchBindingMode,
) -> Result<(), SourceLoweringError> {
    let mut extracted_keys: Vec<String> = Vec::new();
    for prop in &pat.properties {
        let (computed_key_temp, key_name_for_rest, static_key_idx) = if prop.computed {
            let temp = ctx.acquire_temps(1)?;
            let key_expr = prop.key.to_expression();
            lower_return_expression(builder, ctx, key_expr)?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (switch object computed key): {err:?}"
                    ))
                })?;
            (Some(temp), None, None)
        } else {
            let key_name = match &prop.key {
                PropertyKey::StaticIdentifier(ident) => ident.name.as_str().to_owned(),
                PropertyKey::StringLiteral(lit) => lit.value.as_str().to_owned(),
                other => {
                    return Err(SourceLoweringError::unsupported(
                        property_key_tag(other),
                        other.span(),
                    ));
                }
            };
            let idx = ctx.intern_property_name(&key_name)?;
            extracted_keys.push(key_name.clone());
            (None, Some(key_name), Some(idx))
        };

        if let Some(temp) = computed_key_temp {
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Ldar (switch object computed key): {err:?}"
                    ))
                })?;
            builder
                .emit(
                    Opcode::LdaKeyedProperty,
                    &[Operand::Reg(u32::from(src_reg))],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaKeyedProperty (switch object pattern): {err:?}"
                    ))
                })?;
            ctx.release_temps(1);
        } else if let Some(idx) = static_key_idx {
            builder
                .emit(
                    Opcode::LdaNamedProperty,
                    &[Operand::Reg(u32::from(src_reg)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaNamedProperty (switch object pattern): {err:?}"
                    ))
                })?;
        }

        match &prop.value {
            BindingPattern::AssignmentPattern(assign) => {
                emit_default_for_destructured_leaf(builder, ctx, Some(&assign.right))?;
                lower_switch_assign_target(builder, ctx, &assign.left, mode)?;
            }
            BindingPattern::ArrayPattern(_) | BindingPattern::ObjectPattern(_) => {
                let nested_slot = ctx.allocate_anonymous_local()?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(nested_slot))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (switch object nested): {err:?}"
                        ))
                    })?;
                lower_switch_pattern_bind_existing(builder, ctx, &prop.value, nested_slot, mode)?;
            }
            BindingPattern::BindingIdentifier(ident) => {
                lower_switch_binding_identifier_from_acc(builder, ctx, ident, mode)?;
            }
        }
        let _ = key_name_for_rest;
    }

    if let Some(rest) = pat.rest.as_deref() {
        let BindingPattern::BindingIdentifier(rest_ident) = &rest.argument else {
            return Err(SourceLoweringError::unsupported(
                "parser_recovery_object_rest_pattern",
                rest.span,
            ));
        };
        let reg = resolve_switch_binding_target(ctx, rest_ident, mode)?;
        let excluded_base = if extracted_keys.is_empty() {
            None
        } else {
            let count = RegisterIndex::try_from(extracted_keys.len()).map_err(|_| {
                SourceLoweringError::Internal("object rest exclusion count overflow".into())
            })?;
            let base = ctx.acquire_temps(count)?;
            for (offset, key) in extracted_keys.iter().enumerate() {
                emit_string_literal_to_register(builder, ctx, key, base + offset as RegisterIndex)?;
            }
            Some((base, extracted_keys.len(), count))
        };
        emit_object_rest_copy(
            builder,
            src_reg,
            excluded_base.map(|(base, len, _)| (base, len)),
            reg,
        )?;
        if let Some((_, _, count)) = excluded_base {
            ctx.release_temps(count);
        }
        finish_switch_binding_write(ctx, rest_ident, mode)?;
    }
    Ok(())
}

fn resolve_switch_binding_target(
    ctx: &LoweringContext<'_>,
    ident: &BindingIdentifier<'_>,
    mode: SwitchBindingMode,
) -> Result<RegisterIndex, SourceLoweringError> {
    let binding = ctx
        .resolve_own(ident.name.as_str())
        .ok_or_else(|| SourceLoweringError::Internal("missing switch binding".into()))?;
    match (mode, binding) {
        (
            SwitchBindingMode::Lexical { is_const },
            BindingRef::Local {
                reg,
                initialized: false,
                is_const: binding_const,
                runtime_tdz: true,
            },
        ) if binding_const == is_const => Ok(reg),
        (
            SwitchBindingMode::Var,
            BindingRef::Local {
                reg,
                initialized: true,
                is_const: false,
                runtime_tdz: false,
            },
        ) => Ok(reg),
        (SwitchBindingMode::Lexical { .. }, _) => Err(SourceLoweringError::Internal(format!(
            "switch binding {} is not pending lexical local",
            ident.name.as_str()
        ))),
        (SwitchBindingMode::Var, _) => Err(SourceLoweringError::Internal(format!(
            "switch var binding {} is not initialized mutable local",
            ident.name.as_str()
        ))),
    }
}

fn finish_switch_binding_write(
    ctx: &mut LoweringContext<'_>,
    ident: &BindingIdentifier<'_>,
    mode: SwitchBindingMode,
) -> Result<(), SourceLoweringError> {
    match mode {
        SwitchBindingMode::Lexical { .. } => ctx.mark_initialized(ident.name.as_str()),
        SwitchBindingMode::Var => Ok(()),
    }
}
