//! Parse the separately supplied formals and body of a dynamic function.
//!
//! # Contents
//! - [`with_dynamic_function_program`] lends the checked AST to lowering.
//!
//! # Invariants
//! - The parser must consume the generated closing parenthesis and both
//!   generated braces as the delimiters of the same outer function. This
//!   establishes the formal-parameter and function-body grammar goals in one
//!   parse, including their generator and async variants. Combined early
//!   errors, such as duplicate formals in a strict body, remain with lowering.
//! - Script-goal compatibility rewrites translate the parameter boundary
//!   before it is compared with the resulting AST.
//!
//! # See also
//! - [`crate::with_program_goal`] for ordinary scripts and modules.

use oxc_ast::ast::{Expression, Program, Statement};

use crate::{SourceGoal, SourceKind, SyntaxError};

/// Parse a parenthesized dynamic function whose generated parameter list
/// ends at `parameters_end`, immediately before the space in `) {`.
///
/// # Errors
/// Returns a syntax error if either fragment is invalid or crosses a
/// delimiter inserted by the constructor.
pub fn with_dynamic_function_program<R>(
    source: &str,
    kind: SourceKind,
    parameters_end: usize,
    f: impl for<'a> FnOnce(&'a Program<'a>) -> R,
) -> Result<R, SyntaxError> {
    if parameters_end >= source.len() {
        return Err(delimiter_error());
    }
    crate::with_program_goal_after_parse(
        source,
        kind,
        SourceGoal::Script,
        Some(parameters_end),
        || (),
        f,
    )
    .map(|(result, ())| result)
}

pub(super) fn delimiter_error() -> SyntaxError {
    SyntaxError::from_messages(vec![
        "Invalid dynamic function parameter list or body".to_owned(),
    ])
}

pub(super) fn check_delimiters(
    program: &Program<'_>,
    parameters_end: usize,
) -> Result<(), SyntaxError> {
    if let [Statement::ExpressionStatement(statement)] = program.body.as_slice()
        && let Expression::FunctionExpression(function) = statement.expression.without_parentheses()
        && let Some(body) = &function.body
        && function.span.start == 1
        && function.params.span.end as usize == parameters_end
        && body.span.start as usize == parameters_end + 1
        && body.span.end as usize + 1 == program.source_text.len()
    {
        return Ok(());
    }
    Err(delimiter_error())
}
