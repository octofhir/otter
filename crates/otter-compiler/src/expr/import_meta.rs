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
    let import_meta_uv =
        cx.module_state
            .as_ref()
            .map(|s| s.import_meta_uv)
            .ok_or(CompileError::Unsupported {
                node: "`import.meta` outside an ES-module fragment".to_string(),
                span,
            })?;
    let dst = cx.alloc_scratch();
    cx.emit(
        Op::LoadUpvalue,
        vec![
            Operand::Register(dst),
            Operand::Imm32(import_meta_uv as i32),
        ],
        span,
    );
    Ok(dst)
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
    if cx.module_state.is_none() {
        return Err(CompileError::Unsupported {
            node: "dynamic `import()` outside an ES-module fragment".to_string(),
            span,
        });
    }
    match unwrap_ts_expr(&imp.source) {
        Expression::StringLiteral(lit) => {
            // Literal: linker resolves it during fragment merge,
            // opcode reads namespace + wraps in a fulfilled
            // promise.
            let specifier = lit.value.as_str().to_string();
            let spec_const = cx.intern_string_constant(&specifier);
            let ns_dst = cx.alloc_scratch();
            cx.emit(
                Op::ImportNamespace,
                [Operand::Register(ns_dst), Operand::ConstIndex(spec_const)],
                span,
            );
            let promise_dst = cx.alloc_scratch();
            cx.emit(
                Op::PromiseFulfilledOf,
                [Operand::Register(promise_dst), Operand::Register(ns_dst)],
                span,
            );
            Ok(promise_dst)
        }
        other => {
            // Non-literal: opcode returns a Promise<namespace>
            // (or Promise<TypeError>) directly, so no
            // PromiseFulfilledOf wrap is needed.
            let spec_reg = compile_expr(cx, other, span)?;
            let promise_dst = cx.alloc_scratch();
            cx.emit(
                Op::ImportNamespaceDynamic,
                [Operand::Register(promise_dst), Operand::Register(spec_reg)],
                span,
            );
            Ok(promise_dst)
        }
    }
}
