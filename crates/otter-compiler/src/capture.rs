//! Capture analysis: which of a scope's own bindings must live in a context
//! slot rather than a register.
//!
//! A binding needs a slot when a nested function references it, when any
//! direct eval (strict or sloppy) can see it, when a sloppy mapped arguments
//! object aliases it, or — for a derived constructor's `this` — when an
//! arrow, a direct eval, or an arrow `super()` observes it. Over-promotion is
//! sound (a slot nothing reads), under-promotion is not.
//!
//! # Contents
//! - [`CaptureFacts`] — one post-order pass over a compile unit. For every
//!   function body, static block and the unit itself it records the names the
//!   scope declares, the names its nested functions reference, the names it
//!   or a nested scope assigns, how far a direct eval reaches, whether the
//!   scope reads `arguments` and whether that object escapes beyond `.length`
//!   and element reads; for the whole unit, the source position of every
//!   identifier reference and direct eval, answering "does this range mention
//!   `x`" by binary search.
//! - [`CaptureFacts::nested_refs_in_statements`] and friends — region queries
//!   for block, `switch` and loop-head predeclaration.
//! - Single-construct finders: derived `this`, `with` reachability,
//!   `.arguments` reads, `new.target`.
//!
//! # Invariants
//! - Every scope is walked once; a nested scope hands its ancestors only the
//!   names that escape it. Names are compared by spelling: a nested function
//!   binding a name as a parameter, `var`, `arguments` or top-level declaration
//!   hides it from its ancestors; block-level shadowing does not (that only
//!   over-promotes).
//! - Facts are keyed by AST node address and valid while the unit's AST lives;
//!   every function body and static block the compiler lowers belongs to the
//!   analyzed unit.

use std::collections::HashSet;

use oxc_ast::ast::{
    ArrowFunctionExpression, BindingPattern, Class, Expression, FormalParameters, Function,
    FunctionBody, Statement,
};
use oxc_ast_visit::{Visit, walk};
use oxc_span::GetSpan;
use rustc_hash::{FxHashMap, FxHashSet};

type NameId = u32;

/// Capture-relevant facts of one scope: a function body, a class static
/// block, or the compile unit's top level.
#[derive(Debug, Default)]
pub(crate) struct ScopeFacts {
    /// Names declared at any block depth of the scope's own code (parameters,
    /// declarations, catch parameters, function and class names).
    own: Vec<NameId>,
    /// Names referenced from functions nested in the scope (and from class
    /// field initializers and static blocks, which lower to nested frames).
    inner: FxHashSet<NameId>,
    /// A direct eval in the scope or in a nested function that does not
    /// rebind `eval`.
    nested_eval: bool,
    /// Names referenced inside the scope that it does not bind.
    escape: FxHashSet<NameId>,
    /// `nested_eval`, unless the scope binds `eval`.
    escape_eval: bool,
    /// A direct eval in the parameter list's own code.
    params_eval: bool,
    /// A direct eval in the body's own code (outside nested functions,
    /// arrows and classes).
    body_eval: bool,
    /// `arguments` is referenced outside nested non-arrow functions and
    /// class bodies.
    uses_arguments: bool,
    /// The scope's `arguments` object is used other than by `.length` and
    /// element reads in its own code: written, called through, deleted from,
    /// referenced bare or from an arrow.
    arguments_escapes: bool,
    /// Names assigned in the scope's code or in a nested scope that does not
    /// bind them, by spelling (block shadowing only over-reports).
    assigned: FxHashSet<NameId>,
    /// Source range of the parameters and body.
    range: (u32, u32),
}

impl ScopeFacts {
    /// A direct eval in the parameter list's own code.
    pub(crate) fn params_eval(&self) -> bool {
        self.params_eval
    }

    /// A direct eval in the body's own code.
    pub(crate) fn body_eval(&self) -> bool {
        self.body_eval
    }

    /// The scope reads its own `arguments` (arrows included).
    pub(crate) fn uses_arguments(&self) -> bool {
        self.uses_arguments
    }

    /// The scope's `arguments` object is used beyond `.length` and element
    /// reads in its own code.
    pub(crate) fn arguments_escapes(&self) -> bool {
        self.arguments_escapes
    }
}

/// Capture facts of one compile unit.
#[derive(Debug, Default)]
pub(crate) struct CaptureFacts {
    names: Vec<Box<str>>,
    ids: FxHashMap<Box<str>, NameId>,
    scopes: FxHashMap<usize, ScopeFacts>,
    unit: ScopeFacts,
    /// Start offsets of every identifier reference, per name, ascending.
    references: FxHashMap<NameId, Vec<u32>>,
    /// Start offsets of every direct-eval call, ascending.
    evals: Vec<u32>,
    reads_dot_arguments: bool,
}

impl CaptureFacts {
    /// Analyze the top-level statements of one compile unit.
    #[must_use]
    pub(crate) fn of_unit(stmts: &[Statement<'_>]) -> Self {
        let mut builder = Builder::default();
        builder.facts.intern("arguments");
        builder.facts.intern("eval");
        builder.frames.push(Frame::new(FrameKind::Unit, 0));
        for stmt in stmts {
            builder.visit_statement(stmt);
        }
        let unit = builder.frames.pop().expect("unit frame");
        let mut facts = builder.facts;
        facts.unit = unit.facts;
        for positions in facts.references.values_mut() {
            positions.sort_unstable();
        }
        facts.evals.sort_unstable();
        facts
    }

    /// Facts of a function or arrow body of this unit.
    pub(crate) fn function(&self, body: &FunctionBody<'_>) -> &ScopeFacts {
        self.scope(std::ptr::from_ref(body) as usize)
    }

    /// Facts of a class static block of this unit.
    pub(crate) fn static_block(&self, body: &oxc_allocator::Vec<'_, Statement<'_>>) -> &ScopeFacts {
        self.scope(std::ptr::from_ref(body) as usize)
    }

    /// Facts of the unit's top level.
    pub(crate) fn unit(&self) -> &ScopeFacts {
        &self.unit
    }

    fn scope(&self, key: usize) -> &ScopeFacts {
        self.scopes
            .get(&key)
            .expect("capture facts cover every scope of the compile unit")
    }

    /// Own names that a nested function references — every own name when a
    /// direct eval reaches the scope. `with_arguments` counts the scope's
    /// `arguments` binding among its own names.
    pub(crate) fn captured(&self, scope: &ScopeFacts, with_arguments: bool) -> HashSet<String> {
        let arguments = with_arguments.then_some(ARGUMENTS);
        scope
            .own
            .iter()
            .copied()
            .chain(arguments)
            .filter(|id| scope.nested_eval || scope.inner.contains(id))
            .map(|id| self.names[id as usize].to_string())
            .collect()
    }

    /// Every name the scope declares, `arguments` included when asked.
    pub(crate) fn own_names(&self, scope: &ScopeFacts, with_arguments: bool) -> HashSet<String> {
        let arguments = with_arguments.then_some(ARGUMENTS);
        scope
            .own
            .iter()
            .copied()
            .chain(arguments)
            .map(|id| self.names[id as usize].to_string())
            .collect()
    }

    /// Whether the scope's code, or a nested scope not binding it, may assign
    /// `name`.
    pub(crate) fn assigned(&self, scope: &ScopeFacts, name: &str) -> bool {
        self.ids
            .get(name)
            .is_some_and(|id| scope.assigned.contains(id))
    }

    /// Whether a function nested in the scope references `name`.
    pub(crate) fn inner_references(&self, scope: &ScopeFacts, name: &str) -> bool {
        self.ids.get(name).is_some_and(|id| scope.inner.contains(id))
    }

    /// Whether the scope's parameters or body name `name` at any depth,
    /// shadowed or not.
    pub(crate) fn references(&self, scope: &ScopeFacts, name: &str) -> bool {
        self.ids
            .get(name)
            .and_then(|id| self.references.get(id))
            .is_some_and(|positions| any_within(positions, scope.range))
    }

    /// Whether the scope contains a direct eval at any depth.
    pub(crate) fn contains_eval(&self, scope: &ScopeFacts) -> bool {
        any_within(&self.evals, scope.range)
    }

    /// Whether the unit contains a direct eval anywhere.
    pub(crate) fn unit_contains_eval(&self) -> bool {
        !self.evals.is_empty()
    }

    /// Whether the unit reads a `.arguments` property anywhere.
    pub(crate) fn reads_dot_arguments(&self) -> bool {
        self.reads_dot_arguments
    }

    /// Names referenced from functions nested in `stmts`, and whether such a
    /// function or the statements themselves contain a direct eval.
    pub(crate) fn nested_refs_in_statements<'s, 'a: 's>(
        &self,
        stmts: impl IntoIterator<Item = &'s Statement<'a>>,
    ) -> (HashSet<String>, bool) {
        let mut region = Region::new(self);
        for stmt in stmts {
            region.visit_statement(stmt);
        }
        region.finish()
    }

    /// [`Self::nested_refs_in_statements`] for one expression.
    pub(crate) fn nested_refs_in_expression(&self, expr: &Expression<'_>) -> (HashSet<String>, bool) {
        let mut region = Region::new(self);
        region.visit_expression(expr);
        region.finish()
    }

    fn intern(&mut self, name: &str) -> NameId {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = NameId::try_from(self.names.len()).expect("name table exceeds u32");
        self.names.push(name.into());
        self.ids.insert(name.into(), id);
        id
    }
}

const ARGUMENTS: NameId = 0;
const EVAL: NameId = 1;

fn any_within(sorted: &[u32], (start, end): (u32, u32)) -> bool {
    let first = sorted.partition_point(|&position| position < start);
    sorted.get(first).is_some_and(|&position| position < end)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    Unit,
    Function,
    Arrow,
    StaticBlock,
}

#[derive(Debug)]
struct Frame {
    kind: FrameKind,
    key: usize,
    facts: ScopeFacts,
    /// Names referenced in the scope and not bound by a nested scope.
    refs: FxHashSet<NameId>,
    /// Parameters, top-level declarations and `arguments`.
    bound: FxHashSet<NameId>,
    /// `var`s at any block depth, static blocks included.
    vars: FxHashSet<NameId>,
    /// Names assigned in the scope and not bound by a nested scope.
    writes: FxHashSet<NameId>,
    class_depth: u32,
    class_body_depth: u32,
    /// Inside a class field initializer or computed field key.
    field_depth: u32,
    in_params: bool,
}

impl Frame {
    fn new(kind: FrameKind, key: usize) -> Self {
        Self {
            kind,
            key,
            facts: ScopeFacts::default(),
            refs: FxHashSet::default(),
            bound: FxHashSet::default(),
            vars: FxHashSet::default(),
            writes: FxHashSet::default(),
            class_depth: 0,
            class_body_depth: 0,
            field_depth: 0,
            in_params: false,
        }
    }
}

#[derive(Default)]
struct Builder {
    facts: CaptureFacts,
    frames: Vec<Frame>,
    /// Start of the `arguments` reference that is the object of the member
    /// read being visited.
    arguments_read: Option<u32>,
}

impl Builder {
    fn top(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("scope frame")
    }

    fn own(&mut self, name: &str) {
        let id = self.facts.intern(name);
        let frame = self.top();
        if frame.class_depth == 0 {
            frame.facts.own.push(id);
        }
    }

    fn own_pattern(&mut self, pattern: &BindingPattern<'_>) {
        if self.top().class_depth == 0 {
            pattern_leaves(pattern, &mut |name| self.own(name));
        }
    }

    fn enter_function(
        &mut self,
        kind: FrameKind,
        params: &FormalParameters<'_>,
        body: &FunctionBody<'_>,
    ) {
        self.frames
            .push(Frame::new(kind, std::ptr::from_ref(body) as usize));
        self.top().in_params = true;
        self.visit_formal_parameters(params);
        self.top().in_params = false;
        // Every name the parameter list declares covers the whole function.
        let frame = self.top();
        frame.bound.extend(frame.facts.own.iter().copied());
        if kind == FrameKind::Function {
            frame.bound.insert(ARGUMENTS);
        }
        for stmt in &body.statements {
            match stmt {
                Statement::VariableDeclaration(declaration) => {
                    for declarator in &declaration.declarations {
                        pattern_leaves(&declarator.id, &mut |name| {
                            let id = self.facts.intern(name);
                            self.top().bound.insert(id);
                        });
                    }
                }
                Statement::FunctionDeclaration(function) => {
                    if let Some(id) = &function.id {
                        let id = self.facts.intern(&id.name);
                        self.top().bound.insert(id);
                    }
                }
                Statement::ClassDeclaration(class) => {
                    if let Some(id) = &class.id {
                        let id = self.facts.intern(&id.name);
                        self.top().bound.insert(id);
                    }
                }
                _ => {}
            }
        }
        self.visit_function_body(body);
        let start = params.span.start.min(body.span.start);
        self.exit((start, body.span.end));
    }

    fn exit(&mut self, range: (u32, u32)) {
        let mut frame = self.frames.pop().expect("scope frame");
        let parent = self.top();
        if frame.kind == FrameKind::StaticBlock {
            parent.vars.extend(frame.vars.iter().copied());
        } else {
            frame.bound.extend(frame.vars.iter().copied());
        }
        frame.refs.retain(|id| !frame.bound.contains(id));
        parent.writes.extend(
            frame
                .writes
                .iter()
                .copied()
                .filter(|id| !frame.bound.contains(id)),
        );
        frame.facts.assigned = std::mem::take(&mut frame.writes);
        let escape_eval = frame.facts.nested_eval && !frame.bound.contains(&EVAL);
        parent.refs.extend(frame.refs.iter().copied());
        parent.facts.inner.extend(frame.refs.iter().copied());
        parent.facts.nested_eval |= escape_eval;
        frame.facts.escape = frame.refs;
        frame.facts.escape_eval = escape_eval;
        frame.facts.range = range;
        self.facts.scopes.insert(frame.key, frame.facts);
    }

    /// `arguments` read in the innermost scope reaches through arrows to the
    /// nearest non-arrow function, unless a class body intervenes. A read
    /// through an arrow, or any use but a member read, escapes the object.
    fn note_arguments(&mut self, member_read: bool) {
        let mut escapes = !member_read;
        for frame in self.frames.iter_mut().rev() {
            if frame.class_body_depth > 0 {
                return;
            }
            frame.facts.uses_arguments = true;
            if frame.kind != FrameKind::Arrow {
                frame.facts.arguments_escapes |= escapes;
                return;
            }
            escapes = true;
        }
    }

    /// A use of the innermost `arguments` object other than a read.
    fn note_arguments_escape(&mut self) {
        for frame in self.frames.iter_mut().rev() {
            if frame.class_body_depth > 0 {
                return;
            }
            if frame.kind != FrameKind::Arrow {
                frame.facts.arguments_escapes = true;
                return;
            }
        }
    }

    fn note_write(&mut self, name: &str) {
        let id = self.facts.intern(name);
        self.top().writes.insert(id);
    }
}

impl<'a> Visit<'a> for Builder {
    fn visit_function(&mut self, it: &Function<'a>, _flags: oxc_syntax::scope::ScopeFlags) {
        if let Some(id) = &it.id {
            self.own(&id.name);
            if it.is_declaration() {
                self.note_write(&id.name);
            }
        }
        // A body-less declaration (an overload signature) has no runtime scope.
        if let Some(body) = it.body.as_deref() {
            self.enter_function(FrameKind::Function, &it.params, body);
        }
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        self.enter_function(FrameKind::Arrow, &it.params, &it.body);
    }

    fn visit_static_block(&mut self, it: &oxc_ast::ast::StaticBlock<'a>) {
        self.frames.push(Frame::new(
            FrameKind::StaticBlock,
            std::ptr::from_ref(&it.body) as usize,
        ));
        for stmt in &it.body {
            self.visit_statement(stmt);
        }
        self.exit((it.span.start, it.span.end));
    }

    fn visit_class(&mut self, it: &Class<'a>) {
        if let Some(id) = &it.id {
            self.own(&id.name);
        }
        self.top().class_depth += 1;
        walk::walk_class(self, it);
        self.top().class_depth -= 1;
    }

    fn visit_class_body(&mut self, it: &oxc_ast::ast::ClassBody<'a>) {
        self.top().class_body_depth += 1;
        walk::walk_class_body(self, it);
        self.top().class_body_depth -= 1;
    }

    fn visit_property_definition(&mut self, it: &oxc_ast::ast::PropertyDefinition<'a>) {
        // Field initializers and computed field keys lower into a nested
        // frame (the constructor or the statics initializer).
        self.top().field_depth += 1;
        walk::walk_property_definition(self, it);
        self.top().field_depth -= 1;
    }

    fn visit_formal_parameters(&mut self, it: &FormalParameters<'a>) {
        if let Some(rest) = &it.rest {
            self.own_pattern(&rest.rest.argument);
        }
        walk::walk_formal_parameters(self, it);
    }

    fn visit_formal_parameter(&mut self, it: &oxc_ast::ast::FormalParameter<'a>) {
        self.own_pattern(&it.pattern);
        walk::walk_formal_parameter(self, it);
    }

    fn visit_variable_declaration(&mut self, it: &oxc_ast::ast::VariableDeclaration<'a>) {
        if it.kind.is_var() {
            for declarator in &it.declarations {
                pattern_leaves(&declarator.id, &mut |name| {
                    let id = self.facts.intern(name);
                    self.top().vars.insert(id);
                });
            }
        }
        walk::walk_variable_declaration(self, it);
    }

    fn visit_variable_declarator(&mut self, it: &oxc_ast::ast::VariableDeclarator<'a>) {
        self.own_pattern(&it.id);
        if it.init.is_some() {
            pattern_leaves(&it.id, &mut |name| self.note_write(name));
        }
        walk::walk_variable_declarator(self, it);
    }

    fn visit_for_in_statement(&mut self, it: &oxc_ast::ast::ForInStatement<'a>) {
        if let oxc_ast::ast::ForStatementLeft::VariableDeclaration(declaration) = &it.left {
            for declarator in &declaration.declarations {
                pattern_leaves(&declarator.id, &mut |name| self.note_write(name));
            }
        }
        walk::walk_for_in_statement(self, it);
    }

    fn visit_for_of_statement(&mut self, it: &oxc_ast::ast::ForOfStatement<'a>) {
        if let oxc_ast::ast::ForStatementLeft::VariableDeclaration(declaration) = &it.left {
            for declarator in &declaration.declarations {
                pattern_leaves(&declarator.id, &mut |name| self.note_write(name));
            }
        }
        walk::walk_for_of_statement(self, it);
    }

    fn visit_simple_assignment_target(&mut self, it: &oxc_ast::ast::SimpleAssignmentTarget<'a>) {
        match it {
            oxc_ast::ast::SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => {
                self.note_write(&id.name);
            }
            _ => {
                if it
                    .as_member_expression()
                    .is_some_and(|member| is_arguments(member.object()))
                {
                    self.note_arguments_escape();
                }
            }
        }
        walk::walk_simple_assignment_target(self, it);
    }

    fn visit_assignment_target_property_identifier(
        &mut self,
        it: &oxc_ast::ast::AssignmentTargetPropertyIdentifier<'a>,
    ) {
        self.note_write(&it.binding.name);
        walk::walk_assignment_target_property_identifier(self, it);
    }

    fn visit_unary_expression(&mut self, it: &oxc_ast::ast::UnaryExpression<'a>) {
        if it.operator == oxc_syntax::operator::UnaryOperator::Delete
            && member_of_arguments(&it.argument)
        {
            self.note_arguments_escape();
        }
        walk::walk_unary_expression(self, it);
    }

    fn visit_tagged_template_expression(
        &mut self,
        it: &oxc_ast::ast::TaggedTemplateExpression<'a>,
    ) {
        if member_of_arguments(&it.tag) {
            self.note_arguments_escape();
        }
        walk::walk_tagged_template_expression(self, it);
    }

    fn visit_catch_parameter(&mut self, it: &oxc_ast::ast::CatchParameter<'a>) {
        self.own_pattern(&it.pattern);
        walk::walk_catch_parameter(self, it);
    }

    fn visit_identifier_reference(&mut self, it: &oxc_ast::ast::IdentifierReference<'a>) {
        let id = self.facts.intern(&it.name);
        self.facts
            .references
            .entry(id)
            .or_default()
            .push(it.span.start);
        let frame = self.top();
        frame.refs.insert(id);
        if frame.field_depth > 0 {
            frame.facts.inner.insert(id);
        }
        if id == ARGUMENTS {
            let member_read = self.arguments_read.take() == Some(it.span.start);
            self.note_arguments(member_read);
        }
    }

    fn visit_call_expression(&mut self, it: &oxc_ast::ast::CallExpression<'a>) {
        // A method called on `arguments` receives it as `this`.
        if member_of_arguments(&it.callee) {
            self.note_arguments_escape();
        }
        if callee_is_direct_eval(&it.callee) {
            self.facts.evals.push(it.span.start);
            let frame = self.top();
            frame.facts.nested_eval = true;
            if frame.class_depth == 0 {
                if frame.in_params {
                    frame.facts.params_eval = true;
                } else {
                    frame.facts.body_eval = true;
                }
            }
        }
        walk::walk_call_expression(self, it);
    }

    fn visit_static_member_expression(&mut self, it: &oxc_ast::ast::StaticMemberExpression<'a>) {
        if it.property.name == "arguments" {
            self.facts.reads_dot_arguments = true;
        }
        if it.property.name == "length" && is_arguments(&it.object) {
            self.arguments_read = Some(it.object.span().start);
        }
        walk::walk_static_member_expression(self, it);
    }

    fn visit_computed_member_expression(
        &mut self,
        it: &oxc_ast::ast::ComputedMemberExpression<'a>,
    ) {
        if matches!(&it.expression, Expression::StringLiteral(key) if key.value == "arguments") {
            self.facts.reads_dot_arguments = true;
        }
        if is_arguments(&it.object) {
            self.arguments_read = Some(it.object.span().start);
        }
        walk::walk_computed_member_expression(self, it);
    }
}

/// Collector of the names nested functions in one region reference, reading
/// each nested scope's escaping names from the unit's facts.
struct Region<'f> {
    facts: &'f CaptureFacts,
    refs: FxHashSet<NameId>,
    eval: bool,
    field_depth: u32,
}

impl<'f> Region<'f> {
    fn new(facts: &'f CaptureFacts) -> Self {
        Self {
            facts,
            refs: FxHashSet::default(),
            eval: false,
            field_depth: 0,
        }
    }

    fn absorb(&mut self, scope: &ScopeFacts) {
        self.refs.extend(scope.escape.iter().copied());
        self.eval |= scope.escape_eval;
    }

    fn finish(self) -> (HashSet<String>, bool) {
        let names = self
            .refs
            .into_iter()
            .map(|id| self.facts.names[id as usize].to_string())
            .collect();
        (names, self.eval)
    }
}

impl<'a> Visit<'a> for Region<'_> {
    fn visit_function(&mut self, it: &Function<'a>, _flags: oxc_syntax::scope::ScopeFlags) {
        if let Some(body) = it.body.as_deref() {
            self.absorb(self.facts.function(body));
        }
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        self.absorb(self.facts.function(&it.body));
    }

    fn visit_static_block(&mut self, it: &oxc_ast::ast::StaticBlock<'a>) {
        self.absorb(self.facts.static_block(&it.body));
    }

    fn visit_property_definition(&mut self, it: &oxc_ast::ast::PropertyDefinition<'a>) {
        self.field_depth += 1;
        walk::walk_property_definition(self, it);
        self.field_depth -= 1;
    }

    fn visit_identifier_reference(&mut self, it: &oxc_ast::ast::IdentifierReference<'a>) {
        if self.field_depth > 0
            && let Some(&id) = self.facts.ids.get(it.name.as_str())
        {
            self.refs.insert(id);
        }
    }

    fn visit_call_expression(&mut self, it: &oxc_ast::ast::CallExpression<'a>) {
        if callee_is_direct_eval(&it.callee) {
            self.eval = true;
        }
        walk::walk_call_expression(self, it);
    }
}

/// Every identifier a binding pattern declares — `{ a, b: [c, ...d] }`
/// declares `a`, `c` and `d`.
fn pattern_leaves(pattern: &BindingPattern<'_>, out: &mut impl FnMut(&str)) {
    match pattern {
        BindingPattern::BindingIdentifier(id) => out(&id.name),
        BindingPattern::AssignmentPattern(assignment) => pattern_leaves(&assignment.left, out),
        BindingPattern::ArrayPattern(array) => {
            for element in array.elements.iter().flatten() {
                pattern_leaves(element, out);
            }
            if let Some(rest) = &array.rest {
                pattern_leaves(&rest.argument, out);
            }
        }
        BindingPattern::ObjectPattern(object) => {
            for property in &object.properties {
                pattern_leaves(&property.value, out);
            }
            if let Some(rest) = &object.rest {
                pattern_leaves(&rest.argument, out);
            }
        }
    }
}

/// `true` when a program body references `new.target` outside any
/// non-arrow function. §19.2.1.1 PerformEval step 5 — such a
/// reference is an early SyntaxError unless the eval is a direct
/// eval contained in function code (arrows are transparent: they
/// inherit `new.target` lexically).
#[must_use]
pub fn program_references_new_target(stmts: &[Statement<'_>]) -> bool {
    #[derive(Default)]
    struct NewTargetFinder {
        found: bool,
    }
    impl<'a> Visit<'a> for NewTargetFinder {
        fn visit_meta_property(&mut self, it: &oxc_ast::ast::MetaProperty<'a>) {
            if it.meta.name == "new" && it.property.name == "target" {
                self.found = true;
            }
        }
        fn visit_function(&mut self, _it: &Function<'a>, _flags: oxc_syntax::scope::ScopeFlags) {
            // Non-arrow function bodies own their `new.target`.
        }
    }
    let mut finder = NewTargetFinder::default();
    for stmt in stmts {
        finder.visit_statement(stmt);
    }
    finder.found
}


/// `true` when a derived constructor's `this` must live in a
/// `DerivedThis` context slot: an arrow nested in its parameters or body
/// (through arrows only) reads `this`, calls `super(...)`, or reads a
/// `super` property, or the constructor's own code (arrows included)
/// contains a direct eval that may do any of these.
#[must_use]
pub fn derived_this_observed(
    params: &FormalParameters<'_>,
    body: Option<&FunctionBody<'_>>,
) -> bool {
    let mut finder = DerivedThisFinder::default();
    finder.visit_formal_parameters(params);
    if let Some(body) = body {
        finder.visit_function_body(body);
    }
    finder.found
}

#[derive(Default)]
struct DerivedThisFinder {
    arrow_depth: u32,
    found: bool,
}

impl<'a> Visit<'a> for DerivedThisFinder {
    fn visit_call_expression(&mut self, it: &oxc_ast::ast::CallExpression<'a>) {
        if callee_is_direct_eval(&it.callee) {
            self.found = true;
            return;
        }
        walk::walk_call_expression(self, it);
    }
    fn visit_this_expression(&mut self, _it: &oxc_ast::ast::ThisExpression) {
        if self.arrow_depth > 0 {
            self.found = true;
        }
    }
    fn visit_super(&mut self, _it: &oxc_ast::ast::Super) {
        if self.arrow_depth > 0 {
            self.found = true;
        }
    }
    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        self.arrow_depth += 1;
        walk::walk_arrow_function_expression(self, it);
        self.arrow_depth -= 1;
    }
    // Ordinary functions own `this`.
    fn visit_function(&mut self, _it: &Function<'a>, _flags: oxc_syntax::scope::ScopeFlags) {}
    fn visit_class(&mut self, it: &Class<'a>) {
        // Heritage and computed keys evaluate in the surrounding code;
        // member bodies own their `this`.
        if let Some(super_class) = &it.super_class {
            self.visit_expression(super_class);
        }
        for element in &it.body.body {
            let key = match element {
                oxc_ast::ast::ClassElement::MethodDefinition(m) if m.computed => Some(&m.key),
                oxc_ast::ast::ClassElement::PropertyDefinition(p) if p.computed => Some(&p.key),
                oxc_ast::ast::ClassElement::AccessorProperty(a) if a.computed => Some(&a.key),
                _ => None,
            };
            if let Some(key) = key {
                self.visit_property_key(key);
            }
        }
    }
}


/// `true` when `stmt` contains a function, arrow, class, or direct eval —
/// any construct that can reach a `with` object after the statement's own
/// straight-line code, so the object must live in a context slot.
#[must_use]
pub fn statement_contains_closure_or_eval(stmt: &Statement<'_>) -> bool {
    #[derive(Default)]
    struct Finder {
        found: bool,
    }
    impl<'a> Visit<'a> for Finder {
        fn visit_call_expression(&mut self, it: &oxc_ast::ast::CallExpression<'a>) {
            if callee_is_direct_eval(&it.callee) {
                self.found = true;
                return;
            }
            walk::walk_call_expression(self, it);
        }
        fn visit_function(&mut self, _it: &Function<'a>, _flags: oxc_syntax::scope::ScopeFlags) {
            self.found = true;
        }
        fn visit_arrow_function_expression(&mut self, _it: &ArrowFunctionExpression<'a>) {
            self.found = true;
        }
        fn visit_class(&mut self, _it: &Class<'a>) {
            self.found = true;
        }
    }
    let mut finder = Finder::default();
    finder.visit_statement(stmt);
    finder.found
}


/// Whether `expr` contains a direct eval at any depth — used for
/// class field initializers, which compile into the synthesized
/// constructor's frame.
#[must_use]
pub fn expression_contains_direct_eval(expr: &oxc_ast::ast::Expression<'_>) -> bool {
    let mut finder = DirectEvalFinder::default();
    finder.visit_expression(expr);
    finder.found
}


#[derive(Default)]
struct DirectEvalFinder {
    found: bool,
}

/// §13.3.6.1 — a callee that is the bare `eval` identifier, possibly
/// wrapped in parentheses (`(eval)`, `((eval))`), is a direct eval. A
/// non-trivial parenthesized callee such as `(1, eval)` is indirect.
/// `arguments`, possibly parenthesized.
fn is_arguments(expr: &Expression<'_>) -> bool {
    matches!(expr.without_parentheses(), Expression::Identifier(id) if id.name == "arguments")
}

/// A member access on `arguments`, possibly parenthesized or optional.
fn member_of_arguments(expr: &Expression<'_>) -> bool {
    let expr = expr.without_parentheses();
    let member = match expr {
        Expression::ChainExpression(chain) => chain.expression.as_member_expression(),
        _ => expr.as_member_expression(),
    };
    member.is_some_and(|member| is_arguments(member.object()))
}

fn callee_is_direct_eval(callee: &oxc_ast::ast::Expression<'_>) -> bool {
    match callee {
        oxc_ast::ast::Expression::Identifier(id) => id.name.as_str() == "eval",
        oxc_ast::ast::Expression::ParenthesizedExpression(p) => {
            callee_is_direct_eval(&p.expression)
        }
        _ => false,
    }
}

impl<'a> Visit<'a> for DirectEvalFinder {
    fn visit_call_expression(&mut self, it: &oxc_ast::ast::CallExpression<'a>) {
        if callee_is_direct_eval(&it.callee) {
            self.found = true;
            return;
        }
        walk::walk_call_expression(self, it);
    }
}


/// `true` when the body of one function, including its arrow functions but
/// not its other nested functions, reads a `.arguments` property.
///
/// Like SpiderMonkey, only such a function captures its actual arguments at
/// call time: `fn.arguments` is almost always read inside `fn` itself. Any
/// other running activation answers the accessor from its live parameter
/// registers, so an unrelated `node.arguments` elsewhere in the unit does
/// not pessimize every function.
#[must_use]
pub fn function_body_reads_dot_arguments(
    params: &oxc_ast::ast::FormalParameters<'_>,
    body: &oxc_ast::ast::FunctionBody<'_>,
) -> bool {
    let mut finder = DotArgumentsFinder {
        found: false,
        skip_nested_functions: true,
    };
    finder.visit_formal_parameters(params);
    if !finder.found {
        finder.visit_function_body(body);
    }
    finder.found
}

#[derive(Default)]
struct DotArgumentsFinder {
    found: bool,
    /// Stop at nested non-arrow functions, which own their activations.
    skip_nested_functions: bool,
}

impl<'a> Visit<'a> for DotArgumentsFinder {
    fn visit_function(
        &mut self,
        it: &oxc_ast::ast::Function<'a>,
        flags: oxc_syntax::scope::ScopeFlags,
    ) {
        if !self.skip_nested_functions {
            walk::walk_function(self, it, flags);
        }
    }

    fn visit_class(&mut self, it: &oxc_ast::ast::Class<'a>) {
        if !self.skip_nested_functions {
            walk::walk_class(self, it);
        }
    }

    fn visit_static_member_expression(&mut self, it: &oxc_ast::ast::StaticMemberExpression<'a>) {
        if it.property.name.as_str() == "arguments" {
            self.found = true;
            return;
        }
        walk::walk_static_member_expression(self, it);
    }

    fn visit_computed_member_expression(
        &mut self,
        it: &oxc_ast::ast::ComputedMemberExpression<'a>,
    ) {
        if matches!(
            &it.expression,
            oxc_ast::ast::Expression::StringLiteral(key) if key.value.as_str() == "arguments"
        ) {
            self.found = true;
            return;
        }
        walk::walk_computed_member_expression(self, it);
    }
}


/// Whether `expr` names `name` anywhere, read or written, nested functions
/// included.
#[must_use]
pub fn expression_mentions_name(expr: &oxc_ast::ast::Expression<'_>, name: &str) -> bool {
    struct Finder<'n> {
        name: &'n str,
        found: bool,
    }
    impl<'a> Visit<'a> for Finder<'_> {
        fn visit_identifier_reference(&mut self, it: &oxc_ast::ast::IdentifierReference<'a>) {
            if it.name.as_str() == self.name {
                self.found = true;
            }
        }
    }
    let mut finder = Finder { name, found: false };
    finder.visit_expression(expr);
    finder.found
}

