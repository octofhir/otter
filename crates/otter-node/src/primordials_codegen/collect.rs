//! Binding-checked primordial uses in the current compiled Node sources.
//!
//! # Contents
//! - Rust macro AST census of every current nodelib source producer.
//! - Read-only ambient primordial binding and exact original use spans.
//! - The typed-array reconstruction's sole finite computed-name proof.
//!
//! # Invariants
//! Shadows, raw aliases, reassignments, escaping references and unresolved
//! computed reads are errors. The visitor never treats matching identifier
//! spelling as sufficient proof of a primordial use.
//!
//! # See also
//! - `crate::nodelib` owns the current compile-time source declarations.
//! - `internal/util/inspect.js` owns the one dynamic typed-array consumer.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_span::{ContentEq, GetSpan, Span};
use syn::{
    Token,
    parse::{Parse, ParseStream},
};

use super::{Coverage, Result, Site, parse, reject};

struct Declaration {
    path: String,
}

impl Parse for Declaration {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let _: syn::Ident = input.parse()?;
        let _: Token![,] = input.parse()?;
        let _: syn::LitStr = input.parse()?;
        let _: Token![,] = input.parse()?;
        let kind: syn::Ident = input.parse()?;
        if kind != "compat" && kind != "vendored" {
            return Err(input.error("unrecognized nodelib source declaration"));
        }
        let path: syn::LitStr = input.parse()?;
        if !input.is_empty() {
            return Err(input.error("nodelib declaration has unexpected trailing tokens"));
        }
        Ok(Self { path: path.value() })
    }
}

pub(super) fn source_paths(repo: &Path) -> Result<(String, Vec<PathBuf>)> {
    let owner = repo.join("crates/otter-node/src/nodelib.rs");
    let source = std::fs::read_to_string(owner)?;
    let ast = syn::parse_file(&source)?;
    let base = repo.join("crates/otter-node/src");
    let mut paths = BTreeSet::new();
    for item in ast.items {
        if let syn::Item::Macro(item) = item
            && item.mac.path.is_ident("nodelib_module")
        {
            let declaration: Declaration = syn::parse2(item.mac.tokens)?;
            let relative = Path::new(&declaration.path);
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|p| matches!(p, std::path::Component::ParentDir))
            {
                return Err("nodelib producer path escapes its current source owner".into());
            }
            paths.insert(base.join(relative));
        }
    }
    if paths.is_empty() {
        return Err("missing current nodelib source declarations".into());
    }
    Ok((source, paths.into_iter().collect()))
}

const PREFIX: &str =
    "function __otterPrimordialInput__(exports,require,module,__filename,__dirname) {\n";

pub(super) fn inspect(
    path: &str,
    source: &str,
    typed_arrays: &[String],
    coverage: &mut Coverage,
) -> Result<()> {
    // CommonJS top-level returns are parsed inside an analysis-only function.
    // No wrapper is linked, evaluated or used as an engine source context.
    let input = format!("{PREFIX}{source}\n}}\n");
    parse(&input, |program| {
        let mut visitor = Uses {
            path,
            offset: PREFIX.len() as u32,
            coverage,
            typed_arrays,
            allowed: BTreeSet::new(),
            references: Vec::new(),
            errors: Vec::new(),
            function: None,
            function_depth: 0,
            block_depth: 0,
            typed_guard: false,
            dynamic_proved: false,
            helper_bindings: BTreeMap::new(),
            helper_imports: BTreeMap::new(),
            helper_writes: Vec::new(),
            dynamic_sites: Vec::new(),
            format_raw_count: 0,
        };
        visitor.visit_program(program);
        for span in std::mem::take(&mut visitor.references) {
            if !visitor.allowed.contains(&(span.start, span.end)) {
                visitor.fail(span, "primordial binding escapes, aliases or is reassigned");
            }
        }
        visitor.validate_dynamic_bindings();
        if let Some(error) = visitor.errors.into_iter().next() {
            return Err(error);
        }
        Ok(())
    })
}

struct Uses<'p> {
    path: &'p str,
    offset: u32,
    coverage: &'p mut Coverage,
    typed_arrays: &'p [String],
    allowed: BTreeSet<(u32, u32)>,
    references: Vec<Span>,
    errors: Vec<Box<dyn std::error::Error>>,
    function: Option<String>,
    function_depth: usize,
    block_depth: usize,
    typed_guard: bool,
    dynamic_proved: bool,
    helper_bindings: BTreeMap<String, Vec<Span>>,
    helper_imports: BTreeMap<String, Vec<Span>>,
    helper_writes: Vec<Span>,
    dynamic_sites: Vec<Span>,
    format_raw_count: usize,
}

const DYNAMIC_BINDINGS: [&str; 3] = [
    "isTypedArray",
    "TypedArrayPrototypeGetSymbolToStringTag",
    "require",
];

fn is_primordial(expression: &Expression<'_>) -> Option<Span> {
    match expression {
        Expression::Identifier(identifier) if identifier.name == "primordials" => {
            Some(identifier.span)
        }
        _ => None,
    }
}

fn literal_key(key: &PropertyKey<'_>, computed: bool) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(identifier) if !computed => Some(identifier.name.to_string()),
        PropertyKey::StringLiteral(literal) => Some(literal.value.to_string()),
        _ => None,
    }
}

impl Uses<'_> {
    fn original(&self, span: Span) -> Span {
        // All observed uses are in the original body, never the parser wrapper.
        Span::new(span.start - self.offset, span.end - self.offset)
    }

    fn fail(&mut self, span: Span, reason: &str) {
        self.errors
            .push(reject(self.path, self.original(span), reason));
    }

    fn note(&mut self, name: &str, span: Span, kind: &'static str) {
        let span = self.original(span);
        self.coverage.note(
            name,
            Site {
                path: self.path.to_owned(),
                start: span.start,
                end: span.end,
                kind,
            },
        );
    }

    fn allow(&mut self, span: Span) {
        self.allowed.insert((span.start, span.end));
    }

    fn validate_dynamic_bindings(&mut self) {
        if self.dynamic_sites.is_empty() {
            return;
        }
        for name in DYNAMIC_BINDINGS {
            let bindings = self
                .helper_bindings
                .get(name)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let imports = self
                .helper_imports
                .get(name)
                .map(Vec::as_slice)
                .unwrap_or_default();
            // `require` is the wrapper's admitted parameter; it cannot acquire
            // a second source binding. The two helpers are actual module consts.
            let valid = if name == "require" {
                bindings.is_empty()
            } else {
                bindings.len() == 1 && bindings == imports
            };
            if !valid {
                self.fail(
                    self.dynamic_sites[0],
                    &format!("dynamic read has no unique readonly imported {name} binding"),
                );
            }
        }
        for span in std::mem::take(&mut self.helper_writes) {
            self.fail(span, "dynamic proof dependency is reassigned");
        }
        if self.format_raw_count != 1 {
            self.fail(
                self.dynamic_sites[0],
                "dynamic read has no unique formatRaw function owner",
            );
        }
    }

    fn record_imports(&mut self, declaration: &VariableDeclarator<'_>) {
        if self.function_depth != 1
            || self.block_depth != 0
            || declaration.kind != VariableDeclarationKind::Const
        {
            return;
        }
        let BindingPattern::ObjectPattern(pattern) = &declaration.id else {
            return;
        };
        for property in &pattern.properties {
            let BindingPattern::BindingIdentifier(identifier) = &property.value else {
                continue;
            };
            let name = identifier.name.as_str();
            if literal_key(&property.key, property.computed).as_deref() != Some(name) {
                continue;
            }
            let source = match (name, &declaration.init) {
                ("TypedArrayPrototypeGetSymbolToStringTag", Some(expression)) => {
                    is_primordial(expression).is_some()
                }
                ("isTypedArray", Some(Expression::CallExpression(call))) => {
                    matches!(&call.callee, Expression::Identifier(id) if id.name == "require")
                        && call.arguments.len() == 1
                        && matches!(&call.arguments[0], Argument::StringLiteral(literal) if literal.value == "internal/util/types")
                }
                _ => false,
            };
            if source {
                self.helper_imports
                    .entry(name.to_owned())
                    .or_default()
                    .push(identifier.span);
            }
        }
    }
}

/// Prove the whole dynamic branch, not only a matching index identifier.
fn reconstruction(block: &BlockStatement<'_>) -> Result<Option<Span>> {
    let expected = "keys = getOwnNonIndexProperties(value, filter); let bound = value; let fallback = ''; if (constructor === null) { fallback = TypedArrayPrototypeGetSymbolToStringTag(value); bound = new primordials[fallback](value); }";
    parse(expected, |template| {
        // The branch begins with this exact prefix. An earlier assignment or
        // lexical shadow cannot separate the type guard from reconstruction.
        if block.body.len() >= template.body.len()
            && block
                .body
                .iter()
                .zip(&template.body)
                .all(|(a, b)| a.content_eq(b))
        {
            return Ok(Some(block.body[3].span()));
        }
        Ok(None)
    })
}

fn typed_condition(expression: &Expression<'_>) -> bool {
    let Expression::CallExpression(call) = expression else {
        return false;
    };
    matches!(&call.callee, Expression::Identifier(id) if id.name == "isTypedArray")
        && call.arguments.len() == 1
        && matches!(&call.arguments[0], Argument::Identifier(id) if id.name == "value")
}

impl<'a> Visit<'a> for Uses<'_> {
    fn visit_binding_identifier(&mut self, identifier: &BindingIdentifier<'a>) {
        if identifier.name == "primordials" {
            self.fail(
                identifier.span,
                "local binding shadows the injected readonly primordial owner",
            );
        }
        if identifier.span.start >= self.offset
            && DYNAMIC_BINDINGS.contains(&identifier.name.as_str())
        {
            self.helper_bindings
                .entry(identifier.name.to_string())
                .or_default()
                .push(identifier.span);
        }
    }

    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
        if identifier.name == "primordials" {
            self.references.push(identifier.span);
        }
    }

    fn visit_variable_declarator(&mut self, declaration: &VariableDeclarator<'a>) {
        self.record_imports(declaration);
        if let Some(span) = declaration.init.as_ref().and_then(is_primordial) {
            if let BindingPattern::ObjectPattern(pattern) = &declaration.id {
                self.allow(span);
                if let Some(rest) = &pattern.rest {
                    self.fail(
                        rest.span,
                        "primordial object-rest is an unproved escaping use",
                    );
                }
                for property in &pattern.properties {
                    if let Some(name) = literal_key(&property.key, property.computed) {
                        self.note(&name, property.key.span(), "destructure");
                    } else {
                        self.fail(
                            property.key.span(),
                            "unproved computed primordial destructuring key",
                        );
                    }
                }
            }
        }
        walk::walk_variable_declarator(self, declaration);
    }

    fn visit_static_member_expression(&mut self, member: &StaticMemberExpression<'a>) {
        if let Some(span) = is_primordial(&member.object) {
            self.allow(span);
            self.note(member.property.name.as_str(), member.span, "static-member");
        }
        walk::walk_static_member_expression(self, member);
    }

    fn visit_computed_member_expression(&mut self, member: &ComputedMemberExpression<'a>) {
        if let Some(span) = is_primordial(&member.object) {
            self.allow(span);
            match &member.expression {
                Expression::StringLiteral(literal) => {
                    self.note(literal.value.as_str(), member.span, "literal-member")
                }
                Expression::Identifier(identifier)
                    if identifier.name == "fallback" && self.dynamic_proved =>
                {
                    self.dynamic_sites.push(member.span);
                    let site = self.original(member.span);
                    self.coverage.dynamic.push(Site {
                        path: self.path.to_owned(),
                        start: site.start,
                        end: site.end,
                        kind: "typed-array-tag",
                    });
                    for name in self.typed_arrays.to_vec() {
                        self.note(&name, member.span, "typed-array-tag-domain");
                    }
                }
                _ => self.fail(
                    member.expression.span(),
                    "computed primordial name has no finite AST proof",
                ),
            }
        }
        walk::walk_computed_member_expression(self, member);
    }

    fn visit_function(&mut self, function: &Function<'a>, flags: oxc_syntax::scope::ScopeFlags) {
        let previous = self.function.take();
        let typed_guard = self.typed_guard;
        let dynamic_proved = self.dynamic_proved;
        self.function = function.id.as_ref().map(|id| id.name.to_string());
        self.function_depth += 1;
        if self.function_depth == 2 && self.function.as_deref() == Some("formatRaw") {
            self.format_raw_count += 1;
        }
        self.typed_guard = false;
        self.dynamic_proved = false;
        walk::walk_function(self, function, flags);
        self.function_depth -= 1;
        self.function = previous;
        self.typed_guard = typed_guard;
        self.dynamic_proved = dynamic_proved;
    }

    fn visit_arrow_function_expression(&mut self, function: &ArrowFunctionExpression<'a>) {
        let previous = self.function.take();
        let typed_guard = self.typed_guard;
        let dynamic_proved = self.dynamic_proved;
        self.function_depth += 1;
        self.typed_guard = false;
        self.dynamic_proved = false;
        walk::walk_arrow_function_expression(self, function);
        self.function_depth -= 1;
        self.function = previous;
        self.typed_guard = typed_guard;
        self.dynamic_proved = dynamic_proved;
    }

    fn visit_simple_assignment_target(&mut self, target: &SimpleAssignmentTarget<'a>) {
        if let SimpleAssignmentTarget::AssignmentTargetIdentifier(identifier) = target {
            if DYNAMIC_BINDINGS.contains(&identifier.name.as_str()) {
                self.helper_writes.push(identifier.span);
            }
        }
        walk::walk_simple_assignment_target(self, target);
    }

    fn visit_assignment_target_property_identifier(
        &mut self,
        property: &AssignmentTargetPropertyIdentifier<'a>,
    ) {
        if DYNAMIC_BINDINGS.contains(&property.binding.name.as_str()) {
            self.helper_writes.push(property.binding.span);
        }
        walk::walk_assignment_target_property_identifier(self, property);
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if matches!(&call.callee, Expression::Identifier(id) if id.name == "eval") {
            self.fail(call.span, "direct eval prevents a closed binding proof");
        }
        walk::walk_call_expression(self, call);
    }

    fn visit_if_statement(&mut self, statement: &IfStatement<'a>) {
        self.visit_expression(&statement.test);
        let previous = self.typed_guard;
        let proved = self.dynamic_proved;
        self.typed_guard |= typed_condition(&statement.test);
        if self.path == "internal/util/inspect.js"
            && self.function_depth == 2
            && self.function.as_deref() == Some("formatRaw")
            && self.typed_guard
        {
            if let Statement::BlockStatement(block) = &statement.consequent {
                match reconstruction(block) {
                    Ok(Some(branch)) => {
                        for child in &block.body {
                            self.dynamic_proved = child.span() == branch;
                            self.visit_statement(child);
                        }
                        self.typed_guard = previous;
                        self.dynamic_proved = proved;
                        if let Some(alternate) = &statement.alternate {
                            self.visit_statement(alternate);
                        }
                        return;
                    }
                    Ok(None) => {}
                    Err(error) => self.errors.push(error),
                }
            }
        }
        self.visit_statement(&statement.consequent);
        self.typed_guard = previous;
        self.dynamic_proved = proved;
        if let Some(alternate) = &statement.alternate {
            self.visit_statement(alternate);
        }
    }

    fn visit_with_statement(&mut self, statement: &WithStatement<'a>) {
        self.fail(
            statement.span,
            "with prevents binding-aware primordial proof",
        );
        walk::walk_with_statement(self, statement);
    }

    fn visit_block_statement(&mut self, statement: &BlockStatement<'a>) {
        self.block_depth += 1;
        walk::walk_block_statement(self, statement);
        self.block_depth -= 1;
    }
}
