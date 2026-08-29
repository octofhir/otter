//! Site-decoded committed binding and global-declaration calls.
//!
//! # Contents
//! - [`RuntimeCall::binding_values`] completes the schema-owned binding family.
//! - [`RuntimeCall::global_declaration_values`] completes the separate global
//!   declaration/initialization family.
//!
//! # Invariants
//! - Published function/PC and `OpcodeSchema` are the sole semantic authority;
//!   the native ABI carries only two boxed SSA values.
//! - Shadowed captures decode one schema-owned bounded eval prefix; a lookup or
//!   delete cannot cross the captured declaration owner.
//! - Structural site, constant, immediate, and upvalue failures are fatal.
//!   Entered JavaScript semantics return only success or a catchable error.
//! - Both boxed inputs and the result are rooted for the complete allocating or
//!   reentrant operation. Eval-environment ownership stays in the native frame.
//! - No operation advances the PC or writes a destination register.
//!
//! # See also
//! - `otter_bytecode::opcode_schema::BindingSemantics`
//! - [`super::CommittedValueError`]

use otter_bytecode::opcode_schema::{
    BindingDelete, BindingMissing, BindingRead, BindingSemantics, BindingWrite,
    GlobalDeclarationSemantics, ShadowedUpvalueStorePolicy, opcode_schema,
};

use crate::{Value, VmError, rooting::RootScopeExt};

use super::{CommittedValueError, RuntimeCall};

fn semantic_error(error: VmError) -> CommittedValueError {
    match error {
        VmError::MissingReturn
        | VmError::InvalidOperand
        | VmError::OutOfMemory { .. }
        | VmError::Interrupted
        | VmError::BudgetExceeded
        | VmError::Exit { .. } => CommittedValueError::Fatal(error),
        _ => CommittedValueError::JavaScript(error),
    }
}

fn flag(value: i32) -> Result<bool, CommittedValueError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(CommittedValueError::Fatal(VmError::InvalidOperand)),
    }
}

impl RuntimeCall<'_> {
    /// Complete the exact schema-typed binding site from boxed SSA inputs.
    pub fn binding_values(
        &mut self,
        mut value0: Value,
        mut value1: Value,
    ) -> Result<Value, CommittedValueError> {
        let operation = self
            .published_opcode()
            .map_err(CommittedValueError::Fatal)
            .and_then(|op| {
                opcode_schema(op)
                    .binding
                    .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))
            })?;
        let function_id = self.function_id();
        let vm = unsafe { &mut *self.vm.as_ptr() };
        vm.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        let context = unsafe { self.context.as_ref() };
        let stack = unsafe { &mut *self.stack.as_ptr() };

        let mut result = Value::undefined();
        let mut roots = otter_gc::RootScope::new(&mut vm.gc_heap);
        // SAFETY: all three locals precede `roots`, remain stationary, and do
        // not escape the complete committed call.
        unsafe {
            roots.add_value(&mut value0);
            roots.add_value(&mut value1);
            roots.add_value(&mut result);
        }

        result = match operation {
            BindingSemantics::Read(BindingRead::GlobalThis { .. }) => {
                Ok(Value::object(vm.global_this))
            }
            BindingSemantics::Read(BindingRead::Global { name, missing, .. }) => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                match missing {
                    BindingMissing::Throw => {
                        vm.load_global_or_throw_value(stack, context, function_id, name_idx)
                    }
                    BindingMissing::Undefined => {
                        vm.load_global_or_undefined_value(context, stack, function_id, name_idx)
                    }
                }
            }
            BindingSemantics::Read(BindingRead::Exists { name, .. }) => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                vm.global_binding_exists_value(stack, context, function_id, name_idx)
            }
            BindingSemantics::Read(BindingRead::Upvalue { index, .. }) => {
                let index = self
                    .published_imm32(index)
                    .ok()
                    .and_then(|index| u32::try_from(index).ok())
                    .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
                self.with_frame(|frame| vm.frame_load_upvalue_value(frame, index))
            }
            BindingSemantics::Read(BindingRead::Dynamic { name, missing, .. }) => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let eval_env = self
                    .with_frame(|frame| Ok(frame.eval_env()))
                    .map_err(CommittedValueError::Fatal)?;
                match missing {
                    BindingMissing::Throw => {
                        vm.load_dynamic_value(context, stack, function_id, eval_env, name_idx)
                    }
                    BindingMissing::Undefined => {
                        vm.typeof_dynamic_value(context, stack, function_id, eval_env, name_idx)
                    }
                }
            }
            BindingSemantics::Read(BindingRead::ShadowedUpvalue {
                name,
                index,
                eval_depth,
                ..
            }) => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let index = self
                    .published_imm32(index)
                    .ok()
                    .and_then(|index| u32::try_from(index).ok())
                    .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
                let eval_depth = self
                    .published_imm32(eval_depth)
                    .ok()
                    .and_then(|depth| u32::try_from(depth).ok())
                    .filter(|depth| *depth != 0)
                    .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
                self.with_frame(|frame| {
                    vm.load_shadowed_upvalue_value(
                        context, frame, name_idx, index, eval_depth, None,
                    )
                })
            }
            BindingSemantics::Write(BindingWrite::Global { name, strict, .. }) => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let strict = flag(
                    self.published_imm32(strict)
                        .map_err(CommittedValueError::Fatal)?,
                )?;
                vm.store_global_binding_value(context, stack, function_id, value0, name_idx, strict)
                    .map(|()| Value::undefined())
            }
            BindingSemantics::Write(BindingWrite::GlobalChecked { name, .. }) => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let existed = value1
                    .as_boolean()
                    .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
                vm.store_global_checked_value(
                    context,
                    stack,
                    function_id,
                    value0,
                    name_idx,
                    existed,
                )
                .map(|()| Value::undefined())
            }
            BindingSemantics::Write(BindingWrite::Upvalue { index, check, .. }) => {
                let index = self
                    .published_imm32(index)
                    .ok()
                    .and_then(|index| u32::try_from(index).ok())
                    .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
                self.with_frame(|frame| vm.frame_store_upvalue_value(frame, index, value0, check))
                    .map(|()| Value::undefined())
            }
            BindingSemantics::Write(BindingWrite::Dynamic { name, strict, .. }) => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let strict = flag(
                    self.published_imm32(strict)
                        .map_err(CommittedValueError::Fatal)?,
                )?;
                let eval_env = self
                    .with_frame(|frame| Ok(frame.eval_env()))
                    .map_err(CommittedValueError::Fatal)?;
                vm.store_dynamic_value(
                    context,
                    stack,
                    function_id,
                    eval_env,
                    value0,
                    name_idx,
                    strict,
                )
                .map(|()| Value::undefined())
            }
            BindingSemantics::Write(BindingWrite::ShadowedUpvalue {
                name,
                index,
                policy,
                ..
            }) => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let index = self
                    .published_imm32(index)
                    .ok()
                    .and_then(|index| u32::try_from(index).ok())
                    .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
                let policy = self
                    .published_imm32(policy)
                    .map_err(CommittedValueError::Fatal)
                    .and_then(|encoded| {
                        ShadowedUpvalueStorePolicy::from_imm32(encoded)
                            .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))
                    })?;
                self.with_frame(|frame| {
                    vm.store_shadowed_upvalue_value(
                        context,
                        frame,
                        name_idx,
                        index,
                        policy.eval_depth,
                        policy.fallback,
                        value0,
                        None,
                    )
                })
                .map(|()| Value::undefined())
            }
            BindingSemantics::Delete(BindingDelete::Dynamic { name, .. }) => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let eval_env = self
                    .with_frame(|frame| Ok(frame.eval_env()))
                    .map_err(CommittedValueError::Fatal)?;
                vm.delete_dynamic_value(context, function_id, eval_env, name_idx)
            }
            BindingSemantics::Delete(BindingDelete::ShadowedUpvalue {
                name,
                index,
                eval_depth,
                ..
            }) => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let index = self
                    .published_imm32(index)
                    .ok()
                    .and_then(|index| u32::try_from(index).ok())
                    .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
                let eval_depth = self
                    .published_imm32(eval_depth)
                    .ok()
                    .and_then(|depth| u32::try_from(depth).ok())
                    .filter(|depth| *depth != 0)
                    .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
                self.with_frame(|frame| {
                    vm.delete_shadowed_upvalue_value(context, frame, name_idx, index, eval_depth)
                })
            }
        }
        .map_err(semantic_error)?;
        Ok(result)
    }

    /// Complete the exact schema-typed global declaration site.
    pub fn global_declaration_values(
        &mut self,
        mut value0: Value,
        mut value1: Value,
    ) -> Result<Value, CommittedValueError> {
        let operation = self
            .published_opcode()
            .map_err(CommittedValueError::Fatal)
            .and_then(|op| {
                opcode_schema(op)
                    .global_declaration
                    .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))
            })?;
        let function_id = self.function_id();
        let vm = unsafe { &mut *self.vm.as_ptr() };
        vm.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        let context = unsafe { self.context.as_ref() };

        let mut result = Value::undefined();
        let mut roots = otter_gc::RootScope::new(&mut vm.gc_heap);
        // SAFETY: the stationary boxed locals remain within this call.
        unsafe {
            roots.add_value(&mut value0);
            roots.add_value(&mut value1);
            roots.add_value(&mut result);
        }

        let semantic = match operation {
            GlobalDeclarationSemantics::DeclareVar { name, configurable } => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let configurable = flag(
                    self.published_imm32(configurable)
                        .map_err(CommittedValueError::Fatal)?,
                )?;
                vm.declare_global_var_value(context, function_id, name_idx, configurable)
            }
            GlobalDeclarationSemantics::DeclareLexical { name, is_const } => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let is_const = flag(
                    self.published_imm32(is_const)
                        .map_err(CommittedValueError::Fatal)?,
                )?;
                vm.declare_global_lex_value(context, function_id, name_idx, is_const)
            }
            GlobalDeclarationSemantics::Validate {
                name,
                declaration_kind,
            } => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let kind = self
                    .published_imm32(declaration_kind)
                    .map_err(CommittedValueError::Fatal)?;
                if !(0..=2).contains(&kind) {
                    return Err(CommittedValueError::Fatal(VmError::InvalidOperand));
                }
                vm.validate_global_decl_value(context, function_id, name_idx, kind)
            }
            GlobalDeclarationSemantics::DefineVar { name, .. } => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                vm.define_global_var_value(context, function_id, name_idx, value0)
            }
            GlobalDeclarationSemantics::DefineFunction {
                name, deletable, ..
            } => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let deletable = flag(
                    self.published_imm32(deletable)
                        .map_err(CommittedValueError::Fatal)?,
                )?;
                vm.define_global_function_value(context, function_id, name_idx, value0, deletable)
            }
            GlobalDeclarationSemantics::InitializeLexical { name, .. } => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                vm.init_global_lex_value(context, function_id, name_idx, value0)
            }
        };
        semantic.map_err(semantic_error)?;
        Ok(result)
    }
}
