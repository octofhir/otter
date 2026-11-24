//! Source syntax and binding facts available during a synchronous eval turn.
//!
//! # Contents
//! - `compile_prepared` can keep one parse and its capture analysis.
//! - `lower_cached` resolves a lazy body in its exact owning AST.
//!
//! # Invariants
//! - One bounded arena per compiler thread is retained, only for an active
//!   source scope. Its cleanup releases the arena and binding facts on
//!   normal completion, a compilation/linking failure or an unwind.
//! - The AST owns the same admitted source the VM lends back to a first
//!   call. Identical text in another allocation is insufficient.
//! - The compiler still lowers against the closure's live context chain.
//!   Reusing the AST changes neither the scope owner nor source coordinates.
//!
//! # See also
//! - `crate::lazy` owns function-body lowering and the ordinary reparse
//!   used after the source's synchronous execution scope has ended.

use std::cell::RefCell;
use std::rc::Rc;

use otter_bytecode::LazyFunction;
use otter_resource::SharedSource;
use otter_syntax::{RetainedProgram, SourceGoal};
use oxc_ast::ast::{ArrowFunctionExpression, Expression, Function, Program, Statement};
use oxc_ast_visit::{Visit, walk};
use oxc_span::{GetSpan, Span};
use oxc_syntax::scope::ScopeFlags;

use crate::capture::CaptureFacts;
use crate::lazy::{LazyCompileRequest, Target};
use crate::{BytecodeModule, CompileError, SyntaxSourceKind};

const MAX_SOURCE_BYTES: usize = 128 * 1024;
const MAX_ARENA_BYTES: usize = 4 * 1024 * 1024;

struct CachedEval {
    parse: RetainedProgram,
    captures: Rc<CaptureFacts>,
}

thread_local! {
    static CACHED: RefCell<Option<CachedEval>> = const { RefCell::new(None) };
}

pub(super) fn compile_prepared(
    source: &SharedSource,
    release: &mut Option<fn(&SharedSource)>,
    kind: SyntaxSourceKind,
    compile: impl for<'a> FnOnce(
        &'a Program<'a>,
    ) -> Result<(BytecodeModule, Rc<CaptureFacts>), CompileError>,
) -> Option<Result<BytecodeModule, CompileError>> {
    if source.len() > MAX_SOURCE_BYTES {
        return None;
    }
    CACHED.take();
    let parse = RetainedProgram::parse(source.clone(), kind, SourceGoal::Script)?;
    Some(compile(parse.program()).map(|(module, captures)| {
        if parse.arena_capacity() <= MAX_ARENA_BYTES
            && module
                .functions
                .iter()
                .any(|function| function.lazy.is_some())
        {
            *release = Some(release_source);
            CACHED.replace(Some(CachedEval { parse, captures }));
        }
        module
    }))
}

fn release_source(source: &SharedSource) {
    let _ = CACHED.try_with(|cached| {
        let mut cached = cached.borrow_mut();
        if cached
            .as_ref()
            .is_some_and(|entry| entry.parse.owns(source))
        {
            cached.take();
        }
    });
}

pub(super) fn lower_cached(
    request: &LazyCompileRequest<'_>,
    lazy: LazyFunction,
    kind: SyntaxSourceKind,
) -> Option<Result<BytecodeModule, CompileError>> {
    if lazy.module_goal || !matches!(kind, SyntaxSourceKind::JavaScript) {
        return None;
    }
    CACHED.with_borrow(|cached| {
        let cached = cached.as_ref()?;
        if !cached.parse.owns(request.source) {
            return None;
        }
        let mut finder = Finder {
            request,
            lazy,
            kind,
            captures: &cached.captures,
            result: None,
        };
        finder.visit_program(cached.parse.program());
        finder.result
    })
}

struct Finder<'a, 's> {
    request: &'a LazyCompileRequest<'s>,
    lazy: LazyFunction,
    kind: SyntaxSourceKind,
    captures: &'a Rc<CaptureFacts>,
    result: Option<Result<BytecodeModule, CompileError>>,
}

impl Finder<'_, '_> {
    fn can_contain(&self, span: Span) -> bool {
        self.result.is_none()
            && span.start <= self.request.function.span.0
            && span.end >= self.request.function.span.1
    }

    fn matches(&self, span: Span, is_arrow: bool) -> bool {
        self.result.is_none()
            && (span.start, span.end) == self.request.function.span
            && is_arrow == self.request.function.is_arrow
    }
}

impl<'a> Visit<'a> for Finder<'_, '_> {
    fn visit_statement(&mut self, statement: &Statement<'a>) {
        if self.can_contain(statement.span()) {
            walk::walk_statement(self, statement);
        }
    }

    fn visit_expression(&mut self, expression: &Expression<'a>) {
        if self.can_contain(expression.span()) {
            walk::walk_expression(self, expression);
        }
    }

    fn visit_function(&mut self, node: &Function<'a>, flags: ScopeFlags) {
        if self.matches(node.span, false) {
            self.result = Some(crate::lazy::lower_target(
                Target::Function(node),
                self.request,
                self.lazy,
                self.kind,
                Rc::clone(self.captures),
                None,
            ));
        } else if self.can_contain(node.span) {
            walk::walk_function(self, node, flags);
        }
    }

    fn visit_arrow_function_expression(&mut self, node: &ArrowFunctionExpression<'a>) {
        if self.matches(node.span, true) {
            self.result = Some(crate::lazy::lower_target(
                Target::Arrow(node),
                self.request,
                self.lazy,
                self.kind,
                Rc::clone(self.captures),
                None,
            ));
        } else if self.can_contain(node.span) {
            walk::walk_arrow_function_expression(self, node);
        }
    }
}
