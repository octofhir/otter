use super::*;

/// §13.3.9 optional call (`callee?.(args)`) — evaluate the callee,
/// short-circuit if nullish, otherwise call it. The `this`
/// binding follows the callee shape:
///
/// - Identifier / arbitrary expression → `this = undefined`
///   (`CallUndefinedReceiver`).
/// - Static or computed member (`o.m?.()`, `o[k]?.()`) → `this`
///   binds to the member's base object (`CallProperty`). The
///   receiver is spilled into a temp before the callee load so the
///   same value is used for both the `[[Get]]` receiver and the
///   call's `this`.
/// - Super member (`super.m?.()`, `super[k]?.()`) → callee comes
///   from `GetSuperProperty*`, while `this` remains the current
///   receiver.
///
/// Spread arguments (`f?.(...xs)`) are supported through the same
/// `CallSpread` tail the non-optional call paths use. Optional
/// `super?.(...)` stays rejected because direct optional super calls
/// are not valid ECMAScript.
pub(super) fn lower_optional_call<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    call: &oxc_ast::ast::CallExpression<'a>,
    short_circuit: Label,
) -> Result<(), SourceLoweringError> {
    let inner_callee = match &call.callee {
        Expression::ParenthesizedExpression(paren) => &paren.expression,
        other => other,
    };

    let has_spread = call
        .arguments
        .iter()
        .any(|arg| matches!(arg, oxc_ast::ast::Argument::SpreadElement(_)));

    let argc = RegisterIndex::try_from(call.arguments.len())
        .map_err(|_| SourceLoweringError::Internal("call argument count exceeds u16".into()))?;

    match inner_callee {
        Expression::StaticMemberExpression(member) => {
            let receiver_temp = ctx.acquire_temps(1)?;
            let callee_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
            let args_base = if has_spread {
                ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(2))?
            } else if argc == 0 {
                0
            } else {
                ctx.acquire_temps(argc)
                    .inspect_err(|_| ctx.release_temps(2))?
            };
            let super_method = matches!(&member.object, Expression::Super(_));
            let result = (|| -> Result<(), SourceLoweringError> {
                let idx = ctx.intern_property_name(member.property.name.as_str())?;
                if super_method {
                    enforce_super_property_binding(ctx, &member.object)?;
                    builder.emit(Opcode::LdaThis, &[]).map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode LdaThis (optional super call): {err:?}"
                        ))
                    })?;
                    builder
                        .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode Star (optional super receiver): {err:?}"
                            ))
                        })?;
                    builder
                        .emit(
                            Opcode::GetSuperProperty,
                            &[Operand::Reg(u32::from(receiver_temp)), Operand::Idx(idx)],
                        )
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode GetSuperProperty (optional call): {err:?}"
                            ))
                        })?;
                } else {
                    lower_return_expression(builder, ctx, &member.object)?;
                    builder
                        .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode Star (optional call receiver): {err:?}"
                            ))
                        })?;
                    if member.optional {
                        emit_optional_nullish_short_circuit(builder, receiver_temp, short_circuit)?;
                    }
                    let pc = builder
                        .emit(
                            Opcode::LdaNamedProperty,
                            &[Operand::Reg(u32::from(receiver_temp)), Operand::Idx(idx)],
                        )
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode LdaNamedProperty (optional call): {err:?}"
                            ))
                        })?;
                    let slot = ctx.allocate_property_feedback();
                    builder.attach_feedback(pc, slot);
                }
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(callee_temp))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (optional callee): {err:?}"
                        ))
                    })?;
                emit_optional_nullish_short_circuit(builder, callee_temp, short_circuit)?;
                emit_call_args_and_invoke(
                    builder,
                    ctx,
                    call,
                    callee_temp,
                    receiver_temp,
                    args_base,
                    has_spread,
                )?;
                Ok(())
            })();
            if has_spread {
                ctx.release_temps(1);
            } else if argc > 0 {
                ctx.release_temps(argc);
            }
            ctx.release_temps(2);
            return result;
        }
        Expression::ComputedMemberExpression(member) => {
            let receiver_temp = ctx.acquire_temps(1)?;
            let key_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
            let callee_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(2))?;
            let args_base = if has_spread {
                ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(3))?
            } else if argc == 0 {
                0
            } else {
                ctx.acquire_temps(argc)
                    .inspect_err(|_| ctx.release_temps(3))?
            };
            let super_method = matches!(&member.object, Expression::Super(_));
            let result = (|| -> Result<(), SourceLoweringError> {
                if super_method {
                    enforce_super_property_binding(ctx, &member.object)?;
                    builder.emit(Opcode::LdaThis, &[]).map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode LdaThis (optional super call[k]): {err:?}"
                        ))
                    })?;
                    builder
                        .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode Star (optional super receiver[k]): {err:?}"
                            ))
                        })?;
                } else {
                    lower_return_expression(builder, ctx, &member.object)?;
                    builder
                        .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode Star (optional call receiver[k]): {err:?}"
                            ))
                        })?;
                    if member.optional {
                        emit_optional_nullish_short_circuit(builder, receiver_temp, short_circuit)?;
                    }
                }
                lower_return_expression(builder, ctx, &member.expression)?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(key_temp))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (optional call key): {err:?}"
                        ))
                    })?;
                builder
                    .emit(Opcode::Ldar, &[Operand::Reg(u32::from(key_temp))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Ldar (optional call key): {err:?}"
                        ))
                    })?;
                if super_method {
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
                                "encode GetSuperPropertyComputed (optional call): {err:?}"
                            ))
                        })?;
                } else {
                    builder
                        .emit(
                            Opcode::LdaKeyedProperty,
                            &[Operand::Reg(u32::from(receiver_temp))],
                        )
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode LdaKeyedProperty (optional call): {err:?}"
                            ))
                        })?;
                }
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(callee_temp))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (optional callee[k]): {err:?}"
                        ))
                    })?;
                emit_optional_nullish_short_circuit(builder, callee_temp, short_circuit)?;
                emit_call_args_and_invoke(
                    builder,
                    ctx,
                    call,
                    callee_temp,
                    receiver_temp,
                    args_base,
                    has_spread,
                )?;
                Ok(())
            })();
            if has_spread {
                ctx.release_temps(1);
            } else if argc > 0 {
                ctx.release_temps(argc);
            }
            ctx.release_temps(3);
            return result;
        }
        Expression::Super(super_tok) => {
            return Err(SourceLoweringError::unsupported(
                "optional_super_call",
                super_tok.span,
            ));
        }
        _ => {}
    }

    let callee_temp = ctx.acquire_temps(1)?;
    if has_spread {
        let receiver_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
        let args_base = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(2))?;
        let result = (|| -> Result<(), SourceLoweringError> {
            lower_return_expression(builder, ctx, inner_callee)?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(callee_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (optional direct callee): {err:?}"
                    ))
                })?;
            emit_optional_nullish_short_circuit(builder, callee_temp, short_circuit)?;
            builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode LdaUndefined (optional spread recv): {err:?}"
                ))
            })?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (optional spread recv): {err:?}"
                    ))
                })?;
            emit_spread_call_arguments_array(builder, ctx, call, args_base)?;
            builder
                .emit(
                    Opcode::CallSpread,
                    &[
                        Operand::Reg(u32::from(callee_temp)),
                        Operand::Reg(u32::from(receiver_temp)),
                        Operand::RegList {
                            base: u32::from(args_base),
                            count: 1,
                        },
                    ],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode CallSpread (optional direct): {err:?}"
                    ))
                })?;
            Ok(())
        })();
        ctx.release_temps(3);
        return result;
    }
    let args_base = if argc == 0 {
        0
    } else {
        ctx.acquire_temps(argc)
            .inspect_err(|_| ctx.release_temps(1))?
    };
    let result = (|| -> Result<(), SourceLoweringError> {
        lower_return_expression(builder, ctx, inner_callee)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(callee_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (optional direct callee): {err:?}"
                ))
            })?;
        emit_optional_nullish_short_circuit(builder, callee_temp, short_circuit)?;
        lower_call_arguments_into_temps(builder, ctx, call, args_base)?;
        builder
            .emit(
                Opcode::CallUndefinedReceiver,
                &[
                    Operand::Reg(u32::from(callee_temp)),
                    Operand::RegList {
                        base: u32::from(args_base),
                        count: u32::from(argc),
                    },
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode CallUndefinedReceiver (optional direct): {err:?}"
                ))
            })?;
        Ok(())
    })();
    if argc > 0 {
        ctx.release_temps(argc);
    }
    ctx.release_temps(1);
    result
}
