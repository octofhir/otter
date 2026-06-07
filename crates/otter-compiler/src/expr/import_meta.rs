//! Import and meta-property expression lowering.
//!
//! # Contents
//! - [`compile_meta_property`] — lowers `new.target` and `import.meta`.
//! - [`compile_import`] — lowers dynamic `import()` expressions.
//!
//! # See also
//! - [`super`] — expression dispatch and shared helpers.

use crate::*;
use oxc_ast::ast::{ImportExpression, MetaProperty};

pub(crate) fn compile_meta_property(
    cx: &mut Compiler,
    meta: &MetaProperty<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let _ = span;
    let span = (meta.span.start, meta.span.end);
    if meta.meta.name.as_str() == "new" && meta.property.name.as_str() == "target" {
        let dst = cx.alloc_scratch();
        cx.emit(Op::LoadNewTarget, [Operand::Register(dst)], span);
        return Ok(dst);
    }
    // The only legal MetaProperty inside a module is
    // `import.meta`. The runtime materialises it as a
    // JsObject the linker passes in as param 1; we hoist
    // it into `import_meta_uv` at function entry so
    // closures capture it.
    //
    // Spec: <https://tc39.es/ecma262/#prod-ImportMeta>
    //       <https://tc39.es/ecma262/#sec-meta-properties-runtime-semantics-evaluation>
    if meta.meta.name.as_str() != "import" || meta.property.name.as_str() != "meta" {
        return Err(CompileError::Unsupported {
            node: format!(
                "MetaProperty other than `import.meta` ({}.{})",
                meta.meta.name, meta.property.name
            ),
            span,
        });
    }
    if let Some(import_meta_uv) = cx.module_state.as_ref().map(|s| s.import_meta_uv) {
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::LoadUpvalue,
            vec![
                Operand::Register(dst),
                Operand::Imm32(import_meta_uv as i32),
            ],
            span,
        );
        return Ok(dst);
    }
    // Nested function inside a module: `module_state` lives on the
    // module-init frame only, but the init scope registers a
    // synthetic `__otter_import_meta` binding, so inner functions
    // reach the same object through the regular capture cascade.
    let name = crate::synthetic::import_meta_synthetic_name();
    if cx.resolve_capture(&name).is_some() || cx.lookup_binding(&name).is_some() {
        return crate::class::load_synthetic_capture(cx, &name, span);
    }
    Err(CompileError::Unsupported {
        node: "`import.meta` outside an ES-module fragment".to_string(),
        span,
    })
}

pub(crate) fn compile_import(
    cx: &mut Compiler,
    imp: &ImportExpression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let _ = span;
    // §16.2.1.7 ImportCall: literal-string specifiers are
    // pre-resolved by the linker (synchronous namespace lookup
    // wrapped in a fulfilled promise). Non-literal specifiers
    // route through `Op::ImportNamespaceDynamic` which always
    // returns a [`crate::Value::Promise`] directly — fulfilled
    // for a specifier that resolves against the pre-linked
    // module graph; rejected with a TypeError when the runtime
    // cannot satisfy the specifier (no on-demand loader for
    // brand-new modules in this slice).
    //
    // Spec: <https://tc39.es/ecma262/#sec-import-call-runtime-semantics-evaluation>
    let span = (imp.span.start, imp.span.end);
    let is_defer_phase = matches!(imp.phase, Some(oxc_ast::ast::ImportPhase::Defer));
    // §13.3.10.1 EvaluateImportCall steps 2-4 — the specifier
    // expression evaluates first, then the options expression; an
    // abrupt completion from either propagates synchronously before
    // any import machinery runs.
    let spec_reg = match unwrap_ts_expr(&imp.source) {
        Expression::StringLiteral(lit) => {
            // Literal import.defer in module code: linker resolves
            // it during fragment merge, opcode reads the deferred
            // namespace and wraps it in a fulfilled promise. In
            // script code there is no module graph/upvalue state, so
            // it uses the same host dynamic-import promise path as
            // import().
            let specifier = lit.value.as_str().to_string();
            let spec_const = cx.intern_string_constant(&specifier);
            if is_defer_phase && cx.module_state.is_some() {
                if let Some(options) = &imp.options {
                    compile_expr(cx, options, span)?;
                }
                let ns_dst = cx.alloc_scratch();
                cx.emit(
                    Op::ImportNamespaceDeferred,
                    [Operand::Register(ns_dst), Operand::ConstIndex(spec_const)],
                    span,
                );
                let promise_dst = cx.alloc_scratch();
                cx.emit(
                    Op::PromiseFulfilledOf,
                    [Operand::Register(promise_dst), Operand::Register(ns_dst)],
                    span,
                );
                return Ok(promise_dst);
            }
            let spec_reg = cx.alloc_scratch();
            cx.emit(
                Op::LoadString,
                [Operand::Register(spec_reg), Operand::ConstIndex(spec_const)],
                span,
            );
            spec_reg
        }
        // Non-literal: opcode returns a Promise<namespace>
        // (or Promise<TypeError>) directly, so no
        // PromiseFulfilledOf wrap is needed.
        other => compile_expr(cx, other, span)?,
    };
    if let Some(options) = &imp.options {
        compile_expr(cx, options, span)?;
    }
    let promise_dst = cx.alloc_scratch();
    cx.emit(
        Op::ImportNamespaceDynamic,
        [Operand::Register(promise_dst), Operand::Register(spec_reg)],
        span,
    );
    Ok(promise_dst)
}
