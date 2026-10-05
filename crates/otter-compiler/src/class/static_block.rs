//! Static block and static field initializer lowering.
//!
//! # Contents
//! - [`compile_static_block`] - compile a `static { ... }` block as a synthesized function.
//! - [`compile_static_field_initializer`] - compile one static field initializer function.
//!
//! # Invariants
//! - Both compile under their own strict function scope (`this` is the
//!   class, `super.x` resolves through the statics-side home object).
//! - Captures are analyzed before bytecode emission so their bindings a
//!   nested closure or eval reaches become context slots.
//!
//! # See also
//! - [`super`]

use crate::*;

/// Enter the strict variable scope of a synthesized class-element function.
fn enter_element_scope(parent: &mut Compiler) {
    parent.enter_scope_with_flags(
        otter_bytecode::ScopeKind::Body,
        otter_bytecode::ScopeFlags {
            strict: true,
            var_scope: true,
            has_extension: false,
        },
    );
}

/// - <https://tc39.es/ecma262/#sec-class-static-block>
pub(crate) fn compile_static_block(
    parent: &mut Compiler,
    class_name: &str,
    body: &oxc_allocator::Vec<'_, Statement<'_>>,
    span: (u32, u32),
) -> Result<ClosureRecord, CompileError> {
    let module = Rc::clone(&parent.top_mut().module);
    let mut child = FunctionContext::new(Rc::clone(&module))
        .with_strict(true)
        .with_module_url(parent.module_url.clone());
    // §15.7.4 — `var` / `let` / `function` declarations inside a
    // static block live in the block's own scope.
    let unit = Rc::clone(&parent.capture);
    let facts = unit.static_block(body);
    child.captured_names = unit.captured(facts, false);
    // §15.7.4 — `super.x` inside a static block resolves through the
    // statics-side home object.
    child.super_home_static = true;
    child.has_home_object = true;
    child.contains_direct_eval = unit.contains_eval(facts);
    if child.contains_direct_eval {
        child.captured_names.extend(unit.own_names(facts, false));
    }
    let parent_ctx = parent.innermost_ctx();
    child.closure_context_empty =
        parent_ctx == crate::scope::CtxReg::Closure && parent.closure_context_empty;
    parent.push(child);
    enter_element_scope(parent);

    let function_id = module.borrow().functions.len() as u32;
    module.borrow_mut().functions.push(Function {
        id: function_id,
        name: format!("{class_name}.<static-init>"),
        span,
        is_strict: true,
        is_method: true,
        module_url: parent.module_url.clone(),
        ..Default::default()
    });

    let result: Result<(), CompileError> = (|| {
        let mut var_names: Vec<String> = Vec::new();
        hoist_var_names(body, &mut var_names);
        pre_declare_var_bindings(parent, &var_names, span)?;
        let mut lex_names: Vec<(String, bool)> = Vec::new();
        hoist_lexical_names(body, &mut lex_names);
        pre_declare_lexical_bindings(parent, &lex_names, span)?;
        hoist_function_declarations(parent, body)?;
        for stmt in body {
            compile_discarded_statement(parent, stmt)?;
        }
        parent.emit(Op::ReturnUndefined, vec![], span);
        Ok(())
    })();
    parent.exit_scope();
    let mut child = parent.pop();
    result?;
    let contains_direct_eval = child.contains_direct_eval;
    let needs_context = finish_function(parent, &mut child, function_id, span, |slot| {
        // Direct eval routing (§19.2.1.1 `inFunction`) reads this flag.
        slot.contains_direct_eval = contains_direct_eval;
        slot.param_count = 0;
    })?;
    Ok(ClosureRecord {
        function_id,
        ctx: parent_ctx,
        needs_context,
        is_arrow: false,
    })
}

/// §15.7.10 ClassFieldDefinitionEvaluation for a STATIC field — the
/// initializer is its own function-like code unit, invoked with
/// `this` bound to the class value, so `this`, arrows capturing
/// `this`, and `super.x` (statics-side home) all observe the class.
/// `inferred_name` carries the §13.15.2 NamedEvaluation key for an
/// anonymous function initializer.
pub(crate) fn compile_static_field_initializer(
    parent: &mut Compiler,
    class_name: &str,
    value: Option<&oxc_ast::ast::Expression<'_>>,
    inferred_name: Option<&str>,
    span: (u32, u32),
) -> Result<ClosureRecord, CompileError> {
    let module = Rc::clone(&parent.top_mut().module);
    let mut child = FunctionContext::new(Rc::clone(&module))
        .with_strict(true)
        .with_module_url(parent.module_url.clone());
    child.super_home_static = true;
    child.has_home_object = true;
    child.contains_direct_eval = value
        .as_ref()
        .is_some_and(|expr| crate::capture::expression_contains_direct_eval(expr));
    let parent_ctx = parent.innermost_ctx();
    child.closure_context_empty =
        parent_ctx == crate::scope::CtxReg::Closure && parent.closure_context_empty;
    parent.push(child);
    enter_element_scope(parent);

    let function_id = module.borrow().functions.len() as u32;
    module.borrow_mut().functions.push(Function {
        id: function_id,
        name: format!("{class_name}.<static-field-init>"),
        span,
        is_strict: true,
        is_method: true,
        module_url: parent.module_url.clone(),
        ..Default::default()
    });

    let result: Result<(), CompileError> = (|| {
        match value {
            Some(expr) => {
                let value_reg = match inferred_name {
                    Some(key) => {
                        crate::expr::compile_expr_with_inferred_name(parent, expr, key, span)?
                    }
                    None => compile_expr(parent, expr, span)?,
                };
                parent.emit(Op::Return, [Operand::Register(value_reg)], span);
            }
            None => parent.emit(Op::ReturnUndefined, vec![], span),
        }
        Ok(())
    })();
    parent.exit_scope();
    let mut child = parent.pop();
    result?;
    let contains_direct_eval = child.contains_direct_eval;
    let needs_context = finish_function(parent, &mut child, function_id, span, |slot| {
        // Direct eval routing (§19.2.1.1 `inFunction`) reads this flag.
        slot.contains_direct_eval = contains_direct_eval;
        slot.param_count = 0;
    })?;
    Ok(ClosureRecord {
        function_id,
        ctx: parent_ctx,
        needs_context,
        is_arrow: false,
    })
}
