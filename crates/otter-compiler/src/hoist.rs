//! Declaration hoisting and lightweight pre-pass helpers.
//!
//! # Contents
//! - var and lexical name collection
//! - function declaration pre-emission
//! - arguments and top-level-await detection
//!
//! # Invariants
//! - Pre-passes collect names without emitting unrelated runtime effects.
//!
//! # See also
//! - `entry` and `statements`

use crate::*;

/// Walk `stmts` collecting every `var`-declared name reachable
/// without crossing a function or class boundary. Per ECMA-262
/// §8.1.6 VarScopedDeclarations, these names belong to the
/// enclosing function (or script / module) variable environment.
///
/// Walks through:
/// - Block statements (`{ var x; }`).
/// - `if / else`, `while`, `do-while`, `for(;;)`, `for-in`, `for-of`,
///   `switch` cases, `try / catch / finally`, labelled statements.
/// - The init clause of `for(var ... ; ; )` and the head of
///   `for(var x in/of ...)`.
///
/// Stops at function / class declarations: their bodies own their
/// own variable scope.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-static-semantics-varscopeddeclarations>
pub(crate) fn hoist_var_names<'a>(stmts: &[Statement<'a>], out: &mut Vec<String>) {
    for stmt in stmts {
        hoist_var_names_in_stmt_at(stmt, out, true);
    }
}

fn hoist_var_names_nested<'a>(stmts: &[Statement<'a>], out: &mut Vec<String>) {
    for stmt in stmts {
        hoist_var_names_in_stmt_at(stmt, out, false);
    }
}

fn hoist_var_names_in_stmt_at<'a>(stmt: &Statement<'a>, out: &mut Vec<String>, top: bool) {
    match stmt {
        Statement::VariableDeclaration(d)
            if matches!(d.kind, oxc_ast::ast::VariableDeclarationKind::Var) =>
        {
            for declarator in d.declarations.iter() {
                collect_pattern_var_names(&declarator.id, out);
            }
        }
        // §8.2.5 VarScopedDeclarations — a *top-level* function
        // declaration is var-scoped (§10.2.11 step 34). Block-nested
        // declarations are lexical within their block; the sloppy-mode
        // §B.3.3 var extension for them runs through
        // [`pre_declare_annex_b_functions`], which applies the
        // early-error blockers this indiscriminate walk cannot see.
        Statement::FunctionDeclaration(f) if top && !f.declare => {
            if let Some(id) = &f.id {
                out.push(id.name.as_str().to_string());
            }
        }
        // §16.2.3.7 ExportEntry — `export var x` shares the
        // module's `var`-hoisted scope: the name must be
        // pre-declared at the module-init top, exactly as for a
        // bare `var x`. The export side-effect (mirroring into
        // `module_env`) runs at the source position via the
        // export arm; here we only need to surface the name.
        Statement::ExportNamedDeclaration(decl) if !decl.export_kind.is_type() => {
            if let Some(oxc_ast::ast::Declaration::VariableDeclaration(v)) = &decl.declaration
                && matches!(v.kind, oxc_ast::ast::VariableDeclarationKind::Var)
            {
                for declarator in v.declarations.iter() {
                    collect_pattern_var_names(&declarator.id, out);
                }
            }
        }
        Statement::BlockStatement(b) => hoist_var_names_nested(&b.body, out),
        Statement::IfStatement(s) => {
            hoist_var_names_in_stmt_at(&s.consequent, out, false);
            if let Some(alt) = &s.alternate {
                hoist_var_names_in_stmt_at(alt, out, false);
            }
        }
        Statement::WhileStatement(s) => hoist_var_names_in_stmt_at(&s.body, out, false),
        Statement::DoWhileStatement(s) => hoist_var_names_in_stmt_at(&s.body, out, false),
        Statement::ForStatement(s) => {
            if let Some(oxc_ast::ast::ForStatementInit::VariableDeclaration(d)) = &s.init
                && matches!(d.kind, oxc_ast::ast::VariableDeclarationKind::Var)
            {
                for declarator in d.declarations.iter() {
                    collect_pattern_var_names(&declarator.id, out);
                }
            }
            hoist_var_names_in_stmt_at(&s.body, out, false);
        }
        Statement::ForInStatement(s) => {
            if let oxc_ast::ast::ForStatementLeft::VariableDeclaration(d) = &s.left
                && matches!(d.kind, oxc_ast::ast::VariableDeclarationKind::Var)
            {
                for declarator in d.declarations.iter() {
                    collect_pattern_var_names(&declarator.id, out);
                }
            }
            hoist_var_names_in_stmt_at(&s.body, out, false);
        }
        Statement::ForOfStatement(s) => {
            if let oxc_ast::ast::ForStatementLeft::VariableDeclaration(d) = &s.left
                && matches!(d.kind, oxc_ast::ast::VariableDeclarationKind::Var)
            {
                for declarator in d.declarations.iter() {
                    collect_pattern_var_names(&declarator.id, out);
                }
            }
            hoist_var_names_in_stmt_at(&s.body, out, false);
        }
        Statement::SwitchStatement(s) => {
            for case in s.cases.iter() {
                hoist_var_names_nested(&case.consequent, out);
            }
        }
        Statement::TryStatement(s) => {
            hoist_var_names_nested(&s.block.body, out);
            if let Some(handler) = &s.handler {
                hoist_var_names_nested(&handler.body.body, out);
            }
            if let Some(finalizer) = &s.finalizer {
                hoist_var_names_nested(&finalizer.body, out);
            }
        }
        Statement::LabeledStatement(s) => hoist_var_names_in_stmt_at(&s.body, out, false),
        // §14.11 — `with (expr) stmt` does not open a var scope; its
        // body's `var` declarations hoist to the enclosing function /
        // script scope like any other nested statement.
        Statement::WithStatement(s) => hoist_var_names_in_stmt_at(&s.body, out, false),
        // `function`, `class`, plain expressions, etc. — none
        // contribute var-declared names to this scope.
        _ => {}
    }
}

/// Collect every binding identifier reachable from `pattern` —
/// supports plain identifiers and the destructuring patterns the
/// foundation accepts.
pub(crate) fn collect_pattern_var_names(
    pattern: &oxc_ast::ast::BindingPattern<'_>,
    out: &mut Vec<String>,
) {
    use oxc_ast::ast::BindingPattern;
    match pattern {
        BindingPattern::BindingIdentifier(id) => out.push(id.name.as_str().to_string()),
        BindingPattern::ArrayPattern(p) => {
            for elem in p.elements.iter().flatten() {
                collect_pattern_var_names(elem, out);
            }
            if let Some(rest) = &p.rest {
                collect_pattern_var_names(&rest.argument, out);
            }
        }
        BindingPattern::ObjectPattern(p) => {
            for prop in p.properties.iter() {
                collect_pattern_var_names(&prop.value, out);
            }
            if let Some(rest) = &p.rest {
                collect_pattern_var_names(&rest.argument, out);
            }
        }
        BindingPattern::AssignmentPattern(p) => collect_pattern_var_names(&p.left, out),
    }
}

/// Pre-declare each hoisted `var` name on the current scope per
/// §10.2.11 FunctionDeclarationInstantiation step 28: bind to
/// `undefined` with `[[Mutable]]`, no TDZ. Names that already live
/// in the current scope (formal parameters, `let`/`const` shadowing,
/// the function's self-name) are skipped — this matches §10.2.11
/// step 27 ("If the same name is bound by both a parameter and a
/// VarDeclaration, the parameter binding wins").
pub(crate) fn pre_declare_var_bindings(
    cx: &mut Compiler,
    names: &[String],
    span: (u32, u32),
) -> Result<(), CompileError> {
    let mut seen: HashSet<String> = HashSet::new();
    for name in names {
        if !seen.insert(name.clone()) {
            continue;
        }
        if cx.lookup_binding(name).is_some() {
            // Already bound by a parameter or the function self-name —
            // §10.2.11 step 27.b leaves the existing binding intact.
            continue;
        }
        let storage = cx.declare_binding(name, false, span)?;
        let dst = cx.alloc_scratch();
        cx.emit(Op::LoadUndefined, [Operand::Register(dst)], span);
        cx.emit_store_storage(dst, storage, span);
        cx.mark_initialized(name);
    }
    Ok(())
}

/// Walk `stmts` collecting every top-level lexical declaration name
/// — `let x`, `const x`, `class C` — that lives at the current
/// function / script / module scope. Top-level only: nested blocks,
/// loop bodies, etc., own their own block scope and aren't touched.
///
/// Names are returned with their declaration kind so the pre-pass
/// can call [`Compiler::declare_binding`] with the right `is_const`
/// flag.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-static-semantics-lexicallydeclarednames>
pub(crate) fn hoist_lexical_names(stmts: &[Statement<'_>], out: &mut Vec<(String, bool)>) {
    for stmt in stmts {
        match stmt {
            Statement::VariableDeclaration(d)
                if matches!(
                    d.kind,
                    oxc_ast::ast::VariableDeclarationKind::Let
                        | oxc_ast::ast::VariableDeclarationKind::Const
                ) =>
            {
                collect_lexical_var_names(d, out);
            }
            Statement::ClassDeclaration(c) => {
                if let Some(id) = &c.id {
                    out.push((id.name.as_str().to_string(), false));
                }
            }
            // §16.2.3.7 — `export let x` / `export const x` /
            // `export class C` introduce a fresh module-scope
            // lexical binding that must be pre-declared in TDZ
            // before module-init runs, just like any other
            // top-level lexical name. `export function` is
            // handled by [`hoist_function_declarations`].
            //
            // `export var x` is a `var`-scoped binding picked up
            // by [`hoist_var_names_in_stmt`]; we explicitly skip
            // it here so the name doesn't end up double-bound.
            Statement::ExportNamedDeclaration(decl) if !decl.export_kind.is_type() => {
                match &decl.declaration {
                    Some(oxc_ast::ast::Declaration::VariableDeclaration(v))
                        if matches!(
                            v.kind,
                            oxc_ast::ast::VariableDeclarationKind::Let
                                | oxc_ast::ast::VariableDeclarationKind::Const
                        ) =>
                    {
                        collect_lexical_var_names(v, out);
                    }
                    Some(oxc_ast::ast::Declaration::ClassDeclaration(c)) => {
                        if let Some(id) = &c.id {
                            out.push((id.name.as_str().to_string(), false));
                        }
                    }
                    _ => {}
                }
            }
            // `export default class C {}` and `export default
            // expression` do not contribute a top-level lexical
            // name in the foundation slice — the value lives on
            // `module_env.default` and the source-position
            // [`Statement::ExportDefaultDeclaration`] arm wires
            // that store. Per §15.2.3.5 a *named* default class
            // creates a module-scope binding `C`; that binding is
            // a separate spec slice and is filed as a follow-up.
            // `export default function f(){}` is a hoistable
            // declaration; its name lands at the top of
            // [`hoist_function_declarations`].
            // Don't recurse into blocks / control-flow constructs:
            // those declarations belong to the inner block scope,
            // not the enclosing function / module body.
            _ => {}
        }
    }
}

/// Push every plain-identifier name declared by a `let`/`const`
/// declaration into `out` with its `is_const` flag. Shared
/// between the source-statement arm and the
/// `ExportNamedDeclaration(VariableDeclaration)` arm so both pre-
/// hoist passes apply identical rules to the inner declaration.
pub(crate) fn collect_lexical_var_names(
    d: &oxc_ast::ast::VariableDeclaration<'_>,
    out: &mut Vec<(String, bool)>,
) {
    let is_const = matches!(d.kind, oxc_ast::ast::VariableDeclarationKind::Const);
    for declarator in d.declarations.iter() {
        // Destructuring leaves pre-declare alongside plain
        // identifiers (TDZ); `destructure_pattern` re-uses the
        // uninitialized binding at the source position.
        if !matches!(
            &declarator.id,
            oxc_ast::ast::BindingPattern::BindingIdentifier(_)
        ) {
            let mut leaves: Vec<String> = Vec::new();
            collect_pattern_var_names(&declarator.id, &mut leaves);
            out.extend(leaves.into_iter().map(|n| (n, is_const)));
            continue;
        }
        if let oxc_ast::ast::BindingPattern::BindingIdentifier(id) = &declarator.id {
            out.push((id.name.as_str().to_string(), is_const));
        }
    }
}

/// Pre-declare each top-level lexical name from
/// [`hoist_lexical_names`] so inner function declarations (which
/// hoist *above* the lexical declarations) can resolve their
/// captures. Bindings start in TDZ — the source-level `let` /
/// `const` / `class` arm flips them to initialized once the
/// initialiser stores its value.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-functiondeclarationinstantiation>
///   step 33 (CreateMutableBinding for `let`, CreateImmutableBinding
///   for `const`).
pub(crate) fn pre_declare_lexical_bindings(
    cx: &mut Compiler,
    names: &[(String, bool)],
    span: (u32, u32),
) -> Result<(), CompileError> {
    for (name, is_const) in names {
        if cx.lookup_binding(name).is_some() {
            // Already pre-declared (parameter, var-hoist clash, …).
            // §10.2.11 forbids let / const / class from shadowing a
            // var-hoisted name at the same scope; surface a clear
            // diagnostic at declaration time rather than here.
            continue;
        }
        // §10.2.11 step 33 — a captured top-level lexical lives in an
        // own-upvalue cell so closures can observe it. The cell starts
        // life holding `undefined`, which a closure that runs before
        // the source-level declaration would misread as an initialized
        // value. Install the TDZ hole now so such a forward read is a
        // `ReferenceError` until the declaration's store clears it.
        // The hole is written INTO the existing cell (not via
        // `Op::FreshUpvalue`, which replaces the cell) so module-init
        // link/eval phases sharing one persistent spine observe the
        // same cell.
        if let crate::scope::BindingStorage::Upvalue { idx } =
            cx.declare_binding(name, *is_const, span)?
        {
            let hole = cx.alloc_scratch();
            cx.emit(Op::LoadHole, [Operand::Register(hole)], span);
            cx.emit(
                Op::StoreUpvalue,
                [Operand::Register(hole), Operand::Imm32(idx as i32)],
                span,
            );
        }
    }
    Ok(())
}

/// §14.2.3 BlockDeclarationInstantiation — pre-declare a block's
/// top-level lexical names (TDZ) at block entry, so case selectors,
/// closures, and statements textually before the declaration resolve
/// to the block binding instead of an outer same-named one. Unlike
/// [`pre_declare_lexical_bindings`], an outer binding does not
/// suppress the block-scope declaration — only a same-scope one does.
pub(crate) fn pre_declare_block_lexical_bindings(
    cx: &mut Compiler,
    names: &[(String, bool)],
    captured_names: &std::collections::HashSet<String>,
    span: (u32, u32),
) -> Result<(), CompileError> {
    for (name, is_const) in names {
        if cx.lookup_in_current_scope(name).is_some() {
            continue;
        }
        // Same TDZ-hole protocol as the function-top-level pre-pass:
        // a captured binding's cell is holed in place so a closure
        // running before the declaration reads a ReferenceError.
        if let crate::scope::BindingStorage::Upvalue { idx } =
            cx.declare_binding_with_capture(name, *is_const, span, captured_names.contains(name))?
        {
            let hole = cx.alloc_scratch();
            cx.emit(Op::LoadHole, [Operand::Register(hole)], span);
            cx.emit(
                Op::StoreUpvalue,
                [Operand::Register(hole), Operand::Imm32(idx as i32)],
                span,
            );
        }
    }
    Ok(())
}

/// Hoist top-level `function f() {…}` declarations from `stmts` to
/// the start of the current function / script / module scope, per
/// ECMA-262 §10.2.11 FunctionDeclarationInstantiation step 30.
///
/// # Algorithm
/// 1. Find every `FunctionDeclaration` in the *direct* statement
///    list. Per §10.2.11 step 14 the LAST declaration with a given
///    name wins — earlier siblings are pre-empted at the binding
///    site (their bytecode still emits because OXC parses each
///    independently, but only the last store survives).
/// 2. For each surviving declaration: pre-declare its name, compile
///    the function body, materialise the closure value, store it
///    into the binding, and mark the binding initialised. Record the
///    name in `cx.hoisted_function_names` so the source-position
///    arm in `compile_statement` becomes a no-op.
///
/// Block-nested `FunctionDeclaration`s (`if (true) { function f(){} }`)
/// are *not* hoisted by this pass — the foundation models them as
/// block-scoped per the ES strict-mode rule. Annex B sloppy-mode
/// extensions are out of scope.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-functiondeclarationinstantiation>
/// - <https://tc39.es/ecma262/#sec-globaldeclarationinstantiation>
pub(crate) fn hoist_function_declarations(
    cx: &mut Compiler,
    stmts: &[Statement<'_>],
) -> Result<(), CompileError> {
    let refs: Vec<&Statement<'_>> = stmts.iter().collect();
    hoist_function_declarations_from(cx, &refs)
}

/// Statement-reference variant of [`hoist_function_declarations`]
/// for callers whose statement list spans several AST nodes (the
/// switch CaseBlock).
pub(crate) fn hoist_function_declarations_from(
    cx: &mut Compiler,
    stmts: &[&Statement<'_>],
) -> Result<(), CompileError> {
    use std::collections::HashMap;
    // Resolve each statement to its hoistable `FunctionDeclaration`
    // payload, including the export-wrapped forms `export function`
    // and `export default function`. Returns `None` for other
    // statements and for `function f.declare`-only TS shapes.
    fn hoistable_function<'b, 'a: 'b>(
        stmt: &'b Statement<'a>,
    ) -> Option<&'b oxc_ast::ast::Function<'a>> {
        match stmt {
            Statement::FunctionDeclaration(f) if !f.declare => Some(f),
            Statement::ExportNamedDeclaration(decl) if !decl.export_kind.is_type() => {
                if let Some(oxc_ast::ast::Declaration::FunctionDeclaration(f)) = &decl.declaration
                    && !f.declare
                {
                    Some(f)
                } else {
                    None
                }
            }
            // §15.2 ExportDefaultDeclaration with a
            // `FunctionDeclaration` payload hoists when the
            // function carries a binding identifier (per HoistableDeclaration).
            // Anonymous default functions are evaluated at the
            // export's source position by the export arm; they
            // don't introduce a module-scope name.
            Statement::ExportDefaultDeclaration(decl) => {
                if let oxc_ast::ast::ExportDefaultDeclarationKind::FunctionDeclaration(f) =
                    &decl.declaration
                    && !f.declare
                    && f.id.is_some()
                {
                    Some(f)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    // Pass 1 — last-occurrence-wins: identify the surviving
    // declaration index per name.
    let mut last_idx: HashMap<String, usize> = HashMap::new();
    for (idx, stmt) in stmts.iter().enumerate() {
        if let Some(f) = hoistable_function(stmt)
            && let Some(id) = &f.id
        {
            last_idx.insert(id.name.as_str().to_string(), idx);
        }
    }
    // Pass 2 — pre-declare each surviving name in the current scope
    // so mutual references between hoisted siblings (and forward
    // references from inner functions) all resolve before any body
    // is compiled. The binding is initialised to undefined; the
    // closure value lands in pass 3.
    for (idx, stmt) in stmts.iter().enumerate() {
        let Some(f) = hoistable_function(stmt) else {
            continue;
        };
        let Some(id) = &f.id else {
            continue;
        };
        let name = id.name.as_str().to_string();
        if last_idx.get(&name) != Some(&idx) {
            continue;
        }
        let span = (f.span.start, f.span.end);
        // Script global code — the function binding is the global
        // object property the var pre-pass already declared; no
        // local binding exists (§16.1.7 step 17).
        let script_global =
            cx.stack.len() == 1 && cx.scopes.len() == 1 && cx.script_global_vars.contains(&name);
        // Only a binding in the *current* scope (a parameter, a
        // var-hoisted name, a sibling declaration) is reused; an
        // outer-scope binding must not be captured by a block-level
        // declaration's instantiation.
        if !script_global && cx.lookup_in_current_scope(&name).is_none() {
            let storage = cx.declare_binding(&name, false, span)?;
            let dst = cx.alloc_scratch();
            cx.emit(Op::LoadUndefined, [Operand::Register(dst)], span);
            cx.emit_store_storage(dst, storage, span);
            cx.mark_initialized(&name);
        }
        cx.top_mut().hoisted_function_names.insert(name);
    }
    // Pass 3 — compile each surviving function body and store the
    // resulting closure into the pre-bound slot. With every name
    // already declared, mutually-recursive declarations bind
    // correctly regardless of source order.
    for (idx, stmt) in stmts.iter().enumerate() {
        let Some(f) = hoistable_function(stmt) else {
            continue;
        };
        let Some(id) = &f.id else {
            continue;
        };
        let name = id.name.as_str().to_string();
        if last_idx.get(&name) != Some(&idx) {
            continue;
        }
        let span = (f.span.start, f.span.end);
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
        let tmp = cx.alloc_scratch();
        emit_make_callable(cx, tmp, const_idx, &captures, false, span)?;
        let script_global =
            cx.stack.len() == 1 && cx.scopes.len() == 1 && cx.script_global_vars.contains(&name);
        if script_global {
            // §16.1.7 step 17 / §19.2.1.3 step 16.a —
            // CreateGlobalFunctionBinding: the global object property
            // *is* the binding; eval bindings are deletable.
            let name_idx = cx.intern_string_constant(&name);
            let deletable = i32::from(cx.in_eval);
            cx.emit(
                Op::DefineGlobalFunction,
                [
                    Operand::ConstIndex(name_idx),
                    Operand::Register(tmp),
                    Operand::Imm32(deletable),
                ],
                span,
            );
        } else {
            let storage = cx
                .lookup_in_current_scope(&name)
                .expect("pass 2 pre-declared the binding")
                .storage;
            cx.emit_store_storage(tmp, storage, span);
            // §16.1.7 GlobalDeclarationInstantiation step 17 — a script
            // top-level function declaration creates an own property on
            // the global object, so sibling scripts and eval chunks can
            // resolve it. Mirrors the `var` initializer path in
            // `compile_statement`. Strict eval bodies keep their own
            // variable environment (§19.2.1.1) and don't mirror.
            if cx.stack.len() == 1
                && cx.scopes.len() == 1
                && cx.module_state.is_none()
                && !cx.suppress_global_mirror
            {
                let name_idx = cx.intern_string_constant(&name);
                cx.emit(
                    Op::DefineGlobalVar,
                    [Operand::ConstIndex(name_idx), Operand::Register(tmp)],
                    span,
                );
            }
        }
        // Mirror through to `module_env` for `export function f`
        // (and `export default function f` — its export entry
        // landed under `default` from the pre-pass; the named
        // mirror is harmless for non-exported names because
        // `emit_module_export_mirror` filters on
        // `module_state.exported_names`).
        cx.emit_module_export_mirror(&name, tmp, span);
        // §15.2 — `export default function f(){}` also requires
        // `module_env.default = f`. Detect by walking the source
        // statement: when the surviving declaration came from an
        // export-default the `default` mirror must fire too.
        if matches!(stmts.get(idx), Some(Statement::ExportDefaultDeclaration(_))) {
            cx.emit_module_export_default_mirror(tmp, span);
        }
    }
    // §15.2.1.7 / §16.2.1.7.1 — an *anonymous* `export default
    // function/function*` is a HoistableDeclaration named "default":
    // it is initialized at instantiation, so `module_env.default` must
    // hold the closure before any body statement (incl. a self-import
    // observing it). Compile + mirror it here; the source-position arm
    // then skips it.
    for stmt in stmts.iter() {
        let Statement::ExportDefaultDeclaration(decl) = stmt else {
            continue;
        };
        let oxc_ast::ast::ExportDefaultDeclarationKind::FunctionDeclaration(f) = &decl.declaration
        else {
            continue;
        };
        if f.declare || f.id.is_some() {
            continue;
        }
        let span = (f.span.start, f.span.end);
        let (function_id, captures) = compile_function_full(
            cx,
            "default",
            &f.params,
            &f.body,
            span,
            f.r#async,
            f.generator,
            false,
        )?;
        let const_idx = cx.intern_function_id(function_id);
        let tmp = cx.alloc_scratch();
        emit_make_callable(cx, tmp, const_idx, &captures, false, span)?;
        cx.emit_module_export_default_mirror(tmp, span);
        cx.top_mut().default_function_hoisted = true;
    }
    Ok(())
}

/// Compile a function body into a fresh `Function` record and
/// return its id together with the captures it inherits from
/// `parent`. Parameters live in registers `0..param_count` (the
/// raw incoming argv slots); each one is then destructured /
/// defaulted / aliased into named bindings as the body expects.
/// Rest parameters (`...t`) are materialised by the runtime via
/// [`Op::CollectRest`] reading from the call frame's stashed
/// trailing argument list.
/// `true` when any expression at module-top-level (i.e. outside
/// any function / class body) uses `await`. The compiler upgrades
/// `<main>` / `<module-init>` to `is_async = true` when this
/// returns true so `Op::Await` can park the entry frame, matching
/// §16.2.1.7 `top-level-await` modules.
/// Walk a non-arrow function body and report whether it references
/// the `arguments` identifier in a binding that escapes the body's
/// own arrow / nested-function scopes. Arrow functions inherit
/// `arguments` lexically per §10.2.1.4 so a reference inside an
/// arrow within the body still implies the enclosing function
/// must materialise the object.
pub(crate) fn body_references_arguments(
    params: &oxc_ast::ast::FormalParameters<'_>,
    body: Option<&oxc_ast::ast::FunctionBody<'_>>,
) -> bool {
    use oxc_ast_visit::Visit;
    #[derive(Default)]
    struct ArgsFinder {
        nested_function_depth: u32,
        found: bool,
    }
    impl<'a> Visit<'a> for ArgsFinder {
        fn visit_function(
            &mut self,
            it: &oxc_ast::ast::Function<'a>,
            flags: oxc_syntax::scope::ScopeFlags,
        ) {
            // Nested non-arrow function — has its own `arguments`.
            self.nested_function_depth += 1;
            oxc_ast_visit::walk::walk_function(self, it, flags);
            self.nested_function_depth -= 1;
        }
        fn visit_class_body(&mut self, it: &oxc_ast::ast::ClassBody<'a>) {
            // Class methods are functions with their own arguments.
            self.nested_function_depth += 1;
            oxc_ast_visit::walk::walk_class_body(self, it);
            self.nested_function_depth -= 1;
        }
        fn visit_identifier_reference(&mut self, id: &oxc_ast::ast::IdentifierReference<'a>) {
            if self.nested_function_depth == 0 && id.name.as_str() == "arguments" {
                self.found = true;
            }
        }
    }
    let mut finder = ArgsFinder::default();
    // Param defaults can reference `arguments` (sloppy mode); even
    // in strict mode, `function f(x = arguments) {}` is valid.
    for p in &params.items {
        if let Some(init) = p.initializer.as_deref() {
            finder.visit_expression(init);
        }
    }
    if let Some(rest) = &params.rest {
        finder.visit_binding_rest_element(&rest.rest);
    }
    if let Some(b) = body {
        for stmt in &b.statements {
            finder.visit_statement(stmt);
        }
    }
    finder.found
}

pub(crate) fn module_body_uses_top_level_await(stmts: &[Statement<'_>]) -> bool {
    use oxc_ast_visit::Visit;
    #[derive(Default)]
    struct AwaitFinder {
        depth: u32,
        found: bool,
    }
    impl<'a> Visit<'a> for AwaitFinder {
        fn visit_function(
            &mut self,
            it: &oxc_ast::ast::Function<'a>,
            flags: oxc_syntax::scope::ScopeFlags,
        ) {
            self.depth += 1;
            oxc_ast_visit::walk::walk_function(self, it, flags);
            self.depth -= 1;
        }
        fn visit_arrow_function_expression(
            &mut self,
            it: &oxc_ast::ast::ArrowFunctionExpression<'a>,
        ) {
            self.depth += 1;
            oxc_ast_visit::walk::walk_arrow_function_expression(self, it);
            self.depth -= 1;
        }
        fn visit_class(&mut self, it: &oxc_ast::ast::Class<'a>) {
            // Class heritage and computed property names are evaluated in the
            // surrounding scope, so `class C extends f(await x) {}` is a
            // top-level await when the class is a module item. Class element
            // bodies and field/static initializers run in class-created
            // function-like contexts, so await there is not module TLA.
            if let Some(super_class) = &it.super_class {
                self.visit_expression(super_class);
            }
            for element in &it.body.body {
                match element {
                    oxc_ast::ast::ClassElement::MethodDefinition(method) if method.computed => {
                        self.visit_property_key(&method.key);
                    }
                    oxc_ast::ast::ClassElement::PropertyDefinition(prop) if prop.computed => {
                        self.visit_property_key(&prop.key);
                    }
                    oxc_ast::ast::ClassElement::AccessorProperty(accessor) if accessor.computed => {
                        self.visit_property_key(&accessor.key);
                    }
                    _ => {}
                }
            }
        }
        fn visit_await_expression(&mut self, it: &oxc_ast::ast::AwaitExpression<'a>) {
            if self.depth == 0 {
                self.found = true;
            }
            oxc_ast_visit::walk::walk_await_expression(self, it);
        }
    }
    let mut finder = AwaitFinder::default();
    for stmt in stmts {
        finder.visit_statement(stmt);
    }
    finder.found
}

/// §B.3.3.1/2/3 — pre-declare the sloppy-mode var-scope extension for
/// block-level function declarations. Candidates get an initialized
/// (`undefined`) binding in the current variable scope at
/// instantiation time; the declaration's source position later syncs
/// the block binding's value into it. `extra_blocked` carries
/// caller-scope blockers (parameter names, `"arguments"`).
pub(crate) fn pre_declare_annex_b_functions(
    cx: &mut Compiler,
    stmts: &[Statement<'_>],
    extra_blocked: &HashSet<String>,
    span: (u32, u32),
) -> Result<(), CompileError> {
    if cx.is_strict {
        return Ok(());
    }
    let mut blocked = extra_blocked.clone();
    let mut top_lex: Vec<(String, bool)> = Vec::new();
    hoist_lexical_names(stmts, &mut top_lex);
    blocked.extend(top_lex.into_iter().map(|(name, _)| name));
    let candidates = crate::annex_b::collect_annex_b_candidates(stmts, &blocked);
    if candidates.is_empty() {
        return Ok(());
    }
    let global_mirror =
        cx.stack.len() == 1 && cx.module_state.is_none() && !cx.suppress_global_mirror;
    for name in candidates {
        match cx.lookup_binding(&name) {
            Some(info) => {
                cx.annex_b_var_storages
                    .insert(name, (Some(info.storage), global_mirror));
            }
            None if global_mirror => {
                // Script / sloppy-eval bodies: the var extension IS
                // the global own property — a local shadow would mask
                // a pre-existing global's value inside this body.
                // §B.3.3.2 script bindings are non-configurable;
                // §B.3.3.3 eval bindings are deletable.
                let configurable = i32::from(cx.in_eval);
                let name_idx = cx.intern_string_constant(&name);
                cx.emit(
                    Op::DeclareGlobalVar,
                    [Operand::ConstIndex(name_idx), Operand::Imm32(configurable)],
                    span,
                );
                cx.annex_b_var_storages.insert(name, (None, true));
            }
            None => {
                let storage = cx.declare_binding(&name, false, span)?;
                let dst = cx.alloc_scratch();
                cx.emit(Op::LoadUndefined, [Operand::Register(dst)], span);
                cx.emit_store_storage(dst, storage, span);
                cx.mark_initialized(&name);
                cx.annex_b_var_storages.insert(name, (Some(storage), false));
            }
        }
    }
    Ok(())
}
