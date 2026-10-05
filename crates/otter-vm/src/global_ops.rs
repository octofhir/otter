//! Global binding opcode helpers.
//!
//! These are fixed-width global environment reads and writes that dispatch
//! directly from executable operands.
//!
//! # Contents
//! - `globalThis` load.
//! - Throwing global binding lookup with lexical-cell and guarded global-object
//!   slot caches for ordinary identifier reads.
//! - Named global-environment lookup shared by `typeof` and scoped host reads.
//! - Global declaration, initialization, assignment, and deletion helpers.
//!
//! # Invariants
//! - Each global operation resolves the linked source function's realm.
//!   The active or parked RealmState retains that realm's global and lexicals;
//!   disposed sources never substitute another realm's ambient global.
//! - A global-object slot cache is valid only while the object's hidden-class
//!   identity matches and no later global lexical declaration shadows it.
//! - Missing throwing lookups surface as `UndefinedIdentifier` so the normal
//!   error path can synthesize a `ReferenceError`.
//! - Identifier assignment and existing var bindings use the actual `[[Set]]`
//!   ladder with the current activation stack; declaration defines use the sole
//!   descriptor owner and retain their real allocation failure.
//! - Bindings a sloppy direct eval creates live in context eval extensions and
//!   resolve through the `Lookup*` family in [`crate::context_ops`], which
//!   falls back to these global-record helpers.
//!
//! # See also
//! - [`crate::executable`]
//! - [`crate::object`]

use crate::native_abi::CommittedValueError;
use smallvec::SmallVec;

use crate::{
    ExecutionContext, Frame, Interpreter, Value, VmError, VmGetOutcome, VmPropertyKey,
    activation_stack::ActivationStack, object, write_register,
};

/// Guarded own-data slot for one global object-record load site.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GlobalObjectLoadCache {
    shape_id: object::ShapeId,
    slot: u16,
}

/// Missing-name behavior of the two ECMAScript global identifier reads.
#[derive(Debug, Clone, Copy)]
pub(crate) enum GlobalBindingRead {
    /// An unresolvable ordinary identifier raises ReferenceError.
    Identifier,
    /// An unresolvable typeof operand yields undefined; TDZ still throws.
    Typeof,
}

impl Interpreter {
    pub(crate) fn run_load_global_this_reg(
        &self,
        frame: &mut Frame,
        dst: u16,
    ) -> Result<(), VmError> {
        // A frame compiled in another realm sees that realm's global
        // (§9.1.1.4.11 GetThisBinding); the parked realm keeps it in
        // `extra_realms` while inactive.
        let global = self.global_this_for_function(frame.function_id)?;
        write_register(frame, dst, Value::object(global))?;
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
    ) -> Result<(), CommittedValueError> {
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
    ) -> Result<(), CommittedValueError> {
        let value = self.load_global_or_throw_value(stack, context, function_id, name_idx)?;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, value)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        frame
            .advance_pc()
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
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
    ) -> Result<Value, CommittedValueError> {
        // A frame compiled in another realm resolves its globals there —
        // §9.1.1.4 global environment records are per-realm.
        if let Some(realm_id) = self.foreign_function_realm(function_id) {
            return self
                .with_host_realm_id(realm_id, |interp| {
                    Ok(interp.load_global_or_throw_value(stack, context, function_id, name_idx))
                })
                .map_err(CommittedValueError::Fatal)?;
        }
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
                .ok_or(VmError::InvalidOperand)
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            return Err(CommittedValueError::JavaScript(self.err_this_uninit(
                (format!("Cannot access '{name}' before initialization")).into(),
            )));
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
            .ok_or(VmError::InvalidOperand)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        self.load_global_binding_name(
            Some(context),
            stack,
            name,
            GlobalBindingRead::Identifier,
            Some(site),
        )
    }

    pub(crate) fn run_load_global_or_undefined_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), CommittedValueError> {
        let function_id = stack[top_idx].function_id;
        let value = self.load_global_or_undefined_value(context, stack, function_id, name_idx)?;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, value)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        frame
            .advance_pc()
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        Ok(())
    }

    pub(crate) fn load_global_or_undefined_value(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        function_id: u32,
        name_idx: u32,
    ) -> Result<Value, CommittedValueError> {
        if let Some(realm_id) = self.foreign_function_realm(function_id) {
            return self
                .with_host_realm_id(realm_id, |interp| {
                    Ok(
                        interp.load_global_or_undefined_value(
                            context,
                            stack,
                            function_id,
                            name_idx,
                        ),
                    )
                })
                .map_err(CommittedValueError::Fatal)?;
        }
        let name = context
            .string_constant_str_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        self.load_global_binding_name(Some(context), stack, name, GlobalBindingRead::Typeof, None)
    }

    /// Read one named global binding through the active realm's environment.
    /// Only `typeof` suppresses an unresolvable name; every resolved lexical
    /// TDZ or collecting getter keeps its ordinary abrupt completion. A cached
    /// bytecode identifier site retains the existing stable-cell/own-slot proof.
    pub(crate) fn load_global_binding_name(
        &mut self,
        context: Option<&ExecutionContext>,
        stack: &mut ActivationStack,
        name: &str,
        read: GlobalBindingRead,
        cache_site: Option<(u32, u32)>,
    ) -> Result<Value, CommittedValueError> {
        if let Some(value) = self
            .read_global_lexical(name)
            .map_err(|error| CommittedValueError::JavaScript(error.into()))?
        {
            if let Some(site) = cache_site
                && let Some((cell, _)) = self.global_lexicals.get(name).copied()
            {
                self.global_lexical_load_ic.insert(site, cell);
            }
            return Ok(value);
        }
        // Ordinary identifier reads retain the same own-slot fast lookup and
        // [[HasProperty]] check as LoadGlobal. A getter may delete the binding
        // after an earlier typeof read, so an absent identifier must still throw.
        if matches!(read, GlobalBindingRead::Identifier) {
            let (own_hit, own_lookup) =
                object::lookup_own_slot(self.global_this, &self.gc_heap, name);
            match own_lookup {
                object::PropertyLookup::Data { value, .. } => {
                    if let (Some(site), Some(hit)) = (cache_site, own_hit) {
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
                            .map_err(CommittedValueError::completed_call)
                        }
                        _ => Ok(Value::undefined()),
                    };
                }
                object::PropertyLookup::Absent => {}
            }
            let receiver = Value::object(self.global_this);
            let key = VmPropertyKey::String(name);
            if !self.ordinary_has_property_value(stack, context, receiver, &key, 0)? {
                return Err(CommittedValueError::JavaScript(
                    self.err_undefined_ident(name.to_owned().into()),
                ));
            }
        }
        let receiver = Value::object(self.global_this);
        let key = VmPropertyKey::String(name);
        match self.ordinary_get_value(stack, context, receiver, receiver, &key, 0)? {
            VmGetOutcome::Value(value) => Ok(value),
            VmGetOutcome::InvokeGetter { getter } => {
                let receiver = Value::object(self.global_this);
                self.run_callable_sync_rooted(stack, context, &getter, receiver, SmallVec::new())
                    .map_err(CommittedValueError::completed_call)
            }
        }
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
        stack: &mut ActivationStack,
        frame_index: usize,
        name_idx: u32,
        value_reg: u16,
    ) -> Result<(), CommittedValueError> {
        let frame = &stack[frame_index];
        let function_id = frame.function_id;
        let value = *crate::read_register(frame, value_reg)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        self.define_global_var_value(context, stack, function_id, name_idx, value)?;
        stack[frame_index]
            .advance_pc()
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        Ok(())
    }

    pub(crate) fn define_global_var_value(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        function_id: u32,
        name_idx: u32,
        value: Value,
    ) -> Result<(), CommittedValueError> {
        let name = context
            .string_constant_str_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        // §9.1.1.4.18 SetMutableBinding shape — an existing own
        // property keeps its attributes (enumerability,
        // configurability) and only receives the new value; a
        // non-writable existing property silently absorbs the write
        // in sloppy mode. Only an absent property is defined fresh.
        if object::get_own_descriptor(self.global_this, &self.gc_heap, name).is_some() {
            // Existing object-record bindings use the actual [[Set]] ladder:
            // setters re-enter through this same live activation owner and
            // sloppy read-only failures preserve the descriptor unchanged.
            return self.ordinary_set_with_callable_setter(
                stack,
                context,
                self.global_this,
                name,
                value,
                false,
            );
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
        )
        .map_err(|error| CommittedValueError::JavaScript(error.into()))?
        {
            return Err(CommittedValueError::JavaScript(
                self.err_type((format!("Cannot declare global var '{name}'")).into()),
            ));
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
            )? {
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
            )? {
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
            if !object::ordinary_set_data_property(
                &mut self.global_this,
                &mut self.gc_heap,
                name,
                value,
            )? {
                return Err(
                    self.err_type((format!("Cannot declare global function '{name}'")).into())
                );
            }
        }
        Ok(())
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
        stack: &mut ActivationStack,
        top_idx: usize,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), CommittedValueError> {
        let function_id = stack[top_idx].function_id;
        let exists = self.global_binding_exists_value(stack, context, function_id, name_idx)?;
        let frame = &mut stack[top_idx];
        crate::write_register(frame, dst, exists)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        frame
            .advance_pc()
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        Ok(())
    }

    pub(crate) fn global_binding_exists_value(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        function_id: u32,
        name_idx: u32,
    ) -> Result<Value, CommittedValueError> {
        if let Some(realm_id) = self.foreign_function_realm(function_id) {
            return self
                .with_host_realm_id(realm_id, |interp| {
                    Ok(interp.global_binding_exists_value(stack, context, function_id, name_idx))
                })
                .map_err(CommittedValueError::Fatal)?;
        }
        let name = context
            .string_constant_str_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        if self.global_lexicals.contains_key(name)
            || object::get_own_descriptor(self.global_this, &self.gc_heap, name).is_some()
        {
            return Ok(Value::boolean(true));
        }
        // §9.1.1.4.1 HasBinding is HasProperty(globalObject, N), so a
        // proxy standing in as the global's prototype answers through
        // its `has` trap — the non-reentrant chain walk cannot.
        let receiver = Value::object(self.global_this);
        let key = VmPropertyKey::String(name);
        let exists = self.ordinary_has_property_value(stack, Some(context), receiver, &key, 0)?;
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
    ) -> Result<(), CommittedValueError> {
        let frame = &stack[top_idx];
        let existed = crate::read_register(frame, exists_reg)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?
            .as_boolean()
            .unwrap_or(false);
        if !existed {
            let name = context
                .string_constant_str(name_idx)
                .ok_or(VmError::InvalidOperand)
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            return Err(CommittedValueError::JavaScript(
                self.err_undefined_ident((name.to_string()).into()),
            ));
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
    ) -> Result<(), CommittedValueError> {
        if let Some(realm_id) = self.foreign_function_realm(function_id) {
            return self
                .with_host_realm_id(realm_id, |interp| {
                    Ok(interp.store_global_checked_value(
                        context,
                        stack,
                        function_id,
                        value,
                        name_idx,
                        existed,
                    ))
                })
                .map_err(CommittedValueError::Fatal)?;
        }
        if !existed {
            let name = context
                .string_constant_str_for_function(function_id, name_idx)
                .ok_or(VmError::InvalidOperand)
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            return Err(CommittedValueError::JavaScript(
                self.err_undefined_ident((name.to_string()).into()),
            ));
        }
        self.store_global_binding_value(context, stack, function_id, value, name_idx, true)
    }

    pub(crate) fn run_store_global_binding_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        value_reg: u16,
        name_idx: u32,
        strict: bool,
    ) -> Result<(), CommittedValueError> {
        let value = *crate::read_register(&stack[top_idx], value_reg)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let function_id = stack[top_idx].function_id;
        self.store_global_binding_value(context, stack, function_id, value, name_idx, strict)?;
        stack[top_idx]
            .advance_pc()
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
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
    ) -> Result<(), CommittedValueError> {
        if let Some(realm_id) = self.foreign_function_realm(function_id) {
            return self
                .with_host_realm_id(realm_id, |interp| {
                    Ok(interp.store_global_binding_value(
                        context,
                        stack,
                        function_id,
                        value,
                        name_idx,
                        strict,
                    ))
                })
                .map_err(CommittedValueError::Fatal)?;
        }
        let name = context
            .string_constant_str_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        if let Some(&(cell, is_const)) = self.global_lexicals.get(name) {
            if is_const {
                return Err(CommittedValueError::JavaScript(self.err_type(
                    (format!("Assignment to constant variable `{name}`")).into(),
                )));
            }
            if crate::read_upvalue(&self.gc_heap, cell).is_hole() {
                return Err(CommittedValueError::JavaScript(self.err_this_uninit(
                    (format!("Cannot access '{name}' before initialization")).into(),
                )));
            }
            crate::store_upvalue(&mut self.gc_heap, cell, value);
            return Ok(());
        }
        let receiver = Value::object(self.global_this);
        let key = VmPropertyKey::String(name);
        // §9.1.1.4.18 object-record SetMutableBinding — strict mode
        // rejects writes to a binding that does not exist. Existence is
        // §9.1.1.4.1 HasBinding, i.e. HasProperty on the global object,
        // so a proxy standing in as its prototype answers through the
        // `has` trap.
        if strict
            && object::get_own_descriptor(self.global_this, &self.gc_heap, name).is_none()
            && !self.ordinary_has_property_value(stack, Some(context), receiver, &key, 0)?
        {
            return Err(CommittedValueError::JavaScript(
                self.err_undefined_ident((name.to_string()).into()),
            ));
        }
        if !self.ordinary_set_data_value(stack, context, receiver, &key, value, receiver, 0)?
            && strict
        {
            return Err(CommittedValueError::JavaScript(
                self.err_type((format!("Cannot assign to property '{name}'")).into()),
            ));
        }
        Ok(())
    }
}
