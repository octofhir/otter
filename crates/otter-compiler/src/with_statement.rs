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

pub(crate) fn compile_with_statement(
    cx: &mut Compiler,
    w: &oxc_ast::ast::WithStatement<'_>,
) -> Result<Option<u16>, CompileError> {
    let span = (w.span.start, w.span.end);
    if cx.is_strict {
        return Err(CompileError::Unsupported {
            node: "WithStatement is forbidden in strict mode / ES modules (§14.13)".to_string(),
            span,
        });
    }

    let object = compile_expr(cx, &w.object, span)?;
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

    cx.active_with_envs.push(env_name);
    let result = compile_statement(cx, &w.body);
    cx.active_with_envs.pop();
    result
}

pub(crate) fn emit_with_binding_probe(
    cx: &mut Compiler,
    name: &str,
    active_with_envs: &[String],
    span: (u32, u32),
) -> Result<Option<WithBindingProbe>, CompileError> {
    if active_with_envs.is_empty() {
        return Ok(None);
    }

    let object_reg = cx.alloc_scratch();
    cx.emit(Op::LoadUndefined, [Operand::Register(object_reg)], span);
    let found_reg = cx.alloc_scratch();
    cx.emit(Op::LoadFalse, [Operand::Register(found_reg)], span);
    let mut done_patches = Vec::new();

    for env_name in active_with_envs.iter().rev() {
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
