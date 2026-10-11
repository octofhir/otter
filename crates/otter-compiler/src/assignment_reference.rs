//! Prepared identifier references shared by assignment forms.
//!
//! # Contents
//! - With/eval reference acquisition and strict global existence proofs.
//! - Reads through the selected reference for compound/logical assignment.
//! - PutValue through the selected reference after value evaluation.
//!
//! # Invariants
//! Reference selection precedes every intervening extraction or default
//! initializer. Canonical compiler registers retain selected object/environment
//! bases across effects and GC. Static lexical stores keep their TDZ and
//! immutable checks; declarations initialize through a separate owner.
//! A strict global existence proof is captured only on the selected global
//! fallback; with or eval-extension hits do not consult the ambient global.
//!
//! # See also
//! - `assignment` owns AST evaluation order and member targets.
//! - `binding_emit` owns static/eval binding bytecode.
//! - `with_statement` owns object environment record operations.

use crate::compiler::NameRef;
use crate::with_statement::{WithBindingProbe, emit_with_binding_probe};
use crate::{CompileError, Compiler, Op, Operand};

/// A resolved identifier reference retained across value evaluation.
pub(crate) struct PreparedIdentifierTarget {
    pub(crate) name: String,
    reference: NameRef,
    with_ref: Option<WithBindingProbe>,
    eval_ref: Option<u16>,
    strict_global_probe: Option<u16>,
}

pub(crate) fn prepare_identifier_target(
    cx: &mut Compiler,
    name: String,
    reference: NameRef,
    check_unresolvable: bool,
    span: (u32, u32),
) -> Result<PreparedIdentifierTarget, CompileError> {
    let active_with_envs = cx.active_with_envs.clone();
    let with_ref = emit_with_binding_probe(cx, &name, &active_with_envs, span)?;
    let eval_ref = cx.emit_resolve_ref(&name, &reference, span);
    let strict_global_probe =
        if check_unresolvable && matches!(reference, NameRef::Global(_)) && cx.is_strict {
            let exists = cx.alloc_scratch();
            let mut selected_environment = Vec::new();
            if with_ref.is_some() || eval_ref.is_some() {
                cx.emit(Op::LoadFalse, [Operand::Register(exists)], span);
            }
            if let Some(probe) = &with_ref {
                selected_environment.push(cx.emit_branch_placeholder(
                    Op::JumpIfTrue,
                    Some(probe.found_reg),
                    span,
                ));
            }
            if let Some(ref_reg) = eval_ref {
                let global_fallback =
                    cx.emit_branch_placeholder(Op::JumpIfNullish, Some(ref_reg), span);
                selected_environment.push(cx.emit_branch_placeholder(Op::Jump, None, span));
                cx.patch_branch_to_here(global_fallback);
            }
            let name_idx = cx.intern_string_constant(&name);
            cx.emit(
                Op::GlobalBindingExists,
                [Operand::Register(exists), Operand::ConstIndex(name_idx)],
                span,
            );
            for selected in selected_environment {
                cx.patch_branch_to_here(selected);
            }
            Some(exists)
        } else {
            None
        };
    Ok(PreparedIdentifierTarget {
        name,
        reference,
        with_ref,
        eval_ref,
        strict_global_probe,
    })
}

pub(crate) fn store_prepared_identifier(
    cx: &mut Compiler,
    target: PreparedIdentifierTarget,
    value: u16,
    span: (u32, u32),
) {
    let mut with_store_done = None;
    if let Some(probe) = &target.with_ref {
        let fallback = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(probe.found_reg), span);
        crate::with_statement::emit_with_set_mutable_binding(
            cx,
            probe.object_reg,
            &target.name,
            value,
            span,
        );
        with_store_done = Some(cx.emit_branch_placeholder(Op::Jump, None, span));
        cx.patch_branch_to_here(fallback);
    }
    if let Some(exists) = target.strict_global_probe {
        let mut extension_store_done = None;
        if let Some(ref_reg) = target.eval_ref {
            let global_fallback =
                cx.emit_branch_placeholder(Op::JumpIfNullish, Some(ref_reg), span);
            cx.emit_store_ref(value, ref_reg, &target.name, &target.reference, span);
            extension_store_done = Some(cx.emit_branch_placeholder(Op::Jump, None, span));
            cx.patch_branch_to_here(global_fallback);
        }
        let name_idx = cx.intern_string_constant(&target.name);
        cx.emit(
            Op::StoreGlobalChecked,
            [
                Operand::Register(value),
                Operand::ConstIndex(name_idx),
                Operand::Register(exists),
            ],
            span,
        );
        if let Some(done) = extension_store_done {
            cx.patch_branch_to_here(done);
        }
    } else if let Some(ref_reg) = target.eval_ref {
        cx.emit_store_ref(value, ref_reg, &target.name, &target.reference, span);
    } else {
        cx.emit_name_assign(value, &target.name, &target.reference, span);
    }
    cx.emit_module_export_mirror(&target.name, value, span);
    if let Some(done) = with_store_done {
        cx.patch_branch_to_here(done);
    }
}

pub(crate) fn load_prepared_identifier(
    cx: &mut Compiler,
    target: &PreparedIdentifierTarget,
    dst: u16,
    span: (u32, u32),
) {
    let mut with_done = None;
    if let Some(probe) = &target.with_ref {
        let fallback = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(probe.found_reg), span);
        crate::with_statement::emit_with_get_binding_value(
            cx,
            dst,
            probe.object_reg,
            &target.name,
            span,
        );
        with_done = Some(cx.emit_branch_placeholder(Op::Jump, None, span));
        cx.patch_branch_to_here(fallback);
    }
    cx.emit_name_load(dst, &target.name, &target.reference, false, span);
    if let Some(done) = with_done {
        cx.patch_branch_to_here(done);
    }
}
