//! Global binding opcode helpers.
//!
//! These are fixed-width global environment reads and writes that dispatch
//! directly from executable operands.
//!
//! # Contents
//! - `globalThis` load.
//! - Throwing global binding lookup with lexical-cell and guarded global-object
//!   slot caches for ordinary identifier reads.
//! - Undefined-returning global lookup for `typeof`.
//! - Global declaration, initialization, assignment, and deletion helpers.
//!
//! # Invariants
//! - Global properties live on the interpreter's `global_this` object.
//! - A global-object slot cache is valid only while the object's hidden-class
//!   identity matches and no later global lexical declaration shadows it.
//! - Missing throwing lookups surface as `UndefinedIdentifier` so the normal
//!   error path can synthesize a `ReferenceError`.
//! - Identifier assignment routes through descriptor-aware object `[[Set]]`;
//!   raw property writes are reserved for declaration/bootstrap paths.
//! - Dynamic bindings have one authority: the executing frame's traced
//!   [`crate::eval_env::EvalEnvHandle`] chain. The nearest record wins.
//!
//! # See also
//! - [`crate::executable`]
//! - [`crate::object`]

use smallvec::SmallVec;

use crate::{
    ActiveFrameMut, ExecutionContext, Frame, Interpreter, Value, VmError, VmGetOutcome,
    VmPropertyKey, activation_stack::ActivationStack, eval_env::EvalEnvHandle, object,
    write_register,
};

/// Guarded own-data slot for one global object-record load site.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GlobalObjectLoadCache {
    shape_id: object::ShapeId,
    slot: u16,
}

impl Interpreter {
    pub(crate) fn run_load_global_this_reg(
        &self,
        frame: &mut Frame,
        dst: u16,
    ) -> Result<(), VmError> {
        write_register(frame, dst, Value::object(self.global_this))?;
        frame.advance_pc()?;
        Ok(())
    }

    pub(crate) fn run_load_global_or_throw_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let function_id = stack[top_idx].function_id;
        self.run_load_global_or_throw_reg_for_function(
            context,
            stack,
            top_idx,
            function_id,
            dst,
            name_idx,
        )
    }

    pub(crate) fn run_load_global_or_throw_reg_for_function(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        function_id: u32,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let value = self.load_global_or_throw_value(stack, context, function_id, name_idx)?;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, value)?;
        frame.advance_pc()?;
        Ok(())
    }

    /// Resolve one throwing global binding read without coupling the semantic
    /// operation to a physical frame representation.
    ///
    /// Interpreter and compiled tiers commit the returned value through their
    /// own active-frame view. Accessor invocation, lexical caching, and TDZ
    /// errors therefore have one implementation while PC ownership remains at
    /// the dispatch tier.
    pub(crate) fn load_global_or_throw_value(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        function_id: u32,
        name_idx: u32,
    ) -> Result<Value, VmError> {
        // A previously-resolved lexical cell for this load site reads directly,
        // skipping the name-string hash and const-table lookup. The cell is
        // permanent once bound, so the only per-read check is the TDZ hole.
        if let Some(&cell) = self.global_lexical_load_ic.get(&(function_id, name_idx)) {
            let value = crate::read_upvalue(&self.gc_heap, cell);
            if !value.is_hole() {
                return Ok(value);
            }
            let name = context
                .string_constant_str_for_function(function_id, name_idx)
                .ok_or(VmError::InvalidOperand)?;
            return Err(self.err_this_uninit(
                (format!("Cannot access '{name}' before initialization")).into(),
            ));
        }
        // Object-record globals such as constructors and benchmark fixtures
        // dominate identifier reads in script code. A matching hidden class
        // proves this own data slot still denotes the same property, so read
        // its live value without repeating string lookup, `[[HasProperty]]`,
        // deferred-namespace checks, and the second `[[Get]]` walk.
        let site = (function_id, name_idx);
        if let Some(cache) = self.global_object_load_ic.get(&site).copied() {
            if object::shape_id(self.global_this, &self.gc_heap) == cache.shape_id {
                return Ok(object::data_value_at(
                    self.global_this,
                    &self.gc_heap,
                    cache.slot,
                ));
            }
            self.global_object_load_ic.remove(&site);
        }
        let name = context
            .string_constant_str_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)?;
        // §9.1.1.4 — the global declarative record (script lexicals)
        // shadows the object record.
        if let Some((cell, _)) = self.global_lexicals.get(name).copied() {
            self.global_lexical_load_ic
                .insert((function_id, name_idx), cell);
            let value = crate::read_upvalue(&self.gc_heap, cell);
            if value.is_hole() {
                return Err(self.err_this_uninit(
                    (format!("Cannot access '{name}' before initialization")).into(),
                ));
            }
            return Ok(value);
        }
        let (own_hit, own_lookup) = object::lookup_own_slot(self.global_this, &self.gc_heap, name);
        match own_lookup {
            object::PropertyLookup::Data { value, .. } => {
                if let Some(hit) = own_hit {
                    self.global_object_load_ic.insert(
                        site,
                        GlobalObjectLoadCache {
                            shape_id: hit.shape_id,
                            slot: hit.slot,
                        },
                    );
                }
                return Ok(value);
            }
            object::PropertyLookup::Accessor { getter, .. } => {
                return match getter {
                    Some(getter) if crate::abstract_ops::is_callable(&getter) => {
                        let receiver = Value::object(self.global_this);
                        self.run_callable_sync_rooted(
                            stack,
                            context,
                            &getter,
                            receiver,
                            SmallVec::new(),
                        )
                    }
                    _ => Ok(Value::undefined()),
                };
            }
            object::PropertyLookup::Absent => {}
        }
        let receiver = Value::object(self.global_this);
        let key = VmPropertyKey::String(name);
        if !self.ordinary_has_property_value(stack, context, receiver, &key, 0)? {
            return Err(self.err_undefined_ident((name.to_string()).into()));
        }
        let receiver = Value::object(self.global_this);
        let value = match self.ordinary_get_value(stack, context, receiver, receiver, &key, 0)? {
            VmGetOutcome::Value(value) => value,
            VmGetOutcome::InvokeGetter { getter } => {
                let receiver = Value::object(self.global_this);
                self.run_callable_sync_rooted(stack, context, &getter, receiver, SmallVec::new())?
            }
        };
        Ok(value)
    }

    pub(crate) fn run_load_global_or_undefined_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let function_id = stack[top_idx].function_id;
        let value = self.load_global_or_undefined_value(context, stack, function_id, name_idx)?;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, value)?;
        frame.advance_pc()?;
        Ok(())
    }

    pub(crate) fn load_global_or_undefined_value(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        function_id: u32,
        name_idx: u32,
    ) -> Result<Value, VmError> {
        let name = context
            .string_constant_str_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)?;
        // §13.5.3 — `typeof` still raises ReferenceError for a
        // lexical binding read inside its TDZ; only *unresolvable*
        // names yield `undefined`.
        let value = if let Some(value) = self.read_global_lexical(name)? {
            value
        } else {
            // §13.5.3 step 2 — GetValue on the resolved global
            // reference runs the ordinary [[Get]], so an
            // accessor-defined global fires its getter (and its
            // abrupt completion propagates).
            let receiver = Value::object(self.global_this);
            let key = crate::VmPropertyKey::String(name);
            match self.ordinary_get_value(stack, context, receiver, receiver, &key, 0)? {
                VmGetOutcome::Value(value) => value,
                VmGetOutcome::InvokeGetter { getter } => {
                    let receiver = Value::object(self.global_this);
                    self.run_callable_sync_rooted(
                        stack,
                        context,
                        &getter,
                        receiver,
                        SmallVec::new(),
                    )?
                }
            }
        };
        Ok(value)
    }

    /// Read a binding from the global declarative record. `Ok(None)`
    /// when the name has no global lexical binding;
    /// `Err(ReferenceError)` when the binding is still in its TDZ.
    pub(crate) fn read_global_lexical(&self, name: &str) -> Result<Option<Value>, VmError> {
        let Some((cell, _)) = self.global_lexicals.get(name) else {
            return Ok(None);
        };
        let value = crate::read_upvalue(&self.gc_heap, *cell);
        if value.is_hole() {
            // `ThisUninitialized` is the engine's named-TDZ
            // `ReferenceError` vehicle (same as module bindings).
            return Err(self.err_this_uninit(
                (format!("Cannot access '{name}' before initialization")).into(),
            ));
        }
        Ok(Some(value))
    }

    pub(crate) fn run_define_global_var_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        name_idx: u32,
        value_reg: u16,
    ) -> Result<(), VmError> {
        let value = *crate::read_register(frame, value_reg)?;
        self.define_global_var_value(context, frame.function_id, name_idx, value)?;
        frame.advance_pc()?;
        Ok(())
    }

    pub(crate) fn define_global_var_value(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        name_idx: u32,
        value: Value,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)?;
        // §9.1.1.4.18 SetMutableBinding shape — an existing own
        // property keeps its attributes (enumerability,
        // configurability) and only receives the new value; a
        // non-writable existing property silently absorbs the write
        // in sloppy mode. Only an absent property is defined fresh.
        if object::get_own_descriptor(self.global_this, &self.gc_heap, name).is_some() {
            object::set(&mut self.global_this, &mut self.gc_heap, name, value);
            return Ok(());
        }
        let descriptor = object::PartialPropertyDescriptor {
            value: Some(value),
            writable: Some(true),
            enumerable: Some(true),
            configurable: Some(true),
            ..Default::default()
        };
        if !object::define_own_property_partial(
            &mut self.global_this,
            &mut self.gc_heap,
            name,
            descriptor,
        ) {
            return Err(self.err_type((format!("Cannot declare global var '{name}'")).into()));
        }
        Ok(())
    }

    /// §9.1.1.4.17 CreateGlobalVarBinding — define `name` as a
    /// writable / enumerable / configurable `undefined` data property
    /// when absent; an existing own property is left untouched.
    pub(crate) fn run_declare_global_var_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        name_idx: u32,
        configurable: bool,
    ) -> Result<(), VmError> {
        self.declare_global_var_value(context, frame.function_id, name_idx, configurable)?;
        frame.advance_pc()?;
        Ok(())
    }

    pub(crate) fn declare_global_var_value(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        name_idx: u32,
        configurable: bool,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)?;
        // §19.2.1.3 step 5 — a var-scoped name colliding with a
        // global *lexical* binding is a SyntaxError at declaration
        // time (script collisions are early errors; eval collisions
        // surface here).
        if self.global_lexicals.contains_key(name) {
            return Err(
                self.err_syntax((format!("Identifier '{name}' has already been declared")).into())
            );
        }
        if object::get_own_descriptor(self.global_this, &self.gc_heap, name).is_none() {
            let descriptor = object::PartialPropertyDescriptor {
                value: Some(Value::undefined()),
                writable: Some(true),
                enumerable: Some(true),
                configurable: Some(configurable),
                ..Default::default()
            };
            // §9.1.1.4.15/16 CanDeclareGlobalVar — a non-extensible
            // global object cannot accept the new binding.
            if !object::define_own_property_partial(
                &mut self.global_this,
                &mut self.gc_heap,
                name,
                descriptor,
            ) {
                return Err(
                    self.err_type((format!("Cannot declare global variable '{name}'")).into())
                );
            }
        }
        Ok(())
    }

    /// `Op::DefineGlobalFunction` — §9.1.1.4.18
    /// CreateGlobalFunctionBinding: absent / configurable existing
    /// own properties are redefined as `{value, writable: true,
    /// enumerable: true, configurable: deletable}`; a
    /// non-configurable existing property must be a writable +
    /// enumerable data property (§9.1.1.4.16 CanDeclareGlobalFunction)
    /// and only receives the new value.
    pub(crate) fn run_define_global_function_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        name_idx: u32,
        value_reg: u16,
        deletable: bool,
    ) -> Result<(), VmError> {
        let value = *crate::read_register(frame, value_reg)?;
        self.define_global_function_value(context, frame.function_id, name_idx, value, deletable)?;
        frame.advance_pc()?;
        Ok(())
    }

    pub(crate) fn define_global_function_value(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        name_idx: u32,
        value: Value,
        deletable: bool,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let existing = object::get_own_descriptor(self.global_this, &self.gc_heap, name);
        let redefine = match &existing {
            None => true,
            Some(descriptor) => descriptor.flags.configurable(),
        };
        if redefine {
            let descriptor = object::PartialPropertyDescriptor {
                value: Some(value),
                writable: Some(true),
                enumerable: Some(true),
                configurable: Some(deletable),
                ..Default::default()
            };
            if !object::define_own_property_partial(
                &mut self.global_this,
                &mut self.gc_heap,
                name,
                descriptor,
            ) {
                return Err(
                    self.err_type((format!("Cannot declare global function '{name}'")).into())
                );
            }
        } else {
            let permitted = existing.as_ref().is_some_and(|descriptor| {
                matches!(descriptor.kind, object::DescriptorKind::Data { .. })
                    && descriptor.flags.writable()
                    && descriptor.flags.enumerable()
            });
            if !permitted {
                return Err(
                    self.err_type((format!("Cannot declare global function '{name}'")).into())
                );
            }
            object::set(&mut self.global_this, &mut self.gc_heap, name, value);
        }
        Ok(())
    }

    /// `Op::LoadDynamic` — identifier read in a function whose body
    /// contains a direct eval. §9.1.2.1 GetIdentifierReference over
    /// the runtime-extended function environment: an eval-introduced
    /// binding wins, otherwise the ordinary throwing global lookup
    /// runs.
    pub(crate) fn run_load_dynamic_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let frame = &stack[top_idx];
        let function_id = frame.function_id;
        let eval_env = (!frame.eval_env.is_null()).then_some(frame.eval_env);
        let value = self.load_dynamic_value(context, stack, function_id, eval_env, name_idx)?;
        write_register(&mut stack[top_idx], dst, value)?;
        stack[top_idx].advance_pc()?;
        Ok(())
    }

    pub(crate) fn load_dynamic_value(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        function_id: u32,
        eval_env: Option<EvalEnvHandle>,
        name_idx: u32,
    ) -> Result<Value, VmError> {
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        if let Some(cell) = self.eval_env_var(eval_env, name) {
            return Ok(crate::read_upvalue(&self.gc_heap, cell));
        }
        self.load_global_or_throw_value(stack, context, function_id, name_idx)
    }

    /// `Op::StoreDynamic` — §10.2.4.2 PutValue counterpart of
    /// [`Self::run_load_dynamic_reg`]: store through the
    /// eval-introduced binding when present, else the sloppy-mode
    /// `globalThis` property write.
    pub(crate) fn run_store_dynamic_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        value_reg: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let value = *crate::read_register(&stack[top_idx], value_reg)?;
        let frame = &stack[top_idx];
        let function_id = frame.function_id;
        let eval_env = (!frame.eval_env.is_null()).then_some(frame.eval_env);
        self.store_dynamic_value(
            context,
            stack,
            function_id,
            eval_env,
            value,
            name_idx,
            false,
        )?;
        stack[top_idx].advance_pc()?;
        Ok(())
    }

    pub(crate) fn store_dynamic_value(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        function_id: u32,
        eval_env: Option<EvalEnvHandle>,
        value: Value,
        name_idx: u32,
        strict: bool,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)?;
        if let Some(cell) = self.eval_env_var(eval_env, name) {
            crate::store_upvalue(&mut self.gc_heap, cell, value);
            return Ok(());
        }
        // Fall through to the full global SetMutableBinding so
        // realm-wide lexical bindings stay visible; strict mode keeps the
        // unresolvable-reference rejection.
        self.store_global_binding_value(context, stack, function_id, value, name_idx, strict)
    }

    /// `Op::TypeofDynamic` — `typeof` flavour of
    /// [`Self::run_load_dynamic_reg`]; an unresolvable name yields
    /// `undefined` instead of throwing (§13.5.3).
    pub(crate) fn run_typeof_dynamic_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let frame = &stack[top_idx];
        let function_id = frame.function_id;
        let eval_env = (!frame.eval_env.is_null()).then_some(frame.eval_env);
        let value = self.typeof_dynamic_value(context, stack, function_id, eval_env, name_idx)?;
        write_register(&mut stack[top_idx], dst, value)?;
        stack[top_idx].advance_pc()?;
        Ok(())
    }

    pub(crate) fn typeof_dynamic_value(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        function_id: u32,
        eval_env: Option<EvalEnvHandle>,
        name_idx: u32,
    ) -> Result<Value, VmError> {
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        if let Some(cell) = self.eval_env_var(eval_env, name) {
            return Ok(crate::read_upvalue(&self.gc_heap, cell));
        }
        self.load_global_or_undefined_value(context, stack, function_id, name_idx)
    }

    /// `Op::DeleteDynamic` — §13.5.1.2 delete of a name that may
    /// resolve to an eval-created var binding (§19.2.1.3
    /// CreateMutableBinding(vn, true) — deletable). Removes it from the
    /// nearest record in the frame's captured eval-env chain; otherwise falls
    /// through to the global-object delete, whose result reflects
    /// configurability.
    pub(crate) fn run_delete_dynamic_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let mut frame = ActiveFrameMut::materialized(frame);
        self.run_delete_dynamic_active_reg(context, &mut frame, dst, name_idx)
    }

    pub(crate) fn run_delete_dynamic_active_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let removed_env = frame.eval_env().is_some_and(|env| {
            crate::eval_env::eval_env_delete_chain(&mut self.gc_heap, env, name)
        });
        let removed = if removed_env {
            true
        } else if self.global_lexicals.contains_key(name) {
            // A global declarative binding is an Environment Record binding,
            // not a configurable property. Dynamic resolution found it, so
            // sloppy `delete identifier` must report `false` without falling
            // through to the unrelated global object record.
            false
        } else {
            crate::object::delete(self.global_this, &mut self.gc_heap, name)
        };
        frame.write(dst, Value::boolean(removed))?;
        frame.advance_pc()?;
        Ok(())
    }

    /// Look up an eval-introduced var binding through `frame`'s complete
    /// nearest-first environment chain.
    fn eval_env_var(
        &self,
        eval_env: Option<EvalEnvHandle>,
        name: &str,
    ) -> Option<crate::UpvalueCell> {
        let env = eval_env?;
        crate::eval_env::eval_env_lookup_chain(&self.gc_heap, env, name)
    }

    /// `Op::DeclareGlobalLex` — §9.1.1.4 CreateMutableBinding /
    /// CreateImmutableBinding on the global declarative record, with
    /// the §16.1.7 step 4–5 redeclaration / restricted-property
    /// validation.
    pub(crate) fn run_declare_global_lex_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        name_idx: u32,
        is_const: bool,
    ) -> Result<(), VmError> {
        self.declare_global_lex_value(context, frame.function_id, name_idx, is_const)?;
        frame.advance_pc()?;
        Ok(())
    }

    pub(crate) fn declare_global_lex_value(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        name_idx: u32,
        is_const: bool,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)?;
        if self.global_lexicals.contains_key(name) {
            return Err(
                self.err_syntax((format!("Identifier '{name}' has already been declared")).into())
            );
        }
        // §9.1.1.4.14 HasRestrictedGlobalProperty — an existing
        // non-configurable own property of the global object
        // (`undefined`, `NaN`, script vars, …) cannot be shadowed by
        // a lexical. Sloppy-eval-introduced vars are *configurable*
        // and may be shadowed (tc39/ecma262#2205 removed
        // [[VarNames]]; configurability is the only gate).
        if let Some(descriptor) = object::get_own_descriptor(self.global_this, &self.gc_heap, name)
            && !descriptor.flags.configurable()
        {
            return Err(
                self.err_syntax((format!("Identifier '{name}' has already been declared")).into())
            );
        }
        let cell = crate::alloc_upvalue(&mut self.gc_heap, Value::hole())?;
        self.global_lexicals.insert(name.into(), (cell, is_const));
        self.global_lexical_epoch = self.global_lexical_epoch.wrapping_add(1);
        self.global_object_load_ic.clear();
        Ok(())
    }

    /// `Op::ValidateGlobalDecl` — §16.1.7 steps 1–12 / §19.2.1.3
    /// steps 5–11: validate one declared name against the global
    /// environment before any binding is created.
    pub(crate) fn run_validate_global_decl_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        name_idx: u32,
        kind: i32,
    ) -> Result<(), VmError> {
        self.validate_global_decl_value(context, frame.function_id, name_idx, kind)?;
        frame.advance_pc()?;
        Ok(())
    }

    pub(crate) fn validate_global_decl_value(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        name_idx: u32,
        kind: i32,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)?;
        match kind {
            // Lexical: same checks as DeclareGlobalLex, minus the
            // cell creation.
            0 => {
                if self.global_lexicals.contains_key(name) {
                    return Err(self.err_syntax(
                        (format!("Identifier '{name}' has already been declared")).into(),
                    ));
                }
                if let Some(descriptor) =
                    object::get_own_descriptor(self.global_this, &self.gc_heap, name)
                    && !descriptor.flags.configurable()
                {
                    return Err(self.err_syntax(
                        (format!("Identifier '{name}' has already been declared")).into(),
                    ));
                }
            }
            // Var: §9.1.1.4.15 CanDeclareGlobalVar + the step-5
            // lexical-collision SyntaxError.
            1 => {
                if self.global_lexicals.contains_key(name) {
                    return Err(self.err_syntax(
                        (format!("Identifier '{name}' has already been declared")).into(),
                    ));
                }
            }
            // Function: §9.1.1.4.16 CanDeclareGlobalFunction + the
            // lexical-collision SyntaxError.
            _ => {
                if self.global_lexicals.contains_key(name) {
                    return Err(self.err_syntax(
                        (format!("Identifier '{name}' has already been declared")).into(),
                    ));
                }
                if let Some(descriptor) =
                    object::get_own_descriptor(self.global_this, &self.gc_heap, name)
                    && !descriptor.flags.configurable()
                {
                    let permitted = matches!(descriptor.kind, object::DescriptorKind::Data { .. })
                        && descriptor.flags.writable()
                        && descriptor.flags.enumerable();
                    if !permitted {
                        return Err(self.err_type(
                            (format!("Cannot declare global function '{name}'")).into(),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// `Op::InitGlobalLex` — §9.1.1.4 InitializeBinding on the
    /// global declarative record.
    pub(crate) fn run_init_global_lex_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        value_reg: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let value = *crate::read_register(frame, value_reg)?;
        self.init_global_lex_value(context, frame.function_id, name_idx, value)?;
        frame.advance_pc()?;
        Ok(())
    }

    pub(crate) fn init_global_lex_value(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        name_idx: u32,
        value: Value,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let cell = self
            .global_lexicals
            .get(name)
            .map(|(cell, _)| *cell)
            .ok_or(VmError::InvalidOperand)?;
        crate::store_upvalue(&mut self.gc_heap, cell, value);
        Ok(())
    }

    /// `Op::StoreGlobalBinding` — §9.1.1.4 global-environment
    /// SetMutableBinding: declarative record first, then the object
    /// record.
    /// `Op::GlobalBindingExists` — snapshot whether the global
    /// environment can currently resolve `name` (script lexicals, then
    /// the global object including its prototype chain).
    pub(crate) fn run_global_binding_exists_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let exists = self.global_binding_exists_value(context, frame.function_id, name_idx)?;
        crate::write_register(frame, dst, exists)?;
        frame.advance_pc()?;
        Ok(())
    }

    pub(crate) fn global_binding_exists_value(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        name_idx: u32,
    ) -> Result<Value, VmError> {
        let name = context
            .string_constant_str_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let exists = self.global_lexicals.contains_key(name)
            || object::get_own_descriptor(self.global_this, &self.gc_heap, name).is_some()
            || crate::object::get(self.global_this, &self.gc_heap, name).is_some();
        Ok(Value::boolean(exists))
    }

    /// `Op::StoreGlobalChecked` — §6.2.5.6 PutValue over a strict
    /// unresolvable reference: the ReferenceError keys off the
    /// existence snapshot taken BEFORE the RHS ran, so a RHS side
    /// effect creating the property does not legitimize the store.
    pub(crate) fn run_store_global_checked_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        value_reg: u16,
        name_idx: u32,
        exists_reg: u16,
    ) -> Result<(), VmError> {
        let frame = &stack[top_idx];
        let existed = crate::read_register(frame, exists_reg)?
            .as_boolean()
            .unwrap_or(false);
        if !existed {
            let name = context
                .string_constant_str(name_idx)
                .ok_or(VmError::InvalidOperand)?;
            return Err(self.err_undefined_ident((name.to_string()).into()));
        }
        self.run_store_global_binding_reg(context, stack, top_idx, value_reg, name_idx, true)
    }

    pub(crate) fn store_global_checked_value(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        function_id: u32,
        value: Value,
        name_idx: u32,
        existed: bool,
    ) -> Result<(), VmError> {
        if !existed {
            let name = context
                .string_constant_str_for_function(function_id, name_idx)
                .ok_or(VmError::InvalidOperand)?;
            return Err(self.err_undefined_ident((name.to_string()).into()));
        }
        self.store_global_binding_value(context, stack, function_id, value, name_idx, true)
    }

    /// Value flavour of [`Self::run_delete_dynamic_active_reg`]: report the
    /// §13.5.1 delete result over the frame-published eval chain and the
    /// global environment, without touching registers or the PC.
    pub(crate) fn delete_dynamic_value(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        eval_env: Option<EvalEnvHandle>,
        name_idx: u32,
    ) -> Result<Value, VmError> {
        let name = context
            .string_constant_str_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let removed_env = eval_env.is_some_and(|env| {
            crate::eval_env::eval_env_delete_chain(&mut self.gc_heap, env, name)
        });
        let removed = if removed_env {
            true
        } else if self.global_lexicals.contains_key(name) {
            false
        } else {
            crate::object::delete(self.global_this, &mut self.gc_heap, name)
        };
        Ok(Value::boolean(removed))
    }

    pub(crate) fn run_store_global_binding_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        value_reg: u16,
        name_idx: u32,
        strict: bool,
    ) -> Result<(), VmError> {
        let value = *crate::read_register(&stack[top_idx], value_reg)?;
        let function_id = stack[top_idx].function_id;
        self.store_global_binding_value(context, stack, function_id, value, name_idx, strict)?;
        stack[top_idx].advance_pc()?;
        Ok(())
    }

    pub(crate) fn store_global_binding_value(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        function_id: u32,
        value: Value,
        name_idx: u32,
        strict: bool,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)?;
        if let Some(&(cell, is_const)) = self.global_lexicals.get(name) {
            if is_const {
                return Err(
                    self.err_type((format!("Assignment to constant variable `{name}`")).into())
                );
            }
            if crate::read_upvalue(&self.gc_heap, cell).is_hole() {
                return Err(self.err_this_uninit(
                    (format!("Cannot access '{name}' before initialization")).into(),
                ));
            }
            crate::store_upvalue(&mut self.gc_heap, cell, value);
            return Ok(());
        }
        // §9.1.1.4.18 object-record SetMutableBinding — strict mode
        // rejects writes to a binding that does not exist.
        if strict
            && object::get_own_descriptor(self.global_this, &self.gc_heap, name).is_none()
            && crate::object::get(self.global_this, &self.gc_heap, name).is_none()
        {
            return Err(self.err_undefined_ident((name.to_string()).into()));
        }
        let receiver = Value::object(self.global_this);
        let key = VmPropertyKey::String(name);
        if !self.ordinary_set_data_value(stack, context, receiver, &key, value, receiver, 0)?
            && strict
        {
            return Err(self.err_type((format!("Cannot assign to property '{name}'")).into()));
        }
        Ok(())
    }
}
