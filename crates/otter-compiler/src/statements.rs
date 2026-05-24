//! Statement-level bytecode lowering and statement-specific helpers.
//!
//! # Contents
//! - general statement dispatch
//! - for-init declarations
//! - switch lowering
//! - label lowering
//!
//! # Invariants
//! - Statement lowering leaves expression results in the current scratch register when needed.
//!
//! # See also
//! - `for_loops` and `try_catch`

use crate::*;

/// Compile one statement. Returns `Some(reg)` when the statement is
/// an `ExpressionStatement` whose value should propagate as the
/// program's completion value; `None` otherwise.
pub(crate) fn compile_statement(
    cx: &mut Compiler,
    stmt: &Statement<'_>,
) -> Result<Option<u16>, CompileError> {
    if is_erased_ts_statement(stmt) {
        return Ok(None);
    }
    if let Some((node, span)) = rejected_ts_statement(stmt) {
        return Err(CompileError::TypeScriptUnsupported {
            node: node.to_string(),
            span,
        });
    }
    match stmt {
        Statement::EmptyStatement(_) => Ok(None),

        Statement::ExpressionStatement(es) => {
            let span = (es.span.start, es.span.end);
            let reg = compile_expr(cx, &es.expression, span)?;
            Ok(Some(reg))
        }

        Statement::BlockStatement(b) => {
            let span = (b.span.start, b.span.end);
            cx.enter_scope();
            let mut last = None;
            for inner in &b.body {
                if let Some(r) = compile_statement(cx, inner)? {
                    last = Some(r);
                }
            }
            cx.exit_scope();
            let _ = span;
            Ok(last)
        }

        Statement::VariableDeclaration(decl) => {
            let is_const = matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Const);
            let is_var = matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Var);
            // §14.3.2 VariableStatement — the binding itself was
            // hoisted to the enclosing function / script / module
            // variable scope by the entry-point pre-pass. Here we
            // only have to evaluate each initializer (when present)
            // and store it into the pre-bound storage.
            // <https://tc39.es/ecma262/#sec-variable-statement>
            if is_var {
                for declarator in &decl.declarations {
                    let span = (declarator.span.start, declarator.span.end);
                    if let oxc_ast::ast::BindingPattern::BindingIdentifier(id) = &declarator.id {
                        let name = id.name.as_str().to_string();
                        // No initializer → leave the hoisted
                        // `undefined` in place per §14.3.2.1
                        // RuntimeSemantics: Evaluation of
                        // VariableDeclaration step 1.
                        let Some(init) = &declarator.init else {
                            continue;
                        };
                        let info = cx.lookup_binding(&name).ok_or(CompileError::Unsupported {
                            node: format!("var `{name}` not pre-hoisted"),
                            span,
                        })?;
                        let init_reg = compile_expr(cx, init, span)?;
                        cx.emit_store_storage(init_reg, info.storage, span);
                        if cx.stack.len() == 1 && cx.module_state.is_none() {
                            let name_idx = cx.intern_string_constant(&name);
                            cx.emit(
                                Op::DefineGlobalVar,
                                [Operand::ConstIndex(name_idx), Operand::Register(init_reg)],
                                span,
                            );
                        }
                        cx.emit_module_export_mirror(&name, init_reg, span);
                        continue;
                    }
                    // Destructuring `var [a, b] = x`. Spec-correct
                    // semantics: every name was already hoisted —
                    // we walk the pattern and assign each component
                    // into the pre-existing var binding.
                    let init = declarator.init.as_ref().ok_or(CompileError::Unsupported {
                        node: "var destructuring requires an initializer".to_string(),
                        span,
                    })?;
                    let init_reg = compile_expr(cx, init, span)?;
                    destructure_assign(cx, init_reg, &declarator.id, span)?;
                }
                return Ok(None);
            }
            for declarator in &decl.declarations {
                let span = (declarator.span.start, declarator.span.end);
                // Fast path for the overwhelmingly common
                // `let x = init;` shape so the simple binding
                // doesn't pay an extra register copy.
                if let oxc_ast::ast::BindingPattern::BindingIdentifier(id) = &declarator.id {
                    let name = id.name.as_str().to_string();
                    // The entry-point lexical pre-pass already
                    // declared every top-level `let` / `const` name
                    // (TDZ until source-position store) so inner
                    // function declarations could capture them.
                    // Reuse the existing binding when it's there;
                    // otherwise (nested block-scoped declaration)
                    // create a fresh one.
                    let storage = match cx.lookup_in_current_scope(&name) {
                        Some(info) => info.storage,
                        None => cx.declare_binding(&name, is_const, span)?,
                    };
                    let init_reg = match &declarator.init {
                        Some(init) => compile_expr(cx, init, span)?,
                        None => {
                            let dst = cx.alloc_scratch();
                            cx.emit(Op::LoadUndefined, [Operand::Register(dst)], span);
                            dst
                        }
                    };
                    cx.emit_store_storage(init_reg, storage, span);
                    cx.mark_initialized(&name);
                    cx.emit_module_export_mirror(&name, init_reg, span);
                    continue;
                }
                // Destructuring binding (`let [a, b] = …` /
                // `let { x } = …`) — `var` bindings are rejected
                // earlier so we can route everything through the
                // shared destructuring helper.
                let init = declarator.init.as_ref().ok_or(CompileError::Unsupported {
                    node: "VariableDeclarator: destructuring requires an initializer".to_string(),
                    span,
                })?;
                let init_reg = compile_expr(cx, init, span)?;
                destructure_into(cx, init_reg, &declarator.id, span)?;
            }
            Ok(None)
        }

        Statement::IfStatement(s) => {
            let span = (s.span.start, s.span.end);
            let cond_reg = compile_expr(cx, &s.test, span)?;
            // JUMP_IF_FALSE → after consequent
            let jmp_if_false = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(cond_reg), span);
            compile_statement(cx, &s.consequent)?;
            if let Some(alt) = &s.alternate {
                // After consequent, unconditional JUMP past the
                // alternate.
                let jmp_end = cx.emit_branch_placeholder(Op::Jump, None, span);
                cx.patch_branch_to_here(jmp_if_false);
                compile_statement(cx, alt)?;
                cx.patch_branch_to_here(jmp_end);
            } else {
                cx.patch_branch_to_here(jmp_if_false);
            }
            Ok(None)
        }

        Statement::WhileStatement(s) => {
            let span = (s.span.start, s.span.end);
            let loop_top = cx.next_pc;
            cx.push_loop_frame(LoopFrame::iteration());
            let cond_reg = compile_expr(cx, &s.test, span)?;
            let exit_jmp = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(cond_reg), span);
            compile_statement(cx, &s.body)?;
            // Back-edge jump to loop top.
            let back_jmp = cx.emit_branch_placeholder(Op::Jump, None, span);
            cx.patch_branch(back_jmp, loop_top);
            cx.patch_branch_to_here(exit_jmp);
            let frame = cx.loops.pop().expect("loop frame disappeared");
            for pc in frame.continue_patches {
                cx.patch_branch(pc, loop_top);
            }
            for pc in frame.break_patches {
                cx.patch_branch_to_here(pc);
            }
            Ok(None)
        }

        Statement::DoWhileStatement(s) => {
            let span = (s.span.start, s.span.end);
            let body_top = cx.next_pc;
            cx.push_loop_frame(LoopFrame::iteration());
            compile_statement(cx, &s.body)?;
            let continue_target = cx.next_pc;
            let cond_reg = compile_expr(cx, &s.test, span)?;
            let back_jmp = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(cond_reg), span);
            cx.patch_branch(back_jmp, body_top);
            let frame = cx.loops.pop().expect("loop frame disappeared");
            for pc in frame.continue_patches {
                cx.patch_branch(pc, continue_target);
            }
            for pc in frame.break_patches {
                cx.patch_branch_to_here(pc);
            }
            Ok(None)
        }

        Statement::ForStatement(s) => {
            let span = (s.span.start, s.span.end);
            cx.enter_scope();
            // Initializer.
            if let Some(init) = &s.init {
                match init {
                    oxc_ast::ast::ForStatementInit::VariableDeclaration(decl) => {
                        compile_for_init_decl(cx, decl, span)?;
                    }
                    other => {
                        if let Some(expr) = init_to_expression(other) {
                            compile_expr(cx, expr, span)?;
                        }
                    }
                }
            }
            cx.push_loop_frame(LoopFrame::iteration());
            let test_top = cx.next_pc;
            // Test.
            let exit_patch = if let Some(test) = &s.test {
                let cond_reg = compile_expr(cx, test, span)?;
                Some(cx.emit_branch_placeholder(Op::JumpIfFalse, Some(cond_reg), span))
            } else {
                None
            };
            // Body.
            compile_statement(cx, &s.body)?;
            // Continue lands on the update.
            let update_pc = cx.next_pc;
            if let Some(update) = &s.update {
                compile_expr(cx, update, span)?;
            }
            let back_jmp = cx.emit_branch_placeholder(Op::Jump, None, span);
            cx.patch_branch(back_jmp, test_top);
            if let Some(p) = exit_patch {
                cx.patch_branch_to_here(p);
            }
            let frame = cx.loops.pop().expect("loop frame disappeared");
            for pc in frame.continue_patches {
                cx.patch_branch(pc, update_pc);
            }
            for pc in frame.break_patches {
                cx.patch_branch_to_here(pc);
            }
            cx.exit_scope();
            Ok(None)
        }

        Statement::ForOfStatement(s) => compile_for_of_statement(cx, s),

        Statement::BreakStatement(s) => {
            let span = (s.span.start, s.span.end);
            // §14.13 / §13.15: `break label;` targets the matching
            // labelled enclosing statement (loop or switch). Bare
            // `break;` targets the innermost loop or switch frame.
            // <https://tc39.es/ecma262/#sec-break-statement>
            let label = s.label.as_ref().map(|id| id.name.as_str().to_string());
            let target_idx = match &label {
                None => cx
                    .loops
                    .len()
                    .checked_sub(1)
                    .ok_or(CompileError::Unsupported {
                        node: "BreakStatement outside any loop or switch".to_string(),
                        span,
                    })?,
                Some(name) => cx
                    .loops
                    .iter()
                    .rposition(|f| f.label.as_deref() == Some(name.as_str()))
                    .ok_or_else(|| CompileError::Unsupported {
                        node: format!("BreakStatement: unknown label `{name}`"),
                        span,
                    })?,
            };
            let pc = cx.emit_branch_placeholder(Op::Jump, None, span);
            cx.loops[target_idx].break_patches.push(pc);
            Ok(None)
        }

        Statement::FunctionDeclaration(f) => {
            let span = (f.span.start, f.span.end);
            let name =
                f.id.as_ref()
                    .ok_or(CompileError::Unsupported {
                        node: "FunctionDeclaration without name".to_string(),
                        span,
                    })?
                    .name
                    .as_str()
                    .to_string();
            // §10.2.11 step 30 — top-level function declarations
            // were hoisted at scope entry by
            // `hoist_function_declarations`. Skip the source-position
            // re-emit when the name was hoisted; otherwise (nested
            // block declaration in strict mode) fall through to the
            // ordinary block-scoped binding path.
            if cx.hoisted_function_names.contains(&name) {
                return Ok(None);
            }
            let (function_id, captures) = compile_function_full(
                cx,
                &name,
                &f.params,
                &f.body,
                span,
                f.r#async,
                f.generator,
                false,
            )?;
            // §B.3.2 / §B.3.3 web-compat — in sloppy mode a nested
            // function declaration whose name collides with an
            // existing `var` (or pre-hoisted) binding in the same
            // scope reuses that binding rather than redeclaring.
            // The `var f = 123; if (true) function f(){}` shape at
            // global scope is the canonical case.
            //
            // <https://tc39.es/ecma262/#sec-block-level-function-declarations-web-legacy-compatibility-semantics>
            let storage = match cx.lookup_in_current_scope(&name) {
                Some(info) => info.storage,
                None => cx.declare_binding(&name, false, span)?,
            };
            let const_idx = cx.intern_function_id(function_id);
            let tmp = cx.alloc_scratch();
            emit_make_callable(cx, tmp, const_idx, &captures, false, span)?;
            cx.emit_store_storage(tmp, storage, span);
            cx.mark_initialized(&name);
            cx.emit_module_export_mirror(&name, tmp, span);
            Ok(None)
        }

        Statement::ReturnStatement(r) => {
            let span = (r.span.start, r.span.end);
            match &r.argument {
                Some(arg) => {
                    let reg = compile_expr(cx, arg, span)?;
                    cx.emit(Op::ReturnValue, [Operand::Register(reg)], span);
                }
                None => {
                    cx.emit(Op::ReturnUndefined, [], span);
                }
            }
            Ok(None)
        }

        Statement::ClassDeclaration(class) => {
            let span = (class.span.start, class.span.end);
            let name = class
                .id
                .as_ref()
                .ok_or(CompileError::Unsupported {
                    node: "ClassDeclaration without name".to_string(),
                    span,
                })?
                .name
                .as_str()
                .to_string();
            let class_reg = compile_class(cx, class, Some(&name))?;
            // The lexical pre-pass may have already declared this
            // class name (TDZ) so inner functions could capture it.
            let storage = match cx.lookup_in_current_scope(&name) {
                Some(info) => info.storage,
                None => cx.declare_binding(&name, false, span)?,
            };
            cx.emit_store_storage(class_reg, storage, span);
            cx.mark_initialized(&name);
            cx.emit_module_export_mirror(&name, class_reg, span);
            Ok(None)
        }

        Statement::ThrowStatement(s) => {
            let span = (s.span.start, s.span.end);
            let reg = compile_expr(cx, &s.argument, span)?;
            cx.emit(Op::Throw, [Operand::Register(reg)], span);
            Ok(None)
        }

        Statement::TryStatement(s) => compile_try_statement(cx, s),

        Statement::ImportDeclaration(decl) => {
            // Type-only `import type { … }` is erased earlier via
            // `is_erased_ts_statement`. Runtime imports were
            // pre-resolved by `compile_module_program`'s pre-pass:
            // the import-record upvalue is already populated and
            // identifier resolution routes references through
            // `imported_names`. Nothing left to do at the
            // statement site.
            //
            // Outside module mode, an import is a hard error: the
            // foundation rejects mixed script + module input.
            //
            // Spec: <https://tc39.es/ecma262/#sec-imports>
            let span = (decl.span.start, decl.span.end);
            if cx.module_state.is_none() {
                return Err(CompileError::Unsupported {
                    node: "ImportDeclaration outside ES-module fragment".to_string(),
                    span,
                });
            }
            Ok(None)
        }

        Statement::ExportNamedDeclaration(decl) => {
            // Three shapes:
            //   1. `export let x = …` / `export function f() {…}` /
            //      `export class C {…}` — the inner declaration
            //      compiles via the regular path and the export
            //      mirror runs because the pre-pass added the name
            //      to `exported_names`.
            //   2. `export { a, b as c }` — re-export of names that
            //      already live in the module body. We synthesise a
            //      property store on `module_env` for each spec.
            //   3. `export { x } from "./other.ts"` (re-export from
            //      another module) — read from the source's import
            //      record, write to `module_env`.
            //
            // Spec: <https://tc39.es/ecma262/#sec-exports>
            let span = (decl.span.start, decl.span.end);
            if cx.module_state.is_none() {
                return Err(CompileError::Unsupported {
                    node: "ExportNamedDeclaration outside ES-module fragment".to_string(),
                    span,
                });
            }
            if let Some(inner) = &decl.declaration {
                compile_export_inner_declaration(cx, inner, span)?;
            }
            // Mirror each `export { name as exported_name }` spec
            // through to module_env. When `decl.source` is set the
            // source is another module; we read from its import
            // record (already present courtesy of the pre-pass).
            let from_source = decl.source.as_ref().map(|s| s.value.as_str().to_string());
            for spec in &decl.specifiers {
                let exported = module_export_name_to_str(&spec.exported);
                let local = module_export_name_to_str(&spec.local);
                let value_reg = if let Some(src) = &from_source {
                    let record_uv = cx
                        .module_state
                        .as_ref()
                        .and_then(|s| s.import_records.get(src).copied())
                        .ok_or(CompileError::Unsupported {
                            node: format!(
                                "ExportNamedDeclaration: unresolved re-export source `{src}`"
                            ),
                            span,
                        })?;
                    let record_reg = cx.alloc_scratch();
                    cx.emit(
                        Op::LoadUpvalue,
                        vec![
                            Operand::Register(record_reg),
                            Operand::Imm32(record_uv as i32),
                        ],
                        span,
                    );
                    let dst = cx.alloc_scratch();
                    cx.emit_load_property(dst, record_reg, &local, span);
                    dst
                } else if decl.declaration.is_some() {
                    // The declaration arm already mirrored via
                    // emit_module_export_mirror. Nothing to do.
                    continue;
                } else {
                    // `export { name }` — read from the local
                    // binding (the body must have declared it).
                    let info = cx.lookup_binding(&local).ok_or(CompileError::Unsupported {
                        node: format!("export of undeclared `{local}`"),
                        span,
                    })?;
                    let dst = cx.alloc_scratch();
                    cx.emit_load_storage(dst, info.storage, span);
                    dst
                };
                let env_uv = cx
                    .module_state
                    .as_ref()
                    .map(|s| s.module_env_uv)
                    .expect("module_state checked above");
                let env_reg = cx.alloc_scratch();
                cx.emit(
                    Op::LoadUpvalue,
                    [Operand::Register(env_reg), Operand::Imm32(env_uv as i32)],
                    span,
                );
                cx.emit_store_property(env_reg, &exported, value_reg, span);
            }
            Ok(None)
        }

        Statement::ExportDefaultDeclaration(decl) => {
            // `export default expr` — evaluate the expression
            // then store on `module_env.default`.
            //
            // Spec: <https://tc39.es/ecma262/#sec-exports-runtime-semantics-evaluation>
            let span = (decl.span.start, decl.span.end);
            if cx.module_state.is_none() {
                return Err(CompileError::Unsupported {
                    node: "ExportDefaultDeclaration outside ES-module fragment".to_string(),
                    span,
                });
            }
            let value_reg = match &decl.declaration {
                oxc_ast::ast::ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                    // §15.2 — a named `export default function f(){}`
                    // is a HoistableDeclaration: the closure was
                    // already compiled and bound to `f` (and
                    // mirrored as `module_env.default`) by
                    // [`hoist_function_declarations`]. The source-
                    // position arm becomes a pure no-op so the
                    // hoist's instructions stay the single source
                    // of truth.
                    let hoisted_name =
                        f.id.as_ref()
                            .map(|id| id.name.as_str().to_string())
                            .filter(|name| cx.hoisted_function_names.contains(name));
                    if hoisted_name.is_some() {
                        return Ok(None);
                    }
                    let name =
                        f.id.as_ref()
                            .map(|id| id.name.as_str().to_string())
                            .unwrap_or_else(|| "default".to_string());
                    let (function_id, captures) = compile_function_full(
                        cx,
                        &name,
                        &f.params,
                        &f.body,
                        span,
                        f.r#async,
                        f.generator,
                        false,
                    )?;
                    let const_idx = cx.intern_function_id(function_id);
                    let dst = cx.alloc_scratch();
                    emit_make_callable(cx, dst, const_idx, &captures, false, span)?;
                    dst
                }
                oxc_ast::ast::ExportDefaultDeclarationKind::ClassDeclaration(c) => {
                    let name = c.id.as_ref().map(|id| id.name.as_str().to_string());
                    compile_class(cx, c, name.as_deref())?
                }
                other => {
                    let expr = other.as_expression().ok_or(CompileError::Unsupported {
                        node: "ExportDefaultDeclaration: unsupported declaration kind".to_string(),
                        span,
                    })?;
                    compile_expr(cx, expr, span)?
                }
            };
            let env_uv = cx
                .module_state
                .as_ref()
                .map(|s| s.module_env_uv)
                .expect("module_state checked above");
            let env_reg = cx.alloc_scratch();
            cx.emit(
                Op::LoadUpvalue,
                [Operand::Register(env_reg), Operand::Imm32(env_uv as i32)],
                span,
            );
            cx.emit_store_property(env_reg, "default", value_reg, span);
            Ok(None)
        }

        Statement::ExportAllDeclaration(decl) => {
            // `export * from "./other.ts"` and `export * as ns from
            // "./other.ts"`. Foundation supports the latter as a
            // simple re-export (the source module's namespace
            // becomes a property on module_env). The bare
            // `export *` would need to copy every own property of
            // the source's namespace into our module_env at the
            // moment of evaluation — doable but would also need
            // re-resolution of those names on every read for live
            // bindings, which the foundation does not yet model.
            // Reject the bare form for now.
            //
            // Spec: <https://tc39.es/ecma262/#sec-getmoduleexports>
            let span = (decl.span.start, decl.span.end);
            if cx.module_state.is_none() {
                return Err(CompileError::Unsupported {
                    node: "ExportAllDeclaration outside ES-module fragment".to_string(),
                    span,
                });
            }
            let source = decl.source.value.as_str().to_string();
            let record_uv = cx
                .module_state
                .as_ref()
                .and_then(|s| s.import_records.get(&source).copied())
                .ok_or(CompileError::Unsupported {
                    node: format!("ExportAllDeclaration: unresolved source `{source}`"),
                    span,
                })?;
            let exported_alias = decl
                .exported
                .as_ref()
                .map(module_export_name_to_str)
                .ok_or(CompileError::Unsupported {
                    node: "ExportAllDeclaration: bare `export *` not yet supported".to_string(),
                    span,
                })?;
            let record_reg = cx.alloc_scratch();
            cx.emit(
                Op::LoadUpvalue,
                vec![
                    Operand::Register(record_reg),
                    Operand::Imm32(record_uv as i32),
                ],
                span,
            );
            let env_uv = cx
                .module_state
                .as_ref()
                .map(|s| s.module_env_uv)
                .expect("module_state checked above");
            let env_reg = cx.alloc_scratch();
            cx.emit(
                Op::LoadUpvalue,
                [Operand::Register(env_reg), Operand::Imm32(env_uv as i32)],
                span,
            );
            cx.emit_store_property(env_reg, &exported_alias, record_reg, span);
            Ok(None)
        }

        Statement::ContinueStatement(s) => {
            let span = (s.span.start, s.span.end);
            // §14.8: `continue` targets the innermost real iteration
            // statement (switch frames are skipped per §13.10.1).
            // `continue label;` targets the matching labelled loop.
            // <https://tc39.es/ecma262/#sec-continue-statement>
            let label = s.label.as_ref().map(|id| id.name.as_str().to_string());
            let target_idx = match &label {
                None => cx.loops.iter().rposition(|f| f.is_real_loop).ok_or(
                    CompileError::Unsupported {
                        node: "ContinueStatement outside any loop".to_string(),
                        span,
                    },
                )?,
                Some(name) => {
                    let idx = cx
                        .loops
                        .iter()
                        .rposition(|f| f.label.as_deref() == Some(name.as_str()))
                        .ok_or_else(|| CompileError::Unsupported {
                            node: format!("ContinueStatement: unknown label `{name}`"),
                            span,
                        })?;
                    if !cx.loops[idx].is_real_loop {
                        return Err(CompileError::Unsupported {
                            node: format!("ContinueStatement: label `{name}` does not name a loop"),
                            span,
                        });
                    }
                    idx
                }
            };
            let pc = cx.emit_branch_placeholder(Op::Jump, None, span);
            cx.loops[target_idx].continue_patches.push(pc);
            Ok(None)
        }

        // §14.13 — `with` is forbidden in strict mode and ES modules.
        // Foundation is always strict, so reject with a clear
        // diagnostic rather than the generic "unsupported".
        // <https://tc39.es/ecma262/#sec-with-statement>
        Statement::WithStatement(w) => Err(CompileError::Unsupported {
            node: "WithStatement is forbidden in strict mode / ES modules (§14.13)".to_string(),
            span: (w.span.start, w.span.end),
        }),

        Statement::SwitchStatement(s) => compile_switch_statement(cx, s),

        Statement::ForInStatement(s) => compile_for_in_statement(cx, s),

        Statement::LabeledStatement(s) => compile_labeled_statement(cx, s),

        other => Err(CompileError::Unsupported {
            node: stmt_kind_name(other).to_string(),
            span: stmt_span(other),
        }),
    }
}

/// Helper for the `for(...; ...; ...)` initializer's
/// `let`/`const`/`var` declaration form. Mirrors the
/// `VariableDeclaration` arm of `compile_statement` but operates on
/// the borrowed declaration without re-cloning it through OXC's
/// allocator.
pub(crate) fn compile_for_init_decl(
    cx: &mut Compiler,
    decl: &oxc_ast::ast::VariableDeclaration<'_>,
    _span: (u32, u32),
) -> Result<(), CompileError> {
    let is_const = matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Const);
    let is_var = matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Var);
    for declarator in &decl.declarations {
        let span = (declarator.span.start, declarator.span.end);
        match &declarator.id {
            oxc_ast::ast::BindingPattern::BindingIdentifier(id) => {
                let name = id.name.as_str().to_string();
                // §14.7.4 ForLoopEvaluation — `var` re-uses the
                // function-scope binding pre-hoisted at function
                // entry; `let`/`const` declare a fresh per-loop
                // binding.
                let storage = if is_var {
                    cx.lookup_binding(&name)
                        .ok_or(CompileError::Unsupported {
                            node: format!("for-init var `{name}` not pre-hoisted"),
                            span,
                        })?
                        .storage
                } else {
                    cx.declare_binding(&name, is_const, span)?
                };
                let init_reg = match &declarator.init {
                    Some(init) => compile_expr(cx, init, span)?,
                    None => {
                        let dst = cx.alloc_scratch();
                        cx.emit(Op::LoadUndefined, [Operand::Register(dst)], span);
                        dst
                    }
                };
                cx.emit_store_storage(init_reg, storage, span);
                cx.mark_initialized(&name);
            }
            // §14.7.4 — destructuring init: `for (const [a, b] = …; …)`.
            // The initializer is required (let/const/var destructuring
            // without an initializer is a SyntaxError; oxc enforces
            // that).
            _ => {
                let init = declarator.init.as_ref().ok_or(CompileError::Unsupported {
                    node: "for-init destructuring without initializer".to_string(),
                    span,
                })?;
                let init_reg = compile_expr(cx, init, span)?;
                if is_var {
                    destructure_assign(cx, init_reg, &declarator.id, span)?;
                } else {
                    destructure_into(cx, init_reg, &declarator.id, span)?;
                }
            }
        }
    }
    Ok(())
}

/// Lower `switch (disc) { case ...; default: ...; }` per ECMA-262
/// §14.12 SwitchStatement.
///
/// # Algorithm
/// 1. Evaluate the discriminant once into a scratch register.
/// 2. Walk every `case label:` and emit a strict-equality compare
///    (`disc === label`) followed by `JUMP_IF_TRUE → case_body_pc`.
///    Patch the jump targets after we know each case body's pc.
/// 3. After all case probes, emit one unconditional `JUMP` to the
///    `default:` body when present, or to the switch end otherwise.
///    This implements the spec's two-pass `CaseSelector` evaluation:
///    cases run in source order, then `default` if no case matched.
/// 4. Compile every case body in source order, falling through into
///    the next on missing `break`. Each body's start pc is captured
///    so step 2's placeholders can be patched.
/// 5. Push a [`LoopFrame::switch_frame`] so `break` targets the
///    end of the switch and `continue` skips the frame entirely
///    (per §13.10.1).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-switch-statement>
/// - <https://tc39.es/ecma262/#sec-runtime-semantics-caseclauseisselected>
pub(crate) fn compile_switch_statement(
    cx: &mut Compiler,
    s: &oxc_ast::ast::SwitchStatement<'_>,
) -> Result<Option<u16>, CompileError> {
    let span = (s.span.start, s.span.end);
    let disc_reg = compile_expr(cx, &s.discriminant, span)?;

    // Fresh lexical scope so per-case `let` bindings don't leak.
    cx.enter_scope();
    cx.push_loop_frame(LoopFrame::switch_body());

    // Pass 1: emit selector comparisons for every non-default case.
    // The placeholders carry the target PC inside the JUMP_IF_TRUE
    // operand; we patch them once each body's pc is known.
    let mut case_jump_pcs: Vec<(usize, u32)> = Vec::with_capacity(s.cases.len());
    let mut default_idx: Option<usize> = None;
    for (idx, case) in s.cases.iter().enumerate() {
        let case_span = (case.span.start, case.span.end);
        match &case.test {
            Some(test) => {
                let test_reg = compile_expr(cx, test, case_span)?;
                let cmp_reg = cx.alloc_scratch();
                // §13.11.1 strict equality.
                cx.emit(
                    Op::Equal,
                    vec![
                        Operand::Register(cmp_reg),
                        Operand::Register(disc_reg),
                        Operand::Register(test_reg),
                    ],
                    case_span,
                );
                let placeholder =
                    cx.emit_branch_placeholder(Op::JumpIfTrue, Some(cmp_reg), case_span);
                case_jump_pcs.push((idx, placeholder));
            }
            None => {
                if default_idx.is_some() {
                    return Err(CompileError::Unsupported {
                        node: "SwitchStatement: multiple default clauses".to_string(),
                        span: case_span,
                    });
                }
                default_idx = Some(idx);
            }
        }
    }
    // Fall-through after all case probes — jump to default body if
    // present, else to the end of the switch.
    let default_jump = cx.emit_branch_placeholder(Op::Jump, None, span);

    // Pass 2: compile each case body in source order.
    let mut case_body_pcs: Vec<u32> = Vec::with_capacity(s.cases.len());
    for case in s.cases.iter() {
        let body_pc = cx.next_pc;
        case_body_pcs.push(body_pc);
        for inner in case.consequent.iter() {
            compile_statement(cx, inner)?;
        }
    }
    let switch_end_pc = cx.next_pc;

    // Patch case selector jumps to their body pc.
    for (idx, placeholder) in case_jump_pcs {
        cx.patch_branch(placeholder, case_body_pcs[idx]);
    }
    // Patch the post-probe fall-through jump.
    match default_idx {
        Some(idx) => cx.patch_branch(default_jump, case_body_pcs[idx]),
        None => cx.patch_branch(default_jump, switch_end_pc),
    }

    // Patch every `break` inside the switch to land at the end.
    let frame = cx.loops.pop().expect("switch frame disappeared");
    for pc in frame.break_patches {
        cx.patch_branch_to_here(pc);
    }
    debug_assert!(
        frame.continue_patches.is_empty(),
        "switch frames must not collect continue patches"
    );
    cx.exit_scope();
    Ok(None)
}

/// Lower `label: stmt` per ECMA-262 §14.13.
///
/// Foundation supports labels on iteration statements and `switch`
/// only — every other shape (block-statement label, function
/// declaration, etc.) is theoretically valid spec-wise but the
/// foundation keeps the surface tight. Adding the missing shapes is
/// purely a matter of stamping the label onto a fresh
/// [`LoopFrame::switch_frame`]-style entry and patching break
/// targets at scope close.
///
/// The label is attached to the enclosed statement by injecting it
/// into the loop / switch frame the inner statement pushes. We
/// detect the label site, stash the name in the next-pushed
/// frame, and recurse into the body — the inner compile_*
/// helpers consult `cx.loops.last()` already, so the label is
/// visible to nested `break label;` / `continue label;`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-labelled-statements>
pub(crate) fn compile_labeled_statement(
    cx: &mut Compiler,
    s: &oxc_ast::ast::LabeledStatement<'_>,
) -> Result<Option<u16>, CompileError> {
    let span = (s.span.start, s.span.end);
    let label = s.label.name.as_str().to_string();
    // Reject duplicate labels in the enclosing chain — §14.13.1
    // early error.
    if cx
        .loops
        .iter()
        .any(|f| f.label.as_deref() == Some(label.as_str()))
    {
        return Err(CompileError::Unsupported {
            node: format!("LabeledStatement: duplicate label `{label}` in enclosing chain"),
            span,
        });
    }
    // §14.13.4 — labels may wrap any Statement. Three cases:
    //
    // 1. The body is a loop / switch: stash the label as
    //    `pending_label`; the loop's frame consumes it on push.
    // 2. The body is a Block / If / Try / etc.: push a synthetic
    //    "labeled-block" frame so `break label;` from the body
    //    can branch past the block. `continue label;` is a
    //    SyntaxError for non-iteration labels per §13.8.1
    //    (foundation surfaces it at runtime via the loop walk).
    // 3. The body is a plain statement (`a: 1`): nothing to do
    //    after compiling the body.
    use oxc_ast::ast::Statement;
    let body_takes_loop_label = matches!(
        &s.body,
        Statement::ForStatement(_)
            | Statement::ForInStatement(_)
            | Statement::ForOfStatement(_)
            | Statement::WhileStatement(_)
            | Statement::DoWhileStatement(_)
            | Statement::SwitchStatement(_)
    );
    if body_takes_loop_label {
        let prev_pending = cx.pending_label.replace(label);
        let result = compile_statement(cx, &s.body);
        cx.pending_label = prev_pending;
        return result;
    }
    // Synthetic labeled block — push a non-loop frame so
    // `break label;` from inside the body has somewhere to land.
    let mut frame = LoopFrame::switch_body();
    frame.label = Some(label);
    cx.push_loop_frame(frame);
    let result = compile_statement(cx, &s.body);
    let frame = cx.loops.pop().expect("labeled-block frame");
    for pc in frame.break_patches {
        cx.patch_branch_to_here(pc);
    }
    let _ = span;
    result
}
