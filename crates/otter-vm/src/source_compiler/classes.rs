//! Class declaration / expression lowering.
//!
//! Extracted from `source_compiler/mod.rs` in the file-size cleanup
//! follow-up after C4. Covers the public entry points
//! `lower_nested_class_declaration` and `lower_class_expression`, the
//! shared `lower_class_body_core` machinery, and the four synthesised
//! bytecode generators (static blocks, field initializers, derived
//! default constructors, empty constructors). The internal
//! `ClassMethod` / `ClassField` / `PrivateDecl` descriptors live
//! alongside this code because no other module needs them.

use super::*;

struct ClassMethod<'a> {
    name: String,
    is_static: bool,
    is_private: bool,
    kind: MethodDefinitionKind,
    func: &'a Function<'a>,
}

/// M29: one class field declaration. Represents both public
/// (`x = expr;`) and private (`#x = expr;`) fields, instance and
/// static. The initializer lives on the AST and is lowered inside
/// the class body's field-initializer closure (or inline for
/// statics).
struct ClassField<'a> {
    /// Field name without the leading `#` prefix for private fields.
    name: String,
    /// `true` when the declaration used `#` (private element).
    is_private: bool,
    /// Optional initializer expression. Absent initializers
    /// default to `undefined` per §15.7.14.
    initializer: Option<&'a Expression<'a>>,
    span: Span,
}

/// M29.5: per-private-name declaration bookkeeping for
/// §15.7.11's early-error check. `Getter`/`Setter` merge into
/// `GetterSetter` when both halves are seen; every other
/// collision is a duplicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrivateDecl {
    Field,
    Method,
    Getter,
    Setter,
    GetterSetter,
}

/// Validates a new private-name declaration against the running
/// `private_decls` list. Returns Ok when the declaration is
/// either fresh or the complementary half of an existing
/// getter/setter pair; returns `duplicate_private_name` otherwise.
fn record_private_decl(
    decls: &mut Vec<(String, PrivateDecl)>,
    name: &str,
    new_kind: PrivateDecl,
    span: Span,
) -> Result<(), SourceLoweringError> {
    if let Some(slot) = decls.iter_mut().find(|(n, _)| n == name) {
        let merged = match (slot.1, new_kind) {
            (PrivateDecl::Getter, PrivateDecl::Setter)
            | (PrivateDecl::Setter, PrivateDecl::Getter) => PrivateDecl::GetterSetter,
            _ => {
                return Err(SourceLoweringError::unsupported(
                    "duplicate_private_name",
                    span,
                ));
            }
        };
        slot.1 = merged;
    } else {
        decls.push((name.to_owned(), new_kind));
    }
    Ok(())
}

/// Lowers a nested `class Foo { … }` declaration into a const
/// binding of a constructor closure with methods installed on
/// its prototype / static properties. M27 surface:
/// - Explicit `constructor(args) { body }` or synthesised empty
///   constructor if absent.
/// - Instance methods (installed on `Foo.prototype`).
/// - Static methods (installed on `Foo` itself).
/// - Computed keys, getters / setters, class fields, `extends`,
///   decorators all rejected with stable per-shape tags.
///
/// Bytecode shape:
///
/// ```text
///   CreateClosure <ctor_idx>, flags=class_constructor
///   Star r_class
///   LdaNamedProperty r_class, "prototype"
///   Star r_proto
///   ; for each instance method:
///     CreateClosure <m_idx>, 0
///     StaNamedProperty r_proto, "<name>"
///   ; for each static method:
///     CreateClosure <m_idx>, 0
///     StaNamedProperty r_class, "<name>"
///   Ldar r_class           ; acc = Foo (value of the declaration)
///   Star r_<name>          ; bind Foo as a const local
/// ```
pub(super) fn lower_nested_class_declaration<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    class: &'a Class<'a>,
) -> Result<(), SourceLoweringError> {
    let class_ident = class
        .id
        .as_ref()
        .ok_or_else(|| SourceLoweringError::unsupported("anonymous_class", class.span))?;
    let class_name = class_ident.name.as_str();
    // Pre-allocate the class-name local BEFORE lowering methods so
    // `static zero() { return new Point(); }` can resolve the
    // forward self-reference through the capture path.
    let class_slot = ctx.allocate_local(class_name, true, class_ident.span)?;
    lower_class_body_core(builder, ctx, class, Some(class_name))?;
    // acc = constructor at this point — bind it to the class-name
    // local and flip the binding from pending to initialized.
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(class_slot))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (class name binding): {err:?}"))
        })?;
    ctx.mark_initialized(class_name)?;
    Ok(())
}

/// M27: ClassExpression — lowers the class body and leaves the
/// constructor in acc. Unlike `ClassDeclaration`, no outer binding
/// is introduced; the caller consumes the acc value (e.g. `let C =
/// class {…}` or `return class {…};`).
///
/// Named class expressions (`class Foo {…}` as expression) are
/// accepted, but the inner-scope `Foo` binding is NOT exposed to
/// the class body yet — methods that self-refer to the class by
/// name would need a dedicated scope frame. Most class expressions
/// are anonymous in practice, so the trade-off is acceptable for
/// M27.
pub(super) fn lower_class_expression<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    class: &'a Class<'a>,
) -> Result<(), SourceLoweringError> {
    let hint = class.id.as_ref().map(|id| id.name.as_str());
    lower_class_body_core(builder, ctx, class, hint)
}

/// Shared core for `ClassDeclaration` + `ClassExpression`. Validates
/// class elements, optionally evaluates the `extends` expression,
/// lowers the constructor (real or synthesised) with the
/// `class_constructor` flag, lowers instance methods onto
/// `Constructor.prototype` and static methods onto the Constructor
/// itself, wires `[[HomeObject]]` via `SetHomeObject` for every
/// method + the constructor, and — for derived classes — emits
/// `SetClassHeritage` so the runtime can link
/// `Sub.__proto__ = Super` and `Sub.prototype.__proto__ =
/// Super.prototype` (§15.7.14 ClassDefinitionEvaluation).
///
/// `name_hint` is the display name used for the synthesised empty
/// constructor and passed through to `lower_inner_callable`.
fn lower_class_body_core<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    class: &'a Class<'a>,
    name_hint: Option<&str>,
) -> Result<(), SourceLoweringError> {
    if !class.decorators.is_empty() {
        return Err(SourceLoweringError::unsupported(
            "class_decorator",
            class.decorators[0].span,
        ));
    }
    let class_name_owned: String = name_hint.map(str::to_owned).unwrap_or_default();
    let class_name: &str = &class_name_owned;
    // §15.7.14 step 3 — the presence of `extends` puts us in a
    // derived class, which changes constructor synthesis
    // (`constructor(...args) { super(...args); }`), the
    // default-receiver handling in `construct_callable`, and
    // enables `super(args)` inside the constructor.
    let is_derived = class.super_class.is_some();

    // 1) Classify class elements. M29 introduced methods +
    //    accessors + fields (public / private / instance /
    //    static) buckets; M29.5 extends that with private
    //    methods/accessors (same bucket as public methods, now
    //    flagged via `is_private`) and static blocks.
    //
    // `private_decls` tracks per-name what has already been
    // declared so the §15.7.11 duplicate check can permit
    // `get #x` + `set #x` pairs while still rejecting
    // `#x; get #x() {}` and the like.
    let mut constructor_fn: Option<&'a Function<'a>> = None;
    let mut methods: Vec<ClassMethod<'a>> = Vec::new();
    let mut instance_fields: Vec<ClassField<'a>> = Vec::new();
    let mut static_fields: Vec<ClassField<'a>> = Vec::new();
    let mut static_blocks: Vec<&'a oxc_ast::ast::StaticBlock<'a>> = Vec::new();
    let mut private_names: Vec<String> = Vec::new();
    let mut private_decls: Vec<(String, PrivateDecl)> = Vec::new();
    for element in class.body.body.iter() {
        match element {
            ClassElement::MethodDefinition(method) => {
                if method.computed {
                    return Err(SourceLoweringError::unsupported(
                        "computed_class_method_key",
                        method.span,
                    ));
                }
                let (key_name_owned, is_private_method) = match &method.key {
                    PropertyKey::StaticIdentifier(ident) => (ident.name.to_string(), false),
                    PropertyKey::StringLiteral(lit) => (lit.value.to_string(), false),
                    // §15.7.11 — numeric / BigInt method keys stringify
                    // as property names. `class C { 0() {} 1n() {} }`
                    // binds `"0"` and `"1"` on the prototype.
                    PropertyKey::NumericLiteral(lit) => {
                        (numeric_literal_property_key(lit.value), false)
                    }
                    PropertyKey::BigIntLiteral(lit) => (lit.value.to_string(), false),
                    PropertyKey::PrivateIdentifier(ident) => {
                        // Private methods live in the class's
                        // private-name namespace — register the
                        // name so `this.#m()` validates at
                        // compile time. §15.7.11 duplicate check
                        // allows `get #x` + `set #x` pairs to
                        // merge; any other collision is an early
                        // error.
                        let n = ident.name.to_string();
                        let kind = match method.kind {
                            MethodDefinitionKind::Get => PrivateDecl::Getter,
                            MethodDefinitionKind::Set => PrivateDecl::Setter,
                            MethodDefinitionKind::Method => PrivateDecl::Method,
                            MethodDefinitionKind::Constructor => PrivateDecl::Method,
                        };
                        record_private_decl(&mut private_decls, &n, kind, method.span)?;
                        if !private_names.contains(&n) {
                            private_names.push(n.clone());
                        }
                        (n, true)
                    }
                    other => {
                        return Err(SourceLoweringError::unsupported(
                            property_key_tag(other),
                            other.span(),
                        ));
                    }
                };
                match method.kind {
                    MethodDefinitionKind::Constructor => {
                        if is_private_method {
                            return Err(SourceLoweringError::unsupported(
                                "private_class_constructor",
                                method.span,
                            ));
                        }
                        constructor_fn = Some(&method.value);
                    }
                    MethodDefinitionKind::Method
                    | MethodDefinitionKind::Get
                    | MethodDefinitionKind::Set => {
                        methods.push(ClassMethod {
                            name: key_name_owned,
                            is_static: method.r#static,
                            is_private: is_private_method,
                            kind: method.kind,
                            func: &method.value,
                        });
                    }
                }
            }
            ClassElement::PropertyDefinition(prop) => {
                if prop.computed {
                    return Err(SourceLoweringError::unsupported(
                        "computed_class_field",
                        prop.span,
                    ));
                }
                if !prop.decorators.is_empty() {
                    return Err(SourceLoweringError::unsupported(
                        "class_decorator",
                        prop.decorators[0].span,
                    ));
                }
                match &prop.key {
                    PropertyKey::StaticIdentifier(ident) => {
                        let field = ClassField {
                            name: ident.name.to_string(),
                            is_private: false,
                            initializer: prop.value.as_ref(),
                            span: prop.span,
                        };
                        if prop.r#static {
                            static_fields.push(field);
                        } else {
                            instance_fields.push(field);
                        }
                    }
                    PropertyKey::StringLiteral(lit) => {
                        let field = ClassField {
                            name: lit.value.to_string(),
                            is_private: false,
                            initializer: prop.value.as_ref(),
                            span: prop.span,
                        };
                        if prop.r#static {
                            static_fields.push(field);
                        } else {
                            instance_fields.push(field);
                        }
                    }
                    // §15.7.10 — numeric / BigInt class field keys
                    // stringify the same way as object-literal keys.
                    PropertyKey::NumericLiteral(lit) => {
                        let field = ClassField {
                            name: numeric_literal_property_key(lit.value),
                            is_private: false,
                            initializer: prop.value.as_ref(),
                            span: prop.span,
                        };
                        if prop.r#static {
                            static_fields.push(field);
                        } else {
                            instance_fields.push(field);
                        }
                    }
                    PropertyKey::BigIntLiteral(lit) => {
                        let field = ClassField {
                            name: lit.value.to_string(),
                            is_private: false,
                            initializer: prop.value.as_ref(),
                            span: prop.span,
                        };
                        if prop.r#static {
                            static_fields.push(field);
                        } else {
                            instance_fields.push(field);
                        }
                    }
                    PropertyKey::PrivateIdentifier(ident) => {
                        let name = ident.name.to_string();
                        record_private_decl(
                            &mut private_decls,
                            &name,
                            PrivateDecl::Field,
                            prop.span,
                        )?;
                        if !private_names.contains(&name) {
                            private_names.push(name.clone());
                        }
                        let field = ClassField {
                            name,
                            is_private: true,
                            initializer: prop.value.as_ref(),
                            span: prop.span,
                        };
                        if prop.r#static {
                            static_fields.push(field);
                        } else {
                            instance_fields.push(field);
                        }
                    }
                    other => {
                        return Err(SourceLoweringError::unsupported(
                            property_key_tag(other),
                            other.span(),
                        ));
                    }
                }
            }
            ClassElement::StaticBlock(block) => {
                // M29.5: accepted. Each block becomes a 0-param
                // thunk invoked with `this = class` at
                // class-definition time (step 12 below).
                static_blocks.push(block.as_ref());
            }
            ClassElement::AccessorProperty(prop) => {
                return Err(SourceLoweringError::unsupported(
                    "accessor_property",
                    prop.span,
                ));
            }
            ClassElement::TSIndexSignature(sig) => {
                return Err(SourceLoweringError::unsupported(
                    "ts_index_signature",
                    sig.span,
                ));
            }
        }
    }

    let has_instance_fields = !instance_fields.is_empty();
    let class_private_names: std::rc::Rc<[String]> = if private_names.is_empty() {
        std::rc::Rc::from([])
    } else {
        std::rc::Rc::from(private_names.clone().into_boxed_slice())
    };

    // 2) Super-class eligibility flags for methods + constructor.
    //    Methods (including static) allow `super.x`; derived
    //    constructors additionally allow `super(args)`.
    let method_super = ClassSuperBinding {
        allow_super_property: true,
        allow_super_call: false,
    };
    let ctor_super = ClassSuperBinding {
        allow_super_property: true,
        allow_super_call: is_derived,
    };

    // 3) Acquire heritage + spill temps. Ordering mirrors §15.7.14:
    //    evaluate `superclass` first, then build the constructor
    //    closure. Heritage temp is only allocated when `extends`
    //    is present so non-derived classes keep their previous
    //    two-slot temp footprint.
    let heritage_temp: Option<RegisterIndex> = if is_derived {
        Some(ctx.acquire_temps(1)?)
    } else {
        None
    };
    let class_temp = ctx.acquire_temps(1).inspect_err(|_| {
        if is_derived {
            ctx.release_temps(1);
        }
    })?;
    let proto_temp = ctx.acquire_temps(1).inspect_err(|_| {
        ctx.release_temps(1);
        if is_derived {
            ctx.release_temps(1);
        }
    })?;
    let method_temp = ctx.acquire_temps(1).inspect_err(|_| {
        ctx.release_temps(2);
        if is_derived {
            ctx.release_temps(1);
        }
    })?;

    let lower = (|| -> Result<(), SourceLoweringError> {
        // §15.7.14 step 5 — evaluate the superclass expression
        // before anything else, while the outer lexical context is
        // still active. The runtime's `SetClassHeritage` opcode
        // validates "null or constructor" after we've built the
        // class constructor.
        if let Some(super_expr) = class.super_class.as_ref() {
            lower_return_expression(builder, ctx, super_expr)?;
            let heritage = heritage_temp.expect("heritage_temp allocated when derived");
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(heritage))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Star (class heritage): {err:?}"))
                })?;
        }

        // M29: §6.2.12 — allocate a fresh class_id BEFORE we
        // create any closure belonging to the class. Subsequent
        // `CopyClassId r_target` stamps it on the ctor, each
        // method/accessor, and the field initializer. The
        // allocation is a no-op for classes without private
        // names, but emitting it unconditionally keeps the shape
        // predictable and lets tests rely on a non-zero id.
        builder.emit(Opcode::AllocClassId, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode AllocClassId: {err:?}"))
        })?;

        // 4) Lower the constructor — if none present, synthesise
        //    one. Derived classes with no explicit constructor get
        //    `constructor(...args) { super(...args); }` per
        //    §15.7.14 step 10.b; base classes stay with the
        //    `function() {}` synthesis inherited from M27.
        let ctor_idx = match constructor_fn {
            Some(func) => {
                let (idx, captures) = lower_inner_callable_with_super(
                    ctx,
                    &func.params,
                    func.body.as_ref().ok_or_else(|| {
                        SourceLoweringError::unsupported("declared_only_function", func.span)
                    })?,
                    Some(class_name.to_owned()),
                    Some(ctor_super),
                    Some(std::rc::Rc::clone(&class_private_names)),
                    /* is_arrow */ false,
                )?;
                if is_derived {
                    let mut fns = ctx.module_functions.borrow_mut();
                    fns[idx as usize].set_derived_constructor(true);
                }
                let pc = builder.pc();
                // Constructor closure gets the class_constructor
                // flag so plain `Foo()` (without `new`) throws
                // TypeError.
                let template = crate::closure::ClosureTemplate::with_flags(
                    crate::module::FunctionIndex(idx),
                    captures,
                    crate::object::ClosureFlags::class_constructor(),
                );
                ctx.record_closure_template(pc, template);
                builder
                    .emit(Opcode::CreateClosure, &[Operand::Idx(idx), Operand::Imm(0)])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode CreateClosure (class ctor): {err:?}"
                        ))
                    })?;
                idx
            }
            None => {
                let synthetic = if is_derived {
                    let idx = synthesise_derived_default_constructor(ctx, class_name)?;
                    let mut fns = ctx.module_functions.borrow_mut();
                    fns[idx as usize].set_derived_constructor(true);
                    idx
                } else {
                    synthesise_empty_constructor(ctx, class_name)?
                };
                let pc = builder.pc();
                let template = crate::closure::ClosureTemplate::with_flags(
                    crate::module::FunctionIndex(synthetic),
                    Vec::new(),
                    crate::object::ClosureFlags::class_constructor(),
                );
                ctx.record_closure_template(pc, template);
                builder
                    .emit(
                        Opcode::CreateClosure,
                        &[Operand::Idx(synthetic), Operand::Imm(0)],
                    )
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode CreateClosure (class default ctor): {err:?}"
                        ))
                    })?;
                synthetic
            }
        };
        let _ = ctor_idx;

        // acc = constructor — spill to r_class so we can install
        // methods and statics against a stable register.
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(class_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (class ctor spill): {err:?}"))
            })?;
        // M29: stamp the class_id onto the ctor closure now that
        // it lives in a register.
        builder
            .emit(Opcode::CopyClassId, &[Operand::Reg(u32::from(class_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode CopyClassId (class ctor): {err:?}"))
            })?;
        let prototype_idx = ctx.intern_property_name("prototype")?;
        builder
            .emit(
                Opcode::LdaNamedProperty,
                &[
                    Operand::Reg(u32::from(class_temp)),
                    Operand::Idx(prototype_idx),
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode LdaNamedProperty (class prototype): {err:?}"
                ))
            })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(proto_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (class prototype spill): {err:?}"
                ))
            })?;

        // 5) §15.7.14 steps 6-7 — wire the heritage. Must happen
        //    BEFORE method installation so methods that capture the
        //    class (e.g. `static zero() { return new Point(); }`)
        //    observe a fully-initialized prototype chain. Any
        //    subsequent `Get`/`Set` on `Super.prototype` from method
        //    bodies relies on this link being in place.
        if let Some(heritage) = heritage_temp {
            builder
                .emit(
                    Opcode::SetClassHeritage,
                    &[
                        Operand::Reg(u32::from(class_temp)),
                        Operand::Reg(u32::from(heritage)),
                    ],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode SetClassHeritage: {err:?}"))
                })?;
        }

        // 6) §10.2.5 MakeMethod on the constructor — sets its
        //    `[[HomeObject]]` to `Sub.prototype` so `super.foo` from
        //    inside the constructor body walks the prototype chain
        //    rather than the static chain. The acc still holds the
        //    constructor after the earlier `Star`; we refresh it
        //    through `class_temp` for SetHomeObject's target.
        builder
            .emit(
                Opcode::SetHomeObject,
                &[
                    Operand::Reg(u32::from(class_temp)),
                    Operand::Reg(u32::from(proto_temp)),
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode SetHomeObject (class ctor): {err:?}"))
            })?;

        // 7) Instance + static method / accessor installation.
        //    Each closure gets a home object, a class_id stamp,
        //    and an install opcode chosen per `kind`.
        for method in methods.iter() {
            let (idx, captures) = lower_inner_callable_with_super(
                ctx,
                &method.func.params,
                method.func.body.as_ref().ok_or_else(|| {
                    SourceLoweringError::unsupported("declared_only_function", method.func.span)
                })?,
                Some(method.name.to_owned()),
                Some(method_super),
                Some(std::rc::Rc::clone(&class_private_names)),
                /* is_arrow */ false,
            )?;
            let pc = builder.pc();
            let template = crate::closure::ClosureTemplate::with_flags(
                crate::module::FunctionIndex(idx),
                captures,
                crate::object::ClosureFlags::method(),
            );
            ctx.record_closure_template(pc, template);
            builder
                .emit(Opcode::CreateClosure, &[Operand::Idx(idx), Operand::Imm(0)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode CreateClosure (class method): {err:?}"
                    ))
                })?;
            // Spill into `method_temp` so we can stamp HomeObject
            // / class_id without disturbing the accumulator's
            // closure value; the install opcode still reads it
            // back from acc.
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(method_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (class method spill): {err:?}"
                    ))
                })?;
            let home_reg = if method.is_static {
                class_temp
            } else {
                proto_temp
            };
            builder
                .emit(
                    Opcode::SetHomeObject,
                    &[
                        Operand::Reg(u32::from(method_temp)),
                        Operand::Reg(u32::from(home_reg)),
                    ],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode SetHomeObject (class method): {err:?}"
                    ))
                })?;
            // M29: stamp class_id so private-name lookups inside
            // the method body resolve to this class's bucket.
            builder
                .emit(Opcode::CopyClassId, &[Operand::Reg(u32::from(method_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode CopyClassId (class method): {err:?}"
                    ))
                })?;
            let name_idx = ctx.intern_property_name(&method.name)?;
            // M29.5: private methods go to `[[PrivateMethods]]`
            // (copied to each instance during construction) for
            // instance members, or directly to the class's own
            // `[[PrivateElements]]` for static members. Public
            // methods install with the usual StaNamedProperty /
            // DefineClassGetter / DefineClassSetter.
            let (install_op, install_target) =
                match (method.is_private, method.is_static, method.kind) {
                    (false, _, MethodDefinitionKind::Method) => (
                        // §15.7.11 — class methods land as
                        // non-enumerable data properties rather
                        // than the default enumerable shape of
                        // `StaNamedProperty`, so they stay out of
                        // `for…in` / `Object.keys`.
                        Opcode::DefineClassMethod,
                        if method.is_static {
                            class_temp
                        } else {
                            proto_temp
                        },
                    ),
                    (false, _, MethodDefinitionKind::Get) => (
                        Opcode::DefineClassGetter,
                        if method.is_static {
                            class_temp
                        } else {
                            proto_temp
                        },
                    ),
                    (false, _, MethodDefinitionKind::Set) => (
                        Opcode::DefineClassSetter,
                        if method.is_static {
                            class_temp
                        } else {
                            proto_temp
                        },
                    ),
                    (true, false, MethodDefinitionKind::Method) => {
                        (Opcode::PushPrivateMethod, class_temp)
                    }
                    (true, false, MethodDefinitionKind::Get) => {
                        (Opcode::PushPrivateGetter, class_temp)
                    }
                    (true, false, MethodDefinitionKind::Set) => {
                        (Opcode::PushPrivateSetter, class_temp)
                    }
                    (true, true, MethodDefinitionKind::Method) => {
                        (Opcode::DefinePrivateMethod, class_temp)
                    }
                    (true, true, MethodDefinitionKind::Get) => {
                        (Opcode::DefinePrivateGetter, class_temp)
                    }
                    (true, true, MethodDefinitionKind::Set) => {
                        (Opcode::DefinePrivateSetter, class_temp)
                    }
                    (_, _, MethodDefinitionKind::Constructor) => unreachable!("filtered above"),
                };
            builder
                .emit(
                    install_op,
                    &[
                        Operand::Reg(u32::from(install_target)),
                        Operand::Idx(name_idx),
                    ],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode {install_op:?} (class method install): {err:?}"
                    ))
                })?;
        }

        // 8) §15.7.14 step 28 — if the class body declares any
        //    instance fields (public or private), synthesise a
        //    field-initializer closure and attach it to the
        //    constructor via `SetClassFieldInitializer`. The
        //    runtime auto-invokes it on fresh receivers (base
        //    ctors run it in `construct_callable`; derived ctors
        //    run it after `super()` in `super_call_dispatch`).
        if has_instance_fields {
            let (init_idx, init_captures) = synthesise_field_initializer(
                ctx,
                &instance_fields,
                class_name,
                std::rc::Rc::clone(&class_private_names),
            )?;
            let pc = builder.pc();
            let template = crate::closure::ClosureTemplate::with_flags(
                crate::module::FunctionIndex(init_idx),
                init_captures,
                crate::object::ClosureFlags::method(),
            );
            ctx.record_closure_template(pc, template);
            builder
                .emit(
                    Opcode::CreateClosure,
                    &[Operand::Idx(init_idx), Operand::Imm(0)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode CreateClosure (field initializer): {err:?}"
                    ))
                })?;
            // Spill, stamp home + class_id, then install onto
            // the ctor. acc keeps the closure for
            // `SetClassFieldInitializer` to consume.
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(method_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (field init spill): {err:?}"
                    ))
                })?;
            builder
                .emit(
                    Opcode::SetHomeObject,
                    &[
                        Operand::Reg(u32::from(method_temp)),
                        Operand::Reg(u32::from(proto_temp)),
                    ],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode SetHomeObject (field init): {err:?}"
                    ))
                })?;
            builder
                .emit(Opcode::CopyClassId, &[Operand::Reg(u32::from(method_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode CopyClassId (field init): {err:?}"
                    ))
                })?;
            builder
                .emit(
                    Opcode::SetClassFieldInitializer,
                    &[Operand::Reg(u32::from(class_temp))],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode SetClassFieldInitializer: {err:?}"
                    ))
                })?;
        }

        // 9) Static fields — evaluate each initializer inline and
        //    install directly on the class constructor. Runs at
        //    class-definition time (not instance creation), so
        //    the expression sees the outer lexical scope. Real
        //    engines bind `this = class` for these expressions;
        //    M29 keeps that as a known limitation and will
        //    revisit once a dedicated per-field evaluator lands.
        for field in static_fields.iter() {
            if let Some(init) = field.initializer {
                lower_return_expression(builder, ctx, init)?;
            } else {
                builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaUndefined (static field default): {err:?}"
                    ))
                })?;
            }
            let name_idx = ctx.intern_property_name(&field.name)?;
            let opcode = if field.is_private {
                Opcode::DefinePrivateField
            } else {
                Opcode::DefineField
            };
            builder
                .emit(
                    opcode,
                    &[Operand::Reg(u32::from(class_temp)), Operand::Idx(name_idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode {opcode:?} (static field): {err:?}"
                    ))
                })?;
        }

        // 10) M29.5: static blocks. Each `static { … }` compiles
        //     to a 0-param thunk invoked with `this = class` at
        //     class-definition time. Declaration order matters —
        //     they run after methods + static fields so the
        //     class is fully set up.
        for block in static_blocks.iter() {
            let (idx, captures) = synthesise_static_block(
                ctx,
                block,
                class_name,
                std::rc::Rc::clone(&class_private_names),
            )?;
            let pc = builder.pc();
            let template = crate::closure::ClosureTemplate::with_flags(
                crate::module::FunctionIndex(idx),
                captures,
                crate::object::ClosureFlags::method(),
            );
            ctx.record_closure_template(pc, template);
            builder
                .emit(Opcode::CreateClosure, &[Operand::Idx(idx), Operand::Imm(0)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode CreateClosure (static block): {err:?}"
                    ))
                })?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(method_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (static block spill): {err:?}"
                    ))
                })?;
            builder
                .emit(
                    Opcode::SetHomeObject,
                    &[
                        Operand::Reg(u32::from(method_temp)),
                        Operand::Reg(u32::from(class_temp)),
                    ],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode SetHomeObject (static block): {err:?}"
                    ))
                })?;
            builder
                .emit(Opcode::CopyClassId, &[Operand::Reg(u32::from(method_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode CopyClassId (static block): {err:?}"
                    ))
                })?;
            // Invoke the thunk: `CallProperty r_thunk, r_class, {}`.
            // The receiver operand pins `this = class` inside
            // the block body; zero args match the zero-param
            // signature.
            let call_pc = builder
                .emit(
                    Opcode::CallProperty,
                    &[
                        Operand::Reg(u32::from(method_temp)),
                        Operand::Reg(u32::from(class_temp)),
                        Operand::RegList { base: 0, count: 0 },
                    ],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode CallProperty (static block): {err:?}"
                    ))
                })?;
            ctx.attach_call_feedback(builder, call_pc);
        }

        // 11) Leave the constructor in acc — the caller
        //     (declaration or expression path) decides whether
        //     to bind it anywhere.
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(class_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (class result): {err:?}"))
            })?;
        Ok(())
    })();
    ctx.release_temps(3);
    if is_derived {
        ctx.release_temps(1);
    }
    lower
}

/// §15.7.14 step 28 — synthesises the class field initializer
/// closure. Each instance field (public or private) becomes one
/// `DefineField` / `DefinePrivateField` pair in the body:
///
/// ```text
///   LdaThis                             ; once at entry
///   Star r_this
///   ; for each field:
///     <initializer>  (or LdaUndefined)
///     DefineField r_this, name_idx      ; public
///     ; or DefinePrivateField r_this, name_idx
///   LdaUndefined
///   Return
/// ```
///
/// The closure is installed on the class constructor via
/// `SetClassFieldInitializer`; the runtime invokes it once per
/// instance (see `construct_callable` / `super_call_dispatch`).
/// Captures are resolved via the normal parent-chain walk so
/// initializers can reference outer-scope bindings.
/// M29.5: compile a `static { … }` block into a 0-param thunk
/// whose body IS that block's statement list. Invoked at
/// class-definition time with `this = class`, so the block body
/// sees the class constructor as its receiver. Captures outer
/// bindings via the normal parent-chain walk; private-name scope
/// is inherited from the enclosing class.
fn synthesise_static_block<'a>(
    outer: &LoweringContext<'a>,
    block: &'a oxc_ast::ast::StaticBlock<'a>,
    class_name: &str,
    class_private_names: std::rc::Rc<[String]>,
) -> Result<(u32, Vec<crate::closure::CaptureDescriptor>), SourceLoweringError> {
    let params_layout = ParamsLayout {
        names: Vec::new(),
        defaults: Vec::new(),
        patterns: Vec::new(),
        rest_name: None,
        rest_pattern: None,
    };
    let mut builder = BytecodeBuilder::new();
    let mut ctx = LoweringContext::with_parent(
        &params_layout,
        outer.function_names,
        std::rc::Rc::clone(&outer.module_functions),
        Some(outer),
        Some(ClassSuperBinding {
            allow_super_property: true,
            allow_super_call: false,
        }),
        Some(class_private_names),
    );

    let lower = (|| -> Result<(), SourceLoweringError> {
        for stmt in block.body.iter() {
            // `static { ... }` shares the function-body statement
            // surface: `let`/`const` declarations are permitted,
            // expressions / ifs / loops etc. go through the
            // nested path.
            lower_top_statement(&mut builder, &mut ctx, stmt)?;
        }
        builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode LdaUndefined: {err:?}"))
        })?;
        builder
            .emit(Opcode::Return, &[])
            .map_err(|err| SourceLoweringError::Internal(format!("encode Return: {err:?}")))?;
        Ok(())
    })();
    lower?;

    let bytecode = builder
        .finish()
        .map_err(|err| SourceLoweringError::Internal(format!("finish static block: {err:?}")))?;
    let bytecode_len = bytecode.bytes().len() as u32;
    let layout = FrameLayout::new(1, 0, ctx.local_count(), ctx.temp_count())
        .map_err(|err| SourceLoweringError::Internal(format!("static block layout: {err:?}")))?;
    let feedback_layout = feedback_layout_from_kinds(&ctx.take_feedback_slot_kinds());
    let side_tables = crate::module::FunctionSideTables::new(
        ctx.take_property_names(),
        ctx.take_string_literals(),
        ctx.take_float_constants(),
        ctx.take_bigint_constants(),
        ctx.take_closure_table(bytecode_len),
        Default::default(),
        ctx.take_regexp_literals(),
    );
    let exception_handlers = ctx.take_exception_handlers(&BytecodeBuilder::new())?;
    let tables = FunctionTables::new(
        side_tables,
        feedback_layout,
        Default::default(),
        crate::exception::ExceptionTable::new(exception_handlers),
        Default::default(),
    );
    let block_name = format!("{class_name}#staticBlock");
    let func = VmFunction::new(Some(block_name), layout, bytecode, tables);
    let captures: Vec<crate::closure::CaptureDescriptor> = ctx
        .captures
        .borrow()
        .iter()
        .map(|entry| entry.descriptor)
        .collect();
    let mut fns = outer.module_functions.borrow_mut();
    let idx = u32::try_from(fns.len())
        .map_err(|_| SourceLoweringError::Internal("function index overflow".into()))?;
    fns.push(func);
    Ok((idx, captures))
}

fn synthesise_field_initializer<'a>(
    outer: &LoweringContext<'a>,
    fields: &[ClassField<'a>],
    class_name: &str,
    class_private_names: std::rc::Rc<[String]>,
) -> Result<(u32, Vec<crate::closure::CaptureDescriptor>), SourceLoweringError> {
    let params_layout = ParamsLayout {
        names: Vec::new(),
        defaults: Vec::new(),
        patterns: Vec::new(),
        rest_name: None,
        rest_pattern: None,
    };
    let mut builder = BytecodeBuilder::new();
    let ctx = LoweringContext::with_parent(
        &params_layout,
        outer.function_names,
        std::rc::Rc::clone(&outer.module_functions),
        Some(outer),
        Some(ClassSuperBinding {
            allow_super_property: true,
            allow_super_call: false,
        }),
        Some(class_private_names),
    );

    let this_reg = ctx.acquire_temps(1)?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        builder
            .emit(Opcode::LdaThis, &[])
            .map_err(|err| SourceLoweringError::Internal(format!("encode LdaThis: {err:?}")))?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(this_reg))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (field init this): {err:?}"))
            })?;
        for field in fields {
            if let Some(init) = field.initializer {
                lower_return_expression(&mut builder, &ctx, init)?;
            } else {
                builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaUndefined (field default): {err:?}"
                    ))
                })?;
            }
            let name_idx = ctx.intern_property_name(&field.name)?;
            let opcode = if field.is_private {
                Opcode::DefinePrivateField
            } else {
                Opcode::DefineField
            };
            builder
                .emit(
                    opcode,
                    &[Operand::Reg(u32::from(this_reg)), Operand::Idx(name_idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode {opcode:?} (field init): {err:?}"
                    ))
                })?;
        }
        builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode LdaUndefined: {err:?}"))
        })?;
        builder
            .emit(Opcode::Return, &[])
            .map_err(|err| SourceLoweringError::Internal(format!("encode Return: {err:?}")))?;
        Ok(())
    })();
    ctx.release_temps(1);
    lower?;

    let bytecode = builder.finish().map_err(|err| {
        SourceLoweringError::Internal(format!("finish field initializer: {err:?}"))
    })?;
    let bytecode_len = bytecode.bytes().len() as u32;

    let layout = FrameLayout::new(1, 0, ctx.local_count(), ctx.temp_count())
        .map_err(|err| SourceLoweringError::Internal(format!("field init layout: {err:?}")))?;
    let feedback_layout = feedback_layout_from_kinds(&ctx.take_feedback_slot_kinds());
    let side_tables = crate::module::FunctionSideTables::new(
        ctx.take_property_names(),
        ctx.take_string_literals(),
        ctx.take_float_constants(),
        ctx.take_bigint_constants(),
        ctx.take_closure_table(bytecode_len),
        Default::default(),
        ctx.take_regexp_literals(),
    );
    // The field-initializer body can't emit `try`/`catch` — it's
    // compiled from individual expressions, not statements — so
    // the exception handler list is always empty.
    let exception_table = crate::exception::ExceptionTable::new(Vec::new());
    let tables = FunctionTables::new(
        side_tables,
        feedback_layout,
        Default::default(),
        exception_table,
        Default::default(),
    );
    let init_name = format!("{class_name}#init");
    let func = VmFunction::new(Some(init_name), layout, bytecode, tables);
    let captures: Vec<crate::closure::CaptureDescriptor> = ctx
        .captures
        .borrow()
        .iter()
        .map(|entry| entry.descriptor)
        .collect();
    let mut fns = outer.module_functions.borrow_mut();
    let idx = u32::try_from(fns.len())
        .map_err(|_| SourceLoweringError::Internal("function index overflow".into()))?;
    fns.push(func);
    Ok((idx, captures))
}

/// §15.7.14 step 10.b — synthesises the default constructor for a
/// derived class: `constructor(...args) { super(...args); }`.
/// Builds the bytecode directly (no AST round-trip) so the
/// synthesised function stays independent of the outer
/// `LoweringContext`'s parameter layout.
///
/// Frame shape: 1 hidden (receiver) + 0 params + 1 local
/// (`r_args` — the rest-args Array) + 1 temp. Bytecode:
///
/// ```text
///   CreateRestParameters                 ; acc = Array(...args)
///   Star r_args                          ; r_args = acc
///   CallSuperSpread RegList{r_args, 1}   ; super(...args), acc = receiver
///   LdaUndefined                         ; §10.2.1.3 derived ctors return
///   Return                               ; undefined → use `this`
/// ```
///
/// The derived-constructor flag is applied by the caller via
/// [`VmFunction::set_derived_constructor`].
fn synthesise_derived_default_constructor<'a>(
    outer: &LoweringContext<'a>,
    class_name: &str,
) -> Result<u32, SourceLoweringError> {
    // 1 hidden + 0 params + 1 local (rest array) + 0 temp. The
    // RegList for CallSuperSpread operates on the local slot
    // directly, so no extra scratch temp is needed.
    let layout = FrameLayout::new(1, 0, 1, 0)
        .map_err(|err| SourceLoweringError::Internal(format!("derived ctor layout: {err:?}")))?;
    // The rest-args array lives at user-visible slot 0. Register
    // operands carry user-visible indices; `read_bytecode_register`
    // adds `hidden_count` at dispatch time, so we must not
    // pre-resolve here.
    let args_reg: RegisterIndex = 0;
    let mut builder = BytecodeBuilder::new();
    builder
        .emit(Opcode::CreateRestParameters, &[])
        .map_err(|err| {
            SourceLoweringError::Internal(format!(
                "encode CreateRestParameters (derived ctor): {err:?}"
            ))
        })?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(args_reg))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (derived ctor args): {err:?}"))
        })?;
    // C4: synthesised derived constructor has no `LoweringContext`
    // scope chain from which to allocate a feedback slot. Skipped —
    // this bytecode runs at most once per class declaration and is
    // never a hot-path call site.
    builder
        .emit(
            Opcode::CallSuperSpread,
            &[Operand::RegList {
                base: u32::from(args_reg),
                count: 1,
            }],
        )
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode CallSuperSpread (derived ctor): {err:?}"))
        })?;
    builder
        .emit(Opcode::LdaUndefined, &[])
        .map_err(|err| SourceLoweringError::Internal(format!("encode LdaUndefined: {err:?}")))?;
    builder
        .emit(Opcode::Return, &[])
        .map_err(|err| SourceLoweringError::Internal(format!("encode Return: {err:?}")))?;
    let bytecode = builder.finish().map_err(|err| {
        SourceLoweringError::Internal(format!("finish derived default ctor: {err:?}"))
    })?;
    let func = VmFunction::with_empty_tables(Some(class_name.to_owned()), layout, bytecode);
    let mut fns = outer.module_functions.borrow_mut();
    let idx = u32::try_from(fns.len())
        .map_err(|_| SourceLoweringError::Internal("function index overflow".into()))?;
    fns.push(func);
    Ok(idx)
}

/// Synthesises an empty class constructor function
/// (`function() {}`) as a fresh `VmFunction` and appends it to
/// the shared module list. Returns the new index. Used when a
/// `class` declaration omits an explicit `constructor`.
fn synthesise_empty_constructor<'a>(
    outer: &LoweringContext<'a>,
    class_name: &str,
) -> Result<u32, SourceLoweringError> {
    let layout = FrameLayout::new(1, 0, 0, 0)
        .map_err(|err| SourceLoweringError::Internal(format!("empty ctor layout: {err:?}")))?;
    let mut builder = BytecodeBuilder::new();
    builder
        .emit(Opcode::LdaUndefined, &[])
        .map_err(|err| SourceLoweringError::Internal(format!("encode LdaUndefined: {err:?}")))?;
    builder
        .emit(Opcode::Return, &[])
        .map_err(|err| SourceLoweringError::Internal(format!("encode Return: {err:?}")))?;
    let bytecode = builder
        .finish()
        .map_err(|err| SourceLoweringError::Internal(format!("finish empty ctor: {err:?}")))?;
    let func = VmFunction::with_empty_tables(Some(class_name.to_owned()), layout, bytecode);
    let mut fns = outer.module_functions.borrow_mut();
    let idx = u32::try_from(fns.len())
        .map_err(|_| SourceLoweringError::Internal("function index overflow".into()))?;
    fns.push(func);
    Ok(idx)
}
