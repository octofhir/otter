//! Formal parameter binding and validation lowering.
//!
//! # Contents
//! - simple and rest parameter compilation
//! - strict-name validation
//! - mapped arguments binding metadata
//!
//! # Invariants
//! - Parameter order follows source order and preserves mapped arguments aliases.
//!
//! # See also
//! - `functions` for function body lowering

use crate::*;

/// Lower one positional formal parameter at ordinal `ordinal` (the
/// raw argv slot the call dispatcher writes into).
///
/// OXC keeps the default expression on `FormalParameter::initializer`
/// rather than wrapping the pattern in an `AssignmentPattern`
/// (which is reserved for *inner* defaults like
/// `function f({x = 1}) {}`). We honour both spellings here so
/// callers don't have to peek into the OXC structure.
pub(crate) fn compile_formal_parameter(
    parent: &mut Compiler,
    ordinal: u16,
    pattern: &oxc_ast::ast::BindingPattern<'_>,
    initializer: Option<&Expression<'_>>,
    span: (u32, u32),
    allow_duplicate_formals: bool,
) -> Result<(), CompileError> {
    if initializer.is_none()
        && let oxc_ast::ast::BindingPattern::BindingIdentifier(id) = pattern
    {
        return bind_simple_formal_parameter(
            parent,
            ordinal,
            id.name.as_str(),
            span,
            allow_duplicate_formals,
        );
    }
    if let Some(default_expr) = initializer {
        apply_default_into(parent, ordinal, default_expr, span)?;
    }
    if let oxc_ast::ast::BindingPattern::AssignmentPattern(asgn) = pattern {
        apply_default_into(
            parent,
            ordinal,
            &asgn.right,
            (asgn.span.start, asgn.span.end),
        )?;
        return destructure_assign(parent, ordinal, &asgn.left, span);
    }
    destructure_assign(parent, ordinal, pattern, span)
}

pub(crate) fn predeclare_formal_parameters(
    parent: &mut Compiler,
    params: &oxc_ast::ast::FormalParameters<'_>,
    allow_duplicate_formals: bool,
    span: (u32, u32),
) -> Result<(), CompileError> {
    let mut names = Vec::new();
    for param in &params.items {
        collect_pattern_var_names(&param.pattern, &mut names);
    }
    if let Some(rest) = &params.rest {
        collect_pattern_var_names(&rest.rest.argument, &mut names);
    }
    let mut seen = HashSet::new();
    for name in names {
        if allow_duplicate_formals && !seen.insert(name.clone()) {
            continue;
        }
        parent.declare_binding(&name, false, span)?;
    }
    Ok(())
}

pub(crate) fn bind_simple_formal_parameter(
    parent: &mut Compiler,
    ordinal: u16,
    name: &str,
    span: (u32, u32),
    allow_duplicate_formals: bool,
) -> Result<(), CompileError> {
    let storage = if let Some(info) = parent.lookup_in_current_scope(name) {
        info.storage
    } else if allow_duplicate_formals {
        match parent.lookup_in_current_scope(name) {
            Some(info) => info.storage,
            None => parent.declare_binding(name, false, span)?,
        }
    } else {
        parent.declare_binding(name, false, span)?
    };
    parent.emit_store_storage(ordinal, storage, span);
    parent.mark_initialized(name);
    Ok(())
}

pub(crate) fn formal_parameters_are_simple(params: &oxc_ast::ast::FormalParameters<'_>) -> bool {
    params.rest.is_none()
        && params.items.iter().all(|param| {
            param.initializer.is_none()
                && matches!(
                    param.pattern,
                    oxc_ast::ast::BindingPattern::BindingIdentifier(_)
                )
        })
}

pub(crate) fn formal_parameter_length(params: &oxc_ast::ast::FormalParameters<'_>) -> u16 {
    let mut count = 0u16;
    for param in &params.items {
        if param.initializer.is_some()
            || matches!(
                param.pattern,
                oxc_ast::ast::BindingPattern::AssignmentPattern(_)
            )
        {
            break;
        }
        count = count.checked_add(1).expect("too many parameters");
    }
    count
}

pub(crate) fn validate_formal_parameter_names(
    params: &oxc_ast::ast::FormalParameters<'_>,
    is_strict: bool,
    allow_duplicates: bool,
    span: (u32, u32),
) -> Result<(), CompileError> {
    let mut names = Vec::new();
    for param in &params.items {
        collect_pattern_var_names(&param.pattern, &mut names);
    }
    if let Some(rest) = &params.rest {
        collect_pattern_var_names(&rest.rest.argument, &mut names);
    }

    let mut seen = HashSet::new();
    for name in names {
        if is_strict && (name == "eval" || name == "arguments") {
            return Err(CompileError::Unsupported {
                node: format!("restricted formal parameter name `{name}` in strict function"),
                span,
            });
        }
        if !allow_duplicates && !seen.insert(name.clone()) {
            return Err(CompileError::Unsupported {
                node: format!("redeclaration of `{name}` in same scope"),
                span,
            });
        }
    }
    Ok(())
}

/// Lower the rest parameter (`function f(..., ...rest) { … }`).
/// Reads the trailing args off the frame via `Op::CollectRest`,
/// then routes the resulting array through the same
/// destructuring path so `function f(...[a, b])` falls out for
/// free.
pub(crate) fn compile_rest_parameter(
    parent: &mut Compiler,
    pattern: &oxc_ast::ast::BindingPattern<'_>,
    span: (u32, u32),
) -> Result<(), CompileError> {
    let rest_reg = parent.alloc_scratch();
    parent.emit(Op::CollectRest, [Operand::Register(rest_reg)], span);
    destructure_assign(parent, rest_reg, pattern, span)
}

pub(crate) fn simple_formal_names(params: &oxc_ast::ast::FormalParameters<'_>) -> Vec<String> {
    params
        .items
        .iter()
        .filter_map(|param| match &param.pattern {
            oxc_ast::ast::BindingPattern::BindingIdentifier(id) if param.initializer.is_none() => {
                Some(id.name.to_string())
            }
            _ => None,
        })
        .collect()
}

pub(crate) fn mapped_formal_parameter_bindings(
    cx: &Compiler,
    params: &oxc_ast::ast::FormalParameters<'_>,
) -> Vec<MappedArgumentBinding> {
    let names = simple_formal_names(params);
    let mut seen = HashSet::new();
    let mut bindings = Vec::new();
    for (index, name) in names.iter().enumerate().rev() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some(info) = cx.lookup_in_current_scope(name) else {
            continue;
        };
        bindings.push(MappedArgumentBinding {
            argument_index: index as u16,
            formal_name: name.clone(),
            storage: info.storage.to_argument_storage(),
        });
    }
    bindings.reverse();
    bindings
}

/// Lower an [`AssignmentExpression`]. Plain `=` and the compound
/// arithmetic / bitwise / `**=` shapes share one path; logical
/// assignments (`||=`, `&&=`, `??=`) are deferred (they need
/// short-circuit lowering) and short-circuit out with a clear
/// `Unsupported` diagnostic.
/// `true` when `name` is forbidden as an `IdentifierReference` in
/// strict mode per §13.1.1 (FutureReservedWord) plus the `eval` /
/// `arguments` early errors. Escaped reserved words are caught by
/// the same check because oxc reports the decoded `StringValue` in
/// `IdentifierReference.name`.
pub(crate) fn is_strict_reserved_binding_name(name: &str) -> bool {
    matches!(
        name,
        "eval"
            | "arguments"
            | "implements"
            | "interface"
            | "let"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "static"
            | "yield"
    )
}
