use std::collections::{BTreeMap, BTreeSet, VecDeque};

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::SourceType as OxcSourceType;

use super::{
    ImportContext, ModuleLoader, ModuleLoaderError, ModuleType, ResolvedModule, SourceType,
};

/// One resolved dependency edge in the hosted module graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleDependency {
    pub specifier: String,
    pub url: String,
    pub context: ImportContext,
}

/// One loaded node in the hosted module graph.
#[derive(Debug, Clone)]
pub struct ModuleGraphNode {
    pub module: ResolvedModule,
    pub dependencies: Vec<ModuleDependency>,
}

/// Resolved hosted module graph rooted at one entry specifier.
#[derive(Debug, Clone)]
pub struct ModuleGraph {
    entry_url: String,
    nodes: BTreeMap<String, ModuleGraphNode>,
}

impl ModuleGraph {
    #[must_use]
    pub fn new(entry_url: String) -> Self {
        Self {
            entry_url,
            nodes: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, node: ModuleGraphNode) {
        self.nodes.insert(node.module.url.clone(), node);
    }

    #[must_use]
    pub fn entry(&self) -> Option<&ModuleGraphNode> {
        self.nodes.get(&self.entry_url)
    }

    #[must_use]
    pub fn nodes(&self) -> &BTreeMap<String, ModuleGraphNode> {
        &self.nodes
    }

    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
}

/// Errors produced while loading or scanning a hosted module graph.
#[derive(Debug, thiserror::Error)]
pub enum ModuleGraphError {
    #[error("{0}")]
    Loader(#[from] ModuleLoaderError),
    #[error("module graph parse failed for '{url}': {message}")]
    Parse { url: String, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DependencyRecord {
    specifier: String,
    context: ImportContext,
}

impl ModuleLoader {
    /// Load the hosted dependency graph rooted at one entry specifier.
    pub fn load_graph(
        &self,
        specifier: &str,
        referrer: Option<&str>,
    ) -> Result<ModuleGraph, ModuleGraphError> {
        let entry = self.load(specifier, referrer)?;
        let entry_url = entry.url.clone();
        let mut graph = ModuleGraph::new(entry_url);
        let mut seen = BTreeSet::new();
        let mut pending = VecDeque::from([entry]);

        while let Some(module) = pending.pop_front() {
            if !seen.insert(module.url.clone()) {
                continue;
            }

            let records = scan_dependency_records(&module)?;
            let mut dependencies = Vec::with_capacity(records.len());

            for record in records {
                let dep =
                    self.load_with_context(&record.specifier, Some(&module.url), record.context)?;
                dependencies.push(ModuleDependency {
                    specifier: record.specifier,
                    url: dep.url.clone(),
                    context: record.context,
                });

                if !seen.contains(&dep.url) {
                    pending.push_back(dep);
                }
            }

            graph.insert(ModuleGraphNode {
                module,
                dependencies,
            });
        }

        Ok(graph)
    }
}

fn scan_dependency_records(
    module: &ResolvedModule,
) -> Result<Vec<DependencyRecord>, ModuleGraphError> {
    if module.source.is_empty() {
        return Ok(Vec::new());
    }

    if matches!(module.source_type, SourceType::Json) {
        return Ok(Vec::new());
    }

    let allocator = Allocator::default();
    let source_type = source_type_for_module(module);
    let parsed = Parser::new(&allocator, &module.source, source_type).parse();

    if parsed.panicked {
        return Err(ModuleGraphError::Parse {
            url: module.url.clone(),
            message: "parser panicked while scanning dependencies".to_string(),
        });
    }

    if let Some(error) = parsed.errors.first() {
        return Err(ModuleGraphError::Parse {
            url: module.url.clone(),
            message: error.to_string(),
        });
    }

    let mut deps = Vec::new();

    for stmt in &parsed.program.body {
        match stmt {
            Statement::ImportDeclaration(decl) => {
                push_unique(
                    &mut deps,
                    DependencyRecord {
                        specifier: decl.source.value.as_str().to_string(),
                        context: ImportContext::Esm,
                    },
                );
            }
            Statement::ExportNamedDeclaration(decl) => {
                if let Some(source) = &decl.source {
                    push_unique(
                        &mut deps,
                        DependencyRecord {
                            specifier: source.value.as_str().to_string(),
                            context: ImportContext::Esm,
                        },
                    );
                }
            }
            Statement::ExportAllDeclaration(decl) => {
                push_unique(
                    &mut deps,
                    DependencyRecord {
                        specifier: decl.source.value.as_str().to_string(),
                        context: ImportContext::Esm,
                    },
                );
            }
            _ => {}
        }
    }

    scan_stmts(&parsed.program.body, &mut deps);
    Ok(deps)
}

fn source_type_for_module(module: &ResolvedModule) -> OxcSourceType {
    let path_hint = module.url.strip_prefix("file://").unwrap_or(&module.url);
    let mut source_type = OxcSourceType::from_path(path_hint).unwrap_or_default();

    if matches!(module.source_type, SourceType::TypeScript) {
        source_type = source_type.with_typescript(true);
    }

    if path_hint.ends_with(".tsx") || path_hint.ends_with(".jsx") {
        source_type = source_type.with_jsx(true);
    }

    match module.module_type {
        ModuleType::Esm => source_type.with_module(true),
        ModuleType::CommonJs => source_type.with_script(true),
    }
}

fn push_unique(deps: &mut Vec<DependencyRecord>, record: DependencyRecord) {
    if !deps.iter().any(|dep| dep == &record) {
        deps.push(record);
    }
}

fn scan_stmts(stmts: &[Statement<'_>], deps: &mut Vec<DependencyRecord>) {
    for stmt in stmts {
        scan_stmt(stmt, deps);
    }
}

fn scan_stmt(stmt: &Statement<'_>, deps: &mut Vec<DependencyRecord>) {
    match stmt {
        Statement::ExpressionStatement(expr_stmt) => scan_expr(&expr_stmt.expression, deps),
        Statement::VariableDeclaration(decl) => {
            for declarator in &decl.declarations {
                if let Some(init) = &declarator.init {
                    scan_expr(init, deps);
                }
            }
        }
        Statement::ReturnStatement(ret) => {
            if let Some(arg) = &ret.argument {
                scan_expr(arg, deps);
            }
        }
        Statement::IfStatement(if_stmt) => {
            scan_expr(&if_stmt.test, deps);
            scan_stmt(&if_stmt.consequent, deps);
            if let Some(alt) = &if_stmt.alternate {
                scan_stmt(alt, deps);
            }
        }
        Statement::BlockStatement(block) => scan_stmts(&block.body, deps),
        Statement::ForStatement(for_stmt) => {
            if let Some(ForStatementInit::VariableDeclaration(decl)) = &for_stmt.init {
                for declarator in &decl.declarations {
                    if let Some(init) = &declarator.init {
                        scan_expr(init, deps);
                    }
                }
            }
            scan_stmt(&for_stmt.body, deps);
        }
        Statement::ForInStatement(for_in) => {
            scan_expr(&for_in.right, deps);
            scan_stmt(&for_in.body, deps);
        }
        Statement::ForOfStatement(for_of) => {
            scan_expr(&for_of.right, deps);
            scan_stmt(&for_of.body, deps);
        }
        Statement::WhileStatement(while_stmt) => {
            scan_expr(&while_stmt.test, deps);
            scan_stmt(&while_stmt.body, deps);
        }
        Statement::DoWhileStatement(do_while) => {
            scan_stmt(&do_while.body, deps);
            scan_expr(&do_while.test, deps);
        }
        Statement::TryStatement(try_stmt) => {
            scan_stmts(&try_stmt.block.body, deps);
            if let Some(handler) = &try_stmt.handler {
                scan_stmts(&handler.body.body, deps);
            }
            if let Some(finalizer) = &try_stmt.finalizer {
                scan_stmts(&finalizer.body, deps);
            }
        }
        Statement::SwitchStatement(switch) => {
            scan_expr(&switch.discriminant, deps);
            for case in &switch.cases {
                scan_stmts(&case.consequent, deps);
            }
        }
        Statement::ExportDefaultDeclaration(decl) => match &decl.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
                if let Some(body) = &func.body {
                    scan_stmts(&body.statements, deps);
                }
            }
            ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                for elem in &class.body.body {
                    scan_class_element(elem, deps);
                }
            }
            _ => {
                if let Some(expr) = decl.declaration.as_expression() {
                    scan_expr(expr, deps);
                }
            }
        },
        Statement::FunctionDeclaration(func) => {
            if let Some(body) = &func.body {
                scan_stmts(&body.statements, deps);
            }
        }
        Statement::ClassDeclaration(class) => {
            for elem in &class.body.body {
                scan_class_element(elem, deps);
            }
        }
        _ => {}
    }
}

fn scan_expr(expr: &Expression<'_>, deps: &mut Vec<DependencyRecord>) {
    match expr {
        Expression::ImportExpression(import_expr) => {
            if let Expression::StringLiteral(lit) = &import_expr.source {
                push_unique(
                    deps,
                    DependencyRecord {
                        specifier: lit.value.as_str().to_string(),
                        context: ImportContext::Esm,
                    },
                );
            }
            scan_expr(&import_expr.source, deps);
        }
        Expression::CallExpression(call) => {
            if is_require_call(call)
                && let Some(Argument::StringLiteral(lit)) = call.arguments.first()
            {
                push_unique(
                    deps,
                    DependencyRecord {
                        specifier: lit.value.as_str().to_string(),
                        context: ImportContext::Cjs,
                    },
                );
            }
            scan_expr(&call.callee, deps);
            for arg in &call.arguments {
                scan_argument(arg, deps);
            }
        }
        Expression::AwaitExpression(await_expr) => scan_expr(&await_expr.argument, deps),
        Expression::AssignmentExpression(assign) => scan_expr(&assign.right, deps),
        Expression::SequenceExpression(seq) => {
            for expr in &seq.expressions {
                scan_expr(expr, deps);
            }
        }
        Expression::ConditionalExpression(cond) => {
            scan_expr(&cond.test, deps);
            scan_expr(&cond.consequent, deps);
            scan_expr(&cond.alternate, deps);
        }
        Expression::LogicalExpression(logical) => {
            scan_expr(&logical.left, deps);
            scan_expr(&logical.right, deps);
        }
        Expression::BinaryExpression(binary) => {
            scan_expr(&binary.left, deps);
            scan_expr(&binary.right, deps);
        }
        Expression::StaticMemberExpression(member) => scan_expr(&member.object, deps),
        Expression::ComputedMemberExpression(member) => {
            scan_expr(&member.object, deps);
            scan_expr(&member.expression, deps);
        }
        Expression::PrivateFieldExpression(member) => scan_expr(&member.object, deps),
        Expression::ArrowFunctionExpression(arrow) => scan_stmts(&arrow.body.statements, deps),
        Expression::FunctionExpression(func) => {
            if let Some(body) = &func.body {
                scan_stmts(&body.statements, deps);
            }
        }
        Expression::TemplateLiteral(template) => {
            for expr in &template.expressions {
                scan_expr(expr, deps);
            }
        }
        Expression::TaggedTemplateExpression(tagged) => {
            scan_expr(&tagged.tag, deps);
            for expr in &tagged.quasi.expressions {
                scan_expr(expr, deps);
            }
        }
        Expression::ArrayExpression(array) => {
            for elem in &array.elements {
                match elem {
                    ArrayExpressionElement::SpreadElement(spread) => {
                        scan_expr(&spread.argument, deps)
                    }
                    ArrayExpressionElement::Elision(_) => {}
                    _ => {
                        if let Some(expr) = elem.as_expression() {
                            scan_expr(expr, deps);
                        }
                    }
                }
            }
        }
        Expression::ObjectExpression(object) => {
            for prop in &object.properties {
                match prop {
                    ObjectPropertyKind::ObjectProperty(prop) => scan_expr(&prop.value, deps),
                    ObjectPropertyKind::SpreadProperty(spread) => scan_expr(&spread.argument, deps),
                }
            }
        }
        Expression::ParenthesizedExpression(paren) => scan_expr(&paren.expression, deps),
        Expression::UnaryExpression(unary) => scan_expr(&unary.argument, deps),
        Expression::YieldExpression(yield_expr) => {
            if let Some(arg) = &yield_expr.argument {
                scan_expr(arg, deps);
            }
        }
        Expression::ClassExpression(class) => {
            for elem in &class.body.body {
                scan_class_element(elem, deps);
            }
        }
        Expression::NewExpression(new_expr) => {
            scan_expr(&new_expr.callee, deps);
            for arg in &new_expr.arguments {
                scan_argument(arg, deps);
            }
        }
        Expression::ChainExpression(chain) => match &chain.expression {
            ChainElement::CallExpression(call) => {
                scan_expr(&call.callee, deps);
                for arg in &call.arguments {
                    scan_argument(arg, deps);
                }
            }
            ChainElement::StaticMemberExpression(member) => scan_expr(&member.object, deps),
            ChainElement::ComputedMemberExpression(member) => {
                scan_expr(&member.object, deps);
                scan_expr(&member.expression, deps);
            }
            ChainElement::PrivateFieldExpression(member) => scan_expr(&member.object, deps),
            _ => {}
        },
        _ => {}
    }
}

fn scan_argument(arg: &Argument<'_>, deps: &mut Vec<DependencyRecord>) {
    match arg {
        Argument::SpreadElement(spread) => scan_expr(&spread.argument, deps),
        _ => {
            if let Some(expr) = arg.as_expression() {
                scan_expr(expr, deps);
            }
        }
    }
}

fn scan_class_element(elem: &ClassElement<'_>, deps: &mut Vec<DependencyRecord>) {
    match elem {
        ClassElement::MethodDefinition(method) => {
            if let Some(body) = &method.value.body {
                scan_stmts(&body.statements, deps);
            }
        }
        ClassElement::PropertyDefinition(prop) => {
            if let Some(value) = &prop.value {
                scan_expr(value, deps);
            }
        }
        ClassElement::StaticBlock(block) => scan_stmts(&block.body, deps),
        _ => {}
    }
}

fn is_require_call(call: &CallExpression<'_>) -> bool {
    if call.arguments.len() != 1 {
        return false;
    }
    matches!(&call.callee, Expression::Identifier(id) if id.name == "require")
}
