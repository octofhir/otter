//! Expression lowering for all shapes that land in the accumulator
//! via `lower_return_expression` dispatch: unary, conditional (?:),
//! logical (&& / || / ??), object literal, array literal, template
//! literal, and tagged template.
//!
//! Extracted from `source_compiler/mod.rs` in the file-size cleanup
//! follow-up after C4. Two utility helpers used by other
//! submodules — `numeric_literal_property_key` and
//! `property_key_tag` — are also exported.

use super::*;

/// Lowers `!x` / `-x` / `+x` / `~x` / `typeof x` / `void x` into the
/// accumulator.
///
/// Each operator maps to a dedicated single-operand opcode on the
/// accumulator:
/// - `!` → [`Opcode::LogicalNot`] (returns a boolean; works on any
///   value).
/// - `-` → [`Opcode::Negate`] (int32 wraparound on the current
///   source subset).
/// - `+` → [`Opcode::ToNumber`] (identity for int32; coerces other
///   types once the source surface grows).
/// - `~` → [`Opcode::BitwiseNot`] (int32 bitwise NOT).
/// - `typeof` → [`Opcode::TypeOf`].
/// - `void` → evaluate the argument for its side effects, then
///   overwrite acc with `undefined`.
///
/// `delete` is rejected with `unsupported("delete_unary")` — the
/// semantics depend on PropertyAccess / global-binding support that
/// the current source surface hasn't reached yet.
pub(super) fn lower_unary_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &UnaryExpression<'_>,
) -> Result<(), SourceLoweringError> {
    // §13.5.3 `typeof` on an unresolvable reference must return
    // `"undefined"` rather than throw — so for the specific shape
    // `typeof <bare-identifier>` where the identifier doesn't
    // resolve to a local/param/upvalue, emit `TypeOfGlobal` which
    // swallows the missing-global case. Every other argument shape
    // (member access, call, literal, etc.) falls through to the
    // standard evaluate-then-apply path.
    if matches!(expr.operator, UnaryOperator::Typeof)
        && let Expression::Identifier(ident) = &expr.argument
        && ctx.resolve_identifier(ident.name.as_str()).is_none()
    {
        let prop_idx = ctx.intern_property_name(ident.name.as_str())?;
        builder
            .emit(Opcode::TypeOfGlobal, &[Operand::Idx(prop_idx)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode TypeOfGlobal: {err:?}"))
            })?;
        return Ok(());
    }

    // §13.5.1 `delete <Identifier>` — unresolvable-reference case
    // must yield `true` in sloppy mode without triggering the usual
    // GetValue (which would throw). Strict-mode is an early syntax
    // error — caught by the parser, not us. For resolvable local /
    // param / upvalue bindings, return `false` (they can't be
    // deleted).
    if matches!(expr.operator, UnaryOperator::Delete)
        && let Expression::Identifier(_) = &expr.argument
    {
        let opcode = match &expr.argument {
            Expression::Identifier(ident) => {
                if ctx.resolve_identifier(ident.name.as_str()).is_some() {
                    Opcode::LdaFalse
                } else {
                    Opcode::LdaTrue
                }
            }
            _ => unreachable!(),
        };
        builder.emit(opcode, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode delete identifier: {err:?}"))
        })?;
        return Ok(());
    }

    // Evaluate the argument into the accumulator first. The operand
    // lowering already handles every shape
    // `lower_return_expression` accepts, including nested unary /
    // assignment / call expressions, so the operator step below
    // composes cleanly with any int32-producing subexpression.
    lower_return_expression(builder, ctx, &expr.argument)?;

    match expr.operator {
        UnaryOperator::LogicalNot => {
            builder.emit(Opcode::LogicalNot, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode LogicalNot: {err:?}"))
            })?;
        }
        UnaryOperator::UnaryNegation => {
            builder
                .emit(Opcode::Negate, &[])
                .map_err(|err| SourceLoweringError::Internal(format!("encode Negate: {err:?}")))?;
        }
        UnaryOperator::UnaryPlus => {
            builder.emit(Opcode::ToNumber, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode ToNumber: {err:?}"))
            })?;
        }
        UnaryOperator::BitwiseNot => {
            builder.emit(Opcode::BitwiseNot, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode BitwiseNot: {err:?}"))
            })?;
        }
        UnaryOperator::Typeof => {
            builder
                .emit(Opcode::TypeOf, &[])
                .map_err(|err| SourceLoweringError::Internal(format!("encode TypeOf: {err:?}")))?;
        }
        UnaryOperator::Void => {
            // `void x` — evaluate x for side effects (already done
            // above), then discard and return undefined.
            builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaUndefined: {err:?}"))
            })?;
        }
        UnaryOperator::Delete => {
            // §13.5.1 The `delete` Operator. For property accesses
            // we route through `DelNamedProperty` / `DelKeyedProperty`
            // opcodes. For bare-identifier deletes (`delete x` in
            // sloppy mode) JS returns `true` but removes only when
            // `x` is a configurable global — we conservatively
            // surface `true` without any side effect to match the
            // most common test262 cases; actual global removal
            // can land with S1's capability story.
            // Note: we lowered the argument above (for side effects
            // + simple-reference cases); here we emit the delete
            // against the right target.
            match &expr.argument {
                Expression::StaticMemberExpression(member) => {
                    let target_temp = ctx.acquire_temps(1)?;
                    let lower = (|| -> Result<(), SourceLoweringError> {
                        lower_return_expression(builder, ctx, &member.object)?;
                        builder
                            .emit(Opcode::Star, &[Operand::Reg(u32::from(target_temp))])
                            .map_err(|err| {
                                SourceLoweringError::Internal(format!(
                                    "encode Star (delete target): {err:?}"
                                ))
                            })?;
                        let idx = ctx.intern_property_name(member.property.name.as_str())?;
                        builder
                            .emit(
                                Opcode::DelNamedProperty,
                                &[Operand::Reg(u32::from(target_temp)), Operand::Idx(idx)],
                            )
                            .map_err(|err| {
                                SourceLoweringError::Internal(format!(
                                    "encode DelNamedProperty: {err:?}"
                                ))
                            })?;
                        Ok(())
                    })();
                    ctx.release_temps(1);
                    lower?;
                }
                Expression::ComputedMemberExpression(member) => {
                    let target_temp = ctx.acquire_temps(1)?;
                    let lower = (|| -> Result<(), SourceLoweringError> {
                        lower_return_expression(builder, ctx, &member.object)?;
                        builder
                            .emit(Opcode::Star, &[Operand::Reg(u32::from(target_temp))])
                            .map_err(|err| {
                                SourceLoweringError::Internal(format!(
                                    "encode Star (delete keyed target): {err:?}"
                                ))
                            })?;
                        lower_return_expression(builder, ctx, &member.expression)?;
                        builder
                            .emit(
                                Opcode::DelKeyedProperty,
                                &[Operand::Reg(u32::from(target_temp))],
                            )
                            .map_err(|err| {
                                SourceLoweringError::Internal(format!(
                                    "encode DelKeyedProperty: {err:?}"
                                ))
                            })?;
                        Ok(())
                    })();
                    ctx.release_temps(1);
                    lower?;
                }
                _ => {
                    // `delete expr` on a non-reference returns `true`
                    // per §13.5.1 step 3.
                    builder.emit(Opcode::LdaTrue, &[]).map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode LdaTrue (delete non-reference): {err:?}"
                        ))
                    })?;
                }
            }
        }
    }
    Ok(())
}

/// Lowers `test ? consequent : alternate` (ConditionalExpression).
///
/// Bytecode shape — the standard branch-and-join:
///
/// ```text
///   <lower test>                ; acc = test
///   JumpIfToBooleanFalse else_label
///   <lower consequent>          ; acc = consequent
///   Jump end_label
/// else_label:
///   <lower alternate>           ; acc = alternate
/// end_label:
/// ```
///
/// `JumpIfToBooleanFalse` takes the ToBoolean coercion path the
/// interpreter already performs for `if` / `while` conditions, so
/// any truthy-or-falsy JS value works as the test — not just a
/// strict boolean. Result lands in the accumulator ready for
/// composition with surrounding expressions.
pub(super) fn lower_conditional_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &ConditionalExpression<'_>,
) -> Result<(), SourceLoweringError> {
    let else_label = builder.new_label();
    let end_label = builder.new_label();

    lower_return_expression(builder, ctx, &expr.test)?;
    let jmp_pc = builder
        .emit_jump_to(Opcode::JumpIfToBooleanFalse, else_label)
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode JumpIfToBooleanFalse (ternary): {err:?}"))
        })?;
    ctx.attach_branch_feedback(builder, jmp_pc);
    lower_return_expression(builder, ctx, &expr.consequent)?;
    builder
        .emit_jump_to(Opcode::Jump, end_label)
        .map_err(|err| SourceLoweringError::Internal(format!("encode Jump (ternary): {err:?}")))?;
    builder
        .bind_label(else_label)
        .map_err(|err| SourceLoweringError::Internal(format!("bind ternary else: {err:?}")))?;
    lower_return_expression(builder, ctx, &expr.alternate)?;
    builder
        .bind_label(end_label)
        .map_err(|err| SourceLoweringError::Internal(format!("bind ternary end: {err:?}")))?;
    Ok(())
}

/// Lowers `a && b` / `a || b` / `a ?? b` with the spec-mandated
/// short-circuit semantics.
///
/// `&&` returns `a` if `a` is falsy (ToBoolean false), else `b`.
/// `||` returns `a` if `a` is truthy (ToBoolean true), else `b`.
/// `??` returns `a` if `a` is **neither** `null` nor `undefined`,
/// else `b`. None of the operators coerce the surviving left-hand
/// value — `0 && x` returns `0` (not `false`), `"" || x` returns
/// `x` (after the truthy test on `""` sees falsy), and `null ?? x`
/// returns `x`.
///
/// Bytecode shape (for `&&`, showing the representative
/// branch-and-join):
///
/// ```text
///   <lower left>                  ; acc = left
///   JumpIfToBooleanFalse end      ; short-circuit: keep acc = left
///   <lower right>                 ; acc = right
/// end:
/// ```
///
/// `||` uses `JumpIfToBooleanTrue` instead. `??` uses a two-step
/// `JumpIfNotNull` + `JumpIfNotUndefined` sequence so the short-
/// circuit only kicks in when `left` is not null/undefined.
pub(super) fn lower_logical_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &LogicalExpression<'_>,
) -> Result<(), SourceLoweringError> {
    lower_return_expression(builder, ctx, &expr.left)?;

    match expr.operator {
        LogicalOperator::And => {
            let end_label = builder.new_label();
            let jmp_pc = builder
                .emit_jump_to(Opcode::JumpIfToBooleanFalse, end_label)
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode JumpIfToBooleanFalse (&&): {err:?}"
                    ))
                })?;
            ctx.attach_branch_feedback(builder, jmp_pc);
            lower_return_expression(builder, ctx, &expr.right)?;
            builder
                .bind_label(end_label)
                .map_err(|err| SourceLoweringError::Internal(format!("bind &&: {err:?}")))?;
        }
        LogicalOperator::Or => {
            let end_label = builder.new_label();
            let jmp_pc = builder
                .emit_jump_to(Opcode::JumpIfToBooleanTrue, end_label)
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode JumpIfToBooleanTrue (||): {err:?}"
                    ))
                })?;
            ctx.attach_branch_feedback(builder, jmp_pc);
            lower_return_expression(builder, ctx, &expr.right)?;
            builder
                .bind_label(end_label)
                .map_err(|err| SourceLoweringError::Internal(format!("bind ||: {err:?}")))?;
        }
        LogicalOperator::Coalesce => {
            // `a ?? b`: short-circuit to `end` when `a` is neither
            // null nor undefined. Otherwise fall through to the
            // right-hand lowering. The two-step probe exploits the
            // existing `JumpIfNotNull` / `JumpIfNotUndefined`
            // opcodes without introducing a new "is nullish" op.
            //
            // Control flow:
            //   acc = a
            //   if acc != null → jump check_undefined
            //   // acc == null: fall through to lower b
            //   <lower b>
            //   jump end
            //   check_undefined:
            //   if acc != undefined → jump end (keep acc = a)
            //   <lower b>   [reached only when acc was undefined]
            //   end:
            //
            // The block below emits a simpler equivalent by sharing
            // the right-hand lowering for both the null and
            // undefined cases — a single `lower_right` block is
            // used regardless of which nullish value matched.
            let check_undefined = builder.new_label();
            let lower_right_label = builder.new_label();
            let end_label = builder.new_label();
            let jmp_pc = builder
                .emit_jump_to(Opcode::JumpIfNotNull, check_undefined)
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode JumpIfNotNull (??): {err:?}"))
                })?;
            ctx.attach_branch_feedback(builder, jmp_pc);
            // `a` is null — fall through to the right-hand path.
            builder
                .emit_jump_to(Opcode::Jump, lower_right_label)
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Jump (?? null → right): {err:?}"))
                })?;
            builder.bind_label(check_undefined).map_err(|err| {
                SourceLoweringError::Internal(format!("bind ?? check_undefined: {err:?}"))
            })?;
            // Not null — check undefined. If not undefined either,
            // short-circuit to end keeping `acc = a`.
            let jmp_pc = builder
                .emit_jump_to(Opcode::JumpIfNotUndefined, end_label)
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode JumpIfNotUndefined (??): {err:?}"
                    ))
                })?;
            ctx.attach_branch_feedback(builder, jmp_pc);
            builder.bind_label(lower_right_label).map_err(|err| {
                SourceLoweringError::Internal(format!("bind ?? lower_right: {err:?}"))
            })?;
            lower_return_expression(builder, ctx, &expr.right)?;
            builder
                .bind_label(end_label)
                .map_err(|err| SourceLoweringError::Internal(format!("bind ?? end: {err:?}")))?;
        }
    }
    Ok(())
}

/// Lowers an `ObjectExpression` literal with static-identifier or
/// string-literal keys. Computed keys, methods, shorthand, spread,
/// getters, and setters are rejected with a stable per-shape tag —
/// later milestones widen the surface.
///
/// Bytecode shape:
///
/// ```text
///   CreateObject               ; acc = {}
///   Star r_obj                 ; spill object handle to a temp
///   <lower value_0>            ; acc = value_0
///   StaNamedProperty r_obj, k0 ; obj[k0] = value_0
///   <lower value_1>            ; acc = value_1
///   StaNamedProperty r_obj, k1 ; obj[k1] = value_1
///   …
///   Ldar r_obj                 ; acc = obj (result of the expression)
/// ```
///
/// The empty-object case `{}` collapses to a single `CreateObject`
/// with no temp-slot traffic — neither the spill nor the reload are
/// emitted.
///
/// §13.2.5 Object Initializer
/// <https://tc39.es/ecma262/#sec-object-initializer>
pub(super) fn lower_object_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &ObjectExpression<'_>,
) -> Result<(), SourceLoweringError> {
    builder
        .emit(Opcode::CreateObject, &[])
        .map_err(|err| SourceLoweringError::Internal(format!("encode CreateObject: {err:?}")))?;

    if expr.properties.is_empty() {
        return Ok(());
    }

    // Acquire a temp to hold the object handle across the property
    // initialisers — each value lowering clobbers acc.
    let obj_temp = ctx.acquire_temps(1)?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(obj_temp))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (object temp): {err:?}"))
        })?;

    let lower = (|| -> Result<(), SourceLoweringError> {
        for prop_kind in &expr.properties {
            let prop = match prop_kind {
                ObjectPropertyKind::ObjectProperty(p) => p,
                // `{ ...source }` — spread. Evaluate `source`,
                // then copy every own enumerable property onto
                // the target via `CopyDataProperties` (runtime
                // helper).
                ObjectPropertyKind::SpreadProperty(s) => {
                    lower_return_expression(builder, ctx, &s.argument)?;
                    builder
                        .emit(
                            Opcode::CopyDataProperties,
                            &[Operand::Reg(u32::from(obj_temp))],
                        )
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode CopyDataProperties: {err:?}"
                            ))
                        })?;
                    continue;
                }
            };
            // Accessor property (`{ get x() {} }` / `{ set x(v) {} }`).
            // Lower the value (a FunctionExpression) into acc,
            // then emit DefineClassGetter / DefineClassSetter
            // — the class-accessor opcode installs the closure
            // as an accessor-half on the target. Class methods
            // use `enumerable=false`; object-literal accessors
            // are spec'd `enumerable=true`, a small divergence
            // invisible outside `Object.keys` / `for...in`.
            if !matches!(prop.kind, PropertyKind::Init) {
                let is_getter = matches!(prop.kind, PropertyKind::Get);
                if prop.computed {
                    let key_temp = ctx.acquire_temps(1)?;
                    let comp_result = (|| -> Result<(), SourceLoweringError> {
                        lower_return_expression(builder, ctx, prop.key.to_expression())?;
                        builder
                            .emit(Opcode::Star, &[Operand::Reg(u32::from(key_temp))])
                            .map_err(|err| {
                                SourceLoweringError::Internal(format!(
                                    "encode Star (accessor computed key): {err:?}"
                                ))
                            })?;
                        lower_return_expression(builder, ctx, &prop.value)?;
                        let accessor_opcode = if is_getter {
                            Opcode::DefineClassGetterComputed
                        } else {
                            Opcode::DefineClassSetterComputed
                        };
                        builder
                            .emit(
                                accessor_opcode,
                                &[
                                    Operand::Reg(u32::from(obj_temp)),
                                    Operand::Reg(u32::from(key_temp)),
                                ],
                            )
                            .map_err(|err| {
                                SourceLoweringError::Internal(format!(
                                    "encode accessor computed: {err:?}"
                                ))
                            })?;
                        Ok(())
                    })();
                    ctx.release_temps(1);
                    comp_result?;
                    continue;
                }
                let key_name = match &prop.key {
                    PropertyKey::StaticIdentifier(ident) => ident.name.as_str().to_owned(),
                    PropertyKey::StringLiteral(lit) => lit.value.as_str().to_owned(),
                    PropertyKey::NumericLiteral(lit) => numeric_literal_property_key(lit.value),
                    PropertyKey::BigIntLiteral(lit) => lit.value.as_str().to_owned(),
                    other => {
                        return Err(SourceLoweringError::unsupported(
                            property_key_tag(other),
                            other.span(),
                        ));
                    }
                };
                let idx = ctx.intern_property_name(&key_name)?;
                lower_return_expression(builder, ctx, &prop.value)?;
                let accessor_opcode = if is_getter {
                    Opcode::DefineClassGetter
                } else {
                    Opcode::DefineClassSetter
                };
                builder
                    .emit(
                        accessor_opcode,
                        &[Operand::Reg(u32::from(obj_temp)), Operand::Idx(idx)],
                    )
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!("encode accessor: {err:?}"))
                    })?;
                continue;
            }
            // Computed key: `{ [expr]: value }`. Lower the key
            // expression into a temp, then use `StaKeyedProperty`
            // so the runtime handles the ToPropertyKey + set.
            if prop.computed {
                let key_temp = ctx.acquire_temps(1)?;
                let computed_lower = (|| -> Result<(), SourceLoweringError> {
                    lower_return_expression(builder, ctx, prop.key.to_expression())?;
                    builder
                        .emit(Opcode::Star, &[Operand::Reg(u32::from(key_temp))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode Star (obj computed key): {err:?}"
                            ))
                        })?;
                    lower_return_expression(builder, ctx, &prop.value)?;
                    builder
                        .emit(
                            Opcode::StaKeyedProperty,
                            &[
                                Operand::Reg(u32::from(obj_temp)),
                                Operand::Reg(u32::from(key_temp)),
                            ],
                        )
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode StaKeyedProperty (obj computed): {err:?}"
                            ))
                        })?;
                    Ok(())
                })();
                ctx.release_temps(1);
                computed_lower?;
                continue;
            }
            let key_name = match &prop.key {
                PropertyKey::StaticIdentifier(ident) => ident.name.as_str().to_owned(),
                PropertyKey::StringLiteral(lit) => lit.value.as_str().to_owned(),
                // §13.2.5.4 — numeric-literal keys stringify per the
                // CanonicalNumericIndexString algorithm (essentially
                // `ToString(n)`), so `{0: "a", 1.5: "b"}` becomes
                // `{"0": "a", "1.5": "b"}` at runtime.
                PropertyKey::NumericLiteral(lit) => numeric_literal_property_key(lit.value),
                // §13.2.5.4 — BigInt keys stringify without the `n`
                // suffix (`{1n: "a"}` → `{"1": "a"}`). oxc hands us
                // the already-normalised base-10 digits.
                PropertyKey::BigIntLiteral(lit) => lit.value.as_str().to_owned(),
                other => {
                    return Err(SourceLoweringError::unsupported(
                        property_key_tag(other),
                        other.span(),
                    ));
                }
            };
            // Lower the value into acc. `{ x }` (shorthand) and
            // `{ foo() {} }` (method) both have their value
            // correctly modelled by oxc: shorthand's value is an
            // Identifier reference, method's value is a
            // FunctionExpression — both already supported by
            // `lower_return_expression`. No special case
            // required.
            lower_return_expression(builder, ctx, &prop.value)?;
            let idx = ctx.intern_property_name(&key_name)?;
            let pc = builder
                .emit(
                    Opcode::StaNamedProperty,
                    &[Operand::Reg(u32::from(obj_temp)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode StaNamedProperty: {err:?}"))
                })?;
            ctx.attach_property_store_feedback(builder, pc);
        }
        // Reload the object handle so the expression's value is in
        // acc for the caller.
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(obj_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (object reload): {err:?}"))
            })?;
        Ok(())
    })();
    ctx.release_temps(1);
    lower
}

/// Lowers an `ArrayExpression` literal. Elements are emitted in
/// source order via `ArrayPush` — the runtime's array helper bumps
/// `length` and writes into the dense elements slot. Spread elements
/// and holes (`[1, , 2]`) are rejected with a stable tag so future
/// milestones can widen the surface without silently changing
/// semantics.
///
/// Bytecode shape:
///
/// ```text
///   CreateArray                ; acc = []
///   Star r_arr
///   <lower element_0>          ; acc = element_0
///   ArrayPush r_arr            ; arr.push(element_0)
///   <lower element_1>
///   ArrayPush r_arr
///   …
///   Ldar r_arr                 ; acc = arr
/// ```
///
/// The empty-array case `[]` collapses to a single `CreateArray`
/// with no temp traffic.
///
/// §13.2.4 Array Initializer
/// <https://tc39.es/ecma262/#sec-array-initializer>
pub(super) fn lower_array_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &ArrayExpression<'_>,
) -> Result<(), SourceLoweringError> {
    builder
        .emit(Opcode::CreateArray, &[])
        .map_err(|err| SourceLoweringError::Internal(format!("encode CreateArray: {err:?}")))?;

    if expr.elements.is_empty() {
        return Ok(());
    }

    let arr_temp = ctx.acquire_temps(1)?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(arr_temp))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (array temp): {err:?}"))
        })?;

    let lower = (|| -> Result<(), SourceLoweringError> {
        for element in &expr.elements {
            match element {
                ArrayExpressionElement::SpreadElement(spread) => {
                    // M23: `[...iter]` — iterate the spread
                    // source and push each value. The
                    // `SpreadIntoArray r_arr` opcode handles the
                    // iterator protocol + push loop in the
                    // dispatcher; here we just lower the source
                    // into acc and emit the opcode.
                    lower_return_expression(builder, ctx, &spread.argument)?;
                    builder
                        .emit(
                            Opcode::SpreadIntoArray,
                            &[Operand::Reg(u32::from(arr_temp))],
                        )
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode SpreadIntoArray (array literal): {err:?}"
                            ))
                        })?;
                }
                ArrayExpressionElement::Elision(_elision) => {
                    // `[1, , 3]` — a hole creates a sparse slot
                    // whose length counts it but whose indexed
                    // access returns `undefined` and whose `in`
                    // check returns `false`. Simulate by pushing
                    // `undefined`; the resulting array is dense
                    // but indistinguishable for the vast majority
                    // of user code. True holes need an
                    // `ArrayPushHole` opcode that doesn't exist
                    // yet — follow-up work.
                    builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode LdaUndefined (elision): {err:?}"
                        ))
                    })?;
                    builder
                        .emit(Opcode::ArrayPush, &[Operand::Reg(u32::from(arr_temp))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode ArrayPush (elision): {err:?}"
                            ))
                        })?;
                }
                // Non-spread, non-hole element. `to_expression`
                // downcasts the `Expression` variants inlined by
                // `ArrayExpressionElement` back to `&Expression`.
                other => {
                    let element_expr = other.to_expression();
                    lower_return_expression(builder, ctx, element_expr)?;
                    builder
                        .emit(Opcode::ArrayPush, &[Operand::Reg(u32::from(arr_temp))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!("encode ArrayPush: {err:?}"))
                        })?;
                }
            }
        }
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(arr_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (array reload): {err:?}"))
            })?;
        Ok(())
    })();
    ctx.release_temps(1);
    lower
}

pub(super) fn lower_template_literal(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    tpl: &TemplateLiteral<'_>,
) -> Result<(), SourceLoweringError> {
    // Expressions.len() == quasis.len() - 1 by construction.
    if tpl.quasis.len() != tpl.expressions.len() + 1 {
        return Err(SourceLoweringError::Internal(format!(
            "template literal has {} quasis for {} expressions",
            tpl.quasis.len(),
            tpl.expressions.len()
        )));
    }

    let quasi_cooked = |index: usize| -> Result<&str, SourceLoweringError> {
        let q = &tpl.quasis[index];
        match q.value.cooked.as_deref() {
            Some(s) => Ok(s),
            None => Err(SourceLoweringError::unsupported(
                "invalid_template_escape",
                q.span,
            )),
        }
    };

    // No substitutions → just emit the head quasi. This covers the
    // simple form `` `hello` `` and the empty form `` `` ``.
    if tpl.expressions.is_empty() {
        let text = quasi_cooked(0)?;
        let idx = ctx.intern_string_literal(text)?;
        builder
            .emit(Opcode::LdaConstStr, &[Operand::Idx(idx)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaConstStr (template): {err:?}"))
            })?;
        return Ok(());
    }

    // Interpolated form. Keep a running result in `r_buf` and use
    // `r_tmp` to hold each fresh piece before the `Add r_tmp`.
    let buf = ctx.acquire_temps(1)?;
    let tmp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;

    let lower = (|| -> Result<(), SourceLoweringError> {
        // 1) Load quasi[0] into acc, spill to r_buf. Using the head
        //    as the starting value keeps the concat LHS-first for
        //    the first substitution — critical since every later
        //    `Add r_tmp` computes `acc + r_tmp`, which must equal
        //    `buf + piece` in that order.
        let head = quasi_cooked(0)?;
        let head_idx = ctx.intern_string_literal(head)?;
        builder
            .emit(Opcode::LdaConstStr, &[Operand::Idx(head_idx)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaConstStr (head): {err:?}"))
            })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(buf))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (template buf): {err:?}"))
            })?;

        // 2) Walk the pieces: for each expression `e_i` emit
        //    `<lower e_i>; Star r_tmp; Ldar r_buf; Add r_tmp;`. Then
        //    (if the following quasi is non-empty) do the same for
        //    `q_{i+1}`. After each concat, roll the buffer forward
        //    via `Star r_buf` — except after the very last piece,
        //    where we leave the result in acc for the caller.
        let last_expr = tpl.expressions.len() - 1;

        for (i, expr) in tpl.expressions.iter().enumerate() {
            let next_quasi_text = quasi_cooked(i + 1)?;
            let has_next_quasi = !next_quasi_text.is_empty();
            let is_last_piece_overall = i == last_expr && !has_next_quasi;

            // Append `expr` to `r_buf`.
            lower_return_expression(builder, ctx, expr)?;
            concat_step(builder, ctx, tmp, buf)?;

            if is_last_piece_overall {
                // Skip the trailing `Star r_buf` — acc already holds
                // the final running result.
                continue;
            }
            // Roll buffer forward so the next piece concatenates
            // against the fresh value.
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(buf))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (template buf roll): {err:?}"
                    ))
                })?;

            if has_next_quasi {
                let quasi_idx = ctx.intern_string_literal(next_quasi_text)?;
                builder
                    .emit(Opcode::LdaConstStr, &[Operand::Idx(quasi_idx)])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode LdaConstStr (template quasi): {err:?}"
                        ))
                    })?;
                concat_step(builder, ctx, tmp, buf)?;
                if i != last_expr {
                    builder
                        .emit(Opcode::Star, &[Operand::Reg(u32::from(buf))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode Star (template buf roll 2): {err:?}"
                            ))
                        })?;
                }
            }
        }
        Ok(())
    })();
    ctx.release_temps(1); // tmp
    ctx.release_temps(1); // buf
    lower
}

/// Emits `Star r_tmp; Ldar r_buf; Add r_tmp` to append the value
/// currently in the accumulator onto the running buffer in `r_buf`.
/// Result ends up in acc (`r_buf + piece`). Attaches an arithmetic
/// feedback slot to the `Add` so JIT baseline recompiles see the
/// path as observed — the value will always be `Any` (string
/// concat), which keeps the tag guards in place.
fn concat_step(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    tmp: RegisterIndex,
    buf: RegisterIndex,
) -> Result<(), SourceLoweringError> {
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(tmp))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (template tmp): {err:?}"))
        })?;
    builder
        .emit(Opcode::Ldar, &[Operand::Reg(u32::from(buf))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Ldar (template buf): {err:?}"))
        })?;
    let add_pc = builder
        .emit(Opcode::Add, &[Operand::Reg(u32::from(tmp))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Add (template concat): {err:?}"))
        })?;
    let slot = ctx.allocate_arithmetic_feedback();
    builder.attach_feedback(add_pc, slot);
    Ok(())
}

/// §13.3.11 `` tag`quasi0${e0}quasi1…` `` — lowers a tagged
/// template call into `tag(strings, e0, e1, …)` where `strings`
/// is the cooked-parts array with a `.raw` property pointing at
/// the raw-parts array.
///
/// Bytecode shape (`N` = substitution count):
///
/// ```text
///   <lower tag>; Star r_callee
///   CreateArray; Star r_args[0]          ; strings (cooked)
///   <for each cooked>: LdaConstStr; ArrayPush r_args[0]
///   CreateArray; Star r_raw              ; raw array
///   <for each raw>: LdaConstStr; ArrayPush r_raw
///   Ldar r_raw; StaNamedProperty r_args[0], "raw"_idx
///   <lower e0>; Star r_args[1]
///   …
///   <lower eN>; Star r_args[N]
///   CallUndefinedReceiver r_callee, RegList { base: r_args, count: N + 1 }
/// ```
///
/// Departs from the spec in one place: §13.2.8.3 / §13.2.8.4
/// require that the cooked and raw arrays be frozen and cached
/// per template-site across invocations. A fresh array is built
/// on every call — observable only via
/// `template === sameTemplateFn()` identity tests, which aren't
/// in the common path.
pub(super) fn lower_tagged_template_expression<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    tagged: &'a oxc_ast::ast::TaggedTemplateExpression<'a>,
) -> Result<(), SourceLoweringError> {
    let tpl = &tagged.quasi;
    if tpl.quasis.len() != tpl.expressions.len() + 1 {
        return Err(SourceLoweringError::Internal(format!(
            "tagged template has {} quasis for {} expressions",
            tpl.quasis.len(),
            tpl.expressions.len(),
        )));
    }

    let argc = RegisterIndex::try_from(tpl.expressions.len() + 1)
        .map_err(|_| SourceLoweringError::Internal("tagged template argc overflow".into()))?;

    let callee_temp = ctx.acquire_temps(1)?;
    let args_base = ctx
        .acquire_temps(argc)
        .inspect_err(|_| ctx.release_temps(1))?;
    let raw_temp = ctx
        .acquire_temps(1)
        .inspect_err(|_| ctx.release_temps(argc + 1))?;

    let lower = (|| -> Result<(), SourceLoweringError> {
        // 1) Evaluate the tag expression → callee_temp.
        lower_return_expression(builder, ctx, &tagged.tag)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(callee_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (tagged tag): {err:?}"))
            })?;

        // 2) Build the cooked strings array directly into
        //    args_base[0] — it becomes the first argument to the
        //    tag call.
        builder.emit(Opcode::CreateArray, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode CreateArray (tagged cooked): {err:?}"))
        })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(args_base))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (tagged cooked arr): {err:?}"))
            })?;
        for quasi in tpl.quasis.iter() {
            // Per §13.2.8.5, invalid escape sequences leave
            // cooked as `undefined`; unsupported for now so we
            // stay clear of the spec's `undefined` entry shape.
            let cooked = quasi.value.cooked.as_deref().ok_or_else(|| {
                SourceLoweringError::unsupported("invalid_template_escape", quasi.span)
            })?;
            let cooked_idx = ctx.intern_string_literal(cooked)?;
            builder
                .emit(Opcode::LdaConstStr, &[Operand::Idx(cooked_idx)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaConstStr (tagged cooked): {err:?}"
                    ))
                })?;
            builder
                .emit(Opcode::ArrayPush, &[Operand::Reg(u32::from(args_base))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode ArrayPush (tagged cooked): {err:?}"
                    ))
                })?;
        }

        // 3) Build the raw strings array.
        builder.emit(Opcode::CreateArray, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode CreateArray (tagged raw): {err:?}"))
        })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(raw_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (tagged raw arr): {err:?}"))
            })?;
        for quasi in tpl.quasis.iter() {
            let raw_idx = ctx.intern_string_literal(quasi.value.raw.as_str())?;
            builder
                .emit(Opcode::LdaConstStr, &[Operand::Idx(raw_idx)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaConstStr (tagged raw): {err:?}"
                    ))
                })?;
            builder
                .emit(Opcode::ArrayPush, &[Operand::Reg(u32::from(raw_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode ArrayPush (tagged raw): {err:?}"))
                })?;
        }

        // 4) strings.raw = raw.
        let raw_name_idx = ctx.intern_property_name("raw")?;
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(raw_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (tagged raw): {err:?}"))
            })?;
        let sta_pc = builder
            .emit(
                Opcode::StaNamedProperty,
                &[
                    Operand::Reg(u32::from(args_base)),
                    Operand::Idx(raw_name_idx),
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode StaNamedProperty (tagged raw): {err:?}"
                ))
            })?;
        ctx.attach_property_store_feedback(builder, sta_pc);

        // 5) Lower each substitution into args_base[1..].
        for (i, expr) in tpl.expressions.iter().enumerate() {
            lower_return_expression(builder, ctx, expr)?;
            let slot = args_base
                .checked_add(RegisterIndex::try_from(i + 1).map_err(|_| {
                    SourceLoweringError::Internal("tagged arg slot overflow".into())
                })?)
                .ok_or_else(|| {
                    SourceLoweringError::Internal("tagged arg slot overflow (add)".into())
                })?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Star (tagged arg): {err:?}"))
                })?;
        }

        // 6) Dispatch with `this = undefined`.
        let call_pc = builder
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
                    "encode CallUndefinedReceiver (tagged): {err:?}"
                ))
            })?;
        ctx.attach_call_feedback(builder, call_pc);
        Ok(())
    })();

    ctx.release_temps(1); // raw_temp
    ctx.release_temps(argc); // args
    ctx.release_temps(1); // callee_temp
    lower
}

/// §7.1.17 ToString for a numeric property-key literal. Integer
/// values format without a decimal point, matching
/// `Number.prototype.toString` / ES2024 §7.1.17 — so `{0: ...}`
/// becomes property name `"0"`, not `"0.0"`. `NaN` and `Infinity`
/// can't reach here (parser rejects them as property keys) but the
/// fallback uses Rust's default f64 `Display`, which is fine for the
/// rare fractional literal case.
pub(super) fn numeric_literal_property_key(value: f64) -> String {
    if value.is_finite() && value == value.trunc() && value.abs() < 1e21 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Stable tag for unsupported `PropertyKey` shapes — surfaces in
/// `SourceLoweringError::Unsupported { construct }`.
pub(super) fn property_key_tag(key: &PropertyKey<'_>) -> &'static str {
    match key {
        PropertyKey::StaticIdentifier(_) => "static_identifier_key",
        PropertyKey::PrivateIdentifier(_) => "private_identifier_key",
        PropertyKey::StringLiteral(_) => "string_literal_key",
        PropertyKey::NumericLiteral(_) => "numeric_literal_key",
        PropertyKey::BigIntLiteral(_) => "bigint_literal_key",
        PropertyKey::TemplateLiteral(_) => "template_literal_key",
        // All other expression-inherited variants surface as a
        // generic computed-key tag. Reached only when the AST builds
        // something like `{[expr]: v}` slipping past the `computed`
        // guard — the front wall rejects first.
        _ => "computed_property_key",
    }
}
