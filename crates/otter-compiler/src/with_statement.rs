//! Sloppy `with` statement lowering.
//!
//! A `with` statement opens a `With` scope whose one binding holds the
//! object environment. The binding is a context slot when the body contains
//! a closure or a direct eval (either can reach the object after the
//! statement's straight-line code), a register otherwise; every entry into
//! the statement creates a fresh `With` context. Identifier sites inside
//! probe the object environments that are lexically inner to the name's
//! static binding.
//!
//! # Contents
//! - [`compile_with_statement`] installs the object environment.
//! - [`WithEnv`] — one active object environment and its chain position.
//! - [`emit_with_binding_probe`] / [`emit_with_get_binding_value`] /
//!   [`emit_with_set_mutable_binding`] — §9.1.1.2 object-record operations.
//!
//! # Invariants
//! - Strict functions and modules still reject `with`.
//! - A direct eval inside a `with` body sees the object through its caller
//!   chain's `With` scope and rebuilds the same probe order.
//!
//! # See also
//! - `expr::identifier` for identifier reads.

use crate::compiler::ScopeLocation;
use crate::*;

/// Name of the `With` scope's object binding. Not an identifier, so it
/// never collides with a source binding.
pub(crate) const WITH_OBJECT_BINDING: &str = "%with";

pub(crate) struct WithBindingProbe {
    pub(crate) object_reg: u16,
    pub(crate) found_reg: u16,
}

/// One active `with` object environment, positioned in the lexical
/// scope chain so identifier sites can decide whether a static
/// binding shadows it (§9.1.1.2.1 — the chain is walked innermost
/// first, mixing declarative scopes and object environments).
#[derive(Clone, Copy, Debug)]
pub(crate) struct WithEnv {
    /// The `With` scope holding the object.
    pub(crate) location: ScopeLocation,
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
    cx.enter_scope(otter_bytecode::ScopeKind::With);
    let reachable = crate::capture::statement_contains_closure_or_eval(&w.body);
    let storage = if reachable {
        cx.declare_forced_slot(
            WITH_OBJECT_BINDING,
            otter_bytecode::SlotKind::WithObject,
            span,
        )
    } else {
        cx.declare_binding_with_capture(
            WITH_OBJECT_BINDING,
            otter_bytecode::SlotKind::WithObject,
            span,
            false,
        )
    };
    let storage = match storage {
        Ok(storage) => storage,
        Err(error) => {
            cx.exit_scope();
            return Err(error);
        }
    };
    cx.emit_store_storage(object, storage, span);
    cx.mark_initialized(WITH_OBJECT_BINDING);

    let location = cx.innermost_location();
    cx.active_with_envs.push(WithEnv { location });
    let result = compile_statement(cx, &w.body);
    cx.active_with_envs.pop();
    cx.exit_scope();
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
    let probed: Vec<WithEnv> = active_with_envs
        .iter()
        .rev()
        .take_while(|env| match binding_pos {
            None => true,
            Some(position) => cx.location_rank(env.location) > position,
        })
        .copied()
        .collect();
    if probed.is_empty() {
        return Ok(None);
    }

    let object_reg = cx.alloc_scratch();
    cx.emit(Op::LoadUndefined, [Operand::Register(object_reg)], span);
    let found_reg = cx.alloc_scratch();
    cx.emit(Op::LoadFalse, [Operand::Register(found_reg)], span);
    let mut done_patches = Vec::new();

    for env in &probed {
        let env_reg = load_with_env_object(cx, env, span)?;
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
        // §9.1.1.2.1 step 4 — only an *Object* @@unscopables blocks;
        // `typeof null` is "object", so test nullish first.
        let bind_env_nullish =
            cx.emit_branch_placeholder(Op::JumpIfNullish, Some(unscopables), span);
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
        cx.patch_branch_to_here(bind_env_nullish);
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
    env: &WithEnv,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    cx.load_at(env.location, WITH_OBJECT_BINDING, span)
        .ok_or(CompileError::Unsupported {
            node: "with environment not reachable from this scope".to_string(),
            span,
        })
}

/// §9.1.1.2.6 GetBindingValue through a `with` object environment —
/// the earlier HasBinding probe passed, but the spec RE-CHECKS
/// HasProperty (observable on proxy envs, and the @@unscopables
/// getter may have deleted the binding) before Get; a vanished
/// binding yields ReferenceError in strict code and `undefined`
/// otherwise.
pub(crate) fn emit_with_get_binding_value(
    cx: &mut Compiler,
    dst: u16,
    object_reg: u16,
    name: &str,
    span: (u32, u32),
) {
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
            Operand::Register(object_reg),
        ],
        span,
    );
    let missing = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(present), span);
    cx.emit_load_property(dst, object_reg, name, span);
    let done = cx.emit_branch_placeholder(Op::Jump, None, span);
    cx.patch_branch_to_here(missing);
    if cx.is_strict {
        crate::assignment::emit_reference_error(cx, name, span);
    } else {
        cx.emit(Op::LoadUndefined, [Operand::Register(dst)], span);
    }
    cx.patch_branch_to_here(done);
}

/// §9.1.1.2.5 SetMutableBinding through a `with` object environment —
/// HasProperty runs unconditionally (observable trap order); a
/// vanished binding throws ReferenceError only in strict code, and
/// the Set proceeds either way.
pub(crate) fn emit_with_set_mutable_binding(
    cx: &mut Compiler,
    object_reg: u16,
    name: &str,
    value_reg: u16,
    span: (u32, u32),
) {
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
            Operand::Register(object_reg),
        ],
        span,
    );
    if cx.is_strict {
        let ok = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(present), span);
        crate::assignment::emit_reference_error(cx, name, span);
        cx.patch_branch_to_here(ok);
    }
    let scratch = cx.alloc_scratch();
    cx.emit(
        Op::StoreProperty,
        vec![
            Operand::Register(object_reg),
            Operand::ConstIndex(key_idx),
            Operand::Register(value_reg),
            Operand::Register(scratch),
        ],
        span,
    );
}
