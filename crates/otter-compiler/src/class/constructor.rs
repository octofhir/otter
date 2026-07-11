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
        // §15.7.10 InitializeInstanceElements — brand + field
        // initialisers run against the bound `this` BEFORE the
        // implicit return (a private-method brand applies even with
        // no fields).
        emit_instance_field_inits(parent, instance_fields)?;
        parent.emit(Op::Return, [Operand::Register(dst)], span);
    } else {
        emit_instance_field_inits(parent, instance_fields)?;
        parent.emit(Op::ReturnUndefined, vec![], span);
    }

    let direct_eval_meta: Vec<otter_bytecode::DirectEvalBinding> = if contains_direct_eval {
        capture_lexical_environment_for_eval(parent);
        capture_private_environment_for_eval(parent);
        capture_super_bindings_for_eval(parent);
        collect_direct_eval_bindings(parent, &[])
    } else {
        Vec::new()
    };
    parent.exit_scope();
    let mut child = parent.pop();
    if child.register_overflow {
        return Err(CompileError::Unsupported {
            node: "function body exhausts the 65535-register window".to_string(),
            span,
        });
    }

    let captures = child.parent_captures.clone();
    let mut direct_eval_meta = direct_eval_meta;
    crate::function_context::finalize_virtual_capture_indices(
        &mut child.code,
        &mut direct_eval_meta,
        child.own_upvalue_count,
    );
    let mut module_mut = module.borrow_mut();
    let slot = module_mut
        .functions
        .get_mut(function_id as usize)
        .expect("reserved synthetic ctor slot");
    slot.locals = 0;
    slot.scratch = child.scratch_window();
    slot.param_count = 0;
    slot.length = 0;
    slot.has_rest = is_derived;
    slot.is_derived_constructor = is_derived;
    slot.own_upvalue_count = child.own_upvalue_count;
    slot.direct_eval_bindings = direct_eval_meta;
    slot.contains_direct_eval = contains_direct_eval;
    slot.code = child.code.finish();
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
    // §7.3.30 — a class with instance private METHODS brands every
    // instance (the brand store lives in the field-init prologue),
    // so such constructors take the field-init compilation path even
    // with zero fields.
    let needs_brand = parent
        .class_private_instance_methods
        .last()
        .is_some_and(|methods| !methods.is_empty());
    if instance_fields.is_empty() && !needs_brand {
        let module = Rc::clone(&parent.top_mut().module);
        // Constructors get no self-name binding (the class name
        // resolves through the class scope) but keep their
        // [[Construct]] slot — they are NOT flagged is_method.
        parent.next_fn_no_self_name = true;
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

    // No self-name binding here: the class name resolves through
    // the §15.7.14 class-scope binding (an upvalue capture), so the
    // constructor body observes the full class value rather than a
    // bare re-made closure of itself.

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
        capture_lexical_environment_for_eval(parent);
        capture_private_environment_for_eval(parent);
        capture_super_bindings_for_eval(parent);
        collect_direct_eval_bindings(parent, &[])
    } else {
        Vec::new()
    };
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
    let mut direct_eval_meta = direct_eval_meta;
    crate::function_context::finalize_virtual_capture_indices(
        &mut child.code,
        &mut direct_eval_meta,
        child.own_upvalue_count,
    );
    let mut module_mut = module.borrow_mut();
    let slot = module_mut
        .functions
        .get_mut(function_id as usize)
        .expect("reserved function slot");
    slot.locals = 0;
    slot.scratch = child.scratch_window();
    slot.param_count = param_count;
    slot.length = length;
    slot.has_rest = has_rest;
    slot.is_async = is_async;
    slot.is_derived_constructor = is_derived;
    slot.own_upvalue_count = child.own_upvalue_count;
    slot.direct_eval_bindings = direct_eval_meta;
    slot.contains_direct_eval = contains_direct_eval;
    slot.code = child.code.finish();
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
    // §7.3.29 — brand the instance when the class declares private
    // methods; a second branding of the same object throws.
    if let Some(ns) = cx.private_namespaces.last().copied() {
        let binding = format!("__privbrand_{ns}");
        if cx.lookup_binding(&binding).is_some() || cx.resolve_capture(&binding).is_some() {
            let span = (0, 0);
            let key_reg = crate::class::load_synthetic_capture(cx, &binding, span)?;
            let this_reg = cx.alloc_scratch();
            cx.emit(Op::LoadThis, [Operand::Register(this_reg)], span);
            let present = cx.alloc_scratch();
            cx.emit(
                Op::HasProperty,
                [
                    Operand::Register(present),
                    Operand::Register(key_reg),
                    Operand::Register(this_reg),
                ],
                span,
            );
            let fresh = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(present), span);
            emit_field_add_type_error(cx, span);
            cx.patch_branch_to_here(fresh);
            // Brand value = the class prototype object (see
            // `__privproto_*` in class/mod.rs) so branded receivers
            // off the prototype chain still resolve private methods.
            let proto_binding = format!("__privproto_{ns}");
            let brand_value = if cx.lookup_binding(&proto_binding).is_some()
                || cx.resolve_capture(&proto_binding).is_some()
            {
                crate::class::load_synthetic_capture(cx, &proto_binding, span)?
            } else {
                let true_reg = cx.alloc_scratch();
                cx.emit(Op::LoadTrue, [Operand::Register(true_reg)], span);
                true_reg
            };
            cx.emit_store_element(this_reg, key_reg, brand_value, span);
        }
    }
    for (idx, p) in fields.iter().enumerate() {
        // Register recycling: nothing emitted for one field-init
        // survives into the next (key/value/this are all consumed by
        // the define), so a class with thousands of fields stays
        // within the u16 register window.
        let scratch_mark = cx.scratch;
        let pspan = (p.span.start, p.span.end);
        // §15.7.10 / §13.15.2 NamedEvaluation — a static-key field's
        // anonymous initializer takes the field name.
        let static_key_name = if p.computed {
            None
        } else {
            match &p.key {
                oxc_ast::ast::PropertyKey::StaticIdentifier(id) => {
                    Some(id.name.as_str().to_string())
                }
                oxc_ast::ast::PropertyKey::StringLiteral(lit) => Some(lit.value.to_string()),
                oxc_ast::ast::PropertyKey::NumericLiteral(lit) => Some(lit.value.to_string()),
                _ => None,
            }
        };
        let value_reg = match &p.value {
            Some(expr) => match &static_key_name {
                Some(name) => crate::expr::compile_expr_with_inferred_name(cx, expr, name, pspan)?,
                None => compile_expr(cx, expr, pspan)?,
            },
            None => {
                let dst = cx.alloc_scratch();
                cx.emit(Op::LoadUndefined, [Operand::Register(dst)], pspan);
                dst
            }
        };
        let this_reg = cx.alloc_scratch();
        cx.emit(Op::LoadThis, [Operand::Register(this_reg)], pspan);
        if p.computed {
            // §15.7.10 — computed-key field. The key was evaluated
            // exactly once at class-definition time (§15.7.14) into
            // a synthetic captured binding; resolve it here instead
            // of re-evaluating the expression per instance.
            let binding = crate::class::field_key_binding_name(idx);
            let key_reg = load_synthetic_capture(cx, &binding, pspan)?;
            // §15.7.10 step 4 — `[key] = AnonymousFunctionDefinition`
            // names the function from the (already canonical) key.
            if p.value
                .as_ref()
                .is_some_and(crate::expr::object_array::expression_is_anonymous_function)
            {
                let empty_idx = cx.intern_string_constant("");
                cx.emit(
                    Op::SetFunctionName,
                    [
                        Operand::Register(value_reg),
                        Operand::Register(key_reg),
                        Operand::ConstIndex(empty_idx),
                    ],
                    pspan,
                );
            }
            emit_public_field_define(cx, this_reg, key_reg, value_reg, pspan);
            cx.scratch = scratch_mark;
            continue;
        }
        let key_str = match &p.key {
            oxc_ast::ast::PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
            oxc_ast::ast::PropertyKey::StringLiteral(lit) => lit.value.to_string(),
            oxc_ast::ast::PropertyKey::NumericLiteral(lit) => lit.value.to_string(),
            oxc_ast::ast::PropertyKey::PrivateIdentifier(pid) => {
                let key_reg = load_private_key_for_field(cx, pid.name.as_str(), pspan)?;
                // §7.3.28 PrivateFieldAdd — re-initializing the same
                // private field on one object (constructor-return
                // override + second `new`) is a TypeError. Fields
                // never live on the prototype side, so a chain walk
                // is equivalent to an own-presence check here.
                let present = cx.alloc_scratch();
                cx.emit(
                    Op::HasProperty,
                    [
                        Operand::Register(present),
                        Operand::Register(key_reg),
                        Operand::Register(this_reg),
                    ],
                    pspan,
                );
                let fresh = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(present), pspan);
                emit_field_add_type_error(cx, pspan);
                cx.patch_branch_to_here(fresh);
                emit_public_field_define(cx, this_reg, key_reg, value_reg, pspan);
                cx.scratch = scratch_mark;
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
        cx.scratch = scratch_mark;
    }
    Ok(())
}

/// Throw TypeError for a duplicate PrivateFieldAdd (§7.3.28).
fn emit_field_add_type_error(cx: &mut Compiler, span: (u32, u32)) {
    let message_reg = cx.alloc_scratch();
    let message_idx =
        cx.intern_string_constant("Cannot initialize private field twice on the same object");
    cx.emit(
        Op::LoadString,
        [
            Operand::Register(message_reg),
            Operand::ConstIndex(message_idx),
        ],
        span,
    );
    let error_reg = cx.alloc_scratch();
    let kind_idx = cx.intern_string_constant("TypeError");
    cx.emit(
        Op::NewBuiltinError,
        [
            Operand::Register(error_reg),
            Operand::ConstIndex(kind_idx),
            Operand::Register(message_reg),
        ],
        span,
    );
    cx.emit(Op::Throw, [Operand::Register(error_reg)], span);
}

/// Resolve a private FIELD's symbol inside the constructor via the
/// per-class `__privarr_{ns}` array (one capture for the whole
/// class) instead of one `__privsym_*` capture per name —
/// `Op::MakeClosure` tops out at 252 captures.
fn load_private_key_for_field(
    cx: &mut Compiler,
    name: &str,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    if let (Some(ns), Some(ordered)) = (
        cx.private_namespaces.last().copied(),
        cx.class_private_ordered.last(),
    ) && let Some(idx) = ordered.iter().position(|n| n == name)
    {
        let binding = format!("__privarr_{ns}");
        if cx.lookup_binding(&binding).is_some() || cx.resolve_capture(&binding).is_some() {
            let arr_reg = load_synthetic_capture(cx, &binding, span)?;
            let idx_reg = cx.alloc_scratch();
            cx.emit(
                Op::LoadInt32,
                [Operand::Register(idx_reg), Operand::Imm32(idx as i32)],
                span,
            );
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadElement,
                vec![
                    Operand::Register(dst),
                    Operand::Register(arr_reg),
                    Operand::Register(idx_reg),
                ],
                span,
            );
            return Ok(dst);
        }
    }
    crate::class::load_private_key(cx, name, span)
}
