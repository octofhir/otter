//! Member access (read side): static `o.x`, computed `o[k]`, and
//! the optional-chain short-circuit machinery used by both call and
//! member paths.
//!
//! Extracted from `source_compiler/mod.rs` in the file-size cleanup
//! follow-up after C4. Cross-module users include
//! `assignments` (member stores), `calls` (method-call receivers),
//! `for_in_of` (destructuring targets), `updates` (`++o.x`), and
//! `optional_calls` (short-circuit guard on the receiver).

use super::*;

/// Materialises the base object of a member expression into a
/// register that the caller can feed to `Lda*Property` /
/// `Sta*Property`. Fast path: if the base is an in-scope identifier
/// bound to a parameter or initialised local, its slot is returned
/// directly and no temp is acquired. Otherwise the base is lowered
/// into the accumulator and spilled into a freshly-acquired temp
/// slot; the caller must call `release_temps(temp_count)` in LIFO
/// order once the emitted opcode consuming the base has run.
///
/// `temp_count` is always 0 or 1 and tells the caller whether to
/// release a slot.
pub(super) struct MemberBase {
    pub(super) reg: RegisterIndex,
    pub(super) temp_count: RegisterIndex,
}

pub(super) fn materialize_member_base<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    base: &'a Expression<'a>,
) -> Result<MemberBase, SourceLoweringError> {
    if let Expression::Identifier(ident) = base
        && let Some(binding) = ctx.resolve_identifier(ident.name.as_str())
    {
        match binding {
            BindingRef::Param { reg } => return Ok(MemberBase { reg, temp_count: 0 }),
            BindingRef::Local {
                reg,
                initialized: true,
                ..
            } => return Ok(MemberBase { reg, temp_count: 0 }),
            BindingRef::Local {
                initialized: false, ..
            } => {
                return Err(SourceLoweringError::unsupported(
                    "tdz_self_reference",
                    ident.span,
                ));
            }
            // Upvalue base: no dedicated register, so fall
            // through to the complex-path below (lower into acc,
            // spill to a temp).
            BindingRef::Upvalue { .. } => {}
        }
    }

    // Complex / non-local base — lower into acc and spill to a temp.
    lower_return_expression(builder, ctx, base)?;
    let temp = ctx.acquire_temps(1)?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(temp))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (member base spill): {err:?}"))
        })?;
    Ok(MemberBase {
        reg: temp,
        temp_count: 1,
    })
}

/// Lowers `o.x` into the accumulator. Base goes through
/// [`materialize_member_base`] (direct-reg fast path for identifier
/// bases, temp-spill for everything else); the property name is
/// interned into the function's `PropertyNameTable` with dedup.
///
/// Optional chaining (`o?.x`) is handled via a nullish short-circuit
/// jump: the caller — [`lower_chain_expression`] — pushes the
/// chain's short-circuit label onto the context stack before
/// lowering the chain's inner expression. When this helper sees
/// `expr.optional == true` and finds an active short-circuit label
/// on the stack, it emits a `JumpIfNull` / `JumpIfUndefined` pair
/// against the materialised base object before the property load.
/// `o?.x` outside any chain is a parser / AST invariant violation
/// and stays rejected defensively.
///
/// §13.3.9 Optional Chains
/// <https://tc39.es/ecma262/#sec-optional-chains>
/// §13.3.2 Property Accessors
/// <https://tc39.es/ecma262/#sec-property-accessors>
pub(super) fn lower_static_member_read(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &StaticMemberExpression<'_>,
) -> Result<(), SourceLoweringError> {
    let optional_short_circuit = optional_member_short_circuit(ctx, expr.optional)?;
    // M28: `super.x` — §13.3.7 SuperReference. Uses the enclosing
    // method's `[[HomeObject]]` (resolved at runtime inside the
    // `GetSuperProperty` opcode) as the lookup base, and the
    // current frame's `this` as the `[[Get]]` receiver.
    if matches!(&expr.object, Expression::Super(_)) {
        enforce_super_property_binding(ctx, &expr.object)?;
        let receiver_temp = ctx.acquire_temps(1)?;
        let lower = (|| -> Result<(), SourceLoweringError> {
            builder.emit(Opcode::LdaThis, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaThis (super.x): {err:?}"))
            })?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (super.x receiver): {err:?}"
                    ))
                })?;
            let idx = ctx.intern_property_name(expr.property.name.as_str())?;
            builder
                .emit(
                    Opcode::GetSuperProperty,
                    &[Operand::Reg(u32::from(receiver_temp)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode GetSuperProperty: {err:?}"))
                })?;
            Ok(())
        })();
        ctx.release_temps(1);
        return lower;
    }
    let base = materialize_member_base(builder, ctx, &expr.object)?;
    if let Some(short_circuit) = optional_short_circuit {
        emit_optional_nullish_short_circuit(builder, ctx, base.reg, short_circuit)?;
    }
    let idx = ctx.intern_property_name(expr.property.name.as_str())?;
    // P1: attach a property-feedback slot so the dispatcher can
    // probe the cached `(shape_id, slot_index)` for this PC on
    // subsequent executions. On first hit the slot transitions
    // `Uninitialized → Monomorphic`; diverging shapes bump it to
    // `Polymorphic` (up to 4); beyond that it pins `Megamorphic`
    // and always takes the slow path.
    let pc = builder
        .emit(
            Opcode::LdaNamedProperty,
            &[Operand::Reg(u32::from(base.reg)), Operand::Idx(idx)],
        )
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode LdaNamedProperty: {err:?}"))
        })?;
    let slot = ctx.allocate_property_feedback();
    builder.attach_feedback(pc, slot);
    if base.temp_count != 0 {
        ctx.release_temps(base.temp_count);
    }
    Ok(())
}

/// Emits the nullish short-circuit sequence for an optional member
/// / call access. `base_reg` holds the object or callee value;
/// when it's `null` or `undefined` control jumps to `short_circuit`
/// (where the chain lowerer has arranged for `undefined` to be
/// loaded into the accumulator). Two jumps beats a single
/// `TestUndetectable + JumpIfToBooleanTrue` pair in the common
/// non-null case — both JumpIfNull/JumpIfUndefined are single-byte
/// tagged tests followed by a 4-byte jump operand with no
/// boolean-coercion step.
pub(super) fn emit_optional_nullish_short_circuit(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    base_reg: RegisterIndex,
    short_circuit: Label,
) -> Result<(), SourceLoweringError> {
    builder
        .emit(Opcode::Ldar, &[Operand::Reg(u32::from(base_reg))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Ldar (optional chain base): {err:?}"))
        })?;
    let jmp_null_pc = builder
        .emit_jump_to(Opcode::JumpIfNull, short_circuit)
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode JumpIfNull (optional): {err:?}"))
        })?;
    ctx.attach_branch_feedback(builder, jmp_null_pc);
    let jmp_undef_pc = builder
        .emit_jump_to(Opcode::JumpIfUndefined, short_circuit)
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode JumpIfUndefined (optional): {err:?}"))
        })?;
    ctx.attach_branch_feedback(builder, jmp_undef_pc);
    Ok(())
}

pub(super) fn optional_member_short_circuit(
    ctx: &LoweringContext<'_>,
    optional: bool,
) -> Result<Option<Label>, SourceLoweringError> {
    if !optional {
        return Ok(None);
    }
    ctx.optional_chain_short_circuit().map(Some).ok_or_else(|| {
        SourceLoweringError::Internal("optional member outside ChainExpression".into())
    })
}

/// Lowers `o[k]` into the accumulator. Shape:
///
/// ```text
///   <materialize base into r_base>
///   <lower key into acc>
///   LdaKeyedProperty r_base     ; acc = r_base[acc]
/// ```
///
/// Optional chaining mirrors the static-member path: `o?.[k]`
/// short-circuits after the base evaluation and before evaluating
/// the computed key.
///
/// §13.3.2 Property Accessors
/// <https://tc39.es/ecma262/#sec-property-accessors>
pub(super) fn lower_computed_member_read(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &ComputedMemberExpression<'_>,
) -> Result<(), SourceLoweringError> {
    let optional_short_circuit = optional_member_short_circuit(ctx, expr.optional)?;
    // M28: `super[k]` — dynamic-key super property read. Receiver
    // is `this`; key is evaluated into a dedicated temp so the
    // `GetSuperPropertyComputed` operand shape `(Reg, Reg)` matches.
    if matches!(&expr.object, Expression::Super(_)) {
        enforce_super_property_binding(ctx, &expr.object)?;
        let receiver_temp = ctx.acquire_temps(1)?;
        let key_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
        let lower = (|| -> Result<(), SourceLoweringError> {
            builder.emit(Opcode::LdaThis, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaThis (super[k]): {err:?}"))
            })?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (super[k] receiver): {err:?}"
                    ))
                })?;
            lower_return_expression(builder, ctx, &expr.expression)?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(key_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Star (super[k] key): {err:?}"))
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
                        "encode GetSuperPropertyComputed: {err:?}"
                    ))
                })?;
            Ok(())
        })();
        ctx.release_temps(2);
        return lower;
    }
    let base = materialize_member_base(builder, ctx, &expr.object)?;
    if let Some(short_circuit) = optional_short_circuit {
        emit_optional_nullish_short_circuit(builder, ctx, base.reg, short_circuit)?;
    }
    lower_return_expression(builder, ctx, &expr.expression)?;
    builder
        .emit(
            Opcode::LdaKeyedProperty,
            &[Operand::Reg(u32::from(base.reg))],
        )
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode LdaKeyedProperty: {err:?}"))
        })?;
    if base.temp_count != 0 {
        ctx.release_temps(base.temp_count);
    }
    Ok(())
}
