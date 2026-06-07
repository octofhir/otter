//! Identifier expression lowering.
//!
//! # Contents
//! - [`compile_identifier`] — lowers ordinary identifier reads after inline fast paths.
//!
//! # See also
//! - [`super`] — expression dispatch and shared helpers.

use crate::*;
use oxc_ast::ast::IdentifierReference;

pub(crate) fn compile_identifier(
    cx: &mut Compiler,
    id: &IdentifierReference<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let _ = span;
    let span = (id.span.start, id.span.end);
    // Foundation pseudo-globals before falling back to
    // local resolution. An enclosing `with` suppresses the constant
    // fold — §9.1.1.2.1 lets `with ({NaN: 'x'})` shadow the global.
    match id.name.as_str() {
        _ if !cx.active_with_envs.is_empty() => {}
        "NaN" => {
            let dst = cx.alloc_scratch();
            let const_idx = cx.intern_number_constant(f64::NAN);
            cx.emit(
                Op::LoadNumber,
                [Operand::Register(dst), Operand::ConstIndex(const_idx)],
                span,
            );
            return Ok(dst);
        }
        "Infinity" => {
            let dst = cx.alloc_scratch();
            let const_idx = cx.intern_number_constant(f64::INFINITY);
            cx.emit(
                Op::LoadNumber,
                [Operand::Register(dst), Operand::ConstIndex(const_idx)],
                span,
            );
            return Ok(dst);
        }
        _ => {}
    }
    if !cx.active_with_envs.is_empty() {
        let active_with_envs = cx.active_with_envs.clone();
        return compile_identifier_with_envs(cx, id.name.as_str(), &active_with_envs, span);
    }
    compile_identifier_without_with(cx, id.name.as_str(), span)
}

fn compile_identifier_with_envs(
    cx: &mut Compiler,
    name: &str,
    active_with_envs: &[crate::with_statement::WithEnv],
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let dst = cx.alloc_scratch();
    let probe = emit_with_binding_probe(cx, name, active_with_envs, span)?;
    let mut with_done = None;
    if let Some(probe) = &probe {
        let fallback = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(probe.found_reg), span);
        crate::with_statement::emit_with_get_binding_value(cx, dst, probe.object_reg, name, span);
        with_done = Some(cx.emit_branch_placeholder(Op::Jump, None, span));
        cx.patch_branch_to_here(fallback);
    }
    let fallback = compile_identifier_without_with(cx, name, span)?;
    cx.emit(
        Op::StoreLocal,
        [Operand::Register(fallback), Operand::Imm32(dst as i32)],
        span,
    );
    if let Some(done) = with_done {
        cx.patch_branch_to_here(done);
    }
    Ok(dst)
}

fn compile_identifier_without_with(
    cx: &mut Compiler,
    name: &str,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    // ECMA-262 §19.3 / §20.5 native error constructors
    // (`Error`, `TypeError`, `RangeError`, `SyntaxError`,
    // `ReferenceError`, `URIError`, `EvalError`). Bare
    // identifier reads — e.g. `e instanceof TypeError` —
    // lower to `Op::LoadBuiltinError` so the runtime hands
    // back the per-interpreter constructor object whose
    // `prototype` own property feeds `Op::Instanceof`.
    // Local bindings of the same name still take precedence
    // (checked below via `lookup_binding`), so user code
    // can shadow the global if it really needs to.
    //
    // <https://tc39.es/ecma262/#sec-error-objects>
    if cx.lookup_binding(name).is_none()
        && find_module_import_binding(cx, name).is_none()
        && is_builtin_error_class_name(name)
    {
        let dst = cx.alloc_scratch();
        let kind_idx = cx.intern_string_constant(name);
        cx.emit(
            Op::LoadBuiltinError,
            [Operand::Register(dst), Operand::ConstIndex(kind_idx)],
            span,
        );
        return Ok(dst);
    }
    // Module-mode identifier resolution: imported aliases
    // resolve to a `LoadProperty` against the source
    // module's import-record (live binding — every read
    // observes the current export value).
    //
    // Inner functions that reference an imported alias
    // walk up the function-context stack to find the
    // matching record-upvalue, then capture it via the
    // standard `resolve_capture` cascade so the cell is
    // available in the inner frame's upvalues array.
    //
    // Spec: <https://tc39.es/ecma262/#sec-getidentifierreference>
    //       <https://tc39.es/ecma262/#sec-module-environment-records-getbindingvalue-n-s>
    if let Some((binding, synthetic)) = find_module_import_binding(cx, name) {
        // `import * as ns` binds to the Module Namespace Exotic Object
        // (§10.4.6), resolved from the specifier — distinct from the
        // raw env record used for named-import indirection. A `import
        // defer * as ns` binding instead reads its dedicated deferred
        // namespace cell (lazy evaluation), handled by the generic
        // record path below.
        if binding.is_namespace && !binding.is_deferred {
            let dst = cx.alloc_scratch();
            let spec_const = cx.intern_string_constant(&binding.specifier);
            cx.emit(
                Op::ModuleNamespaceObject,
                vec![Operand::Register(dst), Operand::ConstIndex(spec_const)],
                span,
            );
            return Ok(dst);
        }
        // `import defer * as ns` — the deferred cell already holds the
        // deferred namespace object; read it directly.
        if binding.is_namespace {
            let resolved_uv = if cx.module_state.is_some() {
                binding.record_uv_idx
            } else {
                cx.resolve_capture(&synthetic)
                    .expect("synthetic import-record binding must resolve")
            };
            let record_dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadUpvalue,
                vec![
                    Operand::Register(record_dst),
                    Operand::Imm32(resolved_uv as i32),
                ],
                span,
            );
            return Ok(record_dst);
        }
        // §9.1.1.5 GetBindingValue — read the named import through the
        // source module's §16.2.1.6 ResolveExport table so a re-exported
        // / star-exported name observes the *defining* module's live
        // binding (raising ReferenceError if it is still in its TDZ).
        // The source URL is statically known from the host resolution
        // table, so the read needs no per-import record cell.
        let source_url = module_specifier_target(cx, &binding.specifier)
            .unwrap_or_else(|| binding.specifier.clone());
        let url_const = cx.intern_string_constant(&source_url);
        let name_const = cx.intern_string_constant(&binding.source_name);
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::LoadImportBinding,
            vec![
                Operand::Register(dst),
                Operand::ConstIndex(url_const),
                Operand::ConstIndex(name_const),
            ],
            span,
        );
        return Ok(dst);
    }
    if let Some(info) = cx.lookup_binding(name) {
        let dst = cx.alloc_scratch();
        if info.initialized {
            cx.emit_load_storage(dst, info.storage, span);
        } else {
            // Reading a `let` / `const` binding before its
            // initializer ran — runtime raises
            // `ReferenceError` via `Op::TdzError`.
            let diag_idx = match info.storage {
                BindingStorage::Register { reg } => reg,
                BindingStorage::Upvalue { idx } => idx,
            };
            cx.emit(Op::TdzError, [Operand::Imm32(diag_idx as i32)], span);
        }
        return Ok(dst);
    }
    // Walk the parent chain for a closure capture.
    if let Some(uv_idx) = cx.resolve_capture(name) {
        let dst = cx.alloc_scratch();
        // §9.1 — when THIS function body contains a direct eval, an
        // eval-introduced var of the same name shadows the capture
        // (it lands in this frame's variable environment, inner to
        // the captured one).
        if cx.contains_direct_eval && !cx.is_strict {
            let name_idx = cx.intern_string_constant(name);
            cx.emit(
                Op::LoadShadowedUpvalue,
                [
                    Operand::Register(dst),
                    Operand::ConstIndex(name_idx),
                    Operand::Imm32(uv_idx as i32),
                ],
                span,
            );
            return Ok(dst);
        }
        cx.emit(
            Op::LoadUpvalue,
            [Operand::Register(dst), Operand::Imm32(uv_idx as i32)],
            span,
        );
        return Ok(dst);
    }
    // §10.2.4.1 ResolveBinding + §10.2.4.5 GetValue
    // fallback — an unbound free identifier resolves
    // against the global environment record (foundation:
    // `globalThis`). When the global has no own property
    // under that name, the runtime throws a
    // `ReferenceError` per the spec.
    //
    // Inside a function whose body contains a direct eval,
    // the name may instead resolve to a binding the eval
    // introduced into this frame's variable environment at
    // runtime — `Op::LoadDynamic` checks that map first.
    //
    // <https://tc39.es/ecma262/#sec-resolvebinding>
    // <https://tc39.es/ecma262/#sec-getvalue>
    let dst = cx.alloc_scratch();
    let name_idx = cx.intern_string_constant(name);
    let op = if cx.any_enclosing_direct_eval() {
        Op::LoadDynamic
    } else {
        Op::LoadGlobalOrThrow
    };
    cx.emit(
        op,
        [Operand::Register(dst), Operand::ConstIndex(name_idx)],
        span,
    );
    Ok(dst)
}
