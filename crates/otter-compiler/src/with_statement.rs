//! Sloppy `with` statement lowering.
//!
//! # Contents
//! - [`compile_with_statement`] installs a temporary object
//!   environment for identifier lookup.
//!
//! # Invariants
//! - Strict functions and modules still reject `with`.
//! - The object environment is stored in an own-upvalue cell only
//!   while doing so cannot shift existing parent-capture slots.
//!
//! # See also
//! - `expr::identifier` for dynamic `with` identifier reads.

use crate::*;

pub(crate) struct WithBindingProbe {
    pub(crate) object_reg: u16,
    pub(crate) found_reg: u16,
}

/// One active `with` object environment, positioned in the lexical
/// scope chain so identifier sites can decide whether a static
/// binding shadows it (§9.1.1.2.1 — the chain is walked innermost
/// first, mixing declarative scopes and object environments).
#[derive(Clone, Debug)]
pub(crate) struct WithEnv {
    /// Synthetic binding holding the captured scope object.
    pub(crate) binding: String,
    /// `cx.stack.len()` when the `with` was lowered (1-based
    /// function nesting depth).
    pub(crate) fn_depth: usize,
    /// `scopes.len()` in the declaring function when the `with` was
    /// lowered (1-based scope nesting depth).
    pub(crate) scope_depth: usize,
}

pub(crate) fn compile_with_statement(
    cx: &mut Compiler,
    w: &oxc_ast::ast::WithStatement<'_>,
) -> Result<Option<u16>, CompileError> {
    let span = (w.span.start, w.span.end);
    cx.emit_completion_reset(span);
    if cx.is_strict {
        let message =
            "SyntaxError: `with` statements are not allowed in strict mode (§14.13)".to_string();
        return Err(CompileError::Syntax {
            messages: vec![message.clone()],
            diagnostics: vec![crate::SyntaxDiagnostic {
                code: "STRICT_MODE_WITH".to_string(),
                message,
                range: Some(span),
                help: None,
            }],
        });
    }

    let object_raw = compile_expr(cx, &w.object, span)?;
    // §14.11.2 step 2 — ToObject(expr): a primitive scope expression
    // resolves identifier lookups against its wrapper object;
    // `null` / `undefined` throw a TypeError here, before the body.
    let object = cx.alloc_scratch();
    cx.emit(
        Op::ToObject,
        [Operand::Register(object), Operand::Register(object_raw)],
        span,
    );
    let id = cx.next_with_env_id;
    cx.next_with_env_id = id.checked_add(1).expect("with env id overflow");
    let env_name = format!("__otter_with_env_{id}");
    let storage = if cx.parent_captures.is_empty() {
        cx.declare_captured_binding(&env_name, false, span)?
    } else {
        cx.declare_binding(&env_name, false, span)?
    };
    cx.emit_store_storage(object, storage, span);
    cx.mark_initialized(&env_name);

    let fn_depth = cx.stack.len();
    let scope_depth = cx.scopes.len();
    cx.active_with_envs.push(WithEnv {
        binding: env_name,
        fn_depth,
        scope_depth,
    });
    let result = compile_statement(cx, &w.body);
    cx.active_with_envs.pop();
    result
}

pub(crate) fn emit_with_binding_probe(
    cx: &mut Compiler,
    name: &str,
    active_with_envs: &[WithEnv],
    span: (u32, u32),
) -> Result<Option<WithBindingProbe>, CompileError> {
    if active_with_envs.is_empty() {
        return Ok(None);
    }

    // §9.1.1.2.1 — only object environments *inner* than the
    // innermost static declaration of `name` participate: walking
    // the chain innermost-first, the declarative binding is found
    // before any outer `with` object. A function-local `var` inside
    // a function defined in a `with` body therefore shadows the
    // with-object property, while a `var` hoisted *outside* the
    // `with` is shadowed by it.
    let binding_pos = cx.binding_position(name);
    let probed: Vec<String> = active_with_envs
        .iter()
        .rev()
        .take_while(|env| match binding_pos {
            None => true,
            Some((bf, bs)) => env.fn_depth > bf || (env.fn_depth == bf && env.scope_depth >= bs),
        })
        .map(|env| env.binding.clone())
        .collect();
    if probed.is_empty() {
        return Ok(None);
    }

    let object_reg = cx.alloc_scratch();
    cx.emit(Op::LoadUndefined, [Operand::Register(object_reg)], span);
    let found_reg = cx.alloc_scratch();
    cx.emit(Op::LoadFalse, [Operand::Register(found_reg)], span);
    let mut done_patches = Vec::new();

    for env_name in &probed {
        let env_reg = load_with_env_object(cx, env_name, span)?;
        let key_reg = cx.alloc_scratch();
        let key_idx = cx.intern_string_constant(name);
        cx.emit(
            Op::LoadString,
            [Operand::Register(key_reg), Operand::ConstIndex(key_idx)],
            span,
        );
        let present = cx.alloc_scratch();
        cx.emit(
            Op::HasProperty,
            [
                Operand::Register(present),
                Operand::Register(key_reg),
                Operand::Register(env_reg),
            ],
            span,
        );
        let next_env = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(present), span);
        let unscopables_sym = cx.alloc_scratch();
        let unscopables_idx = cx.intern_string_constant("unscopables");
        cx.emit(
            Op::SymbolLoad,
            [
                Operand::Register(unscopables_sym),
                Operand::ConstIndex(unscopables_idx),
            ],
            span,
        );
        let unscopables = cx.alloc_scratch();
        cx.emit(
            Op::LoadElement,
            vec![
                Operand::Register(unscopables),
                Operand::Register(env_reg),
                Operand::Register(unscopables_sym),
            ],
            span,
        );
        let unscopables_type = cx.alloc_scratch();
        cx.emit(
            Op::TypeOf,
            [
                Operand::Register(unscopables_type),
                Operand::Register(unscopables),
            ],
            span,
        );
        let object_type = cx.alloc_scratch();
        let object_idx = cx.intern_string_constant("object");
        cx.emit(
            Op::LoadString,
            [
                Operand::Register(object_type),
                Operand::ConstIndex(object_idx),
            ],
            span,
        );
        let is_object = cx.alloc_scratch();
        cx.emit(
            Op::Equal,
            [
                Operand::Register(is_object),
                Operand::Register(unscopables_type),
                Operand::Register(object_type),
            ],
            span,
        );
        let bind_env = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(is_object), span);
        let blocked = cx.alloc_scratch();
        cx.emit_load_property(blocked, unscopables, name, span);
        let next_env_for_blocked = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(blocked), span);
        cx.patch_branch_to_here(bind_env);
        cx.emit(
            Op::StoreLocal,
            [
                Operand::Register(env_reg),
                Operand::Imm32(object_reg as i32),
            ],
            span,
        );
        cx.emit(Op::LoadTrue, [Operand::Register(found_reg)], span);
        done_patches.push(cx.emit_branch_placeholder(Op::Jump, None, span));
        cx.patch_branch_to_here(next_env_for_blocked);
        cx.patch_branch_to_here(next_env);
    }

    for patch in done_patches {
        cx.patch_branch_to_here(patch);
    }

    Ok(Some(WithBindingProbe {
        object_reg,
        found_reg,
    }))
}

pub(crate) fn load_with_env_object(
    cx: &mut Compiler,
    env_name: &str,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    if let Some(info) = cx.lookup_binding(env_name) {
        let dst = cx.alloc_scratch();
        cx.emit_load_storage(dst, info.storage, span);
        return Ok(dst);
    }
    if let Some(idx) = cx.resolve_capture(env_name) {
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::LoadUpvalue,
            [Operand::Register(dst), Operand::Imm32(idx as i32)],
            span,
        );
        return Ok(dst);
    }
    Err(CompileError::Unsupported {
        node: format!("with environment `{env_name}` not capturable"),
        span,
    })
}
