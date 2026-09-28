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
//! - Constructors compile through the shared function lowering, so their
//!   scopes, contexts, and a `DerivedThis` slot follow the same rules as any
//!   function.
//!
//! # See also
//! - [`super`]

use super::{SUPER_CTOR_NAME, load_synthetic_capture};
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

/// Synthesize the default constructor: an empty base-class body, or
/// `constructor(...args) { super(...args); }` for a derived class, with the
/// instance-field initializers inline.
pub(crate) fn compile_synthetic_constructor(
    parent: &mut Compiler,
    name: &str,
    is_derived: bool,
    span: (u32, u32),
    instance_fields: &[&oxc_ast::ast::PropertyDefinition<'_>],
) -> Result<ClosureRecord, CompileError> {
    let module = Rc::clone(&parent.top_mut().module);
    let mut child = FunctionContext::new(Rc::clone(&module))
        .with_strict(true)
        .with_module_url(parent.module_url.clone());
    // A direct eval inside a field initializer runs with this constructor
    // frame as its caller (§19.2.1.3).
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
    child.has_home_object = true;
    child.is_derived_ctor = is_derived;
    let parent_ctx = parent.innermost_ctx();
    child.closure_context_empty =
        parent_ctx == crate::scope::CtxReg::Closure && parent.closure_context_empty;
    parent.push(child);
    parent.enter_scope_with_flags(
        otter_bytecode::ScopeKind::Body,
        otter_bytecode::ScopeFlags {
            strict: true,
            var_scope: true,
            has_extension: false,
        },
    );

    let function_id = module.borrow().functions.len() as u32;
    module.borrow_mut().functions.push(Function {
        id: function_id,
        name: name.to_string(),
        span,
        is_strict: true,
        module_url: parent.module_url.clone(),
        ..Default::default()
    });

    let body: Result<(), CompileError> = (|| {
        if is_derived {
            // Default derived ctor is `constructor(...args) {
            // super(...args); }`. §13.3.7.1 GetSuperConstructor resolves
            // through the class's LIVE [[GetPrototypeOf]].
            let super_ctor = if parent.resolve_name(crate::class::CLASS_SELF_NAME).is_some() {
                let class_reg =
                    load_synthetic_capture(parent, crate::class::CLASS_SELF_NAME, span)?;
                let proto_reg = parent.alloc_scratch();
                parent.emit(
                    Op::GetPrototype,
                    [Operand::Register(proto_reg), Operand::Register(class_reg)],
                    span,
                );
                proto_reg
            } else {
                load_synthetic_capture(parent, SUPER_CTOR_NAME, span)?
            };
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
            emit_instance_field_inits(parent, instance_fields)?;
            parent.emit(Op::Return, [Operand::Register(dst)], span);
        } else {
            emit_instance_field_inits(parent, instance_fields)?;
            parent.emit(Op::ReturnUndefined, vec![], span);
        }
        Ok(())
    })();
    parent.exit_scope();
    let mut child = parent.pop();
    body?;
    let needs_context = finish_function(parent, &mut child, function_id, span, |slot| {
        slot.param_count = 0;
        slot.length = 0;
        slot.has_rest = is_derived;
        slot.is_derived_constructor = is_derived;
        slot.contains_direct_eval = contains_direct_eval;
    })?;
    Ok(ClosureRecord {
        function_id,
        ctx: parent_ctx,
        needs_context,
        is_arrow: false,
    })
}

/// Compile a user-written class constructor; instance fields initialize
/// inline — before parameter binding for a base class, after the
/// statement-level `super(...)` for a derived class.
///
/// # See also
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
) -> Result<ClosureRecord, CompileError> {
    // §7.3.30 — a class with instance private METHODS brands every
    // instance (the brand store lives in the field-init prologue),
    // so such constructors take the field-init path even with zero
    // fields.
    let needs_brand = parent
        .class_private_instance_methods
        .last()
        .is_some_and(|methods| !methods.is_empty());
    parent.next_fn_has_home = true;
    parent.next_fn_derived_ctor = is_derived;
    let fields = (!instance_fields.is_empty() || needs_brand).then_some(FieldInjection {
        fields: instance_fields,
        is_derived,
    });
    compile_function_impl(
        parent,
        FunctionSpec {
            name,
            params,
            body: body.as_deref(),
            span,
            is_async,
            is_generator: false,
            force_strict: true,
            is_arrow: false,
            arrow_expression: false,
            nfe_self: false,
            fields,
        },
    )
}

/// Emit the instance brand and every field initializer against `this`
/// per §15.7.10 InitializeFieldsForReceiver.
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
    if let Some(location) = cx.class_scope_locations.last().copied()
        && cx
            .resolve_at(location, crate::class::PRIVATE_BRAND_BINDING)
            .is_some()
    {
        let span = (0, 0);
        let key_reg = cx
            .load_at(location, crate::class::PRIVATE_BRAND_BINDING, span)
            .expect("brand slot resolved above");
        let this_reg = cx.alloc_scratch();
        cx.emit_load_this(this_reg, span);
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
        // `PRIVATE_PROTO_BINDING`) so branded receivers off the
        // prototype chain still resolve private methods.
        let brand_value = match cx.load_at(location, crate::class::PRIVATE_PROTO_BINDING, span) {
            Some(reg) => reg,
            None => {
                let true_reg = cx.alloc_scratch();
                cx.emit(Op::LoadTrue, [Operand::Register(true_reg)], span);
                true_reg
            }
        };
        cx.emit_store_element(this_reg, key_reg, brand_value, span);
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
                oxc_ast::ast::PropertyKey::NumericLiteral(lit) => {
                    Some(crate::class::number_literal_property_name(lit.value))
                }
                oxc_ast::ast::PropertyKey::BigIntLiteral(lit) => {
                    crate::expr::literal::bigint_literal_property_name(lit)
                }
                // §15.7.10 NamedEvaluation with a private name: the
                // description keeps the `#` sigil.
                oxc_ast::ast::PropertyKey::PrivateIdentifier(pid) => {
                    Some(format!("#{}", pid.name.as_str()))
                }
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
        cx.emit_load_this(this_reg, pspan);
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
            cx.reset_scratch(scratch_mark);
            continue;
        }
        let key_str = match &p.key {
            oxc_ast::ast::PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
            oxc_ast::ast::PropertyKey::StringLiteral(lit) => lit.value.to_string(),
            oxc_ast::ast::PropertyKey::NumericLiteral(lit) => {
                crate::class::number_literal_property_name(lit.value)
            }
            oxc_ast::ast::PropertyKey::BigIntLiteral(lit) => {
                // §13.2.5.5 — a BigInt key becomes ToString(BigInt)
                // (always decimal, base-independent).
                match crate::expr::literal::bigint_literal_property_name(lit) {
                    Some(name) => name,
                    None => {
                        return Err(CompileError::Unsupported {
                            node: "ClassDeclaration: invalid BigInt field key".to_string(),
                            span: pspan,
                        });
                    }
                }
            }
            oxc_ast::ast::PropertyKey::PrivateIdentifier(pid) => {
                let key_reg = crate::class::load_private_key(cx, pid.name.as_str(), pspan)?;
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
                cx.reset_scratch(scratch_mark);
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
        cx.reset_scratch(scratch_mark);
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
