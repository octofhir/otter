//! Specifier extraction from one source file.
//!
//! Extraction runs on the same parser the engine loads code with, so what is
//! reported is what would actually be imported — not what a text search
//! guessed at. Type-only syntax is erased before it is ever recorded, and an
//! import that a `try` block already accounts for is marked guarded rather
//! than reported as a hard requirement.
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

/// Parse `source` and collect every literal module specifier it loads.
#[must_use]
pub fn extract_specifiers(source: &str, kind: SourceKind) -> Vec<Occurrence> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, kind.to_oxc())
        .with_options(ParseOptions {
            allow_return_outside_function: true,
            ..ParseOptions::default()
        })
        .parse();
    let mut visitor = SpecifierVisitor {
        occurrences: Vec::new(),
        try_depth: 0,
    };
    visitor.visit_program(&parsed.program);
    visitor.occurrences
}

struct SpecifierVisitor {
    occurrences: Vec<Occurrence>,
    try_depth: usize,
}

impl SpecifierVisitor {
    fn record(&mut self, specifier: &str) {
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

    fn visit_export_named_declaration(&mut self, decl: &oxc_ast::ast::ExportNamedDeclaration<'a>) {
        if decl.export_kind.is_type() {
            return;
        }
        if let Some(source) = &decl.source {
            self.record(source.value.as_str());
        }
        walk::walk_export_named_declaration(self, decl);
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
}
