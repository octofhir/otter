//! Class declaration lowering: class body iteration, method/field/static-block
//! installation, private-name environment setup, derived-constructor emission
//! with implicit `super()`, field initializer code-gen, and heritage resolution.
//!
//! Spec: ECMA-262 §15.7 <https://tc39.es/ecma262/#sec-class-definitions>

use super::ast::{expected_function_length, extract_function_params};
use super::module_compiler::{FunctionIdentity, ModuleCompiler};
use super::shared::{Binding, FunctionCompiler, FunctionKind, ValueLocation};
use super::*;
use oxc_ast::ast::{Class, Expression, Function, MethodDefinitionKind};

impl<'a> FunctionCompiler<'a> {
    /// §15.7 ClassDeclaration — `class Name { ... }`
    /// Spec: <https://tc39.es/ecma262/#sec-class-definitions>
    pub(super) fn compile_class_declaration(
        &mut self,
        class: &Class<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        let name = class.id.as_ref().ok_or_else(|| {
            SourceLoweringError::Unsupported("class declarations without identifiers".to_string())
        })?;
        let binding = self.declare_variable_binding(name.name.as_str(), false)?;

        let constructor_value = self.compile_class_body(class, name.name.as_str(), module)?;

        if constructor_value.register != binding {
            self.instructions
                .push(Instruction::move_(binding, constructor_value.register));
        }

        Ok(())
    }

    /// §15.7.14 PrivateBoundNames uniqueness check.
    ///
    /// Collects every private declaration in the class body (private fields,
    /// private methods, private getters, private setters — instance and
    /// static alike) and rejects duplicate entries per the spec's early-error
    /// rule. The only allowed duplication is a `{getter, setter}` pair that
    /// shares its `static` flag with no other entries on the same name.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-static-semantics-privateboundnames>
    /// Spec: <https://tc39.es/ecma262/#sec-class-definitions-static-semantics-early-errors>
    fn validate_private_bound_names(
        &self,
        class: &Class<'_>,
    ) -> Result<std::collections::HashSet<String>, SourceLoweringError> {
        use oxc_ast::ast::{ClassElement, MethodDefinitionKind, PropertyDefinitionType};

        /// One private declaration kind plus its static-ness.
        #[derive(Debug, Clone, Copy)]
        enum PrivateEntry {
            Field,
            Method,
            Getter { is_static: bool },
            Setter { is_static: bool },
        }

        // PrivateBoundNames treats static and instance members as the same
        // namespace (§8.2.3), so we key by the bare name and collect the
        // list of entries observed for each.
        let mut seen: std::collections::HashMap<String, Vec<PrivateEntry>> =
            std::collections::HashMap::new();

        let mut record = |name: &str, entry: PrivateEntry| -> Result<(), SourceLoweringError> {
            let list = seen.entry(name.to_string()).or_default();
            list.push(entry);
            // Allowed multiplicities:
            //   - 1 entry of any kind
            //   - 2 entries iff they are a getter+setter pair that share the
            //     same static-ness (everything else is a duplicate).
            let ok = match list.as_slice() {
                [] => unreachable!(),
                [_] => true,
                [a, b] => matches!(
                    (*a, *b),
                    (
                        PrivateEntry::Getter { is_static: s1 },
                        PrivateEntry::Setter { is_static: s2 },
                    ) | (
                        PrivateEntry::Setter { is_static: s1 },
                        PrivateEntry::Getter { is_static: s2 },
                    ) if s1 == s2
                ),
                _ => false,
            };
            if !ok {
                return Err(SourceLoweringError::EarlyError(format!(
                    "Duplicate private name declaration: #{name}"
                )));
            }
            Ok(())
        };

        for element in &class.body.body {
            match element {
                ClassElement::MethodDefinition(method) => {
                    let oxc_ast::ast::PropertyKey::PrivateIdentifier(ident) = &method.key else {
                        continue;
                    };
                    let name = ident.name.as_str();
                    let is_static = method.r#static;
                    let entry = match method.kind {
                        MethodDefinitionKind::Method => PrivateEntry::Method,
                        MethodDefinitionKind::Get => PrivateEntry::Getter { is_static },
                        MethodDefinitionKind::Set => PrivateEntry::Setter { is_static },
                        MethodDefinitionKind::Constructor => continue,
                    };
                    record(name, entry)?;
                }
                ClassElement::PropertyDefinition(prop) => {
                    if prop.declare
                        || matches!(
                            prop.r#type,
                            PropertyDefinitionType::TSAbstractPropertyDefinition
                        )
                    {
                        continue;
                    }
                    let oxc_ast::ast::PropertyKey::PrivateIdentifier(ident) = &prop.key else {
                        continue;
                    };
                    record(ident.name.as_str(), PrivateEntry::Field)?;
                }
                _ => {}
            }
        }

        Ok(seen.keys().cloned().collect())
    }

    /// §15.7.14 / §8.3 AllPrivateNamesValid — walk the class body and verify
    /// that every `#name` reference inside it resolves to a declaration in
    /// either the current class or an enclosing class's private environment.
    /// Nested classes are skipped (they validate on their own).
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-static-semantics-allprivatenamesvalid>
    fn validate_private_name_references(
        &self,
        class: &Class<'_>,
        declared_here: &std::collections::HashSet<String>,
    ) -> Result<(), SourceLoweringError> {
        use super::ast::{check_expression_private_refs, check_statement_private_refs};
        use oxc_ast::ast::ClassElement;

        let is_declared = |name: &str| -> bool {
            if declared_here.contains(name) {
                return true;
            }
            self.private_name_scopes
                .iter()
                .any(|scope| scope.contains(name))
        };

        // Heritage expression (`extends Foo`) is evaluated *before* the
        // class body's private environment is established, so private
        // references inside it may only resolve to outer scopes — never
        // the current class's declarations.
        if let Some(super_class) = class.super_class.as_ref() {
            let is_declared_outer = |name: &str| -> bool {
                self.private_name_scopes
                    .iter()
                    .any(|scope| scope.contains(name))
            };
            check_expression_private_refs(super_class, &is_declared_outer)?;
        }

        for element in &class.body.body {
            match element {
                ClassElement::PropertyDefinition(prop) => {
                    if prop.declare {
                        continue;
                    }
                    if prop.computed
                        && let Some(key_expr) = prop.key.as_expression()
                    {
                        check_expression_private_refs(key_expr, &is_declared)?;
                    }
                    if let Some(value) = prop.value.as_ref() {
                        check_expression_private_refs(value, &is_declared)?;
                    }
                }
                ClassElement::MethodDefinition(method) => {
                    if method.computed
                        && let Some(key_expr) = method.key.as_expression()
                    {
                        check_expression_private_refs(key_expr, &is_declared)?;
                    }
                    if let Some(body) = method.value.body.as_ref() {
                        for stmt in &body.statements {
                            check_statement_private_refs(stmt, &is_declared)?;
                        }
                    }
                }
                ClassElement::StaticBlock(block) => {
                    for stmt in &block.body {
                        check_statement_private_refs(stmt, &is_declared)?;
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// §15.7.14 ClassDefinitionEvaluation — shared implementation for class
    /// declarations and class expressions.
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
    pub(super) fn compile_class_body(
        &mut self,
        class: &Class<'_>,
        class_name: &str,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        // §15.7.14 Static Semantics: Early Errors — PrivateBoundNames of
        // ClassBody must not contain duplicates, unless a name is used
        // exactly once for a getter and once for a setter with matching
        // static-ness (and no other entries).
        // Spec: <https://tc39.es/ecma262/#sec-static-semantics-privateboundnames>
        let declared_private_names = self.validate_private_bound_names(class)?;

        // §15.7.14 / §8.3 AllPrivateNamesValid — verify every `#name`
        // reference inside the class body resolves against the current class
        // or an enclosing lexical class's private environment.
        self.validate_private_name_references(class, &declared_private_names)?;

        // Push this class's declared private names so nested classes and
        // closures compiled as part of this body see them when validating
        // their own private references. Mirror the change onto the module
        // compiler's pending latch so freshly constructed child
        // `FunctionCompiler`s pick up the inherited chain too.
        self.private_name_scopes
            .push(declared_private_names.clone());
        module
            .pending_private_name_scopes
            .push(declared_private_names);
        let result = self.compile_class_body_inner(class, class_name, module);
        self.private_name_scopes.pop();
        module.pending_private_name_scopes.pop();
        result
    }

    /// Inner implementation of `compile_class_body` that runs after the
    /// `PrivateNameEnvironment` has been pushed for this class.
    fn compile_class_body_inner(
        &mut self,
        class: &Class<'_>,
        class_name: &str,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        use oxc_ast::ast::{ClassElement, MethodDefinitionKind, PropertyDefinitionType};

        // ── First pass: extract constructor, count instance fields, detect private members ──
        let mut constructor = None;
        let mut has_instance_fields = false;
        let mut has_private_members = false;
        for element in &class.body.body {
            match element {
                ClassElement::MethodDefinition(method)
                    if matches!(method.kind, MethodDefinitionKind::Constructor) =>
                {
                    if constructor.is_some() {
                        return Err(SourceLoweringError::Unsupported(
                            "duplicate class constructors".to_string(),
                        ));
                    }
                    if method.r#static {
                        return Err(SourceLoweringError::Unsupported(
                            "static class constructors".to_string(),
                        ));
                    }
                    constructor = Some(&method.value);
                }
                ClassElement::MethodDefinition(method) => {
                    if matches!(&method.key, oxc_ast::ast::PropertyKey::PrivateIdentifier(_)) {
                        has_private_members = true;
                    }
                }
                ClassElement::PropertyDefinition(prop) => {
                    if prop.declare {
                        continue;
                    }
                    if matches!(
                        prop.r#type,
                        PropertyDefinitionType::TSAbstractPropertyDefinition
                    ) {
                        continue;
                    }
                    // §15.7 Static Semantics: Early Errors — FieldDefinition
                    // ContainsArguments and Contains SuperCall checks.
                    if let Some(init_expr) = &prop.value {
                        super::ast::check_field_initializer(init_expr)?;
                    }
                    if !prop.r#static {
                        has_instance_fields = true;
                    }
                    if matches!(&prop.key, oxc_ast::ast::PropertyKey::PrivateIdentifier(_)) {
                        has_private_members = true;
                    }
                }
                ClassElement::StaticBlock(_) => {}
                ClassElement::AccessorProperty(_) => {
                    return Err(SourceLoweringError::Unsupported(
                        "accessor class properties (auto-accessor) are not yet implemented"
                            .to_string(),
                    ));
                }
                _ => {
                    return Err(SourceLoweringError::Unsupported(
                        "unsupported class element".to_string(),
                    ));
                }
            }
        }

        // §15.7.15 ClassDefinitionEvaluation step 2-3: Create a lexical
        // binding for the class name inside the class body scope. Named
        // classes expose an immutable inner binding so `class C { m() { C } }`
        // resolves `C` inside methods. The binding starts in TDZ (hole) so
        // `class x extends x {}` throws ReferenceError from the extends
        // expression, and is initialized after the constructor is compiled.
        // Anonymous class expressions (via NamedEvaluation) do NOT get this
        // binding — references to the contextual name resolve to the outer
        // variable instead (§15.7.15).
        let class_has_self_binding = class.id.is_some();
        // §15.7.15 step 2-3: Named classes get a TDZ inner binding. We
        // allocate the local in the outer FC but do NOT insert the binding
        // into `self.scope` — instead we'll push a temporary class-scope
        // frame that only child compilations (methods, field initialisers)
        // see via `parent_scopes_for_child()`. This avoids leaking the
        // immutable binding into the outer scope where `var C = class C {}`
        // would collide with it.
        let class_name_register = if class_has_self_binding && !class_name.is_empty() {
            let register = self.allocate_local()?;
            self.instructions.push(Instruction::load_hole(register));
            Some(register)
        } else {
            None
        };

        // §15.7.15 — Push the class name inner binding into scope BEFORE
        // the extends expression so `class x extends x {}` hits the TDZ
        // (ImmutableRegister with hole → AssertNotHole → ReferenceError).
        let saved_class_name_binding = if let Some(class_reg) = class_name_register
            && !class_name.is_empty()
        {
            let old = self.scope.borrow_mut().bindings.insert(
                class_name.to_string(),
                Binding::ImmutableRegister(class_reg),
            );
            Some(old)
        } else {
            None
        };

        // ── Compile super class ─────────────────────────────────────────────
        // §15.7.14 step 5: Detect `class extends null` — protoParent = null,
        // constructorParent = %Function.prototype%, constructor kind = base.
        // Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
        let extends_null = matches!(class.super_class.as_ref(), Some(Expression::NullLiteral(_)));
        // §15.7.14 step 7: Evaluate ClassHeritage in the *outer*
        // PrivateEnvironment. Temporarily pop this class's declared-names
        // frame (pushed by `compile_class_body`) so that any nested class
        // expressions inside the heritage expression see only the enclosing
        // lexical classes — not the class currently being defined.
        let super_class = if let Some(super_class) = class.super_class.as_ref() {
            if extends_null {
                None // Don't compile null as a super class value
            } else {
                let saved_fc = self.private_name_scopes.pop();
                let saved_mc = module.pending_private_name_scopes.pop();
                // Class declarations/expressions create an implicitly
                // strict context for the entire ClassTail (including the
                // heritage expression). Force strict so `with` statements
                // inside heritage functions are rejected as SyntaxError.
                let saved_strict = self.strict_mode;
                self.strict_mode = true;
                let super_result = self.compile_expression(super_class, module);
                self.strict_mode = saved_strict;
                if let Some(frame) = saved_fc {
                    self.private_name_scopes.push(frame);
                }
                if let Some(frame) = saved_mc {
                    module.pending_private_name_scopes.push(frame);
                }
                let super_value = super_result?;
                Some(self.stabilize_binding_value(super_value)?)
            }
        } else {
            None
        };
        // §15.7.14 — A class with `extends` (including `extends null`) is
        // derived. `extends null` means the constructor CAN contain `super()`
        // but the call will throw TypeError at runtime because null is not
        // a constructor.
        let is_derived = class.super_class.is_some();

        // §15.7.14 step 5.f: if superclass is not null and not a
        // constructor, throw TypeError BEFORE reading `.prototype`.
        if let Some(ref sc) = super_class {
            self.instructions
                .push(Instruction::assert_constructor(sc.register));
        }

        // ── Compile constructor ─────────────────────────────────────────────
        // RunClassFieldInitializer is needed both for instance fields AND for
        // copying private methods/accessors to instances.
        let needs_field_initializer = has_instance_fields || has_private_members;
        // The ImmutableRegister binding was already pushed BEFORE extends
        // evaluation (above). No second push needed here.
        // When the outer scope already has the class name as
        // ImmutableRegister, the constructor must NOT create its own
        // declare_function_binding — it should capture via upvalue
        // (ImmutableUpvalue) instead, so writes trigger ThrowConstAssign.
        let ctor_has_self_binding = if saved_class_name_binding.is_some() {
            false
        } else {
            class_has_self_binding
        };
        let constructor_value = if let Some(ctor) = constructor {
            self.compile_class_constructor_with_fields(
                class_name,
                ctor_has_self_binding,
                ctor,
                is_derived,
                needs_field_initializer,
                module,
            )?
        } else if is_derived {
            self.compile_default_derived_class_constructor_with_fields(
                class_name,
                ctor_has_self_binding,
                needs_field_initializer,
                module,
            )?
        } else {
            self.compile_default_base_class_constructor_with_fields(
                class_name,
                ctor_has_self_binding,
                needs_field_initializer,
                module,
            )?
        };
        let constructor_value = if constructor_value.is_temp {
            self.stabilize_binding_value(constructor_value)?
        } else {
            constructor_value
        };

        // §15.7.15 step 12: Initialize the class name binding.
        // Now that the constructor closure exists, move its value into the
        // TDZ register allocated earlier so the body scope sees the live
        // constructor instead of the initial hole sentinel.
        if let Some(class_reg) = class_name_register
            && class_reg != constructor_value.register
        {
            self.instructions
                .push(Instruction::move_(class_reg, constructor_value.register));
        }

        // ── Set up prototype chain ──────────────────────────────────────────
        if let Some(super_class) = super_class {
            // Normal extends: constructor.__proto__ = superClass
            self.emit_object_method_call(
                "setPrototypeOf",
                constructor_value,
                &[super_class],
                module,
            )?;
        }
        // For extends null: constructor.__proto__ stays as Function.prototype (default).

        let prototype = self.emit_named_property_load(constructor_value, "prototype")?;
        let prototype = self.stabilize_binding_value(prototype)?;
        let prototype_parent = if extends_null {
            // §15.7.14 step 5.b.i: protoParent = null
            // Stabilize to prevent clobbering by internal allocations in
            // emit_object_method_call.
            let null_val = self.load_null()?;
            self.stabilize_binding_value(null_val)?
        } else if let Some(super_class) = super_class {
            let parent = self.emit_named_property_load(super_class, "prototype")?;
            self.stabilize_binding_value(parent)?
        } else {
            let object_ctor = self.compile_identifier("Object")?;
            let object_ctor = if object_ctor.is_temp {
                self.stabilize_binding_value(object_ctor)?
            } else {
                object_ctor
            };
            let parent = self.emit_named_property_load(object_ctor, "prototype")?;
            self.stabilize_binding_value(parent)?
        };
        self.emit_object_method_call("setPrototypeOf", prototype, &[prototype_parent], module)?;
        self.release(prototype_parent);

        // ── AllocClassId if the class has private members ──────────────────
        // §6.2.12 — Allocate a unique class identifier for private name resolution.
        if has_private_members {
            self.instructions
                .push(Instruction::alloc_class_id(constructor_value.register));
        }

        // §10.2.5 MakeMethod — set `[[HomeObject]]` on the class constructor
        // so that `super.foo` inside the constructor body resolves against
        // `prototype.[[Prototype]]` (which is `SuperClass.prototype`).
        // Static methods and the constructor itself share the constructor
        // as their HomeObject: per spec the static part of a derived class
        // has `home_object = constructor`, so `super.foo` in a static method
        // walks from `constructor.[[Prototype]]` (the parent class
        // constructor). The instance constructor uses `prototype` as its
        // HomeObject so instance `super.foo` resolves against
        // `prototype.[[Prototype]]`.
        self.instructions.push(Instruction::set_home_object(
            constructor_value.register,
            prototype.register,
        ));

        // ── Second pass: install methods ────────────────────────────────────
        // §15.7.14 ClassDefinitionEvaluation step 26–28.
        for element in &class.body.body {
            if let ClassElement::MethodDefinition(method) = element
                && !matches!(method.kind, MethodDefinitionKind::Constructor)
            {
                let is_private =
                    matches!(&method.key, oxc_ast::ast::PropertyKey::PrivateIdentifier(_));
                let class_id_src = if has_private_members {
                    Some(constructor_value.register)
                } else {
                    None
                };
                if is_private {
                    self.compile_private_class_method(
                        method,
                        constructor_value,
                        prototype,
                        module,
                    )?;
                } else {
                    let target = if method.r#static {
                        constructor_value
                    } else {
                        prototype
                    };
                    self.compile_class_method(method, target, class_id_src, module)?;
                }
            }
        }

        self.emit_make_class_prototype_non_writable(constructor_value, module)?;

        // ── Pre-evaluate ALL computed field keys in source order ─────────
        // §15.7.14 step 27: ClassElementEvaluation evaluates computed
        // property names at class definition time, in source order, for
        // BOTH static and instance fields. Errors in key expressions
        // (ReferenceError, ToPrimitive throws, etc.) must fire here, not
        // when an instance is later created.
        // Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
        //
        // `computed_key_bindings_per_element` maps class body element index
        // (source order) → Some(binding_name) if that element is a computed
        // field (static or instance). `instance_computed_key_bindings` is
        // the ordered sublist of instance-only bindings, used by the
        // field_initializer loop which only iterates instance fields.
        let mut computed_key_bindings_per_element: Vec<Option<String>> =
            vec![None; class.body.body.len()];
        let mut instance_computed_key_bindings: Vec<String> = Vec::new();
        {
            let mut ck_index = 0usize;
            for (elem_idx, element) in class.body.body.iter().enumerate() {
                if let ClassElement::PropertyDefinition(prop) = element
                    && !prop.declare
                    && !matches!(
                        prop.r#type,
                        PropertyDefinitionType::TSAbstractPropertyDefinition
                    )
                    && prop.computed
                    && !matches!(&prop.key, oxc_ast::ast::PropertyKey::PrivateIdentifier(_))
                {
                    // Evaluate key expression at class definition time.
                    let key = self.compile_expression(prop.key.to_expression(), module)?;
                    let key_local = self.allocate_local()?;
                    self.instructions
                        .push(Instruction::move_(key_local, key.register));
                    self.release(key);
                    // §7.1.14 ToPropertyKey — coerce to String/Symbol now.
                    self.instructions
                        .push(Instruction::to_property_key(key_local));

                    // Create a synthetic binding so child compilers (field
                    // init function) can capture it as an upvalue.
                    let binding_name = format!("$__ck_{ck_index}");
                    ck_index += 1;
                    self.scope
                        .borrow_mut()
                        .bindings
                        .insert(binding_name.clone(), Binding::Register(key_local));
                    computed_key_bindings_per_element[elem_idx] = Some(binding_name.clone());
                    if !prop.r#static {
                        instance_computed_key_bindings.push(binding_name);
                    }
                }
            }
        }
        let computed_key_bindings = instance_computed_key_bindings;

        // ── Compile instance field initializer ──────────────────────────────
        // §15.7.14 step 29: Create an initializer function for instance fields.
        if needs_field_initializer {
            self.compile_class_field_initializer(
                class,
                constructor_value,
                prototype,
                has_private_members,
                &computed_key_bindings,
                module,
            )?;
        }

        // ── Third pass: static fields and static blocks ─────────────────────
        // §15.7.14 step 34: Evaluate static field initializers in order.
        // Computed keys are already pre-evaluated in source order above.
        // Filter must match the pre-evaluation loop exactly (same skip rules
        // for `declare` and TS abstract properties) so that
        // `computed_key_bindings_per_element[idx]` is always populated for
        // every computed static field this loop processes.
        // Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
        for (elem_idx, element) in class.body.body.iter().enumerate() {
            match element {
                ClassElement::PropertyDefinition(prop)
                    if prop.r#static
                        && !prop.declare
                        && !matches!(
                            prop.r#type,
                            PropertyDefinitionType::TSAbstractPropertyDefinition
                        ) =>
                {
                    if let oxc_ast::ast::PropertyKey::PrivateIdentifier(ident) = &prop.key {
                        // Static private field: compile value and emit DefinePrivateField
                        // on the constructor.
                        self.compile_static_private_field(
                            ident.name.as_str(),
                            prop,
                            constructor_value,
                            module,
                        )?;
                    } else {
                        let precomputed_key =
                            computed_key_bindings_per_element[elem_idx].as_deref();
                        self.compile_static_field(
                            prop,
                            constructor_value,
                            precomputed_key,
                            module,
                        )?;
                    }
                }
                ClassElement::StaticBlock(block) => {
                    self.compile_static_block(block, constructor_value, module)?;
                }
                _ => {} // methods & instance fields handled above
            }
        }

        // Clean up ALL synthetic computed-key bindings from scope.
        for binding_name in computed_key_bindings_per_element.iter().flatten() {
            self.scope
                .borrow_mut()
                .bindings
                .remove(binding_name.as_str());
        }

        // §15.7.15 — Restore the previous class name binding (or remove
        // the temporary one if there was no prior binding).
        if let Some(old_binding) = saved_class_name_binding {
            match old_binding {
                Some(prev) => {
                    self.scope
                        .borrow_mut()
                        .bindings
                        .insert(class_name.to_string(), prev);
                }
                None => {
                    self.scope.borrow_mut().bindings.remove(class_name);
                }
            }
        }

        Ok(constructor_value)
    }

    /// §15.7.14 step 29 — Compile a synthetic function that initializes instance
    /// fields and store it on the constructor via SetClassFieldInitializer.
    /// Spec: <https://tc39.es/ecma262/#sec-definefield>
    fn compile_class_field_initializer(
        &mut self,
        class: &Class<'_>,
        constructor_value: ValueLocation,
        prototype: ValueLocation,
        has_private_members: bool,
        computed_key_bindings: &[String],
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        use super::ast::non_computed_property_key_name;
        use oxc_ast::ast::{ClassElement, PropertyDefinitionType};

        // Compile the synthetic initializer function.
        // It receives `this` as receiver and defines each instance field.
        let class_name = class
            .id
            .as_ref()
            .map(|id| id.name.as_str())
            .unwrap_or("anonymous");
        let reserved = module.reserve_function();
        let mut init_compiler = FunctionCompiler::new(
            self.mode,
            Some(format!("{class_name}.__field_init__")),
            super::shared::FunctionKind::Ordinary,
            self.parent_scopes_for_child(),
            module.source_mapper(),
        );
        init_compiler.strict_mode = true;
        init_compiler.declare_parameters(&[])?;
        init_compiler.declare_this_binding()?;
        init_compiler.reserve_arguments_binding_slot()?;
        init_compiler.compile_parameter_initialization(&[], module)?;

        // Load `this` for field definitions.
        let this_reg = init_compiler.alloc_temp();
        init_compiler
            .instructions
            .push(Instruction::load_this(this_reg));
        let this_reg = init_compiler
            .stabilize_binding_value(ValueLocation::temp(this_reg))?
            .register;

        // Emit field definitions in source order.
        let mut computed_key_index = 0usize;
        for element in &class.body.body {
            if let ClassElement::PropertyDefinition(prop) = element
                && !prop.r#static
                && !prop.declare
                && !matches!(
                    prop.r#type,
                    PropertyDefinitionType::TSAbstractPropertyDefinition
                )
            {
                if let oxc_ast::ast::PropertyKey::PrivateIdentifier(ident) = &prop.key {
                    // §7.3.31 PrivateFieldAdd — private instance field.
                    let value = if let Some(init_expr) = &prop.value {
                        init_compiler.compile_expression(init_expr, module)?
                    } else {
                        init_compiler.load_undefined()?
                    };
                    let prop_id = init_compiler.intern_property_name(ident.name.as_str())?;
                    init_compiler
                        .instructions
                        .push(Instruction::define_private_field(
                            this_reg,
                            value.register,
                            prop_id,
                        ));
                    init_compiler.release(value);
                } else {
                    // Public instance field.
                    let value = if let Some(init_expr) = &prop.value {
                        init_compiler.compile_expression(init_expr, module)?
                    } else {
                        init_compiler.load_undefined()?
                    };

                    if prop.computed {
                        // Key was pre-evaluated at class definition time.
                        // Resolve the captured binding (creates an upvalue).
                        let binding_name = &computed_key_bindings[computed_key_index];
                        computed_key_index += 1;
                        let key = init_compiler.compile_identifier(binding_name)?;
                        init_compiler
                            .instructions
                            .push(Instruction::define_computed_field(
                                this_reg,
                                key.register,
                                value.register,
                            ));
                        init_compiler.release(key);
                    } else {
                        let key_name =
                            non_computed_property_key_name(&prop.key).ok_or_else(|| {
                                SourceLoweringError::Unsupported("unnamed class field".to_string())
                            })?;
                        let prop_id = init_compiler.intern_property_name(&key_name)?;
                        init_compiler.instructions.push(Instruction::define_field(
                            this_reg,
                            value.register,
                            prop_id,
                        ));
                    }
                    init_compiler.release(value);
                }
            }
        }

        init_compiler.emit_implicit_return()?;
        let compiled = init_compiler.finish(reserved, 0, Some("__field_init__"))?;
        module.set_function(reserved, compiled.function);

        // Create the closure and attach it to the constructor.
        let init_closure = ValueLocation::temp(self.alloc_temp());
        self.emit_new_closure(init_closure.register, reserved, &compiled.captures)?;

        // Propagate class_id so DefinePrivateField can resolve private names.
        if has_private_members {
            self.instructions.push(Instruction::copy_class_id(
                init_closure.register,
                constructor_value.register,
            ));
        }

        // §10.2.5 MakeMethod — set [[HomeObject]] on the field initializer
        // so that `super.x` inside a field initializer (or an `eval()` inside
        // it) resolves against the class prototype.
        // Spec: <https://tc39.es/ecma262/#sec-makemethod>
        self.instructions.push(Instruction::set_home_object(
            init_closure.register,
            prototype.register,
        ));

        self.instructions
            .push(Instruction::set_class_field_initializer(
                constructor_value.register,
                init_closure.register,
            ));
        self.release(init_closure);

        Ok(())
    }

    /// §15.7.14 step 34 — Compile a single static field definition.
    /// Evaluates the initializer in a synthetic function with `this` = constructor,
    /// then defines the property on the constructor via DefineField.
    /// `precomputed_key_binding` is the name of the synthetic binding holding
    /// the already-coerced property key for computed fields (pre-evaluated in
    /// source order alongside instance computed keys).
    /// Spec: <https://tc39.es/ecma262/#sec-definefield>
    fn compile_static_field(
        &mut self,
        prop: &oxc_ast::ast::PropertyDefinition<'_>,
        constructor_value: ValueLocation,
        precomputed_key_binding: Option<&str>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        use super::ast::non_computed_property_key_name;

        if prop.computed {
            // §15.7.14 step 27: computed key was already evaluated in source
            // order by compile_class_body_inner's pre-evaluation pass, which
            // guarantees correct interleaving of static and instance field
            // key evaluation relative to other ClassElementEvaluation steps.
            // The pre-evaluated value is stored in a synthetic local binding.
            let binding_name = precomputed_key_binding.expect(
                "compile_class_body_inner must pre-evaluate every computed \
                 static field key before compile_static_field is called",
            );
            let key = self.compile_identifier(binding_name)?;
            let key = self.stabilize_binding_value(key)?;

            let value = self.compile_static_field_value(prop, constructor_value, module)?;

            self.instructions.push(Instruction::define_computed_field(
                constructor_value.register,
                key.register,
                value.register,
            ));
            self.release(key);
            self.release(value);
        } else {
            let key_name = non_computed_property_key_name(&prop.key).ok_or_else(|| {
                SourceLoweringError::Unsupported("unnamed static field".to_string())
            })?;
            let prop_id = self.intern_property_name(&key_name)?;

            let value = self.compile_static_field_value(prop, constructor_value, module)?;

            self.instructions.push(Instruction::define_field(
                constructor_value.register,
                value.register,
                prop_id,
            ));
            self.release(value);
        }
        Ok(())
    }

    /// §15.7.14 — Compile a static private field definition.
    /// Evaluates the initializer via a synthetic function with `this` = constructor,
    /// then emits DefinePrivateField on the constructor.
    /// Spec: <https://tc39.es/ecma262/#sec-definefield>
    fn compile_static_private_field(
        &mut self,
        name: &str,
        prop: &oxc_ast::ast::PropertyDefinition<'_>,
        constructor_value: ValueLocation,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        let value = self.compile_static_field_value(prop, constructor_value, module)?;
        let prop_id = self.intern_property_name(name)?;
        self.instructions.push(Instruction::define_private_field(
            constructor_value.register,
            value.register,
            prop_id,
        ));
        self.release(value);
        Ok(())
    }

    /// §15.7.14 — Compile a private class method/getter/setter.
    ///
    /// For **instance** methods: emits PushPrivateMethod/Getter/Setter on the
    /// constructor — these get copied to instances during RunClassFieldInitializer.
    /// For **static** methods: emits DefinePrivateMethod/Getter/Setter on the
    /// constructor directly (adds to constructor's [[PrivateElements]]).
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
    fn compile_private_class_method(
        &mut self,
        method: &oxc_ast::ast::MethodDefinition<'_>,
        constructor_value: ValueLocation,
        prototype: ValueLocation,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        use super::ast::extract_function_params;

        let private_name = match &method.key {
            oxc_ast::ast::PropertyKey::PrivateIdentifier(ident) => ident.name.as_str(),
            _ => unreachable!("compile_private_class_method called with non-private key"),
        };

        let display_name = match method.kind {
            MethodDefinitionKind::Get => format!("get #{private_name}"),
            MethodDefinitionKind::Set => format!("set #{private_name}"),
            _ => format!("#{private_name}"),
        };

        // Compile the method body as a closure.
        let function = &method.value;
        let reserved = module.reserve_function();
        let params = extract_function_params(function)?;
        let kind = if method.value.generator && method.value.r#async {
            super::shared::FunctionKind::AsyncGenerator
        } else if method.value.generator {
            super::shared::FunctionKind::Generator
        } else if method.value.r#async {
            super::shared::FunctionKind::Async
        } else {
            super::shared::FunctionKind::Ordinary
        };
        let compiled = module.compile_function_from_statements(
            reserved,
            super::module_compiler::FunctionIdentity {
                debug_name: Some(display_name),
                self_binding_name: None,
                length: super::ast::expected_function_length(&params),
            },
            function
                .body
                .as_ref()
                .map(|body| body.statements.as_slice())
                .unwrap_or(&[]),
            &params,
            kind,
            self.parent_scopes_for_child(),
            true, // class bodies are always strict
        )?;
        module.set_function(reserved, compiled.function);

        let method_closure = ValueLocation::temp(self.alloc_temp());
        if kind.is_generator() && kind.is_async() {
            self.emit_new_closure_async_generator(
                method_closure.register,
                reserved,
                &compiled.captures,
            )?;
        } else if kind.is_generator() {
            self.emit_new_closure_generator(method_closure.register, reserved, &compiled.captures)?;
        } else if kind.is_async() {
            self.emit_new_closure_async(method_closure.register, reserved, &compiled.captures)?;
        } else {
            // §15.4.4 — private methods are MethodDefinitions; no own .prototype.
            self.emit_new_closure_method(method_closure.register, reserved, &compiled.captures)?;
        }

        // Propagate class_id so the method can resolve private names.
        self.instructions.push(Instruction::copy_class_id(
            method_closure.register,
            constructor_value.register,
        ));

        // §10.2.5 MakeMethod — set `[[HomeObject]]` so `super.foo` inside the
        // private method body resolves correctly. Static private methods
        // use the constructor; instance private methods use the prototype.
        let home_register = if method.r#static {
            constructor_value.register
        } else {
            prototype.register
        };
        self.instructions.push(Instruction::set_home_object(
            method_closure.register,
            home_register,
        ));

        let prop_id = self.intern_property_name(private_name)?;

        if method.r#static {
            // Static private: add directly to constructor's [[PrivateElements]].
            match method.kind {
                MethodDefinitionKind::Method => {
                    self.instructions.push(Instruction::define_private_method(
                        constructor_value.register,
                        method_closure.register,
                        prop_id,
                    ));
                }
                MethodDefinitionKind::Get => {
                    self.instructions.push(Instruction::define_private_getter(
                        constructor_value.register,
                        method_closure.register,
                        prop_id,
                    ));
                }
                MethodDefinitionKind::Set => {
                    self.instructions.push(Instruction::define_private_setter(
                        constructor_value.register,
                        method_closure.register,
                        prop_id,
                    ));
                }
                MethodDefinitionKind::Constructor => {
                    unreachable!("constructor handled in first pass")
                }
            }
        } else {
            // Instance private: push to constructor's [[PrivateMethods]].
            // Copied to instances during RunClassFieldInitializer.
            match method.kind {
                MethodDefinitionKind::Method => {
                    self.instructions.push(Instruction::push_private_method(
                        constructor_value.register,
                        method_closure.register,
                        prop_id,
                    ));
                }
                MethodDefinitionKind::Get => {
                    self.instructions.push(Instruction::push_private_getter(
                        constructor_value.register,
                        method_closure.register,
                        prop_id,
                    ));
                }
                MethodDefinitionKind::Set => {
                    self.instructions.push(Instruction::push_private_setter(
                        constructor_value.register,
                        method_closure.register,
                        prop_id,
                    ));
                }
                MethodDefinitionKind::Constructor => {
                    unreachable!("constructor handled in first pass")
                }
            }
        }

        self.release(method_closure);
        let _ = prototype; // prototype is not used for private methods
        Ok(())
    }

    /// Compile a static field initializer value. If the field has an initializer,
    /// it's compiled as a synthetic function called with `this` = constructor
    /// (per spec, static field initializers evaluate with `this` bound to the class).
    /// Returns the result value location.
    fn compile_static_field_value(
        &mut self,
        prop: &oxc_ast::ast::PropertyDefinition<'_>,
        constructor_value: ValueLocation,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let Some(init_expr) = &prop.value else {
            return self.load_undefined();
        };

        // Compile the initializer as a synthetic function that returns the value.
        // This ensures `this` inside the initializer refers to the constructor.
        let reserved = module.reserve_function();
        let compiled = module.compile_function_from_expression(
            reserved,
            super::module_compiler::FunctionIdentity {
                debug_name: Some("static_field_init".to_string()),
                self_binding_name: None,
                length: 0,
            },
            init_expr,
            &[],
            super::shared::FunctionKind::Ordinary,
            self.parent_scopes_for_child(),
            true,
        )?;
        module.set_function(reserved, compiled.function);

        let init_closure = ValueLocation::temp(self.alloc_temp());
        self.emit_new_closure(init_closure.register, reserved, &compiled.captures)?;

        // Call with constructor as receiver.
        let argument_count = 1u16;
        let arg_start = self.reserve_temp_window(argument_count)?;
        if constructor_value.register != arg_start {
            self.instructions
                .push(Instruction::move_(arg_start, constructor_value.register));
        }

        let result = self.alloc_temp();
        let pc = self.instructions.len();
        self.instructions.push(Instruction::call_closure(
            result,
            init_closure.register,
            arg_start,
        ));
        self.record_call_site(
            pc,
            crate::call::CallSite::Closure(crate::call::ClosureCall::new_with_receiver(
                argument_count,
                crate::frame::FrameFlags::new(false, true, false),
                arg_start,
            )),
        );
        self.release_temp_window(argument_count);
        self.release(init_closure);

        Ok(ValueLocation::temp(result))
    }

    /// §15.7.12 StaticBlock — `static { ... }`
    /// Compiled as an IIFE with `this` bound to the constructor.
    /// Spec: <https://tc39.es/ecma262/#sec-static-blocks>
    fn compile_static_block(
        &mut self,
        block: &oxc_ast::ast::StaticBlock<'_>,
        constructor_value: ValueLocation,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        // Compile the static block body as a synthetic function.
        let reserved = module.reserve_function();
        let compiled = module.compile_function_from_statements(
            reserved,
            super::module_compiler::FunctionIdentity {
                debug_name: Some("static".to_string()),
                self_binding_name: None,
                length: 0,
            },
            &block.body,
            &[],
            super::shared::FunctionKind::Ordinary,
            self.parent_scopes_for_child(),
            true, // class bodies are always strict
        )?;
        module.set_function(reserved, compiled.function);

        // Create closure and immediately invoke with constructor as `this`.
        let block_closure = ValueLocation::temp(self.alloc_temp());
        self.emit_new_closure(block_closure.register, reserved, &compiled.captures)?;

        // Call: block_closure() with `this` = constructor.
        // argument_count = 1 because the receiver occupies one slot in the window.
        let argument_count = 1u16;
        let arg_start = self.reserve_temp_window(argument_count)?;
        if constructor_value.register != arg_start {
            self.instructions
                .push(Instruction::move_(arg_start, constructor_value.register));
        }

        let result = self.alloc_temp();
        let pc = self.instructions.len();
        self.instructions.push(Instruction::call_closure(
            result,
            block_closure.register,
            arg_start,
        ));
        self.record_call_site(
            pc,
            crate::call::CallSite::Closure(crate::call::ClosureCall::new_with_receiver(
                argument_count,
                crate::frame::FrameFlags::new(false, true, false),
                arg_start,
            )),
        );
        self.release(ValueLocation::temp(result));
        self.release_temp_window(argument_count);
        self.release(block_closure);
        Ok(())
    }

    /// Compiles a class method and installs it on the target (prototype or constructor).
    ///
    /// Handles regular methods, getters, setters — named and computed keys.
    /// If `class_id_source` is provided, emits CopyClassId on the closure before
    /// installing (so methods can resolve private names at runtime).
    /// §15.4.5 MethodDefinitionEvaluation
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-methoddefinitionevaluation>
    pub(super) fn compile_class_method(
        &mut self,
        method: &oxc_ast::ast::MethodDefinition<'_>,
        target: ValueLocation,
        class_id_source: Option<BytecodeRegister>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        use super::ast::{
            expected_function_length, extract_function_params, non_computed_property_key_name,
        };

        // Determine method name for debug/display.
        let method_name = if method.computed {
            None
        } else {
            non_computed_property_key_name(&method.key)
        };

        let display_name = match (&method.kind, &method_name) {
            (MethodDefinitionKind::Get, Some(n)) => Some(format!("get {n}")),
            (MethodDefinitionKind::Set, Some(n)) => Some(format!("set {n}")),
            (_, Some(n)) => Some(n.to_string()),
            _ => None,
        };

        // Compile the method body as a closure.
        let function = &method.value;
        let reserved = module.reserve_function();
        let params = extract_function_params(function)?;
        // §15.2.1.1 / §15.4.1 MethodDefinition Static Semantics: Early Errors —
        // It is a SyntaxError if `FunctionBody` Contains `"use strict"` and
        // `IsSimpleParameterList(FormalParameters)` is false.
        if let Some(body) = function.body.as_ref()
            && super::ast::has_use_strict_directive(&body.directives)
            && !super::ast::is_simple_parameter_list(&params)
        {
            return Err(SourceLoweringError::EarlyError(format!(
                "Illegal 'use strict' directive in function `{}` with non-simple parameter list",
                display_name.as_deref().unwrap_or("<anonymous>")
            )));
        }
        let kind = if method.value.generator && method.value.r#async {
            super::shared::FunctionKind::AsyncGenerator
        } else if method.value.generator {
            super::shared::FunctionKind::Generator
        } else if method.value.r#async {
            super::shared::FunctionKind::Async
        } else {
            super::shared::FunctionKind::Ordinary
        };
        // Propagate private name context so inner closures can resolve
        // private field accesses via CopyClassId at runtime.
        let saved_private_ctx = module.pending_has_class_private_context;
        module.pending_has_class_private_context = class_id_source.is_some();
        let compiled = module.compile_function_from_statements(
            reserved,
            super::module_compiler::FunctionIdentity {
                debug_name: display_name.clone(),
                self_binding_name: None,
                length: expected_function_length(&params),
            },
            function
                .body
                .as_ref()
                .map(|body| body.statements.as_slice())
                .unwrap_or(&[]),
            &params,
            kind,
            self.parent_scopes_for_child(),
            true, // class bodies are always strict
        )?;
        module.pending_has_class_private_context = saved_private_ctx;
        module.set_function(reserved, compiled.function);

        let method_closure = ValueLocation::temp(self.alloc_temp());
        if kind.is_generator() && kind.is_async() {
            self.emit_new_closure_async_generator(
                method_closure.register,
                reserved,
                &compiled.captures,
            )?;
        } else if kind.is_generator() {
            self.emit_new_closure_generator(method_closure.register, reserved, &compiled.captures)?;
        } else if kind.is_async() {
            self.emit_new_closure_async(method_closure.register, reserved, &compiled.captures)?;
        } else {
            // §15.4.4 — class methods, getters, and setters are
            // MethodDefinitions and must not be constructors (§10.2 MakeMethod).
            // Use the method closure flag so no own `.prototype` is installed.
            self.emit_new_closure_method(method_closure.register, reserved, &compiled.captures)?;
        }

        // §10.2.5 MakeMethod — set `[[HomeObject]]` on the method closure so
        // that subsequent `super.foo` / `super[x]` references inside the body
        // resolve against `HomeObject.[[Prototype]]`. `target` is the
        // prototype for instance members or the constructor for static
        // members, which is exactly what the spec wants.
        self.instructions.push(Instruction::set_home_object(
            method_closure.register,
            target.register,
        ));

        // Install on target: getter, setter, or data method.
        // §15.7.14 ClassDefinitionEvaluation step 28 — class methods have
        // `[[Enumerable]]: false` and are installed via `[[DefineOwnProperty]]`.
        match method.kind {
            MethodDefinitionKind::Get => {
                if method.computed {
                    let key = self.compile_expression(method.key.to_expression(), module)?;
                    self.instructions
                        .push(Instruction::define_class_getter_computed(
                            target.register,
                            key.register,
                            method_closure.register,
                        ));
                    self.release(key);
                } else {
                    let name = method_name.as_ref().ok_or_else(|| {
                        SourceLoweringError::Unsupported("unnamed class getter".to_string())
                    })?;
                    let prop = self.intern_property_name(name)?;
                    self.instructions.push(Instruction::define_class_getter(
                        target.register,
                        method_closure.register,
                        prop,
                    ));
                }
            }
            MethodDefinitionKind::Set => {
                if method.computed {
                    let key = self.compile_expression(method.key.to_expression(), module)?;
                    self.instructions
                        .push(Instruction::define_class_setter_computed(
                            target.register,
                            key.register,
                            method_closure.register,
                        ));
                    self.release(key);
                } else {
                    let name = method_name.as_ref().ok_or_else(|| {
                        SourceLoweringError::Unsupported("unnamed class setter".to_string())
                    })?;
                    let prop = self.intern_property_name(name)?;
                    self.instructions.push(Instruction::define_class_setter(
                        target.register,
                        method_closure.register,
                        prop,
                    ));
                }
            }
            MethodDefinitionKind::Method => {
                if method.computed {
                    let key = self.compile_expression(method.key.to_expression(), module)?;
                    self.instructions
                        .push(Instruction::define_class_method_computed(
                            target.register,
                            key.register,
                            method_closure.register,
                        ));
                    self.release(key);
                } else {
                    let name = method_name.as_ref().ok_or_else(|| {
                        SourceLoweringError::Unsupported("unnamed class method".to_string())
                    })?;
                    let prop = self.intern_property_name(name)?;
                    self.instructions.push(Instruction::define_class_method(
                        target.register,
                        method_closure.register,
                        prop,
                    ));
                }
            }
            MethodDefinitionKind::Constructor => unreachable!("constructor handled in first pass"),
        }

        // Propagate class_id so the method can resolve private names at runtime.
        if let Some(source) = class_id_source {
            self.instructions
                .push(Instruction::copy_class_id(method_closure.register, source));
        }

        self.release(method_closure);
        Ok(())
    }

    pub(super) fn compile_default_base_class_constructor(
        &mut self,
        class_name: &str,
        has_self_binding: bool,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let reserved = module.reserve_function();
        let compiled = module.compile_function_from_statements(
            reserved,
            FunctionIdentity {
                debug_name: Some(class_name.to_string()),
                self_binding_name: if has_self_binding {
                    Some(class_name.to_string())
                } else {
                    None
                },
                length: 0,
            },
            &[],
            &[],
            FunctionKind::Ordinary,
            self.parent_scopes_for_child(),
            true,
        )?;
        module.set_function(reserved, compiled.function);

        let destination = self.alloc_temp();
        self.emit_new_closure_class_constructor(destination, reserved, &compiled.captures)?;
        Ok(ValueLocation::temp(destination))
    }

    /// §15.7.14 — Compile an explicit class constructor with field support.
    /// For base class: emits RunClassFieldInitializer at the start.
    /// For derived class: relies on compile_super_call_* to emit RunClassFieldInitializer.
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
    fn compile_class_constructor_with_fields(
        &mut self,
        class_name: &str,
        has_self_binding: bool,
        constructor: &Function<'_>,
        derived: bool,
        has_instance_fields: bool,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let reserved = module.reserve_function();
        let params = extract_function_params(constructor)?;

        // For base class constructors with fields, we need to inject
        // RunClassFieldInitializer at the start of the body. We do this by
        // adding has_instance_fields to the function compiler state.
        let mut compiler = FunctionCompiler::new(
            self.mode,
            Some(class_name.to_string()),
            FunctionKind::Ordinary,
            self.parent_scopes_for_child(),
            module.source_mapper(),
        );
        compiler.strict_mode = true;
        compiler.is_derived_constructor = derived;
        compiler.has_instance_fields = has_instance_fields;

        compiler.declare_parameters(&params)?;
        compiler.declare_this_binding()?;
        compiler.reserve_arguments_binding_slot()?;
        compiler.compile_parameter_initialization(&params, module)?;
        // §15.7.15 step 12.b: only named classes get an inner self-binding
        // for the class body.
        if has_self_binding {
            let closure_register = compiler.declare_function_binding(class_name)?;
            compiler
                .instructions
                .push(Instruction::load_current_closure(closure_register));
        }

        // For base class: emit RunClassFieldInitializer before user code.
        if !derived && has_instance_fields {
            compiler
                .instructions
                .push(Instruction::run_class_field_initializer());
        }

        compiler.predeclare_function_scope(
            constructor
                .body
                .as_ref()
                .map(|body| body.statements.as_slice())
                .unwrap_or(&[]),
            module,
        )?;
        compiler.emit_hoisted_function_initializers()?;
        let terminated = compiler.compile_statements(
            constructor
                .body
                .as_ref()
                .map(|body| body.statements.as_slice())
                .unwrap_or(&[]),
            module,
        )?;
        if !terminated {
            compiler.emit_implicit_return()?;
        }

        let compiled = compiler.finish(
            reserved,
            expected_function_length(&params),
            Some(class_name),
        )?;
        module.set_function(reserved, compiled.function);

        let destination = self.alloc_temp();
        self.emit_new_closure_class_constructor(destination, reserved, &compiled.captures)?;
        Ok(ValueLocation::temp(destination))
    }

    /// §15.7.14 — Default base class constructor with field initializer support.
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
    fn compile_default_base_class_constructor_with_fields(
        &mut self,
        class_name: &str,
        has_self_binding: bool,
        has_instance_fields: bool,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        if !has_instance_fields {
            return self.compile_default_base_class_constructor(
                class_name,
                has_self_binding,
                module,
            );
        }
        let reserved = module.reserve_function();
        let mut compiler = FunctionCompiler::new(
            self.mode,
            Some(class_name.to_string()),
            FunctionKind::Ordinary,
            self.parent_scopes_for_child(),
            module.source_mapper(),
        );
        compiler.strict_mode = true;
        compiler.has_instance_fields = true;
        compiler.declare_parameters(&[])?;
        compiler.declare_this_binding()?;
        compiler.reserve_arguments_binding_slot()?;
        compiler.compile_parameter_initialization(&[], module)?;
        if has_self_binding {
            let closure_register = compiler.declare_function_binding(class_name)?;
            compiler
                .instructions
                .push(Instruction::load_current_closure(closure_register));
        }

        // Emit RunClassFieldInitializer for instance fields.
        compiler
            .instructions
            .push(Instruction::run_class_field_initializer());

        compiler.emit_implicit_return()?;
        let compiled = compiler.finish(reserved, 0, Some(class_name))?;
        module.set_function(reserved, compiled.function);

        let destination = self.alloc_temp();
        self.emit_new_closure_class_constructor(destination, reserved, &compiled.captures)?;
        Ok(ValueLocation::temp(destination))
    }

    /// §15.7.14 — Default derived class constructor with field initializer support.
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
    fn compile_default_derived_class_constructor_with_fields(
        &mut self,
        class_name: &str,
        has_self_binding: bool,
        has_instance_fields: bool,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let reserved = module.reserve_function();
        let mut compiler = FunctionCompiler::new(
            self.mode,
            Some(class_name.to_string()),
            FunctionKind::Ordinary,
            self.parent_scopes_for_child(),
            module.source_mapper(),
        );
        compiler.strict_mode = true;
        compiler.is_derived_constructor = true;
        compiler.has_instance_fields = has_instance_fields;
        compiler.declare_parameters(&[])?;
        compiler.declare_this_binding()?;
        compiler.reserve_arguments_binding_slot()?;
        compiler.compile_parameter_initialization(&[], module)?;
        if has_self_binding {
            let closure_register = compiler.declare_function_binding(class_name)?;
            compiler
                .instructions
                .push(Instruction::load_current_closure(closure_register));
        }
        let forwarded = ValueLocation::temp(compiler.alloc_temp());
        compiler
            .instructions
            .push(Instruction::call_super_forward(forwarded.register));
        if let Some(Binding::ThisRegister(this_register)) =
            compiler.scope.borrow().bindings.get("this").copied()
            && this_register != forwarded.register
        {
            compiler
                .instructions
                .push(Instruction::move_(this_register, forwarded.register));
        }

        // For derived class with fields: emit RunClassFieldInitializer after super().
        if has_instance_fields {
            compiler
                .instructions
                .push(Instruction::run_class_field_initializer());
        }

        compiler.release(forwarded);
        compiler.emit_implicit_return()?;
        let compiled = compiler.finish(reserved, 0, Some(class_name))?;
        module.set_function(reserved, compiled.function);

        let destination = self.alloc_temp();
        self.emit_new_closure_class_constructor(destination, reserved, &compiled.captures)?;
        Ok(ValueLocation::temp(destination))
    }

    pub(super) fn emit_named_property_load(
        &mut self,
        base: ValueLocation,
        name: &str,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let property = self.intern_property_name(name)?;
        let result = ValueLocation::temp(self.alloc_temp());
        self.instructions.push(Instruction::get_property(
            result.register,
            base.register,
            property,
        ));
        Ok(result)
    }

    pub(super) fn emit_make_class_prototype_non_writable(
        &mut self,
        constructor: ValueLocation,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        let descriptor = ValueLocation::temp(self.alloc_temp());
        self.instructions
            .push(Instruction::new_object(descriptor.register));
        let writable = self.compile_bool(false)?;
        let writable_key = self.intern_property_name("writable")?;
        self.instructions.push(Instruction::set_property(
            descriptor.register,
            writable.register,
            writable_key,
        ));
        self.release(writable);

        let prototype_key = self.compile_string_literal("prototype")?;
        self.emit_object_method_call(
            "defineProperty",
            constructor,
            &[prototype_key, descriptor],
            module,
        )?;
        Ok(())
    }

    pub(super) fn emit_object_method_call(
        &mut self,
        method_name: &str,
        receiver: ValueLocation,
        args: &[ValueLocation],
        _module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        let object = self.compile_identifier("Object")?;
        let object = if object.is_temp {
            self.stabilize_binding_value(object)?
        } else {
            object
        };
        let callee = self.emit_named_property_load(object, method_name)?;
        let callee = if callee.is_temp {
            self.stabilize_binding_value(callee)?
        } else {
            callee
        };

        let argument_count = RegisterIndex::try_from(args.len() + 1)
            .map_err(|_| SourceLoweringError::TooManyLocals)?;
        let arg_start = self.reserve_temp_window(argument_count)?;
        let values: Vec<ValueLocation> = std::iter::once(receiver)
            .chain(args.iter().copied())
            .collect();
        for (offset, value) in values.into_iter().enumerate() {
            let destination = BytecodeRegister::new(arg_start.index() + offset as u16);
            if value.register != destination {
                self.instructions
                    .push(Instruction::move_(destination, value.register));
                self.release(value);
            }
        }

        let result = self.alloc_temp();
        let pc = self.instructions.len();
        self.instructions.push(Instruction::call_closure(
            result,
            callee.register,
            arg_start,
        ));
        self.record_call_site(
            pc,
            CallSite::Closure(ClosureCall::new_with_receiver(
                argument_count,
                FrameFlags::new(false, true, false),
                object.register,
            )),
        );
        self.release(ValueLocation::temp(result));
        self.release_temp_window(argument_count);
        Ok(())
    }
}
