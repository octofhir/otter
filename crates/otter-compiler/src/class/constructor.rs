//! Constructor lowering and instance-field initialization for classes.
//!
//! # Contents
//! - [`compile_synthetic_constructor`] - synthesize default base and derived constructors.
//! - [`compile_class_constructor`] - compile user-written constructors with field initialization.
//! - [`emit_instance_field_inits`] - emit instance field stores against `this`.
//!
//! # Invariants
//! - Instance fields are initialized at the class-constructor points required by class evaluation.
//! - Derived constructors initialize fields after the top-level `super(...)` call when present.
//!
//! # See also
//! - [`super`]

use super::{SUPER_CTOR_NAME, is_top_level_super_call, load_synthetic_capture};
use crate::*;

///   capture as user-written constructors.
pub(crate) fn compile_synthetic_constructor(
    parent: &mut Compiler,
    name: &str,
    is_derived: bool,
    span: (u32, u32),
    instance_fields: &[&oxc_ast::ast::PropertyDefinition<'_>],
) -> Result<(u32, Vec<u32>), CompileError> {
    let module = Rc::clone(&parent.top_mut().module);
    let child = FunctionContext::new(Rc::clone(&module)).with_strict(true);
    // No body to pre-pass; only the synthesised super call needs
    // outer captures.
    parent.push(child);
    parent.enter_scope();

    // Reserve the function record up-front so the slot id is
    // stable across recursive compile cycles.
    let function_id = module.borrow().functions.len() as u32;
    module.borrow_mut().functions.push(Function {
        id: function_id,
        name: name.to_string(),
        span,
        is_strict: true,
        ..Default::default()
    });

    if is_derived {
        // Default derived ctor is `constructor(...args) {
        // super(...args); }`: materialise the incoming argument
        // list and construct the captured superclass.
        //
        // # See also
        // - <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
        let super_ctor = load_synthetic_capture(parent, SUPER_CTOR_NAME, span)?;
        let args_reg = parent.alloc_scratch();
        parent.emit(Op::CollectRest, [Operand::Register(args_reg)], span);
        let dst = parent.alloc_scratch();
        parent.emit(
            Op::NewSpread,
            vec![
                Operand::Register(dst),
                Operand::Register(super_ctor),
                Operand::Register(args_reg),
            ],
            span,
        );
        if instance_fields.is_empty() {
            parent.emit(Op::Return, [Operand::Register(dst)], span);
        }
    }
    // §15.7.10 InitializeInstanceElements — run instance-field
    // initialisers with `this` bound to the new instance, after the
    // super() call has run.
    emit_instance_field_inits(parent, instance_fields)?;
    if !is_derived || !instance_fields.is_empty() {
        parent.emit(Op::ReturnUndefined, vec![], span);
    }

    parent.exit_scope();
    let child = parent.pop();
    let captures = child.parent_captures.clone();
    let mut module_mut = module.borrow_mut();
    let slot = module_mut
        .functions
        .get_mut(function_id as usize)
        .expect("reserved synthetic ctor slot");
    slot.locals = 0;
    slot.scratch = child.scratch;
    slot.param_count = 0;
    slot.has_rest = is_derived;
    slot.own_upvalue_count = child.own_upvalue_count;
    slot.code = child.code;
    slot.spans = child.spans;
    Ok((function_id, captures))
}

/// - <https://tc39.es/ecma262/#sec-initializeinstanceelements>
#[allow(clippy::too_many_arguments)]
pub(crate) fn compile_class_constructor(
    parent: &mut Compiler,
    name: &str,
    params: &oxc_ast::ast::FormalParameters<'_>,
    body: &Option<oxc_allocator::Box<'_, oxc_ast::ast::FunctionBody<'_>>>,
    span: (u32, u32),
    is_async: bool,
    instance_fields: &[&oxc_ast::ast::PropertyDefinition<'_>],
    is_derived: bool,
) -> Result<(u32, Vec<u32>), CompileError> {
    if instance_fields.is_empty() {
        return compile_function_full(parent, name, params, body, span, is_async, false, true);
    }
    // Compile the function with field-init injection. We mirror
    // `compile_function` but inject the field stores after the
    // self-name binding and before the user body. The compiler
    // doesn't have a public hook for this, so we duplicate the
    // setup here.
    let module = Rc::clone(&parent.top_mut().module);
    validate_formal_parameter_names(params, true, false, span)?;
    let mut child = FunctionContext::new(Rc::clone(&module)).with_strict(true);
    if let Some(b) = body {
        child.captured_names = capture::analyze_function(Some(params), b);
    }
    parent.push(child);
    parent.enter_scope();

    let param_count = u16::try_from(params.items.len()).expect("too many parameters");
    parent.scratch = param_count;
    let has_rest = params.rest.is_some();

    let function_id = module.borrow().functions.len() as u32;
    module.borrow_mut().functions.push(Function {
        id: function_id,
        name: name.to_string(),
        span,
        is_strict: true,
        ..Default::default()
    });

    for (ordinal, param) in params.items.iter().enumerate() {
        compile_formal_parameter(
            parent,
            ordinal as u16,
            &param.pattern,
            param.initializer.as_deref(),
            span,
            false,
        )?;
    }
    if let Some(rest) = &params.rest {
        compile_rest_parameter(parent, &rest.rest.argument, span)?;
    }

    let self_storage = parent.declare_binding(name, false, span)?;
    let const_idx = parent.intern_function_id(function_id);
    let tmp = parent.alloc_scratch();
    parent.emit(
        Op::MakeFunction,
        [Operand::Register(tmp), Operand::ConstIndex(const_idx)],
        span,
    );
    parent.emit_store_storage(tmp, self_storage, span);
    parent.mark_initialized(name);

    // §15.7.10 InitializeInstanceElements — base classes run field
    // initialisers immediately (before the user body); derived
    // classes run them right after the user-written `super(...)`
    // call returns, so `this` is already allocated.
    if !is_derived {
        emit_instance_field_inits(parent, instance_fields)?;
    }

    if let Some(body) = body {
        let mut var_names: Vec<String> = Vec::new();
        hoist_var_names(&body.statements, &mut var_names);
        pre_declare_var_bindings(parent, &var_names, span)?;
        let mut lex_names: Vec<(String, bool)> = Vec::new();
        hoist_lexical_names(&body.statements, &mut lex_names);
        pre_declare_lexical_bindings(parent, &lex_names, span)?;
        hoist_function_declarations(parent, &body.statements)?;
        let mut fields_emitted = !is_derived;
        for stmt in &body.statements {
            compile_statement(parent, stmt)?;
            // Inject the field initialisers as soon as the user's
            // first statement-level `super(...)` call has run. This
            // mirrors the spec's "after the super call returns" rule
            // for derived constructors. If the user doesn't write a
            // top-level super-call (defensive shape) we fall through
            // to the post-body emission below.
            if !fields_emitted && is_top_level_super_call(stmt) {
                emit_instance_field_inits(parent, instance_fields)?;
                fields_emitted = true;
            }
        }
        if !fields_emitted {
            emit_instance_field_inits(parent, instance_fields)?;
        }
    } else if is_derived {
        // No body at all (degenerate shape) — emit field inits.
        emit_instance_field_inits(parent, instance_fields)?;
    }
    parent.exit_scope();
    parent.emit(Op::ReturnUndefined, vec![], span);

    let child = parent.pop();
    let captures = child.parent_captures.clone();
    let mut module_mut = module.borrow_mut();
    let slot = module_mut
        .functions
        .get_mut(function_id as usize)
        .expect("reserved function slot");
    slot.locals = 0;
    slot.scratch = child.scratch;
    slot.param_count = param_count;
    slot.has_rest = has_rest;
    slot.is_async = is_async;
    slot.own_upvalue_count = child.own_upvalue_count;
    slot.code = child.code;
    slot.spans = child.spans;
    Ok((function_id, captures))
}

/// upvalues) per §15.7.10 InitializeFieldsForReceiver.
pub(crate) fn emit_instance_field_inits(
    cx: &mut Compiler,
    fields: &[&oxc_ast::ast::PropertyDefinition<'_>],
) -> Result<(), CompileError> {
    for p in fields {
        let pspan = (p.span.start, p.span.end);
        let value_reg = match &p.value {
            Some(expr) => compile_expr(cx, expr, pspan)?,
            None => {
                let dst = cx.alloc_scratch();
                cx.emit(Op::LoadUndefined, [Operand::Register(dst)], pspan);
                dst
            }
        };
        let this_reg = cx.alloc_scratch();
        cx.emit(Op::LoadThis, [Operand::Register(this_reg)], pspan);
        if p.computed {
            // §15.7.10 — computed-key field. Evaluate the key
            // expression at constructor-run time and write via
            // `Op::StoreElement`.
            let key_expr = p
                .key
                .as_expression()
                .ok_or_else(|| CompileError::Unsupported {
                    node: "ClassDeclaration: non-expression computed instance field key"
                        .to_string(),
                    span: pspan,
                })?;
            let key_reg = compile_expr(cx, key_expr, pspan)?;
            cx.emit_store_element(this_reg, key_reg, value_reg, pspan);
            continue;
        }
        let key_str = match &p.key {
            oxc_ast::ast::PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
            oxc_ast::ast::PropertyKey::StringLiteral(lit) => lit.value.to_string(),
            oxc_ast::ast::PropertyKey::NumericLiteral(lit) => lit.value.to_string(),
            oxc_ast::ast::PropertyKey::PrivateIdentifier(pid) => cx
                .mangle_private(pid.name.as_str())
                .ok_or(CompileError::Unsupported {
                    node: "ClassDeclaration: private instance field outside class".to_string(),
                    span: pspan,
                })?,
            _ => {
                return Err(CompileError::Unsupported {
                    node: "ClassDeclaration: non-string instance field key".to_string(),
                    span: pspan,
                });
            }
        };
        cx.emit_store_property(this_reg, &key_str, value_reg, pspan);
    }
    Ok(())
}
