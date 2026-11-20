//! Module syntax detection and entry-path classification.
//!
//! # Contents
//! Module detection in a parsed program and entry-path classification.
//!
//! # Invariants
//! Module syntax is classified from the parsed program.
//!
//! # See also
//! - [`crate::Runtime`] owns the runtime session.

use crate::*;

/// AST-based "is this a module?" detection.
///
/// Parses the source through `otter-syntax` (the same OXC frontend
/// the compiler uses) and walks the program's AST looking for any
/// of:
/// - `ImportDeclaration` / `ExportNamedDeclaration` /
///   `ExportDefaultDeclaration` / `ExportAllDeclaration` (these
///   can only appear at the module top level — finding one is
///   conclusive proof);
/// - any `ImportExpression` (`import(specifier)` — only legal in
///   modules);
/// - any `MetaProperty` whose base is `import` (`import.meta` —
///   only legal in modules).
///
/// The walk uses [`oxc_ast_visit::Visit`] so we don't hand-roll a
/// match arm per AST node kind. The frontend policy forbids regex /
/// string parsing of JS/TS source.
///
/// Parse failures default to `false`: the caller routes to the
/// script path, which will re-parse and surface the same syntax
/// error through its diagnostic pipeline.
#[cfg(test)]
pub(super) fn source_text_looks_like_module(text: &str, kind: SourceKind) -> bool {
    with_program(text, kind, program_looks_like_module).unwrap_or(false)
}

pub(crate) fn program_looks_like_module(program: &oxc_ast::ast::Program<'_>) -> bool {
    use oxc_ast::ast::{Expression, Statement};
    use oxc_ast_visit::Visit;

    #[derive(Default)]
    struct ModuleSyntaxFinder {
        found: bool,
    }

    impl<'a> Visit<'a> for ModuleSyntaxFinder {
        fn visit_import_declaration(&mut self, _: &oxc_ast::ast::ImportDeclaration<'a>) {
            self.found = true;
        }
        fn visit_export_named_declaration(&mut self, _: &oxc_ast::ast::ExportNamedDeclaration<'a>) {
            self.found = true;
        }
        fn visit_export_declaration(&mut self, _: &oxc_ast::ast::ExportDeclaration<'a>) {
            self.found = true;
        }
        fn visit_export_from_declaration(&mut self, _: &oxc_ast::ast::ExportFromDeclaration<'a>) {
            self.found = true;
        }
        fn visit_export_default_declaration(
            &mut self,
            _: &oxc_ast::ast::ExportDefaultDeclaration<'a>,
        ) {
            self.found = true;
        }
        fn visit_export_all_declaration(&mut self, _: &oxc_ast::ast::ExportAllDeclaration<'a>) {
            self.found = true;
        }
        // Dynamic `import()` is deliberately NOT module syntax: it is legal
        // inside CommonJS, and Node's own detect-module classifier keys only
        // on static import/export and `import.meta`.
        fn visit_import_meta(&mut self, _: &oxc_ast::ast::ImportMeta) {
            self.found = true;
        }
    }

    let mut finder = ModuleSyntaxFinder::default();
    // Visit each statement; the visitor short-circuits via the
    // `found` flag (we still walk everything, but the boolean
    // result is correct).
    for stmt in &program.body {
        finder.visit_statement(stmt);
        if finder.found {
            return true;
        }
    }
    // Also check program-level expressions inside expression
    // statements — covered by visit_statement above.
    let _ = (Statement::EmptyStatement, Expression::NullLiteral);
    finder.found
}

pub(super) fn source_path_has_module_extension(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("mjs" | "mts")
    )
}

pub(super) fn source_path_has_script_extension(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("cjs" | "cts")
    )
}

pub(super) fn source_path_package_type(
    path: &Path,
    loader: &module_loader::ModuleLoader,
) -> Option<module_loader::LoaderPackageType> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("js" | "ts" | "jsx" | "tsx") => loader.package_type_for_path(path),
        _ => None,
    }
}
