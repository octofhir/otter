//! Site-decoded committed binding and global-declaration calls.
//!
//! # Contents
//! - [`RuntimeCall::binding_values`] completes the schema-owned binding family.
//! - [`RuntimeCall::global_declaration_values`] completes the separate global
//!   declaration/initialization family.
//!
//! # Invariants
//! - Published function/PC and `OpcodeSchema` are the sole semantic authority;
//!   the native ABI carries only two boxed SSA values, in
//!   `BindingSemantics::value_operands` order: the stored value first, then
//!   the context or reference register (or the pre-RHS existence Boolean).
//! - `Lookup*` operations decode one schema-owned static hop bound; an
//!   extension probe cannot cross the declaring context.
//! - Structural site, constant, immediate, and context failures are fatal.
//!   Entered JavaScript semantics return only success or a catchable error.
//! - Both boxed inputs and the result are rooted for the complete allocating or
//!   reentrant operation; allocating kernels re-read contexts from those roots.
//! - No operation advances the PC or writes a destination register.
//!
//! # See also
//! - `otter_bytecode::opcode_schema::BindingSemantics`
//! - [`super::CommittedValueError`]

use otter_bytecode::opcode_schema::{
    BindingDelete, BindingMissing, BindingRead, BindingSemantics, BindingWrite,
    GlobalDeclarationSemantics, opcode_schema,
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
        let context = self.context.clone();
        let context = &context;
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
            BindingSemantics::Read(BindingRead::ContextSlot { coord, .. }) => {
                let coord = self
                    .published_imm32(coord)
                    .map_err(CommittedValueError::Fatal)?;
                vm.load_context_slot_value(context, value0, coord, true)
            }
            BindingSemantics::Read(BindingRead::LookupSlot { name, coord, .. }) => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let coord = self
                    .published_imm32(coord)
                    .map_err(CommittedValueError::Fatal)?;
                vm.load_lookup_slot_value(context, function_id, value0, name_idx, coord)
            }
            BindingSemantics::Read(BindingRead::LookupGlobal {
                name,
                depth,
                missing,
                ..
            }) => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let depth = self
                    .published_imm32(depth)
                    .map_err(CommittedValueError::Fatal)?;
                vm.load_lookup_global_value(
                    context,
                    stack,
                    function_id,
                    value0,
                    name_idx,
                    depth,
                    missing,
                )
            }
            BindingSemantics::Read(BindingRead::ResolveRef { name, target, .. }) => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let target = self
                    .published_imm32(target)
                    .map_err(CommittedValueError::Fatal)?;
                vm.resolve_lookup_ref_value(context, function_id, value0, name_idx, target)
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
            BindingSemantics::Write(BindingWrite::ContextSlot { coord, .. }) => {
                let coord = self
                    .published_imm32(coord)
                    .map_err(CommittedValueError::Fatal)?;
                vm.store_context_slot_value(context, value1, coord, value0, true)
                    .map(|()| Value::undefined())
            }
            BindingSemantics::Write(BindingWrite::BindThis { coord, .. }) => {
                let coord = self
                    .published_imm32(coord)
                    .map_err(CommittedValueError::Fatal)?;
                vm.bind_this_context_slot_value(value1, coord, value0)
                    .map(|()| Value::undefined())
            }
            BindingSemantics::Write(BindingWrite::LookupSlot {
                name,
                coord,
                fallback,
                ..
            }) => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let coord = self
                    .published_imm32(coord)
                    .map_err(CommittedValueError::Fatal)?;
                let fallback = self
                    .published_imm32(fallback)
                    .map_err(CommittedValueError::Fatal)?;
                vm.store_lookup_slot_value(
                    context,
                    function_id,
                    value1,
                    name_idx,
                    coord,
                    fallback,
                    value0,
                )
                .map(|()| Value::undefined())
            }
            BindingSemantics::Write(BindingWrite::LookupGlobal { name, mode, .. }) => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let mode = self
                    .published_imm32(mode)
                    .map_err(CommittedValueError::Fatal)?;
                vm.store_lookup_global_value(
                    context,
                    stack,
                    function_id,
                    value1,
                    name_idx,
                    mode,
                    value0,
                )
                .map(|()| Value::undefined())
            }
            BindingSemantics::Write(BindingWrite::StoreRef { name, mode, .. }) => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let mode = self
                    .published_imm32(mode)
                    .map_err(CommittedValueError::Fatal)?;
                vm.store_ref_value(context, stack, function_id, value1, name_idx, mode, value0)
                    .map(|()| Value::undefined())
            }
            BindingSemantics::Write(BindingWrite::DeclareEvalVar {
                name, var_depth, ..
            }) => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let var_depth = self
                    .published_imm32(var_depth)
                    .map_err(CommittedValueError::Fatal)?;
                vm.declare_eval_var_value(context, function_id, value0, name_idx, var_depth)
                    .map(|()| Value::undefined())
            }
            BindingSemantics::Write(BindingWrite::VarScope {
                name, var_depth, ..
            }) => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let var_depth = self
                    .published_imm32(var_depth)
                    .map_err(CommittedValueError::Fatal)?;
                vm.store_var_scope_value(context, function_id, value1, name_idx, var_depth, value0)
                    .map(|()| Value::undefined())
            }
            BindingSemantics::Delete(BindingDelete::LookupSlot { name, depth, .. }) => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let depth = self
                    .published_imm32(depth)
                    .map_err(CommittedValueError::Fatal)?;
                vm.delete_lookup_slot_value(context, function_id, value0, name_idx, depth)
            }
            BindingSemantics::Delete(BindingDelete::LookupGlobal { name, depth, .. }) => {
                let name_idx = self
                    .published_const_index(name)
                    .map_err(CommittedValueError::Fatal)?;
                let depth = self
                    .published_imm32(depth)
                    .map_err(CommittedValueError::Fatal)?;
                vm.delete_lookup_global_value(context, function_id, value0, name_idx, depth)
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
        let context = self.context.clone();
        let context = &context;

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
