//! A function's body as the compiler reads it.
//!
//! A function or method has a block body. An arrow has either a block or
//! one concise expression, which the parser keeps as the expression itself.
//! Every analysis that walks a body, and every scope that keys facts by
//! one, takes a [`Body`].
//!
//! # Contents
//! - [`Body`] — a block body or an arrow's body.
//!
//! # Invariants
//! - [`Body::key`] is the address of the body node the parser allocated:
//!   stable for the life of the AST, and the same whichever pass asks.
//! - A concise arrow body has no statements and no directives.

use oxc_ast::ast::{ArrowFunctionBody, Expression, FunctionBody, Statement};
use oxc_ast_visit::Visit;
use oxc_span::{GetSpan, Span};

/// A function's body: a block, or an arrow's body.
#[derive(Clone, Copy)]
pub(crate) enum Body<'r, 'a> {
    /// A function's or method's block.
    Block(&'r FunctionBody<'a>),
    /// An arrow's block or concise expression.
    Arrow(&'r ArrowFunctionBody<'a>),
}

impl<'r, 'a> Body<'r, 'a> {
    /// The address facts about this body are kept under.
    pub(crate) fn key(self) -> usize {
        match self {
            Self::Block(body) => std::ptr::from_ref(body) as usize,
            Self::Arrow(body) => std::ptr::from_ref(body) as usize,
        }
    }

    /// The block, unless this is a concise arrow body.
    pub(crate) fn block(self) -> Option<&'r FunctionBody<'a>> {
        match self {
            Self::Block(body) => Some(body),
            Self::Arrow(body) => body.as_function_body(),
        }
    }

    /// The concise arrow body's expression.
    pub(crate) fn expression(self) -> Option<&'r Expression<'a>> {
        match self {
            Self::Block(_) => None,
            Self::Arrow(body) => body.as_expression(),
        }
    }

    /// The block's statements; none for a concise body.
    pub(crate) fn statements(self) -> &'r [Statement<'a>] {
        self.block().map_or(&[], |body| body.statements.as_slice())
    }

    /// Whether the block opens with a `"use strict"` directive.
    pub(crate) fn has_use_strict_directive(self) -> bool {
        self.block()
            .is_some_and(FunctionBody::has_use_strict_directive)
    }

    /// The body's source span.
    pub(crate) fn span(self) -> Span {
        match self {
            Self::Block(body) => body.span,
            Self::Arrow(body) => body.span(),
        }
    }

    /// Walk the body's statements, or its concise expression, with
    /// `visitor`, without entering the body node itself.
    pub(crate) fn visit_contents<V: Visit<'a>>(self, visitor: &mut V) {
        match self.expression() {
            Some(expression) => visitor.visit_expression(expression),
            None => {
                for statement in self.statements() {
                    visitor.visit_statement(statement);
                }
            }
        }
    }

    /// Walk the body with `visitor`.
    pub(crate) fn visit<V: Visit<'a>>(self, visitor: &mut V) {
        match self {
            Self::Block(body) => visitor.visit_function_body(body),
            Self::Arrow(body) => visitor.visit_arrow_function_body(body),
        }
    }
}
