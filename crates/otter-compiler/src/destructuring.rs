//! Destructuring declaration and assignment lowering helpers.
//!
//! # Contents
//! - default application
//! - pattern dispatch
//! - array iterator destructuring
//! - object property destructuring
//!
//! # Invariants
//! - Iterator-based array destructuring preserves source element order.
//!
//! # See also
//! - `assignment` for assignment targets

use crate::*;

/// Overwrite `value_reg` with the lazy default value when its
/// current contents are `undefined`. Compiles to:
///
/// ```text
///   ToBoolean tmp <- undefined?  ; using JumpIfNotUndefined-style
///   actually: equality compare with undefined + branch
/// ```
///
/// Foundation lowering uses two existing opcodes — `LoadUndefined`
/// and `Equal` followed by `JumpIfFalse` — to avoid introducing a
/// dedicated "is-undefined" branch.
pub(crate) fn apply_default_into_with_name(
    parent: &mut Compiler,
    value_reg: u16,
    default_expr: &Expression<'_>,
    inferred_name: Option<&str>,
    span: (u32, u32),
) -> Result<(), CompileError> {
    let undef_reg = parent.alloc_scratch();
    parent.emit(Op::LoadUndefined, [Operand::Register(undef_reg)], span);
    let cond_reg = parent.alloc_scratch();
    parent.emit(
        Op::Equal,
        vec![
            Operand::Register(cond_reg),
            Operand::Register(value_reg),
            Operand::Register(undef_reg),
        ],
        span,
    );
    // If the slot is **not** undefined, skip the default
    // evaluation entirely so the user's expression doesn't fire on
    // the common path.
    let skip_default = parent.emit_branch_placeholder(Op::JumpIfFalse, Some(cond_reg), span);
    let default_value = match inferred_name {
        Some(name) => compile_expr_with_inferred_name(parent, default_expr, name, span)?,
        None => compile_expr(parent, default_expr, span)?,
    };
    parent.emit(
        Op::StoreLocal,
        vec![
            Operand::Register(default_value),
            Operand::Imm32(value_reg as i32),
        ],
        span,
    );
    parent.patch_branch_to_here(skip_default);
    Ok(())
}

pub(crate) fn emit_require_object_coercible(
    parent: &mut Compiler,
    value_reg: u16,
    span: (u32, u32),
) {
    let jump_to_throw = parent.emit_branch_placeholder(Op::JumpIfNullish, Some(value_reg), span);
    let jump_to_body = parent.emit_branch_placeholder(Op::Jump, None, span);
    parent.patch_branch_to_here(jump_to_throw);
    let message_reg = parent.alloc_scratch();
    let message = parent.intern_string_constant("Cannot destructure null or undefined");
    parent.emit(
        Op::LoadString,
        [Operand::Register(message_reg), Operand::ConstIndex(message)],
        span,
    );
    let error_reg = parent.alloc_scratch();
    let kind = parent.intern_string_constant("TypeError");
    parent.emit(
        Op::NewBuiltinError,
        [
            Operand::Register(error_reg),
            Operand::ConstIndex(kind),
            Operand::Register(message_reg),
        ],
        span,
    );
    parent.emit(Op::Throw, [Operand::Register(error_reg)], span);
    parent.patch_branch_to_here(jump_to_body);
}

/// Recursively destructure the value in `src_reg` into the named
/// bindings declared by `pattern`. Handles `BindingIdentifier`
/// (the leaf), `ArrayPattern` (via the iterator protocol),
/// `ObjectPattern` (via property loads with rename / default
/// support), and inner `AssignmentPattern` defaults.
pub(crate) fn destructure_into(
    parent: &mut Compiler,
    src_reg: u16,
    pattern: &oxc_ast::ast::BindingPattern<'_>,
    span: (u32, u32),
) -> Result<(), CompileError> {
    destructure_pattern(parent, src_reg, pattern, span, false)
}

/// Mirror of [`destructure_into`] for `var` destructuring heads —
/// each leaf identifier resolves to an *existing* binding (the
/// var-hoist pass populated it at function entry) and is stored
/// rather than re-declared. Used by `for (var [a, b] of …)` etc.
pub(crate) fn destructure_assign(
    parent: &mut Compiler,
    src_reg: u16,
    pattern: &oxc_ast::ast::BindingPattern<'_>,
    span: (u32, u32),
) -> Result<(), CompileError> {
    destructure_pattern(parent, src_reg, pattern, span, true)
}

pub(crate) fn destructure_pattern(
    parent: &mut Compiler,
    src_reg: u16,
    pattern: &oxc_ast::ast::BindingPattern<'_>,
    span: (u32, u32),
    assign_existing: bool,
) -> Result<(), CompileError> {
    match pattern {
        oxc_ast::ast::BindingPattern::BindingIdentifier(id) => {
            let name = id.name.as_str();
            if assign_existing {
                store_identifier(parent, name, src_reg, span)
            } else {
                // A lexical pre-pass (block / function top-level /
                // loop-head prologue) may have already declared this
                // leaf uninitialized (TDZ) — bind through it so
                // closures made before this point observe the store,
                // and so a loop head's Op::FreshUpvalue re-mints the
                // same slot every iteration. The walk crosses scopes
                // because a for-head binds from inside the body scope;
                // an *initialized* outer binding never matches.
                let storage = match parent.lookup_binding(name).filter(|info| !info.initialized) {
                    Some(info) => info.storage,
                    None => parent.declare_binding(name, false, span)?,
                };
                parent.emit_store_storage(src_reg, storage, span);
                parent.mark_initialized(name);
                // `export const { x } = …` — the mirror is gated on
                // `module_state.exported_names`, so plain destructuring
                // declarations emit nothing here.
                parent.emit_module_export_mirror(name, src_reg, span);
                Ok(())
            }
        }
        oxc_ast::ast::BindingPattern::AssignmentPattern(asgn) => {
            let asgn_span = (asgn.span.start, asgn.span.end);
            let inferred_name = match &asgn.left {
                oxc_ast::ast::BindingPattern::BindingIdentifier(id) => Some(id.name.as_str()),
                _ => None,
            };
            apply_default_into_with_name(parent, src_reg, &asgn.right, inferred_name, asgn_span)?;
            destructure_pattern(parent, src_reg, &asgn.left, span, assign_existing)
        }
        oxc_ast::ast::BindingPattern::ArrayPattern(arr) => {
            destructure_array_inner(parent, src_reg, arr, span, assign_existing)
        }
        oxc_ast::ast::BindingPattern::ObjectPattern(obj) => {
            destructure_object_inner(parent, src_reg, obj, span, assign_existing)
        }
    }
}

pub(crate) fn destructure_array_inner(
    parent: &mut Compiler,
    src_reg: u16,
    pattern: &oxc_ast::ast::ArrayPattern<'_>,
    span: (u32, u32),
    assign_existing: bool,
) -> Result<(), CompileError> {
    let iter_reg = parent.alloc_scratch();
    parent.emit(
        Op::GetIterator,
        [Operand::Register(iter_reg), Operand::Register(src_reg)],
        span,
    );
    parent.emit(Op::IteratorCloseStart, [Operand::Register(iter_reg)], span);
    let mut last_done_reg = None;
    for elem in &pattern.elements {
        let value_reg = parent.alloc_scratch();
        let done_reg = parent.alloc_scratch();
        parent.emit(
            Op::IteratorNext,
            vec![
                Operand::Register(value_reg),
                Operand::Register(done_reg),
                Operand::Register(iter_reg),
            ],
            span,
        );
        last_done_reg = Some(done_reg);
        // A hole (`,,`) leaves the slot unbound — nothing to emit.
        let Some(inner) = elem else {
            continue;
        };
        destructure_pattern(parent, value_reg, inner, span, assign_existing)?;
    }
    if let Some(rest) = &pattern.rest {
        // Drain the rest of the iterator into a fresh array.
        let arr_reg = parent.alloc_scratch();
        parent.emit(
            Op::NewArray,
            [Operand::Register(arr_reg), Operand::ConstIndex(0)],
            span,
        );
        let value_reg = parent.alloc_scratch();
        let done_reg = parent.alloc_scratch();
        let loop_top = parent.next_pc();
        parent.emit(
            Op::IteratorNext,
            vec![
                Operand::Register(value_reg),
                Operand::Register(done_reg),
                Operand::Register(iter_reg),
            ],
            span,
        );
        let exit = parent.emit_branch_placeholder(Op::JumpIfTrue, Some(done_reg), span);
        parent.emit(
            Op::ArrayPush,
            [Operand::Register(arr_reg), Operand::Register(value_reg)],
            span,
        );
        let back = parent.emit_branch_placeholder(Op::Jump, None, span);
        parent.patch_branch(back, loop_top);
        parent.patch_branch_to_here(exit);
        destructure_pattern(parent, arr_reg, &rest.argument, span, assign_existing)?;
        parent.emit(Op::IteratorCloseEnd, [Operand::Register(iter_reg)], span);
    } else if let Some(done_reg) = last_done_reg {
        let skip_close = parent.emit_branch_placeholder(Op::JumpIfTrue, Some(done_reg), span);
        parent.emit(Op::IteratorClose, [Operand::Register(iter_reg)], span);
        parent.patch_branch_to_here(skip_close);
        parent.emit(Op::IteratorCloseEnd, [Operand::Register(iter_reg)], span);
    } else {
        parent.emit(Op::IteratorClose, [Operand::Register(iter_reg)], span);
        parent.emit(Op::IteratorCloseEnd, [Operand::Register(iter_reg)], span);
    }
    Ok(())
}

pub(crate) fn destructure_object_inner(
    parent: &mut Compiler,
    src_reg: u16,
    pattern: &oxc_ast::ast::ObjectPattern<'_>,
    span: (u32, u32),
    assign_existing: bool,
) -> Result<(), CompileError> {
    emit_require_object_coercible(parent, src_reg, span);

    // Track keys extracted by named/computed properties so the
    // rest element (`...r`) can exclude them when copying the
    // remaining own enumerable properties.
    enum ExtractedKey {
        Static(String),
        Runtime(u16),
    }
    let mut extracted_keys: Vec<ExtractedKey> = Vec::new();

    for prop in &pattern.properties {
        let prop_span = (prop.span.start, prop.span.end);
        let value_reg = parent.alloc_scratch();
        if prop.computed {
            // §13.15.5 — computed key evaluated at destructuring
            // time, then `obj[key]` via `Op::LoadElement`.
            let key_reg = match &prop.key {
                oxc_ast::ast::PropertyKey::StaticIdentifier(id) => {
                    let r = parent.alloc_scratch();
                    let s = parent.intern_string_constant(id.name.as_str());
                    parent.emit(
                        Op::LoadString,
                        [Operand::Register(r), Operand::ConstIndex(s)],
                        prop_span,
                    );
                    r
                }
                _ => compile_expr_as_property_key(parent, &prop.key, prop_span)?,
            };
            parent.emit(
                Op::LoadElement,
                vec![
                    Operand::Register(value_reg),
                    Operand::Register(src_reg),
                    Operand::Register(key_reg),
                ],
                prop_span,
            );
            extracted_keys.push(ExtractedKey::Runtime(key_reg));
        } else {
            // Static identifier / string / numeric / bigint key —
            // resolved to a string at compile time.
            let key_str: Option<String> = match &prop.key {
                oxc_ast::ast::PropertyKey::StaticIdentifier(id) => {
                    Some(id.name.as_str().to_string())
                }
                oxc_ast::ast::PropertyKey::StringLiteral(lit) => Some(lit.value.to_string()),
                oxc_ast::ast::PropertyKey::NumericLiteral(lit) => {
                    // §6.1.7.1 ToString(Number) — match runtime
                    // semantics so e.g. `1` and `1.0` both key as
                    // "1". Foundation defers to Rust f64 → string
                    // for the integer cases (NumericLiteral parses
                    // the source form).
                    Some(numeric_literal_to_property_key(lit.value))
                }
                oxc_ast::ast::PropertyKey::BigIntLiteral(lit) => {
                    // BigInt literal in property key: ToString
                    // strips the trailing `n`. oxc preserves the
                    // raw text including the suffix.
                    let raw = lit.raw.as_ref().map(|s| s.as_str()).unwrap_or("");
                    Some(raw.trim_end_matches('n').to_string())
                }
                _ => None,
            };
            match key_str {
                Some(s) => {
                    let key_const = parent.intern_string_constant(&s);
                    parent.emit(
                        Op::LoadProperty,
                        vec![
                            Operand::Register(value_reg),
                            Operand::Register(src_reg),
                            Operand::ConstIndex(key_const),
                        ],
                        prop_span,
                    );
                    extracted_keys.push(ExtractedKey::Static(s));
                }
                None => {
                    return Err(CompileError::Unsupported {
                        node: format!("ObjectPattern: non-string key ({:?})", prop.key),
                        span: prop_span,
                    });
                }
            }
        }
        destructure_pattern(parent, value_reg, &prop.value, prop_span, assign_existing)?;
    }

    if let Some(rest) = pattern.rest.as_ref() {
        // §13.15.5 RestObjectAssignment — build a fresh object,
        // copy every enumerable own property of `src`, then delete
        // each previously-extracted key.
        let rest_obj = parent.alloc_scratch();
        parent.emit(Op::NewObject, [Operand::Register(rest_obj)], span);
        parent.emit(
            Op::CopyDataProperties,
            [Operand::Register(rest_obj), Operand::Register(src_reg)],
            span,
        );
        for key in &extracted_keys {
            match key {
                ExtractedKey::Static(s) => {
                    let key_const = parent.intern_string_constant(s);
                    let del_dst = parent.alloc_scratch();
                    parent.emit(
                        Op::DeleteProperty,
                        vec![
                            Operand::Register(del_dst),
                            Operand::Register(rest_obj),
                            Operand::ConstIndex(key_const),
                        ],
                        span,
                    );
                }
                ExtractedKey::Runtime(key_reg) => {
                    let del_dst = parent.alloc_scratch();
                    parent.emit(
                        Op::DeleteElement,
                        vec![
                            Operand::Register(del_dst),
                            Operand::Register(rest_obj),
                            Operand::Register(*key_reg),
                        ],
                        span,
                    );
                }
            }
        }
        destructure_pattern(parent, rest_obj, &rest.argument, span, assign_existing)?;
    }
    Ok(())
}
