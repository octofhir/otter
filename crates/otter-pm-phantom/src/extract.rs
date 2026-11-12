//! Specifier extraction from one source file.
//!
//! This advisory pass uses OXC's AST rather than text matching. It accepts
//! Script, CommonJS and Module syntax. It does not choose a loader goal from
//! package metadata or perform the compiler's semantic checks. Type-only imports
//! are omitted, and a load inside a `try` block is marked guarded rather than
//! required.
//!
//! # Contents
//! - [`Occurrence`] — one specifier as it appears in a file.
//! - [`extract_specifiers`] — parse a source and collect its specifiers.
//!
//! # Invariants
//! - Type-only imports and exports are not occurrences: they leave no runtime
//!   load behind.
//! - A specifier inside a `try` block is guarded. Code that already handles
//!   the load failing is not asking for a hard dependency.
//! - Only literal specifiers are recorded. A computed one names no package
//!   that could be checked against a manifest.
//! - A file that fails to parse yields no occurrences rather than an error:
//!   this is a diagnostic pass, and it must not become the thing that fails.
//!   A recovered AST with diagnostics is also rejected in its entirety.
//! - Specifiers containing unpaired surrogates are omitted, never replaced.
//!
//! # See also
//! - [`crate::specifier`] for what a recorded specifier is then classified as.

use otter_syntax::SourceKind;
use oxc_allocator::Allocator;
use oxc_ast::ast::Expression;
use oxc_ast_visit::{Visit, walk};
use oxc_parser::{ParseOptions, Parser};

/// One specifier as it appears in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    /// The specifier text as written.
    pub specifier: String,
    /// `true` when the load is inside a `try` block.
    pub guarded: bool,
}

/// Collect literal loads from a source that parses without diagnostics.
///
/// Try Module syntax, then CommonJS (which also accepts ordinary Scripts).
/// Only CommonJS permits top-level `return` and `new.target`; a CommonJS
/// fallback cannot accept module declarations. Sources with parser diagnostics
/// yield no occurrences, including recovered ASTs. This is advisory analysis,
/// not loader or compiler validation.
#[must_use]
pub fn extract_specifiers(source: &str, kind: SourceKind) -> Vec<Occurrence> {
    let module = kind.to_oxc();
    for source_type in [module, module.with_commonjs(true)] {
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(ParseOptions {
                parse_regular_expression: true,
                ..ParseOptions::default()
            })
            .parse();
        // OXC permits TS module declarations even under a Script/CommonJS goal.
        // Do not let that fallback hide a Module-goal error (e.g. top-level return).
        if !parsed.diagnostics.is_empty()
            || (source_type.is_commonjs() && parsed.module_record.has_module_syntax)
        {
            continue;
        }
        let mut visitor = SpecifierVisitor {
            occurrences: Vec::new(),
            try_depth: 0,
        };
        visitor.visit_program(&parsed.program);
        return visitor.occurrences;
    }
    Vec::new()
}

struct SpecifierVisitor {
    occurrences: Vec<Occurrence>,
    try_depth: usize,
}

impl SpecifierVisitor {
    fn record(&mut self, specifier: Option<&str>) {
        let Some(specifier) = specifier else {
            return;
        };
        if specifier.is_empty() {
            return;
        }
        self.occurrences.push(Occurrence {
            specifier: specifier.to_string(),
            guarded: self.try_depth > 0,
        });
    }
}

impl<'a> Visit<'a> for SpecifierVisitor {
    fn visit_import_declaration(&mut self, decl: &oxc_ast::ast::ImportDeclaration<'a>) {
        if decl.import_kind.is_type() {
            return;
        }
        // A declaration whose every named specifier is type-only erases the
        // same way `import type` does.
        if let Some(specifiers) = &decl.specifiers
            && !specifiers.is_empty()
            && specifiers.iter().all(|specifier| match specifier {
                oxc_ast::ast::ImportDeclarationSpecifier::ImportSpecifier(named) => {
                    named.import_kind.is_type()
                }
                _ => false,
            })
        {
            return;
        }
        self.record(decl.source.value.as_str());
    }

    fn visit_export_from_declaration(&mut self, decl: &oxc_ast::ast::ExportFromDeclaration<'a>) {
        if decl.export_kind.is_type() {
            return;
        }
        self.record(decl.source.value.as_str());
        walk::walk_export_from_declaration(self, decl);
    }

    fn visit_export_all_declaration(&mut self, decl: &oxc_ast::ast::ExportAllDeclaration<'a>) {
        if decl.export_kind.is_type() {
            return;
        }
        self.record(decl.source.value.as_str());
    }

    fn visit_import_expression(&mut self, expression: &oxc_ast::ast::ImportExpression<'a>) {
        if let Expression::StringLiteral(literal) = &expression.source {
            self.record(literal.value.as_str());
        }
        walk::walk_import_expression(self, expression);
    }

    fn visit_call_expression(&mut self, call: &oxc_ast::ast::CallExpression<'a>) {
        if let Expression::Identifier(callee) = &call.callee
            && callee.name.as_str() == "require"
            && let Some(argument) = call.arguments.first()
            && let Some(Expression::StringLiteral(literal)) = argument.as_expression()
        {
            self.record(literal.value.as_str());
        }
        walk::walk_call_expression(self, call);
    }

    fn visit_try_statement(&mut self, statement: &oxc_ast::ast::TryStatement<'a>) {
        self.try_depth += 1;
        self.visit_block_statement(&statement.block);
        self.try_depth -= 1;
        if let Some(handler) = &statement.handler {
            self.visit_catch_clause(handler);
        }
        if let Some(finalizer) = &statement.finalizer {
            self.visit_block_statement(finalizer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn specifiers(source: &str, kind: SourceKind) -> Vec<String> {
        extract_specifiers(source, kind)
            .into_iter()
            .map(|occurrence| occurrence.specifier)
            .collect()
    }

    #[test]
    fn static_imports_and_reexports_are_collected() {
        assert_eq!(
            specifiers(
                "import a from 'alpha';\nexport { b } from 'beta';\nexport * from 'gamma';",
                SourceKind::JavaScript
            ),
            ["alpha", "beta", "gamma"]
        );
    }

    #[test]
    fn dynamic_imports_and_requires_are_collected() {
        assert_eq!(
            specifiers(
                "const a = require('alpha');\nconst b = import('beta');",
                SourceKind::JavaScript
            ),
            ["alpha", "beta"]
        );
    }

    #[test]
    fn computed_specifiers_are_not_collected() {
        assert!(
            specifiers(
                "const name = 'alpha';\nrequire(name);\nimport(name);",
                SourceKind::JavaScript
            )
            .is_empty()
        );
    }

    #[test]
    fn type_only_syntax_leaves_no_occurrence() {
        assert!(
            specifiers(
                "import type { A } from 'alpha';\nexport type { B } from 'beta';",
                SourceKind::TypeScript
            )
            .is_empty()
        );
        assert!(specifiers("import { type A } from 'alpha';", SourceKind::TypeScript).is_empty());
        assert_eq!(
            specifiers(
                "import { type A, value } from 'alpha';",
                SourceKind::TypeScript
            ),
            ["alpha"]
        );
    }

    #[test]
    fn a_load_inside_a_try_block_is_guarded() {
        let occurrences = extract_specifiers(
            "try { require('optional'); } catch { require('fallback'); }\nrequire('required');",
            SourceKind::JavaScript,
        );
        assert_eq!(
            occurrences,
            [
                Occurrence {
                    specifier: "optional".to_string(),
                    guarded: true
                },
                Occurrence {
                    specifier: "fallback".to_string(),
                    guarded: false
                },
                Occurrence {
                    specifier: "required".to_string(),
                    guarded: false
                },
            ]
        );
    }

    #[test]
    fn a_file_that_does_not_parse_yields_nothing() {
        assert!(specifiers("import from from from", SourceKind::JavaScript).is_empty());
    }

    #[test]
    fn script_and_module_syntax_keep_their_dependencies() {
        for (kind, source, expected) in [
            (
                SourceKind::JavaScript,
                "var await = require('script'); await.ready; return import('commonjs');",
                vec!["script", "commonjs"],
            ),
            (
                SourceKind::TypeScript,
                "var await: number = 0; return require('commonjs');",
                vec!["commonjs"],
            ),
            (
                SourceKind::JavaScript,
                "if (new.target) require('commonjs');",
                vec!["commonjs"],
            ),
            (
                SourceKind::JavaScript,
                "await import('async-module'); export * from 'reexport';",
                vec!["async-module", "reexport"],
            ),
            (
                SourceKind::TypeScriptJsx,
                "import type { T } from 'types'; const view = <div/>; import('tsx');",
                vec!["tsx"],
            ),
        ] {
            assert_eq!(specifiers(source, kind), expected, "{source}");
        }
    }

    #[test]
    fn module_text_is_never_lossily_replaced() {
        assert_eq!(
            specifiers(
                r"require('bad\ud800'); import('bad\udfff'); require('real\ufffd'); import('pair\ud83d\udc19');",
                SourceKind::JavaScript,
            ),
            ["real�", "pair🐙"]
        );
    }
}
