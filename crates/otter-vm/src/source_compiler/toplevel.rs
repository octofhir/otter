//! Top-level compilation orchestration: program walking, module
//! imports / exports, synthesised top-level entry and module-init
//! functions, and the per-declaration lowering.
//!
//! Extracted from `source_compiler/mod.rs` in the file-size cleanup
//! follow-up after C4. `ModuleCompiler::compile_with_completion` in
//! the parent module routes every compile through
//! `lower_program`; each top-level `FunctionDeclaration` flows
//! through `record_function_declaration` + either
//! `lower_function_declaration` or
//! `lower_function_declaration_with_globals`.

use super::*;

// ---------------------------------------------------------------------------
// Lowering
// ---------------------------------------------------------------------------

pub(super) fn lower_program(
    program: &Program<'_>,
    completion: TopLevelCompletion,
) -> Result<Module, SourceLoweringError> {
    // The program is one or more top-level `FunctionDeclaration`s,
    // optionally mixed with `import` / `export` declarations (M35).
    // Anything else — `class`, `var`, top-level expressions or
    // statements — surfaces as an `Unsupported` pointing at the
    // offending node so later milestones can widen coverage one
    // construct at a time. The conventional `main` pattern
    // (helpers first, entry last) makes the **last** function the
    // module's entry for script-style programs.
    //
    // An empty script body is a valid program (§16.1 Scripts / §16.2
    // Modules: the Script / ModuleBody production is optional). Fall
    // through — the rest of the classifier produces a single
    // synthesised top-level that just returns `undefined`, matching
    // what a browser would do for an empty .js / .mjs file.

    // M35 state: collected import/export records, plus the name of
    // every binding that the runtime installs on the global object
    // before / during module evaluation. Inner function bodies
    // resolve bare identifier references against `module_globals`
    // (via `ctx.is_module_global`) so an imported symbol or a
    // top-level export can be read/called from a nested function.
    let mut imports: Vec<ImportRecord> = Vec::new();
    let mut exports: Vec<ExportRecord> = Vec::new();
    let mut module_globals: Vec<String> = Vec::new();
    // §15.1.11 Script Records / §10.2.11 FunctionDeclarationInstantiation —
    // `var` bindings are hoisted to the script scope and pre-initialized
    // to `undefined` BEFORE the script body executes. We collect every
    // var name (top-level + nested in blocks / if / switch / try / etc.)
    // and emit `LdaUndefined; StaGlobal name` in the preamble.
    let mut top_level_vars: Vec<String> = Vec::new();
    // Per-source-URL flag: this program uses ES-module syntax
    // (static `import` / `export` / dynamic `import()`). An empty
    // set of records with no `import()` expressions means the
    // program is still a plain script and lands on `Module::new`
    // (no synthesised module-init, no `new_esm`).
    let mut is_esm = false;

    // First pass: classify top-level statements into function
    // declarations (with or without an `export` wrapper), pure
    // import/export metadata, and everything else — the latter
    // makes up the "script body" that runs top-to-bottom when the
    // module is evaluated. The script-body path is the idiomatic
    // JS shape (`console.log("hi")` at file top, `const x = …`
    // followed by `fetch(...)`, etc.); no `function main() {}`
    // wrapper required.
    let mut declarations: Vec<&Function<'_>> = Vec::with_capacity(program.body.len());
    let mut names: Vec<&str> = Vec::with_capacity(program.body.len());
    let mut script_body: Vec<&Statement<'_>> = Vec::new();
    // Binding names introduced at the top level via
    // `export const` / `export let` / `export class`. The synth
    // top-level body runs their initialisers as ordinary locals;
    // the flush-to-globals loop after the body copies each local
    // onto the global object so `capture_exports` finds the value.
    let mut exported_const_vars: Vec<String> = Vec::new();
    let mut default_export_local: Option<String> = None;
    for stmt in &program.body {
        match stmt {
            Statement::FunctionDeclaration(func) => {
                let name = record_function_declaration(func, &mut declarations, &mut names)?;
                // Top-level function declarations are visible to
                // every top-level statement in the same module —
                // mirror them onto the global object so
                // `LdaGlobal <name>` resolves. The synth
                // top-level's CreateClosure preamble takes care
                // of the actual installation.
                if !module_globals.iter().any(|n| n == name) {
                    module_globals.push(name.to_string());
                }
            }
            Statement::ImportDeclaration(decl) => {
                is_esm = true;
                let specifier: Box<str> = decl.source.value.as_str().into();
                let mut bindings: Vec<ImportBinding> = Vec::new();
                if let Some(specs) = decl.specifiers.as_ref() {
                    for spec in specs {
                        match spec {
                            oxc_ast::ast::ImportDeclarationSpecifier::ImportSpecifier(s) => {
                                let imported = module_export_name_to_string(&s.imported)
                                    .ok_or_else(|| {
                                        SourceLoweringError::unsupported(
                                            "import_specifier_string_literal",
                                            s.span,
                                        )
                                    })?;
                                let local = s.local.name.as_str().to_string();
                                module_globals.push(local.clone());
                                bindings.push(ImportBinding::Named {
                                    imported: imported.into(),
                                    local: local.into(),
                                });
                            }
                            oxc_ast::ast::ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                                let local = s.local.name.as_str().to_string();
                                module_globals.push(local.clone());
                                bindings.push(ImportBinding::Default {
                                    local: local.into(),
                                });
                            }
                            oxc_ast::ast::ImportDeclarationSpecifier::ImportNamespaceSpecifier(
                                s,
                            ) => {
                                let local = s.local.name.as_str().to_string();
                                module_globals.push(local.clone());
                                bindings.push(ImportBinding::Namespace {
                                    local: local.into(),
                                });
                            }
                        }
                    }
                }
                imports.push(ImportRecord {
                    specifier,
                    bindings,
                });
            }
            Statement::ExportNamedDeclaration(decl) => {
                is_esm = true;
                if let Some(inner) = &decl.declaration {
                    match inner {
                        Declaration::FunctionDeclaration(func) => {
                            let name = record_function_declaration(
                                func.as_ref(),
                                &mut declarations,
                                &mut names,
                            )?;
                            module_globals.push(name.to_string());
                            exports.push(ExportRecord::Named {
                                local: name.to_string().into(),
                                exported: name.to_string().into(),
                            });
                        }
                        Declaration::VariableDeclaration(var_decl) => {
                            // `export const X = expr` / `export let Y = expr`.
                            // Inject the inner VariableDeclaration into
                            // the script body so the RHS evaluates at
                            // module-eval time, then record each
                            // declarator's name so the synth top-level
                            // flushes its local to a same-named global
                            // before it returns.
                            //
                            // §16.2.3.7 Destructuring in an exported
                            // variable declaration (`export const { a,
                            // b } = obj`, `export const [x, y] =
                            // pair`) binds each leaf as its own export
                            // under its own name. Walk the pattern
                            // and collect every identifier leaf so
                            // each leaf local flushes to a same-named
                            // global.
                            for declarator in var_decl.declarations.iter() {
                                let mut leaf_names: Vec<String> = Vec::new();
                                collect_pattern_identifier_names(&declarator.id, &mut leaf_names)?;
                                for name in leaf_names {
                                    module_globals.push(name.clone());
                                    exports.push(ExportRecord::Named {
                                        local: name.clone().into(),
                                        exported: name.clone().into(),
                                    });
                                    exported_const_vars.push(name);
                                }
                            }
                            script_body.push(stmt);
                        }
                        Declaration::ClassDeclaration(_) => {
                            // `export class C {}` — route the class
                            // through the script body; top-level class
                            // declarations already lower to a local
                            // under the script-body path.
                            let name = match inner {
                                Declaration::ClassDeclaration(cls) => cls
                                    .id
                                    .as_ref()
                                    .map(|id| id.name.as_str().to_string())
                                    .ok_or_else(|| {
                                        SourceLoweringError::unsupported(
                                            "anonymous_class",
                                            inner.span(),
                                        )
                                    })?,
                                _ => unreachable!(),
                            };
                            module_globals.push(name.clone());
                            exports.push(ExportRecord::Named {
                                local: name.clone().into(),
                                exported: name.clone().into(),
                            });
                            exported_const_vars.push(name);
                            script_body.push(stmt);
                        }
                        _ => {
                            return Err(SourceLoweringError::unsupported(
                                "export_declaration_non_function",
                                inner.span(),
                            ));
                        }
                    }
                } else if let Some(source) = &decl.source {
                    // `export { x } from "./m"` — re-export named.
                    let specifier = source.value.as_str().to_string();
                    for spec in &decl.specifiers {
                        let local = module_export_name_to_string(&spec.local).ok_or_else(|| {
                            SourceLoweringError::unsupported(
                                "export_specifier_string_literal",
                                spec.span,
                            )
                        })?;
                        let exported =
                            module_export_name_to_string(&spec.exported).ok_or_else(|| {
                                SourceLoweringError::unsupported(
                                    "export_specifier_string_literal",
                                    spec.span,
                                )
                            })?;
                        exports.push(ExportRecord::ReExportNamed {
                            specifier: specifier.clone().into(),
                            imported: local.into(),
                            exported: exported.into(),
                        });
                    }
                } else {
                    // `export { x, y }` — references to top-level
                    // bindings. We record them and rely on the
                    // module-init to install the local as a global
                    // before `capture_exports` runs.
                    for spec in &decl.specifiers {
                        let local = module_export_name_to_string(&spec.local).ok_or_else(|| {
                            SourceLoweringError::unsupported(
                                "export_specifier_string_literal",
                                spec.span,
                            )
                        })?;
                        let exported =
                            module_export_name_to_string(&spec.exported).ok_or_else(|| {
                                SourceLoweringError::unsupported(
                                    "export_specifier_string_literal",
                                    spec.span,
                                )
                            })?;
                        module_globals.push(local.clone());
                        exports.push(ExportRecord::Named {
                            local: local.into(),
                            exported: exported.into(),
                        });
                    }
                }
            }
            Statement::ExportDefaultDeclaration(decl) => {
                is_esm = true;
                // §16.2.3 `export default …` — accepted shapes:
                //
                //   `export default function foo() {}` — register as
                //   a named hoistable declaration.
                //
                //   `export default class Foo {}` — hoist onto the
                //   synthesised top-level script via the same path
                //   as a plain top-level class declaration; bind the
                //   default export to `Foo` on the global object.
                //
                //   Anonymous defaults (`export default class {}` /
                //   `export default function () {}` / `export
                //   default expr`) synthesise a fresh module-level
                //   binding and lower at module-init time.
                match &decl.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(func)
                        if func.id.is_some() =>
                    {
                        let name = record_function_declaration(
                            func.as_ref(),
                            &mut declarations,
                            &mut names,
                        )?;
                        default_export_local = Some(name.to_string());
                        module_globals.push(name.to_string());
                        exports.push(ExportRecord::Default {
                            local: name.to_string().into(),
                        });
                    }
                    ExportDefaultDeclarationKind::ClassDeclaration(class) if class.id.is_some() => {
                        let id = class.id.as_ref().expect("guard ensures named class");
                        let name = id.name.as_str().to_string();
                        // Route through the statement-lowering phase
                        // like any other top-level class. The outer
                        // `Statement::ExportDefaultDeclaration` itself
                        // drops into `script_body` unchanged; the
                        // script-body lowerer recognises the default
                        // wrapper and delegates to
                        // `lower_nested_class_declaration`.
                        module_globals.push(name.clone());
                        exported_const_vars.push(name.clone());
                        default_export_local = Some(name.clone());
                        exports.push(ExportRecord::Default { local: name.into() });
                        script_body.push(stmt);
                    }
                    other => {
                        // `export default <expr>` and anonymous
                        // `export default function () {}` / `export
                        // default class {}` all collapse to
                        // "evaluate the right-hand side at module
                        // init, bind to a synthetic module-level
                        // `default` local, and register that local
                        // as the default export." The binding is
                        // named `__otter_default` — reserved and
                        // not reachable by user identifier refs
                        // (starts with `__otter_` which the compiler
                        // treats as internal).
                        if !matches!(
                            other,
                            ExportDefaultDeclarationKind::ClassDeclaration(_)
                                | ExportDefaultDeclarationKind::FunctionDeclaration(_)
                        ) && !other.is_expression()
                        {
                            return Err(SourceLoweringError::unsupported(
                                "export_default_non_function",
                                decl.span,
                            ));
                        }
                        module_globals.push(MODULE_DEFAULT_EXPORT_LOCAL.to_string());
                        exported_const_vars.push(MODULE_DEFAULT_EXPORT_LOCAL.to_string());
                        default_export_local = Some(MODULE_DEFAULT_EXPORT_LOCAL.to_string());
                        exports.push(ExportRecord::Default {
                            local: MODULE_DEFAULT_EXPORT_LOCAL.into(),
                        });
                        script_body.push(stmt);
                    }
                }
            }
            Statement::ExportAllDeclaration(decl) => {
                is_esm = true;
                let specifier: Box<str> = decl.source.value.as_str().into();
                if let Some(exported) = &decl.exported {
                    let exported = module_export_name_to_string(exported).ok_or_else(|| {
                        SourceLoweringError::unsupported(
                            "export_specifier_string_literal",
                            decl.span,
                        )
                    })?;
                    exports.push(ExportRecord::ReExportNamespace {
                        specifier,
                        exported: exported.into(),
                    });
                } else {
                    exports.push(ExportRecord::ReExportAll { specifier });
                }
            }
            Statement::ClassDeclaration(class) => {
                // Top-level class declarations are visible to every
                // other top-level function in the module — add the
                // name to `module_globals` + `exported_const_vars`
                // so the synth top-level flushes the local to a
                // global of the same name. Inner methods of a
                // top-level function can then refer to the class
                // via `LdaGlobal`.
                if let Some(id) = &class.id {
                    let name = id.name.as_str().to_string();
                    module_globals.push(name.clone());
                    exported_const_vars.push(name);
                }
                script_body.push(stmt);
            }
            Statement::VariableDeclaration(decl) => {
                // §14.2 top-level `let` / `const` / `var` bindings
                // in a module are lexically scoped to the module —
                // but they're visible to every top-level function
                // in the same file (closures over module scope).
                // We don't have a closure from top-level functions
                // into the synth body, so mirror those names onto
                // the global object instead. `function circleArea
                // () { return PI * r * r }` after `const PI = …`
                // resolves `PI` via `LdaGlobal` at call time.
                let is_var =
                    matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Var);
                for declarator in decl.declarations.iter() {
                    if let oxc_ast::ast::BindingPattern::BindingIdentifier(bi) = &declarator.id {
                        let name = bi.name.as_str().to_string();
                        module_globals.push(name.clone());
                        if is_var {
                            // §13.3.2.1 — `var` is hoisted to script scope
                            // with initial value undefined. The actual
                            // assignment (if any) still runs at the
                            // declaration site.
                            top_level_vars.push(name.clone());
                        }
                        exported_const_vars.push(name);
                    }
                }
                script_body.push(stmt);
            }
            other => {
                // Top-level script statement — `console.log(...)`,
                // `if (...) { ... }`, etc. Collect into
                // `script_body` and synthesise a top-level entry
                // function that runs them on module evaluation.
                // Recursively walk for nested `var` declarations
                // (`if (cond) { var x; }` still hoists to script).
                collect_nested_var_names(other, &mut top_level_vars, &mut module_globals);
                script_body.push(other);
            }
        }
    }

    let _ = default_export_local;

    // A completely empty program (whitespace + comments only) is a
    // valid Script / Module. Fall through to the synth top-level
    // path so the resulting Module evaluates to `undefined` with no
    // observable side effects — matches both browser and Node
    // behaviour and unblocks test262 harness files like
    // `harness/compareArray.js` that carry only a doc comment.

    // Second pass: lower each function with the shared name table
    // available so `f(args)` inside one body can resolve `f` to its
    // `FunctionIndex`. Top-level functions land at indices
    // `0..declarations.len()`; any inner `FunctionExpression`
    // encountered during body lowering appends beyond that.
    let module_functions: std::rc::Rc<std::cell::RefCell<Vec<VmFunction>>> = std::rc::Rc::new(
        std::cell::RefCell::new(Vec::with_capacity(declarations.len())),
    );
    // M25: top-level declaration indices need to be stable before
    // any body lowering runs (so nested `f()` inside one body can
    // resolve to the shared `function_names` table). We push
    // placeholder `VmFunction::empty` entries and then overwrite
    // each slot with the real lowered function. Inner functions
    // (landing after the top-level slots) use `Vec::push` to grow
    // the shared list.
    for _ in 0..declarations.len() {
        module_functions.borrow_mut().push(placeholder_function());
    }

    // M35: publish `module_globals` via a shared top-level
    // `LoweringContext` before any body is lowered. Every child
    // context inherits the populated list via the `Rc`, so nested
    // function bodies that reference an imported symbol resolve
    // it through `LdaGlobal` without knowing about module
    // machinery. The context lives only long enough to seed the
    // list — each `lower_function_declaration` creates its own
    // context internally (with no parent), so it picks up the
    // names by constructing a fresh `Rc`. To share, we clone the
    // Rc into every call.
    let module_globals_rc: std::rc::Rc<std::cell::RefCell<Vec<String>>> =
        std::rc::Rc::new(std::cell::RefCell::new(module_globals.clone()));

    for (top_idx, func) in declarations.iter().enumerate() {
        let lowered = lower_function_declaration_with_globals(
            func,
            &names,
            std::rc::Rc::clone(&module_functions),
            std::rc::Rc::clone(&module_globals_rc),
        )?;
        module_functions.borrow_mut()[top_idx] = lowered;
    }

    // Entry: always the synthesised top-level function. The ES
    // spec has no notion of a "main" function — a module / script
    // is just the statements at the top level, evaluated once when
    // the module loads. Top-level function declarations stay
    // callable via their `FunctionIndex` / `CallDirect`, but
    // nothing auto-invokes them; explicit calls (top-level or
    // inside another function) are the only entry points, matching
    // real JS semantics.
    //
    // For ES modules the synth's preamble installs each exported
    // top-level binding on the global object so `capture_exports`
    // in the module loader sees the values.
    //
    // Classic scripts that declare only functions (no imperative
    // statements) still get a top-level entry — its body is just
    // the trailing `LdaUndefined; Return` pair, which runs once
    // and exits with no observable side effect.
    let top_idx = synthesise_top_level_entry(
        &module_functions,
        &names,
        &module_globals,
        &top_level_vars,
        &script_body,
        &exported_const_vars,
        completion,
    )?;
    let entry_idx = u32::try_from(top_idx)
        .map_err(|_| SourceLoweringError::Internal("top-level entry index overflow".into()))?;

    let functions = std::rc::Rc::try_unwrap(module_functions)
        .map_err(|_| SourceLoweringError::Internal("module functions still shared".into()))?
        .into_inner();
    let module = if is_esm {
        Module::new_esm(
            None::<&str>,
            functions,
            FunctionIndex(entry_idx),
            imports,
            exports,
        )
        .map_err(|err| {
            SourceLoweringError::Internal(format!("module construction failed: {err}"))
        })?
    } else {
        Module::new(None::<&str>, functions, FunctionIndex(entry_idx)).map_err(|err| {
            SourceLoweringError::Internal(format!("module construction failed: {err}"))
        })?
    };
    Ok(module)
}

/// Records a single `FunctionDeclaration` into the module's
/// top-level declaration tables. Rejects anonymous or duplicate
/// names with a stable tag so callers don't repeat the check.
fn record_function_declaration<'a>(
    func: &'a Function<'a>,
    declarations: &mut Vec<&'a Function<'a>>,
    names: &mut Vec<&'a str>,
) -> Result<&'a str, SourceLoweringError> {
    let name = func
        .id
        .as_ref()
        .map(|ident| ident.name.as_str())
        .ok_or_else(|| SourceLoweringError::unsupported("anonymous_function", func.span))?;
    if names.contains(&name) {
        return Err(SourceLoweringError::unsupported(
            "duplicate_function_declaration",
            func.span,
        ));
    }
    names.push(name);
    declarations.push(func);
    Ok(name)
}

/// §13.3.2.1 var-hoist collector for the script-body classifier.
///
/// Walks an arbitrary top-level `Statement` and pushes every nested
/// `var` binding name onto `top_level_vars` + `module_globals`. The
/// preamble in `synthesise_top_level_entry` then pre-initialises each
/// to `undefined` so a use that precedes the textual declaration
/// (`console.log(x); var x;`) reads `undefined` instead of throwing
/// ReferenceError.
///
/// Spec: VarScopedDeclarations / VarDeclaredNames
/// <https://tc39.es/ecma262/#sec-static-semantics-vardeclarednames>
fn collect_nested_var_names<'a>(
    stmt: &'a Statement<'a>,
    top_level_vars: &mut Vec<String>,
    module_globals: &mut Vec<String>,
) {
    use oxc_ast::ast::Statement as S;
    match stmt {
        S::BlockStatement(block) => {
            for s in &block.body {
                collect_nested_var_names(s, top_level_vars, module_globals);
            }
        }
        S::IfStatement(stmt) => {
            collect_nested_var_names(&stmt.consequent, top_level_vars, module_globals);
            if let Some(alt) = stmt.alternate.as_ref() {
                collect_nested_var_names(alt, top_level_vars, module_globals);
            }
        }
        S::ForStatement(stmt) => {
            if let Some(init) = stmt.init.as_ref()
                && let oxc_ast::ast::ForStatementInit::VariableDeclaration(decl) = init
                && matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Var)
            {
                push_var_names(decl, top_level_vars, module_globals);
            }
            collect_nested_var_names(&stmt.body, top_level_vars, module_globals);
        }
        S::ForInStatement(stmt) => {
            if let oxc_ast::ast::ForStatementLeft::VariableDeclaration(decl) = &stmt.left
                && matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Var)
            {
                push_var_names(decl, top_level_vars, module_globals);
            }
            collect_nested_var_names(&stmt.body, top_level_vars, module_globals);
        }
        S::ForOfStatement(stmt) => {
            if let oxc_ast::ast::ForStatementLeft::VariableDeclaration(decl) = &stmt.left
                && matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Var)
            {
                push_var_names(decl, top_level_vars, module_globals);
            }
            collect_nested_var_names(&stmt.body, top_level_vars, module_globals);
        }
        S::WhileStatement(stmt) => {
            collect_nested_var_names(&stmt.body, top_level_vars, module_globals);
        }
        S::DoWhileStatement(stmt) => {
            collect_nested_var_names(&stmt.body, top_level_vars, module_globals);
        }
        S::SwitchStatement(stmt) => {
            for case in &stmt.cases {
                for s in &case.consequent {
                    collect_nested_var_names(s, top_level_vars, module_globals);
                }
            }
        }
        S::TryStatement(stmt) => {
            for s in &stmt.block.body {
                collect_nested_var_names(s, top_level_vars, module_globals);
            }
            if let Some(handler) = stmt.handler.as_deref() {
                for s in &handler.body.body {
                    collect_nested_var_names(s, top_level_vars, module_globals);
                }
            }
            if let Some(finalizer) = stmt.finalizer.as_deref() {
                for s in &finalizer.body {
                    collect_nested_var_names(s, top_level_vars, module_globals);
                }
            }
        }
        S::WithStatement(stmt) => {
            collect_nested_var_names(&stmt.body, top_level_vars, module_globals);
        }
        S::LabeledStatement(stmt) => {
            collect_nested_var_names(&stmt.body, top_level_vars, module_globals);
        }
        S::VariableDeclaration(decl)
            if matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Var) =>
        {
            push_var_names(decl, top_level_vars, module_globals);
        }
        // Expression statements / break / continue / return / throw /
        // FunctionDeclaration (already handled by the classifier) /
        // class / let / const / import / export — no var content to
        // hoist.
        _ => {}
    }
}

fn push_var_names<'a>(
    decl: &'a oxc_ast::ast::VariableDeclaration<'a>,
    top_level_vars: &mut Vec<String>,
    module_globals: &mut Vec<String>,
) {
    for declarator in decl.declarations.iter() {
        let mut names: Vec<String> = Vec::new();
        let _ = collect_pattern_identifier_names(&declarator.id, &mut names);
        for name in names {
            if !top_level_vars.contains(&name) {
                top_level_vars.push(name.clone());
            }
            if !module_globals.contains(&name) {
                module_globals.push(name);
            }
        }
    }
}

/// Recursively walks a `BindingPattern` and pushes every
/// `BindingIdentifier` leaf's name onto `out`. Used by
/// `export const { a, b } = obj` / `export const [x, y] = pair`
/// to collect every export-generating leaf name. Rest elements
/// (`export const [...rest] = arr`, `export const { ...rest } =
/// obj`) also bind a name and are included. Default initializers
/// on a leaf (`export const { a = 1 } = obj`) peel back to the
/// BindingIdentifier via the AssignmentPattern wrapper.
fn collect_pattern_identifier_names<'a>(
    pattern: &'a oxc_ast::ast::BindingPattern<'a>,
    out: &mut Vec<String>,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::BindingPattern;
    match pattern {
        BindingPattern::BindingIdentifier(ident) => {
            out.push(ident.name.as_str().to_string());
            Ok(())
        }
        BindingPattern::ArrayPattern(pat) => {
            for element in pat.elements.iter().flatten() {
                collect_pattern_identifier_names(element, out)?;
            }
            if let Some(rest) = pat.rest.as_deref() {
                collect_pattern_identifier_names(&rest.argument, out)?;
            }
            Ok(())
        }
        BindingPattern::ObjectPattern(pat) => {
            for prop in &pat.properties {
                collect_pattern_identifier_names(&prop.value, out)?;
            }
            if let Some(rest) = pat.rest.as_deref() {
                collect_pattern_identifier_names(&rest.argument, out)?;
            }
            Ok(())
        }
        BindingPattern::AssignmentPattern(pat) => {
            // `{ a = 1 }` / `[a = 1]` — the left side is the
            // actual binding; the right is the default.
            collect_pattern_identifier_names(&pat.left, out)
        }
    }
}

/// §16.2.1.4 — converts a `ModuleExportName` AST node (which may be
/// an identifier or a string literal) into the bare string form
/// the runtime records use. Returns `None` for string-literal
/// names because the current module surface doesn't carry those
/// through the runtime registry yet (`"foo \0 bar"` export names
/// need additional care around UTF-16 and property-key interning).
fn module_export_name_to_string(name: &ModuleExportName<'_>) -> Option<String> {
    match name {
        ModuleExportName::IdentifierName(i) => Some(i.name.as_str().to_string()),
        ModuleExportName::IdentifierReference(i) => Some(i.name.as_str().to_string()),
        ModuleExportName::StringLiteral(_) => None,
    }
}

pub(super) const MODULE_DEFAULT_EXPORT_LOCAL: &str = "__otter_default";

/// Appends a synthetic "module-init" [`VmFunction`] to
/// `module_functions`. Its body materialises each top-level
/// declaration whose name is a module global as a closure on the
/// global object so the module loader's `capture_exports` can
/// read the value back out under that name. Returns the appended
/// index.
/// Builds the module's top-level entry function from the
/// collected script-body statements. Runs them top-to-bottom
/// with full local / temp / closure support — the same body
/// lowering every regular function uses. For ES modules, the
/// preamble also installs each exported top-level binding on
/// the global object so `capture_exports` in the module loader
/// sees the values by the time it harvests the namespace
/// (same contract as `synthesise_module_init_function`,
/// delivered inline here instead of in a separate function).
fn synthesise_top_level_entry<'a>(
    module_functions: &std::rc::Rc<std::cell::RefCell<Vec<VmFunction>>>,
    names: &[&str],
    module_globals: &[String],
    top_level_vars: &[String],
    script_body: &[&'a Statement<'a>],
    exported_const_vars: &[String],
    completion: TopLevelCompletion,
) -> Result<usize, SourceLoweringError> {
    // Empty params — the top-level entry takes no arguments.
    // `names` carries the top-level function-declaration names so
    // `f()` inside the script body can still resolve to its
    // `FunctionIndex` and emit `CallDirect`.
    let params_layout = ParamsLayout {
        names: Vec::new(),
        defaults: Vec::new(),
        patterns: Vec::new(),
        rest_name: None,
        rest_pattern: None,
    };
    let mut builder = BytecodeBuilder::new();
    // Publish `module_globals` via the thread-local override so
    // the newly-built `LoweringContext` picks up the full list —
    // the preamble and script body both need to know which
    // top-level names the module considers module-global, so
    // `lower_identifier_reference` routes bare references via
    // `LdaGlobal` (same channel the user-declared top-level
    // functions already use).
    let globals_rc: std::rc::Rc<std::cell::RefCell<Vec<String>>> =
        std::rc::Rc::new(std::cell::RefCell::new(module_globals.to_vec()));
    MODULE_GLOBALS_OVERRIDE.with(|slot| {
        *slot.borrow_mut() = Some(std::rc::Rc::clone(&globals_rc));
    });
    let mut ctx = LoweringContext::new(&params_layout, names, std::rc::Rc::clone(module_functions));
    MODULE_GLOBALS_OVERRIDE.with(|slot| {
        *slot.borrow_mut() = None;
    });
    // This is the synthesised top-level script body. Each top-level
    // `var`/`let`/`const NAME = init;` that the program classifier
    // registered as a module-global must be mirrored onto
    // `globalThis.NAME` at its binding site so a nested function
    // called mid-body reads the value via `LdaGlobal`.
    ctx.enable_top_level_global_mirroring();

    // §15.1.11 var hoisting — pre-declare each top-level `var` name on
    // the global object as `undefined` BEFORE any user statement runs.
    // Without this, `console.log(x); var x;` raises ReferenceError.
    // Idempotent: if a function declaration with the same name lands
    // later, its closure overrides the undefined here.
    let mut declared_var_global: std::collections::HashSet<&str> =
        std::collections::HashSet::new();
    for name in top_level_vars {
        if !declared_var_global.insert(name.as_str()) {
            continue;
        }
        builder
            .emit(Opcode::LdaUndefined, &[])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "top-level var hoist LdaUndefined: {err:?}"
                ))
            })?;
        let prop_idx = ctx.intern_property_name(name)?;
        builder
            .emit(Opcode::StaGlobal, &[Operand::Idx(prop_idx)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "top-level var hoist StaGlobal: {err:?}"
                ))
            })?;
    }

    // Preamble: install each module-global binding on the global
    // object so ESM `capture_exports` finds them. For classic
    // scripts `module_globals` is empty and this loop is a
    // no-op — top-level `let` / `const` still uses the regular
    // local-allocation path in the body lowering.
    let mut pending_templates: Vec<(u32, crate::closure::ClosureTemplate)> = Vec::new();
    for name in module_globals {
        let Some(top_idx) = names.iter().position(|n| *n == name.as_str()) else {
            continue;
        };
        let func_idx = u32::try_from(top_idx).map_err(|_| {
            SourceLoweringError::Internal("top-level function index overflow".into())
        })?;
        let pc = builder
            .emit(
                Opcode::CreateClosure,
                &[Operand::Idx(func_idx), Operand::Imm(0)],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("top-level encode CreateClosure: {err:?}"))
            })?;
        pending_templates.push((
            pc,
            crate::closure::ClosureTemplate::new(FunctionIndex(func_idx), Vec::new()),
        ));
        let prop_idx = ctx.intern_property_name(name)?;
        builder
            .emit(Opcode::StaGlobal, &[Operand::Idx(prop_idx)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("top-level encode StaGlobal: {err:?}"))
            })?;
    }

    // Main body: lower each collected top-level statement through
    // the same path function bodies use.
    lower_top_level_statement_list(&mut builder, &mut ctx, script_body)?;
    // Post-body flush: `export const X = expr` allocated a local
    // for `X` during script-body lowering. Copy each local onto
    // the global object so the module-loader's `capture_exports`
    // sees the value when it walks the module namespace.
    for name in exported_const_vars {
        let Some(binding) = ctx.resolve_identifier(name) else {
            continue;
        };
        let reg = match binding {
            BindingRef::Local {
                reg,
                initialized: true,
                ..
            } => reg,
            BindingRef::Param { reg } => reg,
            _ => continue,
        };
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(reg))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "top-level encode Ldar (export flush): {err:?}"
                ))
            })?;
        let prop_idx = ctx.intern_property_name(name)?;
        builder
            .emit(Opcode::StaGlobal, &[Operand::Idx(prop_idx)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "top-level encode StaGlobal (export flush): {err:?}"
                ))
            })?;
    }
    let returns_trailing_expression = completion == TopLevelCompletion::LastExpressionStatement
        && matches!(script_body.last(), Some(Statement::ExpressionStatement(_)));
    if !returns_trailing_expression {
        builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("top-level encode LdaUndefined: {err:?}"))
        })?;
    }
    builder.emit(Opcode::Return, &[]).map_err(|err| {
        SourceLoweringError::Internal(format!("top-level encode Return: {err:?}"))
    })?;

    // Resolve pending exception handlers + bytecode length BEFORE
    // `builder.finish()` consumes the builder — `finish` drops
    // the label state that `take_exception_handlers` needs to
    // resolve try/catch PCs. A stale `BytecodeBuilder::new()`
    // would see every label as unbound and surface as
    // `exception handler try_start unbound`.
    let exception_handlers = ctx.take_exception_handlers(&builder)?;
    let bytecode_len_u32 = builder.pc();
    // Merge compiler-tracked closure templates (from nested
    // function expressions inside the script body) with our
    // prepended CreateClosure preamble entries.
    let mut closure_vec: Vec<Option<crate::closure::ClosureTemplate>> =
        vec![None; bytecode_len_u32 as usize];
    let compiler_templates = ctx.take_closure_table(bytecode_len_u32);
    for pc in 0..bytecode_len_u32 {
        if let Some(tpl) = compiler_templates.get(pc) {
            closure_vec[pc as usize] = Some(tpl);
        }
    }
    for (pc, tpl) in pending_templates {
        closure_vec[pc as usize] = Some(tpl);
    }
    let closure_table = crate::closure::ClosureTable::new(closure_vec);

    let bytecode = builder
        .finish()
        .map_err(|err| SourceLoweringError::Internal(format!("top-level finish: {err:?}")))?;
    let layout = FrameLayout::new(1, 0, ctx.local_count(), ctx.temp_count())
        .map_err(|err| SourceLoweringError::Internal(format!("top-level layout: {err:?}")))?;
    let feedback_layout = feedback_layout_from_kinds(&ctx.take_feedback_slot_kinds());
    let side_tables = crate::module::FunctionSideTables::new(
        ctx.take_property_names(),
        ctx.take_string_literals(),
        ctx.take_float_constants(),
        ctx.take_bigint_constants(),
        closure_table,
        Default::default(),
        ctx.take_regexp_literals(),
    );
    let tables = FunctionTables::new(
        side_tables,
        feedback_layout,
        Default::default(),
        crate::exception::ExceptionTable::new(exception_handlers),
        ctx.take_source_map(),
    );
    let vm_fn = VmFunction::new(Some("<top-level>"), layout, bytecode, tables);
    let mut fns = module_functions.borrow_mut();
    let idx = fns.len();
    fns.push(vm_fn);
    Ok(idx)
}

fn synthesise_module_init_function(
    module_functions: &std::rc::Rc<std::cell::RefCell<Vec<VmFunction>>>,
    names: &[&str],
    module_globals: &[String],
) -> Result<usize, SourceLoweringError> {
    // Hidden[0] is still the (unused) receiver slot, matching the
    // conventional frame layout for every other top-level
    // function. No params, no scratch. A single `u8` of property
    // names is plenty for the immediate subset.
    let layout = FrameLayout::new(1, 0, 0, 0)
        .map_err(|e| SourceLoweringError::Internal(format!("module-init layout: {e:?}")))?;
    let mut builder = BytecodeBuilder::new();
    let mut property_names: Vec<String> = Vec::new();
    // PC → ClosureTemplate map, built alongside bytecode emission.
    // The runtime looks up the template for each `CreateClosure`
    // opcode via `ClosureTable::get(pc)`; a missing entry trips
    // the "no ClosureTemplate for this PC" native-call error, so
    // every CreateClosure here must register one.
    let mut pending_templates: Vec<(u32, crate::closure::ClosureTemplate)> = Vec::new();
    for name in module_globals {
        let Some(top_idx) = names.iter().position(|n| *n == name.as_str()) else {
            continue;
        };
        let func_idx = u32::try_from(top_idx).map_err(|_| {
            SourceLoweringError::Internal("module-init function index overflow".into())
        })?;
        let pc = builder
            .emit(
                Opcode::CreateClosure,
                &[Operand::Idx(func_idx), Operand::Imm(0)],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("module-init encode CreateClosure: {err:?}"))
            })?;
        pending_templates.push((
            pc,
            crate::closure::ClosureTemplate::new(FunctionIndex(func_idx), Vec::new()),
        ));
        let prop_idx = property_names
            .iter()
            .position(|existing| existing == name)
            .unwrap_or_else(|| {
                property_names.push(name.clone());
                property_names.len() - 1
            });
        builder
            .emit(
                Opcode::StaGlobal,
                &[Operand::Idx(u32::try_from(prop_idx).unwrap_or(u32::MAX))],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("module-init encode StaGlobal: {err:?}"))
            })?;
    }
    builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
        SourceLoweringError::Internal(format!("module-init encode LdaUndefined: {err:?}"))
    })?;
    builder.emit(Opcode::Return, &[]).map_err(|err| {
        SourceLoweringError::Internal(format!("module-init encode Return: {err:?}"))
    })?;

    let bytecode = builder
        .finish()
        .map_err(|err| SourceLoweringError::Internal(format!("module-init finish: {err:?}")))?;
    let bytecode_len = bytecode.bytes().len();
    let mut templates: Vec<Option<crate::closure::ClosureTemplate>> = vec![None; bytecode_len];
    for (pc, template) in pending_templates {
        let idx = pc as usize;
        if idx < templates.len() {
            templates[idx] = Some(template);
        }
    }
    let closure_table = crate::closure::ClosureTable::new(templates);
    let side_tables = crate::module::FunctionSideTables::new(
        crate::property::PropertyNameTable::new(property_names),
        crate::string::StringTable::default(),
        crate::float::FloatTable::default(),
        crate::bigint::BigIntTable::default(),
        closure_table,
        crate::call::CallTable::default(),
        crate::regexp::RegExpTable::default(),
    );
    let tables = FunctionTables::new(
        side_tables,
        FeedbackTableLayout::default(),
        crate::deopt::DeoptTable::default(),
        crate::exception::ExceptionTable::default(),
        crate::source_map::SourceMap::default(),
    );
    let vm_fn = VmFunction::new(Some("<module-init>"), layout, bytecode, tables);
    let mut fns = module_functions.borrow_mut();
    let idx = fns.len();
    fns.push(vm_fn);
    Ok(idx)
}

/// Variant of [`lower_function_declaration`] that injects a shared
/// `module_globals` table into the lowering context so nested
/// function bodies resolve imported / exported names via
/// `LdaGlobal`. The plain [`lower_function_declaration`] keeps
/// its signature for backwards compatibility with internal
/// callers (nested-closure recursion).
fn lower_function_declaration_with_globals<'a>(
    func: &'a Function<'a>,
    function_names: &'a [&'a str],
    module_functions: std::rc::Rc<std::cell::RefCell<Vec<VmFunction>>>,
    module_globals: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
) -> Result<VmFunction, SourceLoweringError> {
    MODULE_GLOBALS_OVERRIDE.with(|slot| {
        *slot.borrow_mut() = Some(module_globals);
    });
    let result = lower_function_declaration(func, function_names, module_functions);
    MODULE_GLOBALS_OVERRIDE.with(|slot| {
        *slot.borrow_mut() = None;
    });
    result
}

std::thread_local! {
    /// Temporary channel carrying the module-globals table from
    /// [`lower_program`] down into the top-level
    /// [`LoweringContext::new`] call sites. `lower_function_declaration`
    /// constructs a `LoweringContext` with `parent = None` (no
    /// natural inheritance path), so a thread-local override is the
    /// least invasive way to seed the list without threading a
    /// module-state parameter through every compiler entry point.
    /// Cleared in `lower_function_declaration_with_globals` once the
    /// top-level body has been lowered — child contexts inherit the
    /// `Rc` via `with_parent`.
    pub(super) static MODULE_GLOBALS_OVERRIDE: std::cell::RefCell<
        Option<std::rc::Rc<std::cell::RefCell<Vec<String>>>>,
    > = const { std::cell::RefCell::new(None) };

    /// D2: Channel carrying the current module's
    /// `SourceTextIndex` from `ModuleCompiler::compile` into
    /// every nested `LoweringContext` so opcode emission can
    /// record `(pc → (line, column))` entries without plumbing
    /// the index through every helper.
    pub(super) static SOURCE_INDEX_OVERRIDE: std::cell::RefCell<
        Option<std::rc::Rc<crate::source_map::SourceTextIndex>>,
    > = const { std::cell::RefCell::new(None) };
}

/// Maps the residual `Statement` variants we explicitly don't handle at
/// M1 back to a stable `construct` tag. Later milestones can move a row
/// from this catch-all into a real lowering arm without touching call
/// sites in tests.
pub(super) fn statement_construct_tag(stmt: &Statement<'_>) -> &'static str {
    match stmt {
        Statement::VariableDeclaration(_) => "variable_declaration",
        Statement::ExpressionStatement(_) => "expression_statement",
        Statement::IfStatement(_) => "if_statement",
        Statement::WhileStatement(_) => "while_statement",
        Statement::DoWhileStatement(_) => "do_while_statement",
        Statement::ForStatement(_) => "for_statement",
        Statement::BlockStatement(_) => "block_statement",
        Statement::ReturnStatement(_) => "return_statement",
        Statement::ImportDeclaration(_) | Statement::ExportAllDeclaration(_) => {
            "module_declaration"
        }
        Statement::ExportDefaultDeclaration(_) | Statement::ExportNamedDeclaration(_) => {
            "export_declaration"
        }
        _ => "statement",
    }
}

/// Placeholder `Function` used to reserve top-level module slots
/// before bodies are lowered. Each slot is overwritten with the
/// real lowered function at the end of
/// `lower_program`; any nested `FunctionExpression` pushes beyond
/// the top-level prefix without shifting reserved indices.
fn placeholder_function() -> VmFunction {
    let layout = FrameLayout::new(0, 0, 0, 0).expect("empty frame layout");
    let empty_bytecode = BytecodeBuilder::new()
        .finish()
        .expect("empty BytecodeBuilder finishes");
    VmFunction::with_empty_tables(None::<&'static str>, layout, empty_bytecode)
}

fn lower_function_declaration<'a>(
    func: &'a Function<'a>,
    function_names: &'a [&'a str],
    module_functions: std::rc::Rc<std::cell::RefCell<Vec<VmFunction>>>,
) -> Result<VmFunction, SourceLoweringError> {
    let name = func
        .id
        .as_ref()
        .map(|ident| ident.name.as_str().to_owned())
        .ok_or_else(|| SourceLoweringError::unsupported("anonymous_function", func.span))?;

    let params_layout = analyze_params(&func.params)?;
    let param_count = params_layout.param_slot_count();

    let body = func
        .body
        .as_ref()
        .ok_or_else(|| SourceLoweringError::unsupported("declared_only_function", func.span))?;

    // Lower the body first so we know the final `let`/`const`,
    // call-temp, feedback-slot counts, and the interned
    // property-name / float-constant tables (M14). FrameLayout
    // needs the first two up front, and the feedback slot count
    // seeds the function's `FeedbackTableLayout` for the JIT's
    // int32-trust consumer (see
    // `analyze_template_candidate_with_feedback`).
    let body_out = lower_function_body(
        body,
        &func.params,
        &params_layout,
        function_names,
        module_functions,
    )?;

    // FrameLayout: 1 hidden slot for `this`, then `param_count`
    // parameter slots (non-rest params only; rest lands in a local),
    // then `local_count` `let`/`const` + rest-param slots, then
    // `temp_count` call-arg scratch slots. The v2 interpreter maps
    // `Ldar r0` through `FrameLayout::resolve_user_visible(0)`, which
    // points at the first parameter (absolute index 1), so parameter
    // / local / temp access stays symmetric with v1's register
    // semantics.
    let layout = FrameLayout::new(1, param_count, body_out.local_count, body_out.temp_count)
        .map_err(|err| SourceLoweringError::Internal(format!("frame layout invalid: {err:?}")))?;

    // M_JIT_C.2: every arithmetic op emitted above allocated a fresh
    // `Arithmetic`-kind slot via `allocate_arithmetic_feedback`. Build
    // the matching side-table layout so the interpreter and JIT can
    // resolve `bytecode.feedback().get(pc) -> FeedbackSlot` against a
    // well-shaped `FeedbackVector`.
    let feedback_layout = feedback_layout_from_kinds(&body_out.feedback_slot_kinds);
    // M14 / M15 / M25: wire the accumulated side tables so the
    // dispatcher can resolve `Idx` operands at runtime
    // (property names, string literals, float constants) and
    // materialise closures at CreateClosure PCs (closure
    // templates). Other tables (bigints, calls, regexps) stay
    // default-empty until later milestones exercise them.
    let side_tables = crate::module::FunctionSideTables::new(
        body_out.property_names,
        body_out.string_literals,
        body_out.float_constants,
        body_out.bigint_constants,
        body_out.closures,
        Default::default(),
        body_out.regexp_literals,
    );
    let tables = FunctionTables::new(
        side_tables,
        feedback_layout,
        Default::default(),
        body_out.exceptions,
        body_out.source_map,
    );

    Ok(
        VmFunction::new(Some(name), layout, body_out.bytecode, tables)
            .with_strict(func.id.is_some())
            .with_async(func.r#async)
            .with_generator(func.generator),
    )
}
