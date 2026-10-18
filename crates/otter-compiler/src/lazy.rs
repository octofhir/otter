//! Body compile of a lazily compiled function.
//!
//! A script or module compiled with [`CompileOptions::lazy_functions`]
//! lowers each function that does not run as soon as it is created to a
//! [`otter_bytecode::LazyFunction`] record: the call-visible shape plus a
//! placeholder body. The VM compiles the real body on the function's first
//! call through [`compile_lazy_function`]: it parses the function's source
//! range again inside a small wrapper that restores the syntactic context
//! (method, accessor, arrow inside an async function, strict code), and
//! lowers it against the closure's context chain the way a direct eval body
//! is lowered against its caller's.
//!
//! # Contents
//! - [`CompileOptions`] — per-unit compile policy.
//! - [`LazyCompileRequest`] / [`compile_lazy_function`] — the body compile.
//! - `lazy_environment_from` — the module top-level facts a body compile
//!   needs and the context chain does not carry.
//!
//! # Invariants
//! - The produced module's function 0 is the requested function; its
//!   nested functions follow and may be lazy again.
//! - Every span in the produced module is a byte offset into the original
//!   module source, as if the function had been compiled with its unit.
//! - The produced module carries no function source: [[SourceText]] ranges
//!   index the source the requesting chunk already holds.
//! - Outer bindings resolve only through the chain: capture analysis of the
//!   enclosing unit already moved every binding the body reaches into a
//!   context slot.
//!
//! # See also
//! - `functions` for the lazy record (`push_lazy_function`).
//! - `entry::inherit_eval_chain_state` for the chain-derived state.

use crate::compiler::LazyUnit;
use crate::*;
use otter_bytecode::{
    EvalCallerChain, LazyEnvironment, LazyForm, LazyFunction, LazyImport, LazyMethodKind,
    ModuleResolution,
};

/// Per-unit compile policy.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompileOptions {
    /// Functions that do not run as soon as they are created compile their
    /// bodies on first call.
    pub lazy_functions: bool,
}

/// Everything the body compile of one lazily compiled function needs.
#[derive(Debug, Clone, Copy)]
pub struct LazyCompileRequest<'a> {
    /// The whole source text of the module the function belongs to.
    pub source: &'a str,
    /// URL of that module.
    pub module_url: &'a str,
    /// The function's lazy record.
    pub function: &'a Function,
    /// The closure's context chain, innermost first.
    pub chain: &'a EvalCallerChain,
    /// The top-level facts of the function's module, when it has any.
    pub environment: Option<&'a LazyEnvironment>,
    /// Import resolutions of the function's module.
    pub resolutions: &'a [ModuleResolution],
}

/// Compile the body of a lazily compiled function.
///
/// # Errors
/// Returns [`CompileError`] when the record names no lazy body, its range
/// does not slice the source, or the body fails to lower.
pub fn compile_lazy_function(
    request: LazyCompileRequest<'_>,
) -> Result<BytecodeModule, CompileError> {
    let function = request.function;
    let lazy = function.lazy.ok_or_else(|| CompileError::Unsupported {
        node: "lazy compile of a function that has a body".to_string(),
        span: function.span,
    })?;
    let (start, end) = lazy.range;
    let text = request
        .source
        .get(start as usize..end as usize)
        .ok_or_else(|| CompileError::Unsupported {
            node: "lazy function range outside its source".to_string(),
            span: lazy.range,
        })?;
    let (prefix, suffix) = wrapper(function, &lazy);
    let mut wrapped = String::with_capacity(prefix.len() + text.len() + suffix.len());
    wrapped.push_str(&prefix);
    wrapped.push_str(text);
    wrapped.push_str(suffix);
    // Functions inside whose facts the record carries are left unparsed.
    let delta =
        i64::from(u32::try_from(prefix.len()).expect("wrapper prefix is short")) - i64::from(start);
    let skips = function.preparse.as_deref().map(|preparse| {
        let (table, blanks) = crate::preparse::SkipTable::new(preparse, delta);
        crate::preparse::blank(&mut wrapped, &blanks);
        table
    });
    let kind = match (lazy.typescript, lazy.jsx) {
        (false, false) => SyntaxSourceKind::JavaScript,
        (false, true) => SyntaxSourceKind::JavaScriptJsx,
        (true, false) => SyntaxSourceKind::TypeScript,
        (true, true) => SyntaxSourceKind::TypeScriptJsx,
    };
    let goal = if lazy.module_goal {
        otter_syntax::SourceGoal::Module
    } else {
        otter_syntax::SourceGoal::Script
    };
    // Wrapper-local offset `o` maps to source offset `o + shift`; every
    // node the body compile reads lies inside the function's own text.
    let prefix_len = u32::try_from(prefix.len()).expect("wrapper prefix is short");
    let shift = Shift {
        from: prefix_len,
        to: start,
    };
    let mut module = otter_syntax::with_program_goal(&wrapped, kind, goal, |program| {
        lower(program, &request, lazy, kind, skips)
    })
    .map_err(CompileError::from)??;
    shift.module(&mut module);
    let first = module
        .functions
        .first_mut()
        .expect("the requested function is function 0");
    first.name.clone_from(&function.name);
    first.span = function.span;
    first.source_text_range = function.source_text_range;
    // Nested functions report the text their spans cover, sliced from the
    // requesting chunk's source.
    for nested in module.functions.iter_mut().skip(1) {
        let range = match nested.source_text_span {
            Some(range) => range,
            None if nested.is_method => continue,
            None => nested.span,
        };
        if range.0 < range.1
            && request
                .source
                .get(range.0 as usize..range.1 as usize)
                .is_some()
        {
            nested.source_text_range = Some(range);
        }
    }
    let observed: Vec<bool> = module
        .functions
        .iter()
        .map(|function| function.body_observes_this(&module.constants, &module.functions, 0))
        .collect();
    for (function, observed) in module.functions.iter_mut().zip(observed) {
        function.ignores_this = !observed;
    }
    if let Some(environment) = request.environment {
        module.lazy_environments = vec![environment.clone()];
    }
    Ok(module)
}

/// Source offset translation from the wrapper back to the module source.
#[derive(Debug, Clone, Copy)]
struct Shift {
    from: u32,
    to: u32,
}

impl Shift {
    fn offset(self, offset: u32) -> u32 {
        offset.saturating_sub(self.from).saturating_add(self.to)
    }

    fn span(self, span: (u32, u32)) -> (u32, u32) {
        (self.offset(span.0), self.offset(span.1))
    }

    fn module(self, module: &mut BytecodeModule) {
        for function in &mut module.functions {
            function.span = self.span(function.span);
            function.source_text_span = function.source_text_span.map(|span| self.span(span));
            if let Some(lazy) = &mut function.lazy {
                lazy.range = self.span(lazy.range);
            }
            if let Some(preparse) = &mut function.preparse {
                crate::preparse::shift_all(
                    &mut preparse.skipped,
                    i64::from(self.to) - i64::from(self.from),
                );
            }
            function.spans = function
                .spans
                .iter()
                .map(|entry| SpanEntry {
                    pc: entry.pc,
                    span: self.span(entry.span),
                })
                .collect();
        }
    }
}

/// Text around the function's range that parses it back into the
/// syntactic context it was written in.
fn wrapper(function: &Function, lazy: &LazyFunction) -> (String, &'static str) {
    let mut prefix = String::new();
    // Class bodies and module code are strict without saying so.
    if lazy.outer_strict && !lazy.module_goal && !matches!(lazy.form, LazyForm::ClassMethod(_)) {
        prefix.push_str("\"use strict\";");
    }
    let method_head = |kind: LazyMethodKind| match kind {
        LazyMethodKind::Getter => "get m",
        LazyMethodKind::Setter => "set m",
        LazyMethodKind::Method => match (function.is_async, function.is_generator) {
            (true, true) => "async *m",
            (true, false) => "async m",
            (false, true) => "*m",
            (false, false) => "m",
        },
    };
    let suffix = match lazy.form {
        LazyForm::Function => {
            prefix.push('(');
            ")"
        }
        LazyForm::FunctionDeclaration => "",
        LazyForm::Arrow {
            enclosing_async,
            enclosing_generator,
            ..
        } => {
            // A function around the arrow gives `new.target` a meaning and
            // `await` / `yield` the one they had where the arrow was written.
            prefix.push_str(match (enclosing_async, enclosing_generator) {
                (true, true) => "(async function*(){(",
                (true, false) => "(async function(){(",
                (false, true) => "(function*(){(",
                (false, false) => "(function(){(",
            });
            ")})"
        }
        LazyForm::ObjectMethod(kind) => {
            prefix.push_str("({");
            prefix.push_str(method_head(kind));
            "})"
        }
        LazyForm::ClassMethod(kind) => {
            prefix.push_str("(class{");
            prefix.push_str(method_head(kind));
            "})"
        }
    };
    (prefix, suffix)
}

/// The function node the wrapper parsed back.
enum Target<'b, 'a> {
    Function(&'b oxc_ast::ast::Function<'a>),
    Arrow(&'b oxc_ast::ast::ArrowFunctionExpression<'a>),
}

fn parenthesized<'b, 'a>(expression: &'b Expression<'a>) -> &'b Expression<'a> {
    match expression {
        Expression::ParenthesizedExpression(inner) => parenthesized(&inner.expression),
        other => other,
    }
}

fn find_target<'b, 'a>(program: &'b Program<'a>, form: LazyForm) -> Option<Target<'b, 'a>> {
    if form == LazyForm::FunctionDeclaration {
        return match program.body.first() {
            Some(Statement::FunctionDeclaration(function)) => Some(Target::Function(function)),
            _ => None,
        };
    }
    let Some(Statement::ExpressionStatement(statement)) = program.body.first() else {
        return None;
    };
    let outer = parenthesized(&statement.expression);
    match form {
        LazyForm::Function | LazyForm::FunctionDeclaration => match outer {
            Expression::FunctionExpression(function) => Some(Target::Function(function)),
            _ => None,
        },
        LazyForm::Arrow { .. } => {
            let Expression::FunctionExpression(wrapper) = outer else {
                return None;
            };
            let Some(Statement::ExpressionStatement(inner)) =
                wrapper.body.as_ref()?.statements.first()
            else {
                return None;
            };
            match parenthesized(&inner.expression) {
                Expression::ArrowFunctionExpression(arrow) => Some(Target::Arrow(arrow)),
                _ => None,
            }
        }
        LazyForm::ObjectMethod(_) => {
            let Expression::ObjectExpression(object) = outer else {
                return None;
            };
            let Some(oxc_ast::ast::ObjectPropertyKind::ObjectProperty(property)) =
                object.properties.first()
            else {
                return None;
            };
            match &property.value {
                Expression::FunctionExpression(function) => Some(Target::Function(function)),
                _ => None,
            }
        }
        LazyForm::ClassMethod(_) => {
            let Expression::ClassExpression(class) = outer else {
                return None;
            };
            let Some(oxc_ast::ast::ClassElement::MethodDefinition(method)) =
                class.body.body.first()
            else {
                return None;
            };
            Some(Target::Function(&method.value))
        }
    }
}

fn lower(
    program: &Program<'_>,
    request: &LazyCompileRequest<'_>,
    lazy: LazyFunction,
    kind: SyntaxSourceKind,
    skips: Option<crate::preparse::SkipTable>,
) -> Result<BytecodeModule, CompileError> {
    let function = request.function;
    let target = find_target(program, lazy.form).ok_or_else(|| CompileError::Unsupported {
        node: "lazy function text does not parse back into its form".to_string(),
        span: lazy.range,
    })?;
    let module = Rc::new(RefCell::new(ModuleBuilder::default()));
    // The creating frame, reduced to what the body compile reads from it:
    // strictness, `super` side, the `.arguments` fact, and (for module
    // code) the import bindings. It owns no scope and emits nothing.
    let mut top = FunctionContext::new(Rc::clone(&module))
        .with_strict(lazy.outer_strict)
        .with_module_url(request.module_url);
    top.dot_arguments_observed = lazy.dot_arguments_observed;
    top.super_home_static = lazy.static_home;
    top.closure_context_empty = request.chain.scopes.is_empty();
    if matches!(lazy.form, LazyForm::Arrow { .. }) {
        top.has_home_object = lazy.has_home_object;
    }
    // An arrow in a derived constructor reaches the constructor's `this`
    // and `super()` through the chain, as a direct eval there does.
    if let LazyForm::Arrow {
        derived_constructor: true,
        ..
    } = lazy.form
    {
        top.is_derived_ctor = true;
        top.eval_this_from_chain = true;
    }
    top.module_state = request
        .environment
        .map(|environment| module_state_from(environment, request.resolutions, request.module_url));
    let capture = capture::CaptureFacts::of_unit_skipping(&program.body, skips.as_ref());
    let mut cx = Compiler::new(top, capture);
    cx.skips = skips.map(Rc::new);
    cx.eval_chain = Some(request.chain.clone());
    crate::entry::inherit_eval_chain_state(&mut cx);
    cx.lazy_unit = Some(LazyUnit {
        module_goal: lazy.module_goal,
        typescript: matches!(
            kind,
            SyntaxSourceKind::TypeScript | SyntaxSourceKind::TypeScriptJsx
        ),
        jsx: matches!(
            kind,
            SyntaxSourceKind::JavaScriptJsx | SyntaxSourceKind::TypeScriptJsx
        ),
        top_level_eager: false,
    });
    cx.in_field_initializer = lazy.in_field_initializer;
    if let Some(environment) = request.environment {
        cx.script_global_lexicals = environment.script_lexicals.iter().cloned().collect();
    }
    // The requested function itself compiles now.
    cx.next_fn_eager = true;
    if function.is_method {
        cx.next_fn_is_method = true;
        cx.next_fn_static_home = lazy.static_home;
    }
    let spec = match target {
        Target::Function(node) => crate::functions::FunctionSpec {
            name: &function.name,
            params: &node.params,
            body: node.body.as_deref().map(crate::body::Body::Block),
            span: (node.span.start, node.span.end),
            is_async: node.r#async,
            is_generator: node.generator,
            force_strict: false,
            is_arrow: false,
            nfe_self: lazy.self_binding,
            fields: None,
        },
        Target::Arrow(node) => crate::functions::FunctionSpec {
            name: &function.name,
            params: &node.params,
            body: Some(crate::body::Body::Arrow(&node.body)),
            span: (node.span.start, node.span.end),
            is_async: node.r#async,
            is_generator: false,
            force_strict: false,
            is_arrow: true,
            nfe_self: false,
            fields: None,
        },
    };
    crate::functions::compile_function_impl(&mut cx, spec)?;
    drop(cx);
    let ModuleBuilder {
        functions,
        constants,
        template_sites,
        ..
    } = Rc::try_unwrap(module)
        .expect("module builder should be uniquely owned at finalize")
        .into_inner();
    Ok(BytecodeModule {
        module: request.module_url.to_string(),
        template_sites,
        source_kind: bytecode_source_kind(kind),
        functions,
        constants,
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
        lazy_environments: Vec::new(),
        function_source: None,
    })
}

/// Rebuild the import view of a module's top level from its lazy
/// environment and resolution table.
fn module_state_from(
    environment: &LazyEnvironment,
    resolutions: &[ModuleResolution],
    module_url: &str,
) -> ModuleState {
    let mut state = ModuleState::default();
    for import in &environment.imports {
        state.imported_names.insert(
            import.alias.clone(),
            crate::module_state::ImportBinding {
                record: import.record,
                source_name: import.source_name.clone(),
                is_namespace: import.is_namespace,
                request: ImportRequest {
                    specifier: import.specifier.clone(),
                    attr_type: import.attr_type.clone(),
                },
                is_deferred: import.is_deferred,
            },
        );
    }
    for (local, exported) in &environment.reexports {
        state
            .reexport_local_targets
            .insert(local.clone(), exported.clone());
    }
    for resolution in resolutions
        .iter()
        .filter(|resolution| resolution.referrer == module_url)
    {
        state.pre_resolved_imports.insert(
            ImportRequest {
                specifier: resolution.specifier.clone(),
                attr_type: resolution.attr_type.clone(),
            },
            resolution.target.clone(),
        );
    }
    state
}

/// The top-level facts of a module that its lazily compiled functions need.
pub(crate) fn lazy_environment_from(state: &ModuleState, module_url: &str) -> LazyEnvironment {
    let mut imports: Vec<LazyImport> = state
        .imported_names
        .iter()
        .map(|(alias, binding)| LazyImport {
            alias: alias.clone(),
            record: binding.record,
            source_name: binding.source_name.clone(),
            is_namespace: binding.is_namespace,
            is_deferred: binding.is_deferred,
            specifier: binding.request.specifier.clone(),
            attr_type: binding.request.attr_type.clone(),
        })
        .collect();
    imports.sort_by(|a, b| a.alias.cmp(&b.alias));
    let mut reexports: Vec<(String, Vec<String>)> = state
        .reexport_local_targets
        .iter()
        .map(|(local, exported)| (local.clone(), exported.clone()))
        .collect();
    reexports.sort();
    LazyEnvironment {
        module_url: module_url.to_string(),
        imports,
        reexports,
        script_lexicals: Vec::new(),
    }
}

#[cfg(test)]
#[path = "lazy_tests.rs"]
mod tests;
