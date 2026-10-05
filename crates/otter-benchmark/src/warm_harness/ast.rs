//! OXC Script inspection and byte-span copying for immutable warm anchors.
//!
//! # Contents
//! - Structural content comparisons for exact original drivers.
//! - Reserved-name and moved-binding rejection.
//! - Original/decoded source-span identity records.
//!
//! # Invariants
//! - No regex, evaluator, source replacement or AST arena escapes are used.
//! - Parser compatibility rewrites are rejected before original-span copying.
//! - Source comments and literals remain original bytes.
//!
//! # See also
//! - `super::recipes` selects the complete work for all seven anchors.

use otter_syntax::{SourceGoal, SourceKind, with_program_goal};
use oxc_ast::ast::{Function, Program, Statement};
use oxc_ast_visit::Visit;
use oxc_span::{ContentEq, GetSpan, Span};

use super::{WarmHarnessError, WarmPieceRole, WarmSourcePiece, reject, source_sha256};

pub(super) fn parse<R>(
    source: &str,
    consume: impl for<'a> FnOnce(&'a Program<'a>) -> Result<R, WarmHarnessError>,
) -> Result<R, WarmHarnessError> {
    with_program_goal(
        source,
        SourceKind::JavaScript,
        SourceGoal::Script,
        |program| {
            if program.source_text != source {
                return Err(reject(
                    "source-span copying rejects parser compatibility rewrites",
                ));
            }
            consume(program)
        },
    )
    .map_err(|error| reject(format!("Script parse: {error:?}")))?
}

#[derive(Default)]
struct Reserved {
    names: Vec<String>,
}
impl<'a> Visit<'a> for Reserved {
    fn visit_binding_identifier(&mut self, it: &oxc_ast::ast::BindingIdentifier<'a>) {
        if it.name.starts_with("__rfWarm") {
            self.names.push(it.name.to_string());
        }
    }
    fn visit_identifier_reference(&mut self, it: &oxc_ast::ast::IdentifierReference<'a>) {
        if it.name.starts_with("__rfWarm") {
            self.names.push(it.name.to_string());
        }
    }
    fn visit_identifier_name(&mut self, it: &oxc_ast::ast::IdentifierName<'a>) {
        if it.name.starts_with("__rfWarm") {
            self.names.push(it.name.to_string());
        }
    }
    fn visit_string_literal(&mut self, it: &oxc_ast::ast::StringLiteral<'a>) {
        if it.value.starts_with("__rfWarm") {
            self.names.push(it.value.to_string());
        }
    }
}

pub(super) fn check_original(program: &Program<'_>) -> Result<(), WarmHarnessError> {
    if !program.directives.is_empty() || program.hashbang.is_some() {
        return Err(reject(
            "anchor directives/hashbang would change generated Script geometry",
        ));
    }
    let mut reserved = Reserved::default();
    reserved.visit_program(program);
    if let Some(name) = reserved.names.first() {
        return Err(reject(format!(
            "reserved harness identifier collision: {name}"
        )));
    }
    Ok(())
}

pub(super) fn tail<'a, 'p>(
    program: &'p Program<'a>,
    expected: &str,
) -> Result<&'p [Statement<'a>], WarmHarnessError> {
    let count = parse(expected, |template| {
        let count = template.body.len();
        let Some(actual) = program.body.get(program.body.len().saturating_sub(count)..) else {
            return Err(reject("missing complete fixed driver"));
        };
        if count == 0
            || actual.len() != count
            || !actual
                .iter()
                .zip(&template.body)
                .all(|(a, b)| a.content_eq(b))
        {
            return Err(reject(
                "final fixed driver differs from the original AST contract",
            ));
        }
        Ok(count)
    })?;
    Ok(&program.body[program.body.len() - count..])
}

pub(super) fn matching_statement<'a, 'p>(
    program: &'p Program<'a>,
    expected: &str,
) -> Result<&'p Statement<'a>, WarmHarnessError> {
    let index = parse(expected, |template| {
        if template.body.len() != 1 {
            return Err(reject("internal statement template must have one node"));
        }
        let matches: Vec<_> = program
            .body
            .iter()
            .enumerate()
            .filter(|(_, statement)| statement.content_eq(&template.body[0]))
            .map(|(index, _)| index)
            .collect();
        if matches.len() != 1 {
            return Err(reject("original reset statement is missing or ambiguous"));
        }
        Ok(matches[0])
    })?;
    Ok(&program.body[index])
}

pub(super) fn function<'a, 'p>(
    program: &'p Program<'a>,
    name: &str,
) -> Result<&'p Function<'a>, WarmHarnessError> {
    let matches: Vec<_> = program
        .body
        .iter()
        .filter_map(|statement| match statement {
            Statement::FunctionDeclaration(function)
                if function.id.as_ref().is_some_and(|id| id.name == name) =>
            {
                Some(&**function)
            }
            _ => None,
        })
        .collect();
    if matches.len() != 1 {
        return Err(reject(format!("expected one top-level function {name}")));
    }
    Ok(matches[0])
}

pub(super) fn text(source: &str, span: Span) -> Result<&str, WarmHarnessError> {
    source
        .get(span.start as usize..span.end as usize)
        .ok_or_else(|| {
            reject(format!(
                "invalid UTF-8 source span {}..{}",
                span.start, span.end
            ))
        })
}

pub(super) fn piece(
    source: &str,
    span: Span,
    role: WarmPieceRole,
) -> Result<WarmSourcePiece, WarmHarnessError> {
    Ok(WarmSourcePiece {
        role,
        start: span.start,
        end: span.end,
        sha256: source_sha256(text(source, span)?.as_bytes()),
    })
}

pub(super) fn result_span(statement: &Statement<'_>) -> Result<Span, WarmHarnessError> {
    if let Statement::ExpressionStatement(statement) = statement
        && let oxc_ast::ast::Expression::CallExpression(call) = &statement.expression
        && call.arguments.len() == 1
    {
        return Ok(call.arguments[0].span());
    }
    Err(reject(
        "original result must be a one-argument console call",
    ))
}

#[derive(Default)]
struct References {
    names: Vec<String>,
}
impl<'a> Visit<'a> for References {
    fn visit_identifier_reference(&mut self, it: &oxc_ast::ast::IdentifierReference<'a>) {
        self.names.push(it.name.to_string());
    }
}

/// The only moved var binding is ast_ctor's driver r. Its setup is small
/// and contains no other reference named r, so a conservative visitor suffices.
pub(super) fn no_external_driver_var(
    program: &Program<'_>,
    driver: Span,
    name: &str,
) -> Result<(), WarmHarnessError> {
    let mut refs = References::default();
    for statement in &program.body {
        if statement.span().start < driver.start {
            refs.visit_statement(statement);
        }
    }
    if refs.names.iter().any(|reference| reference == name) {
        return Err(reject(format!(
            "moved driver var {name} is referenced by original setup"
        )));
    }
    Ok(())
}
