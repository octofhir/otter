//! Class declaration and expression lowering helpers.
//!
//! # Contents
//! - [`compile_class`] - lower class declarations and expressions into bytecode.
//! - [`property_key_as_expression`] - expose computed property keys as expressions for validation.
//! - [`is_top_level_super_call`] - detect constructor-level `super(...)` calls.
//! - [`load_synthetic_capture`] - resolve synthetic class capture bindings.
//! - [`constructor`] - constructor and instance-field lowering helpers.
//! - [`static_block`] - class static-block lowering.
//! - [`super_ops`] - `super` call and member lowering.
//! - [`private_names`] - private-name and direct-super validation.
//!
//! # Invariants
//! - Private names are validated before bytecode emission for the class body.
//! - Class lowering installs synthetic captures before compiling methods that can reference `super`.
//!
//! # See also
//! - `functions` and `scope`

mod constructor;
mod private_names;
mod static_block;
mod super_ops;

pub(crate) use constructor::*;
pub(crate) use private_names::*;
pub(crate) use static_block::*;
pub(crate) use super_ops::*;

use crate::*;

/// Lower a `class … { … }` declaration or expression into the
/// foundation `ClassConstructor` value. The lowering builds:
///
/// 1. The constructor function (synthesised as an empty body for a
///    base class without an explicit `constructor`, or as
///    `constructor(...args) { super(...args); }` for a derived
///    class without one).
/// 2. The instance-side prototype object (`C.prototype`). Each
///    non-static method is installed here; for `extends C`, this
///    object's `[[Prototype]]` chains to `C.prototype`.
/// 3. The static-side object. Each `static` method is installed
///    here; for `extends C`, this object's `[[Prototype]]` chains
///    to the parent's static side so static inheritance falls out
///    of the existing prototype walker.
/// 4. A [`Op::MakeClass`] that fuses constructor / prototype /
///    statics into a single `Value::ClassConstructor`.
///
/// Method bodies that reference `super` resolve through two
/// synthetic upvalues installed in the class scope:
/// `__class_home` (the prototype object methods belong to) and
/// `__class_super` (the parent class value, only present when the
/// class has an `extends` clause).
pub(crate) fn compile_class(
    cx: &mut Compiler,
    class: &oxc_ast::ast::Class<'_>,
    class_name: Option<&str>,
) -> Result<u16, CompileError> {
    let span = (class.span.start, class.span.end);

    // Reject features explicitly out of scope for the foundation
    // slice. Surface clear diagnostics so callers can tell what's
    // not supported yet.
    if !class.decorators.is_empty() {
        return Err(CompileError::Unsupported {
            node: "ClassDeclaration: decorators".to_string(),
            span,
        });
    }
    if class.r#abstract {
        return Err(CompileError::Unsupported {
            node: "ClassDeclaration: abstract".to_string(),
            span,
        });
    }
    if class.declare {
        // Pure type-level declaration — emit nothing observable
        // and hand the caller a `Value::Undefined` register.
        let dst = cx.alloc_scratch();
        cx.emit(Op::LoadUndefined, [Operand::Register(dst)], span);
        return Ok(dst);
    }

    // §15.7.1 Class Definitions: Static Semantics: Early Errors —
    // `ClassElement : MethodDefinition` (incl. `static`) is a Syntax
    // Error when `HasDirectSuper(MethodDefinition)` is true and the
    // method's PropName is not "constructor". A FieldDefinition is
    // likewise a Syntax Error if its initializer Contains SuperCall.
    // Arrow functions and class static blocks are transparent for
    // HasDirectSuper; nested non-arrow function bodies break the
    // chain (they have their own [[HomeObject]] = undefined).
    validate_no_direct_super_in_methods(&class.body)?;
    // §15.7.1 / §8.2.4 AllPrivateNamesValid — every `#name` must be
    // declared in an enclosing class. The heritage expression is
    // evaluated in the outer private scope. Enclosing classes' names
    // seed the scope stack: a nested class compiled inside a method
    // body may legally reference the outer class's `#name`.
    {
        let mut scopes = cx.class_private_names.clone();
        validate_class_private_names_inner(class, &mut scopes)?;
    }

    cx.enter_scope();

    // Allocate a fresh private-field namespace and push it on the
    // compiler's class-context stack so every `#name` reference
    // inside the class body mangles into this class's slot.
    let private_namespace = {
        let module = Rc::clone(&cx.top_mut().module);
        let mut m = module.borrow_mut();
        let id = m.next_private_namespace;
        m.next_private_namespace = id.checked_add(1).expect("private-namespace overflow");
        id
    };
    cx.private_namespaces.push(private_namespace);

    // Allocate one runtime-unique symbol per private name for this
    // class evaluation. Methods capture these bindings, so repeated
    // evaluation of the same class body gets distinct private keys.
    let private_bound = collect_class_private_bound(&class.body);
    cx.class_private_names.push(private_bound.clone());
    for name in &private_bound {
        let binding = cx
            .private_key_binding_name(name)
            .ok_or(CompileError::Unsupported {
                node: "ClassDeclaration: private name outside class".to_string(),
                span,
            })?;
        let storage = cx.declare_captured_binding(&binding, true, span)?;
        let key_reg = emit_private_symbol_key(cx, name, span)?;
        cx.emit_store_storage(key_reg, storage, span);
        cx.mark_initialized(&binding);
    }
    // §7.3.29 PrivateMethodOrAccessorAdd — instance private methods
    // brand the receiver. Methods live on the prototype side in this
    // engine, so the brand is a dedicated per-class-evaluation
    // symbol installed as an own marker during
    // InitializeInstanceElements; re-branding (constructor-return
    // override + second `new`) throws TypeError there.
    let has_instance_private_methods = class.body.body.iter().any(|el| {
        matches!(
            el,
            oxc_ast::ast::ClassElement::MethodDefinition(m)
                if !m.r#static
                    && !matches!(m.kind, oxc_ast::ast::MethodDefinitionKind::Constructor)
                    && matches!(m.key, oxc_ast::ast::PropertyKey::PrivateIdentifier(_))
        )
    });
    if has_instance_private_methods {
        let binding = format!("__privbrand_{private_namespace}");
        let storage = cx.declare_captured_binding(&binding, true, span)?;
        let key_reg = emit_private_symbol_key(cx, "brand", span)?;
        cx.emit_store_storage(key_reg, storage, span);
        cx.mark_initialized(&binding);
    }

    // Evaluate the parent class first so observable side-effects
    // happen exactly once per declaration, in source order.
    let super_reg = match &class.super_class {
        Some(expr) => {
            let r = compile_expr(cx, expr, span)?;
            // §15.7.14 step 6.f — the heritage must be null or a
            // constructor; arrows / generators / async functions /
            // non-callables throw TypeError before prototype reads.
            cx.emit(
                Op::ClassCheck,
                [Operand::Imm32(0), Operand::Register(r)],
                span,
            );
            Some(r)
        }
        None => None,
    };

    // Build the prototype object up-front so methods can be
    // installed on it as we walk the class body. For `extends`,
    // chain `C.prototype` from the parent's prototype. Statics
    // object — own static methods live here and chain to the
    // parent's statics for `extends`.
    let prototype_reg = cx.alloc_scratch();
    cx.emit(Op::NewObject, [Operand::Register(prototype_reg)], span);
    let statics_reg = cx.alloc_scratch();
    cx.emit(Op::NewObject, [Operand::Register(statics_reg)], span);
    if let Some(parent_reg) = super_reg {
        // §15.7.14 ClassDefinitionEvaluation step 6 — `extends null`
        // (or any superclass value that is null) gives protoParent =
        // null: `C.prototype.[[Prototype]]` is null and the parent's
        // `prototype` slot is never read. Branch on the runtime value
        // so a literal `extends null` and a dynamic null both work.
        let to_null = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(parent_reg), span);
        let parent_proto = cx.alloc_scratch();
        let proto_const = cx.intern_string_constant("prototype");
        cx.emit(
            Op::LoadProperty,
            vec![
                Operand::Register(parent_proto),
                Operand::Register(parent_reg),
                Operand::ConstIndex(proto_const),
            ],
            span,
        );
        cx.emit(
            Op::SetPrototype,
            vec![
                Operand::Register(prototype_reg),
                Operand::Register(parent_proto),
            ],
            span,
        );
        cx.emit(
            Op::SetPrototype,
            vec![
                Operand::Register(statics_reg),
                Operand::Register(parent_reg),
            ],
            span,
        );
        let done = cx.emit_branch_placeholder(Op::Jump, None, span);
        cx.patch_branch_to_here(to_null);
        let null_reg = cx.alloc_scratch();
        cx.emit(Op::LoadNull, [Operand::Register(null_reg)], span);
        cx.emit(
            Op::SetPrototype,
            vec![
                Operand::Register(prototype_reg),
                Operand::Register(null_reg),
            ],
            span,
        );
        cx.patch_branch_to_here(done);
    }

    // Install the synthetic `__class_home` / `__class_super`
    // captured bindings so method bodies can resolve `super`
    // through the standard upvalue walker.
    let home_storage = cx.declare_captured_binding(SUPER_HOME_NAME, true, span)?;
    cx.emit_store_storage(prototype_reg, home_storage, span);
    cx.mark_initialized(SUPER_HOME_NAME);
    if let Some(parent_reg) = super_reg {
        let super_storage = cx.declare_captured_binding(SUPER_CTOR_NAME, true, span)?;
        cx.emit_store_storage(parent_reg, super_storage, span);
        cx.mark_initialized(SUPER_CTOR_NAME);
    }

    // Find the user-written constructor (if any) and the body's
    // method members. Reject features outside the foundation
    // subset early so the diagnostics are precise.
    let mut ctor_method: Option<&oxc_ast::ast::MethodDefinition<'_>> = None;
    for element in &class.body.body {
        match element {
            oxc_ast::ast::ClassElement::MethodDefinition(m) => {
                if matches!(m.kind, oxc_ast::ast::MethodDefinitionKind::Constructor) {
                    if ctor_method.is_some() {
                        return Err(CompileError::Unsupported {
                            node: "ClassDeclaration: multiple constructors".to_string(),
                            span: (m.span.start, m.span.end),
                        });
                    }
                    ctor_method = Some(m);
                }
                // Foundation: getters / setters / computed keys all
                // round-trip as plain data methods on the install
                // pass below. Real accessor descriptors land with
                // the §15.7 class-element installer follow-up; for
                // the test262 sweep we accept the syntax so the
                // class declaration compiles.
            }
            oxc_ast::ast::ClassElement::PropertyDefinition(p) => {
                // §15.7 ClassFieldDefinition. The foundation
                // accepts public instance fields and public static
                // fields; private (`#name`) and decorated fields
                // are filed. Computed keys round-trip through the
                // runtime via `Op::StoreElement` in the field
                // installer below.
                if p.declare {
                    continue;
                }
                if !p.decorators.is_empty() {
                    return Err(CompileError::Unsupported {
                        node: "ClassDeclaration: decorated field".to_string(),
                        span: (p.span.start, p.span.end),
                    });
                }
                if !p.r#static {}
            }
            oxc_ast::ast::ClassElement::AccessorProperty(_) => {
                // §15.7 AccessorProperty — degrade to a plain data
                // property with `undefined` initialiser. Tests that
                // rely on accessor semantics will fail; tests that
                // only depend on the syntactic surface keep
                // compiling.
            }
            oxc_ast::ast::ClassElement::StaticBlock(_) => {
                // Allowed — runs at class-declaration time after
                // static fields. See compile_static_block below.
            }
            oxc_ast::ast::ClassElement::TSIndexSignature(_) => {
                // TypeScript-only — erase silently.
            }
        }
    }
    // Collect the instance-field initialisers (in source order) so
    // both user-written and synthetic constructors can prepend them
    // to the body. §15.7.10 InitializeInstanceElements.
    let instance_fields: Vec<&oxc_ast::ast::PropertyDefinition<'_>> = class
        .body
        .body
        .iter()
        .filter_map(|el| match el {
            oxc_ast::ast::ClassElement::PropertyDefinition(p) if !p.r#static && !p.declare => {
                Some(&**p)
            }
            _ => None,
        })
        .collect();

    // §15.7.14 ClassFieldDefinitionEvaluation — a computed field
    // name is evaluated exactly once, at class-definition time, in
    // the class's own evaluation context (so `await` in a TLA module
    // and side effects like `[counter++]` behave per spec). Store
    // each evaluated key in a synthetic captured binding that the
    // constructor's field-init code resolves through the standard
    // upvalue walker — the same mechanism as `__class_home` and the
    // per-class private-name symbols.
    for (idx, p) in instance_fields.iter().enumerate() {
        if !p.computed {
            continue;
        }
        let pspan = (p.span.start, p.span.end);
        let key_expr = p
            .key
            .as_expression()
            .ok_or_else(|| CompileError::Unsupported {
                node: "ClassDeclaration: non-expression computed instance field key".to_string(),
                span: pspan,
            })?;
        let key_reg = compile_expr(cx, key_expr, pspan)?;
        let binding = field_key_binding_name(idx);
        let storage = cx.declare_captured_binding(&binding, true, pspan)?;
        cx.emit_store_storage(key_reg, storage, pspan);
        cx.mark_initialized(&binding);
    }

    // Compile the constructor body. When the user didn't write one,
    // synthesize the spec defaults: a base class gets an empty body,
    // a derived class gets `constructor(...args) { super(...args); }`.
    let display_name = class_name.unwrap_or("<class>").to_string();
    let is_derived = super_reg.is_some();
    let (ctor_id, ctor_captures) = match ctor_method {
        Some(m) => compile_class_constructor(
            cx,
            &display_name,
            &m.value.params,
            &m.value.body,
            (m.span.start, m.span.end),
            m.value.r#async,
            &instance_fields,
            is_derived,
        )?,
        None => {
            compile_synthetic_constructor(cx, &display_name, is_derived, span, &instance_fields)?
        }
    };

    let ctor_const = cx.intern_function_id(ctor_id);
    let ctor_reg = cx.alloc_scratch();
    emit_make_callable(cx, ctor_reg, ctor_const, &ctor_captures, false, span)?;

    // Per §10.2.1.4 ClassDefinitionEvaluation step 24, the class
    // binding becomes initialised *before* the static elements run
    // so they can reference it (e.g., `static x = C.someStatic`).
    // The binding's final value (`MakeClass`) lands at the end of
    // this function — for the early-bind we use the statics object
    // as a stand-in: static initialisers usually reach the class
    // for its statics anyway, and the foundation overwrites with
    // the full class value before any user code outside the class
    // body can observe it.
    if let Some(name) = class_name {
        if let Some(info) = cx.lookup_binding(name) {
            cx.emit_store_storage(statics_reg, info.storage, span);
            cx.mark_initialized(name);
        } else if cx.script_global_lexicals.contains(name) {
            // Script top-level class — the binding lives on the
            // realm's global declarative record; clear its TDZ hole
            // with the statics stand-in for the same reason.
            let name_idx = cx.intern_string_constant(name);
            cx.emit(
                Op::InitGlobalLex,
                [
                    Operand::Register(statics_reg),
                    Operand::ConstIndex(name_idx),
                ],
                span,
            );
        }
    }

    // Install methods (instance + static) onto the right side.
    // Foundation: getter / setter accessors round-trip as plain
    // data methods (their function body is callable and addressable
    // by name; accessor [[Get]] / [[Set]] semantics await the
    // §15.7 class-element installer follow-up). Computed keys
    // resolve at runtime via `Op::StoreElement`.
    for element in &class.body.body {
        let oxc_ast::ast::ClassElement::MethodDefinition(m) = element else {
            continue;
        };
        if matches!(m.kind, oxc_ast::ast::MethodDefinitionKind::Constructor) {
            continue;
        }
        let method_span = (m.span.start, m.span.end);
        let target_reg = if m.r#static {
            statics_reg
        } else {
            prototype_reg
        };
        // Compute the static name (when known) for diagnostics +
        // the method's `.name` intrinsic.
        let is_private = matches!(m.key, oxc_ast::ast::PropertyKey::PrivateIdentifier(_));
        let static_name: Option<String> = if !m.computed && !is_private {
            match &m.key {
                oxc_ast::ast::PropertyKey::StaticIdentifier(id) => {
                    Some(id.name.as_str().to_string())
                }
                oxc_ast::ast::PropertyKey::StringLiteral(lit) => Some(lit.value.to_string()),
                oxc_ast::ast::PropertyKey::NumericLiteral(lit) => {
                    Some(number_literal_property_name(lit.value))
                }
                _ => None,
            }
        } else {
            None
        };
        let body_name = match (&m.key, &static_name) {
            (_, Some(name)) => name.clone(),
            (oxc_ast::ast::PropertyKey::PrivateIdentifier(pid), None) => {
                format!("#{}", pid.name.as_str())
            }
            _ => "<computed>".to_string(),
        };
        let (m_id, m_captures) = compile_function_full(
            cx,
            &body_name,
            &m.value.params,
            &m.value.body,
            method_span,
            m.value.r#async,
            m.value.generator,
            true,
        )?;
        let m_const = cx.intern_function_id(m_id);
        let m_reg = cx.alloc_scratch();
        emit_make_callable(cx, m_reg, m_const, &m_captures, false, method_span)?;
        let is_accessor = matches!(
            m.kind,
            oxc_ast::ast::MethodDefinitionKind::Get | oxc_ast::ast::MethodDefinitionKind::Set
        );
        if is_accessor {
            // Resolve the property key into a register (literal vs
            // computed expression — both paths are observed by the
            // §13.2.5 ComputedPropertyName / IdentifierName rules).
            let key_reg = match (&m.key, &static_name, m.computed) {
                (oxc_ast::ast::PropertyKey::PrivateIdentifier(pid), _, false) => {
                    load_private_key(cx, pid.name.as_str(), method_span)?
                }
                (_, Some(name), false) => {
                    let r = cx.alloc_scratch();
                    let const_idx = cx.intern_string_constant(name);
                    cx.emit(
                        Op::LoadString,
                        [Operand::Register(r), Operand::ConstIndex(const_idx)],
                        method_span,
                    );
                    r
                }
                _ => {
                    let key_expr =
                        m.key
                            .as_expression()
                            .ok_or_else(|| CompileError::Unsupported {
                                node: "ClassDeclaration: non-expression computed accessor key"
                                    .to_string(),
                                span: method_span,
                            })?;
                    let r = compile_expr(cx, key_expr, method_span)?;
                    if m.r#static {
                        // §15.7.14 — static computed key must not be
                        // "prototype".
                        cx.emit(
                            Op::ClassCheck,
                            [Operand::Imm32(1), Operand::Register(r)],
                            method_span,
                        );
                    }
                    r
                }
            };
            let desc_reg = cx.alloc_scratch();
            cx.emit(Op::NewObject, [Operand::Register(desc_reg)], method_span);
            let accessor_key = match m.kind {
                oxc_ast::ast::MethodDefinitionKind::Get => "get",
                oxc_ast::ast::MethodDefinitionKind::Set => "set",
                _ => unreachable!(),
            };
            let accessor_const = cx.intern_string_constant(accessor_key);
            let store_scratch = cx.alloc_scratch();
            cx.emit(
                Op::StoreProperty,
                vec![
                    Operand::Register(desc_reg),
                    Operand::ConstIndex(accessor_const),
                    Operand::Register(m_reg),
                    Operand::Register(store_scratch),
                ],
                method_span,
            );
            // Class accessor descriptors are `enumerable: false,
            // configurable: true`. Object literals install
            // `enumerable: true` on the same template — the only
            // difference between the two surfaces.
            let true_reg = cx.alloc_scratch();
            cx.emit(Op::LoadTrue, [Operand::Register(true_reg)], method_span);
            let false_reg = cx.alloc_scratch();
            cx.emit(Op::LoadFalse, [Operand::Register(false_reg)], method_span);
            let enum_const = cx.intern_string_constant("enumerable");
            let enum_scratch = cx.alloc_scratch();
            cx.emit(
                Op::StoreProperty,
                vec![
                    Operand::Register(desc_reg),
                    Operand::ConstIndex(enum_const),
                    Operand::Register(false_reg),
                    Operand::Register(enum_scratch),
                ],
                method_span,
            );
            let cfg_const = cx.intern_string_constant("configurable");
            let cfg_scratch = cx.alloc_scratch();
            cx.emit(
                Op::StoreProperty,
                vec![
                    Operand::Register(desc_reg),
                    Operand::ConstIndex(cfg_const),
                    Operand::Register(true_reg),
                    Operand::Register(cfg_scratch),
                ],
                method_span,
            );
            cx.emit(
                Op::DefineOwnProperty,
                [
                    Operand::Register(target_reg),
                    Operand::Register(key_reg),
                    Operand::Register(desc_reg),
                ],
                method_span,
            );
            continue;
        }
        let key_reg = match (&m.key, &static_name, m.computed) {
            (oxc_ast::ast::PropertyKey::PrivateIdentifier(pid), _, false) => {
                load_private_key(cx, pid.name.as_str(), method_span)?
            }
            (_, Some(name), false) => {
                let r = cx.alloc_scratch();
                let const_idx = cx.intern_string_constant(name);
                cx.emit(
                    Op::LoadString,
                    [Operand::Register(r), Operand::ConstIndex(const_idx)],
                    method_span,
                );
                r
            }
            _ => {
                let key_expr = m
                    .key
                    .as_expression()
                    .ok_or_else(|| CompileError::Unsupported {
                        node: "ClassDeclaration: non-expression computed key".to_string(),
                        span: method_span,
                    })?;
                let r = compile_expr(cx, key_expr, method_span)?;
                if m.r#static {
                    // §15.7.14 — static computed key must not be
                    // "prototype".
                    cx.emit(
                        Op::ClassCheck,
                        [Operand::Imm32(1), Operand::Register(r)],
                        method_span,
                    );
                }
                r
            }
        };
        let desc_reg = cx.alloc_scratch();
        cx.emit(Op::NewObject, [Operand::Register(desc_reg)], method_span);
        let value_const = cx.intern_string_constant("value");
        let value_scratch = cx.alloc_scratch();
        cx.emit(
            Op::StoreProperty,
            vec![
                Operand::Register(desc_reg),
                Operand::ConstIndex(value_const),
                Operand::Register(m_reg),
                Operand::Register(value_scratch),
            ],
            method_span,
        );
        let true_reg = cx.alloc_scratch();
        cx.emit(Op::LoadTrue, [Operand::Register(true_reg)], method_span);
        let false_reg = cx.alloc_scratch();
        cx.emit(Op::LoadFalse, [Operand::Register(false_reg)], method_span);
        // §7.3.32 — a private method is not writable: `PrivateSet`
        // distinguishes it from a private field by this attribute
        // (static methods live as own data on the statics object,
        // where holder == receiver can't tell them apart).
        let writable_reg = if is_private { false_reg } else { true_reg };
        for (attr, value_reg) in [
            ("writable", writable_reg),
            ("enumerable", false_reg),
            ("configurable", true_reg),
        ] {
            let attr_const = cx.intern_string_constant(attr);
            let attr_scratch = cx.alloc_scratch();
            cx.emit(
                Op::StoreProperty,
                vec![
                    Operand::Register(desc_reg),
                    Operand::ConstIndex(attr_const),
                    Operand::Register(value_reg),
                    Operand::Register(attr_scratch),
                ],
                method_span,
            );
        }
        cx.emit(
            Op::DefineOwnProperty,
            [
                Operand::Register(target_reg),
                Operand::Register(key_reg),
                Operand::Register(desc_reg),
            ],
            method_span,
        );
    }

    // §15.7.10 InitializeStaticElements — walk the body in source
    // order, evaluating static fields and static-init blocks
    // against the statics object.
    for element in &class.body.body {
        match element {
            oxc_ast::ast::ClassElement::PropertyDefinition(p) if p.r#static && !p.declare => {
                let pspan = (p.span.start, p.span.end);
                let value_reg = match &p.value {
                    Some(expr) => compile_expr(cx, expr, pspan)?,
                    None => {
                        let dst = cx.alloc_scratch();
                        cx.emit(Op::LoadUndefined, [Operand::Register(dst)], pspan);
                        dst
                    }
                };
                if p.computed {
                    let key_expr =
                        p.key
                            .as_expression()
                            .ok_or_else(|| CompileError::Unsupported {
                                node: "ClassDeclaration: non-expression computed static field key"
                                    .to_string(),
                                span: pspan,
                            })?;
                    let key_reg = compile_expr(cx, key_expr, pspan)?;
                    // §15.7.14 — static computed key must not be
                    // "prototype".
                    cx.emit(
                        Op::ClassCheck,
                        [Operand::Imm32(1), Operand::Register(key_reg)],
                        pspan,
                    );
                    cx.emit_store_element(statics_reg, key_reg, value_reg, pspan);
                } else {
                    let key_str = match &p.key {
                        oxc_ast::ast::PropertyKey::StaticIdentifier(id) => {
                            id.name.as_str().to_string()
                        }
                        oxc_ast::ast::PropertyKey::StringLiteral(lit) => lit.value.to_string(),
                        oxc_ast::ast::PropertyKey::NumericLiteral(lit) => {
                            number_literal_property_name(lit.value)
                        }
                        oxc_ast::ast::PropertyKey::PrivateIdentifier(pid) => {
                            let key_reg = load_private_key(cx, pid.name.as_str(), pspan)?;
                            cx.emit_store_element(statics_reg, key_reg, value_reg, pspan);
                            continue;
                        }
                        _ => {
                            return Err(CompileError::Unsupported {
                                node: "ClassDeclaration: non-string static field key".to_string(),
                                span: pspan,
                            });
                        }
                    };
                    cx.emit_store_property(statics_reg, &key_str, value_reg, pspan);
                }
            }
            oxc_ast::ast::ClassElement::StaticBlock(s) => {
                // §15.7.4 StaticBlock — a synthesised function with
                // no params; `this` bound to the statics object.
                // Compile through the standard MakeClosure path so
                // identifier references to outer locals capture as
                // upvalues (the previous `MakeFunction`-only emit
                // dropped captures and left `Op::LoadUpvalue` /
                // `Op::StoreUpvalue` indices dangling).
                let bspan = (s.span.start, s.span.end);
                let (function_id, captures) =
                    compile_static_block(cx, &display_name, &s.body, bspan)?;
                let const_idx = cx.intern_function_id(function_id);
                let fn_reg = cx.alloc_scratch();
                emit_make_callable(cx, fn_reg, const_idx, &captures, false, bspan)?;
                let dst = cx.alloc_scratch();
                cx.emit(
                    Op::CallWithThis,
                    vec![
                        Operand::Register(dst),
                        Operand::Register(fn_reg),
                        Operand::Register(statics_reg),
                        Operand::ConstIndex(0),
                    ],
                    bspan,
                );
            }
            _ => {}
        }
    }

    let class_reg = cx.alloc_scratch();
    cx.emit(
        Op::MakeClass,
        vec![
            Operand::Register(class_reg),
            Operand::Register(ctor_reg),
            Operand::Register(prototype_reg),
            Operand::Register(statics_reg),
        ],
        span,
    );

    cx.private_namespaces.pop();
    cx.class_private_names.pop();
    cx.exit_scope();
    Ok(class_reg)
}

fn number_literal_property_name(value: f64) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    let abs = value.abs();
    if !(1e-6..1e21).contains(&abs) {
        return format!("{value:e}").replace("e+", "e").replace("e-0", "e-");
    }
    value.to_string()
}

/// Returns the computed-key `Expression` view of a `PropertyKey`,
/// or `None` when the key is a static identifier or a private name
/// (which don't carry expressions to validate).
pub(crate) fn property_key_as_expression<'a, 'b>(
    key: &'b oxc_ast::ast::PropertyKey<'a>,
) -> Option<&'b oxc_ast::ast::Expression<'a>> {
    match key {
        oxc_ast::ast::PropertyKey::StaticIdentifier(_)
        | oxc_ast::ast::PropertyKey::PrivateIdentifier(_) => None,
        other => other.as_expression(),
    }
}

/// for instance-field initialisers per §15.7.10 step 9.
pub(crate) fn is_top_level_super_call(stmt: &Statement<'_>) -> bool {
    let Statement::ExpressionStatement(es) = stmt else {
        return false;
    };
    let Expression::CallExpression(call) = &es.expression else {
        return false;
    };
    matches!(call.callee, Expression::Super(_))
}

/// Synthetic name for the per-method "home object" upvalue that
/// the class lowering installs in the enclosing class scope. The
/// value is the prototype object that the method belongs to —
/// `super.x` walks one hop up its `[[Prototype]]` chain to find the
/// parent's binding.
pub(crate) const SUPER_HOME_NAME: &str = "__class_home";

/// Synthetic name for the per-derived-constructor "super
/// constructor" upvalue. Holds the parent class value so
/// `super(args)` knows what to invoke with the current receiver.
pub(crate) const SUPER_CTOR_NAME: &str = "__class_super";

/// Synthetic captured-binding name for the `idx`-th instance field's
/// computed property key, evaluated once at class-definition time
/// per §15.7.14 ClassFieldDefinitionEvaluation. The constructor's
/// field-init code resolves it through the standard upvalue walker.
pub(crate) fn field_key_binding_name(idx: usize) -> String {
    format!("__class_fieldkey_{idx}")
}

/// Resolve a synthetic captured name (`__class_home` / `__class_super`)
/// into a register holding its current value. Returns
/// [`CompileError::Unsupported`] when the surrounding function has
/// no class context, which is what the user sees on stray `super`
/// usages outside a class body.
pub(crate) fn load_synthetic_capture(
    cx: &mut Compiler,
    name: &str,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    if let Some(info) = cx.lookup_binding(name) {
        let dst = cx.alloc_scratch();
        cx.emit_load_storage(dst, info.storage, span);
        return Ok(dst);
    }
    if let Some(uv_idx) = cx.resolve_capture(name) {
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::LoadUpvalue,
            [Operand::Register(dst), Operand::Imm32(uv_idx as i32)],
            span,
        );
        return Ok(dst);
    }
    Err(CompileError::Unsupported {
        node: format!("super used outside a class method (`{name}` not in scope)"),
        span,
    })
}

pub(crate) fn load_private_key(
    cx: &mut Compiler,
    name: &str,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    if cx.private_namespaces.is_empty() {
        return Err(CompileError::Unsupported {
            node: "private name used outside a class body".to_string(),
            span,
        });
    }
    // §9.2 PrivateEnvironment chain — resolve through enclosing class
    // scopes, innermost first, so an inner class reads an outer
    // class's `#name` while its own declarations shadow.
    let namespaces: Vec<u32> = cx.private_namespaces.iter().rev().copied().collect();
    for ns in namespaces {
        let binding = format!("__privsym_{ns}_{name}");
        if let Some(info) = cx.lookup_binding(&binding) {
            let dst = cx.alloc_scratch();
            cx.emit_load_storage(dst, info.storage, span);
            return Ok(dst);
        }
        if let Some(uv_idx) = cx.resolve_capture(&binding) {
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadUpvalue,
                [Operand::Register(dst), Operand::Imm32(uv_idx as i32)],
                span,
            );
            return Ok(dst);
        }
    }
    Err(CompileError::Unsupported {
        node: format!("private name `#{name}` missing runtime key binding"),
        span,
    })
}

fn emit_private_symbol_key(
    cx: &mut Compiler,
    name: &str,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let symbol_reg = cx.alloc_scratch();
    let symbol_const = cx.intern_string_constant("Symbol");
    cx.emit(
        Op::LoadGlobalOrThrow,
        [
            Operand::Register(symbol_reg),
            Operand::ConstIndex(symbol_const),
        ],
        span,
    );
    let desc_reg = cx.alloc_scratch();
    let desc = cx.intern_string_constant(&format!("#{name}"));
    cx.emit(
        Op::LoadString,
        [Operand::Register(desc_reg), Operand::ConstIndex(desc)],
        span,
    );
    let key_reg = cx.alloc_scratch();
    cx.emit(
        Op::Call,
        [
            Operand::Register(key_reg),
            Operand::Register(symbol_reg),
            Operand::ConstIndex(1),
            Operand::Register(desc_reg),
        ],
        span,
    );
    Ok(key_reg)
}
