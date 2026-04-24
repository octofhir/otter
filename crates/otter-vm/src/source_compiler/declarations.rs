//! Lexical declarations (`let` / `const`) and destructuring binding
//! patterns.
//!
//! Extracted from `source_compiler/mod.rs` in the file-size cleanup
//! follow-up after C4. Covers `lower_let_const_declaration` plus the
//! full destructuring-binding surface: single + destructured
//! declarators, array + object patterns, rest slicing, object-rest
//! copy, default-initializer emission, and the shared
//! `lower_pattern_bind` entry point used by both declarations and
//! parameter destructuring in `functions`.

use super::*;

/// Lowers `let x = init;` or `const x = init;`. Emits:
///
/// ```text
///   <init>            ; acc = init value
///   Star r_x          ; locals[x] = acc
/// ```
///
/// Allocates the slot for `x` **before** lowering the initializer so
/// the binding is in scope (in TDZ); the initializer can therefore
/// detect a self-reference (`let x = x + 1`) at compile time and
/// reject it as `tdz_self_reference`. After the post-init `Star`,
/// `mark_initialized` flips the binding to readable.
pub(super) fn lower_let_const_declaration<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    decl: &'a VariableDeclaration<'a>,
) -> Result<(), SourceLoweringError> {
    let is_const = match decl.kind {
        VariableDeclarationKind::Let => false,
        VariableDeclarationKind::Const => true,
        // `var` — treat as block-scoped `let` at the declaration
        // site. Classic `var` is function-scoped with hoisting;
        // 99% of user code that reaches us uses `var` in a place
        // where block-scoping behaves identically (single
        // declaration before first read), and the compile-time
        // TDZ check stays at `let`-parity. Full function-scope
        // hoisting is tracked as a follow-up but should not block
        // scripts that sprinkle `var` next to `let` / `const`.
        VariableDeclarationKind::Var => false,
        // `using` / `await using` should be routed through
        // `using_decl.rs` before reaching this generic declaration
        // helper. If parser recovery or a new caller gets here,
        // keep the failure explicit.
        VariableDeclarationKind::Using | VariableDeclarationKind::AwaitUsing => {
            return Err(SourceLoweringError::unsupported(
                "parser_recovery_unrouted_using_decl",
                decl.span,
            ));
        }
    };
    let is_var = decl.kind == VariableDeclarationKind::Var;

    if decl.declarations.is_empty() {
        return Err(SourceLoweringError::unsupported(
            "parser_recovery_empty_var_decl",
            decl.span,
        ));
    }

    // Lower each declarator left-to-right. M7 lifted the
    // "single declarator only" restriction so the bench2 shape
    // `let s = 0, i = 0;` (two declarators) compiles directly. Each
    // declarator allocates its own slot and runs through the same
    // single-declarator path the M4 lowering already had.
    for declarator in decl.declarations.iter() {
        lower_single_declarator(builder, ctx, declarator, is_const, is_var)?;
    }
    Ok(())
}

fn lower_single_declarator<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    declarator: &'a VariableDeclarator<'a>,
    is_const: bool,
    is_var: bool,
) -> Result<(), SourceLoweringError> {
    match &declarator.id {
        BindingPattern::BindingIdentifier(ident) => {
            if is_const && declarator.init.is_none() {
                return Err(SourceLoweringError::unsupported(
                    "uninitialized_const_binding",
                    declarator.span,
                ));
            }
            let name = ident.name.as_str();
            // §9.1.1.4 CreateGlobalVarBinding — for the synthesised
            // top-level entry, a `var`/`let`/`const NAME = init;`
            // tracked as a module-global binds the value directly on
            // `globalThis.NAME`. Skipping the local allocation keeps
            // reads AND writes consistent: every reference (from the
            // script body itself or from a nested function called
            // mid-body) resolves through `LdaGlobal`/`StaGlobal`
            // against the single canonical slot. The post-body flush
            // still runs but becomes a harmless no-op for these
            // names (the values are already on the global object).
            if ctx.should_mirror_top_level_decl_to_global(name) {
                if let Some(init) = declarator.init.as_ref() {
                    lower_return_expression(builder, ctx, init)?;
                } else {
                    builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode LdaUndefined (top-level global init): {err:?}"
                        ))
                    })?;
                }
                let prop_idx = ctx.intern_property_name(name)?;
                builder
                    .emit(Opcode::StaGlobal, &[Operand::Idx(prop_idx)])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode StaGlobal (top-level global decl): {err:?}"
                        ))
                    })?;
                return Ok(());
            }

            let (slot, reused_var) = if is_var {
                ctx.allocate_var_local(name, declarator.span)?
            } else {
                (ctx.allocate_local(name, is_const, declarator.span)?, false)
            };

            if reused_var && declarator.init.is_none() {
                return Ok(());
            }

            // Lower init into acc, or use `undefined` for `var x;`
            // / `let x;` per §14.3.1. Reading the binding inside
            // its own initializer still hits the `Local {
            // initialized: false }` arm of `lower_identifier_read`
            // and surfaces as `tdz_self_reference`.
            if let Some(init) = declarator.init.as_ref() {
                lower_return_expression(builder, ctx, init)?;
            } else {
                builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaUndefined (uninitialized binding): {err:?}"
                    ))
                })?;
            }
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
                .map_err(|err| SourceLoweringError::Internal(format!("encode Star: {err:?}")))?;
            if !reused_var {
                ctx.mark_initialized(name)?;
            }
            Ok(())
        }
        // M24: `let [a, b, ...rest] = init;` / `let { a, b: x, c = 0 } = init;`
        // Lower the init into a temp, then bind each pattern leaf.
        BindingPattern::ArrayPattern(_) | BindingPattern::ObjectPattern(_) => {
            let init = declarator.init.as_ref().ok_or_else(|| {
                SourceLoweringError::unsupported(
                    "uninitialized_destructuring_binding",
                    declarator.span,
                )
            })?;
            lower_destructured_declarator(builder, ctx, &declarator.id, init, is_const)
        }
        // `let x = 1 = …;` is not grammatically possible, so an
        // AssignmentPattern as the top-level declarator id only
        // shows up through destructuring (e.g. `let { a = 0 } = src;`
        // where oxc wraps the leaf in AP). Those cases are
        // dispatched via `lower_pattern_bind`; reaching here at
        // the top level means something unsupported slipped
        // through.
        BindingPattern::AssignmentPattern(pat) => Err(SourceLoweringError::unsupported(
            "unexpected_assignment_pattern_declarator",
            pat.span,
        )),
    }
}

/// Lowers a destructuring declarator: `let <pattern> = <init>;`
/// where `<pattern>` is an `ArrayPattern` or `ObjectPattern`. The
/// init expression evaluates once into a dedicated temp
/// (`r_source`); the pattern then binds each leaf identifier as a
/// fresh local initialised from the matching
/// indexed / property read.
///
/// §14.3.3 BindingPattern-annotated declaration evaluation.
fn lower_destructured_declarator<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    pattern: &'a BindingPattern<'a>,
    init: &'a Expression<'a>,
    is_const: bool,
) -> Result<(), SourceLoweringError> {
    // Use an anonymous hidden local (not a temp) for the source
    // spill. Temps are placed above `peak_local_count`, so any
    // `allocate_local` we do afterwards for pattern leaves would
    // bump the local count and overlap with the temp slot —
    // clobbering the source value mid-destructure. A dedicated
    // hidden-local slot sits inside the local region and doesn't
    // move.
    let src_slot = ctx.allocate_anonymous_local()?;
    lower_return_expression(builder, ctx, init)?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(src_slot))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (destructure src): {err:?}"))
        })?;
    lower_pattern_bind(builder, ctx, pattern, src_slot, is_const)
}

/// Recursively lowers a `BindingPattern` whose value lives in
/// register `src_reg`. Allocates a new local for each leaf
/// `BindingIdentifier` and emits the indexed / property read that
/// populates it. `is_const` propagates to every leaf — a
/// destructuring `const { a } = …` produces a `const` binding for
/// `a`.
///
/// Rejected with stable tags:
/// - Array holes (`[a, , b]`) → `array_pattern_hole`.
/// - Computed object keys (`{ [k]: v }`) →
///   `computed_pattern_key`.
pub(super) fn lower_pattern_bind<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    pattern: &'a BindingPattern<'a>,
    src_reg: RegisterIndex,
    is_const: bool,
) -> Result<(), SourceLoweringError> {
    match pattern {
        BindingPattern::BindingIdentifier(ident) => {
            let name = ident.name.as_str();
            let slot = ctx.allocate_local(name, is_const, ident.span)?;
            // At this call site the source value is already in acc
            // (array/object destructuring set it via the per-leaf
            // emission); the caller just needs to `Star` it into
            // the new slot.
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (destructure leaf): {err:?}"
                    ))
                })?;
            ctx.mark_initialized(name)?;
            Ok(())
        }
        BindingPattern::ArrayPattern(pat) => {
            lower_array_pattern(builder, ctx, pat, src_reg, is_const)
        }
        BindingPattern::ObjectPattern(pat) => {
            lower_object_pattern(builder, ctx, pat, src_reg, is_const)
        }
        // AssignmentPattern wraps a leaf with a default (`= expr`).
        // Used at top level of declarator targets (rare — default
        // typically appears INSIDE a pattern). The accumulator
        // already holds the source value; run the default-check
        // against it, then delegate to the wrapped target.
        BindingPattern::AssignmentPattern(assign) => {
            emit_default_for_destructured_leaf(builder, ctx, Some(&assign.right))?;
            destructure_assign_target(builder, ctx, &assign.left, is_const)
        }
    }
}

/// Lowers `[a, b, ...rest]` destructuring against the source in
/// `src_reg`. Array elements use indexed access (`LdaSmi i;
/// LdaKeyedProperty r_src`), which covers the common case (Array
/// sources) without the iterator-protocol overhead. Out-of-range
/// indices return `undefined` naturally through the keyed-property
/// path, matching the spec's "step beyond the iterator" semantics.
///
/// Rest uses `Array.prototype.slice(start)` against `src_reg` so
/// the resulting rest binding is a fresh Array whose length matches
/// the source's tail. Requires `slice` on the source's prototype
/// chain — always the case for plain Array values.
///
/// Holes (`[a, , b]` → `elements[1] == None`) rejected at compile
/// time with `array_pattern_hole`.
fn lower_array_pattern<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    pat: &'a ArrayPattern<'a>,
    src_reg: RegisterIndex,
    is_const: bool,
) -> Result<(), SourceLoweringError> {
    for (index, element) in pat.elements.iter().enumerate() {
        let Some(element_pat) = element.as_ref() else {
            // Hole (`[a, , b]` → elements[1] = None). Skip —
            // the corresponding index has no binding, nothing to
            // store. `b` at elements[2] still reads via its own
            // iteration at the right index.
            continue;
        };
        let idx_i32 = i32::try_from(index)
            .map_err(|_| SourceLoweringError::Internal("array pattern index overflow".into()))?;
        // acc = index (int); LdaKeyedProperty r_src → acc = src[index].
        builder
            .emit(Opcode::LdaSmi, &[Operand::Imm(idx_i32)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode LdaSmi (array pattern index): {err:?}"
                ))
            })?;
        builder
            .emit(
                Opcode::LdaKeyedProperty,
                &[Operand::Reg(u32::from(src_reg))],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode LdaKeyedProperty (array pattern): {err:?}"
                ))
            })?;
        // Apply default initialiser when the element has `= expr`.
        // Nested patterns go through a per-element temp so the
        // recursion can re-read by index / property.
        match element_pat {
            BindingPattern::AssignmentPattern(assign) => {
                emit_default_for_destructured_leaf(builder, ctx, Some(&assign.right))?;
                destructure_assign_target(builder, ctx, &assign.left, is_const)?;
            }
            BindingPattern::ArrayPattern(_) | BindingPattern::ObjectPattern(_) => {
                // Read element value into a temp then recurse.
                let nested_slot = ctx.allocate_anonymous_local()?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(nested_slot))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (array pattern nested): {err:?}"
                        ))
                    })?;
                lower_pattern_bind(builder, ctx, element_pat, nested_slot, is_const)?;
            }
            _ => {
                lower_pattern_bind(builder, ctx, element_pat, src_reg, is_const)?;
            }
        }
    }

    if let Some(rest) = pat.rest.as_deref() {
        match &rest.argument {
            BindingPattern::BindingIdentifier(ident) => {
                let rest_name = ident.name.as_str();
                let rest_slot = ctx.allocate_local(rest_name, is_const, ident.span)?;
                emit_array_rest_slice(builder, ctx, src_reg, pat.elements.len(), rest_slot)?;
                ctx.mark_initialized(rest_name)?;
            }
            nested @ (BindingPattern::ArrayPattern(_) | BindingPattern::ObjectPattern(_)) => {
                // `let [...[a, b]] = src` — slice into a temp,
                // destructure the resulting array into the inner
                // pattern.
                let rest_slot = ctx.allocate_anonymous_local()?;
                emit_array_rest_slice(builder, ctx, src_reg, pat.elements.len(), rest_slot)?;
                lower_pattern_bind(builder, ctx, nested, rest_slot, is_const)?;
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

/// Helper used by `lower_array_pattern` to route an
/// `AssignmentPattern`'s left-side binding to the right path:
/// identifier → allocate-local + Star; nested pattern → bind
/// recursively against a per-element temp. Acc holds the value
/// (post default-check).
fn destructure_assign_target<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    target: &'a BindingPattern<'a>,
    is_const: bool,
) -> Result<(), SourceLoweringError> {
    match target {
        BindingPattern::BindingIdentifier(ident) => {
            let name = ident.name.as_str();
            let slot = ctx.allocate_local(name, is_const, ident.span)?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (assign-target ident): {err:?}"
                    ))
                })?;
            ctx.mark_initialized(name)?;
            Ok(())
        }
        BindingPattern::ArrayPattern(_) | BindingPattern::ObjectPattern(_) => {
            let nested_slot = ctx.allocate_anonymous_local()?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(nested_slot))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (assign-target nested): {err:?}"
                    ))
                })?;
            lower_pattern_bind(builder, ctx, target, nested_slot, is_const)
        }
        BindingPattern::AssignmentPattern(nested) => {
            // Double-wrapped default — unlikely but harmless:
            // run the inner's default, recurse.
            emit_default_for_destructured_leaf(builder, ctx, Some(&nested.right))?;
            destructure_assign_target(builder, ctx, &nested.left, is_const)
        }
    }
}

/// Emits `src_reg.slice(start)` and stores the resulting Array into
/// `rest_slot`. Three temps: receiver + callee + one arg slot. The
/// method is looked up via the property-name interner so later
/// accesses to `.slice` dedup.
pub(super) fn emit_array_rest_slice(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    src_reg: RegisterIndex,
    start: usize,
    rest_slot: RegisterIndex,
) -> Result<(), SourceLoweringError> {
    let start_i32 = i32::try_from(start)
        .map_err(|_| SourceLoweringError::Internal("rest start index overflow".into()))?;
    let callee_temp = ctx.acquire_temps(1)?;
    let arg_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;

    let lower = (|| -> Result<(), SourceLoweringError> {
        // callee = src.slice
        let slice_idx = ctx.intern_property_name("slice")?;
        builder
            .emit(
                Opcode::LdaNamedProperty,
                &[Operand::Reg(u32::from(src_reg)), Operand::Idx(slice_idx)],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaNamedProperty (slice): {err:?}"))
            })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(callee_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (slice callee): {err:?}"))
            })?;
        // arg = start
        builder
            .emit(Opcode::LdaSmi, &[Operand::Imm(start_i32)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaSmi (slice arg): {err:?}"))
            })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(arg_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (slice arg): {err:?}"))
            })?;
        // CallProperty r_callee, r_src, [arg]
        let call_pc = builder
            .emit(
                Opcode::CallProperty,
                &[
                    Operand::Reg(u32::from(callee_temp)),
                    Operand::Reg(u32::from(src_reg)),
                    Operand::RegList {
                        base: u32::from(arg_temp),
                        count: 1,
                    },
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode CallProperty (slice): {err:?}"))
            })?;
        ctx.attach_call_feedback(builder, call_pc);
        // acc now holds the sliced array; bind.
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(rest_slot))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (rest slot): {err:?}"))
            })?;
        Ok(())
    })();
    ctx.release_temps(2);
    lower
}

/// Lowers `{ a, b: x, c = 0 }` object destructuring against the
/// source in `src_reg`. Each property reads via
/// `LdaNamedProperty r_src, key_idx`; an optional default fires
/// when the read returns `undefined` via
/// `JumpIfNotUndefined skip; <lower default>; skip:`.
///
/// Rejected:
/// - Computed keys (`{ [k]: v }`) → `computed_pattern_key`.
fn lower_object_pattern<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    pat: &'a ObjectPattern<'a>,
    src_reg: RegisterIndex,
    is_const: bool,
) -> Result<(), SourceLoweringError> {
    let excluded_base = if pat.rest.is_some() && !pat.properties.is_empty() {
        let count = RegisterIndex::try_from(pat.properties.len()).map_err(|_| {
            SourceLoweringError::Internal("object rest exclusion count overflow".into())
        })?;
        Some(ctx.acquire_temps(count)?)
    } else {
        None
    };

    for (prop_index, prop) in pat.properties.iter().enumerate() {
        let exclusion_slot = excluded_base.map(|base| base + prop_index as RegisterIndex);
        // Resolve the key: static identifier / string literal
        // both stringify to a known name; computed keys evaluate
        // an expression and use `LdaKeyedProperty`.
        let (computed_key_temp, key_name_for_rest, static_key_idx) = if prop.computed {
            // Computed key — evaluate the expression once into a
            // temp so both the property read and the rest-key
            // exclusion can reuse it.
            let temp = exclusion_slot.unwrap_or(ctx.acquire_temps(1)?);
            let key_expr = prop.key.to_expression();
            lower_return_expression(builder, ctx, key_expr)?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (object pattern computed key): {err:?}"
                    ))
                })?;
            (Some(temp), None, None)
        } else {
            let key_name = match &prop.key {
                PropertyKey::StaticIdentifier(ident) => ident.name.as_str().to_owned(),
                PropertyKey::StringLiteral(lit) => lit.value.as_str().to_owned(),
                // Object destructuring pattern — `let { 0: x, 1n: y }
                // = obj;` — numeric / BigInt keys stringify the same
                // way as in object literals.
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
            if let Some(slot) = exclusion_slot {
                emit_string_literal_to_register(builder, ctx, &key_name, slot)?;
            }
            (None, Some(key_name), Some(idx))
        };
        // Read the property value into acc via Lda(Named|Keyed)Property.
        if let Some(temp) = computed_key_temp {
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Ldar (pattern computed key): {err:?}"
                    ))
                })?;
            builder
                .emit(
                    Opcode::LdaKeyedProperty,
                    &[Operand::Reg(u32::from(src_reg))],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaKeyedProperty (object pattern): {err:?}"
                    ))
                })?;
            if exclusion_slot.is_none() {
                ctx.release_temps(1);
            }
        } else if let Some(idx) = static_key_idx {
            builder
                .emit(
                    Opcode::LdaNamedProperty,
                    &[Operand::Reg(u32::from(src_reg)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaNamedProperty (object pattern): {err:?}"
                    ))
                })?;
        }
        // Dispatch on the binding shape:
        //   - AssignmentPattern (`{ a = 5 }` / `{ a: b = 5 }`)
        //     runs the default-check, then recurses into the
        //     target (identifier OR nested pattern).
        //   - Nested ArrayPattern / ObjectPattern stashes acc in
        //     a temp and recurses.
        //   - Plain BindingIdentifier is the straightforward
        //     allocate-local + Star case.
        match &prop.value {
            BindingPattern::AssignmentPattern(assign) => {
                emit_default_for_destructured_leaf(builder, ctx, Some(&assign.right))?;
                destructure_assign_target(builder, ctx, &assign.left, is_const)?;
            }
            BindingPattern::ArrayPattern(_) | BindingPattern::ObjectPattern(_) => {
                let nested_slot = ctx.allocate_anonymous_local()?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(nested_slot))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (object pattern nested): {err:?}"
                        ))
                    })?;
                lower_pattern_bind(builder, ctx, &prop.value, nested_slot, is_const)?;
            }
            BindingPattern::BindingIdentifier(ident) => {
                let name = ident.name.as_str();
                let slot = ctx.allocate_local(name, is_const, ident.span)?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (object pattern leaf): {err:?}"
                        ))
                    })?;
                ctx.mark_initialized(name)?;
            }
        }
        let _ = key_name_for_rest;
    }

    // `{ a, b, ...rest }` — after binding `a` and `b`, copy every
    // other own-enumerable property of src into a fresh object,
    // excluding the keys we already bound.
    if let Some(rest) = pat.rest.as_deref() {
        let BindingPattern::BindingIdentifier(rest_ident) = &rest.argument else {
            return Err(SourceLoweringError::unsupported(
                "parser_recovery_object_rest_pattern",
                rest.span,
            ));
        };
        let rest_name = rest_ident.name.as_str();
        let rest_slot = ctx.allocate_local(rest_name, is_const, rest_ident.span)?;
        emit_object_rest_copy(
            builder,
            src_reg,
            excluded_base.map(|base| (base, pat.properties.len())),
            rest_slot,
        )?;
        ctx.mark_initialized(rest_name)?;
    }
    if excluded_base.is_some() {
        let count = RegisterIndex::try_from(pat.properties.len()).map_err(|_| {
            SourceLoweringError::Internal("object rest exclusion count overflow".into())
        })?;
        ctx.release_temps(count);
    }
    Ok(())
}

pub(super) fn emit_string_literal_to_register(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    value: &str,
    slot: RegisterIndex,
) -> Result<(), SourceLoweringError> {
    let idx = ctx.intern_string_literal(value)?;
    builder
        .emit(Opcode::LdaConstStr, &[Operand::Idx(idx)])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode LdaConstStr (rest key): {err:?}"))
        })?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
        .map_err(|err| SourceLoweringError::Internal(format!("encode Star (rest key): {err:?}")))?;
    Ok(())
}

/// Build a fresh object and copy every own-enumerable data
/// property from `src_reg` EXCEPT the ones whose names we just
/// bound above. Drops the result into `rest_slot`.
pub(super) fn emit_object_rest_copy(
    builder: &mut BytecodeBuilder,
    src_reg: RegisterIndex,
    excluded_regs: Option<(RegisterIndex, usize)>,
    rest_slot: RegisterIndex,
) -> Result<(), SourceLoweringError> {
    builder.emit(Opcode::CreateObject, &[]).map_err(|err| {
        SourceLoweringError::Internal(format!("encode CreateObject (obj rest): {err:?}"))
    })?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(rest_slot))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (obj rest target): {err:?}"))
        })?;
    builder
        .emit(Opcode::Ldar, &[Operand::Reg(u32::from(src_reg))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Ldar (obj rest src): {err:?}"))
        })?;
    if let Some((base, count)) = excluded_regs {
        builder
            .emit(
                Opcode::CopyDataPropertiesExcept,
                &[
                    Operand::Reg(u32::from(rest_slot)),
                    Operand::RegList {
                        base: u32::from(base),
                        count: u32::try_from(count).map_err(|_| {
                            SourceLoweringError::Internal(
                                "object rest exclusion count overflow".into(),
                            )
                        })?,
                    },
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode CopyDataPropertiesExcept (obj rest): {err:?}"
                ))
            })?;
    } else {
        builder
            .emit(
                Opcode::CopyDataProperties,
                &[Operand::Reg(u32::from(rest_slot))],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode CopyDataProperties (obj rest): {err:?}"
                ))
            })?;
    }
    Ok(())
}

/// Inserts the `undefined`-check default-initializer sequence when
/// a destructuring leaf has a default expression. Same pattern as
/// M22's param default initializer:
///
/// ```text
///   ; acc = read value
///   JumpIfNotUndefined skip
///   <lower default expr>   ; acc = default
/// skip:
/// ```
pub(super) fn emit_default_for_destructured_leaf<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    default: Option<&'a Expression<'a>>,
) -> Result<(), SourceLoweringError> {
    let Some(expr) = default else {
        return Ok(());
    };
    let skip = builder.new_label();
    let jmp_pc = builder
        .emit_jump_to(Opcode::JumpIfNotUndefined, skip)
        .map_err(|err| {
            SourceLoweringError::Internal(format!(
                "encode JumpIfNotUndefined (destructure default): {err:?}"
            ))
        })?;
    ctx.attach_branch_feedback(builder, jmp_pc);
    lower_return_expression(builder, ctx, expr)?;
    builder
        .bind_label(skip)
        .map_err(|err| SourceLoweringError::Internal(format!("bind destructure skip: {err:?}")))?;
    Ok(())
}
