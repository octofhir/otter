use super::*;

/// Lowers direct calls whose callee is an arbitrary expression,
/// not a syntactic identifier or member reference:
///
/// - `(function (x) { ... })(arg)`
/// - `factory()(arg)`
/// - `(cond ? f : g)(arg)`
///
/// The callee expression is evaluated exactly once, before any
/// argument evaluation, then invoked with an undefined receiver per
/// `EvaluateCall` for non-reference callees. Member callees stay on
/// the `CallProperty` path in `mod.rs` so `this` binding is preserved.
pub(super) fn lower_expression_direct_call<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    call: &'a oxc_ast::ast::CallExpression<'a>,
    callee: &'a Expression<'a>,
    has_spread: bool,
) -> Result<(), SourceLoweringError> {
    if has_spread {
        return lower_expression_direct_call_with_spread(builder, ctx, call, callee);
    }

    let argc = RegisterIndex::try_from(call.arguments.len())
        .map_err(|_| SourceLoweringError::Internal("call argument count exceeds u16".into()))?;
    let callee_temp = ctx.acquire_temps(1)?;
    let args_base = if argc == 0 {
        0
    } else {
        ctx.acquire_temps(argc)
            .inspect_err(|_| ctx.release_temps(1))?
    };

    let lower = (|| -> Result<(), SourceLoweringError> {
        lower_return_expression(builder, ctx, callee)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(callee_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (expression direct callee): {err:?}"
                ))
            })?;
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
                    "encode CallUndefinedReceiver (expression direct): {err:?}"
                ))
            })?;
        Ok(())
    })();

    if argc > 0 {
        ctx.release_temps(argc);
    }
    ctx.release_temps(1);
    lower
}

fn lower_expression_direct_call_with_spread<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    call: &'a oxc_ast::ast::CallExpression<'a>,
    callee: &'a Expression<'a>,
) -> Result<(), SourceLoweringError> {
    let callee_temp = ctx.acquire_temps(1)?;
    let receiver_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
    let args_base = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(2))?;

    let lower = (|| -> Result<(), SourceLoweringError> {
        lower_return_expression(builder, ctx, callee)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(callee_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (spread expression callee): {err:?}"
                ))
            })?;
        builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!(
                "encode LdaUndefined (spread expression recv): {err:?}"
            ))
        })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (spread expression recv): {err:?}"
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
                    "encode CallSpread (expression direct): {err:?}"
                ))
            })?;
        Ok(())
    })();

    ctx.release_temps(3);
    lower
}
