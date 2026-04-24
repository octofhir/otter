//! Binary / relational / membership expression lowering.
//!
//! Extracted from `source_compiler/mod.rs` in the file-size cleanup
//! follow-up after C4. Three entry points — `lower_binary_expression`,
//! `lower_membership_expression`, `lower_relational_expression` —
//! cover every `BinaryExpression` shape the compiler accepts. Each
//! path is responsible for its own feedback-slot allocation so the
//! interpreter's C4 observations end up on the right lattice.

use super::*;

/// Per-operator opcode pair: the Reg-RHS form and the optional
/// `*Smi imm` fast path. `Some(smi)` means the bytecode ISA carries a
/// dedicated immediate opcode for this operator; `None` means a
/// literal RHS would have to be materialised into a scratch slot.
pub(super) struct BinaryOpEncoding {
    pub(super) reg_opcode: Opcode,
    pub(super) smi_opcode: Option<Opcode>,
    /// `true` when `a OP b == b OP a` (Add/Mul/BitOr/BitAnd/BitXor).
    /// Non-commutative ops (Sub/Shl/Shr/UShr) need a second temp slot
    /// in the complex-RHS fallback to preserve operand order.
    pub(super) commutative: bool,
    /// Short label used in `SourceLoweringError::Internal` messages so
    /// encoder failures point at the right opcode without resorting to
    /// `format!("{:?}", op)`.
    pub(super) label: &'static str,
}

/// Maps a parsed binary operator to the v2 opcode pair the lowering
/// uses. Returns `None` for operators outside the M3 int32 surface
/// (comparisons, equality, exponent, division, remainder, membership);
/// callers fall back to [`binary_operator_tag`] for the diagnostic.
pub(super) fn binary_op_encoding(op: BinaryOperator) -> Option<BinaryOpEncoding> {
    use BinaryOperator::*;
    Some(match op {
        Addition => BinaryOpEncoding {
            reg_opcode: Opcode::Add,
            smi_opcode: Some(Opcode::AddSmi),
            // M15: JS `+` is non-commutative on strings (`"a" + "b"`
            // ≠ `"b" + "a"`) even though int32 addition is. The
            // complex-RHS fallback must preserve LHS/RHS ordering so
            // string concat composes correctly, so the encoding
            // advertises `commutative: false` and takes the 2-temp
            // path. Int32 `a + b` stays correct because it's
            // genuinely commutative; the only cost is one extra temp
            // slot on nested-binary RHS shapes that rarely appear in
            // hot loops.
            commutative: false,
            label: "Add",
        },
        Subtraction => BinaryOpEncoding {
            reg_opcode: Opcode::Sub,
            smi_opcode: Some(Opcode::SubSmi),
            commutative: false,
            label: "Sub",
        },
        Multiplication => BinaryOpEncoding {
            reg_opcode: Opcode::Mul,
            smi_opcode: Some(Opcode::MulSmi),
            commutative: true,
            label: "Mul",
        },
        BitwiseOR => BinaryOpEncoding {
            reg_opcode: Opcode::BitwiseOr,
            smi_opcode: Some(Opcode::BitwiseOrSmi),
            commutative: true,
            label: "BitwiseOr",
        },
        BitwiseAnd => BinaryOpEncoding {
            reg_opcode: Opcode::BitwiseAnd,
            smi_opcode: Some(Opcode::BitwiseAndSmi),
            commutative: true,
            label: "BitwiseAnd",
        },
        BitwiseXOR => BinaryOpEncoding {
            reg_opcode: Opcode::BitwiseXor,
            smi_opcode: None,
            commutative: true,
            label: "BitwiseXor",
        },
        ShiftLeft => BinaryOpEncoding {
            reg_opcode: Opcode::Shl,
            smi_opcode: Some(Opcode::ShlSmi),
            commutative: false,
            label: "Shl",
        },
        ShiftRight => BinaryOpEncoding {
            reg_opcode: Opcode::Shr,
            smi_opcode: Some(Opcode::ShrSmi),
            commutative: false,
            label: "Shr",
        },
        ShiftRightZeroFill => BinaryOpEncoding {
            reg_opcode: Opcode::UShr,
            smi_opcode: None,
            commutative: false,
            label: "UShr",
        },
        Division => BinaryOpEncoding {
            reg_opcode: Opcode::Div,
            smi_opcode: None,
            commutative: false,
            label: "Div",
        },
        Remainder => BinaryOpEncoding {
            reg_opcode: Opcode::Mod,
            smi_opcode: None,
            commutative: false,
            label: "Mod",
        },
        Exponential => BinaryOpEncoding {
            reg_opcode: Opcode::Exp,
            smi_opcode: None,
            commutative: false,
            label: "Exp",
        },
        _ => return None,
    })
}

/// Lowers `lhs <op> rhs` where `<op>` is one of the M3 int32 binary
/// operators and both operands are int32-safe. Picks the `*Smi imm`
/// fast path whenever the RHS is a literal that fits in `i8` and the
/// operator has a dedicated Smi opcode; falls back to the Reg form
/// otherwise. Operators with no Smi opcode (`^`, `>>>`) reject a
/// literal RHS until a future milestone introduces locals to hold it.
pub(super) fn lower_binary_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &BinaryExpression<'_>,
) -> Result<(), SourceLoweringError> {
    if let Some(encoding) = binary_op_encoding(expr.operator) {
        // LHS must evaluate into the accumulator. Only identifier /
        // int32-safe literal / parenthesised variants of those are
        // allowed — nested binary expressions require a scratch slot
        // we don't allocate yet.
        lower_accumulator_operand(builder, ctx, &expr.left)?;
        return apply_binary_op_with_acc_lhs(builder, ctx, &encoding, &expr.right);
    }
    if matches!(
        expr.operator,
        BinaryOperator::In | BinaryOperator::Instanceof
    ) {
        return lower_membership_expression(builder, ctx, expr);
    }
    if let Some(rel_encoding) = relational_op_encoding(expr.operator) {
        return lower_relational_expression(builder, ctx, expr, rel_encoding);
    }
    Err(SourceLoweringError::unsupported(
        binary_operator_tag(expr.operator),
        expr.span,
    ))
}

fn lower_membership_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &BinaryExpression<'_>,
) -> Result<(), SourceLoweringError> {
    let lhs_temp = ctx.acquire_temps(1)?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        lower_return_expression(builder, ctx, &expr.left)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(lhs_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (membership LHS): {err:?}"))
            })?;
        lower_return_expression(builder, ctx, &expr.right)?;
        let opcode = match expr.operator {
            BinaryOperator::In => Opcode::TestIn,
            BinaryOperator::Instanceof => Opcode::TestInstanceOf,
            _ => unreachable!("caller filters membership operators"),
        };
        let pc = builder
            .emit(opcode, &[Operand::Reg(u32::from(lhs_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode membership test: {err:?}"))
            })?;
        ctx.attach_comparison_feedback(builder, pc);
        Ok(())
    })();
    ctx.release_temps(1);
    lower
}

/// Per-operator opcode pair for the M6 relational operators. The
/// dispatcher's `Test*` opcodes all read `acc` as the LHS and a
/// register as the RHS; literal RHS would need a scratch slot which
/// the M6 frame layout does not yet provide. Instead, the lowering
/// **swaps operands** for the `identifier <op> literal` shape — `n <
/// 5` lowers as `LdaSmi 5; TestGreaterThan r_n`, which evaluates
/// `5 > n` and is equivalent to `n < 5`. `swapped_op` carries the
/// inverted-direction opcode for that swap; for symmetric operators
/// (`===`, `!==`) it equals `forward_op`.
struct RelationalOpEncoding {
    forward_op: Opcode,
    swapped_op: Opcode,
    /// `true` for `!==` only — the lowering follows up the
    /// `TestEqualStrict` with a `LogicalNot` so the accumulator
    /// carries the negated boolean.
    requires_inversion: bool,
    label: &'static str,
}

fn relational_op_encoding(op: BinaryOperator) -> Option<RelationalOpEncoding> {
    use BinaryOperator::*;
    Some(match op {
        LessThan => RelationalOpEncoding {
            forward_op: Opcode::TestLessThan,
            swapped_op: Opcode::TestGreaterThan,
            requires_inversion: false,
            label: "TestLessThan",
        },
        GreaterThan => RelationalOpEncoding {
            forward_op: Opcode::TestGreaterThan,
            swapped_op: Opcode::TestLessThan,
            requires_inversion: false,
            label: "TestGreaterThan",
        },
        LessEqualThan => RelationalOpEncoding {
            forward_op: Opcode::TestLessThanOrEqual,
            swapped_op: Opcode::TestGreaterThanOrEqual,
            requires_inversion: false,
            label: "TestLessThanOrEqual",
        },
        GreaterEqualThan => RelationalOpEncoding {
            forward_op: Opcode::TestGreaterThanOrEqual,
            swapped_op: Opcode::TestLessThanOrEqual,
            requires_inversion: false,
            label: "TestGreaterThanOrEqual",
        },
        Equality => RelationalOpEncoding {
            forward_op: Opcode::TestEqual,
            swapped_op: Opcode::TestEqual,
            requires_inversion: false,
            label: "TestEqual",
        },
        Inequality => RelationalOpEncoding {
            forward_op: Opcode::TestEqual,
            swapped_op: Opcode::TestEqual,
            requires_inversion: true,
            label: "TestEqual",
        },
        StrictEquality => RelationalOpEncoding {
            forward_op: Opcode::TestEqualStrict,
            swapped_op: Opcode::TestEqualStrict,
            requires_inversion: false,
            label: "TestEqualStrict",
        },
        StrictInequality => RelationalOpEncoding {
            forward_op: Opcode::TestEqualStrict,
            swapped_op: Opcode::TestEqualStrict,
            requires_inversion: true,
            label: "TestEqualStrict",
        },
        _ => return None,
    })
}

/// Lowers a relational binary expression. The dispatcher's `Test*`
/// opcodes encode `acc <op> reg`, so one operand must reach a
/// register and the other must reach the accumulator. Literal-on-RHS
/// patterns auto-swap to literal-on-LHS form using the `swapped_op`
/// from [`relational_op_encoding`]; two-literal comparisons reject
/// because neither side reaches a register without a scratch slot.
fn lower_relational_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &BinaryExpression<'_>,
    encoding: RelationalOpEncoding,
) -> Result<(), SourceLoweringError> {
    // Direction:
    //   Forward — LHS lowers to acc, RHS is an identifier whose slot
    //              becomes the register operand.
    //   Swap    — RHS literal lowers to acc, LHS identifier becomes
    //              the register operand. Uses `swapped_op` so the
    //              comparison direction is preserved (`n < 5` ≡
    //              `5 > n`).
    enum Direction<'a> {
        Forward {
            rhs_ident: &'a oxc_ast::ast::IdentifierReference<'a>,
        },
        Swap {
            rhs_literal: &'a NumericLiteral<'a>,
            lhs_ident: &'a oxc_ast::ast::IdentifierReference<'a>,
        },
    }

    let direction = match (&expr.left, &expr.right) {
        // identifier OP identifier — Forward
        (Expression::Identifier(_), Expression::Identifier(rhs)) => {
            Direction::Forward { rhs_ident: rhs }
        }
        // literal OP identifier — Forward
        (Expression::NumericLiteral(_), Expression::Identifier(rhs)) => {
            Direction::Forward { rhs_ident: rhs }
        }
        // identifier OP literal — Swap
        (Expression::Identifier(lhs), Expression::NumericLiteral(rhs)) => Direction::Swap {
            rhs_literal: rhs,
            lhs_ident: lhs,
        },
        // Anything else (member access, call, paren, nested
        // binary, literal-literal, two complex sides, …) takes
        // the complex-operand path: lower LHS into a temp, lower
        // RHS into acc, then emit the RHS-form comparison
        // against the temp.
        _ => {
            let lhs_temp = ctx.acquire_temps(1)?;
            let lower = (|| -> Result<(), SourceLoweringError> {
                lower_return_expression(builder, ctx, &expr.left)?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(lhs_temp))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (relational complex LHS): {err:?}"
                        ))
                    })?;
                lower_return_expression(builder, ctx, &expr.right)?;
                // Acc holds RHS; emit `Test<op>Reg <lhs>` which
                // computes `<lhs> OP <acc>`. Swap direction so
                // the original `lhs OP rhs` meaning holds:
                // `acc < lhs_temp` is `rhs < lhs`, but we want
                // `lhs < rhs`. The `swapped_op` encoding is
                // exactly `lhs OP acc` with lhs as register and
                // rhs in acc — perfect here.
                let pc = builder
                    .emit(encoding.swapped_op, &[Operand::Reg(u32::from(lhs_temp))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode {} (relational complex): {err:?}",
                            encoding.label
                        ))
                    })?;
                ctx.attach_comparison_feedback(builder, pc);
                Ok(())
            })();
            ctx.release_temps(1);
            lower?;
            if encoding.requires_inversion {
                builder.emit(Opcode::LogicalNot, &[]).map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LogicalNot (relational complex): {err:?}"
                    ))
                })?;
            }
            return Ok(());
        }
    };

    match direction {
        Direction::Forward { rhs_ident } => {
            // RHS register operand requires a user-visible
            // register — upvalues and module globals route
            // through the complex path (LHS spilled, RHS lowered
            // into acc via `LdaUpvalue` / `LdaGlobal`, then
            // `swapped_op` emitted).
            let rhs_binding = ctx.resolve_identifier(rhs_ident.name.as_str());
            let rhs_direct = matches!(
                rhs_binding,
                Some(BindingRef::Param { .. })
                    | Some(BindingRef::Local {
                        initialized: true,
                        ..
                    })
            );
            if !rhs_direct {
                let lhs_temp = ctx.acquire_temps(1)?;
                let lower = (|| -> Result<(), SourceLoweringError> {
                    lower_return_expression(builder, ctx, &expr.left)?;
                    builder
                        .emit(Opcode::Star, &[Operand::Reg(u32::from(lhs_temp))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode Star (relational global LHS): {err:?}"
                            ))
                        })?;
                    lower_return_expression(builder, ctx, &expr.right)?;
                    let pc = builder
                        .emit(encoding.swapped_op, &[Operand::Reg(u32::from(lhs_temp))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode {} (relational global): {err:?}",
                                encoding.label
                            ))
                        })?;
                    ctx.attach_comparison_feedback(builder, pc);
                    Ok(())
                })();
                ctx.release_temps(1);
                lower?;
                if encoding.requires_inversion {
                    builder.emit(Opcode::LogicalNot, &[]).map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode LogicalNot (relational global): {err:?}"
                        ))
                    })?;
                }
                return Ok(());
            }
            lower_accumulator_operand(builder, ctx, &expr.left)?;
            let binding = rhs_binding.expect("checked Some above");
            emit_identifier_as_reg_operand(
                builder,
                ctx,
                encoding.forward_op,
                encoding.label,
                binding,
                rhs_ident.span,
            )?;
        }
        Direction::Swap {
            rhs_literal,
            lhs_ident,
        } => {
            let lhs_binding = ctx.resolve_identifier(lhs_ident.name.as_str());
            let lhs_direct = matches!(
                lhs_binding,
                Some(BindingRef::Param { .. })
                    | Some(BindingRef::Local {
                        initialized: true,
                        ..
                    })
            );
            if !lhs_direct {
                let lhs_temp = ctx.acquire_temps(1)?;
                let lower = (|| -> Result<(), SourceLoweringError> {
                    lower_return_expression(builder, ctx, &expr.left)?;
                    builder
                        .emit(Opcode::Star, &[Operand::Reg(u32::from(lhs_temp))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode Star (relational global literal LHS): {err:?}"
                            ))
                        })?;
                    let value = int32_from_literal(rhs_literal)?;
                    builder
                        .emit(Opcode::LdaSmi, &[Operand::Imm(value)])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode LdaSmi (relational global literal RHS): {err:?}"
                            ))
                        })?;
                    let pc = builder
                        .emit(encoding.swapped_op, &[Operand::Reg(u32::from(lhs_temp))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode {} (relational global literal): {err:?}",
                                encoding.label
                            ))
                        })?;
                    ctx.attach_comparison_feedback(builder, pc);
                    Ok(())
                })();
                ctx.release_temps(1);
                lower?;
                if encoding.requires_inversion {
                    builder.emit(Opcode::LogicalNot, &[]).map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode LogicalNot (relational global literal): {err:?}"
                        ))
                    })?;
                }
                return Ok(());
            }
            let value = int32_from_literal(rhs_literal)?;
            builder
                .emit(Opcode::LdaSmi, &[Operand::Imm(value)])
                .map_err(|err| SourceLoweringError::Internal(format!("encode LdaSmi: {err:?}")))?;
            let binding = lhs_binding.expect("checked Some above");
            emit_identifier_as_reg_operand(
                builder,
                ctx,
                encoding.swapped_op,
                encoding.label,
                binding,
                lhs_ident.span,
            )?;
        }
    }

    if encoding.requires_inversion {
        builder
            .emit(Opcode::LogicalNot, &[])
            .map_err(|err| SourceLoweringError::Internal(format!("encode LogicalNot: {err:?}")))?;
    }

    Ok(())
}

/// Returns the feedback kind appropriate for the given "RHS-form"
/// opcode. Arithmetic/bitwise opcodes map to
/// [`FeedbackKind::Arithmetic`]; relational `Test*` opcodes map to
/// [`FeedbackKind::Comparison`] so the interpreter can record
/// operand-type observations on the right lattice.
fn rhs_form_feedback_kind(opcode: Opcode) -> FeedbackKind {
    match opcode {
        Opcode::TestEqual
        | Opcode::TestEqualStrict
        | Opcode::TestLessThan
        | Opcode::TestGreaterThan
        | Opcode::TestLessThanOrEqual
        | Opcode::TestGreaterThanOrEqual
        | Opcode::TestInstanceOf
        | Opcode::TestIn => FeedbackKind::Comparison,
        _ => FeedbackKind::Arithmetic,
    }
}

/// Allocate a feedback slot whose kind matches the given opcode's
/// RHS-form class (see [`rhs_form_feedback_kind`]) and attach it at
/// `pc`. Used by [`emit_identifier_as_reg_operand`] so the same
/// helper can cover both arithmetic RHS loads and relational
/// `Test*` emissions.
fn attach_rhs_form_feedback(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    opcode: Opcode,
    pc: u32,
) {
    match rhs_form_feedback_kind(opcode) {
        FeedbackKind::Comparison => ctx.attach_comparison_feedback(builder, pc),
        FeedbackKind::Arithmetic => {
            let slot = ctx.allocate_arithmetic_feedback();
            builder.attach_feedback(pc, slot);
        }
        _ => unreachable!("rhs_form_feedback_kind returns only Comparison or Arithmetic"),
    }
}

/// Emits an opcode that takes an identifier-bound register as its
/// sole operand (e.g. `Add r_n`, `TestLessThan r_n`). Performs the
/// shared TDZ check on the binding so callers don't have to repeat
/// the match. Used by [`lower_identifier_as_reg_rhs`] (arithmetic
/// RHS) and [`lower_relational_expression`] (relational comparand).
///
/// Allocates a feedback slot whose kind depends on `opcode`
/// (see [`rhs_form_feedback_kind`]). Arithmetic RHS sites get an
/// `Arithmetic` slot so JIT baseline's trust-int32 elision keeps
/// working; `Test*` sites get a `Comparison` slot so downstream
/// consumers see the operand-type observations on the expected
/// lattice.
pub(super) fn emit_identifier_as_reg_operand(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    opcode: Opcode,
    label: &'static str,
    binding: BindingRef,
    ident_span: Span,
) -> Result<u32, SourceLoweringError> {
    let direct_reg = match binding {
        BindingRef::Param { reg } => Some(reg),
        BindingRef::Local {
            reg,
            initialized: true,
            runtime_tdz: false,
            ..
        } => Some(reg),
        BindingRef::Local {
            runtime_tdz: true, ..
        } => None,
        BindingRef::Local {
            initialized: false,
            runtime_tdz: false,
            ..
        } => {
            return Err(SourceLoweringError::unsupported(
                "tdz_self_reference",
                ident_span,
            ));
        }
        BindingRef::Upvalue { .. } => None,
    };
    if let Some(reg) = direct_reg {
        let pc = builder
            .emit(opcode, &[Operand::Reg(u32::from(reg))])
            .map_err(|err| SourceLoweringError::Internal(format!("encode {label}: {err:?}")))?;
        attach_rhs_form_feedback(builder, ctx, opcode, pc);
        return Ok(pc);
    }

    let lhs_temp = ctx.acquire_temps(1)?;
    let rhs_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
    let result = (|| -> Result<u32, SourceLoweringError> {
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(lhs_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star ({label} lhs temp): {err:?}"))
            })?;
        emit_load_binding_value(builder, binding, ident_span, label)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(rhs_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star ({label} rhs temp): {err:?}"))
            })?;
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(lhs_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar ({label} lhs reload): {err:?}"))
            })?;
        let pc = builder
            .emit(opcode, &[Operand::Reg(u32::from(rhs_temp))])
            .map_err(|err| SourceLoweringError::Internal(format!("encode {label}: {err:?}")))?;
        attach_rhs_form_feedback(builder, ctx, opcode, pc);
        Ok(pc)
    })();
    ctx.release_temps(2);
    result
}

/// Applies a binary operation whose LHS is already in the accumulator.
/// Picks `*Smi imm` for int32-safe literal RHS that fits `i8` (when
/// the operator carries a Smi opcode), or the Reg form for an
/// in-scope identifier RHS. Falls back to a temp-spill path for
/// "complex" RHS shapes (call, nested binary, parenthesised binary,
/// assignment) — the LHS gets spilled to a temp, the RHS is lowered
/// into acc through the standard expression path, and the result is
/// stitched back together as `acc = LHS op RHS` (commutative ops
/// reuse one temp; non-commutative ops grab a second temp to
/// preserve operand order).
///
/// Used by both [`lower_binary_expression`] and the compound-
/// assignment path in [`lower_assignment_expression`] — the
/// bytecode shape `<load lhs into acc>; <op> <rhs>` is identical.
pub(super) fn apply_binary_op_with_acc_lhs(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    encoding: &BinaryOpEncoding,
    rhs: &Expression<'_>,
) -> Result<(), SourceLoweringError> {
    match rhs {
        Expression::NumericLiteral(literal) => {
            // If the operator has a dedicated `*Smi` opcode AND
            // the literal fits `i8`, take the fast path. Otherwise
            // — no Smi opcode (`^`, `>>>`, `/`, `%`, `**`), wide
            // literal, or fractional literal — spill to the
            // generic RHS path so the value goes through a temp
            // register and the Reg-form opcode does the work.
            let fits_i8 = int32_from_literal(literal)
                .ok()
                .map(|v| (i32::from(i8::MIN)..=i32::from(i8::MAX)).contains(&v));
            if let (Some(smi_op), Some(true)) = (encoding.smi_opcode, fits_i8) {
                let value = int32_from_literal(literal)?;
                let pc = builder
                    .emit(smi_op, &[Operand::Imm(value)])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode {}Smi: {err:?}",
                            encoding.label
                        ))
                    })?;
                let slot = ctx.allocate_arithmetic_feedback();
                builder.attach_feedback(pc, slot);
                return Ok(());
            }
            apply_binary_op_with_complex_rhs(builder, ctx, encoding, rhs)
        }
        Expression::Identifier(ident) => {
            // §M35 module globals (imports, top-level exports) and
            // upvalue bindings don't live in a user-visible
            // register — both route through the complex-RHS spill
            // path so the RHS is read via `LdaGlobal` /
            // `LdaUpvalue` into acc and stitched against the
            // spilled LHS. Only params / initialised locals can
            // feed the fast `Op reg` shape.
            match ctx.resolve_identifier(ident.name.as_str()) {
                Some(binding) if !matches!(binding, BindingRef::Upvalue { .. }) => {
                    lower_identifier_as_reg_rhs(builder, ctx, encoding, binding, ident.span)
                }
                _ => apply_binary_op_with_complex_rhs(builder, ctx, encoding, rhs),
            }
        }
        // Any other RHS shape — `new T(args)`, a function / arrow /
        // class expression, a chain, a template-tag call, etc.
        // `apply_binary_op_with_complex_rhs` spills the LHS to a
        // temp and drives the RHS through `lower_return_expression`,
        // which handles every Expression variant the expression
        // lowerer knows about. The fast-path arms above stay as
        // optimisations; unrecognised shapes no longer stall the
        // whole compound assignment on a construct-tag error.
        _ => apply_binary_op_with_complex_rhs(builder, ctx, encoding, rhs),
    }
}

/// Fallback path for binary expressions whose RHS doesn't fit the
/// fast `*Smi imm` / `Op reg` shapes — typically because the RHS
/// itself contains a call, a nested binary, or an assignment.
///
/// Bytecode shape (commutative op, single temp):
///
/// ```text
///   ; LHS already in acc (lowered by caller)
///   Star r_lhs_temp      ; spill LHS so RHS can clobber acc
///   <lower RHS>          ; acc = RHS
///   Op r_lhs_temp        ; acc = RHS op LHS = LHS op RHS  (commutative)
/// ```
///
/// For non-commutative ops we need a second temp to preserve
/// operand order:
///
/// ```text
///   Star r_lhs_temp
///   <lower RHS>
///   Star r_rhs_temp
///   Ldar r_lhs_temp      ; acc = LHS
///   Op r_rhs_temp        ; acc = LHS op RHS
/// ```
fn apply_binary_op_with_complex_rhs(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    encoding: &BinaryOpEncoding,
    rhs: &Expression<'_>,
) -> Result<(), SourceLoweringError> {
    let lhs_temp = ctx.acquire_temps(1)?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(lhs_temp))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (LHS spill): {err:?}"))
        })?;

    let lower_result = lower_return_expression(builder, ctx, rhs);
    if let Err(err) = lower_result {
        ctx.release_temps(1);
        return Err(err);
    }

    if encoding.commutative {
        // acc = RHS, lhs_temp = LHS. `Op r_lhs_temp` ⇒ acc = RHS
        // op LHS, which equals LHS op RHS for commutative ops.
        let pc = builder
            .emit(encoding.reg_opcode, &[Operand::Reg(u32::from(lhs_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode {} (commutative complex RHS): {err:?}",
                    encoding.label
                ))
            })?;
        let slot = ctx.allocate_arithmetic_feedback();
        builder.attach_feedback(pc, slot);
        ctx.release_temps(1);
        Ok(())
    } else {
        // Non-commutative: order matters. Spill RHS to a second
        // temp, reload LHS into acc, then apply op against RHS.
        let rhs_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(rhs_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (RHS spill): {err:?}"))
            })?;
        let ldar_pc = builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(lhs_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (LHS reload): {err:?}"))
            })?;
        let ldar_slot = ctx.allocate_arithmetic_feedback();
        builder.attach_feedback(ldar_pc, ldar_slot);
        let pc = builder
            .emit(encoding.reg_opcode, &[Operand::Reg(u32::from(rhs_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode {} (non-commutative complex RHS): {err:?}",
                    encoding.label
                ))
            })?;
        let slot = ctx.allocate_arithmetic_feedback();
        builder.attach_feedback(pc, slot);
        // Release in LIFO order — rhs_temp was acquired last.
        ctx.release_temps(1); // rhs_temp
        ctx.release_temps(1); // lhs_temp
        Ok(())
    }
}
