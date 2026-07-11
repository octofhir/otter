//! Static block lowering for class declarations and expressions.
//!
//! # Contents
//! - [`compile_static_block`] - compile a `static { ... }` block as a synthesized function.
//!
//! # Invariants
//! - Static blocks compile under their own strict function scope.
//! - Captures are analyzed before bytecode emission so outer locals become upvalues.
//!
//! # See also
//! - [`super`]

use crate::*;

/// - <https://tc39.es/ecma262/#sec-class-static-block>
pub(crate) fn compile_static_block(
    parent: &mut Compiler,
    class_name: &str,
    body: &oxc_allocator::Vec<'_, Statement<'_>>,
    span: (u32, u32),
) -> Result<(u32, Vec<u32>), CompileError> {
    let module = Rc::clone(&parent.top_mut().module);
    let mut child = FunctionContext::new(Rc::clone(&module))
        .with_strict(true)
        .with_module_url(parent.module_url.clone());
    // §15.7.4 — `var` / `let` / `function` declarations inside a
    // static block live in the block's own scope. Compute the
    // capture-name set so identifier references to outer locals
    // can promote to upvalues just like any nested function body.
    child.captured_names = capture::analyze_module(body);
    // §15.7.4 — `super.x` inside a static block resolves through the
    // statics-side home object.
    child.super_home_static = true;
    child.contains_direct_eval = crate::capture::program_contains_direct_eval(body);
    parent.push(child);
    parent.enter_scope();

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

    let mut var_names: Vec<String> = Vec::new();
    hoist_var_names(body, &mut var_names);
    pre_declare_var_bindings(parent, &var_names, span)?;
    let mut lex_names: Vec<(String, bool)> = Vec::new();
    hoist_lexical_names(body, &mut lex_names);
    pre_declare_lexical_bindings(parent, &lex_names, span)?;
    hoist_function_declarations(parent, body)?;
    for stmt in body {
        compile_statement(parent, stmt)?;
    }
    parent.exit_scope();
    parent.emit(Op::ReturnUndefined, vec![], span);

    let mut child = parent.pop();
    if child.register_overflow {
        return Err(CompileError::Unsupported {
            node: "function body exhausts the 65535-register window".to_string(),
            span,
        });
    }

    let captures = child.parent_captures.clone();
    let mut no_eval_meta: Vec<otter_bytecode::DirectEvalBinding> = Vec::new();
    crate::function_context::finalize_virtual_capture_indices(
        &mut child.code,
        &mut no_eval_meta,
        child.own_upvalue_count,
    );
    let mut module_mut = module.borrow_mut();
    let slot = module_mut
        .functions
        .get_mut(function_id as usize)
        .expect("reserved static-block slot");
    slot.locals = 0;
    slot.scratch = child.scratch_window();
    // Direct eval routing (§19.2.1.1 `inFunction`) reads this flag —
    // without it the eval body runs on the script path and loses the
    // synthesized frame's `this` (= the class).
    slot.contains_direct_eval = child.contains_direct_eval;
    slot.param_count = 0;
    slot.own_upvalue_count = child.own_upvalue_count;
    slot.code = child.code.finish();
    slot.spans = child.spans;
    Ok((function_id, captures))
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
) -> Result<(u32, Vec<u32>), CompileError> {
    let module = Rc::clone(&parent.top_mut().module);
    let mut child = FunctionContext::new(Rc::clone(&module))
        .with_strict(true)
        .with_module_url(parent.module_url.clone());
    child.super_home_static = true;
    child.contains_direct_eval = value
        .as_ref()
        .is_some_and(|expr| crate::capture::expression_contains_direct_eval(expr));
    parent.push(child);
    parent.enter_scope();

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

    match value {
        Some(expr) => {
            let value_reg = match inferred_name {
                Some(key) => crate::expr::compile_expr_with_inferred_name(parent, expr, key, span)?,
                None => compile_expr(parent, expr, span)?,
            };
            parent.emit(Op::Return, [Operand::Register(value_reg)], span);
        }
        None => parent.emit(Op::ReturnUndefined, vec![], span),
    }
    parent.exit_scope();

    let mut child = parent.pop();
    if child.register_overflow {
        return Err(CompileError::Unsupported {
            node: "function body exhausts the 65535-register window".to_string(),
            span,
        });
    }

    let captures = child.parent_captures.clone();
    let mut no_eval_meta: Vec<otter_bytecode::DirectEvalBinding> = Vec::new();
    crate::function_context::finalize_virtual_capture_indices(
        &mut child.code,
        &mut no_eval_meta,
        child.own_upvalue_count,
    );
    let mut module_mut = module.borrow_mut();
    let slot = module_mut
        .functions
        .get_mut(function_id as usize)
        .expect("reserved static-field-init slot");
    slot.locals = 0;
    slot.scratch = child.scratch_window();
    // Direct eval routing (§19.2.1.1 `inFunction`) reads this flag —
    // without it the eval body runs on the script path and loses the
    // synthesized frame's `this` (= the class).
    slot.contains_direct_eval = child.contains_direct_eval;
    slot.param_count = 0;
    slot.own_upvalue_count = child.own_upvalue_count;
    slot.code = child.code.finish();
    slot.spans = child.spans;
    Ok((function_id, captures))
}
