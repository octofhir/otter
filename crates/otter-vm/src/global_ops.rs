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
        let value =
            crate::object::get(self.global_this, &self.gc_heap, name).unwrap_or(Value::undefined());
        write_register(frame, dst, value)?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
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
        if object::get_own_descriptor(self.global_this, &self.gc_heap, name).is_none() {
            let descriptor = object::PartialPropertyDescriptor {
                value: Some(Value::undefined()),
                writable: Some(true),
                enumerable: Some(true),
                configurable: Some(configurable),
                ..Default::default()
            };
            let _ = object::define_own_property_partial(
                self.global_this,
                &mut self.gc_heap,
                name,
                descriptor,
            );
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
        } else {
            object::set(self.global_this, &mut self.gc_heap, name, value);
        }
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
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
}
