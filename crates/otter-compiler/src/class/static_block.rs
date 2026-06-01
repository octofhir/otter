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
    parent.push(child);
    parent.enter_scope();

    let function_id = module.borrow().functions.len() as u32;
    module.borrow_mut().functions.push(Function {
        id: function_id,
        name: format!("{class_name}.<static-init>"),
        span,
        is_strict: true,
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

    let child = parent.pop();
    let captures = child.parent_captures.clone();
    let mut module_mut = module.borrow_mut();
    let slot = module_mut
        .functions
        .get_mut(function_id as usize)
        .expect("reserved static-block slot");
    slot.locals = 0;
    slot.scratch = child.scratch;
    slot.param_count = 0;
    slot.own_upvalue_count = child.own_upvalue_count;
    slot.code = child.code;
    slot.spans = child.spans;
    Ok((function_id, captures))
}
