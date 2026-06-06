//! Global binding load opcode helpers.
//!
//! These are fixed-width global environment reads that can dispatch directly
//! from executable operands.
//!
//! # Contents
//! - `globalThis` load.
//! - Throwing global binding lookup for ordinary identifier reads.
//! - Undefined-returning global lookup for `typeof`.
//!
//! # Invariants
//! - Global properties live on the interpreter's `global_this` object.
//! - Missing throwing lookups surface as `UndefinedIdentifier` so the normal
//!   error path can synthesize a `ReferenceError`.
//!
//! # See also
//! - [`crate::executable`]
//! - [`crate::object`]

use smallvec::SmallVec;

use crate::{
    ExecutionContext, Frame, Interpreter, Value, VmError, VmGetOutcome, VmPropertyKey, object,
    write_register,
};

impl Interpreter {
    pub(crate) fn run_load_global_this_reg(
        &self,
        frame: &mut Frame,
        dst: u16,
    ) -> Result<(), VmError> {
        write_register(frame, dst, Value::object(self.global_this))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_load_global_or_throw_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        // §9.1.1.4 — the global declarative record (script lexicals)
        // shadows the object record.
        if let Some(value) = self.read_global_lexical(name)? {
            write_register(frame, dst, value)?;
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }
        let receiver = Value::object(self.global_this);
        let key = VmPropertyKey::String(name);
        if !self.ordinary_has_property_value(context, receiver, &key, 0)? {
            return Err(VmError::UndefinedIdentifier {
                name: name.to_string(),
            });
        }
        let value = match self.ordinary_get_value(context, receiver, receiver, &key, 0)? {
            VmGetOutcome::Value(value) => value,
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync(context, &getter, receiver, SmallVec::new())?
            }
        };
        write_register(frame, dst, value)?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_load_global_or_undefined_reg(
        &self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        // §13.5.3 — `typeof` still raises ReferenceError for a
        // lexical binding read inside its TDZ; only *unresolvable*
        // names yield `undefined`.
        let value = if let Some(value) = self.read_global_lexical(name)? {
            value
        } else {
            crate::object::get(self.global_this, &self.gc_heap, name).unwrap_or(Value::undefined())
        };
        write_register(frame, dst, value)?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    /// Read a binding from the global declarative record. `Ok(None)`
    /// when the name has no global lexical binding;
    /// `Err(ReferenceError)` when the binding is still in its TDZ.
    fn read_global_lexical(&self, name: &str) -> Result<Option<Value>, VmError> {
        let Some((cell, _)) = self.global_lexicals.get(name) else {
            return Ok(None);
        };
        let value = crate::read_upvalue(&self.gc_heap, *cell);
        if value.is_hole() {
            // `ThisUninitialized` is the engine's named-TDZ
            // `ReferenceError` vehicle (same as module bindings).
            return Err(VmError::ThisUninitialized {
                message: format!("Cannot access '{name}' before initialization"),
            });
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
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let value = *crate::read_register(frame, value_reg)?;
        // §9.1.1.4.18 SetMutableBinding shape — an existing own
        // property keeps its attributes (enumerability,
        // configurability) and only receives the new value; a
        // non-writable existing property silently absorbs the write
        // in sloppy mode. Only an absent property is defined fresh.
        if object::get_own_descriptor(self.global_this, &self.gc_heap, name).is_some() {
            object::set(self.global_this, &mut self.gc_heap, name, value);
            frame.advance_pc(self.current_byte_len)?;
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
            self.global_this,
            &mut self.gc_heap,
            name,
            descriptor,
        ) {
            return Err(VmError::TypeError {
                message: format!("Cannot declare global var '{name}'"),
            });
        }
        frame.advance_pc(self.current_byte_len)?;
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
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        // §19.2.1.3 step 5 — a var-scoped name colliding with a
        // global *lexical* binding is a SyntaxError at declaration
        // time (script collisions are early errors; eval collisions
        // surface here).
        if self.global_lexicals.contains_key(name) {
            return Err(VmError::SyntaxError {
                message: format!("Identifier '{name}' has already been declared"),
            });
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
                self.global_this,
                &mut self.gc_heap,
                name,
                descriptor,
            ) {
                return Err(VmError::TypeError {
                    message: format!("Cannot declare global variable '{name}'"),
                });
            }
        }
        frame.advance_pc(self.current_byte_len)?;
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
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let value = *crate::read_register(frame, value_reg)?;
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
                self.global_this,
                &mut self.gc_heap,
                name,
                descriptor,
            ) {
                return Err(VmError::TypeError {
                    message: format!("Cannot declare global function '{name}'"),
                });
            }
        } else {
            let permitted = existing.as_ref().is_some_and(|descriptor| {
                matches!(descriptor.kind, object::DescriptorKind::Data { .. })
                    && descriptor.flags.writable()
                    && descriptor.flags.enumerable()
            });
            if !permitted {
                return Err(VmError::TypeError {
                    message: format!("Cannot declare global function '{name}'"),
                });
            }
            object::set(self.global_this, &mut self.gc_heap, name, value);
        }
        frame.advance_pc(self.current_byte_len)?;
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
        frame: &mut Frame,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        if let Some(cell) = self.frame_eval_var(frame, name) {
            let value = crate::read_upvalue(&self.gc_heap, cell);
            write_register(frame, dst, value)?;
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }
        self.run_load_global_or_throw_reg(context, frame, dst, name_idx)
    }

    /// `Op::StoreDynamic` — §10.2.4.2 PutValue counterpart of
    /// [`Self::run_load_dynamic_reg`]: store through the
    /// eval-introduced binding when present, else the sloppy-mode
    /// `globalThis` property write.
    pub(crate) fn run_store_dynamic_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        value_reg: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let value = *crate::read_register(frame, value_reg)?;
        if let Some(cell) = self.frame_eval_var(frame, name) {
            crate::store_upvalue(&mut self.gc_heap, cell, value);
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }
        // Fall through to the full global SetMutableBinding so
        // realm-wide lexical bindings stay visible (sloppy mode).
        self.run_store_global_binding_reg(context, frame, value_reg, name_idx, false)
    }

    /// `Op::TypeofDynamic` — `typeof` flavour of
    /// [`Self::run_load_dynamic_reg`]; an unresolvable name yields
    /// `undefined` instead of throwing (§13.5.3).
    pub(crate) fn run_typeof_dynamic_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        if let Some(cell) = self.frame_eval_var(frame, name) {
            let value = crate::read_upvalue(&self.gc_heap, cell);
            write_register(frame, dst, value)?;
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }
        self.run_load_global_or_undefined_reg(context, frame, dst, name_idx)
    }

    /// Look up an eval-introduced var binding on `frame`'s cold
    /// record.
    fn frame_eval_var(&self, frame: &Frame, name: &str) -> Option<crate::UpvalueCell> {
        self.frame_cold(frame)
            .and_then(|cold| cold.eval_vars.as_ref())
            .and_then(|map| map.get(name))
            .copied()
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
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        if self.global_lexicals.contains_key(name) {
            return Err(VmError::SyntaxError {
                message: format!("Identifier '{name}' has already been declared"),
            });
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
            return Err(VmError::SyntaxError {
                message: format!("Identifier '{name}' has already been declared"),
            });
        }
        let cell = crate::alloc_upvalue(&mut self.gc_heap, Value::hole())?;
        self.global_lexicals.insert(name.into(), (cell, is_const));
        frame.advance_pc(self.current_byte_len)?;
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
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        match kind {
            // Lexical: same checks as DeclareGlobalLex, minus the
            // cell creation.
            0 => {
                if self.global_lexicals.contains_key(name) {
                    return Err(VmError::SyntaxError {
                        message: format!("Identifier '{name}' has already been declared"),
                    });
                }
                if let Some(descriptor) =
                    object::get_own_descriptor(self.global_this, &self.gc_heap, name)
                    && !descriptor.flags.configurable()
                {
                    return Err(VmError::SyntaxError {
                        message: format!("Identifier '{name}' has already been declared"),
                    });
                }
            }
            // Var: §9.1.1.4.15 CanDeclareGlobalVar + the step-5
            // lexical-collision SyntaxError.
            1 => {
                if self.global_lexicals.contains_key(name) {
                    return Err(VmError::SyntaxError {
                        message: format!("Identifier '{name}' has already been declared"),
                    });
                }
            }
            // Function: §9.1.1.4.16 CanDeclareGlobalFunction + the
            // lexical-collision SyntaxError.
            _ => {
                if self.global_lexicals.contains_key(name) {
                    return Err(VmError::SyntaxError {
                        message: format!("Identifier '{name}' has already been declared"),
                    });
                }
                if let Some(descriptor) =
                    object::get_own_descriptor(self.global_this, &self.gc_heap, name)
                    && !descriptor.flags.configurable()
                {
                    let permitted = matches!(descriptor.kind, object::DescriptorKind::Data { .. })
                        && descriptor.flags.writable()
                        && descriptor.flags.enumerable();
                    if !permitted {
                        return Err(VmError::TypeError {
                            message: format!("Cannot declare global function '{name}'"),
                        });
                    }
                }
            }
        }
        frame.advance_pc(self.current_byte_len)?;
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
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let value = *crate::read_register(frame, value_reg)?;
        let cell = self
            .global_lexicals
            .get(name)
            .map(|(cell, _)| *cell)
            .ok_or(VmError::InvalidOperand)?;
        crate::store_upvalue(&mut self.gc_heap, cell, value);
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    /// `Op::StoreGlobalBinding` — §9.1.1.4 global-environment
    /// SetMutableBinding: declarative record first, then the object
    /// record.
    pub(crate) fn run_store_global_binding_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        value_reg: u16,
        name_idx: u32,
        strict: bool,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let value = *crate::read_register(frame, value_reg)?;
        if let Some(&(cell, is_const)) = self.global_lexicals.get(name) {
            if is_const {
                return Err(VmError::TypeError {
                    message: format!("Assignment to constant variable `{name}`"),
                });
            }
            if crate::read_upvalue(&self.gc_heap, cell).is_hole() {
                return Err(VmError::ThisUninitialized {
                    message: format!("Cannot access '{name}' before initialization"),
                });
            }
            crate::store_upvalue(&mut self.gc_heap, cell, value);
            frame.advance_pc(self.current_byte_len)?;
            return Ok(());
        }
        // §9.1.1.4.18 object-record SetMutableBinding — strict mode
        // rejects writes to a binding that does not exist.
        if strict
            && object::get_own_descriptor(self.global_this, &self.gc_heap, name).is_none()
            && crate::object::get(self.global_this, &self.gc_heap, name).is_none()
        {
            return Err(VmError::UndefinedIdentifier {
                name: name.to_string(),
            });
        }
        self.run_define_global_var_reg(context, frame, name_idx, value_reg)
    }
}
