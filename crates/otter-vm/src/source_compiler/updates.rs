use super::assignment_targets::{SimpleAssignmentTargetRef, unwrap_simple_assignment_target};
use super::*;

/// Lowers `++x`, `x++`, `--x`, and `x--`.
///
/// Identifier targets keep the existing register update path.
/// Member targets evaluate the reference once, read the current
/// property/private value, apply `Inc`/`Dec`, and store the updated
/// value back through the same reference. Postfix preserves the old
/// value in a temp and reloads it after the store.
pub(super) fn lower_update_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &UpdateExpression<'_>,
) -> Result<(), SourceLoweringError> {
    match unwrap_simple_assignment_target(&expr.argument)? {
        SimpleAssignmentTargetRef::Identifier(ident) => {
            lower_identifier_update(builder, ctx, expr, ident)
        }
        SimpleAssignmentTargetRef::StaticMember(member) => {
            lower_static_member_update(builder, ctx, expr, member)
        }
        SimpleAssignmentTargetRef::ComputedMember(member) => {
            lower_computed_member_update(builder, ctx, expr, member)
        }
        SimpleAssignmentTargetRef::PrivateField(member) => {
            lower_private_field_update(builder, ctx, expr, member)
        }
    }
}

fn lower_identifier_update(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &UpdateExpression<'_>,
    ident: &IdentifierReference<'_>,
) -> Result<(), SourceLoweringError> {
    let binding = ctx
        .resolve_identifier(ident.name.as_str())
        .ok_or_else(|| SourceLoweringError::unsupported("unbound_identifier", ident.span))?;
    enum Target {
        Reg(u16),
        Upvalue(u16),
    }
    let target = match binding {
        BindingRef::Local {
            reg,
            initialized: true,
            is_const: false,
            ..
        } => Target::Reg(reg),
        BindingRef::Local { is_const: true, .. } => {
            return Err(SourceLoweringError::unsupported("const_update", ident.span));
        }
        BindingRef::Local {
            initialized: false, ..
        } => {
            return Err(SourceLoweringError::unsupported(
                "tdz_self_reference",
                ident.span,
            ));
        }
        BindingRef::Param { reg } => Target::Reg(reg),
        BindingRef::Upvalue {
            idx,
            is_const: false,
        } => Target::Upvalue(idx),
        BindingRef::Upvalue { is_const: true, .. } => {
            return Err(SourceLoweringError::unsupported("const_update", ident.span));
        }
    };

    lower_identifier_read(builder, ctx, binding, ident.span)?;
    apply_update_to_loaded_value(builder, ctx, expr, |builder| match target {
        Target::Reg(target_reg) => {
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(target_reg))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (identifier update): {err:?}"
                    ))
                })?;
            Ok(())
        }
        Target::Upvalue(idx) => {
            builder
                .emit(Opcode::StaUpvalue, &[Operand::Idx(u32::from(idx))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode StaUpvalue (identifier update): {err:?}"
                    ))
                })?;
            Ok(())
        }
    })
}

fn lower_static_member_update(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &UpdateExpression<'_>,
    member: &StaticMemberExpression<'_>,
) -> Result<(), SourceLoweringError> {
    if member.optional {
        return Err(SourceLoweringError::unsupported(
            "parser_recovery_optional_member_assignment",
            member.span,
        ));
    }
    let idx = ctx.intern_property_name(member.property.name.as_str())?;
    if matches!(&member.object, Expression::Super(_)) {
        enforce_super_property_binding(ctx, &member.object)?;
        let receiver_temp = ctx.acquire_temps(1)?;
        let lower = (|| -> Result<(), SourceLoweringError> {
            builder.emit(Opcode::LdaThis, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaThis (super update): {err:?}"))
            })?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (super update receiver): {err:?}"
                    ))
                })?;
            builder
                .emit(
                    Opcode::GetSuperProperty,
                    &[Operand::Reg(u32::from(receiver_temp)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode GetSuperProperty (update): {err:?}"
                    ))
                })?;
            apply_update_to_loaded_value(builder, ctx, expr, |builder| {
                builder
                    .emit(
                        Opcode::SetSuperProperty,
                        &[Operand::Reg(u32::from(receiver_temp)), Operand::Idx(idx)],
                    )
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode SetSuperProperty (update): {err:?}"
                        ))
                    })?;
                Ok(())
            })
        })();
        ctx.release_temps(1);
        return lower;
    }

    let base = materialize_member_base(builder, ctx, &member.object)?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        builder
            .emit(
                Opcode::LdaNamedProperty,
                &[Operand::Reg(u32::from(base.reg)), Operand::Idx(idx)],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaNamedProperty (update): {err:?}"))
            })?;
        apply_update_to_loaded_value(builder, ctx, expr, |builder| {
            builder
                .emit(
                    Opcode::StaNamedProperty,
                    &[Operand::Reg(u32::from(base.reg)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode StaNamedProperty (update): {err:?}"
                    ))
                })?;
            Ok(())
        })
    })();
    if base.temp_count != 0 {
        ctx.release_temps(base.temp_count);
    }
    lower
}

fn lower_computed_member_update(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &UpdateExpression<'_>,
    member: &ComputedMemberExpression<'_>,
) -> Result<(), SourceLoweringError> {
    if member.optional {
        return Err(SourceLoweringError::unsupported(
            "parser_recovery_optional_member_assignment",
            member.span,
        ));
    }
    if matches!(&member.object, Expression::Super(_)) {
        return lower_super_computed_member_update(builder, ctx, expr, member);
    }

    let base = materialize_member_base(builder, ctx, &member.object)?;
    let key_temp = ctx
        .acquire_temps(1)
        .inspect_err(|_| ctx.release_temps(base.temp_count))?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        lower_return_expression(builder, ctx, &member.expression)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(key_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (update key): {err:?}"))
            })?;
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(key_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (update key): {err:?}"))
            })?;
        builder
            .emit(
                Opcode::LdaKeyedProperty,
                &[Operand::Reg(u32::from(base.reg))],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaKeyedProperty (update): {err:?}"))
            })?;
        apply_update_to_loaded_value(builder, ctx, expr, |builder| {
            builder
                .emit(
                    Opcode::StaKeyedProperty,
                    &[
                        Operand::Reg(u32::from(base.reg)),
                        Operand::Reg(u32::from(key_temp)),
                    ],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode StaKeyedProperty (update): {err:?}"
                    ))
                })?;
            Ok(())
        })
    })();
    ctx.release_temps(1);
    if base.temp_count != 0 {
        ctx.release_temps(base.temp_count);
    }
    lower
}

fn lower_super_computed_member_update(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &UpdateExpression<'_>,
    member: &ComputedMemberExpression<'_>,
) -> Result<(), SourceLoweringError> {
    enforce_super_property_binding(ctx, &member.object)?;
    let receiver_temp = ctx.acquire_temps(1)?;
    let key_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        builder.emit(Opcode::LdaThis, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!(
                "encode LdaThis (super computed update): {err:?}"
            ))
        })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (super computed update receiver): {err:?}"
                ))
            })?;
        lower_return_expression(builder, ctx, &member.expression)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(key_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (super computed update key): {err:?}"
                ))
            })?;
        builder
            .emit(
                Opcode::GetSuperPropertyComputed,
                &[
                    Operand::Reg(u32::from(receiver_temp)),
                    Operand::Reg(u32::from(key_temp)),
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode GetSuperPropertyComputed (update): {err:?}"
                ))
            })?;
        apply_update_to_loaded_value(builder, ctx, expr, |builder| {
            builder
                .emit(
                    Opcode::SetSuperPropertyComputed,
                    &[
                        Operand::Reg(u32::from(receiver_temp)),
                        Operand::Reg(u32::from(key_temp)),
                    ],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode SetSuperPropertyComputed (update): {err:?}"
                    ))
                })?;
            Ok(())
        })
    })();
    ctx.release_temps(2);
    lower
}

fn lower_private_field_update(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &UpdateExpression<'_>,
    member: &oxc_ast::ast::PrivateFieldExpression<'_>,
) -> Result<(), SourceLoweringError> {
    if member.optional {
        return Err(SourceLoweringError::unsupported(
            "parser_recovery_optional_member_assignment",
            member.span,
        ));
    }
    let name = member.field.name.as_str();
    enforce_private_name_declared(ctx, name, member.span)?;
    let base = materialize_member_base(builder, ctx, &member.object)?;
    let idx = ctx.intern_property_name(name)?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        builder
            .emit(
                Opcode::GetPrivateField,
                &[Operand::Reg(u32::from(base.reg)), Operand::Idx(idx)],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode GetPrivateField (update): {err:?}"))
            })?;
        apply_update_to_loaded_value(builder, ctx, expr, |builder| {
            builder
                .emit(
                    Opcode::SetPrivateField,
                    &[Operand::Reg(u32::from(base.reg)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode SetPrivateField (update): {err:?}"
                    ))
                })?;
            Ok(())
        })
    })();
    if base.temp_count != 0 {
        ctx.release_temps(base.temp_count);
    }
    lower
}

fn apply_update_to_loaded_value<Store>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &UpdateExpression<'_>,
    store_new_value: Store,
) -> Result<(), SourceLoweringError>
where
    Store: FnOnce(&mut BytecodeBuilder) -> Result<(), SourceLoweringError>,
{
    let op_opcode = match expr.operator {
        UpdateOperator::Increment => Opcode::Inc,
        UpdateOperator::Decrement => Opcode::Dec,
    };
    let op_label = match expr.operator {
        UpdateOperator::Increment => "Inc",
        UpdateOperator::Decrement => "Dec",
    };

    if expr.prefix {
        builder
            .emit(op_opcode, &[])
            .map_err(|err| SourceLoweringError::Internal(format!("encode {op_label}: {err:?}")))?;
        store_new_value(builder)?;
        return Ok(());
    }

    let old_temp = ctx.acquire_temps(1)?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(old_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (postfix old-value spill): {err:?}"
                ))
            })?;
        builder
            .emit(op_opcode, &[])
            .map_err(|err| SourceLoweringError::Internal(format!("encode {op_label}: {err:?}")))?;
        store_new_value(builder)?;
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(old_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (postfix old reload): {err:?}"))
            })?;
        Ok(())
    })();
    ctx.release_temps(1);
    lower
}
