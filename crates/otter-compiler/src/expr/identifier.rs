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
    // local resolution.
    match id.name.as_str() {
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
    if cx.lookup_binding(id.name.as_str()).is_none()
        && find_module_import_binding(cx, id.name.as_str()).is_none()
        && is_builtin_error_class_name(id.name.as_str())
    {
        let dst = cx.alloc_scratch();
        let kind_idx = cx.intern_string_constant(id.name.as_str());
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
    if let Some((binding, synthetic)) = find_module_import_binding(cx, id.name.as_str()) {
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
        if binding.is_namespace {
            return Ok(record_dst);
        }
        let dst = cx.alloc_scratch();
        cx.emit_load_property(dst, record_dst, &binding.source_name, span);
        return Ok(dst);
    }
    if let Some(info) = cx.lookup_binding(id.name.as_str()) {
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
    if let Some(uv_idx) = cx.resolve_capture(id.name.as_str()) {
        let dst = cx.alloc_scratch();
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
    // <https://tc39.es/ecma262/#sec-resolvebinding>
    // <https://tc39.es/ecma262/#sec-getvalue>
    let dst = cx.alloc_scratch();
    let name_idx = cx.intern_string_constant(id.name.as_str());
    cx.emit(
        Op::LoadGlobalOrThrow,
        [Operand::Register(dst), Operand::ConstIndex(name_idx)],
        span,
    );
    Ok(dst)
}
