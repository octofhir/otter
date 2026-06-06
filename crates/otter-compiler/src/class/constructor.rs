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

fn emit_public_field_define(
    cx: &mut Compiler,
    receiver_reg: u16,
    key_reg: u16,
    value_reg: u16,
    span: (u32, u32),
) {
    let desc_reg = cx.alloc_scratch();
    cx.emit(Op::NewObject, [Operand::Register(desc_reg)], span);

    let value_const = cx.intern_string_constant("value");
    let value_scratch = cx.alloc_scratch();
    cx.emit(
        Op::StoreProperty,
        vec![
            Operand::Register(desc_reg),
            Operand::ConstIndex(value_const),
            Operand::Register(value_reg),
            Operand::Register(value_scratch),
        ],
        span,
    );

    let true_reg = cx.alloc_scratch();
    cx.emit(Op::LoadTrue, [Operand::Register(true_reg)], span);
    for attr in ["writable", "enumerable", "configurable"] {
        let attr_const = cx.intern_string_constant(attr);
        let attr_scratch = cx.alloc_scratch();
        cx.emit(
            Op::StoreProperty,
            vec![
                Operand::Register(desc_reg),
                Operand::ConstIndex(attr_const),
                Operand::Register(true_reg),
                Operand::Register(attr_scratch),
            ],
            span,
        );
    }

    cx.emit(
        Op::DefineOwnProperty,
        [
            Operand::Register(receiver_reg),
            Operand::Register(key_reg),
            Operand::Register(desc_reg),
        ],
        span,
    );
}

///   capture as user-written constructors.
pub(crate) fn compile_synthetic_constructor(
    parent: &mut Compiler,
    name: &str,
    is_derived: bool,
    span: (u32, u32),
    instance_fields: &[&oxc_ast::ast::PropertyDefinition<'_>],
) -> Result<(u32, Vec<u32>), CompileError> {
    let module = Rc::clone(&parent.top_mut().module);
    let mut child = FunctionContext::new(Rc::clone(&module))
        .with_strict(true)
        .with_module_url(parent.module_url.clone());
    // No body to pre-pass; only the synthesised super call needs
    // outer captures. A direct eval inside a field initializer still
    // runs with this constructor frame as its caller (§19.2.1.3).
    let contains_direct_eval = instance_fields.iter().any(|field| {
        field
            .value
            .as_ref()
            .is_some_and(capture::expression_contains_direct_eval)
    });
    if contains_direct_eval {
        child.captured_names.insert("arguments".to_string());
    }
    child.contains_direct_eval = contains_direct_eval;
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
        module_url: parent.module_url.clone(),
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
            Op::SuperConstructSpread,
            vec![
                Operand::Register(dst),
                Operand::Register(super_ctor),
                Operand::Register(args_reg),
            ],
            span,
        );
        // §13.3.7.3 steps 7–9 — bind `this` so the field initializers
        // below (and the implicit return) see the constructed value.
        parent.emit(Op::BindThisValue, [Operand::Register(dst)], span);
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

    let direct_eval_meta: Vec<otter_bytecode::DirectEvalBinding> = if contains_direct_eval {
        collect_direct_eval_bindings(parent, &[])
    } else {
        Vec::new()
    };
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
    slot.length = 0;
    slot.has_rest = is_derived;
    slot.is_derived_constructor = is_derived;
    slot.own_upvalue_count = child.own_upvalue_count;
    slot.direct_eval_bindings = direct_eval_meta;
    slot.contains_direct_eval = contains_direct_eval;
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
        let module = Rc::clone(&parent.top_mut().module);
        let (function_id, captures) =
            compile_function_full(parent, name, params, body, span, is_async, false, true)?;
        if is_derived {
            module
                .borrow_mut()
                .functions
                .get_mut(function_id as usize)
                .expect("compiled constructor slot")
                .is_derived_constructor = true;
        }
        return Ok((function_id, captures));
    }
    // Compile the function with field-init injection. We mirror
    // `compile_function` but inject the field stores after the
    // self-name binding and before the user body. The compiler
    // doesn't have a public hook for this, so we duplicate the
    // setup here.
    let module = Rc::clone(&parent.top_mut().module);
    validate_formal_parameter_names(params, true, false, span)?;
    let mut child = FunctionContext::new(Rc::clone(&module))
        .with_strict(true)
        .with_module_url(parent.module_url.clone());
    if let Some(b) = body {
        child.captured_names = capture::analyze_function(Some(params), b);
    }
    // §19.2.1.3 — field initializers compile into this constructor
    // frame; a direct eval in either the body or any initializer
    // receives the constructor's variable environment (and its
    // new.target / this).
    let contains_direct_eval = body
        .as_ref()
        .is_some_and(|b| capture::body_contains_direct_eval(Some(params), b))
        || instance_fields.iter().any(|field| {
            field
                .value
                .as_ref()
                .is_some_and(capture::expression_contains_direct_eval)
        });
    if contains_direct_eval {
        if let Some(b) = body {
            child
                .captured_names
                .extend(capture::all_own_names(Some(params), b));
        }
        child.captured_names.insert("arguments".to_string());
    }
    child.contains_direct_eval = contains_direct_eval;
    parent.push(child);
    parent.enter_scope();

    let param_count = u16::try_from(params.items.len()).expect("too many parameters");
    let length = formal_parameter_length(params);
    parent.scratch = param_count;
    let has_rest = params.rest.is_some();

    let function_id = module.borrow().functions.len() as u32;
    module.borrow_mut().functions.push(Function {
        id: function_id,
        name: name.to_string(),
        span,
        is_strict: true,
        module_url: parent.module_url.clone(),
        ..Default::default()
    });

    predeclare_formal_parameters(parent, params, false, span)?;
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
    let direct_eval_meta: Vec<otter_bytecode::DirectEvalBinding> = if contains_direct_eval {
        collect_direct_eval_bindings(parent, &[])
    } else {
        Vec::new()
    };
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
    slot.length = length;
    slot.has_rest = has_rest;
    slot.is_async = is_async;
    slot.is_derived_constructor = is_derived;
    slot.own_upvalue_count = child.own_upvalue_count;
    slot.direct_eval_bindings = direct_eval_meta;
    slot.contains_direct_eval = contains_direct_eval;
    slot.code = child.code;
    slot.spans = child.spans;
    Ok((function_id, captures))
}

/// upvalues) per §15.7.10 InitializeFieldsForReceiver.
pub(crate) fn emit_instance_field_inits(
    cx: &mut Compiler,
    fields: &[&oxc_ast::ast::PropertyDefinition<'_>],
) -> Result<(), CompileError> {
    // §15.7.10 — field initializers are their own function-like code
    // with no [[NewTarget]]; a direct eval there observes
    // `new.target` as `undefined`.
    let saved_field_init = cx.in_field_initializer;
    cx.in_field_initializer = true;
    let result = emit_instance_field_inits_inner(cx, fields);
    cx.in_field_initializer = saved_field_init;
    result
}

fn emit_instance_field_inits_inner(
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
            // expression at constructor-run time and define a data
            // property via DefineField / CreateDataPropertyOrThrow.
            let key_expr = p
                .key
                .as_expression()
                .ok_or_else(|| CompileError::Unsupported {
                    node: "ClassDeclaration: non-expression computed instance field key"
                        .to_string(),
                    span: pspan,
                })?;
            let key_reg = compile_expr(cx, key_expr, pspan)?;
            emit_public_field_define(cx, this_reg, key_reg, value_reg, pspan);
            continue;
        }
        let key_str = match &p.key {
            oxc_ast::ast::PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
            oxc_ast::ast::PropertyKey::StringLiteral(lit) => lit.value.to_string(),
            oxc_ast::ast::PropertyKey::NumericLiteral(lit) => lit.value.to_string(),
            oxc_ast::ast::PropertyKey::PrivateIdentifier(pid) => {
                let key_reg = crate::class::load_private_key(cx, pid.name.as_str(), pspan)?;
                emit_public_field_define(cx, this_reg, key_reg, value_reg, pspan);
                continue;
            }
            _ => {
                return Err(CompileError::Unsupported {
                    node: "ClassDeclaration: non-string instance field key".to_string(),
                    span: pspan,
                });
            }
        };
        let key_reg = cx.alloc_scratch();
        let key_const = cx.intern_string_constant(&key_str);
        cx.emit(
            Op::LoadString,
            [Operand::Register(key_reg), Operand::ConstIndex(key_const)],
            pspan,
        );
        emit_public_field_define(cx, this_reg, key_reg, value_reg, pspan);
    }
    Ok(())
}
