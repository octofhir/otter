//! Assignment expression lowering: `x = v`, compound `x += v`,
//! destructuring `[a, b] = arr`, member stores, private-field
//! stores, and all operator shapes in between.
//!
//! Extracted from `source_compiler/mod.rs` in the file-size cleanup
//! follow-up after C4. Public entry points:
//! `lower_assignment_expression` plus the destructuring helpers used
//! by `for_in_of` (`destructure_{array,object}_assignment_from_temp*`)
//! and the member stores (`assign_static_member`,
//! `assign_computed_member`). Everything else — identifier / private
//! / computed-member / compound-op paths — is internal.

use super::*;

/// Lowers `target <op>= rhs` (or `target = rhs`) onto a local `let`
/// slot. Leaves the assigned value in the accumulator so nested
/// assignments (`let y = x = 5;`, `return x = 5;`) compose without
/// extra Ldar / Star round-trips.
///
/// Bytecode shape:
/// - `x = rhs` →  `<lower rhs>; Star r_x`
/// - `x += rhs` → `Ldar r_x; <Add/AddSmi rhs>; Star r_x`
/// - other compound forms identical, with the matching binary opcode.
///
/// Rejects:
/// - non-identifier target (member, destructuring, TS-only) →
///   stable per-shape tag;
/// - const binding as target → `const_assignment`;
/// - in-TDZ binding as target → `tdz_self_reference`;
/// - assignment operator outside `=`/`+=`/`-=`/`*=`/`|=` → stable
///   per-operator tag (e.g. `division_assign`).
pub(super) fn lower_assignment_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &AssignmentExpression<'_>,
) -> Result<(), SourceLoweringError> {
    // Dispatch on target shape after peeling TypeScript-only LHS
    // wrappers (`x! =`, `(obj as T).x =`, etc.). Those wrappers are
    // runtime no-ops, so the underlying JS reference determines the
    // emitted store path.
    match unwrap_assignment_target(&expr.left)? {
        AssignmentTargetRef::Identifier(ident) => {
            lower_identifier_assignment(builder, ctx, expr, ident)
        }
        AssignmentTargetRef::StaticMember(member) => {
            lower_static_member_assignment(builder, ctx, expr, member)
        }
        AssignmentTargetRef::ComputedMember(member) => {
            lower_computed_member_assignment(builder, ctx, expr, member)
        }
        AssignmentTargetRef::PrivateField(member) => {
            lower_private_field_assignment(builder, ctx, expr, member)
        }
        AssignmentTargetRef::Array(pattern) => {
            lower_array_destructuring_assignment(builder, ctx, expr, pattern)
        }
        AssignmentTargetRef::Object(pattern) => {
            lower_object_destructuring_assignment(builder, ctx, expr, pattern)
        }
    }
}

/// Identifier-target path for `lower_assignment_expression`. Preserves
/// the original M5 semantics: local `let` only, rejects `const`, TDZ,
/// and param writes; compound `<op>=` emits `Ldar r_x; <apply op>;
/// Star r_x`.
/// Destructuring assignment to an array-shaped target:
/// `[a, b, c] = arr` (no `let` keyword — assigns to EXISTING
/// bindings). Evaluates the RHS once into a temp, then for each
/// element emits a `LdaKeyedProperty` read + assign to the
/// element target. Supports defaults, nested patterns, and rest.
/// Leaves the RHS value in the accumulator so the assignment
/// expression yields the source object per §13.15.
fn lower_array_destructuring_assignment<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    expr: &'a AssignmentExpression<'a>,
    pattern: &'a oxc_ast::ast::ArrayAssignmentTarget<'a>,
) -> Result<(), SourceLoweringError> {
    if !matches!(expr.operator, AssignmentOperator::Assign) {
        return Err(SourceLoweringError::unsupported(
            "destructuring_assignment_target",
            pattern.span,
        ));
    }
    let src_temp = ctx.acquire_temps(1)?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        // RHS → temp.
        lower_return_expression(builder, ctx, &expr.right)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(src_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (array destruct src): {err:?}"))
            })?;
        destructure_array_assignment_from_temp(builder, ctx, pattern, src_temp)?;
        // Leave the RHS value in acc so the assignment-expression
        // yields the source per §13.15.2.
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(src_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Ldar (array destruct yield): {err:?}"
                ))
            })?;
        Ok(())
    })();
    ctx.release_temps(1);
    lower
}

/// Destructuring assignment to an object-shaped target:
/// `({ a, b: c, ...rest } = obj)`.
fn lower_object_destructuring_assignment<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    expr: &'a AssignmentExpression<'a>,
    pattern: &'a oxc_ast::ast::ObjectAssignmentTarget<'a>,
) -> Result<(), SourceLoweringError> {
    if !matches!(expr.operator, AssignmentOperator::Assign) {
        return Err(SourceLoweringError::unsupported(
            "destructuring_assignment_target",
            pattern.span,
        ));
    }
    let src_temp = ctx.acquire_temps(1)?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        lower_return_expression(builder, ctx, &expr.right)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(src_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (obj destruct src): {err:?}"))
            })?;
        destructure_object_assignment_from_temp(builder, ctx, pattern, src_temp)?;
        // Yield the RHS as the assignment's value.
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(src_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (obj destruct yield): {err:?}"))
            })?;
        Ok(())
    })();
    ctx.release_temps(1);
    lower
}

/// Assigns the accumulator (already holding the right value) to
/// a destructuring-assignment leaf. Handles the `MaybeDefault`
/// wrapper by running the default-check first.
fn assign_destructured_target<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    target: &'a oxc_ast::ast::AssignmentTargetMaybeDefault<'a>,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::AssignmentTargetMaybeDefault as M;
    match target {
        M::AssignmentTargetWithDefault(wd) => {
            emit_default_for_destructured_leaf(builder, ctx, Some(&wd.init))?;
            assign_destructured_target_from_assignment_target(builder, ctx, &wd.binding)
        }
        // The `inherit_variants!` macro ensures every
        // `AssignmentTarget` variant is mirrored as a
        // `MaybeDefault` variant with the same discriminant
        // range — match each explicitly so the compiler's
        // exhaustiveness check stays honest.
        M::AssignmentTargetIdentifier(ident) => assign_identifier_reference(builder, ctx, ident),
        M::StaticMemberExpression(member) => assign_static_member(builder, ctx, member),
        M::ComputedMemberExpression(member) => assign_computed_member(builder, ctx, member),
        M::ArrayAssignmentTarget(nested) => {
            let nested_src = ctx.acquire_temps(1)?;
            let lower = (|| -> Result<(), SourceLoweringError> {
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(nested_src))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (nested array destruct): {err:?}"
                        ))
                    })?;
                destructure_array_assignment_from_temp(builder, ctx, nested, nested_src)
            })();
            ctx.release_temps(1);
            lower
        }
        M::ObjectAssignmentTarget(nested) => {
            let nested_src = ctx.acquire_temps(1)?;
            let lower = (|| -> Result<(), SourceLoweringError> {
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(nested_src))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (nested obj destruct): {err:?}"
                        ))
                    })?;
                destructure_object_assignment_from_temp(builder, ctx, nested, nested_src)
            })();
            ctx.release_temps(1);
            lower
        }
        _ => Err(SourceLoweringError::unsupported(
            "destructuring_assignment_target",
            target.span(),
        )),
    }
}

pub(super) fn assign_static_member<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    member: &'a StaticMemberExpression<'a>,
) -> Result<(), SourceLoweringError> {
    let val_temp = ctx.acquire_temps(1)?;
    let recv_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(val_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (destruct static member val): {err:?}"
                ))
            })?;
        lower_return_expression(builder, ctx, &member.object)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(recv_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (destruct static member recv): {err:?}"
                ))
            })?;
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(val_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Ldar (destruct static member reload): {err:?}"
                ))
            })?;
        let prop_idx = ctx.intern_property_name(member.property.name.as_str())?;
        let sta_pc = builder
            .emit(
                Opcode::StaNamedProperty,
                &[Operand::Reg(u32::from(recv_temp)), Operand::Idx(prop_idx)],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode StaNamedProperty (destruct static member): {err:?}"
                ))
            })?;
        ctx.attach_property_store_feedback(builder, sta_pc);
        Ok(())
    })();
    ctx.release_temps(2);
    lower
}

pub(super) fn assign_computed_member<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    member: &'a ComputedMemberExpression<'a>,
) -> Result<(), SourceLoweringError> {
    let val_temp = ctx.acquire_temps(1)?;
    let recv_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
    let key_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(2))?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(val_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (destruct computed val): {err:?}"
                ))
            })?;
        lower_return_expression(builder, ctx, &member.object)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(recv_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (destruct computed recv): {err:?}"
                ))
            })?;
        lower_return_expression(builder, ctx, &member.expression)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(key_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (destruct computed key): {err:?}"
                ))
            })?;
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(val_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Ldar (destruct computed reload): {err:?}"
                ))
            })?;
        builder
            .emit(
                Opcode::StaKeyedProperty,
                &[
                    Operand::Reg(u32::from(recv_temp)),
                    Operand::Reg(u32::from(key_temp)),
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode StaKeyedProperty (destruct computed): {err:?}"
                ))
            })?;
        Ok(())
    })();
    ctx.release_temps(3);
    lower
}

/// Routes an already-loaded accumulator value to an
/// `AssignmentTarget`. Used by destructuring-assignment elements
/// + rest targets.
fn assign_destructured_target_from_assignment_target<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    target: &'a AssignmentTarget<'a>,
) -> Result<(), SourceLoweringError> {
    match target {
        AssignmentTarget::AssignmentTargetIdentifier(ident) => {
            assign_identifier_reference(builder, ctx, ident)
        }
        AssignmentTarget::StaticMemberExpression(member) => {
            assign_static_member(builder, ctx, member)
        }
        AssignmentTarget::ComputedMemberExpression(member) => {
            assign_computed_member(builder, ctx, member)
        }
        AssignmentTarget::ArrayAssignmentTarget(nested) => {
            let nested_src = ctx.acquire_temps(1)?;
            let lower = (|| -> Result<(), SourceLoweringError> {
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(nested_src))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (nested array destruct): {err:?}"
                        ))
                    })?;
                destructure_array_assignment_from_temp(builder, ctx, nested, nested_src)
            })();
            ctx.release_temps(1);
            lower
        }
        AssignmentTarget::ObjectAssignmentTarget(nested) => {
            let nested_src = ctx.acquire_temps(1)?;
            let lower = (|| -> Result<(), SourceLoweringError> {
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(nested_src))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (nested obj destruct): {err:?}"
                        ))
                    })?;
                destructure_object_assignment_from_temp(builder, ctx, nested, nested_src)
            })();
            ctx.release_temps(1);
            lower
        }
        other => Err(SourceLoweringError::unsupported(
            "destructuring_assignment_target",
            other.span(),
        )),
    }
}

pub(super) fn destructure_array_assignment_from_temp<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    pattern: &'a oxc_ast::ast::ArrayAssignmentTarget<'a>,
    src_temp: RegisterIndex,
) -> Result<(), SourceLoweringError> {
    let iter_temp = ctx.acquire_temps(1)?;
    let value_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
    let done_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(2))?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        builder
            .emit(Opcode::GetIterator, &[Operand::Reg(u32::from(src_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode GetIterator (array destruct): {err:?}"
                ))
            })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(iter_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (array destruct iter): {err:?}"))
            })?;
        builder.emit(Opcode::LdaFalse, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode LdaFalse (array destruct done): {err:?}"))
        })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(done_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (array destruct done): {err:?}"))
            })?;

        let try_start = builder.new_label();
        let try_end = builder.new_label();
        let close_handler = builder.new_label();
        let after_try = builder.new_label();
        builder.bind_label(try_start).map_err(|err| {
            SourceLoweringError::Internal(format!("bind array destruct try_start: {err:?}"))
        })?;

        for element in pattern.elements.iter() {
            let done_label = builder.new_label();
            let value_ready = builder.new_label();
            builder
                .emit(
                    Opcode::IteratorStep,
                    &[
                        Operand::Reg(u32::from(value_temp)),
                        Operand::Reg(u32::from(iter_temp)),
                    ],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode IteratorStep (array destruct): {err:?}"
                    ))
                })?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(done_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (array destruct done): {err:?}"
                    ))
                })?;
            let jmp_pc = builder
                .emit_jump_to(Opcode::JumpIfToBooleanTrue, done_label)
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode JumpIfToBooleanTrue (array destruct done): {err:?}"
                    ))
                })?;
            ctx.attach_branch_feedback(builder, jmp_pc);
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(value_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Ldar (array destruct value): {err:?}"
                    ))
                })?;
            builder
                .emit_jump_to(Opcode::Jump, value_ready)
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Jump (array destruct value ready): {err:?}"
                    ))
                })?;
            builder.bind_label(done_label).map_err(|err| {
                SourceLoweringError::Internal(format!("bind array destruct done: {err:?}"))
            })?;
            builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode LdaUndefined (array destruct done): {err:?}"
                ))
            })?;
            builder.bind_label(value_ready).map_err(|err| {
                SourceLoweringError::Internal(format!("bind array destruct value_ready: {err:?}"))
            })?;
            if let Some(elem) = element.as_ref() {
                assign_destructured_target(builder, ctx, elem)?;
            }
        }

        if let Some(rest) = pattern.rest.as_deref() {
            let rest_target = ctx.acquire_temps(1)?;
            let rest_lower = (|| -> Result<(), SourceLoweringError> {
                builder.emit(Opcode::CreateArray, &[]).map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode CreateArray (array destruct rest): {err:?}"
                    ))
                })?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(rest_target))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (array destruct rest): {err:?}"
                        ))
                    })?;
                let rest_loop = builder.new_label();
                let rest_done = builder.new_label();
                builder.bind_label(rest_loop).map_err(|err| {
                    SourceLoweringError::Internal(format!("bind array destruct rest loop: {err:?}"))
                })?;
                builder
                    .emit(
                        Opcode::IteratorStep,
                        &[
                            Operand::Reg(u32::from(value_temp)),
                            Operand::Reg(u32::from(iter_temp)),
                        ],
                    )
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode IteratorStep (array destruct rest): {err:?}"
                        ))
                    })?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(done_temp))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (array destruct rest done): {err:?}"
                        ))
                    })?;
                let jmp_pc = builder
                    .emit_jump_to(Opcode::JumpIfToBooleanTrue, rest_done)
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode JumpIfToBooleanTrue (array destruct rest): {err:?}"
                        ))
                    })?;
                ctx.attach_branch_feedback(builder, jmp_pc);
                builder
                    .emit(Opcode::Ldar, &[Operand::Reg(u32::from(value_temp))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Ldar (array destruct rest value): {err:?}"
                        ))
                    })?;
                builder
                    .emit(Opcode::ArrayPush, &[Operand::Reg(u32::from(rest_target))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode ArrayPush (array destruct rest): {err:?}"
                        ))
                    })?;
                builder
                    .emit_jump_to(Opcode::Jump, rest_loop)
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Jump (array destruct rest loop): {err:?}"
                        ))
                    })?;
                builder.bind_label(rest_done).map_err(|err| {
                    SourceLoweringError::Internal(format!("bind array destruct rest done: {err:?}"))
                })?;
                builder
                    .emit(Opcode::Ldar, &[Operand::Reg(u32::from(rest_target))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Ldar (array destruct rest result): {err:?}"
                        ))
                    })?;
                assign_destructured_target_from_assignment_target(builder, ctx, &rest.target)?;
                Ok(())
            })();
            ctx.release_temps(1);
            rest_lower?;
        }

        builder.bind_label(try_end).map_err(|err| {
            SourceLoweringError::Internal(format!("bind array destruct try_end: {err:?}"))
        })?;
        builder
            .emit_jump_to(Opcode::Jump, after_try)
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Jump (array destruct after): {err:?}"
                ))
            })?;
        ctx.record_exception_handler(try_start, try_end, close_handler);

        builder.bind_label(close_handler).map_err(|err| {
            SourceLoweringError::Internal(format!("bind array destruct close_handler: {err:?}"))
        })?;
        let skip_close = builder.new_label();
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(done_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Ldar (array destruct close done): {err:?}"
                ))
            })?;
        let jmp_pc = builder
            .emit_jump_to(Opcode::JumpIfToBooleanTrue, skip_close)
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode JumpIfToBooleanTrue (array destruct skip close): {err:?}"
                ))
            })?;
        ctx.attach_branch_feedback(builder, jmp_pc);
        builder
            .emit(Opcode::IteratorClose, &[Operand::Reg(u32::from(iter_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode IteratorClose (array destruct): {err:?}"
                ))
            })?;
        builder.bind_label(skip_close).map_err(|err| {
            SourceLoweringError::Internal(format!("bind array destruct skip_close: {err:?}"))
        })?;
        builder.emit(Opcode::ReThrow, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode ReThrow (array destruct): {err:?}"))
        })?;
        builder.bind_label(after_try).map_err(|err| {
            SourceLoweringError::Internal(format!("bind array destruct after_try: {err:?}"))
        })?;
        Ok(())
    })();
    ctx.release_temps(3);
    lower
}

pub(super) fn destructure_array_assignment_from_temp_indexed<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    pattern: &'a oxc_ast::ast::ArrayAssignmentTarget<'a>,
    src_temp: RegisterIndex,
) -> Result<(), SourceLoweringError> {
    for (index, element) in pattern.elements.iter().enumerate() {
        let Some(elem) = element.as_ref() else {
            continue;
        };
        let idx_i32 = i32::try_from(index).map_err(|_| {
            SourceLoweringError::Internal("nested array destruct assign index overflow".into())
        })?;
        builder
            .emit(Opcode::LdaSmi, &[Operand::Imm(idx_i32)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode LdaSmi (nested array destruct): {err:?}"
                ))
            })?;
        builder
            .emit(
                Opcode::LdaKeyedProperty,
                &[Operand::Reg(u32::from(src_temp))],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode LdaKeyedProperty (nested array destruct): {err:?}"
                ))
            })?;
        assign_destructured_target(builder, ctx, elem)?;
    }
    if let Some(rest) = pattern.rest.as_deref() {
        let slice_target = ctx.acquire_temps(1)?;
        let slice_lower = (|| -> Result<(), SourceLoweringError> {
            emit_array_rest_slice(builder, ctx, src_temp, pattern.elements.len(), slice_target)?;
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(slice_target))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Ldar (array destruct rest): {err:?}"
                    ))
                })?;
            assign_destructured_target_from_assignment_target(builder, ctx, &rest.target)?;
            Ok(())
        })();
        ctx.release_temps(1);
        slice_lower?;
    }
    Ok(())
}

pub(super) fn destructure_object_assignment_from_temp<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    pattern: &'a oxc_ast::ast::ObjectAssignmentTarget<'a>,
    src_temp: RegisterIndex,
) -> Result<(), SourceLoweringError> {
    let excluded_base = if pattern.rest.is_some() && !pattern.properties.is_empty() {
        let count = RegisterIndex::try_from(pattern.properties.len()).map_err(|_| {
            SourceLoweringError::Internal("object rest exclusion count overflow".into())
        })?;
        Some(ctx.acquire_temps(count)?)
    } else {
        None
    };

    for (prop_index, prop) in pattern.properties.iter().enumerate() {
        let exclusion_slot = excluded_base.map(|base| base + prop_index as RegisterIndex);
        match prop {
            oxc_ast::ast::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(p) => {
                let name = p.binding.name.as_str().to_owned();
                let key_idx = ctx.intern_property_name(&name)?;
                if let Some(slot) = exclusion_slot {
                    emit_string_literal_to_register(builder, ctx, &name, slot)?;
                }
                builder
                    .emit(
                        Opcode::LdaNamedProperty,
                        &[Operand::Reg(u32::from(src_temp)), Operand::Idx(key_idx)],
                    )
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode LdaNamedProperty (nested obj destruct): {err:?}"
                        ))
                    })?;
                if let Some(default_expr) = &p.init {
                    emit_default_for_destructured_leaf(builder, ctx, Some(default_expr))?;
                }
                assign_identifier_reference(builder, ctx, &p.binding)?;
            }
            oxc_ast::ast::AssignmentTargetProperty::AssignmentTargetPropertyProperty(kv) => {
                let (key_idx, key_is_computed, key_name_for_rest) = match &kv.name {
                    PropertyKey::StaticIdentifier(ident) => {
                        let name = ident.name.as_str().to_owned();
                        let idx = ctx.intern_property_name(&name)?;
                        (Some(idx), false, Some(name))
                    }
                    PropertyKey::StringLiteral(lit) => {
                        let name = lit.value.as_str().to_owned();
                        let idx = ctx.intern_property_name(&name)?;
                        (Some(idx), false, Some(name))
                    }
                    other => {
                        let key_temp = exclusion_slot.unwrap_or(ctx.acquire_temps(1)?);
                        let result = (|| -> Result<(), SourceLoweringError> {
                            lower_return_expression(builder, ctx, other.to_expression())?;
                            builder
                                .emit(Opcode::Star, &[Operand::Reg(u32::from(key_temp))])
                                .map_err(|err| {
                                    SourceLoweringError::Internal(format!(
                                        "encode Star (obj destruct key): {err:?}"
                                    ))
                                })?;
                            builder
                                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(key_temp))])
                                .map_err(|err| {
                                    SourceLoweringError::Internal(format!(
                                        "encode Ldar (obj destruct key): {err:?}"
                                    ))
                                })?;
                            builder
                                .emit(
                                    Opcode::LdaKeyedProperty,
                                    &[Operand::Reg(u32::from(src_temp))],
                                )
                                .map_err(|err| {
                                    SourceLoweringError::Internal(format!(
                                        "encode LdaKeyedProperty (obj destruct): {err:?}"
                                    ))
                                })?;
                            Ok(())
                        })();
                        if exclusion_slot.is_none() {
                            ctx.release_temps(1);
                        }
                        result?;
                        (None, true, None)
                    }
                };
                if let Some(name) = key_name_for_rest.as_ref()
                    && let Some(slot) = exclusion_slot
                {
                    emit_string_literal_to_register(builder, ctx, name, slot)?;
                }
                if !key_is_computed && let Some(idx) = key_idx {
                    builder
                        .emit(
                            Opcode::LdaNamedProperty,
                            &[Operand::Reg(u32::from(src_temp)), Operand::Idx(idx)],
                        )
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode LdaNamedProperty (nested obj destruct kv): {err:?}"
                            ))
                        })?;
                }
                assign_destructured_target(builder, ctx, &kv.binding)?;
            }
        }
    }
    if let Some(rest) = pattern.rest.as_deref() {
        let rest_target = ctx.acquire_temps(1)?;
        let rest_lower = (|| -> Result<(), SourceLoweringError> {
            emit_object_rest_copy(
                builder,
                src_temp,
                excluded_base.map(|base| (base, pattern.properties.len())),
                rest_target,
            )?;
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(rest_target))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Ldar (obj destruct rest): {err:?}"
                    ))
                })?;
            assign_destructured_target_from_assignment_target(builder, ctx, &rest.target)?;
            Ok(())
        })();
        ctx.release_temps(1);
        rest_lower?;
    }
    if excluded_base.is_some() {
        let count = RegisterIndex::try_from(pattern.properties.len()).map_err(|_| {
            SourceLoweringError::Internal("object rest exclusion count overflow".into())
        })?;
        ctx.release_temps(count);
    }
    Ok(())
}

/// Assigns acc to an existing identifier reference —
/// `lower_identifier_assignment`'s core work without the
/// compound-operator logic. Used by destructuring assignment.
fn assign_identifier_reference<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    ident: &'a IdentifierReference<'a>,
) -> Result<(), SourceLoweringError> {
    let name = ident.name.as_str();
    let Some(binding) = ctx.resolve_identifier(name) else {
        // Unresolved identifier target — the destructuring leaf
        // refers to a global reference (§9.1.1.4 + §13.15.2). Plain
        // assignment already routes through `StaGlobal` for this
        // case, so keep destructuring symmetric: `[v2, vNull] = vals`
        // at the top level stores each leaf on `globalThis` via
        // StaGlobal rather than rejecting the whole pattern.
        let prop_idx = ctx.intern_property_name(name)?;
        builder
            .emit(Opcode::StaGlobal, &[Operand::Idx(prop_idx)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode StaGlobal (destructured ident target): {err:?}"
                ))
            })?;
        return Ok(());
    };
    match binding {
        BindingRef::Param { reg }
        | BindingRef::Local {
            reg,
            initialized: true,
            is_const: false,
            runtime_tdz: false,
            ..
        } => {
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(reg))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (destruct ident target): {err:?}"
                    ))
                })?;
            Ok(())
        }
        BindingRef::Local {
            reg,
            is_const: false,
            runtime_tdz: true,
            ..
        } => {
            emit_assert_binding_ready_for_write(
                builder,
                binding,
                ident.span,
                "destruct ident target",
            )?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(reg))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (destruct ident target): {err:?}"
                    ))
                })?;
            Ok(())
        }
        BindingRef::Local { is_const: true, .. } => Err(SourceLoweringError::unsupported(
            "const_assignment",
            ident.span,
        )),
        BindingRef::Local {
            initialized: false, ..
        } => Err(SourceLoweringError::unsupported(
            "tdz_self_reference",
            ident.span,
        )),
        BindingRef::Upvalue {
            idx,
            is_const: false,
        } => {
            emit_assert_binding_ready_for_write(
                builder,
                binding,
                ident.span,
                "destruct ident target",
            )?;
            builder
                .emit(Opcode::StaUpvalue, &[Operand::Idx(u32::from(idx))])
                .map(|_| ())
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode StaUpvalue (destruct ident target): {err:?}"
                    ))
                })
        }
        BindingRef::Upvalue { is_const: true, .. } => Err(SourceLoweringError::unsupported(
            "const_assignment",
            ident.span,
        )),
    }
}

fn lower_identifier_assignment<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    expr: &AssignmentExpression<'a>,
    ident: &IdentifierReference<'a>,
) -> Result<(), SourceLoweringError> {
    let target_ident = ident.name.as_str();
    let target_span = ident.span;
    let Some(binding) = ctx.resolve_identifier(target_ident) else {
        // Unresolved identifier → the assignment targets a global
        // reference (§13.15.2 + §9.1.1.4 GlobalEnvironmentRecord).
        // For plain `=` we skip the read and just StaGlobal; for
        // compound / logical we still need to GetValue(ref) first —
        // which is exactly LdaGlobal's semantics (throws ReferenceError
        // when the binding is genuinely absent on the global object).
        let prop_idx = ctx.intern_property_name(target_ident)?;
        if let Some(kind) = logical_assignment::classify(expr.operator) {
            builder
                .emit(Opcode::LdaGlobal, &[Operand::Idx(prop_idx)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaGlobal (logical global lhs): {err:?}"
                    ))
                })?;
            let end_label = builder.new_label();
            logical_assignment::emit_short_circuit_jump(builder, ctx, kind, end_label)?;
            lower_return_expression(builder, ctx, &expr.right)?;
            builder
                .emit(Opcode::StaGlobal, &[Operand::Idx(prop_idx)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode StaGlobal (logical global): {err:?}"
                    ))
                })?;
            builder.bind_label(end_label).map_err(|err| {
                SourceLoweringError::Internal(format!("bind logical global end: {err:?}"))
            })?;
            return Ok(());
        }
        if expr.operator == AssignmentOperator::Assign {
            lower_return_expression(builder, ctx, &expr.right)?;
            builder
                .emit(Opcode::StaGlobal, &[Operand::Idx(prop_idx)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode StaGlobal (assignment): {err:?}"))
                })?;
            return Ok(());
        }
        let bin_op = compound_assign_to_binary_operator(expr.operator).ok_or_else(|| {
            SourceLoweringError::unsupported(assignment_operator_tag(expr.operator), expr.span)
        })?;
        let encoding = binary_op_encoding(bin_op).ok_or_else(|| {
            SourceLoweringError::Internal(format!(
                "compound assignment {bin_op:?} has no binary opcode encoding"
            ))
        })?;
        builder
            .emit(Opcode::LdaGlobal, &[Operand::Idx(prop_idx)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode LdaGlobal (compound global lhs): {err:?}"
                ))
            })?;
        apply_binary_op_with_acc_lhs(builder, ctx, &encoding, &expr.right)?;
        builder
            .emit(Opcode::StaGlobal, &[Operand::Idx(prop_idx)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode StaGlobal (compound global): {err:?}"
                ))
            })?;
        return Ok(());
    };

    // M25: assignment to an upvalue target goes through
    // `StaUpvalue` — a different shape from the register-based
    // path, so handle it separately.
    if let BindingRef::Upvalue {
        idx,
        is_const: false,
    } = binding
    {
        // Logical compound (`||=`, `&&=`, `??=`) short-circuits on
        // the LHS and therefore needs a gated store — the StaUpvalue
        // below must run only on the fall-through path.
        if let Some(kind) = logical_assignment::classify(expr.operator) {
            emit_assert_binding_ready_for_write(
                builder,
                binding,
                target_span,
                "assign upvalue (logical)",
            )?;
            emit_load_binding_value(builder, binding, target_span, "logical upvalue lhs")?;
            let end_label = builder.new_label();
            logical_assignment::emit_short_circuit_jump(builder, ctx, kind, end_label)?;
            lower_return_expression(builder, ctx, &expr.right)?;
            builder
                .emit(Opcode::StaUpvalue, &[Operand::Idx(u32::from(idx))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode StaUpvalue (logical): {err:?}"))
                })?;
            builder.bind_label(end_label).map_err(|err| {
                SourceLoweringError::Internal(format!("bind logical upvalue end: {err:?}"))
            })?;
            return Ok(());
        }
        if expr.operator == AssignmentOperator::Assign {
            emit_assert_binding_ready_for_write(builder, binding, target_span, "assign upvalue")?;
            lower_return_expression(builder, ctx, &expr.right)?;
        } else {
            let bin_op = compound_assign_to_binary_operator(expr.operator).ok_or_else(|| {
                SourceLoweringError::unsupported(assignment_operator_tag(expr.operator), expr.span)
            })?;
            let encoding = binary_op_encoding(bin_op).ok_or_else(|| {
                SourceLoweringError::Internal(format!(
                    "compound assignment {bin_op:?} has no binary opcode encoding"
                ))
            })?;
            emit_load_binding_value(builder, binding, target_span, "compound upvalue lhs")?;
            apply_binary_op_with_acc_lhs(builder, ctx, &encoding, &expr.right)?;
        }
        builder
            .emit(Opcode::StaUpvalue, &[Operand::Idx(u32::from(idx))])
            .map_err(|err| SourceLoweringError::Internal(format!("encode StaUpvalue: {err:?}")))?;
        return Ok(());
    }
    if let BindingRef::Upvalue { is_const: true, .. } = binding {
        return Err(SourceLoweringError::unsupported(
            "const_assignment",
            target_span,
        ));
    }

    let target_reg = match binding {
        BindingRef::Local {
            reg,
            initialized: true,
            is_const: false,
            runtime_tdz: false,
            ..
        } => reg,
        BindingRef::Local {
            reg,
            runtime_tdz: true,
            ..
        } => {
            emit_assert_binding_ready_for_write(builder, binding, target_span, "assignment lhs")?;
            reg
        }
        BindingRef::Local { is_const: true, .. } => {
            return Err(SourceLoweringError::unsupported(
                "const_assignment",
                target_span,
            ));
        }
        BindingRef::Local {
            initialized: false, ..
        } => {
            return Err(SourceLoweringError::unsupported(
                "tdz_self_reference",
                target_span,
            ));
        }
        // Parameters are ordinary writable bindings in
        // non-strict mode (§10.2.1 FunctionDeclarationInstantiation
        // puts them on the function's VariableEnvironment with
        // `mutable: true`). Assignment writes back into the
        // parameter slot.
        BindingRef::Param { reg } => reg,
        BindingRef::Upvalue { .. } => unreachable!("handled above"),
    };

    // Logical compound (`||=`, `&&=`, `??=`) gates the Star on the
    // short-circuit outcome, so it can't reuse the unconditional
    // Star below.
    if let Some(kind) = logical_assignment::classify(expr.operator) {
        if matches!(
            binding,
            BindingRef::Local {
                initialized: true,
                ..
            }
        ) {
            let ldar_pc = builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(target_reg))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Ldar (logical lhs): {err:?}"))
                })?;
            let ldar_slot = ctx.allocate_arithmetic_feedback();
            builder.attach_feedback(ldar_pc, ldar_slot);
        } else {
            emit_load_binding_value(builder, binding, target_span, "logical lhs")?;
        }
        let end_label = builder.new_label();
        logical_assignment::emit_short_circuit_jump(builder, ctx, kind, end_label)?;
        lower_return_expression(builder, ctx, &expr.right)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(target_reg))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (logical): {err:?}"))
            })?;
        builder.bind_label(end_label).map_err(|err| {
            SourceLoweringError::Internal(format!("bind logical local end: {err:?}"))
        })?;
        return Ok(());
    }

    if expr.operator == AssignmentOperator::Assign {
        lower_return_expression(builder, ctx, &expr.right)?;
    } else {
        let bin_op = compound_assign_to_binary_operator(expr.operator).ok_or_else(|| {
            SourceLoweringError::unsupported(assignment_operator_tag(expr.operator), expr.span)
        })?;
        let encoding = binary_op_encoding(bin_op).ok_or_else(|| {
            SourceLoweringError::Internal(format!(
                "compound assignment {bin_op:?} has no binary opcode encoding"
            ))
        })?;
        if matches!(
            binding,
            BindingRef::Local {
                initialized: true,
                ..
            }
        ) {
            let ldar_pc = builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(target_reg))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Ldar (compound lhs): {err:?}"))
                })?;
            let ldar_slot = ctx.allocate_arithmetic_feedback();
            builder.attach_feedback(ldar_pc, ldar_slot);
        } else {
            emit_load_binding_value(builder, binding, target_span, "compound lhs")?;
        }
        apply_binary_op_with_acc_lhs(builder, ctx, &encoding, &expr.right)?;
    }

    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(target_reg))])
        .map_err(|err| SourceLoweringError::Internal(format!("encode Star: {err:?}")))?;
    Ok(())
}

/// Lowers `o.x = v` (or `o.x <op>= v`). Shape for plain `=`:
///
/// ```text
///   <materialize base into r_base>
///   <lower v into acc>
///   StaNamedProperty r_base, name_idx
/// ```
///
/// Compound `<op>=` (`+=`, `-=`, `*=`, `|=`):
///
/// ```text
///   <materialize base into r_base>
///   LdaNamedProperty r_base, name_idx   ; acc = o.x
///   <apply_binary_op_with_acc_lhs>       ; acc = o.x <op> v
///   StaNamedProperty r_base, name_idx    ; o.x = acc
/// ```
///
/// The accumulator holds the assigned value on exit, so composed
/// forms (`let y = o.x = 5;`) work without extra traffic.
fn lower_static_member_assignment<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    expr: &AssignmentExpression<'a>,
    member: &StaticMemberExpression<'a>,
) -> Result<(), SourceLoweringError> {
    if member.optional {
        return Err(SourceLoweringError::unsupported(
            "parser_recovery_optional_member_assignment",
            member.span,
        ));
    }
    // M28: `super.x = v` / `super.x <op>= v`. The super base is not
    // materialised into a regular register; instead the LHS read
    // goes through `GetSuperProperty` and the store through
    // `SetSuperProperty`. Receiver register holds the current
    // frame's `this`.
    if matches!(&member.object, Expression::Super(_)) {
        enforce_super_property_binding(ctx, &member.object)?;
        let receiver_temp = ctx.acquire_temps(1)?;
        let idx = ctx.intern_property_name(member.property.name.as_str())?;
        let lower = (|| -> Result<(), SourceLoweringError> {
            builder.emit(Opcode::LdaThis, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaThis (super.x write): {err:?}"))
            })?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (super.x receiver): {err:?}"
                    ))
                })?;
            if let Some(kind) = logical_assignment::classify(expr.operator) {
                builder
                    .emit(
                        Opcode::GetSuperProperty,
                        &[Operand::Reg(u32::from(receiver_temp)), Operand::Idx(idx)],
                    )
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode GetSuperProperty (logical lhs): {err:?}"
                        ))
                    })?;
                let end_label = builder.new_label();
                logical_assignment::emit_short_circuit_jump(builder, ctx, kind, end_label)?;
                lower_return_expression(builder, ctx, &expr.right)?;
                builder
                    .emit(
                        Opcode::SetSuperProperty,
                        &[Operand::Reg(u32::from(receiver_temp)), Operand::Idx(idx)],
                    )
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode SetSuperProperty (logical): {err:?}"
                        ))
                    })?;
                builder.bind_label(end_label).map_err(|err| {
                    SourceLoweringError::Internal(format!("bind logical super.x end: {err:?}"))
                })?;
                return Ok(());
            }
            if expr.operator == AssignmentOperator::Assign {
                lower_return_expression(builder, ctx, &expr.right)?;
            } else {
                let bin_op =
                    compound_assign_to_binary_operator(expr.operator).ok_or_else(|| {
                        SourceLoweringError::unsupported(
                            assignment_operator_tag(expr.operator),
                            expr.span,
                        )
                    })?;
                let encoding = binary_op_encoding(bin_op).ok_or_else(|| {
                    SourceLoweringError::Internal(format!(
                        "compound assignment {bin_op:?} has no binary opcode encoding"
                    ))
                })?;
                builder
                    .emit(
                        Opcode::GetSuperProperty,
                        &[Operand::Reg(u32::from(receiver_temp)), Operand::Idx(idx)],
                    )
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode GetSuperProperty (compound lhs): {err:?}"
                        ))
                    })?;
                apply_binary_op_with_acc_lhs(builder, ctx, &encoding, &expr.right)?;
            }
            builder
                .emit(
                    Opcode::SetSuperProperty,
                    &[Operand::Reg(u32::from(receiver_temp)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode SetSuperProperty: {err:?}"))
                })?;
            Ok(())
        })();
        ctx.release_temps(1);
        return lower;
    }
    let base = materialize_member_base(builder, ctx, &member.object)?;
    let idx = ctx.intern_property_name(member.property.name.as_str())?;

    let lower = (|| -> Result<(), SourceLoweringError> {
        if let Some(kind) = logical_assignment::classify(expr.operator) {
            builder
                .emit(
                    Opcode::LdaNamedProperty,
                    &[Operand::Reg(u32::from(base.reg)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaNamedProperty (logical lhs): {err:?}"
                    ))
                })?;
            let end_label = builder.new_label();
            logical_assignment::emit_short_circuit_jump(builder, ctx, kind, end_label)?;
            lower_return_expression(builder, ctx, &expr.right)?;
            let sta_pc = builder
                .emit(
                    Opcode::StaNamedProperty,
                    &[Operand::Reg(u32::from(base.reg)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode StaNamedProperty (logical): {err:?}"
                    ))
                })?;
            ctx.attach_property_store_feedback(builder, sta_pc);
            builder.bind_label(end_label).map_err(|err| {
                SourceLoweringError::Internal(format!("bind logical o.x end: {err:?}"))
            })?;
            return Ok(());
        }
        if expr.operator == AssignmentOperator::Assign {
            lower_return_expression(builder, ctx, &expr.right)?;
        } else {
            let bin_op = compound_assign_to_binary_operator(expr.operator).ok_or_else(|| {
                SourceLoweringError::unsupported(assignment_operator_tag(expr.operator), expr.span)
            })?;
            let encoding = binary_op_encoding(bin_op).ok_or_else(|| {
                SourceLoweringError::Internal(format!(
                    "compound assignment {bin_op:?} has no binary opcode encoding"
                ))
            })?;
            builder
                .emit(
                    Opcode::LdaNamedProperty,
                    &[Operand::Reg(u32::from(base.reg)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaNamedProperty (compound): {err:?}"
                    ))
                })?;
            apply_binary_op_with_acc_lhs(builder, ctx, &encoding, &expr.right)?;
        }
        let sta_pc = builder
            .emit(
                Opcode::StaNamedProperty,
                &[Operand::Reg(u32::from(base.reg)), Operand::Idx(idx)],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode StaNamedProperty: {err:?}"))
            })?;
        ctx.attach_property_store_feedback(builder, sta_pc);
        Ok(())
    })();
    if base.temp_count != 0 {
        ctx.release_temps(base.temp_count);
    }
    lower
}

/// Lowers `o[k] = v` (or `o[k] <op>= v`). Shape for plain `=`:
///
/// ```text
///   <materialize base into r_base>
///   <lower key into acc>; Star r_key
///   <lower v into acc>
///   StaKeyedProperty r_base, r_key
/// ```
///
/// Compound `<op>=`:
///
/// ```text
///   <materialize base into r_base>
///   <lower key into acc>; Star r_key
///   Ldar r_key                       ; acc = key
///   LdaKeyedProperty r_base          ; acc = r_base[key]
///   <apply_binary_op_with_acc_lhs>   ; acc = old <op> v
///   StaKeyedProperty r_base, r_key
/// ```
///
/// The key always spills into a dedicated temp so both the read
/// path (which needs key in acc) and the store path (which needs
/// key in a register via `StaKeyedProperty`'s second operand) can
/// reach it.
fn lower_computed_member_assignment<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    expr: &AssignmentExpression<'a>,
    member: &ComputedMemberExpression<'a>,
) -> Result<(), SourceLoweringError> {
    if member.optional {
        return Err(SourceLoweringError::unsupported(
            "parser_recovery_optional_member_assignment",
            member.span,
        ));
    }
    // M28: `super[k] = v` / `super[k] <op>= v`. Receiver is `this`;
    // key is spilled to a dedicated temp; writes go through
    // `SetSuperPropertyComputed`.
    if matches!(&member.object, Expression::Super(_)) {
        enforce_super_property_binding(ctx, &member.object)?;
        let receiver_temp = ctx.acquire_temps(1)?;
        let key_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
        let lower = (|| -> Result<(), SourceLoweringError> {
            builder.emit(Opcode::LdaThis, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaThis (super[k] write): {err:?}"))
            })?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (super[k] receiver): {err:?}"
                    ))
                })?;
            lower_return_expression(builder, ctx, &member.expression)?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(key_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Star (super[k] key): {err:?}"))
                })?;
            if let Some(kind) = logical_assignment::classify(expr.operator) {
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
                            "encode GetSuperPropertyComputed (logical lhs): {err:?}"
                        ))
                    })?;
                let end_label = builder.new_label();
                logical_assignment::emit_short_circuit_jump(builder, ctx, kind, end_label)?;
                lower_return_expression(builder, ctx, &expr.right)?;
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
                            "encode SetSuperPropertyComputed (logical): {err:?}"
                        ))
                    })?;
                builder.bind_label(end_label).map_err(|err| {
                    SourceLoweringError::Internal(format!("bind logical super[k] end: {err:?}"))
                })?;
                return Ok(());
            }
            if expr.operator == AssignmentOperator::Assign {
                lower_return_expression(builder, ctx, &expr.right)?;
            } else {
                let bin_op =
                    compound_assign_to_binary_operator(expr.operator).ok_or_else(|| {
                        SourceLoweringError::unsupported(
                            assignment_operator_tag(expr.operator),
                            expr.span,
                        )
                    })?;
                let encoding = binary_op_encoding(bin_op).ok_or_else(|| {
                    SourceLoweringError::Internal(format!(
                        "compound assignment {bin_op:?} has no binary opcode encoding"
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
                            "encode GetSuperPropertyComputed (compound lhs): {err:?}"
                        ))
                    })?;
                apply_binary_op_with_acc_lhs(builder, ctx, &encoding, &expr.right)?;
            }
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
                        "encode SetSuperPropertyComputed: {err:?}"
                    ))
                })?;
            Ok(())
        })();
        ctx.release_temps(2);
        return lower;
    }
    let base = materialize_member_base(builder, ctx, &member.object)?;
    let key_temp = ctx.acquire_temps(1)?;

    let lower = (|| -> Result<(), SourceLoweringError> {
        // Evaluate the key into its own temp — JS spec §13.15.2
        // specifies left-to-right evaluation for `o[k] = v`.
        lower_return_expression(builder, ctx, &member.expression)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(key_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (computed key spill): {err:?}"))
            })?;

        if let Some(kind) = logical_assignment::classify(expr.operator) {
            // Reload key into acc for LdaKeyedProperty.
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(key_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Ldar (computed logical key): {err:?}"
                    ))
                })?;
            builder
                .emit(
                    Opcode::LdaKeyedProperty,
                    &[Operand::Reg(u32::from(base.reg))],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaKeyedProperty (logical lhs): {err:?}"
                    ))
                })?;
            let end_label = builder.new_label();
            logical_assignment::emit_short_circuit_jump(builder, ctx, kind, end_label)?;
            lower_return_expression(builder, ctx, &expr.right)?;
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
                        "encode StaKeyedProperty (logical): {err:?}"
                    ))
                })?;
            builder.bind_label(end_label).map_err(|err| {
                SourceLoweringError::Internal(format!("bind logical o[k] end: {err:?}"))
            })?;
            return Ok(());
        }

        if expr.operator == AssignmentOperator::Assign {
            lower_return_expression(builder, ctx, &expr.right)?;
        } else {
            let bin_op = compound_assign_to_binary_operator(expr.operator).ok_or_else(|| {
                SourceLoweringError::unsupported(assignment_operator_tag(expr.operator), expr.span)
            })?;
            let encoding = binary_op_encoding(bin_op).ok_or_else(|| {
                SourceLoweringError::Internal(format!(
                    "compound assignment {bin_op:?} has no binary opcode encoding"
                ))
            })?;
            // Reload key into acc for LdaKeyedProperty.
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(key_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Ldar (computed compound key): {err:?}"
                    ))
                })?;
            builder
                .emit(
                    Opcode::LdaKeyedProperty,
                    &[Operand::Reg(u32::from(base.reg))],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaKeyedProperty (compound): {err:?}"
                    ))
                })?;
            apply_binary_op_with_acc_lhs(builder, ctx, &encoding, &expr.right)?;
        }
        builder
            .emit(
                Opcode::StaKeyedProperty,
                &[
                    Operand::Reg(u32::from(base.reg)),
                    Operand::Reg(u32::from(key_temp)),
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode StaKeyedProperty: {err:?}"))
            })?;
        Ok(())
    })();
    ctx.release_temps(1); // key_temp
    if base.temp_count != 0 {
        ctx.release_temps(base.temp_count);
    }
    lower
}

/// M29: lowers `obj.#name = v` / `obj.#name <op>= v` onto
/// `SetPrivateField`. Accumulator holds the value on exit (JS
/// assignment value is the RHS), so compound assignments compose
/// cleanly via `apply_binary_op_with_acc_lhs`.
fn lower_private_field_assignment<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    expr: &'a AssignmentExpression<'a>,
    member: &'a oxc_ast::ast::PrivateFieldExpression<'a>,
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
        if let Some(kind) = logical_assignment::classify(expr.operator) {
            builder
                .emit(
                    Opcode::GetPrivateField,
                    &[Operand::Reg(u32::from(base.reg)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode GetPrivateField (logical lhs): {err:?}"
                    ))
                })?;
            let end_label = builder.new_label();
            logical_assignment::emit_short_circuit_jump(builder, ctx, kind, end_label)?;
            lower_return_expression(builder, ctx, &expr.right)?;
            builder
                .emit(
                    Opcode::SetPrivateField,
                    &[Operand::Reg(u32::from(base.reg)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode SetPrivateField (logical): {err:?}"
                    ))
                })?;
            builder.bind_label(end_label).map_err(|err| {
                SourceLoweringError::Internal(format!("bind logical #field end: {err:?}"))
            })?;
            return Ok(());
        }
        if expr.operator == AssignmentOperator::Assign {
            lower_return_expression(builder, ctx, &expr.right)?;
        } else {
            let bin_op = compound_assign_to_binary_operator(expr.operator).ok_or_else(|| {
                SourceLoweringError::unsupported(assignment_operator_tag(expr.operator), expr.span)
            })?;
            let encoding = binary_op_encoding(bin_op).ok_or_else(|| {
                SourceLoweringError::Internal(format!(
                    "compound assignment {bin_op:?} has no binary opcode encoding"
                ))
            })?;
            builder
                .emit(
                    Opcode::GetPrivateField,
                    &[Operand::Reg(u32::from(base.reg)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode GetPrivateField (compound): {err:?}"
                    ))
                })?;
            apply_binary_op_with_acc_lhs(builder, ctx, &encoding, &expr.right)?;
        }
        builder
            .emit(
                Opcode::SetPrivateField,
                &[Operand::Reg(u32::from(base.reg)), Operand::Idx(idx)],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode SetPrivateField: {err:?}"))
            })?;
        Ok(())
    })();
    if base.temp_count != 0 {
        ctx.release_temps(base.temp_count);
    }
    lower
}

/// Maps a compound assignment operator to the binary operator whose
/// encoding it should use. Returns `None` only for `=` (handled
/// separately — no underlying binary op) and for the short-circuit
/// logical compounds (`||=`, `&&=`, `??=`) which need guard-
/// evaluation semantics the regular binary lowering doesn't
/// provide.
fn compound_assign_to_binary_operator(op: AssignmentOperator) -> Option<BinaryOperator> {
    use AssignmentOperator as A;
    use BinaryOperator as B;
    Some(match op {
        A::Addition => B::Addition,
        A::Subtraction => B::Subtraction,
        A::Multiplication => B::Multiplication,
        A::Division => B::Division,
        A::Remainder => B::Remainder,
        A::Exponential => B::Exponential,
        A::ShiftLeft => B::ShiftLeft,
        A::ShiftRight => B::ShiftRight,
        A::ShiftRightZeroFill => B::ShiftRightZeroFill,
        A::BitwiseOR => B::BitwiseOR,
        A::BitwiseXOR => B::BitwiseXOR,
        A::BitwiseAnd => B::BitwiseAnd,
        _ => return None,
    })
}

/// Stable diagnostic tag for an assignment operator outside the M5
/// supported set. Mirrors [`binary_operator_tag`] in style so callers
/// don't have to round-trip through `Debug`.
fn assignment_operator_tag(op: AssignmentOperator) -> &'static str {
    use AssignmentOperator::*;
    match op {
        Assign => "assign",
        Addition => "addition_assign",
        Subtraction => "subtraction_assign",
        Multiplication => "multiplication_assign",
        Division => "division_assign",
        Remainder => "remainder_assign",
        Exponential => "exponential_assign",
        ShiftLeft => "shift_left_assign",
        ShiftRight => "shift_right_assign",
        ShiftRightZeroFill => "unsigned_shift_right_assign",
        BitwiseOR => "bitwise_or_assign",
        BitwiseXOR => "bitwise_xor_assign",
        BitwiseAnd => "bitwise_and_assign",
        LogicalOr => "logical_or_assign",
        LogicalAnd => "logical_and_assign",
        LogicalNullish => "logical_nullish_assign",
    }
}
